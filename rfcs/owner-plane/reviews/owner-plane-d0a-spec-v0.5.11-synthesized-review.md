# Synthesized review: D0-A Core + Memory normative specification v0.5.11

*2026-07-13. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.11-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.11-review.md)
(SHA-256
`0d7ccb82d6334a213f000f814774aa4d87a54a881097b51a46fb238ce8191f2c`)
and
[owner-plane-d0a-spec-v0.5.11-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.11-review-2.md)
(SHA-256
`7524088f96378dd902de4a4c9041f78a8d74cc4e74734f6b9c3e66fb69c7ed03`),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.11,
SHA-256
`e304a5d9e8669a6fcfbdd2a73534d939b2bb5dff7ca838516281cef5d28815f1`.
This synthesis adjudicates the evidence rather than averaging the verdicts.*

## Executive verdict

**Cut v0.5.12. Do not move directly from the peer's M1 cleanup to the
artifact sequence.**

The reviews agree that this is a productive revision. D-135 fixes the
first-ratification identity bug and makes a snapshot total over generations
inside each named zone. D-138 replaces the unportable recovery “past base” rule
with a revivable override an independent replica can fold, adds the total C3′
re-fold rule, and supplies the missing space-retirement body. D-141 corrects
the false retention bound, makes cap existence derived, makes KEM custody
transitive, and extends signing-key freshness to enrollments. D-142 provides a
constructible `mimport` schema and the core mirror matrix. Those are substantive
closures, not wording improvements.

The peer's sole finding is valid and independently agrees with the first
review's exactness audit: `mclaim` still exposes an optional
`provenance.import` arm that every v1 operation using `mclaim` must reject.
Removing that dead arm is the correct D-142 consistency cleanup.

The peer's conclusion that every blocker and high is discharged
([peer verdict](/Users/vm/owner-plane-d0a-spec-v0.5.11-review-2.md:13)) does not
survive execution of the new rules together. The report explicitly claims
convergence and lifecycle testing, but it does not exercise these particular
cross-ruling counterexamples:

- D-136's scalar cannot both preserve several live generation heads and close
  every extension, and its authority-agnostic `immutable_cap` also blocks the
  legitimate successor authority.
- D-137 reserves one coordinate, not an unresolved fork-selection scope.
- D-139's “one active grant” does not create the single append-only order its
  proof assumes; one lineage with unknown generation gaps already suffices to
  produce a late canonically earlier claimant.
- D-140 adds one basis and one reopen record, but leaves multi-cause selection,
  abandonment policy, terminal reincarnation, and the published idempotency key
  unresolved.
- D-138's total re-fold conflicts with control-derived outcomes still labeled
  `reject-permanent (never re-enters)`.
- A committed partial import loses its portable flat-bundle proof when another
  source record is erased.

The key lifecycle also needs another recovery pass. Incorporation-cap
eligibility has no defined evaluation stratum; KEM public keys may be reused
across devices; and C3′ can remove a renewal after the old KEM secret has been
irreversibly destroyed. Finally, the fork-inclusive “ever enrolled” signing-key
set has no portable recovery commitment, so replicas can disagree about whether
a key is fresh.

This remains a repair round, not an architectural rejection. Preserve the Owner
Plane, Memory claim model, control/tenant separation, claims-not-facts posture,
and the good D-135/D-138/D-142 changes. Finish the closure, identity, proof,
terminal, and recovery contracts before freezing fixtures.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-135:** materially complete, subject only to rejecting the requester-only
  no-zone no-op.
- **D-136/D-137/D-139/D-140/D-141:** partial; retain their direction and repair
  the cross-rule failures below.
- **D-138:** portable override and total-re-fold direction accepted; complement
  domain and branch-relative lifecycle remain open.
- **D-142:** original constructibility defect fixed; adopt the peer's dead-arm
  cleanup and complete the vector mirrors.
- **Protocol/schema freeze:** no.
- **Gate A:** false independently because the companion and corpus are absent,
  and no green surface run or final discrepancy audit exists.

## Assessment of the peer review

### What it did well

The peer report is concise, source-grounded, and genuinely independent. It
chases the new bodies into CDDL rather than trusting decision-record prose, and
it correctly credits several repairs:

- the tagged D-135 first-event fold and total per-zone snapshot override;
- removal of dead `ratified_through` machinery and the no-effect ratify
  `"none"` arm;
- the portable D-138 recovery-override mechanism and new retirement wire;
- the narrow, constructible D-142 `mimport`; and
- the transitive KEM-custody and expanded signing-key predicates in D-141.

Its M1 is valid. `mclaim` structurally retains `provenance.import` while the
protocol says `m.propose` and `m.assert` must reject it and real imports use
`mimport` ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3635)). Delete the arm; this
is medium schema hygiene, not a freeze blocker by itself.

The peer is also right that Gate A is decided by the companion, corpus,
surfaces, and final discrepancy audit rather than reviewer confidence. The
mistake is recommending entry into that artifact sequence before resolving the
behavioral discrepancies those fixtures would have to choose between.

### Why its one-medium verdict fails

The report applies its “four-question floor” ruling by ruling, but several defects
exist only when adjacent rulings compose:

1. **Representation is not semantics.** A scalar close exists on the wire, but
   the lexicographic scalar cannot represent a closed multi-generation
   Frontier while preserving all accepted heads.
2. **One active grant is not one immutable order.** Unknown generation gaps
   create multiple live chains inside the very lineage D-139 calls singular;
   grant turnover and `c.enroll.grants[]` add separate exactness questions.
3. **A field is not a state machine.** `basis` and `XferReopen` exist, but the
   schema carries one cause, the journal admits multiple terminal intervals,
   and every terminal still shares one effect key.
4. **Total removal must reach dispositions.** A C3′ re-fold that removes a
   retirement cannot coexist with `scope-space` being permanently unable to
   re-enter.
5. **Forever-history needs portable state.** Declaring historical signing keys
   to survive C3′ does not tell a replica that never received the cut
   certificate which keys are no longer fresh.
6. **Transient validation is not portable proof.** A flat bundle that is never
   retained cannot validate one committed member after another member is
   cryptographically erased.

Thus the peer's positive observations should be retained, and its M1 adopted,
but its empty blocker/high ledger and instruction to proceed directly to
artifacts should be rejected.

## Adjudicated disposition ledger

| Decision or issue | Peer disposition | Synthesized disposition |
|---|---|---|
| D-135 tagged first-event fold and snapshot | Complete | **Materially fixed**; reject the unscoped requester-only no-op |
| D-136 scalar authority closure | Complete and exemplary | **Open — blocker:** cannot preserve and close a multi-head Frontier; cap lacks authority dimension |
| D-137 tenant-selector reservation | Complete | **Partial — blocker:** exact coordinate reserved, descendant selection scope open |
| D-138 recovery omission | Complete | **Partial:** portable mechanism fixed; complement domain unresolved |
| D-138 total C3′ re-fold | Complete | **Partial — blocker interaction:** dispositions and terminal lifecycles contradict it |
| D-138 space retirement | Complete twice over | **Partial:** body and epoch event fixed; backdate defense inherits D-136 |
| D-139 import identity | Centerpiece; complete | **Open — blocker:** active grant is not a lifetime single chain/order |
| D-140 terminal stability | Complete | **Partial — blocker:** basis selection, abandonment, and terminal incarnation unspecified |
| D-141 cap lifecycle | Complete | **Partial — blocker:** eligibility has no non-circular evaluation rule |
| D-141 KEM/signing-key closure | Complete | **Partial — blocker:** KEM reuse, recovery rollback, and portable key history open |
| D-142 `mimport` constructibility/mirrors | Complete | **Fixed for the original defect**; vector mirror residue remains |
| Dead `mclaim.provenance.import` arm | Medium M1 | **Agreed — medium** |
| Durable partial-import proof | Not raised | **New blocker** |
| Gate-A companion and corpus | Proceed after M1 | **Gate remains open on both protocol and artifacts** |

## Blocker 1: one scalar cannot both preserve and close a multi-head authority

D-136 says a scalar `zonecutoff` closes old-epoch or predecessor-certificate
authority, using the total lexicographic `(gen, seq)` comparator
([cutoff purposes](/Users/vm/owner-plane-d0a-spec.md:1292),
[promotion](/Users/vm/owner-plane-d0a-spec.md:1520)). But `w.gen("unknown")`
deliberately leaves earlier generation heads live
([generation rules](/Users/vm/owner-plane-d0a-spec.md:1820)). The
64-open-unknown-gap allowance makes this a normal protocol state, not malformed
input.

Concrete trace:

1. Lineage L has live `H1 = (g1, seq10)`.
2. L opens g2 with `H2 = w.gen(last_known = "unknown")` at `(g2, seq1)`;
   H1 stays live.
3. An epoch advance, renewal, or space retirement preserves both heads and
   carries the only scalar close that preserves both accepted heads, H2.
4. After the close, the old authority appends `(g1, seq11)`.
5. Lexicographically `(g1, 11) < (g2, 1)`, so the supposedly closed write is
   still at or below H2 and admits.

Choosing H1 closes the authority only by discarding already accepted H2; it
cannot satisfy both preservation and closure. Duplicate `(zone, lineage)`
entries are non-canonical, so the wire cannot carry both per-generation bounds
([`zonecutoff` CDDL](/Users/vm/owner-plane-d0a-spec.md:3395)). A total order
between coordinates does not make one coordinate an upper bound on every
future causal extension of several independent chains.

The repair must carry an authority-qualified per-generation closure plus a
future-generation ceiling. A frozen Frontier, or an encoding with equivalent
information, has:

- one terminal bound for every live generation of the authority being closed;
- a generation-opening ceiling, so that authority cannot escape through a new
  generation; and
- the authority selector being ended: old capability epoch, predecessor
  certificate, or revoked grant/certificate. Named recovery boundaries may
  intentionally terminate a lineage globally and need a separate continuation
  ruling.

The unknown-gap cap makes the legal per-lineage closure information bounded; an
encoding still has to commit every live generation head. Wide-zone staging can
be paged, but it needs a final commitment that names the consumer and freezes
the complete set.

This counterexample also keeps space retirement open. Its epoch event and body
now exist ([registry](/Users/vm/owner-plane-d0a-spec.md:1278),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3359)), but the g1 append above is a
valid old-epoch backdate after retirement.

### The same close is over-broad in the other direction

The effective-state equation is keyed only by `(zone, lineage, gen)` and puts
materialized close/supersede values into its undimensioned `immutable_cap`
([equation](/Users/vm/owner-plane-d0a-spec.md:1420)). Effective admission then
applies that cap to every operation, with no certificate, grant, epoch, or
space selector.

That contradicts the purpose-specific rules:

- supersede ends the predecessor certificate's authorship
  ([§7.1](/Users/vm/owner-plane-d0a-spec.md:1315));
- revoke ends a named cert, grant, or key
  ([§7.1](/Users/vm/owner-plane-d0a-spec.md:1320)); and
- close ends old-epoch writing
  ([§7.1](/Users/vm/owner-plane-d0a-spec.md:1329)).

As written, renewing C0→C1 at H also rejects C1's legitimate H+1 write. An
epoch-e close at H rejects an epoch-(e+1) H+1 write to an unaffected space. A
space retirement deliberately closes old-epoch writing zone-wide, but this
undimensioned cap also freezes new-epoch continuation throughout that zone.
Conversely, if implementations silently apply the cap only to stale authority,
the schema and equation do not say how to select that authority, and old staged
closes can incorrectly satisfy later advances.

Separate generation-history caps (abandon/incorporation) from
authority-qualified closures. The effective predicate for an operation should
take the minimum only over closure records whose authority selector matches the
operation's signed certificate/grant/epoch context.

## Blocker 2: incorporation-cap eligibility has no defined evaluation point

`cap_eligible` requires `w.gen.last_known` to be a terminal Head, while the cap
created by that same `w.gen` decides whether a successor past that Head can
admit ([eligibility](/Users/vm/owner-plane-d0a-spec.md:1360),
[dissolution](/Users/vm/owner-plane-d0a-spec.md:1379)). Accepting `w.gen` also
retires the named old-generation Head
([generation rule](/Users/vm/owner-plane-d0a-spec.md:1820)). The RFC never pins
whether eligibility is evaluated against the pre-operation state, post-operation
state, all held bytes, or a cap-clamped state.

Let W be `w.gen(last_known = H5)` and let H6 be a delayed valid successor in
H5's generation:

- In W's pre-state, H5 is terminal, so W can become eligible, retire H5, and cap
  H6.
- In an all-held canonical evaluation that admits H6 first, H5 is no longer
  terminal and W's body is invalid.
- Evaluating H6 against W's cap while using H6's rejection to establish W's
  terminality premise is circular.

A pre-W anchored interpretation chooses the first result; an all-held
interpretation chooses the second. Naive incremental implementations can
therefore diverge by arrival order, and the RFC selects neither rule.

Define cap eligibility from a cap-independent anchored prefix predicate and
state which signed assertion wins. If `last_known` is intentionally a writer's
truncation assertion, say so and validate H5's chain membership without asking
whether the cap has already made it terminal. If later same-generation
successors should win, W cannot confer incorporation finality. Add W-first,
H6-first, and all-bytes fresh-fold vectors; the existing displacer and
tenant-fork vectors do not exercise this cycle.

## Blocker 3: D-137 reserves a coordinate, not the fork-selection scope

Issuer commitments correctly reserve their entire issuer scope while ancestry
evidence is missing ([T3](/Users/vm/owner-plane-d0a-spec.md:785)). Tenant
boundaries reserve only the exact `(gen, seq)` coordinate
([Head lifecycle](/Users/vm/owner-plane-d0a-spec.md:1491)).

Counterexample:

1. Earlier control operation B1 names absent A5 at `(Z,L,g,5)` and pends.
2. The replica holds the conflicting branch B5→B6.
3. Later B2 names B6. It does not name coordinate 5, so the exact-coordinate
   reservation does not stop it; B2 commits a boundary on B's suffix.
4. A5 arrives. A fresh control-order fold processes B1 first, but the RFC has
   neither an ancestor-compatibility rule nor a rule for revisiting B2's earlier
   boundary on the opposite suffix.

The required vector only describes opposite variants at one coordinate
([family 7](/Users/vm/owner-plane-d0a-spec.md:2637)). Reserve at least the full
`(zone, lineage, gen)` selection scope while a Head path is unresolved, and
require every later selector in that scope to prove ancestry compatibility or
pend behind it. Missing ancestry means a narrower safe cone cannot always be
known yet.

## Blocker 4: D-139 does not make imported claim identity final

The proof starts from “one active import grant per destination zone”
([control row](/Users/vm/owner-plane-d0a-spec.md:1281)) and concludes that every
import rides one lineage, so the first `(gen, seq)` claimant holds the replay
key forever ([import rule](/Users/vm/owner-plane-d0a-spec.md:2365)). Neither
step follows.

First, the executable invariant is attached to `c.grant`; `c.enroll`
independently carries `grants[]` and does not state the same plane-wide check
([enrollment CDDL](/Users/vm/owner-plane-d0a-spec.md:3264)). Second, “one active
at a time” is not “one lineage for the lifetime of the zone.” Revoke L1's
grant, issue the successor to L2, and the zone now has held claimants for the
same replay key on incomparable chains. C3′ creates the same transition by
cutting a grant and re-issuing authority on the surviving branch.

D-139's decision text can be read as a global invariant that was meant to cover
enrollment too; if so, the registry/CDDL enforcement is incomplete rather than
an intentional bypass. The sequential-lineage rule still disproves the literal
“one lineage” premise, although a correctly resolved revoke cutoff could
serialize old authority before handoff. The RFC states neither that handoff
invariant nor how historical replay reservations compose across it.

Even one lineage is not a single append-only order while unknown generation
heads remain live: an import in g2 can be followed by a canonically earlier
claimant in still-live g1. Effect finality prevents that race from escaping as
an effect-final import, but provisional collisions and references before that
point are not specified as mutable.

One stable repair is a logical imported-claim identity derived from
`(from_plane, release_op, source_op)`, with the emitting operation hash retained
as carrier/audit provenance. If op-hash identity must remain, options include a
lifetime designated import feed with no unknown-gap generations, or explicitly
provisional replay ownership that cannot be referenced or projected until
effect finality freezes it; either option also needs a control-ordered
grant-handoff rule enforced on every grant-bearing operation. The current
active-grant rule alone specifies neither design.

## Blocker 5: committed partial imports lose proof after source erasure

The signed release commits to the hash of one flat bundle. The destination
validates each imported record against the received whole bundle, but bundles
are explicitly never persisted
([bundle rules](/Users/vm/owner-plane-d0a-spec.md:2260),
[non-persistence](/Users/vm/owner-plane-d0a-spec.md:2286)). `mimport` carries the
copied record and the flat digest, but no inclusion proof
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3672)).

Trace:

1. Release R contains records A and B.
2. A is imported and committed.
3. R is effect-final, B has no unresolved durable destination attempt, and B
   is erased before it is imported; recovery writes the source-erased abort.
4. The protocol says committed A stands even though the flat bundle is now
   underivable ([journal rule](/Users/vm/owner-plane-d0a-spec.md:1147)).
5. On restart, index rebuild, or a later replica, A alone cannot reproduce
   `H_bundle({A,B})`. The erased B cannot be decrypted. Portable bytes therefore
   cannot prove that A's statement/kind/floor was the record committed by R.

Remembering that validation once succeeded is precisely the acceptance-history
dependency the rest of this RFC works hard to eliminate. It also leaves an
undefined recovery/liveness state: admission checks time before body invariants
([pipeline](/Users/vm/owner-plane-d0a-spec.md:1927)), while a durable
receipt-pending import makes source-erased abort defer
([journal rule](/Users/vm/owner-plane-d0a-spec.md:1135)). If source content is
erased before any whole-bundle validation artifact becomes durable, later
receipt arrival reaches a body check whose required bundle cannot be
reconstructed. An edge could eagerly check the transient bundle, but the RFC
defines no portable artifact by which restart can rely on that check.

Use a durable per-record proof. A natural shape is a Merkle commitment over
canonical `bundlerec` leaves, binding `export_id`, record count, and ordered
source set at the root, with the inclusion path carried by `mimport`.
Alternatively persist an authenticated manifest as permanent validation
material. Keeping it only until the first terminal is insufficient: committed
partial imports must survive local rebuild. A later distribution gate could add
such a manifest, but D0-A's frozen `mimport` and “never persisted” rule provide
none today.

## Blocker 6: D-140 is not yet a reopenable terminal state machine

### One basis does not define the multi-record abort policy

A destination-rejection abort, after R is effect-final and no unresolved
attempt remains, records every resolved-negative source operation in `missing`
([recovery case 3](/Users/vm/owner-plane-d0a-spec.md:1148)), but
`XferAbort` carries only one optional `basis`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3591)). Two records can be negative
because of two independent removable seals or caps:

1. I1 is negative under B1; I2 is negative under B2.
2. Abort records `missing = [I1,I2]` and only B1 as its basis.
3. B2 dissolves while B1 remains.
4. I2 is now completable, but B1 remains a sufficient reason that the transfer
   cannot reach `XferDone`. One interpretation intentionally abandons I2;
   per-record re-derivation reopens it. The scalar `basis` does not choose.

Either carry per-record causes or define one canonical sufficient witness plus
explicit irreversible-abandonment semantics for all other negative records.
The current scalar has neither a selection rule nor those semantics. If one
terminal-stable negative is intended to abandon every other currently negative
record, say so directly; if each newly completable record should reopen, carry
its causes. The present text leaves those policies observationally different
but both plausible. Distinguish intrinsic rejection from boundary-derived
rejection and define exactly which predicate changes re-evaluate the effective
terminal.

The prose currently overloads that distinction. It says a boundary-derived
permanent quarantine carries a basis, then says “reject-permanent terminals”
carry no basis
([basis prose](/Users/vm/owner-plane-d0a-spec.md:1180)); yet terminal reason
`"reject-permanent"` includes both intrinsic rejection and boundary-derived
permanent quarantine. Presence of a basis could distinguish them, but the RFC
must say so and define a deterministic witness when several terminal causes
exist.

### Reopen reuses an already-consumed effect key

Every source terminal append is idempotent under
`("terminal", release_op)` ([effect keys](/Users/vm/owner-plane-d0a-spec.md:448)).
D-140 then permits `Abort → XferReopen → Done` or a second Abort for the same
release. A conventional consume-once deduper suppresses the new terminal
because its key was consumed by the first Abort. The RFC could instead define a
state-reconciling or explicitly re-armed idempotency contract, but does not.
`XferReopen` carries no terminal identity or incarnation
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3608)).

The retained “one terminal per release” statements likewise contradict D-140
([§11.8](/Users/vm/owner-plane-d0a-spec.md:2396),
[D-53](/Users/vm/owner-plane-d0a-spec.md:2924)). Define the journal as a
transition fold with one effective terminal per open interval. Make Reopen name
the exact Abort it supersedes, and key terminal effects by a deterministic
incarnation or transition identity.

## Blocker 7: total C3′ re-fold and reject-permanent cannot both hold

D-138 requires all control-derived admission state to be recomputed from the
surviving chain ([T2](/Users/vm/owner-plane-d0a-spec.md:704),
[C3′](/Users/vm/owner-plane-d0a-spec.md:1592)). The required vector explicitly
includes a cut retirement reopening its space
([family 8](/Users/vm/owner-plane-d0a-spec.md:2664)). But a held or durably
recorded post-retirement operation receives `scope-space` in the fold, and every
`scope-*` outcome is
`reject-permanent (never re-enters)`
([disposition map](/Users/vm/owner-plane-d0a-spec.md:2028)). D-140 then calls a
reject-permanent transfer terminal basis-free and stable.

Those instructions yield different fresh and incremental lifecycles. If C3′
cuts the retirement, the surviving control state no longer contains the fact
that produced `scope-space`; the total re-fold must reconsider the operation.
It may first become `epoch-unopened` if retirement was the only event opening
its signed epoch, and admit only after the surviving branch opens that epoch;
either way, “never re-enters” forbids the required re-evaluation. A transfer
that terminalized the original rejection has no basis with which to track the
change.

Split permanent outcomes into at least:

- intrinsic held-byte failures, whose cause cannot disappear; and
- control-branch-relative failures, whose cause is a named removable control
  operation and which participate in C3′ re-evaluation.

The latter need typed bases in terminal causes. Apply the same audit to
`cert-revoked`, scope failures, and other outcomes whose truth can depend on a
control operation removed by recovery.

## Blocker 8: key freshness and custody are not recovery-stable

### KEM public-key reuse bypasses device exclusion

V0.5.11 correctly requires every non-genesis certificate to use a signing key
never before enrolled ([T3](/Users/vm/owner-plane-d0a-spec.md:803)). There is no
equivalent KEM public-key rule. Certificates accept a P-256 KEM key, and a wrap
is encrypted to that key while merely labeling the intended device
([certificate](/Users/vm/owner-plane-d0a-spec.md:324),
[KekWrap](/Users/vm/owner-plane-d0a-spec.md:859)).

If compromised device D1's KEM key K is reused in a valid D2 certificate and D2
is subsequently wrapped into a zone from which D1 was excluded, those D2/K
wraps are decryptable with D1's old private key. D1 therefore regains
confidentiality access despite exclusion. K0→K1→K0 on one device similarly
resurrects a known-compromised secret.

Require plane-wide KEM-key uniqueness across device identities and prohibit
reuse after a same-device KEM rotation. An unchanged KEM key on an ordinary
same-device renewal can remain explicitly legal.

### C3′ can roll back a renewal after irreversible key deletion

The local custody rule permits deletion of predecessor KEM secret K0 after all
relevant wraps target a current descendant key
([renewal rule](/Users/vm/owner-plane-d0a-spec.md:1271),
[CDDL commentary](/Users/vm/owner-plane-d0a-spec.md:3310)). Now take a C3′ base
before that renewal—an explicit but valid owner recovery choice. The total
re-fold removes C1/K1 and its replacement wraps, making C0/K0 effective again,
although K0 is gone. Recovery adoption preserves adopted KEK rotations and
their effective wrap maps, but has no corresponding adoption record for
certificate/KEM renewal state
([storage adoption](/Users/vm/owner-plane-d0a-spec.md:1612)). A solo plane can
therefore strand readable storage without any intervening KEK rotation.

Recovery must adopt every irreversible recipient-key transition on which local
ciphertext readability depends, or the client must retain old KEM secrets until
that transition is outside every recoverable branch—an unbounded condition.
The former is the practical design: extend recovery's storage-adoption
commitment to KEM-renewal recipient state.

### “Ever enrolled” signing-key history has no portable commitment

The signing-key rule includes certificates on cut branches and says historical
keys survive C3′ and compaction ([T3](/Users/vm/owner-plane-d0a-spec.md:806)).
But the normative C3′ fresh-fold input is the surviving chain, and
`c.recovery_succession` carries no commitment to the fork-inclusive historical
key set ([recovery body](/Users/vm/owner-plane-d0a-spec.md:1571),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3497)).

Replica A may have seen a soon-cut certificate using signing key K while replica
B never received it. Unless every replica is required to receive and retain
every cut-branch certificate—a requirement the RFC does not state—enrollment
after recovery can reuse K: A rejects and B accepts. Later arrival of the cut
certificate has no revisit rule for the already accepted enrollment. Define an
explicit fork-inclusive historical-key registry, commit/adopt its root in
recovery and compaction, and specify how late evidence affects a pending or
accepted enrollment.

## High ambiguity: renewal coverage names active, not historical, authorship

D-141 correctly moves history coverage away from KEM membership, but defines
its domain as zones in the predecessor device's active op-authoring grants
([registry](/Users/vm/owner-plane-d0a-spec.md:1271),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3278)). That is not obviously the
predecessor certificate's historical authorship domain.

Suppose C0 authored O in Z under grant Gz, then Gz was revoked with cutoff O.
The revocation expressly preserves O, but Gz is inactive when C0 renews. Z can
therefore be omitted from `history_cutoffs`. Section 4.2 says an old
certificate's operations remain valid at or before its *renewal history
cutoffs* ([certificate renewal](/Users/vm/owner-plane-d0a-spec.md:339)); it does
not say a grant-revoke cutoff also keeps C0 valid after supersession.

The likely intended answer is that Gz's immutable revoke boundary also
preserves C0's prefix. If so, define an explicit composition rule showing that
Gz's pre-cutoff operations remain certificate-valid after supersession without
widening other grant or zone scopes, and prove it. Otherwise renewal must cover
the historical scope explicitly. Do not leave the choice to an implementer's
interpretation of “coverage.” Add both last-grant-revoked and
one-of-two-same-zone-grants-revoked renewal vectors.

## High ambiguity: the recovery omission domain is not keyed exactly

Recovery entries are zone-qualified, while one lineage can participate in more
than one zone. The prose nevertheless says that an omitted “lineage” receives an
implicit revivable `"none"` override
([recovery prose](/Users/vm/owner-plane-d0a-spec.md:1592)). If L has an entry for
Z1 but none for Z2, a bare-lineage reading and a `(zone, lineage)` reading yield
different surviving state.

Define the complement's exact universe: pairs at `base`, pairs in the surviving
pre-operation state, or all currently held pairs including cut-branch state.
Separately state how pairs created after recovery are treated and whether every
current and future generation of each omitted existing pair inherits the
implicit override. This is fold-affecting domain semantics, not merely
editorial precision.

## Agreed medium: remove the dead `mclaim.provenance.import` arm

This is the peer report's sole finding
([peer M1](/Users/vm/owner-plane-d0a-spec-v0.5.11-review-2.md:68)), independently
corroborated by the first review, and should be adopted. The `mclaim` CDDL still
permits optional `provenance.import`, while D-142 says every
`m.propose` or `m.assert` carrying it rejects and v1 imports use the dedicated
`mimport` body ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3635)). The optional arm
therefore describes no valid `mclaim` operation.

Delete it structurally. Every valid propose/assert body remains constructible,
and the special-case rejection becomes unnecessary under closed dispatch. This
is exactly the schema-says-more-than-the-protocol drift the final discrepancy
audit should remove, but it does not cure the independent import-identity or
portable-proof blockers.

## Schema and exactness repairs

1. **Define “live lineage.”** Strict advances and retirement require complete
   coverage of every live lineage, but the term does not decide whether a
   lineage with revoked grants, no current wrap, or only historical heads is
   live ([closure rule](/Users/vm/owner-plane-d0a-spec.md:1509)). This changes
   control-operation acceptance.
2. **Reject the requester-only no-op.** `ccutoff` permits `cutoffs=[]`, a
   requester, and `live_heads=[]` ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3430)).
   Require at least one `zoneheads` entry; `heads=[]` inside one named zone
   remains the valid total-none snapshot.
3. **Complete the D-142 mirror battery.** Its family-11 clause currently names
   only `from_plane`, digest, destination-zone, and content-digest mismatches
   ([family 11](/Users/vm/owner-plane-d0a-spec.md:2794)). Add independent
   negatives for `export_id`, derived-versus-signed `release_op`, the three-way
   `pendingxfer.record_count == |sources| == |bundle.recs|` equality, endpoint
   header coordinates, and `m.propose`/`m.assert` import provenance.

## Gate A is mechanically open

Section 15 correctly calls `d0a-vector-cases.v1.json` artifact-pending
([open items](/Users/vm/owner-plane-d0a-spec.md:3015)). Section 16 uses an “all
true” Gate-A checklist whose predicates include a companion and green vector
families ([checklist](/Users/vm/owner-plane-d0a-spec.md:3023)). Whether that
heading is intended as a status claim or merely a predicate heading, Gate A is
presently false: no companion is present under `~/`, and there is no corpus to
run. Make the gate's current status explicit and leave it pending until those
artifacts exist.

The peer recommends deleting M1 and then entering the artifact sequence
([recommendation](/Users/vm/owner-plane-d0a-spec-v0.5.11-review-2.md:105)).
Reverse that dependency for normative artifacts: the Gate-A checklist itself
requires behavior discovered in fixtures to fold back into this document first.
Draft the counterexamples below alongside v0.5.12, but do not make the companion
or corpus choose among the unresolved closure, identity, and terminal semantics.

Required new traces, in addition to the prescribed matrix:

- lower-live-generation append after a higher-generation scalar close;
- new-epoch and renewed-cert writes immediately beyond their predecessor
  close;
- W/H6 cap eligibility in both arrival orders and a fresh fold;
- pending ancestor selector versus later opposite-branch descendant;
- import grant revocation/reissue to another lineage and its C3′ equivalent;
- partial import, source erasure, index deletion, and fresh rebuild;
- durable receipt-pending import followed by source erasure;
- two missing imports with independent bases, removed in both orders;
- Abort→Reopen→Done through the real idempotency store;
- retirement rejection followed by C3′ removal;
- KEM key reuse across devices and K0→K1→K0;
- C3′ before a completed renewal after K0 destruction; and
- cut-branch historical signing-key delivery before versus after reuse.

## What should remain unchanged

- The two-authority frame and the rule that storage contents never acquire
  execution authority.
- Signed operations as claims rather than facts, with conflicts retained.
- Separate control and tenant logs, transition-last control validation, and
  portable held-byte folds.
- Per-item encryption, retrieval exclusion before cryptographic erasure, and
  the explicit zero-daemon durability floor.
- D-127's acyclic release construction.
- D-135's tagged boundary fold and total requester snapshot.
- D-138's portable recovery-override mechanism, subject to pinning its exact
  `(zone, lineage)` complement domain, and its total surviving-chain re-fold.
- D-142's narrow import body and signed-mirror equalities.
- The decision to treat the deadline/lease exemption for incorporation caps as
  an explicit owner-accepted residual.

## Recommended v0.5.12 sequence

1. Replace scalar cert/grant/epoch closure with authority-qualified
   per-generation bounds plus a future-generation ceiling; update epoch
   advance, renewal, revocation, and space retirement together. Adjudicate
   recovery separately: pin whether its named boundaries terminate a lineage
   globally or permit a defined continuation.
2. Define incorporation-cap eligibility with a cap-independent anchored
   evaluation rule.
3. Generalize pending tenant selector reservation to the unresolved selection
   scope.
4. Choose stable imported-claim identity—logical replay-key identity preferred,
   or explicitly provisional op-hash ownership until finality—and add durable
   per-record release proofs.
5. Define the reopenable transfer-journal fold, canonical basis selection or
   per-record causes, explicit abandonment semantics, and incarnation-safe
   effect keys.
6. Reconcile C3′ with branch-relative outcome lifecycles.
7. Add KEM uniqueness plus recovery adoption for KEM renewals and a portable
   fork-inclusive historical-key registry.
8. Close the renewal historical-authorship and recovery-domain ambiguities;
   delete the peer's dead `mclaim` arm and finish the exactness definitions.
9. Update CDDL, prose mirrors, decision rows, and the required vector list in
   one pass.
10. Author the companion and corpus, run every required surface, and only then
    mark Gate A true.

## Bottom line

V0.5.11 is a strong repair draft, especially around D-135, D-138, and D-142.
The peer review accurately validates those improvements and contributes the
dead-arm cleanup, but its “all discharged” conclusion is rejected. This is not a
candidate baseline yet. The remaining counterexamples can cause
old authority to survive a close, new authority to be frozen by that same
close, replicas to choose different fork/import identities, committed imports
to lose their proof, transfer replay/reopen outcomes to become underdetermined,
successor terminals to be suppressed, and recovery to resurrect deleted key
dependencies. Those are exactly the classes Gate A is meant to eliminate. Cut
v0.5.12, preserve the architecture, and finish the protocol state machines
before producing fixtures.
