# Fixed receiver observation study (2026-09-24)

## Scope

This slice measures the complete existing HTTP receiver-inspection path in one
disposable Chromium window on an owned virtual monitor. Production code,
authorization, native posting, the 50 ms AX timeout, four-second operation
budget, and exact human-focus checks are unchanged. No read recovery or input
retry is implemented. The read-only study module has no input API.

The explicit --chromium-receiver-study outer flag requires both
--chromium-keyboard-target and --chromium-keyboard-target-click-first. The inner
flag is --receiver-study. These exclude arrow, pointer, scroll, placement-only,
and additional controls/placement fixtures. Setup retains existing monitor
creation, window placement and one separately opted-in verified selection click.
The study itself sends only read_macos_window_keyboard_target calls. CDP selects
only the synthetic page fields and observes geometry/event counts. It does not
inject the input being tested. No keys are requested by this profile.

## Fixed plan and evidence

Twelve planned reads: first field four times, second field four times, protected
field twice, and first field twice again. Only phase transitions select a field;
refusals never cause re-selection or extra attempts. The same browser, window
and binding stay in use. The study is capped at 75 seconds within the existing
outer deadline. Expiry, transport failure, lost supervisor, changed fixture,
malformed result, protected success, or unexpected key/mouse activity aborts it.

A full checkpoint is replaced atomically before each read and after each row.
Partial attempts and durations survive ordinary exceptions; this is not a
promise of durable cleanup after process death. Cleanup remains the existing
harness/supervisor responsibility and must be checked separately.

Client monotonic durations include CLI startup and transport; they are not AX
call durations. Native failure status and all four #951 timing fields are
extracted separately when actually present. Missing diagnostics stay unknown.
Error classifications do not identify whether macOS, Chromium, the timeout, or
the human caused a refusal. Unknown and contradictory replies are not recovered.

completed/measurement_valid describe completed collection with valid evidence,
not availability. The study profile's passed field describes that measurement
contract and successful cleanup, not a capability acceptance pass. The separate
all_nonprotected_reads_succeeded flag and accepted_nonprotected denominator
state actual read availability. Failed and unattempted slots remain visible.
refused_protected counts refusals while the fixture selected its protected
field; its separate reason categories must not be treated as proof every refusal
was caused by a protection check. The finite, correlated sequence from one
window is not a general application failure-rate estimate.

Successful snapshots are checked against independent fixture geometry and the
original small public schema. No labels, values, document content or authority
tokens are added. The new DOM observation exposes only a key-event count, not
key content. Before/after desktop samples remain unattributed and cannot prove
continuous isolation. Changing user input or focus to obtain a pass is forbidden.

## Controlled focus tests

Five hermetic model interleavings characterize the actual unchanged engine:

- A human-focus change between independent read operations permits a fresh read
  with the same receiver; that read grants no keyboard authority.
- A focus change during one snapshot rejects it even with an unchanged receiver.
- An unavailable focus read is unknown, not equivalent to unchanged.
- A focus change after either arrow preparation rejects before construction,
  consumes the attempt, and cannot be refreshed by read-only inspection.
- A change after posting retains two attempted calls and uncertain effects.

These are model-controlled interleavings, not native concurrent-human acceptance
or evidence that the current guard is the best product policy. No keyboard
policy is relaxed. A native controlled concurrent-user experiment remains a
separate gate before changing these invariants.

## Reproduction

Run scripts/test-macos-receiver-study.py and the existing macOS Python harness
tests without invoking native APIs. Run the repository Rust/library/E2E/Clippy
battery with the compiler governor intact. Native study uses the existing
scripts/verify-macos-monitor-http.py with absolute controller/pattern/browser/
supervisor/report paths, --allow-shared-session-monitor, --check-recovery, and
the three explicit keyboard-target/click-first/receiver-study flags above.
The browser must be Chrome for Testing with a disposable private profile. Never
point this profile at an installed user browser or the live plugin daemon.

Primary API references: Apple AXUIElementCopyAttributeValue and
AXUIElementSetMessagingTimeout documentation. Native results are recorded on
the PR and in hashed proof files, not inferred from passing CI.
