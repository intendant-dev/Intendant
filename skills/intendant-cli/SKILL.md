---
name: intendant-cli
description: Use to operate a running Intendant daemon from the CLI — sessions, approvals, displays and screenshots, computer-use input, browser workspaces, audio, and federated peers (message or delegate to another machine's agent, or drive its screen directly with --peer). Prefer `intendant ctl` over broad MCP tools to keep model context small.
---

# Intendant CLI

Use `intendant ctl` for Intendant control that is not already available as a small MCP bootstrap tool. The CLI talks to the running dashboard/MCP endpoint and exposes broad capabilities lazily through subcommand help.

When `$INTENDANT` is set, run `"$INTENDANT" ctl ...`; Intendant sets it for supervised Codex and Claude Code sessions so the exact controller binary is available even when `intendant` is not on PATH (the injected `INTENDANT_MCP_URL` also carries the loopback auth token and session scope). Otherwise use `intendant ctl ...`.

Start with:

```bash
"${INTENDANT:-intendant}" ctl --help
"${INTENDANT:-intendant}" ctl status --json
"${INTENDANT:-intendant}" ctl tools list
```

Quick recipes — the highest-traffic one-liners; everything else is one `--help` away:

```bash
"${INTENDANT:-intendant}" ctl display screenshot --output shot.png    # see a local display
"${INTENDANT:-intendant}" ctl cu actions --actions '[{"type":"click","x":100,"y":200}]' --output after.png
"${INTENDANT:-intendant}" ctl peer list                               # federated peers + their displays
"${INTENDANT:-intendant}" ctl peer task <peer-id> "instructions"      # the peer's own agent executes
"${INTENDANT:-intendant}" ctl --peer <id> display screenshot --output peer.png   # drive another machine
"${INTENDANT:-intendant}" ctl --peer <id> cu actions --actions '[{"type":"click","x":100,"y":200}]'
```

Useful groups:

- `"${INTENDANT:-intendant}" ctl display --help` for displays, frames, screenshots, and display claims.
- `"${INTENDANT:-intendant}" ctl browser --help` for browser workspaces, including local CDP-backed browsers and lease management.
  CDP workspaces prefer managed Chromium/Chrome-for-Testing; run `"${INTENDANT:-intendant}" setup browsers` to install/repair the managed browser cache, and use `--provider system_cdp` or `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1` to opt into system Chrome/Chromium on macOS.
- `"${INTENDANT:-intendant}" ctl cu --help` for computer-use actions; `ctl cu actions --help` prints the per-action JSON shapes with an example. `ctl cu elements` reads the frontmost app's UI element tree (cheap textual grounding — click the center of a reported frame; user-session only via macOS AX, Linux AT-SPI, or Windows UIA).
- `"${INTENDANT:-intendant}" ctl shared --help` for shared display collaboration.
- `"${INTENDANT:-intendant}" ctl peer --help` for federated peers — list peers, message a peer's agent, delegate tasks (`ctl peer list|message|task`; the peer executes under its own autonomy/IAM).
- Global `--peer ID` routes **any** ctl subcommand to a federated peer's `/mcp` over mTLS — no local daemon needed. It resolves the `[[peer]]` entry in `intendant.toml` by label, card_url host, or `intendant:<label>` id (falling back to the user-level `~/.intendant/peers.toml` when the project has no match, so paired peers work from any directory), then e.g. `ctl --peer dell display screenshot --output peer.png` or `ctl --peer dell cu actions --actions '[...]'` drive the peer's screen directly. `ctl --peer dell cu elements` reads the **peer's** frontmost UI element tree — the cheap first look before screenshots (needs the peer's platform accessibility stack). The peer's IAM profile for this daemon decides: screenshots and `cu elements` need display view (read-only-display or better), `cu actions` needs display input (peer-operator/peer-root). A denial is the peer's owner not having granted it — report, don't retry.
- `"${INTENDANT:-intendant}" ctl approval --help` and `"${INTENDANT:-intendant}" ctl input --help` for pending approval/input flows.
- `"${INTENDANT:-intendant}" ctl context --help` for managed-context rewind/backout.
- `"${INTENDANT:-intendant}" ctl controller --help` for controller-loop and restart controls.
- `"${INTENDANT:-intendant}" ctl tools schema TOOL` and `"${INTENDANT:-intendant}" ctl tools call TOOL --args JSON` for rare or newly-added MCP tools.

Prefer `--json` when the output will be inspected by an agent. Use `--session ID` when operating on a specific session. Use `--managed-context managed` for rewind/backout commands.
