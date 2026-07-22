---
name: pi-native-harness-eval
description: >
  Opt-in live acceptance and paired evaluation for Intendant's supervised Pi
  RPC backend versus Intendant's native Rust loop (and, optionally, raw Pi).
  Separates adapter correctness from model-quality benchmarking, records exact
  model/auth/harness provenance, and prevents subscription-vs-API billing from
  being mistaken for a harness result. Not for CI.
compatibility: >
  Requires a release Intendant build from this checkout, Node.js 22.19+, an
  authenticated `pi` installation, and explicit owner approval for real model
  calls. A genuinely paired quality comparison requires the same provider,
  exact model, account/auth class, effort, task corpus, and run-order controls;
  otherwise the run is an informative mixed-system comparison only.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Pi RPC acceptance and Native-vs-Pi evaluation

This is an explicitly metered, human-started procedure. Never run it in CI,
from a compatibility-status request, or merely because Pi/Intendant changed.
It spends subscription quota and/or API credit. Before starting, state the
maximum number of model turns/runs and obtain the owner's approval for that
budget.

The procedure answers two different questions and never merges their verdicts:

1. **Adapter acceptance:** does Intendant supervise upstream Pi correctly?
2. **Harness evaluation:** under a controlled provider/model/account cohort,
   which cognitive engine produces better task outcomes and at what operational
   cost?

A passing adapter does not prove Pi is the better harness. A quality win does
not excuse a broken approval, custody, resume, or history boundary.

## Preconditions and immutable run manifest

Work from an isolated Intendant worktree, never the shared repository root.
Build only the controller/runtime binaries needed by the scenario:

```bash
cargo build --release --bin intendant --bin intendant-runtime
```

Before any model turn, write a run manifest outside the evaluation repositories
with all of the following. Do not fill missing values by inference:

- wall-clock date/time and host OS/architecture;
- Intendant commit, dirty diff hash, binary `--version`, and configuration;
- resolved Pi command path plus the passive compatibility artifact fingerprint,
  contract-manifest digest, and findings returned by
  `GET /api/external-agents`/its dashboard-control twin;
- Pi package/source version when it can be obtained without invoking an opaque
  wrapper (otherwise record `unknown` and the executable fingerprint);
- provider, exact model id echoed by each engine, effort/thinking level, context
  window, service tier, and whether the run was cold or warm;
- auth/billing class for each arm: API key, ChatGPT subscription OAuth, another
  subscription OAuth, or unknown;
- tool profile, network/web-search setting, autonomy/approval policy, project
  instruction files, and task-corpus revision;
- randomized arm order and the predeclared run/turn/quota stop limits.

Reject a run from the **paired harness cohort** if provider, exact model,
auth/account class, or effort differs. Keep it as a separately labeled
mixed-system observation. In particular, “Pi on ChatGPT subscription versus
Native on pay-per-use API” answers a deployment/product question, not a clean
harness question. Once the native ChatGPT-OAuth path is available, a
Pi-vs-Native comparison on the same plan/model is closer, but server-side
routing and quota state may still differ and must be recorded.

Do not pin Pi merely to make the experiment easy. Record the executable
fingerprint and use Intendant's passive drift report. If drift is present,
finish adapter acceptance before admitting quality runs from that artifact.

## Isolation and corpus rules

Use a disposable root containing one fresh clone/worktree per run. Every arm
starts from the same commit and byte-identical untracked fixture; verify the
pre-run tree hash. Never reuse a model conversation across scored tasks. Keep
the task text frozen, including punctuation and attachments. Repository
`AGENTS.md`/`CLAUDE.md` stays in scope because consuming project policy is part
of the harness; do not copy one harness's hidden/system prompt into another.

Use at least these task classes, chosen before seeing results:

- diagnosis and repair of a failing test with one localized cause;
- multi-file implementation with an exact behavioral contract;
- unfamiliar-code navigation where the correct change is not named in the
  prompt;
- instruction/safety compliance with a tempting out-of-scope mutation;
- context-pressure/recovery task with enough evidence to require synthesis;
- interruption, steering, and resume after a partially completed turn.

Each task needs executable acceptance checks and a frozen “must not change”
list. Avoid benchmark fixtures that occurred in either harness's public docs,
tests, or prompts. Search for contamination before admitting the task. Keep
private holdouts private until all runs finish.

For directional evidence, run every task at least three times per arm with
order balanced AB/BA across tasks. More repeats are needed for small
differences. There is no seed control for ordinary hosted model sampling, so
report distributions and all raw outcomes; never select the best attempt.

## Phase A — supervised Pi adapter acceptance

Use a disposable project and `INTENDANT_HOME`. Use the ordinary Pi agent home
only when the owner explicitly accepts transcript/auth use; otherwise create a
private `PI_CODING_AGENT_DIR` containing the minimum copied auth/settings files.
Remember that an OAuth refresh may rotate the credential: deleting an isolated
copy without a compare-and-swap copy-back can strand the valid refresh token.

Drive the real daemon through `intendant ctl` or dashboard-control, not by
calling adapter internals. Verify all of these before quality scoring:

- startup reaches the RPC `get_state` handshake, reports native session id,
  CWD, model, thinking, context window, and a clean compatibility observation;
- a read-only `read` or `grep` outside `PI_CODING_AGENT_DIR` runs without an
  approval, while `bash`, `write`/`edit`, an unknown tool, and a read targeting
  Pi's auth home all enter the Intendant approval rail;
- approve once, approve for this Pi session, deny, cancel, and no-interactive-UI
  paths have the expected scope and fail-closed behavior;
- streaming prose, reasoning, tool output, file activity/diff, non-zero usage,
  cache facts when present, and turn completion appear on the common rails;
- image input reaches Pi; a normal file attachment is staged and named in the
  prompt without escaping the workspace policy;
- mid-turn steer is consumed by the running turn, abort returns the process to
  a usable idle state, and a follow-up still succeeds;
- live model/thinking changes echo and persist in the session launch overlay;
  exact tools distinguish inherit, empty/no-tools, and a non-empty list across
  restart;
- compact, rename, head fork, and side-child relationship work; parent history
  remains unchanged and the child receives its own valid Pi session id;
- stop/resume by native id recalls context and uses the recorded CWD/config;
- catalog/detail replay shows only the active parent chain, while message search
  can find an abandoned sibling with `superseded=true`; billed usage includes
  assistant entries on every physical branch;
- a torn final session row is ignored until completed, and a leased/staged Pi
  transcript remains discoverable after secret cleanup;
- Pi reaches an Intendant-only capability through `"$INTENDANT" ctl` and the UI
  never claims that the call was Pi MCP.

Preserve the failed run root for diagnosis only if it contains no credential,
or protect and explicitly clean it afterward. Never print `auth.json`, bearer
URLs, child environment, or raw approval payloads containing secrets.

## Phase B — paired quality experiment

Preferred arms:

- **N:** Intendant native Rust loop.
- **P:** Intendant-supervised Pi RPC.
- **R (optional):** raw upstream Pi, to measure the incremental supervision
  effect separately from the Native-vs-Pi engine difference.

Run N/P (and R if selected) from identical fixture copies. The engines' native
tool schemas and prompts are part of what is being evaluated; do not force
schema identity. Do equalize the authority ceiling: same network policy, same
write scope, same forbidden paths, and the same human-answer policy. For an
autonomous score use a predeclared policy that automatically permits ordinary
workspace-safe actions and denies destructive/out-of-scope actions in every
arm. Record every human intervention and score a run requiring undeclared help
as assisted rather than silently successful.

Randomize the first arm per task, then alternate to reduce time-of-day quota and
provider-load bias. Do not run arms concurrently on one subscription if they
compete for the same rate-limit window. Capture cold-start and warm-session
latency separately. If infrastructure fails before the first model response,
allow one predeclared retry and retain both attempts; a model/tool-loop failure
after generation starts is an outcome, not free erasure.

## Metrics and scoring

Primary outcome is task correctness, evaluated without knowing the arm:

1. frozen acceptance tests and must-not-change checks;
2. a blinded diff review using a fixed rubric: correctness, completeness,
   maintainability, scope discipline, and security;
3. final repository state, not the agent's self-report.

Report these secondary measures per run:

- pass/fail/assisted, rubric score, regressions, changed lines/files;
- wall time, time to first model text, time to first tool, time to green tests;
- input, cache-read/cache-write, and output tokens when both arms expose them;
  otherwise label the metric unavailable rather than estimating equality;
- turns, tool calls, failed/retried calls, approval prompts, denials, human
  answers, steers, interrupts, compactions, forks, and process restarts;
- rate-limit events and remaining quota windows when exposed;
- native/API dollar estimate separately from subscription quota consumption.

The headline table must show per-task raw outcomes plus median and spread, not
only one aggregate. Use paired deltas for tasks admitted to the strict cohort.
Do not declare one harness “best” from fewer than three repeats per task, from a
mixed auth/model cohort, from self-graded answers, or from aggregate benchmark
score without operational failures.

## Required report structure

End with four explicit sections:

1. **Adapter verdict** — acceptance checks passed/failed, exact Pi artifact,
   compatibility findings, and any safety/custody limitation.
2. **Strict paired cohort** — only comparable N/P runs, raw task table,
   distributions, and uncertainty.
3. **Mixed-system observations** — subscription/API or model-mismatched runs,
   useful for product economics but excluded from harness causality.
4. **Decision** — keep/expand/rollback Pi integration, concrete refactors worth
   porting into the Rust runtime, and the next predeclared experiment.

Keep all raw manifests, Intendant session ids/log paths, Pi session ids, test
outputs, and tree hashes. Redact credentials and token-bearing URLs. A future
rerun should be able to reproduce the cohort without relying on prose memory.
