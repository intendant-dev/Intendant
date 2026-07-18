# Runtime Protocol

`intendant-runtime` is the command-execution half of the two-process split. It reads a
**single JSON object** from stdin, executes its commands **sequentially**, writes
one result line per command to stdout, and exits. It never holds API keys and
never talks to a model — it is a dumb, auditable command executor that the
controller (`intendant`) drives over pipes.

```
                 stdin: one AgentInput JSON
 intendant ───────────────────────────────────► intendant-runtime
 (controller, holds keys)                        (command executor)
           ◄───────────────────────────────────
                 stdout: one JSON result line per command
```

The controller side of this boundary is `agent_runner.rs::run_agent()`: it locates
`intendant-runtime` next to its own binary, spawns it with stdin/stdout/stderr
piped, writes the JSON, closes stdin, and reads the bounded output back. The
runtime is short-lived — one invocation per batch of tool calls. PTY sessions are
the one stateful exception (see [`execPty`](#execpty)) and they live only for the
duration of a single runtime process.

## Basic Usage

```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' \
  | ./target/release/intendant-runtime
```

Each command produces one stdout line wrapped as:

```json
{"type":"result","nonce":1,"data":"<stringified per-command JSON>"}
```

`data` is itself a JSON string — the per-command result (exit code, output tail,
etc.) serialized into a string. The controller parses the outer envelope, matches
on `nonce`, then parses `data`.

More examples:

```bash
# Inspect a path
echo '{"commands":[{"function":"inspectPath","nonce":1,"path":"/etc/hosts"}]}' | ./target/release/intendant-runtime

# Edit a file (structured, no shell)
echo '{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/test.txt","operation":"write","content":"hello"}]}' | ./target/release/intendant-runtime

# Fetch a web page as text
echo '{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}' | ./target/release/intendant-runtime

# Stateful commands in one persistent PTY (same process only)
echo '{"commands":[{"function":"execPty","nonce":1,"command":"cd /tmp"},{"function":"execPty","nonce":2,"command":"pwd"}]}' | ./target/release/intendant-runtime
```

## The `AgentInput` Shape

The entire stdin payload deserializes into `AgentInput` (`src/models.rs`), which
has exactly one field:

```jsonc
{
  "commands": [ /* array of Command objects, executed in order */ ]
}
```

There is no top-level `context` field at the runtime layer — conversation/context
management is handled entirely on the controller side (see
[Caller-Handled Functions](#caller-handled-functions)). stdin is bounded to 64 MB
(`MAX_INPUT_BYTES`); a parse failure prints a UTF-8-safe preview capped at 2,048
bytes (plus the omitted-byte count) to stderr and exits non-zero. It never dumps
an arbitrarily large malformed payload.

### The `Command` object

Every command shares one flat struct (`models::Command`). Only `function` and
`nonce` are always required; the rest are `Option`al and interpreted per
function:

| Field | Type | Used by |
|-------|------|---------|
| `function` | string | all — selects the handler |
| `nonce` | u64 | all — correlation id, echoed in the result |
| `command` | string | `execAsAgent`, `execPty` |
| `display` | i32 | `execAsAgent`, `captureScreen` |
| `timeout_ms` | u64 | `execAsAgent` (default 120000) |
| `wait_for_port` | u16 | `execAsAgent` (0 = no wait) |
| `path` | string | `inspectPath` |
| `file_path` | string | `editFile` / `writeFile` |
| `operation` | string | `editFile` (`write`/`append`/`replace`/`insert_at`/`replace_lines`) |
| `content` | string | `editFile` write/append/replace/insert/replace_lines |
| `match_content` | string | `editFile` `replace` |
| `line_number` | usize | `editFile` `insert_at` / `replace_lines` |
| `end_line` | usize | `editFile` `replace_lines` |
| `url` | string | `browse` |
| `question` | string | `askHuman` |
| `shell_id` | string | `execPty` (defaults to `default`) |

## Sequential, Blocking Execution

`Agent::process_input()` (`src/agent.rs`) iterates `commands` **in order**, and
each command **blocks until it finishes** before the next starts. There is no
concurrency within a batch. `execAsAgent` waits for the child process to exit (or
its timeout); `askHuman` polls indefinitely for a human response. This determinism
is deliberate — the controller can reason about ordering and the human-oversight
layer can gate each action.

Errors from `inspectPath`, `editFile`/`writeFile`, `browse`, `askHuman`, and
`execPty` are captured as `data: "Error: <message>"` for that nonce and the
batch continues. `execAsAgent` and `captureScreen` errors abort the batch, as
does an **unknown `function`**.

## Functions

### Runtime Functions

These are the ~10 functions `intendant-runtime` actually implements:

| Function | Description | Key fields |
|----------|-------------|------------|
| `execAsAgent` | Run a command via the platform shell (`bash -c` on Unix, `cmd /C` on Windows); blocks until exit; returns pid, exit code, and 10 KB tails of stdout/stderr | `command`, `display`, `timeout_ms`, `wait_for_port` |
| `captureScreen` | Screenshot a display (macOS `screencapture`; X11 `import`) to a PNG in the log dir | `display` |
| `inspectPath` | Filesystem metadata (type, size, mtime; plus mode/uid/gid on Unix) | `path` |
| `editFile` | Structured file editing without a shell | `file_path`, `operation`, `content`, `match_content`, `line_number`, `end_line` |
| `writeFile` | Back-compat alias — rewritten to `editFile` with `operation:"write"` if unset | `file_path`, `content` |
| `browse` | HTTP GET, HTML→text via `html2text` (50 KB cap, 15 s timeout, ≤5 redirects) | `url` |
| `askHuman` | Write a question to the log dir and **poll indefinitely** for a response file | `question` |
| `execPty` | Run a command in a persistent PTY session for the life of this process | `command`, `shell_id` |

Path-taking functions (`inspectPath`, `editFile`) run through `validate_path()`,
which blocks `..` traversal and a fixed set of sensitive locations
(`/etc/shadow`, `/etc/gshadow`, `/proc`, `/sys`, `/dev`, and any `.ssh` / `.gnupg`
component), checking both the raw and canonicalized forms.

Command strings (`execAsAgent`, `execPty`) do **not** pass through
`validate_path()` — no string inspection could do so honestly (shell
expansion, variables, indirection). The secret portion of that policy is
instead enforced at the OS level where the platform can express it: on
macOS the controller always wraps the runtime in a Seatbelt profile
denying `~/.ssh`, `~/.gnupg`, the intendant config home
(`dirs::config_dir()/intendant`, which holds the global `.env` fallback),
and every `.env` on the controller's key search path (launch cwd +
ancestors, covering the project root) to the whole process tree (composed
into the write-restriction profile when the write sandbox is enabled), so
shell commands hit `Operation not permitted` on those paths. On Linux,
Landlock is allowlist-only and cannot subtract read access from a granted
tree — there, command strings are bounded by autonomy/approvals plus the
write sandbox (`INTENDANT_SANDBOX_WRITE_PATHS`), the secret-directory
denylist genuinely covers only the structured tools, and project/config
`.env` files remain readable to sandboxed commands (moving keys out of
agent-readable files — the credential-custody migration — is the tracked
fix). Windows mirrors the Linux posture with a `WRITE_RESTRICTED` token
(`win_sandbox.rs`): reads stay open (so, like Linux, the secret and
`.env` coverage applies only to the structured tools) while writes are
confined to the granted roots.

### `editFile` Operations

| Operation | Description | Required fields |
|-----------|-------------|-----------------|
| `write` | Create or overwrite the file (creates parent dirs) | `file_path`, `content` |
| `append` | Append to the end of the file | `file_path`, `content` |
| `replace` | Replace **every** occurrence of `match_content`; reports `replacements` count (fails gracefully if not found) | `file_path`, `match_content`, `content` |
| `insert_at` | Insert `content` as a line at `line_number` (clamped to file length) | `file_path`, `line_number`, `content` |
| `replace_lines` | Replace lines `[line_number, end_line)` with `content` | `file_path`, `line_number`, `end_line`, `content` |

`insert_at` and `replace_lines` preserve a trailing newline when the original had
one. `replace_lines` errors if `end_line < line_number`.

### `execAsAgent` details

- **Shell**: `crate::utils::agent_shell_command()` picks `bash -c <cmd>` on Unix
  and `cmd.exe /C <cmd>` on Windows; the whole command is one argument so the
  shell does word-splitting. Exit semantics are identical across platforms.
- **stdout/stderr** are streamed to `<log_dir>/<nonce>_stdout.log` /
  `_stderr.log`; the result carries only the **last 10 KB** of each
  (`LOG_TAIL_BYTES`).
- **Credentials are stripped at two boundaries.** The controller removes
  canonical provider names, inherited `*_API_KEY` / `*_API_TOKEN` names, and
  ambient host-credential names (agent sockets, forge/cloud/registry tokens,
  credential-store pointers) before spawning the runtime. The `INTENDANT_*`
  namespace is reserved for controller→runtime control and is exempt. Both
  `execAsAgent` and `execPty` independently repeat the provider and ambient
  scrub before starting their model-driven shell; they do not merely trust the
  runtime's inherited environment. See
  [Configuration → Child-process environment](./configuration.md#child-process-environment)
  for the current passthrough limitation.
- **Display gating**: `DISPLAY` is set to the chosen display. Access to the user's
  session display (`:0` or below) is refused unless
  `INTENDANT_USER_DISPLAY_GRANTED` is set; otherwise a virtual display is used.
  The controller derives that variable onto the runtime child's environment at
  spawn time from the autonomy guard's `user_display_granted` (the grant's
  single source of truth) — it is never set on the controller's own process.
- **Exit codes**: real exit code on completion; `-3` on timeout (process killed),
  `-2` on `wait_for_port` timeout, `-1` on spawn/wait failure.

### `execPty`

Lazily opens a PTY (`bash --norc --noprofile` on Unix; PowerShell with a cmd.exe
fallback on Windows) keyed by `shell_id`, so state (cwd, shell vars) persists
**across commands within the same runtime invocation**. Output is bracketed with
`__PTY_START_<nonce>__` / `__PTY_END_<nonce>__` markers, drained by a background
reader thread, ANSI-stripped, and trimmed of the echoed command and prompt lines.
A 30 s per-command deadline guarantees a quiet shell can't wedge the loop. (The
reader thread also answers ConPTY's startup cursor-position query so Windows
shells don't hang at launch.) The result carries at most the last 10 KiB and a
`truncated` boolean. When output is larger, the runtime attempts to preserve the
full transcript as `<nonce>_pty.log`; the returned truncation marker says where
it was written or reports that preservation failed.

### `askHuman`

Writes the question to `<log_dir>/human_question`, echoes it to stderr, and polls
every 500 ms for `<log_dir>/human_response`, **with no timeout** — it waits as long
as the human is away. The controller correspondingly drops its hard timeout for any
batch containing an `askHuman` (`agent_runner.rs::has_ask_human`). Both files are
removed once a response arrives.

### Caller-Handled Functions

These tool names appear in the controller's tool catalog but are **intercepted by
the controller and never sent to the runtime**. If they ever reached
`process_input()` they would hit the unknown-function error.

| Function | Handled by | Description |
|----------|-----------|-------------|
| `manage_context` | controller loop | Apply context directives (drop/summarize turns) to the conversation |
| `signal_done` | controller loop | Signal task completion to the agent loop |
| `invoke_skill` | controller loop | Run a packaged skill |
| `spawn_live_audio` | controller loop | Start a voice/phone session (untrusted; see [Computer Use & Live Audio](./computer-use-and-audio.md)) |
| `mcp__<server>_<tool>` | MCP client | Tools registered from external MCP servers ([MCP Server](./mcp-server.md)) |

### Native Tool Names

When the provider uses native tool calling (the default), the model sees
snake_case tool names that map onto the runtime functions:

| Native name | Runtime function |
|-------------|------------------|
| `exec_command` | `execAsAgent` |
| `capture_screen` | `captureScreen` |
| `inspect_path` | `inspectPath` |
| `edit_file` | `editFile` |
| `browse_url` | `browse` |
| `ask_human` | `askHuman` |
| `exec_pty` | `execPty` |
| `manage_context`, `signal_done`, `invoke_skill`, `spawn_live_audio` | *(caller-handled)* |

## Nonce Variables

Inside `command` strings, `$NONCE[id]` is substituted with the PID of the process
launched by command `id` earlier in the same batch. For example
`kill -9 $NONCE[10]` kills the process started by nonce 10. This is a regex
substitution in `replace_nonce_refs()`, resolved against the runtime's per-process
PID table.

## Logging Directories

The runtime resolves its working/log directory in `resolve_log_dir()`:

1. `INTENDANT_LOG_DIR` if set by the controller (created if missing) — the normal
   case; the controller passes the session log dir here.
2. Otherwise a fresh timestamped dir under
   `$INTENDANT_HOME/logs/<YYYYMMDD_HHMMSS>` when `INTENDANT_HOME` is non-empty,
   or `$HOME/.intendant/logs/<YYYYMMDD_HHMMSS>` by default.

This directory holds per-command stdout/stderr and over-cap PTY logs, screenshots
(`screenshot_<nonce>.png`), the `askHuman` question/response files, and (on
Linux/X11) the merged `session.Xauthority` cookie file.

On the controller side, stdout and stderr from the runtime process are each
buffer-capped at 64 MiB while excess bytes are still drained to avoid pipe
deadlock. An honest truncation note is appended when bytes were discarded. If
the 120-second batch hard timeout fires, the controller kills the runtime but
salvages every complete JSONL protocol line already flushed by commands that
finished; an incomplete trailing line is discarded.

## Filesystem Sandboxing

The runtime's write boundary is driven by the `INTENDANT_SANDBOX_WRITE_PATHS`
environment variable (a platform path-list: `:`-separated on Unix,
`;`-separated on Windows; empty/unset → no sandbox). The platform default is
**on for macOS/Linux and off for Windows**: the controller
(`configure_sandbox_env` in the caller's `main.rs`) resolves `--sandbox`
(force on), `--no-sandbox` (force off), then `[sandbox] enabled` in
intendant.toml, then that platform default. The default write set is the
project root (omitted for a projectless daemon), the scratch dirs (the live
platform temp dir plus `/tmp` on Unix), the session log dir, the daemon
state root's `logs/` subtree, and — on Unix — the toolchain caches a standard
build writes even when warm (cargo home, rustup home, and the user cache dir;
the state root itself is deliberately excluded because it holds authority;
see `toolchain_cache_write_paths` in `sandbox.rs`); `[sandbox]
extra_write_paths` extends it. When enabled, each platform enforces the same
posture — whole filesystem readable, only the listed paths writable — with
its native primitive:

- **Linux** — a Landlock ruleset applied in-process **before running any
  command** (`apply_sandbox_from_env` in `src/main.rs`, ABI v5). `/dev` is
  always write-granted (tty/PTY nodes, `/dev/null` — DAC still applies),
  mirroring the macOS profile. Nonexistent paths are skipped. If the kernel
  does not enforce Landlock, the runtime **fails closed** and refuses to run
  unconfined — on a Landlock-less kernel the error names the explicit
  opt-outs (`--no-sandbox` / `[sandbox] enabled = false`); it never degrades
  silently.
- **macOS** — the controller wraps the runtime child in `sandbox-exec` with a
  generated Seatbelt profile (write-restriction composed with the always-on
  secret-directory denial; see `sandbox.rs`).
- **Windows (opt-in)** — the runtime re-execs itself under a `WRITE_RESTRICTED`
  restricted token before reading stdin (`win_sandbox.rs`): an access check
  then requires both the user's normal grants *and* a restricting-SID grant
  for every write, and the controller holds temporary ACL entries granting
  the `RESTRICTED` SID on exactly the allowed write roots (refcounted,
  journaled, crash-swept). The token also drops every privilege except
  `SeChangeNotifyPrivilege`, so backup/restore-intent opens from elevated
  parents cannot bypass the DACL. Failure to restrict is fail-closed: the
  runtime refuses to run unconfined. Enabling this path accepts a potentially
  expensive first stamp because Windows propagates each inheritable grant
  through the existing descendants of a write root synchronously.

This is the runtime's primary write-boundary; combined with the key-stripping
and path validation above, it bounds what an agent command can touch even
though it runs with the user's privileges.

## Knowledge System (removed)

The runtime-level knowledge store (`.intendant/memory.json` and its
key-value tools) was removed at the Memory-plane cutover.
Durable, machine-wide facts ride the daemon's Memory service
(`memory_propose`/`memory_search`/`memory_read`); orchestration state
rides the `workflow_checkpoint` coordination files. Leftover
`memory.json` files are inert: nothing reads, ingests, or deletes them.

## JSON Output Mode (controller, not runtime)

`--json` is a **controller** flag (headless JSONL stdio), not part of the runtime
protocol — but it is the closest thing to a machine-readable interface for the
whole system, so it is documented here. Each stdout line is a JSON object with
`type` and `data`. Event types include `turn_started`, `model_response`,
`model_response_delta`, `agent_output`, `done`, `error`, `approval_required`,
`human_question`, `budget_warning`, `round_complete`, and `context_management`.

In `--json` mode the controller's stdin accepts both plain-text follow-ups and
`ControlMsg` JSON (the same vocabulary as the Unix control socket):

```json
{"action":"approve","id":123}
{"action":"deny","id":123}
{"action":"skip","id":123}
{"action":"approve_all","id":123}
{"action":"input","text":"answer to askHuman"}
{"action":"follow_up","text":"continue with this"}
```

Lines that don't start with `{` or don't parse as a `ControlMsg` are treated as
follow-up text, making `--json` fully interactive — approvals, `askHuman`, and
multi-round conversations all work without a dashboard or socket.
