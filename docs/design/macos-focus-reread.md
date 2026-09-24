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
