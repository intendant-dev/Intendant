# Chromium placement and bounded-tree follow-up (2026-09-18)

## Delivered scope

Two narrow corrections on merged #936 (6aef7b1), not general Chromium computer
use or raw canvas input:

1. Owned-window placement writes only components that need changing. Both AX
   and CG must already match a skipped component exactly. All identity, geometry,
   containment, permission and observed-focus checks remain. Counts 0/1/2 name
   actual setter-path attempts. Observation no longer requires both components to
   be writable; each real setter still checks its own capability before mutation.
   Receipt failure preserves a known zero-write result rather than inventing an
   uncertain input effect. Unknown dispatch remains conservative.
2. Semantic traversal depth is 16 instead of 8. A read-only native diagnostic of
   this small Chromium fixture measured 51 nodes, depth 13 and at most 6 children.
   The 128-node, 32-child, 16-control, 16 KiB wire and four-second operation limits
   are unchanged. Exact retained ancestry and protected-subtree checks remain.

No MCP tool, grant, global input path, private event posting API or installed
application configuration is added. The running plugin daemon is not upgraded.

## Reproduction and evidence

Chrome for Testing 153.0.8010.52 (mac-arm64) was downloaded from Google's official
Chrome for Testing distribution into this worktree's ignored target directory;
it was not installed into Applications. Every launch uses a fresh private
profile, mock keychain, disabled synchronization/background networking and a
synthetic local CSP-restricted page. No existing tabs or user documents are used.

The first app-mode launch made the browser foreground despite the nonactivating
NSWorkspace request. That attempt stopped before input, and was cleaned up. The
fixture now starts without a window and creates its test window with CDP's
explicit background option. It records sampled foreground PID, pointer and
clipboard change count (never clipboard contents), and refuses observed browser
activation. User activity can change those samples. They do not prove continuous
focus, pointer or clipboard isolation; this is still one shared WindowServer.

The original production placement moved a 720 x 530 window successfully, then
failed a redundant resize with `AXSize is not settable`. A deterministic regression
failed on the original implementation. The corrected native placement-only
profile passed: one move, matching AX/CG readback and independent browser bounds,
then a verified zero-write placement, exact unbind/browser termination/profile
cleanup, monitor capture/recovery tests and original display inventory restoration.

The **full semantic profile is not passing**. After the depth correction, it
refused `retained AX parent changed at depth 7 for AXWebArea`. No element tokens
were published and no semantic action was dispatched in that Chromium run.
The mismatch has not been bypassed, nor treated as evidence that arbitrary
Chromium controls are supported. It requires a separate exact-parent graph
investigation. Raw canvas input is neither implemented nor exercised.

Other native attempts refused unavailable focus. The standard-AppKit regression
independently verified text, one button effect, protected-subtree handling and
replay/replacement refusal, but later failed an optional protected-content read;
that overall run is recorded as a failure, not a full acceptance pass. Each
report is retained separately. Failed/uncertain mutations are never retried.

## Fixtures and acceptance profiles

- `browser.m` owns the exact NSRunningApplication returned for the explicit test
  bundle. It serializes its launch completion onto its run-loop thread, terminates
  only that retained application, and reports cleanup. Its optional diagnostic
  reads bounded roles/counts from its sole disposable window, never titles or values.
- `browser.html` has a normal field, button counter, password/disabled controls and
  a canvas counter. Only synthetic contents exist. CDP may set up the window and
  observe these counters, or explicitly replace the fixture's own field for the
  stale-object test. It never types or clicks on behalf of a tested AX action.
- `verify-macos-chromium-controls.py` uses real ctl -> HTTP -> native tools for
  tested placement/actions. The separate `placement_only` profile never reports
  a semantic-control pass. The default `semantic_controls` profile preserves the
  observed unsupported case. Its loopback-only CDP client and child output have
  byte/deadline bounds, with hermetic protocol/error/cleanup tests.

```sh
# Pin a downloaded Chrome for Testing bundle; no auto-update or normal profile.
clang -fobjc-arc -framework AppKit -framework CoreGraphics -framework ApplicationServices \
  tests/fixtures/macos-monitor/browser.m -o /tmp/intendant-chromium-supervisor
python3 scripts/test-macos-chromium-harness.py -v
python3 scripts/verify-macos-monitor-http.py \
  --bin /absolute/worktree/target/debug/intendant \
  --fixture /tmp/intendant-monitor-pattern \
  --chromium-app '/absolute/Google Chrome for Testing.app' \
  --chromium-supervisor /tmp/intendant-chromium-supervisor \
  --chromium-placement-only --allow-shared-session-monitor --check-recovery \
  --report /tmp/chromium-placement.json
# Omit --chromium-placement-only to exercise/report the currently blocked
# full semantic profile; a placement-only pass is not a substitute for it.
```

## Validation

Local Rust battery: 6,562 binary tests (10 existing ignores), 1,022 required
library/acceptance tests (3 existing ignores), 57 E2E tests; workspace Clippy
with warnings denied, formatting and whitespace passed. Six hermetic Python
harness tests passed on macOS. The compiler governor and CI policy are unchanged.
The merge-group matrix remains the cross-platform runtime landing gate.
