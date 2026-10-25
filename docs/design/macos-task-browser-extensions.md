# Task-owned offscreen browser extensions

Status: implementation candidate, not installed or merged. Third-party application
compatibility evidence is recorded separately from the generic fixture.

## Why this backend is separate

Extensions may request browser focus for their own windows. Loading it into a
`CGVirtualDisplay` window would not establish freedom from desktop interruption.
An extension-enabled task instead owns a fresh **headless Chrome for Testing**
process. Unified Chrome headless renders extension pages and popup windows without
showing platform windows. This is not a virtual monitor, VM, separate OS seat or
security sandbox for arbitrary native applications.

No-extension tasks retain the accepted macos_virtual implementation. Native
macos_virtual extension launch remains refused; there is no headful fallback.
The extension backend uses no native capture, global mouse/keyboard, cursor warp,
clipboard or focus restoration. Its logical tab activation is confined to its
retained, verified headless browser. Browser capability responses name
`headless_extension` and `page_css_pixels`; `window_on_monitor` is null.

## Actual extension UI, not a popup.html imitation

The task owns an original website page, its uniquely related tab wrapper/window,
one approved extension runtime identity, and bounded opaque extension view handles.
`extension_popup` invokes Chrome's `Extensions.triggerAction` on that retained tab.
It opens the real toolbar action and wakes a sleeping MV3 worker through Chrome;
it does not invoke wallet controller methods or arbitrary JavaScript. The related
page/tab/browser-context/window identities are validated before dispatch.

`extension_views` observes current page targets belonging only to the approved
extension. This includes notification windows created by the extension itself.
A caller receives opaque view IDs and resource paths, never debugging endpoints,
Chrome target IDs, native handles, or query-string contents. Dead views disappear;
foreign pages/extension origins refuse. `extension_page` is a separate explicit
resource-opening operation, honestly described as an ordinary extension tab.

Screenshots and coordinate click/scroll operate on the selected view's actual
renderer; keyboard reuses the existing validated receiver and paired key-release
implementation. Navigation stays on the original page and verifies frame/loader
commit, separately from independently observed site effects. Page acknowledgement
alone never certifies an application decision or wallet approval.

## Automatic session access and one-time extension approval

The ordinary session retains the existing authenticated principal, credential
epoch/incarnation, current IAM, bounded allocation, single-use request IDs,
cancellation and task-stop cleanup. No owner promotion or user-display grant.
One key pair's release is retained after dispatch; uncertain input is not replayed.

The existing immutable startup extension policy approves exact archive SHA-256,
byte count, MV3 version, extension version and service-worker entrypoint. An agent's
request merely selects those already approved bytes; it cannot approve a download.
This candidate deliberately does not replace that rule with caller-provided hashes,
mutable project settings or environment-derived authority. Deployment therefore
needs the trusted daemon launch to carry its approved policy path and exact pin.
It does not introduce per-key/per-click approvals or timed renewal prompts.

A session starts with `task_browser` open and an `extension` tuple. Prefer opening
about:blank, then navigating to the target website after extension startup; an
extension's asynchronous content-script registration is not a document-ready
signal for a website already loading. The generic fixture explicitly navigates before checking its content script.

```
act browser task-open about:blank --extension '<approved archive tuple JSON>'
act browser task-navigate WORKSPACE REQUEST_ID https://example.test/
act browser task-extension-popup WORKSPACE REQUEST_ID
inspect browser task-extension-views WORKSPACE
inspect browser task-screenshot WORKSPACE --view VIEW
act browser task-keyboard WORKSPACE REQUEST_ID '{"type":"key","key":"Tab"}' --view VIEW
act browser task-click WORKSPACE REQUEST_ID X Y --view VIEW
act browser task-scroll WORKSPACE REQUEST_ID X Y DELTA_Y --view VIEW
act browser task-close WORKSPACE
```

## Wallet boundary and test scope

**Profiles are temporary and are destroyed on task stop. Do not create/import a
real funded wallet or its seed into them.** This feature does not supply safe
persistent wallet provisioning, unlock a wallet, import user credentials, authorize
a financial action, or establish hardware-wallet/native-messaging compatibility.
Password/protected receiver checks remain unchanged. No personal Chrome profile,
keychain, seed/private key, wallet storage reset or internal approval bypass is used.

`verify-headless-extension.py` creates only the deterministic repository fixture.
It verifies real toolbar context, renderer screenshots, Unicode typing/editing
and worker-side effects, a focused extension-created notification, duplicate-request
and foreign-view/session refusals, and cleanup. No third-party package, download
URL, version pin or application policy is embedded in the generic harness.
Application compatibility checks belong to the operator's own external workflow;
they cannot modify the daemon's extension approval policy.

The fixture uses a real supervised backend's own injected credential, not owner
input. The observer reads DOM/state to independently verify fixed test effects;
this is not an autonomous visual-reasoning benchmark. Foreground before/after
snapshots are endpoint observations only; physical simultaneous-human typing is
not claimed. Each run has its own daemon/HOME/profile and reports actual cleanup.

## Next product acceptance

Any real wallet job still needs its wallet mode specified and safe
owner-controlled wallet provisioning. A passing empty-wallet onboarding test must
not be sold as a complete signing workflow. Arbitrary native-app input remains a
separate core project; this headless browser feature does not replace that goal.
