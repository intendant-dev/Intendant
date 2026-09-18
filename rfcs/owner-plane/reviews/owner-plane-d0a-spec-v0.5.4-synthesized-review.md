# Synthesized review: D0-A Core + Memory normative specification v0.5.4

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.4-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.4-review.md)
and
[owner-plane-d0a-spec-v0.5.4-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.4-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.4.
This document is not a union, vote count, or compromise between the reports:
it replays disputed claims against the protocol and records one adjudicated
disposition.*

## Executive verdict

**Both reviews correctly conclude that v0.5.4 is strong progress, that a
v0.5.5 is required, and that Gate A is unavailable. The peer contributes an
important contradiction and four exact omissions. Its broader conclusion
that all six prior blocker areas are discharged and only about ten sentences
of prose remain is not supported by the composed protocol.**

The peer is exactly right that T2 and D-69 still call compromise cutoffs the
sole revisit path for admitted operations while D-86 explicitly adds budget
displacement. It is also right about the missing grant-epoch lower bound, the
64-lineage strict-closure dead end, the absent referenced-Head lifecycle, and
the missing cutoff equality pins. Those findings are adopted in full. Its
renewal-volume diagnosis and suggestion to track the absent companion schema
are also useful.

Where the peer is too optimistic is at newly created seams. Passing the old
revoked-grant and two-generation arrival traces establishes the local D-86
repairs, not effect finality. A proof-feed head commitment detects an
alternative prefix eventually, not before that prefix has qualified an
operation. “Intent closes at Fence” is not durable when intent lives in the
control log and Fence in a tenant log with no committed control position.
Checkpoint pages are individually bounded but do not yet form a monotone
state machine. “Admission-time validated” bundle size is still only asserted
by the budgeted writer in the portable bytes. And the new control pipeline
applies recovery precedence before it authenticates the recovery body that
contains the precedence facts.

The adjudicated disposition is:

- **Resolved:** the original fold-current certificate/grant replay defect;
  signed budget-window selection and the simple two-generation arrival race;
  the hash-chain primitive itself; recovery-epoch resurrection of requester
  attestations; checkpoint E6 and pending-coordinate fences; state-6 rotation
  serialization; the new/renewal CDDL union; the direction of source-erasure
  terminality; the epoch-1, destroyed-epoch and survivor-count pins; header
  provenance.
- **Partially resolved:** D-86 through D-92. Each has useful landed machinery
  or mechanical pins, but the decisions still leave protocol or wire seams;
  D-92's cutoff-composition pin is present in prose but not executable.
- **V0.5.5:** required as a focused protocol/schema cut, not merely a prose
  errata pass.
- **Schema freeze:** no.
- **Gate A:** no. The normative case schema, core, corpus and harness do not
  exist; family 14 remains open; several transitions are not yet executable.
- **Durable P1 writes:** remain prohibited under the unchanged later-gate
  requirements.

## Assessment of the peer review

### Findings to adopt

1. **B1 is exact and was missed by the first review.** T2 says admitted
   operations are revisited “only” by compromise cutoffs; D-69 says the same;
   D-86 displaces an admitted operation when a canonically earlier budget
   consumer arrives. Implementations following those texts produce different
   accepted sets.
   ([T2](/Users/vm/owner-plane-d0a-spec.md:596),
   [D-69](/Users/vm/owner-plane-d0a-spec.md:2077),
   [budget displacement](/Users/vm/owner-plane-d0a-spec.md:374))
2. **H1 is a real authorization hole.** Add
   `op.capability_epoch >= grant.capability_epoch` before calculating slack.
   Otherwise a grant issued at epoch 5 can select `policy(1)` on a new chain,
   and unsigned subtraction is either underflow or an accidentally satisfied
   inequality.
   ([grant slack](/Users/vm/owner-plane-d0a-spec.md:1293))
3. **H2 is constructibility, not polish.** A strict policy/bump must close
   every live lineage, but its cutoff set caps at 64. A 65-lineage zone cannot
   change policy or open a budget window.
   ([closure rule](/Users/vm/owner-plane-d0a-spec.md:1300),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2375))
4. **M1 is an arrival-order gap.** A cutoff that arrives before its named
   tenant Head needs `pending-dependency`; a later different hash at the same
   coordinate needs a pinned permanent outcome.
   ([zonecutoff](/Users/vm/owner-plane-d0a-spec.md:2405))
5. **M2 is required exactness.** A grant cutoff must name that grant's zone
   and lineage; policy/bump closure entries must name the advancing zone and
   one of its live lineages.
6. **The renewal wrap-count diagnosis is correct.** Accepted-unretired
   memberships are unbounded while a renewal carries at most 128 wraps.
7. **The companion schema should be explicitly tracked while absent.** The
   peer correctly observes that Gate A is mechanically impossible until it,
   the corpus and the harness exist.
8. **Displacement telemetry is a good product pin.** Use the existing
   quarantine-reproposal review lane rather than inventing an implementation-
   specific surface.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-86 budgets | Machinery complete after correcting “sole revisit” | Text contradiction is real, but loser revival, charge semantics and irreversible-effect finality remain undefined |
| D-87 feeds | No residue | Chain commits eventual identity; qualification before a missing forward path reaches the committed head remains unsafe |
| Requester freshness | No residue | `live_heads` is signed but absent from the wire; singular-head semantics and registry/Appendix formulas disagree |
| D-88 checkpoints | No residue | Retirement paging, page replacement/non-regression, joint construction and witness cardinality remain open |
| D-89 rotation | Queue deadlock dissolved | State-6 ordering is fixed, but Fence cannot reconstruct its control-log recipient set and “unretired” is replica-local |
| Renewal overflow | One pointer to `c.wrap_add` | Diagnosis right; remedy needs future-key staging/commit or a hard drain/cap, not an ordinary wrap-add pointer |
| D-90 transfer | No residue | Terminal trigger order is unrecoverable and bundle charge is self-attested by the constrained writer |
| D-91 control | Pipeline complete | Recovery body/hash is checked after the place/precedence decision that depends on it |
| D-92 cutoff maximum | Verified pin | Bytes contain the phrase, but no comparator, purpose scope or non-resurrection fold makes it executable |
| Companion schema | Open-list pin | Track it, but D-91 remains artifact-pending because the spec delegates normative case shapes to a nonexistent file |

The peer's review method is otherwise strong: it is concise, bytes-first, and
excellent at checking whether named changes landed. Its weak point is stopping
after each old counterexample passes. V0.5.4 introduces interactions among
control history, tenant history, local storage and external effects; those
interactions require fresh composed traces.

### Corrections and qualifications to the first review

The first review also needs three refinements:

1. **Reauthorization banking is a posture decision, not a demonstrated replay
   vulnerability.** Accepting one reauthorization increments
   `lineage_version` and invalidates other signatures made at the old version.
   An unused signature can nevertheless remain valid while generations are
   consumed. If that is a durable one-shot owner capability, say so; if it is
   assent to the *currently exhausted* state, bind consumption/live heads. It
   is not independently Gate-blocking without choosing the latter meaning.
2. **The checkpoint joint-size issue is medium, not by itself a safety
   ambiguity.** The overall 64-KiB cap deterministically rejects an oversized
   triple. The defect is that the advertised individual maxima are not jointly
   constructible and no deterministic page byte budget is supplied.
3. **Checkpoint retirement should be stated more precisely.** The real
   contradiction is between immediate `w.gen`/cutoff retirement and a
   since-predecessor `retired` delta whose omitted entries supposedly remain
   live for later pages. If `retired` is intended to mean only retirements
   caused by this checkpoint, say so and require each member to be live in the
   checkpoint pre-state, not merely at its predecessor. If it is a complete
   retirement-event delta, it needs a durable cursor/log. The current wording
   cannot support both meanings.

The first review's other major findings survive adjudication.

## Consolidated v0.5.5 protocol findings

### 1. Define one scoped cutoff algebra

The peer's H1/H2/M1/M2 belong to one larger missing abstraction.

#### 1.1 Add the grant-epoch lower bound

Before slack subtraction:

```
grant.capability_epoch <= op.capability_epoch
op.capability_epoch - grant.capability_epoch <= grant_epoch_slack
```

Both comparisons use signed inputs. Add the epoch-5-grant/epoch-1-operation
negative to family 10.

#### 1.2 Make strict closure scalable and arrival-independent

A closure exceeding 64 lineages needs a staged set followed by one atomic
advance, or an exact union with earlier `c.cutoff` operations. Reusing
standalone cutoffs is plausible, as the peer suggests, but requires more than
a cross-reference:

- define which prior cutoffs belong to this advance;
- ensure every lineage is still closed at the committed boundary;
- define whether writes after a staged cutoff are impossible, quarantined, or
  invalidate the stage; and
- make the final advance prove complete coverage without embedding more than
  64 entries.

A referenced tenant Head is pending until its exact bytes and chain position
arrive. Same coordinate/different hash is a fork or cutoff-body failure with
one pinned outcome. Grant and closure cutoffs then enforce the peer's equality
pins.

#### 1.3 Scope composition by authority event, not merely zone and lineage

D-92 says every accepted cutoff for `(zone, lineage)` composes at the maximum.
But `zonecutoff` is reused by certificate renewal, device revocation, grant
revocation, epoch closure, recovery and generic lineage retirement. These are
not one global authority boundary.

Counterexample:

1. Grant G is revoked at H5.
2. A later renewal or generic cutoff for the same lineage carries H10.
3. Global maximum composition either restores G for H6–H10 or proves the
   cutoffs were meant to be scoped per consumer—scope absent from the bytes.

Even within one purpose, H5 quarantines H6 while a later H10 makes H6 stand on
a fresh fold. “Maximum” therefore expands the standing prefix; it is not
monotone retirement unless quarantine and every dependent decision can
revive.
([cutoff composition](/Users/vm/owner-plane-d0a-spec.md:2405))

Define a boundary identity/purpose, the exact Head comparator (including
equal-coordinate hash), affected live heads, and one of two semantics:

- later boundaries cannot widen standing authority; or
- widening is allowed and quarantine is a reversible derived fold with
  cascaded revival.

Do not let a generic cutoff amend a grant-, certificate-, epoch- or recovery-
specific boundary merely because their zone and lineage match.

### 2. Turn D-86's canonical budget order into effect finality

First repair the peer's B1 wording: T2/D-69 enumerate both explicit
compromise cutoffs and canonical budget displacement. That makes the prose
internally consistent, but it does not finish the state machine.

Example with a two-operation budget:

1. `w.gen(last_known="unknown")` and egress release B in generation 2 fit the
   budget; B is accepted and egress completes.
2. A delayed generation-1 operation A sorts before both.
3. Canonical consumption displaces B.
4. The exported bytes cannot be un-exported.

The same issue applies to releasing an audited read result. D-86 defines the
eventual classification but not the point at which an effect may escape.
([budget order](/Users/vm/owner-plane-d0a-spec.md:368),
[egress completion](/Users/vm/owner-plane-d0a-spec.md:1729))

Also decide what happens when A later loses its only qualifying receipt under
a compromise cutoff. If only admissible operations consume budget, B must
revive and the affected suffix must be re-derived. If quarantined operations
continue consuming permanently, state that instead. The current text does
not define the eligible charge set.

Add a reservation or finality barrier before egress, audited-result release,
same-plane import, or any later effect consumer. One possible predicate is
that no canonically earlier coordinate can still arrive; another is a signed
budget reservation/checkpoint. The exact choice is a protocol decision, not
telemetry wording.

### 3. Finish proof-boundary and requester bytes

#### 3.1 A head commitment must gate qualification until ancestry is known

Suppose a compromise cutoff commits `(through=100, head_hash=H100)`, while a
replica holds honest statements 1–50. A compromised issuer creates statement
51′ with `prev_stmt=H50`. Its immediate predecessor exists and matches, so the
literal T3 rule accepts the link. Only when the chain reaches the committed
H100 does the replica discover that 51′ was on the wrong branch. In the
meantime 51′ can qualify an operation.
([T3](/Users/vm/owner-plane-d0a-spec.md:603))

Once a cutoff or checkpoint commits a feed head, no statement at or below the
boundary should qualify until a complete verified path connects it to that
head. A missing forward path is `issuer-gap`/pending; a mismatching boundary is
`issuer-fork`. Repeated minimum cutoffs must prove that the smaller committed
head is the corresponding ancestor of the larger one.

Hash chaining was the right D-87 repair. This rule completes rather than
replaces it.

#### 3.2 Carry the state that the requester signs

`ccutoff.requester` contains only `{device_cert, ctrl_frontier, sig}`, while
the signature input includes `live_heads`. A validator must reconstruct the
signed message from replica-local tenant state, so control-first delivery can
look like `sig-invalid` where tenant-first delivery succeeds. The comment also
defines one head per zone even though an unknown-gap lineage may have several
live generation heads.
([requester CDDL](/Users/vm/owner-plane-d0a-spec.md:2413),
[Frontier](/Users/vm/owner-plane-d0a-spec.md:497))

Carry the full sorted zone-qualified live-head set, or its hash plus a carried
frontier object. Missing heads are pending; omitted, stale, cross-lineage and
same-coordinate/different-hash sets are negative vectors.

Make the normative signing formulas byte-identical at the same time:

- the registry's `c.lineage_reauth` formula omits `repoch`;
- the registry's `c.cutoff` formula omits `repoch` and `live_heads`; and
- Appendix A includes those fields.

([registry](/Users/vm/owner-plane-d0a-spec.md:1005),
[Appendix](/Users/vm/owner-plane-d0a-spec.md:2387))

### 4. Make checkpoint pages a monotone state machine

V0.5.4 correctly removes body `v`, adds pending fences and feed-head hashes,
and names the dropped-witness lane. Preserve those changes.

The remaining transition needs:

- **retirement semantics:** either a durable retirement-event cursor that can
  page more than 256 intervening retirements, or checkpoint-only retirement
  defined against the immediate pre-state;
- **effective keys:** say whether a later lineage page replaces or accumulates
  earlier `(lineage, gen)` heads;
- **one coordinate order:** define the fence comparator across generations
  and forbid lower later fences/coverage;
- **joint construction:** add an encoder-exact byte inequality or fixed page
  budget because 256 covers + 256 fences + 256 retired + 64 proof positions
  are about 66 KiB before the envelope; and
- **witness cardinality:** cap `ZonePolicy.time_witnesses` at 64 or define a
  paged/accumulator/sufficient-subset proof rule.

([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1009),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2222),
[witness CDDL](/Users/vm/owner-plane-d0a-spec.md:2303))

Also add checkpoint `covers` and `fences` to E7's central declared-logical-key
inventory; their CDDL comments currently declare keys that the inventory
omits.

### 5. Close rotation and renewal in portable durable bytes

#### 5.1 Fence does not commit the control side of recipient intent

The intended set is rotation wraps plus `c.wrap_add`s accepted before Fence.
Those wrap-adds live in the control log; Fence lives in a tenant log and stores
only `{kek_epoch, rotation_op, fence_frontier}`. Its frontier is tenant state,
not a control position. A crash before RewrapDone loses which wrap-adds counted,
and replicas can Fence around the same wrap-add differently.
([rotation intent](/Users/vm/owner-plane-d0a-spec.md:800),
[Fence CDDL](/Users/vm/owner-plane-d0a-spec.md:2446))

Commit `control_frontier` plus an exact recipient-set hash in Fence and
RewrapDone, or close intent in a portable control operation. Likewise, remove
replica-local Fence progress from `c.wrap_add` admission: “accepted,
unretired epoch” needs a control-log definition.

#### 5.2 Recovery can cut behind an already activated Fence

This composed counterexample was not isolated by either input review:

1. Rotation R is accepted on the control chain.
2. A replica Fences R, rewraps, destroys the old KEK and writes tombstones.
3. A competing control operation at R's sequence is discovered, or recovery
   otherwise chooses a base before R.
4. C3′ installs recovery at that base and retires R's branch.
5. Surviving control state no longer contains R or its wraps, while storage is
   served under R's new epoch and the old KEK is gone.

C3′ currently permits the branch cut and carries tenant cutoffs, not an
activated-storage adoption manifest.
([C3′](/Users/vm/owner-plane-d0a-spec.md:1044),
[rotation states](/Users/vm/owner-plane-d0a-spec.md:795))

Recovery must not cut behind an activated Fence unless the recovery operation
adopts the already-Fenced KEK epoch, wraps, recipient commitment and storage
frontier. Alternatively define a storage-effect finality floor that constrains
the recoverable base. This is the storage analogue of budget effect finality.

#### 5.3 Renewal overflow needs staging or a hard bound

The peer correctly finds that 65 zones with two accepted-unretired epochs
already require 130 replacement wraps. Its proposed pointer to ordinary
`c.wrap_add` does not close the lifecycle: before renewal the new KEM key is
not the enrolled key; after renewal omitted memberships still depend on the
predecessor key.
([renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2333))

Choose one:

- a bounded prepare/commit ceremony binding staged wraps to the future
  certificate hash and retaining the old certificate/key until completion;
- a continuation with equivalent all-or-nothing activation; or
- a hard precondition that accepted-unretired memberships/rotation queues
  drain to at most 128 before KEM renewal.

Interlock renewal with the Fence→KekDestroyed interval. Require each
replacement wrap's `recipient_kem_key` to equal the renewed certificate's KEM
key, and pin outer/inner zone, epoch, plane and device equality for wrap-adds
and rotation wraps.

Secondary renewal pins:

- allow empty `history_cutoffs` when required coverage is empty;
- narrow “mints no authority” to “mints no grants or new-zone access,” or
  constrain expiry-deadline changes; and
- state that pre-established cutoffs already close authority—renewal only
  consumes their coverage.

### 6. Make transfer terminality and charging independently verifiable

V0.5.4's source-erasure direction is sound: once any source is erased, the
flat bundle cannot prove remaining membership, so all unimported records
abort. The peer is right to credit that.

Two terminal rules remain:

1. “Source erasure wins” conflicts with `reason = FIRST terminal trigger`.
   Trigger order is not journaled, so a crash cannot reconstruct whether a
   destination rejection preceded erasure. Use deterministic state precedence
   or persist the observation.
2. If all imports committed before a crash and erasure occurs before
   XferDone, recovery must check the replay keys first and write XferDone—not
   `XferAbort {missing: []}`. Require nonempty abort residue.

([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:896),
[abort CDDL](/Users/vm/owner-plane-d0a-spec.md:2472))

More importantly, `bundle_size` and `content_digest` are accepted by later
replicas as writer-signed facts. A signature proves what the writer asserted,
not that an independent validator checked it. The adversary table explicitly
says signatures do not defend against a compromised signer signing bad
content. A budgeted writer can sign `bundle_size = 0` and evade `max_bytes`.
([export charge](/Users/vm/owner-plane-d0a-spec.md:1698),
[adversary boundary](/Users/vm/owner-plane-d0a-spec.md:1989))

Use a deterministic content-independent surcharge, an independently signed
validation receipt, or a durable committed validation transition. Then place
transfer/egress execution behind the D-86 effect-finality rule from §2 above.

### 7. Authenticate the recovery body before giving it precedence

The dedicated arm-indexed pipeline is a good D-91 correction, but its order is
not valid yet:

```
parse -> arm -> sig -> place/C3' precedence -> body_hash/CDDL/invariants
```

C3′ validity depends on body `base`, `epoch`, `repoch` and recovery commitment,
yet the pipeline grants its precedence exception before validating the body
or even its hash. A validly signed header paired with malformed or mismatched
body bytes can suppress the competing C2 operation under the literal order.
([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1378),
[C3′ fields](/Users/vm/owner-plane-d0a-spec.md:1048))

The normative order should be:

```
parse -> arm resolution -> header signature
      -> body_hash + registry arm + body CDDL
      -> C3' precedence-field validity
      -> placement/precedence
      -> remaining state-dependent body invariants and transition
```

Pin exact outcomes at each boundary.

### 8. Keep D-91 artifact-pending until its schema exists

Section 13 delegates closed case kinds and exact input/result shapes to
`d0a-vector-cases.v1.json`; D-91 calls that contract realized; Gate A requires
the file to predate every fixture. No such file exists under `/Users/vm`.
([vector contract](/Users/vm/owner-plane-d0a-spec.md:1821),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:2107))

Add it to Open-tracked as the peer recommends, but do not mark D-91 complete
merely by tracking it. Repair the transition semantics first, author and audit
the companion as the first corpus artifact, then require every fixture to
validate against both schemas. The core, corpus and harness remain to be
built; the family-14 result remains open.

## Adjudicated v0.5.5 closure sequence

The shortest safe order is:

1. Define scoped cutoff identities, ordering, non-widening/revival semantics,
   scalable closure and missing-Head outcomes; add the grant-epoch lower bound.
2. Define the budget eligible-charge set and effect-finality barrier; reconcile
   T2/D-69 and name displacement telemetry.
3. Complete feed-boundary ancestry and carry requester live-head bytes; align
   signing formulas.
4. Define checkpoint retirement/page accumulation, non-regression, joint
   construction and witness cardinality.
5. Commit Fence recipient/control state, constrain recovery across activated
   storage, and bound or stage renewal wraps.
6. Pin transfer terminal precedence/completion and replace self-attested
   charging with portable validation evidence.
7. Reorder control-body validation before C3′ precedence and freeze exact
   outcomes.
8. Update the decision record and open-artifact list, then author
   `d0a-vector-cases.v1.json` before any fixture.

This is still a focused patch: it does not reopen the owner-plane architecture,
crypto suite, claims-not-facts model, hosted ceiling or Memory operation set.
It is more than ten sentences because several fixes need durable fields,
outcomes and negative vectors.

## Final recommendation

Treat the peer review as valuable but incomplete. Adopt B1, H1, H2, M1, M2,
the renewal-volume warning, companion tracking and telemetry pin. Retain the
first review's cross-log, effect-finality, transfer, checkpoint and control-
ordering findings with the qualifications above. Add the recovery-over-Fence
counterexample to the rotation family.

**Cut v0.5.5; do not freeze the schema yet.** After its prose and CDDL agree,
author the companion schema first, then the core/corpus/harness, run family 14,
and perform the final prose↔schema↔vector discrepancy audit. That is the point
at which another broad prose review should give way to executable evidence and
a credible Gate-A decision.
