# Synthesized review: D0-A Core + Memory normative specification v0.5.16

*2026-07-13. Adjudicated synthesis of
[*Review: D0-A Core + Memory normative specification v0.5.16*](/Users/vm/owner-plane-d0a-spec-v0.5.16-review.md)
(SHA-256
`a721bd7d36884e4498c83befd897f5f9deaea67a2af5b32053dffda92662941a`)
and
[*Review 2: D0-A Core + Memory specification v0.5.16*](/Users/vm/owner-plane-d0a-spec-v0.5.16-review-2.md)
(SHA-256
`e60d5fbc4f36afdc1d0fe6877efa59dca1d4983442e3a222e46caa022cd27864`),
verified against
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.16,
4,666 lines / 51,743 words / 390,864 bytes (SHA-256
`808b2765b714b4a39fe8dfd02fd8e6bae93cb361af761f0c428d5f159a948abd`).
The v0.5.15 synthesis used as the incoming ledger has SHA-256
`dd75e2a2d927356461cdeac0e9bef88b21453b0a15b7a9693224daa9c7e51b68`.
This document adjudicates the reports against normative bytes and composed
traces; it does not average their verdicts.*

## Executive verdict

**Cut v0.5.17. Do not freeze v0.5.16 or begin the normative companion and
independent reducer from this text.**

The reports are more complementary than their verdicts initially suggest.
Review 2 explicitly declares itself an **insertion audit** and assigns
composed-trace replay and the freeze judgment to this synthesis
([scope](/Users/vm/owner-plane-d0a-spec-v0.5.16-review-2.md:7)). Within that
scope it does valuable work: it confirms the intended D-176–D-183 decisions
were inserted, recognizes three honest withdrawals, and identifies the new
rules the next cut should preserve.

The condition on its artifact recommendation—“if the synthesis's replay
concurs”—is not met
([recommendation](/Users/vm/owner-plane-d0a-spec-v0.5.16-review-2.md:77)).
Composed replay and a negative residue sweep show:

- **D-176's reducer decision is sound**, but “consume every stage” survives in
  CDDL and the old decision row beside “consume only applicable stages.”
- **D-177's reachability ruling is sound**, but the normative `mimport`
  commentary still resurrects its deleted `(winner, freeze_basis)` terminal.
- **D-182 is substantively closed** under exact SEC1-point equality, with the
  deliberately accepted P-versus-minus-P residual still needing a threat entry
  and vector.
- **D-178 is not closed.** It replaces one carrier-less fact with another:
  whether source equality was checked and the import accepted before erasure.
- **D-179's writer-chosen tagged cause is the right model**, but terminal-first
  dependency reservation and a total cause table are absent.
- **D-180's state-derived revocation law is the right law**, but the withdrawn
  reference-coverage law remains live in the same registry row and CDDL.
- **D-181's upper-bound compatibility relation is right**, but §10.5 still
  gives every `cert-superseded` result the pending lifecycle and omits the
  proven-permanent context.
- **D-183 repairs which Frontier head is removed**, but leaves terminal-head
  language beside the explicit never-terminality predicate and leaves the
  no-predecessor no-op contradictory.
- **E10 is still false.** Several alleged mappings are prose states or
  dispositions, not members of the closed outcome enum.

The synthesized disposition is therefore five hard protocol/exactness
clusters—D-178, D-179, D-180, D-181 lifecycle, and D-183 Frontier—plus the E10
Gate blocker. D-176 and D-177 need bounded normative mirror cleanup, not
redesign. D-182 is closed under its stated boundary.

The source itself agrees that Gate A remains false: the companion, corpus,
surface runs, and discrepancy audit do not exist
([status](/Users/vm/owner-plane-d0a-spec.md:3689)). The peer's eventual artifact
order is right; starting it from v0.5.16 is not.

## Assessment of Review 2

### What the peer review establishes well

The peer report is unusually disciplined about its method. It does not pretend
that locating an insertion proves a composed state machine; it expressly makes
that the synthesis's job. Its substantive confirmations are useful:

- Applicable-scoped staged consumption and renewal waiting appear in the main
  reducer and vectors.
- Collision terminality has a sound unreachability proof, which is preferable
  to enlarging the wire shape for a nonexistent trace.
- The v0.5.15 source-binding proposal really was carrier-less and deserved
  withdrawal. “Prose storage is not storage” is the correct review standard.
- Replacing cross-feed minimum selection with one writer-chosen, tagged,
  sufficient journal cause is directionally correct.
- State-derived revocation completion is constructible; the old reference
  coverage was ceremonial.
- Certificate/grant compatibility needed an upper bound, not a within-span
  requirement.
- Candidate-side two-tag enumeration solves the opaque recovery-hash problem
  without a wire change.
- The accepted-predecessor Frontier transition, all-renewal adoption wording,
  and `factref` CDDL union were inserted.

The report also keeps its artifact recommendation conditional and retains the
Gate-B/P0.5/tombed-cutover restrictions. Those are good boundaries.

### Why “all discharged” does not follow

The executive conclusion exceeds the declared method in four ways.

1. **Insertion is not replacement.** Review 2 says no live text depends on a
   withdrawn mechanism
   ([claim](/Users/vm/owner-plane-d0a-spec-v0.5.16-review-2.md:60)). Exact old
   clauses remain at the D-176, D-177, D-180, D-179, and Frontier surfaces
   listed below. A positive insertion audit did not establish the negative
   claim.
2. **A prose state is not a portable byte.** The report correctly applies that
   principle to v0.5.15's stored source binding, then treats “accepted before
   erasure” as if acceptance history carried itself.
3. **A disposition is not an E10 outcome.** “Resolved-negative,” “PENDS,” and
   “storage-quarantine” do not become members of §10.4 because D-183's decision
   row says the cases were mapped.
4. **Replicated bytes do not define terminal-first semantics.** Even if D0-B
   later transports the exact journal, D0-A must say how an Abort or Reopen
   behaves before its cited fact arrives and which interval it reserves.

Thus Review 2 is a strong record of intended changes, not evidence of
freeze-readiness. Its own scope caveat tells us how to use it: retain the
decisions it verifies, then let composed replay decide closure.

## Adjudicated disposition ledger

| Topic | Review 2 | Synthesized disposition |
|---|---|---|
| D-176 staged frontiers | Discharged | **Core closed; high mirror cleanup.** The applicable relation is sound, but CDDL and D-153 still say every stage |
| D-177 collision terminal | Discharged | **Core closed; high mirror cleanup.** The terminal is unreachable, but `mimport` still names the deleted conjunction |
| D-178 erasure wins | Discharged | **Hard security/recovery blocker.** Pre-erasure acceptance/source equality has no durable carrier or cross-feed boundary |
| D-179 journal | Discharged | **Hard lifecycle/schema blocker.** Terminal-first reservation, total basis mapping, exact outcomes, and the ordered-journal semantic object remain incomplete |
| D-180 revocation | Discharged | **Hard normative contradiction.** Both reference-derived and state-derived completion remain live |
| D-181 compatibility | Discharged | **Relation closed; hard lifecycle blocker.** §10.5 contradicts the proven-permanent context; revocation mirrors are high ambiguity |
| D-182 material identity | Discharged | **Closed under exact-point equality.** P versus minus P is an accepted but concrete residual to pin |
| D-183 Frontier | Discharged | **Transition choice closed; hard predicate exactness.** Terminality/no-op mirrors still disagree |
| D-183 / E10 | Discharged | **Gate blocker.** The closed enum and disposition map do not contain the claimed mappings |
| Artifact readiness | Begin if synthesis concurs | **No concurrence.** Fix protocol law first; non-normative counterexample drafting only |

## Detailed adjudication

### A1. Retain D-176 and D-177; delete their residue

D-176's main machine is now coherent. A consumer takes stages only inside its
coverage domain; renewal covers its predecessor's authorship domain, renewal
joins the required-coverage pending set, waiting is `ref-unresolved`, and late
acceptance re-folds the bounded suffix
([machine](/Users/vm/owner-plane-d0a-spec.md:1764);
[pending rule](/Users/vm/owner-plane-d0a-spec.md:1787)). The unrelated-lineage
trace therefore converges.

Two older summaries remain. Appendix A first says the next consumer takes
“ALL its unconsumed stages,” then narrows consumption to applicable stages
([CDDL conflict](/Users/vm/owner-plane-d0a-spec.md:4150)). D-153 still says the
next consumer takes “EVERY unconsumed stage” without a D-176 amendment pointer
([decision residue](/Users/vm/owner-plane-d0a-spec.md:3626)). Change both to
“every applicable unconsumed stage.” This is bounded exactness cleanup; do not
reopen the reducer decision.

D-177 is likewise right on reachability. A record owned by a completed winner
counts toward `XferDone`; an unresolved winning attempt defers the terminal.
No collision loser can therefore appear in `missing`
([cause rule](/Users/vm/owner-plane-d0a-spec.md:1294)). The ownership rule and
vectors agree. Yet the `mimport` CDDL comment still says the terminal cause is
`(winner, freeze_basis)`
([stale comment](/Users/vm/owner-plane-d0a-spec.md:4550)), contradicting both
D-177 and the scalar `factref` journal shape. Delete that clause and retain
`import-collision` only as the frozen-owner fold outcome.

One adjacent adoption mirror also survived D-183: E8 still calls
`adopted_renewals` a KEM-renewal pattern
([cap table](/Users/vm/owner-plane-d0a-spec.md:131)), while the repaired rule
includes signing-only certificate renewals. Change the cap description to all
certificate renewals; no semantic redesign is needed.

### A2. D-178's erasure rule still depends on unrecorded history

Review 2 correctly rejects v0.5.15's carrier-less stored binding. It does not
apply the same standard to the replacement.

The current protocol says:

- a destination import accepted but not effect-final survives source erasure
  and keeps the terminal deferred, while a proof-pending import becomes
  negative at erasure
  ([recovery](/Users/vm/owner-plane-d0a-spec.md:1213));
- every import validator still requires source-derived byte equality
  ([validator](/Users/vm/owner-plane-d0a-spec.md:2648));
- the signed leaf alone is not authoritative
  ([posture](/Users/vm/owner-plane-d0a-spec.md:2656)); and
- bundles are never persisted
  ([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:2687)).

Appendix A simultaneously describes the `mimport.proof` field as surviving
source erasure/rebuild and as “the ONLY validator”
([CDDL residue](/Users/vm/owner-plane-d0a-spec.md:4521)). Read literally, that
restores proof-only admission after erasure; read as shorthand for the complete
§11.8 validator, it cannot make unavailable source equality rebuildable. Sweep
that comment with the substantive repair.

No frame, transaction record, or authenticated operation carries “source
equality passed before erasure.” The admission pipeline makes the composed
counterexample especially direct: time/lease validation runs before body
invariants, where source equality lives
([precedence](/Users/vm/owner-plane-d0a-spec.md:2296)). For the same import I,
delayed proof statement P, and erasure E:

1. P arrives, I passes source equality and accepts but waits on effect finality,
   then E occurs. The rule says I stands and the source terminal defers.
2. E occurs while I waits for P, then P arrives. The rule says I resolved
   negative at E and P is inert.

The portable admission inputs can be identical. A later local Abort in the
second history is a derived consequence of choosing that result, not a byte
that proves whether source equality had already passed. Arrival order and HLC
are not authority. A cold reducer rebuilding the first history after source
erasure has no evidence selecting the first answer.

This is the same acceptance-history defect class the specification removes
elsewhere. The existing vector covers pending → erase → proof, but not proof →
accept → erase → cold rebuild
([vector](/Users/vm/owner-plane-d0a-spec.md:3375)).

Select one portable boundary:

- carry a versioned authenticated source-validation/acceptance record, atomic
  with the attempt and serialized with erasure; or
- let erasure kill every import lacking a distinct durable completed-import
  effect record. Accepted-but-not-effect-final is then insufficient.

Name the killed import's exact outcome and vector both schedules plus cold
rebuild. “Negative” is not an E10 result.

### A3. D-179's cause representation is better; its journal machine is incomplete

Keep the writer-chosen sufficient cause and tagged `factref`. It avoids the
impossible attempt to place tenant and statement facts at a control coordinate,
and the CDDL cleanly distinguishes operation hashes from statement IDs
([factref](/Users/vm/owner-plane-d0a-spec.md:4363)).

The terminal-first path is undefined. `XferAbort.basis` must be fact-held, but
only `XferReopen` says an unheld citation pends
([terminal rule](/Users/vm/owner-plane-d0a-spec.md:1269);
[reopen rule](/Users/vm/owner-plane-d0a-spec.md:1317)). Neither rule says that
the pending record reserves its interval.

Let terminal T0 for interval n arrive before cited fact F. If T0 is treated as
ineffective, recovery may append T1. When F arrives, T0 validates and the
journal now has two terminals in n—the storage violation the interval machine
forbids
([invariant](/Users/vm/owner-plane-d0a-spec.md:1330)). Two pending Reopens have
the same defect.

The cause map is also not total despite saying it is
([map](/Users/vm/owner-plane-d0a-spec.md:1285)). It maps every `scope-*` outcome
to a boundary/retirement op, but an immutable wrong-scope grant can fail without
either. Static `no-flow` has the same question; proven
`cert-superseded` needs its certificate-ending fact; contextual `no-cert` needs
a missing-versus-final split. Basis presence is the wire discriminator between
intrinsic and reopenable causes, so these are schema questions, not comments.

The required vectors also demand a statement-kind terminal cause
([vector](/Users/vm/owner-plane-d0a-spec.md:3374)), but the published map names
no reachable issuer statement independently sufficient to make an import
permanently negative. Either supply that outcome and trace, or reserve statement
references for roles they can actually perform—such as invalidation—and
remove or narrow the terminal-cause vector.

Define:

1. the exact pending outcome for an unheld Abort/Reopen factref;
2. reservation of `(release_op, incarnation)` by the earliest pending journal
   transition, blocking the suffix until resolution;
3. bounded suffix re-fold when the fact arrives;
4. `log-corrupt` or another exact outcome for a held-invalid citation; and
5. an outcome/context table stating for every terminal-eligible negative
   whether basis is required or forbidden, its factref kind, and invalidation.

The main prose must also stop calling `invalidation` an operation hash when the
wire type admits a statement ID
([mirror](/Users/vm/owner-plane-d0a-spec.md:1317)). D0-B may own transport, but
D0-A must freeze the semantic journal object it expects transport to preserve:
authenticated bytes, one append authority, exact order, preservation, and
explicit or derivable interval identity.

### A4. D-180 contains both the old and new completion rules

The selected state-derived law is the right one: revocation completes when
authorship cutoffs are total and the target's decryptable-wrap domain is empty.
Accepted exclusions shrink that domain; a later wrap re-admits the zone before
completion. References are typed exclusion-freeze linkage, not coverage proof.

The `c.revoke_device` row still begins by saying referenced rotations trigger
cert/grant/cutoff effects, references and continuations must cover the wrap
domain, and incomplete reference coverage pends. Later in the same row it says
completion is state-derived and references never prove coverage
([two rules](/Users/vm/owner-plane-d0a-spec.md:1423)). CDDL repeats “rotation
refs cover the WRAP zones” and dual coverage before declaring the field linkage
only
([old CDDL](/Users/vm/owner-plane-d0a-spec.md:3976);
[new CDDL](/Users/vm/owner-plane-d0a-spec.md:3995)).

The continuation shape remains ambiguous too: `c.revoke_zones` still advertises
further rotation references
([registry](/Users/vm/owner-plane-d0a-spec.md:1424);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4015)), directly beside the claim that
no reference continuation exists or is needed. Optional continuation of typed
audit/freeze linkage could be coherent; continuation as coverage cannot be.
Choose one description explicitly.

A target authored in A and B, is excluded everywhere, and then receives a
revocation carrying only A's cutoff. Read literally, the opening rule can
activate effects as soon as the references resolve; D-180 keeps the compound
pending for B's cutoff. Same bytes, different authority state.

Rewrite the operation from one law. Separately state that exclusion rotations
are independently effectful, while the compound's cert/grant/cutoff effects
begin only when linkage validation, total authorship closure, and empty
decryptable-wrap state all hold. Give missing linkage, invalid linkage,
incomplete authorship coverage, and a nonempty wrap domain exact outcomes.

### A5. D-181's relation closes; its lifecycle does not

The upper-bound formula is correct. Reject iff grant G was issued strictly
after certificate C ceased being effective; current C1 with inherited G0
admits, while old C0 with later G1 dies
([formula](/Users/vm/owner-plane-d0a-spec.md:1681)). Keep it.

The same rule says a proven incompatible pair is reject-permanent while missing
renewal evidence pends
([contexts](/Users/vm/owner-plane-d0a-spec.md:1692)). §10.5 places
`cert-superseded` only under pending-dependency and nowhere under
reject-permanent
([map](/Users/vm/owner-plane-d0a-spec.md:2380)). Contrary to Review 2's claim,
there is no applied star pattern for this outcome. One reducer may retain a
proven-dead pair; another may release it permanently.

Add contextual entries to both rows or split the outcome. The §4.2 and §10.2
parentheticals mention only supersession while the canonical formula also
includes revocation
([renewal example](/Users/vm/owner-plane-d0a-spec.md:361);
[pipeline example](/Users/vm/owner-plane-d0a-spec.md:2277)). Those can be read as
examples rather than exhaustive rules, so this is high mirror ambiguity, not a
second hard blocker. Repeat the complete formula and define the completion
coordinate of both certificate-ending events.

### A6. D-183's Frontier and E10 sweeps are incomplete

The accepted-predecessor transition itself is repaired. §4.6 and §9.3 now
remove the effective accepted head at or below the named held position
([Frontier](/Users/vm/owner-plane-d0a-spec.md:628);
[writer rule](/Users/vm/owner-plane-d0a-spec.md:2135)). Retain that transition.

The eligibility predicate still has two descriptions. The authoritative rule
requires held canonical chain membership and expressly says never terminality
([eligibility](/Users/vm/owner-plane-d0a-spec.md:1528)), while §4.6 calls the
input a prior generation's “terminal head” and the dependency list calls an
unheld input a “terminal Head”
([dependency mirror](/Users/vm/owner-plane-d0a-spec.md:1543)). A literal
validator can still reject W naming held H5 when held successor H6 makes H5
nonterminal; the intended rule accepts W and lets its cap truncate H6.

§4.6 also says the displaced named head retires its accepted predecessor
“never nothing,” while §9.3 and the vectors define no accepted predecessor as a
successful no-op
([no-op vector](/Users/vm/owner-plane-d0a-spec.md:3286)). Replace terminality
with held chain membership for eligibility, name immutable incorporation
position H in effect-finality, call an unheld dependency the named Head or
chain prefix, and state the no-op exactly.

E10 independently remains false. It requires a closed outcome plus disposition
for every failure
([E10](/Users/vm/owner-plane-d0a-spec.md:147)), but:

- erasure changes a pending import to “negative”/“resolved-negative,” neither
  an enum member
  ([erasure rule](/Users/vm/owner-plane-d0a-spec.md:2651));
- proven `cert-superseded` is missing from the permanent map;
- wrong/duplicate incarnation, a second terminal, and reopen-after-Done name
  only the storage-quarantine disposition, not an outcome
  ([journal CDDL](/Users/vm/owner-plane-d0a-spec.md:4429));
- an unheld Reopen merely “PENDS,” while an unheld Abort basis has no rule; and
- judgment/pin/erase against a provisional import names pending-dependency but
  no exact outcome
  ([ownership](/Users/vm/owner-plane-d0a-spec.md:2812)).

D-183's decision row saying these cases are mapped is a claim about the text,
not the missing mapping itself
([decision](/Users/vm/owner-plane-d0a-spec.md:3656)). Put the outcomes and
contextual lifecycles into §10.4/§10.5 and make those tables authoritative.

## D-182 and the accepted P-versus-minus-P residual

D-182 closes the stated portable comparison problem. A fresh replica checks
candidate P under both closed v1 algorithm tags against opaque retired/adopted
sets
([rule](/Users/vm/owner-plane-d0a-spec.md:864);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4270)).

Exact SEC1-byte identity deliberately treats P and minus P as different. This
is a concrete secret-control residual: a holder of scalar d can derive the
scalar for minus P. The owner has selected exact-point equality, so the
synthesis does not reopen D-182. It does require an acceptance/residual vector,
a §14 threat entry, and qualification of broad prose such as “one key is one
device” or “never returns.” If the intended security property is instead
secret-control equivalence for this simple relation, canonicalizing the P-256
point by x-coordinate would be a separate owner decision.

## v0.5.17 closure and artifact sequence

Before another freeze review:

1. choose and carry the D-178 pre-erasure survival boundary;
2. finish the D-179 terminal-first reservation and outcome/context cause table;
3. replace—not append to—the D-180 operation rule;
4. publish both D-181 lifecycles in §10.5;
5. remove D-176/D-177/Frontier residue and complete the E10 map; and
6. pin the P/minus-P residual in vectors and the threat table.

Run a literal residue search for `EVERY unconsumed stage`, `ALL its unconsumed
stages`, `freeze_basis`, `terminal head`, `terminal Head`, reference coverage,
`op hash` near invalidation, `resolved-negative`, and bare
`storage-quarantine`, plus `KEM renewals`. The remaining exactness failures are
mostly old clauses left beside correct new decisions.

Then proceed in the peer's proposed order:

1. write the normative companion schema;
2. add the counterexample fixtures, including both D-178 schedules and
   terminal-first delivery;
3. build the independent reducer and differential harness;
4. execute families 1–13, family 14 offline, and the named surfaces; and
5. perform the final prose↔vector discrepancy audit.

Non-normative fixtures may be drafted now. The companion and reducer must not
choose the missing protocol law.

## Final assessment

Review 2 is useful and well scoped. It confirms that v0.5.16 made several good
decisions and, importantly, withdrew mechanisms when their premises failed.
The synthesis adopts those decisions. It does not adopt the zero-findings
verdict, because the peer deliberately did not perform the composed replay on
which that verdict depends.

The sharpest lesson comes from D-178. “Prose storage is not storage” applies
equally to source bindings and to historical acceptance. D-179 needs the
reservation that makes tagged citations arrival-invariant. D-180, D-181, and
D-183 mostly need replacement and closed-map work, not new conceptual designs.
That makes v0.5.17 a focused cut, but still a necessary one.

**Final decision: no-go for freeze, normative companion, or independent core;
cut v0.5.17 and review again.**
