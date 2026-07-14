# Trust Architecture

> Status: alpha trust boundary implemented for trusted anchors, immutable
> hosted refusal and migration, signed/encrypted fleet metadata, org documents,
> and transparency records. Browser-key authentication and peer-link ORL gossip
> remain staged/future. This chapter records *why* the access system is shaped
> the way it is and what each piece of infrastructure is allowed to do. The
> operational reference for the UI lives in [Web Dashboard → Access](./web-dashboard.md#access);
> the daemon-to-daemon layer is in [Peer Federation](./peer-federation.md);
> the fleet-level operating model built on these mechanisms — which client
> for which daemon, custody per tier — is [Trust Tiers](./trust-tiers.md).

Intendant's goal is a **network of agentic networks**: daemons (agents) owned
by people and organizations, where an owner can grant other people and other
daemons scoped IAM access to their machines, infrastructure, and resources —
with pleasant abstractions (passkeys, single-use route linking, and route
discovery) on top. The discovery client stays a plain web browser. Daemon
control requires local presence or a browser enrolled for independently
reached direct mTLS by a trusted owner; root remains on one of those trusted
surfaces. The packaged macOS app contains a local mTLS bridge, but no Developer
ID-signed/notarized release has been published for this alpha. An
`-unsigned-dev` app artifact is not a distribution trust anchor.

## The residual trust problem is code provenance

Break a hosted convenience service (`connect.intendant.dev`) into what it
could betray if compromised:

1. **Authentication** — it is the WebAuthn relying party for accounts.
2. **Introductions** — route discovery and daemon-presence metadata.
3. **Delivery** — optional encrypted Web Push notifications. TURN may be a
   separate future transport component, but hosted Connect does not relay
   daemon control in the default build.
4. **Metadata** — fleet lists, labels, claim records.
5. **Application code** — the HTML/JS the browser runs.

Items 2–4 can be bounded cryptographically: trusted daemon-origin dashboard tunnels
verify a daemon-signed binding (daemon public key + session grant + client
nonce), push payloads are encrypted to the browser subscription, and metadata
can be client-signed and client-encrypted. Fleet-record signatures are
self-contained rather than anchored to an owner/device trust set, so they
detect same-key alteration but do not stop a store from substituting a fresh
self-signed record on another device. None of that metadata grants daemon
authority. These mechanisms do not detrust item
5: Connect controls the page that asks for passkey assertions and PRF output,
so malicious code at that endpoint could read or misuse account, fleet, or
decrypted vault state made available to it. Item 1 is account authentication
and route metadata only; a Connect account assertion does not authenticate to
a daemon, and the default service exposes no browser-control signaling.

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
that can serve root-administration code without adding new trust: **an origin
whose compromise is already game over for you and whose route is independently
authenticated**. Your own daemon runs your agent with real authority — it is
already inside your trusted computing base. Code served over its loopback or
fingerprint-verified direct-mTLS route adds zero marginal trust. A WebPKI fleet
name does not: the rendezvous controls that DNS namespace and can serve code at
the same browser origin.

That observation fixes the design:

> **The rule:** root-capable code is served by the resource owner, runs from
> a trusted local console, or ships as a native client whose release signature
> the user verifies (none has been published for this alpha); authority is
> only ever minted by the target daemon's local IAM. A hosted claim carries
> discovery/route metadata and creates no principal or grant. In the default
> binary, hosted provenance is always capped at the zero-permission
> `role:none`; no local grant, persisted edit, or API call can turn hosted code
> into a daemon-control client.

## Anchor daemons

Every browser earns its trust once, through one ceremony, with one daemon the
user controls — the **anchor**:

- The ceremony is the existing certificate enrollment
  (`intendant access serve-certs`, p12/mobileconfig) or an on-LAN bootstrap.
  It happens once per browser, not once per daemon.
- The anchor — or *any* daemon of the user's fleet reached over an independently
  verified route; the serving role is deliberately fungible — serves the
  dashboard and the Access admin surface. Root administration executes in an
  origin backed by hardware and route identity the user controls.
- The app generates a **client identity key** in the browser (WebCrypto
  P-256, non-extractable private key, stored in IndexedDB) for fleet-record
  signatures, attribution, and future identity work. In this alpha it is not
  a daemon login credential: local presence or an mTLS client certificate is
  the active browser authentication boundary.
- Reaching any other daemon (yours, or an organization's) from a trusted
  client means: the rendezvous can supply route metadata, while the
  daemon-served client binds the independently reached channel to the target
  daemon's key and authenticates through loopback or mTLS. A future verified
  packaged native release could do the same for its bundled local daemon. Browser-key
  fingerprints can be stored in local IAM, but no alpha human ingress
  consumes a browser-key signature as authentication. Connect-served code has
  no control ingress at all.
- Passkeys authenticate the Connect account and derive keys for the encrypted
  fleet/vault envelopes. They do not make Connect-served code trustworthy and
  do not turn a claim into daemon authentication. The durable browser identity
  key is enrolled separately by an already-trusted daemon root.

A companion chapter, [Credential Custody](./credential-custody.md),
applies the same discipline to the *other* secret class — the model
provider credentials a daemon spends. Vault, lease, and relay components have
shipped, but a Connect-origin account vault has no trusted delivery bridge to a
daemon in this build; the custody chapter is the source for the exact boundary.

The hosted service keeps route discovery and presence, optional encrypted push
delivery, encrypted/signed fleet-metadata backup, account/route linking, a name
directory, and the browser/installer distribution surface. It has no daemon
control relay in the default build. None of those is a daemon authority mint, but they are
not "zero trust": Connect controls availability, account and route metadata,
and the code it serves. The rendezvous component is self-hostable ([how](./self-hosted-rendezvous.md)),
and a daemon's agent card states *which* rendezvous it uses
(`rendezvous_base`) — `connect.intendant.dev` is the default instance of an
open component, not a chokepoint.

## The trust ledger

What each component can do to you if it turns malicious in the current alpha;
rows explicitly marked future remain design constraints:

| Component | Worst case if malicious | Why it is bounded |
|---|---|---|
| Your anchor daemon | Full compromise | It already runs your agent; nothing new is delegated to it |
| Another daemon of yours | That machine's authority | Each daemon is its own authority island |
| Org portal daemon (guest lane) | The guest's org-scoped session | Blast radius = the org's own resources; self-defeating |
| Connect route directory | Denial of service; route/account misbinding; first-introduction name games | Daemons co-sign route links and flag asserted links they never acknowledged; claims create no daemon IAM state; this build exposes no hosted control signaling |
| Optional Web Push delivery | Denial of service; delivery timing analysis | Payloads are encrypted to each browser subscription; notifications carry no daemon authority |
| Future/separate TURN relay | Denial of service; traffic analysis | Not a Connect control path in the default build; any future use must preserve authenticated, end-to-end encrypted daemon channels |
| Fleet metadata store | Denial of service; false labels/routes or a substituted self-signed record on a new device | Private fields are encrypted; the current browser detects same-key alteration; records carry no daemon authority. An owner/device signer trust set is not shipped yet. |
| Name directory | Handle confusion at first introduction | Key-first identity; handles are labels; org keys sign membership; append-only transparency log over all name bindings (STH pinned + consistency-verified by browsers; inclusion proofs on claims), optional DNS/GitHub attestation badges, invite-gated registration + reserved handles + dormant-handle reclamation |
| Fleet DNS zone + WebPKI (fleet-name route) | Targeted endpoint swap: your daemon's fleet name answered with another box, a fresh certificate minted for it, attacker code served at that origin | The route is discovery-only. The daemon classifies accepted fleet SNI before IAM and rejects every protected HTTP/MCP route, direct signaling request, and WebSocket even when the browser presents an enrolled root certificate. Public shell/discovery bytes run as anonymous `role:none`. CT alerts are diagnostic evidence, not an authority control. |
| Hosted Connect origin (directory lane) | False or missing account/route/presence metadata; misleading UI; exfiltration of anything entered or unlocked on that page; malicious installers | Claims and account assertions grant nothing, and daemon-stamped hosted provenance is compiled to immutable `role:none`, so the default daemon exposes no control capability to this code. Served artifacts are transparency-logged (evidence, not prevention); installers remain a real software-supply-chain trust boundary. |
| Foreign browser origin / DNS rebinding | Drive a browser-held mTLS certificate or loopback root fallback cross-site | Every non-public HTTP route, direct dashboard signaling, and `/ws` reject foreign browser Origins before resolving transport authority; cross-/same-site Fetch Metadata closes navigation and subresource requests that omit `Origin`. Explicit authority-free shell/assets/discovery/signed-document doorbells run as anonymous `role:none`. Cleartext "own origin" is limited to `localhost` or a literal loopback address, so matching attacker-controlled Origin and Host values do not bypass the gate; non-loopback browser administration uses HTTPS/mTLS. |

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
only it, and the org binds *Alice's identity* — an mTLS identity for shipped
alpha human access, or her daemon's peer identity — into IAM grants on its machines with scoped roles.
Alice touching org infra never requires Alice to trust org-served code, and
the org never has to trust Alice's infrastructure. The existing split between
the user/client domain and the peer domain is preserved on purpose: an org
can grant Alice-the-human (the client-key document shape is staged until a
trusted direct authenticator consumes it) or Alice's daemon
(agent-to-agent peer profile with filesystem scoping), and those are
different, separately auditable trust decisions.

**Lane B — guests.** A human with no daemon gets served the app by the org's
own **portal daemon** (orgs have real domains and can hold real TLS
certificates; the ACME/private-IP pain that rules this out for personal
daemons does not apply). The portal could betray its guest — but only with
authority over the org's own resources, which is a categorically smaller and
self-defeating threat compared to a global origin. For the true cold start —
fresh machine, no prior trust, nothing at stake — no browser-only design
escapes trusting *some* server for the first load. We refuse to turn that
observation into ambient authority: the global hosted origin is a directory
lane only. It can link and locate a route, but it cannot create a daemon-control
session in the default product. An organization can offer browser control
from its own resource-owner portal; global Connect cannot be opted into
control.

## Organization grants: implementation and remaining work

> Status: **core implementation shipped**. Grant expiry, org root keys, signed
> grant documents, per-daemon trust with a
> local cap, materialization, the presentation/issue/trust/revoke
> endpoints, direct document presentation/materialization, signed revocation lists + renewal,
> peer-subject documents, and
> issuer-key delegation are live. Peer-subject documents are usable now;
> human client-key documents can materialize but cannot authenticate an alpha
> session because shipped human ingress does not consume browser-key proofs.
> Revocation-list gossip over peer links remains. This section is the spec the code follows; the earlier "two
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
document is *authorization*, not authentication: it is useful only when an
active transport separately authenticates the bound subject. Peer certificate
subjects have that ingress today. Browser client-key subjects do not in this
alpha, so materializing one creates record state but no usable login path.
`chain` is omitted or empty when the root
signed the document and present with one issuer certificate when a
delegated issuer signed.

Subjects are exactly one of two cryptographic fingerprints: a browser
client key for the human lane, or a peer daemon certificate fingerprint
for the peer lane. Connect-account subjects are deliberately absent: they
would make the org-grant path only as trustworthy as the rendezvous's
account assertion — a compromised hosted service could claim to be a
granted member and collect the org's authority on every daemon that trusts
it. Peer certificates authenticate cryptographically end-to-end. Browser
client keys remain staged subject vocabulary until a trusted browser-key
authenticator is implemented; current human access uses loopback or explicit
mTLS.
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
doorbell class (rate-limited and size-capped), and an `org_grant` ride-along field on a daemon-origin
dashboard-control offer so an org member's *first trusted direct-mTLS*
connection to an org daemon can materialize the grant and proceed in one round
trip. Materialization runs *before* grant resolution, so the freshly written
grant resolves for the offer that carried it. Connect does not relay or present
this field in the default build: the service rejects hosted control calls and
the daemon drops legacy Connect offers before parsing any ride-along. A failed
direct presentation is non-fatal when another identity resolves; otherwise its
error is surfaced in the local answer.

"Public" here describes an **authority-free courier door**, not public daemon
authentication. The caller identity receives no role and the HTTP request does
not become a dashboard/control session. The daemon parses the bounded document,
verifies its org signature and local trust/cap, and can materialize a grant only
for the cryptographic subject named inside it. A peer subject must later
authenticate with its peer mTLS key. A human browser-key subject is record-only
in this alpha: peer offers can verify that key for attribution, but no ingress
admits it as the authority-bearing IAM principal. The same distinction applies
to public renewal and ORL read/apply: they verify or return signed documents,
never authenticate the courier.

Browser-side storage (`intendant_org_grants_v1`, keyed by org handle) is a
record format, not a login or automatic alpha presentation promise. The default
Connect directory may display records already present in that origin, but it
does not present them to daemons. The retired browser-key offer code contains a
presentation field, yet no shipped human ingress authenticates that
key. Usable human access therefore requires an explicit trusted mTLS binding;
peer-subject documents remain usable through peer mTLS.

Materialization itself is idempotent-quiet and local-wins: an unchanged
presentation neither rewrites `iam.json` nor grows the audit log, and a
document whose materialized grant (or subject principal) was *locally revoked*
is refused rather than resurrected. The org's escape hatch is a fresh document
(new `grant_id`); the owner's is re-enabling the grant from a root session. This
same property is what lets the step-5 revocation list revoke by `grant_id`
durably.

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
unauthenticated, rate-limited, size-capped — the caller gets no authority;
the locally pinned org signature authorizes only application of the signed
revocation facts, and replaying an old list is refused by `seq`). Two couriers
exist today: the org admin's browser publishes the list to the
rendezvous **bulletin board** (blind storage — signature-checked, with
monotonic writes). The board cannot forge a newer list and a daemon refuses a
sequence below its local high-water mark, but the board can withhold the latest
list or serve an older signed list to a fresh daemon that has no sequence
history. Every member's browser fetches the board's copy and hands it to each
daemon it visits.
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
kept short-lived without the issuer in the loop. The unauthenticated courier
is not logged in by this call and cannot exercise the renewed subject's grant.

### Organization-grant implementation status

1. ✅ Grant expiry in the IAM schema, evaluator, and UI.
2. ✅ Org identity + `intendant org init` + `trusted_orgs` + Access →
   Advanced → Organizations (trust / revoke / issue).
3. ✅ Document format, verification against trusted org keys,
   materialization into local IAM, and paste-to-join UI. The public
   presentation endpoint is `POST /api/access/org-grants` (doorbell class:
   unauthenticated, rate-limited, 16 KiB cap — the signed document authorizes
   only subject-bound verification/materialization, never the courier's
   session); trust/revoke/issue require `access.manage`.
4. ⚠️ Human client-key presentation machinery is staged: browsers can store and
   present a document from trusted daemon-origin code, and daemons can verify
   and materialize it idempotently without resurrecting local revocations, but
   no alpha ingress admits the document's browser key as its controlling IAM
   principal. It therefore creates record state, not a usable human login. The
   hosted rendezvous path is disabled and cannot exercise a stored document.
   Peer subjects are usable through peer mTLS. E2E:
   `scripts/validate-org-grants.cjs`.
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

### Peer subjects and issuer-key delegation

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
payload. The v1 payload was extended before external compatibility was
promised. Any future incompatible change must use an explicit `v: 2` with
dual-version acceptance. Materialized grants are swept by
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
  (browser certificate, client key, metadata-only Connect account records,
  human user, peer daemon), grants (principal → role on this daemon), roles
  over the daemon permission catalog defined in `access/iam.rs`. Implemented;
  the source of all authority. Connect account records remain compatibility
  and audit vocabulary, not an authenticating binding.
- **End-to-end tunnel binding**: trusted direct dashboard-control offers are answered with a
  daemon-signed binding over (daemon public key, session grant, client
  nonce); the browser verifies before trusting the channel. Implemented for
  daemon-origin control. Fleet-SNI and hosted Connect signaling are rejected
  before a grant can be constructed. The retired Connect signaling format used the same
  binding, but the current service rejects those calls and current daemons
  refuse legacy events before opening a channel.
- **Client identity keys (staged, not alpha authentication)**: browser-held
  WebCrypto P-256 keys sign fleet records and may appear as peer-relay
  attribution. Verification and IAM record vocabulary for key-signed offers
  exist from the retired hosted design, but local `/ws`, direct
  `/connect/dashboard/offer`, and the reserved future native-bridge code do not
  consume that proof.
  A stored key grant therefore grants no usable alpha ingress; active browser
  control authenticates with loopback presence or mTLS.
- **Hosted authentication and immutable refusal**: a Connect account assertion
  is route metadata and never authenticates to the daemon. Daemon-stamped
  route provenance prevents a directly enrolled key from shedding hosted
  policy merely because its stored origin looks trusted. Both compatibility
  ceiling entries are normalized to `role:none` on every load; missing, empty,
  or hand-edited values also resolve to `role:none`. The default service returns
  `403` for authenticated browser offer/ICE/close calls before mutation, and
  the daemon drops those event kinds before touching control, IAM, or
  enrollment state. There is no ceiling-raising endpoint or disclosure
  checkbox: no browser-key grant can authorize a Connect-served control
  session. Any future hosted-control
  offering would have to be a deliberately separate binary/product and is not
  part of this release.
  Upgrading only Connect cannot tear down a legacy P2P session that is already
  established; complete the migration by restarting upgraded daemons, closing
  old Connect tabs, and allowing IAM schema v2 to revoke legacy
  `connect-bootstrap` grants.
- **Legacy hosted-root and `--owner` migration**: IAM schema v2 identifies
  principals whose client-key authentication origin is the retired
  `connect-bootstrap` sentinel, revokes every active grant on them, and records
  `revoke_legacy_connect_bootstrap`. It separately revokes active browser-key
  grants whose recorded reason came from the retired CLI `--owner` bootstrap
  and records `revoke_legacy_owner_browser_key_bootstrap`. Both hosted ceilings
  return to `role:none`; direct/mTLS root grants survive. Existing Connect
  account/route links are metadata outside IAM and remain linked. Remove
  `--owner` from old service commands (the parser now rejects it), restart the
  upgraded daemon, close old Connect tabs, and run `intendant access setup`
  locally to establish the generated owner mTLS credential.
- **Device enrollment (staged UI/state)**: the daemon has a bounded pending
  enrollment queue and ordinary key-grant upsert machinery, but the alpha
  shipped local/direct-mTLS transports do not present browser-key authentication, so this
  is not a usable sign-in path. Connect events are dropped before key parsing
  and never create an enrollment request. Remote alpha enrollment uses mTLS
  certificate setup instead; a claim never substitutes for trusted approval.
- **Encrypted fleet sync**: the private fields of a synced fleet record
  (daemon URLs) are sealed with an AES-GCM key derived from the account
  passkey via the WebAuthn PRF extension — bytes the browser evaluates
  locally and does not send in the protocol. Connect still controls that
  browser's JavaScript and can prompt for verification and exfiltrate the PRF
  output or decrypted state while the page has it. Passkeys sync across the user's
  devices, so each device derives the same key; the hosted store holds
  ciphertext (`enc_fields`, signed as stored — fleet-record payload v3),
  and a device without the key still verifies the record and simply shows
  it locked. No PRF support degrades to plaintext-free sync of the public
  fields only.
- **Grant fanout**: the anchor-served Access page applies one grant across
  many daemons — an "Apply to" step in the grant flow calls each selected
  daemon's independently verified direct-mTLS IAM API (browser mTLS,
  cross-origin), with
  per-daemon results; every target authorizes independently and no central
  grant store exists. Cross-origin access to the six fleet Access APIs is
  gated by a per-daemon origin allowlist (itself, the macOS app scheme, its
  outbound peer routes, its approved inbound identities) that both drives
  the CORS echo and refuses state-changing requests from any other page —
  closing the hole where a cert-installed browser could be steered by an
  arbitrary website. This posture is daemon-wide: no authority-bearing API
  response is wildcard-readable and foreign-origin `/api/*` requests are
  refused. `/config` is not public bootstrap: it requires `presence.read`,
  accepts only the daemon's own or a fleet-allowlisted Origin, and is
  `Cache-Control: no-store` because its ICE material may contain credentials.
  Fleet-name SNI is rejected even when CORS would otherwise allow the caller.
  Only authority-free bytes such as the agent card, `/connect/bootstrap`,
  `/connect/status`, and the peer doorbell remain public. Authority-
  bearing `/connect/dashboard/*` signaling and `/ws` apply the same own/app-
  origin gate before resolving local or mTLS authority; cleartext own-origin
  access is loopback-only to resist DNS rebinding.
- **Signed fleet sync**: fleet records that round-trip the hosted metadata
  store are signed by the pushing browser's identity key (over host id,
  label, and route URLs) and verified on read; the Access UI badges each
  synced record as verified-by-this-browser, signed-by-another-device,
  unverified, or hosted-claim. The signing public key travels inside the
  record and there is no owner/device trust set yet. The current browser can
  therefore detect alteration under its own key, but a malicious store can
  replace a record with a newly generated, internally valid self-signed one on
  another device. Connect-served code can also wield that origin's browser key
  while loaded. These signatures are integrity/attribution hints, not daemon
  authentication and not protection from a malicious hosted page or store.
  Private route fields are also encrypted across devices with the
  passkey-PRF-derived key described above; that protects stored ciphertext,
  not plaintext exposed to the currently running hosted page.
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
  each daemon explicitly pins the org root it trusts and verifies signatures
  locally. Handles are labels, not a global authority directory. Connect's
  append-only transparency log can make published metadata auditable, but it
  cannot add or replace an org root in daemon-local trust.

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
- **Certificate Transparency** — public evidence of certificate issuance and
  a useful route diagnostic, not a substitute for an authority anchor.
- **SPKI/SDSI and petnames** — authority bound to keys; human names are
  local, contextual labels.

## Alpha implementation status

The alpha keeps loopback and direct mTLS first-class while separating shipped
authentication from staged identity vocabulary:

1. **Trusted anchors are shipped** — local presence and direct mTLS are the
   remote-capable daemon-control entry points. The packaged macOS app has a
   loopback mTLS bridge in source, but no Developer ID-signed/notarized release
   has been published for this alpha. The release pipeline labels builds
   `-unsigned-dev` until signing credentials are provisioned; those artifacts
   are not distribution anchors.
2. **Hosted refusal and migration are shipped** — hosted provenance is fixed
   at `role:none`; Connect signaling is refused at both ends; IAM schema v2
   revokes grants created by the retired Connect bootstrap and `--owner`
   browser-key bootstrap.
3. **Client identity keys are metadata-only in this alpha** — they sign fleet
   records and remain useful subject vocabulary. Peer offers can verify one for
   attribution, but no live alpha ingress admits it as the controlling IAM
   principal. Device enrollment UI/state is therefore staged, not a replacement
   for mTLS.
4. **Local grant fanout, signed and encrypted fleet sync, org-signed grant and
   revocation documents, self-hostable Connect, and transparency-log records
   are implemented** — every daemon still makes its own trust and
   authorization decision.
5. **Future work requires a new trust design** — any browser-key login or
   hosted-control product must define a trusted authenticator and ship outside
   the default hosted build; it cannot reactivate the retired Connect path.
