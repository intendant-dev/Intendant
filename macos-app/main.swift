import Cocoa
import Security
import WebKit

// MARK: - Backend TLS

struct BackendLaunchPlan {
    let extraArgs: [String]
    let autoNoTls: Bool
    let autoMtls: Bool
    let usesTLS: Bool
    let usesMtls: Bool
    let usesExplicitTlsCertPair: Bool
    let accessCertDir: URL

    var scheme: String {
        usesTLS ? "https" : "http"
    }
}

func defaultAccessCertDir() -> URL {
    FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".intendant")
        .appendingPathComponent("access-certs")
}

func cliHasFlag(_ args: [String], _ flag: String) -> Bool {
    args.contains(flag) || args.contains { $0.hasPrefix(flag + "=") }
}

func readableFileExists(_ url: URL) -> Bool {
    FileManager.default.isReadableFile(atPath: url.path)
}

func installedAccessTlsAvailable(_ certDir: URL) -> Bool {
    readableFileExists(certDir.appendingPathComponent("server.crt")) &&
        readableFileExists(certDir.appendingPathComponent("server.key"))
}

func installedAccessMtlsAvailable(_ certDir: URL) -> Bool {
    installedAccessTlsAvailable(certDir) &&
        readableFileExists(certDir.appendingPathComponent("ca.crt")) &&
        readableFileExists(certDir.appendingPathComponent("client.p12")) &&
        readableFileExists(certDir.appendingPathComponent("p12_password"))
}

func buildBackendLaunchPlan(extraArgs: [String]) -> BackendLaunchPlan {
    let certDir = defaultAccessCertDir()
    let explicitNoTls = cliHasFlag(extraArgs, "--no-tls")
    let explicitTls = !explicitNoTls && (cliHasFlag(extraArgs, "--tls") ||
        cliHasFlag(extraArgs, "--mtls") ||
        cliHasFlag(extraArgs, "--tls-cert") ||
        cliHasFlag(extraArgs, "--tls-key") ||
        cliHasFlag(extraArgs, "--mtls-ca"))
    let usesExplicitTlsCertPair = cliHasFlag(extraArgs, "--tls-cert") ||
        cliHasFlag(extraArgs, "--tls-key")
    let disableAutoTls = ProcessInfo.processInfo.environment["INTENDANT_BUNDLE_DISABLE_TLS"] == "1"
    let autoNoTls = !explicitNoTls && !explicitTls && disableAutoTls
    let autoMtls = !explicitNoTls && !explicitTls && !autoNoTls
    return BackendLaunchPlan(
        extraArgs: extraArgs,
        autoNoTls: autoNoTls,
        autoMtls: autoMtls,
        usesTLS: explicitTls || autoMtls,
        usesMtls: cliHasFlag(extraArgs, "--mtls") || cliHasFlag(extraArgs, "--mtls-ca") || autoMtls,
        usesExplicitTlsCertPair: usesExplicitTlsCertPair,
        accessCertDir: certDir
    )
}

func readPemCertificateDER(_ url: URL) -> Data? {
    guard let pem = try? String(contentsOf: url, encoding: .utf8) else {
        return nil
    }
    let begin = "-----BEGIN CERTIFICATE-----"
    let end = "-----END CERTIFICATE-----"
    guard let beginRange = pem.range(of: begin),
          let endRange = pem.range(of: end, range: beginRange.upperBound..<pem.endIndex) else {
        return nil
    }
    let base64 = pem[beginRange.upperBound..<endRange.lowerBound]
        .components(separatedBy: .whitespacesAndNewlines)
        .joined()
    return Data(base64Encoded: base64)
}

func loadClientIdentity(certDir: URL) -> (SecIdentity, [SecCertificate])? {
    let p12URL = certDir.appendingPathComponent("client.p12")
    let passwordURL = certDir.appendingPathComponent("p12_password")
    guard let p12 = try? Data(contentsOf: p12URL),
          let password = try? String(contentsOf: passwordURL, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines) else {
        NSLog("mTLS requested, but client.p12 or p12_password is missing in \(certDir.path)")
        return nil
    }

    var importOptions: [String: Any] = [kSecImportExportPassphrase as String: password]
    if #available(macOS 15.0, *) {
        // Keep the identity in process memory: no login-keychain item, so
        // no per-binary "allow access to key" prompt on every rebuild, and
        // TLS client signing works in headless/automation contexts too.
        importOptions[kSecImportToMemoryOnly as String] = kCFBooleanTrue as Any
    }
    var items: CFArray?
    let status = SecPKCS12Import(p12 as CFData, importOptions as CFDictionary, &items)
    guard status == errSecSuccess,
          let imported = items as? [[String: Any]],
          let first = imported.first,
          let rawIdentity = first[kSecImportItemIdentity as String] else {
        NSLog("mTLS requested, but SecPKCS12Import failed for \(p12URL.path) with status \(status)")
        return nil
    }
    let identity = rawIdentity as! SecIdentity
    let chain = first[kSecImportItemCertChain as String] as? [SecCertificate] ?? []
    return (identity, chain)
}

class BackendTrustDelegate: NSObject, URLSessionDelegate {
    let pinnedServerCertDER: Data?
    let clientIdentity: SecIdentity?
    let clientCertificates: [SecCertificate]

    init(certDir: URL, pinInstalledServerCert: Bool, usesMtls: Bool) {
        self.pinnedServerCertDER = pinInstalledServerCert
            ? readPemCertificateDER(certDir.appendingPathComponent("server.crt"))
            : nil
        if usesMtls, let identity = loadClientIdentity(certDir: certDir) {
            self.clientIdentity = identity.0
            self.clientCertificates = identity.1
        } else {
            self.clientIdentity = nil
            self.clientCertificates = []
        }
    }

    func urlSession(_ session: URLSession,
                    didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        switch challenge.protectionSpace.authenticationMethod {
        case NSURLAuthenticationMethodServerTrust:
            guard let trust = challenge.protectionSpace.serverTrust else {
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }
            if let pinnedServerCertDER = pinnedServerCertDER {
                if let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
                   let leaf = chain.first {
                    let leafDER = SecCertificateCopyData(leaf) as Data
                    if leafDER == pinnedServerCertDER {
                        completionHandler(.useCredential, URLCredential(trust: trust))
                        return
                    }
                }
                NSLog("Backend TLS certificate did not match the installed access server.crt")
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }

            // Local wrapper fallback for explicitly-requested TLS without an
            // installed access cert. The connection is loopback to the child
            // process this app just spawned; remote browser trust is still
            // controlled by the daemon's TLS certificate.
            let host = challenge.protectionSpace.host
            if host == "127.0.0.1" || host == "localhost" {
                completionHandler(.useCredential, URLCredential(trust: trust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        case NSURLAuthenticationMethodClientCertificate:
            guard let identity = clientIdentity else {
                completionHandler(.performDefaultHandling, nil)
                return
            }
            let credential = URLCredential(
                identity: identity,
                certificates: clientCertificates,
                persistence: .forSession
            )
            completionHandler(.useCredential, credential)
        default:
            completionHandler(.performDefaultHandling, nil)
        }
    }
}

// MARK: - Scheme Handler

/// Proxies requests from the custom `intendant://` scheme to the local backend.
/// WKWebView does not treat `http://localhost` as a secure context, so
/// navigator.mediaDevices (mic/camera) is unavailable. Loading the page from a
/// custom scheme registered via setURLSchemeHandler restores secure-context
/// status while the proxy can still speak HTTP or HTTPS to the spawned daemon.
class BackendSchemeHandler: NSObject, WKURLSchemeHandler {
    let launchPlan: BackendLaunchPlan
    let port: Int
    private var stopped = Set<Int>()
    private let lock = NSLock()
    private let session: URLSession

    init(port: Int, launchPlan: BackendLaunchPlan, session: URLSession) {
        self.port = port
        self.launchPlan = launchPlan
        self.session = session
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        guard let originalURL = urlSchemeTask.request.url,
              var components = URLComponents(url: originalURL, resolvingAgainstBaseURL: false) else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }
        components.scheme = launchPlan.scheme
        components.host = "127.0.0.1"
        components.port = port

        guard let backendURL = components.url else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }

        var request = URLRequest(url: backendURL, cachePolicy: .reloadIgnoringLocalCacheData)
        request.httpMethod = urlSchemeTask.request.httpMethod
        request.httpBody = urlSchemeTask.request.httpBody
        if let headers = urlSchemeTask.request.allHTTPHeaderFields {
            for (key, value) in headers {
                request.setValue(value, forHTTPHeaderField: key)
            }
        }

        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue

        session.dataTask(with: request) { [weak self] data, response, error in
            guard let self = self else { return }
            self.lock.lock()
            let wasStopped = self.stopped.remove(taskHash) != nil
            self.lock.unlock()
            if wasStopped { return }

            if let error = error {
                urlSchemeTask.didFailWithError(error)
                return
            }
            if let response = response {
                urlSchemeTask.didReceive(response)
            }
            if let data = data {
                urlSchemeTask.didReceive(data)
            }
            urlSchemeTask.didFinish()
        }.resume()
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue
        lock.lock()
        stopped.insert(taskHash)
        lock.unlock()
    }
}

// MARK: - App Delegate

final class ConsoleBridge: NSObject, WKScriptMessageHandler {
    func userContentController(_ userContentController: WKUserContentController,
                               didReceive message: WKScriptMessage) {
        NSLog("[webview] \(message.body)")
    }
}

/// Routes the placeholder/crash pages' buttons back into the app.
/// Held strongly by WKUserContentController, so the delegate link is weak.
final class AppMessageBridge: NSObject, WKScriptMessageHandler {
    weak var appDelegate: AppDelegate?
    func userContentController(_ userContentController: WKUserContentController,
                               didReceive message: WKScriptMessage) {
        switch message.name {
        case "activate": appDelegate?.activateDashboard()
        case "restart": appDelegate?.restartBackend()
        default: break
        }
    }
}

class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, WKUIDelegate,
    WKNavigationDelegate
{
    let consoleBridge = ConsoleBridge()
    let messageBridge = AppMessageBridge()
    var window: NSWindow!
    var webView: WKWebView!
    var backendProcess: Process?
    var healthTimer: Timer?
    var healthProbeFailures = 0
    var port: Int = 8765
    let portSearchLimit = 20
    var launchPlan: BackendLaunchPlan!
    var backendSession: URLSession!
    var backendTrustDelegate: BackendTrustDelegate?

    /// Whether the WKWebView currently hosts the dashboard SPA (as opposed
    /// to the placeholder / boot / crash pages). The SPA is the expensive
    /// part — a long streaming session grows the web-content process past
    /// a gigabyte — so it only loads on explicit request and is torn down
    /// with the window.
    var dashboardActive = false
    /// Last moment the window was actually on screen (occlusion-visible).
    var lastWindowVisibleAt = Date()
    /// Unload the SPA back to the placeholder after the window has been
    /// continuously hidden this long. 0 or negative disables. The default
    /// catches "left it open behind other windows for days" without ever
    /// firing on a dashboard someone is glancing at.
    let idleUnloadSeconds: TimeInterval = {
        if let raw = ProcessInfo.processInfo.environment["INTENDANT_DASHBOARD_IDLE_UNLOAD_SECS"],
           let value = Double(raw) {
            return value
        }
        return 3 * 3600
    }()
    /// Load the SPA immediately instead of the placeholder. INTENDANT_DIAG
    /// smoke runs depend on the dashboard coming up unattended, and users
    /// who prefer the old behavior can set INTENDANT_AUTO_DASHBOARD=1.
    let autoActivateDashboard: Bool = {
        let env = ProcessInfo.processInfo.environment
        return env["INTENDANT_DIAG"] == "1" || env["INTENDANT_AUTO_DASHBOARD"] == "1"
    }()

    func applicationDidFinishLaunching(_ notification: Notification) {
        let preferredPort = port
        if let availablePort = findAvailablePort(startingAt: preferredPort) {
            port = availablePort
            if port != preferredPort {
                NSLog("Port \(preferredPort) in use — using port \(port)")
            }
        } else {
            let lastPort = preferredPort + portSearchLimit - 1
            NSLog("No available port found in range \(preferredPort)-\(lastPort)")
        }
        // Check permissions BEFORE creating the window so system prompts
        // aren't hidden behind it. AXIsProcessTrustedWithOptions is the
        // official way to trigger the Accessibility prompt.
        launchPlan = buildBackendLaunchPlan(
            extraArgs: Array(ProcessInfo.processInfo.arguments.dropFirst())
        )
        configureBackendSession()
        checkPermissions()
        installMainMenu()
        startBackend()
        createWindow()
        pollUntilReady()
    }

    /// The app historically had no menu bar because closing the window
    /// quit it. Now that the window is closable without stopping the
    /// daemon, Quit (Cmd+Q) and Close (Cmd+W) need menu items to exist.
    func installMainMenu() {
        let mainMenu = NSMenu()

        let appItem = NSMenuItem()
        mainMenu.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(
            withTitle: "Quit Intendant (stops the daemon)",
            action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q"
        )

        let windowItem = NSMenuItem()
        mainMenu.addItem(windowItem)
        let windowMenu = NSMenu(title: "Window")
        windowItem.submenu = windowMenu
        windowMenu.addItem(
            withTitle: "Close Window (daemon keeps running)",
            action: #selector(NSWindow.performClose(_:)),
            keyEquivalent: "w"
        )
        windowMenu.addItem(
            withTitle: "Minimize",
            action: #selector(NSWindow.performMiniaturize(_:)),
            keyEquivalent: "m"
        )
        NSApp.mainMenu = mainMenu
    }

    func configureBackendSession() {
        if launchPlan.usesTLS {
            backendTrustDelegate = BackendTrustDelegate(
                certDir: launchPlan.accessCertDir,
                pinInstalledServerCert: !launchPlan.usesExplicitTlsCertPair,
                usesMtls: launchPlan.usesMtls
            )
            backendSession = URLSession(
                configuration: .ephemeral,
                delegate: backendTrustDelegate,
                delegateQueue: nil
            )
            if launchPlan.autoMtls {
                if installedAccessMtlsAvailable(launchPlan.accessCertDir) {
                    NSLog("Access certs found in \(launchPlan.accessCertDir.path) — launching bundled backend with --mtls")
                } else {
                    NSLog("No complete access cert set found in \(launchPlan.accessCertDir.path) — launching bundled backend with --mtls so the daemon fails closed with setup guidance")
                }
            } else {
                NSLog("Bundled backend TLS enabled by launch arguments")
            }
        } else {
            backendSession = URLSession(configuration: .ephemeral)
            let cert = launchPlan.accessCertDir.appendingPathComponent("server.crt")
            let key = launchPlan.accessCertDir.appendingPathComponent("server.key")
            if FileManager.default.fileExists(atPath: cert.path) ||
                FileManager.default.fileExists(atPath: key.path) {
                NSLog(
                    "Access cert store exists but server.crt/server.key are not both readable in \(launchPlan.accessCertDir.path); bundled backend will stay on HTTP"
                )
            }
        }
    }

    func checkPermissions() {
        // Request permissions via Apple APIs. These calls REGISTER the app
        // in System Settings (so it appears in the permission lists) and
        // may trigger system prompts. We then check the result and show
        // our own alert if anything is still missing.
        let hasScreenRecording = CGRequestScreenCaptureAccess()
        let accessibilityOpts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary
        let hasAccessibility = AXIsProcessTrustedWithOptions(accessibilityOpts)
        NSLog("Permissions: accessibility=\(hasAccessibility), screenRecording=\(hasScreenRecording)")

        // Both granted — nothing to do
        if hasAccessibility && hasScreenRecording { return }

        // Give system prompts a moment to appear and be dismissed
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.5))

        // Re-check after system prompts
        let finalAccessibility = AXIsProcessTrusted()
        let finalScreenRecording = CGPreflightScreenCaptureAccess()
        if finalAccessibility && finalScreenRecording { return }

        var missing: [String] = []
        if !finalAccessibility { missing.append("Accessibility (for mouse/keyboard control)") }
        if !finalScreenRecording { missing.append("Screen Recording (for screenshots and display capture)") }

        let alert = NSAlert()
        alert.messageText = "Permissions Required"
        alert.informativeText = "Intendant needs these permissions to work properly:\n\n"
            + missing.enumerated().map { "\($0.offset + 1). \($0.element)" }.joined(separator: "\n")
            + "\n\nOpen System Settings > Privacy & Security and toggle each one ON for Intendant."
            + "\n\nIf already toggled on, toggle OFF then ON again (macOS may need a refresh after recompiling)."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Open Settings")
        alert.addButton(withTitle: "Continue Anyway")

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            if !finalAccessibility {
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
            } else {
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")!)
            }
        }
    }

    func isPortAvailable(_ p: Int) -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        guard sock >= 0 else { return false }
        defer { close(sock) }
        // Allow binding even when TIME_WAIT connections linger from a previous
        // session — the backend uses SO_REUSEADDR too, so this matches.
        var reuse: Int32 = 1
        setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("0.0.0.0")  // match backend bind address
        addr.sin_port = UInt16(p).bigEndian
        let result = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(sock, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        return result == 0
    }

    func findAvailablePort(startingAt preferred: Int) -> Int? {
        let lastPort = min(Int(UInt16.max), preferred + portSearchLimit - 1)
        guard preferred > 0 && preferred <= lastPort else { return nil }
        for candidate in preferred...lastPort {
            if isPortAvailable(candidate) {
                return candidate
            }
        }
        return nil
    }

    // Closing the window frees the WKWebView but keeps the daemon (and the
    // app) alive — the whole point of running this Mac as a remote daemon.
    // Quitting the app is the explicit "stop everything" gesture.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication,
                                       hasVisibleWindows flag: Bool) -> Bool {
        if window == nil {
            createWindow()
            showPlaceholder(paused: false)
        } else {
            if window.isMiniaturized { window.deminiaturize(nil) }
            window.makeKeyAndOrderFront(nil)
        }
        return false
    }

    // MARK: - NSWindowDelegate

    func windowWillClose(_ notification: Notification) {
        NSLog("Dashboard window closed — daemon keeps running (quit via Cmd+Q or the Dock menu)")
        teardownWebView()
        window = nil
    }

    func windowDidChangeOcclusionState(_ notification: Notification) {
        if window?.occlusionState.contains(.visible) == true {
            lastWindowVisibleAt = Date()
        }
    }

    // MARK: - Dashboard lifecycle

    /// Destroy the WKWebView outright. Dropping the last reference exits
    /// the WebKit content/GPU helper processes — actual zero cost, unlike
    /// an occluded page that merely throttles.
    func teardownWebView() {
        guard webView != nil else { return }
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
        webView.removeFromSuperview()
        webView = nil
        dashboardActive = false
    }

    /// The cheap resting state: a static page with daemon status and an
    /// Activate button. A web-content process hosting this is ~20 MB; the
    /// SPA it defers is hundreds of MB and grows with session length.
    func showPlaceholder(paused: Bool) {
        guard webView != nil else { return }
        dashboardActive = false
        let title = paused ? "Dashboard paused" : "Intendant daemon is running"
        let detail = paused
            ? "The dashboard was unloaded after staying hidden, to give its memory back. The daemon never stopped."
            : "Remote clients can connect right away — load the dashboard here only when you need it."
        webView.loadHTMLString("""
            <html>
            <body style="background:#1e1e2e;color:#cdd6f4;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center;max-width:480px;padding:0 24px">
                <div style="font-size:24px;margin-bottom:10px">\(title)</div>
                <div style="font-size:14px;color:#6c7086;line-height:1.5">Serving on port \(port). \(detail)</div>
                <button onclick="window.webkit.messageHandlers.activate.postMessage(null)"
                        style="margin-top:18px;padding:10px 28px;border:1px solid #89b4fa;border-radius:6px;background:transparent;color:#89b4fa;font-size:15px;cursor:pointer">
                    Activate Dashboard
                </button>
                <div style="font-size:12px;color:#6c7086;margin-top:16px">Closing this window keeps the daemon running. Quit from the Dock or with Cmd+Q to stop it.</div>
            </div>
            </body>
            </html>
            """, baseURL: nil)
        NSLog(paused
            ? "Dashboard unloaded to placeholder (hidden \(Int(idleUnloadSeconds))s)"
            : "Showing dashboard placeholder — activate to load the SPA")
    }

    func activateDashboard() {
        if window == nil { createWindow() }
        guard !dashboardActive, webView != nil else { return }
        dashboardActive = true
        lastWindowVisibleAt = Date()
        NSLog("Activating dashboard")
        webView.load(URLRequest(url: intendantBackendURL()))
    }

    /// Crash-screen Restart button: relaunch the backend and re-enter the
    /// readiness poll (which lands on the placeholder / auto-activation).
    func restartBackend() {
        NSLog("Backend restart requested")
        healthTimer?.invalidate()
        healthProbeFailures = 0
        if let proc = backendProcess, proc.isRunning {
            proc.terminate()
        }
        startBackend()
        pollUntilReady()
    }

    func applicationWillTerminate(_ notification: Notification) {
        healthTimer?.invalidate()
        guard let proc = backendProcess, proc.isRunning else { return }
        proc.terminate()
        // Wait up to 3 seconds, then force-kill to avoid hanging on quit
        let deadline = Date().addingTimeInterval(3.0)
        while proc.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.1)
        }
        if proc.isRunning {
            kill(proc.processIdentifier, SIGKILL)
        }
    }

    // MARK: - WKUIDelegate (JS alert/confirm/prompt)

    func webView(_ webView: WKWebView,
                 runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.runModal()
        completionHandler()
    }

    func webView(_ webView: WKWebView,
                 runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        completionHandler(alert.runModal() == .alertFirstButtonReturn)
    }

    func webView(_ webView: WKWebView,
                 runJavaScriptTextInputPanelWithPrompt prompt: String,
                 defaultText: String?,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping (String?) -> Void) {
        let alert = NSAlert()
        alert.messageText = prompt
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        input.stringValue = defaultText ?? ""
        alert.accessoryView = input
        completionHandler(alert.runModal() == .alertFirstButtonReturn ? input.stringValue : nil)
    }

    // MARK: - WKNavigationDelegate

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        // macOS killed the web process (memory pressure). Restore what was
        // actually showing — reloading the SPA when only the placeholder
        // was up would defeat the point of deferring it.
        NSLog("Web content process terminated — \(dashboardActive ? "reloading dashboard" : "restoring placeholder")")
        if dashboardActive {
            webView.load(URLRequest(url: intendantBackendURL()))
        } else {
            showPlaceholder(paused: false)
        }
    }

    // Navigation outcomes are otherwise invisible from outside the app;
    // these lines are what `open`-less smoke runs and Console.app get.
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        NSLog("Dashboard loaded: \(webView.url?.absoluteString ?? "?")")
        guard webView.url?.scheme == "intendant" else { return }
        // Diagnostic snapshot: what transport did the dashboard end up on?
        for delay in [4.0, 12.0] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                self?.webView.evaluateJavaScript(
                    "(() => { const s = window.intendantDashboardControl?.status?.() || null; return s ? JSON.stringify({enabled: s.enabled, connected: s.connected, mode: s.signalingMode, err: s.lastError, pc: s.pcState}) : 'no-control-api'; })()"
                ) { result, error in
                    NSLog("Transport status (+\(Int(delay))s): \(result ?? error?.localizedDescription ?? "nil")")
                }
            }
        }
    }

    func webView(_ webView: WKWebView,
                 didFailProvisionalNavigation navigation: WKNavigation!,
                 withError error: Error) {
        NSLog("Dashboard failed to load: \(error.localizedDescription)")
    }

    // MARK: - Backend

    func startBackend() {
        let bundle = Bundle.main
        let binPath = bundle.bundlePath + "/Contents/MacOS/intendant-bin"

        guard FileManager.default.fileExists(atPath: binPath) else {
            NSLog("intendant-bin not found at \(binPath)")
            return
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binPath)
        // Forward any extra CLI arguments (e.g. --agent codex) to the backend
        var args = ["--web", String(port)]
        if launchPlan.autoMtls {
            args.append("--mtls")
        } else if launchPlan.autoNoTls {
            args.append("--no-tls")
        }
        args.append(contentsOf: launchPlan.extraArgs)
        process.arguments = args

        // Inherit environment + ensure Homebrew PATH
        var env = ProcessInfo.processInfo.environment
        let extraPaths = ["/opt/homebrew/bin", "/usr/local/bin"]
        let currentPath = env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        let missing = extraPaths.filter { !currentPath.contains($0) && FileManager.default.fileExists(atPath: $0) }
        if !missing.isEmpty {
            env["PATH"] = missing.joined(separator: ":") + ":" + currentPath
        }
        process.environment = env

        // Working directory: plainly the user's home. Nothing derives from
        // the launch cwd anymore — a daemon started outside a project (no
        // .git / intendant.toml at cwd) runs projectless on the Rust side:
        // no cwd file watching, no cwd-derived sandbox scope, no default
        // session project. Each session picks its project directory in the
        // dashboard's New Session pane. (Home may itself contain a project
        // marker; if so the daemon simply roots there, which is the normal
        // rooted behavior, not a scan hazard — projectless/marker gating
        // lives in the Rust daemon.)
        let dir = FileManager.default.homeDirectoryForCurrentUser
        process.currentDirectoryURL = dir
        NSLog("Working directory: \(dir.path)")

        // Log backend output for debugging (append mode — preserves crash info
        // from previous sessions; the Rust panic hook writes per-session panic.log
        // files for structured auditing, this is the fallback for pre-session crashes)
        let logDir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".intendant")
        try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
        let logFile = logDir.appendingPathComponent("app-backend.log")
        if !FileManager.default.fileExists(atPath: logFile.path) {
            FileManager.default.createFile(atPath: logFile.path, contents: nil)
        }
        let logHandle = FileHandle(forWritingAtPath: logFile.path)
        logHandle?.seekToEndOfFile()
        // Write launch separator
        let sep = "\n--- Launch \(ISO8601DateFormatter().string(from: Date())) ---\n"
        logHandle?.write(sep.data(using: .utf8) ?? Data())
        process.standardOutput = logHandle ?? FileHandle.nullDevice
        process.standardError = logHandle ?? FileHandle.nullDevice

        do {
            try process.run()
            backendProcess = process
            NSLog("Started intendant-bin (PID \(process.processIdentifier)) on port \(port)")
        } catch {
            NSLog("Failed to start intendant-bin: \(error)")
        }
    }

    // MARK: - Window

    func createWindow() {
        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        // Allow media autoplay (for voice features)
        config.mediaTypesRequiringUserActionForPlayback = []

        // Use a non-persistent data store so WKWebView never caches WASM/JS
        // across app launches. Without this, recompiled WASM may not load.
        config.websiteDataStore = WKWebsiteDataStore.nonPersistent()

        // Serve pages from a custom scheme so WKWebView grants a secure
        // context (required for navigator.mediaDevices / getUserMedia).
        config.setURLSchemeHandler(
            BackendSchemeHandler(port: port, launchPlan: launchPlan, session: backendSession),
            forURLScheme: "intendant"
        )

        // Inject backend port so JS can build WebSocket URLs (WebSocket
        // connections bypass the scheme handler and need the real address).
        let tlsLiteral = launchPlan.usesTLS ? "true" : "false"
        let script = WKUserScript(
            source: "window.__intendantPort = \(port); window.__intendantBackendTls = \(tlsLiteral);",
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        config.userContentController.addUserScript(script)

        // Forward page console output to NSLog so `Console.app` and
        // terminal launches can see what the dashboard is doing — the
        // WKWebView inspector is rarely attached when it matters.
        config.userContentController.add(consoleBridge, name: "log")

        // Placeholder "Activate Dashboard" + crash-screen "Restart".
        messageBridge.appDelegate = self
        config.userContentController.add(messageBridge, name: "activate")
        config.userContentController.add(messageBridge, name: "restart")
        let consoleScript = WKUserScript(
            source: """
            (() => {
              const send = level => (...args) => {
                try {
                  window.webkit.messageHandlers.log.postMessage(level + ': ' + args.map(a => {
                    try { return typeof a === 'string' ? a : JSON.stringify(a); } catch (e) { return String(a); }
                  }).join(' '));
                } catch (e) {}
              };
              for (const level of ['log', 'info', 'warn', 'error']) {
                const original = console[level].bind(console);
                console[level] = (...args) => { send(level)(...args); original(...args); };
              }
              window.addEventListener('error', e => send('pageerror')(e.message || String(e)));
              window.addEventListener('unhandledrejection', e => send('unhandledrejection')(e.reason?.message || String(e.reason)));
            })();
            """,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        config.userContentController.addUserScript(consoleScript)

        webView = WKWebView(frame: .zero, configuration: config)
        webView.uiDelegate = self
        webView.navigationDelegate = self
        webView.customUserAgent = "Intendant/1.0"

        // Starting in macOS 13.3, the legacy `developerExtrasEnabled` KVC
        // trick above is a no-op for release-signed builds; Safari's Web
        // Inspector only attaches to a WKWebView when `isInspectable` is
        // explicitly set to `true`. Without this, Safari → Develop →
        // [Mac name] silently omits the Intendant process — which blocks
        // any WebRTC diagnostics that rely on Safari Web Inspector
        // (ICE candidate events, iceConnectionState, getStats output).
        if #available(macOS 13.3, *) {
            webView.isInspectable = true
        }

        let screen = NSScreen.main ?? NSScreen.screens[0]
        let screenFrame = screen.visibleFrame
        let width = min(1400.0, screenFrame.width * 0.85)
        let height = min(900.0, screenFrame.height * 0.85)
        let x = screenFrame.origin.x + (screenFrame.width - width) / 2
        let y = screenFrame.origin.y + (screenFrame.height - height) / 2

        window = NSWindow(
            contentRect: NSRect(x: x, y: y, width: width, height: height),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = port == 8765 ? "Intendant" : "Intendant (port \(port))"
        window.contentView = webView
        window.minSize = NSSize(width: 600, height: 400)
        // ARC owns the window through `self.window`; the default
        // release-when-closed would over-release it on the first close.
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.makeKeyAndOrderFront(nil)

        // Dark title bar matching the ui-v2 background (--bg #0B0C10);
        // the dashboard defaults to v2 since the P3 flip.
        window.titlebarAppearsTransparent = true
        window.backgroundColor = NSColor(red: 11/255, green: 12/255, blue: 16/255, alpha: 1.0)
        window.appearance = NSAppearance(named: .darkAqua)
    }

    // MARK: - Polling

    func pollUntilReady() {
        webView?.loadHTMLString("""
            <html>
            <body style="background:#1e1e2e;color:#cdd6f4;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:24px;margin-bottom:8px">Starting Intendant...</div>
                <div style="font-size:14px;color:#6c7086">Waiting for backend on port \(port)</div>
            </div>
            </body>
            </html>
            """, baseURL: nil)

        poll(attempts: 0)
    }

    func poll(attempts: Int) {
        if attempts > 30 {
            // The window may have been closed while the backend booted;
            // the poll keeps running regardless, only painting is skipped.
            webView?.loadHTMLString("""
                <html>
                <body style="background:#1e1e2e;color:#f38ba8;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
                <div>Failed to connect to backend on port \(port)</div>
                </body>
                </html>
                """, baseURL: nil)
            return
        }

        // Poll the backend directly; under bundled auto-TLS/mTLS this is
        // HTTPS and uses the same local trust delegate as the intendant:// proxy.
        let healthURL = backendURL("/")
        var request = URLRequest(url: healthURL, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        backendSession.dataTask(with: request) { _, response, error in
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                DispatchQueue.main.async {
                    // Backend is up. The SPA is deferred behind the
                    // placeholder unless a harness/user asked for it.
                    if self.autoActivateDashboard {
                        self.activateDashboard()
                    } else {
                        self.showPlaceholder(paused: false)
                    }
                    self.startHealthCheck()
                }
            } else {
                // Silent-by-default; the last attempts say why the failure
                // page is about to render (slow cold start vs TLS refusal).
                if attempts >= 28 {
                    let status = (response as? HTTPURLResponse)?.statusCode ?? 0
                    NSLog("Backend readiness poll failing (attempt \(attempts + 1)/30): status=\(status) error=\(error?.localizedDescription ?? "none")")
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    self.poll(attempts: attempts + 1)
                }
            }
        }.resume()
    }

    // MARK: - Health Check

    func startHealthCheck() {
        healthTimer?.invalidate()
        healthTimer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            // Check if the backend process is still alive
            if let proc = self.backendProcess, !proc.isRunning {
                self.healthTimer?.invalidate()
                self.showBackendCrash()
                return
            }
            // Idle unload: an SPA nobody has seen for hours is pure cost —
            // its web-content process grows with every streamed session.
            if self.dashboardActive,
               self.idleUnloadSeconds > 0,
               let win = self.window,
               !win.occlusionState.contains(.visible),
               Date().timeIntervalSince(self.lastWindowVisibleAt) > self.idleUnloadSeconds {
                self.showPlaceholder(paused: true)
            }
            // Also ping the HTTP endpoint. Probe failures are logged, never
            // fatal: the process-liveness check above is the only thing
            // allowed to declare a crash — a slow daemon or a stalled TLS
            // probe must not replace a working dashboard with a false
            // "Backend process exited" screen.
            let url = self.backendURL("/")
            var req = URLRequest(url: url, timeoutInterval: 2)
            req.httpMethod = "HEAD"
            self.backendSession.dataTask(with: req) { _, response, error in
                let ok = (response as? HTTPURLResponse)?.statusCode == 200
                DispatchQueue.main.async {
                    if ok {
                        if self.healthProbeFailures >= 3 {
                            NSLog("Backend health probe recovered after \(self.healthProbeFailures) failures")
                        }
                        self.healthProbeFailures = 0
                        return
                    }
                    self.healthProbeFailures += 1
                    if self.healthProbeFailures == 3 || self.healthProbeFailures % 24 == 0 {
                        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
                        NSLog("Backend health probe failing (\(self.healthProbeFailures) consecutive; status=\(status) error=\(error?.localizedDescription ?? "none")) — process is still running")
                    }
                }
            }.resume()
        }
    }

    func backendURL(_ path: String) -> URL {
        URL(string: "\(launchPlan.scheme)://127.0.0.1:\(port)\(path)")!
    }

    func showBackendCrash() {
        NSLog("Backend process is no longer running")
        // A dead daemon is worth a window even if the user had closed it —
        // remotely this machine just went dark.
        if window == nil { createWindow() }
        dashboardActive = false
        guard webView != nil else { return }
        webView.loadHTMLString("""
            <html>
            <body style="background:#1e1e2e;color:#cdd6f4;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:20px;color:#f38ba8;margin-bottom:12px">Backend process exited</div>
                <div style="font-size:14px;color:#6c7086;margin-bottom:16px">Check ~/.intendant/app-backend.log for details</div>
                <button onclick="window.webkit.messageHandlers.restart && window.webkit.messageHandlers.restart.postMessage(null)"
                        style="padding:8px 24px;border:1px solid #89b4fa;border-radius:6px;background:transparent;color:#89b4fa;font-size:14px;cursor:pointer">
                    Restart
                </button>
            </div>
            </body>
            </html>
            """, baseURL: nil)
    }
}

// MARK: - Helpers

/// Resolve the URL the WKWebView loads on initial entry and on
/// web-content-process restart. Setting `INTENDANT_DIAG=1` in the
/// environment appends `?diag=1` so the dashboard's visual-freshness
/// sampler activates from page load. Off by default — used only for
/// harness/smoke runs (see `docs/smoke-display.md` §9). Routes through
/// the same `intendant://backend/` custom scheme so the WKWebView keeps
/// its secure context (mic, custom URL scheme handler).
func intendantBackendURL() -> URL {
    let diag = ProcessInfo.processInfo.environment["INTENDANT_DIAG"] == "1"
    let raw = diag ? "intendant://backend/?diag=1" : "intendant://backend/"
    return URL(string: raw)!
}

// MARK: - Main

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
