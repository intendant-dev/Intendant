# Synthesized review: D0-A Core + Memory normative specification v0.4

*2026-07-12. Synthesis of
[`owner-plane-d0a-spec-v0.4-review.md`](/Users/vm/owner-plane-d0a-spec-v0.4-review.md)
and
[`owner-plane-d0a-spec-v0.4-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.4-review-2.md),
adjudicated against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.4 and the
v0.3 synthesized go-ahead criteria. This is a wire/reducer conformance review,
not another umbrella-architecture round.*

## Executive verdict

**Do not declare Gate A or freeze canonical vectors yet. V0.4 is a major,
successful repair, but it needs one focused v0.5 authority/reducer closure
pass first.**

The peer report is valuable. It finds the strongest omission in Review 1:
ordinary space-scoped grants cannot unambiguously authorize `w.gen` at
`SYS_SPACE`. It also sharpens the audit transaction consequences and the
missing initial capability epoch. Its general conclusion, however, is too
optimistic. Calling the rest “editorial-scale” repeatedly treats the existence
of a closed shape as proof that the shape has one executable security meaning.
Several examples disprove that:

- a service receipt contains a `key_id` but no resolvable verification key;
- a revocation references already-effectful rotations, which is not an atomic
  prepare/commit protocol;
- a policy rule carries `relation`, but `authorized(j)` does not evaluate it;
- `Txn` and `XferDone` are encodable, but their singular transfer identity
  cannot complete a release containing many records;
- `erase_manifest` is encodable as hashes, but those hashes cannot derive the
  tombstones recovery requires.

These are not stylistic defects. Independent implementations would make
different authority or state-transition decisions.

The accurate combined disposition is:

- **Architecture and protocol direction:** accepted; do not reopen.
- **V0.4 as the implementation foundation:** yes.
- **Low-level core/vector scaffolding:** may begin in parallel where semantics
  are already stable.
- **Canonical vector bytes and reducer freeze:** not yet.
- **Gate A:** no-go until the v0.5 repairs and executable corpus are complete.
- **Durable P1 Memory writes:** remain prohibited.

The remaining work is still bounded. It is one protocol-closure pass, not a
new design program, but it requires several real owner decisions rather than
only the peer report's two suggested rulings.

## Assessment of the peer review

### What it adds and should be adopted

1. **`w.gen` needs an explicit scope exception.** The operation is fixed to
   `SYS_SPACE` and described as an implicit right of every write-capable grant,
   while the generic admission algorithm still applies the grant's `space`
   axis. A grant limited to real spaces therefore has two plausible outcomes.
   State that `w.gen` bypasses the spaces axis and the inapplicable kind axis
   while retaining tenant, zone, grant, lineage, generation and budget checks,
   then add the peer's proposed family-10 vector.
   ([§9.3](/Users/vm/owner-plane-d0a-spec.md:944),
   [§10.2](/Users/vm/owner-plane-d0a-spec.md:1001))
2. **Audit availability is product-visible authority.** A read-only lock loser
   cannot append the mandatory audit row, and a finite audit budget can darken
   sensitive reads. The specification must either accept and name those
   outcomes or provide a separate bounded audit lane.
   ([browser locking](/Users/vm/owner-plane-d0a-spec.md:733),
   [`m.audit`](/Users/vm/owner-plane-d0a-spec.md:1094))
3. **Capability epoch starts are missing.** KEK epoch 1 is pinned, but that does
   not itself initialize the distinct capability epoch. Pin capability epoch 1
   at genesis and zone creation.
   ([§9.4](/Users/vm/owner-plane-d0a-spec.md:961),
   [`cgenesis`](/Users/vm/owner-plane-d0a-spec.md:1619))
4. **Several exactness pins are sound:** service issuer counter scope,
   `recovery_pk .size 32`, control-array set/dedup rules, requester-attestation
   replay binding, hosted unknown-head honesty, the 16-rotation-reference cap,
   constrained vector outcome/disposition values, the broken §11.9 reference,
   and import-expiry policy selection.

### Where its resolution audit is too optimistic

| Subject | Peer conclusion | Adjudicated conclusion |
|---|---|---|
| Service receipts | Tagged issuer + policy key resolves Connect | `key_id` is only an identifier; no public key, algorithm-bearing descriptor or history exists, so the signature cannot be verified |
| Renewal/time | `history_cutoffs` discharge signed-time history | They delimit tenant operations, not old receipt-key `issuer_seq`; lease qualification and proof-policy binding also remain open |
| Revocation | Rotation references provide pending atomicity | Referenced rotations take effect independently, may be empty/incomplete, and carry no target-exclusion or all-zone coverage invariant |
| Policy relation | D-45 is resolved | The bytes can encode a relation, but the normative status authorization tuple omits it |
| Transfer | `Txn + XferDone` closes the journal | One export permits 128 imports while replay and completion use one `export_id`; import two is a duplicate and completion one can clear the whole transfer |
| Audit | One missing verb line remains | The missing verb is the first failure; actor class, hosted admission, lineage, budget, lock and result-release behavior also conflict |

The peer report's G1, G3, G4, G5 and most of its pins should therefore be
folded in, while its “patch then go” scope should not.

Two peer pins should be rejected. First, the displayed `workflow-v1` rules
**are** in the specification's
[RFC 8949 Core Deterministic](https://www.rfc-editor.org/rfc/rfc8949.html#section-4.2.1)
bytewise-lexicographic set order. The 1133-byte encoding and
`219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c`
hash independently reproduce. `owner-v1` likewise reproduces at 571 bytes and
`d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486`.
Do not reorder either policy under the older length-first scheme.

Second, no extra policy-hash “precedence flip” is needed. Appendix B makes the
literal object authoritative and requires the digest to reproduce; M3 freezes
both at Gate A, whose discrepancy audit already fails on a mismatch. Making an
incorrect printed digest override its literal source object would be worse.

## What v0.4 genuinely resolves

Retain these decisions and implementations:

- per-item derived wrapping keys with fixed nonces;
- recipient wraps bound to plane, zone, epoch, device and KEM key;
- four closed receipt variants bound to plane and zone;
- stable witness device IDs and explicit diagnostic lease frontier;
- finite budgets in budget-posture zones;
- tagged renewal cutoffs and explicit revocation modes as the right general
  shape, subject to the ordering-domain repairs below;
- private personal home plus safe-human diary acceptance;
- literal relational policy objects and the two verified policy constants;
- safe pin/unpin/erase restrictions, temporal revival, terminal erased state,
  sequential declassification and cross-plane import fail-closed;
- typed `Txn`, `nlen`, canonical frame payloads and durable tail truncation;
- a valid JSON Schema shell and useful family-by-surface matrix;
- the abstract root-custody contract with exact envelope bytes consciously
  assigned to Gate B under D-46.

The findings below complete these choices rather than replacing them.

## Consolidated freeze blockers

### 1. Genesis, audit and generation opening must form one bootable authority path

`audit.write` is required by genesis and the Memory registry but absent from
both closed verb vocabularies. Consequently the mandatory `audit_grant` cannot
inhabit `grant.ops`; strict decoding rejects the advertised genesis.
([genesis row](/Users/vm/owner-plane-d0a-spec.md:757),
[Memory vocabulary](/Users/vm/owner-plane-d0a-spec.md:1070),
[CDDL verb](/Users/vm/owner-plane-d0a-spec.md:1515))

Adding the token is necessary but not sufficient. Pin all of the following:

- whether `audit.write` is a system-only verb and its hosted/device-class
  ceiling treatment;
- one portable actor rule for daemon and zero-daemon browser audit writers;
  O5 currently reserves `service` for daemon-internal writers while the
  registry allows any device class;
- whether the audit grant uses genesis's single device lineage or a second
  service lineage—the body contains only one lineage;
- audit-budget behavior, read-only-lock behavior, overflow/chunking above 256
  result IDs, and the exact point at which read results may be released;
- a dedicated `audit-unavailable` outcome if no existing closed outcome
  accurately represents failure to append;
- capability epoch 1, positive usable initial budgets and generation window,
  certificate/provenance constraints, and descriptor/cert/zone/space/wrap/
  lineage/grant cross-field equalities;
- removal of E4's stale statement that `max_generations` is required on write
  grants, since v0.4 moved it to `lineagedef`.

Finally, make the `w.gen` `SYS_SPACE` exception explicit. “Implicit right” is
not enough while the normative algorithm still evaluates `scope-space`.

**Closure test:** every schema-valid genesis is usable rather than merely the
friendly fixture; its first claim and audit succeed; a space-scoped writer can
open generation 2; and every audit failure has one portable outcome.

### 2. Signed time needs a verifiable service and proof-history model

The service issuer arm contains only `key_id`. `key_id = H_key({alg,pk})`
cannot recover `{alg,pk}`; neither `ZonePolicy` nor another control object
supplies the verification key, service certificate, rotation or historical
key validity. Gate-A fixtures would have to invent a resolver.
([key identity](/Users/vm/owner-plane-d0a-spec.md:155),
[issuer union](/Users/vm/owner-plane-d0a-spec.md:443))

Freeze a closed service-key descriptor/history and then resolve these related
issues:

- define service `issuer_seq` scope, persistence, rollback recovery and key
  rotation; for device renewal, require a fresh signing key before permitting
  a counter restart;
- bind receipt qualification to an accepted witness-policy version/control
  event, or explicitly choose and test retroactive reinterpretation when
  witnesses or Connect keys change;
- define who may issue a `LeaseStmt`; T5 defines a qualified accept receipt but
  only calls the lease “valid”;
- collapse `accept_connect_time` and `"connect" in time_witnesses` into one
  exact authority predicate;
- add per-receipt-key accepted-through `issuer_seq` cutoffs. Tenant writer
  heads cannot delimit old-key receipts, and a compromised witness receipt has
  no ordering relationship to the revoked device's writer cutoff;
- state whether renewal deliberately ratifies every previously pending
  deadline/lease operation below its tenant cutoff or only preserves operations
  already admitted at the pre-renewal frontier;
- make checkpoint hardening convergent by committing proof-feed cutoffs, or
  keep missing-proof operations pending. A tenant-operation fence cannot prove
  that no receipt or lease will arrive later.

The peer is right that certificate grandfathering is now explicit. It need not
be silently reversed. But the name `deadline_fallback = "fail-closed"` must
match its actual rule: either it governs old deadline-free certificates and
grants, or it is renamed/scoped to future enrollment and the grandfathered
lane is a tested residual.

**Closure test:** a replica obtains every device/service verification key from
canonical control state; receipt authority cannot move accidentally under
later policy; renewal and compromise delimit operation and proof histories in
their own ordering domains; delayed proofs converge.

### 3. Control cutoffs, revocation, lineage recovery and epochs cross domains

Writer chains are per `(zone, lineage, generation)`, but several cutoff bodies
name only lineage/head. Because control headers use `CTRL_ZONE`, the target
zone cannot be recovered from the envelope. Add zone identity to `c.cutoff`,
`c.abandon_writer`, revocation cutoff and recovery tenant cutoffs; keep receipt
cutoffs separate.
([chain domain](/Users/vm/owner-plane-d0a-spec.md:417),
[control CDDL](/Users/vm/owner-plane-d0a-spec.md:1630))

Rotation references also need executable compound semantics:

- require nonempty, exact all-zone coverage for every zone the target can
  decrypt;
- require every referenced new wrapper set to exclude the target;
- solve the 16-reference cap for a device with more than 16 zones;
- specify staged prepare/commit activation or explicitly accept early
  exclusion—the independently committed rotations are already effectful;
- state how non-erase exclusion rotations fit the hosted control ceiling.

Lineage/epoch closure still needs:

- one reauthorization path: `c.enroll` still claims it may reauthorize while
  the dedicated `c.lineage_reauth` carries requester proof;
- requester signatures bound to plane, device/certificate, current window or
  control frontier, and request ID so they cannot authorize repeated future
  windows;
- an exact definition of `max_generations` accounting;
- a hosted-safe policy for repeated `last_known = "unknown"` heads, or an
  explicit bounded-residual decision and exhaustion behavior;
- grant `issued_admin_epoch`, zone capability epoch at issuance, future grant
  handling, and `flow.kinds` absence semantics;
- reserved values for tenant-inapplicable fields in control headers and exact
  genesis/admin/recovery signer/actor combinations;
- removal of `c.drill`'s nonportable “trusted lane” admission predicate. As the
  ceiling-lift text already recognizes, lane of execution is product guidance;
  recovery authority is what replicas can verify.

**Closure test:** each cutoff names exactly one ordering domain; every legal
device can be excluded from every zone; hosted recovery does not exhaust the
frontier; and no reusable attestation or epoch omission expands authority.

### 4. Memory policy bytes exist, but relational and compound authority is incomplete

`authorized(j)` counts only `(verdict, kind, space_class, actor_class)` and
omits the rule's required `relation`. This is a direct normative gap, not a
missing vector.
([status authorization](/Users/vm/owner-plane-d0a-spec.md:1098),
[policy rule](/Users/vm/owner-plane-d0a-spec.md:1724))

Define a portable authoring principal from signed fields and compare that
principal for `self` and `author`. The current text makes `author` the same
device; on a controller device that permits one human/session to retract or
supersede another's work. If device-level authorship is truly intended, ratify
that security consequence explicitly and align “own claims” language.

Then close the operation rows:

- author retract currently requires `propose`, excluding an assert-only
  author;
- author supersession names no closed capability verb;
- the workflow policy counts only a narrower session-author supersession than
  the registry appears to admit;
- define separate portable claim and judgment halves for `assert`, including
  the authority and state of a lone half; reconcile that recovery story with
  the storage rule that a Txn validates and advances all-or-nothing;
- pin linkage/idempotence for D-40's personal propose + safe-accept Txn, or
  remove the one-Txn promise;
- declare `mclaim.supersedes[]` advisory with a projection effect or consume
  it normatively;
- define the projection field that distinguishes retract from retire;
- reconcile daemon direct-human evidence across O4, §10.1 and D-47.

**Closure test:** every judgment selects one capability, one policy row and one
portable relation; compound halves verify independently; shared device custody
does not silently become shared authorship.

### 5. Evidence, transfer and erase still lack exact identities

Evidence has a literal hash contradiction: `locator_hash` is first raw
SHA-256, then `H_evrec` over the same canonical locator. Choose the
domain-separated definition. Also name the external digest algorithm, define
depth-one taint without recursive cycles, and require a verifiable foreign
frontier/proof—or sensitive unresolved handling—for cross-plane evidence refs.
([§11.5](/Users/vm/owner-plane-d0a-spec.md:1144))

Multi-record transfer remains internally impossible as written. One release
maps to as many as 128 imports, but replay and the completion journal key only
on `(from_plane, export_id)`/`export_id`. Track each record, for example by
`(from_plane, export_id, source_op)`, and commit to the exact completed record
set. Also freeze:

- durable bundle-byte storage for crash replay and Appendix-A `bundle`/
  `bundlerec` shapes;
- equality of imported `class_floor` with its bound record;
- destination plane/zone/space binding;
- the source data/control frontier and `as_of_ms` used for release
  classification;
- flow-expiry evidence and whether source or destination witness policy
  qualifies it;
- no `PendingXfer` for model/embedding/reflection egress, which has no
  destination zone.

Erase likewise needs a typed manifest. `[bytes32]` does not say whether an
entry identifies an erase request, target operation or item, yet crash recovery
must derive both `item_addr` and `erase_op`. Replace it with a sorted closed
record and define `retired_epoch`, duplicates and membership frontier. Close
the survivor-set schema and constrain `ItemWrap.wrapped_dek` to 48 bytes.
([erase recovery](/Users/vm/owner-plane-d0a-spec.md:593),
[storage CDDL](/Users/vm/owner-plane-d0a-spec.md:1661))

**Closure test:** every hash has one input domain; each released record has one
import/completion identity; and every replica can reconstruct identical
tombstones and wrapper completion after any crash point.

### 6. Conformance artifacts are improved, not complete

The JSON Schema parses and the policy constants are correct. Before corpus
generation:

- express random draws as an ordered array/sequence; a JSON object's property
  order is not a portable “input order”;
- constrain `outcome` and `disposition` to the closed protocol enums or require
  explicit harness cross-validation;
- mark control arrays as sets with exact sort/dedup keys where order has no
  meaning;
- constrain `recovery_pk`, wrapper lengths and the missing compound schemas;
- retain the current literal-policy-first mismatch rule through the Gate-A
  discrepancy audit.

The peer's policy display-order concern is not a defect, as noted above.

Finally, `owner-plane-core`, the vector corpus and its harness do not yet
exist; the offline fixture remains open. That is expected before implementation
starts, but it means Gate A itself is mechanically impossible today. Stable
encoding/crypto/storage scaffolding may proceed now; reducer fixtures must not
invent answers to the blockers above.

## Owner decisions versus mechanical repairs

The next pass is bounded, but not purely editorial.

Owner rulings are needed for:

1. service-key distribution/rotation and receipt-policy binding;
2. whether renewal ratifies previously pending operations;
3. staged atomic revocation versus explicitly permitted early exclusion;
4. device-level versus principal-level `author` relation;
5. audit behavior under read-only locks and budget exhaustion;
6. record-level multi-import completion identity;
7. hosted unknown-head exhaustion and the exact grandfathered fail-closed
   posture.

Mechanical repairs include `audit.write`, `w.gen` space handling, initial
capability epoch, zone-qualified cutoffs, requester binding, the locator-hash
conflict, typed erase/survivor objects, fixed byte lengths, set semantics,
vector enums and RNG ordering.

## Recommended v0.5 order

1. **Boot authority:** genesis, `audit.write`, audit actor/lineage/budget, cap
   epoch 1 and `w.gen` scope.
2. **Proof authority:** service key history, witness-policy binding, lease
   issuers, receipt cutoffs and proof finality.
3. **Control authority:** zone-qualified cutoffs, staged all-zone revocation,
   reauthorization and unknown-head semantics.
4. **Memory authority:** consume relations, define principals, close author and
   compound operations.
5. **Derived state:** evidence domain, record-level transfer, typed erase and
   survivor commitments.
6. **Vectors:** close schemas and only then freeze corpus bytes; run every named
   surface and fold all fixture-invented behavior back into prose.

## Gate-A go-ahead

Give the go-ahead only when:

- every valid genesis boots with a valid audit lane and generation-2 path;
- device and service proofs verify and converge through policy change,
  renewal, revocation and reordering;
- every cutoff names its zone and ordering domain and revocation covers every
  decryptable zone;
- hosted continuity cannot silently exhaust its frontier;
- every policy relation compares the intended portable principal;
- assert and diary compounds survive independent replication;
- evidence, bundle, transfer, erase and survivor objects have one canonical
  identity and reducer meaning;
- the core, corpus and harness exist, all required lanes pass, the offline
  fixture is recorded, and the discrepancy audit finds only editorial drift.

## Bottom line

The peer report materially improves the v0.4 review with `w.gen`, audit-lock/
budget and initial-capability-epoch findings. Its optimism does not survive a
wire/reducer audit: it overlooks unverifiable service signatures, mixed cutoff
domains, non-atomic rotation references, an unconsumed policy relation,
singular completion for plural imports, contradictory evidence hashing and an
underivable erase manifest.

V0.4 remains the right foundation and is much closer than v0.3. Cut one
focused v0.5 authority/reducer repair, permit stable scaffolding in parallel,
then let executable vectors decide Gate A. Do not freeze the canonical corpus
or enable durable P1 writes before that closure.
