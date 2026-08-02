---
name: peer-displays
description: "Use when asked to open, create, or activate a display on a federated peer machine (\"open a screen on the dell and show me\"), to put another machine's display in front of the user, or to run GUI work on a peer that the user can watch live. Covers the whole loop — create an agent-owned virtual display on the peer, arrange work on it (peer computer use, or the peer's own agent), and announce it with shared show so it surfaces on the user's dashboard — plus which peer grant each step needs and why an un-announced display stays invisible to the user."
compatibility: Requires a reachable Intendant daemon (supervised sessions have $INTENDANT and INTENDANT_MCP_URL injected) with at least one federated peer paired.
---

> Resolve the CLI first:
>
> ```bash
> INTENDANT="${INTENDANT:-$(command -v intendant || cat "${INTENDANT_HOME:-$HOME/.intendant}/cli-path" 2>/dev/null || echo intendant)}"
> ```
>
> If that resolves nothing anywhere (no `$INTENDANT`, nothing on PATH, no
> `cli-path` descriptor under the Intendant state root), Intendant likely
> isn't on this machine — this skill does not apply; say so and stop. If
> the CLI resolves but the daemon does not answer, that is a DIFFERENT
> stop: say the daemon appears down — do not claim the skill doesn't
> apply. (A running daemon refreshes the descriptor at boot.)

# Peer Displays

"Open a display on peer X and show me" is one loop with three steps. Every
step is a normal ctl command routed to the peer with the global `--peer <id>`
flag (resolve ids with `ctl peer list`):

```bash
"$INTENDANT" ctl peer list                                    # who's paired + displays they already advertise
"$INTENDANT" ctl --peer dell display create                   # 1. agent-owned virtual display on the peer → returns display_id (e.g. 99)
"$INTENDANT" ctl --peer dell cu actions --target display_99 --actions '[...]'   # 2. arrange work on it
"$INTENDANT" ctl --peer dell shared show --target display_99 --reason "watch the run"   # 3. surface it to the user
```

For step 2 you can instead delegate to the peer's own agent —
`"$INTENDANT" ctl peer task dell "start the app on display :99"` — and keep
step 3 for yourself; the announcement is what matters.

## Where the user sees it

A created display **announces itself immediately**: it appears on the primary
dashboard's Live display peer rows and Station peer chips, so the user *can*
open it — but nothing calls their attention. The `shared show` is the
surfacing verb: it raises that peer's shared-view banner on the user's
Activity pane with a click-through to the live pane, exactly like a local
shared view. `"$INTENDANT" ctl --peer dell shared hide` retires the banner
when the moment is over.

**The visibility law:** capture and CU by target string also work on a
display Intendant didn't create (an ssh-launched `Xvfb :99`, say) before any
registration — but invisibly to every user surface until `display create` or
`shared show` announces it. Never leave work "succeeding" on a screen the
owner cannot find; announce it.

## Grants

The peer's IAM profile for **your** daemon decides what it will accept, per
step: `shared show`, screenshots, and `cu elements` need display **view**
(read-only-display or better); `display create` and `cu actions` need display
**input** (peer-operator / peer-root). A denial means the peer's owner has
not granted that operation — report it, don't retry.

## Limits and lifecycle

- `display create` is Xvfb-backed — **Linux peers only** today; a macOS or
  Windows peer answers with a clear error, so target one of its existing
  displays instead (`ctl peer list` shows what it advertises).
- Federated shared-view announcements are live state, not history: the
  banner clears if the peer link drops — re-run `shared show` after a
  reconnect if the collaboration is still on.
- Local flavor: the identical loop works on your own daemon without
  `--peer` (`ctl display create` → `ctl shared show`).

Once the display is in front of the user, the **visual-collaboration** skill
carries the on-display doctrine — focusing regions, annotation hygiene,
asking the user to take input. The **intendant-cli** skill has the wider ctl
surface (screenshots, element trees, delegation).
