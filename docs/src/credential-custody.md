# Credential Custody: the Vault and Leases

> Status: **the vault backends, lease RPCs, and client-egress machinery are
> implemented; the default product does not yet expose a Connect account-vault
> client or bridge that backend to a trusted direct/native daemon session.**
> Hosted Connect is fixed at `role:none` and deliberately does not serve the
> daemon dashboard, its vault client, or `vault-kernel.js`. Connect can store
> opaque account-vault envelopes through its API, but the shipped directory UI
> cannot create or unseal them. A direct daemon-origin vault can use an
> authorized direct channel. A future trusted native/direct bridge is required
> to move or spend Connect-account vault entries. The four sign-off decisions
> were resolved as recommended:
> offline-lease default **24h**; full-credential OAuth leases **built but
> off by default in the browser UX**; recovery phrase **mandatory** at
> vault creation;
> scoping ships as the **single default rule** with per-entry overrides
> deferred. The v1 deviation (OAuth fueling = full-credential opt-in
> only) is resolved: access-token leases (browser-side token refresh,
> explicit `mode: "access_token"`) are now the browser OAuth default.
> Raw dashboard-control callers must send that mode explicitly; omitted
> OAuth mode remains the legacy full-credential grant. Reach caveat:
> OpenAI's token endpoint serves any browser origin, so Codex works
> out of the box; Anthropic's origin-allowlists browsers away, so Claude
> Code still needs the full-credential opt-in until that changes.
> Coverage: `scripts/validate-vault.cjs` exercises vault custody;
> `scripts/validate-credential-leases.cjs` and
> `scripts/validate-client-egress.cjs` pin the default hosted boundary
> (route-only claim, immutable refusal even with an adversarial grant, no
> delivery). Component-level RPC and direct-origin tests cover the underlying
> lease/egress mechanisms. The
> access-control counterpart (who may reach a daemon at all) is
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
  step of an otherwise clean bootstrap (one-time route link, trusted owner
  enrollment, and a key-verified tunnel), and the step that keeps casual
  "spin up a box for the
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

Entries may also carry an **unseal policy** (`unseal_policy:
"trusted"`; absent = anywhere): a trusted-only entry refuses use from
hosted-origin vault code — no reveal, no lease fueling, no egress relay, no
voice mirror — while still syncing inside the encrypted body. The current
Connect directory ships no vault client, so this matters today on the direct
dashboard and remains a constraint on any future hosted client. This is
client-side self-enforcement (a guard against mistakes and casual
exfiltration, not against a malicious bundle — see
[Trust Tiers](./trust-tiers.md)). On a **direct** dashboard backed by
the daemon store (below), trusted-only entries work normally — that is
the tier the policy reserves them for. The policy field is invisible
to every store like any other entry field.

**Storage backends.** The sealed blob has two possible homes, both
blind to its contents, and the dashboard says which one backs it (the
store chip on the vault card):

- **Account store** (backend implemented, client not shipped): the Connect
  service keeps one opaque blob per account. The API retains the original
  cross-device storage design, but the default Connect directory serves no
  dashboard vault client or crypto worker, so users cannot create, unseal, or
  spend this store through the shipped hosted UI.
- **Daemon store** (direct dashboards): the daemon itself keeps the
  blob at `~/.intendant/vault-blob.json` (0600), served over the
  verified control channel (`api_daemon_vault_fetch` /
  `api_daemon_vault_publish`, `credentials.manage`-gated). No Connect
  service is in the loop: a direct dashboard creates, unseals, and
  fuels from a vault that never leaves machines the owner controls.
  The daemon-side rules replicate the hosted store's exactly
  (`vault_store.rs` twin-tested against `bin/connect/fleet.rs`): shape
  validation, the monotonic-revision rollback ratchet, same-revision
  divergence conflicts, and the MAC-presence ratchet.

The two stores are **independent** — each keeps its own revision ratchet, and
nothing syncs implicitly between them. The daemon publish RPC and dashboard
transfer code exist, but the default Connect directory supplies neither the
source-vault client nor a channel to that RPC; therefore there is currently no
shipped Connect-account-vault → direct-daemon-vault transfer path. A
trusted native/direct bridge must be built before that action can be
advertised as working. A direct dashboard can create and use its own
daemon-store vault today; moving an account vault into it is manual and
out of band.

**Keying.** A random 256-bit vault master key `K` encrypts the vault
body (AES-GCM). `K` itself is never stored — it is wrapped into one
**envelope per enrolled unlocker**:

- one envelope per enrolled **passkey**, wrapping key derived from that
  passkey's WebAuthn PRF output (HKDF, salt `intendant-vault-v1` — a
  domain separate from the fleet-sync derivation, so the two features
  never share key material);
- optionally one envelope for a **recovery phrase** (BIP39 12-word, reusing
  the mnemonic/word-grid plumbing rather than Connect claim semantics),
  generated client-side, shown once.

Losing a passkey therefore loses one envelope, not the vault: any
surviving unlocker recovers `K`, and enrolling a new device is adding an
envelope (one small re-wrap), not re-encrypting anything. This dissolves
the "lost passkey = lost vault" objection that parked the vault idea.

**Sync.** The encrypted vault blob syncs through the rendezvous blind
and size-capped, and every blob is **authenticated end-to-end**: the
revision number is bound into the body ciphertext (AES-GCM AAD), and the
whole blob — version, revision, envelope set, body — carries an
HMAC-SHA-256 under a key HKDF-derived from the vault master key
(`vault-mac-v1`). The store never holds the master key, so it can
neither mint a MAC nor splice parts of old blobs together (e.g. re-attach
an envelope set that still contains a revoked passkey); it also enforces
a presence ratchet — once an account's stored vault carries a MAC, a
MAC-less replacement is refused — and clients keep the same ratchet
per-device. (An earlier draft called for browser-identity-key signatures
here; a store can strip or re-sign those with a key of its own, whereas
it can never produce a master-key MAC, so the MAC is what shipped.)
**Rollback protection** is the monotonic revision counter (the ORL `seq`
trick) plus each device's local high-water mark. The trust-ledger entry
is the usual one: a malicious store can withhold or serve a stale
revision — detectably, once any device has seen a newer one — and
nothing else. Local copies in origin storage keep the vault usable when
the rendezvous is down.

**Where it unseals.** Only in a browser worker, only in memory, and only behind
an unlock gesture. The shipped direct daemon-origin dashboard can unseal its
separate daemon-store vault. The default Connect directory does not serve the
vault client or worker, so its account-store envelopes do not currently unseal
in the product UI. Any future hosted client would still run under
Connect-controlled JavaScript and would need to state that malicious hosted
code could misuse entries while unlocked. Bridging the custody domains remains
future work.

### The crypto kernel

Within the browser, the key material lives one layer deeper than the
page: all key-touching crypto — master-key generation and (un)wrapping,
KEK derivation from PRF secrets and the phrase, body AES-GCM, the blob
MAC, the deposit-lane ECIES — runs inside **`static/vault-kernel.js`**,
a small dependency-free dedicated Worker driven over a postMessage RPC
(`unlock-phrase`, `unlock-prf`, `create`, `encrypt-body`, `verify-mac`,
`open-deposit`, …). The master key, the KEKs, and the MAC key exist only
in the worker and are wiped on `lock`; the page holds an opaque unlock
token, and `32-vault-custody.js` keeps only policy and state — envelope
choice, the MAC-downgrade ratchet, storage, rendering.

The point is *pinned instantiation*: the daemon dashboard's app.html assembler
hashes the kernel at build time and injects the sha256 as `VAULT_KERNEL_SHA256`
(a placeholder in the fragment source, substituted at assembly — see
`crates/app-html-assembler`); at first vault use the page fetches
`/vault-kernel.js` (served only as an explicit embedded daemon-gateway asset),
hashes the bytes, and instantiates a
worker from them (blob URL) **only on a hash match** — a mismatch is a
loud hard-refusal with no inline-crypto fallback. The Connect binary's static
cutoff intentionally returns 404 for `/vault-kernel.js` (and for the daemon
SPA/WASM tree); `--static-root` cannot re-enable it. Connect's transparency
manifest therefore covers only the Connect pages and explicitly embedded
assets it actually serves, not this daemon-only worker. A tampered daemon
dashboard that once could exfiltrate the master key at unlock now has to tamper
with one small, manifest-committed, humanly auditable file instead of
hiding in ~3.4 MB of dashboard.

Honest limits: the kernel kills silent **key** exfiltration and offline
future-decryption, not live abuse — a malicious page can still call the
kernel's RPC while unlocked (read entries, encrypt attacker-chosen
bodies), bounded by the page's own transparency story, not the kernel.
WebAuthn must run on the page, so the PRF secret transits page memory
inbound (and stays in sessionStorage for reload-unlock, as before); the
decrypted body plaintext — entries, settings, the deposit lane's private
JWK, which must ride the sealed blob — flows to the page because the UI
renders it. `scripts/vault-kernel-exercise.cjs` drives the kernel's RPC
end to end under node's WebCrypto; the daemon-side parity test
(`web_gateway/static_assets.rs`) fails the suite when the kernel is
edited without regenerating the app.html pin.

**The write-only deposit lane** (`intendant vault deposit <label>`) is
the asymmetric sealing half, shipped: a P-256 deposit keypair lives
*inside* the sealed body (`settings.deposit_lane`, so it reaches every
unlocking device but exists only as ciphertext at rest), and an unlocked
dashboard publishes its public half to the daemon
(`~/.intendant/vault-deposit-key.pub.json`). The CLI reads a secret from
**stdin** — piped, so the plaintext never rides a web UI, a terminal
echo, or this daemon's disk — seals it ECIES-style to that public key
(ephemeral P-256 → HKDF-SHA256 → AES-256-GCM, the label bound into both
the KDF info and the AEAD AAD), and queues one ciphertext record per
deposit under `~/.intendant/vault-deposits.d/`. The next unlocked
dashboard on this daemon folds queued deposits into the vault as
ordinary entries and deletes them **only after** the re-wrapped blob has
published; a deposit sealed to a superseded key stays queued and visible
(`intendant vault status`) rather than being consumed blind. Honest
limits: the depositing CLI trusts the machine it runs on — a malicious
daemon could swap the deposit key and capture *future* deposits (it
still can never read the vault), and deposits are write-only by
construction: nothing on the CLI side can read an entry back out. The
implementation pair is `vault_deposits.rs` (seal) and the crypto
kernel's `open-deposit` op (driven by `32-vault-custody.js`);
`scripts/vault-deposit-parity.cjs` cross-checks the two against real
WebCrypto.

**Still reserved, not v1:** deriving the deposit keypair from the PRF
secret itself (today it is generated randomly and rides the blob), and
the org-root-key-backup tenant with its `envelopes[].kind = "sealed"`
variant.

## Authority transport: credential leases

In the lease path, a daemon **borrows** credentials instead of configuring
them durably. This is optional: `.env` remains supported, and full-credential
OAuth leases temporarily materialize private auth files as documented below.

When an **authorized trusted direct/native browser session** opens over an
E2E-verified dashboard channel (the binding the browser verifies and the
loopback or mTLS principal the daemon authenticates), its daemon-store vault
can unseal the needed entries and grant the daemon a
**lease**: the credential material, delivered over the tunnel, held by
the controller **in memory only**, tagged with an expiry.

The Connect-origin account vault cannot use this transport in the default
build: the service refuses offer/ICE/close and the daemon independently stamps
hosted provenance `role:none`. No cross-origin handoff to an already-open
direct dashboard exists yet.

**Dashboard-control request methods** (mirroring existing RPC conventions;
raw frame names are reserved for the `egress_*` relay path):

| Method | Request | Response result |
|---|---|---|
| `api_credential_lease_grant` | browser → daemon request with `kind`, `label`, credential `material` (or legacy `secret`), optional OAuth `mode` (`access_token` / `full_credential`), `ttl_ms`, and `offline_ms` | daemon-generated `lease_id`, `kind`, `expires_at_unix_ms`, `replaced` |
| `api_credential_lease_renew` | browser → daemon request with `lease_id` (or legacy `leaseId`) | `lease_id`, new `expires_at_unix_ms` |
| `api_credential_lease_revoke` | browser → daemon request with optional `lease_id` / `leaseId` / `kind`; omitted revokes every lease on the daemon | `revoked` count |
| `api_credential_lease_status` | browser → daemon request with no params | active `leases` (`lease_id`, `kind`, `label`, `mode`, grant/renew/expiry timestamps, `ttl_ms`, `offline_ms`, `use_count`), active `egress` relays, and `expired_note` |
| `api_credential_custody_trail` | browser → daemon request with no params | recent custody events (`at_unix_ms`, `event`, `kind`, `label`, `actor`, `origin`, `detail`) from the daemon's own record — lease grants/expiries/revocations, relay changes, restart resets; metadata only, never material. `origin` stamps the session's origin class on ceremonies (`hosted` / `direct` / `local` / `peer`; empty on sessionless events and pre-field records). Kept at `~/.intendant/custody-audit.jsonl` (0600, bounded), rendered in Access → Advanced → Custody trail |
| `api_daemon_vault_fetch` | browser → daemon request with no params | the daemon-stored sealed blob, if any: `revision`, `vault` (E2E ciphertext the daemon cannot read), `updated_unix_ms`; `revision: 0, vault: null` when empty |
| `api_daemon_vault_publish` | browser → daemon request with `revision` and the full `vault` blob | `stored` (`false` = idempotent same-revision republish); rollback, same-revision divergence, and MAC-stripping are refused with a `vault revision conflict:`-prefixed error the dashboard treats like the hosted store's HTTP 409 |

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

- **Access-token lease (the browser UX default):** the browser keeps the **refresh
  token** in the vault and never leases it. It performs token refresh
  itself against the provider's token endpoint (rotated refresh tokens
  are written back into the vault — both providers rotate) and leases
  only short-lived **access tokens** over the tunnel, as material with
  every durable field blanked and `mode: "access_token"` on the grant.
  Raw dashboard-control callers must send `mode: "access_token"` for
  this path; omitting `mode` on an OAuth grant is the compatibility path
  for a legacy full-credential lease.
  The daemon re-verifies the material is refresh-free before accepting —
  fail-closed against custodian bugs — and re-materializes on every
  re-grant; the granting tab's renewal tick re-grants freshly refreshed
  material whenever the current token nears expiry. The daemon's maximum
  authority horizon is the provider's own access TTL (typically ≤1h)
  past the last re-grant, no matter what an attacker does. Reach: this
  needs the token endpoint to answer browser CORS. OpenAI's
  (`auth.openai.com`) serves `Access-Control-Allow-Origin: *`, so
  **Codex fuels this way out of the box**; Anthropic's
  (`console.anthropic.com`) allowlists origins and refuses others, so
  **Claude Code cannot refresh in the browser today** and stays behind
  the full-credential opt-in (the UI says exactly that).
- **Full-credential lease (opt-in per daemon):** for long unattended
  autonomy beyond the provider's access-token lifetime — and for Claude
  Code, per the CORS limit above — the pasted auth-file JSON (refresh
  token included) is leased with a TTL we enforce. Honest note in the
  UI: during that window the daemon holds durable authority; revocation
  then depends on our lease discipline (and, worst case, the provider's
  session-revocation page).

**External-agent materialization (a documented weakening).** Codex and
Claude Code are child processes that read credentials from files, not
from process memory we control. A lease for them therefore materializes
a daemon-private temporary home under `~/.intendant/leased-auth`, outside
any project worktree: `codex-home/auth.json` for Codex and
`claude-home/.credentials.json` for Claude Code. The directories are
0700, the auth files are 0600, and spawns point the child process at
them with `CODEX_HOME` or `CLAUDE_CONFIG_DIR`. The materialization is
deleted on lease expiry, revocation, and daemon shutdown. During an
active lease those bytes are on disk; the ledger says so plainly.
Mitigations: the materialization root is outside worktrees and is never
seen by rewind/snapshot machinery, the file exists only while leased,
and crash recovery deletes stale materializations at startup.

**Transcript staging at cleanup.** The materialized home also holds the
agent's session transcripts (Codex `sessions/`, Claude `projects/`), and
deleting the home would erase them from message search. Cleanup therefore
first *renames* those transcript subdirectories into a credential-free
staging area under `~/.intendant/cache/message_search/staging/`
(same-volume rename — effectively instant), then deletes the home
immediately (`lease_transcript_staging.rs`). Staging is strictly
best-effort and never delays secret deletion: on any failure a marker
records the coverage gap and the deletion proceeds. The startup crash
sweep stages the same way before removing leftovers, staged entries not
drained within the search retention window are GC'd at startup, and an
`active/` registry (one file per materialized home) tells the
message-search indexer which leased homes are live right now.

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

For an authorized trusted channel, **leases are the default
egress-preserving mechanism** (daemon calls
providers directly, as today, with borrowed credentials), and
**client egress is an optional per-provider mode** — worthwhile for the
maximally cautious. In client-egress mode the daemon
sends prompt payloads to the browser over the tunnel, the browser calls
the provider, and streams results back; the credential never leaves the
browser. The mode advertises itself per-session so the UI can show which
path is live. It is not reachable from the Connect account vault today;
a trusted native/direct bridge is still required.

As shipped: a session holding `credentials.manage` registers as the
relay per kind (`api_credential_egress_register`); the daemon ships each
request auth-less (`egress_request` + 16 KiB chunks), the browser
attaches the key from the unlocked vault, enforces a fixed per-provider
host allowlist (a compromised daemon cannot turn the tab into an open
proxy), performs the fetch, and streams the body back under a 1 MiB
credit window with `egress_ack` refills. Response frames are bound to
the registering session; relays die with their session; selection order
is lease → `.env` → egress. The fueling panel carries the per-provider
toggles, the live relay chips (the path indicator), and a Test-relay
probe (`api_credential_egress_probe`).

## What this honestly buys (threat tiers)

| Scenario | Today (`.env`) | With leases |
|---|---|---|
| Stolen disk / VPS snapshot / backup leak / idle-box compromise | full credential loss | no provider secret **only when deliberately keyless and outside an active full-credential OAuth lease**; `.env` and active materialized auth homes remain disk exposure |
| Runtime compromise, no active lease | full credential loss | no leased material; locally configured `.env`/auth stores remain reachable |
| Runtime compromise during a lease | full credential loss, unbounded | capability abuse **bounded by TTL + offline window**, per-daemon scoped credential, browser-witnessed lease log, revocable from any of the user's devices |
| Malicious rendezvous | n/a | the passive store sees only ciphertext and can withhold or serve stale; malicious Connect-served code can read or misuse entries exposed after a hosted unlock, but the default build gives it no daemon delivery channel |

For a deliberately keyless daemon, leases remove durable provider material
from the first two rows outside an active full-credential OAuth lease. A daemon
configured with `.env` or another local auth store retains that exposure; the
custody machinery does not erase it. The active-lease row is the honest limit
of *any* design in which the
daemon composes prompts and consumes outputs — client egress does not
beat it either (a runtime-compromised daemon spends tokens through
whatever path exists while connected); what leases add there is bounded
time, bounded blast radius, and a daemon-local custody log for normal
operations. That JSONL log is not tamper-proof: a compromised daemon can
alter or omit its own record, so independent client/provider records remain
the stronger forensic source.

## The bootstrap this unlocks

With custody and leases in place, standing up a new daemon copies no
provider secrets. Connect discovery remains a roughly ninety-second browser
flow, while ownership deliberately requires a trusted anchor:

1. **Install**: `curl -fsSL https://intendant.dev/install.sh | sh` on the
   fresh box (every rendezvous — hosted or self-run — serves its own
   version-matched installer at `/install.sh`). On Windows the same step is
   `& ([scriptblock]::Create((irm https://intendant.dev/install.ps1)))` from
   PowerShell. On an unattended box add `--service` (`-Service`):
   `intendant service install` picks the platform's native supervisor —
   systemd **where present**, launchd on macOS, Task Scheduler on
   Windows, cron `@reboot` plus the built-in restart supervisor on
   systemd-less Linux; no init system is a dependency — so the daemon
   outlives the SSH session and restarts on failure, and the installer
   prints where the one-time claim code lands (journal or service log). The
   landing page's fold-out advisor ("Not sure which shape fits?") maps
   four questions — OS, where it runs, what fuels it, attended or not —
   onto this same command plus an honest fueling plan.
2. **Link**: the daemon prints a single-use twelve-word claim code; the user
   enters it in Connect to add discovery and route metadata to the account.
   Linking creates no IAM principal or grant and grants no access.
3. **Anchor**: establish root through `intendant access setup` from the
   machine's console/SSH session, a direct mTLS root connection, or the native
   app's mTLS bridge. The former `--owner <client-key-fingerprint>` shortcut is
   retired; alpha root establishment does not treat a bare browser-key
   fingerprint as authentication.
4. **Authorize on a trusted surface**: install an owner-approved browser mTLS
   certificate from the daemon enrollment flow or use the signed native app.
   Browser identity keys remain record/signature vocabulary and are not an
   alpha login. Hosted Connect remains discovery-only: its compiled ceiling is immutably
   `role:none`, with no opt-in or role-raising control.
5. **Fuel**: create or unlock the separate daemon-store vault from that trusted
   direct dashboard, or use existing local credential configuration. The
   Connect account vault cannot be handed across to this session in the default
   build; a trusted native/direct bridge is still unimplemented.

Claiming and custody are intentionally independent: the claim makes a machine
findable and the trusted anchor creates authority. Vault storage alone does not
create the missing cross-origin delivery bridge.

## Rollout

1. ✅ Vault v1: format, envelopes (PRF + recovery phrase), blind sync
   with revision counter + rollback high-water mark, Advanced-drawer UI
   (create ceremony, unlock, enroll, entries). Voice keys migrate in.
2. ✅ Lease RPCs + controller-side memory custody + `credentials.manage`
   gate (operator holds it; peer lane excluded) + lease-first provider
   plumbing. `.env` fallback untouched; distinct "unfueled" error.
3. ✅ OAuth materialization for Codex (`CODEX_HOME`) and Claude Code
   (`CLAUDE_CONFIG_DIR`): private temporary homes under
   `~/.intendant/leased-auth/{codex-home,claude-home}`, outside
   worktrees and snapshots, deleted on expiry, revocation, shutdown, and
   a startup recovery sweep; full-credential opt-in per daemon (OFF by
   default in the browser UX). Access-token mode shipped as the browser
   OAuth default (browser refresh + rotation write-back; daemon-verified
   refresh-free material; raw RPC callers must send
   `mode: "access_token"` explicitly; Codex live, Claude Code pending
   provider CORS).
4. ✅ Offline-lease knob, fueling panel (per-daemon status, revocation,
   usage audit fields), dry-daemon Web Push. Custody trail shipped: the
   daemon records every grant/expiry/revocation/relay change (+ restart
   resets) locally and serves them over `api_credential_custody_trail`.
5. ✅ Client-egress mechanism for Anthropic/Gemini on an authorized control
   channel (host-allowlisted browser relay, credit-windowed streaming, probe);
   **not reachable from the Connect account vault until a trusted bridge ships**.
6. ✅ Hosted installers leave route linking authority-free by default;
   legacy `--owner <browser-key>` bootstrap is retired/rejected rather than
   shipping an incomplete certless key-auth protocol.
7. ⏳ Trusted native/direct bridge for transferring or spending a
   Connect-account vault in a daemon-origin session.

## V1 decisions

1. **Offline leases default to 24 hours.** The fueling UI exposes the
   per-daemon choice (`while connected`, 1h, 24h, or 72h); grants made after a
   change use the new value.
2. **Full-credential OAuth is implemented but opt-in.** Browser fueling uses
   refresh-free access-token leases by default. An explicit per-daemon toggle,
   off by default and accompanied by the durable-authority warning, enables
   full auth-file leases where provider CORS or unattended duration requires
   them.
3. **A recovery phrase is mandatory at vault creation.** Every new vault has a
   second unlocker rather than depending on one passkey envelope.
4. **V1 uses one default daemon-scoping rule.** "Lease to any daemon I own,
   ask per new daemon" is the shipped policy; per-entry overrides remain a v2
   item.
