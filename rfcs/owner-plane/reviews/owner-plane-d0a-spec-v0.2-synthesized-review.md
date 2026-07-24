# Synthesized review: D0-A Core + Memory normative specification v0.2

*2026-07-11. Synthesis of
[`owner-plane-d0a-spec-v0.2-review.md`](/Users/vm/owner-plane-d0a-spec-v0.2-review.md)
and
[`owner-plane-d0a-spec-v0.2-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.2-review-2.md),
against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.2 and the
frozen umbrella RFC v3.1/D0. This is a conformance/freeze synthesis, not a new
architecture review.*

## Executive verdict

**Keep v0.2 as a draft. Do not declare Gate A yet.**

Both reviews independently reach the same high-level conclusion: v0.2 is a
large, high-quality improvement, the owner-plane architecture no longer needs
reopening, and one focused closure revision should be enough. The remaining
work is exact protocol work—bytes, authority, transitions, and failure
dispositions—not another program of design.

The most accurate reconciliation of the reports is:

> **All seven v0.1 blocker families are materially addressed in design, but
> several are not yet discharged at the normative wire/reducer level.**

Review 2 is right that lineages, the edge/portable IAM split, ciphertext-only
tenant storage, compound lifecycle operations, and the erase state machine are
strong solutions. Review 1 is right that a protocol cannot freeze while
receipt/lease objects are unsigned, operation bodies remain open or absent,
capability epochs cannot represent multi-zone grants, log recovery can discard
ambiguous data, and Memory policy/evidence/export behavior still requires
implementer invention.

The consolidated disposition is:

- **Direction and umbrella architecture:** approved; do not reopen.
- **V0.2 architecture completion:** successful.
- **Gate-A specification freeze:** no.
- **Safe implementation now:** strict-decoder, closed-schema, and vector-harness
  scaffolding only; no durable production Memory writes.
- **Next revision:** one v0.3 normative-closure pass in the order below.
- **Freeze mechanism:** green executable vectors followed by one discrepancy
  audit, not another broad prose review.

Gate A is also mechanically unmet under the document's own definition: no
`owner-plane-core` crate or owner-plane vector corpus currently exists, while
§16 requires all named families to be green.

## Assessment of the peer review

Review 2 is a valuable and unusually concrete audit. Its strongest technique is
walking the specification's own §7.3 bootability invariant through every other
rule. That exposes three important internal failures:

1. the hosted owner cannot currently accept or erase the diary it is supposed
   to use;
2. mandatory certificate deadlines plus self-witness exclusion block a solo
   plane's first write;
3. the most authority-critical control shapes have been deferred to fixtures.

Its best peer-only additions are also valid:

- `SpaceCreate` lacks the governed `space_class` consumed by assert admission
  and judgment policy;
- old writer-generation heads have no canonical Frontier retirement rule;
- the audit space has no audit operation or valid system writer;
- the genesis witness-policy default and solo→multi-device transition need
  exact control behavior;
- the bespoke generation-start hash is absent from the closed domain-tag
  inventory.

Several peer conclusions need qualification before being adopted:

1. **“All seven blockers discharged” is too strong.** The peer often marks a
   concept resolved when the architecture now contains the right nouns. Gate A
   requires the verbs and bytes too. For example, IAM has a table but no closed
   portable grant-verb mapping; capability epochs exist but one scalar cannot
   serve a multi-zone grant; bounds exist but remain incomplete and internally
   inconsistent; receipts exist but have no signature envelope.

2. **The V1 per-verb owner repair is directionally correct but not sufficient.**
   Generic `memory.curate` is too broad. Hosted owners need narrowly typed
   rights for safe acceptance, retirement, dispute, and erase request while
   declassification and instruction-grade curation remain impossible. Pinning
   itself can become instruction-grade auto-context, so hosted pin authority
   needs target-kind/provenance limits rather than blanket admission. The
   portable fold must also prove that an owner act came from a human/trusted
   gesture; `actor.kind: "human"` is currently signer-controlled text.

3. **The V2 genesis-default repair does not alone solve certificate expiry.**
   A budget fallback is described for grants, while every certificate has a
   required deadline. V0.3 must say explicitly how a solo operation under an
   expiring certificate becomes acceptable without independent time evidence.

4. **The V5 declassification formula must preserve every immutable floor, not
   only `space_minimum`.** Verified evidence and imported source floors are
   lower bounds too.

5. **The V6 raise ceiling is only an additional bound, not a DoS fix.** A
   hostile writer can itself hold a `sensitive` grant. A distinct
   classification-raise capability, target scope, and possibly quota are still
   needed—or the authorized classification-DoS residual must be explicitly
   accepted.

6. **Hosted continuity is ambiguous, not unconditionally doomed.** Compound
   renewal-enrollment may be intended to create a lineage successor. If that
   resets budget/cutoff state, it reopens the v0.1 evasion; if it does not, the
   bounded generation window is eventually exhausted. The transition must be
   made explicit and non-resetting.

7. **Do not add standalone hosted `RevokeGrant` silently.** Narrowing authority
   is attractive, but D-15 ratifies compound hosted enroll/revoke only. Either
   make grant narrowing part of the existing compound renewal/revocation path
   or obtain an explicit owner amendment.

8. **Review 2's `DrillProof` explanation is correct.** A trusted client may
   author a recovery-arm proof that a hosted-genesis plane accepts. The missing
   sentence is that phrase/private-key material never enters hosted-origin
   code; `DrillProof` need not be removed.

9. **The decision-record D-10 statement is incorrect.** V0.1 D-10 is the
   classification ladder; deterministic wrapper nonces are D-22. V0.2's I2 and
   carry-forward row should not call wrapper-nonce determinism a D-10
   refinement. D-24 through D-27 also say “adopted from review,” which is not
   owner ratification under a record that says nothing passes by silence.

These qualifications strengthen rather than diminish the peer report. Its
V1–V3, V5, V7, and V8 findings are blocker-grade; V4 and V6 identify real
problems but need the narrower remedies above.

## What should not be re-litigated

The two reports strongly endorse retaining these decisions:

- RFC 8949 Core Deterministic Encoding with text-keyed signed maps and sorted
  record arrays;
- separate stable `device_id`, writer lineage, and generation concepts, with
  lineage-summed budgets and cutoffs;
- service-edge IAM shapes separated from portable replica admission;
- one bootstrapping control chain with tagged proof arms and deterministic
  recovery branch cutting;
- item-level DEKs, immutable encrypted cores, mutable epoch wrappers, and
  ciphertext-only tenant persistence;
- join-time `WrapAdd`, with rotation reserved for exclusion;
- claims rather than facts, append-only judgments, decisions propose-only,
  owner-only status-changing disputes, and supersession revival;
- explicit `as_of_ms`, per-item read authorization for pins, and separate
  source release/destination import authority;
- hosted private/sensitive storage with an honest authority ceiling;
- separate Gate A specification conformance and Gate B production readiness.

The remaining work completes these choices; it does not replace them.

## Consolidated freeze blockers, in repair order

### 1. Close the normative object and operation universe

V0.2 says it supersedes v0.1 but repeatedly delegates normative rules to it:
signatures, genesis, recovery, item crypto, status precedence, authorization,
and migration are described as “as v0.1,” “unchanged,” or “stands.” A frozen
specification should be implementable without reconciling an archived draft.

Appendix A is not closed yet:

- `signedop.body = {* text => any}` bypasses operation-specific unknown-field,
  type, depth, and enum validation;
- all control bodies are deferred to vector fixtures, including
  `GenesisBundle`, enrollment/revocation compounds, `KekRotation`, policy
  operations, and `RecoverySuccession`;
- the HPKE recipient/KEK-wrap object has no exact shape;
- receipt/lease signature envelopes do not exist;
- `mimport` is a comment saying “+ mclaim fields” rather than CDDL;
- daemon record payloads are prose shapes;
- exact built-in policy bytes and hashes are promised in a nonexistent
  Appendix A.9;
- `policy` lacks the top-level `v` required by E6;
- Ed25519/P-256 public keys and signatures, P-256 point form, HPKE `enc`, and
  malformed-key outcomes remain unconstrained byte strings;
- `SpaceCreate`/`SpacePolicySet` do not carry the closed `space_class` consumed
  by workflow assert admission and policy rules.

Add a closed operation registry for every
`(tenant, operation_type, operation_version)`:

```
exact body | authority/proof or portable grant verb | chain lane
budget charge | state transition | replay rule | failure disposition
```

Fixtures should instantiate this registry, never define it.

Close the profile inconsistencies in the same pass:

- E4 says no defaults, while absent `max_generations` means 8 and other fields
  have stated default/absence behavior. Make security-relevant values required
  or define canonical omission semantics and narrow E4.
- E7's full-canonical-member order and Frontier's `(lineage, gen)` order need
  one explicit relationship.
- The 64-KiB control cap cannot contain 1,024 realistic HPKE wraps. Reconcile
  the limits and cap every unbounded array/physical record.
- Add `genstart` and the deterministic assert-request derivation to the closed
  hash-domain inventory instead of using bespoke unlisted SHA-256 prefixes.
- Freeze exact UTF-8 byte-length accounting, numeric ranges, and all
  key/signature lengths.

Finally, S7 covers device keys but not the genesis/successor administrative
root. The umbrella requires vault-style passkey sealing with mandatory recovery
envelopes. A degraded daemon file fallback must not silently apply to that
root. Define the root envelope, unlock, rotation, recovery, and migration
contract explicitly.

**Closure criterion:** an independent implementer can build a strict decoder,
operation dispatcher, and root/device custody adapter using v0.3 plus cited
standards only—without opening v0.1 or inventing a field or default.

### 2. Make time authority signed, non-circular, and solo-plane viable

The receipt taxonomy and self-witness exclusion are good. The authority is
still incomplete:

- receipt and lease statements have `issuer_key_id` but no signature,
  algorithm, or canonical signed wrapper;
- self-exclusion must resolve both signer and issuer keys to `device_id`, so a
  renewed key cannot receipt its own old-key operation;
- T4 validates issuer state “at `seen_ms`,” circularly relying on the time being
  proven;
- witness classes allowed in prose do not have a matching closed
  `ZonePolicy.time_witnesses` encoding;
- Connect/device issuer discovery, key rotation, and fork handling are absent;
- a lease is not bound to an operation, request, or sequence range, and
  untrusted `created_hlc` can be backdated after lease expiry;
- the exact predicate over lease issuance, expiry, maximum age, skew, and
  timely operation observation is unstated.

Define signed receipt/lease envelopes under their existing domains, accepted
issuer/control-frontier qualification, device-based self-exclusion, and a
non-backdateable link from an online-lease operation to timely witness
observation.

Then repair the boot invariant. Every certificate has a required deadline;
after genesis a solo plane has no second device; self-receipting is forbidden;
and the default policy is fail-closed. The first P1 write therefore
quarantines, contradicting §7.3.

V0.3 must freeze:

1. the exact genesis `ZonePolicy` bytes;
2. the no-independent-witness rule for both grant **and certificate** validity;
3. whether solo mode uses a required budget lane, non-expiring certificate, or
   another deterministic non-time authority;
4. whether adding an independent witness changes policy automatically or only
   through explicit `ZonePolicyInstall`;
5. solo→multi and witness-loss transitions as vectors.

The honest statement is that a solo plane cannot possess independent time
evidence. A bounded non-time lane is therefore a defined authority posture,
not an implementation failure.

**Closure criterion:** trusted-local, hosted-solo, and multi-device planes can
all accept their permitted first write, while no device can backdate its own
deadline or reuse an expired lease.

### 3. Repair hosted-human authority without widening the hosted ceiling

The peer's V1 is a direct contradiction. `owner` requires a human on a device
class with no exclusions. `hosted-browser` always has exclusions, so a pure
hosted plane has no owner-class actor. It consequently cannot accept its own
safe diary observations, retire them, submit a status-counting dispute, or
request the retrieval-exclusion erase promised by §7.5. The same issue affects
unattested/other-only planes.

The repair should be **per action and target**, not “hosted browsers are now
owners” and not one generic `memory.curate` grant:

- introduce closed portable rights for safe owner judgment, erase request,
  classification raise, pin, declassification, and instruction-grade
  curation;
- permit hosted direct-human acceptance/retirement/dispute/erase only within
  its ceiling;
- decide whether hosted pinning is limited to observation/episode and
  non-instruction-grade targets, because an auto-context pin can itself compile
  durable instruction influence;
- keep declassification, cross-plane/high-impact export, procedure/preference
  acceptance or graduation, effects, and admin unavailable;
- define portable evidence for a human/owner action. `actor.kind` is not proof;
  it is signed but chosen by the signer.

Hosted lifecycle must also be non-resetting. Define how renewal after browser
eviction advances or reauthorizes a lineage without clearing budgets/cutoffs,
and how capability-epoch maintenance remains available within the ceiling.
Keep `DrillProof` accepted only when generated on a trusted lane. Any standalone
hosted grant-narrowing operation requires explicit reconciliation with D-15.

The certificate class also needs to distinguish platform-attested from
unattested mobile, or carry closed portable evidence for that distinction; the
current `mobile` enum plus opaque evidence cannot select the two different
ceiling rows.

**Closure criterion:** a hosted owner can use, curate at the explicitly safe
level, and retrieval-erase the hosted diary indefinitely, while no byte pattern
lets hosted-origin code acquire declassification, instruction-grade curation,
export approval, effect, recovery-secret, or admin authority.

### 4. Close writer, capability, KEK, recovery, and Frontier transitions

The state model now has the right components, but several transitions are
underdetermined:

- `w.gen` has no tenant/space lane, grant verb, budget charge, or admission row;
- O6 currently requires it for generation 1 and therefore contradicts both a
  normal first tenant operation and control `GenesisBundle`;
- old Frontier heads are keyed by `(lineage, gen)` but never retire, so the
  canonical frontier grows forever;
- capability epochs are per zone, while a grant may cover multiple zones or
  `"*"` and contains one scalar epoch;
- `authored_kek_epoch` is signed but has no old-epoch admission rule after
  rotation/fencing;
- recovery names a retained base but not the successor header's exact
  generation, sequence, previous hash, or exception to ordinary fork
  detection;
- initial admin/recovery epochs and control-chain sentinel values are not
  frozen.

Recommended v1 closure:

- generation 1 opens directly on the generation-start sentinel; `w.gen` is the
  charged system operation opening generation `g >= 2`; control stays at a
  separately specified generation/lane;
- once a valid successor generation causally incorporates the prior terminal
  head, define whether the Frontier retains only the active head or retains old
  heads until an explicit checkpoint—one canonical rule, with vectors;
- require one zone per write-capable grant or encode a sorted zone→epoch map;
- define `authored_kek_epoch` as audit-only or give exact accept/quarantine/
  reject behavior against accepted rotation/cutoff state;
- place `RecoverySuccession` at exact `base.seq + 1`, previous `base.op` (or
  another frozen rule), and state its precedence exception over the normal
  duplicate-sequence fork rule.

Ordinary exclusion rotation also needs history semantics. `WrapAdd` gives a new
device only the current KEK, so every surviving historical DEK must acquire a
current-epoch wrapper under the same crash-safe completion protocol used by
erase—or a separate historical-key delivery scheme must be frozen.

**Closure criterion:** every transition has one predecessor, authority,
counter/epoch update, budget effect, replay rule, and canonical Frontier
result; generation or enrollment churn cannot reset security state.

### 5. Complete item wrapping, atomic commits, and corruption-safe storage

The ciphertext-only item/log architecture should remain. Freeze these details:

- the exact AEAD wrapping operation and 32-byte DEK plaintext;
- recomputation of deterministic wrapper nonces;
- exact-byte idempotence for a repeated `(KEK epoch, item_addr)` wrapper—a
  differing duplicate under the same AES-GCM nonce is corruption/fork;
- a survivor/current-wrapper-set digest and count before old-KEK destruction;
- validation that plaintext `ItemCommit` lineage/generation/sequence agrees
  with the decrypted signed header;
- an atomic batch/transaction record for `m.assert`, which promises two items
  in one durable commit while `ItemCommit` holds one;
- a transaction/recovery rule for the claimed one-local-commit cross-zone
  release/import pair, whose records live in separate zone logs.

The file format must freeze magic/version/kind values, zone sentinel, payload
encoding, what `len` counts, maximum frame size, and CRC32C parameters. Because
the CRC excludes `len`, a corrupted length can destroy synchronization. A
complete final bad-CRC record is ambiguous—it may be a torn write or committed
data later corrupted—and must quarantine rather than be silently truncated.
Only an incomplete trailing frame is safely truncatable without additional
framing evidence. Protect the length or add independently checkable
resynchronization/footer data.

Gate A should freeze the abstract durability and custody contract. Gate B
validates adapters: Windows owner-only ACLs rather than POSIX mode 0600,
user-agent-managed rather than claimed hardware-sealed WebCrypto keys, the
actual Linux `keyctl` persistence model, macOS flush semantics, IndexedDB
degradation, disk-full behavior, and cross-process locks.

**Closure criterion:** crash/corruption vectors never accept a torn record,
silently discard ambiguous committed data, expose tenant plaintext, destroy an
old KEK before all survivors are proven wrapped, or split a promised atomic
semantic transition.

### 6. Make Memory admission, folds, evidence, and transfer executable

The Memory model is substantially improved but not yet a pure function of
portable bytes.

#### Admission and operations

`CapabilityGrant.ops` remains arbitrary text, while §11.1 mixes wire operation
types, edge permissions, and actor prose. Freeze exact grant verbs and body
variant mappings. In particular:

- encode or deterministically recognize propose versus assert;
- name permissions for author/owner retract and supersede branches;
- add the missing import-grant flag or a distinct import verb;
- add the `w.gen`, audit, and any system projection operations to the registry;
- define how accepted `m.erase_request` changes retrieval immediately and what
  status/metadata remains;
- give `raise_class` distinct, scoped authority rather than every reader-writer.

#### Policy and status

Spaces bind a policy hash, but judgments and pins record only `{id, version}`.
Include the content hash in `polref`, publish the exact built-ins, apply
`valid_from_ms`, and state how later policy/space-minimum changes affect an
existing projection at its stamped control frontier. The audit writer needs a
real actor/proof and body rather than a nonexistent `system` actor.

#### Classification

The current declassification expression collapses algebraically to
`d.new_class` and can cross immutable floors. Define separately:

1. immutable lower bound = at least space minimum + verified evidence/import
   floors;
2. mutable declared/raised component;
3. causal dominance required for a declassify to lower the mutable component;
4. deterministic selection among multiple qualifying declassifications,
   conservatively choosing the higher concurrent result;
5. exact behavior after a space-minimum change.

#### Evidence

A dangling plane reference currently trusts the claimant's `class_floor`, so a
malicious writer can underlabel unavailable evidence. Treat it as `sensitive`
or require cryptographically verified source classification. Add `plane_id`
where cross-plane references are possible, use typed refs rather than bare
hashes for judgment evidence, and specify the resolver binding for opaque
external locator hashes.

#### Export/import

Define the canonical released bundle, digest, source authorization proof,
foreign-plane genesis/cert/grant verification inputs, destination idempotency
and replay rules, expiry, resulting claim identity, and complete `mimport`
schema. Model/embedding/reflection endpoints need a governed retention/training
profile identifier or hash; `provider_id` alone does not describe the egress
policy being approved.

**Closure criterion:** two replicas with identical accepted inputs and explicit
`as_of_ms` derive the same admission, status, classification, retrieval,
auto-context, audit, erase, and export/import result without caller-selected
policy or free-text role interpretation.

### 7. Give every outcome a disposition and every vector an executable surface

The closed outcome names are useful but do not determine lifecycle. Map each to
permanent reject, pending dependency, quarantine/re-proposal, exact-byte
duplicate, writer/control freeze, or local read-only/rebuild. Add missing
classes for signature/key/AEAD/wrapper failure, issuer fork, duplicate,
control-plane freeze, corrupt log, lock failure, and storage failure—or name a
separate closed storage/edge enum.

The vector container is pseudocode, not JSON Schema. Freeze hex conventions,
byte order, integer ranges, expected reason **and disposition**, seeded RNG
algorithm/consumption, and fixed-signature handling where browser WebCrypto
cannot inject randomness.

Replace “all families in Rust + browser” with a required surface matrix:

| Surface | Required work |
|---|---|
| Shared Rust/WASM core | canonical bytes, hashes, admission, reducers, Memory folds |
| Native crypto adapters | sign/verify, HPKE, AEAD, custody integration |
| Browser WebCrypto | supported import/sign/verify/encrypt/decrypt paths |
| Native storage on macOS/Linux/Windows | framing, flush, lock, crash/corruption matrix |
| IndexedDB/Web Locks on named browsers | transactions, exclusion, eviction/degraded floor |

Name browser/version support and distinguish required CI from manual acceptance.
Keep actual replica/backup/distributed-GC guarantees in D0-B; D0-A may use
local checkpoint/copy simulations.

Gate B must be strengthened and described as **necessary, not sufficient** for
P1. It needs production reducer/IAM/receipt/projection/migration integration,
runtime-boundary tests, and storage/keystore failure cases. The umbrella also
requires P0.5's orchestration-checkpoint replacement and atomic removal of the
tombed Memory implementation before durable P1 writes. Name those dependencies
so “Gate B green” is not mistaken for sole launch authority.

Clean §15 before freeze: correct D-10 versus D-22, split or otherwise make the
D-1…D-12 carry-forward unambiguous, obtain explicit owner ratification for
D-24…D-27, and record the offline confirmation fixture and result.

**Closure criterion:** the vector corpus can be implemented without inventing
normative data; every negative case asserts both reason and disposition; every
test runs on a named meaningful surface; and all Gate-A lanes are green.

## V0.3 repair sequence

The two reviews combine into a practical order of work:

1. **Close schemas first:** self-contained normative text, operation registry,
   control/HPKE/receipt/policy bodies, key encodings, root custody, defaults,
   bounds, and `space_class`.
2. **Close authority transitions:** signed time/lease inputs, solo policy,
   hosted safe-owner verbs, human attestation, `w.gen`, capability/KEK/recovery
   transitions, and Frontier retirement.
3. **Close durable transactions:** wrapper-set completion, ordinary rotation,
   atomic assert/cross-zone transfer, exact file framing and recovery.
4. **Close Memory functions:** admission verbs, audit/erase, policy/status,
   classification, evidence, and transfer bundles.
5. **Close failure behavior:** outcome dispositions and storage/edge enums.
6. **Freeze the vector schema/matrix and implement the shared core.** Add every
   finding above as a positive or negative vector.
7. **Run a final prose↔vector discrepancy audit.** If it finds only editorial
   drift, declare Gate A; if behavior is invented in fixtures, fold it back into
   the normative document first.

## Proposed Gate-A checklist

Gate A is ready only when all of these are true:

- V0.3 is self-contained and has no normative dependency on v0.1.
- Every signed, hashed, wrapped, or stored object has a closed schema, key
  encoding, cap, canonical order, and unknown-field rule.
- Every operation has a typed authority, lane, transition, budget effect,
  replay rule, and failure disposition.
- Admin-root custody satisfies the umbrella's mandatory passkey/recovery-
  envelope invariant.
- Receipts and leases are signed, issuer-qualified, device-self-excluding, and
  non-backdateably tied to the operation they authorize.
- Solo, hosted, and multi-device boot/lifecycle vectors all pass without
  crossing a provenance or device-class ceiling.
- Generation, renewal, enrollment, and epoch changes cannot reset budgets or
  cutoffs; Frontier retirement is canonical.
- Crash/erase vectors fail closed for torn frames, bad lengths/checksums,
  missing survivor wrappers, and torn multi-item/cross-zone transactions.
- Memory admission, status, classification, evidence, audit, erase, and
  transfer are deterministic from portable bytes plus explicit `as_of_ms`.
- The decision record contains explicit owner rulings for every normative
  choice and correct decision identifiers.
- Every Gate-A vector family is green on its named implementation surfaces.

## Bottom line

The peer review reinforces rather than reverses the first verdict. V0.2 is the
right design and a successful completion pass, but it is not yet a frozen
protocol. Review 2 catches several excellent internal contradictions; Review 1
supplies the byte, storage, custody, Memory-transfer, and gate closure that its
optimistic resolution table overlooks.

One disciplined v0.3 can plausibly close the combined set. After that, the
owner should give the go-ahead only when implementations and vectors prove the
document has one meaning. Until then, prototype the decoder and conformance
harness, but do not authorize durable P1 Memory writes.
