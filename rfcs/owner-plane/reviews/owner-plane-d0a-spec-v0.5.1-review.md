# Review: D0-A Core + Memory normative specification v0.5.1

*2026-07-12. Reviewed against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.1,
the archived
[`v0.5 text`](/Users/vm/agenda-rfc-archive/2026-07-12-d0a-v0.5-as-reviewed.md),
and the
[`v0.5 synthesized review`](/Users/vm/owner-plane-d0a-spec-v0.5-synthesized-review.md).
This is a protocol/reducer and prose↔schema discrepancy audit, not an
implementation review.*

## Verdict

**Do not declare Gate A. Cut a focused v0.5.2 exactness patch before
freezing operation schemas or canonical vectors.**

V0.5.1 is a substantial improvement. It resolves most of the prior review's
product choices and many of its schema gaps. In particular, the deadline
postures now have a coherent meaning, verb classes and actor identity are
portable, Frontier is zone-scoped, writer arithmetic is exact, audit and
transfer have much better state machines, and erase now commits the target
mapping and expected survivor membership.

The remaining problems are not a reason to reopen the architecture. They are,
however, protocol-level:

- the chosen receipt-policy rule still has no canonical cross-log position;
- hosted revocation contains a cryptographic hash cycle and cannot be
  constructed;
- ordinary Connect service-key rotation is rejected by the new epoch rule;
- checkpoint/finality state is still not a closed portable object;
- the published wrap/erase caps cannot fit their enclosing operation;
- several claimed closed objects and durable frame shapes remain absent or
  contradictory;
- a release can choose a deadline later than its authorizing flow;
- audit chunks do not yet prove exact coverage of a read;
- Frontier retirement and the genesis defaults still admit two meanings.

The right disposition is therefore:

- **Architecture and D-56's deadline ruling:** accept.
- **Low-level CBOR and crypto scaffolding:** may proceed.
- **SignedOperation/control-body schema freeze:** not yet.
- **Canonical corpus/reducer freeze:** not yet.
- **Durable P1 writes:** still prohibited by the document's own gates.

## Closure against the v0.5 synthesis

| Prior closure area | V0.5.1 disposition |
|---|---|
| Deadline posture | **Resolved.** Present deadlines always bind; budgets is the finite deadline-free lane; fail-closed governs new issuance; witnessless behavior is explicit. |
| Proof-policy history | **Open.** “Citing operation's admission position” has no portable position in the bytes or a deterministic cross-log merge rule. |
| Hosted self-service | **Mostly resolved.** Requester certificate/signature and consumed request IDs exist; freshness is still unchecked. |
| Hosted revocation | **Regressed.** The new compound-first rotation rule is hash-cyclic. |
| Service-key lifecycle | **Partial.** Descriptor intervals and compromise intent exist, but normal rotation and cutoff mechanics do not close. |
| Frontier/writer arithmetic | **Mostly resolved.** Zone and sequence/generation arithmetic are exact; retirement rules conflict. |
| Genesis | **Partial.** Most cross-fields are pinned; deadline/lease/default-verb bootability is not. |
| Memory identity | **Resolved.** Op-authoring versus claim-authoring and `actor.id` are now explicit. |
| Audit | **Mostly resolved.** Trigger, principal, time, failure and remedy are present; chunk/scope completeness is not. |
| Transfer | **Mostly resolved.** Snapshot, record identity, endpoint, Done/Abort and replay are present; flow expiry, bundle schema and byte accounting remain open. |
| Erase | **Semantically resolved, durably partial.** Target and survivor membership are exact; Fence/Rewrap storage and size limits are not. |
| Conformance | **Improved but not achieved.** Keyed sets, RNG byte counts and several outcomes are fixed; the core/corpus/harness do not exist and the offline result is open. |

## Gate-A blockers

### 1. D-57 still has no canonical policy position

T2 now qualifies a receipt or lease against the witness policy and service
descriptor at the citing operation's eventual “admission position.” Neither
the operation header, its device authorization proof, nor the receipt carries
an authoritative control position; the lease's frontier remains diagnostic.
([T2](/Users/vm/owner-plane-d0a-spec.md:541),
[operation header](/Users/vm/owner-plane-d0a-spec.md:384),
[proof/lease shapes](/Users/vm/owner-plane-d0a-spec.md:370))

That produces a simple divergent replay:

1. Replica A receives witness receipt R and admits operation O under policy P.
2. Replica B receives `c.zone_policy` or `c.service_key` P′ before R.
3. B later receives R and evaluates the still-pending O under P′.
4. The text says an already admitted operation is never re-litigated.

Both replicas have the same final signed objects but can retain different
states. “Admission position” is a local event, not a canonical position, until
the specification supplies one.

There is a second contradiction: T2 says admitted operations are never
re-litigated, while T4 and the new compromise cutoffs intentionally
retro-disqualify proofs. State exactly which later control events revisit an
admission and what its resulting outcome becomes.

**Required repair:** either add a signed authoritative control/policy reference
with a freshness rule, or define one deterministic cross-feed ordering and
full replay rule. A local arrival order or diagnostic frontier is insufficient.
Vector the two-replica ordering above and a later compromise cutoff.

### 2. Checkpoint finality is not yet a closed machine

The added zone scope and lease coverage are good, but the remaining fields do
not identify a portable fence:

- `c.checkpoint.checkpoint` is an opaque `bytes32`; no versioned Checkpoint
  object, hash domain or retained-head schema defines it;
- `gc_fence: uint` has no ordering domain or consuming rule;
- `covers` is an accepted Frontier hash, but the text never states how a
  still-unadmitted operation occupies a position covered by that Frontier;
- `proof_cutoffs = {key_id, through}` is not a unique issuer scope. The same
  public key bytes may be used by a device and a service without a hash
  collision, while their sequence scopes are different;
- §10.5 says every pending dependency hardens at a GC fence, but the
  checkpoint rule only closes deadline and lease proof feeds—not
  `causal-missing`, `cert-superseded`, or `policy-missing`.

([checkpoint registry](/Users/vm/owner-plane-d0a-spec.md:888),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2007),
[dispositions](/Users/vm/owner-plane-d0a-spec.md:1217))

**Required repair:** define a versioned checkpoint object, its hash, its zone
and retained heads, the exact pending-operation cursor domain, and tagged
device/service proof-feed cutoffs. Narrow the generic hardening sentence to
the feeds actually fenced, or add cutoffs for the others.

### 3. Hosted revocation is cryptographically cyclic

The hosted ceiling permits a `c.kek_rotate` only when an already accepted
`c.revoke_device` names that rotation's operation hash. But the rotation is a
later descendant on the single control chain, so its signed header depends on
the revoke operation's hash. The revoke body simultaneously depends on the
rotation hash.
([hosted rule](/Users/vm/owner-plane-d0a-spec.md:973),
[control chaining](/Users/vm/owner-plane-d0a-spec.md:384))

```text
revoke_hash   = H(... rotation_hash ...)
rotation_hash = H(... previous control history containing revoke_hash ...)
```

There is no constructible fixed point. The hosted revocation vector cannot be
written.

The exception also needs an authority-preserving wrap rule: a hosted
exclusion rotation must not add a recipient that did not already hold current
zone access, and every recipient key must resolve to the intended live
certificate. Merely requiring an empty erase manifest and excluding the
target does not prevent a nominal “exclusion” from wrapping the new KEK to an
attacker.

**Required repair:** use a non-cyclic ceremony—for example a signed revocation
intent keyed by `revocation_id`, constrained exclusion rotations that cite the
intent, then a completion op—or permit tightly constrained rotation-first and
reference it from the later revoke operation. Pin the exact eligible-recipient
set and block new target grants/wraps while the ceremony is pending.

### 4. Service-key rotation and compromise do not close

`c.service_key.valid_from_admin_epoch` must equal the current admin epoch, and
a second descriptor for `(service, epoch)` rejects. Because `c.service_key`
does not advance the admin epoch, an ordinary Connect rotation is impossible
without also replacing the plane admin key. That contradicts the row's stated
rotation and compromise remedy.
([registry](/Users/vm/owner-plane-d0a-spec.md:881),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:1925))

The cutoff mechanism also needs closure:

- receipts and leases share one sequence domain, but the prose cuts off only
  “receipts”;
- the scalar targets only the immediate predecessor, so a newly discovered
  compromise of an older historical key is unrepresentable;
- `issuer_seq` is merely strictly monotone. A compromised signer can backfill
  an unused sequence at or below the chosen cutoff unless the feed is exact
  `+1` or hash-chained;
- later compromise must have an explicit effect on operations already admitted
  with the disqualified proof.

**Required repair:** give each service a consecutive service-key generation or
explicit predecessor/target key ID, allow multiple rotations inside one admin
epoch, apply compromise cutoffs to both proof kinds, and make issuer feeds
consecutive or hash-linked. State the accepted-operation rollback/quarantine
transition.

### 5. Revocation and requester exactness remain incomplete

The new requester bytes are a real improvement, but their signed
`ctrl_frontier` is not required to resolve, be an ancestor, or match a current
window. A hosted service can stockpile multiple same-state reauthorization
requests and submit them after intervening generation consumption. Consumed
request IDs stop replay of one request; they do not stop delayed first use of
several stale requests.

Other remaining gaps:

- `crevokedev` does not carry `device_id` or a certificate reference. Its
  `revocation_id` is not explicitly bound to a unique target or given renewal
  semantics, yet the reducer must derive “all target grants/wraps/lineages”;
- `c.revoke_zones` continues rotation references but not the per-zone lineage
  cutoffs that also overflow the main operation;
- generic `zonecutoff` duplicates lineage outside and inside `Head`; equality
  and zone membership are only stated in the revoke row, not for standalone
  cutoff, renewal or recovery;
- mutation of the target's grants/wraps while a compound revocation is pending
  lacks a deterministic block or completion-time repair rule.

([requester bodies](/Users/vm/owner-plane-d0a-spec.md:1983),
[revocation bodies](/Users/vm/owner-plane-d0a-spec.md:1961),
[`zonecutoff`](/Users/vm/owner-plane-d0a-spec.md:1997))

**Required repair:** bind requester state to a consumed current window/control
position; identify the revocation target explicitly; make cutoff invariants
global; and continue both rotations and cutoffs under one bounded ceremony.

### 6. The published control-operation caps are impossible

E8 says 256 KEK wraps are approximately 38 KiB and that 256 wraps plus
192 erase entries fit the 64-KiB control-operation cap. The normative
text-key CDDL makes a minimal `kekwrap` about 306 canonical-CBOR bytes and an
erase entry about 132 bytes. Therefore:

- 256 wraps alone are about 78 KiB before the control envelope;
- 192 erase entries add about 25 KiB;
- the combined body is over 103 KiB before its header and signature.

([E8](/Users/vm/owner-plane-d0a-spec.md:97),
[`kekwrap`](/Users/vm/owner-plane-d0a-spec.md:1930),
[`erasemref`](/Users/vm/owner-plane-d0a-spec.md:1988))

This is not merely a pessimistic combination of independent maxima: the wrap
maximum alone exceeds the enclosing maximum. It also invalidates the claimed
large-device rotation/revocation lane.

**Required repair:** derive caps from the exact canonical encoder including
the complete SignedOperation envelope. Either publish conservative joint
limits, increase the whole-object limit, or define a batch ceremony whose
partial operations cannot create authority gaps. Make the generated
worst-case-size fixtures normative.

### 7. Claimed closed schemas and the durable erase fence still disagree

Several byte identities remain obvious in intent but absent from the normative
schema:

- `AuthorizationProof.dev.{cert,cap}`, `cert.renews`, receipt issuers and
  `actor.attested_by` never explicitly say `H_cert(certificate)` and
  `H_grant(grant)`, nor does the proof pin `signer_key_id` to the referenced
  certificate key;
- the hashed `bundle` and `bundlerec` are absent from Appendix A even though
  D-65 says they are present;
- the checkpoint object is absent, as above;
- §6.2 promises a registered CDDL payload for each frame, but exact wrapper
  shapes for ItemRewrap, Fence, Receipt-or-Lease and CtrlOp are absent;
- the survivor preimage remains a bare array rather than a versioned object,
  and its `item_addr` logical key is missing from E7's declared list.

([authorization proof](/Users/vm/owner-plane-d0a-spec.md:370),
[bundle prose](/Users/vm/owner-plane-d0a-spec.md:1445),
[Appendix A](/Users/vm/owner-plane-d0a-spec.md:1819),
[frame contract](/Users/vm/owner-plane-d0a-spec.md:815))

The durable erase machine has a more immediate contradiction. Survivor
membership freezes the Frontier at the Fence, but frame `0x13` persists only
`kek_epoch`. A crash before RewrapDone loses the exact fence needed to derive
the expected set. Inline `0x14 RewrapDone` then omits `fence_frontier`, while
the state machine and Appendix A include it.
([state machine](/Users/vm/owner-plane-d0a-spec.md:678),
[frame table](/Users/vm/owner-plane-d0a-spec.md:737),
[RewrapDone CDDL](/Users/vm/owner-plane-d0a-spec.md:2026))

**Required repair:** persist `{rotation_op, kek_epoch, fence_frontier}` in the
Fence itself; make RewrapDone identical in prose and CDDL; add every omitted
versioned/hash/frame shape to Appendix A; and pin all reference hashes and
signer cross-fields.

### 8. Transfer still has one authority-extension path

The flow and release each carry a mandatory `expiry_deadline_ms`, but the
release invariant never requires its chosen deadline to equal or precede the
matching flow's deadline. The later receipt rule enforces only the release
value, so a writer can extend the authority granted by the flow.
([flow](/Users/vm/owner-plane-d0a-spec.md:1865),
[release](/Users/vm/owner-plane-d0a-spec.md:2073),
[release invariant](/Users/vm/owner-plane-d0a-spec.md:1469))

Also close:

- which matching flow authorizes a release when several match;
- whether bundle bytes count toward `max_bytes`: §4.3 says only the signed
  operation triple, while the registry charges bundle bytes;
- the bundle's own maximum and persistence/recovery contract (128 statements
  can exceed 1 MiB, so the bundle cannot silently inherit the frame limit);
- `header.plane_id == release.to.plane_id`, alongside the stated zone/space
  equality;
- the registry transition: plane endpoints create PendingXfer, egress
  endpoints do not.

([budget rule](/Users/vm/owner-plane-d0a-spec.md:350),
[registry](/Users/vm/owner-plane-d0a-spec.md:1285),
[storage exception](/Users/vm/owner-plane-d0a-spec.md:785))

The exact bundle identity, record floors, source eligibility, snapshot,
record-level replay, XferDone and XferAbort repairs should otherwise be kept.

### 9. Audit chunking does not prove the read it claims to audit

The new audit body has the right ingredients, but no invariants say:

- whether chunk indexes are zero- or one-based, `index < count`, and exactly
  one row exists for every index;
- all chunks for one `read_id` share principal and scope;
- result IDs are disjoint and their union equals exactly the released result
  set;
- `read_id` is fresh/unique in a portable scope.

The trigger permits a multi-space query, while `maudit.scope` names only one
space. The claimed “4096 IDs = at most 16 rows” bound therefore does not close
a query spanning many spaces, especially one with few or zero results in each.
([audit registry](/Users/vm/owner-plane-d0a-spec.md:1287),
[audit CDDL](/Users/vm/owner-plane-d0a-spec.md:2092))

**Required repair:** either restrict each audited request to one exact
`(zone,space)` or make scope a bounded canonical set. Define a unique read ID,
complete chunk partition, exact result union and one-Txn validator. Also state
that controller-assigned session IDs are fresh within their portable
principal scope; “stable” alone does not guarantee the cross-session denial
vector.

### 10. Frontier retirement and genesis still admit two reducers

Section 4.6 retires a prior-generation head only when causally incorporated.
Section 9.3 says one `w.gen(last_known = head)` retires every lower-generation
head. After an earlier `last_known = "unknown"` opening, the other lower heads
need not be ancestors of the named head. One reducer retains them; another
drops them.
([Frontier retirement](/Users/vm/owner-plane-d0a-spec.md:481),
[`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1084))

`w.gen.last_known` also needs validation that the head is accepted, terminal,
in the same zone/lineage and older than `g`. Retire the named head and its
causal ancestors only; leave unknown branches for the now-requester-attested
cutoff.

Genesis gained most requested equalities, but the supposedly valid object can
still be unable to perform the bootability walkthrough: validation does not
require the first certificate/grant deadlines to be absent, the ordinary grant
to set `online_lease = false`, or exact adequate ops/class/kinds. “Trusted:
full verbs” also conflicts with reserved `admin` rejecting at issuance and is
ambiguous about system-only `audit.write` despite the separate audit grant.
([genesis row](/Users/vm/owner-plane-d0a-spec.md:871),
[bootability](/Users/vm/owner-plane-d0a-spec.md:900),
[verb rules](/Users/vm/owner-plane-d0a-spec.md:1241))

**Required repair:** reconcile retirement and pin the exact ordinary/audit
genesis grant fields, including ops, class/kinds, deadline absence and
`online_lease = false`.

## Pre-corpus exactness and drift

Resolve these in the same patch so fixtures do not choose behavior:

- Tenant replay says byte-different `request_id` reuse is
  “`duplicate`→fork evidence,” while `duplicate` means byte-identical
  idempotence and `fork` freezes a writer. Give tenant request collision one
  outcome and disposition.
- `cert-expired` remains in the outcome/disposition tables, but §9.1 emits
  `deadline-unreceipted` for missing certificate/grant deadline proof and the
  fold reads no wall clock. Define a producer or delete it.
- State validation precedence for inputs with several simultaneous failures;
  E10 otherwise does not guarantee one portable outcome.
- D-48 still says receipt-admission policy while D-57 says citing-operation
  admission; D-54's old deadline wording conflicts with D-56.
- D-65 falsely says bundle CDDL is in Appendix A.
- Family 14 and §15 cite “App-C #2,” but this self-contained document has no
  Appendix C; qualify the umbrella reference or inline it.
- The opening archive glob omits the existing v0.5 archive and names the v0.4
  synthesis rather than the v0.5 synthesis folded by this revision.

([tenant replay](/Users/vm/owner-plane-d0a-spec.md:1266),
[outcomes](/Users/vm/owner-plane-d0a-spec.md:1196),
[decision record](/Users/vm/owner-plane-d0a-spec.md:1752),
[family 14](/Users/vm/owner-plane-d0a-spec.md:1687))

## Conformance status

Even after the prose patch, Gate A is an executable gate. There is currently
no `owner-plane-core`, vector corpus or harness in the repository, and the
offline family-14 result is explicitly open. The document therefore cannot
yet satisfy its own checklist.
([open result](/Users/vm/owner-plane-d0a-spec.md:1782),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:1787))

Add vectors for the failures above, especially:

- receipt-versus-policy arrival order on two replicas;
- service rotation twice within one admin epoch and old-key compromise;
- the hosted revocation hash construction and exact recipient set;
- pending proof at a checkpoint plus device/service same-key scopes;
- generated worst-case control-operation sizes;
- crash immediately after Fence followed by post-fence writes;
- release deadline later than its matching flow;
- multi-space audited reads and incomplete chunk sets;
- unknown-head versus causally incorporated Frontier retirement;
- every invalid but schema-conforming genesis default;
- tenant request-ID collision and all edge/fold dual dispositions.

## Recommended v0.5.2 order

1. **Ordering/finality:** portable control position, proof replay,
   checkpoint object/feed scopes and issuer sequencing.
2. **Hosted/service lifecycle:** non-cyclic exclusion, exact recipients,
   service-key generations/cutoffs, revocation target/continuations and
   requester freshness.
3. **Byte closure:** regenerate caps; close bundle/checkpoint/reference/frame
   schemas; persist the Fence frontier.
4. **Tenant authority:** bind release to flow expiry and finish audit chunk/
   scope completeness.
5. **Reducer exactness:** reconcile Frontier retirement, freeze bootable
   genesis defaults, close replay/outcome drift.
6. **Executable gate:** build the core/corpus/harness, run all named surfaces,
   record family 14, then perform the final discrepancy audit.

## Bottom line

V0.5.1 successfully closes most of the prior review and should be retained as
the foundation. Its best new decision is D-56: time proof is never made
advisory, while the solo lane remains finitely bounded. The verb, actor,
transfer-terminal and erase-membership repairs are also strong.

It is not yet a freeze candidate because several new details are impossible or
non-canonical at the byte level. Fix the cross-log policy position, hash-cyclic
hosted revocation, service rotation, checkpoint identity, cap arithmetic,
durable Fence, release deadline and audit partition in v0.5.2. Then stop prose
review and let the executable corpus decide Gate A.
