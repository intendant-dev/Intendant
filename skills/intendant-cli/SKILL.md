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
"${INTENDANT:-intendant}" ctl session note "Milestone: tests green"   # display-only note in your transcript
"${INTENDANT:-intendant}" ctl session note "Before/after" --image before.png --image after.png
"${INTENDANT:-intendant}" ctl ask "Which database?" --option "postgres:Existing infra" --option sqlite   # BLOCKS; prints the answer
"${INTENDANT:-intendant}" ctl notify "Long build finished — opening the PR" --title CI   # fire-and-forget
"${INTENDANT:-intendant}" ctl peer list                               # federated peers + their displays
"${INTENDANT:-intendant}" ctl peer task <peer-id> "instructions"      # the peer's own agent executes
"${INTENDANT:-intendant}" ctl --peer <id> display screenshot --output peer.png   # drive another machine
"${INTENDANT:-intendant}" ctl --peer <id> cu actions --actions '[{"type":"click","x":100,"y":200}]'
```

Useful groups:

- `"${INTENDANT:-intendant}" ctl display --help` for displays, frames, screenshots, and display claims.
- `"${INTENDANT:-intendant}" ctl display request --reason "why" [--access view|control] [--wait SECS]` to **ask the user for their real display** (display 0 / `user_session`) when yours is a scoped session that cannot grant it. This raises a dashboard popup with your reason and blocks until the user clicks (default 120 s) — their click is the only thing that can grant it; no autonomy setting can. `--access view` = you can see the stream (frames/dashboard) but CU input/screenshots on `user_session` stay denied; `--access control` = the full grant. Read the JSON `status`: on `denied`/`timed_out` a cooldown applies — do not re-ask until `retry_after_secs` passes; on `denied_for_session`, never ask again in this session. Ask only when the user's own screen genuinely matters (an agent-owned virtual display needs no permission). A granted answer (or `already_granted`) may still carry an `os_readiness` block: the grant is Intendant authority only, and any OS layers listed there (macOS Screen Recording/Accessibility, Wayland portal, missing display) are still blocking actual CU — relay their `fix` steps to the user instead of retrying.
- `"${INTENDANT:-intendant}" ctl display status [--target TARGET]` for **per-layer CU readiness** with a fix per blocked layer: Intendant display authority, OS screen-capture permission, accessibility permission, target display availability, input backend. Run it FIRST when a screenshot/read_screen/CU call fails or before starting user-display work — a held grant does not imply the OS permissions. Probes live state every call; `unknown` layers count as not ready.
- `"${INTENDANT:-intendant}" ctl browser --help` for browser workspaces, including local CDP-backed browsers and lease management.
  CDP workspaces prefer managed Chromium/Chrome-for-Testing; run `"${INTENDANT:-intendant}" setup browsers` to install/repair the managed browser cache, and use `--provider system_cdp` or `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1` to opt into system Chrome/Chromium on macOS.
- `"${INTENDANT:-intendant}" ctl cu --help` for computer-use actions; `ctl cu actions --help` prints the per-action JSON shapes with an example. `--observe pixels|ax|auto|none` picks the post-action observation: `pixels` (default) attaches a clean screenshot, `ax` attaches the frontmost UI element tree as text instead (far cheaper; user-session targets only), `auto` picks the tree when usable with screenshot fallback, `none` returns results only (chain batches, observe once at the end). The result names the observation it carries and why. `--annotate` opts into click markers on captured screenshots (off by default — they obscure the controls being verified). `ctl cu elements` reads the frontmost app's UI element tree standalone (cheap textual grounding — click the center of a reported frame; user-session only via macOS AX, Linux AT-SPI, or Windows UIA). Long values/titles come capped at 80 chars with a `… [N chars total, #hash]` marker; add `--full-values` only when you need the exact long text (e.g. a full URL).
- `"${INTENDANT:-intendant}" ctl session note --help` to post a **display-only note** into your session transcript — show the user progress, findings, or before/after screenshots without touching any model's context. The note appears live in the dashboard transcript and persists for replay; each `--image` file (png/jpg/gif/webp/bmp, ≤4 MB each, ≤6 per note) renders as a clickable thumbnail. Use `--source LABEL` to label the entry.
- `"${INTENDANT:-intendant}" ctl shared --help` for shared display collaboration.
- `"${INTENDANT:-intendant}" ctl peer --help` for federated peers — list peers, message a peer's agent, delegate tasks (`ctl peer list|message|task`; the peer executes under its own autonomy/IAM).
- Global `--peer ID` routes **any** ctl subcommand to a federated peer's `/mcp` over mTLS — no local daemon needed. It resolves the `[[peer]]` entry in `intendant.toml` by label, card_url host, or `intendant:<label>` id (falling back to the user-level `~/.intendant/peers.toml` when the project has no match, so paired peers work from any directory), then e.g. `ctl --peer dell display screenshot --output peer.png` or `ctl --peer dell cu actions --actions '[...]'` drive the peer's screen directly. `ctl --peer dell cu elements` reads the **peer's** frontmost UI element tree — the cheap first look before screenshots (needs the peer's platform accessibility stack). The peer's IAM profile for this daemon decides: screenshots and `cu elements` need display view (read-only-display or better), `cu actions` needs display input (peer-operator/peer-root). A denial is the peer's owner not having granted it — report, don't retry.
- `"${INTENDANT:-intendant}" ctl ask --help` to ask the user a **structured question** on the dashboard and **block for the answer** (printed to stdout; `--json` for `{status, answer, answers}`). Ask **before destructive or hard-to-reverse choices** — schema changes, force-pushes, deleting data, picking between materially different designs — instead of guessing. Up to 4 `--option "Label[:desc]"` choices; none (or `--free-text`) for a typed answer; `--multi` to allow several. Default wait 300 s (max 900, `--wait N`); on timeout it prints proceed-on-best-judgment guidance and exits nonzero — do that rather than blocking your task forever. A question is a request for input, never permission, and is never auto-approved.
- `"${INTENDANT:-intendant}" ctl notify --help` to send a **fire-and-forget notification** (toast + transcript row; returns immediately). Notify on **long-task completion or milestones** (`--urgency info`, the default), use `--urgency attention` when the user should look soon (badges the tab, raises a browser notification when the tab is hidden), and reserve `--urgency urgent` for **being genuinely blocked** or needing prompt human action — it additionally pushes a content-free nudge to the owner's phone/browser and is cooldown-limited per session, so crying wolf mutes real alarms. Never use `notify` to ask something — that is `ctl ask`.
- `"${INTENDANT:-intendant}" ctl approval --help` and `"${INTENDANT:-intendant}" ctl input --help` for pending approval/input flows.
- `"${INTENDANT:-intendant}" ctl context --help` for managed-context rewind/backout.
- `"${INTENDANT:-intendant}" ctl controller --help` for controller-loop and restart controls.
- `"${INTENDANT:-intendant}" ctl tools schema TOOL` and `"${INTENDANT:-intendant}" ctl tools call TOOL --args JSON` for rare or newly-added MCP tools.

Prefer `--json` when the output will be inspected by an agent. Use `--session ID` when operating on a specific session. Use `--managed-context managed` for rewind/backout commands.
