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
user-display toggle or `intendant ctl display grant-user` — and the agent keeps
access to the user's display for the rest of the session (used by both
[computer use](./computer-use-and-audio.md) and WebRTC streaming). Revoke from
the same places to drop it.
