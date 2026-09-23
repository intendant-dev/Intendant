# Bound-window AX error diagnostics (2026-09-23)

Keep native AX status names/numbers and reply presence in existing bound-window
refusals. Diagnostic-only: no new calls, retries, permission changes, focus
mutation, event dispatch, or relaxed protected-content checks.

This is independent of the ArrowLeft feature in #949. The generic plugin
transport is not a brake subsystem; the isolated controller returns ordinary
validation errors before dispatch. Native errors are evidence, not attribution
to Chromium, macOS, or the human user without further investigation.

## Scope

The diagnostic lives in `src/bin/caller/ax.rs`, not in the ChatGPT plugin
transport or test harness. The controller/helper is the source of the refusal.
An error now keeps its existing context plus the AXError symbol, signed integer,
and `value_present` flag. Unknown statuses retain their numeric value rather
than being guessed. No AX field value, label, document text, pointer address or
extra PID is logged.

Covered paths: optional metadata (`AXSubrole`, `AXContainsProtectedContent`),
required bound-window attributes such as `AXFrontmost`, strict focused-element
copies, and rejected system-focus replies. A shared one-call Copy wrapper owns
non-null results even on error; in particular, the required bound path no longer
loses an anomalous error-plus-value result in the legacy Option-only reader.
The legacy general AX tree reader is not changed by this patch.

All existing acceptance decisions remain the same. Optional unsupported/no-value
replies without a value remain absent; failed or contradictory replies remain
errors. Required values remain required, and strict receiver copies remain
AX-element-typed. The existing system-focus fallback accepts only an empty
`kAXErrorCannotComplete` response. No extra AX read, native retry, event posting,
permission request, timeout increase or changed focus checkpoint is introduced.

## Interpreting an error

Example format (illustration, not a measured result):

```
AX AXSubrole read failed; metadata is not known; optional AX metadata unavailable or contradictory; kAXErrorCannotComplete (-25204); value_present=false
```

The symbols/numbers are taken from the installed Apple SDK `AXError.h` and
accessibility-sys constants. `kAXErrorCannotComplete` alone does not identify
which process caused a messaging failure or whether a configured timeout,
application workload or another condition was responsible. A success status with
no returned object is a different contradictory reply; it is not relabeled as
an OS error. A present object does not imply the expected type.

Hermetic macOS unit tests cover every SDK error, unknown numeric statuses,
required/optional acceptance matrices, strict wrong-type replies, preserved
system-focus fallback, and absence of synthetic field content from diagnostics.
The existing protection/identity/native event tests are not weakened.

Native evidence and CI status are recorded on PR #950. This diagnostic patch
must not be described as proof that old failed runs returned a particular status:
those runs discarded that information. No read-only retry strategy is added here.
