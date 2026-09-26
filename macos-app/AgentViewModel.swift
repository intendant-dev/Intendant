import Foundation
import CoreFoundation

// Read-only wire vocabulary. Raw/native display IDs and implicit targets are never accepted.
enum AgentViewError: Error, LocalizedError {
    case unavailable, invalidReply, imageTooLarge, noPermission
    var errorDescription: String? {
        switch self {
        case .unavailable: return "Display or connection unavailable. Refresh the display list."
        case .invalidReply: return "The display response could not be verified."
        case .imageTooLarge: return "The preview exceeds its image limit."
        case .noPermission: return "Display viewing is not authorized by this daemon."
        }
    }
}

struct AgentViewMonitor: Equatable {
    let target: String
    let id: UInt32
    let width: Int
    let height: Int
    var label: String { "Virtual display \(id) · \(width) × \(height)" }

    static func boolean(_ value: Any?) -> Bool? {
        guard let n = value as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { return nil }
        return n.boolValue
    }
    static func integer(_ value: Any?) -> Int? {
        guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID(),
              n.doubleValue.isFinite, n.doubleValue.rounded() == n.doubleValue,
              abs(n.doubleValue) <= Double(Int32.max) else { return nil }
        return n.intValue
    }
    static func validTarget(_ target: String) -> Bool {
        let parts = target.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "macos_virtual", parts[1].utf8.count == 32,
              parts[1].utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
              let id = UInt32(parts[2]), (0x2000_0000...0x3fff_ffff).contains(id),
              String(id) == parts[2] else { return false }
        return true
    }
    init(_ value: [String: Any]) throws {
        guard let target = value["display_target"] as? String, Self.validTarget(target),
              value["capture_generation"] as? String == target,
              let id = Self.integer(value["display_id"]),
              let suffix = target.split(separator: ":").last, String(id) == suffix,
              let width = Self.integer(value["width"]), let height = Self.integer(value["height"]),
              (64...8192).contains(width), (64...8192).contains(height), width * height <= 16_777_216,
              value["backend"] as? String == "macos_virtual", Self.boolean(value["lifecycle_ready"]) == true,
              Self.boolean(value["ephemeral_capture_supported"]) == true else {
            throw AgentViewError.invalidReply
        }
        self.target = target; self.id = UInt32(id); self.width = width; self.height = height
    }
}

struct AgentViewFrame {
    let png: Data
    let capturedAt: String
    // The source is the exact requested generation, not a best-effort display fallback.
    static func decode(_ result: [String: Any], monitor: AgentViewMonitor) throws -> AgentViewFrame {
        let (metadata, content) = try AgentViewWire.content(result)
        guard metadata["display_target"] as? String == monitor.target,
              metadata["capture_generation"] as? String == monitor.target,
              AgentViewMonitor.integer(metadata["width"]) == monitor.width,
              AgentViewMonitor.integer(metadata["height"]) == monitor.height,
              AgentViewMonitor.boolean(metadata["artifact_retained"]) == false, metadata["screenshot_path"] == nil,
              AgentViewMonitor.boolean(metadata["capture_ready"]) == true,
              let stamp = metadata["captured_at"] as? String, stamp.utf8.count <= 64,
              content.count == 2,
              let image = content.first(where: { $0["type"] as? String == "image" }),
              image["mimeType"] as? String == "image/png",
              let encoded = image["data"] as? String, encoded.utf8.count <= 24 * 1024 * 1024,
              let png = Data(base64Encoded: encoded), png.count >= 24, png.count <= 18 * 1024 * 1024,
              Array(png.prefix(8)) == [137,80,78,71,13,10,26,10],
              Array(png[12..<16]) == [73,72,68,82] else { throw AgentViewError.invalidReply }
        func dimension(_ offset: Int) -> UInt32 {
            png[offset..<offset+4].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        }
        guard dimension(16) == monitor.width, dimension(20) == monitor.height else {
            throw AgentViewError.invalidReply
        }
        return AgentViewFrame(png: png, capturedAt: stamp)
    }
}

enum AgentViewWire {
    static func result(_ data: Data, id: String) throws -> [String: Any] {
        guard data.count <= 32 * 1024 * 1024,
              let envelope = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              envelope["jsonrpc"] as? String == "2.0", envelope["id"] as? String == id,
              envelope["error"] == nil, let result = envelope["result"] as? [String: Any],
              (result["isError"] == nil || AgentViewMonitor.boolean(result["isError"]) == false) else { throw AgentViewError.invalidReply }
        return result
    }
    static func content(_ result: [String: Any]) throws -> ([String: Any], [[String: Any]]) {
        guard (result["isError"] == nil || AgentViewMonitor.boolean(result["isError"]) == false),
              let content = result["content"] as? [[String: Any]], (1...2).contains(content.count),
              content.filter({ $0["type"] as? String == "text" }).count == 1,
              let text = content.first(where: { $0["type"] as? String == "text" })?["text"] as? String,
              text.utf8.count <= 65536, let data = text.data(using: .utf8),
              let metadata = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              metadata["error"] == nil, (metadata["ok"] == nil || AgentViewMonitor.boolean(metadata["ok"]) == true) else {
            throw AgentViewError.invalidReply
        }
        return (metadata, content)
    }
    static func inventory(_ result: [String: Any]) throws -> [AgentViewMonitor] {
        let (metadata, content) = try self.content(result)
        guard content.count == 1, let state = metadata["broker_state"] as? String,
              let rows = metadata["monitors"] as? [[String: Any]], rows.count <= 2 else {
            throw AgentViewError.invalidReply
        }
        if state == "not_started" || state == "retired" { guard rows.isEmpty else { throw AgentViewError.invalidReply }; return [] }
        guard state == "live" else { throw AgentViewError.unavailable }
        let monitors = try rows.map(AgentViewMonitor.init)
        guard Set(monitors.map(\.target)).count == monitors.count else { throw AgentViewError.invalidReply }
        return monitors
    }
}

protocol AgentViewRequest: AnyObject { func cancel() }
protocol AgentViewSource: AnyObject {
    func inventory(_ completion: @escaping (Result<[AgentViewMonitor], Error>) -> Void) -> AgentViewRequest
    func capture(_ monitor: AgentViewMonitor, completion: @escaping (Result<AgentViewFrame, Error>) -> Void) -> AgentViewRequest
}

// Main-thread state machine; stale/cancelled callbacks cannot repopulate a hidden or retargeted view.
final class AgentViewModel {
    let source: AgentViewSource
    let clock: () -> TimeInterval
    var changed: (() -> Void)?
    private(set) var visible = false
    private(set) var monitors: [AgentViewMonitor] = []
    private(set) var selected: AgentViewMonitor?
    private(set) var frame: AgentViewFrame?
    private(set) var receivedAt: TimeInterval?
    private(set) var message = "Choose an owned virtual display."
    private(set) var busy = false
    private var epoch: UInt64 = 0
    private var request: AgentViewRequest?
    private var nextRefresh: TimeInterval = 0

    init(source: AgentViewSource, clock: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime }) {
        self.source = source; self.clock = clock
    }
    private func invalidate() {
        epoch &+= 1; request?.cancel(); request = nil; busy = false
    }
    func show() { visible = true; reload() }
    func hide() {
        visible = false; invalidate(); frame = nil; receivedAt = nil
        message = "Preview hidden. Agent and display unchanged."; changed?()
    }
    func reload() {
        guard visible else { return }
        invalidate(); frame = nil; receivedAt = nil; busy = true
        message = "Checking owned virtual displays…"; changed?()
        let ticket = epoch
        request = source.inventory { [weak self] result in
            guard let self = self, self.visible, self.epoch == ticket else { return }
            self.busy = false; self.request = nil
            switch result {
            case .success(let monitors):
                self.monitors = monitors
                if let selected = self.selected, !monitors.contains(selected) { self.selected = nil }
                self.message = monitors.isEmpty ? "No owned virtual displays. The preview does not create one." : "Choose an owned virtual display."
                self.nextRefresh = 0
            case .failure(let error): self.message = (error as? AgentViewError ?? .invalidReply).localizedDescription; self.monitors = []; self.selected = nil
            }
            self.changed?(); self.tick()
        }
    }
    func select(_ monitor: AgentViewMonitor?) {
        guard visible else { return }
        invalidate(); frame = nil; receivedAt = nil
        selected = monitor.flatMap { monitors.contains($0) ? $0 : nil }
        message = selected == nil ? "Choose an owned virtual display." : "Requesting a verified frame…"
        nextRefresh = 0; changed?(); tick()
    }
    func tick() {
        guard visible, !busy, let monitor = selected, clock() >= nextRefresh else { return }
        busy = true; let ticket = epoch
        request = source.capture(monitor) { [weak self] result in
            guard let self = self, self.visible, self.epoch == ticket, self.selected == monitor else { return }
            self.request = nil; self.busy = false
            switch result {
            case .success(let frame):
                self.frame = frame; self.receivedAt = self.clock(); self.message = "Snapshot preview · Read-only"
                self.nextRefresh = self.clock() + 2
            case .failure(let error):
                self.frame = nil; self.receivedAt = nil; self.message = (error as? AgentViewError ?? .invalidReply).localizedDescription
                self.nextRefresh = self.clock() + 5
            }
            self.changed?()
        }
    }
    var freshness: String {
        guard let receivedAt = receivedAt else { return "No verified frame" }
        return "Received \(max(0, Int(clock() - receivedAt)))s ago · Not live video"
    }
}
