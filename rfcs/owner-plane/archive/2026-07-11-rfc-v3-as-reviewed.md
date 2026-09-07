# RFC: The Owner Plane, Memory, and Agenda (v3 / D0)

*2026-07-11. Supersedes v2 (archived: `~/agenda-rfc-archive/2026-07-11-rfc-v2-as-reviewed.md`)
by folding `~/agenda-owner-plane-rfc-synthesized-review.md` — itself a synthesis
of two independent reviews checked against current source — plus the owner's
genesis-lane decision (§3.5). Self-contained: reviewers and implementers need
no other document. Companion chapters once implemented: `docs/src/owner-plane.md`
(new), touching `trust-architecture.md`, `trust-tiers.md`,
`credential-custody.md`, `self-hosted-rendezvous.md`, `autonomy.md`,
`mcp-server.md`.*

> **Status — adoption statement.** Accepted as product architecture and D0
> requirements. The authority format, wire protocol, and tenant reducers
> remain **provisional** until the three specification gates are specified
> and golden-tested: **D0-A** freezes before any durable P1 Memory data,
> **D0-B** freezes before P2 sync, and the **Agenda-effects gate** freezes
> before P5/P6 (§15). Product UI and service shapes may be prototyped
> earlier; durable user data is never cut over into a provisional format.

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
by both paths, with per-path tests (§15.0).

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
| daemon-plane | one box | none | sessions, logs, caches |
| project-plane | one repo | with the repo | `intendant.toml`, instruction files, space marker |
| **owner/principal plane** | one principal | Connect (or self-hosted), E2E | Memory, Agenda; later: preferences, diary |
| **coordination dir** (§9) | one box, ephemeral | none (daemon may relay) | session heartbeats, dirty sets, inter-session notes |

### 2.3 Space identity

The stable space ID for a repository lives in a **small, deliberately
tracked marker file** — not `intendant.toml`, which is git-ignored local
operational configuration and may contain executable MCP configuration. A
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
  `genesis_provenance` — §3.5) with `plane_id = hash(genesis descriptor)`;
- versioned **administrative-key epochs** and an algorithm-migration path;
- **distinct loss-recovery and compromise-recovery** procedures, with
  recovery threshold/custodian rules;
- deterministic handling of **competing successor chains**: a root
  compromised before rotation can sign a rival successor, so a simple
  root-signed successor operation is necessary but not sufficient —
  recovery and witness/quorum rules must state how clients resolve the
  fork;
- **authenticated ordering** for enrollment, revocation, checkpoints, and
  scheduler epochs (the control history is itself a per-writer feed);
- `governance_scheme` v1 implements only a single-person root, but the
  field is versioned so org/multi-party governance is an upgrade, not a
  fork.

The root private key is sealed under vault-style, domain-separated passkey
custody with **mandatory recovery envelopes**. The existing vault is real
precedent — domain-separated passkey envelopes, a generated BIP39 recovery
phrase, recovery drills — and its cryptographic envelope pattern is
reused. It does not answer plane *governance*: today's daemon and Connect
validators merely require a nonempty envelope array, so the plane layer
explicitly enforces its own chosen recovery policy. Root operations
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
  itself as native nor mint anything above its ceiling.
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
  is valid;
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
  serves — code from the GPG-signed, release-transparency-logged build,
  served by hardware the user controls).
- **Degraded lane**: hosted-tab genesis survives solely for the true
  zero-install persona, and is explicitly labeled degraded. For that lane,
  Connect-served code is **admitted as the administrative TCB** — E2E
  encryption blinds Connect's storage, not the JavaScript it serves, and a
  passkey gesture proves presence to code, not that the code presented the
  transaction the user believes they approved. Consequences are structural,
  not cosmetic:
  - the plane's genesis descriptor is stamped `genesis_provenance: hosted`
    (honest evidence, carried forever);
  - hosted devices sit under a **ceiling**: no provenance elevation, no
    cross-plane/high-impact export approval, low-effect read/write
    capabilities otherwise;
  - at the **first trusted-device enrollment**, the product aggressively
    nudges a **re-root succession ceremony** onto a native-held root —
    this is where §3.1's succession machinery pays for itself;
  - the **recovery phrase is never entered in a hosted tab**. Recovery and
    recovery drills run on the trusted lane only. (Display at hosted
    genesis is unavoidable for that lane and is covered by the admitted
    TCB; re-entry is not.) A hosted-genesis owner who loses their only
    device installs a signed client or claims a daemon to recover.
- **Accepted consequence**: platform developer signing — the previously
  deferred Apple-secrets step, plus Windows Authenticode — moves onto the
  **consumer default's critical path**. GPG + the existing
  release-transparency log remains sufficient for the daemon/CLI/
  self-hosted lane (Linux consumer entry included).

High-urgency delivery and execution approval likewise require a
trusted-origin device or independent executor-local approval; browser-only
mode remains a real product promise, explicitly labeled as this lane.

## 4. Wire protocol

### 4.0 Layering: public manifest, ciphertext, signed operation

The signed header carries plane/space/actor/capability metadata, yet
Connect must see almost none of it. Both hold only with explicit layers:

```
PublicSegmentManifest              # what Connect sees and indexes
    opaque routing/feed handles, crypto epoch, segment id,
    ciphertext length + content address, recipient wraps,
    incremental-sync cursor/range
AEAD ciphertext
    exact SignedOperation bytes[]  # stored verbatim; no re-serialization
SignedOperation                    # §4.1 — visible only inside the zone
    authorization refs, writer chain, causal refs, operation body
```

D0-B freezes the exact public schema **and documents its leakage**:
Connect need not see real signing-key IDs (it indexes opaque feed
handles), but incremental sync inevitably exposes stable routing handles,
segment order/ranges, sizes, and timing. D0-B also specifies: AEAD choice,
nonce construction, AAD binding (manifest ↔ ciphertext), compression and
padding policy, partial-upload commit, ciphertext content addressing,
recipient-wrap binding, and whether `body_hash` covers canonical plaintext
or stored ciphertext.

### 4.1 Envelope (minimum signed header)

```
protocol_version, tenant, plane_id, zone_id, space_id,
crypto_epoch, capability_epoch,
signer_algorithm, signer_key_id, certificate_hash,
writer_id, actor_principal, capability_hash,
request_id, writer_sequence, previous_writer_hash, causal_references,
created_hlc, operation_type, operation_version, body_hash
```

- `writer_id` is explicit and **not silently equated with the signing
  key**: signing-key rotation continues a writer chain under the same
  `writer_id` via a root-authorized continuation record.
- Writer-chain scope is `(plane, zone, writer)`.
- `request_id` is the idempotency handle (duplicate `request_id` with
  different bytes is fork evidence, §4.2).
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

- atomic single-writer sequencing locally (fsync'd sequence allocation);
- multi-tab/browser writer coordination (one tab holds the writer lease);
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
the existing Connect transparency log. No new witness network. (Proof
distribution — how replicas obtain the certificate/revocation material to
*validate* what they fetch — is §4.8.)

### 4.4 Durability states

A local replica is a **durable replica + outbox**, not a cache: before
acknowledgement elsewhere, it is the only copy of an offline write. The
UI exposes four states:

```
local-only → server-durable → replica-acked(n) → checkpoint-witnessed
```

"A wiped box loses nothing" begins at `replica-acked`; `server-durable`
is honest-service durability only (a Connect acknowledgement, not
Byzantine availability).

**The zero-daemon lane needs its own floor, stated honestly**: browser
storage may be evicted by the platform; a single device provides no
cross-device witness; a transparency commitment proves a commitment but
does not hold ciphertext; and losing the local pinned checkpoint may also
lose rollback protection. Onboarding therefore uploads aggressively,
creates/exports recovery material that includes the **latest trusted
checkpoint/log identity** (not just the phrase), and exercises a recovery
drill. A Connect-owned transparency log alone is not an independent
witness for a client that loses all local pins.

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
Historical plaintext already decrypted by an evicted member is not
recoverable — the standard E2E-group limitation, stated in docs.

Because HLC is not authority, an operation arriving after capability
expiry cannot prove it was authored before expiry. Working position
(D0-A confirms): **expiry is an acceptance/witness deadline** for
ordinary capabilities; **high-impact capabilities are online-only
leases**; offline writers can be given **epoch/sequence budgets** instead
of wall-clock authority. Operation/byte budgets are per-writer or
escrowed (a shared pool cannot be divided deterministically offline).
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
survives replicas and backups; tenant volumes are modest (a decades-long
diary is millions of items, not billions — wrap overhead is bytes per
item); and retrofitting item granularity later is precisely the format
break D0-A exists to prevent.

**Erase** therefore means: destroy the item DEK's availability
(re-wrap the zone KEK epoch excluding it; delete wrapped copies), leaving
a minimal audit hash/tombstone. Projections, indexes, checkpoints, and
backups must never retain plaintext outside the item-key boundary — they
hold ciphertext under the same key domain or are rebuildable and are
rebuilt/erased on revocation and erasure (§6.5). Erasure has
segment/compaction latency; UI language must match the implemented
granularity and latency, not promise instant global destruction.

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

- v1 recipient wraps use X25519 (the only KEM-capable primitive with a
  real browser custody path today — WebCrypto specifies X25519 and not
  ML-KEM);
- **every recipient wrap carries a KEM algorithm ID**, so hybrid
  X25519+ML-KEM (FIPS 203; Signal PQXDH as precedent) can be added
  without a format break;
- harvest-now/decrypt-later is accepted and documented as v1 residual
  risk for a forever-retention diary;
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

Judgment (append-only): Accept | Dispute | Supersede | Retract {
  target_claim (causal ref), actor, capability_hash,
  policy_id + policy_version, reason / evidence_refs
}

MemoryPin (append-only, owner-curated) {
  target_claim, destination_space/role, expiry,
  token_budget, provenance_floor
}
```

- Effective status (`candidate`, `accepted`, `disputed`, `superseded`,
  `retired`) is a **derived view** over judgments, computable per policy
  version; concurrent Accept and Dispute both survive and both surface.
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
plus separately authorized **evidence dereferencing** (`memory.read` on a
claim does not imply reading its evidence refs). `search` and `read` are
distinct operations: search returns bounded index entries; read returns
bodies.

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
  must not become a second copy of the sensitive data). Owner-visible in
  the plane's admin history.
- **Indexes are sensitive**: lexical/FTS indexes are encrypted under the
  same key domain or rebuildable-and-rebuilt; they are removed/rebuilt on
  revocation and erasure (§4.7).

### 6.3 Export is two-sided; projections respect the boundary

Read-in-A plus write-in-B *is* export; the mediated service refuses that
composition without a flow grant (or requires a composite export
operation carrying source hashes), so separate calls cannot bypass export
policy.

- **Same-plane, cross-zone**: requires source read + an explicit flow
  grant + destination write:

```
memory.export { from(plane,zone,space) -> to(plane,zone,space),
                allowed_kinds, classification_ceiling, expiry }
```

- **Cross-plane**: requires **two independent authorities** — source-plane
  release/export *and* destination-plane import/write. The imported claim
  begins as a **candidate** under the destination's own zone identity;
  source acceptance and classification do not transfer; the immutable
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

The flow **generates a draft**; curation alone never activates
future-agent instructions. Nothing automatic, not v1.

## 7. Agenda (second tenant)

### 7.1 Kinds and effects are orthogonal

```
entry_kind = note | task | question
effects    = notification(policy)
           | session_launch(immutable_manifest_hash)
```

(The v1 T0–T3 ladder is dropped: it collided with trust-tier vocabulary
and promoted effects into kinds — and omitted plain `task`.) Notes and
tasks may carry reminders; tasks may or may not launch sessions;
questions have reply semantics; urgency is separate from channels.

### 7.2 Operations

`Add`, field-level `Patch` (non-effectful presentation metadata only),
`Complete`, `Reopen` (explicit transition — no "monotone but
un-completable" hand-waving), `Retire`, `Reply`/`Answer`,
`ProposeEffect`, `ApproveEffectRevision`, `RejectEffectRevision`,
**`RevokeEffectApproval`**, **`CancelOccurrence`**,
`RecordOccurrenceStarted`, `RecordOccurrenceResult`. Cancellation is a
first-class operation, never implied by tests. Tags, if an OR-set, carry
real observed-remove semantics.

### 7.3 Executable state is atomic; effect lifecycle is explicit

Fieldwise merge must never compose an executable state no author
proposed. Every effect revision — **including notification policy** — is
an **immutable manifest referencing the immutable semantic entry revision
it belongs to**; approval signs the manifest digest. Manifest contents:
goal; occurrence/due instant with timezone interpretation; executor
daemon; local execution profile; project/space binding; backend/model;
sandbox; filesystem roots; network policy; credential requirements and
failure behavior; autonomy ceiling; token/cost/wall-time/concurrency/
subagent limits; retry/misfire policy; **declared retry-safety class**
(§7.5); approval expiry; standing-grant ID + evaluator version. **Any
material edit or reschedule creates a new proposed revision and
invalidates the old approval.**

Lifecycle rules (D0-A freezes these):

- `RevokeEffectApproval` and `CancelOccurrence` take effect at the
  freshness predicate (§7.5); a revoked approval never fires.
- `Complete` and `Retire` **cancel pending occurrences** of the entry's
  effects.
- `Reopen` restores the entry's semantic state only; a past one-shot
  occurrence **never refires** without a new approved effect revision.
- Reschedule = supersession: new revision, old approval invalid.
- Concurrent approvals / conflicting heads fail closed:

> **An occurrence is fireable only when exactly one causally maximal,
> approved, unrevoked effect revision applies. Any unresolved concurrent
> head disables firing.**

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

- **one explicitly selected `scheduler_daemon_id`** per plane (or per
  item override). Not "the anchor" — the anchor serving role is
  deliberately fungible; overloading it couples two lifecycles.
- `occurrence_id = hash(item_id, approved_revision_hash, due_instance)`;
- a **daemon-plane occurrence ledger** ("ledger", reserving "journal" for
  the product view, §7.7), fsync'd before spawn:
  `Prepared → Started(session_id) → Completed(result_hash)`; session
  creation idempotent by occurrence ID; crash recovery reattaches before
  retrying;
- **at-least-once with deduplication**, never exactly-once, stated
  honestly — and honestly bounded: idempotent *session creation* does not
  make arbitrary external actions inside a retried session idempotent.
  If a session disappears with unknown effects, the executor **fails
  closed for high-impact work and surfaces `unknown` for owner
  resolution** unless the manifest's declared retry-safety class permits
  re-execution;
- session-launch execution requires a sufficiently **fresh
  control/revocation frontier**, where "fresh" is an enforceable
  predicate frozen in D0-A: required control epoch/checkpoint, maximum
  age, witness class, and failure behavior. A Connect response alone
  cannot prove a cancellation was not withheld; stale offline state fails
  closed absent an explicit bounded offline grant;
- **scheduler transfer is an owner ceremony**: a monotonic, root-signed
  **scheduler epoch** fences the old executor — manual reselection *is*
  failover, and without fencing a returning offline daemon double-fires.
  Automatic election stays deferred;
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
narrowly scoped standing consent. Current reality (source-verified):
attention rail + content-free Web Push exist; voice is a future
attachment point; live audio always requires human approval **as policy —
the enforcement gap in main is the §15.0 standalone fix**. Voice/phone
are designed as future escalation steps — never described as an existing
ladder.

### 7.7 Three views over the same substrate

**Agenda** (actionables), **Journal/Diary** (curated chronology across
selected Memory episodes and completed Agenda occurrences — an operation
log is not automatically a pleasant diary; persisting a narrative is an
export, §6.3; source-side inclusion is an explicit curation
operation/judgment), **Audit** (complete signed history: edits,
approvals, device changes, execution attestations). "Occurrence ledger"
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

- `agenda.search / read / propose / write / complete / answer`;
- `agenda.effect.propose / approve / revoke`;
- `agenda.occurrence.record`;
- channel/notification rights (per §7.6's intersection);
- `agenda.admin`.

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
  daemon process) and is injected for external backends only — so add
  live-session validation and revocation, cover all native supervised
  children, redact the token from logs, bind it to local transport, and
  make plane grants fail closed. The token attributes the supervised
  **process tree** (actor = `session-bound external principal`,
  propose-only capabilities); it does not identify one child subprocess
  or prove higher trust. Context cost: one skill description until
  invoked (vs MCP's always-loaded schema rent).
- **Channel 2 — transcript reflection (universal floor).** The controller
  owns every wrapper transcript; a supervised reflection step emits
  Memory *proposals* with signer/actor attribution and **exact source
  span/hash evidence**. Universally *available*, not universally
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
  `INTENDANT_COORDINATION_DIR`. Until stable space IDs exist (§2.3), the
  fallback normalizes **all worktrees of one repository to one
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
  model turn. The daemon injects a **bounded, fixed-schema summary**;
  agents retrieve quoted bodies lazily (same discipline as memory
  retrieval, §6.5). Daemonless sessions reading files directly apply the
  standard quoted-data treatment the skill teaches.
- **One bus, not two**: when running, the daemon reads *and writes* the
  same files (daemonless guests see daemon-derived signals; daemon-aware
  sessions additionally get push — relevance-filtered reminders injected
  append-only into their next turn, which is prefix-cache-safe), plus
  peer relay, deterministic detection, and GC. No parallel drifting
  state.
- **Ephemeral by convention**: mtime-TTL staleness, daemon GC, no secrets
  in the dir (box-local plaintext, daemon-plane trust). It must never
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

- authenticated account/device-to-plane **routing bindings** (routing and
  quota only — never authority, per §0's invariant);
- immutable object identity = **hash of exact ciphertext**;
- transactional append/CAS and quota behavior;
- pagination/frontier cursors for incremental sync;
- partial-upload commit and corruption handling;
- **signed acknowledgements behind each durability state** (§4.4);
- root-signed checkpoint/GC fences (§4.6);
- **plane deletion**: root-signed deletion intent + a grace window + a
  recoverable tombstone — ordinary account takeover must not be able to
  use the legitimate API for immediate destruction. A malicious Connect
  can still delete or withhold ciphertext; independent replicas and
  exported recovery state remain the availability defense (§4.4);
- deletion vs backup retention; crash-consistent backup, restore, and
  self-hosted migration;
- aggregate per-plane storage quotas and abuse controls.

**Zero-daemon milestone**: today the hosted `/app` refuses to open
without a daemon ID, so the zero-daemon promise needs explicit exit
criteria in P2a — plane creation and recovery in a browser alone,
IndexedDB outbox, second-browser restore, and a hosted-provenance E2E
(§15).

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
  projections stamped with frontier + reducer version, rebuilt on parity
  failure (and encrypted/rebuildable per key domain, §6.2). Nothing
  re-folds a forever log per list render.

## 14. Threat model

| Threat | Posture |
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
| Stolen daemon | Capability revocation + epoch rotation; historical plaintext exposure documented |
| Storage spam / DoS | Ingestion/dispatch quotas (rate limits are not fold policy — folds stay deterministic); Connect per-plane quotas |
| Coordination-dir forgery / hostile files | Proposals/data only; safe v0 protocol (no-follow, bounds, atomic writes); fixed-schema injection summaries |
| Index / audit leakage | Encrypted or rebuildable-per-key-domain indexes; ID-only audit records (§6.2) |
| Schema downgrade | Versioned envelope/ops; unknown effectful operations fail closed |
| Compaction resurrection | Signed checkpoints + GC fences + coverage/abandonment rule; behind-fence re-proposal |
| Selective erasure | Per-item DEKs from v1; erase = key destruction; projections/backups inside the key boundary (§4.7) |
| Harvest-now / decrypt-later | Documented v1 deferral + KEM algorithm-ID agility; native-first hybrid path (§4.9) |
| Metadata leakage | Public segment manifest frozen + leakage documented (§4.0); minimized where cheap |

## 15. Delivery plan

### 15.0 Immediate, out of band

- **Live-audio always-consent fix — LANDED (PR #236, merged 2026-07-11)**:
  one centralized gate (`live_audio::request_spawn_consent`) enforced on
  the native and MCP/ctl dispatch paths before any audio side effect, with
  per-path tests; resolution races the approval-registry responder against
  the bus's ControlCommand verbs so it works across daemon shapes;
  ApproveAll approves the single prompt only.
- Housekeeping (done 2026-07-11): the stale early draft with a
  newer-looking filename is archived; this file is the only canonical
  RFC.

### 15.1 Ordering rule and gates

**D0-A freezes before durable P1 Memory data. D0-B freezes before P2
sync. The Agenda-effects gate freezes before P5/P6.** UI and service
shapes may be prototyped at any point behind a "pre-protocol, local-only"
flag — but durable user data is written only in the D0-A format, so P1
data never becomes a second legacy import problem.

- **D0-A — authority and tenant semantics** (gate for P1 durable data):
  genesis descriptor + succession/recovery + `governance_scheme` (§3.1);
  mandatory v1 algorithm suite; DeviceCertificate/CapabilityGrant split
  (§3.2); canonical signed local operation bytes + golden vectors (§4.1);
  typed fail-closed plane IAM (§6.2's intersection); Memory
  judgments/pins reducer (§6.1); flow-admission rules (§6.3); Agenda IAM
  + effect lifecycle + fireability + freshness-predicate definitions
  (§7.3, §7.5, §7.9); deterministic expiry rule (§4.5).
- **D0-B — distributed protocol** (gate for P2): public segment manifest
  + leakage documentation + AEAD/nonce/AAD/padding/addressing spec
  (§4.0); authorization-proof distribution (§4.8); frontier/witness
  receipts; writer-fork recovery mechanics (§4.2); offline validity;
  checkpoint coverage/abandonment (§4.6); plane deletion (§12);
  durability-state acknowledgements (§4.4); erase mechanics finalization
  (§4.7) + PQ record (§4.9); the Connect plane-store contract (§12).
- **Agenda-effects gate** (gate for P5/P6): everything in §7.3/§7.5/§7.9
  demonstrated under test — approval revocation, cancellation,
  Complete/Retire/Reopen effects, concurrent heads, scheduler transfer +
  fencing, freshness, unknown-outcome handling, notification consent
  intersection.

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
  attribution unforgeable; zone/space denial tested; conflicts
  represented, not overwritten; fresh sessions receive no unrequested
  memory.
- **P2a — storage and first sync**: Connect plane-store subsystem (§12);
  browser IndexedDB outbox; one browser + one daemon; backup/migration;
  **zero-daemon exit criteria**: plane creation + recovery in a browser
  alone, second-browser restore, hosted-provenance E2E.
- **P2b — multiwriter**: second browser/daemon; offline convergence;
  multi-tab and restored-snapshot recovery (§4.2); revocation/rotation/
  checkpoints under churn.
- **P2c — mediation and flows**: disposable peers; typed peer/session
  IAM; cross-zone and cross-plane export/import (§6.3).
- **P3 — Agenda note/task tenant**: Add/Patch/Complete/Reopen/Retire;
  Agenda + Journal projections; no effects; provenance labels visible.
- **P4 — cross-links + promotion**: observation→memory→agenda→result→
  reflection flows; explicit exports; diary projection; bounded agent
  tools.
- **P5 — one-shot reminders** (after the Agenda-effects gate): single
  explicit executor; UTC occurrence + original timezone; rail +
  content-free push; quiet hours + caps; duplicate/missed policy. No
  voice/phone/recurrence/failover.
- **P6 — one-shot scheduled sessions**: immutable revisions; explicit
  approval; scheduled-session principal; executor-local profile;
  preflight (cost/tokens/wall-time/credentials/network); occurrence
  ledger; result attestation + reflective proposal. Standing grants only
  after per-constraint enforcement is demonstrated.
- **External dependency**: the consumer-default genesis lane requires
  platform-signed clients — Apple notarization and Windows Authenticode
  move onto the consumer default's critical path (owner decision, §3.5).
  GPG + release transparency covers the daemon/CLI/self-hosted lane
  meanwhile.
- **Later**: durable questions/replies; user-created zones; sealed
  items; embeddings; foreign proposal inbox; peer gossip + witnessed
  frontiers; automatic scheduler election; recurrence +
  recurrence-instance identity; event/location triggers; blob/attachment
  storage (references reserved, §7.10); voice/phone escalation steps
  (per-item consent); org planes; family/multi-owner governance;
  person/contact entities.

## 16. Testing requirements

- **Protocol/fold**: op permutations + duplicate delivery; missing causal
  deps; `(writer,sequence)` and `request_id` fork cases; multi-tab writer
  sequencing; restored-snapshot self-forks; writer retirement +
  re-enrollment after a fork; future-HLC poisoning; concurrent
  patch/complete/reopen/retire; unknown op versions; checkpoint +
  old-replica resurrection; **incomplete-view checkpoint refusal +
  explicit writer abandonment**; cold-start rollback + split-view
  simulation; canonical malformed encodings; signature/AEAD malleability;
  parser/fold fuzzing with hard byte, reference, depth, and time bounds.
- **Authorization/crypto**: Rust/WebCrypto golden vectors as **required
  keyless CI**; KEM + zone epoch rotation; per-item DEK erase (index,
  view, checkpoint, and backup copies all unreadable after erase);
  expired/revoked/wrong-zone/space/kind capabilities; **capability late
  arrival vs expiry deadline; per-writer budget exhaustion; signed
  strict/lenient revocation policy**; actor/signer mismatch;
  hosted-provenance ceiling (incl. attempted self-elevation); offline
  writer across a revocation cutoff; plane-capability-allowed but
  executor-IAM-denied; **root succession, competing successors,
  compromise recovery, algorithm migration**.
- **Memory**: candidate vs accepted retrieval; concurrent
  Accept/Dispute; contradictory claims; supersession + expiry; injection
  payloads retained as quoted data; procedures/preferences denied on the
  ordinary retrieval route; bounded retrieval/token budgets; **identical
  allow/deny requests through native, ctl/MCP, dashboard, and peer
  surfaces**; export allow/deny incl. read-A+write-B composition
  refusal; mediated search without zone key; ID-only read audit.
- **Agenda/scheduling**: approve-A-then-edit-to-B; concurrent approved
  heads fail closed; **approval revocation; Complete/Retire canceling
  occurrences; Reopen not refiring a past one-shot**; stale
  cancel/reschedule; freshness-predicate failure; **manual scheduler
  transfer + fenced old-scheduler return; unknown session outcome**;
  clock jumps + sleep/wake; crash before launch / after launch / before
  result; duplicate occurrence delivery; missing credentials; unavailable
  project binding; headless approval; scheduled-session sandbox/IAM
  enforcement; **notification endpoint consent, quiet hours, cost caps,
  provenance ceiling**.
- **Storage/ops**: segment corruption/truncation; partial upload + retry;
  quota exhaustion; **root-signed deletion + grace recovery;
  malicious-store deletion; backup restore; browser eviction + loss of
  the last local checkpoint**; plane deletion + GC; self-hosted
  migration; **zero-daemon Connect-app E2E**; macOS/Linux/Windows +
  sleep/clock behavior.
- **Coordination**: adversarial files — symlinks, path traversal, torn
  writes, oversized frontmatter, forged writers, TTL/GC races, prompt
  payloads retained as data.
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
- a **CI absence test** over prompts, schemas, runtime fields, Presence,
  configuration, control messages, browser fragments, and agent cards.

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

## Appendix C — remaining open questions

1. **Authorization-proof distribution shape** (D0-B): per-zone control
   feeds vs hash-fetched capability objects vs Merkle
   inclusion/non-revocation proofs (§4.8) — including what zone members
   see, what Connect sees, and the offline floor.
2. **Offline expiry rule confirmation** (D0-A): working position is
   acceptance/witness-deadline expiry + online-only leases for
   high-impact capabilities + epoch/sequence budgets for offline writers
   (§4.5); confirm against real offline usage before freeze.
3. **Recovery custodian/threshold specifics and drill UX** (D0-A):
   who/what holds recovery envelopes beyond the owner (second device,
   printed phrase, trusted contact, none), threshold rules, and the
   drill cadence — phrase-entry-on-trusted-lane-only is already decided
   (§3.5).
4. **Native-side hybrid ML-KEM timing** (post-v1): when to turn on hybrid
   wraps for daemon recipients (§4.9); agility is already in the format.
5. **Coordination v0 constants** (P0.5): byte/file-count/scan/TTL bounds
   and per-box sizing for the coordination dir (§9).
6. **Org/family plane RFC scheduling** (post-personal-plane
   stabilization; principles pinned in §11).
