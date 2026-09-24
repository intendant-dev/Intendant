# Bounded focused-element re-observation (2026-09-24)

The corrected #954 acceptance stopped before its setup click: the existing
foreground-application AXFocusedUIElement read returned empty
kAXErrorCannotComplete after 55,102 microseconds with a configured 50,000
microsecond timeout and 3,793,942 microseconds of operation budget remaining.
That is evidence of a failed read near the configured timeout, not proof of
which component caused the delay. No input was posted by that attempt.

This slice permits at most one additional read of AXFocusedUIElement on the
SAME retained application object after that specific empty native failure.
It never retries an input, selects a replacement window, or treats unknown
metmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmetmbudget are checked again before a second call.
Existing process identity, foreground, exact receiver, ancestry, geometry and
protected-content validation remains authoritative. No safety metadata read,
AX action, setter or posting is retried, and there is no retry-until-success.

Changes are deliberately limited to application-local focused-element reads
used by the existing foreground fallback and bound-keyboard receiver path.
The system-wide focus router and optional security metadata rules are unchanged.
Native acceptance, source review and test results must be recorded separately;
implementing a reread is not evidence that it cures every intermittent failure.

Primary references: Apple AXUIElementCopyAttributeValue and
AXUIElementSetMessagingTimeout documentation and installed AXUIElement.h.

## Exact recovery boundary

The limit is one extra native copy per focused-element read invocation. A
transaction contains several separate focus observations; each remains bounded
by the original shared operation deadline. The second copy is attempted only
with at least one unchanged 50 ms call allowance remaining, after a fresh
permission/timeout check. Both copies have post-read deadline checks. No sleep,
global timeout setting, missing-element substitution, or optional-attribute
absence rule is added. All foreground/process checks and duplicate focused
object comparisons still execute in the existing caller.

Failed second reads report their status, presence and timing plus the initial
failure. A successful second read emits scalar diagnostic evidence on helper
stderr, never an object, field value, label, PID or wire capability. Success
means a usable observation, not a focus lock or verified input effect.

Hermetic tests inject native statuses and monotonic checkpoints: one-read
success, documented absence, all known/unknown errors, error-plus-value,
wrong-type success, exact object preservation, persistent failure, permission
loss before reread, insufficient remaining budget, and deadline expiry after
either copy. They create only AX reference objects and never read attributes
from a running application or post input. Existing focus and receiver tests
remain the end-to-end policy checks; live evidence is recorded separately.

The ordinary broker currently discards helper stderr. The success diagnostic
is therefore visible only when a caller explicitly captures that stream; it
is not public proof that a particular successful HTTP operation used recovery.
Native acceptance and injected-status unit tests are reported separately.
