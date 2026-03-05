# Integrations

## Control Socket

When `--control-socket` is enabled, a Unix domain socket is created at `/tmp/intendant-<pid>.sock`.

- Outbound event broadcast is implemented.
- Inbound command handling is implemented for status, approval, denial, human input, autonomy change, quit, controller-restart workflow commands, and controller-loop intervention commands (in MCP mode).
- Socket server is opt-in via `--control-socket`.

### Inbound Commands (JSON-line)

```json
{"action": "status"}
{"action": "approve", "id": 123}
{"action": "deny", "id": 123}
{"action": "input", "text": "answer to askHuman"}
{"action": "set_autonomy", "level": "high"}
{"action": "schedule_controller_restart", "controller_id":"codex", "north_star_goal":"audit and improve", "restart_after":"turn_end"}
{"action": "controller_turn_complete", "restart_id":"<id>", "turn_complete_token":"<token>", "status":"ok", "handoff_summary":"..."}
{"action": "get_restart_status"}
{"action": "cancel_controller_restart", "restart_id":"<id>"}
{"action": "request_controller_loop_halt", "persistent": true}
{"action": "clear_controller_loop_halt"}
{"action": "intervene_controller_loop", "mode":"stop"}
{"action": "get_controller_loop_status"}
{"action": "query_detail", "scope": "diff"}
{"action": "query_detail", "scope": "file", "target": "src/main.rs"}
{"action": "recall_memory", "keywords": ["auth", "login"], "channel": "project_state"}
{"action": "usage"}
{"action": "quit"}
```

### Outbound Events (streamed to connected clients)

```json
{"event": "turn_started", "turn": 5, "budget_pct": 12.3}
{"event": "agent_output", "stdout": "...", "stderr": "..."}
{"event": "approval_required", "id": 123, "command": "rm -rf /tmp/test"}
{"event": "ask_human", "question": "Which database?"}
{"event": "task_complete", "reason": "done signal"}
{"event": "status", "turn": 3, "phase": "thinking", "autonomy": "medium", "session_id": "abc-123", "task": "fix tests"}
{"event": "usage", "main": {"provider": "openai", "model": "gpt-5", "tokens_used": 12000, "context_window": 128000, "usage_pct": 9.4}}
{"event": "usage_update", "main": {"provider": "openai", "model": "gpt-5", "tokens_used": 15000, "context_window": 128000, "usage_pct": 11.7}}
{"event": "command_result", "action": "get_restart_status", "ok": true, "message": "ok", "data": {...}}
```

- The `status` event now includes `session_id` and `task` fields.
- The `usage` event is a response to `{"action": "usage"}`, returning per-model token usage.
- The `usage_update` event is broadcast automatically after each agent turn, providing streaming token consumption updates. The `presence` field is included when the presence layer is active.

`command_result.ok` is `false` when a control action fails (for example, `schedule_controller_restart` with `restart_after="now"` and no executable restart action configured).

### Example Usage

```bash
echo '{"action":"status"}' | socat - UNIX:/tmp/intendant-$(pgrep intendant).sock
```

## Live Gateway

The `--live` flag starts a WebSocket server that enables voice and text control of Intendant from a browser. It supports both the Gemini Live API and OpenAI Realtime API via a provider abstraction. `--live` implies `--mcp`, so no initial task is required — the agent starts idle and accepts tasks dynamically.

### How It Works

```
Browser ──WebSocket──> Intendant live gateway (port 8765)
  │                              │
  │  (audio / text)              │ (ControlMsg JSON, same as control socket)
  v                              v
Model API (Gemini/OpenAI)  EventBus / broadcast channel
  │                              │
  │  (function calls)            │ (OutboundEvent JSON)
  v                              v
JS provider bridge ──────> Intendant agent loop
```

The browser connects directly to the model's realtime API for low-latency voice I/O, and to the Intendant gateway for control messages. A JS provider bridge translates model function calls (`submit_task`, `approve_action`, `check_status`, etc.) into Intendant `ControlMsg` JSON and injects Intendant events back into the model session for voice narration. A text input bar also allows typed messages.

The live gateway serves a `/config` endpoint that returns the configured provider and model as JSON, which the frontend uses to select the correct provider adapter.

### Running

```bash
# Start idle, waiting for tasks via live UI
./target/release/intendant --live

# Custom port
./target/release/intendant --live 9000
```

Open `http://<host>:8765/` on your phone or browser. On first visit, enter your API key (Gemini or OpenAI depending on configuration; stored in browser localStorage, never sent to Intendant).

### Provider Configuration

The live gateway auto-detects the provider from `presence.audio_model` in `intendant.toml`, or falls back to environment variable detection (`GEMINI_API_KEY` → Gemini, `OPENAI_API_KEY` → OpenAI).

### Requirements

- **Microphone access requires a secure context**: Use `localhost` (via SSH tunnel: `ssh -L 8765:localhost:8765 host`), or set browser flags for insecure origins.
- **API key**: Gemini (free tier from Google AI Studio) or OpenAI. The key is used browser-side only.

### Voice Identity

The live gateway speaks in first person as Intendant — "I'm running your tests now" rather than "The agent is running tests." Event narration from the agent loop is rewritten into first-person system messages before being injected into the model session.

### Supported Voice Commands

| Voice command | Maps to |
|---|---|
| "List files in /tmp" | `submit_task({description: "list files in /tmp"})` |
| "What's the status?" | `check_status()` |
| "Approve that" | `approve_action({id: N})` |
| "No, skip it" | `skip_action({id: N})` |
| "Set autonomy to full" | `set_autonomy({level: "full"})` |
| "The answer is PostgreSQL" | `respond_to_question({text: "PostgreSQL"})` |
| "Show me the diff" | `query_detail({scope: "diff"})` |
| "What do you know about auth?" | `recall_memory({keywords: ["auth"]})` |
