# Review 2: D0-A Core + Memory specification v0.5.12

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.12 (4,051 lines, D-1..D-151), against the v0.5.11 synthesized
review's six cross-ruling counterexamples and key-lifecycle pass.
Written without reading `owner-plane-d0a-spec-v0.5.11-review.md`.
Method: the four-question floor, applied as wire verification of all
nine rulings plus the synthesis's named traces; scope note — this
round's composed-trace depth is thinner than my last two reviews, and
the freeze judgment belongs to the synthesis in any case.*

## Executive verdict

**All six cross-ruling counterexamples and the key-lifecycle pass are
discharged, with two honest reversals that raise confidence rather
than lower it.** D-143 fixes the worst trace of the series — v0.5.11's
authority-blind `immutable_cap` meant every strict zone **bricked on
its first epoch advance** (the close quarantined the successor
certificate's writes too); closures are now frontier-shaped
(`frontierclose` with the D-135 total-override pattern, CDDL verified)
and **selector-qualified** (close → old epochs; supersede → the
predecessor cert; revoke → the revoked grant; recover → global), with
`immutable_cap` re-scoped to authority-agnostic seals and caps.
D-146 is the first honest reversal: the D-139 one-grant lemma is
admitted false (grant turnover and unknown-gap generations both defeat
it), and import replay-key ownership becomes **provisional until
effect finality, frozen there** — using D-94's own closure as the
no-late-claimant proof, which is the structurally right dependency.
D-147 is the second: the flat `H_bundle` domain is retired (a
committed partial import lost its proof when a sibling was erased)
for a Merkle root over `H_bhdr`/`H_brec` leaves with per-record
`rec_index` + ≤ 8-sibling `proof` in `mimport` — durable, per-record,
erasure-proof (tag inventory and CDDL verified). D-148 makes the
journal a transition fold — interval-chained `XferReopen` with
`incarnation` (wire verified), per-record `? basis` on `missing`,
incarnation-scoped terminal effect keys (a consume-once deduper would
have suppressed every post-reopen terminal). D-149 resolves the
total-re-fold vs never-re-enters contradiction by splitting rejection
permanence **by the rejecting fact** — intrinsic byte failures never
re-enter anywhere; control-derived rejections are branch-relative and
re-evaluate under the C3′ re-fold (§10.5 verified). D-150 makes the
key lifecycle recovery-stable: KEM-key freshness (cross-device reuse
and K0→K1→K0 both reject), `adopted_renewals` (the D-112 pattern —
without it the re-fold silently re-keyed to a destroyed secret), and
`retired_keys` as the **portable** freshness domain, with the
uncarried-cut-branch-key residual stated and vectored rather than
hidden. D-144 anchors cap eligibility to chain membership (the
truncation-assertion posture, all three folds converging), D-145
widens reservation to the selection scope, and D-151 lands the
composition pins, the family-11 mirror battery, my M1 (the `mclaim`
import arm **structurally deleted** — the three remaining mentions
are the deletion comment), and an explicit Gate-A status line
("pending — currently false") in §16.

**Findings: none in scope.** No blocking, high, or medium survived
the checks I could run; I found no wire gap in any of the nine
rulings, no dangling consumer of the retired machinery, and no stale
mirror among the ones I chased.

---

## Gate-A readiness

Thirteen rounds. The last three cuts each replaced a subtly wrong
mechanism with a structurally right one under adversarial synthesis
pressure — and this cut's two reversals (D-146, D-147) show the
process correcting its own recent rulings when the composed protocol
demanded it, which is the property a freeze decision should actually
rest on. The specification now states its own Gate-A status honestly
as false pending artifacts. My verdict is deliberately scoped: the
wire is clean under my checks, the named traces close, and whether
this text is the audit baseline is the synthesis's call.

**Recommendation.** If the synthesis concurs, proceed at last to the
artifact sequence: `d0a-vector-cases.v1.json` first — its opening
tranche now includes the selector-qualified closure matrix, the
finality-frozen import handoff, the Merkle construct-and-rederive
with per-record proofs, the incarnation-chained terminal cases, the
D-149 permanence split, and the D-150 freshness-domain traces — then
the independent core and harness, the corpus, family 14, every
required surface, and the prose↔CDDL↔companion↔vector discrepancy
audit as the Gate-A decider. Durable P1 writes remain prohibited
until Gate B plus the umbrella's P0.5/tombed-cutover prerequisites,
unchanged.
