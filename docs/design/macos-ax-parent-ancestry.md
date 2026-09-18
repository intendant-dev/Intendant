# Chromium exact-parent ancestry investigation

Follow-up to merged #938. Investigate the AXWebArea parent mismatch in the disposable Chromium fixture, retaining exact object/window identity, protected ancestry checks, bounded traversal, and one-use tokens.

No global input fallback, user-browser/profile access, activation, permission changes, or daemon deployment. Native evidence and regression results will be recorded before landing.
