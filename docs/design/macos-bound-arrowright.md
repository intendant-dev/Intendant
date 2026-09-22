# One-use retained-receiver ArrowRight (2026-09-22)

Production follow-up on merged #947, base dc13ce0d. This slice supports only
one fixed, unmodified ArrowRight down/up pair on an enabled nonprotected
AXTextField or AXTextArea inside an exact bound window on an owned monitor.
It does not provide arbitrary text, chords, other keys, activation, hidden
clicks, global posting, focus restoration or an independent input seat.

## Public surface and authority

`prepare_macos_window_arrowright {binding}` requires DisplayView and OwnerSurface.
`press_macos_window_arrowright {binding,token}` requires DisplayInput and OwnerSurface.
The facade spellings are `inspect display prepare-arrowright BINDING` and
`act display arrowright BINDING TOKEN`. Typed stdio and HTTP schemas derive
from the same declarations. Extra keys/text/modifiers/PIDs/coordinates are
rejected. Unsupported hosts and unused/retired monitor brokers refuse without
starting a helper or falling back to the user's real display.

A preparation is not an IAM grant. Existing per-request ingress authorization
and the broker's owner checks at admission, dequeue, dispatch and delivery apply.
This is not a new atomic revocation fence after native dispatch begins. The
generic macos_virtual lane still reports input_supported:false. The existing
read_macos_window_keyboard_target report remains read-only, non-retaining, and
continues to report keyboard_dispatch_supported:false: that observation alone
is neither eligibility nor a retained preparation for this separate operation.

## Retention, selection and invalidation

Preparation retains the exact target-application AXFocusedUIElement and its
original bounded ancestor path from #947, including the supported Chromium
projection. It freezes receiver role/enabled state/geometry, process start
identity, retained AX window/CGWindowID, owned-monitor generation and geometry,
AX/CG whole-window bounds and exact human global-focus witness. It never reads
labels, field values or document text. Protected-content checks apply to the
receiver and its ancestors before metadata/traversal.

One pending key preparation exists per helper. Its random macos_key token has
ten monotonic seconds of validity. Expiry is checked, not a background timer;
idle retained references live until the next invalidation or helper teardown.
Every key attempt reaching the helper consumes its preparation before token or
binding validation. Frontend/broker refusals cannot mutate helper state.
Preparation and dispatch clear semantic and pointer preparations; pointer or
semantic preparation/actions invalidate the key. Placement, unbinding and
monitor destruction invalidate affected keys. Read-only receiver inspection
does not consume any preparation.

Dispatch revalidates the original receiver and original ancestors, rather than
discovering a replacement with identical metadata. It uses the same four-second
operation budget and AX timeout as inspection, first before native construction
and again afterward. Changed receiver, protection, role, enablement, geometry,
window/process identity, monitor or human-focus witness refuse before posting.
A field's text, selection and document content are NOT frozen. An unchanged
receiver may handle ArrowRight differently after content changes.

## Native boundary and outcome semantics

The documented bound_arrow.rs/bound_arrow.m island owns typed, main-thread-only
FFI and an ARC/exception boundary. Rust pairs are not Send/Sync. An addressed
NSEvent seed carries the exact window; copied private-source CGEvents have
fixed keycode 124/NSRightArrowFunctionKey, no autorepeat or shortcut modifiers,
and checked source/target PID, tag, type, Unicode and window readback. AppKit's
function-key classification is checked separately from zero Quartz modifiers.
No new private key-posting API or undocumented numeric event field is used.

Readiness checks existing Accessibility/PostEvent permission without prompting,
a background target and no held human keys/buttons/shortcut modifiers. Caps
Lock's latched nontext state is not treated as a held shortcut. Both events are
preallocated; posting attempts are adjacent, with no allocation, await,
cancellation, retargeting or fallback between them. First/second-call exceptions
retain an independent failure flag even when two calls were attempted.

Pairs share the pointer engine's eight-object source pool and minimum four-
second hold, pruned by later operations or dropped at helper EOF. This hold is
not a delivery acknowledgment or a guarantee about OS queue latency.

`dispatched` means two native posting attempts and successful postchecks, not
verified delivery. `effect_verified` is always false; possible input sets
`effects_unconfirmed`. Partial results retain counts and independently available
window/focus observations even when receiver or readiness postchecks fail. A
receiver that cannot be reverified is unknown, not silently replaced.
Cancellation before helper dispatch is known no-effect; lost replies after
possible dispatch are uncertain. The serialized actor retains cleanup ownership.
No retry, rollback or corrective key-up is synthesized after uncertainty.

This shares WindowServer. AX/CG/native observations are not atomic locks on OS
event routing, and races remain between the final observation and OS handling.
A successful fixture is not an arbitrary-app, arbitrary-site or isolation claim.

## Acceptance and reproduction

The opt-in profile extends the existing disposable Chromium receiver fixture.
It requires ALL of --chromium-keyboard-target,
--chromium-keyboard-target-click-first and --chromium-arrowright. The selection
click is separately requested through existing owner-bound click tools, with
independent DOM effect verification. The keyboard API NEVER supplies that click.
The fixture uses its own nonsecret fields; CDP only prepares/observes fixture
state and closes its verified private browser, never injects tested input.

The test requires the exact trusted ArrowRight down/up sequence and an actual
caret move from 2 to 3 with unchanged nonsecret field value. It also exercises
refresh, consumed tokens, focused receiver changes, same-geometry replacement,
protection changes and replay/stale-binding refusals. Dispatch, DOM effects and
cleanup are separate evidence. DOM has no native correlation-tag provenance.

Build the controller and runtime in this worktree with the installed compiler
governor intact. Build browser.m and pattern.m from tests/fixtures/macos-monitor
with clang ARC, warnings denied and AppKit/CoreGraphics/ApplicationServices. Use
absolute paths with verify-macos-monitor-http.py and a Python with Pillow, as
the outer fixture checks exact pattern capture. The system Python used by the
prior receiver tests has Pillow; the shell's newer Python did not. That first
invocation failed before any daemon/browser/display launch and is retained.

Final exact-head native results and local/CI status are recorded separately on
PR #948. No running plugin daemon is upgraded by building or running this
isolated temporary-HOME acceptance profile.
