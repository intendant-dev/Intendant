# RFC D0 (v2): The Principal Plane, Memory, and Agenda

*2026-07-11. Supersedes `agenda-owner-plane-design.md` (v1) and incorporates
`agenda-owner-plane-design-review.md` nearly wholesale, plus four subsequent
design rounds (external agents, teaching architecture, coordination bus,
file-substrate revision). Self-contained: reviewable without the prior
documents. Status: for re-review; nothing frozen. Once accepted this becomes
`docs/src/owner-plane.md` + amendments to `trust-architecture.md`,
`trust-tiers.md`, `credential-custody.md`, `self-hosted-rendezvous.md`,
`autonomy.md`, `integrations.md`.*

---

## 0. Reading guide

- §1–2 product vision and conceptual model.
- §3–5 authority, wire protocol, zones — the D0 core that must settle
  before any sync ships (it gates P2, **not** P1; see §16).
- §6–7 the two tenants: Memory (first) and Agenda (second).
- §8–10 the agent-facing surfaces: external agents, teaching, and the
  daemon-plane coordination bus (deliberately *not* part of the plane).
- §11–15 org seam, Connect storage, shared crate, threat + test matrices.
- §16 delivery plan. §17 resolved-questions ledger. §18 open questions for
  this review round.

## 1. Vision

Intendant grows a **principal plane**: principal-scoped (person or org),
end-to-end-encrypted, log-structured state that travels with its owner, not
with any box. Its first tenants are **Memory** (what the system believes,
observes, decides, learns) and **Agenda** (what the system intends or
commits to do). Product framing:

- The seed of Intendant-as-everything-app: an AI diary/agenda for technical
  and regular life, valuable with **zero daemons** (browser + account),
  with daemons adding reminders, scheduled sessions, and voice/phone later.
- The standing anti-fragmentation rule: **features never grow ad-hoc
  sync.** Anything principal-scoped joins the plane and inherits its
  custody, sync, signing, and zone machinery.
- Tenets carried from the existing architecture: Connect stays blind and
  zero-authority; disposable boxes hold no durable secrets *or intentions*;
  scheduled work executes only through supervised sessions; unsigned data
  is not lesser data — it is not in the plane at all; derive, don't mirror.

## 2. Conceptual model

### 2.1 Four axes (never conflated)

- **Plane — ownership/governance.** A person or organization. Its root key
  governs devices, capabilities, zones, administrative history.
- **Zone — cryptography/replication.** Determines which devices can
  decrypt and replicate. Zones are key domains, not labels.
- **Space — semantic context.** Project, workflow, personal domain, team —
  a namespace inside a plane. Audience ("for agents" / "for the user") is
  space/metadata semantics, **not** a zone (v1 lesson: the `agents`
  default zone was a conflation and is dropped).
- **Network — transport/discovery.** Fleet/peer reachability. Grants no
  data authority.

A project is a **space** inside a plane, identified by a stable
`space_id`, not by a filesystem path. Proposal: the `space_id` lives in
the project's committed `intendant.toml` (travels with the repo, shared by
worktrees); each daemon keeps a local `space_id → checkout path(s)`
binding. (Resolves review §14.6.)

### 2.2 Three storage planes

| Plane | Scope | Sync |
|---|---|---|
| daemon-plane | one box (`~/.intendant`, caches, coordination dir §10) | none — correct |
| project-plane | one repo (`intendant.toml`, project space binding) | travels with repo |
| **principal plane** | one person/org (vault, claim, Memory, Agenda) | Connect or self-hosted, E2E |

### 2.3 Promotion, not sync

Agent working memory (session context, scratch notes, external agents'
native stores) stays daemon-/project-plane. It enters the principal plane
only by **deliberate promotion** — a signed proposal op carrying
provenance. No bulk mirroring, ever.

## 3. Authority

### 3.1 The two-resource invariant (revised foundational invariant)

> Authority is minted only by the local authority of the governed
> resource: **plane-root capabilities govern plane data; daemon-local IAM
> governs daemon effects.** Connect mints neither.

This resolves v1's zero-daemon contradiction: with no daemon there is no
daemon IAM, and the browser self-signing proves authorship, not right —
the plane root is what makes browser-only mode sound.

### 3.2 Personal plane root

- `plane_id = hash(plane_root_public_key)` — never a Connect account UUID.
- Root private key sealed under vault-style, domain-separated passkey
  custody **plus mandatory recovery envelopes** (key loss must not orphan
  a life's diary).
- Root operations (device enrollment, capability grants, zone
  administration, checkpoints, scheduler-epoch changes) are administrative
  **ceremonies**, not per-entry writes.
- Structurally: the personal analog of the existing org root — root keys
  signing grant documents is already the house pattern.

### 3.3 Device certificates

Root-signed certificates bind, per device: signing algorithm + public key;
**separate KEM/key-agreement algorithm + public key** (Ed25519 signing
keys are never converted to X25519 — distinct keys, distinct purposes);
stable device key ID; plane ID; allowed zones/spaces/tenants/operations;
capability ID; issuance/expiry; revocation (capability) epoch; and a
**provenance class**: `hosted-browser | owner-served-browser | native-app
| daemon`. Current key inventory is not sufficient as-is (browser: P-256
ECDSA, no KEM; daemon: Ed25519, no KEM) — enrollment issues the missing
identities; nothing is retrofitted onto existing keys.

### 3.4 Capabilities

Root-signed, replicated, constraint-carrying: plane, zones, spaces,
tenant operations, kind ceilings, sensitivity ceilings, candidate-vs-
accepted rights, expiry, byte/op quotas, allowed flows. Revocation
semantics in §4.4.

### 3.5 Two-authority execution rule

An Agenda approval never mints authority on an executor. Launching a
scheduled session requires **both**: (1) a valid plane capability and
approval over an immutable execution revision (§7.3), and (2) a
daemon-local grant accepting that plane, approver class, execution
profile, project scope, and resource ceilings. A compromised plane writer
can create proposals; it cannot reach `task.run`, filesystem, credential,
or network authority on any daemon.

### 3.6 Signer, actor, capability, evidence, provenance

Every envelope distinguishes: **signer** (device key), **actor** (session,
human, process, peer on whose behalf), **capability** (plane authorization
id), **local authorization evidence** (daemon-side session/grant), and
**content provenance** (source session/space/item/proposal). Supervised
agent sessions hold no durable signing keys: the controller daemon signs
while attesting the bound session principal. Free-form `source` strings
are never authorization.

**Session principal-binding token** (new; required by the ctl surface,
§8): the controller injects a session-scoped bearer token into every
supervised session's environment; `intendant ctl` presents it
automatically on the local control socket; the daemon resolves it against
the live session registry and binds the actor. Properties: local socket
only, session lifetime, ring-capped rights (typically propose-only).
Without it, shell-invoked ctl writes would attribute to the OS user —
ring laundering. Applies to native and external sessions alike.

## 4. Wire protocol and sync

### 4.1 Envelope (minimum signed header)

```
protocol_version, tenant, plane_id, zone_id, space_id,
crypto_epoch, capability_epoch,
signer_algorithm, signer_key_id, actor_principal, capability_id,
writer_sequence, previous_writer_hash, causal_references,
created_hlc, operation_type, operation_version, body_hash
```

Signature over domain-separated canonical bytes excluding itself. Exact
signed bytes are stored; verification never happens after
deserialize/reserialize. Rust/WebCrypto golden vectors land **before**
freeze.

### 4.2 Per-writer chains; no timestamp authority

Each writer produces an authenticated chain (`writer_sequence` +
`previous_writer_hash`). HLC is approximate chronology only — it never
decides grant validity, revocation ordering, approval validity, winning
executable revisions, or offline-vs-revocation ordering (future-clock
poisoning). Conflicts are represented causally (revision references) and
surfaced, not silently resolved. Duplicate `(writer, sequence)` with
different hashes = signer equivocation → quarantine the writer pending
ceremony.

### 4.3 Frontiers and residual guarantees

A zone frontier is a **per-writer vector / Merkle frontier**, never one
scalar head. Signed ops prove integrity, not completeness: Connect can
still withhold a feed, serve stale-but-valid state, or split views. The
protocol states its residual guarantee precisely: withholding is
detectable only after this device or a witness observed a newer
commitment; cold-start rollback requires a checkpoint, witness, or
transparency proof; split views surface through cross-device comparison.
**Witness set v1 (concrete): the plane's own enrolled devices, plus
optional anchoring of frontier commitments in the existing transparency
log.** No new witness network.

### 4.4 Revocation: two epochs and a cutoff

- **Encryption epoch** — controls future decryption (rotate zone KEK,
  re-wrap for remaining members). Historical plaintext already received is
  not revocable; stated plainly.
- **Writer-capability epoch** — controls accepted operations, with a
  deterministic cutoff `accepted_through = {writer, sequence, op_hash}`.
  Old-capability ops beyond the cutoff are quarantined for re-proposal.
- The offline trade-off is a policy choice made honestly in UX: strict
  cutoff may discard honest unsynced work; lenient acceptance creates a
  stale-authority window.

### 4.5 Durability states (replica = outbox, not cache)

Before acknowledgement elsewhere, the local replica is the **only**
durable copy. User-visible states: `local-only → uploaded → replicated →
witnessed`. The "a wiped box loses nothing" promise begins at
acknowledgement, and the UI must say so.

### 4.6 Checkpoints, GC, retire vs erase

Compaction is an admin-authorized **checkpoint**: canonical folded state,
reducer/schema version, `covers_frontier`, GC fence. Replicas behind the
fence rebootstrap; unsynced work rebases or quarantines — never blind
reintroduction. **Retire** = semantic removal, history preserved.
**Erase** = destroy content-key availability, retain minimal audit
hash/tombstone; cannot force forgetting by parties that already decrypted.

### 4.7 Encryption hierarchy

Random per-segment DEK → wrapped by zone-epoch KEK → KEK wrapped to
enrolled recipient KEM keys. **Item-level content-key indirection is
reserved in the v1 schema** (sealed items and selective erasure are real
requirements; retrofitting would force migration). Direct segment-DEK
wraps only for deliberately limited slices / disposable-task capsules.

### 4.8 Metadata honesty

Connect sees: zone IDs, segment sizes and timing, recipient
relationships, traffic patterns. Documented, minimized where cheap, not
pretended away.

## 5. Zones and spaces

- **Default zones: exactly one** private/personal zone per personal
  plane. Project/shared zones are created by explicit enrollment. (v1's
  `agents` default is dropped — audience is space semantics.)
- Zone membership = KEK wrapped per device KEM key, authorized by
  root-signed grants; replication follows key possession — a daemon
  physically cannot read zones it isn't enrolled in.
- Per-space replication policy: an integrated daemon may hold full zone
  replicas while specific owner-private **spaces** are marked
  mediated-only (resolves review §14.5); disposable workers never receive
  zone KEKs — they use mediated access or expiring task capsules (§8).
- A daemon that holds a personal/org zone key is **integrated-tier for
  that data**, whatever its prior label.

## 6. Memory (first tenant — non-effectful, proves the plane)

> Memory is what the system believes, observes, decides, or learns.
> Agenda is what it intends or commits to do.

### 6.1 Kinds (v1) — resolves review §14.1

`observation | decision | episode | procedure | preference`. Risk
ordering is explicit: procedures and preferences are instruction-adjacent
and carry the strictest write rules.

### 6.2 Claims, not mutable facts

```
MemoryClaim {
  id, plane_id, zone_id, space_id,
  kind, statement, status,           // candidate|accepted|disputed|superseded|retired
  observed_at, valid_from/until, expires_at,
  provenance { signer, actor, daemon, session, project, model, evidence_refs },
  supersedes[], labels
}
```

Corrections append (`Supersede`/`Dispute`/`Retract`); contradictory
claims may coexist and retrieval shows the conflict; validity/expiry are
first-class; recorded model confidence is the writer's assessment, never
authorization; `accepted` means accepted-under-policy, not true.

### 6.3 IAM vocabulary and write rules — resolves §14.2, §14.3

Ops: `memory.search/read/propose/assert/curate/export/admin`, all
capability-constrained (plane/zone/space/kind/sensitivity/status/expiry/
quotas/flows).

- Ordinary supervised agents: `propose` everywhere they can write;
  `assert` only for `observation`/`decision` in designated **workflow
  spaces** with short default retention.
- `procedure` and `preference`: **proposal-only for all agents, always**;
  acceptance is an owner `curate`. Owner preferences are never
  agent-writable without per-item approval (§14.3: no).
- `curate` (confirm/dispute/supersede/retract/pin) is owner/browser-class
  by default; `export` authorizes one specific cross-zone flow.

### 6.4 Cross-zone flow = export capability

Read-in-A plus write-in-B is an export whether or not the API names it.
Explicit flow grants (`from/to plane-zone-space, allowed_kinds,
classification_ceiling, expiry`); the destination gets a **new** claim
with its own zone identity plus an immutable provenance reference — an
object never merely changes encryption domains. Enforceable for mediated
tools; sessions with broad shell/network authority need matching egress
limits, and v1 claims only the mediated-tool guarantee (§14.10: the
narrow, honest answer; whole-session egress belongs to the org/strict
RFC).

### 6.5 Retrieval safety — resolves §14.8, §14.9

No whole-store injection, ever (the tombed system's worst habit). Agents
call bounded `memory.search` with least-privilege default spaces; results
carry zone, status, actor, evidence, age, validity, conflicts; candidates
excluded or marked; **retrieved content is quoted data, not
instruction**; every retrieval has count and token budgets; sensitive
reads are auditable by result ID. Pinning (context-injection eligibility)
is owner-curated only; agents may *suggest* pins (§14.8). Owner-visible
per-principal query/read audit, default 90-day retention (§14.9).
Retrieval index v1 is local lexical; embeddings are an explicit
egress/credential decision later, local models preferred (§14.7).

### 6.6 Retention — resolves §14.4

Workflow-space memory: short default expiry (proposal: 30 days,
auto-expire unless promoted). Durable project/personal memory: forever by
default (it is a diary), per-zone retention overridable; org spaces
require explicit retention/legal