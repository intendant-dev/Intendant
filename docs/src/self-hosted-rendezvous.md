# Self-Hosted Rendezvous

`intendant-connect` — the hosted rendezvous behind `connect.intendant.dev`
— is an open, first-class deployable, not a chokepoint. It introduces
browsers to daemon route and presence records, carries account/route metadata,
delivers optional encrypted Web Push notifications, serves the discovery
client and installers, and stores client-signed fleet metadata. It does not
serve the daemon dashboard SPA or its WASM/static assets, and it does not relay
daemon control traffic in the default build. A claim creates no daemon
IAM principal or grant. The default
daemon stamps this route as hosted and applies immutable `role:none`, so
Connect-served code cannot open a daemon-control session. Org grant documents
are verified by the target daemon against its own trusted keys, and authority
on trusted local or independently reached direct-mTLS routes still comes from
the target daemon's local IAM.

That is a precise boundary, not an absolute "zero-authority" claim. Connect
is trusted for availability, its account and route metadata, push delivery,
and the code it serves. Push payloads are encrypted to the browser
subscription, but a malicious Connect-served page can lie about or exfiltrate
anything exposed in the Connect UI, and a malicious installer can compromise
what it installs.
What it cannot do in the default build is turn any claim, account assertion,
browser-key grant, or configuration edit into daemon authority. See [Trust
Architecture](./trust-architecture.md).

## Build and run

```bash
cargo build --release --bin intendant-connect
./target/release/intendant-connect \
  --listen 127.0.0.1:9876 \
  --origin https://connect.example.com \
  --rp-id example.com \
  --data-file /var/lib/intendant-connect/state.json \
  --daemon-token <random-shared-token>
```

| Flag | Env | Meaning |
|---|---|---|
| `--listen` | `INTENDANT_CONNECT_LISTEN` | Bind address (put a TLS reverse proxy in front) |
| `--origin` | `INTENDANT_CONNECT_ORIGIN` | Public origin browsers use; also the WebAuthn origin |
| `--rp-id` | `INTENDANT_CONNECT_RP_ID` | WebAuthn relying-party id (a registrable suffix of the origin's host) |
| `--static-root` | `INTENDANT_CONNECT_STATIC_ROOT` | Deprecated compatibility input; accepted but ignored. Connect serves only embedded discovery pages/assets. `/app` and `/app.html` redirect to `/connect`; every other unknown path is `404` |
| `--data-file` | `INTENDANT_CONNECT_DATA_FILE` | JSON state (accounts, claims, fleet records) |
| `--daemon-token` | `INTENDANT_CONNECT_TOKEN` | Shared deployment bearer required at registration unless open registration is enabled; also the admin-API credential. It is not the per-daemon polling credential |
| `--open-registration` | `INTENDANT_CONNECT_OPEN_REGISTRATION` | Skip only the shared deployment bearer on registration (rate-limited; stale unlinked records expire). Registration still requires a fresh daemon-key signature; each success rotates a short-lived daemon-session credential required for poll/answer/error/dry/claim-proof. The token keeps guarding the admin API |
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

The warning-free discovery option ([Trust Tiers](./trust-tiers.md)):
delegate one subzone to the service and every registered daemon gets a
real name — `d-<hash>.<zone>`, an opaque sha256-derived label (these
names land in public CT logs) — plus a one-click Let's Encrypt
certificate from its Access card. The daemon publishes its own
addresses (LAN addresses are the point: public name + real certificate
+ private address gives a warning-free public shell on your own network with
no port forwarding), answers the ACME DNS-01 challenge through the
service with daemon-signed requests, and keeps its private keys local.
The service's DNS authority covers exactly the delegated subzone and nothing
else. That still makes it an authority over code at every name in the subzone,
so the daemon classifies fleet SNI before IAM and serves only public
shell/discovery bytes there. Protected HTTP/MCP routes, direct signaling, and
WebSockets require an independently reached direct-mTLS or loopback origin.

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
daemon's Connect card shows it with an **Enable HTTPS discovery** button.
Address records persist in the state file and follow the daemon-record
lifecycle (they survive link/release; the stale-unlinked sweep drops
them). ACME TXT challenges are in-memory and self-expire. Posture:
authoritative-only, `Refused` outside the zone, no AXFR, RFC 8482
minimal `ANY`, 60 s TTLs. Daemons validating against Let's Encrypt
*staging* set `INTENDANT_ACME_DIRECTORY` to the staging directory URL.
Honest caveats: a single NS is a SPOF for fleet *names* (independently
remembered direct routes keep working; renewals retry), Let's
Encrypt rate-limits new certificates per registered domain (~50/week —
request a limit raise before any large fleet), and a hostile zone
operator could redirect fleet names and mint certificates for them. CT logs
make issuance public evidence, but that evidence does not protect an enrolled
browser credential from same-origin code. The discovery-only SNI gate is the
authority boundary.

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
`enabled`, shows registration/link state, and reveals the one-time claim
code to manage-grade sessions.

## Claiming, co-signed route links, and release

An unlinked daemon locally mints a single-use 12-word BIP39 claim code and URL
and shows them in its startup log and Access card. Its fresh, identity-signed
registration sends Connect only the code's SHA-256 hash, timestamp, daemon id,
and public key. Connect stores the hash with a 10-minute TTL and never receives
or returns the plaintext. The printed URL places the phrase in a fragment
(`/connect#claim_code=...`), which browsers do not include in HTTP requests or
referrers. The Connect page reads and immediately scrubs that fragment,
normalizes and hashes the phrase locally with Web Crypto, and sends only the
base64url SHA-256 digest to the strict claim API. There is no plaintext or
query-string compatibility path. The service matches the digest once,
challenges the daemon, and verifies the daemon's signed proof.

Every successful registration is itself single-use: the fresh signature must
be newer than the previous accepted proof, and the response rotates a
short-lived daemon-session credential. `/api/daemon/next`, `answer`, `error`,
`dry`, and `claim-proof` require that credential even when
`--open-registration` is enabled. Open registration removes only the shared
deployment bearer; it never makes a public key or daemon id a polling token.
The public registration edge is additionally bounded: request bodies cap at
4 KiB; daemon ids cap at 128 ASCII `A-Z a-z 0-9 . _ - :` bytes; public keys and
signatures must be canonical unpadded base64url encodings of exactly 32 and 64
bytes; the general endpoint allows 120 requests per observed source per minute,
while new identities allow 30 per observed source per hour; and at most 1,024
unclaimed daemon records may exist after the stale-record sweep. Existing
identities can still refresh at capacity. If a reverse proxy supplies client-IP
headers, it must overwrite rather than append or trust inbound forwarding
headers, as in the Caddy example above; the global unclaimed-record cap remains
the backstop against distributed sources.

Registration binds a `daemon_id` to its first identity key even before the
route is linked. A second key cannot replace that binding or inherit the code
already printed for the first key; an unlinked stale record must age out before
the id can be registered afresh. This prevents open registration from moving a
live one-time code between daemon identities.

Treat the code as a short-lived bearer token for this route link, not as an
owner secret. The claim page asks for this one-time code only; it must never
ask for a password, API key, recovery phrase, private key, or passkey secret.

Claim proofs come in two shapes. `intendant-connect-claim-v1` is
account-blind (legacy). `intendant-connect-claim-v2` — signed whenever
the challenge names the claiming account — binds the account's user id
and handle into the payload the **daemon** signs, so the account↔daemon
route link is co-signed by the daemon's own identity key instead of resting
on the service's assertion. The daemon persists that acknowledgment
beside its identity key, and every later register response returns the
service-asserted linked account (`claimed_by_user_id`/`claimed_by_handle`;
the `claimed_*` names are retained for wire compatibility); the daemon
cross-checks the two and surfaces the result in the Access card
as **co-signed**, **service-asserted**, or **mismatch** (a re-bind the
daemon never acknowledged). The transparency log records which proof
protocol sealed each claim.

Claiming grants **no authority**, including on a fresh box with empty IAM.
It creates no principal or grant; it only associates discovery and routing
metadata with the account. A hosted Connect account assertion never
authenticates to the daemon. The default build fixes hosted provenance at
`role:none`; a separately enrolled browser key, stored grant, hand-edited
ceiling, or disclosure cannot enable hosted control. Use a trusted local or
independently reached daemon-served direct/mTLS surface for daemon access. The
packaged macOS app is not an alternative distribution anchor in this alpha:
no Developer ID-signed/notarized release has been published.

Root bootstrap is deliberately outside this flow. Use `intendant access
setup` from the machine's console/SSH session or a direct mTLS root session.
The former `--owner <browser-key>` shortcut is
retired in this alpha because a fingerprint alone is not a complete certless
remote authentication protocol. Never treat a Connect claim code as a password,
owner secret, or proof of root authority.

> **Alpha migration (IAM schema v2).** Earlier alpha builds could create an
> uncapped `role:root` client-key grant with origin `connect-bootstrap` on
> first claim. Loading that state now revokes every active grant on those
> legacy principals, records a `revoke_legacy_connect_bootstrap` audit event,
> and restores both hosted ceilings to `role:none`. The account/route link
> survives as discovery metadata, but trusted re-enrollment is required.

> **Mixed-version rollout.** The current Connect service returns `403` for
> authenticated browser `offer`, `ice`, and `close` calls before it mutates a
> queue, rate-limit bucket, or active-session record. Current daemons also drop
> those three event kinds before touching dashboard-control, IAM, or enrollment
> state, which protects them against older/self-hosted services. Restarting only
> the service cannot revoke a legacy peer-to-peer DataChannel that is already
> established: upgrade and restart the daemon, close every old Connect tab, and
> let IAM schema v2 revoke legacy `connect-bootstrap` grants.

Bindings are releasable from both sides, and both paths append
`daemon_unclaimed` transparency-log entries: the linked account releases
from the service UI, and the **daemon** posts a timestamp-fresh release
signed with its identity key to `POST /api/daemon/unclaim` — the
recovery verb for a squatted or mis-linked route (whose linked account
would never release it), also exposed as the Access card's **Unlink account**.
After release, the daemon locally rotates its one-time claim code and registers
the new signed hash.

## Reachability for NAT'd daemons

Register responses echo `observed_ip` — the public address the service
saw the poll arrive from. This remains useful reachability metadata and keeps
the lower-level ICE-TCP transport testable, but it is not authority: the
default build refuses hosted-provenance control before a Connect tab can use
that candidate. A cloud box's interfaces carry only private addresses (the
public IP lives on the provider's 1:1 NAT), so transport experiments can
advertise an **ICE-TCP candidate at `observed_ip:gateway_port`**. The echoed
address family follows how the daemon reached
the service — a daemon polling over IPv6 advertises a v6 candidate that
v4-only visitors cannot dial, so pin the daemon's egress (or the service
hostname it resolves) to v4 if your visitors are. Reachability metadata
only: a lying proxy chain could at worst advertise an unreachable
candidate.

Because the service reads the caller's address from `X-Forwarded-For`
(falling back to `X-Real-IP`), the reverse proxy in front of a
self-hosted instance **must set one of those headers** if it relies on this
reachability metadata. With a plain proxy pass and no forwarding headers,
`observed_ip` stays empty. Verify the full chain after deploying with the
operator bearer (the deliberately unsigned deployment probe is accepted only
because that token already guards the admin API):

```bash
curl -s -X POST https://connect.example.com/api/daemon/register \
  -H "authorization: Bearer $INTENDANT_CONNECT_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"protocol":"intendant-connect-rendezvous-v1","daemon_id":"observed-ip-probe","daemon_public_key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","claim_code_hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","issued_at_unix_ms":0,"signature":""}' \
  | grep observed_ip   # must echo YOUR public IP, not null
```

(`scripts/deploy-connect-prod-alpha.sh` runs this probe automatically
after every deploy and fails loudly when the echo is missing.)

Caddy gotcha (this bit the default instance): within a `reverse_proxy`
block, `header_up -X-Forwarded-For` deletions are applied **after**
`header_up X-Forwarded-For {remote_host}` sets, so the strip-then-set
idiom deletes the value it just set. Use the set alone — a set already
replaces anything the client supplied.

### Reachability relay (ciphertext SNI passthrough)

`observed_ip` only helps a client that can open a direct connection. A
daemon behind NAT with no port forward is unreachable at its fleet name
except on the LAN. The **reachability relay** closes that gap without
Connect ever terminating TLS or seeing plaintext:

- A raw TCP listener (`--relay-listen`) peeks each connection's TLS
  **ClientHello to read the SNI without terminating the handshake**
  (fragmented ClientHellos are handled; non-TLS bytes are refused). When
  the SNI names a registered fleet label whose daemon holds an active
  tunnel, the relay splices the raw bytes to that daemon. Everything else
  is refused.
- Each daemon holds a persistent **control channel** to the service
  (`POST /api/relay/next`, a long-poll authenticated by the daemon
  identity key with the same signed/freshness discipline as
  `/api/dns/publish`). When a browser connects, the relay mints a
  single-use nonce, hands it to the daemon over the control channel, and
  the daemon **dials back** a data connection carrying that nonce. The
  relay correlates the two and splices them 1:1.
- The browser's TLS handshake therefore completes end-to-end against the
  **daemon's own fleet certificate**. Connect moves only ciphertext.

The relay is **availability-only**: it terminates no TLS, holds no
certificate, mints no authority, and logs no plaintext. Routing a fleet
SNI to a daemon does not change how the daemon classifies that
connection — it still arrives bearing the fleet SNI, which the gateway
already treats as discovery-only, so a relayed connection is refused at
every protected route exactly as a direct one is.

Enable it with the all-or-nothing `--relay-*` group (both flags or
neither; default off, mirroring `--dns-*`):

```bash
intendant-connect \
  --relay-listen 0.0.0.0:443 \      # raw passthrough port (browsers + dial-backs)
  --relay-address 203.0.113.10      # public address published in fleet DNS
```

Equivalently `INTENDANT_CONNECT_RELAY_LISTEN` / `INTENDANT_CONNECT_RELAY_ADDRESS`.

Deployment notes:

- **The relay must receive raw TLS.** Do not place `--relay-listen`
  behind a TLS-terminating reverse proxy — that would break the
  passthrough. Expose the port directly, or front it with a TCP
  (layer-4) passthrough only.
- The relay and fleet DNS are usually co-deployed: a relay-mode daemon
  publishes `POST /api/dns/relay` (daemon-signed) so the zone answers its
  fleet label with `--relay-address` instead of the daemon's own (NAT'd)
  addresses. The store/serve split is unchanged — `dns.rs` serves the
  substituted address verbatim.
- Abuse is bounded by per-source-IP and per-tunnel connection caps, a
  per-connection byte cap, idle teardown, and a bounded dial-back wait.

A daemon opts in through `[connect] relay_enabled` + `relay_endpoint`
(see the configuration reference). It then holds the control channel,
dials back browser connections into its own gateway, and publishes
relay-mode fleet DNS while the tunnel is up.

## End-to-end transport validation

`scripts/connect-transport-e2e.cjs` asserts the outcome this whole
chapter exists for — a browser registers at the rendezvous, links a
**fresh** daemon with its locally minted one-time code, receives no authority
from that claim, and remains refused at immutable `role:none` even when a
trusted fixture creates a local grant for its browser key — entirely locally:
no cloud resources, no real accounts.
It spawns `intendant-connect` (`--open-registration`, `localhost`
WebAuthn origin), a ~30-line `X-Forwarded-For`-injecting forward proxy
standing in for the production reverse proxy, and a scratch-`HOME`
daemon (keyless, `PROVIDER=mock`) whose empty IAM receives no claim-time
mutation; headless Chromium then mints the passkey account with a CDP
virtual authenticator and walks the real `/connect` signup → claim-code
link flow.

The validator requires the daemon IAM to remain unchanged by linking, checks
that registration never exposes plaintext code and rotates a daemon-session
credential, and proves authenticated browser `offer`, `ice`, and `close` calls
return `403` at the service before enqueueing anything, both before and after an
adversarial local operator grant. A daemon-side regression separately feeds all
three event kinds as if they came from an older/self-hosted service and verifies
that dashboard-control, IAM, and enrollment state stay unchanged. Direct/local
dashboard-control validators cover successful DataChannel and ICE transport;
the Connect validator expects zero hosted control sessions.

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
signature and monotonic `seq` again). A malicious board cannot forge a newer
list because it lacks the root signature, and it cannot make a daemon that
already applied sequence `N` accept an older sequence. It can withhold the
latest list—or serve a still-valid older list to a fresh daemon with no local
sequence history—so availability and first-sync freshness still depend on the
courier. The list contains only org-public revocation data.

## Notifications

Signed-in browsers can opt into Web Push alerts (Advanced →
Notifications). Two alert kinds exist, flagged per subscription
(`GET /api/push/subscriptions` lists yours;
`POST /api/push/preferences` flips `notify_presence` /
`notify_requests` per endpoint):

- **Presence** (`notify_presence`, on by default when you enable push):
  a linked daemon stopped polling (default: offline for 3 minutes;
  `INTENDANT_CONNECT_PRESENCE_OFFLINE_MS`) or came back. Composed purely
  from the polling presence the rendezvous already sees.
- **Pending agent requests** (`notify_requests`, strictly opt-in): a
  daemon reports that an agent→user request — a command approval or a
  question — has sat unanswered with no dashboard connected
  (`POST /api/daemon/notify`, signed with the daemon's registered
  identity key like unlink/DNS publishes, rate-limited, linked daemons
  only). **Privacy posture, load-bearing:** the nudge wire and the push
  payload carry only the request *kind*, the daemon's display label, and
  a session display label — never command text, question text, file
  paths, or any other work content. The service stays zero-knowledge
  about the work itself; the payload constructor in `push.rs` pins this
  by test. The daemon side is conservative by construction: a 45-second
  grace period, only when no dashboard has connected since the request
  appeared, one nudge per session per 10 minutes, silent degrade when
  unlinked or offline (`attention_nudge.rs`).

Payloads are encrypted to each browser subscription (RFC 8291 — the
push relay carries ciphertext), and the VAPID signing key is generated
automatically into the state file on first start. Dead subscriptions
are pruned on 404/410. Self-hosters get both kinds with zero extra
configuration — daemons pointed at your rendezvous nudge it exactly as
they would the hosted one.

## Transparency log and attestations

Every name binding the service hands out is committed to an append-only
RFC 6962-shaped Merkle log: which public key a computer had when its route was
linked, handle creations, org revocation-list publications, verified
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

### Code transparency for the served Connect client

The log also commits **what the service serves**, not just what it says
([Trust Tiers](./trust-tiers.md), first-contact rung three: the hosted
origin's residual power is serving a different bundle). At startup the
service hashes every embedded artifact it can serve, exactly as this
instance renders it (`/`, `/connect`, `/access`, `/trust`, the
origin-injected `/install.sh` and `/install.ps1`, `/logo.svg`,
`/favicon.png`, the embedded `/sw.js` push worker, and the landing
screenshots) — and appends an
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
A deploy that changes the compiled Connect pages/assets without restarting
the service cannot change the running bytes; restart to serve and log the new
embedded bundle.

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

**Reproducibility.** A manifest entry maps back to source: take the entry's
`git_sha`, check out that commit, and rebuild. The embedded Connect pages are
deterministic functions of the public origin; run `intendant-connect` locally
with the same `--origin` and hash the manifest's paths. The daemon-only
`static/app.html`, WASM, and vault kernel are intentionally absent because this
binary cannot serve them.

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
linked daemon routes and no sign-in for the configured window loses its
handle (the account survives, renamed). Enforcement is opt-in via
`INTENDANT_CONNECT_RECLAIM_AFTER_MS` (unset/0 = off) and every
reclamation is logged.

## Discovery

A daemon with Connect enabled advertises its rendezvous in its agent card
(`/.well-known/agent-card.json` → `rendezvous_base`, `connect_daemon_id`),
and the dashboard records it in the signed fleet records
(`connect_signaling_base`, fleet-record payload v2). Those fields are retained
as route/protocol compatibility metadata; the default hosted directory does
not turn them into `/app?connect=1` daemon-control links. A claimed Connect
route can show discovery and presence only. An independently recorded direct
daemon URL opens the daemon's own HTTPS/mTLS origin, where the daemon—not the
rendezvous—serves the dashboard and authenticates the client.
