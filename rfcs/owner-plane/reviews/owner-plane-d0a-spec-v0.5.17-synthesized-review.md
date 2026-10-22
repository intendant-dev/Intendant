# Synthesized review: D0-A Core + Memory normative specification v0.5.17

*2026-07-13. Adjudicated synthesis of
[*Review: D0-A Core + Memory normative specification v0.5.17*](/Users/vm/owner-plane-d0a-spec-v0.5.17-review.md)
(SHA-256
`4eeca4281627236d4848ee74b19e525a0d02a53786a2f9d6ef298b628b653ee7`)
and
[*Review 2: D0-A Core + Memory specification v0.5.17*](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md)
(SHA-256
`ee880ffe4e502bccc83363cfcfcef76daea36aaaf324a545ad181a652960f3c3`),
verified against
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.17,
4,736 lines / 53,191 words / 401,854 bytes (SHA-256
`92c9b43ff9e1b651486d1a638f5d57e3c8fc8fa650e2cce6c2fd5ece9a5fcd7d`).
The v0.5.16 synthesis used as the incoming disposition ledger has SHA-256
`982ef9cc5957eeeb0087b9d5f99bc7eb33899c26c255c770fae17087ac161bc3`.
This document adjudicates insertion claims against normative bytes and composed
replay; it does not average the reports' verdicts.*

## Executive verdict

**Cut v0.5.18. Do not freeze v0.5.17 or begin the normative companion and
independent reducer from this text.**

The peer report is explicit that it is an **insertion audit** and assigns
composed replay and the freeze judgment to this synthesis
([scope](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:8)). Its artifact
recommendation is expressly conditional on replay concurrence
([condition](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:71)). That is a
sound review boundary. Within it, the report correctly confirms several
important changes:

- commit is a better intended erasure boundary than acceptance history;
- the basic terminal-first reservation algorithm landed;
- the main registry row states the selected state-derived revocation law;
- both contextual `cert-superseded` lifecycles are now in §10.5;
- the Frontier predicate and no-predecessor no-op are repaired;
- `source-erased` and journal `log-corrupt` appear in the closed outcome map;
- the P/−P residual is stated honestly; and
- the searched D-153 and `(winner, freeze_basis)` decision-ledger hits are
  historical or explicit withdrawal text, not live resurrection by themselves.

Those conclusions should survive v0.5.18.

The peer's broader “all eight residue items are discharged” conclusion does
not survive the negative replacement audit or composed replay. Four direct
normative contradictions are enough to decide the round:

1. Appendix A still says `rotation_refs` cover wrap zones and revocation
   completion requires wrap “coverage,” immediately beside the new rule that
   references are linkage only and never coverage.
2. The main journal prose and CDDL still allow an Abort basis to be an issuer
   statement, immediately beside D-185's “basis is ALWAYS op-kind.”
3. The basis-free discriminator says static scope/flow/class failures omit
   basis, then says only intrinsic byte failures and `source-erased` do.
4. Incomplete revocation coverage and a nonempty wrap domain are called
   “pending” without one exact member of the closed outcome enum.

Composed replay adds three protocol-level blockers:

- D-184 asserts that erasure shares a critical section with destination commit,
  but the detailed critical section does not include the erase machine, the
  events live in different logs, and no common authenticated order exists.
- D-185's repaired reservation depends on one exact journal order, but D0-A
  does not freeze the authenticated append stream or terminal incarnation from
  which that order is recovered.
- A dead staged frontier is both outside the applicable domain and required to
  be consumed, while an `import-collision` claimant is both permanently unable
  to win and eligible to become owner when the current owner's proof dies.

The adjudicated result is therefore five hard clusters—erasure serialization,
journal carrier/cause exactness, revocation replacement/E10, dead-stage
consumption, and collision lifecycle—plus one high temporal-coordinate issue
for delayed revocation completion. D-187's central relation, D-188, and D-190's
substance are closed.

The source itself remains honest about artifacts: Gate A is “pending —
currently false” because the companion, corpus, surface runs, and discrepancy
audit do not exist
([status](/Users/vm/owner-plane-d0a-spec.md:3750)). The peer's proposed artifact
order is good; its condition to begin that order is not met.

## Assessment of Review 2

### What the peer review establishes well

The report is concise and disciplined about its declared method. It checks
that the intended D-184–D-190 rulings appear in the expected main surfaces and
does not pretend to have run an independent reducer. Its strongest positive
contributions are:

- recognizing that v0.5.16's acceptance-history rule needed a durable boundary
  and that ordinary commit is the natural place to look;
- confirming the exact terminal-first reservation language, including
  `ref-unresolved`, suffix blocking, and held-invalid `log-corrupt`;
- confirming the selected three-part revocation law in the registry row;
- verifying the two `cert-superseded` lifecycle entries rather than accepting
  the decision row as a substitute;
- distinguishing historical decision-ledger text from live reducer text; and
- keeping its artifact recommendation conditional and all durable P1 writes
  behind Gate B plus the umbrella prerequisites.

That is useful evidence of intended insertion and should be preserved in the
review record.

### Where the conclusion exceeds the evidence

The report applies “prose storage is not storage” to acceptance history, then
accepts the phrases “authenticated,” “serializes,” and “in the log” without
checking the storage machine that would make them true
([claim](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:19)). The live lock
does not mention erasure, and there is no single log containing both events.

It also performs a positive insertion check where a negative replacement check
is required:

- It says D-186 collapsed the old revocation law
  ([claim](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:32)), while the
  old law remains in normative CDDL.
- It says basis is always op-kind
  ([claim](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:27)), while the
  live CDDL still types terminal bases as op-or-statement.
- It says E10 is complete
  ([claim](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:45)), while the
  revocation registry still supplies only a disposition-like “pending” state.
- It calls the P/−P probe a family-13 vector
  ([claim](/Users/vm/owner-plane-d0a-spec-v0.5.17-review-2.md:49)); it actually
  appears in family 7, while family 13 is storage.

Finally, an insertion audit cannot see two state-machine contradictions that
are not literal leftovers from the incoming search list: dead-lineage stage
consumption and same-branch collision revival. Those require the composed
replay the peer correctly assigned to this synthesis.

Thus the peer report is valuable confirmation of selected intentions, not
evidence of freeze-readiness. Its recommendation remains conditional by its
own terms.

## Adjudicated disposition ledger

| Topic | Review 2 | Synthesized disposition |
|---|---|---|
| D-176 staged frontiers | Discharged; old “EVERY” hit is ledger text | **Ordinary repair closed; hard dead-stage conflict.** “Only current-domain stages” and “out-of-domain stage consumed vacuously” are both live |
| D-177 collision | Discharged; stale conjunction is withdrawal text | **Terminal-cause repair closed; hard fold-lifecycle conflict.** A collision claimant can become owner when the frozen owner's proof dies |
| D-184 erasure/import | Discharged by authenticated serialized `ItemCommit` | **Hard storage/recovery blocker.** Intended boundary is good; erase-side lock, exact point, common durable order, and append authority are absent |
| D-185 journal | Discharged; reservation and op-kind basis landed | **Algorithm closed only under an assumed stream; hard schema/transport blocker.** Wire roles, basis absence, missing refs, and ordered journal object remain open |
| D-186 revocation | Discharged; one law | **Hard normative contradiction.** Registry has one intended law; CDDL still has old and new laws plus contradictory continuation text |
| D-187 certificate × grant | Discharged | **Central repair closed.** Define the cessation coordinate for a revocation that pends and completes later |
| D-188 Frontier | Discharged | **Closed.** Historical mentions carry supersession pointers |
| D-189 / E10 | Discharged | **Partially closed; Gate blocker.** `source-erased` and journal mappings landed, but revocation/missing-reference contexts and CDDL mirrors remain incomplete |
| D-190 P/−P | Discharged | **Substantively closed.** Correct family 13 → family 7 and make the acceptance setup explicit |
| Artifact readiness | Begin if synthesis concurs | **No concurrence.** Fix protocol law before the companion and reducer |

## Detailed adjudication

### A. Retain commit as the intended boundary; D-184 does not yet carry its order

Both reviews agree on the key design improvement: commit is a durable local
fact, while prior acceptance was an unrecorded reducer history. The closed
`source-erased` outcome and full-validator CDDL wording are real repairs
([outcome](/Users/vm/owner-plane-d0a-spec.md:2401),
[validator](/Users/vm/owner-plane-d0a-spec.md:2682),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4587)).

The disagreement is whether the current bytes actually order commit against
erasure. They do not.

The published `release_op` critical section serializes destination replay-key
observation, in-flight import completion, and source terminal append
([scope](/Users/vm/owner-plane-d0a-spec.md:1212)). Fence, key destruction, and
Tombstone are separate erase-machine transitions and are not required to take
that section
([machine](/Users/vm/owner-plane-d0a-spec.md:1037)). The destination
`ItemCommit` and source erasure are in different zone logs; cross-zone transfer
is explicitly “two commits in two zone logs”
([storage](/Users/vm/owner-plane-d0a-spec.md:1176)). `ItemCommit` itself carries
core, wrap, and writer coordinates but no validator verdict or shared
serialization coordinate
([shape](/Users/vm/owner-plane-d0a-spec.md:4404)).

Therefore this schedule remains legal under the detailed machines:

1. I verifies source S and pauses before destination fsync.
2. The erase rotation destroys/tombstones S.
3. I resumes and fsyncs its `ItemCommit`.

The final bytes do not distinguish that order from commit-first then erase,
although D-184 requires opposite results. A runtime assertion that the lock
serializes them cannot substitute for defining which erase transition takes
which lock and which durable byte carries the order.

The lock key is also wrong for erasure: one source item may appear in many
releases, so source erasure cannot directly acquire one `release_op` section
without an unbounded discovery and lock-order rule. Use a shared
`(source_zone, source_op)` exclusion from source-equality read through
destination flush and across the exact erasure point, with a fixed nesting
order relative to the release journal lock.

The repair may preserve `ItemCommit` as the boundary, but it must freeze its
trusted append-authority semantics and a byte-visible cross-log order. A
source-zone `ImportCommitted` marker, a plane-global WAL coordinate, or an
authenticated append stream could do that. Also name whether erasure occurs at
erase-request acceptance, Fence, successful key deletion/`KekDestroyed`, or
Tombstone; define import precommit/pending storage; and vector pause→erase→
flush, crashes at each erase state, one source in two releases, opposite-log
replay, and late replica delivery.

The peer's central idea is retained. Its claim that the current record already
implements the idea is not.

### B. Retain terminal-first reservation; finish the journal contract

The peer is right that D-185 closes the simple local race. Given one exact
stream, a terminal or Reopen with an unheld citation gets `ref-unresolved`,
reserves the interval, blocks its suffix, and re-folds when the fact arrives
([machine](/Users/vm/owner-plane-d0a-spec.md:1352)). Held-invalid citations and
interval violations map to `log-corrupt` → storage-quarantine
([map](/Users/vm/owner-plane-d0a-spec.md:2419)). Keep this algorithm.

It is not yet a complete schema or portable journal:

1. The main prose first allows a terminal basis to be an op hash or statement
   ID
   ([broad rule](/Users/vm/owner-plane-d0a-spec.md:1281)), then declares basis
   always op-kind
   ([narrow rule](/Users/vm/owner-plane-d0a-spec.md:1312)). CDDL keeps the
   broad union on Abort and Reopen basis
   ([CDDL](/Users/vm/owner-plane-d0a-spec.md:4429)). Split `opfactref` for
   terminal/copied bases from the full union used only by invalidation.
2. Static scope/flow/class failures are declared basis-free
   ([map](/Users/vm/owner-plane-d0a-spec.md:1298)), then basis absence is said
   to identify only intrinsic bytes and `source-erased`
   ([discriminator](/Users/vm/owner-plane-d0a-spec.md:1323)); CDDL repeats the
   latter. Replace the lists with one outcome-by-context cause table.
3. `no-cert` and “never-issued” grant still conflate unheld reference with
   proven nonexistence. I-before-control and control-before-I need one
   `ref-unresolved`/closed-prefix rule.
4. `XferAbort` and `XferDone` have no incarnation. Their interval and effect key
   depend on exact prior journal order, but the native file defines only
   physical CRC-framed order and the browser mapping does not define an ordered
   journal object. D0-A must freeze one authenticated append authority,
   sequence/previous linkage, gaps, exact replay, conflicts, and explicit or
   exactly derived terminal incarnation for D0-B to transport.

Add `T0 → R0 → T1` under every dependency and delivery order, two pending
transitions, duplicate/conflicting records, statement-basis rejection,
statement invalidation acceptance, missing cert/grant order, and native/browser
cold replay.

The peer's statement “journal order survives pendency” is a correct desired
property. It is not the carrier that makes the property portable.

### C. D-186 is an unequivocal replacement failure

No composed judgment is needed for the core contradiction. The registry row
now states the intended law
([registry](/Users/vm/owner-plane-d0a-spec.md:1455)). Appendix A still says
`rotation_refs` cover wrap zones and completion is authorship coverage plus
wrap coverage
([old law](/Users/vm/owner-plane-d0a-spec.md:4037)), then says references are
typed linkage, never coverage, and completion is state-derived
([new law](/Users/vm/owner-plane-d0a-spec.md:4056)). It says no reference
continuation exists and immediately defines `crevokezones.rotation_refs`
([texts](/Users/vm/owner-plane-d0a-spec.md:4069)).

Rewrite the CDDL commentary from one law rather than appending another
qualification:

- cutoffs continue authorship closure;
- accepted exclusion state empties the decryptable-wrap domain;
- references are typed linkage only, mandatory on hosted planes;
- a continuation reference, if retained, is additional linkage, never coverage.

Then assign exact outcomes to unheld link, held-invalid link, incomplete
authorship closure, and nonempty wrap state. “Pending” and
`pending-dependency` are not members of §10.4. `ref-unresolved` may be reused if
its contexts are explicitly widened.

The repair must also pin D-187's cessation coordinate. If the initial
`c.revoke_device` pends and a later rotation/continuation completes it, say
whether the certificate ceases at the initial operation, the completing
carrier, or by a named retroactive fold. Vector a grant and tenant operation
between the two positions.

This direct CDDL evidence disproves the peer's claim that D-186 was collapsed;
no methodological tie-break is needed.

### D. D-189 is a partial success, not E10 completion

Keep the genuine improvements: `source-erased` is in the body enum and
reject-permanent map; proven-versus-awaiting `cert-superseded` has both
lifecycles; journal invariant violations are `log-corrupt`; unheld journal
citations are `ref-unresolved`.

E10 nevertheless remains false because:

- revocation incomplete/nonempty states have no exact outcome;
- missing cert/grant references lack an arrival-independent context split;
- statement-kind terminal basis is syntactically allowed while semantically
  forbidden; and
- CDDL/vector comments still state bare `storage-quarantine` for wrong
  incarnation/double terminal/reopen-after-Done rather than the pair
  `(log-corrupt, storage-quarantine)`
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:4495)).

The peer correctly verified positive insertions, but “every negative site now
names its outcome” is factually too strong.

### E. Composed replay exposes two additional live conflicts

#### E1. Applicable versus dead staged frontiers

The ordinary D-176 relation says a consumer takes only stages whose pair lies
inside its current coverage domain
([rule](/Users/vm/owner-plane-d0a-spec.md:1796)). The same live section says a
stage whose lineage has left that domain is consumed vacuously
([exception](/Users/vm/owner-plane-d0a-spec.md:1841)); Appendix A repeats both.

If the dead stage is not consumed, accept-stage → revoke-last-grant → regrant
can resurrect an old frontier under new authority. If it is consumed, the text
must identify the consuming event and selector. Consume it at the
authority-ending revocation, or explicitly extend the next consumer's
applicable set to staged pairs rendered pre-covered by an intervening immutable
close. Pin revoke/regrant under incremental and fresh folds.

The peer is right that D-153's literal “EVERY” decision row carries a D-176
amendment pointer. That historical hit is not this live contradiction.

#### E2. Permanent collision versus derived ownership

D-177 correctly keeps collision outside transfer terminal causes. At the
replay-key fold, however, claims against a frozen owner are called permanently
incapable of winning and get reject-permanent
([lifecycle](/Users/vm/owner-plane-d0a-spec.md:2839),
[map](/Users/vm/owner-plane-d0a-spec.md:2413)). The same reducer says proof
retro-disqualification or removal of the freezing basis unfreezes ownership
and re-derives the next owner
([unfreeze](/Users/vm/owner-plane-d0a-spec.md:2845)).

Thus A-frozen → B-collision → same-branch issuer-fork/compromise disqualifies A
requires B to re-enter the claimant fold, while its disposition says it cannot.
Either make freeze truly indissoluble, contrary to D-155, or make
`import-collision` a derived/revisitable state while the freeze basis stands.
The latter fits the existing design. Vector both incremental and fresh folds.

The peer's searched `(winner, freeze_basis)` hits are indeed withdrawals. This
is a different live lifecycle conflict.

## Closed topics to preserve

### D-187's central relation and lifecycle

Current certificate + inherited grant admits; old certificate + post-cessation
grant rejects; revocation and supersession are both named; proven incompatibility
is permanent within the branch while missing chain evidence pends
([semantics](/Users/vm/owner-plane-d0a-spec.md:359),
[map](/Users/vm/owner-plane-d0a-spec.md:2413)). Preserve this. Only the delayed
revocation completion coordinate in C remains.

### D-188

Held chain membership, accepted-predecessor removal, and successful no-op now
agree across Frontier, cap, generation, and vector surfaces
([Frontier](/Users/vm/owner-plane-d0a-spec.md:633),
[generation](/Users/vm/owner-plane-d0a-spec.md:2167)). Historical “terminal
Head” phrases name their own supersession. D-188 is closed.

### D-190

The exact-SEC1 boundary and P/−P/related-key residual are correctly stated in
the rule, threat table, and vector inventory
([threat](/Users/vm/owner-plane-d0a-spec.md:3514),
[vector](/Users/vm/owner-plane-d0a-spec.md:3210)). Correct the decision-ledger
label from family 13 to family 7 and make the vector setup explicit: P is first
in the relevant enrolled/burned domain, then candidate −P is accepted under a
named role/device context. No protocol redesign is needed.

## v0.5.18 closure and artifact sequence

Before another freeze review:

1. finish D-184 with one exact erasure transition, shared source-item lock,
   authenticated durable order, precommit/pending path, and crash/replay traces;
2. freeze the authenticated ordered journal object, split basis/invalidation
   types, and publish one outcome-by-context cause table;
3. rewrite revocation CDDL from the one selected law, assign all exact outcomes,
   and pin the delayed-completion coordinate;
4. reconcile applicable and vacuous dead-stage consumption;
5. make collision ownership revisitable when its standing freeze basis dies;
6. complete the remaining E10/mirror/vector pairs; and
7. correct D-190's family label and add the composed traces above.

Then follow the peer's proposed artifact order:

1. author `d0a-vector-cases.v1.json` before any fixture;
2. add the counterexamples first;
3. build the independent reducer and differential harness;
4. run the corpus, family 14, required surfaces, and crash matrices; and
5. perform the final prose↔vector discrepancy audit.

Gate A—not editorial confidence—then decides freeze. Gate B and the umbrella's
P0.5/tombed-cutover prerequisites continue to prohibit durable P1 writes.

## Bottom line

The peer review correctly confirms the intent of v0.5.17 and explicitly leaves
the decisive replay to this synthesis. Replay does not concur. Commit remains
the right place to found erasure survival, terminal-first reservation remains
the right journal algorithm, and the state-derived revocation law remains the
right authority model. What is missing is the protocol machinery and negative
replacement work that makes those intentions the only possible reading of the
bytes. Cut v0.5.18, close that bounded set, and only then begin the artifact
phase.
