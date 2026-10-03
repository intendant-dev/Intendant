# Review: D0-A Core + Memory normative specification v0.5.13

*Independent review, 2026-07-13. Reviewed source:
[`~/owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md), v0.5.13
draft, 4,248 lines / 345,716 bytes, SHA-256
`0d3a316a8082392744f2890a92e2824a4bb287796fe210c1ac07eb710f9c8609`.
The archived v0.5.12 source is
`01cd50f8fba3df806be0e73bd19b6ad886e57146beb94dc728e2f2af788436c9`;
the v0.5.13 delta is 348 insertions and 151 deletions. The v0.5.12
synthesized review used as the prior finding ledger is
`dbf85d80bb6c9f8a47cbb0770d71c2a9b043a4fcf3ad2555a0a2fc99db53289d`.
No peer v0.5.13 review was consulted.*

## Executive verdict

**Do not freeze v0.5.13. Cut v0.5.14.**

This revision makes real progress. D-152 fixes the 64/65 frontier
constructibility error. D-154 closes the explicit cross-generation selector
race. D-153 correctly forbids reuse of a prior consumer's materialized
frontier. D-155 chooses the right broad shape for portable import ownership:
a cross-grant total order and a wholly derived freeze predicate. D-156 binds
the current record's export context and ordinal in every Merkle leaf. D-157
admits both control and tenant causes and gives journal intervals explicit
incarnations. D-158 puts adopted renewals into precedence resolution and into
typed key history. D-159 makes several overdue choices explicit: authoring
versus wrap domains, selector intersection, named-versus-omitted recovery,
ordinary `w.gen` chain membership, and hashed `retired_keys` membership.

Those decisions should be retained. They do not yet form one deterministic,
constructible protocol, however. Eight freeze-blocking clusters remain:

1. an implicit staged-frontier consumer can reject before its earlier pending
   stage resolves, and automatic zone-wide consumption has no stale or
   unrelated-stage rule;
2. the same losing import claimant is quarantine-reproposal or
   reject-permanent solely according to arrival order, while unresolved
   earlier claimants have no reservation semantics;
3. proof-only Merkle validation is still not equivalent to the exact-bundle
   invariant, and the new leaf preimage has neither a versioned CDDL shape nor
   unambiguous bytes;
4. `XferReopen` is legal only after a cross-log invalidation but carries no
   portable invalidation coordinate, making its storage-corruption check
   unreconstructible;
5. adopted-renewal ordering is stated in both directions, while historic
   versus terminal adopted KEM keys and signing-only intermediates remain
   ambiguous;
6. device revocation cannot encode an empty authorship domain and can complete
   wide author revocation after checking only wrap coverage;
7. D-159's selector-intersection result conflicts with the still-normative
   certificate lifecycle and validation-stage prose; and
8. the recovery blanket's post-base universe is different in the main rule,
   E8, and normative CDDL, while the enrollment freshness rule still omits
   adopted history and mixes raw keys with key IDs.

These are mostly local repairs, not an argument to reopen the Owner Plane,
Memory, the authority-closure model, or the cryptographic suite. But several
produce two different outcomes from the same held bytes, and two make legal
ceremonies unencodable. Fixtures would have to invent answers, so the
companion/corpus should not start from this cut.

Recommended disposition:

- **Architecture and security posture:** accept.
- **D-152 and D-154:** accept as closed.
- **D-153, D-155, D-156, D-157:** accept the decisions; repair their state
  machines and exact wire semantics.
- **D-158 and D-159:** accept the intended choices; reconcile the contradictory
  executable mirrors.
- **Protocol/schema freeze:** no.
- **Gate A:** false, independently confirmed by the specification's own status
  line.

## Disposition ledger

| Prior issue / v0.5.13 decision | v0.5.13 assessment |
|---|---|
| 64 legal gaps require 65 live heads | **Fixed by D-152** in E8, `frontierclose`, `zoneheads`, and the vector inventory |
| Prior consumer's staged frontier reused | **Fixed by D-153**; materialized entries never count again |
| Stage-to-consumer binding and lifecycle | **Still blocking:** an implicit consumer does not pend behind an unresolved stage; stale/unrelated stages have no rule |
| Cross-generation selector reservation | **Fixed by D-154** for later explicit boundaries |
| Import comparator across grant turnover | **Core fixed by D-155** |
| Frozen import ownership across C3′ | **Core fixed in direction:** freeze is derived and can unfreeze |
| Import loser outcome and pending earlier claimant | **Still blocking:** arrival-relative disposition and undefined claimant/reservation set |
| Record binds export context and source rank | **Fixed by D-156 for the selected record** |
| Full-bundle versus proof-only equivalence | **Still blocking**; an opaque bad sibling can make the two validators disagree |
| Merkle leaf byte shape | **Still blocking:** unnamed, unversioned, and ambiguous map-vs-bstr preimage |
| Terminal cause can be control or tenant fact | **Improved by D-157** |
| Journal interval transition | **Shape improved; causal validity still blocking** |
| Adopted renewal joins freshness and precedence | **Core omission fixed by D-158** |
| Adopted renewal ordering/history | **Still blocking:** reversed base predicate; current-key and intermediate-chain semantics undefined |
| Device revocation author/write domains | **Conceptual split fixed by D-159** |
| Device revocation constructibility/completion | **Still blocking:** non-empty author cutoff wire and wrap-only completion |
| Renewal after revoked grant | **Intended formula selected by D-159; executable certificate rule still contradicts it** |
| Recovery named versus omitted continuation | **Main rule fixed by D-159** |
| Recovery blanket universe | **Main rule fixed; E8/CDDL mirrors still contradict it** |
| Ordinary `w.gen` terminality cycle | **Original H5/H6 cycle fixed**; accepted-versus-held membership remains high-priority exactness |
| `retired_keys` raw-vs-ID comparison | **CDDL fixed**; T3 and enrollment reducer still need one normalized formula |

## Freeze blockers

### B1. An implicit consumer does not wait for its earlier pending stage

D-153 makes an accepted stage automatic, total, and one-shot
([promotion](/Users/vm/owner-plane-d0a-spec.md:1648)). D-154 makes a pending
boundary reserve its whole `(zone, lineage)` scope, but only says that
**later boundaries naming a coordinate** wait
([reservation](/Users/vm/owner-plane-d0a-spec.md:1605)). A consumer relying
implicitly on a stage names no head for that lineage.

Concrete two-order trace:

1. Control operation C1 is a requesterless `c.cutoff` carrying staged
   frontier S for lineage L. S names H, which is not yet held, so C1 is
   `ref-unresolved`.
2. Later control operation C2 is a strict epoch advance. It deliberately has
   no inline entry for L because S is meant to supply it.
3. Under the current text, C2 does not name a coordinate in L, so D-154 does
   not clearly make it wait. C1 is not accepted, hence there is no
   **accepted, unconsumed** stage in C2's coverage union
   ([coverage](/Users/vm/owner-plane-d0a-spec.md:1629)).
4. C2 therefore fails coverage and rejects permanently.
5. H then arrives and C1 accepts. Nothing revisits C2. A fresh fold holding H
   accepts C1 first and then accepts C2.

This is the exact pending-reference arrival-order class that the reservation
machinery exists to prevent. Every consumer must pend behind every earlier
unresolved stage in its required consumption domain, including when the
consumer's own bytes carry no head.

Automatic zone-wide consumption has a second missing transition. The text
says the next consumer takes **every** unconsumed stage for the zone
([D-153 rule](/Users/vm/owner-plane-d0a-spec.md:1654)), while closure equality
requires a consumer entry to name one of its live lineages
([equality pins](/Users/vm/owner-plane-d0a-spec.md:1602)). Consider either:

- stage S for L, followed by revocation of L's final authoring grant, followed
  by an epoch advance; or
- stage S for device A's lineage, followed by device B's renewal in the same
  zone.

The stage is respectively no longer live or unrelated to the renewal's
predecessor-certificate selector. The specification says neither “drop it,”
“leave it staged,” nor “consume it harmlessly despite the equality pin.”
Depending on the reading, it poisons all later consumers or is silently burned
by an unrelated renewal.

Repair D-153 as an exact state machine:

- define a consumer's stage domain as the exact `(zone, lineage)` keys for
  which that consumer requires coverage;
- make the consumer pend behind earlier unresolved stages in that domain;
- say whether a stage outside the current live/required domain is retired,
  remains staged, or rejects the stage at the transition that made it stale;
  and
- add both delivery orders plus stale-lineage and unrelated-renewal vectors.

The original v0.5.12 reuse bug is closed; this is the remaining carrier-to-
consumer seam.

### B2. D-155 derives the owner but not the losing claimant's lifecycle

D-155 says the owner is the first surviving claimant under
`(grant control position, gen, seq)`, and that an earlier claimant
displaces a provisional owner into quarantine-reproposal
([owner rule](/Users/vm/owner-plane-d0a-spec.md:2572)). It then says an
order-later claimant against a held claim is `import-collision` /
reject-permanent
([collision rule](/Users/vm/owner-plane-d0a-spec.md:2602),
[disposition](/Users/vm/owner-plane-d0a-spec.md:2203)).

Let A and B be otherwise valid claims for one replay key, with A earlier than
B and no freeze predicate yet:

- Replica R1 receives B first. B is provisional owner. A arrives and displaces
  B; the text requires B to become quarantine-reproposal, revivable if A dies.
- Replica R2 receives A first. B arrives later; the text requires B to become
  `import-collision`, reject-permanent.

The replicas hold identical bytes and derive the same current owner, yet must
surface different outcomes and different revival behavior. A fresh fold has
no principled way to choose which history to imitate. The contradiction is
even more direct for a frozen owner: D-155 says dissolution of its freeze
basis **unfreezes** the key and derives the next owner
([unfreeze](/Users/vm/owner-plane-d0a-spec.md:2584)), while the later claim
that would become that owner was declared unable ever to win.

The claimant set is also undefined. Let earlier claimant P be durable but
pending a receipt, proof, or causal dependency, and later claimant Q be fully
admissible. If “surviving” means accepted, Q can become owner and freeze; when
P's proof arrives, the total order says P should win. If P reserves the replay
key, Q must remain pending. Both behaviors are plausible, and effect finality
only proves that no earlier **bytes** can still arrive—it does not resolve
earlier held candidates.

Define one set-derived state independent of arrival history. A coherent shape
is:

- structurally and authority-valid unresolved earlier claimants reserve the
  key;
- while the owner is provisional, every non-owner claimant has one derived,
  revivable loser disposition;
- references to any non-frozen carrier remain pending;
- if a stable within-branch freeze justifies a collision classification,
  define its re-evaluation when that basis is cut; and
- give import displacement a closed outcome/disposition rather than relying
  on prose outside the §10.5 table.

Add A-first/B-first exact-outcome vectors, pending-P-before/after-proof vectors,
and freeze→unfreeze vectors that assert both ownership and every claimant's
lifecycle.

### B3. D-156 authenticates one leaf but not the exact bundle, and its leaf is not a frozen wire object

The current-record repair is good: every selected leaf binds `export_id` and
the record's 0-based rank in signed `sources`
([construction](/Users/vm/owner-plane-d0a-spec.md:2443),
[verification](/Users/vm/owner-plane-d0a-spec.md:2452)). That prevents a
proof from moving the selected record to another export or ordinal.

It does not make proof-only verification equivalent to the still-global exact
bundle rule. The bundle is required to contain exactly the release's sources
([bundle contract](/Users/vm/owner-plane-d0a-spec.md:2435)), and a destination
may either rederive the root from the whole bundle or validate a per-record
path ([alternative validators](/Users/vm/owner-plane-d0a-spec.md:2479)).

Trace:

1. A signed release names sources [A, B].
2. Its compromised—but validly signing—exporter sets
   `R = H_bnode(leaf(A@0), X)`, where X is not the leaf of the actual
   signed-source record B.
3. A full-bundle verifier derives the real B leaf and rejects R.
4. After B is erased, an A-only rebuild receives A's correct self-describing
   leaf plus opaque sibling X. Its proof reaches signed R and accepts A.

The adversary table explicitly says signatures do not protect against a
compromised signer signing bad content
([boundary](/Users/vm/owner-plane-d0a-spec.md:3163)). Self-description proves
the selected record; it cannot prove the unseen sibling's content. This is
fundamental, not a missing tag. Choose one semantic:

- retain enough durable, nonsecret validation material to preserve a global
  exact-bundle decision after erasure; or
- define validity per released record, so a bad B makes B unimportable but
  does not make A's validity depend on whether B plaintext happens to remain,
  and remove the all-or-nothing full-bundle alternative.

The latter matches the per-record replay and partial-import architecture more
naturally.

Independently, the new hash preimage is not constructible from the normative
CDDL. The formula is
`H_brec({export_id, rec_index, rec: canonical bundlerec bytes})`,
but no `bundleleaf` rule says whether `rec` is an inline CBOR map or a
CBOR byte string containing encoded `bundlerec`. The outer hashed map also
lacks `v`, contrary to E6
([E6](/Users/vm/owner-plane-d0a-spec.md:77)). This specification already
recognizes the same rule for direct `H_recips` preimages in D-110
([precedent](/Users/vm/owner-plane-d0a-spec.md:3291)).

Define, for example,
`bundleleaf = { v: 1, export_id: bytes16, rec_index: uint, rec: bundlerec }`,
hash that exact object, and pin encoder bytes plus the full (record_count,
rec_index) path-shape algorithm. The current “missing/extra sibling” prose
also needs to say explicitly that expected left/right siblings and promotions
are derived from both signed source count and index.

### B4. `XferReopen` has a causal precondition but no causal evidence

D-157 makes `Reopen(n)` legal only after a recorded terminal cause
invalidates, and otherwise classifies it as storage corruption
([interval rule](/Users/vm/owner-plane-d0a-spec.md:1242)). The wire carries only
`{ export_id, release_op, incarnation }`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4017)). The source journal, tenant
logs, and control log are separate ordering domains; the reopen carries no
control/data frontier or invalidating operation.

Two traces expose the problem:

1. Abort(0) records tenant claimant P as a collision basis. Control/proof fact
   C later retro-quarantines P, so the writer appends Reopen(0). A replica
   seeing C first calls the reopen legal; one seeing Reopen first sees P still
   standing and must storage-quarantine the same journal bytes. There is no
   named missing reference on which it can pend.
2. C invalidates P and Reopen(0) is historically legal, but a later C3′ cuts C
   and revives P. A fresh rebuild at final state cannot prove that P was
   invalid at the moment the reopen was appended.

Either carry a portable invalidation witness and evaluation coordinate—at
minimum the invalidated basis, exact invalidating fact, and sufficient
control/tenant frontier to reproduce the transition—or explicitly make reopen
legality trusted writer-local state and withdraw the strict
storage-quarantine validation claim. The first is much more consistent with
the rest of this specification.

The cause rule also is not yet as closed as D-157 claims. “The first
branch-relative fact in fold order” does not define a shared order between
control and tenant facts, nor map every D-149 outcome to a basis operation
([basis prose](/Users/vm/owner-plane-d0a-spec.md:1219)). Tie selection to the
§10.2 failure-stage order plus an exact within-stage rule, or carry a canonical
set of sufficient causes. Also map illegal reopen transitions to one actual
closed outcome—“storage-quarantine” is a disposition, not a member of §10.4,
so the present wording violates E10
([E10](/Users/vm/owner-plane-d0a-spec.md:147)).

### B5. The adopted-renewal rule rejects the ceremony it is meant to admit

The intended ordering is stated several times: recovery `base` precedes a
cut KEM-rotating renewal
([storage adoption](/Users/vm/owner-plane-d0a-spec.md:1764),
[CDDL rationale](/Users/vm/owner-plane-d0a-spec.md:3882),
[vector](/Users/vm/owner-plane-d0a-spec.md:2942)). The new validity sentence
then says an entry “whose renewal does not precede `base`” is
`body-invariant`
([contradiction](/Users/vm/owner-plane-d0a-spec.md:1784)); the decision/CDDL
shorthand “not-before-base” is equally hazardous.

For base C5 and cut renewal C8, the intended rule admits adoption because
C5 precedes C8, while the new negative rejects it because C8 does not precede
C5. Replace this with one graph predicate: the adopted renewal must be a
strict descendant of / strictly after `base` on the held cut branch.

Two further key-history decisions are needed:

- T3 says an adopted renewal's KEM key may be reused by the same device
  ([typed history](/Users/vm/owner-plane-d0a-spec.md:833)). For an adopted
  K0→K1→K2 chain, generic same-device reuse appears to admit K1 even though
  D-150 forbids returning to a rotated-away KEM key. Only the terminal/current
  KEM key of the adopted chain should be eligible for the recovery
  re-enrollment ritual; historic adopted KEM keys remain burned.
- The list is described both as “each entry adopts a KEM-rotating renewal” and
  as a contiguous chain **including signing-only intermediate renewals**
  ([§7.4](/Users/vm/owner-plane-d0a-spec.md:1784),
  [CDDL](/Users/vm/owner-plane-d0a-spec.md:3879)). Decide whether every renewal
  is listed and counts against 64, or only KEM rotations are listed while
  validation dependency-closes over unlisted signing-only links. This changes
  E7, cap exhaustion, which signing keys are burned, and deep-fork orphaning.

Publish one typed history reducer: signing key IDs are globally burned; KEM
key IDs map to device plus rotation order/currentness; unadopted cut history is
ignored after C3′; and only the current adopted KEM key gets the narrow
same-device exception.

### B6. Device revocation is not constructible for zero authorship and can complete on the wrong domain

D-159 correctly distinguishes:

- `cutoffs` over active authoring-grant zones; and
- exclusion rotations over current wrap zones

([registry](/Users/vm/owner-plane-d0a-spec.md:1335),
[CDDL comment](/Users/vm/owner-plane-d0a-spec.md:3660)).

But `crevokedev.cutoffs` remains `[+ frontierclose]`. A legal
`cenrollnew` may have an empty grants array and empty wraps
([enrollment CDDL](/Users/vm/owner-plane-d0a-spec.md:3597)); a read-only or
already-grant-revoked device likewise can have an empty authorship domain.
Revoking its certificate is unencodable unless the owner invents an
out-of-domain cutoff.

Wide authorship exposes the dual bug. A device can have 65 authoring zones and
zero current wraps. The initial revocation carries 64 cutoffs and requires a
continuation. The registry's explicit completion sentence nevertheless says
the compound completes when the union covers every **wrapped** zone
([completion](/Users/vm/owner-plane-d0a-spec.md:1335)); that condition is
vacuously true before the 65th author cutoff arrives.

Use `[* frontierclose]` and define completion as the conjunction of two
exact pre-state predicates:

1. author cutoff union equals the complete authorship-zone set; and
2. rotation-reference union equals the complete wrap-zone set, with each
   rotation after the last accepted wrap.

Missing either keeps the compound pending. Pin 0-author/0-wrap,
65-author/0-wrap, and 0-author/65-wrap ceremonies.

### B7. Selector intersection and certificate validity still give opposite answers

D-159 says an operation is bounded by the intersection of every **matching**
closure, and absence of a closure leaves D-86 position-relative validity to
govern. It explicitly uses this as the renewal-after-revocation formula
([new formula](/Users/vm/owner-plane-d0a-spec.md:1579)).

The still-normative certificate section says instead that a renewed old
certificate remains valid only at or before renewal `history_cutoffs`, using
the retired `accepted_through: "none"` shape
([§4.2](/Users/vm/owner-plane-d0a-spec.md:347)). The admission pipeline resolves
certificate validity before grant validity
([pipeline](/Users/vm/owner-plane-d0a-spec.md:2101)).

Trace:

1. Old certificate C has grant G in zone Z.
2. G is revoked at frontier O.
3. C is renewed. Z is correctly omitted from renewal history coverage because
   no active authoring grant remains there.
4. Replay an old G operation at or below O.

The §4.2/certificate-stage reading returns `cert-superseded` because no
renewal cutoff exists for Z. The D-159 reading sees G's matching revoke
frontier and accepts the preserved prefix. The latter is clearly the intended
answer, but an independent implementation cannot discard §4.2.

Define certificate validity as an explicit predicate over matching certificate
closures, grant validity independently over matching grant closures, and admit
only if both predicates hold. Remove any global “currently superseded” read
from the cert stage, replace the obsolete scalar shape in §4.2, and make the
two renewal-after-revocation vectors assert the cert-stage result as well as
the final outcome.

### B8. Recovery and key-freshness mirrors still describe different reducers

The main D-159 recovery rule says the omission blanket covers only lineages
enrolled at or before `base`; a later enrollment folds normally
([main rule](/Users/vm/owner-plane-d0a-spec.md:1747)). The E8 row still says an
omitted pair universally receives the implicit override
([E8](/Users/vm/owner-plane-d0a-spec.md:130)), and the normative CDDL commentary
does the same with no post-base exception
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3867)).

A first write by a lineage enrolled after recovery therefore admits under the
main rule and required vector but quarantines under the E8/CDDL reading.
Mirror the exact universe everywhere, including the non-obvious consequence
for a pre-base lineage later granted into a newly created zone.

The freshness reducer has similar executable drift:

- T3 compares raw `sig_pk` against a domain containing hashed
  `retired_keys`
  ([T3](/Users/vm/owner-plane-d0a-spec.md:825));
- the `c.enroll` registry still defines the signing domain as surviving
  enrollments plus `retired_keys`, omitting adopted renewals, and defines
  KEM history as literal “ever enrolled”
  ([registry](/Users/vm/owner-plane-d0a-spec.md:1334)); while
- D-150/D-158 say uncarried, unadopted cut-branch keys are reusable and adopted
  keys contribute typed history.

“Ever enrolled” recreates delivery-history dependence: a replica that saw an
unadopted cut certificate rejects reuse while one that never received it
accepts. Publish one formula in T3 and cite it from the registry:

`key_id = H_key({alg, pk})` for every comparison; surviving enrollment
history plus recovery `retired_keys` plus typed adopted history; no other
cut-branch bytes participate. Until this is normalized, D-158's original
security repair is not reliably executable.

## High-priority exactness repairs

### H1. Ordinary `w.gen` and `cap_eligible` still use different head predicates

D-144's cap predicate is deliberately held-chain membership, independent of
budget displacement
([cap anchor](/Users/vm/owner-plane-d0a-spec.md:1416)). Ordinary `w.gen`
now removes terminality but still requires `last_known` to be **accepted**
([ordinary rule](/Users/vm/owner-plane-d0a-spec.md:1983)).

If H is held on the canonical chain but budget-displaced, a W naming H can
still mint a cap under the first rule while being `body-invariant` under the
second. That may be a deliberate restriction-only posture, but D-159 says it
extended the same chain-membership rule into ordinary admission. Either remove
“accepted” or explicitly state that a body-rejected W can still cap and why.
Also replace the stale “terminal Head” input at
[cap pending](/Users/vm/owner-plane-d0a-spec.md:1456).

### H2. D-157 needs an outcome-to-basis table, not a claim of closure

The disposition row calls `no-grant`, scope failures, ceilings, and
`import-collision` control-derived facts, even though collision is a tenant
fact and several failures can be absence/derived predicates
([map](/Users/vm/owner-plane-d0a-spec.md:2203)). For every branch-relative
outcome that can close a transfer, name:

- the exact basis object/hash;
- the canonical choice when several sufficient causes coexist;
- the event that invalidates it; and
- whether the destination attempt itself re-enters or only its source journal
  reopens.

This table will expose whether a scalar basis is sufficient. It should include
`request-fork` and control-relative `body-invariant` instances, not only the
scope-space and collision examples currently in the vector prose.

### H3. Direct interval wording should be updated rather than globally qualified

Several earlier sentences still say “no terminal record,” “clears only on its
matching terminal,” or “never after XferAbort” without mentioning intervals
([recovery preface](/Users/vm/owner-plane-d0a-spec.md:1135),
[clear rule](/Users/vm/owner-plane-d0a-spec.md:1213)). The blanket at §6.1 says
to read all of them as “current interval,” but direct edits will prevent the
next implementer from treating the nearest normative sentence literally.

### H4. Decision rows should distinguish “decision selected” from “mechanically closed”

D-155 through D-159 are useful affirmative decisions, but phrases such as
“vocabulary is closed,” “portability is free,” and “made explicit” overstate
the current executable text. Keeping the decision record affirmative while
adding “schema/state-machine repair pending v0.5.14” would make it a more
accurate governance ledger.

## Required v0.5.14 vectors

Add these to §13 now so the companion cannot choose a different rule:

1. unresolved stage before/after its head, followed by an implicit strict
   consumer—same pending then accepted result;
2. staged lineage revoked before consumption, plus an unrelated device renewal
   in the same zone;
3. import A/B in both arrival orders, asserting exact loser outcome, charge,
   references, and revival;
4. earlier durable-but-proof-pending import versus later admissible import,
   proof arriving before/after the later claim;
5. two-record release whose root contains correct A and bogus B sibling:
   full-bundle and A-proof-only validation must agree;
6. encoder-exact versioned `bundleleaf` for 1/2/127/128 records, including
   exact expected sibling count and sides;
7. Reopen arriving before its invalidation evidence, and a historically valid
   Reopen followed by basis revival;
8. dual sufficient causes (for example retirement plus collision), asserting
   the same canonical basis and reopen trigger;
9. recovery base C5 adopting renewal C8, plus K0→K1→K2 where K1 reuse rejects
   and terminal K2 same-device recovery reuse follows the selected rule;
10. signing-only intermediate renewals on both sides of a KEM rotation,
    including the 64/65 boundary under the chosen list semantics;
11. device revocation for 0-author/0-wrap, 65-author/0-wrap, and
    0-author/65-wrap domains;
12. last-grant-revoked then renewal-omits-zone, asserting certificate stage,
    grant stage, and final prefix validity;
13. first write from a post-recovery enrollment under main prose, CDDL, and
    fresh fold; and
14. `w.gen(last_known=H)` where H is held/canonical but budget-displaced.

## Recommended repair sequence

1. **Freeze the small wire facts first:** versioned `bundleleaf`, adopted
   renewal ordering/list semantics, nullable revocation cutoffs, normalized key
   IDs, and recovery-blanket universe.
2. **Then close the three state machines:** staged consumption/reservation,
   import claimant status/reservations, and journal cause/reopen evidence.
3. **Publish one executable authority formula:** certificate closure ∩ grant
   closure ∩ epoch closure, and cite it from §4.2, §7.1, §10.2, CDDL comments,
   and vectors.
4. **Run a mirror sweep:** E8, registry rows, main prose, CDDL, decision record,
   and vector inventory must state the same predicates.
5. **Only then cut the D-91 companion and corpus.** At present they would
   necessarily decide several behaviors that the specification does not.

## Final assessment

v0.5.13 is directionally strong and notably willing to replace attractive but
insufficient mechanisms. The 65-head fix, lineage-wide reservation, total
cross-grant comparator, self-describing selected leaves, typed adopted history,
and selector intersection are all worth keeping.

The remaining work is less about discovering a new architecture than making
the chosen one survive its own delivery-order, recovery, and fresh-rebuild
tests. One focused v0.5.14 can plausibly do that. This cut cannot: it contains
arrival-relative required outcomes, an unversioned hash preimage, an inverted
recovery predicate, an unencodable legal revocation, and normative mirrors that
give opposite answers.

Gate A also remains explicitly false until the companion, corpus, surface
runs, and final discrepancy audit exist
([status](/Users/vm/owner-plane-d0a-spec.md:3373)). Repair the text first; then
let those artifacts test it rather than complete it.
