# Agent Execution & Multi-Agent Orchestration

Intendant runs a task one of three ways. The simplest is a single native agent
loop; the richest is an orchestration session that decomposes a task and
delegates pieces to sub-agents, each a fully supervised session of its own. The
third hands the whole task to a third-party coding CLI (Codex or Claude Code)
and supervises it — that path has its own chapter,
[External-Agent Orchestration](./external-agent-orchestration.md).

This chapter covers Intendant's *native* execution: how a session's shape is
chosen, and the sub-agent machinery (the spawn/wait/submit tools, supervised
child sessions, worktrees, role prompts, and knowledge routing).

Historically these were separate *process* modes — orchestration ran as a
subprocess pipeline (`run_user_mode` → `INTENDANT_ROLE=…` child processes →
result files polled from disk). That February-era pipeline is gone. Everything
below runs in-process on the session supervisor: one substrate for direct
sessions, orchestration sessions, sub-agents, and external agents alike.

## Session shapes

| Shape | Selected by | What runs | Source entry point |
|-------|-------------|-----------|--------------------|
| **Direct** | `--direct`, a "simple" task heuristic, or any non-daemon CLI path | One supervised agent loop, no delegation | `run_direct_mode` (`main.rs`) |
| **Orchestrate** | Default for non-trivial tasks under the daemon (no `--direct`); explicit `orchestrate` flag on task submission | The same loop with the orchestration prompt; delegates via `spawn_sub_agent` | `run_direct_mode` with `SubAgentRole::Orchestrator` |
| **Sub-Agent** | Spawned by another session's `spawn_sub_agent` tool call | A supervised child session with a role prompt; reports back with `submit_result` | `SessionSupervisor::start_sub_agent_session` (`session_supervisor/sub_agents.rs`) |
| **External-Agent** | `--agent <backend>` or `[agent] default_backend`, or `backend` on `spawn_sub_agent` | A supervised third-party coding CLI wired to Intendant's MCP server | `run_external_agent_mode` (`main.rs`) — see [External-Agent Orchestration](./external-agent-orchestration.md) |

These are configurations of one thing, not modes of different things: every
native session is `run_direct_mode` with a `NativeSessionConfig` (role,
optional prompt override, optional sub-agent identity, and — under the daemon —
the orchestration handle that enables the spawn tools).

### How the shape is chosen

- **External-Agent** is resolved by `resolve_agent_backend_from_config()`: an
  explicit `--agent` flag wins, otherwise the `[agent] default_backend` TOML
  key. If neither names a backend, native execution is used.
- **Direct vs. Orchestrate** (daemon sessions): an explicit `direct` /
  `orchestrate` flag on the task submission wins. Otherwise the
  `is_simple_task()` heuristic decides — a task of three lines or fewer that
  contains none of the "complex" keywords (`research`, `investigate`,
  `implement`, `build`, `refactor`, `migrate`, `deploy`, `set up`, `analyze`,
  `compare`, `design`, `create a`) runs Direct; anything else gets the
  orchestration prompt.
- **Non-daemon CLI paths** (headless `--no-web`, standalone
  `--mcp`) always run Direct: sub-agent spawning requires the daemon's session
  supervisor, so the orchestration prompt would describe tools that cannot
  work there.

Orchestration is a **capability, not a role**: every supervised native session
carries the spawn tools, whatever prompt it runs. The orchestrate shape just
adds the prompt section that teaches decomposition and delegation
(`SysPrompt_orchestrator.md`).

## Sub-agents are supervised sessions

`spawn_sub_agent` starts the child through the same session supervisor that
owns every dashboard session:

```
Orchestration session (or any supervised native session)
    │  spawn_sub_agent { task, role?, system_prompt?, backend?,
    │                    worktree?, name? }
    ▼
SessionSupervisor::start_sub_agent_session
    ├─ enforces [orchestrator] max_parallel_agents (per parent, default 4)
    ├─ optional git worktree branched off the parent project's HEAD
    ├─ records the parent link ("subagent" relationship — the same kind
    │  Codex-spawned children use, so the dashboard renders both alike)
    └─ spawns the child session: internal loop, or codex / claude-code
    ▼
Child session — its own dashboard row, live activity, approvals under the
daemon's autonomy, steerable, stoppable, lineage-tracked
    │  submit_result { status, summary, brief?, findings?, artifacts? }
    │  … then signal_done
    ▼
Parent's wait_sub_agents call returns the structured results
```

- **Spawning is non-blocking**: `spawn_sub_agent` returns the child's session
  id immediately. Independent sub-tasks run in parallel.
- **Delegation is bounded two ways**: width by
  `[orchestrator] max_parallel_agents` (concurrently running children per
  parent, default 4), and depth by a fixed cap two levels below the root —
  a root session can spawn workers, and those workers can delegate once
  more; deeper spawns are refused with instructions to do the work
  directly. Children are also told in their system prompt not to
  re-delegate their own task.
- **Collection is explicit**: `wait_sub_agents` blocks until the requested
  children finish (`mode: "all"`, default), the first finishes
  (`mode: "any"`), or `timeout_secs` lapses — then returns each finished
  child's result and lists what is still running. The wait honors user
  interrupts and session stop.
- **Results are structured**: a child reports with `submit_result`
  (status/summary/brief/findings/artifacts — the `SubAgentResult` shape in
  `sub_agent.rs`). A child that finishes without submitting gets a result
  synthesized from its final message and exit state; usage always comes from
  session accounting, not self-report.
- **Backends compose**: `backend: "codex"` / `"claude-code"` runs the child as
  a supervised external agent instead of the internal loop. The
  orchestrator/worker matrix is fully general — native conducting Codex
  workers, or (via the MCP `start_task` tool external agents already have)
  Codex conducting native specialists.
- **Lifecycle is tied to the parent**: children die with their parent, like
  Codex subagent threads. A native child is also a managed session in its own
  right — it can be stopped, interrupted, or steered directly from the
  dashboard (related *backend threads* inside a parent process, e.g. Codex
  subagent threads, still route through their parent).

A sub-agent always runs headless (no interactive frontend of its own) with no
MCP client, under the daemon's shared autonomy — approvals it raises land in
the dashboard like any other session's.

## Delegating from the dashboard

Both halves of the delegation surface are exposed in the web dashboard:

- **Execution shape at launch**: the Sessions tab's *New Session* pane has an
  **Execution** control (internal agent only) — *Auto* leaves the choice to
  the `is_simple_task` heuristic, *Orchestrate* / *Direct* set the
  corresponding flag on `create_session`. An explicit choice beats the global
  *Direct* header toggle. Station's launch composer carries the same
  three-state control as pills (*auto* / *orch* / *direct*), hidden when an
  external agent is selected.
- **Delegate on a live session**: every internal session's window menu has
  **Delegate…** — task, optional name, role, backend (internal / Codex /
  Claude Code), and worktree isolation. It sends
  `ControlMsg::SpawnSubAgent { session_id, task, name?, role?, agent?,
  worktree? }`, and the supervisor (`delegate_sub_agent`) runs the same
  `start_sub_agent_session` path the tool uses: same relationship kind, same
  width and depth caps (the parent's depth is tracked on its supervisor
  entry).

A dashboard-delegated child is indistinguishable from a model-spawned one on
the parent side: the supervisor retains each native session's children
registry (shared with the loop's orchestration handle), inserts the child
there, and wakes the parent with a notification follow-up naming the child —
so the model knows the delegation happened and collects the result with
`wait_sub_agents` exactly like one of its own spawns. External-agent sessions
are refused with a pointer to send them a follow-up instead — they delegate
through their own injected `start_task` tool.

## Agent Roles

Roles are the `SubAgentRole` enum in `sub_agent.rs`:
`Research`, `Implementation`, `Testing`, `Orchestrator`, `LiveAudio`, and
`Custom(String)`. On `spawn_sub_agent`, the `role` string picks the child's
prompt preset; any unrecognized string becomes `Custom` (base prompt only).

Prompt resolution (`prompts.rs::resolve_system_prompt[_for_tools]`) always
loads the base prompt first, then **appends** a role-specific prompt for three
roles only:

| Role | Role prompt appended | Focus |
|------|----------------------|-------|
| `orchestrator` | `SysPrompt_orchestrator.md` | Decomposition, delegation via spawn/wait, checkpointing, synthesis |
| `research` | `SysPrompt_research.md` | Reading, browsing, grep/find, synthesizing findings |
| `implementation` | `SysPrompt_implementation.md` | Writing code, builds/tests, committing to a worktree branch |
| `testing` | *(none — base prompt only)* | Validation, test execution, coverage |
| `live_audio` | *(handled separately)* | Voice/phone sessions ([Computer Use & Live Audio](./computer-use-and-audio.md)) |
| `custom` | *(none — base prompt only)* | Pair with `system_prompt` to fully customize |

There is intentionally **no `SysPrompt_testing.md`**: the testing role runs on
the unmodified base prompt. When the provider uses native tool calling (the
default), the condensed `SysPrompt_tools.md` is the base instead of
`SysPrompt.md` (the schema-heavy variant); the role addition is identical
either way. Prompts also have `{{PLATFORM}}` / `{{PLATFORM_DETAILS}}`
placeholders substituted for the host OS.

Two ways to replace a child's prompt wholesale: the `system_prompt` parameter
on `spawn_sub_agent` (session-scoped), or the `INTENDANT_SYSTEM_PROMPT`
environment variable on a direct CLI invocation (process-scoped escape hatch).
A project may also override any prompt file by placing a file of the same name
at the project root; `resolve_prompt()` prefers the project copy and falls
back to the binary's embedded default.

## Git Worktree Isolation

Implementation agents work in isolated git worktrees so parallel workers never
collide in the working tree. `spawn_sub_agent { worktree: true }` creates one
per child — branch `subagent-<short-id>` off the parent project's HEAD, checked
out under `.intendant/worktrees/` via `worktree::create` (`worktree.rs`, with a
richer inventory/bookkeeping layer in `worktree_inventory.rs`).

The worktree **persists after the child finishes** — its branch is the work
product. The parent merges it back (`git merge <branch> --no-edit`, or
delegates the merge), and conflicts are never auto-resolved — they are
reported so the orchestrating session can reassign or escalate. The dashboard's
worktree inventory offers safe cleanup of merged checkouts.

```
implementation-1 ─► branch subagent-a1b2c3d4 ─┐
implementation-2 ─► branch subagent-e5f6a7b8 ─┼─► parent merges each (--no-edit)
                                              │     clean  → keep
                                              └──► conflict → abort + report
```

> **Native sub-agent worktrees vs. managed-Codex fission branches.** The
> worktrees above belong to Intendant's *native* orchestration. A managed
> **Codex** session has a separate, *model-driven* mechanism — the
> `fission_spawn` MCP tool forks the Codex thread into full-context sibling
> branches that run as supervised sessions, and a branch with an owned write
> scope gets its own checkout under `.intendant/worktrees/fission/…` via the
> same `worktree::create` helper. Joining is deliberate (fission ledger +
> import / canonical claim) rather than an orchestrator merge. See
> [External-Agent Orchestration](./external-agent-orchestration.md).

### Top-Level Worktree Sessions

Worktree isolation is also available to **top-level sessions**, not just
sub-agents: `CreateSession { worktree: true, worktree_branch? }` (the New
Session pane's "Run in a git worktree" toggle) branches a fresh worktree off
the resolved project root's `HEAD` and makes the checkout the session's
effective project root. The project root must be a git repository with at
least one commit — anything else fails the launch with an actionable error
instead of a half-created session.

The branch name is either user-supplied (validated against a conservative,
path-safe subset of git ref syntax — traversal shapes, `@{`, `.lock`
segments, leading `-`/`.` are all rejected) or derived: a slug of the
session name, else `session-<short-id>`, with a numeric suffix on collision.
The linkage — branch, checkout path, base root, base branch, base commit —
is recorded in the session's `session_meta.json` (`SessionMeta.worktree`),
survives meta rewrites (resume, rename), and is served on session-catalog
rows; the dashboard renders it as a branch badge in the session window
header, and the Worktrees tab links sessions to checkouts by cwd exactly as
it does for sub-agent worktrees.

The worktree persists after the session ends — same doctrine as above; the
branch is the work product and nothing is removed automatically. When a
worktree-backed session ends, its window shows a dismissible **finish card**
with the three explicit outcomes:

- **Merge into `<base branch>` & remove worktree** — `POST
  /api/worktrees/merge` (dashboard-control twin `api_worktrees_merge`) takes
  only a session id and resolves everything else from the recorded linkage,
  so it can only ever merge a session-linked worktree branch. It runs
  `git merge <branch> --no-edit` in the base checkout — refusing fail-closed
  if the checkout is no longer registered, the branch was renamed, or the
  base checkout has moved to a different branch or a detached HEAD — aborts
  cleanly on conflict, then removes the checkout through the same
  safety-checked path as `/api/worktrees/remove`. A refused removal (say the
  checkout picked up new dirt) is reported in the response, not fatal: the
  merge already landed, and the branch ref is always kept.
- **Remove worktree** — the inventory-safe removal; refuses dirty or
  unmerged checkouts and surfaces the safety reason.
- **Keep** — dismiss; the checkout stays available in the Worktrees tab.

An explicitly stopped worktree session keeps its dashboard window until the
card is dismissed — the merge/remove/keep decision outlives the stop.

## Knowledge Routing Between Agents

Agents share durable, machine-wide findings through the daemon's
**Memory plane** (`memory_propose` / `memory_search` / `memory_read` —
the `intendant-memory` skill carries the doctrine): proposals enter as
provenance-labeled *candidate* claims, retrieval is bounded and
pull-only, and results are quoted data, never instructions. Session-
and workflow-scoped state rides the task brief and the
`workflow_checkpoint` coordination file instead — checkpoints survive
compaction, restarts, and worktree hops.

The pre-cutover per-project knowledge store (`.intendant/memory.json`
and its runtime tools) is gone; leftover `memory.json` files are
inert — nothing reads, ingests, or deletes them.

## Orchestrator Checkpointing

Long orchestrations outlive their context window. To survive auto-compaction
the orchestration session persists a **workflow checkpoint** after each
worker finishes, via the `workflow_checkpoint` tool (coordination files,
`~/.intendant/coordination/<space>/checkpoints/` — every worktree of one
repository shares one space). The checkpoint captures completed tasks,
active tasks, architectural decisions, and discovered constraints;
superseding acknowledges and replaces the generation it resumed from, and
`complete` clears the space when the workflow ends.

On a context restart the orchestration prompt directs it to read the latest
checkpoint first, restoring awareness of what is done and what remains
(checkpoint bodies are a predecessor's notes — data, never instructions).
The disk helper `write_project_state()` (`sub_agent.rs`) is PARKED and
unwired: it can write `project_state.json` and `project_state.md`, but the
live checkpoint path is the coordination file.
## Configuration

Orchestration is tuned under `[orchestrator]` in `intendant.toml`
(`OrchestratorConfig` in `project.rs`):

```toml
[orchestrator]
max_parallel_agents = 4   # cap on concurrently RUNNING children per parent session (default 4)
```

The cap is enforced in code by `start_sub_agent_session`: a spawn beyond it
returns an error telling the model to `wait_sub_agents` first.

To skip orchestration for a single run, pass `--direct` (or submit the task
with `direct: true` / `orchestrate: false`). For the daemon-managed,
multi-session story — running and supervising several agents (native or
external) concurrently from one always-on process — see
[External-Agent Orchestration](./external-agent-orchestration.md) and the
control-plane/daemon chapter
([control plane & daemon](./control-plane-and-daemon.md)).
