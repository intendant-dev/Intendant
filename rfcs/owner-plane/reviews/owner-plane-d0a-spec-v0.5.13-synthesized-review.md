# Synthesized review: D0-A Core + Memory normative specification v0.5.13

*2026-07-13. Adjudicated synthesis of
[*Review: D0-A Core + Memory normative specification v0.5.13*](/Users/vm/owner-plane-d0a-spec-v0.5.13-review.md)
(SHA-256
`927e16c30feb39dd5a9b663575bfa553972fc9050a0f9c437ee1e283f531457e`)
and
[*Review 2: D0-A Core + Memory specification v0.5.13*](/Users/vm/owner-plane-d0a-spec-v0.5.13-review-2.md)
(SHA-256
`cb82da797b843d6217d537efc315df8fd22c32d87f04801b6a153f0fdd98dbfb`),
verified against
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.13,
4,248 lines / 345,716 bytes (SHA-256
`0d3a316a8082392744f2890a92e2824a4bb287796fe210c1ac07eb710f9c8609`).
The prior v0.5.12 synthesis used as the finding ledger has SHA-256
`dbf85d80bb6c9f8a47cbb0770d71c2a9b043a4fcf3ad2555a0a2fc99db53289d`.
This synthesis adjudicates claims against normative rules and composed traces;
it does not average the two verdicts.*

## Executive verdict

**Cut v0.5.14. Do not freeze v0.5.13 or begin the normative companion,
corpus, or independent core from this text.**

The peer review is right that v0.5.13 lands substantial repairs. In
particular:

- D-152 closes the 64-gap/65-head constructibility error in every relevant
  carrier.
- D-153 prevents a prior consumer's materialized staged frontier from being
  reused by a later consumer.
- D-154 closes the previously identified explicit cross-generation
  `last_known` reservation race.
- D-155 replaces grant-local import ownership with a portable cross-grant
  comparator and makes C3′ unfreezing derived.
- D-156 authenticates the selected record's export identifier and signed
  source rank.
- D-157 gives journal terminal causes a broader domain and gives intervals
  explicit incarnations.
- D-158 brings adopted renewals into typed key history and precedence
  resolution.
- D-159 makes the authorship/wrap-domain split, selector intersection, and
  named-versus-omitted recovery choices explicit.

Those decisions should be retained. The peer's conclusion that all clusters
are discharged
([peer verdict](/Users/vm/owner-plane-d0a-spec-v0.5.13-review-2.md:13))
does not survive reducer-level checking, however. Its own scope note says
composed-trace depth was limited
([peer scope](/Users/vm/owner-plane-d0a-spec-v0.5.13-review-2.md:6)).
That audit establishes that many fields and comments landed; it does not prove
that replicas reach the same result under reordering, rebuild, recovery, and
partial retention.

Eight freeze-blocking clusters remain:

1. staged-frontier eligibility and disposal are undefined for pending carriers
   and stale lineages;
2. import-loser disposition is arrival-relative, and an unresolved earlier
   claimant does not reserve ownership;
3. proof-only Merkle validation is not equivalent to full-bundle validation,
   while the leaf preimage is unversioned and has no normative CDDL type;
4. `XferReopen` has no portable coordinate proving the prior invalidation on
   which its legality depends;
5. adopted-renewal ordering is stated in both directions, and the eligible
   historical key/list semantics are not closed;
6. device revocation cannot encode an empty authorship domain and can declare
   completion without covering a wide authorship domain;
7. selector intersection conflicts with the still-normative certificate
   validity and validation-stage rules; and
8. recovery's post-base universe differs across main prose, E8, and CDDL,
   while freshness mirrors disagree about key identity and adopted history.

Several yield different durable outcomes from the same eventual operation set;
others make a stated legal ceremony unencodable. They are specification
defects, not merely missing tests. The companion or core would have to invent
protocol law to proceed.

Recommended disposition:

- **Architecture and security posture:** accept.
- **D-152 and D-154:** accept as closed.
- **D-153:** accept the one-shot repair; keep the stage state machine open.
- **D-155 through D-159:** accept their intended choices; repair the
  contradictory or incomplete executable rules.
- **Peer wire audit:** retain as useful field-presence evidence, but reject its
  empty finding ledger.
- **Protocol/schema freeze:** no.
- **Gate A:** false, also explicitly so in the specification.
- **Artifact sequence:** accept the peer's sequence after v0.5.14 closes the
  rules, not before.

## Assessment of the peer review

### What it did well

The peer review is concise, independent, and unusually clear about the limits
of its method. It correctly checked the eight new decision records against E8,
CDDL, domain inventories, and named vector obligations. Its strongest
contributions are the narrow confirmations that:

- the maximum frontier shape is now 65 everywhere;
- already-materialized stage entries cannot be reused;
- explicit boundary reservation is lineage-wide across generations;
- import claimant order is no longer grant-local;
- a selected Merkle record binds `export_id` and zero-based source rank;
- journal `basis` can physically carry either a control or tenant operation;
- adopted keys now enter a typed history domain; and
- revocation now recognizes authorship and wrap coverage as different sets.

It is also right to preserve the durable-P1 gate: even after a future Gate A
pass, Gate B and the umbrella RFC's P0.5/tombed-cutover prerequisites still
apply. Finally, its proposed artifact order—companion, independent core and
harness, corpus, family 14, required surfaces, then discrepancy audit—is the
right order once the prose and schema are deterministic
([peer recommendation](/Users/vm/owner-plane-d0a-spec-v0.5.13-review-2.md:48)).

### Where its method stops short

The report is best understood as a **representation audit**, not a protocol
adjudication. Three distinctions matter:

1. **A field can exist without a complete lifecycle.** D-153 has stage bytes,
   but the text does not say whether a held, reference-unresolved carrier
   registers a stage or what happens when its lineage dies before consumption.
2. **A local proof can authenticate its selected record without enforcing a
   global bundle invariant.** D-156's selected leaf is self-describing, but a
   proof-only verifier cannot reconstruct an erased sibling whose exact bytes
   the full-bundle rule still requires.
3. **Two individually plausible rules can contradict when composed.** D-159's
   grant selector says an old prefix survives where the renewal omitted a
   zone; §4.2's certificate rule says the same prefix is certificate-
   superseded.

The peer's sentence “Every ruling has its bytes”
([peer finding](/Users/vm/owner-plane-d0a-spec-v0.5.13-review-2.md:43))
is too strong even on its stated wire scope:

- D-156's `H_brec({export_id, rec_index, rec})` object has no named,
  versioned CDDL production, so its canonical bytes are not actually fixed.
- `crevokedev.cutoffs` remains `[+]`, so the protocol has no bytes for the
  expressly legal zero-authorship case.
- recovery's CDDL blanket has no post-base-enrollment exception, contradicting
  the main rule and required vector.

This does not make the peer review poor. It makes its valid conclusion
narrower: **the intended v0.5.13 changes are visible in most of the expected
surfaces.** Its method cannot support “findings: none,” freeze approval, or
artifact start.

## Adjudicated disposition ledger

| Decision / topic | Peer assessment | Synthesized assessment |
|---|---|---|
| D-152, 65-head frontier | Discharged | **Closed** across E8, CDDL carriers, and vector inventory |
| D-153, one-shot stages | Discharged | **Narrow reuse bug closed; blocker remains** for pending-stage eligibility and stale-stage disposal |
| D-154, cross-generation reservation | Discharged | **Closed** for the prior explicit-boundary race |
| D-155, import order/freeze | Discharged | **Direction fixed; blocker remains** because loser disposition and pending ownership are not order-independent |
| D-156, selected Merkle leaves | Discharged | **Selected leaf fixed; blocker remains** for full/proof equivalence and canonical leaf bytes |
| D-157, journal basis/intervals | Discharged | **Representation improved; blocker remains** because reopen legality is not reconstructible |
| D-158, adopted keys | Discharged | **Domain choice fixed; blocker remains** due to inverted ordering and unclear current/history/list rules |
| D-159, revocation/selectors/recovery | Discharged | **Choices accepted; blockers remain** in constructibility and normative mirror composition |
| Ordinary `w.gen` eligibility | No finding | **High exactness:** body validity and cap eligibility use different head predicates |
| Artifact readiness | Start if synthesis concurs | **Not ready:** repair v0.5.14 first |

## Freeze blockers

### B1. D-153 does not define the staged-frontier state machine

D-153 correctly makes an accepted stage one-shot and says the next consuming
operation consumes every applicable staged frontier
([staging rule](/Users/vm/owner-plane-d0a-spec.md:1629)). D-154 also correctly
reserves later explicit boundaries naming an unresolved `(zone, lineage)`
([reservation](/Users/vm/owner-plane-d0a-spec.md:1605)).

Two lifecycle cases remain undefined.

First, let C1 carry a complete `ccutoff.closes` frontier H but remain
`ref-unresolved`. A later strict consumer C2 relies on C1's implicit staged
materialization and names no explicit boundary. Under an accepted-effects
reading, no stage exists yet and C2 permanently fails coverage. Under a
held-wire reading, C1 registers a pending stage and C2 waits. H later arrives
and C1 accepts. A fresh fold can then accept C2, while an incremental fold can
retain its rejection. The text never chooses whether a held but unresolved
carrier is stage-eligible or whether an implicit consumer joins its dependency
cone.

Second, an accepted stage for lineage L can be followed by a grant revocation
that makes L non-live before the next consumer. D-153 says the consumer takes
**every** stage, while closure equality says every entry names a currently live
lineage. Dropping L, consuming it inertly, rejecting this consumer only, or
poisoning every future consumer are all plausible. No expiry or cancellation
rule selects one.

The unrelated-device-renewal example is not needed to prove the blocker:
literal “consume every stage” may deterministically burn such a stage. The
pending-carrier and dead-lineage cases are sufficient.

Required repair: define stage creation, pending dependency, identity, scope,
one-use transition, cancellation/expiry, dead-lineage treatment, and fresh-
fold reconstruction in one state table.

### B2. D-155 still makes import outcomes depend on arrival order

D-155's portable comparator is the right mechanism
([claimant order](/Users/vm/owner-plane-d0a-spec.md:2572)). Its loser
disposition is not portable.

Take two valid claimants A < B under the canonical order, with no freeze:

- if B arrives first and A later displaces it, B becomes quarantine-
  reproposable/revivable;
- if A arrives first and B arrives later, B is reject-permanent as an import
  collision.

The final bytes, canonical winner, and owner decision are identical, but B's
required disposition differs solely by delivery order
([collision rules](/Users/vm/owner-plane-d0a-spec.md:2595)). That difference
can affect later reproposal behavior and therefore is not harmless local
bookkeeping.

“Surviving claimant” is also undefined while an earlier claimant is
cryptographically or reference-pending. If later Q is accepted while earlier
P waits for a proof or receipt, Q can become owner—or even freeze—before P
later proves it should win. Treating pending P as a reservation blocks Q; an
accepted-only reading does not. Both readings fit the current prose.

Required repair: derive loser disposition from final claimant relations, not
arrival, and specify whether otherwise-admissible unresolved earlier
claimants reserve the replay key. Reconcile the later statement that a frozen
identity never moves with the explicit C3′ derived-unfreeze transition.

### B3. D-156 defines two non-equivalent Merkle validators

The exact-bundle rule requires all signed `sources` records to be present and
canonically reconstructed
([bundle rule](/Users/vm/owner-plane-d0a-spec.md:2435)). The proof-only path
allows a retained selected record plus opaque sibling hashes
([proof verifier](/Users/vm/owner-plane-d0a-spec.md:2479)).

Consider signed sources [A, B]. The release root was built from correct A at
rank 0 and malicious or noncanonical B′ at rank 1. While both records remain,
the full verifier derives the required B and rejects the mismatch. After B is
erased, A plus B′'s opaque sibling hash still reaches the signed root and the
proof-only verifier accepts A. Binding A to `export_id` and rank proves A's
position; it does not prove the unseen sibling had the exact source-derived
preimage required by the all-or-nothing rule.

The leaf formula itself is also not wire-complete. The object
`H_brec({export_id, rec_index, rec})` has no named CDDL shape, no `v` field
despite E6's versioned domain rule, and no statement whether `rec` is embedded
as a map or a byte string. Two implementations can hash different canonical
objects while both believe they followed the prose.

Required repair: choose one invariant. Either retain enough globally committed
material to validate every source-derived leaf after selective erasure, or
make validity genuinely per-record and remove the exact-bundle/all-or-nothing
requirement. In either case add a named, versioned `bundleleaf` CDDL
production and exact hash preimage.

### B4. D-157 cannot reconstruct whether `XferReopen` was legal

A reopen is legal only after the selected terminal basis has been invalidated;
otherwise it is storage corruption and storage-quarantine
([basis and interval rules](/Users/vm/owner-plane-d0a-spec.md:1219)).
The wire operation carries only `export_id`, `release_op`, and `incarnation`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4017)). It carries neither the
invalidated basis nor the exact invalidating fact/frontier.

Tenant, control, and transfer-journal operations live in different feeds. A
replica can therefore see the invalidating fact before the reopen and accept
it, or see the reopen first and quarantine it as corrupt. If a still later fact
restores the old terminal condition, a fresh final-state rebuild cannot infer
the historical point at which the reopen was legal. “First branch-relative
fact in fold order” also lacks a total cross-feed order.

Required repair: make the reopen cite the prior basis and a portable,
reproducibly ordered invalidation witness/frontier, or withdraw the strict
historical-legality claim and define a monotone alternative. Publish an exact
D-149 outcome → canonical-basis table, including an E10 outcome for an illegal
reopen.

### B5. D-158's adopted-renewal order is inverted in its own negative

The intended rule and CDDL say the recovery base precedes the cut renewal
([adoption rule](/Users/vm/owner-plane-d0a-spec.md:1764),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3882)). The new negative rejects a
renewal that “does not precede” the base
([negative](/Users/vm/owner-plane-d0a-spec.md:1784)). Thus a base at C5 and an
adopted renewal at C8 is simultaneously required and rejected. The decision
record repeats the reversed “not-before-base” formulation.

The typed history also does not say which adopted KEM key is reusable by the
same adopted device. In K0→K1→K2, generic same-device history appears to allow
K1, while the rotated-away-key rule forbids it. Only the terminal/current
adopted KEM key should be eligible if the intent matches normal enrollment
freshness.

Finally, “each entry KEM-rotating” conflicts with an adoption chain that can
contain signing-only intermediate renewals. Listing all renewals and following
implicit dependencies have different 64/65 boundedness and history behavior.

Required repair: require each adopted renewal to be a strict descendant/after
the base on the cut branch; define current versus historical adopted key
eligibility; and choose whether the bounded list contains all renewals,
KEM-changing renewals, or terminal renewals plus a separately validated
dependency closure.

### B6. D-159's device-revocation ceremony is not constructible

The conceptual split is correct: authorship zones need cutoffs, while wrapped
zones need KEK rotation references. The schema does not implement its full
domain:

- `c.enroll_new.grants` and `rotation_refs` allow empty arrays;
- `c.revoke_device.cutoffs` remains `[+]`
  ([revocation CDDL](/Users/vm/owner-plane-d0a-spec.md:3655)).

A device with certificate authority but zero authored zones is therefore
legally revocable in prose but unencodable on the wire.

At the other boundary, a device can have 65 author zones and zero wraps. The
first operation can carry only 64 cutoff entries and needs continuation. The
explicit completion check, however, is phrased over wrapped zones only, so
that empty wrap domain can make the ceremony appear complete before all
authorship cutoffs exist.

Required repair: make cutoffs `[*]` and define completion as both:

1. cutoff coverage equals the complete authorship domain; and
2. rotation-reference coverage equals the complete wrap domain.

Add 0-author/0-wrap, 65-author/0-wrap, and 0-author/65-wrap vectors.

### B7. Selector intersection has no single executable authority formula

D-159 says certificate and grant survival are intersected
([selector rule](/Users/vm/owner-plane-d0a-spec.md:1579)). That is the right
choice. Section 4.2 still says the old certificate becomes superseded and
preserves only operations at or before its carried renewal
`history_cutoffs`; it contains no D-159 rule for a zone legitimately omitted
because its last grant was already revoked
([certificate validity](/Users/vm/owner-plane-d0a-spec.md:347)). The staged
validation pipeline also evaluates certificate validity before the grant stage
([pipeline](/Users/vm/owner-plane-d0a-spec.md:2104)).

Let the last grant for zone Z be revoked at O. A later certificate renewal C
omits Z. Replaying an old grant-authorized operation at or before O should
survive the grant closure under D-159. Section 4.2 can instead classify the
certificate as superseded and reject the same prefix before grant logic runs.

Required repair: publish one executable predicate:

`certificate closure ∩ grant closure ∩ epoch closure`,

with separate matching rules and explicit precedence/disposition. Cite the
same formula from §4.2, §7.1, §10.2, E8/CDDL comments, and vectors.

### B8. Recovery and key freshness still have contradictory mirrors

The main recovery rule applies implicit-none only to lineages enrolled at or
before the recovery base; later normal enrollment remains possible
([main recovery rule](/Users/vm/owner-plane-d0a-spec.md:1747)). E8 and CDDL say
every absent pair is implicit none, without that exception
([E8](/Users/vm/owner-plane-d0a-spec.md:130),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3867)). A first write from a
post-recovery enrollment therefore accepts under the main rule and required
vector but quarantines under the normative CDDL mirror.

Freshness has a separate identity mismatch. One rule compares raw `sig_pk`
values against a domain otherwise expressed as typed `key_id =
H_key({alg,pk})`; the enrollment mirror omits adopted history and says a KEM
key “ever enrolled” is burned, while D-150/D-158 allow reuse of uncarried,
unadopted cut keys. Replicas with different visibility into the cut
certificate can disagree.

Required repair:

- put the at-or-before-base universe and post-base exception into E8 and CDDL;
- normalize every comparison to the same typed `key_id`;
- define the freshness set as surviving, retired, and typed adopted history
  under the chosen current/history rule; and
- state explicitly whether an unadopted cut key ever entered the portable burn
  set.

## High exactness items to close in the same cut

These are subordinate to the eight blockers but should not cross the freeze:

1. **Ordinary `w.gen` uses two head predicates.** Cap eligibility is based on
   held-chain membership and stays stable through budget displacement, while
   the ordinary body rule still requires the head to be last-known accepted.
   A canonical but budget-displaced H can therefore be eligible for the cap
   calculation and invalid as the operation's own `last_known`. Choose one
   rule or state and test the intentional difference.
2. **D-157 needs an exact basis table.** Map every branch-relative D-149
   outcome to one canonical basis, including dual sufficient causes and
   tie-breaking.
3. **Interval prose needs cleanup.** Direct terminal-to-reopen wording remains
   stale beside the newer blanket rule; remove it rather than relying on
   readers to infer which sentence dominates.
4. **Decision records overclaim closure.** D-153 and D-155 through D-159
   should describe the accepted direction without saying the whole issue is
   closed until their executable mirrors agree.

The original terminality-cycle concern is closed: the H5/H6 dependency now has
a valid evaluation order. It should not be carried forward.

## Required v0.5.14 acceptance traces

At minimum, require both delivery orders plus fresh rebuild for:

1. a pending stage followed by an implicit consumer;
2. a staged lineage revoked before consumption;
3. import B then winning A versus A then losing B, asserting identical loser
   disposition;
4. an earlier canonical import claimant pending while a later claimant is
   otherwise admissible;
5. a full two-record bundle versus a retained-record proof after the sibling
   is erased or unavailable;
6. a reopen before versus after its cited invalidation across feeds;
7. recovery base C5 with adopted renewal C8;
8. K0→K1→K2 adoption, including K1 reuse and terminal K2 same-device reuse;
9. signing-only intermediate renewals under the chosen bounded-list rule;
10. device revocation with 0/0, 65/0, and 0/65 author/wrap domains;
11. last-grant revocation followed by a renewal omitting the zone, asserting
    certificate, grant, and final-prefix validity;
12. a first write from a post-recovery enrollment under prose, CDDL, and fresh
    fold;
13. a cut but unadopted key observed by one replica and absent from another;
    and
14. `w.gen(last_known=H)` where H is held and canonical but
    budget-displaced.

Every trace needs the required E10 outcome and disposition, not only a vector
title.

## Recommended next sequence

1. **Cut v0.5.14** with the eight state/schema repairs above.
2. **Run a normative mirror sweep** across the main rules, E8, operation
   registry, CDDL and comments, decision record, adversary table, and required
   vector inventory.
3. **Repeat an independent composed-trace review.** A wire-presence audit is
   useful but insufficient for the freeze decision.
4. **Then begin the peer's artifact sequence:** D-91 companion, independent
   core and harness, corpus, family 14, required surfaces, and final
   prose↔CDDL↔companion↔vector discrepancy audit.
5. **Keep durable P1 writes gated** until Gate B and the umbrella RFC's
   P0.5/tombed-cutover prerequisites are also satisfied.

Non-normative harness scaffolding that encodes no reducer choices is harmless,
but neither the companion nor fixtures should settle any item above on behalf
of the specification.

## Final assessment

v0.5.13 is a productive cut. It closes two prior issues outright and replaces
several weak mechanisms with the right abstractions. The peer review usefully
confirms that those abstractions reached many of their expected wire and
mirror surfaces.

It is not a freeze cut. The decisive failures are not speculative complexity:
the text gives arrival-relative import dispositions, full and proof-only
validators that disagree, opposite adopted-renewal orderings, no encoding for
a legal empty-domain revocation, and recovery mirrors that classify the same
post-base enrollment differently. Stage and reopen state also lack enough
portable information to replay deterministically.

One focused v0.5.14 can plausibly close this residue. Repair the normative text
first, then let the companion and independent implementation test the
protocol. Gate A remains explicitly false until those artifacts and the final
discrepancy audit pass
([status](/Users/vm/owner-plane-d0a-spec.md:3373)).
