# Review: D0-A Core + Memory normative specification v0.5.5

*2026-07-12. Fresh review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.5,
diffed against the archived v0.5.4 and replayed against the adjudicated
[v0.5.4 synthesis](/Users/vm/owner-plane-d0a-spec-v0.5.4-synthesized-review.md).
The review checked D-93 through D-99 first against their original
counterexamples, then composed the new cutoff, budget, proof, checkpoint,
recovery, rotation and transfer rules in fresh two-replica/crash traces.*

## Executive verdict

**V0.5.5 is the strongest cut yet and closes most of the direct v0.5.4
findings. It is not a valid freeze candidate. I recommend a focused v0.5.6
before authoring the normative case schema or declaring Gate A.**

The cut gets a great deal right. The grant-epoch lower bound is exact. Budget
charges now have a derived eligible set and revival cascade. T2 acknowledges
all three intended revisit paths. Proof statements below a committed boundary
must prove a complete path to its head. Requester live heads are finally in
the wire. Checkpoint retirement now means what the operation itself retires,
the object has a joint byte cap, and witness values are bounded. Fence and
RewrapDone carry the missing control frontier and recipient commitment.
Transfer terminal precedence is deterministic, empty aborts are impossible,
and the self-attested bundle-size charge is gone. The control pipeline checks
body hash and CDDL before applying recovery precedence. The companion schema
is honestly marked artifact-pending instead of being described as present.

The residual problems are mostly interactions created by those fixes:

- a growable ratify boundary is used to establish effect finality, then may
  revive an earlier budget consumer after the effect escaped;
- requester `gens_total` is neither carried nor monotone, and “current tenant
  state” still has no portable position relative to a control operation;
- recovery adopts only a rotation hash, not the particular Fence/control/
  recipient state being adopted;
- a legal lineage with 257 live unknown-gap heads cannot be represented by
  latest-page-wins checkpoint pages of 256;
- KEM renewal counts every historically accepted epoch, so draining a local
  queue never makes the stated ≤128 precondition true again; and
- body structure now precedes C3′ precedence, but state-dependent recovery
  validity and exact missing-reference outcomes still do not.

Recommended disposition:

- **Direction:** accept.
- **D-93–D-99:** substantial landed machinery; all remain at least partly
  open except D-98's narrow terminal-precedence repair.
- **Protocol/schema freeze:** no.
- **Gate A:** no. The companion, core, corpus and harness do not exist, family
  14 remains open, and the protocol findings below precede executable work.
- **Next cut:** v0.5.6, focused on the seven state-machine decisions below.
- **Durable P1 writes:** unchanged and still prohibited until the later gates.

## Closure ledger

| Decision | What v0.5.5 genuinely closes | Remaining disposition |
|---|---|---|
| D-93 | Grant-epoch lower bound; equality pins; scalable-closure direction; referenced-Head pending/mismatch direction; purpose categories | Partial: renewal/generic scope, comparator, receipt rule, outcome and snapshot semantics |
| D-94 | Accepted-only charge set; cascading release/revival; T2 wording; named effect barrier | Partial: growable ratify boundaries and cross-lineage dependencies defeat finality; deferred-effect lifecycle conflicts |
| D-95 | Original alternative-prefix attack; minimum-cutoff ancestry; carried `live_heads`; formula alignment | Partial: `gens_total`, cross-log freshness, later boundary/fork revisits and checkpoint proof-position monotonicity |
| D-96 | Immediate-prestate retirement; 48-KiB joint cap; E7 keys; witness-value cap; fence non-regression direction | Partial: wide-lineage paging and exact page/comparator transition |
| D-97 | Simple Fence crash recovery; portable wrap-add admission; explicit unadopted-storage quarantine direction; renewal pins | Partial: recovery adoption bytes, closed outcome, historical epoch count and renewal interlock |
| D-98 | State-derived terminal order; replay-keys-first; nonempty abort; removal of self-attested size | Mostly closed locally; charge name/mirror, durable effect deferral and destination-finality integration remain |
| D-99 | Body hash, registry arm and CDDL precede C3′ placement | Partial: state-dependent recovery invalidity can still be discovered after precedence; failure outcomes remain broad |

## What should be preserved

1. **Grant anchoring is fixed correctly.** The lower bound precedes slack
   subtraction, so an epoch-5 grant cannot select `policy(1)`.
   ([§9.4](/Users/vm/owner-plane-d0a-spec.md:1396))
2. **The D-86 charge fold now has explicit revival.** Quarantined operations
   release charge and the suffix is re-derived instead of retaining an
   arrival-dependent loser.
   ([§4.3](/Users/vm/owner-plane-d0a-spec.md:373))
3. **The original proof-prefix attack is closed.** An immediate predecessor
   match cannot qualify a statement until the chain reaches the committed
   head; minimum boundaries require ancestry.
   ([T3](/Users/vm/owner-plane-d0a-spec.md:630))
4. **Checkpoint retirement has the right local meaning.** It now records only
   heads this checkpoint removes from its immediate pre-state; ordinary
   `w.gen`/cutoff retirements are not forced through a later page.
   ([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1062))
5. **Fence intent survives the simple crash trace.** Both frames carry
   `control_frontier` and `recipients_hash`.
   ([frame table](/Users/vm/owner-plane-d0a-spec.md:886),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2615))
6. **Transfer terminal precedence is now total in the ordinary trace.** All
   imports first yields XferDone even after erasure; erasure precedes a
   destination rejection; abort residue is nonempty.
   ([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:947))
7. **D-99 repairs the immediate body-integrity defect.** A malformed or
   hash-mismatched recovery body no longer receives precedence merely because
   its header signature verifies.
   ([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1484))
8. **Artifact status is honest.** D-91 and the Open list now say the companion
   schema does not exist yet.
   ([decision/Open list](/Users/vm/owner-plane-d0a-spec.md:2228))

## Freeze-blocking findings

### 1. The effect-finality boundary can be reopened

#### 1.1 A growable ratify cutoff cannot prove finality

D-94 declares an effect final when every lower generation is closed, including
closure by a ratify cutoff. D-93 simultaneously allows ratify cutoffs to grow
and revive operations beyond the earlier boundary.
([effect finality](/Users/vm/owner-plane-d0a-spec.md:399),
[ratify algebra](/Users/vm/owner-plane-d0a-spec.md:1071))

Counterexample with a three-operation budget:

1. H1 in generation 1, `w.gen(last_known="unknown")`, and egress release B in
   generation 2 consume the budget.
2. `c.cutoff(H1)` closes the gap; B is deemed effect-final and exports.
3. Delayed generation-1 operation A at H2 is initially quarantined.
4. A later `c.cutoff(H2)` revives A.
5. Canonical order becomes H1, A, `w.gen`, B; B is displaced after its bytes
   escaped.

Use only an immutable close/reservation boundary for effect finality, seal a
ratify boundary once an effect relies on it, or reserve budget for the whole
potential earlier prefix. The present claim that no earlier consumer can
arrive is false under the immediately following ratify rule.

#### 1.2 Finality must include every authorizing/result dependency

The barrier examines “every generation of **its lineage** below it.” An export
may read claims from other lineages. A release in closed lineage B can export
claim S from lineage A while A still has an unknown lower-generation gap; a
delayed A operation can displace S afterward. Audited result sets have the
same multi-writer shape.
([effect predicate](/Users/vm/owner-plane-d0a-spec.md:399),
[export sources](/Users/vm/owner-plane-d0a-spec.md:1787))

Effect finality must cover the release/audit operation **and** every source or
result operation whose standing constitutes or authorizes the escaped effect,
at the signed evaluation frontier.

#### 1.3 Storage prose still completes egress at acceptance

Section 6.1 says an egress release completes at release acceptance, while
D-94 separates admission from effect execution and §11.8 invokes the barrier.
There is also no durable pending/terminal record for an egress effect deferred
across a crash.
([storage rule](/Users/vm/owner-plane-d0a-spec.md:963),
[export rule](/Users/vm/owner-plane-d0a-spec.md:1806))

If `m.export.release` is only portable authorization, say what component owns
execution and idempotency. If D0-A owns completion, add a durable deferred-
effect lifecycle and replace “at acceptance” with “once accepted and
effect-final.” Later compromise can still invalidate evidence for an already
escaped effect; name that unavoidable residual rather than implying no
executed effect can ever be revisited.

### 2. Requester freshness remains arrival-relative

#### 2.1 `gens_total` is signed but absent from both wire shapes

Both requester formulas include `gens_total`; neither `clineagereauth.requester`
nor `ccutoff.requester` carries it. Unlike `lineage_version` and `repoch`, this
value derives from tenant logs, not the preceding control chain.
([reauth CDDL](/Users/vm/owner-plane-d0a-spec.md:2540),
[cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2570))

A replica receiving control first reconstructs different signed bytes from a
replica that already holds the latest `w.gen`. Carrying the scalar alone is
not sufficient: admission also needs the tenant frontier against which it was
counted.

#### 2.2 The count is not monotone under the new budget fold

`w.gen` is a charged operation. D-94 can displace or retro-disqualify an
accepted charged operation. Therefore a “lifetime accepted `w.gen` count” can
decrease. If `gens_total` instead means “ever observed accepted,” it depends
on arrival history and a fresh replay differs from an incremental one.
([w.gen](/Users/vm/owner-plane-d0a-spec.md:1326),
[eligible charge set](/Users/vm/owner-plane-d0a-spec.md:390))

Use a monotone control-plane nonce/counter consumed by the ceremony, or bind a
complete immutable tenant-frontier snapshot. Do not derive a freshness nonce
from a reversible accepted set.

#### 2.3 Carrying live heads does not order the control and tenant logs

Consider a withheld `w.gen` and a cutoff request signed over the earlier head
set. A replica processing the control operation first sees equality and
accepts; one processing `w.gen` first sees a stale request. There is no tenant
frontier in the control body and no deterministic relation between those two
logs. Accepting the cutoff may itself quarantine the write that made the
request stale, creating a circular fixed point.

Choose one posture explicitly:

- requester assent is durable authority to ratify the carried snapshot, so
  later writes do not invalidate it; or
- freshness against later writes is required, in which case tenant operations
  need a monotone control-visible nonce/frontier that creates a portable order.

The current “dies the moment the lineage writes past it” promise cannot be
implemented from the signed bytes.

### 3. The cutoff algebra still lacks exact scope and lifecycle

#### 3.1 Renewal and generic ratification share one boundary

D-93 puts `c.cutoff` and renewal `history_cutoffs` in the same max-composed
ratify class. Renewal separately says a particular predecessor certificate is
valid only through its supplied history boundary.
([cutoff algebra](/Users/vm/owner-plane-d0a-spec.md:1067),
[renewal semantics](/Users/vm/owner-plane-d0a-spec.md:316))

If C1→C2 supersession closes C1 at H5, a later generic `c.cutoff(H10)` either
restores C1 authority through H10 or reveals that the renewal boundary was
secretly certificate-scoped. The bytes contain no such identity. Conversely,
if the H5 ratify boundary remains global, it can constrain C2's later lineage
history.

Give supersession a predecessor-certificate boundary, and define whether a
generic admin ratification may deliberately amend it. Likewise, when staged
ratify cutoffs satisfy a later epoch-close operation, snapshot them into that
operation's immutable close boundaries; later ratify growth must not reopen
old-epoch writing.

#### 3.2 Head maximum and fence order are still undefined

`accepted_through` is a full Head, yet “maximum” never states the comparator.
Pin `none < (gen,seq)`, lexicographic generation/sequence ordering, and
equal-coordinate hash rules, or another explicit total order. Reuse the same
definition for checkpoint fences.

#### 3.3 Receipt-cutoff rules contradict

D-93 classifies device/service receipt cutoffs as revoke boundaries that are
“never composed.” T3 says repeated feed cutoffs merge at the minimum with
ancestor proof. The likely rule is that each individual revoke boundary is
immutable while effective boundaries intersect at the minimum; state that
instead of leaving two literal reducers.
([D-93 revoke rule](/Users/vm/owner-plane-d0a-spec.md:1077),
[T3 merge](/Users/vm/owner-plane-d0a-spec.md:656))

#### 3.4 Missing Heads have a disposition but no outcome

D-93 calls an unheld cutoff Head `pending-dependency`, but that is a
disposition. No closed outcome names this case, §10.5 does not list it, and
most affected control rows still advertise reject-permanent.
([Head lifecycle](/Users/vm/owner-plane-d0a-spec.md:1088),
[outcomes](/Users/vm/owner-plane-d0a-spec.md:1512))

Add an exact outcome such as `control-reference-missing`, its ordering, and
the rule that later control operations may pass or may not pass each pending
control operation. Apply it uniformly to cutoff Heads, rotation references and
adoption dependencies.

### 4. Checkpoint paging cannot represent one legal wide lineage

`covers` has at most 256 heads. A later page naming lineage L replaces L's
earlier coverage. But a lineage may legally have more than 256 live heads:
`max_generations` is an unbounded uint and the Frontier permits 4096 heads.
With 257 `last_known="unknown"` generations, page 2 erases page 1; complete
effective coverage is impossible.
([caps](/Users/vm/owner-plane-d0a-spec.md:119),
[page transition](/Users/vm/owner-plane-d0a-spec.md:1062),
[generation window](/Users/vm/owner-plane-d0a-spec.md:1320))

Cap live heads per lineage at 256, page by `(lineage, range/page-id)`, or make
latest-wins operate per `(lineage, gen)` with explicit retirement/removal.

The page machine also needs:

- the cross-generation fence comparator;
- a definition of whether a fence-only or retired-only occurrence of L
  replaces, preserves, or clears L's coverage;
- a way to encode an explicit empty replacement;
- a rule preventing a replacement from citing an older accepted head and
  regressing coverage; and
- non-regression/ancestor rules for successive `proof_positions`.

The 64-witness cap is useful, but does not alone prove that 64 proof positions
suffice: witnesses are stable device IDs while proof feeds are keyed by
certificate/signing key, and renewal starts another feed. Close predecessor
feeds at renewal, cap active scopes, or page/accumulate proof positions.

### 5. Recovery adoption does not identify the adopted storage state

#### 5.1 One rotation hash does not select a Fence commitment

`adopted_rotations` carries only `{zone_id, rotation_op}`. Two replicas may
have Fenced the same rotation at different control frontiers—one before and
one after wrap-add W—and therefore hold different `recipients_hash` values.
The same recovery bytes adopt different states on the two replicas.
([recovery semantics](/Users/vm/owner-plane-d0a-spec.md:1158),
[recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:2600))

The adoption must commit the exact activated state: at least the Fence
control frontier, recipient hash, epoch, required wrap/control dependencies,
and ordered rotation prefix or an equivalent active-storage checkpoint.
`adopted_rotations` is keyed only by zone, so it also cannot describe several
cut activated rotations unless adopting the latest explicitly adopts and
validates the complete dependency prefix.

#### 5.2 Invalid adoption can still receive recovery precedence

D-99 validates body hash and CDDL before precedence, which is correct. It
validates state-dependent invariants afterward. A recovery operation with a
valid base/epoch/repoch but an invalid adopted-rotation reference or cutoff
Head can therefore suppress C2 before the reference is proven.

All conditions required for recovery acceptance must resolve before its
precedence exception, or precedence must remain provisional and produce no
transition/effect. A later missing dependency may pend; a later mismatch must
never invalidate a recovery operation that already retired branches.

#### 5.3 Unadopted activated storage has no closed outcome

The prose assigns `storage-quarantine`, a disposition. The closed storage
outcomes contain no cut/orphaned-epoch member, so E10 cannot map the required
transition.
([storage outcomes](/Users/vm/owner-plane-d0a-spec.md:1528),
[disposition table](/Users/vm/owner-plane-d0a-spec.md:1544))

Add a dedicated outcome and recovery vectors.

### 6. The selected KEM-renewal bound never drains

A KEM-changing renewal now requires one wrap for **every accepted epoch** of
every held zone and at most 128 accepted memberships. “Drain queued rotations
first” is offered as the remedy. Accepted epochs never leave control history;
local state-6 completion does not reduce that number. After 129 historical
zone-epochs, renewal is permanently unencodable.
([renewal row](/Users/vm/owner-plane-d0a-spec.md:1046),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2478))

Define a portable control-side epoch-completion/retirement fact, stage bounded
future-certificate wraps before atomic renewal, or explicitly impose a lifetime
128-zone-epoch ceiling. The current hard-drain precondition is not executable.

The Fence→state-6 interlock also remains. If device D renews from Kold to Knew
after Fence but before RewrapComplete, the Fence recipient hash names Kold
while the logical wrap map now supersedes it with Knew. Specify historical-
Fence semantics and retain Kold until completion, or make renewal wait on a
portable completion fact.

Mechanical custody pins still missing from the prior synthesis: require
`cwrapadd.zone_id/epoch` to equal its inner wrap, and every rotation wrap to
equal the rotation plane/zone/new epoch.

### 7. D-98 needs exact charge and terminal integration

The terminal-order and self-attestation repairs are sound. Three mirrors remain:

1. The charge uses `record_count × 512 B`, but `mexportrel` has no
   `record_count`. State `record_count = |sources|` everywhere and validate
   `PendingXfer.record_count` against it. For egress there is no PendingXfer,
   so the current term is otherwise undefined.
   ([charge](/Users/vm/owner-plane-d0a-spec.md:1806),
   [release CDDL](/Users/vm/owner-plane-d0a-spec.md:2691))
2. The operation registry still says “1 op + bundle bytes” and promises a
   PendingXfer for every release, contrary to the fixed surcharge and the
   egress exception.
   ([registry](/Users/vm/owner-plane-d0a-spec.md:1604))
3. XferDone must wait until every destination import is accepted **and
   effect-final**. Otherwise an import can later be displaced while the source
   journal remains terminal. On crash recovery, destination logs/replay-key
   indexes must rebuild before source PendingXfers resolve, or an already
   durable import can be mistaken for missing after source erasure.

The fixed 512-byte surcharge is deterministic, but it is a record-rate charge,
not a byte-egress bound. A 16-KiB claim may be exported repeatedly for 512 B
each, potentially under a different grant from the one that paid for storage.
If that weakening is intentional, name it; otherwise use a worst-case or
independently committed byte charge.

### 8. Feed/checkpoint revisits need one last rule

The original D-95 wrong-prefix counterexample is resolved. Two follow-ons
remain:

- if a later checkpoint commitment exposes an issuer fork after receipts from
  that branch already qualified operations, T2's “exactly three” revisit
  paths omits this transition; and
- successive checkpoint `proof_positions` do not require nondecreasing
  `through` or ancestor-consistent heads.

Add boundary-conflict/feed-freeze to the revisit enumeration and make proof
position accumulation monotone.

## Mechanical discrepancies before the vector audit

- §5.5's RewrapComplete literal omits the `control_frontier` and
  `recipients_hash` required by both frame schemas.
- `H_recips` has a tag and an inline comment but no named, versioned, capped
  CDDL object or declared logical key. Define the exact effective superseding
  wrap map hashed at Fence.
- `control_frontier` needs an exact identity/resolution rule and Fence
  validation must recompute `recipients_hash` from accepted control history.
- §4.6 says all retired heads live in checkpoints, contradicting the new rule
  that immediate retirements are never re-listed.
- D-80 still calls the checkpoint object versioned and `retired` a
  since-predecessor delta; mark those clauses superseded by D-96.
- O7 says all admin operations use the root key; the pipeline correctly uses
  the current `admin(e)` key.
- Inner `zoneheads.heads` needs an explicit set key/order, not only the outer
  `live_heads` key by zone.

## Gate and next step

The missing companion schema is now honestly Open-tracked, but it is still the
normative home of every exact vector case and does not exist anywhere under
`/Users/vm`. Neither the core, corpus nor harness exists, and family 14 remains
open. The header's present-tense self-contained claim and Gate-A checklist are
therefore not true yet.
([vector contract](/Users/vm/owner-plane-d0a-spec.md:1932),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:2246))

I would fix the protocol before writing that schema. In order:

1. make effect finality rely on immutable closure/reservation and include all
   effect dependencies;
2. choose a portable requester-snapshot/freshness model;
3. finish boundary identity/comparison and exact pending outcomes;
4. make checkpoint pages work for a 4096-head single lineage;
5. encode exact recovery adoption and a real epoch-retirement/renewal rule;
6. complete transfer/effect persistence and exact charge mirrors; and
7. make all recovery validity precede precedence, then close the outcome map.

Then cut v0.5.6, author `d0a-vector-cases.v1.json` as the first corpus
artifact, build the core/corpus/harness, run family 14, and perform the final
prose↔schema↔vector discrepancy audit. At that point executable evidence—not
another broad prose pass—should decide Gate A.
