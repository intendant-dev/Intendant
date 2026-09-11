# Synthesized review: D0-A Core + Memory normative specification v0.5.5

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.5-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.5-review.md)
and
[owner-plane-d0a-spec-v0.5.5-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.5-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.5.
This document resolves disagreements rather than unioning the reports.*

## Executive verdict

**Both reviews correctly conclude that v0.5.6 is required and Gate A is not
available. The peer catches an exact wire blocker, three stale effect-consumer
mirrors and an unclassified abandonment boundary. Its conclusion that these
amount to one field and roughly six sentences before freeze is not supported
by the full composed state machine.**

The peer's B1 is precise: D-95 signs tenant-derived `gens_total` into both
requester messages but carries it in neither requester object. That repeats
the very signed-but-not-carried error D-95 fixes for `live_heads`. Its M1 is
also exact: egress completion, transfer terminal recovery and audited-result
release still omit D-94's effect-finality gate. Its M2 is new and sound:
`c.abandon_writer.at` is a boundary Head that belongs to none of D-93's four
purposes. The live-head lag pin and the desire for a general signed-input
reconstruction invariant are worth adopting.

The proposed B1 repair is insufficient, however. Carrying `gens_total` and
waiting until a local tenant count “reaches” it does not establish a portable
snapshot: tenant and control logs have no total order, and the count of
accepted `w.gen` operations can decrease when the reversible D-94 budget fold
displaces one. A replica that saw count N and accepted the control operation
can later fall to N−1; a fresh replica can start at N−1. This needs a monotone
control-visible nonce or an immutable tenant-frontier commitment, not merely
one CDDL scalar.

The peer also credits the existence of new machinery as complete semantics.
That misses the fresh seams: a growable ratify boundary can reopen an escaped
effect; latest-page-wins cannot represent one legal 257-head lineage;
recovery adoption names a rotation but not the Fence state being adopted;
“drain first” cannot reduce the number of historically accepted epochs; and
state-invalid recovery references and missing cutoff Heads still lack exact
pre-precedence outcomes.

The adjudicated disposition is:

- **Resolved:** the grant-epoch lower bound; the original D-95 wrong-prefix
  trace; carried requester live heads; D-96 immediate-prestate retirement,
  joint byte cap and E7/witness pins; simple Fence crash intent; simple D-98
  terminal precedence and removal of self-attested bundle size; body hash/CDDL
  before C3′ precedence; honest artifact-pending status.
- **Partially resolved:** D-93 through D-99. D-98's local terminal-order repair
  is closest to complete; its integration remains unfinished.
- **V0.5.6:** required as a focused protocol/schema cut, not a one-field
  correction.
- **Protocol freeze:** no.
- **Gate A:** no. The companion schema, core, corpus and harness do not exist;
  family 14 remains open; protocol transitions below remain ambiguous.
- **Durable P1 writes:** remain prohibited under the unchanged later gates.

## Assessment of the peer review

### Findings to adopt

1. **B1 diagnosis.** `gens_total` is in both signature formulas and neither
   requester wire shape.
   ([registry](/Users/vm/owner-plane-d0a-spec.md:1058),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2540))
2. **M1's three mirrors.** Egress still completes at acceptance; transfer
   recovery writes terminal records without testing finality; audited results
   release after durability alone.
   ([egress](/Users/vm/owner-plane-d0a-spec.md:963),
   [transfer](/Users/vm/owner-plane-d0a-spec.md:947),
   [audit](/Users/vm/owner-plane-d0a-spec.md:1606))
3. **M2's abandonment gap.** `c.abandon_writer` has a boundary Head but no
   ratify/revoke/close/recover purpose, composition rule or missing-Head
   lifecycle.
   ([registry](/Users/vm/owner-plane-d0a-spec.md:1061),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2594))
4. **Lagging replicas should pend rather than reject.** A validator missing a
   carried live Head must not map that absence to stale/signature failure.
5. **Promote a reconstruction invariant, with one qualification:** every
   signature input must be carried directly **or derivable from an immutable
   carried reference**. “Body alone” literally would wrongly forbid legitimate
   derivation of `lineage_version`/`repoch` from control history.
6. **Credit the direct closures.** D-93–D-99 contain real structural progress;
   those mechanisms should be refined rather than rolled back.
7. **Executable work should follow the next clean prose/schema cut.** The
   companion remains the correct first corpus artifact.

### Why the peer's B1 repair is not complete

Adding `gens_total: uint` prevents signature-byte reconstruction from an
unstated scalar. It does not answer either of the harder questions:

1. **What snapshot is it counting?** Control operation C10 can arrive before
   or after a withheld tenant `w.gen`; there is no tenant frontier in the
   reauth body and no cross-log order.
2. **Why is it monotone?** `w.gen` is charged. Under D-94 an accepted charged
   operation may be displaced or retro-disqualified, so the current accepted
   count decreases. “Ever accepted” instead depends on arrival history.

([w.gen](/Users/vm/owner-plane-d0a-spec.md:1326),
[eligible charge set](/Users/vm/owner-plane-d0a-spec.md:390))

“Pending until the derived count reaches the carried value” can accept at N,
then see N+1 or fall to N−1. Another replica may see that state before the
control operation and reject or pend. The fix needs a control-plane nonce or a
complete immutable tenant snapshot plus an exact cross-log rule.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-93 cutoff algebra | Complete except abandon | Original grant/revoke attack fixed; renewal/generic ratify scope, comparator, receipt composition, missing-reference outcome and abandonment remain |
| D-94 finality | Only three stale mirrors | Mirrors are real, but growable ratification can displace an escaped effect and deferred egress has no durable lifecycle |
| D-95 requester | One non-carried scalar | Scalar is also nonmonotone and has no portable tenant snapshot; live-head equality remains cross-log-relative |
| D-96 checkpoint | No residue | A legal 257-head lineage cannot fit latest-page-wins pages; page identity/comparator/proof succession remain open |
| D-97 recovery | Adopt-or-quarantine complete | Direction is sound; adoption bytes do not select a Fence commitment or dependency prefix and quarantine has no outcome |
| D-97 renewal | ≤128 drain complete | Accepted epochs never leave control history, so draining local work does not reduce the count |
| D-98 transfer | Complete apart from mirrors | Local terminal order fixed; charge name/registry, import finality and crash rebuild ordering remain |
| D-99 control | Complete | Structural body validation moved correctly; state-invalid references may still reach precedence and outcomes are broad/missing |
| Artifacts | Step 8 discharged | Honest tracking is progress, but a nonexistent normative schema is still artifact-pending, not complete |

The peer's review is valuable because it finds compact, exact drift. Its
methodological weakness is treating the presence of each D-93–D-99 mechanism
as proof that newly composed traces close.

## Correction to the first review

One first-review claim should be downgraded. The cross-lineage export-source
example is not an independent demonstrated blocker because the release carries
a frozen `{data_frontier, control_frontier, as_of_ms}` evaluation point, and
transfer recovery explicitly re-derives against that snapshot rather than
later status.
([export stamp](/Users/vm/owner-plane-d0a-spec.md:1787),
[recovery stamp](/Users/vm/owner-plane-d0a-spec.md:942))

The specification should say explicitly that later displacement/status change
outside that frozen frontier does not retroactively invalidate the source
read. That is a clarification/vector, not the main finality blocker. The
release writer's own ratify/revival displacement trace remains fully valid.

Successive checkpoint `proof_positions` non-regression is likewise best
treated as a medium monotonicity pin unless a separate divergent fold is
demonstrated. The first review's other major findings survive. Add the peer's
`c.abandon_writer` finding.

## Consolidated v0.5.6 findings

### 1. Give requester ceremonies a portable monotone snapshot

Carry `gens_total` if it remains in the signature, but do not derive freshness
from a reversible accepted-event count. Choose one:

- a monotone control-plane requester nonce advanced by `c.lineage_reauth` and
  requester-attested `c.cutoff`;
- an immutable, complete per-zone tenant-frontier commitment carried by the
  ceremony; or
- an explicit durable one-shot authority posture that does not claim later
  tenant writes invalidate the request.

The selected mechanism must define control-first, tenant-first, behind,
ahead, withheld-write and later-budget-displacement traces identically.
`ccutoff.live_heads` needs a pending rule for missing carried heads, but
“equals current local set” cannot by itself prove that no write is withheld.

### 2. Make effect finality rely on an immutable boundary

D-94 treats a lower generation as closed by an accepted ratify cutoff. D-93
allows that boundary to grow and revive operations beyond it.
([effect rule](/Users/vm/owner-plane-d0a-spec.md:399),
[ratify rule](/Users/vm/owner-plane-d0a-spec.md:1071))

Trace:

1. H1, unknown-gap `w.gen`, and generation-2 egress B fill a budget.
2. Ratify H1 closes the lower generation; B executes.
3. A delayed lower-generation A beyond H1 is quarantined.
4. Ratify grows to A's head and revives it.
5. Canonical ordering displaces already-executed B.

A finality decision must use an immutable close/reservation, seal the ratify
boundary once relied upon, or reserve the entire possible earlier prefix. The
three consumer mirrors identified by the peer then need exact updates:

- egress completes only after acceptance **and** effect finality;
- transfer terminals inherit the release finality barrier; and
- audited results require durable audit rows **and** effect finality.

If D0-A owns deferred egress execution, add a durable/idempotent pending and
terminal lifecycle. If it owns only authorization, name the execution owner
and semantics. Later compromise invalidation of already-escaped evidence is
an unavoidable residual and should be stated separately.

### 3. Finish boundary purpose, comparison and outcomes

The original grant-revoke/generic-ratify widening attack is fixed. Four
specific gaps remain:

1. Renewal `history_cutoffs` and generic `c.cutoff`s still share one
   max-composed ratify boundary. A later H10 either widens predecessor
   certificate C1 beyond its H5 supersession boundary or reveals an unstated
   certificate scope. Give supersession a predecessor-cert boundary or state
   explicit admin re-ratification semantics.
   ([renewal](/Users/vm/owner-plane-d0a-spec.md:316),
   [ratify class](/Users/vm/owner-plane-d0a-spec.md:1071))
2. Define the exact total comparator for `none` and Head `(gen,seq,op)`, and
   reuse it for checkpoint fences.
3. Reconcile D-93's “receipt cutoffs never compose” with T3's minimum merge;
   individual boundaries can be immutable while their effective intersection
   is the minimum.
   ([D-93](/Users/vm/owner-plane-d0a-spec.md:1077),
   [T3](/Users/vm/owner-plane-d0a-spec.md:656))
4. Classify `c.abandon_writer` as close-like immutable (or another explicit
   purpose), including missing-Head behavior and interaction with later
   ratification.

An unheld cutoff/adoption/rotation reference currently has a disposition but
no closed outcome. Add the outcome, exact stage ordering and whether later
control operations may pass a pending reference.

### 4. Make checkpoint paging work at legal cardinality

`covers` caps at 256, while latest-page-wins replaces the prior page for the
same lineage. A lineage may legally hold 257–4096 unknown-gap heads because
`max_generations` is an unbounded uint and Frontier caps only the plane/zone
set at 4096.
([caps](/Users/vm/owner-plane-d0a-spec.md:119),
[page semantics](/Users/vm/owner-plane-d0a-spec.md:1062),
[generation rules](/Users/vm/owner-plane-d0a-spec.md:1320))

Cap live heads per lineage, page by lineage range/slot, or accumulate latest
state per `(lineage,gen)` with explicit removal. Also define:

- the cross-generation fence comparator;
- fence-only/retired-only/explicit-empty page membership;
- coverage non-regression; and
- predecessor-feed closure or paging when one stable witness has several
  historical certificate-key feed scopes.

Successive checkpoint proof positions should be ancestor-consistent and
nondecreasing; treat this as a required pin in the case schema.

### 5. Encode the exact recovery adoption, not only its rotation hash

The simple Fence crash trace is fixed. Recovery adoption is not yet portable.
`adopted_rotations` carries only `{zone_id, rotation_op}`. Replica A may have
Fenced before wrap-add W and replica B after W; the same adoption bytes select
different control frontiers and recipient hashes.
([recovery](/Users/vm/owner-plane-d0a-spec.md:1158),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:2600))

Adopt an exact activated-rotation prefix or active-storage checkpoint per
zone, including Fence control frontier, recipient hash, epoch and required
cut-branch wrap/enrollment/erase dependencies. Define whether adopting the
latest rotation adopts its entire predecessor chain; the current set key
allows only one entry per zone.

An activated-but-unadopted replica transitions to the
`storage-quarantine` disposition, but no closed storage outcome names the
condition. Add one.

### 6. Replace the non-draining KEM-renewal precondition

Renewal requires a replacement wrap for every **accepted epoch** of every held
zone and says to drain queued rotations until memberships are ≤128. Accepted
epochs never disappear from control history, so local state-6 completion does
not reduce that count. After 129 historical memberships, KEM renewal is
permanently unencodable.
([renewal row](/Users/vm/owner-plane-d0a-spec.md:1046),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2478))

Use a portable epoch-retirement/completion fact, bounded pre-renewal staging
with atomic activation, or an explicit lifetime cap. Also define the
Fence→state-6 Kold/Knew interlock and retain historical Fence custody until
completion.

### 7. Complete control precedence and the closed outcome map

D-99 correctly moves body hash and CDDL before precedence. It leaves
state-dependent validity afterward. A recovery with a valid base/epoch/repoch
but a bad or missing adopted rotation/cutoff reference can therefore appear
to suppress C2 before the reference resolves.
([pipeline](/Users/vm/owner-plane-d0a-spec.md:1484))

All recovery acceptance conditions must resolve before precedence, or
placement must be explicitly provisional and effect-free until the final
transition. Pin exact outcomes for arm resolution, precedence, state
invariants, missing control references and orphaned storage; the current
pipeline points to broad outcome families.

### 8. Finish D-98 integration and storage mirrors

D-98's deterministic surcharge and simple terminal order are genuine
closures. Remaining exactness:

- define `record_count = |release.sources|` because `mexportrel` carries no
  `record_count`, and validate the PendingXfer mirror;
- update the registry's stale “bundle bytes”/unconditional PendingXfer row;
- require XferDone to count accepted, effect-final destination imports and
  rebuild destination replay indexes before source PendingXfer recovery;
- align RewrapComplete's state-machine literal with its frame fields;
- give `H_recips` a named closed CDDL type/key/cap and define
  `control_frontier` identity plus recipient-hash recomputation; and
- pin outer/inner wrap zone/epoch/plane equality.

([charge](/Users/vm/owner-plane-d0a-spec.md:1806),
[registry](/Users/vm/owner-plane-d0a-spec.md:1604),
[RewrapComplete](/Users/vm/owner-plane-d0a-spec.md:798))

The 512-byte surcharge is explicitly a record-rate rather than exact egress-
byte bound. Name that posture or replace it with a worst-case/committed byte
charge; this is a policy pin, not the main protocol blocker.

### 9. Complete feed revisits and the normative artifacts

The original D-95 alternative-prefix window is fixed. If a later checkpoint
commitment discovers an issuer fork after that feed already qualified
operations, T2's “exactly three” revisit list omits the required feed-freeze/
quarantine transition. Add it and its vector.
([T2](/Users/vm/owner-plane-d0a-spec.md:622),
[boundary gate](/Users/vm/owner-plane-d0a-spec.md:646))

The companion case schema remains absent and D-91 remains artifact-pending.
Tracking it is honest; it is not completion of the synthesis's artifact step.
The self-contained claim and Gate-A checklist become true only after the
companion exists, all fixtures validate against it, the core/corpus/harness
run, family 14 is recorded and the discrepancy audit is clean.
([vector contract](/Users/vm/owner-plane-d0a-spec.md:1932),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:2246))

Mechanical discrepancy-audit pins from the first review also remain: retire-
history wording, D-80's stale checkpoint description, O7's root/current-admin
key wording and inner `zoneheads.heads` ordering.

## Adjudicated v0.5.6 sequence

The shortest defensible order is:

1. choose the requester snapshot/monotonicity model and carry every signature
   input or immutable reference;
2. make effect finality immutable, durable and consistent at every consumer;
3. finish cutoff/abandon purposes, comparison and missing-reference outcomes;
4. repair wide-lineage checkpoint paging and page/proof monotonicity;
5. encode exact recovery adoption and a real epoch-retirement/renewal path;
6. make recovery validity precede precedence and close the outcome map;
7. finish transfer/storage mirrors and feed-fork revisit semantics; and
8. update the decision record, then author the companion schema as the first
   corpus artifact.

This remains a focused convergence pass; it does not reopen the architecture,
crypto suite, hosted ceiling, Memory model or operation vocabulary. It is more
than one field and six sentences because several fixes change durable bytes,
outcomes and crash behavior.

## Final recommendation

Adopt the peer's B1, M1, M2, both pins and direct closure credits. Strengthen
B1's remedy to a portable monotone snapshot. Retain the first review's
ratify/finality, cross-log requester, wide-checkpoint, recovery-adoption,
renewal and outcome findings, with the cross-lineage export clarification
downgraded as above.

**Cut v0.5.6, but do not call its prose frozen before the executable audit.**
Once its protocol and CDDL agree, author `d0a-vector-cases.v1.json`, build the
core/corpus/harness, run family 14 and use the prose↔schema↔vector discrepancy
audit as the Gate-A decider. Durable P1 writes remain unchanged and later-gated.
