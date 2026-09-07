# Synthesized review: D0-A Core + Memory normative specification v0.5.1

*2026-07-12. Adjudicated synthesis of
[`owner-plane-d0a-spec-v0.5.1-review.md`](/Users/vm/owner-plane-d0a-spec-v0.5.1-review.md)
and
[`owner-plane-d0a-spec-v0.5.1-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.5.1-review-2.md),
verified against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.1
and the v0.5 synthesized closure criteria. This document adjudicates both
reviews; it is not a union of their findings.*

## Executive verdict

**Both reviews agree that Gate A cannot be declared and that v0.5.2 should be
the final specification patch before executable conformance work. They
disagree materially about the size of that patch.**

The peer report is valuable. It independently confirms the trusted-genesis
verb defect and the missing bundle/Rewrap schemas, and adds two important
findings:

- survivor completeness is ambiguous across the second and later KEK
  rotations because “committed under the old epoch” can mean original commit
  epoch rather than current wrapper epoch;
- the audit trigger is stated as an exhaustive two-branch rule, yet the same
  row says audit-space reads are always audited.

Its service-descriptor reading is also a useful diagnosis, and several of its
pins should be adopted.

The peer's headline conclusion—“one prose-only micro-patch,” with no new wire
shape or owner ruling—is not supported by the normative bytes. It reads
several intended mechanisms charitably without testing whether they can be
constructed, replayed on two replicas, or recovered after a crash. In
particular:

- “citing operation's admission position” is not encoded or canonically
  derived;
- hosted revocation requires two operation hashes that depend on each other;
- the service-key rules reject a second key in one admin epoch;
- checkpoint identity, pending positions and proof-feed scope remain absent;
- the durable Fence omits the state needed to reconstruct the survivor set;
- the advertised wrap maximum cannot fit the 64-KiB control operation;
- transfer expiry can exceed the authorizing flow;
- audit chunks do not establish exact result coverage;
- Frontier retirement and valid genesis still admit divergent reducers.

The adjudicated disposition is:

- **Architecture and D-56 deadline posture:** accepted.
- **Most D-60…D-68 mechanisms:** keep; they are real progress.
- **V0.5.2 scope:** bounded exactness and lifecycle repair, but not merely
  stale prose. Some signed/stored schemas must change.
- **Schema/vector freeze:** no.
- **Gate A:** no, independently also blocked by the absent core/corpus/harness.
- **Durable P1 writes:** remain prohibited.

## Assessment of the peer review

### Findings to adopt

1. **F1 is valid and converges with the first review.** “Trusted: full verbs”
   literally includes reserved `admin`, making trusted genesis self-rejecting;
   a charitable reading still leaves fixtures to choose the list. Ratify the
   ordinary trusted grant as the exact 15-verb set excluding `admin` and the
   system-only `audit.write`; keep `audit.write` on the separate audit grant.
   This must be accompanied by the remaining bootability fields below.
2. **F2 is an important peer-only finding.** The survivor set must include
   every live pre-fence item required to hold the current old-epoch wrapper,
   regardless of its original commit epoch, minus the current manifest. Add a
   third-rotation vector containing an epoch-1-committed item.
3. **F3 identifies the right service-history direction.** Routine rotation
   must not silently invalidate every predecessor-key proof. Historic
   descriptors remain resolvable unless an explicit compromise cutoff excludes
   them. The peer's sentence repair is necessary but not sufficient; the
   service succession and policy-anchor mechanics also need changes.
4. **F4 is correct.** The current `iff` trigger and “audit-space reads are
   themselves audited” cannot both hold. The clearest intended repair is a
   third trigger branch: a read whose scope includes the audit space is always
   audited. Writing the resulting row is not a read, so recursion still ends.
5. **F5 and F6 are confirmed.** Move `bundle`/`bundlerec` into Appendix A and
   align `RewrapDone`; F6 is part of a larger durable-Fence repair rather than
   an editorial-only fix.
6. **Useful peer pins:** header/archive provenance; D-54 refinement note;
   `Write-capable` terminology; portable hosted-cutoff identity check; exact
   egress receipt coordinates; hosted-enroll grant targeting; shared-device
   principal residual; audit-principal shape 4; and the fail-closed policy
   cross-field.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-57 proof-policy binding | Complete with zero new bytes | Open blocker: no portable cross-log admission position exists; arrival order can permanently diverge replicas |
| Hosted revocation | Coherent exhaustive ceiling | Unconstructible: compound-first revoke and descendant rotation form a hash cycle; rotation recipients are also underconstrained |
| Service-key lifecycle | One ambiguous sentence | Protocol gap: same-epoch rotation rejects, leaf-key policy binding/history are unresolved, cutoffs do not close leases/older keys/gaps |
| Checkpoint/finality | Complete | No checkpoint object/hash, undefined `gc_fence`, unclear pending position, untagged proof scope, overbroad hardening sentence |
| Erase | Complete modulo F2/F6 prose | Target logic is strong, but Fence cannot recover D-67 and cap arithmetic is impossible; the survivor preimage remains incompletely versioned/keyed |
| Audit | Complete modulo trigger sentence | Trigger improved, but chunk identity, exact union, multi-space scope, result-ID domain and read-ID uniqueness are open |
| Transfer | Complete modulo Appendix location | Release can extend flow expiry; flow selection, bundle charge/cap/storage, plane equality and egress transition remain open |
| Frontier/genesis | Complete modulo verbs | Retirement rules conflict; genesis can be valid but unusable through deadline/lease/class/kind fields |
| Closed schema | Complete | Reference hashes, checkpoint, bundle and several stored frame payloads remain absent or inconsistent |
| Conformance | One micro-patch from freeze | No core/corpus/harness; offline result open; outcome and validation precedence still diverge |

### Adjudication of the peer's pins

Adopt with these exact rulings:

- `c.cutoff.requester.device_cert` resolves to a certificate whose
  `device_id` owns the target lineage, and its signature verifies under that
  certificate's key.
- Egress deadline proof uses the source zone and the release operation's
  `item_addr` as receipt subject.
- Every `cenroll.grants[]` entry and enrollment wrap targets the enrolled or
  renewed certificate's device. Existing-device grant changes require that
  device's own renewal or a separately admissible grant operation; fixtures
  must not choose.
- The trusted owner reads audit space through a later explicit `c.grant`, as
  the genesis row already implies.
- Humans sharing one enrolled device share the device-derived principal. State
  this as the principal-layer form of O4's admitted shared-custody residual.
- `deadline_fallback = "fail-closed"` with
  `require_cert_deadlines = false` rejects. D-56 already defines the latter as
  the certificate half; inventing a hybrid meaning would contradict it.
- Split audit-principal shape 4 into exact peer and mediated-session variants,
  or name one closed identifier rule for each. A field named only `peer` is
  insufficient for both.
- `result_ids` are identified explicitly—preferably the operation hashes of
  the returned records, which also covers audit-row reads—not
  fixture-defined `bytes32`s.

## Consolidated v0.5.2 freeze blockers

### 1. Canonical proof ordering and finality

D-57's “eventual admission position” is a local processing event, not a
portable position. An operation, proof and policy update can arrive in
different orders on two replicas; one admits under P, the other evaluates the
still-pending operation under P′, and the “never re-litigated” sentence makes
the difference permanent.
([T2](/Users/vm/owner-plane-d0a-spec.md:541),
[operation header](/Users/vm/owner-plane-d0a-spec.md:384))

Choose and encode or canonically derive one control position. Then reconcile
the intentional exception: device/service compromise cutoffs do revisit
previously qualified proofs. State the resulting transition for an operation
that was already admitted.

Checkpoint finality must close the same ordering model:

- define a versioned Checkpoint object, hash domain, zone, retained heads and
  exact cursor/fence domain;
- state how a still-pending operation occupies a position covered by the
  checkpoint;
- tag proof cutoffs with device/service issuer scope, not only `key_id`;
- make receipt/lease issuer sequences exact `+1` or hash-linked, preventing a
  compromised key from backfilling an unused sequence below a cutoff;
- narrow generic GC hardening to deadline/lease feeds, or add typed cutoffs for
  causal, certificate and policy dependencies.

([checkpoint](/Users/vm/owner-plane-d0a-spec.md:888),
[dispositions](/Users/vm/owner-plane-d0a-spec.md:1217))

### 2. Non-cyclic hosted revocation and exact self-service

The hosted rule requires an accepted revoke operation to contain the later
rotation's hash, while that descendant rotation's control-chain history
contains the revoke hash. No such pair can be constructed.
([hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:973))

Use a non-cyclic intent/rotate/complete ceremony keyed by `revocation_id`, or
permit a tightly constrained rotation-first operation that the later revoke
references. The constrained rotation must add no new recipient authority: its
recipients and KEM keys resolve exactly to already eligible non-target
devices. Freeze target grants/wrap additions while exclusion is pending.

Also:

- identify the revocation target explicitly (`device_id` or certificate
  reference) and define `revocation_id` across renewal;
- continue both rotation references and lineage cutoffs when the main
  operation reaches its cap;
- make `zonecutoff` outer/head lineage equality and zone membership global;
- require requester `ctrl_frontier` to identify a current/accepted window, so
  a service cannot stockpile several stale reauthorization requests and spend
  them after intervening generation consumption.

### 3. Service-key succession and historical proofs

V0.5.1 requires a service descriptor's `valid_from_admin_epoch` to equal the
current admin epoch and rejects a second descriptor for the same service and
epoch. Since `c.service_key` does not advance that epoch, ordinary rotation
requires an unrelated admin-key succession.
([service registry](/Users/vm/owner-plane-d0a-spec.md:881))

The policy stores a leaf `connect_service_key`, so merely appending a new
descriptor also does not explain how the new leaf becomes qualified while old
proofs remain historically resolvable. The peer's preferred historic reading
should be retained, but it needs a real succession model. At minimum:
descriptors persist as verification history; installation of a successor does
not invalidate the predecessor; a later explicit `c.zone_policy` rebind selects
the new leaf; and old policy positions continue resolving the old descriptor
unless a targeted cutoff disqualifies it. Alternatively, bind policy to a
stable service lineage/trust anchor with consecutive service generations and
explicit predecessor/target key IDs. Pick one model and freeze it.

Hosted planes cannot perform `c.service_key` or `c.zone_policy` before recovery
under the present ceiling. That is consistent with their witnessless budgets
posture, but should be stated so product behavior does not promise hosted
Connect time witnesses.

Apply compromise cutoffs to both receipts and leases; allow a cutoff to target
any historical key, not only the immediate predecessor; and state the effect
on already admitted and pending operations. Add old-key-after-routine-rotation,
old-key-after-cutoff and two-rotations-in-one-admin-epoch vectors.

### 4. Closed bytes, durable Fence and generated limits

The closed-schema claim needs one mechanical sweep:

- pin `cert_ref = H_cert(certificate)` and
  `cap_ref = H_grant(grant)` everywhere those references occur, plus
  `signer_key_id` equality with the referenced certificate key;
- add versioned Checkpoint and transfer bundle objects to Appendix A;
- register exact frame payloads for ItemRewrap, Fence, Receipt-or-Lease,
  RewrapDone and CtrlOp;
- make the survivor preimage a versioned object keyed by `item_addr`.

Persist `{rotation_op, kek_epoch, fence_frontier}` in the Fence itself. A crash
before RewrapDone otherwise loses the expected membership point; adding the
field only to RewrapDone is too late.
([frames](/Users/vm/owner-plane-d0a-spec.md:737),
[Rewrap CDDL](/Users/vm/owner-plane-d0a-spec.md:2026))

Regenerate E8 from exact canonical bytes. A minimal published `kekwrap` is
about 306 bytes, so 256 wraps alone are roughly 78 KiB before the enclosing
header and signature; the claimed 64-KiB fit is impossible. Publish safe joint
limits or a batch protocol with atomic authority semantics.
([E8](/Users/vm/owner-plane-d0a-spec.md:97),
[`kekwrap`](/Users/vm/owner-plane-d0a-spec.md:1930))

### 5. Multi-epoch survivor completeness

Keep D-67, but replace “committed under the old epoch” with an unambiguous
wrapper-current definition:

> Expected membership is every non-tombstoned item at or before the frozen
> Frontier that holds, or is required to hold, a wrapper under the retiring
> current epoch, minus the current rotation manifest.

An item's original commit epoch is irrelevant after it has been rewrapped.
Vector three rotations and require the third survivor set to include items
originally committed in epoch 1. This peer finding closes a real silent-data-
loss path.
([survivor rule](/Users/vm/owner-plane-d0a-spec.md:688))

### 6. Transfer authority and bundle accounting

Require the release deadline to be no later than the selected matching flow's
deadline. Define matching as an exact existential rule when several flows
match, including all endpoint/kind/class/deadline axes.
([release rule](/Users/vm/owner-plane-d0a-spec.md:1469),
[flow CDDL](/Users/vm/owner-plane-d0a-spec.md:1865))

Then close:

- whether bundle bytes count toward `max_bytes`—the current global rule and
  registry charge disagree;
- bundle maximum, delivery and deterministic recovery/persistence;
- `header.plane_id`, `zone_id` and `space_id` equality with a plane endpoint;
- egress versus plane transition (only the latter writes PendingXfer);
- egress proof coordinates: source zone and release item address.

Keep the now-correct versioned digest, per-record floor, snapshot,
record-level replay, XferDone and XferAbort machinery.

### 7. Exact audit trigger, scope and partition

Define the trigger as three branches:

1. scope contains a sensitive-minimum space;
2. results contain an effective-sensitive claim;
3. scope contains the audit space.

Define `result_ids`' identity domain and a unique read-ID scope. For all rows
of one read, require one principal and canonical scope, indexes exactly
`0..count-1`, disjoint result sets, and an exact union equal to the released
results. Either restrict each request to one `(zone,space)` or make scope a
bounded canonical set; the current singular scope cannot represent the
multi-space trigger. Ensure the entire partition fits one physical Txn.
([audit row](/Users/vm/owner-plane-d0a-spec.md:1287),
[audit CDDL](/Users/vm/owner-plane-d0a-spec.md:2092))

### 8. Frontier retirement, bootable genesis and outcome closure

Reconcile the two retirement rules. `w.gen(last_known = head)` retires that
head and its causal ancestors, not unrelated lower-generation heads left by an
earlier `"unknown"` opening. Validate that `last_known` is accepted, terminal,
in the same zone/lineage and older than the new generation.
([Frontier](/Users/vm/owner-plane-d0a-spec.md:481),
[`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1084))

Pin the complete genesis ordinary/audit grants, not only verbs: exact ops,
class/kinds, finite budget, deadline absence, `online_lease = false`, spaces,
lineage and epochs. Reject the otherwise schema-valid witnessless genesis that
cannot write.

Before vectors, give byte-different tenant request-ID reuse one outcome and
disposition; define or remove `cert-expired`; and state validation precedence
for multi-fault inputs.

## Normative drift and accepted pins

Apply the peer's editorial sweep plus the first review's remaining pins:

- header/archive and v0.5-synthesis provenance;
- D-48/D-54 refinement wording aligned to D-56/D-57;
- `Write-capable` → `op-authoring`;
- D-65's Appendix-A claim made true;
- qualify or inline the missing “App-C #2” reference;
- state the trusted later-grant audit-read path and shared-device principal
  residual;
- close hosted-enroll target equality, fail-closed policy cross-fields and
  audit-principal variants;
- align every inline/storage CDDL mirror.

These are appropriate for the final prose↔schema discrepancy sweep, but they
do not substitute for the lifecycle and byte repairs above.

## Conformance and Gate A

Gate A remains mechanically impossible today: no `owner-plane-core`, vector
corpus or harness exists, and the family-14 offline result is explicitly open.
After v0.5.2:

1. build the core/corpus/harness;
2. add the two-replica control/proof ordering, non-cyclic hosted revocation,
   service succession, checkpoint cursor, generated-size, post-Fence crash,
   third-rotation survivor, flow-deadline, multi-space audit, Frontier branch,
   genesis and outcome vectors named above;
3. run every required surface;
4. record family 14;
5. perform the final discrepancy audit and feed any fixture-invented behavior
   back into the specification before declaring Gate A.

## Recommended v0.5.2 order

1. **Owner/protocol rulings:** canonical control position, service succession,
   and non-cyclic hosted exclusion.
2. **Finality/control:** checkpoint object, proof feeds, revocation target,
   requester freshness and continuations.
3. **Stored bytes:** Fence/Rewrap/frame schemas, bundle/checkpoint/reference
   identities and generated caps.
4. **Tenant semantics:** survivor wording, flow expiry/bundle accounting,
   audit trigger/partition.
5. **Reducer exactness:** Frontier retirement, full genesis defaults, outcomes
   and drift.
6. **Executable Gate A:** core, corpus, harness, offline result and discrepancy
   audit.

## Bottom line

The peer review improves the combined result: its multi-epoch survivor and
audit-trigger findings should definitely land, its service-history reading is
directionally right, and most of its pins are useful. Its assessment of its
own scope is too optimistic. The unresolved defects include an unconstructible
hash cycle, arrival-order divergence, missing checkpoint/storage bytes, an
impossible size claim and an authority-extending flow deadline. Those are not
stale sentences.

Cut v0.5.2 with the consolidated repairs above. Then stop prose-only review,
build the executable corpus, and let the discrepancy audit decide Gate A.
