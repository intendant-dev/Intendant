# Synthesized review: D0-A Core + Memory normative specification v0.5.9

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.9-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.9-review.md)
and
[owner-plane-d0a-spec-v0.5.9-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.9-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.9,
SHA-256 `560728bd6f43d1ab30bd568b0af5f4640cb215ef5e5325d7bba0a30ba932abeb`.
This synthesis resolves disagreements by replaying the composed protocol; it
does not average the two verdicts or union their findings.*

## Executive verdict

**Cut v0.5.10. Do not freeze v0.5.9 or declare it the candidate audit
baseline.**

The peer review is useful as a narrow regression-discharge audit. It confirms
that the requested D-120–D-126 fields, CDDL shapes, mirrors, and ledger
supersession pointers landed. It also contributes one valid pin:
`ratified_through` has no named consumer. Its positive traces show that the
draft now handles several cases better than v0.5.8: snapshot override followed
by later growth, held-versus-accepted cap convergence, `export_id` reuse,
accepted cross-carrier commitments, Fenced/unFenced admission parity, and the
one-record destination-rejection abort.

Those results are necessary, but not sufficient. The peer's conclusion that
there are no blockers is disproved by a construction equation:

```text
release_op = H_op(release whose body contains
                  H_bundle(bundle containing release_op))
```

No release can be constructed as specified. The peer says the bundle ripple
was verified, but it verified the completed object shape rather than tracing
how the object can be minted.

The peer also conflates convergence with authorization in D-122. A set-derived
cap can converge perfectly while still giving a revoked, read-only, or
wrong-lineage signing key permanent truncation authority. “Fail closed” is not
a substitute for IAM when the restriction changes another principal's
durable history and may confer effect finality.

The remaining high findings are likewise composition gaps rather than a
return to architecture debate:

- D-121 is not a total equation and can ignore an explicit zero-history
  ratification in favor of a larger requester snapshot;
- exact Head references and D-124 commitments still have arrival-dependent
  missing-evidence and pending-carrier paths;
- recovery does not totalize removal, revival, or omitted-lineage behavior;
- D-125 can re-add a device intentionally excluded by a later epoch and can
  retain Kold forever;
- logical import replay has no byte-distinct collision rule; and
- D-126 omits permanently non-revivable quarantine from cleanup.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-120, D-123 identity, D-124 registry, D-125 domain split, D-126 reason
  split:** keep, then finish.
- **Protocol/schema freeze:** no.
- **Candidate audit baseline:** no.
- **V0.5.10:** required; focused but substantive.
- **Gate A:** false independently because the companion artifact and all later
  corpus/surface work remain absent.
- **Durable P1 writes:** remain prohibited under the existing Gate-B and
  umbrella prerequisites.

## Assessment of the peer review

### What it did well

The peer report is a careful literal incorporation audit. It checks the new
wire split, E7 keys, reason enum, mirrors, ledger pointers, and several traces
the v0.5.8 synthesis expressly requested. Its positive accounting is worth
preserving:

- D-120 really does fix the v0.5.8 keyed-set collision;
- D-121's override-then-growth mechanism fixes the specific H3/H2/H3 trace
  when the entry and snapshot agree;
- D-122 replaces an arrival-history fact with a held-byte set fact;
- D-123 correctly removes replicated `export_id` reservation;
- D-124 gives the three commitment carriers one registry and makes renewal
  closure a retro-disqualification event;
- D-125 removes local Fence state from portable admission; and
- D-126 closes the earlier one-record `reject-permanent` abort hole.

Its P1 is also valid: `ratified_through` is defined, while effective admission
reads only `admit_bound` and `immutable_cap`
([equation](/Users/vm/owner-plane-d0a-spec.md:1304)).

### Why its verdict overcredits the text

The peer applies the three-run convergence test to the narrow prior
counterexamples, but three additional questions are required before a
protocol can freeze:

1. **Constructibility:** can the signed/hashed bytes be generated without a
   dependency cycle?
2. **Authority:** is every convergent state transition authorized by the
   correct IAM and branch-relative principal?
3. **Lifecycle closure:** do missing dependencies, recovery removal, revival,
   permanent quarantine, and logical collisions have total outcomes?

The peer's method misses one class at each boundary:

- schema verification misses the D-123 self-hash;
- same-byte convergence misses D-122's authority amplification;
- accepted-carrier traces miss earlier-pending D-124 carriers;
- Fenced/unFenced parity misses exclusion followed by KEM renewal; and
- the previous one-record abort trace misses permanent quarantine under a
  different disposition name.

Therefore adopt the peer's wire-verification evidence and P1, but reject its
“all six discharged,” empty severity ledger, no-version-bump recommendation,
and instruction to stop cutting prose. The first review's v0.5.10 verdict is
the evidence-supported one.

## Adjudicated decision ledger

| Decision | Peer disposition | Synthesized disposition |
|---|---|---|
| D-117 | Implicitly complete | **Substantially coherent; vectors pending** |
| D-118 | Implicitly complete | **Substantially coherent; vectors pending** |
| D-120 | Complete | **Preserve; partial exactness remains around requester zero-history and scalar consumption** |
| D-121 | Complete except consumer pin | **Open:** no top state, entry/snapshot contradiction, dead/unnamed state, scalar promotion undefined |
| D-122 | Complete | **Unsafe as written:** convergence gained by granting signature-only destructive authority |
| D-123 | Complete | **Identity direction correct; release construction broken; import/effect identity incomplete** |
| D-124 | Complete | **Registry direction correct; pending, reservation, recovery, and scope lifecycle incomplete** |
| D-125 | Complete | **Portable/local split correct; current-membership predicate wrong** |
| D-126 | Complete | **Reason split correct; terminal predicate incomplete** |
| D-91 / Gate A | Artifact-pending | **Artifact-pending; agreed** |

## Critical 1: D-123 makes releases unconstructible

Four normative facts form a cycle:

1. `content_digest = H_bundle(canonical bundle bytes)`
   ([§11.8](/Users/vm/owner-plane-d0a-spec.md:2062));
2. the bundle contains `release_op`
   ([bundle CDDL](/Users/vm/owner-plane-d0a-spec.md:3212));
3. the release body contains `content_digest`
   ([release CDDL](/Users/vm/owner-plane-d0a-spec.md:3245)); and
4. `release_op` is the hash of the complete signed release operation
   ([O2](/Users/vm/owner-plane-d0a-spec.md:521)).

This is an infeasible hash/signature fixed point, not an ordering problem. A
fixture that accepts both values as inputs can validate the CDDL while hiding
the fact that no producer can create them.

**Repair:** remove `release_op` from the bundle's hashed preimage. Hash a
bundle such as `{ v, export_id, recs }`, transport the verified signed release
alongside it, and derive `release_op = H_op(release)` after signing. Local
`PendingXfer` and terminal records can continue to carry the derived hash.
Require a vector that starts with source records and keys, constructs the
release, then independently re-derives the bundle digest and `release_op`.

This finding alone prevents freeze and invalidates the peer's recommendation
to proceed directly to fixtures.

## Critical 2: D-122 turns signature possession into truncation authority

D-122 creates an immutable incorporation cap for every held `w.gen` whose
header signature verifies under its named certificate
([cap rule](/Users/vm/owner-plane-d0a-spec.md:1267)). Normal `w.gen`
admission requires much more: an op-authoring grant, correct tenant and zone,
lineage ownership, generation-window and budget eligibility, exact next-
generation arithmetic, and a valid terminal `last_known` Head in the same
zone and lineage
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1604),
[verb classes](/Users/vm/owner-plane-d0a-spec.md:1864)).

The peer is right that a held-byte rule makes cap existence independent of
budget arrival order. It is wrong that this makes the rule safe. A revoked,
read-only, superseded, cut-branch, or wrong-lineage signer can create an
otherwise inadmissible `w.gen` naming `H1`; replicas then converge on a
permanent cap at `H1`, truncating valid `H2...` history. Effect finality also
names `last_known` incorporation as an immutable closure without a separate
eligibility predicate
([effect finality](/Users/vm/owner-plane-d0a-spec.md:417)).

This violates the Owner Plane authority boundary even though it is
restriction-only and deterministic.

**Repair:** define a portable, branch-relative `cap_eligible` predicate. It
may deliberately omit revisable budget/time proof, but it must include strict
parse/body hash, installed and position-valid certificate/grant, device-to-
lineage ownership, zone/capability/revocation/supersede/recovery boundaries,
generation arithmetic, and exact same-zone/lineage terminal-Head validity.
The alternative is an explicit control-authorized incorporation operation.

Also state whether pre-admission caps confer effect finality, how C3′ removes
cut-branch cap authority, and how rejected cap-bearing bytes survive
checkpoint/compaction/GC. Vector both the valid-but-budget-displaced case and
unauthorized/revoked/wrong-lineage/cut-branch negatives.

## High 1: D-120/D-121 boundary state is not total

The peer's P1 belongs in a larger semantic cluster, not a one-clause pin.

### Missing identities

`admit_bound` has no initial value and `min(empty immutable caps)` has no top
identity. Absence cannot silently equal wire `"none"`, because `"none"` is an
explicit zero-history decision and E4 forbids unstated defaults
([equation](/Users/vm/owner-plane-d0a-spec.md:1304)). Define distinct
`Top/Absent`, `Zero/"none"`, and `Head(H)` states.

### The explicit requester entry can be ignored

Requester form permits `accepted_through <= carried_head` and calls the entry
the ratification, with the snapshot as its outer bound
([`c.cutoff`](/Users/vm/owner-plane-d0a-spec.md:1209)). The equation then
overrides `admit_bound` to the carried Head. Therefore a valid body containing
`accepted_through: "none"` and carried `H5` can admit through H5; an H1 entry
with carried H5 similarly has no admission effect. `ratified_through` is not
read by the admission formula.

Choose one normative posture:

- require entry equality with the carried Head, plus an explicit no-Head
  zero-history case; or
- keep `<=` and make the entry the post-operation boundary, using the snapshot
  only as freshness/outer cap; or
- publish another exact equation that consumes both values.

### Per-generation state cannot silently become scalar state

Ratify is now per generation, while supersede and close remain scalar
`zonecutoff` boundaries
([algebra](/Users/vm/owner-plane-d0a-spec.md:1220),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3031)). A renewal or epoch advance
may consume several staged ratify entries and “materialize that boundary's
value” as scalar immutable state
([promotion](/Users/vm/owner-plane-d0a-spec.md:1357)). No reduction exists
for values such as `g1 -> H10`, `g2 -> "none"`, and `g3 -> H5`.

Require the consuming operation to carry an explicit scalar cutoff, or define
an exact no-widening reduction. Explicit carriage is easier to inspect.

### Exact-Head references are still arrival-dependent

An unheld exact Head pends, but a different hash at the coordinate permanently
rejects the boundary
([reference lifecycle](/Users/vm/owner-plane-d0a-spec.md:1336)). `H_A` then
cutoff accepts; `H_B` then the cutoff rejects; later both replicas can hold
`H_A`, `H_B`, and the cutoff with different durable results.

Make reference resolution a set property: the named exact hash selects, any
coordinate fork freezes, or both invalidate—whichever posture the owner
chooses, both arrival orders and fresh replay must agree.

Finally, requester zero-history needs an explicit schema exception: an entry
with `accepted_through: "none"` names a generation that cannot be “present in
the carried live Heads” when that generation has no Head.

## High 2: D-124 has a registry, not yet a registry state machine

The cross-carrier registry and renewal-closure revisit are correct additions.
Four lifecycle rules remain.

1. **Missing ancestry:** a new commitment that cannot yet prove an ancestry
   path must be `issuer-gap`/`ref-unresolved` pending, not permanent
   `body-invariant`. The generic chain rule already distinguishes missing
   paths from proven forks, while the new registry paragraph says
   `body-invariant` “otherwise”
   ([T3](/Users/vm/owner-plane-d0a-spec.md:710)).
2. **Earlier pending reservation:** if earlier C1 commits branch B but pends,
   later C2 must not accept branch A and then become impossible to reconcile
   when C1 resolves. Either C1 reserves that issuer scope and later carriers
   pend, or selection order must be redefined. Scope reservation best matches
   “first in control order.”
3. **Recovery removal:** when C3′ cuts the selecting checkpoint/cutoff/closure,
   both suffixes must refreeze; removing a closure/cutoff may revive proofs.
   The exhaustive revisit list names additions/narrowing but not removal of
   these control facts
   ([T2](/Users/vm/owner-plane-d0a-spec.md:675),
   [C3′](/Users/vm/owner-plane-d0a-spec.md:1398)).
4. **Scope and fork exactness:** define fork point as the last common ancestor
   so a commitment through `k-1` selects neither and `A_k` selects A. Prevent
   signing-key sequence `A -> B -> A`; either prohibit every historical key
   reuse or include certificate incarnation in issuer scope
   ([scope](/Users/vm/owner-plane-d0a-spec.md:710)).

Vector all three carrier pairings, both control orders, missing-chain-first,
earlier-pending/later-conflicting, ancestor/descendant, common-ancestor versus
first-divergent, C3 removal, and historical key reuse.

## High 3: recovery and authority-widening events remain incomplete

C3′ says cut-branch tenant operations quarantine “per `tenant_cutoffs`,” but
the set may be empty, is capped at 256, and has no continuation while the
protocol permits up to 4096 Frontier heads
([E8](/Users/vm/owner-plane-d0a-spec.md:129),
[C3′](/Users/vm/owner-plane-d0a-spec.md:1398)). There is no omission default
for an unnamed lineage. Choose preserve, implicit zero-history, or invalid
incomplete coverage, and make the posture representable through paging,
continuation, or an explicit default.

The same root cause appears in two existing operations:

- `c.lineage_reauth` is the stated remedy for `lineage-gen` and opens a new
  generation window, but generation-window growth is absent from the
  supposedly exhaustive revisit inventory
  ([reauth](/Users/vm/owner-plane-d0a-spec.md:1207),
  [lineages](/Users/vm/owner-plane-d0a-spec.md:1600)); and
- `c.space_retire` says later writes reject but carries no tenant cutoff or
  capability-epoch anchor, so a delayed pre-retirement write is
  indistinguishable from a post-retirement mint under the old grant
  ([registry](/Users/vm/owner-plane-d0a-spec.md:1201)).

Add removal/widening events to the derived reducer. For reauth, explicitly
choose revival of held bytes or mandatory reproposal. For space retirement,
advance a signed epoch with closure coverage, carry per-lineage cutoffs, or
add a dedicated position-relative space-write boundary.

## High 4: D-125 conflates historical membership, current access, and custody

The peer correctly verifies that local Fence retirement no longer changes
portable admission. It does not test exclusion followed by renewal.

`held_zones(device)` includes a zone if the device has an effective wrap at
**any accepted epoch**, and renewal emits a wrap at the zone's latest accepted
epoch for every such zone
([definition](/Users/vm/owner-plane-d0a-spec.md:460),
[`c.enroll`](/Users/vm/owner-plane-d0a-spec.md:1195)). Thus:

1. E1 includes device D;
2. E2 intentionally excludes D;
3. the historical E1 wrap keeps the zone in D's effective-wrap set; and
4. a later KEM renewal must add D to E2.

Renewal has restored access that E2 deliberately removed. The local Kold
predicate also waits for Knew coverage in every accepted, locally unretired
epoch of every effective-wrap zone; an excluding epoch intentionally has no
wrap, so Kold can become undeletable
([renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2941)).

Split three predicates:

1. monotone historical finite-zone state for the 128-zone admission cap;
2. current intended membership, derived from the latest accepted membership
   epoch, for renewal; and
3. actual locally unretired `(zone, epoch, device)` tuples whose effective
   wrap targets Kold, for destruction custody.

Absence from an epoch must not require a Knew wrap. State whether historical
membership permanently consumes a cap slot, and vector exclusion, queued
epochs, divergent Fence progress, and eventual Kold destruction without
re-admission.

## High 5: D-123/D-126 transfer identity and terminality are incomplete

### Logical import collisions

One destination replay key is `(from_plane, release_op, source_op)`, but
`m.import.claim` permits byte-distinct sensitivity, times, provenance,
evidence, labels, headers, and operation hashes
([import rule](/Users/vm/owner-plane-d0a-spec.md:2135),
[`mimport`](/Users/vm/owner-plane-d0a-spec.md:3255)). Two authorized writers
can therefore produce different valid claimants under one logical key.
First-seen-wins diverges, accepting both violates idempotency, and
byte-identical `duplicate` does not apply.

Choose a designated writer, a fully derived deterministic import, or an
order-independent collision/freeze rule with budget and chain effects. Vector
both arrival orders and fresh replay.

The execution sentence is also too coarse: imports are said to be idempotent
per `release_op`, but one release intentionally produces up to 128 imports
([effect execution](/Users/vm/owner-plane-d0a-spec.md:441)). Publish exact
effect keys:

| Effect | Key |
|---|---|
| source release delivery and source terminal | `release_op` |
| one destination import | `(from_plane, release_op, source_op)` |
| audited result release | `read_id` |

Pin equality among the release, bundle, `PendingXfer`, import provenance, and
terminal mirrors, or remove redundant fields.

### Permanent quarantine is not terminalized

D-126 closes a dormant journal when its release resolves
`reject-permanent`, and admits destination attempts to `missing` under named
negative outcomes
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:1053)). But an operation
beyond `immutable_cap` or in a voided generation remains labeled
`quarantine-reproposal` while being permanently non-revivable
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1830)).

An accepted release plus durable `PendingXfer` can later be sealed beyond the
immutable bound. It can never revive, yet no cleanup branch matches it. A
destination import can leak the same way.

Define terminality over **permanent non-revivability**, not the spelling of a
disposition. Every revivable proof/budget/snapshot/recovery state stays
unresolved; effect-final accepted attempts complete; permanently
non-revivable or never-arrived records become eligible `missing`.

## Required owner rulings

V0.5.10 needs six explicit choices; these should not be delegated to fixtures:

1. **Requester boundary:** does `accepted_through` equal the carried Head, or
   is it the effective inner bound beneath a snapshot? How do per-generation
   entries become scalar close/supersede state?
2. **Cap authority:** what exact branch-relative predicate creates an
   incorporation cap, does it confer effect finality before admission, and
   how does recovery remove it?
3. **Fork/reference lifecycle:** does an exact Head select a tenant fork or
   freeze it, and does an earlier pending issuer commitment reserve its scope?
4. **Recovery/retirement:** what does omitted `tenant_cutoffs` mean, how is
   large coverage represented, and what position-relative boundary ends space
   writing?
5. **KEM membership:** which state is historical cap usage, which is current
   access, and which actual Kold-wrapped tuples block key destruction?
6. **Import/terminal identity:** who wins a byte-distinct same-key import
   collision, and which permanent-negative states close transfers?

Recommended defaults are: explicit scalar cutoffs on consumers; a complete
canonical cap-eligibility predicate or control-carried incorporation; an
earlier-pending scope barrier; an explicit fail-closed recovery omission plus
a scalable continuation; latest-epoch intended membership separated from
historical usage; and permanent-non-revivability as the terminal predicate.

## Vector and schema delta

Before the companion freezes, add at least:

- release construct-and-rederive, proving the hash graph is acyclic;
- unauthorized/revoked/wrong-lineage/cut-branch caps, cap finality, and cap
  recovery removal;
- absent versus explicit `"none"`, `none + carried H5`, entry below snapshot,
  requester zero-history, and multi-generation scalar promotion;
- exact-Head conflicting arrival in both orders plus fresh fold;
- D-124 missing ancestry, earlier pending reservation, all carrier pairings,
  recovery removal, and historical key reuse;
- recovery omitted-lineage and >256 coverage; reauth revival/reproposal; and
  retire/write arrival orders;
- E1 include, E2 exclude, KEM renewal, queued epochs, and Kold deletion;
- byte-distinct same-triple imports in both orders; effect-key granularity;
  permanent-quarantine source and destination cleanup; and
- D-117 accepted-but-unFenced non-adoption plus D-118 same-lineage
  multi-generation omission and insufficient-fence negatives.

Also pin that every requester Head belongs to the requester's lineage and its
outer zone, that `cabandon.seals[].at` resolves in the body's outer zone, and
that zero-history has a representable requester form.

## Recommended v0.5.10 sequence

1. Remove `release_op` from the bundle digest preimage and require a real
   construction vector.
2. Make the six owner rulings above.
3. Totalize boundary identities, requester composition, exact-Head references,
   scalar promotion, recovery removal, reauth, and retirement.
4. Complete D-124's missing-evidence, pending-reservation, recovery, fork, and
   scope state machine.
5. Split D-125's historical, current-membership, and Kold-tuple predicates.
6. Complete D-123/D-126 import collision, effect-ID, mirror-equality, and
   permanent-negative terminal rules.
7. Align prose, CDDL, decision ledger, and vector requirements.
8. Only then author `d0a-vector-cases.v1.json`, followed by core, corpus,
   family 14, required surfaces, and the discrepancy audit.

## Bottom line

The peer review confirms that v0.5.9 faithfully incorporated the requested
surface changes. It does not establish that the composed protocol can be
constructed, that every convergent restriction is authorized, or that every
pending/removal/terminal lifecycle closes.

V0.5.9 remains a strong near-freeze draft, but it is not a candidate baseline.
Cut v0.5.10. None of the required work reopens the Owner Plane or Memory
architecture; it finishes the reducer and authorization contracts those
designs already chose.
