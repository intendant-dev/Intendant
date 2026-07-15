# CLAUDE.md

> **Living document — reviewed 2026-07-13 for the alpha trust refactor, based on `main` @ `714f24af`.**
> This is a *tight orientation* for working in the repo. The deep reference lives in
> the mdBook under `docs/src/` (mapped below). **Both this file and those docs lag the
> code** — Intendant moves fast (~500 commits/month) and the docs are *not* updated on
> every change. When this file, the docs, and the source disagree, **trust the source**,
> then fix the doc. See what changed since this was written with
> `git log --oneline 714f24af..HEAD`. (`AGENTS.md` is a tracked, byte-for-byte copy of this
> file — when you edit CLAUDE.md, run `cp CLAUDE.md AGENTS.md` in the same commit; CI enforces they match.)

## What Intendant Is

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop — shell, file editing, a graphical display it can see and control, voice, and phone calls — under layered human oversight. Beyond running its own agent loop, it **supervises external coding agents** (Codex, Claude Code) as managed backends and **federates with peer machines**. It is provider-agnostic (OpenAI, Anthropic, Gemini) and cross-platform (macOS, Linux, Windows — all first-class). Its shipped trust anchors are local presence and an independently reached direct-mTLS dashboard; CLI and MCP provide automation, and the dashboard provides visual, voice, and phone control. The packaged macOS app contains a local mTLS bridge, but no Developer ID-signed/notarized release has been published for this alpha. An `-unsigned-dev` app artifact is not a distribution trust anchor.

Past the single box, daemons federate into a **network of agentic networks** — fleets owned by people and organizations, where owners grant other people and other daemons scoped access to their machines, infrastructure, and resources. Three pillars carry it: the **trust architecture** (authority is only ever minted by the target daemon's local IAM; a hosted claim is discovery/route metadata and creates no principal or grant; the default binary fixes hosted provenance at the zero-permission `role:none` and treats rendezvous-controlled fleet WebPKI names as discovery-only, so shipped daemon control requires a trusted local or independently reached daemon-served direct/mTLS surface; org root keys sign grant documents and revocation lists), **credential custody** (sealed vault stores, time-boxed leases, and client egress exist, but the default build has no bridge from a Connect-origin account vault to a trusted daemon session; `.env` remains supported, and active full-credential OAuth leases temporarily materialize private auth files), and the **zero-install discovery client** (a browser tab can link a daemon with a single-use twelve-word claim code and find the fleet, but cannot control it). Connect remains self-hostable and is not an authority mint, but it is trusted for availability, account/route metadata, and the browser code and installers it serves. The name is the thesis: agents perform, orchestrators conduct, the Intendant runs the house — and answers to the owner.

## The Three Binaries (security boundary)

- **intendant-runtime** (`src/main.rs`, `src/agent.rs`) — sandboxed executor. Reads one JSON `AgentInput` from stdin, runs commands sequentially, writes JSONL results. Landlock-restricted on Linux, Seatbelt-wrapped on macOS, restricted-token re-exec on Windows (`src/win_sandbox.rs`). **Never holds API keys.**
- **intendant** (`src/bin/caller/main.rs`) — controller. Drives the LLM loop, calls model APIs, dispatches tool calls to the runtime subprocess, supervises external agents, and runs every frontend.
- **intendant-connect** (`src/bin/connect/main.rs`) — hosted rendezvous + account/metadata service (deployed to intendant.dev; self-hostable, see `docs/src/self-hosted-rendezvous.md`). Stores what daemons and browsers publish. Fleet records are self-signed: the current browser detects same-key alteration, but there is not yet an owner/device trust set, so a malicious store can substitute a newly self-signed record on another device. That metadata never grants daemon authority. Connect also serves browser code and installers, so malicious served code can alter or exfiltrate what the Connect page sees and a malicious installer can compromise what it installs. It holds no daemon API keys, cannot mint daemon-local IAM, and the default daemon refuses hosted-provenance control at immutable `role:none`.

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
| Trust tiers: which client for which daemon, fleet zones, custody per tier | `docs/src/trust-tiers.md` |
| Credential custody: leases, vault, egress relay, OAuth modes | `docs/src/credential-custody.md` |
| Hosted rendezvous (intendant-connect), claims, self-hosting | `docs/src/self-hosted-rendezvous.md` |
| Computer use, live audio, phone/voice-call skills | `docs/src/computer-use-and-audio.md` |
| Presence layer (server text + browser voice) | `docs/src/presence.md` |
| The autonomy/approval model | `docs/src/autonomy.md` |
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
cargo test --bins         # unit tests (no API keys; what CI runs)
cargo nextest run --bins  # same tests, much faster: one process per test
                          # (needs cargo-nextest; config in .config/nextest.toml)
cargo clippy              # lint
```

Never clear `RUSTC_WRAPPER` or set `RUSTC` for builds in this repo: the
box-wide compile governor (scripts/ci/README.md, "Governor") rides those
settings, and overriding them opts your build out of the machine's
RAM-protecting compile ceiling (the 2026-07-10 OOM-spiral class).
Permitted only when deliberately diagnosing the wrapper chain itself.

**Avoid needless final links** — they are the expensive step (a debug
`intendant` link peaks ~2GiB linker RSS; concurrent final links are what
swap-storm the box, and the governor serializes them machine-wide, so
extra links also queue everyone else's): `cargo check` while iterating;
when you need binaries, name them — `cargo build --bin intendant --bin
intendant-runtime` covers running the controller (`intendant-connect`
matters only for Connect work) — and save the all-binaries `cargo build`
for validation that genuinely needs all three.

**WASM** (`crates/presence-web` → `static/wasm-web/`, `crates/station-web` → `static/wasm-station/`): `build.rs` auto-detects stale WASM in either crate and rebuilds it via `wasm-pack`, then re-embeds, on a normal `cargo build`. wasm-pack is **version-pinned** by `.wasm-pack-version` (releases emit byte-different artifacts, and the artifacts are committed — cross-version rebuilds churn them and conflict concurrent landings): build.rs skips the rebuild under any other version, and the setup scripts install the pin. Manual fallback only if the auto-rebuild fails: `bash scripts/build-wasm.sh`
(the canonical builder — it carries the registry-path remap that makes
artifact bytes account-independent; the CI drift gate rebuilds through the
same script, and build.rs mirrors its flags). **Regenerate wasm artifacts on
macOS only** — output is not byte-deterministic across host triples.

Common invocations (full flag reference in `docs/src/getting-started.md`):

```bash
./target/release/intendant "task"                  # web dashboard ON by default (port 8765)
./target/release/intendant --no-web "task"         # headless
./target/release/intendant --direct "task"         # single agent (skip orchestrator)
./target/release/intendant --agent codex "task"    # supervise an external coding CLI
./target/release/intendant --mcp "task"            # MCP server on stdio
./target/release/intendant --continue "..."        # resume most recent session
./target/release/intendant org init <handle>       # create an org root key on this daemon (trust model)
```

Requires an API key in `.env` (searched: cwd + parents → project root → `~/.config/intendant/.env`). `.env` and `intendant.toml` are git-ignored.

**Operating vs. developing:** to drive a *running* daemon — sessions, approvals,
displays, screenshots, computer use, federated peers — use `intendant ctl`
(self-describing: `ctl --help`; agent guide in `skills/intendant-cli/SKILL.md`).
Read the source to change Intendant, not to operate it.

**Tests:** unit tests are inline `#[cfg(test)]` modules. `tests/e2e/` is the headless end-to-end suite (in CI on all three platforms): it spawns the real binaries against the scripted mock provider (`PROVIDER=mock` + `INTENDANT_MOCK_SCRIPT`, `src/bin/caller/provider_mock.rs`) — keyless, no network, no display; run it with `cargo test --test e2e`. "No display" is enforced by `INTENDANT_MOCK_DISPLAY=synthetic` (honored only alongside `PROVIDER=mock`; fail-closed otherwise), which serves display enumeration/capture from a deterministic 1280×720 synthetic backend (`crates/intendant-display/src/synthetic.rs`) so no native capture API (SCK, GDI/DXGI, X11, Wayland) is ever touched. Real-LLM scenarios live as SKILL.md files under `tests/skills/` and are **not** in CI (real API calls / need a display). `scripts/validate-dashboard.cjs` is the dashboard/Station QA harness (drives a real browser over CDP; also not in CI). Run `cargo test --bins` and `cargo clippy` locally before committing.

## Repository Layout

```
src/
├── main.rs, agent.rs           # intendant-runtime (sandboxed executor)
├── models.rs, error.rs, utils.rs, win_sandbox.rs
├── bin/caller/                 # the intendant controller:
│   ├── main.rs                 # entry: CLI flags/help, panic hook, startup prologue + mode dispatch
│   ├── agent_loop.rs, run_modes.rs, external_mode.rs, external_supervision.rs, display_glue.rs   # carved from main.rs: the native loop + orchestration handlers; native/external mode runners; external supervision helpers; frame/CU/user-display glue
│   ├── startup/                # web bind/TLS + peer boot; the three mode branches (daemon, mcp_mode, headless)
│   ├── control_plane.rs, event.rs, frontend.rs   # single-writer state; EventBus + ControlMsg; state snapshots
│   ├── session_supervisor/, task_dispatch.rs, file_watcher.rs   # daemon: sessions (dispatch/launch/sub_agents/routing/agent_config/registry slices), dispatch, rewind snapshots
│   ├── provider/, tools.rs, prompts.rs, approval.rs   # provider/: ChatProvider + selection (mod.rs), per-provider openai/anthropic/gemini
│   ├── sub_agent.rs, worktree.rs, worktree_inventory.rs, agent_runner.rs   # native multi-agent
│   ├── context_rewind.rs, fission_ledger.rs, fission_lifecycle.rs, lineage_ledger.rs   # managed context: rewinds, fission, lineage
│   ├── external_agent/         # supervise Codex / Claude Code (+ external_wrapper_index.rs)
│   ├── access/                 # trust architecture: client keys, IAM, org roots/issuers/ORL, enrollment, peer identity policy + cert pinning, platform keystores
│   ├── credential_leases.rs, credential_egress.rs, daemon_identity.rs, connect_rendezvous.rs   # credential custody; Connect client
│   ├── peer/, web_tls.rs       # peer federation (transport, pairing; identity policy lives in access/); native HTTPS/WSS
│   ├── display/                # WebRTC: encode/{pool,vp8,h264_*}, tile/, capture/, webrtc, {x11,wayland,macos,windows}
│   ├── computer_use.rs, ax.rs, recording.rs, frames.rs
│   ├── presence.rs, live_audio.rs, audio_routing.rs, transcription.rs, quarantine.rs, schema_validator.rs
│   ├── web_gateway/                # HTTP/WS gateway: listener (accept/TLS), ws_session (WS tasks), http_dispatch (route dispatch), http, routes_{sessions,files,peers,access}, session_catalog/, settings, access_gates, input_authority, dashboard_presence, connect_bootstrap, peer_requests, agent_card, mcp_gate, static_assets
│   ├── dashboard_control/, terminal.rs, browser_workspace.rs   # dashboard tunnel (method table + wire/dispatch/api slices); PTY registry; agent browser
│   ├── mcp/, mcp_client.rs, control.rs
│   ├── transfer_store.rs, upload_store.rs, peer_file_transfer.rs   # transfer jobs; upload/attachment stores
│   ├── session_log/, session_names.rs, project.rs, app_state_pricing.rs
│   ├── sandbox.rs, daemon_log_tee.rs, diagnostics.rs, …
└── bin/connect/                # intendant-connect: hosted rendezvous (accounts, daemon claims, fleet sync, vault blobs, push, transparency log)
crates/{presence-core, presence-web, station-web}   # WASM: shared presence types/tools/dispatch, browser presence client, Station renderer
crates/intendant-platform   # OS integration leaf: platform probes/spawn (platform.rs), DisplayTarget, virtual-display mgmt (vision.rs)
crates/intendant-core       # shared-vocabulary leaf: error, autonomy, frames, net (probes, listener rebind, gateway port), peer_id, usage (TokenUsage), vitals, conversation, knowledge, skills
crates/intendant-display    # the WebRTC display pipeline (encoder pool, tiles, capture backends, per-OS input)
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
- **Tests are hermetic.** A test must never read or mutate machine state
  outside the repo checkout and its own temp dirs — CI runs on the fleet's
  real accounts, so a fixture that resolves `dirs::home_dir()` scans a
  live box (worktree roots, `~/.intendant` session stores) and its outcome
  and duration become machine state. Functions under test take their roots
  as parameters (`home: &Path`, store dirs); the transport edge resolves
  the real environment, tests inject `tempfile` dirs. A nextest lane or
  widened timeout around an environment-dependent test is a smell, not a
  fix.
- **File size budget:** keep a source file under ~3k lines of non-test code
  (4k absolute ceiling; inline `#[cfg(test)]` modules don't count against it;
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
  declared once and derived everywhere else (exemplar: the tunnel method
  table — `gateway_routes::ROUTES` tunnel columns ∪ the `CONTROL_ONLY_METHODS`
  residue in `dashboard_control/mod.rs` — drives the authorizer, the
  `features` list, and the per-method availability booleans). When a static
  frontend fallback copy is unavoidable (app.html's IAM catalog, the
  peer-profile picker), a daemon-side parity test pins its ID sets to the
  source, so a catalog change that forgets the mirror fails the suite
  instead of shipping as drift.
- WASM boundary: `serde_wasm_bindgen` with `serialize_maps_as_objects(true)`
- **Gateway API routes are declared once** in `src/bin/caller/gateway_routes.rs`
  (`ROUTES`): dispatch, the pre-dispatch IAM classification, the OPTIONS
  preflight, and the docs endpoint table in `docs/src/web-dashboard.md` all
  derive from the declaration (the HTTP instance of "derive, don't mirror").
  Never add an HTTP route by editing `web_gateway/http_dispatch.rs`'s dispatch chain —
  add a table row plus a `RouteHandlerId` match arm; the row also declares the
  request-body policy (dispatch reads and caps the body before the handler
  runs). A route's dashboard-control (datachannel) twin is declared on the
  same row: tunnel-twinned methods get the row's `tunnel:` column
  (`TunnelSpec`) — never a `CONTROL_ONLY_METHODS` entry; that table is the residue
  for tunnel-only methods, and the tunnel derives each twinned method's IAM
  operation from its row. Unit tests enforce the table invariants, pin the
  docs chapter, pin every route-specific body cap, and freeze the tunnel
  method partition (`tunnel_method_partition_is_pinned`).
- `static/app.html` is **generated** from the `static/app/` fragments (order =
  `static/app/manifest.txt`; assembled by `build.rs` via
  `crates/app-html-assembler`; CI enforces the match). Edit the fragments,
  never the artifact. Merge conflicts: resolve them in the fragments, run
  `cargo run -p app-html-assembler`, then `git add static/app.html` — never
  hand-reconcile the generated file. The dashboard ships the **ui-v2 chrome**
  (design overhaul: `ui2-*` fragments + the `16-styles-v2-tokens` palette).
  The v1 chrome, its Catppuccin palette, and the `?ui=v1` escape hatch were
  deleted post-soak; `html.ui-v2` remains as the permanent scoping namespace,
  and the tokens file's alias layer defines the Catppuccin-era var names the
  base stylesheets still consume.
- Pure-safe Rust by default. The Unix (macOS / Linux) code paths keep `unsafe`
  confined to documented islands: small platform probes/signals and display or
  identity queries in `platform.rs` (now `crates/intendant-platform`); macOS Accessibility bindings in `ax.rs`
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
commits on a feature branch.

**Landing goes through the merge queue.** `main` is ruleset-protected on
`github.com/intendant-dev/Intendant`: direct pushes are rejected for everyone, and
every landing is a PR that GitHub's merge queue validates (speculatively, against
`main` + everything queued ahead) and merges in order. The ritual from an agent
worktree:

```bash
git push origin <worktree-branch>
gh pr create --fill --head <worktree-branch>
gh pr merge --merge --auto        # enters the merge queue; merges when checks pass
```

Do not merge `main` into your branch just to "keep up" before landing — the queue
validates your PR against the future main for you; merge main into your worktree
only when you actually need newer code to build on. Still run the local battery
(`cargo test --bins`, `cargo clippy`, the relevant `tests/skills/` smokes) before
queueing: the queue gate is the deterministic subset, not the full battery, and a
red queue entry wastes everyone's cycle time. Never bypass the ruleset; if the
queue itself is wedged, that is an operator (org-owner) decision.

**Land small, land immediately.** This main takes hundreds of commits a month
from concurrent agents; every hour a green change sits unqueued is another
chance main moves under it (a real one-PR landing ate three conflict
reconciles this way, and two sessions once wrote the same fix in parallel
because neither had landed it). So: an independent fix ships as its own PR
the moment it's green — never held back to ride a batch. Two habits make the
collisions cheap:

- **Open a draft PR when you start, not when you finish** (`gh pr create
  --draft --fill`, then `gh pr ready` once green). Drafts are the fleet's
  files-in-flight signal — before touching hot files, check what's already
  in motion: `gh pr list --state open --json number,title,headRefName,isDraft,files`.
- **Auto-merge can silently disarm** (check failure, queue ejection) — after
  every conflict-resolution push or flake rerun, re-run
  `gh pr merge <n> --merge --auto` and confirm `autoMergeRequest` is set
  again. But **disarm is NOT a reliable conflict signal**: a pre-queue PR
  that goes `CONFLICTING`/`DIRTY` under a moving main can keep auto-merge
  ARMED and park forever (observed live, PR #293 2026-07-13) — any
  landing watcher must query `mergeable` + `mergeStateStatus` directly and
  treat `CONFLICTING`/`DIRTY` as terminal. While the PR sits IN the queue,
  `autoMergeRequest` nulling and `mergeStateStatus: UNKNOWN` are normal;
  only `state` (`MERGED`/`CLOSED`) is terminal. A queued branch is frozen —
  pushes are rejected until the entry merges or is dequeued.

After arming auto-merge, confirm the PR actually **enters the queue** once its
checks go green (GraphQL `pullRequest.mergeQueueEntry`). Known stall: a job
that dies mid-run (runner lost communication) and auto-recovers in place can
leave its per-commit **check run** stuck at `failure` while the workflow run
shows success — auto-merge reads the check run and waits forever. Detect it by
comparing `gh pr checks` against `gh run view`; remedy with
`gh run rerun --job <id>` to mint a fresh check run. Treat any
"green run, armed auto-merge, still not queued after ~5 minutes" as this
class of stall, not as normal latency.

**Post-landing: fast-forward the shared mirror.** The queue owns origin/main;
nothing updates the repo root's local `main` anymore. After your PR merges, run
`git -C <repo-root> pull --ff-only` (and `git merge --ff-only origin/main` in
your own worktree). External harnesses' worktree spawns and any session launched
from the repo root branch from local main's HEAD — a stale mirror silently bases
them on pre-landing code. (Intendant's own sub-agent and fission worktrees
branch from the parent checkout's `HEAD` — `worktree::create` base `"HEAD"` —
so they follow the parent: safe when the parent runs in its own worktree, but a
session whose project root is the repo root spawns children off the mirror's
HEAD like any other root-based path.) Before basing
new work on `main` — a fresh worktree, a root-checkout session — fetch and
compare first: landings from other machines can stale the mirror between your
own. If the fast-forward fails (dirty root checkout, ref lock), report it and
move on; never force it and never resolve the root checkout's state yourself.

## CI/CD

GitHub Actions on PR to `main` and — for the required checks — on every
`merge_group`. The required-check workflows run **unfiltered on `pull_request`
and `merge_group`**: GitHub only lets a PR enter the merge queue after its own
required checks pass, so a paths-skipped required check blocks queue entry
(and on the group side wedges the entry at "Expected"). The heavy workflow
(`windows.yml`) has **no push trigger at all** — a push-to-main run would
revalidate the identical tree the merge group just validated, and the
external per-listener build caches stay warm from the constant PR + group
flow. The remaining push triggers (repo-integrity, app.html, audit, docs
deploy) are cheap and paths-filtered.

Trusted refs (pushes, merge-queue refs, same-repo PRs) run on the
**self-hosted fleet** (`dell-206` = `intendant-linux`, `macbook-host` =
`intendant-macos`, `samsung-win` = `intendant-windows`) with build state
in **external per-listener cargo target caches** (`CARGO_TARGET_DIR`
under the runner account's `~/.cache/intendant-ci/`, keyed by `rustc -V`
— checkout's `git clean -ffdx` wipes an in-workspace `target/` every
job, so warmth can only live outside it) — warm gate runs are minutes,
not half-hours.
**Fork PRs route to GitHub-hosted runners instead** (dynamic `runs-on`;
`matrix.os` doubles as the hosted label): external code never executes on
our hardware, yet its required checks really run. Fork-PR workflows also
need maintainer approval before anything runs (all outside collaborators,
not just first-timers). All three platforms' runners run as dedicated
non-admin `ci` users (the Mac's `scripts/ci` service-account kit —
`_intendant-ci`: hidden role account, LaunchDaemon listeners, job hooks,
PF egress deny — now lives on the physical Mac rather than inside the
Vortex guest, so CI load no longer competes with the agents' VM), and
the check *names* stay pinned to the
`test (ubuntu-latest)`-style contexts the ruleset requires (matrix `os` is
the name key, `runner` is the fleet placement):
- **`windows.yml`** — cross-platform `cargo test` (the `intendant` bins + the `intendant-core`/`intendant-display`/`intendant-platform` lib crates) + the headless mock-provider e2e on Windows + macOS + Linux (catches platform-specific build breaks *and* Unix-only test/path assumptions; excludes the WASM crates). Full suites run in exactly two places: the **merge group** (all three platforms — the actual gate) and the **Linux `pull_request` leg** (the pre-queue runtime signal); the non-Linux PR legs are `cargo check` only, and there is no push trigger. The Linux leg is the **whole Linux gate**: after unit tests + e2e it runs the keyless smokes (session-vitals, native-goal, peer-sessions — real binaries under the mock provider) and the dashboard-boot probe (SPA booted in headless Chromium over CDP; promoted from advisory on its 40/40 soak) as tail steps reusing the same checkout and warm tree — these were smokes.yml's jobs until 2026-07-11. The Windows and Linux legs build with debuginfo off (`CARGO_PROFILE_DEV_DEBUG=0` — the Linux leg measured ~95% compile+link vs ~12s of test execution; repro locally with default debuginfo when a CI backtrace is too thin). Jobs are bounded by `timeout-minutes: 60` (no per-test timeout exists under plain `cargo test`, so a single hung test otherwise holds a queue slot indefinitely). Headless-safe: needs no display or API keys. **Required check.**
- **`app-html.yml`** — the `static/app/` fragments ↔ generated `static/app.html` regen gate. **Required check.**
- **`wasm-drift.yml`** — the committed `static/wasm-*` artifacts ↔ their crates: rebuilds both WASM crates with the pinned wasm-pack + pinned toolchain and fails on any byte difference (build.rs's mtime staleness check is blind in a fresh checkout, so drift otherwise ships silently). Runs on the **Mac fleet leg**: wasm output is not byte-deterministic across host triples (proven live 2026-07-11), and aarch64-darwin is the canonical artifact host — regenerate wasm artifacts on macOS only. In-job relevance skip: only wasm inputs (the two crates, `presence-core`, the artifacts, `Cargo.lock`, the pins) trigger the rebuild — everything else green-skips. **Required check.**
- **`agents-md-sync.yml`** — repo integrity: CLAUDE.md ↔ AGENTS.md byte-parity + actionlint over the workflow files (fleet runner labels are declared in `.github/actionlint.yaml`; shellcheck/pyflakes passes await their own baseline pass). **Required check.**
- **`audit.yml`** — `cargo audit` on push/PR plus a weekly cron (Mondays 08:00 UTC). Advisory only — new upstream advisories must not block unrelated landings.
- **`docs.yml`** — mdBook (`docs/`) deploy to GitHub Pages on push to `main`.

The `tests/skills/` scenarios that need real API calls or a display (the live
haiku claude-code-e2e, browser/Station probes, the peer smoke's `--browser` leg)
stay out of CI and run on operator hardware as the post-landing battery. Run
`cargo test --bins` and `cargo clippy` locally before committing.
