# macOS owned monitor: controller and read-only capture slice

Builds on PR #927. This slice connects an owned CGVirtualDisplay lifecycle to existing MCP tools without granting global input or reusing Xvfb privacy assumptions.

Contract: bounded same-binary main-thread helper; private pipes; daemon-scoped ownership; opaque generation selectors; first-frame capture validation; cancellation-safe teardown; original IAM plus explicit shared-session display authority; no numeric-ID adoption or primary-display fallback.

Creation, capture and destruction must remain independently testable with hermetic transports. Native smoke is explicitly opt-in and limited to test-owned resources. The installed daemon is not changed by this work.

Out of scope for this capture slice: global mouse/keyboard injection, clipboard isolation, AX routing, browser placement, dashboard/peer streaming and merging #925/#927/#928. Explicit owner-only AX window binding/placement is implemented separately in `macos-monitor-window-placement.md`, with its validation still pending. Input will need a separate verified design and fixture acceptance, not a claim that a second monitor is a separate login session.

## Independent HTTP pixel acceptance

The opt-in HTTP harness boots a separate controller with an isolated HOME and synthetic default display. Explicit owned-monitor capture still uses the native SCK lane. A disposable nonactivating AppKit panel paints a known four-quadrant pattern only on the newly created non-primary monitor. The test validates PNG geometry/pixels, 0600 artifacts, authentication, input-selector rejection using a zero-duration wait (no input), stale generations, exact destruction and parent-EOF cleanup. It does not replace the installed daemon. Requires existing Screen Recording permission and Python Pillow.

```sh
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/pattern.m -o /tmp/intendant-monitor-pattern
python3 scripts/verify-macos-monitor-http.py --bin /absolute/path/to/built/intendant --fixture /tmp/intendant-monitor-pattern --report /tmp/monitor-http-proof-result.json --allow-shared-session-monitor
```

Monitor hotplug may rearrange windows. The fixture rejects the primary display; the harness refuses ambiguous inventory changes. Display-inventory restoration is not a claim of unchanged window placement or independent focus/clipboard. No native smoke is a default CI test. Native acceptance results must be recorded from an actual run, not inferred from compilation.

## Observed native acceptance

The independent HTTP test passed against the production daemon/tool path on the plugin Mac. The owned 800x600 frame sampled exactly RGB (255,0,0), (0,255,0), (0,0,255) and (255,255,255) at the four quadrant centers. Authentication, artifact mode 0600, non-input rejection of reserved selectors, stale capture/destruction and parent-exit cleanup passed. The original online display inventory and primary display ID were restored; the fixture process was reaped. Temporary images were removed with the rig. Capture timestamp reported by the test system: 2026-09-16T03:25:40.279Z.

This validates the read-only local HTTP daemon path, not an installed ChatGPT plugin upgrade or a separate keyboard/focus/clipboard seat. No existing application windows or documents were used as targets.
