# Bounded owned-window scrolling (2026-09-20)

Production source following merged #943. Native HTTP acceptance on 2026-09-20
verified both vertical directions on an owned monitor and the existing click
regression. These are disposable Chromium observations, not general application
compatibility or continuous-isolation guarantees. No running daemon was upgraded.
Published-head evidence and queue status are recorded separately on PR #944.

## Public surface and authority

`prepare_macos_window_scroll {binding,point:{x,y},delta_y}` requires OwnerSurface
and DisplayView. `delta_y` is a signed integer in logical pixel scroll request
units, positive down, nonzero with absolute value at most 600. It is a request,
not a promise of a matching document displacement. The point is window-local
logical points with a top-left origin and strictly excluded right/bottom edges.
No coordinate normalization, clamping or implicit scale is applied.

`scroll_macos_window {binding,token}` requires OwnerSurface and DisplayInput.
It accepts no replacement point, delta, PID, window or display ID. The facade
routes are `inspect display prepare-scroll BINDING POINT_JSON DELTA_Y` and
`act display scroll-window BINDING TOKEN`; DELTA_Y is a JSON integer, including
negative values. HTTP definitions derive from the same typed stdio declarations.
Both transport classifications and the owner-only helper broker apply; ordinary
scoped user-display grants do not authorize either operation. Unsupported OSes
refuse, and no scroll operation starts a previously unused broker/helper.

The generic `macos_virtual` lane still advertises `input_supported:false`.
Reserved handle parsing, capture, display grants and generic CU routing are
unchanged. Preparation is not an authority grant. Ingress authorization and the
owner check at admission, dequeue and private-helper dispatch reuse #943's
semantics, not an atomic revocation fence once native dispatch begins.

## Shared transaction engine

Scroll reuses the pointer engine's single pending preparation, ten-second monotonic
expiry, exact retained AX window/process-start identity and owned monitor geometry,
whole-window/point containment, permission checks and exact observed focus. The
validated application-local focus fallback is unchanged. A private plan records
click versus scroll and, for scroll, the validated delta. Cross-kind dispatch takes
and refuses the token; refresh replaces either kind, even when helper preparation
fails. Click wire fields and its two adjacent posting attempts remain compatible.

Every attempt that reaches the helper consumes the preparation before token/kind
validation. Ingress/broker refusals cannot mutate helper inventory. Placement,
unbind, monitor teardown and semantic snapshot/actions retain the existing
invalidation rules. Pointer prepare/dispatch clear semantic tokens. Identity,
geometry and focus are checked before construction and again before posting;
geometry changes of any size refuse. Content navigation inside an unchanged window
is not frozen, and positional scroll has no semantic protected-field exclusion.

Click and scroll share the eight-object source retention limit and minimum
four-second hold, pruned on a later pointer operation or released at helper EOF.
Capacity pressure refuses before input. The hold is not a delivery acknowledgement.
Cancellation before helper dispatch prevents effects. Once dispatch may have begun,
a lost reply is uncertain; the serialized actor keeps ownership and never retries,
rolls back, activates a target or sends corrective input.

## Native construction

All mutating FFI stays in `crates/intendant-platform/src/bound_pointer.rs` and
`bound_pointer.m`, under the narrow CLAUDE.md/AGENTS.md scroll exception. Rust owns
a main-thread-only, !Send/!Sync native scroll object. It preallocates one event and
retains its private source through the shared retention lifecycle.

An addressed NSEvent seed is copied and converted to `kCGEventScrollWheel`.
Documented wheel delta fields are copied from
`CGEventCreateScrollWheelEvent(source,kCGScrollEventUnitPixel,1,-delta_y)`.
The template's position is never used to address input: actual global coordinates
and runtime-probed `CGEventSetWindowLocation`/`CGEventGetWindowLocation` are set and
read back independently using #942's window-local convention. No numeric private
field guesses or global-post fallback are introduced.

Modifiers, horizontal/third-axis deltas, scroll phase and momentum phase are
explicitly zero. Readbacks require actual NSEvent windowNumber/type/phase/deltas,
CG private source state and source/target PIDs, tag, wheel fields,
pixel delta, continuous-pixel flag and both coordinate spaces to agree. The source
state must match the actual private source and differ from HID/combined state;
mouse-only under-pointer window hints are not assumed to exist on wheel events. Missing
SPI, allocation failure, contradictory readbacks or exceptions refuse construction.

The one posting count is incremented before `CGEventPostToPid`. The native object
is consumed once, including a readiness refusal; the Rust owner independently
refuses replay. Exceptions remain failures even with one recorded attempt.
Cleanup only releases objects. There is no horizontal, keyboard, drag, activation,
momentum sequence, global posting, corrective input or automatic retry.

## Receipts and regression coverage

`prepared` contains the frozen point/global coordinates, window observations,
delta, token and expiry. `action` carries delta, point/global coordinates, before
and available after observations, focus interference, detail and posting counts.
`dispatched` requires exactly one attempt, no native error and successful state
postchecks; it never establishes delivery or application effects.
`effect_verified` is always false, and possible input sets `effects_unconfirmed`.
Native failures and post-readiness failures preserve independent geometry/focus
observations. Late liveness/receipt failures preserve available evidence; invalid
helper success/counts retire the broker with uncertainty rather than replaying.

New hermetic Rust cases cover integer/type/overflow and point bounds, kind mismatch,
expiry/refresh/replay and semantic invalidation, owner/IAM gates across typed,
HTTP and facade calls, stale geometry/focus/identity, post-readiness observations,
source capacity, cancellation, late evidence and lying helper replies.
`tests/fixtures/macos-monitor/bound-scroll-tests.m` invokes the actual C posting
loop with a synthetic nonposting callback, including exception and no-replay cases.
The existing `bound-pointer-tests.m` click-loop regression is unchanged.
The local tests and native acceptance passed as recorded below. No independent
input/focus isolation is claimed.

## Validation and native acceptance

The full local battery passed: 6,614 binary tests (10 existing ignores), 1,022
required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace
Clippy with warnings denied, formatting and whitespace. The 61 Python checks
passed. Native nonposting coverage passed 17 production-constructor cases, seven
scroll exception/replay cases and nine unchanged click cases. Independent
read-only source review found no concrete issues.

Actual ctl -> HTTP -> broker -> retained-window helper scrolling passed on an
owned 800x600 monitor. A +120 request produced one trusted wheel and independently
observed pane offset 700 -> 820. A separate fresh -120 fixture produced 700 -> 580.
Both required exact client/global coordinates, zero horizontal delta/modifiers,
unchanged identity/geometry and passing focus checks. The supervisor was in its
non-input mode; CDP only set up and observed the synthetic local page.

Refresh/consumed-token, bounds, zero/oversized delta, both cross-kind directions,
replay, placement invalidation and unbind negatives caused no additional input.
The existing HTTP click regression passed separately. Each run cleaned up its
browser/profile and supervisor and restored the original display inventory.
DOM cannot expose the native correlation tag; matching effects are not
authenticated source attribution. Pointer sample changes remain unattributed.

Failed runs remain separate: the first stopped at focus observation during
placement; a later upward test refused a changed preparation with zero posting
calls and no pane movement. Both cleaned up. No guard was weakened and no
uncertain input was replayed; subsequent tests used newly created fixtures.

Nonposting constructor checks caught two errors: the private source creation
selector is not its unique table ID; mouse-only under-pointer window hints are
unavailable on wheel events. The correction retains actual source identity and
NSEvent destination window validation, plus all coordinate/process/delta/phase
checks. Both signs, minimal/extreme deltas and malformed arguments are covered.

## Reproduction

Build the worktree controller normally with the compile governor intact. Build
`browser.m` and `pattern.m` using clang with ARC and AppKit/CoreGraphics/
ApplicationServices frameworks. Give the HTTP harness absolute binary, fixture
and bundle paths because it creates a temporary project directory.

Run `scripts/test-macos-scroll-evidence.py -v` for the pure evidence tests. Run
`scripts/verify-macos-monitor-http.py` with the existing explicit browser/fixture
paths, `--chromium-scroll-delta=120 --allow-shared-session-monitor --check-recovery`
and a fresh `--report` path. A separate invocation with `--chromium-scroll-delta=-120`
uses another fresh profile. Replace the scroll option with `--chromium-bound-pointer`
for the existing click regression. These opt-in tests create/remove shared-session
virtual displays; they do not upgrade the running plugin daemon.
