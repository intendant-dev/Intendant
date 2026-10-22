# Review 2: D0-A Core + Memory specification v0.5.14

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.14 (4,408 lines, D-1..D-167), against the v0.5.13 synthesized
review's eight freeze-blocking clusters. Written without reading the
peer review. Scope: wire verification of all eight rulings in full
(rows, CDDL shapes, mirrors) — composed-trace depth limited and
stated, freeze judgment the synthesis's.*

## Executive verdict

**All eight clusters are discharged, and three of the repairs fix
defects in the process's own recent rulings — the strongest kind of
evidence that the adjudication machinery works.** D-160 pins the
staged-frontier state machine (a stage exists only at its carrier's
acceptance; strict consumers pend behind held-but-pending carriers —
the reservation pattern again; vacuous consumption for
already-closed lineages; state reconstructs from positions alone, no
expiry machinery). D-161 makes import dispositions
**arrival-invariant** — the order-loser is always
quarantine-reproposal, displaced or arrived-second, fixing a
fold-relative disposition the synthesis found in its own prior
ruling; freeze becomes reservation-aware (an admissible earlier
claimant pending proof reserves the key — a held-bytes fact). D-162
collapses the two-validator disagreement: admission is per-record
always (leaf + path against the signed root), whole-bundle
re-derivation demoted to transport integrity, and the leaf preimage
becomes the named versioned `bundleleaf` production (E6 discipline).
D-163 makes reopens citable (`basis` + `invalidation` on the wire —
verified) with the canonical basis as a state function — the
**minimal** op hash among sufficient facts, replacing
first-in-fold-order which had no total cross-feed order. D-164
corrects an inverted negative — v0.5.13's adoption bound rejected
exactly the valid adoptions — and types same-device adopted-KEM
eligibility to the terminal key only. D-165 fixes the recurring
unencodability genre caught by the synthesis this round: `cutoffs`
becomes `[*]` so a zero-authorship device is legally revocable, and
completion requires **both** domains independently (the
many-author/zero-wrap ceremony no longer completes on wraps alone;
the three boundary vectors pinned). D-166 publishes the authority
predicate once — certificate closure ∩ grant closure ∩ epoch closure,
each axis consulted by its own selector, absence leaving D-86
position-relative validity — and rewrites §4.2 and the §10.2 cert
stage to cite it (a legitimately-uncovered zone no longer rejects
everything beyond coverage). D-167 sweeps the mirrors: the
at-or-before-base blanket + post-base-enrollment exception into E8
and the recovery CDDL (v0.5.13's mirrors quarantined the
post-recovery first write the main text admitted), typed `key_id`
freshness comparisons, the explicit uncarried-cut-branch-key
statement, `last_known` dropping the acceptance requirement
(held-chain membership only, completing D-144), and the fourteen
acceptance traces folded into families with outcomes and
dispositions.

**Findings: none in scope.** Every ruling has its bytes; the shapes,
dispositions, and mirrors I chased are consistent.

## Recommendation

Fifteen rounds. This cut's texture — an inverted negative, an
off-by-one encodability, a fold-relative disposition, all caught by
the synthesis and all fixed structurally — is exactly the endgame the
two-review process was built for: the errors are now the size that
executable vectors catch mechanically, and three of eight fixes were
corrections to rulings less than two rounds old, which argues the
remaining risk lives in composition the corpus will exercise, not in
missing design. If the synthesis concurs, begin the artifacts:
companion schema first (the fourteen folded traces, the bundleleaf
production, the minimal-basis map, the three revocation boundary
vectors, and the reservation-aware freeze cases in the opening
tranche), then the independent core and harness, corpus, family 14,
surfaces, and the discrepancy audit as the Gate-A decider. Durable P1
writes remain prohibited until Gate B plus the umbrella's
P0.5/tombed-cutover prerequisites, unchanged.
