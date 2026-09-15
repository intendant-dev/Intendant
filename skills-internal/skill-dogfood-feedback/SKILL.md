---
name: skill-dogfood-feedback
description: Dev-fleet overlay for Intendant skill QA — use the productized `report` MCP tool for exceptional shipped-skill friction or concrete efficiency opportunities. Never report routine success; the daemon deduplicates open reports and stamps trusted provenance. This internal skill adds only the dev-fleet trigger and examples; it no longer writes agenda hubs directly.
compatibility: Dev machines only — symlinked by scripts/install-dev-skills.sh from a repo checkout, never embedded in the binary or installed for users. Requires a reachable Intendant daemon exposing the `report` tool.
---

# Dogfood feedback on shipped Intendant skills

The shipped skills are product surfaces. When a skill misleads you,
lacks a verb/recipe you needed, forces a workaround, or creates a
concrete avoidable-call/context cost, file one exception through
Intendant's `report` MCP tool:

- `kind`: `issue` for wrong/missing/confusing guidance, otherwise
  `efficiency` for a concrete lower-cost interface/recipe.
- `surface`: `skill:<skill-name>`.
- `summary`: one distinct friction/opportunity.
- `details`: what the skill taught and what was actually needed.
- `recommendation`: the smaller/correct recipe when known.
- `evidence_ref`: only a short scrubbed opaque id when useful.

Routine successful use is **not feedback**. Never paste prompts,
transcripts, environment dumps, tool arguments/output, machine names,
credentials, tokens, or secrets. The report tool owns dedupe and trusted
actor/build provenance; do not create or search a feedback hub yourself.
Feedback is evidence, not a fix or a publication: do not edit the shipped
skill or create an external issue merely because you filed a report.
