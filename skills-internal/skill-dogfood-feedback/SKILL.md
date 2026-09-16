---
name: skill-dogfood-feedback
description: "Developer opt-in only: report exceptional Intendant skill, MCP facade, ChatGPT plugin, or daemon friction and concrete efficiency opportunities through the narrow report tool. Never report routine success. If report is unavailable or denied, continue silently; never enable dogfooding, widen permissions, or write Agenda feedback as a workaround."
compatibility: Dev machines only — installed explicitly by scripts/install-dev-skills.sh or a developer plugin build with --dev-dogfood. Never embedded in the binary or included in the default plugin. Requires daemon startup opt-in INTENDANT_DEV_DOGFOOD=1 and gate-authorized feedback.write.
---

# Developer dogfood feedback

Use this skill only in an explicitly opted-in Intendant development environment,
not during ordinary end-user operation. Installing this skill does not enable
the daemon feature or grant permission. Those are owner decisions.

When an Intendant surface misleads you, lacks a needed verb or recipe, forces a
workaround, or causes concrete avoidable calls, latency, or context cost, submit
one concise exception through the MCP `report` tool and continue the task.
Routine success, generic feature wishes, ordinary model uncertainty, and
upstream-provider problems are not dogfood feedback.

- `kind`: `issue` for wrong/missing/confusing behavior; `efficiency` for a concrete
  lower-cost interface or recipe.
- `surface`: `skill:<skill-name>` for skill guidance; otherwise `facade`,
  `chatgpt-plugin`, `relay`, `agenda`, `display`, or another short Intendant surface.
- `summary`: one distinct friction or opportunity.
- `details`: observed behavior and what was actually needed, without raw logs.
- `recommendation`: the smaller or correct interface/recipe when known.
- `impact` and `avoidable_calls`: optional concrete effect and estimated wasted
  round trips; estimates are not required.
- `evidence_ref`: only a short scrubbed opaque reference when useful.
- `client_context`: optional self-described host label, never trusted identity.

The daemon owns deduplication and stamps authenticated actor/session and build
provenance. Reports stay in the developer daemon's local Agenda; they are not
sent centrally or published to GitHub.

**If the tool is absent, disabled, or permission-denied, stop reporting and
continue the user's task.** Do not repeatedly probe, change the daemon's
configuration, grant yourself authority, or fall back to `agenda_op`, `act`,
`ctl agenda`, or a feedback hub. A stale installed skill is not consent.

Never paste prompts, transcripts, environment dumps, raw tool arguments/output,
machine names or addresses, credentials, tokens, or secrets. Omit sensitive
evidence rather than asking to include it. Feedback is evidence, not a fix or
publication: do not edit product behavior, close feedback, or open an external
issue merely because you filed a report.
