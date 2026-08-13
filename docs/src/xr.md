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
  the select event family. Per-frame raycast hover; cards select on
  release; **approve/deny require a 900 ms uninterrupted pinch-hold** that
  visibly fills the pill and cancels when aim drifts — a trust-critical
  action never misfires off a stray pinch. `activate(name)` runs the same
  dispatch path for automation/accessibility (activation by name is the
  deliberate act).

### Feed and action routing

The dashboard side is one lazy fragment (`static/app/ui2-xr.js`): a header
entry chip (feature-detected; hidden on non-XR browsers), lazy WASM import on
first use, a 300 ms snapshot pump reusing the exact feed builder the other
rendered surface consumes, display-slot mirroring (same 6-arg registration
shape), and action routing straight into `handleStationAction` — approvals
emit the dashboard's existing `{type:'approval', host_id, approval_id,
decision}` shape and land in `send_approval` / `resolvePeerApproval` like
every other frontend. Deferred deliberately: the voice composer toggle (no
existing action seam to reuse; needs a designed one, not a hack) and
compositor media layers (below).

### Dev gate

Until milestone 1 is validated on hardware, the entry chip is gated behind
`?xr=dev` (same convention as `?station_panes=on`). The gate drops once the
Quest pass confirms the experience.

## Testing

### On the Quest 3 (the canonical device loop)

WebXR requires a secure context, and the dashboard beyond loopback is an
mTLS wall the headset browser can't client-cert through. The standard
developer loop sidesteps both:

1. Enable developer mode on the headset (Meta Horizon phone app), plug in
   USB-C, accept the debug prompt.
2. `adb reverse tcp:8765 tcp:8765` (after `adb devices` shows the headset;
   works over Wi-Fi adb after a one-time USB bootstrap).
3. In Horizon Browser: `http://localhost:8765/?xr=dev` — localhost is a
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
approvals work); the **lease lane** is zero-setup supervision (watch the
fleet, watch the screens, see approvals pending — and resolve them from
your desk, phone, or any trusted surface).

### Without a headset

`scripts/validate-dashboard.cjs --xr-probe` injects a deterministic WebXR
shim (no dependency; it covers exactly the interface surface
`webxr_sys.rs` binds) into headless Chromium and drives the real path end to
end: chip → immersive entry → stereo frame loop (2 views) →
synthetic-snapshot scene build → activation-by-name selection → a captured
approval dispatch asserted against the dashboard's action shape (captured,
never routed to a live daemon). `xrProbe` (the `stationProbe`-convention QA
facade) and `debugJson()` expose engine/scene state for ad-hoc probing.

## Roadmap

- **M2 — the feel:** compositor video via `XRMediaBinding` quad layers where
  the Layers module exists (Quest's most optimized path; requires moving the
  scene to `XRWebGLBinding` projection layers — the current in-scene textured
  quads remain the universal fallback), the designed voice-composer seam
  (talking to your agents is the native XR composer), spatialized per-session
  presence audio, hand-tracking polish, multi-monitor walls, comfort and
  legibility tuning on hardware.
- **M3 — the place:** hosts as arrangeable zones, the timeline as a spatial
  structure, a composed VR environment, the WebXR-WebGPU binding encoder
  behind the existing seam (Vision Pro first-class), deeper admin surfaces.
- **Scope stance:** XR v1 is the operator loop, deep — watch, select,
  approve, monitor. Administration (settings, IAM tables, plugin management)
  stays on the 2D dashboard, which a headset can still open as a flat browser
  window at any time.

## Relationship to Station

Station remains the 2D rendered-canvas tab (see [Station](./station.md));
its parallel constellation/HUD design is **not** the XR direction and
receives no new investment as of 2026-08-13. What XR reuses from it are the
proven *patterns* — the snapshot-feed contract, the `debug_json`/`activate`
QA conventions, the glyph-atlas technique — not the experience. Whether the
constellation eventually retires is a separate product decision.
