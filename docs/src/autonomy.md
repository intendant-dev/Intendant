# Autonomy & Approvals

Autonomy controls which actions require human approval (`autonomy.rs`). It
layers three mechanisms, enforced in the agent loop and surfaced identically
by every frontend.

> **Frontends are display-only clients of the control plane.** They render
> state and emit [`ControlMsg`](./integrations.md) values onto the `EventBus`;
> they do **not** mutate shared state directly. The centralized
> `control_plane.rs` is the single writer for autonomy level, external-agent
> backend, runtime config, etc. Its module doc states this explicitly:
> *"Frontends remain display-only — they render state changes but never write
> to shared state."* Approval resolutions go through the shared
> `ApprovalRegistry`; everything else is a `ControlMsg`.

## Layer 1 — global level

Set with `--autonomy`, from the dashboard, or with
`intendant ctl settings autonomy`. `AutonomyLevel`:

| Level | Behavior |
|-------|----------|
| Low | Ask before every category except `FileRead` |
| Medium (default) | Ask for writes, deletes, destructive, and network |
| High | Don't ask for the above (only the always-ask categories below) |
| Full | Ask only for the always-ask categories: `HumanInput` and `LiveAudioSpawn` |

## Layer 2 — per-category rules

The `[approval]` section of `intendant.toml` sets a per-category rule that
overrides the global level: `auto` (always approve), `ask` (require approval),
`deny` (refuse without prompting). A `deny` rule on a runtime batch ends the
task (`Denied by policy`); on a controller-dispatched tool call (below) it
refuses that one call with an error tool result and lets the session continue.

## Layer 3 — per-action approval

When approval is required, the agent loop pauses and the frontends surface the
command preview and category — the dashboard shows an approval card, and
MCP / `intendant ctl` expose the same choices ([MCP Server](./mcp-server.md)):
approve, skip (continue with the next command), approve-all (which also flips
autonomy to Full), or deny (and stop).

A pending request does not depend on someone happening to look at a
dashboard: an open-but-hidden tab badges its title/favicon with the pending
count and can raise a browser notification
([Web Dashboard](./web-dashboard.md)), and when *no* dashboard has been
connected since the request appeared, a linked daemon nudges the Connect
rendezvous after a grace period so opted-in browsers get a Web Push that
names only the request kind and the daemon/session labels — never the
command or question itself (`attention_nudge.rs`;
[Hosted rendezvous](./self-hosted-rendezvous.md)). Headless daemons with no
frontend at all still auto-deny as before.

## Questions and notifications are not permissions

Two agent→user primitives share the approval *plumbing* (id space, rail,
attention chain) without being approvals:

- **Questions** (`ask_user`, the native loop's askHuman, supervised Claude
  Code's AskUserQuestion) request *input*. Autonomy policy never
  auto-resolves one — no level, per-category rule, or session-wide
  approve-all grant answers a question — and answering (or approving one
  through a verbs-only surface) never widens command autonomy. The asking
  agent blocks until an answer, a dismissal, or its wait expires; the
  timeout and the no-frontend shapes both hand the agent explicit
  best-judgment guidance instead of a fabricated choice.
- **Notifications** (`notify_user`) request *nothing*: fire-and-forget,
  display-only, never blocking. `urgency` picks the delivery escalation —
  `info` renders a dashboard toast plus a transcript row; `attention` also
  registers in the attention center (tab badge, hidden-tab browser
  notification); `urgent` also sends an immediate Connect nudge — an
  explicit escalation, so it skips the pending-request grace period while
  keeping the per-session cooldown and the content-free payload (kind +
  labels only). Pending `ask_user` questions ride the ordinary
  pending-request nudge above; they need no separate kind.

`urgency: urgent` is also the designed attach point for audible/voice
escalation ("ring the owner"): the `UserNotification` event carries the
urgency on the bus, so a future voice leg (see [Presence](./presence.md))
can subscribe to it without a new wire shape. No such leg exists yet.

## How `needs_approval` actually resolves

The precise logic (`Autonomy::needs_approval`) has nuances worth knowing:

- **Always ask, regardless of level:** `HumanInput` and `LiveAudioSpawn` — these
  always require a human even at Full.
- **`DisplayControl`** — asks on *first* use, then the session grant takes over
  (`return !user_display_granted`).
- **Full** — auto-approves everything else.
- **Low** — asks for everything except `FileRead` (a `deny` rule still blocks).
- **Medium / High** — start from the per-category rule. For an `ask` rule,
  Medium asks only for `FileWrite` / `FileDelete` / `Destructive` /
  `NetworkRequest` / `ToolCall` (the last only under an explicit
  `tool_call = "ask"` — its default rule is `auto`); High asks for none of
  them.

## Approval dedup (what "remembered" means)

Approving an action records its **dedup source** so an identical retry does
not re-prompt. Two properties bound that memory:

- **Per session.** One autonomy state backs every native session of a daemon,
  but remembered approvals are bucketed by session id — an approval in one
  session never silences a prompt in another. (The approve-all **level**
  escalation stays daemon-wide by design; see the table below.)
- **Content-aware.** The dedup source is not the display preview. For
  `writeFile`/`editFile` it includes a digest of the full command (minus the
  per-call `nonce`), so approving one edit of a path never covers different
  content aimed at the same path; controller-dispatched tool calls digest
  their full arguments the same way. Exec commands keep exact-string matching
  (with the display/nonce normalization that recognizes benign retries).

## Controller-dispatched tools

Tools the controller executes itself never reach the runtime, so
`classify_command` never sees them. They consult the same approval flow
through a dedicated chokepoint (`gate_controller_tool_call` in the agent
loop), **before any side effect**, honoring the `[approval] tool_call` rule
and the autonomy level, with the same prompt, session-log rows
(`waiting` / `approved` / `denied` / `denied-policy` / `dedup-auto-approved`),
and bus events (`ApprovalRequired` / `ApprovalResolved` / `AutoApproved`)
that runtime batches get:

| Tool | Gate |
|------|------|
| Outbound MCP calls (`mcp__*`) | `tool_call` gate, per call, before dispatch |
| `invoke_skill` | `tool_call` gate before the skill body loads |
| `spawn_sub_agent` | `tool_call` gate before the child session exists |
| `workflow_checkpoint` | `tool_call` gate before the coordination write |
| `wait_sub_agents` | ungated — a pure join on children whose spawn already passed the gate |
| `peer` | dedicated gate peer-side: the profile the peer issued this daemon plus the peer's own approval flow |
| `shared_view` | dedicated `user_display_granted` opt-in for user-display show/capture |
| `spawn_live_audio` | dedicated always-ask consent gate (never auto-approved) |

Semantics at the gate: the default `tool_call = "auto"` dispatches without a
prompt at Medium/High (orchestration and MCP stay usable at default
autonomy) while still emitting `AutoApproved`; **Low always prompts**; an
explicit `ask` rule prompts at Medium; `deny` refuses the call at every
level — absolute, like a runtime-batch deny rule — returning an error tool
result with no dispatch. At the prompt, skip refuses just that call and the
session continues; deny stops the task, exactly like a runtime-command deny.
Headless with no approver surface fails closed. Calls are gated and
dispatched strictly in order, so a later call never runs while an earlier
prompt in the batch is unresolved.

## Action classification

Commands are classified into categories by inspecting the command JSON
(`classify_command`):

| Category | Examples |
|----------|----------|
| FileRead | `inspectPath` |
| FileWrite | `editFile`, `writeFile` |
| FileDelete | shell commands with `rm`, `rmdir` |
| CommandExec | `execAsAgent`, `execPty` |
| NetworkRequest | shell commands with `curl`, `wget`, `ssh`, `git` |
| Destructive | shell commands with `rm -rf`, `kill`, `dd`, `mkfs`, `sudo` |
| HumanInput | `askHuman` |
| LiveAudioSpawn | `spawn_live_audio` (voice sessions, phone calls) |
| DisplayControl | user-session display access (session grant) |
| ToolCall | external-agent MCP/tool approval category |

For shell commands (`execAsAgent`/`execPty`), the command string is further
inspected for destructive patterns (including long-form flags, absolute
binary paths like `/bin/rm`, and `find … -delete`/`-exec rm`), network
tools, and file writes (redirects, `tee`, `mv`, `cp`). A `sudo` prefix is
flagged Destructive *and* the command after `sudo` is classified too. When
multiple categories apply, the highest-severity one drives the prompt label.

The shell classifier is **best-effort keyword matching for approval
prompting — UX, not a security boundary**: string matching cannot see
through variable indirection, subshells, or novel spellings. The runtime's
filesystem/exec sandbox is what actually confines commands; an evasion
dodges the prompt, never the sandbox.

`ToolCall` is not produced by ordinary runtime `classify_command`; it governs
[controller-dispatched tools](#controller-dispatched-tools) and
external-agent approval routing through `external_approval_decision`.

## Sandbox denial consent

The runtime write sandbox (on by default — see
[Configuration § `[sandbox]`](./configuration.md)) is a hard OS wall, but a
denial is not a dead end: when a runtime batch result carries a write
denied by the sandbox, the daemon classifies it
(`sandbox_denial_grant_offer`) and raises a **"Sandbox" card on the
question rail** offering three resolutions:

- **Allow for this session** — the path joins that session's write set at
  the next runtime spawn; gone on daemon restart.
- **Always allow** — the grant is live-applied to the daemon's write set
  immediately (no restart) and persisted to `[sandbox]
  extra_write_paths` in the session project's `intendant.toml`.
- **Keep denied** — nothing changes.

The model simultaneously sees an `[intendant]` note on the denied tool
result explaining that the sandbox (not the task) blocked the write and
that a grant prompt was raised — so it retries after a grant instead of
giving up. Approval and consent stay distinct layers: an approved command
can still be denied by the wall, and the card is how the wall becomes
negotiable without dropping it.

Guardrails: the card shows the exact path a grant would cover (for a
not-yet-existing target, the nearest existing ancestor — honestly wide
when it is wide); a filesystem root is never offered; credential
locations (`~/.ssh`, `~/.gnupg`, the intendant config home, any `.env`)
are never offered — on Linux, Landlock has no deny layer under a grant,
so offering those would genuinely open them. Denials inside the grant
set (plain filesystem permissions) get no card. Each (session, path)
offers once per daemon run; headless runs get the note only.

## DisplayControl session grant

`DisplayControl` uses a **session-grant** model: approve once — the dashboard's
**Share with agent** action (or its v1 user-display toggle) or
`intendant ctl display grant-user` — and the agent keeps
access to the user's display for the rest of the session (used by both
[computer use](./computer-use-and-audio.md) and WebRTC streaming). Revoke from
the same places to drop it. The grant is enforced fail-closed at the CU
executor on every platform; only an owner/root surface (an owner/root
dashboard, local `ctl`, or the owner-wired stdio MCP transport) may reach the
user display without it, because its call is the opt-in. A scoped role's
`display.view` or `display.input` permission covers agent-visible displays
only; neither permission is proof that the owner chose to expose the private
user session. Note the grant is a single per-daemon flag: once granted, it
holds for every principal the IAM layer lets at the display tools until
revoked.

The dashboard's **View this machine** action is *not* a `DisplayControl`
grant: it opens a **private user view** — a capture session flagged
`agent_visible = false` that streams only to owner/root dashboards. That
ceiling applies on both browser transports: the legacy `/ws` signaling/input
lane and the verified dashboard-control DataChannel. The action never touches
the standing grant, and the session itself is skipped by every generic,
agent-facing display lookup (a second fence, independent of the flag — see
[Computer Use](./computer-use-and-audio.md)). Revoking *any* user-display
session — shared or private — clears the per-daemon grant flag: over-revocation
is the fail-closed direction.

## The display request rail (doorbell)

Scoped callers — supervised external agents, session-scoped grants,
federated peers — cannot perform the owner's opt-in themselves
(`grant_user_display` refuses them). What they can do is **ask**: the
`request_user_display` MCP tool (`intendant ctl display request`) raises a
dedicated dashboard popup with the agent's short reason and the requested
access level, then blocks until the user decides or the wait window
(default 120 s, max 600 s) closes.

**Never auto-approved, by construction.** Display requests live in their
own registry and id space, deliberately outside the approval registry:
`approve` / `approve_all` / any autonomy level or per-category rule cannot
reach them. The only resolution is the dedicated
`ResolveDisplayRequest` control message — the popup's **Allow** /
**Deny** / **Deny for this session** buttons — accepted only from an
owner/root surface. Both it and `GrantUserDisplay` are classified
`display.input`, but that permission is only the coarse admission floor:
resolving or granting additionally requires owner/root authority on either
browser transport. `RevokeUserDisplay` remains available to an otherwise
authorized scoped caller because de-escalation is the fail-safe direction. On
approve, the control plane mints the grant through the same state flip and
events the owner's own grant takes.

Two access levels:

- **`view`** — the display stream activates agent-visible (dashboard tile
  + `list_frames`/`read_frame`), but the `DisplayControl` grant flag stays
  **off**: computer-use input and screenshots against `user_session`
  remain denied at the CU executor's fail-closed gate.
- **`view_and_control`** — the full session grant described above.

Three durations, chosen by the user at approval: **this session**
(auto-revokes when the requesting session ends), **15 minutes** (a timer
revokes through the normal revoke path; superseded if the owner grants or
revokes manually in the meantime), **until revoked**.

Spam resistance: one pending request per session (a second call reports
the existing one); a deny — or a timeout, which counts as declined by
absence — starts a 5-minute per-session cooldown during which new
requests are refused without a popup; **Deny for this session**
suppresses the session server-side until it ends. Pending requests feed
the attention chain (tab badge, hidden-tab notification, and the
Connect Web Push nudge with kind `display_request` — the push carries
only the kind and session label, never the reason text). On a headless
daemon with no owner surface, requests are refused immediately
(`unavailable`) instead of blocking — the same fail-closed posture as
headless approvals.

## "Approve all" scope, by surface

The same two words appear on several surfaces with deliberately different
blast radii. What each one actually grants:

| Surface | What "approve all" does | Scope | Lifetime |
|---|---|---|---|
| Native runtime approvals | Sets the autonomy level to **Full** (`apply_user_approval`) | **Daemon-wide for native sessions** — one shared autonomy state backs every native session | In-memory: until lowered again (autonomy control or restart, which returns to the configured level) |
| External agents (Codex / Claude Code) | Auto-approves that backend's subsequent approval requests (`approve_all_session`) | **That one external session only** — deliberately never touches native autonomy | The external session's lifetime |
| Live audio | Does not exist. Every live-audio spawn requires its own explicit human approval; with no approver surface the spawn is denied outright | Per spawn | One consent per spawn |
| Questions (`ask_user` and kin) | Nothing. Questions are not permissions: no level or approve-all grant answers one, and an `Answer` aimed at a command approval fails closed (denied) | — | — |
| Display requests (`user_session` rail) | Nothing. The rail lives outside the approval id space; approve/approve-all can never mint a display grant there. (Approving a **DisplayControl-category runtime action** is different: the first such approval grants agent-visible user-display access session-wide — that approval *is* the opt-in) | Rail: per request | Grant durations are the rail's own (this session / 15 min / until revoked) |

The asymmetry between the first two rows is intentional: a native
approve-all is the operator saying "run autonomously" to *their daemon*,
while a button on one supervised Codex/Claude session must not escalate
every other surface of the daemon.
