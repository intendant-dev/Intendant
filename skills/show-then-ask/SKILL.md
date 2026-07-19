---
name: show-then-ask
description: Use when asking the user to choose between design or implementation variants, judge a before/after, or approve a visual change — show rendered previews on the dashboard question rail instead of describing them. Attach prototype HTML pages (interactive, sandboxed), images, or text snippets to a blocking `intendant ctl ask`; the user's choice or free-text reply returns as the command's stdout.
compatibility: Requires a reachable Intendant daemon (supervised sessions have $INTENDANT and INTENDANT_MCP_URL injected).
distribution: global
---

> If `$INTENDANT`/`INTENDANT_MCP_URL` is unset and no local Intendant daemon answers, this skill does not apply — say so and stop.

# Show, then ask

Don't describe three UI directions in prose and ask the user to imagine
them — render them. `intendant ctl ask` attaches up to 4 preview cards
above the options of a dashboard question, blocks until the user
answers, and prints their answer to stdout. Works from any agent that
can run a shell command (supervised Claude Code and Codex sessions get
`$INTENDANT` and a session-scoped `INTENDANT_MCP_URL` injected, so the
question is automatically attached to your own session).

## Recipes

```bash
# Pick between prototypes — write self-contained HTML files, then:
"${INTENDANT:-intendant}" ctl ask "Which landing direction?" \
    --option "A:dense ops grid" --option "B:calm editorial" --wait 600 \
    --preview-html A=proto-a.html --preview-html B=proto-b.html

# Before/after — screenshots of a change, before it lands:
"${INTENDANT:-intendant}" ctl ask "Ship this restyle?" \
    --option "Ship it" --option "Needs work" \
    --preview-image Before=before.png --preview-image After=after.png

# Small text artifacts (diffs, copy, error messages) render preformatted:
"${INTENDANT:-intendant}" ctl ask "Keep the new hero copy?" \
    --preview-text "New=The house runs itself. It answers to you."
```

The command exits 0 with the answer ("A", "Ship it", or whatever the
user typed — free text is always accepted on top of options) and
nonzero on timeout with best-judgment guidance. Cards persist in the
session log, so replays show exactly what the user saw when they chose.

## Constraints

- **HTML must be one self-contained file**: inline all CSS/JS, use
  `data:` URLs for images. It renders in a locked-down sandboxed frame —
  scripts run (tabs, toggles, hover states work), but external fetches
  and daemon APIs do not resolve.
- Images: png/jpg/gif/webp/bmp, inferred from the file extension.
- Caps: 4 cards, 2 MB per html, 4 MB per image, 4 KB per text, 8 MB
  total. `--wait` default 300 s, max 900.
- ctl reads the files itself, client-side — pass paths, not content;
  the bytes never enter your model context in either direction.
- Live apps (dev servers, HMR, real backends) are not preview material —
  use a browser workspace plus the shared view to stream those instead.

MCP-direct callers can pass the same cards inline via the `ask_user`
tool's `previews` parameter, at context cost — prefer ctl for files.
`"${INTENDANT:-intendant}" ctl ask --help` is the authoritative flag
reference.
