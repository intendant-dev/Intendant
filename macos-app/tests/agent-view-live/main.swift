import Cocoa
import Foundation

// Explicit test-only host. Never launches the installed app or changes its daemon.
let env = ProcessInfo.processInfo.environment
let output = URL(fileURLWithPath: env["AGENT_VIEW_TEST_REPORT"] ?? "/invalid-agent-view-test-report")
var evidence: [String:Any] = ["passed":false,"phase":"starting","native_input_calls":0]
func save() throws { try JSONSerialization.data(withJSONObject:evidence,options:[.prettyPrinted,.sortedKeys]).write(to:output,options:.atomic) }
func require(_ ok: Bool, _ detail: String) throws {
    guard ok else { throw NSError(domain:"AgentViewTest",code:1,userInfo:[NSLocalizedDescriptionKey:detail]) }
}
func waitUntil(_ detail:String, _ predicate:() -> Bool) throws {
    let end = ProcessInfo.processInfo.systemUptime + 18
    while !predicate() && ProcessInfo.processInfo.systemUptime < end { RunLoop.main.run(until:Date(timeIntervalSinceNow:0.05)) }
    try require(predicate(),detail)
}
func pattern(_ frame: AgentViewFrame?) throws {
    guard let frame=frame, let bitmap=NSBitmapImageRep(data:frame.png) else { throw AgentViewError.invalidReply }
    try require(bitmap.pixelsWide==800 && bitmap.pixelsHigh==600,"exact frame dimensions")
    let points=[(200,150),(600,150),(200,450),(600,450)]
    let expected:[[CGFloat]]=[[1,0,0],[0,1,0],[0,0,1],[1,1,1]]
    for (index,p) in points.enumerated() {
        guard let c=bitmap.colorAt(x:p.0,y:p.1)?.usingColorSpace(.sRGB) else { throw AgentViewError.invalidReply }
        try require(zip([c.redComponent,c.greenComponent,c.blueComponent],expected[index]).allSatisfy { abs($0-$1)<0.3 },"known pattern mismatch")
    }
}
func run() throws {
    guard let port=Int(env["AGENT_VIEW_TEST_PORT"] ?? ""), let token=env["AGENT_VIEW_TEST_TOKEN"],
          let target=env["AGENT_VIEW_TEST_TARGET"], AgentViewMonitor.validTarget(target) else { throw AgentViewError.invalidReply }
    _ = NSApplication.shared; NSApp.setActivationPolicy(.accessory)
    let front=NSWorkspace.shared.frontmostApplication?.processIdentifier
    let source=AgentViewTransport(scheme:"http",port:port,token:{token},authenticate:{_,_,done in done(.cancelAuthenticationChallenge,nil)})
    let suite="intendant-agent-view-live-"+UUID().uuidString
    let defaults=UserDefaults(suiteName:suite)!
    let view=AgentViewController(source:source,defaults:defaults)
    defer { view.shutdown(); defaults.removePersistentDomain(forName:suite) }
    view.show()
    try waitUntil("inventory deadline") { !view.model.busy }
    try require(view.model.selected==nil,"unexpected automatic selection")
    guard let monitor=view.model.monitors.first(where:{$0.target==target}) else { throw AgentViewError.unavailable }
    view.model.select(monitor)
    try waitUntil("first capture deadline") { !view.model.busy }
    try pattern(view.model.frame)
    try require(!view.panel.isKeyWindow && !view.panel.isMainWindow,"panel took keyboard focus")
    try require(view.panel.frame.width<=500,"preview expanded beyond compact bounds")
    evidence["first_frame_pattern_verified"]=true
    evidence["frame_freshness_label"]=view.model.freshness
    view.hide(); RunLoop.main.run(until:Date(timeIntervalSinceNow:2.2))
    try require(!view.model.visible && !view.model.busy && view.model.frame==nil && !view.panel.isVisible,"hidden view retained work or pixels")
    evidence["hide_forgets_pixels"]=true
    view.show()
    try waitUntil("reopen capture deadline") { !view.model.busy && view.model.frame != nil }
    try require(view.model.selected==monitor,"reopen switched generation")
    try pattern(view.model.frame)
    evidence["reopen_pattern_verified"]=true
    evidence["phase"]="waiting_for_destroy";try save()
    // Parent deletes only its exact test-owned monitor. The next capture must refuse.
    try waitUntil("destroyed display frame did not clear") { view.model.frame==nil && !view.model.busy }
    evidence["removed_display_clears_pixels"]=true
    evidence["layout_change_hid_preview"] = !view.panel.isVisible
    RunLoop.main.run(until:Date(timeIntervalSinceNow:0.3))
    view.show()
    try waitUntil("removed inventory deadline") { !view.model.busy }
    try require(view.model.selected==nil && view.model.monitors.isEmpty,"removed generation was reused")
    try require(NSWorkspace.shared.frontmostApplication?.processIdentifier==front,"foreground application changed during test")
    evidence["foreground_unchanged"]=true
    evidence["removed_display_no_fallback"]=true
    evidence["passed"]=true;evidence["phase"]="finished";try save()
}
do { try run() } catch { evidence["error"]=error.localizedDescription; evidence["phase"]="failed";try? save();fputs("Agent View native acceptance failed\n",stderr);exit(1) }
print("AGENT_VIEW_LIVE_PASS")
