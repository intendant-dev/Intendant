# External-Agent Orchestration

Intendant can hand a whole task to a third-party coding CLI — **OpenAI Codex** or
**Claude Code** — and supervise it as a subordinate worker. The
external tool does the actual coding; Intendant wraps it in its own oversight,
display, and computer-use surface by pointing the tool's MCP client at Intendant's
own [MCP server](./mcp-server.md).

This is the fourth execution mode (alongside Direct, User, and Sub-Agent — see
[Agent Execution & Multi-Agent Orchestration](./multi-agent.md)). It is selected
by `--agent <backend>` or the `[agent] default_backend` config key.

## Why

These CLIs are excellent autonomous coders but live in their own terminals, with
their own approval prompts, no shared display, and no voice/phone reach. Wrapping
one in Intendant gives you:

- **One oversight surface.** The supervised agent's command/file approval requests
  are lifted into Intendant's frontends (TUI, web dashboard, MCP, `--json`) and the
  same autonomy policy that governs the native agent.
- **Display & computer use.** Intendant injects an `intendant` MCP server into the
  external tool's config, so the coding agent can call Intendant's MCP tools —
  screenshots, computer use, etc. — over MCP-over-HTTP against the running gateway.
- **Presence & multi-session.** The supervised session is just another session on
  the [EventBus](./architecture.md); the [presence layer](./presence.md) narrates
  it and the daemon can run several alongside native agents
  (see [control plane & daemon](./control-plane-and-daemon.md)).

Crucially, external-agent control does **not** flow through the `UserAction` enum
that unifies the native frontends. It rides `ControlMsg` (inbound) and `AppEvent`
(outbound) on the EventBus (`event.rs`), because the verbs are backend-shaped
(steer a turn, fork a thread, roll back) rather than the native action set.

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
**`AgentEvent`**s; everything the backend emits (deltas, messages, reasoning, plan
updates, tool start/output/complete, approval requests, diffs, usage, termination)
is translated into that enum so the controller's display and oversight code is
backend-agnostic. `AgentEvent::Scoped { thread_id, turn_id, .. }` wraps inner
events when a backend (Codex) multiplexes several threads through one process.

`AgentConfig` carries the working dir, model, approval policy, the
**`web_port`** (used to generate the MCP-over-HTTP config), an optional
`resume_session` id, and the Codex-only knobs (`sandbox`, `reasoning_effort`,
`web_search`, `network_access`, `writable_roots`). Backends that don't model a
field ignore it.

The supported backend identities are the `AgentBackend` enum (`Codex`,
`ClaudeCode`). `from_str_loose()` accepts the canonical short forms plus older
Display forms (`codex`, `claude-code`/`claude_code`/`cc`, case-insensitive);
`as_short_str()` emits the canonical wire form that matches the dashboard
dropdown's `<option value>`.

Gemini CLI was previously supported as a backend and was retired in July 2026;
persisted sessions from it remain readable but cannot be resumed.

## Per-Backend Reference

`create_external_agent()` (`main.rs`) constructs the right adapter from
`[agent.<backend>]` config, then `run_external_agent_mode()` drives the supervise
loop.

| | **Codex** (reference impl) | **Claude Code** |
|---|---|---|
| Module | `external_agent/codex.rs` (~200 KB) | `external_agent/claude_code.rs` |
| Spawn command | `codex app-server` | `claude -p --output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio` |
| Wire protocol | JSON-RPC over JSONL (`app-server`) | stream-json over stdio |
| MCP injection | Per-process `-c mcp_servers.intendant.{type,url}` overrides plus scoped env; no workspace config file | Inline `--mcp-config '{…}'` JSON string |
| Multi-thread | Yes — many threads per process | No |
| Native thread id | Yes | Yes — announced via `AgentEvent::NativeSessionId` on the first turn (placeholder `claude-code-session` until then; `--resume` keeps the id stable so resumed threads are canonical immediately) |
| Mid-turn steer | Yes (`turn/steer`) | Yes — a user message written mid-turn is absorbed into the running turn |
| Mid-turn interrupt | Yes (`turn/interrupt`) | Yes (`control_request` `interrupt`; the process survives for follow-up turns) |
| Token usage / context meter | Yes | Yes (`message_delta` + `result` usage; context window from `modelUsage`) |
| Reasoning trace | Yes | Yes (`thinking` blocks) |
| Rollback turns | Yes (`thread/rollback`) | No → session reset |
| Fork / side threads / review / goals / compact / fast / memory-reset | Yes (`thread_action`) | `compact`, `fork` (respawns via `--resume <id> --fork-session`), the full `goal*` family (wrapper goal engine), and live `model` / `permission-mode` — all via universal `thread_actions`. No side/fast/review/memory-reset — see [Dashboard and Station parity](#dashboard-and-station-parity-codex-vs-claude-code) |
| Native sub-agents | Yes — collab tools spawn real attachable threads (`SubAgentToolCall`) | Yes — the in-band `Agent`/`Task` tool; async children stream `parent_tool_use_id`-tagged envelopes, surfaced as ephemeral `task-*` child sessions on the same `SubAgentToolCall`/relationship rail |

Both spawn through `crate::platform::spawn_command(&cfg.command)` with the
working dir set to the project root and stdin/stdout/stderr piped; stderr is
forwarded into the session activity log line-by-line
(`spawn_stderr_forwarder` in `external_agent/mod.rs`), so transport/auth
failures are visible from every frontend.

### Capability plumbing per backend

What each supervised backend actually receives:

| | **Codex** | **Claude Code** |
|---|---|---|
| MCP tool exposure | `tool_profile=core` (bootstrap set) | `tool_profile=core` (bootstrap set) |
| Loopback `mcp_token` in URL | yes | yes |
| `session_id` scope in URL | yes | yes |
| `$INTENDANT` + `INTENDANT_MCP_URL` env (`ctl` bootstrap) | yes (+ `INTENDANT_MANAGED_CONTEXT`) | yes |
| Guidance channel | managed-context developer message | first-prompt bootstrap addendum |

The bootstrap set for both Codex and Claude Code includes the CU path
(`read_screen`, `take_screenshot`, `execute_cu_actions`, `list_displays`,
`grant_user_display`, `revoke_user_display`) and the shared-view tools
regardless of managed context; managed-context/fission tools remain
managed-only.

### Codex (the reference backend)

Codex is the most fully wired backend; Claude Code falls back to defaults for
features it lacks.

- **MCP injection — per-process config.** Codex receives the Intendant MCP
  server exclusively through command-line `-c` overrides on the app-server
  process; Intendant does not write, back up, or restore
  `<workspace>/.codex/config.toml`. The command line includes
  `-c mcp_servers.intendant.type="http" -c mcp_servers.intendant.url="…"`, plus the
  user's toggles as further `-c` overrides:
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
  gets the same treatment (scoped URL with `tool_profile=core` + `mcp_token` +
  `session_id`, the `$INTENDANT`/`INTENDANT_MCP_URL`/`INTENDANT_SESSION_ID`
  env, and a first-prompt bootstrap addendum naming the bootstrap tools and
  the `ctl` discovery flow).

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
  intendant binary as `--web <port> --no-tui`, waits for HTTP readiness, and
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

- **Rich `thread_action` ops** (`codex.rs`): `compact`, `fast`, `fork`,
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
managed-context gate exposes a fission surface (`fission_tool()` in `mcp.rs`)
that lets the model split separable work into parallel **full-context sibling
branches** and join the results back deliberately. The spawn/import mechanics
live in `main.rs` (`apply_fission_spawn_action` / `apply_fission_import_action`),
the runtime contract in `fission_lifecycle.rs`, and the durable state in
`fission_ledger.rs`.

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
into each fork (`fission_charter_message`, `main.rs`) carrying identity
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
`codex.rs`). Any failed spawn step removes the worktree that branch created,
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
block carries a fission policy (`codex.rs`): prefer `fission_spawn` with a
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
Protocol details that are load-bearing (verified against Claude Code 2.1.200):

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
- **Steer**: a user message written while a turn runs is queued by the CLI
  and absorbed into the *running* turn (the model reads it between tool
  calls) — Intendant's `steer_turn` is exactly that write.
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
  native identity yet — expected, not a bug.
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
`--mcp-config` (not a file path); the URL is the scoped bootstrap endpoint
(`session_id` + `tool_profile=core` + `mcp_token`), and the child gets
`$INTENDANT`, `INTENDANT_MCP_URL`, and `INTENDANT_SESSION_ID` so
`"$INTENDANT" ctl ...` works from its shell. The first user message carries a
bootstrap addendum naming the MCP bootstrap tools
(`read_screen`/`take_screenshot`/`execute_cu_actions`, shared-view), the lazy
`ctl --help` discovery flow, and the dashboard-validation helper.
`--permission-mode` (normalized — the legacy Intendant value `auto` maps to
the CLI default) and `--allowedTools` are added from config when set.

## Dashboard and Station parity: Codex vs Claude Code

The per-session dashboard features (Activity → Logs agent windows and the
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

| Feature | Universal rail (exists today) | Codex producer | Claude Code today → plan |
|---|---|---|---|
| Steer / interrupt / stop affordances | `SessionCapabilities.{follow_up,steer,interrupt}`; the UI gates on capabilities, not backend type | emits all three | **Parity** (emits all three) |
| Usage / context meter | `AgentEvent::Usage` → `UsageSnapshot` / `ContextSnapshot` | `token_count` notifications | **Parity** (`message_delta` + `result` usage) |
| Goal chip in the agent-window header (`/goal`) | `SessionGoal` type; `AgentEvent::GoalUpdated/GoalCleared`; `session_goal` outbound + log replay; the window chip renderer is backend-neutral; goal wire conventions (statuses, budget shape, objective limit) shared via `external_agent` helpers | native `thread/goal/*` RPCs | **Live — wrapper goal engine in the adapter.** The full `goal*` op family is advertised and dispatched; goal state lives in `CcShared`, notices reach the model as mid-turn steers (absorbed) or as a prelude on the next prompt (idle updates never buy a turn), and budget spend is measured in FRESH tokens (uncached input + cache creation + output — cache reads excluded), flipping `active` → `budgetLimited` at exhaustion. Engine state is per-process: after a resume the chip rehydrates from the log but the engine starts empty (re-set the goal) |
| Per-window action menu (fork / compact / goals / …) | **Universal (landed):** `SessionCapabilities.thread_actions` op vocabulary + the `thread_action` control message (`codex_thread_action` stays a wire alias); the kebab and Station session actions render from the advertised op list, with the codex heuristic as legacy-replay fallback | full op set | **`compact` + `fork` live.** `compact` sends the native `/compact` user message (status → `compact_boundary` → free result); `fork` respawns via `ForkHandling::RespawnResume` → `ResumeSession { fork: true }` → `--resume <parent> --fork-session` (the child binds its own native id + the `fork` relationship on its first prompt). No Claude analog planned: side / fast / review / memory-reset |
| Relationship wiring (parent/sub/fork header chips + SVG wires; Station edges) | `session_relationship` event + lineage ledger + `/api` serving + both renderers — all backend-neutral | side / subagent / fork / fission / rewind emitters | **`fork` + `subagent` emitted.** Fork on the forked child's first identity announcement (persisted `forked_from` lineage); in-band Task sub-agents ride `SubAgentToolCall` → ephemeral `task-*` child sessions with `subagent` relationships (fission observations stay Codex-only by design) |
| Per-session persisted launch overlay | `SessionAgentConfig` + `ConfigureSessionAgent` / `Restart` (universal `agent_command` + backend fields, bundled as `LaunchOverrides`) | all `codex_*` fields | **Live.** `claude_model` / `claude_permission_mode` / `claude_allowed_tools` / `claude_effort` pins with inherit-vs-pin sentinels ("default" stays a pinnable permission mode; `all` pins explicitly-unrestricted tools), Launch-config modal rows, and LIVE apply of model + permission on save via the `model` / `permission-mode` thread actions (`set_model` / `set_permission_mode` control requests, verified on 2.1.201) |
| Global runtime config pane | `Set*` ControlMsgs + `*ConfigChanged` broadcast + Settings/Control panes | 12 knobs | **3 knobs** (model / permission mode / allowed tools) — by design; grows only when CC grows equivalent concepts |
| Station controls-panel runtime block | the controls panel renders per-backend blocks | approval policy / managed-context / fork-binary warning | **Live.** Model pills (default + the CLI's latest-version aliases fable/opus/sonnet/haiku, with a truthful `custom:` row for out-of-alias pins) and permission pills (default/edits/plan/bypass), gated `backend == "claude-code" \|\| launch_agent == "claude-code"` exactly like the Codex block, dispatching `set_claude_model` / `set_claude_permission_mode` (persisted to `intendant.toml` + broadcast, same as the dashboard Control pane) |
| Plan / todo display | `AgentEvent::PlanUpdate` exists | emits plan updates | **Missing.** Plan: translate `TodoWrite` tool inputs into `PlanUpdate` |
| Managed context / fission / rewind family | managed-context tools + ledgers | patched managed fork only | **Out of scope for parity** — Codex-fork-specific by design; Claude Code manages its own context (`/compact`, auto-compaction) |

Catch-up order (each step unlocks UI in both surfaces at once):

1. ~~universal `thread_actions` capability + a Claude `thread_action`
   implementation (`compact`, `fork`)~~ — **landed** (window kebab and
   Station session actions render from the advertised ops; e2e phases 6–8);
2. ~~the wrapper-level goal engine~~ — **landed for Claude Code** (adapter
   engine; the kebab goal submenu and `/goal` slash light up from the
   advertised ops). Still open: goals for NATIVE sessions and Station goal
   rendering (plumbed but unrendered);
3. ~~remaining relationship producers (in-band Task sub-agents)~~ —
   **landed** (async `Agent`-tool children become ephemeral `task-*` child
   sessions with scoped transcripts; `fork` already wired);
4. ~~per-session Claude overlay fields + Launch-config modal rows + live
   apply~~ — **landed** (drafted by an unattended session, adopted after
   review, finished with the modal UI and live model/permission apply);
5. ~~the Station controls Claude block~~ — **landed** (model + permission
   pill rows in the rendered controls panel).

All five catch-up items have landed; what remains on the Claude side of
the matrix is the `TodoWrite` → `PlanUpdate` translation and goals for
native sessions, plus the Station Phase B rendering work tracked in
[station.md](./station.md).

## Approval Routing

When a supervised agent asks to run a command or change a file, the backend emits
`AgentEvent::ApprovalRequest` / `FileApprovalRequest`. `drain_external_agent_events()`
(`main.rs`) routes the decision through **the same autonomy policy and approval
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
frontend that already renders native approvals — the TUI gate, the web dashboard,
the MCP `approve`/`deny` tools, and `--json` stdin — handles external-agent
approvals identically. `ApprovalDecision` (re-exported from `crate::approval`) is
the shared decision vocabulary; `AcceptForSession` is how "approve all" sticks for
the rest of the session. Note that `--web` providing a `web_port` is what keeps an
otherwise-headless run from auto-denying: it signals that an interactive frontend
exists.

## Configuration

External-agent settings live under `[agent]` in `intendant.toml`
(`ExternalAgentConfig` in `project.rs`). `default_backend` selects the mode; the
per-backend subtables tune each tool. All keys have defaults, so a bare `[agent]`
with just `default_backend` works.

```toml
[agent]
# Which backend to use when --agent is not passed. Omit/empty = native agent.
# Accepts: "codex", "claude-code".
default_backend = "codex"

[agent.codex]
command          = "codex"            # binary on PATH or absolute path
model            = "gpt-5-codex"      # optional; omit to use Codex's default
approval_policy  = "on-request"       # untrusted | on-request | never
sandbox          = "workspace-write"  # read-only | workspace-write | danger-full-access
reasoning_effort = "medium"           # ""(default) | minimal | low | medium | high | xhigh
service_tier     = ""                 # ""(inherit Codex default) | priority (Fast) | flex | standard (explicit opt-out sentinel)
web_search       = false              # enable the Responses web_search tool
network_access   = false              # outbound net inside workspace-write only
writable_roots   = []                 # extra writable dirs (absolute), each → -c writable_roots
managed_context = "vanilla"          # vanilla | managed
context_archive = "summary"          # summary | exact | off — context snapshot archive mode ("Context replay" in the UI)

[agent.claude_code]
command         = "claude"
model           = "claude-sonnet-4-6"  # optional; any claude CLI --model value (e.g. "haiku")
permission_mode = "default"           # default | acceptEdits | plan | bypassPermissions (legacy "auto" = default)
allowed_tools   = []                  # e.g. ["Read", "Edit", "Bash"]; empty = all
```

Values are normalized at dispatch (`normalize_sandbox_mode`,
`normalize_approval_policy`, `normalize_reasoning_effort`,
`normalize_codex_managed_context`, `normalize_codex_context_archive`): unknown
or empty values fall back to the safe
default rather than silently escalating privileges (e.g. a typo'd Codex sandbox
becomes `workspace-write`, not `danger-full-access`; an unknown
`managed_context` becomes `vanilla`; an unknown `context_archive` becomes
`summary`).

### Selecting the backend with `--agent`

```bash
intendant --agent codex "refactor the auth module"
intendant --agent claude-code "add tests for the parser"
```

`--agent <name>` parses via `AgentBackend::from_str_loose` and overrides
`default_backend` for that run; an unknown name is a hard config error.
`resolve_agent_backend_from_config()` applies the precedence: explicit flag → MCP
shared state (when driven over MCP) → config default → native.

## Gotchas and Caveats

- **No workspace config mutation.** Codex MCP injection is per-process:
  Intendant passes `-c` overrides and scoped env to the app-server process. It
  does not write `<workspace>/.codex/config.toml`, create
  `config.toml.intendant-backup`, or restore files on shutdown.
- **Settings latch at thread/process start.** Codex latches sandbox, approval
  policy, model, reasoning effort, tool set, and writable roots at `thread/start`.
  Changing these mid-session requires a teardown + respawn. The daemon's runtime
  config checks detect drift across tasks and force a rebuild when any latched
  field changes.
- **Codex resume cwd is thread-stateful.** Intendant sends `cwd` on
  `thread/resume`, and then sends `thread/settings/update` with the requested
  project root for resumed Codex threads. A non-running Codex thread can load
  with that override, but a running app-server thread resumes from its loaded
  config snapshot and reports that effective cwd back to the client. Intendant
  logs a warning when Codex reports a different cwd than the requested project
  root, and logs later `thread/settings/updated` cwd notifications so harness
  runs do not silently display a requested root as if Codex had accepted it.
- **Per-session launch config beats global defaults.** Dashboard-created and
  dashboard-configured external sessions persist their binary command and, for
  Codex, `managed_context` mode. Both resume paths — daemon resume/attach and
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
  context-injection queue for delivery at the next turn. Both current backends
  (Codex, Claude Code) steer natively, so the fallback is dormant — but it is
  the contract any future backend inherits. Don't reword those strings without
  checking the drain logic.
- **`--direct` does not bypass external mode.** It only forces single-agent
  execution of the *native* worker. If a backend is configured, the supervised CLI
  still runs.
- **MCP reachability needs the gateway.** The injected `intendant` MCP server is
  MCP-over-HTTP at `http://localhost:<web_port>/mcp`. The external tool can only
  reach Intendant's display/CU tools while the gateway is up; without a resolved
  `web_port`, the MCP entry still points at the default port but nothing answers.
- **The external tool brings its own keys.** Intendant supervises the process but
  the coding CLI authenticates to its own provider with its own credentials —
  Intendant's `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` are for the
  native agent and presence layer, not the supervised tool.

## See Also

- [Agent Execution & Multi-Agent Orchestration](./multi-agent.md) — the four modes
  and native sub-agent orchestration.
- [MCP Server](./mcp-server.md) — the control surface the external tool's MCP
  client connects back to.
- [Control plane & daemon](./control-plane-and-daemon.md) — running and supervising
  multiple sessions (native and external) from one daemon.
- [Configuration](./configuration.md) — the full `intendant.toml` reference.
