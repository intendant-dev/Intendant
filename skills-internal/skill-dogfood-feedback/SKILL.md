---
name: skill-dogfood-feedback
description: Dev-fleet internal skill — after using any Intendant skill (agenda, cli, coordination, log-search, memory, remote-compute, …), park concise feedback on the daemon agenda ONLY when something exceptional happened — the skill misled you, lacked a verb or recipe you needed, forced a workaround, or notably carried the task. Never report routine successful use. Feedback lands under the skill-feedback hub (resolved by tag, created if missing); main sessions only — subagents never park agenda items.
compatibility: Dev machines only — symlinked by scripts/install-dev-skills.sh from a repo checkout, never embedded in the binary or installed for users. Requires a reachable Intendant daemon.
---

> Resolve the CLI first:
>
> ```bash
> INTENDANT="${INTENDANT:-$(command -v intendant || cat "${INTENDANT_HOME:-$HOME/.intendant}/cli-path" 2>/dev/null || echo intendant)}"
> ```
>
> If nothing resolves, Intendant is not on this machine — this skill does
> not apply. This is an internal dev-fleet skill: if you are not working on
> the Intendant project itself, it also does not apply.

# Dogfood feedback on Intendant skills

The shipped skills are product surfaces; the fleet using them all day is
the cheapest QA it will ever have. This skill turns exceptional moments
into durable, deduplicated feedback the owner can act on — without
drowning the agenda in routine.

## When to park (exceptions only — protect the signal)

- The skill's instructions **misled you** or contradicted current code.
- You needed a **verb, flag, or recipe the skill does not teach**.
- You **worked around** the skill instead of through it.
- The skill **notably carried a task** — say what carried it, so it is
  kept through future rewrites.

Routine successful use is NOT feedback. If nothing exceptional happened,
park nothing.

## How

1. Find the hub by TAG — never hard-code an id; ids are daemon-local:

   ```bash
   "$INTENDANT" ctl agenda list --all | grep "#skill-feedback" | head -3
   ```

   If no hub exists yet, create it once:

   ```bash
   "$INTENDANT" ctl agenda add "Skill feedback (dogfood)" --note \
     --tag skill-feedback --tag hub \
     --body "Exception-based feedback on the shipped Intendant skills, parked by dev-fleet sessions. One item per distinct friction; dedupe by annotating."
   ```

2. Dedupe before adding — list the hub's subtree and scan titles:

   ```bash
   "$INTENDANT" ctl agenda list --under <hub-id>
   ```

   Same friction already parked → `ctl agenda annotate <id> "…"` with your
   occurrence and any new evidence. Only a genuinely new friction gets a
   new item.

3. New item: one distinct friction per item, tagged with the skill's name,
   filed under the hub:

   ```bash
   "$INTENDANT" ctl agenda add "<skill>: <one-line friction>" --note \
     --tag skill-feedback --tag <skill-name> \
     --body "What happened; what the skill said; what was actually needed. Evidence: session id / file ref."
   "$INTENDANT" ctl agenda place <new-id> --under <hub-id>
   ```

## Rules

- **Main sessions only.** Subagents surface findings to their parent
  instead of writing the agenda.
- **Write clean bodies.** Feedback text gets quoted into fixes and PRs in
  a public repo: no machine names, addresses, environment ids,
  credentials, or pasted secrets — reference sessions and files by id and
  path.
- **Feedback is not a fix.** Never edit a shipped skill from a feedback
  impulse; park first, and let fixes ride normal reviewed slices.
- Completing or retiring feedback items is the owner's call, not yours.
