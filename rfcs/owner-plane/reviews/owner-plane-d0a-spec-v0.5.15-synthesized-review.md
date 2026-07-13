# Synthesized review: D0-A Core + Memory normative specification v0.5.15

*2026-07-13. Adjudicated synthesis of
[*Review: D0-A Core + Memory normative specification v0.5.15*](/Users/vm/owner-plane-d0a-spec-v0.5.15-review.md)
(SHA-256
`6fd32e460dc2310d4c062049dad071ed2201c6fc75e0997e02112829753765a3`)
and
[*Review 2: D0-A Core + Memory specification v0.5.15*](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md)
(SHA-256
`6405f5eb6d603eed13574c7c47e7f77d07dc4a7038a9031bdec5aea7163bd249`),
verified against
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.15,
4,566 lines / 50,034 words / 378,085 bytes (SHA-256
`94299df5e470bdd7878ef9866fbaa3e2f7cc67031bd5160d0ed3061dfe648e73`).
The v0.5.14 synthesis used as the incoming ledger has SHA-256
`960f40f5fc44439c659d5a29d32c541c96cde1dcf8afef74476214515d9fa3ba`.
This document adjudicates the two reports against normative bytes and
composed traces; it does not average their verdicts.*

## Executive verdict

**Cut v0.5.16. Do not freeze v0.5.15 or begin the normative companion and
independent reducer from this text.**

The peer report is valuable and accurate as a local insertion audit. It
confirms that the intended D-168 through D-175 repairs appear on many of the
expected prose, decision, CDDL, and vector-inventory surfaces. It also says
explicitly that composed depth was limited and leaves the freeze judgment to
this synthesis
([scope](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:3)). That caveat
matters: the report's broader claims that all blockers are discharged and
that the additions are “all present and consistent”
([verdict](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:12);
[finding](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:54)) do not
survive cross-rule replay or, in several cases, a check of the closed durable
shapes.

The combined disposition is:

- **Seven hard freeze blockers remain:** D-168 staged-consumer selection;
  D-169 collision-terminal representation; D-170 durable source-binding
  carriage; D-171 cross-feed journal reconstruction; D-174 certificate/grant
  compatibility; D-175 recovery-portable material identity; and D-175
  Frontier retirement.
- **D-173 is high normative/schema exactness, not a demonstrated reducer or
  constructibility failure.** The peer is right that the shrinking
  decryptable-wrap state machine can be deterministic and constructible at
  the portable control layer. The remaining defect is that accepted
  exclusions remove themselves before `rotation_refs` are asked to prove
  coverage, leaving the references and continuation story non-operative or
  contradictory.
- **D-172's original adoption-list blocker is substantially closed.** Keep
  all-chain membership, terminal overlap rejection, and the >64 residual.
  Its remaining cross-role overlap issue belongs to D-175, plus one CDDL
  wording cleanup.
- **E10 and canonical-shape closure remain false.** Several new pending,
  terminal, incompatibility, and continuation states still have no exact
  outcome/disposition or bounded tagged representation.
- **Gate A remains false**, as the source itself records
  ([status](/Users/vm/owner-plane-d0a-spec.md:3616)).
- **The artifact sequence does not begin normatively.** Counterexample
  fixtures may be drafted as non-normative design aids, but the companion and
  independent core must not choose protocol law that v0.5.15 leaves open.

This is not a disagreement about polish. The unresolved rules determine
which control operation consumes a staged closure, whether an erased import
has durable validation evidence, which certificate may author under an
inherited grant, and which accepted head remains in a Frontier. Different
literal implementations can reach different accepted state from the same
portable bytes.

## Assessment of the peer review

### What it establishes well

The peer report is concise, independent, and unusually clear about its
method. It correctly confirms that v0.5.15 now contains these local repairs:

- D-168 makes retirement strictness-independent and expressly mandates a
  suffix re-fold after late carrier acceptance.
- D-169 extends freeze reservation to revivably quarantined earlier
  claimants and expresses the intended collision cause as winner plus freeze
  basis.
- D-170 chooses source-derived equality over making the export signer's leaf
  authoritative.
- D-171 physically adds `XferAbort.at`, expands the cause map, and aligns the
  minimal-hash wording on several surfaces.
- D-172 requires signing-only intermediate renewals, rejects the stated
  retired/terminal overlap, and names deep-chain orphaning.
- D-173 publishes a historical decryptable-wrap domain, its evaluation
  position, shrink-on-exclusion rule, and `wrap_add` re-entry.
- D-174 puts recovery into the main closure equation and blocks the motivating
  old-C0/new-G1 signer-resurrection trace.
- D-175 adds `mat_id`, rejects direct same-point cross-role reuse, and gives
  §9.3 the intended accepted-predecessor retirement transition.

Those are real advances. The peer is also correct that the 0-author/65-wrap
ceremony is constructible at the portable control layer under a shrinking
historical domain; the first review's severity on D-173 was too high and is
corrected here.

### Why “all closed” does not follow

The two reports used complementary methods. The peer asked whether each new
ruling appeared where expected. The first review asked whether the resulting
rules form one replayable machine. Six distinctions account for the verdict
gap:

1. **A prose-stored value is not a durable byte.** D-170 says an attempt
   stores a binding, but no frame, transaction record, or authenticated
   operation carries it.
2. **A control coordinate is not a cross-feed snapshot.** `XferAbort.at`
   cannot order tenant operations or issuer-feed facts that carry no
   authoritative control position.
3. **A predicate is not necessarily one hash.** Effect finality may be
   structural or supported by several boundaries, while D-169 assumes one
   `freeze_basis` member.
4. **A locally useful relation can have the wrong direction.** D-174 blocks
   the motivating old certificate, but also blocks the current renewed
   certificate using its inherited grant.
5. **Role-neutral comparison needs role-neutral evidence.** D-175 works when
   the SEC1 point is held; recovery carries only an opaque role-tagged hash.
6. **A corrected transition does not repeal a contradictory core rule.**
   §9.3 now retires the accepted predecessor, while §4.6 still removes the
   exact named terminal head.

The peer's proposed artifact tranche is therefore a useful list of future
tests, not evidence that those tests can yet be instantiated without making
design choices. Its own recommendation anticipates that composition is the
synthesis's job
([recommendation](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:61)).

## Adjudicated disposition ledger

| Topic | Peer conclusion | Synthesized disposition |
|---|---|---|
| D-168 staged frontiers | Discharged | **Hard reducer blocker.** Renewal is absent from pending consumers, while generic next-renewal consumption can burn another lineage's stage under the wrong selector |
| D-169 import collision | Discharged | **Hard journal/schema blocker.** Reservation is repaired; immediate and multi-boundary finality do not fit one `freeze_basis`, and the collision-Abort path may be unreachable |
| D-170 source binding | Discharged | **Hard security/portability blocker.** The intended stored binding has no durable authenticated carrier |
| D-171 journal coordinate | Discharged | **Hard reconstruction blocker.** A control hash cannot delimit tenant/proof facts; fact references, terminal-first handling, and causes remain incomplete |
| D-172 adoption | Discharged | **Mostly closed.** Retain the repair; clean up the KEM-only introduction and resolve cross-role overlap through D-175 |
| D-173 revocation | Discharged | **High exactness.** The shrinking-domain machine is deterministic and constructible at the portable control layer, but `rotation_refs` no longer discharge the coverage they are said to prove |
| D-174 authority | Discharged | **Hard functional/lifecycle blocker.** The compatibility direction orphans inherited grants; §4.2 and the failure lifecycle still disagree |
| D-175 material identity | Discharged | **Hard portable-reducer/security blocker.** Recovery has only role-tagged hashes and no mandated alternate-role match |
| D-175 Frontier | Discharged | **Hard normative contradiction.** §4.6 retains exact named-head retirement beside §9.3's accepted-predecessor transition |
| Artifact readiness | Begin if synthesis concurs | **No concurrence.** Cut v0.5.16 first; only non-normative counterexample drafting may proceed |

## Detailed adjudication

### A1. D-168 repairs the late re-fold but omits an applicable consumer relation

The peer correctly identifies two landed repairs: any space retirement now
waits regardless of strictness, and late acceptance mandates the bounded
suffix re-fold
([peer D-168](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:13);
[stage machine](/Users/vm/owner-plane-d0a-spec.md:1749)).

The enumeration nevertheless excludes renewal. The generic rule says the
zone's next epoch advance, **renewal**, or retirement consumes every
unconsumed stage and materializes it under that consumer's selector
([consumption](/Users/vm/owner-plane-d0a-spec.md:1731)). The pending-carrier
rule and CDDL enumerate only strict advances and all retirements
([pending consumers](/Users/vm/owner-plane-d0a-spec.md:1749);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4065)). Yet renewal coverage is
mandatory over the renewed device's authorship domain, and the >64-zone case
depends on staged closes
([renewal registry](/Users/vm/owner-plane-d0a-spec.md:1395)).

This creates two failures:

1. A renewal R may reject permanently while an earlier carrier P that would
   complete its 65th closure is pending; after P accepts, a fresh fold accepts
   R but the incremental lifecycle has no exact revival rule.
2. With live lineages A and B in Z, the generic “next renewal” can let B's
   renewal consume A's stage and materialize it under B's predecessor-cert
   selector. The closure is inert or invalid for A, but the one-shot stage is
   burned before A's applicable boundary.

Repair: define the next **applicable** consumer from `(zone, lineage,
boundary purpose, consumer selector)`. Renewal consumes only stages within
its predecessor's authorship/selector domain; zone advance and retirement
consume the stages applicable to their own selectors. Every required-coverage
consumer, including renewal, waits behind an earlier held carrier that could
satisfy it. Name the pending outcome—most naturally `ref-unresolved`—and pin
unrelated-renewal plus late-resolution traces.

### A2. D-169 fixes ownership reservation, not the collision terminal

Keep the expanded reservation rule: pending and revivably quarantined
order-earlier claimants now prevent freeze
([ownership](/Users/vm/owner-plane-d0a-spec.md:2747)). The remaining defect is
primarily the journal representation, not the owner-selection reducer.

The cause map calls `(winner, freeze_basis)` a typed conjunction
([cause rule](/Users/vm/owner-plane-d0a-spec.md:1271)), but CDDL encodes an
untagged `[+ bytes32]`
([terminal CDDL](/Users/vm/owner-plane-d0a-spec.md:4268)). No normative
function defines `freeze_basis`:

- the solo common case is effect-final immediately and may have no distinct
  boundary-operation hash
  ([effect finality](/Users/vm/owner-plane-d0a-spec.md:437)); and
- a later generation can depend on several independent immutable closures,
  so removal of any one may unfreeze it. Winner plus one support hash cannot
  represent that predicate.

The semantic “≤2” appears only in D-169's decision row; the CDDL array is
unbounded and E8 has no corresponding cap. The release language also omits
permanently non-revivable quarantine and uses the undefined `frozen-out`
label.

Before adding a larger wire shape, test reachability. A winner frozen by
effect finality completes the source record. A winner frozen only by an
authority-ending frontier but not yet effect-final is an accepted unresolved
attempt, so terminal recovery defers. In neither branch does rejection of a
duplicate loser appear to make that source record `XferAbort.missing`. The
vector inventory nevertheless requires collision Abort → reopen
([vector](/Users/vm/owner-plane-d0a-spec.md:3290)). If no reachable trace
exists, remove `import-collision` from terminal causes and delete that vector.
If one exists, specify it and carry a versioned typed freeze certificate with
structural/immediate, multi-boundary, and authority-frontier arms, canonical
support selection, ordering, and caps.

### A3. D-170 chooses the right binding but carries none

The peer correctly endorses the security posture: derive the binding from
the actual source and do not grant the export signer authority to substitute
record content
([peer D-170](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:21)). The
normative rule says the durable destination attempt records that source leaf
hash
([validator](/Users/vm/owner-plane-d0a-spec.md:2606)).

No durable protocol object contains it:

- `ItemCore` encrypts the exact `SignedOperation` triple, so auxiliary
  verifier metadata cannot be hidden there
  ([item plaintext](/Users/vm/owner-plane-d0a-spec.md:915));
- `itemcommit`, `pendingxfer`, and the closed `txnrec` union have no
  attempt-registration member or binding field
  ([item commit](/Users/vm/owner-plane-d0a-spec.md:4243);
  [journal union](/Users/vm/owner-plane-d0a-spec.md:4256));
- `mimport` carries importer-controlled record bytes and its Merkle proof,
  not independently authenticated source equality
  ([import CDDL](/Users/vm/owner-plane-d0a-spec.md:4404));
- bundles are deliberately not persisted
  ([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:2644)); and
- the local index is rebuildable and explicitly removed/rebuilt
  ([index](/Users/vm/owner-plane-d0a-spec.md:1090)).

Thus B′ can be durably pending after a verifier saw B, B can be erased, and a
crash or second replica can rebuild with B′ plus a valid path but with neither
B nor the unspecified cached binding. Trusting B′, rejecting it, and waiting
forever are all materially different and none is selected.

Add a versioned durable and portable registration record authenticated by
the verifier/service that performed source equality, under authority the
importer cannot mint via `m.import` alone. Define atomicity with attempt
registration, identity/replay, ingestion of already-carried `mimport` bytes,
source-erasure precedence, cold rebuild, and missing/mismatch outcomes. D0-A
must define the bytes and rebuild law; D0-B may later distribute those same
bytes. An importer-asserted hash, or an unauthenticated/rebuild-local cache
entry written by the plane writer, does not prove that source equality
occurred. A verifier-authenticated durable record may use the plane writer
only if the specification names that authority and makes it unavailable
through ordinary `m.import` authorship. The simpler coherent alternative
remains erasure-wins for every not-yet-accepted import.

### A4. D-171's `at` is a control coordinate, not a historical snapshot

`XferAbort.at` is a useful repair for the narrow case in which both sufficient
facts are control operations on the same surviving chain. It does not support
the broader peer claim that any later lower-hash fact can be placed relative
to the terminal
([peer D-171](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:28)).

The basis universe includes tenant facts, and basis invalidation can
additionally be driven by proof-feed events
([cause universe](/Users/vm/owner-plane-d0a-spec.md:1252)). `at`, however, is
specifically a control-operation hash
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4282)). Tenant operations carry no
authoritative control frontier and their HLC is chronology-only
([operation header](/Users/vm/owner-plane-d0a-spec.md:529)). Receipts have
issuer-sequence coordinates; only leases carry a `ctrl_frontier`, and it is
explicitly diagnostic
([proof shapes](/Users/vm/owner-plane-d0a-spec.md:660)).

Let Abort T be written at control head C using sufficient control cause S.
Later, held eligible tenant fact W arrives, independently makes the same
record negative, and has `H(W) < H(S)`. W's bytes do not place its arrival
before or after C. A cold rebuild cannot know whether to include W in “facts
at or before `at`”; including it changes the required minimum, while excluding
it has no carried basis.

Several adjacent shapes remain incomplete:

- `XferReopen.invalidation` is an operation hash, but an issuer-fork event is
  identified by a receipt/lease `stmt_id`
  ([reopen CDDL](/Users/vm/owner-plane-d0a-spec.md:4309)).
- The “TOTAL” map still cannot cite static scope mismatch, a never-issued
  grant, request fork, class exclusion, or the cap/void fact that makes some
  budget/lineage negatives permanent.
- The main terminal literal still omits mandatory `at`
  ([main literal](/Users/vm/owner-plane-d0a-spec.md:1235)).
- Basis-free intrinsic and source-erased Aborts nevertheless require `at`,
  acquiring an unexplained dependency when C3′ cuts that position.
- Unheld `at`, basis, or invalidation has no complete terminal-first
  reservation and exact outcome; wrong incarnation, double terminal, and
  reopen-after-Done name dispositions without full E10 results.

The smallest repair is to abandon historical-minimum selection: carry one
typed, currently verifiable sufficient cause certificate and reopen if that
chosen certificate invalidates, even if another cause now exists. If
historical canonicality is retained, the terminal must carry or reference a
versioned, reconstructible snapshot whose preimage or membership proofs are
durably available. It must cover relevant held tenant facts—including
nonaccepted eligible caps and revivably quarantined claimants—and proof-feed
branch state. An ordinary accepted Frontier or opaque state hash is
insufficient. Use a tagged `factref` union for operations, issuer statements,
and structural facts, and define terminal-first delivery plus C3′ removal.

### A5. D-173 is constructible, but its reference proof is ceremonial

This is the synthesis's material correction to the first review. The peer is
right that a state machine defined as “complete when the target's
decryptable-wrap domain is empty” is deterministic and constructible at the
portable control layer: an accepted excluding rotation makes the zone absent
from the portable control coverage domain; `wrap_add` reintroduces it before
completion
([domain](/Users/vm/owner-plane-d0a-spec.md:1396)). The 0-author/65-wrap
ceremony is therefore constructible. Do not restore the void current-membership
reading.

The remaining issue is representational. An accepted rotation R excluding D
removes Z from the domain before a later revoke V evaluates
`rotation_refs`. Therefore:

- if R exists, Z is absent and R is unnecessary to coverage; and
- if Z remains, no accepted R exists that can be cited to cover it.

After 65 prior exclusions, the shrinking-state reading permits completion
with zero references, so the advertised 64-reference cap and
`c.revoke_zones` continuation never prove the 65-zone case
([E8](/Users/vm/owner-plane-d0a-spec.md:121);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3922)). Hosted mode independently
requires naming the rotation
([hosted rule](/Users/vm/owner-plane-d0a-spec.md:1943)), making the field
mandatory there for freeze linkage but still redundant to the main coverage
equation.

Choose and publish one role:

1. **State-derived completion:** completion means the decryptable-wrap domain
   is empty; demote references to typed audit/freeze linkage and remove claims
   that they discharge coverage or need a 65-zone continuation.
2. **Reference-derived completion:** snapshot outstanding exclusion
   obligations before rotations remove them and require references to
   discharge that frozen set.

This is high freeze exactness; the evidence does not establish a reducer or
constructibility failure. Also separate control-authority exclusion from
per-replica cryptographic completion:
rotation acceptance authorizes, while the local durable Fence activates the
new epoch
([Fence rule](/Users/vm/owner-plane-d0a-spec.md:972)).

### A6. D-174 rejects the current renewed certificate

The peer correctly confirms that recovery now appears in the main equation
and that the old-C0/new-G1 attack is blocked
([peer D-174](/Users/vm/owner-plane-d0a-spec-v0.5.15-review-2.md:39);
[equation](/Users/vm/owner-plane-d0a-spec.md:1640)). The added compatibility
relation has the wrong direction.

It admits `(certificate C, grant G)` only when G was issued during C's
effective span or C has a supersede frontier covering the operation
([compatibility](/Users/vm/owner-plane-d0a-spec.md:1654)). Apply that to the
ordinary case:

1. Enroll C0 with live G0.
2. Renew to current C1; grants intentionally survive because they bind
   `device_id`.
3. C1 authors under inherited G0.

G0 predates C1's span. Current C1 has no supersede frontier covering its new
operation. Both arms fail, so the protocol emits `cert-superseded`, directly
contradicting “renewal never orphans [grants]”
([renewal invariant](/Users/vm/owner-plane-d0a-spec.md:374)).

The relation needs an upper bound: reject when G was issued strictly **after
C ceased being effective**. That admits co-issued and inherited grants for a
current certificate while rejecting old C0 with post-supersession G1. The
four closure axes independently bound the operation position.

Two mirrors also remain wrong. §4.2 still lists certificate ∩ grant ∩ epoch,
omitting recovery
([§4.2](/Users/vm/owner-plane-d0a-spec.md:351)). And
`cert-superseded` exists only as pending-dependency “awaiting renewal chain”
([disposition](/Users/vm/owner-plane-d0a-spec.md:2338)). Missing renewal
evidence may pend; a proven incompatible pair is permanent within the branch.
Give those cases distinct exact lifecycles and vector C0/G0 → C1/G0,
co-issued C/G, and old C0/new G1.

### A7. D-175 cannot compare retired material a fresh replica never saw

For held certificates, D-175's `mat_id = H_mat(SEC1 point bytes)` is the right
direct comparison
([identity rule](/Users/vm/owner-plane-d0a-spec.md:845)). The `mat` tag,
role-swap rejection, and intra-certificate check all landed. Recovery's
portable burn set still carries only role-tagged opaque hashes:

```
retired_keys: [* bytes32]  # key_id = H_key({alg, pk})
```

([recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:4179)).

Suppose recovery carries `H_key({"hpke-p256-v1", P})` from a cut branch and a
fresh replica later sees P proposed as a `p256` signing key. Its candidate
`key_id` differs, and the recovery bytes do not reveal P from which to derive
the retired member's `mat_id`. A v1 reducer could compute both closed P-256
role-tagged IDs from candidate P, but the specification never mandates that
alternate-tag enumeration.

Either carry typed public material, mandate candidate-side alternate-role
matching for the closed v1 tag set, or carry both hashes as recovery-authority
assertions. If both hashes are carried without P, their cryptographic
relationship cannot itself be verified; say so. Apply the same relation to
D-172's retired-versus-terminal-adopted overlap.

Also state the exact identity boundary. Canonicalizing SEC1 can handle the
specific P versus −P equivalence if desired; no public identifier can detect
arbitrary related-key derivation. Do not use “same material” more broadly
than the chosen equivalence relation.

### A8. D-175 leaves two live Frontier transitions

§9.3 now gives the intended transition: a `w.gen(last_known = H)` naming a
held-but-displaced head retires the effective **accepted** head at or below
H, with a successful no-op if none exists
([§9.3](/Users/vm/owner-plane-d0a-spec.md:2093)).

The core Frontier rule still says `last_known` names a terminal head and the
Frontier drops exactly the incorporated/named head
([§4.6](/Users/vm/owner-plane-d0a-spec.md:628)). With accepted H4 and
canonical-but-budget-displaced H5, accepted W citing H5 retires H4 under
§9.3; under §4.6 it rejects or removes absent H5 and leaves H4 live. The two
reducers produce different Frontier and checkpoint state from identical
bytes.

D-76's old exact-head summary is a historical row amended by D-175, not a
third coequal current rule, but it should be marked superseded or updated to
stop reintroducing the bug
([D-76](/Users/vm/owner-plane-d0a-spec.md:3484)). Sweep §4.6 and every live
mirror. Vector acceptance, displacement/restoration, later revival, and the
no-accepted-predecessor success case.

## D-172 and repairs to retain

D-172 is not a standalone blocker. Main prose now carries every link in a
selected adopted-renewal chain, including signing-only intermediates; the
terminal-adopted/retired overlap rejects; and >64 links have an explicit
orphaning residual. Keep all three.

Two cleanups remain:

- the CDDL introduction still calls the mechanism “KEM RENEWALS” before
  requiring signing-only members
  ([CDDL introduction](/Users/vm/owner-plane-d0a-spec.md:4149)); and
- overlap across signing/KEM tags cannot be evaluated until A7 defines the
  role-neutral recovery relation.

Also retain D-168's late suffix re-fold, D-169's expanded reservation, D-170's
source-equality security posture, D-171's citable-cause direction, D-173's
shrinking historical domain, D-174's four-axis equation and old-signer
objective, and D-175's direct same-point checks plus accepted-predecessor
transition.

## E10 and canonical-shape closure

Before freeze, map every state to one exact outcome and disposition. At
minimum, close:

1. a renewal or other required consumer waiting on a staged carrier;
2. ordinary import displacement and a judgment/pin/erase waiting on
   provisional ownership;
3. missing, mismatching, or unauthenticated durable source binding,
   distinguishing source-still-held from source-erased;
4. incomplete main/continuation revocation and the chosen role of references;
5. unheld, wrong-kind, non-ancestral, or C3′-removed journal `at`; unheld or
   invalid cause members and invalidations; and terminal-first reservation
   behavior for each;
6. known certificate/grant incompatibility versus missing renewal evidence;
7. wrong journal incarnation, double terminal, and reopen-after-Done; and
8. permanently non-revivable quarantine in the freeze-reservation release
   rule.

At the same time, use tagged fact references instead of untyped hashes, give
every logical set an E7 key/sort/duplicate rule, and place every semantic cap
in both E8 and CDDL. Vector names in §13 are requirements; until the companion
and runner exist, they are not passing evidence.

## v0.5.16 closure checklist

Before another freeze review, prose, CDDL, decisions, outcomes, and vector
inventory should agree on:

1. one next-applicable staged-consumer relation, with renewal and selector
   scope;
2. either removal of an unreachable collision-terminal cause or a reachable
   trace plus a typed freeze certificate;
3. a durable authenticated source-binding carrier and cold-rebuild law;
4. either a typed nonhistorical journal cause or a versioned, reconstructible
   multi-feed held-fact snapshot with durable preimages or membership proofs;
5. one explicit role for `rotation_refs`, with authority-versus-Fence
   completion named;
6. upper-bound certificate/grant compatibility and split missing/known
   lifecycles;
7. recovery-portable cross-role material retirement; and
8. one accepted-Frontier retirement rule on every normative surface.

Run the E10/E7/E8 sweep with those edits. Then make these counterexamples the
companion's opening tranche.

## Artifact sequence

The eventual order remains sound:

1. ratify and propagate the seven hard repairs plus D-173 exactness;
2. write the counterexample fixtures into the normative companion;
3. build the independent reducer and differential harness;
4. generate and execute families 1–13;
5. perform family 14 offline confirmation; and
6. run the final prose↔CDDL↔vector discrepancy audit.

Non-normative fixtures may be drafted now. The schema and reducer must not
silently decide the missing law.

## Final assessment

v0.5.15 is another meaningful convergence step. The peer report supplies
good independent evidence that the intended repairs were inserted, and it
correctly resolves D-173's prior constructibility dispute. The first review's
composed traces, however, expose failures that field-presence checking cannot:
the stored binding has no bytes, the journal coordinate covers only one of
several ordering domains, inherited grants fail under the new relation, and
the core Frontier still states the superseded transition.

The right synthesis is therefore neither “nothing landed” nor “all closed.”
Most chosen directions should be retained, D-172 is substantially complete,
and D-173 needs a narrower clarification. Seven protocol-level blockers still
require one more text cut before the artifact pipeline becomes normative.

**Final decision: no-go for freeze, normative companion, or independent core;
cut v0.5.16 and review again.**
