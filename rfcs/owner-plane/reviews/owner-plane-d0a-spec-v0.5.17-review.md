# Review: D0-A Core + Memory normative specification v0.5.17

*2026-07-13. Independent review of
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.17,
4,736 lines / 53,191 words / 401,854 bytes (SHA-256
`92c9b43ff9e1b651486d1a638f5d57e3c8fc8fa650e2cce6c2fd5ece9a5fcd7d`).
The reviewed predecessor is the archived
[*v0.5.16 source*](/Users/vm/agenda-rfc-archive/2026-07-13-d0a-v0.5.16-as-reviewed.md)
(SHA-256
`808b2765b714b4a39fe8dfd02fd8e6bae93cb361af761f0c428d5f159a948abd`);
the delta is 145 insertions and 75 deletions. I used the
[*v0.5.16 synthesized review*](/Users/vm/owner-plane-d0a-spec-v0.5.16-synthesized-review.md)
(SHA-256
`982ef9cc5957eeeb0087b9d5f99bc7eb33899c26c255c770fae17087ac161bc3`)
as the incoming disposition ledger. No v0.5.17 peer report was consulted.
Findings were re-derived against the live prose, CDDL, storage records,
decision ledger, outcome map, and required-vector inventory, including
composed crash, dependency-order, branch-revisit, and cold-rebuild traces.*

## Executive verdict

**Cut v0.5.18. Do not freeze v0.5.17 or begin the normative companion and
independent reducer from these bytes.**

v0.5.17 is not a failed cut. It lands several important repairs correctly:

- `source-erased` is now a closed outcome with a stated lifecycle, and the
  Merkle-proof commentary no longer claims that the proof alone authorizes a
  post-erasure import.
- The basic terminal-first dependency race is repaired: an Abort or Reopen
  with an unheld citation pends as `ref-unresolved`, reserves its interval,
  blocks its suffix, and re-folds when the citation arrives.
- The main revocation registry row selects the right reducer: exclusion
  rotations are independently effectful; compound revocation completes from
  valid linkage, total authorship closure, and an empty state-derived
  decryptable-wrap domain.
- The contextual `cert-superseded` lifecycles are finally present in the
  authoritative disposition map, with revocation named beside supersession.
- The Frontier rules now consistently use held chain membership and the
  accepted-predecessor transition, including the successful no-predecessor
  no-op.
- The exact-SEC1 P-versus-minus-P limitation is honestly pinned in the
  freshness prose, adversary table, and vector inventory.
- The ordinary applicable-stage rule, all-certificate-renewal adoption
  wording, and old collision-terminal commentary were cleaned up.

Those repairs should survive. The remaining blockers are narrower than the
v0.5.16 set, but several are still protocol choices rather than editorial
polish:

1. **D-184 names an `ItemCommit` boundary without defining the erase-side
   serialization that makes it true.** The only published `release_op`
   critical section still excludes Fence, key destruction, and Tombstone;
   erasure is keyed by a source item that may occur in many releases; and the
   destination commit and source erasure live in different logs with no common
   authenticated order. “The order is in the log” is therefore not true of
   the frozen format.
2. **D-185 repairs terminal-first evaluation only under an ordered journal
   that D0-A still has not defined.** The CDDL also continues to admit
   statement-kind terminal bases after the prose declares every basis
   op-kind, and basis absence has two contradictory definitions.
3. **D-186 did not replace the old revocation law in normative CDDL.** The
   appendix still says references cover wrap zones and completion requires
   wrap “coverage,” then says references are linkage only and never coverage.
   It simultaneously says no reference continuation exists and encodes one.
4. **D-176 leaves dead staged frontiers both inapplicable and obligatorily
   vacuously consumed.** The contradiction matters on revoke then regrant: an
   old stage can otherwise become applicable again under new authority.
5. **D-177's fold outcome has the wrong lifecycle.** A claim against a frozen
   owner is called permanently unable to win, but the same reducer explicitly
   unfreezes ownership when the owner's proof or freeze basis dies, including
   within the same control branch.
6. **E10 is still false.** Incomplete revocation coverage and a nonempty wrap
   domain are merely called “pending”; several journal mirrors name only the
   `storage-quarantine` disposition; and missing certificate/grant references
   remain conflated with proven nonexistence.

The document itself correctly says Gate A is currently false because the
companion, corpus, surface runs, and final discrepancy audit do not exist
([status](/Users/vm/owner-plane-d0a-spec.md:3750)). That is expected at this
stage, but it means the right next step is one more bounded text cut, not
fixtures that would have to choose among the contradictions below.

## Disposition ledger

| Topic | What v0.5.17 closes | Remaining disposition |
|---|---|---|
| D-176 staged frontiers | Ordinary applicable scope, renewal waiting, `ref-unresolved`, and bounded suffix re-fold agree | **Hard edge case:** a stage whose lineage left the domain is both excluded by “only those” and required to be consumed vacuously |
| D-177 collision | Collision is correctly a fold outcome, never a transfer-terminal cause; stale conjunction commentary is gone | **Hard lifecycle conflict:** a supposedly never-winning collision claimant can win when the frozen owner is retro-disqualified |
| D-184 erasure/import | Commit is a much better candidate boundary than acceptance history; `source-erased` and both intended schedules are named | **Hard storage/security blocker:** erase-side locking, exact erasure point, common durable order, and append-authority semantics are not frozen |
| D-185 journal | Basic terminal/Reopen-first pending reservation and `log-corrupt` mapping are present | **Hard schema/transport blocker:** basis roles and absence disagree; the authenticated ordered stream and terminal interval carrier remain undefined |
| D-186 revocation | Main registry row selects state-derived completion and typed linkage | **Hard normative contradiction:** Appendix A retains the withdrawn reference-coverage law and omits exact pending outcomes |
| D-187 certificate × grant | Upper-bound compatibility and both `cert-superseded` lifecycles now agree in the main surfaces | **Core closed; high exactness:** define the cessation coordinate when a pending compound revocation completes later |
| D-188 Frontier | Held-chain predicate, accepted-predecessor removal, and successful no-op now agree | **Closed.** Remaining old terms are historical rows with explicit amendment pointers |
| D-189 outcomes | `source-erased`, journal `log-corrupt`, and unheld-citation `ref-unresolved` are in the authoritative enum/map | **Gate blocker:** revocation and missing-reference contexts remain unmapped; CDDL/vector mirrors still name dispositions instead of outcomes |
| D-190 material identity | Exact-point boundary and P/−P residual are stated and vectored | **Substantively closed; low cleanup:** decision rows call the control-fold vector “family 13,” but it is family 7 |

## Freeze blockers

### A. D-184's commit boundary is not yet a storage protocol

The direction is good. A committed destination record is a more plausible
cold-rebuild fact than “this operation happened to have been accepted before
the source disappeared.” `source-erased` is now present in the body outcome
enum and reject-permanent map
([enum](/Users/vm/owner-plane-d0a-spec.md:2401),
[map](/Users/vm/owner-plane-d0a-spec.md:2413)), and the import CDDL correctly
points to the full source-equality rule rather than elevating the Merkle path
alone
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4587)).

The missing part is the transition that makes commit and erasure mutually
exclusive.

The live critical-section rule still says that a `release_op` lock serializes
only destination replay-key observation, in-flight import completion, and the
source terminal append
([critical section](/Users/vm/owner-plane-d0a-spec.md:1212)). It does not say
that the erase rotation's Fence, `KekDestroyed`, or Tombstone takes that lock.
The erase machine remains a separate source-zone sequence
([machine](/Users/vm/owner-plane-d0a-spec.md:1037)). The new validator then
asserts, without defining that transition, that the same critical section
serializes `ItemCommit` against erasure and that “the order is IN THE LOG”
([claim](/Users/vm/owner-plane-d0a-spec.md:2682)).

The following schedule is still permitted by the detailed machines:

1. Import I reads source S, verifies the release and source-derived record,
   then pauses before the destination flush.
2. The source rotation destroys the old KEK and durably tombstones S.
3. I resumes and flushes its destination `ItemCommit`.

The final store contains both records. D-184 wants I to lose because it was not
committed at erasure, but a cold rebuild sees no byte-visible relation that
distinguishes that history from commit-first then erase. Cross-zone transfer is
explicitly two commits in two zone logs
([storage rule](/Users/vm/owner-plane-d0a-spec.md:1176)); the framing format
provides per-file physical order and CRC, not a shared cross-log coordinate
([framing](/Users/vm/owner-plane-d0a-spec.md:1383)). Signed HLC is explicitly
chronology only, not authority.

The lock key is also mismatched. One source claim may be released many times,
so erasing one `(source_zone, source_op)` would have to discover and acquire an
unbounded set of `release_op` locks. No discovery, canonical acquisition order,
or deadlock rule exists. The natural mutual-exclusion key is the source item,
possibly nested with the release journal lock under one fixed lock order.

Finally, `ItemCommit` is a generic record containing core, wrap, and writer
coordinates
([shape](/Users/vm/owner-plane-d0a-spec.md:4404)). Its inner operation and AEAD
authenticate bytes, while the outer frame is CRC-framed. If the intended rule
is that only a trusted plane validator may append this record after source
equality succeeds, that append-authority contract must be normative and must
survive whatever D0-B replicates. Otherwise a device-authored `ItemCommit`
proves only that the signed import bytes were stored, not that the independent
source-equality check ran before destruction—the same authority downgrade the
text rejects for a signer-authoritative Merkle leaf.

There is a smaller pipeline ambiguity here too. Source equality is a body-stage
check, after chain and time in the first-failure admission order
([pipeline](/Users/vm/owner-plane-d0a-spec.md:2310)). D-184 says an import
`ItemCommit` is written only after equality passes, including for imports whose
reported outcome is an earlier pending dependency. That is implementable via a
separate storage preflight, but no such preflight or durable-pending rule is
specified. The closed tenant log has no other pending-import carrier.

#### Required repair

Freeze all of the following together:

1. Name the exact erasure point: accepted erase request, Fence, successful key
   deletion / durable `KekDestroyed`, or Tombstone. The present four candidates
   have different availability and crash behavior.
2. Require every source-equality read through destination commit-flush and the
   erase transition to hold one shared `(source_zone, source_op)` exclusion,
   with a fixed order relative to the `release_op` journal lock.
3. Put the selected order into authenticated durable state. This could be a
   source-zone `ImportCommitted { release_op, source_op, import_op/item_addr }`
   record, a plane-global WAL coordinate, or an explicitly authenticated
   append stream whose semantics make the destination `ItemCommit` sufficient.
   A process mutex alone does not make cross-log order portable.
4. Define the import-specific precommit validation/pending-storage path and
   what a cold rebuild treats as a committed attempt.
5. Add equality-pass/pause → erase → destination-flush, crash at each
   erase state, one source in two releases, opposite log replay orders, and
   destination-record-after-erasure replica delivery to the vector inventory.

Until then, D-184 supplies a desired invariant and happy-path examples, not the
machine that enforces the invariant.

### B. D-185 closes the local race only under an undefined journal

The new terminal-first algorithm itself is coherent. Given one exact journal
stream, `T0(F)` arriving before fact F pends `ref-unresolved`, reserves interval
0, blocks `R0` and `T1`, and then applies before its suffix when F arrives
([rule](/Users/vm/owner-plane-d0a-spec.md:1352)). A held-invalid citation or a
second terminal maps to `log-corrupt` and storage quarantine
([rule](/Users/vm/owner-plane-d0a-spec.md:1361),
[map](/Users/vm/owner-plane-d0a-spec.md:2419)). Keep that algorithm.

Three surrounding contracts remain inconsistent.

#### B1. Terminal basis has two wire types

The main rule first says `XferAbort.missing[].basis` is a `factref` containing
an operation hash **or** issuer statement ID
([broad rule](/Users/vm/owner-plane-d0a-spec.md:1281)), then says every reachable
terminal basis is always op-kind and statement references are invalidations
only
([narrow rule](/Users/vm/owner-plane-d0a-spec.md:1312)). Appendix A retains the
broad union for both `XferAbort.basis` and `XferReopen.basis`, and its comment
explicitly says a terminal cause may be op or statement
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4429),
[Abort](/Users/vm/owner-plane-d0a-spec.md:4434),
[Reopen](/Users/vm/owner-plane-d0a-spec.md:4476)).

Split the types. `opfactref` should type Abort bases and Reopen's copied
`basis`; the op-or-statement union should remain only on `invalidation`.
Vector a held statement basis as `(log-corrupt, storage-quarantine)` and a
statement invalidation as valid.

#### B2. Basis absence has two definitions

D-185 correctly observes that a static wrong scope, `no-flow`, static class
exclusion, or genuinely never-issued grant may have no dissolvable fact to
cite, and declares such cases basis-free
([map](/Users/vm/owner-plane-d0a-spec.md:1298)). A few lines later, the stated
wire discriminator says **only** intrinsic byte failures and `source-erased`
omit basis
([discriminator](/Users/vm/owner-plane-d0a-spec.md:1323)); Appendix A repeats
the narrower list
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4463)). The same static `no-flow`
Abort is therefore valid or corrupt depending on which normative sentence an
implementer follows.

Replace the prose lists with one outcome-by-context table containing:
terminal eligibility, basis required/forbidden, allowed reference kind,
sufficient fact, invalidation predicate, outcome, and disposition. Generate
the CDDL comments and vector cases from that table's categories.

That table must also settle missing references. `no-cert` is starred
reject-permanent inside the fold
([map](/Users/vm/owner-plane-d0a-spec.md:2421)), while a “never-issued” grant is
basis-free. But a replica receiving an import before its cited certificate or
grant cannot prove nonissuance from absence. In the current header the
certificate and capability are hash references, not carried objects. Either an
unheld cert/grant is `ref-unresolved` until a portable closed control prefix
proves impossibility, or another non-arrival-relative proof of permanent
absence must be defined. I-first and cert/grant-first need to converge.

#### B3. The canonical journal stream is still assumed, not frozen

`XferAbort` and `XferDone` carry no incarnation; only Reopen does. A terminal's
incarnation and effect key are therefore derivable only from exact preceding
journal order
([shapes](/Users/vm/owner-plane-d0a-spec.md:4426)). Native file append order is
physical but has no journal sequence/previous hash or separately named append
authority; the browser mapping does not identify an ordered journal object
([browser mapping](/Users/vm/owner-plane-d0a-spec.md:1424)). D0-B owns
transport, but D0-A's writer-chosen-cause rule already depends on every replica
receiving one exact stream.

For a valid source stream `T0 → R0(incarnation 0) → T1`, delivery of T1
first lets a reducer interpret it as interval 0 because the terminal carries no
interval. The later prefix then appears to double-terminal or reopen the wrong
interval. Saying “journal order survives pendency” does not define how a
replica detects the gap.

D0-A need not choose D0-B's segment container, but it must freeze the semantic
object D0-B preserves: one authenticated append authority, exact record
sequence and previous linkage, gap behavior, byte-identical replay
idempotency, conflicting-record behavior, and either explicit terminal
incarnations or one exact derivation rule. Native and browser stores must map
to the same object.

Add `T0/R0/T1` under every dependency and journal-delivery order, two pending
transitions, exact duplicate delivery, conflicting same-sequence records, and
crash/replay after each transition.

### C. D-186's old revocation law remains normative

The rewritten registry row begins with the intended three-part law: references
validate only as linkage, authorship cutoffs are total, and the state-derived
decryptable-wrap domain is empty
([registry](/Users/vm/owner-plane-d0a-spec.md:1455)). That is the constructible
rule. Accepted exclusion rotations shrink the domain themselves; references
cannot also prove that same coverage.

Appendix A still says the opposite. Its `cutoffs` commentary says
`rotation_refs` cover the wrap zones and that completion is authorship coverage
**and wrap coverage**
([old law](/Users/vm/owner-plane-d0a-spec.md:4037)). The `rotation_refs`
comment immediately below says references are typed linkage, never coverage,
and completion is state-derived
([new law](/Users/vm/owner-plane-d0a-spec.md:4056)). It then says no reference
continuation exists or is needed, while `crevokezones` encodes an optional
reference continuation on the next line
([contradiction](/Users/vm/owner-plane-d0a-spec.md:4069),
[shape](/Users/vm/owner-plane-d0a-spec.md:4076)).

This is the same insertion-versus-replacement failure v0.5.17 was meant to
remove. Rewrite the whole CDDL comment from the selected registry law:

- `cutoffs` continue authorship closure only;
- accepted exclusion state, not references, empties the wrap domain;
- references are optional typed linkage on trusted planes and mandatory on
  hosted planes;
- `c.revoke_zones.rotation_refs`, if retained, is additional linkage only,
  never a coverage continuation.

The failure contract is also incomplete. An invalid held link is now
`body-invariant`, but incomplete authorship coverage and a nonempty wrap domain
are merely called “pending” in the registry. `pending-dependency` is a
disposition, not an E10 outcome. Assign exact outcomes to:

1. an unheld referenced rotation;
2. a held invalid link;
3. incomplete authorship closure; and
4. a nonempty state-derived wrap domain.

`ref-unresolved` may cover some or all of the pending contexts if explicitly
extended; otherwise add a dedicated closed outcome. Vector every transition,
including a `wrap_add` that re-enters the domain before completion.

One D-187 coordinate depends on this repair. A pending `c.revoke_device` may be
positioned earlier than the rotation or continuation that makes its effects
begin. The certificate rule says cessation occurs at the ending operation's
control position
([certificate rule](/Users/vm/owner-plane-d0a-spec.md:361)), while D-186 speaks
of an evaluating/completing position. State whether the certificate and grants
cease at the original revocation operation, at the completing carrier, or by a
specified retroactive re-fold. Add a grant and a tenant operation between
initial revocation and completion. Without that coordinate, the repaired
`cert-superseded` lifecycle still has two possible inputs.

### D. D-176's dead-stage exception contradicts “only applicable”

The normal rule is now clear: an advance or retirement consumes stages for the
zone's live lineages; a renewal consumes stages in its predecessor's authorship
domain; it consumes those stages **and only those**
([applicable rule](/Users/vm/owner-plane-d0a-spec.md:1796)). The same section
later requires a stage whose lineage has left that domain to be consumed
vacuously, materializing nothing
([dead-stage rule](/Users/vm/owner-plane-d0a-spec.md:1841)). Appendix A repeats
both statements
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4213)).

This is not harmless cleanup:

1. Stage S for `(zone Z, lineage L)` is accepted.
2. L's last grant in Z is revoked; its revocation frontier closes the old
   authority, so L leaves Z's live coverage domain.
3. Under “only applicable,” no later consumer consumes S.
4. L is later regranted authority in Z and S becomes applicable again.
5. A stale frontier staged for the old authority can now materialize under a
   new consumer's selector.

Choose an exact cancellation rule. The cleanest options are to consume S
vacuously at the authority-ending revocation that removes L, or to define the
next zone consumer's applicable set as current coverage **plus** staged pairs
rendered pre-covered by an intervening immutable closure. If renewal is also a
possible vacuous consumer, say how it identifies former predecessor-authorship
zones. Pin accept-stage → revoke → consume/cancel → regrant under both
incremental and fresh folds.

### E. D-177 calls a revisitable claimant permanently dead

The reachability ruling remains correct: an `import-collision` loser is never
listed as transfer `missing`, because the winner imported the record or its
unresolved attempt keeps the terminal deferred
([terminal rule](/Users/vm/owner-plane-d0a-spec.md:1316)). Keep collision out of
the transfer-cause vocabulary.

The fold lifecycle is not correct. The replay-key reducer says ownership
unfreezes when a frozen owner's proof is retro-disqualified or its freezing
basis is removed, then re-derives the next owner from all claimants
([unfreeze](/Users/vm/owner-plane-d0a-spec.md:2845)). Yet it calls claims
against that owner permanently incapable of winning
([reservation release](/Users/vm/owner-plane-d0a-spec.md:2839)) and maps
`import-collision` to reject-permanent
([map](/Users/vm/owner-plane-d0a-spec.md:2413)). The latter allows re-evaluation
for a dissolved control fact under C3′, but not for the same-branch
proof-retro-disqualification that D-155 explicitly names.

Trace:

1. A owns and freezes replay key K under effect finality.
2. Later claimant B receives `import-collision` because A is frozen.
3. An issuer fork or compromise cutoff retro-disqualifies A's proof without a
   C3′ branch cut.
4. D-155 unfreezes K and requires the total claimant order to select the next
   surviving claimant—potentially B.
5. The unconditional reject-permanent lifecycle says B cannot re-enter.

Either make freeze genuinely indissoluble—which would contradict the current
derived-ownership design and its stated escaped-effect posture—or give
`import-collision` a derived, revisitable lifecycle keyed to the standing
freeze basis. The latter is more consistent: B remains unable to win while the
basis stands and re-evaluates when it dies. Name the transition in §10.5 and
vector effect-final A → collision B → proof retro-disqualification under
incremental and fresh folds.

## Repairs that are substantively closed

### D-187's main compatibility and lifecycle repair

The current-certificate/inherited-grant trace now admits, old-certificate/
post-cessation-grant rejects, revocation is included beside supersession, and
§10.5 separates proven incompatibility from awaiting chain evidence
([certificate semantics](/Users/vm/owner-plane-d0a-spec.md:359),
[validator](/Users/vm/owner-plane-d0a-spec.md:2307),
[map](/Users/vm/owner-plane-d0a-spec.md:2413)). Preserve this rule. Only the
pending-revocation completion coordinate described in C remains open.

### D-188 is closed

The main Frontier definition, incorporation cap, ordinary `w.gen` admission,
and dependency clause now all use held chain membership rather than terminal
head status, and all agree that no accepted predecessor is a successful no-op
([Frontier](/Users/vm/owner-plane-d0a-spec.md:633),
[cap](/Users/vm/owner-plane-d0a-spec.md:1576),
[generation rule](/Users/vm/owner-plane-d0a-spec.md:2167)). Historical decision
rows retain old phrases only while naming their supersession. No further
semantic change is needed.

### D-190 is closed modulo vector labeling

The exact-SEC1 identity boundary is now honest: literal cross-role reuse is
blocked, P and −P remain distinct, and arbitrary related-key derivation is
outside the identifier's power
([threat table](/Users/vm/owner-plane-d0a-spec.md:3514)). The acceptance probe
appears in the control-fold family
([vector](/Users/vm/owner-plane-d0a-spec.md:3210)). D-182 and D-190 call it
“family 13”
([D-182](/Users/vm/owner-plane-d0a-spec.md:3709),
[D-190](/Users/vm/owner-plane-d0a-spec.md:3717)), but family 13 is storage;
this is family 7. Correct the label and make the setup explicit: after P is in
the relevant burned/enrolled domain, candidate −P is accepted under the stated
role/device context.

### The good parts of D-189 should stay

Keep `source-erased` in the body outcome family, contextual
`cert-superseded` in both lifecycle rows, and `log-corrupt` as the journal
invariant outcome. Clean the remaining mirrors: the CDDL and vector prose still
say a wrong incarnation or reopen-after-Done equals bare
`storage-quarantine`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4495)); they should assert the pair
`(log-corrupt, storage-quarantine)`.

## Recommended v0.5.18 closure sequence

Before another freeze review:

1. finish D-184 as one exact source-item serialization and authenticated
   durable-order rule, including its precommit/pending path and crash traces;
2. freeze the ordered journal semantic object, split basis/invalidation wire
   types, and replace prose cause lists with one outcome-by-context table;
3. rewrite the revocation CDDL from the state-derived law and assign exact
   outcomes plus the revocation-completion coordinate;
4. reconcile applicable versus vacuous stage consumption;
5. make collision ownership revisitable when its freeze basis dies;
6. finish the E10 mirror sweep and the two missing-reference contexts; and
7. correct the D-190 family label and add the composed traces named above.

Then author the normative companion schema before fixtures, add the
counterexamples first, build the independent reducer/differential harness,
run all required surfaces, and perform the final prose↔vector discrepancy
audit. The source's own Gate-A status should remain false until that sequence
is complete.

## Bottom line

v0.5.17 gets several hard choices right, and the next cut should be smaller
than this one. It is nevertheless not safe to give the go-ahead. The most
important remaining sentence is not a new cryptographic primitive; it is an
exact statement of **which authenticated byte orders import commit before
source erasure**, followed closely by **which ordered journal byte assigns a
terminal to an interval**. Once those carriers are real and the revocation,
stage, collision, and E10 mirrors have one answer, the specification will be
ready for the artifact phase rather than another conceptual redesign.
