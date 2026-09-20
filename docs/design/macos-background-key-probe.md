# Background keyboard routing probe (2026-09-20)

This fixture-only investigation follows merged #944 (`8604614`). Production
click and bounded vertical scrolling remain unchanged. No keyboard tool, daemon
capability, authorization exception or production native-input FFI is added.

## Result and promotion boundary

The tested addressed ArrowRight path has **not passed receiver acceptance**.
The non-key AppKit panel received a matching tagged down/up pair in its application
queue, but neither event reached its view and the effect counter remained zero.
A separately owned Chrome for Testing 153.0.8010.52 window had an internally
focused DOM receiver, but two native posting attempts produced no DOM key events
and no counter change. Neither result is a successful keyboard operation.

The observations separate construction, posting, application-queue delivery,
first-responder delivery and application effects. They do not establish the full
cause of the missing delivery. AppKit's documented key-window/responder routing
is a relevant next investigation, not proof that a particular private API will
solve the problem. No activation, key-window acquisition, direct view dispatch,
CDP Input call or global-input fallback was used to manufacture success.

This is not evidence that CGVirtualDisplay cannot support keyboard use. The new
keyboard tests did not hotplug a monitor or use the daemon: they isolate input
routing first. Exact owned-monitor/binding authorization, process/window/document
identity, focus, token consumption and cancellation are separate production gates.

## Fixed key and native construction

`key-event.h` constructs only ArrowRight (SDK kVK_RightArrow, 124), with the
NSRightArrowFunctionKey Unicode character. The NSEvent seed carries the exact
window number. A private Quartz source, target/source PID, correlation tag, zero
Quartz modifier flags and zero autorepeat are set and verified along with
NSEvent type, key code, destination window, Unicode and decoded flags.

On the tested runtime, AppKit adds NSEventModifierFlagFunction to this special
key when decoding a zero-flag Quartz event. The initial test incorrectly demanded
zero in both representations and failed without posting. The corrected test pins
this exact distinction; it does not accept arbitrary flags. Missing allocation
or contradictory readback refuses construction. No new private keyboard SPI or
undocumented numeric event field is introduced.

Shift, Control, Option, Command, Fn, the tested physical arrow and held mouse
buttons block live dispatch. Caps Lock is a toggled state, not a shortcut modifier
for this fixed nontext arrow; it is left unchanged and does not authorize text or
shortcuts. No human keystroke contents, field values or clipboard contents are read.

## Native AppKit probe

`raw-key.m` defaults to nonposting construction and creates no application/window.
Its explicit `--allow-disposable-key` mode creates its own nonactivating panel,
which cannot become key or main. Setting that panel's own first responder is
fixture setup, not a request for system focus. Only getpid() is posted to.
There is no arbitrary PID, window, key or text argument. Both events are
preallocated and validated before adjacent posting attempts. Exceptions preserve
attempt counts and do not trigger corrective input. The source survives the
bounded receiver loop. `verify-macos-key.py` bounds lifetime/output through the
existing raw supervisor and reaps only its own child. Reports are reserved with
mode 0600 and are not overwritten.

Only tagged synthetic events are inspected in the receiver/application queue;
untagged events are counted without retaining their contents. The evidence
checker requires exact source/receiver PID, window, tag, keycode, decoded flags,
character agreement, nonrepeat, pair order, one effect and successful cleanup.
A queue-only result stays failed even if native posting completed.

## Chromium probe

`browser.m` adds a separate `--disposable-chromium-key` opt-in. Existing non-input
and pointer modes cannot dispatch a key. A bounded 48-byte pipe plan freezes
window bounds and tag; it accepts no caller PID or chosen key. Only the retained
Chrome for Testing instance launched with the supervisor's private profile can
be targeted. The target must remain background and retain its exact observed
window geometry. Permission/held-input checks and native construction readback
run before dispatch; the target is checked again after construction.

The first command consumes the attempt, including malformed/refused input.
Duplicate commands preserve the original evidence and cannot post again. Native
exceptions remain failures regardless of whether one or two calls were attempted.
The private source stays owned through browser teardown.

The local synthetic page has a tabindex receiver. CDP opens the page, sets its
internal DOM focus and independently reads its state; it does not supply tested
key input, invoke event handlers or change the counter. Positive acceptance would
require one trusted, unmodified, nonrepeating ArrowRight down/up sequence at the
exact receiver, one counter increment and retained native identity/geometry.
DOM does not expose the native correlation tag: even a future positive result
would not establish authenticated source attribution or independent focus.

## Validation and failed evidence

The preserved initial results include constructor flag mismatch (zero posting),
Caps Lock preflight refusal (zero posting), AppKit queue-only delivery (two
posting calls, zero effects), and Chromium no-DOM delivery (two calls, zero
effects). Changes in desktop samples are reported with undetermined attribution;
equal samples do not prove continuous isolation. Browser/profile/panel cleanup
was verified for completed native attempts.

Pure evidence tests reject queue-only, missing, duplicate, reordered, foreign,
modified, repeating or untrusted events; wrong types/identities; missing cleanup;
and a native success claim without the independent effect. Native nonposting
tests cover the actual constructor, plan parser, geometry and modifier policy.
The repository's keyless Rust battery and existing Python harness regressions
are run separately from these manual GUI probes. Exact published-head reports,
counts and any unresolved review findings are recorded on the draft PR.

## Reproduction

Compile `raw-key.m`, `key-tests.m` and `browser.m` with clang, ARC/ARC exception
cleanup, warnings denied, and the AppKit, ApplicationServices and CoreGraphics
frameworks. All are under `tests/fixtures/macos-monitor/`.

Run `scripts/test-macos-key-evidence.py -v` without any GUI. Run the raw fixture
with `--self-test` for nonposting construction, or use `scripts/verify-macos-key.py`
with explicit `--fixture` and a fresh `--report` path. Its live mode additionally
requires `--allow-disposable-key` and currently fails receiver acceptance.

The browser harness is `scripts/verify-macos-chromium-key.py`, with absolute
`--browser-app` and `--supervisor` paths, a fresh `--report`, and explicit
`--allow-disposable-chromium-key`. It currently fails the keyboard-effect gate.
The existing `verify-macos-chromium-pointer.py` provides a separate positive
mouse regression using the same rebuilt supervisor. No installed daemon is used.

## Primary API references

- Apple Cocoa Event Handling Guide, Handling Key Events:
  https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/EventOverview/HandlingKeyEvents/HandlingKeyEvents.html
- Apple NSEvent keyEvent constructor documentation and local AppKit NSEvent.h.
- Apple CoreGraphics CGEventPostToPid and local CGEvent.h/CGEventTypes.h.

The documented APIs and observed constructor readbacks do not promise inactive
application key delivery. Keyboard promotion remains blocked until actual
receiver effects and focus containment are independently demonstrated.

## Failure cleanup

The new Chromium harness sends EOF to its native cleanup owner and waits for
normal shutdown. A timeout is failure, not browser-exit evidence. It does not
kill the only owner of a separately launched browser while that browser's exit
is unconfirmed: it records the retained supervisor and keeps the private profile.
The native supervisor still owns the exact NSRunningApplication and its existing
bounded teardown path. A hung native owner is an unresolved cleanup state, not a
claimed successful cleanup. Forced reaping is permitted only after browser exit
is independently confirmed. No caller-reported browser PID is signalled.

Immutable key dispatch attempts, native errors and replay counts survive in final
status. Cleanup preserves that record even when the browser exits before normal
acceptance reads it. Contradictory final evidence fails without overwriting the
original. Hermetic tests cover this distinction and owner retention.

## Local validation snapshot

The unchanged production Rust tree passed 6,614 binary tests (10 existing ignores),
1,022 required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace
Clippy with warnings denied, formatting and whitespace. Sixteen new Python
evidence/cleanup tests and all 61 existing input/scroll/harness tests passed.
Native nonposting checks passed seven basic construction cases, twenty
plan/modifier/constructor checks, and six pipe cases. These passes are not
positive native keyboard acceptance. The existing standalone Chromium click
regression passed using the rebuilt supervisor, with complete cleanup.
