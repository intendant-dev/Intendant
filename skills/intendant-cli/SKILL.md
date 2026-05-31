---
name: intendant-cli
description: Use when an agent needs Intendant control beyond the small MCP bootstrap set. Prefer intendantctl over broad MCP tools to keep model context small.
---

# Intendant CLI

Use `intendantctl` for Intendant control that is not already available as a small MCP bootstrap tool. The CLI talks to the running dashboard/MCP endpoint and exposes broad capabilities lazily through subcommand help.

Start with:

```bash
intendantctl --help
intendantctl status --json
intendantctl tools list
```

Useful groups:

- `intendantctl display --help` for displays, frames, screenshots, and display claims.
- `intendantctl cu --help` for computer-use actions.
- `intendantctl shared --help` for shared display collaboration.
- `intendantctl approval --help` and `intendantctl input --help` for pending approval/input flows.
- `intendantctl context --help` for managed-context rewind/backout.
- `intendantctl controller --help` for controller-loop and restart controls.
- `intendantctl tools schema TOOL` and `intendantctl tools call TOOL --args JSON` for rare or newly-added MCP tools.

Prefer `--json` when the output will be inspected by an agent. Use `--session ID` when operating on a specific session. Use `--managed-context managed` for rewind/backout commands.
