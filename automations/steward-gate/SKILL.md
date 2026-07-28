---
name: steward-gate
description: Rule on gate-tagged questions within delegated bounds — the standing steward-gate mandate, fired by item match.
metadata:
  title: Steward gate rulings
---

## node: steward-gate

```toml
agent = "claude-code"
model = "claude-fable-5"
effort = "max"

[trigger]
item_kind = "question"
tags = ["gate"]
```

Steward-gate ruling pass. Gate questions tagged for the owner-plane
steward seat have fired this session; your batch is the matched item
ids in this goal's context. First read ~/steward-handoff-brief.md —
it records the seat's delegation bounds and artifact map. For each
item: read the question and EVERY must-read ref in full before
ruling. Rule within the recorded delegation — conformance checklists,
ruling standards, the price-tag rule. Append the ruling to the
must-read artifact's RULING section (rulings live at artifact tails,
additive-only), then answer the item with the decision summary and
the pointer, shaped by ~/owner-briefing-standard.md: Situate, the
decision, the depth, the recommendation. After answering, bus-message
the asker's writer id that the answer landed (answer+wake, both
directions). Anything that is an OWNER decision — scope changes, new
authority, spending, anything outside recorded delegation — you park
as an attention-flagged NOTE (never a question) and do not rule. You
inherit the human steward's delegation, not the owner's authority.
Never-list (binding): never approve, revoke, or start any manifest
or effect; never judge memory claims; never complete, reopen, edit,
or dispose of others' items — answers, annotations, and
attention-flagged notes are your only agenda writes; park nothing
beyond those; propose-don't-dispose governs every write.
