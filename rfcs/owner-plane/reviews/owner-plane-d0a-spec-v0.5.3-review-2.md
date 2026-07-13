# Review 2: D0-A Core + Memory specification v0.5.3

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.3 (2,482 lines), against the v0.5.2 synthesized review's seven
consolidated blockers and six required owner decisions, the new
rulings D-78..D-85, and the v0.5.2 archive (393 changed lines
diffed). Written without reading `owner-plane-d0a-spec-v0.5.2-review.md`.
Method: construction-level verification continued — I re-ran the
synthesis's two-replica counterexample against the D-78 rules, walked
the D-82 banked-attestation replay, checked every ruling's bytes
against Appendix A (E8/E7 rows, tag inventory, frame mirrors,
superseded-term sweep), and probed the new explicit-closure model for
fresh seams.*

## Executive verdict

**All seven v0.5.2-synthesis blockers and all six required owner
decisions are discharged, and the two deep protocol repairs are
genuinely correct.** D-78's explicit-closure currency model is the
round's centerpiece: every admission comparison is now
signed-vs-signed or control-chain content (`epoch-unopened` pends
until the chain opens the epoch; chain monotonicity; signed grant
slack; `lenient_epochs` removed with the fold-current comparison that
made it order-divergent; closure via explicit cutoffs on
epoch-advancing ops, mandatory-covering under `strict`). I re-ran the
synthesis's counterexample — op O signed at epoch 1, policy P2
advancing to epoch 2, folded in opposite orders on two replicas —
and it dissolves: epoch openings are monotone chain content and no
staleness rule reads current state, so both orders admit O. As a
bonus, grant slack becoming signed-vs-signed retro-moots slack aging
under policy churn. The other five land cleanly: D-79 separates
budget windows from policy epochs (my H2, resolved the conservative
way); D-80 embeds the checkpoint (`ccheckpoint = checkpointobj`,
`prev_checkpoint` chain, since-predecessor `retired` ≤ 256,
currently-qualified `proof_positions` ≤ 64, the `ckpt` tag removed
from the inventory, multi-head `c.cutoff`); D-81 fixes activation
(Fence activates; acceptance authorizes; rotations serialize N+1's
Fence after N's `KekDestroyed`; last-holder exclusion made
inexpressible); D-82 replaces the resetting `window_state` with the
monotonic `lineage_version` (banked attestations can never
re-validate, and the zone ambiguity dissolves for free); D-83/D-84
land the audit wire and the renewals-mint-no-authority rule. D-85's
sixteen mechanical items all verified in place — dense-prefix
cutoffs with `issuer-gap`, derived grant revocation, the source-zone
import anchor (my H1), signed `bundle_size`, frozen-stamp recovery
with abort-collects-all, encoder-exact E8 (314 B/132 B, joint ≈ 58
KiB), the `case_kind` vector contract, and both frame mirrors.

**One blocking defect — and it is the same genre as the last three
rounds, in the very operation D-84 edited:** the `c.enroll` registry
row now mandates that renewals carry no `grants[]`, but the `cenroll`
CDDL still declares `grants` as a required field. Every renewal is
unencodable: omitting the key violates strict decoding, including it
violates the row. One token fixes it. Beyond that: the header
provenance is stale for the **third consecutive cut** despite being
adopted in two syntheses, and two pin-level seams from the new
rulings (the accepted-vs-active meaning of "current epoch" for
`c.wrap_add` under D-81; the deterministic-but-unnamed forever-pending
lane D-80's currently-qualified `proof_positions` creates). No new
owner decision is required — B1's fix is the mechanical consequence
of D-84 as ratified.

---

## Part 1 — Discharge audit

| # | v0.5.2 blocker / owner decision | v0.5.3 disposition | Residue |
|---|---|---|---|
| 1 + OD1/OD2 | Canonical ordering; budget axis | **D-78** (verified in §9.4, outcomes/dispositions, `zonepolicy`/`czonepolicy`/`cepochbump`/`crevokegrant` CDDL, families 7/9/10 incl. two-replica equivalence "now provable"); **D-79** (§4.3 + §9.4 aligned: budget windows = spans between bumps; policy advance re-arms nothing) | none — counterexample re-run and dissolved |
| 2 | Cutoff finality; requester freshness | **D-85** dense-prefix (`through` = observed contiguous head, 0 = empty; `issuer-gap` → freeze-writer; min-merge; device cutoffs cover leases); **D-82** `lineage_version` in both attestations (replay walked: first reauth consumes version 0; a banked twin at 0 can never validate at 1) | none |
| 3 + OD3 | Checkpoint carriage/retirement/bounds | **D-80** embedded object (body IS `checkpointobj`; op hash = identity; control log = carriage; `H_ckpt` and the `ckpt` tag removed); `prev_checkpoint` from `"genesis"`; validated since-predecessor deltas; E8 rows (≤ 256 / ≤ 64) + E7 keys added; multi-head `ccutoff` (set ≤ 64, attested per lineage) | **P3** (dropped-witness pending posture unnamed) |
| 4 + OD4 | KEK activation/serialization | **D-81**: I3 rewritten (served = last-Fenced epoch); state 1 "authorizes only"; serialization sentence in §5.x (N+1's Fence only after N's `KekDestroyed`; queue = local storage invariant, control admission stays portable via epoch consecutiveness); >128 staged ceremony; ≥ 1 recipient (last-holder inexpressible); 0x14/0x16 mirrors byte-aligned | **P2** (`c.wrap_add` "current epoch" wording) |
| 5 + OD5 | Audit wire | **D-83**: `maudit.scope = { zone, spaces (set ≤ 64) }`, one read = one zone (cross-zone searches partition per zone); `auditprin` shape-4 split into tagged `peer`/`session` variants; zero-result chunk `{0, 1}` with empty `result_ids`; one-Txn/4096 proof preserved | none |
| 6 + OD6 | Renewal budget remedy | **D-84**: renewals mint no authority — two disjoint enroll shapes; re-wraps of already-held zones only; new grants ride `c.grant` (hosted planes lack it), so "hosted remedy = re-root" is now true; D-64 row annotated | **B1** — the CDDL didn't follow the row |
| 7 | Closed-byte/reducer/conformance sweep | **D-85** all verified: derived revocation (`revoke_grants` removed; one live compound per `revocation_id`; D-71 freeze plane-wide); source-zone import anchor `policy(release.header.capability_epoch)`; signed `bundle_size` (§4.3 surcharge + §11.8 + CDDL); frozen-stamp recovery, abort-collects-all; genesis `class_ceiling = sensitive`; `H_genesis`/`renews = H_cert` domains; explicit `sig` stage in §10.2 + signer alg/key-id equality; contextual §10.5 header; E7 corrections; encoder-exact E8; `case_kind` + unique draw names | **M1** (provenance, third repeat) |

Superseded-term sweep: `window_state`, `H_ckpt`, `lenient_epochs`,
`revoke_grants` survive only as historical notes in decision rows and
supersession comments — no live uses. B.1 correctly drops
`lenient_epochs`; B.2/B.3 untouched. §15 refinement pointers on
D-33/D-64/D-67/D-68/D-69/D-72/D-74/D-75/D-77 are all present and
accurate.

---

## Part 2 — Findings

### Blocking

**B1. Renewals are unencodable: the `cenroll` CDDL contradicts
D-84.** The §7.1 row (correctly) defines two disjoint shapes and says
renewal `grants[]` and `lineage` **MUST be absent**; the CDDL still
reads `grants: [* grant]` — a required key (`lineage` got its `?` back
in v0.5.2; `grants` never did). Under E9/O3 strict decoding a renewal
omitting `grants` fails parse; one including it (even empty) fails the
row invariant. Every certificate renewal — and with it T3's
counter-recovery path and the D-84 vector family — is dead on arrival.
Fix: `? grants: [+ grant]` with the comment "REQUIRED for a new
device; ABSENT on renewal (D-84)" mirroring `lineage`; while there,
delete the row's vestigial tail "…targets the enrolled**/renewed**
certificate's `device_id` (D-76)" for grants (renewals have none —
keep it for wraps). Fourth consecutive round with a
ruling-minus-its-bytes blocker; the corpus harness will make this
class extinct, which is the strongest argument for building it now.

### Medium

**M1. The header provenance is stale for the third consecutive
cut.** Still "Folds the v0.4 synthesized review …"; the
archived-drafts glob still names `v0.{1,2,3,4}` while v0.5, v0.5.1,
and v0.5.2 archives exist. This pin was adopted in the v0.5.1 **and**
v0.5.2 syntheses and missed both times. The document claims
self-containedness and Gate A requires "§15 complete with correct
identifiers" — a false statement about which review the text folds is
a provenance error in the freeze candidate itself. Treat it as a
freeze-gate item, not a courtesy: "Folds the v0.5.2 synthesized
review", glob `v0.{1,2,3,4,5,5.1,5.2}`, and (per the earlier adopted
pin) name the actual ruling dates.

### Pins

1. **`crevokegrant.cutoff` equality**: pin `cutoff.zone_id ==
   grant.zone ∧ cutoff.lineage == grant.lineage` — as written, a
   mismatched cutoff (admin-signed, so no escalation) would close a
   different lineage's chain as a side effect of revoking an
   unrelated grant.
2. **"Current epoch" is now two things (D-81)** — accepted (control
   state) vs active (last-Fenced, local). `c.wrap_add` "adds
   current-epoch KekWrap" and D-84's "current-epoch wrap" re-wrap
   rule must mean **accepted** (portable control content; a wrap_add
   between acceptance and Fence must target the accepted epoch for
   the >128 ceremony to work). One clarifying clause at the
   `c.wrap_add` row.
3. **Name the dropped-witness pending posture (D-80)**: a pending op
   anchored at an old epoch whose policy lists witness W, where a
   later policy dropped W without a compromise cutoff, can never
   fence-harden — `proof_positions` carries currently-qualified
   issuers only, so the hardening condition is undecidable-false. It
   is deterministic on every replica (same checkpoint, same absence)
   and conservative, but it is a forever-pending lane an implementer
   will trip over: state it (pending until a qualifying receipt, a
   re-listing, or a feed cutoff) and add the family-9 vector.
4. **`c.lineage_reauth` + `c.cutoff` attestation rows**: family 7
   names "attested multi-head self-cutoff" — add the negative where a
   hosted requester's `cutoffs` set names a second lineage (must
   reject; the CDDL comment states the rule, the family list doesn't
   name the negative).

---

## Part 3 — Gate-A readiness

Against the v0.5.2 synthesis's own bar: canonical ordering is now
signed/chain-content everywhere (re-verified by counterexample);
freshness is monotonic; the checkpoint is a replayable embedded
object; activation is single-pointed and serialized; audit and
renewal authority are on the wire (modulo B1's one token); transfer
charging replays from signed bytes; and the vector contract is typed.
The corpus, harness, and `owner-plane-core` still do not exist, so
Gate A remains mechanically impossible today — unchanged, and the
synthesis was right that only that work can establish terminality.

**Recommendation.** Cut **v0.5.4** with B1 (one CDDL token + one
vestigial clause), M1 (two header lines), and the four pins (four
sentences, one vector name) — under an hour of editing, no owner
decisions. Then declare the prose frozen *as input to the audit* and
build: `owner-plane-core`, the corpus (lead with the vectors this
round created — two-replica equivalence under D-78, `issuer-gap`
dense-prefix denial, banked `lineage_version` replay, checkpoint
chain folds, rotation serialization crash matrix, the D-84 renewal
negative), the offline family-14 fixture, and the prose↔vector
discrepancy audit as the deciding gate. Each of the last four rounds
has produced exactly one ruling-without-bytes blocker; the discrepancy
audit is the machine that catches that class, and every additional
prose round now costs more than it retires. Durable P1 writes stay
prohibited until Gate B plus the umbrella's P0.5/tombed-cutover
prerequisites, unchanged.
