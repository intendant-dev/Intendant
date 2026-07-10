# Web Dashboard

The web dashboard is Intendant's **default frontend**. It is a single-page app
served by the controller's built-in HTTP/WebSocket gateway, running entirely in
the browser with WASM-powered state management (the `presence-web` crate,
mobile-responsive). Since the design-overhaul flip the default look is the
**v2 chrome** (Iris accent: left navigation rail, oversight bar, ⌘K command
palette, bottom composer). Dark is the default theme; a **light theme** ships
alongside it (Settings → Appearance, or the ⌘K theme toggle — browser-scoped,
persisted per browser). The previous Catppuccin Mocha generation and its
`?ui=v1` escape hatch were deleted after the soak period. The SPA is served as one
self-contained file, `static/app.html` — a **generated artifact**: `build.rs`
assembles it from the ordered fragments in `static/app/` (`manifest.txt` fixes
the order) via `crates/app-html-assembler`, and CI rejects any drift between
fragments and artifact. Edit the fragments, never the artifact; see
`static/app/README.md`. For iteration, `INTENDANT_APP_HTML_PATH=<file>` makes
the gateway re-read a disk copy of app.html on every request instead of
serving the embedded one (see [Configuration](./configuration.md)) — edit a
fragment, run the assembler, refresh.

## On by default

There is no opt-in: the gateway starts automatically unless you pass `--no-web`,
`--mcp`, or `--json` (those modes own stdio / are headless by contract). The
`--web` flag simply forces it on and optionally sets the port.

```bash
./target/release/intendant "task"          # dashboard comes up; URL is printed
./target/release/intendant --web            # explicitly enable
./target/release/intendant --web 9000       # explicit port
./target/release/intendant --no-web "task"  # disable; headless single round
```

The server binds port **8765** by default, auto-incrementing through 8785 if it
is busy; the chosen port is printed at startup. With the default mTLS transport,
open `https://<host>:<port>/` in a browser after running
`intendant access setup` and enrolling that browser/device. Use
`--bind 127.0.0.1` when starting plaintext local/debug dashboards with
`--no-tls`.

> **Correction vs. older docs:** `--web` is the default and no longer "implies
> `--mcp`". Earlier docs described `--web` as opt-in and tied to MCP mode —
> neither is true now.

## Secure Browser Contexts

The dashboard shell, Activity log, Sessions, Settings, and basic display viewing
can run over ordinary HTTP. Some browser capabilities are different: browsers
expose them only to a **secure context**.

Use a secure dashboard context when you need:

- **Station WebGPU rendering** (`navigator.gpu`) — otherwise Station falls back
  to its canvas-2D WASM renderer.
- **Microphone and camera** (`navigator.mediaDevices`, `getUserMedia`) for live
  voice, browser-side audio/video capture, or camera recording.
- **Screen/window capture from the browser** (`getDisplayMedia`) when a browser
  is the capture source.
- **Privileged browser APIs** such as the async clipboard in stricter browsers.

Practical rules:

- `https://` with a trusted certificate is the normal secure context for remote
  browsers.
- `http://localhost` and `http://127.0.0.1` are treated as secure by most
  desktop browsers, but not by every embedding. In particular, the macOS
  `WKWebView` wrapper uses the custom `intendant://` scheme because
  `http://localhost` there does not expose media devices.
- `http://<host-ip>` is not a secure context. Use default native mTLS,
  `--tls` with a trusted certificate, the macOS app wrapper, or another trusted
  HTTPS reverse proxy. The macOS app wrapper starts its bundled backend with
  mTLS by default and fails closed with setup guidance when access certs are
  missing.
- Clicking through a self-signed certificate warning is not a reliable substitute
  for installing/trusting the certificate; browsers may still withhold secure
  APIs.

### Headless daemon posture

The controller always runs headless and tees its stdout/stderr to `daemon.log`
in the session directory (so the dashboard's "Download session report" can
include controller output). With no task argument the agent starts idle and
waits for tasks submitted from the dashboard; with a task argument it runs the
task as the foreground session under the same gateway, then falls through to
the idle daemon loop.

## Tabs

The v2 chrome groups eleven destinations in the left navigation rail —
**Activity** and **Sessions** (Work), **Live display** and **Station** (Watch),
**Terminal** and **Files** (Machine), **Usage** (Insight), **Access** and
**Vault** (Trust), **Settings** and **Debug** (System). The oversight bar on
top carries the phase pill, stop control, context meter, transport state, the
Activity Focus/Grid layout toggle, and the ⌘K command palette; the composer —
the global task input — docks at the bottom and reaches the daemon from any
destination. New events arriving while you are elsewhere raise a badge on the
rail item.

The section headings below keep the internal pane names (`Video` is the Live
display destination, `Stats` is Usage) — ids, routes, and deep links are
unchanged by the redesign.

### One design language, bounded DOM

Every tab speaks the design language introduced with the Access redesign:
glass cards with a title + one-line explainer, chips for status (a chip
always carries its label — never color alone), warm-scale badges for
authority, folds for power-user surfaces, and real empty states. The
shared component classes are `ui-*` (grouped-selector aliases of the
Access `acc-*` rules — one source of truth); chart and heatmap colors come
from the validated `--viz-*` ramp tokens in `:root`. Don't improvise new
button/chip styles or viz colors — reuse these.

Two invariants keep a long-lived dashboard responsive:

- **Hidden panes don't render.** High-frequency events (transport status
  ticks, `update_usage`, session refreshes, transfer progress) route
  through `renderOrDefer(tab, key, fn)`: visible panes render immediately;
  hidden panes remember only the latest render thunk per key and run it
  once on the next pane entry (`switchTab` flush / `visibilitychange`).
  Data merging still happens eagerly — only DOM work is deferred.
- **Client state is bounded.** The live log DOM is capped (10k entries,
  pruned with scroll compensation), session-window histories cap at 5000
  items (older entries stay reachable via remote paging; replay dedup
  survives the trim), finished file transfers prune beyond 100, and
  per-display metric cards retire with their display.

The log stream manages scroll position manually (`overflow-anchor: none`):
when you scroll up to read, appends and prunes never move your view — new
entries count on a bottom-center jump-to-live pill instead. QA harnesses
can probe pane/render state and drive the real log append pipeline via
`window.__intendantPaneDiag` (the app script is module-scoped, so probes
cannot reach bindings by name).

### QA probes (`window.qa`)

The SPA is one `<script type="module">`, so harnesses evaluating in the
page's main world cannot reach module bindings by name. Fragments that have
QA-relevant state export it on the shared readback namespace, declared next
to the state it reads:

```js
window.qa = Object.assign(window.qa || {}, {
  sessionsHydration: () => ({ seen, unresolved, inFlight }),
});
```

Each entry is a cheap, side-effect-free function returning a
JSON-serializable snapshot. Current probes: `qa.sessionsHydration()`
(sessions-tab relationship-hydration termination, `40-session-launch.js`),
`qa.sessionsFuel()` (new-session credential preflight, `55-files-ide.js`),
and `qa.station()` — a pointer to `window.stationProbe`, which predates the
namespace and keeps its legacy name (the validator's `--station-*` probes
and smoke skills depend on it). `window.__intendantPaneDiag` above is the
other legacy surface.

`scripts/validate-dashboard.cjs` reads probes back without a bespoke sink
via the repeatable `--probe-json EXPR` flag (optional `label=EXPR` form):
after the checks pass — and on failure, before exit, so you see the state
that failed — it evaluates each expression in the page and prints one
`probe <label> = <compact JSON>` line to stdout (thrown errors print as
`{"error":...}`; ~4KB cap per probe):

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:<port>/#sessions" \
  --wait-for-function "(() => typeof window.qa === 'object')()" \
  --probe-json "fuel=window.qa.sessionsFuel()" \
  --probe-json "window.qa.sessionsHydration()"
```

### Activity

The default tab, and the classic DOM control surface. It remains fully
supported as the legacy fallback (the accessibility floor, the low-GPU path,
and the surface most automation drives today), but [Station](./station.md)
is the designated canonical control surface going forward.

Five subtabs:

- **Log** — a scrollable, color-coded event stream of everything in the system,
  grouped by turn with visual separators, with a verbosity selector
  (Normal/Verbose). Event sources are color-coded:
  - **system** — session lifecycle, approvals, context management
  - **worker** — model responses, reasoning summaries, task completion
  - **agent** — command execution output (stdout/stderr, exit codes)
  - **live** — voice transcripts, presence lifecycle, tool requests
  - **server** — presence model internals (thinking, tool calls)

  Under v2 the Log pane has two layouts, toggled from the oversight bar and
  persisted per browser: **Focus** (the default) shows the combined stream as
  one centered timeline with role eyebrows, and on wide viewports a vitals
  rail for the foreground session (working tree, context budget, prompt
  cache, rate limits, changes); **Grid** shows the classic per-session
  window grid with relationship wires and the concurrent stream below.

  The Log pane also carries the approval card. **Approve** clears the
  pending command once; **Approve all like this** sets that approval
  category's rule to `auto` (the shipped per-category machinery, scoped and
  revocable in Settings) and then approves; **Switch to Full autonomy** is
  the old approve-all — it lifts every gate, and is labeled for what it is.
  Skip and Deny complete the set (`y` / `a` / `s` / `n`). A follow-up text
  input sends a message after a round completes.

  Pending requests also escalate beyond the open tab (the **attention
  center**, `static/app/57-attention-notifications.js`): every pending
  approval/question across sessions counts into a `(N)` document-title
  prefix and favicon badge (on by default; toggle under Settings →
  Appearance → Notifications), and — strictly opt-in from the same
  card, which is the only place notification permission is ever
  requested — a browser Notification fires when a request arrives while
  the tab is hidden; clicking it focuses the tab and the owning session.
  The badge clears as requests resolve and drops on stream disconnect
  (the reconnect bootstrap rebuilds what still stands). For closed tabs
  entirely, the daemon nudges the Connect rendezvous and opted-in
  browsers get a Web Push (see `self-hosted-rendezvous.md` —
  Notifications; payloads never carry work content).
- **Context** — the agent's current working context (what it is operating on).
- **Managed** — operator console for managed-Codex context maintenance (see
  below).
- **Changes** — file changes / diffs produced during the session (with its own
  badge when new changes land).
- **Control** — direct controls for steering the run.

Session-window headers carry a **vitals chip** (the operator-statusline
port, backend-neutral): a git segment for the session's working tree
(`⎇ branch ●dirty +ahead/−behind ✓|⚠ merge-parity ⇡unpushed`, fetch-free,
conflict-tinted when a merge with the primary would conflict); a
prompt-cache segment — `⚡NN%` hit share of the latest request (green ≥90,
yellow ≥50) plus a live `⏳m:ss` TTL countdown where the provider's cache
TTL is known (Anthropic 5m/1h; hidden for OpenAI whose TTL is
undocumented), dimming to `✗` once cold; and a rate-limit gauge —
`▮NN% 5h` for the most-used window (Claude Code subscription 5h/7d,
Codex primary/secondary, native Anthropic per-minute headers), dim below
70%, yellow from 70%, red from 90% with the reset countdown appended;
the tooltip lists every window. When a cache countdown enters its final
minute the dashboard raises one toast per idle period (plus a browser
notification if permission was already granted and the tab is hidden) —
early enough that a follow-up still reuses the warm cache, never after the
fact. Sections appear as producers fill them; the chip hides in narrow
windows. Station's agent focus panel shows the same vitals as git /
cache / limits rows.

#### Managed (Activity → Managed)

The manual counterpart to the model-driven managed-context tools. A session
picker lists Codex-like sessions — live windows plus historical sessions from
the session store — sorted prompt-target first, then managed-mode, live, and
most recently updated (labels show name, short id, source, and `via <id>` when
the Codex thread is reached through an Intendant wrapper session). **Use
target** snaps back to the current prompt target.

For a live session the pane calls the per-session MCP `get_status` and renders
a density card: managed/vanilla mode, pressure status, effective and hard-limit
token usage with a colored pressure bar, the soft rewind-at threshold, and
whether rewind-only gating is active. When the verified dashboard-control
tunnel is connected, these dashboard-originated MCP `tools/call` requests use
`api_mcp_tool_call`; otherwise they fall back to `/mcp?session_id=...`.
Historical sessions show `historical` status — records and anchors stay
readable, but live actions are disabled.
Alerts flag non-Codex selections, sessions without managed mode, an
insufficient last rewind, and a configured Codex command that doesn't look like
the patched managed build.

- **Rewind** — manual `rewind_context` dispatch with an exact item anchor
  (`call_id` or response item id) plus anchor side (`before`/`after`), a
  required reason and carry-forward primer, and optional preserve / discard /
  artifacts / next-steps lists (one entry per line). **Inspect anchor** runs
  `inspect_rewind_anchor` to show a small window around the candidate before
  committing.
- **Recent anchors** — harvested from the live activity log and the
  `/api/managed-context/anchors` history, each with a one-click **Use** that
  fills the anchor field (switching the picker to the anchor's session if
  needed).
- **Records / Backout** — the session's rewind records from
  `/api/managed-context/records`; clicking one shows its JSON and fills the
  backout form, which runs `rewind_backout` in `inspect`, `restore`, `fork`,
  or `backout` mode with an optional fork name.
- **Lineage and fission** — the ledger card. Lineage groups come from the live
  `get_status` payload; fission groups come from
  `GET /api/managed-context/fission`, the merged ledger + extension view that
  works for historical sessions too (live-status `fission_ledger` groups are
  only a fallback when the endpoint has nothing yet). A fission group row
  shows the group id, its anchoring tool (`fission_spawn`, or
  `fission_spawn:head` when the spawn fell back to the catalog head), the
  spawn anchor item id, the canonical session (`--` when unclaimed), and — for
  severed groups — a **detached** chip carrying the detach time and reason.
  Each branch row carries a status chip colored by the ledger's canonical
  status vocabulary (`running` / `blocked` / `completed` / `failed` /
  `detached` / `cancelled`; legacy raw values fold the same way the ledger
  normalizes them), a **canonical** chip on the claimed branch, an
  **imported** chip once the branch result was imported, a changed-file
  count, the branch charter (objective, write scope, worktree path), and its
  latest summary. (For the fission model itself — charters, worktrees,
  detach-on-rewind — see
  [External-Agent Orchestration](./external-agent-orchestration.md).)
- **Per-branch fission actions** — **Wait** / **Import** / **Cancel** /
  **Detach** run `fission_control` against the selected session. Wait uses a
  60 s window, and a `still_running` result is surfaced as an info toast, not
  an error; import, cancel, and detach ask for confirmation first, with
  cancel and detach styled as destructive. **Claim** calls
  `claim_fission_canonical`, passing the group's current canonical id as the
  compare-and-swap guard when one exists.
- **Spawn fission branches** — the spawn form above the ledger list: one to
  four branch rows (objective required; optional comma-separated write scope
  and display name; **Add branch** adds rows, each row has a remove control,
  and the last row is always kept) plus a tri-state worktree select —
  `default` omits `use_worktree` so write-scoped branches in a git project
  get isolated worktrees, while `on`/`off` force it either way — submitted as
  a single `fission_spawn` call for the selected session.
- **Copy status JSON** copies the raw status payload.

Rewind, backout, inspect, and fission spawn stay disabled unless the selected
session is live and effectively managed. The pane refreshes when the Managed
subtab is opened and re-schedules itself (only while the subtab is active)
after each pane action, thread-action result, session start, and usage update.

### Stats

Token-consumption and cost tracking:

- A KPI tile row up top: today / this-week / all-time cost, lifetime
  tokens, active days (skeleton tiles while session stats stream in)
- Per-model breakdown for the main and presence models (prompt, completion, and
  cached token counts), with a token-pressure meter per card
- Cost estimates from a built-in pricing table (OpenAI, Anthropic, Gemini)
- Token activity: a daily skyline and a GitHub-style year heatmap on the
  validated single-hue `--viz-*` ramp, filterable by agent and period
- All-sessions cumulative usage and disk usage
- Display-transport metrics (frame rate, encode latency, bandwidth per display)

### Terminal

An embedded xterm.js terminal hosting an interactive **Shell** session on the
daemon (or a selected peer). Session monitoring and control live in the
Activity/Station tabs, not here.

### Video

WebRTC display viewers for the agent's graphical displays, with interactive
control (see [Display Pipeline](./display-pipeline.md)):

- **View mode** (default) — watch the agent's display in real time
- **Take Control** — forward mouse and keyboard events to the agent's display
- **Release** — relinquish control, with an optional note
- **Display picker** — choose which monitor to view when several are present
- **Recording replay** — browse and play back recorded sessions with timeline
  seeking and speed control (1x / 2x / 4x)

The live rail's **Your screen** card keeps the three screen-on-the-wire
concepts separate (see
[Computer Use](./computer-use-and-audio.md#three-separate-concepts-private-view-agent-share-presence-streaming)):

- **View this machine** — a private remote view: watch and control this
  machine's display from the dashboard. The agent cannot see it — the
  session is `agent_visible = false` and every agent-facing display
  lookup skips it. The tile wears a **Private view** chip and the live
  rail row a `PRIVATE` tag.
- **Share with agent** — the classic `DisplayControl` grant: the screen
  becomes visible to the agent for computer-use tasks until revoked. The
  tile wears an **Agent can see this** chip. Sharing while a private
  view is active upgrades it in place; the reverse never happens
  implicitly.
- The tile's **Stream** button is the third, unrelated control: frames
  to the live presence (voice) model only.

Both modes revoke from the same card (**Stop viewing** / **Revoke
access**), and
`GET /api/displays` annotates entries with `capture_active` +
`agent_visible` so pickers and chips can render live state.

Displays appear automatically when the agent's first command triggers Xvfb
auto-launch, or when access to the user's real session display is granted.
WebRTC negotiation (SDP offer/answer + ICE candidates) is multiplexed over the
existing dashboard WebSocket. When the verified dashboard-control DataChannel is
connected, local display input authority requests and keyboard/mouse input can
use that daemon-scoped control tunnel; video media still flows through the
per-display WebRTC session.

- **New virtual display** (empty state + display picker) — create a virtual
  display keylessly: the daemon launches an Xvfb through the same machinery
  agent sessions use, registers a capture session, and every connected
  dashboard gets a streaming tile (`create_virtual_display`, gated as
  `display.input`, on both the `/ws` and dashboard-control transports). This
  is how a freshly claimed headless box — no display server, no API key —
  gets a working display; agents can then target it for computer use. The
  created display is daemon-owned: it never touches the "Your display"
  opt-in, and it is destroyed (Xvfb killed) when any dashboard closes its
  tile; after a hard daemon kill it is reclaimed like any orphaned agent
  Xvfb on the next allocation. Xvfb is Linux-only; on macOS/Windows the
  button reports a clear error and "Your display" streams the real desktop
  instead.

### Station

An immersive WASM-rendered control center for the same operational surfaces as
the rest of the dashboard — Activity, Context, Managed context, Changes,
Sessions, Peers/displays, and Control. The `station-web` crate draws the whole
scene into a single canvas: WebGPU when the browser exposes it (a secure
context is required — see Secure Browser Contexts above), with a canvas-2D
WASM fallback used automatically when WebGPU is unavailable or forced with
`?station_gpu=canvas`. The renderer runs on `requestAnimationFrame` and
re-renders only when state or view input changes, so an idle Station stays
cheap.

There is no DOM dock: the rendered scene is the UI. An invisible hotspot
overlay mirrors the scene's interactive elements so they stay reachable from
the keyboard. Station actions dispatch through the same control plane as the
classic tabs, so anything triggered from Station behaves exactly like its
canonical dashboard equivalent. View settings shape the scene: layout
(`orbital` / `constellation`), mood (`calm` / `cockpit`), and fov, motion, ar,
and density tuning.

Station is the designated successor to the classic Activity surface as the
canonical way to operate agents; the DOM Logs view remains the legacy
fallback. Today the scene is a 3D constellation backdrop with the working UI
painted as screen-space HUD panels; the destination is action panes living
*in* the scene, and eventually WebXR spatial computing. The dedicated
[Station](./station.md) chapter carries the architecture, an honest
current-state inventory, and the roadmap.

### Sessions

A browser of past and current sessions. Four subtabs:

Listing is fast by construction: per-session summaries (tokens, costs,
day buckets, disk sizes) are persisted under
`~/.intendant/cache/session_index/` keyed by exact file fingerprints
(length/mtime/ctime/dev/inode), so a daemon restart re-parses only
sessions that actually changed instead of every log in every store. The
daemon warms the list at startup, responses are served
stale-while-revalidate (an expired cache answers instantly and refreshes
in the background), and the Stats tab fetches a slim `view=usage`
variant (~a tenth of the full-row payload) for its whole-corpus fold.
Summaries are list-sized by design — day buckets and fingerprint
digests, never per-request usage history or per-file stat lists — so the
resident cache scales with session count, not transcript length, and the
startup sweep prunes entries whose sessions have been deleted. The index
directory is safe to delete; it rebuilds on the next scan.

- **Recent** — recent sessions as calm three-line cards (title + status
  chip + source badge; task snippet; compact meta) — the long tail (ids,
  absolute dates, token breakdown, disk) lives in the meta tooltip and the
  detail overlay. A stat-tile strip aggregates the filtered set. The list
  retrieves the **complete** history: the stream's quick phase paints the
  newest ~600 immediately, then the full corpus replaces it, so the tiles
  agree with the Stats tab. Lists render 300 cards and grow with an
  explicit **Show more** control; the first load shows skeleton rows while
  the list streams. "Changed" timestamps and the newest-first order follow
  **transcript activity** (`session.jsonl` mtime), not daemon bookkeeping
  writes into the log dir. Child sub-agent sessions are hidden by default;
  enable **Show subagents** to include them. Fork and side sessions stay
  visible with lineage chips that point back to their parent session.
  When a peer daemon is connected, a **host strip** appears above the
  toolbar: pick a peer chip to browse that daemon's sessions in place
  (read-only cards; clicking one hands off to the peer's own dashboard,
  where the peer's own auth applies).
- **Deep Search** — search across session history.
- **Worktrees** — the git worktrees in use by sub-agents (same card +
  Show-more treatment).
- **New Session** — start a fresh session from the dashboard.
  Internal-agent launches get an **Execution** control — *Auto* (the
  task-size heuristic decides), *Orchestrate* (delegates to supervised
  sub-agents), or *Direct* (single agent); an explicit choice beats the
  global *Direct* header toggle, and the control disables when an
  external backend is selected. Internal launches also preflight the
  daemon's **fueled** state: with no API key or vault lease, an inline
  banner explains why the internal agent can't start (external agents
  sign in with their own accounts and still work) and deep-links to
  Settings → API Keys — which applies immediately, no restart.
  External Codex sessions can choose both the binary path and the
  `managed_context` mode (`vanilla` or `managed`) for that session; the
  external-agent options sit in a fold that opens when an external
  backend is selected. Claude Code sessions get per-launch dropdowns for
  the model (version-safe aliases — `fable`, `opus`, `sonnet`, `haiku` —
  that the CLI resolves to the latest release, with a Custom-id escape
  for full model names), the permission mode, and the reasoning effort
  (`low` … `max`).

Internal sessions' window menus additionally expose **Delegate…** — spawn a
supervised sub-agent (task, optional name, role, backend, worktree isolation)
under that session on the model's behalf. The parent is notified with a
follow-up and collects the result with its `wait_sub_agents` tool; see
[Native Multi-Agent Orchestration](./multi-agent.md#delegating-from-the-dashboard).

External-agent session cards and Activity windows also expose **Launch config**
for per-session binary and managed-context settings. Use **Save** to update the
next attach/resume, or **Save & restart** to apply the new binary/mode
immediately to that external backend. These settings are stored with the
Intendant wrapper session and, for canonical backend session IDs, in an
external-session overlay. They are used on the next attach/resume so a daemon
restart or page refresh does not fall back to the current global Settings pane.
Claude Code sessions get four pinnable rows: model (the same version-safe
aliases as New Session, plus a Custom-id escape), permission mode, allowed
tools (comma-separated rules; `all` pins the explicitly-unrestricted empty
list so a session can escape a restrictive global list), and reasoning
effort. Every field saves as a pin or the explicit `inherit` clear — and
"default" is a *real* permission mode that pins, unlike the other fields'
clear sentinels. On a live session, **Save** additionally applies the model
and permission pins immediately (native `set_model` / `set_permission_mode`
control requests); tools and effort take effect at the next launch or via
**Save & restart**.
The separate **Restart with saved config** action is a power-user shortcut for
reapplying settings that were already persisted elsewhere.
The Managed activity view exposes rewind anchors, saved records, restore, and
fork/backout actions. With the patched managed Codex binary, fork/backout starts
a new Codex thread while inheriting the saved rollout's lineage prompt-cache key;
there is no separate cache-reset opt-in in the dashboard.
Editable user-message entries still perform an in-place rollback when the
message is active in the current thread. Superseded user-message entries in a
managed Codex session show the same edit control as a historical branch action:
submitting the text creates a child thread from the newest saved pre-rewind
rollout containing the clicked message, rolls that child back to the selected
turn, and sends the replacement there. The edit chip labels this as branching so
the active compacted session is not mistaken for the target of the mutation.

### Files

Edit, browse, download, and upload files on the local daemon or a configured
peer target. The tab is split into two sub-tabs: **Editor** (the default) and
**Transfers** (the download/transfer-history/upload tooling). The target
summary uses the same access abstraction as Terminal: local/mTLS, hosted
transports, and peer dashboard-control routes are shown as targets with their
available capabilities rather than as transport internals.

The daemon-side durable state behind this surface — staged uploads
(`uploads/<session-id>/` blob + sidecar) and transfer job metadata
(`transfers/jobs`, plus daemon-materialized `transfers/artifacts`) — lives
under `<project>/.intendant/` on project-rooted daemons. A **projectless
daemon** (the macOS app daemon, or any `intendant` launched from a directory
with no project marker) serves the same endpoints from a daemon-global
fallback store at `~/.intendant/global-store/` with an identical layout; a
project root, when present, always wins. The global store is pruned on
daemon startup: upload session dirs, job files, and materialized artifacts
idle for more than 14 days are removed (project stores are never pruned).

The **Editor** sub-tab is a full-bleed workbench: a slim toolbar (target
picker + one-line route summary + new file/folder), a lazy directory tree
rail on the left (rooted at the project root locally, `~` on peers;
hidden-file toggle; hover-revealed per-row rename and delete — rename edits
the name inline, delete is a two-click arm-then-confirm that escalates to an
explicit "Delete all?" for non-empty folders), and a multi-tab CodeMirror
editor filling the rest (vendored bundle, `static/codemirror-bundle.js`,
lazy-loaded on first use; syntax highlighting by filename, dirty markers,
hover-reveal close, a Reload-or-Overwrite conflict banner, Cmd/Ctrl-S, and a
Cmd/Ctrl-F find bar with smart-case matching, live counts, and Enter /
Shift-Enter stepping). Renaming a file whose buffer is open retargets the
tab in place (same undo history); deleting one closes a clean tab and flips
a dirty one to the missing-file banner so unsaved work survives. One accent
answers "whose disk is this?": the active editor tab's underline, the tree
selection, and the statusbar host chip render blue while editing this daemon
and mauve on a peer (`--files-accent`). Reads and writes ride the same fs
API family as everything else and are therefore IAM-scoped end to end:

- Local targets use `GET /api/fs/stat|list|read` and
  `POST /api/fs/write|rename|delete` (all classified
  `FilesystemWrite`→`write_roots` for mutation, and gated by
  `authorize_http_filesystem_access` exactly like `mkdir` — a rename
  authorizes both legs, since removing the source and creating the
  destination are each writes).
- Peer targets ride the peer's dashboard-control tunnel: `api_fs_stat/list`
  requests, `api_fs_read` byte streams, `api_fs_write` upload frames, and
  `api_fs_rename`/`api_fs_delete` requests.
  Enforcement happens on the receiving daemon against its own peer profile
  (`file-operator` vs `file-reader`) and per-peer filesystem roots; the
  browser only picks where a request is sent, never whether it is allowed.
- Saves are conflict-checked: full reads return the content's sha256
  (`X-Content-Sha256` header on HTTP, `sha256` in the byte-stream result),
  the editor sends it back as `expected_sha256`, and a mismatch returns
  `409 {code:"conflict", current_sha256}`, which the UI surfaces as a
  Reload-or-Overwrite banner instead of clobbering. New files save with
  `create_new`; `force` is the explicit overwrite escape hatch. Writes land
  atomically (same-directory tempfile, fsync, permission-preserving rename).
- Guardrails: binary or non-UTF-8 and >2 MB files are refused with a pointer
  to the Downloads flow; per-request write payloads cap at the shared 100 MB
  upload limit; UTF-8 files keep their dominant line-ending style on save.
  A rename never replaces an existing destination (`409 code:"exists"`) and
  refuses cross-filesystem moves; deletes remove symlinks as links (never
  following) and require an explicit `recursive` for non-empty directories
  (`409 code:"not_empty"`).

### Access

Unified administration for how dashboards and daemons reach each other:

- Access is available as both the in-dashboard **Access** tab and a first-class
  `/access` page on daemon origins. The page opens the same Access surface
  without task/session chrome so it can act as a local fleet-admin home.
- The surface is organized around the product mental model — *who* is acting,
  *how* the request travels, and *the target daemon decides* — in six panes:
- **Overview** answers "who am I here and what can I reach": actionable
  attention banners first (failed routes, a hosted Connect account the daemon
  refuses, draft grants, unreadable IAM state), then an identity hero (your
  principal, the route you arrived on, your role and what it allows), a daemon
  card with fleet/people/peer counts, a fleet-at-a-glance strip, and a
  three-step explainer of the access model.
- **Daemons** lists targets, not raw transports: each row shows a clean display
  name (never a bare IP or key when a better name exists), a route chip, your
  role badge there, capability chips, and Stats/Files/Shell/Display actions.
  The pane also hosts the per-peer messaging/task/approval quick controls and
  capability routing, both in collapsed poweruser folds.
- **People & Devices** owns the user/client domain. It shows your identity on
  this daemon, a guided grant flow (who → identity details → role → active or
  draft) with a role-card picker and advanced identity metadata folded away,
  and every user/client principal with its grants. Grants can be activated,
  drafted, or revoked inline; draft and revoked records stay visible for
  review.
- **Peers** owns the daemon-to-daemon domain: inbound/outbound summaries, the
  Link-daemons wizard (Request Peer Access, Join Invite, Grant Peer Invite,
  Manual Add), inbound peer access requests, approved/revoked inbound peer
  identities, and every peer-profile grant in both directions.
- **Diagnostics** owns dashboard route health, including hosted Connect,
  local/mTLS, local WebRTC control, event delivery, byte streams, uploads, and
  self-tests. The oversight bar's access dot links here, and its prefix names
  the actual route (`mTLS`, `Connect`, `WebRTC`, or `local`) so "Ready" is
  never ambiguous.
- **Advanced** is the poweruser den: the role catalog and the exact
  policy×permission matrix, every grant with policy/transport/reason detail,
  the raw model inspector (principals, grants, policies, permissions, IAM
  roles, audit events, transports), the local IAM state card with raw JSON
  links, unresolved architecture notes, and the public-sharing posture
  (nothing is shared unless explicitly granted).
- Old deep links keep working: `#access/policies`, `#access/audit`, and
  `#access/public` resolve to Advanced, `#access/invitations` to Peers, and
  `#access/targets` to Daemons.

Access uses one vocabulary across the hosted dashboard, direct/self-hosted mTLS,
and peer federation:

- A **target** is a daemon the dashboard can operate.
- A **principal** is the actor being trusted: the current browser session, a
  browser certificate, a hosted Connect account, a future organization group, or
  a peer daemon.
- A **grant** connects one principal to one target with a role and status. The
  current browser has a root user/client grant to the local daemon. A peer route
  has a daemon peer-profile grant. An approved inbound peer identity appears as
  a peer-daemon principal with a peer-profile grant to this daemon; revoked
  identities remain visible as revoked grants for audit clarity. Local IAM
  grants loaded from `iam.json` are enforced when active and bound to browser
  mTLS certificate fingerprints, hosted Connect account metadata, or a
  `human_user` record that can carry both bindings plus account/provider and
  organization metadata. Draft and revoked records remain visible for review.
- A **policy** defines the shape of authority behind a grant. `root` and
  `peer-profile` are enforced today, but they live in different domains:
  `root` is user/client authority and `peer-profile` is daemon-to-daemon
  authority. Local user/client bindings can also use enforced scoped roles:
  `scoped-human` (access model inspection only), `observer`, `session-reader`,
  `terminal`, `files-read`, `files-write`, `peer-user`, and `operator`.
  Directory-scoped file access, public shares, organization groups, and
  external identity policy are design targets, not hidden enforcement.
- A **permission** is the operation gate the daemon enforces. Access
  administration now separates `access.inspect` from `access.manage`, and peer
  topology separates `peer.inspect`, `peer.manage`, and `peer.use`.
  `peer.use` is the delegation gate: acting through a connected peer —
  opening a tunnel (dashboard-control, file-transfer, or display signaling)
  or sending it a message, task, or approval decision — presents *this
  daemon's* peer credentials, and the receiving peer authorizes each action
  against its own grants for this daemon — so relaying is
  never inferred from local capabilities, it is granted by name
  (`operator` and `peer-user` carry it; `peer.manage` implies it for
  compatibility). Owner/root dashboard sessions have all of these. Existing
  peer profiles are mapped conservatively: `peer-root` can inspect access and
  inspect/manage/use peer topology, but `access.manage` remains reserved for
  trusted root user/client sessions.
- A **transport** is only how the route is carried: browser mTLS, hosted
  Connect/WebRTC tunnel, local/debug HTTP, or daemon-to-daemon peer mTLS. The
  product UI should not make Connect a separate access system.

The browser may also maintain a local fleet registry for navigation: daemon ids,
labels, remembered URLs, and the route/auth summary last seen from a daemon. This
registry is client-side metadata, not an authorization source. If a remembered
target is no longer configured on the current daemon, Access shows it as a
browser-local record with operation buttons disabled. The daemon still owns IAM
enforcement for every request.

When the dashboard is loaded from the hosted Connect/Access origin in
`?connect=1` mode, that browser-local fleet registry is also synced to the
signed-in account through `GET /api/fleet/targets`,
`POST /api/fleet/targets/sync`, and
`POST /api/fleet/targets/{target_id}/forget`. The hosted service stores only
navigation metadata: target ids, labels, route labels, URLs, capability hints,
and last-seen timestamps. It does not store browser mTLS private keys, daemon
IAM grants, peer secrets, dashboard session grants, or passkey private material.
Claimed Connect daemons are merged into the same target list as live
`connect_daemon` records and override stale remembered labels. Records the
browser pushes are signed with its identity key and re-verified after every
round trip; target rows and the fleet strip badge each synced record as
verified (this browser), signed (another device), unverified, or a hosted
claim — so the metadata store can remember the fleet but cannot invent or
alter it unnoticed. Direct mTLS
dashboards on daemon origins remain local-first; cross-origin sync to
`intendant.dev` is a separate explicit-consent design problem, not something the
current cookie model does silently.

The important security-domain split is:

- **User/client daemon access** means a human-operated dashboard can control a
  daemon. Hosted Connect passkey access and browser mTLS client certificates are
  both in this domain. Unbound browser mTLS owner sessions remain
  root-compatible; hosted Connect sessions require a daemon-local IAM binding
  for the routed account. Active local IAM bindings can scope a browser
  certificate, Connect account, or combined human mTLS user through this same
  domain. Future coworker/team access belongs here, not in peer federation.
- **Peer access** means one daemon can call capabilities on another daemon. That
  uses daemon-to-daemon mTLS identities and peer profiles such as `peer-operator`
  or `peer-root`. Peer access does not imply that the human's browser can open
  the remote daemon directly, and browser access does not imply that two daemons
  can federate.

The model is backend-backed. `GET /api/access/overview` and the
dashboard-control `api_access_overview` method return schema version 1 with
`scope`, `targets`, `principals`, `grants`, `policies`, `permissions`,
`transports`, `supported_principal_kinds`, `iam`, and explicit unresolved
architecture notes. Root dashboard sessions and peer daemon sessions now pass
through the shared IAM operation evaluator, while preserving the existing root
and peer-profile semantics. The overview still exposes one product model over
the current transport/auth paths rather than replacing mTLS, Connect account
checks, or peer profiles.

The local IAM foundation lives beside the native access cert store as
`iam.json` and is also available at `GET /api/access/iam/state` or
dashboard-control `api_access_iam_state`. Its schema contains `principals`,
`roles`, `grants`, and `audit_events`. The daemon exposes this state for
inspection, merges managed principals/grants into the unified overview, and
enforces active scoped user/client grants when a request can be bound to a
stable local principal. Today those stable bindings are browser mTLS client
certificate fingerprints, hosted Connect account metadata, and `human_user`
records that can combine both while carrying optional account provider,
verified-provider, handle, and organization metadata. Existing owner browser
mTLS requests remain root-compatible when no active local binding exists so
direct/self-hosted access stays first-class. Hosted Connect requests do **not**
get that root fallback: the daemon only answers a Connect dashboard offer when
the Connect account matches a daemon-local IAM principal/grant. Active grants
are evaluated by role, while draft or revoked records deny instead of silently
becoming root again. The `iam.enforcement` object reports
`root_session_grants: true`, `peer_profile_grants: true`,
`user_client_grants: true`, and
`principal_binding: root_peer_and_local_user_client`. Root sessions can create
or update local user/client grants through the People & Devices pane,
`POST /api/access/iam/user-client-grants`, or dashboard-control
`api_access_iam_upsert_user_client_grant`. Existing grants can be activated,
drafted, revoked, or role-changed with `POST /api/access/iam/grants/update` or
dashboard-control `api_access_iam_update_grant`.

The grant flow's **Apply to** step fans one grant out across the fleet: the
page calls each selected daemon's
`POST /api/access/iam/user-client-grants` directly and reports per-daemon
results; every target authorizes independently. Cross-origin use of the
fleet Access APIs (`/api/access/overview`, `/api/access/iam/state`,
`/api/access/iam/user-client-grants`, `/api/access/iam/grants/update`,
`/api/access/enrollment-requests[/decide]`) is origin-gated per daemon: only
the daemon's own origin, the macOS app scheme, its outbound peer routes, and
its approved inbound peer identities may drive them, and responses are never
wildcard-readable. Requests from any other page are refused outright, so a
browser-installed mTLS certificate cannot be steered cross-site. Reach the
Access page by an origin the fleet advertises (the target rows already link
that way) for cross-daemon administration to work.

The same posture now applies daemon-wide: API responses carry no
`Access-Control-Allow-Origin` by default (same-origin only), every `/api/*`
request bearing a foreign Origin is refused, and only the deliberate
public-bootstrap surfaces stay wildcard-readable — `/config`, the agent
card, local Connect signaling (`/connect/status`, `/connect/dashboard/*`,
whose real authentication is the daemon-signed binding plus IAM), and the
public peer-access doorbell. The macOS app is unaffected: its custom-scheme
pages are proxied natively, and the `intendant://` scheme is treated as the
daemon's own origin.

Device enrollment closes the loop for browsers that hold an identity key but
no grant yet: when a *verified* client key is refused, the daemon queues a
pending enrollment (in-memory, capped, TTL'd — the queue grants nothing by
itself). `GET /api/access/enrollment-requests` /
`api_access_enrollment_requests` list the queue (`access.inspect`), and
`POST /api/access/enrollment-requests/decide` / `api_access_enrollment_decide`
(`access.manage`) approve with a role or deny. Approval reuses the normal
user-client grant upsert with the queued key's public key and route origin
attached, so role ceilings and audit apply as usual. People & Devices shows
the queue as **Pending devices**, and the Overview raises an attention banner
while any request waits.

The same IAM evaluator now protects the direct dashboard HTTP routes that expose
Access, target discovery, settings, filesystem reads/writes, sessions,
worktrees, displays, diagnostics, and managed-context data. Static bootstrap
assets, `/config`, `/.well-known/agent-card.json`, local Connect signaling, and
the WebSocket bootstrap stay outside this generic HTTP gate because they either
do not mutate daemon state or have their own transport/authentication binding.

`GET /api/dashboard/targets` and `api_dashboard_targets` remain the compatibility
target model used by older UI paths: target id/host id, display label, access
domain (`user_client` or `peer`), route (`current_dashboard` or `peer_route`),
effective role (`root` or `peer_profile`), connection state, and capability
hints. The browser may refine the local route label to **Intendant Connect**,
**Browser mTLS**, or **Local/debug** because only the browser knows how the
current page was reached, but it should not invent principal/grant/policy
vocabulary.

### Debug

Observer display and browser workspaces (daemon diagnostics and raw event
streams live under **Access → Diagnostics**). The observer display is a
headless debug screen the agent can draw on, recordable from here. The
browser-workspace panel does manual smoke testing of local CDP-backed
browser workspaces and their leases.
CDP workspaces prefer managed Chromium/Chrome-for-Testing executables; on macOS
system Chrome/Chromium apps require choosing `system_cdp` or setting
`INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1`. Run
`intendant setup browsers` to install or repair the managed browser cache.

### Settings

The configuration panel for the current session: API keys, external-agent
backend settings, computer-use/provider options, presence, transcription,
recording, and live audio. Peer/network administration moved to **Access**.
Old `#settings/network` deep links are redirected to `#access/overview`.

## Late-join and session replay

The gateway is built so a browser that connects late sees the full picture
immediately. On WebSocket connect the server sends a sequence of bootstrap
messages:

1. **`state_snapshot`** — the full `AgentStateSnapshot` plus this connection's
   id, the `/config` payload, and the `session_id`
2. **Cached `usage_update`** — latest token usage
3. **Cached `status`** — latest autonomy / session id / task
4. **Cached `display_ready`** — latest display info for WebRTC sessions
5. **`browser_workspace_snapshot`** — active browser workspaces and lease state
6. **`log_replay`** — historical session events parsed from `session.jsonl`

So refreshing the page, or opening a second browser mid-run, replays the
session rather than starting from blank.

## Live voice (optional)

The dashboard supports optional low-latency voice via **Gemini Live** or
**OpenAI Realtime**. Voice is entirely optional — the dashboard is fully usable
without it.

When activated:

- The browser connects **directly** to the model's realtime API for voice I/O.
- The WASM layer (`presence-web`) handles mic capture, resampling, and WebSocket
  streaming.
- The live model receives agent events and narrates progress, and can call
  presence tools (`submit_task`, `approve_action`, `check_status`, …) which are
  routed over the dashboard WebSocket to the server.
- Server-side text presence is automatically paused (the two are mutually
  exclusive).

### Voice setup

1. Enter your provider API key on first visit (Gemini or OpenAI).
2. Keys are stored in browser **localStorage** and are never sent to the
   Intendant server (the server only mints short-lived session tokens via
   `POST /session`).
3. Click the microphone button to connect.

### Active vs. passive browsers

Only one browser can be the **active** voice controller at a time:

- The first browser to connect voice becomes active.
- Additional browsers are passive observers — they receive events
  but do not pause server-side presence.
- A passive browser can request active status, which force-disconnects the
  previous active browser. Handover carries the last checkpoint summary and
  conversation context.

### Session continuity across reconnects

The presence session protocol survives refreshes and dropped connections:

1. On connect the server sends a `presence_welcome` with current state, missed
   events, and conversation context.
2. The browser sends periodic `presence_checkpoint` messages summarizing the
   conversation.
3. On reconnect the server replays events since the last checkpoint.

This keeps the voice model from losing context. The protocol and mutual
exclusion are detailed in [Presence Layer](./presence.md).

## Server-side transcription

Independently of browser-side voice, the server can transcribe microphone audio
via the Whisper API when `[transcription]` is enabled (or `--transcription` is
passed):

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"
language = "en"
```

The browser streams PCM16 audio; the server buffers it in ~3s chunks
(`buffer_secs`, RMS-filtered to skip silence) and sends each chunk to the
transcription endpoint. Transcripts are broadcast as `user_transcript` events
and written to the session log. See
[Configuration](./configuration.md#transcription).

## Secure Browser Contexts

- **Microphone/camera require a secure context.** Plain `http://<host-ip>` is not
  a secure context in the browser, so `getUserMedia` is blocked there. Reach the
  dashboard over one of:
  - `http://localhost` (e.g. an SSH tunnel: `ssh -L 8765:localhost:8765 host`),
  - HTTPS/mTLS via the default dashboard transport, `--tls`, or `[server.tls]`
    (see below), or
  - the macOS app bundle, which serves the page over a custom `intendant://`
    scheme specifically to restore the secure context (see
    [Getting Started](./getting-started.md#macos-app-bundle)). The bundle starts
    its daemon with native mTLS by default so remote browsers get a safe context
    over `https://` and must present an enrolled client identity.
- **API key for voice:** Gemini or OpenAI, stored browser-side only.

### HTTPS / TLS

```bash
./target/release/intendant                       # default: mTLS, requires access certs
./target/release/intendant --tls                 # TLS-only; installed access certs when present, else self-signed
./target/release/intendant --no-tls --bind 127.0.0.1 # explicit local plaintext/debug escape
./target/release/intendant --tls-cert c.pem --tls-key k.pem   # bring your own
```

By default, the gateway serves HTTPS/WSS with browser client certificates
required. `--tls` (or `[server.tls] enabled = true`) makes the gateway serve
HTTPS/WSS without requiring client certificates. With no explicit cert/key
override, TLS-only uses installed access server certs when present and falls back
to an auto self-signed certificate. Plain HTTP via `--no-tls` is intended for
local/programmatic debugging; wildcard plaintext refuses startup when the host
has a public interface unless `--allow-public-plaintext` is passed.
The gateway demuxes per connection: a first byte of `0x16` (a TLS ClientHello)
is wrapped in the rustls acceptor, while raw WebRTC ICE-TCP/UDP media is left
untouched. The TLS stack is pure Rust (`rustls` + `rcgen`) and works on every
platform including Windows — no nginx, no OpenSSL. See the
`[server.tls]` keys under
[Configuration → `[server]`](./configuration.md#server-daemon-and-federation).

For explicit mutual-TLS with client certificates (only enrolled devices can
connect), use native `--mtls` / `[server.mtls]`; this is also the default when no
transport flag is supplied. Use `intendant access setup` to generate the
per-user access CA/server/client certs and run strict enrollment. See
[Getting Started → Dashboard access over TLS](./getting-started.md#dashboard-access-over-tls) and
[Peer Federation](./peer-federation.md). For the daemon posture and remote
control surface, see [Control Plane & Daemon](./control-plane-and-daemon.md).

### WebRTC Dashboard Control Tunnel

Intendant also has an experimental daemon-scoped WebRTC DataChannel transport
for dashboard control traffic. It is not a replacement for dashboard
authentication yet: today the browser still bootstraps from the normal dashboard
origin, then uses the daemon's local `/connect/dashboard/*` signaling endpoints
to establish the DataChannel. The existing WebSocket signaling path remains as
a compatibility fallback for older dashboard bundles. The point is to prove the
future "public HTTPS bootstrap + direct local daemon data path" shape without
weakening the current mTLS default.

The handshake is bound to the daemon identity:

- the browser creates a `intendant-dashboard-control` DataChannel and sends an
  SDP offer to `/connect/dashboard/offer`;
- the daemon answers with SDP plus a signed binding over the offer hash, answer
  hash, session id, timestamp, and daemon Ed25519 public key;
- the browser verifies that binding with WebCrypto before using the channel.
- in Connect-rendezvous mode, the browser also requires the answer to carry the
  daemon public key registered for the selected daemon id and rejects the tunnel
  if that advertised key differs from the key inside the signed binding.

When enabled with
`localStorage.intendant_dashboard_transport = "webrtc-control"` (or
`window.intendantDashboardControl.enable()`), dashboard JSON reads prefer the
DataChannel and fall back to HTTP through the browser-side `DashboardTransport`
boundary. Current tunneled reads include the local Agent Card identity, sessions,
session detail, lazy command-output loads for the active session,
active-session timeline history, active-session changes/diffs, lazy exact
context-snapshot loads, filesystem picker stat/list/mkdir operations, deep
session search, settings, API-key status, server-side voice-session token
minting, project root, display enumeration, recording metadata, worktree
inventory, staged-upload descriptors, scoped recording asset byte streams,
archived session frame image byte streams, bounded session-report zip downloads,
and peer state.
Managed-context history reads for records, anchors, and fission groups also use
the tunnel.
Current tunneled mutations include
active-session rollback/redo/prune, session-data deletion, staged upload
deletion, settings save, API-key save, peer add/remove, peer access-request
pairing, peer message/task/approval actions, peer-display WebRTC signaling,
eligible-peer lookup,
visual-freshness diagnostics NDJSON appends, worktree scan/remove, dashboard
managed-context MCP tool calls, coordinator routing, and dashboard
session-control and dashboard-action controls. Annotation attach/save/send and
clip creation use a dedicated dashboard media/editor protocol over the same
verified channel: annotation image bytes travel as `upload_*` frames committed
by `api_media_annotation_attach` or `api_media_annotation_submit`; clips use
`api_media_clip_start`, ordered `api_media_clip_frame` uploads keyed by
`clip_id`, then `api_media_clip_end` or `api_media_clip_cancel`.
Allowlisted settings-style `ControlMsg`s, such as autonomy, approval-rule,
external-agent, Codex, and verbosity settings, can also dispatch over
the DataChannel when it is verified. Display input authority uses dedicated
DataChannel RPCs and a `display_input` frame rather than the generic
`ControlMsg` allowlist. The Shell terminal tab uses dedicated
`terminal_*` frames over the same verified channel. Session lifecycle,
steering, approvals, interrupt, resume, stop/restart, rename, and launch-config
changes use a separate
`api_session_control_msg` RPC with its own allowlist instead of broadening the
generic settings-style `api_control_msg`. Smaller dashboard action controls use
`api_dashboard_action_msg`; this includes Codex thread actions, display
take/release/grant/revoke, the diagnostics visual-marker toggle, recording and
debug toggles, and browser workspace create/acquire/close/release. It has its
own allowlist and the same no-replay fallback rule as the other mutation RPCs.
Mutation fallbacks are deliberately conservative: if a connected WebRTC RPC
fails after it may have reached the daemon, the dashboard surfaces the error
instead of repeating the write over HTTP. The visual-freshness sampler follows
the same rule for NDJSON appends; it uses the legacy HTTP endpoint only when no
verified DataChannel path is available.

The tunnel mirrors HTTP JSON response semantics. Application errors travel as
successful transport frames with `_httpStatus`/`_httpOk` metadata so existing UI
code can render the same error message it would render for an HTTP response.
Transport failures, unknown RPC methods, and aborted requests still reject the
browser-side promise.

Several paths intentionally stay outside this JSON tunnel:

- static assets and WASM bundles;
- native media fallback URLs and transfer paths outside the scoped dashboard
  byte/upload protocols;
- general filesystem mutations and durable broad file-transfer queues;
- generic MCP-over-HTTP for external clients;
- non-allowlisted `ControlMsg` mutations;
- display WebRTC media channels;
- daemon-to-daemon federation authentication.

Peer mTLS remains a separate trust boundary. The dashboard tunnel authenticates
the browser-to-this-daemon control path; it does not grant or replace a
daemon's peer-scoped client certificate for federation.

### Connect-Style Local Bootstrap Slice

The daemon also exposes a narrow, experimental Connect-style bootstrap surface
for testing the public-origin signaling shape without changing the normal
dashboard:

| Endpoint | mTLS? | Purpose |
|----------|-------|---------|
| `GET /connect/bootstrap` | not required | Minimal HTML bootstrap page for WebRTC dashboard-control transport testing |
| `GET /connect/status` | not required | JSON health/capability probe for the bootstrap surface |
| `POST /connect/dashboard/offer` | not required | Browser SDP offer -> daemon SDP answer plus signed binding |
| `POST /connect/dashboard/ice` | not required | Browser trickle ICE candidate for a control session |
| `POST /connect/dashboard/close` | not required | Close a control session |

Those paths are deliberately allowlisted one by one. They do **not** make `/`,
`/config`, `/ws`, `/api/*`, assets, recordings, or the full dashboard available
without the normal dashboard authentication. The bootstrap page exposes
`window.intendantConnectDashboard` for tests and diagnostics; it verifies the
same daemon-signed binding as the full dashboard control experiment, then uses
the DataChannel RPC protocol directly. Its small browser-side transport supports
plain JSON requests, chunked JSON responses, bounded `byte_stream_*` downloads,
and `upload_*` frames, so the local bootstrap check can cover both read-style
artifacts and media/editor writes without making the full dashboard certless.
These local endpoints are useful for same-origin dashboard experiments and
diagnostics; by themselves they do not solve browser trust for a public page
talking to a daemon HTTPS certificate the browser has not already accepted.

Run the focused browser check against a local daemon with:

```bash
PLAYWRIGHT_NODE_PATH=/path/to/node_modules \
  node scripts/validate-connect-bootstrap.cjs --origin https://127.0.0.1:8766
```

The check intentionally uses no client certificate. It must see `/config`
rejected with `401`, then prove that `/connect/bootstrap` can create a verified
dashboard-control DataChannel, issue RPC requests, read a bounded byte stream,
and commit media/editor uploads over the tunnel.

To test the full dashboard bundle's local signaling path, run a loopback-only
plaintext debug daemon through:

```bash
node scripts/validate-dashboard-control-local-signaling.cjs \
  --dashboard-binary ./target/release/intendant \
  --daemon-port 8877
```

That harness enables `window.intendantDashboardControl` in the real SPA and
asserts that the verified DataChannel reports `signalingMode: "local-http"`.

This slice is a local low-level harness for the dashboard-control tunnel. It
does not implement account signup, passkeys, daemon claiming, or a durable
daemon registry. Its job is to keep the same-origin tunnel protocol easy to
exercise while the hosted Connect service owns the account and daemon-claim UX.
The validator seeds a temporary daemon-local IAM grant for its fixed test
Connect account so the tunnel still exercises the same no-implicit-root rule as
hosted Connect.

### Local Rendezvous Emulator Slice

The next experimental slice moves signaling off the daemon-served page. A daemon
can opt into an outbound rendezvous client with `[connect]` or the
`INTENDANT_CONNECT_RENDEZVOUS_URL` environment variable. In that mode:

1. The daemon registers a daemon id and daemon identity public key with a
   rendezvous endpoint.
2. The daemon long-polls the rendezvous endpoint for dashboard-control offers,
   ICE candidates, and close requests.
3. A browser loads a separate public-origin emulator page instead of a daemon
   page.
4. The emulator brokers SDP/ICE only; the browser and daemon still establish a
   direct WebRTC DataChannel when ICE succeeds.
5. The browser verifies the same daemon-signed binding before issuing RPC over
   the channel.

Run the end-to-end validator with:

```bash
node scripts/validate-connect-rendezvous.cjs
```

That script uses Playwright when it is installed (`PLAYWRIGHT_NODE_PATH` may
point at a temporary `node_modules`), otherwise it falls back to launching
Chrome/Chromium through the DevTools Protocol. The fallback honors
`INTENDANT_BROWSER_WORKSPACE_EXECUTABLE`, `INTENDANT_BROWSER_EXECUTABLE`,
`CHROME_PATH`, and `CHROME_BIN`.

The validator starts a local rendezvous HTTP origin, launches a fresh daemon
child with Connect env vars, verifies that
`https://127.0.0.1:<daemon-port>/config` still rejects a certless request with
`401`, verifies that daemon rendezvous endpoints reject missing bearer auth, and
then performs these browser passes:

1. It loads the minimal public bootstrap page from the rendezvous origin and
   drives `status`, `config`, `api_sessions`, id-filtered `api_sessions`,
   streamed `api_sessions_stream` hydration, a chunked large
   `api_sessions_stream` event, a chunked large `api_sessions` response,
   active-session command-output lookup, active-session timeline
   lookup/validation, bounded byte streams, uploads, media/editor writes, and
   application error RPCs over the verified DataChannel.
2. It serves the real `static/app.html` bundle from the same public origin at
   `/app?connect=1&daemon_id=...`, proves that it uses rendezvous signaling
   (`signalingMode: "connect-rendezvous"`), and checks that first-load dashboard
   data such as config, Agent Card identity, sessions, bootstrap frames, event
   subscription, and visible transport status all arrive through
   `window.intendantDashboardControl` instead of same-origin daemon HTTP/WSS.
   It also asserts that the SPA's signed daemon binding key matches the daemon
   public key registered with the rendezvous service for the selected daemon id.
   It injects a synthetic `api_control_msg` failure in the connected SPA and
   verifies that the generic settings-style write path does not replay the same
   mutation over the legacy WebSocket.
   This real-SPA pass also fails if the public-origin dashboard attempts daemon
   REST/media/WebSocket fallback paths such as `/config`,
   `/.well-known/agent-card.json`, `/api/...`, `/recordings`,
   `/connect/dashboard/...`, or `/ws`.
3. It opens the same real dashboard with an unregistered daemon id and asserts
   that the UI reports a Connect failure while still avoiding those public-origin
   REST/media/WebSocket fallbacks. This page must also stop daemon-dependent
   startup hydrators such as settings, project-root, and recording refreshes so
   the initial rendezvous failure does not cascade into unrelated errors.
4. It opens the real dashboard with the same registered daemon id while the
   emulator deliberately tampers with the advertised registry key for that offer.
   The SPA must reject the tunnel before it stores a verified binding, report a
   failed Connect transport, and still avoid daemon REST/media/WebSocket
   fallbacks.
5. It opens the real dashboard with the same registered daemon id while the
   emulator deliberately tampers with the browser-visible session grant. The
   daemon signs the grant hash it received in the offer event, so the SPA must
   reject the answer before it stores a verified binding or grant hash.
6. It opens the real dashboard while the emulator deliberately tampers with the
   browser challenge nonce forwarded to the daemon. The daemon signs the nonce it
   received, so the SPA must reject the answer before it stores a verified
   binding or expiry.

This is still a protocol emulator rather than the consumer Connect service. It
has no account signup, passkey ceremony, daemon claim, revocation, audit log, or
hosted public HTTPS. Its fixed test account metadata must match the temporary
daemon-local IAM grant seeded by the validator. The emulator's per-offer session
value is only an opaque binding token used to prove that rendezvous state can be
carried through signaling and bound into the daemon-signed WebRTC session
statement; it is not dashboard authority.

### Hosted Connect Production Alpha

The hosted-service slice is implemented as a separate binary,
`intendant-connect`. It serves a public web origin, handles passkey-only account
registration/login, lets a signed-in user claim a daemon with a short-lived
12-word phrase, and brokers dashboard WebRTC signaling without asking the
browser to trust the daemon's private HTTPS certificate.

In production, run it behind ordinary public TLS for a public origin such as
`https://connect.intendant.dev`:

```bash
INTENDANT_CONNECT_TOKEN="$(openssl rand -base64 32)" \
  ./target/release/intendant-connect \
    --listen 127.0.0.1:9876 \
    --origin https://connect.intendant.dev \
    --rp-id intendant.dev \
    --static-root static \
    --data-file <state-file>
```

The `--rp-id intendant.dev` value means passkeys are scoped to the owned
Intendant parent domain while the actual UI can live on `connect.intendant.dev`.
For compatibility, the live production-alpha instance currently keeps its
original `INTENDANT_CONNECT_RP_ID=connect.intendant.dev`; changing that value is
a credential migration and existing users must register new passkeys. Browsers
also allow `http://localhost:<port>` as a secure context for local development,
so the same binary can be E2E-tested without public TLS.

The hosted service also serves `/access` as the account/fleet entry point.
`/connect` remains a compatibility alias for the same passkey, claim, daemon
list, open-dashboard, label, revoke, and audit workflows.

The daemon side still uses the normal `[connect]` outbound rendezvous client:

```toml
[connect]
enabled = true
rendezvous_url = "https://connect.intendant.dev"
daemon_id = "vortex-deb-x11-intendant"
auth_token = "same daemon token configured on intendant-connect"
```

The hosted MVP flow is:

1. The daemon registers its `daemon_id` and persistent daemon identity public
   key through `/api/daemon/register`.
2. If the daemon is unclaimed, Connect returns a short-lived claim phrase and
   URL. The phrase is a standard 12-word BIP39 English mnemonic generated from
   128 bits of entropy, stored only as a hash at rest, and regenerated if it
   collides with another active unclaimed daemon.
3. The user opens Connect, signs in or registers with a passkey, and submits the
   claim phrase.
4. Connect sends a `claim_challenge` event to the daemon. The daemon signs that
   challenge with its daemon identity key, and Connect verifies the signature
   before assigning ownership.
5. The user chooses the daemon in Connect and opens the dashboard. Connect
   issues a short-lived opaque routing/session grant, forwards the browser SDP offer
   to the daemon, and waits for the daemon answer.
6. The daemon signs the same WebRTC binding used by the local/rendezvous paths,
   including the offer hash, answer hash, browser nonce, expiry, daemon public
   key, and hash of the Connect-issued routing grant.
7. Connect validates that the answer came from the registered daemon key and
   that the signed grant hash matches before returning the answer to the
   browser. The browser independently verifies the daemon-signed binding before
   sending dashboard RPC over the DataChannel.

The state file durably stores users, passkeys, daemon ownership, hashed claim
phrases, account-scoped fleet navigation metadata, and a capped audit log. Plain
claim phrases, WebAuthn challenge state, browser offers, and routing grants
are memory-only. The service exposes a minimal account/fleet UI today: passkey
registration/login, claim-phrase entry, daemon list, daemon labels, open
dashboard, revoke ownership, fleet target listing/forget, and audit events.
The visible account identity is the globally unique account name/handle; the
internal WebAuthn display-name field is derived from that handle and is not a
separate user-facing profile field in the MVP UI.

Hosted Connect does not yet have Google/GitHub verification, organizations, or
team IAM. The local daemon IAM schema already has portable account/provider,
verified-provider, handle, and organization fields so a future hosted identity
layer can issue grants without changing the daemon-side access vocabulary, but
today those fields are operator-entered local metadata unless the current
transport already supplies them.

Inside the hosted dashboard, Settings -> Debug includes a **Connect Health**
panel. It summarizes the active dashboard-control transport, daemon binding,
ICE route, event stream, byte-stream support, terminal-frame support, and other
advertised tunnel capabilities. Its self-test button runs the same safe
browser-side probes used by the hosted E2E harness: no legacy HTTP/WebSocket
fallback for Connect-only mutations, Shell input ordering, terminal-output
dedupe behavior, display-control routing, and tunneled presence callbacks. It is
not a file-transfer integrity test; the Files tab owns the user-facing ranged
download flow, and the hosted validator still uses a known fixture path for
byte-accurate transfer checks.

Production-alpha hardening now includes:

- cookie-backed user mutations require same-origin requests and a per-session
  CSRF header;
- auth, claim, daemon, and browser signaling hot paths have simple in-memory
  rate limits keyed by reverse-proxy client headers;
- `/healthz` is a cheap liveness probe and `/readyz` verifies that the static
  dashboard bundle and state directory are usable;
- security-relevant service events are emitted as structured JSON on stderr in
  addition to the persisted user audit log;
- revoking a daemon removes ownership, blocks future grants, and enqueues close
  events for active dashboard-control sessions known to the service.

The reverse proxy in front of `intendant-connect` must terminate public TLS for
`connect.intendant.dev`, forward `Host`, set `X-Forwarded-For`/`X-Real-IP`, and
strip any inbound copies of those client-IP headers before setting them. Keep
the service bound to `127.0.0.1`, keep `INTENDANT_CONNECT_TOKEN` in a secret
store, and back up the configured state file; that file is the current
account/passkey/device ownership database.

The production-alpha operator path is captured in scripts, but live target
details are not stored in the public repository. Provide them through a private
env file or command-line flags:

```bash
cat > ~/.config/intendant/connect-prod-alpha.env <<'EOF'
CONNECT_HOST=<ssh-host>
CONNECT_SSH_USER=<ssh-user>
CONNECT_SSH_KEY=<private-ssh-key-path>
CONNECT_REMOTE_SOURCE=<remote-source-directory>
CONNECT_SERVICE=<systemd-service-name>
CONNECT_REMOTE_READYZ_URL=<local-readiness-url>
CONNECT_REMOTE_STATE=<remote-state-json-path>
CONNECT_PUBLIC_ORIGIN=https://connect.intendant.dev
EOF

CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/deploy-connect-prod-alpha.sh
CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/connect-state-backup.sh --passphrase-file ~/.config/intendant/connect-backup.passphrase
CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/connect-state-restore.sh --yes \
  --passphrase-file ~/.config/intendant/connect-backup.passphrase \
  ~/.local/share/intendant/connect-backups/intendant-connect-state-YYYYMMDDTHHMMSSZ.json.enc
```

The deploy script syncs the current worktree to the configured remote source
directory, builds on the host, restarts the configured systemd service, and
checks both the configured local readiness URL and the public
`connect.intendant.dev` readiness URL. Backup and restore default to encrypted
state snapshots and require an explicit plaintext flag for diagnostics.

Current alpha limits:

- one owner per daemon; no shared roles, teams, recovery, or account email flow;
- one bearer token protects daemon service endpoints;
- rate limits, sessions, pending offers, plain claim phrases, and active-session
  tracking are single-process in-memory state;
- no high-availability storage or database migrations; the state file is a
  single-node alpha persistence layer;
- no application-layer dashboard RPC relay; the default path is browser to
  daemon WebRTC, with TURN/WebRTC relay remaining a transport-level option;
- Files transfer history and resumed download offsets are browser-local state
  (`localStorage` plus IndexedDB parts), not server-side durable state shared
  across browsers; uploads are current-session staged attachments rather than
  arbitrary daemon filesystem writes;
- peer daemon-to-daemon mTLS remains separate from Connect account login.

Run the hosted MVP E2E locally with:

```bash
cargo build --bin intendant-connect --bin intendant
node scripts/validate-connect-hosted-mvp.cjs
```

That validator starts `intendant-connect`, launches a daemon with outbound
Connect enabled, uses a browser virtual authenticator for passkey registration,
claims the daemon, labels it, opens the real SPA in `connect=1` mode, verifies
the daemon-signed binding and Connect grant hash, exercises the Shell tab
over tunneled terminal frames, and runs the SPA's no-legacy-transport probes
for control actions, media/editor upload, visual-freshness diagnostics, display
signaling, display input authority, peer mutation fallback, presence
media, presence server callbacks, the Files tab's ranged download/resume flow,
the lower-level generic filesystem download probe, and staged upload raw range
reads. It then revokes the daemon while the tunnel is still open, waits for the
tunnel to close, and checks the audit events.

### Design Target: Public Bootstrap with a Direct WebRTC Dashboard Tunnel

> **Superseded in part.** The tunnel/bootstrap mechanics below shipped, but
> the trust conclusions were revised after production experience: a hosted
> origin must never serve privileged code or hold account authority. The
> adopted model — anchor daemons serve privileged code, the hosted service is
> demoted to zero-authority introductions/relay/backup/directory, and
> low-provenance sessions are role-capped — is specified in
> [Trust Architecture](./trust-architecture.md).

The current dashboard access model is certificate-first: a remote browser
reaches the daemon over HTTPS/WSS, usually with mTLS. That keeps the
implementation simple and gives the browser a secure context, but it also means
private host names, changing VM addresses, and locally generated server
certificates leak into the user experience.

The product problem is specifically **browser server trust**. Passkeys can prove
that the user approved a login, but they do not make
`https://192.168.x.y:8765` or `https://daemon.local:8765` a browser-trusted
origin. Public Web PKI also cannot directly cover VM-local names, `.local`
names, or changing private IP addresses. Pointing public DNS at private IPs is
fragile because DNS rebinding defenses, cache lifetimes, and per-network address
choices all become visible to users.

A plausible future direction is a **public-trusted bootstrap with a direct data
path**:

1. The browser loads an Intendant-owned public HTTPS origin with an ordinary
   publicly trusted certificate.
2. That origin handles account, passkey, device, and daemon-claim UX.
3. The daemon maintains an outbound signaling connection to Intendant Connect.
4. The browser and daemon establish a daemon-scoped WebRTC DataChannel directly
   where possible.
5. TURN/WebRTC relay remains available when a direct path cannot form.
6. Dashboard RPC, the main event stream, and display signaling move over that
   encrypted browser-to-daemon DataChannel.

This avoids asking the browser to trust private LAN HTTPS names: public TLS
secures the bootstrap page, while WebRTC supplies a private encrypted transport
to the daemon. It also fits Intendant's existing shape better than trying to make
public Web PKI cover VM-local or LAN-only daemon addresses. The codebase already
has browser-offer WebRTC, data channels, ICE-TCP multiplexing, relay fallback
for display/federation paths, and now an opt-in daemon-scoped dashboard control
tunnel with a browser-side `DashboardTransport` boundary.

#### Trust Model

WebRTC encryption is not the same thing as daemon identity. The DataChannel is
encrypted with DTLS, but the browser learns the DTLS fingerprint through
signaling. Therefore the signaling path must be authenticated and bound to the
daemon the user intended to reach.

The high-assurance trust model is:

- **Trusted dashboard code** is loaded from a daemon-served mTLS origin, or from
  another locally pinned/installed bundle. The hosted `intendant.dev` origin is
  not the root admin trust anchor in this mode.
- **Intendant Connect** is a public rendezvous/mailbox and optional WebRTC relay
  helper. It can help route signaling and store convenience fleet metadata, but
  it is not sufficient authority to open a dashboard.
- **The daemon** has a persistent daemon identity key, separate from both the
  ephemeral WebRTC DTLS certificate and peer mTLS client certificates.
- **The browser** accepts a dashboard tunnel only when it receives a fresh
  daemon-signed session statement bound to the claimed daemon identity, the
  current user/device/account metadata, the Connect-issued routing grant, the
  WebRTC session material, an expiry, and a nonce.
- **The daemon** accepts Connect dashboard control only when the routed Connect
  account matches a daemon-local IAM principal/grant, then applies that local
  role before exposing control-plane APIs over the DataChannel.

The current experimental tunnel implements the daemon-signed binding locally: the
browser sends a fresh challenge nonce with its SDP offer, and the daemon signs
the SDP offer hash, SDP answer hash, WebRTC control session id, creation time,
expiry time, daemon Ed25519 public key, that browser challenge nonce, and, when
rendezvous signaling supplies one, a Connect session-grant hash. The browser
verifies the signature with WebCrypto, rejects expired bindings, checks that the
signed nonce matches its own challenge, and checks that the visible grant hashes
to the signed grant hash before using the channel. A public bootstrap service
should keep that daemon identity binding and add account/device grants around it.

The local Connect-rendezvous emulator now also models the registry side of that
identity check: the daemon registers its public key for a daemon id, the browser
offer answer carries that registered key, and the public-origin dashboard accepts
the DataChannel only when the signed binding key matches the registered key. It
also models grant binding with an opaque per-offer value and nonce binding with a
browser-generated challenge: the browser accepts the answer only when that
visible grant hashes to the daemon-signed grant hash and the signed nonce matches
the nonce it put in the offer. This does not make Connect an authorization
authority; the daemon still requires local IAM for Connect account access.

This makes the security boundary explicit: hosted Connect is in the trusted
computing base only when the user chooses to load root-capable dashboard
JavaScript from the hosted origin. The high-assurance path keeps trusted admin
code local/daemon-served and uses `intendant.dev` only for rendezvous, relay, and
optional metadata sync. A compromised Connect service can then delay, drop, or
misroute signaling, but it cannot by itself mint daemon-local dashboard
authority.

#### Claim and Login Flow

A concrete flow should look like this:

1. The daemon generates or loads a persistent daemon identity key.
2. The daemon opens an outbound TLS connection to Intendant Connect and publishes
   a short-lived high-entropy claim phrase or QR URL.
3. The user opens the public Intendant Connect URL and signs in with a passkey.
4. The user claims the daemon by entering the phrase or scanning the QR code.
5. Connect records the daemon identity public key, owner account, device label,
   and any local policy metadata the daemon chooses to expose.
6. On later visits, the browser signs in with a passkey and selects the daemon.
7. A root/admin user opens the daemon-served Access page over direct mTLS and
   creates a daemon-local IAM grant for the Connect account if hosted
   rendezvous should be allowed.
8. Connect issues a short-lived routing/session grant and brokers WebRTC
   signaling between browser and daemon.
9. The daemon checks that the routed Connect account matches local IAM, signs
   the WebRTC session binding with its daemon identity key, and only then
   accepts dashboard RPC over the DataChannel.

Passkey step-up can then protect high-impact actions such as approving a peer
access request, changing autonomy policy, exposing display control, or minting
long-lived credentials.

#### Direct Path and Relay Fallback

There are two different fallback concepts, and they should not be conflated:

- **TURN/WebRTC relay fallback** keeps the browser-to-daemon DataChannel
  encrypted end-to-end at the WebRTC layer. The relay forwards packets but does
  not see dashboard RPC plaintext.
- **Application RPC relay fallback** would terminate or proxy dashboard messages
  at Intendant Connect unless an additional application-layer encryption scheme
  is added. That is a materially different trust posture and should be an
  explicit product mode, not the default fallback implied by "relay."

The preferred consumer path is direct WebRTC first, TURN/WebRTC relay second,
and no plaintext dashboard RPC through the public service by default. If an
operator deliberately enables an application relay for locked-down networks, the
UI should label it as "proxied through Intendant Connect" rather than "direct."

#### Dashboard Transport Contract

This is not a drop-in replacement for mTLS today. The dashboard mostly still
assumes ordinary HTTP endpoints plus a main WebSocket, while existing display
WebRTC sessions remain display-scoped. The current `DashboardTransport` boundary
is the first browser-side split: when the opt-in DataChannel is connected,
selected JSON reads and conservative mutations can use WebRTC and fall back to
HTTP where safe.

A production version still needs two explicit transport implementations:

- `HttpDashboardTransport`: current HTTPS/WSS REST plus main WebSocket.
- `WebRtcDashboardTransport`: request/response RPC, streaming events, and
  cancellation over a reliable ordered DataChannel.

The DataChannel protocol should stay explicitly framed rather than ad hoc JSON
messages. The first useful envelope set is:

| Frame | Direction | Purpose |
|-------|-----------|---------|
| `hello` / `hello_ack` | both | Version negotiation, daemon identity, session id, role, feature flags |
| `request` | browser -> daemon | HTTP-like method/body call with a request id |
| `response` | daemon -> browser | Status, metadata, body, or application error for a request id |
| `response_start` / `response_chunk` / `response_end` | daemon -> browser | Chunked delivery of an oversized JSON `response` frame |
| `stream_start` / `stream_event` / `stream_end` | daemon -> browser | Ordered event stream for a long-lived request id |
| `byte_stream_start` / `byte_stream_chunk` / `byte_stream_end` | daemon -> browser | Bounded raw-byte artifact transfer for a request id |
| `upload_start` / `upload_chunk` / `upload_end` | browser -> daemon | Bounded raw-byte upload transfer for a request id |
| `terminal_open` / `terminal_input` / `terminal_resize` / `terminal_close` | browser -> daemon | Standalone Shell PTY control for one terminal id |
| `terminal_output` / `terminal_exited` / `terminal_opened` / `terminal_error` | daemon -> browser | Standalone Shell PTY data and lifecycle frames |
| `event` | daemon -> browser | Control-plane event stream entry |
| `cancel` | browser -> daemon | Cancel an in-flight request or stream |
| `credit` | browser -> daemon | Backpressure for chunked responses, chunked stream events, or bounded byte streams |
| `ping` / `pong` | both | Liveness, latency, and reconnect diagnostics |

Oversized DataChannel `response` and `stream_event` frames are split at the
transport layer. The daemon sends a `response_start` header, base64-encoded
`response_chunk` frames containing the original JSON frame bytes, and a
`response_end` marker. The browser reassembles and parses the original frame
before handing it to existing request or stream code, so API semantics stay
unchanged. Current browser clients advertise `response_credit`, `byte_streams`,
`upload_frames`, and `terminal_frames` in `hello`; when
`response_credit` is negotiated, the daemon sends an initial chunk window and
then waits for browser `credit` frames before releasing more chunks. Stream
chunks carry a `chunk_id` so a large event inside a longer stream can be
credited and cancelled without ending the whole request id. Older clients that
do not advertise the feature still receive the legacy eager chunk burst.

Bounded artifact downloads use `byte_stream_start`, base64 `byte_stream_chunk`
frames, and `byte_stream_end`. This avoids wrapping raw bytes inside a JSON
result and reuses the same credit-window queue. Individual byte streams remain
bounded, but browser helpers can build resumable user-facing downloads by
issuing repeated ranged requests and resuming from the last completed offset.

Bounded dashboard uploads use `upload_start`, base64 `upload_chunk`, and
`upload_end`. The daemon writes chunks into a tempfile and commits through the
same upload store as `POST /api/session/current/uploads`, including the
`UploadReady` broadcast. The Files tab uses this same primitive for browser
file uploads, so uploaded files become current-session staged attachments. This
is still a one-shot, ordered transfer with no resume token, destination
filesystem path, or cross-refresh queue. Resume tokens, explicit destination
policy, and application-level restart semantics are still required before
treating uploads as broad resumable file transfer.

Dashboard media/editor writes intentionally stay outside the generic
`api_dashboard_action_msg` and `api_control_msg` allowlists. They use the
dedicated media protocol instead: annotation attach/save/send commits use
media-specific upload methods, and clip creation uses an operation id with
ordered frame uploads plus commit/cancel. Older daemons that do not advertise the
media protocol still receive the legacy `annotation_*` and `clip_*` WebSocket
messages before any tunneled write is attempted.

The **Terminal** tab's Shell uses `terminal_*` frames when the
verified tunnel advertises `terminal_frames`. The daemon attaches the tunnel to
the same PTY registry used by the WebSocket path, so scrollback and reconnect
behavior stay consistent.

The first streamed API on this substrate is `api_sessions_stream`, which mirrors
the existing `/api/sessions/stream` NDJSON event shape (`start`, partial
`session`, `phase`, final `replace`, `done`). When the verified DataChannel is
connected, local dashboard session hydration uses that stream and falls back to
the HTTP stream on safe errors. Peer session lists still use direct peer HTTP.
The local daemon identity is available as `api_agent_card`, returning the same
Agent Card shape served by `/.well-known/agent-card.json`; the HTTP endpoint
remains the unauthenticated discovery surface.
When the verified channel opens, the browser also applies `config` and
`api_agent_card` results to the same runtime-config and self-identity state
normally hydrated by `/config` and `/.well-known/agent-card.json`.
`api_cached_bootstrap_events` returns the daemon's current non-personalized
dashboard event cache (`usage`/`usage_update`, `live_usage_update`, `status`,
`autonomy_changed`, `external_agent_changed`, and `user_display_granted` when
present) as parsed JSON events. This cached-event RPC is intentionally narrower
than the WebSocket open sequence, but the tunnel exposes the personalized
bootstrap pieces as separate identity-aware APIs below. `api_dashboard_bootstrap`
composes those pieces so a public-origin dashboard can hydrate without the
primary daemon WebSocket.
`api_browser_workspace_snapshot` returns the existing
`browser_workspace_snapshot` message shape with active browser workspaces and
lease state; callers can feed it through the same browser workspace handler that
currently receives the WebSocket bootstrap message.
`api_state_snapshot` returns the existing `state_snapshot` message shape with
the current `AgentStateSnapshot`, dashboard config, daemon session id when known,
and a DataChannel-scoped `connection_id`. The connection id is the WebRTC
control session id, not the legacy WebSocket connection id.
`api_display_bootstrap` returns a DataChannel-safe display bootstrap envelope
whose `frames` array contains `display_ready` events for every active display
session known to the daemon. Those frames use the same event shape as the
WebSocket bootstrap (`event`, `display_id`, `width`, `height`), so browser code
can feed them through the existing display-slot path. When the daemon exposes a
dashboard-control display authority bridge, the envelope also includes
personalized `display_input_authority_state` frames for the same active display
ids; otherwise `display_input_authority_state` is listed in `omitted`.
`api_display_input_authority_snapshot` returns just those personalized authority
frames, while `api_display_input_authority_request` and
`api_display_input_authority_release` claim or release the display for the
current dashboard-control session and return fresh state frames for immediate UI
application. The browser then sends local keyboard/mouse events as
fire-and-forget `display_input` frames over the same daemon-scoped DataChannel.
If the tunnel or authority bridge is unavailable, the dashboard falls back to
the older WebSocket plus per-display input-channel path.
Local display WebRTC signaling uses `api_display_webrtc_signal` when the
verified tunnel advertises it. The browser sends the same `display_id`, offer
SDP, and ICE candidate shapes that the legacy `display_offer`/`display_ice`
WebSocket frames used; the offer RPC returns a `display_answer`, while daemon
ICE candidates arrive later as `display_ice` event payloads over the control
DataChannel. Daemon-origin dashboards may still fall back to the WebSocket when
the tunnel is unavailable. Public-origin Connect mode does not attempt a daemon
WebSocket fallback for local display signaling, so missing tunnel support fails
the display slot visibly.
`api_session_log_replay` returns the existing capped `log_replay` message shape
used by late WebSocket joiners. When no active session log exists it returns an
empty replay with `available: false`.
`api_external_session_activity_replay` returns an envelope whose `frames` array
contains compact external attached-session activity replay frames for currently
attached Codex/Claude Code sessions. It uses the same transcript payloads as
WebSocket bootstrap. The combined bootstrap skips an attached external session
when the active Intendant session log replay already names the same
`external_session_id`, avoiding duplicate transcript hydration.
`api_dashboard_bootstrap` composes the DataChannel-safe bootstrap pieces into an
ordered `frames` array: state snapshot, cached dashboard events, browser
workspace snapshot, active display `display_ready` frames, and capped session
log replay, followed by active external attached-session activity replay frames.
When the display authority bridge is available, it appends personalized
`display_input_authority_state` frames as well so a refreshed public-origin
dashboard can hydrate display control chips without the primary WebSocket.
Lazy command-output expansion for finalized log command groups uses
`api_session_current_agent_output`, preserving the same `_httpStatus`/`_httpOk`
metadata as the existing HTTP endpoint.
The active-session timeline uses `api_session_current_history`,
`api_session_current_rollback`, `api_session_current_redo`, and
`api_session_current_prune`; the mutation calls use the same no-replay fallback
rule as other writes.
Active-session change list/detail reads use `api_session_current_changes`,
preserving the existing path validation and `_httpStatus`/`_httpOk` metadata.
Live and per-session recording stream lists use `api_recordings` and
`api_session_recordings`. Scoped recording asset reads use `api_recording_asset`
and `api_session_recording_asset` for `segments`, `playlist.m3u8`, and validated
`seg_*.mp4`/`seg_*.ts` filenames with optional `offset`/`length` ranges. The
recording player uses these byte streams for segment lists and MP4 MSE buffers
when the verified tunnel is available. The non-MSE MP4 fallback also reads the
segment over the tunnel and assigns a local blob URL to the video element.
HLS/`.ts` playback also prefers the tunnel when available: the browser reads
`playlist.m3u8` and validated `.ts` segments with the same recording asset RPC,
rewrites the playlist to local blob URLs, and points the native video element at
that object URL. If the browser rejects the blob playlist, it falls back to the
daemon-served `m3u8` URL only on a daemon-origin dashboard page; public-origin
Connect mode does not attempt same-origin HTTP fallback for self-daemon media.
Archived session frame images use `api_session_frame_asset` for validated `.jpg`
and `.png` filenames under a resolved session's `frames/` directory. The session
detail gallery renders returned bytes through browser blob URLs when the verified
tunnel advertises byte streams, falling back to the existing HTTP image URL when
the tunnel is unavailable or a tunneled image read fails.
The Settings debug session-report download uses `api_session_report`, returning
the same text-artifact zip as `/api/session/{id}/report` through bounded
`byte_stream_*` frames. This remains intentionally scoped to the diagnostic
report; generic daemon file downloads use the Files tab and `api_fs_read`.
The task attachment upload path uses `api_session_current_upload` over
`upload_*` frames when the verified tunnel advertises `upload_frames`; it falls
back to `POST /api/session/current/uploads` only when the tunnel feature is not
available. Failed tunneled uploads are not replayed over HTTP, to avoid creating
duplicate attachments after an ambiguous partial transfer.
Dashboard annotation media uses the same ordered `upload_*` frame substrate but
commits to media-specific methods instead of the task attachment store:
`api_media_annotation_attach` registers a pending annotation frame, and
`api_media_annotation_submit` registers a saved annotation and optionally queues
it for the live presence context. Clip creation is stateful: the browser first
opens a `clip_id` operation with `api_media_clip_start`, uploads each JPEG frame
with `api_media_clip_frame` in strict `frame_index` order, then commits with
`api_media_clip_end` or discards with `api_media_clip_cancel`. The dashboard
chooses the transport once per media operation. If the media protocol is not
advertised before the first write, daemon-origin dashboards use the legacy
WebSocket media messages. Public-origin Connect mode has no daemon WebSocket
fallback, so annotation and clip writes fail visibly when the verified media
tunnel is not available. After a tunneled media write is attempted, failures are
surfaced and are not replayed over the WebSocket.
Browser-side live voice keeps its provider WebSocket in the browser, but the
daemon coordination side uses the Connect control tunnel. The WASM presence
bridge can install a custom sender so its normal server messages route over the
verified DataChannel instead of `/ws`: `presence_frame` carries
`presence_connect`, `presence_disconnect`, `make_active`, `voice_log`,
`presence_checkpoint`, `voice_diagnostic`, `live_usage_update`, `tool_request`,
and `async_query`. Server responses such as `presence_welcome`,
`force_disconnect_voice`, `active_granted`, `tool_response`, and
`async_query_result` are delivered back to the same WASM callback router that
the WebSocket path uses. HQ browser video/archive frames use
`api_presence_video_frame` over the ordered `upload_*` substrate. Public-origin
Connect pages therefore do not depend on the daemon-origin WebSocket for active
voice handoff, voice event logging, live voice tool/query dispatch, or frame
archival. Server-side transcription audio remains intentionally untunneled for
now; Connect mode drops that optional audio stream rather than replaying it over
the legacy bridge.
Current-upload list reads use `api_session_current_uploads`, returning the same
staged-upload descriptor array as `GET /api/session/current/uploads`. The Files
tab shows this as its staged-upload list and can remove entries with
`api_session_current_upload_delete`.
Current-upload raw reads use `api_session_current_upload_raw` over
`byte_stream_*` frames. The request names an uploaded attachment id and may
include `offset`/`length`; the response carries `range_start`, `range_end`,
`total_size`, and `resumable: true` metadata with the returned bytes. The Files
tab uses repeated ranged reads to download staged uploads back to the browser.
This is a bounded current-session attachment primitive, not yet a general
daemon-filesystem upload/download adapter.
Worktree cached inventory reads, explicit scans, guarded removals, and the
session finish card's merge use `api_worktrees`, `api_worktrees_scan`,
`api_worktrees_remove`, and `api_worktrees_merge`; the writes use the same
no-replay fallback rule as other writes.
The filesystem picker's path checks, directory listings, and mkdir operation use
`api_fs_stat`, `api_fs_list`, and `api_fs_mkdir`; mkdir uses the same no-replay
fallback rule as other writes.
Bounded filesystem file reads use `api_fs_read` when the verified tunnel
advertises byte streams. The request uses the same absolute-path or `~/` path
rules as the picker, rejects directories, accepts optional `offset`/`length`,
and returns bytes plus `content_type`, `range_start`, `range_end`, `total_size`,
and `resumable: true` metadata. The Files tab exposes this as the download side
of its transfer center: users can type a path or browse with the filesystem
picker, queue downloads, pause/cancel/retry, and resume from completed ranges
inside the current browser session. Public-origin Connect mode does not fall
back to daemon HTTP for this path. The queue/history and partially completed
ranges are browser-local state, not daemon-side transfer records.

Daemon-origin dashboards reached directly over native mTLS use the same Files
transfer center but read arbitrary files through `GET /api/fs/read?path=...`
with ordinary HTTP `Range` requests. The endpoint follows the same path rules,
rejects directories, advertises `Accept-Ranges: bytes`, returns `206 Partial
Content` plus `Content-Range` for ranged reads, and returns `416 Range Not
Satisfiable` with `Content-Range: bytes */total` for invalid ranges. This keeps
direct mTLS downloads resumable without routing them through the Connect
DataChannel. Connect dashboards intentionally keep using `api_fs_read` over the
verified tunnel and never fall back to daemon-origin HTTP.
Lazy exact context-snapshot loads use `api_session_context_snapshot`, keeping
large raw request payloads out of ordinary session-detail hydration while still
allowing the Context pane to fetch a single archived snapshot on demand.
Staged upload deletion uses `api_session_current_upload_delete` so removing a
pending attachment can travel over the verified control channel. Browser image
chips now prefer `api_session_current_upload_raw` and render the returned bytes
through a local blob URL, falling back to the legacy raw HTTP URL only when the
tunnel is unavailable or preview loading fails.
OpenAI browser live-audio token minting uses `api_voice_session`; it preserves
the existing `/session` behavior and error envelope while avoiding a direct
dashboard HTTPS POST when the verified control channel is available.
Dashboard-originated managed-context MCP actions use `api_mcp_tool_call`, which
wraps a single `tools/call` against the daemon's existing MCP server. These
calls use the same no-replay fallback rule as other writes because tools such
as `rewind_context`, `fission_control`, and `fission_spawn` mutate live session
state.
Confirmed session-data deletion uses `api_session_delete` with the same
no-replay fallback rule as other writes; the dashboard still requires the
existing confirmation modal before issuing the RPC.
Peer-display WebRTC signaling uses `api_peer_webrtc_signal`, carrying the same
`display_id`, `session_id`, and `signal` body as `POST /api/peers/{id}/webrtc`
plus the target `peer_id`. Answers and remote ICE still arrive asynchronously
through the normal peer-event path; the RPC only confirms that the signal was
accepted for forwarding. Failed tunneled signaling requests are not replayed
over HTTP after a verified tunnel attempt.
Dashboard session-control actions use `api_session_control_msg`. This includes
create/start/resume/stop/restart session, targeted follow-up, mid-turn steer,
cancel queued steer/follow-up, edit user message, interrupt, approvals,
session rename, and per-session launch-config persistence. The browser only
falls back to the WebSocket before it has attempted the RPC; once a verified
DataChannel write is sent, an error is surfaced to the operator instead of
replaying a potentially duplicated action.
Small dashboard action controls use `api_dashboard_action_msg`. This covers
Codex attached-thread actions, local display authority toggles, the
diagnostics visual-marker toggle, recording and debug screen controls, and
browser workspace create/acquire/close/release. The browser applies the same
no-replay fallback rule: use the WebSocket only before a verified DataChannel
request is attempted, then surface RPC failures instead of duplicating a
potentially state-changing action. For `set_diagnostics_visual_marker`, the
daemon applies the request directly to the active display registry when
available, or records the desired state as a pending per-display default for
the next display session.

The remaining migration work is mostly byte-stream and file-transfer heavy:
native media fallback URLs, broader bidirectional file transfer, durable
cross-refresh resume tokens, and any remaining non-allowlisted control
mutations should move only after resumable stream/file-transfer semantics and
per-action no-replay rules are settled.

The oversight bar exposes the selected control transport. Direct
dashboard access shows the existing HTTP/mTLS path, while opt-in WebRTC control
shows `checking`, verified `WebRTC`, `relay` when browser ICE stats report a
TURN-relayed candidate pair, or `failed` when signaling or daemon-binding
verification fails. The tooltip carries the detailed state that is also exposed
through `window.intendantDashboardControl.status()`. In public-origin Connect
mode, the legacy `ws` indicator is relabeled to `events`; it turns green only
after the verified DataChannel has hydrated dashboard bootstrap events, since no
same-origin daemon WebSocket is expected in that mode.

Peer access-request APIs now use the same transport boundary. The dashboard's
pairing/request panes call `api_peer_pairing_requests`,
`api_peer_pairing_request_decision`, invite/join/request-access/poll, identity
list, and identity revoke over the DataChannel when it is connected. Mutating
pairing operations deliberately fail rather than silently falling back after a
WebRTC RPC error, so an operator does not approve or mint credentials over an
unexpected transport.
Pairing authorization follows the access/peer split: request and identity lists
require `access.inspect`, invite/approve/revoke require `access.manage`, and
join/request-access/poll remain peer-topology operations gated by `peer.manage`.
Acting through an already-connected peer is gated by `peer.use` instead —
using a peer relationship is not administering it. That covers the signaling
relays that open tunnels (`api_peer_webrtc_signal`,
`api_peer_file_transfer_signal`, `api_peer_dashboard_control_signal`, and
their `POST /api/peers/{id}/…-webrtc` HTTP twins) and the quick controls
(`api_peer_message`, `api_peer_task`, `api_peer_approval`, and their
`POST /api/peers/{id}/message|task|approval` HTTP twins): each delegates this
daemon's peer identity, and the receiving peer authorizes the action against
its own grants for this daemon.
General peer and coordinator controls are covered by the same rule. Peer add,
remove, eligibility discovery, per-peer message/task/approval, peer-display
signaling, and coordinator route calls use `api_peer_add`, `api_peer_remove`,
`api_peer_eligible`, `api_peer_message`, `api_peer_task`, `api_peer_approval`,
`api_peer_webrtc_signal`, and `api_coordinator_route` over the verified tunnel.
They preserve the existing HTTP endpoint metadata (`_httpStatus`/`_httpOk`) so
the dashboard can render the same success and error states on either transport,
but state-changing calls do not replay over HTTP once a verified DataChannel
request has been attempted.

#### Relationship to Existing Auth Modes

This design should not remove local/offline mTLS. It gives the product two clear
dashboard access modes:

- **Consumer cloud-assisted mode:** public Intendant Connect origin for
  account/rendezvous UX, with dashboard access still gated by daemon-local IAM.
- **Local/offline/power-user mode:** direct daemon HTTPS/WSS with browser mTLS
  enrollment, as implemented today.

Peer daemon-to-daemon trust remains separate. Humans may use passkeys to approve
a peer access request, but the resulting daemon-to-daemon connection should
still use Intendant-issued peer-scoped mTLS certificates unless the federation
trust model is deliberately redesigned. In user-facing copy, that should appear
as "grant access to this daemon" and "revoke access," not as manual certificate
management.

In other words, Connect and browser mTLS authenticate a **user/client route to a
daemon**. Peer mTLS authenticates a **daemon route to another daemon**. The
dashboard can present both as targets, but target selection is only a product
abstraction; it does not collapse the two security domains.

#### Status and Remaining Rollout

The current implementation has crossed from protocol sketch into hosted MVP:

1. Direct mTLS dashboard access remains the default local/offline path.
2. The daemon has a persistent daemon identity key and can expose a
   dashboard-control WebRTC DataChannel.
3. The real SPA has a `connect=1` public-origin mode and a
   `DashboardTransport` boundary for tunneled reads, streams, bounded byte
   transfer, uploads, terminal frames, selected control messages, peer
   pairing actions, local display signaling, and media/editor writes.
4. The daemon has a disabled-by-default outbound Connect polling client.
5. `intendant-connect` provides the hosted production alpha: passkey-only
   account sessions, daemon registration, claim-phrase route ownership proof,
   rendezvous signaling, labels, revoke, active tunnel close, rate limits, CSRF
   protection, readiness checks, and audit.
6. The daemon refuses Connect dashboard-control offers unless the routed Connect account
   matches daemon-local IAM; Connect account ownership is no longer an implicit
   root grant.
7. The browser and hosted service both verify that the daemon-signed WebRTC
   binding matches the registered daemon identity and Connect-issued routing
   grant.
8. Focused validators cover the local bootstrap, local rendezvous emulator, and
   hosted Connect MVP paths.
9. `connect.intendant.dev` has a repeatable production-alpha deploy path plus
   encrypted state backup/restore scripts.

The remaining rollout work is production operations, breadth, and making the
local trusted Access app pleasant enough that hosted Connect can stay a
rendezvous/convenience layer:

1. Add durable/database-backed rate limits, structured metrics, and database
   migrations.
2. Add account recovery, richer multi-device management, teams/roles, and optional
   passkey step-up for sensitive actions.
3. Add daemon identity rotation/recovery semantics for VM clones, disk restore,
   and deliberate transfer of ownership.
4. Continue migrating remaining dashboard APIs only when the tunnel has the
   required streaming, byte-range, resumable transfer, or media semantics.
5. Keep direct mTLS dashboard access and peer daemon-to-daemon mTLS working
   throughout.

Non-goals for this path:

- no loopback or same-host bypass of dashboard authentication;
- no native host app requirement for the general web dashboard;
- no passkeys as daemon-to-daemon federation credentials;
- no silent downgrade from verified direct WebRTC to opaque relay;
- no attempt to obtain public certificates for private VM IPs or `.local`
  names.

Remaining design questions before production rollout:

- Do we want an additional app-integrity story such as signed static assets,
  pinned PWA bundles, or browser extension packaging for users who want trusted
  local dashboard code without a native app?
- How are daemon identity keys backed up, rotated, revoked, and recovered after
  VM cloning or disk restore?
- What local policy does the daemon enforce when Connect says a signed-in user
  wants access?
- Do browser WebRTC privacy policies, enterprise restrictions, or future Private
  Network Access rules constrain direct DataChannels from a public origin to
  LAN/VM candidates?
- What is the visible product distinction between "direct," "TURN-relayed," and
  "application-proxied" dashboard transport?
- What audit log should exist for passkey logins, daemon claims, step-up
  approvals, and peer certificate issuance?

## HTTP endpoints

Routing matches the parsed `(method, path)` — exact routes or their
`/`-nested sub-routes, query string stripped — so the dispatch chain and the
per-route IAM/Origin gates always classify a request identically. Grouped by
family (sub-routes elided where the family is uniform):

| Endpoint | Description |
|----------|-------------|
| `GET /` | The dashboard SPA |
| `GET /config` | Live-model configuration JSON (provider, model, sample rates, git SHA) |
| `GET /debug` | Debug JSON (agent state, voice connection, active browser) |
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime |
| `GET /wasm-web/*`, `GET /wasm-station/*` | Compiled WASM + JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for mic capture |
| `GET /.well-known/agent-card.json` | Agent card (identity + capabilities) for peers and integrations |
| `POST /mcp` | Streamable-HTTP MCP server (per-tool IAM; see [MCP server](./mcp-server.md)) |
| `WS /` or `WS /ws` | Main WebSocket: events, fallback Shell terminal I/O, presence protocol, WebRTC signaling |
| `GET /api/sessions` | List past sessions (`/stream` NDJSON variant, `/search` full-text) |
| `GET /api/session/{id}/*` | Per-session detail, report, log replay, context snapshots, recordings, frame assets |
| `POST /api/session/{id}/agent-output` | Fetch persisted agent output by id for a historical/session-scoped transcript (POST-shaped read: the ids ride the body) |
| `DELETE /api/session/{id}[/{target}]`, `POST /api/session/{id}/delete[/{target}]` | Delete archived session data (native DELETE plus WKWebView fallback) |
| `GET /api/session/current/history`, `GET /api/session/current/changes[/*]` | Current-session reads: serialized history and file-change list/detail |
| `POST /api/session/current/{rollback,redo,prune,agent-output}` | Current-session actions: history rollback/redo/prune mutations, plus the POST-shaped agent-output fetch |
| `GET /api/session/current/uploads[/*]`, `POST /api/session/current/uploads`, `DELETE /api/session/current/uploads/{id}` | Task attachment store: list, upload, raw fetch, delete |
| `GET /api/managed-context/{records,anchors,fission}` | Managed-context state: rewind records, anchors, fission groups |
| `GET /recordings/*`, `GET /frames/*` | Current-session recording segments and captured frame assets |
| `GET /api/fs/{stat,list,read}`, `POST /api/fs/{mkdir,write}` | Scoped filesystem browsing and editor writes (fs scope enforced per grant) |
| `GET/POST /api/settings`, `POST /api/api-keys`, `GET /api/api-key-status`, `GET /api/project-root` | Settings and provider-key management |
| `GET /api/external-agents` | External-agent backend availability (configured command, installed, auth posture: active oauth lease / local login, last used) — drives the fueling nudge and new-session picker |
| `GET /api/displays`, `POST /api/diagnostics/visual-freshness` | Display inventory; visual-freshness probe marker |
| `GET /api/access/{overview,iam/state}`, `GET /api/dashboard/targets` | Trust-architecture snapshots (IAM state, fleet targets) |
| `POST /api/access/...` | Trust mutations: enrollment decide, IAM grant upsert/update, org trust/revoke, org-grant issue/renew/revoke-member, issuer init/delegate/install, revocation-list apply |
| `GET /api/peers[/*]`, `POST /api/peers[/*]`, `DELETE /api/peers` | Peer federation: registry reads (GET), pairing + management/signaling (POST), registry removal (DELETE) |
| `POST /api/coordinator/route` | Multi-agent coordinator task routing (peer lane) |
| `GET /api/worktrees`, `POST /api/worktrees/{inspect,scan,remove,merge}` | Agent worktree inventory and lifecycle (merge = session-linked worktree finish card) |
| `GET /connect/{bootstrap,status}`, `POST /connect/dashboard/{offer,ice,close}` | Intendant Connect tunnel: bootstrap metadata and dashboard-control WebRTC signaling |

### Declared API routes

Every `/api/*` and `/mcp` route is declared exactly once in
`gateway_routes::ROUTES` (`src/bin/caller/gateway_routes.rs`); dispatch, the
pre-dispatch IAM classification, and the OPTIONS preflight (CORS posture +
allowed-method union) all derive from those declarations, and the table below
is rendered from them. A unit test (`endpoint_docs_match_chapter`) fails when
the chapter and the code drift; regenerate with
`cargo test --bin intendant endpoint_docs_match_chapter -- --nocapture` and
paste the printed block between the markers. Authorization names the
`PeerOperation` the IAM gate evaluates; `public` routes carry their authority
in the payload itself (signature/shape), and the federation surface derives
its operation per method/path from `federation_http_operation`.

<!-- gateway-route-table:begin (generated; do not edit by hand) -->
| Method | Path | Authorization | CORS | Body | Description |
|---|---|---|---|---|---|
| GET | `/api/fs/stat` | FilesystemRead | own origin | none | Stat a filesystem path (scope-checked) |
| GET | `/api/fs/list` | FilesystemRead | own origin | none | List a directory (scope-checked) |
| GET | `/api/fs/read` | FilesystemRead | own origin | none | Read file bytes (scope-checked; supports byte ranges) |
| POST | `/api/fs/mkdir` | FilesystemWrite | own origin | bounded | Create a directory (scope-checked) |
| POST | `/api/fs/write` | FilesystemWrite | own origin | ≤ 150 MiB | Write file bytes (scope-checked; sha256-guarded overwrite) |
| POST | `/api/fs/rename` | FilesystemWrite | own origin | bounded | Move/rename a file or directory (scope-checked) |
| POST | `/api/fs/delete` | FilesystemWrite | own origin | bounded | Delete a file or directory (scope-checked) |
| GET | `/api/session/current/changes[/…]` | SessionManage | own origin | none | List the session's changed files, or the unified diff for one file (subpath) |
| GET | `/api/session/current/history` | SessionManage | own origin | none | Serialized rollback History for the current session |
| POST | `/api/session/current/rollback` | SessionManage | own origin | bounded | Roll the current session back to a round (optionally reverting files) |
| POST | `/api/session/current/redo` | SessionManage | own origin | bounded | Redo the last rolled-back round |
| POST | `/api/session/current/prune` | SessionManage | own origin | bounded | Prune rollback state for the current session |
| POST | `/api/session/current/agent-output` | SessionManage | own origin | bounded | Fetch the current session's persisted agent output by id (POST-shaped read) |
| POST | `/api/session/current/uploads` | SessionManage | own origin | streaming | Upload a file attachment (raw streamed body; name/destination in query) |
| GET | `/api/session/current/uploads[/…]` | SessionManage | own origin | none | List uploads, or fetch one (subpath {id}/raw) |
| DELETE | `/api/session/current/uploads/{upload_id}` | SessionManage | own origin | none | Delete one upload (file + sidecar) |
| DELETE | `/api/session/{id}` | SessionManage | own origin | none | Delete a session's data |
| DELETE | `/api/session/{id}/{target}` | SessionManage | own origin | none | Delete one data kind for a session (recordings, frames, …) |
| DELETE | `/api/session/{id}/{target}/delete` | SessionManage | own origin | none | Delete one data kind for a session (suffix form) |
| POST | `/api/session/{id}/delete` | SessionManage | own origin | none | Delete a session's data (POST fallback for WKWebView) |
| POST | `/api/session/{id}/{target}/delete` | SessionManage | own origin | none | Delete one data kind for a session (POST fallback) |
| POST | `/api/session/{id}/agent-output` | SessionInspect | own origin | bounded | Fetch a session's persisted agent output by id (POST-shaped read) |
| GET | `/api/session/current[/…]` | SessionManage | own origin | none | Current-session detail and artifact sub-routes |
| POST | `/api/session/current[/…]` | SessionManage | own origin | none | Current-session detail sub-routes (POST fallback callers) |
| GET | `/api/session/{id}/context-snapshot` | SessionInspect | own origin | none | Replay one archived context snapshot (file/request_id/request_index/ts selector) |
| GET | `/api/session/{id}` | SessionInspect | own origin | none | Session detail (paged replay entries; limit/before/source) |
| GET | `/api/session[/…]` | SessionInspect | own origin | none | Session artifact sub-routes: recordings (+segments/playlist), report zip, frames |
| POST | `/api/session[/…]` | SessionManage | own origin | none | Session detail sub-routes (POST fallback callers) |
| GET | `/api/managed-context/anchors` | SessionInspect | own origin | none | Managed-context anchor catalog |
| GET | `/api/managed-context/records` | SessionInspect | own origin | none | Managed-context record index |
| GET | `/api/managed-context/fission` | SessionInspect | own origin | none | Managed-context fission state |
| POST | `/api/worktrees/inspect` | SessionInspect | own origin | bounded | Inspect one worktree (branch, ahead/behind, dirty state) |
| POST | `/api/worktrees/remove` | SessionManage | own origin | bounded | Remove a worktree from the inventory |
| POST | `/api/worktrees/merge` | SessionManage | own origin | bounded | Merge a session's linked worktree branch into its base checkout, then remove the checkout |
| POST | `/api/worktrees/scan` | SessionManage | own origin | none | Rescan the worktree inventory (refreshes the cache) |
| GET | `/api/worktrees` | SessionInspect | own origin | none | Cached worktree inventory |
| GET | `/api/sessions/stream` | SessionInspect | own origin | none | NDJSON stream of the session list |
| GET | `/api/sessions/search` | SessionInspect | own origin | none | Search sessions (q, source, mode, project filters) |
| GET | `/api/sessions` | SessionInspect | own origin | none | List sessions (id filter, limit, usage view; response CORS * for the fleet Stats tab) |
| GET | `/api/project-root` | Settings | own origin | none | Project root path this daemon serves |
| POST | `/api/settings` | Settings | own origin | bounded | Update runtime settings |
| GET | `/api/settings` | Settings | own origin | none | Current runtime settings |
| POST | `/api/api-keys` | Settings | own origin | bounded | Store provider API keys in the project .env |
| GET | `/api/api-key-status` | Settings | own origin | none | Which provider keys are configured (presence only) |
| GET | `/api/external-agents` | SessionInspect | own origin | none | Detected external coding agents (codex, claude) |
| POST | `/api/diagnostics/visual-freshness` | DisplayInput | own origin | ≤ 16 MiB | Visual-freshness diagnostics transcript sink (NDJSON body) |
| GET | `/api/displays` | DisplayView | own origin | none | Enumerate active displays |
| any | `/api/peer-pairing/requests[/…]` | public | public | streaming | Peer access-request doorbell: knock (POST, size-capped) or poll one request's status (GET subpath) |
| POST | `/api/access/org-grants` | public | public | ≤ 16 KiB | Present a signed org grant document (verified against locally trusted org keys) |
| GET | `/api/access/orgs/{org_handle}/revocations` | public | public | none | Org revocation list (ORL) for a trusted org |
| POST | `/api/access/orgs/revocations/apply` | public | public | ≤ 64 KiB | Apply a signed org revocation list |
| POST | `/api/access/org-grants/renew` | public | public | ≤ 16 KiB | Renew an org grant document (signed payload) |
| POST | `/api/access/iam/user-client-grants` | AccessManage | fleet allowlist | bounded | Upsert a user-client grant |
| POST | `/api/access/iam/grants/update` | AccessManage | fleet allowlist | bounded | Update an IAM grant |
| POST | `/api/access/orgs/trust` | AccessManage | fleet allowlist | bounded | Trust an org root key on this daemon |
| POST | `/api/access/orgs/revoke` | AccessManage | fleet allowlist | bounded | Withdraw trust in an org root key |
| POST | `/api/access/org-grants/issue` | AccessManage | own origin | bounded | Issue an org grant (org root/issuer key on this daemon) |
| POST | `/api/access/org-grants/revoke-member` | AccessManage | own origin | bounded | Revoke an org member (appends to the ORL) |
| POST | `/api/access/org-grants/issuers/init` | AccessManage | own origin | bounded | Initialize an org issuer key |
| POST | `/api/access/org-grants/issuers/delegate` | AccessManage | own origin | bounded | Delegate to an org issuer |
| POST | `/api/access/org-grants/issuers/install` | AccessManage | own origin | bounded | Install a delegated org issuer key |
| POST | `/api/access/enrollment-requests/decide` | AccessManage | fleet allowlist | bounded | Approve or deny a pending enrollment request |
| GET | `/api/access/enrollment-requests` | AccessInspect | fleet allowlist | none | Pending enrollment requests |
| GET | `/api/access/iam/state` | AccessInspect | fleet allowlist | none | Local IAM state (roles, grants, bindings) |
| GET | `/api/access/overview` | AccessInspect | fleet allowlist | none | Access overview for the calling principal |
| GET | `/api/access/connect/status` | AccessInspect | fleet allowlist | none | Connect rendezvous status (claim state, binding provenance; no claim phrase) |
| GET | `/api/access/connect/claim-code` | AccessManage | fleet allowlist | none | Reveal the current twelve-word claim phrase (unclaimed daemons only) |
| POST | `/api/access/connect/config` | AccessManage | fleet allowlist | bounded | Enable/disable the Connect client (persists to intendant.toml, applies live) |
| POST | `/api/access/connect/unclaim` | AccessManage | fleet allowlist | bounded | Release this daemon's claim binding at the rendezvous (daemon-signed) |
| POST | `/api/access/tier` | AccessManage | fleet allowlist | bounded | Set this daemon's trust tier label (integrated/disposable; null clears) |
| POST | `/api/access/hosted-ceiling` | AccessManage | fleet allowlist | bounded | Set the hosted-control ceiling role for hosted-provenance sessions |
| GET | `/api/dashboard/targets` | AccessInspect | own origin | none | Dashboard target list (this daemon + connected peers) |
| any | `/api/peers[/…]` | federation (per method/path) | own origin | bounded | Peer registry (list/add/remove), pairing (invite/join/requests/identities), eligibility, and per-peer quick controls + signaling relays |
| POST | `/api/coordinator/route` | federation (per method/path) | own origin | bounded | Capability-based task routing through the Coordinator |
| POST | `/mcp` | MCP token | own origin | ≤ 16 MiB | MCP Streamable HTTP endpoint (JSON-RPC requests + notifications) |
| GET | `/mcp` | MCP token | own origin | none | MCP SSE stream (405: stateless server) |
| DELETE | `/mcp` | MCP token | own origin | none | MCP session delete (405: stateless server) |
<!-- gateway-route-table:end -->

The full WebSocket message protocol (inbound key/resize/presence/WebRTC frames,
outbound term/state/log-replay/tool-response frames) and the gateway's internal
layering are documented in [Integrations → Web Gateway](./integrations.md#web-gateway).
Dashboard session-control actions use the `api_session_control_msg`
dashboard-control RPC; there is no HTTP `control-msg` route.
