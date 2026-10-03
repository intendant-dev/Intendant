# Dots powering Presence: integration research

Status (2026-10-03): **backend research, read-only diagnostic and explicit
separate test-dot preparation, not an enabled Presence provider.** The requested direction is to use OpenAI's actual
dots backend behind Intendant Presence, analogous to the Codex Cloud remote
compute integration. Recreating dots with another model, capturing the
ChatGPT window, or merely exposing Intendant's MCP tools to a dot is not the
same integration.

## What is already established

The [dots product documentation](https://learn.chatgpt.com/docs/dots/getting-started)
describes persistent cloud agents with their own computer and background work.
The cloud computer and an optionally connected local computer are separate
resources; local access requires the ChatGPT app to remain online. Viewing the
cloud desktop is distinct from taking control. See
[Computers and apps](https://learn.chatgpt.com/docs/dots/computers-and-apps).

Calls continue the dot's conversation, but ending a call does not end its
background work. The current documentation marks dot-initiated calls as a
future feature and texting as coming soon; they are not acceptance criteria
for this integration. See [Channels](https://learn.chatgpt.com/docs/dots/channels).
Notes belong to the dot, separately from ChatGPT saved memory. Pause does not
cancel independent tasks or schedules. See
[Tasks and memory](https://learn.chatgpt.com/docs/dots/tasks-and-memory) and
[Controls](https://learn.chatgpt.com/docs/dots/controls).

Intendant already has a **different** ChatGPT-subscription voice integration:
`[presence] live_provider = "chatgpt"` uses a hardened local Codex App Server
child. Its durable voice-thread lineage and closed dynamic-tool lane do not
make it a dots integration. Do not rename that lane or quietly substitute it
for a call to the persistent dot.

## Installed-app evidence

Read-only inspection used ChatGPT desktop **26.930.21537, build 12776**
(`com.openai.codex`). No application profile, cookies, signing keys, or
integrity proofs were extracted. The visible app had an active dot and a live
cloud-computer stream; its work was left untouched.

The shipped application's bundles establish separate protocol families:

| Lane | Observed app interface | Validation here |
|---|---|---|
| Cloud thread transport | Authenticated JSON-RPC WebSocket at `wss://codex-cloud-backend.chatgpt.com/` | Subscription login, initialize, thread list, and metadata read succeeded |
| Dot profile and root mapping | `/tbo/primary`, `/tbo/by-thread/{thread_id}`, `/tbo/{tbo_id}/root-thread` | Honest primary and by-thread GETs returned JSON 200; selection parsed, live profile ID refused the diagnostic's plain-atom assumption; full identity not yet established |
| Messages | `/messaging/rooms/{room_id}/messages`; also a cloud `turn/addUserMessage` extension | Inspected, not sent |
| Voice | `/tbo/{tbo_id}/voice/calls`, then call-specific attach and stop | SDP/Location lifecycle inspected, not called |
| Desktop | `/tbo/{tbo_id}/computer/sessions?thread_id=...` | SDP and observe/control lifecycle inspected, not connected |
| Lifecycle | Dot onboarding, pause/resume, and environment recreation | Inspected, not invoked |

The backend accepts an honest `codex-client.intendant` client identifier and
the existing Codex subscription login. The cloud catalog returned `aeon`
root threads, `aeon_child` worker threads, `dreaming` threads, and ordinary
`user` threads. **A root thread is not a stable dot identity**: multiple root
threads can belong to one dot. Count or display them only as candidate
threads until the profile/root mapping is validated.

The bundled CLI's generated experimental App Server schema had 167 request
methods and no dots/aeon/orbit control methods. `thread/startAeon` and
`turn/addUserMessage` are cloud extensions, not local App Server methods.

Earlier direct read-only HTTP requests to the inspected profile routes, and the
cloud metadata HTTP route, returned **403 with HTML**, while WebSocket
metadata reads succeeded. This does not establish whether the HTTP rejection
is an edge/network restriction, account scope, or another requirement. It
does not prove that the account lacks dots access. Do not silently work
around the rejection, invent route aliases, or advertise those HTTP lanes
as usable.

### Account routing and the HTTP gate

The follow-up read-only investigation used the bundled Codex App Server
`account/read` and `configRequirements/read` methods. The returned workspace
routing matched the selected subscription account and specified the same
`https://chatgpt.com` origin with `NO_CONSTRAINT`; a routing mismatch was not
the cause of this account's fixed-origin probe failure. The app normally
performs that discovery before HTTP and invalidates it on account changes.

The profile responses carry `cf-mitigated: challenge` with HTML, including
an unauthenticated baseline request. This is a provider-edge challenge,
not a dots eligibility result. The header's meaning is documented by
[Cloudflare](https://developers.cloudflare.com/cloudflare-challenges/challenge-types/challenge-pages/detect-response/).
Those observations are historical, not a permanent admission verdict. After
the startup account-binding fix, a live probe of commit `3e30f916` with
desktop **26.930.31428** and its bundled Codex CLI **0.160.0** returned
**JSON HTTP 200** from the pinned primary route. It downloaded no body, so
that observation alone established neither stable identity nor eligibility.
No alternate origin, copied cookie/proof, desktop impersonation, or automatic
challenge solver was used.
Desktop-app access alone neither proves nor disproves that an Intendant
client can obtain the necessary provider admission.

The first implementation slice is an explicitly invoked controller command:

```bash
intendant presence-dots doctor
intendant presence-dots doctor --json
intendant presence-dots doctor --http --json
intendant presence-dots doctor --profile --json
```

It needs the existing Codex ChatGPT subscription login but no running daemon,
model API key, or local ChatGPT GUI. The report deliberately excludes account
IDs, titles, previews, environment IDs, and raw provider bodies. A successful
report proves catalog/metadata access only. `presence_backend_enabled`,
`message_send_validated`, `dots_voice_validated`, and
`cloud_desktop_validated` remain false; it is not a setup-success signal for
the proposed product plugin.

`--http` additionally needs a current Codex CLI with workspace-routing
discovery. It starts an isolated, short-lived App Server child for only
`initialize`, `account/read` without token refresh, and
`configRequirements/read`; it does not widen the voice broker's method or
tool allowlists. A selected-account mismatch, unknown schema, or unhandled
residency/network requirement refuses the HTTP probe. Only the researched
normal account origin is supported in this diagnostic slice.

A fresh App Server can announce its initial `account/updated` auth mode
before its first account snapshot. The diagnostic counts and bounds those
startup notifications, then binds only the `account/read` result that matches
the held subscription credential and supported route. An update after that
binding still refuses discovery. A new CLI version alone does not guarantee
that the private workspace-routing fields are available; missing fields are
not permission to guess the account's origin.

Once account routing is verified, it sends one profile GET, with an honest
Intendant identity and no redirects, cookies, or native proofs. It examines
status and response headers only, not a profile or challenge body.
`provider_edge_challenge` specifically identifies `cf-mitigated: challenge`;
`forbidden_unknown` does not guess the cause of another 403. Even a JSON 200
is `profile_response_unvalidated`, with `eligibility_validated` and
`stable_dot_identity_validated` false. This command diagnoses the current
gate; it neither solves a challenge nor enables a dots feature.

`--profile` explicitly opts into bounded JSON metadata reads and implies
`--http`. The primary endpoint is a **selection envelope**, not a flat dot
profile. Its selected thread can be historical. The diagnostic resolves that
thread's profile, reads the stable profile's current root, resolves the
current root back to the same active `orbit` profile, and verifies the exact
cloud thread with `thread/read` and `includeTurns: false`. Both by-thread
profiles must name the same stable TBO profile ID; optional active-root and
room fields must agree when present. The selection's optional `aeon_id` is
separate, opaque internal metadata, not a TBO route ID. It is bounded, never
used in a URL or as profile/account/local authority, and compared only as part
of primary-selection consistency. The diagnostic then re-reads the
root and primary selection and refuses observed rotation or selection changes
during inspection, without automatic retry. These observations are not an
atomic provider lock or a durable Presence binding.

Only the pinned GET-only routes are available. Each successful JSON response
is capped at 256 KiB, including unknown-length/chunked bodies; body errors,
schema drift, redirects and challenges are sanitized. The HTTP/profile phase
has a 90-second total deadline. Reports contain booleans and named outcomes,
never dot/root/room IDs, names, transcripts or response bodies. A fully
consistent result can set `stable_dot_identity_validated` and report whether
the selected thread was historical and whether room metadata linked. It still
leaves eligibility, Presence activation, messaging, voice and desktop
validation false. Inspecting the primary selection does **not** authorize
repurposing it: product activation and E2E use an explicitly selected separate
test dot, without changing the account's primary selection.

Live acceptance of commit `9f716d9a` reached an authenticated JSON 200 but
returned `primary_selection_schema_changed`; stable identity was not
established. An earlier attempt conservatively refused a later account
notification before HTTP. Do not widen the parser or ignore account updates
just to obtain a green report. On metadata parse refusal, the diagnostic now
adds a closed `schema_observation`: phase, document/selection JSON types,
fixed allowlisted field names, their JSON types, and contract-valid booleans.
It exposes no scalar values, string lengths, unknown keys, IDs or raw bodies.
The headers-only `--http` probe and successful identity reports omit this
observation. It is evidence for reconciling a private schema, not permission
to activate Presence or to bind a dot from an ambiguous response.

The live field-type observation at commit `107ba59b` found the selection
envelope and all required fields valid; only `aeon_id` refused the diagnostic's
assumed URL-path-atom contract. The app's profile/root requests route by the
by-thread profile's `id`, not by that optional selection field. These namespaces
must not be equated. The follow-up keeps stable identity rooted in the two
matching active profiles, their root mapping and exact cloud metadata; it
preserves the bounded opaque `aeon_id` solely to detect primary-selection
changes. This reconciles the observed private contract, not a grant to bind
Presence, route by arbitrary identifiers, or weaken account/root/room checks.

The live probe at `f043c644` passed selection parsing and reached the by-thread
profile with JSON 200. Its `aeon_kind`, status, root and room fields met the
contract; only `id` refused our assumed plain URL-atom grammar. The app's request
helper substitutes path-parameter strings directly into its template. Intendant
does **not** copy that unsafe interpolation: TBO IDs are bounded opaque resource
strings (512 bytes, no controls, no leading/trailing whitespace or exact `.`/`..`)
and its pinned URL builder adds them as a single encoded path segment. Slash,
query, fragment and percent characters cannot replace the origin, base route or
fixed suffix. Equality checks compare the original IDs, not guessed aliases.
This encoding fix still needs live acceptance of the full identity chain.

The app's message path also has integrity preparation and an optional
app-attestation challenge. A working integration must use an available,
legitimate provider authentication/integrity contract; copying the app's
proofs, spoofing desktop identity, and bypassing those checks are not an
implementation plan. No public dots embedding/control API was found in the
official documentation reviewed on this date. All inspected interfaces above
are **private and version-sensitive**, not a supported API guarantee.

## Explicit separate test-dot preparation

The owner selected a **separate test dot** for E2E. This is an experimental,
explicitly invoked preparation command, not product setup or Presence activation:

```bash
intendant presence-dots test-dot create --json
intendant presence-dots test-dot status --json
intendant presence-dots test-dot reconcile --json
```

`create` first verifies the held subscription's routing and the complete primary
profile/current-root chain. It records a private, synced intent under the Intendant
state root at `presence-dots/test-dot.json`, with a unique generated test label,
before sending **one** `POST /tbo`. Its closed body uses `create_thread: true`,
`create_additional: true` and `should_initialize: true`. This can initialize the
new dot and its cloud resources; it never PUTs or DELETEs primary selection,
repurposes an existing dot, sends a message, places a call or takes desktop control.
The request uses honest Intendant identity, no redirects/cookies/native proofs,
and an explicitly disabled HTTP retry policy. The app's observed creation deadline
is nine minutes; the command gives that request the same bounded deadline.

An advisory cross-process lock prevents concurrent preparations. Unix journal
directories/files are created 0700/0600, and symlinks, hard-linked state files,
loose Unix permissions, corrupt/oversized/unknown journal formats refuse instead
of resetting intent. The existing synced staging seam preserves atomic reads;
Unix also syncs the directory entry before the POST. Tests pass explicit temp roots
and never read a real login, home, daemon store or provider endpoint.

The journal's continuity hash includes the selected account and login subject,
but never tokens or raw account/subject claims. Local JWT payload decoding is
**unverified continuity metadata, not authority**: every operation still needs
fresh matching App Server account routing and authenticated provider reads.
Token refresh with the same subject/account preserves the hash; observed account
or subject changes refuse. These observations are not an atomic account lock.

A response, including an HTTP rejection, is not enough to silently discard an
attempt. Transport loss, unexpected status/media/body/identity, cancellation or
restart leaves existing intent; `create` refuses another POST while that journal
exists. Do not erase an uncertain journal to retry. `reconcile` performs reads only:
it scans at most four 25-profile pages for one exact, unique journal-generated
label, then resolves that candidate's stable profile/current-root/room and exact
cloud thread metadata. Missing, ambiguous, incomplete or drifting evidence stays
unconfirmed and never triggers another creation attempt.

Binding requires a distinct stable TBO ID and root from the protected primary,
the exact generated label, matching room metadata, a stable current root and an
unchanged primary-selection fingerprint. Root rotation between the creation
receipt and later verification is allowed only through the same stable dot;
observed rotation during verification refuses. Primary or identity/room conflicts
require owner review, with no automatic restoration, deletion, pause or other
corrective effect. The primary dot is never a fallback.

`status` reads the journal without auth, provider access or lock/directory creation.
It distinguishes a **recorded historical binding** from one **live-verified now**.
Even a freshly verified separate dot leaves Presence, messaging, voice and desktop
flags false. No product plugin should show setup success from this preparation.
Creation and all full-provider lanes still require live acceptance.

## Implementation sequence and acceptance

1. **Read-only transport diagnostic.** Reuse controller-held subscription
   credentials; identify as Intendant; allow only initialize, thread list,
   metadata read, and the initialized notification. Bound sizes/timeouts,
   reject server-initiated requests, never emit raw credentials or RPC bodies,
   and distinguish a reachable catalog from a usable Presence backend.
2. **Stable identity and admission.** Resolve account/user/dot/root identities;
   bind explicitly to a dedicated dot rather than silently selecting the
   account's primary dot. Establish the legitimate HTTP/integrity lane. Prove
   an isolated message send, acknowledgement, response, reconnect, root
   rotation, and outcome-unknown handling without duplicate sends. Never
   interrupt or repurpose an existing busy dot for a test.
3. **Presence text and worker reconciliation.** Preserve provider-owned
   conversation history instead of replaying `ChatProvider` transcripts into
   it. Map background dot work and Intendant workers as separate task
   identities. Expose Intendant's narrow, authorized tool lane; remote text,
   notes, and tool results are data, not owner approval or local IAM.
4. **Voice to that same dot.** Bridge the dot-specific SDP/call lifecycle,
   retain identity across text/calls, stop microphone delivery immediately
   on disconnect, and distinguish call stop from dot/task pause. Keep the
   existing subscription-voice broker's authority gates unchanged; a dot
   call without verifiable user-role evidence cannot authorize its
   trust-critical tools.
5. **Cloud desktop.** Add a provider-owned display source rather than a
   federated daemon or Codex build-worker lease. Observation is the default;
   take-over and return-control are explicit, separately authorized owner
   actions. Test blur, socket loss, reconnect, and input-authority revocation.
6. **Optional product activation and E2E.** Default off, separately from Codex
   remote compute. Derive capability/UI availability from validated live
   lanes. Test missing auth, unsupported plan, denied HTTP, schema drift,
   account changes, concurrent viewers, and daemon restart. Do not show
   "connected" merely because the WebSocket opens.

The first slice intentionally creates no dot, sends no prompt, places no
call, takes no desktop control, grants no daemon authority, and changes no
Presence configuration. It is not the completed feature.
