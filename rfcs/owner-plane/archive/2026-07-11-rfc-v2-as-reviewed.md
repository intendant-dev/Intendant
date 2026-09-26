# RFC: The Owner Plane, Memory, and Agenda (v2 / D0)

*2026-07-11. Supersedes `~/agenda-owner-plane-design.md` (v1) by folding in
`~/agenda-owner-plane-design-review.md` (adopted nearly wholesale) and the
subsequent design rounds: org seam, external agents, teaching architecture,
coordination bus. Self-contained — reviewers need no other document. Status:
D0 foundation RFC for review; nothing is frozen. Companion chapters once
accepted: `docs/src/owner-plane.md` (new), touching `trust-architecture.md`,
`trust-tiers.md`, `credential-custody.md`, `self-hosted-rendezvous.md`,
`autonomy.md`, `mcp-server.md`.*

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
   the box (§9). Agent working memory lives with the agent (§6.0, §11).
2. **Data vs instruction decides the channel.** Plane content is always
   quoted, provenance-labeled data. Instructions reach models only through
   curated channels (skills, prompts) or approved effects. Nothing in the
   plane self-executes.

And one invariant, revised from the original trust doctrine:

> **Authority is minted only by the local authority of the governed
> resource: plane-root capabilities govern plane data; daemon-local IAM
> governs daemon effects. Connect mints neither.**

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
  plane; its root key governs devices, capabilities, zones, and admin
  history. ("Owner Plane" is the product name; *principal plane* is the
  internal term, since org planes are not personal accounts.)
- **Zone — cryptography and replication.** A zone determines which
  devices can decrypt and replicate. Zones are key domains, not labels.
- **Space — semantic context.** A project, workflow, personal domain, or
  team namespace *inside* a plane. Audience is semantics, not crypto.
- **Network — transport and discovery.** Fleet/peer/Connect reachability.
  Grants no data authority by itself.

A project is ordinarily a **space** in a personal or org plane, not an
authority root inferred from a filesystem path. Each daemon keeps a local
binding from stable space ID → checkout path(s); the binding survives
worktrees and machines (review Q6).

### 2.2 Storage classes

| Class | Scope | Sync | Examples |
|---|---|---|---|
| daemon-plane | one box | none | sessions, logs, caches |
| project-plane | one repo | with the repo | `intendant.toml`, instruction files |
| **owner/principal plane** | one principal | Connect (or self-hosted), E2E | Memory, Agenda; later: preferences, diary |
| **coordination dir** (§9) | one box, ephemeral | none (daemon may relay) | session heartbeats, dirty sets, inter-session notes |

## 3. Authority

### 3.1 The zero-daemon problem and the plane root

Browser-only value cannot rest on daemon-local IAM (no daemon → no minting
authority), and making the Connect account the root would hand Connect
authority. Therefore:

- **Personal plane root**: `plane_id = hash(plane_root_public_key)` — not
  a Connect account UUID. The root private key is sealed under
  vault-style, domain-separated passkey custody with **mandatory recovery
  envelopes**. Root operations (device enrollment, capability grants, zone
  admin, scheduler-epoch changes) are explicit ceremonies, not per-entry
  writes.
- **Device certificates**, root-signed, bind: signing algorithm + public
  key; **separate** key-agreement/KEM algorithm + public key (Ed25519
  signing keys are never converted to X25519 — distinct keys, distinct
  purposes); stable device key ID; plane ID; allowed zones, spaces,
  tenants, operations; capability/grant ID; issuance/expiry;
  revocation/capability epoch; and a **device provenance class**: hosted
  browser, owner-served browser, native app, daemon.
- **Capabilities**, root-signed, grant scoped rights (zones × spaces ×
  tenant operations × expiry × policy constraints). Connect associates an
  authenticated account with opaque plane material for routing and quota
  only; it appears nowhere in any signature chain.

Existing key inventory (why this section exists): today's browser identity
is a non-extractable P-256 ECDSA sign-only key in IndexedDB; the daemon
identity is an Ed25519 signing key. Neither can receive a wrapped zone key;
both need companion KEM identities under a device certificate. The v1 doc's
"wrap to existing identity keys" was not implementable as written.

### 3.2 Signer, actor, capability, evidence, provenance

Every operation distinguishes:

- **signer** — the device key that produced the signature;
- **actor** — the browser, agent session, human, or peer on whose behalf
  it acted (supervised agent sessions hold **no durable keys**; their
  controller daemon signs while attesting the bound session principal);
- **capability** — the plane authorization under which the op is valid;
- **local authorization evidence** — the daemon-local session/grant that
  admitted the actor to the plane service;
- **content provenance** — source session, project/space, evidence refs.

Free-form `source` strings are never authorization or provenance.

### 3.3 Two-authority execution rule

A plane-side approval never mints authority on an executor. Launching a
scheduled session requires **both**: (1) a valid plane capability and
approval over an immutable Agenda execution revision, and (2) a
daemon-local grant accepting that plane, approver class, execution
profile, project scope, and resource ceilings. A compromised plane writer
can create proposals; it cannot acquire `task.run`, filesystem,
credential, or network authority on any daemon.

### 3.4 Hosted-browser mode is a degraded-provenance lane

E2E makes Connect's *storage* blind, not Connect-served JavaScript
trustworthy. Hosted-origin devices get low-effect read/write capabilities.
Plane administration, cross-zone export, high-urgency delivery, and
execution approval require a trusted-origin/native device or independent
executor-local approval. A passkey gesture proves presence to code, not
that the code presented the transaction the user believes they approved.
Browser-only mode remains a real product promise — explicitly labeled as
this lane.

## 4. Wire protocol

### 4.1 Envelope (minimum signed header)

```
protocol_version, tenant, plane_id, zone_id, space_id,
crypto_epoch, capability_epoch,
signer_algorithm, signer_key_id, actor_principal, capability_id,
writer_sequence, previous_writer_hash, causal_references,
created_hlc, operation_type, operation_version, body_hash
```

The signature covers domain-separated canonical bytes excluding itself.
Replicas store the exact signed bytes (no verify-after-reserialize).
Canonical Rust/WebCrypto **golden vectors land before the protocol
freezes**.

### 4.2 Ordering, conflicts, equivocation

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
- Duplicate `(writer, sequence)` with different hashes, or duplicate
  request IDs with different bytes, is **signer equivocation** — flagged,
  quarantined, surfaced to the owner.

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
the existing Connect transparency log. No new witness network.

### 4.4 Durability states

A local replica is a **durable replica + outbox**, not a cache: before
acknowledgement elsewhere, it is the only copy of an offline write. The
UI exposes: `local-only → uploaded → replicated → witnessed`. "A wiped box
loses nothing" begins only at `replicated`.

### 4.5 Revocation

Two separate mechanisms: an **encryption epoch** (future reads) and a
**writer-capability epoch** (accepted operations), plus a per-writer
cutoff `accepted_through = {writer, sequence, operation_hash}`. Old-epoch
operations beyond the cutoff land in quarantine for re-proposal. The
unavoidable trade-off is documented honestly: an honest offline write
made before revocation is indistinguishable from a backdated malicious
one — strict cutoffs may discard honest unsynced work; lenient ones open
a stale-authority window. Policy and UX must choose per zone. Historical
plaintext already decrypted by an evicted member is not recoverable — the
standard E2E-group limitation, stated in docs.

### 4.6 Compaction, retention, erasure

Naive set-union breaks under GC (old replicas resurrect discarded ops).
Compaction requires an admin-authorized **checkpoint**: canonical folded
state + reducer/schema version + `covers_frontier` + a GC fence; replicas
behind the fence rebootstrap and rebase or quarantine unsynced work.
Distinguish **Retire** (semantic removal, history preserved) from
**Erase** (destroy content-key availability; only a minimal audit
hash/tombstone remains). Retention is **per-zone**: personal zones default
to forever (it is a diary); org spaces require explicit retention/legal
policy (review Q-retention).

### 4.7 Encryption hierarchy

Random per-segment DEK → wrapped by zone-epoch KEK → KEK wrapped to
enrolled recipient KEM keys. Item-level content-key indirection is
**reserved in the v1 schema** (sealed items and selective erasure are real
requirements; retrofitting would force a migration). Metadata visible to
Connect (zone IDs, sizes, timing, recipient relationships, traffic
patterns) is documented as such.

## 5. Zones and spaces

- **Default zones: exactly one** — `private`. (`agents` was dropped:
  audience is a space/label concern, not a key domain.) Additional zones
  are created by explicit enrollment/product flows, not defaults.
- Zone membership = KEK wrapped to device certs, authorized by root-signed
  grants; replication follows key possession. Revocation = epoch rotation
  (§4.5).
- Spaces organize meaning inside zones (projects, workflows, "household",
  "health"). Capabilities scope to zones × spaces × kinds.

## 6. Memory (first tenant)

### 6.0 What memory is not

Not everything called "memory" syncs:

- **Session context** (transcripts, tool output, scratch reasoning):
  session-local, disposable.
- **Workflow state** (current plan, blockers, handoffs): goal-scoped,
  short-lived, shareable with explicitly collaborating agents; short
  retention by default (review Q4).
- **Durable knowledge** (observations, decisions, lessons, procedures):
  plane-backed, zone-aware — this tenant.
- **Owner/private memory** (personal facts, preferences, episodes): this
  tenant, not agent-readable by default.
- **Org memory**: org plane (§12).

"Agent memory" means agent-authored/consumed but person- or org-governed.
It is never an agent-owned authority domain. The **old Memory/Knowledge
system is tombed** — its channels, cursors, KV model, inheritance flag,
and file format constrain nothing here (cutover inventory: Appendix A).

### 6.1 Claims, not mutable facts

```
MemoryClaim {
  id, plane_id, zone_id, space_id,
  kind, statement, status,
  observed_at, valid_from, valid_until, expires_at,
  provenance { signer, actor, daemon, session, project, model,
               evidence_refs },
  supersedes[], labels
}
status: candidate -> accepted -> disputed | superseded | retired
```

- Agents create **attributed claims**, not truth; corrections append
  `Supersede`/`Dispute`/`Retract`; contradictions may coexist and
  retrieval surfaces them with provenance.
- Validity/expiry are first-class. Model confidence is the writer's
  self-assessment, never authorization. "Accepted" means accepted under a
  policy.
- **Kinds v1** (review Q1): `observation`, `decision`, `episode`,
  `procedure`, `preference`. Procedures and preferences are higher-risk.

### 6.2 Memory IAM

Operations: `memory.search`, `memory.read`, `memory.propose`,
`memory.assert`, `memory.curate`, `memory.export`, `memory.admin` —
capability-scoped by plane, zone, space, kind, sensitivity ceiling,
candidate-vs-accepted, expiry, byte/op quotas, and allowed flow
endpoints.

- **Q2 (what agents assert directly)**: ordinary supervised agents may
  `assert` only `observation`/`episode` claims in designated **workflow
  spaces** with short retention; everything else is `propose`-only.
  `decision` is assertable only by the session that owns the decision's
  approval trail.
- **Q3 (preferences)**: owner preferences are **never** agent-assertable;
  propose + explicit owner curation, always.
- **Q8 (pinning)**: only owner-curated (`memory.curate`) claims are
  pinnable for automatic context injection; agents cannot pin.
- **Q9 (audit)**: sensitive-space reads/searches are audited by result
  IDs, owner-visible in the plane's admin history.

### 6.3 Cross-zone flow is an export capability

Read-in-A plus write-in-B *is* export. Cross-zone/plane copying requires
an explicit flow grant:

```
memory.export { from(plane,zone,space) -> to(plane,zone,space),
                allowed_kinds, classification_ceiling, expiry }
```

The destination receives a **new** claim under its own zone identity with
an immutable provenance reference to the source; objects never silently
change encryption domains. Enforcement honesty (Q10): this binds the
mediated Memory tools; a session with unrestricted shell/network can
paraphrase — strict deployments pair it with session egress/tool limits.

### 6.4 Two access shapes

1. **Replicating integrated daemon**: holds zone key + writer capability,
   keeps a durable replica, works offline. (Q5: integrated daemons get
   full replicas of enrolled zones; owner-private spaces may be marked
   mediated-only, in which case even integrated daemons query rather than
   replicate them.)
2. **Mediated access** (disposable workers, peers, low-trust sessions): a
   Memory service on a trusted daemon authenticates the caller principal,
   evaluates local IAM + plane capability, executes the scoped
   search/read/propose, and signs accepted writes while attesting the
   actor. The remote box never receives the zone key or replica. A daemon
   holding a durable zone key **is integrated-tier for that data**
   regardless of its label; network/fleet membership is never itself
   memory access.

Plane keys and plaintext live in the controller-side service only —
`intendant-runtime` never touches them (the runtime/controller boundary
holds).

### 6.5 Retrieval safety

- No whole-store injection, ever. Agents call bounded `memory.search`
  with count and token budgets; session/tool grants define
  least-privilege default spaces.
- Results carry zone, status, actor, evidence, age, validity, and
  conflict info; candidates are excluded or clearly marked; content is
  wrapped as **quoted data, not instruction**.
- Context-injection shape: a small per-session **memory index**
  (one-liners with IDs — the same index-plus-lazy-bodies delivery shape
  as skills, deliberately *not* the skills trust class), derived from
  accepted + pinned claims for the session's spaces; bodies fetched on
  demand.
- **Q7 (embeddings)**: v1 is a local lexical/FTS index. Embeddings later,
  local-only first; sending private memory to an embedding provider is an
  explicit credential/egress decision. History search, session-log
  search, and Memory retrieval remain separate operations.

### 6.6 Memory → skills (graduation, not merger)

Skills load as instructions; claims load as data. The bridge is a
lifecycle: episodic observation → claim → accepted → owner-curated
procedure → **explicit export to a skill file**. Skills are compiled
memory — the instruction-grade tier, reachable only through curation.
(Consequently: projecting memory into instruction files —
CLAUDE.md/AGENTS.md managed blocks — is restricted to exactly this
owner-curated export; nothing automatic, not v1.)

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
`RecordOccurrenceStarted`, `RecordOccurrenceResult`. Tags, if an OR-set,
carry real observed-remove semantics.

### 7.3 Executable state is atomic

Fieldwise merge must never compose an executable state no author
proposed. Every execution revision is an **immutable manifest**; approval
signs its digest. Manifest contents: goal; occurrence/due instant with
timezone interpretation; executor daemon; local execution profile;
project/space binding; backend/model; sandbox; filesystem roots; network
policy; credential requirements and failure behavior; autonomy ceiling;
token/cost/wall-time/concurrency/subagent limits; retry/misfire policy;
approval expiry; standing-grant ID + evaluator version. **Any material
edit or reschedule creates a new proposed revision and invalidates the
old approval.**

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

### 7.5 Firing: coordination is real

`ClaimFiring` ops in an eventually-consistent log cannot provide mutual
exclusion (two partitioned daemons both fire; union reveals it later).
v1:

- **one explicitly selected `scheduler_daemon_id`** per plane (or per
  item override). Not "the anchor" — the anchor serving role is
  deliberately fungible; overloading it couples two lifecycles.
- `occurrence_id = hash(item_id, approved_revision_hash, due_instance)`;
- a **daemon-plane occurrence journal**, fsync'd before spawn:
  `Prepared → Started(session_id) → Completed(result_hash)`; session
  creation idempotent by occurrence ID; crash recovery reattaches before
  retrying;
- **at-least-once with deduplication**, never exactly-once, stated
  honestly;
- T3 execution requires a sufficiently **fresh control/revocation
  frontier**; stale offline state fails closed absent an explicit
  bounded offline grant;
- missed-while-down: fire-on-wake within a staleness window, then degrade
  to a "missed" digest entry.
- Failover (fenced leases / signed scheduler epochs / accepted duplicate
  execution) is explicitly deferred.

### 7.6 Notification policy

Author-controlled delivery is too much power. Separate: entry priority;
notification policy; **maximum allowed escalation**; quiet hours;
rate/cost caps; channel availability; per-channel user consent. Current
reality (docs-verified): attention rail + content-free Web Push exist;
voice is a future attachment point; live audio always requires human
approval today. Voice/phone are designed as future rungs behind explicit
per-item consent — never described as an existing ladder.

### 7.7 Three views over the same substrate

**Agenda** (actionables), **Journal/Diary** (curated chronology across
selected Memory episodes and completed Agenda occurrences — an operation
log is not automatically a pleasant diary), **Audit** (complete signed
history: edits, approvals, device changes, execution attestations).

### 7.8 Memory ↔ Agenda conversion

```
session observation → memory proposal → accepted memory
        → (explicit effect boundary) → agenda proposal
        → approval / reminder / scheduled session
        → occurrence result → reflective memory proposal (supervised)
```

Example: Memory "TLS cert expires Aug 1" → Agenda "renew by Jul 20".
Results and reflections never auto-become accepted truth.

## 8. External agents and foreign memory systems

- **Native memory systems of externals are out of scope by design.**
  Claude Code's local files stay Claude Code's (their locality is a
  feature — nothing to chase); Codex under the app-server has none (its
  desktop-only memory is irrelevant to us). No parsing, mirroring, or
  reconciliation; bulk import is not worth doing (anything imported would
  enter as quarantined untrusted observations anyway).
- **Channel 1 — `intendant ctl` verbs taught via skills (primary).**
  Memory/agenda verbs are ctl subcommands sharing the one authorizer
  (the same derived method table as the tunnel/MCP/dashboard). Context
  cost: one skill description until invoked (vs MCP's always-loaded
  schema rent). Requires **session principal binding**: the controller
  injects a session-scoped token into supervised sessions' env; ctl
  presents it; the daemon resolves actor = that session (ring-2,
  propose-only caps). Without the token, external writes would launder
  into OS-user authorship. Token blast radius: local socket, session
  lifetime, propose-only.
- **Channel 2 — transcript reflection (universal floor).** The controller
  owns every wrapper transcript; a supervised reflection step emits
  Memory *proposals* with signer/actor attribution. Requires zero
  cooperation from the external — works for any backend, forever.
- **Channel 3 — MCP (optional projection, not default).** Same method
  table, exposed for MCP-only ecosystems; pays the context tax only where
  chosen.
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

- **Per-writer files, never shared appends**: a `coordination/` dir with
  `sessions/<id>.md` (intent, dirty-set summary, heartbeat = mtime) and
  `messages/<writer>/<ulid>.md` (frontmatter + markdown, the ecosystem's
  lingua franca). No locks needed; maps 1:1 onto per-writer feeds if any
  of it ever syncs upward.
- **Trust ladder by channel**: file entries are quoted data and effect
  *proposals* at most (filesystem-grade attribution — anything with your
  uid can forge them); token-bound ctl is attributed ring-2; signed plane
  ops are the top. The daemon never executes effects sourced from bare
  files without the normal approval path.
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
- Cache note (motivating observation, verified in practice): mid-session
  reads of updated files append tokens without busting the prefix cache —
  the mechanism is sound; the daemon push simply beats polling.

**First feature: collision radar.** Two sessions whose dirty sets overlap
(or overlap an open PR's files) get flagged before they duplicate work.
Layering: detection = daemon, deterministic, zero-LLM (control-plane edit
streams, worktree status, `gh` PR files); delivery = injected reminders to
both sessions + rail badge; reaction = skill-taught negotiation; residue =
a workflow-state note recording the agreed split. **Degraded daemonless
mode ships first**: the skill alone says "before hot edits, scan
`coordination/sessions/*.md`; write your own" — pure convention, useful
immediately.

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
- **Derive the advertised skill index per session.** The daemon assembles
  the listing from context predicates (worktree? peers? display? plane
  enrolled?) — the catalog pattern applied to teaching. Headless sessions
  never pay for display skills; worktree sessions always see the
  collision skill.
- **Seam hooks for duties.** Ads are probabilistic; seams are
  deterministic. Must-happen behaviors are injected just-in-time at
  mechanical seams (first Edit → collision reminder; session start in a
  historied space → memory-search nudge). Ads for judgment, hooks for
  duties.

## 11. Org and family planes (separate RFC — principles pinned)

Deliberately after the personal plane stabilizes. Pinned principles:

- **Provenance is not ownership.** Personal annotations of org-context
  events live in the personal plane with **cross-plane references**
  (never auto-copies); on offboarding, org keys rotate (ordinary
  revocation), references dangle gracefully, the diary survives. The org
  keeps its records; the person keeps their memories.
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
- Today's org roots sign short-lived access documents only; org planes
  need real machinery (storage membership, key distribution, recovery,
  flow enforcement) — scoped there, not assumed here.

## 12. Connect: a storage subsystem, not endpoints

Connect currently keeps a monolithic in-memory store with whole-file JSON
persistence. The plane needs: bounded blob/object storage; transactional
metadata; immutable content-addressed segments; pagination + incremental
sync; per-plane quotas; GC/retention fences; backup/restore/migration;
plane deletion; crash recovery; abuse resistance — all in a self-hosted
mode that stays operationally simple. P2 is a storage project first.

## 13. Shared reducer and views

- A small pure crate (`owner-plane-core`) compiles native + WASM
  (precedent: `presence-core`) and owns canonical wire types,
  signature/capability validation interfaces, tenant reducer semantics,
  frontier/checkpoint types, deterministic projections, and test
  vectors. Crypto stays WebCrypto in browsers and Rust-native on daemons,
  pinned by shared golden vectors.
- "Derive, don't mirror" permits **materialized views**: persisted
  projections stamped with frontier + reducer version, rebuilt on parity
  failure. Nothing re-folds a forever log per list render.

## 14. Threat model

| Threat | Posture |
|---|---|
| Malicious Connect storage | AEAD, signed per-writer chains, frontiers, witnesses; withholding remains possible and is documented |
| Malicious hosted UI bundle | Degraded capability ceiling; trusted origin/native or executor-local approval for high effects |
| Compromised authorized writer | Capability scope, quotas, provenance, visible conflicts, revocation cutoffs |
| Future-clock poisoning | HLC never decides authorization or executable winners |
| Memory poisoning | Candidate status, provenance, bounded quoted-data retrieval, curated-only procedures/pins |
| Prompt injection via content | Sanitized Markdown, no uncontrolled remote resources, data-not-instruction framing, effects only via approval |
| Time-bomb scheduling | Immutable digest-approved manifests, two-authority rule, scheduled-session principal, fresh-frontier requirement |
| Cross-zone exfiltration | Explicit flow capabilities + honest session-egress pairing |
| Stale cancellation | Fresh control/revocation frontier required before firing |
| Double firing | Single v1 executor, fsync'd occurrence journal, idempotent creation, at-least-once + dedup |
| Crash around spawn | Durable Prepared/Started/Completed journal, reattach-before-retry |
| Key loss | Passkey + mandatory recovery envelopes; device re-enrollment |
| Stolen daemon | Capability revocation + epoch rotation; historical plaintext exposure documented |
| Storage spam / DoS | Ingestion/dispatch quotas (rate limits are not fold policy — folds must stay deterministic); Connect per-plane quotas |
| Coordination-dir forgery | File lane is proposals/data only; effects require authenticated channels |
| Schema downgrade | Versioned envelope/ops; unknown effectful operations fail closed |
| Compaction resurrection | Signed checkpoints + GC fences; behind-fence replicas rebootstrap |
| Selective erasure | Item content-key indirection reserved pre-freeze |
| Metadata leakage | Zone IDs/sizes/timing/recipients visible to Connect — documented, minimized where cheap |

## 15. Delivery plan

**Ordering rule: D0 blocks P2 (sync), not P1.** P1 needs plane/space/claim
semantics and IAM vocabulary behind a "pre-protocol, local-only" flag;
the wire protocol freezes before anything syncs.

- **D0 — this RFC, finalized**: plane root/recovery/enrollment; device
  cert (sign + KEM); plane/zone/space/network semantics; capability
  vocabulary + revocation cutoffs; canonical encoding + cross-language
  vectors; per-writer chains + frontiers; cold-start/split-view
  guarantees; segment/key hierarchy; checkpoint/GC + retire/erase;
  hosted-origin ceilings; Memory lifecycle + retrieval safety; Agenda
  manifests + occurrence semantics.
- **P1 — controller-owned Memory v1** (no sync): one personal plane;
  explicit project/workflow spaces; typed claims + candidate/curation
  lifecycle; principal-bound Memory service + ctl verbs + skill; bounded
  local lexical index; Memory Explorer UI; **no whole-store prompt
  injection; tombed-system cutover (Appendix A)**. *Exit:* attribution
  unforgeable; zone/space denial tested; conflicts represented, not
  overwritten; fresh sessions receive no unrequested memory.
- **P1.5 — coordination dir + collision radar (degraded mode)**: the file
  convention + skill; daemon detection/push lands with the daemon's
  normal release cadence. (Independent of the plane; can land anytime.)
- **P2 — plane sync**: Connect storage subsystem; browser + two
  integrated daemons; encrypted immutable segments; device removal +
  writer revocation + rotation; offline outbox + durability states;
  mediated queries for disposable boxes; cross-zone export; export/import
  + recovery. *Exit:* convergence under reorder/duplication;
  deterministic revocation cutoffs; peer IAM denial; zone isolation;
  restart recovery; cross-language crypto parity.
- **P3 — Agenda note/task tenant**: Add/Patch/Complete/Reopen/Retire;
  Agenda + Journal projections; no effects; provenance labels visible.
- **P4 — cross-links + promotion**: observation→memory→agenda→result→
  reflection flows; explicit exports; diary projection; bounded agent
  tools.
- **P5 — one-shot reminders**: single explicit executor; UTC occurrence +
  original timezone; rail + content-free push; quiet hours + caps;
  duplicate/missed policy. No voice/phone/recurrence/failover.
- **P6 — one-shot scheduled sessions**: immutable revisions; explicit
  approval; scheduled-session principal; executor-local profile;
  preflight (cost/tokens/wall-time/credentials/network); occurrence
  journal; result attestation + reflective proposal. Standing grants only
  after per-constraint enforcement is demonstrated.
- **Later**: durable questions/replies; user-created zones; sealed items;
  embeddings; foreign proposal inbox; peer gossip + witnessed frontiers;
  firing failover; voice/phone rungs (per-item consent); org planes;
  family/multi-owner governance; person/contact entities.

## 16. Testing requirements

- **Protocol/fold**: op permutations + duplicate delivery; missing causal
  deps; `(writer,sequence)` equivocation; duplicate request-ID/different
  bytes; future-HLC poisoning; concurrent patch/complete/reopen/retire;
  unknown op versions; checkpoint + old-replica resurrection; cold-start
  rollback + split-view simulation.
- **Authorization/crypto**: Rust/WebCrypto golden vectors; KEM + zone
  epoch rotation; expired/revoked/wrong-zone/space/kind capabilities;
  actor/signer mismatch; hosted-provenance ceiling; offline writer across
  a revocation cutoff; plane-capability-allowed but executor-IAM-denied.
- **Memory**: candidate vs accepted retrieval; contradictory claims;
  supersession + expiry; injection payloads retained as quoted data;
  procedural memory denied to low-trust writers; bounded retrieval/token
  budgets; export allow/deny; mediated search without zone key; read
  audit.
- **Agenda/scheduling**: approve-A-then-edit-to-B; concurrent executable
  revisions fail closed; stale cancel/reschedule; clock jumps +
  sleep/wake; crash before launch / after launch / before result;
  duplicate occurrence delivery; missing credentials; unavailable project
  binding; headless approval; scheduled-session sandbox/IAM enforcement.
- **Storage/ops**: segment corruption/truncation; partial upload + retry;
  quota exhaustion; backup/restore; plane deletion + GC; self-hosted
  migration; macOS/Linux/Windows + sleep/clock behavior.

## Appendix A — tombed Memory cutover inventory

Live hooks requiring deliberate removal/replacement in P1: the runtime
`store_memory`/`recall_memory` tools and their command fields; the
controller's `.intendant/memory.json` path injection; the runtime's
unlocked whole-file JSON read-modify-write; whole-store injection into
fresh conversations as user-role content; Presence's conflation of
durable memory, voice transcripts, and session-log search; prompt text
teaching the old system; recall vocabulary in control messages and
browser voice (requires a WASM artifact regen via the canonical builder);
the federation Knowledge capability advertisement with no peer
implementation; `intendant_core::knowledge`; `[memory]`/`inherit_memory`
config. Optional one-shot forensic importer only if old data is wanted —
imported entries become quarantined, untrusted legacy observations with
new provenance; old channels/cursors/IDs/timestamps/model-supplied
sources are never trusted semantics.

## Appendix B — resolved-decision log (chronological)

1. Owner-plane as a storage class; Agenda as first product; promotion,
   not sync, from agent memory. *(round 1)*
2. Zones = key domains; sessions-only execution; foreign proposal inbox;
   vault = sibling with opposite key policy. *(round 1)*
3. Org seam: provenance ≠ ownership; principal planes; cross-plane
   references; enrollment-grant offboarding contracts; honesty clause.
   *(round 2)*
4. Review adopted nearly wholesale: plane root + device certs (sign +
   KEM); two-resource authority; per-writer chains + frontiers; two-epoch
   revocation + cutoffs; durable outbox states; checkpoints/GC;
   kinds×effects; immutable manifests; single-executor occurrence
   journal; Memory-first; org RFC split; family-as-org ≠ governance.
   Nuances: D0 blocks P2 not P1; `agents` default zone dropped; witnesses
   v1 = own devices + transparency log. *(review round)*
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

## Appendix C — remaining open questions

1. Recovery-envelope custody UX for the plane root (who/what holds the
   recovery shares; product flow).
2. Notification capability defaults per device-provenance class (which
   lanes may set `push` vs `rail` without approval).
3. T0-equivalent digest cadence: dashboard-on-open only, or an opt-in
   daily digest (no real-time by default is decided).
4. Per-zone retention defaults beyond `private` = forever.
5. Coordination-dir location: per-project (`<project>/.intendant/
   coordination/`) vs per-box (`~/.intendant/coordination/`) vs both with
   scoping rules.
6. Whether P1.5's degraded collision radar ships as a fleet convention
   immediately (before any Intendant release vehicle).
7. Naming, final: "Agenda" (actionables) + "Journal" (projection) is the
   working answer; confirm before UI copy lands.
