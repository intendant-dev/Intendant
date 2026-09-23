# Bound-window AX read timing (2026-09-23)

Follow-up to #950: preserve elapsed native CopyAttributeValue time, configured
per-call timeout, and remaining operation budget in failed bound-window reads.
This is diagnostic-only. No retries, changed AX acceptance, longer timeouts,
extra AX reads, input dispatch, focus mutation, or returned-value logging.

## Scope and interpretation

The optional, required and strict bound-window attribute readers still use one
native copy. Monotonic time is sampled immediately before and after that call;
the duration includes scheduling delays and is not a kernel-side timeout trace.
The existing native status, value-presence flag and failure context are retained.
Successful and documented-absent replies are unchanged and are not logged.

Errors append integer microseconds: `ax_copy_us`, `ax_timeout_us`,
`budget_before_us` and `budget_after_us`. Remaining budgets saturate at zero.
`ax_timeout_us=50000` is the unchanged configured timeout, single-sourced with
the value passed to AXUIElementSetMessagingTimeout; a test pins the old float.
This is not a new timing-based acceptance rule and does not extend the existing
four-second operation budget. The optional/strict readers keep their existing
post-copy deadline checkpoint; a deadline refusal also preserves native status.
The required reader gains no new acceptance checkpoint in this diagnostic patch.

The instrumentation covers failures in these copy/result wrappers, not every
AX operation, successful-call latency, or later metadata-type/protection errors.
The system-wide focused-element copy is not modified. Its application fallback
uses the covered bound readers for attributes such as AXFrontmost. Existing
outer error context identifies the traversal/attribute stage when available.
No field value, title, document text, AX address, extra PID or focus identity is
added to the diagnostic. There is no telemetry stream or new public tool.

A CannotComplete near 50 ms supports investigating the configured timeout, but
does not prove which component caused the delay or that a longer wait is safe.
A controlled read-only timeout comparison remains separate from input acceptance;
no such comparison result is claimed merely from implementing this diagnostic.

## Safety and regression coverage

The required/optional result matrices are unchanged: unsupported/no-value plus
no object is absent only for optional reads; every other failed or contradictory
reply is refused. Strict receiver copies retain their type check. Non-null
Copy-rule objects are still released on failure. No retry, replacement receiver,
activation, hidden selection click or keyboard posting is added.

Hermetic tests cover exact elapsed/budget formatting, exhausted/expired budget
saturation, unchanged timeout bits, the native-status/presence matrix, preserved
successful payload identity, and absence of synthetic private text in errors.
Native acceptance/evidence and CI status are recorded separately on #951.

## Relationship to #949 and concurrent use

The combined #949 revision 942a26f6 includes #950 but not this independent change.
Its fixed L,R,R,L native set yielded one full ArrowRight pass and three pre-key
refusals; both ArrowLeft runs refused. That is not a passing acceptance set.
Adding timing diagnostics does not satisfy its missing same-head ArrowLeft gate.

The existing exact human-focus equality guard is unchanged. It can refuse when
the human's focused object changes even if the background receiver is stable.
Before changing that invariant, a separate controlled concurrent-use experiment
must distinguish independent human activity from agent-caused activation or
misdelivery. Before/after desktop samples do not prove continuous isolation, and
AX/CG observations do not make an atomic lock on OS input routing.

Primary references: Apple AXUIElementCopyAttributeValue documentation and the
installed ApplicationServices/HIServices AXUIElement.h and AXError.h declarations.
