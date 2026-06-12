# Authoring Probe Sets

Probe sets are written **per task, after pilot task selection, before the
scored runs** — from facts that any competent run of the task must encounter
early, so the same probe set is valid for both lanes (lane-neutral). This
document is the deterministic recipe.

## Inputs

One reference trajectory per task — any cheap early run works (a pilot trial,
either lane; ground truth is re-extracted per scored trial at grading time,
so the reference run only needs to establish *which facts exist*, not their
exact per-run values):

- the trial's `agent-logs/codex-home/sessions/` parent rollout (and, managed,
  `agent-logs/intendant/context_rewinds/*-source-rollout.jsonl`)
- the task definition (`INSTRUCTION.md`, test files) for facts that are
  task-determined rather than run-determined

## Recipe (per task)

1. **Replay the first ~20% of the reference trajectory** (by rollout lines):
   `python3 - <<'EOF'` over `rollout_lib.tool_outputs()` /
   `rollout_lib.tool_calls()` works, or just read the JSONL.
2. **Harvest candidate facts**, one per class, preferring *task-determined*
   facts (identical across runs) over run-determined ones:
   - `first_failing_test` — task-determined when the task ships a fixed test
     suite the agent is told to run (LongCLI F2P suites): the first failing
     test in a fresh checkout is a property of the task. Record the exact
     identifier (`tests/test_foo.py::test_bar`, `Suite.Case`).
   - `early_error_string` — the exact error line a fresh build/run prints
     (compiler error, missing dep, assertion). Task-determined for
     fixed-seed tasks; verify on the reference trajectory.
   - `first_edited_file` — run-determined but stable for tasks with an
     obvious entry point; phrase the question as "first file you edited"
     and let grading extract per-trial ground truth automatically.
   - `early_numeric_value` — a constant/config value the instructions force
     early (port numbers, buffer sizes, magic constants from INSTRUCTION.md).
     Supply `ground_truth` explicitly (task-determined) or a
     `ground_truth_pattern` regex for per-trial extraction.
3. **Write 3–5 probes covering ≥3 classes.** Questions must:
   - target one fact each, answerable in a sentence;
   - not leak the answer or its shape ("what was the test name" — never
     "was it test_foo or test_bar");
   - make sense at task end without referring to wall-clock ("when you FIRST
     ran the suite", "the FIRST file you edited").
4. **Pin ground truth.** For each probe set one of:
   - `ground_truth` — exact string, for task-determined facts (preferred);
   - `ground_truth_pattern` — regex with one capture group, evaluated against
     each trial's archived history by `grade_probes.py`;
   - neither — fall back to the fact-class auto-extractor; run
     `grade_probes.py` against the reference trial and **eyeball the
     extracted value** before accepting the probe.
5. **Dry-run the set** against the reference trial:
   `inject_probes.py vanilla --codex-home <ref-trial>/codex-home ...` then
   `grade_probes.py ...` — every probe must yield a ground truth and a
   non-degenerate grade. Discard/replace probes that grade `correct` for a
   trivially wrong reason (e.g. the answer string appears in the question).

## File format

`scripts/benchmarks/probes/sets/<task-id>.json` (one file per task; the
managed tb agent matches files by task id — `--agent-kwarg
probes_dir=.../probes/sets` enables in-run injection):

```json
{
  "task_id": "61810_cow",
  "authored_from": "<run-id>/<trial>, lines 1-840",
  "probes": [
    {
      "id": "p1",
      "fact_class": "first_failing_test",
      "question": "What was the name of the first failing test when you first ran the cow test suite?",
      "ground_truth": "tests/test_cow.py::test_basic_fork"
    },
    {
      "id": "p2",
      "fact_class": "early_numeric_value",
      "question": "What page size value does the lab use, as you noted early in the task?",
      "ground_truth": "4096"
    },
    {
      "id": "p3",
      "fact_class": "first_edited_file",
      "question": "Which file did you edit first, and what was that change meant to accomplish?"
    }
  ]
}
```

`id` values must be unique within the file; keep them stable once scored runs
begin (they key the grades).

## Anti-patterns

- Facts from the last 20% of the trajectory (still in every context — no
  discrimination).
- Facts the model can re-derive from the final workspace state without
  history (defeats the no-tools rule rather than testing recall).
- Questions whose ground truth differs between lanes by construction
  (anything mentioning rewinds, compaction, or tool names).
