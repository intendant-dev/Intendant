---
name: intendant-dogfood
description: "Report exceptional Intendant-specific friction or concrete efficiency opportunities through the narrow `report` MCP tool. Use only after something meaningful happened while operating Intendant: a capability was wrong/misleading/missing, forced a workaround, or caused clearly avoidable calls, latency, or context. Never report routine successful use; never paste transcripts, prompts, environment dumps, tool arguments/output, credentials, or secrets."
---

# Intendant dogfood feedback

Intendant's dogfood inbox is for **exceptions, not telemetry**. Most successful
use should produce no report at all. When an Intendant product surface creates a
specific, actionable problem or an unusually clear efficiency opportunity,
submit one concise report through the MCP `report` tool and continue the user's
task.

## Report when

- Intendant's instructions or observed behavior were **wrong, misleading, or
  contradictory**.
- A needed Intendant verb/capability/recipe was **missing**, so you had to work
  around the product rather than through it.
- Intendant caused clearly avoidable **tool calls / round trips, latency, or
  context consumption** and you can state the simpler shape.
- A product boundary made the task materially confusing or blocked progress.

Routine success, ordinary model uncertainty, upstream-provider problems, and
generic feature wishes are not dogfood reports.

## Shape

Use `kind: issue` for incorrect, missing, or confusing behavior. Use
`kind: efficiency` when the product works but a concrete interface improvement
would reduce calls, latency, or context.

Keep `surface` short and product-facing (`facade`, `chatgpt-plugin`, `relay`,
`agenda`, `display`, `remote-compute`, ...). Make `summary` one distinct
friction/opportunity; the daemon combines kind + surface + normalized summary
into its dedupe key. Put reproduction detail in `details` and the concrete
better shape in `recommendation`. If useful, `impact` is one of `blocked`,
`workaround`, `extra_calls`, `extra_latency`, `extra_context`, `confusing`, or
`minor`; `avoidable_calls` is an estimate, not a requirement.

`evidence_ref` is for a **short scrubbed opaque reference** such as a session,
agenda item, or tool-call id. `client_context` may name a host such as ChatGPT
or Codex, but it is self-described metadata, not trusted identity. The daemon
stamps authenticated principal/session provenance and its own build revision
out of band.

## Protect the signal and the user

- **Never report routine successful use.** Silence is the normal outcome.
- One distinct friction per report. The daemon atomically merges repeat
  occurrences of the same open report instead of creating duplicates.
- Never include raw transcripts, prompts, environment dumps, full tool
  arguments/output, machine names/addresses, credentials, tokens, or secrets.
- Do not interrupt the user's task merely to ask permission to report a
  scrubbed Intendant-product observation. If useful evidence itself contains
  user-sensitive material, omit it instead.
- A report is **evidence, not a fix or a publication**. Do not edit product
  behavior, close feedback, or create an external/GitHub issue merely because
  you filed a report; those are owner/review decisions.
