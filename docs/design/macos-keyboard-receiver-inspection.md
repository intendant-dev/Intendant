# Bound-window keyboard receiver inspection

Follow-up to merged #945. This slice is an observation only. It adds
`read_macos_window_keyboard_target {binding}` and the façade spelling
`inspect display keyboard-target BINDING`; it does **not** add keyboard input.

## Contract

The typed stdio/HTTP route is `DisplayView`-gated and the monitor broker also
requires its existing `OwnerSurface` gate at admission, dequeue and result
delivery. The private helper can receive the operation only from that serialized
broker path. An unused or retired broker is refused without being started, and
non-macOS returns a clear unsupported-platform error. An owner surface remains
necessary even when a caller has a scoped display-view grant.

The sole successful report is:

```json
{
  "role": "AXTextField",
  "bounds": {"x": 0, "y": 0, "width": 1, "height": 1},
  "enabled": true,
  "keyboard_dispatch_supported": false
}
```

It contains no AX title/label/value, document text, secret, native object
pointer, process metadata, receiver ID, preparation or authority token.
`keyboard_dispatch_supported:false` is fixed by reply validation. A reported
role, enabled state or geometry does not imply key eligibility. The snapshot is
neither proof of key delivery nor a durable identity for a receiver.

The operation never posts a key, activates an application, clicks, restores
focus, changes privilege/TCC state, creates an input capability, or retains a
receiver after producing its response. It also never clears or creates existing
pointer or semantic-control preparations: it observes a binding without changing
those inventories.

## Native observation boundary

The helper starts with the exact retained PID/start generation, retained AX/CG
window and owned monitor binding. It requires full AX and CG window containment
within unchanged monitor geometry before and after native reads. It takes a
strict successful typed `AXFocusedUIElement` copy from the *retained target
application*, then strictly checks that receiver's `AXWindow`, PID and window ID
against the exact retained window. It does not use the human system-wide focused
object to choose a receiver.

The established exact human-focus observer is reused only as an unchanged
before/after witness. Its previously validated application-local fallback for
an empty system-wide CannotComplete response remains unchanged; it never
selects the target receiver. Target-application receiver copies stay strict. The application-local receiver is read twice,
must be the same exact object, and is checked at bounded checkpoints. Missing,
foreign, replaced, detached, protected, contradictory, oversized or timed-out
observations refuse.

Traversal begins at the retained window only, uses the established 128-node,
16-depth and 32-children limits, and reuses exact ancestry projection and
reciprocal current-membership checks (including the narrowly supported Chromium
bridge). No lookup can select a replacement by ID, label or metadata. Safety and
protected-content checks happen before descendant traversal or any label/value
read; this path never reads labels or `AXValue`. It reads only bounded role,
enabled state and geometry, then repeats safety/membership/window/monitor checks
after metadata and after both focused-object reads. The whole native transaction
shares the established four-second budget and AX FFI island in `ax.rs`; it adds
no unsafe-policy exception.

## Disposable acceptance profile

`tests/fixtures/macos-monitor/browser-keyboard-target.html` is a local
non-networked fixture with two nonsecret fields and one synthetic password field.
Its `selectKeyboardTarget(name)` function is fixture-only DOM focus setup;
it sends no native input and neither observes nor sets system-wide focus.

`scripts/verify-macos-chromium-controls.py --keyboard-target` is an opt-in
acceptance profile whose setup creates a monitor and places its disposable window.
The inspected API is read-only; the setup is not. A fresh Chrome for Testing
profile is launched by the existing
monitor supervisor. It is selected through the monitor HTTP fixture below and
checks a matching positive role/bounds report, a different focused nonsecret
field, protected-password refusal, stale-binding refusal, report field limits and
owned cleanup. It does not use a user browser profile, native key injection,
global-focus mutation or any key-delivery claim.

Run only with explicit owner approval on a disposable test monitor:

```bash
python3 scripts/verify-macos-monitor-http.py \
  --bin /absolute/worktree/target/debug/intendant \
  --fixture /absolute/worktree/target/receiver-proof/pattern \
  --report /tmp/macos-keyboard-target.json \
  --allow-shared-session-monitor \
  --chromium-app '/path/to/Google Chrome for Testing.app' \
  --chromium-supervisor /absolute/worktree/target/receiver-proof/browser-supervisor \
  --chromium-keyboard-target
```

This is a bounded acceptance profile, not a general browser, canvas, global
focus, keyboard routing, isolated-seat or arbitrary-site compatibility claim.

## Implementation notes and supervisor validation

The new helper operation is private stdio with only the existing opaque helper
binding; public callers can never provide a native receiver or authority handle.
The inspection branch intentionally avoids the semantic inventory and pointer
inventory APIs, so a successful or refused observation leaves their pending
tokens unchanged.

The supervising environment ran the focused checks and complete local battery.
Relevant reproduction commands:

```bash
cargo test -p intendant --bin intendant macos_monitor::keyboard
cargo test -p intendant --bin intendant macos_monitor::protocol
cargo test -p intendant --bin intendant mcp::tool_gate::tests::macos_window_tools
cargo test -p intendant --bin intendant web_gateway::mcp_gate::tests::window_http_dispatch
cargo test -p intendant --bin intendant mcp::tools_macos_monitor
cargo test -p intendant --bins
cargo fmt --check
```

The native browser command above is separately opt-in because it creates a
test-owned virtual monitor. It must remain a disposable Chrome-for-Testing
profile only; it cannot validate arbitrary applications, continuous focus
stability, key routing, keyboard dispatch, or a separate input seat.

Receiver and global-focus reads are bounded observations, not atomic snapshots.
Changes after the last check remain possible; no exclusive or durable keyboard
seat is claimed. Whole-run pointer/clipboard changes remain unattributed and do
not invalidate a passing action-level focus check. A foreground fixture observed
at any reported checkpoint, including cleanup, does invalidate native acceptance.

## Validation recorded on 2026-09-21

The final local run passed 6,626 binary tests (10 existing ignores), 1,022
required library/acceptance tests (3 existing ignores), 57 end-to-end tests,
workspace Clippy with warnings denied, formatting, whitespace and the controller
build. All 109 Python harness/evidence tests passed, including new independent
receiver-geometry tests. Native browser/pattern fixtures compiled with warnings
denied. An independent focused follow-up review found no remaining concrete
source defects. The existing compiler governor was selected explicitly without
changing its installed configuration.

Live acceptance did not reach receiver inspection: the existing window-placement
focus guard refused an unavailable AXFocusedUIElement. The browser/profile and
supervisor were cleaned up and original display inventory restored. A separate
metadata-only diagnostic returned CannotComplete for both system-wide and
foreground-application focused-element queries. Neither this result nor the
owner operating the machine is treated as permission to weaken focus checks.
The failed native run is retained separately; a complete published-head native
receiver acceptance pass remains a merge gate. No live plugin daemon, user
profile, TCC setting, CI policy or native input capability was changed.

## Resumed portability correction (2026-09-22)

The Ubuntu PR run exposed a compile error in the non-macOS refusal regression:
Result::unwrap_err requires the successful Receipt type to implement Debug.
That test is excluded on macOS, so the prior local battery did not compile it.
The assertion now extracts the error with err().expect while retaining the
required unsupported-platform message and the empty-queue assertion. No Debug
implementation was added to the production receipt, and no runtime behavior,
authorization, focus checks, traversal limits or CI policy changed.

Published-head native acceptance and refreshed validation are recorded on PR
#947 separately from the earlier focus-observation refusal.

## Explicit selection in native acceptance (2026-09-22)

The resumed no-click profile completed placement but the strict target-application
AXFocusedUIElement read refused. Its browser/profile cleanup and original display
restoration passed; that negative report remains separate. DOM focus by itself
was not sufficient in this tested Chromium instance. No production check was
removed or converted into a successful empty receiver.

A separately opted-in --chromium-keyboard-target-click-first profile now uses
the existing owner-bound prepare/click tools for exactly one click on the first
nonsecret fixture field. The inner spelling is --keyboard-target-click-first.
Both require the keyboard-target profile; neither is enabled by default. The
setup validates frozen native/DOM geometry, the returned preparation and actual
dispatch, then independently verifies the exact trusted down/up/click sequence
at the requested point. Uncertain results retain evidence and never retry. The
inspection tool itself remains read-only and never performs a selection click.

With that explicit setup, the native HTTP test passed both nonsecret receiver
reads with independently matching geometry, the protected-password refusal and
stale-binding refusal. No key was posted. The browser/supervisor/profile were
cleaned up and original display inventory restored. Eight new hermetic tests
cover selection planning, native/effect evidence, partial refusal, no retry and
explicit option gates. The earlier floating-point test subtraction assertion
was corrected without changing production or native acceptance tolerances.

For the positive acceptance command above add the separate flag
--chromium-keyboard-target-click-first. Omit it to retain the no-click comparison.
These are observations of one disposable Chromium profile, not an authenticated
DOM source, arbitrary-application compatibility or an independent keyboard seat.

The next Ubuntu run compiled and exposed two test-only message mismatches:
requires macOS was incorrectly checked as require macOS. The direct unsupported
case now pins the exact existing error; HTTP acceptance recognizes that same
existing tool-specific error while retaining IAM/owner checks. The fixed
production error and behavior were not changed. The refreshed Python suite
passes 117 tests. A new independent review of the fixture-only setup changes
was unavailable because the reviewer reached its usage limit; the saved
independent review covers the production receiver implementation.
