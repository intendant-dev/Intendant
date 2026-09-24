# macOS AX window-enumeration diagnostics (2026-09-24)

Diagnostic-only follow-up to the #956 native ArrowRight refusal. Preserve the native AXWindows bounded-enumeration status, whether a CF array was returned, returned count when available, and monotonic call/budget timing. Acceptance, candidate cap, exact window mapping, 50 ms per-object timeout, four-second operation budget, focus/protection checks, and input behavior remain unchanged. No retry is added.
