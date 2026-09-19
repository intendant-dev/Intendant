# Chromium native canvas pointer acceptance (2026-09-19)

## Delivered scope

Fixture-only follow-up to merged #941 (`b863aa2`). This slice demonstrates one
process/window-addressed native mouse pair against a new Chrome for Testing
153.0.8010.52 profile and a synthetic local canvas. It does not add a daemon or MCP
input tool, change IAM, use the owner's browser profile, or deploy the daemon.
The standalone native browser window used here is not on a CGVirtualDisplay;
owned-monitor routing and retained-window production dispatch are later gates.

The supervisor owns the exact NSRunningApplication returned by its nonactivating
new-instance launch. It checks that application's sole visible layer-zero window,
its unchanged exact CG bounds, existing Accessibility/post-event permissions,
foreground status and the human's left-button state. A private fixed-size pipe
frame contains geometry, a window ID and a correlation tag, never a target PID.
The target is derived from the retained application, not the pipe message.

Both events are prepared and read back before dispatch. A run consumes one
attempt, including malformed/refused frames. A repeated frame increments a refusal
counter without replacing the original dispatch report or posting again. The
private event source remains alive through observation and browser teardown.
There is no global post, cursor warp, activation, keyboard event, rollback, input
retry or alternate routing path. This is a disposable test fixture, not a claim
that a numeric window ID alone is a production-safe capability.

## Coordinate defect found by the first Chromium run

The initial run posted two events and produced one canvas click, but its reported
client/screen Y coordinate was eight points above the intended location. The
acceptance check failed despite the visible effect. A read-only native diagnostic
confirmed agreement between CG window bounds, AX web-area bounds and JavaScript
viewport geometry; a guessed titlebar offset was not the correction.

The private CGEventSetWindowLocation field uses a top-left window-local origin in
the tested runtime. The previous fixture supplied a bottom-left Cocoa coordinate.
#941's center-of-window test was symmetric and therefore could not expose this
reflection. The Chromium plan now uses:

    global_y = window_top + (outer_height - inner_height) + client_y
    quartz_window_y = global_y - window_top

This is derived from current geometry, not a fixed eight-point adjustment. Zoom,
scroll, horizontal browser borders, mismatched bounds and out-of-interior points
refuse rather than being silently normalized. The browser test deliberately uses
an off-center point. A deterministic Python negative confirms the reflected point
cannot pass. Both dimensions and negative global positions have nonposting tests.

The shared fixture constructor now supplies only window addressing through the
NSEvent seed, then writes both global and Quartz window-local coordinates. The
private symbols are runtime-resolved and require readback; absence refuses.
The existing AppKit self/cross-process fixture now also uses an asymmetric point,
with a separately calculated Cocoa expected receiver coordinate. Its initial
conversion retained the old center expectation and failed; after correcting that
expectation, the off-center cross-process receiver verified both coordinates and
one effect. The failed report remains separate from the successful one.

## Evidence and limits

The synthetic page records only its own canvas mousedown/up/click sequence, button
state, trusted flag and client/screen coordinates. It separately increments the
canvas counter. Acceptance requires exactly that sequence, the planned point,
one counter increment, unchanged viewport/native geometry and a successful
single native dispatch record. CDP creates the background test window, arms a
fresh observation nonce and reads fixture state; it does not inject input.
A post call or an isTrusted event alone is insufficient.

DOM does not expose the Quartz correlation tag. The report explicitly sets
`dom_tag_correlation: false`: the sender report plus independently observed DOM
events constitute empirical fixture evidence, not authenticated source attribution.
Concurrent human activity cannot be conclusively excluded by before/after samples.
Whole-run desktop changes remain unattributed, while target activation, missing
observations and failed native dispatch still refuse. No keyboard monitoring or
clipboard-content reads are added.

The supervisor and browser are reaped/terminated before removing the private
profile. Status and CDP responses have byte/time limits; the status tick must
advance. Reports are reserved with mode 0600, must not exist already, and preserve
failures. The owner was using this Mac during development; no user documents,
normal browser profiles, display hotplug or screenshots were involved.

## Validation

The complete local Rust battery on the unchanged production tree passed: 6,569
package-binary tests (10 existing ignores), 1,022 required library/acceptance tests
(3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied,
formatting and whitespace. Child-process filtered test summaries are not counted
again. The compiler governor and CI policy were not modified.

Python coverage: ten new pointer-planning/evidence tests plus the existing 22
raw-pointer and eight Chromium-harness tests. Native nonposting plan validation
has 17 cases; six pipe cases cover a valid frame, truncation, empty input, invalid
version/window and reflected coordinates, with zero posts. The native constructor
and fixtures compile with -Wall -Wextra -Werror.

The Chromium native positive run verified exact off-center coordinates, one canvas
effect, replay refusal and complete cleanup. Published-head confirmation and
merge-group status are recorded in the PR execution record, not inferred from
compile-only PR checks. The original wrong-coordinate result, read-only geometry
probe and intermediate AppKit expectation failure remain retained.

## Reproduction

```sh
clang -fobjc-arc -Wall -Wextra -Werror \
  -framework AppKit -framework CoreGraphics -framework ApplicationServices \
  tests/fixtures/macos-monitor/browser.m -o /tmp/intendant-browser-pointer
clang -fobjc-arc -Wall -Wextra -Werror \
  -framework AppKit -framework CoreGraphics -framework ApplicationServices \
  tests/fixtures/macos-monitor/browser-pointer-tests.m -o /tmp/intendant-pointer-plans
/tmp/intendant-pointer-plans # No application/window and zero posting.
python3 scripts/test-macos-chromium-pointer.py -v
python3 scripts/test-macos-input-evidence.py -v
python3 scripts/test-macos-chromium-harness.py -v
# Explicit real native input only to the newly launched disposable browser:
python3 scripts/verify-macos-chromium-pointer.py \
  --browser-app '/absolute/Google Chrome for Testing.app' \
  --supervisor /tmp/intendant-browser-pointer \
  --allow-disposable-chromium-click --report /tmp/new-pointer-report.json
```

The supplied bundle must identify as Chrome for Testing. No normal browser profile
or installed application is discovered or selected automatically. An explicit
bundle is trusted test instrumentation, not authenticated solely by its bundle ID.
The private location signatures originate in WebKit's
`Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h`; the coordinate-origin conclusion
above comes from these native off-center tests, not a public Apple compatibility
promise.
