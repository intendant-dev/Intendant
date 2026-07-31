---
name: session-digest
description: Daily narrative digest — every day, digest yesterday's closed sessions into the derived narrative estate (journal-led, idempotent) via a bounded pool of codex workers. The steady-state lane of the narrative-synthesis cycle; the approve-click on the stamped manifest is the standing spend authorization.
license: MIT
compatibility: Requires the Codex CLI authenticated via subscription OAuth on this machine — digests run as `codex exec` workers and never bill an API key. Run the narrative-backfill action once first; it stages the estate's pinned export tool.
metadata:
  title: Daily session digest
---

The steady-state stage of the narrative-synthesis cycle: one firing a
day digests the previous day's closed sessions into the derived
estate the weekly narrative-pyramid workflow folds. Stamp with an
evening anchor — `ctl agenda stamp session-digest --project <dir>
--at 'YYYY-MM-DD HH:MM'` (or the dashboard's Automate sheet): the
daily cadence phase-locks to the first fire, so the anchor decides
the nightly hour; pick an evening time so the day's sessions have
gone idle. The cadence and the three-failure suspension breaker ride
this definition's `[cadence]` block; the executor (codex /
gpt-5.6-sol / xhigh) rides its node pins. Approving the proposed
manifest once is the standing spend authorization — steady-state cost
scales with session volume, and on subscription OAuth the figures are
ESTIMATES, never billed dollars.

## node: digest

```toml
agent = "codex"
model = "gpt-5.6-sol"
effort = "xhigh"

[cadence]
every = "1d"
suspend_after = 3
```

Daily narrative digest firing: digest yesterday's closed sessions
into the derived narrative estate (journal-led, idempotent).

ESTATE="${INTENDANT_HOME:-$HOME/.intendant}/derived/narrative/v1"
(derive from the daemon state root).

GUARD: if a narrative-backfill run is live on this machine (ctl
agenda list --json: an open item stamped from the narrative-backfill
automation whose latest occurrence is still running, or one carrying
an armed, unrevoked schedule), annotate THIS item ('skipped: backfill
owns digestion') and exit 0 — the two lanes never double-digest.

DISPATCH + AUTH LAW: Codex subscription OAuth ONLY — NEVER an API key
or direct API billing. Digest via a SYNCHRONOUS ROLLING `codex exec`
pool targeting up to 22 attached workers: each an ephemeral `codex
exec` child process of your session handling exactly ONE session,
writing only /tmp, then exiting; no batch barrier — reap/validate/
commit each completion immediately and refill the slot; you the sole
estate/journal writer. NEVER codex's built-in subagent/collaboration
orchestration for digest work (self-caps at 3 — live incident
2026-07-26). Wait on every worker PID — no detaching, daemons, or
survivors. Never kill a healthy worker; retry self-failures once in
the same slot; a second genuine failure journals {skipped:'stalled'}
and moves on. QUOTA LAW: rate-limit/quota failures never consume a
retry and never journal 'stalled' (stalled = permanent skip) — pause
refilling pool-wide, back off via shell sleeps (they cost no quota),
resume when a cheap probe succeeds. If your own calls fail on quota:
end the firing cleanly, journal nothing for unattempted sessions —
the cadence resurrects you; a hard cap surfaces to the owner via the
suspension breaker by design. Long sessions chunk under the same
rolling rules: chunks are slot work units, the merge runs as a worker
task in a slot (never in you), chunk digests are /tmp intermediates,
and the estate write + journal line happen once after the merge; a
chunk failing its retry merges as a stated hole, never a stalled
session.

THEN, bounded to the daily increment:
1. TOOL="$ESTATE/bin/intendant" (missing => exit nonzero, note 'NS
   tool not installed — run the narrative-backfill action first; it
   stages the tool').
2. "$TOOL" transcripts export --list; candidates = journal-absent or
   watermark-advanced sessions idle >6h.
3. Per session, oldest-first: export with redaction ON -> digest
   sessions/<source>/<id>.md (1-3K tokens, [n] markers; >150K prose
   chunks and merges citing original anchors) -> .json sidecar
   (key_claims with <=240-char VERBATIM quotes + {locator,ts_ms,role}
   anchors; dead intents kind:'intent' as candidates only; generator
   {model:'gpt-5.6-sol',prompt_version:'ns1-v2',
   exporter_version:'pr609'}; cost) -> atomic file writes THEN
   journal append. Nothing exported => journal
   skipped:'wrapper'|'empty'; <300 prose tokens => skipped:'trivial'
   (no model call).
4. End of firing: annotate THIS item one line: 'NS daily: <n>
   digested, <m> skipped, ~$<cost>'.
NEVER: agenda items or memory proposals from this mandate (the weekly
synthesis curates products); nothing outside the estate + /tmp;
redaction stays ON.

TERRITORY ADDENDUM: each sidecar additionally carries `territory` —
the file/dir paths the SESSION ITSELF demonstrably touched,
verbatim-observable from the transcript (tool calls, diffs, explicit
reads/writes), each entry `{path, kind:'file'|'dir', anchor}`; empty
array when none; cap 24; never inferred, never normalized beyond
trimming. Additive only; no new write surfaces; no extra model calls
— extraction rides the digest pass.
