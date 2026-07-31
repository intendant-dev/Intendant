---
name: narrative-backfill
description: One-shot narrative bootstrap — digest this machine's session history into the derived narrative estate, journal-led and idempotent, until the eligible diff is empty. Doubles as the catch-up tool after daily-digest lapses; the cumulative cost cap is an owner-set annotation, fail-closed.
license: MIT
compatibility: Requires the Codex CLI authenticated via subscription OAuth on this machine — digests run as `codex exec` workers and never bill an API key.
metadata:
  title: Narrative backfill
---

The bootstrap stage of the narrative-synthesis cycle: run once at
adoption to digest the machine's whole session history, then re-run
after any daily session-digest lapse — the append-only journal keys
every skip, so re-runs digest only what is missing. Before stamping,
decide the spend bound: the run refuses to digest until the owner has
annotated the stamped item with a cost cap (see the mandate below),
so the approve-click plus the cap annotation together are the spend
authorization. Costs on subscription OAuth are ESTIMATES, never
billed dollars.

## node: backfill

```toml
agent = "codex"
model = "gpt-5.6-sol"
effort = "xhigh"
```

Narrative backfill run. MISSION: digest this machine's session
history into the derived narrative estate, journal-led, until the
eligible diff is empty. Idempotent by construction — the journal keys
every skip — so this action is both the one-shot bootstrap at
adoption and the catch-up tool after any daily-digest lapse: run once,
re-run after gaps. A run that ends mid-arc is resumed by the NEXT run
from the journal — that is the design; never build your own survivor.

ESTATE="${INTENDANT_HOME:-$HOME/.intendant}/derived/narrative/v1"
(derive from the daemon state root; create the directories on first
run). Layout: journal.jsonl (append-only; the last entry per
session_key wins), sessions/<source>/<session_id>.{md,json}, bin/.
The estate is derived and rebuildable; losing it costs money, never
correctness.

SPEND AUTHORITY (owner-set cap, fail-closed): the cumulative cost cap
is an OWNER parameter, not part of this definition. Before any model
call, read this item's annotations for the newest owner-written line
'NS CAP: $<amount>' — the owner states it when approving this
instance and may re-annotate to raise or lower it mid-arc. If no cap
annotation exists: annotate this item 'waiting on owner cap —
annotate NS CAP: $<amount> and re-run', then end the run without
digesting; the manifest approval alone never authorizes unbounded
spend. All dollar figures are ESTIMATES (API-equivalent arithmetic
serving the cap; journal cost fields stay labeled observed:false when
provider telemetry is absent), NOT billed dollars. Auth mode (HOUSE
RULE): Codex subscription OAuth ONLY — NEVER an API key or direct API
billing; nothing in this mandate authorizes provisioning a key.
Reasoning tokens bill as output and dominate at xhigh; honest
wall-clock is 2-6 min/session. CAP ENFORCEMENT: at run start AND
every 25 journal commits, sum the journal's cost fields PLUS actual
pending costs and conservative reservations for every live worker; if
the sum crosses the cap, STOP REFILLING, wait out attached workers,
then annotate this item 'NS CAP REACHED: $<sum>' and end the run.

EACH RUN:
1. TOOL="$ESTATE/bin/intendant". If missing, stage it: copy the
   running daemon's own intendant binary there (the path in
   $INTENDANT, else the state root's cli-path file, else `command -v
   intendant`) — the staged copy pins the exporter so watermarks and
   anchors stay byte-stable across daemon upgrades.
2. "$TOOL" transcripts export --list > /tmp/ns-list.jsonl. Candidates
   = sessions absent from journal.jsonl OR whose
   (newest_mtime_ms,total_bytes) advanced past the journaled
   watermark, AND idle >6h (newest_mtime_ms older than now-6h). Never
   digest a live session.
3. DISPATCH LAW (rolling-pool form; supersedes any other dispatch
   reading): run a SYNCHRONOUS ROLLING `codex exec` pool targeting 22
   attached workers — each an ephemeral `codex exec` child process OF
   YOUR SESSION handling exactly ONE session's exported prose,
   writing only /tmp, then exiting; YOU remain the sole estate/journal
   writer. NO BATCH BARRIER: reap, validate, and commit each
   completion immediately, then refill that slot with the next-oldest
   eligible candidate. Retries occupy the same slot and occur ONCE
   after a genuine self-failure. QUOTA LAW: a failure that looks like
   rate-limiting or quota exhaustion (429s, quota/limit messages)
   never consumes a session's retry and never journals 'stalled' —
   stalled entries are PERMANENT skips (journal-present, watermark
   frozen), so quota is a POOL condition: pause refilling and back
   off via shell sleeps (sleeping costs no quota), resuming when a
   cheap probe call succeeds. If YOUR OWN model calls are failing on
   quota, do not improvise workarounds — end the run cleanly,
   journaling NOTHING for unattempted sessions, and annotate this
   item that quota ended the run: a hard quota cap is an
   owner-visible event, not something to engineer around; the owner
   re-runs this action once the window resets and the journal resumes
   the arc exactly where it stopped. NEVER use codex's built-in
   subagent/collaboration orchestration for digest work — it
   self-caps at 3 concurrent subagents and crawled the live backfill
   (incident 2026-07-26). Wait on every worker PID — no detaching, no
   daemons, no survivors. Do not interrupt a prior run's live
   children; let them finish, then apply this law. Near the cap: stop
   refilling, always wait out attached workers. LONG-SESSION CHUNKING
   rides the same pool: chunks are ordinary work units — keep 22
   total slots busy (slots not needed by chunks keep pulling
   next-oldest sessions), reap/refill continuously, no long-session
   drain and no chunk batch barrier. When a session's last chunk
   lands, its MERGE runs as a worker task in a slot too — never in
   you (the parent stays thin; the merge worker reads chunk drafts
   from /tmp). Chunk digests are /tmp INTERMEDIATES: the session's
   estate write + journal line happen ONCE, after the merge — a crash
   mid-chunking means that session simply re-digests next run
   (journal-absent, by design). A chunk that fails its single retry
   does NOT stall the session: merge the chunks that succeeded with
   the gap NAMED in the digest and sidecar, journal normally with a
   note — a stated hole beats losing a giant session to one bad
   chunk. Process candidates OLDEST-first until the diff empties or
   the cap trips:
   a. "$TOOL" transcripts export --session <session_key> --out
      /tmp/ns-work (redaction stays ON; NEVER pass --redact off).
   b. Read that session's prose; write
      sessions/<source>/<session_id>.md in the estate: 1-3K tokens
      (4K hard cap) — what was worked on, decisions made, outcomes,
      unresolved intents; every key claim carries a [n] citation
      marker. Sessions >150K prose tokens: chunk, digest chunks,
      merge — the merged digest still cites ORIGINAL anchors.
   c. Write the .json sidecar: key_claims[] of {claim, quote (<=240
      chars VERBATIM from the exported text),
      anchor:{locator,ts_ms,role}}; mark dead/unresolved intentions
      kind:'intent'
      (candidates only — do NOT create agenda items from this
      mandate); territory[] — the file/dir paths the SESSION ITSELF
      demonstrably touched, verbatim-observable from the transcript
      (tool calls, diffs, explicit reads/writes), each entry {path,
      kind:'file'|'dir', anchor}, empty array when none, cap 24,
      never inferred, never normalized beyond trimming; generator
      {model:'gpt-5.6-sol', prompt_version:'ns1-v2',
      exporter_version:'pr609'}; cost {input_tokens,output_tokens}
      (your usage if observable, else a stated estimate).
   d. Write digest files atomically (tmp+rename), THEN append the
      journal line. Export emitted nothing: journal
      {skipped:'wrapper'|'empty'}. Prose under ~300 tokens:
      {skipped:'trivial'}, no model call.
   e. Drop that session's prose from working context before the next
      target; never re-read digested prose.
   f. PATIENCE (binding): NEVER kill a live digest child — while its
      process is alive and error-free, wait; xhigh latency variance
      is expected and can exceed 15 minutes even on tiny inputs. If a
      call fails ON ITS OWN, retry it once; on a second genuine
      failure for the same target, journal {skipped:'stalled'} with a
      note and move to the NEXT target — never loop on one session.
4. Diff empty: FIRST re-attempt every journal entry
   {skipped:'stalled'} exactly once each (stalled entries never
   re-candidate on their own — their watermarks are frozen; this
   sweep is their only second chance), journaling each result
   normally; THEN annotate this item 'NS BACKFILL COMPLETE: <n>
   digested, <m> skipped, ~$<sum> spent — the daily session-digest
   mandate carries on from here.' Later runs finding an empty diff:
   exit quickly as a no-op.

NEVER: create agenda items or memory proposals from this mandate (the
weekly synthesis curates products), touch anything outside the
derived estate and /tmp, or disable redaction.

NEVER (added 2026-07-26 after a live incident): detach standalone
workers. THE RUN ITSELF IS THE ORCHESTRATOR — digest children run as
direct children of your session and die with you; background scripts,
daemons, or any process that would outlive your session are
forbidden. A run that ends mid-arc is resumed by the NEXT run from
the journal — that is the design; never build your own survivor.
