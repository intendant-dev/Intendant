# Architecture

## Overview

Intendant ships three binaries: a sandboxed **runtime** that executes commands,
a **controller** that drives it via AI model APIs, and **intendant-connect**, the
hosted/self-hostable rendezvous service. The runtime/controller split remains
the security boundary. What has grown is the controller: it is no longer a
single agent loop with a TUI bolted on. It is a multi-session, multi-backend
orchestration host built around a shared **EventBus**, a single-writer **control
plane**, and a long-lived **session supervisor** that owns the lifecycle of
every session launched at runtime.

```
                              ┌──────────────────────────────────────────────┐
   stdin (JSON commands)      │            intendant  (controller)            │
        │                     │                                               │
        ▼                     │   Frontends (display-only: render + emit       │
┌───────────────────┐         │   intents; never write shared state)           │
│  (sandboxed exec) │  Agent  │     ├─ Web dashboard  ─┐ ControlMsg            │
│                   │  Input  │     ├─ MCP server      ─┤  (intents)            │
│  - OS sandbox     │  (JSON) │     └─ Control socket  ─┘     │                 │
│  - no API keys    │────────▶│                                ▼                 │
│  - exec/edit/PTY  │ results │            ┌──────────────────────────────┐    │
│  - screenshot     │         │            │          EventBus            │    │
│  - in-mem proc map│         │            │  broadcast::channel<AppEvent>│    │
└───────────────────┘         │            │  (ControlMsg ⊂ AppEvent)     │    │
        │                     │            └──────────────────────────────┘    │
        ▼                     │             │            │             │        │
$INTENDANT_LOG_DIR/           │             ▼            ▼             ▼        │
 (per-session dir:            │      ┌────────────┐ ┌──────────┐ ┌──────────┐  │
  session.jsonl, turns/,      │      │  Control   │ │ Session  │ │   Task   │  │
  <nonce>_stdout.log, …)      │      │   Plane    │ │Supervisor│ │ Dispatch │  │
                              │      │(single     │ │(owns     │ │(presence/│  │
                              │      │ writer of  │ │ session  │ │ task/    │  │
                              │      │ shared     │ │ graph +  │ │ follow-up│  │
                              │      │ state)     │ │ lifecycle│ │ routing) │  │
                              │      └────────────┘ └──────────┘ └──────────┘  │
                              │                            │                    │
                              │   Per-session agent loops (execution shapes):  │
                              │   Direct · Orchestrate · Sub-Agent · External  │
                              │                            │                    │
                              │   Cross-cutting subsystems:                     │
                              │     Presence layer · WebRTC display · Live audio │
                              │     · Phone (SIP) · File watcher (rewind) ·      │
                              │     Knowledge store · Peer federation (A2A) ·    │
                              │     Cost accounting · Session logging            │
        ┌─────────────────────┴───────────────────────────────────────────────┘
        ▼ model APIs (OpenAI Responses · Anthropic Messages · Gemini)  ── streaming SSE
```

Two facts about this diagram drive everything below:

1. **Frontends are display-only.** The web dashboard, MCP server, and
   control socket all *render* state and *emit intents* (`ControlMsg`) onto the
   EventBus. None of them mutate shared state directly. The single writer is the
   [control plane](./control-plane-and-daemon.md).
2. **The EventBus is the spine.** It is one `tokio::sync::broadcast` channel
   (`event.rs`, `EventBus`) carrying `AppEvent`. `ControlMsg` intents travel as
   `AppEvent::ControlCommand`. Every long-lived subsystem subscribes to the bus;
   adding a frontend or a backend means adding a subscriber, not rewiring the
   others.

## Security Model

The runtime/controller split is a deliberate security boundary:

- **intendant-runtime** executes arbitrary shell commands but runs under
  OS filesystem restrictions (Landlock on Linux, Seatbelt on macOS, restricted
  tokens on Windows) and **never holds API keys**. It
  reads JSON commands from stdin, executes them sequentially, and writes results
  to stdout.
- **intendant** (the controller) holds API keys and manages model conversations
  but **never executes user-requested shell commands directly** — it pipes them
  to the runtime subprocess.
- **intendant-connect** is the hosted rendezvous/account metadata service. It is
  outside the runtime/controller command-execution boundary and holds no daemon
  API keys or runtime authority.

A compromised model conversation therefore cannot reach API keys, and the
runtime process cannot exfiltrate data through a model API. See
[Runtime Protocol](./runtime-protocol.md) for the wire format and
[Autonomy & Approvals](./autonomy.md) plus [Configuration](./configuration.md) for the
layered approval system that gates what the runtime is even asked to do.

## Runtime: Process State and Execution Model

The runtime keeps an in-memory `HashMap<u64, ProcessInfo>` keyed by command
*nonce* (PID, status, exit code, timestamp). It is ephemeral — it does not
survive a runtime restart, and each runtime invocation starts with an empty map.

Commands are processed **sequentially**. Each blocks until completion and
returns its result directly (exit code, stdout tail, stderr tail). The runtime
exits after processing the batch. Daemons backgrounded in a shell continue after
the tool returns. Per-nonce stdout/stderr go to `<nonce>_stdout.log` /
`<nonce>_stderr.log` inside the session directory the controller passes via
`INTENDANT_LOG_DIR`.

## Execution Shapes

The controller runs every native session through **one in-process loop**
(`run_direct_mode`); what used to be separate process modes are now
configurations of that loop, plus external-agent supervision. The February-era
subprocess pipeline (User mode's `run_user_mode` monitor and `INTENDANT_ROLE`
child processes with progress/result files) is gone.

### Direct (`run_direct_mode`)

Single in-process agent loop driving Intendant's own provider abstraction
(OpenAI / Anthropic / Gemini). Selected for simple tasks, forced with `--direct`,
chosen automatically when a task looks simple (`is_simple_task`), and always
used by non-daemon CLI paths. Budget-aware: stops at context exhaustion, an
explicit `done` signal, or a 500-turn safety cap (`SAFETY_CAP`). This is the
loop documented step-by-step below.

### Orchestrate (`run_direct_mode` with the orchestration prompt)

Selected for complex tasks under the daemon without `--direct`. The same loop
runs with `SysPrompt_orchestrator.md` appended; it decomposes the task and
delegates through the `spawn_sub_agent` / `wait_sub_agents` tools. Every
supervised native session carries those tools — orchestration is a capability,
not a mode; the shape only changes the prompt. Full detail in
[Multi-Agent Orchestration](./multi-agent.md).

### Sub-Agent (a supervised child session)

Spawned by another session's `spawn_sub_agent` call
(`SessionSupervisor::start_sub_agent_session`). The child is a full managed
session — dashboard row, approvals, steering, lineage link to its parent — with
a role prompt (`SysPrompt_research.md`, `SysPrompt_implementation.md`, …),
optionally isolated in a git worktree. It reports back with the
`submit_result` tool and ends when its task ends. Full detail in
[Multi-Agent Orchestration](./multi-agent.md).

### External-Agent Mode (`run_external_agent_mode`)

Selected with `--agent <backend>` or when an external backend is configured
(including `backend` on `spawn_sub_agent`). Instead of running Intendant's own
loop, the controller spawns and supervises an external coding CLI as a
subordinate worker (`external_agent::AgentBackend`): `Codex` or `ClaudeCode`.
Intendant translates its task, approval, and attachment surface onto each
backend's native protocol (Codex app-server JSON-RPC, Claude Code stream-json)
and surfaces their events back onto the EventBus so every frontend renders
them identically. This is a master/worker relationship — see
[External-Agent Orchestration](./external-agent-orchestration.md).

> **Peer federation is orthogonal to all of these.** The `peer/` module federates
> with *other* autonomous daemons (other Intendants, A2A-speaking peers,
> MCP-shaped peers) as equals, where `external_agent` supervises a *subordinate*
> CLI. The two compose: a peer Intendant can itself supervise a Codex subprocess
> while being driven from this side as a peer. Federation is in progress.

## The Control Plane, Session Supervisor, and Daemon

These three pieces are the architectural shift the rest of the docs build on, so
they get their own chapter:
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md).

In brief:

- **Control plane** (`control_plane.rs`) is the *single writer* of shared mutable
  state: autonomy level, the active external-agent backend, and the runtime
  external-agent configuration. It subscribes to the bus and is the only place
  `ControlMsg` mutations land, so a setting changed from the dashboard,
  MCP, or the control socket takes effect identically (and persists to `intendant.toml` where
  relevant).
- **Session supervisor** (`session_supervisor.rs`) is the long-lived owner of
  every session launched at runtime. It handles `CreateSession`, `StartTask`,
  `ResumeSession`, and targeted follow-ups off the bus, creates per-session
  resources (log dir, approval registry, follow-up channel), and tracks the
  parent/child/related-session graph plus the active session.
- **Task dispatch** (`task_dispatch.rs`) routes a task to the right channel —
  presence, task envelope, or follow-up — replacing the dispatch logic that used
  to live in the TUI.
- An **idle `--web` launch starts a headless daemon** (`run_daemon_loop`,
  gated by `should_start_idle_web_daemon`): the supervisor owns all
  launches, and tasks arrive over WebSocket/control-socket.
- **Not yet built:** there is no recurring/scheduled-task facility. The only
  scheduling primitive is the one-shot `ScheduleControllerRestart`
  (`event.rs` / `mcp.rs`).

## How It Works (Direct Mode loop)

The Direct-Mode loop is the canonical agent loop; the other modes wrap or
delegate it. Verified against `main.rs`:

1. Loads `.env` and selects the provider. OpenAI uses the Responses API
   (`/v1/responses`), Anthropic the Messages API, Gemini `generateContent`. All
   three stream via SSE.
2. Configures structured output, reasoning controls, native tool calling,
   prompt caching, and max output tokens from model capabilities and env vars.
3. Detects the project root (`git rev-parse --show-toplevel`, falling back to
   cwd).
4. Resolves the role-appropriate system prompt via a cascade: project root →
   `~/.config/intendant/` → compiled-in default. With native tools enabled it
   uses the condensed `SysPrompt_tools.md` (tool docs live in the API tool
   definitions, not prose).
5. Injects the project working directory so the model knows where to work.
6. Loads tagged knowledge from the project memory store and injects it.
7. Loads `INTENDANT.md` project instructions (global then project-local) and
   injects them.
8. Logs the full messages array to `turns/turn_NNN_messages.json` before each
   API call.
9. Sends the task via `chat_stream()` with `max_tokens`/`max_output_tokens`,
   optional reasoning, optional JSON format, and native tool definitions.
   Requests use exponential-backoff retry (up to 5 attempts) for 429 and 5xx.
   Text deltas stream to the frontends in real time.
10. Logs reasoning content (summary + full text) to `turns/turn_NNN_reasoning.txt`
    when the provider returns it.
11. Processes the response on one of two paths:
    - **Native tool-call path**: collects tool calls, assembles an `AgentInput`
      batch, pipes it to the runtime, maps results back per tool call. Handles
      `manage_context` / `signal_done` caller-side. Raw API output items
      (reasoning + function_call) are preserved for verbatim echo-back.
    - **Legacy text-extraction path** (fallback): extracts JSON from the
      response text (structured output, code fences, or bare JSON) and checks
      for an explicit `{"done": true}` signal.
12. Applies context directives (`drop_turns`, `summarize`).
13. Injects project context into relevant commands.
14. Classifies each command by action category (file read/write/delete, exec,
    network, destructive, display control, live audio, human input) and checks
    autonomy rules.
15. If approval is required: interactive frontends (web/MCP via the EventBus)
    surface an approval request and wait; headless mode denies (no implicit
    auto-approve).
16. Pipes the JSON to `intendant-runtime` and waits with a hard timeout (120s
    default). `askHuman` batches disable that normal timeout because the runtime
    polls indefinitely for the response file.
17. Feeds output back as the next user message (text path) or as individual tool
    results (tool-call path), appending a token-budget summary.
18. Repeats until done, no JSON / no commands, the budget is exhausted, or the
    safety cap is hit.
19. In headless mode, if the model emits `askHuman`, the loop sends a recovery
    prompt ("continue with explicit assumptions") instead of blocking on the
    human-input timeout.

## Frontend Vocabulary

`ControlMsg` and `AppEvent` are the shared vocabulary across frontends. The
web dashboard, MCP server, and control socket render `AppEvent` state and send
`ControlMsg` intents through the EventBus; the control plane and session
supervisor are the state writers. The MCP surface additionally maps its
approval/input tools to `UserAction` and serializes some resource snapshots via
`StateResult` — `UserAction` is MCP-only vocabulary today.

There is no single compile-time exhaustiveness guarantee across all frontends.
Rust exhaustive matching protects each local handler, and cross-frontend parity
is maintained by routing new capabilities through the `ControlMsg`/`AppEvent`
surface.

## askHuman Behavior

- Under the **dashboard**, `askHuman` surfaces as a question card; the answer
  is written to the session-scoped response file (`--json` accepts an `input`
  command for the same file).
- In **headless mode** (no dashboard, non-interactive stdin), `askHuman` cannot
  be answered interactively, so the loop tells the model to continue with
  explicit assumptions rather than wait.
- In interactive paths, `askHuman` has no effective timeout: the controller
  disables the normal command timeout and the runtime polls indefinitely for the
  response file.

## Streaming

All three providers stream via `chat_stream()` on the `ChatProvider` trait:

- **Anthropic**: `stream: true` on Messages; parses `content_block_delta`,
  `content_block_start/stop`, `message_delta`.
- **OpenAI**: `stream: true` on Responses; parses `response.output_text.delta`,
  `response.function_call_arguments.delta`, `response.completed`.
- **Gemini**: `streamGenerateContent?alt=sse`; parses chunked JSON candidates.

Text deltas forward to frontends via `AppEvent::ModelResponseDelta` and
accumulate in a streaming buffer that clears when the full `ModelResponse`
arrives.

## Rate-Limit Retry

API requests use `send_with_retry()` with exponential backoff
(`1s × 2^attempt + jitter`, up to 5 retries) for HTTP 429 and 5xx. Non-retryable
errors (400, 401, …) fail immediately. API keys in error messages are masked via
`mask_api_keys()`.

## Prompt Caching

- **Anthropic**: `anthropic-beta: prompt-caching-*` header with structured system
  content carrying `cache_control: {"type": "ephemeral"}`.
- **OpenAI**: automatic server-side caching for prompts over ~1024 tokens (no API
  changes).
- **Gemini**: implicit context caching (no API changes).

## Auto-Compaction

When context usage reaches 90% (`usage_fraction() >= 0.90`),
`conversation.auto_compact()` triggers:

- **Keeps**: the system message, the first 2 context messages (working directory
  + ack), and the last 4 messages.
- **Summarizes**: all messages between the system/context prefix and the last
  four messages via `summarize_turns()`.
- Emits a `ContextManagement` event to the frontends.

Sub-agents and orchestrators additionally checkpoint structured state to the
knowledge store so essential context survives the compaction boundary (see
[Multi-Agent Orchestration](./multi-agent.md)).

## Project Status and Direction

The original eight-step arc (CLI → TUI → web → voice → desktop/computer-use →
WebRTC display → phone → persistent daemon) is complete through step 8, with one
explicit gap: the persistent daemon exists but **scheduled / recurring tasks do
not** (only one-shot controller restarts). The dominant current direction is the
multi-session, multi-backend orchestration hub described in this chapter —
parallel local and external-agent sessions, a session graph, and rewindable
history. Windows is a first-class target (see
[Windows Support](./windows-support.md)); peer federation (A2A) is in progress.

## Environment

- **OS:** macOS, Linux (Debian 12+), or Windows (`x86_64-pc-windows-msvc`). See
  [Windows Support](./windows-support.md).
- **Runtime:** Tokio async (full features).
- **Permissions:** unprivileged user with passwordless sudo (Linux).
- **Display:** auto-managed Xvfb (Linux), native display (macOS), GDI/DXGI
  capture (Windows). See [Display Pipeline](./display-pipeline.md).
- **X11 auth:** at startup the runtime discovers active X displays and merges
  their xauth cookies into a session-scoped `session.Xauthority`, passed as
  `XAUTHORITY` to spawned commands.

## Where to Go Next

- [Control Plane & Persistent Daemon](./control-plane-and-daemon.md) — the
  single-writer control plane, session supervisor, file-watcher rewind, headless
  daemon, and cost accounting.
- [Session Logging](./session-logging.md) — the on-disk session layout, JSONL
  event format, replay/rehydration, and cross-backend naming.
- [Multi-Agent Orchestration](./multi-agent.md) — orchestration sessions,
  supervised sub-agents, worktrees, and external-agent supervision.
- [Presence Layer](./presence.md), [Web Dashboard](./web-dashboard.md),
  [MCP Server](./mcp-server.md), [Display Pipeline](./display-pipeline.md),
  [Computer Use & Live Audio](./computer-use-and-audio.md).
