# Autonomy & Approvals

Autonomy controls which actions require human approval
(`crates/intendant-core/src/autonomy.rs`). It layers three mechanisms, enforced
in the agent loop and surfaced identically by every frontend.

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
| Medium (default) | Apply category rules; defaults ask for arbitrary shell execution, writes, deletes, and destructive actions (`network` defaults to auto for the structured `browse` tool) |
| High | Auto-approve ordinary `ask` rules; native policy denials and hard gates remain |
| Full | Native runtime and controller actions auto-approve except policy denials and the `HumanInput` / `LiveAudioSpawn` hard gates; see the external-agent caveat below |

## Layer 2 — per-category rules

The `[approval]` section of `intendant.toml` sets a per-category baseline:
`auto`, `ask`, or `deny`. For native runtime batches and
controller-dispatched tools, `deny` is consulted before the autonomy level and
refuses the action without presenting an approval card. A runtime-batch denial
ends the task (`Denied by policy`); a controller-tool denial returns an error
for that one call and lets the session continue. `auto` and `ask` remain
level-sensitive: Low intentionally prompts for ordinary categories even when
their rule is `auto`, while High and Full auto-approve ordinary `ask` rules.
None of these rules bypasses the human-input/live-audio hard gates or the
separate user-display grant. External-agent requests take a separate path,
described below.

`CommandExec` is capability-composed rather than parser-composed. Its effective
rule is the strictest of `command_exec`, `file_read`, `file_write`,
`file_delete`, `network`, `destructive`, and `display_control` (`deny` >
`ask` > `auto`). A shell can reach all of those effects through interpreters,
subshells, variable expansion, or binaries the classifier has never seen, so
setting only `command_exec = "auto"` cannot weaken a stricter effect rule. To
auto-allow arbitrary shell execution, every reachable category must permit it.

## Layer 3 — per-action approval

When a native approval is required, the agent loop pauses and the frontends
surface the command preview and category — the dashboard shows an approval
card, and MCP / `intendant ctl` expose the same choices
([MCP Server](./mcp-server.md)): approve, skip (continue with the next command),
approve-all (which also flips native autonomy to Full), or deny (and stop).
External-agent approval cards use the same verbs, but their approve-all is
session-scoped and does not change native autonomy; the scope table below
spells out that asymmetry.

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
  best-judgment guidance instead of a fabricated choice. A question may
  carry **preview cards** (show, then ask — prototype variants,
  before/after states): self-contained HTML rendered only inside a
  sandboxed opaque-origin iframe, raster images, or inline text snippets;
  blob kinds live in the session upload store and travel as references
  (see the `ask_user` row in the MCP chapter and `intendant ctl ask
  --help` for caps and flags).
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

## Scheduled Sessions Do Not Bypass Autonomy

Agenda schedule approval is a separate owner decision over an exact one-shot
manifest (goal and fire time). It authorizes the daemon to create the session at
that instant; it does **not** pre-approve the actions the session later proposes.
The scheduled task is dispatched as a normal supervised session with its own
agent-session principal, sandbox, and the same autonomy/approval machinery
described here. Missed or uncertain occurrences are terminal and never
auto-retried.

## How `needs_approval` actually resolves

The precise logic (`AutonomyState::needs_approval`) has nuances worth knowing:

- **Always ask, regardless of level:** `HumanInput` and `LiveAudioSpawn` — these
  always require a human even at Full.
- **Explicit deny is checked before the level on native paths:** the runtime
  loop and controller-tool gate reject the action without presenting an
  approval.
- **`DisplayControl` below Full** — asks until the separate user-display grant
  is present (`return !user_display_granted`). Full bypasses this category
  prompt, but does not mint the executor's user-display grant.
- **Full** — auto-approves every other native action that survived the
  explicit-deny check.
- **Low** — asks for everything except `FileRead` (a `deny` rule still blocks).
- **Medium / High** — start from the per-category rule. For an `ask` rule,
  Medium asks only for `CommandExec` / `FileWrite` / `FileDelete` /
  `Destructive` / `NetworkRequest` / `ToolCall`; High asks for none of them.
  `ToolCall` defaults to `ask`, so the shipped Medium posture prompts before
  outbound MCP, skill invocation, orchestration spawns/checkpoints, and
  external-agent MCP permission requests.

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
  their full arguments the same way. Exec commands keep byte-exact string
  matching. In particular, display selectors and `$NONCE[...]` process
  references remain part of the identity because they change the target and
  may contain executable shell syntax. Only the structured runtime call's
  top-level JSON `nonce` is removed before hashing.

## Controller-dispatched tools

Tools the controller executes itself never reach the runtime, so
`classify_command` never sees them. They consult the same approval flow
through a dedicated chokepoint (`gate_controller_tool_call` in the agent
loop), **before any side effect**, honoring the `[approval] tool_call` rule
and the autonomy level. Prompted calls use the same `ApprovalRequired` /
`ApprovalResolved` bus flow as runtime batches; prompt, dedup, and policy
outcomes write the corresponding session-log rows (`waiting`, `approved`,
`approve-all`, `skipped`, `denied`, `denied-no-approver`, `denied-policy`, or
`dedup-auto-approved`), while an automatic policy permit emits
`AutoApproved`.

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

Semantics at the gate: the default `tool_call = "ask"` prompts at Medium and
Low; High/Full auto-approve it. A user who deliberately trusts the configured
tool boundary can set `tool_call = "auto"` to dispatch without a prompt at
Medium (with an `AutoApproved` audit event). `deny` refuses the call at every
level — absolute, like a runtime-batch deny rule — returning an error tool
result with no dispatch. At the prompt, skip refuses just that call and the
session continues; deny stops the task, exactly like a runtime-command deny.
Headless with no approver surface fails closed. Calls are gated and dispatched
strictly in order, so a later call never runs while an earlier prompt in the
batch is unresolved.

External-agent approval requests use the deliberately separate
`external_approval_decision` path. Below Full, an explicit `deny` rejects,
`ToolCall` plus an explicit `auto` auto-approves, and every other request is
shown to the human even when the native category default is `auto`. Because
`ToolCall` defaults to `ask`, external-agent MCP/tool permission requests are
shown at Medium unless the owner opts into auto.
At Full, the current implementation returns `AutoApprove` **before** reading
the category rule, so an external approval request bypasses an explicit
`deny`; the `external_approval_full_overrides_deny` test codifies that
precedence. Separately, the external event drain consults its per-session
approve-all flag before the current policy decision, so a category changed to
`deny` after that grant is also auto-approved for the remainder of the
session. External-agent denials are therefore not absolute under those two
conditions. This differs from the native runtime/controller path and is a
current implementation caveat, not an authority guarantee.

## Action classification

Commands are classified into categories by inspecting the command JSON
(`classify_command`):

| Category | Examples |
|----------|----------|
| FileRead | `inspectPath` |
| FileWrite | `editFile`, `writeFile` |
| FileDelete | Reserved/configurable category; the current runtime classifier does not emit it |
| CommandExec | `execAsAgent`, `execPty` |
| NetworkRequest | shell commands with `curl`, `wget`, `ssh`, `git` |
| Destructive | shell commands with `rm -rf`, `kill`, `dd`, `mkfs`, `sudo` |
| HumanInput | `askHuman` |
| LiveAudioSpawn | Dedicated `spawn_live_audio` consent gate (voice sessions, phone calls) |
| DisplayControl | user-session display access (session grant) |
| ToolCall | controller-dispatched tools and external-agent MCP/tool approvals |

For shell commands (`execAsAgent`/`execPty`), the command string is further
inspected for destructive patterns (including long-form flags, absolute
binary paths like `/bin/rm`, and `find … -delete`/`-exec rm`), network
tools, and file writes (redirects, `tee`, `mv`, `cp`). A `sudo` prefix is
flagged Destructive *and* the command after `sudo` is classified too. When
multiple categories apply, the highest-severity one drives the prompt label.
Every shell command also carries `CommandExec`, whose effective policy is the
strictest reachable rule described above.

The shell substring classifier is **display enrichment, not an authorization
parser**: it cannot see through variable indirection, subshells, interpreters,
or novel spellings. Missing one of those spellings no longer downgrades the
decision because `CommandExec` governs the whole shell capability. The
runtime's filesystem/exec sandbox remains an independent hard boundary on
where an approved command can act.

`ToolCall` is not produced by ordinary runtime `classify_command`; it governs
[controller-dispatched tools](#controller-dispatched-tools) and
external-agent approval routing through `external_approval_decision`.

## Sandbox denial consent

The runtime write sandbox (on by default on macOS/Linux and opt-in on Windows;
see the [sandbox configuration](./configuration.md#sandbox)) is a hard OS wall,
but a denial is not a dead end: when a runtime batch result carries a write
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

One current gap narrows that guarantee: the forbidden-offer classifier does not
include the daemon state root's credential-bearing subtrees or Windows'
separate access-certificate directory. macOS' later Seatbelt deny still blocks
its known state-root credential paths, but on Linux/Windows a denied write there
can produce a consent offer when an overlapping project or explicit path makes
the target reachable. That is a security defect, not an intended grant surface.

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
