# Owner-bound window pointer transactions (2026-09-19)

This slice follows merged #942 (`0024a80`). It promotes the tested asymmetric
window-addressed pointer path into the existing daemon-owned monitor helper,
not into the generic display input route. Only one left-click pair is supported.
No keyboard, drag, scrolling, activation, clipboard operation, global post,
user-display fallback, automatic retry or focus restoration is introduced.

## Public surface and authority

`prepare_macos_window_click {binding,point:{x,y}}` (facade
`inspect display prepare-click BINDING POINT_JSON`) requires DisplayView and an
OwnerSurface. `click_macos_window {binding,token}` (facade
`act display click-window BINDING TOKEN`) requires DisplayInput and OwnerSurface.
HTTP schemas derive from the typed stdio declarations. Scoped callers are refused
even if they have an ordinary user-display grant. A preparation is not an IAM grant.
Each HTTP call passes the existing ingress authorization gate; the broker checks
owner posture again when admitted, dequeued, and before private helper dispatch.
These are the existing per-request authorization semantics, not a new atomic
revocation fence after native dispatch has started.

The binding must come from explicit candidate selection and `bind_macos_window`.
It names a retained exact AX window and process-start identity, and an exact owned
monitor generation. The whole current AX and CG window rectangles must fit that
monitor, which must retain its bound geometry. AX/CG observations agree within
the existing one-point observation tolerance; containment has no tolerance.
The requested point is finite, nonnegative, window-local logical points with a
top-left origin. Right and bottom edges are excluded. Nothing is clamped or
converted from normalized coordinates. The same global point must be inside
both AX and CG windows and the monitor. No PID, window ID or coordinate override
is accepted by the dispatch tool.

## Preparation and one-shot state

One preparation exists across the helper, replacing the previous one even on a
failed refresh. Its random `macos_pointer:` token freezes the binding, point,
current AX/CG rectangles and observed focus for ten monotonic seconds. A click
that reaches the helper takes that preparation before validating its token:
stale/foreign/expired attempts cannot preserve a newer usable preparation.
Ingress/broker rejection that does not reach the helper cannot mutate its inventory.
Placement, unbind, monitor teardown, semantic inventory refresh and semantic
actions invalidate the relevant preparation (semantic operations invalidate all).
Pointer preparation/actions also clear semantic tokens.

A click rechecks exact process/window identity, monitor geometry, containment,
existing OS permissions and observed focus. Both events are constructed without
posting, then those checks run again. Window geometry changes of any size between
preparation and dispatch refuse rather than adjusting the coordinates. Positional
input is not document identity: content or browser navigation inside an unchanged
window is not frozen by this token. It does not inherit the semantic-control
path's protected-field exclusion or claim a sandbox against app-supplied content.

## Native construction and lifetime

The native ARC/exception shim lives in `intendant-platform`, alongside the existing
CGVirtualDisplay shim; Rust ownership/FFI is confined to the dedicated `bound_pointer.rs` native-input
island, with a compatibility-only re-export from `platform.rs`. The narrowly
scoped exception is explicit in CLAUDE.md/AGENTS.md; other native-input mutations
remain outside this exception. The helper
invokes it only on the main thread. Native pairs are not Send/Sync.

Public NSEvent construction carries the destination window. The pair uses a
private source state with explicit zero modifier flags/deltas and exact button,
click-count, pressure, source/destination PID and window metadata. Runtime-probed
`CGEventSetWindowLocation`/`CGEventGetWindowLocation` set and verify top-left
window-local coordinates, following the tested #942 convention. Missing symbols,
failed allocation, contradictory readback or Objective-C exceptions fail closed.
No new numeric undocumented event field is guessed. The target must be background;
existing Accessibility/Screen Recording/PostEvent permissions are checked without
prompting, and a held human mouse button refuses native posting.

After all checks the two preallocated events are posted adjacently: there is no
allocation, await, cancellation check or retargeting between them. Posting-call
counts are recorded before calls, and exceptions preserve a partial count. The
pair cannot post twice. A source/events object remains retained for at least four
seconds and until a subsequent pointer operation prunes it, or helper EOF drops
it. Retention is bounded to eight objects; capacity pressure refuses before input.
Four seconds is not a delivery acknowledgement or a guarantee of OS queue latency.

## Cancellation and honest evidence

The existing broker publication-before-final-cancellation-check rule also covers
pointer dispatch. A queued request cancelled before helper dispatch is known to
have no native effect. Once dispatch may have started, a lost/expired response is
uncertain; it must not be replayed. Cancellation does not interrupt a mouse pair,
roll back application effects, restore focus, or send corrective input elsewhere.
The actor continues owning the helper and cleanup after its requesting future ends.

`dispatched` means exactly two posting attempts with successful state postchecks,
not delivered input or verified application effects. `partial` preserves zero,
one or two attempted calls, before/available-after geometry and focus observations.
`effect_verified` is always false; `effects_unconfirmed` tracks possible input.
Late liveness/receipt failures preserve available native evidence rather than
inventing either success or zero effects. Unexpected/malformed helper replies
retire the broker and return uncertainty after possible dispatch.

This is a shared WindowServer, not an independent input/focus/clipboard seat.
There is a residual race between observations and OS event handling. Whole-run
pointer/foreground differences remain unattributed; equal samples do not prove
continuous isolation. No user activity is used to excuse failed action-level checks.

## Native HTTP acceptance

The opt-in `verify-macos-bound-pointer.py` harness runs under a separate temporary-
HOME daemon and uses only a new Chrome for Testing profile and local synthetic page.
Its supervisor is launched in its non-pointer mode. All tested preparations and
clicks travel through ctl -> HTTP MCP -> broker -> retained-window helper. CDP only
sets up the fixture and observes its own DOM; it never injects tested input.

The first native run passed: an 800x600 owned monitor, a 720x530 browser window
placed at global (-770,30), and an asymmetric window-local click. The dispatcher
reported two calls/effect-unverified. Independently, the browser exposed the exact
trusted down/up/click sequence at matching client/global coordinates and one
canvas-counter increment. Refresh/consumed-token, bounds, replay, placement and
unbind negatives caused no additional counter change. The browser/profile were
cleaned up; the outer exact capture/recovery test restored the original monitor
inventory. DOM cannot expose the native correlation tag; this is independent
effect evidence, not authenticated source attribution.

```sh
clang -fobjc-arc -Wall -Wextra -Werror -framework AppKit -framework CoreGraphics \
  -framework ApplicationServices tests/fixtures/macos-monitor/browser.m -o /tmp/browser-supervisor
clang -fobjc-arc -Wall -Wextra -Werror -framework AppKit -framework CoreGraphics \
  -framework ApplicationServices tests/fixtures/macos-monitor/pattern.m -o /tmp/monitor-pattern
python3 scripts/test-macos-bound-pointer.py -v
python3 scripts/verify-macos-monitor-http.py --bin /absolute/worktree/target/debug/intendant \
  --fixture /tmp/monitor-pattern --chromium-app '/absolute/Google Chrome for Testing.app' \
  --chromium-supervisor /tmp/browser-supervisor --chromium-bound-pointer \
  --allow-shared-session-monitor --check-recovery --report /tmp/bound-pointer.json
```

The generic `macos_virtual` display lane remains read-only and continues reporting
`input_supported:false`; only these explicitly bound-window tools gain one paired
left click. Streaming, generic CU action batches, arbitrary browser compatibility,
keyboard/chords and an independent seat remain outside this slice. The running
plugin daemon is not upgraded by these source changes or acceptance tests.


## Independent review corrections

The read-only review identified two evidence-loss defects before publication.
Native posting now carries an independent failure flag: an exception on the
second attempted call cannot become a successful `dispatched` result merely
because its attempt count is two. A native test invokes the production posting
loop with a nonposting exception callback, including first/second-call failures
and refused replay. It creates no application and sends no OS input.

Post-action observation now gathers window geometry and focus independently of
pointer readiness. Foreground transitions, changed geometry or unavailable
readiness preserve available observations and produce partial results. Hermetic
regressions cover changed/unchanged focus and geometry with failed post-readiness.


## Final local validation

The full post-review battery passed: 6,587 binary tests (10 existing ignores),
1,022 required library/acceptance tests (3 existing ignores), 57 end-to-end tests,
workspace Clippy with warnings denied, formatting and whitespace. The 45 Python
harness/evidence checks and 9 nonposting native exception/replay cases passed.
The focused read-only follow-up review confirmed both corrections and found no
new concrete regressions in them. Final published-head native evidence is recorded
on the PR alongside the separately retained earlier runs.


## Hosted-review boundary correction and final-head gate

Hosted review required a documented unsafe island rather than extending generic
platform probes/signals to mutating pointer input. The entire Rust wrapper moved
verbatim to `crates/intendant-platform/src/bound_pointer.rs` in a pure-move commit;
`platform.rs` preserves its public path by re-export. A separate documentation
commit defines only this main-thread, non-Send/Sync, exact-pair FFI exception and
keeps CLAUDE.md/AGENTS.md identical. There is no dispatch behavior change.

The first two native attempts on published `a3d0980` stopped before click dispatch
because the existing system-wide focused-element observation was unavailable.
Both cleaned up the browser/profile and restored the original display inventory.
A three-read metadata-only diagnostic returned AX -25204 (CannotComplete) with no
focused element while Accessibility, Screen Recording and PostEvent preflights
passed. It read no labels, values or documents and made no input/activation calls.
This is an observation failure, not proof that the owner caused it; it is not
converted into a safe focus state. These failed runs remain separate from the
successful development-tree acceptance. Published-head acceptance is still a
landing gate unless a later independently recorded fresh run succeeds.


## Resumed focus-routing correction

The interrupted final pass left a macOS-only Clippy module-inception error after
its pure FFI move. The private file-module alias is now `bound_pointer_ffi`;
`platform::bound_pointer` and the native posting behavior are unchanged. The
complete local battery passed after that correction.

A bounded metadata-only diagnostic then distinguished a system-wide AX routing
failure from unavailable application focus: both system-wide focused-application
and focused-element queries returned CannotComplete, while the foreground
application returned a real focused AX element directly. No labels, field values,
documents, keyboard events or clipboard contents were read.

Bound-window focus observation retains the system-wide path first. Only
CannotComplete **without** a returned value admits an application-local read.
Other errors, successful nulls, malformed types and contradictory Other errors, suche alternative requires the live foreground process start Other errors, successful n AXFrontmost property on its retained application object at three checkpoints.
It reads the focused AX element twice, checks its process both times and requires
exact object equality. A stale NSWorksexact object equality. A stale NSWorksexact object equality. A stale replaced focus, missing metadata and deadline expiry
all refuse. The same exact element/PID/start identity is stored in preparations
and compared around each action, regardless of which observation route succeeded.

The four-second operation budget and existing 50-millisecond per-attribute timeout
remain unchanged. No event tap, activation, focus restoration, empty-focus sentinel
or input fallback was added. The scalar foreground query is read-only and lives in the permitted
platform-query island (`platform.rs` with the ARC/exception-only `foreground.m`
shim), not the narrower pointer-input island. Like all AX-based observation,
this remains app/OS-reported state with observation-to-dispatch races, not an
independent focus seat or an atomic isolation guarantee.


### Resumed validation

The complete corrected local battery passed: 6,594 binary tests (10 existing
ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E
tests, workspace Clippy with warnings denied, formatting and whitespace. All
45 Python checks and 9 nonposting native exception/replay cases passed.
The read-only follow-up review found no concrete regressions after the foreground
query moved into the permitted platform-query island.

The fresh development-tree HTTP/owned-monitor Chromium test passed, including
one independently observed canvas click, exact event coordinates, stale-token
and replay refusals, browser/profile cleanup and original display restoration.
A prior invocation used a relative supervisor path under the temporary project
and failed before browser launch; it remains recorded separately. Published-head
acceptance and landing status are recorded on the PR, never inferred from CI.
