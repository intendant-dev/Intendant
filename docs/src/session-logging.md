# Session Logging

## Overview

Every `intendant` invocation gets a structured session log directory. It is the
single source of truth for what happened in a session: a line-per-event JSONL
stream, full per-turn artifacts, the agent's stdout/stderr, file-history
snapshots, and (in headless/web/MCP runs) the controller's own console output.
It serves four audiences: a human debugging after the fact, the dashboard
replaying a session into the browser, the resume path rehydrating a
conversation to continue work, and the message-search indexer deriving its
rolling index from the canonical message lane (see
[Message Search](#message-search-the-rolling-message-index)).

The implementation lives in the `session_log/` module: `mod.rs` (the
`SessionLog` core — open/meta/discovery, emit, CU events, summaries, turn
files, voice/presence logging), `bus_events.rs` (the event-bus-driven typed
writer methods), `replay.rs` (the JSONL → `AppEvent` inverse), and
`history.rs` (conversation read-back and recent-entry tails). Sessions
are fully isolated — there is no global state file; each session is one
self-contained directory.

## On-Disk Layout

By default each session is a UUID-named directory under `~/.intendant/logs/`
(verified in `SessionLog::resolve_path`). `--log-file <DIR>` overrides the
directory outright (used to pin a session to a known path). The controller hands
the chosen directory to the runtime subprocess via the `INTENDANT_LOG_DIR`
environment variable, so per-command stdout/stderr land in the same place.

```
~/.intendant/logs/<uuid>/
├── session_meta.json        # id, created_at, project_root, name, task, status, last_turn, role, rounds
├── session.jsonl            # structured event log — one JSON object per line (the spine)
├── transcript.jsonl         # simplified {ts, role, text, tools_called?} — rebuilt at session end
├── conversation.jsonl       # serialized native Conversation, for --continue / --resume
├── session_summary.json     # accumulated stats (duration, voice, CU tasks, tokens, errors)
├── daemon.log               # controller stdout/stderr tee (web/headless/MCP only; Unix only)
├── human_question           # askHuman IPC: question file (session-scoped)
├── human_response           # askHuman IPC: response file (session-scoped)
├── <nonce>_stdout.log       # runtime stdout for command nonce N  (e.g. 1_stdout.log)
├── <nonce>_stderr.log       # runtime stderr for command nonce N
├── frames/                  # display & camera frame captures
│   ├── frames.jsonl         #   frame manifest (id, stream, timestamp, sent_to_live)
│   └── *.jpg                #   HQ JPEG frames
├── file_snapshots/          # file-watcher rewind/redo history (see Control Plane & Daemon)
│   ├── baseline/            #   initial text-file snapshot
│   ├── objects/             #   content-addressed blobs (sha256-named)
│   ├── rounds/              #   per-round artifacts
│   └── history.json         #   rounds[], abandoned_branches[], current_head_id, next_id
└── turns/
    ├── turn_001_messages.json    # full messages array sent to the API
    ├── turn_001_model.txt        # full model response text
    ├── turn_001_reasoning.txt    # full reasoning content (when the provider returns it)
    ├── turn_001_agent_in.json    # commands sent to the runtime (pretty-printed)
    ├── turn_001_stdout.txt       # agent stdout for this turn
    ├── turn_001_stderr.txt       # agent stderr for this turn (only when non-empty)
    └── turn_001_context_<id>.json # context snapshot, when a context directive fires
```

Turn files are named `turn_{NNN}_{suffix}` with `NNN` zero-padded to three digits
(`write_turn_file` / `append_turn_file`). Per-nonce runtime logs are named
`{nonce}_stdout.log` / `{nonce}_stderr.log` (`agent.rs`).

### `session_meta.json`

```json
{
  "session_id": "a1b2c3d4-...",
  "created_at": "2026-05-24T10:30:00",
  "created_at_ms": 1782808200123,
  "project_root": "/home/user/myproject",
  "name": "Fix auth bug",
  "task": "Fix the authentication bug",
  "status": "running",
  "last_turn": 5,
  "role": null,
  "rounds": 2
}
```

`name` is an optional user-facing label (see [Session naming](#session-naming-and-aliasing-across-backends));
`role` is set for sub-agent sessions (`orchestrator`, `research`,
`implementation`, `testing`) and is how the resume scan skips them. This file
drives `--continue` (most-recent session for the project) and `--resume <id>`
(by full id or prefix).

## The `session.jsonl` Event Stream

`session.jsonl` is the spine: one `LogEvent` JSON object per line. Each event
carries a local time-of-day timestamp plus a machine-readable epoch-ms UTC
timestamp (`ts_ms`; events written before 2026-07 lack it — recover their date
from `session_meta.json`), an optional turn number, the event name, an optional
level, an optional human message, optional structured `data`, and optional
`file` / `file2` references pointing at the full-content turn files (so the
line stays small and the bulk lives in `turns/`).

```rust
struct LogEvent {
    ts: String, ts_ms: i64, turn: Option<usize>, event: String,
    level: Option<String>, message: Option<String>,
    data: Option<serde_json::Value>,
    file: Option<String>, file2: Option<String>,
}
```

The event vocabulary is broad and grows with the system. Grouped by area
(verified against the `session_log/` module):

| Area | Events |
|------|--------|
| Lifecycle | `session_start`, `session_started`, `agent_started`, `turn_start`, `round_complete`, `task_complete`, `done_signal`, `safety_cap_reached`, `session_end`, `session_ended` |
| Model I/O | `messages_input`, `model_response`, `reasoning`, `json_extracted` |
| Message lane | `conversation_message`, `conversation_rewound`, `conversation_message_epoch` |
| Runtime | `agent_input`, `agent_output` |
| Approvals | `approval`, `approval_resolved`, `auto_approved`, `human_question`, `human_response_sent` |
| Context | `context_snapshot`, `snapshot_created`, `conversation_rolled_back`, `rolled_back`, `redone`, `history_pruned` |
| Sessions/graph | `session_identity`, `session_relationship`, `session_attached`, `session_capabilities`, `sub_agent_result`, `presence_checkpoint` |
| Computer use | `cu_task_start`, `cu_turn`, `cu_task_complete`, `cu_task_error` |
| Display | `display_ready`, `display_taken`, `display_released`, `display_resize`, `debug_screen_ready`, `debug_screen_torn_down` |
| Voice/live | `live_audio_started`, `live_audio_progress`, `live_audio_completed`, `live_usage_update`, `presence_connected`, `presence_disconnected`, `presence_log`, `presence_usage_update` |
| Recording | `recording_started`, `recording_stopped`, `recording_error` |
| Generic | `info`, `debug`, `error`, `tool_request`, `tool_response` |

Each is written by a typed method on `SessionLog` (e.g. `turn_start`,
`model_response`, `agent_input`, `agent_output`, `approval`, `json_extracted`,
`reasoning_content`), not by hand-formatting JSON.

### The message lane (`conversation_message`)

`conversation_message` is the canonical append-only record of what was
*actually said* in the native worker conversation — the source the
message-search index consumes (see
[Message Search](#message-search-the-rolling-message-index)). One record per
genuine entry, written by two typed methods in `bus_events.rs`:

- **User side** (`conversation_message_user`): the task (provenance `task`),
  resume continuations (`resume_task`), follow-ups (`follow_up`), steers
  delivered into model context (`steer`), and accepted askHuman answers
  (`ask_human_answer`). `data.text` carries the RAW user text — attachment
  preludes and `[Session resumed]`/`[New Task]`/`[User]` wrappers are the
  conversation's concern, not the record's. askHuman answers arrive on two
  paths: the loop-level answer is an ordinary user message plus its record
  (closing the audit hole where `human_response_sent` carries no text), while
  the native-tool answer enters the conversation as a *tool result* and is
  **projected** into the lane — `data.ref_seq` references that result's seq so
  rewind cuts cover it.
- **Assistant side** (`model_response_with_message`): one call writes the
  `turns/*_model.txt` sidecar span, the diagnostic `model_response` event, and
  the canonical record referencing the same bytes (`file` +
  `data.model_offset`/`data.model_bytes`) — no crash window between diagnostic
  and canonical, no second copy of the text. It fires whenever
  `response.content` is non-empty, on both the plain and the tool-call branch
  (assistant prose regularly accompanies tool calls). External-agent wrapper
  sessions keep plain `model_response`: their messages are canonical in the
  external backend's own log, and indexing skips the mirror.

System injections, tool output, context summaries, the CU sub-conversation,
and presence never emit it. The emit/skip decision is driven by
`MessageProvenance` (`crates/intendant-core/src/conversation.rs`), a
serialized per-message provenance axis
(`task | resume_task | follow_up | steer | ask_human_answer |
system_injection | tool_output | context_summary | assistant | unknown` —
`unknown` only in legacy files). `data.message_seq` is `Message.seq`: a
monotonic per-conversation ordinal assigned at append time and **never
reused** — truncation does not rewind the counter — so a cut is unambiguous.

Two companion events complete the lane:

- `conversation_rewound { cut_after_seq, kind, superseded_at_ms }` marks
  messages with `seq > cut_after_seq` as superseded. The daemon's
  round-rollback rail emits it with kind `tail_rollback` (`run_modes.rs`);
  consumers key on `cut_after_seq` alone, so `kind` is an open vocabulary.
  Compaction (`drop_turns`/`summarize_turns`) deliberately does NOT emit it —
  a compacted message was still said and remains canonical history.
- `conversation_message_epoch` is the mixed-version cutover marker: on resume,
  `Conversation::ensure_seqs_assigned` renumbers a legacy file (any loaded
  message with `seq == 0`) `1..=N` and the marker records the ordered
  `mapping: [[seq, role, content-hash16], …]`. Extractors use legacy
  extraction strictly before the marker and `conversation_message` records
  after it, correlating legacy records through the hashes. The pass is a
  deliberate no-op for files whose seqs are all assigned — renumbering a pure
  new-era file would break prior event references.

### Querying

```bash
S=~/.intendant/logs/<uuid>

# Event overview
jq -r '.event' "$S/session.jsonl"

# What the model received on turn 5
jq . "$S/turns/turn_005_messages.json"

# Model reasoning on turn 3
cat "$S/turns/turn_003_reasoning.txt"

# Every batch of commands sent to the runtime
grep '"event":"agent_input"' "$S/session.jsonl" | jq -r '.message'

# What was actually said (the canonical message lane)
grep '"event":"conversation_message"' "$S/session.jsonl" \
  | jq -r '[.data.message_seq, .data.provenance, .data.text // "(sidecar span)"] | @tsv'

# Approvals and how they resolved
grep -E '"event":"(approval|approval_resolved|auto_approved)"' "$S/session.jsonl" | jq .

# All sessions, newest first
ls -lt ~/.intendant/logs/

# Sessions for one project
grep -l '"project_root":"/home/user/myproject"' ~/.intendant/logs/*/session_meta.json
```

## `transcript.jsonl` and `session_summary.json`

`transcript.jsonl` is a simplified, human-skimmable conversation log
(`{ts, role, text, tools_called?}` per line). It is appended live and then fully
**rebuilt from `session.jsonl` at session end** (`rebuild_transcript`) so it is
complete and consistent even if the live append missed anything (notably voice
tokens, which are buffered into whole utterances before being emitted).

`session_summary.json` is written at session end with accumulated statistics:
duration, voice provider/model and connection/reconnect counts, model-turn
count, computer-use task summaries, frames sent, errors, and total tokens.

## Resume and Rehydration

The native conversation is serialized to `conversation.jsonl` so a session can
be continued:

```bash
# Resume the most recent session for this project
./target/release/intendant --continue "fix that bug"

# Resume a specific session by id or prefix
./target/release/intendant --resume abc123 "continue"
```

On resume, `Conversation::load_from_file(conversation.jsonl, context_window)`
rehydrates the message history, the new task is appended as a
`[Session resumed] Continue with: …` continuation message (with any attachments
folded in), and the loop continues from the rehydrated turn. `session_meta.json`
is updated with the new task.

`conversation.jsonl` is specific to Intendant's **internal** agent. External
backends (Codex / Claude Code) own their own conversation history; the
session supervisor resumes those through each backend's native resume token (see
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md) →
`ResumeSession`), keyed by the session `source`.

## Multi-Session and the Session Graph

A persistent daemon (an idle `--web` launch) runs many sessions over its
lifetime, each its own `~/.intendant/logs/<uuid>/` directory. The
`session_supervisor` (see
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md)) creates these
directories on `CreateSession`/`StartTask`, tracks which is active, and records
parent/child relationships (`side`, `subagent`). Those relationships are also
logged into the streams as `session_identity` and `session_relationship` events,
which lets a consumer reconstruct the session tree purely from the logs.

## Session Naming and Aliasing Across Backends

Sessions can be renamed for display, and the same abstraction works whether the
session is an Intendant session or an external backend's. This lives in
`session_names.rs`.

- **Source normalization.** Free-form source strings collapse to a canonical set:
  `intendant`, `codex`, `claude-code` (so `"claude code"` and `"cc"` map
  correctly).
- **Intendant sessions** store the name directly in their own
  `session_meta.json` (`write_intendant_session_name`), located by id or prefix
  under `~/.intendant/logs/`.
- **External backends** get an **overlay**: a single
  `~/.intendant/session_names.json`, keyed `source → { session_id → name }`. When
  the dashboard lists sessions, `apply_session_name_overlays` merges these names
  onto the listed sessions (matching on `session_id` or `resume_id`), without
  touching the backend's own files.
- Names are normalized (whitespace-collapsed, truncated at 180 chars) on both
  write and read.

`ControlMsg::RenameSession` carries `session_id`, optional `backend_session_id`,
optional `source`, and the new `name`; the supervisor dispatches it through this
abstraction. A backend with native rename support can map it to its own protocol;
otherwise the overlay is used.

## The `daemon.log` Controller Tee

When the controller does **not** own a real interactive TTY — i.e. web, headless,
or MCP runs — `daemon_log_tee::install` redirects the controller's own stderr and
stdout into `~/.intendant/logs/<uuid>/daemon.log`, prefixing each line with a
wallclock timestamp, while still mirroring everything to the original terminal.
This captures controller-side `eprintln!`, panics, and tracing that would
otherwise never reach `session.jsonl` (which only records *agent* events). The
dashboard's "Download session report" zip includes `daemon.log` so a tester's
bundle is temporally analyzable by a developer.

This is **Unix-only**: on Windows `install` is a no-op. It is deliberately
**always installed** — no frontend owns the raw TTY, so routing stdout
through the tee is always safe.

## How the Dashboard Consumes the Logs

A browser that connects late does not miss history. The web gateway reads
`session.jsonl` and converts it to a stream of outbound events for the WASM
client (`replay_jsonl_to_outbound_entries` in `web_gateway/session_catalog/replay.rs`):

- The first replayed entry is a `replay_start` marker carrying the
  provider/model/autonomy values scanned from the log (`scan_replay_status`), so
  the dashboard seeds its status bar correctly before any live event arrives.
- Each subsequent line is converted to an `OutboundEvent`-shaped object with its
  original `ts` preserved, so replay reproduces the exact event sequence the
  Activity tab would have shown live.
- External-agent activity replay is a bounded UI bootstrap with
  full-audit-transcript semantics: it includes user/assistant transcript entries,
  command output, rollback metadata, session goals, and context snapshots where
  available, while still omitting internal events that are not useful to render.

Live events then continue to stream over the same WebSocket. See
[Web Dashboard](./web-dashboard.md) for the tab structure and
[Control Plane & Persistent Daemon](./control-plane-and-daemon.md) for the event
producers (session supervisor, file watcher) behind the stream.

## Message Search: The Rolling Message Index

The message-search subsystem (`message_search/` — every module carries a doc
header worth reading before changing it) maintains a rolling, message-only
index over the last **14 days** of conversations on this box: native sessions
via the message lane above, plus the supervised backends' own transcripts
(Codex rollouts, Claude Code project files). It backs the dashboard's
quick-search message lane and the ⌘K command palette, answering "where did we
say X"
from pre-extracted shards instead of raw-log scans. The exhaustive audit path
over full logs remains Deep Search (`GET /api/sessions/search`).

Three layers: per-source **extractors** derive `MessageRecord`s plus
supersession marks per session; the **indexer** sweep enumerates this box's
sources and publishes shards to the **store**, which owns durability,
multi-daemon coordination, retention, and stable snapshots; the **query** side
owns matching, normalization, and the resident fold arena.

### The shard store

`Store::default_root()` is `~/.intendant/cache/message_search/v1`; the
adjacent lease-staging dirs (below) share the parent:

```
~/.intendant/cache/message_search/
├── v1/
│   ├── manifest.json       # the single mutable file — swapped by atomic rename
│   ├── writer.lock         # advisory O_EXCL lockfile (5-minute stale takeover)
│   └── generations/
│       └── <hash16>.json   # immutable, content-named: one session's records + marks
├── staging/                # lease-cleanup transcript remnants awaiting drain
└── leased-active/          # registry of live leased homes (one JSON per home)
```

- **Generations are immutable and content-named** (hash of the shard body), so
  an open snapshot keeps reading the files it resolved even while newer
  manifests land — the query side's pagination builds on this.
- **The manifest** carries `schema`, `parser_version`, a monotonic `revision`
  counter (the query side's snapshot watermark — `updated_at_ms` alone cannot
  pin a snapshot, since two writes inside one clock millisecond read as
  unchanged), the session map keyed `<source>:<session_id>` (each entry:
  generation file, record count, `newest_ts_ms`, `source_watermark`, cursors,
  `source_gone`), and deletion tombstones.
- **The writer lock is efficiency, not correctness**: shards are
  content-deterministic from their sources, so a lost race self-heals on the
  next pass — the lock buys query-visible stability and avoids N-daemon
  duplicate work. The correctness gate is the publish path: it re-reads the
  latest manifest under the lock, rejects a publish whose watermark (summed
  consumed-source offsets) is *lower over the same source state* (a stale
  writer that lost a race), and never lets an older `parser_version` clobber a
  newer manifest. A lower watermark with changed source fingerprints or a
  changed cursor set is a legitimate rebuild (rewritten/shrunk source, or a
  publisher that stopped double-counting a file) and is accepted.
- **Tombstones** make deliberate deletion sticky: `Store::delete_session`
  drops the shard and tombstones the key so a stale source can never
  resurrect it. The session-deletion flow does not call it yet — today a
  deleted session's sources just vanish, the next sweep flips the shard to
  `source_gone`, and the shard keeps serving until the window expires it.
- **Retention**: sessions whose newest message left the 14-day `ts_ms` window
  are dropped, tombstones expire with the window, and unreferenced generation
  files are deleted — at boot (`message_search::startup_gc`) and on every
  120th sweep (hourly at the 30 s cadence). A corrupt manifest is quarantined
  (`manifest.corrupt-<ms>`) and rebuilt from empty; generations are
  re-derivable from their sources.

### Records, supersession, and locators

Every extractor produces the shared `MessageRecord` (`record.rs`): source,
session id, role (`user`/`assistant`), `ts_ms`, the ORIGINAL text (capped at
256 KiB on a char boundary; `truncated` flags the cut), a locator, and the
identity fields the marks key on — `seq` (native `Message.seq`), `user_turn` /
`item_id` (Codex), `generation`, `subagent`.

**Active vs. superseded is never stored.** It is derived at read time by
replaying the shard's `SupersessionMark`s over the records in source order
(`record::derive_active`), because a Codex same-thread restore can reactivate
previously superseded messages — supersession must stay recomputable, never an
irreversible stamp. Mark kinds:

| Mark | Source | Effect |
|---|---|---|
| `SeqCut { cut_after_seq }` | native `conversation_rewound` | records with `seq > cut_after_seq` supersede |
| `TurnCount { num_turns }` | Codex `thread_rolled_back` | the last N still-active user turns (and their assistant records) supersede — bounded by what exists, so corrupt counts cannot loop |
| `ItemAnchor { item_id, position }` | Codex item-anchored rewind | everything after the anchored item (or from it, `position: "before"`) supersedes |
| `GenerationRestore { active_generation }` | Codex same-thread restore | records of generations newer than the restored one stop reading active |

Locators say where a hit lives in its source — opaque to clients, versioned by
variant, and **frozen** (resolution verifies exactly the way extraction
minted): `native_message_id` (a `conversation_message` id),
`native_event` (legacy user-side: line number + content hash),
`native_sidecar_span` (legacy assistant: file/offset/len + hash),
`external_record_id` (Claude record `uuid`, Codex `response_item` id), and
`external_line` (generation + line + hash, for external records without a
native id).

### The extractors

All three are hermetic — every entry point takes its paths as parameters; only
the indexer's production edge resolves the real environment.

- **Intendant** (`extract_intendant.rs`) — walks one session log dir
  (`session.jsonl` + the `turns/` sidecars its events reference). Two eras
  coexist on disk. New era: canonical `conversation_message` rows, with
  `conversation_rewound` becoming `SeqCut` marks. Legacy era (pre-lane logs,
  or the pre-marker segment of a resumed session): user text is reconstructed
  best-effort from `session_started` tasks (falling back to
  `session_meta.json`), delivered steers (`steer_requested` joined with
  `steer_delivered` on id), and `"Round {N} follow-up:"` info lines; assistant
  text from `model_response` sidecar spans; legacy timestamps recover from
  meta `created_at` + time-of-day with midnight-wrap inference. The epoch
  marker splits the eras explicitly; without one, the presence of any
  canonical row means the session was born new-era, and its info lines are
  diagnostics that must not be extracted a second time. **Wrapper sessions
  (any `session_identity` event) are skipped entirely** — their
  `model_response` events mirror messages that are canonical in the external
  backend's own log.
- **Codex** (`extract_codex.rs`) — walks one rollout file (under `sessions/`
  or `archived_sessions/`). The user lane is dual-represented in the rollout:
  `event_msg`/`user_message` is canonical here (it twins a user-role
  `response_item` 99.3% of the time, full-text exact), and `response_item`
  user items are skipped entirely — which also drops the machine injections
  that ride that lane with no event twin (AGENTS.md dumps,
  `<environment_context>`, `<turn_aborted>`, … — 39% of naive user-lane
  bytes). Assistant prose is canonical on assistant-role `response_item`
  `message` items (`developer`-role items are harness config, skipped).
  `thread_rolled_back` events become `TurnCount` and/or `ItemAnchor` marks.
  **Generations**: a same-thread restore rewrites the rollout in place; the
  cursor detects the rewrite, the fresh parse gets a bumped generation, and
  the previously published records are retained and republished alongside it
  with a `GenerationRestore` mark naming the fresh branch active — messages
  from pre-restore generations stay findable. Prior `TurnCount`/`ItemAnchor`
  marks are deliberately not carried across the merge (replayed over the
  merged record vec they would alias onto the new generation's frame); dropping
  them fails safe toward active.
- **Claude Code** (`extract_claude.rs`) — one main `<uuid>.jsonl` plus its
  `<uuid>/subagents/agent-*.jsonl` transcripts, joined by session uuid
  corpus-wide (a subagent dir can live under a *different* project dir than
  its main after a worktree relocation). Subagent records are indexed under
  the parent session, tagged `subagent`; hardlinked twins of the same
  subagent transcripts (one session observed under both its real project path
  and a volume-alias path) are deduped by `FileIdentity` in the indexer.
  Strict `.jsonl` only (`.jsonl.backup`/`.bak-*` siblings refused); prose is
  the explicit text blocks (`message_prose_text` structurally rejects
  `tool_result` blocks, so the parent's copy of a subagent's final report is
  never double-indexed). This extractor **never emits supersession marks**:
  Claude Code has no rollback mechanism, and compaction is not supersession.

### The indexer sweep

`spawn_indexer` (wired unconditionally in `main.rs`'s startup prologue) runs a
background sweep every **30 seconds**; the first sweep runs one full interval
after boot, so one-shot CLI runs exit before ever paying for it. Sweeps do
real file I/O and run under `spawn_blocking`. One sweep:

- enumerates `~/.intendant/logs/*/session.jsonl`, the Codex roots (the user's
  Codex home plus leased-active homes and staged lease entries), and the
  Claude project roots (`~/.claude/projects` plus the leased/staged
  equivalents);
- checks each known file against its stored cursor (`cursor.rs`:
  `FileIdentity` + length + mtime + a 4 KiB prefix hash +
  `last_complete_line_offset`) — `Unchanged` skips without reading,
  `Appended` reads incrementally past the offset (a partial trailing line
  stays unconsumed until it completes), `Rewritten` rebuilds (for Codex: the
  generation bump above), `Gone` leaves the shard serving with the
  `source_gone` coverage flag until retention expires it;
- remembers sources that publish nothing — wrapper session logs, rollouts
  with no message content yet — in an in-process cache rather than the store
  (an empty shard's `newest_ts_ms` of 0 would be GC-evicted and re-parsed
  forever);
- drains staged lease entries: deletes an entry once every transcript file
  inside it has been published.

**Leased homes and custody** (`lease_transcript_staging.rs`): OAuth leases
materialize private `CODEX_HOME`/`CLAUDE_CONFIG_DIR` roots that hold the
borrowed secret *and* the agent's transcripts. Live homes are registered under
`leased-active/` and indexed during the lease; on cleanup
(expiry/revocation/shutdown/crash sweep) the transcript subdirectories are
**renamed** into `staging/` (same volume, effectively O(1)) and the auth root
is deleted immediately — **secret deletion never waits on indexing**, and
there is deliberately no copy fallback. Staging failures are markers, not
blockers: deletion proceeds regardless.

Freshness needs no event plumbing: the steady-state sweep is cheap (metadata
plus a prefix hash per known file), and the query edge shares the same
indexer instance through `refresh_if_stale(1_000)` — a query first sweeps if
the last completed sweep is older than a second, which meets ~1 s freshness
for native and supervised sessions. That refresh is deliberately a no-op
before the first boot sweep completes (the backfill can take seconds and must
never run inline with a query); until then coverage reports `building`.

### The query route

`GET /api/sessions/message-search` is declared as a `gateway_routes.rs` row
(`PeerOperation::SessionInspect`, `BodyPolicy::None`, tunnel twin
`api_sessions_message_search`) — it appears in the derived endpoint table in
[Web Dashboard](./web-dashboard.md). The gateway edge
(`sessions_message_search_api_response` in `web_gateway/routes_sessions.rs`)
owns transport, the freshness refresh, and a per-daemon concurrency cap of 2
(excess answers 429 `busy`); the pure query core is
`message_search::run_message_search`.

| Param | Meaning |
|---|---|
| `q` | required; ≤ 256 bytes, ≤ 8 whitespace-separated terms — **all terms must match within one message** |
| `source` | comma list of `intendant`, `codex`, `claude-code`; empty or `all` = every source |
| `include_superseded` | default `true`: superseded hits are included and badged, hideable by the client |
| `subagents` | default `true`: include Claude subagent records |
| `cursor` | opaque continuation cursor from the previous page |
| `limit` | sessions per page, clamped 1–50, default 20 |

Mechanics (constants in `query.rs`):

- **Matching**: needle and haystack are folded identically — per-char simple
  lowercase plus Unicode canonical decomposition (NFD; matches the same pairs
  as NFC + case-fold while keeping the folded→original offset map exact per
  original char). Highlight `ranges` are **byte offsets into the ORIGINAL
  text**; the client never re-derives them. Each hit carries a snippet
  (≤ 280 bytes around the earliest highlight) plus `snippet_offset_bytes`.
- **Ordering**: sessions by best-hit `ts_ms` descending (tiebreak session
  key), ≤ 3 most-recent hits per session, `total_hits` on the group. The
  manifest scan walks sessions newest-first with an early exit once the page
  can no longer improve, so broad terms are limit-bounded rather than
  full-corpus.
- **Snapshot-bound pagination**: the cursor pins the query/filter fingerprint
  and the manifest `revision` it was minted against. Any manifest change
  between pages answers 410 `cursor_expired` and the client restarts —
  stricter than the design minimum (a page can never silently skip a session
  that moved), and cheap to loosen later.
- **Budgets**: a 150 ms per-query time budget (`state: "partial"`,
  `partial_reason: "timeout"`; deliberately *no* continuation cursor — a
  timeout page scanned an unknown subset, so the client retries the whole
  query against a then-warm arena); a 256 KiB response budget and a 48 MiB
  per-query fresh-load ceiling (`partial_reason: "budget"`, with a
  continuation cursor). Parsed + folded shards stay resident in a 192 MiB
  byte-bounded LRU arena keyed by content-named generation file, with derived
  active flags computed once at load.
- **Response**: `state` (`ready` | `building` — nothing published yet |
  `partial`), `window_days`, session groups (`session_key`, `source`,
  `session_id`, `best_ts_ms`, `total_hits`, `source_gone`, `hits` with role /
  `ts_ms` / `seq` / `superseded` / `truncated` / `subagent` / snippet /
  `ranges` / `locator`), the continuation `cursor`, and a `coverage` block:
  per-source session counts with oldest/newest indexed `ts_ms` and
  `source_gone` counts, plus the legacy matrix (below).
- **Privacy**: responses are `Cache-Control: no-store` (the canonical
  envelope's `no-cache` still allows caching), and `q` is never logged —
  neither the HTTP lane nor the tunnel logs request lines or params.

### Jump-to-hit: `locate=` on the session detail read

Every hit carries its locator, and the session detail read resolves it:
`GET /api/session/{id}?locate=<locator>` accepts the locator JSON itself
(URL-encoded) or base64url of it — the tunnel twin may pass the object
directly (`web_gateway/session_catalog/locate.rs`). The response is the
normal paged detail body plus an additive `locate` object:

- `resolved` — the served page is centered on the anchored entry;
  `entry_index` is its position in the response's `entries`, `total_index`
  its position in the full list, and `anchor` is `"exact"`, or `"nearest"`
  when the located row renders no entry of its own (canonical
  `conversation_message` rows — their diagnostic twins are what the detail
  view shows) and the closest rendered neighbor anchors it.
- `stale` — the source no longer matches the locator (content-hash mismatch,
  rewritten/rolled-back external thread, truncated log). The page is served
  exactly as an unanchored request, with the reason.
- `unavailable` — well-formed but nothing resolvable backs it (missing
  file/record, locator kind vs. source mismatch).

A malformed `locate` parameter is a 400 like any bad parameter; every
well-formed locator degrades typed, so the dashboard can open the detail view
unanchored and say why. Sidecar reads never escape the session directory.

### Dashboard integration (default on; `?message_search=off` escape)

The dashboard message lane is enabled by default. `?message_search=off` (or
`=0`) disables it as an operational escape hatch; daemon-derived method
availability still makes older daemons degrade cleanly. Two surfaces
(`static/app/57-sessions-message-search.js`, `ui2-chrome.js`): the Recent
list's quick search gains a full-text message lane *under* the immediate
metadata lane — ≥ 2 characters, ~225 ms debounce, in-flight aborts and
stale-response rejection, hits attached to their session cards with role,
timestamp, best snippet with server-anchored highlights, and
superseded/truncated/subagent badges (a toolbar toggle flips
`include_superseded`); and the ⌘K palette's Messages section (which queries
with `include_superseded: false`). Availability is the derived per-method
boolean (`api_sessions_message_search_available`, from the tunnel method
table) — an older daemon hides the lane rather than erroring.

### What is never indexed

By construction (enforced by provenance on the native side and the extractor
skip rules on the external side):

- tool output — except the askHuman-answer projection (`ref_seq`);
- reasoning content (`turns/*_reasoning.txt` stays a diagnostic);
- meta and sidecar records (Claude `isMeta`/`isCompactSummary` rows, sidecar
  state types, records without `uuid`/`timestamp`);
- system injections — working-dir/memory/skills preludes, nudges, acks,
  agent-stdout-as-user, Codex machine injections, Claude harness envelopes
  (`is_injected_external_user_text`);
- context summaries (compaction output);
- the CU sub-conversation — excluded by construction in both eras
  (`run_cu_task` writes only `info` and `cu_*` events, never
  `model_response`);
- presence conversations;
- wrapper-mirrored external messages (canonical in the backend's own log;
  the wrapper session is skipped wholesale).

**The legacy matrix** — what sessions written before the message lane
(pre-2026-07) can ever yield, surfaced verbatim in the coverage block:
askHuman answers **none** (they were never persisted — the audit hole the
lane closed); follow-ups **best-effort** (reconstructed from
`"Round {N} follow-up:"` info lines).

## Test Coverage

Session logging is exercised by inline `#[cfg(test)]` tests in `session_log/`
and `session_names.rs`: turn-file creation and pretty-printing, separate
stdout/stderr files, skipping empty stderr, `json_extracted` function extraction,
reasoning-file writes, span-based chunk reads that avoid re-reading whole turn
files, intendant-meta renames, and external-source overlay application.

The message-search subsystem carries its own inline suites (`message_search/`):
per-source extractor fixtures across both eras, store coverage (crash-safe
publish, corrupt-manifest quarantine, stale-writer rejection, tombstones,
retention GC, lock takeover), snapshot-cursor stability under concurrent
publishes, and fold/highlight offset mapping. Two headless e2e tests pin the
message-lane wire contract against the real binary
(`supervised_session_writes_task_ask_human_and_steer_message_rows`,
`headless_rollback_writes_a_conversation_rewound_cut` in `tests/e2e/`).

The 20 integration cases in `tests/e2e/main.rs` spawn the real binaries against
the deterministic mock provider and synthetic display; they are keyless,
headless, network-independent, and run in CI on all three supported platforms.
They cover direct execution and runtime file writes; daemon session/project and
worktree creation; HTTP transfers and NDJSON session streaming; WebSocket
display-request and computer-use event rails; blocking questions and persisted
notifications through `intendant ctl`; federated peer tasks plus mTLS-scoped
display input; supervised approvals and ask-human flows; parked steering; and
both supervised and headless conversation rollback/message-lane cuts. Real-LLM,
native-display, browser/Station, voice, and audio scenarios remain separate
`tests/skills/` smokes rather than fictional extra e2e tiers.
