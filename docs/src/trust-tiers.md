# Trust Tiers

> Status: adopted doctrine (2026-07-08). [Trust Architecture](./trust-architecture.md)
> bounds what each *component* may do to you if it turns malicious; this
> chapter is the operating model an owner applies across a fleet whose
> machines carry different stakes. Almost nothing here is new mechanism — it
> composes ceilings, grants, custody, and client choice that already exist.
> The product hooks at the end are the tracked exceptions.

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
  is honest. At one end, the hosted dashboard tab: convenient, zero-install,
  and explicitly a [degraded-trust tier](./trust-architecture.md#organizations-two-lanes),
  because the hosted origin can change what it serves at any time. At the
  other, code whose provenance you control: the signed native app, or a
  dashboard served by a daemon you own (the
  [anchor rule](./trust-architecture.md#anchor-daemons)).

The doctrine is one sentence: **match the client's provenance to the payload
of the daemon it is driving — per daemon, not per person.**

Stated per tier, and resolving what looks like a paradox:

- Driving a *disposable* daemon from a hosted tab is not a compromise you
  tolerate — it is the design working. The custody machinery (vault,
  time-boxed leases, zero-authority rendezvous, claims-grant-nothing) exists
  precisely so that the worst a poisoned hosted page can harvest from this
  tier is bounded, revocable, and logged. The hosted path is *most*
  compelling exactly where trust in the infrastructure is lowest, because
  the payload puts a hard ceiling on the loss.
- Driving an *integrated* daemon demands provenance: the native app or an
  owner-served origin. This is where "just open intendant.dev" stops being
  an acceptable answer, however encrypted the transport and however honest
  the service intends to be.

Trust machinery scales with payload, not with paranoia. A user who "doesn't
trust Intendant" and keeps every daemon disposable is served perfectly well
by the most convenient path; a user who trusts it with everything needs the
inconvenient-once path for exactly one machine.

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
2. **Origin ceilings, hardened per daemon.** The default
   [role ceilings](./trust-architecture.md#mechanisms) already cap
   hosted-provenance sessions at `role:operator` everywhere
   (`role_ceilings` in `iam.json`). That is a floor of protection sized for
   the disposable tier. Integrated daemons should harden it further —
   ceiling hosted provenance at an observer role, or refuse hosted-origin
   control outright — so that "this box cannot be driven from a hosted tab"
   is enforced where all authority already lives, not remembered by the
   owner.
3. **Separate keys, not separate networks.** The one genuinely cross-tier
   single point is the browser identity key: one browser profile enrolled
   as root on every daemon means one stolen profile owns both worlds. Keep
   the enrollment that holds root on integrated daemons in a dedicated
   browser profile or device (or in the native app's own storage); let the
   everyday profile carry the disposable tier. This costs one extra
   enrollment ceremony, once.

Two accounts — two actual fleets — buy exactly one additional property:
the rendezvous cannot see that both worlds belong to the same person.
That is metadata unlinkability, a legitimate but niche posture, and it is
opt-in paranoia rather than the recommended shape.

## Custody inverts across tiers

The [credential custody](./credential-custody.md) discipline — vault blob on
the account, nothing durable on disk, browsers minting time-boxed leases —
was built *for* boxes you do not trust. Apply it there and only there:

- **Disposable tier**: leases only. The box's disk holds no durable secret;
  destroying the box revokes nothing because there was nothing to revoke.
- **Integrated tier**: the box is already inside your trusted computing
  base — it runs the agent that reads the mail. It may simply hold its own
  credentials (OS keystore, local config), because routing them through the
  account vault adds a hosted dependency without adding safety. Where vault
  storage is still preferred (cross-device sync, sealed-at-rest), those
  entries want a stricter unseal policy than the disposable tier's — see
  hook 3.

## The client ladder

- **Disposable tier**: any hosted tab, anywhere. This is the zero-install
  promise, delivered honestly.
- **Integrated tier**: the signed native app, or a direct/owner-served
  origin. Store-signed releases are the out-of-band code anchor a bare
  browser tab cannot have — the same reason messengers with real E2E
  guarantees treat their web clients as the weak tier. The app is not the
  non-technical user's consolation prize; it is *everyone's* correct client
  for the machines that matter.

Getting a pleasant direct origin today: the fleet strip offers **↗
direct** wherever a daemon's own URL is known, and the daemon-store vault
([Credential Custody](./credential-custody.md#the-vault)) makes that tab
self-sufficient. For the warning-free padlock, **fleet certificates** do
it in one click: a rendezvous serving a delegated DNS subzone
([Self-Hosted Rendezvous → Fleet DNS](./self-hosted-rendezvous.md#fleet-dns-real-certificates-for-daemons))
gives each daemon a real name, and the Connect card's *Get a real
certificate* button publishes the daemon's addresses (LAN included — no
port forwarding needed) and mints a Let's Encrypt certificate via DNS-01,
renewed automatically, private keys never leaving the machine. Without
fleet DNS, the manual routes remain: a hostname you own with a DNS-01
cert, `tailscale cert` on a tailnet, or the browser's one-time exception
plus the enrollment ceremony.

A worked example, one fleet:

| Daemon | Tier | Control origins | Custody | Peer grants |
|---|---|---|---|---|
| `home` (desktop) | integrated | native app / direct only (hosted ceiling: none) | local keystore; vault entries app-only | holds grants **on** `vps-1`, `vps-2` |
| `vps-1`, `vps-2` (rented) | disposable | any hosted tab | vault leases only | none; controlled **by** `home` |

The owner claims all three into one account, sees them in one dashboard,
and the tier boundary is carried entirely by ceilings, grant direction, and
which client they open for which box.

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
2. **Trust with mandatory evidence — the fleet name.**
   `https://d-<hash>.fleet.intendant.dev:8765` is daemon-served code on a
   rendezvous-named route: the zone operator — or anything else that can
   answer DNS for the name and convince a CA — could point your daemon's
   name at a box of its choosing. What this rung guarantees is not that
   the swap cannot happen but that it **cannot happen quietly**: serving
   code at the `https` origin requires a certificate for the name, the
   attack must be live at the moment you connect (nothing is exposed
   passively or retroactively), and every issued certificate lands in
   public Certificate Transparency logs — where the daemon's own CT
   tripwire watches for serials it never requested and raises **CT
   ALERT** on the Connect card. Betrayal is possible, targeted, and loud.
3. **Trusted but bounded — the hosted tab.** The rendezvous origin serves
   the code itself, so betrayal is a silently different bundle to one
   visitor, once, with no artifact anywhere. No evidence machinery can
   apply; what bounds the damage is authority, not detection — role
   ceilings cap hosted-provenance sessions, trusted-only vault entries
   refuse hosted unseal, and custody keeps durable secrets off the tier
   that would leak them.

The product states the rung wherever an owner makes a trust decision:
device-enrollment approvals carry a daemon-computed route chip (*via
direct origin* / *via fleet name* / *via hosted route*), and owners who
want rung-two sessions capped like rung-three ones add the daemon's fleet
origin to `hosted_origins` (the ceiling test matches exact origins, so it
is the daemon's own fleet URL that goes in the list, not the bare zone).
Device enrollment (`intendant access serve-certs`) rides the same ladder:
with a live fleet certificate it leads with the warning-free fleet URL and
skips the fingerprint transcription (a rung-two bootstrap), while the
fingerprint ceremony against a direct address remains the rung-one path —
shortened to an 80-bit prefix, since pre-grinding a certificate that
shares 20 hex characters is out of reach.

One consequence is easy to miss: for any *browser* client, first contact
re-asks itself on every page load — the tab re-fetches its code each
visit, so a rung's guarantee is only as durable as its serving origin.
Enrolled identity keys do not change this: browser storage is
origin-scoped, so a key enrolled at a fleet name is wieldable by whatever
code that name serves. Rungs one and two therefore fully converge only
when the client stops being re-served — the signed native app and code
transparency over serving origins (both tracked) are the ladder's missing
top, not polish.

### Still blurry, on purpose

Named honestly rather than smoothed over — each is either tracked or a
stated non-goal:

- **The time axis (TOFU).** Everything above grades *first* contact;
  later visits inherit pinned material (enrolled keys, remembered
  certificates) but re-inherit the code channel every load. The signed
  app collapses code trust to install-and-update moments; until it
  ships, rung two re-runs per visit.
- **The update channel.** A signed app trusts its updater. Code
  transparency — served-artifact hashes in a public log, monitors,
  verifiers — is the evidence leg for that too. (Tracked.)
- **Lookalike names.** `d-<hash>` labels are deliberately opaque, which
  also means humans cannot eyeball them; a phished lookalike with its own
  valid certificate raises no CT alarm on *your* name, because it is not
  your name. The mitigation is navigational: reach fleet names from the
  fleet strip, bookmarks, or the app — never by retyping.
- **The browser itself.** Every rung assumes the browser and OS are
  honest; an extension with page access reads all tiers alike. Outside
  Intendant's reach — stated so the ladder is not mistaken for covering
  it.
- **Hosted-passkey coupling.** Unsealing the vault with a passkey inside
  a hosted tab is rung-three code wielding rung-one credentials. The
  write-only CLI vault lane and the pinned crypto kernel (both tracked)
  are the untangle.

## Product hooks

Four pieces of mechanism let the product carry this doctrine instead of
the owner's memory. All four are **shipped**:

1. **Tier labels + upward-grant guard.** Each daemon carries its tier in
   local IAM (`tier` in `iam.json`; `POST /api/access/tier`,
   audit-logged, manage-gated), chosen on the **Trust tier card** at the
   top of Access → Overview. The guard is advisory and local-tier-driven:
   on an integrated machine, the peer pairing-approval card warns that
   approving grants a peer authority *here* (the upward-grant alarm), and
   hosted-route device enrollments get an integrated-tier warning chip
   beside the existing hosted-route one. *Deliberately deferred:*
   cross-daemon tier visibility (a tier field on fleet records or agent
   cards, so the granting side can compare both ends' tiers) — that needs
   a metadata-carrier decision (browser-signed fleet payload v4 vs. the
   public agent card) and is not required for the local alarm.
2. **Per-daemon hosted-ceiling knob.** The same card carries "Hosted tabs
   may: Operate / View only / Nothing" — one control writing both
   hosted-provenance `role_ceilings` bindings
   (`POST /api/access/hosted-ceiling`), with `role:none` (a
   zero-permission, ceiling-only builtin) as the honest refuse-entirely
   position. Choosing Integrated while hosted tabs can still operate
   surfaces a one-click "Cap to View only" nudge. Raising ceilings,
   per-binding divergence, and disabling remain deliberate `iam.json`
   edits.
3. **Per-entry vault unseal policy.** Vault entries accept
   `unseal_policy: "trusted"` (add form + per-entry toggle): trusted-only
   entries refuse reveal, lease fueling, egress relay, and the voice
   mirror from hosted tabs, and the custody trail stamps every
   lease/relay ceremony with the session's origin class
   (`hosted`/`direct`/`local`/`peer`). Honest limits, stated in the UI
   too: this is client-side self-enforcement — protection against
   mistakes and casual exfiltration, not against a malicious bundle.
   With the **local vault** shipped (the daemon-store backend in
   [Credential Custody](./credential-custody.md#the-vault)), trusted-only
   entries do real work: sealed against hosted tabs, fully usable from a
   direct dashboard backed by the daemon's own store.
4. **First-contact route, surfaced and watched.** Enrollment approvals
   carry a route chip computed daemon-side (`iam::origin_route_class`:
   hosted / fleet / direct / unknown — route provenance for approval
   decisions, distinct from `session_origin_class`, the custody-trail
   code-provenance class), with honest per-rung copy and an
   integrated-tier warning on any network route. The **CT tripwire**
   backs rung two's evidence claim: `fleet_cert` records the serial of
   every certificate it obtains (before install, so a crash cannot make
   an own certificate look foreign), polls crt.sh for the daemon's fleet
   name on each renewal tick, and flips the Connect card to **CT ALERT**
   on any serial the daemon never requested. Advisory and fail-open by
   design: a crt.sh outage stamps `ct_last_error` rather than blocking
   renewal.
