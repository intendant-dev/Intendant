# Station

Station is the web dashboard's rendered control center — a single-canvas,
WASM-drawn operational surface for everything the classic tabs do: watch
agents, approve, prompt, launch, steer, and administer sessions across every
connected host. It is **the designated successor to the classic
Activity → Logs DOM surface** as the canonical way to operate agents. The DOM
surface stays as the legacy fallback — kept working and bug-fixed, but no
longer the investment target: new operational UX lands Station-first, or in
both when that is cheap.

This chapter records both the implementation as it exists and the roadmap,
because Station is mid-flight: today it is a stylized 3D constellation
*backdrop* with the real UI painted as 2D heads-up panels on top of it; the
destination is a fully immersive 3D environment whose action panes live *in*
the scene — and, beyond that, real spatial computing on XR devices
(Apple Vision Pro-class) via WebXR.

## Architecture today

*(surveyed 2026-07-04 @ `d590ad94` — trust the source when they disagree)*

Two stacked canvases plus a deliberately tiny set of DOM elements:

- **Scene canvas** — drawn by `crates/station-web` (Rust → WASM). wgpu
  pinned to the browser WebGPU backend; a Canvas-2D wireframe fallback
  engages automatically when WebGPU is unavailable (or forced with
  `?station_gpu=canvas`). The "3D" is CPU-projected: camera math lives in
  Rust (`scene.rs`), the WGSL shader is a passthrough over pre-projected
  vertices, there is no depth buffer, and everything is alpha-blended
  wireframe line/triangle lists. Scene contents: an operator core at the
  origin, one node per connected host orbiting it, agent nodes orbiting
  their host, approval-glow and token-budget rings, event sparks, a
  starfield, and a ground grid.
- **HUD canvas** — the entire interactive UI, painted per-frame by `hud.rs`:
  header band, command deck, the nine orbital "system" targets (activity /
  context / managed / controls / sessions / peers / changes / worktrees /
  view), scrollable per-domain panels, focus-detail panels (agent / host /
  view), transcript & diff viewer, activity runway, and the composer chrome.
  Every clickable region pushes a hit-zone rect; `input.rs` dispatches
  pointer/wheel/keyboard input against those zones plus 3D node picking,
  orbit/zoom camera control, pinch, and device-orientation / pointer-tilt
  parallax.
- **DOM, kept minimal** — peer display chips above the canvas, a lower-left
  status chip, an invisible hotspot-button layer mirroring scene targets for
  keyboard / screen-reader / automation access, one transparent `<textarea>`
  positioned over the canvas-drawn composer slot (the only real text
  editing), and an off-screen holder for the WebRTC `<video>` elements the
  renderer paints as live display thumbnails anchored to host nodes.

There are no Station-specific backend endpoints: `app.html` coalesces the
dashboard's existing client state into a `StationSnapshot` (~300 ms batching)
and hands it to `station.update_snapshot()`. Transcripts arrive lazily from
`/api/session/{id}` via `set_transcript`; live video via
`register_display_source`. Actions emitted by the renderer route through
`handleStationAction` into the **same control-plane messages as the classic
tabs** — approving, prompting, launching, stopping, or reconfiguring from
Station behaves exactly like doing it from Activity. This is a design
invariant: Station is a *renderer* over the one control plane, never a
second brain.

QA lives in `scripts/validate-dashboard.cjs`: render-health probes
(fps / frame pacing / webgpu / debug-json), composer workflow and
interaction probes, and state assertions (`--require-station-state`,
`--require-ai-provider-session`, `--require-external-agent <backend>` — the
backend argument is generic, not Codex-only).

## Where it actually stands

Working end-to-end from inside the canvas: approvals (approve/deny on the
focused agent), the composer (prompt or steer the target, launch new
sessions with a backend picker), session lifecycle (focus, resume, attach,
stop, halt, fork, transcript, copy), the controls panel (autonomy, backend
selection, mic/cam/display, browser workspaces, recordings, Codex runtime
options), managed-context operations (seed/rewind/backout/records), context
replay, changes/diff, peer/display lanes, and view settings
(orbital/constellation layout, mood, fov / motion / AR / density).

Known seams — the honest gap between the vision and the pixels:

- **Sessions are not in the scene.** The constellation renders one agent
  node for the local daemon, one per peer daemon, and synthetic approval
  nodes — real sessions, sub-agents, and forks never become 3D entities.
  The parent/child edge machinery in `scene.rs` exists but is exercised
  only by the approval nodes.
- **Goals are plumbed, not rendered.** `StationSnapshot` carries per-session
  goal fields end-to-end; the renderer never reads them.
- **The scene is a backdrop.** All operational UI is screen-space 2D HUD
  paint; nothing interactive lives in world space yet.
- **Codex is the only backend with a rendered runtime block** in the
  controls panel (approval policy, managed-context mode, fork-binary
  warning). Claude Code is a selectable label with no options block, even
  though its runtime config (model / permission mode / allowed tools)
  already exists in the control plane.
- **Wireframe-only rendering** (no depth buffer or shading), plus a stack of
  WebGPU-reliability fallbacks (auto Canvas-2D, scene-on-HUD underlay, a
  liveness watchdog) that reflect real-world driver flakiness.

## Roadmap

The direction, in dependency order. Phases A and B are near-term and
concrete; C and D set the trajectory.

### Phase A — backend parity through the universal rails

The per-session operational features (goal chips, per-window action menus,
relationship wiring) were built against Codex first. The transports are
already backend-neutral; Claude Code — and any future backend, including
native sessions — catches up by *producing into those rails*, not by growing
parallel `claude_*` UI paths. The concrete matrix and catch-up order live in
[Dashboard and Station parity](./external-agent-orchestration.md#dashboard-and-station-parity-codex-vs-claude-code).

### Phase B — the session graph becomes real

Project real sessions into the scene: one node per live/recent session,
orbiting its host, wired to its parent by the existing
`session_relationship` data (sub-agent / fork / side), ringed by context
pressure, glowing on pending approval. The latent edge machinery lights up;
goal state renders on the agent focus panel and the command deck; per-node
action menus reach parity with the session-window kebab. At the end of
Phase B, Station shows the *actual* multi-agent topology — the constellation
stops being a metaphor.

### Phase C — panes move into the scene

Migrate the HUD from screen-space paint to world-space surfaces: panels
become billboarded or gently curved quads anchored near the nodes they
describe, with real depth (depth buffer, occlusion), in-scene text
rendering, and raycast picking. Screen-space HUD remains as the
compact/fallback presentation (small viewports, the Canvas-2D fallback,
accessibility). This is the "full immersive 3D experience" milestone: the
scene is no longer a backdrop behind the UI — the scene *is* the UI,
spatially.

### Phase D — XR spatial computing

WebXR immersive sessions over the same scene graph: head-tracked cameras,
hand / gaze / pointer input mapped onto the existing hit-testing, panes as
floating spatial surfaces around the operator. Target devices are Apple
Vision Pro-class headsets (visionOS Safari exposes WebXR with
transient-pointer input) plus generic WebXR runtimes. The 2D dashboard
remains fully supported — XR is an additional presentation of the same
control plane, subject to the same trust model and approval routing as
every other frontend.

## Relationship to Activity → Logs

The classic DOM surface (session windows, log stream, control pane) remains
the legacy fallback: it must keep working — it is the accessibility floor,
the low-GPU path, and the surface most automation drives today — but new
operational UX should land Station-first, or in both when cheap. When
behavior is added to either surface, prefer routing it through
control-plane messages and universal events so the other surface (and the
TUI, MCP, and voice frontends) inherits it for free.
