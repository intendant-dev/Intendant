import Foundation

/// Dedicated, bounded, ephemeral connection to the app-supervised loopback endpoint.
/// Every callback and public entry is serialized on the main queue. No cookies, disk
/// cache, redirects, generic tool invocation, or authority supplied by web content.
final class AgentViewTransport: NSObject, AgentViewSource, URLSessionDataDelegate, @unchecked Sendable {
    typealias Authenticate = (URLSession, URLAuthenticationChallenge, @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) -> Void
    private struct Pending {
        let id: String
        var data = Data()
        var failure: Error?
        let complete: (Result<[String: Any], Error>) -> Void
    }
    private let endpoint: URL
    private let token: () -> String?
    private let authenticate: Authenticate
    private var pending: [Int: Pending] = [:]
    private lazy var session: URLSession = {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil; configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 10; configuration.timeoutIntervalForResource = 12
        configuration.httpMaximumConnectionsPerHost = 1
        return URLSession(configuration: configuration, delegate: self, delegateQueue: .main)
    }()
    private final class Handle: AgentViewRequest {
        let stop: () -> Void
        init(_ stop: @escaping () -> Void) { self.stop = stop }
        func cancel() { stop() }
    }
    init(scheme: String, port: Int, token: @escaping () -> String?, authenticate: @escaping Authenticate) {
        precondition(["https", "http"].contains(scheme) && (1...65535).contains(port))
        self.endpoint = URL(string: "\(scheme)://127.0.0.1:\(port)/mcp")!
        self.token = token; self.authenticate = authenticate
        super.init()
    }
    func close() { precondition(Thread.isMainThread); pending.removeAll(); session.invalidateAndCancel() }
    func inventory(_ completion: @escaping (Result<[AgentViewMonitor], Error>) -> Void) -> AgentViewRequest {
        call("list_macos_monitors", arguments: [:]) { completion($0.flatMap { result in Result { try AgentViewWire.inventory(result) } }) }
    }
    func capture(_ monitor: AgentViewMonitor, completion: @escaping (Result<AgentViewFrame, Error>) -> Void) -> AgentViewRequest {
        call("take_screenshot", arguments: ["display_target": monitor.target, "ephemeral": true]) {
            completion($0.flatMap { result in Result { try AgentViewFrame.decode(result, monitor: monitor) } })
        }
    }
    private func call(_ tool: String, arguments: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) -> AgentViewRequest {
        precondition(Thread.isMainThread)
        guard let admission = token(), !admission.isEmpty else {
            DispatchQueue.main.async { completion(.failure(AgentViewError.noPermission)) }
            return Handle({})
        }
        let id = UUID().uuidString
        var request = URLRequest(url: endpoint, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 10)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("2025-06-18", forHTTPHeaderField: "MCP-Protocol-Version")
        request.setValue(admission, forHTTPHeaderField: "x-intendant-loopback-token")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["jsonrpc": "2.0", "id": id,
            "method": "tools/call", "params": ["name": tool, "arguments": arguments]])
        let task = session.dataTask(with: request)
        pending[task.taskIdentifier] = Pending(id: id, complete: completion)
        task.resume()
        return Handle { [weak self, weak task] in
            guard let task = task else { return }
            self?.pending.removeValue(forKey: task.taskIdentifier); task.cancel()
        }
    }
    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse, completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        precondition(Thread.isMainThread)
        guard pending[dataTask.taskIdentifier] != nil, let http = response as? HTTPURLResponse,
              http.statusCode == 200, response.expectedContentLength <= 32 * 1024 * 1024 else {
            let status = (response as? HTTPURLResponse)?.statusCode
            pending[dataTask.taskIdentifier]?.failure = (status == 401 || status == 403) ? AgentViewError.noPermission : AgentViewError.unavailable
            completionHandler(.cancel); return
        }
        completionHandler(.allow)
    }
    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        precondition(Thread.isMainThread)
        guard let count = pending[dataTask.taskIdentifier]?.data.count else { return }
        guard count + data.count <= 32 * 1024 * 1024 else {
            pending[dataTask.taskIdentifier]?.failure = AgentViewError.imageTooLarge; dataTask.cancel(); return
        }
        pending[dataTask.taskIdentifier]?.data.append(data)
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        precondition(Thread.isMainThread)
        guard let row = pending.removeValue(forKey: task.taskIdentifier) else { return }
        if let failure = row.failure { row.complete(.failure(failure)); return }
        guard error == nil else { row.complete(.failure(AgentViewError.unavailable)); return }
        row.complete(Result { try AgentViewWire.result(row.data, id: row.id) })
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        pending[task.taskIdentifier]?.failure = AgentViewError.unavailable
        completionHandler(nil) // Never forward the admission token to a redirected endpoint.
    }
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge, completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.host == "127.0.0.1", challenge.protectionSpace.port == endpoint.port else {
            completionHandler(.cancelAuthenticationChallenge, nil); return
        }
        authenticate(session, challenge, completionHandler)
    }
}
