# Review: D0-A Core + Memory normative specification v0.5.16

*2026-07-13. Independent review of
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.16,
4,666 lines / 51,743 words / 390,864 bytes (SHA-256
`808b2765b714b4a39fe8dfd02fd8e6bae93cb361af761f0c428d5f159a948abd`).
The reviewed predecessor is the archived v0.5.15 source (SHA-256
`94299df5e470bdd7878ef9866fbaa3e2f7cc67031bd5160d0ed3061dfe648e73`);
the delta is 237 insertions and 137 deletions. I used the v0.5.15
synthesized review (SHA-256
`dd75e2a2d927356461cdeac0e9bef88b21453b0a15b7a9693224daa9c7e51b68`)
as the incoming disposition ledger. No v0.5.16 peer report was consulted.
Findings below were re-derived against the normative prose, CDDL, decision
records, failure map, and required-vector inventory.*

## Executive verdict

**Cut v0.5.17. Do not freeze v0.5.16 or begin the normative companion and
independent reducer from this text.**

v0.5.16 resolves more of its incoming review than the raw blocker count might
suggest. Keep these decisions:

- D-176 gives staged frontiers an applicable consumer relation, includes
  renewal among pending consumers, names `ref-unresolved`, and requires the
  late suffix re-fold. One earlier CDDL sentence still needs narrowing from
  “all stages” to “all applicable stages.”
- D-177 correctly proves that `import-collision` is unreachable as a transfer
  terminal cause and removes the conjunction from the journal wire shape.
- D-179's writer-chosen, tagged `factref` cause is a better model than trying
  to order tenant and proof facts at a control-log coordinate.
- D-180 chooses the constructible revocation law: total authorship closure plus
  a state-derived empty decryptable-wrap domain, with rotation references as
  linkage rather than coverage proof.
- D-181 fixes certificate/grant compatibility in the right direction: the
  relation is an upper bound, so a current renewal may use its inherited grant
  while an old certificate may not use a post-cessation grant.
- D-182 closes the recovery-portable cross-role key hole by checking each
  candidate point under both closed v1 role tags.
- D-183 puts the accepted-predecessor Frontier transition into §4.6 and repairs
  the adoption and `factref` CDDL shapes.

Two incoming topics are substantively closed: **D-177's reachability ruling
and D-182**. D-176's reducer defect is closed modulo one contradictory CDDL
phrase. The intended reducer choices behind D-179, D-180, D-181, and the
Frontier part of D-183 are also sound and should survive the next cut.

Six protocol/exactness clusters still block freeze:

1. **D-178 still keys erasure precedence on unrecorded acceptance history.**
   A cold fold cannot prove that source equality happened before erasure, and
   identical portable authority/feed inputs can require opposite results based
   only on arrival order; any later journal divergence is an effect of that
   disagreement, not evidence resolving it.
2. **D-179 has no terminal-first dependency machine and its cause map is not
   total.** An `XferAbort` with an unheld basis can lose its journal slot to a
   second terminal before its fact arrives; `XferReopen` has the same
   reservation gap, while several permanent negatives have no legal basis
   rule.
3. **D-180 leaves both revocation-completion laws live** in the same registry
   row and again in CDDL comments.
4. **D-181's proven-incompatibility lifecycle is absent from §10.5.** Its
   §4.2/§10.2 parenthetical mirrors also omit revocation from the full formula,
   a high ambiguity to clean up in the same pass.
5. **D-183 fixes which Frontier head is removed but not whether `last_known`
   must be terminal.** §4.6 and the unheld-input clause retain terminal-head
   language beside the explicit held-chain-membership rule.
6. **E10 remains false**, most clearly for erased pending imports and journal
   invariant failures; several other pending compounds and citations still
   name a disposition without one exact closed outcome.

There are also small but dangerous residue clauses: the staged-frontier CDDL
first says “all” stages before narrowing to applicable stages, the normative
`mimport` comment resurrects D-177's deleted `(winner, freeze_basis)` terminal,
and the main journal prose still calls a tagged invalidation an operation hash.
These are easy edits, but their presence demonstrates why the final
discrepancy sweep cannot yet be delegated to fixtures.

The source itself correctly keeps Gate A false
([status](/Users/vm/owner-plane-d0a-spec.md:3689)). That status should remain
false until the rules below have one answer in prose, CDDL, outcomes,
dispositions, and vectors.

## Disposition ledger

| Topic | What v0.5.16 genuinely closes | Remaining disposition |
|---|---|---|
| D-176 staged frontiers | Applicable consumer scope, renewal waiting, exact `ref-unresolved` outcome, and bounded suffix re-fold are sound | **Core closed; one high mirror defect.** CDDL still first says the consumer takes “ALL” unconsumed stages |
| D-177 collision terminal | The reachability proof is correct; scalar `factref` is again sufficient | **Core closed; one high mirror defect.** Delete the old conjunction from `mimport` CDDL commentary |
| D-178 erasure wins | Correctly rejects the carrier-less v0.5.15 binding and rejects signer-authoritative leaf content | **Hard security/recovery blocker.** “Accepted before erasure” is itself uncarried and cross-feed order is absent |
| D-179 transfer journal | Writer-chosen sufficient cause and tagged op/statement references remove the impossible minimal-hash/control-coordinate rule | **Hard lifecycle/schema blocker.** Terminal-first citations do not reserve an interval, the cause map is incomplete, and exact outcomes plus the ordered-journal transport obligation remain open |
| D-180 revocation | State-derived empty wrap domain is deterministic and constructible; control exclusion remains distinct from local Fence activation | **Hard normative contradiction.** The old reference-coverage/completion rule remains live beside the new rule |
| D-181 certificate × grant | The upper-bound relation admits inherited grants and rejects old-C/new-G | **Hard lifecycle exactness.** §10.5 lacks the proven-permanent context; the supersession-only mirror examples are high ambiguity |
| D-182 material identity | Candidate-side two-tag enumeration makes opaque retired hashes portable; D-172 overlap uses the same relation | **Closed.** Pin the accepted P-versus-minus-P residual in vectors/threat text |
| D-183 Frontier | §4.6 now removes the effective accepted predecessor, matching §9.3 | **Hard predicate exactness.** Terminal-head language survives in the core definition and dependency clause; the no-predecessor no-op also has one contrary phrase |
| D-183 E10 sweep | Some ruling text now names intended lifecycles | **Gate blocker.** Several names are dispositions or prose states, not members of the closed outcome enum |

## Freeze blockers

### B1. D-178 moves the missing carrier from source equality to acceptance history

Rejecting v0.5.15's unspecified “stored binding” was correct. The replacement
does not remove the need for durable evidence; it changes the fact that needs
to be evidenced.

The live rules jointly say:

- every import is always checked against the source-derived record
  ([validator](/Users/vm/owner-plane-d0a-spec.md:2648));
- bundles are never persisted
  ([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:2687));
- the signed Merkle leaf is deliberately not authoritative by itself, because
  an export-authorized signer could substitute record content
  ([posture](/Users/vm/owner-plane-d0a-spec.md:2656));
- an import accepted but not effect-final remains an unresolved attempt and
  survives source erasure, while a proof-pending import becomes negative “at
  the erasure itself”
  ([recovery rule](/Users/vm/owner-plane-d0a-spec.md:1213)); and
- the `mimport` proof is nevertheless described as surviving source erasure
  and rebuild
  ([CDDL claim](/Users/vm/owner-plane-d0a-spec.md:4521)).

No durable object proves the historical predicate “source equality was
successfully checked before erasure.” After erasure, the carried verifier
material is the import, its Merkle path, and the signed release/root. Those
bytes prove that the exporter committed to the carried leaf; they do not prove
that the leaf equalled the real source record. That is precisely the authority
the specification says not to infer.

The convergence failure is stronger than a missing crash note. Consider the
same import I, delayed receipt/lease statement P, source-erasure fact E, and
control bytes:

1. Replica A holds P, accepts I, but I is not yet effect-final; E then erases
   the source. D-178 says I remains an unresolved accepted attempt and stands.
2. Replica B holds I pending, sees E, and later receives the same P. D-178 says
   E resolved I negative and P is forever inert.

The portable operation, statement-feed, tombstone, and control inputs can be
identical before recovery emits a derived terminal. Complete local journals
may then differ—A defers while B may append an Abort—but that difference is an
output of the disputed arrival-sensitive rule, not a durable byte proving
pre-erasure acceptance. Arrival order is not authority; `created_hlc` is
explicitly chronology-only
([operation header](/Users/vm/owner-plane-d0a-spec.md:550)). A fresh fold has
no byte from which to select A's answer rather than B's. This is the same defect
class the specification repeatedly rejects elsewhere: an acceptance-history
fact that a fresh reducer cannot reconstruct.

The local critical section does not supply the missing boundary. It serializes
destination replay observation, import completion, and source terminal append
per release
([critical section](/Users/vm/owner-plane-d0a-spec.md:1204)); the physical erase
machine separately destroys the old KEK and writes tombstones
([erase transition](/Users/vm/owner-plane-d0a-spec.md:1051)). The text does not
make source validation/acceptance and erasure one atomic ordered transition.

Choose one portable law:

1. Add a versioned, authenticated source-validation/acceptance record, durable
   atomically with the attempt, whose authority and cold-rebuild verification
   are explicit. Serialize that record with the erasure boundary and require
   D0-B to carry the same bytes.
2. Make erasure dominate every import that lacks a distinct durable
   completed-import/effect record. In that design, “accepted but not
   effect-final” and the attempt's ordinary ItemCommit are not enough to
   survive; the new completion record is the portable line.

Either choice also needs an exact closed outcome for the killed import. Calling
it “negative” or “resolved-negative” does not satisfy E10. Vector both
proof-before-erasure and erasure-before-proof schedules, then cold-rebuild from
their final portable bytes; do not test only the latter schedule already named
in family 11.

### B2. D-179 needs a terminal-first reservation machine

The writer-chosen cause decision is a real improvement. One tagged, sufficient
branch-relative fact is reconstructible without inventing a total order across
control, tenant, and issuer feeds; reopening when that recorded fact dies is
also monotone even if another sufficient fact exists
([cause rule](/Users/vm/owner-plane-d0a-spec.md:1269)). The `factref` union
correctly distinguishes operation hashes from statement IDs
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4363)).

The dependency machine is incomplete at the moment it matters most. An
`XferAbort.basis` is only described as “fact-held”; no rule says what a replica
does when the terminal arrives before that fact. `XferReopen` does say that an
unheld invalidation pends
([reopen rule](/Users/vm/owner-plane-d0a-spec.md:1318)), but it names neither an
exact outcome nor an interval reservation.

A direct trace violates the one-terminal invariant:

1. Abort T0 for interval n arrives citing fact F from another feed; F is not
   held, so T0 cannot yet be verified.
2. Recovery treats n as unterminated and appends another terminal T1.
3. F arrives and validates T0.
4. The same journal now contains two terminals in interval n, which the
   specification calls a storage invariant violation
   ([interval rule](/Users/vm/owner-plane-d0a-spec.md:1330)).

Two pending reopens have the analogous problem. Arrival of a dependency must
not retroactively turn a previously ignored record into a conflicting
transition.

The cause function also still falls short of its “total” claim
([cause map](/Users/vm/owner-plane-d0a-spec.md:1285)). It assigns every
`scope-*` result to a boundary/retirement operation, but an immutable grant
whose own tenant, zone, space, verb, or kind scope is wrong can fail without
either fact. The same basis question is unanswered for static `no-flow`, while
proven branch-relative `cert-superseded` needs its certificate-ending control
fact and contextual `no-cert` needs an explicit missing-versus-final split.
Because basis presence is the wire discriminator between intrinsic and
reopenable terminal causes, these omissions can make an otherwise valid Abort
either unencodable or unverifiable.

Publish an outcome-and-context table that says whether each terminal-eligible
negative has a required or forbidden basis, its `factref` kind, the sufficient
fact, and its invalidation rule. Static wrong-grant scope and `no-flow` can be
declared basis-free if that is the intended immutable posture; the text must
say so rather than forcing a nonexistent boundary hash.

Specify one rule for both record types:

- a held `XferAbort` or `XferReopen` with any unheld `factref` resolves to an
  exact pending outcome, likely `ref-unresolved`;
- the earliest such journal record reserves `(release_op, incarnation)` and
  blocks every later terminal/reopen transition and effect in its suffix;
- dependency arrival re-folds that bounded suffix;
- a held but invalid citation maps to an exact corruption outcome, likely
  `log-corrupt` → storage-quarantine; and
- vectors cover Abort-first, Reopen-first, two-pending-records, invalid
  op-kind and stmt-kind references, dependency dissolution, and fresh rebuild.

One boundary contract should be frozen with this repair. D-179 says the
writer's cause is canonical because “the journal is replicated log bytes”
([rationale](/Users/vm/owner-plane-d0a-spec.md:1273)), while synchronization and
replica guarantees are deferred to D0-B
([scope](/Users/vm/owner-plane-d0a-spec.md:45)). D0-A need not define transport,
but it must define the semantic object D0-B transports: one authenticated
append authority, exact record order, interval derivation or an explicit
terminal incarnation, and a requirement that replicas preserve that stream
verbatim. Otherwise writer choice is canonical only on the source box, not a
portable reducer input.

### B3. D-180 leaves two revocation machines live

The selected state-derived model is sound at the portable control layer. An
accepted exclusion rotation removes its zone from the target's
decryptable-wrap domain; a later `c.wrap_add` re-enters the zone before compound
completion. The revocation completes when authorship cutoffs are total and the
domain is empty. Rotation references can then serve typed linkage and the
hosted exclusion-freeze ceremony without pretending to prove coverage.

The operation registry still begins with the superseded model. In the same
`c.revoke_device` row it says:

- certificate/grant/cutoff effects take effect when every referenced rotation
  is accepted;
- references plus continuations must cover the decryptable-wrap domain; and
- incomplete reference coverage pends.

Later in that row it says the compound completes only when authorship cutoffs
are total **and** the state-derived wrap domain is empty, and that references
never discharge coverage
([registry row](/Users/vm/owner-plane-d0a-spec.md:1423)). Appendix A repeats the
conflict: the cutoff commentary says `rotation_refs` cover wrap zones and that
authorship/wrap coverage are checked independently
([old CDDL rule](/Users/vm/owner-plane-d0a-spec.md:3976)), while the field's own
comment says typed linkage, never coverage proof
([new CDDL rule](/Users/vm/owner-plane-d0a-spec.md:3995)).

This changes state, not terminology. Let the target have authored in zones A
and B. Accepted rotations exclude it from every decryptable zone. A
`c.revoke_device` cites the rotations but carries only A's authorship cutoff.
The opening rule can activate certificate/grant/cutoff effects after the
references resolve; D-180 keeps the compound pending until B's cutoff arrives.

Rewrite the row from one law rather than appending another amendment. Define
the completing position as the first control position at which:

1. every carried typed linkage is held and valid;
2. authorship-domain frontier coverage is total; and
3. the state-derived decryptable-wrap domain is empty.

Then assign exact outcomes and lifecycles to an unheld reference, an invalid or
stale reference, incomplete authorship coverage, and a still-nonempty wrap
domain. Sweep the old rule from the CDDL comments, `c.revoke_zones` wording,
and the D-50/D-159 amendment summaries. Preserve the already-correct
distinction between portable control exclusion and per-replica cryptographic
activation at Fence.

### B4. D-181 fixes the relation but not its lifecycle or full domain

The canonical rule is now correctly upper-bounded: reject an operation using
certificate C and grant G iff G was issued strictly after C ceased being
effective through supersession **or revocation**
([authority rule](/Users/vm/owner-plane-d0a-spec.md:1681)). This admits the
ordinary `C0/G0 → C1/G0` renewal case, admits a co-issued pair, and rejects old
C0 with later G1. Retain it.

Two normative surfaces still disagree about the result. The same paragraph
says missing renewal evidence pends while a proven incompatible pair is
reject-permanent
([split](/Users/vm/owner-plane-d0a-spec.md:1692)). §10.5 lists
`cert-superseded` only under pending-dependency and omits it from
reject-permanent
([disposition map](/Users/vm/owner-plane-d0a-spec.md:2380)). A literal reducer
may therefore retain a proven-dead operation forever; another may release its
reservation permanently.

Use a contextual starred entry on both rows or split the outcome. The important
property is that missing evidence and proven incompatibility have distinct,
closed lifecycles on the authoritative map, not only in nearby prose.

Two mirrors give only the supersession case. §4.2's renewal discussion speaks
of a superseded certificate using a post-supersession grant
([renewal mirror](/Users/vm/owner-plane-d0a-spec.md:361)); the §10.2 cert stage
uses the same case as its parenthetical example
([pipeline mirror](/Users/vm/owner-plane-d0a-spec.md:2277)). The main formula
includes revocation. These passages are not as strong a contradiction as the
§10.5 lifecycle error—the first is scoped to renewal and the second can be
read as an example—but repeating the full formula removes an avoidable
security-sensitive ambiguity. Define “ceased effective” as the completing
control position of either supersession or revocation and repeat it on all
three surfaces. Vector both causes with evidence-held and evidence-missing
orders.

### B5. D-183 fixes Frontier removal but leaves terminality as a second predicate

The actual retirement transition is now aligned. §4.6 and §9.3 both remove the
effective accepted head at or below the held named position
([Frontier](/Users/vm/owner-plane-d0a-spec.md:628);
[writer rule](/Users/vm/owner-plane-d0a-spec.md:2135)). This closes the
v0.5.15 displaced-head leak.

It does not complete the requested sweep. The authoritative cap and admission
rules say `last_known` requires held canonical **chain membership, never
terminality**
([cap eligibility](/Users/vm/owner-plane-d0a-spec.md:1528);
[admission](/Users/vm/owner-plane-d0a-spec.md:2143)). Two live passages still
use terminality as though it were the input predicate:

- §4.6 calls the incorporated position a prior generation's “terminal head”
  ([Frontier prose](/Users/vm/owner-plane-d0a-spec.md:628));
- the unheld-dependency rule lists a “terminal Head” as the required input
  ([dependency rule](/Users/vm/owner-plane-d0a-spec.md:1543)).

The adjacent effect-finality phrase “incorporated generation's terminal head”
can reasonably mean the head made terminal by the incorporation cap rather
than a precondition
([effect finality](/Users/vm/owner-plane-d0a-spec.md:452)); it is not a third
demonstrated predicate. Naming the immutable incorporation position H there
would nevertheless remove the ambiguity.

Take a held chain `… H5 → H6` and W with `last_known = H5`. Under the deliberate
chain-membership rule, W is eligible, caps the suffix beyond H5, can accept,
and retires the effective accepted predecessor at or below H5. Under the
terminal-head wording, H5 is nonterminal because H6 is held, so W rejects or
pends. This is exactly the arrival-order/stratum circle D-144 was meant to
eliminate.

Use held canonical chain membership for `last_known` eligibility, name the
immutable incorporation cap/position in effect-finality, and call an unheld
dependency the named Head or chain prefix—never a terminality predicate. In
the same sweep, change §4.6's “never nothing” wording: §9.3 and the vectors
correctly define no accepted predecessor at or below H as a successful no-op
([vector inventory](/Users/vm/owner-plane-d0a-spec.md:3286)). The intended
sentence is “never the absent displaced head; if no accepted predecessor
exists, retirement is a no-op.”

### B6. D-183's E10 claim is not true yet

E10 requires every parse or validation failure to map to one closed outcome
**and** one disposition
([requirement](/Users/vm/owner-plane-d0a-spec.md:147)). D-183 says the prior
outstanding states are now mapped
([decision](/Users/vm/owner-plane-d0a-spec.md:3656)), but several mappings name
only an English state or a disposition:

1. A pending import killed by erasure becomes “negative” or
   “resolved-negative”
   ([validator](/Users/vm/owner-plane-d0a-spec.md:2651)). Neither is in the
   closed outcome enum, and `source-erased` is only an Abort reason.
2. Proven `cert-superseded` is permanent in prose but only pending in the
   disposition table, as B4 details.
3. Wrong/duplicate incarnation, a second terminal, and reopen-after-Done map
   only to the **disposition** storage-quarantine
   ([interval machine](/Users/vm/owner-plane-d0a-spec.md:1330);
   [CDDL mirror](/Users/vm/owner-plane-d0a-spec.md:4429)). Assign `log-corrupt`
   or introduce a journal-invariant outcome.
4. An unheld reopen citation merely “PENDS”; an unheld Abort basis has no rule
   at all. Invalid held citations also need an exact corrupt outcome.
5. A judgment, pin, or erase request targeting a provisional import is called
   pending-dependency without an exact outcome
   ([ownership rule](/Users/vm/owner-plane-d0a-spec.md:2812)).
6. An incomplete state-derived revocation compound has a disposition arrow in
   the registry but no exact outcome distinguishing missing input from proven
   invalid linkage or incomplete domain state.

Do one closed-map pass rather than repairing these ad hoc. For every pending
state, record: outcome, disposition, reservation scope, release event, suffix
re-fold, and hardening/permanence transition. For every storage invariant,
record the outcome before the disposition. Then make §10.5 the authoritative
exhaustive map and have registry rows reference it.

## High-value cleanup after the blockers

These are not new design debates, but they should ride v0.5.17 because each
stale sentence points an implementer at a deleted rule:

- The staged-frontier CDDL first says a consuming operation takes “ALL its
  unconsumed stages”
  ([old phrase](/Users/vm/owner-plane-d0a-spec.md:4150)), immediately before
  saying it takes only stages in its applicable coverage domain
  ([new rule](/Users/vm/owner-plane-d0a-spec.md:4154)). Change the first phrase
  to “all applicable unconsumed stages.”
- The `mimport` CDDL comment still says an import collision's terminal cause is
  `(winner, freeze_basis)`
  ([stale comment](/Users/vm/owner-plane-d0a-spec.md:4550)). D-177 proves that
  terminal unreachable, and scalar `factref` cannot encode the conjunction.
  Delete the clause; keep collision only as the frozen-owner fold outcome.
- The main reopen prose still calls `invalidation` “the op hash”
  ([stale prose](/Users/vm/owner-plane-d0a-spec.md:1317)), while the wire type
  permits either an op or statement reference. Say `factref` and vector a real
  statement-kind invalidator.
- The required statement-kind terminal-cause vector needs a reachable trace.
  The current cause map's stable examples are control operations or basis-free
  intrinsic facts. Publish one outcome for which an issuer statement alone is
  sufficient, or limit `stmt` to invalidations and remove the uninstantiable
  terminal-basis vector
  ([vector](/Users/vm/owner-plane-d0a-spec.md:3374)).
- E8 still calls `adopted_renewals` a KEM-renewal pattern
  ([cap table](/Users/vm/owner-plane-d0a-spec.md:131)); the repaired CDDL
  correctly includes every certificate renewal.
- Pin the explicit P-versus-minus-P material-identity residual in the threat
  table and an acceptance/residual vector. Exact SEC1-byte equivalence is a valid
  chosen rule, but scalar negation gives control of the opposite point and the
  limit should be test-visible, not prose-only.
- E7 lists `xferabort.missing` twice under two descriptions
  ([set keys](/Users/vm/owner-plane-d0a-spec.md:95)). Harmless, but easy to
  remove during the canonical-shape sweep.

## Repairs to retain

### D-176's reducer repair is sound; one CDDL phrase is not

The new relation is well typed. An advance or retirement consumes stages for
the zone's live lineages; a renewal consumes only stages in its predecessor's
authorship domain. Renewal joins the required-coverage waiting set, the exact
outcome is `ref-unresolved`, and late acceptance re-folds the bounded consumer
suffix
([state machine](/Users/vm/owner-plane-d0a-spec.md:1764);
[pending rule](/Users/vm/owner-plane-d0a-spec.md:1787);
[CDDL mirror](/Users/vm/owner-plane-d0a-spec.md:4154)).

In the decisive trace, lineage B's renewal neither consumes nor waits on
lineage A's staged close; A's applicable consumer waits and later consumes it.
Incremental and fresh folds reach the same state. Keep the lenient-advance,
retirement, renewal, unrelated-lineage, dead-lineage-vacuous, and late-carrier
vectors together. The preceding “ALL its unconsumed stages” CDDL sentence is a
high mirror contradiction, not a reason to change this reducer decision; make
it say “all applicable” before calling D-176 fully closed.

### D-177's semantic ruling is closed

The reachability proof is convincing. If a winner has imported the record,
`XferDone` counts it; if its attempt is unresolved, the transfer terminal
defers. A collision loser is therefore never a missing source record
([cause map](/Users/vm/owner-plane-d0a-spec.md:1294)). Ownership reservation
also now releases only claimants that are permanently incapable of winning
([ownership](/Users/vm/owner-plane-d0a-spec.md:2790)). Keep the ruling and
delete only the stale CDDL comment.

### D-182 is closed under its stated equivalence boundary

The candidate P is checked under both `H_key({"p256", P})` and
`H_key({"hpke-p256-v1", P})` against opaque retired/adopted sets
([freshness rule](/Users/vm/owner-plane-d0a-spec.md:864)). Appendix A and the
vector inventory repeat the same algorithm
([recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:4270);
[vectors](/Users/vm/owner-plane-d0a-spec.md:3166)). A fresh replica therefore
detects both KEM-as-signing and signing-as-KEM reuse without inverting a hash.
The D-172 terminal-adopted-versus-retired overlap uses that same relation.

Exact SEC1 bytes, not secret-control equivalence, are the explicitly selected
boundary. P versus minus P and arbitrary related-key derivation are accepted
residuals; they merit vectors, not a redesign in this round.

## v0.5.17 closure checklist

Before another freeze review, make all normative surfaces agree on:

1. one durable, portable boundary between source validation, import survival,
   and erasure—or the simpler rule that only a durable completed import marker
   survives;
2. an outcome/context-total journal cause map, terminal/reopen-first pending
   semantics, interval reservation, suffix re-fold, exact outcomes, and the
   authenticated ordered journal object D0-B must transport;
3. one `c.revoke_device` completion law, with typed-link validation and exact
   pending/permanent outcomes;
4. both contextual `cert-superseded` lifecycles and both certificate-ending
   causes, supersession and revocation;
5. held-chain membership as the `last_known` eligibility predicate, the named
   incorporation position in effect-finality/dependency prose, and the
   no-predecessor no-op; and
6. an exhaustive E10 pass over erasure, provisional ownership, revocation, and
   journal invariant/citation states.

Then run a literal residue search for at least `ALL its unconsumed stages`,
`freeze_basis`, `terminal head`, `terminal Head`, reference coverage, `op hash`
near invalidation, `storage-quarantine`, `resolved-negative`, and `KEM
renewals`. The current revision's remaining contradictions are overwhelmingly
old clauses left beside correct new ones.

One editorial-architecture improvement would pay for itself now. The giant
registry rows have become amendment logs as well as executable law; the
`c.revoke_device` row contains both D-173 and D-180 because the new rule was
appended rather than replacing the old one. Give each complex operation one
short authoritative transition block with named subpredicates—inputs,
completion, pending cases, permanent failures, and effects—and make the table,
CDDL comments, decision ledger, and vectors point to that block. Historical
decisions can explain what changed without restating a superseded executable
rule. This is not a request to shorten the protocol; it is a way to stop
amendment residue from becoming protocol ambiguity.

## Artifact sequence

The safe sequence remains:

1. ratify and propagate B1–B6;
2. add their counterexamples to the companion schema as the opening fixtures;
3. run the prose/CDDL/outcome/disposition residue sweep;
4. build the independent reducer and differential harness;
5. execute vector families 1–13, then family 14 offline; and
6. perform the final prose↔vector discrepancy audit.

Non-normative fixtures can be drafted now. The normative companion and reducer
should not be asked to choose whether pre-erasure acceptance is portable,
which of two revocation rules is authoritative, or whether an unresolved
terminal reserves its interval.

## Final assessment

v0.5.16 is a worthwhile revision. D-176 supplies the reducer repair the staged
frontier machine needed, with one stale “all stages” CDDL phrase still to
narrow; D-177 removes an unreachable journal complication; D-182 solves the
opaque recovery-hash problem without a wire change. The
writer-chosen journal cause, state-derived revocation domain, upper-bound
certificate relation, and accepted-predecessor Frontier transition are also
the right choices.

The remaining work is narrower than v0.5.15's, but one issue is deeper: D-178
still asks acceptance history to survive without carrying it. D-179 lacks the
dependency reservation that makes its new fact references arrival-invariant.
The other blockers are mostly exactness failures where the correct rule already
exists beside a stale one. That is good news for the next cut, but it is not a
basis for freezing this one.

**Final decision: no-go for freeze, normative companion, or independent core;
cut v0.5.17 and review again.**
