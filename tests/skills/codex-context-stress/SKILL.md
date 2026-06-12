---
name: codex-context-stress
description: >
  E2E test the managed Codex long-context feature against installed vanilla
  Codex. Uses a deterministic large-output task to force vanilla auto-compaction,
  then verifies managed Codex avoids hidden compaction, enters rewind-only
  recovery where ordinary tools are gated, rewinds to an exact tool-call anchor
  chosen from the anchor catalog, and returns to a safe context-pressure state.
compatibility: Requires OpenAI Codex auth in ~/.codex/auth.json, installed vanilla Codex at /opt/homebrew/bin/codex, and the patched Codex fork checkout at /Users/vm/projects/codex-minimal-lineage.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Codex Context Stress E2E

## Purpose

This is the regression test for the Codex-as-external-agent context-management
feature. It exercises the feature at the scale where the fork matters:
a long task that would normally trigger Codex auto-compaction.

The test compares:

- **Vanilla Codex**: `/opt/homebrew/bin/codex exec --json` with a deliberately
  low `model_auto_compact_token_limit`.
- **Managed Codex**: patched Codex launched by Intendant on an isolated web port
  with auto-compaction disabled, Intendant MCP context tools available, and
  context-pressure gating active.

## What It Verifies

- Vanilla Codex auto-compacts under the deterministic long-output workload.
- Managed Codex records the same long output without hidden compaction.
- The exact `exec_command` tool-call id for `python3 emit_context.py 3000` is
  recoverable from the Codex rollout.
- Once backend-reported usage crosses the effective window, the rewind-only
  gate engages. Since 2026-06 this is a recovery *flow*, not a single message:
  the supervisor replaces/holds ordinary follow-ups behind a
  `<managed_context_recovery>` kickstart, the fork runs a dedicated recovery
  turn where only `get_status`, `list_rewind_anchors`, `inspect_rewind_anchor`,
  `rewind_context`, and `rewind_backout` are visible, and both the fork
  dispatch layer and Intendant's MCP dispatch reject other tools with that
  allowlist. The harness accepts any of those signals as the gate observation.
  (The *first* over-limit tool call cannot be blocked mid-turn — Codex
  dispatches a response's tool calls before persisting that response's token
  report — which matches managed.md: the supervisor interrupts and the rewind
  prunes the noise.)
- The model performs `rewind_context` itself through the Intendant MCP tools,
  choosing an exact anchor from `list_rewind_anchors` (the harness does not —
  and since 2026-06 cannot — prescribe the anchor, because user text is held
  during recovery). The pass check requires both the `codex_thread_action_result`
  success event and a durable rewind record on disk with an exact item id.
- Post-rewind, `get_status.context_pressure.status` returns to `ok` with
  ordinary tools allowed, and a held/post-recovery ordinary command (`pwd`)
  executes normally.

## Run

The feature is on `main` in both repos. Build the patched Codex fork, then
build and run from the Intendant repo root:

```bash
# 1. Patched Codex (minimal-lineage fork)
cd /Users/vm/projects/codex-minimal-lineage/codex-rs
cargo build -p codex-cli --bin codex

# 2. Intendant (debug build matches the harness's --intendant-bin default)
cd /Users/vm/projects/intendant
cargo build --bin intendant

# 3. Harness
scripts/codex_context_stress_e2e.py \
  --managed-codex-bin /Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex
```

Binary selection is flags-only (no env vars): `--vanilla-codex-bin` defaults to
`/opt/homebrew/bin/codex`, `--intendant-bin` defaults to
`target/debug/intendant` relative to the repo containing the script, and
`--managed-codex-bin` defaults to the codex-minimal-lineage checkout's debug
binary — pass it explicitly to pin the exact build under test.

The script refuses to use port `8765`; it starts Intendant on a random isolated
port (loopback, `--no-tls`, since the dashboard defaults to mTLS) and
terminates that process at the end of the run.

The benchmark workspace is created outside any git repository (under the
system temp dir when `--output-root` is repo-nested): the managed session
resolves its project root via git discovery, and a repo-nested workspace would
both break the `emit_context.py` invocation and pull repo docs (AGENTS.md)
into the baseline prompt, distorting the pressure numbers.

## Expected Pass Criteria

The final output should include:

```json
{
  "vanilla_compacted_under_pressure": true,
  "managed_avoided_hidden_compaction": true,
  "managed_found_exact_long_output_anchor": true,
  "managed_rewind_only_gate_observed": true,
  "managed_model_rewind_succeeded": true,
  "managed_post_rewind_pressure_ok": true,
  "production_ready_gate": true
}
```

## Artifacts

Each run writes a timestamped directory under the `--output-root`
(`/tmp/intendant-codex-context-e2e/` by default; the production-gate runs use
`target/context-stress-e2e/`).

Important files:

- `summary.json`: concise pass/fail, timing, rollout metrics, pressure
  reports, rewind records (anchor item id, position, primer length).
- `raw/vanilla/*.stdout`: vanilla Codex JSON event stream.
- `raw/managed/websocket_events.jsonl`: Intendant dashboard/backend events.
- `codex_home_{vanilla,managed}/sessions/.../rollout-*.jsonl`: Codex rollouts.
- `intendant_logs/`: Intendant session log for the managed run. The durable
  rewind records live under the per-session daemon log dir
  (`~/.intendant/logs/<session>/context_rewinds/`); `summary.json` copies the
  relevant fields.

## Tuning (2026-06-11)

The managed baseline (Codex instructions + the managed tool surface, measured
with the workspace outside any git repo) is ~13.8k tokens; the recorded
3000-line output adds ~10.6k. The defaults encode the working geometry:

- Vanilla context window: `30000` (auto-compact limit `5000`,
  `body_after_prefix`).
- Managed context window: `25000` → reported effective window `23750`
  (rewind-only), hard `25000`, density watch `20187`. Post-output usage
  (~24.4k) lands between effective and hard; post-rewind usage (~14.4k)
  returns to `ok` and clears the ≥8000-token anchor-eligibility headroom.
- Long output: `python3 emit_context.py 3000`.

The pre-June tuned managed window (`20500`) is now degenerate: the June tool
surface grew the baseline past the point where `19475 − baseline` clears the
8000-token anchor headroom, so the recovery catalog comes back empty on this
workload. Re-measure (first `token_count` of turn 1 = baseline; delta after
the emit output = recorded output size) before changing `--line-count`,
`--context-window`, or `--managed-context-window`.

## Notes

- This test makes real model calls and intentionally consumes enough context to
  trigger compaction behavior. Do not put it in normal CI.
- Use `--skip-vanilla` while iterating on managed behavior, but run the full
  comparison before calling the feature production-ready.
- After a successful round, the supervisor may start the recovery kickstart on
  its own as soon as rewind-only pressure is reported — the model often
  recovers before the harness's next prompt arrives. Both orderings (held
  follow-up → kickstart → replay, and spontaneous post-turn recovery) satisfy
  the same checks.
