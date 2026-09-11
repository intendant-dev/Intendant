# Synthesized review: D0-A Core + Memory normative specification v0.5.8

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.8-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.8-review.md)
and
[owner-plane-d0a-spec-v0.5.8-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.8-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.8,
SHA-256 `b2a2859d7d888d1ccc0b2aa1f52d1538da455ec8dd2c99b8cc40913f4d1ab262`.
This synthesis resolves disagreements rather than unioning the two reports.*

## Executive verdict

**Both reviews correctly call for v0.5.9 and agree that Gate A has not passed.
The peer contributes one important new high finding and one useful inventory
pin. Its broader conclusion—that all six prior areas are discharged, no owner
decisions remain, and three line edits plus one clause are enough to freeze—is
not supported by the composed reducer.**

V0.5.8 is a strong near-freeze revision. Preserve its real gains:

- ratification and immutable caps are described per generation;
- direct issuer-fork discovery freezes both suffixes independent of delivery
  order;
- checkpoint coverage is now per `(lineage, generation)`, omission-never-
  removal, with one lineage fence dominating all covers;
- recovery adoption has the correct key, a hard bound, complete Fence
  identities, dependency retention, effective-wrap preservation, and an
  explicit unFenced-cut posture;
- global wrap validation and the intended custody/cardinality predicates are
  present in every major layer;
- durable pending imports block premature transfer termination; and
- all transfer journal records carry `release_op`.

Adopt from the peer:

1. **H1 is valid:** per-generation ratify entries collide with the unchanged
   `(zone_id, lineage)` logical key.
2. **P1 is valid but under-inclusive:** the supposedly derived revisit
   inventory under-names the retirement algebra.
3. The recovery/checkpoint improvements are accurately credited.
4. The companion-first artifact sequence is right.

Reject from the peer:

- “all six areas discharged”;
- D-116 and D-119 marked complete;
- the claim that every demanded arrival/crash trace now closes;
- “no owner decisions left”; and
- freezing the prose after H1/P1 alone.

Recommended disposition:

- **Direction:** accept and preserve.
- **V0.5.9:** required; substantive but focused.
- **Protocol/schema freeze:** no.
- **Candidate baseline for artifact audit:** after v0.5.9 closes the semantic
  choices below.
- **Gate A:** no; only after companion, core/harness, corpus, surfaces, family
  14, and discrepancy audit are green.
- **Durable P1 writes:** remain prohibited under the existing later gates.

## Assessment of the peer review

### What the peer did well

The peer review is careful, constructive, and unusually accountable about its
own earlier mistakes. It verifies that requested fields and mirrors exist,
checks the revision against the archive, and correctly recognizes that
v0.5.8's strongest move is replacing the fragile numbered revisit list with a
derivational intent. Its H1 was missed by the first review and should be
adopted. Its P1 also catches a real contradiction between “Nothing else
revisits” and the larger cutoff table.

It also correctly avoids reintroducing a fold-current lower bound for
destructive seals and accurately credits D-117, the checkpoint half of D-118,
and the newly carried `release_op` fields.

### Where its method overcredits the text

The discharge audit often treats the presence of a ruling or phrase as proof
that a reducer exists:

- “the snapshot is the outer bound” is accepted without composing it with a
  previously larger ratify maximum;
- “boundary facts are monotone” is accepted without comparing an
  accepted-then-displaced fold with a fresh fold where the operation was never
  accepted;
- “accepted, unretired epoch” is accepted without identifying whether
  `unretired` is portable control state or local Fence progress;
- “acceptance-consumed export_id” is accepted without an order-independent
  claimant rule;
- “a committed boundary selects the winner” is accepted without checking two
  different boundary types selecting opposite branches; and
- “durable pending attempts defer” is accepted without evaluating the record
  after that durable attempt resolves permanently negative.

This verifies editorial incorporation, not fold closure. The necessary audit
for every durable fact is:

1. fix one immutable final byte set;
2. fold it in both delivery orders;
3. rebuild it from scratch;
4. enumerate every durable reducer fact and terminal; and
5. require the same result in all three runs.

The counterexamples below use exactly that standard.

## Adjudicated decision ledger

| Decision area | Peer disposition | Synthesized disposition |
|---|---|---|
| D-107 / D-108 | Complete except H1/P1 | **Partial:** per-generation direction landed; encoding, sequential snapshot composition, `none`, requester variants, and seal lifecycle remain |
| D-109 / D-110 / D-116 | Complete | **Open/partial:** `unretired` is local/undefined for admission; renewal zone set and effective-certificate equality conflict |
| D-111 | Complete | **Mostly complete:** promotion and checkpoint paging landed; proof-boundary integration remains |
| D-112 / D-117 | Complete | **Materially complete in prose/CDDL; vectors pending** |
| D-113 / D-119 | Complete | **Partial:** `release_op` and pending-deferral intent landed; portable ID reservation and terminal cleanup remain |
| D-114 | Complete except H1/P1 | **Partial:** per-generation intent and residual landed; effective state equation and portable cap materialization remain |
| D-115 | Complete | **Partial:** freeze-both is correct; cross-type winner consistency remains |
| D-118 | Complete | **Partial:** checkpoint portion complete; feed-closure revisit and cross-type ancestry remain |
| D-91 / Gate A | Artifact-pending | **Artifact-pending; agreed** |

## 1. Adopt peer H1, but not its three-line fix

### The finding is correct

E7 keys zone/history/closure cutoffs by `(zone_id, lineage)`
([E7](/Users/vm/owner-plane-d0a-spec.md:82)). D-114 now defines ratify state
per `(zone, lineage, gen)` and says each requester cutoff entry is a
per-generation ratification
([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1158),
[algebra](/Users/vm/owner-plane-d0a-spec.md:1165)). The same row and Appendix
A still promise one operation can retire several unknown-gap heads
([CDDL comment](/Users/vm/owner-plane-d0a-spec.md:2883)).

A lineage with live generations `g1` and `g3` cannot carry ratify entries for
both in one `c.cutoff`: strict E7 decoding sees two identical logical keys and
rejects the set. This is a genuine wire/algebra collision.

### The proposed repair is incomplete

Changing three key annotations to `(zone_id, lineage, gen)` is necessary only
if the body has a generation in every case. It does not:
`accepted_through = "none"` carries no generation
([zonecutoff](/Users/vm/owner-plane-d0a-spec.md:2858)). The same `zonecutoff`
shape is also reused by supersede, revoke, close, and recover boundaries whose
semantics may intentionally remain lineage-scalar.

The clean repair is likely a purpose-specific shape:

```text
ratifycutoff = {
  zone_id,
  lineage,
  gen,
  accepted_through: head / "none"
}
```

with logical key `(zone_id, lineage, gen)`, equality
`head.gen == gen` when a Head is present, and `c.cutoff` using this shape.
Retain or separately define the scalar `zonecutoff` for non-ratify purposes.
If the product instead chooses one generation per ceremony, remove the D-80
multi-head promise explicitly. Either way, this is a schema decision, not
three annotations.

This fix must also state that requesterless trusted cutoffs do not perform the
requester-snapshot validation, and requester-bearing cutoffs either target one
lineage or carry per-lineage freshness versions. The current signature binds
one derived `lineage_version` while trusted syntax permits multiple lineages.

## 2. Adopt and broaden peer P1

T2 says the revisit inventory is exact and then names retirement boundaries
as “per-generation seals, snapshot cutoffs”
([T2](/Users/vm/owner-plane-d0a-spec.md:667)). The cutoff algebra also contains
supersede, revoke, close, and recover boundaries
([algebra](/Users/vm/owner-plane-d0a-spec.md:1180)). Each can cause a previously
admitted suffix to become quarantined. The peer is correct: the parenthetical
cannot read as exhaustive.

But adding “e.g.” alone is too weak for a normative incremental reducer. The
derived inventory must cover:

- issuer-fork **discovery** freezing both suffixes and later resolution
  selecting one;
- proof compromise cutoffs;
- renewal `feed_closure` truncating an old issuer feed;
- ratify snapshot introduction/shrink and later growth/revival;
- abandon/seal and incorporation caps;
- supersede, revoke, and close boundaries; and
- recover branch cuts.

The best formulation derives re-evaluation from changes to the effective
proof, budget, and boundary functions, then gives the list as exhaustive
instances. Otherwise “Nothing else revisits” remains a trap for incremental
implementations.

## 3. Boundary state still lacks its composition equation

H1 fixes how several per-generation entries are encoded; it does not say how
successive boundaries compose.

Replay:

1. `C1` ratifies generation `g` through `H3`.
2. `C2` carries requester snapshot head `H2` and cutoff entry `H1`.
3. `H1 <= H2`, so C2 passes the new per-operation invariant.
4. Ratify-max remains `H3` under the cutoff algebra.
5. Snapshot-wins retires uncarried `H3`.

`H3` is simultaneously admitted by ratify-max and retired by the snapshot.
Repeating an H3 ratification is not “growth,” so the stated revival rule does
not identify a later event that can restore it.

V0.5.9 needs one effective per-generation state equation, for example separate
`ratify_max`, revivable requester `snapshot_cap`, and immutable incorporation/
seal cap. It must define introduction, shrink, growth, revival, and precedence.

Two associated pins remain:

- repeated seals need an explicit effective intersection/minimum per
  generation, with `"none"` strongest; and
- §10.5 makes snapshot retirement anywhere “under a sealed generation”
  permanent, even when the retired operation lies below the seal and ratify
  growth up to the seal is otherwise legal
  ([dispositions](/Users/vm/owner-plane-d0a-spec.md:1719)). Permanence should
  attach to the effective immutable bound, not the mere existence of a seal.

The hosted ceiling also calls its exception a narrow “self-gap seal,” while
the live operation legally permits own-lineage below-history truncation and
whole-generation void
([abandon row](/Users/vm/owner-plane-d0a-spec.md:1159),
[hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:1376)). Either constrain the
wire to gaps or name the broader authority honestly.

## 4. “Once accepted” cannot materialize portable state

### Incorporation caps

Tenant budget acceptance is canonical and revisable: a late earlier consumer
can displace a previously accepted operation
([budget fold](/Users/vm/owner-plane-d0a-spec.md:395)). D-114 nevertheless says
an incorporation cap persists once its creating `w.gen` was accepted, even if
that operation is later displaced
([cap lifetime](/Users/vm/owner-plane-d0a-spec.md:1208)).

Replica A accepts W and creates the cap, then a late generation-2 consumer
displaces W; A retains the cap. Replica B sees the consumer first, so W never
enters its accepted set and no rule creates the cap. A fresh fold matches B.
The same later ratify cutoff can therefore reject only on A.

“Monotone after acceptance” does not repair the bug; it makes the divergence
permanent. Materialize the boundary in portable control bytes, derive it from
an order-independent pre-budget qualification class, or derive it from the
final canonical set.

### `export_id`

The same defect appears in the plane-wide `export_id` rule: an ID is consumed
if a release was **ever accepted**
([export rule](/Users/vm/owner-plane-d0a-spec.md:1985)). A accepts `R1(X)` then
displaces it; B sees the displacer first and never accepts R1. A later `R2(X)`
rejects only on A.

`release_op` correctly distinguishes local journal attempts, but it cannot
turn arrival history into a portable claimant—especially for egress releases,
which write no PendingXfer. Choose a deterministic byte-derived claimant or
collision freeze, or make `(export_id, release_op)` the identity and remove
plane-wide single use.

These are protocol choices. The peer's statement that no owner decisions
remain is therefore false.

## 5. Issuer boundaries can still select two winners

D-115's freeze-both rule is correct. The missing part is one winner registry
across all boundary types.

T3 specifies ancestry within repeated cutoffs and between renewal
`feed_closure` and receipt cutoffs. The checkpoint row separately constrains
successive checkpoint proof positions. It never requires checkpoint,
compromise-cutoff, and renewal-closure commitments for the same issuer scope to
be mutually ancestor-compatible
([T3](/Users/vm/owner-plane-d0a-spec.md:699),
[checkpoint](/Users/vm/owner-plane-d0a-spec.md:1160)).

A checkpoint can commit fork branch A10; a later renewal closure or first
compromise cutoff can commit B10. Both pass their type-local rules while the
protocol says a committed boundary selects *the* winner.

Create one per-issuer-scope commitment registry across all three carriers.
Every new boundary must be ancestor-compatible with existing effective
commitments or produce a named conflict outcome. A boundary below the fork
does not select either suffix.

Renewal closure also remains an omitted revisit trigger. If receipt 8 already
qualified X and a later renewal closes the old feed through 7, receipt 8 must
die and X must retro-quarantine—or the closure does not close the feed. Add
closure acceptance to proof retro-disqualification and the escaped-effect
residual
([renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2756)).

## 6. D-116 is not discharged

### Portable admission reads a local retirement predicate

The recipient and held-zone caps use `(accepted, unretired epoch)`
([E8](/Users/vm/owner-plane-d0a-spec.md:102),
[held zones](/Users/vm/owner-plane-d0a-spec.md:458)). Epoch activation and
retirement are local Fence/KekDestroyed state
([I3](/Users/vm/owner-plane-d0a-spec.md:818)). `c.wrap_add` simultaneously says
control admission never reads local Fence progress and locally retired wraps
are inert
([wrap-add row](/Users/vm/owner-plane-d0a-spec.md:1147)).

With E2 at 256 recipients, replica A may have Fenced E3/destroyed E2 while B
has not. The same 257th E2 wrap is inert and apparently admissible on A but
cap-violating on B. The same distinction can change whether a zone is the
129th held zone.

Split the domains:

- portable, control-derived recipient and held-zone cap sets; and
- local active/queued/not-destroyed epochs for Kold custody.

### The renewal set contradicts itself

The central definition says renewal wraps the effective-wrap subset of
`held_zones`; registry/CDDL say one wrap per held zone, including grant-only
zones; history coverage names only zones already holding wraps
([definition](/Users/vm/owner-plane-d0a-spec.md:458),
[renewal](/Users/vm/owner-plane-d0a-spec.md:1144)). A grant-only zone therefore
either gains new cryptographic access during renewal or makes the renewal
invalid. Wildcard grants add another undefined expansion path.

Use the renewal-time effective-wrap-zone set for renewal and history coverage;
define the cardinality set separately.

### “An enrolled certificate” is not the effective current key

The global rule accepts a wrap for **an** enrolled certificate's KEM key
([KekWrap](/Users/vm/owner-plane-d0a-spec.md:2713)). A superseded predecessor
remains validation history. After Kold deletion, a later wrap can therefore
target that predecessor key and supersede the usable Knew wrap. Require the
unique effective, unsuperseded certificate at the control position, retaining
the same-operation enrollment exception.

The `recipientset` CDDL's stale current-epoch/enroll-or-wrap-add wording must be
aligned after the portable cap set is chosen
([recipientset](/Users/vm/owner-plane-d0a-spec.md:2960)).

## 7. D-119 is not discharged

The peer correctly credits the new durable-attempt deferral and `release_op`
fields. Two terminal gaps remain.

First, source-erasure recovery defines `missing` as records with **no durable
destination attempt**, then says a durable pending attempt waits until it
becomes permanently rejected and may then be listed
([terminal algorithm](/Users/vm/owner-plane-d0a-spec.md:1031)). The durable
attempt still exists after rejection, so a one-record transfer has an empty
literal set and cannot write the required non-empty XferAbort. Say **no
unresolved durable attempt**: pending and accepted-but-not-effect-final remain
unresolved; permanent rejection becomes eligible missing.

Second, the finality exception is justified for cleanup after the **release**
itself becomes permanently rejected
([cleanup exception](/Users/vm/owner-plane-d0a-spec.md:1017)). Case 3 uses the
same `reason = "reject-permanent"` for a rejected **destination import** while
the release may remain accepted and subject to its normal finality barrier.
Separate release-rejection journal cleanup from destination-rejection transfer
termination and state the latter's gate.

The prose shorthands should also show `release_op`, matching Appendix A.

## Consolidated v0.5.9 change set

### 1. Split and encode boundary purposes exactly

- Add a ratify-specific per-generation cutoff shape/key or explicitly abandon
  one-operation multi-head ratification.
- Preserve/split scalar non-ratify boundary shapes deliberately.
- Define `"none"` with an explicit generation for ratify.
- Pin requesterless semantics and requester-lineage/version cardinality.

### 2. Publish the effective boundary reducer

- Define ratify maximum, requester snapshot cap, incorporation cap, and
  repeated-seal intersection per generation.
- Define snapshot shrink/growth/revival and permanent-vs-revivable state.
- Reconcile hosted gap-seal naming with actual truncation authority.

### 3. Remove acceptance-history consensus facts

- Give `w.gen` cap materialization an order-independent portable trigger.
- Give `export_id` collision/reservation a deterministic portable rule.

### 4. Unify proof-boundary commitments and revisits

- One ancestry-compatible registry per issuer scope across checkpoint,
  compromise cutoff, and renewal closure.
- Derive every proof/budget/boundary re-evaluation, including fork discovery,
  renewal closure, supersede/revoke/close/recover, and revival events.
- Name conflict outcomes and escaped-effect residuals.

### 5. Separate portable membership from local custody

- Control-derived recipient and held-zone cap sets.
- Local activation/custody epoch set.
- Renewal over exactly pre-existing effective-wrap zones.
- Effective-current-certificate KEM equality.
- Repair the recipientset mirror and wildcard posture.

### 6. Totalize transfer terminals

- “No unresolved durable attempt.”
- Separate release rejection from destination rejection.
- Align all `release_op` mirrors.

## Gate-A sequence

Both reviews agree on the execution order after semantic closure:

1. author `d0a-vector-cases.v1.json` first;
2. implement the independent owner-plane core/harness;
3. generate the corpus, beginning with the disputed traces above;
4. record family 14;
5. run every required surface; and
6. perform the prose↔CDDL↔companion↔vector discrepancy audit.

The companion and other artifacts remain absent, and the specification itself
marks D-91 artifact-pending
([open record](/Users/vm/owner-plane-d0a-spec.md:2506)). A repaired v0.5.9 may
be the **candidate audit baseline**. It is not frozen and Gate A is not true
until the artifacts and audit are green.

## Final recommendation

**Cut a substantive but focused v0.5.9, then review its semantic traces before
freezing the artifact baseline.** Adopt the peer's H1, broaden P1, and preserve
its accurate credit for recovery/checkpoint work. Do not accept the peer's full
discharge ledger or minimal-fix estimate. The remaining decisions are small in
architectural scope but load-bearing for replica convergence, key custody, and
transfer finality; encoding them into the companion before they are resolved
would merely move specification invention into the fixtures.
