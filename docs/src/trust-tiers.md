# Trust Tiers

> Status: adopted doctrine (2026-07-08). [Trust Architecture](./trust-architecture.md)
> bounds what each *component* may do to you if it turns malicious; this
> chapter is the operating model an owner applies across a fleet whose
> machines carry different stakes. Almost nothing here is new mechanism — it
> composes ceilings, grants, custody, and client choice that already exist.
> The product hooks at the end are the shipped exceptions.

## Two axes, not one

"How much should I trust Intendant?" is a single axis, and the wrong one.
Every real deployment decision sits on two independent axes:

- **Payload tier of a daemon** — what a compromise of that box costs its
  owner. At one end, a **disposable** daemon: a rented VPS holding a
  time-boxed provider lease and scratch work; worst case is a rotated key
  and a destroyed box. At the other, an **integrated** daemon: the machine
  that reads your mail, holds your files, drives your accounts; worst case
  is your life.
- **Client provenance** — how sure you are that the code driving a daemon
  is honest. Hosted Connect is convenient and zero-install for discovery,
  but is no longer on the control ladder at all: the default binary fixes it
  at `role:none` because the hosted origin can change what it serves at any
  time. Control starts with code whose provenance you accept: a dashboard
  served by a daemon you own over loopback or an independently
  fingerprint-verified direct-mTLS route (the
  [anchor rule](./trust-architecture.md#anchor-daemons)). The packaged macOS
  app contains a local mTLS bridge, but no Developer ID-signed/notarized
  release has been published for this alpha; an `-unsigned-dev` bundle is not
  a distribution anchor.

The doctrine is one sentence: **match the client's provenance to the payload
of the daemon it is driving — per daemon, not per person.**

Stated per tier, and resolving what looks like a paradox:

- A *disposable* remote daemon still wants a trusted client, usually an
  independently reached direct-mTLS origin. Custody then bounds what
  compromise of the disposable box costs. Linking it in hosted Connect or
  publishing a WebPKI fleet name makes the route easy to find; neither
  service-controlled name is a control origin.
- Driving an *integrated* daemon demands provenance and authentication:
  loopback or an independently reached owner-served direct-mTLS origin. A
  future Developer ID-signed/notarized packaged macOS release may add its
  local bridge for the bundled daemon. This is
  where "just open intendant.dev" stops being
  an acceptable answer, however encrypted the transport and however honest
  the service intends to be.

Trust machinery scales with payload, not with paranoia. A user who keeps every
daemon disposable can lean heavily on hosted discovery and short-lived
credential custody, but control still starts with direct enrollment. A user
who trusts one integrated machine with everything adds stricter client and
credential discipline on that machine; neither tier turns a service-controlled
origin into an authority surface.

One footgun completes the model: **a daemon's tier is set by the most
sensitive thing that ever touches it**, not by the label its owner had in
mind. Pasting a production credential into a session on a disposable box
silently promotes the box. Tier discipline is as much about what you feed a
daemon as about how you reach it.

There is a third axis, easy to conflate with client provenance and
untangled [below](#first-contact-three-rungs): **first contact** — who
named the route you followed to reach a daemon at all, and what evidence
their betrayal would leave.

## One fleet, zones — not two networks

The instinct, once both tiers exist side by side, is to ask for two fleets
or two isolated networks. Neither is needed, because a fleet is a phonebook,
not a trust domain: [claiming grants no authority](./self-hosted-rendezvous.md),
membership is directory metadata, and every daemon's local IAM remains the
only mint. Two daemons in one fleet are no more security-coupled than two
strangers — until an owner writes a grant. The boundary between tiers is
therefore made of three disciplines, not of infrastructure:

1. **Grants flow down the trust gradient, never up.** An integrated daemon
   may hold peer grants on disposables (it orchestrates them); no disposable
   ever holds a grant on an integrated daemon. An upward grant is the only
   way tiers actually bridge, which makes it the alarm condition — the one
   thing a fleet owner should never do casually.
2. **Hosted provenance is an immutable refusal.** The
   [role ceilings](./trust-architecture.md#mechanisms) normalize hosted
   sessions to `role:none` (`role_ceilings` remains compatibility state in
   `iam.json`). Missing, empty, or hand-edited values fail closed, and the
   default build exposes no knob that raises them.
3. **Separate credentials, not separate networks.** Browser identity keys are
   fleet-signing/attribution records in this alpha, not daemon login
   credentials. Authority belongs on trusted loopback or direct-mTLS
   surfaces; the packaged macOS app code contains a local bridge to its bundled
   daemon, not a remote-client transport, but the current unsigned artifact is
   not an anchor. Hosted Connect and fleet-name SNI have no control ingress.
   Keep integrated-daemon root material in a dedicated direct profile/device;
   use hosted and fleet-name profiles only for account and route metadata.

Two accounts — two actual fleets — buy exactly one additional property:
the rendezvous cannot see that both worlds belong to the same person.
That is metadata unlinkability, a legitimate but niche posture, and it is
opt-in paranoia rather than the recommended shape.

## Custody inverts across tiers

The [credential custody](./credential-custody.md) mechanisms — sealed stores,
time-boxed leases, and client egress — are most valuable on boxes you do not
trust. The current boundary matters: Connect account-vault blobs cannot be
delivered to a daemon because no trusted bridge ships. Use a daemon-store vault
from a loopback/direct-mTLS client, or local credential configuration. A
future signed/notarized packaged app release may use its local bridge for its
bundled macOS daemon:

- **Disposable tier**: prefer memory-only API-key leases from an authorized
  daemon-store vault. A deliberately keyless box outside an active
  full-credential OAuth lease can avoid durable provider secrets; `.env` and
  full-credential OAuth mode do write durable/private material.
- **Integrated tier**: the box is already inside your trusted computing
  base — it runs the agent that reads the mail. It may simply hold its own
  credentials (OS keystore, local config), because routing them through the
  account vault adds a hosted dependency without adding safety. Where vault
  storage is still preferred (cross-device sync, sealed-at-rest), those
  entries want a stricter unseal policy than the disposable tier's — see
  hook 3.

## The client ladder

- **Disposable tier**: loopback or an independently reached direct-mTLS
  origin. Hosted Connect and the fleet WebPKI name remain discovery routes.
- **Integrated tier**: the same shipped anchors, with stricter device and
  credential discipline. A Developer ID-signed/notarized native release could
  become an out-of-band code anchor a bare browser tab cannot have, but no such
  release is available in this alpha; the current `-unsigned-dev` artifact is
  development-only.

Getting a direct control origin today: use a typed/pinned address, an
owner-controlled hostname, mDNS or a tailnet route, then complete the direct
mTLS enrollment ceremony. The fleet strip may remember that independently
verified URL, and the daemon-store vault
([Credential Custody](./credential-custody.md#the-vault)) makes the trusted
tab self-sufficient.

**Fleet certificates are different.** A rendezvous serving a delegated DNS subzone
([Self-Hosted Rendezvous → Fleet DNS](./self-hosted-rendezvous.md#fleet-dns-real-certificates-for-daemons))
gives each daemon a real name, and the Connect card's *Enable HTTPS
discovery* button publishes the daemon's addresses (LAN included — no
port forwarding needed) and mints a Let's Encrypt certificate via DNS-01,
renewed automatically, private keys never leaving the machine. That gives a
warning-free public shell/discovery endpoint, not a control endpoint. The
rendezvous controls the name and can serve code at the same origin, so the
daemon rejects every protected HTTP/MCP route, direct signaling request, and
WebSocket on fleet SNI before it considers browser mTLS or IAM. CT monitoring
is useful evidence of unexpected issuance, not authority. Certless root exists
only on verified loopback; `--allow-public-plaintext` and fleet WebPKI grant no
authority.

A worked example, one fleet:

| Daemon | Tier | Control origins | Custody | Peer grants |
|---|---|---|---|---|
| `home` (Mac desktop) | integrated | loopback or fingerprint-verified direct mTLS | local keystore or daemon-store vault | holds grants **on** `vps-1`, `vps-2` |
| `vps-1`, `vps-2` (rented) | disposable | fingerprint-verified direct mTLS; fleet name for discovery only | prefer memory-only leases; full OAuth may materialize files | none; controlled **by** `home` |

The owner links all three routes to one account and sees them in the directory.
Linking changes no IAM. Remote control uses the separately verified direct
routes; grants on the disposable boxes are still scoped independently from
grants on `home`.

## First contact: three rungs

The two axes above describe steady state: a client you already hold,
driving a daemon you already reach. A third question is orthogonal to both
and only looks answered until you ask it precisely: **who did you have to
trust to reach the daemon at all — and what evidence would their betrayal
leave?** Client provenance says who serves the code that runs; first
contact says who *named the route* you followed to it. The answers can
differ for the same URL, and conflating them is how "it's daemon-served"
quietly overstates a link's safety.

Three rungs, ordered by what betrayal costs the attacker:

1. **Trustless — nothing between you and the box.** A typed direct address
   plus the enrollment ceremony (the fingerprint verified out of band pins
   the daemon's certificate), preinstalled mTLS material, or a client you
   built or installed yourself. No third party participates in naming or
   serving, so there is nothing whose betrayal you would need evidence of.
   This is the only rung that deserves the word *anchor*, and it is bought
   with the one deliberately inconvenient ceremony.
2. **Warning-free discovery with evidence — the fleet name.**
   `https://d-<hash>.fleet.intendant.dev:8765` is daemon-served code on a
   rendezvous-named route: the zone operator — or anything else that can
   answer DNS for the name and convince a CA — could point your daemon's
   name at a box of its choosing and serve same-origin JavaScript. CT can
   leave evidence of a newly issued certificate, but that is detection after
   the trust decision, not an anchor. The default daemon therefore treats
   fleet SNI like the hosted tab for authority: public shell and discovery
   bytes only, with protected HTTP/MCP, signaling, and WebSocket traffic
   refused before mTLS or IAM resolution. The CT tripwire remains a useful
   diagnostic for the route directory. Assigned fleet names are remembered
   durably even when Connect is later disabled or reports no current zone; a
   previously service-controlled name never decays into a direct anchor.
   Pre-provenance installs recover exact names from `fleet-cert.pem` on
   startup; malformed or incomplete recovery conservatively treats unknown
   DNS browser-key origins as fleet provenance until repaired.
3. **Directory only — the hosted tab.** The rendezvous origin serves
   the code itself, so betrayal is a silently different bundle to one
   visitor, once, with no artifact anywhere. No evidence machinery can
   apply. The default product therefore gives it no daemon authority at all:
   claims grant nothing and hosted provenance is immutably `role:none`.
   Connect can still lie about or exfiltrate its own account, route, presence,
   and unlocked vault UI state, and its installers remain a real trust boundary.

The product states route provenance wherever it displays historical/staged
enrollment metadata (*via direct origin* / *via fleet name* / *via hosted
route*). Fleet and hosted origins are refusals, not lower-authority control
tiers. Marking an origin in `hosted_origins` also means refusing it with
`role:none`; it is not a ceiling-raising mechanism.
Device enrollment (`intendant access serve-certs`) is intentionally stricter
than ordinary navigation: it always uses the direct-address access certificate
and requires the browser-observed fingerprint, shortened to an 80-bit prefix
since pre-grinding a certificate that shares 20 hex characters is out of reach.
A warning-free fleet URL is not accepted as root bootstrap evidence and cannot
use a previously enrolled client certificate for control; hosted DNS or origin
control must never be enough to release or exercise the shared owner/root
client bundle.

One consequence is easy to miss: for any *browser* client, first contact
re-asks itself on every page load — the tab re-fetches its code each
visit, so a rung's guarantee is only as durable as its serving origin.
Enrolled identity keys or mTLS certificates do not change this: browser
credentials are presented to the origin, so whatever code that name serves can
try to wield them. The daemon-side fleet-SNI refusal is the controlling
boundary. A native app could instead collapse code trust to install-and-update
moments, and the repository contains a signing/notarization pipeline, but its
credentials are not provisioned and no Developer ID-signed/notarized release
has been published for this alpha. Tags without those credentials produce
clearly labelled `-unsigned-dev` artifacts. Separately, serving origins are
answerable to **code transparency** —
the artifacts an origin serves are committed to the rendezvous's public
append-only log, and `intendant hosted-verify` re-downloads them exactly
as a browser would and checks them against the log from a machine the
origin does not control.

### Still blurry, on purpose

Named honestly rather than smoothed over — each is either tracked or a
stated non-goal:

- **The time axis (TOFU).** Everything above grades *first* contact;
  later visits inherit pinned material (enrolled keys, remembered
  certificates) but re-inherit the code channel every load. A future signed
  app release could collapse code trust to install-and-update moments; every
  browser client still re-runs its route risk per visit. Fleet-name control is
  refused rather than betting authority on TOFU.
- **The update channel.** A signed app trusts its updater. For the
  *serving* channel the evidence leg is shipped: served-artifact
  manifests live in the rendezvous's public transparency log, verified
  out of band by `hosted-verify` and advisorily by every daemon's
  bundle tripwire. The *release* channel has code for the same tie, but no
  signed/notarized release exists yet: when a release workflow is run it can
  publish artifacts (public source and workflow runs; Developer ID +
  notarization only after credentials are provisioned), and the pipeline
  commits every artifact's hash to the same log
  (`release_manifest` entries), `hosted-verify --releases` checks the
  log against GitHub out of band, and the app's update check surfaces
  logged / not-logged as a fail-open advisory.
- **Lookalike names.** `d-<hash>` labels are deliberately opaque, which
  also means humans cannot eyeball them; a phished lookalike with its own
  valid certificate raises no CT alarm on *your* name, because it is not
  your name. Two mitigations now, one navigational and one nominal:
  reach fleet names from the fleet strip or bookmarks — never by retyping —
  and give machines **petnames**: the owner's name for a
  daemon, signed into the fleet record (payload v5) and keyed to the
  record's identity, shown first everywhere with the self-reported label
  demoted to a muted secondary. A lookalike arrives *nameless* — no
  store, phisher, or self-chosen label can dress it in a name you
  assigned.
- **The browser itself.** Every rung assumes the browser and OS are
  honest; an extension with page access reads all tiers alike. Outside
  Intendant's reach — stated so the ladder is not mistaken for covering
  it.
- **Account-vault status and hosted-passkey coupling.** Connect's account-vault
  API stores opaque blobs, but the default hosted directory ships no vault
  client or crypto worker that creates, unseals, or spends vault envelopes. A
  future hosted client that unsealed them with a passkey PRF would still be
  rung-three code wielding rung-one credentials: Connect would control the page
  and worker selection, could prompt for a PRF evaluation, and could exfiltrate
  the output, decrypted state, or plaintext it rendered. Passkey sealing narrows
  ambient and at-rest exposure; it does not detrust the hosted origin. The
  stronger current boundary is that Connect has no daemon-control channel or
  vault-delivery bridge at all.

## Product hooks

Four pieces of mechanism let the product carry this doctrine instead of
the owner's memory. All four are **shipped**:

1. **Tier labels + upward-grant guard.** Each daemon carries its tier in
   local IAM (`tier` in `iam.json`; `POST /api/access/tier`,
   audit-logged, manage-gated), chosen on the **Trust tier card** at the
   top of Access → Overview. The guard is advisory and local-tier-driven:
   on an integrated machine, the peer pairing-approval card warns that
   approving grants a peer authority *here* (the upward-grant alarm), and
   direct enrollment records whose key originated at a hosted origin get an
   integrated-tier warning chip. Connect itself never queues an enrollment.
   When a verified doorbell caller
   states its own tier ([Where fleet metadata
   rides](#where-fleet-metadata-rides)), the alarm sharpens: a disposable
   machine asking for authority on an integrated one is named as exactly
   that. Same-account cross-daemon
   visibility ships via the signed fleet record — each fleet card carries
   its daemon's tier chip, offline daemons included (the carrier
   reasoning is [Where fleet metadata rides](#where-fleet-metadata-rides)).
2. **Immutable hosted refusal.** Both hosted-provenance compatibility entries
   are forced to the zero-permission `role:none` on every IAM load. The former
   hosted-ceiling UI/API is retired; missing, empty, or hand-edited values
   cannot enable hosted control.
3. **Per-entry vault unseal policy.** Vault entries accept
   `unseal_policy: "trusted"` (add form + per-entry toggle). The shipped vault
   UI is daemon-origin and backed by the daemon store; trusted-only entries work
   normally from that direct dashboard. Connect's account-vault endpoints store
   opaque blobs, but the default hosted directory has no vault UI and cannot
   invoke lease fueling, egress relay, or the voice mirror because no
   control/delivery bridge ships. The custody trail stamps every lease/relay
   ceremony with the session's origin class
   (`hosted`/`direct`/`local`/`peer`). Any future hosted vault client would need
   to honor the trusted-only policy, but that would remain client-side mistake
   prevention, not protection from a malicious served bundle. See the **local
   vault** in [Credential Custody](./credential-custody.md#the-vault).
4. **First-contact route, surfaced and watched.** Historical/staged enrollment approvals
   carry a route chip computed daemon-side (`iam::origin_route_class`:
   hosted / fleet / direct / unknown — route provenance for approval
   decisions, distinct from `session_origin_class`, the custody-trail
   code-provenance class), with honest per-rung copy and an
   integrated-tier warning on any network route. Fleet and hosted classes do
   not admit control. The **CT tripwire** is a route diagnostic: `fleet_cert`
   records the serial of
   every certificate it obtains (before install, so a crash cannot make
   an own certificate look foreign), polls crt.sh for the daemon's fleet
   name on each renewal tick, and flips the Connect card to **CT ALERT**
   on any serial the daemon never requested. Advisory and fail-open by
   design: a crt.sh outage stamps `ct_last_error` rather than blocking
   renewal.

## Two lanes: whose authority a pane spends

"Browser→daemon vs peer-to-peer" conflates two axes. The *transport* —
who carries the bytes — genuinely mixes: a peer-routed terminal is
signaled through the daemon you're logged into but its data plane is a
direct browser↔target datachannel. Hosted Connect has no control tunnel. The axis that carries trust
weight is the **principal**: whose authority the *target* daemon
enforces and audits. Every fleet surface sits in one of two lanes:

- **The user lane** — the target binds *you*: a loopback/direct-mTLS tab. A
  future Developer ID-signed/notarized packaged app release could add its
  bridge for the locally bundled macOS daemon; the current unsigned bundle
  cannot.
  The local session or mTLS certificate is the
  shipped alpha principal; a browser identity key is record-only and the
  Connect account is route metadata, not an authenticator. Your role applies
  and the audit names you.
- **The delegation lane** — the target binds *a daemon*: the peer-routed
  panes (terminal, files, folded sessions, displays) are admitted under
  the intermediary's peer grant (`DashboardControlGrant::Peer` — its
  fingerprint, its profile). You are invisible to the target: it cannot
  distinguish you clicking from the intermediary's agent acting, and its
  audit names the daemon. Spending the intermediary's peer grants is an
  operation *on the intermediary*, gated by your grant *there*.

Neither lane is the degenerate case of the other, and they deliberately
do not merge. The user lane is for **owner control** — reaching machines
that know you. The delegation lane is for **orchestration and downward
reach** — an integrated anchor conducting its disposables, or seeing a
box that has granted your daemon (not you) access. The
grants-flow-down discipline is what keeps the delegation lane safe: a
loopback/mTLS-authenticated session on your anchor can spend its peer grants
only down the tier gradient. A hosted Connect tab cannot spend them.

Lane rules, stated once:

1. **User-lane-only capabilities.** A target's Access administration
   (`access.manage`) and everything credential custody touches
   (`credentials.manage` — leases, vault blobs, the deposit lane) are
   never reachable through the delegation lane: no peer profile grants
   these operations, `role:admin-peer` included (the profile matrix in
   `access/access_policy.rs` enforces it). This is doctrine, not a v1
   deferral: authority over who may reach a machine, and over the
   secrets it spends, must always be exercised by an identified person
   the target itself admitted — a laundered principal is exactly the
   wrong identity to record for either.
2. **Lane switches are trust events.** Every pane states whose authority
   it is spending as a badge — *you · admin* versus *via dell‑206 ·
   operator* — and a route change that changes the principal is shown,
   never silent. The product bar: one fleet list, each machine wearing
   that badge, "as you" preferred wherever the target knows you, the
   delegation fallback visible, and a warning reserved for one case —
   reaching an **integrated** machine indirectly. If a surface requires
   the user to understand more than the badge, the surface is wrong.
3. **Attribution is the tracked mechanism.** The delegation lane's
   honest gap is that the target cannot name the human behind the
   intermediary. One primitive closes it and two other gaps at once:
   the browser (or requesting daemon) proving its identity key over a
   relayed, channel-bound exchange — giving peer-routed connections an
   *attributed-to* identity beside their *admitted-under* profile,
   giving the pairing doorbell a verifiable caller ID (the prerequisite
   named below), and unlocking cross-owner tier comparison. Displays
   already prototype the split locally: viewing rides the peer grant,
   input requires a user-granted authority.

## Where fleet metadata rides

Fleet facts have three possible carriers, and each datum lands where its
provenance and audience allow — not where plumbing is cheapest:

- **The public agent card** (`/.well-known/agent-card.json` — unauthenticated,
  CORS-open) carries operational facts a stranger legitimately needs
  *before* any trust exists: transports, capabilities, auth requirements,
  the advertised rendezvous base — and connection hints like ICE servers,
  should the hosted path ever need browser-side TURN (a parked seed with
  no consumer today). The card is daemon-asserted and unauthenticated by
  nature; nothing on it may function as evidence.
- **The signed fleet record** (browser-signed payload, v4) carries the
  owner's account-scoped view: labels, daemon URLs (PRF-encrypted), the
  rendezvous base — and the daemon's **trust tier**. The record is
  verifiable by the owner's own devices and readable without the daemon
  being up, which is exactly what tier chips on fleet cards need.
- **The daemon's authorized payloads** (the dashboard targets response,
  overview) carry daemon truth to sessions the daemon already admitted —
  the seam through which the tier reaches the browser to be folded into
  the record.

Two deliberate absences, so their reasons don't get re-litigated:

- **The tier is not on the public card.** An unauthenticated "integrated"
  label is a target beacon — it tells an attacker which box is worth the
  effort — and as a self-assertion it cannot serve the upward-grant
  guard as evidence anyway.
- **The doorbell's tier claim rides only inside the verified caller-ID.**
  The authenticated daemon-identity linkage that cross-owner comparison
  waited for now exists: a requester states its tier inside the caller-ID
  transcript (the v2 doorbell line), so the claim is pinned to a proven
  daemon key before it is ever stored or shown on the approval card. The
  absence that remains is deliberate: legacy and unverified callers show
  no tier, because a bare "I'm disposable" is an assertion dressed as
  evidence — and even the signed claim is only the requester's word about
  itself, evidence of who says it, not of its truth.
