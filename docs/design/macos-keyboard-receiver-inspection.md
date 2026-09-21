# Bound-window keyboard receiver inspection

Follow-up to merged #945. Add a read-only owner-scoped inspection of the actual application-local focused AX receiver inside an exact retained window on an owned monitor. Do not add key posting, activation, implicit clicks, input capabilities or reusable authority tokens. Validate receiver identity, ancestry, protected content, window/monitor geometry and before/after focus under existing bounds. Native acceptance must use disposable fixtures only.
