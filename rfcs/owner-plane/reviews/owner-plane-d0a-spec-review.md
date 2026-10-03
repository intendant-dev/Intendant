# Review: D0-A Core + Memory normative specification (v0.1)

*2026-07-11. Freeze-readiness review of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) against the
current v3.1 umbrella RFC and its accepted review record. This review is scoped
to D0-A; D0-B is mentioned only where the proposed D0-A bytes accidentally
depend on a later decision.*

## Verdict

**Do not freeze v0.1.** It is a strong normative skeleton and it resolves the
umbrella's remaining architectural choices unusually well. It is not yet a
self-consistent byte-level protocol, portable storage contract, or executable
Memory reducer.

The distinction matters. No architectural rewrite is needed. The next pass
should complete exact schemas and state machines before golden vectors are
written; otherwise the vectors will fossilize contradictions. The highest-risk
problems are:

1. Tenant logs persist plaintext `SignedOperation` records beside encrypted
   items, defeating DEK erasure.
2. The IAM intersection requires a daemon grant and token even for a
   zero-daemon browser, denying the flagship access shape.
3. Any plane device can backdate its own receipt, defeating capability and
   certificate expiry.
4. Hosted bootstrap cannot issue the second browser's grant, rotate its KEK,
   or perform the `RecoverySuccession` that the next paragraph requires.
5. Several supposedly canonical encodings directly disagree or remain
   pseudocode, including CBOR ordering, frontier keys, control constants,
   recovery resolution, receipts, and local records.
6. Memory policies, classification, evidence, and export are still prose where
   a deterministic fold or two-sided durable operation is required.

The document should remain `v0.1 draft`. After the issues below are corrected,
the vector harness is exactly the right freeze mechanism.

## What is already strong

Keep these decisions unless implementation uncovers a concrete contradiction:

- One deterministic binary encoding profile, closed signed objects, domain
  separation, and low-S P-256 validation.
- The tagged `AuthorizationProof`, reserved control namespace, and one signed
  envelope family.
- Independent recovery as an owner-held offline authority, with trusted-client
  `RecoverySuccession` taking precedence over the hosted admin branch.
- D0-A ownership of the canonical frontier type.
- Separation of authored crypto epoch from mutable key-wrap epoch.
- Item-level AEAD, per-item DEKs, and zone-rotation erase rather than an
  undefined erasable-key side store.
- A local durable outbox rather than a cache.
- Concrete claim, judgment, pin, unpin, replacement, declassification, and
  evidence-read vocabulary.
- Advisory-only body supersession; status-changing supersession is a judgment.
- Replayable `assert` as claim plus self-accept, conservative disputes,
  policy-bound pins, source-classification preservation, composite export as
  the hard service path, and honest paraphrase residuals.
- A migration invariant that preserves exact signed operation bytes.
- Rust/browser vectors as a required gate rather than example-only fixtures.

## P0 freeze blockers

### 1. Make the canonical encoding actually canonical

Section 1 selects RFC 8949 Core Deterministic Encoding, then describes map-key
ordering as “length-first, then lexicographic.” Core deterministic CBOR uses
bytewise lexicographic order of the encoded keys; length-first ordering is a
separate optional profile. Pick one and name the correct profile. The cleanest
choice is Core Deterministic Encoding throughout. See
[RFC 8949 §4.2](/Users/vm/owner-plane-d0a-spec.md:44) and the
[RFC Editor text](https://www.rfc-editor.org/rfc/rfc8949.html).

There is also a direct internal conflict: E3 allows only UTF-8 text map keys,
while `Frontier.heads` uses byte-string `writer_id` keys
([§4.6](/Users/vm/owner-plane-d0a-spec.md:305)). Prefer a canonically sorted,
duplicate-free array of `{writer_id, seq, op_hash}` records. It maps cleanly to
Rust and JavaScript and avoids a one-off exception to E3.

Before vectors, publish normative CDDL plus a validation table that pins:

- exact 16-byte control constants rather than `0x00…00 || "CTRLZONE"-style`;
- Ed25519 and P-256 public-key encodings, allowed lengths, P-256 point
  validation, signature bytes, and the domain used for `signer_key_id`;
- `protocol_version` versus `v`—M2 names a field absent from the header;
- first-sequence and `previous_writer_hash` sentinel values;
- the control-envelope values for `authored_crypto_epoch` and
  `capability_epoch`;
- required, optional, mutually exclusive, and forbidden fields per tagged
  variant;
- sorted/unique semantics for capabilities, flows, causal references, labels,
  evidence, and other set-like arrays;
- duplicate-map-key rejection, integer ranges, UTF-8 policy, maximum nesting,
  maximum body/text/list sizes, and exact treatment of explicit defaults;
- version fields on Memory and control bodies, or a correction to the claim
  that all objects begin with `v: 1`.

Generic CBOR deserialization is not sufficient to prove original-byte
canonicality or reliably reject duplicate keys. Require a strict raw decoder or
duplicate-aware parse followed by canonical re-encoding and byte equality.

The blanket rule that every hash is domain-separated also conflicts with raw
`SHA-256(ct)`, unspecified public-key/evidence/name hashes, and the CRC. Either
list deliberately raw digests as explicit exceptions or give each a frozen
domain tag.

Freeze the HPKE recipient package as an actual object, not only a suite name:
recipient certificate/KEM-key identity, KEM/KDF/AEAD IDs, P-256 public-key and
encapsulated-key encodings, `enc`, ciphertext, exact `info`/AAD, and validation
errors. RFC 9180 requires validation of P-256 inputs and DH outputs; malformed
points must fail before any wrap is accepted.

### 2. Redesign local persistence so erasure can work

The daemon log currently stores type `0x01 SignedOperation bytes` and type
`0x02 ItemRecord`; IndexedDB similarly has separate `ops` and `items` stores
([§6](/Users/vm/owner-plane-d0a-spec.md:410)). That durably preserves every
plaintext Memory body outside its item DEK. Destroying the DEK would not erase
the plaintext copy.

Tenant operations MUST be persisted only inside encrypted item records. If
control operations are intentionally plaintext, give the control log a
separate schema and explicitly prohibit plaintext operation records in tenant
zone logs. Writer-sequence allocation, encrypted-item append, and outbox
mutation must form one logical durable commit, not two independently tearable
records.

Split the current outer object into:

- an immutable, transportable ciphertext record containing version, AEAD ID,
  nonce, ciphertext, and enough fixed context to verify it;
- a mutable key-wrapper revision bound to item identity, wrap epoch, and the
  accepted control operation that installed that epoch;
- an optional local private index mapping `op_hash` to ciphertext address,
  which is never sent to Connect and remains inside the same erasure boundary.

`ct_hash = SHA-256(ct)` omits the nonce, algorithm, and context required to
open the object. Make the content address a domain-separated hash of the whole
immutable ciphertext record. After decryption, recompute `body_hash`, validate
the signature, recompute `op_hash`, and compare any private index entry.

The DEK-wrapper AES-GCM nonce needs an exact uniqueness rule under the reused
zone KEK. A deterministic nonce derived from `(plane, zone, wrap_epoch,
item_address)` is attractive; random nonces instead require a per-KEK
invocation cap and collision rejection. AES-GCM nonce reuse under one key is
not a benign parser error. The item nonce is less troublesome because each item
uses a fresh DEK, but “zero cost” is still inaccurate—it consumes stored bytes.

Finally, define current-wrapper selection and a crash-safe local erase state
machine:

```text
authorized -> old epoch fenced -> new KEK installed -> survivors rewrapped
           -> local checkpoint committed -> old wrapper/key destroyed
           -> tombstone complete
```

Specify restart behavior at every boundary. A concurrent old-epoch write or an
older wrapper must not resurrect after a crash. D0-B can add replica
acknowledgements and distributed GC; D0-A owns this local state machine.

### 3. Complete genesis and the control reducer

`GenesisBundle` contains `EnrollDevice…` and `IssueGrant…` placeholders, but
the certificate section says their root authorization rides an enclosing
control operation. Freeze whether genesis embeds the raw cert/grant bodies,
nested signed operations, or creates only the descriptor followed by ordinary
control operations.

A usable genesis must establish, atomically or through explicitly admissible
bootstrap steps:

- initial admin and recovery epochs;
- the first device certificate and writer registration;
- its capability grant;
- the initial private zone, zone policy, KEK, recipient wrap, and default
  space/policy binding.

The hosted ceiling currently permits exactly hosted `EnrollDevice`,
`RevokeDevice`, and tenant writes. It therefore forbids all of the following:

- `IssueGrant`, so the newly enrolled browser cannot write or read;
- `KekRotation`, so revoking that browser cannot remove future decryption;
- `RecoverySuccession`, even though §7.3 names it as the only trusted re-root
  path.

Permit narrowly ceiling-constrained issue/revoke grants, same-class KEK
rotation, and `RecoverySuccession`. Conversely, the ceiling does not currently
forbid `memory.curate`, pins, procedure/preference acceptance, or
declassification. Add a body-sensitive hosted matrix enforcing the umbrella's
instruction-grade curation ceiling.

The newly introduced `class_ceiling <= internal` also needs explicit owner
ratification. As written it prevents the zero-daemon private diary from storing
`private` or `sensitive` claims, while offering little protection from the
hosted JavaScript already admitted as plaintext TCB. The safer ceiling is on
export, declassification, instruction curation, and effects—not on accurately
labeling private data.

Add the control lifecycle operations already referenced elsewhere:

- `SpaceCreate` and governed space-policy/classification-minimum assignment;
- `ZonePolicySet` for strictness and accepted time witnesses;
- writer registration, retirement, and signing-key continuation;
- capability-epoch advance and scope rules;
- provenance-evidence installation/validation;
- exact checkpoint/hash references needed locally.

Also define whether an operation authored under an old crypto/capability epoch
is rejected, quarantined, or accepted through a signed cutoff after rotation.
The current fields record epochs but the reducer does not consume them, and the
strict/lenient `ZonePolicy` is still an ellipsis rather than a schema or fold.

`KekRotation.wraps` and `CheckpointCommit.gc_fence` currently depend on
undefined later-gate shapes. Either freeze their D0-A signed subtypes now or
move distribution/GC packages outside the immutable control body.

Private-key custody is also absent despite D0-A claiming root/device ceremonies.
Define, per provenance class, where the admin signing key and device signing/KEM
keys live, whether they are extractable, how passkey unlock/sealing works, how
native/daemon keys use platform custody, and what is backed up. Reconcile the
umbrella's vault-style root custody with the newly separate recovery key. State
the hard implementation boundary: durable keys and plane plaintext remain in
the controller/client custody layer and never enter `intendant-runtime`.

### 4. Make recovery convergence deterministic

The trusted-client recovery choice is good, but the chain mechanics are not
yet closed. A recovery resolver has one `previous_writer_hash` while claiming
to resolve multiple branch heads. Define its base uncontested head, sequence,
canonically sorted abandoned-head set, `safe_control_frontier`, and the exact
rule by which branch-only certificates, grants, policies, and KEK mutations
become invalid.

`epoch = max(observed epochs) + 1` is not deterministic: replicas can observe
different branches. Derive the new epoch from the causally enumerated recovery
input, or make `(recovery_epoch, admin_epoch)` lexicographic authority where a
new recovery epoch dominates every admin epoch from the prior recovery epoch.
Then define tenant-writer cutoff generation separately.

The phrase derivation needs interoperable details:

- English BIP39 wordlist and checksum validation;
- UTF-8 NFKD normalization of mnemonic and salt;
- empty passphrase as an explicit v1 choice;
- exact PBKDF2 output length;
- exact HKDF salt, Extract, Expand, `info`, and Ed25519 seed interpretation.

Those details are normative in the
[BIP39 specification](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki),
and the additional HKDF step is Intendant's own protocol and must be fully
specified. Add phrase→seed→public key→commitment→drill vectors.

The healthy-admin/lost-recovery-phrase rule deliberately forces plane
migration. That can be a valid v1 security choice, but it is consequential
enough to require affirmative owner ratification and a migration procedure;
“silence ratifies” is not sufficient.

### 5. Replace self-attested timestamps with an explicit time-witness model

“Plane devices always qualify” as receipt signers makes expiry ineffective. A
compromised writer can author an operation after expiry and sign its own receipt
with a backdated `ts_ms`. A signature authenticates who asserted a time, not
whether the time is true.

Separate three objects:

- storage durability receipt over a ciphertext/item address;
- replica/frontier acknowledgement;
- authorization `AcceptanceReceipt` or bounded `LeaseProof` from a
  root-authorized time/freshness witness.

Zone policy must identify allowed time-witness keys/classes, normally exclude
the operation signer, and state the residual trust when Connect is selected.
If no accepted witness is online, deadline-based authority fails closed or the
writer uses an explicit sequence/byte budget. A single-device offline plane
cannot honestly claim independent wall-clock expiry.

X1 also expects Connect's receipt to name `op_hash`, but Connect sees only
ciphertext identities. Let Connect receipt the immutable item address; a zone
member verifies the item-address→decrypted-operation binding. Receipts need a
closed signed wrapper with issuer key/algorithm, statement, policy reference,
sequence or anti-replay data, and signature.

No lease-proof wire shape is currently defined, and X3 references a nonexistent
grant `max_age_ms`. Add both. Validator-local time may gate a live service
request, but it cannot change the replicated fold. Durable admission must carry
an immutable qualifying proof; otherwise two replicas evaluating at different
times disagree.

Budget rules need the same precision: define `max_bytes` over exact canonical
bytes, key accounting by grant hash as well as writer/epoch (or forbid multiple
budgets in one epoch), and define how capability epochs advance. The current
grant-specific budget combined with `(writer_id, capability_epoch)` accounting
is ambiguous when two grants share an epoch.

### 6. Separate service-edge IAM from portable operation validation

The five-way evaluator requires a live daemon-local session grant and token and
says any absent component denies ([§10](/Users/vm/owner-plane-d0a-spec.md:657)).
A zero-daemon browser has neither. Native supervised sessions intentionally use
controller-bound dispatch and also have no MCP bearer. Conversely, a remote
replica cannot reconstruct whether an ephemeral token was live when another
daemon signed an operation.

Define access-shape variants:

1. direct enrolled human/browser/native device;
2. controller-attested native supervised session;
3. external process-tree bearer;
4. mediated peer/session.

At the service edge, each shape intersects the applicable local grant, token or
controller binding with plane capability, request constraints, and provenance
ceiling. Durable operation validation uses only portable evidence: exact
certificate/capability, accepted control state, signed actor attestation, body
invariants, and causal dependencies. The signing daemon attests that its local
check occurred; the replicated fold does not depend on expired ephemeral state.

Also separate durable write budgets from live search/read rate limits. A read
request has no writer sequence and cannot consume the deterministic fold budget
described in X4.

## Memory reducer blockers

### 7. Map service permissions to exact wire operations

Grants authorize `memory.propose`, `memory.assert`, and `memory.curate`, while
the log contains `m.claim`, `m.judge`, `m.pin`, and related bodies. A replica
cannot tell whether an `m.claim` was a proposal or the first half of an assert,
or whether an accept/declassify judgment was authorized by the cited grant.

Freeze an admission table mapping every wire operation and body variant to:

- required grant operation;
- allowed target kind and actor relation;
- causal references and body invariants;
- hosted-ceiling rule;
- budget charge and idempotency behavior.

For `assert`, use a compound request/bundle identity and define partial commit:
either persist claim+accept atomically, or explicitly leave a candidate if the
accept half fails. State whether it consumes one or two op/byte budget units.

### 8. Make policies and policy selection canonical

The spec calls policies content-addressed but judgments and pins name only ID
and version; the rule tuples and two built-ins are prose, so their promised
hashes cannot be generated. Freeze exact policy objects and hashes, exact actor
class derivation, and the admissibility table for each judgment.

A caller-supplied `context.policy_id` cannot choose its own more-permissive
view. A control operation must bind each space/use to an accepted policy hash
and classification minimum. Projection stamps need at least data frontier,
control frontier, reducer version, policy hashes/mapping version, key epoch,
classification policy, and explicit `as_of_ms`.

Semantic claim/pin expiry is a time-relative view rule evaluated at that named
`as_of_ms`; it is not the receipt-time rule used to decide whether an
authorization artifact arrived before its deadline. The current text would
otherwise let a claim received before `expires_at` remain eligible forever.

### 9. Replace the classification equation with a fold

`max(...) - authorized declassifications` does not define concurrent raises,
declassifications, new evidence, or causal precedence. “Curator raises” has no
operation; declassification has no distinct grant operation.

Define exact classify/raise and declassify judgments and capabilities. A safe
shape is:

- immutable space/source/evidence floors;
- concurrent raises combine by maximum;
- declassification is effective only when authorized and causally observes the
  basis it lowers;
- a concurrent or later unseen raise/source taint wins conservatively;
- missing or dangling evidence receives a defined conservative class.

Request classification is derived by the service from the target and sources,
not trusted from the optional caller input.

### 10. Define typed evidence and the complete export/import flow

A bare 32-byte evidence hash cannot locate cross-plane or cross-zone evidence,
session transcript spans, daemon approval records, or external artifacts.
Freeze a typed reference carrying resource namespace, locator semantics,
content digest, classification/taint, and optional source span. Define dangling
refs, cycle/depth limits, and separately authorized dereference. Until Agenda
approval evidence has a deterministic verifier, workflow agents should propose
`decision` claims rather than self-accept them.

`m.export` is only a source-side intent. It neither creates destination-encrypted
content nor represents the second plane's authority. Define:

- a source release operation with stable export ID/nonce, exact source hashes,
  destination, copied-content digest, and classification floor;
- a destination import/claim operation referencing that release and proof,
  idempotently creating a candidate under destination policy;
- same-plane cross-zone wrapping/signing rules;
- a typed endpoint union covering plane zones and provider/model egress.

Capability flow endpoints currently omit plane IDs and cannot represent the
cross-plane pair. Add them or use the endpoint union.

### 11. Ratify the remaining fold behavior

The status fold is a good start, but freeze these outcomes explicitly:

- author Retract versus curator Retire authority and whether either is
  irreversible;
- whether an old claim revives when its accepted replacement becomes disputed,
  retracted, or expired;
- supersession-chain and cycle output, including visibility;
- pin validity when several accepts qualify—there is no undefined “winning”
  accept; a pin should name one still-authorized acceptance or define a
  deterministic selector;
- exact pin destination, role, token-budget, and provenance-floor types;
- evaluation-time and missing-policy outcomes.

Unknown versions, missing dependencies/proofs/receipts, expired ops, revoked
ops, duplicates, forks, frozen control state, and corrupt records also need a
closed outcome taxonomy: permanent reject, pending, quarantine/re-proposal,
idempotent accept, or accepted. Later proof arrival must have a defined
re-evaluation rule.

## Storage portability and implementation gate

The custom daemon record needs magic/version and plane/zone binding; exact
length and CRC coverage; hard record limits; middle-corruption versus torn-tail
behavior; locking; wrapper supersession; and platform durability semantics.
`fsync` plus directory `fsync` is not a Windows contract—define the abstraction
implemented by Unix sync calls and Windows `FlushFileBuffers`, including
failure behavior. Multiple Intendant controller processes can run on one box,
so per-plane append/sequence allocation also needs cross-process exclusion.

IndexedDB transactions are atomic, but `durability: "strict"` is a user-agent
hint rather than a power-loss guarantee. Request it where supported, request
persistent storage, and state the degraded floor elsewhere. The
[IndexedDB specification](https://www.w3.org/TR/IndexedDB/) explicitly frames
strict durability as a hint. Feature-detect Web Locks; the
[Web Locks specification](https://www.w3.org/TR/web-locks/) provides
cooperative same-storage-bucket exclusion, not authorization. Fail read-only or
use a specified fallback when unavailable. A new writer generation also needs
the control-plane writer/grant lifecycle missing above.

The current checklist conflates format freeze with P1 production readiness.
Use two gates:

1. **D0-A format/reducer freeze:** exact schemas, decision record, and all
   cross-language vectors green.
2. **P0 implementation readiness:** production crypto/key-custody adapters,
   encrypted local storage, strict parsers, crash recovery, platform durability,
   and macOS/Linux/Windows/browser conformance green.

Only the second authorizes durable P1 data. The absent crate/vectors are expected
for a first draft; they simply mean the mechanical freeze criterion is not yet
met.

## Vector and test corrections

Before implementing the vectors, define their JSON schema, binary convention
(normally CBOR hex), deterministic RNG injection, expected output/error, and
source object. Add:

- BIP39/HKDF/recovery commitment/drill vectors;
- exact public-key encodings, malformed P-256/HPKE inputs, recipient-package
  serialization, and wrong-context failures; RFC 9180 requires P-256 point and
  DH output validation ([RFC 9180](https://www.rfc-editor.org/rfc/rfc9180.html));
- complete item-address and wrapper-revision vectors;
- receipt/time-witness and lease-proof vectors;
- exact local-record bytes, oversized length, torn tail, middle corruption,
  cross-process collision, and every erase crash boundary;
- migration/re-encapsulation proving exact signed bytes survive;
- complete reducer output snapshots and projection stamps;
- parser/fold fuzzing with hard resource bounds;
- browser feature/degraded-path tests and native crash tests on macOS, Linux,
  and Windows.

The IAM list currently has seventeen distinct ordered deny reasons, so “at
least 12 vectors covering every deny reason” cannot work if each decision emits
one reason. Require one vector per reason plus allow-path compositions.

Remove “read-A + write-B composition refusal” as an absolute vector. Test the
composite export hard gate and the explicitly bounded taint defense; an
unconstrained session's paraphrase remains outside the guarantee.

Separate local D0-A erase tests from D0-B checkpoint/backup integration tests
whose wire machinery is not yet defined.

## Decisions requiring affirmative ratification

The following are genuine v1 product/security choices, not mere formatting:

- Core deterministic CBOR versus the alternative length-first profile.
- Twenty-four-word owner-held recovery and whole-plane migration after phrase
  loss with a still-healthy admin key.
- Whether hosted planes may store correctly classified private/sensitive data.
- The trusted time-witness policy and the no-witness offline behavior.
- Recovery branch-cut semantics and post-recovery writer cutoff defaults.
- Superseded-claim revival behavior.
- Deterministic versus random per-KEK wrapper nonces.

Record explicit decisions; do not use “silence at freeze ratifies” for these.

## Recommended next pass

Do these in order:

1. Publish exact CDDL, bounds, constants, validation outcomes, and control-op
   schemas.
2. Fix encrypted local persistence, item identity/wrapper selection, and the
   erase crash state machine.
3. Fix hosted genesis, control lifecycle, recovery branch cuts, and direct
   browser/native IAM.
4. Replace generic receipts with explicit time-witness and lease proofs.
5. Make Memory admission, policy selection, classification, evidence, and
   export/import executable.
6. Only then implement the vectors and production P0 storage/crypto adapters.

At that point a second, much shorter freeze audit should be possible. The
architecture is settled; the remaining question will be whether every byte and
state transition has exactly one answer.
