# Bound-window controls and semantic actions

Implemented atop merged #932. Native semantic-control acceptance passed on the plugin Mac on 2026-09-18 using an isolated temporary-HOME daemon and disposable AppKit controls. This establishes the bounded fixture contract, not general application or Chromium compatibility.

## Public contract

| MCP operation | Facade | IAM |
| --- | --- | --- |
| `read_macos_window_elements {binding}` | `inspect display window-elements BINDING` | `DisplayView` |
| `act_macos_window_element {binding,element,action}` | `act display window-element BINDING ELEMENT ACTION_JSON` | `DisplayInput` |

Both require gate-resolved `OwnerSurface`, independently of IAM. HTTP and facade
calls preserve the actual caller, including when a scoped caller has an existing
user-display grant. Neither operation starts a broker/helper from idle. Generated
stdio entry points follow the existing owner-surface trust model. Linux and
Windows cannot execute these native operations. No permissions, grants, settings
or credentials are changed.

Actions are strictly tagged JSON:

```json
{"type":"press"}
{"type":"set_value","text":"Disposable exact text"}
```

Both binding and element token are mandatory. Tokens are opaque
`macos_element:<random-128-bit-id>` strings, never display selectors. Generic CU,
screenshot and display parsing reserve their malformed/case/whitespace variants
against fallback. No identifiers, titles, paths, native IDs or coordinates can
substitute for a retained element token.

## Read and retention

The same private main-thread monitor helper retains windows and elements. No AX
object is sent across a thread, and no unsafe Send/Sync implementation is added.
The semantic engine in `macos_monitor/controls.rs` injects a fakeable native
adapter; the narrow AX wrappers remain in `ax.rs`.

A read checks the exact retained process birth and AX window identity using the
existing AX/CG machinery, the retained nonprimary monitor identity/geometry, and
full containment of **both** current AX and CG window rectangles. Monitor geometry
must still equal the geometry frozen at binding; a layout change requires rebind.
AX/CG agreement retains placement's one-point observation tolerance, but window
and control containment has **zero tolerance**. The window must remain unchanged
during snapshot construction. No other window's controls are walked.

DFS is limited to 128 visited nodes, depth 16 (root at depth zero), 32 children per
node, and 16 actionable controls. Native children reads check the count first,
then request cap+1 and refuse overflow. Documented AXChildren absence
(`kAXErrorAttributeUnsupported` / `kAXErrorNoValue`) and successful zero counts
mean empty children; a zero count avoids an out-of-range indexed copy. IPC
failures, invalid objects, malformed types and excess counts still refuse.
The Chromium 153 fixture measured 51 nodes and depth 13; the initial depth-8
profile could not traverse even that small tree. Only the depth cap was widened;
128 nodes, 32 children, 16 controls, wire size and operation budgets remain unchanged.
Cycles, excess depth/nodes/controls, missing required attributes,
invalid geometry and oversized strings refuse the whole snapshot. There is one
replaceable inventory across all bindings and callers, not one per window.
It retains exact control objects and bounded exact ancestor chains, plus the
observed focus identity. It returns only:

- `element`: opaque one-use token;
- `role`: one of the admitted AX roles;
- `label`: AXTitle only, at most 128 UTF-8 bytes (absent title is empty);
- `bounds`: global logical-point rectangle;
- `operations`: `press` or `set_value`.

No AXValue, AXDescription, full document text or title-based lookup is used for
inspection. AXRole remains required. AXSubrole and AXContainsProtectedContent
are optional metadata: only AttributeUnsupported/NoValue without a value means
absence. Transport errors, successful null replies, contradictory replies and
wrong/oversized types refuse the snapshot or action. Explicit secure/password
roles or subroles and a true protected-content flag omit the entire subtree
before labels, children or values. Disabled controls and unsupported operations
remain excluded. AXTitle absence is an empty label; other label failures refuse.

This deliberately supersedes the initial requirement that every node supply an
AXSubrole. Apple's SDK declares it required only when role alone is insufficient;
ordinary AppKit controls need not synthesize AXUnknown. Explicit protected
content is now checked on containers too. This follows app-reported metadata,
not a guarantee against a dishonest app or sensitive text in ordinary fields.
See `macos-appkit-controls.md` for the baseline reproduction and native profile.

Every read reaching the helper clears the previous inventory first, including
failed refreshes. Every helper action takes the whole inventory before native
validation, including a rejected foreign element or mismatched binding. Placement
and unbind invalidate their binding's snapshot before attempting work; monitor
destruction invalidates its bindings before teardown. EOF releases everything.
Ingress rejection or a queued request cancelled before dispatch does not reach
the helper and does not refresh its inventory. The unchanged 16 KiB private JSON
line cap rejects oversize: no silent list or string truncation. The actual reply
envelope is size-checked before publishing snapshot tokens.

## Actions and evidence

Only enabled `AXButton`, `AXCheckBox` and `AXRadioButton` objects advertising
`AXPress` can be pressed. Only enabled, nonsecure `AXTextField` and `AXTextArea`
objects with settable AXValue can accept text. Settable AXValue is the required
editable-text capability; unavailable mutability refuses. Input is at most 1024
UTF-8 bytes. Text is compared to a bounded CFString readback exactly, including
Unicode and empty strings, without returning either the requested or read text.

Before an attempted setter/action, the engine checks the process birth, exact
retained window, monitor, current window rectangles against the snapshot,
current element role/security/enabled/operation state, exact bounded label and bounds, full
containment, existing TCC and unchanged observed focus. Bounded AXWindow and
AXParent checks use CFEqual on the retained window and each retained ancestor;
each parent must also expose its exact retained child in bounded current
AXChildren. Detachment with stale back-pointers, reparenting or replacement
refuses, even with the same title/role/frame. A changed label requires a fresh
inventory, even if identity and geometry are unchanged.
It never substitutes a freshly resolved object. Native wrappers repeat the
role/security/enabled/operation checks immediately before dispatch, and repeat
security checks before reading a value. Checks run again after the action and
around text readback. AX reads use the existing 50 ms per-object limit. Mutation IPC uses at most 500 ms, clamped to the remaining four-second operation budget, with deadline checks between calls. There
are no write retries, rollback, activation, raise, AXFocused writes, keyboard
shortcuts, CGEvents, clipboard use, permission prompts or focus restoration.

A result's `action` object carries `status`, `operation`, `action_attempted`,
`before`, last available `after`, `value_matches`, `focus_interference`,
`effects_unconfirmed` and optional detail. `before`/`after` are control bounds;
no contents are returned. The outer response repeats the attempt, focus and
uncertainty fields for receipt/transport failures.

- `verified` requires a text write, exact bounded readback and successful final
  identity/geometry/permission/focus checks. It verifies that observation only.
- `dispatched` is the successful AXPress result. Its effect is **unverified**;
  `effects_unconfirmed` stays true. There is no blanket verified press claim.
- `partial` is an unsuccessful validation/attempt. `action_attempted:false`
  records a known preflight refusal, with observed focus/bounds retained and
  `effects_unconfirmed:false`. Once the native setter/action path is entered,
  `action_attempted:true` is conservative even if a later native guard refuses.
  Errors after that point keep `effects_unconfirmed:true`; setters may apply
  before returning an error.

`focus_interference:true` records an observed change; false records a matching
last focus check; null means a required check could not complete. No failure
restores focus. Partial readback, observed focus changes and verified observations
survive final helper liveness or receipt-commit failure; outer `ok` is false.
A receipt failure cannot turn a known non-attempt into an uncertain effect.
The existing six-second helper exchange and twenty-second frontend wait remain.
Timeout while queued reports no attempt. Possible dispatch is published before
the final cancellation check, so cancellation at that boundary conservatively
reports uncertainty even if the helper is ultimately not called. Timeout/lost exchange after dispatch
reports `action_attempted:null`, because the parent cannot know whether AX acted,
and `effects_unconfirmed:true`. A result already buffered at the wait deadline is
preserved. The serialized helper finishes its bounded attempt after delivery
cancellation; a pipe failure retires the broker. Token consumption is never
rolled back, including after receipt expiry or lost delivery. Replay cannot
repeat a write.

A shared WindowServer is **not an isolated seat**. Other actors can change state
between observations; AX/WindowServer calls are not an atomic transaction, and
noninterruptible OS calls can exceed cooperative deadlines. These safeguards do
not establish continuous focus isolation, exclusive window ownership or a
security sandbox. Raw canvas/keyboard control and general Chromium compatibility
remain separate work.

## Supervisor validation

Hermetic tests cover: semantic fake coverage for identity,
replacement, ancestry, permissions/focus, secure/disabled controls, strict
containment, traversal/text/wire limits, inventory replacement across bindings,
replay and place/unbind/destroy invalidation; broker coverage for no startup,
queued versus dispatched cancellation, receipt expiry, late liveness evidence
and no duplicate effects; deterministic cancellation at the final dispatch check;
changed-label refusal and fresh-inventory recovery; native edge-validator
detachment/replacement checks and AX leaf/count/type/error/cap decoding;
actual HTTP/facade scope and IAM matrices; strict typed
schemas and private protocol. Existing placement fakes/protocol paths are retained.

The supervisor should compile on macOS (including Objective-C and AX FFI), check
Linux/Windows cfg paths, and run focused `macos_monitor`, `window_` and `element_`
binary tests, the repo's normal keyless regression battery and Clippy. Keep the
compiler governor environment unchanged. Rustfmt/Python AST/whitespace checks
are only static validation, not compilation or native acceptance.

Native acceptance is **supervisor-only**, opt-in, and requires preexisting
Accessibility/Screen Recording permissions. Build the disposable fixtures and
run from an owner shell, outside a supervised agent session:

```sh
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/pattern.m -o /tmp/intendant-monitor-pattern
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/placement.m -o /tmp/intendant-placement-fixture
clang -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/controls.m -o /tmp/intendant-controls-fixture
python3 scripts/verify-macos-monitor-http.py \
  --bin /absolute/worktree/target/debug/intendant \
  --fixture /tmp/intendant-monitor-pattern \
  --placement-fixture /tmp/intendant-placement-fixture \
  --controls-fixture /tmp/intendant-controls-fixture \
  --report /tmp/window-controls-proof.json \
  --allow-shared-session-monitor --check-recovery
```

The existing HTTP harness owns a separate temporary-HOME mock-provider daemon and
test monitor, invokes both companion ctl harnesses, and checks original display
inventory restoration. The controls harness binds/places only its own disposable
panel, verifies text independently through the fixture's normal field, checks the
button counter increments exactly once, rejects refresh/replay/replaced-control
and unbind tokens, and releases its binding/process. It records failure honestly;
no mutation is automatically retried. The fixture has an explicit small AX tree
with AppKit's normal leaf children behavior (no empty-children overrides),
a normal editable field, button counter, secure and disabled controls, and a
one-byte `r` command that replaces only its field with the same title/frame. It
starts below other windows without activation/key/main/raise, uses no user
documents, and exits on EOF or after 120 seconds. Its independent status file
never reads or exports the secure field value. Permission revocation, uncertain
cancellation and same-ID replacement remain fake tests, not changes to owner TCC.

## Native acceptance on 2026-09-18

Passed on macOS 26.4.1 (25E253), arm64. The fixture independently observed the exact Unicode text and one button-counter increment. Refresh/replay/replaced-control/unbind refusals passed, with secure and disabled controls excluded. The fixture was reaped, its directory removed, and the original online display inventory restored. Successful text and press checks reported no observed focus interference.

Acceptance found and corrected fixture hierarchy reporting: ignored AppKit controls must be filtered with NSAccessibilityUnignoredChildren and corresponding unignored-parent reporting, while native retained-ancestry guards stay intact. The 50 ms read timeout also produced an uncertain AXPress result; mutation-only messaging now gets a bounded 500 ms allowance. No uncertain action is retried. A fixture cleanup race was fixed by reaping its writer before removing its directory. Earlier failed reports remain retained.

The press JSON variant is an empty struct rather than a unit variant, so unknown press fields are rejected by serde as required; the existing strict-protocol regression now passes.

Two pure native-adapter regressions additionally pin the positive, remaining-budget-clamped mutation timeout and optional-label absence versus communication/type failures.

## Final local validation

The serialized local battery passed on 2026-09-18 with the compile governor unchanged: 6,547 binary tests (10 existing ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. The initial focused monitor pass covered 84 tests; the later full binary run also includes the added label/error and timeout-bound regressions. An initial test-only missing import was corrected and the entire binary suite rerun.

A fresh local independent review attempt was quota-blocked; no fresh review result is claimed. The previous four concrete review findings were addressed with regressions, and resumed source/native review found and corrected the issues recorded above. CI and repository review on the published head remain separate gates.

A subsequent final-build combined run passed placement but refused controls discovery; a diagnostic controls-only run listed and placed the window, then refused a read when focus was unavailable. Both restored the display inventory and cleaned up. These are retained failures, not a final-build native pass. The successful semantic run preceded the final optional-label error-classification tightening; the final source passed the full hermetic battery. Shared-session focus availability and general app compatibility remain limitations, not reasons to relax identity/focus checks or retry uncertain actions.

Post-publication continuation: the unchanged e2354b2 head completed its full native monitor/placement/semantic-control fixture on 2026-09-18 and then merged through the normal queue as 035c760. Earlier refusals remain historical evidence, not passes. Standard AppKit compatibility is recorded separately in macos-appkit-controls.md.
