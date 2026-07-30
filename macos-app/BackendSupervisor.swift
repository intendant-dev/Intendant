import Foundation

// MARK: - Backend Supervision

/// Supervision states surfaced to the app layer, each driving a screen
/// (boot page, placeholder/dashboard, failure page, restart countdown).
/// The `detail` string on the delegate callback carries the human-readable
/// line that varies per transition (countdown seconds, log-file pointer).
enum BackendState {
    /// A spawn was requested and the readiness poll is in flight.
    case starting
    /// The readiness poll reached the backend (HTTP 200); the recurring
    /// health check is now running.
    case ready
    /// The readiness poll exhausted its attempts without an answer.
    case unreachable
    /// The backend exited cleanly — a deliberate stop; only the manual
    /// Restart button revives it.
    case stoppedCleanly
    /// The backend exited abnormally; the auto-restart countdown is armed.
    case crashed
}

/// Callbacks are always delivered on the main queue.
protocol BackendSupervisorDelegate: AnyObject {
    /// Drive the boot / placeholder / crash / countdown screens.
    func backendSupervisor(_ supervisor: BackendSupervisor,
                           didChangeState state: BackendState,
                           detail: String)
    /// Recurring 5s housekeeping hook, fired while the backend process is
    /// alive and health checks run (the app layer uses it for dashboard
    /// idle-unload).
    func backendSupervisorHealthTick(_ supervisor: BackendSupervisor)
    /// A one-click update swap promoted a successor child on `newPort`
    /// (HS6/P3): the app layer re-points the scheme handler, injected
    /// scripts, and webview. Deliberately NOT a `didChangeState` screen —
    /// the working dashboard must never be painted over mid-swap.
    func backendSupervisor(_ supervisor: BackendSupervisor, didSwapToPort newPort: Int)
}

/// Supervises the bundled Rust daemon: spawn, readiness polling, health
/// checks, exit policy (auto-restart with capped exponential backoff), quit
/// teardown, and the `~/.intendant/app-backend.log` sink the daemon's
/// stdout/stderr append to (rotation + launch/exit markers).
///
/// The daemon MUST remain a **direct child of the app process**: macOS TCC
/// grants (Screen Recording, Accessibility, Microphone, Camera) flow to the
/// daemon and its subprocesses through the app's process tree. Moving
/// supervision to launchd/SMAppService would put the daemon outside that
/// tree and silently strip every capture capability the wrapper exists to
/// provide.
final class BackendSupervisor {
    weak var delegate: BackendSupervisorDelegate?

    private let binaryPath: String
    /// Arguments WITHOUT the `--web <port>` pair — composed per spawn, so
    /// an update swap can launch the successor on a fresh port with the
    /// same policy args.
    private let baseArguments: [String]
    /// The port the CURRENT backend serves; flips at update-swap promote.
    private(set) var port: Int
    private let scheme: String
    /// Trust-configured session shared with the app layer (pins the
    /// installed access server cert / presents the mTLS client identity).
    private let session: URLSession

    private var backendProcess: Process?
    /// The swapped-out predecessor, draining toward its own exit (HS6/P3).
    /// The swap NEVER terminates it — it serves its in-flight sessions and
    /// exits at the last one (`app_supervisor_one_click_swaps_without_kill`);
    /// only app quit tears it down along with everything else.
    private var drainingProcess: Process?
    private var swapInFlight = false
    private var healthTimer: Timer?
    private var healthProbeFailures = 0
    // Backend auto-restart: exponential backoff on abnormal exits, reset
    // once a backend has stayed up long enough to count as stable.
    private var backendRestartAttempts = 0
    private var backendLastStart = Date.distantPast
    private var backendAutoRestartTimer: Timer?
    private var isTerminating = false
    // Readiness-poll chains carry a generation stamp; bumping it orphans
    // any chain already in flight (a crash mid-boot must not let a stale
    // chain paint its dead-end failure page over the restart countdown).
    private var pollGeneration = 0

    /// Rotate `app-backend.log` at launch once it exceeds this many bytes.
    private static let maxLogBytes: UInt64 = 10 * 1024 * 1024

    init(binaryPath: String, baseArguments: [String], port: Int, scheme: String, session: URLSession) {
        self.binaryPath = binaryPath
        self.baseArguments = baseArguments
        self.port = port
        self.scheme = scheme
        self.session = session
    }

    // MARK: - Process lifecycle

    /// Spawn the daemon as a direct child (see the class doc for why not
    /// launchd), wiring stdout/stderr into the append-mode backend log.
    @discardableResult
    func startBackend() -> Bool {
        guard let process = spawnChild(onPort: port) else { return false }
        backendProcess = process
        backendLastStart = Date()
        // React to backend death immediately (the 5s health tick stays
        // as the belt for a missed handler). Remote dashboards have no
        // crash screen — without a restart this machine just goes dark.
        process.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async {
                self?.backendDidExit(proc)
            }
        }
        NSLog("Started intendant-bin (PID \(process.processIdentifier)) on port \(port)")
        return true
    }

    /// Spawn one daemon child on `childPort` (shared by the normal boot
    /// and the update swap's successor): environment, working directory,
    /// and the append-mode backend log, but NO lifecycle wiring — the
    /// caller owns handlers and slots.
    private func spawnChild(onPort childPort: Int) -> Process? {
        guard FileManager.default.fileExists(atPath: binaryPath) else {
            NSLog("intendant-bin not found at \(binaryPath)")
            return nil
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["--web", String(childPort)] + baseArguments

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
        // files for structured auditing, this is the fallback for pre-session
        // crashes). Rotated at launch so it can't grow without bound.
        let logFile = backendLogFile()
        try? FileManager.default.createDirectory(
            at: logFile.deletingLastPathComponent(), withIntermediateDirectories: true)
        rotateBackendLogIfNeeded(logFile)
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
            return process
        } catch {
            NSLog("Failed to start intendant-bin: \(error)")
            return nil
        }
    }

    // MARK: - One-click update swap (HS6/P3)

    /// One-click update (the owner directive on HS6): spawn a SECOND
    /// child from the (updated) on-disk binary on `newPort`, poll
    /// readiness THERE, and only once the successor answers: promote it,
    /// ask the predecessor to drain (the HS3 takeover lane on its own
    /// port), and report `didSwapToPort` so the app layer re-points the
    /// webview. Ordering is the intake sketch's — spawn → readiness →
    /// swap → drain — so a successor that never comes up is simply
    /// terminated (it acquired nothing and drained nothing) and the
    /// running daemon is left exactly as it was; the predecessor is
    /// NEVER terminated by the swap — it serves its in-flight sessions
    /// and exits on its own at the last one.
    func beginUpdateSwap(newPort: Int, completion: @escaping (Bool, String) -> Void) {
        guard !swapInFlight else {
            completion(false, "an update swap is already in flight")
            return
        }
        guard let old = backendProcess, old.isRunning else {
            completion(false, "no running backend to swap from")
            return
        }
        guard let successor = spawnChild(onPort: newPort) else {
            completion(false, "could not start the new daemon — see ~/.intendant/app-backend.log")
            return
        }
        swapInFlight = true
        NSLog("Update swap: successor spawned (PID \(successor.processIdentifier)) on port \(newPort)")
        pollSwapReadiness(successor: successor, newPort: newPort, attempts: 0, completion: completion)
    }

    private func pollSwapReadiness(successor: Process, newPort: Int, attempts: Int,
                                   completion: @escaping (Bool, String) -> Void) {
        // A successor that died mid-boot (the broken-new-binary case)
        // fails the swap fast and leaves the running daemon untouched.
        if !successor.isRunning {
            finishFailedSwap(successor: successor, completion: completion,
                             detail: "the new daemon exited during startup — the running daemon is untouched (see ~/.intendant/app-backend.log)")
            return
        }
        if attempts > 30 {
            finishFailedSwap(successor: successor, completion: completion,
                             detail: "the new daemon never became ready — the running daemon is untouched (see ~/.intendant/app-backend.log)")
            return
        }
        var request = URLRequest(url: URL(string: "\(scheme)://127.0.0.1:\(newPort)/")!, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        session.dataTask(with: request) { _, response, _ in
            DispatchQueue.main.async {
                if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                    self.promoteSuccessor(successor, newPort: newPort, completion: completion)
                } else {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                        self.pollSwapReadiness(successor: successor, newPort: newPort,
                                               attempts: attempts + 1, completion: completion)
                    }
                }
            }
        }.resume()
    }

    /// The successor answered: swap roles, then ask the predecessor to
    /// drain. The predecessor keeps running (never terminated here) with
    /// a log-only exit handler — its clean drain exit must paint no
    /// crash/stopped screen and schedule no restart.
    private func promoteSuccessor(_ successor: Process, newPort: Int,
                                  completion: @escaping (Bool, String) -> Void) {
        guard swapInFlight else { return }
        swapInFlight = false
        let oldPort = port
        if let old = backendProcess {
            old.terminationHandler = { [weak self] proc in
                DispatchQueue.main.async {
                    NSLog("Drained predecessor exited (status \(proc.terminationStatus))")
                    self?.writeExitMarker(status: proc.terminationStatus,
                                          signalled: proc.terminationReason == .uncaughtSignal)
                    if self?.drainingProcess === proc {
                        self?.drainingProcess = nil
                    }
                }
            }
            drainingProcess = old
        }
        backendProcess = successor
        port = newPort
        backendLastStart = Date()
        backendRestartAttempts = 0
        healthProbeFailures = 0
        successor.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async {
                self?.backendDidExit(proc)
            }
        }
        startHealthCheck()
        NSLog("Update swap: promoted successor on port \(newPort); asking :\(oldPort) to drain")
        requestDrain(ofPort: oldPort)
        delegate?.backendSupervisor(self, didSwapToPort: newPort)
        completion(true, "")
    }

    private func finishFailedSwap(successor: Process, completion: @escaping (Bool, String) -> Void,
                                  detail: String) {
        swapInFlight = false
        successor.terminationHandler = nil
        if successor.isRunning {
            // The successor acquired nothing and drained nothing — a
            // plain secondary; terminating it is clean recovery.
            successor.terminate()
        }
        NSLog("Update swap failed: \(detail)")
        completion(false, detail)
    }

    /// The HS3 takeover lane against the predecessor's own port: ask it
    /// to drain so the promoted successor's lease poll acquires. Rides
    /// the same per-port loopback admission token every owner surface
    /// uses; the request is logged, never fatal — the drain banner and
    /// the Q4 successor watch carry the story from here.
    private func requestDrain(ofPort oldPort: Int) {
        guard let url = URL(string: "\(scheme)://127.0.0.1:\(oldPort)/api/daemon/takeover") else { return }
        var request = URLRequest(url: url, timeoutInterval: 5)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        if let token = loopbackAdmissionToken(port: oldPort), !token.isEmpty {
            request.setValue(token, forHTTPHeaderField: "x-intendant-loopback-token")
        }
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["requested_by": "app update swap"])
        session.dataTask(with: request) { _, response, error in
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            NSLog("Update swap: drain request to :\(oldPort) answered \(status)\(error.map { " error: \($0.localizedDescription)" } ?? "")")
        }.resume()
    }

    /// Manual (crash-screen Restart button) or auto-restart-timer restart:
    /// relaunch the backend and re-enter the readiness poll (whose `.ready`
    /// lands on the placeholder / auto-activation).
    @discardableResult
    func restartBackend() -> Bool {
        NSLog("Backend restart requested")
        backendAutoRestartTimer?.invalidate()
        healthTimer?.invalidate()
        healthProbeFailures = 0
        if let proc = backendProcess, proc.isRunning {
            proc.terminationHandler = nil
            proc.terminate()
        }
        let started = startBackend()
        if started {
            pollUntilReady()
        }
        return started
    }

    /// Quit teardown: kill the children on purpose without letting exit
    /// handlers paint a crash screen or schedule a restart mid-teardown.
    /// Quit is the explicit "stop everything" gesture, so a mid-drain
    /// predecessor goes down with the promoted successor.
    func shutdown() {
        isTerminating = true
        backendAutoRestartTimer?.invalidate()
        healthTimer?.invalidate()
        let children = [backendProcess, drainingProcess].compactMap { $0 }.filter { $0.isRunning }
        guard !children.isEmpty else { return }
        for proc in children {
            proc.terminationHandler = nil
            proc.terminate()
        }
        // Wait up to 3 seconds, then force-kill to avoid hanging on quit
        let deadline = Date().addingTimeInterval(3.0)
        while children.contains(where: { $0.isRunning }) && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.1)
        }
        for proc in children where proc.isRunning {
            kill(proc.processIdentifier, SIGKILL)
        }
    }

    // MARK: - Exit policy

    /// Backend exit policy: abnormal exits (signal / non-zero status)
    /// auto-restart with capped exponential backoff; a clean exit means
    /// someone stopped the daemon deliberately, so only the manual Restart
    /// button revives it.
    private func backendDidExit(_ proc: Process) {
        guard !isTerminating, proc === backendProcess, !proc.isRunning else { return }
        // Consume the reference first: the health tick and the
        // terminationHandler can both observe the same exit, and a second
        // pass would double-count the attempt and reschedule the timer.
        backendProcess = nil
        healthTimer?.invalidate()
        let signalled = proc.terminationReason == .uncaughtSignal
        let status = proc.terminationStatus
        let abnormal = signalled || status != 0
        NSLog("Backend exited (\(signalled ? "signal" : "status") \(status), \(abnormal ? "abnormal" : "clean"))")
        writeExitMarker(status: status, signalled: signalled)
        guard abnormal else {
            pollGeneration += 1
            notifyState(.stoppedCleanly, "The daemon exited cleanly. Restart it to keep using this app.")
            return
        }
        if Date().timeIntervalSince(backendLastStart) > 600 {
            backendRestartAttempts = 0
        }
        scheduleBackendAutoRestart()
    }

    /// Arm (or re-arm) the auto-restart countdown with capped exponential
    /// backoff (2s..60s), orphaning any readiness-poll chain so nothing
    /// paints over the countdown screen. Re-entered by the timer itself
    /// when a spawn attempt fails, so the chain never dead-ends.
    private func scheduleBackendAutoRestart() {
        guard !isTerminating else { return }
        pollGeneration += 1
        let delay = min(60.0, pow(2.0, Double(backendRestartAttempts + 1)))
        backendRestartAttempts += 1
        NSLog("Auto-restarting backend in \(Int(delay))s (attempt \(backendRestartAttempts))")
        notifyState(
            .crashed,
            "Restarting in \(Int(delay))s (attempt \(backendRestartAttempts)) — see ~/.intendant/app-backend.log"
        )
        backendAutoRestartTimer?.invalidate()
        backendAutoRestartTimer = Timer.scheduledTimer(withTimeInterval: delay, repeats: false) { [weak self] _ in
            guard let self = self, !self.isTerminating else { return }
            if !self.restartBackend() {
                self.scheduleBackendAutoRestart()
            }
        }
    }

    // MARK: - Readiness polling

    /// Begin (or restart) the generation-stamped readiness poll; announces
    /// `.starting` so the app layer paints the boot screen.
    func pollUntilReady() {
        pollGeneration += 1
        notifyState(.starting, "Waiting for backend on port \(port)")
        poll(attempts: 0, generation: pollGeneration)
    }

    private func poll(attempts: Int, generation: Int) {
        // A crash or scheduled restart bumps pollGeneration; a stale chain
        // must go silent rather than paint over the countdown screen.
        guard generation == pollGeneration else { return }
        if attempts > 30 {
            // The window may have been closed while the backend booted;
            // the poll ran to its end regardless — whether the failure
            // page can actually paint is the delegate's problem.
            notifyState(.unreachable, "Check ~/.intendant/app-backend.log for details")
            return
        }

        // Poll the backend directly; under bundled auto-TLS/mTLS this is
        // HTTPS and uses the same local trust delegate as the intendant:// proxy.
        let healthURL = backendURL("/")
        var request = URLRequest(url: healthURL, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        session.dataTask(with: request) { _, response, error in
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                DispatchQueue.main.async {
                    guard generation == self.pollGeneration else { return }
                    self.notifyState(.ready, "Backend ready on port \(self.port)")
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
                    self.poll(attempts: attempts + 1, generation: generation)
                }
            }
        }.resume()
    }

    // MARK: - Health check

    private func startHealthCheck() {
        healthTimer?.invalidate()
        healthTimer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            // Check if the backend process is still alive
            if let proc = self.backendProcess, !proc.isRunning {
                self.healthTimer?.invalidate()
                self.backendDidExit(proc)
                return
            }
            // Housekeeping hook (dashboard idle-unload lives in the app layer).
            self.delegate?.backendSupervisorHealthTick(self)
            // Also ping the HTTP endpoint. Probe failures are logged, never
            // fatal: the process-liveness check above is the only thing
            // allowed to declare a crash — a slow daemon or a stalled TLS
            // probe must not replace a working dashboard with a false
            // "Backend process exited" screen.
            let url = self.backendURL("/")
            var req = URLRequest(url: url, timeoutInterval: 2)
            req.httpMethod = "HEAD"
            self.session.dataTask(with: req) { _, response, error in
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

    // MARK: - Backend log

    /// `~/.intendant/app-backend.log` — the daemon's stdout/stderr sink.
    private func backendLogFile() -> URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".intendant")
            .appendingPathComponent("app-backend.log")
    }

    /// The append-mode log otherwise grows forever. On each backend launch,
    /// once it exceeds 10 MB shift it aside to `app-backend.log.1`,
    /// replacing any previous `.1` — exactly one predecessor is kept.
    private func rotateBackendLogIfNeeded(_ logFile: URL) {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: logFile.path),
              let size = (attrs[.size] as? NSNumber)?.uint64Value,
              size > Self.maxLogBytes else { return }
        let rotated = logFile.appendingPathExtension("1")
        do {
            try? FileManager.default.removeItem(at: rotated)
            try FileManager.default.moveItem(at: logFile, to: rotated)
            NSLog("Rotated \(logFile.lastPathComponent) (\(size) bytes) to \(rotated.lastPathComponent)")
        } catch {
            NSLog("Failed to rotate \(logFile.lastPathComponent): \(error) — continuing with the oversized log")
        }
    }

    /// Matching bookend for the launch separator, written where the
    /// supervisor observes a backend exit — crash forensics need only this
    /// one file. (Deliberate teardowns — quit, manual restart — clear the
    /// termination handler first and intentionally write no marker.)
    private func writeExitMarker(status: Int32, signalled: Bool) {
        guard let handle = FileHandle(forWritingAtPath: backendLogFile().path) else { return }
        handle.seekToEndOfFile()
        let marker = "\n--- Exit \(ISO8601DateFormatter().string(from: Date())) status=\(status) signal=\(signalled) ---\n"
        handle.write(marker.data(using: .utf8) ?? Data())
        try? handle.close()
    }

    // MARK: - Helpers

    private func backendURL(_ path: String) -> URL {
        URL(string: "\(scheme)://127.0.0.1:\(port)\(path)")!
    }

    private func notifyState(_ state: BackendState, _ detail: String) {
        delegate?.backendSupervisor(self, didChangeState: state, detail: detail)
    }
}
