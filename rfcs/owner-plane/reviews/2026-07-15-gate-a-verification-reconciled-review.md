# Reconciled Gate-A verification review — D0-A Core + Memory @ `80ccd04f`

**Date:** 2026-07-15  
**Target:** draft PR #318, branch `owner-plane-d0a`  
**Reviewed head:** `80ccd04fa66e7e8a282d5a5f4f18774c908954bc`  
**Specification:** v0.5.20, SHA-256 `ec3a9a6dda8f8c839b6c6eb7fb3322b439bf3976a8cd8ac0f6297838102dedef`  
**Companion:** SHA-256 `a3d6f779d30492978d6871b97d42037143f4a95c97256aaa92bf5aaa8be0f319`  
**Peer report reconciled:** `~/owner-plane-gate-a-verification-review-2.md`  
**Repository state:** read-only throughout this review; all mutations and order sweeps ran in scratch copies under `/tmp`; PR #318 was not changed.

## Verdict

**FAIL — do not stamp Gate A yet. Keep PR #318 in draft.**

The artifact program is substantial and mostly honest. The committed 157-vector corpus passes both implementations; the differential is structurally real; the semantic gate now exits nonzero on an ordinary tampered fixture; the Chromium lane genuinely executes WebCrypto plus its IndexedDB/Web-Locks substrate; and the portable-storage lane genuinely exercises files, corruption/truncation, and cross-process locking on the three operating systems. Those achievements reproduce and should be retained.

The peer review's principal finding is also correct: the reducer is not order-convergent. However, its conclusion that eleven of twelve Gate-A clauses verify is not supported by the complete executable evidence. Reconciliation found:

1. the peer's five convergence failures all reproduce;
2. two broader, independently designed order sweeps find **three additional divergent vectors**, for eight total;
3. the normative control pipeline still places operations before body validation in a path where v0.5.20 requires the opposite;
4. a signature-invalid recovery operation can verify a Journal reopen kill;
5. an incomplete audit partition can pass as exact;
6. both execution lanes can silently shrink through self-authored surface annotations, despite the review brief explicitly asking whether they can be green-washed;
7. the storage lane does not execute the required flush or atomic-replacement primitives; and
8. D-202's endpoint states are executable, but its ruled sticky-rejection/re-proposal lifecycle is not.

This is not a reason to reopen the RFC or resume prose-only design rounds. It is one bounded artifact-repair tranche. Most protocol choices already exist in v0.5.20; the implementation and executable coverage need to conform to them.

## Reconciliation of the peer review

### What the peer review establishes correctly

The following peer findings were independently reproduced or verified against the artifacts and are accepted:

- Baseline suites are green: core 140/140, reducer 35/35, strict harness 157/157, storage 19/19, browser 56/56.
- The two Rust implementations have no code dependency on one another; the differential is meaningful.
- Ordinary semantic tampering turns the strict harness red with exit 1.
- Stripping a committed convergence delivery turns the structural gate red.
- The Chromium high-S negative is discriminating. Bare WebCrypto accepts the high-S signature, while the lane's explicit low-S rule rejects it; flipping the expected result turns the lane red.
- Browser and storage lock-denial negative controls are behavioral, not labels.
- The erase-crash lane is de-oracled in the repaired sense: the signed manifest is the input, and the reducer derives the machine state and tombstones.
- D-201 is genuinely discriminating: the bare-writer judgments fold but remain status-inert.
- The five convergence failures in peer finding F1 are real.
- The peer's documentation corrections F2–F7 are substantially correct, including the wrong family histogram, stale executed-surface riders, missing convergence failure detail, unpinned lane counts, storage-lane header overstatement, and stale D-151 status prose.

### Where the peer review's conclusion is narrowed or overruled

The peer's evidence supports “the current listed corpus is green,” not “eleven of twelve clauses verify in substance.” In particular:

- Its convergence search considered the eleven vectors lacking a committed full reversal. Full reversal is not a sufficient probe: three additional vectors that already had reversals diverge under rotations or pair transpositions.
- Its D4 check proves the narrow signature-before-freeze arm only. It does not test the body-before-precedence rule that D-99 makes normative.
- Its reopen check reparses held bytes and verifies kind, lineage, and base. It does not authenticate the held recovery operation or prove it was admitted.
- Its audit-partition check verifies index bounds, duplicate indexes, and disjointness. It does not verify completeness, union with independently supplied released results, or the one-transaction release condition.
- Its statement that the lanes “cannot be green-washed” conflicts with its own F5 observation that annotations can silently disappear. An executable trace confirms the green-wash.
- Its evidence for “D-201/D-202 genuinely discriminate” demonstrates D-201 only. The D-202 source and decision record expressly say the receipt-arrival lifecycle is outside the current fixture lane.
- Its storage verification proves real files and locks. It does not prove flush or atomic replacement because the lane invokes neither primitive.

The score is therefore not usefully described as “only clause 2 fails.” At minimum clauses 2, 6, 8, and 10 fail in substance; clause 7 is only partially executed; and the narrow wording of clause 4 omits a known normative control-pipeline violation. A protocol freeze cannot safely ignore the latter merely because the audit predicate names only one arm of that pipeline.

## Clause-by-clause adjudication

| # | Gate-A clause | Reconciled status | Basis |
|---:|---|---|---|
| 1 | Strict gate | **Narrowly verified; hardening required** | A normal semantic red exits nonzero. However, an empty custom vector directory exits 0 silently, and the convergence structural rule accepts byte-distinct arrays that are not permutations of the item set. These are small gate-integrity defects. |
| 2 | Real convergence | **FAIL** | Eight committed vectors diverge on legal unlisted orders; six produce different durable reducer state. |
| 3 | D1 executable | **Verified** | Checksum-invalid BIP-39 rejection is minted and independently derived. |
| 4 | D4 executable | **Narrow D4 pair verified; governing pipeline nonconformant** | Signature-invalid input does precede freeze classification. Body validation does not precede placement as D-99 requires, and request-ID classification also precedes intrinsic validation. Do not treat the narrow pair as approval of the whole control pipeline. |
| 5 | D6 de-oracled | **Verified** | The erase-crash manifest path is no longer circular. |
| 6 | Reopen kill verified | **FAIL** | A recovery operation with an invalid signature is accepted as held kill evidence and makes the Journal trace pass. |
| 7 | No silent owner rulings | **Partially verified** | D-201 is executed. D-202 is recorded and its two endpoint evidence sets are executed, but sticky rejection, later evidence arrival, and re-proposal are not represented. |
| 8 | Machine-enforced coverage | **FAIL** | Audit partition “exactness” lacks completeness/union inputs; required surface membership can silently shrink; executed-surface coverage prose is stale. |
| 9 | CI visibility | **Verified with documentation correction** | All five advisory jobs exist and were green at the reviewed commits. Workflow comments lag the delivered lanes. |
| 10 | Execution lanes | **FAIL as the full §13.2 cell is written** | Chromium is substantive. Storage executes framing/truncation/locks but not flush or atomic replacement. Neither lane pins the exact required vector manifest. |
| 11 | P1 v1 profile | **Verified with wording correction** | The profile is ratified and the executable rows landed; clause 11 should not call the fifth C.1 row “vectored” when the profile calls it an internal replay invariant. |
| 12 | Owner rulings recorded | **Verified as a recordkeeping clause** | D-201..D-203 and the wire-gap disposition are recorded. This does not turn D-202's unexecuted lifecycle into executable evidence. |

## Findings

### R1 — Universal convergence fails in eight vectors

**Classification:** Gate-A blocker; protocol/reducer behavior, not a prose correction.

**Artifact anchors:** `owner-plane-d0a-spec.md:3090–3092`; `reducer/src/fold.rs:1782–1897, 2650–2684, 4347–4378`; `reducer/src/harness.rs:718–750`.

All five orders reported by the peer reproduce with the structural layers green, `semantics=FAIL`, and harness exit 1. A bounded sweep then tested 6,104 additional orders over all 72 convergence-bearing vectors:

- exhaustive permutations where the vector has at most six items;
- full reversal and cyclic rotations for larger vectors;
- every pair transposition from sorted order for larger vectors; and
- every committed order.

A second independently composed sweep used exhaustive small-vector permutations plus rotations, reverse rotations, and deterministic shuffled orders. Both sweeps found the same eight-vector set.

| Vector | Reproducing added order | Effect |
|---|---|---|
| `f10-tenant-same-seq-fork` | `[c1,c2,i2,i1]` | Different durable tenant head and admitted Memory claim. |
| `f07-second-live-compound-rejects` | `[r2,r1,c1,c2]` | Durable state is equal, but `r2` changes between permanent rejection and pending. This is real classification nondeterminism, not plane-state divergence. |
| `f07-pending-revocation-window-grant-completing-rotation` | `[k,g,r,c2,c1]` | Different durable control frontier and revocation/pending state. |
| `f07-staged-frontier-consumed-no-resurrection` | `[k2,k1,g4,rg,s,c1,c2]` | Different durable frozen/frontier state. |
| `f07-byte-identical-replay-duplicate` | `[c1,c2dup,c2]` | Underlying state is equal; only which fixture label receives the delivery-edge `duplicate` verdict changes. This is a comparator/convention defect. |
| `f07-c2-post-freeze-sig-invalid-kept` | `[c1,g4,x2,e2]` | Different durable control overlay/cut chain. |
| `f07-c2-post-freeze-valid-op-frozen` | `[c1,g4,x2,e2]` | Different durable control overlay and cut positions. |
| `f09-compromise-cutoff-retro-disqualifies` | `[k,r,c4,i,c3,c2,c1]` | Different durable control sequence, cutoff, and revoked/pending state. |

Six vectors therefore change durable reducer state, one changes an operation verdict with identical durable state, and one is purely a fixture-label artifact.

The immediate implementation causes are observable:

- `run_delivery_full` preserves pending arrival order and repeatedly calls a mutating classifier;
- pending compounds reserve and mutate control state before completion;
- same-coordinate tenant variants use first-accepted-wins;
- ordinary control admission can test chain placement before signature/body validity; and
- the harness compares only the named deliveries plus a name-sorted fresh fold.

The repair must be semantic, not “add eight fixture orders”:

1. make pending resolution set-derived or canonically control-sequence-derived rather than arrival-ordered;
2. perform intrinsic control validation before placement or pendency;
3. represent same-coordinate tenant variants as unresolved fork evidence, with both inert/frozen until the existing D-130 control-ordered selection resolves them—or surface a narrowly stated owner decision if D-130 cannot be implemented literally;
4. exclude or normalize delivery-edge duplicate labels when comparing semantic state; and
5. retain all eight traces as regressions plus a generated metamorphic convergence suite.

For small vectors, exhaustive permutation testing is cheap and should be mandatory. For larger vectors, require at least full reversal, rotations, racing-pair swaps, and deterministic bounded permutations. The implementation should also make the convergence argument structurally evident; testing alone cannot enumerate every large order.

### R2 — Control admission violates the normative stage order

**Classification:** Gate-A blocker independently of how narrowly audit clause 4 is worded.

**Artifact anchors:** `owner-plane-d0a-spec.md:2397–2437` and decision D-99 at `:3737`; `gate-a-audit.md:178–187`; `reducer/src/fold.rs:3026–3103, 4411–4462`.

The v0.5.20 control pipeline is explicit:

`parse → arm → signature → body → precedence/placement → remaining state invariants`.

It further says a validly signed header over malformed or body-hash-mismatched bytes exerts no precedence effect. The reducer's `ctrl_prevalidate`, by contrast, intentionally checks pins/arm/signature without body validity and runs the fork/placement gate first; its comment attributes that ordering to D-99 even though D-99 records the opposite resolution.

Reproducing trace:

1. Begin with `f07-c2-post-freeze-valid-op-frozen`.
2. Change one byte of `x2`'s body while retaining its signed header and signature. The carried `body_hash` therefore no longer binds the body, but the header signature remains valid.
3. Update only the scratch expectation to the normative first-failure result: `body-hash/reject-permanent`, without freezing the plane.
4. Run the harness.

Observed: the reducer classifies the challenger `ctrl-fork/freeze-control`; the corrected expectation turns the harness red. The malformed body affected precedence before the body stage ran.

A related trace reuses an accepted control operation's `request_id` in a signature-invalid operation. Because `request_seen` is consulted before control prevalidation, the reducer reports `request-fork` rather than `sig-invalid`. The spec scopes request-ID consumption to accepted operations and puts signature validation earlier.

Required repair: implement one control-admission pipeline with the normative stage order and make replay/request-ID effects transition-last. Add multi-fault vectors that distinguish body failure, signature failure, request reuse, placement conflict, and freeze. The two new convergence failures on the D4 vectors should be repaired by the same work, not patched separately.

### R3 — Journal reopen kill accepts unauthenticated recovery evidence

**Classification:** Gate-A blocker; clause 6 is not verified.

**Artifact anchors:** `owner-plane-d0a-spec.md:1359–1372`; `reducer/src/journal.rs:75–96, 473–528`.

The Journal reducer registers an aux operation when it parses and its body hash binds. It does not verify the operation signature, resolve the signer, or establish that the operation was accepted on the control chain. Reopen-kill verification later reparses those bytes and checks:

- operation kind is `c.recovery_succession`;
- invalidation and basis share the writer lineage; and
- the recovery base cuts below the basis sequence.

Those checks prove the claimed shape would kill the basis if authoritative. They do not prove the cited recovery is an authoritative fact.

Reproducing trace:

1. Begin with `f13-txn-internal-order-and-competing-terminals`.
2. Flip one byte in the recovery operation's signature.
3. Recompute its operation hash and update the reopen's fact reference so all hash bindings remain internally consistent.
4. Run the strict harness.

Observed: all layers, including semantics, PASS with exit 0. A signature-invalid recovery therefore kills the recorded basis.

Required repair: the invalidation reference must resolve to an authenticated, admitted fact—not merely a parsable, hash-bound operation-shaped byte string. Add at least these arms:

- valid accepted recovery cuts the basis;
- invalid recovery signature cannot kill it;
- validly signed but unadmitted/cut-branch recovery cannot kill it;
- unheld citation pends; and
- held authoritative recovery that keeps the basis produces the specified verified-false/log-corrupt result.

### R4 — Audit partition exactness is asserted from the answer sheet

**Classification:** Gate-A blocker; false machine-enforced coverage claim.

**Artifact anchors:** `owner-plane-d0a-spec.md:2540` and `:3452`; `reducer/src/fold.rs:3840–3905`; `reducer/src/harness.rs:630–657`; `d0a-vector-cases.v1.json:725–746`.

The spec requires one read's chunks to have indexes exactly `0..count−1`, disjoint result sets whose union equals the released results, and one transaction before release. The reducer checks per-row shape, `index < count`, shared fields, duplicate index, and disjointness. The harness then compares the reducer's surviving `(index,count)` pairs to the vector's own expected list. Neither implementation receives an independent released-result set or transaction/release boundary from which completeness can be derived.

Reproducing trace:

1. Begin with `f11-audit-partition-two-chunks`.
2. Remove chunk `a1` from items and every delivery.
3. Remove `a1` from `expected.result.chunks`.
4. Leave `a0` declaring `chunk { index: 0, count: 2 }`.

Observed: container, companion, pair, decode, and semantics all PASS; harness exits 0. A declared two-chunk partition with only one chunk is accepted as exact.

The correct repair is not to reject chunk zero merely because chunk one has not arrived yet. The executable model needs an independent read-release input:

- the released result IDs;
- the complete set of rows/transaction boundary presented for that read; and
- the release decision being evaluated.

At release, the reducer must derive completeness, exact union, disjointness, shared metadata, and one-transaction membership from those inputs. Add missing-middle, missing-last, extra-chunk, wrong-count, overlap, omitted-result, extra-result, and split-transaction negatives. Until that exists, the coverage ledger must not claim audit partition exactness as executable.

### R5 — Required surface membership is self-annotated and shrinkable

**Classification:** Gate-A blocker for lane integrity; current executions remain useful evidence.

**Artifact anchors:** `core/src/surfaces.rs:4–11, 69–97`; `browser-lane/driver.cjs:105–120`; `reducer/src/bin/storage_lane.rs:319–385`; `core/src/coverage.rs:102–103, 285–290`.

The current committed browser and storage sets did run. The defect is that nothing independently pins which vectors must run:

- surface validation accepts any nonempty subset of a family's allowed surfaces;
- the browser driver selects vectors from their mutable `surfaces` annotation and checks only that required families remain represented;
- the storage driver does the same and rejects only if no storage vector runs at all.

Reproducing browser green-wash:

1. Remove `browser` from the high-S rejection vector.
2. Change that vector's expected browser result to the knowingly incorrect `valid: true`.
3. Run the Chromium lane.

Observed: Chromium selects 55 vectors, reports 55/55 green, and exits 0. The one discriminating vector that proves the low-S pre-check can disappear without making the lane red.

The storage lane can likewise shrink from 19 vectors to one while the storage harness and core surface checks remain green.

Required repair: derive the required surface manifest from a single authoritative applicability table, or commit an exact generated manifest and parity-test it in both directions. At minimum the gate must fail on:

- a required vector losing a surface;
- a newly applicable vector missing that surface;
- a required vector disappearing entirely; and
- unexpected count/name drift.

Counts alone are insufficient, but pinning the current 56/19 names is a useful immediate guard. The generated coverage artifacts and workflow comments must then be regenerated to state the surfaces that actually execute.

### R6 — The storage lane does not execute flush or atomic replacement

**Classification:** Gate-A blocker under §13.2 and the funded execution-lane plan; alternatively requires an explicit owner scope reduction.

**Artifact anchors:** `owner-plane-d0a-spec.md:1453–1458, 3102`; `execution-lanes-plan.md:117–121`; `reducer/src/bin/storage_lane.rs:55–115`.

The storage lane honestly exercises:

- real file creation/write/read;
- truncation and corrupt-frame reads; and
- real cross-process advisory lock denial.

It does not call `sync_all`, `sync_data`, `fsync`, `fdatasync`, `rename`, `renameat`, or the Windows replacement equivalent. Its implementation uses ordinary write/read, `set_len`, and locks. Binary-symbol inspection on the reviewed Mac build likewise showed file/lock/truncate imports but no sync or rename primitive.

This is not a demand for Gate-B production crash injection or a proof of platform-specific fsync ordering. It is the narrower Gate-A observation that the §13.2 cell says `framing, flush, locks, crash/corruption`, and the funded plan names portable `open/write/rename/flock`; two named primitive classes never execute.

Required repair:

- execute at least one real successful file flush through the intended abstraction on every OS;
- execute the intended temporary-file-to-final-file replacement primitive on every OS;
- prove with a negative control or invocation counter that removing/bypassing either call turns its lane red; and
- leave power-loss ordering, directory-sync nuances, failure injection, and exhaustive crash matrices explicitly at Gate B.

If that was not the intended Gate-A boundary, return the scope choice to the owner. Do not call the full cell executed while silently narrowing “flush” and “rename” to ordinary writes.

### R7 — D-202's decision is recorded, but its lifecycle is not executable

**Classification:** execution-claim defect; Gate-A blocker if the predicate continues to say the ruling is executed/pinned.

**Artifact anchors:** decision D-202 at `owner-plane-d0a-spec.md:3840`; `core/src/corpus_time.rs:12–16`; `decisions-pending.md:179–203`; `gate-a-audit.md:190–202, 379–383`.

D-202 contains four observable propositions:

1. a held qualified receipt outside every valid window yields terminal `lease-stale`;
2. a timely receipt already held at first evaluation wins over late evidence;
3. later arrival of timely evidence does not revive the original stale operation; and
4. convergence occurs through a newly signed/re-proposed operation.

The current pair executes propositions 1 and 2 as separate endpoint fixtures. It does not execute 3 or 4. Receipts are static `aux` held before the fold; `core/src/corpus_time.rs` says receipt-arrival dynamics are outside the lane, while `decisions-pending.md` says the endpoint drafts deliberately do not encode the lifecycle.

The peer's cited bare-daemon/human comparison demonstrates D-201, not D-202.

Required repair: add a delivered-receipt/event lane or equivalent executable companion input showing:

1. original operation evaluates with only late evidence and becomes sticky stale;
2. timely evidence arrives later;
3. original operation remains stale; and

4. a distinct re-proposed operation admits under the now-held timely evidence.

If the owner deliberately accepts endpoint-only evidence, amend the audit claim to say exactly that and list the lifecycle as a carried-open executable gap. Do not call it “genuinely discriminating” or “executed” as currently written.

### R8 — Smaller gate-integrity and documentation repairs

**Classification:** not individually sufficient to reject the architecture, but repair before the next Gate-A claim.

**Artifact anchors:** `reducer/src/harness.rs:151–177, 818–826`; `coverage/outcomes-map.json:200–204`; `core/src/coverage.rs:102–103`.

1. **Empty corpus is vacuously green.** Running the strict harness on an empty custom directory exits 0 with no output because `all_green([])` is true. Require at least one vector and, for the committed run, the exact corpus manifest.
2. **Delivery arrays need permutation validation.** The structural rule checks byte-distinct arrays but does not require each delivery to contain every item exactly once. A delivery with a duplicated item can satisfy the rule. Enforce set equality and multiplicity one.
3. **Convergence failure output is hidden.** The harness detects the failure but the CLI omits the convergence layer's per-vector reason. Print it.
4. **Audit histogram is stale.** Actual counts are f07×27 and f11×29, not f07×26 and f11×30.
5. **D-203 attribution is one saga too broad.** Four sagas are directly named, not five.
6. **Clause 11 overstates C.1 row five.** It is an internal replay invariant, not a vector.
7. **Executed-surface riders are false.** Coverage JSON and workflow comments still say no browser or per-OS storage lane executed.
8. **Browser clippy is not CI-enforced.** Correct the audit statement.
9. **Storage-lane header overstates read-back substitution.** Code runs semantics over the original vector after equality gates.
10. **Ed25519 exclusion rationale is over-broad.** Its signing is deterministic; keep the P-256 randomness limitation scoped to P-256.
11. **D-151 status prose is obsolete.** Update it at freeze preparation.

## What should not be reopened

This review does not revisit:

- Gate-B production fsync ordering or power-loss injection;
- production keystores and custody;
- IndexedDB eviction/failure injection;
- Firefox/Safari execution;
- D11's deliberately recorded-open offline-expiry confirmation;
- the §4.7 fork-discovery wire gap shelved for v1; or
- the prohibition on P1 durable writes before Gate B.

It also does not call for a new prose mechanism for each order trace. The main repairs are faithful execution of already-ratified semantics: deterministic folding, the D-99 pipeline, authenticated fact references, and exact audit release.

## Bounded repair tranche

### A. Reducer determinism and control admission

1. Replace arrival-ordered mutating pending resolution with canonical/set-derived re-folding.
2. Remove first-accepted-wins tenant-fork semantics in favor of D-130 selection/freeze behavior.
3. Implement the normative intrinsic-validation and transition-last pipeline, including request-ID consumption.
4. Normalize delivery-only duplicate labels out of semantic convergence comparison.
5. Mint all eight discovered orders and add a generated metamorphic order suite.

### B. Journal and audit exactness

1. Resolve reopen invalidations only through authenticated/admitted facts.
2. Add the forged-signature and unadmitted-recovery negatives.
3. Add independent released-result and transaction/release inputs for audit partitions.
4. Add completeness and exact-union negatives.

### C. Execution-lane integrity

1. Derive or pin exact browser/storage manifests.
2. Add real portable flush and replacement calls with discriminating controls.
3. Add the D-202 evidence-arrival and re-proposal trace, or return its executable scope to the owner explicitly.

### D. Gate hygiene and truthfulness

1. Reject an empty corpus.
2. Require every delivery to be a true permutation.
3. Print convergence failure details.
4. Regenerate coverage artifacts and apply the accepted peer documentation corrections.

## Acceptance criteria for the next review

Do not present another “predicate satisfied” claim until all of the following hold at one pinned commit:

1. Core, reducer, strict harness, browser, and all three storage lanes are green.
2. The eight orders above are committed as regressions and a generated convergence suite is CI-visible.
3. The convergence suite fails under a deliberate restoration of arrival-order pending processing or first-accepted-wins tenant selection.
4. A body-hash-mismatched but validly signed control operation cannot freeze or affect precedence.
5. A signature-invalid or unadmitted recovery cannot verify a Journal reopen kill.
6. An incomplete audit partition cannot release results or report exactness.
7. Removing the browser annotation from the high-S vector, or any required storage annotation, turns the corresponding lane red.
8. The storage lane proves that its flush and atomic-replacement primitives actually ran on macOS, Linux, and Windows.
9. The D-202 lifecycle is either executable end-to-end or accurately recorded as an owner-approved exception rather than claimed as executed.
10. Empty-corpus and non-permutation delivery negative controls are red.
11. Coverage ledgers, workflow comments, corpus counts, and Gate-A prose match the delivered artifacts.
12. A fresh independent reviewer reruns the gate and the bounded order suite from the pinned commit and reports no executable finding.

## Final recommendation to the owner

Send PR #318 back for the bounded repair tranche above. Do not stamp Gate A, do not move the PR out of draft, and do not reopen broad protocol prose. The reference program is close enough that a focused implementation pass is the right response, but the remaining defects sit precisely on the properties Gate A is meant to freeze: deterministic folding, precedence, authenticated evidence, audit exactness, and honest execution-surface coverage.
