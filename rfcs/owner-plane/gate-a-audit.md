# D0-A Gate-A Discrepancy Audit — amended after the repair tranche

**Date:** 2026-07-14 (original audit), amended 2026-07-14 after the repair tranche
**Auditor:** the artifact-phase differential program; predicate amendments per the external audit review's mandate
**Spec:** `owner-plane-d0a-spec.md` v0.5.19, SHA-256 `410880e04433b629d5d11956e322f59832494d8f25042b3dfcf34d8b694c6748` (unchanged)
**Companion:** `d0a-vector-cases.v1.json`, SHA-256 `a3d6f779d30492978d6871b97d42037143f4a95c97256aaa92bf5aaa8be0f319` (amendments #1–#4; #3 = the phrase-derive negative arm, #4 = the erase-crash `rotation_ops` control context)
**Corpus:** 143 vectors (f01×17, f02×7, f03×6, f04×4, f05×4, f06×6, f07×19, f08×4, f09×9, f10×7, f11×25, f12×15, f13×16, f14×4)
**Suites at amendment:** core 140/140 · reducer 33/33 · the strict harness 143/143 with a nonzero-exit gate · fmt/clippy clean both crates · mint byte-idempotent (vectors + coverage map)

> **VERDICT: FAIL.** Gate A is **not** stamped. The original
> recommendation (PASS conditional on twelve ratifications) rested on
> two unratified scope reductions — treating never-executed browser
> and per-OS storage lanes as annotation-satisfiable, and carrying
> the §13.3/§10.4 coverage debt untracked — and on four artifact
> defects the repair tranche has since closed (a gate that could not
> go red, vacuous convergence orders, an erase-lane oracle that read
> the answer from the stream under replay, and a journal reopen trace
> whose cited invalidation could not kill its basis). §5 states the
> amended predicate; Gate A passes only when every clause holds.

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
non-human unattested writer.** **REOPENED — awaiting ruling, mapping
withdrawn:** the original recommendation (adopt the `session`
derivation both engines used) silently granted bare daemons the
status-counting authority B.2 reserves for attested sessions. The
scaffold mapping is REMOVED: the derivation returns no class, and any
path that would decide through it (a bare writer holding a judge
verb; a status fold containing a standing bare-writer judgment)
surfaces `Unimplemented` naming `decisions-pending.md`, which carries
the four alternatives, their authority consequences against the
PINNED B.2/B.3 bytes, and two discriminating vector drafts
(deliberately unminted). Verified: no committed vector reaches the
boundary.

**D3. [#63] No (outcome, disposition) exists for a control operation
cut by a C3′ branch cut.** OPEN (prose ratification): both
implementations classify `(cutoff, quarantine-reproposal)`, pinned by
`f07-walkthrough-c3-branch-cut-below-head`. §10.4/§7.4 should name
it.

**D4. [#65] No stated classification for a control op arriving while
the plane is C2-frozen.** **CLOSED (implemented + vectored, stage
order repaired):** the reducer's control prevalidation (pins → arm →
SIGNATURE, no chain/body) precedes freeze classification, so a
forgery never freezes the plane (anti-DoS) while the body stage stays
behind placement (D-99: a signed header over garbage bytes is real
fork evidence). Pinned by the pair
`f07-c2-post-freeze-valid-op-frozen` (`ctrl-fork`, `freeze-control`)
and `f07-c2-post-freeze-sig-invalid-kept` (`sig-invalid`,
`reject-permanent`). The prose sentence remains worth adding at
freeze.

**D5. [#57] `lease-stale`'s firing condition is never stated.**
**PARTIALLY CLOSED:** the boundary negatives are vectored
(`f09-lease-present-no-receipt-pends` — a valid lease with NO receipt
pends `lease-missing`; `f09-lease-overlong-window-invalid` — a window
exceeding `max_age_ms` is not a lease). The "conclusive staleness"
wording is withdrawn: `lease-stale` is staleness ON THE HELD EVIDENCE
(the disposition is quarantine-reproposal precisely because it is
re-proposable). What happens when a later TIMELY receipt reaches a
store that already classified stale — the convergence lifecycle — is
an owner decision: `decisions-pending.md` surfaces re-evaluation vs
sticky-rejection-plus-reproposal vs timeboxed-pendency with paired
endpoint vector drafts (unminted). No lifecycle is chosen or encoded.

**D6. [#52] The §5.5 state-6 re-derivation vs the erase-crash
contract.** **CLOSED (the oracle is gone):** finding 10 above —
companion amendment #4, signed rotation ops as the control context,
tombstones bound to the signed manifest, all 8 vectors re-minted.

**D7. [#54] The classification of a durable RewrapDone omitting an
expected survivor.** OPEN (prose ratification): pinned `(log-corrupt,
storage-quarantine)`; the D-89 serialization violation pins the same
pair.

**D8. [#61] The recovery arm's `repoch` on a NON-succession operation
(`c.drill`).** OPEN (prose ratification): pinned as the CURRENT
repoch.

**D9. [#70] No outcome named for an audit row contradicting its
read's established partition.** **CLOSED (implemented + vectored):**
five conflict vectors (duplicate chunk index, changed principal,
changed scope, changed count, overlapping result sets), each
`(body-invariant, reject-permanent)`, arrival-order-proof via the
chain. The prose row remains worth adding at freeze.

**D10. [#22] The companion's "fresh fold of the union" names no
arrival order.** OPEN (one clarifying sentence in §13.1).

**D11. [#46] Umbrella App C #2 (offline expiry confirmation) remains
unperformed.** OPEN — an explicitly-recorded owner choice;
`f14-offline-expiry-confirmation-pending` keeps it visible. Gate A
can pass with this open only by that recorded choice.

**D12. [#8] The op signature and op identity share the domain tag
`op`.** OPEN (design observation; no failing trace exists).

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
  tree, drift-gated, with the uncovered set (**22 of 59 outcomes**)
  pinned shrink-only in `core::coverage::UNCOVERED_10_4` — new
  coverage must delete its pin entry; a regression fails the suite.
- **`coverage/obligations-13-3.json`** — the §13.3 obligation ledger:
  84 line-ranged entries whose quotes must appear verbatim in §13.3
  and whose ranges must jointly cover the whole 492-line section (a
  spec edit that adds an obligation breaks coverage); every named
  vector must exist and statuses must match the lists. Current
  truth: **14 vectored, 25 partial, 43 pending** — the per-entry
  notes say exactly which clauses are missing (the f7 ceremony debt,
  f9 issuer feeds, f10 generation machine, f11 transfer composites,
  f13 checkpoint machine, among others).

Both artifacts state the executed surfaces are exactly
`rust-core` + `rust-reducer`: a vector's `surfaces` array is a §13.2
applicability ANNOTATION, never execution. The browser and per-OS
storage lanes have never run; `execution-lanes-plan.md` carries the
concrete plan and estimates (Chromium: 2–4 sessions; portable
storage 3-OS matrix: 1–2 sessions) and names the Gate-B production
concerns (fsync, keystores, IndexedDB failure/eviction,
Firefox/Safari) that stay distinguishable from both.

The reducer's honest frontier is 99 `Unimplemented` sites (58
distinct reasons); none is reachable from the committed corpus (the
strict gate proves it on every run). `p1-v1-profile.md` dispositions
every NORMATIVE one: five recommended implement-before-Gate-A
(erase-manifest fold admission, compromise-mode T4, rotation_refs
linkage, frontier-head validation residue, the cut-op re-fold arm)
and the rest explicitly fail-closed with named outcomes —
recommendations pending ratification.

## 5. Gate-A verdict

**FAIL.** Gate A is not stamped, and this document must not be read
as a conditional pass. The amended predicate — every clause must hold
before a future audit may say PASS:

1. **The strict gate stands** (nonzero exit on any structural
   failure, semantic FAIL, or Unimplemented; argv handled; the
   negative test in place). — **holds** (repair tranche 1).
2. **Real convergence**: every convergence-bearing vector lists ≥ 2
   byte-distinct orders, enforced structurally. — **holds** (tranche
   2; exposed and fixed the D-185 reservation gap).
3. **D1 executable**: the checksum-invalid rejection minted and
   independently verified. — **holds** (tranche 3).
4. **D4 executable**: signature precedes freeze classification, both
   vectors minted. — **holds** (tranche 4).
5. **D6 de-oracled**: the signed rotation context in the companion,
   tombstones bound to it, re-minted. — **holds** (tranche 5).
6. **The reopen trace is semantically valid and the kill is
   verified**, with pend and verified-false arms vectored. —
   **holds** (tranche 6b).
7. **No silent owner rulings**: D2 and D5 surfaced with alternatives
   and discriminating drafts, decided by NOTHING in the artifacts. —
   **holds** (tranche 6c/6d); the RULINGS themselves remain open.
8. **Machine-enforced coverage**: the §10.4 map + §13.3 ledger
   enforced in CI-visible suites; executed-surface honesty stated in
   the artifacts. — **holds** (tranche 7). The DEBT they expose (22
   uncovered outcomes; 43 pending + 25 partial obligations) is
   open: Gate A requires the owner to ratify a scope line through
   it explicitly — this audit proposes none.
9. **CI visibility**: the advisory, accurately-named
   reference-artifact workflow exists. — **holds** (tranche 7).
10. **Execution lanes**: Chromium and per-OS storage lanes either
    EXECUTED or covered by the ratified plan with the annotation
    caveat stated everywhere coverage is claimed. — **plan exists;
    lanes have not run.**
11. **The P1 v1 profile ratified**: every unimplemented normative
    mechanism implemented+vectored or fail-closed by owner
    ratification. — **written, unratified.**
12. **Owner rulings**: D2, D5 (lifecycle), D3/D7/D8/D10/D12 prose
    ratifications, D11's recorded acceptance, and the §4.7
    fork-discovery wire gap (finding 12) dispositioned. — **open.**

Clauses 10–12 are open; therefore **FAIL**. The remaining path is
owner action (rulings, profile ratification, scope line) plus the
execution lanes; no further artifact work is known to be required
for clauses 1–9.

Nothing in this verdict stamps the spec, opens P1, or amends the
Gate-A predicate silently; P1 writes stay barred until Gate B and
the P0.5/tombed-cutover sequence regardless.
