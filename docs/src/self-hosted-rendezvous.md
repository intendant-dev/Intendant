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
badges, and handle reclamations. The signed tree head is public
(`/api/log/sth`, ES256 key auto-generated into the state file) along
with entries, inclusion proofs, and consistency proofs
(`/api/log/{entries,proof,consistency,find}`). Browsers pin the tree
head and verify consistency on every visit (Advanced → Transparency
log), so rewriting history is detectable, not merely forbidden.

Accounts can attach verified identities as decoration (Advanced →
Verified identity): a `_intendant.<domain>` TXT record checked over
DNS-over-HTTPS (`INTENDANT_CONNECT_DOH_URL` overrides the resolver) or
a public gist containing the claim line
(`INTENDANT_CONNECT_GIST_BASE`). Badges appear in the public directory
(`/api/directory/<handle>`) and in the log. Verification never gates
anything — keys stay the identity.

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
