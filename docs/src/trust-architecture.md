# Trust Architecture

> Status: adopted design, incremental rollout. This chapter records *why* the
> access system is shaped the way it is, what each piece of infrastructure is
> allowed to do, and the order in which the remaining pieces land. The
> operational reference for the UI lives in [Web Dashboard → Access](./web-dashboard.md#access);
> the daemon-to-daemon layer is in [Peer Federation](./peer-federation.md);
> the fleet-level operating model built on these mechanisms — which client
> for which daemon, custody per tier — is [Trust Tiers](./trust-tiers.md).

Intendant's goal is a **network of agentic networks**: daemons (agents) owned
by people and organizations, where an owner can grant other people and other
daemons scoped IAM access to their machines, infrastructure, and resources —
with pleasant abstractions (passkeys, one-phrase claiming, rendezvous, relays)
on top. The product constraint that makes this hard: the client stays a plain
web browser. No native app, no browser extension.

## The residual trust problem is code provenance

Break a hosted convenience service (`connect.intendant.dev`) into what it
could betray if compromised:

1. **Authentication** — it is the WebAuthn relying party for accounts.
2. **Introductions** — rendezvous/signaling between browsers and daemons.
3. **Relay** — TURN for NATed paths.
4. **Metadata** — fleet lists, labels, claim records.
5. **Application code** — the HTML/JS the browser runs.

Items 2–4 are already cheap to detrust: dashboard tunnels verify a
daemon-signed binding (daemon public key + session grant + client nonce), so a
malicious rendezvous can deny service but cannot impersonate a daemon; TURN
only ever sees ciphertext; metadata can be client-signed and client-encrypted.
Item 1 *feels* fundamental but collapses into item 5: trusted code can hold
keys locally and do identity without a server vouching for anything.

Item 5 is the one the web platform will not let us escape. The browser binds
code identity to *origin*, origin trust to TLS+DNS, and the server behind an
origin can change what it serves at any time, per user, silently:

- Service workers do not pin code — the worker script itself re-fetches from
  the origin, so the server always stays in control.
- IPFS and friends relocate the trust to whichever gateway resolves the
  content.
- Signed web bundles / Isolated Web Apps are effectively app installation
  under another name, and are not portably available.
- Passkeys prove user presence *to the relying party*. They protect the
  credential; they do not make the relying party honest, and they do nothing
  about the code the RP serves after login.

So within "pure browser, no extension" there is exactly one class of origin
that can serve privileged code without adding new trust: **an origin whose
compromise is already game over for you**. Your own daemon runs your agent
with real authority — it is already inside your trusted computing base.
Code served by a daemon you own adds zero marginal trust.

That observation fixes the design:

> **The rule:** privileged code is served by the resource owner or by
> yourself; authority is only ever minted by the target daemon's local IAM;
> global services carry introductions, ciphertext, and signatures — nothing
> else. A hosted service never wears two hats: it is not an authority, and it
> is not a code origin for privileged surfaces.

## Anchor daemons

Every browser earns its trust once, through one ceremony, with one daemon the
user controls — the **anchor**:

- The ceremony is the existing certificate enrollment
  (`intendant access serve-certs`, p12/mobileconfig) or an on-LAN bootstrap.
  It happens once per browser, not once per daemon.
- The anchor — or *any* daemon of the user's fleet; the serving role is
  deliberately fungible — serves the dashboard and the Access admin surface.
  All privileged logic executes in an origin backed by hardware the user
  owns.
- The app generates a **client identity key** in the browser (WebCrypto
  P-256, non-extractable private key, stored in IndexedDB). This key — not
  the mTLS certificate — is the durable identity. Browser storage is
  origin-scoped, which is load-bearing: a key created under the anchor's
  origin *cannot be wielded by code served from any other origin*. Hosted
  code cannot sign with your anchor key, by construction.
- Reaching any other daemon (yours, or an organization's) means: a rendezvous
  introduces the two ends; the channel binds to the target daemon's key
  (verified end-to-end, as today); and the client authenticates by signing
  the offer with its identity key. The target daemon resolves the key
  fingerprint against its local IAM and enforces the granted role. Nobody in
  the middle holds authority.
- Passkeys are reframed as **local key protection**, not server-side
  authentication: the WebAuthn PRF extension derives a wrapping key so a
  Face ID / Touch ID gesture unlocks the local identity key. No server is in
  the authentication loop at all.

A companion chapter, [Credential Custody](./credential-custody.md),
applies the same discipline to the *other* secret class — the model
provider credentials a daemon spends. Credential custody has shipped; the
custody chapter is the source for the current vault, lease, relay, and
caveat details.

The hosted service keeps exactly four jobs, all zero-authority:
introductions (signaling for endpoints that authenticate each other
end-to-end), blind relay, encrypted/signed fleet-metadata backup, and a name
directory. The rendezvous component is self-hostable ([how](./self-hosted-rendezvous.md)),
and a daemon's agent card states *which* rendezvous it uses
(`rendezvous_base`) — `connect.intendant.dev` is the default instance of an
open component, not a chokepoint.

## The trust ledger

What each component can do to you if it turns malicious, once the design is
fully rolled out:

| Component | Worst case if malicious | Why it is bounded |
|---|---|---|
| Your anchor daemon | Full compromise | It already runs your agent; nothing new is delegated to it |
| Another daemon of yours | That machine's authority | Each daemon is its own authority island |
| Org portal daemon (guest lane) | The guest's org-scoped session | Blast radius = the org's own resources; self-defeating |
| Rendezvous / signaling | Denial of service; first-introduction name games | Channels bind to daemon keys; claim phrases bind keys out of band; daemons co-sign claim bindings (v2 proofs) and flag asserted owners they never acknowledged; first-owner bootstrap phrases are daemon-minted (the service holds only a hash, so it cannot enroll a key of its own) |
| TURN relay | Denial of service; traffic analysis | Sees only ciphertext |
| Fleet metadata store | Denial of service | Records are client-signed (and encrypted where private); clients verify |
| Name directory | Handle confusion at first introduction | Key-first identity; handles are labels; org keys sign membership; append-only transparency log over all name bindings (STH pinned + consistency-verified by browsers; inclusion proofs on claims), optional DNS/GitHub attestation badges, invite-gated registration + reserved handles + dormant-handle reclamation |
| Hosted dashboard origin (degraded lane) | The session's granted authority | Sessions are principal-marked and role-capped below root by daemon policy |

Trust scales with the blast radius of the relationship: a global service that
could betray *everyone at once* is allowed to hold approximately nothing; the
party whose resources you are touching may hold exactly the trust that
relationship already implies; you hold everything about yourself.

## Organizations: two lanes

An organization is a **root keypair plus a handle** — not a row in a central
database. Org membership and grants are documents signed by the org key; each
org daemon verifies the chain and enforces its own local IAM, exactly as a
personal daemon does.

**Lane A — members bring their own agent.** The consistent version of the
network: Alice has her own daemon (laptop, VPS, anything), her browser trusts
only it, and the org binds *Alice's identity* — her client key, or her
daemon's peer identity — into IAM grants on its machines with scoped roles.
Alice touching org infra never requires Alice to trust org-served code, and
the org never has to trust Alice's infrastructure. The existing split between
the user/client domain and the peer domain is preserved on purpose: an org
can grant Alice-the-human (browser sessions via her key) or alice's-daemon
(agent-to-agent peer profile with filesystem scoping), and those are
different, separately auditable trust decisions.

**Lane B — guests.** A human with no daemon gets served the app by the org's
own **portal daemon** (orgs have real domains and can hold real TLS
certificates; the ACME/private-IP pain that rules this out for personal
daemons does not apply). The portal could betray its guest — but only with
authority over the org's own resources, which is a categorically smaller and
self-defeating threat compared to a global origin. For the true cold start —
fresh machine, no prior trust, nothing at stake — no browser-only design
escapes trusting *some* server for the first load. We refuse to pretend
otherwise: the hosted dashboard survives as an explicitly labeled
**degraded-trust tier** whose sessions are principal-marked and capped below
root by daemon-side policy (see role ceilings below), useful for emergencies
and first contact, honest about what it is.

## Phase 6 design: organization grants in detail

> Status: **implemented** (v1: steps 1-6 of the rollout below). Grant
> expiry, org root keys, signed grant documents, per-daemon trust with a
> local cap, materialization, the presentation/issue/trust/revoke
> endpoints, the offer ride-along (documents attached to dashboard-control
> offers), signed revocation lists + renewal, peer-subject documents, and
> issuer-key delegation are live. Revocation-list gossip over peer links
> remains. This section is the spec the code follows; the earlier "two
> lanes" section is the product narrative it serves.

### Objects

**Org identity.** An organization is an Ed25519 root keypair plus a handle.
The root key lives on an org-designated daemon
(`intendant org init <handle>` →
`~/.intendant/access-certs/org/<handle>/root.pk8`, 0600), following the
existing daemon-identity custody pattern; it is exportable for offline
custody. Day-to-day signing can move to delegated issuer keys certified by
the root; root-signed documents carry no chain, and delegated documents
carry the issuer certificate beside the signature it explains.

**Org grant document.** A self-contained, signed statement a member presents
to any daemon that trusts the org key:

```json
{
  "v": 1,
  "kind": "org-grant",
  "org": { "handle": "acme", "root_key": "<ed25519 b64u>" },
  "subject": {
    "client_key_fingerprint": "…",       // or "peer_fingerprint": "…";
    "label": "…"                         // exactly one subject fingerprint
  },
  "role_id": "role:session-reader",      // or "peer:<profile>" for peer subjects
  "targets": ["*"],                       // or explicit daemon ids
  "grant_id": "<uuid>",                   // stable id, used by revocation
  "issued_at_unix_ms": 0,
  "expires_at_unix_ms": 0,                // REQUIRED; hard cap 90d, default 30d
  "chain": [{ "…": "issuer cert object" }], // present when an issuer key signed
  "sig": "<ed25519 over the newline payload>"
}
```

The signing payload is newline-joined fields (the protocol style already
used by claim proofs and client-key offers), not canonical JSON. The
document is *authorization*, not authentication: only the bound subject can
use it, because sessions still authenticate the subject itself — a stolen
document is useless to anyone else, and third-party replay just
re-materializes the same grant. `chain` is omitted or empty when the root
signed the document and present with one issuer certificate when a
delegated issuer signed.

Subjects are exactly one of two cryptographic fingerprints: a browser
client key for the human lane, or a peer daemon certificate fingerprint
for the peer lane. Connect-account subjects are deliberately absent: they
would make the org-grant path only as trustworthy as the rendezvous's
account assertion — a compromised hosted service could claim to be a
granted member and collect the org's authority on every daemon that trusts
it. Client keys and peer certificates authenticate cryptographically
end-to-end. A member without a daemon still joins fine: any page mints a
key, the org grants that key, and hosted-origin keys remain ceiling-capped.
Account subjects can be added later as an explicit, documented weakening
if a real need appears.

**Daemon-side org trust.** `iam.json` gains
`trusted_orgs: [{handle, root_key, max_role, status, added_at}]`. Trusting
an org is a root-session action on each daemon — one click across the fleet
via the phase-4 fanout. `max_role` defaults to `role:operator`: an org can
never hand out more authority on your daemon than you allowed it, and
org-root requires an explicit local override (ceilings still apply on top,
by binding provenance, as today). Operator is the right default because
trusting an org is itself the consent moment, operator already excludes
access/settings/runtime administration, and a lower default would make the
org lane's normal grants (terminal, files, sessions) fail confusingly. A
document whose role exceeds `max_role` is rejected at presentation rather
than silently downgraded, so issuers learn the cap immediately.

### Verification and materialization

On presentation, the daemon verifies: signature against a *trusted* org
root or delegated issuer key → expiry → `targets` contains this daemon
(or `*`) → subject kind and role/profile namespace match → the document
fits under the trusted-org cap for its lane. A human-subject document
**materializes an ordinary local IAM grant** (`source: "org:acme"`, the
document's `grant_id`, expiry recorded) rather than evaluating documents
per-request; a peer-subject document materializes into the peer identity
store. Auditability and the existing evaluators come for free, and the
local owner can revoke or re-role the materialized authority like any
other; local IAM and local peer identity state always win.

Prerequisite schema work, valuable on its own: `IamGrant` gains
`expires_at_unix_ms: Option<u64>`, and enforcement treats an expired grant
as inactive (temporary grants for humans drop out of this immediately).

Presentation paths: an explicit `POST /api/access/org-grants` in the public
doorbell class (rate-limited, size-capped — the document itself is the
authorization), and an `org_grant` ride-along field on dashboard-control
offers so an org member's *first* connection to an org daemon materializes
the grant and proceeds in one round trip. The ride-along works the same on
both offer doors — the hosted rendezvous (`connect_rendezvous.rs`, with
`intendant-connect` relaying the field verbatim like the client-key
fields) and the daemon's own `/connect/dashboard/offer` — and runs
*before* grant resolution, so the freshly written grant resolves for the
very offer that carried it. Presentation failure is non-fatal: if another
identity resolves, the session proceeds (the error is logged and, on the
local path, surfaced in the answer); only when nothing resolves does the
org error ride back inside the refusal.

Browser side, the join fold stores a pasted document in
`localStorage` (`intendant_org_grants_v1`, keyed by org handle) even when
the presenting daemon refuses it — the daemon may simply not trust the org
yet. Offers then attach the freshest stored document that is unexpired,
bound to *this* browser's identity key, and targeted at the daemon (when
its id is known client-side). Since the identity key is origin-scoped,
a stored document only ever helps the browser it was issued to.

Because offers re-present automatically on every connect, materialization
is idempotent-quiet and local-wins: an unchanged presentation neither
rewrites `iam.json` nor grows the audit log, and a document whose
materialized grant (or subject principal) was *locally revoked* is
refused rather than resurrected — otherwise a member's browser would undo
the owner's revocation on its next reconnect. The org's escape hatch is a
fresh document (new `grant_id`); the owner's is re-enabling the grant from
a root session. This same property is what lets the step-5 revocation
list revoke by `grant_id` durably.

### Revocation

Layered, with the failure mode stated honestly:

1. **Short expiries + renewal** are the primary mechanism. Documents are
   cheap to re-issue; the org daemon serves renewals to still-valid members.
2. **Org revocation list**: the root signs
   `{org, seq, revoked_grant_ids[], revoked_subjects[], revoked_issuer_keys[]}`;
   org daemons serve it publicly, browsers carry it today, and peer-link
   gossip remains later plumbing; daemons enforce monotonic `seq` and
   apply it by revoking materialized grants and peer identities.
3. **Local override** always works: any daemon root can revoke an
   org-materialized grant locally, ORL or not.

An unreachable revocation list plus a long expiry is a stale-authority
window — hence the 90-day hard cap and the 30-day default.

**ORL format and semantics.** The list is cumulative and self-contained:

```json
{
  "v": 1,
  "kind": "org-revocations",
  "org": { "handle": "acme", "root_key": "<ed25519 b64u>" },
  "seq": 4,                          // monotonic; consumers refuse stale
  "revoked_grant_ids": ["…"],        // document grant_ids, not local grant ids
  "revoked_subjects": ["…"],         // subject fingerprints — "member is out"
  "revoked_issuer_keys": ["…"],      // delegated issuer keys revoked wholesale
  "issued_at_unix_ms": 0,
  "sig": "<ed25519 over the newline payload>"
}
```

The signing payload is newline-joined like every other protocol here and
includes the grant-id list, subject-fingerprint list, issuer-key list, and
issue time. The org daemon persists the current list next to the root key
(`org/<handle>/orl.json`), bumps `seq` on every change, and serves it at
`GET /api/access/orgs/<handle>/revocations` (public — it is org-public
data; an empty seq-0 list is signed lazily on first read).

**Delivery is carried, not discovered.** A consumer daemon has no
address for "the org daemon" — there is no membership server to ask — so
the list travels the same way grant documents do: anyone may push it to
`POST /api/access/orgs/revocations/apply` (doorbell class:
unauthenticated, rate-limited, size-capped — the signature is the
authority, and replaying an old list is refused by `seq`). Two couriers
exist today: the org admin's browser publishes the list to the
rendezvous **bulletin board** (blind storage — signature-checked and
rollback-proof, so the board can only withhold), and every member's
browser fetches it from there and hands it to each daemon it visits.
Peer-link gossip can come later; with browsers as couriers, revocations
already reach any daemon a member still talks to.

**Application.** A daemon that trusts the org verifies the signature
against its *trusted* key for that handle, requires `seq` strictly greater
than the last applied (equal is an idempotent no-op), then persists the
lists and the new `seq` on its `trusted_orgs` entry and revokes every
materialized grant whose document `grant_id`, subject fingerprint, or
issuer key is listed. Persisting matters beyond the sweep:
**materialization and renewal both check the stored lists**, so a revoked
grant_id, subject, or issuer key is refused *future* presentation too —
combined with the no-resurrection rule above, an ORL revocation sticks
even against a member who still holds a validly signed document.

**Renewal.** A member (or anyone carrying the document) presents a
still-valid document to the *org daemon* —
`POST /api/access/org-grants/renew`, doorbell class — and receives a
freshly signed copy: same subject, role, targets, and **the same
`grant_id`**, with `issued_at` set to now and the original lifetime span
preserved (capped at 90 days). Keeping `grant_id` stable is deliberate:
ORL revocation by grant_id keeps working across renewals, and the
document's identity is the grant, not the signature instance. The org
daemon refuses renewal for anything its own ORL lists (by grant_id or
subject) — expiry then retires the member within the document's remaining
lifetime. Only the daemon holding the org root key can renew, and renewal
grants nothing a fresh issue would not; it exists so membership can be
kept short-lived without the issuer in the loop.

### Rollout

1. ✅ Grant expiry in the IAM schema, evaluator, and UI.
2. ✅ Org identity + `intendant org init` + `trusted_orgs` + Access →
   Advanced → Organizations (trust / revoke / issue).
3. ✅ Document format, verification against trusted org keys,
   materialization into local IAM, and paste-to-join UI. The public
   presentation endpoint is `POST /api/access/org-grants` (doorbell class:
   unauthenticated, rate-limited, 16 KiB cap — the document is the
   authorization); trust/revoke/issue require `access.manage`.
4. ✅ Offer ride-along (one-round-trip first contact): browsers store
   pasted documents and attach them to dashboard-control offers on both
   the hosted-rendezvous and local paths; daemons materialize before
   grant resolution, idempotent-quiet, without resurrecting local
   revocations. E2E: `scripts/validate-org-grants.cjs`.
5. ✅ Revocation list + renewal: root-signed cumulative ORL maintained on
   the org daemon (`orl.json`, served publicly), carried to consumers via
   the public apply doorbell, enforced monotonically and persisted so
   listed grant ids/subjects are refused at materialization and renewal;
   renewal re-signs a still-valid document with the same `grant_id` and
   its original lifetime span. UI: revoke-member / copy-list / apply-list
   / renew flows under Access → Advanced → Organizations. Peer-link
   gossip and periodic pull remain for later plumbing.
6. ✅ Peer-daemon subjects and issuer-key delegation, per the design
   below and its sign-off decisions: fail-closed `max_peer_profile`,
   explicit presentation only (no peer-doorbell ride-along in v1.1), and
   chain-only issuer certificates (deputies initialize a key, the root
   delegates, documents carry the certificate; revocation lists revoke
   issuers wholesale via recorded `issued_via`). Renewal stays root-only
   for now. E2E: `scripts/validate-org-grants.cjs` scenarios 4–5.

### Step 6 design: peer subjects and issuer keys (built)

**Peer-daemon subjects.** An org grant whose subject is a *peer daemon*
materializes into the peer identity store
(`access/access_policy.rs::PeerIdentityRecord`), not IAM — daemons are
peers, never people, and the peer lane's profile vocabulary is the right
authority language for them. Format: `subject` carries
`peer_fingerprint` instead of `client_key_fingerprint` (exactly one must
be present), and the signing payload's subject-kind line — the literal
`client_key` today, deliberately baked into every existing signature —
becomes `peer_daemon`, so a signature can never be replayed across
subject kinds. `role_id` uses a `peer:<profile>` namespace (e.g.
`peer:session-reader`) so a peer document cannot be confused with a
human-role document even outside the payload.

Materialization upserts an approved `PeerIdentityRecord` bound to the
fingerprint. Two prerequisite schema steps, each valuable alone (the
same pattern as grant expiry in step 1): the record gains
`expires_at_unix` (org documents require expiry; enforcement treats an
expired record as revoked) and `source` (`org:<handle>` provenance, so
org revocation can sweep records it created and the UI can say where an
identity came from). The org's cap for the peer lane is a separate
`max_peer_profile` on the trusted-org entry. It is empty by default, so
trusting an org grants no peer authority until the owner sets a peer cap.
The cap relation is operation-set containment, not a strict ladder:
file-oriented and session-oriented profiles can be siblings, and a
document fits only when its profile allows no operation outside the cap.
Over-cap documents are rejected, not downgraded, like the human lane.
Local rules carry over verbatim: locally revoked records are never
resurrected, re-presentation is idempotent-quiet, and ORL
`revoked_subjects` matches peer fingerprints exactly as it matches client
keys.

**Issuer-key delegation.** Day-to-day signing moves off the root: the
root signs a delegation certificate

```json
{
  "v": 1, "kind": "org-issuer",
  "org": { "handle": "acme", "root_key": "…" },
  "issuer_key": "<ed25519 b64u>", "label": "…",
  "issued_at_unix_ms": 0,
  "expires_at_unix_ms": 0,          // REQUIRED; suggested cap 365d
  "max_role": "role:operator",      // optional role:* or peer:* scope; "" allows both
  "sig": "<root, newline payload>"
}
```

and a document may then carry `chain: [<issuer-cert>]` (the array
reserved since v1) with its own `sig` made by the issuer key.
Verification walks outside-in: the *trusted* root key validates the
cert, the cert must be unexpired, then the issuer key validates the
document. `max_role` is a scoped string despite the historic field name:
`role:*` scopes sign only human-subject documents, `peer:*` scopes sign
only peer-subject documents, and an empty scope allows both lanes. A
`peer:*` scope is enforced during verification by operation-set
containment; a `role:*` scope refuses peer documents during verification
and enforces human role permission containment during materialization,
where the receiving daemon's local IAM catalog exists. Everything else —
org cap, targets, expiry, ORL — applies unchanged. One level only: an
issuer cannot mint issuers.

Revoking an issuer revokes everything it signed going forward: the ORL
gains a `revoked_issuer_keys` list, which adds a line to the ORL signing
payload. Nothing consuming v1 lists has shipped outside this branch, so
the payload change lands as a plain extension of the v1 protocol before
first release; were that no longer true it would ship as an explicit
`v: 2` with dual-version acceptance. Materialized grants are swept by
matching the `chain` recorded at materialization time — which means the
materialization audit/grant record starts persisting the issuer key it
accepted.

**Decisions (signed off 2026-07-02).** (a) Explicit presentation only in
v1.1 — a peer-doorbell ride-along would bypass the peer approval queue
and needs its own design pass; (b) fail-closed: `max_peer_profile` is
empty by default and trusting an org grants no daemon-to-daemon
authority until the owner raises it — the two lanes are separate trust
decisions; (c) chain-only issuer certs — a published list adds no
verification value; documents stay self-contained.

## Mechanisms

The pieces that implement the model, mapped to the codebase:

- **Daemon-local IAM** (`~/.intendant/access-certs/iam.json`): principals
  (browser certificate, client key, Connect account, human user, peer
  daemon), grants (principal → role on this daemon), roles over the daemon
  permission catalog defined in `access/iam.rs`. Implemented; the source
  of all authority.
- **End-to-end tunnel binding**: dashboard-control offers are answered with a
  daemon-signed binding over (daemon public key, session grant, client
  nonce); the browser verifies before trusting the channel. Implemented —
  this is what demotes the rendezvous to an introducer.
- **Client identity keys**: browser-held WebCrypto P-256 keys; offers carry
  `client_key` + a signature over (daemon id, client nonce, SDP digest,
  timestamp); daemons verify (ring, fixed-form ECDSA) and resolve the key
  fingerprint to a local principal. The recorded key metadata includes the
  origin it was enrolled from, so policy can distinguish anchor-origin keys
  from hosted-origin keys. Grants to a key are created from an
  already-trusted session — an anchor-served root session — which is what
  makes the recorded origin meaningful.
- **Role ceilings**: daemon policy caps the *effective* permissions of
  low-provenance sessions regardless of the grant — by default,
  Connect-account principals and client keys enrolled from a hosted origin
  cannot exceed `role:operator` (no `access.manage`, no `settings.manage`,
  no `runtime.control`). Ceilings are enforced as a permission intersection
  at decision time and surfaced in the Access UI. Hardening is one knob —
  the hosted-control cap on the Trust tier card (Access → Overview) moves
  both hosted bindings to operator / observer / `role:none` (refuse
  entirely), per [Trust Tiers](./trust-tiers.md); raising a ceiling or
  clearing them stays an explicit `iam.json` edit for owners who accept
  hosted-root risk.
- **Device enrollment**: when a browser's *verified* key is refused, the
  daemon queues a pending enrollment (in-memory, capped, TTL'd — the queue
  grants nothing by itself); the owner approves it with a role, or denies it,
  from Access → People & Devices in an already-trusted session. Approval goes
  through the ordinary grant upsert with the key's route origin recorded, so
  ceilings and audit apply unchanged. The certificate ceremony happens once
  per *user*, not once per browser or per daemon.
- **Encrypted fleet sync**: the private fields of a synced fleet record
  (daemon URLs) are sealed with an AES-GCM key derived from the account
  passkey via the WebAuthn PRF extension — a secret the browser evaluates
  locally and the rendezvous never sees. Passkeys sync across the user's
  devices, so each device derives the same key; the hosted store holds
  ciphertext (`enc_fields`, signed as stored — fleet-record payload v3),
  and a device without the key still verifies the record and simply shows
  it locked. No PRF support degrades to plaintext-free sync of the public
  fields only.
- **Grant fanout**: the anchor-served Access page applies one grant across
  many daemons — an "Apply to" step in the grant flow calls each selected
  fleet daemon's IAM API directly (browser mTLS, cross-origin), with
  per-daemon results; every target authorizes independently and no central
  grant store exists. Cross-origin access to the six fleet Access APIs is
  gated by a per-daemon origin allowlist (itself, the macOS app scheme, its
  outbound peer routes, its approved inbound identities) that both drives
  the CORS echo and refuses state-changing requests from any other page —
  closing the hole where a cert-installed browser could be steered by an
  arbitrary website. This posture is daemon-wide: no API response is
  wildcard-readable, foreign-origin `/api/*` requests are refused, and only
  the deliberate public-bootstrap surfaces (`/config`, the agent card,
  local Connect signaling, the peer doorbell) remain open to any page.
- **Signed fleet sync**: fleet records that round-trip the hosted metadata
  store are signed by the pushing browser's identity key (over host id,
  label, and route URLs) and verified on read; the Access UI badges each
  synced record as verified-by-this-browser, signed-by-another-device,
  unverified, or hosted-claim. The store carries the signatures verbatim
  and cannot silently inject or relabel a daemon in your fleet view.
  Cross-device encryption of private fields (via passkey-PRF-derived keys)
  remains the follow-on step.
- **MCP principal binding**: `/mcp` requests (supervised backends, `intendant
  ctl`, the dashboard's tool RPC) enter the same evaluator as every other
  surface. Supervised agents authenticate with a session-scoped token derived
  from the daemon's per-process secret, binding them to
  `principal:agent-session:<id>`; tokenless loopback callers bind to
  `principal:local-process:loopback`; browser pages must present the daemon's
  own origin. Each tool call is checked against a per-tool permission map at
  call time, so `agent_session` / `local_process` grants scope what a given
  supervised agent or local shell may reach. Defaults stay root-compatible on
  a single-user daemon; once any agent session has ever been scoped, the
  tokenless loopback default fails closed until a `local_process` grant
  states what bare local callers get, and a lapsed grant (expired or
  revoked) denies rather than restoring default trust. See
  [MCP Server](./mcp-server.md#mcp-authorization).
- **Org root keys**: membership and role assertions signed by the org key;
  daemons verify signatures rather than trusting a directory. The global
  directory maps handle → org root key and is cross-checkable; a
  transparency log can be layered on when scale warrants it.

## Prior art

The pattern is proven elsewhere; we are assembling, not inventing:

- **Tailscale tailnet lock** — coordination server distributes node keys but
  cannot mint them; signatures chain to user-held keys. Our rendezvous +
  client keys is the same demotion.
- **Keybase** — server as untrusted directory over user sigchains;
  key-first identity with names as labels.
- **Matrix** — the client trusts *its* homeserver; federation carries
  verifiable events between sovereign servers. Our anchor daemon is the
  homeserver role.
- **Certificate Transparency** — the eventual answer for directory
  equivocation, once the namespace is worth attacking.
- **SPKI/SDSI and petnames** — authority bound to keys; human names are
  local, contextual labels.

## Rollout sequence

Each step is independently shippable and none breaks the poweruser mTLS path,
which remains first-class throughout:

1. **Client identity keys + key-signed offers** — establishes key-bound
   sessions on both the local and rendezvous signaling paths; removes the
   account-authority residue from Connect entirely (accounts become spam
   control and petname sync).
2. **Role ceilings for low-provenance routes** — closes the hosted-root hole
   by policy instead of by warning.
3. **Device enrollment via existing session** — kills the per-browser
   certificate dance without touching the trust model.
4. **Grant fanout from the anchor Access page** — one grant, N daemons, all
   authority local.
5. **Signed + encrypted fleet sync** — detrusts the metadata store.
6. **Org root keys + signed membership** — organizations as key-rooted
   namespaces.
7. **Self-hostable rendezvous as a first-class deployable** — the default
   instance stops being special.
8. **Directory transparency** — when the namespace is big enough to deserve
   it.
