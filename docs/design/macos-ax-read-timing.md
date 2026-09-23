# Bound-window AX read timing (2026-09-23)

Follow-up to #950: retain elapsed native CopyAttributeValue time, configured
per-call timeout, and remaining operation budget on failed bound-window reads.
Diagnostic-only. No retries, changed AX acceptance, longer timeouts, extra AX
reads, keyboard dispatch, focus mutation, or logging of returned values.

The combined #949 acceptance remains independent. Timing is evidence for the
50 ms timeout hypothesis, not proof that a particular process is responsible.
A controlled read-only timeout comparison is separate from input acceptance.
The existing human-focus guard remains unchanged pending concurrent-use design
review; before/after samples do not establish continuous desktop isolation.
