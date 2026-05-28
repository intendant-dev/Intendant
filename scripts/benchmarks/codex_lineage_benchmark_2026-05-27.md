# Codex Lineage Benchmark - 2026-05-27

This records the benchmark evidence for the Codex lineage/Intendant-managed
context-recovery work. The important comparison is Intendant-managed Codex
against installed vanilla Codex. Bare patched Codex was kept as a small sanity
control only; Terminal-Bench does not meaningfully activate the patch without
Intendant's management protocol.

## Environment

- Remote Terminal-Bench host: `user@192.168.1.206` (Debian)
- Terminal-Bench dataset: `/home/user/tbench-datasets/terminal-bench`
- Harbor venv: `/home/user/tbench-harbor-venv/bin/harbor`
- Intendant worktree: `/Users/vm/projects/intendant/.worktrees/codex-lineage-port`
- Minimal Codex worktree: `/Users/vm/projects/codex/.worktrees/minimal-lineage-upstream`
- Model: `gpt-5.5`, reasoning effort `low`
- Auth: Codex auth copied/persisted for benchmark agents

## Terminal-Bench@2.0 Summary

Run artifacts:

- Managed: `/home/user/tbench-jobs/intendant-codex-expanded-22-authpersist/2026-05-27__08-18-21`
- Vanilla: `/home/user/tbench-jobs/vanilla-codex-0133-expanded-22-authpersist/2026-05-27__12-37-17`
- Bare patched sanity sample: `/home/user/tbench-jobs/bare-patched-codex-expanded-22-authpersist/2026-05-27__16-32-11`

Aggregate result:

| Lane | Trials | Reward | Cost | Input tokens | Cached tokens | Output tokens | Agent seconds | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Intendant-managed Codex | 22 | 17/22, 0.773 | $33.365 | 30,117,925 | 28,131,712 | 312,282 | 12,893 | Full managed lane |
| Vanilla Codex | 22 | 17/22, 0.773 | $37.992 | 39,635,394 | 37,676,928 | 312,032 | 11,240 | Installed Codex baseline |
| Bare patched Codex | 2 completed / 20 pending | partial | $0.960 | 732,676 | 667,264 | 9,971 | partial | Stopped after deciding this is only an isolation smoke lane |

Managed versus vanilla on the matched 22-task Terminal-Bench run:

- Reward was equal: 17/22 in both lanes.
- Managed used 12.18% less cost, 24.01% fewer input tokens, and 25.33% fewer cached-input tokens.
- Managed wall-clock agent time was 14.71% higher overall, mostly from task-level variance rather than a uniform slowdown.
- Terminal-Bench did not exercise context rewind in either lane: compaction, rewind, and auth-error signals were all zero in completed task summaries.

Task matrix:

| Task | Managed | Vanilla |
| --- | --- | --- |
| build-cython-ext | pass, $1.495, 362s | pass, $2.424, 526s |
| configure-git-webserver | fail, $0.860, 296s | fail, $1.244, 313s |
| custom-memory-heap-crash | fail, $1.250, 240s | fail, $1.079, 177s |
| db-wal-recovery | pass, $0.569, 201s | pass, $2.027, 400s |
| extract-elf | pass, $0.787, 314s | pass, $1.248, 332s |
| financial-document-processor | pass, $0.929, 210s | pass, $1.848, 229s |
| fix-git | pass, $0.386, 158s | pass, $0.567, 128s |
| gcode-to-text | pass, $1.683, 501s | fail, $1.978, 417s |
| kv-store-grpc | pass, $0.608, 177s | pass, $0.669, 178s |
| large-scale-text-editing | pass, $1.659, 687s | pass, $0.492, 182s |
| llm-inference-batching-scheduler | pass, $0.569, 249s | pass, $1.261, 409s |
| make-mips-interpreter | fail, $3.524, 727s | fail, $3.568, 733s |
| portfolio-optimization | pass, $1.235, 414s | pass, $0.684, 249s |
| regex-chess | pass, $2.316, 924s | pass, $2.151, 675s |
| reshard-c4-data | pass, $1.205, 448s | pass, $1.194, 350s |
| rstan-to-pystan | pass, $5.658, 1210s | pass, $3.478, 764s |
| sanitize-git-repo | fail, $0.810, 341s | pass, $0.748, 168s |
| schemelike-metacircular-eval | pass, $1.545, 584s | pass, $1.064, 301s |
| sqlite-with-gcov | pass, $0.867, 236s | pass, $1.831, 352s |
| train-fasttext | fail timeout, $2.667, 3607s | fail timeout, $5.706, 3608s |
| video-processing | pass, $1.439, 535s | pass, $2.139, 525s |
| write-compressor | pass, $1.305, 471s | pass, $0.592, 222s |

Failure classification:

- `configure-git-webserver`: both lanes failed the web-server verifier. This is not an Intendant-specific regression.
- `custom-memory-heap-crash`: both lanes failed under Valgrind because the verifier process inherited an extremely high file-descriptor limit (`1073741804`), triggering Valgrind private-file creation failure. Treat as benchmark/environment sensitivity, not a managed-Codex-specific failure.
- `gcode-to-text`: managed passed; vanilla failed with `flag{gcod3_iz_ch4LLenGiNg}` instead of expected `flag{gc0d3_iz_ch4LLenGiNg}`.
- `make-mips-interpreter`: both lanes failed, but at different subtasks. Managed produced VM execution but no `/tmp/frame.bmp`; vanilla produced the frame but missed expected DOOM init text.
- `sanitize-git-repo`: managed failed by changing `tools/eval_expdb.py`; vanilla passed. This is the main managed-lane task regression in the matched run.
- `train-fasttext`: both lanes timed out at the 3600s Harbor agent limit and failed to produce `/app/model.bin`. Both logs also showed Codex thread-store/cache TTL errors near timeout.

Benchmark-validity caveat:

- `db-wal-recovery` passed in both lanes. The vanilla trace used web/public trace discovery for expected values, so this task is less useful as a pure reasoning comparison.

## Long-Context/Rewind Stress E2E

The Terminal-Bench subset did not trigger the feature under test, so the
production gate is the dedicated context-pressure harness:

```text
scripts/codex_context_stress_e2e.py
```

The harness compares installed vanilla Codex against Intendant-managed patched
Codex. It forces a large deterministic command output into context, verifies
vanilla compaction behavior, verifies managed Codex retains the exact output
anchor, then confirms Intendant blocks ordinary tools above the rewind-only
threshold until `rewind_context` is called.

The successful tuned configuration used:

- Vanilla context window: `30000`
- Managed context window: `20500`
- Managed reported window: `19475`
- Rewind-only threshold observed: `16553`
- Long output: `python3 emit_context.py 3000`

Successful repeated runs:

| Run | Vanilla compacted | Managed avoided hidden compaction | Anchor found | Rewind-only gate | Model rewind | Post-rewind pressure ok | Production gate |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `target/context-stress-e2e/20260527-170346/summary.json` | pass | pass | pass | pass (`16836/16553`) | pass | pass | pass |
| `target/context-stress-e2e/20260527-170910/summary.json` | pass | pass | pass | pass (`16878/16553`) | pass | pass | pass |
| `target/context-stress-e2e/20260527-171428/summary.json` | pass | pass | pass | pass (`16846/16553`) | pass | pass | pass |

Tuning runs kept for diagnosis:

- `target/context-stress-e2e/20260527-164615/summary.json`: context window was too loose; rewind succeeded but the ordinary-tool gate did not trigger.
- `target/context-stress-e2e/20260527-165750/summary.json`: managed context window `18000` triggered the gate and rewind, but post-rewind status crossed back into high pressure.
- `target/context-stress-e2e/20260527-165233`: invalid/killed tuning run; setting the global context window to `18000` made vanilla repeatedly compact and rerun the long-output command.

Harness fixes made from this evidence:

- Resolve `--output-root` before changing task working directories. Without this, relative `CODEX_HOME` paths could break managed Codex resumes.
- Add `--managed-context-window` so the feature stress can tune the managed threshold independently from the vanilla compaction baseline.

## Readout

The current evidence supports these claims:

- Intendant-managed Codex is not worse than vanilla on the matched 22-task Terminal-Bench subset by pass rate.
- Terminal-Bench alone is not a valid test of the context-recovery feature, because none of the completed task logs reached compaction or rewind.
- The dedicated long-context stress harness now exercises the actual feature path and passed 3/3 tuned production-gate repetitions.
- Bare patched Codex should remain a smoke/isolation lane, not the main comparator, unless we are specifically investigating whether the Codex-side patch changes standalone behavior.

Remaining limits:

- The Terminal-Bench sample is 22 tasks, not the full suite.
- The stress E2E is synthetic but deliberately targets the product invariant: no hidden compaction under management, no ordinary tool calls while over the rewind-only threshold, successful `rewind_context`, and safe continuation below threshold.
- One matched Terminal-Bench task (`sanitize-git-repo`) regressed in the managed lane and should be investigated if this benchmark is used as a release gate.
