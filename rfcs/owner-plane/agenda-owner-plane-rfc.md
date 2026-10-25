# RFC: The Owner Plane, Memory, and Agenda (v3.1 / D0)

*2026-07-11. Supersedes v3 (archived: `~/agenda-rfc-archive/2026-07-11-rfc-v3-as-reviewed.md`)
by folding the v3 synthesized review
(`~/agenda-owner-plane-rfc-v3-synthesized-review.md`): a correction pass —
contradictions fixed, the four-gate split, the ratified picks (Appendix B
decision 11) — not new scope. Earlier provenance: v3 folded
`~/agenda-owner-plane-rfc-synthesized-review.md` plus the owner's
genesis-lane decision (§3.5); v2 and its reviews are archived alongside.
Errata pass applied 2026-07-11 per the go-ahead audit
(`~/agenda-owner-plane-rfc-v3.1-review.md`; pre-errata copy archived as
`2026-07-11-rfc-v3.1-as-audited.md`) — the umbrella is now frozen.
Self-contained: reviewers and implementers need no other document. Companion chapters once implemented: `docs/src/owner-plane.md`
(new), touching `trust-architecture.md`, `trust-tiers.md`,
`credential-custody.md`, `self-hosted-rendezvous.md`, `autonomy.md`,
`mcp-server.md`.*

> **Status — adoption statement.** Accepted as product architecture and D0
> requirements. The authority format, wire protocol, and tenant reducers
> remain **provisional** until the four specification gates are specified
> and golden-tested: **D0-A (Core + Memory)** freezes before any durable
> P1 Memory data, **D0-B (Sync)** freezes before P2 sync,
> **D0-Agenda-Data** freezes before P3, and **D0-Agenda-Effects** freezes
> before P5/P6 (§15). Each gate ships as its own normative specification;
> this umbrella stops growing here. Product UI and service shapes may be
> prototyped earlier; durable user data is never cut over into a
> provisional format. **The umbrella is frozen (2026-07-11, post-errata)**:
> changes past this point belong in the four gate specifications, which
> carry the protocol bytes and reducer truth tables — not here.

---

## 0. Executive summary

Intendant grows an **owner-plane**: person-governed, end-to-end-encrypted,
log-structured state that travels with its owner rather than any box. Two
tenants share that plane's kernel:

> **Memory is what the system believes, observes, decides, or learns.
> Agenda is what the system intends or commits to do.**

Memory ships first (it exercises authorship, provenance, retrieval, and IAM
with no irreversible effects); Agenda follows (it adds clocks and
real-world effects). Both are product steps toward Intendant as an
everything-app: an AI diary/agenda for technical and regular life, valuable
with **zero daemons** (browser + account), with daemons as the layer that
makes it act.

Two boundaries organize every decision in this document:

1. **Durable vs ephemeral decides the plane.** Durable, owner-governed
   state lives in the owner plane. Ephemeral coordination state lives on
   the box (§9). Agent working memory lives with the agent (§6.0, §8).
2. **Data vs instruction decides the channel.** Plane content is always
   quoted, provenance-labeled data. Instructions reach models only through
   curated channels (skills, prompts) or approved effects. Nothing in the
   plane self-executes.

And one invariant, revised from the original trust doctrine:

> **Authority is minted only by the local authority of the governed
> resource: plane-root capabilities govern plane data; daemon-local IAM
> governs daemon effects. Connect mints neither.**

*Out-of-band defect (found by review, independent of this RFC):* the
`LiveAudioSpawn` always-require-approval policy was dead code on **both**
dispatch paths in main — the native loop dispatched `spawn_live_audio`
before runtime-command classification, and the MCP/ctl path mapped it only
to coarse `runtime.control` with no always-consent check. **Fixed and
merged 2026-07-11 (PR #236)**: one centralized always-consent gate shared
by both paths, with gate-leg and MCP-dispatch tests, then hardened into a
compile-time token requirement (§15.0).

## 1. Vision and product framing

- The Agenda/diary is the first Intendant feature whose value does not
  require owning a machine: a browser tab plus an account yields a
  private, portable AI diary and to-do list. Claiming a daemon upgrades it
  to a system that reminds, executes, and (eventually) calls you.
- Beyond software development by design: zones and spaces carry personal
  and household life as naturally as repos.
- The owner plane is also the standing anti-fragmentation answer: any
  future account-scoped feature joins it (preferences, cross-daemon
  session notes, the diary narrative) instead of inventing its own sync.

## 2. Conceptual model

### 2.1 Four independent axes

- **Plane — ownership and governance.** A person or organization owns a
  plane; its root governs devices, capabilities, zones, and admin
  history. ("Owner Plane" is the product name; *principal plane* is the
  internal term, since org planes are not personal accounts.)
- **Zone — cryptography and replication.** A zone determines which
  devices can decrypt and replicate. Zones are key domains, not labels.
- **Space — semantic context.** A project, workflow, personal domain, or
  team namespace *inside* a plane. Audience is semantics, not crypto.
- **Network — transport and discovery.** Fleet/peer/Connect reachability.
  Grants no data authority by itself.

A project is ordinarily a **space** in a personal or org plane, not an
authority root inferred from a filesystem path.

### 2.2 Storage classes

| Class | Scope | Sync | Examples |
|---|---|---|---|
| daemon-plane | one box | none | sessions, logs, caches, `intendant.toml` (repo-local but **git-ignored** — local operational config, never synced) |
| project-plane | one repo | with the repo | instruction files, tracked space marker (§2.3) |
| **owner/principal plane** | one principal | Connect (or self-hosted), E2E | Memory, Agenda; later: preferences, diary |
| **coordination dir** (§9) | one box, ephemeral | none (daemon may relay) | session heartbeats, dirty sets, inter-session notes |

### 2.3 Space identity

The stable space ID for a repository lives in a **small, deliberately
tracked marker file** — not `intendant.toml`, which is git-ignored local
operational configuration and may contain executable MCP configuration.
(D0-A names the marker and freezes its exact filename, versioned format,
and the binding-confirmation UX.) A
repository-provided space ID is a **binding request, never authority**:
the local daemon binds it to a plane space only with owner/IAM
confirmation, and keeps a local binding from stable space ID → checkout
path(s) that survives worktrees and machines. All worktrees of one
repository resolve to one space identity (this also anchors the
coordination dir, §9).

## 3. Authority

### 3.1 Plane genesis and the lifetime authority model

Browser-only value cannot rest on daemon-local IAM (no daemon → no minting
authority), and making the Connect account the root would hand Connect
authority. Therefore the plane has its own root — but a root key alone is
a moment, not a lifetime. D0-A must freeze:

- a **canonical genesis descriptor** (root public key, mandatory v1
  algorithm suite, versioned `governance_scheme`, creation metadata,
  `genesis_provenance` — §3.5 — and a **commitment to an independent
  recovery authority**) with `plane_id = hash(genesis descriptor)`;
- versioned **administrative-key epochs** and an algorithm-migration path;
- a **canonical control reducer with a tagged authorization proof**. The
  ordinary device path (§3.2/§4.1) names a certificate and capability by
  hash — but the first certificate and capability are themselves
  root-authorized control operations, so the root cannot use the path it
  is creating. Every control operation therefore carries one of:

  ```
  AuthorizationProof =
      DeviceCapability   { certificate_hash, capability_hash }
    | GenesisAuthority   { genesis_hash, root_key_id }
    | AdminEpoch         { epoch, admin_key_id, control_frontier }
    | RecoveryAuthority  { recovery_epoch, proof }
  ```

  Bootstrap, normal device activity, administrative succession, and
  recovery are distinguishable and canonically validated; the reducer
  defines explicit **epoch, precedence, stale-root, and replay** rules —
  per-writer chains alone cannot adjudicate two concurrent root
  ceremonies;
- **loss recovery and compromise recovery are different problems.** A
  recovery envelope or phrase for the *same* root key solves loss only:
  restoring it hands the owner another copy of an authority the attacker
  may also hold. Genesis therefore commits an authority **independent of
  the online administrative root** (offline recovery key or threshold
  custodian set); recovery succession has **defined precedence over a
  compromised admin branch**; and a successful re-root **rotates both**
  the administrative and the recovery authority;
- deterministic handling of **competing successor chains**: a root
  compromised before rotation can sign a rival successor, so a simple
  root-signed successor operation is necessary but not sufficient —
  recovery-precedence and witness/quorum rules must state how clients
  resolve the fork (§16 tests competing successor *histories*, not only a
  lost key);
- **authenticated ordering** for enrollment, revocation, checkpoints, and
  scheduler epochs (the control history is itself a per-writer feed);
- `governance_scheme` v1 implements only a single-person root, but the
  field is versioned so org/multi-party governance is an upgrade, not a
  fork.

The root private key is sealed under vault-style, domain-separated passkey
custody with **mandatory recovery envelopes**. The existing vault is real
precedent — domain-separated passkey envelopes and a generated, mandatory
BIP39 recovery phrase ship today — and its cryptographic envelope pattern
is reused; **recovery drills are to-build** (none exist in source). The
vault does not answer plane *governance*: today's validators enforce
envelope structure (version, size, kind, revision, body shape, MAC
presence) but only require the envelope list to be nonempty and never
interpret its governance semantics, so the plane layer explicitly enforces
its own chosen recovery policy. Root operations
(device enrollment, capability grants, zone admin, scheduler-epoch
changes, succession) are explicit ceremonies, not per-entry writes.

### 3.2 Device certificates and capability grants (two objects, not one)

Identity and authorization are separate objects with separate lifecycles:

- **DeviceCertificate** (root-signed identity): plane ID; stable device
  ID; signing algorithm + public key; **separate** key-agreement/KEM
  algorithm + public key (Ed25519 signing keys are never converted to
  X25519 — distinct keys, distinct purposes); **device provenance class
  with binding evidence** (hosted browser, owner-served browser,
  platform-signed native app, daemon); issuance/expiry; certificate
  revocation ID. Provenance is never self-asserted: enrollment binds
  evidence for the claimed class, and hosted code can neither certify
  itself as native nor mint anything above its ceiling. **Provenance means
  where and under what ceremony the key is held — never that the software
  is incorruptible.** Notarization, Authenticode, and signed release
  manifests establish publisher/distribution facts *locally*; they do not
  remotely prove to another plane member that a key was generated by
  honest running code. v1 therefore uses a per-class evidence matrix:
  hosted and owner-served browser classes bind origin/bundle provenance;
  desktop-native and daemon classes bind a locally verified artifact plus
  **explicit owner attestation during the trusted ceremony**; mobile
  classes use platform attestation where available; unsupported platforms
  get an explicit weaker class, never a self-asserted stronger one.
- **CapabilityGrant** (root-signed, immutable): subject device/writer;
  resource and operation scopes (zones × spaces × tenants × operations ×
  kinds); classification ceilings; flow endpoints; byte/op budgets;
  validity constraints; capability epoch.

Operations reference the **exact certificate hash and capability hash**
they were authorized under — not mutable-looking IDs.

Existing key inventory (why this section exists): today's browser identity
is a non-extractable P-256 ECDSA sign-only key in IndexedDB; the daemon
identity is an Ed25519 signing key. Neither can receive a wrapped zone
key; both need companion KEM identities under a device certificate. The
v1 doc's "wrap to existing identity keys" was not implementable as
written.

### 3.3 Signer, actor, capability, evidence, provenance

Every operation distinguishes:

- **signer** — the device key that produced the signature;
- **actor** — a **structured, namespaced principal** (browser, agent
  session, human, peer) on whose behalf the signer acted. Supervised
  agent sessions hold **no durable keys**; their controller daemon signs
  while attesting the bound session principal;
- **capability** — the plane authorization (by hash) under which the op
  is valid. Ordinary device operations carry the `DeviceCapability` arm;
  genesis, succession, and recovery operations carry their corresponding
  `AuthorizationProof` arm (§3.1);
- **local authorization evidence** — the daemon-local session/grant that
  admitted the actor to the plane service. This is signed
  attribution/audit evidence, never transferable plane authority;
- **content provenance** — source session, project/space, evidence refs.

Free-form `source` strings are never authorization or provenance.

### 3.4 Two-authority execution rule

A plane-side approval never mints authority on an executor. Launching a
scheduled session requires **both**: (1) a valid plane capability and
approval over an immutable Agenda execution revision, and (2) a
daemon-local grant accepting that plane, approver class, execution
profile, project scope, and resource ceilings. A compromised plane writer
can create proposals; it cannot acquire `task.run`, filesystem,
credential, or network authority on any daemon. Executor-local approval
satisfies the daemon-effect half of this rule only — it never substitutes
for plane authority over administration or export.

### 3.5 Genesis and administration lanes (owner decision, 2026-07-11)

**Signed-client genesis is the default; hosted-tab genesis is permitted
only as the degraded lane.**

- **Default / recommended path**: plane genesis and all administrative
  ceremonies — device enrollment, recovery, provenance elevation,
  high-impact export approval — happen only on **trusted clients**: a
  platform-signed native client (Apple-notarized macOS/iOS, Authenticode
  Windows) or the user's own daemon (including the dashboard that daemon
  serves — code from a release-transparency-logged build, served by
  hardware the user controls; publisher signing of that build is a
  deliverable, per the consequence below).
- **Degraded lane**: hosted-tab genesis survives solely for the true
  zero-install persona, and is explicitly labeled degraded. For that lane,
  Connect-served code is **admitted as the administrative TCB** — E2E
  encryption blinds Connect's storage, not the JavaScript it serves, and a
  passkey gesture proves presence to code, not that the code presented the
  transaction the user believes they approved. Consequences are structural,
  not cosmetic:
  - the plane's genesis descriptor is stamped `genesis_provenance: hosted`
    (honest evidence, carried forever);
  - hosted devices sit under a **ceiling that is a canonical reducer
    invariant derived from the plane's genesis provenance** — never a
    revocable policy document signed by the same hosted-custody root it
    constrains: no provenance elevation, no cross-plane/high-impact export
    approval, **no effect approval, no instruction-grade Memory curation,
    no recovery-phrase entry**; low-effect read/write capabilities
    otherwise. Pre-re-root hosted admin powers are enumerated and narrow —
    live enrollment of another hosted browser and same-class revocation
    (ordinary continuity), nothing above the ceiling. **A pure
    hosted-genesis plane therefore supports notes, tasks, and reminder
    *drafts* only: no effect can be approved or fired until a trusted
    client or daemon joins** (owner-ratified 2026-07-11 — §1's "claiming a
    daemon upgrades it to a system that reminds" made precise);
  - when the owner **first opens a trusted client**, the product
    aggressively nudges the **hosted-to-trusted re-root ceremony** —
    §3.1's succession machinery paying for itself. One narrow flow:
    (1) the signed client locally verifies its distribution identity to
    the extent the platform permits; (2) recovery material is entered
    only on that trusted client — this exercises the **independent
    recovery authority**; (3) it independently fetches and verifies the
    complete control history and pinned genesis state; (4) it generates a
    **fresh native-held administrative root and a fresh independent
    recovery authority** (recovering only the hosted root would improve
    custody without resolving compromise — §3.1); (5) it submits the
    succession proof **under the `RecoveryAuthority` arm** — never by
    reconstructing the hosted administrative root, so a
    later-compromised hosted root cannot self-succeed onto attacker
    hardware (the offline recovery material gates elevation) — then
    independently fetches and displays the accepted post-succession
    state; (6) the fresh root **issues the trusted client's first
    DeviceCertificate** (trusted-device enrollment is the ceremony's
    *outcome*, not a prerequisite), recovery precedence (§3.1) **retires
    the competing hosted administrative branch**, and hosted-root
    authority is permanently reduced to the declared hosted ceiling;
  - the **recovery phrase is never entered in a hosted tab**. Recovery and
    recovery drills run on the trusted lane only, and hosted code
    **refuses phrase input by construction** (P2a tests the refusal).
    (Display at hosted genesis is unavoidable for that lane and is covered
    by the admitted TCB; re-entry is not.) A hosted-genesis owner who
    loses their only device installs a signed client or claims a daemon to
    recover.
- **Accepted consequence**: platform developer signing — the previously
  deferred Apple-secrets step, plus Windows Authenticode — moves onto the
  **consumer default's critical path**. GPG-signed releases + the
  release-transparency log remain the sufficiency **bar** for the
  daemon/CLI/self-hosted lane (Linux consumer entry included). Delivery
  status per artifact, source-verified: **the release-manifest log and
  Connect-signed tree heads ship today** — the manifest is artifact
  hashes submitted under a scoped bearer token, not itself a publisher
  signature; **publisher-signed daemon/CLI releases (GPG), the notarized
  macOS/iOS client, the Authenticode Windows client, and zero-daemon
  native-client packaging remain deliverables**. Today's release workflow
  builds only the macOS-arm64 wrapper and may produce an unsigned dev
  artifact when signing secrets are absent.

High-urgency delivery and execution approval follow §3.4's intersection:
the plane-side approval must come from a trusted-origin device **and** the
executor-local grant must independently exist — executor-local approval
never substitutes for trusted plane approval and cannot lift the
pure-hosted plane's no-effects ceiling. Browser-only mode remains a real
product promise, explicitly labeled as this lane.

## 4. Wire protocol

### 4.0 Layering: public manifest, ciphertext, signed operation

The signed header carries plane/space/actor/capability metadata, yet
Connect must see almost none of it. Both hold only with explicit layers:

```
SignedOperation                    # §4.1 — plaintext only inside the zone
    authorization refs, writer chain, causal refs, operation body
  → item AEAD ciphertext           # the exact signed bytes, encrypted
                                   #   under this item's DEK (§4.7);
                                   #   stored verbatim, never re-serialized
  → authenticated item record      # item ciphertext + content address
                                   #   + key-envelope reference
                                   #   + mutable key_wrap_epoch (§4.1)
  → segment container              # transport/storage batch of item
                                   #   records (§4.7)
  → PublicSegmentManifest          # ALL Connect sees and indexes:
      opaque routing/feed handles, key-wrap epoch, segment id,
      ciphertext lengths + content addresses, recipient wraps,
      incremental-sync cursor/range
```

**One object graph** (superseding any reading of earlier drafts in which
the segment was the AEAD unit): the **item** is the encryption unit, the
**segment** is a batching unit, and nothing outside the item ciphertext
reveals operation content. **D0-A freezes the local layers** — because the item is the AEAD unit,
every immutable item-crypto choice is a D0-A choice: the **item AEAD
suite, nonce construction, and item AAD**; **item-DEK wrapping under the
zone KEK**; item ciphertext identity and the local ciphertext/key-envelope
encoding; tombstone/erase-marker format; and `body_hash` semantics —
`body_hash` covers the **canonical plaintext operation body** (it sits
inside the signature and can never be re-decided); ciphertext content
addressing is a separate, outer identity. **D0-B freezes the distribution
layers** — the segment container and the exact public manifest schema
**with its leakage documented**: Connect need not see real signing-key IDs
(it indexes opaque feed handles), but incremental sync inevitably exposes
stable routing handles, segment order/ranges, sizes, timing,
**recipient-set cardinality, and membership-change events**. D0-B also
specifies: container/manifest authentication and AAD binding (manifest ↔
container ↔ item records), recipient packaging and recipient-wrap
binding, compression and padding policy, and partial-upload commit.

### 4.1 Envelope (minimum signed header)

```
protocol_version, tenant, plane_id, zone_id, space_id,
authored_crypto_epoch, capability_epoch,
signer_algorithm, signer_key_id,
writer_id, actor_principal, authorization_proof,
request_id, writer_sequence, previous_writer_hash, causal_references,
created_hlc, operation_type, operation_version, body_hash
```

- `authorization_proof` **is** §3.1's tagged union: ordinary tenant
  operations carry `DeviceCapability { certificate_hash,
  capability_hash }`; control operations (genesis, succession, recovery)
  carry their corresponding arms — bootstrap is representable without
  circularity. Genesis and recovery are plane-wide, so D0-A also chooses
  how control operations fill the resource fields: a **canonical control
  tenant + resource namespace** for the control feed, or a **separate
  canonical control-operation header** — either way the first control
  operation is representable.
- `authored_crypto_epoch` is the **authored authorization epoch** and is
  the only epoch inside the signed bytes. The **`key_wrap_epoch`** —
  which zone-KEK epoch currently wraps this item's DEK — lives in the
  mutable outer item record (§4.0): zone-rotation erase (§4.7) rewraps
  items without touching signed operations, per the migration invariant.
- `writer_id` is explicit and **not silently equated with the signing
  key**: signing-key rotation continues a writer chain under the same
  `writer_id` via a root-authorized continuation record.
- Writer-chain scope is `(plane, zone, writer)`.
- `request_id` is the idempotency handle, **unique per writer chain**
  (duplicate `request_id` with different bytes within a chain is fork
  evidence, §4.2).
- The signature covers domain-separated canonical bytes excluding itself.
  Replicas store the exact signed bytes (no verify-after-reserialize).
  Canonical Rust/WebCrypto **golden vectors land before the protocol
  freezes** and run as required keyless CI (§13, §16).

### 4.2 Ordering, conflicts, writer forks

- **Per-writer authenticated chains**: monotonically increasing
  `writer_sequence` + `previous_writer_hash`. Content hashes are durable
  operation identities.
- **HLC is approximate chronology only.** It never decides grant
  validity, revocation ordering, approval validity, executable winners,
  or offline-vs-revocation ordering (a compromised writer choosing a
  far-future HLC must win nothing).
- Concurrent revisions get **deterministic conflict representation**
  (causal revision references; both sides visible), not silent
  timestamp-LWW dominance.
- Duplicate `(writer, sequence)` with different bytes — or duplicate
  `request_id` with different bytes — is a **writer fork**: evidence, not
  proof of malice. VM snapshots, backup restores, browser multi-tab
  races, and rolled-back durable state reproduce it honestly. The word
  "equivocation" is reserved for a fork the owner has adjudicated as
  malicious.

**Fork prevention and recovery** (D0-A/D0-B requirements):

- atomic single-writer sequencing locally, via the **platform's atomic
  durable transaction** (fsync'd allocation on daemons; IndexedDB
  transactions plus a writer-generation rule in browsers — there is no
  browser fsync to promise);
- multi-tab/browser writer coordination (one tab holds the writer lease;
  writer-generation and two-writer convergence rules land **before** any
  second-browser-restore claim — §15 P2a);
- **one frozen logical frontier map**, and its canonical
  per-writer-heads *type* freezes in **D0-A** — `AdminEpoch` signs a
  `control_frontier`, so the type is embedded in D0-A bytes; D0-B adds
  Merkle compression and witness proofs, never a competing definition.
  Every surface that says "frontier" means this structure;
- after any gap (restore, eviction, snapshot), reconcile with a
  **witnessed own frontier** before writing — fetching from Connect alone
  is insufficient when Connect can serve a stale view; compare against a
  trusted checkpoint or witness where rollback protection is required;
- **fail closed on detected self-fork** (stop writing as that writer);
- owner adjudication; re-enrollment as a **new writer generation**;
  retained fork evidence; explicit old-writer retirement.

### 4.3 Frontiers, completeness, rollback

A multiwriter zone has no scalar head: the frontier is a **vector/Merkle
frontier of per-writer heads**. Signed ops prove integrity, never
completeness — Connect can still omit a feed, serve stale-but-valid
state, or split views. Residual guarantees, stated precisely:

- withholding is detectable only once this device or a witness observes a
  newer commitment;
- cold-start rollback protection requires a paired checkpoint, witness,
  or transparency proof;
- split views surface only through cross-device gossip or witnessing.

**Witness set v1**: the plane's own enrolled devices, plus (optionally)
the existing Connect transparency log. No new witness network. **Witness
design and zone privacy are one decision** (D0-B): either non-members
witness zone-blind commitments, or a zone's witness set is restricted to
its members — and in the member-only design a single-member zone has **no
independent witness** (§4.4's zero-daemon floor states this honestly).
(Proof distribution — how replicas obtain the certificate/revocation
material to *validate* what they fetch — is §4.8.)

### 4.4 Durability states

A local replica is a **durable replica + outbox**, not a cache: before
acknowledgement elsewhere, it is the only copy of an offline write.
Durability is **two orthogonal axes**, not one ladder — availability and
rollback evidence are different guarantees (a witness need not hold
ciphertext; a replica need not have witnessed the latest checkpoint):

```
Availability:      local-only → server-durable → replica-acked(n)
Rollback evidence: unwitnessed → checkpointed → independently-witnessed(n)
```

The UI derives its display from the pair. "A wiped box loses nothing"
begins at `replica-acked`; `server-durable` is honest-service durability
only (a Connect acknowledgement, not Byzantine availability); a
transparency commitment raises rollback evidence while raising
availability not at all.

**The zero-daemon lane needs its own floor, stated honestly**: browser
storage may be evicted by the platform; a single device provides no
cross-device witness; a transparency commitment proves a commitment but
does not hold ciphertext; and losing the local pinned checkpoint may also
lose rollback protection. Onboarding therefore uploads aggressively,
creates/exports recovery material that includes the **latest trusted
checkpoint/log identity** (not just the phrase), and **verifies that the
recovery material and pinned checkpoint identity were created** — a
phrase-free check; the actual recovery drill happens later on a trusted
client (§3.5). A Connect-owned transparency log alone is not an
independent witness for a client that loses all local pins.

### 4.5 Revocation, expiry, and budgets — deterministic under untrusted time

Two separate revocation mechanisms: an **encryption epoch** (future
reads) and a **writer-capability epoch** (accepted operations), plus a
per-writer cutoff `accepted_through = {writer, sequence, operation_hash}`.
Old-epoch operations beyond the cutoff land in quarantine for
re-proposal. The unavoidable trade-off is documented honestly: an honest
offline write made before revocation is indistinguishable from a
backdated malicious one — strict cutoffs may discard honest unsynced
work; lenient ones open a stale-authority window. **Strict-vs-lenient is
a root-signed, versioned zone policy — never a local UX choice.**
The revocation boundary, stated exactly: revocation protects **future
writes and future fetches**. An evicted member that holds an old zone KEK
and replicated ciphertext can decrypt **every item from the epochs it
possessed** — not merely what it had already viewed — and neither key
rotation nor erasure revokes keys or plaintext a recipient retained. The
standard E2E-group limitation, stated in docs and the threat table.

Because HLC is not authority, an operation arriving after capability
expiry cannot prove it was authored before expiry. Working position
(D0-A confirms): **expiry is an acceptance/witness deadline** for
ordinary capabilities; **high-impact capabilities are online-only
leases**; offline writers can be given **epoch/sequence budgets** instead
of wall-clock authority. **Device-certificate expiry has the same
late-arrival semantics** as capability expiry. Deadline expiry is
deterministic only when the protocol defines the acceptable signers, the
signed receipt bytes, clock/skew rules, and offline re-verification —
**D0-A freezes the receipt and bounded-lease proof types; D0-B defines
their transport and witnessing**. Connect may issue a narrowly scoped
storage/time receipt **only where plane policy explicitly accepts that
role** — it never silently becomes an authorization clock. Operation/byte
budgets are per-writer or escrowed (a shared pool cannot be divided
deterministically offline).
Connect storage quotas and local rate limits remain ingestion policy,
never fold policy. D0-A also defines capability-epoch scope and the
minimum control frontier required for high-impact curation/export and for
scheduled effects (§7.5).

### 4.6 Compaction, retention, erasure

Naive set-union breaks under GC (old replicas resurrect discarded ops).
Compaction requires an admin-authorized **checkpoint**: canonical folded
state + reducer/schema version + `covers_frontier` + a GC fence. A valid
checkpoint either covers acknowledged heads for **every admitted,
non-revoked writer** or carries **explicit abandonment cutoffs** for
writers it excludes. Replicas behind the fence rebootstrap; behind-fence
work is never "rebased" (immutable signed bytes cannot be) — it becomes a
**new re-proposal referencing the quarantined operation hash**. Enough
authorization evidence is retained with the checkpoint to validate
compacted state after older certificates and capabilities are collected.

Retention is a **signed hierarchy**: a zone default (personal zones
default to forever — it is a diary) with stricter per-space, per-kind,
and per-status overrides (workflow observations expire in days; curated
episodes/decisions/procedures persist; org spaces require explicit
retention/legal policy). Four removal notions stay distinct: **retrieval
expiry** (stops surfacing), **semantic retirement** (Retire; history
preserved), **physical GC** (checkpointed compaction), and
**cryptographic erasure** (§4.7).

### 4.7 Encryption hierarchy and erase semantics

**Per-item content DEKs from v1** (decided — no longer "reserved"):

```
item DEK  → wrapped by zone-epoch KEK → KEK wrapped to enrolled
             recipient KEM keys
segments  → transport/storage batching of item ciphertexts (§4.0)
```

Rationale: erase-by-key-destruction is the only erasure story that
survives **cooperative replicas and managed backups** — after rotation,
GC, and old-KEK destruction; retained KEKs or plaintext stay outside the
guarantee (§4.5); tenant volumes are modest (a decades-long
diary is millions of items, not billions — wrap overhead is bytes per
item); and retrofitting item granularity later is precisely the format
break D0-A exists to prevent.

Per-item DEKs are **necessary but not sufficient**: while an immutable old
segment still holds an item-DEK wrapper decryptable with a retained zone
KEK, deleting a newer wrapper erases nothing. The v1 mechanism (decided
2026-07-11) is **zone-rotation erase**: rotate the zone KEK epoch, rewrap
every surviving item DEK, omit erased items' wrappers, checkpoint and
garbage-collect **all old segments and wrappers behind the fence**, and
destroy the old KEK on every cooperative replica and backup — leaving a
minimal audit hash/tombstone. Chosen because it composes with the
checkpoint/GC and epoch-rotation machinery the protocol already requires
and adds no second governed store; the alternative — a separately
governed erasable key-envelope store — was rejected as a new replicated
system with its own deletion/backup/malicious-store semantics. Costs
stated honestly: the rewrap is O(zone size) in item count (wrappers are
bytes per item — cheap at diary scale); **erasure latency is compaction
latency**; and erasure cannot affect a member that retained an old KEK or
plaintext (§4.5's boundary). Projections, indexes, checkpoints, and
backups share the same erasure boundary — they hold ciphertext under the
same key domain or are rebuildable and are rebuilt/erased on revocation
and erasure (§6.5). UI language must match the implemented granularity
and latency, never promising instant global destruction.

Metadata visible to Connect (zone IDs via routing handles, sizes, timing,
recipient relationships, traffic patterns) is documented as such (§4.0).

### 4.8 Authorization-proof distribution

Every replica needs enough certificate, capability, revocation, cutoff,
and checkpoint material to validate its feeds — but **one plane-wide
control feed readable by every device would disclose the whole
device/grant graph**. D0-B chooses a proof-distribution design; candidate
shapes:

- global minimal root/succession material plus **per-zone control
  feeds**;
- selectively disclosed **immutable capability objects fetched by hash**;
- **Merkle commitments** with inclusion/non-revocation proofs.

D0-B defines which control material every zone member may see, what
Connect sees, how a replica proves freshness, and what must be available
offline.

### 4.9 Post-quantum posture

Explicit **deferral with format agility**, recorded rather than implied:

- v1 recipient wraps are **HPKE (RFC 9180) base mode with
  DHKEM(P-256, HKDF-SHA256), HKDF-SHA256, and AES-256-GCM** — the
  *component primitives* match the only shipped custody precedent (the
  vault's P-256 ECDH → HKDF-SHA256 → AES-256-GCM construction; browser
  identity, vault custody, and Web Push are all P-256 today, and no
  X25519 custody path exists in source) with universal WebCrypto support;
  HPKE itself is new work, not something the vault already ships;
- **every recipient wrap carries a KEM algorithm ID**, so X25519 (now in
  WebCrypto) and hybrid ML-KEM (FIPS 203; Signal PQXDH as precedent) are
  additions, never format breaks;
- **the weakest wrap bounds the zone**: a zone remains classically
  exposed while any recipient wrap is classical;
- harvest-now/decrypt-later is accepted and documented as v1 residual
  risk for a forever-retention diary; **long-lived signature
  authenticity** is likewise deferred with agility (`signer_algorithm` is
  versioned per certificate);
- when adopted, hybrid lands **native-side first** (daemons control their
  crypto stack; browsers follow when the platform ships ML-KEM).

## 5. Zones and spaces

- **Default zones: exactly one** — `private`. (`agents` was dropped:
  audience is a space/label concern, not a key domain.) Additional zones
  are created by explicit enrollment/product flows, not defaults.
- Zone membership = KEK wrapped to device certs, authorized by root-signed
  grants; replication follows key possession. Revocation = epoch rotation
  (§4.5).
- Spaces organize meaning inside zones (projects, workflows, "household",
  "health"). Capabilities scope to zones × spaces × kinds.
- **Key-domain honesty**: a daemon holding a zone KEK can decrypt every
  segment it fetches in that zone. A "mediated-only" space *inside* a
  fully replicated zone is therefore transport policy, not
  confidentiality. Content that must be confidential from a device lives
  in a zone whose KEK that device never receives. Spaces stay semantic —
  they are deliberately **not** quietly promoted into per-space crypto
  sub-zones.

## 6. Memory (first tenant)

### 6.0 What memory is not

Not everything called "memory" syncs:

- **Session context** (transcripts, tool output, scratch reasoning):
  session-local, disposable.
- **Workflow state** (current plan, blockers, handoffs): goal-scoped,
  short-lived, shareable with explicitly collaborating agents; short
  retention by default (§4.6); its *live* form is the coordination bus
  (§9), not the plane.
- **Durable knowledge** (observations, decisions, lessons, procedures):
  plane-backed, zone-aware — this tenant.
- **Owner/private memory** (personal facts, preferences, episodes): this
  tenant, not agent-readable by default.
- **Org memory**: org plane (§11).

"Agent memory" means agent-authored/consumed but person- or org-governed.
It is never an agent-owned authority domain. The **old Memory/Knowledge
system is tombed** — its channels, cursors, KV model, inheritance flag,
and file format constrain nothing here (cutover inventory: Appendix A).

### 6.1 Claim bodies and judgments — status is derived, never mutated

A single mutable `status` field is not a sound multiwriter model:
acceptance, dispute, supersession, retraction, and pinning are
**judgments** — concurrent, actor-attributed, policy-relative — not
last-writer field values. The reducer therefore has three object families:

```
MemoryClaimBody (immutable) {
  id, plane_id, zone_id, space_id,
  kind, statement,
  sensitivity_classification,          # signed; IAM ceilings need it
  observed_at, valid_from, valid_until, expires_at,
  provenance { signer, actor, daemon, session, project, model,
               evidence_refs },
  supersedes[], labels
}

Judgment (append-only):
    Accept | Dispute | Retract | Retire { target_claim (causal ref) }
  | Supersede  { target_claim, replacement_claim }
  | Declassify { target_claim, new_classification }   # own capability, §6.2
  — all carry: actor, capability_hash,
    policy_id + policy_version, reason / evidence_refs
  (D0-A pins each variant's exact derived-state consequence,
   Retract included)

MemoryPin (append-only, owner-curated) {
  target_claim, destination_space/role, expiry,
  token_budget, provenance_floor,
  accepted_under (judgment ref + policy version)   # what the pin relied on
}
MemoryUnpin { target_pin }   # revokes a specific pin op, not the claim
```

- Effective status (`candidate`, `accepted`, `disputed`, `superseded`,
  `retired`) is a **derived view** over judgments, computable per policy
  version; concurrent Accept and Dispute both survive and both surface.
  **`supersedes[]` in a claim body is advisory lineage only** — only an
  authorized Supersede judgment changes derived status. Derivation names
  its policy; concurrent judgments resolve conservatively; and a pinned
  claim with an unresolved dispute, retraction, supersession, or expiry
  **fails closed for automatic context** (§6.5).
- `propose` emits a claim body; **`assert` emits the claim plus an
  explicit self-accept judgment under a named workflow policy** — the
  trust difference between the two verbs is visible and replayable, never
  ambient.
- Agents create **attributed claims**, not truth; corrections append
  judgments; contradictions may coexist and retrieval surfaces them with
  provenance.
- Validity/expiry are first-class. Model confidence is the writer's
  self-assessment, never authorization. "Accepted" always means accepted
  under a named policy version.
- **Kinds v1**: `observation`, `decision`, `episode`, `procedure`,
  `preference`. Procedures and preferences are higher-risk (§6.5).

### 6.2 Memory IAM — typed, fail-closed

Operations: `memory.search`, `memory.read`, `memory.propose`,
`memory.assert`, `memory.curate`, `memory.export`, `memory.admin` —
plus **`memory.evidence.read`**, the separately authorized evidence
dereference (`memory.read` on a claim does not imply reading its evidence
refs; the operation name is pinned so surface-parity tests can target
it). `search` and `read` are distinct operations: search returns bounded
index entries; read returns bodies.

The plane service evaluates a **typed decision input**, not a bare
operation string:

> effective rights = device/plane capability ∩ daemon-local session grant
> ∩ live session-token scope ∩ request resource constraints
> (plane/zone/space/kind/status/classification/flow/quota) ∩ device
> provenance ceiling

Agent and peer plane access **fails closed without an explicit plane
grant**. This deliberately does *not* extend today's daemon-IAM default —
where an unscoped supervised session receives a root-compatible
principal — to plane operations; plane verbs are not added as strings to
existing coarse roles.

Decided constraints:

- **What agents assert directly**: an ordinary supervised agent may
  `assert` only **its own session's** `observation`/`episode` claims in
  designated workflow spaces with short retention; `decision` only when
  backed by the approval trail that session itself holds. An agent may
  never assert that an owner or organization decided something merely
  because it can describe an approval trail. Everything else is
  `propose`-only.
- **Preferences**: never agent-assertable; propose + explicit owner
  curation, always.
- **Pinning**: owner-curated only (`memory.curate`); agents cannot pin.
- **Candidates**: excluded from retrieval by default; callers opt in and
  results are marked.
- **Audit**: sensitive-space reads/searches are audited by **principal,
  scope, and result IDs** — not raw queries or content (the audit trail
  must not become a second copy of the sensitive data). Audit records for
  a sensitive zone live **in that zone or a dedicated owner-audit
  encryption domain** — a global plaintext control feed would leak
  cross-zone existence and access patterns. Owner-visible in the plane's
  admin history.
- **Indexes are sensitive**: lexical/FTS indexes are encrypted under the
  same key domain or rebuildable-and-rebuilt; they are removed/rebuilt on
  revocation and erasure (§4.7).
- **Classification is derived, monotonic, and destination-aware.** A
  writer's `sensitivity_classification` is a claim, never export
  authority: effective classification is the conservative result of
  trusted space minima, the writer claim, source/evidence taint, and
  curator judgments — and the **destination policy participates in
  authorization**. It never silently decreases through export,
  summarization, Journal synthesis, or Memory-to-skill graduation;
  declassification is its own capability and judgment. **Retrieval into a
  remote model's context is data egress**: the provider/model destination
  and its retention/training posture participate in authorization exactly
  as embedding and transcript-reflection destinations do.

### 6.3 Export is two-sided; projections respect the boundary

Read-in-A plus write-in-B *is* export. The **primary enforceable path is
a stateless composite export operation carrying immutable source
hashes**; short-lived per-session taint refusal is bounded defense in
depth (no promise of perfect dynamic taint tracking across sessions or
colluding principals); and paraphrase/laundering through an ordinary
write remains a **residual risk** unless the session's egress and tools
are constrained — the enforcement-honesty clause below is the boundary,
not a bypass-proof guarantee.

- **Same-plane, cross-zone**: requires source read + an explicit flow
  grant + destination write:

```
memory.export { from(plane,zone,space) -> to(plane,zone,space),
                allowed_kinds, classification_ceiling, expiry }
```

- **Cross-plane**: requires **two independent authorities** — source-plane
  release/export *and* destination-plane import/write. The imported claim
  begins as a **candidate** under the destination's own zone identity;
  source **acceptance** does not transfer, while source
  classification/taint travels as a **lower bound** in the immutable
  provenance — the destination may raise it, and may lower it only
  through authorized declassification (§6.2's monotonicity); the
  provenance reference confers **no read authority** and may legitimately
  dangle (§11 offboarding).
- **Journal has the same boundary**: a transient authorized projection may
  combine sources while retaining their labels, but **persisting a
  narrative into a zone creates new content and is an export/import
  operation** — never merely setting a `diary-visible` label.

Enforcement honesty: this binds the mediated Memory tools; a session with
unrestricted shell/network can paraphrase — strict deployments pair it
with session egress/tool limits.

### 6.4 Two access shapes

1. **Replicating integrated daemon**: holds zone key + writer capability,
   keeps a durable replica, works offline. Owner-private material that
   must stay confidential from a given daemon lives in a zone that daemon
   is not enrolled in (§5) — a "mediated-only" marker inside a replicated
   zone is transport policy only.
2. **Mediated access** (disposable workers, peers, low-trust sessions): a
   Memory service on a trusted daemon authenticates the caller principal,
   evaluates local IAM + plane capability (§6.2's intersection), executes
   the scoped search/read/propose, and signs accepted writes while
   attesting the actor. The remote box never receives the zone key or
   replica. A daemon holding a durable zone key **is integrated-tier for
   that data** regardless of its label; network/fleet membership is never
   itself memory access.

Plane keys and plaintext live in the controller-side service only —
`intendant-runtime` never touches them (the runtime/controller boundary
holds).

### 6.5 Retrieval safety

- No whole-store injection, ever. Agents call bounded `memory.search`
  with count and token budgets; session/tool grants define
  least-privilege default spaces.
- Results carry zone, derived status, actor, evidence, age, validity, and
  conflict info; candidates are excluded or clearly marked; content is
  wrapped as **quoted data, not instruction**. Quoted-data framing is a
  mitigation, not prompt-injection isolation — which is why effects
  always ride the approval path and why instruction-grade content has its
  own route (§6.6).
- Automatic context injection means **accepted AND explicitly pinned**
  (intersection, not union), bounded top-N with token budgets: a small
  per-session **memory index** (one-liners with IDs — the same
  index-plus-lazy-bodies delivery shape as skills, deliberately *not* the
  skills trust class), derived per the session's spaces and effective
  authorization; bodies fetched on demand.
- `procedure` and `preference` kinds ride a **higher-trust retrieval
  route**: never auto-injected from ordinary retrieval; they surface via
  pins and curation flows only.
- **Embeddings**: v1 is a local lexical/FTS index (encrypted/rebuildable,
  §6.2). Embeddings later, local-only first; sending private memory to an
  embedding provider is an explicit credential/egress decision. History
  search, session-log search, and Memory retrieval remain separate
  operations.

### 6.6 Memory → skills: graduation is a gated daemon effect

Skills load as instructions; claims load as data. The bridge is a
lifecycle — episodic observation → claim → accepted → owner-curated
procedure → **explicit export to a skill file** — but "skills are
compiled memory" describes the lifecycle, not a security guarantee:
curation does not validate an instruction. Exporting a claim into
`SKILL.md` / `CLAUDE.md` / `AGENTS.md` managed blocks requires all of:

1. plane-side source read/export/**graduation** authority;
2. a trusted-origin approval appropriate to instruction-grade content
   (§3.5 lanes);
3. daemon-local filesystem authority scoped to the destination;
4. a rendered diff and review;
5. separate installation/activation;
6. retained source provenance and classification checks.

The flow **generates a draft written outside every auto-loaded
instruction path**, reviewed as a patch/artifact, and only then atomically
installed — a pending draft must never sit at a path agents already load.
Installation respects destination-repo conventions (this repo's
`CLAUDE.md`/`AGENTS.md` byte-parity included). Curation alone never
activates future-agent instructions. Nothing automatic, not v1.

## 7. Agenda (second tenant)

### 7.1 Kinds and effects are orthogonal

```
entry_kind = note | task           # v1; `question` reserved, parked
effects    = notification(policy)
           | session_launch(immutable_manifest_hash)
```

(The earlier T0–T3 ladder is dropped: it collided with trust-tier
vocabulary and promoted effects into kinds — and omitted plain `task`.
The `question` kind and its `Reply`/`Answer` semantics are **parked in
Later alongside durable questions/replies** (§15) rather than shipping
operations whose delivery plan is parked; the schema reserves the kind.)
Notes and tasks may carry reminders; tasks may or may not launch
sessions; urgency is separate from channels.

### 7.2 Operations

`Add`, field-level `Patch` (non-effectful presentation metadata only),
`Complete`, `Reopen` (explicit transition — no "monotone but
un-completable" hand-waving), `Retire`, `JournalInclude`/`JournalRemove`
(curation judgments, §7.7), `ProposeEffect`, `ApproveEffectRevision`,
`RejectEffectRevision`, **`RevokeEffectApproval`**, **`CancelOccurrence`**,
`RecordOccurrenceStarted`, `RecordOccurrenceResult`. (`Reply`/`Answer`
ride the parked `question` kind, §7.1.) Cancellation is a first-class
operation, never implied by tests. Tags, if an OR-set, carry real
observed-remove semantics. **A display-only due date is patchable
presentation state; a time that fires an effect lives in the approved
effect manifest** — the UI distinguishes "due" from "will run/notify", so
a cosmetic date edit never invalidates an approval. The exact entry
reducer — immutable entry revisions, causal parents/heads, the
semantic-vs-presentation field split, tag OR-set identifiers, and
deterministic-or-visibly-conflicted outcomes for concurrent
Complete/Reopen/Retire/patch — freezes in **D0-Agenda-Data** (§15.1).

### 7.3 Executable state is atomic; effect lifecycle is explicit

Fieldwise merge must never compose an executable state no author
proposed. **Every effect has a stable `effect_id`** — an entry can carry
several reminders plus a session launch, so approval, revocation, and
fireability scope per effect, never per entry. Every effect revision —
**including notification policy** — is an **immutable,
`effect_id`-lineaged manifest referencing the immutable semantic entry
revision it belongs to**; approval signs the manifest digest. Manifests
are **typed variants under a common header** (a notification manifest
carries no backend/model/sandbox fields). Common header: goal;
`effect_id`; occurrence/due instant with timezone interpretation;
retry/misfire policy; **declared retry-safety class** (§7.5); approval
expiry. Session-launch variant: executor daemon; local execution profile;
project/space binding; backend/model; sandbox; filesystem roots; network
policy; credential requirements and failure behavior; autonomy ceiling;
token/cost/wall-time/concurrency/subagent limits. Notification variant:
channel/urgency policy (§7.6). Execution profiles, standing grants, and
evaluator policies are referenced **by content hash** (immutable), never
by mutable ID — otherwise a digest approval can be changed indirectly.
**Any material edit or reschedule creates a new proposed revision and
invalidates the old approval.**

Lifecycle rules (D0-Agenda-Effects freezes these — §15.1):

- `RevokeEffectApproval` and `CancelOccurrence` take effect at the
  freshness predicate (§7.5); a revoked approval never fires.
- `Complete` and `Retire` **cancel pending occurrences** of the entry's
  effects.
- `Reopen` restores the entry's semantic state only; a past one-shot
  occurrence **never refires** without a new approved effect revision.
- Reschedule = supersession: new revision, old approval invalid.
- Concurrent approvals / conflicting heads fail closed:

> **An occurrence is fireable only when, for its `(entry_id, effect_id)`,
> exactly one causally maximal, approved, unrevoked effect revision
> applies. Any unresolved concurrent head disables that effect's firing.**

### 7.4 Scheduled sessions

- Launch under a fail-closed **`scheduled-session` principal** with an
  executor-local profile ("a normal supervised session" is necessary,
  not sufficient — sandbox/IAM/credentials/network/autonomy are
  independently constrained; goal text remains untrusted input).
- The existing approval registry's UI/rail is reused; its in-memory
  one-shot semantics are **not** — approvals here are durable plane
  operations.
- Standing grants ship only after each manifest constraint's enforcement
  is demonstrated.

### 7.5 Firing and the occurrence ledger

`ClaimFiring` ops in an eventually-consistent log cannot provide mutual
exclusion (two partitioned daemons both fire; union reveals it later).
v1:

- **v1 invariant: the scheduler and the executor are the same daemon**
  (decided 2026-07-11) — one explicitly selected daemon per plane holds a
  **bounded, root-authorized firing lease** (the scheduler epoch is its
  lease epoch). Scheduler and executor remain distinct *roles* in the
  vocabulary — an occurrence binds effect revision, scheduler identity +
  lease epoch, executor identity, and due instance — but v1 never
  separates them, and per-item scheduler overrides are deferred with the
  split. Not "the anchor" — the anchor serving role is deliberately
  fungible; overloading it couples two lifecycles.
- `occurrence_id = hash(entry_id, effect_id, approved_revision_hash,
  due_instance)`;
- a **daemon-plane occurrence ledger** ("ledger", reserving "journal" for
  the product view, §7.7), fsync'd before spawn. States:
  `Prepared → Started(session_id) → Completed(result_hash)`, plus
  **`Failed(error)`, `Cancelled`, `Missed` (staleness window elapsed),
  and `Unknown` (executor lost sight of a launched session)**, with retry
  attempts recorded per occurrence. Cancellation races are defined on
  both sides of `Prepared`/`Started` — preventing a launch and
  terminating a running session are different actions. Session creation
  is idempotent by occurrence ID; crash recovery reattaches before
  retrying. **Notification delivery keeps its own idempotency key and
  attempt ledger** (rail/push retries must not multiply);
- **at-least-once with deduplication**, never exactly-once, stated
  honestly — and honestly bounded: idempotent *session creation* does not
  make arbitrary external actions inside a retried session idempotent.
  If a session disappears with unknown effects, the executor **fails
  closed for high-impact work and surfaces `unknown` for owner
  resolution** unless the manifest's declared retry-safety class permits
  re-execution;
- firing **any effect — notification delivery included** (a stale
  reminder is still an effect) — requires a sufficiently **fresh
  control/revocation frontier**, where "fresh" is an enforceable
  predicate frozen in D0-Agenda-Effects (on D0-A's receipt/proof types,
  §4.5): required control epoch/checkpoint, maximum
  age, witness class, and failure behavior. A Connect response alone
  cannot prove a cancellation was not withheld; stale offline state fails
  closed absent an explicit bounded offline grant;
- **scheduler transfer is an owner ceremony, and fencing is causal, not
  telepathic**: a newer root-signed scheduler epoch existing elsewhere
  does not stop an offline scheduler that has never seen it. The
  successor activates only after the **old scheduler acknowledges the
  transfer or its bounded lease/freshness horizon expires**. Manual
  reselection *is* failover; "no failover" in P5 means **no automatic
  election** — manual *fenced* transfer is a v1 requirement, and without
  it a returning offline daemon double-fires;
- missed-while-down: fire-on-wake within a staleness window, then degrade
  to a "missed" digest entry;
- occurrence results record executor identity, session/log reference,
  usage/cost, terminal state, and an evidence hash. Results are
  **evidence for a later Memory proposal, never auto-accepted truth**
  (§7.8).

### 7.6 Notification policy

Author-controlled delivery is too much power. Separate: entry priority;
notification policy; **maximum allowed escalation step**; quiet hours;
rate/cost caps; channel availability; per-channel user consent. Effective
delivery authority is an intersection:

> plane-side consent ∩ per-channel endpoint consent ∩ executor-local
> grant — with device provenance capping who may *raise* any of them.

Defaults: the rail may carry low-effect notices; push requires
per-endpoint opt-in; voice/phone require explicit per-item consent or a
narrowly scoped standing consent. Current reality (source-verified): the
dashboard's **attention center** and **work-content-free** Web Push exist
— the nudge carries a request kind, daemon/session display labels (a
user-chosen session rename may ride), and a navigation URL, never
command/question/file text; "metadata-free" it is not. Voice is a future
attachment point; live audio always requires human approval — the
enforcement gap found in main was fixed out of band (§15.0). Voice/phone
are designed as future escalation steps — never described as an existing
ladder.

### 7.7 Three views over the same substrate

**Agenda** (actionables), **Journal/Diary** (curated chronology across
selected Memory episodes and completed Agenda occurrences — an operation
log is not automatically a pleasant diary; persisting a narrative is an
export, §6.3; source-side inclusion is an explicit curation
operation/judgment), **Audit** (the signed history — complete **relative
to retained checkpoints and GC fences**: after retention GC or
cryptographic erasure, only the permitted hashes/tombstones survive;
edits, approvals, device changes, execution attestations). "Occurrence ledger"
(§7.5) is reserved for scheduler durability and is not one of these
product views.

### 7.8 Memory ↔ Agenda conversion

```
session observation → memory proposal → accepted memory
        → (explicit effect boundary) → agenda proposal
        → approval / reminder / scheduled session
        → occurrence result → reflective memory proposal (supervised)
```

Example: Memory "TLS cert expires Aug 1" → Agenda "renew by Jul 20".
Results and reflections never auto-become accepted truth.

### 7.9 Agenda IAM

Explicit vocabulary, same typed fail-closed evaluator as §6.2:

- `agenda.search / read / propose / write / complete / answer` (`answer`
  activates with the parked `question` kind, §7.1);
- `agenda.effect.propose / approve / revoke`;
- **`agenda.effect.execute`** — the permission to perform the underlying
  side effect (launch/deliver). `agenda.occurrence.record` authorizes an
  **attestation about** an occurrence, never the side effect itself;
- channel/notification rights (per §7.6's intersection);
- `agenda.admin`.

Cross-zone and cross-plane Agenda movement uses §6.3's flow vocabulary
(source release + destination import) — Agenda does not grow a second
export model.

Scoped by plane, zone, space, kind, **effect type, executor, channel,
time horizon**, classification, and budgets. Ordinary agents propose;
trusted owner classes approve; scheduler/executor principals may record
**only their own bound occurrences**.

### 7.10 Reserved shapes (Later, but schema-visible now)

- **Recurrence** (with recurrence-instance identity), **event triggers**
  ("when PR X merges"), and location/contextual triggers: v1 is one-shot
  only; these are explicitly re-parked in Later (§15), not silently
  dropped.
- **Attachments/blobs**: Memory and Agenda schemas reserve an encrypted
  attachment/blob **reference shape** (content hash + reference
  semantics + classification) from v1 — diaries will contain images and
  documents, and daemon-local upload references do not resolve
  cross-device. Blob bytes, metadata, erasure keys, and evidence
  authorization get a later storage design; the reference must not
  require a schema break.

## 8. External agents and foreign memory systems

- **Native memory systems of externals are out of scope by design.**
  Claude Code's local files stay Claude Code's (their locality is a
  feature — nothing to chase); Codex under the app-server has none (its
  desktop-only memory is irrelevant to us). No parsing, mirroring, or
  reconciliation; bulk import is not worth doing (anything imported would
  enter as quarantined untrusted observations anyway).
- **Channel 1 — `intendant ctl` verbs taught via skills (primary).**
  Memory/agenda verbs are ctl subcommands. **The session rail already
  exists**: external sessions receive an `INTENDANT_MCP_URL` carrying a
  session-ID-derived token, and ctl uses it. The plane work is
  **hardening, not invention**: today the derived token is validated
  without consulting the live session registry (it lives as long as the
  daemon process) and is injected for external backends only. The
  hardening list: replace the bespoke secret-prefix SHA-256 with **HMAC
  or a standard KDF plus constant-time verification** (cryptographic
  hygiene — no practical length-extension exploit is claimed against the
  present parser); bind validity to a **live session**, revoked when the
  session or grant ends; keep the tokened URL **out of child argv**
  (argv + environment is the real local leak surface, not logs); bind
  local tokens to the intended local transport with a concrete v1
  promise — **loopback + bearer possession, or OS peer identity over
  Unix sockets/named pipes** — stated honestly: a loopback bearer alone
  cannot prove *which* same-UID process holds it (default mTLS already
  mitigates many non-loopback requests); make plane grants fail closed. **Two lanes, one boundary**: external process trees get this
  bearer-style rail with its inherited subprocess exposure stated; native
  supervised agents keep controller-bound principal dispatch — an MCP
  bearer is never injected into `intendant-runtime` (the
  runtime/controller boundary holds). The token attributes the supervised
  **process tree** (actor = `session-bound external principal`,
  propose-only capabilities); it does not identify one child subprocess
  or prove higher trust. Context cost: one skill description until
  invoked (vs MCP's always-loaded schema rent).
- **Channel 2 — transcript reflection (universal floor).** The controller
  owns every wrapper transcript; a supervised reflection step emits
  Memory *proposals* where **the reflection model/service is the actor
  and the external transcript/session is provenance** — a paraphrasing
  model must never attribute its interpretation as the external agent's
  own claim — with **exact source span/hash evidence**. Universally *available*, not universally
  automatic: it runs under session/space policy or end-of-session
  consent, honors provider/egress policy, binds to a destination zone,
  redacts, and is proposal-quota-bounded. Requires zero cooperation from
  the external — works for any backend, forever.
- **Channel 3 — MCP (optional projection, not default).** Exposed for
  MCP-only ecosystems; pays the context tax only where chosen.
- **One canonical declaration.** Honesty about today: HTTP and
  dashboard-control twins derive from the gateway route table, but
  MCP/ctl still runs a separate handwritten tool→operation map — the
  surfaces share IAM *vocabulary*, not one derived method table. New
  plane verbs enter **one canonical service/method declaration** from
  which all surface mappings (HTTP, tunnel, MCP/ctl, dashboard) and their
  parity tests derive — extending "derive, don't mirror" to the MCP
  surface rather than inheriting the split.
- Per-backend integration is a **declarative capability descriptor**
  (`memory_interface: ctl-skill | mcp | none`; `reflection: transcript`),
  never per-agent adapter logic.

## 9. The coordination bus (box-plane, ephemeral)

Live multi-session coordination state — dirty file sets, intents,
heartbeats, inter-session notes — is **not** owner-plane data (writing
per-turn churn into a diary log is spam and a sync storm). It is also not
daemon-internal state:

**Plain-text files are the substrate; the daemon is the interpreter and
accelerator; daemonless operation is a permanent fallback, not a
transition.** (Progressive enhancement — the same lesson as
ctl-over-MCP; mixed fleets coordinate through files today, provably.)

- **Location (decided)**: per-box, keyed by stable space —
  `~/.intendant/coordination/<stable-space-key>/`, exposed to sessions as
  `INTENDANT_COORDINATION_DIR`. The key derives from the **locally
  confirmed plane/binding/space tuple** (§2.3's binding map), never from
  trusting a copied public marker alone. Until stable space IDs exist,
  the fallback normalizes **all worktrees of one repository to one
  identity** — hashing each checkout path independently would blind the
  collision radar to exactly the collisions it exists to catch.
- **Per-writer files, never shared appends**: `sessions/<id>.md` (intent,
  dirty-set summary, heartbeat = mtime) and `messages/<writer>/<ulid>.md`
  (frontmatter + markdown, the ecosystem's lingua franca). No locks
  needed. This shares the **no-shared-append discipline** of per-writer
  feeds — not their cryptographic properties: same-UID files carry no
  writer identity or sequence integrity, and attribution is
  filesystem-grade only.
- **Safe v0 file protocol — specified before any fleet convention
  ships**: versioned frontmatter + UTF-8 rules; stable/sanitized IDs and
  filename grammar; atomic temp-write-plus-rename; byte, file-count,
  scan, and TTL bounds; no-follow opens with symlink and
  non-regular-file rejection; ownership/mode expectations; **separate
  daemon-writer paths** (the daemon never rewrites a session-owned
  file); explicit unverified same-UID attribution; GC/read race rules
  and daemonless cleanup behavior.
- **Trust ladder by channel**: file entries are quoted data and effect
  *proposals* at most (anything with your uid can forge them);
  token-bound ctl is an attributed session-bound principal; signed plane
  ops are the top. The daemon never executes effects sourced from bare
  files without the normal approval path.
- **Injection rule**: raw Markdown from this bus is never injected into a
  model turn. The daemon injects a **bounded, fixed-schema summary whose
  fields carry only daemon-derived structural facts** (never free-form
  text copied into supposedly safe fields);
  agents retrieve quoted bodies lazily (same discipline as memory
  retrieval, §6.5). Daemonless sessions reading files directly apply the
  standard quoted-data treatment the skill teaches.
- **One bus, not two**: when running, the daemon reads *and writes* the
  same files (daemonless guests see daemon-derived signals; daemon-aware
  sessions additionally get push — relevance-filtered reminders injected
  append-only into their next turn, which is prefix-cache-safe), plus
  peer relay, deterministic detection, and GC. No parallel drifting
  state.
- **Ephemeral by convention, with two hard rules**: mtime-TTL staleness
  (ULIDs and mtimes are *hints* — future timestamps are capped, never
  trusted), daemon GC, no secrets in the dir (box-local plaintext,
  daemon-plane trust) — and **TTL GC never deletes restart-critical
  state**: an orchestration checkpoint generation is removed only on an
  **explicit terminal record or successor acknowledgement** — a session
  vanishing from the live registry (a crash) is not evidence its restart
  checkpoint is no longer needed. It must never
  become a second durable memory store; durable lessons go through the
  Memory lifecycle.

**First feature: collision radar.** Two sessions whose dirty sets overlap
(or overlap an open PR's files) get flagged before they duplicate work.
Layering: detection = daemon, deterministic, zero-LLM — noting that the
current file watcher covers only the daemon's startup project and ignores
worktrees, so radar detection adds **explicit per-worktree watching or
deterministic git-status scans**, plus `gh` PR file sets; delivery =
injected reminders to both sessions + rail badge; reaction = skill-taught
negotiation; residue = a workflow-state note recording the agreed split.
The **degraded daemonless mode** (the skill alone: "before hot edits,
scan `coordination/sessions/*.md`; write your own") ships as an
experimental convention **only after the safe v0 file protocol above is
specified** — an unversioned fleet habit in CLAUDE.md would calcify.

## 10. Teaching architecture

The skill-description line is the only capability surface a model sees
unprompted; it is a scarce advertising space.

- **Split skills by trigger class.** Proactive behaviors (search memory
  before historied work; collision check before hot edits; agenda
  proposal when deferring) each get their own SKILL.md whose description
  is a **trigger pattern** ("Use BEFORE …"), not a feature list. Reactive
  verbs (screenshots, peers, audio) stay consolidated behind topical
  skills; the `intendant-cli` mega-skill becomes a router/reference, with
  `ctl --help` as the self-describing deep layer.
- **Derive the advertised skill index per session** from context
  predicates (worktree? peers? display? plane enrolled?) — the catalog
  pattern applied to teaching — **and from effective session
  authorization, not resource presence alone**: a memory nudge must not
  reveal that an unauthorized private space has history.
- **Seam hooks for duties — with the honest limit.** Ads are
  probabilistic; seams are deterministic; but a hook is still only a
  reminder. Doctrine: **hooks provide timely reminders where Intendant
  observes the seam; actual duties require enforceable tool/precondition
  gates** (first Edit → collision reminder; session start in a historied
  space → memory-search nudge; anything that *must* happen → a gate, not
  a hook).

## 11. Org and family planes (separate RFC — principles pinned)

Deliberately after the personal plane stabilizes. Pinned principles:

- **Provenance is not ownership.** Personal annotations of org-context
  events live in the personal plane with **cross-plane references**
  (never auto-copies); on offboarding, org keys rotate (ordinary
  revocation), references dangle gracefully (§6.3 — references confer no
  read authority), the diary survives. The org keeps its records; the
  person keeps their memories.
- Offboarding terms ride the **enrollment grant**, machine-readable,
  visible at join time.
- Classification labels + agent-honored flow policy govern cross-plane
  writes (`org-internal` annotate-freely vs `org-confidential`
  reference-only stubs). Default cross-plane behavior for *automated*
  flows is reference-only; user-authored annotation is the user's own
  content.
- **Honesty clause**: no cryptography prevents a person — or their
  personally-keyed agent — from remembering what they saw. "Strict mode"
  is controller/tool policy + hardware separation + attestation (org
  hardware holds no personal-zone write keys; org sessions run
  session-scoped egress; highly classified org spaces prefer mediated
  access over offline replicas), never cryptographic DLP.
- **Family-as-org is a product shape, not a governance solution**: one
  household root = one administrator. Joint governance (recovery,
  succession, quorum, divorce, unilateral revocation) stays parked for
  its own round.
- Precedent, stated accurately: today's org roots sign short-lived access
  documents **and** 365-day-capped `OrgIssuerCert` delegations plus
  revocation lists — useful precedent for delegated plane
  administration. Org planes still need real machinery (storage
  membership, key distribution, recovery, flow enforcement) — scoped
  there, not assumed here.

## 12. Connect: a plane-store contract, not endpoints

Connect today keeps one global mutex over a monolithic store with
whole-file JSON persistence, plus endpoint/IP rate limits and structural
caps — **no plane concept and no aggregate per-plane quota exists**. P2
is a storage project first: a separate plane-store subsystem, not an
extension of the current persistence shape. The contract to specify
before implementation (D0-B):

- account/device-to-plane **routing bindings** whose creation or mutation
  requires **both account authorization and a plane-issued proof** — an
  account attacker alone must not attach a victim plane for quota
  consumption or traffic metadata. Destructive deletion/GC requires a
  verifiable **current-root or succession proof**, never a signature from
  just any historical root. Bindings stay routing and quota only — never
  authority, per §0's invariant. Tests pin the four-way matrix
  (account-only / plane-only / both / neither) plus stale-root
  deletion/GC;
- immutable object identity = **hash of exact ciphertext**;
- transactional append/CAS and quota behavior;
- pagination/frontier cursors for incremental sync;
- partial-upload commit and corruption handling;
- **signed acknowledgements behind each durability state** (§4.4);
- root-signed checkpoint/GC fences (§4.6);
- **plane deletion**: root-signed deletion intent + a grace window + a
  recoverable tombstone, with **signed deletion cancellation,
  stale-admin-epoch, and replay semantics defined** — ordinary account
  takeover must not be able to use the legitimate API for immediate
  destruction. A malicious Connect can still delete or withhold
  ciphertext; independent replicas and exported recovery state remain the
  availability defense (§4.4);
- **distinct receipt types** — Connect durability, device replica,
  checkpoint witness — each naming its signer and its exact claim (§4.4's
  two axes);
- transactional semantics under **concurrent append/CAS races**, atomic
  persistence, and partial-commit recovery;
- deletion vs backup retention; crash-consistent backup, restore, and
  self-hosted migration **preserving objects, frontiers, tombstones, and
  receipt provenance**;
- aggregate per-plane storage quotas and abuse controls.

**Zero-daemon milestone**: today the hosted `/app` refuses to open
without a daemon ID, so the zero-daemon promise needs explicit exit
criteria in P2a — degraded hosted plane creation, IndexedDB outbox,
live-device second-browser enrollment, eviction re-sync, and a
hosted-provenance E2E. **Recovery is never "in a browser alone"**: loss
recovery runs through a signed native client or the owner's daemon, and
hosted code refuses recovery-phrase input (§3.5, §15).

## 13. Shared reducer and views

- A small pure crate (`owner-plane-core`) compiles native + WASM
  (precedent: `presence-core`) and owns canonical wire types,
  signature/capability validation interfaces, tenant reducer semantics,
  frontier/checkpoint types, deterministic projections, and test
  vectors. Crypto stays WebCrypto in browsers and Rust-native on daemons,
  pinned by shared golden vectors.
- **CI wiring is explicit**: the new crate is added to the required
  native CI lanes by name, and the Rust/browser golden vectors run as a
  required keyless job — the hand-listed test lanes do not automatically
  execute a new workspace crate.
- "Derive, don't mirror" permits **materialized views**: persisted
  projections stamped with the **data frontier, control
  frontier/checkpoint, reducer version, policy version(s), key epoch, and
  any evaluation-time parameters** — frontier + reducer version alone is
  insufficient when authorization, revocation, expiry, and tenant policy
  shape the derived view — rebuilt on parity failure (and
  encrypted/rebuildable per key domain, §6.2). Nothing re-folds a forever
  log per list render.

## 14. Threat model

| Threat | Required target posture (delivery-gated, §15 — most controls are to-build) |
|---|---|
| Malicious Connect storage | AEAD, signed per-writer chains, frontiers, witnesses; withholding remains possible and is documented |
| Malicious hosted UI bundle | §3.5 lanes: admitted TCB only for the degraded lane, genesis_provenance stamp, hosted ceiling, re-root nudge; trusted clients for admin ceremonies |
| Root compromise / competing successors | Versioned succession + recovery/witness adjudication rules (§3.1) |
| Account takeover at Connect | Root-signed deletion intent + grace window + tombstone; account credentials never mint plane authority |
| Compromised authorized writer | Capability scope, quotas, provenance, visible conflicts, revocation cutoffs |
| Writer fork (honest or malicious) | Fail-closed self-fork detection, witnessed-frontier reconcile, owner adjudication, new writer generation (§4.2) |
| Future-clock poisoning | HLC never decides authorization or executable winners |
| Memory poisoning | Candidate default, judgments + policy versions, provenance, bounded quoted-data retrieval, curated-only pins/procedures |
| Prompt injection via content | Markdown sanitization (to-build), no uncontrolled remote resources, data-not-instruction framing, effects only via approval |
| Time-bomb scheduling | Immutable digest-approved manifests, two-authority rule, scheduled-session principal, fresh-frontier predicate |
| Cross-zone exfiltration | Two-sided export, composition-bypass refusal, honest session-egress pairing |
| Stale cancellation | Enforceable freshness predicate before firing (§7.5) |
| Double firing | Single v1 executor, root-signed scheduler epochs + fencing on transfer, fsync'd occurrence ledger, idempotent creation, at-least-once + dedup |
| Unknown session outcome | Fail closed for high-impact work; surface `unknown`; manifest-declared retry-safety class (§7.5) |
| Crash around spawn | Durable Prepared/Started/Completed ledger, reattach-before-retry |
| Key loss | Passkey + mandatory recovery envelopes incl. checkpoint identity; device re-enrollment; drills (trusted lane) |
| Stolen daemon / evicted member | Capability revocation + epoch rotation; **ciphertext from held epochs stays decryptable by retained keys** (§4.5) — documented, not preventable |
| Storage spam / DoS | Ingestion/dispatch quotas (rate limits are not fold policy — folds stay deterministic); Connect per-plane quotas |
| Coordination-dir forgery / hostile files | Proposals/data only; safe v0 protocol (no-follow, bounds, atomic writes); fixed-schema injection summaries |
| Index / audit leakage | Encrypted or rebuildable-per-key-domain indexes; ID-only audit records (§6.2) |
| Schema downgrade | Versioned envelope/ops; unknown effectful operations fail closed |
| Compaction resurrection | Signed checkpoints + GC fences + coverage/abandonment rule; behind-fence re-proposal |
| Selective erasure | Per-item DEKs from v1; zone-rotation erase = key destruction; projections/backups inside the key boundary (§4.7); **cooperative replicas/backups only — retained keys/plaintext stay exposed (§4.5)** |
| Harvest-now / decrypt-later | Documented v1 deferral + KEM algorithm-ID agility; native-first hybrid path (§4.9) |
| Metadata leakage | Public segment manifest frozen + leakage documented (§4.0); minimized where cheap |

## 15. Delivery plan

### 15.0 Immediate, out of band

- **Live-audio always-consent fix — LANDED (PR #236, merged 2026-07-11)**:
  one centralized gate (`live_audio::request_spawn_consent`) enforced on
  the native and MCP/ctl dispatch paths before any audio side effect, with
  gate-leg and MCP-dispatch tests (native dispatch coverage is indirect);
  resolution races the approval-registry responder against the bus's
  ControlCommand verbs so it works across daemon shapes; ApproveAll
  approves the single prompt only. **Chokepoint hardening — LANDED (PR
  #238, merged 2026-07-11)**: the gate now mints a single-use, unforgeable
  `SpawnConsent` token and `run_session` requires it — a future dispatch
  path that skips the gate is a compile error, not a policy bug (the
  audit's guard-at-the-chokepoint recommendation, implemented by
  construction).
- Housekeeping (done 2026-07-11): the stale early draft with a
  newer-looking filename is archived; this file is the only canonical
  RFC.

### 15.1 Ordering rule and gates

**Four gates: D0-A (Core + Memory) freezes before durable P1 Memory
data; D0-B (Sync) freezes before P2 sync; D0-Agenda-Data freezes before
P3; D0-Agenda-Effects freezes before P5/P6.** Each gate ships as its own
normative specification — canonical encodings, reducers, adversary
boundaries, golden tests — and the umbrella RFC stops growing. UI and
service shapes may be prototyped at any point behind a "pre-protocol,
local-only" flag — but durable user data is written only in the D0-A
format, so P1 data never becomes a second legacy import problem.

- **D0-A — Core + Memory** (gate for P1 durable data): genesis
  descriptor + succession + independent recovery authority +
  `governance_scheme` + the control reducer with tagged
  `AuthorizationProof` (§3.1); mandatory v1 algorithm suite (§4.9's HPKE
  choice); DeviceCertificate/CapabilityGrant split + provenance evidence
  matrix (§3.2); hosted-to-trusted ceremony + ceiling invariant (§3.5);
  **the complete local object and key format** — canonical signed
  operation bytes + golden vectors (§4.1), the item AEAD
  suite/nonce/AAD + item-DEK wrapping, item ciphertext + key-envelope
  identities and `body_hash` semantics (§4.0/§4.7), the canonical
  frontier type (§4.2), tombstone/erase-marker format, atomic local
  log/outbox records; typed fail-closed plane
  IAM (§6.2's intersection); the exact Memory judgments/pins reducer
  incl. policy selection (§6.1); flow-admission rules (§6.3);
  deterministic expiry + receipt/lease proof types (§4.5). Migration
  invariant, stated once: **operations are immutable and never re-signed;
  containers, manifests, and projections may be rewritten; P1-to-P2
  migration re-encapsulates the exact stored operation bytes.**
- **D0-B — Sync** (gate for P2): segment container + public manifest +
  leakage documentation + AEAD/nonce/AAD/padding/addressing spec (§4.0);
  authorization-proof distribution (§4.8); the witness/zone-privacy
  design + frontier Merkle compression/witness proofs (the canonical
  frontier *type* freezes in D0-A, §4.2) + witness receipts (§4.3, §4.4);
  writer-fork recovery + multiwriter/writer-generation rules (§4.2);
  offline validity; checkpoint coverage/abandonment (§4.6); plane
  deletion (§12); durability-state acknowledgements (§4.4);
  erase-across-replicas mechanics (§4.7) + PQ record (§4.9); the Connect
  plane-store contract (§12).
- **D0-Agenda-Data** (gate for P3): the Agenda entry reducer — immutable
  entry revisions, causal parents/heads, the semantic-vs-presentation
  split, tag OR-sets, concurrent Complete/Reopen/Retire/patch outcomes
  (§7.2); Agenda data IAM (§7.9's data verbs); due-vs-fire presentation
  semantics; Journal include/remove curation (§7.7).
- **D0-Agenda-Effects** (gate for P5/P6): effect lineage (`effect_id`) +
  typed manifest variants (§7.3); approval/revocation/cancellation;
  scheduler lease + fenced manual transfer (§7.5); the executor contract
  + freshness predicate; notification delivery idempotency (§7.6);
  occurrence-ledger states incl. Unknown/Missed/Cancelled/Failed —
  everything demonstrated under test.

### 15.2 Phases

- **P0 — local kernel**: pure `owner-plane-core`; atomic local log +
  outbox; root/device ceremonies (trusted-lane, §3.5); native + browser
  crypto adapters; encrypted projections; required golden vectors wired
  into CI (§13).
- **P0.5 — coordination minimum** (one deliverable, independent of the
  plane): the safe v0 coordination file protocol (§9) **including the
  workflow-checkpoint message kind that replaces the tombed system's one
  live orchestration duty** — the orchestrator prompt's `store_memory`
  checkpoints (Appendix A). Degraded collision radar ships as an
  experimental convention on top.
- **P1 — controller-backed Memory** (no sync; D0-A format): one personal
  plane; explicit project/workflow spaces; claim bodies + judgments +
  pins; principal-bound Memory service + ctl verbs + skill (hardened
  session tokens, §8); bounded local lexical index; Memory Explorer UI;
  no whole-store prompt injection; **atomic tombed-system cutover
  (Appendix A), landed with the P0.5 checkpoint replacement**. *Exit:*
  attribution **device-signed and session-bound under the documented
  process-tree bearer threat model** (§8 — a loopback bearer cannot prove
  which same-UID process holds it); zone/space denial tested; conflicts
  represented, not overwritten; fresh sessions receive no unrequested
  memory.
- **P2a — storage and first sync**, five explicit sub-deliverables:
  durable store (the Connect plane-store subsystem, §12), client
  replica/crypto, sync, hosted shell, backup/migration. One browser + one
  daemon — **already two writers**, so P2a includes the minimum
  writer-generation, browser tab-lease, and two-writer convergence rules
  (§4.2); offline-convergence torture stays P2b. A **named direct
  browser↔daemon convergence test** is in P2a scope. **Zero-daemon exit
  criteria**: degraded hosted plane creation; ordinary unlock on an
  already-enrolled browser; live-device second-browser enrollment;
  eviction re-sync — replica/index loss while the device key and pin
  survive; **whole-origin eviction that destroys the IndexedDB identity
  is trusted-lane loss recovery, not re-sync**; hosted-to-trusted
  re-root; loss recovery through a
  signed native client or owner daemon; a test that hosted code
  **refuses recovery-phrase input** (a phrase-free onboarding check
  verifies recovery material exists — it is not a recovery drill);
  hosted-provenance E2E.
- **P2b — additional concurrent writers + offline-partition torture**
  (P2a already runs browser + daemon and live second-browser
  enrollment): offline convergence; multi-tab and restored-snapshot
  recovery (§4.2); revocation/rotation/checkpoints under churn.
- **P2c — mediation and flows**: disposable peers; typed peer/session
  IAM; cross-zone and cross-plane export/import (§6.3).
- **P3 — Agenda note/task tenant** (after D0-Agenda-Data):
  Add/Patch/Complete/Reopen/Retire; Agenda + Journal projections; no
  effects; provenance labels visible.
- **P4 — cross-links + promotion**: observation→memory→agenda→result→
  reflection flows; explicit exports; diary projection; bounded agent
  tools.
- **P5 — one-shot reminders** (after D0-Agenda-Effects): single explicit
  scheduler=executor daemon under a bounded lease (§7.5); UTC occurrence
  + original timezone; attention center + work-content-free push; quiet
  hours + caps; duplicate/missed policy. No voice/phone/recurrence; **no
  automatic election** — manual fenced transfer works.
- **P6 — one-shot scheduled sessions**: immutable revisions; explicit
  approval; scheduled-session principal; executor-local profile;
  preflight (cost/tokens/wall-time/credentials/network); occurrence
  ledger; result attestation + reflective proposal. Standing grants only
  after per-constraint enforcement is demonstrated.
- **External dependency**: the consumer-default genesis lane requires
  platform-signed clients — Apple notarization and Windows Authenticode
  move onto the consumer default's critical path (owner decision, §3.5).
  The transparency log covers release integrity meanwhile; GPG publisher
  signing for daemon/CLI artifacts is itself part of the deliverable
  (§3.5's per-artifact status).
- **Later**: durable questions/replies; user-created zones; sealed
  items; embeddings; foreign proposal inbox; peer gossip + witnessed
  frontiers; automatic scheduler election; recurrence +
  recurrence-instance identity; event/location triggers; blob/attachment
  storage (references reserved, §7.10); voice/phone escalation steps
  (per-item consent); org planes; family/multi-owner governance;
  person/contact entities.

## 16. Testing requirements

- **Protocol/fold**: op permutations + duplicate delivery; missing causal
  deps; `(writer,sequence)` and `request_id` fork cases; local log/outbox
  crash atomicity + browser transaction/multi-tab recovery; multi-tab
  writer sequencing; restored-snapshot self-forks; writer retirement +
  re-enrollment after a fork; future-HLC poisoning; concurrent
  patch/complete/reopen/retire; unknown op versions; checkpoint +
  old-replica resurrection; **incomplete-view checkpoint refusal +
  explicit writer abandonment**; cold-start rollback + split-view
  simulation; canonical malformed encodings; signature/AEAD malleability;
  parser/fold fuzzing with hard byte, reference, depth, and time bounds.
- **Authorization/crypto**: Rust/WebCrypto golden vectors as **required
  keyless CI**; genesis bootstrap + first certificate/grant via
  `GenesisAuthority`; stale admin epoch; deletion cancellation;
  signing-key continuation incl. the old-key-keeps-writing fork; KEM +
  zone epoch rotation; per-item DEK erase (index, view, checkpoint, and
  backup copies all unreadable after erase); **erase racing a checkpoint;
  erase across backup/restore; retained old keys/plaintext explicitly
  outside the guarantee**; zone-blind vs member-only witnessing +
  single-member-zone degradation;
  expired/revoked/wrong-zone/space/kind capabilities; **capability late
  arrival vs expiry deadline; per-writer budget exhaustion; signed
  strict/lenient revocation policy**; actor/signer mismatch;
  hosted-provenance ceiling (incl. attempted self-elevation); offline
  writer across a revocation cutoff; plane-capability-allowed but
  executor-IAM-denied; **root succession, competing successor histories,
  independent-recovery precedence over a compromised admin branch,
  algorithm migration**; hosted recovery-phrase refusal; live
  second-browser enrollment; trusted-client re-root; per-class provenance
  evidence + owner attestation; publisher-key compromise;
  recovery-custodian compromise; provenance-evidence spoof/relay;
  **session-rail suite**: HMAC/KDF golden vectors + constant-time
  verification, cross-session / post-termination / post-revocation token
  refusal, argv absence, runtime boundary (no bearer reaches
  `intendant-runtime`), method parity across surfaces.
- **Memory**: candidate vs accepted retrieval; `assert` vs `propose`
  folds (the self-accept judgment); pin revocation; underclassification +
  declassification; composite export; concurrent
  Accept/Dispute; contradictory claims; supersession + expiry; injection
  payloads retained as quoted data; procedures/preferences denied on the
  ordinary retrieval route; bounded retrieval/token budgets; **identical
  allow/deny requests through native, ctl/MCP, dashboard, and peer
  surfaces** incl. `memory.search`-allowed with `memory.read`-denied on
  every surface; export allow/deny incl. read-A+write-B composition
  refusal; mediated search without zone key; ID-only read audit.
- **Agenda/scheduling**: approve-A-then-edit-to-B; concurrent approved
  heads fail closed; **multiple effects per entry (per-`effect_id`
  fireability); display due date vs firing time**; approval revocation;
  Complete/Retire canceling occurrences; Reopen not refiring a past
  one-shot; stale cancel/reschedule; freshness-predicate failure;
  **manual scheduler transfer during partition + fenced old-scheduler
  return; unavailable executor as a material revision; unknown session
  outcome; duplicate notification delivery**;
  clock jumps + sleep/wake; crash before launch / after launch / before
  result; duplicate occurrence delivery; missing credentials; unavailable
  project binding; headless approval; scheduled-session sandbox/IAM
  enforcement; **notification endpoint consent, quiet hours, cost caps,
  provenance ceiling**.
- **Storage/ops**: segment corruption/truncation; partial upload + retry;
  concurrent append/CAS/deletion/GC races + partial-commit recovery;
  quota exhaustion; **root-signed deletion + grace recovery + signed
  cancellation**; malicious-store deletion; backup restore; **browser
  eviction + loss of the last local checkpoint**; plane deletion + GC; self-hosted
  migration; **zero-daemon Connect-app E2E**; macOS/Linux/Windows +
  sleep/clock behavior.
- **Coordination**: adversarial files — symlinks, path traversal, torn
  writes, oversized frontmatter, forged writers, future-mtime attacks,
  copied/forged space-marker rejection, worktree-normalization identity,
  TTL/GC races, orchestration-checkpoint survival across context restart,
  prompt payloads retained as data.
- **Cutover**: a CI absence test proving the old Memory names,
  capabilities, prompts, schemas, and control messages are gone
  (Appendix A).

## Appendix A — tombed Memory cutover inventory

Live hooks requiring deliberate removal/replacement in P1 (corrected and
expanded against source):

- the runtime `store_memory`/`recall_memory` tools, their command fields,
  and their **model-facing tool schemas and name maps**;
- the controller's `.intendant/memory.json` path injection and the
  runtime's unlocked whole-file JSON read-modify-write;
- **both** whole-store injection sites that feed stored memory into fresh
  conversations as user-role content;
- `inherit_memory` — a **spawn argument and supervisor wiring field, not
  a TOML config key** — plus the `[memory]` config section;
- the control-plane/MCP recall path (`RecallMemory` control message and
  its handlers), **including the silent-success MCP stub** that
  acknowledges recalls it never performs;
- the orchestrator prompt's `store_memory` checkpoint mandate — replaced
  by the P0.5 coordination-dir checkpoint kind **before or atomically
  with** this cutover (deleting the old system must not regress
  orchestration);
- Presence's conflation of durable memory, voice transcripts, and
  session-log search; prompt text teaching the old system;
- recall vocabulary in browser voice — **two JS fragment hooks**, so the
  cutover regenerates `static/app.html` from fragments *and* the
  affected WASM artifacts via the canonical builder;
- the federation `Knowledge` capability advertisement with no peer
  implementation (remove and parity-test);
- `intendant_core::knowledge`;
- the remaining inventory, swept against source at cutover time: the
  dashboard/web recall dispatch paths; Presence core/web schemas and
  routing; activity/IAM/peer hooks; built-in tool-count tripwires; every
  prompt hook; the repo `[memory]` config section; the systems' tests and
  companion docs; and **`CLAUDE.md` + byte-identical `AGENTS.md`**, whose
  "authority is only ever minted by the target daemon's local IAM"
  sentence is amended to §0's two-boundary invariant in the same change;
- a **CI absence test** over prompts, schemas, runtime fields, Presence,
  configuration, control messages, browser fragments, and agent cards —
  built on an **exact legacy-identifier denylist**, never a ban on the
  word "memory"; it excludes unrelated Codex `/memory-reset` and
  deliberate no-knowledge paths. Existing `.intendant/memory.json` files
  stay **inert** until explicit forensic import or deletion — never
  auto-ingested, never silently deleted.

Optional one-shot forensic importer only if old data is wanted — imported
entries become quarantined, untrusted legacy observations with new
provenance; old channels/cursors/IDs/timestamps/model-supplied sources
are never trusted semantics.

## Appendix B — resolved-decision log (chronological)

1. Owner-plane as a storage class; Agenda as first product; promotion,
   not sync, from agent memory. *(round 1)*
2. Zones = key domains; sessions-only execution; foreign proposal inbox;
   vault = sibling with opposite key policy. *(round 1)*
3. Org seam: provenance ≠ ownership; principal planes; cross-plane
   references; enrollment-grant offboarding contracts; honesty clause.
   *(round 2)*
4. First review adopted nearly wholesale: plane root + device certs
   (sign + KEM); two-resource authority; per-writer chains + frontiers;
   two-epoch revocation + cutoffs; durable outbox states;
   checkpoints/GC; kinds×effects; immutable manifests; single-executor
   occurrence ledger; Memory-first; org RFC split; family-as-org ≠
   governance. Nuances: D0 blocks P2 not P1 (later refined by decision
   8); `agents` default zone dropped; witnesses v1 = own devices +
   transparency log. *(review round 1)*
5. External agents: native memories out of scope; ctl-skill primary with
   session-token binding; transcript reflection universal; MCP optional
   projection; declarative descriptors. *(round 3)*
6. Teaching: trigger-class skill split; per-session derived skill index;
   seam hooks. Memory adopts the skills *shape*, not trust class; skills
   = compiled memory via explicit graduation. *(round 4)*
7. Coordination bus: **files as substrate, daemon as accelerator,
   daemonless fallback permanent** (user pushback accepted); per-writer
   files; proposals-only file lane; one bus; ephemeral by convention;
   collision radar as v1 feature with a degraded daemonless mode.
   *(round 5)*
8. Synthesized review adopted nearly wholesale *(review round 2,
   2026-07-11)*: **D0-A/D0-B gate split** (durable-format-before-durable-
   data); lifetime authority model (genesis/succession/recovery/
   governance_scheme); DeviceCertificate/CapabilityGrant split with hash
   references; typed fail-closed plane IAM (five-way intersection);
   Memory reducer = immutable bodies + append-only judgments + pins,
   derived status; auto-context = accepted AND pinned; two-sided export
   + composition-bypass refusal; graduation = six-step gated daemon
   effect; segment manifest layering + leakage documentation;
   authorization-proof distribution as a D0-B decision; writer-fork
   vocabulary + recovery; deterministic expiry/budgets/revocation
   policy; checkpoint coverage/abandonment; retention hierarchy;
   root-signed plane deletion + grace; zero-daemon durability floor;
   Connect plane-store contract; Agenda IAM + explicit effect lifecycle
   + the fail-closed fireability rule; scheduler epochs + fencing;
   freshness as enforceable predicate; unknown-outcome fail-closed;
   "occurrence ledger" naming; recurrence/event-triggers re-parked;
   blob references reserved; session-rail hardening (exists, not new);
   reflection available-not-automatic; one canonical method declaration
   incl. MCP; coordination location per-box space-keyed with worktree
   normalization; safe v0 file protocol before any fleet convention;
   no-raw-markdown injection; authorization-aware skill advertising;
   hooks-vs-gates doctrine; testing additions; all vocabulary/source
   corrections. Out-of-band: live-audio always-consent bypass found in
   main (both dispatch paths) → standalone security fix (§15.0).
9. Either/or picks *(2026-07-11)*: per-item content DEKs from v1 (not
   "reserved"); PQ = explicit deferral + KEM algorithm-ID agility +
   native-first hybrid; P0.5 = one deliverable (coordination protocol ∪
   orchestration-checkpoint replacement).
10. **Owner decision (2026-07-11): signed-client genesis is the default;
    hosted-tab genesis is the degraded lane only** — flipping the earlier
    recommendation's emphasis. Trusted clients = platform-signed native
    apps or the user's own daemon; admin ceremonies (enrollment,
    recovery, provenance elevation) trusted-lane only; degraded lane =
    admitted Connect TCB + `genesis_provenance: hosted` + hosted ceiling
    + aggressive re-root nudge; recovery phrase never entered in a hosted
    tab. Accepted consequence: Apple notarization + Windows Authenticode
    on the consumer default's critical path; GPG + release transparency
    suffices for daemon/CLI/self-hosted. *(§3.5)*
11. **v3 synthesized review folded as v3.1** *(2026-07-11; owner ratified
    drafts-only + the picks)*: four-gate split (D0-Agenda-Data added
    before P3; the complete local format moved into D0-A + the migration
    invariant); tagged `AuthorizationProof` + control-reducer
    epoch/precedence rules; independent recovery authority + branch
    precedence (loss ≠ compromise; re-root rotates both);
    hosted-to-trusted ceremony + ceiling as a reducer invariant derived
    from genesis provenance; per-class provenance evidence matrix; one
    encryption object graph (item = encryption unit); **zone-rotation
    erase** (erasable key-envelope store rejected); two-axis durability
    (availability × rollback evidence); revocation boundary = held-epoch
    ciphertext; receipt/lease proof types frozen in D0-A; Memory reducer
    completion (Retire judgment + Unpin; `supersedes[]` advisory;
    `assert` = claim + self-accept judgment under a named policy; pins
    bind their acceptance; disputed pins fail closed for auto-context);
    derived monotonic destination-aware classification +
    retrieval-to-remote-model-is-egress + composite export primary;
    per-`effect_id` lineage + typed manifest variants + content-addressed
    profile/grant/evaluator refs + `agenda.effect.execute`; **v1
    scheduler = executor same-daemon invariant under a bounded firing
    lease, fenced manual transfer** (old-scheduler ack or lease expiry —
    a newer epoch alone fences nothing); occurrence states
    Unknown/Missed/Cancelled/Failed + notification delivery idempotency;
    `question` kind parked to Later; **pure hosted-genesis plane = notes/
    tasks/reminder drafts only** (owner-ratified); **KEM = HPKE
    DHKEM(P-256, HKDF-SHA256) + HKDF-SHA256 + AES-256-GCM v1** (matches
    shipped vault custody; X25519/ML-KEM ride the algorithm ID); P2a
    five sub-deliverables + minimum two-writer rules + no
    recovery-in-browser; session-rail HMAC/KDF + argv-not-logs correction
    + the two-lane boundary statement; reflection actor = the reflection
    model; coordination future-mtime caps + checkpoint retention vs GC;
    source corrections (attention center; work-content-free push; drills
    to-build; `intendant.toml` untracked; per-artifact release-pipeline
    status; threat-table column). The review's live-audio section was
    stale (written pre-PR #236); its residual chokepoint recommendation
    landed as PR #238 (§15.0).
12. **Go-ahead audit errata applied; umbrella FROZEN** *(2026-07-11, per
    `~/agenda-owner-plane-rfc-v3.1-review.md` — "go with edits", no v3.2
    cycle)*: §4.1 header carries the `AuthorizationProof` union itself,
    plus the D0-A control-tenant/namespace choice for plane-wide control
    ops; `authored_crypto_epoch` (signed) split from the mutable outer
    `key_wrap_epoch` so zone-rotation erase never rewrites signed bytes;
    item AEAD suite/nonce/AAD + DEK wrapping moved into D0-A; the
    frontier *type* frozen in D0-A (`AdminEpoch` signs it); re-root
    authorized by the `RecoveryAuthority` arm (a later-compromised hosted
    root cannot self-succeed), enrollment = ceremony outcome, hosted
    branch retired; §3.5 delivery/execution approval restored to §3.4's
    intersection; §4.4 phrase-free onboarding check (drills =
    trusted-lane); erasure claims bounded to cooperative
    replicas/backups; composite export primary with paraphrase residual
    stated; cross-plane classification = lower bound in provenance;
    judgment shapes completed (`Supersede{target, replacement}`,
    `Declassify`, `MemoryUnpin{target_pin}`) + `memory.evidence.read`
    named; Audit completeness relative to checkpoints/GC fences;
    freshness predicate covers notification delivery; Connect routing
    bindings require account ∧ plane-issued proof, deletion/GC needs a
    current-root/succession proof; rail transport promise =
    loopback+bearer or OS peer identity, P1 exit reworded to the
    process-tree bearer threat model; P2a eviction split + named
    browser↔daemon convergence test; P2b = additional writers +
    partition torture; coordination checkpoints removed only on explicit
    terminal record or successor ack; release status corrected against
    `release.yml` (manifest log + Connect-signed tree heads ship;
    GPG-signed daemon/CLI releases, notarized and Authenticode clients =
    deliverables; HPKE is new work, only its components match the vault);
    PR #236 test wording made precise; PR #238 MERGED. Next documents:
    the four gate specifications.

## Appendix C — remaining open questions

1. **Authorization-proof distribution shape** (D0-B): per-zone control
   feeds vs hash-fetched capability objects vs Merkle
   inclusion/non-revocation proofs (§4.8) — including what zone members
   see, what Connect sees, and the offline floor; paired with the
   witness/zone-privacy choice (zone-blind commitments vs member-only
   witnessing, §4.3).
2. **Offline expiry rule confirmation** (D0-A): working position is
   acceptance/witness-deadline expiry + online-only leases for
   high-impact capabilities + epoch/sequence budgets for offline writers
   (§4.5); confirm against real offline usage before freeze.
3. **Recovery custodian/threshold specifics and drill UX** (D0-A): the
   independent recovery authority is now required (§3.1) — open is who or
   what holds it (offline key, second device, printed phrase, trusted
   contact, threshold custodian set), threshold rules, and drill cadence.
   Phrase-entry-on-trusted-lane-only is already decided (§3.5).
4. **Native-side hybrid ML-KEM timing** (post-v1): when to turn on hybrid
   wraps for daemon recipients (§4.9); agility is already in the format.
5. **Coordination v0 constants** (P0.5): byte/file-count/scan/TTL bounds
   and per-box sizing for the coordination dir (§9).
6. **Org/family plane RFC scheduling** (post-personal-plane
   stabilization; principles pinned in §11).
