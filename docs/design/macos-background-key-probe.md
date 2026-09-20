# Background keyboard routing and native-click prerequisite (2026-09-20)

Fixture-only investigation on merged #944 (8604614). Production click/scroll,
Rust, IAM, tool declarations and native-input exceptions remain unchanged.
No production keyboard capability or running-daemon upgrade is introduced.

## Observed boundaries

The key-only ArrowRight path still fails receiver acceptance. The non-key
AppKit panel gets two exact tagged queue receipts, but zero panel/view receipts
and zero effects. The corresponding trace has no key or main window, while the
event resolves to the retained panel and its first responder remains the view.
Self-targeted CGEventPostToPSN gives the same result as CGEventPostToPid.

A separately labeled cooperative control forwards only exact tagged queue events
to the retained panel via its normal sendEvent implementation. It gets two panel
and view receipts and one effect while the application remains inactive. This
shows that the fixture receiver works and isolates the application-to-window stop;
it is NOT external native keyboard support. There is no method replacement,
direct keyDown call, activation or key-window acquisition.

Fresh background Chrome for Testing 153.0.8010.52 also receives no DOM keys when
only the synthetic receiver DOM focus is set. Application-targeted
AXUIElementPostKeyboardEvent returned two success codes but no DOM events in a
separate diagnostic. That experiment uses only the retained application AX object,
never the system-wide object, and is not a production fallback.

The successful development experiment explicitly clicked the Chromium receiver
with the existing native window-addressed mouse pair BEFORE posting ArrowRight.
Independent DOM observations then showed one trusted down/up key sequence and
one counter increment. This supports a native-click prerequisite in that tested
runtime/profile, not a universal explanation of Chromium internals or other apps.

## Fixed key and native state

Only ArrowRight is constructed (SDK kVK_RightArrow 124 and
NSRightArrowFunctionKey). NSEvent carries the destination window. Private Quartz
source state, source/target PID, tag, Unicode, key code and no autorepeat are
verified. Quartz flags are zero; decoded AppKit flags are exactly Function for
this special key. The private source selector is not its generated state ID.
Missing allocation or contradictory readback refuses construction.

Shift, Control, Option, Command, Fn, the physical tested arrow and held mouse
buttons block live keyboard dispatch. Caps Lock is a toggled state, not a
shortcut modifier for this fixed nontext arrow, and is left unchanged. No human
key contents, field values or clipboard contents are read. No new private
keyboard SPI or undocumented numeric field is guessed.

The native AppKit default is construction-only: no application/window or posting.
Explicit modes target only the process-owned nonactivating panel. The PSN mode
resolves only that same live process. Two events are preallocated; exceptions
retain attempted-call counts. The source survives the bounded receiver loop.

## Explicit Chromium click-first mode

The supervisor keeps non-input, pointer-only, key-only and click-key modes
separate. The new --disposable-chromium-click-key mode is selected explicitly
by --native-click-first in verify-macos-chromium-key.py. It permits one native
click followed by one fixed key pair; it never retries a failed key by clicking.
A click is an application effect and must never be silently added to typing.

The first key attempt consumes its opportunity, including malformed input or a
missing/refused prerequisite. The prerequisite must refer to the same retained
browser and exact original clicked window/bounds, with successful native click
posting. Current browser geometry, liveness, permissions and background status
are rechecked. Neither frame accepts an arbitrary caller PID or chosen key.

The harness independently requires one trusted synthetic receiver click at exact
client AND screen coordinates, without modifiers, before sending the key. It
checks unchanged layout and receiver state, distinct click/key correlation tags,
then exact trusted ArrowRight down/up and one counter increment. Native click
and key results and replay counts remain separate and immutable through shutdown.
Replay must produce no additional input or receiver effects.

CDP only opens the local fixture, initializes its state and reads observations;
it does not inject tested input, call event handlers or change counters. DOM
cannot expose native source tags: matching native dispatch and DOM effects do
not authenticate source attribution. Desktop sample changes are unattributed;
equal samples cannot prove continuous focus/pointer/clipboard isolation.

## Evidence and cleanup

The original failed results remain: construction flag mismatch, held-input
refusals with zero posting, AppKit queue-only delivery, and Chromium key-only
no-effect. A queue receipt or successful API return never establishes effect.
The cooperative-control classifier keeps native support false even when that
control works, and the original assessor rejects relabeled cooperative evidence.
Malformed, partial, duplicated, reordered or contradictory receipts stay unknown.

Reports are bounded, reserved exclusively with mode 0600 and never overwritten.
Normal cleanup closes the private browser/profile and reaps the supervisor.
Late native errors and attempt counts are recovered independently of successful
acceptance. Broken-pipe shutdown does not skip final evidence recovery.

An unconfirmed browser exit is failure, not permission to kill its only native
owner. The bounded Python harness retains the private profile and owner reference.
The native key-mode owner can outlive its diagnostic deadline, publishing pending
cleanup without further input or termination retries until the exact browser
reports exit. This residual state has no absolute lifetime guarantee. Actual
hung-browser behavior is not established by policy-only unit tests.

## Reproduction and promotion

Build raw-key.m, key-tests.m and browser.m with clang, ARC/ARC exception cleanup,
-Wall -Wextra -Werror, and AppKit/ApplicationServices/CoreGraphics frameworks.
The scripts under scripts/ use explicit absolute fixture/browser paths.

verify-macos-key-routing.py defaults to nonposting construction. Live diagnostic
routes application, psn and window-control require --allow-disposable-key-routing.
A valid negative still fails native acceptance; a cooperative control can complete
its diagnostic while passed and native_background_delivery_verified remain false.

verify-macos-chromium-key.py requires --allow-disposable-chromium-key, an explicit
Chrome for Testing bundle, its supervisor and a new report. Without
--native-click-first it preserves the key-only comparison. The existing
verify-macos-chromium-pointer.py is an independent mouse regression.

The normal Rust battery, pure Python evidence tests and native nonposting checks
are separate from live acceptance. Exact published-head native results, counts
and review status are recorded on PR #945. No fixture pass automatically enables
production keyboard tools. Retained-window/monitor authority, focused receiver
identity, content changes, cancellation and uncertain results remain production
integration gates. These standalone keyboard tests do not create CGVirtualDisplay.

## Primary references

Apple Cocoa Event Handling Guide, Handling Key Events:
https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/EventOverview/HandlingKeyEvents/HandlingKeyEvents.html

Chromium chrome/browser/chrome_browser_application_mac.mm, ordinary key routing:
https://github.com/chromium/chromium/blob/main/chrome/browser/chrome_browser_application_mac.mm

Hammerspoon application-targeted PSN implementation, compared only on self:
https://github.com/Hammerspoon/hammerspoon/blob/master/extensions/eventtap/libeventtap_event.m

Local macOS SDK NSEvent.h, CGEvent.h/CGEventTypes.h and AXUIElement.h supply
typed public declarations. Source descriptions do not replace native acceptance.


## Resumed validation and acknowledgement contract

A fresh click-first Chromium run passed with the explicit native acknowledgement:
one verified synthetic-receiver click, an exact trusted ArrowRight down/up pair
and one counter increment. Key-only remained a negative comparison; AppKit PID
and PSN routes stopped before their window, and the cooperative window control
worked without becoming external keyboard support. The standalone mouse
regression passed separately. Every completed run closed its test browser and
removed its private profile. Published-commit results are recorded separately.

A successful click posting alone cannot unlock the key. The native owner issues
a fresh challenge after its click. The trusted driver verifies one exact DOM
click, unchanged receiver geometry and layout, and sends a fixed-size receipt
bound to that challenge, the clicked plan and a distinct key tag. The native
owner consumes receipt availability before key validation. Malformed, replayed,
wrong-plan and wrong-tag attempts cannot preserve a reusable receipt. This
handshake checks sequencing and identity; it does not authenticate DOM provenance
or grant production keyboard authority.

All live routing-verifier modes return nonzero while external native support is
false. The cooperative control may report control_diagnostic_completed but never
a passing verifier exit. Unexpected receiver effects on a self-only route
invalidate the diagnostic rather than promoting it to cross-process support.

The continuation corrected a fixture metric error: devicePixelRatio describes
backing pixel density and is not the visual viewport scale. They are now recorded
and frozen separately. The Retina run used backing scale 2 and visual scale 1;
coordinates remained logical points with unchanged native/DOM geometry checks.
The original pre-input scale refusal remains preserved. Missing visual viewport
metadata, zoom/scroll changes, or changed backing scale still refuse.

New regressions cover native receipt state and Python DOM receipt construction.
An initially failing test exposed a missing receiver-rectangle comparison; it
passed after that comparison was added. Cleanup records browser termination
independently of click/replay acceptance, including early-refusal and key-only
paths. Native constructor tests compile with warnings denied.

Reference for viewport scale and device pixel ratio:
https://www.w3.org/TR/cssom-view-1/


## Final review corrections

The resumed independent review found three additional defects. A first key
attempt now seals the later click opportunity before plan freezing, challenge
creation or native posting; key-before-click cannot become a hidden click
fallback. Key-mode shutdown makes at most one ordinary terminate request and
never force-terminates the browser. A late-created browser still receives that
single request; an unconfirmed exit preserves its native owner and source.
The existing pointer-only supervisor cleanup policy is unchanged.

Both AppKit evidence assessors now require unrelated_events to be an exact
integer zero. Missing, boolean, floating-point or nonzero values cannot produce
a clean diagnostic or verified effect. Regression coverage includes the ordering
gate, repeated shutdown-policy evaluation, late browser availability and mutated
evidence. The updated pure Python suite passes 103 tests, and the native
nonposting suite passes 79 cases. No genuinely hung browser was injected.


### Orderly browser closure and pending launch

Native ordinary termination alone left two disposable Chromium runs pending.
Their keyboard observations were preserved, but both complete tests failed
cleanup. Recovery verified each private profile and browser PID over its own
CDP connection, sent one Browser.close request, observed the retained native
owner exit, and removed the profile. The original failed reports are unchanged.

The key harness now requests Browser.close once only after verifying that its
private connection belongs to the exact launched fixture browser. It never
issues this request for an unverified connection and never retries a lost reply.
Native observed process exit remains the cleanup proof. This is fixture teardown,
not input injection. If launch completion itself remains pending beyond the
native loop deadline, key modes retain the original callback owner and publish
launch_pending rather than claiming a nil application has terminated. A genuinely
stuck launch/exit may therefore outlive the diagnostic deadline; no bounded
successful cleanup is claimed.

Pure Python coverage now passes 106 tests. Native nonposting coverage includes
83 constructor, receipt, ordering and cleanup-policy cases. The unchanged Rust
implementation tree retains the full successful local battery recorded above.
