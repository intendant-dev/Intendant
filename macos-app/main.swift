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
    /// Mutable: a one-click update swap re-points the proxy at the
    /// promoted successor's port (main-thread writes; per-request reads).
    var port: Int
    private var stopped = Set<Int>()
    /// Live proxied requests by scheme-task hash, so stop() can cancel
    /// the network task instead of letting an abandoned request run to
    /// the session's idle timeout holding its loopback socket.
    private var inFlight = [Int: URLSessionDataTask]()
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

        let dataTask = session.dataTask(with: request) { [weak self] data, response, error in
            guard let self = self else { return }
            self.lock.lock()
            let wasStopped = self.stopped.remove(taskHash) != nil
            self.inFlight.removeValue(forKey: taskHash)
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
        }
        lock.lock()
        inFlight[taskHash] = dataTask
        lock.unlock()
        dataTask.resume()
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue
        lock.lock()
        stopped.insert(taskHash)
        let dataTask = inFlight.removeValue(forKey: taskHash)
        lock.unlock()
        // Cancel the network task too: WebKit only promises not to hear
        // back, but an uncancelled request would keep running to the
        // session's request timeout holding its loopback socket.
        dataTask?.cancel()
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
        case "updateSwap": appDelegate?.beginUpdateSwapFromDashboard()
        default: break
        }
    }
}

class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, WKUIDelegate,
    WKNavigationDelegate, BackendSupervisorDelegate
{
    let consoleBridge = ConsoleBridge()
    let messageBridge = AppMessageBridge()
    var window: NSWindow!
    var webView: WKWebView!
    /// Retained so an update swap can re-point the `intendant://` proxy
    /// at the promoted successor's port.
    var schemeHandler: BackendSchemeHandler!
    /// Owns the daemon child process, restart/backoff policy, readiness
    /// polling, health checks, and the backend log (see
    /// macos-app/BackendSupervisor.swift). State changes come back through
    /// BackendSupervisorDelegate and drive the screens below.
    var backendSupervisor: BackendSupervisor!
    var port: Int = 8765
    let portSearchLimit = 20
    /// Q6 (update-abstraction §4.4): while this app supervises a
    /// single-instance daemon, the port is an implementation detail —
    /// the window is just "Intendant", including across one-click
    /// update swaps. The suffix survives only when the app launched
    /// into a shared-host topology (its preferred port was already
    /// taken — the manual multi-instance case), where it is
    /// load-bearing disambiguation between co-homed instances.
    var sharedHostTopology = false

    func windowTitle(for port: Int) -> String {
        sharedHostTopology ? "Intendant (port \(port))" : "Intendant"
    }
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
                sharedHostTopology = true
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
        backendSupervisor = makeBackendSupervisor()
        backendSupervisor.startBackend()
        createWindow()
        backendSupervisor.pollUntilReady()
        checkForUpdatesQuietly()
    }

    /// The app historically had no menu bar because closing the window
    /// quit it. Now that the window is closable without stopping the
    /// daemon, Quit (Cmd+Q) and Close (Cmd+W) need menu items to exist.
    ///
    /// The Edit menu exists for its key equivalents: without one, macOS
    /// routes no standard editing combos at all, so Cmd+C/V/X/A/Z were
    /// dead inside the WKWebView dashboard. Every Edit action is a
    /// nil-targeted standard selector, resolved down the responder chain
    /// to the web view's focused content.
    func installMainMenu() {
        let mainMenu = NSMenu()

        let appItem = NSMenuItem()
        mainMenu.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        // Standard about panel — shows the CFBundleShortVersionString stamped
        // by scripts/bundle-macos.sh (the release tag, or a git-describe dev
        // version), which is how a user answers "what version am I running?".
        appMenu.addItem(
            withTitle: "About Intendant",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
        let updateItem = appMenu.addItem(
            withTitle: "Check for Updates…",
            action: #selector(checkForUpdates(_:)),
            keyEquivalent: ""
        )
        updateItem.target = self
        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Hide Intendant",
            action: #selector(NSApplication.hide(_:)),
            keyEquivalent: "h"
        )
        let hideOthersItem = appMenu.addItem(
            withTitle: "Hide Others",
            action: #selector(NSApplication.hideOtherApplications(_:)),
            keyEquivalent: "h"
        )
        hideOthersItem.keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(
            withTitle: "Show All",
            action: #selector(NSApplication.unhideAllApplications(_:)),
            keyEquivalent: ""
        )
        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Quit Intendant (stops the daemon)",
            action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q"
        )

        let editItem = NSMenuItem()
        mainMenu.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editItem.submenu = editMenu
        // Undo/Redo have no ObjC-visible Swift declaration to hang a
        // #selector on (NSResponder picks them up informally), so the
        // string form is the canonical spelling. The uppercase "Z" key
        // equivalent means Shift+Cmd+Z.
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        editMenu.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        editMenu.addItem(NSMenuItem.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(NSMenuItem.separator())
        editMenu.addItem(
            withTitle: "Select All",
            action: #selector(NSText.selectAll(_:)),
            keyEquivalent: "a"
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

    /// Proxy request budget: the dashboard's own HTTP and tunnel lanes
    /// time out at 120s and render honest retry states, so the scheme
    /// proxy must sit above them — Foundation's 60s default undercut the
    /// JS timers and killed slow-but-healthy endpoints (the first
    /// worktree scan after a reboot) with an opaque proxy error first.
    private func backendSessionConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 180
        return configuration
    }

    func configureBackendSession() {
        if launchPlan.usesTLS {
            backendTrustDelegate = BackendTrustDelegate(
                certDir: launchPlan.accessCertDir,
                pinInstalledServerCert: !launchPlan.usesExplicitTlsCertPair,
                usesMtls: launchPlan.usesMtls
            )
            backendSession = URLSession(
                configuration: backendSessionConfiguration(),
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
            backendSession = URLSession(configuration: backendSessionConfiguration())
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
            <body style="background:#0B0C10;color:#EAECF2;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center;max-width:480px;padding:0 24px">
                <div style="font-size:24px;margin-bottom:10px">\(title)</div>
                <div style="font-size:14px;color:#7E8896;line-height:1.5">Serving on port \(port). \(detail)</div>
                <button onclick="window.webkit.messageHandlers.activate.postMessage(null)"
                        style="margin-top:18px;padding:10px 28px;border:1px solid #7E8CFA;border-radius:6px;background:transparent;color:#7E8CFA;font-size:15px;cursor:pointer">
                    Activate Dashboard
                </button>
                <div style="font-size:12px;color:#7E8896;margin-top:16px">Closing this window keeps the daemon running. Quit from the Dock or with Cmd+Q to stop it.</div>
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
        webView.load(URLRequest(url: intendantBackendURL(port: port)))
    }

    /// Crash-screen Restart button: relaunch the backend and re-enter the
    /// readiness poll (which lands on the placeholder / auto-activation).
    @discardableResult
    func restartBackend() -> Bool {
        backendSupervisor.restartBackend()
    }

    func applicationWillTerminate(_ notification: Notification) {
        // Quitting kills the backend on purpose; the supervisor suppresses
        // its exit handling and takes the child down with a bounded wait.
        backendSupervisor?.shutdown()
    }

    // MARK: - Update check

    /// Launch-time check: strictly non-intrusive. Dev builds skip it, every
    /// failure mode (offline, rate limit, no releases yet) is silent, and a
    /// newer release surfaces as a window sheet — never an app-modal alert.
    func checkForUpdatesQuietly() {
        let local = UpdateChecker.bundledVersion()
        guard !UpdateChecker.isDevBuild(local) else { return }
        UpdateChecker.fetchLatestRelease { [weak self] release in
            guard let self = self,
                  let release = release,
                  UpdateChecker.isNewer(remote: release.tag, than: local) else { return }
            // Advisory transparency-log check on the release being
            // offered — fail-open: every outcome still shows the prompt,
            // annotated with logged / not logged / couldn't check.
            UpdateChecker.fetchReleaseLogStatus(tag: release.tag) { [weak self] status in
                self?.presentUpdatePrompt(release: release, logStatus: status, interactive: false)
            }
        }
    }

    /// "Check for Updates…" menu item. An explicit request deserves explicit
    /// answers, so unlike the launch check this also reports "no update" and
    /// "couldn't check".
    @objc func checkForUpdates(_ sender: Any?) {
        UpdateChecker.fetchLatestRelease { [weak self] release in
            guard let self = self else { return }
            guard let release = release else {
                let alert = NSAlert()
                alert.messageText = "Couldn't check for updates"
                alert.informativeText =
                    "GitHub releases could not be reached (offline, rate-limited, or nothing published yet)."
                alert.addButton(withTitle: "OK")
                alert.addButton(withTitle: "Open Releases Page")
                if alert.runModal() == .alertSecondButtonReturn {
                    NSWorkspace.shared.open(UpdateChecker.releasesPageURL)
                }
                return
            }
            let local = UpdateChecker.bundledVersion()
            // The explicit check also reports the transparency-log
            // advisory for the latest release, whichever branch answers.
            UpdateChecker.fetchReleaseLogStatus(tag: release.tag) { [weak self] status in
                guard let self = self else { return }
                if UpdateChecker.isNewer(remote: release.tag, than: local) {
                    self.presentUpdatePrompt(release: release, logStatus: status, interactive: true)
                } else {
                    let alert = NSAlert()
                    alert.messageText = "No update available"
                    alert.informativeText =
                        "This app is version \(local). The latest published release is \(release.tag).\n\n"
                        + UpdateChecker.advisoryLine(for: status, tag: release.tag)
                    alert.addButton(withTitle: "OK")
                    alert.runModal()
                }
            }
        }
    }

    /// The prompt never downloads anything: "View Release" opens the GitHub
    /// release page in the default browser and the user takes it from there.
    /// `logStatus` rides along as an advisory line — whether the offered
    /// release is committed to the public transparency log (never gates
    /// the prompt; docs/src/self-hosted-rendezvous.md, "Release
    /// transparency").
    func presentUpdatePrompt(
        release: UpdateChecker.Release,
        logStatus: UpdateChecker.ReleaseLogStatus,
        interactive: Bool
    ) {
        let alert = NSAlert()
        alert.messageText = "Intendant \(release.tag) is available"
        alert.informativeText =
            "This app is version \(UpdateChecker.bundledVersion()). "
            + "Updating is manual: View Release opens the GitHub release page in your browser — "
            + "nothing is downloaded or installed automatically.\n\n"
            + UpdateChecker.advisoryLine(for: logStatus, tag: release.tag)
        alert.addButton(withTitle: "View Release")
        alert.addButton(withTitle: "Not Now")
        let handle: (NSApplication.ModalResponse) -> Void = { response in
            if response == .alertFirstButtonReturn {
                NSWorkspace.shared.open(release.pageURL)
            }
        }
        if !interactive, let window = window {
            // Sheet on the (placeholder) window: visible next time the user
            // looks, steals no focus from other apps.
            alert.beginSheetModal(for: window, completionHandler: handle)
        } else if interactive {
            handle(alert.runModal())
        }
        // Launch check with no window (closed before the response arrived):
        // stay silent; the menu item remains available.
    }

    // MARK: - External links (system browser)

    /// Whether `url` stays inside the app's own dashboard world: the
    /// `intendant://` proxy scheme, the wrapper's generated pages
    /// (about:blank via loadHTMLString) and in-page content schemes, and
    /// anything on loopback — the supervised daemon in every TLS/WS/port
    /// shape, co-homed sibling daemons included.
    func isDashboardOrigin(_ url: URL) -> Bool {
        guard let scheme = url.scheme?.lowercased() else { return true }
        if ["intendant", "about", "data", "blob", "file", "javascript"].contains(scheme) {
            return true
        }
        let host = url.host?.lowercased() ?? ""
        return host == "localhost" || host.hasSuffix(".localhost")
            || host == "127.0.0.1" || host == "::1" || host == "[::1]"
    }

    /// Map a webview-internal URL to what the system browser should get.
    /// `intendant://backend/...` is the proxy origin — its real address is
    /// the supervised daemon on loopback (an explicit port in the URL
    /// survives, so co-homed sibling links keep working; the default is
    /// the supervised port). Pure in-page schemes yield nil (nothing a
    /// browser could open).
    func systemBrowserURL(for url: URL?) -> URL? {
        guard let url = url, let scheme = url.scheme?.lowercased(), !scheme.isEmpty else {
            return nil
        }
        if ["about", "data", "blob", "javascript"].contains(scheme) { return nil }
        if scheme == "intendant" {
            var components = URLComponents(url: url, resolvingAgainstBaseURL: false)
            components?.scheme = launchPlan.scheme
            components?.host = "127.0.0.1"
            components?.port = url.port ?? port
            return components?.url
        }
        return url
    }

    /// Hand a URL out of the webview to the user's default browser.
    /// Sign-in and enrollment flows depend on the real profile (password
    /// manager, passkeys, existing sessions) — an isolated webview window
    /// is never the right place for them.
    func openInSystemBrowser(_ rawURL: URL?, reason: String) {
        guard let target = systemBrowserURL(for: rawURL) else {
            NSLog("External open skipped (\(reason)): no browser-usable URL in \(rawURL?.absoluteString ?? "nil")")
            return
        }
        NSLog("Opening in system browser (\(reason)): \(target.absoluteString)")
        NSWorkspace.shared.open(target)
    }

    // MARK: - WKUIDelegate (popups + JS alert/confirm/prompt)

    /// window.open / target=_blank from the dashboard. The app never
    /// grows a second webview: popups are sign-in and enrollment flows
    /// that must land in the system default browser, so the URL goes
    /// there and nil comes back (window.open yields null). The SPA knows
    /// this contract through the injected `__intendantAppExternalOpen`
    /// marker and treats the null as handled instead of rendering its
    /// popup-blocked fallback. about:blank pre-opens get nothing useful
    /// here — under the marker the SPA skips them and opens the final
    /// URL directly.
    func webView(_ webView: WKWebView,
                 createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction,
                 windowFeatures: WKWindowFeatures) -> WKWebView? {
        openInSystemBrowser(navigationAction.request.url, reason: "popup")
        return nil
    }

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

    /// One policy for sign-in, enrollment, and every future external
    /// link: the dashboard main frame never navigates off its own origin
    /// — anything else opens in the system default browser instead.
    /// Sub-frame loads pass through untouched, as do new-window targets
    /// (nil frame — those reach createWebViewWith above).
    func webView(_ webView: WKWebView,
                 decidePolicyFor navigationAction: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let frame = navigationAction.targetFrame, frame.isMainFrame,
              let url = navigationAction.request.url,
              !isDashboardOrigin(url)
        else {
            decisionHandler(.allow)
            return
        }
        decisionHandler(.cancel)
        openInSystemBrowser(url, reason: "main-frame navigation")
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        // macOS killed the web process (memory pressure). Restore what was
        // actually showing — reloading the SPA when only the placeholder
        // was up would defeat the point of deferring it.
        NSLog("Web content process terminated — \(dashboardActive ? "reloading dashboard" : "restoring placeholder")")
        if dashboardActive {
            webView.load(URLRequest(url: intendantBackendURL(port: port)))
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

    // MARK: - Backend supervision

    /// Assemble the supervisor for the bundled daemon. Argument policy
    /// (TLS mode, forwarded CLI args) derives from the launch plan here;
    /// process lifecycle, restart backoff, readiness polling, health
    /// checks, and the backend log live in BackendSupervisor.
    func makeBackendSupervisor() -> BackendSupervisor {
        // Forward any extra CLI arguments (e.g. --agent codex) to the
        // backend. The `--web <port>` pair is composed per spawn by the
        // supervisor, so an update swap can boot the successor on a
        // fresh port with the same policy args.
        var args: [String] = []
        if launchPlan.autoMtls {
            args.append("--mtls")
        } else if launchPlan.autoNoTls {
            args.append("--no-tls")
        }
        args.append(contentsOf: launchPlan.extraArgs)
        let supervisor = BackendSupervisor(
            binaryPath: Bundle.main.bundlePath + "/Contents/MacOS/intendant-bin",
            baseArguments: args,
            port: port,
            scheme: launchPlan.scheme,
            session: backendSession
        )
        supervisor.delegate = self
        return supervisor
    }

    /// Map supervision states to the screens the user sees. The supervisor
    /// owns the process and the policy; this layer only paints.
    func backendSupervisor(_ supervisor: BackendSupervisor,
                           didChangeState state: BackendState,
                           detail: String) {
        switch state {
        case .starting:
            showBackendStarting(detail: detail)
        case .ready:
            // Backend is up. The SPA is deferred behind the
            // placeholder unless a harness/user asked for it.
            if autoActivateDashboard {
                activateDashboard()
            } else {
                showPlaceholder(paused: false)
            }
        case .unreachable:
            showBackendUnreachable(detail: detail)
        case .stoppedCleanly:
            showBackendCrash(title: "Backend stopped", detail: detail)
        case .crashed:
            showBackendCrash(title: "Backend process crashed", detail: detail)
        }
    }

    /// 5s housekeeping tick from the supervisor's health timer.
    func backendSupervisorHealthTick(_ supervisor: BackendSupervisor) {
        // Idle unload: an SPA nobody has seen for hours is pure cost —
        // its web-content process grows with every streamed session.
        if dashboardActive,
           idleUnloadSeconds > 0,
           let win = window,
           !win.occlusionState.contains(.visible),
           Date().timeIntervalSince(lastWindowVisibleAt) > idleUnloadSeconds {
            showPlaceholder(paused: true)
        }
    }

    // MARK: - One-click update swap (HS6/P3)

    /// The dashboard's update chip asked for the one-click update —
    /// through the webview bridge (`updateSwap` message) or the daemon
    /// relay (claim poll): pick a fresh port and let the supervisor run
    /// the spawn → readiness → swap → drain sequence. Failure feedback
    /// rides `window.__intendantAppSwapFailed` into our own webview AND
    /// the daemon's result route (for browser surfaces we cannot reach);
    /// success repaints through `didSwapToPort`.
    func beginUpdateSwapFromDashboard() {
        guard let fresh = findAvailablePort(startingAt: port + 1) else {
            NSLog("Update swap: no free port near \(port)")
            notifySwapFailed("no free port for the new daemon")
            backendSupervisor.reportSwapFailure(detail: "no free port for the new daemon")
            return
        }
        NSLog("Update swap requested from the dashboard — successor on port \(fresh)")
        backendSupervisor.beginUpdateSwap(newPort: fresh) { [weak self] ok, detail in
            if !ok {
                self?.notifySwapFailed(detail)
                self?.backendSupervisor.reportSwapFailure(detail: detail)
            }
        }
    }

    /// The daemon relay's claim poll surfaced a swap request from a
    /// dashboard surface beyond our webview — same entry as the bridge.
    func backendSupervisorUpdateSwapRequested(_ supervisor: BackendSupervisor) {
        beginUpdateSwapFromDashboard()
    }

    func notifySwapFailed(_ detail: String) {
        let literal: String
        if let data = try? JSONSerialization.data(withJSONObject: detail, options: .fragmentsAllowed),
           let encoded = String(data: data, encoding: .utf8) {
            literal = encoded
        } else {
            literal = "\"update swap failed\""
        }
        webView?.evaluateJavaScript(
            "window.__intendantAppSwapFailed && window.__intendantAppSwapFailed(\(literal))",
            completionHandler: nil
        )
    }

    /// The supervisor promoted the successor: re-point the proxy and the
    /// injected port script, then reload whatever surface is up so the
    /// tab attaches to the new daemon (fresh boot_id, fresh token). The
    /// drained predecessor keeps serving its in-flight sessions until it
    /// exits on its own.
    func backendSupervisor(_ supervisor: BackendSupervisor, didSwapToPort newPort: Int) {
        NSLog("Update swap: dashboard re-pointing to port \(newPort)")
        port = newPort
        schemeHandler?.port = newPort
        if let controller = webView?.configuration.userContentController {
            installUserScripts(controller, port: newPort)
        }
        // Q6: an app-supervised swap never renames the window — the
        // fresh port is the supervisor's own mechanics. The suffix
        // stays only for shared-host topologies (set at launch).
        window?.title = windowTitle(for: newPort)
        if dashboardActive {
            dashboardActive = false
            activateDashboard()
        } else {
            showPlaceholder(paused: false)
        }
    }

    // MARK: - Window

    /// (Re-)install the injected user scripts for `port`. The port and
    /// TLS flag ride a document-start script (WebSocket connections
    /// bypass the scheme handler and need the real address), and
    /// `__intendantAppSupervisor` marks the supervisor's presence so the
    /// dashboard's update chip renders the one-click action instead of
    /// the CLI-daemon hand-off. `__intendantAppExternalOpen` tells the
    /// SPA that popups and external links are routed to the system
    /// default browser (window.open returns null yet succeeded — see
    /// createWebViewWith). Re-run at update swap with the new port
    /// (scripts are fixed strings, so a swap must replace them).
    func installUserScripts(_ controller: WKUserContentController, port: Int) {
        controller.removeAllUserScripts()
        let tlsLiteral = launchPlan.usesTLS ? "true" : "false"
        let portScript = WKUserScript(
            source: "window.__intendantPort = \(port); window.__intendantBackendTls = \(tlsLiteral); "
                + "window.__intendantAppSupervisor = true; window.__intendantAppExternalOpen = true;",
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        controller.addUserScript(portScript)
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
        controller.addUserScript(consoleScript)
    }

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
        schemeHandler = BackendSchemeHandler(port: port, launchPlan: launchPlan, session: backendSession)
        config.setURLSchemeHandler(schemeHandler, forURLScheme: "intendant")

        // Forward page console output to NSLog so `Console.app` and
        // terminal launches can see what the dashboard is doing — the
        // WKWebView inspector is rarely attached when it matters.
        config.userContentController.add(consoleBridge, name: "log")

        // Placeholder "Activate Dashboard" + crash-screen "Restart" +
        // the update chip's one-click swap.
        messageBridge.appDelegate = self
        config.userContentController.add(messageBridge, name: "activate")
        config.userContentController.add(messageBridge, name: "restart")
        config.userContentController.add(messageBridge, name: "updateSwap")
        installUserScripts(config.userContentController, port: port)

        webView = WKWebView(frame: .zero, configuration: config)
        webView.uiDelegate = self
        webView.navigationDelegate = self
        webView.customUserAgent = "Intendant/\(UpdateChecker.bundledVersion())"

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
        window.title = windowTitle(for: port)
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

    // MARK: - Backend screens

    /// Boot screen while the readiness poll runs. The window may be
    /// closed; the poll keeps running regardless, only painting is
    /// skipped.
    func showBackendStarting(detail: String) {
        webView?.loadHTMLString("""
            <html>
            <body style="background:#0B0C10;color:#EAECF2;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:24px;margin-bottom:8px">Starting Intendant...</div>
                <div style="font-size:14px;color:#7E8896">\(detail)</div>
            </div>
            </body>
            </html>
            """, baseURL: nil)
    }

    /// Readiness poll exhausted. Unlike a crash this never conjures a
    /// window — if it was closed mid-boot, only the painting is skipped.
    func showBackendUnreachable(detail: String) {
        paintBackendFailurePage(
            title: "Failed to connect to backend on port \(port)",
            detail: detail
        )
    }

    func showBackendCrash(
        title: String = "Backend process exited",
        detail: String = "Check ~/.intendant/app-backend.log for details"
    ) {
        NSLog("Backend crash screen: \(title) — \(detail)")
        // A dead daemon is worth a window even if the user had closed it —
        // remotely this machine just went dark.
        if window == nil { createWindow() }
        dashboardActive = false
        paintBackendFailurePage(title: title, detail: detail)
    }

    /// Shared failure page (red title, detail line, Restart button);
    /// paints only when a webview exists.
    func paintBackendFailurePage(title: String, detail: String) {
        guard webView != nil else { return }
        let esc = { (s: String) -> String in
            s.replacingOccurrences(of: "&", with: "&amp;")
                .replacingOccurrences(of: "<", with: "&lt;")
                .replacingOccurrences(of: ">", with: "&gt;")
        }
        webView.loadHTMLString("""
            <html>
            <body style="background:#0B0C10;color:#EAECF2;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:20px;color:#EC6A85;margin-bottom:12px">\(esc(title))</div>
                <div style="font-size:14px;color:#7E8896;margin-bottom:16px">\(esc(detail))</div>
                <button onclick="window.webkit.messageHandlers.restart && window.webkit.messageHandlers.restart.postMessage(null)"
                        style="padding:8px 24px;border:1px solid #7E8CFA;border-radius:6px;background:transparent;color:#7E8CFA;font-size:14px;cursor:pointer">
                    Restart now
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
func intendantBackendURL(port: Int) -> URL {
    let diag = ProcessInfo.processInfo.environment["INTENDANT_DIAG"] == "1"
    var raw = diag ? "intendant://backend/?diag=1" : "intendant://backend/"
    // The daemon refuses tokenless loopback on owner surfaces, and the
    // page's direct WebSocket (which bypasses the mTLS scheme handler)
    // needs the per-boot admission token. Hand it over the same `?token=`
    // channel every other owner surface uses — the SPA stores it and
    // strips it from the URL. The dashboard only loads after the backend
    // readiness poll, so the boot's token file exists by the time this
    // URL is built; if it is unreadable the SPA surfaces the daemon's
    // named refusal rather than the app guessing.
    if let token = loopbackAdmissionToken(port: port), !token.isEmpty {
        let sep = raw.contains("?") ? "&" : "?"
        let encoded = token.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? token
        raw += "\(sep)token=\(encoded)"
    }
    return URL(string: raw)!
}

/// This boot's loopback admission token, read from the daemon state root
/// the way every owner process discovers it (`INTENDANT_HOME` override,
/// else `~/.intendant`), keyed by the backend port the app supervises.
func loopbackAdmissionToken(port: Int) -> String? {
    let env = ProcessInfo.processInfo.environment
    if let explicit = env["INTENDANT_LOOPBACK_TOKEN"], !explicit.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return explicit.trimmingCharacters(in: .whitespacesAndNewlines)
    }
    let root = env["INTENDANT_HOME"].map { URL(fileURLWithPath: $0) }
        ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".intendant")
    let tokenFile = root
        .appendingPathComponent("loopback-tokens")
        .appendingPathComponent("\(port).token")
    guard let raw = try? String(contentsOf: tokenFile, encoding: .utf8) else {
        return nil
    }
    let token = raw.trimmingCharacters(in: .whitespacesAndNewlines)
    return token.isEmpty ? nil : token
}

// MARK: - Main

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
