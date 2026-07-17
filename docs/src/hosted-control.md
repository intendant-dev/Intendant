# Hosted Control

Hosted control is an optional daemon-local authority lane for a browser that
reaches a daemon at its fleet name. It is off by default. When off, the
fleet-name and reachability-relay surfaces retain their discovery-only
`role:none` behavior.

The lane does not turn a Connect account, passkey assertion, fleet route, or
browser origin into daemon authority. A trusted owner surface approves a
short-lived lease, the daemon materializes that lease in local IAM, and the
browser proves possession of the lease's tab-ephemeral key on every protected
HTTPS request. WebSocket admission uses a seconds-lived one-use ticket minted
by such a proof. Connect continues to route ciphertext and metadata without
terminating daemon TLS or minting a daemon principal.

The runtime switch is deliberately separate from ordinary Connect discovery:

```toml
[connect]
hosted_control_enabled = false
```

There is no environment-variable override. A restart with the switch off
refuses lease proofs and tickets and invalidates hosted sockets at their next
authority recheck without changing direct/mTLS sessions.

## Authority flow

1. The fleet-name page creates a non-extractable P-256 key in the current tab.
   The private key is not persisted in origin storage.
2. The page signs and submits a bounded lease request containing the public
   key, requested preset, requested lifetime, and a display label. The
   signature also binds the daemon, fleet origin, nonce, and timestamp, so a
   copied public key cannot be used to create a prompt.
3. The daemon stores and signs the canonical request. Connect may send a
   content-free notification that a request is waiting, but does not carry the
   request fields or an approval.
4. A trusted local console, direct-mTLS owner dashboard, or qualifying signed
   app fetches the daemon-signed request and approves or denies it. Approval
   may reduce, never increase, the requested preset or lifetime.
5. Approval creates an exact `hosted_lease` IAM principal and expiring grant.
   The returned daemon-signed lease document is non-bearer: every use also
   requires a fresh P-256 signature by its named browser key.
6. Protected HTTPS requests carry a proof bound to the method, raw
   path-and-query, daemon, lease-document digest, nonce, and timestamp.
7. The event lane obtains a random, one-use WebSocket ticket through a
   proof-bound HTTPS request. The daemon consumes that ticket atomically before
   upgrade and rechecks the opening authority throughout the connection.

Requests expire after ten minutes. Leases default to four hours and have a
compiled maximum of 24 hours. Request creation, anonymous polling, proof
nonces, and outstanding tickets all have per-key or per-request and global
bounds. Anonymous doorbell and poll proofs use replay capacity separate from
active lease proofs, so public traffic cannot consume the authenticated
request window. Pending requests are not displaced by newer requests; a full
pending queue refuses another doorbell until an owner decides one or one
expires. The relay's loopback last hop is not treated as a distinct
remote-client address, so public availability is globally bounded rather than
promised fairly among anonymous callers.

## Trust anchors

Lease approval is available on:

- the local console;
- a direct-mTLS owner dashboard;
- an enrolled signed application whose key is held by the platform keystore.

The trust property of an application anchor is signed distribution plus
platform-keystore custody, not its device category. A phone is the preferred
confirmation surface, but is not required. Product copy recommends confirming
on a device other than the requesting browser when convenient.

Application-anchor enrollment is itself a local or direct-mTLS owner ceremony.
An unsigned development artifact does not qualify. Until a qualifying signed
distribution is published, the compiled eligible-distribution set is empty
and the app-anchor set remains empty; local and direct-mTLS approval continue
to work.

On borrowed hardware, account authentication uses cross-device WebAuthn so the
credential remains on the owner's phone. The resulting browser tab is still a
disposable client borrowing only the approved lease.

## Presets and immutable floor

Each daemon has an owner-selected ceiling. The initial ceiling is **Tasks**.
Raising it is a dedicated owner ceremony; an integrated-tier daemon also
requires the hardening acknowledgement before accepting **Operate**. A daemon
cannot change its tier to integrated while the hosted ceiling is Operate: the
owner first lowers the ceiling, changes the tier, and then deliberately
re-enables Operate with the acknowledgement. The tier check and ceiling update
occur in the same IAM transaction. Lowering the ceiling revokes leases above
it in that transaction. Raising it does not upgrade an existing lease.

Preset ordering is `View < Tasks < Operate`:

| Operation family | View | Tasks | Operate |
|---|---:|---:|---:|
| Presence and bounded status | yes | yes | yes |
| Session inspection and logs | yes | yes | yes |
| Agent-visible display view | yes | yes | yes |
| Task submit and message/steer | no | constrained | constrained |
| Session lifecycle | no | no | yes |
| Terminal and shell | no | no | yes |
| Filesystem read/write | no | no | yes |
| Agent-visible display input | no | no | yes |

Every operation not listed is denied. No hosted preset admits IAM/access
management, credential management or vault unseal, organization-root
operations, approval resolution, settings/API-key management, peer
administration, or a change to the lane's own ceiling. Those omissions are a
compiled floor and are not lifted by a root role, state-file role edits, or a
generic IAM mutation.

The evaluator authorizes a hosted lease only when all of these agree:

```text
exact active lease principal and grant
∩ compiled preset operations
∩ current daemon ceiling
∩ hosted route/method/frame classification
∩ concrete action and target constraints
```

Reserved hosted role ids and labels are display metadata, not persisted role
rows or an authorization input. Generic IAM APIs cannot create or assign the
reserved hosted principal kind, grant source, or role ids.

### Tasks action wall

Tasks may create a session with daemon defaults or send `StartTask`,
`FollowUp`, or `Steer` to an explicit hosted-eligible session. A hosted-created
session is marked eligible by the daemon after it assigns the session id; no
wire field can set that marker. Trusted owner surfaces can mark an existing
session eligible without changing its autonomy.

Hosted task creation cannot override the project root, sandbox or approval
policy, execution shape, backend command, worktree behavior, display target, or
other launch policy. Leading slash-command forms are refused before supervisor
translation, and implicit "current session" targeting is unavailable.
Resume/fork/rewind/edit, sub-agent delegation, cancellation, agent
reconfiguration, autonomy changes, and approval answers are outside Tasks.

Tasks inherits the daemon's owner-selected autonomy defaults. If a task reaches
an approval wall, the hosted lane cannot resolve it. Operate may use the
ordinary session-lifecycle methods admitted by its preset, but its task and
message actions still require an explicit hosted-eligible target.

## Route, frame, and event projection

The immutable relay-ingress marker remains authoritative. Fleet-SNI or reachability-relay
traffic enters a protected route only after a valid hosted request proof; an
unticketed `/ws`, `/mcp`, cleartext demux, trusted-local resolution, and every
unproved protected request retain their discovery-only refusal.

Hosted HTTP methods, WebSocket frames, and tunnel methods have a second
compiled classification in addition to their IAM operation. Multiplexed
control messages are checked again at the concrete action and target. Unknown
methods and frames are denied until deliberately classified, and parity tests
cover the complete route/method/frame catalogs.

Hosted sockets also use an explicit outbound projection. They receive only the
session catalog/state, bounded usage/status, session conversation and
lifecycle events, agent-visible display readiness and authority state, and
events needed to keep those views current. Generic daemon log and audit
events, diagnostic report archives, Access/IAM state, peer state, settings,
autonomy controls, approval payloads, browser-workspace state, private
displays, app anchors, and lease-management records are omitted. The same
classifier filters bootstrap, replay, and live events; a new event kind is
absent until classified.

## Control and media reachability

The Connect relay carries daemon-terminated HTTPS and WSS, which is the
reachability path for tasks, session control, terminals, files, input commands,
and event delivery. The lease and its proof remain the authority boundary.

Display media uses WebRTC ICE separately. The SNI relay does not carry raw ICE,
so a NAT-obscured display requires either a successful direct ICE route or a
configured TURN server. Hosted bootstrap reports that media capability
separately and does not describe a control-plane-only relay as display
reachability. A production deployment must provision short-lived TURN
credentials before enabling hosted control for the advertised remote-display
experience.

## Capability bounds

| Condition | Enforced result |
|---|---|
| Feature switch off | Fleet and relay ingress remain anonymous discovery-only `role:none`. |
| Connect account or passkey assertion | No daemon principal or grant is resolved. |
| Raw hosted browser key without an approved lease | No protected-route admission. |
| Pending request | No IAM principal, grant, or control admission. |
| Pending request queue is full | A new request is refused; existing pending decisions remain present. |
| Copied lease document | No authority without a fresh proof by its bound tab key. |
| Reused proof nonce or WebSocket ticket | Refused by replay/one-use state. |
| Anonymous replay window or poll budget is exhausted | Public proof is refused without consuming active-lease replay capacity. |
| Wrong daemon, origin, method, path, key, or time window | Proof is refused. |
| Expired or revoked lease | New requests fail and live authority rechecks close the socket. |
| Ceiling lowered below a lease | The lease is revoked in the policy transaction. |
| Ceiling raised | Existing leases are unchanged; a new approval is required. |
| Persisted hosted role edited | Compiled preset evaluation preserves the operation set. |
| Generic IAM mutation names a hosted principal/grant/role | The generic mutation is refused. |
| Multiplexer reaches an unclassified action or target | The hosted action wall refuses dispatch. |
| New route, method, frame, or event lacks hosted classification | It remains unavailable. |
| Diagnostic session report is requested | The archive remains outside hosted View and is refused. |
| Tasks reaches an approval wall | The hosted lane cannot answer it. |
| Private user display is requested | The agent-visible-display boundary refuses it. |
| No direct ICE route or TURN | Media is reported unavailable; no broader transport is substituted. |
| Hosted policy/state cannot be loaded | Admission and live authorization fail closed. |

Every request creation, decision, lease issue/revoke/expiry observation,
ceiling change, and eligibility change produces a bounded IAM audit record.
A policy update that revokes several leases also emits one record for each
revoked lease. Audit records contain ids, actor, preset, and lifetime—not task
text, file content, signaling payloads, or private key material.
