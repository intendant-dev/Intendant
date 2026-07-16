# Intendant Log Artifact Map

Use this when you need to know where evidence lives and what each artifact can prove.

## Contents

- Roots and session identity
- Core session files
- Turn files and byte spans
- Runtime side files
- Display, frame, and recording artifacts
- File snapshots and uploads
- External-agent wrappers and backend logs
- Managed-context sidecars
- Peer and daemon logs
- Artifact selection checklist
- Source files to check when stale

## Roots and Session Identity

Default session root:

```text
~/.intendant/logs/<uuid>/
```

The controller can override this with `--log-file <DIR>`. At startup it prints `Session log: <dir>/session.jsonl` and `Session ID: <id>`. The runtime receives the directory through `INTENDANT_LOG_DIR`.

If `SessionLog::open()` cannot create/open the requested dir, startup may fall back to `/tmp/intendant_session`.

The session id is normally the log directory basename. `session_meta.json` can also carry a canonical `session_id`; session lookup accepts exact ids, path-looking ids, and prefixes.

`session_meta.json` fields:

- `session_id`: canonical Intendant session id.
- `created_at`: local timestamp string written when the log opens.
- `project_root`: project directory, when known.
- `name`: user/session display name, if set.
- `task`: initial task or current task label.
- `status`: `running`, `completed`, `interrupted`, `idle`, etc.
- `last_turn`: latest completed turn number.
- `role`: orchestrator/sub-agent/external role marker.
- `rounds`: native multi-agent round count.

`SessionLog::find_latest_session(project_root)` filters by `project_root`, skips sub-agent roles like `orchestrator`, `research`, `implementation`, and `testing`, then picks the newest `created_at`.

## Core Session Files

`session.jsonl`

- Canonical durable timeline for Intendant sessions.
- One JSON object per line.
- Rows have `ts`, optional `turn`, `event`, optional `level`, optional `message`, optional `data`, optional `file`, optional `file2`.
- The writer flushes every row.
- Many large values are stored by reference in `turns/` files.

`transcript.jsonl`

- Compact conversation log with `ts`, `role`, `text`, optional `tools_called`.
- Appended live for user speech and voice paths; rebuilt at session end from durable events for selected transcript-like events.
- Use for orientation and quick summaries. Do not treat it as a complete audit log.

`conversation.jsonl`

- Native internal conversation state, one serialized message per JSON line.
- Used for resume/rehydration, not for dashboard replay.
- On resume the controller appends a synthetic resumed-session user message.

`summary.json`

- Simple end-of-session summary.

`session_summary.json`

- Rich session summary: duration, voice provider/model/connections/reconnects, model turn count, computer-use task count, frames sent, errors, user transcript count, and total tokens.

`daemon.log`

- Controller stdout/stderr tee for non-interactive/headless/web/MCP-style runs.
- On Unix, lines are prefixed with local wallclock `HH:MM:SS.mmm`.
- Mirrors output to the original terminal and writes the file line-buffered.
- Windows install is currently a no-op.

## Turn Files and Byte Spans

All paths in `file` and `file2` are relative to the session directory.

`turns/turn_NNN_messages.json`

- Full model input messages for a turn.
- Logged by `messages_input`.

`turns/turn_NNN_model.txt`

- Assistant/model response text.
- `model_response` rows append chunks here and record `data.model_offset` and `data.model_bytes`.
- Read the file for the whole turn, or use the span for the exact row/chunk.

`turns/turn_NNN_reasoning.txt`

- Full reasoning text when the provider/backend exposes it.
- If only a summary exists, the `reasoning` row may have no file.

`turns/turn_NNN_agent_in.json`

- Pretty JSON runtime request for native tool execution.
- `agent_input` rows include `data.functions` and `data.json_length`.

`turns/turn_NNN_stdout.txt` and `turns/turn_NNN_stderr.txt`

- Append-only stdout/stderr material returned to the controller.
- `agent_output` rows include `data.stdout_offset`, `data.stdout_bytes`, `data.stderr_offset`, `data.stderr_bytes`, `data.output_id`, optional `data.source`, and optional `data.session_id`.
- For external agents these files contain normalized tool output. For native runtime commands, full stdout/stderr may instead require the runtime side files described below.

`turns/turn_NNN_context_<uuid>.json` or `turns/context_<uuid>.json`

- Raw or archived context snapshot payload.
- `context_snapshot` rows include `data.source`, `label`, `request_id`, `request_index`, `format`, `token_count`, `token_count_kind`, `context_window`, `hard_context_window`, `item_count`, optional `session_id`, and `file`.

## Runtime Side Files

The sandboxed runtime writes additional files directly into the session directory.

`<nonce>_stdout.log` and `<nonce>_stderr.log`

- Full stdout/stderr from native `execAsAgent` commands.
- The command result returned to the controller includes only a tail, so use these files when output completeness matters.

`screenshot_<nonce>.png`

- Screenshot created by runtime screen capture.

`human_question` and `human_response`

- Temporary files for the runtime `askHuman` path.
- The runtime writes `human_question`, polls for `human_response`, then deletes both after reading.

`session.Xauthority`

- Linux/X11 support file written when needed for display access.

## Display, Frame, and Recording Artifacts

`frames/`

- Created by `FrameRegistry`.
- Stores high-quality JPEG frames as `frames/<frame_id>.jpg`.
- Appends frame metadata to `frames/frames.jsonl`.

`frames/frames.jsonl` row schema:

- `frame_id`: client-assigned id like `cam0-f00047`.
- `stream`: source such as `cam0` or `display:99`.
- `timestamp`: UTC capture timestamp.
- `sent_to_live`: whether this frame was sent to the live model.
- `live_resolution`: optional model image resolution.
- `hq_resolution`: optional saved image resolution.
- `note`: optional user/annotation note.

`recordings/<stream>/`

- Created by `RecordingRegistry`.
- Streams are named like `display_<id>`, `display_<id>_2`, or a frame-fed stream name.
- `manifest.json`: `stream_name`, `started_at`, `framerate`, optional `resolution`, `codec`, `source`.
- `segments.csv`: ffmpeg segment list, `filename,start_time,end_time`.
- `seg_*.mp4`: fragmented MP4 segments.
- `ffmpeg.log`: ffmpeg stderr and startup/capture failures.
- Empty/unplayable recording directories are deleted when stopped.

Session list rows compute counts/bytes for recordings, frames, annotations, clips, turns, and logs, but those summary fields are derived. Inspect files for exact evidence.

## File Snapshots and Uploads

`file_snapshots/`

- Created by `FileWatcher` inside the session dir.
- Captures initial baselines and per-round content-addressed snapshots.
- Used for dashboard rewind/redo and file history.

Layout:

```text
file_snapshots/
  baseline/
  baseline_manifest.json
  objects/
  rounds/
  history.json
  store.lock
```

`store.lock` is the store's advisory cross-process lock (held for the owning
watcher's lifetime; a second process opens the store read-only). A
`history.json.damaged-<ts>-<pid>-<seq>` file is a previous index that failed
to parse, preserved verbatim when a fresh timeline was started (forensic
only: the fresh timeline reuses round ids, and its epoch/maps-hash binding
already makes the old manifests unresolvable).

`history.json` schema (format 2 — a slim index; `"format": 2` marks it):

- `current_head_id`: active round id.
- `rounds[]`: `id`, `parent_id`, `summary`, `timestamp_unix`, `files_changed`, optional `turn_count`, optional `native_message_count`, optional `maps_from_round`, optional `maps_hash` (content hash of the round's maps — the resolver refuses a manifest whose payload doesn't hash to it). Round stubs carry no path→hash maps — the per-round maps live in `rounds/round_<id>/manifest.json` (below). A round may exceptionally retain inline `files_at_end`/`all_files_at_end` when its manifest write failed, marked by `maps_inline: true` (the marker is what keeps an empty-tree retention alive, since empty maps serialize to nothing); the index stays authoritative for it until a later load migrates it.
- `abandoned_branches[]`: rollback-then-new-action branches with `branched_from_id`, `rounds`, `created_at_unix`.
- `next_id`: next round id.
- `store_epoch`: identity stamp binding this index to its manifests. Absent on a pre-epoch store whose manifest stamping has not completed yet; while absent, restores are guarded by content binding (the manifest's scalars must match its index row) instead.

`rounds/round_<id>/manifest.json` (load-bearing since format 2): the full `HistoryRound` for that round — `files_at_end` (restorable path → sha256) and `all_files_at_end` (display mirror) inline, stamped with `store_epoch`. A no-op round (tree identical to an earlier round) writes a tiny stub whose `maps_from_round` names the round holding the maps inline (backreferences are depth-1). Restore refuses manifests whose `id` or `store_epoch` doesn't match the index (fails closed rather than restoring a wrong tree).

Legacy (pre-format-2) `history.json` files carried every round's maps inline; they are migrated to stamped manifests on the next load.

Snapshots ignore common generated directories and binary/image/archive extensions. Files above the snapshot size cap are represented by hash/metadata, not restorable text content.

Uploads:

- Current dashboard uploads are stored under `<project_root>/.intendant/uploads/<session-id>/`.
- Legacy task uploads may exist under `<session_dir>/uploads/`.
- Each upload has a sidecar JSON descriptor next to the bytes. Fields include `id`, `name`, `original_name`, `mime`, `size`, `path`, `destination`, `session_id`, `created_at`.

## External-Agent Wrappers and Backend Logs

Wrapper logs:

- External Codex and Claude Code sessions also get Intendant wrapper dirs under `~/.intendant/logs/<uuid>/`.
- The wrapper `session.jsonl` normalizes backend activity into Intendant events.
- `session_identity` rows map wrapper id to backend-native id.
- `session_agent_config.json` persists launch config: `source`, `project_root`, `agent_command`, Codex sandbox/approval/managed-context/service-tier values, `codex_context_archive`, and `codex_home`.

Global wrapper index:

```text
~/.intendant/external_wrapper_index.json
```

Schema:

- `version`
- `wrappers[]`: `source`, `backend_session_id`, `intendant_session_id`, `log_path`, optional `project_root`, `updated_at_secs`.

Other global session overlays:

- `~/.intendant/session_names.json`: external session display-name overrides, keyed by source and session id.
- `~/.intendant/deleted_external_sessions.json`: external sessions hidden/deleted in the dashboard, keyed by source to arrays of ids. Dashboard deep search filters parent references to these ids.

Codex native logs:

- Determine `codex_home` from `session_agent_config.json`, `$CODEX_HOME`, or `~/.codex`.
- Search `codex_home/sessions/**/*.jsonl` and `codex_home/archived_sessions/**/*.jsonl`.
- Match the file by reading a `session_meta` row with `payload.id == backend_session_id`; filename containment is only a fast path.
- Rollout rows include `session_meta`, `turn_context`, `event_msg`, and `response_item` variants. Useful payloads include `user_message`, `agent_message`, `token_count`, `thread_rolled_back`, `thread_goal_updated`, `thread_goal_cleared`, `message`, `function_call`, and `function_call_output`.

Claude Code native logs:

- Dashboard lookup scans `~/.claude/projects/**/*.jsonl`.
- It matches files whose stem equals the session id.

## Managed-Context Sidecars

`model-request-traces/`

- Exact Codex request trace archives live inside the session dir when `codex_context_archive` is `exact`.
- Summary/default archive mode may use a temporary trace dir and persist only summarized context snapshot payloads.
- Prefer session `context_snapshot` rows/files first; use trace bundles when investigating exact provider request payloads.
- Trace bundles have `trace.jsonl` and payload files. `inference_started` rows point to request payload paths; `inference_completed` rows point to response payloads.

`context_rewinds/`

- `context_rewinds/<record_id>.json`: managed-context rewind record.
- `context_rewinds/<record_id>-source-rollout.jsonl`: copied source rollout before mutation.

Important rewind fields:

- `record_id`, `created_at`, `session_id`, `thread_id`, `item_id`, `position`, `reason`.
- `primer`, `preserve`, `discard`, `artifacts`, `next_steps`.
- `source_rollout_path`, `recovery_rollout_path`.
- Optional `fission_snapshot`, `lineage_ledger`, `fission_ledger`, `detached_fission_group_ids`.
- `used_tokens_at_rewind`, `context_window_at_rewind`, `pressure_band_at_rewind`, `surgical`.

`fission_ledger.json`

- Tracks parallel managed-context branch groups.
- Top-level document has `groups[]` and optional `ext`.
- Group fields include `group_id`, `parent_session_id`, `anchor_item_id`, `tool`, `objective`, `prompt`, timestamps, `canonical_session_id`, and `branches[]`.
- Branch fields include `session_id`, optional `backend_session_id`, `status`, `summary`, `task`, `model`, `reasoning_effort`, `worktree_path`, `raw_log`, `ephemeral`, and `updated_at`.
- Status values include `running`, `blocked`, `completed`, `failed`, `detached`, `cancelled`, plus legacy `ended`, `interrupted`, and `unknown`.
- `raw_log` may be `session.jsonl#session_id=...`, meaning the branch data is embedded in a wrapper log and must be filtered by session id.

## Peer and Daemon Logs

`peers.jsonl`

- Session-scoped peer federation event log.
- Each row serializes a `TaggedPeerEvent` with `peer`, `payload`, and `seq`.
- The writer flushes after every event.

`daemon.log`

- Use when `session.jsonl` is silent but the controller or external backend emitted stdout/stderr.
- It often contains panics, eprintln diagnostics, backend stderr lines, and failed setup details.

## Artifact Selection Checklist

- User-facing conversation: `transcript.jsonl`, then `session.jsonl` user/voice/model events.
- Exact model prompt/context: `messages_input` file and `context_snapshot` file.
- Exact assistant output: `model_response` file spans.
- Reasoning: `reasoning` rows and `turn_NNN_reasoning.txt`.
- Tool command/output: `agent_input`, `agent_output`, turn stdout/stderr spans, runtime `<nonce>_*.log` files.
- Runtime failures: `daemon.log`, runtime stderr files, `error`/`warn` rows.
- External backend identity: `session_identity`, `session_agent_config.json`, `external_wrapper_index.json`.
- Display evidence: `frames/frames.jsonl`, frame JPEGs, recording segments, `ffmpeg.log`.
- File edits and rewind: `file_snapshots/history.json`, `snapshot_created`, `rolled_back`, `redone`, `conversation_rolled_back`.
- Context pressure/rewind/fission: `context_snapshot`, `context_rewinds/`, `fission_ledger.json`, Codex rollouts.

## Source Files to Check When Stale

If behavior changes, start with these files instead of rediscovering the whole repo:

- `src/bin/caller/session_log/`: `SessionLog`, `LogEvent`, turn sidecars, summaries (`mod.rs`), bus-event writers (`bus_events.rs`), replay conversion + file-span readers (`replay.rs`), history read-back (`history.rs`).
- `src/bin/caller/event.rs`: `AppEvent` durability allow-list and `write_event_to_session_log()`.
- `src/agent.rs`: sandboxed runtime log files, screenshot files, `askHuman` temp files, stdout/stderr tailing.
- `src/bin/caller/main.rs`: controller session creation/resume, daemon log tee install, runtime env, frame/recording/file-watcher setup.
- `src/bin/caller/session_supervisor/`: daemon-created sessions, related sessions, external resume/attach flows.
- `src/bin/caller/web_gateway.rs`: dashboard session list/search/detail/replay, external native log discovery, deleted-session filtering.
- `src/bin/caller/external_wrapper_index.rs`: global backend-id to wrapper-log index.
- `src/bin/caller/session_config.rs`: `session_agent_config.json` and effective Codex home.
- `src/bin/caller/external_agent/mod.rs` and `src/bin/caller/external_agent/codex.rs`: normalized external-agent events, Codex rollout/context trace behavior.
- `src/bin/caller/frames.rs`: `frames/frames.jsonl` and saved JPEGs.
- `src/bin/caller/recording.rs`: recording manifests, segment CSVs, ffmpeg logs, empty recording deletion.
- `src/bin/caller/file_watcher.rs`: `file_snapshots/`, `history.json`, rollback/redo/prune behavior.
- `src/bin/caller/context_rewind.rs` and `src/bin/caller/fission_ledger.rs`: managed-context rewind/fission sidecars.
- `src/bin/caller/daemon_log_tee.rs`: `daemon.log` tee behavior.
- `src/bin/caller/upload_store.rs`: project-local upload store and upload descriptor sidecars.
- `src/bin/caller/peer/log_writer.rs`: `peers.jsonl` durable peer federation log.
