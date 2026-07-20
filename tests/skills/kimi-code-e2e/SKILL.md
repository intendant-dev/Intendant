---
name: kimi-code-e2e
description: >
  Reproducible live end-to-end acceptance test for Intendant's Kimi Code
  external-agent backend. Exercises the real authenticated Kimi 0.27-0.28
  server-v1/v2 adapter with K2.7 Coding, including approvals/questions,
  attachments, streaming/usage/tools/diffs, native lifecycle and goal
  actions, exact historical fork/undo, side and background agents, search,
  interrupt/steer, and resume. Not for CI.
compatibility: >
  Requires an authenticated Kimi Code 0.27.x or 0.28.x installation and a
  release intendant build from this checkout. Makes real K2.7 Coding model
  calls.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Kimi Code External-Agent E2E

This acceptance scenario catches protocol and orchestration drift that mock
tests cannot: Intendant starts a private foreground Kimi server (`kimi server
run` on 0.27, or `kimi web --no-open` on 0.28), uses its real
bearer-authenticated REST/WebSocket server-v1 surface plus its allowlisted
server-v2 agent RPCs, and drives it through the same control socket and
dashboard HTTP routes used by frontends.

It is test documentation and a runnable harness, not an operational Intendant
skill. Do not run it in CI; it makes real model calls.

## Fixed model and isolation

Every model turn uses the canonical model id
`kimi-code/kimi-for-coding`, displayed by Kimi as **K2.7 Coding**. The driver
does not accept a model override.

The harness creates:

- a disposable git project;
- a disposable `INTENDANT_HOME`;
- a disposable `KIMI_CODE_HOME` containing private copies of only the
  authenticated Kimi credential/config files needed to run.

Kimi transcripts, server lock/token, MCP bridge configuration, Intendant
logs, and test project files therefore stay out of the user's normal stores.
Default cleanup terminates the Intendant process, tracks and reaps only its
proven descendants, removes its Unix socket, and deletes the disposable root.
Because Kimi's OAuth provider can rotate its refresh grant during a successful
call, the stopped harness first compare-and-swap publishes a changed isolated
credential back to the source file with an atomic 0600 replacement. It refuses
to overwrite a source changed by a concurrent `kimi login` or direct refresh;
without that copy-back, deleting the isolated home would strand the only valid
rotated grant and make the machine login unusable.
Use `--keep` only when inspecting a failure; that root contains copied Kimi
credentials and must be deleted securely afterward.

## What the full scenario verifies

- Native `session_identity` capture and the first-class Kimi capability
  catalog, including goal completion, live goal budgets, review, high-speed
  model toggling, exact active-tool mutation, and destructive context clear.
- Kimi's distinct structured `AskUserQuestion` request/answer rail, including
  a one-choice answer that must remain typed as multi-select.
- The generated bearer-authenticated Intendant MCP bridge, through a real
  read-only `list_displays` call, Kimi 0.28's native MCP approval request, and
  correlated tool output while a project MCP declaration deliberately
  occupies the default server name.
- Dashboard-staged ordinary-file and image attachments delivered through
  Kimi's native file/base64 APIs.
- Manual approval allow (MCP) and deny (destructive Bash), safe workspace
  Write execution, tool activity/output, file diff, incremental text,
  thinking, and non-zero K2.7 usage.
- Exact historical real-user-turn-boundary fork, parent-preserving lineage,
  native undo, an attached/independently usable native head fork with exact
  inherited profile, compact, rename, archive, and restore.
- Live model, thinking, permission, plan, and swarm profile switches without
  restarting the server, plus the complete model catalog and display-label
  resolution.
- Exact active-tool replacement, a deliberately empty active set, and
  restoration of every registered tool.
- Native goal set/get/pause/resume/clear, token/turn/wall-clock budget
  mutation, and explicit completion.
- Enforced read-only review with exactly zero active Kimi tools, bounded
  controller-collected workspace evidence, exact post-turn tool-profile
  restoration, and a whole-worktree integrity check, plus
  `/fast` on/off switching through Kimi's real K2.7 Coding
  Highspeed alias with no model turn while that alias is selected.
- Todo/plan translation.
- True mid-turn steering into a running Bash tool and prompt/session
  interruption with a surviving follow-up process.
- Native `:btw` Kimi child identity, ephemeral side relationship, scoped
  response/terminal boundary, per-agent exact tool mutation/context clear,
  exact close event, and restored parent routing.
- Real background `Agent` tasks: scoped child relationships, a completed
  child's native output through list/output actions and the shared HTTP task
  inspector/output peek, plus a distinct long-running child observed and
  cancelled through Kimi's task API. Active-child control scoping is covered
  separately by the `:btw` phase.
- Kimi session catalog/detail replay, deep search, indexed message search,
  explicit stop/resume by native id, and post-resume context recall.
- Destructive native context clear only after every context-sensitive check,
  at the end of the disposable harness session.
- A clean passive-protocol report for the exact Kimi executable/version after
  both initial supervision and native-session resume.

## Run

From an isolated Intendant worktree:

```bash
cargo build --release --bin intendant --bin intendant-runtime
node tests/skills/kimi-code-e2e/driver.cjs
```

Options:

```text
--binary <path>   Intendant binary (default target/release/intendant)
--kimi <path>     Kimi binary (default: resolve `kimi` on PATH)
--workdir <path>  Test project (default: disposable)
--port <n>        Loopback dashboard port (default: choose a free port)
--keep            Preserve the disposable root for diagnosis (contains auth)
--quick           Skip long steer/interrupt/background-agent phases
--background-only Run only startup plus the background-agent acceptance phase
--auth-sync-self-test
                  Hermetically test OAuth rotation copy-back and CAS refusal
```

The full run can take several minutes because it intentionally waits on live
tool, child-agent, compaction, indexing, and resume boundaries. It prints a
pass/fail table and exits non-zero on any failed assertion. With `--keep`,
`e2e.log` is written under the printed root.

## Reading failures

- No native identity or WebSocket events: inspect server-v1 handshake,
  bearer/token permissions, bridge-home setup, and cursor/snapshot reconnect.
- Question timeout: confirm Kimi remained in `manual` permission mode and
  `AskUserQuestion` is active.
- Approved Write does not run: inspect approval answer translation and the
  Kimi interaction id map.
- Attachment response lacks the token/color: inspect dashboard upload
  resolution, Kimi file upload, and base64 image content construction.
- Historical fork child remembers the DROP codeword: the fork-at-head plus
  pre-publication atomic undo staging regressed.
- Undo removes KEEP or retains DROP: active real-user turn counting no longer
  matches Kimi's native `:undo` horizon.
- Steer file appears only after another follow-up: `prompts:steer` fell back
  to ordinary queued delivery.
- Interrupt takes close to 90 seconds or the next reply fails: prompt abort /
  session abort routing killed too little or too much.
- Side/background child output lands on the parent: scoped agent ids or
  relationship registration regressed.
- Native task action works but HTTP inspector is empty: adapter task events
  are not populating the shared background-task registry.
- Detail works but message search times out: Kimi home discovery, complete-line
  cursors, or the live indexer sweep regressed.
- Resume binds a different native id: wrapper identity/overlay persistence or
  Kimi resume selection regressed.
- Exact/empty/all tool checks fail: the v2 active-tool profile mutation or
  readback diverged.
- A model response appears between the two fast toggles: work ran on the
  temporary Highspeed alias instead of canonical K2.7 Coding.
- Goal completion still leaves an active goal: the v2 goal terminal snapshot
  was not translated before Kimi cleared its standing goal.
- Post-clear context still contains the attachment token: native
  `clearContext` did not clear the live agent journal.
