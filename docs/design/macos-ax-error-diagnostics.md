# Bound-window AX error diagnostics (2026-09-23)

Keep native AX status names/numbers and reply presence in existing bound-window
refusals. Diagnostic-only: no new calls, retries, permission changes, focus
mutation, event dispatch, or relaxed protected-content checks.

This is independent of the ArrowLeft feature in #949. The generic plugin
transport is not a brake subsystem; the isolated controller returns ordinary
validation errors before dispatch. Native errors are evidence, not attribution
to Chromium, macOS, or the human user without further investigation.
