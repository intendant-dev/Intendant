# Standard AppKit control compatibility

Follow-up to #933. Replace fixture-only assumptions about optional AX subroles with documented optional-attribute decoding: distinguish absence from failed IPC or malformed values. Continue excluding explicit secure/password roles and subroles, and additionally exclude AXContainsProtectedContent subtrees before labels, children or values. Exact retained identity, membership, current permissions, geometry, focus and single-use tokens remain required.

Add native acceptance with unmodified NSTextField, NSButton, NSSecureTextField and ordinary AppKit hierarchy, without synthetic subroles or parent/children overrides. Record baseline failures before changing the adapter. No raw input, activation, clipboard, permission changes or live-daemon installation.

Validation is pending. This draft is a work-in-progress, not a compatibility claim.
