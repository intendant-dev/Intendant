# Background keys with concurrent-activity witnesses (2026-09-24)

## Scope

Fixture-only extension to #953. One fixed production ArrowLeft or ArrowRight
attempt is independently bracketed at preparation and dispatch. Production Rust,
IAM, input construction, one-use tokens, timeouts and focus/protection guards
are unchanged. No automatic native retry, activation or human-focus mutation.
The installed daemon and existing user browser profiles are not targets.

The explicit --chromium-concurrent-key-study outer flag requires one of
--chromium-arrowleft / --chromium-arrowright plus keyboard-target and the
separately opted-in setup click. It excludes receiver-study, native-focus,
pointer/scroll, placement-only, and other fixtures. The inner spelling is
--concurrent-key-study. The native supervisor has its own
--disposable-chromium-concurrent-key mode; it cannot post keys itself.
The tested key is sent only through the existing production HTTP tools to the
owned window on the isolated daemon's virtual monitor.

## Experiment

One fresh disposable browser/profile per key. After one verified native setup
click, the nonsecret fixture starts with caret 2. The preparation must describe
the selected direction, same window and independently checked receiver bounds.
The key-preparation token remains internal and is omitted from study evidence.
Each operation gets a separate sequence/phase-checked native focus bracket.
A definite preparation/dispatch refusal is recorded, never retried. Unknown,
partial or lost dispatch, malformed evidence, changed geometry, target
activation, or missing cleanup stops the run. Fixture effects are recovered
where possible even after a lost reply or failed end witness.

The whole study is bounded by 40 seconds within the existing outer deadline.
Each witness keeps the #953 30-second bracket bound. Observation polling after
the one posting request is bounded by two seconds and never posts another key.
No new key preparation follows uncertain delivery. Separate fresh runs are a
fixed test plan, not a retry-until-success policy.

A verified effect requires the existing strict arrow acceptance: exactly one
trusted down/up pair with the selected key, one effect, caret 2 to 1 or 3,
unchanged nonsecret text, and consistent production receipts. The native
receipt must also match the exact preparation's receiver and window.
Collection completion, validated collection, verified application effect, and
observed concurrent activity are separate report fields. A profile-level pass
means valid collection/cleanup; a recorded refusal is not a keyboard success.

## Passive activity evidence and its limits

The fixture observes CGEventSourceCounterForEventType on
kCGEventSourceStateHIDSystemState, for any input, key-down, key-up, mouse motion
and scrolling. It emits only interval count differences. It uses no event tap,
key identity, characters, input contents, event stream or installed hook.
The event counters are sampled only at the marker endpoints in this explicit
mode. Counter regression or wrap becomes unknown, not zero activity.
Sequential counter reads are not an atomic snapshot. Zero differences mean
no counter change observed, not proof of inactivity or continuous isolation.

Apple documents this source as the hardware/HID event state table. It is not
an authenticated human-identity signal. A positive count supplies activity
context, not proof that a specific person produced input or that it overlapped
the narrower internal posting instant. The full bracket includes command
startup, transport, AX checks and marker exchange. Preparation activity is
never substituted for dispatch activity. Focus endpoints retain the existing
null/unknown handling and cannot prove continuous stability.

Human activity is never induced by changing the user's focus or posting input
to their applications. A run with no observed activity does not satisfy a
concurrent-human acceptance claim. Deliberate human keyboard overlap and
general routing isolation remain unverified unless separately established.
No production guard is changed on the strength of counter or endpoint data.

## Failure evidence

Checkpoints precede each operation and preserve returned native receipts before
validation. A finish-witness error cannot erase a lost-dispatch error or make
the attempt look unattempted. The outer timeout reader now accepts the exact
requested study profile, including native_focus and concurrent_key; previously
it accepted only receiver_study and could discard other profiles' checkpoints.
It still rejects a different profile and preserves the original execution error.
Ordinary exceptions are covered, not process/power loss or filesystem failure.

## Validation

Run scripts/test-macos-concurrent-keys.py and existing macOS Python tests.
Compile activity-witness-tests.m and browser.m with clang -fobjc-arc -Wall
-Wextra -Werror and AppKit/ApplicationServices/CoreGraphics. Counter tests are
pure: they perform no native counter sampling, AX attribute reads or posting.
Run the repository Rust/library/E2E/Clippy battery with the governor intact.
Native runs use the existing isolated HTTP runner and owned monitor; never
substitute user applications or weaken a refusal to collect a success.

Primary references: Apple CGEventSourceCounterForEventType and
CGEventSourceStateID.hidSystemState documentation, and installed CGEventSource.h.
Exact revisions, outcomes, cleanup and evidence hashes are recorded on #954.

Unexpected fixture text or key names are redacted in saved evidence. The known fixture text and the two fixed arrow names remain available for inspection; arbitrary typed contents are not copied into the report.

The initial live run on 5df66502 caught a harness schema mismatch before key dispatch: prepared receivers have three fields, while read-only observations have a fourth capability field. Both schemas now have separate strict decoders sharing the unchanged numerical geometry checker. The failed run and confirmed cleanup remain recorded; its ArrowRight slot was not attempted.
