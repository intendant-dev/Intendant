# Trust Architecture

> Status: adopted design, incremental rollout. This chapter records *why* the
> access system is shaped the way it is, what each piece of infrastructure is
> allowed to do, and the order in which the remaining pieces land. The
> operational reference for the UI lives in [Web Dashboard → Access](./web-dashboard.md#access);
> the daemon-to-daemon layer is in [Peer Federation](./peer-federation.md).

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

The hosted service keeps exactly four jobs, all zero-authority:
introductions (signaling for endpoints that authenticate each other
end-to-end), blind relay, encrypted/signed fleet-metadata backup, and a name
directory. The rendezvous component is self-hostable, and a daemon's signed
agent card states *which* rendezvous it uses — `connect.intendant.dev` is the
default instance of an open component, not a chokepoint.

## The trust ledger

What each component can do to you if it turns malicious, once the design is
fully rolled out:

| Component | Worst case if malicious | Why it is bounded |
|---|---|---|
| Your anchor daemon | Full compromise | It already runs your agent; nothing new is delegated to it |
| Another daemon of yours | That machine's authority | Each daemon is its own authority island |
| Org portal daemon (guest lane) | The guest's org-scoped session | Blast radius = the org's own resources; self-defeating |
| Rendezvous / signaling | Denial of service; first-introduction name games | Channels bind to daemon keys; claim phrases bind keys out of band |
| TURN relay | Denial of service; traffic analysis | Sees only ciphertext |
| Fleet metadata store | Denial of service | Records are client-signed (and encrypted where private); clients verify |
| Name directory | Handle confusion at first introduction | Key-first identity; handles are labels; org keys sign membership; transparency log later |
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

> Status: **implemented** (v1: steps 1-4 of the rollout below). Grant
> expiry, org root keys, signed grant documents, per-daemon trust with a
> local cap, materialization, and the presentation/issue/trust/revoke
> endpoints are live; signed revocation lists and renewal (step 5) and
> peer-subject/issuer-key delegation (step 6) remain. This section is the
> spec the code follows; the earlier "two lanes" section is the product
> narrative it serves.

### Objects

**Org identity.** An organization is an Ed25519 root keypair plus a handle.
The root key lives on an org-designated daemon
(`intendant org init <handle>` →
`~/.intendant/access-certs/org/<handle>/root.pk8`, 0600), following the
existing daemon-identity custody pattern; it is exportable for offline
custody. The format reserves a delegation chain so day-to-day signing can
later move to issuer keys certified by the root — v1 signs with the root
directly.

**Org grant document.** A self-contained, signed statement a member presents
to any daemon that trusts the org key:

```json
{
  "v": 1,
  "kind": "org-grant",
  "org": { "handle": "acme", "root_key": "<ed25519 b64u>" },
  "subject": {
    "client_key_fingerprint": "…", "label": "…"
  },                                      // client keys ONLY in v1 (see below);
                                          // peer_fingerprint subjects: v1.1
  "role_id": "role:session-reader",
  "targets": ["*"],                       // or explicit daemon ids
  "grant_id": "<uuid>",                   // stable id, used by revocation
  "issued_at_unix_ms": 0,
  "expires_at_unix_ms": 0,                // REQUIRED; hard cap 90d, default 30d
  "sig": "<ed25519 over the canonical payload>"
}
```

The signing payload is newline-joined fields (the protocol style already
used by claim proofs and client-key offers), not canonical JSON. The
document is *authorization*, not authentication: only the bound subject can
use it, because sessions still authenticate the subject itself — a stolen
document is useless to anyone else, and third-party replay just
re-materializes the same grant.

Subjects are **client keys only in v1**, deliberately: a Connect-account
subject would make the org-grant path only as trustworthy as the
rendezvous's account assertion — a compromised hosted service could claim
to be a granted member and collect the org's authority on every daemon
that trusts it. Client keys authenticate cryptographically end-to-end, and
a member without a daemon still joins fine: any page mints a key, the org
grants that key, and hosted-origin keys remain ceiling-capped. Account
subjects can be added later as an explicit, documented weakening if a real
need appears.

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
root key → expiry → `targets` contains this daemon (or `*`) → role exists,
is enforced, is not a peer profile for a human subject, and fits under the
org's `max_role`. It then **materializes an ordinary local IAM grant**
(`source: "org:acme"`, the document's `grant_id`, expiry recorded) rather
than evaluating documents per-request — auditability and the existing
evaluator come for free, and the local owner can revoke or re-role the
materialized grant like any other; local IAM always wins.

Prerequisite schema work, valuable on its own: `IamGrant` gains
`expires_at_unix_ms: Option<u64>`, and enforcement treats an expired grant
as inactive (temporary grants for humans drop out of this immediately).

Presentation paths: an explicit `POST /api/access/org-grants` in the public
doorbell class (rate-limited, size-capped — the document itself is the
authorization), and an `org_grant` ride-along field on dashboard-control
offers so an org member's *first* connection to an org daemon materializes
the grant and proceeds in one round trip.

### Revocation

Layered, with the failure mode stated honestly:

1. **Short expiries + renewal** are the primary mechanism. Documents are
   cheap to re-issue; the org daemon serves renewals to still-valid members.
2. **Org revocation list**: the root signs
   `{org, seq, revoked_grant_ids[], revoked_subjects[]}`; org daemons serve
   it publicly and gossip it over peer links; daemons enforce monotonic
   `seq` and apply it by revoking materialized grants.
3. **Local override** always works: any daemon root can revoke an
   org-materialized grant locally, ORL or not.

An unreachable revocation list plus a long expiry is a stale-authority
window — hence the 90-day hard cap and the 30-day default.

### Rollout

1. ✅ Grant expiry in the IAM schema, evaluator, and UI.
2. ✅ Org identity + `intendant org init` + `trusted_orgs` + Access →
   Advanced → Organizations (trust / revoke / issue).
3. ✅ Document format, verification against trusted org keys,
   materialization into local IAM, and paste-to-join UI. The public
   presentation endpoint is `POST /api/access/org-grants` (doorbell class:
   unauthenticated, rate-limited, 16 KiB cap — the document is the
   authorization); trust/revoke/issue require `access.manage`.
4. ⏳ Offer ride-along (one-round-trip first contact) — presentation is
   an explicit call for now.
5. ⏳ Revocation list + renewal endpoint + peer gossip.
6. ⏳ Peer-daemon subjects and issuer-key delegation.

## Mechanisms

The pieces that implement the model, mapped to the codebase:

- **Daemon-local IAM** (`~/.intendant/access-certs/iam.json`): principals
  (browser certificate, client key, Connect account, human user, peer
  daemon), grants (principal → role on this daemon), roles over an 18-gate
  permission catalog. Implemented; the source of all authority.
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
  at decision time and surfaced in the Access UI, and are configurable in
  `iam.json` for owners who explicitly accept hosted-root risk.
- **Device enrollment**: when a browser's *verified* key is refused, the
  daemon queues a pending enrollment (in-memory, capped, TTL'd — the queue
  grants nothing by itself); the owner approves it with a role, or denies it,
  from Access → People & Devices in an already-trusted session. Approval goes
  through the ordinary grant upsert with the key's route origin recorded, so
  ceilings and audit apply unchanged. The certificate ceremony happens once
  per *user*, not once per browser or per daemon.
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
