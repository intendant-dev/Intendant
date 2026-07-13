# Review: D0-A Core + Memory normative specification v0.2

*2026-07-11. Strict delta/freeze audit of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.2 against
the v0.1 synthesized review. This is deliberately not another umbrella-RFC
architecture round. The review used three independent passes: protocol bytes
and transitions, Memory/IAM semantics, and storage/delivery readiness.*

## Executive verdict

**Keep v0.2 as a draft; do not declare Gate A yet.**

The important news is positive: v0.2 resolves most of the previous review's
architectural blockers. It now has a credible encrypted-item model, a bootable
control lifecycle, stable writer lineages, capability epochs, a principled
service-edge/portable-admission split, a substantially more executable Memory
model, explicit hosted-device ceilings, and an honest two-gate release model.
I do not see a reason to reopen the owner-plane architecture or the umbrella
RFC.

What remains is narrower but still protocol-critical. The document does not
yet determine one unique implementation at several authority and byte
boundaries. In particular, implementations would have to invent parts of the
control schemas, receipt signatures, lease linkage, writer-generation
admission, multi-zone capability-epoch behavior, file recovery, and Memory
policy/export rules. Those inventions could produce mutually incompatible—or
materially less secure—replicas.

The right next step is therefore **one v0.3 closure pass**, not more design
expansion. Close the seven work packages below, then implement the vectors.
Gate A is also mechanically unmet today: the document defines it as all vector
families passing in Rust and browser CI, while no `owner-plane-core` crate or
owner-plane vector corpus currently exists in the repository.

My suggested decision is:

- **Direction:** approved.
- **Architecture:** sufficiently settled for D0-A.
- **Normative bytes/state transitions:** not frozen yet.
- **Implementation start:** appropriate for a small schema/vector harness, but
  not for durable production Memory writes.
- **Expected remaining design work:** one exactness pass, followed by failures
  discovered while implementing vectors—not another broad review cycle.

## What v0.2 gets right

These are substantive completions, not editorial improvements:

1. **The storage/erasure model is now coherent in shape.** Tenant persistence
   is ciphertext-only; `ItemCore` covers the immutable encrypted record;
   `op_hash` remains zone-private; wrappers are epoch-selected; local erase has
   a crash-oriented sequence; and the browser no longer keeps a durable
   plaintext operation store.

2. **Genesis now represents a complete initial control state.** It creates the
   first certificate, lineage, private zone and KEK, home/audit spaces, policy,
   wrap, and grant. Enrollment and revocation are compound lifecycle
   operations; join-time `WrapAdd` avoids rotate-on-every-join; recovery branch
   cutting is derived from a signed base rather than replica-local
   observations. The deadline issue discussed below still prevents calling the
   first-write path fully bootable.

3. **The writer model closes the v0.1 budget-reset hole.** Grants bind a stable
   `device_id` and lineage; generations do not reset cutoffs or accounting;
   capability-epoch bumping is an explicit admin act.

4. **IAM now respects the actual access shapes.** Service-edge facts such as
   live sessions and bearers no longer contaminate portable replicated
   admission. The direct enrolled-device lane is no longer denied merely
   because it has no daemon token.

5. **Memory is much more honest and operational.** Decisions are propose-only;
   low-trust disputes cannot suppress accepted context; superseded claims can
   revive; semantic expiry uses explicit `as_of_ms`; pins select without
   granting read access; export and import are separate authority events; and
   evidence references are typed.

6. **The document distinguishes specification safety from production
   readiness.** Gate A versus Gate B is exactly the right framing. The browser
   durability floor, hosted erasure limit, cooperative-replica boundary, and
   colluding-witness residual are stated rather than hidden.

Those decisions should be retained. The remaining findings are completion work
around them.

## Freeze work package 1: make the normative specification closed and self-contained

V0.2 says it supersedes v0.1, but still delegates normative behavior to it:
algorithm/signature rules, genesis fields, authorization rules, recovery,
status precedence, migration invariants, item nonce generation, and KEK
details are variously described as “as v0.1,” “unchanged,” or “stands.” An
archived draft can be useful history, but a frozen protocol should not require
readers to reconcile two supposedly superseded normative texts.

Appendix A is also not yet the promised closed schema:

- `signedop.body` is `{* text => any}`, which admits shapes and values that the
  closed-profile rules otherwise reject.
- Every control-operation body—including compound genesis/enrollment,
  rotation, policy installation, and recovery—is explicitly deferred to future
  vector files.
- `mimport` ends with a comment saying “+ mclaim fields,” rather than a schema.
- The HPKE recipient/KEK-wrap package, signed receipt/lease wrapper, and daemon
  record payloads do not have exact types.
- The two built-in policy constants and pinned hashes are promised as
  “Appendix A.9,” but no such appendix or fixtures exist.
- `policy` lacks the top-level `v` required by E6.
- Public keys and signatures are unconstrained byte strings. Ed25519 lengths,
  P-256 SEC1 point form, P-256 signature representation, HPKE `enc` form, and
  malformed-key outcomes need to be frozen.
- S7 does not cleanly specify custody for the genesis and successor admin-root
  keys. The umbrella requires passkey-sealed custody with a mandatory recovery
  envelope. If S7's degraded file fallback includes the admin root it conflicts
  with that invariant; if it excludes it, the root envelope, unlock, rotation,
  and migration contract is still missing.

Vectors must test a schema; they should not be the only place that schema is
invented. Fold all remaining v0.1 normative text into v0.3 and add a closed
operation registry with, for every `(operation_type, operation_version)`:

```
tenant/lane | exact body type | admissible proof arm or capability verb
chain rule | budget charge | state transition | failure disposition
```

That registry would resolve several later ambiguities at once. Unknown keys
should be rejected after dispatch to the exact body type, not accepted through
an open `any` map.

Two profile contradictions should be fixed in the same pass:

- E4 says there are **no defaults**, while absent `max_generations` means 8,
  `deadline_fallback` and `lenient_epochs` have defaults, and absent
  `grant_epoch_slack` means unbounded. Either make these fields required or
  explicitly define schema-level omission semantics and narrow E4.
- E7 orders set members by their complete canonical encodings, while Frontier
  separately orders heads by `(lineage, gen)`. Declare Frontier as an explicit
  specialized order or encode each head so the two orders are identical.

Also make every bound executable. The 64-KiB control-object cap conflicts with
up to 1,024 HPKE wraps in one operation; several arrays and physical records
have no count/size cap; and “text size” should say UTF-8 byte length. Derive the
maximum wrap count from the actual encoded package size or define a bounded,
atomic continuation scheme.

**Closure test:** an implementer who has only v0.3 and the cited external
standards can write a strict decoder and operation dispatcher without opening
v0.1 or inventing a field, default, encoding, or policy constant.

## Freeze work package 2: complete signed time, receipt, and lease authority

The conceptual split among storage receipts, acceptance receipts, replica
acks, checkpoint witnesses, and leases is good. The wire authority is not yet
complete:

- Receipt and lease records contain `issuer_key_id` but no signature, signer
  algorithm, or canonical signed envelope. As written, any replica can
  manufacture one.
- Qualification is phrased as excluding the operation's *signer*. It should
  exclude the same `device_id` across certificate renewal, not merely the
  current key.
- T4 validates an issuer certificate “at `seen_ms`,” but `seen_ms` is the
  timestamp whose authority the receipt is meant to establish. Define issuer
  qualification from accepted control state and a pinned witness-policy
  version/frontier, without circularly trusting the asserted time.
- `LeaseProof` is not bound to a particular operation, request, sequence range,
  or subsequent timely acceptance. A writer can retain an old lease, create an
  operation later, and backdate untrusted `created_hlc`.
- The exact live predicate over `issued_ms`, `expires_ms`, `max_age_ms`, skew,
  and the operation/acceptance event is not stated.

Define `SignedReceipt` and `SignedLease` envelopes over the existing `receipt`
and `lease` hash domains; define issuer-key discovery and rotation; bind the
witness policy/control frontier; and tie an online-lease operation to its lease
hash plus a qualified observation of that operation within the lease interval
(or define an equivalently non-backdateable attestation).

There is a separate bootability issue. Every certificate has a mandatory
expiry, and §9.1 requires independent deadline evidence for operations under
an expiring certificate. A local P1 plane with one device, no Connect, and no
second witness cannot produce that evidence; the budget fallback is stated for
grants, not certificates. Choose one exact rule: non-expiring local
certificates, a sequence/control-frontier validity lane, or a budget fallback
that explicitly covers certificate validity too. As written, the first local
durable write can be impossible.

**Closure test:** a replica can verify receipt/lease bytes without ambient
state beyond accepted control inputs, and every flagship bootstrap lane has a
defined way to accept its first write without permitting self-backdating.

## Freeze work package 3: close control-chain and epoch transitions

Several new control concepts are correct but do not yet have complete
admission transitions.

### Writer generations

Every generation must begin with `w.gen`, but `w.gen` has no defined
zone/space placement, capability verb, budget charge, or row in the Memory
admission table. It also conflicts with the universal wording for the initial
control chain, whose first operation is `GenesisBundle`, not `w.gen`.

Give `w.gen` an exact system lane and grant rule, or define a precise exception.
Pin generation-zero behavior and all sentinel/header values.

### Capability epochs

Capability epochs are per zone, while one grant may cover several zones or
`"*"` and carries only one scalar epoch. A multi-zone grant cannot record its
issuance epoch once the zones diverge. For v1, the cleanest rule is likely one
zone per write-capable grant; alternatively use a canonical zone-to-epoch map.

### KEK epochs

`authored_kek_epoch` is signed but portable admission never consumes it. Define
whether an operation authored under an old epoch is accepted, quarantined, or
rejected after rotation, including interaction with revocation cutoffs and the
local erase fence. If it is audit-only, say so and state which accepted control
state actually governs decryption/admission.

### Recovery placement

C3′ determines the retained base but not the resolver operation's exact header
placement. Pin its control generation, `writer_sequence`,
`previous_writer_hash`, initial recovery epoch, and precedence over the normal
fork rule. A natural rule is sequence `base.seq + 1` and previous hash
`base.op`, explicitly treated as the authorized branch-cut transition.

### Hosted continuity and device classes

A hosted plane may enroll/revoke/wrap but cannot explicitly reauthorize a
lineage or bump a capability epoch. `EnrollDevice` ambiguously permits
“WriterLineage create-or-successor.” If that compound operation can reset the
lineage or its grant accounting, it reopens the evasion v0.2 is meant to close;
if it cannot, repeated browser eviction eventually exhausts the bounded window
and makes the plane read-only. Define a narrowly ceiling-constrained continuity
transition that preserves lineage budgets/cutoffs, or ratify another exact
non-resetting rule.

Keeping `DrillProof` admissible on a pre-re-root hosted plane is fine **only**
when a trusted client produces and submits the recovery-arm proof. State that
hosted-origin code never solicits the phrase or handles the recovery private
key; the plane's hosted provenance and the proof-generation lane are separate
questions.

Finally, the ceiling table distinguishes attested and unattested mobile
devices, while the certificate enum has only `mobile` and the evidence blob is
opaque. Use distinct certificate classes or a closed, portable attestation
level that every replica can evaluate.

**Closure test:** every control and chain transition has exact predecessor,
authorization, counter/epoch, state update, and replay/fork behavior; a hosted
plane can continue indefinitely within its ceiling but cannot cross it.

## Freeze work package 4: finish item crypto and make log recovery fail safely

The encrypted-item split is now right. A few details remain load-bearing:

1. State the complete wrapping formula and plaintext—e.g. the exact
   AES-256-GCM operation over the 32-byte DEK, derived nonce, and specified
   AAD—rather than inheriting part of it from v0.1. Require validators to
   recompute the deterministic nonce.
2. A second wrapper for the same `(KEK epoch, item_addr)` must be byte-identical.
   Reusing the deterministic AES-GCM nonce with different wrapped plaintext is
   catastrophic, so classify a differing duplicate as corruption/fork and pin
   it in vectors.
3. `RewrapDone {covers: frontier_hash}` proves an operation frontier, not that
   every surviving item has a current-epoch wrapper. Before destroying the old
   KEK, bind completion to a canonical survivor/current-wrapper-set digest and
   count.
4. Apply that survivor-rewrap rule to **every** exclusion rotation, not only
   erase. Because `WrapAdd` distributes only the current KEK, a later device
   can read retained history only if surviving historical DEKs have
   current-epoch wrappers (or if a separately specified historical-key scheme
   exists).
5. `m.assert` promises claim plus judgment in one durable commit, but
   `ItemCommit` stores exactly one item. Add an atomic batch/transaction log
   record or change the asserted semantics. “Idempotently complete the second
   half after a crash” is useful recovery, but it is not the promised one
   commit.
6. Validate that plaintext sequencing metadata on `ItemCommit` agrees with the
   decrypted signed header. Otherwise the outer log/index binding is not
   actually authenticated.

The physical daemon format also needs exact, corruption-safe framing. Freeze
the format-version value, kind values, zone sentinel, payload encodings, what
`len` counts, maximum record length, and CRC32C parameters. The CRC currently
covers `type || payload` but not `len`; a corrupted length can destroy record
synchronization. Worse, a complete final frame with a bad CRC is currently
truncated as though it were necessarily a torn append, silently discarding
data that may have been committed and later corrupted.

Only an incomplete trailing frame should be automatically truncated. A
complete bad-CRC frame should quarantine the log. Protect the length as part of
the checksum or add an independently checkable framing marker/footer that
permits safe resynchronization; do not rely on finding later records after an
untrusted length has already redirected the scanner.

Platform wording should match the actual contract: WebCrypto keys are
user-agent-managed/nonextractable, not necessarily hardware- or
platform-sealed; Windows needs owner-only ACLs rather than POSIX mode 0600;
Linux `keyctl` has a different persistence/threat profile from Keychain/DPAPI;
and macOS should state whether critical commits require `F_FULLFSYNC` rather
than ordinary `fsync`. Gate A should freeze the abstract custody/durability
contract and remove misleading equivalences; validation of each platform
adapter belongs to Gate B.

**Closure test:** deterministic crash/corruption vectors establish that a
committed item is never silently discarded, a torn append never becomes an
accepted item, and an old KEK is not destroyed until every survivor wrapper is
cryptographically accounted for.

## Freeze work package 5: make Memory admission and policy fully deterministic

The Memory model is close, but several rows still depend on prose identities
or capabilities not represented on the wire.

### Capability vocabulary and actor paths

`CapabilityGrant.ops` is an unconstrained list of text, while the table maps
wire operations such as `m.claim` to edge permissions such as
`memory.propose`. Define the closed portable grant-verb vocabulary and exact
operation/body-variant-to-verb mapping. In particular:

- `m.claim` does not encode propose versus assert on its own;
- retract, supersede, and `raise_class` use actor prose instead of a named grant
  verb;
- `m.import.claim` requires an “import-flagged grant,” but no such grant field
  exists;
- allowing any reader-writer to raise classification creates a durable
  classification-denial-of-service power; either name and grant that power or
  record the residual explicitly.

A direct hosted human is neither the `owner` actor class nor an authoring
`session`, so the flagship hosted diary appears able to propose observations
but not self-accept them. It also cannot submit `m.erase_request`, because that
requires owner-class `memory.curate`, despite §7.5 promising immediate
retrieval-exclusion erasure. Define constrained direct-human self-accept and
erase-request paths that do not accidentally grant general curation.

The opposite direction also needs protection: `actor.kind: "human"` is a
signer-controlled header value. Portable admission cannot derive owner-class
authority from that assertion alone, or a daemon/native signer with a curate
grant can label itself human. Define the portable human-presence/owner
attestation that makes the actor class valid, or make owner acts a distinct
trusted-client proof arm.

### Policies and judgments

Spaces bind a content-addressed policy hash, but judgments and pins record only
`{id, version}`. Carry the hash in `polref` so the same identifier/version
cannot be reinterpreted. Publish the exact built-in policy bytes and hashes.
Apply `valid_from_ms` explicitly in projection eligibility, and state how later
space-minimum/policy changes affect existing claims at a given control
frontier.

Complete the system projections too. The `audit` space is described as holding
system-actor operations, but no `m.audit` body, authority, or admission row
exists. An accepted `m.erase_request` is said to exclude targets immediately,
but the status/retrieval fold never consumes it. Both need exact operation and
projection rules, including idempotency and what metadata remains visible.

### Classification

The current declassification equation is not well-defined:

```
max(base capped by d.new_class, d.new_class)
```

algebraically collapses to `d.new_class` under the natural interpretation and
can fall below the space or provenance minimum. It also does not select among
multiple causally qualifying declassifications.

Split the fold into an immutable lower bound—at least space minimum, trusted
evidence/import floors—and a mutable claimed/raised component. A declassify may
lower only the latter, must causally dominate every relevant raise, and must
have a deterministic winner rule (the conservative maximum among concurrent
qualifying lowers is simple). Define how a later space-minimum change is
stamped into the projection.

### Evidence

A dangling plane reference currently contributes the writer-supplied
`class_floor`; a malicious proposer can therefore underlabel unavailable
evidence. Treat unresolved plane evidence as `sensitive`, as external evidence
already is, or require a cryptographically verified source classification.
Include `plane_id` if plane refs may ever cross plane boundaries, and use typed
evidence references for judgment evidence rather than bare hashes. If
`locator_hash` is intentionally non-dereferenceable without a local resolver
record, specify that resolver binding.

### Export/import

The pair is the right authority model but is not yet “exact.” Define the
canonical released-bundle schema and digest, source proof/authorization
material, destination verification inputs, idempotency/replay behavior,
expiry, and the resulting claim identity/body. Complete `mimport` CDDL. For
model/embedding/reflection egress, `provider_id` alone does not bind the
retention/training treatment that makes a flow acceptable; include a governed
egress-policy/profile identifier or hash.

The claimed same-plane cross-zone “one local commit” also spans distinct
per-zone logs. Either add a small transaction journal/recovery protocol for the
release/import pair or remove atomicity from that claim and specify the
candidate/pending states after a crash.

**Closure test:** two replicas given the same accepted bytes and explicit
`as_of_ms` derive identical admission, status, effective classification,
exportability, and auto-context results without consulting caller policy or
free-text role interpretation.

## Freeze work package 6: specify outcomes as state transitions, not only names

The closed outcome vocabulary is a good start, but an outcome code alone does
not tell implementations what to do. Freeze a disposition for every code:

- permanent reject;
- pending dependency and retry when the frontier advances;
- quarantine for re-proposal;
- exact-byte idempotent duplicate;
- writer/control freeze requiring recovery;
- local read-only/rebuild required.

For example, `causal-missing`, `deadline-unreceipted`, `fork`, and
`unknown-version` must not all become the same generic rejected state.

The vocabulary also lacks common failures implied by the protocol: bad
operation/receipt signature, malformed key or point, AEAD/item-address/wrapper
failure, receipt-issuer fork, exact duplicate, control-plane frozen, corrupt
log, lock unavailable, and storage failure. Storage/edge-only failures may live
in a separate closed enum, but E10's claim that every validation failure maps
to a closed outcome must then name which enum and boundary.

**Closure test:** every negative vector asserts both a stable reason code and a
stable disposition; replaying it or adding its missing dependency has a
specified result.

## Freeze work package 7: turn the vector and gate plan into an executable matrix

The 14-family coverage plan is strong. Its container and execution contract
are still pseudocode:

- the block labeled JSON is not valid JSON or JSON Schema;
- hex case/byte order, integer limits, expected-error/disposition form, and
  deterministic RNG stream consumption are unspecified;
- browser WebCrypto cannot inject deterministic randomness into every
  operation, notably ECDSA signing. Cross-language tests should verify fixed
  signatures/keys where injection is unavailable and test production
  generation separately;
- “every family in Rust + browser” cannot literally apply to `flock`,
  `FlushFileBuffers`, native crash recovery, and cross-process file tests.

Publish a per-family matrix:

| Surface | Appropriate conformance work |
|---|---|
| Shared Rust/WASM core | canonical bytes, hashes, reducers, admission, Memory folds |
| Rust native crypto adapters | signing, HPKE, AEAD, custody adapters |
| Browser WebCrypto | import/verify/decrypt and browser-supported signing paths |
| Native storage per OS | framing, flush, locking, crash/corruption matrix |
| IndexedDB/Web Locks per supported browser | transactions, eviction/degraded mode, tab exclusion |

Family 13 should use local checkpoint/copy simulations in D0-A; actual replica,
backup, and distributed-GC guarantees remain D0-B as the scope section says.
Name the supported browser/version matrix and say which lanes are required CI
gates versus manual acceptance; “browser” by itself is not an auditable target.

Gate B should then be strengthened before it is allowed to authorize P1 writes.
In addition to custody/parser/storage primitives, require production integration
of the control and Memory reducers, all applicable service-edge IAM adapters,
receipt/lease verification, projection rebuild, migration/re-encapsulation,
and tests proving durable keys/plaintext never enter `intendant-runtime`.
Include disk-full, lock failure, quota/transaction abort, keystore unavailable,
and recovery-from-corrupt-local-copy behavior. Gate B should be a necessary
cutover gate, not an implication that primitive conformance alone enables the
feature.

More strongly: **Gate B is necessary, not sufficient, for P1.** The umbrella
plan also requires P0.5's orchestration-checkpoint replacement and an atomic
cutover that removes the tombed Memory implementation before durable P1 writes
are enabled. Name those dependencies in §16 so “Gate B green” cannot be read as
sole feature-launch authority.

Finally, clean the affirmative decision record before freeze: give D-1 through
D-12 individual unambiguous rows, fix the D-10/D-22 wrapper-nonce numbering
collision, and obtain explicit owner ratification for D-24 through D-27 rather
than leaving them as “adopted from review.” Record the offline confirmation's
fixture and pass/fail result when it is run.

**Closure test:** the vector corpus can run without inventing normative data,
and each test has one named implementation surface rather than an impossible
“all tests everywhere” requirement.

## Recommended v0.3 sequence

To keep the pass small and minimize rework:

1. **Close the object and operation registry first.** Fold in v0.1 rules;
   complete CDDL, key encodings, defaults, caps, policy constants, signed
   receipts/leases, and control bodies.
2. **Write exact transition tables.** Control/recovery, `w.gen`, capability and
   KEK epochs, deadline/lease admission, Memory capability mapping, and outcome
   dispositions.
3. **Repair the local transaction/framing contract.** Atomic assert, protected
   record lengths, corruption handling, wrapper-set completion.
4. **Finish the four Memory folds.** Status/policy, classification, evidence,
   export/import—including direct hosted-human lanes.
5. **Freeze a real vector schema and execution matrix.** Then add the shared
   core and make all Gate-A families green.
6. **Run one final conformance audit against the implemented vectors.** At that
   point review should be about discrepancies between prose and executable
   behavior, not new architecture.

## Proposed Gate-A acceptance checklist

I would give the go-ahead when all answers below are “yes”:

- Is v0.3 self-contained, with no normative “as v0.1” dependencies?
- Does every signed/hashed/stored object have a closed schema, encoding, cap,
  and unknown-field rule?
- Does every operation have an exact authority, chain lane, transition, charge,
  and failure disposition?
- Are receipts and leases signed, issuer-qualified, anti-replay-bound, and
  non-backdateably tied to the operation they authorize?
- Can trusted-local, hosted-only, and trusted-multidevice planes each perform
  their complete permitted lifecycle without accidentally crossing a ceiling?
- Do crash recovery and erase completion fail closed under corrupt length,
  corrupt checksum, torn batch, and missing wrapper cases?
- Are policy, classification, evidence, and export/import folds functions only
  of accepted portable bytes plus explicit `as_of_ms`?
- Is the vector format exact, and are all required families green on the
  surfaces where they meaningfully apply?
- Does the affirmative decision record contain owner-ratified rulings rather
  than “adopted from review” entries, including any new v0.3 choices?

## Bottom line

V0.2 is a successful architecture-completion revision. It should not be
discarded or broadened. Its remaining defects are concentrated at precisely
the boundaries a normative protocol must freeze: exact bytes, exact authority,
exact transitions, and exact failure handling.

One disciplined v0.3 pass can plausibly make this implementable. Freeze only
after that pass is embodied by green vectors; until then, allow prototype work
on the shared decoder/vector harness but do not authorize durable P1 Memory
writes.
