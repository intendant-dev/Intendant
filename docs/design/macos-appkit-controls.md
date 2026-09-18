# Standard AppKit control compatibility

Follow-up to #933. Replace fixture-only assumptions about optional AX subroles with documented optional-attribute decoding: distinguish absence from failed IPC or malformed values. Continue excluding explicit secure/password roles and subroles, and additionally exclude AXContainsProtectedContent subtrees before labels, children or values. Exact retained identity, membership, current permissions, geometry, focus and single-use tokens remain required.

Add native acceptance with unmodified NSTextField, NSButton, NSSecureTextField and ordinary AppKit hierarchy, without synthetic subroles or parent/children overrides. Record baseline failures before changing the adapter. No raw input, activation, clipboard, permission changes or live-daemon installation.

Native acceptance passed for both profiles below; repository validation and review status are recorded with the PR.

## Reproduced baseline

On the unchanged #933 head e2354b2, a fixture compiled with
`-DINTENDANT_STANDARD_APPKIT` refused `required AX attribute AXSubrole unavailable`.
The standard profile uses native NSTextField, NSButton, NSSecureTextField and
NSView classes, plus a nonactivating panel whose only overrides prevent becoming
key/main. No AX subrole, parent or children override is used in this profile.

## Contract and sources

The installed Apple SDK's AXAttributeConstants.h documents AXSubrole as required
only when the role alone is insufficient. NSAccessibilityProtocols.h declares
accessibilitySubrole nullable and accessibilityProtectedContent as the Boolean
for NSAccessibilityContainsProtectedContentAttribute. The native adapter admits
only documented absent values, not errors; explicit secure/password metadata or
true AXContainsProtectedContent excludes the entire subtree. This is reported
application metadata, not an isolation boundary or a secret detector.

Apple reference: https://developer.apple.com/documentation/appkit/nsaccessibilityprotocol/accessibilitysubrole()
Apple reference: https://developer.apple.com/documentation/appkit/nsaccessibilityprotocol/isaccessibilityprotectedcontent()
Apple hierarchy reference: https://developer.apple.com/library/archive/documentation/Accessibility/Conceptual/AccessibilityMacOSX/OSXAXmodel.html

The unchanged control engine checks safety again before and after action dispatch
and before reading text back. The native fixture includes secure and disabled
controls and an explicitly protected container. Existing strict limits, owner/IAM,
retained membership and no-retry/focus rules remain unchanged. Native results are recorded below.

## Native validation, 2026-09-18

The standard AppKit profile passed against the new adapter on macOS 26.4.1 arm64. The normal field reported no subrole. Exact Unicode text, independently observed one-time button action, refresh/replay/replacement/unbind refusal, monitor capture/recovery and cleanup passed.

Protected-content validation made a synthetic subtree visible while explicitly unprotected, retained its element token, re-protected the container, and verified that the old token was refused before any action. A fresh inventory again excluded the whole subtree.

The original explicit-subrole controls profile also passed. A separate combined run stopped in the older placement fixture with an application-window availability refusal; that run remains a failure, not a controls pass. All runs restored display inventory and cleaned up their fixtures.

```sh
clang -DINTENDANT_STANDARD_APPKIT -fobjc-arc -framework AppKit -framework CoreGraphics tests/fixtures/macos-monitor/controls.m -o /tmp/intendant-standard-appkit-fixture
python3 scripts/verify-macos-monitor-http.py --bin /absolute/built/intendant --fixture /tmp/intendant-monitor-pattern --controls-fixture /tmp/intendant-standard-appkit-fixture --report /tmp/standard-appkit-proof.json --allow-shared-session-monitor --check-recovery
```

Without the compilation define, the same source retains the original explicit-metadata fixture profile. It uses the same acceptance harness. Neither profile establishes arbitrary app/Chromium/canvas compatibility or independent focus/clipboard isolation.

## Local regression validation

Passed: 6,550 binary tests (10 existing ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. All compiler-governor settings were left intact. The AX adapter subset passed eight tests with one existing GUI-only ignore. No ignored native test was counted as executed.
