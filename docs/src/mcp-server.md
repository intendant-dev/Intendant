# MCP Server

The `--mcp` flag runs Intendant as a [Model Context Protocol](https://modelcontextprotocol.io/)
server over stdio JSON-RPC (`src/bin/caller/mcp/`). It lets an external agent
(Claude Code, Codex, etc.) observe and control Intendant: every action a human
can take in the dashboard is exposed as an MCP tool, plus display/CU/frame
tools, live audio, and a controller-orchestration surface.

Architecturally the MCP server is a **frontend peer of the dashboard**: it
subscribes to the same `EventBus`, and user intents are
[`ControlMsg`](./integrations.md) values everywhere — the web dashboard and the
Unix control socket dispatch them to the centralized `control_plane.rs` (see
[Autonomy & Approvals](./autonomy.md) for why frontends are display-only), and
the MCP server's approval/input tools apply the same state helpers as its own
`ControlMsg` arms (`resolve_pending_approval` & co. in `mcp/mod.rs`; the former
MCP-only `UserAction` enum is retired). `--mcp` is its own run mode and is
**not** implied by `--web`.

## Running

```bash
# MCP server on stdio
./target/release/intendant --mcp "Deploy the application"

# With provider/model overrides
./target/release/intendant --mcp --provider anthropic --model claude-sonnet-4-6-20250929 "Fix the tests"

# With an autonomy preset
./target/release/intendant --mcp --autonomy high "Refactor the auth module"
```

In MCP mode, stdin/stdout are reserved for JSON-RPC, so the initial task is taken
from the command line (or the server starts idle and accepts `start_task`).

### Client Configuration

Add Intendant to your MCP client config (Claude Code
`~/.claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "intendant": {
      "command": "intendant",
      "args": ["--mcp", "Your task description here"]
    }
  }
}
```

## Tools

The full MCP tool surface (dispatched in `call_tool_by_name`) is broad. For
model clients that front-load tool schemas into every request, prefer the
HTTP transport's `tool_profile=core` query parameter and the `intendant ctl`
CLI for lazy discovery. `tool_profile=core` advertises the bootstrap set:
status, shared-view collaboration, and the minimal real-display/CU tools
(`list_displays`, `grant_user_display`, `request_user_display`,
`revoke_user_display`, `read_screen`,
`take_screenshot`, `execute_cu_actions`) — managed and vanilla alike; managed
context additionally advertises the managed-context rewind/backout and fission
tools. Omitting `tool_profile` keeps the historical full tool list. Profile
filtering applies to `tools/list` only — hidden tools remain callable (that
is the lazy `ctl tools call` path). *Authorization* is separate: see the next
section.

### /mcp authorization

Every `POST /mcp` request binds to a principal in the same local IAM system
that gates the dashboard and federation surfaces
([Trust Architecture](./trust-architecture.md)), and **every `tools/call` is
evaluated at call time** against that principal's permissions via a per-tool
operation map (`mcp_tool_operation` in `mcp/tool_gate.rs`; e.g. `execute_cu_actions`
and `grant_user_display` require `display.input`, `start_task` requires
`task.run`, unclassified tools require `runtime.control`). `tools/list` is
filtered to what the principal may actually call. The display tools carry a
second, separate gate: a `user_session` target needs the standing
user-display grant unless the bound principal is an owner surface (the
trusted dashboard / enrolled root user client or bare local loopback —
`AccessPrincipal::is_owner_surface`); the stdio transport, being wired up by
the owner's own client config, always counts as an owner surface. See
[Computer Use](./computer-use-and-audio.md#display-targets). Binding order:

1. **Peer daemons** (mTLS peer identity) use their peer-profile principal.
2. **Supervised backends** present the token in their injected
   `INTENDANT_MCP_URL`. It is *session-scoped* — derived from the daemon's
   per-process token and the `session_id` — so it authenticates exactly that
   agent session (`principal:agent-session:<id>`). Possession of the raw
   per-process token remains root-equivalent. Explicit-but-wrong tokens are
   refused with 401. A session whose binding is known but whose grant has
   *lapsed* (expired or revoked) binds the scoped principal and is denied
   with the real reason — it does not fall back to default trust; only
   sessions with no binding at all do.
3. **Browser pages** may only call `/mcp` from this daemon's own origin (or
   the macOS app scheme) and then bind like any dashboard HTTP request
   (mTLS certificate principal or trusted-transport root). Foreign origins
   get 403 — same posture as the rest of `/api/*`.
4. **Tokenless loopback** processes bind to
   `principal:local-process:loopback`. Tokenless non-loopback requests are
   refused. Once any `agent_session` binding exists — even one whose grant
   has since expired or been revoked — this path **fails closed** (401)
   until an explicit `local_process` grant states what bare loopback
   callers may do; otherwise a scoped agent could shed its injected token
   and re-enter as the root-compatible local default, making its grant
   decorative. A lapsed `local_process` grant likewise denies rather than
   restoring the open default.

The rule across all of these: **once a principal is named, its authority
comes only from grants, and a lapsed grant means "no" — never "back to
defaults".** Security posture only relaxes when a person explicitly
relaxes it: re-grant `role:root` (to the `"*"` agent principal or to
`local_process`) to restore the implicit-trust behavior visibly and
auditable, rather than by timer or revocation side effect.

By default the supervised-agent, token-holder, and local-loopback principals
are root-compatible, so bare `intendant ctl` on the daemon host and existing
supervised backends keep working with zero ceremony. The point of the
binding is that the owner can now *scope* them: an
`agent_session` grant (exact `session_id`, or `"*"` for every supervised
agent) or a `local_process` grant against
`POST /api/access/iam/user-client-grants` pins that principal to a role, and
call-time enforcement + `tools/list` follow it. Example — cap every
supervised agent at operator (no runtime control, no settings/access
administration):

```bash
curl -X POST http://localhost:8765/api/access/iam/user-client-grants \
  -H 'Content-Type: application/json' \
  -d '{"kind": "agent_session", "session_id": "*", "role_id": "role:operator"}'
```

Scoping any agent session flips the tokenless loopback default to
fail-closed, so pair it with an explicit statement of what your own bare
`intendant ctl` gets (root keeps it exactly as before, now as a visible,
revocable grant):

```bash
curl -X POST http://localhost:8765/api/access/iam/user-client-grants \
  -H 'Content-Type: application/json' \
  -d '{"kind": "local_process", "role_id": "role:root"}'
```

The shared per-process token still exists as the transport-layer fallback
(and is what the strict-TLS loopback-cleartext exception checks), but
possession of it is no longer the *authorization* story — grants and the
evaluator are.

CORS on `/mcp` matches the gate: responses echo `Access-Control-Allow-Origin`
only for the daemon's own origin or the app-bundle scheme (which genuinely
needs it); foreign origins and non-browser clients get no CORS grant at all.
With the patched managed Codex binary, `rewind_backout mode="fork"` creates a
new Codex thread while inheriting the lineage prompt-cache key from the saved
rollout; same-thread `restore` remains available when the current thread should
be rewritten in place.

The CLI mirrors the broad surface without loading every schema into model
context:

```bash
"${INTENDANT:-intendant}" ctl --help
"${INTENDANT:-intendant}" ctl tools list
"${INTENDANT:-intendant}" ctl tools schema take_screenshot
"${INTENDANT:-intendant}" ctl tools call grant_user_display --args '{}'
"${INTENDANT:-intendant}" ctl display grant-user
"${INTENDANT:-intendant}" ctl display screenshot --target user_session --output screen.png
```

Full MCP tool groups:

### Status & logs (observation)

| Tool                   | Description | Params |
|------------------------|-------------|--------|
| `get_status`           | Provider, model, turn, budget %, phase, autonomy, verbosity, tokens. | — |
| `get_logs`             | Log entries, cursor-paginated, level-filterable. | `since_id?`, `level_filter?`, `limit?` |
| `get_pending_approval` | The current pending approval request (or null). | — |
| `get_pending_input`    | The current pending `askHuman` question (or null). | — |

### Interactive actions

| Tool            | Description | Params |
|-----------------|-------------|--------|
| `approve`       | Approve a pending command. | `id` |
| `deny`          | Deny and stop. | `id` |
| `skip`          | Skip, continue with the next command. | `id` |
| `approve_all`   | Approve and set autonomy to Full. | `id` |
| `respond`       | Answer an `askHuman` question. | `text` |
| `post_session_note` | Post a **display-only note** into the session transcript — rendered live in the dashboard and persisted for replay, never added to any model's context. Optional base64 images are committed to the session upload store and rendered as clickable thumbnails. Caps: 16 KB text, 6 images, 4 MB per image, 8 MB total; raster types only (`image/png`, `image/jpeg`, `image/gif`, `image/webp`, `image/bmp`). Session-scoped callers post into their own session by default. | `text`, `images?` (`[{media_type, data, name?}]`), `session_id?`, `source?` |
| `set_autonomy`  | Set autonomy. | `level`: `low`/`medium`/`high`/`full` |
| `set_verbosity` | Set log verbosity. | `level`: `quiet`/`normal`/`verbose`/`debug` |
| `start_task`    | Start a new agent task (also used as follow-up when waiting). | `task` |
| `quit`          | Shut down the agent. | — |

### Display, computer use & frames

| Tool                 | Description | Params |
|----------------------|-------------|--------|
| `list_displays`      | Enumerate displays with their session state. | — |
| `take_display`       | Take control of a display. | `display_id` |
| `release_display`    | Release control of a display. | `display_id`, `note?` |
| `grant_user_display` | Grant access to the user's real display session (owner surfaces only — this call *is* the opt-in); on Wayland, enable **Allow Remote Interaction** in the GNOME portal before clicking **Share** so CU input works. | `display_id?` |
| `request_user_display` | Ask the user for their display: raises the dashboard doorbell popup with your reason and blocks for their click — the only thing that can grant it (never auto-approved; see [Autonomy — the display request rail](./autonomy.md#the-display-request-rail-doorbell)). `access="view"` shares the stream without CU input; `"view_and_control"` requests the full grant. | `reason`, `access?`, `wait_seconds?`, `session_id?` |
| `revoke_user_display` | Revoke access to the user's real display session. | `display_id?`, `note?` |
| `take_screenshot`    | Capture a screenshot (returns image content). | display params |
| `read_screen`        | Frontmost app's accessibility element tree — cheap textual grounding (macOS user session). | `display_target?`, `format?` |
| `execute_cu_actions` | Run a batch of [computer-use](./computer-use-and-audio.md) actions. | CU action params |
| `list_frames`        | List captured video frames. | filter params |
| `read_frame`         | Read a specific frame. | `frame_id` |

### Browser workspaces

Browser workspaces are addressable browser-control surfaces for agent/human
collaboration and headed UI testing. The first executable backend launches a
managed local Chromium-family browser with an isolated profile and Chrome
DevTools Protocol metadata. On macOS, Intendant does not launch the user's
installed `/Applications/Google Chrome.app` by default; use `provider=system_cdp`
or `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1` to opt into system
Chrome/Chromium, and use `INTENDANT_BROWSER_WORKSPACE_EXECUTABLE` for an
explicit browser binary. Run `intendant setup browsers` to install Chrome for
Testing into Intendant's managed cache. The wire contract already carries
`provider` and `peer_id` fields so Playwright/Agent Browser adapters and
federated peer-hosted browsers can slot in later. Each workspace has a lease,
so concurrent agents must explicitly acquire it and use `force` to take over an
active holder.

| Tool                          | Description | Params |
|-------------------------------|-------------|--------|
| `browser_workspace_providers` | Report available workspace providers. | — |
| `list_browser_workspaces`     | List active browser workspaces and leases. | — |
| `create_browser_workspace`    | Launch/register a workspace. | `url?`, `label?`, `provider?`, `peer_id?`, `owner_session_id?`, `profile_dir?` |
| `acquire_browser_workspace`   | Acquire a workspace lease. | `workspace_id`, `holder_id`, `holder_kind?`, `note?`, `force?` |
| `release_browser_workspace`   | Release a workspace lease. | `workspace_id`, `holder_id?`, `note?` |
| `close_browser_workspace`     | Close a workspace and terminate its local browser process when owned here. | `workspace_id`, `reason?` |

### Live audio

| Tool               | Description | Params |
|--------------------|-------------|--------|
| `spawn_live_audio` | Spawn an untrusted [live-audio](./computer-use-and-audio.md#live-audio) voice session. | `id`, `provider`, `playbook`, `response_schema`, … |

### Peer federation

The agent-facing surface for [peer federation](./peer-federation.md):
inspect the peer roster, delegate work to sibling daemons, and do direct
computer use on peer displays. `list_peers` is gated as `peer.inspect` (same
classification as `GET /api/peers`); every other tool here is gated as
`peer.use` — acting through a peer delegates this daemon's peer identity, and
the receiving peer authorizes the request against its own grants for this
daemon. A delegated task runs on the peer's machine under the peer's own
autonomy/approval policy. The direct-CU trio is one stateless `tools/call`
POST to the *peer's* `/mcp` over the transport's mTLS identity; the peer's
gate then requires display view for `peer_list_displays` /
`peer_take_screenshot` (profile `read-only-display` or better) and display
input for `peer_execute_cu_actions` (`peer-operator` / `peer-root`).

| Tool                 | Description | Params |
|----------------------|-------------|--------|
| `list_peers`         | Peer snapshot list — id, label, connection state, capabilities, sessions, displays (same payload as `GET /api/peers`). | — |
| `peer_send_message`  | Send a message to a peer's agent. | `peer_id`, `message`, `session?` |
| `peer_delegate_task` | Delegate a task executed by the peer's own agent; returns `task_id`. | `peer_id`, `instructions`, `context?` |
| `peer_list_displays` | List a peer's displays (ids, names, resolutions) over its `/mcp`. | `peer_id` |
| `peer_take_screenshot` | Screenshot a peer display; returns an MCP image content block. | `peer_id`, `display_target?` |
| `peer_execute_cu_actions` | Run CU actions on a peer display; returns per-action status + annotated post-action screenshot. | `peer_id`, `actions`, `display_target?`, `coordinate_space?` |

### Controller Orchestration

| Tool                            | Description | Params |
|---------------------------------|-------------|--------|
| `schedule_controller_restart`   | Schedule a controller restart / autonomous re-init workflow. | `controller_id`, `north_star_goal`, `reason?`, `restart_after?`, `restart_command?`, `auto_start_task?`, `max_attempts?`, `cooldown_sec?` |
| `controller_turn_complete`      | Final handshake; validates token and executes the scheduled restart. | `restart_id`, `turn_complete_token`, `status?`, `handoff_summary?` |
| `get_restart_status`            | Current restart state (or null). | — |
| `cancel_controller_restart`     | Cancel a scheduled restart. | `restart_id?` |
| `request_controller_loop_halt`  | Request loop halt. | `persistent?` |
| `clear_controller_loop_halt`    | Clear loop-halt flags so restarts can resume. | — |
| `intervene_controller_loop`     | Intervene in the active loop process and visible Codex app-server descendants. | `mode`: `stop`/`abort` |
| `get_controller_loop_status`    | Unified loop-health snapshot. | — |

`schedule_controller_restart`, `controller_turn_complete`, and
`cancel_controller_restart` return JSON payloads with an `ok` boolean and status
fields; rejections come back as JSON (`ok: false`) with an `error` message rather
than plain text.

## Resources

Resources provide push-based observation via subscriptions. The server emits
`notifications/resources/updated` when state changes so clients re-fetch.

| URI                              | Description |
|----------------------------------|-------------|
| `intendant://status`             | Provider, model, turn, budget %, phase, autonomy, session ID, task. |
| `intendant://usage`              | Per-model token usage (main + optional presence). |
| `intendant://logs`               | Last 100 chronological log entries (same as the dashboard's activity log). |
| `intendant://pending-approval`   | The current pending approval, if any. |
| `intendant://pending-input`      | The current pending `askHuman` question, if any. |
| `intendant://controller-restart` | Current controller-restart workflow state, if any. |
| `intendant://controller-loop`    | Loop-health snapshot (intervention flags, singleton lock owner, active wrapper/codex PIDs, latest run pointers). |

## Controller Restart Workflow

Use this when you want Intendant to trigger a controller re-init cycle safely
(e.g. an external Codex/Claude controller relaunching itself).

1. Call `schedule_controller_restart`; capture `restart_id` + `turn_complete_token`.
2. Before ending the controlling agent's turn, call `controller_turn_complete`
   with both values.
3. Intendant executes the restart actions:
   - spawn `restart_command` (if provided), and/or
   - start a fresh Intendant task from `north_star_goal`
     (`auto_start_task=false` by default; opt in only for E2E testing).
4. Inspect via `get_restart_status` or `intendant://controller-restart`.

### Notes & guarantees

- Restart state persists to the session dir as `controller_restart.json`.
- `restart_after` defaults to `"turn_end"`; only `"turn_end"` or `"now"` are
  accepted (others rejected). String inputs are trimmed before validation.
- `restart_command`, when provided, must be non-empty/non-whitespace.
- At least one restart action is required: `restart_command` and/or
  `auto_start_task=true`.
- `max_attempts` must be `>= 1` (`0` rejected). Optional `status`,
  `handoff_summary`, and the cancel `restart_id` guard treat whitespace-only as
  unset.
- If `restart_after="now"` and execution fails after validation,
  `schedule_controller_restart` reports `"ok": false` with `execution_error`, and
  the persisted phase becomes `"failed"` with `last_error` populated.
- `controller_turn_complete` only accepts restarts in
  `"awaiting_turn_complete"`; duplicate/late handshakes (e.g. `"phase": "ready"`)
  are rejected to prevent double execution.
- `get_restart_status` and `intendant://controller-restart` redact
  `turn_complete_token` as `"[redacted]"`; only `schedule_controller_restart`
  returns the raw token (for the final handshake).
- `request_controller_loop_halt`, `clear_controller_loop_halt`,
  `intervene_controller_loop`, and `get_controller_loop_status` return/emit
  normalized loop-health data (flags, lock owner PID + liveness, latest run
  pointers, active PID counts). The control socket's `command_result.data`
  mirrors the same structured payloads.

### Controller recursion profile

Recommended for Codex/Claude-style controllers:

- Set `auto_start_task=false` (or omit it — `false` is the default).
- Use `restart_command` to relaunch the external controller process.
- Treat `start_task` as optional E2E testing, not the default recursion path.

## Controller Loop Monitoring

For `restart_command` wrapper scripts, loop artifacts live under
`.intendant/controller-loop/`:

- Stable pointers: `latest` (symlink), `latest.pid`, `latest.status.json`,
  `latest.jsonl`, and the singleton `active.lock/` (`pid`, `run_id`,
  `acquired_at`).
- Inspect: `tail -f .intendant/controller-loop/latest/codex.jsonl`,
  `cat .intendant/controller-loop/latest.status.json`.
- Intervention markers: `touch .intendant/controller-loop/request_halt`
  (persistent), `request_halt_after_cycle` (one-shot legacy), `request_stop`
  (graceful), `request_abort` (immediate). History:
  `.intendant/controller-loop/latest/intervention.log`.
- Per-run PIDs: `.intendant/controller-loop/<run_id>/wrapper.pid` and
  `codex.pid`. The Codex wrapper applies stop/abort to the recorded Codex
  process and its visible descendants so nested app-server children are not
  orphaned.

## Typical Agent Workflow

1. `get_status` for the current phase and budget.
2. Poll `get_logs` with `since_id` to stream new events (or subscribe to
   `intendant://logs`).
3. On an approval, `get_pending_approval` gives the command preview → `approve`,
   `deny`, or `skip`.
4. On an `askHuman`, `get_pending_input` gives the question → `respond`.
5. `quit` when done.

## MCP Client

Intendant can also be an MCP **client**, connecting to external MCP servers
configured in `intendant.toml` so the agent can use their tools alongside
Intendant's native ones (`mcp_client.rs`).

### Configuration

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

At startup, `McpClientManager::connect_all()` spawns each server, discovers its
tools, and registers them as `mcp__<server>_<tool>` (e.g. a `filesystem` server's
`read_file` → `mcp__filesystem_read_file`). Tool calls with the `mcp__` prefix
are routed to the right server. If a server fails to connect, it is skipped with
a warning; other servers and native tools keep working.

### Trust model — read this before adding a server

Each `[[mcp_servers]]` entry is launched as a **child process with the user's
full privileges**:

```rust
let mut cmd = Command::new(&config.command);
cmd.args(&config.args);
let transport = TokioChildProcess::new(cmd)?;   // mcp_client.rs
```

Intendant performs **no checksum verification, no signature check, and no
sandboxing** of MCP server binaries. Adding an MCP server is equivalent to adding
a line to your `~/.zshrc` that runs a binary.

Mitigating defaults: `mcp_servers = []` by default, and `intendant.toml` is
**git-ignored**, so the repo ships no MCP servers. Treat copying an
`intendant.toml` between machines like copying shell rc files — read it before
you source it.

## See Also

- [Autonomy & Approvals](./autonomy.md) — the autonomy model that gates
  approvals.
- [Integrations](./integrations.md) — `ControlMsg`, the control socket, and the
  web gateway WebSocket protocol.
