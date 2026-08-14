# XR (immersive spatial dashboard)

The XR surface is the immersive presentation of the **regular dashboard**:
inside a WebXR session, the operator's room becomes the operating surface —
session cards shelved around you, the focused session on a near-field
workbench, approvals that arrive with physical gravity, and the fleet's live
display streams as floating screens. It consumes the same coalesced
client-state snapshots and dispatches the same action vocabulary through the
same control-plane handlers as every other tab. **It is a renderer, never a
second brain**: no state originates in XR, no new backend endpoints exist for
it, and every trust boundary (approval routing, autonomy, IAM) is exactly the
dashboard's.

Direction (ratified 2026-08-13): the XR surface is a **careful port of the
regular UI** — its rails, its detail work, its design language — re-expressed
natively for the medium. It deliberately does *not* build on Station's
parallel constellation/HUD design (that track is frozen; see
[Station](./station.md)), and it is not flat dashboard panes floating in
space: layout follows headset ergonomics, interactions follow hands and gaze.
Since a headset browser is just another dashboard client, this adds **no new
binaries and no new trust surface** — the daemon serves the same page to a
Quest that it serves to Chrome.

## Platform reality (surveyed 2026-08-13)

| Platform | What's real |
|---|---|
| **Meta Quest 3** — Horizon Browser 146.x (Chromium 146) | immersive-vr **and** immersive-ar (passthrough) shipped; hand tracking shipped; WebXR Layers shipped. WebGPU experimental-only; **no WebXR-WebGPU binding** → XR presents through **WebGL2 only** |
| **Apple Vision Pro** — Safari 26.2 | immersive-vr shipped (no AR sessions); transient-pointer (gaze+pinch) input; **ships the WebXR-WebGPU binding** (first platform) |
| **Desktop Chrome** | WebXR via OpenXR runtimes; WebGPU binding flag-only, no ship milestone |
| **wgpu (Rust)** | No WebXR support upstream (v30); community recipe is raw JS-interop for the XR present |

These facts pick the architecture: the floor everyone gets is **WebXR +
WebGL2**, and the WebXR-WebGPU binding slots in later behind the same seams
(Vision Pro first, Quest whenever Horizon Browser ships it).

## Architecture

`crates/xr-web` (Rust → WASM, artifacts committed under `static/wasm-xr/`,
same pinned-wasm-pack pipeline as the other browser crates):

- **`webxr_sys.rs`** — hand-written WebXR externs. web-sys gates XR behind
  its unstable-APIs cfg and lacks the Layers API entirely, so the crate binds
  exactly the surface it uses. Extend here; never via a web-sys cfg flag.
- **`math.rs`** — dependency-free column-major Vec3/Mat4 (WebXR's matrix
  layout), rigid inverses, ray-from-pose, oriented-panel raycasts.
- **`gl.rs`** — the WebGL2 encoder (the universal floor): an XR-compatible
  context on a hidden canvas feeding `XRWebGLLayer`; three deliberately
  boring programs (pos+color streams, rounded-rect SDF panels drawn one
  uniform-set per panel, glyph-atlas text) plus a video program for display
  streams (per-frame `texImage2D` from the live `<video>` elements). No
  `unsafe` — buffer uploads copy through `Float32Array::from`.
- **`atlas.rs`** — the dashboard's type (Hanken Grotesk) baked at 48 px into
  an R8 texture with mips; world-space text runs with ellipsis truncation.
- **`kit.rs`** — ported ui-v2 tokens (surface/text/line/iris/status colors),
  panel/text/monitor primitives, and the ergonomic layout constants: fleet
  shelf on a 2 m cylinder, near-field workbench at ~1 m, everything
  world-locked inside a comfortable frontal arc.
- **`ui.rs`** — snapshot → scene. Session cards carry the session-window
  vocabulary (status color, label, goal line, context-pressure meter,
  approval border, recent-session dimming) grouped in host rows; the
  selected session gets the workbench (detail rows + label-fitted
  approve/deny pills); pending approvals raise a front-and-high banner;
  display streams become up to two stacked 16:9 monitors on the left.
  AR keeps your real room (just a floor ring); VR gets a calm grounding
  grid until the composed environment lands.
- **`session.rs`** — lifecycle and time: immersive-ar preferred
  (passthrough-first; Quest 3 is an MR device), immersive-vr fallback
  (Vision Pro), `local-floor` reference space with a `local` fallback that
  sinks the scene to a plausible floor, `fixedFoveation`, the
  self-rescheduling XR rAF loop, and a single 'end' cleanup seam for every
  termination path.
- **`input.rs`** — one abstraction for every input source (Quest
  controllers, Quest hands, Vision Pro transient-pointer): target ray +
  the select event family. Per-frame raycast hover; the interaction
  grammar is absolute — **quick pinch = light/reversible acts** (select,
  paging, summon/dismiss, layout toggles, thread-action pills, reopen),
  **900 ms uninterrupted pinch-hold = trust-critical or destructive
  acts** (approve/deny, interrupt, terminal open/kill, agenda complete)
  with the visible confirm fill that cancels when aim drifts. A pinch on
  a grab bar instead steers that surface along its cylinder band until
  release. `activate(name)` runs the same dispatch path for
  automation/accessibility (activation by name is the deliberate act,
  so hold-tier targets fire without the hold).
- **`terminal.rs`** — the in-scene terminal pane (below): summon pill,
  pane layout, the honest empty/warming/watching/exited states, the
  lifecycle pills (held open/restart + held end-shell, quick
  view-dismiss), and the facade seams the dashboard's terminal painter
  feeds.
- **`keyboard.rs`** — in-scene text entry (below): the focused-field
  model (`TextEntry` — field id, label, buffer, cursor, one-shot
  shift), the ray-typed QWERTY board, and the `text_commit` emit path.
- **`voice.rs`** — hold-to-talk voice input (below): the talk pill and
  the pure capture state machine; the captured transcript lands in the
  text entry's buffer for the keyboard's own confirm grammar.

### Feed and action routing

The dashboard side is one lazy fragment (`static/app/ui2-xr.js`): a header
entry chip (feature-detected; hidden on non-XR browsers), lazy WASM import on
first use, a 300 ms snapshot pump reusing the exact feed builder the other
rendered surface consumes, display-slot mirroring (same 6-arg registration
shape), and action routing straight into `handleStationAction` — approvals
emit the dashboard's existing `{type:'approval', host_id, approval_id,
decision}` shape and land in `send_approval` / `resolvePeerApproval` like
every other frontend. Two action families are consumed by the
fragment's own appended sections before that router: `text_commit`
(the text-entry routing seam — the composer path, below) and the
`voice_talk` capture verbs (the voice section, below). Deferred
deliberately: the live voice-composer *conversation* (talking WITH the
presence AI in XR — a designed seam, not a hack) and compositor media
layers (below); ray typing and hold-to-talk dictation shipped instead
as the first text-input slices.

### Terminal pane (slice 2: watching + lifecycle)

A "terminal" pill sits on the operator's right (mirroring the monitor
stack's default slot on the left); a quick pinch summons a pane that
watches the dashboard's standalone shell — the same PTY the flat
Terminal tab drives (`terminal_open`/`terminal_output` over the
dashboard-control tunnel, `terminal.rs` on the daemon). The flat tab's
machinery owns the attach and the xterm buffer; the `ui2-xr.js`
terminal section paints that buffer onto an offscreen canvas (cell
colors/SGR from the same ui-v2 token theme the flat terminal resolves)
and registers it through the facade's canvas-texture seam, so the wire
protocol is reused verbatim and no second listener is attached. The
pane label carries the PTY's real id and host ("shell-0 · This
daemon") and the status line is the flat tab's verbatim.

Lifecycle lives in-scene (owner-directed; this relaxed slice 1's
read-only-no-open stance): with no live session the pane offers an
**open terminal** (or, over an exited session, **restart shell**) pill —
a 900 ms hold with the confirm fill, because the daemon's
`open_or_attach` spawns a shell when none exists; the note beside it
says so ("hold — spawns a shell on this daemon"). Completing the hold
emits the dashboard's own `navigate → terminal/shell` action, arming
the flat machinery exactly as clicking the Terminal tab would — no new
protocol. A live session instead carries the held **end shell** pill
(a `terminal_close` frame through the existing shell-frame sender —
it kills the PTY and the pane relabels to the honest exited state),
while the quick **close** pill only dismisses the XR view: the PTY and
the flat tab's attach keep running. Spawning works where the flat
Terminal works (loopback and Operate-tier lanes); a lane that refuses
terminal frames keeps refusing — XR renders the same refusal, never a
second path.

Typing from the headset still waits for the keyboard seat (hardware
keyboards in immersive sessions are unverified on the Quest); the
"watching — input on the dashboard" line states that contract, and the
seat's entry point is already plumbed: `xrTerminalStdin(text)` in
`ui2-xr.js` routes bytes through the exact sender the flat xterm's
onData uses (queueing and re-open behavior included).

### Scene control (dismiss/summon, grab-to-move, verbs)

The room is operable, not just watchable:

- **Dismiss and summon.** The agenda rail and each monitor carry a
  small `x` pill (quick pinch); a fixed **layout strip** low in front —
  the one surface that never hides — carries four toggle pills
  (sessions / terminal / agenda / monitors) with state dots, so
  anything dismissed comes back the same way it left. Hiding
  `sessions` folds the shelf and workbench but **never the approval
  banner** — an urgent ask is not tidy-away-able. Hidden state lives in
  the engine (`debug_json.layout`) and survives snapshot ticks.
- **Grab-to-move.** The terminal pane, agenda rail, and monitor stack
  each carry a slim grab bar: pinch-hold it and the surface follows the
  ray along its cylinder band (azimuth + height, clamped to a
  comfortable range); release drops it. `ui2-xr.js` persists poses and
  hidden state to localStorage per browser and restores them before
  entry; `xrProbe.moveSurface(name, dAz, dY)` is the QA twin.
- **Session verbs.** The focused session's workbench carries a verb row
  above its top edge: **stop** when the feed says the turn is
  interruptible (900 ms hold — it stops a live turn) plus the session's
  advertised thread-action ops (**compact** / **fork**, quick pinch).
  They emit the flat surface's exact `session_action` shapes through
  `handleStationAction`, so routing, peer scoping, and refusals are the
  dashboard's own.
- **Agenda quick-verbs.** The rail's selected card carries **complete**
  (900 ms hold — semi-destructive) or, on a just-completed card,
  **reopen** (quick pinch — the undo; completed items linger briefly as
  muted done cards so the undo has a target). Both route through the
  dashboard's own `api_agenda_op` projection via `daemonApi`, and the
  rail renders honest per-op status (completing… / completed / the
  daemon's refusal text). Answer and park-with-text wait for the
  keyboard seat.

### Text input (steering a session from the room)

Text entry is a first-class substrate (`keyboard.rs`), not a bolt-on:
any affordance can open an entry bound to a **field id**, and the
committed text is one more action through the one router. The first
consumer is the workbench — a **steer** pill on the focused session's
bench (only for cards projecting a live session) opens a rendered
QWERTY board bound to `steer:<agent>`.

The board sits in a fixed near-field slot below the workbench, tilted
up toward the gaze; every key is ≥ 35 mm at ~0.9 m — the ray-typing
legibility floor. Typing is hover + **quick pinch** (release-resolved,
exactly like cards): the 900 ms deliberate-confirm hold stays
approvals-only, because a keystroke is trivially reversible where an
approval is not. Shift is one-shot (next character), the preview strip
shows the buffer with a visible cursor (arrow keys move it), cancel
drops the draft, and **send** commits.

Commit emits `{type:'text_commit', field_id, text}` through the action
callback; the `ui2-xr.js` text-entry section resolves the field and
routes it through the flat dashboard's REAL composer path — for a local
session `focusSessionWindow(sid)` + `submitComposedText(text)` (the
exact pair Station's own steer op runs), for a peer session
`setPromptTargetPeer(hostId, sid)` + `submitComposedText(text)` — so
mid-turn steer vs queued follow-up vs start_task is decided by the same
code the flat composer uses, and the one-composer-one-target rule holds
across surfaces. The router then reports the verdict back
(`textEntryResult`), and the bench renders it beside the pill —
"sending…", then "sent" (dispatched to the daemon) or the refusal text.
Nothing in the scene pretends a send worked.

Trust: this adds **no daemon routes** — commits ride the same session
Message/Task control operations the flat composer already dispatches,
so a hosted lease sees exactly the ops its projection already carries
(and approvals stay behind the action wall regardless).
`activate("steer:<agent>")` / `activate("key:<token>")` run the same
dispatch arms for automation and accessibility; `debug_json` exposes a
`textEntry` section (field, buffer length, cursor, shift, delivery
status).

### Voice input (hold-to-talk)

A "talk" pill sits below-left of the workbench (mirroring the terminal
summon pill on the right, beside the keyboard slot). It is the **third
hold semantic**, kept strictly apart from the other two: a quick pinch
selects, the 900 ms confirm-hold approves — and the talk hold
**records**. Pinch-and-hold the pill and the hold is the recording
window (a pulsing ring says so from across the room); release stops it.
The talk hold never fires on a timer and never cancels on aim drift — a
hand wanders while its owner speaks — releasing the pinch is the only
stop, so the mic can never stay hot. Releases under 300 ms read as
accidental pinches and cancel with a rendered "hold to talk" hint. Mic
permission is requested on the FIRST talk press, never at session
entry, and release always stops the capture tracks (the browser's
recording indicator goes out).

The capture rides the dashboard's **existing server-side transcription
lane** end to end: the `ui2-xr.js` voice section streams mic PCM over
the page's `user_audio` frames into the daemon's Whisper pipeline
(`transcription.rs`, `[transcription] enabled = true` — off by
default), and the transcript returns on the broadcast `user_transcript`
event. That lane only logs daemon-side; nothing injects it into any
conversation, so voice capture adds **no presence-pipeline changes and
no daemon changes**. Two lane facts shape the glue: the daemon
transcribes in fixed ~3 s windows with no flush verb, so release pads
one full window of silence to flush the spoken tail (pure-silence
windows are RMS-gated daemon-side and transcribe nothing); and if the
flat dashboard mic is already streaming (live voice session with
transcription on), no second capture opens — the section just taps the
transcripts already flowing.

The transcript is **never auto-sent** — voice is a capture lane into
the text-entry substrate above, never a second send path. With the
board open, the utterance appends at the cursor (dictate into the draft
you were typing); with it closed, the board opens bound to the focused
session's steer field carrying the transcript as its draft; with no
session focused, the pill says "select a session to dictate to".
Review, edits, and the commit all go through the keyboard's own grammar
— enter emits the same `{type:'text_commit', field_id, text}` a typed
draft emits, the same routing seam dispatches it, and the same delivery
verdict comes back. Speak, glance, pinch send.

Honesty contract: every unavailability — transcription off, hosted
Connect lane (user_audio is not tunneled), dead event stream, no secure
context, mic denied — renders as a visible status line on the pill,
never a silent no-op. `debug_json` exposes the capture machine under
`voice` ({phase, available, detail, note}), and the validator's
`--xr-probe` drives the full loop through `xrProbe.voice` with a shim
transcript — no mic or ASR in CI.

### Availability

The entry chip ships on by default for any browser reporting immersive
support (milestone 1 graduated on the owner's Quest 3, 2026-08-13);
`?xr=off` is the opt-out escape hatch.

## Testing

### On the Quest 3 (the canonical device loop)

WebXR requires a secure context, and the dashboard beyond loopback is an
mTLS wall the headset browser can't client-cert through. The standard
developer loop sidesteps both:

1. Enable developer mode on the headset (Meta Horizon phone app), plug in
   USB-C, accept the debug prompt.
2. `adb reverse tcp:8765 tcp:8765` (after `adb devices` shows the headset;
   works over Wi-Fi adb after a one-time USB bootstrap).
3. In Horizon Browser: `http://localhost:8765/` — localhost is a
   secure context, and the daemon sees a loopback client (local-presence
   lane; no cert ceremony).
4. Tap the **XR** chip in the header. Passthrough is the default premise —
   your room, plus the fleet.
5. Remote DevTools via `chrome://inspect` on a desktop Chrome over the same
   adb link.

### The no-adb lane: hosted leases (watch mode)

When this daemon has the hosted-control opt-in enabled
(`[connect] hosted_control_enabled`, see
[Hosted control](./hosted-control.md)), the headset needs no developer
mode at all: open the daemon's fleet name (or Connect) in Horizon
Browser — ordinary WebPKI HTTPS, so a secure context with no
client-cert problem — ring the doorbell, approve the lease from a
trusted surface, and enter XR.

Know what that buys, honestly: **over a hosted lease the XR room fully
renders but stays watch-mode.** The state feed and session events clear
the lease's outbound projection, and display streams flow at the `View`
preset — the shelf, workbench, banner, and monitors all live. The
approval verb does not: the action wall refuses `approve`/`deny` (and
every trust-critical verb) from hosted surfaces **at every preset**, by
design — approving stays on surfaces the daemon trusts directly. The
contract is pinned by
`xr_over_hosted_lease_watches_everything_approves_nothing` in
`access/hosted_control/policy.rs`; widening it is a deliberate policy
change that must move the pin and this paragraph together.

So the two lanes divide cleanly: the **adb/loopback lane** is
full-powers XR (the headset is a trusted local surface — pinch-hold
approvals work, and so do the scene verbs: interrupt, thread actions,
terminal lifecycle, agenda ops); the **lease lane** is zero-setup
supervision (watch the fleet, watch the screens, see approvals pending —
and resolve them from your desk, phone, or any trusted surface). The
scene verbs never widen a wall: each rides an existing dashboard
route/action, so whatever a lane's projection or the action wall
refuses stays refused, and XR renders the refusal (the agenda rail's op
status line, the terminal status line) instead of inventing a second
path. Scene-local control — dismiss/summon, grab-to-move, the layout
strip — works on every lane; it touches no daemon.

### Without a headset

`scripts/validate-dashboard.cjs --xr-probe` injects a deterministic WebXR
shim (no dependency; it covers exactly the interface surface
`webxr_sys.rs` binds) into headless Chromium and drives the real path end to
end: chip → immersive entry → stereo frame loop (2 views) →
synthetic-snapshot scene build → activation-by-name selection → a captured
approval dispatch asserted against the dashboard's action shape (captured,
never routed to a live daemon) → transcript paging → the text-entry pass
(steer pill → ray-typed keys → a captured `text_commit` asserted
field-and-text, plus the honest "sending" park under captureOnly and a
clean cancel) → the voice pass (talk toggle → captured capture verbs → a
shim transcript through the real `voiceResult` seam landing in the
text-entry buffer → the keyboard's enter committing it as a captured
`text_commit` → failure/unavailability honesty; no mic, no ASR) → the
terminal pane pass (summon → honest empty state → canvas-seam
registration → dismiss) → the scene-verbs pass: captured
interrupt/thread-action shapes off the workbench, the terminal open
affordance captured as the navigate action (no PTY is ever spawned by
the probe) and the held kill's intent, layout strip hide/show asserted
through `debug_json`'s hidden set and panel deltas, grab-move nudges
with band clamps via `xrProbe.moveSurface`, and the agenda
complete/reopen captures. The agenda rail leg rides the same snapshot
(three cards including a done one). `xrProbe` (the
`stationProbe`-convention QA facade) and `debugJson()` expose
engine/scene state for ad-hoc probing.

## Roadmap

- **M2 — the feel:** compositor video via `XRMediaBinding` quad layers where
  the Layers module exists (Quest's most optimized path; requires moving the
  scene to `XRWebGLBinding` projection layers — the current in-scene textured
  quads remain the universal fallback), the live voice-composer
  *conversation* (talking WITH the presence AI in XR; hold-to-talk text
  input shipped as the first slice), spatialized per-session
  presence audio, hand-tracking polish, multi-monitor walls, comfort and
  legibility tuning on hardware.
- **M3 — the place:** hosts as arrangeable zones, the timeline as a spatial
  structure, a composed VR environment, the WebXR-WebGPU binding encoder
  behind the existing seam (Vision Pro first-class), deeper admin surfaces.
- **Scope stance:** XR v1 is the operator loop, deep — watch, select,
  approve, monitor. Administration (settings, IAM tables, plugin management)
  stays on the 2D dashboard, which a headset can still open as a flat browser
  window at any time.

## DOM-overlay spike (`?xr=overlay`)

Flag-gated experiment toward full-richness XR: entering with `?xr=overlay`
requests the WebXR DOM Overlay module (an *optional* feature — unlike
`layers` it coexists with `renderState.baseLayer`) with the entire dashboard
body as the overlay root. Where the runtime grants it (Quest's Horizon
Browser ships the module; desktop Chrome and the probe shim don't), the
regular UI composites as a live, interactive DOM layer over the spatial
scene — composer, tabs, and (to be verified on-device) the system keyboard
on focused text fields. The status line beside the chip reports the grant
verdict at entry, and `debugJson().overlay` carries `{requested, active}`.
Open on-device questions: input routing quality between overlay DOM and
scene ray targets (`beforexrselect` gating), keyboard summon inside the
immersive session, and legibility of the composited page. If the spike
holds up, "overlay mode" becomes the bridge that keeps every dashboard
capability reachable in-headset while native spatial surfaces mature.

## Relationship to Station

Station remains the 2D rendered-canvas tab (see [Station](./station.md));
its parallel constellation/HUD design is **not** the XR direction and
receives no new investment as of 2026-08-13. What XR reuses from it are the
proven *patterns* — the snapshot-feed contract, the `debug_json`/`activate`
QA conventions, the glyph-atlas technique — not the experience. Whether the
constellation eventually retires is a separate product decision.
