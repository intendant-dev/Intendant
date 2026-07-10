# Peer Federation

Intendant can federate with **other autonomous agent daemons as equals** — other
Intendants, A2A-speaking peers, OpenClaw gateways, MCP-server-shaped peers. A
federated peer is a sibling, not a subordinate: the two daemons exchange events,
delegate tasks by capability, and — between Intendants — share each other's
displays across machines.

This chapter covers what federation is and how it differs from external agents,
the Agent Card and discovery model, the peer actor/registry/coordinator layer,
the agent-facing control surface (`ctl peer` + MCP tools), the transport stack
(native WebSocket, multi-URL probing, cert pinning), the cross-machine display
path, and dashboard TLS/mTLS setup. For the local display pipeline
those federated displays plug into, see [Display Pipeline](./display-pipeline.md).

Peer federation is **not** the same security domain as a browser dashboard login.
Hosted Connect passkeys and browser mTLS client certificates authenticate a
human/client route to one daemon and are root dashboard access for the owner
today. Peer federation authenticates a daemon route to another daemon and should
use peer-scoped mTLS identities plus peer profiles. Future coworker/team access
belongs in user-scoped IAM unless the federation trust model is deliberately
expanded.

The dashboard now describes both domains with the same access vocabulary:
principal, target, grant, policy, and transport. In that model a peer daemon is
a principal with a peer-profile grant to a target daemon, carried over
daemon-to-daemon mTLS plus optional browser-to-peer DataChannels for interactive
views. That shared vocabulary does not make peer mTLS a human login mechanism;
it only lets the Access UI compare peer grants with user/client grants without
conflating them.

## Federation vs. External Agents

These are two orthogonal relationships, and they compose:

| | **Peer federation** (`src/bin/caller/peer/`) | **External agents** (`external_agent`) |
|---|---|---|
| Relationship | Peer / peer (A2A-shaped) | Master / worker (ACP-shaped) |
| Mental model | "I federate with a peer daemon" | "I spawn a process and give it a task" |
| Right for | OpenClaw, Hermes, Letta, **another Intendant** | Codex, Claude Code, Aider, goose |
| Lifecycle | Connect to an already-running daemon | Spawn and supervise a child process |

A peer Intendant can itself supervise a Codex subprocess via its own
`external_agent` layer while being driven from this side as a `peer` — the two
layers don't know about each other.

## Agent Card and Discovery

Every Intendant daemon serves an **Agent Card** at
`/.well-known/agent-card.json` (`peer/card.rs`). The card is the single source of
truth for *who this peer is, what it can do, how to reach it, and how to
authenticate*:

```json
{
  "id": "intendant:nicks-mac",
  "label": "nicks-mac",
  "version": "0.x.y",
  "git_sha": "abc1234",
  "transports": [
    { "type": "intendant-ws", "url": "wss://192.168.1.42:8765/ws" },
    { "type": "intendant-ws", "url": "wss://node.tail-abcd.ts.net:8443/ws" }
  ],
  "capabilities": [
    { "kind": "display" },
    { "kind": "computer-use" },
    { "kind": "voice" }
  ],
  "auth": { "transport": { "scheme": "none" } }
}
```

Key fields:

- **`id`** — a stable opaque `PeerId` (`intendant-core`'s `peer_id.rs`). `id.kind()` is the source of
  truth for the daemon kind (`intendant`, `a2a`, `openclaw`, `mcp`, …); there is
  no separate `kind` field, by design.
- **`transports`** — one or more addresses **in preference order** (highest first).
  A single Intendant typically advertises its native WebSocket *and*, once
  shipped, an MCP/A2A endpoint, all in one card. Transport kinds:
  `intendant-ws` (native), `a2a`, `mcp` (with a nested transport kind), and
  `openclaw-ws` (with a role).
- **`capabilities`** — what the peer *offers* as services: `display`, `voice`,
  `phone`, `computer-use`, `knowledge`, `recording`, `task-delegation`,
  `message-relay`, or a string-tagged `custom:<name>`. The coordinator routes work
  by matching against this list.
- **`auth`** — what the peer *requires* of inbound connections (see
  [Authentication](#authentication) below).

A peer that advertises something an older build doesn't recognize (a future
transport, capability, or auth scheme) deserializes that one position to an
`Unknown` fallback variant rather than failing the whole card parse; the registry
then filters `Unknown` out when picking a transport.

### Advertised endpoints — `--advertise-url`

A daemon's card lists the URLs *peers should try*. By default the gateway
auto-detects its listener URL, but a NAT'd / tunneled / multi-homed daemon must
advertise what's actually reachable:

```bash
intendant --web --advertise-url wss://192.168.1.42:8765/ws \
                --advertise-url wss://node.tail-abcd.ts.net:8443/ws
```

`--advertise-url` is repeatable; each occurrence appends one URL in preference
order. When non-empty, the CLI list replaces the `[server.advertise]` config
list at config-merge time. The Agent Card then prepends those operator URLs
ahead of auto-detected URLs; auto-detected entries are always appended as
fallbacks. The merged list also seeds the **primary-relay TCP fallback** for
cross-machine display (see below).

## The Peer Actor / Registry / Coordinator Model

```
            ┌───────────────────────────────────────────────┐
            │                   Coordinator                  │   capability-based
            │   TaskRequest{ required_capabilities } ──► pick │   routing
            └──────────────────────┬────────────────────────┘
                                   │ delegate_task
            ┌──────────────────────▼────────────────────────┐
            │                  PeerRegistry                  │   HashMap<PeerId, PeerHandle>
            │   add_peer: fetch card → pick transport → spawn │
            └──────────────────────┬────────────────────────┘
                                   │ one per peer
            ┌──────────────────────▼────────────────────────┐
            │   PeerHandle  ◀──watch── ConnectionState        │
            │      │ commands (mpsc)        events (broadcast) │
            │      ▼                                           │
            │   per-peer actor task                           │
            │   connect → main-loop → reconnect (backoff)     │
            │      │ owns                                      │
            │      ▼                                           │
            │   Box<dyn PeerTransport>  ───────► the wire      │
            └─────────────────────────────────────────────────┘
```

- **`PeerTransport`** (`peer/traits.rs`) is the single transport trait. It accepts
  the sender side of an `mpsc::Sender<PeerEvent>` at construction and pushes
  events as they arrive off the wire — there's no "take the stream once"
  awkwardness. Outbound work is a transport-neutral `PeerOp` envelope
  (`SendMessage`, `DelegateTask`, `CancelTask`, `QueryTaskStatus`,
  `InvokeCapability`, `ResolveApproval`, `WebRtcSignal`,
  `PeerFileTransferSignal`, `PeerDashboardControlSignal`); a
  `TransportFeatures` struct declares which verbs a transport class supports.
  The WebRTC, file-transfer, and dashboard-control signaling relays are
  authorized as `peer.use`, separate from peer inspection and management; the
  receiving peer applies its own grants to the tunnel contents.
- **The per-peer actor** (`peer/actor.rs`) owns the transport by value and runs a
  `connect → main-loop → reconnect` state machine with **indefinite exponential
  backoff** (500 ms initial, 30 s cap, jitter, reset on every successful connect).
  Inbound events fan out **durable-first**: to a bounded log sink (must not drop —
  if it's slow the actor pauses, transitively back-pressuring the wire), then to a
  lossy broadcast for UI subscribers. Commands are only processed while
  `Connected`; ones that arrive mid-reconnect wait in the bounded command channel.
- **`PeerRegistry`** (`peer/registry.rs`) owns the `HashMap<PeerId, PeerHandle>`.
  `add_peer` fetches the card from `/.well-known/agent-card.json`, picks the first
  supported `TransportSpec`, constructs the transport, and spawns the actor. If no
  supported transport is advertised it fails cleanly with `PeerError::CardFetch`.
- **`Coordinator`** (`peer/coordinator.rs`) sits on top and does **capability-based
  routing**: given a `TaskRequest` with `required_capabilities`, it picks the first
  eligible peer — one that is `Connected` *and* whose card advertises every
  required capability — in lexicographic `PeerId` order (deterministic, so
  idempotent retries route to the same peer) and delegates via the handle.

### Per-peer sessions — the folded `SessionInfo` rail

A peer's `/ws` stream already narrates its sessions event by event
(`session_started`, `session_identity`, `session_relationship`, per-session
`status`, `session_goal`, `session_vitals`, session-scoped `usage` and
approvals). The consuming side folds that stream instead of extending the wire:
`PeerSessionFold` (`peer/upcast.rs`, shared by both upcasters) merges the
events into per-session [`SessionInfo`] snapshots — label, source, phase,
parent/relationship, tokens, `needs_approval` (derived from the pending-
approval set), goal, and vitals, every enrichment field optional — and emits a
`PeerEvent::SessionUpdated` carrying the full merged snapshot whenever
something actually changes. `SessionStarted` announces, `SessionUpdated`
evolves, `SessionEnded` retires; consumers fold the first two identically by
`session_id`, so the stream is idempotent and loss-tolerant.

Because fold updates **upsert**, a primary that connects (or reconnects) while
the peer's sessions are already running learns them from their next live event.
Three bootstrap channels close the remaining late-join gaps: the transport
peeks the `/ws` `state_snapshot` frame to learn the peer daemon's own primary
session id (stamped `is_primary`; renderers merge that session into the peer's
daemon node instead of drawing it twice); the gateway re-sends the latest
change-detected per-session state lines (`session_vitals` / `session_goal` —
which fire on change only, so an idle repo's git vitals would otherwise never
recur for a late joiner; this also fixes browser refresh on an idle daemon,
tunnel clients included via `api_cached_bootstrap_events`); and when the peer
serves a `log_replay` frame, the transport folds it through a replay-only lane
(`upcast_replayed`) that updates session state without re-firing historical
messages or activities as live events. Phases fold from the lifecycle events
sessions actually emit (`TurnStarted`/`AgentStarted` → working,
`DoneSignal`/`TaskComplete` → done) — native daemon-lane sessions have no
per-session `status` rail, and a completed session **lingers as done** rather
than retiring, because the persistent daemon keeps it resumable (same
semantics as local session windows); `SessionEnded` arrives on explicit stop.
The fold is bounded (`MAX_TRACKED_PEER_SESSIONS`, oldest-started evicted)
since a peer could announce sessions forever without ending them.

The per-peer actor mirrors the folded stream into a watch-published sessions
view (cleared on disconnect — the fold is connection-scoped, and stale entries
would ghost if the peer restarted), and `PeerSnapshot` carries it as
`sessions`, so `GET /api/peers` seeds a late-joining dashboard and every
pushed `peer_state_changed` snapshot replaces the row's list losslessly (same
source of truth as the live events). The dashboard renders them as
display-only nodes orbiting the peer's host in the Station scene (capped per
peer — the scene is a bounded constellation; the peer's own dashboard is the
exhaustive list). Action pills stay off peer session nodes in v1: the
session-action handlers assume local session ids. None of this changes what a
peer exposes — the sessions rail is derived entirely from the event stream the
peer already sends under its access profile.

### Per-peer displays — the folded availability rail

Displays ride the same consumer-side pattern. The peer's `/ws` already
announces display availability (`display_ready`, `display_resize`) and
retirement (`display_capture_lost`, `user_display_revoked`), and its gateway
replays `display_ready` for every live display when a connection —
including a federation transport — (re)attaches. `PeerDisplayFold`
(`peer/upcast.rs`, embedded in both upcasters) folds that stream change-only
into `PeerEvent::DisplayReady { display_id, width, height }` /
`PeerEvent::DisplayLost` — repeats from the on-connect replay are silent, a
geometry change re-announces. Historical `log_replay` frames deliberately do
**not** fold displays: replayed availability is not current availability;
the on-connect replay carries the live truth as real wire events.

The per-peer actor mirrors the fold into a watch-published displays view
(cleared on disconnect, same ghost-avoidance as sessions), and
`PeerSnapshot` carries it as `displays`, so `GET /api/peers` — and therefore
the `list_peers` MCP tool — seeds late joiners and every pushed snapshot
replaces the row losslessly. On the dashboard each known peer display gets
its own Station chip (`label · :id`) wired to the existing federated WebRTC
view path, and the Activity → Log rail narrates transitions with the display
id and geometry (`display :99 ready (1920x1080)`). An agent that brings up a
display on a peer is therefore *discoverable* the moment the peer announces
it — no display id needs to be guessed or typed.

## Agent-Facing Peer Control (ctl + MCP)

Federation is not a dashboard-only surface. An agent-facing control surface
exposes the delegation verbs *and* direct computer use on peers in three
shapes with identical semantics:

- **MCP tools** on the daemon's `/mcp` surface — `list_peers`,
  `peer_send_message`, `peer_delegate_task`, plus the direct-CU trio
  `peer_list_displays`, `peer_take_screenshot`, `peer_execute_cu_actions`.
- **CLI** — the `intendant ctl peer` verb group (distinct from the top-level
  `intendant peer …` pairing commands below, which provision relationships
  rather than acting through them):

  ```bash
  intendant ctl peer list
  intendant ctl peer message <peer-id> "text" [--session ID]
  intendant ctl peer task <peer-id> "instructions" [--context JSON]
  ```

  and, one level up, a global `--peer <id>` flag that points *any* ctl
  subcommand at a federated peer's `/mcp` directly (see below).

- **Native agent tool** — a `peer` tool with actions `list` | `message` |
  `task` | `displays` | `screenshot` | `cu`, so supervised and native agent
  sessions reach federation without the dashboard. Peer screenshots come
  back as image attachments in the tool result — the agent literally sees
  the peer's screen in its conversation.

`list_peers` returns the same peer snapshot list as `GET /api/peers` — id,
label, connection state, capabilities, and the folded per-peer `sessions`
and `displays` rails above. `peer_send_message` (`peer_id`, `message`, optional `session`)
sends a message to the peer's agent. `peer_delegate_task` (`peer_id`,
`instructions`, optional `context`) delegates a task and returns a `task_id`.

Delegation keeps the sibling-not-subordinate contract: a delegated task
executes on the peer's machine, by the peer's own agent, under the peer's own
autonomy/approval policy. The caller gets a `task_id` to follow up on, not a
supervised child process.

All of these are IAM-gated at call time (`mcp_tool_operation`), mirroring the
classification of the HTTP routes they parallel, under one rule: reading
*local* federation state is inspection; causing traffic *on* a peer is use.
`list_peers` requires `peer.inspect` (same as `GET /api/peers`); everything
else — `peer_send_message`, `peer_delegate_task`, and the direct-CU trio —
requires `peer.use` (same as `POST /api/peers/{id}/message` and `…/task`).
`peer.use` is the gate because acting through a peer delegates **this
daemon's peer identity** — the caller acts with the daemon's peer
credentials, and what is actually permitted is decided by the *receiving*
peer's grants for this daemon, not by anything the caller holds locally (the
same split the signaling relays above ride).

### Direct computer use on peers — the `/mcp` side-channel

The direct-CU operations (`peer_list_displays`, `peer_take_screenshot`,
`peer_execute_cu_actions`; `peer` tool actions `displays`/`screenshot`/`cu`)
do not ride the WebSocket transport at all. Each is one stateless JSON-RPC
`tools/call` POST to the **peer's** `/mcp` endpoint (`peer/mcp_http.rs`),
authenticated with the exact identity the federation transport uses: the
registry retains the assembled `TransportCredentials` (bearer, parsed pin
bytes, mTLS client identity) on the `PeerHandle` at spawn time, and the
side-channel builds its pinned rustls client from them. The endpoint is the
card's advertised streamable-HTTP MCP transport when present, else derived
from the WS transport URL (`wss://host/ws` → `https://host/mcp` — same
gateway, same origin). Requests carry the `x-intendant-peer` marker, so an
unresolvable client cert is a hard 403, never an anonymous fallback.

On the receiving side this is the ordinary peer-principal path: the peer's
`/mcp` gate binds the client cert to the IAM profile the peer's owner granted
this daemon, then classifies the inner tool per its own gate —
`take_screenshot`/`list_displays` need **display view** (`read-only-display`
or better), `execute_cu_actions` needs **display input** (`peer-operator` /
`peer-root` only). `read_screen` is display-view class too — an element tree
reveals screen content just as a screenshot does — so `intendant ctl --peer
<id> cu elements` reads the peer's frontmost UI element tree under the same
display-view grant when the peer's platform accessibility stack is available
(deliberately no `peer_read_screen` twin; the generic side-channel covers
it). A denial comes back as a structured `isError` tool result
with the peer's diagnostic text, which every caller surface passes through
verbatim.

A peer is never an *owner surface* on the target daemon: targeting the
peer's real desktop (`user_session`) additionally requires the target
daemon's own standing user-display grant, enforced at the CU executor
([Computer Use](./computer-use-and-audio.md#display-targets)). The peer
profile authorizes the operation class; the grant is the target owner's
opt-in to their desktop specifically — a `peer-operator` profile alone
reaches agent virtual displays, not an ungranted user session.

Screenshot and CU replies carry real MCP image content blocks; the native
tool folds them into the conversation as image attachments
(`add_tool_result_with_images`), and the MCP twins re-emit them as image
blocks. This is why agent-driven CU on peers needs **no WebRTC**: capture is
request/response. The browser WebRTC path below remains the human viewing
surface (with the stage-1 display chips for discovery), and delegation
(`task`) remains the preferred verb when the peer's own agent can do the
work — reach for direct CU when this agent needs to see or drive the peer's
screen itself.

The same side-channel is reachable from the CLI with **no daemon in the
loop**: `intendant ctl --peer <id> …` resolves the `[[peer]]` entry from the
project's `intendant.toml` first and, when the project yields no match (or
has no config at all), from the user-level `~/.intendant/peers.toml`
(`$INTENDANT_HOME/peers.toml` under an overridden state root) — peers are
machine-scoped identities, so a pairing recorded there works from any
working directory. Both layers use the same matching rules (label
case-insensitive, card_url host, exact card_url, or the suffix of an
`intendant:<label>` peer id); a project match always wins, and only
`ctl --peer` reads the user-level file — daemon boot federates from the
project config alone. Resolution then derives the `/mcp` endpoint from the
card_url origin and builds the same pinned mTLS client — explicit
`client_cert`/`client_key` first (the peer-boot pairing rule; half-set
config errors out), else the installed access identity for TLS targets.
Every existing ctl subcommand then drives the peer:

```bash
intendant ctl --peer dell display screenshot --output peer.png
intendant ctl --peer dell cu actions --actions '[{"type":"click","x":100,"y":200}]'
```

`--peer` conflicts with `--url`, silently overrides the env URL/port, sends
the configured `bearer_token` and the `x-intendant-peer` marker, and appends
no `session_id`/`managed_context` — local session scoping is meaningless
cross-daemon. Because this path reads key material from disk rather than
acting as a session, it is not gated by the local daemon's IAM at all; the
peer's profile for this daemon is the sole authority (exactly like the
`intendant peer request/approve` pairing CLI).

## Transports

Phase 1 ships the native Intendant↔Intendant transport. A2A, OpenClaw, and
MCP-as-peer transports slot in as sibling modules behind the same `PeerTransport`
trait.

### Native WebSocket — `IntendantWsTransport`

Speaks Intendant's own `/ws` protocol — the highest-fidelity path between
Intendants. The full `AppEvent` stream is upcast into the lean transport-neutral
`PeerEvent` vocabulary by `peer/upcast.rs` (there is deliberately no
`Native(AppEvent)` escape hatch). HTTP(S) base URLs for card discovery are derived
from the WebSocket URL (`ws://…/ws` → `http://…`).

### Multi-URL probing — `MultiTransport`

When a card advertises several reachable addresses (LAN IP, Tailscale tailnet IP,
port-forwarded WAN URL), `MultiTransport` (`peer/transport/multi.rs`) walks the
candidates **in card order** and uses the first whose `connect()` succeeds. Every
reconnect re-walks the whole list from the top, so if a more-preferred path comes
back online while running on a fallback, the next reconnect picks it up. Before any
candidate connects, `features()` reports the *union* of all candidates' features
so coordinator-level checks don't prematurely reject an op a candidate could
support once connected.

### Cert pinning over mTLS — `PinnedMutualTls`

`access/pinning.rs` provides a custom rustls `ServerCertVerifier` that
accepts a presented server cert **iff its SHA-256 fingerprint matches one of the
operator-supplied pinned values**. This is defense in depth on top of (or instead
of) plain mTLS: mTLS alone trusts every cert a trusted CA signed, so a CA
compromise or a leaked wildcard lets an attacker impersonate the peer. Pinning the
exact expected cert (or a rotation set) closes that gap.

The pinned peer advertises its fingerprints in its card under
`auth.transport = PinnedMutualTls { server_cert_fingerprints }`; connecting daemons
build a `PinnedFingerprintVerifier` and use it for **both** the WebSocket connect
and the agent-card HTTP fetch. Fingerprints are lowercase or uppercase hex, with
optional `:` separators (the OpenSSL format). Pinning replaces only the cert-path
check — the TLS handshake **signature** is still verified normally, so an attacker
who steals the cert bytes but not the private key still fails the handshake.

## Cross-Machine Display

Federated Intendants can share each other's displays in the browser. The defining
property:

> **The primary is a signaling middleman only — encoded video flows
> browser ↔ peer directly, never through the primary.** A primary-relay TCP path
> exists strictly as a fallback when no direct path can be formed.

```
                signaling (SDP/ICE)
   browser ───────────────────────────► primary ───────────────► peer daemon
      │         ws (/ws + PeerOp::WebRtcSignal)   IntendantWs        │
      │                                                              │
      │              direct encrypted media (WebRTC, via TURN)       │
      └──────────────────────────────────────────────────────────────┘
                          (primary not in this path)

   ── fallback when no direct path forms ──
   browser ──RFC4571/STUN-framed TCP──► primary ──relays bytes──► peer daemon
```

### Signaling

WebRTC signaling is carried over the federation transport as
`PeerOp::WebRtcSignal` (primary → peer) and `PeerEvent::WebRtcSignal`
(peer → primary), both scoped by `{ display_id, session_id }` (`peer/event.rs`).
The **browser is the offerer** — mirroring the local browser→daemon flow:

- **Primary → peer** carries the browser's `Offer` and trickled `IceCandidate`s.
  The `Offer` may include `advertise_tcp_via_url` (the URL the operator typed into
  "Add Peer"), which the peer uses to derive its ICE-TCP host candidate and
  register against its own `TcpPeerRegistry`.
- **Peer → primary** carries the peer's `Answer` and trickled `IceCandidate`s.

`session_id` is a browser-generated UUID, so multiple sessions to the same display
don't collide and a stale tab can't interfere with a fresh one. Unknown signal
kinds parse to an ignored `Unknown` variant for forward compatibility.

### Federated Browser Workspaces

Browser workspaces are the browser-specific sibling of shared displays: they
represent a concrete browser surface that an agent can control through CDP,
Playwright, Agent Browser, or a streamed-display fallback. The local registry
models `placement = local | peer` and carries the target `peer_id`, but remote
peer placement intentionally fails closed until the federation transport has a
first-class browser-workspace operation.

The intended federation rule is the same one used for display input authority:
the peer that owns the browser process is the source of truth for leases. If two
agents on one primary try to access a browser workspace hosted by another peer,
or if agents on multiple peers race for the same remote browser, the owning peer
serializes `acquire_browser_workspace` and rejects the second holder unless the
caller uses an explicit force-takeover. Local same-machine workspaces can use
CDP/Playwright semantics for low-latency automation; cross-machine users can
fall back to the display/shared-view streaming path when local browser control is
not possible.

### Direct media and the TCP-relay fallback

Once signaled, the browser forms a **direct** WebRTC media path to the peer,
typically through TURN: when a TURN server is configured in `[webrtc].ice_servers`
the federated path pins the browser to `iceTransportPolicy: 'relay'` and both ends
can allocate on the configured coturn (without a TURN server the policy is left at
its default). When no direct path can be formed, a **primary-relay TCP fallback**
kicks in (`display/webrtc/tcp_mux.rs` `TcpRelayRegistry`):

1. As the peer's `Answer` flows back through the primary, the primary parses the
   peer's ICE ufrag and resolves the peer's advertised URL to a `SocketAddr`,
   registering `(ufrag → addr)` in a `TcpRelayRegistry`.
2. The primary injects a relay TCP candidate (pointing at its own HTTP port) into
   the Answer SDP alongside the peer's direct candidates.
3. If the browser ends up using that candidate, the connection lands on the
   primary's HTTP port with the peer's ufrag in its first STUN USERNAME. The
   primary finds no local match in `TcpPeerRegistry` but a hit in
   `TcpRelayRegistry`, dials the peer, re-frames the peeked first frame, and
   shuttles bytes bidirectionally between browser and peer.

The relay multiplexes onto the same HTTP port as the dashboard (the same accept-loop
peek that distinguishes HTTP / WebSocket / local ICE-TCP grows a relay branch), so
it needs no extra port-forwarding.

### Federation codec policy — `federation_allow_h264`

H.264 over a lossy TURN-relayed path is fragile: a full-resolution 2.5 Mbps stream
produces a seed IDR of hundreds of RTP packets, and at ~17% loss the probability of
reassembling every packet is effectively zero, so the stream never bootstraps. By
default federation therefore **pins VP8** in the browser:

```toml
[webrtc]
federation_allow_h264 = false   # default: VP8-pinned over relays
```

Setting `federation_allow_h264 = true` lets the federated path negotiate the
peer's H.264. To survive lossy relays, that H.264 uses a dedicated **loss-resilient
shape**: a quarter-resolution layer at a capped bitrate (`LayerSpec::single_federated`,
RID `fed`, ~250 kbps — a small ~17-packet IDR with ~24% intact-arrival odds),
combined with periodic IDRs and same-SSRC NACK retransmission. The federated H.264
encoder keys a *distinct* pool slot (`EncoderId { H264, fed }`) so it can never be
handed a full-resolution H.264 encoder a local viewer spawned, or vice versa. The
local, same-machine display path is unaffected by this flag and uses the full
pipeline from [Display Pipeline](./display-pipeline.md).

A transport must support relaying `WebRtcSignal` frames (`TransportFeatures::webrtc_signal`)
for the federated display path to work; the dashboard hides the "View display"
affordance for peers whose transport can't carry it.

### Input authority on federated displays

The peer remains the **single source of truth** for who holds each of its displays.
A unified authority registry on the peer arbitrates both local WebSocket holders
and federated WebRTC holders with the same last-taker-wins rules, distinguishing
provenance explicitly (`LocalWs` vs. `FederatedWebRtc`, never inferred from string
shape). Federated authority requests/state ride a dedicated
`display_input_authority` data channel on the federated connection; federated input
events reuse the existing `control` / `pointer` channels with raw `InputEvent` JSON.
The **peer-side gate is the security boundary** — input arriving without authority
is dropped silently at the peer regardless of what the browser believes; the
browser-side check is UX only. The full protocol is in
[`docs/design-federated-input-authority.md`](https://github.com/intendant-dev/Intendant/blob/main/docs/design-federated-input-authority.md).

## Dashboard Access and TLS

Two independent mechanisms expose the dashboard securely; they can be used
together or separately.

### Native HTTPS/WSS / mTLS

`web_tls.rs` serves the `--web` dashboard over HTTPS/WSS directly, with no proxy,
on **every platform including Windows**. It's pure-Rust (`rustls` + `rcgen`, both
on the `ring` crypto provider — no OpenSSL anywhere in the tree). The gateway's
accept loop peeks the first bytes of each connection and, on seeing a TLS
ClientHello, wraps the socket in a `TlsAcceptor` before handing the decrypted
stream to the existing HTTP/WebSocket handling.

```bash
intendant                                      # default: mTLS with access certs
intendant --tls                                # TLS-only; installed access certs when present, else self-signed
intendant --no-tls --bind 127.0.0.1           # explicit local plaintext/debug escape
intendant --tls-cert chain.pem --tls-key key.pem   # explicit PEM (implies --tls)
```

`--tls-cert` / `--tls-key` must be supplied together; supplying either implies
`--tls`. With no transport flag, Intendant requires browser/client certificates
against the installed access CA. `--mtls` makes that default explicit, and
`--mtls-ca` or `[server.mtls] ca` overrides the client CA.

Native HTTPS/WSS is also the direct way to make a remote dashboard origin a
browser secure context when you need Station WebGPU, microphone/camera, browser
screen capture, or stricter clipboard APIs. Use a trusted certificate; merely
clicking through a self-signed certificate warning is not a reliable way to
unlock these browser APIs. See
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).

### Native access certs — `intendant access`

`src/bin/caller/access/` creates the certificate material used by native default
mTLS and TLS-only mode: a per-user access CA, server certificate, client
identity, and strict HTTPS enrollment server. Access clients (phones, tablets,
other boxes) can
then reach the dashboard over HTTPS authenticated by a **client certificate**.
Cert generation is pure-Rust (`rcgen` + RustCrypto `rsa` + `p12-keystore`); new
cert material uses RSA-2048 with SHA-256 signatures so Apple
configuration-profile certificate payloads match Apple's documented
compatibility path. Subcommands:

| Command | Action |
|---|---|
| `intendant access setup` | Generate CA + server/client certs and start the strict HTTPS enrollment server |
| `intendant access recert` | Re-issue certs |
| `intendant access remove` | Remove the per-user access cert store |
| `intendant access list` | List issued client certs |
| `intendant access serve-certs` | Run strict HTTPS enrollment for importing `ca.crt`, client `.p12`/`.pfx`, or Apple `.mobileconfig` onto devices |

```bash
intendant access setup --name nicks-mac --port 8765
```

`--name` is the daemon display label. Use a stable human name; transport
addresses belong in SANs and advertised URLs, not in the label. When setup is
run without `--name`, Intendant uses the system hostname when available and uses
the primary IP only as a last resort.

The interactive `intendant access` setup/enrollment flow is currently validated on
Unix hosts. Cert *generation* and native HTTPS/WSS are cross-platform, so a
Windows daemon can still use native HTTPS/mTLS and
`read_server_cert_fingerprint` to publish a pinned fingerprint. See
[Windows Support](./windows-support.md).

Enrollment is not a plain unauthenticated download. The temporary
`serve-certs` endpoint runs HTTPS with the access server certificate, prints
the enrollment URL as a terminal QR code (scan instead of typing), and
answers accidental plain-`http://` dials with a redirect to the https URL.
The CLI does not print the expected server fingerprint or the enrollment
secret at startup; the operator first copies the SHA-256 fingerprint observed
in the browser's certificate UI into the CLI — the first 20 hex characters
are enough (an attacker cannot pre-grind a certificate sharing an 80-bit
prefix). Only a match reveals a one-time secret, and only a browser that
redeems that secret can download the CA, client certificate, or Apple
configuration profile. The page detects the browser only to put the most
likely install path first; all artifacts remain gated by the terminal-paired
browser session.

When the daemon holds a live **fleet certificate**
([Self-Hosted Rendezvous → Fleet DNS](./self-hosted-rendezvous.md#fleet-dns-real-certificates-for-daemons)),
`serve-certs` also serves it for the fleet name on the enrollment port and
leads with that URL instead: the page then loads warning-free under WebPKI,
and the fingerprint transcription is skipped — the operator just presses
Enter to reveal the secret once the page is open. That path bootstraps
device trust through [first-contact rung two](./trust-tiers.md#first-contact-three-rungs)
(active-only betrayal, CT-logged, tripwire-watched) rather than rung one;
the classic fingerprint ceremony against the IP URL remains available from
the same prompt for trustless assurance. There is deliberately no
short-code/PAKE variant: until the device trusts the daemon's certificate,
a code typed into the *page* goes to whoever served the page — under an
active MITM, the attacker — so the trustless leg keeps reading from browser
chrome into the trusted terminal.

Native access TLS also solves browser secure-context requirements for access clients
once the CA/client identity are installed. That matters for Station's WebGPU
renderer, microphone/camera, browser screen capture, and stricter clipboard APIs;
plain `http://<host-ip>:8765` does not expose those features. Plaintext mode is
intended for explicit local/debug use; `--no-tls` on a wildcard listener refuses
startup when a public interface exists unless `--allow-public-plaintext` is
passed.

### Peer pairing — invites and access requests

Daemon-to-daemon mTLS uses the same access CA. The human model is: peers are
relationships, not raw endpoints. A daemon relationship has identity, a server
certificate pin, a peer-scoped client certificate, capability metadata, and local
policy that can later be changed or revoked.

The dashboard's **Access** tab exposes peer relationship management:
**Invitations** contains onboarding flows, **Peer Trust** shows inbound
identities and outbound peer routes, and **Daemons** shows peer-routed targets
alongside local/user-client targets. Targets are a dashboard navigation
abstraction backed by `/api/dashboard/targets`; the security decision is still
the peer profile on the daemon-to-daemon mTLS identity.

#### Invite flow

Use an invite when the accepting daemon's operator can copy a secret directly to
the connecting daemon:

1. On the daemon that will accept inbound peer connections, open
   **Access → Invitations → Grant Peer Invite** and click **Create**.
2. Copy the generated `intendant-peer-v1...` invite.
3. On the daemon that should connect to it, paste the invite into
   **Access → Invitations → Join Invite**.

Joining from the dashboard writes or updates the local daemon's outbound
`[[peer]]` entry in `intendant.toml`, stores the peer-issued client
certificate/key in the local access cert store, and queues live registry
registration so the daemon can connect without a restart.

The same flow is available from the CLI for headless peers or terminals:

```bash
# On the daemon that will accept inbound peer connections:
intendant access setup --name workstation --port 8765
intendant peer invite --card-url https://workstation.example:8765

# On the daemon that should connect to it:
intendant peer join 'intendant-peer-v1....'
```

`invite` issues a fresh client certificate from the accepting daemon's access CA,
adds the accepting daemon's Agent Card URL, and includes the accepting daemon's
server certificate fingerprint. The invite contains the client private key, so
treat it as a secret and paste it only to the daemon that should connect.

`join` stores that peer-issued client identity under the local per-user access
cert store and writes or updates an outbound `[[peer]]` block in
`intendant.toml` with:

- `card_url` — where to fetch the peer's Agent Card
- `client_cert` / `client_key` — the peer-issued mTLS keycard this daemon presents
- `pinned_fingerprints` — the accepting daemon's exact server certificate pin

If `--card-url` is omitted, `invite` derives
`https://<access-primary-ip>:8765/.well-known/agent-card.json` from
`intendant access setup` metadata. Use `--card-url` when peers reach the daemon
through DNS, Tailscale, a tunnel, NAT, or a non-default port.

#### Access-request flow

Use an access request when a primary daemon wants to add another Intendant
without first copying a private-key-bearing invite, or when the target daemon is
headless and should be approved from its own CLI/logs.

1. On the daemon that wants access, use
   **Access → Invitations → Request Peer Access** in the dashboard or:

   ```bash
   intendant peer request https://target.example:8765 --profile peer-operator
   ```

2. The requester generates a client keypair locally and sends only a bounded
   public request to the target: requester label, public key, nonce, requested
   profile, and optional requester card URL.
3. The target records a short-lived pending request and shows it in
   **Access → Invitations → Inbound Peer Access Requests**,
   `intendant peer requests`, and the daemon log.
4. The target operator approves, denies, or approves with a downgraded profile:

   ```bash
   intendant peer approve ABCD-1234
   intendant peer deny ABCD-1234
   ```

   When neither the request nor the approval states a profile, the grant
   defaults to **`read-only-display`** (`DEFAULT_PROFILE` in
   `access_policy.rs`): the peer can view displays but holds no input.
   Upgrading is an explicit owner act — `peer approve --profile
   peer-operator` at approval time, or `peer set-profile` later. The
   identity a `peer invite` pre-approves carries the same default.

5. Approval signs the requester's public key with the target's access CA and
   exposes only the signed client certificate to the requester. The requester's
   private key never leaves the requester.
6. The requester clicks **Check** or runs:

   ```bash
   intendant peer complete <request-id>
   ```

   This installs the signed client certificate/key pair under the requester's
   access cert store, writes or updates the requester's outbound `[[peer]]`,
   pins the target server fingerprint, and starts the live peer registration
   when the dashboard daemon is running.

A granted profile can be changed later without re-pairing:

```bash
intendant peer set-profile <fingerprint> --profile peer-operator
```

`set-profile` rewrites the stored identity record for an approved inbound peer
— copy the fingerprint from `intendant peer identities` (an unambiguous prefix
works; no match and ambiguous prefixes error with the candidates listed). Like
approval, this is an offline state-file write: the gateway resolves a presented
client certificate to its stored profile on every request, so the new profile
takes effect on the peer's next request with no daemon restart. Revoked
identities cannot be edited — approve a new pairing instead.

`--profile` values typed at the CLI (`request`, `approve`, `set-profile`) are
validated against the canonical profile vocabulary and fail loudly on unknown
names, listing the known profiles and aliases — a typo no longer silently
lands as a presence-only grant. Aliases (e.g. `peer-daemon` for `peer-root`)
keep working and resolve to their canonical name. Unknown profile strings
arriving *on the wire* are still accepted and stay fail-closed: they degrade
to presence-only at authorization time.

The unauthenticated public surface for this flow is intentionally tiny:
`POST /api/peer-pairing/requests` creates a pending request and
`GET /api/peer-pairing/requests/<id>` lets the requester poll. Those endpoints do
not list peers, read config, mint certificates, or approve anything. They enforce
a small body limit, strict JSON schema, short TTL, global and per-source pending
caps, in-process global and per-source rate limits, and no certificate signing
until local approval. Default mTLS leaves this public doorbell reachable while
all other dashboard/API/WebSocket paths still require a verified client
certificate. For public deployments, keep TLS enabled; plaintext `--no-tls` is
for explicit local/debug use. Set `INTENDANT_PEER_ACCESS_REQUESTS=0` to disable
public request creation entirely, or set `[server.peer_access_requests]
enabled = false` in `intendant.toml`.

The hardening defaults are intentionally conservative and configurable:
`body_limit_bytes = 4096`, `ttl_secs = 600`, `max_pending = 32`,
`max_pending_per_source = 5`, `rate_limit_window_secs = 60`,
`max_creates_per_window = 64`, and
`max_creates_per_source_per_window = 8`.

For a real two-machine check, run the VM harness from a worktree:

```bash
scripts/e2e-peer-pairing-vm.sh --remote vm@192.168.66.7
```

The harness builds/syncs the current worktree, creates isolated local and remote
access cert stores, starts the VM daemon with default TLS/mTLS, runs
request/approve/complete headlessly, then starts a local dashboard daemon and
waits until `/api/peers` reports the VM connected.

### How auth maps to the Agent Card

The human model is certificate-first: the server certificate proves the daemon
you reached, a client certificate is the peer's keycard, and peer
profile/capability metadata decides which doors that keycard opens. Use
`peer-operator` for ordinary delegated display/task/approval work and
`peer-root` only when the other daemon should have all peer operations,
including settings, shell, files, and runtime control. Older profile names such
as `operator`, `admin-peer`, and `peer-daemon` remain compatibility aliases.
`peer-root` can inspect the unified access model and inspect/manage peer
topology, but it is still daemon-to-daemon authority; it does not grant future
human/account `access.manage` authority.
The card's `auth` field tells connecting peers what proof to send. Construct it
via the `AuthRequirements` helpers:

| Helper | `transport` | `application` | Use when |
|---|---|---|---|
| `none()` | `None` | — | Trusted network: loopback, tailnet, LAN behind a firewall (the phase-1 default) |
| `mutual_tls()` | `MutualTls` | — | Normal federation behind `intendant access setup` |

For daemon-to-daemon mTLS, the connecting daemon presents a client certificate
during the HTTPS/WSS handshake. Config-driven peers can set
`[[peer]] client_cert` and `client_key` to a client identity issued by the
remote peer's access CA. If those fields are absent, Intendant tries the
installed local access `client.crt` / `client.key` for TLS peer URLs; that works
only when the remote peer trusts the same issuing CA. Independent daemons still
need a pairing/provisioning step; `intendant peer invite` / `join` is the
built-in path for that.

`PinnedMutualTls` is the stricter transport form when an operator pins a server
certificate fingerprint out of band. Bearer `ApplicationAuth` still exists in the
wire format and code for legacy deployments, but it should not be the normal
dashboard or daemon-to-daemon UX.

## See Also

- [Display Pipeline](./display-pipeline.md) — the local capture/encode/WebRTC
  pipeline that federated displays plug into, and the `[webrtc]` config
- [Windows Support](./windows-support.md) — why `intendant access` is gated off
  Windows and what to use instead
- [`docs/design-federated-input-authority.md`](https://github.com/intendant-dev/Intendant/blob/main/docs/design-federated-input-authority.md)
  — the full federated input-authority protocol
