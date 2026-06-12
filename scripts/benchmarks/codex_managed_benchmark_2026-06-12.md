# Codex Managed-Context Benchmark — Constrained Window (2026-06-12) — DRAFT

> **Status: DRAFT.** The PRIMARY tier (20 tasks x 2 attempts, context window
> 40k) is complete and analyzed below. The DEEP tier (8 tasks x 2 attempts,
> window 28k) is still running; its section is stubbed and this report gets
> finalized when that lane lands.

This is the constrained-window follow-up to
`codex_lineage_benchmark_2026-05-27.md`. The May run showed reward parity at
lower managed cost — but Terminal-Bench never touched the context machinery
(zero compactions, zero rewinds in 44 trials). This run constrains the model
context window to 40k tokens specifically to force engagement, and it did:
33/40 managed trials produced rewind records (53 total) and 37/40 vanilla
trials auto-compacted (114 events).

**The headline is a real negative result for cliff-edge managed mode.**
Vanilla Codex beat Intendant-managed Codex on both reward (25/40 vs 20/40)
and cost ($51.96 vs $72.65) at w40. The deficit is not noise and it is not
context *quality* — managed context stayed denser-than-baseline and primers
carried 83% of facts across rewinds. The deficit is the *control flow and
price of the intervention itself*: every managed-specific lost trial sits in
the gate-forced engagement group, 8 of 20 managed misses ended *inside* the
management protocol (7 recovery-step-limit anchor-paging loops + 1
anchor-handoff dead-end), and the gate's cache-busting interrupts put 53% of
the lane's uncached input spend into the largest prompts. The numbers below
quantify each mechanism; they are the empirical case for the density-first
overhaul that landed after these binaries were built (`feat/density-policy`,
now merged at `b49d6923`: noise-triggered pruning, living-index primer).

## Environment

- Remote Terminal-Bench host: `user@192.168.1.206` (Debian)
- Terminal-Bench dataset: `/home/user/tbench-datasets/terminal-bench`
- Harbor venv: `/home/user/tbench-harbor-venv/bin/harbor`
- Benchmark binaries: `/home/user/projects/bench-binaries-20260611/{codex,intendant}`
  - `codex` = lineage fork `f7a06d81f` (ubuntu:22.04 / glibc 2.34 release build)
  - `intendant` = `bench/managed-harness` @ `edc13230` (debian:12 / glibc 2.36
    release build; `a4fd05ec` + pilot fixes: rollback-aware anchor catalog,
    autonomous density-gate continuation)
- Vanilla comparator: npm Codex `0.133.0`
- Agent defs: `/home/user/tbench-agents/` (June revision;
  `harbor_intendant_codex_agent:IntendantCodex` vs
  `harbor_persistent_codex_agent:PersistentAuthCodex`)
- Model: `gpt-5.5`, reasoning effort `xhigh`
- Window: `context_window=40000` both lanes. Vanilla gets
  `-c model_context_window=40000 -c model_auto_compact_token_limit=36000`;
  managed writes `model_context_window = 40000` into `$CODEX_HOME/config.toml`
  and Intendant forces `model_auto_compact_token_limit=i64::MAX` (no hidden
  compaction; rewind is the only pressure valve). Both lanes' rollouts report
  an effective `model_context_window` of **38,000** (95% of configured); the
  managed rewind-only gate sits at **32,300** (85% of 38,000, mirroring
  `mcp.rs`), vanilla auto-compacts at **36,000**.
- Auth: per-lane Codex auth homes refreshed immediately before launch
  (`/home/user/tbench-codex-homes/{managed-w40,vanilla-w40}`); lanes strictly
  serialized (managed finished before vanilla started).
- Pilot gate: PASSED at w40 with no retune (`pilot-managed-w40` 3/6,
  `pilot-vanilla-w40` 4/6 on 3 tasks x 2 — same direction as the full tier).
- Analysis tooling: `bench/managed-harness` @ `b49d6923`
  (`scripts/benchmarks/summarize_harbor_results.py`,
  `scripts/benchmarks/managed_density_report.py`).

Run artifacts:

- Managed: `/home/user/tbench-jobs/managed-w40-p20/2026-06-12__01-35-17`
- Vanilla: `/home/user/tbench-jobs/vanilla-w40-p20/2026-06-12__05-02-38`

## Terminal-Bench Summary (primary tier, w40)

| Lane | Trials | Reward | pass@2 | Cost | Input tokens | Cached (hit rate) | Output | Agent s (sum) | Job wall | Exceptions |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Intendant-managed Codex | 40 | 20.0/40, 0.500 | 0.60 | $72.65 | 32,709,403 | 24,740,096 (75.6%) | 681,139 | 23,032 | 9,075s | 2 (train-fasttext timeouts) |
| Vanilla Codex 0.133.0 | 40 | 25.0/40, 0.625 | 0.70 | $51.96 | 30,567,494 | 26,113,920 (85.4%) | 554,406 | 27,485 | 10,498s | 2 (train-fasttext timeouts) |

Context-machinery engagement: managed 53 rewind records across 33 trials
(plus 2 more trials that entered the recovery-required state without ever
completing a rewind), 0 compactions; vanilla 114 compaction events across 37
trials, 0 rewinds. Exceptions are the matched `train-fasttext` 3600s agent
timeouts in both lanes (also timed out in May).

Managed vs vanilla:

- Reward: managed **-5 tasks** (20 vs 25; mean 0.500 vs 0.625).
- Cost: managed **+39.8%** ($72.65 vs $51.96).
- Wall-clock: managed **-16.2% agent time** (23,032s vs 27,485s) and -13.6%
  job wall — managed is *faster*, just much more expensive per token.
- Cache: 75.6% vs 85.4% hit rate (decomposed in §Overhead).

For reference, the May-27 unconstrained run on the same task family:
managed 17/22 = vanilla 17/22, managed $33.37 vs $37.99 (-12.2%), zero
engagement in either lane. The constrained window flipped the sign of both
deltas; §May-27 comparison below explains why that is the expected
consequence of cliff-edge engagement rather than a contradiction.

## Task Matrix

Per task: two attempts per lane (sorted by trial suffix). `P/F` = reward 1/0,
then cost, agent wall-clock, `r<N>` = rewind records (managed) / `c<N>` =
compaction events (vanilla). `EXC` = harness exception (timeout).

| Task | Managed a1 | Managed a2 | Vanilla a1 | Vanilla a2 | M/V |
| --- | --- | --- | --- | --- | --- |
| build-cython-ext | F $1.00 175s r1 | P $1.25 338s r1 | P $1.69 415s c2 | P $2.24 706s c5 | 1/2 |
| configure-git-webserver | F $1.84 576s r1 | F $1.26 368s r1 | F $0.91 353s c1 | F $0.74 242s c1 | 0/0 |
| custom-memory-heap-crash | F $2.13 436s r4 | F $1.39 295s r1 | F $0.91 273s c2 | F $0.71 262s c2 | 0/0 |
| db-wal-recovery | F $4.08 507s r2 | P $0.19 74s r0 | F $0.99 424s c2 | F $0.90 456s c4 | 1/0 |
| extract-elf | F $1.01 322s r1 | F $0.72 264s r1 | F $0.82 296s c1 | F $0.80 388s c2 | 0/0 |
| financial-document-processor | P $1.62 357s r1 | P $1.21 352s r2 | P $0.86 288s c3 | P $0.98 293s c2 | 2/2 |
| gcode-to-text | F $1.57 324s r1 | F $2.73 532s r1 | F $1.59 383s c2 | F $0.76 176s c1 | 0/0 |
| large-scale-text-editing | P $1.31 386s r0 | P $0.67 238s r0 | P $0.72 261s c0 | P $0.80 306s c0 | 2/2 |
| llm-inference-batching-scheduler | P $1.03 356s r1 | P $1.49 499s r1 | P $1.10 450s c2 | P $1.42 671s c4 | 2/2 |
| make-mips-interpreter | F $2.77 762s r5 | F $2.12 223s r1 | P $1.48 1021s c6 | P $1.03 822s c6 | 0/2 |
| portfolio-optimization | P $1.27 457s r1 | P $0.89 343s r0 | P $0.81 291s c1 | P $1.05 493s c2 | 2/2 |
| regex-chess | P $1.54 665s r1 | P $2.87 1229s r1 | P $1.74 1357s c3 | F $1.79 2021s c3 | 2/1 |
| reshard-c4-data | P $1.25 444s r1 | P $0.87 361s r1 | P $1.67 550s c2 | P $1.33 415s c1 | 2/2 |
| rstan-to-pystan | F $1.99 221s r0 | F $2.44 364s r1 | P $1.17 958s c3 | P $1.60 967s c3 | 0/2 |
| sanitize-git-repo | P $3.95 456s r5 | P $2.44 545s r2 | F $1.44 531s c3 | P $1.40 673s c5 | 2/1 |
| schemelike-metacircular-eval | P $3.15 711s r2 | F $0.93 232s r1 | P $0.94 523s c3 | P $1.31 636s c4 | 1/2 |
| sqlite-with-gcov | P $0.84 237s r1 | F $1.98 189s r0 | P $0.87 281s c2 | P $1.07 256s c1 | 1/2 |
| train-fasttext | F EXC $4.62 3603s r3 | F EXC $3.49 3603s r1 | F EXC $4.43 3601s c12 | F EXC $3.21 3611s c12 | 0/0 |
| video-processing | F $2.80 818s r4 | F $2.19 527s r1 | F $1.36 455s c2 | P $1.59 699s c3 | 0/1 |
| write-compressor | P $0.75 282s r0 | P $0.98 358s r1 | P $1.17 461s c1 | P $0.57 220s c0 | 2/2 |

Reward flips (per-task, out of 2):

- **Managed losses (-7):** build-cython-ext (1/2 vs 2/2),
  make-mips-interpreter (0/2 vs 2/2), rstan-to-pystan (0/2 vs 2/2),
  schemelike-metacircular-eval (1/2 vs 2/2), sqlite-with-gcov (1/2 vs 2/2),
  video-processing (0/2 vs 1/2). Every one of the seven lost trials on these
  tasks except video-processing ended inside the management protocol (see
  §Failure taxonomy).
- **Managed wins (+3):** db-wal-recovery (1/2 vs 0/2 — the managed pass is a
  legitimate, unusually efficient 12-tool-call WAL-header repair at $0.19/74s;
  the other managed attempt cliff-rewound twice and failed at $4.08),
  regex-chess (2/2 vs 1/2), sanitize-git-repo (2/2 vs 1/2).
- **sanitize-git-repo — the May regression did NOT recur; it inverted.** In
  May the managed lane was the only sanitize failure. Here managed passed
  both attempts *through* 5- and 2-rewind sessions (the heaviest successful
  engagement in the lane; primer carry preserved the contaminated-path
  checklist across resets), while vanilla's failing attempt (`HGqEUVz`)
  missed `test_correct_replacement_of_secret_information` — it left
  contaminated paths unsanitized after 3 compactions.
- **Both-lane failures (5 tasks, 0/2 + 0/2):** configure-git-webserver
  (verifier asserts in both lanes, as in May), custom-memory-heap-crash (the
  May-documented Valgrind fd-limit environment failure, all four trials),
  gcode-to-text (flag case-transcription errors in both lanes — managed
  `AMSp7Fz` wrote `iZ` for `iz`, the same error class vanilla made in May),
  train-fasttext (3600s cap, both lanes, as in May), and **extract-elf — a
  new both-lane casualty of the constrained window** (both lanes passed it
  unconstrained in May; at w40 all four trials produced extractors whose
  output failed verifier compilation).

## Engagement-Conditional Reward

"Engaged" for managed = >=1 rewind record; the two trials that entered the
recovery-required gate but died without completing a rewind
(`rstan-to-pystan__CmUVh5b`, `sqlite-with-gcov__R8siwqY`, both r0) are
counted as engaged-forced in the three-way split. Vanilla engaged = >=1
compacted event.

| Group | Trials | Pass | Rate |
| --- | ---: | ---: | ---: |
| Managed, never engaged | 5 | 5 | **100%** |
| Managed, voluntary rewinds only (all records below the 32.3k gate) | 11 | 7 | 64% |
| Managed, gate-forced (>=1 record at/over the gate, or gate-death) | 24 | 8 | **33%** |
| Vanilla, never compacted | 3 | 3 | 100% |
| Vanilla, compacted | 37 | 22 | 59% |

(Flat two-way cut for comparability: managed engaged 15/33 = 45% vs vanilla
engaged 22/37 = 59%; both lanes' unengaged trials all passed.)

The managed deficit is **entirely concentrated in forced engagement**:

- The 4 failures in the voluntary-only group are configure-git-webserver x2
  and extract-elf x2 — tasks vanilla also failed 0/2. Voluntary, below-gate
  rewinds cost **zero reward relative to vanilla**.
- All nine managed-specific lost trials (the -7 task flips above) are in the
  gate-forced group.
- Both lanes degrade under engagement (constrained windows are simply harder:
  59% engaged vanilla vs 100% unengaged), but managed degrades much further
  (33% forced vs 59%), and that extra drop is the machinery, not the tasks.

## Rewind Pressure Bands (the overhaul datum)

All 53 rewind records predate the `pressure_at_rewind` instrumentation
(binaries were built before `feat/density-policy`); bands are recovered from
each record's archived pre-rewind rollout (last `token_count`, the exact
value the record writer would capture) against the reported 38,000 window:
`ok` < 32,300 (below the rewind-only gate = voluntary), `watch` 32,300-37,999
(at/over the gate), `high` >= 38,000 (past the reported window).

| Band | Records | Share | Reading |
| --- | ---: | ---: | --- |
| ok (voluntary) | 24 | 45% | model-initiated pruning at 29-83% fill |
| watch (gate zone) | 15 | 28% | forced at/over the 85% gate |
| high (over window) | 14 | 26% | cliff overshoot — up to 74,676 used (196% of window, `make-mips__gcwDMQC`); 6 records over 47k |
| critical | 0 | 0% | — |

Per-trial outcome by worst band: trials whose records are all `ok` pass 64%
(and lose nothing vs vanilla); trials with >=1 `high` record pass **3/11 =
27%**. The cliff, not the rewind, is what kills.

Cliff dynamics (from record timestamps + per-request token series):

- **Overshoot is single-turn:** the worst records were produced by one turn
  ingesting 13-37k tokens of tool output (e.g. three parallel
  `exec_command`s with `max_output_tokens` budgets of 12k/18k/16k fired from
  a 30.8k prompt). The gate can only interrupt *between* requests, so by the
  time it fires the prompt is already 43-75k.
- **14 rapid re-rewind pairs (<=150s apart):** 9 of the 14 `high`-band
  records were followed by another rewind, and in every such case the
  follow-up came 24-70s later at only 11-21k used — the model rewinds, then
  immediately rewinds again instead of resuming work (anchor repositioning
  without progress). The other 5 `high` records were single-rewind trials.
- **3 re-blow loops:** in `custom-memory-heap-crash__Kqr8nEm`,
  `make-mips-interpreter__WysfhkX`, and `sanitize-git-repo__mL4BnLv` the
  context jumped from ~13k back over the window within ~90-100s of a rewind —
  post-rewind re-derivation re-ran the same huge-output command and slammed
  the cliff again. Only sanitize survived this (and paid $3.95, 2.8x the
  vanilla pass price, for it).

Worked example, `make-mips-interpreter__WysfhkX` (failed, 5 rewinds), the
forced-rewind round-trip at ~06:03:

| Req | Input | Cached | What happened |
| --- | ---: | ---: | --- |
| r13 | 43,630 | 30,592 | 3 parallel objdump/strings calls; 3rd output replaced by the gate message ("pressure at or above the rewind-only threshold (43984/38000)... only get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, rewind_backout available"); Intendant aborts the turn (`<turn_aborted>` injected) |
| r14 | 46,999 | 8,192 | model calls `list_rewind_anchors` — the listing round-trip re-bills a 47k prompt at 17% cache |
| r15 | 47,726 | 8,192 | model calls `rewind_context`; the call is aborted ("aborted by user after 0.1s") and executed out-of-band → `thread_rolled_back`, primer injected |
| r16 | 15,154 | 8,192 | post-rewind continuation |

One forced episode = ~91k uncached input tokens ≈ **$0.46** before any
recovery work. A vanilla auto-compaction on the same hardware shape is one
mostly-cached summarization request ≈ **$0.02-0.05**, with no turn abort and
no tool-surface change: roughly a **10x per-cycle price difference**, before
counting indecision loops.

## Failure Taxonomy — all 20 managed misses

Class A — **recovery-step-limit indecision (7 trials, the headline failure
mode).** The trial crosses the rewind-only gate; the model calls
`list_rewind_anchors` repeatedly — usually re-listing offset 0 instead of
paging or committing (the catalog pages 5 anchors with `next_offset`; each
listing also *adds* an item to the thread, so the catalog grows 41→46 while
it loops); the fork's recovery follow-up step limit fires ("Managed context
recovery reached the follow-up step limit before reducing context");
Intendant kickstarts recovery (up to 2x, the pilot's autonomous continuation
fix); the model loops again; the session ends with the task half-done, final
message a forward-looking plan. Wall-clock at exit 175-532s — these trials
*had* 3,000+ seconds of budget left.

| Trial | Exit | Notes |
| --- | --- | --- |
| rstan-to-pystan__CmUVh5b | 221s, $1.99, r0 | 6+ offset-0 listings; 2 kickstarts; "refusing to mark the session complete"; **never completed any rewind** |
| rstan-to-pystan__oegRGCc | 364s, $2.44, r1 | step limit after post-rewind listings |
| build-cython-ext__sYNgg8k | 175s, $1.00, r1 | catalog had only 3 eligible anchors; final message is about checking pressure, not the task |
| sqlite-with-gcov__R8siwqY | 189s, $1.98, r0 | gate-death without a completed rewind |
| schemelike-metacircular-eval__rgugkxz | 232s, $0.93, r1 | `done_signal` 0.8s after the step-limit warning |
| make-mips-interpreter__gcwDMQC | 223s, $2.12, r1 | 196%-of-window overshoot, then step limit |
| gcode-to-text__CQnpwVq | 532s, $2.73, r1 | did page (offset 13) but never committed; vanilla also failed this task |

Class B — **anchor-handoff dead-end (1):**
`make-mips-interpreter__WysfhkX` (762s, r5). After 5 rewinds the density
handoff asked for another rewind; the model's last message: "No density
rewind applied. The only density candidate returned was a management/status
anchor, and the handoff explicitly disallowed management-tool anchors" —
then `done_signal`. The protocol cornered itself: management items polluted
the anchor catalog until no eligible anchor remained.

Class C — **both-lane task/environment failures (10):**
configure-git-webserver x2, custom-memory-heap-crash x2 (Valgrind fd-limit
environment issue, May-documented), extract-elf x2 (new both-lane
constrained-window casualty), gcode-to-text__AMSp7Fz (flag case error;
vanilla failed both attempts of this task too), train-fasttext x2 (3600s
timeouts, matched in vanilla), db-wal-recovery__NJSovog (vanilla 0/2 as
well; this trial additionally showed the worst paging — 16 anchor listings
for 2 rewinds — but the task itself defeated both lanes).

Class D — **ordinary quality failures, vanilla split (2):**
video-processing__GmXHqnQ (818s, 4 rewinds incl. 2 rapid pairs; implemented
a fragile analyzer that failed the verifier) and video-processing__zQupi3q
(527s, 1 watch-band rewind; same verifier failure; vanilla also failed 1 of
2). Context machinery added overhead but the misses look like solution
quality.

Net reward accounting: classes A+B on tasks vanilla swept = -7; managed-only
wins (db-wal, regex-chess, sanitize) = +3; video split = -1 → **-5**, the
entire topline gap. Remove the A/B protocol endings and the lanes are at
parity on this tier.

## Overhead Accounting

**Fitted pricing** (exact least-squares fit across all 80 trials, residual
<$0.0001): gpt-5.5 = $5.00/M uncached input, $0.50/M cached input, $30/M
output.

**Cost delta decomposition (managed - vanilla):**

| Component | Token delta | $ delta |
| --- | ---: | ---: |
| Uncached input | +3.52M | **+$17.58** |
| Output | +127k | +$3.80 |
| Cached input | -1.37M | -$0.69 |
| **Total** | | **+$20.69** |

**Structural surfaces (measured, corrects the planning assumption):**

| | Managed | Vanilla |
| --- | ---: | ---: |
| First-request prompt (system+tools+task) | 14,040 median (13,840-14,942) | 22,919 median (22,607-23,821) |
| Reported window | 38,000 | 38,000 |
| Forced-action ceiling | 32,300 (rewind-only gate) | 36,000 (auto-compact) |
| Working room per cycle | ~18.3k | ~13.1k |
| Cycles (resets) | 53 (1.3/trial) | 114 (2.9/trial) |
| Model requests | 1,265 | 1,034 |
| Mean prompt size | 26.0k | 29.7k |

The pre-run framing ("managed effective room ~26k vs vanilla ~37k") was
wrong on the vanilla side: vanilla 0.133.0's measured baseline is ~22.9k,
not ~3k, so managed actually ran with the *leaner* per-request surface
(~13.8-14.0k, matching the expected managed baseline) and *more* working
room per cycle, and it cycled 2.2x *less* often. **The handicap is not
room — it is the price and failure rate of each cycle.**

**Where the uncached tokens went** (every request bucketed; gate zone =
prompt >= 32,300):

| Bucket | Managed req / uncached | avg/req | Vanilla req / uncached | avg/req |
| --- | --- | ---: | --- | ---: |
| First request | 40 / 330k (4%) | 8.2k | 40 / 609k (14%) | 15.2k |
| Gate zone (>=32.3k prompt) | 236 / **4,269k (53%)** | **18.1k** | 316 / 1,119k (25%) | 3.5k |
| Within 8 req after reset | 283 / 1,202k (15%) | 4.2k | 433 / 1,553k (35%) | 3.6k |
| Other deep busts (<50% hit) | 60 / 1,137k (14%) | 19.0k | 34 / 798k (18%) | 23.5k |
| Normal | 646 / 1,127k (14%) | 1.7k | 211 / 385k (9%) | 1.8k |
| **Total** | **8,066k** | | **4,464k** | |

The single dominant term: **managed pays 18.1k uncached per request in the
pressure zone where vanilla pays 3.5k — a 5.1x penalty exactly where prompts
are biggest.** The gate-zone gap (+3.15M tokens, +$15.75) explains ~76% of
the total cost delta on its own. Mechanism (visible in the worked example):
the gate interrupt injects a `<turn_aborted>` message and swaps the tool
surface to the 5 rewind tools, invalidating the prompt prefix beyond a
shared ~8,192-token head, so the 38-48k prompts at the cliff are re-billed
nearly uncached — typically 2-3 times per episode (listing, rewind call,
sometimes a blocked ordinary call first). Vanilla's compaction does nothing
to the prefix until it rewrites history once, and its cache hit rate is flat
(~3.8-4.0k uncached/request) right through the compaction point.

**Hygiene round-trips:** 162 `list_rewind_anchors` calls for 53 completed
rewinds (**3.06 listings per rewind**; worst trial 16 listings / 2 rewinds),
53 `rewind_context`, 24 `get_status`, 0 `inspect_rewind_anchor`, 0
`rewind_backout`. >=1 recovery kickstart in 20/40 trials; step-limit
warnings in 10/40; 101 `<turn_aborted>` injections lane-wide. Hygiene tools
+ their outputs occupy a mean 3.1% (max 10.1%) of the billed prompt in
engaged subset trials (M2), and the hygiene round-trips are ~19% of the
lane's extra request count (1,265 vs 1,034).

**Output-side:** +127k output tokens (+$3.80) ≈ 53 rewind payloads (primer
median ~870 tok + preserve + next_steps ≈ 51k total) plus recovery-turn
planning and anchor-paging reasoning spread across the extra ~230 requests.

**Cache-hit summary:** 75.6% vs 85.4% lane-wide. Attribution: gate-zone
surface swaps + turn aborts (the 5.1x zone penalty above), 2-3 reduced-cache
recovery requests per rewind (~8.2k-head hits), and occasional head-boundary
busts where cache falls back to exactly the 13.7-14.7k static head
mid-trial. Vanilla shows none of these shapes; its only systematic misses
are first requests and the single history rewrite per compaction.

## Density Deep-Dive (M1-M4 on the 5-task subset)

Both lanes, both attempts of build-cython-ext, make-mips-interpreter,
rstan-to-pystan, sanitize-git-repo, financial-document-processor (20
trials; calibration TRUSTED at corrected p50 = 1.000 on every trial,
tiktoken-o200k_base). Full per-trial JSON + `report.md` regenerable via the
commands in §Reproduction.

| Metric | Managed (n=10) | Vanilla (n=10) |
| --- | ---: | ---: |
| Mean density (1 - stale share) | 0.973 | **0.991** |
| Tail density (last quarter) | 0.962 | **1.000** |
| Tier-1 old-output share (mean / max) | 0.219 / 0.852 | **0.057** / 0.595 |
| Turns-to-prune >2k outputs (median; pruned share) | 6.5; 86% of 35 | **2; 100% of 75** |
| Hygiene-tool prompt share (mean / max) | 3.1% / 10.1% | 0 / 0 |
| Re-derivation: dups (in-context / post-prune) | 38 (33/5) | 67 (43/**24**) |
| Post-prune re-derived token weight | **1,084** | 6,529 |
| Primer facts carried → next primer | 83% (245/296) | n/a |
| Primer facts referenced after rewind | 74% (432/587) | n/a |
| Primer tokens (median / max) | 1,100 / 1,853 | n/a |

Honest readout of the mechanism quality:

- **The managed *content* machinery works.** Primers are high quality (83%
  carry, 74% post-rewind reference, no runaway growth — the make-mips chain
  even shrank 1,355→1,004 tokens), and post-prune re-derivation is 5 cases /
  1.1k tokens vs vanilla's 24 / 6.5k — the primers genuinely prevent
  re-derivation ~6x better than compaction summaries. All 5 managed
  post-prune dups were `primer_ignored` (fact present, model re-ran anyway).
- **But the saved quantity is ~260x smaller than the machinery's price.**
  ~5.4k tokens of avoided post-prune re-derivation across these 10 trials vs
  +1.41M extra uncached input tokens on the same 10 trials (managed 2.75M vs
  vanilla 1.34M).
- **At w40, vanilla's effective context is *cleaner* than managed's by the
  staleness metrics.** Auto-compaction prunes every >2k output within a
  median of 2 requests (100% pruned), so old noise simply doesn't live long
  enough to accumulate (tier-1 share 0.057); managed lets outputs age ~6.5
  requests and prunes 86%, carrying 3.8x the old-output share (0.219).
  Episodic rewinds lose the janitorial race against per-turn compaction even
  while "density" stays nominally high in both lanes.
- Failing managed trials did not fail stale: `make-mips__WysfhkX` held
  density 1.000 through 5 rewinds and still failed — the failure is
  disruption and dead-ends, not noise.

## May-27 Comparison — what changed

| | May-27 (unconstrained) | Jun-12 (w40) |
| --- | --- | --- |
| Reward | managed 17/22 = vanilla 17/22 | managed 20/40 vs vanilla 25/40 |
| Cost | managed -12.2% | managed +39.8% |
| Engagement | none (0 compactions, 0 rewinds) | forced (53 rewinds / 114 compactions) |
| Cache | managed advantage (smaller prompts) | managed 75.6% vs vanilla 85.4% |

Same tasks, same model family, same harness lineage. Unconstrained, the
managed lane's leaner prompt surface (~14k vs ~23k baseline) made it
*cheaper* at equal reward, and the context machinery was never exercised —
the May report said exactly that and gated the feature on a synthetic stress
harness instead. The synthetic harness proved the *mechanics* (rewind fires,
anchors hold, gate blocks); it could not price the *economics*. w40 prices
them: when the window forces engagement, every engagement is a cliff-edge
event — interrupt, surface swap, full-price re-bill of the largest prompts,
round-trip, and a model-driven anchor decision under duress with a step
limit ticking. Vanilla's compaction is a worse memory mechanism (24
post-prune re-derivations, lost-fact failures like sanitize `HGqEUVz`) but
it is ~10x cheaper per cycle, invisible to the model, and it cannot
dead-end. At 2.9 cycles/trial, the cheap-dumb valve beats the
expensive-smart one.

## Implications for the density-first overhaul

This tier is the empirical case for the `feat/density-policy` redesign
(landed after these binaries were built; merged at `b49d6923`):

1. **Prune at ok/watch, never at the cliff.** Voluntary below-gate rewinds
   cost zero reward vs vanilla (7/11 with all 4 failures shared); gate-forced
   engagements pass 33%. Noise-triggered pruning moves all hygiene into the
   voluntary band by construction.
2. **The reset must not be a round-trip.** 3.06 catalog listings per rewind
   at 38-48k uncached tokens each is the dominant cost line (53% of uncached
   spend). A living-index primer maintained *during* normal turns removes
   the at-pressure listing/decision loop entirely.
3. **Protocol dead-ends must be impossible.** 8 of 20 misses ended in the
   step-limit loop or the no-eligible-anchor corner with hours of budget
   left. Whatever the model does, the harness must converge to *some*
   context reduction (auto-pick fallback anchor, exclude management/status
   items from the catalog — they polluted it until nothing eligible
   remained, and each listing made the catalog bigger), and a failed
   recovery must hand the task back, not end the session.
4. **Don't bust the cache at maximum prompt size.** The turn-abort +
   tool-surface swap at the gate re-bills the biggest prompts at 17% cache,
   2-3x per episode. Pressure interception needs a prefix-stable mechanism
   (constant tool surface, appended-not-injected control messages).
5. **Single-turn overshoot needs a budget guard.** The worst cliffs were
   parallel tool calls with 12-18k `max_output_tokens` budgets fired from
   30k prompts; the gate can only react after ingestion. Cap per-turn
   ingestion against remaining headroom.

What managed mode should keep: the primer/carry machinery (83%/74%, 6x less
re-derivation than compaction summaries) and the lean baseline (14k vs 23k,
which is also why managed remains 16% faster in wall-clock).

## Deep Tier (w28, 8 tasks x 2) — PENDING

> **Stub.** `managed-w28-d8` / `vanilla-w28-d8` (window 28,000 — chosen so
> the managed baseline + post-rewind headroom forces several cycles per
> trial) were launching as of this draft; the managed lane is running. This
> section will get: the same matrix, engagement split, band distribution,
> taxonomy, and overhead decomposition at the harsher window, plus
> cross-window scaling of the per-cycle cost. Expectation set by the primary
> tier: more forced cycles per trial should *amplify* the cliff-edge
> mechanisms quantified above; if it does not, that is evidence the w40
> failure modes are threshold-tuning artifacts rather than structural.

## Reproduction

Lane launches (full file: `/home/user/tbench-jobs/FULLRUN-COMMANDS-20260611.md`
on the host; auth homes refreshed per-lane immediately before launch):

```bash
PRIMARY_TASKS="-i build-cython-ext -i configure-git-webserver -i custom-memory-heap-crash -i db-wal-recovery -i extract-elf -i financial-document-processor -i gcode-to-text -i large-scale-text-editing -i llm-inference-batching-scheduler -i make-mips-interpreter -i portfolio-optimization -i regex-chess -i reshard-c4-data -i rstan-to-pystan -i sanitize-git-repo -i schemelike-metacircular-eval -i sqlite-with-gcov -i train-fasttext -i video-processing -i write-compressor"

# managed lane
cd /home/user/tbench-agents && /home/user/tbench-harbor-venv/bin/harbor run \
  -p /home/user/tbench-datasets/terminal-bench $PRIMARY_TASKS \
  --agent-import-path harbor_intendant_codex_agent:IntendantCodex -m gpt-5.5 \
  --ak binary_path=/home/user/projects/bench-binaries-20260611/codex \
  --ak intendant_path=/home/user/projects/bench-binaries-20260611/intendant \
  --ak reasoning_effort=xhigh --ak context_window=40000 \
  --ae CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/managed-w40/auth.json \
  -n 4 -k 2 --debug -o /home/user/tbench-jobs/managed-w40-p20

# vanilla lane (after managed completes)
cd /home/user/tbench-agents && /home/user/tbench-harbor-venv/bin/harbor run \
  -p /home/user/tbench-datasets/terminal-bench $PRIMARY_TASKS \
  --agent-import-path harbor_persistent_codex_agent:PersistentAuthCodex -m gpt-5.5 \
  --ak version=0.133.0 --ak reasoning_effort=xhigh --ak context_window=40000 \
  --ae CODEX_AUTH_JSON_PATH=/home/user/tbench-codex-homes/vanilla-w40/auth.json \
  -n 4 -k 2 --debug -o /home/user/tbench-jobs/vanilla-w40-p20
```

Analysis (run from this repo; rsync the run dirs locally first, excluding the
heavyweight `file_snapshots`/`frames` subdirs):

```bash
rsync -a --exclude file_snapshots --exclude frames \
  user@192.168.1.206:/home/user/tbench-jobs/managed-w40-p20/2026-06-12__01-35-17/ /tmp/mbench/managed-w40-p20/
rsync -a \
  user@192.168.1.206:/home/user/tbench-jobs/vanilla-w40-p20/2026-06-12__05-02-38/ /tmp/mbench/vanilla-w40-p20/

python3 scripts/benchmarks/summarize_harbor_results.py \
  /tmp/mbench/managed-w40-p20 /tmp/mbench/vanilla-w40-p20 \
  --csv /tmp/mbench/trials.csv --lanes-csv /tmp/mbench/lanes.csv

python3 scripts/benchmarks/managed_density_report.py \
  /tmp/mbench/{managed,vanilla}-w40-p20/{make-mips-interpreter,rstan-to-pystan,build-cython-ext,sanitize-git-repo,financial-document-processor}__* \
  --out /tmp/mbench/density-subset --no-plot
```

## Limitations

- 2 attempts per task; per-task flips of +-1 are within attempt noise —
  the engagement-conditional and taxonomy results, which aggregate across
  tasks, are the load-bearing findings.
- Rewind-record pressure bands use the archived pre-rewind rollout fallback
  (all 53 records; the records predate the `pressure_at_rewind` fields) —
  band source is uniform, so the distribution is internally consistent.
- Density/M-metrics are computed on a 5-task subset (20 trials), not the
  full 80.
- The vanilla lane ran 3.5h after the managed lane (serialized); both lanes
  hit the same org prompt-cache, and the first-request cache behavior was
  symmetric (2,432-token static-preamble hits only).
- `db-wal-recovery`'s managed pass is legitimate but unusually efficient;
  with n=2 it contributes a +1 flip that should not be over-read.
