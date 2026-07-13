# Review 2: D0-A Core + Memory specification v0.5.17

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.17 (4,736 lines, D-1..D-190), against the v0.5.16 synthesized
review's residue list (one carrier-less replacement, one missing
reservation/cause table, two live-text-beside-new-law residues, the
§10.5 edit debt, the frontier predicate sweep, E10 completion, and
the P/−P threat entry). Scope declared: insertion audit — every
ruling chased to bytes, residues classified live-vs-ledger; composed
replay and the freeze judgment are the synthesis's.*

## Executive verdict

**All eight residue items are discharged, and the round's centerpiece
applies my own adopted floor better than I did:** D-184 re-founds
erasure-wins on **the commit as the erasure boundary** — v0.5.16's
rule keyed on *acceptance*, an uncarried history fact, the very class
D-178 had just withdrawn (a proof→accept→erase cold rebuild held no
evidence source equality ever ran); the ItemCommit is authenticated
durable bytes written only after equality passed, and the
`release_op` critical section serializes it against erasure, so the
order is **in the log** and both schedules plus a cold rebuild
converge by replay. D-185 adds the journal's terminal-first
reservation (an Abort/Reopen citing an unheld factref pends and
reserves `(release_op, incarnation)`; later transitions pend behind
the earliest — journal order survives pendency; held-invalid =
`log-corrupt`) and tightens the cause model: **basis is always
op-kind** (no reachable trace makes an issuer statement a sufficient
terminal cause), stmt-kind factrefs serve invalidation only, and the
static/positional split is total (retirement-driven `scope-space`
cites the retirement; static mismatches and never-issued grants are
basis-free intrinsic). D-186 collapses the three coexisting
revocation completion rules — the withdrawn references-must-cover
clause and the D-50-era every-reference-accepted trigger had
survived beside the state-derived law, same bytes different authority
state — into **one law** (linkage valid ∧ authorship cutoffs total ∧
decryptable-wrap domain empty), with `c.revoke_zones` continuing
authorship coverage and references as typed linkage only. D-187 pays
the §10.5 edit debt (the contextual dual entry is in the map —
proven incompatibility reject-permanent within the branch,
awaiting-renewal pending; verified at both sites). D-188 sweeps the
frontier predicate ("terminal head" eliminated from §4.3/§4.6/the
cap list — eligibility is held chain membership; the contradictory
"never nothing" deleted; no-head-at-or-below = successful no-op).
D-189 completes E10 (`source-erased` in the closed body enum —
verified; journal invariant violations named `log-corrupt` →
storage-quarantine; every "resolved-negative" prose site now names
its outcome — "dispositions are not outcomes, and a decision row
claiming a mapping is not the mapping"). D-190 pins the P/−P
residual into §14 with its family-13 acceptance vector (negation
reuse deliberately undetected under exact-SEC1 equality, stated with
the scalar-holder rationale).

**Findings: none in scope.** The two residue candidates my sweep
surfaced — "consumes EVERY unconsumed" and the four
`(winner, freeze_basis)` hits — both classify as ledger/withdrawal
text (D-153 carries its D-176 pointer; line 1321 is the live
withdrawal notice itself), which is exactly the hygiene the last
synthesis demanded.

## Recommendation

Eighteen rounds. The v0.5.16 synthesis's sharpest observation — that
D-178 replaced one carrier-less fact with another — is answered here
by anchoring the boundary to the one artifact the protocol already
authenticates and serializes: the log commit. That is the
"prose storage is not storage" floor applied at full strength, and
the D-186 collapse (three completion rules, same bytes, different
authority — dead text shedding two rounds of history) is the
accretion-resistance the process has now demonstrated three cuts
running. If the synthesis's replay concurs, the artifact sequence
begins: companion schema (commit-boundary erasure schedules,
terminal-first reservation cases, the op-kind basis table, one-law
revocation completion, the §10.5 dual lifecycles, and the −P
acceptance vector in the opening tranche), then the independent core
and harness, corpus, family 14, surfaces, and the discrepancy audit
as the Gate-A decider. Durable P1 writes remain prohibited until
Gate B plus the umbrella's P0.5/tombed-cutover prerequisites,
unchanged.
