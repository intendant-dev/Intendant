# D0-A Gate-A Discrepancy Audit — amended after the repair tranche

**Date:** 2026-07-14 (original audit), amended 2026-07-14 after the repair tranche; owner rulings recorded 2026-07-14 (spec v0.5.20, D-201..D-203); post-ruling execution recorded 2026-07-15 (the C.1 mechanisms, the cheap-gap batch, the storage lane); the browser lane recorded 2026-07-15; **re-amended 2026-07-15 on the reconciled verification review** (`reviews/2026-07-15-gate-a-verification-reconciled-review.md`) — the interim "predicate satisfied" claim was WRONG and is withdrawn; **re-amended 2026-07-16 after the criterion-12 tranche** (the synthesized criterion-12 review, `reviews/2026-07-15-gate-a-criterion12-synthesized-review.md`, found three executable protocol counterexamples — F1 D-99, F2 D-130, F3 D-202 — plus the criterion-8 proof gaps and this document's own drift; the owner directed the bounded repairs 2026-07-15 and they are executed below); **re-amended 2026-07-16 after the ff23f1cd review round** (`reviews/2026-07-16-gate-a-ff23f1cd-review.md` — the fresh independent review at the pinned head returned FAIL with five findings, every one verified and repaired under the owner's delegated adjudication: F1 = the D-205 self-evidence exception completing D-204; F2 = closed key sets in the intrinsic body stage; F3 = three-coordinate registry dispatch; F4 = the storage proof's count-equality, verified pre-seed, and stated sync limit; F5 = this truth sweep)
**Auditor:** the artifact-phase differential program; predicate amendments per the external audit review's mandate
**Spec:** `owner-plane-d0a-spec.md` v0.5.23, SHA-256 `22f09e43ee59284e1c0903fd638dc8563b655443904b40e439582395159bb240` (v0.5.22 = `30c91f94…`, v0.5.21 = `5ca12fe7…`, v0.5.20 = `ec3a9a6d…`, v0.5.19 = `410880e0…`, archived byte-exact). v0.5.21 = the owner's D-204 ratification (the D-202 convergence carrier narrowed to shared evidence-arrival structure); v0.5.22 = D-205 under the owner's delegated adjudication of the ff23f1cd review round (the self-evidence exception to same-coordinate fork registration; KEPT by explicit owner ratification 2026-07-16); v0.5.23 = the owner's closure filing (D-206, the Gate-A closure rule; the freeze-time D4 and D9 prose sentences; the D-151 row correction and its c.enroll registry-row mirror — all owner-ratified 2026-07-16 via the head-session brief)
**Companion:** `d0a-vector-cases.v1.json`, SHA-256 `8d2f880006502563b528b64f70eb2f0fd3ccdb721b894df93596dc6bfab8d859` (amendments #1–#7; #5 = the audit read-release input + derived `released` verdict, review R4; #6 = the `evidence-lifecycle` case kind, the D-202 ruling made executable, review R7; #7 = the machine-readable `evidence_class` required on evidence-lifecycle vectors, the ff23f1cd review's F1; the family-3 browser-exclusion comment re-scoped to P-256 per R8.10)
**Corpus:** 170 vectors (f01×17, f02×7, f03×6, f04×4, f05×4, f06×6, f07×31, f08×4, f09×13, f10×7, f11×36, f12×15, f13×16, f14×4 — regenerated after the ff23f1cd round: +1 extra-field × C2 multi-fault, +1 operation_version negative; the late-first lifecycle vector gains its re-proposal-first third delivery and both lifecycle vectors declare `evidence_class`)
**Suites at this amendment:** core 141/141 · reducer 37/37 (incl. the metamorphic-convergence corpus test, the arrival-order restoration control, and the D-202 cross-world pin) · the strict harness 170/170 with a nonzero-exit gate that also rejects an EMPTY corpus · the portable-storage lane 19/19 on real files (EVERY stream through the durable path with counter EQUALITY against the corpus-derived stream count — `sync_all=14 rename=14` of 14 — each rename replacing a read-back-verified pre-seeded destination, plus the flush failpoint control) · the browser lane 56/56 in headless Chromium (WebCrypto semantics + the f13 IndexedDB/Web-Locks substrate), both lanes pinned to `coverage/lane-manifests.json` · fmt/clippy clean all three crates · mint byte-idempotent (vectors + coverage artifacts)

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
> (spec v0.5.21). The FRESH independent review then ran at the
> pinned head `ff23f1cd` and returned FAIL with five findings (all
> verified; filed in `reviews/`): the D-204 late class diverged when
> the re-proposal was delivered before the original, the intrinsic
> body stage accepted unknown fields in closed maps,
> `operation_version` was parsed but never dispatched, the storage
> controls proved less than criterion 8 said, and the truth pass had
> leftovers. The owner delegated that round's adjudication
> ("recommendations; you have the authority to decide"), and the
> repairs are EXECUTED: the D-205 self-evidence exception (spec
> v0.5.22 — a condemned original never freezes its own convergence
> carrier), closed key sets across all dispatched arms and nested
> shapes, three-coordinate registry dispatch, the storage proof's
> count-equality + verified pre-seed + honestly stated sync limit,
> and this truth sweep. Every finding-bearing trace is now a
> committed regression. The closure question is now governed by
> **D-206** (owner, 2026-07-16): Gate A closes on a fresh
> independent review at a single pinned commit reporting **zero
> blockers** (executable or normative, per the §5 criterion-12
> text); all other findings are residuals — filed in
> `residuals.md`, repaired in ordinary follow-ups, never
> gate-reopening. This document never self-stamps; the verdict
> stays FAIL until that review's PASS report, and the §16 stamp
> remains the owner's act.

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
2026-07-15; criterion-8 rework 2026-07-16, hardened per ff23f1cd F4
the same day) runs every storage-annotated vector on real files:
byte round-trips, real truncations per crash cut, the lock matrix
across two real processes on OS advisory locks, EVERY
`inputs.stream` — the framing-only vectors included — through the
durable write-temp → sync-seam → rename path with counter EQUALITY
against the corpus-derived stream count (`sync_all=14 rename=14` of
14 — skipping any stream is red), each rename REPLACING a
read-back-verified pre-seeded destination on all three OSes
(deleting the pre-seed is red), and the flush observation coupled to
the call's result by the `STORAGE_LANE_FAIL_SYNC` failpoint control
(the counter-keeping sync-deletion mutation is red — all three
mutation classes verified live). Stated limit: the seam's own
`sync_all` body is source-inspection ground truth (no portable
runtime observation distinguishes a no-op replacement). It rides the
advisory workflow as a 3-OS matrix job. The **browser lane** (`browser-lane/`,
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
verified condition after THREE owner-directed repair rounds (the
reconciled-review tranche, the criterion-12 tranche, and the
ff23f1cd round executed under the owner's delegated adjudication):

1. *All suites green at one pinned commit* — core 141/141, reducer
   37/37, the strict gate 170/170, browser 56/56, the 3-OS storage
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
   the COMPLETE body stage precedes the replay consult and the
   placement gate in `classify`: hash binding, the registry row
   keyed by ALL THREE coordinates (`operation_version != 1` rejects
   `unknown-version` at the row consult — the ff23f1cd F3 repair;
   the tenant preamble carries the same gate at its body-stage
   boundary), and the arm-indexed intrinsic CDDL shape with CLOSED
   KEY SETS for every dispatched arm and nested object (App-A
   verbatim tables; unknown fields reject `body-invariant` exactly
   as in headers — the ff23f1cd F2 repair; a deferred MECHANISM's
   known key set is still enforced). State-dependent invariants stay
   in the transitions. The committed regressions:
   `f07-c2-post-freeze-cddl-invalid-kept`,
   `f07-consumed-request-id-cddl-invalid`,
   `f07-c2-post-freeze-extra-field-kept`, and
   `f07-operation-version-unknown-rejects` beside the earlier
   body-hash and signature arms (`f07-header-unknown-version-rejects`
   stays the PROTOCOL-version negative — the header's own `v`,
   rejected at parse).
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
8. *Storage flush/replacement proven — with the proof's exact
   strength stated* (the ff23f1cd F4 repairs). What the executable
   controls prove: the durable-materialization counters must EQUAL
   the corpus-derived stream count (skipping any stream — the F4-B
   mutation — is red, not just a zero count); every destination's
   pre-seed is READ BACK before the durable write and the sentinel
   must be gone after it, so each rename is a verified replacement
   of an existing file on all three OSes (deleting the pre-seed —
   F4-C — is red); and the `STORAGE_LANE_FAIL_SYNC` failpoint
   control proves the sync seam is INVOKED on the durable path with
   its error propagating (the counter-keeping sync-deletion mutation
   is red). Stated limit (F4-A): no portable runtime observation
   distinguishes a seam whose real `sync_all` body became a no-op —
   that the seam performs the OS flush is source-inspection ground
   truth, stated on the seam itself. Power-loss ordering, directory
   fsync, keystores, and fault injection remain Gate B.
9. *The D-202 lifecycle executable in BOTH ruled worlds — on every
   relative delivery order* — the late-first world
   (`f09-lease-lifecycle-sticky-reproposal`, the sticky registry
   probed non-vacuous; its THIRD listed delivery is the ff23f1cd F1
   regression, the re-proposal delivered before the original) and
   the timely-first world
   (`f09-lease-lifecycle-timely-first-forks`), with the cross-world
   relationship pinned from one byte source
   (`d202_two_worlds_derive_ruled_states`). D-204 (spec v0.5.21)
   narrowed the carrier to shared evidence-arrival structure; D-205
   (spec v0.5.22, the delegated adjudication) completes it — a
   late-class original arriving after its re-proposal
   self-classifies sticky `lease-stale` at the occupied coordinate
   and registers no fork evidence, so the class converges on every
   relative order. Each vector declares its `evidence_class`
   (companion amendment #7), making the harness's shared-structure
   rule machine-checkable.
10. *Empty-corpus and non-permutation controls red* — the bin exits
    2 on an empty directory; every delivery must be a true
    permutation of the item set.
11. *Ledgers, comments, counts, prose match* — the ff23f1cd truth
    sweep repaired that review's named leftovers: the
    execution-lanes-plan header now records the delivered six-surface
    state (with the retired interim job name and the delivery-day vs
    current f13 aggregates both dated), the P1 profile's
    `Unimplemented` count is stated as grep-derived-and-moving, the
    reducer crypto wiring comment drops its stale corpus count, §13.1
    carries the evidence-lifecycle exception beside its
    universal-convergence sentence, this document's header/verdict/
    criteria reflect the round, and the PR description is rewritten
    at each amendment. Standing rule: counts in prose are stated
    with their derivation or dated, so the next drift is visible.
12. *Gate-A closure (D-206, owner 2026-07-16 — supersedes this
    criterion's earlier "finds nothing" form).* Gate A closes when a
    fresh independent review — conducted at a single pinned commit
    by a reviewer with no prior authorship in this program —
    re-executes the full battery and reports **zero blockers**,
    where a blocker is exactly: **(a) executable** — any suite or
    lane red at the pinned head (core, reducer, strict gate,
    storage, browser, including the discrimination and negative
    controls), or a demonstrated divergence between normative text
    (spec + companion) and committed artifact behavior that changes
    any admitted, pended, rejected, frozen, or derived outcome;
    **(b) normative** — a contradiction within or between the spec
    and companion under which two conforming implementations would
    disagree on the outcome of a covered behavior. All other
    findings are **residuals** (editorial drift, stale comments or
    counts, documentation mismatches, coverage-annotation gaps,
    style): filed with severity labels, recorded in `residuals.md`,
    repaired in ordinary follow-up commits — they do not reopen
    Gate A, do not reset the review, and do not require a further
    fresh review. A zero-blocker review is a **PASS report**; the
    §16 stamp remains an owner act (D-151's reservation unchanged).
    If a review reports blockers, the repair requires **one scoped
    re-review only** — the repair diff plus a battery re-run at the
    new pin — never a full-program re-review. Status: two full
    fresh reviews performed (each returned findings, all repaired);
    the closure review at the owner-named inspection pin is
    commissioned by the owner. The verdict below stands FAIL until
    that review's PASS report.

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
