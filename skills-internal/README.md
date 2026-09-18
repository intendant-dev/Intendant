# skills-internal/

Dev-fleet-only agent skills: in the public repo, outside the product.

The five skill tiers:

| Tier | Embedded in binary | Installed |
|---|---|---|
| `skills/` | yes (`builtin_skills.rs`, byte-pinned) | both global roots, minus the owner's persisted disabled-set (dashboard deactivate/re-enable) |
| `plugins/<name>/skills/` | yes (`plugin_registry.rs`) | materialized only while the plugin is enabled and ready |
| `<state root>/skills/` (user library) | no — dashboard-added bytes, recorded with attribution + sha256 (`user_skills.rs`) | both global roots while enabled, marked `source: user (dashboard-added)`; removable from the dashboard |
| `skills-internal/` (this tier) | **never** | dev-machine opt-in only |
| `tests/skills/` | no | never (operator E2E briefs) |

Activation is per dev machine and explicit:

```bash
bash scripts/install-dev-skills.sh
```

The script symlinks each skill here from the MAIN checkout into
`~/.agents/skills` and `~/.claude/skills`. The daemon's installer treats
symlinks as user-owned — never followed, replaced, or swept — so these
links are invisible to product behavior, and landed changes go live
through the link with no reinstall.

## Dogfood reporting requires a second explicit opt-in

Installing the developer skills does not enable reporting on a daemon. Start
only the intended developer daemon with `INTENDANT_DEV_DOGFOOD=1` (exactly `1`,
captured at startup; restart after changes). Its MCP `report` tool remains
hidden and rejects calls otherwise, even for root and cached clients.

The caller must also be authorized for `feedback.write`; ordinary
`role:operator` does not grant it. An existing owner/root surface or permitted
enrolled agent profile works on an opted-in daemon; scoped installations need
an owner-configured role explicitly carrying the permission. Do not widen an
agent to root merely for feedback. The flag does not grant any authority.

For ChatGPT, the explicit developer package generator option `--dev-dogfood`
copies this tier's dogfood skill into that generated package only. It does not
change the default plugin or daemon configuration. Without it, the plugin has
no dogfood instructions. Never distribute a developer package as the default.

To opt out, remove the variable (or set it to `0`) and restart the daemon.
Regenerate/reinstall developer plugin packages without `--dev-dogfood` and remove
the internal skill symlinks to stop teaching the behavior. Reports already in
Agenda are preserved. Agents must not fall back to generic Agenda writes when
`report` is absent or denied.

Rules for skills in this tier:

- **Never** reference one from `builtin_skills.rs`, `plugin_registry.rs`,
  or any default product code path. The explicit developer plugin generator
  option may copy a skill from this tier into a developer-only package; the
  daemon and default plugin must never embed or install it. A unit test
  (`plugin_registry.rs`: `internal_skills_stay_disjoint_from_shipped_names_and_parse`)
  enforces name disjointness from every shipped skill — a collision would
  silently block the shipped copy's install on dev machines.
- **Public-repo scrub law applies to skill text**: no machine names,
  addresses, environment ids, credentials, or daemon-local ids (agenda or
  hub ULIDs). Reference agenda hubs by TAG and resolve at runtime.
- Frontmatter `name` must equal the directory name; the `description`
  carries the trigger (it is the ambient catalog line).
- Keep each skill self-contained and small; this tier documents fleet
  practice, it does not ship features.
