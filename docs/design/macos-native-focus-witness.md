# Native focus witness experiment (2026-09-24)

## Scope

Fixture-only follow-up to #952. Bracket production receiver reads with
independent native AX focused-element observations. Keep exact CF objects and
process launch identities in memory; publish only status, presence, validity,
equality and monotonic duration. No labels, field values, addresses or extra
PIDs are emitted by the witness. It is an observation, never input authority.

The new --chromium-native-focus flag requires --chromium-receiver-study,
--chromium-keyboard-target and --chromium-keyboard-target-click-first. All
existing exclusions for keyboard actions, pointer/scroll, placement-only and
other fixtures remain. The supervisor's --disposable-chromium-focus mode can
observe only its own explicitly launched Chrome for Testing process. The
installed daemon and existing user browser profiles are never the test target.

The only native input is the separately opted-in existing setup click. The
experiment sends no keys and never activates an application or alters human
focus. Native foreground-focus observation reads identities only. Existing
supervisor sampling reads pointer location and clipboard change count, not
contents. Production Rust, input behavior, IAM, the 50 ms AX timeout and
four-second operation budget are unchanged.

## Fixed cases

Six predetermined receiver reads on one owned window/binding:

1. Stable first field.
2. First-to-second fixture-only DOM focus transition inside a native bracket.
3. Second-to-first-to-second DOM round trip inside a bracket.
4. Eight scheduled DOM focus changes at 75 ms intervals, capped and stopped
   when the one client read returns. Actual completed changes are recorded;
   background timer scheduling is not assumed.
5. Protected field, for which successful receiver access is invalid evidence.
6. Stable first field again.

The churn case captures both ordinary fields' known geometry before beginning.
A successful read during churn must match one of those two nonsecret receivers,
not an arbitrary element. Other successful reads must match the selected field.
The study verifies no new keyboard or mouse events. Refusals stay in the fixed
dataset and do not cause another click, retry-until-success or extra attempt.

Collection is bounded by the existing 75-second study deadline. A native
witness permits at most 16 non-overlapping brackets, each capped at 30 seconds.
Start/end sequence and phase are checked, and the end receipt must retain the
same start metadata. Target process identity cannot be substituted at finish.
Unknown native replies remain null, not false/unchanged. Malformed evidence,
lost ownership, target foreground, unexpected input or transport loss aborts.
Native marker receipts and the original error survive ordinary exceptions;
cleanup still belongs to the existing supervisor/harness. Checkpoints are not a
guarantee against filesystem failure, process death or power loss.

## What the observations mean

The native witness samples the system-wide focused AX element and the owned
browser's focused AX element at the bracket endpoints. Both are retained and
compared using native CF equality plus process launch identity. Failed reads
preserve numeric AX status, returned-value presence and PID-read status.
It uses no fallback that interprets failed global focus as unchanged.

Bracket duration includes marker exchange, fixture DOM setup, command startup,
transport and the production read. It is not the duration of an atomic native
snapshot. Churn is scheduled across the client-call interval, not synchronized
to a particular internal AX checkpoint. A successful read does not prove that
all churn overlapped the engine's observation or that a concurrent input would
be correctly delivered. No keyboard input is exercised here.

A DOM switch-away-and-back can coexist with equal native endpoints. This does
not establish that AX exposed every intermediate DOM state; it demonstrates
why endpoint equality cannot by itself establish continuous UI stability.
Likewise, unchanged human endpoints are not proof of continuous isolation.
Human changes are observed passively, never induced or attributed to the agent.
A live experiment with deliberate human activity remains a separate gate before
changing the production human-focus rule.

completed/measurement_valid describe collection quality. The separate response
categories, native_transition_observed, roundtrip_native_endpoints_equal and
human-known/unknown counters describe observations, not keyboard capability or
general application reliability. Failed and partial attempts remain retained.

## Validation

Run scripts/test-macos-focus-witness.py and all existing macOS Python harness
tests. Native nonposting tests: compile focus-witness-tests.m with clang
-fobjc-arc -Wall -Wextra -Werror and AppKit/ApplicationServices frameworks.
They create only AX reference objects; they do not launch an application,
create a window, request focus attributes, or post input. Compile browser.m
with the same flags plus CoreGraphics. Use the existing HTTP proof runner with
the explicit flags above for manual native acceptance, never against user data.

Primary API references are Apple's AXUIElementCopyAttributeValue,
AXUIElementGetPid, AXUIElementSetMessagingTimeout and NSRunningApplication
launchDate documentation and their installed SDK declarations.
Native results and limitations are recorded on #953 and in hashed proof files.
