# Dots powering Presence: integration research

Status (2026-10-02): **backend research and read-only diagnostic, not an
enabled Presence provider.** The requested direction is to use OpenAI's actual
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
| Dot profile and root mapping | `/tbo`, `/tbo/by-thread/{thread_id}`, `/tbo/{tbo_id}/root-thread` | Shapes inspected; HTTP access not established |
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

Direct read-only HTTP requests to the inspected profile routes, and the
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
Legitimate HTTP admission remains unresolved; no alternate origin, copied
cookie/proof, desktop impersonation, or automatic challenge solver was used.
Desktop-app access alone neither proves nor disproves that an Intendant
client can obtain the necessary provider admission.

The first implementation slice is an explicitly invoked controller command:

```bash
intendant presence-dots doctor
intendant presence-dots doctor --json
```

It needs the existing Codex ChatGPT subscription login but no running daemon,
model API key, or local ChatGPT GUI. The report deliberately excludes account
IDs, titles, previews, environment IDs, and raw provider bodies. A successful
report proves catalog/metadata access only. `presence_backend_enabled`,
`message_send_validated`, `dots_voice_validated`, and
`cloud_desktop_validated` remain false; it is not a setup-success signal for
the proposed product plugin.

The app's message path also has integrity preparation and an optional
app-attestation challenge. A working integration must use an available,
legitimate provider authentication/integrity contract; copying the app's
proofs, spoofing desktop identity, and bypassing those checks are not an
implementation plan. No public dots embedding/control API was found in the
official documentation reviewed on this date. All inspected interfaces above
are **private and version-sensitive**, not a supported API guarantee.

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
