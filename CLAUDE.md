# CLAUDE.md

> **Living document — last verified 2026-05-24 against `main` @ `2ac9e16`.**
> Intendant moves fast (~500 commits/month). Treat the *specifics* below — module
> lists, CLI flags, model names, exact file paths — as **last-known-good, not
> gospel**. If the code disagrees with this file, the code wins: trust it, and fix
> this doc. The big-picture architecture and the conventions drift more slowly than
> the details, so weight them accordingly. To see what has changed since this was
> written: `git log --oneline 2ac9e16..HEAD`. (`AGENTS.md` is a symlink to this
> file, so both stay in sync.)

## What Intendant Is

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop to work in — shell access, file editing, a graphical display it can see and control, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system.

Two binaries form a security boundary:

- **intendant-runtime** — Sandboxed command executor. Reads JSON commands from stdin, executes them sequentially, writes results to stdout. Runs under Landlock filesystem restrictions. Never holds API keys.
- **intendant** — Controller/caller. Manages the LLM conversation loop, calls model APIs, dispatches tool calls to the runtime subprocess, supervises external agent CLIs, and runs all user-facing interfaces (CLI, TUI, Web, MCP, voice).

The system is **provider-agnostic** (OpenAI, Anthropic, Gemini for its native loop), **cross-platform** (macOS, Linux/Debian, Windows — all first-class), and designed around the principle that every capability should be accessible through any interface — TUI, web dashboard, MCP, voice, or programmatic control.

Beyond running its *own* agent loop, Intendant has grown into a **control plane / mission-control for orchestrating fleets of heterogeneous coding agents** — its native loop *plus* external CLIs (Codex, Gemini CLI, Claude Code) — across many concurrent sessions, with a persistent daemon, a multi-session dashboard, and peer-to-peer federation across machines.

## Vision and Direction

The original arc was a single "always-on AI steward." That steward exists; the project has since widened into a multi-session, multi-backend orchestration hub. Current status of the arc (assessed against the code, 2026-05-24):

1. CLI tool that executes agent commands — **done**
2. TUI application with approval gates — **done** (the TUI is now a *display-only* client of the control plane; it no longer owns dispatch authority)
3. Web dashboard with real-time streaming — **done**, and now the **default** frontend; an idle `--web` launches a headless daemon
4. Voice-interactive presence layer — **done**
5. Full desktop agent with display control and computer use — **done**
6. WebRTC display transport with hardware encoding — **done**, extended with federated peer-to-peer display, tile/dirty-region streaming, and loss-resilient H.264 over TURN relays
7. Phone call capability via SIP — **done**
8. Persistent daemon with long-lived session supervision and a centralized control plane — **done**, *except* scheduled/recurring tasks (the lone unbuilt piece of this step; only one-shot controller-restart scheduling exists today)
9. **Multi-session, multi-backend orchestration hub** — *in progress, and the dominant current focus*: supervise / steer / fork / rewind / cost-account many concurrent sessions across Intendant's native loop **and** external coding-agent CLIs (Codex, Gemini CLI, Claude Code), including their side-threads and subagents, surfaced through a multi-session dashboard
10. **Windows as a first-class platform** — **done** (full capture / input / encode / PTY backends merged)
11. **Federation & any-device access** — *in progress*: multi-host peer registry/coordinator, mobile-friendly dashboard

## Architecture Pillars

### 1. Agent Execution

The core loop: select provider, load prompts/skills/knowledge, run a budget-aware conversation loop that dispatches tool calls to the runtime subprocess. Stops at context exhaustion, an explicit `done` signal, or a turn cap.

Execution modes:
- **Direct** (`--direct`): Single agent loop.
- **User** (default native mode): Spawns an orchestrator that decomposes tasks and delegates to specialized sub-agents (Research, Implementation, Testing) running in isolated git worktrees.
- **Sub-Agent** (`INTENDANT_ROLE` set): Scoped child task with a role-specific prompt. Roles: Orchestrator, Research, Implementation, Testing (Testing reuses the base prompt — there is no `SysPrompt_testing.md`).
- **External-Agent** (`--agent <backend>` or `[agent] default_backend` in `intendant.toml`): Intendant *supervises* a third-party coding CLI instead of running its own LLM loop — see pillar 2.

The runtime (`agent.rs`) exposes ~10 functions (`execAsAgent`, `captureScreen`, `inspectPath`, `editFile`, `writeFile`→`editFile`, `browse`, `askHuman`, `execPty`, `storeMemory`, `recallMemory`). The controller-side LLM tool surface (`tools.rs`) is the 9 runtime-mapped tools plus caller-handled `manage_context`, `signal_done`, `invoke_skill`, `spawn_live_audio`, plus any registered MCP client tools.

### 2. External-Agent Orchestration

`src/bin/caller/external_agent/` lets Intendant supervise third-party coding CLIs as subordinate workers. This is the single largest area of recent development and the current frontier.

- **Backends** (`AgentBackend`): **Codex** (`codex app-server`, JSON-RPC over JSONL — by far the richest integration), **Gemini CLI** (`gemini --acp`, Agent Client Protocol), **Claude Code** (`claude -p --output-format stream-json …`).
- Each backend is spawned as a child process (PATHEXT-aware on Windows) and wired to **Intendant's own MCP-over-HTTP server** so the external agent gets Intendant's display/computer-use tools. Codex/Gemini do this by injecting an `[mcp_servers.intendant]` entry into the CLI's config file and restoring it on shutdown; a crash can leave residue.
- The `ExternalAgent` async trait (`mod.rs`) is the contract every backend implements; `codex.rs` is the reference. Backends emit a normalized `AgentEvent` vocabulary that Intendant translates into `AppEvent`s for the frontends.
- **Codex** uniquely supports mid-turn steering (`turn/steer`), side conversations (`side`/`btw`), native subagents, rollback/rewind, forking, compaction, goals, and fork cost accounting. Other backends fall back to a context-injection queue / full session reset (the "not supported by this backend" error strings are **load-bearing** — `drain_external_agent_events` matches on them to distinguish "unsupported → fall back" from "failed"; don't reword casually).
- Approval requests raised *by* the supervised agent are intercepted and surfaced to TUI/web/MCP via the approval registry (when a web dashboard is serving, they go to the gate instead of being auto-denied as headless).
- Per-backend config lives in `[agent.<backend>]` (`project.rs`): Codex sandbox / approval_policy / reasoning_effort / web_search / network_access / writable_roots. Codex latches sandbox/approvals/model/reasoning at `thread/start`, so the daemon rebuilds the Codex process when those change.

### 3. Presence Layer

A separate AI (defaulting to a fast model — currently `gemini-3-flash-preview`) that mediates between the human and the agent system. It observes agent state, narrates events, dispatches tasks, handles approval gates, and maintains conversational continuity. "You ARE Intendant" — the user talks to presence, not directly to the worker agent.

Runs in two modes:
- **Server-side text** (`presence.rs`): For TUI and non-voice web.
- **Browser-side voice** (`crates/presence-web/`): WASM-powered, connects directly to Gemini Live (default `gemini-2.5-flash-native-audio-preview-12-2025`) or OpenAI Realtime (default `gpt-4o-realtime-preview`) from the browser.

These are **not strictly mutually exclusive**: server-side narration is *ref-count paused* while any browser holds active voice, rather than disabled. The `presence-core` crate compiles to both native Rust and WASM, ensuring identical tool definitions and dispatch logic everywhere.

### 4. Display Pipeline (WebRTC)

Agents can see and interact with graphical displays via a custom WebRTC transport built on the sans-I/O **`rtc` crate (rtc-rs 0.9)** — not `webrtc-rs`. The current architecture is a **shared encoder pool feeding per-peer drivers** (an evolution past the old single-track diagram):

```
[CaptureBackend] → broadcast<Frame> → [EncoderPool: always-on baseline + on-demand codecs, VP8 simulcast]
                                          → per-peer rtc driver (picks codec/layer, packetizes) → browser/peer
  browser input → WebRTC data channel → input injection
```

- **EncoderPool** (`display/encode/pool.rs`): one always-on baseline codec (VP8 on macOS/Linux, H.264 on Windows where VP8 is gated off) with VP8 simulcast layers (full/half/quarter, upper layers demand-bound), plus on-demand H.264 (and declared VP9/AV1) encoders refcounted per viewer. A per-display layer-policy coordinator (`aggregator.rs`) pauses/resumes layers; TWCC is surfaced via an interceptor tap (`twcc_tap.rs`) since rtc 0.9 doesn't expose it to the app.
- **Tile / dirty-region streaming (#82)** (`display/tile/`): three browser-created data channels (Control / Snapshot / Deltas), 64×64-px tiles, X11 XDamage (with a frame-diff fallback for non-X11), a 32 KiB datachannel message cap, and a tile↔video fallback policy.
- **Platform capture**: X11 (XShm), Wayland (PipeWire via xdg-portal), macOS (ScreenCaptureKit), Windows (**GDI BitBlt by default**; DXGI Desktop Duplication is opt-in via `INTENDANT_WINDOWS_CAPTURE=dxgi` because DXGI captures black on virtual/RDP/headless adapters).
- **Encoding**: hardware H.264 via VideoToolbox (macOS) and VA-API/libx264 (Linux); Windows uses a Media Foundation **software** H.264 MFT.
- Bidirectional clipboard sync, multi-monitor with stable display identity (Wayland enumeration is portal-limited).
- CU-first routing: display/computer-use tasks go to a fast model first (configured via `[computer_use] provider/model`), with escalation to the heavy agent for coding tasks handled in the orchestrator/task-dispatch layer.

### 5. Peer Federation

`src/bin/caller/peer/` and `src/bin/caller/lan/` let Intendant federate with **peer daemons as equals** (other Intendants, and A2A/OpenClaw/MCP-shaped peers) — distinct from `external_agent`'s master/worker relationship.

- An **Agent Card** is served at `/.well-known/agent-card.json`; a per-peer actor handles connect→loop→reconnect; a capability-based coordinator routes tasks; a multi-transport layer probes candidate URLs (LAN / Tailscale / WAN) in card order; `PinnedFingerprintVerifier` does SHA-256 server-cert pinning on top of mTLS. Only the native Intendant WebSocket transport ships today; A2A/OpenClaw/MCP-as-peer are stubbed.
- **Cross-machine display**: the primary acts as a signaling middleman only — encoded video flows browser↔peer **directly**, with a primary-relay TCP fallback. Use `--advertise-url` to advertise WebSocket endpoints to peers.
- `lan/` is the `intendant lan` subcommand: an mTLS nginx reverse proxy in front of `--web`, with pure-Rust cert generation (rcgen). Separately, `web_tls.rs` adds **native HTTPS/WSS** directly to the `--web` gateway (rustls + rcgen, all platforms) via `--tls`/`--tls-cert`/`--tls-key`.

### 6. Live Audio and Phone Calls

`spawn_live_audio` (an agent tool, not a CLI flag) connects to Gemini Live or OpenAI Realtime via WebSocket, piping audio through a virtual audio bridge (PulseAudio on Linux, the **Vortex Audio** HAL plugin on macOS via a direct shared-memory bridge). Untrusted: zero tools, zero file access. Responses validated against a `ResponseSchema`; unexpected content quarantined.

Skills:
- **phone-call**: outbound SIP calls via `pjsua` with the voice model conducting the conversation, returning structured data. macOS-only (requires the Vortex HAL plugin + a GUI/TCC session).
- **voice-call-app**: make a voice call through *any* app (Element, FaceTime, WhatsApp, …) by driving the UI with computer-use plus `spawn_live_audio`. macOS or Linux.

### 7. Control Plane & Persistent Daemon

- **`control_plane.rs`** is the **single writer** of shared mutable state (autonomy level, active external-agent backend, runtime Codex/Gemini config). Frontends are **display-only** — they render state changes and emit intents, but never write shared state directly.
- **`session_supervisor.rs`** is the long-lived daemon-side owner of sessions launched from the control plane: it accepts `StartTask`/`ResumeSession`/targeted follow-ups off the EventBus, creates per-session resources, and tracks the parent/child/related-session graph.
- **`task_dispatch.rs`** routes dispatch to the right channel (presence / task / follow-up); **`file_watcher.rs`** is a live FS watcher with content-addressed per-round snapshots (rollback/redo/branching) powering the dashboard's activity diffs and rewind, for *all* agent types; **`app_state_pricing.rs`** estimates per-model USD cost.
- An idle `--web` starts a headless daemon (no TUI). **Scheduled/recurring tasks are not yet built** — the only scheduling primitive is one-shot `ScheduleControllerRestart`.

### 8. Human Oversight

Three-layer autonomy system:
1. **Global level** (`--autonomy` Low/Medium/High/Full; default Medium)
2. **Category rules** (`[approval]` in `intendant.toml` — per-category Auto/Ask/Deny)
3. **Per-action approval** (y/s/a/n in any frontend)

Categories: FileRead, FileWrite, FileDelete, CommandExec, NetworkRequest, Destructive, HumanInput, LiveAudioSpawn, DisplayControl.

DisplayControl uses a session-grant model (approve once, revoke anytime via `d` hotkey). Landlock filesystem sandboxing restricts what the runtime can write. A shared `ApprovalDecision` enum (`approval.rs`: Accept / AcceptForSession / Decline / Cancel) is used by both the external-agent and peer layers.

### 9. Frontend Parity

Frontend parity is a **compile-time contract**, but it now runs through *two* enums depending on the frontend:

- **`UserAction`/`StateQuery` in `frontend.rs`** — the parity contract shared by the **TUI and MCP server** (Approve/Deny/Skip/ApproveAll/RespondHuman/SetAutonomy/SetVerbosity/SubmitFollowUp/Quit, …). Exhaustive matching, no wildcards.
- **`ControlMsg` in `event.rs`** — what the **web dashboard and control socket** dispatch; processed centrally by `control_plane.rs` (the single writer). External-agent control (steering, Codex/Gemini thread actions, backend switching) flows here, *not* through `UserAction`.

So "add a capability and the compiler forces you to handle it" still holds — but a new control-plane op means touching `event.rs`/`control_plane.rs`, while a new approval/follow-up action means touching `frontend.rs`.

### 10. MCP (Server and Client)

**Server** (`--mcp`): Exposes Intendant's control surface as MCP tools — approve/deny/skip, start tasks, query status/logs, schedule/cancel controller restarts, intervene in / inspect the controller loop, `rebuild_and_reload` (rebuild + `exec()` hot-reload), display/computer-use/frame tools, and `spawn_live_audio`. Architecturally a peer of the TUI, consuming the same EventBus.

**Client**: Connects to external MCP servers configured in `intendant.toml`. Tools registered as `mcp__<server>_<tool>`.

**Trust model for the client**: each MCP server entry is spawned as a child process with the user's full privileges (`Command::new(&config.command).args(&config.args)` in `mcp_client.rs`). Intendant performs **no checksum verification, no signature check, and no sandboxing** of MCP server binaries — adding one is equivalent to adding a line to your `~/.zshrc` that runs a binary. Default is `mcp_servers = []`, and `intendant.toml` is git-ignored, so the repo ships no MCP servers. Treat copying an `intendant.toml` between machines like copying shell rc files: read it before sourcing.

## Repository Layout

```
src/
├── main.rs                  # intendant-runtime entry point (sandboxed executor)
├── agent.rs                 # Runtime functions (exec, edit, browse, screenshot, PTY, memory)
├── models.rs, error.rs, utils.rs
└── bin/caller/
    ├── main.rs              # intendant entry point (controller): CLI parsing, agent/daemon loops
    ├── provider.rs          # Multi-provider LLM abstraction
    ├── event.rs             # EventBus, AppEvent, ControlMsg
    ├── control_plane.rs     # Single writer of shared state; frontends are display-only
    ├── frontend.rs          # UserAction/StateQuery parity contract (TUI ↔ MCP)
    ├── tools.rs, tool_batch.rs   # Core tool definitions + batching
    ├── autonomy.rs, approval.rs  # Autonomy levels; shared ApprovalDecision
    ├── conversation.rs, prompts.rs, skills.rs
    ├── sub_agent.rs, worktree.rs, worktree_inventory.rs, user_mode.rs, agent_runner.rs, task_dispatch.rs
    ├── session_supervisor.rs, session_log.rs, session_names.rs   # daemon session lifecycle / logging / naming
    ├── knowledge.rs, file_watcher.rs        # tagged knowledge store; live FS watch + rollback snapshots
    ├── app_state_pricing.rs                 # per-model USD cost estimation
    ├── external_agent/      # Supervise Codex / Claude Code / Gemini CLI (mod, codex, claude_code, gemini)
    ├── peer/                # Peer federation (card, actor, registry, coordinator, transport/, …)
    ├── lan/                 # `intendant lan` mTLS reverse-proxy setup (certs, nginx_config, wizard, …)
    ├── computer_use.rs, vision.rs, recording.rs, frames.rs
    ├── display/             # WebRTC transport — encode/{pool,vp8,h264_*}, tile/, capture/, webrtc,
    │                        #   aggregator, twcc_tap, forward, clipboard, {x11,wayland,macos,windows}
    ├── web_gateway.rs, web_tls.rs           # HTTP/WS server + native HTTPS/WSS (rustls/rcgen)
    ├── mcp.rs, mcp_client.rs                # MCP server and client
    ├── live_audio.rs, live_audio_types.rs, audio_routing.rs, transcription.rs, schema_validator.rs, quarantine.rs
    ├── presence.rs          # Server-side presence layer
    ├── control.rs           # Unix control socket (no-op on Windows)
    ├── sandbox.rs, platform.rs              # Landlock; cfg-gated platform abstraction
    ├── diagnostics.rs, debug.rs, daemon_log_tee.rs, upload_store.rs, project.rs, types.rs
    └── tui/                 # ratatui TUI (app, web, widgets, layout, markdown, theme, event)
crates/
├── presence-core/           # WASM-compatible: types, tools, dispatch, prompt (native + wasm32)
└── presence-web/            # Browser WASM: app_state, callbacks, gemini, openai, server
static/                      # app.html dashboard SPA, audio-processor.js, wasm-web/ (compiled WASM)
macos-app/                   # Native macOS WKWebView wrapper (main.swift); built by scripts/bundle-macos.sh
vendor/                      # vortex-guest-tools/ (macOS Vortex Audio HAL plugin pkg)
examples/                    # damage-trace.rs
scripts/                     # setup-{linux,macos,windows}, setup-lan*, bundle-macos, etc.
skills/                      # SKILL.md: phone-call, voice-call-app, wayland-portal-e2e
tests/                       # skills/ (tui/web/voice/recording e2e as SKILL.md), test_gemini_live_*.py; e2e/ is a stub
docs/                        # mdBook (src/) + design docs; includes windows-support.md
SysPrompt*.md                # Per-role prompts (base/direct, tools, user, orchestrator, research, implementation, presence, live audio)
```

The dashboard SPA (`static/app.html`) tabs: **Activity** (Log / Context / Changes / Control), **Stats**, **Terminal** (TUI / Shell), **Video**, **Sessions** (Recent / Deep Search / Worktrees / New Session), **Debug**, **Settings**.

## Build and Run

```bash
cargo build --release     # Produces target/release/intendant-runtime and target/release/intendant
cargo build               # Debug build
cargo check               # Type-check only
```

### WASM (presence-web)
`build.rs` auto-detects stale WASM (mtimes of `crates/presence-web/src` + `crates/presence-core/src` vs the embedded artifact) and **automatically runs `wasm-pack` and re-embeds it on a normal `cargo build`**. You only need the manual command below as a fallback if wasm-pack isn't found or the auto-build fails (`wasm-pack` must be installed either way):
```bash
cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web
```

### CLI usage
Requires an API key in `.env` (searched in cwd + parents, then the project root, then `~/.config/intendant/.env`). `.env` and `intendant.toml` are git-ignored.
```bash
./target/release/intendant "task"                          # Default mode (web dashboard is ON by default)
./target/release/intendant --no-web "task"                 # Disable the web dashboard
./target/release/intendant --no-tui "task"                 # Headless
./target/release/intendant --direct "task"                 # Single-agent (skip orchestrator)
./target/release/intendant --json "task"                   # JSONL output (implies --no-tui)
./target/release/intendant --agent codex "task"            # Supervise an external CLI (codex | claude-code | gemini)
./target/release/intendant --provider anthropic --model claude-sonnet-4-6-20250929 "task"
./target/release/intendant --autonomy low "rm /tmp/test"   # Ask before every action
./target/release/intendant --continue "fix that bug"       # Resume most recent session
./target/release/intendant --resume abc123 "continue"      # Resume by session ID
./target/release/intendant --mcp "task"                    # MCP server on stdio
./target/release/intendant --web [PORT]                    # Explicit dashboard port (default 8765; idle → daemon)
./target/release/intendant --tls --tls-cert C --tls-key K  # HTTPS/WSS (auto self-signed if cert/key omitted)
./target/release/intendant --transcription "task"          # User speech transcription
./target/release/intendant --record-display 0              # Record an existing X11 display (repeatable)
./target/release/intendant --advertise-url ws://host:port  # Advertise to federation peers (repeatable)
./target/release/intendant --sandbox "task"                # Landlock sandboxing (Linux)
./target/release/intendant --control-socket "task"         # Unix control socket
./target/release/intendant --no-presence "task"            # Disable presence layer
./target/release/intendant --log-file <dir> "task"         # Override session log dir
./target/release/intendant lan setup                       # mTLS LAN access (subcommand)
echo "task" | ./target/release/intendant                   # Auto-detects non-TTY -> headless
```

### Runtime
```bash
echo '{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo hello"}]}' | ./target/release/intendant-runtime
```

### macOS app
```bash
./scripts/bundle-macos.sh   # swiftc main.swift + bundle the release binary + codesign → /Applications/Intendant.app
```
A windowed WKWebView wrapper that spawns the Rust backend (`intendant-bin --web <port>`), serves the dashboard over a custom `intendant://` scheme (so WKWebView grants a secure context for mic/camera), and triggers TCC prompts.

### Windows
`./scripts/setup-windows.ps1`; target `x86_64-pc-windows-msvc`. See `docs/src/windows-support.md`.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys)
cargo test -- --list      # List all tests
```

Unit tests: inline `#[cfg(test)]` modules, `#[tokio::test]` for async, `tempfile` for filesystem isolation.

**`tests/e2e/main.rs` is currently an empty stub** — the previously documented `cargo test --test e2e test_basic|test_control_socket|test_web|test_voice` tiers no longer exist. The integration/e2e scenarios now live as **SKILL.md files under `tests/skills/`** (`tui-e2e`, `web-e2e`, `voice-e2e`, `recording-e2e`) plus two Python live-audio harnesses (`tests/test_gemini_live_*.py`). These make real API calls / need a display and are **not for CI**. Run `cargo test --bins` and `cargo clippy` locally before committing.

## Key Design Decisions

- **Two-process security split**: Runtime executes commands under Landlock; controller holds API keys but never runs user-requested commands directly.
- **Provider-agnostic with native tool calling**: Proper support for each provider's native tool calling, CU, and streaming APIs — not a prompt-level abstraction.
- **External-agent supervision**: Orchestrate Codex/Gemini/Claude Code CLIs as subordinate workers behind a normalized `ExternalAgent` trait, wired to Intendant's own MCP server for display/CU.
- **Control plane is the single writer**: `control_plane.rs` owns shared-state writes; frontends render and emit intents, never mutate state directly.
- **Peer federation as equals**: `peer/` federates with other daemons (vs the external-agent master/worker relationship).
- **WebRTC on rtc-rs (sans-I/O)**: a shared encoder pool feeding per-peer drivers, with an interceptor TWCC tap, rather than per-peer encoders.
- **Quarantine for untrusted voice**: Live audio model outputs are schema-validated and quarantined, never exposed to agents.
- **Git worktree isolation**: Sub-agents work in isolated worktrees, enabling parallel development on separate branches.
- **Frontend parity via exhaustive enums**: `UserAction` (TUI/MCP) and `ControlMsg` (web/control socket) give a compile-time guarantee that interfaces handle the same operations.
- **Presence as a separate AI**: a distinct model with its own conversation, tools, and state awareness — not a chat wrapper.

## Code Conventions

- Rust 2021 edition, default rustfmt/clippy (no config files)
- snake_case functions/modules, PascalCase types, SCREAMING_SNAKE_CASE constants
- `thiserror`-based error enums (`AgentError`, `CallerError`)
- tokio (full features), `Arc<RwLock/Mutex<T>>` for shared state, `mpsc` for channels
- TLS/cert code is **pure-Rust `ring`/`rcgen`/`rustls`** (`web_tls.rs`, `lan/certs.rs`) — no OpenSSL. Prefer that path when touching crypto/cert code.
- Pure-safe Rust by default. The Unix (macOS / Linux) code paths contain no
  `unsafe` beyond a handful of well-documented libc existence/identity probes
  in `platform.rs`. The Windows backends are the deliberate exception: capture,
  input injection, and H.264 encoding necessarily go through Win32/COM/Media
  Foundation FFI (`display/windows.rs`, `display/encode/h264_windows.rs`,
  `platform.rs`), which has no safe alternative. Confine that `unsafe` to those
  `#[cfg(windows)]` blocks, keep each block as small as the FFI call it wraps,
  prefer the `windows` crate's RAII interface types (which Release COM refs on
  drop) and small safe wrappers / RAII guards over hand-rolled lifetime
  management, and annotate every `unsafe` block with a `// SAFETY:` comment
  stating the invariant that makes it sound (handle/pointer validity, COM
  refcount/ownership, buffer bounds, thread/apartment affinity). Do not
  introduce `unsafe` on the cross-platform or Unix paths.
- Tests: inline `#[cfg(test)]` modules only
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`
- When adding a new system / `-sys` crate dependency, update **both**
  `scripts/setup-linux.sh` (`APT_PACKAGES`) and `scripts/setup-macos.sh`
  (`check_core` or an appropriate check function) in the same commit.
  Silent missing deps break fresh-machine setups with cryptic `pkg-config`
  errors long after the crate was added.

### Platform Support

Target platforms: **macOS, Linux** (Debian, X11 and Wayland), **and Windows**
(`x86_64-pc-windows-msvc`). Windows is a first-class target — capture, input
injection, H.264 encode, and the gateway all have Windows backends, built and
run via `scripts/setup-windows.ps1` (see `docs/src/windows-support.md`). Windows
deferrals (degrade cleanly): the WASAPI audio bridge is wired but not E2E
validated, `intendant lan` is gated off, `--sandbox` (Landlock) is a no-op, and
there is no Xvfb/virtual-display equivalent (an interactive desktop session is
required — a Session-0 headless service won't capture/inject).

**OS-specific `std` APIs must be `#[cfg]`-guarded.** Don't
`use std::os::unix::fs::MetadataExt;` (→ `.ctime()/.dev()/.ino()/.nlink()/.blocks()`),
or any `std::os::unix::*` / `std::os::windows::*` item, unconditionally — it
breaks the other platform's build. Wrap the platform call in a
`#[cfg(unix)]`/`#[cfg(windows)]`-paired helper in `platform.rs` (the existing
convention) with a portable fallback, and route callers through it.

Prefer platform-agnostic code by default. When platform-specific behavior is
unavoidable, use `cfg!(target_os = ...)` runtime checks for small branches or
`#[cfg(target_os = "...")]` compile-time gates for entire implementations.
Collect OS-specific helpers in dedicated modules (e.g. `platform.rs`,
per-platform blocks in `vision.rs`, `audio_routing.rs`, `computer_use.rs`,
`display/`).

Every feature must either work or degrade gracefully with a clear error on all
supported platforms — never panic or silently do nothing.

## Environment Requirements

- **OS**: macOS, Linux (Debian), or Windows (Server 2022 / 11); unprivileged user with passwordless sudo on Linux
- **API keys**: `.env` (cwd / project root / `~/.config/intendant/.env`) with `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`
- **Display capture**: libxcb + libxcb-shm (Linux X11), PipeWire (Linux Wayland), ScreenCaptureKit (macOS)
- **Input injection**: xdotool (Linux X11), ydotool (Linux Wayland), cliclick (macOS)
- **Encoding**: libvpx (VP8), ffmpeg with x264/VA-API (Linux H264), VideoToolbox (macOS H264), Media Foundation (Windows H264)
- **Recording**: ffmpeg
- **Voice audio (macOS)**: Vortex Audio HAL plugin (from `vendor/vortex-guest-tools/`, BlackHole as fallback), `SwitchAudioSource`, `sox`
- **Misc tooling expected by agents/setup**: ripgrep, ImageMagick (X11 screenshots), x11vnc, xdg-utils
- **WASM build**: `wasm-pack` (`cargo install wasm-pack`)
- **External-agent backends** (optional): `codex`, `gemini`, and/or `claude` CLIs on `PATH`
- **Full setup**: `./scripts/setup-linux.sh` (Debian/Ubuntu), `./scripts/setup-macos.sh` (macOS), or `./scripts/setup-windows.ps1` (Windows)

## Multi-Agent Development

Multiple AI agents run concurrently on this machine, each in an isolated git
worktree. The **main worktree (the repo root** — e.g. `/Users/<you>/projects/intendant`
on macOS, `/home/<you>/projects/intendant` on Linux**) is the shared merge
target** — never build or run intendant from the main worktree. Always build and
launch from your own worktree's `target/release/intendant`.

Each running intendant instance binds its own web port (printed at startup).
Port discovery is automatic — the dashboard finds all running instances. Note
your port so the user can access your instance. Don't kill intendant processes
you didn't spawn; they belong to other agents.

## CI/CD

GitHub Actions (on push / PR to `main`):
- **`windows.yml`** — cross-platform `cargo check -p intendant` on Windows + macOS + Linux (catches platform-specific build breaks; deliberately excludes the WASM `presence-web` crate).
- **`audit.yml`** — `cargo audit` on push/PR **plus a weekly cron** (Mondays 08:00 UTC).
- **`docs.yml`** — mdBook docs deploy to GitHub Pages.

The `tests/e2e/`-style integration tests are NOT in CI (they make real API calls / need a display). Run `cargo test --bins` and `cargo clippy` locally before committing.
