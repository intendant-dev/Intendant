# Intendant Session Event Taxonomy

Use this when you need to interpret `session.jsonl` rows.

## Contents

- Base row schema
- Large-payload indirection
- Lifecycle and identity
- Model, context, and runtime execution
- Approvals and human steering
- External-agent activity
- Computer use, display, frames, and recordings
- Voice and presence
- Rewind, file history, and managed context
- Replay caveats

## Base Row Schema

Every persisted session event is a JSON object with these common fields:

- `ts`: local time string, usually `HH:MM:SS.mmm`.
- `turn`: optional numeric model/agent turn.
- `event`: event name.
- `level`: optional severity/category such as `debug`, `info`, `warn`, `error`, `model`, or `reasoning`.
- `message`: optional compact display/search string.
- `data`: optional structured fields.
- `file`: optional relative sidecar file.
- `file2`: optional second relative sidecar file, usually stderr.

The session log writer flushes each row immediately. The event bus uses a lossless session-log sink separate from the bounded UI broadcast channel.

## Large-Payload Indirection

Use row file fields before assuming `message` is complete.

- `messages_input`: `file` is `turns/turn_NNN_messages.json`. Debug-only rows (`INTENDANT_LOG_MESSAGES_JSON=1`), except when the provider produced no context snapshot for the turn — then the dump is written unconditionally.
- `model_response`: appends text to `turns/turn_NNN_model.txt`; exact row span is `data.model_offset` and `data.model_bytes`.
- `reasoning`: may write `turns/turn_NNN_reasoning.txt`; summary-only rows can omit `file`.
- `agent_input`: `file` is `turns/turn_NNN_agent_in.json`.
- `agent_output`: stdout file in `file`, stderr file in `file2`; exact spans are `data.stdout_offset`, `data.stdout_bytes`, `data.stderr_offset`, and `data.stderr_bytes`.
- `context_snapshot`: `file` is a raw/archive context JSON file; replay may summarize or omit large raw values. Sidecars rotate to latest-only per (source, session id) stream — only the newest row's `file` per stream exists on disk (`INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL=1` keeps all), and replayed rows carry `exact_replay_available` derived from disk truth.

## Lifecycle and Identity

Common lifecycle rows:

- `session_start`: emitted when `SessionLog::open()` creates/opens a log.
- `session_started`: visible session started/attached in daemon/dashboard flows.
- `session_identity`: maps visible Intendant session ids to backend/source ids. Important for external-agent wrappers.
- `session_attached`: dashboard or replay attachment event.
- `session_relationship`: parent/child or related session relationship.
- `session_capabilities`: frontend/backend capabilities for a session.
- `session_goal`: current goal state for a session.
- `turn_start`: model/agent turn boundary.
- `round_complete`: native multi-agent round boundary.
- `done_signal`: agent signaled done.
- `task_complete`: task completion.
- `session_ended` and `session_end`: session completion/summary rows.
- `safety_cap_reached`: execution/budget safety cap.

`session_meta.json` is usually easier for session id, task, status, project root, and role. Use lifecycle rows for timeline and relationship details.

## Model, Context, and Runtime Execution

Model/context rows:

- `messages_input`: full model request messages in `turns/`.
- `context_snapshot`: context payload, token count, context window, hard window, item count, source, labels, request ids, and optional backend session id.
- `model_response`: assistant output chunk or full response, plus token/cost fields when available.
- `reasoning`: model/backend reasoning content or summary.
- `json_extracted`: internal parsed tool-call JSON details.

Runtime/tool rows:

- `agent_input`: native runtime request JSON, including function/tool names.
- `agent_output`: stdout/stderr returned to the controller or emitted by an external-agent tool.
- `agent_started`: external/native tool execution started.

Useful `agent_output` fields:

- `data.output_id`: stable id for output chunk lookup.
- `data.stdout_bytes` and `data.stderr_bytes`: bytes written for this event.
- `data.source`: backend/source label when available.
- `data.session_id`: visible/backend session id when output belongs to a child or external session.

For native `execAsAgent`, use runtime `<nonce>_stdout.log` and `<nonce>_stderr.log` files if completeness matters.

## Approvals and Human Steering

Approval rows:

- `approval`: approval requested.
- `approval_resolved`: approval answered.
- `auto_approved`: autonomy policy auto-approved an action.

Human question rows:

- `human_question`: model/runtime asked for human input.
- `human_response_sent`: answer was sent back to the runtime/model path.

Steering/follow-up rows:

- `steer_requested`
- `steer_queued`
- `steer_accepted`
- `steer_delivered`
- `steer_cancelled`

Some interrupt and follow-up state changes are persisted as generic `info` rows.

## External-Agent Activity

Wrapper logs normalize backend events into Intendant row types.

Identity:

- `session_identity` data contains `source`, wrapper `session_id`, and backend-native `backend_session_id` or equivalent.
- `~/.intendant/external_wrapper_index.json` provides a global map from backend id to wrapper log path.

Assistant/user activity:

- External assistant text is persisted as `model_response`.
- External user messages can appear as replay/transcript entries and native backend JSONL rows.
- External tool starts often emit `agent_started`.
- External tool output emits `agent_output` with `output_id`.

Backend stderr:

- Backend stderr is stripped of ANSI, capped, classified, and forwarded into wrapper logs as generic `info`, `warn`, or `error` rows with messages like `[codex stderr] ...`.

Codex rollout rows in native files are not Intendant `LogEvent` rows. Important native row types include:

- `session_meta`
- `turn_context`
- `event_msg` with payload types such as `user_message`, `agent_message`, `token_count`, `thread_rolled_back`, `thread_goal_updated`, `thread_goal_cleared`
- `response_item` with payload types such as `message`, `function_call`, and `function_call_output`

Dashboard external replay filters and deduplicates native backend transcript rows. It intentionally hides internal developer/environment/subagent notification messages in compact transcript views.

## Computer Use, Display, Frames, and Recordings

Computer-use rows:

- `cu_task_start`
- `cu_turn`
- `cu_task_complete`
- `cu_task_error`

Display/debug rows:

- `display_ready`
- `display_resize`
- `display_taken`
- `display_released`
- `debug_screen_ready`
- `debug_screen_torn_down`

Recording rows:

- `recording_started`
- `recording_stopped`
- `recording_error`
- `recording_deleted`

Several display-control state changes are logged as generic `info` or `warn`, including capture lost, display approval pending, shared-view state, and user display grant/revoke.

Actual visual evidence is usually in `frames/` and `recordings/`, not the event row itself.

## Voice and Presence

Voice/presence rows:

- `voice_log`
- `user_transcript`
- `presence_checkpoint`
- `presence_connected`
- `presence_disconnected`
- `voice_audio`
- `voice_protocol`
- `voice_frame`
- `voice_usage`
- `voice_error`
- `presence_log`
- `presence_usage_update`
- `live_usage_update`
- `live_audio_started`
- `live_audio_progress`
- `live_audio_completed`
- `tool_request`
- `tool_response`

High-frequency audio/protocol events are often suppressed from dashboard replay. Use `session.jsonl` for audit and `transcript.jsonl` for conversation.

`search_voice_entries()` searches only `voice_log` and `user_transcript`.

## Rewind, File History, and Managed Context

File watcher/history rows:

- `snapshot_created`
- `rolled_back`
- `redone`
- `history_pruned`
- `conversation_rolled_back`

Live file changes are often persisted as generic `info` messages like `file_modified: path (+n/-m)`.

Managed context rows and sidecars:

- `context_snapshot`: current context payload and token pressure.
- `rolled_back` or backend-native `thread_rolled_back`: context rollback markers.
- `context_rewinds/<record_id>.json`: surgical/managed context rewind details.
- `fission_ledger.json`: parallel branch group/branch ledger.

Use `context_snapshot` plus sidecars when investigating context pressure, fission branches, or why context was trimmed.

## Replay Caveats

`session_log_entry_to_app_event()` intentionally skips some persisted rows during dashboard replay:

- `session_start`
- `messages_input`
- `json_extracted`
- `agent_input`
- `voice_audio`
- `voice_frame`
- summary/interrupted internals
- many voice/presence/protocol/usage rows
- live-audio progress internals

Replay reconstructs UI entries by reading referenced files and spans for `model_response`, `agent_output`, `reasoning`, and `context_snapshot`.

Do not infer that a missing dashboard Activity row means the event was not persisted. Search `session.jsonl` and sidecars directly.

Do not infer that a persisted event name exactly matches an `AppEvent` variant. The session-log writer persists many `AppEvent`s, but several become generic `info` or `warn` rows for compactness/backward compatibility.
