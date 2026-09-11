# Review: D0-A Core + Memory normative specification v0.5.7

*2026-07-12. Fresh review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.7,
diffed against the archived v0.5.6 and replayed against the adjudicated
[v0.5.6 synthesis](/Users/vm/owner-plane-d0a-spec-v0.5.6-synthesized-review.md).
The review first retested D-107 through D-113 against the exact traces that
motivated them, then composed the new per-generation boundaries, renewal
custody, recovery adoption and transfer lifecycle in fresh multi-replica and
crash traces.*

## Executive verdict

**V0.5.7 is another major convergence cut and directly addresses essentially
every v0.5.6 finding. It is not yet a safe protocol freeze. I recommend a
focused v0.5.8 before authoring the companion schema.**

The cut gets a great deal right:

- per-generation abandonment is the correct shape for preserving a later
  live branch, and hosted self-seal authority is explicitly bounded;
- cutoff snapshot-wins removes the old control-first/tenant-first equality
  race;
- `last_known` is now explicitly intended as an immutable incorporation cap;
- held-zone and recipient cardinalities are bounded, and `recipientset` is
  versioned and centrally keyed;
- consumed ratify boundaries are promoted into immutable scoped state;
- checkpoint replacement is stated per `(lineage, generation)`, renewal
  carries a predecessor-feed closure, and closed feeds leave hardening;
- recovery selection is explicitly provisional, full Fence identity is on
  the wire, and predecessor activations are individually representable;
- direct issuer-fork discovery, transfer critical-section ownership and
  PendingXfer dormancy are finally named; and
- most of the stale mirrors identified in the synthesis were repaired.

The remaining defects are primarily **new seams created by those repairs**:

1. ratify remains one lineage-scalar Head while incorporation and abandon
   caps are per generation; the literal total comparator can reject every
   later-generation cutoff after an ordinary incorporation;
2. D-108 says carried `live_heads` are the boundary but never relates that
   multi-head set to the still-present scalar `accepted_through`;
3. abandon and snapshot-wins cutoff can retroactively quarantine an already
   executed effect, while the spec calls revisit paths exhaustive and omits
   this residual;
4. direct issuer-fork discovery still chooses a “losing” branch at an
   arrival-relative discovery position;
5. predecessor-KEM deletion can become true at active epoch E2 and false again
   when queued E3 activates; current-key equality and recipient caps are not
   yet global across every wrap-bearing path/accepted epoch;
6. recovery's contiguous per-rotation adoption list is unbounded under a
   64-KiB control op, its E7 key contradicts the new CDDL, and effective
   wrap-add state is still missing from dependency closure;
7. a durable-but-pending destination import can accept after crash recovery
   wrote XferAbort, and dormant releases can reuse `export_id`; and
8. live checkpoint/T3 prose retains several superseded rules.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-107–D-113:** substantial implementations of the rulings; all remain at
  least partly open except D-111's consumed-boundary promotion itself.
- **Protocol/schema freeze:** no.
- **Gate A:** no. The companion/core/corpus/harness and family-14 result remain
  absent independently of the protocol findings.
- **Next cut:** v0.5.8, confined to the dimensionality, custody, recovery and
  crash-lifecycle decisions below.
- **Durable P1 writes:** unchanged; still prohibited until the later gates.

## Closure ledger

| Decision | What v0.5.7 genuinely closes | Remaining disposition |
|---|---|---|
| D-107 | Per-generation seal wire; hosted requester form; incorporation-cap direction; stale “until cutoff” sentence | **Partial:** scalar ratify interaction, retro-effect posture, and seal scope/cardinality pins |
| D-108 | Removes fold-current head equality; carried snapshot deterministically wins over extra successors | **Partial:** `live_heads` vs scalar `accepted_through`; escaped-effect consequence; stale contradictory text |
| D-109 | Activation-based custody direction; 128-zone bound; current-key equality on `c.wrap_add` | **Partial:** deletion predicate is non-monotone across queued activation; equality is not global; held-zone set undefined |
| D-110 | `recipientset.v`, E7 key, E8 cap direction and the original 257-active-holder case | **Partial:** cap must apply per accepted epoch and every wrap-bearing path, not ambiguously “current epoch” |
| D-111 | Consumed-boundary promotion; feed-closure wire; per-generation page-update direction | **Mostly resolved:** live row/CDDL page mirrors, multi-cover fence rule, and T3 feed integration remain |
| D-112 | Provisional precedence; exact stage outcomes; full Fence tuple; per-rotation adoption direction; stale C3′ literal | **Partial:** stale E7 key, unbounded adoption list, unFenced wording and incomplete wrap dependency closure |
| D-113 | Live critical-section direction; PendingXfer dormancy; direct fork named; several mirrors fixed | **Partial:** pending-import crash race, export-id reuse, reject-permanent cleanup, and fork winner/order |

## What should be preserved

1. **The hosted availability design now has the right authority shape.** A
   requester-attested self-seal can close its own old generation without
   granting general hosted administration.
   ([hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:1310),
   [abandon CDDL](/Users/vm/owner-plane-d0a-spec.md:2815))
2. **Snapshot-wins is the right resolution of the old cross-log equality
   race.** Extra already-held successors no longer cause one replica to reject
   what another accepted.
   ([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1128))
3. **Consumed-boundary promotion is clean.** The total control order provides
   the value, so no duplicate wire field is needed to materialize immutable
   close/supersede state.
   ([promotion rule](/Users/vm/owner-plane-d0a-spec.md:1199))
4. **The direct checkpoint fixes are structurally right.** Per-generation
   update, omission-never-removal and coordinate-projected fences are the
   correct reducer direction.
   ([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1130))
5. **Recovery precedence is now honest.** Selection is pure/provisional and
   commits only through the final transition; malformed new-admin keys have
   an exact outcome.
   ([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1600))
6. **`recipientset`'s original schema failure is fixed.** It carries `v: 1`,
   is keyed by device, and the declared cap is in E8.
   ([E7/E8](/Users/vm/owner-plane-d0a-spec.md:88),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2864))

## Freeze-blocking findings

### 1. Per-generation immutable caps sit on a lineage-scalar ratify boundary

#### 1.1 The incorporation-cap rule literally bricks later-generation ratify

D-107 says a ratify cutoff exceeding an incorporated generation's terminal
Head is `body-invariant`. The only ratify boundary remains one
`zonecutoff.accepted_through`, and the normative comparator is lexicographic
`(gen, seq)`.
([incorporation cap](/Users/vm/owner-plane-d0a-spec.md:1172),
[comparator](/Users/vm/owner-plane-d0a-spec.md:1181),
[zonecutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2775))

Trace:

1. Generation 1 ends at H1.
2. W2 opens generation 2 with `last_known = H1`; H1 is now an immutable cap.
3. Generation 2 reaches H2; W3 opens generation 3 with `last_known =
   "unknown"`.
4. The owner attempts `c.cutoff(H2)` to clean the generation-2 gap.
5. H2 is lexically greater than H1, so the literal “ratify may never exceed an
   incorporation” rule rejects it—even though it does not need to reopen
   generation 1.

Per-generation abandon has the same problem: any scalar cutoff in a later
generation exceeds every earlier seal under the total comparator, despite the
promise that unnamed generations remain untouched.

Make ratification per generation, either in the wire or in the reducer. A
later scalar frontier can advance while the effective ratified prefix for
each older generation is clamped by its incorporation/abandon cap. The
`body-invariant` negative must mean “attempts to revive past the cap **within
that generation**,” not “names any later generation.” Pin that exact trace.

#### 1.2 `live_heads` and `accepted_through` are now two competing boundaries

D-108 says the carried multi-head `live_heads` set **is** the snapshot-wins
boundary. `c.cutoff` still carries one scalar `accepted_through`, which D-93
uses as the max-composed ratified-history boundary. No invariant relates the
two.
([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1128),
[cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2788))

For live heads `{g1:H1, g2:H2}`, a signed request can carry both but name
`accepted_through = H1`; another can name H2. “Carried heads stand” and
“ratified only through H1” yield different accepted sets from the same
operation. Define their composition explicitly—for example, carried heads can
supply per-generation snapshot caps while the scalar ratified prefix
intersects them. If intentional partial cleanup is not needed, the simpler
rule is `accepted_through == max(live_heads)` (and `"none"` iff empty); if it
is needed, encode the per-generation result rather than leaving two reducers.

### 2. New boundary operations can invalidate already escaped effects

D-107 says seals are outside the four revisit paths. D-108 intentionally lets
a delayed cutoff retire extra successors. Neither operation is constrained to
an open, non-effect-final gap.
([effect finality](/Users/vm/owner-plane-d0a-spec.md:409),
[abandon row](/Users/vm/owner-plane-d0a-spec.md:1129),
[four paths](/Users/vm/owner-plane-d0a-spec.md:661))

Two examples:

1. Solo generation-1 effect E is immediately effect-final and executes. A
   later `cabandon {gen: 1, at: "none"}` voids its entire generation and
   quarantines E.
2. The device signs cutoff C over H1, later writes and executes effect H2, and
   C is delivered afterward. Snapshot-wins deliberately retires H2.

The result may be an acceptable owner-authorized residual—revocation cannot
un-send bytes—but it contradicts “exactly four” revisit paths and the current
residual, which names only later compromise/fork proof invalidation. Choose
one:

- restrict gap seals/cutoffs to generations and heads that cannot already have
  released effects, with a portable predicate; or
- name immutable boundary application as another revisit class and extend the
  escaped-effect residual/product warning.

At minimum, `at = "none"` should be legal only under an explicit posture, and
`cabandon.seals` should be non-empty (`[+]`) unless no-op control ceremonies
are intentional.

There is a related cap-lifetime ambiguity: `w.gen` is charged and its proofs
can later lose standing. If the accepted `w.gen` that created an incorporation
cap is retro-quarantined, either the cap persists as an explicitly monotone
side effect outside the accepted-set fold, or it disappears and any effect
that relied on it joins the escaped-effect residual. State and vector that
choice.

### 3. Direct issuer-fork discovery still has no portable winner

D-113 now names direct conflicting-statement arrival, but says it exposes a
“losing branch” at the arriving statement's admission position. With no
committed boundary, neither branch is canonically winning and receipt arrival
is not a control-log order.
([T2](/Users/vm/owner-plane-d0a-spec.md:661),
[T3](/Users/vm/owner-plane-d0a-spec.md:674))

Replica A receives S then S′; replica B receives S′ then S. If “the arriving
statement loses,” A keeps qualifications based on S and B keeps those based on
S′. T3's scope freeze is deterministic only if **both suffixes at and after
the fork** stop qualifying until an owner boundary selects one, or if another
portable deterministic branch selector is defined. The boundary-revealed
case is already sound; direct discovery needs the conservative both-branches
rule and two-order vector.

### 4. Renewal custody and cardinality are not yet stable across queued epochs

#### 4.1 The predecessor-key deletion predicate is non-monotone

D-109 retains Kold until every held zone's **active** epoch serves a Knew wrap.
With active E2 and accepted/queued E3/E4:

1. renewal carries the required latest-accepted E4 Knew wrap;
2. post-renewal `c.wrap_add(E2, Knew)` makes the active-epoch predicate true;
3. Kold is destroyed;
4. E3 activates with only its pre-renewal Kold wrap and strands the renewed
   recipient.

([renewal row](/Users/vm/owner-plane-d0a-spec.md:1114),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2672))

The deletion condition must be monotone: retain Kold until the renewal-time
target epoch has activated, or until every accepted/unretired epoch that can
become active has a Knew wrap. “Interim epochs get wrap-adds” must be a
required precondition, not descriptive guidance.

#### 4.2 Current-key equality must cover every wrap-bearing operation

D-109 adds equality only to `c.wrap_add`. General `c.kek_rotate` wraps can
still name a superseded certificate key after renewal; the operation registry
also omits the new equality/cap rule from its `c.wrap_add` row.
([wrap-add row](/Users/vm/owner-plane-d0a-spec.md:1117),
[wrap-add CDDL](/Users/vm/owner-plane-d0a-spec.md:2728))

Pin one global invariant: every newly accepted KekWrap equals
`recipient_device`'s live certificate KEM key at that operation's control
position, with deliberate same-operation exceptions where genesis/new
enrollment/renewal carries the certificate and wraps together. Apply it to
enroll, zone-create, rotation and wrap-add.

#### 4.3 The 256 cap must be per accepted epoch, not “current epoch”

`c.wrap_add` is valid for older accepted, not-yet-retired epochs. If E2 is
queued and E3 is later accepted, an implementation reading “current epoch” as
latest accepted can stage E2 to 257 recipients; E2 must later Fence a
`recipientset` whose wire cap is 256.
([E8](/Users/vm/owner-plane-d0a-spec.md:127),
[rotation staging](/Users/vm/owner-plane-d0a-spec.md:887))

Enforce ≤256 for each `(zone, accepted epoch)` that may still Fence, across
every wrap-bearing operation. Also define the portable **held-zone set** used
by the 128 cap—effective wraps, grants, or their union—and make enrollment,
grant, zone-create, rotation and wrap-add consume the same definition.

### 5. Recovery adoption is exact per entry but not bounded or complete

#### 5.1 E7 rejects the new multi-rotation representation

The central key inventory still declares `adopted_rotations → zone_id`, while
D-112 and Appendix A key it by `(zone_id, rotation_op)`. A strict E7 decoder
therefore rejects the multiple same-zone entries that D-112 requires as
duplicate/non-canonical.
([E7](/Users/vm/owner-plane-d0a-spec.md:88),
[recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:2835))

This is a direct wire contradiction; update E7 before any bytes are pinned.

#### 5.2 A contiguous per-rotation adoption chain can exceed 64 KiB

Recovery must carry one full Fence tuple for every adopted rotation in the
post-base contiguous chain. The chain has no count cap or continuation, while
the complete control operation is capped at 64 KiB. An offline control branch
can perform hundreds of rotations before discovering a fork; a forever-
retention plane can then preserve its active store only by emitting an
unencodable recovery operation.
([control-op cap](/Users/vm/owner-plane-d0a-spec.md:104),
[adoption rule](/Users/vm/owner-plane-d0a-spec.md:1266))

Recovery cannot safely page adoption after precedence. Use a compact
authenticated activation-chain/root with local predecessor proofs, impose and
justify a bounded divergence window, or explicitly accept forced storage
orphaning beyond a bound.

#### 5.3 Dependency closure still omits effective wrap-add state

`control_frontier` locates cut-branch wrap-adds, enrollments and descriptors.
D-112 preserves certificates/descriptors as validation material and says
erase manifests stand, but never says that the effective/superseding
`c.wrap_add` map through each adopted frontier survives as adopted storage
state.

Base B → rotation R → wrap-add W admits device D → Fence commits a recipient
hash including D → recovery adopts R/F. C3′ retires the post-base control
branch; D-97 preserves R's adopted storage activation/epoch, but the spec does
not say whether W's effective/superseding wrap survives that cut. One reducer
loses D's wrap; another silently keeps W effective. Define the adopted closure
as the rotation plus its effective KEK-wrap map through `control_frontier`,
with certificates/descriptors retained only for validation and
manifest/tombstone erasure remaining authoritative.

As an exactness cleanup, clarify that only **activated/Fenced** rotations are
eligible for storage adoption; an accepted-but-unFenced rotation has no Fence
identity or storage activation and is simply cut/reissued.

### 6. Transfer remains unsafe across pending admission and revival

#### 6.1 A pending destination import can accept after XferAbort

The new critical section serializes live accepted commits, but it cannot span
an indefinite proof wait or process restart.
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:981))

1. Destination import A is durably committed but pending proof/effect finality.
2. The daemon crashes.
3. Source erasure occurs.
4. Nothing requires recovery to retain/count the pending attempt—terminal
   completion counts only accepted/effect-final imports—so a conforming
   implementation can write `XferAbort(missing = {A})`.
5. A's delayed proof arrives and A becomes accepted after the terminal.

Persist per-record in-flight attempt state and defer abort while an attempt
can revive, or add a portable terminal fence that prevents an already-durable
pending import from later admitting. A mutex alone is not crash-safe.

#### 6.2 Dormancy makes `export_id` reuse ambiguous

`export_id` is reserved only against another **accepted** release. When release
R1 is budget-displaced, it becomes dormant and no longer occupies the accepted
set; R2 can accept with the same ID. If R1 later revives, two PendingXfers
exist. PendingXfer/XferDone/XferAbort match only on `export_id`; replay identity
also has `(from_plane, source_op)` but none of these identities carries the
authorizing `release_op` needed to distinguish R1 from R2.
([export identity](/Users/vm/owner-plane-d0a-spec.md:1937),
[terminal CDDL](/Users/vm/owner-plane-d0a-spec.md:2894))

Make the first structurally valid committed release reserve the ID under a
portable collision rule, or include `release_op` in every journal, terminal
and replay identity. Add displaced → replacement → revival ordering vectors.

#### 6.3 Reject-permanent cleanup is not release effect-finality

Section 6.1 says a terminal inherits the release's effect-final coordinate,
but dormancy says a reject-permanent release writes XferAbort. A rejected
release can never be effect-final. Classify that abort as immutable-rejection
journal cleanup rather than a release-authorized effect, and state that any
post-final retro-quarantine stops further imports while prior escapes remain
the named residual.

### 7. D-111 mostly lands, but live mirrors still contradict it

1. The checkpoint row first says effective coverage is the latest page **per
   lineage**, then later says replacement is per `(lineage,generation)`.
   Appendix A repeats the former. Replace both live statements.
   ([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1130),
   [checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2557))
2. A checkpoint has one fence per lineage but may carry several generation
   covers for that lineage. Require the fence to be ≥ **every** effective/page
   cover for L under the `(gen,seq)` projection, not an unspecified singular
   “covers head.”
3. T3 describes compromise boundaries but never integrates renewal's
   `feed_closure`. State that it targets the predecessor
   `(device_id, signing key)` scope, uses the ancestry/minimum rules, rejects
   seq > through, and resolves `key_id` through the predecessor cert.

These are exactness/mirror repairs, not a redesign of D-111.

## Normative drift and small schema pins

1. Family 9 and D-87 still say cutoff assent “dies on a later write—never
   durable rollback authority,” directly contradicting D-108 snapshot-wins.
   ([vectors](/Users/vm/owner-plane-d0a-spec.md:2197),
   [D-87](/Users/vm/owner-plane-d0a-spec.md:2406))
2. The `c.wrap_add` registry row omits D-109 current-key equality and the
   relevant D-110/D-109 cardinality checks.
3. `cabandon.seals` is `[*]`, so an empty no-op ceremony is valid despite the
   operation being described as a seal; make it `[+]` or explicitly permit
   no-op.
4. The older D-88 checkpoint ledger wording still says latest page per
   lineage; D-111 supersedes it, but the live row/CDDL must be authoritative.

## Gate A and artifact status

The companion `d0a-vector-cases.v1.json`, independent core, corpus and harness
are still absent; family 14 remains open. The Open list is honest about D-91,
and the Gate-A checklist still requires all of them.
([Open list](/Users/vm/owner-plane-d0a-spec.md:2434),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:2444))

V0.5.8 should therefore repair prose/CDDL first. Then author the companion as
the first corpus artifact, implement the independent core/harness, generate
fixtures, run every surface and perform the final discrepancy audit. No
fixture should decide the open semantics above.

## Required v0.5.8 vector additions

Reserve exact cases for:

1. incorporation cap in generation 1 followed by legal ratification in
   generation 2, same-generation revival past the cap (reject), and later
   retro-quarantine of the cap-creating `w.gen`;
2. multi-head `live_heads` with mismatched scalar `accepted_through`;
3. effect E → later `at:none` seal, and delayed snapshot cutoff after an
   executed successor;
4. direct issuer fork delivered in both branch orders with neither branch
   qualifying until selection;
5. active E2 + queued E3/E4 renewal, including deletion after E2 coverage and
   E3 activation; stale-key wraps through rotation; recipient cap on an older
   queued epoch; and the 129th held-zone path through every carrier;
6. recovery with multiple same-zone entries under E7, a chain exceeding the
   control cap, adopted wrap-add dependency, and accepted-but-unFenced
   rotation;
7. durable pending import → crash → source erasure → delayed proof; dormant
   release → same export_id replacement → revival; reject-permanent cleanup;
8. multi-generation checkpoint page/fence semantics and renewal closure in
   T3; and
9. every stale live-row/CDDL/decision-record mirror above.

## Final recommendation

Treat v0.5.7 as a successful near-freeze cut, not as the freeze itself. It
closes the v0.5.6 review at the level of design intent, but several new wire
and state transitions do not yet implement that intent under their declared
bounds. A tight v0.5.8 should be enough if it focuses on per-generation ratify
semantics, monotone renewal custody, bounded recovery adoption and durable
transfer attempt identity. After that, the companion/core/corpus work is the
right next move.
