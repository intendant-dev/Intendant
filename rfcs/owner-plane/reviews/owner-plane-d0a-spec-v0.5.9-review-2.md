# Review 2: D0-A Core + Memory specification v0.5.9

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.9 (3,383 lines, D-1..D-126), against the v0.5.8 synthesized
review's six-area change set, the new rulings D-120..D-126, and the
v0.5.8 archive (489 changed lines diffed). Written without reading
`owner-plane-d0a-spec-v0.5.8-review.md`. Method: the synthesis's own
three-run standard applied to each disputed trace — fix one byte set,
fold both delivery orders, rebuild fresh, require identical durable
facts — plus wire verification of every ruling (CDDL, E7/E8, mirrors,
ledger pointers).*

## Executive verdict

**All six areas are discharged, and the arrival-history defect class
is systematically dead — not patched per instance but removed by
construction.** D-120 gives ratify its own wire shape
(`ratifycutoff` keyed `(zone_id, lineage, gen)` — the synthesis's
recommended repair, not my three-annotation one) while the five
immutable purposes deliberately keep scalar `zonecutoff`; the
requester-cardinality pin lands (requester-present cutoffs bind the
requester's own lineage on every plane; trusted multi-lineage
ceremonies go requesterless and skip the snapshot composition —
equation-only). D-121 publishes the effective-state equation the
composition needed: per `(zone, lineage, gen)`, `ratified_through`
(monotone max), `admit_bound` (control-order fold with
**override-then-growth** — the snapshot override *is* the retirement
event, later larger ratify is growth and revives, closing the
both-admitted-and-retired H3 trace), and `immutable_cap`
(min-composed seals/incorporation caps, `"none"`/void strongest);
admission = `p ≤ min(admit_bound, immutable_cap)`, and §10.5 splits
permanent from revivable quarantine exactly on the immutable line.
D-122 fixes cap provenance the portable way: an incorporation cap
exists for every **held** signature-valid `w.gen(last_known)` — a
set property, restriction-only, fail-closed — and clamps ratify **at
evaluation** rather than as arrival-relative admission rejection
(budget acceptance, displacement, revival, and proof
retro-disqualification neither create nor remove it; I walked the
accepted-then-displaced vs never-accepted vs fresh-rebuild triple and
all three converge). D-123 demotes `export_id` to a correlation
label with no fold semantics and re-keys everything by `release_op` —
unique by construction — including the wire ripple into
`mimport.provenance.import` and the bundle (verified). D-124 creates
the per-issuer-scope commitment registry across all three carriers
with chain-ancestor-verification, making the first commitment above a
fork *be* the D-115 selection, and enrolls renewal `feed_closure`
into revisit class (a). D-125 splits control-derived admission caps
from local custody (the `unretired` qualifier that read Fence state
is gone from admission; Kold custody keeps it — its correct domain),
fixes the renewal set to effective-wrap zones, and anchors wrap
equality to the effective unsuperseded certificate. D-126 totalizes
transfer terminals (`missing` = no **unresolved** durable attempt;
the abort reason split adds `"release-rejected"` as journal cleanup
vs `"reject-permanent"` as a true finality-gated terminal — all three
reasons in the CDDL).

**Findings: one pin. No blocking, no high, no medium — a first for
this series.** The residue is a consumer-naming clause and then only
artifacts.

---

## Part 1 — Discharge audit

| # | v0.5.8-synthesis area | v0.5.9 disposition | Residue |
|---|---|---|---|
| 1 | Purpose-exact boundary encoding | **D-120**: `ratifycutoff` shape + per-generation key (CDDL + E7 verified, dual with scalar `zonecutoff` for immutable purposes); per-generation `"none"`; `head.gen == gen` equality; requesterless/requester cardinality pinned | none |
| 2 | Effective boundary reducer | **D-121**: the three-component equation in §9.3 (verified verbatim); override-then-growth; min-composed immutable caps; §10.5 disposition split on the immutable bound; hosted exception renamed **self-seal** with its truncation authority named honestly | pin 1 |
| 3 | Acceptance-history consensus facts | **D-122** (held-bytes caps, evaluation clamp — three-run walked); **D-123** (`export_id` correlation-only; courtesy edge-reject replicates nothing) | none |
| 4 | Commitment unification + revisits | **D-124**: one registry per issuer scope spanning `receipt_cutoffs`/`feed_closure`/`proof_positions`; ancestor-verify `body-invariant`; first-above-fork = selection; at-or-below-fork selects neither; closure retro-disqualification + escaped-effect residual | none |
| 5 | Portable membership vs local custody | **D-125**: caps per (zone, ACCEPTED epoch); `held_zones` = grant-named ∪ effective-wrap zones, wildcards nothing; renewal + history coverage over effective-wrap zones (grant-only zones gain no access); effective-certificate KEM equality (no wrap-back after Kold destruction); `recipientset` mirror aligned | none |
| 6 | Total transfer terminals | **D-126**: unresolved-attempt rule (the one-record reject-permanent transfer can now write its non-empty abort); three-way reason enum on the wire; release-rejection cleanup vs destination-rejection terminal, the latter gated at the release's effect-final coordinate; `release_op` in the prose shorthands | none |

Ledger integrity: D-53/D-65/D-80/D-101/D-113/D-114 carry their
supersession pointers; B.2/B.3 hashes intact; the archive-not-summary
row-count lesson is visible in the record's own hygiene.

---

## Part 2 — Findings

### Pin

**P1. Name `ratified_through`'s consumers.** The equation defines
three components; admission consumes `min(admit_bound,
immutable_cap)`, and the only other stated use of `ratified_through`
is its evaluation clamp. If it drives checkpoint
incorporation-eligibility or renewal history validation (the
plausible consumers), say so at the equation; if nothing consumes it,
it is a dead aggregate the discrepancy audit will flag — one clause
either way, and the companion schema's family-10 cases need to know
which facts to assert.

---

## Part 3 — Gate-A readiness

Nine rounds of two-review adjudication, and this is the first with an
empty severity ledger above pin. The v0.5.8 residue class —
arrival-history facts masquerading as fold state — was not patched
but eliminated: every durable fact I can find now derives from held
bytes, control order, or signed content, and the five traces the
synthesis demanded (snapshot-over-ratify, displaced-cap, export
reuse, cross-carrier winners, Fenced/unFenced caps) pass the
three-run standard. The specification's remaining distance to Gate A
is exactly its stated artifact list: `d0a-vector-cases.v1.json`
(first), the independent core and harness, the corpus, family 14, the
surfaces, and the prose↔CDDL↔companion↔vector discrepancy audit.

**Recommendation.** Fold P1's clause without a version bump (or as
v0.5.9's final edit), declare this text the **candidate audit
baseline** — the synthesis's own term — and stop cutting prose.
Author the companion schema first and write its earliest cases
against the newest machinery, where fixture-invention pressure is
highest: the D-121 equation table (override-then-growth, min-compose,
the §10.5 split line), D-122's held-vs-accepted cap traces, D-123
replay keys, D-124 fork-selection and at-or-below-fork negatives, and
D-126's three-reason terminals. Then core, corpus, family 14, all
surfaces, and the discrepancy audit as the Gate-A decider — where any
finding should now be editorial drift, because that is all that is
left. Durable P1 writes remain prohibited until Gate B plus the
umbrella's P0.5/tombed-cutover prerequisites, unchanged.
