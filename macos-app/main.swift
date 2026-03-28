import Cocoa
import WebKit

// MARK: - Scheme Handler

/// Proxies requests from the custom `intendant://` scheme to the local HTTP
/// backend. WKWebView does not treat `http://localhost` as a secure context,
/// so navigator.mediaDevices (mic/camera) is unavailable. Loading the page
/// from a custom scheme registered via setURLSchemeHandler restores secure
/// context status.
class BackendSchemeHandler: NSObject, WKURLSchemeHandler {
    let port: Int
    private var stopped = Set<Int>()
    private let lock = NSLock()

    init(port: Int) {
        self.port = port
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        guard let originalURL = urlSchemeTask.request.url,
              var components = URLComponents(url: originalURL, resolvingAgainstBaseURL: false) else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = port

        guard let backendURL = components.url else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }

        var request = URLRequest(url: backendURL)
        request.httpMethod = urlSchemeTask.request.httpMethod
        request.httpBody = urlSchemeTask.request.httpBody
        if let headers = urlSchemeTask.request.allHTTPHeaderFields {
            for (key, value) in headers {
                request.setValue(value, forHTTPHeaderField: key)
            }
        }

        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue

        URLSession.shared.dataTask(with: request) { [weak self] data, response, error in
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

class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var backendProcess: Process?
    let port: Int = 8765

    func applicationDidFinishLaunching(_ notification: Notification) {
        startBackend()
        createWindow()
        pollUntilReady()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        backendProcess?.terminate()
        backendProcess?.waitUntilExit()
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
        process.arguments = ["--web"]

        // Inherit environment + ensure Homebrew PATH
        var env = ProcessInfo.processInfo.environment
        let extraPaths = ["/opt/homebrew/bin", "/usr/local/bin"]
        let currentPath = env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        let missing = extraPaths.filter { !currentPath.contains($0) && FileManager.default.fileExists(atPath: $0) }
        if !missing.isEmpty {
            env["PATH"] = missing.joined(separator: ":") + ":" + currentPath
        }
        process.environment = env

        // Set working directory to the directory containing the .app bundle.
        // For ~/projects/intendant/target/Intendant.app this gives ~/projects/intendant/target/
        // Then walk up to find the project root (directory with .env or Cargo.toml)
        var dir = URL(fileURLWithPath: bundle.bundlePath).deletingLastPathComponent()
        for _ in 0..<5 {
            if FileManager.default.fileExists(atPath: dir.appendingPathComponent("Cargo.toml").path) ||
               FileManager.default.fileExists(atPath: dir.appendingPathComponent(".env").path) {
                break
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        process.currentDirectoryURL = dir
        NSLog("Working directory: \(dir.path)")

        // Log backend output for debugging
        let logDir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".intendant")
        try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
        let logFile = logDir.appendingPathComponent("app-backend.log")
        FileManager.default.createFile(atPath: logFile.path, contents: nil)
        let logHandle = FileHandle(forWritingAtPath: logFile.path)
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

        // Serve pages from a custom scheme so WKWebView grants a secure
        // context (required for navigator.mediaDevices / getUserMedia).
        config.setURLSchemeHandler(BackendSchemeHandler(port: port), forURLScheme: "intendant")

        // Inject backend port so JS can build WebSocket URLs (WebSocket
        // connections bypass the scheme handler and need the real address).
        let script = WKUserScript(
            source: "window.__intendantPort = \(port);",
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        config.userContentController.addUserScript(script)

        webView = WKWebView(frame: .zero, configuration: config)
        webView.customUserAgent = "Intendant/1.0"

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
        window.title = "Intendant"
        window.contentView = webView
        window.minSize = NSSize(width: 600, height: 400)
        window.makeKeyAndOrderFront(nil)

        // Dark title bar to match Catppuccin Mocha theme
        window.titlebarAppearsTransparent = true
        window.backgroundColor = NSColor(red: 30/255, green: 30/255, blue: 46/255, alpha: 1.0)
        window.appearance = NSAppearance(named: .darkAqua)
    }

    // MARK: - Polling

    func pollUntilReady() {
        webView.loadHTMLString("""
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
            webView.loadHTMLString("""
                <html>
                <body style="background:#1e1e2e;color:#f38ba8;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
                <div>Failed to connect to backend on port \(port)</div>
                </body>
                </html>
                """, baseURL: nil)
            return
        }

        // Poll the HTTP backend directly
        let healthURL = URL(string: "http://127.0.0.1:\(port)/")!
        var request = URLRequest(url: healthURL, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        URLSession.shared.dataTask(with: request) { _, response, error in
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                DispatchQueue.main.async {
                    // Load via custom scheme for secure context
                    let appURL = URL(string: "intendant://backend/")!
                    self.webView.load(URLRequest(url: appURL))
                }
            } else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    self.poll(attempts: attempts + 1)
                }
            }
        }.resume()
    }
}

// MARK: - Main

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
