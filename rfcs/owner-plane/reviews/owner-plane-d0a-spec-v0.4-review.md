# Review: D0-A Core + Memory normative specification v0.4

*2026-07-12. Strict closure audit of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.4 against
[`owner-plane-d0a-spec-v0.3-synthesized-review.md`](/Users/vm/owner-plane-d0a-spec-v0.3-synthesized-review.md).
Independent passes covered protocol/control/time, Memory/IAM, and
crypto/storage/conformance. This asks whether canonical vectors can now be
implemented without inventing protocol behavior; it does not reopen the
umbrella architecture.*

## Executive verdict

**V0.4 is a substantial and successful repair, but it is not yet prose-ready
for Gate A. Cut one focused v0.5 authority/reducer patch before freezing vector
bytes.**

The revision closes a great deal correctly:

- per-item derived wrapping keys remove the shared-key 96-bit nonce problem;
- recipient wraps now bind plane, zone, epoch and recipient KEM key;
- receipt variants are closed and bind plane/zone;
- stable witness device IDs, optional certificate deadlines, finite solo
  budgets, renewal cutoffs, and initially pending proofs now exist;
- home/audit spaces and policies are pinned, with a conservative private home;
- policy objects have closed relation/verdict enums and literal hashes;
- the stated policy encodings and hashes independently recompute exactly;
- safe pin/unpin/erase, instruction-grade checks, sequential declassification,
  temporal revival, erased status and cross-plane fail-closed all landed;
- `Txn`, PendingXfer/XferDone, `nlen`, canonical frame payloads and durable tail
  truncation resolve the main physical-format complaints;
- the fenced JSON Schema is valid JSON and a real family×surface matrix exists.

This is now recognizably a protocol, not an architecture sketch.

The remaining issues are fewer but still freeze-stopping. Several operations
cannot be verified or encoded under their own closed vocabularies, and several
security transitions use the wrong ordering domain. The clearest examples:

- Connect receipts name only a `key_id`; no public key/certificate or service
  key lifecycle lets a replica verify the signature.
- `audit.write` is required by genesis and `m.audit` but absent from the closed
  verb vocabulary, so the mandatory audit grant is invalid.
- compromise revocation cuts a tenant writer head, which cannot order receipt
  `issuer_seq` values.
- revocation rotation references neither cover every zone nor compose with the
  hosted ceiling and 16-reference cap.
- policy `relation` is not consumed by the status fold, and “author” means same
  device rather than the same actor/principal.
- one export may create 128 imports, but replay and completion key only on one
  `export_id`, so import two is a duplicate and one XferDone can falsely mark
  the bundle complete.
- the erase manifest lacks enough information to deterministically derive its
  tombstones.

Gate A is also mechanically impossible today: `owner-plane-core`, the vector
corpus and harness do not exist, the offline-confirmation result is open, and
the final prose↔vector audit cannot run.

Recommended disposition:

- **Architecture/direction:** accepted; no reopening.
- **V0.4 as implementation foundation:** yes.
- **Canonical vector freeze:** not yet.
- **Next step:** one v0.5 authority/reducer closure patch, then vectors.
- **Durable P1 Memory writes:** still prohibited.

## What should be retained

Do not re-litigate the following v0.4 choices:

1. **Per-item wrap keys.** Domain-separated single-use wrapping keys with a
   fixed nonce are the right deterministic construction.
2. **Explicit root-custody boundary.** D0-A freezes the abstract mandatory
   passkey/recovery-envelope contract; exact envelope bytes are consciously a
   Gate-B artifact under D-46.
3. **Private personal home.** `home = personal/private/workflow-v1` plus safe
   human acceptance is a coherent hosted-diary posture.
4. **Portable ceiling lift and hosted budget posture.** Any valid recovery
   succession lifts by authority; hosted planes cannot silently upgrade time
   policy.
5. **Txn and IPLOG2 direction.** Typed transactional subrecords, transfer
   completion, redundant length and quarantine semantics are sound.
6. **Policy relation shape and constants.** A relation field is useful and the
   published v0.4 policy hashes are correct; the remaining problem is relation
   semantics and consumption.
7. **Memory temporal/erase direction.** Future validity, revival, terminal
   erased status and immediate retrieval exclusion belong in the normative
   reducer.
8. **Valid vector schema and surface matrix.** These are now credible starting
   points for the eventual corpus.

The findings below complete these decisions rather than replacing them.

## Freeze blocker 1: signed time still lacks a complete service and history model

### Connect signatures remain unverifiable

The service issuer arm and `ZonePolicy` contain only a `key_id`. Neither carries
the algorithm/public key, a service certificate, a resolver, nor a rotation/
succession chain. `Signed<T>` likewise has no signer descriptor. A vector author
must invent how `key_id` becomes a verification key.

Define a closed service-key descriptor/certificate and its control-governed
history, or include `{alg, pk}` and verify `key_id = H_key({alg,pk})`. Specify
rotation and which historical keys remain valid.

T3 scopes counters by `(device_id, signing key)`, which does not exist for a
service issuer. Define the service scope, counter persistence, rollback and key
rotation recovery. For devices, require certificate renewal to use a fresh key
before claiming that the sequence restarts at 1.

### Receipt/lease qualification is not frozen to policy state

Receipt bytes contain plane and zone but no witness-policy hash/control
frontier. T2/T4 consult fold-current policy, so removing a witness or rotating
Connect can retroactively reinterpret accepted receipts. Either bind each proof
to an accepted policy version/frontier or explicitly freeze and test the
intended retroactive rule.

T5 requires a “valid” lease but never says who may issue one, whether the
operation signer may self-issue it, or whether `time_witnesses` applies. Define
lease issuer qualification independently of the separate acceptance receipt.
Declaring `LeaseStmt.ctrl_frontier` diagnostic is acceptable; the remaining
issuer rule is not.

`accept_connect_time` and the literal `"connect"` in `time_witnesses` can
disagree. Collapse them to one authority predicate or give their exact Boolean
relationship. Define absent `time_witnesses` semantics under E4.

### Renewal and compromise use the wrong cutoffs for receipts

`history_cutoffs` order tenant operations, not receipts signed by the old key.
Without a per-key accepted-through `issuer_seq`, supersession either invalidates
historical proofs or lets an old key mint late backdated receipts.

Similarly, T4 says compromise revocation invalidates receipts “from its cutoff
forward,” but `c.revoke_device.cutoff` is a tenant writer head with no ordering
relation to receipt sequence. Add receipt-key sequence cutoffs. Require renewal
cutoffs to commit only operations already admitted at the pre-renewal frontier;
otherwise renewal can bless previously pending deadline/lease operations merely
because they fall below a head.

### Fail-closed and pending-proof transitions do not yet converge

`require_cert_deadlines` constrains new enrollment only. Existing deadline-free
certificates are grandfathered, and new deadline-free grants remain possible;
the supposedly fail-closed zone can therefore continue indefinitely on the
non-time lane. Either rename this posture honestly or require deadline/budget
rules at operation and grant admission.

A tenant checkpoint cannot prove that no receipt or lease proof will later
arrive because it carries no receipt/lease sequence frontiers. Hardening
`deadline-unreceipted` or `lease-missing` at that GC fence can make replicas
terminally disagree. Add proof-feed cutoffs/acks or keep the operation pending.

**Closure test:** device and service signatures resolve from canonical control
state; proof qualification cannot change accidentally under a later policy;
renewal/revocation delimit both operation and receipt histories; and delayed
proofs converge on every replica.

## Freeze blocker 2: control, revocation, lineage and epoch transitions still cross domains

### Common control-header values remain unpinned

Control operations still carry tenant-oriented `authored_kek_epoch` and
`capability_epoch` without reserved values. Pin them, plus genesis/admin/
recovery signer/actor combinations, initial previous hash and control request-ID
rules in the registry. Fixtures should not choose these bytes.

### Cutoffs need zones and receipt scopes

Writer chains are per `(zone, lineage, generation)`, but `c.cutoff`,
`c.abandon_writer`, `c.revoke_device.cutoff`, and recovery tenant cutoffs omit
zone identity. One device lineage may hold write grants in several zones. Make
these per-zone sets/frontiers. Receipt cutoffs are a separate per-issuer-key
sequence domain and must not reuse a writer head.

### Rotation references do not yet make revocation executable

`rotation_refs` may be empty; need not cover every decryptable zone; need not
exclude the target key; and are capped at 16 without a 16-zone device bound.
Separately accepted `c.kek_rotate` operations take effect before the later
“atomic” revocation, so references are not a prepare/commit protocol.

Define staged rotations whose effect activates with revocation, or explicitly
accept early exclusion and freeze the coverage/ordering rules. Every zone must
be covered exactly once, each rotation must omit the target, and larger legal
devices need an encodable continuation.

This also conflicts with hosted revocation: `c.revoke_device` is hosted-
admissible, but its required separately committed `c.kek_rotate` operations are
not clearly inside the hosted control ceiling. State the non-erase rotation
exception or hosted revocation still cannot complete.

### Lineage reauthorization and unknown recovery remain ambiguous

`c.enroll` still says it may reauthorize a lineage while renewal says it does
not touch lineage state and the dedicated `c.lineage_reauth` now requires a
requester signature. Pick one path.

The requester signs only `{lineage,max_generations}`. Bind plane, device/cert,
current window/control frontier and request ID so one old signature cannot open
unlimited future windows. Define whether `max_generations` is absolute or a new
per-zone/per-lineage window and how the counter advances.

`last_known="unknown"` preserves old heads until admin cutoff. Hosted planes
cannot issue cutoff/abandon operations, so repeated browser loss still grows
Frontier to its cap. Renewal also supplies one head while multiple unknown heads
may be live. Define a hosted-safe consolidation/cutoff or an explicit
frontier-valued renewal rule.

### Grant issuance invariants remain implicit

Require a new write grant's `capability_epoch` to equal the zone epoch at
issuance, tie `issued_admin_epoch` to the enclosing control epoch, and reject
future grant epochs before subtraction. `flow.kinds` also needs the same absent
semantics now given to `grant.kinds`.

Finally, `c.drill` still says portable admission accepts it only when authored
on a trusted lane, which signed bytes cannot prove. Keep trusted-lane execution
as product guidance; recovery authority is the portable predicate.

**Closure test:** every control header and cutoff has one domain; all legal
devices can be revoked; hosted continuity terminates old heads; and no reusable
attestation or epoch omission can mint unintended authority.

## Freeze blocker 3: audit and genesis are still not valid closed operations

`audit.write` is required by `c.genesis` and the `m.audit` registry row but is
absent from both closed verb vocabularies. The mandatory `audit_grant` therefore
cannot inhabit `grant.ops`. Add the verb, class/hosted ceiling treatment and
system-only admission rule.

Audit then has three more contradictions:

- O5 reserves `actor.kind = service` for daemon-internal writers, while the
  registry claims zero-daemon browsers can write audits.
- Genesis carries one lineage, yet describes a separate-sounding service
  lineage for the audit grant; require equality with the device lineage or add
  an explicitly authorized second lineage.
- §11.7 still says audits use the daemon service writer, conflicting with the
  browser path.

State overflow behavior when a search returns more than the 256 audit-result
cap, whether audit-space reads are audited, and precisely when a read result may
be released after the audit append.

Genesis also remains schema-valid with zero budgets or zero generation window.
Pin minimum or exact initial budget/window values, initial certificate classes
by provenance, grant spaces/epochs/flags, wrap recipient equality, and all
descriptor/cert/zone/space/lineage/grant cross-field equalities. E4 is stale:
it still says `max_generations` is required on write grants even though v0.4
deliberately moved the field to `lineagedef`.

**Closure test:** every valid genesis boots by construction—not merely by a
friendly fixture—and both daemon and zero-daemon audit writes pass the same
portable admission model.

## Freeze blocker 4: Memory relation and compound authority remain under-specified

### Policy relations are encoded but not consumed

`authorized(j)` still lists only `(verdict, kind, space_class, actor_class)` and
omits `rule.relation`. Add relation evaluation to the normative fold.

The current `author` definition means “same device,” while the registry promises
the actor's own claim. Multiple sessions and humans can share a controller
device; device equality lets one retract/supersede another's work. Define a
canonical authoring principal from signed actor fields and compare that exact
principal. Define the exact fields for `self` as well.

The operation table still requires `propose` for author retract, excluding an
assert-only author. Author supersession names no closed capability verb, and the
policy counts only session-author supersession while the registry admits a
broader author branch. Align capability, admission relation and counting.

### Assert and personal safe-accept need portable two-half rules

The workflow assert compound contains two independently signed operations, but
portable admission remains per operation. Define the claim half and judgment
half separately, the verb each cites, deterministic linkage, and behavior when
only one arrives. A claim under an assert-only grant cannot simply become an
ordinary propose after losing its companion.

D-40's personal propose+safe-accept can physically share a Txn, but no pinned
compound/linkage rule says when this is one idempotent action versus two
independent writes. Either define it or remove the “one Txn” promise.

`mclaim.supersedes[]` remains unconsumed; explicitly mark it advisory and define
its projection role, or remove it. Retract and retire both derive `retired` but
need a pinned projection field if they “surface distinctly.”

### Human and portable actor evidence still disagree

§10.1 and D-47 include daemon-class direct-human actions; O4 allows only
browser/native/mobile classes. Choose one. Make session/external/peer actor
classes derivable from signed controller attestation at the portable fold,
without depending on another daemon's live token state.

**Closure test:** every judgment evaluates one canonical relation and one
capability, compound halves remain independently verifiable, and no shared
device identity grants cross-principal authorship.

## Freeze blocker 5: evidence, transfer and erase still need exact hashed inputs

### Evidence has contradictory hash algorithms

The same `locator_hash` is defined first as raw SHA-256 and later as
`H_evrec(canonical locator text)`; those bytes differ. Keep the domain-separated
form only. Name the external content digest algorithm.

Operationalize taint depth one without recursive classification walks or cycles.
For cross-plane plane refs, carry or defer the foreign stamped frontier; absent
foreign proof must take the unresolved→sensitive path.

### Multi-record transfer cannot complete correctly

One release can yield up to 128 imports, but replay keys only on
`(from_plane,export_id)`, so import two is a duplicate. One PendingXfer/XferDone
pair likewise cannot prove all records arrived. Key import and completion by
`(from_plane, export_id, source_op)` or commit to an exact completed-record set.

Define where bundle bytes are durably stored for crash recovery; add
`bundle`/`bundlerec` to Appendix A; require each import's class floor to equal
the bound record; and enforce destination header zone/space against the release
endpoint. Pin the source data/control frontier and `as_of_ms` used to derive
release classification.

Consume `flow.expiry_deadline_ms` and define release-expiry evidence for import.
Do not create PendingXfer for model/embedding/reflection egress, which has no
destination zone.

### Erase manifest cannot derive its tombstones

`erase_manifest` is `[bytes32]` without saying whether entries are erase-request
hashes, target operation hashes or item addresses. Recovery later claims it can
derive `{item_addr, erase_op}` tombstones from that list. Replace it with a
closed sorted record containing the necessary identities; define
`retired_epoch` and duplicate handling.

The survivor commitment still lacks a closed `survivor_set` CDDL and frozen
membership frontier. Its `wrap_hash` uses raw SHA-256 despite the closed-domain
policy, and `ItemWrap.wrapped_dek` remains unconstrained rather than 48 bytes.

**Closure test:** every evidence/export/erase hash has one canonical input;
multi-record transfer tracks every record; and crash recovery derives identical
tombstones and wrapper completion on every implementation.

## Freeze blocker 6: conformance scaffolding is improved but Gate A remains future work

The JSON Schema now parses and the family×surface matrix is useful. The policy
lengths/hashes also independently recompute exactly:

- `workflow-v1`: 1133 bytes,
  `219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c`;
- `owner-v1`: 571 bytes,
  `d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486`.

Before corpus generation, make RNG draw order explicit as an array/sequence;
JSON object property order is not a portable draw order. Close expected outcome
and disposition enums or require the harness to validate them against §10.
Add the missing CDDL shapes and exact conditional/set constraints identified
above.

Then create `owner-plane-core`, the vector corpus and harness, run the offline
scenario, make every required lane green, and conduct the final discrepancy
audit. None of those mechanical Gate-A steps exists yet.

**Closure test:** a fixture author chooses data, not protocol behavior; all
schemas and hashes validate independently; every required lane is executable
and green.

## Recommended v0.5 repair order

1. **Time/service authority:** service-key certificates, proof-policy binding,
   receipt counter cutoffs, renewal semantics, fail-closed and pending-proof
   convergence.
2. **Control/revocation:** header constants, zone-specific cutoffs, staged
   rotation coverage, hosted revocation, requester/window accounting and
   unknown-head consolidation.
3. **Genesis/audit:** add the verb and portable browser/daemon actor path; pin
   nonzero boot values and cross-field invariants.
4. **Memory authority:** consume relations, identify principals, close author/
   assert compounds and projection distinctions.
5. **Evidence/transfer/erase:** one hash domain per field, record-level transfer
   identity, bundle persistence, expiry semantics, typed erase and survivor
   manifests.
6. **Vectors:** only after those choices are normative; then implement the core
   and corpus and run the discrepancy audit.

## Gate-A go-ahead

The prose is ready for vector freeze only when:

- device and service proofs verify and converge across renewal, policy changes,
  revocation and feed reordering;
- every cutoff names the domain it orders, and every legal device can be
  atomically excluded from all zones;
- hosted unknown-generation recovery remains bounded;
- every valid genesis has nonzero usable grants/lineage and a valid audit lane;
- policy relations compare portable principals rather than shared devices;
- assert/safe compounds remain valid when replicated as individual operations;
- evidence, bundle, transfer, erase and survivor objects have closed identities;
- all vector artifacts exist and pass on their named surfaces.

## Bottom line

V0.4 is the strongest and most implementable draft so far. It genuinely closes
the cryptographic wrapper, policy-constant, framing, temporal and hosted-diary
work from v0.3. Its remaining defects are now concentrated in authority-domain
joins: device versus service receipts, writer versus receipt cutoffs, device
versus principal authorship, one export versus many imported records, and erase
requests versus item tombstones.

That is excellent convergence, but those joins are exactly where independent
implementations fork. Cut one focused v0.5 repair before canonical vectors; do
not declare Gate A or enable durable P1 writes yet.
