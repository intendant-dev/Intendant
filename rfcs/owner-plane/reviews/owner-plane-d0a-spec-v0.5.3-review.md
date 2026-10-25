# Review: D0-A Core + Memory normative specification v0.5.3

*2026-07-12. Reviewed against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.3,
the archived
[`v0.5.2 text`](/Users/vm/agenda-rfc-archive/2026-07-12-d0a-v0.5.2-as-reviewed.md),
and the
[`v0.5.2 synthesized review`](/Users/vm/owner-plane-d0a-spec-v0.5.2-synthesized-review.md).
The review independently traces convergence, control/tenant ordering,
checkpoint and rotation recovery, transfer terminality, and prose↔CDDL
constructibility.*

## Verdict

**Do not freeze the schema or begin the canonical corpus. Cut a focused
v0.5.4, then move immediately to executable conformance work.**

V0.5.3 is another substantial improvement. It closes several issues cleanly:

- D-78 removes the fold-current capability-epoch comparison that caused the
  original policy-update/tenant-operation arrival divergence;
- D-79 separates policy epochs from budget windows;
- D-80 embeds and chains checkpoint objects, fixing their carriage and
  identity;
- D-82 replaces the resetting `window_state` with a control-derived lineage
  version;
- D-83 lands the audit scope, mediated-principal split, third trigger, and
  zero-result rule in both prose and CDDL;
- D-85 derives device grant revocation, signs the export charge, fixes the
  import policy anchor, adds the signature stage, aligns the frame mirrors,
  pins genesis ceilings and corrects the E8 arithmetic.

Keep those directions. The remaining blockers are mostly interactions among
otherwise sensible repairs:

1. current certificate/grant status is still checked before historical
   cutoffs, so fresh replay can reject operations the cutoff says stand;
2. budgets shared across concurrent generation chains still select winners by
   arrival order;
3. dense receipt cutoffs still distinguish delayed delivery from backfill by
   local arrival order;
4. embedded checkpoints cannot fit their advertised Frontier and use the
   wrong witness universe for historical pending operations;
5. queued rotations can strand required wrap-adds, and the next Fence may run
   before the prior rotation's tombstones exist;
6. the two renewal shapes are not encodable and do not define wrapping across
   active/queued epochs;
7. collect-all transfer recovery is impossible after one source is erased
   under a flat, non-persisted bundle digest;
8. the promised typed vector contract and separate control-operation pipeline
   still do not exist in normative form.

These require a v0.5.4, but not a redesign of the owner plane. The work is now
a bounded exactness pass.

## Discharge of the v0.5.2 synthesis

| Decision | V0.5.3 disposition |
|---|---|
| D-78 portable epoch currency | **Partial.** The original current-epoch defect is fixed. Historical cert/grant status, cutoff dependency, multi-head semantics and the 64-lineage ceiling remain open. |
| D-79 budget windows | **Partial.** Policy edits no longer reset budgets; window identity and ordering across concurrent generations remain undefined. |
| D-80 checkpoint embedding | **Partial.** Carriage/hash indirection is fixed; size, feed universe, pending coverage and retirement are not. |
| D-81 Fence activation | **Partial.** Active epoch and basic Fence ordering are clear; staged recipients and erase completion can deadlock or overlap incorrectly. |
| D-82 requester freshness | **Partial.** Ordinary reauthorization replay is fixed; cutoff freshness and recovery-branch rollback remain open. |
| D-83 audit wire | **Resolved.** Prose, CDDL and vector requirements now agree in all material respects. |
| D-84 authority-free renewal | **Open at the wire.** The prose defines two shapes, but CDDL encodes neither distinction; wrapping and certificate-delta authority remain incomplete. |
| D-85 mechanical closure | **Substantially partial.** Many pins landed; dense-prefix replay, transfer erasure, control precedence and vector typing remain blockers. |

## Gate-A blockers

### 1. Historical authorization and cutoff replay are not canonical

D-78 makes epoch selection portable, but the ordered admission pipeline still
resolves **current** certificate and grant status before its chain/cutoff
stage.
([renewal history](/Users/vm/owner-plane-d0a-spec.md:314),
[pipeline](/Users/vm/owner-plane-d0a-spec.md:1290))

Minimal grant counterexample:

1. O is validly authored under grant G.
2. `c.revoke_grant(G, cutoff=O)` is accepted; its normative meaning is that O
   and earlier operations stand.
3. Replica A admitted O first and retains it.
4. A fresh replica folds control first, resolves G as revoked at the proof
   stage, and returns `no-grant` before reaching the cutoff.

Certificate renewal and `c.revoke_device` have the same shape: the prose
preserves pre-cutoff history, but a fresh fold sees `cert-superseded` or
`cert-revoked` first. Define cert/grant resolution as historical and
cutoff-aware, or define one canonical control/tenant interleaving. Also narrow
T2's remaining “only compromise cutoffs revisit” statement to proof
qualification; D-78 closure and grant cutoffs necessarily revisit epoch or
authority status.
([T2](/Users/vm/owner-plane-d0a-spec.md:576),
[grant cutoff](/Users/vm/owner-plane-d0a-spec.md:958))

The new closure-cutoff mechanism has four additional exactness failures:

- strict epoch advances must cover every live lineage, but the body caps at
  64 while a zone may have thousands; after lineage 65, neither a policy
  change nor a budget bump is constructible;
- a referenced tenant Head may be missing when the control operation arrives,
  but no cutoff-head pending outcome exists;
- one `zonecutoff` names one Head for `(zone,lineage)`, while that lineage may
  have several incomparable generation heads; “at/before/beyond” and which
  heads are retired are not defined;
- cross-fields are not pinned: zone-policy/bump cutoffs must name that
  operation's zone, and a grant cutoff must name the revoked grant's exact
  zone and lineage.

([caps](/Users/vm/owner-plane-d0a-spec.md:99),
[epoch closure](/Users/vm/owner-plane-d0a-spec.md:1246),
[`zonecutoff`](/Users/vm/owner-plane-d0a-spec.md:2248))

Use a continuation/intent for strict closure, give missing tenant heads a
portable dependency lifecycle, and define cutoff order by causal ancestry:
normally the named branch stands through its named Head and every incomparable
or later branch is cut. Vector all three arrival orders.

Finally, add the missing lower bound
`op.capability_epoch >= grant.capability_epoch` before applying slack. As
written, a grant issued at epoch 5 can cite an operation claiming epoch 1;
implementations may underflow, reject, or accept, and the operation can select
a policy older than the grant itself.
([grant slack](/Users/vm/owner-plane-d0a-spec.md:1243))

### 2. Budget and raise-quota admission still depend on arrival order

D-79 correctly says only `c.cap_epoch_bump` opens a new budget window. Define
the window of operation epoch `e` explicitly as the most recent bump at or
before the control operation that opened `e`; “since the last bump” must not
mean the fold's current last bump.

More importantly, lineage-summed budgets span several live generation chains,
but those chains have no total order. With room for two operations:

1. O1 extends still-live generation 1.
2. W2 opens generation 2 with `last_known="unknown"`.
3. O2 follows W2.
4. A sees O1, W2, O2 and quarantines O2; B sees W2, O2, O1 and quarantines O1.

Every chain is internally valid and both replicas eventually possess the same
bytes. The same issue applies to `raise_quota`. Define a canonical total
budget order across live heads—then re-evaluate deterministically—or use a
signed reservation/allocation model that cannot oversubscribe.
([budget definition](/Users/vm/owner-plane-d0a-spec.md:364),
[multi-head lineages](/Users/vm/owner-plane-d0a-spec.md:1195))

### 3. Dense-prefix cutoffs still confuse network delay with backfill

D-85 says `through=N` means the cutoff author held statements `1..N`, but the
wire still carries only `{key_id, through}`. It carries neither a signed head
hash nor an accumulator, and validators are not required to possess the prefix
before accepting the cutoff.
([T3](/Users/vm/owner-plane-d0a-spec.md:583),
[receipt cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2197))

Replica A holds 1–100 when cutoff 100 arrives. Replica B holds 1–50, accepts
the same cutoff, then receives honest statement 51. Under the new rule B
freezes the issuer as `issuer-gap`, while A remains healthy. “Arrived later”
is a local event, not proof of post-compromise minting.

Make cutoff effect pending until the exact contiguous prefix is locally
available, with an explicit missing-prefix outcome; or commit a signed/hash-
chained feed head or accumulator that a replica can verify. `issuer-gap`
cannot be defined by receipt arrival relative to the control operation.

### 4. D-82 fixes reauthorization but not all requester freshness

The monotonic control-chain `lineage_version` prevents two ordinary banked
reauthorizations at one version from both succeeding. Pin its initial value as
zero and compare against the control operation's pre-state.

Two cases remain:

- ordinary writes and earlier cutoffs do not advance `lineage_version`, so a
  hosted device can sign a cutoff through H, continue writing H+1, and a
  delayed service can later submit the old request and discard H+1;
- C3′ recovery can cut a later reauthorization branch, returning the derived
  count from N+1 to N and making an unconsumed pre-recovery attestation at N
  valid again.

([request rows](/Users/vm/owner-plane-d0a-spec.md:960),
[recovery branch cut](/Users/vm/owner-plane-d0a-spec.md:1005))

Either explicitly ratify durable delayed cutoff intent, or bind cutoff assent
to the current live-head/cutoff commitment. Include `repoch` or another
recovery-advanced nonce so branch-cut recovery cannot resurrect requests.

### 5. Embedded checkpoints remain unbounded and semantically incomplete

D-80 successfully makes the control log carry the checkpoint object. The
object now embeds `covers: Frontier` inside a ≤64-KiB control operation, while
Frontier permits 4096 heads. It also permits 256 retired heads and 64 proof
positions in the same body. A full legal Frontier cannot fit; no partial-cover
type or joint cap is defined.
([control cap](/Users/vm/owner-plane-d0a-spec.md:99),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2104))

The attempted delta also cannot always page. If more than 256 heads retire
after the previous checkpoint, a later checkpoint cannot list them all; a
second checkpoint cannot truthfully say the omitted heads retired since its
immediate predecessor. Define a paged accumulator/delta protocol, enforce a
retirement backpressure rule, or choose smaller jointly generated state caps.

The proof universe is wrong for hardening. `proof_positions` lists only
**currently** qualified issuers, while a pending operation is judged under its
own historical signed policy epoch. If epoch 1 qualifies A, O remains pending,
and epoch 2 replaces A with B, a checkpoint listing only B can quarantine O
even though a later A receipt still qualifies for O. Cover the union of feeds
required by every operation being hardened, and cap `time_witnesses` or define
feed paging.
([policy anchor](/Users/vm/owner-plane-d0a-spec.md:553),
[checkpoint transition](/Users/vm/owner-plane-d0a-spec.md:964))

Three more contradictions need exact rulings:

- live Frontier heads are accepted-only, yet a pending tail is declared
  covered by a Frontier Head at or beyond its unaccepted coordinates;
- §4.6 says checkpoint coverage drops incorporated heads, while D-80 says
  exactly `retired` does;
- E6 says operation bodies carry no `v`, but the embedded checkpoint control
  body begins with `v: 1`.

Use a distinct bounded position-fence type if pending coordinates must be
covered, pin the one retirement formula, and add an explicit E6 exception or
remove the redundant body version.
([Frontier](/Users/vm/owner-plane-d0a-spec.md:482),
[E6](/Users/vm/owner-plane-d0a-spec.md:77))

### 6. Rotation queueing and renewal are not constructible together

D-81 makes Fence the activation point, but only Fences are serialized. This
can deadlock staged membership:

1. rotation N is control-accepted with a bounded initial wrap set;
2. rotation N+1 is accepted before N's remaining wrap-adds;
3. `c.wrap_add` is legal only for the current control epoch, now N+1;
4. N's required wrap-adds can no longer admit, so N cannot reach
   RewrapComplete and N+1 cannot Fence.

Require portable recipient closure before accepting N+1, or allow an
explicitly identified authorized-but-not-retired epoch in `c.wrap_add`.
([rotation queue](/Users/vm/owner-plane-d0a-spec.md:770),
[`c.wrap_add`](/Users/vm/owner-plane-d0a-spec.md:951))

Serialization also stops one state too early. N+1 may Fence after N's
`KekDestroyed`, but N's tombstones are written in state 6 afterward. An item
in N's erase manifest is then non-tombstoned at N+1's fence, lacks its
retiring-epoch wrapper, and cannot appear in another manifest because the
first entry already won. Gate the next Fence on durable tombstones, preferably
with a `RotationDone` marker after state 6.
([rotation states](/Users/vm/owner-plane-d0a-spec.md:718))

D-84's renewal is directly prose↔CDDL inconsistent. The registry requires
`grants[]` absent on renewal; `cenroll` still requires the `grants` key.
An empty array is not absence under E4. Encode an actual
`cenrollnew / cenrollrenew` union.
([renewal registry](/Users/vm/owner-plane-d0a-spec.md:948),
[`cenroll`](/Users/vm/owner-plane-d0a-spec.md:2184))

Then define renewal/rotation interaction:

- if an epoch-2 rotation is authorized but not Fenced, wrapping only the
  control-current epoch loses current epoch-1 access; wrapping only the active
  epoch loses access when epoch 2 activates;
- a replacement wrap collides with the old wrap's logical key
  `(zone,epoch,device_id)` unless certificate-renewal supersession is defined;
- require every renewal wrap's `recipient_kem_key` to equal the renewed
  certificate and decide whether a fresh KEM key is mandatory;
- certificate class and deadline changes can activate persistent
  device-bound grants, so “mints no authority” is stronger than the actual
  rule—constrain those deltas or say only “mints no grants/new-zone access.”

The renewal shape also lacks a scalable cap/continuation. A legal maximum
renewal with 128 wraps and 128 headed history cutoffs already exceeds the
64-KiB control cap; `history_cutoffs` itself is uncapped while a trusted device
may span many zones. Generate safe joint limits and a continuation or
accumulator.

D-84 also does not make “hosted remedy = re-root” true at plane scope. Hosted
`c.enroll` still permits a **new** browser device with a fresh audit grant and
fresh `grant_id` budget. Narrow the claim to “no refresh for an existing
device,” explicitly accept enrollment as the remedy, or make the intended
limit plane-wide rather than per grant.

### 7. Transfer charging is durable, but post-erasure recovery is not

Signed `bundle_size` makes the charged number replayable once admission is
trusted. It does not make a flat, non-persisted bundle reconstructible.

Suppose a release contains A and B, PendingXfer is durable, A is erased, and B
has not imported when recovery runs. D-85 says A's permanent failure does not
stop B. But the source can no longer reconstruct A's statement/class floor,
so it cannot reproduce the full `H_bundle` preimage that proves B belongs to
the signed release. The destination cannot verify a partial flat bundle.
([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:1591),
[recovery](/Users/vm/owner-plane-d0a-spec.md:851))

Choose one policy:

- actual source erasure aborts every remaining record;
- erasure waits until every referencing PendingXfer is terminal;
- persist an encrypted bundle until terminal state; or
- use a per-record Merkle commitment and proofs.

The same missing preimage means a late replica cannot independently verify
that signed `bundle_size` was truthful after erasure; “replay as signed” trusts
an earlier validation event not present in portable bytes. A deterministic
worst-case charge or durable admission/bundle proof closes that path.

Finally, collect-all recovery may see both erased-source and destination-
rejection failures, but `XferAbort` has one scalar `reason`. Add per-record
reasons, a `mixed` value, or define the field solely as the terminal trigger.
([`XferAbort`](/Users/vm/owner-plane-d0a-spec.md:2305))

### 8. The control pipeline and vector contract are still promises, not bytes

The tenant signature stage is now explicit. “Control operations run the same
ordered stages” cannot be implemented literally: genesis/admin/recovery
operations have no device certificate for the cert stage; their key comes
from the descriptor, current admin state, or recovery commitment, and valid
recovery must be recognized before its C2 precedence exception can apply.
Define a separate ordered control pipeline with signer resolution and
`signer_alg`/`signer_key_id` checks for every arm.
([admission pipeline](/Users/vm/owner-plane-d0a-spec.md:1290),
[arm rules](/Users/vm/owner-plane-d0a-spec.md:969))

D-85 also says each family defines a closed `case_kind` vocabulary and exact
input/result fields. The actual JSON Schema still accepts any case-kind
string, arbitrary object inputs, and arbitrary JSON results; §13.3 lists no
tokens or schemas. A harness must still invent the contract.
([vector schema](/Users/vm/owner-plane-d0a-spec.md:1682),
[typed-case claim](/Users/vm/owner-plane-d0a-spec.md:1739))

Define a real conditional `$defs` union or a normative companion schema before
writing fixtures. This is the last place to make fixture semantics explicit,
not something the corpus should silently decide.

## Exactness pins for the same patch

- State that epoch 1 is active after genesis/zone creation despite having no
  Fence.
- Pin `KekDestroyed.epoch = new_epoch - 1` and
  `RewrapDone.count == |survivorset.pairs|`.
- Make `c.grant` reject issuance to a revoked device and define conflicts
  among repeated/continued cutoffs.
- State the main-section `H_genesis`/`H_cert(renews)` reference rules, not only
  Appendix comments, and require `renews` to identify the active predecessor.
- Repair the header provenance, which still names only v0.1–v0.4 archives and
  the v0.4 synthesis despite this being v0.5.3.

## Gate status and recommendation

Gate A remains mechanically unavailable: no `owner-plane-core`, canonical
corpus or harness exists in the workspace, and family 14 is still open.
([Gate A](/Users/vm/owner-plane-d0a-spec.md:1992),
[open item](/Users/vm/owner-plane-d0a-spec.md:1987))

Recommended v0.5.4 order:

1. historical cert/grant replay, cutoff dependency/multi-head/cap semantics,
   and canonical cross-generation budget ordering;
2. dense-feed commitment and requester freshness across cutoff/recovery;
3. checkpoint paging/position-fence/feed-union semantics;
4. rotation recipient closure, post-tombstone completion, and the real
   enrollment union/renewal interlock;
5. source-erasure transfer policy;
6. separate control precedence plus the actual case-kind schema;
7. then stop prose iteration, build the core and corpus, and let executable
   discrepancies drive any further amendments.

## Bottom line

V0.5.3 fixes the original D-69 arrival-order error and genuinely closes the
audit wire, checkpoint carriage, policy/budget-axis choice, transfer charge
field, frame mirrors and several reference/cap details. It is not yet a
freeze candidate: cutoffs, budgets, proof feeds and control outcomes still
lack canonical replay, while checkpoint/rotation/renewal/transfer recovery
contain direct constructibility contradictions.

Cut v0.5.4 as a narrow protocol-exactness patch. If it closes the items above,
the next meaningful review should be of executable vectors rather than
another prose-only revision.
