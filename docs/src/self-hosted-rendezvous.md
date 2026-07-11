# Self-Hosted Rendezvous

`intendant-connect` — the hosted rendezvous behind `connect.intendant.dev`
— is an open, first-class deployable, not a chokepoint. The trust
architecture is built so this is safe: the service holds **zero
authority**. It introduces browsers to daemons, relays ciphertext, and
stores client-signed fleet metadata; dashboard tunnels verify a
daemon-signed binding end-to-end, browser identity keys authenticate
end-to-end, org grant documents are verified by the target daemon against
its own trusted keys, and every session's authority comes from the target
daemon's local IAM. A malicious rendezvous can deny service; it cannot
impersonate a daemon, mint authority, or read tunnel contents. See
[Trust Architecture](./trust-architecture.md).

## Build and run

```bash
cargo build --release --bin intendant-connect
./target/release/intendant-connect \
  --listen 127.0.0.1:9876 \
  --origin https://connect.example.com \
  --rp-id example.com \
  --static-root /opt/intendant/static \
  --data-file /var/lib/intendant-connect/state.json \
  --daemon-token <random-shared-token>
```

| Flag | Env | Meaning |
|---|---|---|
| `--listen` | `INTENDANT_CONNECT_LISTEN` | Bind address (put a TLS reverse proxy in front) |
| `--origin` | `INTENDANT_CONNECT_ORIGIN` | Public origin browsers use; also the WebAuthn origin |
| `--rp-id` | `INTENDANT_CONNECT_RP_ID` | WebAuthn relying-party id (a registrable suffix of the origin's host) |
| `--static-root` | `INTENDANT_CONNECT_STATIC_ROOT` | The repo `static/` directory (serves `/app` and `/connect`) |
| `--data-file` | `INTENDANT_CONNECT_DATA_FILE` | JSON state (accounts, claims, fleet records) |
| `--daemon-token` | `INTENDANT_CONNECT_TOKEN` | Bearer token daemons present on the polling endpoints; also the admin-API credential |
| `--open-registration` | `INTENDANT_CONNECT_OPEN_REGISTRATION` | Let daemons register/poll without the token (rate-limited; unclaimed records expire after a day; the gate moves to claim time). The token keeps guarding the admin API. This is what makes the landing one-liner claimable by people who never saw the token |
| `--dns-zone` | `INTENDANT_CONNECT_DNS_ZONE` | Fleet DNS: the delegated subzone this service answers for authoritatively (see below). All three `--dns-*` values or none |
| `--dns-ns-name` | `INTENDANT_CONNECT_DNS_NS_NAME` | The NS host the parent zone delegates to (served in the apex SOA/NS) |
| `--dns-listen` | `INTENDANT_CONNECT_DNS_LISTEN` | UDP+TCP bind for the DNS server, e.g. `0.0.0.0:53` |

The service speaks plain HTTP; terminate TLS in front of it (nginx,
Caddy, a cloud load balancer). WebAuthn requires the public origin to be
HTTPS. A systemd unit is just the command above with
`Restart=always` and a writable state directory; the deploy script the
default instance uses (`scripts/deploy-connect-prod-alpha.sh`) is a
worked example.

A worked Caddy site block (the shape the default instance runs — the
forwarding headers are load-bearing, see
[Reachability](#reachability-for-natd-daemons)):

```caddy
connect.example.com {
	encode gzip zstd

	reverse_proxy 127.0.0.1:9876 {
		header_up Host {host}
		header_up -X-Forwarded-Host
		header_up X-Forwarded-For {remote_host}
		header_up X-Real-IP {remote_host}
		header_up X-Forwarded-Proto {scheme}
	}
}
```

## Fleet DNS: real certificates for daemons

The convenient-direct-path option ([Trust Tiers](./trust-tiers.md)):
delegate one subzone to the service and every registered daemon gets a
real name — `d-<hash>.<zone>`, an opaque sha256-derived label (these
names land in public CT logs) — plus a one-click Let's Encrypt
certificate from its Access card. The daemon publishes its own
addresses (LAN addresses are the point: public name + real certificate
+ private address gives a warning-free padlock on your own network with
no port forwarding), answers the ACME DNS-01 challenge through the
service with daemon-signed requests, and keeps its private keys local.
The service's DNS authority covers exactly the subzone and nothing else
— the zero-authority doctrine applied to DNS.

Setup, one time:

1. In the parent zone (wherever `example.com` is hosted), add two
   records: `A ns-fleet.example.com → <this box's public IP>` and
   `NS fleet.example.com → ns-fleet.example.com`. Pin that IP (an
   elastic/static address) — replacing the box means keeping it.
2. Open 53/udp and 53/tcp to the box. Binding :53 as a non-root service
   needs `AmbientCapabilities=CAP_NET_BIND_SERVICE` in the systemd unit.
3. Run with `--dns-zone fleet.example.com --dns-ns-name
   ns-fleet.example.com --dns-listen 0.0.0.0:53`.

The register response then carries each daemon's `fleet_dns` name; the
daemon's Connect card shows it with a **Get a real certificate** button.
Address records persist in the state file and follow the daemon-record
lifecycle (they survive claim/release; the stale-unclaimed sweep drops
them). ACME TXT challenges are in-memory and self-expire. Posture:
authoritative-only, `Refused` outside the zone, no AXFR, RFC 8482
minimal `ANY`, 60 s TTLs. Daemons validating against Let's Encrypt
*staging* set `INTENDANT_ACME_DIRECTORY` to the staging directory URL.
Honest caveats: a single NS is a SPOF for fleet *names* (enrolled
browsers keep working via remembered routes; renewals retry), Let's
Encrypt rate-limits new certificates per registered domain (~50/week —
request a limit raise before any large fleet), and a hostile zone
operator could mint certificates for fleet names — key verification
protects enrolled browsers, and CT logs make mis-issuance public
evidence.

## Pointing daemons at it

In `intendant.toml`:

```toml
[connect]
enabled = true
rendezvous_url = "https://connect.example.com"
daemon_id = "my-daemon"          # optional; defaults to the daemon public key
auth_token = "<the --daemon-token value>"
```

or via `INTENDANT_CONNECT_RENDEZVOUS_URL`, `INTENDANT_CONNECT_DAEMON_ID`,
and `INTENDANT_CONNECT_TOKEN`. `enabled = true` with no `rendezvous_url`
defaults to the hosted instance. The dashboard's **Access → Intendant
Connect** card drives all of this without touching the file: it toggles
`enabled`, shows registration/claim state, and reveals the claim phrase
to manage-grade sessions.

## Claiming, co-signed bindings, and release

An unclaimed daemon's register response carries a 12-word BIP39 claim
phrase (10-minute TTL, hash-at-rest, reminted on expiry) plus a claim
URL; the daemon shows them in its startup log and in the Access card.
Entering the phrase while signed in (passkey) starts a claim: the
service challenges the daemon, and the daemon signs a proof.

Claim proofs come in two shapes. `intendant-connect-claim-v1` is
account-blind (legacy). `intendant-connect-claim-v2` — signed whenever
the challenge names the claiming account — binds the account's user id
and handle into the payload the **daemon** signs, so the account↔daemon
binding is co-signed by the daemon's own identity key instead of resting
on the service's assertion. The daemon persists that acknowledgment
beside its identity key, and every later register response returns the
service-asserted owner (`claimed_by_user_id`/`claimed_by_handle`); the
daemon cross-checks the two and surfaces the result in the Access card
as **co-signed**, **service-asserted**, or **mismatch** (a re-bind the
daemon never acknowledged). The transparency log records which proof
protocol sealed each claim.

Claiming grants **no authority** — sessions still resolve against the
daemon's local IAM (see the role ceilings and org lanes in the trust
chapter) — with one deliberate, tightly-scoped exception below.

## First-owner bootstrap (fresh boxes)

A daemon whose local IAM is completely empty (no principals, no grants —
a fresh VPS) **mints its own claim phrase** instead of accepting a
service-minted one: it registers only the SHA-256 of the normalized
phrase, so the rendezvous can route a claim but never sees plaintext.
The claim page hashes what the user types (plaintext codes stop leaving
the browser altogether) and, when the service answers
`needs_bootstrap_arm`, arms the claim: it loads-or-mints this origin's
browser identity key and posts it with an HMAC tag keyed by the phrase,
binding that exact key and account. The daemon recomputes the tag — a
valid tag proves the claimer read the phrase off the box (the same proof
SSH access would be) and endorses exactly that key, so the daemon
enrolls it as `role:root` (recorded with the sentinel origin
`connect-bootstrap`, which no hosted-origin role ceiling caps) and only
then co-signs the claim. A relay cannot substitute its own key (the tag
would not verify), a wrong phrase refuses the whole claim with the real
reason surfaced to the page, and the window closes forever the moment
any principal or grant exists. This completes the zero-install story:
`curl … | sh` on a fresh box, read twelve words from its log, and the
browser that claims it owns it.

Bindings are releasable from both sides, and both paths append
`daemon_unclaimed` transparency-log entries: the account owner revokes
from the service UI, and the **daemon** posts a timestamp-fresh release
signed with its identity key to `POST /api/daemon/unclaim` — the
recovery verb for a squatted or mis-claimed box (whose claiming account
would never revoke), also exposed as the Access card's "Release claim".
A fresh claim phrase mints on the next register poll.

## Reachability for NAT'd daemons

Register responses echo `observed_ip` — the public address the service
saw the poll arrive from. A cloud box's interfaces carry only private
addresses (the public IP lives on the provider's 1:1 NAT), and the
dashboard-control engine gathers no server-reflexive candidates, so
Connect offers advertise an **ICE-TCP candidate at
`observed_ip:gateway_port`** — the one address the world can actually
reach, over the same port that already serves the dashboard. Browsers
dial it directly; no STUN or TURN is required for the hosted
dashboard-control path (the box's firewall must allow the gateway port
inbound). Display sessions opened through a hosted dashboard advertise
the same tuple. The echoed address family follows how the daemon reached
the service — a daemon polling over IPv6 advertises a v6 candidate that
v4-only visitors cannot dial, so pin the daemon's egress (or the service
hostname it resolves) to v4 if your visitors are. Reachability metadata
only: a lying proxy chain could at worst advertise an unreachable
candidate.

Because the service reads the caller's address from `X-Forwarded-For`
(falling back to `X-Real-IP`), the reverse proxy in front of a
self-hosted instance **must set one of those headers** — with a plain
proxy_pass and no forwarding headers, `observed_ip` stays empty and
hosted dashboards cannot reach any NAT'd daemon, a failure that only
shows up later as an ICE timeout. Verify the full chain after deploying:

```bash
curl -s -X POST https://connect.example.com/api/daemon/register \
  -H 'content-type: application/json' \
  -d '{"protocol":"intendant-connect-rendezvous-v1","daemon_id":"probe","daemon_public_key":"probe"}' \
  | grep observed_ip   # must echo YOUR public IP, not null
```

(`scripts/deploy-connect-prod-alpha.sh` runs this probe automatically
after every deploy and fails loudly when the echo is missing.)

Caddy gotcha (this bit the default instance): within a `reverse_proxy`
block, `header_up -X-Forwarded-For` deletions are applied **after**
`header_up X-Forwarded-For {remote_host}` sets, so the strip-then-set
idiom deletes the value it just set. Use the set alone — a set already
replaces anything the client supplied.

## End-to-end transport validation

`scripts/connect-transport-e2e.cjs` asserts the outcome this whole
chapter exists for — a browser that registered at the rendezvous,
claimed a **fresh** daemon with its first-owner bootstrap phrase, and
opened the hosted dashboard gets an **OPEN dashboard-control
DataChannel** — entirely locally: no cloud resources, no real accounts.
It spawns `intendant-connect` (`--open-registration`, `localhost`
WebAuthn origin), a ~30-line `X-Forwarded-For`-injecting forward proxy
standing in for the production reverse proxy, and a scratch-`HOME`
daemon (keyless, `PROVIDER=mock`) whose empty IAM mints the bootstrap
phrase; headless Chromium then mints the passkey account with a CDP
virtual authenticator and walks the real `/connect` signup → phrase →
bootstrap-arm page flow.

Two hosted `/app` dashboard connections are asserted. The baseline pass
requires channel open + verified daemon binding + an answered status RPC
(a granted session, not a refusal — the claiming browser key was
bootstrap-enrolled `role:root`). The TCP-forced pass then makes UDP
unroutable from inside the page (answer-SDP candidate strip plus
suppression of the browser's own UDP trickle, so no peer-reflexive UDP
pair can sneak back) — the topology of a cloud box whose UDP is filtered
— and requires the *selected* ICE pair to be `tcp` at
`observed_ip:gateway_port`, the daemon to log `ICE-TCP enabled on …`,
and a second `[dashboard/control] data channel open`. Each regression
the first cloud deployment hit fails a distinct printed stage: a proxy
dropping `X-Forwarded-For` fails the register-echo stage, a daemon not
advertising ICE-TCP fails the candidate stage, and DTLS/SCTP transmits
misrouted off the nominated TCP pair reproduce their 30-second DTLS
stall in the final stage.

```bash
cargo build --bin intendant --bin intendant-runtime --bin intendant-connect
node scripts/connect-transport-e2e.cjs      # target/debug; --release for release bins
```

Operator battery, not CI: it needs a Chromium (Playwright's browser or
`CHROME_PATH`/`CHROME_BIN`; see `scripts/lib/browser-automation.cjs`)
and one routable non-loopback IPv4 interface (auto-detected; `--lan-ip`
overrides). Prints staged progress, exits 0/1, cleans up its scratch
state (kept on failure for inspection).

The service stores each org's latest root-signed revocation list, blind:
`POST /api/orgs/revocations/publish` accepts a list whose embedded
signature verifies and whose `seq` is not lower than the stored one;
`GET /api/orgs/revocations?handle=&root_key=` serves it publicly. Member
browsers fetch the list for orgs they hold documents for and carry it to
every daemon they visit (the daemon's own public apply endpoint enforces
signature and monotonic `seq` again). A malicious board can only
withhold — it cannot forge (root signature), roll back (`seq`), or read
anything that is not already org-public.

## Notifications

Signed-in browsers can opt into Web Push alerts (Advanced →
Notifications). Two alert kinds exist, flagged per subscription
(`GET /api/push/subscriptions` lists yours;
`POST /api/push/preferences` flips `notify_presence` /
`notify_requests` per endpoint):

- **Presence** (`notify_presence`, on by default when you enable push):
  a claimed daemon stopped polling (default: offline for 3 minutes;
  `INTENDANT_CONNECT_PRESENCE_OFFLINE_MS`) or came back. Composed purely
  from the polling presence the rendezvous already sees.
- **Pending agent requests** (`notify_requests`, strictly opt-in): a
  daemon reports that an agent→user request — a command approval or a
  question — has sat unanswered with no dashboard connected
  (`POST /api/daemon/notify`, signed with the daemon's registered
  identity key like unclaim/DNS publishes, rate-limited, claimed daemons
  only). **Privacy posture, load-bearing:** the nudge wire and the push
  payload carry only the request *kind*, the daemon's display label, and
  a session display label — never command text, question text, file
  paths, or any other work content. The service stays zero-knowledge
  about the work itself; the payload constructor in `push.rs` pins this
  by test. The daemon side is conservative by construction: a 45-second
  grace period, only when no dashboard has connected since the request
  appeared, one nudge per session per 10 minutes, silent degrade when
  unclaimed or offline (`attention_nudge.rs`).

Payloads are encrypted to each browser subscription (RFC 8291 — the
push relay carries ciphertext), and the VAPID signing key is generated
automatically into the state file on first start. Dead subscriptions
are pruned on 404/410. Self-hosters get both kinds with zero extra
configuration — daemons pointed at your rendezvous nudge it exactly as
they would the hosted one.

## Transparency log and attestations

Every name binding the service hands out is committed to an append-only
RFC 6962-shaped Merkle log: which public key a computer had when it was
claimed, handle creations, org revocation-list publications, verified
badges, handle reclamations — and the served-artifact and release
manifests described below. The signed tree head is public
(`/api/log/sth`, ES256 key auto-generated into the state file) along
with entries, inclusion proofs, and consistency proofs
(`/api/log/{entries,proof,consistency,find,artifact-manifest,release-manifest}`).
Browsers pin the tree head and verify consistency on every visit
(Advanced → Transparency log), so rewriting history is detectable, not
merely forbidden.

Accounts can attach verified identities as decoration (Advanced →
Verified identity): a `_intendant.<domain>` TXT record checked over
DNS-over-HTTPS (`INTENDANT_CONNECT_DOH_URL` overrides the resolver) or
a public gist containing the claim line
(`INTENDANT_CONNECT_GIST_BASE`). Badges appear in the public directory
(`/api/directory/<handle>`) and in the log. Verification never gates
anything — keys stay the identity.

### Code transparency for the served dashboard

The log also commits **what the service serves**, not just what it says
([Trust Tiers](./trust-tiers.md), first-contact rung three: the hosted
origin's residual power is serving a different bundle). At startup the
service hashes every static artifact it can serve — each file under the
static root at its URL path, plus the embedded routes exactly as this
instance renders them (`/`, `/connect`, `/access`, `/trust`, the
origin-injected `/install.sh` and `/install.ps1`, `/logo.svg`,
`/favicon.png`, the landing screenshots) — and appends an
`artifact_manifest` entry when the result differs from the latest logged
one. The entry carries `artifacts` (a path-sorted list of
`{path, sha256}` with lowercase-hex hashes, comparable to `sha256sum`
output), `bundle_version` (the crate version), `git_sha` (the build's
commit, `-dirty` suffixed for uncommitted trees), and `manifest_hash` —
sha256 over the canonical byte string
`intendant-artifact-manifest-v1\n` then `{path}\t{sha256}\n` per
artifact. `GET /api/log/artifact-manifest` returns the current entry
with its log index, an inclusion proof, and the signed tree head, all
computed coherently.

Verification is deliberately **out of band** — page JS can never
honestly verify the origin that serves it:

```bash
intendant hosted-verify                     # the default rendezvous
intendant hosted-verify --connect https://connect.example.com
```

The verifier fetches the logged manifest, checks the tree-head
signature, the entry's inclusion proof, and consistency against the
tree head pinned under the daemon state root
(`~/.intendant/hosted-verify/<host>.json`, honoring `$INTENDANT_HOME`),
then downloads every listed artifact exactly as a browser would and
compares hashes — nonzero exit and a per-artifact diff on divergence.
Every daemon with Connect enabled also runs this check twice daily as an
advisory tripwire (the CT tripwire's sibling): a divergence flips
`hosted_bundle_state` to `alert` on the Connect status payload and
raises **HOSTED CODE ALERT** on the dashboard's Connect card; network
failures only stamp `hosted_bundle_last_error` and never block anything.
A deploy that replaces static files without restarting the service will
read as a divergence — restart so the new manifest is logged.

**Honest limits.** A malicious server can still serve targeted different
bytes to one victim, once — no log prevents that. What the log plus
independent monitors from multiple vantage points buy is that
*sustained* or *later-denied* equivocation becomes evidenced: the
operator is publicly committed to a manifest history, every daemon is a
monitor from its own vantage point, and "we never served that" stops
being deniable. Coverage is what the service declares — but the HTML
entrypoints are declared, so undeclared payloads require serving
modified (hash-diverging) entrypoints. A transforming proxy between
verifier and service (one that rewrites bodies) will surface as a
divergence; the verifier sends no `Accept-Encoding`, so ordinary
compression layers do not.

**Reproducibility.** A manifest entry maps back to source: take the
entry's `git_sha`, check out that commit, and rebuild — the dashboard
bundle is deterministic (`static/app/` fragments assemble into
`static/app.html` via `cargo run -p app-html-assembler`; the committed
WASM artifacts are pinned by `.wasm-pack-version`), so
`sha256sum static/app.html static/wasm-web/* static/wasm-station/*`
comparing clean against the logged hashes ties the served bytes to
reviewable source. The embedded pages are deterministic functions of
the public origin, reproducible by running `intendant-connect` locally
with the same `--origin` and hashing what it serves.

### Release transparency

The same log commits **what the project releases**, closing the update
channel's side of the story ([Trust Tiers](./trust-tiers.md)): after
publishing to GitHub Releases, the tag-triggered release pipeline
(`.github/workflows/release.yml`) hashes every uploaded artifact and
submits a `release_manifest` entry — `tag`, `version`, `platforms`, and
a name-sorted `artifacts` list of `{name, sha256, size}` (lowercase-hex
hashes, comparable to `sha256sum` output), plus `manifest_hash`: sha256
over the canonical byte string `intendant-release-manifest-v1\n{tag}\n`
then `{name}\t{sha256}\t{size}\n` per artifact. Submission is
`POST /api/log/release-manifest`, gated by a dedicated bearer token
(`--release-token` / `INTENDANT_CONNECT_RELEASE_TOKEN`; the pipeline
holds it as the `CONNECT_RELEASE_TOKEN` repository secret). The token
is deliberately not the operator `daemon_token`: the CI credential can
only ever append release manifests. With no token configured the
endpoint answers 503; identical re-submissions dedupe; a *changed*
manifest for an existing tag appends a new entry — republished
artifacts become public history, never silent replacement. Reads stay
public: `GET /api/log/release-manifest[?tag=<tag>]` returns the latest
entry (for a tag) with its index, inclusion proof, and signed tree
head, coherently.

```bash
intendant hosted-verify --releases              # the latest logged release
intendant hosted-verify --releases v0.3.0       # this tag MUST be logged
intendant hosted-verify --releases v0.3.0 --download
```

The default mode verifies the log legs exactly like the bundle check
(tree-head signature, inclusion proof, consistency against the same
per-host pin), then compares the logged artifact list against the
GitHub release's asset *metadata* — names, sizes, and the sha256
digests the API reports — without downloading multi-hundred-MB
artifacts; assets on the release that the log never blessed are flagged
too. `--download` upgrades to fetching every artifact and hashing it
against the log — the strongest check. With an explicit tag, a release
absent from the log is a failure (exit 1): an unlogged release is
exactly what the check exists to catch. (`--repo <owner/name>` points
self-hosted forks at their own repository, and the optional
`CONNECT_RELEASE_URL` repository *variable* points their pipeline at
their own rendezvous.) The macOS app's update check (launch and "Check
for Updates…") also asks the log about the release it is offering and
appends a "publicly committed / NOT committed / couldn't check"
advisory line — fail-open like the bundle tripwire: a log outage never
blocks updating, and the error is surfaced instead of swallowed.

Dormant-handle reclamation is stated policy: an account with zero
claimed daemons and no sign-in for the configured window loses its
handle (the account survives, renamed). Enforcement is opt-in via
`INTENDANT_CONNECT_RECLAIM_AFTER_MS` (unset/0 = off) and every
reclamation is logged.

## Discovery

A daemon with Connect enabled advertises its rendezvous in its agent card
(`/.well-known/agent-card.json` → `rendezvous_base`, `connect_daemon_id`),
and the dashboard records it in the signed fleet records
(`connect_signaling_base`, fleet-record payload v2). Links that open a
daemon through a rendezvous carry the base as the `connect_base` URL
parameter, so browsers follow the daemon's own rendezvous instead of
assuming the default instance.
