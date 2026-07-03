# Credential Custody: the Vault and Leases

> Status: **proposed design, awaiting sign-off**. Nothing below is built.
> This chapter follows the trust-architecture convention: spec first, open
> decisions listed at the end, code only after sign-off. The access-control
> counterpart (who may reach a daemon at all) is
> [Trust Architecture](./trust-architecture.md); this chapter is about the
> *other* secrets — the model-provider credentials a daemon spends.

## The problem

Every Intendant daemon today reads its provider credentials from a plain
`.env` file (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`) or,
for the external agents, from their own on-disk auth stores (Codex:
`auth.json` under `CODEX_HOME`; Claude Code: its credentials file or the
macOS keychain). Consequences:

- Credentials live **at rest, in plaintext, forever** on every machine
  that runs a daemon — in disk images, VPS snapshots, backups, and
  whatever a future compromise of an idle box turns up.
- Standing up a new daemon means **copying secrets to it** — the worst
  step of an otherwise clean bootstrap (claim phrase + key-verified
  tunnel), and the step that keeps casual "spin up a box for the
  afternoon" out of reach.
- The user's *subscription* identities (ChatGPT plan auth for Codex,
  Claude plan auth for Claude Code — both now permitted for programmatic
  use under their current terms) are duplicated onto every machine, with
  no central place to see or withdraw them.

Meanwhile the browser presence client already demonstrates the other
model: voice API keys live in browser `localStorage` and calls go
straight from the client to the provider. The daemon never holds them.
That precedent generalizes — but not naively, because agentic traffic is
not voice traffic. The design below decomposes the problem into three
independent decisions: **custody** (where credentials live), **authority
transport** (how a daemon gets to use them), and **egress** (whose
network path carries model calls).

## Custody: the vault

The vault is the user's credential store, owned by their devices, opaque
to every server.

**Contents (v1 tenants).** Provider API keys (Anthropic, OpenAI, Gemini,
plus voice keys migrating in from today's per-origin `localStorage`), and
subscription OAuth credential sets for the external agents (Codex,
Claude Code). Each entry carries a kind, a label, provider metadata, and
optional per-daemon scoping rules (below).

**Keying.** A random 256-bit vault master key `K` encrypts the vault
body (AES-GCM). `K` itself is never stored — it is wrapped into one
**envelope per enrolled unlocker**:

- one envelope per enrolled **passkey**, wrapping key derived from that
  passkey's WebAuthn PRF output (HKDF, salt `intendant-vault-v1` — a
  domain separate from the fleet-sync derivation, so the two features
  never share key material);
- optionally one envelope for a **recovery phrase** (BIP39 12-word, the
  claim-phrase plumbing reused), generated client-side, shown once.

Losing a passkey therefore loses one envelope, not the vault: any
surviving unlocker recovers `K`, and enrolling a new device is adding an
envelope (one small re-wrap), not re-encrypting anything. This dissolves
the "lost passkey = lost vault" objection that parked the vault idea.

**Sync.** The encrypted vault blob syncs through the rendezvous exactly
like fleet records and the revocation bulletin board: blind, signed by
the browser identity key, size-capped, with **rollback protection** via a
signed monotonic revision counter (the ORL `seq` trick). The trust-ledger
entry is the usual one: a malicious store can withhold or serve a stale
revision — detectably, once any device has seen a newer one — and
nothing else. Local copies in origin storage keep the vault usable when
the rendezvous is down.

**Where it unseals.** Only in the browser, only in memory, only behind a
passkey gesture. Hosted-origin pages may unseal (that is the point of
multi-device custody), consistent with the trust rule: what a
hosted-origin page handles is bounded by what that page is for, and a
credential lease is already scoped, time-boxed, and revocable (below).
Users who refuse hosted-origin unsealing browse from an anchor origin —
both escape hatches stay first-class.

**Reserved, not v1:** an asymmetric sealing half (X25519 keypair derived
from the same PRF secret, public key published) that would let *daemons*
seal secrets into the vault one-way — the org-root-key-backup tenant.
The format reserves an `envelopes[].kind = "sealed"` variant for it.

## Authority transport: credential leases

A daemon never stores credentials; it **borrows** them.

When a browser session opens over the E2E-verified dashboard tunnel (the
binding the browser already verifies, the client key the daemon already
verifies), the browser unseals the needed entries and grants the daemon a
**lease**: the credential material, delivered over the tunnel, held by
the controller **in memory only**, tagged with an expiry.

**Frames** (dashboard-control, mirroring existing RPC conventions):

| Frame | Direction | Meaning |
|---|---|---|
| `credential_lease_grant` | browser → daemon | credential material + lease id + TTL + scope |
| `credential_lease_renew` | browser → daemon | extend a lease (sent automatically while connected) |
| `credential_lease_revoke` | browser → daemon | kill a lease now; daemon wipes the material |
| `credential_lease_status` | daemon → browser | active leases, expiries, usage counters (for the UI and the audit trail) |

Leases ride the same per-frame IAM checks as every other tunnel
operation; granting requires a session whose principal holds a new
`credentials.manage` gate (IAM v2 catalog), so a scoped guest session
cannot fuel or drain a daemon.

**Lifetime.** Two knobs, both user-visible:

- **Connected renewal**: while any granting browser session is attached,
  leases auto-renew (e.g. every 5 minutes against a 15-minute TTL).
- **Offline lease**: how long the daemon may keep working after the last
  granting session detaches. This one knob *is* the autonomy/security
  dial: `0` means the daemon is only fueled while you watch;
  `24h`–`72h` keeps overnight agent runs alive with bounded exposure.
  Per-daemon, defaulting per the sign-off decision below.

Expiry and revocation both end the same way: the controller drops the
material (zeroized where the type allows), model calls start failing
with a distinct "lease expired — reconnect a fueling session" error, and
the presence layer can push an E2E-encrypted notification (the Web Push
lane) telling the user which daemon went dry.

**The OAuth split (Codex, Claude Code).** Subscription OAuth is *better*
suited to leasing than raw keys, because the protocol already separates
durable from ephemeral authority:

- **Access-token lease (default):** the browser keeps the **refresh
  token** in the vault and never leases it. It performs token refresh
  itself and leases only short-lived **access tokens** over the tunnel.
  The daemon's maximum authority horizon is the provider's own access
  TTL (typically ≤1h) past the offline-lease window, no matter what an
  attacker does.
- **Full-credential lease (opt-in per daemon):** for long unattended
  autonomy, the refresh token itself is leased with a TTL we enforce.
  Honest note in the UI: during that window the daemon holds durable
  authority; revocation then depends on our lease discipline (and, worst
  case, the provider's session-revocation page).

**External-agent materialization (a documented weakening).** Codex and
Claude Code are child processes that read credentials from files, not
from process memory we control. A lease for them therefore materializes
a **session-scoped auth file** (0600, inside the session directory, e.g.
a synthesized `CODEX_HOME/auth.json` — the injection point already
exists) that is deleted on lease expiry, revocation, and daemon
shutdown. During an active lease those bytes are on disk; the ledger
says so plainly. Mitigations: the file exists only while leased, the
directory is excluded from the rewind/snapshot machinery, and crash
recovery deletes stale materializations at startup. On macOS, Claude
Code's keychain path is preferred over a file where it works.

**Fallback.** `.env` keeps working untouched (`custody = "local"`, the
implicit default), so nothing breaks for existing daemons and CI. A
daemon with no local keys and no lease reports "unfueled" in the
dashboard rather than erroring opaquely — the same graceful state the
no-API-key path shows today.

## Egress: whose network path

Voice went client-direct because voice is client-shaped: realtime media,
browser-supported provider endpoints, useless without the user present.
Agentic traffic is daemon-shaped: it must run at 3am, survive the
laptop sleeping, and fan out to sub-agent fleets. Routing completions
through a browser tab would make the user's phone a single point of
failure for their server farm — and it isn't even uniformly possible:

| Provider | Browser-direct calls | Notes |
|---|---|---|
| Anthropic | Yes (opt-in CORS header) | `anthropic-dangerous-direct-browser-access` |
| Gemini | Yes | the voice client already does it |
| OpenAI | Generally no | completions API refuses browser CORS |
| Codex / Claude Code (subscription) | No | they are local child processes by nature |

So: **leases are the default egress-preserving mechanism** (daemon calls
providers directly, as today, with borrowed credentials), and
**client egress is an optional per-provider mode** — worthwhile for the
maximally cautious, and as a zero-lease way to drive a brand-new daemon
before deciding to fuel it at all. In client-egress mode the daemon
sends prompt payloads to the browser over the tunnel, the browser calls
the provider, and streams results back; the credential never leaves the
browser. The mode advertises itself per-session so the UI can show which
path is live.

## What this honestly buys (threat tiers)

| Scenario | Today (`.env`) | With leases |
|---|---|---|
| Stolen disk / VPS snapshot / backup leak / idle-box compromise | full credential loss | **nothing to steal** |
| Runtime compromise, no active lease | full credential loss | nothing to steal |
| Runtime compromise during a lease | full credential loss, unbounded | capability abuse **bounded by TTL + offline window**, per-daemon scoped credential, browser-witnessed lease log, revocable from any of the user's devices |
| Malicious rendezvous | n/a | sees only the encrypted vault blob; can withhold or serve stale (detectable), cannot read or forge |

The middle row is most real-world credential leakage; the design wins it
outright. The third row is the honest limit of *any* design in which the
daemon composes prompts and consumes outputs — client egress does not
beat it either (a runtime-compromised daemon spends tokens through
whatever path exists while connected); what leases add there is bounded
time, bounded blast radius, and an audit trail the daemon cannot forge.

## The bootstrap this unlocks

With custody and leases in place, standing up a new daemon copies no
secrets, installs nothing on the user's device, and takes about ninety
seconds from a phone:

1. **Install**: `curl -fsSL https://intendant.dev/install.sh | sh -s --
   --owner <client-key-fingerprint>` on the fresh box. The fingerprint
   is public (shown in the Access drawer); the daemon boots with an
   owner grant pinned to that browser key — authority minted locally, as
   always. Nothing sensitive appears in the command or on the wire.
2. **Claim**: the daemon prints its claim phrase; the user claims it in
   the browser they are already holding (existing flow).
3. **Fuel**: the first dashboard session opens over the verified tunnel
   and the browser grants leases from the vault, per that daemon's
   scoping rules (e.g. "new daemons get the scoped work key, never the
   personal Anthropic key").

Step 1's `--owner` bootstrap is the only new trust mechanism, and it is
key-first in the existing spirit: ownership asserted by public key,
enforced by the daemon's own IAM from first boot.

## Rollout

1. Vault v1: format, envelopes (PRF + recovery phrase), blind signed
   sync with revision counter, Advanced-drawer UI (enroll, view entries,
   recovery-phrase ceremony). Voice keys migrate in.
2. Lease frames + controller-side memory custody + `credentials.manage`
   gate + provider plumbing for the three native API keys. `.env`
   fallback and "unfueled" state.
3. OAuth leases: access-token mode for Codex and Claude Code
   (browser-side refresh), session-scoped materialization with cleanup;
   full-credential opt-in.
4. Offline-lease knob, lease UI (per-daemon status, revocation, audit),
   dry-daemon push notification.
5. Client-egress mode for Anthropic/Gemini; per-session path indicator.
6. `install.sh --owner` bootstrap.

## Open questions for sign-off

1. **Offline-lease default**: `0` (fuel only while connected — maximum
   custody, breaks overnight autonomy) or `24h`/`72h` (agents keep
   working, bounded exposure)? Recommendation: **24h**, per-daemon
   adjustable, surfaced at fueling time.
2. **Full-credential OAuth lease**: allowed at all in v1, or
   access-token-only until demand proves out? Recommendation: build the
   plumbing, ship **off by default** behind an explicit per-daemon
   toggle with the honest warning.
3. **Recovery phrase**: mandatory at vault creation (no vault without a
   second unlocker) or optional-with-nagging? Recommendation:
   **mandatory** — a single-envelope vault is a support incident waiting
   to happen, and the ceremony is one screen.
4. **Scoping model**: per-daemon allow-lists on vault entries in v1, or
   defer to v2 and lease everything to every owned daemon?
   Recommendation: v1 ships a single default rule ("lease to any daemon
   I own, ask per new daemon") with per-entry overrides deferred.
