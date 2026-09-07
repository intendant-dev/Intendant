# Synthesized review: D0-A Core + Memory normative specification (v0.1)

*2026-07-11. Synthesis of
[`owner-plane-d0a-spec-review.md`](/Users/vm/owner-plane-d0a-spec-review.md)
and the independent peer report
[`owner-plane-d0a-spec-review-2.md`](/Users/vm/owner-plane-d0a-spec-review-2.md),
against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md).*

## Executive verdict

**Do not freeze v0.1.** The two reviews independently agree that this is a
strong, architecture-complete skeleton. Neither finds a reason to reopen the
umbrella RFC. They also agree that the current document is not yet a unique,
implementable Rust/browser protocol or a safe gate for durable P1 data.

The peer report strengthens the review in four important places:

1. Unbound writer IDs let a device reset budgets and evade cutoffs by minting a
   new writer generation.
2. Capability epochs have no scope, lifecycle, bump operation, or old-epoch
   admission rule despite being an explicit D0-A obligation.
3. New-device KEK delivery is absent; enrollment currently implies either an
   unusable device or an undocumented O(zone-size) rotation.
4. Device-class ceilings are missing independently of the hosted-*plane*
   ceiling.

The first review remains stronger on the highest-risk byte/storage issues: the
local format durably stores plaintext beside ciphertext, the receipt model does
not establish trustworthy time, exact CBOR/item/control schemas remain
contradictory, and Memory policy/classification/export are not yet executable.

The combined next step is therefore not “write more prose everywhere.” It is a
specific protocol-completion pass, in the order given below, followed by
vectors.

## What should be retained

Both reviews strongly endorse these decisions:

- Low-S P-256 is derived from the protocol's fork-evidence model rather than
  adopted by habit.
- `authored_crypto_epoch` in signed bytes and mutable `key_wrap_epoch` outside
  them reconcile immutable operations with rotation and erasure.
- One envelope family plus tagged `AuthorizationProof` removes bootstrap
  circularity.
- Recovery-arm-only hosted re-root prevents the hosted admin root from
  elevating itself, and competing recovery claims fail honestly.
- The item—not the segment—is the encryption unit.
- Per-item DEKs plus zone-rotation erase are an explicit, costed mechanism.
- The frontier type belongs to D0-A while D0-B owns proofs and transport.
- Immutable bodies plus append-only judgments are the correct Memory shape.
- Body supersession is advisory; authorized judgments change derived status.
- `assert` is represented as claim plus self-accept, pins bind their acceptance,
  source classification remains provenance, composite export is the hard path,
  and paraphrase remains an admitted residual.
- Exact operation bytes survive P1-to-P2 re-encapsulation.
- Cross-language vectors and required CI are the right freeze mechanism after
  the schemas become coherent.

## Freeze blockers, in repair order

### 1. Freeze exact bytes and remove plaintext tenant persistence

The canonical profile is not yet internally consistent:

- It selects RFC 8949 Core Deterministic Encoding but describes the alternative
  length-first key order. Core deterministic CBOR sorts by bytewise
  lexicographic order of encoded keys. Pick one profile and name it accurately;
  Core Deterministic Encoding is the simpler recommendation. See
  [RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html).
- E3 permits only text map keys while `Frontier.heads` uses byte-string
  `writer_id` keys. Use a sorted, duplicate-free array of head records or
  explicitly revise E3.
- Control constants are still “`CTRLZONE`-style,” key encodings and key-ID hash
  domains are absent, the first-chain sentinel is unknown, set-like arrays lack
  sort/dedup rules, and `protocol_version` is named in M2 but absent from the
  header.
- “All objects begin with `v:1`” does not match unversioned Memory/control
  bodies. Default-value rejection cannot be implemented until defaults are
  enumerated.
- Control bodies, `ZonePolicy`, evidence, receipts, HPKE recipient wraps, and
  log payloads remain pseudocode or ellipses rather than closed schemas.
- No normative byte/count/depth limits exist, although acceptance limits are
  part of deterministic parsing, not merely operational policy.

Publish normative CDDL plus a validation table: required/forbidden fields,
lengths/ranges, duplicate-key rejection, exact UTF-8 policy, canonical set
ordering, hard caps, and a closed parse/admission outcome enum. Require a strict
raw decoder or duplicate-aware parse plus canonical re-encode equality; an
ordinary object deserializer cannot prove original-byte canonicality.

More seriously, the daemon log stores both plaintext `SignedOperation` records
and encrypted `ItemRecord` records, and IndexedDB has separate `ops` and
`items` stores ([§6](/Users/vm/owner-plane-d0a-spec.md:410)). This defeats the
entire DEK-erasure story.

Tenant logs must persist encrypted item commits only. If control operations are
intentionally plaintext, give them a distinct control-log schema and prohibit
the plaintext record type in tenant logs. Sequence allocation, encrypted-item
append, and outbox mutation must be one logical durable commit.

The item shape also needs completion:

- Content-address the complete immutable ciphertext record—version, AEAD ID,
  nonce, ciphertext, and fixed context—not only `ct`.
- Keep `op_hash` in a private local index or encrypted portion; if D0-B batches
  the current outer record verbatim, Connect learns a stable operation identity
  the umbrella keeps zone-private.
- Recompute `body_hash`, signature, and `op_hash` after decryption.
- Define the DEK-wrapper nonce's uniqueness rule under a reused zone KEK.
  Deterministic derivation from item identity and wrap epoch is attractive;
  random nonces need an invocation cap and collision handling.
- Define which wrapper revision is current and bind it to an accepted
  key-rotation control operation.

Finally, freeze a crash-safe local erase state machine: authorize, fence old
epoch, install new KEK, rewrap survivors, commit local checkpoint, destroy old
wrappers/key, complete tombstone. D0-B later adds replica acknowledgements and
distributed GC; D0-A must prevent local crash resurrection now.

### 2. Complete genesis, hosted bootstrap, and control lifecycle

A fresh plane cannot walk the stated genesis→write→enroll→revoke path:

- `GenesisBundle` has no default private zone, policy, epoch-1 KEK/wrap, space,
  or writer registration.
- `ZoneCreate` carries no initial wraps.
- `EnrollDevice` does not add a current-epoch KEK wrap. Requiring a full
  rotation on every join would cause O(zone-size) survivor rewrap; add an
  authenticated current-epoch wrap-add operation or ratify rotate-on-join.
- There is no `SpaceCreate` or governed space-policy/classification-minimum
  assignment.
- `ZonePolicy` is referenced but cannot be installed.

The hosted ceiling then compounds the problem. It allows exactly hosted
Enroll/Revoke and tenant writes, excluding `IssueGrant`, KEK rotation, and the
`RecoverySuccession` used by the next paragraph. A second hosted browser can be
certified but never granted access; a revoked browser cannot be excluded from
future decryption.

Do not solve this by making every non-explicitly-forbidden control operation
available to hosted code. That would silently authorize destructive
checkpoint/erase behavior beyond the ratified narrow ceiling. Prefer constrained
compound/lifecycle operations:

- genesis creates the first zone, space, cert, writer, ceiling-constrained
  grant, policy, KEK and wrap;
- hosted enrollment atomically adds the hosted cert, constrained grant, writer
  lineage and current-epoch wrap;
- hosted revocation atomically revokes grants, cuts off the writer and rotates
  future keys;
- `RecoverySuccession` remains admissible only through the recovery arm;
- destructive maintenance and erase follow an explicitly ratified request/
  execution model.

Add exact control operations for zone/space policy, writer lifecycle,
signing-key continuation, capability epochs, provenance evidence, and the local
checkpoint references already consumed elsewhere. Define certificate renewal
under stable `device_id`: coexistence, replacement, grant reissuance, and old
cert revocation.

Recovery convergence also needs exact bytes and ancestry. A resolver has one
`previous_writer_hash` but claims multiple abandoned heads; define base
uncontested head, sequence, sorted resolved-head set, safe control frontier,
branch cut, and tenant cutoff output. Replace `max(observed epochs)+1`, which
varies by replica, with a rule derived solely from the enumerated recovery input
or a lexicographic recovery-epoch precedence scheme.

Root/device key custody must be part of the ceremony contract: per-class
extractability, passkey sealing/unlock, native/daemon platform custody, backup,
and the invariant that durable keys and plane plaintext never enter
`intendant-runtime`.

The BIP39 recovery derivation needs English wordlist/checksum, UTF-8 NFKD,
empty-passphrase choice, PBKDF2 output length, and exact HKDF salt/Extract/
Expand/Ed25519-seed rules. See the
[BIP39 specification](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki).

### 3. Bind capability epochs and writer generations to non-resettable authority

This is the peer review's most important new security finding.

`writer_id` is locally minted, `CapabilityGrant.writer_id` is optional, budgets
are keyed by `(writer_id, capability_epoch)`, and cutoffs name a writer. A
device can mint a fresh writer when its budget is exhausted or after a cutoff,
resetting the budget and escaping the cutoff.

Freeze one writer-lineage model:

- every write-capable grant binds a stable writer lineage to a certificate or
  stable device identity;
- each browser restore/generation is a successor within that lineage, not a new
  budget principal;
- budgets and security cutoffs apply at least to grant/lineage, not only the
  current generation;
- a pre-authorized successor rule lets an honest browser recover without an
  admin ceremony while preventing arbitrary new lineages;
- signing-key continuation preserves the lineage across cert/key rotation;
- new lineage creation is a control-authorized event.

Deriving `writer_id` directly from `cert_hash` is not automatically safe because
certificate renewal would change the basis. A stable lineage object or stable
device identity plus root-authorized continuation is cleaner.

Capability epochs likewise need:

- exact scope—grant lineage, subject, zone, or another named domain;
- the operation that advances them;
- who may advance them;
- quarantine/re-proposal behavior for old-epoch operations;
- budget reset semantics;
- the relationship among header epoch, cited grant epoch, cutoffs and strict/
  lenient zone policy.

`max_bytes` must name the exact canonical bytes counted, and accounting must be
unambiguous when multiple grants share a writer/epoch.

Separately, freeze a per-device-class ceiling table for *every* plane. A hosted
browser enrolled after trusted re-root is still a hosted browser; `mobile` and
`other` also need defined maxima. Pin grant contents and judgment/approval verbs
per class, including export, declassification, graduation and future effects.

### 4. Replace generic device timestamps with a real time/lease authority

The receipt idea can make the fold deterministic, as the peer review notes.
The current signer policy does not make the timestamp trustworthy. “Plane
devices always qualify” lets the operation signer create a backdated receipt
for its own post-expiry operation. Replicas can deterministically agree on a
forged authorization fact.

Treat this as a freeze blocker, not merely an adversary-table caveat:

- storage receipts prove ciphertext durability;
- replica acknowledgements prove a device observed a frontier;
- `AcceptanceReceipt` and `LeaseProof` are separate signed authorization inputs
  from policy-authorized time/freshness witnesses;
- the operation signer does not qualify for its own deadline receipt;
- zone policy names trusted issuer keys/classes and the residual trust when
  Connect is selected;
- without an accepted witness, wall-clock deadline authority fails closed or
  uses explicit sequence/byte budgets.

Connect receipts an item/ciphertext address, not zone-private `op_hash`; a zone
member validates the item→operation binding. Freeze receipt signature wrappers,
issuer rotation, anti-replay/sequence data, and policy references.

No lease-proof wire object exists today, and X3 references a grant
`max_age_ms` field that does not exist. Add both. Validator-local time may gate
a live request, but must not make the replicated fold change between replicas.

### 5. Split service-edge IAM from portable fold admission

Both reviews independently find that §10 denies the flagship lane. Requiring
all five components means a zero-daemon browser lacks a daemon-local session
grant and token; native controller-bound sessions lack an MCP bearer.

Define explicit service-edge access shapes:

1. direct enrolled browser/human/native device;
2. controller-attested native supervised session;
3. external process-tree bearer;
4. mediated peer/session.

Each shape intersects only its applicable local binding/token with plane
capability, request constraints and provenance ceiling. “Vacuously satisfied”
is not enough by itself: portable operation validation must be a separate
algorithm using only certificate/grant, accepted control state, signed actor
attestation, body/causal invariants and immutable receipts. A remote replica
cannot validate whether another daemon's ephemeral token remains live.

Keep read/search service quotas separate from durable writer-sequence budgets;
a read call has no replicated writer sequence.

### 6. Turn the Memory skeleton into executable admission and folds

The status precedence skeleton is promising, but the full Memory protocol is
not fixed by adding one IAM table.

First, map every persisted `m.*` body variant to the required service permission,
target kind, actor relationship, causal invariants, class ceiling and budget
charge. `memory.assert` needs compound request/idempotency and partial-commit
semantics. Judgment authority is security-sensitive: if a low-trust proposer's
`Dispute` counts, it can suppress accepted pinned context until an owner
causally answers it. Add an unauthorized-dispute non-counting vector and an
explicit self-retract decision.

Second, make policies real objects:

- exact content-addressed bytes and hashes;
- full verdict×kind×space-class×actor-class tables;
- exact actor-class derivation;
- control-governed space/use→policy selection;
- complete projection inputs and stamps.

A caller cannot choose a permissive `context.policy_id`. Semantic claim/pin
expiry is evaluated at an explicit view `as_of_ms`; receipt deadlines govern
authorization arrival and must not make a claim received before `expires_at`
remain retrievable forever.

Third, replace `max(...) - declassifications` with a deterministic
classification fold. Define raise/declassify operations and capabilities,
causal rules, concurrent behavior, unknown/dangling evidence taint, and the rule
that a concurrent or unseen raise wins conservatively. Request classification
is derived by the service, not trusted from caller input.

Fourth, freeze typed evidence references: plane/zone or external namespace,
locator/source-span semantics, digest, classification, dangling behavior and
depth/cycle bounds. Until an approval-trail verifier exists, workflow agents
should propose `decision` claims rather than self-accept them.

Finally, `m.export` alone cannot create a destination candidate or express two
planes' authority. Define a source release and destination import/claim pair
bound by export ID/nonce, copied-content digest, source classification floor,
destination and proof. Same-plane cross-zone export also creates
destination-encrypted content. Capability flows need plane IDs or a typed
endpoint union that also represents provider/model egress.

Freeze the remaining view outcomes: Retract versus Retire authority and
presentation, replacement dispute/expiry and old-claim revival, pin validity
with multiple accepts, pin field types, missing-policy behavior, and full
per-item read authorization during auto-context. A pin selects; it never grants
read authority. Define where the “owner-audit domain” lives or defer it
explicitly.

### 7. Finish portable storage and the gate itself

The local record format needs magic/version, plane/zone binding, exact length
and CRC coverage, hard limits, middle-corruption versus torn-tail behavior,
wrapper supersession, and cross-process exclusion. Intendant permits multiple
controller processes on one box, so one per-zone append file cannot assume one
process.

Define a platform durability abstraction rather than Unix `fsync` as the
protocol: appropriate Unix sync behavior, Windows `FlushFileBuffers`, atomic
creation/replace and failure outcomes.

IndexedDB transactions are atomic, but strict durability is a browser hint, not
a power-loss guarantee. Request strict/persistent storage where available and
state the degraded floor. See the
[IndexedDB specification](https://www.w3.org/TR/IndexedDB/). Web Locks provide
cooperative exclusion within a storage bucket, not authorization; feature-detect
and specify read-only/fallback behavior. See the
[Web Locks specification](https://www.w3.org/TR/web-locks/).

Before implementing vectors, define their JSON/binary schema, deterministic RNG
injection, expected bytes/result/error and normative source object. Add recovery
derivation, malformed HPKE/P-256 inputs, receipt/time/lease, local-record crash,
wrapper selection, migration, projection and hard-bound vectors. RFC 9180
requires validation of P-256 inputs and DH outputs; pin those failures
([RFC 9180](https://www.rfc-editor.org/rfc/rfc9180.html)).

There are seventeen ordered IAM deny reasons, so “at least 12 vectors covering
every reason” is not a coherent minimum if one result emits one reason. Require
one per reason plus allow-path combinations. Test composite export and bounded
taint; do not reintroduce an absolute cross-call refusal guarantee.

Use two gates:

1. **D0-A specification freeze:** exact schemas/reducers, affirmative decision
   record and cross-language vectors green.
2. **P0 implementation readiness:** production custody/crypto, encrypted local
   storage, strict parser, crash recovery and macOS/Linux/Windows/browser
   conformance green.

Only P0 implementation readiness authorizes durable P1 writes.

## Decisions requiring explicit ratification

Do not let these pass through “silence ratifies”:

- Core deterministic versus length-first CBOR profile.
- Hosted data classification: the current `internal` cap conflicts with a
  private diary. Prefer accurate private/sensitive labeling while restricting
  export/declassification/curation/effects, unless the owner deliberately wants
  a reduced-storage product.
- Which destructive maintenance operations hosted code may authorize. Do not
  infer a blanket exclusion-list policy.
- Join-time KEK wrap-add versus rotate-and-rewrap.
- Erase workflow: every item currently requires an admin control ceremony and
  O(zone-size) rewrap. Prefer a tenant-authorized erase request followed by
  batched admin rotation/GC, or explicitly ratify per-item ceremonies.
- Twenty-four-word recovery and whole-plane migration when the phrase is lost
  but the admin key remains healthy.
- Honest control forks requiring the recovery phrase.
- Recovery branch-cut and post-recovery writer-cutoff defaults.
- Superseded-claim revival behavior.
- Deterministic versus random per-KEK wrapper nonces.
- Trusted time witnesses and no-witness offline behavior.

## Batched exactness fixes before freeze

- Correct O3 to say the only **crypto/KEK** epoch in signed bytes; capability and
  admin epochs are also signed.
- Freeze certificate renewal semantics.
- Define per-class provenance-evidence document shapes or explicitly opaque
  byte semantics.
- State that auto-context still performs full per-session `memory.read`
  authorization.
- Preserve index/view/checkpoint/backup coverage in erase tests.
- Surface Retract versus Retire distinctly and state that v1 has no un-retract.
- Define admin-key custody and owner-audit-domain placement.
- Add vectors for trusted/hosted genesis→first write, second-device history
  read, revoke→rotate→old-device denial, fresh-writer budget evasion, every
  access shape, hosted device on trusted plane, and unauthorized dispute.
- Replace “silence ratifies” with an affirmative decision record.

## How the reviews reconcile

The reports agree on the verdict and most major gaps. Their differences resolve
as follows:

1. **Receipt time:** the peer is right that receipt-based folding can be
   deterministic; the first review is right that current receipts do not
   establish trustworthy time. This is a blocker until issuer policy and lease
   objects exist, not merely a threat-table residual.
2. **Hosted control operations:** the peer's liveness diagnosis is correct, but
   its broad exclusion-list remedy grants too much destructive authority.
   Constrained compound operations preserve both usability and the ratified
   ceiling.
3. **Memory fold:** judgment precedence is a good skeleton, not a completed
   reducer. Policy selection, time, classification, evidence and export still
   affect deterministic views.
4. **Writer identity:** the peer contributes the concrete budget/cutoff evasion.
   Bind budgets to stable authorization lineage; do not derive solely from a
   renewable cert hash without renewal semantics.
5. **Highest severity omitted by the peer:** plaintext tenant operation storage
   still defeats erasure and must be corrected before any vector or durable
   write.

## Final disposition

Keep D0-A at `v0.1 draft`. Revise in the seven-blocker order above, make the
listed owner decisions explicit, and only then implement vectors. The next
review should be a narrow conformance audit over schemas, state transitions and
test coverage—not another architecture round.

The architecture is ready. The bytes, authority lifecycles, and reducer inputs
are not yet frozen.
