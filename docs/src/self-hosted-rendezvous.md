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
| `--daemon-token` | `INTENDANT_CONNECT_TOKEN` | Bearer token daemons present on the polling endpoints |

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
and `INTENDANT_CONNECT_TOKEN`. On startup the daemon registers and prints
a claim URL; opening it signs the account in (passkey) and binds the
daemon to it. Claiming grants **no authority** — sessions still resolve
against the daemon's local IAM (see the role ceilings and org lanes in
the trust chapter).

## Discovery

A daemon with Connect enabled advertises its rendezvous in its agent card
(`/.well-known/agent-card.json` → `rendezvous_base`, `connect_daemon_id`),
and the dashboard records it in the signed fleet records
(`connect_signaling_base`, fleet-record payload v2). Links that open a
daemon through a rendezvous carry the base as the `connect_base` URL
parameter, so browsers follow the daemon's own rendezvous instead of
assuming the default instance.
