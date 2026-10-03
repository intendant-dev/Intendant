# Synthesized review: D0-A Core + Memory normative specification v0.5.10

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.10-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.10-review.md)
and
[owner-plane-d0a-spec-v0.5.10-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.10-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.10,
SHA-256 `8b3e069795978808e0c49e2cb341e784843e8bff9b4232328d1104834ce39ddc`.
This synthesis adjudicates the evidence rather than averaging the verdicts.*

## Executive verdict

**Cut v0.5.11. Do not treat B1 plus a freeze edit as sufficient.**

The peer review materially improves on its prior method and contributes one
decisive blocker: D-134 says import evidence and optional metadata are absent,
but the closed `mimport` CDDL still requires `evidence`. No body satisfies both
rules. Its `c.space_retire` wire-parity observation is also valid, though it is
only one part of the retirement defect.

The peer is also right to credit the revision:

- D-127 removes the transfer self-hash and makes releases constructible;
- D-128 replaces signature-only caps with a real structural predicate;
- D-129 fixes requester entry-versus-carried-Head mismatch;
- D-130/D-131 substantially improve exact-Head and issuer-fork handling;
- D-132 chooses reauthorization revival;
- D-133 fixes the central exclusion/KEM-renewal trace; and
- D-134 publishes record-level effect keys and names permanent-negative
  transfer cleanup.

But the peer's conclusion that every earlier critical/high is discharged does
not survive execution of the new rules:

- `max(Top, H)` is still `Top`, so the first requesterless cutoff cannot lower
  `admit_bound`;
- a pure empty snapshot allegedly retires everything while carrying no
  generation that the equation can update;
- per-generation promotion does not end the authority to open future
  old-epoch generations;
- recovery's control `base` cannot order tenant bytes, and a writer can mint
  against the old epoch after `space_retire`;
- a transfer terminal based on a cap/seal can outlive the recovery event that
  removes its basis;
- lowest-hash selection changes the claim `op_hash` that judgments, pins, and
  erase requests target; and
- certificate-authorship history is still incorrectly keyed to current KEM
  membership.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-127:** materially complete.
- **D-128–D-134:** partial; retain the chosen direction and finish the
  lifecycle contracts below.
- **Protocol/schema freeze:** no.
- **V0.5.11:** required, not merely an unversioned freeze edit.
- **Gate A:** false independently because the companion/corpus/surface work
  remains absent.
- **Durable P1 writes:** remain prohibited under the existing prerequisites.

## Assessment of the peer review

### What it did well

The peer explicitly raises its review floor to include constructibility,
authority, and lifecycle in addition to convergence. That methodological
correction pays off in B1:

- D-134 requires a fully derived import with evidence absent
  ([import prose](/Users/vm/owner-plane-d0a-spec.md:2300));
- `mimport.provenance` nevertheless requires `evidence: [* evref]`
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3518)); and
- E4 distinguishes absence from an empty array.

The peer's proposed repair is right: give `mimport` a purpose-specific narrow
shape, structurally remove the prohibited temporal/label/evidence fields, and
make provenance exactly the import tuple.

Its M1 also catches real drift: the registry describes `c.space_retire` as an
epoch-advancing strict-closure operation, while its body is only
`{ space_id }`. Complete pre-staging can technically satisfy the existing
union-coverage rule, so the first review's word “unencodable” was too broad in
that narrow circumstance. The peer is right that the pre-staging-only lane
would at least need to be explicit if the body remains narrow.

### Why its near-freeze verdict fails

The raised method is stated, but not applied to the adjacent transitions:

1. **Algebra:** naming `Top` and `Absent` is not enough; their operators must
   be executed. `Top` is a min-fold identity, not a max-fold identity.
2. **Cross-domain ordering:** a signed capability epoch is not proof that a
   tenant operation was authored before a control event.
3. **Lifecycle finality:** convergence over a fixed claimant set does not make
   the winner immutable when a lower claimant may arrive later.
4. **Semantic identity:** derived content does not make two operations
   interchangeable when the protocol defines the claim ID as `op_hash`.
5. **Removal:** a terminal cannot rely on a “permanent” boundary that C3′ is
   explicitly allowed to remove.

The peer's import-collision telemetry pin also proposes the wrong lifecycle.
`import-collision` is `reject-permanent`; the standing budget-displacement lane
is `quarantine-reproposal`. The deeper issue is not where to surface the loser
but whether a late winner may replace an effect-final, referenced claim at all.

The suggested deletion of the “no duplicated top-level copies” comment is not
necessary. That constraint remains useful after narrowing, as long as the
exact nested mirror and its equalities are correctly specified.

Adopt B1 and the retirement wire observation. Reject the peer's empty
severity ledger beyond B1/M1 and its instruction to move directly to
artifacts.

## Adjudicated decision ledger

| Decision | Peer disposition | Synthesized disposition |
|---|---|---|
| D-127 | Complete | **Materially complete** |
| D-128 | Complete | **Partial:** non-budget proof authority, dissolution, recovery attachment, and retention remain |
| D-129 | Complete | **Open:** first-event max fold and snapshot omission are undefined |
| D-130 | Complete | **Partial:** promotion lacks scalar closure; tenant selectors lack pending reservation |
| D-131 | Complete | **Mostly complete for issuer commitments; C3′ and key-history scope remain** |
| D-132 | Complete | **Partial:** reauth fixed; recovery omission and retirement remain non-portable |
| D-133 | Complete | **Partial:** membership/custody fixed; authorship and overlapping renewals remain |
| D-134 | One CDDL blocker | **Protocol-partial:** CDDL impossible, logical claim identity unstable, terminal basis removable |
| D-91 / Gate A | Artifact-pending | **Artifact-pending; agreed** |

## Blocker 1: no `mimport` body satisfies prose and CDDL

This is the peer's strongest contribution and should be adopted unchanged in
substance.

D-134 requires:

- `sensitivity == class_floor`;
- temporal fields absent;
- provenance exactly the import tuple;
- labels absent; and
- evidence absent.

The CDDL still uses the broad claim provenance shape with mandatory
`evidence: [* evref]` and optional session/project/model, temporal, and label
fields
([`mimport`](/Users/vm/owner-plane-d0a-spec.md:3518)). Omitting `evidence`
fails the closed schema; including even `[]` violates the exact-absence rule.

Create a dedicated shape such as:

```text
mimport = {
  source_op, class_floor, kind, statement,
  sensitivity,                         ; == class_floor
  provenance: {
    import: { from_plane, export_id, release_op, digest }
  }
}
```

Then keep every prohibited field structurally absent, not merely rejected by
a prose comment. Add positive canonical bytes and one negative for each
forbidden field.

## Blocker 2: D-129's equation still fails its first event

The reducer orders
`Absent < "none" < Head < Top`, initializes `admit_bound = Top`, and says
ratify acceptance max-composes
([equation](/Users/vm/owner-plane-d0a-spec.md:1380)). Thus the first
requesterless ratification at H3 computes `max(Top, H3) = Top` and changes no
admission state. `ratified_through` records H3, but effective admission again
ignores the recorded boundary.

Use a tagged state:

```text
Unbounded --first Head--> Bounded(H)
Bounded(B) --later Head--> Bounded(max(B,H))
any --snapshot H--> Bounded(H)
```

Handle requesterless `"none"` as an explicit special case if it is intended
to affect coverage but not current admission.

The pure-snapshot zero-history form is also incomplete. Empty `cutoffs` plus
`heads: []` allegedly retires everything, while the equation overrides only
generations the snapshot carries. It carries none. Do not derive the omitted
generation set from replica-local knowledge. Carry explicit per-generation
`"none"` bounds, a `retired_gens` set, or a signed generation high-water and
define its fold.

## Blocker 3: per-generation promotion does not close old authority

Coverage for a lineage is satisfied when any generation has
`ratified_through != Absent`, while D-130 promotes only generations that have
ratify state
([promotion](/Users/vm/owner-plane-d0a-spec.md:1474)). A strict epoch advance
can therefore accept g1 coverage while g3 stays open.

Even complete coverage of all currently live generations is insufficient: a
writer may later open g4 at the old epoch, because staleness against the
current epoch is deliberately not an admission predicate
([epoch closure](/Users/vm/owner-plane-d0a-spec.md:1826)). Per-generation caps
freeze known prefixes; they do not terminate future authorship.

Require both:

1. the per-generation promoted map; and
2. an explicit scalar authority-ending `zonecutoff` for every consuming
   lineage.

Do not let “some generation is ratified” stand in for a lineage-wide close.
If large zones need staging, add a standalone scalar-close operation or an
equivalent paged commitment.

## Blocker 4: tenant Head selection lacks pending reservation

D-130 fixes one selector under both Head-arrival orders, but later control
operations pass an earlier pending reference
([Head lifecycle](/Users/vm/owner-plane-d0a-spec.md:1458)). Therefore earlier
B1 naming absent `H_A` may pend while later B2 names held `H_B` and selects B.
After `H_A` arrives, a fresh fold selects A at B1, while the incremental fold
already committed B.

D-131 already contains the solution for issuer commitments: an earlier
pending selector reserves its scope. Apply it to tenant Head coordinates and
their affected suffixes, and specify reservation release on permanent
rejection or C3′ removal.

## Blocker 5: D-132 recovery and retirement have no author-time coordinate

### Recovery omission

Recovery `base` is a control-chain coordinate. Ordinary tenant operations
carry cert/grant references, tenant chain coordinates, and a capability epoch;
they carry no authoritative control frontier
([AuthorizationProof](/Users/vm/owner-plane-d0a-spec.md:490),
[SignedOperation](/Users/vm/owner-plane-d0a-spec.md:505)). D-132's omitted-
lineage rule nevertheless quarantines tenant operations “past base”
([C3′](/Users/vm/owner-plane-d0a-spec.md:1540)).

A delayed pre-fork write and a post-fork mint under the same surviving grant
and epoch cannot be distinguished on a fresh fold. The equation also has no
transition for the purported revivable blanket. Replace it with a portable
per-`(zone,lineage)` default: an explicit universal/zero-history bound, a
tenant Frontier commitment, or checkpoint/paged tenant cutoffs.

### C3′ removal

The symmetric-removal list omits control-derived widening and authorization
state such as a cut `c.lineage_reauth`. A cut-branch reauth can revive a held
`w.gen`; removing that reauth must re-quarantine it. The robust rule is a full
reconstruction of all surviving control-derived admission state followed by
tenant re-evaluation, not a short exhaustive list.

### Space retirement

The peer correctly notes the missing inline `cutoffs` field. Full pre-staging
can make a strict retirement pass the union rule, so the body is not
unconstructible in every case. But the deeper contradictions remain:

- §9.4 does not list `c.space_retire` as an epoch-opening operation or define
  the carried-forward policy
  ([§9.4](/Users/vm/owner-plane-d0a-spec.md:1792));
- the CDDL body carries only `space_id`
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3239)); and
- a post-retirement writer can deliberately sign the old epoch. In lenient
  zones, old-epoch writing remains valid by design.

An epoch number proves policy selection, not authoring time. End space writing
with an immutable space-qualified per-lineage cutoff, or require complete
scalar closure for every writer. Then align §9.4, CDDL, inline versus staged
coverage, and vectors.

## Blocker 6: D-134 import identity is mutable after effect finality

Lowest-`op_hash` selection converges for a fixed set, but no boundary proves
that a lower cross-lineage claimant will never arrive
([import rule](/Users/vm/owner-plane-d0a-spec.md:2291)). The peer treats the
choice as invisible because imported content is derived. It is not:

- claim identity is the emitting `op_hash`;
- judgments, pins, evidence, and erase requests target op hashes; and
- actor/writer lineage participates in authorship semantics.

Trace:

1. `I_hi` accepts, becomes effect-final, and the source writes `XferDone`;
2. a judgment, pin, or erase request targets `I_hi`;
3. later authorized `I_lo` has the same replay key and a lower hash; and
4. D-134 rejects `I_hi` and substitutes the distinct claim `I_lo`.

Downstream references remain attached to the loser; erased content can
reappear under the new identity. Hash grinding makes the transition
adversarial. It also adds a revisit class absent from T2's supposedly
exhaustive proofs/budgets/boundaries inventory
([T2](/Users/vm/owner-plane-d0a-spec.md:690)).

The candidate set is not defined either: an unauthorized, malformed,
pending-proof, budget-quarantined, or later-disqualified low hash may reserve
the key, or winner removal must revive a loser currently labeled
`reject-permanent`.

Use a designated destination import writer or derive a stable logical claim
ID from `(from_plane, release_op, source_op)` and treat signed operations as
carriers/aliases. Collision telemetry is secondary to making identity final.

## Blocker 7: transfer “permanence” is only branch-relative

D-134 terminalizes releases/imports beyond `immutable_cap` or in a voided
generation
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:1095)). D-128 and D-131
then allow the cause to disappear: a cap may dissolve under a qualifying
cutoff, and C3′ may remove cut-branch seals, closures, or caps.

An `XferAbort` written while R is beyond the cap permanently closes the
journal. If recovery removes the cap's basis, the reducer re-admits R but the
terminal still blocks imports. A destination import listed in `missing` has
the same problem.

The predicate must be **terminal-stable under every allowed future
transition**, not merely negative under the current branch. Options are:

- keep branch-relative negatives dormant;
- bind terminals to a recovery/control basis and invalidate/reopen them when
  that basis is removed; or
- define a durable `XferReopen`/recovery-adoption transition.

The permanence map also omits immutable scalar supersede/revoke/close/recover
cutoffs. §10.5 places generic `cutoff` in the revivable row, despite those
purposes being non-widenable within their branch
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1971)). Split permanence by
boundary purpose and keep the intentionally revivable recovery-omission
blanket distinct.

## High: D-128 cap authority and lifecycle remain incomplete

The structural eligibility checks are a real improvement. Three exactness
issues remain:

1. D-128 ignores deadline/lease proof yet confers truncation and effect-final
   closure. That lets an expired or off-lease writer close history despite
   normal `w.gen` denial. Either explicitly ratify this as a separate
   capability/residual or require every non-budget IAM predicate.
2. Cap eligibility can change under ordinary revoke/grant cutoffs, renewal,
   tenant-fork selection, or removal of reauth. Define cap existence as a
   derived function of the complete predicate; do not name only compromise
   and C3′ dissolution.
3. The claimed 64-open-gap retention bound is false for known-`last_known`
   cap evidence. Reauthorization permits unbounded generations. Define
   checkpoint materialization/retention or admit historical growth
   ([cap rule](/Users/vm/owner-plane-d0a-spec.md:1320)).

## High: renewal identity still conflates unrelated domains

D-133's current-membership and actual-old-wrap predicates correctly solve the
main exclusion trace. Two additional separations remain.

First, certificate `history_cutoffs` preserve predecessor authorship and old
history, but their required coverage follows current KEM membership
([certificate renewal](/Users/vm/owner-plane-d0a-spec.md:339),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:3168)). A device may have old
authored history in a zone from which a later epoch excluded its wrap. Renewal
then requires neither a wrap nor a history boundary, leaving old history or
old-key minting undefined. History coverage must follow predecessor
authorship/history, independently of KEM membership.

Second, overlapping K0→K1→K2 renewals need transitive local custody: a current
descendant-key wrap must satisfy every ancestor-key obligation, and each old
secret remains until all tuples it opens are descendant-covered or retired.

Finally, D-131's “plane-wide forever” key uniqueness is applied only to
renewals. A new device can reuse another device's signing key; because witness
self-exclusion compares `device_id`, one private key can sign as D1 and
receipt as D2
([witness qualification](/Users/vm/owner-plane-d0a-spec.md:652)). Apply
uniqueness to every non-genesis certificate and preserve historical key use
across C3′/GC, or scope feeds by certificate incarnation.

## Schema, mirror, and vector delta

In addition to B1 and the protocol traces above:

- Require
  `provenance.import.from_plane == release.header.plane_id` and
  `provenance.import.digest == release.body.content_digest`.
- Require
  `PendingXfer.content_digest == release.body.content_digest` and
  `PendingXfer.dest_zone == release.body.to.zone_id`.
- Call `release_op` in `mimport` a signed mirror of an independently derived
  hash, not “never signed input.”
- State the actual count equality:
  `PendingXfer.record_count == |release.sources| == |bundle.recs|`.
- Namespace release-delivery and source-terminal effect keys if they share a
  dedupe store.
- Add first-ratify and explicit empty-generation snapshot vectors.
- Add full strict-close plus future-old-epoch `w.gen` negatives.
- Add earlier-pending/later-conflicting tenant selectors.
- Add recovery omission, cut-reauth, cut-retire, and old-epoch retirement
  traces.
- Add import lower-winner after judgment/pin/erase/terminal, including invalid
  and later-disqualified contenders.
- Add terminal cleanup followed by cap/seal/checkpoint removal.
- Add old-history-after-exclusion, K0→K1→K2, same-key D1/D2, and cut-branch
  historical-key reuse.
- Complete all D-131 carrier pairings/control orders and reservation-release
  cases.

The peer's suggested import-collision telemetry can be added only after the
winner/loser lifecycle is fixed. D-117/D-118's unFenced and multi-generation
vectors are now present and should be preserved.

## Required owner rulings

V0.5.11 still needs six choices:

1. **Boundary fold:** first ratify transition and explicit snapshot absence.
2. **Authority close:** scalar ceiling plus per-generation promotion, and
   tenant-selector reservation.
3. **Recovery/retirement:** portable tenant cutoff defaults, complete C3′
   re-folding, and non-backdateable space closure.
4. **Cap/terminal lifecycle:** non-budget proof authority, all dissolution
   events, terminal basis, and reopen behavior.
5. **Import identity:** designated writer versus derived logical claim ID.
6. **Renewal identity:** authorship coverage, transitive KEM custody, and
   global historical signing-key uniqueness.

## Recommended v0.5.11 sequence

1. Narrow `mimport` CDDL so at least one valid import exists; add the full
   mirror matrix.
2. Correct the D-129 tagged fold and make snapshot absence explicit.
3. Require scalar authority ceilings alongside per-generation promotion.
4. Add pending tenant-selector reservation.
5. Replace recovery “past base” and epoch-only retirement with explicit tenant
   coordinates; make C3′ a complete surviving-state re-fold.
6. Choose cap-proof and basis-scoped terminal/reopen semantics; repair the
   purpose-specific permanence lattice.
7. Replace mutable lowest-hash claim identity.
8. Split authorship from KEM membership, make KEM custody transitive, and
   close signing-key history.
9. Align prose, CDDL, decision rows, and required vectors.
10. Only then author the companion and corpus.

## Bottom line

The peer correctly catches the last transfer-body shape mismatch and improves
the retirement wire audit. Those findings add to the first review; they do not
discharge its reducer, recovery, identity, and authority counterexamples.

V0.5.10 remains a strong repair draft, not a candidate baseline. Cut v0.5.11.
The remaining work finishes already chosen architecture rather than reopening
the Owner Plane or Memory design.
