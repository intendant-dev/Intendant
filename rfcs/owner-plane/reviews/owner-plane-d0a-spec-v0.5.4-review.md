# Review: D0-A Core + Memory normative specification v0.5.4

*2026-07-12. Fresh review of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.4,
with the v0.5.3 synthesis used only as a regression checklist. The review
replayed authorization and budget folds, proof-feed cutoffs, checkpoints,
rotation/renewal, transfer recovery, and the control/vector contract. It did
not attempt to preserve objections that v0.5.4 actually resolves.*

## Executive verdict

**V0.5.4 is a substantial and disciplined improvement, but it is not yet a
schema-freeze candidate and Gate A should not be declared. I recommend one
focused v0.5.5 protocol pass before the vector corpus is written.**

The cut closes more than its version number suggests. Position-relative
certificate and grant authorization fixes the prior fresh-replay defect;
canonical `(gen, seq)` budget ordering fixes the simple two-generation
arrival race; chained proof feeds are the right primitive; `repoch` fixes the
recovery-resurrection trace; checkpoints now have useful fences and no E6
version violation; queued rotation and state-6 serialization are repaired;
the enrollment wire shape is finally a real union; source erasure now has a
constructible terminal direction; and the accumulated mechanical pins are
mostly sound.

The remaining problems are not a call to redesign the owner plane. They are
compositional: a rule that is deterministic in one log becomes ambiguous
across two logs, individually bounded checkpoint arrays do not jointly fit,
canonical budget displacement is unsafe once an effect has escaped, and a
writer's signature is treated as proof that the writer's own budget charge
was independently validated. The control pipeline also grants recovery
precedence before it has authenticated the recovery body, and the normative
case schema on which the vector contract now depends does not exist in the
workspace.

Recommended disposition:

- **Direction:** accept.
- **D-86 through D-92:** genuine progress; none should be rolled back.
- **Protocol/schema freeze:** no.
- **Gate A:** no.
- **Next cut:** v0.5.5, narrowly addressing the findings below and then
  authoring the companion case schema before the first fixture.
- **Durable P1 writes:** still subject to the existing later-gate prohibition;
  this review does not change it.

## Closure ledger

| Decision | Assessment | What v0.5.4 closes | What remains |
|---|---|---|---|
| D-86 | Partly closed | Historical cert/grant status is position-relative; budget-window selection and the basic cross-generation winner are canonical | Cutoff capacity/composition and budget finality/revival |
| D-87 | Partly closed | Hash-chained receipt/lease feeds, committed cutoff heads, `repoch`, and write-sensitive cutoff assent | Boundary-ancestry pendency, carried live-head state, and reauth consumption freshness |
| D-88 | Partly closed | E6, explicit pending fences, proof head hashes, and the dropped-witness lane | Joint size, retirement paging, monotone page composition, witness cardinality |
| D-89 | Partly closed | Queued wrap-add eligibility, state-6 rotation serialization, true enrollment union, class pin | Durable recipient closure and bounded/interlocked renewal custody |
| D-90 | Partly closed | Source erasure aborts the remaining flat bundle instead of requiring impossible continuation | Deterministic terminal precedence and trustworthy portable bundle charging |
| D-91 | Partly closed | A dedicated arm-indexed control path is now present in the prose | Recovery body validation precedes neither placement nor precedence; companion schema is absent |
| D-92 | Mostly mechanical closure | Genesis epoch, destroyed-epoch, survivor count, revoked-device grant, provenance, and hosted-negative pins | “Maximum cutoff” needs an executable comparator and non-resurrection semantics |

## What is now strong

These changes should survive the next pass intact:

1. **Position-relative authorization is the right model.** An operation below
   an explicit certificate or grant boundary no longer changes validity merely
   because a fresh replica folded later control state first.
   ([certificate renewal](/Users/vm/owner-plane-d0a-spec.md:316),
   [admission pipeline](/Users/vm/owner-plane-d0a-spec.md:1344))
2. **Canonical budget ordering repairs the reported race.** Window selection
   is anchored to the signed capability epoch, and `(gen, seq)` supplies the
   missing cross-generation order.
   ([budget rule](/Users/vm/owner-plane-d0a-spec.md:368))
3. **Hash chaining is the correct proof-feed primitive.** Honest delayed
   delivery can fill a missing link while a forged replacement prefix cannot
   silently join the committed chain.
   ([T3](/Users/vm/owner-plane-d0a-spec.md:603))
4. **Requester recovery freshness is materially better.** `repoch` prevents a
   recovery branch cut from restoring an old `lineage_version`, and version
   zero is now pinned.
   ([genesis descriptor](/Users/vm/owner-plane-d0a-spec.md:285),
   [requester CDDL comments](/Users/vm/owner-plane-d0a-spec.md:2387))
5. **The checkpoint direction is much clearer.** Pending operations compare
   to explicit fences; proof positions carry feed-head commitments; and the
   conservative forever-pending result for a removed witness is honest.
   ([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1009),
   [checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2222))
6. **Rotation no longer stops one state too early.** N+1 waits for N's durable
   tombstones, and accepted queued epochs can still receive wrap-adds.
   ([rotation serialization](/Users/vm/owner-plane-d0a-spec.md:800))
7. **Renewal is encodable in the ordinary case.** The CDDL union is real,
   certificate class is immutable, and the typical maximum of 128 wraps plus
   64 headed cutoffs fits below 64 KiB (approximately 57.7 KiB at the specified
   widths).
   ([enrollment CDDL](/Users/vm/owner-plane-d0a-spec.md:2327))
8. **Source erasure has a constructible transfer outcome.** Once a flat bundle
   becomes underivable, aborting all unimported records is coherent; already
   committed imports stand.
   ([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:880))
9. **The storage pins agree.** Epoch 1 is active without a Fence,
   `KekDestroyed.epoch` names the destroyed epoch, and `RewrapDone.count`
   equals the survivor-pair count.
   ([frame registry](/Users/vm/owner-plane-d0a-spec.md:839),
   [frame CDDL](/Users/vm/owner-plane-d0a-spec.md:2442))

## Gate-blocking findings

### 1. Cutoff closure is still not a closed protocol

#### 1.1 Strict advancement is unencodable above 64 live lineages

A strict `c.zone_policy` or `c.cap_epoch_bump` must carry closure cutoffs for
**every** live lineage, while every cutoff set is capped at 64. Unlike device
revocation, those operations have no continuation or prepare/commit scheme.
A valid zone with 65 live lineages therefore cannot change policy or open a
budget window.
([E8](/Users/vm/owner-plane-d0a-spec.md:99),
[closure rule](/Users/vm/owner-plane-d0a-spec.md:1300),
[control CDDL](/Users/vm/owner-plane-d0a-spec.md:2375))

Permit the advancing operation to consume a pre-established, committed set
of `c.cutoff`s, add a bounded continuation followed by an atomic commit, or
raise a separately justified zone-lineage cap. Simply emitting several
epoch-advancing operations changes the policy/epoch and is not equivalent.

#### 1.2 A referenced Head needs a pending lifecycle

`zonecutoff` requires its Head to belong to the named zone and lineage, but
does not say what happens when a replica has the control operation before the
referenced tenant Head. Rejecting makes delivery order observable; accepting
cannot prove membership. The cutoff should be `pending-dependency` until the
named operation and its chain position arrive, then either apply or fail with
a pinned permanent outcome.
([`zonecutoff`](/Users/vm/owner-plane-d0a-spec.md:2405))

#### 1.3 “Maximum accepted_through” is neither ordered nor monotone as written

A Head contains `(lineage, gen, seq, op)`, and a lineage may have several live
generation heads. D-92 says repeated cutoffs compose at the maximum without
defining a total comparator or what one cutoff does to incomparable live
heads. More importantly, these cutoffs preserve operations at or before the
boundary and quarantine operations beyond it. If cutoff H5 quarantines H6,
then a later “maximum” H10 makes H6 valid on a fresh replay. An incremental
replica either has to revive H6 and every dependent budget/result decision or
it diverges. Calling that “monotone retirement” hides the reversal.
([position-relative rule](/Users/vm/owner-plane-d0a-spec.md:316),
[`zonecutoff` composition](/Users/vm/owner-plane-d0a-spec.md:2405))

Define separately:

- the coordinate order, probably lexicographic `(gen, seq)` with exact hash
  equality at the selected coordinate;
- the live-head set affected by a boundary;
- whether accepted cutoffs may ever expand the standing prefix; and
- if they may, a reversible derived-state fold that revives all affected
  operations deterministically.

For revocation/supersession safety, the simpler posture is usually that a
given authority closure can only move toward *less* standing authority.

#### 1.4 Cross-field exactness and epoch arithmetic remain loose

The device-revocation row pins its cutoff to the target lineage, but the CDDL
and general rules do not equivalently require:

- a grant cutoff to equal the revoked grant's zone and lineage;
- every policy/bump closure entry to name the advancing operation's zone; and
- `op.capability_epoch >= grant.capability_epoch` before evaluating the
  unsigned subtraction used for slack.

The last omission permits backdating or implementation-dependent underflow.
([grant and bump rows](/Users/vm/owner-plane-d0a-spec.md:1001),
[grant slack](/Users/vm/owner-plane-d0a-spec.md:1293))

### 2. Canonical budget ordering is not yet effect finality

D-86 makes the final accepted set deterministic only while every earlier
operation remains qualified and while displacement is harmless.

First, displacement is described as moving an operation to
quarantine-reproposal. Suppose A sorts before B and displaces B under a full
budget; a later compromise cutoff retro-disqualifies A. A fresh fold now has
room for B. Unless quarantine is an explicitly reversible derived state and
the incremental reducer re-runs the whole affected suffix, B remains
quarantined only on the incremental replica.

Second, some accepted operations cause irreversible external effects. An
egress `m.export.release` completes at release acceptance. If B exported data
and a late-arriving earlier A subsequently displaces it, the bytes cannot be
un-exported. Audit release can have the same observation problem. Canonical
reclassification after effect execution is too late.
([budget displacement](/Users/vm/owner-plane-d0a-spec.md:368),
[egress completion](/Users/vm/owner-plane-d0a-spec.md:1729))

Choose and encode a finality rule for effectful operations. Viable shapes
include a signed budget reservation/sequence, a frontier/checkpoint barrier
before effect execution, or a rule that effectful operations cannot execute
while an earlier canonical coordinate remains an unknown gap. Then specify
whether ordinary displaced operations are continuously re-derived and can
revive, including cascaded dependants.

### 3. Proof boundaries and requester attestations need their complete state

#### 3.1 A committed feed head must gate the whole boundary prefix

The new chain solves the cryptographic problem but the admission lifecycle is
underspecified. If a cutoff commits statement 100 and a replica holds only
1–50, an attacker can present an alternative statement 51 that chains from
the honest 50. It looks locally well linked until the replica reaches the
committed head and discovers the fork. No proof at or below the committed
boundary should qualify until its ancestry to the committed `head_hash` is
available and verified. All affected operations should remain
`issuer-gap`/pending in the meantime.
([T3](/Users/vm/owner-plane-d0a-spec.md:603),
[cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2288))

Repeated minimum cutoffs also need an ancestry rule: the head committed at a
smaller `through` must be the corresponding ancestor of a previously accepted
larger boundary, or the feed is a fork. State this at the boundary, not only
as pairwise statement verification.

#### 3.2 `live_heads` is signed but not carried

The cutoff signature includes `live_heads`, but the requester object carries
only `{device_cert, ctrl_frontier, sig}`. A validator must reconstruct the
signed bytes from its local tenant state. When control and tenant logs arrive
in the opposite order, it cannot distinguish a missing head from a bad
signature. The comment also defines one “current head” per zone although an
unknown-gap lineage may have several live generation heads.
([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1007),
[`ccutoff` CDDL](/Users/vm/owner-plane-d0a-spec.md:2413))

Carry a sorted complete `live_heads` set, or a hash of a separately carried
frontier object, in the requester body. Validate every named head with a
pending dependency for missing bytes. Bind the full live set for that lineage
and zone, not a singular head.

The registry's displayed signing formulas also omit newly required fields:
`c.lineage_reauth` omits `repoch`; `c.cutoff` omits both `repoch` and
`live_heads`, while Appendix comments include them. Signed-byte formulas may
not be “understood” additions—make the registry and CDDL exact mirrors.
([registry formulas](/Users/vm/owner-plane-d0a-spec.md:1005),
[Appendix formulas](/Users/vm/owner-plane-d0a-spec.md:2387))

#### 3.3 Reauthorization is still bankable across generation consumption

`lineage_version` changes only when a reauthorization is accepted. A device
can sign two requests at the same version; after one generation window is
consumed, a still-unused request remains contemporaneously valid if its
`request_id` differs. If this is deliberately reusable owner assent, say so.
If reauthorization is meant to approve the live exhausted state, bind the
current generation/window consumption or the lineage's complete live-head
commitment, as the cutoff request now tries to do.

### 4. Checkpoint paging is not yet an executable monotone fold

#### 4.1 The individual maxima do not jointly fit

At maximum-width encodings, 256 `covers` heads are roughly 22.8 KiB, 256
fences roughly 13.3 KiB, 256 retired heads roughly 22.8 KiB, and 64 proof
positions roughly 7.5 KiB: already about 66 KiB before the operation envelope.
The global 64-KiB cap rejects that object, but implementations have no joint
constraint or deterministic page-building rule.
([E8](/Users/vm/owner-plane-d0a-spec.md:99),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2222))

Add an encoder-exact joint inequality or a fixed page byte budget and make
family 1 construct the maximum valid combination plus the one-byte-over
negative.

#### 4.2 Retirement cannot be deferred to an arbitrary later page

Accepting `w.gen(last_known=head)` immediately retires that head. A checkpoint
requires a `retired` head to have been live at its predecessor and says an
omitted head remains live for a later page. If that retirement is omitted from
the first checkpoint after `w.gen`, the head was no longer live at the next
checkpoint's predecessor and can never be truthfully included later.
([`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1227),
[checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1009))

Use a stable retirement-event log/cursor whose events can be paged exactly
once, or defer the actual retirement transition until its checkpoint page.
The former better preserves the current `w.gen` semantics.

#### 4.3 Page accumulation can regress

“Union of the latest page per lineage” is not a full state transition.
`covers` is keyed by `(lineage, gen)` while fences are keyed only by lineage;
the prose refers to a singular covers head. It neither says whether a later
page replaces or accumulates earlier generations nor forbids a lower later
fence. Define the effective state keys, comparator, replacement/union rule,
and non-regression checks. D-92's cutoff maximum should reuse the same
coordinate definition rather than invent another one.

#### 4.4 Witness cardinality can exceed the checkpoint cap

`proof_positions` is capped at 64, but `ZonePolicy.time_witnesses` has no
corresponding cap. A valid current policy with 65 witnesses cannot produce the
complete proof commitment the hardening rule asks for. Cap witnesses, page
proof positions with a committed accumulator, or weaken the hardening
predicate to a precisely defined sufficient subset.
([ZonePolicy CDDL](/Users/vm/owner-plane-d0a-spec.md:2303))

### 5. Rotation closure and renewal custody still cross an uncommitted seam

#### 5.1 Fence-closed recipient intent is not durably reconstructible

The intended recipient set is rotation wraps plus wrap-adds “accepted before
the Fence frame is written.” Wrap-adds live in the control log; Fence lives in
a tenant zone log and stores only `{kek_epoch, rotation_op, fence_frontier}`.
That frontier is tenant state, not a control position. After restart, the
implementation cannot prove whether a particular wrap-add preceded Fence;
two replicas can close different recipient sets.
([intent rule](/Users/vm/owner-plane-d0a-spec.md:808),
[Fence frame](/Users/vm/owner-plane-d0a-spec.md:839))

Commit an exact control frontier and recipient-set hash in Fence and
RewrapDone, or make recipient closure a portable control operation. The
intended set must be a function of durable bytes, not the race between two
local append streams.

Relatedly, `c.wrap_add` admission says an epoch must be accepted and
“unretired,” while retirement depends on local Fence/state-6 progress. A
portable control reducer cannot read replica-local storage progress. Define
control-log closure/retirement independently, and let local storage only
realize it.

#### 5.2 KEM-rotated renewal can require more than 128 wraps

A KEM-changing renewal requires a replacement wrap for every accepted,
unretired epoch in every held zone, but queued epochs and held zones have no
joint bound. One device with 129 such memberships cannot encode a renewal.
Pre-established history cutoffs solve only the cutoff side, not this wrap
side.
([renewal row](/Users/vm/owner-plane-d0a-spec.md:993),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2333))

Cap total memberships, stage future-certificate wraps, provide a continuation
that keeps the predecessor live until completion, or require the rotation
queue to drain before KEM renewal. Also interlock renewal with the interval
after Fence but before old-KEK destruction, when the retiring KEK may still be
needed to finish recovery.

Pin `wrap.recipient_kem_key == H_key({renewed cert.kem_alg,
renewed cert.kem_pk})`; the registry currently pins the renewed device but not
that key equality.

#### 5.3 Narrow the renewal authority claim

Class immutability is good, but renewal may extend or remove
`expiry_deadline_ms`, potentially reactivating persistent device-bound grants.
It therefore “mints no grants or new-zone access,” not categorically “mints no
authority,” unless deadline changes are constrained.

`history_cutoffs: [+ zonecutoff]` also makes renewal impossible for a valid
read-only or zero-history device. Permit an empty set when the required union
is empty. State that standalone `c.cutoff` operations are themselves
authority-closing ceremonies; renewal merely proves/consumes their existing
coverage.

### 6. Transfer terminality is improved, but its charge is self-attested

#### 6.1 Terminal precedence is contradictory and not crash-recoverable

The text says source erasure “wins,” then defines `reason` as the **first**
terminal trigger. Trigger order is not journaled. If destination rejection
occurs, the process crashes, and source erasure follows, recovery cannot know
which was first. Choose deterministic state precedence (for example, any
erased source selects `source-erased`) or persist every terminal observation.
([transfer terminality](/Users/vm/owner-plane-d0a-spec.md:896))

Pin the completed-before-erasure case too: if all destination replay keys
exist and the source crashed before XferDone, later source erasure must yield
XferDone—not `XferAbort { missing: [] }`. Recovery should check complete
imports before applying the erasure-abort rule.

#### 6.2 A writer signature does not prove independent validation

The release's `bundle_size` and `content_digest` are validated only by the
authoring daemon and later replicas accept them as signed facts. But an
authorized or compromised writer can sign `bundle_size = 0` and inject the
operation directly, bypassing the finite `max_bytes` budget. Its signature
proves the assertion, not that a trusted admission check occurred.
([export charge](/Users/vm/owner-plane-d0a-spec.md:1698))

Use a content-independent deterministic surcharge, a durable independently
signed admission/validation receipt, or a committed state transition that
proves the bundle was validated while readable. If revalidation is used while
sources exist, define the exact durable event after which source erasure may
rely on the previously verified values. Budget enforcement cannot trust the
budgeted principal's own charge declaration.

### 7. The control pipeline and normative vector contract are not complete

#### 7.1 Recovery precedence is applied before body integrity

The new control pipeline resolves the signer and verifies the signature over
the header, then performs placement and says C3′ validity is established
before the precedence exception. Only afterward does it verify `body_hash`,
CDDL, and body invariants. Yet C3′ validity depends on body fields such as
`base`, `epoch`, and `repoch`. A validly signed header paired with a malformed
or hash-mismatched body must never suppress a competing control operation.
([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1378),
[C3′](/Users/vm/owner-plane-d0a-spec.md:1048))

Split the pipeline explicitly:

1. parse and canonicalize;
2. resolve arm and verify the header signature;
3. verify `body_hash`, operation registry arm, and body CDDL;
4. validate every C3′ field needed for precedence;
5. apply placement/precedence;
6. evaluate remaining state-dependent body invariants and transition.

Pin the exact outcome at each boundary, including arm-resolution failures and
body-hash failure.

#### 7.2 The normative companion case schema is absent

Section 13 delegates every closed `case_kind`, exact input shape, and exact
result shape to `d0a-vector-cases.v1.json`; Gate A requires that file to exist
before every fixture. A filesystem audit found no file with that name under
`/Users/vm`. The container schema is deliberately generic, so it cannot stand
in for the missing contract.
([vector contract](/Users/vm/owner-plane-d0a-spec.md:1821),
[Gate A checklist](/Users/vm/owner-plane-d0a-spec.md:2107))

This is not editorial debt: D-91 says the typed contract is realized by that
normative artifact. Author and discrepancy-audit the companion before the
corpus. Until then the spec has an unresolved normative reference and Gate A
is mechanically false. The still-open family-14 offline-confirmation result
also remains a gate item.

## Focused v0.5.5 closure plan

I would keep the next pass deliberately narrow and make the following
decisions in this order:

1. **One cutoff algebra.** Define a total coordinate order, exact affected
   live-head set, missing-head pendency, non-expanding authority boundary,
   and a bounded prepare/commit mechanism. Reuse it for renewal, revocation,
   epoch closure, recovery, and D-92 composition.
2. **One effect-finality rule.** Say when an operation selected by canonical
   budget order becomes irreversible; prohibit export/audit release before
   that point. Define whether displaced non-effectful operations can revive.
3. **Complete signed requester state.** Carry full live-head commitments,
   decide whether reauth binds generation consumption, and make registry/CDDL
   signing formulas byte-identical.
4. **Make checkpoint paging a state machine.** Add retirement cursors,
   monotone page keys, witness bounds, and a joint encoded-size limit.
5. **Close rotation in portable bytes.** Commit the control frontier and
   recipient set; remove local storage state from control admission; bound or
   stage KEM-renewal wraps.
6. **Make transfer recovery total.** Pin terminal precedence and completed
   replay-key priority; replace self-attested bundle charge with verifiable
   evidence or a deterministic surcharge.
7. **Repair control precedence, then write the companion schema.** Only after
   the prose transition order is settled should case kinds and expected
   results be frozen.

## Minimum counterexample vectors to add

Before declaring Gate A, I would require at least these new cases in addition
to the existing family matrix:

- strict epoch advance with 65 live lineages;
- control cutoff arriving before its referenced tenant Head;
- two incomparable generation heads plus repeated cutoffs;
- H5 cutoff, delayed H6, then H10 cutoff—fresh and incremental equality;
- budget displacement followed by retro-disqualification of the winner;
- an egress release held behind the effect-finality barrier;
- feed cutoff at 100 with only 1–50 present and a forged alternative 51;
- repeated feed cutoffs whose committed heads are not ancestors;
- hosted cutoff whose full multi-head set is missing, stale, or cross-lineage;
- two banked reauthorizations around generation-window consumption;
- maximum jointly encodable checkpoint and one-byte-over checkpoint;
- a retirement event omitted from the first page and carried later;
- a lower later fence/page attempting to regress effective coverage;
- a ZonePolicy with 65 witnesses;
- wrap-add acceptance on opposite sides of Fence, followed by crash/replay;
- KEM renewal with 129 accepted-unretired memberships;
- renewal during Fence→KekDestroyed and zero-history renewal;
- destination rejection versus source erasure in both observation orders;
- all imports committed, crash before XferDone, then source erasure;
- malicious signed `bundle_size = 0` against a nonempty bundle;
- malformed/hash-mismatched recovery body competing at a C2 position; and
- every fixture validating against both the container and the normative case
  schema.

## Final recommendation

V0.5.4 should be treated as a successful convergence pass, not a failed
freeze. It eliminates several foundational defects and leaves a much smaller
set of boundary problems. But those boundaries govern irreversible exports,
authority closure, erasure recovery, and recovery-key precedence—the places
where “eventually deterministic” is not enough.

**Cut v0.5.5 before freezing.** Once the seven closure items above are in the
prose and CDDL, author `d0a-vector-cases.v1.json` first, build the corpus and
harness against it, run the family-14 fixture, and then conduct the final
prose↔schema↔vector discrepancy audit. If that audit is clean, Gate A will be
credible rather than aspirational.
