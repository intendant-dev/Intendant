# Your Fleet, Your Name

The optional custom-domain lane serves the bounded daemon dashboard at an
exact DNS name the owner controls, such as `box.example.com`. The reachability
relay still moves only TLS ciphertext. The daemon owns the certificate key,
the ACME account, the WebAuthn relying-party id, and every resulting lease.
The lane is dark until explicitly configured.

```toml
[connect]
enabled = true
relay_enabled = true
relay_endpoint = "relay.intendant.dev:443"

[connect.custom_domain]
enabled = true
name = "box.example.com"
acme_issuance_enabled = false

[connect.custom_domain.dns]
provider = "cloudflare"
zone_id = "your-zone-id"
token_env = "CLOUDFLARE_API_TOKEN"
```

Point the name at the relay:

```dns
box.example.com. 300 IN CNAME relay.intendant.dev.
```

The daemon registers that exact SNI name over its daemon-identity-signed relay
control channel. The relay routes an exact name only when one active daemon
claims it; conflicting live claims are refused. Fleet-label routing remains a
separate compatibility path. Neither registration nor routing grants daemon
authority.

## Pin certificate issuance

On first boot, **Access → Hosted control → Your fleet, your name** displays the
daemon's ACME account URI. Publish a CAA record that pins both that account and
DNS-01:

```dns
box.example.com. 300 IN CAA 0 issue "letsencrypt.org; accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/EXAMPLE; validationmethods=dns-01"
```

Use the exact URI shown by the daemon. CAA is inherited from the nearest
ancestor when the name has no CAA RRset, so placing the record at
`example.com` can cover every intended child; placing it at
`box.example.com` limits the policy to that name. Check existing CAA policy
before adding a record. DNSSEC is an additional, optional protection at the
registrar.

After the CAA record is visible from the daemon's network, set
`acme_issuance_enabled = true` and restart the daemon. This separate switch is
false by default: the first boot may create the ACME account and display its
URI, but it cannot submit a certificate order before the owner completes the
CAA ceremony.

The daemon reuses the same locally stored ACME account used by its certificate
client. The DNS credential can add and remove only the exact `_acme-challenge`
TXT value for the order. Store it as a daemon credential lease where possible;
configuration names an environment-variable fallback but never embeds the
secret.

Cloudflare requires a narrowly scoped token with DNS edit access to the named
zone. Generic RFC2136 is also supported:

```toml
[connect.custom_domain.dns]
provider = "rfc2136"
server = "ns1.example.com:53"
zone = "example.com"
key_name = "intendant-acme."
secret_env = "INTENDANT_RFC2136_TSIG_SECRET"
ttl_secs = 60
propagation_delay_secs = 2
```

The RFC2136 secret is the base64 TSIG key. Updates use TCP, HMAC-SHA256, an
exact-value append, and exact-value cleanup; unrelated TXT records are not
replaced. Alternate credential environment names must end in `_API_TOKEN`
(Cloudflare) or `_TSIG_SECRET` (RFC2136), so the controller can derive and
enforce the runtime-child scrub. The `INTENDANT_` namespace is reserved; use
the documented RFC2136 default there.

## Passkeys and bounded leases

The configured WebAuthn `rp_id` defaults to the exact custom name and, when
specified, must equal it. It cannot be widened to a parent domain. A local
owner or enrolled direct-mTLS root dashboard creates a one-time enrollment
invitation; the link opens the exact custom origin, where WebAuthn creates the
credential. This split keeps both the owner authorization and the browser's
rp_id check intact. Invitations expire after ten minutes and are consumed at
ceremony start. Passkey records and counters stay in the daemon's owner-only
authority store.

Opening `https://box.example.com/` creates a non-extractable tab key. A
successful user-verifying passkey assertion approves only the signed request
for that tab key, preset, and lifetime. The result is the same short-lived
View, Tasks, or Operate lease described in [Hosted Control](./hosted-control.md);
the immutable floor still excludes access/IAM management, credential
management and vault unseal, organization-root operations, approval
resolution, and changes to the lane's ceiling. Root administration remains on
the local or independently enrolled direct-mTLS surface.

On borrowed hardware, choose the browser's cross-device WebAuthn flow so the
credential remains on the phone. Revoking a registered passkey prevents new
assertions; active leases remain separately visible and revocable.

## Operational checks

The daemon checks the stored certificate at boot and checks renewal every
twelve hours, renewing inside thirty days of expiry. The Access card shows the
certificate state, expiry, provider, account URI, passkeys, and the last
configuration or issuance error.

The custom name and service-assigned fleet name are distinct TLS provenance
classes. Exact custom SNI must agree with the HTTP Host and browser Origin.
Public custom-name requests receive no ambient local, mTLS-root, or fleet
authority: protected HTTP and WebSocket routes require the bounded lease proof
and one-use ticket. `/mcp`, approval resolution, and access-management routes
remain outside the public lane.
