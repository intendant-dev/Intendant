# Reconciled Gate-A criterion-12 review — D0-A Core + Memory @ `2f66b592`

*2026-07-15. Synthesis of the independent repair verification and the
peer's criterion-12 review over draft PR #318, branch
`owner-plane-d0a`, seven-commit tranche `52b9b8e5..2f66b592`.*

**Reviewed head:**
`2f66b5926258bcb0d90d0ffd62445abae43cd41d`

**Normative pins:** spec v0.5.20
`ec3a9a6dda8f8c839b6c6eb7fb3322b439bf3976a8cd8ac0f6297838102dedef`;
companion
`11dd88972220cac3a120f6f729c9b3eb9cd9e6a9a332bff75b4765efd178aaba`.

**Input reviews:**

- `~/owner-plane-gate-a-repair-verification-review.md`
  (`d4daa1643fc13a438430286f9df362d22b23ce79d8e9c62b293390dd8d20141c`)
- `~/owner-plane-gate-a-criterion12-review-2.md`
  (`fb8d7d3561805df0b9e4de4ac955fc3b18201f6adaa5caa6c5d39513ffa52793`)

The repository and PR were kept read-only. All mutations used to test
whether a gate could be made red were made in scratch copies.

## Verdict

**FAIL — keep PR #318 draft and do not stamp Gate A yet.**

Criterion 12 has now been performed procedurally: fresh independent
review happened, twice. It did not end with a defensible “no executable
finding” result. Three legal, executable traces still contradict frozen
protocol semantics:

1. **D-99:** a correctly signed, hash-valid, registry-known but
   CDDL-invalid control body reaches request replay and control placement
   before complete body validation.
2. **D-130:** a boundary can select a random Head hash whose operation
   bytes are not held, although the exact-reference rule requires
   `ref-unresolved` until those bytes arrive.
3. **D-202:** the proposed same-coordinate re-proposal admits on the
   late-first replica but fork-freezes both operations on the timely-first
   replica, so it is not a cross-replica convergence carrier.

The portable-storage lane also proves less than criterion 8 and the audit
say: the current binary genuinely calls `sync_all` and `rename`, but four
raw streams bypass that path, replace-existing is untested, and the flush
counter is not coupled tightly enough to detect removal of the call. This
is an execution-evidence and truthfulness gap, not evidence that the
current binary never flushes.

Criterion 11 also remains false because several counts, comments,
profile rows, audit claims, and the PR description lag the current
artifacts.

This is a bounded send-back, not a reopening of the RFC. The canonical
fold, Journal authority, audit release checks, lane manifests, browser
lane, strict gate, and most of the repair tranche are strong and should
stand.

## How the two reviews reconcile

The peer review's positive evidence is substantial and reproducible. It
tested the committed corpus deeply, including 5,509 additional delivery
orders, the browser and storage lanes, negative controls, and current CI.
The first review independently tested 62,280 legal orders over the larger
convergence vectors and also found no ordinary convergence regression.

The disagreement comes from **case selection**, not from different
results on the same bytes:

- the peer tested the literal body-hash and signature-invalid D-99 arms;
  the failing trace uses a valid signature and valid body hash over an
  invalid arm-specific body;
- the peer treated any same-coordinate Head mismatch as D-130 selection;
  the failing fixture names a hash for which no bytes are held;
- the peer tested only D-202 deliveries where late evidence precedes the
  original operation; the failing trace supplies the complementary
  timely-first replica that D-202 itself expressly permits;
- the peer verified that the current storage source invokes both
  primitives; the other review tested the completeness and
  discriminating strength of that proof.

A large green sample is valuable evidence, but it cannot override one
legal red counterexample to a universal protocol rule. The reconciled
answer is therefore not a vote between reviews: accept the peer's green
evidence where it applies, and retain the counterexamples it did not
exercise.

## Shared reproduction record

Both reviews independently verified the following at the exact head:

| Artifact | Reconciled result |
|---|---|
| Core suite | 141/141 PASS |
| Reducer suite | 36/36 PASS |
| Strict differential harness | 165/165 PASS, exit 0 |
| Browser lane | 56/56 PASS in headless Chromium; current substrate totals `records=45 bytes=40053 frames=72 cuts=11` |
| Portable-storage lane | 19/19 PASS on real files; current run reports `sync_all=10 rename=10` |
| Mint | Vectors and coverage regenerate byte-identically |
| CI | All owner-plane browser, Rust, and three-OS storage jobs green at `2f66b592` |
| Generated convergence | All eight formerly failing orders committed; arrival-order-restoration negative is discriminating |
| Additional order testing | Peer: 5,509 adversarial orders; first review: 62,280 legal orders over larger vectors; no divergence or `Unimplemented` |
| Gate controls | Semantic red exits nonzero; empty corpus and non-permutation delivery both fail |

The order counts should not be added together: the sweeps use different
selection strategies and overlap. Together they are compelling evidence
that the canonical set-derived fold repaired the prior arrival-order
class.

## Reconciled acceptance-criteria adjudication

| # | Result | Reconciled basis |
|---:|---|---|
| 1 | **PASS** | Both reviewers reproduced all suites and relevant CI at one pinned commit. |
| 2 | **PASS** | The eight regression orders and generated convergence suite are committed and real. Walkthrough vectors already enter `run_fold_vector`, which invokes the metamorphic sweep. |
| 3 | **PASS** | The retained arrival-ordered implementation genuinely diverges on the control trace while the canonical engine converges. |
| 4 | **Literal arm PASS; governing D-99 rule FAIL** | Body-hash mismatch and signature-invalid replay are fixed. Complete body validity—hash, registry, and CDDL—still does not precede replay and placement. |
| 5 | **PASS at the committed model** | Forged and unadmitted recovery evidence cannot verify a Journal kill. The redundant-aux byte-retention assumption remains a recorded model boundary, not a demonstrated authority bypass. |
| 6 | **PASS at the declared abstraction** | Completeness, disjointness, exact union, and split-Txn refusal derive independently. The fixture declares release membership rather than carrying a physical Txn; do not claim more. |
| 7 | **PASS** | Exact lane manifests and both annotation-loss controls are discriminating. |
| 8 | **PARTIAL / claim not established as written** | The current code really executes ten flush/rename paths. It does not route every stream through them, does not replace a pre-existing destination, and its flush counter survives removal of the flush call. |
| 9 | **Local lifecycle PASS; cross-replica D-202 FAIL** | Late-first stickiness is executable and load-bearing. The timely-first world defeats the same-coordinate convergence carrier. |
| 10 | **PASS** | Empty-corpus and non-permutation controls are red with useful diagnostics. |
| 11 | **FAIL** | Multiple live documentation and metadata statements remain false at the reviewed head. |
| 12 | **Performed; findings remain** | Fresh independent review occurred. Its “no executable finding” conclusion is not sustainable after the three reproduced counterexamples. |

**Additional protocol blocker — D-130:** the repair criteria did not
give this newly re-authored behavior its own row. That does not exempt it
from the frozen spec. The exact named Head must be held before selection;
a merely occupied coordinate is insufficient.

## Executable findings

### F1 — D-99 validates only part of the body stage before replay and placement

**Classification:** Gate-A protocol blocker.

The normative control pipeline in §10.2 and D-99 is:

```text
parse → arm → signature → body(hash + registry + CDDL)
      → precedence-field validity → placement/precedence → state
```

At `reducer/src/fold.rs:4930–4961`, classification instead performs:

```text
arm/signature → body_hash → registry → request-ID consult → placement
```

Arm-specific body shape remains inside the later `admit_*` transition.
For example, the required `c.grant` body members are checked only in
`admit_grant` (`fold.rs:1253–1259`).

The peer correctly proves two narrower fixes:

- a body-hash mismatch cannot freeze the plane; and
- a signature-invalid operation cannot win request-ID replay.

Neither covers a correctly signed operation whose hash binds an invalid
body. Re-sealing the existing post-C2 `g4` as `c.grant` over
`{bogus: 1}` with the real root key produced a signature-valid,
hash-valid, registry-known operation. With the normative expected first
failure, the strict harness returned exit 1:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: g4 expected (body-invariant, reject-permanent),
derived (ctrl-fork, freeze-control)
GATE RED
```

The consumed-request-ID sibling derives `request-fork` before the same
body failure. These are exactly the multi-fault cases D-76/D-99's ordered
pipeline exists to settle.

**Repair:** add arm-indexed intrinsic CDDL/shape validation before both
`request_check` and `ctrl_fork_gate`; leave state-dependent invariants in
the final transition stage. Commit at least CDDL-invalid × C2 and
CDDL-invalid × consumed-request-ID vectors.

### F2 — D-130 selects an exact Head whose bytes do not exist

**Classification:** Gate-A protocol blocker.

`f07-revoke-cutoff-head-mismatch-selects` does not contain two signed
variants at one tenant coordinate. Its builder draws 32 random bytes as
`wrong.head.op` (`core/src/corpus_ctrl.rs:697–748`, draw at line 721) and
puts that hash in the boundary. No operation with that hash is present in
the vector or held auxiliary state.

`parse_heads` (`reducer/src/fold.rs:1417–1448`) finds any held operation
at the coordinate and commits the arbitrary named hash as a D-130
selection. The boundary admits and the one real operation is quarantined.

The peer's reading is correct only for **differing held variants**. The
spec separately and explicitly says an unheld named Head is
`(ref-unresolved, pending-dependency)` until the exact bytes arrive
(§7.1, lines 1788–1792). Correcting only the expected state of the
committed fixture makes the head reducer red:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: i expected admitted,
derived (cutoff, quarantine-reproposal)
GATE RED
```

The implementation also still contains
`Unimplemented("D-130 selected-variant revival")` at `fold.rs:2869`,
while the coverage ledger keeps `f7-fork-selection` pending. The README's
claim that D-130 selection is implemented is therefore too broad.

**Repair:** require the exact named operation bytes at the named
coordinate or pend `ref-unresolved`. Then either:

- honestly defer full fork selection and remove the implementation claim;
  or
- complete it with two real signed variants, selector commitment, losing
  branch quarantine, selected-variant revival, and conflicting-selector
  rejection.

Do not retain the random-hash shortcut.

### F3 — D-202's re-proposal is not portable across its two ruled replica histories

**Classification:** Gate-A protocol blocker and owner-visible semantic
decision.

D-202 permits the original operation's verdict to depend on whether
timely or late evidence is held at first evaluation, then states that
convergence rides the writer's re-proposed operation. The committed
`f09-lease-lifecycle-sticky-reproposal` fixture proves only the
late-first world: in both listed deliveries, `late` precedes original
operation `i` (`core/src/corpus_time.rs:549–576`).

The fixture's `i` and “fresh” `i2` share lineage, generation 1, sequence
1, and generation-start predecessor (`corpus_time.rs:513–531`). That
coordinate is free only where `i` was rejected stale.

Using the same committed bytes, but making the second replica hold
`timely_i` before the first evaluation of `i`, returns exit 1:

```text
R1: c1,c2,c3,c4,late,i,timely_i,timely_i2,i2
R2: c1,c2,c3,c4,late,timely_i,i,timely_i2,i2

container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: listed delivery 1 diverges from delivery 0 —
the listed orders must share the declared evidence-arrival structure
GATE RED
```

The decisive detail is not that the original verdicts differ—that is
allowed. It is the carrier's result:

| Replica history | Original `i` | Re-proposal `i2` |
|---|---|---|
| Late-first | sticky `lease-stale` | admits at the freed sequence-1 coordinate |
| Timely-first | initially admits at sequence 1 | same-coordinate fork; both `i` and `i2` become `(fork, freeze-writer)` |

Thus the promised carrier does not admit in both worlds and the local
divergence remains visible in derived Memory state. The peer's test of
“every listed order” does not cover the second ruled world because the
builder deliberately lists only deliveries with the first world's
evidence structure.

**Repair:** return the carrier question to the owner. A same-lineage
sequence-1 retry cannot satisfy both histories; a sequence-2 retry cannot
follow the rejected sequence-1 history without another rule. Choose an
already-authorized portable carrier, change coordinate-consumption
semantics explicitly, or narrow the convergence promise. Then commit one
lifecycle test containing both replica histories and assert the derived
Memory result, not equality of the knowingly divergent original verdict.

### F4 — Portable-storage execution is real but the criterion-8 proof is incomplete

**Classification:** execution-evidence gap and criterion-11 overclaim;
Gate-A blocking if criterion 8 retains its present “every stream” and
“replacement proven” wording.

The peer is right about the current artifact: `durable_write` calls
`File::sync_all` and then `rename`, and ten present vectors traverse that
function on macOS, Linux, and Windows. Removing `rename` makes the lane
red because reads target the final path. Those are meaningful positives.

Three narrower defects remain:

1. The flush count is a manual increment after `sync_all`. Removing only
   the call while leaving `counters.0 += 1` produces 19/19 PASS and still
   prints `sync_all=10 rename=10`. The proof detects a zero counter, not
   bypass of the primitive.
2. Fourteen storage vectors carry `inputs.stream`, but only the ten that
   also carry `inputs.cuts` reach `durable_write`
   (`storage_lane.rs:115–124`). The four framing vectors use the ordinary
   recursive write/read path. Therefore the audit's “every stream” claim
   is false.
3. The final destination is absent before `rename`. This tests atomic
   publication, not replacement of an existing file.

**Repair or scope correction:** route all raw streams through the durable
abstraction, couple flush observation to a test seam or failpoint, and
pre-seed the destination with distinguishable bytes on all three OSes.
Alternatively, narrow criterion 8 and the audit to the evidence actually
provided. Power-loss ordering, directory fsync, keystores, and production
fault injection remain Gate B.

## Documentation and metadata truth

The peer correctly identifies the stale `p1-v1-profile.md` row and the
audit's confusing layering. Direct inspection finds additional current
drift, so criterion 11 is not “PASS modulo one row”:

- `README.md:19` and `:51` say 157 vectors; the corpus has 165.
- `README.md:22` says 22/59 uncovered and 14/25/43 obligations; current
  artifacts report 12 uncovered and 14 vectored / 26 partial / 42 pending
  / 2 structural.
- `README.md:82` overstates D-130 selection as implemented.
- `core/src/coverage.rs:7–10` says only the two Rust implementations
  execute the corpus, although browser and three storage OS lanes now do.
- `core/src/surfaces.rs:85–86` documents a non-empty subset rule while
  the test enforces exact equality.
- `p1-v1-profile.md:53` names the deleted
  `f07-revoke-cutoff-head-hash-mismatch-rejects` vector and says mismatch
  rejects, rather than distinguishing unheld exact references from held
  fork variants.
- `gate-a-audit.md:185–187` says body validation stays behind placement
  and attributes that inversion to D-99; its criterion 4 at `:472–473`
  says body comes first.
- `gate-a-audit.md:485–488` overclaims “every stream” and proven
  replacement.
- `execution-lanes-plan.md` records the 19-vector delivery but omits the
  later reference-lane flush/replacement claim; clarify it without
  weakening the explicit Gate-B production-durability boundary.
- PR #318's current body still describes 143 vectors, the old coverage
  counts, two Rust-only execution, and pre-ruling open items.

One earlier complaint should be **withdrawn**: the audit's
`records=37 bytes=30781` browser figures are explicitly attributed to
historical delivering commit `94848163`. They are accurate as a
historical citation; current-head figures are 45/40053.

## Peer-only findings adjudicated

- **Stale P1 profile row:** valid and included above.
- **Walkthroughs allegedly exempt from the metamorphic sweep:** not valid
  at this head. `run_walkthrough` calls `run_fold_vector`
  (`reducer/src/harness.rs:931–935`), and `run_fold_vector` calls
  `metamorphic_divergence` at line 1021. No additional wiring is needed.
- **D-202 stickiness disable-control is not committed:** valid test
  hardening. The current fixture and code make stickiness load-bearing,
  but a retained mutation/control test would guard it more explicitly.
- **Audit §5 layering is confusing:** valid documentation cleanup. The
  old clause annotations, withdrawn verdict, and later acceptance record
  should be flattened into one current statement before stamping.

## Repairs that verified cleanly — preserve these

- canonical, content-derived folding and pending-set resolution;
- all eight previously divergent orders plus broad generated order tests;
- a genuine arrival-order-restoration negative control;
- signature and body-hash checks before control placement;
- Journal invalidation authority derived from admitted control facts;
- audit partition completeness, disjointness, exact union, and split-Txn
  refusal at the declared fixture abstraction;
- exact, bidirectional browser/storage lane manifests;
- browser high-S rejection outside permissive WebCrypto behavior;
- Ed25519 unavailability failing rather than skipping the browser lane;
- real cross-process lock-denial negatives;
- strict semantic, empty-corpus, and non-permutation exit behavior;
- D-202's **local late-first stickiness**;
- current browser and three-OS storage execution.

## Residuals to record accurately, not inflate into this verdict

- Journal kill verification still depends on retained auxiliary operation
  bytes. Removing a redundant aux copy can lead to honest
  `Unimplemented`; no forged authority was demonstrated.
- Audit one-Txn membership is a declared release-input abstraction rather
  than replay from raw Txn bytes.
- Journal fixtures do not yet deeply exercise the normative
  `(frame ordinal, record index)` ordering, and the cut-branch
  invalidation arm lacks its own committed vector.

These are coverage/model boundaries. They should remain visible, but they
do not outrank the three executable protocol failures.

## Minimal path to a defensible PASS

1. **Complete D-99:** validate intrinsic arm-specific body CDDL before
   replay and placement; mint the two multi-fault regressions.
2. **Repair D-130:** require exact held bytes. Either implement the full
   two-real-variant selection lifecycle or defer it honestly; remove the
   random-hash selection.
3. **Resolve D-202 with the owner:** choose a carrier valid in both ruled
   histories, or narrow the convergence statement. Commit both worlds in
   one discriminating test.
4. **Close or narrow criterion 8:** make flush evidence discriminating,
   cover every raw stream, and test replace-existing—or state the smaller
   property actually proved.
5. **Run one truth pass:** repair the concrete README, source-comment,
   profile, audit, plan, and PR-body drift. Add the useful D-202 retained
   control while there.
6. Rerun the three red protocol traces and the storage negative as
   committed regressions, then request the genuinely final fresh review.

Do not turn this into another broad prose-only mechanism round. D-99 and
the exact-reference half of D-130 already have normative answers. D-202 is
the one bounded owner decision; if no existing carrier fits cleanly,
narrow that ruling rather than growing a new protocol family.

## Recommendation to the owner

Send PR #318 back for this bounded repair. Preserve the tranche's strong
work, keep the PR draft, and withhold the Gate-A stamp. The next review
should be small: the three committed counterexamples, the clarified
storage criterion, and a truth pass—not another general RFC round.
