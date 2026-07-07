# Intendant Log Query Recipes

Use this for concrete commands. Set `S` to the session directory first:

```bash
S="$HOME/.intendant/logs/<session-id>"
```

## Contents

- Locate sessions
- Inspect a session quickly
- Search text and JSON fields
- Read turn files and spans
- Find failures, approvals, tool calls, and context pressure
- External-agent mappings and backend logs
- Display, recordings, file history, and managed context
- Dashboard deep-search behavior

## Locate Sessions

Newest sessions:

```bash
find "$HOME/.intendant/logs" -type f -name session_meta.json -print 2>/dev/null |
while IFS= read -r f; do
  jq -r '[.created_at // "", .session_id // (input_filename|split("/")[-2]), .status // "", .project_root // "", .task // ""] | @tsv' "$f"
done | sort -r | head -50
```

Sessions for the current project:

```bash
find "$HOME/.intendant/logs" -type f -name session_meta.json -print 2>/dev/null |
while IFS= read -r f; do
  jq -r --arg p "$PWD" 'select((.project_root // "") == $p) | [.created_at // "", .session_id // (input_filename|split("/")[-2]), .status // "", .task // ""] | @tsv' "$f"
done | sort -r
```

Find a session by full or prefix id:

```bash
SID="<id-or-prefix>"
find "$HOME/.intendant/logs" -type d -name "$SID*" -print 2>/dev/null
find "$HOME/.intendant/logs" -type f -name session_meta.json -print 2>/dev/null |
while IFS= read -r f; do
  rg -l "\"session_id\"\\s*:\\s*\"$SID" "$f" 2>/dev/null
done
```

Find wrapper logs for a backend external-agent id:

```bash
BACKEND="<backend-session-id>"
jq -r --arg id "$BACKEND" '.wrappers[]? | select((.backend_session_id // "") == $id or (.backend_session_id // "" | contains($id))) | [.source, .backend_session_id, .intendant_session_id, .log_path, (.project_root // "")] | @tsv' \
  "$HOME/.intendant/external_wrapper_index.json"
```

## Inspect a Session Quickly

Metadata:

```bash
jq . "$S/session_meta.json"
```

Event histogram:

```bash
jq -r '.event // "?"' "$S/session.jsonl" | sort | uniq -c | sort -nr
```

Timeline:

```bash
jq -r '[.ts // "", (.turn // ""), .level // "", .event // "", .message // ""] | @tsv' "$S/session.jsonl" | less -S
```

Last 80 rows:

```bash
tail -n 80 "$S/session.jsonl" | jq -c '{ts,turn,level,event,message,data,file,file2}'
```

Transcript:

```bash
jq -r '[.ts // "", .role // "", .text // ""] | @tsv' "$S/transcript.jsonl" 2>/dev/null | less -S
```

Summary:

```bash
jq . "$S/session_summary.json" 2>/dev/null || jq . "$S/summary.json" 2>/dev/null
```

## Search Text and JSON Fields

Search relevant text-bearing files without walking binary-heavy frame/recording dirs:

```bash
find "$S" \( -path "$S/frames" -o -path "$S/recordings" -o -path "$S/file_snapshots" -o -path "$S/context_rewinds" \) -prune -o \
  \( -name session.jsonl -o -name transcript.jsonl -o -name daemon.log -o -name '*_stdout.log' -o -name '*_stderr.log' -o -path "$S/turns/*" \) \
  -type f -print0 |
while IFS= read -r -d '' f; do
  rg -n -i "needle" "$f"
done
```

Search `session.jsonl` string fields recursively, like dashboard deep search does:

```bash
jq -c 'select(([.. | strings] | join("\n")) | test("needle"; "i"))' "$S/session.jsonl"
```

Search only messages and data:

```bash
jq -c 'select((((.message // "") + "\n" + (.data // {} | tostring)) | test("needle"; "i")))' "$S/session.jsonl"
```

Search warnings and errors:

```bash
jq -c 'select((.level // "") == "warn" or (.level // "") == "error" or (.event // "") == "error")' "$S/session.jsonl"
rg -n -i "error|panic|failed|forbidden|unauthorized|connection refused" "$S/daemon.log" "$S/session.jsonl" 2>/dev/null
```

## Read Turn Files and Spans

List file-backed rows:

```bash
jq -c 'select(.file or .file2) | {ts,turn,event,file,file2,data}' "$S/session.jsonl"
```

Read full model output for a turn:

```bash
sed -n '1,240p' "$S/turns/turn_005_model.txt"
```

Read exact model response span:

```bash
row=$(jq -c 'select(.event=="model_response" and .turn==5)' "$S/session.jsonl" | head -1)
file=$(jq -r '.file // empty' <<<"$row")
off=$(jq -r '.data.model_offset // 0' <<<"$row")
len=$(jq -r '.data.model_bytes // empty' <<<"$row")
dd if="$S/$file" bs=1 skip="$off" count="$len" 2>/dev/null
```

Read exact stdout span by `output_id`:

```bash
OID="<output-id>"
row=$(jq -c --arg id "$OID" 'select(.event=="agent_output" and (.data.output_id // "") == $id)' "$S/session.jsonl")
file=$(jq -r '.file // empty' <<<"$row")
off=$(jq -r '.data.stdout_offset // 0' <<<"$row")
len=$(jq -r '.data.stdout_bytes // empty' <<<"$row")
dd if="$S/$file" bs=1 skip="$off" count="$len" 2>/dev/null
```

Read exact stderr span:

```bash
file=$(jq -r '.file2 // empty' <<<"$row")
off=$(jq -r '.data.stderr_offset // 0' <<<"$row")
len=$(jq -r '.data.stderr_bytes // empty' <<<"$row")
dd if="$S/$file" bs=1 skip="$off" count="$len" 2>/dev/null
```

Show model input files:

```bash
jq -r 'select(.event=="messages_input") | [.turn, .file] | @tsv' "$S/session.jsonl"
jq . "$S/turns/turn_005_messages.json" | less
```

Show runtime requests:

```bash
jq -r 'select(.event=="agent_input") | [.turn, .file, (.data.functions // [] | join(","))] | @tsv' "$S/session.jsonl"
jq . "$S/turns/turn_005_agent_in.json" | less
```

## Find Failures, Approvals, Tool Calls, and Context Pressure

Approvals and human questions:

```bash
jq -c 'select((.event // "") | IN("approval","approval_resolved","auto_approved","human_question","human_response_sent"))' "$S/session.jsonl"
```

Steering and follow-up delivery:

```bash
jq -c 'select((.event // "") | startswith("steer_"))' "$S/session.jsonl"
```

Tool starts and outputs:

```bash
jq -c 'select((.event // "") | IN("agent_started","agent_input","agent_output")) | {ts,turn,event,level,message,data,file,file2}' "$S/session.jsonl"
```

Context snapshots and token pressure:

```bash
jq -r 'select(.event=="context_snapshot") | [.ts // "", (.data.source // ""), (.data.session_id // ""), (.data.token_count // ""), (.data.context_window // ""), (.data.hard_context_window // ""), (.data.item_count // ""), (.file // "")] | @tsv' \
  "$S/session.jsonl"
```

Open the latest context snapshot:

```bash
latest=$(jq -r 'select(.event=="context_snapshot" and .file) | .file' "$S/session.jsonl" | tail -1)
jq . "$S/$latest" | less
```

Reasoning:

```bash
jq -c 'select(.event=="reasoning") | {ts,turn,message,data,file}' "$S/session.jsonl"
find "$S/turns" -type f -name '*reasoning.txt' -print -exec sed -n '1,160p' {} \;
```

Native command full logs:

```bash
find "$S" \( -path "$S/turns" -o -path "$S/frames" -o -path "$S/recordings" -o -path "$S/file_snapshots" -o -path "$S/context_rewinds" \) -prune -o \
  -type f \( -name '*_stdout.log' -o -name '*_stderr.log' \) -print -exec tail -n 80 {} \;
```

## External-Agent Mappings and Backend Logs

Read wrapper identity rows:

```bash
jq -c 'select(.event=="session_identity")' "$S/session.jsonl"
jq . "$S/session_agent_config.json" 2>/dev/null
```

Find wrapper records globally:

```bash
jq -r '.wrappers[]? | [.source, .backend_session_id, .intendant_session_id, .log_path, (.project_root // ""), (.updated_at_secs // "")] | @tsv' \
  "$HOME/.intendant/external_wrapper_index.json" 2>/dev/null | sort
```

Find a Codex native rollout by backend id:

```bash
BACKEND="<backend-session-id>"
CODEX_HOME_FROM_LOG=$(jq -r '.codex_home // empty' "$S/session_agent_config.json" 2>/dev/null)
CH="${CODEX_HOME_FROM_LOG:-${CODEX_HOME:-$HOME/.codex}}"
find "$CH/sessions" "$CH/archived_sessions" -name '*.jsonl' -type f -print0 2>/dev/null |
while IFS= read -r -d '' f; do
  rg -l "\"id\"\\s*:\\s*\"$BACKEND\"" "$f"
done
```

Inspect Codex token-count rows:

```bash
R="<codex-rollout.jsonl>"
jq -c 'select(.type=="event_msg" and .payload.type=="token_count") | {timestamp,info:.payload.info}' "$R"
```

Inspect Codex rollback markers:

```bash
jq -c 'select(.type=="event_msg" and .payload.type=="thread_rolled_back") | {timestamp,payload}' "$R"
```

Inspect Codex function calls and outputs:

```bash
jq -c 'select(.type=="response_item" and (.payload.type=="function_call" or .payload.type=="function_call_output")) | {timestamp,payload}' "$R"
```

Claude Code native file lookup:

```bash
find "$HOME/.claude/projects" -name '<session-id>.jsonl' -type f -print 2>/dev/null
```

## Display, Recordings, File History, and Managed Context

Frames:

```bash
jq -r '[.timestamp // "", .stream // "", .frame_id // "", (.sent_to_live|tostring), (.hq_resolution // ""), (.note // "")] | @tsv' \
  "$S/frames/frames.jsonl" 2>/dev/null
find "$S/frames" -type f -name '*.jpg' | sort | tail
```

Recordings:

```bash
find "$S/recordings" -type f -name manifest.json -print -exec jq . {} \; 2>/dev/null
find "$S/recordings" -type f -name segments.csv -print -exec tail -n 20 {} \; 2>/dev/null
find "$S/recordings" -type f -name ffmpeg.log -print -exec tail -n 80 {} \; 2>/dev/null
```

File snapshot history:

```bash
jq '{current_head_id, rounds:[.rounds[]? | {id,parent_id,summary,timestamp_unix,files_changed,turn_count,native_message_count}]}' \
  "$S/file_snapshots/history.json" 2>/dev/null
```

Find rounds touching a path:

```bash
PATH_PART="src/bin/caller/session_log/mod.rs"
jq -c --arg p "$PATH_PART" '.rounds[]? | select((.files_changed // []) | index($p)) | {id,summary,files_changed}' \
  "$S/file_snapshots/history.json"
```

Context rewinds:

```bash
find "$S/context_rewinds" -name '*.json' ! -name '*-source-rollout.jsonl' -type f -print -exec jq '{record_id,created_at,session_id,thread_id,item_id,position,reason,used_tokens_at_rewind,context_window_at_rewind,pressure_band_at_rewind,surgical}' {} \; 2>/dev/null
```

Fission ledger:

```bash
jq '.groups[]? | {group_id,parent_session_id,anchor_item_id,tool,objective,canonical_session_id,branches:[.branches[]? | {session_id,backend_session_id,status,task,worktree_path,raw_log}]}' \
  "$S/fission_ledger.json" 2>/dev/null
```

Peer events:

```bash
jq -c '{seq,peer,payload}' "$S/peers.jsonl" 2>/dev/null | tail -80
```

## Dashboard Deep-Search Behavior

Dashboard deep search:

- Builds a session list from Intendant logs plus Codex/Claude Code native logs.
- Supports source filters `all`, `external`, `intendant`, `codex`, and `claude-code`.
- Supports modes `all_keywords`, `exact_phrase`, `any_keyword_session`, and `user_message_all_keywords`.
- Searches full log files and recursively collects every JSON string field.
- Searches beyond the recent-session display window.
- Can prefilter by project directory.
- Filters parent-log references to deleted external sessions using `~/.intendant/deleted_external_sessions.json`.

Manual equivalent for exact phrase over Intendant sessions:

```bash
phrase="needle phrase"
find "$HOME/.intendant/logs" -type f -name session.jsonl -print 2>/dev/null |
while IFS= read -r f; do
  rg -qi --fixed-strings "$phrase" "$f" && printf '%s\n' "$(dirname "$f")"
done
```

Manual equivalent for all keywords in one row:

```bash
terms='["alpha","beta"]'
jq -c --argjson terms "$terms" 'def all_strings: [.. | strings] | join("\n") | ascii_downcase; select(all_strings as $s | all($terms[]; $s | contains(.)))' \
  "$S/session.jsonl"
```
