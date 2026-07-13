# Review: D0-A Core + Memory normative specification v0.5.10

*2026-07-12. Independent freeze review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md), v0.5.10,
SHA-256 `8b3e069795978808e0c49e2cb341e784843e8bff9b4232328d1104834ce39ddc`.
The review replayed the v0.5.9 synthesis against the normative prose, CDDL,
decision ledger, and required vector matrix. The archived v0.5.9 source was
used for a direct revision audit; v0.5.10 is 404 insertions and 136 deletions
from that baseline.*

## Executive verdict

**V0.5.10 is a strong repair round, but it is not freeze-ready. Cut a focused
v0.5.11 before declaring the candidate audit baseline.**

The revision genuinely closes several serious v0.5.9 defects:

- D-127 removes `release_op` from the bundle digest preimage and gives the
  transfer a constructible, acyclic hash graph;
- D-128 replaces signature-only truncation with a substantive
  `cap_eligible` predicate and pins effect-finality to eligible caps;
- D-129 requires requester entry/snapshot equality, names reducer identities,
  and gives `ratified_through` consumers;
- D-130 makes one exact-Head selector independent of tenant-byte arrival order
  and materializes consumed ratification per generation;
- D-131 correctly adds missing-ancestry pendency, pending issuer-scope
  reservation, a last-common-ancestor fork definition, renewal-key non-reuse,
  and symmetric C3′ removal;
- D-132 explicitly chooses lineage-reauth revival;
- D-133 fixes the E1-includes/E2-excludes/KEM-renewal trace and scopes Kold
  custody to tuples that actually hold old-key wraps; and
- D-134 publishes record-level effect keys and recognizes permanently
  quarantined transfer attempts.

Those are material advances. The remaining problems arise where the repairs
compose:

1. `admit_bound` initializes to `Top` and then max-composes ratification, so a
   first requesterless cutoff can never lower the bound;
2. per-generation promotion does not close future old-epoch generation
   openings, so a strict epoch advance can remain writable under the old
   authority;
3. recovery's omitted-lineage blanket compares tenant writes to a control
   `base` they do not carry, while `c.space_retire` is both unencodable as
   described and bypassable by signing the old epoch;
4. the new “permanently non-revivable” transfer terminal can later be
   invalidated by D-128 cap dissolution or D-131 recovery removal;
5. lowest-`op_hash` import selection can change the claim identity after
   judgments, pins, erase requests, or `XferDone`; and
6. renewal still uses KEM membership as certificate-authorship coverage and
   leaves overlapping KEM rotations and signing-key history incomplete.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-127:** materially complete.
- **D-128–D-134:** preserve their chosen direction, but finish the state
  transitions below.
- **Protocol/schema freeze:** no.
- **V0.5.11:** required.
- **Gate A:** no; independently artifact-pending.
- **Durable P1 writes:** remain prohibited under the existing later gates.

## Decision discharge ledger

| Decision | Assessment | Required disposition |
|---|---|---|
| D-127 | **Materially complete** | Preserve the acyclic graph and real construction vector |
| D-128 | **Partial** | Close proof/lease authority, cap dissolution, recovery attachment, and retention semantics |
| D-129 | **Open** | Correct the first-event fold and give empty/omitted snapshot generations explicit state |
| D-130 | **Partial** | Add a scalar authority ceiling and pending tenant-selector reservation |
| D-131 | **Mostly complete for issuer commitments** | Generalize C3′ re-folding and make signing-key history truly plane-wide/forever |
| D-132 | **Partial** | Reauth is fixed; recovery omission and space retirement remain non-portable |
| D-133 | **Partial** | Membership/custody split is good; authorship coverage and overlapping renewals remain |
| D-134 | **Partial** | Effect keys and negative-state naming improve; claim identity and terminal stability remain open |
| D-91 / Gate A | **Artifact-pending** | Companion, corpus, surfaces, family 14, and discrepancy audit remain |

## 1. D-129 still does not define an executable admission fold

### 1.1 `Top` defeats the first ratification

The ordered domain is
`Absent < "none" < Head < Top`; `admit_bound` initializes to `Top`; and a
ratify acceptance max-composes its entry
([equation](/Users/vm/owner-plane-d0a-spec.md:1380)). Therefore the first
requesterless entry at `H3` computes:

```text
admit_bound = max(Top, H3) = Top
```

It has no admission effect. Meanwhile `ratified_through` becomes H3, so the
draft again has one component recording the cutoff while effective admission
ignores it. This contradicts the cutoff algebra's statement that ratify
boundaries quarantine beyond their bound
([cutoff algebra](/Users/vm/owner-plane-d0a-spec.md:1270)).

`Top` is the correct *effective value* for “no bound exists,” but it cannot
also participate in the later-growth max fold. Use a tagged state, for
example:

```text
admit_bound = Unbounded | Bounded("none" | Head)

ratify(H):
  Unbounded   -> Bounded(H)       # first retirement boundary
  Bounded(B)  -> Bounded(max(B,H))

snapshot(H):  _ -> Bounded(H)
```

If requesterless `"none"` is intentionally coverage-only, specify its special
transition separately. Add first-event vectors for Head, `"none"`, requester
snapshot, and growth after each.

### 1.2 Empty requester snapshots have no per-generation meaning

The registry and CDDL say a requester may carry empty `cutoffs` plus
`{ zone_id, heads: [] }`, and that this pure snapshot retires everything in
the zone
([`c.cutoff`](/Users/vm/owner-plane-d0a-spec.md:1259),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3304)). But the equation overrides
`admit_bound` only “for every generation it carries.” An empty Head set carries
no generation, so the equation updates no per-generation state.

A nonempty snapshot has the related omission problem: it does not say what
happens to generations omitted from the carried set. Inferring an implicit
`"none"` for whatever generations a replica currently knows is arrival-
relative; applying it to all future generations can prevent legitimate later
`w.gen` openings.

Make absence explicit. Options include requester-form per-generation
`"none"` entries, a `retired_gens` set, or a signed generation high-water plus
per-generation bounds. Then define the exact `admit_bound` transition. Do not
make an empty array carry an unbounded, fold-relative generation set.

## 2. D-130's per-generation promotion is not an authority closure

The new promotion rule correctly freezes every generation that has accepted
ratify state. But coverage for a `(zone, lineage)` is satisfied when
`ratified_through != Absent` on **some** generation, and generations without
ratify state freeze nothing
([coverage and promotion](/Users/vm/owner-plane-d0a-spec.md:1474)).

This fails in two ways.

First, a strict epoch advance may treat g1/H10 as covering the lineage while
g3 remains writable under the old epoch. Second, even if every currently live
generation is ratified, the old signer can later mint
`w.gen(g4, last_known = g3/H5)` at the old capability epoch: staleness against
the current epoch is deliberately not an admission predicate
([epoch closure](/Users/vm/owner-plane-d0a-spec.md:1826)). Per-generation caps
freeze known history; they do not end the authority to open a later
generation.

Strict close therefore needs both:

1. per-generation promotion, preserving the no-widening map; and
2. an authority-ending scalar lineage ceiling, preventing any later old-
   authority coordinate beyond the consumed frontier.

Require an explicit scalar `zonecutoff` on each consuming epoch advance or
renewal. If staged coverage remains, add a standalone scalar-close mechanism;
“some generation has ratify state” cannot substitute for an authorship
ceiling. This is the simpler and safer resolution of the v0.5.9
per-generation-to-scalar question.

## 3. Exact-Head selection needs the same pending reservation as D-131

D-130 fixes the one-selector trace: a cutoff naming `H_A` pends until `H_A`
arrives and then selects it regardless of whether conflicting `H_B` arrived
first
([reference lifecycle](/Users/vm/owner-plane-d0a-spec.md:1442)).

It does not fix two selectors:

1. earlier boundary B1 names `H_A`, which is absent, so B1 pends;
2. later boundary B2 names held `H_B` at the same coordinate;
3. later control operations explicitly pass the earlier pending reference, so
   B2 accepts and selects B; and
4. `H_A` arrives.

A fresh fold holding both Heads processes B1 first and selects A, while the
incremental fold already selected B. D-131 solved the identical issuer problem
by reserving the pending issuer scope
([commitment registry](/Users/vm/owner-plane-d0a-spec.md:773)). Apply the same
rule to tenant Head selectors: an earlier pending selector reserves the
affected `(zone, lineage, generation, coordinate)`/suffix scope, and later
conflicting selectors pend behind it. Define reservation release when the
earlier boundary rejects or is cut.

The existing vector covers one boundary in both byte-arrival orders; add the
earlier-pending/later-conflicting two-boundary trace.

## 4. D-132 recovery and retirement rules remain non-portable

### 4.1 “Past base” has no tenant coordinate

A recovery body carries `base = { control seq, control op }`. Ordinary tenant
operations carry certificate/grant hashes and `capability_epoch`, but no
authoritative control frontier; `ctrl_frontier` is explicitly diagnostic and
`created_hlc` is chronology only
([AuthorizationProof](/Users/vm/owner-plane-d0a-spec.md:490),
[SignedOperation](/Users/vm/owner-plane-d0a-spec.md:505)).

D-132 nevertheless says an omitted cut-branch lineage quarantines “entirely
past base”
([C3′](/Users/vm/owner-plane-d0a-spec.md:1540)). A delayed pre-fork write and a
post-fork mint under the same surviving certificate, grant, and epoch are the
same kind of signed byte; no fresh fold can order either against the control
base. The per-generation equation also contains no recovery-omission event or
implicit value, despite promising later ratify revival.

Choose a portable rule per `(zone_id, lineage)`, not bare lineage. For
example:

- omission means an explicit universal/zero-history `admit_bound` override;
- the recovery carries a signed tenant Frontier/default commitment; or
- recovery references a checkpoint/paged cutoff map whose tenant coordinates
  are known.

The current wording solves the 256-entry capacity problem with a boundary the
wire cannot calculate.

### 4.2 C3′ removal must re-fold every control-derived admission input

D-131 lists commitments, boundaries, closures, and eligible caps as symmetric
removal inputs. It omits other control state that can be introduced on a cut
branch. For example:

1. cut-branch `c.lineage_reauth` revives a held `w.gen`;
2. recovery cuts that reauthorization; and
3. a fresh fold again produces `lineage-gen`, while the incremental removal
   inventory never reverses the widening.

The same issue applies to a cut `c.space_retire`, capability/policy epoch,
grant, certificate, or budget-window transition. State the stronger and
simpler rule: C3′ reconstructs the entire surviving control-derived reducer
state and re-evaluates every tenant operation. The named list should be
examples, not an exhaustive subset. Vector cut-reauth and cut-retire cases.

### 4.3 `c.space_retire` is unencodable and bypassable

The registry says retirement advances the zone capability epoch and, under
strict policy, carries closure cutoffs
([registry](/Users/vm/owner-plane-d0a-spec.md:1252)). But:

- §9.4 says only `c.cap_epoch_bump` and `c.zone_policy` advance epochs and does
  not define `policy(e)` for a retirement-opened epoch
  ([§9.4](/Users/vm/owner-plane-d0a-spec.md:1792));
- `cspaceretire = { space_id }` has no `cutoffs`, `zone_id`, or `new_epoch`
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3239)); and
- a malicious writer can sign a post-retirement operation with the old epoch.
  In a lenient zone, old-epoch writing explicitly remains open; chain
  monotonicity only stops writers whose own chain already advanced.

Thus the rule cannot distinguish the delayed pre-retirement write it wants to
preserve from a post-retirement mint under an old anchor. Make retirement an
immutable space-qualified per-lineage cutoff, or require complete scalar
closure for every authoring lineage. If the epoch event remains, align §9.4
and CDDL, but do not claim the epoch alone provides author-time evidence.

## 5. D-128 cap eligibility is safer, but not yet a closed authority lifecycle

D-128 correctly adds parse/body, certificate/grant, lineage ownership,
generation arithmetic, and same-zone terminal-Head checks
([cap eligibility](/Users/vm/owner-plane-d0a-spec.md:1320)). This resolves the
signature-only v0.5.9 defect.

Three issues remain.

### 5.1 Deadline and lease authority are bypassed

`cap_eligible` ignores not only revisable budget consumption but also
deadline/lease receipts, while the resulting cap grants permanent truncation
and effect-final closure regardless of the `w.gen` budget result. An expired
or off-lease writer can therefore close history even though ordinary `w.gen`
admission denies it.

If this is a deliberate new capability, name it as an explicit owner ruling
and adversary residual; “signature possession is not truncation authority” is
then too broad. Prefer requiring all non-budget IAM, including qualified
deadline/lease proof. Missing proof can pend, and later proof compromise can
dissolve/rederive the cap under the same residual already used for escaped
effects.

### 5.2 Every eligibility transition must create or dissolve the cap

The predicate can change under ordinary revoke/grant cutoffs, certificate
renewal, tenant-branch selection, or removal of a reauthorization that made
the generation window valid. Dissolution names only a compromise cutoff and
C3′ branch cut. Define cap existence as a derived function of the full current
`cap_eligible` predicate and include every input transition in the revisit
inventory.

The phrase “cap riding the cut branch” has the same cross-domain problem as
recovery omission: a tenant `w.gen` has no authoritative control frontier.
Tie dissolution to explicit tenant coordinates or to the complete surviving
fold, not an unstated control-branch attachment.

### 5.3 The retention bound is false

The text calls cap-bearing bytes GC-exempt and says the 64-open-gap cap bounds
them. That cap applies to `w.gen(last_known = "unknown")`; cap-bearing
operations use a known `last_known` Head, and repeated reauthorization permits
unbounded generations over a forever-retention plane
([generation rules](/Users/vm/owner-plane-d0a-spec.md:1745)). Define checkpoint
materialization/retention of cap evidence or state that it grows with history;
do not rely on the unrelated unknown-gap bound.

## 6. D-134's transfer terminal is not stable under allowed future events

D-134 now treats operations beyond `immutable_cap` or in a voided generation
as resolved-negative and writes a cleanup/abort terminal
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:1095)). But D-128 and
D-131 explicitly allow the cause to disappear:

- an incorporation cap dissolves under a qualifying cutoff or recovery; and
- C3′ removes cut-branch seals, boundaries, closures, and caps and re-derives
  their consequences.

Trace:

1. release R is accepted and `PendingXfer` is durable;
2. a branch-local seal/cap puts R beyond `immutable_cap`;
3. recovery writes `XferAbort(reason = "release-rejected")` and clears the
   journal;
4. C3′ cuts the branch containing that boundary, or a compromise cutoff
   dissolves the cap; and
5. R re-admits, but its terminal permanently bars the transfer.

A destination import listed in `missing` has the same contradiction. The
correct predicate is **terminal-stable under every allowed future transition**,
not “currently beyond a boundary described as immutable.”

Choose one:

- keep branch-relative negative states dormant rather than terminal;
- bind each terminal to a control/recovery basis and invalidate/reopen it when
  that basis is cut; or
- add a durable `XferReopen`/recovery-adoption transition.

The escaped-effect residual is not enough by itself: the terminal is durable
protocol state that controls future imports. Vector cleanup followed by
cap/seal/checkpoint removal and compare incremental recovery with a fresh fold.

### 6.1 Immutable scalar cutoffs are missing from the permanence lattice

Supersede, revoke, close, and explicit recover `zonecutoff`s are declared
immutable authority-ending boundaries. Yet §10.5 places every `cutoff`
quarantine in the generally revivable row and attaches permanence only to
per-generation `immutable_cap`/void
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1971)). D-134's
resolved-negative list has the same omission.

An operation beyond an explicit recovery or revoke cutoff must not revive
under generic ratify growth and must be eligible for whatever terminal policy
applies to branch-stable negative state. Split the contextual lifecycle by
boundary purpose, and keep D-132's omitted recovery blanket separately
revivable if that remains the ruling.

## 7. Lowest-hash import selection is convergent but not final

D-134 derives import content and chooses the lowest `op_hash` among
same-replay-key claimants
([import rule](/Users/vm/owner-plane-d0a-spec.md:2291)). That selects a stable
winner for a fixed set, but no finite event says another lower claimant cannot
arrive later from a different authorized lineage.

Claim identity is the emitting operation hash. Judgments, pins, evidence, and
erase requests target that hash. Therefore:

1. higher-hash `I_hi` accepts and becomes effect-final;
2. the source writes `XferDone`, and users judge, pin, or erase `I_hi`;
3. later `I_lo` arrives with the same replay key and a lower hash; and
4. D-134 rejects `I_hi` and substitutes the distinct claim `I_lo`.

The choice is not semantically invisible. References still target `I_hi`, and
an erased import can reappear under `I_lo`. Hash grinding makes the race
adversarial, not merely accidental. The transition also contradicts T2's
exhaustive revisit inventory, which names only proofs, budgets, and boundaries
([T2](/Users/vm/owner-plane-d0a-spec.md:690)).

The claimant set is itself undefined: does a malformed, unauthorized,
pending-proof, budget-quarantined, or later-disqualified lower hash reserve the
key? Counting every held body enables denial; counting only accepted bodies
requires winner removal and loser revival despite `import-collision` being
`reject-permanent`.

Use either:

- one uniquely designated import writer; or
- a logical imported-claim identity derived from
  `(from_plane, release_op, source_op)`, with signed carrier operations treated
  as aliases rather than semantic claim identities.

Do not use a mutable operation-hash minimum as the durable claim ID.

## 8. D-133 needs a separate authorship set and transitive KEM custody

### 8.1 `history_cutoffs` cannot follow current KEM membership

Certificate-renewal `history_cutoffs` delimit predecessor authorship and keep
old accepted history valid
([certificate renewal](/Users/vm/owner-plane-d0a-spec.md:339)). V0.5.10
requires their coverage over the same current-membership set used for
replacement wraps
([renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:3168)).

Trace:

1. device D writes H1 in zone Z under the old certificate;
2. a later epoch excludes D's wrap from Z;
3. D renews its certificate for another zone; and
4. Z requires neither a replacement wrap nor a history cutoff.

The old H1 either loses its position-relative certificate validity, or the old
key remains unbounded for later minting. Split a fourth predicate:

- replacement-wrap set = latest-epoch current membership;
- Kold custody = actual locally unretired old-wrap tuples; and
- history coverage = every `(zone, lineage)` with predecessor authorship
  authority or history, independent of current KEM membership.

### 8.2 Overlapping KEM renewals need transitive obligations

If K0→K1 begins while an old K0-wrapped epoch remains unretired, and K1→K2
occurs before the first drain completes, global wrap equality now permits only
K2 wraps. State that a current descendant-key wrap satisfies every ancestor
custody obligation, and retain each old secret until every tuple it can open is
covered by the current descendant or retired. Rejecting the second renewal
would make portable admission depend on local retirement, so transitive local
obligations are preferable.

Also correct the cap-slot wording: a later epoch's exclusion frees the wrap
component only; an active finite grant to that zone still contributes to
`held_zones`.

## 9. Signing-key freshness is not yet plane-wide and forever

D-131 rejects a **renewal** whose signing key appeared in any historical
certificate. A new-device enrollment may still reuse another device's key.
T2 excludes self-receipts by `device_id`, so one private key enrolled as D1 and
D2 can sign an operation as D1 and its purportedly independent receipt as D2
([witness qualification](/Users/vm/owner-plane-d0a-spec.md:652)).

Apply signing-key uniqueness to every non-genesis enrollment, not only
`renews`, unless shared-key witnesses are an explicit accepted posture.

“Forever” also requires consensus state. If a key first appears on a branch
later cut by C3′, a fresh surviving fold may forget it and permit reuse. Keep a
GC-exempt, non-removable historical-key tombstone registry, or scope issuer
feeds by certificate incarnation so reuse cannot resurrect the old sequence
domain. Vector same-key D1/D2 and cut-branch-key reuse.

## 10. Schema and mirror exactness

These repairs are smaller but should land with v0.5.11:

- Prose requires import provenance to be exactly the import triple with
  evidence absent, while `mimport.provenance` requires an `evidence` array
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3518)). Use a purpose-specific map
  or require `evidence = []`; absence and empty are distinct under E4.
- Require `provenance.import.from_plane == release.header.plane_id` and
  `provenance.import.digest == release.body.content_digest`. Varying
  `from_plane` otherwise partitions the replay key.
- Require `PendingXfer.content_digest == release.body.content_digest` and
  `PendingXfer.dest_zone == release.body.to.zone_id`.
- `release_op` is called “never signed input,” but it appears inside the signed
  `mimport` body. Call it a signed mirror that MUST equal the independently
  derived release hash.
- The D-134 ledger says `record_count` is mirrored across artifacts that do
  not carry it. State the actual equality
  `PendingXfer.record_count == |release.sources| == |bundle.recs|` in a
  per-field matrix.
- Namespace effect keys if release delivery and source-terminal append use one
  shared dedupe store, e.g. `("delivery", release_op)` and
  `("terminal", release_op)`.
- The D-131 required vectors still need all three carrier pairings in both
  control orders, reservation release after a pending carrier rejects, and C3′
  removal for each carrier.
- D-117/D-118's previously requested unFenced and multi-generation vectors
  are now present; preserve them.

The normative companion `d0a-vector-cases.v1.json` remains absent, so Gate A
is false independently of these prose findings.

## Required owner rulings

The next revision needs explicit choices in six areas:

1. **Boundary fold:** first ratify transition and explicit per-generation
   snapshot absence.
2. **Authority closure:** mandatory scalar ceilings for epoch advance/renewal,
   plus pending tenant-selector reservation.
3. **Recovery/retirement:** a tenant-coordinate omission default, full C3′
   re-folding, and a non-backdateable space-write boundary.
4. **Cap/finality:** whether deadline/lease proof is required, every cap
   dissolution event, retention, and terminal behavior when a basis dissolves.
5. **Import identity:** a designated writer or derived logical claim ID, plus
   collision eligibility and downstream-reference behavior.
6. **Renewal identity:** authorship coverage independent of KEM membership,
   transitive KEM obligations, and truly plane-wide signing-key uniqueness.

## Recommended v0.5.11 sequence

1. Correct the D-129 tagged state machine and make snapshot absence explicit.
2. Require scalar authorship ceilings on consuming epoch/renewal operations;
   retain per-generation promotion as the complementary history freeze.
3. Extend pending reservation to tenant selectors.
4. Replace recovery “past base” and epoch-only space retirement with explicit
   tenant coordinates; make C3′ a complete surviving-state re-fold.
5. Decide cap proof authority and basis-scoped terminal/reopen behavior; repair
   the permanence lattice for scalar cutoff purposes.
6. Replace mutable lowest-hash import identity with a designated writer or
   derived logical claim identity.
7. Split renewal authorship from KEM membership, define transitive KEM custody,
   and close signing-key history.
8. Align CDDL, mirror matrices, decision rows, and vector requirements.
9. Only then author the companion and corpus.

## Bottom line

V0.5.10 is closer: it repairs the v0.5.9 fixed point, removes signature-only
cap authority, completes much of the issuer commitment state machine, and
fixes the central KEM exclusion trace. But the remaining gaps affect strict
closure, recovery determinism, durable transfer identity, and witness
independence—the protocol's core safety properties.

Do not freeze this draft. A v0.5.11 can close these seams without reopening
the Owner Plane or Memory architecture.
