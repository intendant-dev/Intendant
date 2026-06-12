# Recall-Probe Protocol (density-first managed-context evaluation)

Probes measure what an agent can still *recall* from its own task history
after a long run — the central claim of managed context (rewind + append-only
archives) vs vanilla compaction (lossy summarization). Probes are a
measurement layered on top of a benchmark trial; they must not change the
trial's outcome.

## Ground rules

1. **Injection only after `task_complete`.** No probe text enters the session
   while the task is being worked. The benchmark scores (F2P/P2P, step
   completion) are computed from a run whose trajectory is byte-identical to
   a probe-free run up to the completion event.
2. **3–5 probes per task**, authored per task *before* the scored runs of
   that task (after pilot task selection — see `probe_authoring.md`), from
   lane-neutral facts of the task's early trajectory.
3. **Probe prompts forbid new work.** Every probe is wrapped in the template
   below, instructing the model to answer from its own context/records only
   and to say "I don't know" rather than guess. Managed sessions MAY use the
   Intendant recovery/records tools (`get_status`, `list_rewind_anchors`,
   `inspect_rewind_anchor`, `rewind_backout`) — consulting one's own archive
   is *the mechanism under test*, and that usage is counted, not penalized.
   Any other tool call during a probe turn (e.g. `exec_command` re-deriving
   the answer from the workspace) is a **protocol violation**: the grader
   flags the probe `tainted` and it scores as not-recalled.

## Fact classes

Each task's probe set draws from these classes (≥3 classes per task):

| class | example question shape |
|---|---|
| `first_failing_test` | "What was the name of the first failing test you saw when you first ran the test suite?" |
| `early_error_string` | "Quote the exact error message the first failed build/run printed." |
| `first_edited_file` | "Which file did you edit first, and what was the change meant to accomplish?" |
| `early_numeric_value` | "What value did you choose for <config/constant> early in the task?" |

"Early" deliberately targets material that vanilla compaction is most likely
to have summarized away by task end, while the managed lane retains it by
construction (rewind archives keep the full pre-rewind branch).

## Probe prompt template

```
Recall check (do not run commands, do not edit files, do not re-derive the
answer from the workspace): <QUESTION>
Answer from your own memory of this session. If you are not certain, say
exactly what you do remember and state clearly that you do not know the rest.
```

## Injection mechanics (`inject_probes.py`)

- **Managed lane** — the trial's Intendant instance is kept alive after
  `task_complete`; each probe is delivered as a follow-up turn over the web
  gateway WebSocket (`ws://<host>:<port>/ws`), payload
  `{"action": "follow_up", "session_id": <SID>, "text": <probe>, "direct": true}` —
  the same `ControlMsg::FollowUp` the dashboard sends (see
  `tests/skills/codex-fission-e2e/SKILL.md`). `<SID>` is parsed from the
  launch banner (`Session ID:` line) in the console log. The answer is read
  from the parent codex rollout: the next `task_complete` event's
  `last_agent_message`. Surfaces considered and rejected: `intendant ctl`
  (Unix socket is per-PID, host-side scripting against a container is
  awkward) and MCP-over-HTTP `tools/call` (no follow-up tool; it addresses
  backend threads, not the session input queue).
  Probes run before the harness's test phase (the container would otherwise
  be gone) — hence rule 3's tainting of any workspace-touching tool call.
- **Vanilla lane** — post-hoc, fully outside the trial:
  `codex exec resume <session-id> -- <probe>` against the archived
  `codex-home` (`CODEX_HOME=<archive>`), one probe per resume invocation,
  sequentially. Resume reconstructs the live context from the rollout —
  including its compacted state — so the probe sees exactly what the agent
  "remembered" at task end. Verified against npm codex 0.133.0:
  `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]`. Probe turns run with
  `--sandbox read-only` so a non-compliant model cannot mutate anything.

The lifecycle asymmetry (managed: live session pre-test-phase; vanilla:
post-hoc resume) is operational, not epistemic: both probe the conversation
state as of `task_complete`. The managed path is the proven live-session
surface; resuming an Intendant external-agent session from relocated archives
is untested machinery and not worth the risk for v1.

## Grading (`grade_probes.py`)

Verdicts: `correct | partial | confabulated | admitted-unknown` (+ `tainted`
flag per rule 3).

1. **Ground truth** is auto-extracted from the *archived full history* —
   - managed: `<intendant-log-dir>/context_rewinds/*-source-rollout.jsonl`
     retains every pre-rewind branch by construction, plus the live parent
     rollout in `codex-home/sessions/`;
   - vanilla: the live rollout in `codex-home/sessions/` — which, post
     auto-compaction, may itself have lost the fact. **That asymmetry is the
     measurement**, so vanilla ground truth is extracted from the
     *pre-compaction prefix* of the rollout (response items are append-only;
     compaction inserts a marker but does not delete earlier lines from the
     file), or supplied by the probe author.
   Auto-extraction is a scan for identifiers (test names, error lines, first
   patched file, authored regexes); authors must eyeball it (see
   `probe_authoring.md`).
2. **Exact-match tier** (no model): normalized identifier match → `correct`;
   explicit unknown-admission phrasing → `admitted-unknown`.
3. **LLM-judge tier** for everything else (`partial` vs `confabulated`
   discrimination): rubric-driven judge call, pluggable; v1 emits
   `needs-judge` and a ready-to-fill judge request file. (No model-call
   precedent exists in this repo's scripts; the interface is a clean TODO —
   exact-match grading works standalone.)
4. **Recovery-tool usage** during managed probe turns is counted from the
   rollout (`function_call` items named `get_status`, `list_rewind_anchors`,
   `inspect_rewind_anchor`, `rewind_context`, `rewind_backout`) and reported
   per probe — "answered via records" vs "answered from in-context memory"
   is part of the result, not a correction.

## Outputs

- `probe_answers.json` (injector): per probe — question, raw answer, turn
  latency, tool calls observed in the probe turn, rollout line span.
- `probe_grades.json` (grader): per probe — ground truth, verdict, method
  (`exact` / `judge` / `unknown-admission`), taint flags; plus per-task
  rollups (recall rate, confabulation rate, records-assisted rate).
