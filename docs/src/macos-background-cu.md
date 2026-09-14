# macOS background computer use (experimental)

Background CU addresses a native application window without deliberately moving
or restoring the system pointer, activating an application, raising a window, or
using the system clipboard as a typing transport. This is an opt-in controller
path, not a VM, second login session, virtual display, or security sandbox.

**Status:** implementation under review. Native app acceptance, Chromium/canvas
compatibility, and simultaneous human/agent operation need explicit GUI fixture
validation before production promotion. A dispatched event is not proof of an
application effect. Existing foreground CU is unchanged.

## MCP controller facade

Use the controller's existing `inspect` and `act` tools. The installed daemon must
be built from a version containing this feature; a source checkout alone does
not upgrade the running service.

Discover targets (requires the same user-display authority as screen reading):

```json
{"argv":["cu","windows"]}
```

Each result contains an opaque `target` such as
`macos_window:4812:723:1789370000:123456`. Use the returned value rather than
constructing one: it includes PID, CGWindowID and process start time, so an app
restart invalidates old selectors. These are user-session resources, not agent
virtual-display IDs.

Read only the selected window's accessibility tree:

```json
{"argv":["cu","elements","--target","<returned target>","--format","json"]}
```

Execute through `act` (coordinates below are examples, not discovered controls):

```json
{"argv":["cu","actions","--target","<returned target>","--observe","auto","[{\"type\":\"click\",\"x\":120,\"y\":90},{\"type\":\"type\",\"text\":\"Hello\"}]"]}
```

The typed MCP tools use the same selectors: `read_screen`, `take_screenshot`,
and `execute_cu_actions`, through their existing `display_target` parameter.
`read_screen` with `display_target="macos_windows"` is the discovery operation.
No new authority-bearing tool or approval bypass is introduced. Window discovery
and capture retain `display.view` classification; actions retain the existing
CU input classification and caller checks. An owner-surface invocation is itself
an opt-in under the existing trust policy; scoped callers still need the
user-display grant. OS Accessibility and Screen Recording permissions remain
independent requirements.

## Coordinates and observations

Pixels and AX frames use **window-local logical points**, with the selected
window's top-left as `(0,0)`. The PNG is resized to those logical dimensions so
Retina pixels do not introduce a factor-of-two mismatch. `normalized_1000` maps
the inclusive 0–1000 grid to the window's addressable extent. Out-of-window
coordinates fail; they are not clamped into another surface.

`observe=pixels` captures the selected window; `ax` reads its exact AX root;
`auto` prefers that tree and falls back only to that window's pixels; `none`
returns dispatch results. An explicit screenshot action returns pixels. Earlier
screenshots are discarded when followed by another action, rather than being
presented as evidence of a later effect. Moving/resizing a target during a batch
invalidates the batch and requires a new capture.

## Native implementation

- ScreenCaptureKit provides window-filtered capture. The background capture
  backend has its cursor overlay disabled and rejects input through its normal
  HID-oriented display interface.
- Accessibility provides exact-window observation and semantic `AXPress` when
  available. Exact AX-to-CGWindowID matching uses the dynamically resolved private
  `_AXUIElementGetWindow` SPI. Missing mapping fails closed; titles, geometry and
  enumeration order are never used as identity substitutes.
- Other supported actions use private-state CoreGraphics events posted to the
  target PID, not the global HID event tap. Raw events additionally require that
  the app's own AX-focused window is the selected window. The implementation does
  not change that internal focus for the caller.
- App/process generation and geometry are checked repeatedly. Agent batches are
  serialized per process, because two windows of an app share its event queue.
  Input is refused when the target app is the human's foreground app.

The normal success ceiling is **`injected`**, not `ok`: dispatch succeeded, but
application acceptance/effect is unverified. An AX transport error can occur after
an action was delivered; it therefore stops the batch rather than causing a
second click through a different input mechanism.

## Deliberate limits

This is not complete vendor parity. Process-local mouse events are not accepted
uniformly by all apps, especially Chromium renderers, custom canvases and games.
This implementation does not use SkyLight event posting or private
activation-without-raise tricks, and never silently falls back to foreground
control. AX trees can be sparse or unavailable when apps suppress background
accessibility. Hidden/minimized/off-Space capture depends on what ScreenCaptureKit
actually exposes; a missing or timed-out window does not trigger desktop capture.

The foreground-app check is a best-effort collision fence, not a macOS-wide lock.
A user focus change can race an event, and an app may raise its own UI in response
to an action. Working on different apps is the intended model, not simultaneous
editing of the same application. This is not an isolated clipboard/filesystem/
network environment: app actions and keyboard shortcuts can still affect shared
application or OS state even though Intendant does not use clipboard transport.

`paste`, split `mouse_down`/`mouse_up`, `hold_key`, `zoom`, screenshot annotation,
and visual-quiescence settling are rejected in this initial path. Batches have at
most 32 actions, 4096 typed UTF-8 bytes and 5000 ms of explicit waits. Observation
uses a 150 ms freshness delay, not a claim that the UI has settled. Dispatch has a
30-second budget checked between actions and typed characters; blocking native
IPC can delay the next check. Non-macOS platforms return an unsupported error.

Do not pass these selectors to unrelated display/stream management APIs. They
are currently defined for the three controller CU tools above, not the internal
provider-side numeric-display protocol.

## Validation

`python3 scripts/check-macos-background-cu.py` compiles the production AX, native
input and target-validation modules in an isolated small Cargo harness and runs
hermetic tests. Capture transport and surrounding daemon infrastructure are
stubbed there; this is not a full daemon build or a desktop acceptance test.
The normal cross-platform CI suite compiles the integrated daemon and runs its
keyless tests. Neither test suite establishes real-app compatibility.

Before promotion, use test-owned fixture apps through authorized CU tools and
verify the user's cursor position, foreground app, key focus and clipboard are
unchanged while the selected background window receives each intended action.
Include multi-window apps, Retina scaling, an occluded window, user focus changes,
window closure/PID reuse, Chromium and AX-poor canvases. Do not use a person's
live documents as test fixtures.

## Research basis

Product behavior is public; the exact proprietary implementations are not:

- [OpenAI: Codex for (almost) everything, April 16, 2026](https://openai.com/index/codex-for-almost-everything/)
- [Anthropic: Let Claude use your computer in Cowork](https://support.claude.com/en/articles/14128542-let-claude-use-your-computer-in-cowork)
- [Apple: CGEvent.postToPid](https://developer.apple.com/documentation/coregraphics/cgevent/posttopid(_:))
- [Apple: AXUIElementPerformAction](https://developer.apple.com/documentation/applicationservices/1462091-axuielementperformaction)
- [Cua: Inside macOS window internals](https://github.com/trycua/cua/blob/main/blog/inside-macos-window-internals.md)

Cua's independent write-up explains why generic process posting is not enough for
some Chromium mouse paths and discusses private SkyLight alternatives. It is not
evidence that OpenAI or Anthropic use those exact internals. This feature uses
original Intendant implementation code rather than either vendor's proprietary
code.
