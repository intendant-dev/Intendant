# External-Agent Orchestration

Intendant can hand a whole task to a third-party coding harness — **OpenAI
Codex**, **Claude Code**, **Kimi Code**, or **Pi** — and supervise it as a
subordinate worker. The external tool does the actual coding; Intendant wraps
it in its own oversight, lifecycle, display, and computer-use surfaces. Codex,
Claude Code, and Kimi receive a scoped Intendant MCP bootstrap. Pi intentionally
does not pretend to have MCP: its supervised system prompt points at the scoped
`$INTENDANT ctl` bootstrap instead.

This is one of the four current execution shapes — Direct, Orchestrate,
Sub-Agent, and External-Agent (see
[Agent Execution & Multi-Agent Orchestration](./multi-agent.md)). It is selected
by `--agent <backend>` or the `[agent] default_backend` config key.

## Why

These CLIs are excellent autonomous coders but live in their own terminals, with
their own approval prompts, no shared display, and no voice/phone reach. Wrapping
one in Intendant gives you:

- **One oversight surface.** The supervised agent's command/file approval requests
  are lifted into Intendant's frontends (web dashboard, MCP, `--json`) and the
  same autonomy policy that governs the native agent.
- **Display & computer use.** MCP-capable backends receive a scoped `intendant`
  MCP server over the running gateway. Pi receives the same session-scoped
  authority through `$INTENDANT ctl`; this preserves Pi's intentionally small
  core instead of inventing a fake MCP integration.
- **Presence & multi-session.** The supervised session is just another session on
  the [EventBus](./architecture.md); the [presence layer](./presence.md) narrates
  it and the daemon can run several alongside native agents
  (see [control plane & daemon](./control-plane-and-daemon.md)).

External-agent control rides the same vocabulary as everything else:
`ControlMsg` (inbound) and `AppEvent` (outbound) on the EventBus (`event.rs`).
The verbs are backend-shaped (steer a turn, fork a thread, roll back) rather
than the native action set.

## The `ExternalAgent` Trait

Every backend implements one async trait, `ExternalAgent`
(`src/bin/caller/external_agent/mod.rs`). The controller supervises through this
contract and never touches a backend's wire protocol directly:

```rust
#[async_trait]
pub trait ExternalAgent: Send + Sync {
    fn name(&self) -> &str;

    // Lifecycle
    async fn initialize(&mut self, config: AgentConfig)
        -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError>;
    async fn start_thread(&mut self) -> Result<AgentThread, CallerError>;
    async fn shutdown(&mut self) -> Result<(), CallerError>;

    // Turns
    async fn send_message(&mut self, thread: &AgentThread, message: &str) -> Result<(), CallerError>;
    async fn send_message_with_images(/* … */) -> Result<(), CallerError>;          // default: text-only
    async fn send_message_with_attachments(/* … */) -> Result<(), CallerError>;     // default: stage files + prelude

    // Oversight
    async fn resolve_approval(&mut self, request_id: &str, decision: ApprovalDecision)
        -> Result<(), CallerError>;
    async fn interrupt_turn(&mut self) -> Result<(), CallerError>;                  // default: unsupported error
    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError>;          // default: unsupported error

    // Rich thread control (Codex)
    async fn thread_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError>; // default: unsupported
    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError>;           // default: unsupported
    async fn rollback_thread_turns(&mut self, thread_id: &str, n: u32) -> Result<(), CallerError>;
    async fn activate_thread(&mut self, thread_id: &str) -> Result<(), CallerError>; // local adapter state only
    fn supports_user_message_rewind(&self) -> bool;                                  // default: false

    // Exact provider request payload (if the backend exposes one)
    async fn context_snapshot(&mut self) -> Result<Option<AgentContextSnapshot>, CallerError>;
}
```

`initialize()` spawns the backend process and returns a channel of normalized
**`AgentEvent`**s; everything the backend emits (deltas, messages, reasoning,
plan updates, tool start/output/complete, approval and structured-question
requests, diffs, usage/vitals facts, limit-rejected turns, and termination) is
translated into that enum so the controller's display and oversight code is
backend-agnostic. `AgentEvent::Scoped { thread_id, turn_id, .. }` wraps inner
events when a backend multiplexes several threads or native sub-agents through
one process (Codex threads and Kimi `:btw`/swarm agents).

`AgentConfig` carries the working dir, model, approval policy, the
**`web_port`** (used to generate the MCP-over-HTTP config), an optional
`resume_session` id, and the Codex-only knobs (`sandbox`, `reasoning_effort`,
`web_search`, `network_access`, `writable_roots`). Kimi's adapter additionally
receives its launch profile (`thinking`, permission mode, plan mode, and swarm
mode); Pi receives model, thinking level, and an exact active-tool override.
Backends that don't model a field ignore it.

The supported backend identities are the `AgentBackend` enum (`Codex`,
`ClaudeCode`, `Kimi`, `Pi`). `from_str_loose()` accepts the canonical short
forms plus older/display forms (`codex`, `claude-code`/`claude_code`/`cc`,
`kimi`/`kimi-code`/`kimi_code`, and
`pi`/`pi-coding-agent`/`pi_coding_agent`/`pi coding agent`, case-insensitive);
`as_short_str()` emits the canonical wire form that matches the dashboard
dropdown's `<option value>`.

Gemini CLI was previously supported as a backend and was retired in July 2026;
persisted sessions from it remain readable but cannot be resumed.

## Per-Backend Reference

`create_external_agent()` (`external_supervision.rs`) constructs the right
adapter from `[agent.<backend>]` config, then `run_external_agent_mode()`
(`external_mode.rs`) drives the supervise loop.

| | **Codex** (reference impl) | **Claude Code** | **Kimi Code** | **Pi** |
|---|---|---|---|---|
| Module | `external_agent/codex/` (mod, threads, wire, context_trace, reader) | `external_agent/claude_code.rs` | `external_agent/kimi_code/` (mod, bridge, events, review, rpc, runtime, websocket, wire) | `external_agent/pi.rs` |
| Spawn command | `codex app-server` | `claude -p --output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio --permission-mode <mode>` | Kimi 0.27: `kimi server run --foreground --port 0 --log-level silent`; Kimi 0.28: `kimi web --no-open --port 0 --log-level silent` | `pi --mode rpc --no-extensions --no-approve --extension <private> --append-system-prompt <bootstrap>` plus session/model/thinking/tool flags |
| Wire protocol | JSON-RPC over JSONL (`app-server`) | stream-json over stdio | bearer-authenticated loopback REST + reconnecting cursor/snapshot WebSocket (`server-v1`), plus a typed allowlist over authenticated reflected v2 RPCs | documented LF-delimited JSON RPC over stdio |
| Intendant capability injection | Per-process `-c mcp_servers.intendant.{type,url,bearer_token_env_var}` overrides plus scoped env; no workspace config file | Inline `--mcp-config '{…}'` JSON with an environment-expanded Authorization header | Per-session bridge home containing generated `mcp.json`; scoped bearer stays in the child environment | No MCP. Scoped `$INTENDANT`, `INTENDANT_MCP_URL`, and session authority support on-demand `intendant ctl` calls |
| Multi-thread | Yes — many threads per process | No | Yes — the main session plus native `:btw` and swarm agents | No — one Pi RPC process/session per wrapper |
| Native thread id | Yes | Yes — announced via `AgentEvent::NativeSessionId` on the first turn (placeholder `claude-code-session` until then; `--resume` keeps the id stable so resumed threads are canonical immediately) | Yes — returned at create/resume before the first prompt | Yes — `get_state.sessionId` before the first prompt |
| Mid-turn steer | Yes (`turn/steer`) | Yes — a stdin user message is absorbed into the running turn at the CLI's next checkpoint (verified on 2.1.215; 2.1.207 discarded such lines). No stdout echo, so delivery is inferred at the next model checkpoint; an idle session delivers the steer immediately as its own turn | Yes — Kimi queued prompts plus `prompts::steer`; a completion race becomes an ordinary immediate follow-up without losing text | Yes (`steer`) |
| Mid-turn interrupt | Yes (`turn/interrupt`) | Yes (`control_request` `interrupt`; the process survives for follow-up turns) | Yes — active prompt abort, with a session abort fallback | Yes (`abort`) |
| Token usage / context meter | Yes | Yes (`message_delta` + `result` usage; context window from `modelUsage`) | Yes — usage events plus typed `agentRPCService.getContext` snapshots: Kimi's current post-compaction model history and measured `tokenCount`, scoped to the exact selected main/composite agent, with the configured model catalog's context window | Yes — assistant-message usage/cache fields plus model context window from state/model events |
| Reasoning trace | Yes | Yes (`thinking` blocks) | Yes — thinking deltas/messages | Yes — thinking deltas/messages |
| Rollback turns | Yes (`thread/rollback`) | No → session reset | Yes — native `undo`, including edit-and-rerun of an active historical user turn | No → fork or reset |
| Fork / side threads / review / goals / compact / fast / memory-reset | Yes (`thread_action`) | `compact`, `fork` (respawns via `--resume <id> --fork-session`), `side` (`/btw` — the same respawn carrying a side boundary + question as the child's first prompt; lineage `fork_relationship: "side"`), the full `goal*` family (wrapper goal engine), and live `model` / `permission-mode` — all via universal `thread_actions`. No fast/review/memory-reset — see [Dashboard and Station parity](#dashboard-and-station-parity-codex-vs-claude-code) | Native `compact`, head and exact historical real-user-turn-boundary `fork`, `side`/`:btw`, `undo`, archive/restore/rename, goal get/set/pause/resume/complete/clear with enforced token/turn/wall-clock budgets, live model/thinking/permission/plan/swarm switches, official normal↔highspeed model toggling, supervisor-enforced tool-free read-only review turns over bounded controller-collected workspace evidence, background-task list/output/cancel, exact per-agent active-tool control, model catalog, and destructive per-agent context clear. Kimi has no persistent-memory plane equivalent to Codex `memory-reset`, explicit “mark budget-limited” setter, arbitrary item/message/child fork anchor, or child-only undo | Native compact and rename; fork/side respawn from the parent session; live model and thinking. No native goals/review/fast/memory reset |
| Native sub-agents | Yes — collab tools spawn real attachable threads (`SubAgentToolCall`) | Yes — the in-band `Agent`/`Task` tool; async children stream `parent_tool_use_id`-tagged envelopes, surfaced as ephemeral `task-*` child sessions on the same `SubAgentToolCall`/relationship rail | Yes — native swarm and `:btw` agents retain their own ids, scoped activity, relationships, status, and results | No — upstream Pi deliberately omits built-in sub-agents |

All four spawn through `crate::platform::spawn_command(&cfg.command)` with the
working dir set to the project root. Codex and Claude pipe their protocol over
stdio and forward stderr into the session activity log; Pi does the same for
its RPC stream. Kimi starts its local
server in silent mode, reads the one-line ephemeral origin from stdout and the
private bearer token from its isolated bridge home, validates the API/RPC
contract, and immediately unlinks the on-disk token while retaining it only in
the supervisor's HTTP clients. It then drains both streams; REST or WebSocket
failures are normalized as backend errors for every frontend.

Kimi's exact native tool-call ids also keep tool starts, streaming output, and
completion correlated in persistent daemon sessions. The legacy Codex/Claude
presence lane coalesces those `AgentStarted` rows with model activity; Kimi
leaves them unsuppressed because its server stream provides a stable,
deduplicated lifecycle boundary.

### Passive protocol compatibility watch

Every supervised Codex, Claude Code, Kimi Code, or Pi process carries a passive
compatibility watch. It fingerprints the resolved executable with filesystem metadata and,
while a user-started session is already running, compares fixed wire
discriminants against the adapter's embedded vocabulary. Unknown message
types, methods, subtypes, item types, and critical root-field type changes are
persisted under `<state-root>/diagnostics/external-agent-compatibility/` and
surfaced in the session log. Observation records include the resolved and
canonical executable paths, their filesystem fingerprint, and a strictly
allowlisted numeric release string when the handshake supplies one. Finding
records contain only fixed field names, JSON value kinds, and opaque SHA-256
fingerprints for protocol identifiers; raw identifiers, messages, prompts,
tool arguments, stderr, and model output are never retained. The store is
bounded: each handshake prunes the profile's artifact directories down to the
four most recently observed fingerprints, so upgrade residue does not
accumulate.

The initial vocabulary baseline is Claude Code 2.1.210, Codex app-server
0.144.1, the complete projected Kimi Code server-v1/agent-event-bus vocabulary
in 0.27.0/0.28.0, and Pi RPC as source-audited in `pi-mono` package 0.81.1.
Known-but-intentionally-ignored notifications are
included so the
watch reports new protocol surface, not ordinary traffic the adapter already
chose to ignore. Structural-check changes bump a separate contract revision,
which is folded into the manifest digest.

`GET /api/external-agents` reports the matching artifact fingerprint,
contract-manifest digest, in-band reported version when available, finding
counts, and one of `unobserved`, `no_drift_observed`, or `drift`. An executable
replacement naturally returns to `unobserved` until an ordinary supervised
session reaches its protocol handshake. `no_drift_observed` is deliberately
not called “verified”: passive evidence cannot prove semantic behaviors such
as steering or interruption. A diagnostics write failure is logged and held
as an in-memory error finding for the daemon lifetime, so storage trouble
cannot turn observed drift into a misleading `no_drift_observed` status.

The artifact fingerprint covers the resolved executable itself. A custom,
stable wrapper can change what it launches without changing its own filesystem
identity; in that case the status remains historical until the next ordinary
session handshake, and `last_observed_secs` is the staleness signal. The watch
does not claim to identify opaque targets hidden behind wrappers.

The watch consumes **no additional model quota**. Neither the status path nor
session setup runs `--version`, starts a probe conversation, or contacts a
provider. Configured commands may be arbitrary wrappers, so even apparently
harmless command-line probes are reserved for a future explicit,
budget-authorized verification action. Unknown or known-but-unsupported
Claude control requests and Codex server requests fail closed; they are never
eligible for autonomy or a session-wide approve-all grant. Each watch keeps
upstream-known request vocabulary separate from the narrower set for which
Intendant has an exact request classification and response shape; observing a
known-but-unsupported request is itself a compatibility finding.

### Capability plumbing per backend

What each supervised backend actually receives:

| | **Codex** | **Claude Code** | **Kimi Code** | **Pi** |
|---|---|---|---|---|
| MCP tool exposure | `tool_profile=core` (bootstrap set) | `tool_profile=core` (bootstrap set) | `tool_profile=core` (bootstrap set) | none — Pi has no built-in MCP |
| Session-scoped authority | child env → MCP Authorization header | child env → environment-expanded MCP Authorization header | child env → generated bridge MCP bearer-variable reference | child env → scoped `intendant ctl` bootstrap |
| `session_id` scope in URL | yes | yes | yes | yes, consumed by `ctl`, not advertised as Pi MCP |
| `$INTENDANT` + `INTENDANT_MCP_URL` env (`ctl` bootstrap) | yes (+ `INTENDANT_MANAGED_CONTEXT`) | yes | yes | yes |
| Guidance channel | managed-context developer message | first-prompt bootstrap addendum | generated bridge-home MCP config + ordinary tool discovery | appended system prompt naming truthful `ctl --help` discovery |

The MCP bootstrap set for Codex, Claude Code, and Kimi includes the CU path
(`read_screen`, `take_screenshot`, `execute_cu_actions`, `list_displays`,
`grant_user_display`, `revoke_user_display`) and the shared-view tools
regardless of managed context; managed-context/fission tools remain
managed-only.

### Environment at spawn

A supervised CLI does **not** inherit the controller's environment. Every
backend spawn starts from `env_clear()` plus an explicit allowlist
(`external_agent::apply_external_child_env_policy`): system basics (`PATH`,
`HOME`, `USER`, `SHELL`, `TERM`, `TMPDIR`, `TZ`, `LANG`/`LC_*`, …), the
platform's process-bootstrap set (macOS `__CF_USER_TEXT_ENCODING`; Linux
`DISPLAY`/`WAYLAND_DISPLAY`/`XDG_*`; Windows `SYSTEMROOT`, `COMSPEC`,
`PATHEXT`, `APPDATA`, `USERPROFILE`, …), proxy vars
(`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`, either case), the CLIs'
own config-home pointers (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
`KIMI_CODE_HOME`, `PI_CODING_AGENT_DIR`), and the
`INTENDANT`/`INTENDANT_*` control channel. Everything else is dropped —
in particular the controller's provider API keys
(`OPENAI`/`ANTHROPIC`/`GEMINI_API_KEY` and every `*_API_KEY`/`*_API_TOKEN`
shape), ambient host credentials (`SSH_AUTH_SOCK`, `AWS_*` secrets,
`GH_TOKEN`/`GITHUB_TOKEN`, `KUBECONFIG`, `DOCKER_CONFIG`, registry tokens),
the Linux D-Bus session bus (`DBUS_SESSION_BUS_ADDRESS` — desktop-keyring
reach), and `NODE_OPTIONS`. Backends authenticate with their own
subscription auth under their own homes (`~/.codex`, `~/.claude`,
`~/.kimi-code`, `~/.pi/agent`) or a vault-leased home injected explicitly at
spawn; they never see the controller's model keys.

`INTENDANT_ENV_PASSTHROUGH` (comma-separated exact names,
case-insensitive, set on the controller) deliberately extends the
allowlist — e.g. `INTENDANT_ENV_PASSTHROUGH=SSH_AUTH_SOCK` for supervised
sessions that must push over SSH. Provider API keys never pass, even if
named there. The same variable exempts names from the ambient-credential
scrub at the native runtime's spawn boundary. The runtime's own exec/PTY
shells apply a second defense-in-depth scrub that currently does not consult
this variable, so classified ambient credentials such as `SSH_AUTH_SOCK` do
not survive into native shell commands even when the runtime process inherited
them.

### Codex (the original reference backend)

Codex was the first fully wired backend and remains the reference for its
backend-specific managed-context, fission, and item-anchor rewind features.
Claude Code and Kimi use the same universal supervision rails, exposing their
own native capabilities where the upstream CLIs provide them.

- **MCP injection — per-process config.** Codex receives the Intendant MCP
  server exclusively through command-line `-c` overrides on the app-server
  process; Intendant does not write, back up, or restore
  `<workspace>/.codex/config.toml`. The command line includes
  `-c mcp_servers.intendant.type="http"`,
  `-c mcp_servers.intendant.url="…"`, and
  `-c mcp_servers.intendant.bearer_token_env_var="INTENDANT_MCP_BEARER_TOKEN"`.
  The URL on argv carries session/profile routing but no credential; the
  session-derived bearer exists only in the explicitly injected child
  environment and Codex sends it as `Authorization: Bearer`. The user's
  toggles ride as further `-c` overrides:
  `tools.web_search=true`, `model_reasoning_effort="…"`,
  `sandbox_workspace_write.network_access=true` (only in `workspace-write`), and
  `sandbox_workspace_write.writable_roots=[…]`.

  Codex app-server launches in `managed_context = "managed"` also pass config
  overrides that suppress inherited user-global Codex MCP servers and plugin/app
  connectors by default. This keeps managed startup to Intendant MCP plus
  explicitly requested Codex toggles instead of loading arbitrary global
  Google/Gmail/Linear/Slack/etc. servers. Set
  `INTENDANT_CODEX_INHERIT_MCP_SERVERS=1` for a managed launch that should
  deliberately inherit the user's Codex MCP/plugin configuration. Vanilla
  supervised launches preserve Codex's normal user configuration inheritance.

  Codex uses `tool_profile=core` by default to avoid MCP tool-schema bloat. The
  core profile keeps a small bootstrap surface: `get_status`, the shared-view
  tools, and the minimal display/CU path (`list_displays`, `grant_user_display`,
  `revoke_user_display`, `read_screen`, `take_screenshot`,
  `execute_cu_actions`) for managed **and** vanilla sessions; managed context
  additionally exposes the managed-context/fission tools. Broad or rare
  Intendant operations should be discovered lazily through
  `intendant ctl --help`, `intendant ctl tools list`, and focused subcommand
  help. Supervised Codex sessions receive
  `INTENDANT=/absolute/path/to/intendant`, `INTENDANT_MCP_URL`,
  `INTENDANT_SESSION_ID`, and `INTENDANT_MANAGED_CONTEXT`, so agent shells can
  run `"$INTENDANT" ctl ...` without relying on user PATH setup. Claude Code
  gets the same treatment: its inline MCP JSON contains a token-free scoped URL
  and an `Authorization` header that expands
  `${INTENDANT_MCP_BEARER_TOKEN}` inside the child. Both backends also receive
  the token-bearing `$INTENDANT_MCP_URL` for `ctl` through their private
  environment, plus `$INTENDANT`/`INTENDANT_SESSION_ID`; no bearer value rides
  either process's argv. Claude's first-prompt bootstrap addendum names the
  bootstrap tools and the `ctl` discovery flow.

  For dashboard/browser validation against an already-running Intendant web port,
  managed agents should use the repository helper instead of generating ad-hoc
  Chromium/CDP scripts:

  ```bash
  node scripts/validate-dashboard.cjs --port <web_port> --selector '<css>'
  node scripts/validate-dashboard.cjs --url http://127.0.0.1:<web_port>/app \
    --wait-for-function '() => Boolean(window.someReadyFlag)'
  node scripts/validate-dashboard.cjs --port <web_port> \
    --station-probe rendered --station-probe dock-hidden
  node scripts/validate-dashboard.cjs --url http://127.0.0.1:<web_port>/app \
    --require-current-static --require-station-state --require-ai-provider-session \
    --require-external-agent codex \
    --station-probe rendered --station-interaction-probe --json
  node scripts/validate-dashboard.cjs --launch-dashboard --port <throwaway_port> \
    --dashboard-arg --no-tls --headed \
    --require-station-state --require-managed-context-state \
    --require-ai-provider-session --require-external-agent codex \
    --station-probe rendered --station-interaction-probe \
    --screenshot /tmp/intendant-station.png --json
  node scripts/validate-dashboard.cjs --launch-dashboard --port <throwaway_port> \
    --selector '<css>'
  node scripts/validate-dashboard.cjs --hold-dashboard --port <throwaway_port> --json
  ```

  The helper launches a fresh isolated headless Chromium, waits for CDP
  readiness, supports selector/function waits plus named Station probes and
  optional headed Station interaction/screenshot artifacts, falls back when Node
  has no WebSocket module, and prints compact PASS/FAIL output with bounded log
  excerpts on failure. Station renders canvas-only — a WebGPU scene with a
  canvas-2D WASM fallback and an invisible hotspot overlay for keyboard
  access, with no DOM dock — so the named probes assert against the rendered
  scene: `dock-hidden` passes when the legacy DOM dock is absent from the
  page, and the interaction probe drives the rendered hotspots. Use `--require-station-state
  --require-managed-context-state --require-ai-provider-session
  --require-external-agent codex --station-interaction-probe --screenshot <png>
  --json` for meaningful headed Station QA instead of scraping the helper's
  temporary DevTools profile, and review the returned screenshot path before
  counting the run as a product pass. With `--launch-dashboard`, it starts the built
  intendant binary as `--web <port>`, waits for HTTP readiness, and
  stops the temporary process afterward; use that instead of a separate
  foreground/nohup dashboard launch. For real headed CU/browser E2E that needs
  a temporary dashboard to remain available while separate CU tools run, use
  `--hold-dashboard` as a foreground long-running command, read the printed URL,
  run the CU steps while the command stays active, and interrupt the command
  afterward for helper-owned cleanup. It does not default to port 8765; pass
  `--port`/`--url` or let it derive the port from `INTENDANT_MCP_URL`. Managed
  Station product validation against an already-running controller should add
  `--require-current-static --require-station-state --require-managed-context-state --require-ai-provider-session --require-external-agent codex`:
  the first compares the served embedded app/WASM/JS assets with this worktree's
  `static/` files so a stale controller on the target port fails clearly, the
  second fails if Station sessions, events, managed context, and peers are all
  empty, the managed-context requirement fails unless Station exposes active
  live managed/context session state rather than historical, stopped, idle,
  unknown, or stale state, and the provider requirement fails if Station only
  exposes placeholder/no-provider session state instead of a non-placeholder
  provider, model, and session id.
  The external-agent requirement then verifies that same real Station session is
  backed by Codex, so native/default-provider sessions cannot satisfy Codex QA.
  Omit `--require-current-static` only for generic connectivity checks against a
  controller intentionally built from another worktree, and use
  `--allow-empty-station-state` only for renderer smoke tests where an empty
  fixture is expected. Managed agents should keep validation bounded: one
  primary smoke, at most one diagnostic retry such as `--diagnostics --json`,
  then either a targeted fix or a clear partial-validation conclusion with the
  helper reason/logs/diagnostics.

  `[agent.codex] managed_context = "vanilla"` is the default and is safe for
  upstream Codex or the original Codex fork. Set it to `"managed"` only when
  launching the Intendant-aware Codex fork — and configure that fork's binary
  as `[agent.codex] managed_command`: managed sessions then spawn it
  automatically while vanilla sessions keep using `command` (the upstream
  CLI), so flipping the mode never points the wrong binary at a session.
  Without `managed_command`, managed sessions fall back to `command`, which
  then must itself be the fork (legacy setups); the dashboard and the Station
  controls panel flag that ambiguity. Managed mode advertises
  `rewind_context` / `rewind_backout`, suppresses Codex auto-compaction, and
  uses same-thread rollback/restore to keep the active thread informationally
  dense. Rewinds are not just emergency context-limit recovery: they can also be
  appropriate after noisy tool output, failed exploration, or a long research
  branch whose useful result can be crystallized into a compact primer. Managed
  agents should not call `list_rewind_anchors` at startup or merely prefetch the
  catalog while backend pressure is `ok`/below the recommended density
  threshold; listing anchors is for an immediate recovery, density handoff, or
  targeted cleanup rewind that is expected to materially improve the live
  transcript. Model-driven rewinds must first call `list_rewind_anchors`; by
  default it returns a bounded compact page with exact `item_id` values, short
  semantic rows, `filtered_total`, and `next_offset`. Use `offset`/`limit` to
  page through the catalog, `query` for semantic or exact-id filters, and
  `reverse=true` for newest-first order. When a compact row is ambiguous,
  `inspect_rewind_anchor` returns a small before/after window for the candidate.
  `rewind_context` still validates the exact `item_id` against the current
  rollout before mutating the Codex thread.
  When backend-reported pressure is at or above the rewind-only threshold,
  `list_rewind_anchors` defaults to recovery candidates: anchors where the best
  available usage evidence for the cut — for `after`, the first backend token
  report that actually measured the anchor's content (a report persisted
  between a tool call and its output never measured that output); for
  `before`, the nearest prefix-measuring backend report or the labeled prefix
  estimate, whichever is higher — stays below that threshold with enough
  normal-tool resume headroom. An anchor whose prior rewind proved
  insufficient is not re-offered at either position. The default recovery
  catalog narrows `positions` to accepted `rewind_context` values. `include_pruning_estimates=true` adds approximate
  discard sizes to compact rows, while `detail=true` requests diagnostic
  detailed pages. Passing `include_non_recovery=true` is an
  audit escape hatch, not the normal recovery path. Anchors inside the active
  managed-context recovery span, starting at the recovery kickstart prompt, are
  not valid recovery targets because they would preserve recovery instructions
  and anchor-discovery calls. A successful
  `rewind_context` only proves the lineage mutation was
  applied; Intendant and Codex keep normal tools hidden until a later backend
  token report confirms the active thread is below the rewind-only limit.
  Below the rewind-only threshold, status may be `watch` when density is getting
  high, but the MCP payload also reports `normal_tools_allowed=true` and
  `required_action=continue_or_rewind_optional`; that is an advisory density
  signal, not an emergency recovery state. When Intendant sends a post-turn
  density handoff, a model-selected exact rewind target must still be materially
  useful: the chosen anchor/position must be expected to clear the recommended
  density threshold, and Intendant rechecks backend-reported pressure after the
  rollback before replaying any held follow-up. If the committed rewind remains
  above the density threshold, the harness sends another maintenance handoff
  instead of treating the shallow rewind as successful.

  Managed Codex relies on the minimal lineage patch separating Codex's thread id
  from the Responses `prompt_cache_key`. Same-thread restore keeps the active
  thread id. Fork/backout creates a new Codex thread id but inherits the rollout's
  lineage prompt-cache key, so branch recovery is git-style without deliberately
  resetting cache routing. The old `allow_cache_reset` flag is accepted only for
  compatibility with older clients; it is not required for managed forks.
  Dashboard edits of a user message that is still active use the normal precise
  Codex rollback path. If the clicked message has been overwritten by a managed
  rewind, Intendant treats the edit as a branch operation: it finds the newest
  saved pre-rewind rollout that still contains that exact message text, forks that
  rollout, rolls the child back to just before the selected user turn, and starts
  the child with the replacement message. The original compacted thread is not
  mutated silently.

  Managed launches inject a generic managed-context developer-instructions
  block (transcript density, GUI via Intendant MCP, rewind discipline) into
  `thread/start` / `thread/resume`. Projects can extend it: when
  `<working_dir>/.intendant/codex-managed-instructions.md` exists, its contents
  are appended under a "Project managed-context instructions" delimiter for
  every managed Codex session launched in that project, capped at 16 KiB with
  an explicit truncation marker; read failures are logged and non-fatal. Keep
  repo-specific validation/QA guidance there rather than in the generic block
  that every project pays a token tax for — this repository's own file carries
  the `validate-dashboard.cjs` usage and helper retry discipline above.

- **Rich `thread_action` ops** (`codex/threads.rs`): `compact`, `fast`, `fork`,
  `side`/`btw` (open a side conversation) and `side-close`, `review`,
  `goal`/`goal-set`/`goal-get`/`goal-edit`/`goal-clear`/`goal-pause`/`goal-resume`/
  `goal-complete`/`goal-budget-limited`, and `memory-reset`. Side threads can be
  steered and rolled back independently of the
  parent (`rollback_thread_turns`, `activate_thread`). Codex also reports native
  **sub-agent** activity (`AgentEvent::SubAgentToolCall`) and per-fork token
  accounting.

  `/fast` is also a session-bootstrap command: when a new-session request contains
  exactly `/fast`, the supervisor starts a new idle Codex session and passes
  `serviceTier: "priority"` on `thread/start`. Existing targeted `/fast` commands
  remain live thread actions and toggle the service tier for future turns in that
  Codex session; if typed while the prompt is in steer mode, the supervisor still
  converts it to the thread action rather than sending `/fast` as model text.

- **Diff handling.** Codex's `turn/diff/updated` sometimes carries paths only
  inside the diff body; `parse_diff_file_paths()` recovers them from the unified
  diff when the explicit `files_changed` list is empty.

#### Model-driven fission (managed Codex)

Managed Codex sessions can fork themselves. Alongside the rewind tools, the
managed-context gate exposes a fission surface (`fission_tool()` in `mcp/tool_gate.rs`)
that lets the model split separable work into parallel **full-context sibling
branches** and join the results back deliberately. The spawn/import mechanics
live in `thread_actions.rs` (`apply_fission_spawn_action` /
`apply_fission_import_action`), the runtime contract in
`fission_lifecycle.rs`, and the durable state in `fission_ledger.rs`.

The tools:

- **`fission_spawn`** — `branches: [{objective, write_scope?: [paths], name?}]`
  (1-4 entries), an optional `use_worktree: bool` override applying to all
  branches, and the usual optional `session_id`. Each entry forks the parent
  Codex thread (`thread/fork`) into a sibling that inherits the full
  conversation context and runs as a real supervised session. Returns the
  `group_id`, branch session/thread ids, and worktree paths.
- **`fission_control`** — `{group_id, op: "wait" | "import" | "cancel" |
  "detach", branch_session_id?, timeout_s?}`. `branch_session_id` is required
  for `import`/`cancel`/`detach`; omitting it for `wait` waits for *any*
  branch of the group to reach a terminal status.
- **`claim_fission_canonical`** — `{group_id, branch_session_id,
  expected_canonical_session_id?}`: claims the group's canonical outcome,
  first-writer-wins; pass the current canonical id to deliberately
  compare-and-swap. Refused for detached groups and for branches carrying a
  sticky `detached`/`cancelled` status.

**Charters are the whole context contract.** Branches fork from the **last
completed turn** and do not see the spawning turn — the in-flight tool call
(including the `fission_spawn` arguments) is invisible to the children — so
every `objective` must be self-contained: each fact, path, and constraint the
branch needs. The supervisor injects a `<fission_charter>` developer message
into each fork (`fission_charter_message`, `thread_actions.rs`) carrying identity
(group id + branch session id), the objective, the **owned write scope** (or
"read-only"), the worktree if any, and the report-back contract: work only
within your write scope; end the final turn with a concise outcome summary
(it becomes the ledger summary); call `claim_fission_canonical` if the result
should become the group's canonical outcome; prefer the fission ledger in
`get_status` over reading sibling raw logs. The branch then starts with a
"Begin your fission charter: …" kickoff task as its own session, inheriting
the parent's launch config (binary, sandbox, approval policy, managed mode).

**Worktree default.** A branch that declares a `write_scope` in a git project
gets an isolated checkout by default: `git worktree add` of a
`fission/<short-group-hash>-<ordinal>` branch from `HEAD` under
`.intendant/worktrees/` (`fission_branch_uses_worktree`); the spawn-level
`use_worktree` overrides in both directions. Because a linked worktree's git
metadata lives under the *main* repository's `.git`, the fork passes a
per-fork config override appending the main repo's common `.git` directory to
`sandbox_workspace_write.writable_roots` (re-including launch-level roots,
which a per-fork value would otherwise replace), so branch commits work under
Codex's `workspace-write` sandbox (`fork_thread_with_options_params`,
`codex/threads.rs`). Any failed spawn step removes the worktree that branch created,
and a fission fork leaves the parent thread untouched — its context-pressure
floor persists.

**The fission ledger** (`fission_ledger.json` in the session log dir) is the
durable join surface. Groups are keyed by `(parent session, spawn anchor)`;
the anchor is the very `fission_spawn` tool-call item of the active turn,
falling back to the newest rollout anchor named for `fission_spawn` and then
to the catalog head (recorded honestly as tool `fission_spawn:head`). A
daemon bus watcher (`fission_lifecycle.rs`) feeds branch lifecycle into it:
done/task-complete events → `completed` with the branch's outcome summary
(capped at 240 chars); interruption → `cancelled`; error-shaped teardowns →
`failed`, generic ones → `ended` (normalizes to `completed`); project
file-change events accumulate per-branch `changed_files` (a deduplicated
union capped at 200 entries — branches in isolated worktrees edit outside the
watch root and accumulate nothing). Statuses normalize onto `running |
blocked | completed | failed | detached | cancelled`; `detached` and
`cancelled` are **sticky** — written only by explicit supervisor APIs and
never overwritten by a passive observation, so a stray completion event from
a detached branch's still-running child cannot resurrect it — and a terminal
status is never downgraded by a later, coarser observation. Routes for
still-running branches are rehydrated from persisted ledgers at daemon
startup; detached groups are skipped.

**Waiting.** `fission_control(op="wait")` polls the ledger (1 s cadence) with
`timeout_s` clamped to **[5, 300] (default 60)** and returns a JSON group
snapshot tagged `outcome: terminal | still_running | detached`.
`still_running` is a **normal** result, not an error — re-issue the wait or
keep working and check `get_status` later. A detached group refuses the wait
and points at the salvage paths instead.

**Importing.** `fission_control(op="import")` builds a compact
`<fission_import>` payload from the ledger — objective, normalized status,
summary, `changed_files`/`tests_run`, worktree, and the `raw_log` pointer —
injects it as a developer message into the parent thread, and stamps the
branch's `imported_at` marker (re-importing refreshes it). Import is
artifact-level: it never changes the branch status. `op="cancel"` stops the
branch session through the same control-plane intent as the dashboard stop
button and flips the ledger status to the sticky `cancelled`; `op="detach"`
severs the whole group *without* stopping its sessions.

**Detach-on-rewind.** A managed rewind whose cut precedes a group's spawn
anchor severs that group. Right after a successful rollback,
`apply_external_context_rewind` detaches every group whose anchor was cut out
of the effective history (decided against a pre-rewind snapshot of anchor
line positions), flips its non-terminal branches to `detached`, clears the
canonical claim if the canonical branch itself was detached, drops the
branches' parent-facing delivery routes — a late completion cannot
auto-deliver into the rewound parent — and records the severed ids as
`detached_fission_group_ids` in the durable rewind record. Branches that had
already reached a terminal status keep it: their recorded results stay real
even though the join point is gone. Detached groups are sticky — they refuse
`wait`/`import`/canonical claims and cannot host new branches; salvage a
detached branch's results manually via its `raw_log` pointer, or revisit the
parent's pre-rewind lineage with `rewind_backout` on the covering record.

**Fission is ex-ante, rewind is ex-post.** The managed developer-instructions
block carries a fission policy (`codex/threads.rs`): prefer `fission_spawn` with a
self-contained charter over a deep in-context detour when a subtask is
separable, favor breadth before pressure builds, keep working after spawning
instead of idling behind a branch, and wait only when genuinely blocked. The
fission tools share the managed-context exposure gate but are deliberately
**not** in the rewind-only allowlist: under rewind-only context pressure they
are blocked like any other ordinary tool — fission is not a recovery tool.

**Observability.** `get_status` embeds the session's merged `fission_ledger`
document — groups plus per-branch charters, import/detach markers, and any
canonical claim — and the dashboard reads the same merged view from
`GET /api/managed-context/fission` (newest-first, capped at 50 groups of up
to 50 branches), rendered as the Managed tab's fission panel (see
[Web Dashboard](./web-dashboard.md)).

### Claude Code

Spawned in non-interactive stream-json mode (`-p --input-format stream-json
--output-format stream-json --verbose --include-partial-messages`) with
`--permission-prompt-tool stdio`, so permission prompts arrive as
`control_request`/`can_use_tool` messages on the JSON stream and become
`AgentEvent::ApprovalRequest` (file tools carry the `FileChange` category).
Protocol details that are load-bearing (compatibility vocabulary verified
through Claude Code 2.1.210):

- **Approvals**: the allow response **must echo the original tool input as
  `updatedInput`** — a bare `{"behavior":"allow"}` fails the CLI's schema
  validation and the tool never runs. `AcceptForSession` additionally returns
  `updatedPermissions` built from the request's own `permission_suggestions`
  (`addRules`/allow entries only), always retargeted to the `session`
  destination — a supervised run never writes grants into the checkout's
  `.claude/settings.local.json`. `Decline` denies with a supervisor message;
  `Cancel` denies with `interrupt: true`, aborting the whole turn. Unknown
  control-request subtypes are rejected with a control error (fail closed),
  never auto-approved.
- **Tool results ride `user`-type messages** (as `tool_result` content
  blocks), not assistant messages; the adapter closes `ToolStarted` items
  from there, and force-closes still-open tools as cancelled at turn end.
- **Native session id**: Claude Code stamps `session_id` on every stdout
  message once the first turn begins. The adapter announces it via
  `AgentEvent::NativeSessionId`; the drain loop upgrades Intendant's
  identity (`AppEvent::SessionIdentity`) and writes the external overlay so
  `--continue`/resume finds the native id. `--resume <id>` keeps the same
  session id and context, so a resumed thread is canonical from
  `start_thread`.
- **Interrupt**: a client→CLI `control_request` with subtype `interrupt`
  aborts the running turn (its `result` arrives as
  `error_during_execution`, mapped to a completed turn rather than a
  backend error when Intendant requested the interrupt); the process stays
  usable for follow-up turns.
- **Steer**: a user message written while a turn runs is **absorbed into
  the running turn** at the CLI's next checkpoint (verified live on
  2.1.215; 2.1.207 was observed discarding such lines, 2.1.200 absorbed
  them). `steer_turn` writes the stream-json user message and the drain
  tracks it as a pending runtime steer — the CLI never echoes the injected
  message on stdout, so delivery is inferred at the next model checkpoint
  (turn completion at the latest). An idle session keeps the "no active
  turn" marker and delivers the steer immediately as its own turn. Goal
  notices still queue as next-prompt preludes: unlike a steer's
  best-effort injection, a notice must never silently vanish on a
  discard-era CLI.
- **Usage**: per-API-call usage from `message_delta` stream events plus the
  turn `result` feed `AgentEvent::Usage`; the context window comes from the
  result's `modelUsage` map (200k default until the first result).
  `thinking` blocks surface as `AgentEvent::Reasoning`. Turn `result`s with
  error subtypes (`error_max_turns`, `error_during_execution`, …) emit
  `AgentEvent::BackendError` before completing the turn.
- **Thread actions**: `compact` writes the native `/compact` user message —
  the CLI answers `status: compacting` → `compact_boundary` (with
  `pre_tokens`) → a free zero-usage `result`, and the session keeps its
  facts (no `control_request` equivalent exists). `fork` never reaches the
  adapter: the drain sees `ForkHandling::RespawnResume` and starts a NEW
  supervisor session with `--resume <parent> --fork-session`; the child
  announces its own session id on its first prompt, which upgrades its
  identity and emits the `fork` relationship from the persisted
  `forked_from` lineage. Until that first prompt the forked window has no
  native identity yet — expected, not a bug. `side` (`/btw`) rides the
  same respawn: Claude Code's native `/btw` is interactive-only (over
  stream-json the CLI answers with a synthetic "isn't available in this
  environment" result — probed on 2.1.206), so the side conversation is a
  respawned `--fork-session` child whose first prompt carries the side
  boundary (inherited history is reference-only, no mutations) plus the
  question, and whose lineage persists as `fork_relationship: "side"` —
  the identity upgrade emits an ephemeral `side` relationship (same flag
  as Codex's in-process side start), so the dashboard renders a side
  child window, fully conversable for follow-ups. The side contract text
  is `external_agent::SIDE_CONVERSATION_CONTRACT`, shared verbatim with
  Codex's side-thread developer instructions; display surfaces (session
  meta, `SessionStarted`) strip the contract and show the bare question.
  Unlike a Codex side thread there is no `side-close` — the respawned
  child is its own live backend, so the dashboard offers **Stop session**
  for it (the kebab's Close side appears only when the parent advertises
  `side-close`). With `max_budget_usd` set, forks and side children
  inherit the parent's counted spend (see the config reference). Both
  dispatch sites (the external drain and the presence loop's inline
  mirror) share `respawn_resume_thread_action`.
- **Live reconfig** (wired): `control_request` subtypes `set_model` and
  `set_permission_mode` (verified on 2.1.201) back the `model` /
  `permission-mode` thread actions; the Launch-config modal applies both
  live on save.
- **In-band sub-agents** (the `Agent` tool; `Task` pre-2.1): **async by
  default** on 2.1.201 — the Agent tool_result returns launch metadata
  immediately (`tool_use_result.status: "async_launched"`, suppressed by
  the adapter) and the child keeps working, potentially past the parent
  turn's `result`. The spawn emits `AgentEvent::SubAgentToolCall`
  (`inProgress`), which the drain turns into an ephemeral child session
  (`task-<tool_use_id suffix>`): identity (`claude-code` source), a
  `subagent` `session_relationship`, a no-follow-up capability ceiling, and
  a started log line. Child activity arrives ONLY as complete
  assistant/user envelopes tagged with top-level `parent_tool_use_id` (no
  stream deltas) — the adapter scopes them to the child window, including
  per-child open-tool tracking so a parent turn's end doesn't cancel a
  live child's tools. `system:task_started` supplies the `task_id`
  correlation key; `system:task_notification` (status + summary) is the
  authoritative end and emits the scoped terminal state (child window ends;
  duplicate notifications are absorbed; EOF shuts down any still-open
  children). An unrecognized `parent_tool_use_id` (resume replay)
  lazily materializes its child from the envelope's `task_description`.
  After an async child completes, the CLI **spontaneously starts a
  notification turn** (fresh `init` → the model reports the outcome → its
  own `result`); while idle the drain absorbs it as a normal spontaneous
  round. Known race (accepted): if that notification turn lands while a
  real turn is being drained, its `result` can complete the round early —
  same class as Codex's spontaneous rounds.
- The init message's `mcp_servers` status for the injected `intendant`
  server is logged (warn on `failed`/missing) so a broken loopback MCP is
  visible from frontends instead of silently running without CU tools.

The Intendant MCP server is passed **inline** as a JSON string to
`--mcp-config` (not a file path). Its argv-visible URL carries `session_id` +
`tool_profile=core` but no token; the Authorization header expands
`${INTENDANT_MCP_BEARER_TOKEN}` from the child environment. The child also gets
the token-bearing `INTENDANT_MCP_URL` plus `$INTENDANT` and
`INTENDANT_SESSION_ID` so `"$INTENDANT" ctl ...` works from its shell. The
first user message carries a
bootstrap addendum naming the MCP bootstrap tools
(`read_screen`/`take_screenshot`/`execute_cu_actions`, shared-view), the lazy
`ctl --help` discovery flow, and the dashboard-validation helper.
`--permission-mode` is always passed explicitly (normalized; `manual` and
empty map to `default`): when the flag is omitted the CLI resolves its
default from the user's own `~/.claude/settings.json`
(`permissions.defaultMode`), silently running a different mode than the one
recorded in the session's launch config. The reader reconciles the
`system:init` echo's `permissionMode` against the requested mode and logs a
warning on divergence (once per distinct echoed value).
`--allowedTools` is added from config when set.

### Kimi Code

Kimi uses the local `server-v1` interface rather than ACP. ACP is convenient
for editor interoperability, but Kimi Code 0.27-0.28's ACP facade does not
expose the native goal, undo, fork, side-agent, structured-interaction,
background-task, usage, and live-profile surfaces needed for Intendant parity.
The adapter therefore starts one private foreground server process per
supervised session and speaks its bearer-authenticated loopback REST and
WebSocket APIs. It starts 0.27's `kimi server run` entrypoint and retries with
0.28's `kimi web --no-open` only when the first process exits with Kimi's exact
entrypoint-removal diagnostic; other startup failures remain failures.
The server binds port `0`; its chosen origin is read from stdout, and its bearer
is read from `server.token`. Neither value is put on argv or emitted as an
Intendant event. Once health, metadata, and the typed v2 method catalog have
been authenticated, Intendant unlinks `server.token`; the bearer remains only
inside the supervisor's in-memory REST/RPC clients. Those loopback clients
explicitly disable environment/system proxies so neither the bearer nor
private control traffic can be redirected through an egress proxy.

Kimi keeps its server lock, token, journal, and MCP config under
`KIMI_CODE_HOME`. Sharing the user's primary home among simultaneous
supervisors would serialize unrelated sessions and would require mutating the
user's `mcp.json`, so Intendant creates a stable, 0700 bridge under
`<kimi-home>/intendant-bridges/session-<hash>`. It mirrors the primary home's
auth, config, sessions, skills, plugins, and caches (symlinks where supported,
refreshed copies otherwise), while owning `server`, `server.token`, and a 0600
merged `mcp.json`. The generated `intendant` entry wins over a same-named user
entry, preserves every other user MCP server, and names
`INTENDANT_MCP_BEARER_TOKEN`; no bearer is serialized into the file. A
malformed primary `mcp.json` fails closed. Windows bridge objects receive and
verify a protected owner/SYSTEM/Administrators DACL rather than relying on
ambient profile inheritance. The predictable managed parent and session leaf
must both be real directories: preplanted/replaced symlinks and canonical-path
escapes fail closed before chmod, sync, pruning, or MCP generation. MCP files
are written through a randomly named, create-new private temporary file and an
atomic rename, so a guessed PID-based symlink cannot redirect the write.

When symlinks are unavailable, Intendant monitors the one known rotating OAuth
file, `credentials/kimi-code.json`, in a real copy-fallback bridge. Every
250 ms and once more after the Kimi child stops, a changed bridge credential is
published through an owner-private atomic replacement only if the primary
credential still byte-matches the last value this monitor synchronized. A
logout, new login, or concurrent refresh changes or removes that primary and
permanently detaches the monitor; stale bridge authority is never replayed or
resurrected. This bounds the abrupt-crash window in which Kimi's rotated grant
exists only in the bridge and keeps repeated Windows sessions authenticated
without turning general bridge copy-back into an authority restore.

Bridge teardown otherwise copies back only Kimi's native `sessions/` tree and
`session_index.jsonl`. Configuration, plugins, caches, MCP declarations,
server state, and every credential other than the live CAS-guarded refresh
file are never copied from a bridge into the primary home: a long-lived bridge
may hold a stale snapshot, so broader copy-back could undo a logout or
resurrect removed authority. Before each launch, copy-backed mirrors are also
reconciled recursively with the current primary home, removing
credential/config/plugin/cache entries that the user deleted while retaining
bridge-only session history.
Duplicate native session ids across bridge and primary history resolve to the
copy with the newest filesystem activity. Append-only journals merge only when
one byte-exact ordered record sequence is a prefix of the other. Divergent
histories fail closed instead of being treated as an unordered set (which
could reorder turns); file identity, length, and prefix content are rechecked
immediately before any suffix append. Link-like source/destination entries,
including Windows junctions and other reparse points, are never traversed.

The WebSocket driver subscribes after create/resume, snapshots first, and keeps
a per-session sequence/epoch cursor. Disconnects reconnect with bounded
backoff; gaps or epoch changes trigger a REST snapshot resync before deltas
continue. Eight consecutive reconnect failures terminate the supervised
backend instead of leaving a deceptively live session. Translation covers:

- assistant and thinking deltas/messages, plan/todo changes, model/config
  echoes, diffs, tool start/output/finish, background tasks, errors, usage and
  context limits;
- approval requests plus session-scoped approval, and Kimi's distinct
  structured-question objects and answer schema;
- native sub-agent spawn/start/suspend/resume/complete/fail events, scoped to
  attachable child windows with normal relationship and activity rails;
- session goals, compaction, archive state, prompt/turn completion, and a
  snapshot of pending interactions after reconnect.

Kimi also exposes several controls that are richer than the other shipped
adapters:

- **True queued steering.** Intendant submits a queued prompt and calls
  `prompts::steer`; if the active turn ends in that race, Kimi starts the text
  as the next ordinary turn, so it is never discarded.
- **Native undo, edit/rerun, and historical forks.** `undo` backs `/undo` and
  the universal active-user-message edit flow. The fork-point catalog exposes
  every active real-user turn boundary: Intendant asks Kimi to fork the full
  wire history, then synchronously applies the exact native undo count before
  publishing or subscribing to the child. The planned head carries a compact
  revision/floor/generation proof; after Kimi reports undo success, Intendant
  reparses the child and requires the exact derived post-undo turn count and
  fingerprint. A missing legacy proof or any mismatch fails closed and archives
  the unpublished child. Arbitrary item/message anchors remain Codex-specific.
- **Native side/swarm agents.** `:btw` creates a real Kimi agent inside the
  session. Its first prompt carries the universal side-conversation contract,
  and its activity remains scoped by `session-id:agent-id`. Swarm mode is a
  launch and live profile switch, not an Intendant emulation.
- **Live profile changes.** Model, thinking effort, permission mode
  (`manual`/`auto`/`yolo`), plan mode, and swarm mode can change without
  restarting the server; changes emit the same config-vitals rail as launch.
- **Authenticated v2 controls.** The public v1 facade omits several registered
  services that the same loopback server exposes through bearer-authenticated
  v2 reflection. Intendant never offers a generic RPC passthrough: it validates
  the expected method catalog and exposes only typed, fixed service/method
  calls for goal completion/budgets, active tools, native current-context
  history/`tokenCount`, model/profile facts, the configured model catalog, and
  context clear. `getContext` is required by the startup capability handshake;
  Intendant never substitutes the durable transcript when it is absent. RPC
  and REST response bodies are stream-capped at 32 MiB, and ordinary file
  attachments stream from a fixed-size opened file rather than buffering the
  whole upload. This matters because reflection also contains
  implementation-private methods that must not become a user dispatch surface.
- **Native session actions.** Compact, head fork, undo, archive, restore,
  rename, goal get/set/pause/resume/complete/clear, and side start/close are
  advertised through `SessionCapabilities.thread_actions`. Goal limits are
  native and enforced for tokens, turns, and wall-clock time. The same rail
  exposes Kimi's background-task list/output/cancel endpoints, active/inactive
  tool inventory, exact active-tool replacement (including an intentionally
  empty set), activate-all, configured-model catalog, supervisor-enforced
  read-only working-tree review with exactly zero active Kimi tools and a
  bounded workspace-only evidence packet collected by the controller, K2.7
  normal/highspeed toggle, and per-agent context clear. A review starts only
  from an idle session with no pending interaction; after evidence collection
  and tool disabling, the prompt set is checked again. If an interaction
  appeared or Kimi queued rather than immediately started the review, the
  adapter restores the prior tools and fails closed because its evidence is no
  longer point-in-time. Protocol drift never widens tools until the exact
  submitted prompt is proved absent.
- **Native task inspector.** Kimi task ids/statuses feed the same dashboard
  inspector used by Claude Code. The adapter refreshes bounded native output
  previews into the data-only task registry, so frontends can tail output
  without retaining Kimi's loopback bearer; running tasks expose Cancel through
  the ordinary live thread-action lane.
- **Child-scoped controls.** Every Kimi `:btw` side and native swarm child
  advertises only the operations the server can target to that exact agent:
  tool inventory/replacement/activate-all and destructive context clear.
  Side conversations additionally advertise close. Dashboard, control-plane,
  and slash-command routing preserve the composite child id in `threadId`;
  parent-only operations remain blocked instead of silently affecting `main`.
- **Native attachments.** Images are submitted as base64 content and ordinary
  files are uploaded to Kimi's file API before the prompt, preserving their
  name, media type, and size.

The capability list intentionally omits operations Kimi 0.27-0.28 cannot perform
honestly: arbitrary item/message/child fork anchors, item-anchor rewind, an
explicit “mark budget-limited” transition, persistent-memory reset, and
independent undo of one child agent. Historical forks are exact only at active
real-user turn boundaries; superseded revisions, system prompts, and
child-agent turns are not offered as anchors. Goal objective edits validate
first, then use Kimi's native cancel-and-create sequence because there is no
atomic edit endpoint. Native budget fields can be set but not individually
cleared; cancel-and-recreate is the only reset and also resets goal identity
and accounting, so Intendant never disguises it as a clear-limit edit. Kimi
represents budget exhaustion as `blocked` plus reached/over-budget facts;
Intendant derives the universal `budget-limited` display status but does not
advertise a setter Kimi lacks.

Active-tool control is deliberately not described as Claude
`allowed_tools`: Claude's field is an approval allowlist where empty means
unrestricted, while Kimi persists an exact active-tool name set where empty
means no optional tools. An unset Intendant override leaves Kimi's current
profile in control. `tools-all` resolves the live registered catalog and
activates every name; it does not pretend to reconstruct an undefined profile
default that Kimi's RPC cannot write.

Create, resume, attach, `--continue`, per-session launch pins, restart with
saved config, protocol compatibility diagnostics, session catalog/replay,
usage aggregation, detail/deep/message search, names, aliases, file watching,
vault leases, and the dashboard/Station control surfaces all use the same
backend-neutral rails as Codex and Claude Code. Persisted Kimi history is read
from its session store, including nested agents; leased and staged Kimi homes
are swept alongside the normal home so custody does not make transcripts
temporarily invisible.

### Pi

Pi is integrated as an upstream, replaceable cognitive engine, not copied into
Intendant and not wrapped in a terminal scraper. Intendant launches the
documented RPC mode and speaks LF-delimited JSON on stdin/stdout. The adapter
uses correlated request ids and bounded response waits; its initial
`get_state` handshake has a 25-second whole-handshake ceiling and a bounded
pre-response event buffer. EOF, malformed JSON, failed requests, and protocol
drift become ordinary supervised backend errors instead of leaving a window
that looks live. The child is killed and reaped if any startup step fails.

The launch deliberately keeps upstream Pi's useful defaults while removing
ambient executable code from the trust boundary:

```text
pi --mode rpc --no-extensions --no-approve \
   --extension <intendant-private-supervision.ts> \
   --append-system-prompt <truthful ctl bootstrap> \
   [--session-id <id> | --session <id> | --fork <id> --session-id <child>] \
   [--model <pattern>] [--thinking <level>] [--tools <exact,list> | --no-tools]
```

`--no-extensions` disables discovered user/project extensions but still admits
the one explicit Intendant extension. `--no-approve` suppresses Pi's own
project-code trust ceremony. Pi's independent project-context loader still
reads the normal `AGENTS.md`/`CLAUDE.md` instruction chain, so supervision does
not throw away repository policy merely to prevent project code execution.
`PI_SKIP_VERSION_CHECK=1` and `PI_TELEMETRY=0` eliminate startup network noise;
Intendant never runs `pi --version`, starts a probe conversation, or spends
quota merely to populate compatibility status.

The private extension is the approval boundary. Upstream's read-only built-ins
`read`, `grep`, `find`, and `ls` pass without a prompt except when they target
Pi's own agent home (including Pi's `~`, `@`, `file://`, Unicode-space, and
canonicalized symlink aliases). `write`, `edit`,
`bash`, and every unknown/future tool block on Intendant's existing approval
rail. The extension sends a fixed marker and bounded structured preview through
Pi's extension UI `select`; Intendant maps the choice to approve once, approve
that tool name for this supervised session, deny, or cancel. An absent UI,
extension exception, malformed marker, unknown future blocking UI method, or
unrecognized tool all fail closed. This is an authorization gate, not a second
filesystem sandbox: an approved `bash` command has the authority of the
supervised child process, subject to the operating environment and any separate
host controls.

Pi has no built-in MCP and Intendant does not claim otherwise. The child gets a
private session-scoped `$INTENDANT`/`INTENDANT_MCP_URL` environment and an
appended system instruction to discover missing platform capabilities with
`"$INTENDANT" ctl --help`. Computer use, shared displays, peers, Agenda, and
Memory therefore remain Intendant services above the harness. The scoped URL
and bearer stay out of argv and out of the model-visible prompt. As with every
external child, provider API keys and ambient host credentials are removed from
its environment; Pi authenticates from its own agent home.

The RPC translation covers user/assistant messages, streaming text and
thinking, tool start/output/completion, file activity, errors, usage/cache
facts, model/context-window facts, compaction boundaries, and turn lifecycle.
Native image input is preserved. Pi's `steer` and `abort` commands implement
mid-turn follow-up and interrupt. Universal thread actions expose:

- `compact`, including optional custom instructions;
- `fork` and `side`, implemented by a new supervised process using Pi's native
  `--fork <parent> --session-id <child>` path (side adds the shared
  read-only-side-conversation boundary as the child's first prompt);
- native session rename;
- live `set_model` and `set_thinking_level`, with the resulting launch pins
  persisted so a later reattach does not silently revert them.

Pi intentionally advertises no native sub-agent, goal, plan/todo, review,
memory-reset, rollback, or MCP surface. Those are honest upstream boundaries,
not placeholders. Intendant's own orchestration remains available above Pi by
starting separately supervised sessions; it is not smuggled into Pi as a fake
native feature.

Global defaults live in `[agent.pi]` and are editable in Settings: command,
model, thinking, and the exact active-tool override. `allowed_tools` has three
states: omitted means Pi profile defaults, `[]` means no tools, and a non-empty
array is the exact active set. Model and thinking can also change live from a
session's Configure controls; tools are launch-time because Pi's public RPC
does not expose active-tool mutation. There is deliberately no daemon-wide
`PiRuntimeConfig` mirror: new sessions load the project TOML, reattached
sessions reload it as their base and reapply their persisted launch overlay,
and active sessions use RPC actions.
That keeps the cognitive-engine boundary thin instead of spreading Pi-specific
state through the agent OS.

Native Pi v3 history is parsed directly from
`$PI_CODING_AGENT_DIR/sessions/--encoded-cwd--/*.jsonl` (default
`~/.pi/agent`). A session file is a header followed by parent-linked entries;
the last complete entry is the active leaf. Replay walks that leaf's parent
chain, while search indexes every physical branch and labels inactive siblings
as superseded. Usage aggregation counts all physical assistant entries because
all branches may have incurred billed/subscription usage. Torn trailing JSONL
is ignored, scans and row sizes are bounded, exact upstream session-id grammar
is enforced, and the transport edge injects roots so tests never scan a real
home. Catalog, replay, message search, resume lookup, names, CWD, model/thinking,
leased homes, and credential-free staged transcripts all share that parser.

Pi can use its ordinary local `auth.json`, including its `openai-codex` OAuth
provider for a ChatGPT subscription. Custody-managed sessions use `oauth:pi`:
Intendant materializes a private `PI_CODING_AGENT_DIR` containing `auth.json`,
best-effort copies `settings.json`, stages `sessions/` before cleanup, and
deletes the leased home on expiry/revocation/shutdown. Access-token leases
recursively reject every Pi refresh token and API-key credential. Browser
refresh currently supports Pi's `openai-codex` entry with the same public
form-encoded refresh request Pi uses; other Pi OAuth providers require the
explicit full-credential mode. A Pi process can rotate a refresh token while
using a full-credential materialized copy, but Intendant does not yet
compare-and-swap that mutated copy back into the browser vault. Deleting the
leased home can therefore leave the vault holding the superseded refresh
token; prefer access-token mode for `openai-codex`, and re-import a changed
full credential before tearing its lease down.

## Rate-limit Parking

Claude Code's `rate_limit_event` with status `rejected`, correlated with the
turn's terminal result, becomes `AgentEvent::TurnLimitRejected` rather than an
ordinary completed round. The backend process remains usable, but the rejected
round did no work and consumes no round budget. Both the foreground
external-mode lane and persistent-daemon lane apply the same
`external_supervision.rs` policy:

- The rejected message remains pending. If the wire supplied `resetsAt`, it is
  resent after that instant plus 30–90 seconds of jitter; any one sleep is
  capped at six hours so long windows are rechecked. Without a reset time,
  consecutive rejections back off from 5 to 30 minutes.
- Follow-ups arriving while parked queue FIFO behind the pending resend instead
  of being burned against the exhausted backend. A cancelled follow-up is
  skipped when the queue drains.
- An interrupt cancels the park and drops its pending resend (other queued
  follow-ups remain queued). Backend termination also cancels the resend; in
  the persistent lane, queued user messages can run against the next agent
  build. `/new` is an explicit reset and drops both the park and that lane's
  queued messages.
- An out-of-band `compact` action is refused with the reset-time explanation
  while a park is armed; request it again after the reset.

The park is an in-memory session-lane state, not a durable scheduler. Activity
and session-log rows make the pause, queued messages, cancellation, and resend
visible.

## Dashboard and Station parity

The per-session dashboard features (Activity → Timeline agent windows and the
[Station](./station.md) canvas) were built against Codex first. An audit
(2026-07-04 @ `d590ad94`) found that the *rails* are almost all
backend-neutral already — what is Codex-only is the *producers*. The
standing rule for closing the gap: **wire Claude Code (and native sessions)
into the universal rails; do not clone `codex_*`-shaped UI paths.** The
rails that already exist end-to-end and are backend-agnostic:
`SessionCapabilities` (universal `follow_up` / `steer` / `interrupt`
booleans plus backend-specific knobs), `AgentEvent::GoalUpdated/GoalCleared`
→ the `session_goal` event and its log replay, `session_relationship` plus
the lineage/fission ledgers and their `/api` serving, the
capability-gated affordances in `app.html`, and `external_wrapper_index`.

The original audit table below remains the detailed Claude Code catch-up
record. Kimi was integrated after those rails became universal, so it plugs
into them directly rather than adding a parallel `kimi_*` UI architecture.

| Feature | Universal rail (exists today) | Codex producer | Claude Code today → plan |
|---|---|---|---|
| Steer / interrupt / stop affordances | `SessionCapabilities.{follow_up,steer,interrupt}`; the UI gates on capabilities, not backend type | emits all three | **Parity** (emits all three) |
| Usage / context meter | `AgentEvent::Usage` → `UsageSnapshot` / `ContextSnapshot` | `token_count` notifications | **Parity** (`message_delta` + `result` usage) |
| Goal chip in the agent-window header (`/goal`) | `SessionGoal` type; `AgentEvent::GoalUpdated/GoalCleared`; `session_goal` outbound + log replay; the window chip renderer is backend-neutral; op semantics + wire conventions (statuses, budget shape, objective limit, notice texts) live in the shared `external_agent::GoalEngine`, which the Claude Code adapter and the native presence loop both run | native `thread/goal/*` RPCs | **Live — wrapper goal engine in the adapter.** The full `goal*` op family is advertised and dispatched; goal state lives in `CcShared`, notices always queue as a prelude on the next prompt (mid-turn stdin delivery is unconfirmable and one CLI era discarded it; consecutive notices coalesce in order, and updates never buy a turn), and budget spend is measured in FRESH tokens (uncached input + cache creation + output — cache reads excluded), flipping `active` → `budgetLimited` at exhaustion. Engine state is per-process: after a resume the chip rehydrates from the log but the engine starts empty (re-set the goal) |
| Per-window action menu (fork / compact / goals / …) | **Universal (landed):** `SessionCapabilities.thread_actions` op vocabulary + the `thread_action` control message (`codex_thread_action` stays a wire alias); the kebab and Station session actions render from the advertised op list, with the codex heuristic as legacy-replay fallback | full op set | **`compact` + `fork` + `side` live.** `compact` sends the native `/compact` user message (status → `compact_boundary` → free result); `fork` respawns via `ForkHandling::RespawnResume` → `ResumeSession { fork: true }` → `--resume <parent> --fork-session` (the child binds its own native id + the `fork` relationship on its first prompt); `side` (`/btw`) is the same respawn with `relationship_kind: "side"` and the boundary + question as the child's first prompt. No Claude analog planned: fast / review / memory-reset |
| Relationship wiring (parent/sub/fork header chips + SVG wires; Station edges) | `session_relationship` event + lineage ledger + `/api` serving + both renderers — all backend-neutral | side / subagent / fork / fission / rewind emitters | **`fork` + `side` + `subagent` emitted.** Fork/side on the forked child's first identity announcement (persisted `forked_from` + `fork_relationship` lineage); in-band Task sub-agents ride `SubAgentToolCall` → ephemeral `task-*` child sessions with `subagent` relationships (fission observations stay Codex-only by design) |
| Per-session persisted launch overlay | `SessionAgentConfig` + `ConfigureSessionAgent` / `Restart` (universal `agent_command` + backend fields, bundled as `LaunchOverrides`). The daemon owns this overlay: implicit resumes (`ResumeSession` from auto-attach or a Resume button) carry NO launch overrides — only the explicit configure/restart flows do — and every config funnel drops (or, for the explicit flows, rejects with an error) an `agent_command` whose executable is a *different* backend's CLI than the session's source, so cross-agent contamination can neither launch nor persist | all `codex_*` fields | **Live.** `claude_model` / `claude_permission_mode` / `claude_allowed_tools` / `claude_effort` pins with inherit-vs-pin sentinels ("default" stays a pinnable permission mode; `all` pins explicitly-unrestricted tools), Launch-config modal rows, and LIVE apply of model + permission on save via the `model` / `permission-mode` thread actions (`set_model` / `set_permission_mode` control requests, verified on 2.1.201) |
| Global runtime config pane | `Set*` ControlMsgs + `*ConfigChanged` broadcast + Settings/Control panes | 12 knobs | **3 knobs** (model / permission mode / allowed tools) — by design; grows only when CC grows equivalent concepts |
| Station controls-panel runtime block | the controls panel renders per-backend blocks | approval policy / managed-context / fork-binary warning | **Live.** Model pills (default + the CLI's latest-version aliases fable/opus/sonnet/haiku, with a truthful `custom:` row for out-of-alias pins) and permission pills (default/edits/plan/bypass), gated `backend == "claude-code" \|\| launch_agent == "claude-code"` exactly like the Codex block, dispatching `set_claude_model` / `set_claude_permission_mode` (persisted to `intendant.toml` + broadcast, same as the dashboard Control pane) |
| Plan / todo display | `AgentEvent::PlanUpdate` exists | emits plan updates | **Translation live for both tool families.** `TodoWrite` tool calls translate into `PlanUpdate` (statuses normalized via the shared helper; the raw call and its acknowledgment are suppressed, failures still warn, a sub-agent's TodoWrite scopes to its `task-*` child; malformed inputs fall back to plain-tool rendering). Print-mode Claude Code (verified on 2.1.201) does not enable `TodoWrite` and exposes the incremental Task tools instead, so the adapter also folds `TaskCreate`/`TaskUpdate` into per-scope task-list state and re-renders the full snapshot on every mutation: the CLI only reveals the assigned id in `TaskCreate`'s tool_result, so creates hold a provisional entry until the ack arrives (failed creates retract it), updates upsert by id (unknown ids materialize a placeholder row — creation may predate the supervisor), `status: "deleted"` removes the row, and both acks are suppressed like TodoWrite's. `TaskList`/`TaskGet` stay plain tool calls |
| Session vitals chip (git / prompt-cache / rate limits — the operator-statusline port) | `SessionVitals{git,cache,limits}` + `session_vitals` outbound/log/replay; a change-detecting hub (`session_vitals.rs`) merges sections from two producers — the fetch-free git prober (branch, dirty, ahead/behind, `merge-tree` parity, unpushed; primary session) and a bus listener over `UsageSnapshot` + `SessionRateLimits` that computes the latest request's cache-hit receipt + TTL anchor and folds rate-limit windows. Provider windows are ACCOUNT-scoped: the hub keeps one window store per backend source (freshest report per label, `observedAtEpoch`) and mirrors the merged view into every session of that source — a warning reported through one session elevates them all, and a session starting mid-warning inherits it; native sessions keep per-session header gauges. Reset countdowns are exact and tick client-side from `resetsAtEpoch` (the cache-ttl pattern); a window whose reset epoch passed reads "reset" until the next report; the once-per-escalation toast and once-per-idle-period cache alert also derive client-side (browser notifications only when permission is already granted). Station renders the same vitals as focus-panel rows (git/limits pre-formatted by the feed, the cache countdown live per frame) | `token_count` `last` bucket → hit receipt (no TTL — OpenAI's is undocumented, countdown hidden); `account/rateLimits/updated` `{primary,secondary}` windows → 5h/7d gauges, delivered as `RateLimitWindows` at the report | **Cache + limits sections live** — per-request reads/writes/uncached from the wire usage, TTL flavor from `cache_creation` ephemeral splits (1h beta) with a 5-minute default; every `rate_limit_event` (`five_hour`, and `seven_day` / `seven_day_overage_included` once elevated) updates its window's status/reset and emits `RateLimitWindows` immediately — a rejected turn produces no usage snapshot to ride (2.1.2xx sends no `utilization`, so no percentage is shown or synthesized). The native loop feeds the same rail through the derived `UsageSnapshot`, with `anthropic-ratelimit-*` per-minute headers as its gauges (header-less egress-relay calls degrade to none) |
| Managed context / fission / rewind family | managed-context tools + ledgers | patched managed fork only | **Out of scope for parity** — Codex-fork-specific by design; Claude Code manages its own context (`/compact`, auto-compaction) |

Kimi's current rail coverage is:

| Universal rail | Kimi producer |
|---|---|
| Follow-up / steer / interrupt / stop | native prompt submit, `prompts::steer`, prompt/session abort |
| Usage, context, reasoning, plan, tools, diffs | server-v1 event translation and reconnect snapshots |
| Approvals and questions | distinct approval and structured-question endpoints, both rendered through the shared interaction UI |
| Thread actions and goal chip | native compact/head-or-turn-boundary-fork/undo/archive/restore/side/rename; native goal get/set/pause/resume/complete/clear and enforced budgets; live model/thinking/permission/plan/swarm; normal/highspeed toggle; supervisor-enforced tool-free read-only review over bounded controller-collected evidence; background-task list/output/cancel; model catalog; exact per-agent active-tool report/set/all; per-agent context clear |
| Relationships and sub-agent windows | native `:btw` and swarm agent events scoped to child ids |
| Launch and persisted per-session config | command, model, thinking, permission, plan, swarm, and exact active-tool pins; Save applies every profile field live, Save & restart also replaces the binary |
| Global Settings, dashboard Control, Station controls | command, model, thinking, permission, plan, swarm, and exact active-tool defaults, all persisted and broadcast through the control plane |
| Catalog, replay, Stats, search, names | Kimi session-store parser plus external wrapper index, including leased/staged homes and child records |
| Credentials | local-login detection, private `kimi login` ceremony, `oauth:kimi` full-credential vault leases, cleanup/staging |

The intentional non-parity cells are upstream capability boundaries, not
missing UI: no arbitrary item/message/child fork point, item-anchor rewind,
explicit budget-limited setter or individual budget clear, Codex
managed-context/fission or persistent-memory reset, or independent undo of one
Kimi child agent in Kimi Code 0.27-0.28.

Catch-up order (each step unlocks UI in both surfaces at once):

1. ~~universal `thread_actions` capability + a Claude `thread_action`
   implementation (`compact`, `fork`)~~ — **landed** (window kebab and
   Station session actions render from the advertised ops; e2e phases 6–8);
2. ~~the wrapper-level goal engine~~ — **landed for Claude Code, then for
   the primary NATIVE session** (the engine's op semantics now live in the
   shared `external_agent::GoalEngine`; the presence loop answers the
   `goal*` family for its native session, advertises it via
   `SessionCapabilities`, delivers notices through the context-injection
   queue — absorbed at the next turn boundary of a running task, surviving
   idle gaps as a prelude so idle updates never buy a turn — and measures
   budgets in fresh tokens off the cumulative native usage; the kebab goal
   submenu and `/goal` slash light up from the advertised ops; Station
   renders goal state on the focus panel, command deck, and node ring).
   Still open: goals for supervisor-SPAWNED native sessions — their
   `run_direct_mode` loops answer no thread actions yet, and the
   supervisor's fallback responder says so honestly (it defers only for
   ops a live loop advertised);
3. ~~remaining relationship producers (in-band Task sub-agents)~~ —
   **landed** (async `Agent`-tool children become ephemeral `task-*` child
   sessions with scoped transcripts; `fork` already wired);
4. ~~per-session Claude overlay fields + Launch-config modal rows + live
   apply~~ — **landed** (drafted by an unattended session, adopted after
   review, finished with the modal UI and live model/permission apply);
5. ~~the Station controls Claude block~~ — **landed** (model + permission
   pill rows in the rendered controls panel).

All five catch-up items have landed, followed by the `TodoWrite` →
`PlanUpdate` translation (later extended to the incremental
`TaskCreate`/`TaskUpdate` fold print-mode sessions actually use) and goals
for the primary native session; what remains is goal support for
supervisor-spawned native sessions, plus the Station work tracked in
[station.md](./station.md).

## Approval Routing

When a supervised agent asks to run a command or change a file, the backend emits
`AgentEvent::ApprovalRequest` / `FileApprovalRequest`. `drain_external_agent_events()`
(`external_events.rs`) routes the decision through **the same autonomy policy and approval
registry as the native agent**:

```
External agent ─► AgentEvent::ApprovalRequest { request_id, command, category }
                       │
       map category ──►  CommandExecution → CommandExec
                         FileChange       → FileWrite
                       │
   autonomy.external_approval_decision(category)
        ├── AutoApprove ─► resolve_approval(Accept)            + AppEvent::AutoApproved
        ├── Reject ──────► resolve_approval(Decline)           + AppEvent::ApprovalResolved("deny")
        ├── headless &&  ─► resolve_approval(Decline)          (no interactive frontend → auto-deny)
        │   no json &&
        │   no web_port
        └── otherwise ───► AppEvent::ApprovalRequired { id, command_preview, category }
                              └─ await decision via ApprovalRegistry / JsonApprovalSlot
                                 approve      → Accept
                                 approve_all  → AcceptForSession
                                 deny         → Decline
                                 skip         → Cancel
                                 channel drop → Decline (fail safe)
                              └─ AppEvent::ApprovalResolved + resolve_approval(decision)
```

Because the request becomes an ordinary `AppEvent::ApprovalRequired`, every
frontend that already renders native approvals — the web dashboard,
the MCP `approve`/`deny` tools, and `--json` stdin — handles external-agent
approvals identically. `ApprovalDecision` (re-exported from `crate::approval`) is
the shared decision vocabulary; `AcceptForSession` is how "approve all" sticks for
the rest of the session. Note that `--web` providing a `web_port` is what keeps an
otherwise-headless run from auto-denying: it signals that an interactive frontend
exists.

## User Questions (AskUserQuestion)

Claude Code's `AskUserQuestion` tool is **not a permission request** — it's the
model asking the human to pick between structured options (question text, a
short header chip, 2–4 labeled options with descriptions, optional
multi-select). The adapter detects the tool inside the same `can_use_tool`
control request and emits `AgentEvent::UserQuestionRequest` instead of an
approval (a malformed input degrades to the generic approval prompt rather
than being dropped). The drain surfaces it as
`AppEvent::UserQuestionRequired { id, questions }` →
`OutboundEvent::UserQuestion`, and — deliberately unlike approvals — **never
auto-resolves it from autonomy policy or a session-wide approve-all grant**:
somebody asked a question; policy can't answer it.

Frontends answer with `{"action": "answer_question", "id", "answers":
{question → chosen label(s) or free text}}` (multi-select answers join with
", "). The adapter replies `allow` + `updatedInput.answers`, exactly what the
external CLI's own interactive picker returns, so the tool result reads "Your
questions have been answered: …". The web dashboard renders a dedicated
question panel (option buttons + free-text input + Skip), and presence narrates
the question text with its option labels. Dismissals (deny/skip) send a plain
`deny` — never `interrupt` — so the model continues gracefully without an
answer, and the bare approval verbs (`approve`/`approve_all` from clients that
only speak approvals) let the question through with a "proceed on your best
judgment" note instead of fabricating a choice. Headless runs without any
frontend answer the same way instead of blocking forever, mirroring the
external CLI's own away-from-keyboard fallback.

## Skills

Intendant installs every shipped skill machine-wide into the independent
`~/.agents/skills/` and `~/.claude/skills/` roots at daemon startup, so
supervised and bare Codex or Claude Code sessions see the shipped catalog
through their normal personal discovery. Intendant manages only marked
per-skill directories: the roots themselves and unmarked user-authored
collisions are always left untouched.

Starting an external session never copies skills into its project. Personal
global and project-scoped skills remain user-owned under the backend's normal
global or project path and belong in that project's ignore rules where
applicable. There is no Intendant-specific legacy skill path and no automatic
mirroring between Claude Code's `.claude/skills/` and the Agent Skills
standard `.agents/skills/`. See "Global distribution" in the configuration
chapter.

## Configuration

External-agent settings live under `[agent]` in `intendant.toml`
(`ExternalAgentConfig` in `project.rs`). An attached project uses its own file;
a projectless daemon uses `<state-root>/intendant.toml` (normally
`~/.intendant/intendant.toml`) for daemon-wide defaults. `default_backend`
selects the mode; the per-backend subtables tune each tool. All keys have
defaults, so a bare `[agent]` with just `default_backend` works.

```toml
[agent]
# Which backend to use when --agent is not passed. Omit/empty = native agent.
# Accepts: "codex", "claude-code", "kimi", "pi".
default_backend = "codex"

[agent.codex]
command          = "codex"            # binary on PATH or absolute path
model            = "gpt-5.6-sol"      # optional; omit to use Codex's default
approval_policy  = "on-request"       # untrusted | on-request | never
sandbox          = "workspace-write"  # read-only | workspace-write | danger-full-access
reasoning_effort = "medium"           # ""(default) | none | minimal | low | medium | high | xhigh | max | ultra
service_tier     = ""                 # ""(inherit Codex default) | priority (Fast) | flex | standard (explicit opt-out sentinel)
web_search       = false              # enable the Responses web_search tool
network_access   = false              # outbound net inside workspace-write only
writable_roots   = []                 # extra writable dirs (absolute), each → -c writable_roots
managed_context = "vanilla"          # vanilla | managed
context_archive = "summary"          # summary | exact | off — context snapshot archive mode ("Context replay" in the UI)

[agent.claude_code]
command         = "claude"
model           = "claude-sonnet-4-6"  # optional; any claude CLI --model value (e.g. "haiku")
permission_mode = "default"           # default (alias manual) | acceptEdits | plan | auto | dontAsk | bypassPermissions
allowed_tools   = []                  # e.g. ["Read", "Edit", "Bash"]; empty = all

[agent.kimi]
command         = "kimi"
model           = "kimi-code/kimi-for-coding" # optional; "k2.7 coding" is accepted too
thinking        = "high"                     # off | low | medium | high
permission_mode = "manual"                   # manual | auto | yolo
plan_mode       = false
swarm_mode      = true
# Exact active-tool replacement. Omit to inherit Kimi's profile; [] disables
# every optional tool (unlike Claude's empty allowlist, which means all).
allowed_tools   = ["Read", "Grep", "Glob", "AskUserQuestion"]

[agent.pi]
command       = "pi"
# Optional Pi model pattern or provider/model id; omit for the Pi profile default.
model         = "openai-codex/gpt-5.6-sol"
thinking      = "high" # off | minimal | low | medium | high | xhigh | max
# Exact active-tool replacement. Omit for Pi's profile; [] means no tools.
allowed_tools = ["read", "grep", "find", "ls", "bash"]
```

Values are normalized at dispatch (`normalize_sandbox_mode`,
`normalize_approval_policy`, `normalize_reasoning_effort`,
`normalize_codex_managed_context`, `normalize_codex_context_archive`,
`normalize_kimi_permission_mode`, `normalize_kimi_thinking`,
`normalize_pi_thinking`, `normalize_pi_allowed_tools`): unknown or empty
authority values fall back to the safe
default rather than silently escalating privileges (e.g. a typo'd Codex sandbox
becomes `workspace-write`, not `danger-full-access`; an unknown
`managed_context` becomes `vanilla`; an unknown `context_archive` becomes
`summary`).

### Selecting the backend with `--agent`

```bash
intendant --agent codex "refactor the auth module"
intendant --agent claude-code "add tests for the parser"
intendant --agent kimi "implement the parser tests"
intendant --agent pi "implement the parser tests"
```

`--agent <name>` parses via `AgentBackend::from_str_loose` and overrides
`default_backend` for that run; an unknown name is a hard config error.
`resolve_agent_backend_from_config()` applies the precedence: explicit flag → MCP
shared state (when driven over MCP) → config default → native.

## Gotchas and Caveats

- **No workspace config mutation.** Codex MCP injection is per-process:
  Intendant passes token-free `-c` overrides and a session-scoped bearer in the
  child environment to the app-server process. It does not write
  `<workspace>/.codex/config.toml`, create
  `config.toml.intendant-backup`, or restore files on shutdown.
- **Kimi bridge homes are supervisor state, not workspace state.** Kimi's
  generated MCP declaration and server-private files live below
  `KIMI_CODE_HOME/intendant-bridges`, never in the checkout and never in the
  primary `mcp.json`. A stable bridge is intentionally reused for the same
  Intendant session so resume keeps Kimi's server-side state addressable.
- **Settings latch at thread/process start.** Codex latches sandbox, approval
  policy, model, reasoning effort, tool set, and writable roots at `thread/start`.
  Changing these mid-session requires a teardown + respawn. The daemon's runtime
  config checks detect drift across tasks and force a rebuild when any latched
  field changes. Pi's active tool set is likewise process-start-only; model and
  thinking use live RPC controls.
- **Codex resume cwd is thread-stateful.** Intendant sends `cwd` on
  `thread/resume`, and then sends `thread/settings/update` with the requested
  project root for resumed Codex threads. A non-running Codex thread can load
  with that override, but a running app-server thread resumes from its loaded
  config snapshot and reports that effective cwd back to the client. Intendant
  logs a warning when Codex reports a different cwd than the requested project
  root, and logs later `thread/settings/updated` cwd notifications so harness
  runs do not silently display a requested root as if Codex had accepted it.
- **Per-session launch config beats global defaults.** Dashboard-created and
  dashboard-configured external sessions persist their binary command and
  backend-specific launch fields, including launch-time Codex model and
  reasoning-effort pins. Both
  resume paths — daemon resume/attach and
  CLI `--resume` — rehydrate that persisted per-session config with the same
  precedence: explicit overrides (dashboard launch options or CLI flags), then
  the persisted per-session config, then the global Settings pane /
  `intendant.toml`. This keeps old sessions from silently adopting a new global
  Codex binary or managed-context mode after a daemon restart.
- **Managed historical edits are branches.** Once a managed rewind has replaced
  old rollout context with a dense primer, the old user-turn number may no longer
  exist in the active Codex thread. Editing or jumping to that overwritten message
  must fork from the closest saved pre-rewind rollout containing the clicked
  message, then roll the fork back to the selected turn. Do not send stale visible
  turn numbers directly to the compacted active thread.
- **Load-bearing fallback error strings.** Several trait methods return a *typed
  error* by default (`steer_turn`, `rollback_turns`, `interrupt_turn`,
  `thread_action`). `drain_external_agent_events` distinguishes "feature
  unsupported by this backend" from "feature attempted but failed" partly by these
  error messages — a backend without native steering returns the
  unsupported error, and the caller falls back to **queueing** the text onto the
  context-injection queue for delivery at the next turn. Codex, Claude Code,
  Kimi, and Pi now steer through their adapter paths (Claude's behavior remains
  version-sensitive and delivery is inferred at its next checkpoint). Don't
  reword those strings without checking the drain logic.
- **Only turn-implying events wake an idle session into the observe drain.**
  While idle, messages/reasoning/tool/plan/diff/turn events are treated as a
  backend-initiated turn and drained; ambient events — stderr `Log` lines,
  `Usage` snapshots, out-of-turn `BackendError`s — are recorded inline and the
  loop stays idle. The distinction is load-bearing: entering the drain on an
  ambient event wedges the session (no real turn ⇒ no terminal event ⇒ queued
  follow-ups never picked up again; codex-cli 0.142's connector stderr made
  this deterministic on every resume). Classify new `AgentEvent` variants
  deliberately.
- **Interrupt twice to escape a wedged drain.** The first interrupt forwards
  `interrupt_turn()` (time-bounded so an unresponsive backend can't freeze the
  drain's select loop) and keeps waiting for the backend's terminal event; a
  second interrupt while one is pending force-returns the drain to idle, where
  queued follow-ups flow again. Stop Session also exits a drain immediately.
- **Resume tokens are addressable from spawn.** A non-fork external resume
  registers `resume_token → wrapper` as a session alias in the same lock as
  registration, so concurrent resumes of one thread dedupe against the
  in-flight wrapper (no duplicate app-servers on one rollout) and follow-ups
  targeted at the thread id during the attach window queue into the wrapper's
  channel instead of failing "not managed by this daemon". The attach-dedupe
  keys are held until the attach completes or provably fails (30s ceiling).
- **Follow-up routing is logged on both sides of the channel.** The
  supervisor prints `[supervisor] FollowUp <id> queued …` to the daemon log
  when it enqueues; the session loop writes `Follow-up <id> delivered` to the
  session log when it picks the message up. A queued line without a matching
  delivered line means the session loop stopped draining its queue.
- **`--direct` does not bypass external mode.** It only forces single-agent
  execution of the *native* worker. If a backend is configured, the supervised CLI
  still runs.
- **MCP/ctl reachability needs the gateway.** The injected `intendant` MCP server is
  MCP-over-HTTP at `http://localhost:<web_port>/mcp`. The external tool can only
  reach Intendant's display/CU tools while the gateway is up; without a resolved
  `web_port`, the MCP entry still points at the default port but nothing
  answers. Pi uses the same gateway authority through `$INTENDANT ctl`, not MCP.
- **The external tool brings its own keys.** Intendant supervises the process but
  the coding CLI authenticates to its own provider with its own credentials —
  Intendant's `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` are for the
  native agent and presence layer, not the supervised tool.

## See Also

- [Agent Execution & Multi-Agent Orchestration](./multi-agent.md) — the execution
  shapes and native sub-agent orchestration.
- [MCP Server](./mcp-server.md) — the control surface the external tool's MCP
  client connects back to.
- [Control plane & daemon](./control-plane-and-daemon.md) — running and supervising
  multiple sessions (native and external) from one daemon.
- [Configuration](./configuration.md) — the full `intendant.toml` reference.
