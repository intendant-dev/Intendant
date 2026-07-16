# Independent Gate-A repair verification — D0-A Core + Memory @ `2f66b592`

**Date:** 2026-07-15  
**Target:** draft PR #318, branch `owner-plane-d0a`  
**Reviewed head:** `2f66b5926258bcb0d90d0ffd62445abae43cd41d`  
**Review scope:** the seven commits after the previously reviewed `80ccd04f`, i.e. `80ccd04f..2f66b592` (`52b9b8e5` through `2f66b592`, inclusive)  
**Specification:** v0.5.20, SHA-256 `ec3a9a6dda8f8c839b6c6eb7fb3322b439bf3976a8cd8ac0f6297838102dedef`  
**Companion:** SHA-256 `11dd88972220cac3a120f6f729c9b3eb9cd9e6a9a332bff75b4765efd178aaba`  
**Method:** source review, local reproduction, independent order sweeps, scratch-fixture mutations, and CI-log verification. The repository worktree and PR were not changed; all mutations lived under `/tmp`.

## Verdict

**FAIL — do not stamp Gate A. Keep PR #318 in draft.**

The repair tranche closes a great deal of the prior review correctly. In particular, the canonical set-derived fold is convincingly order-stable; the earlier eight divergent orders are fixed; the Journal now bases reopen authority on admitted control facts; audit partition completeness and exact union are independently derived; lane manifests resist annotation shrinkage; the strict gate rejects empty and malformed delivery corpora; and all five owner-plane CI jobs are green at the reviewed head.

Four substantive findings remain:

1. D-99 still validates only the body hash and registry row before replay/placement; operation-specific body CDDL remains later, so signed malformed control bodies can exert precedence or request-ID effects.
2. The new D-130 fixture and reducer select an arbitrary **unheld** hash, despite the spec requiring an exact referenced Head to pend until its bytes arrive.
3. D-202's re-proposal works only on the late-first replica. On the timely-first replica, the original already occupies the same writer coordinate, so the re-proposal freezes both operations as a fork rather than carrying convergence.
4. The storage lane does call `sync_all` today, but its claimed invocation proof is non-discriminating, four stream-bearing vectors bypass the durable path, and it never exercises replacement of an existing destination.

The first three sit directly on protocol semantics. The fourth means the prior review's execution-lane acceptance criterion is not yet met as claimed. Documentation and PR metadata also still disagree with the delivered artifacts, so the truthfulness criterion is red independently.

This is still a bounded send-back, not a reason to reopen broad RFC design. D-99 and the unheld-Head rule already have explicit normative answers. D-202 is the one item that needs an owner-visible semantic choice if no existing portable retry carrier can satisfy the ruling.

## Reproduction record

The following baseline claims reproduced at the pinned head:

| Check | Result |
|---|---|
| Core suite | 141/141 PASS |
| Reducer release suite | 36/36 PASS |
| Strict release harness | 165/165 PASS, exit 0 |
| Portable-storage lane | 19/19 PASS locally; current output reports `sync_all=10 rename=10` |
| Chromium lane | 56/56 PASS in the head CI run; high-S and manifest negative controls independently reproduced red |
| Additional convergence sweep | 62,280 legal orders over every >5-item convergence vector; zero divergence and zero `Unimplemented` |
| Empty custom corpus | exit 2 |
| Non-permutation delivery | exit 1 with an explicit structural error |
| PR checks | all checks green at exact head, including Chromium and all three storage OSes |
| Worktree | clean before and after review |

Green baseline suites do not contradict the findings below: each finding is a legal case or property the committed corpus does not currently express.

## Acceptance-criteria adjudication

This table uses the twelve criteria in the reconciled review, not the audit's self-report.

| # | Acceptance criterion | Independent result |
|---:|---|---|
| 1 | All suites green at one pin | **Verified.** |
| 2 | Eight regressions committed; generated convergence suite CI-visible | **Verified.** The canonical fold also survived the independent 62,280-order sweep. |
| 3 | Convergence suite discriminates against arrival-order restoration | **Verified.** The retained restoration control is non-vacuous. |
| 4 | Body-hash-mismatched control bytes cannot affect precedence | **Literal hash arm verified, governing D-99 repair FAILS.** A hash-valid but CDDL-invalid signed body still affects precedence and replay classification. |
| 5 | Forged/unadmitted recovery cannot verify a Journal kill | **Verified on the committed arms.** Authority now comes from the admitted control fold. A delivered accepted recovery still needs a redundant aux byte copy; see residuals. |
| 6 | Incomplete audit partition cannot release | **Verified at the companion's declared read-release/Txn-set abstraction.** Physical Txn carriage is not modeled. |
| 7 | Required annotation loss reddens each lane | **Verified.** Exact generated manifests catch both browser and storage shrinkage. |
| 8 | Storage proves flush and atomic replacement ran on all OSes | **FAIL as claimed.** Current source invokes flush, but the negative proof can stay green without it; durable coverage is incomplete; replace-existing is untested. |
| 9 | D-202 lifecycle executable end to end, or accurately excepted | **FAIL.** The complementary ruled replica history makes both original and carrier fork-frozen. |
| 10 | Empty-corpus and non-permutation controls red | **Verified.** |
| 11 | Ledgers, comments, counts, and prose match the artifacts | **FAIL.** Several current files and the PR body remain stale or internally contradictory. |
| 12 | Fresh independent reviewer reruns and reports no executable finding | **Review performed; findings remain.** This criterion cannot produce PASS. |

There is also a blocker outside the narrow wording of those criteria: the tranche's new D-130 exact-Head behavior contradicts the frozen spec and its own coverage ledger.

## Findings

### F1 — D-99 remains incomplete: body CDDL still follows replay and placement

**Classification:** Gate-A blocker; normative conformance failure.

**Normative anchors:** `owner-plane-d0a-spec.md:2397–2437`, especially `:2411–2426`; D-99 at `:3737`.

The required control order is:

```text
parse → arm → signature → body(hash + registry + CDDL)
      → precedence-field validity → placement/precedence → state
```

At `reducer/src/fold.rs:4930–4961`, the implementation performs:

```text
arm/signature → body_hash → registry → request-ID consult → placement
```

Operation-specific body shape/CDDL is not checked there. It remains inside the arm-specific `admit_*` function after placement, for example the required `grant` member at `fold.rs:1253–1259`.

#### Executable trace A: malformed signed body can freeze control

In a scratch copy, the existing post-C2 `g4` operation was re-sealed with the real root key and body `{bogus: 1}`. Its signature and `body_hash` are therefore both valid, but it is not a valid `c.grant` body. Only its expected first-failure classification was changed to `(body-invariant, reject-permanent)`.

Running the head reducer over that one-vector corpus produced exit 1:

```text
f07 f07-c2-post-freeze-valid-op-frozen.json
  container=ok companion=ok pairs=ok decode=ok convergence=ok
  semantics=FAIL: g4: expected Some(("body-invariant", "reject-permanent")),
  reducer derived Some(("ctrl-fork", "freeze-control"))
GATE RED
```

Scratch artifact: `/tmp/d0a-d99-vector.WTzK6y`.

#### Executable trace B: malformed signed body can win replay classification

A second scratch fixture used the same correctly signed, hash-valid invalid `c.grant` body while reusing an accepted `request_id`. The expected first failure remained `body-invariant`. The reducer derived:

```text
expected body-invariant/reject-permanent
derived  request-fork/reject-permanent
```

Thus the replay consult also precedes complete body validity.

The audit is internally contradictory on this point. `gate-a-audit.md:185–187` says the body stage deliberately stays behind placement and attributes that to D-99; `:472–473` says the pipeline validates body before placement. The spec unambiguously says the latter and includes CDDL in the body stage.

**Required repair:** introduce one arm-indexed intrinsic body-shape validation stage before both `request_check` and `ctrl_fork_gate`. Keep state-dependent body invariants transition-last. Mint multi-fault vectors for at least CDDL-invalid × C2 and CDDL-invalid × consumed request ID. Do not weaken D-99 to mean only `body_hash`.

### F2 — D-130 commits an unheld, arbitrary Head hash

**Classification:** Gate-A blocker; exact-reference and fork-selection semantics.

**Normative anchors:** `owner-plane-d0a-spec.md:1773–1810`, especially the referenced-Head lifecycle at `:1788–1792`; D-130 at `:3768`.

The newly re-authored committed fixture `f07-revoke-cutoff-head-mismatch-selects` does not carry two byte variants at one coordinate. Its builder draws 32 random bytes named `wrong.head.op` (`core/src/corpus_ctrl.rs:697–748`, draw at `:721`) and puts that random hash in the boundary. No operation with that hash exists in `items` or held context.

Nevertheless, `parse_heads` (`reducer/src/fold.rs:1417–1448`) finds **any** held operation at the same coordinate and records the boundary's arbitrary `hop` as the selection. The boundary admits and the actually held operation is quarantined.

The spec says the opposite: a boundary naming a tenant Head not yet held is `ref-unresolved/pending-dependency` until the exact bytes arrive.

#### Executable trace

Using the committed bytes unchanged, the scratch expectation was corrected to the normative state:

```diff
- i = cutoff / quarantine-reproposal
- r = admitted
+ i = admitted
+ r = ref-unresolved / pending-dependency
```

Running the head reducer produced exit 1:

```text
f07 f07-revoke-cutoff-head-mismatch-selects.json
  container=ok companion=ok pairs=ok decode=ok convergence=ok
  semantics=FAIL: i: expected None,
  reducer derived Some(("cutoff", "quarantine-reproposal"))
GATE RED
```

Scratch artifact: `/tmp/d0a-d130-trace`.

This is not merely a misleading fixture name. The reducer can commit a selection for bytes it has never authenticated or even seen. The same tranche still contains `Unimplemented("D-130 selected-variant revival")` at `fold.rs:2869`, and `coverage/obligations-13-3.json` keeps `f7-fork-selection` pending, while `README.md` claims D-130 fork selection is implemented.

**Minimum repair:** require the named `op` hash itself to resolve to exact held bytes at the coordinate; otherwise pend. Then choose one of two honest scopes:

- keep exact fork selection deferred, re-mint this fixture as the unheld-Head pending arm, and remove “D-130 implemented” claims; or
- implement the complete mechanism using two real signed byte variants, a selecting boundary, losing-suffix quarantine, selected-variant revival, and later-conflicting-selector rejection.

Do not retain the random-hash shortcut.

### F3 — D-202's re-proposal does not converge across the two ruled replica histories

**Classification:** Gate-A blocker; owner-visible semantic gap exposed by a fixture omission.

**Normative anchors:** T5 at `owner-plane-d0a-spec.md:922–938`; D-202 at `:3840`; selected alternative in `decisions-pending.md:214–221`.

D-202 deliberately allows the original operation's verdict to differ by evidence arrival order, but says convergence rides a re-proposed operation that both replicas admit. The committed lifecycle fixture never tests that proposition. Both listed orders keep the late receipt before original operation `i` (`core/src/corpus_time.rs:549–576`).

Worse, the original `i` and “fresh” re-proposal `i2` use the same lineage, generation 1, sequence 1, and generation-start predecessor (`corpus_time.rs:513–531`). That coordinate is free only on the late-first replica.

#### Executable cross-replica trace

The bytes and expected result were left unchanged. Only the second delivery was changed to place `timely_i` before `i`:

```text
R1: c1,c2,c3,c4,late,i,timely_i,timely_i2,i2
R2: c1,c2,c3,c4,late,timely_i,i,timely_i2,i2
```

The head harness returned exit 1 with every structural layer green:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: listed delivery 1 diverges from delivery 0 —
the listed orders must share the declared evidence-arrival structure
GATE RED
```

Scratch artifact: `/tmp/d0a-d202-cross-replica.t3L5nI`.

An R2-only observation makes the state explicit:

```text
i:  (fork, freeze-writer)
i2: (fork, freeze-writer)
```

The two actual worlds are therefore:

| Replica history | Original `i` | Re-proposal `i2` |
|---|---|---|
| Late receipt held at first evaluation | sticky `lease-stale` | admits at the freed sequence-1 coordinate |
| Timely receipt held at first evaluation | initially admits at sequence 1 | creates a same-coordinate fork; both freeze |

The permitted local divergence in the original verdict is consequently visible in derived Memory state. The committed vector demonstrates the local R1 lifecycle, not cross-replica convergence.

The harness contributes to the blind spot: `reducer/src/harness.rs:703–755` requires the full final verdict maps to match and therefore intentionally lists only orders with one evidence-arrival structure. The property D-202 needs is different: allow the original verdict to differ, while requiring the convergence carrier to admit and derived Memory state to agree.

**Required decision and repair:** identify a portable carrier admissible in both histories. A same-lineage sequence-1 retry cannot satisfy that; sequence 2 cannot satisfy the late-first history because the rejected original did not advance the chain. Plausible directions include an explicitly authorized fresh lineage, an existing D-130 selection ceremony over a real re-proposal, or revisiting whether sticky rejection consumes/reserves a chain coordinate. If none is intended, withdraw the convergence claim. Record the owner choice, implement it, and commit both replica histories in one discriminating lifecycle test.

### F4 — The storage lane's flush/replacement proof is not yet discriminating

**Classification:** Gate-A execution-evidence blocker under acceptance criterion 8; bounded implementation work.

This finding does **not** claim that the current binary never calls `sync_all`: it does, ten times in the present corpus. The defect is that the asserted proof can stay green when flush is bypassed, and the durable path covers less than the prose claims.

At `reducer/src/bin/storage_lane.rs:100–109`, the counter is a manual increment after the call. Removing only:

```rust
f.sync_all().map_err(|e| format!("sync_all: {e}"))?;
```

while leaving `counters.0 += 1` unchanged produced:

```text
19/19 PASS
storage lane: 19 vector(s) executed on real files (sync_all=10 rename=10)
exit 0
```

Scratch artifact: `/tmp/d0a-storage-audit.lQjbny`.

Removing only `rename` does turn ten vectors red because reads target the final path. Rename is genuinely load-bearing; flush is not independently observed by the lane.

There are two additional scope mismatches:

- Fourteen storage vectors contain `inputs.stream`, but `truncate_cuts` calls `durable_write` only when both `stream` and `cuts` exist (`storage_lane.rs:115–124`). Four framing/corruption streams therefore use only the ordinary `std::fs::write` path at `:64–84`. The audit's “every stream materializes” claim is false.
- `durable_write` renames onto a destination that does not yet exist. That exercises atomic installation/publication, not replacement of an existing final file, despite the review criterion and code comments calling it atomic replacement.

**Required repair:** route every stream through the durable abstraction; make the flush/rename observations derive from a seam or failpoint whose bypass is covered by a negative unit/control test; and pre-seed the destination with distinguishable bytes so the 3-OS lane proves replace-existing behavior. Keep exact power-loss ordering, directory-sync subtleties, and production failure injection at Gate B.

### F5 — Truthfulness and metadata still lag the tranche

**Classification:** criterion-11 failure; cheap, but required before review-ready status.

Concrete current mismatches:

- `gate-a-audit.md:185–187` states the opposite of D-99 and contradicts the same file's acceptance-criterion claim at `:472–473`.
- `gate-a-audit.md:485–488` says every stream uses the durable path and calls sync load-bearing; neither statement is true as tested.
- The audit still records old browser substrate aggregates (`records=37 bytes=30781`); the head CI log reports `records=45 bytes=40053` (frames 72, cuts 11).
- `README.md:19` and `:51–52` say 157 vectors; the current corpus is 165.
- `README.md:22` still says 22/59 uncovered and 14/25/43 obligation states; current audit truth is 12 uncovered and 14/26/42.
- `README.md:82` says D-130 fork selection is implemented while the exact-selection obligation is pending and selected-variant revival is explicitly unimplemented.
- `core/src/coverage.rs:7–10` still says only the two Rust implementations execute and that the map states this; current artifacts list six executed surfaces.
- `core/src/surfaces.rs:85–86` still describes a nonempty subset although code now requires exact equality.
- `p1-v1-profile.md:53` names the deleted `f07-revoke-cutoff-head-hash-mismatch-rejects` fixture rather than the current file.
- `execution-lanes-plan.md:142–148` does not record the newly claimed flush/replacement work.
- PR #318's body is from an older tranche: it says 143 vectors and describes unresolved items that have since changed. It should be rewritten before the PR is marked ready.

## Repairs that verified cleanly

The send-back should preserve these parts:

- **Canonical convergence:** all eight prior failing orders are committed and green. The generated suite and independent 62,280-order sweep found no new divergence. Freeze-both on the tenant same-coordinate fork passes all 24 permutations. The arrival-order restoration control is discriminating.
- **Gate integrity:** semantic red exits nonzero; empty corpora and non-permutation deliveries fail; convergence error detail is visible.
- **Journal authority:** forged recovery cannot kill; a held-but-unadmitted recovery pends; accepted recovery that keeps the basis rejects; accepted recovery that cuts the basis verifies. Decisions key on content hashes, not fixture labels.
- **Audit release:** index completeness, disjointness, and exact result union are derived from the independent read-release input. The prior missing-chunk trace is now red.
- **Lane manifests:** dropping the high-S browser annotation or a required storage annotation turns the appropriate lane red. The high-S check remains genuinely WebCrypto-discriminating.
- **CI:** the release-profile change solved the earlier timeout, and all five owner-plane jobs are green at the reviewed head.
- **D-202 local stickiness:** on the late-first replica, the stale verdict remains sticky when timely evidence later arrives; the defect is specifically the promised cross-replica carrier.

## Residuals to record accurately, not inflate into this verdict

Two narrower model limits surfaced during review:

1. The Journal authority decision is correct, but accepted control bytes delivered as an item are not sufficient for later kill parsing. Removing the redundant `aux["recovery.op"]` copy yields an honest `Unimplemented("reopen kill verification needs the invalidation bytes held")`. Retrieve accepted bytes from the fold's retained control log, or document and schema-require the duplicate. The cut-branch invalidation arm is implemented but lacks its own committed vector.
2. Audit one-Txn membership is checked against the independent `inputs.release.txn_rows` event, not raw Txn bytes. That is acceptable only as an explicit fixture abstraction; do not describe it as physical Txn carriage. Likewise, Journal semantic fixtures lack frame ordinals even though the normative journal order is `(frame ordinal, record index)`; the current reducer uses content-hash order for frames. These are coverage/model boundaries to carry honestly.

Neither residual weakens the four executable blockers above.

## Minimal path to the next review

1. Complete D-99's intrinsic body stage before replay and placement; mint the two multi-fault controls.
2. Fix exact-Head resolution. Either restore honest `ref-unresolved` behavior and defer full D-130, or implement selection over two real held variants through revival.
3. Return D-202's carrier question to the owner with the cross-replica trace, then execute the chosen portable carrier in both worlds.
4. Make storage flush proof discriminating, cover every stream, and exercise replace-existing.
5. Fix the listed audit/README/code-comment/profile/PR metadata drift and regenerate the current browser aggregates.
6. Rerun the same pinned suites and the four traces. Request another independent review only after each trace has become a committed negative/regression.

Do not broaden this into another large prose mechanism round. If D-202 cannot reuse an existing carrier cleanly, narrow or amend that one ruling explicitly rather than adding unrelated protocol machinery.

## Recommendation to the owner

Send PR #318 back for this bounded repair. Keep it draft and do not stamp Gate A. The canonical-fold, Journal-authentication, audit-release, manifest, and gate-integrity work is good and should stand; the remaining work is concentrated in complete control-stage ordering, exact Head resolution, a real cross-replica D-202 carrier, honest storage evidence, and documentation truth.
