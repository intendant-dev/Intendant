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
`deny` (always deny — surfaced as an approval that will be denied).

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
connected since the request appeared, a claimed daemon nudges the Connect
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
  `NetworkRequest`; High asks for none of them.

## Action classification

Commands are classified into categories by inspecting the command JSON
(`classify_command`):

| Category | Examples |
|----------|----------|
| FileRead | `inspectPath`, `recallMemory` |
| FileWrite | `editFile`, `writeFile`, `storeMemory` |
| FileDelete | shell commands with `rm`, `rmdir` |
| CommandExec | `execAsAgent`, `execPty` |
| NetworkRequest | shell commands with `curl`, `wget`, `ssh`, `git` |
| Destructive | shell commands with `rm -rf`, `kill`, `dd`, `mkfs`, `sudo` |
| HumanInput | `askHuman` |
| LiveAudioSpawn | `spawn_live_audio` (voice sessions, phone calls) |
| DisplayControl | user-session display access (session grant) |
| ToolCall | external-agent MCP/tool approval category |

For shell commands (`execAsAgent`/`execPty`), the command string is further
inspected for destructive patterns, network tools, and file writes (redirects,
`tee`, `mv`, `cp`). A `sudo` prefix is flagged Destructive *and* the command
after `sudo` is classified too. When multiple categories apply, the highest-
severity one drives the prompt label.

`ToolCall` is not produced by ordinary runtime `classify_command`; it is used by
external-agent approval routing through `external_approval_decision`.

## DisplayControl session grant

`DisplayControl` uses a **session-grant** model: approve once — the dashboard's
**Share with agent** action (or its v1 user-display toggle) or
`intendant ctl display grant-user` — and the agent keeps
access to the user's display for the rest of the session (used by both
[computer use](./computer-use-and-audio.md) and WebRTC streaming). Revoke from
the same places to drop it. The grant is enforced fail-closed at the CU
executor on every platform; only the owner's own surfaces (dashboard, local
`ctl`, the owner-wired stdio MCP transport) may reach the user display
without it, because their call is the opt-in. Note the grant is a single
per-daemon flag: once granted, it holds for every principal the IAM layer
lets at the display tools until revoked.

The dashboard's **View this machine** action is *not* a `DisplayControl`
grant: it opens a **private user view** — a capture session flagged
`agent_visible = false` that streams to the owner's dashboards only. It
never touches this grant, and the session itself is skipped by every
agent-facing display lookup (a second fence, independent of the flag —
see [Computer Use](./computer-use-and-audio.md)). Revoking *any*
user-display session — shared or private — clears the per-daemon grant
flag: over-revocation is the fail-closed direction.
