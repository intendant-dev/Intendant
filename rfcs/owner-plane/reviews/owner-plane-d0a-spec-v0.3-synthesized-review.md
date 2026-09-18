# Synthesized review: D0-A Core + Memory normative specification v0.3

*2026-07-12. Synthesis of
[`owner-plane-d0a-spec-v0.3-review.md`](/Users/vm/owner-plane-d0a-spec-v0.3-review.md)
and
[`owner-plane-d0a-spec-v0.3-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.3-review-2.md),
against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.3. This is a
freeze-candidate conformance synthesis, not another umbrella-architecture
round.*

## Executive verdict

**Do not declare Gate A yet. Keep v0.3 as the implementation basis and cut one
focused v0.4 protocol-repair pass before generating canonical vectors.**

The reviews agree on the important strategic conclusion:

- v0.3 is the strongest D0-A draft so far;
- the owner-plane architecture is settled and should not be reopened;
- the previous architecture-completion work landed in substance;
- remaining work is local to exact bytes, authority transitions, reducer
  semantics, and conformance artifacts;
- durable P1 Memory writes remain prohibited until Gate B plus the named P0.5
  and tombed-Memory cutover prerequisites.

The reports differ mainly in what “discharged” means. Review 2 treats a blocker
as discharged once the right mechanism and prose section exist. Review 1 uses
the stricter Gate-A meaning: an independent implementation must derive one
verifiable byte shape and state transition without inventing behavior. Under
that standard, the accurate synthesis is:

> **The seven v0.2 blocker families are architecturally discharged, but several
> remain open at the normative wire/reducer level.**

The peer report adds an important last-mile class that Review 1 did not state
as sharply: the policy schema cannot encode the self/author relationships its
own built-in policies rely on. It also usefully exposes unpinned genesis spaces,
safe-human policy lag, `LeaseStmt.ctrl_frontier` dead weight, and several caps
and fold pins.

Review 1 remains necessary because Review 2's optimistic resolution audit
misses independently demonstrated contradictions:

- Connect cannot inhabit the signed receipt issuer schema;
- renewal invalidates receipt-free solo history;
- hosted self-lineage reauthorization has no requester proof;
- capability-grant epoch validity is undefined;
- deterministic 96-bit wrapper nonces have an unbounded cross-item collision
  risk under one GCM key;
- survivor completion does not commit to verified current wrappers;
- the cross-zone completion transaction cannot be encoded by any defined
  frame;
- generic Memory judgment rights bypass dedicated classification and
  instruction-grade controls;
- audit authority is contradictory;
- classification, evidence, temporal revival, and export/import retain
  underdefined reducer branches;
- the normative JSON Schema is invalid JSON, built-in policy bytes/hashes are
  absent, and no vector/core corpus exists.

Gate A is therefore both substantively and mechanically incomplete. This is
still one bounded repair pass—not a new program—but the patch is broader than
only Appendix B.

## Assessment of the peer review

Review 2 is strong and should materially influence v0.4. Its best findings are:

1. **Hosted-safe admission and policy counting disagree.** `judge.safe`
   admits safe-human acceptance/retirement/dispute, while the built-in tables
   do not consistently count those transitions in the hosted diary.
2. **Genesis spaces and initial grants are not pinned.** The boot invariant
   depends on `home` class/policy and safe verbs that `c.genesis` does not
   normatively require.
3. **Policy objects cannot express relational predicates.** “Self,” “same
   actor/session,” and “author of target” are relations, not actor classes; the
   current A.6 policy schema cannot serialize them.
4. **`require_cert_deadlines` and `LeaseStmt.ctrl_frontier` are semantically
   unused.** Signed authority fields must either be validated or declared
   diagnostic.
5. **Audit has no genesis grant and no hosted writer.** The operation's stated
   lane cannot pass admission.
6. **Several exactness pins are valid:** author supersession is admitted but
   non-counting; erased targets need tombstone fold semantics; `mimport`
   duplicates authority fields without equality; optional `grant.kinds` has no
   absence semantics; `w.gen.space_id` is not pinned; and one-device revocation
   assumes one lineage without enforcing it.

Several peer conclusions should not be adopted as written:

- **“Closed CDDL, no fixture-defined shapes” is too strong.** Receipt variants,
  root custody envelopes, survivor commitments, literal policies, released
  bundles, audit authority, and mediator proofs remain absent or prose-only.
- **Signed time is not discharged.** Connect signer identity, witness-policy
  binding, issuer-counter recovery, renewal history, and delayed-proof
  dispositions remain unresolved.
- **Item/storage completion is not discharged.** The nonce construction,
  wrapper-set proof, recipient-key binding, old-KEK deletion durability,
  Batch/PendingXfer representation, and corrupted-final-length handling remain
  open.
- **Memory execution is not discharged modulo only F1/F3.** `judge.full`,
  `curate.instruction`, safe unpin/human evidence, compound assert, sequential
  declassification, evidence verification, `valid_from`, erasure view effects,
  and transfer binding remain open.
- **The vector container is not discharged.** The fenced JSON fails parsing
  because descriptions contain raw newlines; RNG and family×surface behavior
  are not exact.
- **Do not silently adopt `home.class_minimum = public`.** That is a substantive
  privacy default, not a mechanical fix. Pin it through an explicit owner
  ruling; `private` is the conservative personal-memory default unless product
  semantics require otherwise.

The peer's “one targeted patch” conclusion is still directionally right if
“targeted” means the consolidated repair set below.

## What should remain frozen in direction

Do not re-litigate:

- deterministic CBOR/text-key profile and low-S signatures;
- stable device IDs, writer lineages, and non-resetting budgets/cutoffs;
- edge IAM separated from portable admission;
- tagged control proof arms and deterministic recovery branch cutting;
- optional certificate deadlines and an honest non-time solo posture;
- single-zone write grants, generation-one opening, and explicit hosted
  continuity;
- item DEKs, ciphertext-only tenant persistence, and rotation for exclusion;
- hosted-safe versus full owner rights, claims/judgments, supersession revival,
  and sensitive fallback for unresolved evidence;
- signed receipts/leases as distinct authority objects;
- outcome dispositions, named conformance surfaces, and the Gate-A/Gate-B
  split.

V0.4 should complete these mechanisms, not replace them.

## Consolidated v0.4 freeze blockers

### 1. Pin genesis and make policy objects express their actual rules

The boot path is still parameterized by unspecified choices. Freeze the valid
`c.genesis` contents:

- identify exactly one `home` and one `audit` space;
- pin each space's `space_class`, `class_minimum`, and `status_policy`;
- pin hosted/trusted first-certificate constraints;
- pin the initial grant's safe/read/write verbs, finite budget, kinds semantics,
  and generation window;
- require the solo certificate deadline posture and exact ZonePolicy template.

`home` must match the hosted workflow promised by D-29. If it is `personal`,
the built-in policy must count safe-human observation/episode acceptance there;
if assert remains workflow-only, hosted personal entries need the explicit
propose+safe-accept path. Add safe-human retirement rows. Reconcile dispute
scope and remove the false claim that safe-human disputes count in “both
built-ins” unless `owner-v1` actually includes them.

The A.6 rule schema must either:

- add a canonical relational predicate such as `relation: self / author /
  any`, with exact target/actor matching semantics; or
- state that policy objects encode class-level counting only and move all
  self/author relations into a closed reducer table.

Do this before hashing policies. Publish literal canonical policy objects and
actual hashes, not English summaries. `verdict` must be a closed enum; rule
ordering and duplicates must be frozen. Appendix B.1 is a parameterized
template because `zone_id` varies, not one globally hashable constant.

Author supersession is currently admitted but never counted under
`workflow-v1`; add the relational counting row or remove the inert admission
branch. Define absent `grant.kinds` (`all otherwise authorized` or `none`) and
all other conditional-field semantics.

**Closure test:** the exact genesis bytes always pass the hosted/trusted boot
vectors, and every admitted judgment relation can be represented and counted by
the canonical policy bytes.

### 2. Finish signed time, witness, renewal, and solo-budget authority

Use a tagged receipt/lease issuer:

- plane device certificate, resolved to stable `device_id`; or
- Connect/service signer descriptor/certificate with algorithm, key, rotation,
  and counter scope.

The current `issuer_cert` cannot represent Connect. Make receipt kinds four
closed variants so storage-only `size` and subject meanings are enforced.
Require `connect_service_key` exactly when Connect time is enabled.

Bind receipts to plane, zone, and witness policy/control frontier. Store stable
device IDs rather than renewable certificate hashes in witness policy.
`LeaseStmt.ctrl_frontier` must have an exact freshness/coverage check or be
explicitly diagnostic. “Covering checkpoint witness” likewise needs a frozen
operation-coverage predicate or removal from D0-A.

Define `issuer_seq` generation, key rotation, snapshot-rollback recovery, and
fork disposition; otherwise an honest restore can permanently quarantine an
issuer.

Certificate renewal must preserve budget-lane history that has no receipts.
Carry admin-signed per-zone/per-lineage accepted-through cutoffs or an exact
checkpoint commitment; disable only new authorship under the old key. Define
revocation mode/cutoff rather than referring to an unencoded “for cause.”

Make the solo posture genuinely bounded: every write grant in a
`deadline_fallback = budgets` zone must contain finite operation/byte bounds.
Define `require_cert_deadlines` for both enrollment and operation admission,
including pre-policy certificates. Missing receipts/leases are
`pending-dependency` while reordering can still supply them, not immediate
reproposal.

**Closure test:** device and service proofs verify from stable policy state;
renewal preserves prior accepted bytes; solo authority is finite; and late
proof delivery converges.

### 3. Close control, lineage, capability-epoch, and revocation transitions

Pin control-header `authored_kek_epoch`/`capability_epoch` constants, actor/
signer combinations, genesis previous hash, request IDs, and `w.gen.space_id`.

Hosted `c.lineage_reauth` needs a device-signed requester proof; the admin-arm
header cannot prove “own lineage.” Define one effective generation-window
authority—`lineagedef`, grant, or their exact composition—and prevent multiple
grants from multiplying openings. `w.gen.last_known = unknown` needs an
accepted-through/abandon rule; otherwise old generations never retire and
Frontier grows despite D-33. State the disposition of late old-generation
operations.

`grant_epoch_slack` is present but unused. Freeze grant issuance epoch versus
zone/header epoch, strict/lenient windows, future epochs, and budget reset.
Wildcard read grants need a reserved scalar rather than pretending one value
represents multiple zones.

`c.revoke_device` must either enforce one live lineage per device or carry
plural lineage cutoffs. Its embedded rotations can exceed the 64-KiB control
cap for a multi-zone device in populous zones; define separately committed
rotation references with atomic revocation semantics or a safely encodable
bound.

The hosted-to-trusted ceiling transition must be portable. Either any valid
recovery-authority succession lifts the ceiling by definition, or the operation
must carry trusted-client provenance evidence. Replicas cannot infer the UI or
machine that emitted identical signed bytes. Hosted planes cannot currently
install `c.zone_policy`; explicitly state that they remain on the budget posture
after enrolling more browsers, or add a ceiling-safe explicit transition by
owner ruling.

**Closure test:** all control decisions derive from signed portable inputs;
generation/enrollment churn cannot reset authority or grow Frontier without a
defined bound; and revocation remains encodable at maximum legal scope.

### 4. Repair wrapper nonce safety, rewrap proof, recipient context, and custody

HKDF-compressing item addresses to 96-bit nonces leaves an unbounded
probabilistic cross-item collision risk under one AES-GCM key. Prefer a
domain-separated per-item wrapping key used for exactly one encryption with a
fixed nonce; otherwise define a conservative invocation bound, collision
handling, and mandatory rotation.

Replace “H_frontier-style” survivor hashing with a closed, dedicated commitment
at a frozen fence/control frontier. Bind rotation op/epoch and sorted
`(item_addr, verified current-wrapper hash)` pairs, not addresses alone. Define
durable old-KEK deletion, retry, delete-before-tombstone recovery, and removal
of local backup envelopes.

`KekWrap` must identify the recipient certificate/KEM key. Enrollment wraps
need zone and epoch. Freeze `new_epoch = current + 1`, fresh KEK generation,
HPKE external AAD, and 48-byte ciphertext/tag layouts for wrapping 32-byte
keys.

D-38 is still a policy sentence. Either normatively import the exact existing
vault-envelope version or define root envelope/KDF/AEAD/domain/recovery
semantics here; otherwise narrow Gate A and explicitly assign the format to
Gate B. Close `evrec`, locator, survivor, and recovery-info hash/KDF inputs too.

**Closure test:** every GCM key/nonce use is unique by construction, survivor
completion proves actual current wrappers, every recipient can select one KEM
key/context, and every claimed wrapped/hash object has canonical bytes.

### 5. Make storage transactions physically representable and corruption-safe

`m.export.release + PendingXfer` cannot inhabit one frame: `Batch` accepts only
ItemCommits and PendingXfer is a separate type. Add a dedicated transactional
frame or a closed subrecord union, plus `XferDone {export_id}`; `OutboxMark`
cannot identify which transfer clears.

Freeze Batch ordering, same-zone/writer rule, contiguous sequences/hash links,
exact two-item assert shape, all-or-nothing validation, and outbox advancement.
Apply the 1-MiB frame cap explicitly to the complete encoded Batch.

Correct SYNC byte/integer order and state that payloads are canonical CBOR of
their registered CDDL. A trailing CRC does not detect an upward-corrupted final
length before the scanner reaches EOF; add an immediately checkable header
checksum/redundant length or equivalent evidence. Define bad header/SYNC
behavior and durably flush truncation before append.

Keep platform adapters in Gate B, but freeze honest abstract semantics for
durable key deletion and commits. A Linux persistent keyring alone is not a
universal durable-keystore guarantee.

**Closure test:** every logical transaction maps to encodable frames, every
crash point converges, and committed corruption is never classified as a torn
tail solely because its length moved the CRC.

### 6. Make Memory authority rows disjoint and audit executable

Align safe-human operations with policy counting, then close adjacent bypasses:

- require direct-human/owner evidence for `pin.safe` and `erase.request`;
- add `pin.safe` unpin—hosted writers currently cannot self-unpin;
- constrain safe pin destination and role so it cannot become instruction-
  grade auto-context;
- exclude raise/declassify from generic `judge.full`, preserving flags and
  quotas;
- consume `curate.instruction` for procedure/preference acceptance, pins, or
  graduation;
- give author retract/supersede exact verbs, including assert-only authors;
- define separate portable rows and deterministic linkage for the claim and
  self-accept halves of assert.

Audit needs one coherent lane: add `audit.write` and a genesis/service grant,
or a separate system proof arm. Define daemon and zero-daemon/browser writers,
budget/lineage, fail-open versus fail-closed reads when audit append fails, and
prevent recursive auditing of audit reads.

**Closure test:** every Memory variant selects one non-bypassable authority row;
hosted humans can safely accept/retire/pin/unpin/erase; agents cannot borrow
human rights; and audit writes pass portable admission on every claimed lane.

### 7. Finish status, temporal, erase, evidence, and transfer reducers

For declassification, select causally maximal qualifying judgments first, then
take the conservative higher class only among concurrent maxima. The current
maximum over all qualifying judgments prevents a later sequential lowering.

Define `evidence_effective_floor`: verify the source claim's effective class at
a named frontier or return `sensitive`. Canonicalize locators under a declared
domain. Claimant-supplied `class_floor` is not verification.

Apply `valid_from_ms` in view eligibility. Define whether an expired or
future-valid replacement revives its predecessor; `status(..., as_of)` currently
does not use `as_of`. Add erase-request exclusion to the normative read/search/
auto-context predicate, and define judgments/pins targeting a cryptographically
erased claim, evidence references to it, and supersession/revival edges—normally
target-dependent effects become inert while a visible tombstone remains.

Make the export bundle a closed schema with deterministic redaction, record
mapping, persistence, and digest. Define how up to 128 released records map to
one or more imports and cryptographically bind each imported statement to a
released record. Require equality for duplicated `mimport` top-level and
`provenance.import` fields. Key replay by `(from_plane, export_id)`.

The interim cross-plane mediator has no signed attestation input. Fail closed
on cross-plane import until D0-B, or define its portable proof. Release expiry
needs signed receipt/frontier/`as_of` semantics rather than validator wall time.

**Closure test:** two replicas derive identical status, revival, erase,
classification, evidence, and transfer state from the same portable bytes and
explicit frontier/`as_of_ms`.

### 8. Publish valid conformance artifacts and run Gate A

Replace the invalid fenced JSON with a valid closed Draft 2020-12 schema. Pin
family/outcome/disposition enums, field types, unknown-field behavior, and
family-specific inputs. Specify ChaCha20 variant, nonce, counter, byte order,
and draw consumption.

Turn the surface list into a family×surface matrix. Keep portable format/model
tests in Gate A and production keystore/flush adapters in Gate B. Add caps—or
one exact enclosing-object/count rule—for audit results, erase targets,
rotation manifests, revocation grants, recovery cutoffs, and bundles.

Publish literal policy bytes/hashes and all parameterized constant derivations.
Then create `owner-plane-core`, the vector corpus and harness, run the offline
confirmation, make every named lane green, and conduct the final prose↔vector
audit.

**Closure test:** schemas parse, constants are literal, all vectors have one
meaning and one named surface, and no fixture invents normative behavior.

## Secondary exactness pins

Before vectors freeze accidental behavior:

- reconcile shape-1 “human on daemon” with O4's browser/native/mobile evidence;
- describe `mobile-attested` as root/owner-attested unless evidence is parsed
  and platform-verified;
- exempt `assert_req` from the reserved-ID prefix rule or define deterministic
  retry;
- require `connect_service_key` iff Connect time is enabled;
- constrain `recovery_pk` to 32 bytes;
- define array sorting/dedup for grants, wraps, cutoffs, flows, and compounds;
- distinguish `storage-io` from corruption/rebuild disposition;
- make PendingXfer clearing depend on a matching destination import, not a
  lineage-only OutboxMark;
- define semantic required/forbidden fields for tagged receipt/control variants.

## Recommended repair order

1. **Genesis/policies:** exact home/audit/grants, relational policy model,
   literal bytes and hashes.
2. **Authority:** service/device receipt issuers, witness/frontier binding,
   renewal history, finite solo budgets, control/requester/epoch semantics.
3. **Crypto/storage:** per-item wrap safety, wrapper-set proof, recipient
   context, durable KEK deletion, encodable transactions, final framing.
4. **Memory:** disjoint verbs, safe unpin/human checks, assert halves, audit,
   temporal/erase/evidence/export reducers.
5. **Conformance:** valid schema, exact RNG, caps, family matrix, core and
   vectors.
6. **Final audit:** only after every required lane is green; fold any behavior
   invented by fixtures back into the normative document first.

## Gate-A go-ahead

Give the go-ahead only when:

- genesis deterministically creates a usable, correctly classified hosted and
  trusted plane;
- policy bytes can express and hash every counting relation;
- device and service time proofs survive renewal/rollback and bind stable
  policy state;
- all control, lineage, epoch, and revocation transitions are portable and
  encodable;
- GCM uniqueness and survivor-wrapper completion are protocol-enforced;
- every logical storage transaction has exact crash-safe frames;
- Memory authority rows cannot bypass each other and every reducer branch is
  deterministic;
- root/evidence/bundle/policy/hash objects are closed;
- the conformance schema parses, the core/corpus exists, all vectors and the
  offline scenario are green, and the final discrepancy audit finds only
  editorial drift.

## Bottom line

The peer review correctly identifies the policy/genesis last mile and several
valuable pins. It reinforces the conclusion that no new architecture round is
needed. It does not, however, supersede the broader wire/crypto/storage/Memory
findings in Review 1; its resolution table calls several mechanisms complete
before their bytes or transitions are actually verifiable.

V0.3 is the right foundation. Cut one comprehensive but focused v0.4 repair,
then let executable vectors decide Gate A. Until that proof exists, do not
freeze the protocol or authorize durable P1 Memory writes.
