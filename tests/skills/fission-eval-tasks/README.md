# Fission-Shaped Evaluation Tasks

Two evaluation tasks designed so that Intendant's managed-Codex **model-driven
fission** (`fission_spawn` / `fission_control` / `claim_fission_canonical` —
full-context branches with charters and isolated git worktrees) has a genuine,
measurable reason to fire. The constrained-window Terminal-Bench run
(133 trials, see `scripts/benchmarks/codex_managed_benchmark_2026-06-12.md`)
measured **zero organic fission uses** — single-container, single-stream tasks
give the model no reason to branch. These tasks do: each is one repo with
2–4 separable work streams that own **disjoint write scopes** and share only a
small contract, sized so a competent agent finishes serially in 30–60 minutes.
Fission is never required and never mentioned to the agent; whether the model
chooses it is the measurement.

| Task | Components (disjoint write scopes) | Integration check |
|---|---|---|
| `polyglot-pipeline/` | `normalizer/` (Python CSV→JSONL), `dedup/` (Rust merge/dedupe), `report/` (jq/shell aggregator) | `make pipeline` end-to-end on verifier-generated CSVs |
| `service-triplet/` | `api/` (REST job store), `worker/` (job processor), `cli/` (client) — all Python stdlib | live trio booted by the verifier, driven through the agent's own CLI on random ports/payloads |

## Layout (per task)

```
<task>/
├── TASK.md       # the agent-facing prompt — NEUTRAL: never mentions fission,
│                 # branching, parallelism, or sub-agents
├── SKILL.md      # runner: launch a managed Intendant session on the task,
│                 # score it, collect artifacts (incl. fission_ledger.json)
├── HARDENING.md  # adversarial review of the verifier + reward-hack probes
├── verify.sh     # scorer: verify.sh <workdir> [--seed N] → JSON on stdout
├── verify/       # verifier internals (generators + independent reference logic)
├── skeleton/     # the repo the agent gets — copy ONLY this into the workdir
└── reference/    # full solutions for verifier self-test — NEVER expose to agents
```

**The agent must only ever see `skeleton/` contents.** `verify/`, `reference/`,
and the task docs stay outside its working directory (the SKILL runners do this
correctly; keep it that way).

## Scoring contract

`verify.sh <workdir> [--seed N]` always exits 0 when scoring completed
(non-zero only on harness-internal error) and prints a single JSON object on
stdout:

```json
{
  "task": "polyglot-pipeline",
  "seed": 12345,
  "component_scores": {"normalizer": 0.83, "dedup": 1.0, "report": 1.0},
  "integration": 0.5,
  "total": 3.33,
  "max_total": 4.0,
  "details": {"...": "per-subcheck booleans + first-failure snippets"}
}
```

- Each component score is `passed/total` over an independent behavioral
  battery; `total = sum(component_scores) + integration` (max 4.0).
- **Behavioral, hack-resistant:** every battery runs the agent's code against
  inputs *generated at check time* from a random seed, compared against the
  verifier's own independent implementation of the spec (`verify/`). Nothing
  checks for the presence of files or strings. `--seed` reproduces a run
  exactly; omitting it draws a fresh seed (printed in the JSON).
- Scoring runs on a scratch **copy** of the workdir (`.git`/`.intendant`
  excluded), so it is safe mid-run and has no side effects on the agent's
  tree. It can also be pointed at an individual fission-branch worktree
  (`<workdir>/.intendant/worktrees/fission/<x>`) to score un-merged branch
  work separately.

## Environment

Local throwaway dir (macOS/Linux) or a plain docker container. Needs:
`python3` (≥3.9, stdlib only), `bash`, `make`, `git`, `jq` (≥1.6), and for
polyglot-pipeline a Rust toolchain with the `serde`/`serde_json` crates either
cached in `CARGO_HOME` or fetchable (registry traffic only — the package-manager
equivalent of pip/npm; run `cargo fetch` in `dedup/` once at setup, after which
everything builds offline). service-triplet needs no toolchain beyond python3.
Docker example: `rust:1-bookworm` + `apt-get install -y python3 jq make git`.

Smoke-run notes for the pair live in `SMOKE.md`.
