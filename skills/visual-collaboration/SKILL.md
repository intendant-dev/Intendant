---
name: visual-collaboration
description: "Use when the user should see an agent-owned display through Intendant's shared view: demoing a finished result, letting the user watch live GUI/browser work, focusing attention on a region, capturing a display frame, or asking the user to take input authority."
---

# Visual Collaboration

Use this skill when work benefits from the user having live visibility into an **agent-owned display** — a sandbox, VM, dedicated agent machine, or virtual display: UI debugging, demos, app setup, visual inspection, or explaining what is happening on a display. The shared view streams the agent's workspace to the dashboard; it is not a mechanism for unattended control or observation of the user's personal machine.

## Core Tools

- `show_shared_view`: opens the shared display surface and marks the relevant display as the shared view, requesting display-stream activation so the dashboard shows it live.
- `focus_shared_view`: highlights a normalized region `{x, y, width, height}` on the shared display. Coordinates are fractions from 0.0 to 1.0.
- `clear_shared_view_focus`: removes the focus highlight and its note while keeping the shared view open. Idempotent — safe when nothing is highlighted.
- `capture_shared_view_frame`: captures the current display as an MCP image and foregrounds the same dashboard view.
- `request_shared_view_input`: asks the user to take input authority. Input authority is always granted by the user clicking the dashboard control — the tool only asks; it never grants.
- `hide_shared_view`: dismisses the banner and focus overlay when collaboration is done.

## When to Show

Proactively open the shared view when the human should visually stay in the loop:

1. **Demo a result** — after finishing GUI-visible work, show the display with a short `reason` so the user can see what was done.
2. **Watch live work** — before longer computer-use sessions (browsing, form filling, writing an email), open the view so the user can follow along.
3. **Auth and judgment handoffs** — when the user must type a password, approve a login, or choose from an account picker, show the display and `request_shared_view_input`; wait for the user to take control from the dashboard.

Use `focus_shared_view` whenever you reference a specific UI area, keep notes short and concrete, and `hide_shared_view` when the shared visual moment is over.

A focus annotation is content-bound guidance: the moment the thing it points at is gone (tab closed, page navigated, dialog dismissed), replace it with a new focus or remove it with `clear_shared_view_focus` — stale guidance on a live view is worse than none. Annotations also auto-clear when the shared view hides, when the display's user grant is revoked, and when the session that drew them ends; the explicit clear is for every earlier moment the content changes under the highlight.

## Display Targets

Prefer `display_id` when known. Use `display_target` otherwise:

- `display_99`, `99`, or legacy `:99` for an agent-owned virtual display — the primary case.
- `user_session` **only when the user has explicitly opted into sharing their own screen** (they asked you to work on or look at their desktop). Never default to it, and never treat auto-detection landing there as consent.
- Omit both only when auto-detection of the agent's display is acceptable.

The shared view is a dashboard coordination layer. For actual computer-use actions, continue using `read_screen`, `take_screenshot`, and `execute_cu_actions`; for archived stream frames, use `list_frames` and `read_frame`.
