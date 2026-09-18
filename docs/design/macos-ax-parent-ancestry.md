# Exact native AX parent bridges (2026-09-18)

## Observed baseline

Merged #938 provides placement, not Chromium semantic acceptance. A fresh isolated
Chrome for Testing 153.0.8010.52 fixture reproduced `retained AX parent changed at
depth 7 for AXWebArea`. Its bounded read-only diagnostic showed that an AXGroup
exposes an AXWebArea directly while that WebArea reports an intermediate
AXScrollArea as its parent. The intermediate reports the original group as its
parent, exposes the exact WebArea, and shares its process/window. The group does
not expose the intermediate itself. No labels, values or user application data
were needed for this graph diagnostic. The baseline restored the display inventory
and terminated its exact test browser/profile.

## Correction and boundary

`macos_monitor/ancestry.rs` implements a fakeable graph projection used by the
native AXChildren adapter. Only Group -> ScrollArea -> WebArea is recognized.
All role, PID, AXWindow, actual-parent and bounded child-list checks must agree.
The projected ScrollArea retains the exact originally exposed child as a witness;
its traversal returns only that child, never other content hidden beneath it.

The normal engine walks the actual intermediate, including its security check,
before reading descendant labels. Protected intermediates omit the subtree.
Every later membership check compares both the intermediate and original-child
witness. A current projection cannot silently replace either retained object.
The original exact-parent, process generation, window, geometry, focus, permissions,
one-use-token, receipt/cancellation and no-retry behavior are unchanged.

The projected node consumes the existing 128-node/depth-16 budget. Each node
holds at most two AX references (one additional exposed-child witness only on a
bridge); a retained path remains at most 17 nodes, with at most 16 controls.
The 32-child, 16 KiB wire and four-second operation caps are unchanged. Handles
and bridges stay on the helper thread and are never part of the public protocol.
Other mismatched graph shapes still fail the original exact-parent validation.
This is not a general browser compatibility or independent input-seat claim.

## Validation plan and reproduction

The same synthetic Chromium page and fresh private browser profile are used.
Tested text and button actions go through ctl -> HTTP -> the native AX adapter.
CDP creates the background window and observes synthetic results; field replacement
and document reload are deliberate negative-fixture changes, not implementations
of the actions under test. No existing tabs, user documents, OS permissions,
installed daemon or runner configuration are changed.

Run the existing `verify-macos-monitor-http.py` harness with `--chromium-app` and
`--chromium-supervisor`, without `--chromium-placement-only`. `browser.m` now records
bounded parent relationships on a failed inventory. The harness also refuses an
old element token after same-window document reload. Native positive acceptance
and final test totals are recorded below only after execution.

Deterministic graph tests cover exact bridge retention, changed original child
under an unchanged intermediate, replaced intermediate, detachment, reparenting,
foreign process/window, wrong roles, metadata errors, protected subtrees, byte-free
content inspection, existing child limits and expired deadlines.

## Executed validation

On macOS 26.4.1 arm64, the full semantic Chrome for Testing 153.0.8010.52
profile passed (`target/ancestry-proof/quiet-native.json`). It verified exact
Unicode text via AX and independent page readback, one button-counter increment,
refresh/replay refusal, same-frame control replacement refusal and refusal of an
old token after document reload in the same native window. Placement used one
write and its identical no-op used zero. The browser was never observed frontmost;
foreground, pointer and clipboard-change-counter observations were unchanged.
No clipboard contents were read. The exact browser/profile were removed and the
original display inventory restored. The successful run used one discovery call.

The original standard-AppKit profile also passed after the changes
(`appkit-quiet.json`), including protected-subtree revalidation and cleanup.
These are bounded observations on disposable fixtures, not continuous isolation
or arbitrary-site/canvas/keyboard compatibility. All mutations under test used
HTTP/ctl and AX; CDP was only fixture setup, observation and negative replacement.

Validation retained negative runs too: the baseline parent mismatch, initial
AXRole unavailability, a four-second traversal deadline, window-startup
unavailability, and a text write whose exact readback matched but whose final
focus observation failed. That write remained partial/effects-unconfirmed and was
not replayed. An AppKit run likewise refused unavailable focus. Every recorded
native attempt restored the display inventory and reaped its own fixture.

The targeted membership path checks only the exact retained edge rather than
re-reading unrelated sibling metadata for every ancestor/control. Ordinary edges
still need exact current raw child membership; projected edges also recheck the
original exposed-child witness and all bridge relationships. This removes
redundant IPC without relaxing the four-second operation or 50 ms read limits.
The harness separately allows at most three read-only startup discovery calls,
records each, refuses PID/window changes, and never retries a mutation. Its two
new hermetic tests cover that bound, exact identity and permission refusals.

Final local validation: 6,569 binary tests (10 existing ignores), 1,022 required
library/acceptance tests (3 existing ignores), 57 E2E tests, 96 focused monitor
tests and 8 Python harness tests. Workspace Clippy with warnings denied,
formatting and whitespace passed. A library doctest initially failed to resolve
a crate during overlapping shared-target builds; the serial rerun passed. One
E2E run missed the unchanged mock-provider test's expected startup-error text
although the process exited with failure. The full E2E confirmation passed with
no source/assertion change; the original failure is retained, not counted as a
pass, and its historical timing was not traced. No CI/runner settings changed.
