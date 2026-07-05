# CLAUDE.md

> **Living document — last verified 2026-07-04 against `main` @ `3fb8eb30`.**
> This is a *tight orientation* for working in the repo. The deep reference lives in
> the mdBook under `docs/src/` (mapped below). **Both this file and those docs lag the
> code** — Intendant moves fast (~500 commits/month) and the docs are *not* updated on
> every change. When this file, the docs, and the source disagree, **trust the source**,
> then fix the doc. See what changed since this was written with
> `git log --oneline 3fb8eb30..HEAD`. (`AGENTS.md` is a tracked, byte-for-byte copy of this
> file — when you edit CLAUDE.md, run `cp CLAUDE.md AGENTS.md` in the same commit; CI enforces they match.)

## What Intendant Is

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop — shell, file editing, a graphical display it can see and control, voice, and phone calls — under layered human oversight. Beyond running its own agent loop, it **supervises external coding agents** (Codex, Claude Code) as managed backends and **federates with peer machines**. Provider-agnostic (OpenAI, Anthropic, Gemini); cross-platform (macOS, Linux, Windows — all first-class); every capability reachable from any interface (CLI, TUI, web dashboard, MCP, voice).

Past the single box, the ambition is a **network of agentic networks** — fleets of daemons owned by people and organizations, where owners grant other people and other daemons scoped access to their machines, infrastructure, and resources. Three pillars carry it: the **trust architecture** (authority is only ever minted by the target daemon's local IAM; browser identity keys protected by passkeys; org root keys sign grant documents and revocation lists; the hosted rendezvous is zero-authority and self-hostable), **credential custody** (daemons borrow time-boxed leases from a passkey-sealed vault or relay calls through the owner's browser — a disposable box's disk holds no durable secrets), and the **zero-install client** (the entire client is a browser tab: claim a fresh daemon with a twelve-word phrase, watch every fleet display live, phone included). The name is the thesis: agents perform, orchestrators conduct, the Intendant runs the house — and answers to the owner.

## The Three Binaries (security boundary)

- **intendant-runtime** (`src/main.rs`, `src/agent.rs`) — sandboxed executor. Reads one JSON `AgentInput` from stdin, runs commands sequentially, writes JSONL results. Landlock-restricted on Linux, Seatbelt-wrapped on macOS, restricted-token re-exec on Windows (`src/win_sandbox.rs`). **Never holds API keys.**
- **intendant** (`src/bin/caller/main.rs`) — controller. Drives the LLM loop, calls model APIs, dispatches tool calls to the runtime subprocess, supervises external agents, and runs every frontend.
- **intendant-connect** (`src/bin/connect/main.rs`) — hosted rendezvous + account/metadata service (deployed to intendant.dev; self-hostable, see `docs/src/self-hosted-rendezvous.md`). Stores only what daemons and browsers publish; fleet records are browser-signed and re-verified client-side so the service cannot invent or alter them. Holds no daemon secrets and no API keys.

The runtime/controller split is the load-bearing security decision: a compromised model conversation can't reach API keys; the runtime can't exfiltrate through model APIs. Preserve it.

## Architecture at a Glance

The controller runs a budget-aware in-process loop in one of several **execution shapes**: Direct (`--direct`, and every non-daemon CLI path), Orchestrate (the same loop with the orchestration prompt; delegates via the `spawn_sub_agent` / `wait_sub_agents` tools), Sub-Agent (a supervised child session that reports back with `submit_result`, optionally in an isolated git worktree), and External-Agent (`--agent`, supervising a third-party coding CLI). Orchestration is a capability of every supervised native session, not a separate mode — the February-era subprocess pipeline (`run_user_mode`, `INTENDANT_ROLE` child processes, result-file polling) is gone. A separate **presence** AI mediates between the user and the worker. A single-writer **control plane** owns shared state — frontends are display-only, emitting intents (`ControlMsg`) rather than mutating state. A persistent **daemon** owns long-lived sessions; the web dashboard is the default frontend (`--web` is on by default).

Read the relevant chapter before changing a subsystem:

| Area | Chapter |
|---|---|
| Whole-system overview, the agent loop, streaming, caching | `docs/src/architecture.md` |
| Native multi-agent orchestration (modes, sub-agents, worktrees) | `docs/src/multi-agent.md` |
| Supervising Codex / Claude Code | `docs/src/external-agent-orchestration.md` |
| Control plane, persistent daemon, session lifecycle | `docs/src/control-plane-and-daemon.md` |
| Runtime stdin/stdout JSON protocol | `docs/src/runtime-protocol.md` |
| WebRTC display (shared encoder pool, tile streaming) | `docs/src/display-pipeline.md` |
| Peer federation, cross-machine display, LAN/mTLS | `docs/src/peer-federation.md` |
| Trust model: anchor daemons, client identity keys, role ceilings, IAM | `docs/src/trust-architecture.md` |
| Credential custody: leases, vault, egress relay, OAuth modes | `docs/src/credential-custody.md` |
| Hosted rendezvous (intendant-connect), claims, self-hosting | `docs/src/self-hosted-rendezvous.md` |
| Computer use, live audio, phone/voice-call skills | `docs/src/computer-use-and-audio.md` |
| Presence layer (server text + browser voice) | `docs/src/presence.md` |
| TUI + the autonomy/approval model | `docs/src/tui.md` |
| Web dashboard (tabs, sessions, live voice) | `docs/src/web-dashboard.md` |
| Station (rendered control canvas): architecture + roadmap to immersive 3D/XR | `docs/src/station.md` |
| MCP server + client (trust model) | `docs/src/mcp-server.md` |
| MCP client, external CLIs, audio stack, system tools, control socket, CI | `docs/src/integrations.md` |
| Full `intendant.toml` + env reference | `docs/src/configuration.md` |
| Session logging, replay, rehydration | `docs/src/session-logging.md` |
| Windows backends and gotchas | `docs/src/windows-support.md` |

## Build, Run, Test

```bash
cargo build --release     # → target/release/{intendant-runtime, intendant}
cargo build               # debug
cargo check               # type-check only
cargo test --bins         # unit tests (fast, no API keys)
cargo clippy              # lint
```

**WASM** (`crates/presence-web` → `static/wasm-web/`, `crates/station-web` → `static/wasm-station/`): `build.rs` auto-detects stale WASM in either crate and rebuilds it via `wasm-pack`, then re-embeds, on a normal `cargo build` (wasm-pack must be installed). Manual fallback only if that fails:
`cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web` or
`cd crates/station-web && wasm-pack build --target web --out-dir ../../static/wasm-station --out-name station_web`.

Common invocations (full flag reference in `docs/src/getting-started.md`):

```bash
./target/release/intendant "task"                  # web dashboard ON by default (port 8765)
./target/release/intendant --no-web --no-tui "task"  # headless
./target/release/intendant --direct "task"         # single agent (skip orchestrator)
./target/release/intendant --agent codex "task"    # supervise an external coding CLI
./target/release/intendant --mcp "task"            # MCP server on stdio
./target/release/intendant --continue "..."        # resume most recent session
./target/release/intendant org init <handle>       # create an org root key on this daemon (trust model)
```

Requires an API key in `.env` (searched: cwd + parents → project root → `~/.config/intendant/.env`). `.env` and `intendant.toml` are git-ignored.

**Tests:** unit tests are inline `#[cfg(test)]` modules. `tests/e2e/main.rs` is an empty stub; end-to-end scenarios now live as SKILL.md files under `tests/skills/` and are **not** in CI (they make real API calls / need a display). `scripts/validate-dashboard.cjs` is the dashboard/Station QA harness (drives a real browser over CDP; also not in CI). Run `cargo test --bins` and `cargo clippy` locally before committing.

## Repository Layout

```
src/
├── main.rs, agent.rs           # intendant-runtime (sandboxed executor)
├── models.rs, error.rs, utils.rs, win_sandbox.rs
├── bin/caller/                 # the intendant controller:
│   ├── main.rs                 # entry: CLI parsing, agent + daemon loops
│   ├── control_plane.rs, event.rs, frontend.rs   # single-writer state; EventBus; UserAction/ControlMsg
│   ├── session_supervisor.rs, task_dispatch.rs, file_watcher.rs   # daemon: sessions, dispatch, rewind snapshots
│   ├── provider.rs, conversation.rs, tools.rs, prompts.rs, skills.rs, autonomy.rs, approval.rs
│   ├── sub_agent.rs, worktree.rs, worktree_inventory.rs, agent_runner.rs   # native multi-agent
│   ├── context_rewind.rs, fission_ledger.rs, fission_lifecycle.rs, lineage_ledger.rs   # managed context: rewinds, fission, lineage
│   ├── external_agent/         # supervise Codex / Claude Code (+ external_wrapper_index.rs)
│   ├── access/                 # trust architecture: client keys, IAM, org roots/issuers/ORL, enrollment, platform keystores
│   ├── credential_leases.rs, credential_egress.rs, daemon_identity.rs, connect_rendezvous.rs   # credential custody; Connect client
│   ├── peer/, web_tls.rs       # peer federation (transport, pairing, access profiles); native HTTPS/WSS
│   ├── display/                # WebRTC: encode/{pool,vp8,h264_*}, tile/, capture/, webrtc, {x11,wayland,macos,windows}
│   ├── computer_use.rs, ax.rs, vision.rs, recording.rs, frames.rs
│   ├── presence.rs, live_audio.rs, audio_routing.rs, transcription.rs, quarantine.rs, schema_validator.rs
│   ├── web_gateway.rs, dashboard_control.rs, terminal.rs, browser_workspace.rs   # HTTP/WS gateway; dashboard tunnel; PTY registry; agent browser
│   ├── mcp.rs, mcp_client.rs, control.rs
│   ├── transfer_store.rs, upload_store.rs, peer_file_transfer.rs   # transfer jobs; upload/attachment stores
│   ├── session_log.rs, session_names.rs, knowledge.rs, project.rs, app_state_pricing.rs
│   ├── sandbox.rs, platform.rs, daemon_log_tee.rs, diagnostics.rs, …
│   └── tui/                    # ratatui TUI (display-only client of the control plane)
└── bin/connect/                # intendant-connect: hosted rendezvous (accounts, daemon claims, fleet sync, vault blobs, push, transparency log)
crates/{presence-core, presence-web, station-web}   # WASM: shared presence types/tools/dispatch, browser presence client, Station renderer
crates/app-html-assembler   # assembles static/app.html from static/app/ (build.rs + the CI regen gate)
static/         # dashboard SPA: app/ fragments (source) → generated app.html; compiled wasm-web/ + wasm-station/
macos-app/      # native macOS WKWebView wrapper (built by scripts/bundle-macos.sh)
vendor/         # vortex-guest-tools (macOS Vortex Audio HAL plugin)
scripts/        # setup-{linux,macos,windows}, setup-lan*, bundle-macos, validate-dashboard.cjs (dashboard/Station QA), …
skills/         # intendant-cli, visual-collaboration, phone-call, voice-call-app, …
docs/src/       # this project's mdBook — the deep reference (see the table above)
SysPrompt*.md   # per-role system prompts (base, tools, user, orchestrator, research, implementation, presence, live audio)
```

## Code Conventions

- Rust 2021 edition, default rustfmt/clippy (no config files)
- snake_case functions/modules, PascalCase types, SCREAMING_SNAKE_CASE constants
- `thiserror`-based error enums (`AgentError`, `CallerError`)
- tokio (full features), `Arc<RwLock/Mutex<T>>` for shared state, `mpsc` for channels
- TLS/cert code is **pure-Rust `ring`/`rcgen`/`rustls`** (`web_tls.rs`, `access/certs.rs`) — no OpenSSL; prefer that path when touching crypto/cert code
- Tests live in inline `#[cfg(test)]` modules only
- **File size budget:** keep a source file under ~3k lines (4k absolute ceiling;
  the remaining god-files are legacy being carved down, not precedents). When a
  file outgrows its seams, split along domain boundaries as **pure-move
  commits**: relocate code *and its tests* verbatim into a new module, add
  `mod new_module; pub(crate) use new_module::*;` at the old location so every
  existing reference keeps compiling, and widen moved items to `pub(crate)` as
  needed — that widening is the only permitted non-move edit. No renames,
  reformatting, or logic changes ride in a move commit; review with
  `git diff --color-moved=dimmed-zebra`, where any non-dimmed red/green is a
  violation.
- **Derive, don't mirror.** Daemon truth a frontend needs — permission
  catalogs, feature lists, availability booleans, option vocabularies — is
  declared once and derived everywhere else (exemplar: `CONTROL_METHODS` in
  `dashboard_control.rs` drives the authorizer, the `features` list, and the
  per-method availability booleans). When a static frontend fallback copy is
  unavoidable (app.html's IAM catalog, the peer-profile picker), a
  daemon-side parity test pins its ID sets to the source, so a catalog
  change that forgets the mirror fails the suite instead of shipping as
  drift.
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`
- `static/app.html` is **generated** from the `static/app/` fragments (order =
  `static/app/manifest.txt`; assembled by `build.rs` via
  `crates/app-html-assembler`; CI enforces the match). Edit the fragments,
  never the artifact. Merge conflicts: resolve them in the fragments, run
  `cargo run -p app-html-assembler`, then `git add static/app.html` — never
  hand-reconcile the generated file.
- Pure-safe Rust by default. The Unix (macOS / Linux) code paths keep `unsafe`
  confined to documented islands: small platform probes/signals and display or
  identity queries in `platform.rs`; macOS Accessibility bindings in `ax.rs`
  (raw `accessibility-sys` FFI wrapped once there — no safe wrapper crate exists
  without dragging in a duplicate `core-graphics`/legacy `objc` stack); and the
  Vortex direct POSIX shared-memory bridge in `live_audio.rs` (`shm_open`,
  `mmap`, and raw ring-buffer access to the Vortex HAL plugin's shared state).
  Every unsafe block must be type-checked, `// SAFETY:`-commented, and kept as
  small as the FFI call or raw-pointer access it wraps; AX object lifetimes are
  RAII-managed via `core-foundation` `TCFType` wrappers. Do not add AX `unsafe`
  outside `ax.rs`, Vortex-shm `unsafe` outside `live_audio.rs`, or small OS
  probes/signals outside `platform.rs`. The Windows backends are the other
  deliberate exception: capture,
  input injection, and H.264 encoding necessarily go through Win32/COM/Media
  Foundation FFI (`display/windows.rs`, `display/encode/h264_windows.rs`,
  `platform.rs`), which has no safe alternative. Confine that `unsafe` to those
  `#[cfg(windows)]` blocks, keep each block as small as the FFI call it wraps,
  prefer the `windows` crate's RAII interface types (which Release COM refs on
  drop) and small safe wrappers / RAII guards over hand-rolled lifetime
  management, and annotate every `unsafe` block with a `// SAFETY:` comment
  stating the invariant that makes it sound (handle/pointer validity, COM
  refcount/ownership, buffer bounds, thread/apartment affinity). Do not
  introduce `unsafe` on the cross-platform or Unix paths beyond these
  documented exceptions.
- When adding a new system / `-sys` crate dependency, update **both**
  `scripts/setup-linux.sh` (`APT_PACKAGES`) and `scripts/setup-macos.sh`
  (`check_core` or an appropriate check function) in the same commit. Silent
  missing deps break fresh-machine setups with cryptic `pkg-config` errors long
  after the crate was added.

## Reconciling Contradictions

This codebase is heavily AI-coding-agent built, at high velocity — contradictions
between older and newer code, and between code and its comments/docs, are expected
accumulated debt, not anomalies. Resolve them like amendments to a statute:
**the latest deliberate change expresses the current intent and overrides what it
contradicts** (*lex posterior*). Qualifiers, in order:

1. **Invariants outrank recency.** The runtime/controller key boundary,
   authority-minted-only-by-local-IAM, fail-closed defaults, and explicit user
   decisions are not repealed by a newer commit that happens to conflict with them
   — that's a bug in the newer commit.
2. **Deliberate exceptions survive newer generalities** (*lex specialis*): a
   documented platform carve-out or workaround is not steamrolled by a newer
   general pattern. Deliberately parked seeds (unwired modules kept for a future
   pass) are decisions, not stale losers — reconcile per-idea with the user,
   never gut them wholesale.
3. **Date the idea, not the line.** Use `git log -S`/`-L` on the semantic change;
   mechanical sweeps (fmt, clippy, merge fixups, renames) re-touch lines without
   carrying intent about them.
4. **Age predicts craft.** Coding-agent capability rises quickly, so the older an
   implementation, the weaker the model that likely wrote it. When two live
   implementations of the same idea conflict, prefer the newer approach and port
   what the older one still does better — but an old *decision* nothing has
   revisited is not thereby wrong.
5. **Codify the resolution.** Fix the losing side (comment, doc, or code) in the
   same change, so the contradiction dies instead of being re-litigated by the
   next agent.

## Platform Support

macOS, Linux (Debian, X11 and Wayland), and Windows (`x86_64-pc-windows-msvc`) are all
first-class targets. **OS-specific `std` APIs must be `#[cfg]`-guarded** — don't use
`std::os::unix::*` / `std::os::windows::*` items unconditionally; wrap the platform call
in a `#[cfg(unix)]`/`#[cfg(windows)]`-paired helper in `platform.rs` (the existing
convention) with a portable fallback, and route callers through it. Prefer
platform-agnostic code; when unavoidable, use `cfg!(target_os = ...)` for small branches
or `#[cfg(target_os = "...")]` for whole implementations, collected in dedicated modules
(`platform.rs`, per-platform blocks in `display/`, `vision.rs`, `audio_routing.rs`,
`computer_use.rs`). Every feature must work or degrade gracefully with a clear error on
all supported platforms — never panic or silently do nothing. See `docs/src/windows-support.md`.

## Multi-Agent Development

Multiple AI agents run concurrently on this machine, each in an isolated git worktree.
**The main worktree (the repo root) is the shared merge target — never build or run
intendant from it.** Always build and launch from your own worktree's
`target/release/intendant`. Each running instance binds its own web port (printed at
startup) and the dashboard auto-discovers running instances; note your port so the user
can reach your instance. Don't kill intendant processes you didn't spawn — they belong to
other agents.

Never rewrite git history unless the user explicitly asks for it. This includes
`git rebase`, `git commit --amend`, force-pushes, and rewriting already reported
commits on a feature branch. When moving work from an agent worktree into `main`, use a
fast-forward merge if possible; if not, create a normal merge commit or ask the user how
to proceed.

## CI/CD

GitHub Actions on push / PR to `main`:
- **`windows.yml`** — cross-platform `cargo test -p intendant --bins` on Windows + macOS + Linux (catches platform-specific build breaks *and* Unix-only test/path assumptions; excludes the WASM crates). Headless-safe: the unit suite needs no display or API keys.
- **`audit.yml`** — `cargo audit` on push/PR plus a weekly cron (Mondays 08:00 UTC).
- **`docs.yml`** — mdBook (`docs/`) deploy to GitHub Pages.

The `tests/skills/` end-to-end scenarios are not in CI (real API calls / need a display). Run `cargo test --bins` and `cargo clippy` locally before committing.
