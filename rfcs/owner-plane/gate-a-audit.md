# D0-A Gate-A Discrepancy Audit

**Date:** 2026-07-14 · **Auditor:** the artifact-phase differential program
**Spec:** `owner-plane-d0a-spec.md` v0.5.19, SHA-256 `410880e04433b629d5d11956e322f59832494d8f25042b3dfcf34d8b694c6748`
**Companion:** `d0a-vector-cases.v1.json`, SHA-256 `a48d7a376836ea02016dbc21d36d52d5f1f495a0ccc84b7f71e5b42f2a183e0b`
**Corpus:** 131 vectors (f01×17, f02×7, f03×6, f04×4, f05×4, f06×6, f07×17, f08×3, f09×7, f10×7, f11×18, f12×15, f13×16, f14×4)
**Suites at audit:** core 135/135 · reducer 30/30 · fmt/clippy clean both crates · mint byte-idempotent

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
  BIP-39 leg, and B.2/B.3 policy transcription.

The harness validates every vector against the §13.1 container schema
(extracted from the spec's own fenced block and compiled by a real
Draft 2020-12 engine), the companion vocabulary and per-case_kind
contracts, the §10.4×§10.5 pair relation, a strict-decode differential
over every byte input, and the three-run converge standard (every
listed delivery order plus a fresh fold of the union). All 131 vectors
pass all layers.

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

Live findings, each caught by the differential before commit:

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
   `browser` surface; §13.2 pins family 11 to `core` only (browsers
   exercise folds through the shared WASM core lane). Corrected by
   the surfaces gate on its first run.
5. **Companion defects (2 amendments, recorded in the ledger):**
   (a) fold `per_item` originally required an (outcome, disposition)
   pair on every row, making the all-admitted case unexpressible
   under E10 (pairs are pinned to failures) — widened to
   `dependentRequired`; (b) two `$comment` annotations sat inside
   `properties` objects, which a conforming Draft 2020-12 engine
   cannot compile — moved to legal positions. Both amendments are
   backward-compatible; the companion hash above includes them.
6. **Reducer defect (self-caught):** its first key_id draft emitted
   `alg` before `pk`; its own strict reader rejected the
   non-canonical order (canonical encoded-byte order sorts `pk`
   first). Core was already correct via its sorting writer.

## 2. Discrepancies requiring an owner ruling before freeze

These are places where the artifacts disagree with each other or the
prose is silent on something a conforming implementation must decide.
Each is pinned by a committed vector or convention; ratifying the
pinned reading (or amending it) closes the item. Register numbers in
brackets.

**D1. [#38] §2.4 checksum-invalid phrase rejection is unexpressible
under the companion.** The `phrase-derive` contract requires
`result.keys`; a checksum-invalid mnemonic has none. Either the
companion gains a negative arm for `phrase-derive` or §2.4's negative
is documented as covered by implementation tests only.

**D2. [#47] §11.4 has no actor-class row for a bare autonomous
non-human unattested writer.** Both engines derive `session` (the
closest §10.1 reading); the status corpus pins it. The table should
gain the row.

**D3. [#63] No (outcome, disposition) exists for a control operation
cut by a C3′ branch cut.** E10 demands totality; both implementations
classify cut control ops `(cutoff, quarantine-reproposal)` — the
D-140 recover-boundary reading — pinned by
`f07-walkthrough-c3-branch-cut-below-head`. §10.4/§7.4 should name
it.

**D4. [#65] No stated classification for a control op arriving while
the plane is C2-frozen.** The reducer classifies `(ctrl-fork,
freeze-control)` (implemented, deliberately NOT minted). Needs prose.

**D5. [#57] `lease-stale`'s firing condition is never stated.** §9.1
defines the two pendings; §10.5 places `lease-stale` in
quarantine-reproposal; nothing says when it fires. Pinned reading
(f09-lease-stale): a held QUALIFIED receipt outside every valid lease
window is conclusive staleness → `lease-stale`; no held receipt at
all remains `lease-missing` (pending). An invalid lease (window >
`max_age_ms`) is not a lease → the `lease-missing` lane [#59].

**D6. [#52] The §5.5 state-6 tombstone re-derivation reads the
control op's `erase_manifest`, but the erase-crash-matrix contract
carries only `{stream, cuts, machine_state}`.** The corpus stands in
with the manifest oracle (the FULL stream's tombstone set, keyed
`retired_epoch = new_epoch − 1` — the same typed entries in durable
form). Ratify the convention or amend the companion with a
control-context input.

**D7. [#54] The classification of a durable RewrapDone omitting an
expected survivor.** §5.5 says "omission of any survivor blocks
destruction" but names no outcome. Pinned: `(log-corrupt,
storage-quarantine)` — a false completeness commitment is a log
invariant violation ("completeness is provable"); blocked destruction
is the consequence. The D-89 serialization violation (an N+1 Fence
before N's tombstones) is pinned to the same pair.

**D8. [#61] The recovery arm's `repoch` on a NON-succession operation
(`c.drill`).** Pinned: `repoch` = the CURRENT repoch (a drill is a
proof, not a succession; C3′ alone uses current+1). §7.1's drill row
should state it.

**D9. [#70] No outcome is named for an audit row that contradicts its
read's established partition** (duplicate index, differing
principal/scope/count, overlapping result sets). Pinned:
`(body-invariant, reject-permanent)`.

**D10. [#22] The companion's "fresh fold of the union" names no
arrival order.** The harness delivers sorted-by-name and relies on
fixpoint re-evaluation; the converge standard makes the choice
unobservable for a correct reducer — worth one sentence in §13.1.

**D11. [#46] Umbrella App C #2 (offline expiry confirmation) remains
unperformed.** `f14-offline-expiry-confirmation-pending` pins the
§4.5 working position and the §15 recording obligation. Gate A can
pass with this open only if the owner accepts the recorded PENDING
status (the fixture exists precisely to keep it visible).

**D12. [#8] The op signature (`msg("op", header)`) and op identity
(`H_op(triple)`) share the domain tag `op`,** separated only by
content shape. Flagged as a design observation (per D-200, no prose
change without a failing trace — none exists; the shapes are
disjoint).

## 3. Derived conventions to codify (clarifications, no behavior change)

Fixture-layer and engine conventions both implementations share; each
should land as a §13 note or a registry-row sentence at freeze:

- **Fold-vector conventions [#10, #29–#36]:** `per_item` = exactly one
  row per delivered item, absence-of-pair = finally admits; trace rows
  assert failure intermediates only; duplicates are edge facts about a
  DELIVERY, never overlaid by the shared op's fold state.
- **Aux is held state, not folded events [#56, #58, #60]:** the fold
  lane's `aux` carries the §5.6 index (`index` = a CBOR array of
  `{item_addr, op}` maps) plus `Signed` receipts/leases, validated
  lazily at admission (qualification is a pure function of receipt
  bytes, operation bytes, and control history — an unheld issuer
  certificate simply fails qualification). Receipt-arrival dynamics
  are outside the lane.
- **Journal conventions [#12–#17, #27]:** the journal machine
  validates holding/basis/interval arithmetic, not cause-sufficiency;
  a Txn with an invariant-violating record discards whole; release/
  source ids are opaque to the machine; state probes are
  fixture-named canonical CBOR of derived constructs.
- **Erase-crash conventions [#51, #53, #55]:** one tenant log per
  vector; `machine_state` = the state of the CONFORMANT durable
  prefix at every cut; Fence commitment fields are opaque in the
  storage lane (mirror-checked, probe-recovered); an empty-manifest
  rotation completes state 6 vacuously at `KekDestroyed`.
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
  `capability_epoch`) → window ordinal — equivalent to §4.3's
  "most recent bump at or before the epoch-opening control op" while
  epochs open densely one advance at a time, which the registry
  guarantees.

## 4. Coverage inventory (§13.3 obligations vs the corpus)

Families **1–6, 8, 12, 14 closed** for their §13.3 lists at Gate-A
scope. Substantial but partial: **7** (17 vectors: bootability both
provenances, the ceiling positives/negatives, drill, C2 freeze-both,
C3′ below-head cut + placement + precedence exception, D-190/D-195/
D-196/D-199 arcs), **9** (the deadline/lease core: qualified accepts,
T2 self-exclusion, witnessless zone, T5 both-legs, lease-stale),
**10** (epoch currency, forks, gaps, budgets ×3), **11** (judgments,
status fold, export/import, merkle, collision, erase, audit
partition), **13** (framing, corruption, crash, locks, the full
erase-crash matrix).

**Open corpus debt** (explicitly out of this phase's scope; none
blocks the D0-A format freeze, all block Gate-B-grade confidence in
their subsystems):

- f7: solo→multi transition, witness loss, compromise cutoffs (T4),
  renewal union shapes + custody, the cutoff algebra ceremonies
  (ratify/snapshot/override cycles), checkpoint machine, abandon-seal
  arcs, adoption bounds, staged >128-membership ceremony.
- f9: issuer feeds/forks/gaps, the cross-carrier commitment registry,
  key-freshness arcs, service-key descriptors ("connect" witnesses),
  fence-hardening at GC.
- f10: `w.gen` generations (displacement across generations,
  reauth revival, abandon seals) — everything gen>1 is gated on the
  generation machine, deliberately Unimplemented.
- f11: §11.2-row admission negatives; the ancestry exemption
  (`causal_references` never exercised [#49]).
- f13: Txn-recovery composites, the IndexedDB subset lane.
- Byte surcharges (export `record_count × 512`, D-98) uncharged
  [#67].

The reducer's honest frontier (every `Unimplemented` reason, 37
distinct) is greppable and matches this inventory; no vector reports
Unimplemented — the committed corpus is 131/131 Pass.

## 5. Gate-A verdict — recommendation

The §16 checklist asks for review, a reference implementation with
green vectors, and the discrepancy audit. The differential program
delivered: two independent implementations agreeing byte-for-byte on
every pinned constant; a 131-vector corpus with real signed bytes;
three-run convergence everywhere, including through control forks,
branch-cut recoveries, and budget displacement; and this audit.

**Recommendation: PASS Gate A conditional on ratifying §2's twelve
items** — eleven are one-to-three-sentence prose/table amendments
adopting readings the corpus already pins (D1–D10, D12); D11 stays an
explicitly-recorded open confirmation carried past the gate by owner
choice. None changes any committed byte. The §3 clarifications can
ride the same editing pass or a post-freeze editorial commit. The §4
debt is Gate-B-lane work and does not gate the D0-A format freeze.

After ratification: stamp the spec (v0.6.0 or the freeze label), pin
the new spec hash in both crates' drift gates, and declare P1 open
per §16.
