# Synthesized review: D0-A Core + Memory normative specification v0.5.7

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.7-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.7-review.md)
and
[owner-plane-d0a-spec-v0.5.7-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.7-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.7.
This document resolves disagreements rather than unioning the reports.*

## Executive verdict

**Both reviews correctly require v0.5.8 and withhold the present freeze. The
peer's blocking finding is exact: seal/snapshot retirement is a missing revisit
class. Its medium finding also identifies real destructive authority that
needs an explicit posture. The conclusion that every other D-107–D-113 area is
discharged and four sentences can finish the protocol is not supported by the
composed state machine.**

V0.5.7 remains a strong near-freeze cut. Preserve:

- per-generation abandon and the hosted requester-attested self-seal;
- snapshot-wins removal of fold-current cutoff equality;
- the explicit incorporation-cap direction;
- `recipientset.v`, central keying and bounded membership direction;
- consumed-boundary promotion, per-generation checkpoint-update intent and
  predecessor feed-closure wire;
- provisional recovery selection and complete per-entry Fence identity;
- direct issuer-fork naming, transfer critical-section ownership and
  PendingXfer dormancy; and
- the many repaired registry/CDDL/decision mirrors.

The adjudicated residue is broader than the peer's B1:

1. per-generation incorporation/seal caps still compose with one scalar
   ratify Head, making later-generation cleanup ambiguous or literally
   invalid;
2. D-108 never relates its multi-head snapshot boundary to scalar
   `accepted_through`;
3. boundary retirement is a missing revisit path and can invalidate an
   already escaped effect; seal lifetime after its creating `w.gen` loses
   standing is also undefined;
4. direct issuer-fork discovery has no portable winner or fold position;
5. renewal's active-epoch deletion predicate is non-monotone across queued
   epochs, wrap-key equality is not global, and recipient/held-zone caps are
   not exact portable sets;
6. recovery's new per-rotation representation contradicts E7, is unbounded
   under 64 KiB, and does not preserve effective wrap-add state;
7. checkpoint/feed live prose still contradicts D-111's intended reducer;
8. the transfer mutex does not survive a durable pending attempt across crash,
   dormant releases can reuse `export_id`, and rejection cleanup conflicts
   with inherited release finality; and
9. the Gate-A artifacts remain absent.

Recommended disposition:

- **Direction:** accept and preserve.
- **V0.5.8:** required; substantive but focused.
- **Protocol/schema freeze:** no.
- **Gate A:** no. A repaired v0.5.8 may become the audit baseline; the
  companion/core/corpus/harness, family 14 and discrepancy audit still follow.
- **Durable P1 writes:** remain prohibited under the unchanged later gates.

## Adjudicated decision ledger

| Decision | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-107 | Complete except revisit/M1 | **Partial:** correct wire direction; scalar ratify composition, retro-effects and cap lifetime remain |
| D-108 | Complete except revisit | **Partial:** arrival race fixed; snapshot/scalar algebra, retro-effects and stale normative text remain |
| D-109 | Complete | **Open/partial:** active-only deletion is non-monotone; equality/cap sets incomplete |
| D-110 | Complete | **Schema fix complete; cardinality partial:** “current epoch” does not bound every queued accepted epoch |
| D-111 | Complete | **Mostly complete:** live checkpoint mirrors, multi-cover fence and T3 closure semantics remain |
| D-112 | Complete | **Partial:** E7 contradiction, unbounded adoption and effective-wrap closure remain |
| D-113 | Complete except revisit | **Partial:** direct-fork order, pending-attempt crash, export identity and rejection cleanup remain |
| D-91/Gate A | Artifact-pending | **Artifact-pending:** agrees; independently withholds Gate A |

## Assessment of the peer review

### Findings to adopt

1. **B1 is correct.** D-107 seals and D-108 snapshot cutoffs deliberately
   transition accepted operations to quarantine, yet T2 still says there are
   exactly four revisit paths and the abandon row says seals are “outside”
   them.
   ([four paths](/Users/vm/owner-plane-d0a-spec.md:661),
   [cutoff/abandon rows](/Users/vm/owner-plane-d0a-spec.md:1128))
2. **Deriving the revisit inventory is better than repeatedly renumbering
   it.** Reference the boundary algebra's retiring/reviving events, or add a
   fifth class with an explicit exhaustive definition.
3. **M1's authority concern is real.** A trusted admin can seal below accepted
   history, and `at = "none"` can void a whole generation. That is
   recovery-grade truncation in effect and must be either constrained or
   deliberately documented/vector-pinned.
4. **The permanent seal lifecycle needs an exact disposition.** Abandon is not
   a normal reproposal lane: a later ratify boundary may never revive beyond
   it.
5. **The proposed artifact sequence is correct.** Companion first, then core/
   harness/corpus/family 14/surfaces/discrepancy audit.

### Corrections to the peer report

#### The missing-E8-row pin is false

E8 already contains `c.abandon_writer seals ≤ 64`.
([E8](/Users/vm/owner-plane-d0a-spec.md:118))

#### The disposition pin must distinguish abandon from ratify snapshot

The peer groups seal and snapshot boundaries as permanent. An abandon seal is
permanent; a snapshot `c.cutoff` is still ratify purpose and can be superseded
by later ratify growth. §10.5 should distinguish permanent seal quarantine
from the derived/revivable ratify cutoff lane rather than labeling both
permanent.
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1668))

#### M1's conservative lower bound is not portable as proposed

Requiring `seal.at ≥ current accepted terminal at the seal's fold position`
recreates the fold-current cross-log comparison D-108 removed: two replicas
can hold different tenant successors when the control op arrives. The posture
must either be carried/snapshot-based, or explicitly grant admin/requester
retroactive truncation and accept the escaped-effect residual. M1 also applies
to hosted self-seals and stale snapshot cutoffs, not only the trusted form.

### Where the peer overcredits v0.5.7

#### D-107's cap is per generation in intent, scalar in the live algebra

The spec says a ratify cutoff exceeding incorporated H is `body-invariant` and
uses lexicographic `(gen,seq)` ordering. `zonecutoff` still carries one Head.
After H1 is incorporated by W2, a legitimate cutoff at H2 in generation 2 is
greater than H1 and therefore literally crosses the cap. The same problem
affects later cutoffs after an earlier per-generation seal.
([incorporation cap](/Users/vm/owner-plane-d0a-spec.md:1172),
[comparator](/Users/vm/owner-plane-d0a-spec.md:1181),
[zonecutoff](/Users/vm/owner-plane-d0a-spec.md:2775))

The fold needs per-generation intersection/clamping: advancement in a later
generation remains legal, while revival past H in the **same** incorporated or
sealed generation rejects.

#### D-108 carries two uncomposed boundaries

The multi-head `live_heads` set is declared “THE boundary,” while scalar
`accepted_through` remains the max-composed ratify boundary. No invariant says
what happens when the carried set includes g2:H2 but `accepted_through` is
g1:H1. Define per-generation snapshot caps plus scalar-prefix intersection, or
another exact algebra; equality with the max is a simple option only if
intentional partial cleanup is abandoned.
([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1128),
[cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2788))

#### Adding a fifth revisit label does not solve escaped effects

A solo effect can execute, then be voided by `cabandon {gen:1, at:none}`. A
device can sign snapshot cutoff H1, later execute H2, then have delayed H1
retire it. The bytes cannot be un-sent. Either constrain those boundaries with
a portable carried predicate or extend the residual/product warning to
owner-authorized boundary retirement.

`w.gen` is itself charged and proof-bearing. If the accepted W that created an
incorporation cap later retro-quarantines, specify whether the cap persists as
a monotone side effect or disappears and places any dependent escaped effect
inside the residual.

#### Direct issuer-fork discovery remains arrival-relative

Without a committed head, there is no “losing branch.” A sees S then S′; B
sees S′ then S. D-113 currently places retro-quarantine at the arriving
statement's admission. Conservatively freeze qualifications from **both**
suffixes at/after the fork until a portable boundary selects one, or define a
deterministic selector.
([T2](/Users/vm/owner-plane-d0a-spec.md:661),
[T3](/Users/vm/owner-plane-d0a-spec.md:674))

#### D-109's activation predicate can become true too early

With E2 active and E3/E4 accepted/queued, renewal gives E4 a Knew wrap. Adding
a Knew wrap to active E2 makes “every active epoch is renewed” true, permitting
Kold deletion. E3 then activates with only its old-key wrap. Retain Kold until
the renewal-time target activates or until every accepted/unretired epoch that
can become active has Knew coverage.
([renewal](/Users/vm/owner-plane-d0a-spec.md:1114),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2674))

Current-key equality is written only for `c.wrap_add`, not every wrap-bearing
operation, and “held zone” is not defined as grants, effective wraps or their
union. D-109 therefore does not yet supply one portable renewal set.

#### D-110 does not yet prove every Fence constructible

`c.wrap_add` may target any accepted epoch. “Current-epoch recipients” can be
read as active or latest accepted, leaving an older queued E2 able to collect
257 recipients after E3 is accepted. Enforce ≤256 per `(zone, accepted epoch)`
that may still Fence and across every wrap-bearing operation.

#### D-112's new representation conflicts with its central schema and cap

E7 still keys `adopted_rotations` only by `zone_id`; Appendix A and D-112 key
it by `(zone_id, rotation_op)`. A strict decoder rejects the multiple same-zone
entries D-112 requires.
([E7](/Users/vm/owner-plane-d0a-spec.md:88),
[adoption CDDL](/Users/vm/owner-plane-d0a-spec.md:2835))

The contiguous full-Fence entry list is also unbounded under a 64-KiB control
operation. A long offline fork can contain more activated rotations than one
recovery op can encode. Use a compact authenticated activation chain/proof,
bound the divergence posture, or explicitly accept forced storage orphaning.

D-97 says the adopted activation's epoch/wraps survive, but the current
dependency closure names only certificates/descriptors and erase manifests.
It must explicitly preserve the effective/superseding `c.wrap_add` map through
each adopted `control_frontier` as storage state, without reviving cut control
authority.

#### D-113's mutex is not a durable attempt protocol

A destination import may be durable but pending proof/effect finality across a
crash. Terminal completion counts accepted/effect-final imports only; nothing
requires recovery to retain/defer on that pending attempt. Recovery can abort
it missing after source erasure, then delayed proof admits it after XferAbort.

Dormancy also exposes identity reuse: `export_id` is reserved only by an
accepted release. A displaced R1 can free X for R2; R1's revival leaves two
journals. Terminal records key only by X, and replay identity lacks the
authorizing `release_op`. Finally, a reject-permanent release cannot be
effect-final, so its XferAbort must be classified as immutable-rejection
journal cleanup rather than release-authorized effect.
([transfer](/Users/vm/owner-plane-d0a-spec.md:981),
[export identity](/Users/vm/owner-plane-d0a-spec.md:1937))

## Consolidated v0.5.8 change set

### 1. Finish the boundary algebra and lifecycle

- Make ratify advancement/caps explicitly per generation or define the exact
  scalar-to-per-generation intersection.
- Define how `live_heads` and `accepted_through` compose.
- Add boundary retirement to the revisit inventory by derivation, not another
  fragile count.
- Give permanent abandon and revivable ratify snapshot distinct dispositions.
- Choose and state the retroactive-truncation/escaped-effect posture.
- Define whether an incorporation cap survives loss of its creating `w.gen`.
- Remove the stale family-9/D-87 “assent dies on later write” assertions.

### 2. Make direct issuer-fork handling order-independent

Quarantine/freeze both conflicting suffixes until an owner boundary selects a
branch, or specify another portable deterministic winner. Pin both delivery
orders and the later boundary-selection transition.

### 3. Make renewal and membership predicates monotone and portable

- Gate Kold destruction on the renewal-time target or complete Knew coverage
  of every intervening accepted/unretired epoch.
- Apply current/live recipient-key equality to every new wrap, with explicit
  same-operation enrollment/renewal exceptions.
- Define `held_zones` once and use it on every carrier.
- Enforce the recipient cap per accepted epoch that may Fence, across every
  wrap-bearing operation.

### 4. Bound and complete recovery adoption

- Repair E7's logical key.
- Replace or bound the unbounded contiguous Fence tuple list.
- Preserve the exact effective wrap map through each adopted frontier as
  storage state; retain certs/descriptors only for validation and erasure
  evidence as authoritative.
- Clarify that accepted-but-unFenced rotations are cut/reissued rather than
  adopted.

### 5. Finish checkpoint and feed exactness

- Replace the remaining “latest page per lineage” live row/CDDL text with
  latest entry per `(lineage,generation)`.
- Require a lineage fence to dominate every relevant generation cover.
- Integrate renewal feed closure into T3's scope/ancestry/minimum rules.

### 6. Give transfer durable attempt and release identity

- Persist per-record in-flight attempt identity or add a portable abort fence;
  a live mutex alone is insufficient across crash.
- Reserve export identity monotonically under a portable collision rule or
  add `release_op` to journal/terminal/replay keys.
- Separate reject-permanent cleanup from release effect-finality and define
  post-final retro-quarantine behavior.

## Peer pins and mechanical sweep

- **Reject** the missing seal-cap pin: E8 already has it.
- **Adopt with correction** the disposition pin: permanent abandon and
  revivable ratify snapshot need distinct contextual lifecycle.
- Update E7's adoption key, the `c.wrap_add` registry mirror, checkpoint live/
  CDDL wording, T3 renewal closure, and `cabandon.seals` empty-array posture.
- Keep the peer's rejection of its old cutoff-formula pin; that formula remains
  correct.

## Gate-A sequence

After v0.5.8 resolves the protocol choices above:

1. author `d0a-vector-cases.v1.json` first;
2. build the independent owner-plane core/harness;
3. generate the corpus with every ordering/cardinality/crash trace above;
4. record family 14;
5. run every required surface; and
6. perform the final prose↔schema↔vector discrepancy audit.

Only then is Gate A true. V0.5.8 may be the audit baseline; it is not the Gate-A
result by itself.

## Final recommendation

Cut v0.5.8. Adopt the peer's missing-revisit diagnosis and destructive-seal
posture question, reject its complete-discharge ledger and false E8 pin, and
retain the first review's dimensional, temporal, boundedness and crash traces.
The remaining work is focused, but it is genuine protocol work—not four table
sentences—and it belongs in prose/CDDL before the corpus is allowed to freeze
bytes around it.
