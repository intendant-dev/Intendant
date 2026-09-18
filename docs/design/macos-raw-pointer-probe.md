# Raw pointer probe and concurrent-owner evidence (2026-09-18)

## Scope

This test-infrastructure slice follows merged #939 (`19cf73c`). It does not add a
raw-input MCP tool, change daemon capabilities, or enable generic input on an
owned virtual monitor. The owner was actively using the Mac during development;
only nonposting construction tests and hermetic tests were run. No native live
click, keyboard event, monitor hotplug, GUI capture or user-content access was
performed for this slice.

## Concurrent desktop observations

`scripts/macos_input_evidence.py` separately reports sampled foreground-app,
pointer and clipboard-change-count differences. Their cause is **undetermined**:
owner activity may explain them, but samples cannot establish attribution.
Missing or malformed observations stay unknown, not unchanged. Even equal
samples never claim continuous isolation.

The Chromium harness adds `desktop_observation_assessment` on success and failure.
Its existing raw observations and `human_observations_unchanged` field remain.
Target foreground detection still refuses, and native per-action focus checks
are unchanged. A whole-run pointer movement is not itself an action-focus failure,
nor does the new report turn a failed or partial native action into success.
No keyboard-event monitoring or clipboard-content reads are added.

## Default: construction only

`tests/fixtures/macos-monitor/raw-pointer.m` defaults to `--self-test`. It creates
Quartz event objects, reads their fields back, and releases them. It does not
create NSApplication, a window or a display, and it never posts an event.
Thirteen cases cover paired left-down/up at negative, zero and positive fractional
coordinates; explicit zero modifiers/deltas; click count and pressure; window
hints, process IDs and correlation tag; and rejected zero identities, bad event
types, nonfinite and excessive coordinates.

The initial native readback test incorrectly compared the event source's state
ID with the `kCGEventSourceStatePrivate` creation selector. The SDK specifies a
new unique state-table ID. The corrected check compares the event with
`CGEventSourceGetSourceStateID(source)` and rejects the HID/combined global tables.
The original failed report is retained; it also posted zero events.

## Explicit experimental live mode

`--allow-disposable-process-click` is a separate opt-in. It creates a disposable,
nonactivating panel that cannot become key or main. Both events are preallocated
and checked. The only posting calls target `getpid()`: there is no PID/window/
coordinate input, global CGEventPost, pointer warp, event tap, keyboard event,
focus restoration, retry, or alternate routing path. Existing Accessibility
permission is required without prompting. Unavailable observations, an observed
foreground target, or a held human left mouse button refuse before posting.

The fixture consumes its own application's event queue. Its canvas records only
tagged mouse-down/up receipts; untagged events increment a counter without
retaining their contents. A random per-run tag is carried in
`kCGEventSourceUserData`. A matching tag is diagnostic correlation, **not an
authentication credential or a new IAM grant**. Receiver PID, source PID, exact
window, coordinates and pair order must agree. Receipt presence alone does not
verify a canvas effect: a separate tagged canvas click count must equal one.
Partial, reversed or duplicate pairs, wrong identities and mismatching coordinates
never pass. The supervisor also binds the reported process identity to its own
Popen child. `posted_events` counts posting calls, not confirmed deliveries.

This self-process pilot is deliberately NOT cross-process or Chromium acceptance.
Its live mode was compiled but **not executed** in this continuation. Setting
window-related event fields alone is not evidence of correct application routing.
The process-private event state is also not a separate WindowServer/input seat.

## Reproduction

```sh
clang -fobjc-arc -Wall -Wextra -Werror \
  -framework AppKit -framework CoreGraphics -framework ApplicationServices \
  tests/fixtures/macos-monitor/raw-pointer.m -o /tmp/intendant-raw-pointer-probe-bin
python3 scripts/test-macos-input-evidence.py -v
python3 scripts/test-macos-chromium-harness.py -v
# Safe default: constructs events; creates no application/window and posts none.
# The report path must not already exist. It is reserved with mode 0600 first.
python3 scripts/verify-macos-raw-pointer.py \
  --fixture /tmp/intendant-raw-pointer-probe-bin --report /tmp/raw-pointer-construction.json
```

The supervisor bounds stdout, stderr and child lifetime, preserves failed native
results, rejects duplicate/nonfinite JSON, and reserves a non-overwriting private
report before any possible effect. It kills/reaps only its own child on errors.
Hermetic receipt tests do not constitute native event delivery evidence.

## Remaining promotion gates

Live self-process delivery and independent effect acceptance come first. Then a
separate cross-process receiver and Chromium/canvas fixture must demonstrate
exact routing and retained process/window lifetime, with explicit shared-session
input consent. Production integration additionally needs binding/monitor
generation, coordinate conversion, current authorization, single-use dispatch,
and cancellation/partial-pair handling. None is silently supplied by this probe.
Keyboard/chords and an independent focus/clipboard session are separate work.

## Primary contracts

The local Apple SDK's `CGEventSource.h` documents unique private state-table IDs;
`CGEvent.h` documents process-directed posting, and `CGEventTypes.h` documents
source user data and window fields. Online references:

- https://developer.apple.com/documentation/coregraphics/cgeventsource/sourcestateid
- https://developer.apple.com/documentation/coregraphics/cgevent/posttopid(_:)
- https://developer.apple.com/documentation/coregraphics/cgeventfield/eventsourceuserdata

The probe deliberately does not assume these APIs guarantee background routing,
continuous isolation, or compatibility with arbitrary applications.

## Final local regression

6,569 binary tests (10 existing ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace all passed. Both Python suites passed (16 + 8 tests). Native construction-only acceptance passed with 13 cases, no application/window and zero posted events.
