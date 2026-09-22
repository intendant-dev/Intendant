# Direction-bound retained-receiver ArrowLeft

Follow-up to merged #948 (`e5db8198`). This extends the same transaction engine
with one fixed, unmodified ArrowLeft down/up pair; it does not add arbitrary
keys, text, modifiers, chords, activation, a hidden selection click or retries.

## Public and private contracts

`prepare_macos_window_arrowleft {binding}` requires OwnerSurface and DisplayView.
`press_macos_window_arrowleft {binding,token}` requires OwnerSurface and DisplayInput.
The facade routes are `inspect display prepare-arrowleft BINDING` and
`act display arrowleft BINDING TOKEN`. All schemas derive from the typed tool
methods; both structs reject additional fields. Existing ArrowRight method names,
argument shapes and receipt field shapes remain unchanged.

The helper has ONE pending key preparation across both directions, with the
existing ten-second monotonic expiry. The immutable key enum permits only
ArrowLeft and ArrowRight. A different-direction dispatch that reaches the helper
consumes and refuses the preparation before constructing or posting any event.
Refreshing either direction replaces the other. Ingress/broker refusals cannot
mutate helper state. Broker validation pins the key in both preparation and
action responses to the requested operation; a wrong-direction helper response
retires the helper, conservatively preserving uncertainty after possible input.

The exact receiver object and original ancestry, process start identity, retained
AX/CG window, monitor generation and geometry, enabled/nonprotected text role,
and human-focus witnesses are checked by the same engine as #948. Its operation
budget, AX timeouts, invalidation rules, held-human-input checks and read-only
receiver inspection semantics are unchanged. The two directions share the
existing eight-object native source hold pool, not separate capacity limits.

Cancellation before dispatch prevents effects. Partial posting, late liveness
failure and lost receipts remain uncertain, never a reason to replay or send a
corrective key. Dispatch is not application-effect verification. The common
uncertainty error prefix is now 'keyboard arrow effects unconfirmed'; callers
should use the structured effect fields, not key-specific prose. Generic
macos_virtual input and keyboard receiver-inspection capability flags stay false.

## Native implementation

Only the existing bound_arrow.rs / bound_arrow.m island changes. Its internal
closed 0/1 direction selector maps to SDK kVK_RightArrow/kVK_LeftArrow and
NSRightArrowFunctionKey/NSLeftArrowFunctionKey. Other values fail before native
allocation. Both halves are preallocated with zero Quartz modifiers, no repeat,
a private event source, exact target process/window and correlation tag. Readback
must agree on direction in both keycode and Unicode. The adjacent one-shot
posting loop, exception accounting, thread affinity and cleanup remain unchanged.
The Rust public native wrapper exposes fixed create/create_left methods rather
than a caller-supplied keycode. No new dependency or OS privilege is introduced.

These are application/OS-reported observations, not atomic routing locks or an
independent input seat. Content/selection is not frozen by a preparation; an app
may interpret an arrow differently. Protection metadata is not a content sandbox.

## Regression and native acceptance

The hermetic tests exercise both directions through the receiver/refusal matrix,
including replacement, changed ancestry/protection, expiry, refresh, partial
posting, post-effect changes, source capacity, cancellation and late liveness.
Additional cases pin cross-direction consumption, shared pending state, the
constructed native key and wrong-direction helper responses. HTTP/facade IAM
classification and strict input schemas cover the new fixed entry points.

The browser fixture accepts either explicit --chromium-arrowleft or the existing
--chromium-arrowright, never both. Both still require --chromium-keyboard-target
and --chromium-keyboard-target-click-first as separate opt-ins. Only the test
setup clicks; neither keyboard tool silently establishes focus. The left positive
path uses the facade over real HTTP and requires trusted ArrowLeft down/up events,
exact caret movement 2 to 1, and unchanged synthetic field value. A separate fresh
right fixture must still show 2 to 3. Each run checks both cross-direction refusal
orders, receiver replacement/protection, replay, stale binding and owned cleanup.
CDP only sets up/observes the synthetic page and closes the verified test browser;
it never injects the tested input. DOM lacks native event-tag attribution.

Reproduce with the existing verify-macos-monitor-http.py command from #948,
replacing --chromium-arrowright with --chromium-arrowleft for a fresh profile.
Use absolute paths, the system Python with Pillow, and build with the installed
compile governor. Each real run creates/removes a test-owned shared-session
virtual monitor. Normal tests and constructor checks do not post OS input.

Validation counts, exact published-head acceptance and landing status are
recorded on PR #949, not inferred from CI or the build. The running plugin daemon
is not upgraded or restarted by this slice.

Primary native references: Apple Cocoa Event Handling Guide, Handling Key Events
(https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/EventOverview/HandlingKeyEvents/HandlingKeyEvents.html)
and the local SDK Carbon/HIToolbox Events.h and AppKit NSEvent.h declarations.

## Local validation

The complete local battery passed: 6,658 top-level binary tests (10 existing
ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E,
workspace Clippy with warnings denied, formatting and whitespace. These counts
exclude nested child-process test invocations printed into the parent log.
All 128 Python evidence/harness tests and 58 native nonposting constructor,
direction-readback, exception and replay cases passed. Browser and pattern
fixtures compile with warnings denied. Published-head native acceptance and
cross-platform CI remain separate gates; these results alone claim no live
keyboard delivery. No live plugin daemon, OS permissions or runner configuration
were changed.
