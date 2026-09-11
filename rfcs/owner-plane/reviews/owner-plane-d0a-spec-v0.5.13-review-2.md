# Review 2: D0-A Core + Memory specification v0.5.13

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.13 (4,248 lines, D-1..D-159), against the v0.5.12 synthesized
review's seven freeze-blocking clusters. Written without reading the
peer review. Scope note: wire verification of all eight new rulings
plus their E8/CDDL/domain landings; composed-trace depth again
limited — stated, not hidden — and the freeze judgment remains the
synthesis's.*

## Executive verdict

**All seven clusters are discharged on the wire.** D-152 corrects the
off-by-one the synthesis caught — the legal maximum lineage is 64
open gaps PLUS the live tip, and `frontierclose.heads` /
`zoneheads.heads` now cap at 65 (E8 row + both CDDL comments
verified). D-153 makes staged-frontier consumption automatic, total,
and **one-shot** at the next consuming operation (already-materialized
prior-advance entries can no longer satisfy a later advance). D-154
widens pending-selector reservation to the `(zone, lineage)` scope —
the `last_known` dependency cone crosses generations, so a later
generation opened on the unresolved branch now hangs on the same
selection. D-155 refines D-146 again in the right direction: import
replay-key ownership is **wholly derived** with a total portable
claimant order `(import-grant control position, gen, seq)` —
claimants compare across grant turnover and C3′ without any local
fact. D-156 makes Merkle leaves self-describing —
`H_brec({export_id, rec_index, rec})` with the verifier computing the
expected rank from the release's signed sorted `sources` — closing
the path-authentication gap. D-157 closes the terminal-cause
vocabulary (`basis` = the op hash of the **first branch-relative fact
in fold order**, spanning exactly D-149's branch-relative class) and
pins the interval machine. D-158 closes the last key-lifecycle bypass
the synthesis named: an adopted renewal's signing key burns globally
and its KEM key binds to the adopted device — adopted keys join the
portable freshness domain, so a later surviving enrollment can no
longer assign an adopted live KEM key to another device. D-159 splits
device-revocation coverage into the authorship domain (cutoffs —
authored-but-unwrapped zones included) versus the wrap domain
(`rotation_refs`), the same two-domain discipline D-141 established
for renewals.

**Findings: none in scope.** Every ruling has its bytes; the caps,
domains, orders, and vocabularies I chased all landed; the two-domain
and freshness-domain integrations are consistent with their D-141/
D-150 parents.

## Recommendation

Fourteen rounds. The residue clusters have shrunk from subsystems to
an off-by-one, and the last three cuts have each corrected their own
predecessors under synthesis pressure — the process is doing what a
freeze gate should. If the synthesis concurs that v0.5.13 closes its
seven clusters, the artifact sequence begins: the companion schema
first (adding the 65-head boundary cases, one-shot stage consumption,
cross-generation reservation, the derived claimant-order traces,
rank-checked Merkle paths, the closed basis vocabulary, and the
adopted-key freshness negatives), then the independent core and
harness, the corpus, family 14, all required surfaces, and the
prose↔CDDL↔companion↔vector discrepancy audit as the Gate-A decider.
Durable P1 writes remain prohibited until Gate B plus the umbrella's
P0.5/tombed-cutover prerequisites, unchanged.
