# Raw pointer receiver delivery (2026-09-18)

## Scope

This follow-up to merged #940 (`82449ea`) validates disposable AppKit mouse
receivers. It does not add a daemon/MCP input operation, change IAM, or claim
Chromium/canvas compatibility or an independent input seat. The owner remained
active on the Mac. Whole-run pointer/foreground changes remain unattributed;
observed target activation still refuses. No keyboard input, global posting,
input taps, cursor warps, monitor hotplug, screenshots, user profiles/documents,
permission changes or installed-daemon changes are part of this slice.

## Native findings

The original pilot queried its window before AppKit/WindowServer had published
its bounds. It refused with zero posting calls. The fixture now finishes startup
and pumps only its own application queue while waiting at most one second for
its exact retained window. It checks foreground observations and never retries
a posting attempt. Separate preflight fields distinguish unavailable bounds,
a key window and a held human left button; an observed held button refuses.

After readiness, the original CGEventCreateMouseEvent construction reached the
application queue with AppKit window number zero, despite correct Quartz
under-pointer window hints. No canvas receipt/effect occurred. An NSEvent mouse
constructor with the explicit window number, copied to a CGEvent, supplies the
window address. Events still travel through CGEventPostToPid and the ordinary
NSApplication/NSView dispatch path; the receiver never directly invokes its own
mouse handler or synthesizes an application result.

That constructor worked within the receiver process. In the separate sender
process, the queue received the correct global coordinates/window number but
incorrect local coordinates (-1, 253 rather than 160, 126 in the diagnostic).
The fixture therefore runtime-resolves CGEventSetWindowLocation and
CGEventGetWindowLocation, sets the exact local point, and requires exact readback
before posting. Their signatures are declared in WebKit's test SPI header:

https://github.com/WebKit/WebKit/blob/main/Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h

These are private APIs, not compatibility guarantees. Missing symbols or failed
readback refuse without an alternate routing path. The default construction-only
mode still creates no NSApplication/window and posts nothing. When the SPI is
available it additionally checks zero, negative and fractional local points.

## Separate sender and receiver

The new explicit --allow-disposable-cross-process-click mode creates the same
nonactivating receiver panel, then launches a private sender from the same
executable with an NSTask-owned PID. The sender accepts a fixed-size plan over a
private pipe and a dispatch marker, never PID/window/coordinate command-line
arguments. The only allowed destination is its current direct parent, which
must own the exact window with the same observed frame. Permission, foreground,
human-button and parent/window checks run before dispatch and again after both
events have been allocated and checked. The receiver retains its window for the
whole exchange. This fixture-only parent relationship is not a general remote
application-lifetime or IAM solution.

The sender posts one down/up pair and retains its private event source until a
receiver acknowledgement or bounded shutdown. In the initial immediate-exit run,
no receiver events were observed. With acknowledgement-based lifetime, the queue
received the pair; the exact historical timing is not independently traced.
There are no event retries, focus restoration or fallback posting routes. The
sender has a four-second alarm; the receiver has bounded dispatch/cleanup waits,
and the outer Python supervisor bounds output and kills/reaps only its child.

Posting-call counts, application-queue observations, canvas receipts and canvas
state are distinct evidence. A verified result requires the exact ordered pair,
correlation tag, receiver/source PID, window, global AND local coordinates, and
one tagged canvas-counter increment. Tags are correlation, not credentials.
Unrelated human events cannot supply positive evidence. Overflow, reordered or
duplicate pairs, unknown posting count, missing acknowledgement, unavailable
observations, wrong identities and incomplete cleanup cannot pass. A missing
launched-sender report leaves posting count null rather than claiming zero.

## Acceptance and limits

Both final native modes passed: self-process and cross-process. Each delivered
exactly two matching canvas receipts and one effect. The receiver window closed;
the cross-process sender was reaped. The self-process run recorded a whole-run
pointer change with attribution undetermined; the cross-process run had unchanged
before/after samples. Neither run observed the target foreground. These samples
are not proof of continuous isolation, and the owner was active throughout.

Earlier failures remain separately recorded: unavailable initial bounds,
window-zero events, wrong local coordinates, no receiver delivery from the
immediate-exit sender, and a held-human-button refusal with zero posts. No failed
run was converted to a pass or overwritten by a later report.

Production Rust, tool declarations, permissions, CI and compiler settings are
unchanged. Chromium raw-canvas routing, owned-monitor/binding integration,
cancellation between pair halves, and keyboard/chords are still separate gates.
The existing HTTP semantic text/button capability is unaffected.

## Reproduction

All report paths must be new; the supervisor reserves each owner-private file
before any possible effect. The two live commands below create one disposable
panel each; they are not unattended CI checks.

```sh
clang -fobjc-arc -Wall -Wextra -Werror \
  -framework AppKit -framework CoreGraphics -framework ApplicationServices \
  tests/fixtures/macos-monitor/raw-pointer.m -o /tmp/raw-pointer-delivery
python3 scripts/test-macos-input-evidence.py -v
python3 scripts/test-macos-chromium-harness.py -v
python3 scripts/verify-macos-raw-pointer.py \
  --fixture /tmp/raw-pointer-delivery --report /tmp/construction-new.json
python3 scripts/verify-macos-raw-pointer.py \
  --fixture /tmp/raw-pointer-delivery --report /tmp/self-new.json \
  --allow-disposable-process-click
python3 scripts/verify-macos-raw-pointer.py \
  --fixture /tmp/raw-pointer-delivery --report /tmp/cross-new.json \
  --allow-disposable-cross-process-click
```

## Local validation

The complete binary, required-library/acceptance and E2E suites passed, as did
workspace Clippy with warnings denied, formatting and whitespace. Thirty Python
checks passed (22 evidence/supervisor plus 8 Chromium harness). Native compilation
passed with warnings denied; nonposting construction/readback passed 16 cases
on this Mac, and seven invalid-argument/pipe cases refused without posting.
Full Windows/macOS runtime validation remains the merge-group gate, not the
compile-only non-Linux PR checks. Exact counts and commit-bound native results
are recorded in the PR and execution manifest.
