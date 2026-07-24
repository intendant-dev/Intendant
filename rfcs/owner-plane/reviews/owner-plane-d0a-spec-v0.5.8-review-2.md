# Review 2: D0-A Core + Memory specification v0.5.8

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.8 (3,178 lines, 118 decision rows D-1..D-119), against the
v0.5.7 synthesized review's six consolidated areas, the new rulings
D-114..D-119, and the v0.5.7 archive (300 changed lines diffed).
Written without reading `owner-plane-d0a-spec-v0.5.7-review.md`.
Method: composed traces against the new algebra (multi-gap lineage
closure; queued-epoch custody E2/E3/E4; both fork delivery orders;
erased-source-with-durable-pending-import; dormant-release identity),
then byte verification of every ruling. Two of my v0.5.7 items were
adjudicated against me — the E8 seals row existed (false pin) and my
proposed seal lower bound was itself fold-relative (the exact class
D-108 removed); both corrections are reflected in this round's
method: every "missing X" claim below was grepped before writing, and
no proposed fix compares against fold-current state.*

## Executive verdict

**All six v0.5.7-synthesis areas are discharged, and the centerpiece
is the one I asked for: the revisit inventory is now *derived*, with
the spec's own epitaph — "hand-numbered enumerations broke three
times."** T2 defines three derived classes (proof
retro-disqualification, budget displacement/revival, the boundary
events of the cutoff algebra) and closes with "Nothing else
revisits"; D-115 makes direct issuer-fork handling order-independent
the conservative way (holding two conflicting statements at one
sequence is a set property — both suffixes freeze, a committed
boundary later selects the winner, both delivery orders vectored).
D-114 completes the boundary algebra per generation (ratify binds its
named head's generation; caps clamp within their generation so
later-generation cleanup is legal; snapshot entries compose with the
scalar as per-generation ratifications bounded by carried heads;
seal/void quarantine split permanent vs ratify-revivable in §10.5;
truncation ratified as deliberate owner-grade authority with the
escaped-effect residual extended — and my non-portable lower bound
correctly rejected). D-116 makes renewal custody monotone (Kold until
**every accepted, unretired epoch** holds Knew coverage — the
active-only predicate that stranded queued intermediates is voided;
`held_zones` defined once; the recipient cap re-scoped per
(zone, accepted unretired epoch) across every wrap-bearing
operation). D-117 bounds adoption (E7 key corrected, ≤ 64 entries
with deep-fork orphaning as a stated residual, the effective wrap-add
map surviving as storage-not-authority, unFenced cut rotations never
adopted). D-118 lands the checkpoint/feed wording exactness, and
D-119 gives transfer durable-attempt semantics (a durable pending
import **defers the terminal** until admitted or
reject-permanent/fence-hardened — my quarantine-resolution question
is answered in the text), acceptance-consumed `export_id`,
`release_op` in all three journal records, and the reject-permanent
XferAbort correctly reclassified as journal cleanup outside the
finality gate. The stale assent-dies text is swept (zero hits);
B.2/B.3 hashes intact.

**One high finding and one pin — the smallest residue of any round:**
D-114's per-generation ratification entries collide with the
unchanged `(zone_id, lineage)` cutoff key, so the multi-gap closure
the prose still promises is unencodable in one operation; and the
derived inventory's class-(c) parenthetical under-names its own
algebra. Three line edits and one clause.

---

## Part 1 — Discharge audit

| # | v0.5.7-synthesis area | v0.5.8 disposition | Residue |
|---|---|---|---|
| 1 | Boundary algebra + lifecycle | **D-114**: per-generation binding/clamping; snapshot/scalar composition pinned ("the snapshot is the outer bound, the entries are its per-generation ratifications"); derived revisit inventory; split dispositions (§10.5: permanent seal/void vs revivable ratify); truncation posture ratified + residual extended; boundary facts monotone under displacement of their creating operation; stale text swept | **H1** (entry key vs per-generation entries); **P1** (class-(c) parenthetical) |
| 2 | Order-independent direct fork | **D-115**: both-suffix freeze (set property), committed-boundary selection, losing-branch stays quarantined / winning re-qualifies, both orders + transition vectored | none |
| 3 | Monotone renewal/membership | **D-116**: every-accepted-unretired-epoch Kold predicate; global recipient-key equality (same-operation enrollment exception); `held_zones` = grants ∪ effective wraps, defined once in §4.3, cited by every consumer incl. the E8 rows; recipient cap per (zone, accepted unretired epoch) | none |
| 4 | Bounded, dependency-complete adoption | **D-117**: E7 key `(zone_id, rotation_op)` (line 95 verified); E8 ≤ 64 with deep-fork orphaning residual; effective wrap-add map survives as storage state, never authority; accepted-but-unFenced cut rotations cut-and-reissued | none |
| 5 | Checkpoint/feed exactness | **D-118**: CDDL comment now "latest entry per `(lineage, generation)`, omission never removal"; lineage fence dominates every generation cover; `feed_closure` integrated into T3 (boundary commitment: ancestry gating, min-merge under ancestor proof, hardening exemption); `cabandon.seals` non-empty; `c.wrap_add` row carries the equality mirror | none |
| 6 | Transfer durable attempts + identity | **D-119**: `missing` = never-arrived ∨ reject-permanent only; durable pending import defers the terminal (resolution = admitted ∨ reject-permanent ∨ fence-hardened — stated); `export_id` consumed by acceptance event (dormancy never frees it; a revived release finds its journal unshared); `release_op` in `pendingxfer`/`xferdone`/`xferabort` (CDDL verified); reject-permanent abort = journal cleanup outside the finality gate | none |

---

## Part 2 — Findings

### High

**H1. D-114's per-generation ratification entries are unencodable
beyond one generation per lineage — the E7 key wasn't updated, and
the prose still promises what the key forbids.** The new composition
rule makes each `cutoffs[]` entry "name a generation present in the
carried `live_heads` … the entries are its per-generation
ratifications" — but E7 still keys zone/history/closure cutoffs by
`(zone_id, lineage)`, so a set with two entries for one lineage at
different generations is non-canonical. A hosted device with three
open gaps (the own-lineage self-service case sits in the same row)
can therefore ratify one generation per operation — while the row and
the A.3 comment both still say "one operation retires several
unknown-gap heads (D-80)", and D-80's ledger row promises "multi-head
unknown-gap retirement." An implementer honoring the promise emits
duplicate-key sets that strict decoders reject; one honoring E7
cannot express the ruled capability. Fix (three line edits): key the
snapshot-cutoff entries `(zone_id, lineage, gen)` — the natural
completion of D-114's per-generation algebra — in E7's table, the
row, and the A.3 comment (history/closure cutoffs may stay
per-lineage if their single-boundary semantics is intended; say
which). Alternatively keep one-per-op and fix both comments plus a
D-80 annotation — but that quietly downgrades a ruled hosted
capability from one ceremony to N.

### Pin

**P1. The derived inventory's class (c) under-names its algebra.**
"Retirement boundaries (per-generation seals, snapshot cutoffs)
quarantine" reads as exhaustive, but the cutoff algebra it derives
from also retires via **recover** boundaries (C3′ `tenant_cutoffs`
quarantining cut-branch admitted operations — a revisit that predates
every enumeration and was never in any of them) and **supersede**
boundaries (admitted old-key operations beyond a renewal's carried
cutoffs). Since the inventory is now derivational, make the
parenthetical explicitly exemplary ("e.g.") or name all retiring
purposes — one clause, and the derivation stays honest against its
own table.

---

## Part 3 — Gate-A readiness

Eight review rounds, each converging: the blocker/finding arc ran
missing verb → unencodable lane → missing verb list → un-landed audit
wire → required-key contradiction → stale absolute sentence →
non-carried field → enumeration off-by-one → and now a logical-key
line lagging its own ruling by one edit. Every composed trace the
syntheses demanded — arrival orders, crash matrices, queued-epoch
custody, fork deliveries, dormant identity — now closes on the text.
The residue is three line edits and a clause, there are no owner
decisions left to make, and the remaining Gate-A distance is entirely
executable artifacts: `d0a-vector-cases.v1.json` (still correctly
artifact-pending), `owner-plane-core`, the corpus, family 14, the
discrepancy audit.

**Recommendation.** Fold H1 + P1 as **v0.5.9** (or as the freeze
commit itself — the change is small enough that a separate review
round would cost more than it retires), then **freeze the prose as
the audit baseline**. Author the companion schema first, build the
core and harness, generate the corpus — the multi-gap ratification
vectors land exactly on H1's fix, so write them first — record
family 14, run every required surface, and let the
prose↔schema↔vector discrepancy audit make the Gate-A call. Durable
P1 writes remain prohibited until Gate B plus the umbrella's
P0.5/tombed-cutover prerequisites, unchanged.
