# Review: D0-A Core + Memory normative specification v0.5.8

*2026-07-12. Independent freeze review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md), v0.5.8,
SHA-256 `b2a2859d7d888d1ccc0b2aa1f52d1538da455ec8dd2c99b8cc40913f4d1ab262`.
The review replayed the v0.5.7 synthesis against the normative prose, CDDL,
decision ledger, and required vector matrix. The archived v0.5.7 source was
used for a direct revision audit; v0.5.8 is 200 insertions and 102 deletions
from that baseline.*

## Executive verdict

**V0.5.8 is a strong and materially improved revision, but it is not yet
protocol-freeze-ready. Cut a focused v0.5.9 before authoring the corpus.**

The revision genuinely resolves much of the v0.5.7 residue:

- ratify state is now explicitly per generation;
- the revisit inventory is derived rather than hand-numbered;
- direct issuer forks freeze both suffixes independent of delivery order;
- checkpoint coverage is keyed per `(lineage, generation)`, omission no longer
  erases other pages, and one lineage fence dominates all its covers;
- recovery adoption has the right E7 key, a hard bound, complete Fence
  identities, explicit dependency retention, and an unFenced-cut posture;
- global wrap validation, portable recipient caps, and renewal custody are at
  least named in every relevant layer;
- durable pending imports are recognized as terminal blockers; and
- transfer journals and terminals now bind `release_op`.

Those are substantial corrections. The remaining issues are not a return to
architecture debate. They are compositional exactness problems: several rules
depend on “ever accepted,” “unretired,” or an implicit winner without defining
an order-independent portable state from which a fresh implementation can
reproduce the answer.

I find five blocker clusters:

1. requester snapshots, scalar ratification, and immutable seals do not yet
   form one closed boundary state machine;
2. incorporation caps and `export_id` reservations are made durable by a
   local acceptance event that canonical displacement can erase on fresh
   replay;
3. issuer-feed commitments do not share one cross-type ancestry algebra, and
   renewal feed closure is missing from the exhaustive revisit rules;
4. portable KEM admission depends on locally retired epochs, while held-zone
   and current-key predicates remain contradictory; and
5. the transfer terminal algorithm has one unresolved-attempt contradiction
   and conflates two different rejection cleanup cases.

Recommended disposition:

- **Direction:** accept and preserve.
- **V0.5.8 as an implementation sketch:** yes.
- **Protocol/schema freeze:** no.
- **V0.5.9:** required, focused.
- **Gate A:** no; independently artifact-pending.
- **Durable P1 writes:** remain prohibited under the existing gates.

## 1. Boundary algebra still has two simultaneous truths

### 1.1 Sequential requester snapshots do not compose with ratify-max

The live algebra says ratify cutoffs max-compose per
`(zone, lineage, gen)`
([cutoff algebra](/Users/vm/owner-plane-d0a-spec.md:1165)). The requester form
separately says its carried heads are an outer snapshot boundary and every
uncarried successor retires
([operation row](/Users/vm/owner-plane-d0a-spec.md:1158)). No reducer equation
relates the persistent maximum to a later, smaller snapshot.

Concrete trace:

1. `C1` ratifies generation `g` through `H3`.
2. A later valid requester snapshot carries `H2` and includes a cutoff entry
   `H1`, satisfying `H1 <= H2`.
3. Ratify-max still admits through `H3`.
4. Snapshot-wins retires uncarried `H3`.

`H3` is now both admitted and retired. Re-emitting `H3` cannot serve as the
stated “ratify growth” revival because the stored maximum never decreased.
The required vectors currently test only the per-operation inequality, not
this sequential composition
([family 7](/Users/vm/owner-plane-d0a-spec.md:2222)).

A complete rule could maintain, per generation:

- `ratify_max`: maximum scalar ratification ever accepted;
- `snapshot_cap`: the latest applicable requester snapshot head; and
- `immutable_cap`: the intersection/minimum of incorporation and abandon
  bounds.

It would then define the effective boundary and name which later event can
raise a revivable snapshot cap. The exact equation is an owner decision, but
the current prose cannot leave all three independently authoritative.

### 1.2 Three schema variants remain unclosed

- `requester` is optional on trusted `c.cutoff`, yet the row unconditionally
  requires every cutoff entry to name a generation in carried `live_heads`.
  A requesterless cutoff has no carried set.
- `accepted_through = "none"` contains no generation, but ratify state is now
  keyed per generation. Define the generation it addresses or prohibit this
  sentinel for ratify-purpose cutoffs.
- On a trusted plane, one requester-bearing cutoff can syntactically name
  multiple lineages, while `zoneheads` carries no lineage and the signed
  `lineage_version` is singular
  ([CDDL](/Users/vm/owner-plane-d0a-spec.md:2858)). Either requester-bearing
  cutoffs are always own-one-lineage operations, or the wire must carry the
  missing lineage/version association.

### 1.3 Seal composition and disposition disagree

Repeated abandon seals for one generation have no effective intersection
rule. Since an immutable truncation cannot be widened, the natural rule is a
minimum with `"none"` as the strongest bound, but that must be normative.

The disposition map also says a snapshot-retired operation “under a sealed
generation” never revives
([dispositions](/Users/vm/owner-plane-d0a-spec.md:1719)). That is broader than
the algebra. If generation `g` is sealed at `H3`, a snapshot at `H1` may retire
`H2`; later ratification through `H3` is legal under the seal and is supposed
to revive ratify-relative quarantine. Permanence should attach to operations
**beyond the effective immutable seal/void**, not every operation in a
generation that happens to have a seal.

The lifetime sentence also groups a tenant-derived incorporation cap with a
control-authorized seal. A seal's creating `c.abandon_writer` can leave the
surviving control branch through C3′ recovery. If the seal nevertheless
persists, cut-branch admin authority survives the recovery that retired it; if
it disappears, D-114's blanket persistence statement is false. State
explicitly that control seals follow the surviving control history—or ratify
the much stronger recovery exception—and keep the `w.gen` cap-lifetime problem
separate.

### 1.4 The hosted exception is broader than its name

The operation row intentionally allows a hosted owner to seal below accepted
history or void a generation
([abandon row](/Users/vm/owner-plane-d0a-spec.md:1159)). The ceiling and D-15
describe the exception more narrowly as a “self-gap seal” needed to restore
unknown-gap finality
([hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:1376),
[D-15](/Users/vm/owner-plane-d0a-spec.md:2401)). The wire has no gap-only
predicate.

The revision already chose the broad authority posture and documented its
escaped-effect residual. Make the naming honest: this is hosted
own-lineage truncation authority, including void, not merely gap closure.
Alternatively add the missing gap-only constraint.

## 2. “Ever accepted” is not a portable fold fact

### 2.1 Incorporation caps diverge after canonical displacement

D-114 says an incorporation cap persists even if its creating `w.gen` is
later displaced or retro-quarantined
([incorporation cap](/Users/vm/owner-plane-d0a-spec.md:1208)). But tenant
budgets intentionally make acceptance a derived, revisable state: a late,
canonically earlier consumer displaces a later operation
([budget fold](/Users/vm/owner-plane-d0a-spec.md:395)).

Replay:

1. Replica A has an open generation 2 and one remaining budget slot.
2. It accepts `W = w.gen(g3, last_known = H1)` and records the generation-1
   incorporation cap.
3. A late generation-2 operation, canonically before `W`, consumes the slot
   and displaces `W`; A retains the cap under D-114.
4. Replica B receives the generation-2 operation before `W`. `W` never enters
   B's accepted set, so no stated rule creates the cap.
5. A later ratify cutoff above `H1` rejects on A and can admit on B.

The problem is not whether the cap should survive. It is that “was accepted at
some point on this replica” is arrival history, not signed state. Choose one:

- derive the cap from a final canonical qualification set;
- make gap finalization a portable control operation; or
- define a distinct, order-independent pre-budget qualification class whose
  valid `w.gen` boundaries always materialize.

Do not make an unrecorded local transition consensus state.

### 2.2 `export_id` has the same divergence

The release rule makes an ID plane-wide single-use once any release was “ever
accepted,” and displacement never frees it
([export rule](/Users/vm/owner-plane-d0a-spec.md:1985)).

Replica A can accept `R1(X)`, later displace it, and permanently reserve `X`.
Replica B can receive the canonical displacer first, never accept `R1`, and
then accept `R2(X)`. Adding `release_op` to local journal records correctly
separates attempts, but it does not define a portable winner for the
plane-wide ID.

Use an immutable-byte rule: for example a deterministic claimant ordering,
an explicit collision freeze, or identity `(export_id, release_op)` with the
plane-wide uniqueness requirement removed. Whichever rule is chosen must
produce the same result from a fresh fold and an incremental fold.

## 3. Proof boundaries need one registry, not pairwise rules

### 3.1 A renewal feed closure can retroactively disqualify proof

Renewal requires predecessor `feed_closure {key_id, through, head_hash}` but
does not require `through` to cover every already-qualified statement
([renewal row](/Users/vm/owner-plane-d0a-spec.md:1144),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2756)). T3 correctly makes
the closure an immutable boundary commitment
([T3](/Users/vm/owner-plane-d0a-spec.md:699)).

If receipt 8 already qualified operation X and renewal closes the old scope at
7, either:

- receipt 8 dies and X must be revisited; or
- receipt 8 stands, in which case the closure did not close the suffix and no
  portable fact distinguishes a pre-renewal mint from a post-renewal mint.

The exhaustive revisit inventory names compromise cutoffs and issuer-fork
resolution, but not renewal closure
([T2](/Users/vm/owner-plane-d0a-spec.md:667)). The clean posture is to state
that statements beyond `feed_closure` are disqualified, include closure in
proof-boundary revisits, and extend the escaped-effect residual. A portable
non-truncation proof is also possible, but a fold-current lower-bound check is
not.

The inventory should also say issuer-fork **discovery and resolution**:
discovery itself freezes qualifications from both suffixes; resolution later
re-qualifies only the selected branch.

### 3.2 Different boundary types can select different fork winners

Cutoffs, checkpoint `proof_positions`, and renewal `feed_closure` are all
boundary commitments. Ancestry consistency is currently specified only in
subsets: checkpoint-to-checkpoint, cutoff-to-cutoff, and
feed-closure-to-receipt-cutoff.

Trace:

1. An issuer forks into branches A and B at sequence 5.
2. A checkpoint commits A10 and therefore selects A.
3. A later renewal closure or first compromise cutoff commits B10.

Both commitments can pass their local rules, leaving two immutable winners.
Define one per-issuer-scope boundary registry across **all** commitment types.
Every new commitment must be ancestor-compatible with every effective prior
commitment, or fail with a named outcome. A commitment below the fork cannot
select either suffix; only a boundary at or descending from the fork can do
so.

## 4. KEM rules mix portable control state with local storage state

### 4.1 `unretired epoch` makes admission replica-dependent

E8 caps recipients per `(zone, accepted unretired epoch)` and `held_zones`
counts wraps over the same domain
([E8](/Users/vm/owner-plane-d0a-spec.md:102),
[held zones](/Users/vm/owner-plane-d0a-spec.md:458)). Yet epoch retirement is
local Fence/KekDestroyed progress
([active wrapper](/Users/vm/owner-plane-d0a-spec.md:818)), and `c.wrap_add`
explicitly says control admission never reads that progress
([registry](/Users/vm/owner-plane-d0a-spec.md:1147)).

Replay:

1. Epoch E2 has 256 recipients.
2. Replica A has Fenced E3 and destroyed E2; replica B has not.
3. The same 257th E2 wrap arrives.
4. On A it is a locally retired, inert wrap. On B it exceeds the cap for an
   unretired epoch.

The operation cannot both admit and reject. The same split can change whether
a grant/wrap creates a device's 129th held zone.

Separate the sets:

- a **portable control-derived cap set** used by operation admission; and
- a **local custody/activation set** used to decide when this box may destroy
  Kold.

The conservative portable recipient rule is ≤256 for every accepted epoch,
regardless of local retirement. `held_zones` likewise needs a control-only
definition or an explicit lifetime posture.

### 4.2 Renewal targets two different zone sets

The central definition says renewal rewraps only the effective-wrap subset of
`held_zones`. The registry and CDDL require one wrap per **held zone**, while
history closure covers only zones where the device already holds wraps
([central definition](/Users/vm/owner-plane-d0a-spec.md:458),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2764)).

A normal `c.grant` can give device D authority in zone Z without a wrap; grant
and wrap are separate operations. On KEM renewal, wrapping Z either grants D
cryptographic access it did not previously have—contrary to “no new-zone
acquisition”—or omits it and violates the every-held-zone rule.

Use the renewal-time effective-wrap-zone set for renewal and history coverage.
Define the 128-zone admission cap separately. Also pin wildcard grants: because
wildcards expand at evaluation time, a read-only `zone = "*"` grant must not
silently acquire every subsequently created zone for a cardinality rule that
claims only grant/wrap operations can create the 129th member.

### 4.3 “An enrolled certificate” is not current-key equality

The global CDDL rule accepts a wrap matching **an** enrolled certificate's KEM
key
([KekWrap](/Users/vm/owner-plane-d0a-spec.md:2713)). Superseded predecessor
certificates remain validation history
([renewal semantics](/Users/vm/owner-plane-d0a-spec.md:337)).

After C0→C1 rotates the KEM key and Kold is destroyed, an admin can submit a
new wrap for C0.Kold. If “enrolled” includes the historical predecessor, the
literal rule accepts it and can supersede C1's wrap for that
`(zone, epoch, device)`. A later Fence strands the device.

Require the unique current, unsuperseded certificate for `recipient_device` at
the control operation's position, retaining only the same-operation enrollment
exception.

### 4.4 One stale CDDL mirror remains

`recipientset` still says the cap is enforced only at enrollment/wrap-add and
only for the current epoch
([recipientset](/Users/vm/owner-plane-d0a-spec.md:2960)). Align this after the
portable epoch set is resolved.

## 5. Transfer closure is close, but not total

### 5.1 A resolved durable attempt is simultaneously in and out of `missing`

The recovery rule initially defines `missing` as unimported records with **no
durable destination attempt**, then says a durable pending attempt defers until
it becomes admitted or reject-permanent and that reject-permanent records may
be listed
([terminal algorithm](/Users/vm/owner-plane-d0a-spec.md:1031)).

For a one-record transfer:

1. the destination import is durable but pending proof;
2. the source is erased, so recovery correctly defers;
3. the proof later fence-hardens to permanent rejection;
4. the durable attempt still exists, so the literal “no durable attempt” set
   is empty; but an empty `XferAbort` is illegal.

Use **no unresolved durable attempt**. Pending proof and
accepted-but-not-effect-final remain unresolved; an effect-final import counts
completed; permanent/fence-hardened rejection is eligible for `missing`.

### 5.2 Two rejection cleanups are conflated

The finality exception is introduced for a **release** that is permanently
rejected and therefore can never become effect-final
([cleanup exception](/Users/vm/owner-plane-d0a-spec.md:1017)). Case 3 also emits
`reason = "reject-permanent"` when a **destination import** rejects, even though
the source release may remain accepted and has an ordinary finality barrier.

Specify separately:

- release-rejected journal cleanup, outside the release finality gate; and
- destination-rejected transfer termination, either still gated by the
  accepted release's effect finality or explicitly classified as a second
  authority-free cleanup case.

The prose shorthands at the start and end of §6.1 should also show
`release_op` on `PendingXfer`, `XferDone`, and `XferAbort`, matching Appendix A.

## 6. Decision-by-decision disposition

| Decision | V0.5.8 disposition |
|---|---|
| D-107 | **Partial:** per-generation shape is right; cap materialization and repeated-seal composition remain |
| D-108 | **Partial:** snapshot-wins arrival race is fixed; sequential snapshot↔ratify state and requester variants remain |
| D-109 / D-110 | **Partial:** intended caps are named; `unretired` is not portable and current-key equality is too broad |
| D-111 | **Mostly complete:** checkpoint paging/promotion are repaired; proof closure participates in the D-118 issue |
| D-112 / D-117 | **Materially complete in prose/CDDL:** key, bound, identity, dependency closure, and unFenced posture align; vectors still pending |
| D-113 | **Partial:** durable critical section stands; plane-wide export identity remains non-portable |
| D-114 | **Partial:** per-generation direction and residual posture landed; sequential composition and acceptance-event lifetime remain |
| D-115 | **Partial:** freeze-both is correct; cross-boundary winner consistency is not closed |
| D-116 | **Open/partial:** local vs portable epoch sets, renewal zone set, wildcard scope, and effective certificate remain |
| D-118 | **Partial:** checkpoint repairs landed; renewal closure needs revisit and cross-type ancestry rules |
| D-119 | **Partial:** durable attempts and `release_op` landed; terminal membership and cleanup scope remain |
| D-91 / Gate A | **Artifact-pending** |

## 7. Focused v0.5.9 change set

### A. Publish the actual boundary reducer

- Define `ratify_max`, requester snapshot state, incorporation cap, and
  repeated-seal intersection per `(zone, lineage, gen)`.
- Define requesterless and `"none"` ratify semantics.
- Restrict requester-bearing cutoff bodies to one explicit lineage or carry
  per-lineage versions.
- Make permanence depend on the effective immutable bound, not the existence
  of any seal in the generation.
- Define whether recovery branch-cut removes control seals; do not let a
  generic “once accepted” rule decide it implicitly.
- Reconcile “self-gap seal” language with the chosen hosted truncation power.

### B. Eliminate local acceptance-event consensus state

- Give incorporation-cap materialization an order-independent portable trigger.
- Give `export_id` conflicts a deterministic byte-derived winner/freeze, or
  make `release_op` part of the identity and drop plane-wide single use.

### C. Unify proof commitments

- Maintain one cross-type boundary registry per issuer scope.
- Require every cutoff/checkpoint/feed-closure commitment to be mutually
  ancestor-compatible.
- State when a boundary reaches a fork and can select its winner.
- Add issuer-fork discovery and renewal closure to the derived revisit rules
  and escaped-effect residual.

### D. Split portable membership from local custody

- Define the recipient cap over a control-derived epoch set.
- Define the 128-zone cap over a control-derived zone set.
- Define the local active/queued/not-destroyed epoch set separately for Kold
  deletion.
- Make renewal target exactly pre-existing effective-wrap zones.
- Require the effective current certificate's KEM key everywhere.
- Repair the `recipientset` mirror.

### E. Totalize transfer recovery

- Replace “no durable attempt” with “no unresolved durable attempt.”
- Separate release-rejection cleanup from destination-record rejection.
- Align all prose shorthands with the `release_op` CDDL.

## 8. Required regression traces before corpus authoring

Add these cases to the normative family inventory before the companion schema
freezes their shapes:

1. prior ratify H3 → later snapshot cap H2/cutoff H1 → later revival;
2. requesterless trusted cutoff, ratify `"none"`, and invalid multi-lineage
   requester body;
3. repeated seals H3→H1, snapshot H1→ratify H3 under a seal, and recovery
   cutting the control branch that created a seal;
4. `w.gen` accepted-then-displaced versus displaced-on-first-fold;
5. `export_id` accepted-then-displaced versus displaced-on-first-fold;
6. checkpoint selects fork A, then cutoff/feed closure attempts branch B;
7. receipt 8 qualified, then renewal closes through 7;
8. 257th old-epoch wrap on replicas at different Fence progress;
9. 129th held zone on replicas at different storage progress;
10. grant-only zone followed by KEM renewal;
11. post-renewal wrap using the superseded Kold certificate;
12. durable pending import → source erase → permanent proof failure;
13. release rejection versus destination-record rejection cleanup;
14. multi-page `(L,g1)` coverage surviving a page for `(L,g2)`, with one fence
    dominating both; and
15. accepted-but-unFenced cut rotation omitted and re-issued.

## 9. Gate A remains false by construction

The document explicitly keeps `d0a-vector-cases.v1.json` open and
artifact-pending
([open record](/Users/vm/owner-plane-d0a-spec.md:2506)). The Gate-A checklist
requires that companion to predate fixtures, every vector to validate against
both schemas, every named surface to pass, and the final discrepancy audit to
complete
([Gate A](/Users/vm/owner-plane-d0a-spec.md:2514)).

No companion, owner-plane core/harness, corpus, family-14 result, or surface
result is present in the repository or the expected home-document location.
That is not a criticism of v0.5.8—the specification correctly labels the
work pending—but it means “freeze candidate” must not be read as “Gate A
passed.”

After v0.5.9 closes the reducer choices:

1. author the companion schema;
2. implement the independent core/harness;
3. generate the corpus, including the traces above;
4. run the required surface matrix and family 14;
5. fold any behavior invented by fixtures back into the spec; and
6. perform the final prose↔CDDL↔companion↔vector discrepancy audit.

## Final recommendation

**Cut v0.5.9; do not freeze v0.5.8 or begin corpus authorship from it.** Preserve
the v0.5.8 direction, especially per-generation state, freeze-both issuer
forks, the checkpoint repairs, bounded recovery adoption, durable transfer
attempts, and `release_op` identity. The next revision should not broaden the
architecture. It should make every durable fact derivable from portable bytes,
publish the missing composition equations, and then let the vectors challenge
those equations rather than define them.
