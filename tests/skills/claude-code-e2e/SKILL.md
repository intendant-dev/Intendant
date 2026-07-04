---
name: claude-code-e2e
description: >
  Live end-to-end test of the Claude Code external-agent integration: spawn a
  supervised Claude Code session in a toy repo, exercise the approval protocol
  (allow + deny), native session-id capture, mid-turn steering, interrupt,
  thread actions (/compact, fork via --fork-session), and resume-by-native-id,
  driving everything through the Unix control socket.
compatibility: Requires the `claude` CLI (≥ 2.1.x) authenticated on this machine
  (subscription OAuth or API key) and a release build of intendant. Makes real
  model calls — haiku ONLY (enforced via [agent.claude_code] model); not for CI.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Claude Code External-Agent E2E

## Purpose

Smoke-test the whole supervised-Claude-Code loop against the *real* CLI: the
stream-json protocol drift that broke this integration in the past (approval
`updatedInput` requirement, tool results on `user`-type messages, session-id
capture) is exactly the kind of thing unit tests can't catch. Everything runs
on **claude-haiku** — never a more expensive model.

## What It Verifies

- **Approvals**: a Bash `can_use_tool` request surfaces as
  `approval_required` on the control socket; *approve* actually runs the tool
  (CC ≥ 2.x requires the allow response to echo `updatedInput` — a regression
  here makes approved tools silently fail), and *deny* blocks it while the
  model is told why.
- **Native session id**: `session_identity` upgrades from the
  `claude-code-session` placeholder to the real UUID announced on the first
  turn (`AgentEvent::NativeSessionId` → overlay + identity).
- **Usage**: `usage_update` events carry haiku token counts against the
  `modelUsage` context window.
- **Capabilities**: the session advertises `steer: true, interrupt: true`.
- **Native steer**: a `steer` sent mid-turn is absorbed into the *running*
  turn (the steered file exists when that same turn ends — the queue-fallback
  path would only deliver it at the next turn).
- **Interrupt**: `interrupt` aborts a ~90s command well under its runtime,
  and the Claude Code process survives for follow-up turns.
- **Thread actions**: capabilities advertise the universal
  `thread_actions: ["compact","fork"]`; `{"action":"thread_action",
  "op":"compact"}` performs a real in-place compaction (the session still
  recalls pre-compact facts); `op:"fork"` materializes a NEW wrapper session
  that resumes the thread with `--fork-session` — its first prompt binds a
  fresh native id, emits the `fork` session relationship, and recalls the
  parent's pre-fork context while the parent stays untouched.
- **Resume**: a second `--continue` run binds to the most recent session's
  native id — after the fork phase that is the FORK child (resolution reads
  the wrapper log's identity record, written directly by
  `persist_native_backend_session_id` since the bus tee only reaches the
  daemon-main log) — and recalls conversation context.

## Run

```bash
cargo build --release
node tests/skills/claude-code-e2e/driver.cjs            # uses target/release/intendant
# options: --binary <path> --workdir <path> --port <n> --keep
```

The driver creates a disposable git repo with:

```toml
[agent]
default_backend = "claude-code"

[agent.claude_code]
model = "claude-haiku-4-5-20251001"   # e2e policy: haiku only
permission_mode = "default"           # so Bash prompts for approval
```

launches `intendant --agent claude-code --no-tui --web <port>
--control-socket "<task>"` (the dashboard must be enabled — truly headless
runs auto-deny external approvals), connects to
`/tmp/intendant-<pid>.sock`, and scripts the scenario with
`{"action": approve|deny|steer|interrupt|follow_up}` control messages while
asserting on outbound `approval_required` / `session_identity` /
`usage_update` / `model_response` / turn-end events. It prints a ✅/❌ check
table and exits non-zero on any failure; the full event log lands in
`<workdir>/e2e.log`.

## Interpreting failures

- `approved-tool-ran` failing while `approval-surfaced` passes → the allow
  response schema regressed (check `updatedInput` in
  `external_agent/claude_code.rs::approval_response_payload`, and look for a
  `ZodError` tool_result in the session activity log).
- `native-session-id` timeout → the reader stopped capturing `session_id`
  from stdout messages, or `AgentEvent::NativeSessionId` lost its drain arm
  in `main.rs`.
- `steer-absorbed-in-turn` failing → steering fell back to the queue path
  (check `steer_turn` and the load-bearing "not supported" error strings) or
  the CLI stopped absorbing queued user messages mid-turn.
- `interrupt-aborts-turn` failing → the `control_request`/`interrupt`
  round-trip broke; `process-survives-interrupt` failing → the abort is
  killing the whole CLI process rather than the turn.
- `compact-dispatched` failing → the `/compact` user-message trigger regressed
  in the CLI (verify by hand: send `/compact` as a stream-json user message
  and look for `status: compacting` → `compact_boundary`), or the
  `thread_action` control alias / drain routing broke.
- `fork-creates-wrapper-session` timeout → the drain's
  `ForkHandling::RespawnResume` branch isn't emitting `ResumeSession
  { fork: true }`, or no session supervisor is running (it requires `--web`).
- `fork-child-has-own-native-id` failing → `--fork-session` missing from the
  spawned argv (`fork_resume` derivation compares the resume id against the
  persisted `forked_from`), or the child's placeholder identity leaked.
- `resume-same-native-session` failing → the external overlay/identity
  records aren't being written (or `--resume` stopped keeping the session id
  stable in the CLI — verify with `claude --resume <id>` by hand).
- Approval prompts may auto-resolve if the machine's global Claude Code
  settings inject allow rules or hooks; the driver's toy repo can't fully
  isolate `~/.claude` (the supervised CLI inherits user settings by design —
  that's the supervisor's identity model, see the credential-custody lease
  notes). Hook-based Bash blocks (e.g. long `sleep` rewrites) are why the
  long-running commands use `for i in $(seq 1 N); do sleep 1; done` loops.
