# MCP Server

The `--mcp` flag runs Intendant as a [Model Context Protocol](https://modelcontextprotocol.io/)
server over stdio JSON-RPC (`src/bin/caller/mcp/`). It lets an external agent
(Claude Code, Codex, Kimi Code, etc.) observe and control Intendant through a broad
operational tool surface: session actions, display/CU/frame tools, shared-view
collaboration, live audio, managed context, and controller orchestration.
Presentation-only dashboard affordances are not necessarily one-for-one tools.

Architecturally the MCP server is a **frontend peer of the dashboard**: it
subscribes to the same `EventBus`, and user intents are
[`ControlMsg`](./integrations.md) values everywhere — the web dashboard and the
Unix control socket dispatch them to the centralized `control_plane.rs` (see
[Autonomy & Approvals](./autonomy.md) for why frontends are display-only), and
the MCP server's approval/input tools apply the same state helpers as its own
`ControlMsg` arms (`resolve_pending_approval` & co. in `mcp/mod.rs`; the former
MCP-only `UserAction` enum is retired). `--mcp` is its own run mode and is
**not** implied by `--web`.

## Running

```bash
# MCP server on stdio
./target/release/intendant --mcp "Deploy the application"

# With provider/model overrides
./target/release/intendant --mcp --provider anthropic --model claude-sonnet-4-6-20250929 "Fix the tests"

# With an autonomy preset
./target/release/intendant --mcp --autonomy high "Refactor the auth module"
```

In MCP mode, stdin/stdout are reserved for JSON-RPC, so the initial task is taken
from the command line (or the server starts idle and accepts `start_task`).

### Client Configuration

Add Intendant to your MCP client config (Claude Code
`~/.claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "intendant": {
      "command": "intendant",
      "args": ["--mcp", "Your task description here"]
    }
  }
}
```

## Tools

The full MCP tool surface (dispatched in `call_tool_by_name`) is broad. For
model clients that front-load tool schemas into every request, prefer the
HTTP transport's `tool_profile=core` query parameter and the `intendant ctl`
CLI for lazy discovery. `tool_profile=core` advertises `get_status`; `whoami`
(the caller's own identity, for provenance in memory/agenda writes); the
agent-to-user collaboration primitives (`post_session_note`, `ask_user`,
`notify_user`); the Agenda tools (`agenda_list`, `agenda_item`, `agenda_op`)
and the Memory retrieval/propose tools; the shared-view
tools; and the minimal display/CU set (`list_displays`,
`create_virtual_display`, `grant_user_display`, `request_user_display`,
`revoke_user_display`, `take_screenshot`, `read_screen`, `execute_cu_actions`,
`display_readiness`) — managed and vanilla alike. Managed context additionally
advertises rewind/backout and fission tools. The other profile names
(`tool_allowed_for_profile` in `mcp/tool_gate.rs`): `codex-core`, `cli`, and
`minimal` are aliases of `core`; `screen` (alias `display`) advertises the
served display/CU and shared-view set — its allowlist also names the
browser-workspace and raw frame tools, which currently have no served
`tools/list` definitions anywhere and are reachable only by direct call;
`managed` (alias `managed-context`) advertises `get_status` plus the managed
rewind/fission set; `facade` advertises only the six meta-tools of the
CLI-shaped facade (next section); `full` — or omitting `tool_profile` —
keeps the whole
list, and unknown profile names fall back to the `core` bootstrap set (the
daemon logs the unknown name, and hidden tools stay callable, so a typoed
URL stays diagnosable without the full-list context cost). Profile filtering applies to `tools/list` only —
hidden HTTP tools remain callable (the lazy `ctl tools call` path).
*Authorization* is separate: see the next section.

### The facade profile

`tool_profile=facade` is the context-efficient control surface: instead of a
typed schema per capability, it advertises six meta-tools and everything
else is discovered lazily. `inspect`, `act`, and `authorize` each execute
one registered command per call, named as an argv array
(`{"argv":["agenda","list","--status","open"]}`) — risk-split so MCP hosts
can auto-allow reads and gate authority-class calls per tool. `help` renders
the command map from the registry (families first, then per-family usage
lines with each command's lane); `docs` lists and fetches the embedded
operating skills. `events` is the cursor long-poll over the daemon's
session/approval/task lifecycle stream — omit `since` to start at now,
pass `wait_s` (≤60) to block until something happens, re-poll with the
returned `next_cursor`, and treat `gap: true` as "resync via the read
commands". Its cursors are opaque, bound to the minting principal, and
invalidated by a daemon restart (the error says so); the underlying ring
ingests only the lifecycle families the `session.inspect` read tools
already serve, so the stream is push semantics over existing authority,
never new authority — and delivery is session-scoped for agent-session
callers (a supervised backend on its session-bound token sees only its
own session's events, sessionless events withheld, matching the scoped
approval reads). A facade call is authorized as the **resolved** command's
operation against the caller's principal, at every ingress, before any side
effect — a parse failure never dispatches, and a command invoked through the
wrong lane is redirected to the right tool by name. Argv values are literal
strings: no shell, no `@file`/stdin expansion, no local output paths (those
are `intendant ctl` frontend behaviors and stay client-side). The command
registry (`mcp/facade.rs`) covers the full `ctl` grammar — status,
approvals, input, ask/notify/notes, tasks, agenda (reads and the whole
write/effect family), memory (reads, proposals, and owner curation),
displays, browser workspaces, computer use, shared view, settings,
remote compute, controller, audio, peers, terminal, and context — with
nested/structured parameters passed as literal JSON values (parsed at
plan time; a parse failure never dispatches). The serialized facade
listing is budget-pinned in tests so the whole advertised surface stays
a few kilobytes regardless of registry size; `intendant ctl events` is
the CLI leg of the `events` verb (client-side ≤60s chunking over a
`--for` budget, NDJSON on stdout, cursor on stderr).

The tables below describe the full daemon HTTP MCP surface. Bare `--mcp`
stdio mode serves only the thirteen `#[tool]`-router tools — `get_status`,
`get_logs`, `get_pending_approval`, `get_pending_input`, `approve`, `deny`,
`skip`, `approve_all`, `respond`, `set_autonomy`, `set_verbosity`, `quit`,
and `start_task`. It does not carry the daemon's Agenda or Memory service
handles; everything else below is served by the wired daemon `/mcp` surface.

### /mcp authorization

Every `POST /mcp` request binds to a principal in the same local IAM system
that gates the dashboard and federation surfaces
([Trust Architecture](./trust-architecture.md)), and **every `tools/call` is
evaluated at call time** against that principal's permissions via a per-tool
operation map (`mcp_tool_operation` in `mcp/tool_gate.rs`; e.g. `execute_cu_actions`
and `grant_user_display` require `display.input`, `start_task` requires
`task.run`, unclassified tools require `runtime.control`). `tools/list` is
filtered to what the principal may actually call. The display tools carry a
second, separate gate: a `user_session` target needs the standing
user-display grant unless the bound principal is an owner/root caller (the
trusted dashboard, an independently enrolled direct-mTLS `role:root` user
client, or bare local loopback — derived by `ToolCallerTrust` without
widening `AccessPrincipal::is_owner_surface`); the stdio transport, being
wired up by the owner's own client config, always counts as an owner surface. See
[Computer Use](./computer-use-and-audio.md#display-targets). Binding order:

1. **Peer daemons** (mTLS peer identity) use their peer-profile principal.
2. **Supervised backends** receive a bearer only in their cleared child
   environment and send it in the `Authorization` header. Their argv-visible
   MCP config contains the environment-variable name, never the value.
   `INTENDANT_MCP_URL` remains the private environment bootstrap for `ctl`.
   The bearer is *session-scoped* — derived from the daemon's per-process token
   and the `session_id` — so it authenticates exactly that agent session
   (`principal:agent-session:<id>`). Possession of the raw per-process token
   remains root-equivalent. Explicit-but-wrong tokens are refused with 401. A
   session whose binding is known but whose grant has
   *lapsed* (expired or revoked) binds the scoped principal and is denied
   with the real reason — it does not fall back to default trust; only
   sessions with no binding at all do.
3. **Browser pages** may only call `/mcp` from this daemon's own origin (or
   the macOS app scheme) and then bind like any dashboard HTTP request
   (an enrolled mTLS certificate principal or trusted-local root). Foreign origins
   get 403 — same posture as the rest of `/api/*`.
4. **mTLS client certificates** bind to their IAM principal.
5. **Tokenless loopback** processes must present the daemon's per-boot
   loopback admission token (below) and bind to
   `principal:local-process:loopback`. Tokenless non-loopback requests are
   refused. Once any `agent_session` binding exists — even one whose grant
   has since expired or been revoked — this path **fails closed** (401)
   until an explicit `local_process` grant states what bare loopback
   callers may do; otherwise a scoped agent could shed its injected token
   and re-enter as the root-compatible local default, making its grant
   decorative. A lapsed `local_process` grant likewise denies rather than
   restoring the open default.

Two transport details back that ladder. First, loopback reachability alone
is no longer a credential: the daemon mints a fresh random **loopback
admission token** each boot (`loopback_token.rs`), persists it 0600 at
`<state root>/loopback-tokens/<port>.token`, and owner-posture loopback
surfaces — the tokenless `/mcp` rung included — refuse loopback requests
that do not present it (`x-intendant-loopback-token` header, `?token=`
query parameter, or a bearer). Same-uid owner processes such as `intendant
ctl` read the file (clients may override via `INTENDANT_LOOPBACK_TOKEN`;
the daemon itself never reads that variable). The token admits a caller to
the loopback owner posture without creating any new principal class, and it
is deliberately independent of the MCP token ladder above. Second, the
daemon also binds a dedicated **session-MCP loopback listener** on a
kernel-assigned `127.0.0.1` port: it serves only `/mcp`, and its access
ladder has exactly one rung — a session-scoped token binds that agent
session; everything else (peer identity, the shared process token, browser
origins, mTLS certificates, tokenless loopback) is refused by name. The
`INTENDANT_MCP_URL` injected into a supervised native session's runtime
children targets this listener (external backends keep the main gateway
port), so sandboxed `ctl` calls keep working where the sandbox blocks the
daemon's main port; nothing reachable through it exceeds the calling
session's own gate-resolved authority.

The rule across all of these: **once a principal is named, its authority
comes only from grants, and a lapsed grant means "no" — never "back to
defaults".** Security posture only relaxes when a person explicitly
relaxes it: re-grant `role:root` (to the `"*"` agent principal or to
`local_process`) to restore the implicit-trust behavior visibly and
auditable, rather than by timer or revocation side effect.

By default the supervised-agent, token-holder, and local-loopback principals
are root-compatible, so bare `intendant ctl` on the daemon host and existing
supervised backends keep working with zero ceremony. Root-compatible does
**not** mean unconstrained, though: independent of IAM, agent-session
provenance is contained on the approval/settings surface. A caller bound as
an agent session (session-scoped token, or the process token naming a
session) is refused `set_autonomy` and `approve_all` outright — both rewrite
the daemon-global shared autonomy, which would let a supervised agent widen
its own approval policy — and `approve`/`deny`/`skip` resolve only approvals
raised by the caller's own session (cross-session resolution is denied;
unknown ownership fails closed). The same containment covers daemon
lifecycle: `quit`, `schedule_controller_restart`, and
`cancel_controller_restart` are refused to agent-session callers (a
supervised agent must not stop or bounce its supervisor), while
`controller_turn_complete` (the session's own completion signal inside an
owner-scheduled restart) and the controller-loop halt tools (the autonomous
loop's self-stop verbs) stay open. This mirrors `grant_user_display`'s
owner-surface rule: owner surfaces (dashboard, `intendant ctl` on loopback,
enrolled root mTLS clients, the stdio transport) and deliberately granted
non-agent principals keep the full surface. The point of the
binding is that the owner can now *scope* them: an
`agent_session` grant (exact `session_id`, or `"*"` for every supervised
agent) or a `local_process` grant against
`POST /api/access/iam/user-client-grants` pins that principal to a role, and
call-time enforcement + `tools/list` follow it. Example — cap every
supervised agent at operator (no runtime control, no settings/access
administration):

```bash
curl -X POST http://localhost:8765/api/access/iam/user-client-grants \
  -H 'Content-Type: application/json' \
  -d '{"kind": "agent_session", "session_id": "*", "role_id": "role:operator"}'
```

Scoping any agent session flips the tokenless loopback default to
fail-closed, so pair it with an explicit statement of what your own bare
`intendant ctl` gets (root keeps it exactly as before, now as a visible,
revocable grant):

```bash
curl -X POST http://localhost:8765/api/access/iam/user-client-grants \
  -H 'Content-Type: application/json' \
  -d '{"kind": "local_process", "role_id": "role:root"}'
```

The shared per-process token still exists as the transport-layer fallback
(and is what the strict-TLS loopback-cleartext exception checks), but
possession of it is no longer the *authorization* story — grants and the
evaluator are.

CORS on `/mcp` matches the gate: responses echo `Access-Control-Allow-Origin`
only for the daemon's own origin or the app-bundle scheme (which genuinely
needs it); foreign origins and non-browser clients get no CORS grant at all.
`POST /mcp` bodies are read and capped before dispatch at 16 MB
(`MCP_BODY_CAP_BYTES` in `gateway_routes.rs`) — tool calls legitimately
carry file-sized arguments.
With the patched managed Codex binary, `rewind_backout mode="fork"` creates a
new Codex thread while inheriting the lineage prompt-cache key from the saved
rollout; same-thread `restore` remains available when the current thread should
be rewritten in place.

The CLI mirrors the broad surface without loading every schema into model
context:

```bash
"${INTENDANT:-intendant}" ctl --help
"${INTENDANT:-intendant}" ctl tools list
"${INTENDANT:-intendant}" ctl tools schema take_screenshot
"${INTENDANT:-intendant}" ctl tools call grant_user_display --args '{}'
"${INTENDANT:-intendant}" ctl display grant-user
"${INTENDANT:-intendant}" ctl display screenshot --target user_session --output screen.png
```

Full MCP tool groups:

### Status & logs (observation)

| Tool                   | Description | Params |
|------------------------|-------------|--------|
| `get_status`           | Provider, model, turn, budget %, phase, autonomy, verbosity, tokens. | — |
| `whoami`               | The caller's own gate-resolved identity, for provenance in memory/agenda writes: supervised callers get their daemon session id, backend harness (`claude-code`/`codex`/`kimi`/`native`) with its harness session id, wrapper aliases (restart/resume rotations of the same conversation), project root, and log dir; unsupervised callers get `supervised:false` plus their principal id. Claims a session only when the call authenticated with that session's token — never from request fields. Also `intendant ctl whoami`. | — |
| `get_logs`             | Log entries, cursor-paginated and level-filterable. Without `session_id`, HTTP/ctl reads the daemon's currently observed session. | `session_id?`, `since_id?`, `level_filter?`, `limit?` |
| `get_pending_approval` | The current pending approval request (or null). | — |
| `get_pending_input`    | The current pending `askHuman` question (or null). | — |

### Interactive actions

| Tool            | Description | Params |
|-----------------|-------------|--------|
| `approve`       | Approve a pending command. Owner surfaces only; HTTP/ctl routes the exact prompt id and owning session through the daemon control plane. | `id` |
| `deny`          | Deny and stop. Owner surfaces only; HTTP/ctl routes through the daemon control plane. | `id` |
| `skip`          | Decline this command and let the agent continue. Owner surfaces only; HTTP/ctl routes through the daemon control plane. | `id` |
| `approve_all`   | Approve and set autonomy to Full. Owner surfaces only (daemon-global autonomy escalation). | `id` |
| `respond`       | Answer an `askHuman` question. | `text` |
| `post_session_note` | Post a **display-only note** into the session transcript — rendered live in the dashboard and persisted for replay, never added to any model's context. Optional base64 images are committed to the session upload store and rendered as clickable thumbnails. Caps: 16 KB text, 6 images, 4 MB per image, 8 MB total; raster types only (`image/png`, `image/jpeg`, `image/gif`, `image/webp`, `image/bmp`). Session-scoped callers post into their own session by default. | `text`, `images?` (`[{media_type, data, name?}]`), `session_id?`, `source?` |
| `ask_user`      | Ask the user one **structured question** on the dashboard question rail and **block** until answered or the wait expires. A question requests *input*, never permission: it is never auto-approved and answering it never widens autonomy. 0–4 options; free-text answers are always accepted (zero options = free-text only). Up to 4 **preview cards** render above the options (show, then ask — prototype variants, before/after states): `html` must be one self-contained document, rendered only inside a sandboxed opaque-origin iframe (scripts run; external fetches and daemon APIs do not resolve); `image` is base64 raster (session-note MIME allowlist); `text` renders preformatted. Blob kinds commit to the session upload store and travel as references. Caps: 2 MB/html, 4 MB/image, 4 KB/text, 8 MB total. Returns `{status, answer, answers}` — `answered` carries the choice(s); `timeout`/`dismissed`/`pass` carry best-judgment guidance; shapes with no answerable frontend auto-answer immediately with the same guidance. Session-scoped callers ask as their own session. Also `intendant ctl ask` (whose `--preview-html/-image LABEL=FILE` flags read the files ctl-side, under the caller's own sandbox). | `question`, `options?` (`[{label, description?}]`), `previews?` (`[{label, html \| image+media_type \| text}]`), `header?`, `multi_select?`, `wait_seconds?` (default 300, max 900), `session_id?` |
| `notify_user`   | Fire-and-forget **notification** to the user; returns immediately, renders as a dashboard toast plus a transcript row (persisted for replay), never enters model context. `urgency` escalates delivery: `info` (default) dashboard-only; `attention` + tab badge and hidden-tab browser notification; `urgent` + an immediate content-free push nudge to the owner's opted-in browsers. Cap: 4 KB text. Also `intendant ctl notify`. | `text`, `title?`, `urgency?` (`info`/`attention`/`urgent`), `session_id?` |
| `set_autonomy`  | Set autonomy. Denied to agent-session callers — the setting is daemon-global. | `level`: `low`/`medium`/`high`/`full` |
| `set_verbosity` | Set log verbosity. | `level`: `quiet`/`normal`/`verbose`/`debug` |
| `start_task`    | Start work, route a follow-up to `session_id`, or resume a persisted external-agent wrapper. A non-empty `reference_frame_ids` list is the supervisor's CU-routing gate; `display_target` selects the display for that grounded request but is not sufficient by itself. | `task`, `session_id?`, `orchestrate?`, `reference_frame_ids?`, `display_target?` |
| `quit`          | Shut down the agent. | — |

`start_task` currently has two routing caveats. A new-session CU request needs
at least one frame id that resolves in the frame registry: a `display_target`
alone, or only stale/unknown frame ids, is accepted and acknowledged as “CU
task dispatched” by the MCP edge but falls through to an ordinary task in the
supervisor. With `session_id`, any supplied frame ids and `display_target` are
discarded and only the text is routed as a follow-up. Treat those acknowledgments
as enqueue acknowledgments, not proof that grounded CU started; both behaviors
are tracked implementation defects.

### Agenda & Memory

Agenda is the durable, append-only parking ledger; Memory is a bounded
provenance-labeled claim plane. Both render their text as quoted data, never
instructions. Agenda effect approval/revocation is owner-only even though
agents and peers may propose an effect. Memory proposals enter the candidate
lane; judging them is the owner's act — `memory_judge` refuses agent and
peer callers, so an agent that disagrees with a claim proposes a countering
claim instead.

| Tool | Description | Params |
|------|-------------|--------|
| `agenda_list` | List oldest-first items plus Open / Done / Retired counts; filter by status or server-side query, choose full or summary grain, delta-poll with a previous response's `seq`, and page the live/archive windows. | `status?`, `q?`, `shape?`, `since_seq?`, `window?`, `before?`, `before_id?`, `limit?` |
| `agenda_item` | Fetch one item at full detail by id or unique id prefix: body, tags, provenance, the annotation thread, blockers, dependency/relation edges, refs, effects with manifests and run history, ask payload, and answer. | `id` |
| `agenda_op` | Apply one tagged operation (the `AgendaCommand` vocabulary in `agenda/types.rs`). Item lifecycle: `add`, `ask`, `answer`, `acknowledge_answer`, `patch`, `complete`, `reopen`, `retire`, `annotate`, `pick_up`, `attest`. Blockers and dependencies: `set_blocker`, `clear_blocker`, `add_relies_on`, `remove_relies_on`. Structure, links, and refs: `add_part_of`, `remove_part_of`, `place`, `add_relates_to`, `remove_relates_to`, `add_ref`, `remove_ref`. Effects: `propose_effect`, `stamp`, `withdraw_effect`, plus the owner-surface-only `approve_effect`, `revoke_effect`, `request_occurrence`, and `start_now`. | operation-specific `op` shape |
| `memory_search` | Bounded claim search (default 10, maximum 50); candidates are excluded unless explicitly requested. Responses report effective durability. | `query?`, `limit?`, `include_candidates?` |
| `memory_read` | Read one claim by an id prefix of at least eight hex characters. | `id` |
| `memory_propose` | Propose a typed, sensitivity-labeled candidate. Authorship comes from the gate-bound caller, not writer-supplied context fields. | `kind`, `statement`, `sensitivity?`, `session?`, `project?`, `model?`, `labels?` |
| `memory_judge` | Owner curation: judge one claim (`accept`/`dispute`/`retire`/`supersede`); dashboard and owner-shell surfaces only — agent and peer callers are refused. `supersede` names a replacement claim, which holds only while the replacement's derived status is accepted; status is re-derived by the fold, never edited. | `verdict`, `id`, `reason?`, `replacement?` |

### Display, computer use & frames

| Tool                 | Description | Params |
|----------------------|-------------|--------|
| `list_displays`      | Enumerate displays with their session state. | — |
| `create_virtual_display` | Create a daemon-owned virtual display (Xvfb) and activate it for capture and streaming; it announces as `display_ready` to every dashboard and federated peer. The display survives the calling session and dies with the daemon; closing its dashboard tile (or revoking its id) reaps it early. Linux hosts only today — other platforms report a clear error. | `width?`, `height?` |
| `take_display`       | Optional dashboard signal that an agent is using a display; it neither grants input authority nor is required before screenshot/CU calls. | `display_id` |
| `release_display`    | Release control of a display. | `display_id`, `note?` |
| `grant_user_display` | Grant access to the user's real display session (owner surfaces only — this call *is* the opt-in); on Wayland, enable **Allow Remote Interaction** in the GNOME portal before clicking **Share** so CU input works. | `display_id?` |
| `request_user_display` | Ask the user for their display: raises the dashboard doorbell popup with your reason and blocks for their click — the only thing that can grant it (never auto-approved; see [Autonomy — the display request rail](./autonomy.md#the-display-request-rail-doorbell)). `access="view"` shares the stream without CU input; `"view_and_control"` requests the full grant. | `reason`, `access?`, `wait_seconds?`, `session_id?` |
| `revoke_user_display` | Revoke access to the user's real display session. | `display_id?`, `note?` |
| `take_screenshot`    | Capture a screenshot (returns image content). | display params |
| `read_screen`        | User session's frontmost-app accessibility tree — macOS AX, Linux AT-SPI, or Windows UIA. | `display_target?`, `format?`, `full_values?` |
| `display_readiness`  | Probe display authority, capture/accessibility permission, target availability, and input backend live; names each missing layer. | `display_target?` |
| `execute_cu_actions` | Run a batch of [computer-use](./computer-use-and-audio.md) actions. | CU action params |
| `list_frames`        | List captured video frames. | filter params |
| `read_frame`         | Read a specific frame. | `frame_id` |

### Shared-view collaboration

These tools control the agent-owned display presentation the user sees. They
do not silently grant keyboard/mouse authority: in particular,
`request_shared_view_input` only raises an advisory request and the user must
click the dashboard control.

| Tool | Description | Params |
|------|-------------|--------|
| `show_shared_view` | Open/foreground a shared display, optionally with an initial highlighted region. | `display_target?`, `display_id?`, `reason?`, `focus_region?` |
| `hide_shared_view` | Dismiss the shared-view banner and focus overlay. | `reason?` |
| `focus_shared_view` | Highlight a normalized region and optional note. | `region`, `display_target?`, `display_id?`, `note?` |
| `clear_shared_view_focus` | Clear only the focus annotation; safe when none exists. | `reason?` |
| `request_shared_view_input` | Ask the dashboard user to take input authority; never grants it. | `display_target?`, `display_id?`, `reason?` |
| `capture_shared_view_frame` | Foreground the shared view and return its current frame as an MCP image. | `display_target?`, `display_id?`, `reason?` |

### Managed context & fission

These definitions are advertised only for sessions whose Codex managed-context
mode is enabled; calls against a vanilla session fail with an explicit
disabled-mode error. Rewind tools operate on exact Codex item ids. Fission
forks the completed-turn context into real sibling sessions and records their
group/canonical state in the lineage ledger.

| Tool | Description | Params |
|------|-------------|--------|
| `list_rewind_anchors` | Return bounded exact rewind anchors, with optional paging/search and density/recovery estimates. | `session_id?`, paging/filter/density flags |
| `inspect_rewind_anchor` | Inspect a compact window around one exact anchor. | `item_id`, `session_id?`, `radius?` |
| `rewind_context` | Schedule rollback to `anchor.position` (`before`/`after`) and inject a required carry-forward primer. | `anchor`, `reason`, `primer`, `session_id?`, `preserve?`, `discard?`, `artifacts?`, `next_steps?` |
| `rewind_backout` | Inspect, restore, or fork/back out a prior rewind record. | `record_id`, `session_id?`, `mode?`, `name?` |
| `fission_spawn` | Fork 1–4 full-context sibling branches; write-scoped branches use isolated worktrees by default. | `branches`, `session_id?`, `use_worktree?` |
| `fission_control` | `wait`, `import`, `cancel`, or `detach` one fission branch/group. | `group_id`, `op`, `session_id?`, `branch_session_id?`, `timeout_s?` |
| `claim_fission_canonical` | Claim or compare-and-swap the group's canonical continuation. | `group_id`, `branch_session_id`, `expected_canonical_session_id?` |

### Browser workspaces

Browser workspaces are addressable browser-control surfaces for agent/human
collaboration and headed UI testing. The first executable backend launches a
managed local Chromium-family browser with an isolated profile and Chrome
DevTools Protocol metadata. On macOS, Intendant does not launch the user's
installed `/Applications/Google Chrome.app` by default; use `provider=system_cdp`
or `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1` to opt into system
Chrome/Chromium, and use `INTENDANT_BROWSER_WORKSPACE_EXECUTABLE` for an
explicit browser binary. Run `intendant setup browsers` to install Chrome for
Testing into Intendant's managed cache. The wire contract already carries
`provider` and `peer_id` fields so Playwright/Agent Browser adapters and
federated peer-hosted browsers can slot in later. Each workspace has a lease,
so concurrent agents must explicitly acquire it and use `force` to take over an
active holder.

| Tool                          | Description | Params |
|-------------------------------|-------------|--------|
| `browser_workspace_providers` | Report available workspace providers. | — |
| `list_browser_workspaces`     | List active browser workspaces and leases. | — |
| `create_browser_workspace`    | Launch/register a workspace. | `url?`, `label?`, `provider?`, `peer_id?`, `owner_session_id?`, `profile_dir?` |
| `acquire_browser_workspace`   | Acquire a workspace lease. | `workspace_id`, `holder_id`, `holder_kind?`, `note?`, `force?` |
| `release_browser_workspace`   | Release a workspace lease. | `workspace_id`, `holder_id?`, `note?` |
| `close_browser_workspace`     | Close a workspace and terminate its local browser process when owned here. | `workspace_id`, `reason?` |

### Terminal

Request/response shell access sharing the dashboard's PTY pool — a shell
opened over MCP is attachable from a dashboard terminal tile and vice
versa. Reads ride `terminal.view`; input, resize, and close ride
`terminal.write`; `terminal_open` creates the shell when absent and is
therefore gated as `shell.spawn` structurally (the attach-vs-create split
the dashboard tunnel frame decides statefully is simply two different
authorities here). Output is polled with a monotonic cursor over the
256 KiB scrollback ring; a cursor that has fallen off the window reports
`gap: true` — the polling analogue of the push lane's dropped-output
marker — and the exit status is retained so a poller that missed the
death still learns it. Visibility is the registry's own model: root
surfaces see every session, scoped principals see their own and shared
ones. A shell spawned for a scoped principal is OS-sandboxed to the
grant's filesystem scope and — independently, even when the grant
carries no scope — never inherits the daemon's process environment (the
daemon env holds provider API keys; the child env is cleared and
rebuilt secret-free, and the shell starts profile-less so rc files
can't repopulate it — a shell with no known profile-suppression mode is
substituted by profile-less bash). The sandbox is fixed at spawn, so a caller-owned
session whose grant scope has since changed is refused as stale by
open-attach, reads, writes, and resizes — close it and reopen to get a
shell under the current scope. None of these ride the scoped profiles; they appear on
full/unprofiled listings and through the facade's `terminal` commands
(`open` and `write` on the `authorize` lane — writing into a live shell
is running commands).

| Tool | Description | Params |
|------|-------------|--------|
| `terminal_list` | Visible sessions: id, liveness, sharing, geometry, retained exit status. | — |
| `terminal_open` | Open or create a shell PTY (shell-spawn class); returns the id, whether it was created, geometry, and the starting read cursor. | `terminal_id?`, `cols?`, `rows?`, `shared?` |
| `terminal_read` | Cursor-paged output read with gap reporting, liveness, and exit status. | `terminal_id`, `cursor?`, `max_bytes?` |
| `terminal_write` | Write to a live shell's stdin (appends Enter — a carriage return — by default; `enter: false` for raw keystrokes). | `terminal_id`, `input`, `enter?` |
| `terminal_resize` | Resize the PTY. | `terminal_id`, `cols`, `rows` |
| `terminal_close` | Close the session. | `terminal_id` |

### Remote compute & Codex Cloud workers

`remote_command` offloads heavy platform-neutral compilation and testing to
a provider-neutral remote host — today Codex Cloud workers, reached through
the daemon host's authenticated Codex CLI — while the three Codex Cloud
tools manage the underlying provider tasks as tracked Intendant worker
leases. None of these ride the scoped profiles: `remote_command` was
deliberately dropped from the `core` bootstrap set (its schema was the
largest single item there while consumers stayed rare — context rent;
supervised backends reach the same lane, with the same session-bound
identity, as `intendant ctl remote`), and the Codex Cloud tools appear only
in the unscoped/full listing. All four stay callable by name.

| Tool | Description | Params |
|------|-------------|--------|
| `remote_command` | Start, inspect, wait for, or cancel a remote command job. `start` runs an argv command (never a shell string) against a pushed `git_revision` or an explicit bounded `working_tree` snapshot and returns immediately with acquisition stage/deadline detail; `status`/`wait` return bounded output and exact terminal/cache results. The whole tool is gated as shell spawn. | `op`: `start` (`argv`, `host?`, `branch?`, `cwd?`, `env?`, `source?`, `expected_revision?`, `require_clean?`, `cache?`, `timeout_s?`), `status`/`wait`/`cancel` (`job_id`, `wait_s?`) |
| `list_codex_cloud_workers` | Refresh Codex Cloud tasks into the local worker-lease store and list them, including tracked leases with live attachments outside the provider window; never modifies a Cloud task. | `environment_id?`, `limit?` |
| `submit_codex_cloud_task` | Submit a new Codex Cloud task and track it as an ephemeral Intendant worker lease. | `environment_id`, `prompt`, `branch?`, `attempts?`, `title?` |
| `follow_up_codex_cloud_task` | Send a follow-up turn into an existing Cloud task, reusing its warm worker and incremental build state; refuses tasks with an active turn and fails closed on schema drift. | `task_id`, `prompt` |

### Live audio

| Tool               | Description | Params |
|--------------------|-------------|--------|
| `spawn_live_audio` | Spawn an untrusted [live-audio](./computer-use-and-audio.md#live-audio) voice session. | `id`, `provider`, `playbook`, `response_schema`, … |

### Peer federation

The agent-facing surface for [peer federation](./peer-federation.md):
inspect the peer roster, delegate work to sibling daemons, and do direct
computer use on peer displays. `list_peers` is gated as `peer.inspect` (same
classification as `GET /api/peers`); every other tool here is gated as
`peer.use` — acting through a peer delegates this daemon's peer identity, and
the receiving peer authorizes the request against its own grants for this
daemon. A delegated task runs on the peer's machine under the peer's own
autonomy/approval policy. The direct-CU trio is one stateless `tools/call`
POST to the *peer's* `/mcp` over the transport's mTLS identity; the peer's
gate then requires display view for `peer_list_displays` /
`peer_take_screenshot` (profile `read-only-display` or better) and display
input for `peer_execute_cu_actions` (`peer-operator` / `peer-root`).

| Tool                 | Description | Params |
|----------------------|-------------|--------|
| `list_peers`         | Peer snapshot list — id, label, connection state, capabilities, sessions, displays (same payload as `GET /api/peers`). | — |
| `peer_send_message`  | Send a message to a peer's agent. | `peer_id`, `message`, `session?` |
| `peer_delegate_task` | Delegate a task executed by the peer's own agent; returns `task_id`. | `peer_id`, `instructions`, `context?` |
| `peer_list_displays` | List a peer's displays (ids, names, resolutions) over its `/mcp`. | `peer_id` |
| `peer_take_screenshot` | Screenshot a peer display; returns an MCP image content block. | `peer_id`, `display_target?` |
| `peer_execute_cu_actions` | Run CU actions on a peer display; returns per-action status + the peer's post-action observation (clean screenshot by default). | `peer_id`, `actions`, `display_target?`, `coordinate_space?`, `observe?`, `annotate?` |

### Controller Orchestration

| Tool                            | Description | Params |
|---------------------------------|-------------|--------|
| `schedule_controller_restart`   | Schedule a controller restart / autonomous re-init workflow. | `controller_id`, `north_star_goal`, `reason?`, `restart_after?`, `restart_command?`, `auto_start_task?`, `max_attempts?`, `cooldown_sec?` |
| `controller_turn_complete`      | Final handshake; validates token and executes the scheduled restart. | `restart_id`, `turn_complete_token`, `status?`, `handoff_summary?` |
| `get_restart_status`            | Current restart state (or null). | — |
| `cancel_controller_restart`     | Cancel a scheduled restart. | `restart_id?` |
| `request_controller_loop_halt`  | Request loop halt. | `persistent?` |
| `clear_controller_loop_halt`    | Clear loop-halt flags so restarts can resume. | — |
| `intervene_controller_loop`     | Intervene in the active loop process and visible Codex app-server descendants. | `mode`: `stop`/`abort` |
| `get_controller_loop_status`    | Unified loop-health snapshot. | — |

`schedule_controller_restart`, `controller_turn_complete`, and
`cancel_controller_restart` return JSON payloads with an `ok` boolean and status
fields; rejections come back as JSON (`ok: false`) with an `error` message rather
than plain text.

## Resources

Resources provide push-based observation via subscriptions. The server emits
`notifications/resources/updated` when state changes so clients re-fetch.

| URI                              | Description |
|----------------------------------|-------------|
| `intendant://status`             | Provider, model, turn, budget %, phase, autonomy, session ID, task. |
| `intendant://usage`              | Per-model token usage (main + optional presence). |
| `intendant://logs`               | Last 100 chronological log entries (same as the dashboard's activity log). |
| `intendant://pending-approval`   | The current pending approval, if any. |
| `intendant://pending-input`      | The current pending `askHuman` question, if any. |
| `intendant://controller-restart` | Current controller-restart workflow state, if any. |
| `intendant://controller-loop`    | Loop-health snapshot (intervention flags, singleton lock owner, active wrapper/codex PIDs, latest run pointers). |

## Controller Restart Workflow

Use this when you want Intendant to trigger a controller re-init cycle safely
(e.g. an external Codex/Claude/Kimi controller relaunching itself).

1. Call `schedule_controller_restart`; capture `restart_id` + `turn_complete_token`.
2. Before ending the controlling agent's turn, call `controller_turn_complete`
   with both values.
3. Intendant executes the restart actions:
   - spawn `restart_command` (if provided), and/or
   - start a fresh Intendant task from `north_star_goal`
     (`auto_start_task=false` by default; opt in only for E2E testing).
4. Inspect via `get_restart_status` or `intendant://controller-restart`.

### Notes & guarantees

- Restart state persists to the session dir as `controller_restart.json`.
- `restart_after` defaults to `"turn_end"`; only `"turn_end"` or `"now"` are
  accepted (others rejected). String inputs are trimmed before validation.
- `restart_command`, when provided, must be non-empty/non-whitespace.
- At least one restart action is required: `restart_command` and/or
  `auto_start_task=true`.
- `max_attempts` must be `>= 1` (`0` rejected). Optional `status`,
  `handoff_summary`, and the cancel `restart_id` guard treat whitespace-only as
  unset.
- If `restart_after="now"` and execution fails after validation,
  `schedule_controller_restart` reports `"ok": false` with `execution_error`, and
  the persisted phase becomes `"failed"` with `last_error` populated.
- `controller_turn_complete` only accepts restarts in
  `"awaiting_turn_complete"`; duplicate/late handshakes (e.g. `"phase": "ready"`)
  are rejected to prevent double execution.
- `get_restart_status` and `intendant://controller-restart` redact
  `turn_complete_token` as `"[redacted]"`; only `schedule_controller_restart`
  returns the raw token (for the final handshake).
- `request_controller_loop_halt`, `clear_controller_loop_halt`,
  `intervene_controller_loop`, and `get_controller_loop_status` return/emit
  normalized loop-health data (flags, lock owner PID + liveness, latest run
  pointers, active PID counts). The control socket's `command_result.data`
  mirrors the same structured payloads.

### Controller recursion profile

Recommended for Codex/Claude/Kimi-style controllers:

- Set `auto_start_task=false` (or omit it — `false` is the default).
- Use `restart_command` to relaunch the external controller process.
- Treat `start_task` as optional E2E testing, not the default recursion path.

## Controller Loop Monitoring

For `restart_command` wrapper scripts, loop artifacts live under
`.intendant/controller-loop/`:

- Stable pointers: `latest` (symlink), `latest.pid`, `latest.status.json`,
  `latest.jsonl`, and the singleton `active.lock/` (`pid`, `run_id`,
  `acquired_at`).
- Inspect: `tail -f .intendant/controller-loop/latest/codex.jsonl`,
  `cat .intendant/controller-loop/latest.status.json`.
- Intervention markers: `touch .intendant/controller-loop/request_halt`
  (persistent), `request_halt_after_cycle` (one-shot legacy), `request_stop`
  (graceful), `request_abort` (immediate). History:
  `.intendant/controller-loop/latest/intervention.log`.
- Per-run PIDs: `.intendant/controller-loop/<run_id>/wrapper.pid` and
  `codex.pid`. The Codex wrapper applies stop/abort to the recorded Codex
  process and its visible descendants so nested app-server children are not
  orphaned.

## Typical Agent Workflow

1. `get_status` for the current phase and budget.
2. Poll `get_logs` with `since_id` to stream new events (or subscribe to
   `intendant://logs`).
3. On an approval, `get_pending_approval` gives the command preview → `approve`,
   `deny`, or `skip`.
4. On an `askHuman`, `get_pending_input` gives the question → `respond`.
5. `quit` when done.

## MCP Client

Intendant can also be an MCP **client**, connecting to external MCP servers
configured in `intendant.toml` so the agent can use their tools alongside
Intendant's native ones (`mcp_client.rs`).

### Configuration

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

At startup, `McpClientManager::connect_all()` spawns each server, discovers its
tools, and registers them as `mcp__<server>_<tool>` (e.g. a `filesystem` server's
`read_file` → `mcp__filesystem_read_file`). Tool calls with the `mcp__` prefix
are routed to the right server. If a server fails to connect, it is skipped with
a warning; other servers and native tools keep working.

Outbound calls pass the controller's `tool_call` approval gate before dispatch.
That category defaults to `ask`, so Medium autonomy prompts unless the owner
explicitly configures `tool_call = "auto"`.

Returned text is not inserted raw. Intendant preserves `is_error`, quotes and
labels the content as untrusted external data, normalizes or exposes control
and invisible formatting, and caps the complete rendered result at 64 KiB.
Transport error text uses the same boundary; non-text and structured payloads
are omitted from this text bridge. This reduces prompt-injection and context
exhaustion risk but cannot make the server's data or claims trustworthy.

### Trust model — read this before adding a server

Each `[[mcp_servers]]` entry is launched as a **child process with the user's
full privileges**:

```rust
let mut cmd = Command::new(&config.command);
cmd.args(&config.args);
let transport = TokioChildProcess::new(cmd)?;   // mcp_client.rs
```

Intendant performs **no checksum verification, no signature check, and no
sandboxing** of MCP server binaries. Adding an MCP server is equivalent to adding
a line to your `~/.zshrc` that runs a binary.

Mitigating defaults: `mcp_servers = []` by default, and `intendant.toml` is
**git-ignored**, so the repo ships no MCP servers. Treat copying an
`intendant.toml` between machines like copying shell rc files — read it before
you source it.

## See Also

- [Autonomy & Approvals](./autonomy.md) — the autonomy model that gates
  approvals.
- [Integrations](./integrations.md) — `ControlMsg`, the control socket, and the
  web gateway WebSocket protocol.
