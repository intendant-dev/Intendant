import Cocoa
import Foundation

// Default tests are hermetic; native window observation is an explicit opt-in.
var assertions = 0
func check(_ value: @autoclosure () -> Bool, _ message: String) {
    assertions += 1
    if !value() { fputs("FAIL: \(message)\n", stderr); exit(1) }
}
func rejects(_ message: String, _ action: () throws -> Void) {
    var rejected = false
    do { try action() } catch { rejected = true }
    check(rejected, message)
}
let selector = "macos_virtual:" + String(repeating: "a", count: 32) + ":536870912"
func descriptor(_ id: Int = 536870912) -> [String: Any] {
    ["display_target": "macos_virtual:" + String(repeating: "a", count: 32) + ":\(id)",
     "capture_generation": "macos_virtual:" + String(repeating: "a", count: 32) + ":\(id)",
     "display_id": id, "width": 800, "height": 600, "backend": "macos_virtual",
     "lifecycle_ready": true, "ephemeral_capture_supported": true]
}
func result(_ value: [String: Any], image: Data? = nil) -> [String: Any] {
    let text = String(data: try! JSONSerialization.data(withJSONObject: value), encoding: .utf8)!
    var content: [[String: Any]] = [["type": "text", "text": text]]
    if let image = image { content.append(["type": "image", "mimeType": "image/png", "data": image.base64EncodedString()]) }
    return ["content": content, "isError": false]
}
func pngFixture() -> Data {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 800, pixelsHigh: 600,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let count = bitmap.bytesPerRow * bitmap.pixelsHigh
    memset(bitmap.bitmapData!, 70, count)
    for i in stride(from: 3, to: count, by: 4) { bitmap.bitmapData![i] = 255 }
    return bitmap.representation(using: .png, properties: [:])!
}
let fixturePNG = pngFixture()
func frameResult(_ target: String = selector) -> [String: Any] {
    result(["display_target": target, "capture_generation": target, "width":800, "height":600,
            "artifact_retained":false, "capture_ready":true, "captured_at":"2026-09-25T22:00:00.000Z"], image: fixturePNG)
}
let monitor = try! AgentViewMonitor(descriptor())
let second = try! AgentViewMonitor(descriptor(536870913))
let frame = try! AgentViewFrame.decode(frameResult(), monitor: monitor)
final class Handle: AgentViewRequest {
    private(set) var cancelled = false
    func cancel() { cancelled = true }
}
final class Source: AgentViewSource {
    var inventories: [(Handle, (Result<[AgentViewMonitor], Error>) -> Void)] = []
    var captures: [(AgentViewMonitor, Handle, (Result<AgentViewFrame, Error>) -> Void)] = []
    func inventory(_ complete: @escaping (Result<[AgentViewMonitor], Error>) -> Void) -> AgentViewRequest {
        let handle = Handle(); inventories.append((handle,complete)); return handle
    }
    func capture(_ target: AgentViewMonitor, completion: @escaping (Result<AgentViewFrame, Error>) -> Void) -> AgentViewRequest {
        let handle = Handle(); captures.append((target,handle,completion)); return handle
    }
}
func unitTests() {
    check(AgentViewMonitor.validTarget(selector), "canonical selector")
    for bad in ["user_session", "0", "display_99", "macos_virtual", selector.uppercased(), selector+":1",
                " "+selector, selector+" ", selector.replacingOccurrences(of:"536870912",with:"0536870912"),
                selector.replacingOccurrences(of:"536870912",with:"1073741824")] {
        check(!AgentViewMonitor.validTarget(bad), "reject implicit/malformed selector")
    }
    for (field, value) in [("display_id",true as Any),("display_id",536870913 as Any),
                           ("width",true as Any),("width",63 as Any),("height",1.2 as Any),
                           ("ephemeral_capture_supported",1 as Any),("ephemeral_capture_supported",false as Any),
                           ("capture_generation","user_session" as Any),("lifecycle_ready",false as Any)] {
        var row = descriptor(); row[field] = value
        rejects("descriptor \(field)") { _ = try AgentViewMonitor(row) }
    }
    var old = descriptor(); old.removeValue(forKey:"ephemeral_capture_supported")
    rejects("old daemon not silently writing artifacts") { _ = try AgentViewMonitor(old) }
    check(try! AgentViewWire.inventory(result(["broker_state":"live","monitors":[descriptor()]])).count == 1, "inventory")
    for rows in [[descriptor(),descriptor()], [descriptor(),descriptor(536870913),descriptor(536870914)]] {
        rejects("bounded unique inventory") { _ = try AgentViewWire.inventory(result(["broker_state":"live","monitors":rows])) }
    }
    check(try! AgentViewWire.inventory(result(["broker_state":"not_started","monitors":[]])).isEmpty, "nonstarting inventory")
    rejects("unavailable not empty success") { _ = try AgentViewWire.inventory(result(["broker_state":"unavailable","monitors":[]])) }
    rejects("retired cannot retain monitors") { _ = try AgentViewWire.inventory(result(["broker_state":"retired","monitors":[descriptor()]])) }
    check(frame.png == fixturePNG, "verified inline frame")
    for (field,value) in [("display_target","user_session" as Any),("capture_generation",second.target as Any),
                           ("width",640 as Any),("artifact_retained",true as Any),("screenshot_path","/tmp/leak" as Any),
                           ("capture_ready",1 as Any)] {
        var metadata = try! AgentViewWire.content(frameResult()).0; metadata[field] = value
        rejects("frame metadata \(field)") { _ = try AgentViewFrame.decode(result(metadata,image:fixturePNG),monitor:monitor) }
    }
    var data = fixturePNG; data[0] = 0
    rejects("not PNG") { _ = try AgentViewFrame.decode(result(try AgentViewWire.content(frameResult()).0,image:data),monitor:monitor) }
    var wrongPixels = fixturePNG; wrongPixels[19] = 1
    rejects("PNG dimensions not metadata") { _ = try AgentViewFrame.decode(result(try AgentViewWire.content(frameResult()).0,image:wrongPixels),monitor:monitor) }
    let rpc: [String:Any] = ["jsonrpc":"2.0","id":"fixture-request","result":frameResult()]
    let rpcData = try! JSONSerialization.data(withJSONObject:rpc)
    _ = try! AgentViewWire.result(rpcData,id:"fixture-request")
    rejects("wrong request ID") { _ = try AgentViewWire.result(rpcData,id:"other") }
    for invalid in [["content":[],"isError":true],["content":[],"isError":0],["content":[]]] as [[String:Any]] {
        rejects("error/invalid content") { _ = try AgentViewWire.content(invalid) }
    }

    var now: TimeInterval = 10
    let source = Source(); let model = AgentViewModel(source:source,clock:{now})
    model.tick(); check(source.captures.isEmpty && source.inventories.isEmpty, "initially idle")
    model.show(); check(source.inventories.count == 1, "show inventories")
    source.inventories[0].1(.success([monitor,second]))
    check(model.selected == nil && source.captures.isEmpty, "no auto selection")
    model.select(monitor); check(source.captures.count == 1 && model.busy, "one pending frame")
    model.tick(); model.tick(); check(source.captures.count == 1, "single flight")
    source.captures[0].2(.success(frame)); check(model.frame != nil && !model.busy, "frame accepted")
    now = 11; model.tick(); check(source.captures.count == 1 && model.freshness.contains("1s"), "freshness and pacing")
    now = 12; model.tick(); check(source.captures.count == 2, "next bounded frame")
    model.hide(); check(source.captures[1].1.cancelled && model.frame == nil && !model.visible, "hide cancels and drops image")
    source.captures[1].2(.success(frame)); check(model.frame == nil, "late hidden reply ignored")
    now = 50; model.tick(); check(source.captures.count == 2, "hidden does no work")
    model.show(); source.inventories[1].1(.success([monitor,second]))
    check(source.captures.count == 3 && model.selected == monitor, "reopen revalidates retained selection")
    model.select(second); check(source.captures[2].1.cancelled && source.captures.count == 4, "retarget cancels")
    source.captures[2].2(.success(frame)); check(model.frame == nil, "old target reply ignored")
    source.captures[3].2(.failure(AgentViewError.noPermission))
    check(model.frame == nil && model.message.contains("not authorized"), "no stale pixels after denial")
    now = 54; model.tick(); check(source.captures.count == 4, "failure backoff")
    now = 55; model.tick(); check(source.captures.count == 5, "bounded reobservation")
    model.reload(); check(source.captures[4].1.cancelled, "inventory refresh cancels frame")
    source.inventories[2].1(.success([])); check(model.selected == nil && model.frame == nil, "destroyed monitor cleared")
    model.tick(); check(source.captures.count == 5, "no fallback to other display")
    model.show(); model.hide(); check(source.inventories[3].0.cancelled, "hide during inventory")
    source.inventories[3].1(.success([monitor])); check(!model.visible && model.frame == nil, "late inventory cannot reopen")
    let screen = NSRect(x:0,y:0,width:1440,height:900)
    let clamped = AgentViewController.clamp(NSRect(x:-900,y:10000,width:9000,height:9000),to:screen)
    check(screen.contains(clamped), "offscreen restored bounds clamped")
    print("{\"passed\":true,\"assertions\":\(assertions),\"native_windows\":0,\"native_input_calls\":0}")
}
func pump(_ duration: TimeInterval) { RunLoop.main.run(until:Date(timeIntervalSinceNow:duration)) }
func windowSmoke() {
    _ = NSApplication.shared
    NSApp.setActivationPolicy(.accessory)
    let front = NSWorkspace.shared.frontmostApplication?.processIdentifier
    let source = Source(); let suite = "intendant-agent-view-test-"+UUID().uuidString
    let defaults = UserDefaults(suiteName:suite)!
    defer { defaults.removePersistentDomain(forName:suite) }
    let view = AgentViewController(source:source,defaults:defaults)
    check(!view.panel.canBecomeKey && !view.panel.canBecomeMain, "panel refuses key/main")
    view.show(); source.inventories.last!.1(.success([monitor])); view.model.select(monitor)
    source.captures.last!.2(.success(frame)); pump(0.25)
    check(view.panel.isVisible, "panel visible")
    check(!view.panel.isKeyWindow && !view.panel.isMainWindow, "panel not focused")
    check(NSWorkspace.shared.frontmostApplication?.processIdentifier == front, "show preserves foreground application")
    if CommandLine.arguments.contains("--save-render"), let content = view.panel.contentView {
        let image = content.bitmapImageRepForCachingDisplay(in:content.bounds)!
        content.cacheDisplay(in:content.bounds,to:image)
        try! image.representation(using:.png,properties:[:])!.write(to:URL(fileURLWithPath:"target/agent-view-proof/synthetic-panel.png"))
    }
    view.panel.performClose(nil); pump(0.05)
    check(!view.panel.isVisible && view.model.frame == nil, "close means hide and forget pixels")
    let captured = source.captures.count
    pump(1.1); check(source.captures.count == captured, "hidden timer stops")
    view.show(); source.inventories.last!.1(.success([monitor])); pump(0.05)
    check(view.panel.isVisible, "reopen")
    view.hide(); view.shutdown()
    check(NSWorkspace.shared.frontmostApplication?.processIdentifier == front, "full lifecycle preserves foreground")
    print("{\"passed\":true,\"window_assertions\":\(assertions),\"native_input_calls\":0,\"foreground_unchanged\":true}")
}
if CommandLine.arguments.contains("--window-smoke") { windowSmoke() } else { unitTests() }
