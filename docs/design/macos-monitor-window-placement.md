# Owned monitor window binding and placement

This branch implements explicit owner-only window binding and placement on top
of the owned monitor helper. The native disposable-window acceptance passed on
2026-09-16 (macOS 26.4.1, arm64). This is not a general application compatibility
claim; the exact native checks and reproduction are recorded below.

## Public contract

| Typed MCP operation | Facade command | IAM |
| --- | --- | --- |
| `list_macos_windows {pid}` | `inspect display windows PID` | `DisplayView` |
| `bind_macos_window {display_target, candidate, identity}` | `act display bind-window MONITOR CANDIDATE IDENTITY_JSON` | `DisplayInput` |
| `place_macos_window {binding, bounds}` | `act display place-window BINDING BOUNDS_JSON` | `DisplayInput` |
| `unbind_macos_window {binding}` | `act display unbind-window BINDING` | `DisplayInput` |

Every operation also requires gate-resolved `ToolCallerTrust::OwnerSurface`,
checked at ingress, actor dequeue, before dispatch and result delivery. A scoped
caller with a user-display grant still cannot enumerate, bind or move app
windows. HTTP and facade dispatch preserve the actual caller; generated stdio
entry points keep their existing owner-surface trust model. Existing monitor
create/destroy/capture authority and Linux display behavior are unchanged.
Tool descriptions and schemas derive from the typed declarations; public
identity and geometry types also drive the private protocol.

Listing requires an explicitly supplied positive application PID and an
already-started owned-monitor helper. It returns at most 16 candidates containing
`candidate` (an opaque token), `identity: {pid, start_seconds, start_micros, window_id}`
and global logical-point `bounds`. The helper retains the exact listed AX object
for each token in one replaceable inventory of at most 16 entries. Every listing
that reaches the helper refreshes this inventory across all PIDs and callers,
including a failed refresh; old unbound tokens become stale. Rejected ingress or
undispatched requests do not refresh it. Listing never invalidates existing
bindings. A bind consumes its token when the helper selects the object after
identity, capacity and initial monitor checks; later failure requires relisting.
Helper exit releases the inventory. No ID-only re-resolution can create a
binding. Listing reads no window titles or document contents. The process start pair
comes from `proc_pidinfo(PROC_PIDTBSDINFO)`. Missing process identity, missing
Accessibility or Screen Recording permissions, missing mapping SPI, duplicate
window mappings, excessive enumeration, unavailable/hidden windows and AX/CG
geometry disagreement refuse; permissions are preflighted, never requested.

Binding requires that candidate token and its matching complete identity plus the
**exact** owned monitor selector returned by `create_virtual_display`/`list_macos_monitors`. No native display ID,
PID-only selector, title matching, enumeration-order choice, numeric monitor
alias, primary display or fallback is admitted. Binding returns
`bound_window: {binding, display_target, identity}`. Only the helper retains the
AX object, transferred from the inventory into the binding. Public
`macos_window:<broker-owner>:<generation>` bindings never expose private helper
handles. A window cannot be bound twice without an explicit unbind. Retain the returned binding for cleanup.

Placement accepts `bounds: {x,y,width,height}` in **monitor-local logical points**,
with top-left origin. Finite positive sizes and the **entire requested rectangle**
must fit the bound monitor, with no containment tolerance. Negative global
monitor origins work; negative local origins refuse. Coordinates are bounded to
±1,000,000 and dimensions to 16,384, with the stricter live monitor size limiting
actual targets. The primitive's retained monitor object supplies live global
bounds; dimensions must still match its owned 1x creation mode. Mirrored, primary,
stale, offline or changed monitor geometry refuses. Geometry is frozen at bind;
layout changes require explicit rebind.

The helper checks the process start generation, PID, `_AXUIElementGetWindow`
mapping, a unique current AX match, and `CFEqual` against the **retained exact AX
window**. Thus same-process reuse of a CGWindowID cannot substitute a fresh AX
object. Minimized/fullscreen windows, unknown state or non-settable AX geometry
refuse. The implementation consulted PR925 source at `17e316c` for the generation
and exact mapping ABI shape; its input implementation is not imported.

Position and size are attempted once each, in that order. Identity, live monitor
geometry, current focus and AX/CG geometry are rechecked around **each** write.
Geometry changes between checks stop the next write. The helper never activates,
raises, unminimizes or refocuses an app; never injects keyboard/mouse input; and
never touches the clipboard. Other actors can still act between observations;
this is not an atomic WindowServer transaction or an isolated login session.
Intermediate geometry can differ from the requested final rectangle.

`ok:true` / `placement.status:"verified"` requires both AX and CG readback to
match the requested global x/y/width/height to **1 logical point per component**,
and both full readback rectangles to lie inside the owned monitor with **zero**
containment tolerance. It also requires unchanged observed focus and live
identity/monitor checks after writes. Dispatch success alone is insufficient.
After both single-attempt setters, readback may settle for at most 250 ms / 20 polls within the existing four-second operation budget. This repeats observation only, never writes; every poll rechecks retained identity, monitor geometry and focus. Unsettled or changed targets still return partial.

Before any write path, failures return `ok:false,error`. Once a write path has
been attempted, failures return `ok:false,placement.status:"partial"`, with
requested global bounds, before/last available after observations,
`writes_attempted`, `focus_interference` and detail. A setter may act before
returning an error. Attempts conservatively include entering a setter path that
subsequently refuses. `focus_interference:true` means a changed focused AX element
or process was observed; `false` means the final focus check matched; `null` means
focus could not be reobserved. No failure triggers rollback or focus restoration.
Second-write refusals retain the newest geometry observation even if AX and CG
disagree. Post-dispatch helper/deadline/delivery failures explicitly report
`effects_unconfirmed:true`; movement/resize may already have applied or may still
be in progress. Available placement results survive final liveness failure or
receipt expiry, including `status:"verified"` observations, but the outer response
is `ok:false`. A helper exchange failure retires the broker; a frontend receipt
wait timeout only closes delivery while the worker retains ownership and finishes
the bounded attempt. Buffered results at that deadline are drained and preserved.

## Ownership and bounds

The **same** private main-thread helper owns monitors and windows; no second
native worker or unsafe AX `Send`/`Sync` exists. Bind/place/unbind serialize with
monitor destruction. There are at most 16 retained bindings plus one inventory
of 16 retained candidates, 16 AX window roots per PID (request cap + 1 to detect overflow), two monitors, eight queued broker
requests, and 16 KiB per private JSON line. AX messaging uses 50 ms per-object
IPC limits and a four-second operation budget checked between native calls;
there is no unbounded retry. The existing pipe exchange deadline is six seconds,
frontend receipt wait is twenty seconds, and receipt commit is limited to two
seconds. Non-interruptible OS calls can still exceed cooperative budgets; the
parent owns exact-child termination and terminal broker retirement.

A cancelled queued request does nothing. Cancellation during a dispatched
placement lets that bounded serialized attempt finish verification; it does not
undo writes. An undelivered/uncommitted bind is explicitly unbound before the next
operation. Receipt expiry closes the commit receiver before draining any buffered
commit: a successful send before closure wins and cannot race rollback; closure
before send refuses publication. Deterministic hermetic tests cover both orders.
Commit means local response construction, not network delivery: transport loss after commit
can leave a binding whose handle the client did not receive. Retention remains
capped; destroying its monitor or helper EOF releases it.

Unbind, monitor destruction, malformed protocol, output failure and helper EOF
release references without moving or closing user windows. Cleanup invalidates
public bindings before monitor teardown. Child close waits five seconds for EOF
cleanup, then terminates only its retained child and gives reaping two seconds;
uncertainty is reported, the broker stays retired, and kill-on-drop/Tokio reaping
retain responsibility. No path adopts a PID or abandons monitor ownership to
start another helper. Linux/Windows window operations return a clear unsupported
error. Ordinary startup and generic CU/input/readiness paths gain no placement.

## Supervisor validation still required

Static checks completed for this slice: rustfmt parse/format checks on every
changed Rust file, Python AST parsing of the unexecuted harness, added-code
inspection for forbidden mutation APIs, and `git diff --check`. These checks do
not type-check Rust/Objective-C or execute regressions.

No Cargo command or native/GUI call was run for this slice. Keep the existing
compile governor environment intact. Suggested serialized checks in this
worktree:

```sh
cargo check -p intendant --bin intendant
cargo test -p intendant --bin intendant macos_monitor
cargo test -p intendant --bin intendant window_
cargo test -p intendant --bins
cargo test -p intendant-core -p intendant-custody -p intendant-display -p intendant-platform -p owner-plane-core -p owner-plane-reducer
cargo test -p intendant --test e2e
cargo clippy --workspace -- -D warnings
```

macOS compilation must type-check AX, libc and the Objective-C bridge; Linux and
Windows checks must validate cfg guards and existing paths. Added inline fakes
cover permissions, missing SPI/ambiguity, owner and HTTP IAM gates, exact schema
and facade routing, negative origins/bounds, stale monitors/windows/PIDs,
same-PID replacement before and after bind, inventory refresh/consumption,
latest second-write observations, late-result preservation, cancellation before/after dispatch, dropped/expired bind
receipts, capacity/retention, helper EOF/output failure, and verified/partial
readback/focus outcomes. These regressions passed in the supervisor's focused and full binary validation.

Native acceptance is opt-in and needs existing AX/Screen Recording permissions,
a supervisor-built controller already running on an explicit owner port, and an
existing disposable owned monitor of at least 640x480. The harness starts no
daemon, builds nothing, changes no permissions/grants and never destroys the
supplied monitor. It operates only on its own panel, initially ordered **below**
other windows without activation/key/main/raise calls. The fixture exits on EOF
or after 90 seconds; the harness caps waits, output and its overall exercise.

```sh
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/placement.m -o /tmp/intendant-placement-fixture
python3 scripts/verify-macos-window-placement.py \
  --bin /absolute/worktree/target/debug/intendant \
  --fixture /tmp/intendant-placement-fixture --port SUPERVISOR_PORT \
  --monitor 'macos_virtual:EXACT_OWNER:EXACT_GENERATION' \
  --report /tmp/window-placement-acceptance.json \
  --allow-fixture-window-placement
```

Run from an owner shell outside supervised-agent credentials. The harness calls
typed MCP through existing `ctl tools call` authentication, lists only its own
PID, checks refreshed/consumed tokens and a listed window replaced before binding,
binds the exact fixture candidate plus identity, checks out-of-bounds refusal without
movement, verifies independent fixture CG readback, releases/rebinds, replaces
its own window and checks stale-window refusal. Failure/partial results are
recorded honestly. Same-ID reuse, permission revocation and cancellation remain
hermetic cases; do not manipulate the owner's TCC settings to test them. A
separate supervisor manual focus-interference trial should record a refusal or
partial outcome and confirm that focus was never restored. Record actual native
results before asserting compatibility; there is no native acceptance claim yet.

Window binding and candidate tokens are reserved against generic display parsing, including malformed/case/whitespace variants; they cannot become legacy :99 or primary-display fallbacks.

## Observed native acceptance and reproduction

The isolated temporary-HOME HTTP daemon passed monitor creation/recovery/status,
window candidate refresh invalidation, listed-then-replaced rejection, explicit
binding, strict off-monitor refusal without movement, verified placement,
independent fixture CG readback, consumed-token rejection, replaced-bound-window
rejection, unbinding, exact screenshots, monitor destruction and parent-EOF
cleanup. The original display inventory was restored and the fixture was reaped.
Neither the installed daemon nor user documents were used.

The requested local rectangle (80,80,400,300) translated to
global (-720,80,400,300); final AX, CG and independent fixture observations agreed.
Exactly two setters were attempted, with no focus change observed. These are
bounded observations, not proof of an independent input seat or continuous focus
isolation.

Two native iterations exposed distinct issues: fixture launch animation changed
its geometry without any placement setter, so the disposable fixture now disables
that animation; AX could report a completed resize before CG published it, so the
engine now performs bounded readback-only settling without replaying writes.
Earlier failure records were retained; neither identity nor containment checks
were weakened to pass acceptance.

Build the two disposable fixtures, then opt in explicitly (use absolute paths):

```sh
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/pattern.m -o /tmp/intendant-monitor-pattern
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/placement.m -o /tmp/intendant-placement-fixture
python3 scripts/verify-macos-monitor-http.py --bin /absolute/path/to/intendant --fixture /tmp/intendant-monitor-pattern --placement-fixture /tmp/intendant-placement-fixture --report /tmp/placement-proof.json --allow-shared-session-monitor --check-recovery
```

Existing Screen Recording and Accessibility permission are required. No native
test is part of default CI, and no permission prompt is requested. Ordinary
application compatibility, streaming, keyboard/mouse control and clipboard/focus
isolation remain separate work.

Final serialized local validation passed: 6,499 binary tests (10 existing ignores), 1,020 required library/acceptance tests (3 existing ignores), 55 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. Focused monitor coverage passed 63 tests. The compile governor remained intact.
