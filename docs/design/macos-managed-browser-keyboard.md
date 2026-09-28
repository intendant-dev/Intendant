# Managed browser keyboard input on an owned macOS monitor

This is browser-page input, not an independent macOS input seat. The owned
browser and monitor share the logged-in WindowServer with the user, but keyboard
commands go over the exact managed page's CDP socket instead of global CGEvents.
There is no browser activation, clipboard operation, system shortcut, arbitrary
CDP method, or fallback to the user's display.

## Interface

Owner-authorized callers use `execute_browser_workspace_keyboard` with an exact
`workspace_id`, a fresh canonical UUID `request_id`, and one `action`:

```json
{"workspace_id":"<owned-workspace>","request_id":"<fresh-uuid>","action":{"type":"insert_text","text":"Hello"}}
```

`insert_text` inserts 1..4096 UTF-8 bytes into the eligible focused page field.
It supports Unicode but does not manufacture physical key-down events. `key`
takes one closed key name: Enter, Tab, ShiftTab, Backspace, Delete, ArrowLeft,
ArrowRight, ArrowUp, ArrowDown, Home, End, PageUp, PageDown, Escape, or Space.
`select_all` is a bounded page editing operation, not a system-wide shortcut.
Each key is a down/up pair; the API never exposes a held-key primitive.

The dynamic plugin facade is `act` with argv:

```text
browser keyboard WORKSPACE REQUEST_ID ACTION_JSON
```

The CLI's existing generic tool path also works:

```sh
"$INTENDANT" ctl tools schema execute_browser_workspace_keyboard
"$INTENDANT" ctl tools call execute_browser_workspace_keyboard --args '<request JSON>'
```

An ordinary supervised session does **not** gain owner authority by supplying a
workspace ID or owner-session label. Scoped callers remain refused. This initial
route needs an authenticated owner surface plus the normal display permissions;
a separately explicit per-session delegation is not implied by this feature.

## Identity and lifecycle

The controller validates the live workspace, its retained supervisor, exact
monitor generation, and native bound window. The native window is checked through an internal read-only Accessibility helper
operation: exact identity, root protection, AX/CG geometry and monitor containment,
without minting semantic tokens or reading the unrelated human focused object.
All existing native input focus guards remain unchanged. The
page-local active element is read through one fixed, side-effect-checked native
DOM getter in an isolated execution world, pinned to the frame and a system-unique
context ID. The page cannot substitute its own getter, and there is no focus setter,
universal-access grant, caller-supplied script, or CSP bypass. DOM identity and the
browser accessibility tree then validate the enabled, nonprotected receiver. Text
and selection editing require an eligible text receiver; navigation keys can
operate on other enabled nonprotected controls such as a button. Unknown state
refuses; document changes invalidate the observation.

The connection is a numeric loopback TCP socket with a matching recorded page
path and port. The private profile's endpoint and the browser's process inventory
must match the retained native process. The original page is checked independently;
an unexpected second page refuses rather than retargeting. Message/frame/time
budgets are bounded, and no caller chooses an endpoint or arbitrary protocol verb.

The browser lifecycle lane serializes keyboard use with managed create/close and
monitor destruction. A busy lane refuses before input rather than queueing an
unbounded stale request. Once dispatch starts, an owned worker finishes the one
key pair despite requester cancellation. Loss of the down acknowledgement still
permits one release attempt on the **same socket**, never reconnection or replay.
A delivery error quarantines that workspace until its owner inspects/closes it.

Request UUIDs are one-use, including uncertain dispatch. The bounded daemon-local
ledger does not evict an old request to admit its replay. A new UUID is not a reason
to replay an action whose effect is uncertain. Results distinguish command attempts
and acknowledgements; `effects_verified` stays false because CDP acknowledgement
is not application readback. Foreground-process and monitor observations report
changes without claiming an atomic OS routing lock or continuous isolation.

## Native policy and acceptance

The earlier native-arrow concurrent-typing proposal remains historical, not part
of this deliverable's native routing policy. A forward commit restores main's
held-key and exact focus guards; no browser-only test can accept those relaxations.

The opt-in `verify-macos-managed-keyboard.py` harness owns a temporary daemon,
private browser profile, and virtual monitor. It sends tested actions through
Intendant, observes actual fixture values/key events separately, and records a
read-only foreground/clipboard/HID counter witness. It distinguishes text insertion
from key pairs, rejects duplicate effects, never reads personal typed text or
clipboard contents, and cleans up only its own resources. `--require-activity`
requires observed counter progress inside the action span; even that is aggregate
evidence, not authenticated human provenance or proof of continuous isolation.

Native results, exact binaries, and outstanding acceptance limits belong on the
PR and in the evidence report; hermetic mapping tests are not native acceptance.

## Native development result (2026-09-28 UTC)

The fixed 14-step managed-browser plan passed in one fresh session after correcting
page-local focus observation and separating CDP window validation from the native
input focus witness. Nine complete page key pairs, six input events, exact ASCII/
Unicode/multiline and rich-text readback, one button activation, and one refused
UUID replay were independently observed. The production receipts still do not
claim effects from CDP acknowledgements alone. Both raw and facade entry points
were exercised. Exact memory-only capture and browser/profile/monitor cleanup passed.

The observer reported unchanged foreground process and clipboard across 63 samples
inside the action span, but no physical keyboard activity during that successful
span. This is not positive concurrent-human-typing acceptance or continuous isolation.
The development binary and source hashes, steps and limitations are recorded in
`evidence/macos-managed-keyboard-20260928.json`. Historical failed runs remain in the
local proof directory; no failed or uncertain input was replayed in the same session.


## Coordinated human-activity acceptance

The opt-in harness supports fixed `--start-delay-ms` (0..30000, default 0)
and `--step-delay-ms` (0..2000, default 100). A ten-second start delay and
1500 ms step delay give the operator time to type normally while the fixed
plan runs; they never wait for activity or retry input until acceptance.
`--require-activity` requires counter progress strictly inside at least one
successful request interval, not just between requests during pacing gaps.
The report distinguishes request overlap from whole-plan activity. It still
cannot authenticate human provenance or prove atomic/continuous isolation.

An exclusive private POSIX lock serializes updated harnesses across worktrees
before any display observation. Contention refuses rather than queues. The
lock does not coordinate older harnesses; operators must stop concurrent test
scheduling before this acceptance. Existing unrelated processes and displays
are never killed or recreated to manufacture inventory restoration.

Keyboard effects, the final exact screenshot, and owned cleanup are recorded
even when the separate activity requirement fails. Such a run remains failed
for concurrent acceptance; a successful baseline is not renamed as overlap.

The paced 2026-09-28 confirmation used the published 46fb228f implementation
with unchanged Rust sources and a separately pinned coordination harness. All
13 positive transitions, nine key pairs, six input events and the duplicate
refusal passed. Exact captures and complete owned cleanup passed; the foreground
process and clipboard change count stayed unchanged across 434 action-span
samples. No HID keyboard activity occurred, so concurrent acceptance remains
false. See `evidence/macos-managed-keyboard-coordinated-20260928.json`.
The earlier overlapping-test inventory failures remain preserved separately;
they are not evidence that the browser failed its own resource cleanup.
