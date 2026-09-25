# macOS Agent View (2026-09-25)

First slice: an explicitly opened, read-only, nonactivating floating snapshot preview of one exact daemon-owned virtual monitor. Hide closes only the viewer and cancels capture work, never the agent or monitor. Reopen from the native app menu. Show frame age and unavailable/disconnected states; no primary fallback, input forwarding, clipboard sync, persisted frames, activation or new display authority. Reuse the owner/DisplayView screenshot API. Streaming, direct control, automatic reveal and agent-status association remain separate follow-ups.


## Implemented snapshot slice

Window > Show Agent View opens a floating read-only NSPanel on the physical main display. Explicit monitor selection is required; no primary-display or numeric fallback. Close/Hide releases pixels and cancels the view timer/request without stopping the agent, daemon or monitor. Reopen revalidates the previously selected generation. Window bounds are saved locally; images, selected identities and credentials are not saved as preferences. Expansion changes viewer size only. Backend changes and display-layout changes hide/disconnect the view.

The native transport reuses the app-supervised loopback endpoint, boot admission token and existing TLS/client-certificate delegate. It rejects redirects, disables disk cache/cookies, caps responses at 32 MiB and PNGs at 18 MiB, and has no generic tool or input interface. Requests are serial, at least two seconds apart after success and five after failure; hiding stops refresh. Frame age is labeled time since receipt, not smooth live video or proof of agent actions. Missing/denied/invalid frames clear pixels; late callbacks cannot repopulate a hidden or retargeted panel.

The existing take_screenshot tool now accepts optional ephemeral:true, default false. It is allowed only with an exact owned macos_virtual selector and inline output. The same broker, DisplayView authorization, capture stop, generation rechecks and receipt protocol apply. Memory-only capture skips all file storage. Inventory advertises ephemeral_capture_supported so old daemons cannot silently save periodic screenshots. Generic streaming/input capabilities remain false.

The first slice does not implement agent/session status association, Stop Agent, automatic reveal, fullscreen guarantees, streaming or direct control. No background-workspace sandbox or independent focus seat is claimed.

## Tests

Compile macos-app/tests/agent-view/main.swift with AgentViewModel.swift, AgentViewTransport.swift and AgentView.swift, -swift-version 5 -warnings-as-errors -framework Cocoa. Default execution is hermetic model/wire testing. --window-smoke explicitly opens only a synthetic nonactivating panel; --save-render saves only its synthetic contents under target/agent-view-proof.

Compile macos-app/tests/agent-view-live/main.swift with those same files. scripts/verify-macos-agent-view.py --bin PATH --fixture PATH --viewer PATH --report FRESH_JSON --allow-shared-session-monitor starts only an isolated temporary-HOME mock-provider daemon and test-owned monitor. It verifies known synthetic pixels through the actual native transport, hide/reopen survival, clearing on exact monitor destruction, no additional screenshot files, and teardown. It never installs or starts the packaged app. Native acceptance is opt-in, not CI.
