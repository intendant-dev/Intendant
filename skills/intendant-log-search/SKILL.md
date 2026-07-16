---
name: intendant-log-search
description: Use when investigating Intendant run history, session logs, dashboard replay/search behavior, external-agent wrapper logs, Codex/Claude backend logs, context rewind/fission sidecars, display frames, recordings, or task artifacts under ~/.intendant/logs. Teaches how to find sessions, search session.jsonl and sidecars, follow turn-file byte spans, map wrapper sessions to backend ids, and interpret what each log artifact provides.
---

# Intendant Log Search

Use this skill to investigate what happened in an Intendant run without rediscovering the log layout from source. Treat the source as authoritative when this skill and the repo disagree; Intendant log behavior changes quickly.

## Quick Workflow

1. Locate the session.

```bash
find "$HOME/.intendant/logs" -type f -name session_meta.json -print 2>/dev/null |
while IFS= read -r f; do
  jq -r '[.created_at // "", .session_id // (input_filename|split("/")[-2]), .status // "", .project_root // "", .task // ""] | @tsv' "$f"
done | sort -r | head -50
```

If you know a project:

```bash
find "$HOME/.intendant/logs" -type f -name session_meta.json -print 2>/dev/null |
while IFS= read -r f; do
  jq -r --arg p "$PWD" 'select((.project_root // "") == $p) | [.created_at // "", .session_id // (input_filename|split("/")[-2]), .status // "", .task // ""] | @tsv' "$f"
done | sort -r
```

Then set:

```bash
S="$HOME/.intendant/logs/<session-id>"
```

2. Read the metadata, event mix, and timeline before grepping sidecars.

```bash
jq . "$S/session_meta.json"
jq -r '.event // "?"' "$S/session.jsonl" | sort | uniq -c | sort -nr
jq -r '[.ts // "", (.turn // ""), .level // "", .event // "", .message // ""] | @tsv' "$S/session.jsonl" | less -S
```

3. Use `transcript.jsonl` for the human-readable conversation, but switch to `session.jsonl` and `turns/` when you need exact inputs, model output, reasoning, tool output, approvals, context snapshots, or replay details.

```bash
jq -r '[.ts // "", .role // "", .text // ""] | @tsv' "$S/transcript.jsonl" 2>/dev/null | less -S
```

4. Follow file references. `session.jsonl` stores many large payloads as relative files in `file` and `file2`. Some rows also store byte spans in `data.*_offset` and `data.*_bytes`.

```bash
jq -c 'select(.event=="agent_output") | {ts,turn,file,file2,data}' "$S/session.jsonl" | tail
```

Read an exact stdout span:

```bash
row=$(jq -c 'select(.event=="agent_output" and (.data.output_id=="<output-id>"))' "$S/session.jsonl")
file=$(jq -r '.file // empty' <<<"$row")
off=$(jq -r '.data.stdout_offset // 0' <<<"$row")
len=$(jq -r '.data.stdout_bytes // empty' <<<"$row")
dd if="$S/$file" bs=1 skip="$off" count="$len" 2>/dev/null
```

For stderr spans, use `file2`, `data.stderr_offset`, and `data.stderr_bytes`. For model response spans, use `file`, `data.model_offset`, and `data.model_bytes`.

5. Decide which deeper reference to load.

- Need the directory layout and sidecar schemas: read `references/artifact-map.md`.
- Need event meanings and fields: read `references/event-taxonomy.md`.
- Need concrete `jq`/`rg` searches: read `references/query-recipes.md`.

## Search Targets

- `session.jsonl`: canonical Intendant session timeline, replay events, file references, warnings/errors, approvals, context snapshots, external-agent normalized output.
- `transcript.jsonl`: compact conversation/speech transcript. It is useful for orientation, not a complete audit record.
- `turns/`: full or append-only per-turn payloads: model inputs, model responses, reasoning, runtime requests, stdout/stderr, context snapshots.
- `daemon.log`: controller stdout/stderr, panics, tracing, backend stderr forwarding, and operational diagnostics.
- Runtime side files: `<nonce>_stdout.log`, `<nonce>_stderr.log`, `screenshot_<nonce>.png`, temporary human-question files.
- Visual artifacts: `frames/frames.jsonl`, `frames/*.jpg`, `recordings/<stream>/manifest.json`, `segments.csv`, `seg_*.mp4`, `ffmpeg.log`.
- Managed-context sidecars: `context_rewinds/*.json`, `context_rewinds/*-source-rollout.jsonl`, `fission_ledger.json`, `model-request-traces/`.
- External-agent indexes and native logs: `~/.intendant/external_wrapper_index.json`, `session_agent_config.json`, Codex rollout JSONL, Claude Code JSONL.

## External Sessions

Intendant wrapper logs normalize Codex and Claude Code activity into the Intendant `session.jsonl` shape. Use `session_identity` rows or `~/.intendant/external_wrapper_index.json` to map:

- `intendant_session_id`: wrapper log directory under `~/.intendant/logs/`.
- `backend_session_id`: native Codex/Claude Code id.
- `source`: `codex` or `claude-code`.

For Codex, inspect `session_agent_config.json` for `codex_home`; otherwise use `$CODEX_HOME` or `~/.codex`. Codex native logs are rollout JSONL files under `sessions/` and `archived_sessions/`, identified by a `session_meta` row whose `payload.id` equals the backend id.

## Cautions

- Do not assume `session.jsonl` includes full command output. Native runtime `execAsAgent` writes full `<nonce>_stdout.log` and `<nonce>_stderr.log` files; the controller may only receive and persist output tails.
- Do not assume every dashboard event has a one-to-one persisted event. Some durable events are logged as generic `info` or `warn`; some high-frequency/internal events are skipped during replay.
- Do not search only `session_meta.json` or summaries. Dashboard deep search scans full log files and full JSON string fields because useful evidence often lives in `message`, `data`, or backend-native JSONL.
- For exact context, prefer `context_snapshot` files over dashboard previews. Large raw context can be summarized or omitted from replay. Only the LATEST sidecar per (source, session id) stream is retained on disk (rotation; `INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL=1` archives all) — historical `context_snapshot` rows keep their metadata but their files are gone, and `exact_replay_available` on replayed rows reflects disk truth. Selecting the newest `context_snapshot` row (`... | tail -1`) still resolves to a real file.
- `turns/turn_NNN_messages.json` is debug-only (`INTENDANT_LOG_MESSAGES_JSON=1`); expect it to be absent in normal sessions except where the provider produced no context snapshot (then it is written as the only exact input record).
