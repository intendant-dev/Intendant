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
control channel and proves possession of the matching, publicly trusted
singleton-SAN certificate key. The relay verifies the chain, exact name, and
key signature before accepting the route; a daemon identity alone cannot claim
another tenant's name. The relay routes an exact name only when one active
daemon proves it. A conflicting live registration is rejected while the
incumbent route remains active. Each v2 process has a signed poller id, its own
proof liveness, and its own exact-name dialback queue. During a rolling upgrade,
a v1 poll may refresh and consume only fleet-label fallback work; it cannot
extend or consume a v2 exact-name route. An explicit empty v2 registration
clears that poller's names. Before every registration, the process reloads
shared certificate generations into its process-local TLS resolver. Neither
registration nor routing grants daemon authority. If exact-name proof
construction or proof-specific validation fails, the client retains the
independent v1 fleet-label route; daemon-authentication failures do not trigger
that fallback.

The custom name must also remain outside every current or previously recorded
service fleet zone. The daemon re-evaluates that separation whenever the route
is used. While Connect is enabled, the lane stays closed until the current
rendezvous registration has supplied a fleet-zone observation and that
provenance has been accepted durably. Learning a later overlapping fleet zone
therefore disables the custom lane instead of temporarily reclassifying a
service-controlled name as owner-controlled during startup. HTTP requests and
WebSocket upgrades recheck that live gate even on an existing TLS connection;
active custom-domain sockets carry the same gate into their recurring
authorization and buffered-input checks, so losing eligibility closes them.
A fleet-DNS observation is accepted only when both fields form the same
canonical `d-<20hex>.<zone>` pair; an absent, empty, noncanonical, or mismatched
pair leaves the gate closed and writes no provenance. Absence clears current
live fleet-name metadata but is not independent evidence that the service has
no delegated zone. A Connect-enabled custom lane therefore waits for a
complete current observation; a Connect-disabled deployment does not use that
service-observation gate.
The historical exact-name and zone sets, their serialized file, and the
process cache are all bounded. Invalid or excess history sets a durable
incomplete marker and closes the owner-name lane; a new Connect observation
cannot grow the live TLS classifier past the same exact-name cap. Repeated
route checks reuse only a cache entry whose cross-platform file identity and
change stamp still match the authority record.

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
TXT value for the order. Before changing DNS, the daemon durably journals the
provider, exact name, and exact value without storing the provider secret.
Cleanup removes that journal only after the provider confirms the exact record
is gone. Before the provider call begins, the daemon also reserves the exact
name, value, and provider in a bounded secondary cleanup journal. Provider
completion first makes that reservation cleanup-capable, then commits the
primary phase, and retires the reservation last. A crashed creator therefore
leaves either a completed cleanup entry or a stale creation reservation that
becomes an idempotent exact cleanup after its lease plus grace period. The
primary journal carries creation, active-validation, and cleanup phases with
bounded owner leases. A sibling process therefore leaves a live challenge
alone; only an explicit handoff to cleanup or an expired lease plus grace
period makes it reclaimable. Startup and every later certificate pass retry a
reclaimable journal before creating another challenge, covering crashes,
cancellation, and transient provider failures. Store the credential as a
daemon credential lease where possible; configuration names an
environment-variable fallback but never embeds the secret. While a cleanup
journal is being reaped, its mutation-completion generation is compared again
before removal. If a newer challenge already owns the primary journal, the
secondary reservation retains the older exact cleanup independently and
blocks further challenge creation until it is drained. A late TXT write
therefore cannot land after the last durable cleanup record disappeared. While
a cleanup journal exists, its fallback name remains in the supervised-child
environment scrub even if the lane is disabled or later names a different
fallback. An unreadable journal makes that scrub conservatively remove all DNS-shaped
credential names until the journal is repaired or retired. Every supervised
coding-agent spawn reloads the shared journal immediately before constructing
the child environment and holds the shared authority lock until the operating
system copies it, so a sibling daemon process cannot create a journal across
that boundary or leave the scrub cache stale.

Certificate files are shared across daemon processes. Every renewal pass
reloads and validates that shared pair before deciding to order. New
generations commit the certificate chain and private key together in one
atomic authority record; an incomplete or mismatched legacy two-file
generation is excluded from TLS and can be replaced by the guarded issuance
path. A durable owner lease serializes issuance through pair commit. The same
record retains the ACME order URL, private key, and CSR across cancellation, so
a replacement process resumes the exact order and finalization material.
Explicitly missing or expired orders are replaced, and resumable state has a
bounded lifetime measured from the current order's immutable start time;
ownership claims and retry updates cannot extend it. A sibling process
therefore adopts a newly committed generation instead of consuming another CA
order; a stopped owner's lease can be reclaimed without changing the order key.
The active worker renews and rechecks its owner lease throughout DNS and ACME
waits and before certificate side effects. The final pair write, process-local
TLS install, and issuance-record removal run under the same authority lock and
owner-token check, so a superseded worker cannot install after takeover. CT
comparison is deferred only
while that live owner is inside the pre-ledger issuance window; a dormant or
expired resumable order remains recoverable but cannot suppress CT evidence.
Normal ownership replacement or a sibling-completed generation is treated as
worker handoff, not as authority-store corruption.

Custom-domain, relay, and credential wiring is restart-only. The live Connect
toggle may change enablement or the rendezvous destination, but the running
daemon preserves those boot-wired fields together until restart; this also
keeps DNS credential scrubbing aligned with the certificate worker. A running
relay tunnel pins its signed control polls, relay-mode DNS updates, and raw
dialback endpoint to that same boot configuration generation, so a live
Connect destination change cannot split a nonce and its data endpoint. Turning
Connect off cancels the boot-pinned poll and its active dialback tasks, sends a
signed poller disconnect, and explicitly withdraws relay-mode fleet DNS.
Turning it back on resumes only after the new registration has closed the
fleet-zone observation gate again.

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
ceremony start. Invitations, registration/authentication challenges, and the
ceremony rate window are stored under the daemon's cross-process authority
lock, so a relay request may move between service processes without duplicating
or losing the flow. Invitation consumption and registration-flow creation are
one atomic transaction, so a failed durable write does not burn the
invitation. Authentication starts have per-source and global windows, a
per-source pending cap, and capacity reserved for a previously unseen source;
the relay supplies a per-route, salted opaque bucket derived from the
connection source address so unrelated relay clients do not share one
admission window. That bucket is an availability hint only: it is not an
identity, credential, or authorization input. The empty passkey store,
including its stable WebAuthn user id, is created atomically under that lock
before any ceremony is exposed. Each exact custom name and `rp_id` has its own
derived store generation; changing the configured identity starts an empty
generation without deleting the old one, and returning to the former identity
reopens only its matching credentials. A legacy singleton store migrates only
when both fields match. Passkey records and counters stay in the same
owner-only authority store. Authentication finish rechecks the current fleet
zone and durable name provenance inside that same authority transaction before
it updates a counter or issues a lease. Its proved issuance does not enter or
consume the anonymous doorbell queue and rate windows; passkey ceremony
admission retains its independent per-source and global bounds.

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
twelve hours, renewing inside thirty days of expiry. Failed checks retry with a
bounded backoff, and granting a DNS credential lease wakes both issuance and
pending cleanup immediately. The wake uses a monotonic grant generation, so a
grant completed between the provider error and the retry waiter cannot be
lost. The Access card shows the certificate state, expiry, provider, account
URI, passkeys, and the last configuration or issuance error.

The custom name and service-assigned fleet name are distinct TLS provenance
classes. Exact custom SNI must agree with the HTTP Host and browser Origin.
Public custom-name requests receive no ambient local, mTLS-root, or fleet
authority: protected HTTP and WebSocket routes require the bounded lease proof
and one-use ticket. `/mcp`, approval resolution, and access-management routes
remain outside the public lane.
