# Native keyboard-activity overlap study (2026-09-25)

Fixture-only follow-up to #956. Observe HID-system key-down/key-up counter progress around one existing production ArrowLeft or ArrowRight dispatch. The fixture never synthesizes, captures, names, or records the user's keys, characters, shortcuts, text, or target application; it uses only aggregate `CGEventSourceCounterForEventType(kCGEventSourceStateHIDSystemState, ...)` deltas already present in the activity witness.

A fixed fresh run uses one disposable Chromium profile and owned virtual monitor with the existing separately opted-in setup click. Inside the dispatch focus/activity witness bracket, the harness waits for at least one completed HID key pair (both key-down and key-up counters advance) before launching the one production arrow request, then samples aggregate counters while the client process is alive. The run records one of two containment outcomes: (1) production explicitly refuses before posting, with zero posting calls/effects and unchanged fixture, or (2) production dispatches exactly one arrow pair/effect while the HID counters advance by at least two key-down and two key-up events during the live-client interval, with unchanged foreground-focus endpoints and no target foreground observation. Because the tested Arrow can contribute at most one down/up pair even if it appears in this counter source, that threshold establishes counter progress beyond the tested pair without assigning the additional activity to a person or device. A refusal is evidence about containment, not keyboard availability; a dispatched effect is evidence about the disposable fixture only.

The existing production held-key/button/shortcut checks, exact retained receiver/window/monitor identity, protected-content checks, one-use token, 50 ms AX timeout and four-second operation budget are unchanged. The harness does not keep trying until input happens: missing required keyboard activity, counter regression, malformed evidence, uncertain/partial dispatch, changed receiver/geometry, target activation or cleanup failure ends that predetermined run. ArrowLeft and ArrowRight are separate fixed cases, not retries.

HID-system counters are activity context, not authenticated human identity. They do not prove which device/person generated activity, what keys were pressed, or overlap with the narrower native posting instant. Endpoint equality does not prove continuous focus stability. No production guard is weakened from this study, and no general routing/isolation claim follows from it.

## Acceptance precision (2026-09-25 follow-up)

Prior keyboard-counter activity and activity during the request are different facts.
In-flight deltas are calculated between the first and later samples both collected
while the client process remains alive, not against a prelaunch sample. One in-flight
sample alone cannot prove progression. A zero-post refusal is retained as such even
without subsequent counter changes, but its overlap flag stays false. No synthetic
human input is used to obtain a positive sample.

A delivered-effect containment result additionally requires known unchanged native
human, receiver and foreground endpoints. Unknown or changed observations do not
become a containment pass. Counter schemas, monotonicity and derived claims are
validated. The existing production focus/input guards and source-attribution caveats
are unchanged; optional review availability is not a landing requirement.
