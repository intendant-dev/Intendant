# Review: D0-A Core + Memory normative specification v0.3

*2026-07-12. Freeze-candidate audit of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.3 against
the consolidated v0.2 closure checklist in
[`owner-plane-d0a-spec-v0.2-synthesized-review.md`](/Users/vm/owner-plane-d0a-spec-v0.2-synthesized-review.md).
Three independent passes covered protocol/control, Memory/IAM, and
crypto/storage/delivery. This is deliberately a narrow prose↔protocol audit,
not another umbrella-architecture round.*

## Executive verdict

**V0.3 is a major closure improvement, but it is not ready for Gate A.**

The revision resolves most of the previous review in substance: it no longer
depends normatively on archived drafts; key and signature encodings are stated;
receipts and leases are signed in concept; certificate expiry is optional for
solo planes; control bodies exist; write grants are single-zone; generation one
is repaired; hosted-safe verbs and mobile classes exist; item and DEK wrapping
formulas are present; every exclusion rotation uses one erase/rewrap machine;
IPLOG2 includes length in its trailing CRC and quarantines complete bad-CRC
frames; Memory has explicit audit, erase, evidence, classification, and
transfer sections; outcomes have dispositions; and the decision record is
affirmative.

This is no longer an architecture-completion draft. It is a serious protocol
candidate with a finite set of byte and transition defects.

Those defects still cross security and interoperability boundaries. Examples:

- Connect is an authorized receipt issuer but cannot inhabit the required
  `issuer_cert` schema.
- A hosted self-lineage reauthorization has no device proof with which the
  reducer can enforce “own lineage.”
- The deterministic 96-bit wrapper nonce can collide between different items
  under one KEK; the required AES-GCM nonce-uniqueness invariant is not closed.
- The cross-zone completion journal promises one frame containing two record
  types, but no defined frame can represent it.
- `m.audit` simultaneously requires no grant and a nonexistent audit grant.
- `judge.full` can bypass the dedicated raise/declassify/instruction-grade
  controls.
- Appendix B promises exact policy constants and hashes but supplies prose
  summaries and no hashes.
- The block labeled normative JSON Schema is invalid JSON; parsing it fails on
  raw newlines inside quoted descriptions.

Gate A is also mechanically incomplete by definition: there is no
`owner-plane-core` crate or owner-plane vector corpus in the repository, the
offline-confirmation result remains open, and no final prose↔vector discrepancy
audit can run yet.

Recommended disposition:

- **Architecture and product direction:** accepted; do not reopen.
- **V0.3 as the basis of implementation:** yes.
- **Normative freeze:** no.
- **Next work:** one v0.4 byte/transition repair pass, then implement vectors.
- **Durable P1 Memory writes:** still prohibited.

## What v0.3 successfully closes

These changes should be preserved:

1. **Archived-draft independence and decisions.** Archived drafts are no longer
   normative dependencies, D-1 through D-38 are separated and affirmatively
   ruled, and Gate B is correctly necessary but not sufficient for P1.

2. **Core crypto shape.** Ed25519/P-256 encodings, low-S validation, HPKE suite,
   signed receipt/lease envelope concept, recovery derivation, and the
   runtime/controller plaintext-key boundary are explicit.

3. **Solo-plane honesty.** Certificate expiry can be absent; the genesis
   posture names budgets rather than pretending a single device has independent
   time evidence; policy transitions are explicit control operations.

4. **Control and writer lifecycle.** Control bodies now have CDDL; recovery
   placement and precedence are stated; generation one opens without a
   meaningless `w.gen`; write grants cover one zone; hosted lineage continuity
   is acknowledged; Frontier retirement has a proposed rule.

5. **Item/storage architecture.** Item AEAD and DEK-wrap formulas are explicit;
   differing duplicate wrappers fail closed; all rotations rewrap survivors;
   tenant logs remain ciphertext-only; `Batch` is introduced; CRC covers frame
   length; complete bad-CRC tails quarantine rather than truncate.

6. **Memory architecture.** Safe/full hosted rights, policy hashes in `polref`,
   typed judgment evidence, sensitive fallback for dangling references,
   floor-preserving classification, immediate retrieval exclusion, audit rows,
   provider policy hashes, and a cross-zone completion concept are all sound
   directions.

7. **Testing shape.** Named browser/platform surfaces, outcome dispositions,
   expanded vector families, and explicit Gate-A/Gate-B checklists are the
   right framework once the remaining protocol inputs are closed.

The findings below refine these mechanisms; none calls for discarding them.

## Freeze blocker 1: finish receipt, lease, deadline, and renewal authority

### Connect cannot sign the specified receipt object

`ReceiptStmt` requires `issuer_cert`, and T4 resolves that hash as a plane
`DeviceCertificate`. Connect instead appears in `ZonePolicy` as
`connect_service_key: bytes32`; it has no plane certificate, signer algorithm,
public key, or rotation chain. A validator therefore cannot verify a Connect
receipt using the specified bytes.

Use a tagged issuer union:

```
issuer = { kind: "device", cert: cert_hash }
       / { kind: "service", key_descriptor_or_cert: ... }
```

The service arm must bind algorithm, key ID/public key or service certificate,
rotation, and anti-replay sequence scope. Define whether a lease issuer must
meet the same witness qualification as an acceptance-receipt issuer.

`issuer_seq` also needs a generation and recovery lifecycle. An honest device
snapshot restore can reuse a sequence and trigger permanent `issuer-fork`;
certificate/service-key rotation has no defined counter rollover. Bind the
counter to an issuer key/generation and specify rollback recovery and
succession.

`ReceiptStmt` should also be a tagged union of four closed shapes. Its current
single map permits `size` on non-storage receipts and permits storage receipts
without `size`, despite the prose saying otherwise.

The alternate `witness` path is incomplete too: deadline admission accepts a
“covering” checkpoint witness, but no exact checkpoint/frontier predicate proves
that it covers the operation. Freeze the checkpoint object and coverage proof,
or remove this authority path from D0-A until that proof exists.

### Witness policy is unstable and under-bound

T2 defines witnesses as devices, while `time_witnesses` holds unexplained
32-byte values and Appendix B calls them certificate hashes. Certificate
renewal changes the hash while retaining `device_id`, so the policy either
breaks on renewal or silently has a different meaning. Store stable device IDs
for device witnesses and a distinct service-issuer reference.

Receipts carry neither `plane_id`/`zone_id` nor the policy/control frontier
under which their issuer qualified. A later policy change can therefore
reinterpret old receipt bytes. Bind the statement to its plane, zone, and
witness policy/control frontier, or state and vector-pin a deliberate
retroactive rule.

T4 says certificates “revoked for cause” retro-disqualify receipts, but
`c.revoke_device` has no cause/mode field. Either all revocations apply from an
explicit receipt cutoff, or add a closed revocation mode and exact historical
receipt semantics.

### Renewal breaks receipt-free history

An old certificate validates only operations with acceptance evidence before
renewal. Solo/budget-lane operations intentionally have no acceptance receipts,
so certificate renewal can make their historical signatures unverifiable.
Prefer the conventional distinction: an enrolled historical certificate
continues to validate already accepted bytes, while renewal disables only new
authorship after signed per-zone/per-lineage accepted-through cutoffs. A single
frontier is ambiguous for a certificate spanning zones and generations. Put an
exact cutoff/checkpoint commitment in `c.enroll`, or define an equivalent
portable transition that cannot admit a late fork.

### The solo “budget” posture does not require a budget

Genesis uses a non-expiring certificate and `deadline_fallback = "budgets"`,
but capability budgets remain optional. A conforming genesis can therefore
mint authority bounded by neither time nor operations/bytes. Require a finite
budget on every write grant in a budget-fallback zone, including genesis,
enrollment, and later grant issuance.

`require_cert_deadlines` is present in `ZonePolicy` but has no admission
semantics. Define whether it rejects new operations under deadline-free
certificates, requires only newly issued certs to carry deadlines, or triggers
another transition. Likewise, the meaning of absent `time_witnesses` must be
explicit.

### Missing evidence is not yet failure

`deadline-unreceipted` and `lease-missing` currently cause immediate
reproposal. A qualifying proof may simply arrive later because feeds reorder;
the deterministic fold has no local clock proving that it never will. These
states should be `pending-dependency` until a signed frontier/cutoff establishes
that the proof window is closed, then transition to quarantine if appropriate.

**Closure test:** every receipt/lease byte identifies a verifiable issuer and
policy state; renewal preserves receipt-free history; solo grants are genuinely
bounded; and delayed proofs converge without unnecessary new operations.

## Freeze blocker 2: close control, genesis, lineage, and epoch transitions

### Common control-header bytes are still invented by implementers

Every control operation carries the common header's `authored_kek_epoch` and
`capability_epoch`, but no control-namespace values are assigned. Pin reserved
values—probably zero—and freeze actor/signer combinations for genesis, admin,
recovery, and drill operations. Also state the initial control previous-hash
rule and request-ID requirements in the registry, not by inference.

### Genesis is described, not fully validated

`c.genesis` structurally carries arbitrary spaces, policy, and one grant. Its
validity rules do not pin:

- which entry is `home` and which is `audit`;
- `home`'s `space_class` and bound built-in policy;
- the audit space's class/policy;
- the minimum first-grant verbs, finite budget, and generation window;
- trusted versus hosted first-certificate class constraints;
- the required absence of certificate expiry in the solo posture.

This matters immediately: `workflow-v1` counts safe-human acceptance only in a
workflow space, while a natural personal `home` remains candidate-only. Freeze
the genesis-space/grant invariants and make `judge.safe` behavior agree with the
built-in policy in workflow and any intended personal diary space.

### Hosted self-reauthorization is not provably “self”

Hosted `c.lineage_reauth` is an admin-arm operation whose body contains only a
lineage and generation count. The header actor is signer-controlled and no
device co-signature names the requester. The portable reducer cannot prove
that the lineage belongs to the requesting device, as D-30 requires. Embed a
device-signed request/certificate or define a compound proof that binds the
admin ceremony to that device and lineage.

The related “trusted-lane recovery succession lifts the hosted ceiling” is not
a portable predicate either. Either define **any valid recovery-authority
succession** as the ceiling-lifting act (the phrase is the authority), or carry
trusted-client provenance evidence in the operation. Do not make replicas
infer which UI or machine produced identical signed bytes. `c.drill` can remain
a product requirement on trusted clients, but that unenforceable lane should
not masquerade as a reducer predicate.

### Capability-epoch grant validity is missing

`grant_epoch_slack` remains in CDDL but has no semantics in §9.4. An old grant
can apparently author an operation carrying the current epoch, making a
capability bump a budget reset rather than the soft-revocation mechanism the
field implies. Freeze the relationship among zone epoch, grant issuance epoch,
header epoch, strict/lenient policy, and slack. For wildcard read grants, pin a
reserved scalar value and state that epoch currency is ignored, rather than
claiming one scalar represents several zones.

### Generation state still has two authorities and an unbounded unknown path

Both `lineagedef.max_generations` and `grant.max_generations` exist, while
`c.lineage_reauth` updates only the former and §9.3 checks the latter. Define
one effective window and its accounting key. Multiple grants must not let a
writer multiply generation openings accidentally.

When `w.gen.last_known = "unknown"`, no prior generation head is causally
incorporated, so the claimed “one active head per lineage” retirement rule does
not apply. Repeated browser loss can still grow Frontier by generation until
the global cap. Specify the accepted-through/abandon behavior for unknown
recovery and whether later operations from a superseded generation are
quarantined.

Device revocation likewise needs all of a device's lineages/cutoffs, not the
single `cutoff` currently carried by `c.revoke_device`, or a previously authored
offline chain can survive the ceremony.

**Closure test:** every control header is unique; a valid genesis always passes
the boot vector; hosted continuity is self-scoped from portable evidence; and
renewal, grants, epochs, generations, and revocation cannot retain or reset
authority through an unspecified path.

## Freeze blocker 3: repair the remaining cryptographic commitments

### Deterministic wrapper nonces still have a collision problem

`wrap_nonce = HKDF(..., item_addr, L=12)` compresses arbitrary item addresses
to 96 bits. Two different items therefore have an unbounded probabilistic
chance of obtaining the same nonce under one KEK; the current duplicate check
covers only the same `(item_addr, epoch)`. The protocol has no per-epoch item
bound or collision response, yet AES-GCM requires nonce uniqueness per key.

The clean repair is to derive a domain-separated 256-bit **per-item wrapping
key** from `(KEK, item_addr)`, use that key for exactly one encryption, and use
a fixed nonce—retaining deterministic idempotence without sharing one GCM key
across compressed nonces. Alternatively freeze a strict invocation bound,
collision detection, and mandatory rotation response; that is more
operationally fragile for a forever store.

### Rewrap completion does not yet prove current wrappers

`survivors_digest` is an undefined “H_frontier-style” hash over item addresses.
It has no dedicated domain/schema, snapshot frontier, rotation identity, or
binding to the actual new wrapper bytes. An address list proves which items
survive, not that each has a valid current-epoch wrapper.

Define an exact `survivor_set` object under its own domain, frozen at the fence
frontier, containing or committing to sorted `(item_addr, current_wrapper_hash)`
pairs plus rotation op/epoch. Verify each wrapper before recording completion
and destroying the old KEK.

`OldKekDestroyed` itself also needs durable semantics. There is no record or
keystore acknowledgement defining when deletion survives power loss, how a
failed deletion retries, or how recovery distinguishes delete-before-
tombstone. Freeze the durable deletion contract—including removal of any local
backup envelopes—before treating the erase ceremony as complete.

### Recipient wraps are not key- or context-identifying

`KekWrap` names only stable `recipient_device`, not the certificate/KEM key that
can decrypt it. Renewal therefore leaves historical packages ambiguous.
Include `recipient_cert` or KEM `key_id`.

`c.enroll.wraps[]` also lacks zone and epoch association, so multiple granted
zones cannot be decoded unambiguously. Put `{zone_id, epoch, wrap}` around every
enrollment wrap. Require `new_epoch = current + 1`, a fresh KEK, exact HPKE
external AAD, and exact ciphertext lengths. AES-GCM wrapping 32 bytes with a
16-byte tag yields 48-byte ciphertext; CDDL currently accepts arbitrary sizes.

### Root custody remains a contract without a format

D-38 says passkey-sealed with mandatory recovery envelopes, but v0.3 still has
no envelope version/schema, PRF/KDF/AEAD construction, domain strings, or
relationship between the same-root recovery envelope and the independent
recovery-authority phrase. Either import one exact existing vault-envelope
version normatively or define the object here; otherwise explicitly assign it
to Gate B and narrow Gate A's self-contained/wrapped-object claim. In the
current text, which says D0-A freezes custody and every wrapped/stored shape,
the omission is a Gate-A defect.

Also close the remaining hash inputs: `evidence_hash` lacks an exact `evrec`
object; `locator_hash` uses raw SHA-256 over an undefined “canonical locator
string”; the recovery HKDF info `ed25519-seed` is absent from the supposedly
closed context inventory; and survivor hashing needs its own domain.

**Closure test:** no conforming history can reuse a GCM key/nonce pair; every
wrap names one recipient key and context; old-key destruction is justified by
a wrapper-set commitment; and every custody/hash object has one canonical
input.

## Freeze blocker 4: make physical transactions encodable and exact

### The cross-zone completion transaction cannot be represented

Section 6.1 says the source commits `m.export.release + PendingXfer` in one
frame. `Batch` contains only `ItemCommit` payloads, while `PendingXfer` is a
separate frame type. No defined frame can contain both. Clearing with
`OutboxMark` also cannot identify an `export_id`.

Add a dedicated `ExportBegin { item_commit, pending }` transaction frame or let
`Batch` contain a closed union of transactional subrecords, plus
`XferDone { export_id }`. Then specify idempotent recovery ordering.

### Batch is physically atomic but semantically open

Freeze item ordering, same-zone/writer requirements, contiguous sequence and
previous-hash linkage, exact two-operation assert shape, all-or-nothing
validation behavior, and the outbox advancement. The prose claims outbox state
rides `ItemCommit`/`Batch`, but neither payload contains it while a separate
`OutboxMark` type still exists.

E8 allows up to 16 tenant items, each individually capped at 256 KiB, while the
enclosing frame is capped at 1 MiB. The frame cap can correctly be the aggregate
constraint, but state explicitly that a batch is valid only when its complete
encoded payload also fits that cap.

### Frame and recovery bytes need a final pin

The SYNC declaration mixes byte and integer order:

```
u32-le = 0x52 4C 50 49 ("IPLR")
```

ASCII `IPLR` is bytes `49 50 4c 52`, represented as little-endian integer
`0x524c5049`. State both forms unambiguously. Require payload to be canonical
CBOR of the frame type's CDDL, define corrupt header/bad-SYNC behavior, and
durably flush a tail truncation before reopening for append.

Including `len` in a **trailing** CRC does not fully protect framing: if a
committed final frame's length is corrupted upward, the scanner reaches EOF
before it can locate/check that CRC and may misclassify committed corruption as
an incomplete torn tail. Add an immediately checkable header checksum,
complement/redundant length, or equivalent evidence, and vector-test corrupted
final lengths.

Platform adapter questions—`F_FULLFSYNC`, Windows ACL enforcement, persistent
Linux key storage, IndexedDB eviction—belong to Gate B, but the abstract power-
loss/durable-key contract must be honest. In particular, a persistent Linux
keyring alone is not durable secret storage across every reboot/configuration.

**Closure test:** every promised logical transaction has one encodable frame
sequence; replay after every crash point is deterministic; and no combination
of legal item/batch/frame bounds exceeds its enclosing cap.

## Freeze blocker 5: make Memory authority rows mutually exclusive and usable

### Safe hosted operations and policies disagree

`judge.safe` admits accept/retire/dispute for observations and episodes, while
the prose `workflow-v1` constant counts safe-human acceptance only in workflow
spaces and does not count safe-human retirement. Genesis does not pin `home` to
a compatible class/policy. Align the operation invariants, exact built-in
table, and genesis spaces so every advertised safe transition actually counts.

`pin.safe` and `erase.request` also lack the direct-human/owner invariant that
`judge.safe` has. A supervised agent can exercise them whenever its device
grant contains those verbs. Require portable human evidence. Add a safe unpin
path: hosted devices can create `pin.safe` but can never use `pin.full`, so the
current registry prevents them from self-unpinning (a later trusted owner with
`pin.full` could still remove the pin).

Constrain the safe pin's destination and `role`; arbitrary role text can
reintroduce instruction-grade auto-context even when the target kind is an
observation.

### Generic judgment authority bypasses dedicated controls

`judge.full` covers “all verdicts,” overlapping the dedicated `raise` and
`declassify` rows. Read literally, it bypasses `can_raise`, raise quotas, and
`can_declassify`. Exclude those verdicts from generic judgment rights.

Likewise, `curate.instruction` is never consumed: an owner with `judge.full`
can accept a procedure/preference without it. Require the instruction-grade
verb for instruction-grade acceptance/pinning/graduation. Give author
supersede and assert-only author retract exact named verbs rather than prose
exceptions.

### Compound assert is still not a portable registry entry

An assert produces two separately signed operations (`m.claim` and
self-accepting `m.judge`), but the registry describes them as one row while
portable admission accepts one operation at a time. Define a row for each half,
their deterministic linkage/request IDs, the grant verb each cites, and replica
behavior when only one half has arrived. Local `Batch` atomicity does not make
remote admission atomic.

### Audit has no coherent authorization lane

The `m.audit` row says “system; no grant” and “under a genesis-issued audit
grant” simultaneously. No `audit.write` verb exists and `c.genesis` contains
only the ordinary first grant. Add a system-only verb/grant or a separate
portable proof arm and define its budget/lineage.

Also define browser-only audit writing, whether the protected read fails if its
audit commit fails, and an exclusion preventing reads of audit data from
recursively generating audits. The current daemon-only service writer does not
cover the zero-daemon hosted lane.

**Closure test:** every Memory body variant selects exactly one authority row;
hosted humans can accept, retire, pin/unpin, and erase only within their safe
ceiling; agents cannot borrow those human rights; and audit writes can actually
pass portable admission on every supported lane.

## Freeze blocker 6: finish Memory reducers, policies, evidence, and transfer

### Classification selection is still ambiguous

Taking the maximum over every qualifying declassification means an older
`sensitive→private` judgment can permanently defeat a later causally descending
`private→internal` judgment. First select **causally maximal** qualifying
declassifications; only then take the higher class among concurrent maxima,
and finally apply the immutable floor.

The formula says “VERIFIED evidence floors,” while §11.5 says an unresolved
reference contributes `sensitive`. Define one `evidence_effective_floor` that
either verifies the source claim's classification at a named frontier or
returns `sensitive`; never trust only the claimant-recorded floor.

Projection eligibility still omits `valid_from_ms`. A future-valid claim must
remain absent until `as_of_ms >= valid_from_ms`.

Supersession revival also needs temporal semantics. The status rule checks only
whether the replacement's status is `accepted`, while expiry/future-validity
are separate view filters. An expired or not-yet-valid replacement can
therefore keep its predecessor superseded while neither is retrievable,
contrary to D-21's revival intent. Define status/view eligibility together at
`as_of_ms`, or state exactly when temporal ineligibility revives the
predecessor.

Likewise, §11.7's normative search/read/auto-context predicate never consumes
the immediate retrieval-exclusion flag created by `m.erase_request`. Add that
flag explicitly and define its interaction with pins, supersession, and audit;
otherwise projections can disagree while both follow the written status fold.

### Built-in policy constants are not constants yet

Appendix B.2/B.3 are English summaries, not literal canonical `policy` objects,
and no pinned hashes are supplied. The policy CDDL leaves `verdict` as arbitrary
text. Publish exact closed tables, canonical bytes, and actual hashes. Reconcile
the statement that safe-human disputes count in “both built-ins” with
`owner-v1`, which currently permits owner only.

### Evidence locator and verification remain incomplete

Define URL/file/session locator canonicalization and hash it under the declared
evidence domain. For resolvable plane evidence, recompute or prove the source's
effective floor at a named frontier; a recorded `class_floor` alone is not
verification.

### Export/import is still not an exact portable pair

The released bundle is prose, not CDDL, and “redacted source record” /
`provenance_summary` have no deterministic construction. One release may
contain 128 records while one import creates one claim; mapping, identity,
redaction, and replay semantics are unspecified. An imported statement is not
cryptographically tied to a particular released record merely by citing the
bundle digest.

The interim cross-plane mediating-service attestation has no signed field,
issuer, or verifier in `mimport`. The clean v1 answer is to fail closed on
cross-plane import until D0-B defines foreign proofs; same-plane cross-zone is
already implementable. If interim mediation is retained, specify its signed
portable proof.

Release expiry also needs a deterministic witness/as-of rule; “imports after
it quarantine” cannot read validator wall-clock time. Key replay on
`(from_plane, export_id)`, not bare `export_id`, and freeze equality among the
duplicated import provenance fields.

**Closure test:** exact policy objects hash identically cross-language, and two
replicas derive the same status/classification/evidence/import result from the
same bytes and explicit frontiers/`as_of_ms`.

## Freeze blocker 7: make the conformance artifacts executable

### The normative JSON Schema is not valid JSON

The quoted `description` values contain raw line breaks. Extracting the fenced
block and parsing it as JSON fails. Replace it with a valid Draft 2020-12
schema. Close `additionalProperties`, family/outcome/disposition enums,
expected-field types, and family-specific input shapes rather than leaving
`inputs` arbitrary.

`chacha20` is not an exact deterministic RNG definition. Pin the variant,
nonce, initial counter, byte ordering, and draw/consumption rules. Fixed
WebCrypto verification vectors are the right solution where signing randomness
cannot be injected.

### The surface list is not yet a family×surface matrix

State exactly which families run in shared Rust/WASM, native Rust adapters,
Chromium, each native OS storage lane, and manual Firefox/Safari acceptance.
Pure format/crash-model vectors belong to Gate A; production flush/keystore
adapter validation remains Gate B.

### Pinned values and executable corpus are absent

Publish the built-in policy hashes and any parameterized genesis-policy
derivation. Then create the core crate, vector files, and harness; record the
offline-confirmation result; run every named lane; and conduct the final
prose↔vector audit. Until that work exists, “freeze candidate” means the prose
is a candidate for implementation, not that Gate A is nearly a checkbox.

**Closure test:** the schema parses, every vector has one deterministic meaning
and named surface, all required lanes are green, and no fixture invents
behavior absent from the normative text.

## Secondary pins before vectors

These are smaller than the blockers above but should be fixed before fixtures
make them accidental protocol:

- clarify that direct human actions use browser/native/mobile credentials;
  §10.1 currently includes a human “on daemon” while O4 excludes daemon class;
- word `mobile-attested` honestly as root/owner-attested unless validators
  actually parse and verify a closed platform-attestation document;
- exempt derived request IDs from the reserved-ID prefix rule or specify a
  deterministic retry if `assert_req` lands in the reserved range;
- define all set-array sort/dedup rules for grants, flows, wraps, cutoffs,
  evidence, and control compounds;
- make `ReceiptStmt` subject/size constraints and control-body conditional
  fields part of strict validation, not merely comments;
- specify frame-header corruption and file-header binding/checksum behavior;
- define the exact disposition of an old-generation operation arriving after a
  successor generation has been accepted;
- distinguish `storage-io` (writer stopped, existing store readable) from
  corruption requiring rebuild; the current disposition table conflates them.

## Recommended v0.4 sequence

The shortest path to a real Gate-A run is:

1. **Repair authority bytes first:** tagged receipt/service issuers, witness
   policy binding, renewal/cutoff history, finite solo budgets, control header
   constants, hosted requester proof, and capability/grant epoch semantics.
2. **Repair crypto/storage commitments:** per-item wrap keys or another
   nonce-safe construction, exact wrapper-set snapshot, recipient key/context,
   encodable Batch/export transactions, and final frame bytes.
3. **Make Memory rows disjoint:** safe-human constraints and unpin, dedicated
   raise/declassify/instruction verbs, two assert rows, audit authority.
4. **Finish deterministic folds:** genesis/policies, declass selection,
   evidence verification, `valid_from`, and an exact same-plane transfer pair;
   defer cross-plane proof cleanly if needed.
5. **Publish the remaining constants and valid vector schema.** Only then
   implement the corpus and shared core, because doing so earlier forces
   fixtures to choose unresolved behavior.
6. **Run the final discrepancy audit after all vectors are green.** That round
   should compare prose to executable behavior and contain no new architecture.

## Proposed Gate-A go-ahead

I would give the go-ahead only when:

- every device and Connect receipt/lease verifies from a closed issuer and
  policy state;
- solo, renewal, hosted continuity, trusted re-root, multi-device, and
  revocation histories all converge without ambient lane facts;
- every GCM invocation has a protocol-enforced unique key/nonce pair;
- survivor completion commits to verified new wrappers at a frozen snapshot;
- every logical storage transaction is physically representable and
  crash-vector-pinned;
- every Memory variant has one non-bypassable authority row and every fold is
  exact;
- root custody, evidence, bundles, policies, and all hash inputs have closed
  schemas and domains;
- the vector schema is valid, constants/hashes are published, the core/corpus
  exists, all named lanes are green, and the offline confirmation is recorded;
- the final prose↔vector audit finds only editorial drift.

## Bottom line

V0.3 is the strongest D0-A draft so far and a successful response to the
combined v0.2 review. Its remaining issues are concentrated, testable, and
repairable. They are nevertheless exactly the issues Gate A exists to catch:
authority that cannot be verified from bytes, crypto commitments that do not
prove what their names claim, transactions that cannot be encoded, and reducer
branches whose policy or capability rows overlap.

Use v0.3 as the implementation basis, cut one focused v0.4 repair, and then let
the vector corpus decide the freeze. Do not authorize durable P1 Memory writes
yet.
