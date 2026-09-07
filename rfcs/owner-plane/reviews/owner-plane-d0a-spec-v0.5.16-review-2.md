# Review 2: D0-A Core + Memory specification v0.5.16

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.16 (4,666 lines, D-1..D-183), against the v0.5.15 synthesized
review's seven hard blockers, the D-173 exactness item, and the E10
closure debt. Written without reading the peer review. Scope
declared exactly: this is an **insertion audit** (every ruling chased
to its bytes, withdrawals chased to their live-text replacements) —
composed-trace replay is the synthesis's instrument, and the freeze
judgment is its.*

## Executive verdict

**All seven blockers, the D-173 item, and the E10 debt are
discharged — and this round's dominant move is honest withdrawal:
three v0.5.15 mechanisms are removed because their premises failed,
each replaced by something simpler.** D-176 scopes stage consumption
to the consumer's own coverage domain (lineage B's renewal can no
longer burn lineage A's stage — inert for A, one-shot-spent before
A's boundary) and adds renewal to the required-coverage pending
consumers its enumeration had omitted. D-177 resolves the
collision-terminal representation problem by proving it
**unreachable** — a collided record was either imported by its winner
or the terminal defers on the unresolved attempt, so no trace lists a
collision in `missing`; the conjunctive cause and its vector are
withdrawn *with the reachability*, and bases revert to one typed
cause. D-178 is the sharpest withdrawal: v0.5.15's stored-binding
repair — which I had verified as landed — is removed as
**carrier-less** ("prose storage is not storage": no `txnrec`, frame,
or authenticated record held the binding, and an importer-mintable
cache proves nothing), replaced by the simpler and honest **erasure
wins** (a not-yet-accepted import dies with its erased source — which
is what source-erased means; accepted-not-effect-final still defers).
D-179 re-founds the journal cause on what the journal actually is —
replicated log bytes, so the recorded cause IS canonical — with the
typed `factref` union (op hash / issuer `stmt_id`, CDDL verified) and
withdraws `XferAbort.at` + minimal-hash selection as solving a
non-problem (a control coordinate could not order tenant facts, and
never needed to). D-180 makes revocation completion **state-derived**
(authorship cutoffs total ∧ decryptable-wrap domain empty at the
completing position), exposing v0.5.15's references-cover-the-domain
rule as ceremonial — if the rotation exists its zone is already gone —
and retyping `rotation_refs` as D-71 freeze linkage (mandatory on
hosted planes); the dead 65-zone continuation text dies with it.
D-181 flips certificate×grant compatibility to the correct
upper-bound-only form — v0.5.15's within-span requirement rejected
every renewed certificate's inherited grants (grants bind `device_id`
and deliberately survive renewal) — while still killing the
resurrection direction, and splits `cert-superseded` contextually via
the §10.5 star pattern. D-182 makes freshness matching
**candidate-side** across the closed v1 role tags (a candidate point
checked under both `H_key` taggings against the opaque sets —
recovery-portable with zero wire change), with the exact-SEC1
equivalence boundary and its residuals (P vs −P, related-key
derivation) stated rather than hidden. D-183 sweeps the mirrors (the
§4.6/§9.3 retirement split, the adoption CDDL introduction, `factref`
in journal citations) and maps all eight outstanding E10 states to
outcomes and dispositions.

**Findings: none in scope.** The three withdrawals are clean — each
superseded rule's ledger row carries its pointer, and no live text
still depends on the withdrawn mechanism (the one surviving
stored-binding mention is the D-170 ledger row itself; the live §11.8
text carries erasure-wins).

## Recommendation

Seventeen rounds. This cut answers the question the last three raised
— what happens when the synthesis's own repairs are wrong — and the
answer is that they get withdrawn with stated reasons and replaced by
simpler rules, which is the behavior that distinguishes a
specification process from an accretion process. The withdrawals also
carry a lesson this review adopts for its own method: my v0.5.15
verification of the stored-binding repair confirmed prose, not a
carrier — "prose storage is not storage" now joins my floor
(a durable fact must name its `txnrec`/frame/record, or it does not
exist). If the synthesis's replay concurs that the seven are closed,
the artifact sequence begins: companion schema first (the
coverage-scoped consumption cases, erasure-wins schedules, `factref`
journal citations, upper-bound compatibility negatives, and
candidate-side freshness matches in the opening tranche), then the
independent core and harness, corpus, family 14, surfaces, and the
discrepancy audit as the Gate-A decider. Durable P1 writes remain
prohibited until Gate B plus the umbrella's P0.5/tombed-cutover
prerequisites, unchanged.
