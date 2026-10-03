# Independent Gate-A verification — D0-A Core + Memory @ `ff23f1cd`

**Date:** 2026-07-16  
**Target:** draft PR #318, branch `owner-plane-d0a`  
**Reviewed head:** `ff23f1cd7bd2db709e2ef9030d7bcd2ef5f3fc16`  
**Review range:** `2f66b592..ff23f1cd`  
**Specification:** v0.5.21, SHA-256 `5ca12fe7a049ea223130c470e3b1234ad2b96e90f4b54c792e31d7dc1de4909a`  
**Companion:** SHA-256 `11dd88972220cac3a120f6f729c9b3eb9cd9e6a9a332bff75b4765efd178aaba`  
**Audit:** SHA-256 `1743b6487acbefa17158e5f26c9a9f62cacb3e994b129922a6df8e6078ecaaf9`  
**Method:** source inspection, local suite and lane reproduction, scratch-fixture mutations, order perturbation, mutation controls, and exact-head CI inspection. The reviewed worktree and PR were not changed.

## Scope calibration

This is a review of a pre-alpha development artifact against its own Gate-A predicate. It is not a production-readiness, deployment-security, or operational-hardening audit.

Power-loss ordering, directory fsync, keystore integration, production browser eviction/failure injection, Firefox/Safari, quota behavior, and the other recorded Gate-B concerns remain out of scope. A finding below is a blocker only where the current Gate-A text itself claims the behavior or executable proof.

## Verdict

**FAIL — do not stamp Gate A yet. Keep PR #318 in draft.**

The repair tranche closes important prior findings correctly:

- the D-130 unheld-Head shortcut is gone and now pends honestly;
- both owner-ruled D-202 evidence worlds are committed and genuinely discriminating;
- the current storage implementation routes all fourteen streams through the durable path and performs real replacement;
- the browser teardown fix preserves the semantic verdict;
- all committed suites and all five owner-plane CI jobs are green at the exact pin.

Four Gate-A criteria nevertheless remain red:

1. **Criterion 4:** the D-99 “complete body stage” is still not complete. Closed-CDDL unknown fields pass, and `operation_version` is not part of registry dispatch.
2. **Criterion 8:** the current storage implementation is correct by source inspection, but its executable proof does not establish everything the audit says it proves.
3. **Criterion 9:** D-204 still diverges within its newly narrowed late-evidence class when the re-proposal is delivered before the original operation.
4. **Criterion 11:** several live documents and the PR description still describe an earlier state.

Criterion 12—the fresh independent review—has now occurred, but findings remain, so it cannot close the gate.

This is a bounded repair round. It does not justify reopening broad RFC design or adding another large family of prose mechanisms.

## Reproduction record

| Check | Independent result |
|---|---|
| Reviewed pin | clean worktree at exact `ff23f1cd7bd2db709e2ef9030d7bcd2ef5f3fc16` |
| Core suite | 141/141 PASS |
| Reducer release suite | 37/37 PASS |
| Strict differential harness | 168/168 PASS, exit 0 |
| Portable-storage lane | 19/19 PASS; `sync_all=14 rename=14`; plain/failpoint control behaves as committed |
| Chromium lane | 56/56 PASS under local Headless Chrome 150; f13 substrate `records=45 bytes=40053 frames=72 cuts=11` |
| CI | all five owner-plane jobs green at exact head: reference artifacts, Chromium, and macOS/Linux/Windows storage |
| Arrival-order restoration control | PASS: the retained old engine diverges and the canonical engine converges |
| D-202 cross-world unit | PASS and non-vacuous |
| Empty corpus | exit 2 |
| Non-permutation delivery | exit 1 with an explicit structural error |
| Worktree after review | clean |

The green committed corpus does not contradict the findings. Each semantic finding below is a companion-valid scratch vector whose structural layers remain green and whose expected result is derived from current normative text.

## Acceptance-criteria adjudication

| # | Gate-A criterion | Independent result |
|---:|---|---|
| 1 | All suites green at one pinned commit | **Verified.** |
| 2 | Eight regressions committed plus generated convergence suite | **Verified for the ordinary convergence-bearing lanes.** |
| 3 | Convergence suite discriminates against arrival-ordered restoration | **Verified.** |
| 4 | Complete D-99 body stage precedes replay and placement | **FAIL.** Closed body maps and semantic operation-version dispatch are not enforced. |
| 5 | Forged or unadmitted recoveries cannot verify a Journal kill | **Verified at the committed abstraction.** |
| 6 | Incomplete audit partitions cannot release | **Verified at the companion’s declared read-release abstraction.** |
| 7 | Required annotation loss reddens each lane | **Verified.** Exact surface sets and bidirectional manifests are load-bearing. |
| 8 | Storage flush/replacement is proven as claimed | **FAIL as a proof predicate.** Current code performs the operations, but three claim-removing mutations stay green. |
| 9 | D-202 lifecycle is executable in both ruled worlds under D-204 | **FAIL.** A third order within the late-first class diverges. |
| 10 | Empty-corpus and non-permutation controls red | **Verified.** |
| 11 | Ledgers, comments, counts, prose, and PR metadata match | **FAIL.** Concrete stale claims remain. |
| 12 | Fresh independent reviewer reruns the gate | **Performed; findings remain.** |

## Findings

### F1 — D-204 does not converge within its narrowed late-evidence class

**Classification:** Gate-A protocol blocker under criterion 9.

**Normative anchors:** T5 at `owner-plane-d0a-spec.md:926–953`; D-204 at `:3857`; evidence-lifecycle harness at `reducer/src/harness.rs:703–765`.

D-204 narrows the convergence promise to replicas holding the same qualified-evidence class when the original operation first evaluates. For the late-first class, it states that the original receives sticky `lease-stale` and the re-proposal admits at the freed coordinate.

The committed late-first deliveries both place original `i` before re-proposal `i2`. Reversing only that relationship exposes another outcome while preserving the original operation’s late-first evidence class:

```text
R1: c1,c2,c3,c4,late,i,timely_i,timely_i2,i2
R2: c1,c2,c3,c4,late,timely_i2,i2,i,timely_i
```

When `i` first evaluates in both histories, `late` is held and `timely_i` is not. These therefore share the exact evidence class D-204 names. The strict harness returns exit 1:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: listed delivery 1 diverges from delivery 0 —
the listed orders must share the declared evidence-arrival structure
GATE RED
```

Scratch artifact: `/tmp/d0a-ff23-d204-order.cIO4S5`.

Isolating R2 shows the actual state:

```text
i:  expected lease-stale/quarantine-reproposal
    derived  fork/freeze-writer

i2: expected admitted
    derived  fork/freeze-writer
```

The cause is straightforward: `i2` occupies the coordinate before `i` arrives. The later original is treated as D-130 fork evidence before receiving its late-evidence classification. No current protocol rule requires delivery of the original before its re-proposal.

This also leaves two prose/schema inconsistencies:

- §13.1 still says a conforming reducer converges “on every order” with no evidence-lifecycle exception (`owner-plane-d0a-spec.md:3105–3107`).
- The companion has only `arrival_is_semantic: true`; it contains no machine-readable declaration of the evidence-arrival structure it says listed deliveries share (`d0a-vector-cases.v1.json:2711–2739`).

**Required repair:** make the late-class result independent of original-versus-re-proposal delivery order, then commit this order as a regression. A narrow implementation direction is to ensure the original’s already-held late evidence classifies before same-coordinate fork placement; an explicit signed dependency from re-proposal to original could also serialize the relationship. If the owner instead intends relative original/re-proposal delivery to be part of “structure,” D-204 must say so explicitly—but that would narrow the convergence claim much further and should not be smuggled in as a harness convention.

### F2 — D-99 still accepts unknown fields in nominally closed body maps

**Classification:** Gate-A protocol blocker under criterion 4.

**Normative anchors:** O3 at `owner-plane-d0a-spec.md:575–580`; `cgrant = { grant: grant }` at `:4261`; the claimed complete precheck at `reducer/src/fold.rs:3296–3317`.

The new `ctrl_intrinsic_shape` stage is a real improvement: required members, coarse types, selected caps, and several byte-internal equalities now precede replay and placement. It is not, however, a closed-CDDL validator.

For `c.grant`, the precheck reads the required `grant` member and ignores every other top-level member (`fold.rs:3370–3378`). The same pattern appears across most arms and nested objects.

A scratch fixture re-sealed the committed post-freeze operation with a valid `grant` plus one unknown field:

```text
{
  grant: <otherwise valid grant>,
  bogus: 1
}
```

The operation remains correctly signed and body-hash-valid. The expectation stays the O3 result, `(body-invariant, reject-permanent)`. The reducer instead allows the body through to placement:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: g4 expected body-invariant/reject-permanent,
reducer derived ctrl-fork/freeze-control
GATE RED
```

Scratch artifact: `/tmp/d0a-ff23-d99-extra.i9kync`.

This is the same first-failing-stage class D-99 was meant to close: a body the registry rejects still exerts a precedence effect.

**Required repair:** enforce exact allowed-key sets, including nested closed maps, in the arm-indexed intrinsic stage before replay and placement. Commit the extra-field trace. Prefer a shared per-arm key-set table/helper so “all thirteen dispatched arms” is mechanically true rather than a comment that can drift.

### F3 — `operation_version` is parsed but never dispatched

**Classification:** Gate-A protocol blocker under criterion 4; coverage-label defect.

**Normative anchors:** O3 at `owner-plane-d0a-spec.md:575–580`; M2 at `:3009–3014`; Appendix dispatch rule at `:3978–3988`.

The registry is specified as keyed by:

```text
(tenant, operation_type, operation_version)
```

The reducer parses `operation_version` into the header, but no reducer source reads it after parsing. Registry admission checks only `operation_type` (`reducer/src/fold.rs:5416–5417`).

The committed `f07-header-unknown-version-rejects` vector does not cover this rule. It mutates the header object’s own `v` from 1 to 2 (`core/src/corpus_ctrl.rs:847–865`), exercising the protocol/container version, not `operation_version`.

In a companion-valid scratch vector, I instead set `header.operation_version = 2`, re-sealed the operation with the real root key, and retained the expected `unknown-version/reject-permanent`. The reducer returned:

```text
container=ok companion=ok pairs=ok decode=ok convergence=ok
semantics=FAIL: x expected unknown-version/reject-permanent,
reducer derived body-invariant/reject-permanent
GATE RED
```

Scratch artifact: `/tmp/d0a-ff23-d99-extra.i9kync/one-version`.

The exact later result is incidental; the operation’s semantic version was not rejected at registry dispatch.

**Required repair:** dispatch on all three registry coordinates and reject unsupported semantic versions before arm-specific CDDL, replay, or placement. Retain or rename the existing `v` negative as a protocol-version test, and add a distinct `operation_version` vector so the `unknown-version` coverage claim is honest.

### F4 — Criterion 8 overstates what the storage controls prove

**Classification:** Gate-A executable-evidence blocker as criterion 8 is currently worded; not a production durability finding.

The current implementation is good by direct source inspection and ordinary execution:

- all fourteen `inputs.stream` values call `materialize_stream`;
- each destination is pre-seeded;
- `durable_write` writes a temporary sibling, calls `File::sync_all`, and renames over the destination;
- the local and three-OS lanes pass with `sync_all=14 rename=14`.

The problem is narrower: the audit says the executable controls prove those properties and resist the earlier greenwashing class. Three scratch mutations remove one claimed property each while the lane still exits 0.

#### A. Remove the real OS flush

```diff
-    f.sync_all()
+    let _ = f;
+    Ok(())
```

Result:

```text
storage lane: 19 vector(s) executed on real files (sync_all=14 rename=14)
flush failpoint control: probe green plain, red under STORAGE_LANE_FAIL_SYNC
exit 0
```

The failpoint is checked before the actual `sync_all` call, so it proves that the seam and its error path are invoked, not that the OS flush remains.

#### B. Skip the four framing-only streams

Conditioning materialization on `inputs.cuts` reduces the durable calls from fourteen to ten:

```text
storage lane: 19 vector(s) executed on real files (sync_all=10 rename=10)
flush failpoint control: probe green plain, red under STORAGE_LANE_FAIL_SYNC
exit 0
```

The end gate requires only nonzero counters, not equality with the corpus-derived stream count.

#### C. Remove destination pre-seeding

Deleting the pre-seed write still produces:

```text
storage lane: 19 vector(s) executed on real files (sync_all=14 rename=14)
flush failpoint control: probe green plain, red under STORAGE_LANE_FAIL_SYNC
exit 0
```

Readback proves publication of the new bytes but no longer proves replacement of an existing path.

Scratch source: `/tmp/d0a-storage-audit.6xk4wN`. Logs:

- `/tmp/d0a-storage-mutation-no-os-flush.log`
- `/tmp/d0a-storage-mutation-skip-framing.log`
- `/tmp/d0a-storage-mutation-preseed.log`

The intended positive control is real: deleting the `sync_seam` invocation makes the failpoint probe remain green and the lane exits 1. The gap is that the control cannot distinguish a seam whose real flush body has been replaced by a no-op.

**Required repair:** keep this proportional to a pre-alpha artifact gate. Derive and assert the expected stream-materialization count; make pre-existing destination status an asserted input/invariant; and either strengthen the sync proof or state its actual limit truthfully: source inspection confirms `sync_all`, while the executable failpoint proves seam invocation and error propagation. Do not pull Gate-B crash guarantees into this repair.

### F5 — Criterion 11’s truth pass is incomplete

**Classification:** Gate-A documentation/metadata blocker; cheap repair.

Concrete live mismatches:

- `execution-lanes-plan.md:3–11` still calls itself a planning document for surfaces that have never executed and says exactly two surfaces run, although Chromium and three storage OS lanes are delivered.
- The same file still carries the old pending Chromium job name at `:79–80` and the old f13 aggregate `records=37, bytes=30781` at `:102–103`; current execution reports `45 / 40053`.
- PR #318 still describes spec v0.5.20 with D-201..D-203 and says proposed D-204 awaits the owner. The reviewed head is spec v0.5.21 with D-204 ratified.
- `p1-v1-profile.md:16–18` says its stated grep finds 99 `Unimplemented` sites; the stated top-level glob now finds 89.
- `reducer/src/crypto.rs:358–360` still calls the harness a 157-vector corpus; it is 168.
- The universal convergence sentence in spec §13.1 remains unreconciled with the later D-204 exception.

The audit itself correctly remains at FAIL, and its current corpus histogram and primary hashes are accurate. The problem is the criterion-11 assertion that the truth pass is complete.

**Required repair:** one bounded truth sweep across the files above and the PR body. Historical review files should remain historical; only live/current-state surfaces need changing.

## Repairs that verified cleanly

The send-back should preserve these results:

- **D-130 exact-reference rule:** a boundary naming a hash whose bytes are not held now pends `ref-unresolved`; the held tenant operation remains admitted. Full two-variant selection remains explicitly deferred with an honest `Unimplemented` marker and coverage row.
- **D-202 two-world execution:** the committed late-first and timely-first vectors genuinely derive their owner-ruled states, and `d202_two_worlds_derive_ruled_states` is non-vacuous. F1 is a third ordering inside the late class, not a rejection of the owner’s cross-class residual.
- **Canonical convergence:** the generated suite is real, the retained arrival-order restoration engine diverges, and the current canonical engine passes the committed ordinary convergence lanes.
- **Journal kill authority:** the forged and unadmitted recovery arms remain correctly refused/pending at the declared abstraction.
- **Audit partition release:** completeness and exact-union checks remain independently derived at the companion’s read-release abstraction.
- **Lane manifests:** required surface equality and bidirectional lane manifests are load-bearing.
- **Browser lane and teardown:** local 56/56 reproduces; injected cleanup failure does not turn semantic green red, and a tampered semantic result still exits red.
- **Gate integrity:** semantic red, empty corpus, and non-permutation deliveries all exit nonzero as intended.

## Minimal path to the next review

1. Fix the D-204 retry-before-original order and commit the exact failing delivery.
2. Complete closed-map validation in the D-99 intrinsic body stage and commit the unknown-extra-field vector.
3. Enforce `(tenant, operation_type, operation_version)` dispatch and add a real semantic-version negative.
4. Repair or accurately narrow criterion 8’s executable-proof claims; keep Gate-B durability out of scope.
5. Perform the small criterion-11 truth pass, including PR #318’s description and the §13.1/D-204 exception.
6. Rerun the existing pin battery plus the three semantic traces and three storage mutations.

No new broad mechanism family is required. If the D-204 ordering fix starts expanding beyond a narrow first-failing-stage or signed-dependency repair, return that one point to the owner rather than growing another prose-only protocol layer.

## Recommendation to the owner

Send PR #318 back for this bounded repair and keep it draft. Do not stamp Gate A at `ff23f1cd`.

The branch is close in the meaningful sense: the independent reducer, committed corpus, CI lanes, D-130 correction, Journal/audit work, and browser/storage implementations are substantial and mostly honest. The remaining blockers are concentrated and executable—closed body/version dispatch, one same-class D-204 ordering hole, accurate storage proof language or controls, and a final truth pass.
