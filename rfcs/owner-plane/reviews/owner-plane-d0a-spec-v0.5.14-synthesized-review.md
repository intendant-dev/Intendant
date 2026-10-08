# Synthesized review: D0-A Core + Memory normative specification v0.5.14

*2026-07-13. Adjudicated synthesis of
[*Review: D0-A Core + Memory normative specification v0.5.14*](/Users/vm/owner-plane-d0a-spec-v0.5.14-review.md)
(SHA-256
`e20beb6a82756e7a96e94c6675e9eb89777e0118c47ae0210ca8938599951256`)
and
[*Review 2: D0-A Core + Memory specification v0.5.14*](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md)
(SHA-256
`633f0888a786beb8cd84aa53db1300367fe2a7e7cc057859c74fcaf76aa32874`),
verified against
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.14,
4,408 lines / 361,874 bytes (SHA-256
`e87bdbdee1406e33d6bc1c604fedc54c876509935d526e982e9bce24b9a833ed`).
The v0.5.13 synthesis used as the incoming ledger has SHA-256
`2588f0a05742129a2c7de3d96483920fb8205adea90790deb9a017ba87e1069a`.
This document adjudicates claims against the normative rules and composed
traces; it does not average the two verdicts.*

## Executive verdict

**Cut v0.5.15. Do not freeze v0.5.14 or begin the normative companion and
independent reducer from this text.**

The peer report is useful and largely accurate within its explicitly narrow
method. It independently confirms that D-160 through D-167 were propagated
into the expected rows, CDDL shapes, decisions, and vector inventory. Its
scope, however, says composed-trace depth was limited and leaves the freeze
judgment to this synthesis
([peer scope](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:3)).
Consequently, its opening claim that all eight clusters are discharged
([peer verdict](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:10))
does not follow from the audit it performed.

The combined disposition is:

- **Eight hard freeze blockers remain:** D-160 staged-frontier composition;
  D-161 import freeze/collision cause; D-162 availability-dependent source
  binding; D-163 journal history and cause totality; D-164 adopted-renewal
  membership/overlap; D-166 authority composition; D-167 cross-role P-256
  material identity; and D-167 `w.gen` Frontier retirement.
- **D-165 is downgraded from the first review's claimed constructibility
  failure to a high exactness prerequisite.** The intended revocation wrap
  domain may be historical “all-decryptable zones,” in which case the
  0-author/65-wrap ceremony works. The specification still needs to name that
  domain and its evaluation position before freeze.
- **The v0.5.14 local repairs are accepted.** In particular, keep
  acceptance-only stage creation, simple arrival-invariant import loser
  disposition, named/versioned `bundleleaf`, citable reopen fields,
  strictly-after-base adoption, empty authorship encoding, selector
  intersection as a direction, and the recovery-universe mirror sweep.
- **Gate A remains false**, also expressly so in the source
  ([status](/Users/vm/owner-plane-d0a-spec.md:3492)).
- **Artifact work should not become normative yet.** It is reasonable to
  draft the counterexample vectors as non-normative fixtures while cutting
  v0.5.15; the companion schema and independent core should follow the
  repaired text, not define missing protocol law themselves.

The peer's “findings: none in scope” statement
([peer finding](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:55))
is too strong even as a wire/mirror claim. Three literal surface
contradictions remain:

1. D-160 says only a strict dependent waits, while retirement coverage is
   mandatory regardless of strictness and the CDDL says a generic dependent
   waits.
2. D-163's main rule selects the minimal sufficient hash, while D-157 and
   the CDDL still say first fact in fold order.
3. D-164's main prose says every adopted entry is KEM-rotating, while its
   decision, CDDL, and vector require signing-only entries in the same list.

Those are representation-audit misses before any deeper reducer analysis.

## Assessment of the peer review

### What it establishes well

The report is independent, concise, and admirably candid about its method. It
correctly verifies that:

- D-160 contains the chosen acceptance-only stage and vacuous-consumption
  language;
- D-161 fixes the simple A/B arrival-order disposition and adds a pending
  earlier-claimant reservation;
- D-162 names and versions `bundleleaf` and demotes whole-bundle rebuilding;
- D-163 physically adds `basis` and `invalidation` to `XferReopen`;
- D-164 corrects the inverted post-base comparison and narrows terminal KEM
  reuse;
- D-165 makes empty authorship encodable and states dual-domain completion;
- D-166 places the intended three-axis equation on its main surfaces; and
- D-167 carries the recovery blanket, post-base exception, typed key IDs,
  cut-key residual, and held-chain predicate to the named mirrors.

These are real closures of the narrow v0.5.13 examples. The peer is also
right that this revision's correction texture—an inverted comparison,
off-by-one encodability, and arrival-relative disposition—is evidence that
the review loop is doing useful work
([peer recommendation](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:58)).
Its proposed artifact order is sensible once the rules are deterministic,
and its reminder that durable P1 writes remain gated by Gate B and the
umbrella prerequisites is correct.

### Where its conclusion outruns its evidence

The audit checks that a ruling appears in the expected places; it does not
show that the ruling composes with the adjacent state machines. Four
distinctions explain nearly every disagreement:

1. **Fields are not historical proof.** A journal can carry `basis` and
   `invalidation` while still lacking the coordinate needed to prove what
   state existed when the terminal was written.
2. **One named validator can still be availability-dependent.** D-162 removed
   the whole-bundle validator but retained source equality only while source
   bytes remain derivable.
3. **A typed hash can use the wrong equivalence relation.** Hashing
   `{alg, pk}` consistently does not detect the same P-256 point reused under
   two algorithm-role tags.
4. **A changed admission predicate must compose with its transition.**
   Allowing held-but-unaccepted `last_known` is incomplete while the
   transition still removes exactly that absent Frontier member.

The report also treats names in §13's vector inventory as if executable
vectors existed. The source says the companion, corpus, and surface runs do
not yet exist. A prose promise to test a trace is valuable design inventory,
not evidence that the trace passes.

The correct interpretation of the peer report is therefore:

> The intended v0.5.14 decisions are visibly propagated across most named
> surfaces.

That does not support “all discharged,” schema freeze, or artifact start.

## Adjudicated disposition ledger

| Topic | Peer conclusion | Synthesized disposition |
|---|---|---|
| D-160 staged frontiers | Discharged | **Hard blocker.** Narrow repair landed; retirement strictness conflicts and late acceptance lacks an explicit incremental suffix re-fold |
| D-161 import ownership | Discharged | **Hard blocker.** Simple arrival invariance landed; revivable earlier claimants do not reserve freeze, and collision terminal cause omits the freeze basis |
| D-162 Merkle/source binding | Discharged | **Hard blocker.** Canonical leaf landed; conditional source equality changes admission after erasure |
| D-163 journal | Discharged | **Hard blocker.** Citation fields landed; no historical coordinate, cause map is not total, collision needs conjunction, and mirrors still disagree |
| D-164 renewal adoption | Discharged | **Hard blocker.** Post-base direction landed; list membership and terminal-key/retired-key overlap remain contradictory |
| D-165 device revocation | Discharged | **High exactness, not a proven constructibility blocker.** Empty authorship and dual coverage landed; historical wrap-domain equation/evaluation position remain unnamed |
| D-166 authority | Discharged | **Hard security blocker.** Equation omits recovery and permits an old certificate to cite a post-supersession grant |
| D-167 key freshness | Discharged | **Hard security blocker.** Algorithm-tagged IDs miss cross-role reuse of identical P-256 material |
| D-167 `w.gen` / Frontier | Discharged | **Hard reducer blocker.** Held-chain admission can name a head absent from the accepted-only Frontier transition |
| Artifact readiness | Begin companion if synthesis concurs | **No concurrence.** Cut v0.5.15 first; draft only non-normative counterexample fixtures meanwhile |

## Detailed adjudication

### A1. D-160 closes its examples but not the stage consumer machine

The peer accurately summarizes acceptance-only creation, strict dependent
pendency, and vacuous consumption
([peer D-160](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:14)).
Those decisions stand.

The normative rules still disagree. Every next epoch advance, renewal, or
retirement consumes every accepted zone stage
([consumer rule](/Users/vm/owner-plane-d0a-spec.md:1685)). D-160 reserves only
a **strict** dependent behind a pending carrier
([stage rule](/Users/vm/owner-plane-d0a-spec.md:1707)). Space retirement,
however, requires frontier coverage regardless of strictness
([registry](/Users/vm/owner-plane-d0a-spec.md:1375);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3822)), while §9.4 says the grouped
operations require it only under strict policy
([§9.4](/Users/vm/owner-plane-d0a-spec.md:2109)). The CDDL staging comment
uses generic “dependent consumer”
([stage CDDL](/Users/vm/owner-plane-d0a-spec.md:3934)).

There is also an incremental transition obligation not stated by the text.
Let earlier carrier P be pending; later renewal R accepts with complete inline
coverage; then P resolves. Control position plus D-153 imply that R is the
consumer of P on a fresh fold. An incremental reducer must revisit R and
materialize P under R's selector. The source says state reconstructs from
positions, but never mandates or bounds this downstream re-fold. A local
“apply P now” reducer leaves P for the next consumer.

Repair: distinguish all stage consumers from operations whose coverage is
mandatory; align retirement; and define the suffix re-fold/reservation
transition with exact outcomes.

### A2. D-161 fixes simple order but misses revivable reservations

The peer correctly confirms that an unfrozen order-loser is always
quarantine-reproposal and that an unresolved earlier claimant reserves the
key
([peer D-161](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:19)).

“Unresolved” is too narrow. A held earlier claimant can be in resolved but
revivable budget quarantine. A constructible handoff is:

1. P accepts under import grant G1.
2. G1 is revoked with a frontier preserving P; G2 is issued.
3. An earlier held budget consumer under G1 resolves inside the preserved
   prefix and displaces P.
4. Q under G2 becomes owner and effect-final. P is not unresolved, so Q may
   freeze.
5. The earlier consumer's proof is retro-disqualified; P's charge revives.

The claimant order says P precedes Q, while Q's freeze basis still stands.
Reservation must cover every held order-earlier candidate not permanently
incapable of winning, or revival must be a defined unfreeze trigger.

The journal cause has a separate, crisp defect. A collision is permanent only
because winner A has live freeze basis F
([ownership](/Users/vm/owner-plane-d0a-spec.md:2660)). D-163 records only A
as the cause
([basis map](/Users/vm/owner-plane-d0a-spec.md:1238)). If recovery removes F
while A remains the provisional winner, loser B changes from collision to
revivable order-loser. A itself has not dissolved, displaced, or
retro-quarantined, so the recorded cause never invalidates. The cause must
encode `(winner, freeze_basis)` or an equivalent typed conjunction.

### A3. D-162's remaining validator is availability-dependent

The peer is right that the selected leaf now has canonical bytes and that
whole-bundle reconstruction is no longer admission law
([peer D-162](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:25)).
The new predicate is still not stable: source-byte equality runs only while
the source record is derivable
([validator](/Users/vm/owner-plane-d0a-spec.md:2536)).

The strongest counterexample requires no scheduler race:

1. Malicious B′ has a valid signed leaf/path but differs from source B.
2. Its durable destination attempt is pending time evidence; the admission
   pipeline has not yet reached body equality because time precedes body
   ([pipeline](/Users/vm/owner-plane-d0a-spec.md:2181)).
3. Source B is erased. Recovery must defer `XferAbort` for the unresolved
   durable attempt
   ([recovery rule](/Users/vm/owner-plane-d0a-spec.md:1184)).
4. The time proof arrives. Body validation now finds no derivable B, skips
   equality, and accepts B′ on its valid path.
5. Replay-first terminal precedence can finish the transfer despite erasure.

The family inventory says B′ never imports before or after erasure
([vector promise](/Users/vm/owner-plane-d0a-spec.md:3211)); the rule admits
exactly the after-erasure schedule.

Preferred repair: retain durable source-binding evidence through erasure.
Making the signed leaf authoritative is a possible but material security
change: an export-authorized signer could substitute statement, kind, and
class-floor bytes for an allowed source. That would require amending the
source-derived bundle contract, not merely removing equality.

### A4. D-163 adds citations but not verifiable history

The peer confirms the two reopen fields and repeats the intended minimal-hash
rule
([peer D-163](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:29)).
Four defects remain:

- `XferAbort` carries no control, tenant, or proof evaluation coordinate
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:4129)), although canonical cause
  is defined relative to the terminal's fold position
  ([main rule](/Users/vm/owner-plane-d0a-spec.md:1238)). A fresh rebuild
  cannot tell whether a later lower-hash sufficient fact existed then.
- The collision cause is conjunctive, as A2 shows; one `bytes32` winner is
  insufficient.
- The cause map omits checkpoint-hardened deadline/lease/causal negatives and
  control-relative `body-invariant`, despite both being terminal-relevant.
- D-157 and the CDDL still select the **first** fact in fold order
  ([D-157](/Users/vm/owner-plane-d0a-spec.md:3449);
  [CDDL](/Users/vm/owner-plane-d0a-spec.md:4142)), contradicting the main
  minimal-hash rule.

This last item is directly inside the peer's claimed mirror scope. Repair
requires either a portable historical coordinate or a monotone cause
construction, plus a typed cause union and total outcome-to-cause map.

### A5. D-164 fixes order but contradicts itself about list members

The peer correctly confirms “strictly after base” and terminal adopted KEM
eligibility
([peer D-164](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:33)).

Main prose says every `adopted_renewals` entry adopts a KEM-rotating renewal
([main rule](/Users/vm/owner-plane-d0a-spec.md:1833)). D-164, CDDL, and the
vector inventory require the bounded list to carry every chain renewal,
including signing-only links
([D-164](/Users/vm/owner-plane-d0a-spec.md:3456);
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4012);
[vector](/Users/vm/owner-plane-d0a-spec.md:3033)). A signing-only entry is
therefore both required and disallowed.

Also, nothing forbids `retired_keys` from containing the terminal adopted KEM
ID. That key is then globally retired and expressly same-device reusable at
once. Choose disjointness or precedence and vector the chosen rule.

The 64-versus-65-link boundary is high exactness rather than a separate hard
blocker: “follow the same discipline” and storage-orphaning plausibly imply
the result, but the residual or continuation should be stated.

### A6. D-165 is an exactness gap, not a proven failed ceremony

This is where the synthesis narrows the first review. The peer correctly
confirms that `cutoffs: [*]` encodes a zero-author device, that authorship and
wrap coverage are independent, and that all three boundary cases are named
([peer D-165](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:35)).

The first review's 0-author/65-wrap counterexample assumed the wrap domain was
current membership, which exclusion rotations shrink before the main revoke.
That reading is plausible because `held_zones` calls itself one definition
for every consumer
([membership](/Users/vm/owner-plane-d0a-spec.md:488)). It is not conclusive:
D-50 calls the revocation obligation all-decryptable-zones coverage
([D-50](/Users/vm/owner-plane-d0a-spec.md:3342)), and the row requires each
rotation to follow the target's last accepted wrap. Under a
historical-wrap/outstanding-obligation domain, all 65 zones remain owed and
the continuation works.

The protocol still says only “every zone the target holds wraps for per
control state”
([registry](/Users/vm/owner-plane-d0a-spec.md:1369)). It does not publish the
historical-wrap equation, snapshot/evaluation position, or re-admission and
abandonment transitions. Gate A requires those to be exact, but the evidence
does not justify calling the ceremony itself unconstructible. Disposition:
**must specify in v0.5.15; do not count as a demonstrated state-machine
failure.**

### A7. D-166 omits an authority axis and permits signer resurrection

The peer confirms that certificate ∩ grant ∩ epoch appears on the intended
surfaces
([peer D-166](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:40)).
That equation is incomplete.

First, the closure algebra defines recovery as global/unqualified and says
every matching closure bounds admission
([closure algebra](/Users/vm/owner-plane-d0a-spec.md:1595)). The named
equation and pipeline list only certificate, grant, and epoch
([equation](/Users/vm/owner-plane-d0a-spec.md:1613);
[pipeline](/Users/vm/owner-plane-d0a-spec.md:2157)). An otherwise-valid H6
beyond a named recovery frontier through H5 is rejected by the general rule
and admitted by the published equation.

Second, absence on an axis is deliberately neutral. This permits:

1. C0/G0 in zone Z;
2. G0 revoked with its frontier;
3. C0 renewed to C1, with Z legitimately omitted because no active grant
   remains;
4. later G1 issued to the same device/lineage in Z; and
5. old C0 signing a new operation under G1.

G1 binds the device, not a certificate generation. There is no C0 closure in
Z and no G1 revocation closure, so the literal formula admits the old signer,
contradicting “the old key authors nothing new”
([renewal](/Users/vm/owner-plane-d0a-spec.md:347)).

Publish recovery ∩ certificate ∩ grant ∩ epoch and add a portable
certificate/grant temporal-compatibility rule.

### A8. D-167's typed IDs use the wrong cross-role identity

The peer treats consistent `key_id` comparison as closure
([peer D-167](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:45)).
`H_key` includes the algorithm tag. Browser signing uses `p256` while HPKE
uses `hpke-p256-v1`, although both keys are the same SEC1 P-256 point format
([suite](/Users/vm/owner-plane-d0a-spec.md:185);
[key encoding](/Users/vm/owner-plane-d0a-spec.md:195)).

The same private scalar/public point therefore receives different key IDs
across roles. D1's KEM material can become D2's signing material, bypassing
device self-exclusion; D1's signing material can become D2's KEM material,
letting D1 decrypt D2's wraps. Same-certificate role reuse also lacks a
rejection.

Keep role-tagged `key_id` for addressing, but add a role-neutral P-256
material identity for freshness and retirement. Vector both role-swap
directions and intra-certificate reuse.

### A9. D-167's held-chain admission does not compose with Frontier removal

The peer correctly confirms that `last_known` no longer requires acceptance
([peer D-167](/Users/vm/owner-plane-d0a-spec-v0.5.14-review-2.md:45)).
The Frontier contains accepted heads only
([Frontier](/Users/vm/owner-plane-d0a-spec.md:624)), while accepting
`w.gen(last_known = H)` retires exactly H
([transition](/Users/vm/owner-plane-d0a-spec.md:2034)).

With accepted H4 and held-but-budget-displaced H5, a funded `w.gen` may cite
H5. Removing H5 from the accepted-only Frontier is a no-op, so H4 remains
live even though the incorporation cap closes the generation at H5.
Repeated use leaks stale live heads and breaks Frontier/checkpoint exactness.

Retire the effective accepted Frontier head at or below the named canonical
position. If none exists, define a successful no-op or a deliberate rejection
with a named outcome.

## v0.5.15 closure checklist

Before the next freeze review, the text, CDDL, outcomes, and vectors should
agree on:

1. one staged-frontier transition table, including lenient retirement and
   late-carrier suffix re-fold;
2. the complete import freeze-reservation set and a compound collision cause;
3. durable, erasure-stable source binding for every import record;
4. a journal evaluation coordinate or monotone typed cause construction;
5. adopted-renewal membership plus terminal-KEM/`retired_keys` precedence;
6. the named historical revocation wrap-domain equation;
7. recovery ∩ certificate ∩ grant ∩ epoch plus cert/grant temporal
   compatibility;
8. role-neutral P-256 material freshness across signing and KEM; and
9. exact accepted-Frontier retirement for held-chain `last_known`.

Run the E10 sweep at the same time: ordinary import displacement,
provisional-target waiting, staged-consumer waiting, reopen citation states,
journal invariant failures, adoption overlap, old-cert/new-grant rejection,
and no-accepted-head retirement all need exact outcomes or explicitly
successful transitions.

## Artifact sequence

The peer's order is good after v0.5.15:

1. land the nine rule/exactness repairs in prose, CDDL, and decision records;
2. write the counterexample cases above into the companion's opening tranche;
3. build the independent core and differential harness;
4. generate the corpus and run families 1–13;
5. perform family 14 offline confirmation;
6. run required surfaces and the final prose↔vector discrepancy audit.

Counterexample fixtures may be drafted now as non-normative design aids.
They must not silently choose between the alternatives above. The companion
becomes normative only after each choice is ratified in the specification.

## Final assessment

v0.5.14 is another strong convergence step. The peer report provides useful
independent evidence that the intended rulings were propagated, and the first
review's D-165 claim deserved the downgrade made here. That is the value of
synthesis: retain corroborated repairs, narrow an overclaim, and preserve
counterexamples that survive source-level challenge.

The remaining defects are nevertheless protocol law, not merely missing
tests. Two cross security boundaries (old-key authority and cross-role key
material); the others determine accepted state, Frontier state, or whether a
journal transition is verifiable. Executable vectors will expose these
problems only after the prose says what the correct result is.

**Final decision: no-go for freeze, companion, or independent core; cut
v0.5.15, then review again.**
