# Agent

A Rust runtime that executes commands on behalf of an AI agent, plus an AI caller that drives the agent via the OpenAI or Anthropic API. The runtime manages process lifecycles via shared memory (SHM), streams status updates, and persists logs across binary restarts. The caller includes persistent project memory, active context management, and multi-provider support.

## Architecture

```
stdin (JSON) --> Agent --> spawns bash commands
                  |
                  +--> /dev/shm/agent_processes  (process state, survives restarts)
                  +--> /dev/shm/agent_session     (log directory path, survives restarts)
                  +--> /var/log/agent/<timestamp>/ (stdout/stderr logs per nonce)
                  |
                  +--> StatusMonitor --> stdout (status lines)

Caller --> detects project root (git) --> loads memory
  |
  +--> selects provider (OpenAI / Anthropic)
  +--> injects memory into conversation
  +--> main loop: model -> extract JSON -> apply context directives -> agent -> repeat
```

- **Shared Memory (`/dev/shm/agent_processes`):** Fixed-size array of `ProcessInfo` structs (1024 slots). Each slot stores nonce, PID, status, exit code, and timestamp. Survives binary restarts since it lives on tmpfs.
- **Session File (`/dev/shm/agent_session`):** Stores the log directory path so consecutive runs reuse the same directory.
- **Log Directory (`/var/log/agent/<timestamp>/`):** Per-nonce stdout and stderr log files. Created once per session.
- **Status Monitor:** Background task that polls SHM for status changes and writes update lines to stdout.

## Building

```bash
cargo build --release
```

Two binaries are produced:
- `./target/release/agent` — the command runtime
- `./target/release/caller` — the AI caller

## Usage

The agent reads a single JSON object from stdin and writes status lines to stdout.

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/agent
```

Output:

```
1r0        # nonce 1, running, exit code 0
1c0        # nonce 1, completed, exit code 0
```

Retrieve results in a subsequent run (returns JSON with `content`, `total_size`, `offset`, `bytes_read`):

```bash
echo '{"commands":[{"function":"fetchStatus","nonce":1,"status_type":"stdout"}]}' \
  | ./target/release/agent
```

Read only the last 1024 bytes of a log:

```bash
echo '{"commands":[{"function":"fetchStatus","nonce":1,"status_type":"stdout","limit":1024}]}' \
  | ./target/release/agent
```

Inspect a file path:

```bash
echo '{"commands":[{"function":"inspectPath","nonce":1,"path":"/etc/hosts"}]}' \
  | ./target/release/agent
```

Edit a file:

```bash
echo '{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/test.txt","operation":"write","content":"hello"}]}' \
  | ./target/release/agent
```

Fetch a web page as text:

```bash
echo '{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}' \
  | ./target/release/agent
```

Run stateful commands in a persistent PTY:

```bash
echo '{"commands":[{"function":"execPty","nonce":1,"command":"cd /tmp"},{"function":"execPty","nonce":2,"command":"pwd"}]}' \
  | ./target/release/agent
```

Store and recall memory:

```bash
echo '{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"db-config","memory_summary":"PostgreSQL on port 5432","memory_file":"/path/to/.agent/memory.json"}]}' \
  | ./target/release/agent

echo '{"commands":[{"function":"recallMemory","nonce":1,"memory_query":"database","memory_file":"/path/to/.agent/memory.json"}]}' \
  | ./target/release/agent
```

## Protocol

### Functions

| Function | Description | Key Fields |
|----------|-------------|------------|
| `execAsAgent` | Run a bash command in the background | `command`, `display`, `depending_nonce`, `wait`, `expected_status`, `wait_for_port` |
| `captureScreen` | Screenshot a display via ImageMagick | `display` |
| `fetchStatus` | Read process state/logs (JSON with offset/limit) | `status_type` (`status`, `stdout`, `stderr`, `exit_code`), `offset`, `limit` |
| `inspectPath` | Inspect filesystem path metadata | `path` |
| `editFile` | Structured file editing without shell commands | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `browse` | Fetch URL and convert HTML to text | `url` |
| `askHuman` | Ask the operator a question and wait for response | `question` |
| `execPty` | Run command in a persistent PTY session | `command`, `shell_id` |
| `storeMemory` | Store a key-value memory entry for the project | `memory_key`, `memory_summary`, `memory_file` |
| `recallMemory` | Search project memory by keyword | `memory_query`, `memory_file` |

### Status Codes

| Code | Meaning |
|------|---------|
| `r` | Running |
| `c` | Completed |
| `f` | Failed (could not start) |
| `s` | Skipped (dependency not met) |
| `w` | Waiting (on dependency) |

Status lines are formatted as `[nonce][status_char][exit_code]`, e.g. `42c0` means nonce 42 completed with exit code 0.

### Dependencies

Commands can be chained using `depending_nonce`, `wait`, and `expected_status`. When `wait` is `true`, the dependent command blocks until its dependency finishes. When `false`, it is skipped immediately if the dependency is not yet done.

### Nonce Variables

Use `$NONCE[id]` in command strings to reference the PID of a previously launched nonce. For example, `kill -9 $NONCE[10]` kills the process started by nonce 10.

### Context Management

The model can include a `context` field alongside `commands` to manage conversation history:

```json
{
  "commands": [...],
  "context": {
    "drop_turns": [3, 4, 5],
    "summarize": { "turns": [7, 8, 9, 10], "summary": "Set up nginx with reverse proxy" }
  }
}
```

- **`drop_turns`**: Remove messages at given indices (system prompt and last 2 messages are protected).
- **`summarize`**: Replace a range of messages with a single summary.
- Context-only turns (empty commands) are supported for pruning without executing anything.

## Memory System

Project memory persists key-value entries across sessions in `<project>/.agent/memory.json`.

- **`storeMemory`**: Creates or updates an entry with a key and summary.
- **`recallMemory`**: Searches entries by keyword, returns results ranked by relevance.
- Memory is loaded and injected into the conversation at session start.
- Can be disabled in `agent.toml`:

```toml
[memory]
enabled = false  # default: true
```

## Testing

```bash
cargo test
```

161 tests cover both binaries:

- **Agent binary:** models serialization, status formatting, error types, shared memory operations, nonce replacement, path inspection, status fetching, dependency checking, command processing, file editing, browsing, port waiting, human interaction, PTY sessions, memory storage, and memory recall.
- **Caller binary:** JSON extraction, conversation management, context directives (drop/summarize), error types, project detection, config parsing, memory loading/formatting, and provider selection.

## Session Management

State persists across binary restarts via `/dev/shm/`:

- **Process state** is stored in `/dev/shm/agent_processes` — the process map is rebuilt from SHM on each startup.
- **Log directory** is stored in `/dev/shm/agent_session` — subsequent runs reuse the same log directory.

To reset all state (start a fresh session):

```bash
rm -f /dev/shm/agent_processes /dev/shm/agent_session
```

## AI Caller

The caller binary detects the project, loads memory, sends the task to an AI model, and feeds the model's JSON output to the agent binary in a loop.

### Setup

Create a `.env` file (or export the variables):

```bash
# OpenAI
OPENAI_API_KEY=sk-...

# Or Anthropic
ANTHROPIC_API_KEY=sk-ant-...

# If both are set, choose one:
PROVIDER=openai          # or "anthropic"

MODEL_NAME=gpt-4o        # optional, provider-specific default used if omitted
```

### Running

```bash
# With a task as CLI argument
./target/release/caller "List the files in /tmp"

# Interactive mode (prompts for task on stdin)
./target/release/caller
```

### How it works

1. Loads `.env` and selects the API provider (OpenAI or Anthropic)
2. Detects the project root (via `git rev-parse --show-toplevel`, falls back to cwd)
3. Reads `SysPrompt.md` as the system message
4. Loads memory from `<project>/.agent/memory.json`, injects into conversation
5. Sends the task to the chat API
6. Extracts JSON from the model's response (handles code fences and bare JSON)
7. Applies context directives (`drop_turns`, `summarize`) to the conversation
8. Injects project context (`memory_file`) into relevant commands
9. Pipes the JSON to the agent binary, reads stdout/stderr with idle timeout (3s) and hard timeout (30s)
10. Feeds the agent output back as the next user message
11. Repeats until the model responds with no JSON (task complete) or 50 turns are reached

## Environment

- **OS:** Debian 12+
- **Runtime:** Tokio async
- **Display:** DISPLAY is automatically set to `:1` (configurable via `display` field) for GUI commands
- **Permissions:** Runs as unprivileged user with passwordless sudo

### Caller Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` / `OPENAI` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` / `ANTHROPIC` | — | Anthropic API key |
| `PROVIDER` | auto-detect | `"openai"` or `"anthropic"` (used when both keys are set) |
| `MODEL_NAME` | `gpt-4o` / `claude-sonnet-4-5-20250929` | Model to use (default depends on provider) |
| `AGENT_IDLE_TIMEOUT` | `3` | Seconds to wait for agent output before assuming idle |
| `AGENT_HARD_TIMEOUT` | `30` | Maximum seconds to wait for agent output |

Increase timeouts when using `askHuman` (e.g., `AGENT_HARD_TIMEOUT=600`).

### Project Configuration

Create `agent.toml` in the project root:

```toml
[memory]
enabled = true  # default: true
```
