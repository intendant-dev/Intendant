<p align="center">
  <img src="static/icon-128.png" width="96" alt="Intendant" />
</p>

# Intendant

An autonomous AI agent operating environment written in Rust. Intendant gives AI agents a full desktop to work in — shell access, file editing, a graphical display they can see and control via computer use, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system. It can also supervise external coding agents (Codex, Gemini CLI, Claude Code) as managed backends and federate with peer machines. Provider-agnostic (OpenAI, Anthropic, Gemini), cross-platform (macOS, Linux, Windows), accessible through CLI, TUI, web dashboard, MCP, or voice.

## Architecture

```
                          ┌──────────────────────────────────────────┐
                          │           intendant (controller)         │
                          │                                          │
  Web Dashboard ◄─────────┤  presence ─── agent loop ────┐           │
  TUI / MCP     ◄─────────┤     │            │           │           │
  Voice         ◄─────────┤     │      ┌─────┴──────┐    │           │
                          │     │      │ sub-agents │    │           │
                          │     │      └────────────┘    │           │
                          └─────┼────────────────────────┼───────────┘
                                │                        │
                    ┌───────────┤                        │
                    │           │                        │
                    v           v                        v
              Voice APIs   Model APIs              intendant-runtime
           (Gemini Live,  (OpenAI/Anthropic/       (sandboxed command
            OAI Realtime)  Gemini + streaming)      execution, Landlock)
```

**Presence layer** — a separate AI that mediates between user and agent. Handles conversation, dispatches tasks, narrates events, manages approval gates. Runs as server-side text or browser-side voice (Gemini Live / OpenAI Realtime via WASM).

**WebRTC display pipeline** — agents see and interact with graphical displays through a custom WebRTC transport (built on rtc-rs): a shared encoder pool with a VP8 baseline plus on-demand hardware H264 (VideoToolbox on macOS, VA-API/x264 on Linux, Media Foundation on Windows), tile-based dirty-region streaming, bidirectional clipboard, multi-monitor, and peer-to-peer display sharing across federated machines.

**External-agent orchestration** — supervise Codex, Gemini CLI, or Claude Code as managed backends, with mid-turn steering, approval gates, rewind, and per-session cost accounting surfaced through the dashboard.

**Persistent daemon** — a control plane supervises many concurrent sessions and is the single writer of shared state; an idle web server runs headless. Federate with peer daemons for multi-host display and capability-based task routing.

**Phone calls** — outbound SIP calls via pjsua with a voice model conducting the conversation, returning structured data.

Four execution modes: *direct* (single agent), *user* (orchestrator + sub-agents in git worktrees), *sub-agent* (scoped child task), and *external-agent* (supervise a third-party coding CLI).

## Dependencies

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack`
- **ffmpeg** — display recording and H264 encoding
- **macOS**: `./scripts/setup-macos.sh` installs everything (cliclick, ffmpeg, Vortex Audio, wasm-pack, app bundle)
- **Linux**: `./scripts/setup-linux.sh` installs everything (build-essential/binutils, libvpx, libxcb, xdotool, PipeWire, ffmpeg, PulseAudio, Xvfb)
- **Windows**: `./scripts/setup-windows.ps1` (`x86_64-pc-windows-msvc`) — see the [Windows support](https://lovon-spec.github.io/intendant/windows-support.html) docs

## Quick Start

```bash
# Build
cargo build --release

# Set up API keys (~/.config/intendant/.env for global use)
echo 'OPENAI_API_KEY=sk-...' > .env

# Run with TUI
./target/release/intendant "List the files in /tmp"

# Headless mode
./target/release/intendant --no-tui "echo hello"

# Choose provider/model
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "Fix the tests"

# Web dashboard runs by default (port 8765); --web sets the port, --no-web disables it
./target/release/intendant --web 9000

# Supervise an external coding agent (codex | claude-code | gemini)
./target/release/intendant --agent codex "Fix the tests"

# Run as MCP server (for Claude Code, etc.)
./target/release/intendant --mcp "Deploy the application"

# JSONL structured output
./target/release/intendant --json "echo hello"

# Resume most recent session
./target/release/intendant --continue "fix that bug"

# Force single-agent mode
./target/release/intendant --direct "simple task"

# Enable Landlock sandboxing (Linux)
./target/release/intendant --sandbox "run tests"
```

## Web Dashboard

The web dashboard runs by default (port 8765; `--no-web` disables it) with a multi-tab interface:

- **Activity** — Live event log with color-coded entries, context/changes views, approval buttons, follow-up input
- **Stats** — Token usage per model with cost estimates, disk usage
- **Terminal** — Embedded xterm.js for the server-side TUI and a live shell
- **Video** — WebRTC display viewers with remote control, recording replay, annotations
- **Station** — Immersive WASM control center for activity, context, managed Codex, changes, sessions, peers/displays, launch config, and Codex thread actions
- **Sessions** — Browse, search, resume, and fork sessions across all backends
- **Settings** — Provider/model, autonomy, external-agent backend, and approval rules
- **Debug** — Diagnostics and internal state

Optional **live voice** via Gemini Live or OpenAI Realtime — the browser connects directly to the model's realtime API through WASM with presence tools for approving actions, submitting tasks, and querying status by voice.

Late-connecting browsers receive the full session replay and cached state.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/intendant/)** — covers architecture, configuration, runtime protocol, display pipeline, computer use, live audio, TUI & autonomy, multi-agent orchestration, the presence layer, web gateway, MCP, Windows support, and session logging.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
