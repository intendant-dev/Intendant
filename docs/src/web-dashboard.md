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
- `http://<host-ip>` is not a secure context. Use default native mTLS or
  `--tls` with a trusted certificate for public/authority-free bytes. The
  packaged macOS app's local bridge also supplies a secure context for local
  development, but no Developer ID-signed/notarized release exists for this
  alpha, so its current artifact is not a distribution anchor. A generic Caddy/nginx HTTPS reverse proxy supplies
  encryption and a secure context, not client authentication. If it forwards
  to daemon loopback, the proxy is itself a root trust boundary and must enforce
  approved client identity while protecting its upstream from other callers.
  The macOS app wrapper starts its bundled backend with
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
the tooltip lists every window. Session-facts chips (the vitals `config`
section, wire-first from each backend's own echoes with the launch config
as the honestly-marked fallback) show the model + configured reasoning
effort (`⬡ fable-5 · max`) and the permission mode in plain words
(`🛡 Acts without asking`) — bypass/ungated modes get a quiet warning
tint (cosmetic, never the health dot), the raw backend vocabulary
(`bypassPermissions`, `workspace-write · on-request`, the autonomy level)
is one tap away in the explainer, and the vitals pane always lists both
rows, degrading to "not reported" instead of hiding. When a cache
countdown enters its final minute the dashboard raises one toast per idle
period (plus a browser notification if permission was already granted and
the tab is hidden) — early enough that a follow-up still reuses the warm
cache, never after the fact. Sections appear as producers fill them; the
chip hides in narrow windows. Station's agent focus panel shows the same
vitals as git / cache / limits rows.

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
- Per-model breakdown for the main and presence models (prompt, completion,
  cache-read, and cache-write token counts), with a token-pressure meter per card
- Cost estimates from a built-in pricing table (OpenAI, Anthropic, Gemini),
  including the distinct GPT-5.6 Sol/Terra/Luna input, cache-write, cached-read,
  and output rates
- Token activity: a daily skyline and a GitHub-style year heatmap on the
  validated single-hue `--viz-*` ramp, filterable by agent and period
- All-sessions cumulative usage and disk usage
- Display-transport metrics (frame rate, encode latency, bandwidth per display)

### Terminal

An embedded xterm.js terminal hosting an interactive **Shell** session on the
daemon (or a selected peer). Session monitoring and control live in the
Activity/Station tabs, not here.

### Live display

The Computer Use workspace combines a selected WebRTC stage with a live rail
for displays, input authority, browser-observed display activity, peer
launchers, and the user's own screen (see
[Display Pipeline](./display-pipeline.md)):

- **View mode** (default) — watch the agent's display in real time
- **Take Control** — forward mouse and keyboard events to the agent's display
- **Release** — relinquish control, with an optional note
- **Selected display stage** — switch among active local displays without
  recreating their capture sessions or video elements. A shared-view request
  selects its target once unless that would interrupt active human input,
  annotation, callout, or full-screen work; later manual selection is
  respected.
- **Input authority** — the toolbar and rail project the same server truth:
  you, another viewer, available, or connecting. Hiding an interactive or
  pending display first flushes held keys and mouse buttons, then releases it
  before input listeners are removed. Editable annotation/callout work blocks
  navigation instead of being discarded.
- **Display activity** — reports real connection, authority, visibility,
  streaming, recording, annotation, callout, and shared-view transitions,
  plus the daemon's per-action `cu_action` lane: every executed CU action
  renders as a two-line row (friendly sentence + raw call like
  `left_click(612, 233)`) and drives the stage overlays — agent cursor with
  verb pill, click ripple, keypress chips, screenshot flash. The feed shows
  only what the daemon actually reported (failed actions never render, and
  the lane is ephemeral: no session log, no replay, no peer forwarding).
- **Approval card** — when a pending approval's session is the reported
  driver of the selected display, an amber card in the rail proxies the
  approval panel's own Approve/Deny; without real display→session
  attribution it stays hidden.
- **Peer displays** — open on Station, whose viewer understands federated
  display targets, rather than masquerading as local stages.
- **Responsive controls** — below the desktop breakpoint the rail becomes a
  keyboard-accessible **Displays & input** drawer; primary controls remain in
  the stage toolbar.
- **Recording replay** — browse and play back recorded sessions with timeline
  seeking and speed control (1x / 2x / 4x). Live recording controls show a
  pending command but change REC/activity state only on daemon confirmation.
  Starting a recording is owner/root-only for the alpha and succeeds only for
  an active agent-visible display. Private views are never recordable: their
  pixels must not enter the ordinary session artifact tree, where recording
  and filesystem APIs follow their existing broader grants.

The live rail's **Your screen** card keeps the three screen-on-the-wire
concepts separate (see
[Computer Use](./computer-use-and-audio.md#three-separate-concepts-private-view-agent-share-presence-streaming)):

- **View this machine** — a private remote view: an owner/root dashboard may
  watch and control this machine's display. The agent cannot see it — the
  session is `agent_visible = false` and every agent-facing display lookup
  skips it. The tile wears a **Private view** chip and the live rail row a
  `PRIVATE` tag.
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

The private-view ceiling is identical on the legacy `/ws` transport and the
verified dashboard-control DataChannel: only an owner/root dashboard may
create a private user-session display, enumerate it through the live registry,
receive its media, or send it input. Generic `display.view` and
`display.input` permissions enumerate and control only agent-visible displays.
The browser bootstrap filters explicit private ready/grant records so replay
cannot recreate a private display tile for a scoped client. Display lifecycle
failure/teardown records do not carry the original visibility bit, however,
so live fanout and historical session/event logs may disclose audit metadata
such as a display ID, capture failure, or revocation. The private-view ceiling
protects pixels and control; it is not an existence-metadata secrecy promise
and does not rewrite owner audit history. Revocation remains available to an
otherwise authorized scoped caller as a de-escalation path.

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
  is how a newly authorized headless box — no display server, no API key —
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
- **Worktrees** — the git worktree inventory (same card + Show-more
  treatment): per-checkout size, merge/dirty state, and safety verdicts;
  aggregate tiles including **free disk** on the tightest worktree-hosting
  volume (amber under 10% free, rose under 5%) and **reclaimable** build
  output; and **related-session chips** — the sessions observed inside
  each checkout, supervised and raw codex/claude alike. Clicking a chip
  focuses the live session window when one exists, otherwise it lands on
  Recent with the ID prefilled. Checkouts with a CACHEDIR.TAG-marked Cargo
  `target/` offer **Clean target/** — delete the build directory to
  reclaim its bytes without removing the worktree (sources and git state
  untouched; a warning notes active sessions first).
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
  External Codex sessions can choose the binary path, a model, a compatible
  reasoning effort, sandbox and approval policies, `managed_context` mode
  (`vanilla` or `managed`), context replay mode, and Fast service tier for that
  session. The model picker derives its normal choices and per-model reasoning
  levels from the signed-in Codex account's `models_cache.json`; hidden entries
  stay out, and API-key auth also filters subscription-only models exactly as
  Codex does. A conservative offline catalog and a Custom-id escape cover
  pre-fetch, staged, or private models. Blank inherits the global/Codex default;
  when a selected model cannot inherit the global effort, the picker explicitly
  selects that model's advertised default. Model and effort pins persist across
  attach/resume. The external-agent options sit in a fold
  that opens when an external backend is selected. Claude Code sessions get per-launch
  dropdowns for the model (version-safe aliases — `fable`, `opus`, `sonnet`,
  `haiku` — that the CLI resolves to the latest release, with a Custom-id escape
  for full model names), the permission mode, and the reasoning effort
  (`low` … `max`).

**Settings → Providers & models** exposes the daemon's global Codex and Claude
defaults independently of whichever backend is currently selected. With an
attached project, Save writes that project's `intendant.toml`; a projectless
daemon writes `<state-root>/intendant.toml` (normally
`~/.intendant/intendant.toml`) while `/api/project-root` remains `null`. These
defaults seed new sessions, but an existing session's persisted launch config
continues to control its next attach/resume.

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
summary uses the same access abstraction as Terminal: local/mTLS and peer
dashboard-control routes are shown as targets with their available
capabilities rather than as transport internals. Hosted Connect directory
records are not Files or Terminal targets: they carry no control URL or
authority and appear only as non-operable discovery/remembered metadata.

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
  role badge there, capability chips, and Stats/Files/Shell/Display actions for
  local, mTLS, or peer-operable targets. Connect-only directory records never
  become Files/Terminal targets and expose no such actions.
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
- **Diagnostics** owns route health, including hosted Connect directory/link
  status, local/mTLS, trusted local WebRTC control, event delivery, byte streams, uploads, and
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

Access uses one vocabulary across trusted daemon-served loopback/direct-mTLS
dashboards and peer federation. The packaged macOS app contains a local mTLS
bridge, but no signed/notarized distribution exists for this alpha. Hosted Connect
contributes directory metadata only; it is not an Access or control surface:

- A **target** is a daemon the trusted dashboard can operate. A Connect-linked
  directory record is navigation metadata, not a target, until it carries a
  separately trusted direct route.
- A **principal** is the actor being trusted: the current local session, an
  mTLS browser certificate, or a peer daemon. Browser identity-key records and
  future organization groups also exist in the model, but keys are not an
  active alpha login credential. `connect_account` remains in the IAM vocabulary for compatibility,
  but a hosted account assertion is route metadata and does not authenticate to
  the daemon.
- A **grant** connects one principal to one target with a role and status. A
  loopback browser or a verified owner-mTLS browser is root-compatible with the
  local daemon. A hosted-origin key may exist in IAM and may be assigned a
  scoped grant for use after trusted re-enrollment, but presentation through
  Connect cannot exercise that grant or open a control session. A peer route
  has a daemon peer-profile grant. An
  approved inbound peer identity appears as
  a peer-daemon principal with a peer-profile grant to this daemon; revoked
  identities remain visible as revoked grants for audit clarity. Local IAM
  grants loaded from `iam.json` are enforced when active and bound to an
  authentication mechanism the transport actually supplies. In this alpha
  that browser mechanism is mTLS, not a browser identity key. A `human_user`
  record can carry account/provider and organization metadata, but Connect
  account metadata alone is not an authenticating binding. Draft and revoked
  records remain visible for review.
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
  or sending it a message, task, or approval decision, directly or via
  coordinator routing — presents *this
  daemon's* peer credentials, and the receiving peer authorizes each action
  against its own grants for this daemon — so relaying is
  never inferred from local capabilities, it is granted by name
  (`operator` and `peer-user` carry it; `peer.manage` implies it for
  compatibility). Owner/root dashboard sessions have all of these. Existing
  peer profiles are mapped conservatively: `peer-root` can inspect access and
  inspect/manage/use peer topology, but `access.manage` remains reserved for
  trusted root user/client sessions. Display permissions carry the same
  private-view ceiling: `display.view` and `display.input` reach only
  `agent_visible` sessions; private user-session displays additionally require
  an owner/root dashboard.
- A **transport** is only how an authorized route is carried: browser mTLS,
  loopback HTTP, trusted daemon-origin WebRTC control, or daemon-to-daemon peer
  mTLS. Hosted Connect is a directory/link surface, not an access transport.

The browser may also maintain a local fleet registry for navigation: daemon ids,
labels, remembered URLs, and the route/auth summary last seen from a daemon. This
registry is client-side metadata, not an authorization source. If a remembered
target is no longer configured on the current daemon, Access shows it as a
browser-local record with operation buttons disabled. The daemon still owns IAM
enforcement for every request.

The hosted `/connect` directory maintains account-scoped fleet navigation
metadata through `GET /api/fleet/targets`,
`POST /api/fleet/targets/sync`, and
`POST /api/fleet/targets/{target_id}/forget`. The hosted service stores only
navigation metadata: target ids, labels, route labels, URLs, capability hints,
and last-seen timestamps. It does not store browser mTLS private keys, daemon
IAM grants, peer secrets, dashboard session grants, or passkey private material.
Linked Connect daemons are merged into that directory list as live
`connect_daemon` records and override stale remembered labels. Records the
browser pushes are signed with its identity key and re-verified after every
round trip; target rows and the fleet strip badge each synced record as
verified (this browser), signed (another device), unverified, or a hosted
claim. The signer key is carried inside the record and is not yet anchored to
an owner/device trust set. The current browser detects same-key alteration,
but a malicious store can substitute a fresh, internally valid self-signed
record on another device. Connect-served code can also wield the hosted
origin's browser key while loaded. These signatures are metadata
integrity/attribution hints, not daemon authentication. The former hosted
`/app?connect=1&daemon_id=...` dashboard route is retired and always redirects
to `/connect`; linked targets expose no dashboard URL or Open action. Direct mTLS
dashboards on daemon origins remain local-first; cross-origin sync to
`intendant.dev` is a separate explicit-consent design problem, not something the
current cookie model does silently.

The important security-domain split is:

- **User/client daemon access** means a human-operated trusted dashboard can
  control a daemon. Browser/native mTLS certificates are the shipped remote
  credential; browser identity keys are record-only in this alpha. Unbound browser mTLS owner sessions remain
  root-compatible only when the browser is loopback or presents the verified
  owner client certificate. Hosted Connect passkeys authenticate only the
  directory account; Connect browser signaling is disabled at the service and
  legacy Connect control events are dropped by the daemon. A local grant does
  not turn that page into a control session. Future
  coworker/team access belongs on trusted user/client surfaces, not in peer
  federation.
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
the current transport/auth paths rather than replacing mTLS, browser-key
checks, or peer profiles.

The local IAM foundation lives beside the native access cert store as
`iam.json` and is also available at `GET /api/access/iam/state` or
dashboard-control `api_access_iam_state`. Its schema contains `principals`,
`roles`, `grants`, and `audit_events`. The daemon exposes this state for
inspection, merges managed principals/grants into the unified overview, and
enforces active scoped user/client grants when a request can be bound to a
stable local principal. Today the shipped browser binding is a browser/native
mTLS client-certificate fingerprint. Browser identity-key records are staged
but not consumed by direct `/ws` or dashboard-control offers; `human_user` records can
carry optional account provider, verified-provider, handle, and organization
metadata without making that metadata an authenticator. Requests remain
root-compatible without a stored binding only when they are loopback or present
the verified owner mTLS certificate. Certless remote `--tls` supplies HTTPS and
a secure context, not daemon authority; remote protected HTTP, WebSocket, and
dashboard-control routes require mTLS. `--allow-public-plaintext` never opts a
remote caller into ambient root. Hosted Connect has no root fallback or control
request path: the service returns `403` for browser offer/ICE/close before
mutation, and the daemon drops those events from old/self-hosted services before
inspecting a browser key or grant. There is no hosted-ceiling raise in the
default build. Active grants are evaluated by role on trusted routes, while draft
or revoked records deny instead of silently becoming root again. The
`iam.enforcement` object reports
`root_session_grants: true`, `peer_profile_grants: true`,
`user_client_grants: true`, and
`principal_binding: root_peer_and_local_user_client`. Root sessions can create
or update local user/client grants through the People & Devices pane,
`POST /api/access/iam/user-client-grants`, or dashboard-control
`api_access_iam_upsert_user_client_grant`. Existing grants can be activated,
drafted, revoked, or role-changed with `POST /api/access/iam/grants/update` or
dashboard-control `api_access_iam_update_grant`.

An active grant whose only credential is a browser key is refused when that
key records a hosted or rendezvous-controlled fleet origin. Renaming the
principal `human_user` does not change that composition check. A human record
that also carries a valid browser mTLS certificate remains grantable; its key
and account fields are metadata beside the certificate authenticator. Older
IAM files can still contain active hosted/fleet pure-key grants, but the access
overview projects them as `status: inactive_binding`, `enforced: false`, and
`authority: none`, and the lifecycle API refuses to activate them.

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

The same posture now applies daemon-wide: authority-bearing responses carry no
`Access-Control-Allow-Origin` by default (same-origin only), and every request
for a non-authority-free route bearing a foreign Origin is refused before IAM
or transport-authority resolution. Cross-/same-site Fetch Metadata closes the
top-level-navigation and subresource cases where browsers omit `Origin`.
`/config` is not public:
it requires `presence.read`, echoes only the daemon's own or a fleet-allowlisted
Origin, and returns `Cache-Control: no-store` because ICE configuration can
contain TURN credentials. Only authority-free shell/static bytes, the agent
card, `/connect/bootstrap`, `/connect/status`, and the public signed-document
doorbells remain public; those requests always use an anonymous `role:none`
context even on loopback or when a client certificate is present. Authority-bearing
direct signaling under `/connect/dashboard/*` and the legacy `/ws` event lane
accept browser requests only from the daemon's own origin or the local packaged-app
scheme, before mTLS/local transport facts become a grant. For cleartext
browser traffic, "own origin" additionally requires `localhost` or a literal
loopback address; merely matching attacker-controlled Origin and Host values
is not enough, which closes the DNS-rebinding route into local root authority.
Non-loopback browser access uses HTTPS/mTLS. Native and daemon clients that do
not send a browser Origin remain transport-authenticated as before. The macOS
app is unaffected: its custom-scheme pages are proxied natively, and the
`intendant://` scheme is treated as the daemon's own origin.

Device enrollment has implemented queue/state/UI plumbing, but it is staged in
this alpha: the production queue intentionally has no writer, direct `/ws` and
dashboard-control offers do not present a browser identity-key proof, and
hosted/fleet traffic is refused before enrollment. The GET response reports
`status: staged` and `writer_available: false`; ordinary alpha traffic therefore
cannot create a request or a usable key-login enrollment.
`GET /api/access/enrollment-requests` /
`api_access_enrollment_requests` list the queue (`access.inspect`), and
`POST /api/access/enrollment-requests/decide` / `api_access_enrollment_decide`
(`access.manage`) approve with a role or deny. Approval reuses the normal
user-client grant upsert with the queued key's public key and route origin
attached, so the ordinary active-binding validation and audit apply for
test-seeded/legacy records. People & Devices can render such records, but its
queue is empty in production and remote alpha enrollment uses mTLS certificates.

The same IAM evaluator now protects the direct dashboard HTTP routes that expose
Access, target discovery, settings, filesystem reads/writes, sessions,
worktrees, displays, diagnostics, and managed-context data. Static bootstrap
assets, `/config`, `/.well-known/agent-card.json`, local Connect status, direct
dashboard signaling, and the WebSocket bootstrap stay outside this generic HTTP
IAM gate because they either do not mutate daemon state or have their own
same/app-origin plus transport/authentication binding.

`GET /api/dashboard/targets` and `api_dashboard_targets` remain the compatibility
target model used by older UI paths: target id/host id, display label, access
domain (`user_client` or `peer`), route (`current_dashboard` or `peer_route`),
effective role (`root` or `peer_profile`), connection state, and capability
hints. The browser may refine the local route label to **Intendant Connect**,
**Browser mTLS**, or **Local/debug** because only the browser knows how the
current page was reached, but it should not invent principal/grant/policy
vocabulary.

`GET /api/dashboard/tabs` / `api_dashboard_tabs` (`access.inspect`) is the
**tab-presence** surface: every live dashboard connection on either event lane
(the `/ws` WebSocket or a dashboard-control tunnel session), each entry
carrying its lane, grant provenance (`local` / `client` / `peer`), grant
label, remote host, user agent, connect time, and whether it currently holds
the voice presence or display input authority (with display ids). Browser
tabs group by a client-declared per-tab id: the SPA mints a random id in
`sessionStorage` and sends it as `?tab=` on the `/ws` URL and `tab_id` in
control-tunnel offers. The id is sanitized, display-only, and grants nothing;
server-internal connection ids never appear in the payload (the same rule the
input-authority wire follows). The Access **Overview** renders this as the
**Open dashboards** card — "N tabs connected · this tab · holds voice /
display input" — refreshed on pane entry and every 15 s while the pane is
visible; peer-daemon control connections are counted separately from tabs.
Hosted Connect cannot create an entry: the service refuses browser signaling
and the daemon drops legacy Connect control events before the registry.

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
for trusted dashboard control traffic. It is not authentication: the browser
bootstraps from the daemon's normal dashboard origin, then uses that daemon's
local `/connect/dashboard/*` signaling endpoints. Loopback callers may use the
local-root posture; remote callers must present mTLS. Certless `--tls` provides
only a secure context, and `--allow-public-plaintext` grants no authority. The
existing WebSocket lane remains as a compatibility fallback.

The handshake is bound to the daemon identity:

- the browser creates a `intendant-dashboard-control` DataChannel and sends an
  SDP offer to `/connect/dashboard/offer`;
- the daemon answers with SDP plus a signed binding over the offer hash, answer
  hash, session id, timestamp, and daemon Ed25519 public key;
- the browser verifies that binding with WebCrypto before using the channel.
- the retired Connect-rendezvous prototype additionally compared the directory's
  daemon key; that path is unreachable in the default product and remains only
  in negative mixed-version tests.

The tunnel can be the **primary event lane** — dashboard events and intents flow
through it instead of the legacy `/ws` — for an authenticated daemon-origin
dashboard, a locally built loopback packaged macOS app, and the **capability fallback**.
There is no remote signed-native authentication bridge in this release.
WebKit browsers
(Safari) share that WebSocket limitation — against an mTLS daemon the `/ws`
hangs in CONNECTING forever while fetch/XHR keeps working — so a plain browser
tab whose `/ws` has not opened within a few seconds of an attempt promotes the
tunnel automatically; the event-lane reconnect machinery then owns start,
`api_dashboard_bootstrap` hydration, retry, backoff, and status. The promotion
is derived, never stored: it clears the moment a `/ws` proves able to open
(dual delivery is absorbed by event dedup), and intent sends that find no open
lane surface an actionable error and kick the fallback instead of dropping
silently.

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
All three control-message RPCs are multiplexers, not authorization buckets:
after admitting the outer method, the daemon authorizes every parsed inner
`ControlMsg` against that action's operation and any extra owner requirement.
The outer method can never launder its coarse permission into another action's
authority. In particular, `GrantUserDisplay` and `ResolveDisplayRequest` are
owner/root-only, `RevokeUserDisplay` remains available for de-escalation, and
`StartRecording` plus the pre-encoder diagnostics marker are owner/root-only
for the alpha. Setting up the debug screen and starting its recorder are also
owner/root-only; stopping a debug recording or tearing its screen down remains
available for de-escalation. Recording also requires an active agent-visible
display; private views never produce artifacts. The legacy `/ws` control lane
applies the same inner-message authorization, so switching transports does not
change authority.
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

| Endpoint | Authentication | Purpose |
|----------|----------------|---------|
| `GET /connect/bootstrap` | Public, authority-free bytes | Minimal HTML bootstrap page for WebRTC dashboard-control transport testing; receiving it grants nothing |
| `GET /connect/status` | Public, authority-free bytes | JSON health/capability probe for the bootstrap surface; contains no credential or grant |
| `POST /connect/dashboard/offer` | loopback local root or remote mTLS | Browser SDP offer -> daemon SDP answer plus signed binding |
| `POST /connect/dashboard/ice` | loopback local root or remote mTLS | Browser trickle ICE candidate for a control session |
| `POST /connect/dashboard/close` | loopback local root or remote mTLS | Close a control session |

Those paths are deliberately allowlisted one by one; allowlisting is routing,
not authority. They do **not** make `/`,
`/config`, `/ws`, `/api/*`, recordings, or the full dashboard's protected data
without the normal dashboard authentication. The bootstrap page exposes
`window.intendantConnectDashboard` for tests and diagnostics; it verifies the
same daemon-signed binding as the full dashboard control experiment, then uses
the DataChannel RPC protocol directly. Its small browser-side transport supports
plain JSON requests, chunked JSON responses, bounded `byte_stream_*` downloads,
and `upload_*` frames, so the local bootstrap check can cover both read-style
artifacts and media/editor writes without making the full dashboard certless.
The bootstrap HTML/status bytes may be fetched remotely without a certificate,
but certless **signaling and control** remain loopback-only. A custom `Origin`,
public plaintext opt-in, or self-signed HTTPS connection from a remote address
does not synthesize local root; remote protected paths require mTLS. These endpoints are useful for same-origin dashboard experiments and
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
does not implement account signup, passkeys, daemon linking, or a durable
daemon registry. Its job is to keep the same-origin tunnel protocol easy to
exercise while the hosted Connect service owns the account and route-link UX.
It is not a claim-authority path; hosted acceptance tests assert persistent
`role:none` refusal instead of authorizing this local transport through Connect.

### Retired Local Rendezvous Control Emulator

> **Rejected protocol history; no success path remains.** An earlier emulator
> served the daemon SPA from a public rendezvous origin and brokered a direct
> WebRTC control channel. That experiment established transport feasibility but
> failed the code-provenance boundary: the hosted service could replace the JS
> that exercised authority. Its success-path contract and dashboard inventory
> have been removed from the current documentation so they cannot be mistaken
> for product behavior.

`scripts/validate-connect-rendezvous.cjs` now exists only as an adversarial
mixed-version refusal check. It sends a legacy hosted offer and requires the
daemon to reject it before registry, IAM, enrollment, or DataChannel mutation.
Trusted daemon-origin tests cover successful dashboard-control transport.

### Hosted Connect Production Alpha

The hosted-service slice is implemented as a separate binary,
`intendant-connect`. It serves a public web origin, handles passkey-only account
registration/login, lets a signed-in user link a daemon route with a single-use
12-word claim code, and stores directory/fleet metadata. Browser dashboard
signaling is compiled off: authenticated offer/ICE/close calls return `403`
before service state mutation.

In production, run it behind ordinary public TLS for a public origin such as
`https://connect.intendant.dev`:

```bash
INTENDANT_CONNECT_TOKEN="$(openssl rand -base64 32)" \
  ./target/release/intendant-connect \
    --listen 127.0.0.1:9876 \
    --origin https://connect.intendant.dev \
    --rp-id intendant.dev \
    --data-file <state-file>
```

The retired `--static-root PATH` flag (and
`INTENDANT_CONNECT_STATIC_ROOT`) is accepted and ignored for deployment
compatibility. It cannot mount a filesystem tree or re-enable the daemon SPA,
WASM, or vault kernel on the hosted origin. Connect serves only explicit
compile-time embedded pages/assets.

The `--rp-id intendant.dev` value means passkeys are scoped to the owned
Intendant parent domain while the actual UI can live on `connect.intendant.dev`.
For compatibility, the live production-alpha instance currently keeps its
original `INTENDANT_CONNECT_RP_ID=connect.intendant.dev`; changing that value is
a credential migration and existing users must register new passkeys. Browsers
also allow `http://localhost:<port>` as a secure context for local development,
so the same binary can be E2E-tested without public TLS.

The hosted service also serves `/access` as the account/fleet entry point.
`/connect` is the canonical passkey, route-link, daemon-list, label, release,
and audit surface. It exposes no Open-dashboard action or daemon control URL.
The historical `/app` route always redirects to `/connect`, including crafted
`?connect=1&daemon_id=...` queries.

The daemon side still uses the normal `[connect]` outbound rendezvous client:

```toml
[connect]
enabled = true
rendezvous_url = "https://connect.intendant.dev"
daemon_id = "vortex-deb-x11-intendant"
auth_token = "same daemon token configured on intendant-connect"
```

The hosted MVP flow is:

1. The daemon locally generates a short-lived 12-word BIP39 claim code, then
   registers its `daemon_id`, persistent identity public key, code hash, fresh
   timestamp, and identity signature through `/api/daemon/register`. Connect
   never receives or returns the plaintext code or URL.
2. A successful, single-use registration proof rotates a short-lived
   daemon-session credential. The daemon must present it on `/api/daemon/next`,
   `answer`, `error`, `dry`, and `claim-proof`, including when deployment-wide
   open registration skips the shared bearer.
3. The user opens the daemon-printed `/connect#claim_code=...` URL or enters the
   code, then signs in or registers with a passkey. The browser scrubs the
   fragment, normalizes and hashes the code locally, and submits only the
   digest. Query-string and plaintext claim bodies are rejected. The page
   states that the code is single-use and is not a
   password, recovery phrase, API key, private key, or passkey secret.
4. Connect sends a `claim_challenge` event to the daemon. The daemon signs that
   challenge with its daemon identity key, and Connect verifies the signature
   before recording the account/route link. This changes no daemon IAM state
   and grants no access.
5. Control is established separately through a loopback local console or a
   daemon-served mTLS dashboard. A remote signed-native authentication bridge
   is future work. A Connect account
   assertion never authenticates to the daemon, and daemon-stamped hosted
   provenance is always `role:none` in the default build.
6. Selecting the daemon in Connect shows directory metadata only. The service
   rejects browser offer/ICE/close calls with `403` before queue, rate-limit, or
   active-session mutation; a current daemon also drops those event kinds from
   an old/self-hosted service before registry, IAM, or enrollment mutation. A
   browser-key grant does not override either refusal.

The state file durably stores users, passkeys, daemon account/route links,
hashed claim codes, account-scoped fleet navigation metadata, and a capped
audit log. Plain claim codes are never in the service; WebAuthn challenge
state and rotating daemon-session credentials are memory-only. The service
does not accept browser offers. It exposes a minimal account/fleet UI today: passkey
registration/login, claim-code entry, daemon list, daemon labels, route
metadata, release link, fleet target listing/forget, and audit events.
The visible account identity is the globally unique account name/handle; the
internal WebAuthn display-name field is derived from that handle and is not a
separate user-facing profile field in the MVP UI.

Hosted Connect does not provide daemon control or team IAM. The local daemon
IAM schema already has portable account/provider, verified-provider, handle,
and organization fields, but hosted account metadata remains non-authenticating
directory data.

There is no hosted dashboard or Settings/Debug control panel in the default
service. Connect shows directory/link health. Lower-level transport self-tests
run on trusted loopback/mTLS harnesses; hosted validators stop before dashboard
RPC and expect zero control sessions.

Production-alpha hardening now includes:

- cookie-backed user mutations require same-origin requests and a per-session
  CSRF header;
- auth, claim, and daemon hot paths have simple in-memory
  rate limits keyed by reverse-proxy client headers;
- `/healthz` is a cheap liveness probe and `/readyz` verifies that the state
  directory is usable; the Connect pages/assets are embedded in the binary, so
  readiness does not depend on a static dashboard root;
- security-relevant service events are emitted as structured JSON on stderr in
  addition to the persisted user audit log;
- releasing a daemon link removes it from the account and clears legacy
  service-side active-session bookkeeping. Current daemons ignore any legacy
  hosted close event.

For mixed-version alpha upgrades, restarting only Connect does not terminate a
legacy P2P DataChannel that was already established. Upgrade and restart each
daemon, close old Connect tabs, and let IAM schema v2 revoke legacy
`connect-bootstrap` grants. New mixed-version attempts are blocked twice: at
the current service and at the current daemon.

The reverse proxy in front of `intendant-connect` must terminate public TLS for
`connect.intendant.dev`, forward `Host`, set `X-Forwarded-For`/`X-Real-IP`, and
strip any inbound copies of those client-IP headers before setting them. Keep
the service bound to `127.0.0.1`, keep `INTENDANT_CONNECT_TOKEN` in a secret
store, and back up the configured state file; that file is the current
account/passkey/route-link database, not a daemon authority store.

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

- one linked account per daemon; the link is route metadata, not ownership;
  no teams, recovery, or account email flow;
- an optional shared deployment bearer gates registration (and always guards
  admin APIs); fresh daemon-key proofs and rotating short-lived daemon-session
  credentials protect the per-daemon mailbox endpoints even in open mode;
- rate limits, web sessions, and daemon-session credentials are single-process
  in-memory state; browser offers are rejected before pending/active session
  mutation, and plaintext route codes never enter the service;
- no high-availability storage or database migrations; the state file is a
  single-node alpha persistence layer;
- no hosted daemon-control path or application-layer dashboard RPC relay in the
  default build; successful WebRTC/dashboard transport is exercised only from
  trusted local/direct clients;
- Connect exposes no Files or Terminal target. File-transfer history, resumed
  offsets, and staged uploads belong to authenticated daemon dashboards, not
  the hosted directory;
- peer daemon-to-daemon mTLS remains separate from Connect account login.

Run the hosted MVP E2E locally with:

```bash
cargo build --bin intendant-connect --bin intendant
node scripts/validate-connect-hosted-mvp.cjs
```

That validator starts `intendant-connect`, launches a daemon with outbound
Connect enabled, uses a browser virtual authenticator for passkey registration,
links the daemon route, labels it, verifies that authenticated browser
offer/ICE/close calls return `403` without enqueueing, creates a local operator
grant as an adversarial regression fixture, and verifies the service still
creates zero control sessions. A daemon regression feeds the same events as if
from an old/self-hosted service and checks that control, IAM, and enrollment
state stay unchanged. The validator then releases the route and verifies the
audit events. Successful
Shell, Files, media, display, and byte-stream transport remain covered by the
trusted local/direct dashboard-control validators.

### Rejected Hosted-Dashboard Design

> **Historical decision only; there is no hosted dashboard contract here.**
> The public-origin WebRTC prototype was rejected because encrypted transport
> did not make replaceable hosted JavaScript a trusted authority client.
> Its detailed success-path, session-grant, relay, RPC, media, and rollout
> specifications were removed: they were not a roadmap and no longer describe
> callable code.

The surviving engineering result is narrower: daemon-signed WebRTC bindings and
the dashboard transport abstraction remain useful on authenticated daemon-origin
paths and local packaged-app development builds. No signed/notarized packaged
release exists for this alpha. Hosted Connect is an account, route, presence, push, and
metadata directory. It serves no daemon SPA or privileged dashboard assets;
`/app` and `/app.html` redirect to `/connect`; browser
offer/ICE/close calls are refused before mutation; and current daemons drop
legacy hosted-control events. No browser-key grant or persisted ceiling edit can
reactivate the design.

Any future hosted-control product would require a separate binary/product and a
new trust design. The current boundaries and migration are specified in
[Trust Architecture](./trust-architecture.md).

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
| `POST /session` | Mint ephemeral session tokens for Gemini Live / OpenAI Realtime (own/app origin; `CredentialsManage` IAM permission) |
| `GET /wasm-web/*`, `GET /wasm-station/*` | Compiled WASM + JS glue (content-hash cache-busted) |
| `GET /audio-processor.js` | AudioWorklet processor for mic capture |
| `GET /vault-kernel.js` | The vault crypto kernel worker; the SPA hash-verifies it against its assembled-in pin before instantiating (see [credential custody](./credential-custody.md#the-crypto-kernel)) |
| `GET /.well-known/agent-card.json` | Agent card (identity + capabilities) for peers and integrations |
| `POST /mcp` | Streamable-HTTP MCP server (per-tool IAM; see [MCP server](./mcp-server.md)) |
| `WS /` or `WS /ws` | Main WebSocket: events, fallback Shell terminal I/O, presence protocol, WebRTC signaling; browser upgrades require the daemon's independently trusted origin or the local app scheme before transport authority is resolved; fleet SNI is refused |
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
| `GET /api/external-agents` | External-agent backend availability (configured command, installed, auth posture, last used) plus passive zero-quota compatibility status (artifact fingerprint, in-band version, manifest digest, finding counts) — drives the fueling nudge and new-session picker |
| `GET /api/displays`, `POST /api/diagnostics/visual-freshness` | Display inventory; visual-freshness probe marker |
| `GET /api/access/{overview,iam/state}`, `GET /api/dashboard/targets` | Trust-architecture snapshots (IAM state, fleet targets) |
| `POST /api/access/...` | Trust mutations: enrollment decide, IAM grant upsert/update, org trust/revoke, org-grant issue/renew/revoke-member, issuer init/delegate/install, revocation-list apply |
| `GET /api/peers[/*]`, `POST /api/peers[/*]`, `DELETE /api/peers` | Peer federation: registry reads (GET), pairing + management/signaling (POST), registry removal (DELETE) |
| `POST /api/coordinator/route` | Multi-agent coordinator task routing (peer lane) |
| `GET /api/worktrees`, `POST /api/worktrees/{inspect,scan,remove,clean,merge}` | Agent worktree inventory and lifecycle (clean = reclaim a checkout's Cargo target/; merge = session-linked worktree finish card) |
| `GET /connect/{bootstrap,status}`, `POST /connect/dashboard/{offer,ice,close}` | Daemon-origin WebRTC control bootstrap: certless only on loopback, remote callers require direct mTLS; fleet SNI and hosted Connect browser APIs cannot open it |

### Declared API routes

Every `/api/*`, `/session`, and `/mcp` route is declared exactly once in
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

`fleet allowlist` in the generated CORS column describes which already-trusted
dashboard origins may call a daemon's **independently verified direct-mTLS
URL**. It does not authorize the rendezvous-controlled fleet WebPKI URL. The
listener classifies fleet SNI first and rejects all non-public HTTP/MCP,
signaling, and WebSocket traffic before route CORS, browser mTLS, or IAM is
resolved. The fleet certificate endpoint itself can therefore be requested
only from a trusted direct surface; the resulting name is discovery/public
shell only. `fleet or loopback` (the session-list rows the multi-daemon Stats
tab reads) is the same allowlist echo plus loopback-host page origins on
connections that themselves arrive over loopback — a sibling daemon's
dashboard on another port of the same machine; these rows historically
answered with a wildcard `Access-Control-Allow-Origin`, which is retired —
an admitted origin is echoed exactly (with `Vary: Origin`) and every other
response omits the header.

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
| GET | `/api/transfers` | FilesystemRead | own origin | none | List transfer jobs, newest first (`?id=` filters by job id or resume token) |
| POST | `/api/transfers` | FilesystemWrite | own origin | bounded | Create a transfer job (kind download|upload, path/destination, name, total_size, sha256, conflict; fs-scope-checked on the target path) |
| POST | `/api/transfers/{id}/chunk` | FilesystemWrite | own origin | streaming | Append one raw-body chunk to an upload job (`?offset=`; ≤ 32 MiB per chunk) |
| POST | `/api/transfers/{id}/commit` | FilesystemWrite | own origin | bounded | Verify (size + declared sha256) and atomically rename a finished upload into place |
| DELETE | `/api/transfers/{id}` | FilesystemWrite | own origin | none | Delete a transfer job (cancels partials; removes managed artifacts) |
| POST | `/api/transfers/{id}/delete` | FilesystemWrite | own origin | none | Delete a transfer job (WKWebView POST fallback) |
| GET | `/api/transfers/{id}/download` | FilesystemRead | own origin | none | Read download-job bytes (`?offset=&length=` or `Range` → 206; resume metadata echoed as X-Transfer-* headers, X-Content-Sha256 on full reads) |
| POST | `/session` | CredentialsManage | own origin | none | Mint an ephemeral Gemini Live / OpenAI Realtime token from a daemon-held provider credential |
| GET | `/api/session/current/changes[/…]` | SessionManage | own origin | none | List the session's changed files, or the unified diff for one file (subpath) |
| GET | `/api/session/current/history` | SessionManage | own origin | none | Serialized rollback History for the current session |
| POST | `/api/session/current/rollback` | SessionManage | own origin | bounded | Roll the current session back to a round (optionally reverting files) |
| GET | `/api/agenda` | AgendaRead | own origin | none | Agenda ledger snapshot: items (oldest first) plus status counts |
| POST | `/api/agenda/op` | AgendaWrite | own origin | bounded | Apply one agenda command (add, answer, patch, transitions, or scheduled-session propose/approve/revoke) |
| POST | `/api/agenda/reminders/policy` | Settings | own origin | bounded | Merge-patch the agenda reminder policy (quiet hours, urgency, per-item overrides) |
| GET | `/api/memory/search` | MemoryRead | own origin | none | Bounded Memory claim search (q, limit, candidates); results carry derived status |
| GET | `/api/memory/claim` | MemoryRead | own origin | none | Read one Memory claim by id prefix (id); status derived at read time |
| POST | `/api/memory/propose` | MemoryWrite | own origin | bounded | Propose one Memory claim (candidate lane; ephemeral plane in P1.1) |
| POST | `/api/session/current/redo` | SessionManage | own origin | bounded | Redo the last rolled-back round |
| POST | `/api/session/current/prune` | SessionManage | own origin | bounded | Prune rollback state for the current session |
| POST | `/api/session/current/agent-output` | SessionManage | own origin | bounded | Fetch the current session's persisted agent output by id (POST-shaped read) |
| POST | `/api/session/current/uploads` | SessionManage | own origin | streaming | Upload a file attachment (raw streamed body; name/destination in query) |
| GET | `/api/session/current/uploads` | SessionManage | own origin | none | List uploads for the current session |
| GET | `/api/session/current/uploads/{id}/raw` | SessionManage | own origin | none | Fetch one upload's raw bytes (attachment; MIME sniffing disabled) |
| GET | `/api/session/current/uploads[/…]` | SessionManage | own origin | none | Unknown upload subpaths (handler-owned JSON 404) |
| DELETE | `/api/session/current/uploads/{upload_id}` | SessionManage | own origin | none | Delete one upload (file + sidecar) |
| DELETE | `/api/session/{id}` | SessionManage | own origin | none | Delete a session's data |
| DELETE | `/api/session/{id}/{target}` | SessionManage | own origin | none | Delete one data kind for a session (recordings, frames, …) |
| DELETE | `/api/session/{id}/{target}/delete` | SessionManage | own origin | none | Delete one data kind for a session (suffix form) |
| POST | `/api/session/{id}/delete` | SessionManage | own origin | none | Delete a session's data (POST fallback for WKWebView) |
| POST | `/api/session/{id}/{target}/delete` | SessionManage | own origin | none | Delete one data kind for a session (POST fallback) |
| POST | `/api/session/{id}/agent-output` | SessionInspect | own origin | bounded | Fetch a session's persisted agent output by id (POST-shaped read) |
| GET | `/api/session/{id}/fork-points` | SessionInspect | own origin | none | Unified fork-point catalog for a session (anchors + eligibility, backend-tagged) |
| GET | `/api/session/{id}/background-tasks` | SessionInspect | own origin | none | Background tasks a supervised session announced (id, description, status, output availability) |
| GET | `/api/session/{id}/background-tasks/{task}/output` | SessionInspect | own origin | none | Tail of one background task's output file (tail_kb query, capped; registry-resolved path) |
| GET | `/api/session/current[/…]` | SessionManage | own origin | none | Current-session detail and artifact sub-routes |
| POST | `/api/session/current[/…]` | SessionManage | own origin | none | Current-session detail sub-routes (POST fallback callers) |
| GET | `/api/session/{id}/context-snapshot` | SessionInspect | own origin | none | Replay one archived context snapshot (file/request_id/request_index/ts selector) |
| GET | `/api/session/{id}/report` | SessionInspect | own origin | none | Session report zip (text artifacts; id=current targets the live session) |
| GET | `/api/session/{id}/recordings` | SessionInspect | own origin | none | List a session's recording streams |
| GET | `/api/session/{id}/recordings/{stream}/{asset}` | SessionInspect | own origin | none | Recording assets: segments listing, playlist.m3u8, or a segment file |
| GET | `/api/session/{id}/frames/{filename}` | SessionInspect | own origin | none | Session frame image asset |
| GET | `/api/session/{id}` | SessionInspect | own origin | none | Session detail (paged replay entries; limit/before/source) |
| GET | `/api/session[/…]` | SessionInspect | own origin | none | Session artifact sub-routes: recordings (+segments/playlist), report zip, frames |
| POST | `/api/session[/…]` | SessionManage | own origin | none | Session detail sub-routes (POST fallback callers) |
| GET | `/api/managed-context/anchors` | SessionInspect | own origin | none | Managed-context anchor catalog |
| GET | `/api/managed-context/records` | SessionInspect | own origin | none | Managed-context record index |
| GET | `/api/managed-context/fission` | SessionInspect | own origin | none | Managed-context fission state |
| POST | `/api/worktrees/inspect` | SessionInspect | own origin | bounded | Inspect one worktree (branch, ahead/behind, dirty state) |
| POST | `/api/worktrees/remove` | SessionManage | own origin | bounded | Remove a worktree from the inventory |
| POST | `/api/worktrees/clean` | SessionManage | own origin | bounded | Delete a worktree's Cargo target/ dir (CACHEDIR.TAG-gated) to reclaim disk, keeping the checkout |
| POST | `/api/worktrees/merge` | SessionManage | own origin | bounded | Merge a session's linked worktree branch into its base checkout, then remove the checkout |
| POST | `/api/worktrees/scan` | SessionManage | own origin | none | Rescan the worktree inventory (refreshes the cache) |
| GET | `/api/worktrees` | SessionInspect | own origin | none | Cached worktree inventory |
| GET | `/api/sessions/stream` | SessionInspect | fleet or loopback | none | NDJSON stream of the session list |
| GET | `/api/sessions/search` | SessionInspect | own origin | none | Search sessions (q, source, mode, project filters) |
| GET | `/api/sessions/message-search` | SessionInspect | own origin | none | Message-lane search over the shard index (q, source, superseded, subagents, cursor) |
| GET | `/api/sessions` | SessionInspect | fleet or loopback | none | List sessions (id filter, limit, usage view; fleet/loopback CORS echo for the multi-daemon Stats tab) |
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
| POST | `/api/access/enrollment-requests/decide` | AccessManage | fleet allowlist | bounded | Staged decision API; the default product has no queue writer |
| GET | `/api/access/enrollment-requests` | AccessInspect | fleet allowlist | none | Staged enrollment capability and normally empty queue |
| GET | `/api/access/iam/state` | AccessInspect | fleet allowlist | none | Local IAM state (roles, grants, bindings) |
| GET | `/api/access/overview` | AccessInspect | fleet allowlist | none | Access overview for the calling principal |
| GET | `/api/access/connect/status` | AccessInspect | fleet allowlist | none | Connect rendezvous status (discovery-link state and provenance; no claim code) |
| GET | `/api/access/connect/claim-code` | AccessManage | fleet allowlist | none | Reveal the current one-time twelve-word claim code (unlinked daemons only) |
| POST | `/api/access/connect/config` | AccessManage | fleet allowlist | bounded | Enable/disable the Connect client (persists to intendant.toml, applies live) |
| POST | `/api/access/connect/unclaim` | AccessManage | fleet allowlist | bounded | Unlink this daemon's discovery record from its Connect account (daemon-signed) |
| POST | `/api/access/tier` | AccessManage | fleet allowlist | bounded | Set this daemon's trust tier label (integrated/disposable; null clears) |
| POST | `/api/access/fleet-cert/request` | AccessManage | fleet allowlist | bounded | Request a fleet certificate (publish addresses, run the ACME DNS-01 order; async start) |
| GET | `/api/dashboard/targets` | AccessInspect | own origin | none | Dashboard target list (this daemon + connected peers) |
| GET | `/api/dashboard/tabs` | AccessInspect | own origin | none | Live dashboard connections (open tabs) with voice/input-authority holders |
| POST | `/api/peers/pairing/invite` | federation (per method/path) | own origin | bounded | Issue a peer-scoped mTLS pairing invite |
| POST | `/api/peers/pairing/request-access` | federation (per method/path) | own origin | bounded | Start an outgoing access request against a remote daemon's doorbell |
| POST | `/api/peers/pairing/request-access/poll` | federation (per method/path) | own origin | bounded | Poll an outgoing access request (installs the approved identity) |
| GET | `/api/peers/pairing/requests` | federation (per method/path) | own origin | bounded | List pending/decided peer access requests |
| GET | `/api/peers/pairing/identities` | federation (per method/path) | own origin | bounded | List approved/revoked peer identities |
| POST | `/api/peers/pairing/identities/revoke` | federation (per method/path) | own origin | bounded | Revoke a peer identity |
| POST | `/api/peers/pairing/requests/{code}/{decision}` | federation (per method/path) | own origin | bounded | Decide a pending access request (approve or deny) |
| POST | `/api/peers/pairing/join` | federation (per method/path) | own origin | bounded | Import a pairing invite and register the peer |
| GET | `/api/peers` | federation (per method/path) | own origin | bounded | List registered peers (snapshots) |
| POST | `/api/peers` | federation (per method/path) | own origin | bounded | Add a peer by card URL (optionally persisted) |
| DELETE | `/api/peers` | federation (per method/path) | own origin | bounded | Remove a registered peer |
| GET | `/api/peers/eligible` | federation (per method/path) | own origin | bounded | List connected peers satisfying every ?capability= filter |
| POST | `/api/peers/{peer_id}/message` | federation (per method/path) | own origin | bounded | Send a message to a connected peer |
| POST | `/api/peers/{peer_id}/task` | federation (per method/path) | own origin | bounded | Delegate a task to a connected peer |
| POST | `/api/peers/{peer_id}/approval` | federation (per method/path) | own origin | bounded | Resolve a peer-forwarded approval request |
| POST | `/api/peers/{peer_id}/webrtc` | federation (per method/path) | own origin | bounded | Relay display WebRTC signaling to a connected peer |
| POST | `/api/peers/{peer_id}/file-transfer-webrtc` | federation (per method/path) | own origin | bounded | Relay file-transfer WebRTC signaling to a connected peer |
| POST | `/api/peers/{peer_id}/dashboard-control-webrtc` | federation (per method/path) | own origin | bounded | Relay dashboard-control WebRTC signaling to a connected peer |
| any | `/api/peers[/…]` | federation (per method/path) | own origin | bounded | Peers sub-router catch-all (handler-owned JSON 404/405 for unknown subpaths and undeclared methods) |
| POST | `/api/coordinator/route` | federation (per method/path) | own origin | bounded | Capability-based task routing through the Coordinator |
| POST | `/mcp` | MCP token | own origin | ≤ 16 MiB | MCP Streamable HTTP endpoint (JSON-RPC requests + notifications) |
| GET | `/mcp` | MCP token | own origin | none | MCP SSE stream (405: stateless server) |
| DELETE | `/mcp` | MCP token | own origin | none | MCP session delete (405: stateless server) |
<!-- gateway-route-table:end -->

The four signed-organization rows marked `public` are courier/verification
doors, not daemon-authentication or control doors. The HTTP caller receives no
principal, role, or session. A locally trusted org signature authorizes only
processing of the bounded document for its named cryptographic subject (or
application of signed revocation facts); that subject must authenticate later
through a real ingress. Peer subjects use peer mTLS. Human browser-key subjects
remain record-only in this alpha: peer offers can verify them for attribution,
but no live ingress admits them as its controlling IAM principal.

The full WebSocket message protocol (inbound key/resize/presence/WebRTC frames,
outbound term/state/log-replay/tool-response frames) and the gateway's internal
layering are documented in [Integrations → Web Gateway](./integrations.md#web-gateway).
Dashboard session-control actions use the `api_session_control_msg`
dashboard-control RPC; there is no HTTP `control-msg` route.
