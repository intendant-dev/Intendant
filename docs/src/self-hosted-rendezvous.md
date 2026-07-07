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

The service speaks plain HTTP; terminate TLS in front of it (nginx,
Caddy, a cloud load balancer). WebAuthn requires the public origin to be
HTTPS. A systemd unit is just the command above with
`Restart=always` and a writable state directory; the deploy script the
default instance uses (`scripts/deploy-connect-prod-alpha.sh`) is a
worked example.

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
chapter).

Bindings are releasable from both sides, and both paths append
`daemon_unclaimed` transparency-log entries: the account owner revokes
from the service UI, and the **daemon** posts a timestamp-fresh release
signed with its identity key to `POST /api/daemon/unclaim` — the
recovery verb for a squatted or mis-claimed box (whose claiming account
would never revoke), also exposed as the Access card's "Release claim".
A fresh claim phrase mints on the next register poll.

## Revocation bulletin board

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
Notifications): the service notifies when a claimed daemon stops polling
(default: offline for 3 minutes; `INTENDANT_CONNECT_PRESENCE_OFFLINE_MS`)
and when it returns. Alerts are composed purely from the polling
presence the rendezvous already sees, payloads are encrypted to each
browser subscription (RFC 8291 — the push relay carries ciphertext), and
the VAPID signing key is generated automatically into the state file on
first start. Dead subscriptions are pruned on 404/410.

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
