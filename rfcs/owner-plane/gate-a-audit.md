# D0-A Gate-A Discrepancy Audit — amended after the repair tranche

**Date:** 2026-07-14 (original audit), amended 2026-07-14 after the repair tranche; owner rulings recorded 2026-07-14 (spec v0.5.20, D-201..D-203); post-ruling execution recorded 2026-07-15 (the C.1 mechanisms, the cheap-gap batch, the storage lane); the browser lane recorded 2026-07-15; **re-amended 2026-07-15 on the reconciled verification review** (`reviews/2026-07-15-gate-a-verification-reconciled-review.md`) — the interim "predicate satisfied" claim was WRONG and is withdrawn; **re-amended 2026-07-16 after the criterion-12 tranche** (the synthesized criterion-12 review, `reviews/2026-07-15-gate-a-criterion12-synthesized-review.md`, found three executable protocol counterexamples — F1 D-99, F2 D-130, F3 D-202 — plus the criterion-8 proof gaps and this document's own drift; the owner directed the bounded repairs 2026-07-15 and they are executed below)
**Auditor:** the artifact-phase differential program; predicate amendments per the external audit review's mandate
**Spec:** `owner-plane-d0a-spec.md` v0.5.21, SHA-256 `5ca12fe7a049ea223130c470e3b1234ad2b96e90f4b54c792e31d7dc1de4909a` (v0.5.20 = `ec3a9a6d…` and v0.5.19 = `410880e0…`, archived byte-exact). v0.5.21 = the owner's D-204 ratification (2026-07-16): the D-202 convergence carrier narrowed to shared evidence-arrival structure — the T5 prose amendment, the D-204 decision row, and the D-202 row's supersession rider, exactly the wording the owner approved from `decisions-pending.md`; prose only, no artifact bytes changed
**Companion:** `d0a-vector-cases.v1.json`, SHA-256 `11dd88972220cac3a120f6f729c9b3eb9cd9e6a9a332bff75b4765efd178aaba` (amendments #1–#6; #5 = the audit read-release input + derived `released` verdict, review R4; #6 = the `evidence-lifecycle` case kind, the D-202 ruling made executable, review R7; the family-3 browser-exclusion comment re-scoped to P-256 per R8.10; the criterion-12 tranche's three new vectors ride existing case kinds — no amendment #7)
**Corpus:** 168 vectors (f01×17, f02×7, f03×6, f04×4, f05×4, f06×6, f07×29, f08×4, f09×13, f10×7, f11×36, f12×15, f13×16, f14×4 — regenerated from the vectors directory after the criterion-12 tranche: +2 D-99 multi-fault regressions, +1 D-202 timely-first world; the f07 unheld-head fixture re-authored in place)
**Suites at this amendment:** core 141/141 · reducer 37/37 (incl. the metamorphic-convergence corpus test, the arrival-order restoration control, and the D-202 cross-world pin) · the strict harness 168/168 with a nonzero-exit gate that also rejects an EMPTY corpus · the portable-storage lane 19/19 on real files (EVERY stream through the durable path — `sync_all=14 rename=14` — each rename replacing a pre-seeded destination, plus the flush failpoint control) · the browser lane 56/56 in headless Chromium (WebCrypto semantics + the f13 IndexedDB/Web-Locks substrate), both lanes pinned to `coverage/lane-manifests.json` · fmt/clippy clean all three crates · mint byte-idempotent (vectors + coverage artifacts)

> **VERDICT: FAIL — both repair tranches executed; awaiting the
> fresh independent review the acceptance criteria require.** Gate A
> is **not** stamped. The history in one paragraph: the original
> audit FAILED; an interim 2026-07-15 amendment claimed the predicate
> satisfied and was REFUTED by the reconciled verification review
> (filed in `reviews/`), whose findings the owner-directed repair
> tranche then repaired in full; the criterion-12 review round
> (two independent reviews + their synthesis, filed in `reviews/`)
> then found three executable protocol counterexamples the first
> round's case selection had not reached — D-99 arm-CDDL validation
> still behind replay/placement (F1), D-130 selection of an unheld
> random hash (F2), and D-202's re-proposal carrier failing across
> its two ruled evidence worlds (F3) — plus an under-proven
> criterion-8 storage claim (F4) and documentation drift (criterion
> 11). The owner directed the bounded criterion-12 tranche
> 2026-07-15 (D-202 resolved by narrowing the promise; D-130
> honest-defer; criterion 8 by code; D-99 in full), §5 records its
> execution, and the owner RATIFIED the D-204 narrowing 2026-07-16
> (spec v0.5.21 — the one protocol-text consequence of the round).
> The one criterion this repository cannot satisfy from inside
> remains: a FRESH independent reviewer rerunning the gate from a
> pinned commit. This document never self-stamps; the verdict stays
> FAIL until that review reports.

## 0. Scope and method

Two independent implementations were built against the prose alone and
run differentially over every committed vector:

- **owner-plane-core** — the writer side: canonical CBOR encoder,
  domain hashes, suite-v1 crypto, the Appendix-A shapes with signing
  composition, the key schedule, and the fixture rig that mints every
  vector as real signed bytes (genesis ceremonies, HPKE wraps, C3′
  recoveries included).
- **owner-plane-reducer** — the reader side, sharing **no code** with
  core: a strict E1–E10 decoder, its own domain/envelope layer, the
  control + tenant + judgment fold engine (with the D-138 total
  re-fold implemented literally), the journal machine, the §5.5
  erase-crash replayer, the §10.1 edge predicate, its own CRC32C,
  an independent BIP-39 leg (wordlist indexes, entropy rebuild, and
  SHA-256 checksum verification, NFKD before PBKDF2), and B.2/B.3
  policy transcription.

The harness validates every vector against the §13.1 container schema
(extracted from the spec's own fenced block and compiled by a real
Draft 2020-12 engine), the companion vocabulary and per-case_kind
contracts, the §10.4×§10.5 pair relation, a strict-decode differential
over every byte input, and the three-run converge standard (every
listed delivery order plus a fresh fold of the union). Since the
repair tranche, convergence-bearing vectors must list **≥ 2
byte-distinct delivery orders** (racing-pair fixtures preserve the
pair and vary the prefix), and the harness CLI is a real gate: any
structural failure, semantic FAIL, or Unimplemented exits nonzero
(`GATE RED`), with `--help` and strict argument handling.

Every place either implementation chose a reading the spec left terse
was recorded in the **interpretation register** (71 entries, kept in
the program ledger and at the code sites). This audit classifies them.

## 1. Differential scoreboard — what the method caught

Cross-implementation byte agreements (the deepest validation targets):

- **B.2/B.3 policy literals** reproduced independently by BOTH
  implementations through their own writers: workflow-v1 = 1133 B /
  `219b9bac…`, owner-v1 = 571 B / `d7d5559a…`.
- The §13.3 pinned worst-width figures (kekwrap **314 B**, erasemref
  **132 B**), the 128+128 rotation fit, and the 48-KiB checkpoint
  joint budget re-derived by the reducer's own encoder.
- CRC32C (RFC 3720 check value), the §11.8 bundle-leaf bytes, Merkle
  roots/paths, HPKE against the CFRG vector for the exact suite,
  RFC 6979/8032/5869/8439 pins on both sides.
- **Post-tranche:** the state-5 tombstone bytes CONSTRUCTED by the
  reducer from the signed `erase_manifest` equal core's
  independently encoded tombstone payloads — the two canonical
  writers agree on the §5.5 re-derivation with no oracle between
  them.

Live findings, each caught by the differential before commit
(original phase):

1. **Fixture defect (f11 erase-deferral):** the release cited the
   flowless genesis grant — unreachable under §11.8's flow match +
   D-76. Fixture re-minted with a flow grant.
2. **Fixture-harness defect (f8/f11 merkle-proof):** the width
   quantifier verified a wrong-index vector (leaf 2 @ width 3 ≡
   leaf 1 @ width 2 structurally). Resolved by D-162's own
   self-description: the declared `rec_index` must equal the leaf's
   internal index before width quantification.
3. **Fixture defect (f12):** quota was keyed shape-globally; §10.1
   specifies a per-SHAPE service policy. Both sides re-keyed
   `quota[shape][op]`.
4. **Fixture annotation defect (f11 merkle-proof ×3):** claimed the
   `browser` surface; §13.2 pins family 11 to `core` only. Corrected
   by the surfaces gate on its first run.
5. **Companion defects (amendments #1/#2, recorded in the ledger):**
   the fold `per_item` pair requirement and two misplaced `$comment`
   annotations. Backward-compatible.
6. **Reducer defect (self-caught):** its first key_id draft emitted
   `alg` before `pk`; its own strict reader rejected the
   non-canonical order.

Repair-tranche findings (each an artifact defect the original audit
under-weighted or missed; all closed except where noted):

7. **The harness could not go red.** `main` printed reports and
   exited 0 on semantic FAIL and Unimplemented alike. Fixed: the
   `all_green` gate, strict argv, and a negative test that plants a
   lying vector and asserts the red exit.
8. **Fifteen convergence-bearing vectors listed one effective
   order** (single-order or `==sorted` duplicates) — the "converge"
   assertion was vacuous on fork-critical traces. Fixed with real
   second deliveries; the repair EXPOSED finding 9.
9. **REAL ENGINE GAP (D-185 journal reservation):** reversed
   deliveries on the journal traces diverged — an unheld-journal /
   future-incarnation record classified `log-corrupt` on first
   contact instead of reserving `(ref-unresolved,
   pending-dependency)`, and reopen ran legality before its
   citation-holding checks. Fixed: whole-Txn pendency with no
   mutation; the reservation pattern generalized.
10. **The erase-lane manifest oracle was circular:** §5.5 state-6
    re-derivation read the FULL stream's tombstones — under replay,
    the answer sheet. Fixed by companion amendment #4: the signed
    `c.kek_rotate` triples ride `inputs.rotation_ops`; every durable
    Fence must resolve `rotation_op = H_op` over one (the hash
    covers the signature bytes); durable tombstones are validated
    against the resolved manifest entry and state-5 re-derivations
    are CONSTRUCTED from it.
11. **The reopen trace's invalidation was semantically invalid:** a
    storage receipt for the release op was held and accepted as
    killing a revocation basis. Fixed: the invalidation is an owner
    recovery based below the revocation (the §7.4 branch cut), and
    the journal machine now VERIFIES the kill (`base.seq <` the
    basis's chain position on the same writer chain) — with an
    unheld-pends vector and a verified-false (recovery keeps the
    basis → `log-corrupt`) vector giving the predicate both arms.
12. **OPEN — the §4.7 fork-discovery wire gap:** D-193 keeps
    stmt-kind invalidations because "fork-discovery statements are
    real killers", but §4.7's closed statement shapes (receipt /
    lease) contain no fork-discovery statement — a held stmt-kind
    invalidation is UNVERIFIABLE on the current wire. The reducer
    surfaces it honestly (`Unimplemented`); closing it needs a wire
    mechanism (out of tranche scope by the standing rule). The
    D-193-promised "statement-invalidation acceptance" vector cannot
    exist until then.

## 2. Discrepancies requiring an owner ruling before freeze

Register numbers in brackets; repair-tranche status on each.

**D1. [#38] §2.4 checksum-invalid phrase rejection was unexpressible
under the companion.** **CLOSED (implemented + vectored):** companion
amendment #3 added the negative arm;
`f08-phrase-checksum-invalid-rejects` carries a 24-word mnemonic with
a broken checksum; the reducer's independent BIP-39 leg rejects it by
its own wordlist/entropy/SHA-256 computation (`key-malformed`,
`reject-permanent`), with NFKD normalization ahead of PBKDF2 and an
entropy↔phrase cross-check. No ruling remains.

**D2. [#47] §11.4 has no actor-class row for a bare autonomous
non-human unattested writer.** **CLOSED — RULED (D-201, owner
2026-07-14): alternative (c), no class / no vote.** Bare writers
never count toward status; their judgments are recordable where
authoring verbs admit them and inert in the §11.2 fold; status
influence requires attestation (the session path). The withdrawn
session mapping had granted status-counting authority B.2 reserves
for attested sessions. Pinned by the minted pair
`f11-status-bare-daemon-retract-inert` /
`f11-status-bare-daemon-supersede-inert` (both derive `candidate`
where the mapping would have derived `retired`/`superseded`); §11.4
carries the rule in prose (v0.5.20).

**D3. [#63] No (outcome, disposition) exists for a control operation
cut by a C3′ branch cut.** **CLOSED — ratified (D-203):** §7.4 now
names `(cutoff, quarantine-reproposal)` with D-140 boundary-purpose
permanence; pinned by `f07-walkthrough-c3-branch-cut-below-head`.

**D4. [#65] No stated classification for a control op arriving while
the plane is C2-frozen.** **CLOSED (implemented + vectored, stage
order repaired TWICE):** the reducer's control prevalidation (pins →
arm → SIGNATURE, no chain/body) precedes freeze classification, so a
forgery never freezes the plane (anti-DoS) — and since the
criterion-12 tranche the COMPLETE body stage (hash binding, registry
row, arm-indexed CDDL shape) precedes the replay consult and the
placement gate too, closing the F1 inversion (this section's earlier
"the body stage stays behind placement" reading contradicted §10.2's
pipeline and this document's own criterion-4 record; a signed header
over CDDL-invalid bytes is a BODY failure, never fork evidence).
Pinned by the trio `f07-c2-post-freeze-valid-op-frozen` (`ctrl-fork`,
`freeze-control`), `f07-c2-post-freeze-sig-invalid-kept`
(`sig-invalid`, `reject-permanent`), and
`f07-c2-post-freeze-cddl-invalid-kept` (`body-invariant`,
`reject-permanent`), with the consumed-request-ID sibling
`f07-consumed-request-id-cddl-invalid` pinning body-before-replay.
The prose sentence remains worth adding at freeze.

**D5. [#57] `lease-stale`'s firing condition is never stated.**
**CLOSED — RULED (D-202, owner 2026-07-14): alternative (ii), sticky
rejection + writer re-proposal — with the convergence sentence
NARROWED (D-204, owner 2026-07-16) after the criterion-12 review's F3
counterexample.** The firing condition and lifecycle are in the T5
prose: a held qualified receipt outside every valid window
classifies `(lease-stale, quarantine-reproposal)` on the evidence
held at evaluation, terminal where issued; the original op's verdict
is knowingly evidence-order-relative. The review proved v0.5.20's
unqualified "convergence rides the re-proposed op" cannot hold ACROSS
the two ruled evidence worlds (timely-first replicas admit the
original, so the same-coordinate re-proposal is D-130 fork evidence
and BOTH variants freeze pending selection); the owner chose
narrowing the promise to shared evidence-arrival structure and
ratified the wording into spec v0.5.21 (the T5 amendment + the D-204
row + the D-202 rider). Both worlds are vectored
(`f09-lease-lifecycle-sticky-reproposal` late-first /
`f09-lease-lifecycle-timely-first-forks` timely-first), the
cross-world relationship is pinned from one byte source by the
reducer's `d202_two_worlds_derive_ruled_states` test, the endpoints
(`f09-lease-stale-quarantines` /
`f09-lease-late-then-timely-receipt-admits`) stand, and the boundary
negatives (`f09-lease-present-no-receipt-pends`,
`f09-lease-overlong-window-invalid`) stand.

**D6. [#52] The §5.5 state-6 re-derivation vs the erase-crash
contract.** **CLOSED (the oracle is gone):** finding 10 above —
companion amendment #4, signed rotation ops as the control context,
tombstones bound to the signed manifest, all 8 vectors re-minted.

**D7. [#54] The classification of a durable RewrapDone omitting an
expected survivor.** **CLOSED — ratified (D-203):** §5.5 now names
`(log-corrupt, storage-quarantine)` for both the omission and the
D-89 N+1-Fence serialization violation.

**D8. [#61] The recovery arm's `repoch` on a NON-succession operation
(`c.drill`).** **CLOSED — ratified (D-203):** the drill prose now
states the CURRENT repoch (a drill proves, never advances).

**D9. [#70] No outcome named for an audit row contradicting its
read's established partition.** **CLOSED (implemented + vectored):**
five conflict vectors (duplicate chunk index, changed principal,
changed scope, changed count, overlapping result sets), each
`(body-invariant, reject-permanent)`, arrival-order-proof via the
chain. The prose row remains worth adding at freeze.

**D10. [#22] The companion's "fresh fold of the union" names no
arrival order.** **CLOSED — ratified (D-203):** §13.1 now states the
fresh fold commits to no arrival order.

**D11. [#46] Umbrella App C #2 (offline expiry confirmation) remains
unperformed.** **DISPOSITIONED (D-203):** stays a recorded open by
owner choice; `f14-offline-expiry-confirmation-pending` keeps it
visible.

**D12. [#8] The op signature and op identity share the domain tag
`op`.** **CLOSED — recorded as accepted (D-203):** disjoint shapes,
no failing trace; no change.

## 3. Derived conventions to codify (clarifications, no behavior change)

Fixture-layer and engine conventions both implementations share; each
should land as a §13 note or a registry-row sentence at freeze:

- **Fold-vector conventions [#10, #29–#36]:** `per_item` = exactly one
  row per delivered item, absence-of-pair = finally admits; trace rows
  assert failure intermediates only; duplicates are edge facts about a
  DELIVERY, never overlaid by the shared op's fold state.
- **Aux is held state, not folded events [#56, #58, #60]:** the fold
  lane's `aux` carries the §5.6 index plus `Signed` receipts/leases,
  validated lazily at admission. Receipt-arrival dynamics are outside
  the lane (and are exactly the D5 open lifecycle).
- **Journal conventions [#12–#17, #27]:** the journal machine
  validates holding/basis/interval arithmetic AND — post-tranche —
  the verifiable-when-held citation content (body_hash-bound aux,
  source-erased basis-freedom, the recovery-cut kill predicate);
  full cause SUFFICIENCY (this fact makes THAT record
  resolved-negative) still needs source dereferencing and stays fold
  territory; a Txn with an invariant-violating record discards
  whole; release/source ids are opaque to the machine.
- **Erase-crash conventions [#51, #53, #55], amended:** one tenant
  log per vector; `machine_state` = the state of the CONFORMANT
  durable prefix at every cut; `rotation_ops` = the signed control
  context every durable Fence must resolve by `H_op` (amendment #4 —
  the manifest oracle is withdrawn); Fence commitment fields other
  than `rotation_op` stay opaque in the storage lane
  (mirror-checked, probe-recovered); an empty-manifest rotation
  completes state 6 vacuously at `KekDestroyed`.
- **Walkthrough probe vocabulary [#62]:** `plane.provenance`,
  `ceiling.lifted`, `ctrl.head`, `ctrl.frozen`, `repoch`,
  `serving.epoch`, `fence.recovered`, `rewrap.recovered`,
  `survivorset.recomputed`, `tombstones.rederived/durable`.
- **Audit-partition lane [#69]:** one read per vector; every item
  must finally admit (the contract has no per_item); negatives ride
  `fold` vectors.
- **Derived-but-undocumented facts:** key_id preimage order is
  (`pk`, `alg`) [#21]; E7 tuple sort keys concatenate canonical
  component encodings [#2]; E8 depth counts container levels only
  [#1]; control-chain gap successors classify by §9.3's generic chain
  arithmetic [#9]; grant list fields are order-preserved arrays, not
  E7 sets [#4].
- **Budget window selection [#68]:** implemented as (zone, signed
  `capability_epoch`) → window ordinal — equivalent to §4.3's rule
  while epochs open densely, which the registry guarantees.

## 4. Coverage — now machine-enforced

The prose inventory this section previously carried is superseded by
two enforced artifacts:

- **`coverage/outcomes-map.json`** — GENERATED by the mint bin: the
  §10.4 outcome → vector map harvested from every vector's expected
  tree, drift-gated, with the uncovered set pinned shrink-only in
  `core::coverage::UNCOVERED_10_4`. The D-203 cheap-gap batch closed
  ten outcomes (the five scope axes, no-grant, no-flow, op-unknown,
  unknown-version, and the lineage-gen fail-close); the remaining
  **12 of 59** are EXPLICIT Gate-B deferrals — the pin carries a
  per-outcome reason, the inventory's `gate_b_deferrals` section
  lists the same set plus the five deferred ceremony sagas, and the
  suite enforces that the two lists match exactly (deferral is a
  decision, not drift).
- **`coverage/obligations-13-3.json`** — the §13.3 obligation ledger:
  84 line-ranged entries whose quotes must appear verbatim in §13.3
  and whose ranges must jointly cover the whole 492-line section (a
  spec edit that adds an obligation breaks coverage); every named
  vector must exist and statuses must match the lists. Current
  truth: **14 vectored, 26 partial, 42 pending** — the per-entry
  notes say exactly which clauses are missing, and the pending mass
  is the D-203-deferred saga set (f7 ratify/checkpoint ceremonies,
  f9 issuer feeds, f10 generation machine, f11 transfer composites,
  f13 checkpoint/renewal storage shapes).

Both artifacts declare `executed_surfaces` = the two Rust
implementations, the Chromium browser lane, and the three storage
OSes — a vector's `surfaces` array is a §13.2 applicability
ANNOTATION, never execution, and the riders name what actually
runs. The **portable-storage lane** (`reducer --bin storage_lane`,
2026-07-15; criterion-8 rework 2026-07-16) runs every
storage-annotated vector on real files: byte round-trips, real
truncations per crash cut, the lock matrix across two real
processes on OS advisory locks, EVERY `inputs.stream` — the
framing-only vectors included — through the durable
write-temp → sync-seam → rename path (`sync_all=14 rename=14`),
each rename REPLACING a pre-seeded destination on all three OSes,
and the flush observation coupled to the call's result by the
`STORAGE_LANE_FAIL_SYNC` failpoint control (the criterion-12
review's counter-keeping sync-deletion mutation now turns the lane
red — verified live). It rides the advisory workflow as a 3-OS
matrix job. The **browser lane** (`browser-lane/`,
2026-07-15) runs every browser-annotated vector in headless
Chromium over raw CDP: semantics through a WebCrypto backend
(Ed25519/P-256 verification with the §3 low-S policy enforced on
raw signature bytes before verify — the high-S rejection vector
passing is live proof, since bare WebCrypto accepts high-S; HPKE
composed per RFC 9180 from ECDH `deriveBits` plus an HMAC-built
labeled-HKDF schedule; AES-GCM; HKDF/PBKDF2 `deriveBits`), and
family 13's §13.2 cell — the IndexedDB Txn subset + Web Locks —
executed as the fixture substrate (one record per awaited IDB
transaction, frame-per-record stream storage at the reducer
walker's REAL frame boundaries, fixture-layer crash cuts against
the in-memory prefix, the lock matrix over `navigator.locks` with
worker actors and the denied loser's store-read proof). Negative
controls verified red on both lanes (a flipped verify expectation;
a flipped lock-denial step caught independently by the in-memory
lane AND the real Web Locks denial). Both lanes cover the
portable/Txn subset only; the Gate-B production concerns (fsync
ordering, keystores, IndexedDB failure injection/eviction,
Firefox/Safari, quota pressure) stay distinguishable and named.

The reducer's honest frontier shrank with the C.1 work; none of
the remaining `Unimplemented` sites is reachable from the committed
corpus (the strict gate proves it on every run). The RATIFIED
`p1-v1-profile.md` (D-203) is now fully executed on its §C.1 side:
rows one through four are implemented and vectored (erase-manifest
fold admission; compromise-mode T4 with derived-lane
retro-disqualification; rotation_refs post-last-wrap linkage; the
D-93/D-143 frontier-head validation at every site), and row five —
the cut-op re-fold arm — is implemented as an internal replay
invariant, which is what the profile records (not a vector; R8.6),
with
every §C.2 row is the binding fail-closed contract — two of them
(lineage-gen, op-unknown) now vectored.

**Documentation-correction record (review R8, resolved this
tranche):** the corpus histogram is regenerated from the vectors
directory (R8.4); the fifth Gate-B saga is marked audit-added, not
D-203-named (R8.5); clause 11 and this section state §C.1 row five
as the internal replay invariant the profile records (R8.6); the
executed-surface riders in `coverage/*.json`, the maintenance tool,
and the workflow header state the delivered lanes (R8.7); the
browser lane's wasm build is clippy-enforced in the advisory
workflow (R8.8); the storage-lane header no longer overstates
read-back substitution (R8.9); the family-3 browser exclusion is
scoped to P-256 nonce injection — Ed25519 signing is deterministic
(R8.10). One item is deliberately deferred to the owner's
freeze-time pass (R8.11): the spec's D-151 decision row still says
"two renewal-after-revocation vectors" — renewal machinery is
fail-closed in the ratified P1 v1 profile and no such vectors
exist; the row's claim is stale prose in an owner document, so it
is RECORDED here rather than edited unilaterally.

## 5. Gate-A verdict

**FAIL.** Gate A is not stamped, and this document must not be read
as a conditional pass. Two refuted self-assessments shape the rule
this section now follows: state the CURRENT verified condition of
each acceptance criterion, once, and leave the verdict to the fresh
independent review — the superseded interim narratives live in the
filed reviews and the git history, not here.

The criteria below are the criterion-12 review round's twelve (which
bind the NEXT review, not this self-report), stated with their
verified condition after BOTH owner-directed repair tranches:

1. *All suites green at one pinned commit* — core 141/141, reducer
   37/37, the strict gate 168/168, browser 56/56, the 3-OS storage
   lane 19/19; every lane runs per push on the advisory workflow.
2. *The eight orders committed + a generated convergence suite* —
   all eight ride their vectors as regression deliveries; the
   metamorphic sweep (exhaustive permutations through five items;
   reversal, rotations, and adjacent transpositions of every base
   above) runs inside the fold, journal, status-derive, and
   audit-partition lanes on every harness run.
3. *The suite discriminates* — the pre-repair arrival-ordered loop
   is retained test-only and the control test proves it diverges on
   the review's r2 order while the canonical engine converges
   (`convergence_standard_fails_under_arrival_order_restoration`).
4. *The governing D-99 pipeline holds, not just the literal arms* —
   the COMPLETE body stage (hash binding, registry row, arm-indexed
   intrinsic CDDL shape for all thirteen dispatched arms) precedes
   the replay consult and the placement gate in `classify`;
   state-dependent invariants stay in the transitions, and honest
   `Unimplemented` branches pass through untouched. The F1
   multi-fault pair is committed
   (`f07-c2-post-freeze-cddl-invalid-kept`,
   `f07-consumed-request-id-cddl-invalid`) beside the earlier
   body-hash and signature arms.
5. *Forged/unadmitted recoveries cannot verify a kill* — admission
   is the authentication; both arms are committed vectors
   (`f11-reopen-forged-recovery-log-corrupt`,
   `f11-reopen-unadmitted-recovery-pends`), and the basis must be
   genuinely dead on the fold. The redundant-aux byte-retention
   assumption remains a recorded model boundary (review residual).
6. *Incomplete partitions cannot release* — the independent
   read-release event (companion amendment #5) with completeness,
   exact-union, and one-Txn derivation; five refusal negatives. The
   fixture declares release membership rather than replaying raw
   Txn bytes — the declared abstraction, stated, not more.
7. *Annotation loss reddens the lane* — the surfaces suite requires
   EXACT equality with the §13.2 R-set, and both drivers pin their
   run sets to `coverage/lane-manifests.json` bidirectionally.
8. *Storage flush/replacement proven AS CLAIMED* — every
   `inputs.stream` (framing-only vectors included) materializes
   through the durable write-temp → sync-seam → rename path; every
   rename replaces a pre-seeded destination on all three OSes; the
   zero-count check stands and the flush observation is COUPLED via
   the `STORAGE_LANE_FAIL_SYNC` failpoint control — the review's
   counter-keeping sync-deletion mutation was applied live and the
   lane went red. Power-loss ordering, directory fsync, keystores,
   and fault injection remain Gate B.
9. *The D-202 lifecycle executable in BOTH ruled worlds* — the
   late-first world (`f09-lease-lifecycle-sticky-reproposal`, the
   sticky registry probed non-vacuous) and the timely-first world
   (`f09-lease-lifecycle-timely-first-forks`: the original admits at
   evaluation, the re-proposal contests the occupied coordinate, and
   BOTH variants freeze pending selection), with the cross-world
   relationship pinned from one byte source
   (`d202_two_worlds_derive_ruled_states`). The convergence
   sentence's narrowing is RATIFIED (D-204, owner 2026-07-16, spec
   v0.5.21) — the harness's shared-structure rule for listed
   deliveries is now the narrowed promise's normative precondition,
   not a harness convention.
10. *Empty-corpus and non-permutation controls red* — the bin exits
    2 on an empty directory; every delivery must be a true
    permutation of the item set.
11. *Ledgers, comments, counts, prose match* — this amendment: the
    README counts and D-130 wording, the coverage/surfaces source
    comments, the P1-profile frontier-head row, this document's
    header/histogram/suite counts, the D4/D5 records, the §4
    executed-surfaces and storage-lane paragraphs, the
    execution-lanes-plan note, and the PR description were all
    re-verified against the artifacts in the criterion-12 truth
    pass.
12. *A fresh independent reviewer reruns the gate* — OUTSTANDING by
    construction: this document cannot satisfy it, and the verdict
    stands FAIL until that review reports.

**The additional D-130 blocker (the review's un-rowed finding)** is
repaired honest-defer per the owner's direction: `parse_heads`
selects only byte-variants genuinely HELD at the named coordinate
(the accepted op or a registered fork variant); an unheld named hash
pends `ref-unresolved` under §7.1's referenced-Head lifecycle. The
ninth-fixture history closes accordingly: the original
`…-head-hash-mismatch-rejects` encoded the superseded v0.5.9
rejection; the first tranche over-rotated it to admit-and-select
against a randomly drawn hash; the criterion-12 review's F2 refuted
that with the fixture's own bytes, and it now stands as
`f07-revoke-cutoff-unheld-head-pends` (the revoke pends, the held
`i` stays admitted, later control ops pass the pending reference).
Full two-variant fork selection stays honestly deferred (the
`fork-selection` coverage row; the selected-variant revival arm
remains an `Unimplemented` marker), and the README/profile say
exactly that.

Nothing in this verdict stamps the spec, opens P1, or amends the
Gate-A predicate silently; D-204 is RATIFIED (owner, 2026-07-16 —
spec v0.5.21), while the freeze-time prose ratifications (the D4/D9
sentences, the recorded D-151 row fix) and the §16 stamp remain the
OWNER's acts; P1 writes stay barred until Gate B and the
P0.5/tombed-cutover sequence regardless.
