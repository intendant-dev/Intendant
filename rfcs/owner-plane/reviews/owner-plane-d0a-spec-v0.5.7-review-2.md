# Review 2: D0-A Core + Memory specification v0.5.7

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.7 (3,080 lines, D-1..D-113), against the v0.5.6 synthesized
review's seven consolidated areas, the new rulings D-107..D-113, and
the v0.5.6 archive (322 changed lines diffed). Written without
reading `owner-plane-d0a-spec-v0.5.6-review.md`. Method: composed
traces first (hosted gap → seal → restoration; snapshot orders;
queued-epoch custody; import-after-abort), then byte verification of
every ruling (CDDL, tag inventory, E7/E8 rows, §15 pointers,
registry↔Appendix mirrors).*

## Executive verdict

**All seven synthesis areas are discharged, and the two protocol
inventions are the deep versions rather than the patches:** D-107
resolves my v0.5.6 H1 with more than the ceiling row I proposed —
`c.abandon_writer` becomes **per-generation seals** (`{gen, at/"none"}`
≤ 64, keyed by gen, with the at/gen/lineage equality pins), so an old
gap closes while the live branch survives; `last_known` incorporation
becomes an explicit immutable cap (a crossing ratify cutoff is
`body-invariant`, killing the delayed-successor revival trace by
construction); the **hosted requester-attested self-gap seal** joins
the ceiling with the new `abandonreq` domain in the tag inventory, and
the D-15 row carries its "amended exactly once" pointer. D-108 makes
the hosted cutoff **snapshot-wins** — the carried `live_heads` ARE
the boundary, voiding the arrival-relative equality check that
diverged control-first and tenant-first replicas, with the five
ordering vectors named. D-109 fixes KEM-renewal custody the honest
way (**activation-based** predecessor-secret retention — the
acceptance-based rule that stranded queued epochs' KEKs is voided;
held zones ≤ 128 keeps renewal encodable). D-110 lands my
`recipientset.v` pin plus admission-enforced recipient cardinality.
D-111 materializes consumed ratify boundaries as immutable
close/supersede state, keys checkpoint replacement per
`(lineage, generation)` with omission-never-removal, and adds the
REQUIRED renewal `feed_closure`. D-112 makes precedence selection
pure-and-provisional and adoption a complete Fence-frame identity
with contiguous chains. D-113 adds the transfer `export_id` critical
section, PendingXfer dormancy, and — correctly — records the
rejection of my v0.5.6 formula pin (the formula was already right;
my error, now in the ledger).

**One blocking finding, one medium, two pins — and the blocker is a
sentence the last three rounds have already had to fix twice:** T2's
revisit enumeration still says "exactly **four** deterministic
paths," while D-107 seals and D-108 snapshot-wins cutoffs both
deliberately transition *accepted* operations to quarantine at their
fold positions — a fifth path the exhaustive list excludes. One
sentence fixes it. This cut is the genuine freeze candidate.

---

## Part 1 — Discharge audit

| # | v0.5.6-synthesis area | v0.5.7 disposition | Residue |
|---|---|---|---|
| 1 | Immutable gap-finality ceremony + hosted posture | **D-107**: per-generation seals (CDDL verified: set keyed by gen ≤ 64, `at = "none"` voids, unnamed generations untouched, unheld Head → `ref-unresolved`); incorporation cap; hosted self-seal (attested, snapshot-wins, `abandonreq` tag); §4.3's "until its cutoff" → "until an immutable seal or close"; family vectors (gap → cleanup → still blocked → self-seal → restored) | **B1** (revisit enumeration); **M1** (trusted seal truncation unbounded) |
| 2 | Portable hosted cutoff order | **D-108**: snapshot-wins ratified — uncarried successors retire at the op's fold position on every replica; extra held head = beyond-boundary, never staleness; missing carried head = `ref-unresolved`; five ordering vectors | B1 (same enumeration) |
| 3 | KEM renewal atomic + cardinality-safe | **D-109**: activation-based custody (retain predecessor secret until every held zone's ACTIVE epoch serves a renewed-key wrap); interim epochs via post-acceptance `c.wrap_add`; current-key equality (no future-key staging); held zones ≤ 128 (E8 row, 129th grant/wrap rejects) | none |
| 4 | `recipientset` versioning + cardinality | **D-110**: `v: 1` (CDDL verified); ≤ 256 current-epoch recipients enforced at enroll/wrap-add admission (E8 row) — every Fence commitment constructible | none |
| 5 | Promotion, paging, feed closure | **D-111**: consumed ratify → materialized immutable close/supersede at the consuming position (no re-widening); checkpoint replacement per `(lineage, generation)`, omission-never-removal; fence non-regression on the `(gen, seq)` projection; renewal `feed_closure {key_id, through, head_hash}` REQUIRED (CDDL present); closed scopes exempt from hardening coverage | none |
| 6 | Recovery selection + adoption dependencies | **D-112**: precedence pure/provisional, commits only through the final `state` stage (exact arrows incl. `key-malformed`); `adopted_rotations` re-keyed `(zone_id, rotation_op)` carrying the complete Fence-frame identity (`fence_frontier` added) with contiguous-chain validation; dependency closure (cut-branch certs/descriptors = validation material; adopted erase manifests stand); §7.4 literal aligned | none |
| 7 | Transfer/feed seams + sweep | **D-113**: `export_id` critical section (import can never commit after an XferAbort listed it missing); PendingXfer dormancy/revival/terminal; revisit path 4 broadened to direct **and** boundary-revealed issuer-fork discovery; §4.6 wording, D-80 note, `zonecutoff` purpose inventory, `zoneheads` key = gen; my formula pin correctly rejected and recorded | none |

B.1/B.2/B.3 untouched; the record is coherent through D-113 with
supersession pointers where rulings amended (D-15, D-104, D-106).

---

## Part 2 — Findings

### Blocking

**B1. T2 still says admitted operations are revisited by "exactly
four deterministic paths" — D-107 and D-108 just added a fifth.**
The enumeration (compromise cutoffs, budget displacement, ratify
revival, issuer-fork exposure) is exhaustive by design; it is the
sentence implementers build the fold's revisit logic from, and it has
already been corrected twice (v0.5.4's displacement, v0.5.6's
feed-fork). This round: D-108's snapshot-wins rule *deliberately*
retires **accepted** uncarried successors at the cutoff's fold
position ("an extra held head is beyond-boundary" — its operations
transition to quarantine), and a D-107 seal "quarantine[s]
permanently" accepted operations beyond its boundary. Both are
deterministic, fold-position-keyed, ratified — and absent from the
list. An implementer treating the enumeration as exhaustive will
refuse to quarantine an accepted successor when a snapshot cutoff or
seal folds; one following the D-107/D-108 rows will; accepted sets
diverge. Fix (one sentence): add the fifth path — **snapshot-boundary
retirement** (a requester-attested snapshot cutoff or seal retiring
accepted operations beyond its carried boundary at its fold
position) — and, while there, disambiguate the seal row's "outside
the four revisit paths" (it means seals are not *revisitable* —
receipt-independent — not that they never revisit). Given three
corrections in four rounds, consider making the enumeration derive
("the boundary algebra's retiring events, §7.1") rather than count.

### Medium

**M1. The trusted-form seal has no lower bound — arbitrary-depth
truncation authority rides an operation ratified as a gap-closer.**
The hosted form is snapshot-wins over carried heads; the trusted
(plain admin) form accepts any `at`, so an admin seal below a
generation's accepted terminal head permanently quarantines accepted
history — recovery-grade branch amputation without C3′'s ceremony,
placement, or tenant-cutoff discipline. D-107's stated intent is
closing gaps while "later live branches survive." Either constrain
`at` ≥ the generation's accepted terminal head at the seal's fold
position (making seals pure future-fences; B1's fifth path then
covers only snapshot retirement), or state the truncation authority
deliberately with its own negative vectors. One sentence either way;
the first option is the conservative one.

### Pins

1. **E8 lacks the seal-entry row**: `cabandon.seals` ≤ 64 lives only
   in the CDDL comment; every other ≤ 64 cap (cutoffs, rotation refs,
   proof positions) has an E8 row. Add it.
2. **Name the seal-retirement disposition explicitly** in §10.5's
   quarantine row (the `cutoff` outcome covers beyond-boundary ops;
   one parenthetical "(incl. seal/snapshot boundaries — permanent, no
   ratify revival)" keeps the disposition table honest about the
   no-reproposal-success case).

---

## Part 3 — Gate-A readiness

Every structural question from eleven review rounds now has ratified
machinery with verified bytes. The residue is one sentence
(an enumeration that keeps losing count of its own algebra — the
strongest argument yet for deriving it), one constraint choice, and
two table rows. The composed traces that defined the last three
rounds — hosted gap restoration, snapshot ordering, queued-epoch
custody, import-after-abort — all close on this text.

**Recommendation.** Fold B1/M1/pins as **v0.5.8** (four sentences,
no new machinery), and treat that as the freeze: this document is the
audit baseline. Then, in order: author `d0a-vector-cases.v1.json`
(the first corpus artifact — D-91 stays artifact-pending until every
fixture validates against it), build the independent
`owner-plane-core` and harness, generate the corpus with the
ordering/cardinality/composition traces the syntheses named (the
five snapshot orders, the seal-restoration composition, the adoption
chains, the third-rotation survivors, the four-plus-one revisit
paths), record the family-14 offline result in §15, run every
required surface, and let the prose↔schema↔vector discrepancy audit
decide Gate A. Durable P1 writes remain prohibited until Gate B plus
the umbrella's P0.5/tombed-cutover prerequisites — unchanged, and
correctly so.
