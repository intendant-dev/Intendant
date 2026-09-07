# Review 2: D0-A Core + Memory specification v0.5.2

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.2 (2,333 lines), against the v0.5.1 synthesized review's eight
consolidated blocker areas and the new rulings D-69..D-77, the frozen
umbrella RFC (v3.1/D0), and the v0.5.1 archive (381 changed lines
diffed). Written without reading `owner-plane-d0a-spec-v0.5.1-review.md`.
Method upgraded per the v0.5.1 adjudication: **verification by
construction, not by reading** — I walked the hosted exclusion
ceremony end-to-end for cycles, traced the D-69 anchor through every
proof-consuming site, recomputed the E8 byte arithmetic from the CDDL
(independently: `kekwrap` = 306 B exactly, matching the spec; joint
128 + 128 fit ≈ 55 KiB < 64 KiB — the claim now holds), and checked
each new ruling's bytes against Appendix A.*

## Executive verdict

**v0.5.2 is the strongest cut in the series. The three hard protocol
inventions this round are correct and constructible:** D-69's
epoch-anchored proof policy (the anchor is the operation's *signed*
`capability_epoch`, `c.zone_policy` becomes an epoch event, and
`policy(e)` is fold-derivable — arrival-order divergence is gone with
zero new receipt bytes); D-70's service succession (append-only
registry + explicit policy-leaf rebind — the same-epoch deadlock and
the rotation-purges-history defect both dissolve); and D-71's
rotation-first hosted exclusion (the v0.5.1 hash cycle is honestly
voided; the exclusion-shaped rotation's no-new-authority constraints
plus the freeze rule make the non-cyclic ceremony safe). D-72
(window-state freshness), D-73 (wrapper-current survivors), D-74
(three-branch trigger + partition exactness), D-75 (flow-bounded
transfer), D-76 (reducer exactness incl. the 15-verb genesis), and
D-77 (regenerated caps, durable Fence, checkpoint object, dense
issuer sequences, frame CDDL) each land where the synthesis aimed.
All six of my v0.5.1 findings are fixed; eight of my ten pins applied.

**Not freezable yet, by one ruling's missing bytes and two seams the
D-69 rewrite opened:** D-74's audit body never reached Appendix A
(blocking — the ratified shape is unencodable under strict decoding);
cross-zone import deadline qualification still cites the abolished
"admission position" anchor and has no source-zone anchor on the wire
(high); and §4.3's "budgets reset **only** on `c.cap_epoch_bump`" is
now false, since every policy change advances the epoch and thereby
resets every budget in the zone — an unratified authority side effect
(high). Plus four mediums and six pins. One more micro-patch
(v0.5.3): one CDDL edit, roughly six sentences, one small owner
ratification (the budget-reset semantics). Then the prose is done.

---

## Part 1 — Discharge audit of the v0.5.1 synthesized blockers

| # | Blocker | v0.5.2 disposition | Residue |
|---|---|---|---|
| 1 | Canonical proof ordering + finality | **D-69**: signed-epoch anchor; `policy(e)` defined (§9.4); two-replica order equivalence vectored; compromise cutoffs = the sole revisit, with the stated quarantine transition; **D-77**: versioned `checkpointobj` + `H_ckpt`, covers/fence identity ("the fence IS the covers frontier", positions = signed coordinates), tagged per-issuer proof cutoffs, dense `issuer_seq` (backfill denial), narrowed hardening (deadline/lease/causal; cert-superseded/policy-missing never harden) | **H1** (import anchor un-migrated); **M2** (`checkpointobj.retired` has no construction rule); pin 1 (E7 key table lags the tagged cutoffs) |
| 2 | Non-cyclic hosted revocation + exact self-service | **D-71**: rotation-first, cycle voided; exclusion-shaped rotation = empty manifest + no new recipient authority (recipients ⊆ enrolled unrevoked current-epoch holders, KEM keys = cert keys, minus ≥ 1 target); freeze rule; target = renewal-stable `revocation_id`; `c.revoke_zones` continues cutoffs too (new E8 cap); **D-72**: attestations bind `window_state` — banked requests go stale | **M1** (`window_state` formula ambiguous for the zone-less reauth body); pin 5 (freeze rule stated only in the hosted section) |
| 3 | Service-key succession | **D-70**: registry keyed by `key_id`, idempotent installs, any number per epoch; qualification = the policy leaf at the epoch anchor; rotation = install + rebind (which advances the epoch); cutoffs target any installed key, cover receipts **and** leases; hosted non-rotation stated; succession-trio + order-equivalence vectors | none |
| 4 | Closed bytes, durable Fence, generated limits | **D-77**: `H_cert`/`H_grant`/`signer_key_id` pins; Fence persists `{kek_epoch, rotation_op, fence_frontier}` (§5.5/0x13/CDDL all aligned); frame payload rules for 0x01/0x12/0x13/0x16; versioned keyed `survivorset`; E8 regenerated — **I recomputed `kekwrap` = 306 B and the joint 128-wrap + 128-entry fit ≈ 55 KiB < 64 KiB independently; both hold**; `c.wrap_add` overflow lane coherent (post-Fence wraps at the new epoch) | pin 4 (the manifest *sub*-figure is ≈ 16.5 KiB, not "≈ 14 KiB" — the joint bound is unaffected) |
| 5 | Multi-epoch survivor completeness | **D-73**: wrapper-current definition verbatim; third-rotation epoch-1-item vector required (family 13) | none |
| 6 | Transfer authority + bundle accounting | **D-75**: release deadline ≤ the matching flow's; whole-flow existential matching; op + bundle bytes charge; bundles never persisted/framed; three-coordinate import-header equality; egress proof coordinates (source zone, release's own `item_addr`); `bundle`/`bundlerec` now in A.5 | **H1** (the import-side anchor sentence) |
| 7 | Audit trigger, scope, partition | **D-74**: third branch (audit-space scope always audits — my F4's recommended repair); shared `read_id`/principal/scope; indexes `0..count−1`, disjoint union = released results; `result_ids` = op hashes; `scope.spaces` ≤ 64 (E8 row added) | **B1**: none of the D-74 body changes reached the `maudit`/`auditprin` CDDL |
| 8 | Frontier retirement, bootable genesis, outcomes | **D-76**: exact-named-head retirement with `last_known` validation (accepted, terminal, same zone/lineage, gen < g); unknown-gap heads persist to their cutoff; genesis grants complete (15-verb trusted set — my F1's exact fix — no deadline, `online_lease = false`, no flows, kinds absent, inert raise/declassify); enroll grants target the enrolled/renewed device; tenant `request-fork`; `cert-expired` removed; first-failing-stage precedence; `fail-closed` ⇒ `require_cert_deadlines = true` | **H2** (the budget-reset sentence D-69 falsified); pin 2 (D-33/D-67 refinement pointers) |

My v0.5.1 findings: F1 ✓ (D-76), F2 ✓ (D-73), F3 ✓ (D-70), F4 ✓
(D-74), F5 ✓ (A.5), F6 ✓ (D-77 Fence/frames). Pins: eight applied
(D-54 annotation, `Write-capable`, cutoff identity check, egress
coordinates, enroll targeting, trusted audit-read implication,
shared-device residual, fail-closed cross-field); the header
provenance pin was missed **again** (M4); the shape-4 pin was ruled
(D-74) but not applied (part of B1). B.1/B.2/B.3 untouched and
correct.

---

## Part 2 — Findings

### Blocking

**B1. D-74's audit body never reached Appendix A.** The registry row,
E8 (`audit scope.spaces ≤ 64`), and the D-74 decision row all require
`scope.spaces` as a bounded set and a mediated principal "split into
peer vs session variants" — but A.5 still reads
`maudit = { …, scope: { zone: ulid, space: ulid }, … }` (singular)
and `auditprin` shape 4 is still the single
`{ shape: 4, peer: text, ? token_hash }` variant. Under O3/E9 strict
decoding, the ratified partition shape is **unencodable**: family 11's
"multi-space scope" and "typed principal" vectors cannot be written,
and a multi-space search cannot emit a conforming audit row at all.
Same defect class as v0.5.1's F1 — a ruling without its bytes. Fix
(one edit): `scope: { zone: ulid, spaces: [+ ulid] }` (set, E7, ≤ 64)
and split shape 4 into peer and mediated-session variants (e.g.
`{ shape: 4, peer: text, ? token_hash }` /
`{ shape: 4, session: text, token_hash: bytes32 }`), mirroring the
§10.1 shape-4 components.

### High

**H1. Cross-zone import deadline qualification still uses the
abolished anchor — and no source-zone anchor exists on the wire.**
§11.8: qualification is "evaluated under the **source zone's** witness
policy **at the import's admission position** (D-57…)". D-69 replaced
admission-position anchoring precisely because it is a local event
that diverges under arrival order; every other proof site migrated.
Worse, the import operation's signed `capability_epoch` is the
**destination** zone's, so the D-69 anchor cannot be applied as-is:
the source zone's policy has no signed anchor in the import operation.
The flow is already declared source-governed, and the source-zone
operation with a signed epoch exists — the release. Fix (one
sentence): the import's deadline receipt qualifies under the source
zone's `policy(release.header.capability_epoch)`; add the
order-equivalence vector to family 11.

**H2. D-69 silently turned every policy change into a plane-wide
budget reset, and §4.3 still denies it.** §4.3: budgets "reset
**only** on an admin `c.cap_epoch_bump` (deliberate)". §9.4: "Budget
accounting resets per epoch" and — new in D-69 — `c.zone_policy`
acceptance advances the epoch. Both sentences cannot hold: after any
policy edit (say, adding a time witness), every grant's
`(grant_id, lineage, capability_epoch)` accounting starts fresh, so
the solo posture's "bounded by construction" budgets are re-armed by
an operation whose purpose is unrelated. Admin-only, so not an
escalation — but it is an **unratified authority side effect** of
D-69, and the two normative sentences now contradict. Owner choice
(one line): either ratify "every epoch event resets budgets" and amend
§4.3, or key budget accounting to bump events only (epoch value ≠
budget key), keeping D-69's anchor semantics untouched. Family 10
already vectors "epoch bump reset"; add the policy-change case for
whichever ruling lands.

### Medium

**M1. `window_state` is well-defined for `c.cutoff` but ambiguous for
`c.lineage_reauth`.** §9.3 counts accepted `w.gen` **per
(zone, lineage)**; the reauth body names no zone, and D-72 defines
`window_state` as "the count … for the lineage". Sum-across-zones and
per-zone readings both exist; a validator recomputes this value to
verify the signature, so the wrong formula either bricks honest
self-service (never matches) or fails to stale banked attestations
(D-72's purpose). Pin one formula (sum across the lineage's zones is
the natural lineage-wide reading; the cutoff attestation uses the
named zone's count — say both explicitly).

**M2. `checkpointobj` is hashed but not constructible.** `H_ckpt`
pins the object; `covers` is hash-checked against the frontier; but
`retired: [* head]` has no construction rule (heads retired since the
previous checkpoint? cumulative? whose view?) and nothing pins
`proof_positions == proof_cutoffs` (the ccheckpoint comment implies
byte-sharing but no equality is stated, unlike `covers`). Without
both rules a replica cannot reconstruct the object to verify
`H_ckpt`, and fixtures would invent the answer. Two sentences.

**M3. D-64's "no in-ceiling budget refresh exists" now has a
counterexample D-76 created.** Renewal compounds may carry grants
targeting "the enrolled **or renewed** certificate's device"
(§7.1 c.enroll, D-76), renewal is hosted-admissible, and
`audit.write` is hosted-grantable — so a hosted device's own renewal
can mint a fresh audit grant (fresh `grant_id` = fresh budget). Either
amend the D-64 sentence ("the remedy is the device's own renewal
compound; re-root remains the fallback") — the friendlier outcome —
or forbid new grants on renewal compounds. State the same answer for
ordinary-grant budget exhaustion (the renewal lane is the natural
remedy there too).

**M4. The header provenance is stale for the second consecutive cut**
— still "Folds the v0.4 synthesized review", archives
`v0.{1,2,3,4}` while the v0.5 and v0.5.1 archives exist. This pin was
adopted in the v0.5.1 synthesis and missed again; in a self-contained
freeze candidate, the provenance line now misstates which review the
document folds by three rounds. Elevated from pin on repeat.

### Pins

1. **E7's keyed-set table lags D-77**: it still declares
   `proof_cutoffs` → `key_id`, but the shape is now
   `{ issuer, through }` keyed by issuer (`receipt_cutoffs` correctly
   stays `key_id`).
2. **D-33 and D-67 lack refinement pointers** (→ D-76's exact-head
   retirement with multiple live heads; → D-73's wrapper-current
   membership) — D-48/D-54/D-57/D-58/D-64 all got theirs in the D-77
   sweep.
3. **§11.7's audit bullet restates only two trigger branches**;
   D-74 added the third. Point at §11.1 instead of restating.
4. **E8's manifest sub-figure**: 128 × ~132 B ≈ 16.5 KiB, not
   "≈ 14 KiB" (the joint ≈ 55 KiB bound is correct — verified
   independently); fix before family 1 pins exact bytes.
5. **The D-71 freeze rule lives only in §7.5(c)** — state whether
   grant/wrap-freezing of exclusion targets binds trusted planes'
   rotation-first exclusions too (the same pending-window exists).
6. **`grant_epoch_slack` now ages with policy changes** (every
   `c.zone_policy` advances the epoch) — one clause so owners size
   slack for policy-edit frequency, not only deliberate bumps.

---

## Part 3 — Gate-A readiness

Against the v0.5.1 synthesis's go-ahead criteria: proof ordering is
portable (D-69 — verified through every consuming site except H1's
one sentence); hosted revocation is constructible (I walked the
rotation-first ceremony; no cycle); service succession is a real
model (registry + leaf + epoch anchor); the stored-byte sweep is done
(frames, Fence, checkpoint object, reference domains — B1's audit
shapes excepted); the caps are honest (recomputed); survivors,
transfer, and the trigger are exact (B1's encoding excepted); the
reducer walks one path (precedence, chain arithmetic, retirement,
genesis completeness). The core, corpus, and harness still do not
exist — Gate A remains mechanically impossible today, as every round
has said.

**Recommendation.** Cut **v0.5.3 as the terminal micro-patch**: B1 is
one CDDL edit; H1/M1/M2/M3 are about six sentences total; H2 is the
sole owner ratification (budget-reset semantics — either answer is
one line); M4 and the pins are editorial. Nothing else should be
touched: this round's inventions (D-69/D-70/D-71) survived
construction-level checking, which is the first time in the series
the hard parts have. After v0.5.3, the specification work is over —
build `owner-plane-core`, generate the corpus (the two-replica
order-equivalence, succession-trio, third-rotation, joint-size, and
partition vectors are the ones that will bite), record the offline
family-14 result, and let the prose↔vector discrepancy audit decide
Gate A. Durable P1 writes remain prohibited until Gate B plus the
umbrella's P0.5/tombed-cutover prerequisites, unchanged.
