# macOS owned monitor: controller and read-only capture slice

Builds on PR #927. This slice connects an owned CGVirtualDisplay lifecycle to existing MCP tools without granting global input or reusing Xvfb privacy assumptions.

Contract: bounded same-binary main-thread helper; private pipes; daemon-scoped ownership; opaque generation selectors; first-frame capture validation; cancellation-safe teardown; original IAM plus explicit shared-session display authority; no numeric-ID adoption or primary-display fallback.

Creation, capture and destruction must remain independently testable with hermetic transports. Native smoke is explicitly opt-in and limited to test-owned resources. The installed daemon is not changed by this work.

Out of scope: global mouse/keyboard injection, clipboard isolation, AX routing, browser placement, dashboard/peer streaming and merging #925/#927/#928. Input will need a separate verified design and fixture acceptance, not a claim that a second monitor is a separate login session.
