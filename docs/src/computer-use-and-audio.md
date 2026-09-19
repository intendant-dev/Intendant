# Computer Use & Live Audio

This page covers three related capabilities: the provider-agnostic **computer
use** (CU) abstraction that lets a model see and drive a desktop, the
**live audio** system that connects an untrusted voice model to an application
through a virtual audio bridge, and the **phone-call** / **voice-call-app**
skills built on top of live audio.

For the WebRTC capture/encode/stream stack that puts pixels on screen for a
human viewer, see [Display Pipeline](./display-pipeline.md). This page is about
the *model* seeing and acting, not the human-facing video transport.

## Computer Use

The CU abstraction (`src/bin/caller/computer_use.rs`) gives any provider a
common action set and dispatches it through a platform backend. Provider-
specific parsing of CU tool calls (OpenAI computer-use, Anthropic
`computer_20251124`, Gemini) lives in the `provider/` per-provider modules
(`openai.rs`, `anthropic.rs`, `gemini.rs`); the executor here is
provider-neutral. Anthropic `wait`/`hold_key` durations arrive in **seconds**
and are converted to milliseconds (clamped to 30 s) at parse time.

The full `CuAction` vocabulary (the same tagged JSON accepted by the MCP
`execute_cu_actions` tool and `intendant ctl cu actions`):
`click`, `double_click`, `triple_click`, `mouse_down`, `mouse_up`, `type`,
`paste` (clipboard + paste chord — fast for long text; previous clipboard
text is restored where the platform allows, see "Action results" below),
`key`, `hold_key`, `scroll`,
`move_mouse`, `drag`, `screenshot`, `zoom` (region capture at the highest
resolution the platform can supply — native 2x pixels on Retina), and `wait`.
`intendant ctl cu actions --help` prints the per-action field shapes with an
example.

### Backends

`DisplayBackend` (in `computer_use.rs`) is detected at runtime by
`DisplayBackend::detect()` — macOS → `MacOS`; Windows → `Windows`; otherwise
`WAYLAND_DISPLAY` set → `Wayland`; else `X11`. It can be forced via the
`backend` config value.

| Backend | Screenshot capture                          | Input injection            | Platform        |
|---------|---------------------------------------------|----------------------------|-----------------|
| X11     | live session frame, else in-process root-window capture via `x11rb` | `x11rb`/XTest | Linux (X11) |
| Wayland | live session frame (PipeWire portal)        | portal `InputEvent`s       | Linux (Wayland) |
| MacOS   | live session frame, else `screencapture`    | in-process CGEvents        | macOS           |
| Windows | live Windows display-session frame          | `SendInput` via session    | Windows         |

Wayland and Windows are **session-only** backends: both capture and input run
through the live `DisplaySession` (WebRTC pipeline); without an active session
the executor returns an actionable error naming the recovery path. X11 and
macOS inject input directly — X11 uses a persistent `x11rb`/XTest connection
with in-process root-window capture and clipboard support; macOS posts
CoreGraphics `CGEvent`s in-process (no `cliclick`/`osascript` subprocesses; key
chords and typed ASCII use ANSI-US virtual keycodes, characters with no
ANSI-US key ride paced unicode-string events, and typed text is read back via
AX where possible — see "Action results" below) — and prefer the in-memory
frame of a live capture session for screenshots, falling back to their
platform capture paths when no session exists.

**Post-action freshness:** capture backends are damage-driven, so a screenshot
taken right after an input action waits (bounded, 300 ms) for a frame captured
*after* the action before falling back to the freshest available frame — the
model never sees pre-click pixels after a click that changed the screen.

Coordinates from the model are in the provider's logical screenshot space and
are scaled to the backend's actual pixel/point space before dispatch (important
on HiDPI and under the Wayland portal, which reports its own stream size).
`zoom` is the deliberate exception: on macOS it captures raw physical pixels
(2x on Retina) and crops the requested logical region, because detail is the
point.

### Action results: dispatch vs. effect

OS input APIs only confirm that events were *dispatched* — not that the
target app acted on them — so every action result carries an honest status
(`CuActionStatus`) instead of a bare boolean:

- **`ok`** — the intended effect was verified: screenshots and zooms (the
  captured bytes are the effect), `wait`, and `type` on the macOS user
  session when the focused element's AX value was read back and contained
  the typed text.
- **`injected`** — events were dispatched to the OS but the effect was not
  verified. This is the honest ceiling for clicks, keys, scrolls, drags, and
  typing on backends without read-back; verify from the post-action
  screenshot. A dispatched-but-ineffective action (a click that only
  activated an inactive window, a chord swallowed by the wrong app) reports
  `injected`, never `ok`.
- **`failed`** — dispatch failed, or verification contradicted the intent: a
  macOS `type` whose read-back does not contain the typed text returns
  `failed` with expected-vs-observed evidence rather than pretending
  success.

Type read-back is bounded and best-effort (two AX reads of the focused
element): multi-line/control text (Return may submit, Tab may move focus),
secure fields, and elements without an AX-readable value stay `injected`
with the reason in the result detail.

### Externally driven CU sessions (no internal model)

Ordinary `ctl cu actions` never needs a model provider. For evidence work,
`ctl cu session --request JSON|@file|-` adds an owner-bound, multi-call recording
session without delegating to another model. The current external agent sends
explicit actions and receives private PNG observations. No provider selection
or LLM request exists in this path; `cu task` below remains an optional
model-backed orchestration convenience.

The request is bounded, duplicate-key-safe JSON. `begin` pins the job digest,
attempt, ready local CDP workspace, leased display and exact display generation.
The daemon records the actor resolved by the authenticating control surface;
request fields cannot substitute that identity. Continuations require the same
actor, opaque proof ID and exact sequence. `status` reconciles a lost response;
replayed or out-of-order mutations do not repeat input.

The phase sequence is `begin -> actions -> freeze -> observe -> finish -> close`.
The worker owns the display lock across all calls. Stage is limited to 180
seconds, 64 total actions and 16 actions per batch. Clipboard paste, split
mouse edges and cropped frames are forbidden. Freeze permanently disables
further actions and starts a 45-second completion deadline. Observe binds one
full-display PNG to the caller's pre-capture observation digest. Finish records
the external caller's claims about exactly that image; it does not independently
validate those claims or authorize an application decision. The lock remains held until
close, so a caller can verify post-capture browser state before releasing it.

Every native operation uses the same generation, lease, input-sealing, private
scratch and cancellation-safe executor as the bounded task. Disconnecting an
HTTP call does not discard the worker's fence while OS input is in flight;
unattended sessions expire. Close is acknowledged only after executor cleanup.
The owner-side caller still closes its browser, destroys its exact display
generation and removes its private profile.

The separate v2 external receipt identifies `external-actions`, reports zero
internal model calls, and binds the authenticated actor, action/frame transcript,
external claim bytes and frozen PNG. Typed text and key values (including their
hashes) are excluded from the public action projection. Receipt hashes provide
content integrity, not independent authentication or policy approval: consumers
must retain their pinned authenticated transport and require the matching close
acknowledgement. They must not relabel this receipt as a legacy model attestation.

`python3 scripts/test-external-cu-session.py --bin /absolute/intendant --evidence
/private/new-directory` exercises real native input, PNG changes, replay and
freeze refusal, exact closure, idle expiry, and foreign-Xvfb/CI isolation on Linux.
It starts a separate credential-free loopback daemon and never uses the user's
physical display.

See [Externally driven CU sessions](./external-cu-session.md) for the complete provider-free protocol.

### Bounded CU tasks

`intendant ctl cu task` is the owner-only orchestration lane for evidence
capture on an exact daemon-created virtual-display generation and exact
attempt-leased local CDP browser workspace. It is deliberately smaller than
an agent session: the selected provider receives native CU plus no function
tools, shell, filesystem, browser API, delegation, or escalation capability.
`stage` has fixed turn/action/time limits and may operate the isolated display;
`attest` receives one internally captured PNG and refuses every later action.
The daemon rechecks the exact display generation and browser lease around each
action, serializes tasks per display generation, and deletes its mode-0700
scratch directory before returning.

The 180-second `stage` and 45-second `attest` deadlines begin at request
admission, including any wait for exclusive display control and all setup. A
ready workspace is also probed for a live daemon-owned browser child and its
exact CDP page target around every action. The initial model frame must have
been captured after browser-originated input was permanently sealed. Text,
scroll, hold, and wait parameters carry fixed per-action caps; if a platform
operation has already entered non-abortable OS work when the deadline fires,
it retains the exclusive display and private scratch lifetimes until that
bounded work finishes.

Successful calls return a compact receipt rather than screenshots or typed
text. Its identity binds the exact resources, provider/model, timestamps and
monotonic elapsed time, task and normalized-result hashes, every model-visible
frame hash, action kinds, redacted geometry/timing projection hashes and
counters, plus the prior stage and observation lineage required by `attest`.
Typed text and key values are excluded from the public projection so the
receipt cannot act as an offline oracle for low-entropy credentials. Provider
response text, hashes, and exact lengths are excluded for the same reason; its
public event records only whether content existed and the CU/tool-call counts.
Scoped agent callers are rejected; the intended caller is a local owner-side
orchestrator using the loopback `intendant ctl` lane.

**Typing on macOS**: ASCII is delivered as real ANSI-US keycode events (the
same proven event shape as `key`), newlines as Return and tabs as Tab;
characters with no ANSI-US key are delivered as paced unicode-string events
(≤ 20 UTF-16 units per event, payload on both keyDown and keyUp, surrogate
pairs never split). This replaces the 2026-07-13 failure mode where
back-to-back `CGEventKeyboardSetUnicodeString` chunks were silently dropped
by the focused app while the action still reported success.

**Clipboard hygiene (`paste`)**: paste routes through the system clipboard,
so each backend restores what it can and reports what it did in the result
detail. macOS captures the previous *text* clipboard (`pbpaste`) and
restores it ~300 ms after the chord — non-text content (images) cannot be
captured, so the clipboard is cleared rather than left holding the paste
payload; Windows does the same via arboard. X11 and Wayland selections are
pull-based (the target may fetch the payload after the chord returns), so
restore would race the paste itself: the pasted text deliberately remains
the active selection and the result detail says so. The restore delay is an
honest race, not a guarantee — an app that lazily re-reads the clipboard
later sees the restored content.

### Screen-capture permissions & signing identity (macOS)

macOS TCC keys every permission grant (Screen Recording, Accessibility,
Apple Events, Microphone, Camera) to the app's **code-signing designated
requirement** as it was at grant time. If the binary is rebuilt or re-signed
under a different identity — or the signing *identifier* changes — every
existing grant silently stops validating: System Settings keeps showing the
toggle ON while `SCShareableContent::get` fails with "The user declined
TCCs…". The macOS capture backend classifies that failure and appends the
recovery path to the error (permission missing **or** invalidated by a
re-signed build; toggle it off/on under System Settings → Privacy &
Security → Screen & System Audio Recording, then relaunch — grants are only
re-read at launch), and requests screen-capture access once per process so
a fresh install gets the native prompt. The CU executor's raw
`screencapture` fallback gets the same treatment
(`cu_readiness::enrich_capture_failure`): when the capture fails **and**
`CGPreflightScreenCaptureAccess` confirms the permission is missing, the
error names Screen Recording, the affected binary path, and the System
Settings destination instead of the bare "could not create image from
display".

`scripts/bundle-macos.sh` defends the identity's stability:

- The local-dev signing identity ("Intendant Dev", in
  `~/.intendant/signing.keychain-db`) is **escrowed** to
  `~/.intendant/signing-identity.p12` (chmod 600) when created, and
  backfilled from the keychain for identities that predate the escrow. If
  the keychain is ever lost, the script re-imports that file — same cert,
  same designated requirement, existing grants keep validating — instead of
  silently minting a fresh identity that voids them all.
- When a genuinely new identity must be minted (keychain *and* escrow both
  gone), the script prints a loud re-grant warning listing the System
  Settings steps.
- After every signing run it diffs the bundle's designated requirement
  against `~/.intendant/last-signed-dr.txt` and prints the same warning on
  any drift (this also catches signing-identifier changes and dev-cert ↔
  Developer ID switches), then refreshes the cache.

### Element Observation (`read_screen`)

`read_screen` is the cheap textual grounding path: a filtered accessibility
tree of the frontmost application's focused window — roles, labels, values,
and logical-point frames, plus a one-line summary of other visible windows —
typically a few hundred tokens versus ~1.5k for a screenshot. Clicks ground
deterministically: click the center of a reported frame. Depth/node caps are
always announced in the output, never silent.

Implemented for the user session on all supported platforms: macOS AX
(`src/bin/caller/ax.rs`, raw `accessibility-sys` bindings — the documented Unix
`unsafe` exception), Linux AT-SPI (`src/bin/caller/atspi_read.rs`), and Windows
UIA (`src/bin/caller/windows_uia.rs`). It inspects the user's session, not
virtual display pixels, and requires the platform accessibility path to be
available. Exposed as an MCP tool (in the `core` bootstrap profile) and as
`intendant ctl cu elements [--format json]`. Pixels remain the fallback for
visual verification and for apps with sparse accessibility trees (web views
notably).

Long element values and window titles (a `data:` URL, a whole document in a
text view) are length-capped **once, centrally**
(`computer_use::cap_screen_elements_texts`, 80 chars) with a stable marker —
`prefix… [N chars total, #hash8]` — so one long value cannot dominate the
observation while staying distinguishable from other long values sharing its
prefix. The platform readers deliberately do not pre-truncate; the
`full_values: true` tool param (`ctl cu elements --full-values`) skips the
cap for exact-value requests (Linux AT-SPI still bounds each text fetch at a
documented 4096-char transport cap).

### Who Can Call CU

Every agent Intendant runs or supervises reaches the same executor:

- **Native agent** — provider-native CU tool calls (`cu_enabled`), executed
  in the controller.
- **Codex / Claude Code / Kimi Code (supervised)** — the MCP bootstrap set
  (`tool_profile=core`) carries `read_screen`, `take_screenshot`, and
  `execute_cu_actions`; the injected `$INTENDANT`/`INTENDANT_MCP_URL` env
  makes `"$INTENDANT" ctl cu ...` work from their shells for the long tail.
  See [External Agent Orchestration](./external-agent-orchestration.md).
- **Pi (supervised)** — upstream Pi has no MCP client, so its appended
  supervision prompt uses the same scoped `$INTENDANT ctl cu ...` path
  directly. It reaches the same executor and grants; the UI never labels it
  as native Pi MCP.
- **Anything with a shell** — `intendant ctl cu` speaks to the running
  daemon's `/mcp` endpoint; discovery is lazy via `--help`.

All paths execute under the same server-side autonomy/approval model
(`DisplayControl` session grant for the user's display).

### Display Targets

CU actions operate on a `DisplayTarget` (`#[serde(tag = "kind")]`):

- **`Virtual { id }`** — an Xvfb-managed virtual display (`:99`, `:100`, …).
  `display_env_string()` → `":<id>"`. Virtual displays come into being three
  ways: the agent loop's Xvfb auto-launch, the agent starting one itself
  (`Xvfb :99 … &`), or `create_virtual_display` — the dashboard's keyless
  **New virtual display** action and a first-class MCP tool
  (`intendant ctl display create [--width N] [--height N]`, federated via
  `ctl --peer`; it awaits the ready/failed outcome and returns the new
  display's id plus an opaque capture generation) — the path that gives an
  authorized headless box a display with no API key configured. Created
  displays register a capture session immediately (streaming tile,
  CU-routable, announced to dashboards and federated peers) and are
  daemon-owned. Automated callers tear
  down exactly the generation they created with
  `intendant ctl display destroy DISPLAY_ID CAPTURE_GENERATION`; stale
  generations are refused and bound browsers are retired first. Displays are
  also destroyed when their tile is closed (a hard daemon kill leaves the
  usual orphan for the next allocation to reclaim). Xvfb is Linux-only. The
  macOS tool lane described below has separate shared-session authorization and
  does not register a display tile or streaming session.
- **`UserSession`** — the user's real desktop. On Linux X11 it resolves the
  login session's `DISPLAY` (falling back to `:0`); on macOS the primary display
  doesn't use `DISPLAY`. Requires an explicit `DisplayControl` grant via the
  autonomy system — enforced fail-closed at the `execute_actions` chokepoint
  on **every** backend (the raw X11/macOS capture/injection paths included,
  not just by session existence on Wayland/Windows), with one exemption: an
  **owner surface** (an owner/root dashboard, an enrolled root user client,
  local loopback `ctl`, or the stdio MCP transport the owner wired up —
  `ToolCallerTrust` in `mcp/mod.rs`, derived from the bound
  `AccessPrincipal`) may target its own desktop ungranted, because the
  owner's call *is* the opt-in. Every other caller — supervised external
  agents, scoped grants, federated peers — needs the standing grant.

When a pixel-CU call (`take_screenshot`, `execute_cu_actions`) omits
`display_target`, the default is availability-aware
(`computer_use::default_display_target`): the lowest-id live virtual capture
session wins, then the conventional agent Xvfb display (`:99`) when its X
socket is up (Linux only), then the user session. Explicit targets are never
second-guessed, and every downstream gate applies to the fallback exactly as
it would to an explicit `user_session` request. The native agent loop uses the
same resolution when a session has no CU display configured, except its
user-session fallback additionally requires the user-display grant — the
model-driven path never auto-targets the user's desktop ungranted.
(`read_screen` defaults to the user session unconditionally — element trees
only exist there — and sits behind the same grant/owner gate on all
platforms: the element tree reveals window titles and field values just as
pixels do, and it bypasses the session pipeline entirely.)

User-session access uses a **session-grant** model: approve once (the `d` hotkey
the dashboard's **Share with agent** action, MCP `grant_user_display`, or
`intendant ctl display grant-user`), and the grant holds for the rest of the
session until revoked. On Wayland, granting starts the GNOME portal flow. For
Computer Use, the operator must enable **Allow Remote Interaction** in the
physical portal dialog before clicking **Share**; approving screen sharing alone
can produce screenshots while leaving keyboard/mouse injection unavailable. See
[Autonomy & Approvals](./autonomy.md) for the approval surface.

### Experimental macOS monitor lifecycle and read-only controller

`intendant_platform::cgvirtual::VirtualDisplays` is an experimental main-thread
lifecycle primitive. The controller now exposes a deliberately separate local
MCP lane through `create_virtual_display`, `take_screenshot` and
`destroy_virtual_display`; it is not a general `DisplayTarget` backend or a
streaming session. Linux's Xvfb behavior is unchanged.

**A CGVirtualDisplay monitor shares the logged-in WindowServer session's focus,
cursor and clipboard. It is not a security sandbox.** Creating a monitor does
not grant Intendant display authority, bypass TCC, isolate an application, or
make input safe for an agent. Hotplug/removal can rearrange the user's windows.
The Xvfb authorization/isolation assumptions must not be carried over to macOS.

The underlying platform contract remains narrow:

- `open()` requires the main thread and probes private classes, selectors,
  argument counts and exact argument/return encodings before native creation.
  Missing classes/selectors, unfamiliar encodings and non-NSObject ancestry
  are refused. The probe does not create a monitor or request permissions.
  Linux and Windows return `UnsupportedPlatform`.
- One owner per process; at most **two** live monitors, each dimension
  **64–4096**, with one **1x, 60 Hz** mode. There is no resize, adoption,
  mirroring, layout assignment, input, clipboard, capture or permission API.
  `create()` polls only the returned object's ID for online/requested-size
  observation (two seconds / 100 iterations). This is not capture readiness.
- Handles bind an owner identity and a non-wrapping generation. Native IDs
  come only from retained objects' `displayID` getters; they are not Intendant
  stable display IDs or authorization tokens. Reused native IDs, foreign-owner
  handles and repeated destruction cannot select a replacement monitor.
  There is no fallback to any physical/user display.
- A small Objective-C ARC bridge owns the descriptor, settings, mode and
  display, including partial-creation rollback. Rust RAII releases the exact
  object graph on explicit destruction, failure and owner drop. Temporary
  autorelease references drain **before** waiting for removal. Exceptions are
  contained at the bridge; creation exceptions conservatively disable further
  creation because initialization may have performed unobservable partial work.
- `destroy()` invalidates the generation first, releases its exact objects and
  polls that native ID's offline status for at most two seconds / 100 iterations.
  Unconfirmed teardown disables further creation **for the process lifetime**,
  including after reopening the owner. Drop attempts the same cleanup for each
  remaining monitor but cannot return errors. No helper monitor, global reset,
  kill, or destruction by an adopted ID is attempted. Polling is bounded;
  synchronous private OS calls themselves cannot be interrupted, and process
  abort/kill does not run Rust destructors.

An independent reference for the private API shape is
[Chromium's macOS virtual-display test utility](https://chromium.googlesource.com/chromium/src/+/HEAD/ui/display/mac/test/virtual_display_util_mac.mm). This is a small original implementation, with no copied Chromium
lifecycle/workarounds. Runtime signature checks reject detectable ABI changes;
they cannot establish semantic compatibility of an undocumented OS API.
Native OS acceptance is required before integration.

#### Manual native smoke (supervisor only; never a default test)

Run only in a supervisor-approved logged-in macOS session where monitor
hotplug is acceptable, from an isolated worktree:

```bash
cargo run -p intendant-platform --example cgvirtual-smoke -- --create-shared-session-monitor
```

Without that exact argument the example exits with code 2 before any native
probe. It opens a 1024×768 monitor for three seconds, prints its native ID,
destroys it with offline observation, creates a replacement, verifies stale
handle refusal, and drops the owner to exercise RAII removal. Reopening the
owner verifies that teardown did not latch an uncertainty error. Failure exits
nonzero and RAII attempts cleanup. It does not start/contact/restart a daemon, install anything,
read user configuration/auth, write files, request TCC, capture pixels, or inject
input. The OS can still persist monitor preferences as a consequence of hotplug.
Record OS/build/architecture, stdout/stderr, exit status, and whether monitors
visibly appeared/disappeared. Leave any unexpected residual monitor to the
supervisor; never reset user displays to make the smoke pass.

Default inline tests inject ABI metadata and fake native objects; the macOS
bridge test inspects public NSObject method metadata only. They never create
native displays or touch GUI/TCC. `cargo check -p intendant-platform --examples`
checks the primitive smoke harness without running it. The controller smoke
below is separate; input/clipboard and dashboard integration remain out of scope.

#### Native lifecycle acceptance on the plugin Mac

Tested on macOS 26.4.1 (25E253), arm64, on 2026-09-15.

The first manual create/destroy pass verified the private classes but caught an
incorrect `release` ABI expectation: the installed Apple `NSObject.h` declares
`oneway void`, encoded `Vv`, not plain `v`. The runtime check now preserves that
qualifier and a hermetic regression pins it.

A second pass created a 1024x768 monitor but `CGDisplayIsOnline` did not confirm
removal. Polling a fresh `CGGetOnlineDisplayList` instead confirmed explicit
removal (approximately 84 ms in the successful run), replacement creation,
stale-generation refusal and RAII cleanup. The existing display inventory and
primary ID matched before and after. A full or failed enumeration cannot prove
removal and fails closed. This is lifecycle evidence on one Mac, not a guarantee
for other macOS releases or of unchanged window placement during hotplug.

No pixels were captured, no input was injected, no TCC prompt was requested, and
the running daemon was not replaced. Exact-ID capture and authority-preserving
daemon/plugin integration remain separate slices.

### macOS controller and exact-generation screenshot

`src/bin/caller/macos_monitor/` owns the local tool lane. A lazy broker shared
by the daemon's EventBus starts **one same-binary private helper**, intercepted
before runtime, configuration, credentials, logging or network startup. The
helper retains all native objects on its main thread. Its versioned JSON-line
protocol uses private stdin/stdout pipes, a 16 KiB line cap and monotonic
request/handle counters; there is no socket, authentication protocol, native
display ID adoption, or respawn. Explicit window identities carry a PID and start
generation; they confer no monitor ownership. EOF releases its owned monitors
and bound AX references. The parent closes the
pipe, waits, and if necessary terminates and reaps only the exact retained child.
Any uncertain protocol/native cleanup permanently retires that broker.

The request queue holds at most eight requests; at most two monitors exist,
each dimension **even and 64–4096**, rejected before helper startup or native
effects. `create_virtual_display` rejects Linux display-pool bounds on macOS
and returns a broker-local `display_id` in `0x20000000..=0x3fffffff` and an opaque
`macos_virtual:<owner>:<generation>` value as both `display_target` and
`capture_generation`. Preserve those values verbatim. Creation reports
`lifecycle_ready: true`, **`capture_ready: false`**, shared-WindowServer isolation,
and no input/streaming support. It does not publish a dashboard tile or peer
stream. Only a successful exact screenshot validates capture readiness for that
request; TCC Screen Recording permission remains separately required. Public
IDs are distinct from private helper handles and native IDs; numeric aliases
cannot be used for capture, destruction, input or streaming.

Lifecycle calls keep their existing `DisplayInput` IAM classification and
screenshots keep `DisplayView`. All three additionally require an owner surface
or the existing explicit user-display grant, including at dequeue and result
delivery. Monitor ownership does not grant display access or change the grant.

`take_screenshot` resolves only a live owned generation to the helper-retained
native ID. Before SCK content enumeration, read-only capture checks existing
Screen Recording permission with `ScreenCaptureAccess.preflight()`. Missing
permission and SCK errors return guidance without automatically requesting a
TCC prompt; ordinary capture keeps its existing permission behavior.
SCK selects that exact ID with no primary fallback; its backend
refuses input before any CGEvent call and disables the cursor overlay. The
five-second first-frame budget includes start time. Stop is driven to completion
on success, error, deadline or caller cancellation; destruction remains
serialized through stop and response delivery. Generation, helper identity and
liveness are rechecked before image delivery. Normal MCP image blocks and compact
artifact metadata use the existing contracts. Frame dimensions must match the
owned generation exactly. Complete PNGs are published without overwriting an
existing artifact, with owner-private file permissions (0600 on Unix).

Synchronous native start/stop calls cannot safely be interrupted. A twenty-second
frontend deadline cancels its result, **not** the cleanup worker. A stalled
native call retains the sole worker, backend and helper ownership, admitting no
further lifecycle operations until cleanup completes. Dropping a create receipt
before the local tool handler finishes constructing its response rolls back that
exact generation. **This is not a network or client acknowledgment**: transport
loss after local commit can leave a live monitor whose handle the client never
received. No timeout authorizes another helper or assumes cleanup succeeded.

Retain both `display_id` and `capture_generation` from each successful create
response for exact cleanup. `list_displays` keeps its existing shape and visibility;
it does not recover broker handles or generation selectors (an OS inventory entry
is not a lifecycle handle). Owner surfaces can recover committed handles using
`list_macos_monitors` (`inspect display monitors`), under `DisplayView` IAM.
Scoped callers cannot enumerate this daemon-wide inventory even with a
user-display grant. The non-starting read returns `not_started` and an empty
inventory before any create, and queues behind pending receipts and cleanup.
Live entries are verified against the exact helper-owned objects. Failure
retires the broker through owned cleanup and removes all usable handles; a busy
or timed-out inspection reports `unavailable`. Neither outcome permits respawn,
ID adoption or bypassing cleanup.

Reserved selectors, including malformed/case/whitespace variants, are refused
by generic input, AX-tree, shared-view, browser-workspace and peer forwarding APIs.
Explicit owner-only window placement is described below. Exact canonical
selectors alone have a read-only `display_readiness` route: existing `DisplayView`
and shared-session authority are required, and status uses only broker liveness
and `Resolve`. It reports lifecycle verification separately from unverified
capture and unsupported input/streaming; overall CU readiness remains false.
Unknown/stale/foreign generations are distinct from a retired broker. Malformed
selectors and raw-ID aliases remain refused, without a physical-display fallback.
There is no automatic/default selection of these monitors, global input,
clipboard isolation, streaming or browser-placement claim.

#### Explicit owner-only app window placement

The same private main-thread helper now retains exact AX windows alongside
owned monitors. `list_macos_windows {pid}` retains one inventory of at most 16
exact AX objects and lists their opaque `candidate` tokens plus identities
(`pid`, `start_seconds`, `start_micros`, `window_id`) under `DisplayView` plus
owner-surface-only authority. `bind_macos_window {display_target,candidate,identity}`,
`place_macos_window {binding,bounds}` and `unbind_macos_window {binding}` require
`DisplayInput` **and** owner-surface-only authority. Existing scoped user-display
grants do not authorize these operations. Neither listing nor placement starts a
native helper; an owned monitor must already have been created.

Each helper listing refresh (even failed) invalidates unbound tokens across PIDs
and callers. Existing bindings survive refresh. Bind requires token plus matching
identity, transfers the retained listed AX object, and consumes the token when
selected; no ID-only replacement is admitted. It verifies unique SPI mapping and
process start generation, freezes the retained monitor's live bounds, and returns an opaque
binding. Place uses monitor-local logical-point bounds wholly inside that
monitor. Identity, monitor geometry and focus are checked around each AX
position/size write. Verified success requires AX **and** CG readback within one
logical point per component and strict whole-rectangle containment; dispatch
alone is not success. Failure after a setter path returns partial application
and observed/unknown focus interference, preserving the latest observation even
on AX/CG disagreement. Late deadline/delivery/liveness failures explicitly mark
`effects_unconfirmed:true` and preserve any available partial or verified placement
result, with outer `ok:false`. Movement may already have applied or still be in
progress. There is no activation, raising,
unminimizing, keyboard/mouse/clipboard action, retry or focus restoration.

Sixteen bindings maximum; 50 ms AX IPC limits and a four-second operation budget;
receipt commit has a two-second limit. Expiry closes the receiver then drains a
buffered commit, so successful send cannot race binding rollback. Dropped
bind receipts roll back only the retained binding. Cancellation after placement
dispatch finishes the bounded attempt without undoing window movement. Unbind,
monitor teardown and helper EOF release references without closing/moving user
windows. This is shared-session app placement, not input isolation or general CU
readiness. Windows/Linux refuse clearly. Ordinary startup remains unchanged.

See the source-tree design record `docs/design/macos-monitor-window-placement.md`
for exact response/error semantics, limitations, validation commands and
`scripts/verify-macos-window-placement.py`. The native fixture passed on the
plugin Mac on 2026-09-16 in a separate temporary-HOME HTTP daemon. It verified
candidate lifetime, exact placement/readback and cleanup using only its disposable
panel. The existing HTTP harness supports `--placement-fixture` for reproduction.
Final readback may settle for at most 250 ms / 20 polls within the original budget;
this never repeats a setter and still fails closed on identity/focus/geometry changes.

#### Recovery/status validation

The owner-only recovery and read-only status fixture passed on 2026-09-16.
See `docs/design/macos-monitor-recovery.md` in the source tree for its contract
and acceptance record. Readiness retains the common target/summary/layers
shape; unprobed permissions remain unknown and input remains blocked.

#### Opt-in controller smoke (supervisor only; not a default test)

After building the controller in this worktree, and only in an approved logged-in
macOS session where monitor hotplug/removal and capture are acceptable:

```bash
python3 scripts/macos-monitor-smoke.py --binary ./target/debug/intendant \
  --accept-shared-session-hotplug
```

The private smoke entry point loads no daemon configuration and starts no daemon.
The script requires an already-built binary, never invokes Cargo, and refuses
native work without the opt-in flag. Screen Recording permission must already exist.
It creates its own 640×480 monitor, captures and checks its PNG/geometry, destroys
that exact generation, checks stale capture/destruction refusal, then creates a
second test-owned monitor and closes the helper pipe to verify EOF cleanup and
successful child exit. Images are confined to its temporary directory. It never
adopts, captures or destroys any pre-existing monitor. It can still rearrange the
login session's windows as a consequence of hotplug. It checks lifecycle, exact
geometry and PNG encoding; it does not prove which visible pattern was captured.
The supervisor's independent HTTP pixel fixture supplies that separate evidence.
**This smoke has not been run for the controller slice.** Default inline tests use fake owners, transports and
capture backends; the process fixtures are private shell pipes with no GUI calls.

### CU Readiness Diagnosis (`display_readiness`)

The display grant is **Intendant authority only** — OS-level capability is a
separate set of layers, and any of them can block actual CU while the grant
reads as held (the classic macOS shape: `already_granted`, yet Screen
Recording denies every capture). `src/bin/caller/cu_readiness.rs` probes five
layers independently and names the non-ready ones, each with a fix:

1. **`intendant_display_authority`** — the user-display grant / caller trust
   (virtual targets need none).
2. **`screen_capture_permission`** — macOS Screen Recording
   (`CGPreflightScreenCaptureAccess`); Linux Wayland portal session or X11
   socket; Windows desktop capture session.
3. **`accessibility_permission`** — macOS Accessibility (`AXIsProcessTrusted`);
   Linux AT-SPI bus reachability (bounded probe); Windows UIA client
   instantiation.
4. **`target_display`** — the requested display exists / has a live capture
   session.
5. **`input_backend`** — CGEvent (macOS, rides Accessibility), XTest (X11),
   portal remote desktop (Wayland), SendInput via session (Windows).

Probes are cheap, strictly read-only (they never pop permission prompts), and
**never cached** — TCC and portal state can change or be revoked at any
moment. A probe that cannot determine its layer reports `unknown`, and
unknown counts as **not ready** (fail closed). Surfaces:

- the `display_readiness` MCP tool (display-view IAM class; listed in the
  `core` and `screen` profiles),
- `intendant ctl display status [--target TARGET]`,
- `request_user_display` answers (`already_granted` and approvals) carry an
  `os_readiness` gap block naming any still-blocked OS layers, so a granted
  request can never masquerade as a CU-ready one.

### Observation Policy (`observe`)

Every `execute_cu_actions` batch ends with a **trailing observation**, and
what that observation is is a per-call policy (`observe` param;
`src/bin/caller/cu_observation.rs`), not an unconditional screenshot:

- **`pixels`** (default) — the historical behavior: a post-action screenshot
  is captured and appended as one extra result. The default stays `pixels`
  because the tool description and the native `peer cu` guidance promise a
  screenshot; token-sensitive callers opt in to the cheaper modes (managed
  Codex's developer instructions teach `observe:"auto"`).
- **`ax`** — the frontmost element tree (the `read_screen` walk, same central
  text caps) is attached as text *instead of* pixels: a few hundred tokens
  versus ~1.5k image tokens, and no capture/encode work at all. User-session
  targets only (element trees exist nowhere else); a failed walk reports the
  error as the observation rather than silently substituting an image.
- **`auto`** — `ax` when the frontmost tree is usable (≥
  `cu_observation::AX_AUTO_MIN_NODES` nodes), `pixels` otherwise (sparse
  tree, walk error, virtual-display target).
- **`none`** — per-action results only, for callers chaining batches that
  will observe once at the end.

The result always **names the observation it carries and why**
(`observation: pixels (auto: ax sparse (2 nodes) → pixels)`), so a fallback
is never silent. Two invariants regardless of mode: an explicit trailing
`screenshot`/`zoom` action always returns its pixels (the action *is* the
request), and the managed-context compact contract is preserved — path +
metadata, no inline image bytes — with `ax`/`auto` the compact caller gets
the element tree **inline**, which is the first time the managed path
carries an actual observation instead of a path to fetch.

**Screenshots are clean by default, encoded once.** Click markers
(crosshairs at click coordinates) are opt-in via `annotate: true` — baked-in
markers obscured the very controls being verified (e2e finding CU-06), and
the dashboard Live tab already overlays actions in real time. Annotation is
drawn on the raw frame *before* the single PNG encode; the historical
pipeline (capture → encode → decode → draw → re-encode → rewrite the disk
artifact) is gone, and the disk artifact now always carries the same bytes
as the model payload. That parity is deliberate: the artifact's remaining
reader is the managed-Codex `view_image` path (the Activity-tab
disk-substitution consumer was retired with the Gemini CLI backend), which
wants exactly what the model would have seen — including, on macOS, the
logical-size (not raw-Retina) image that CU coordinates map to. Each batch
logs a `[cu]` measurement line (observation kind/reason, capture+encode ms,
AX walk ms, observation bytes, settle outcome) to the daemon log, and the
native loop logs the same line into the session log.

### Settle: bounded UI quiescence (`settle`)

The 300 ms freshness floor guarantees a *recent* frame, not a *finished* UI —
after a click that starts a page load, the model historically padded batches
with guessed `wait` actions. `settle` replaces the guess with a bounded
quiescence wait (`cu_observation`): after the last input action, watch the
display and return once no content change has been observed for a ~300 ms
quiet window, capped at 2 s (`settle: true`) or a caller-supplied cap
(`settle: <ms>`, clamped to 5 s; `0`/`false` = off).

- **Anchoring:** the wait is anchored at the last input action. It runs
  before the batch's first capture that follows an input (`[click,
  screenshot]` settles between the two), or before the trailing observation
  — the AX walk and the auto-screenshot both read the settled UI. A batch
  with no input actions settles from call start ("capture once quiet").
- **Damage signal:** platform-native dirty rects when frames carry them
  (ScreenCaptureKit); otherwise a per-frame content fingerprint — the X11
  backend polls at the capture rate and re-delivers unchanged frames, so
  frame *arrival* is deliberately not treated as change.
- **Honest reporting:** the result carries `settle: settled after Nms (no
  display change for 300ms)` or `still_loading after Nms (display still
  changing at the cap)`. Paths without a usable damage signal — no live
  capture session, a capture stream that ends mid-wait, or the synthetic
  test-card backend (whose counter strip free-runs, which also keeps the
  e2e suite deterministic) — degrade to a fixed minimal wait and say so
  (`fixed 300ms wait (...)`).
- **No double-wait:** a damage-verified settle subsumes the capture
  freshness wait — the stream was watched past the input, so the following
  capture serves the current frame immediately.

`settle` composes with every `observe` mode (with `none` it simply bounds
the batch's return for callers chaining batches), and is exposed on the MCP
tool, `ctl cu actions --settle MS`, and the peer twins (older peers ignore
it).

### Three separate concepts: private view, agent share, presence streaming

Putting the user's screen on the wire means one of three deliberately
distinct things, and the surfaces never conflate them:

1. **Private view** (dashboard **View this machine**) — remote
   view/control of this machine's display from the owner's dashboards.
   The capture session is created with **`agent_visible = false`**
   (`ControlMsg::GrantUserDisplay { agent_visible: false }`): it streams
   over WebRTC only to owner/root dashboard connections and accepts input only
   from those connections. The same ceiling is enforced on the legacy `/ws`
   path and the verified dashboard-control DataChannel; a scoped role's
   `display.view` or `display.input` permission covers agent-visible displays
   only. The private view is invisible to agents, **fail closed**, at two
   independent layers:
   - the `DisplayControl` autonomy grant is never set (so the CU
     `execute_actions` chokepoint, `INTENDANT_USER_DISPLAY_GRANTED` on
     runtime children, `shared_view`, and `read_screen` all refuse the
     user session exactly as if nothing was granted), and
   - the session itself is skipped by every agent-facing registry
     lookup — `SessionRegistry::get` / `display_ids()` are filtered by
     the session's `agent_visible` flag, so CU session routing, the
     screenshot session lookup, `default_display_target`,
     `list_displays` resolution overlays, federated peer streaming and
     peer display announcements, presence's available-display list, and
     the 1 Hz FrameRegistry sampler (`display_<id>` model-feed frames,
     hence `list_frames`/`read_frame` and conversation auto-attach) all
     read it as absent. Owner/root dashboard paths may use the unfiltered
     `get_any` / `all_display_ids` accessors after that authority check;
     scoped dashboards and federated peers stay on the filtered accessors.
   Auto-recording also skips private views. For the alpha, an explicit
   `StartRecording` is owner/root-only and still requires an active
   agent-visible display. A private or absent display is rejected before
   ffmpeg starts: recording artifacts live in the ordinary session tree, so
   private pixels must never be written there in the first place.
2. **Agent share** (dashboard **Share with agent**, MCP
   `grant_user_display`, ctl `display grant-user`) — the classic grant:
   `agent_visible = true`, the autonomy grant is set, and the display is
   enumerable/screenshotable/drivable by agents until revoked. A share
   over an existing private view **upgrades it in place** (frames start
   feeding the registry on the next sampler tick; no capture restart,
   no second portal dialog). The reverse never happens implicitly: a
   view request over a shared display does not downgrade it — taking
   the agent's access away is an explicit revoke.
3. **Presence streaming** (the tile's **Stream** button) — continuous
   frames to the live presence (voice) model only. Independent of both
   modes above and unchanged by them; main agents are not affected.

Revoking a user display (either mode) tears the session down and clears
the per-daemon grant flag. On the wire, `GrantUserDisplay` without
`agent_visible` keeps its historical meaning (`true` — the agent share),
so pre-split frontends and scripts are unaffected.

Granting and resolving a display request are themselves owner/root-only:
`GrantUserDisplay` and `ResolveDisplayRequest` from a scoped caller are refused
on both dashboard transports. `RevokeUserDisplay` stays open to an otherwise
authorized scoped caller — de-escalation is fail-safe. The shared-view tools
(`show_shared_view`, `focus_shared_view`, `request_shared_view_input`,
`capture_shared_view_frame`) activate a user-session target only for a granted
or owner caller instead of flipping the grant themselves. Sharing an
agent-owned virtual display never touches the user-display grant.

### Dashboard Live workspace

The dashboard's **Live** tab presents local Computer Use as one selected
display stage. Every active `DisplaySlot` and its WebRTC connection remain
alive, but only the selected slot is visible; the rail switches that
projection without recreating the capture or video element. Agent-requested
shared views select their target when the current stage has no active human
work. They remain advisory when the user is controlling, awaiting input,
annotating, calling out, or viewing full-screen; the banner and rail keep the
requested target discoverable without discarding that work. Peer-display
launchers are different: they route to the peer-aware **Station** surface
before opening the stream rather than pretending a federated pane is local.

Input authority in the stage and rail is always the browser-relative server
state: **you**, **another viewer**, **available** (unclaimed), or connecting.
Take is last-take-wins; no holder identity or approval ceremony is inferred.
Only one hidden-input hazard is handled locally: selecting another display or
leaving Live releases an actively bound slot after flushing held keys and mouse
buttons, then removes its keyboard, pointer, and document-level paste listeners.
Annotation and armed-callout state is editable work, so display and tab
navigation is blocked with a visible explanation until the user finishes or
closes it.
Pending Take requests and already-held-but-locally-unbound authority are also
cancelled before a surface can be hidden. Activity's shared-view **Take input**
returns to the full Live stage before requesting authority; thumbnails remain
view-only.

The rail's per-display activity feed combines browser-observed lifecycle
events (stream connection, authority, private/share mode, presence streaming,
recording, annotation, and shared-view focus) with the daemon's **live action
lane**: `computer_use::execute_actions` emits one `cu_action` event per
successfully executed CU action (`CuActionObserver`), carrying the kind
(`left_click`, `type`, `screenshot`, …), display id, driving session id,
display-space coordinates plus their reference resolution, a short raw call
string (`left_click(612, 233)`; embedded text truncated for presentation —
the Activity log keeps the full trace), a unix-ms timestamp, and a dedupe
`event_id`. The lane is deliberately **ephemeral**: it rides the outbound
broadcast to the `/ws` and dashboard-control lanes only — never the session
log, never replay — and mouse moves coalesce to 10 Hz. Failed actions never
emit; the overlays must not show clicks that did not happen. Peer upcasters
drop the event, so **federated (peer) displays render no action overlays**
today — a known follow-up.

Those events drive the stage's action overlays on the display they belong
to: an agent cursor (white arrow + verb pill reading Look / Move / Click /
Type / Scroll / Waiting) that eases toward each action point, dims while the
dashboard user holds input authority, and fades out when idle; a click
ripple; last-typed keypress chips (from the truncated raw call, space shown
as `·`); and a full-stage screenshot flash. The concept's target-highlight
box + role tag are deliberately not implemented — no element/role data
exists on the wire (future AX integration). All overlays are
`pointer-events: none`, `aria-hidden`, honor `prefers-reduced-motion`, and
compute geometry from the letterboxed video rect — nothing touches the video
element itself. Feed rows for actions use a two-line grammar (friendly
sentence + seconds-precision timestamp, raw mono call below); the feed keeps
the last 50 entries per display and follows the bottom only when already
scrolled there.

`cu_action` session attribution also feeds the rail's **approval card**:
when a pending approval belongs to the session the daemon last reported
driving the selected display, an amber card renders under Input authority
whose Approve/Deny proxy the main approval panel's own session-scoped
actions. No attribution, no card — it never guesses. Recording transitions
enter the feed only after the daemon confirms them; Start, Stop, and Delete
remain pending and retain the last confirmed state on transport error or
timeout, and while recording the Record button ticks the elapsed time from
the confirmation. At narrow widths the same rail becomes a
keyboard-accessible modal drawer; the stage toolbar remains the fallback for
primary controls. In-page full screen similarly contains focus, hides its
background from assistive technology, supports Escape, and restores the
invoking control.

### CU-First Routing

> **Status note (vaulted 2026-07-04):** the all-tasks CU-first interception —
> where every non-direct task in the agent loop is first offered to a fast CU
> model (`try_cu_first` in `display_glue.rs`) — is **off by default**, behind
> `[experimental] cu_first_routing = true`. In practice it added a model hop
> (latency) to every task and, under subscription-based external agents
> (Codex, Claude Code, Kimi Code, Pi), reintroduced an API-key model the deployment
> otherwise didn't need. The code stays runnable for a future pickup. What
> remains always-on is the *frame-grounded* dispatch described below: when
> the user issues a task while pointing at a display, that is an explicit
> computer-use request, and the CU task path is the only machinery that can
> act on the referenced frames. The fast paths and observation layer above
> are designed for the heavy agents (native, Codex, Claude Code, Kimi Code, Pi) as primary
> consumers and do not depend on either router.

Frame-grounded work goes to a fast CU model, with escalation to the heavy
agent for anything that turns out to need code changes. The routing decision
lives in the session supervisor (`session_supervisor/launch.rs`,
`start_new_session` → `spawn_cu_task`):

```
submit_task / StartTask
        │
        ▼
 reference_frame_ids non-empty?
        │
        ├── yes ─▶ spawn_cu_task  (fast CU model, with the referenced frames
        │             │            resolved to images as visual context)
        │             │
        │             └── CU model decides it's not a display task
        │                  → calls escalate_to_agent → heavy agent runs it
        │
        └── no  ─▶ normal agent / orchestrator path
```

The gate is **`reference_frame_ids` being non-empty** (the frames the user was
looking at, supplied by [presence](./presence.md)'s `submit_task`); the
`display_target` hint is carried through to tell the CU pipeline which display
to act on. Earlier docs implied `display_target` alone triggers CU routing — the
actual trigger is the presence of reference frames. The CU provider is given
native CU tools plus a single `escalate_to_agent` function tool; calling it ends
the CU run with `CuTaskResult::Escalate` and hands the task to the main agent.

### Configuration

```toml
[computer_use]
provider = "gemini"          # optional; default = CU_PROVIDER / PROVIDER env, else auto
model = "gemini-3-flash-preview"  # optional; gemini default shown
backend = "auto"             # "x11" | "wayland" | "macos" | "windows" | "auto" (default)
```

Provider/model resolution (`provider::select_cu_provider`): config → `CU_PROVIDER`
/ `CU_MODEL` env → `PROVIDER` env → auto. Default models when unset: gemini
`gemini-3-flash-preview`, anthropic / openai use their CU-capable defaults.

## Live Audio

`spawn_live_audio` is an **agent tool** (defined in `src/bin/caller/tools.rs`),
not a CLI flag. It spins up an *untrusted* voice sub-agent that talks to Gemini
Live or OpenAI Realtime and exchanges audio with an application through a virtual
audio bridge.

### How It Works

```
voice model ──WebSocket──▶ Intendant ──audio bridge──▶ application
   │  PCM16 mono 24 kHz        │   virtual mic/speaker      │
   │  structured tool calls    │   (platform audio bridge) │
   │◀─────────────────────────│◀──────────────────────────│
```

1. The agent calls `spawn_live_audio` with `id`, `provider`, `playbook`, and a
   mandatory `response_schema` (plus optional `timeout_secs`, `voice`,
   `display_id`, `initial_message`).
2. Intendant opens an audio bridge and connects to the voice model with a
   *whitelisted* tool set generated from the response schema.
3. The model follows the playbook; its turns are bridged as audio to/from the
   app, and inbound audio is also teed to Whisper for a transcript.
4. When the model calls `submit_response`, the data is validated against the
   schema; the tool returns a `LiveAudioResult` with a `status` of `Completed`,
   `TimedOut`, `Disconnected`, `SchemaError`, or `Failed` (the last two carry
   an error string).

The live path is bounded for real-time behavior. Vortex capture still polls its
shared-memory ring every 5 ms, but aggregates samples into 20–40 ms WebSocket
frames and flushes a short tail when the input goes quiet. Playback, capture,
and the optional Whisper tee each hold at most 30 s of PCM16 and evict the
oldest audio on overflow, so a stalled consumer skips ahead instead of growing
memory or replaying a call far behind real time. Whisper intake forms 3 s
windows, keeps at most four pending while one sequential request is in flight,
drops the oldest completed window on overflow, and flushes each ordered
transcript entry to disk.

### Security Model — Untrusted, Schema-Validated, Quarantined

The voice model is treated as hostile input. It has **zero tools** beyond the
two generated from the schema (`submit_response`, `end_call`) and **zero file
access**. Three layers protect the rest of the system:

- **Whitelisted tools + schema validation** (`schema_validator.rs`): the model
  can only call the response tool; submitted data is checked against the
  declared `ResponseSchema`, with oversized fields truncated and off-schema data
  rejected.
- **Quarantine** (`quarantine.rs`): any unexpected content — a tool-call attempt
  for an unknown tool, oversized strings, off-schema payloads — is written to
  `~/.intendant/quarantine/<live_audio_id>/<payload_id>.json` and **only a
  reference is returned**; the raw content is never surfaced to the agent.
- **Sandbox**: when the write sandbox is enabled, live-audio processes can write only to the session
  log and quarantine directories — no project root, no `/tmp`.

### Silence Watchdog

The session loop runs a time-based watchdog (`live_audio.rs`): if there has been
**no model output for 15 seconds**, it sends one nudge ("Are you still there?
Please continue the conversation.") to unstick a frozen model, and resets when
output resumes. A separate turn counter nudges the model toward emitting its JSON
response after enough turns. (Earlier docs described "6 consecutive unresponsive
turns" — the real mechanism is the 15-second silence timer.)

### Audio Bridge — Per Platform

On macOS and Linux the bridge first probes for the Vortex shared-memory segment
(`shm_open("/vortex-audio")`). The preferred Unix path is the **Vortex Audio
HAL plugin via a direct POSIX shared-memory bridge**
(`start_vortex_shm_bridge`, `shm_open` + `mmap`, no daemon/socket); if its
segment is absent, the platform fallback is used. Windows goes directly to its
ffmpeg/VB-CABLE fallback:

| Path                  | Implementation |
|-----------------------|----------------|
| Vortex (preferred)    | Direct POSIX shm ring buffers shared with the Vortex HAL plugin (`start_vortex_shm_bridge`). Converts Vortex Float32 stereo 48 kHz ↔ model PCM16 mono 24 kHz. POSIX-only. |
| Linux fallback        | PulseAudio null sinks via `pactl` (virtual mic/speaker, set as default for the session, restored on drop). |
| macOS fallback        | BlackHole virtual device + `SwitchAudioSource` (legacy). |
| Windows fallback      | `ffmpeg` captures `CABLE Output` with DirectShow and `ffplay` consumes model PCM from stdin through the system default output. Requires VB-CABLE and manual default-device routing; per-app routing and automatic default save/restore are unavailable. |

Audio routing is only needed for the app-to-model bridge. Browser voice
interaction through the [dashboard](./web-dashboard.md) (Gemini Live / OpenAI
Realtime) needs none of this.

### Configuration

```toml
[live_audio]
enabled = false                # default: false
default_timeout_secs = 300     # default: 300 (5 minutes)
gemini_model = "gemini-2.5-flash-native-audio-preview-12-2025"  # optional
openai_model = "gpt-4o-realtime-preview"                        # optional
sample_rate = 24000            # default: 24000
```

`LiveAudioSpawn` is its own [autonomy category](./autonomy.md#action-classification),
so spawning a voice session can be gated independently of other actions.

## Phone Calls (`phone-call` skill)

`skills/phone-call/SKILL.md` places an outbound SIP call and conducts the
conversation with a voice model. **macOS only**; requires the Vortex Audio HAL
plugin, `pjsua`, and a GUI session with TCC mic permission.

```
voice model ──shm──▶ Vortex Audio device ──▶ pjsua (SIP/SRTP) ──▶ phone
   │                                                                          │
   │◀────────────────────────────────────────────────────────────────────────│
```

How it works:

1. Find the Vortex device index (`pjsua --null-audio | grep vortex`).
2. Start `pjsua` with Vortex as both `--capture-dev` and `--playback-dev`,
   SRTP enabled, dialing the target SIP URI.
3. **Immediately** call `spawn_live_audio` (`provider: openai`) — do not wait
   for the call to connect; the shm bridge polls and works before connect.
4. The model conducts the call per the playbook and returns structured data.
5. Clean up `pjsua`.

`response_schema` is **mandatory** — without it the call is rejected with a parse
error. Do **not** set `initial_message`: the model starts speaking when it hears
the callee.

## Voice Calls Through Any App (`voice-call-app` skill)

`skills/voice-call-app/SKILL.md` makes a voice call through **any** app (Element,
FaceTime, WhatsApp, …) by combining [computer use](#computer-use) to drive the
UI with `spawn_live_audio` for the conversation. **macOS or Linux with a
display**; requires the Vortex Audio HAL plugin and a GUI/TCC mic permission.

How it works:

1. Prepare the `spawn_live_audio` arguments (playbook, schema, voice, id)
   *before* dialing, so they fire the instant the call connects.
2. Use CU actions to foreground the app, navigate to the contact, and click the
   call button. (`take_display_control` is **not** required for
   `execute_cu_actions` — only take it if you need exclusive input.)
3. Call `spawn_live_audio` (ideally in the same turn as the call click to
   minimize dead air).
4. Write the result from `response_data` immediately; hang up on completion.

The voice model has two generated functions here: `submit_response` (the schema
fields) and `end_call`. It submits data, then signals `end_call`.

### Response Schema Format

Both skills use the same `ResponseSchema` shape (`live_audio_types.rs`). Each
field nests its type under `field_type`:

```json
{
  "fields": [
    {"name": "guest_name",       "field_type": {"type": "string",  "max_length": 100, "tainted": true}, "required": true,  "description": "Guest name"},
    {"name": "party_size",       "field_type": {"type": "integer", "min": 1, "max": 50},                "required": true,  "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string",  "max_length": 50,  "tainted": true}, "required": true,  "description": "Confirmed time"},
    {"name": "confirmed",        "field_type": {"type": "boolean"},                                     "required": true,  "description": "Whether confirmed"},
    {"name": "special_requests", "field_type": {"type": "string",  "max_length": 200, "tainted": true}, "required": false, "description": "Any special requests"}
  ]
}
```

Field types: `string` (`max_length`, `allowed_values`, `tainted`), `integer`
(`min`, `max`), `boolean`, `array`. The voice model cannot submit until all
`required: true` fields are filled. Fields marked **`tainted: true`** carry
user-/callee-provided content and are treated as untrusted data, never as
instructions.

## Browser Microphone Transcription

Separately from live audio, the server can transcribe the *user's* dashboard
microphone via Whisper (`transcription.rs`). Off by default. The web gateway
buffers `user_audio` WebSocket frames into ~3-second chunks, filters silence by
RMS energy, wraps them as WAV, sends them to the transcription API, and
broadcasts `user_transcript` events.

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"          # default
language = "en"              # optional ISO-639-1 hint
# endpoint = "http://..."    # optional; custom/self-hosted whisper-compatible endpoint
```

Requires `OPENAI_API_KEY` (or a custom `endpoint`).

### Owner-only bound-window semantic controls

`read_macos_window_elements {binding}` (`inspect display window-elements BINDING`)
returns a bounded inventory of actionable controls only inside the retained exact
window on its unchanged owned monitor. It requires DisplayView and OwnerSurface,
reads no values/document text, skips secure subtrees, and replaces prior tokens
across bindings. `act_macos_window_element {binding,element,action}` (`act display
window-element BINDING ELEMENT ACTION_JSON`) requires DisplayInput and OwnerSurface
through typed, HTTP and facade dispatch. Both handles are mandatory. Actions are
`{"type":"press"}` or `{"type":"set_value","text":"..."}`; tokens are consumed
before native validation. Only known enabled AXPress controls and nonsecure
settable text roles are admitted. Text is at most 1024 UTF-8 bytes and exact
readback returns no contents. Press reports dispatched, never verified effect.
Identity, retained ancestry (including reciprocal bounded AXChildren membership),
exact bounded labels, bounds, monitor, permissions and focus are checked
before/after the attempt; cancellation/late failures preserve available evidence.
There is no global input, activation, clipboard, retry, rollback or focus restoration.

This shares WindowServer and is not an isolated seat. AXRole remains required.
Documented absence of optional AXSubrole/AXContainsProtectedContent is admitted; errors are not absence. Explicit secure/password metadata and protected
containers are excluded before traversal or content reads. Malformed, oversized,
contradictory or failed metadata replies still refuse. The standard-AppKit fixture
profile uses native controls without subrole or hierarchy overrides; its acceptance
scope and results are recorded in `docs/design/macos-appkit-controls.md`.
No general Chromium/canvas compatibility or protection against dishonest
application metadata is claimed. Existing exact-identity, focus, containment,
permission and single-use-token checks are unchanged.
See the [bounded control design and supervisor harness](../design/macos-bound-window-actions.md).

### Chromium acceptance and redundant placement writes

Owned-window placement skips position or size only when both native observations
already match that component exactly. Results report 0, 1 or 2 attempted setters;
a no-op still validates current identity, containment and focus. A known zero-write
result preserves that evidence on a late receipt failure. Mutability is checked
for the setter being attempted, not demanded by read-only observation.

The bounded semantic tree depth is 16 (formerly 8), covering the measured depth-13
disposable Chromium tree. Other traversal, retention, wire and time limits are
unchanged. The opt-in Chromium fixture uses a dedicated Chrome for Testing profile
and a synthetic local page. CDP creates the test window and independently observes
its state; it does not supply tested text, clicks or canvas input. Browser launch
can still activate an app despite an OS nonactivating request; the background-window
fixture detects observed activation and refuses, never restoring focus.

This does not enable arbitrary canvas input or make an independent input seat.
See `docs/design/macos-chromium-controls.md` for the exact acceptance status.

### Exact Chromium accessibility bridges

The native adapter projects one narrowly checked shape: an AXGroup exposes an
AXWebArea whose actual AXParent is an omitted AXScrollArea under that same group.
The exact intermediate node and original exposed child are retained together.
The original AXParent/AXWindow/PID checks still run on the resulting path; current
child membership also compares the exposed-child witness, not only the intermediate
object. Replacement, detachment, reparenting or protection requires a fresh read
or refuses the action. This is not a generic ancestry-repair fallback.

A projected intermediate counts toward the existing node/depth limits and is
checked for protected content before traversing its sole exposed child. It does
not expose other children hidden under that intermediate. No global input or
activation fallback is added. See `docs/design/macos-ax-parent-ancestry.md` for
native evidence and the deliberately narrow acceptance scope.

The Chrome for Testing 153.0.8010.52 semantic fixture passed on 2026-09-18:
exact Unicode text and one button effect were independently observed; old tokens
were refused after control replacement and same-window document reload. The
standard-AppKit regression passed too. Earlier unavailable-focus/metadata and
deadline refusals remain recorded separately. This does not establish arbitrary
site/canvas input, continuous focus isolation or an independent clipboard seat.

### Concurrent-owner evidence and raw-pointer experiments

The manual Chromium harness reports whole-run desktop sample changes separately
from action-level focus checks. Pointer, foreground-app and clipboard-change-count
differences are unattributed; missing observations are unknown, and equal samples
do not prove continuous isolation. Target activation and failed native focus
checks still refuse.

The separate raw-pointer fixture defaults to nonposting event construction with
native field readback. Its explicitly opted-in live mode targets only its own
process and requires matching tagged receiver events plus an independent canvas
counter. That live mode is not yet acceptance-tested and is not a daemon tool.
No arbitrary application, raw keyboard/canvas capability, independent seat or
expanded display grant is advertised. See
[raw pointer probe](../design/macos-raw-pointer-probe.md) for boundaries and commands.

### Disposable raw-pointer receiver acceptance

The opt-in test fixture now demonstrates a paired click both within its own
process and from a private child to its retained parent window. It uses an
explicit window-addressed event plus runtime-probed private window-local
coordinate accessors; missing symbols or failed readback refuse. Delivery is
separate from posting: exact tagged receiver/source/window/global/local receipts
and one canvas-counter increment are required. The source stays alive through
acknowledgement and both processes have bounded cleanup. Whole-run owner activity
remains unattributed; target activation never becomes acceptable because the
owner is active.

This is test infrastructure, not an enabled daemon raw-input tool, arbitrary
Chromium support or an independent input/clipboard seat. The default mode remains
nonposting and creates no application/window. See
`docs/design/macos-raw-pointer-delivery.md` for the evidence and private-API boundary.


### Native Chromium canvas pointer fixture

The opt-in Chromium pointer fixture verifies a single native process/window-addressed
mouse pair on a synthetic page in a fresh Chrome for Testing profile. It requires
matching browser client/screen coordinates, one independent canvas effect and a
refused replay. The private event window location uses a top-left origin in the
tested runtime; asymmetric Chromium and AppKit cases expose the reflection that
center-only tests missed. Missing SPI or inconsistent geometry refuses.

This is test infrastructure, not a raw-input MCP tool or an isolated input seat.
DOM does not expose the Quartz correlation tag; observed events are not authenticated
source attribution. The standalone fixture does not create a virtual monitor.
Production integration still needs exact retained-window/monitor generations,
current authorization and cancellation/partial-pair handling. See
`docs/design/macos-chromium-pointer.md` for reproduction and evidence boundaries.
