# Review: D0-A Core + Memory normative specification v0.5.9

*2026-07-12. Independent freeze review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md), v0.5.9,
SHA-256 `560728bd6f43d1ab30bd568b0af5f4640cb215ef5e5325d7bba0a30ba932abeb`.
The review replayed the v0.5.8 synthesis against the normative prose, CDDL,
decision ledger, and required vector matrix. The archived v0.5.8 source was
used for a direct revision audit; v0.5.9 is 347 insertions and 142 deletions
from that baseline.*

## Executive verdict

**V0.5.9 makes several good decisions, but it is not freeze-ready. Cut a
focused v0.5.10 before freezing the prose or authoring fixtures.**

The strongest changes should be preserved:

- D-120 cleanly separates per-generation `ratifycutoff` from scalar,
  authority-ending `zonecutoff`;
- D-121 finally names the three kinds of boundary state and correctly places
  permanence at the effective immutable bound rather than at bare seal
  existence;
- D-123 correctly makes `release_op`, not an arrival-dependent reservation of
  `export_id`, the transfer-workflow identity;
- D-124 correctly recognizes that checkpoint positions, compromise cutoffs,
  and renewal closures need one issuer-scope commitment registry, and that a
  renewal closure must retro-disqualify the closed suffix;
- D-125 correctly removes local Fence progress from portable admission,
  excludes wildcard grants from finite held-zone counting, and anchors new
  wraps to the effective certificate; and
- D-126 correctly distinguishes release-rejection journal cleanup from a
  destination-rejection transfer terminal.

D-117's recovery-adoption shape and D-118's per-generation checkpoint
coverage are also materially coherent in prose and CDDL. These are real
advances, not merely editorial incorporation.

The remaining faults are compositional. Two are construction/authority
blockers:

1. adding `release_op` to the bytes hashed by the release's own
   `content_digest` creates an infeasible cryptographic fixed point; and
2. D-122 lets any signature-valid `w.gen` byte create a permanent history cap
   without the normal lineage, grant, revocation, generation, or body
   invariants.

The other major gaps concern deterministic replay: the D-121 equation has no
initial state and ignores its explicit ratification value in requester form;
referenced-Head mismatch and D-124 commitment ancestry can still depend on
arrival order; recovery does not enumerate removal/revival effects; KEM
renewal can re-add an intentionally excluded device; logical import
idempotency has no byte-distinct collision rule; and D-126 does not terminate
permanently quarantined transfers.

Recommended disposition:

- **Direction:** accept and preserve.
- **V0.5.9 as an implementation sketch:** no, because a release cannot be
  constructed as written.
- **Protocol/schema freeze:** no.
- **V0.5.10:** required; focused, but substantive.
- **Gate A:** no, both semantically and because the normative companion
  artifact is still absent.
- **Durable P1 writes:** remain prohibited under the existing later gates.

## Decision discharge ledger

| Decision | Assessment | Required disposition |
|---|---|---|
| D-117 | Prose/CDDL substantially coherent | Preserve; add the unFenced and deep-chain vectors |
| D-118 | Prose/CDDL substantially coherent | Preserve; expand multi-generation omission/fence vectors |
| D-120 | **Mostly complete** | Preserve the split; pin requester snapshots and zero-history representation |
| D-121 | **Open** | Define initial/top state, make the explicit entry effective, and define per-generation-to-scalar promotion |
| D-122 | **Unsafe as written** | Replace “signature-valid held byte” with a bounded, portable cap-eligibility predicate or a control-carried boundary |
| D-123 | **Direction right, construction broken** | Remove the hash cycle; define logical-import collision and exact effect IDs |
| D-124 | **Partial** | Add missing-evidence pendency, pending-scope reservation, recovery removal, key-reuse, and fork-point rules |
| D-125 | **Partial** | Split historical cap state, current membership, and old-key custody tuples |
| D-126 | **Partial** | Base cleanup and `missing` eligibility on permanent non-revivability, not only `reject-permanent` |
| D-91 / Gate A | **Artifact-pending** | Author `d0a-vector-cases.v1.json`, then corpus, harness, and discrepancy audit |

## 1. Blocker: the release bundle has a hash cycle

The release's `content_digest` is the hash of the canonical bundle
([§11.8](/Users/vm/owner-plane-d0a-spec.md:2062)). V0.5.9 adds the authorizing
release's `op_hash` as `bundle.release_op`
([bundle CDDL](/Users/vm/owner-plane-d0a-spec.md:3212)). But the release body
itself carries `content_digest`
([release CDDL](/Users/vm/owner-plane-d0a-spec.md:3245)), and `op_hash` hashes
the entire signed operation, including that body
([O2](/Users/vm/owner-plane-d0a-spec.md:521)). Construction therefore requires
solving a fixed point of the form:

```text
R = H_op(Sign(header(body_hash = H_body({
      content_digest = H_bundle({ release_op: R, ... })
    }))), body))
```

That is not a circularity an encoder can resolve by ordering its work; it is
an infeasible hash/signature fixed point. No conforming release can be minted.

The narrow repair is:

1. remove `release_op` from the bundle's hashed preimage;
2. hash a bundle such as `{ v, export_id, recs }`;
3. transport the verified signed release alongside the bundle;
4. derive `release_op = H_op(release)` after the release exists; and
5. continue to bind imports and local journal records to that derived hash.

`PendingXfer`, `XferDone`, and `XferAbort` may carry `release_op`; they are not
inside the signed release's digest preimage. Add a family-11 vector that
constructs a release from source records, re-derives the bundle, verifies the
digest, and independently re-derives `release_op`. A schema-only fixture that
starts with both values already supplied would miss this defect.

## 2. Blocker: held-byte incorporation caps bypass authority

D-122 creates a permanent cap for every held `w.gen(last_known = H)` whose
**header signature** verifies under its named certificate
([incorporation cap](/Users/vm/owner-plane-d0a-spec.md:1267)). Normal `w.gen`
admission is much stronger: it retains grant, tenant, zone, lineage,
generation-window, and budget checks
([lineage rules](/Users/vm/owner-plane-d0a-spec.md:1604)); the registry also
requires the implicit op-authoring right
([verb classes](/Users/vm/owner-plane-d0a-spec.md:1864)). Its `last_known`
must resolve to an accepted terminal Head in the same zone and lineage, and
the generation arithmetic must be exact
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1621)).

D-122 drops all of those predicates. As written, a byte can confer the cap
even when its signer:

- has no op-authoring grant or only read authority;
- names a lineage the certificate's device does not own;
- is beyond a revoke, supersede, capability-epoch, or recovery boundary;
- names a nonterminal, wrong-zone, wrong-lineage, or otherwise invalid Head;
- exceeds the generation window or is not the next generation; or
- fails body-hash/body-shape validation while retaining a valid header
  signature.

Concrete attack:

1. a revoked or read-only certificate key signs a canonical-looking `w.gen`
   naming victim lineage `L` and `last_known = H1`;
2. normal tenant admission rejects it;
3. every replica that holds the byte nevertheless materializes an immutable
   cap at `H1` under D-122; and
4. later valid history `H2...` in `L` becomes permanently unratifiable.

Calling this “restriction-only” does not make it harmless. It grants durable
destructive authority. It also composes ambiguously with effect finality,
which names `last_known` incorporation as an immutable closure
([effect finality](/Users/vm/owner-plane-d0a-spec.md:417)). If an inadmissible
held byte is not intended to confer finality, that distinction must be
explicit; if it does confer finality, the defect is more serious than denial
of history.

The v0.5.8 problem was real: budget acceptance is revisable, so “once
accepted on this replica” was not portable. The replacement should be a
canonical **cap-eligibility** class, not signature validity alone. It can be
independent of revisable proof and budget while still requiring, at minimum:

- strict parse, canonicality, operation/body version, and body-hash validity;
- a valid signature and a certificate installed on the surviving branch;
- position-relative certificate, grant, revocation, capability-epoch, and
  lineage ownership;
- exact `w.gen` zone/lineage/generation/chain invariants; and
- exact `last_known` Head identity in the same zone and lineage.

Alternatively, make incorporation an explicit control-authorized operation.
Whichever posture is chosen must also state:

- how C3′ removes caps whose authority lived only on a cut branch;
- whether a cap can confer effect finality before its `w.gen` admits;
- which held/rejected bytes are durable reducer input; and
- how checkpoints, compaction, sync, and GC preserve or retire that input.

The vector must include unauthorized, wrong-lineage, revoked, superseded,
cut-branch, unresolved-certificate, invalid-body, and valid-but-budget-
displaced cases. The desired order independence is achievable without giving
every historical signing key a permanent truncation primitive.

## 3. D-121 is not yet a complete state equation

### 3.1 The empty state is undefined

The equation defines `ratified_through` as `"none"` when no entry exists, an
`admit_bound` fold with no initializer, and `immutable_cap` as a minimum over
possibly no seals or caps
([equation](/Users/vm/owner-plane-d0a-spec.md:1304)). Effective admission is
then `p <= min(admit_bound, immutable_cap)`.

There must be an explicit top/unbounded identity for absent revivable and
immutable boundaries. Otherwise either:

- a normal generation with no cutoff has no `admit_bound`; or
- absence is collapsed into wire `"none"`, which is an explicit zero-history
  decision and would reject every position.

Use distinct algebraic states such as `Absent/Top`, `Zero/"none"`, and
`Head(H)`. Do not reuse the wire sentinel as the empty reducer state.

### 3.2 Requester snapshots ignore the explicit ratification value

The requester rule permits `accepted_through <= carried_head` and calls the
entry the generation's ratification while calling the snapshot the outer
bound
([`c.cutoff` row](/Users/vm/owner-plane-d0a-spec.md:1209)). But D-121 first
max-composes the entry into `admit_bound` and then overrides `admit_bound` to
the carried head. `ratified_through` is not read by the admission formula.

Trace:

1. the requester carries live Head `H5` for generation `g`;
2. its `ratifycutoff` explicitly says
   `{ gen: g, accepted_through: "none" }`;
3. `"none" <= H5`, so the operation passes the stated composition check; and
4. snapshot override sets `admit_bound = H5`, admitting the entire prefix
   despite the explicit zero-history ratification.

The same problem occurs less visibly with an entry at `H1` and a carried Head
at `H5`: the entry has no effect on admission.

Choose and state one semantics:

- require the entry to equal the carried Head, with a special zero-history
  representation for a generation that has no carried Head; or
- retain `<=`, make the entry the post-operation admission boundary, and use
  the carried Head only as the signed freshness/outer cap; or
- define a third composition that reads both values explicitly.

Any choice is implementable. The current text is not. Also say whether
`ratified_through` is merely audit/coverage state or participates in
admission; a named but unused state variable is especially risky in a
normative reducer.

### 3.3 Per-generation ratification has no scalar-promotion rule

D-120 deliberately keeps supersede, revoke, close, and recover boundaries on
scalar `zonecutoff`
([cutoff algebra](/Users/vm/owner-plane-d0a-spec.md:1220),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3031)). Yet renewal and strict epoch
advance may consume staged, per-generation ratify entries and “materialize
that boundary's value” as immutable scalar supersede/close state without
carrying another value
([promotion](/Users/vm/owner-plane-d0a-spec.md:1357)).

There is no unique “that value” for a lineage with, for example,
`g1 -> H10`, `g2 -> "none"`, and `g3 -> H5`. Define an exact no-widening
reduction, or require the consuming operation to carry the scalar boundary it
is creating. The second option is easier to audit. In either case, vector the
multi-generation consumption; the generic “consumed-boundary promotion” item
is not enough.

## 4. Referenced-Head mismatch remains arrival-dependent

An exact Head not yet held is pending, but the presence of a different hash
at the same coordinate permanently rejects the boundary
([Head lifecycle](/Users/vm/owner-plane-d0a-spec.md:1336)). That still permits
two replicas with the same final bytes to disagree:

1. replica A receives `H_A`, then a cutoff naming `H_A`; the cutoff accepts;
2. replica B first receives conflicting `H_B` at the same `(gen, seq)`, then
   the cutoff naming `H_A`; the cutoff rejects permanently;
3. both later hold `H_A`, `H_B`, and the cutoff; and
4. no rule revisits A's accepted cutoff or revives B's permanent rejection.

The final held set is identical, but incremental state differs; a fresh fold
is not specified.

Reference resolution must be a set property. Decide whether an owner boundary
naming the exact op hash selects that tenant branch, whether any coordinate
fork freezes the reference, or whether both branches make the boundary
invalid. Then derive the same result in both arrival orders and on fresh
replay. A conflicting coordinate cannot, by its mere earlier arrival, create
an irreversible outcome. This rule applies to ratify entries, scalar cutoffs,
abandon seals, checkpoint references, and recovery cutoffs.

## 5. D-124's commitment registry needs pending and removal semantics

### 5.1 Missing ancestry evidence must pend

D-124 requires each new commitment to ancestor-verify against every accepted
commitment and assigns `body-invariant` otherwise
([T3](/Users/vm/owner-plane-d0a-spec.md:740)). It does not distinguish a
proven opposite branch from a missing chain path.

Trace:

1. the registry holds issuer commitment `A5`;
2. R1 holds statements `A6...A10`, so a new carrier committing `A10` verifies
   and accepts;
3. R2 receives the same carrier before those statements; and
4. a literal implementation gives R2 permanent `body-invariant`.

After the statements arrive, the replicas differ. Missing ancestry material
must be `issuer-gap` or `ref-unresolved` with pending-dependency; only a fully
proved incompatible branch may reject permanently.

### 5.2 An earlier pending carrier must reserve its scope

The text says the first commitment in **control order** above the fork selects
the branch. Later control operations are nevertheless allowed to pass an
earlier pending reference, and `c.revoke_device` is itself a pending compound
([operation row](/Users/vm/owner-plane-d0a-spec.md:1196)). Therefore:

1. earlier carrier C1 commits branch B but pends on a missing path or compound
   dependency;
2. later carrier C2 commits branch A and accepts because C1 is not in the
   effective registry; and
3. C1's dependency arrives.

If control order governs, C1 should now select B and C2 should become
incompatible. No reservation, retro-disqualification, or replay rule says so.
Choose one:

- an unresolved earlier commitment reserves its issuer scope and later
  commitments in that scope pend behind it; or
- selection is by a separately defined effective-acceptance order rather than
  control order.

The first is simpler and matches the current prose. Vector an earlier pending
carrier followed by a conflicting later carrier, in both dependency-delivery
orders.

### 5.3 Registry removal and scope identity are incomplete

C3′ removes cut-branch control operations
([recovery](/Users/vm/owner-plane-d0a-spec.md:1398)). If it removes the
checkpoint/cutoff/closure that selected an issuer branch, both suffixes must
freeze again until a surviving commitment selects one. This removal event is
missing from the supposedly exhaustive revisit inventory
([T2](/Users/vm/owner-plane-d0a-spec.md:675)). The same applies when recovery
removes a compromise cutoff or `feed_closure`: previously retro-quarantined
proof-backed operations may revive.

Two smaller exactness defects belong in the same patch:

- define “fork point” as the last common ancestor. If two statements first
  differ at sequence `k`, commitment to `A_k` selects A; only a commitment
  through `k-1` selects neither. The current statement that a commitment “at
  or below the fork” is ancestor to both is off by one unless “fork point” is
  redefined
  ([fork rule](/Users/vm/owner-plane-d0a-spec.md:694)); and
- renewal forbids reusing only the immediate predecessor key. A rotation
  `A -> B -> A` resurrects the old `(device_id, signing key)` scope while the
  new feed restarts at sequence 1, colliding with the historical registry.
  Require that a device never reuse any previous signing key, or key scope by
  certificate incarnation
  ([scope and renewal](/Users/vm/owner-plane-d0a-spec.md:710)).

## 6. Recovery coverage and revisit rules are still incomplete

C3′ says cut-branch tenant operations past the base quarantine “per
`tenant_cutoffs`,” but the array may be empty and is capped at 256 while a
plane may have thousands of live heads/lineages
([recovery body](/Users/vm/owner-plane-d0a-spec.md:1398),
[E8](/Users/vm/owner-plane-d0a-spec.md:129)). There is no default for an
unnamed lineage and no continuation operation. E4 explicitly prohibits
unstated defaults.

Specify whether omission means preserve, zero-history cut, or invalid
incomplete recovery. If complete explicit coverage is required, add paging or
a continuation mechanism so the posture is representable at the protocol's
own scale. If omission is fail-closed, put that semantic in the schema and
vectors.

The incremental reducer also needs an explicit **boundary removal/recompute**
event when C3′ cuts prior control state. At minimum replay:

- selector removed -> issuer suffixes freeze again;
- feed closure or compromise cutoff removed -> proofs may revive;
- supersede/revoke/close/seal removed -> tenant boundary state recomputes;
- adopted/cut rotation state changes -> storage state follows C3′; and
- a held-byte incorporation cap whose authority was only on the cut branch is
  removed or explicitly survives under a named rule.

“Every admission input is one of proofs, budgets, boundaries” is a useful
derivation principle, but an incremental implementation also needs the events
that **remove or widen** those functions, not only those that add or narrow
them.

## 7. D-125 can re-add a deliberately excluded device

`held_zones(device)` includes zones where the device has an effective wrap at
**any accepted epoch**, and renewal rewraps the effective-wrap subset at each
zone's latest accepted epoch
([definition](/Users/vm/owner-plane-d0a-spec.md:460),
[`c.enroll`](/Users/vm/owner-plane-d0a-spec.md:1195)). This composes badly
with membership-excluding rotations:

1. device D has a wrap in zone Z at epoch E1;
2. a legitimate rotation creates E2 while intentionally omitting D;
3. D remains enrolled because it still belongs to another zone;
4. Z remains in D's effective-wrap set because its E1 wrap still exists in
   accepted control history; and
5. a later KEM renewal is required to add D at Z's latest accepted epoch E2.

The renewal has silently restored access that E2 removed, contradicting
“renewal mints no new access.”

The Kold custody rule has the matching liveness failure. It retains Kold until
every accepted, locally unretired epoch of every effective-wrap zone has a
Knew wrap. An epoch that intentionally excludes D has no such wrap, so Kold
can never be destroyed unless the implementation wrongly re-adds D
([renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2938)).

Split three concepts:

1. **historical finite-zone set** for the monotone 128-zone admission cap;
2. **current intended-membership set** for renewal, derived from whether D has
   an effective wrap in the latest accepted membership epoch; and
3. **old-key custody tuples**: every locally unretired
   `(zone, epoch, device)` whose effective wrap actually targets Kold must be
   superseded by Knew or locally retired before Kold destruction.

Absence from an epoch must not require a wrap. State separately whether old
historical membership permanently consumes one of the 128 cap slots; with no
portable wrap-removal operation, that is the current consequence and should
be explicit.

Required traces include E1-includes/E2-excludes/renewal, a queued include
followed by an excluding epoch, replicas with different local Fence progress,
and eventual Kold destruction without re-admission.

## 8. D-123 still lacks a logical-import collision rule

The destination replay key is
`(from_plane, release_op, source_op)`
([§11.8](/Users/vm/owner-plane-d0a-spec.md:2135)). But two authorized
destination writers can emit byte-distinct `m.import.claim` operations under
the same key. The body permits different sensitivity, timestamps,
provenance, evidence, and labels, and the signed headers necessarily have
independent writer/request identities
([`mimport`](/Users/vm/owner-plane-d0a-spec.md:3255)). Claim identity remains
the emitting operation hash
([N4](/Users/vm/owner-plane-d0a-spec.md:296)).

No current outcome closes the collision:

- `duplicate` means byte-identical replay, not two valid claimants;
- first-seen-wins diverges replicas;
- accepting both violates one-record idempotency and creates two claims; and
- choosing one without a canonical rule makes budgets, chains, and status
  arrival-dependent.

Define an order-independent result. Plausible postures are a uniquely
designated import writer, a deterministic logical imported object whose
mutable-looking fields are derived/forbidden, or a canonical claimant/freeze
rule with its budget and chain effects defined. Vector both arrival orders and
a fresh fold.

The umbrella execution-ID sentence is also too coarse: it says import writes
are idempotent per `release_op`
([effect execution](/Users/vm/owner-plane-d0a-spec.md:441)), but one release
intentionally creates up to 128 imports. A release-level deduper can write the
first and suppress the other 127. Publish a small normative table:

| Effect | Idempotency / critical-section key |
|---|---|
| source release delivery and source terminal | `release_op` |
| one destination import | `(from_plane, release_op, source_op)` |
| audited read result release | `read_id` |
| later Agenda effects | their defined occurrence/effect ID |

Finally pin redundant-field equality or remove the redundant copies. The
source Txn should require at least:

```text
PendingXfer.release_op     = H_op(enclosed release)
PendingXfer.export_id      = release.body.export_id
PendingXfer.content_digest = release.body.content_digest
PendingXfer.dest_zone      = release.body.to.zone_id
PendingXfer.record_count   = |release.body.sources|
```

The bundle, import provenance, and terminal should carry the same correlation
`export_id` if it remains present. A mismatch needs an explicit storage or
body disposition.

## 9. D-126 omits permanent quarantine from terminal cleanup

D-126 closes a dormant source journal if the release resolves
`reject-permanent`, and lets destination `missing` contain attempts that
resolve reject-permanent/fence-hardened
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:1053)). But §10.5 has a
different terminal-negative lifecycle: an operation beyond an
`immutable_cap` or inside a voided generation remains
`quarantine-reproposal` while being permanently non-revivable
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1830)).

Trace:

1. release R and `PendingXfer` commit, but R is not effect-final;
2. a valid later seal places R beyond the immutable bound;
3. R can never revive, but it never receives the `reject-permanent` outcome;
4. the `release-rejected` cleanup branch never fires; and
5. the source journal remains forever.

The same leak exists for a durable destination import that becomes
permanently quarantined before finality: it can neither complete nor become
eligible `missing` under the listed cases.

Define cleanup in terms of **permanent non-revivability**, not one disposition
name. Conversely, every revivable state must remain unresolved: missing
proof, accepted-not-final, budget displacement, requester-snapshot/cutoff
quarantine below an immutable cap, and any other state that later proof,
budget release, recovery removal, or boundary growth can revive. This makes
the predicate total:

- unresolved/revivable attempt -> terminal defers;
- effect-final accepted attempt -> completed;
- permanently non-revivable attempt or never-arrived record -> eligible
  `missing`.

Use a reason such as `destination-unrecoverable` if `reject-permanent` no
longer accurately describes the whole destination set. Add source- and
destination-side permanent-quarantine vectors.

## 10. Two authority lifecycle operations remain outside the “exhaustive” reducer

These are not introduced by v0.5.9, but the full replay still finds them at
freeze level.

### 10.1 `c.lineage_reauth` is a missing revival event

`lineage-gen` is a quarantine outcome whose stated remedy is
`c.lineage_reauth`; that operation opens another generation window
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1600)). The T2 inventory claims
nothing outside proofs, budgets, and boundaries revisits an operation, but a
generation-window change is none of the listed events.

Either add lineage-window state as a fourth derived admission function whose
growth revisits held `w.gen` bytes, or say old bytes never revive and the
writer must re-propose under a new operation/request identity. Vector the
chosen lifecycle.

### 10.2 `c.space_retire` has no portable write boundary

`c.space_retire` says future writes reject while reads/status remain
([registry](/Users/vm/owner-plane-d0a-spec.md:1201)), but it carries no tenant
cutoff and does not advance a capability epoch. A delayed pre-retirement
operation and a post-retirement mint under the still-valid old grant are
indistinguishable in the signed tenant bytes.

A fold-current “space is retired now” check creates the familiar order split:
write-first accepts while retire-first rejects the same final byte set. Make
retirement advance the zone capability epoch with closure coverage, carry
per-lineage cutoffs, or introduce a dedicated signed space-write boundary.
Then add retirement to the revisit algebra.

## 11. Schema and vector pins

These are smaller than the blockers above but should land in the same
exactness pass:

- Every requester `zoneheads.heads` member must have the requester's lineage,
  and the outer `zone_id` must be the zone in which the Head resolves. Define
  the exact equality/coverage relation between carried zone/generation sets
  and the cutoff entries. The comment currently says “requester lineage,” but
  the CDDL does not make the cross-field invariant normative
  ([`zoneheads`](/Users/vm/owner-plane-d0a-spec.md:3059)).
- A requester `ratifycutoff` with `accepted_through: "none"` cannot satisfy
  “generation present in carried live heads” if zero history has no Head.
  Add the explicit empty-generation special case.
- `c.abandon_writer.seals[].at` must resolve in the body's outer `zone_id` as
  well as matching `gen` and `lineage`; Head itself has no zone field
  ([`cabandon`](/Users/vm/owner-plane-d0a-spec.md:3090)).
- D-117 needs an explicit accepted-but-unFenced non-adoption vector. This is a
  recovery-authoring rule based on the local authoring device's durable
  state, not a portable claim that every replica can observe global Fence
  absence.
- D-118 needs a same-lineage, multi-generation omission-preservation vector
  and a negative where a lineage fence lies below one covered generation.
- D-124 needs all three carrier pairings in both control orders, same-branch
  ancestor and descendant commitments, missing-chain-first versus
  chain-first, earlier-pending versus later-conflicting, common-ancestor
  versus first-divergent commitment, recovery removal, and signing-key reuse.
- D-121/D-122 need absent-versus-explicit-`"none"`, zero-history requester,
  cap-eligibility, cap removal by recovery, and finality-with-inadmissible-cap
  vectors.

The normative companion `d0a-vector-cases.v1.json` is still absent, so Gate A
is false independently of the prose findings. Preserve the specified order:
companion schema first, then fixtures, harness, required surfaces, family 14,
and the final prose-to-vector discrepancy audit.

## Recommended v0.5.10 sequence

The shortest safe sequence is:

1. **Repair construction first:** remove `release_op` from the bundle hash
   preimage and add a construct-and-rederive vector requirement.
2. **Make the owner decisions:** choose D-122 cap eligibility/finality;
   requester entry-versus-snapshot semantics; scalar promotion; import
   collision behavior; and recovery omission semantics.
3. **Close order independence:** Head-reference set semantics, D-124 missing
   ancestry and pending reservation, recovery removal/revival, and signing-key
   scope reuse.
4. **Split KEM predicates:** historical admission cap, current intended
   membership, and actual Kold-wrapped local tuples.
5. **Totalize transfer lifecycle:** exact effect-ID table, mirror equalities,
   logical import collision, and permanent-non-revivability cleanup.
6. **Close the residual lifecycle:** `c.lineage_reauth`, `c.space_retire`, and
   the schema pins above.
7. **Only then author the companion and corpus.** A fixture should validate
   the protocol, not silently choose among unresolved reducer semantics.

## Bottom line

V0.5.9 is a disciplined response to the v0.5.8 synthesis and its architectural
direction remains strong. It is also exactly the kind of revision that
benefits from one more adversarial composition pass: individually sensible
repairs cross their own boundaries and reveal a fixed point, an unauthorized
destructive primitive, and several missing state transitions.

Do not freeze this draft. A focused v0.5.10 can close it without reopening the
Owner Plane or Memory architecture: make every durable fact derivable from a
canonical byte set, every missing dependency pending rather than permanent,
every authority-ending event position-relative, and every terminal predicate
total over the disposition lattice.
