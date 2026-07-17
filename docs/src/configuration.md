# Configuration

Intendant is configured through three layers, in increasing specificity:

1. **`intendant.toml`** — in the project root for a rooted daemon, or under
   the daemon state root (`~/.intendant/intendant.toml` by default) for a
   projectless daemon such as the bundled app. The latter stores daemon-wide
   defaults without making the state directory a session project or sandbox
   root (structure in `src/bin/caller/project.rs`).
2. **Environment variables** (often via `.env`) — keys, provider/model
   overrides, and a few runtime toggles.
3. **CLI flags** — per-invocation overrides (see
   [Getting Started](./getting-started.md#cli-flag-reference)).

CLI flags win over env vars where they overlap (`--provider` sets `PROVIDER`,
`--model` sets `MODEL_NAME`). `intendant.toml` and `.env` are both git-ignored.

## Environment variables

The controller reads these from the process environment (populated from `.env`;
see [Getting Started](./getting-started.md#api-keys-env) for the search order).

### Keys and provider selection

| Variable | Alias | Default | Description |
|----------|-------|---------|-------------|
| `OPENAI_API_KEY` | `OPENAI` | — | OpenAI key |
| `ANTHROPIC_API_KEY` | `ANTHROPIC` | — | Anthropic key |
| `GEMINI_API_KEY` | `GEMINI` | — | Google AI (Gemini) key |
| `PROVIDER` | — | auto-detect | `openai`, `anthropic`, or `gemini` — which provider to use when multiple keys are set |
| `MODEL_NAME` | — | per-provider | Main model name |
| `ANTHROPIC_ENDPOINT` | — | `https://api.anthropic.com` | Anthropic API base URL override for self-hosted API-compatible gateways and proxies |
| `GEMINI_ENDPOINT` | — | `https://generativelanguage.googleapis.com` | Gemini API base URL override for self-hosted API-compatible gateways and proxies |

**Auto-detection** (when `PROVIDER` is unset): if an OpenAI key is present it is
used first, then Anthropic, then Gemini. Setting `PROVIDER` explicitly forces
that provider (and errors if its key is missing).

`PROVIDER=mock` selects the keyless scripted provider (no network calls; built
for the headless `tests/e2e/` suite and demos) and requires
`INTENDANT_MOCK_SCRIPT=<path>` — the script format is documented in
`src/bin/caller/provider_mock.rs`. It is never auto-detected.

**Per-provider default models** (used when `MODEL_NAME` is unset):

| Provider | Default model |
|----------|---------------|
| OpenAI | `gpt-5.5` |
| Anthropic | `claude-sonnet-4-5-20250929` |
| Gemini | `gemini-2.5-pro` |

### Daemon state root

| Variable | Default | Description |
|----------|---------|-------------|
| `INTENDANT_HOME` | `~/.intendant` | Overrides the daemon state root — the one directory holding session logs, the session-index cache, recordings, quarantine, leased credentials, access certs, the service pidfile, the projectless daemon's general settings (`intendant.toml`) and Connect config (`connect.toml`), the projectless upload/transfer global store (`global-store/`, pruned after 14 idle days at daemon startup), and the rest of the machine-local daemon state. The value is used verbatim as the root (no `.intendant` component is appended); a relative path resolves against the startup directory. Read once at first use and fixed for the process lifetime. Useful for scratch daemons and hermetic harnesses. Locations deliberately outside this root are unaffected: project-local `.intendant/` directories, external-agent homes (`~/.codex`, `~/.claude`), and the durable Ed25519 daemon identity private key at the OS data directory's `intendant/daemon-identity/ed25519.pk8` (0600 on Unix; the temp-directory fallback is only for platforms where no data directory resolves). |

### Model and behavior tuning

| Variable | Default | Description |
|----------|---------|-------------|
| `MODEL_CONTEXT_WINDOW` | per-model | Context window in tokens (also settable via `[model] context_window`) |
| `MAX_OUTPUT_TOKENS` | per-model | Max output tokens per API call (also `[model] max_output_tokens`) |
| `USE_NATIVE_TOOLS` | `true` | Use the provider's native tool-calling API; `false` falls back to text-based JSON extraction |
| `STRUCTURED_OUTPUT` | provider-dependent | Enable JSON object mode for deterministic parsing |
| `REASONING_EFFORT` | — | For reasoning models: `low`, `medium`, `high` |
| `REASONING_SUMMARY` | — | Reasoning summary mode: `auto`, `concise`, `detailed` |

`[model] context_window` / `max_output_tokens` from `intendant.toml` are applied
into `MODEL_CONTEXT_WINDOW` / `MAX_OUTPUT_TOKENS` only when those env vars are
not already set, so env/CLI always win.

### Presence and computer-use overrides

| Variable | Default | Description |
|----------|---------|-------------|
| `PRESENCE_PROVIDER` | falls back to `PROVIDER` | Override the presence layer's provider |
| `PRESENCE_MODEL` | falls back to `PRESENCE_PROVIDER`'s default | Override the presence model |
| `CU_PROVIDER` | falls back to `PROVIDER` | Override the computer-use model's provider |
| `CU_MODEL` | — | Override the computer-use model |

These mirror the `[presence]` and `[computer_use]` sections below; the
precedence is **explicit config > env var > auto-detect**.

### Browser workspace overrides

| Variable | Default | Description |
|----------|---------|-------------|
| `INTENDANT_BROWSER_WORKSPACE_EXECUTABLE` | managed browser cache | Explicit Chromium/Chrome-for-Testing executable for CDP browser workspaces |
| `INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER` | `false` on macOS, `true` elsewhere | On macOS, explicitly permit CDP workspaces to launch system Chrome/Chromium apps such as `/Applications/Google Chrome.app` |

The default CDP resolver prefers managed Playwright/Puppeteer/Chrome-for-Testing
browser caches and Intendant's own browser cache locations. This avoids
attributing Google Chrome updater/app-bundle activity to Intendant on macOS. Set
the explicit executable variable when a managed browser lives in a custom path,
or choose `provider=system_cdp` for a deliberate one-off system-browser launch.
Run `intendant setup browsers` to download Chrome for Testing into Intendant's
managed cache. The helper accepts `--check`, `--force`,
`--channel stable|beta|dev|canary`, `--json`, and `--print-path`; use
`--check` to verify the cache without network access.

### Dashboard dev override

| Variable | Default | Description |
|----------|---------|-------------|
| `INTENDANT_APP_HTML_PATH` | unset | Serve the dashboard entry point from this file, re-read on every request, instead of the embedded copy |

Development-only: point it at a checkout's `static/app.html`, then iterate
with `cargo run -p app-html-assembler` after each fragment edit — the next
browser refresh picks up the change with no daemon rebuild or restart. The
disk copy gets the same `?v=` asset-URL rewriting as the embedded one;
everything else (WASM, vendored JS/CSS, icons) stays embedded and still
needs a normal build. A read failure serves a loud 500 naming the override
rather than silently falling back to the embedded copy, and the gateway
logs the active override at startup.

### Session-log retention variables

| Variable | Default | Description |
|----------|---------|-------------|
| `INTENDANT_LOG_MESSAGES_JSON` | unset (off) | Write the per-turn full-conversation dump `turns/turn_NNN_messages.json`. Off by default — the context snapshot already archives the exact provider request. The dump is still written automatically, gate or no gate, when a provider cannot produce a request snapshot (mock/custom providers), so every turn keeps at least one exact input record |
| `INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL` | unset (rotate) | Keep every per-turn context-snapshot sidecar instead of rotating to the latest one per (source, session id) stream. Debugging aid — latest-only rotation is what keeps per-session context disk O(1) instead of O(turns × context) |

### Process-plumbing variables

Sub-agents no longer travel through the environment — they are supervised
sessions spawned in-process via the `spawn_sub_agent` tool (see
[Multi-Agent Orchestration](./multi-agent.md)), so the old
`INTENDANT_ROLE` / `INTENDANT_ID` / `INTENDANT_RESULT_FILE` /
`INTENDANT_PROGRESS_FILE` family is gone. What remains:

| Variable | Description |
|----------|-------------|
| `INTENDANT_TASK` | Task description fallback when no CLI task argument is given |
| `INTENDANT_SYSTEM_PROMPT` | Replaces the resolved system prompt wholesale for a direct CLI invocation (escape hatch; per-session overrides use `spawn_sub_agent`'s `system_prompt`) |
| `INTENDANT_SANDBOX_WRITE_PATHS` | Sandbox write paths (set by the caller when sandboxing; enforced by Landlock on Linux, Seatbelt on macOS, restricted tokens on Windows) |
| `INTENDANT_LOG_DIR` | Session log directory (set by the caller for the runtime) |

## `intendant.toml`

Create `intendant.toml` in your project root (the git top-level). A daemon
started without a project marker reads and writes the same schema at
`<INTENDANT_HOME>/intendant.toml`; the dashboard's Settings page uses that
daemon-wide file while `/api/project-root` remains null. Every section is
optional; an absent section uses its defaults. The structure and defaults below
are taken directly from `src/bin/caller/project.rs`.

### `[memory]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable the persistent project memory store (`.intendant/memory.json`) |

### `[model]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `context_window` | int | per-model | Override the model's context window (tokens) |
| `max_output_tokens` | int | per-model | Override max output tokens per call |

### `[orchestrator]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_parallel_agents` | int | `4` | Cap on concurrently *running* sub-agent children per parent session; `spawn_sub_agent` refuses beyond it |

### `[approval]`

Per-category approval rules. Each value is `auto` (run without asking), `ask`
(prompt the human), or `deny` (refuse). These are layered under the global
`--autonomy` level — see [Autonomy and approval](#autonomy-and-approval).

| Key | Type | Default | Category |
|-----|------|---------|----------|
| `file_read` | rule | `auto` | FileRead |
| `file_write` | rule | `ask` | FileWrite |
| `file_delete` | rule | `ask` | FileDelete |
| `command_exec` | rule | `auto` | CommandExec |
| `network` | rule | `auto` | NetworkRequest |
| `destructive` | rule | `ask` | Destructive |
| `display_control` | rule | `ask` | DisplayControl |

The `HumanInput` and `LiveAudioSpawn` categories always require a human and are
not configurable here.

### `[presence]`

The conversational presence layer that mediates between you and the worker
agent (see [Presence Layer](./presence.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable the presence layer |
| `provider` | string | auto-detect | Provider for text-mode presence |
| `model` | string | `gemini-3-flash-preview` | Text-mode presence model |
| `context_window` | int | `1048576` | Context window for text-mode presence |
| `live_provider` | string | auto-detect | Provider for browser-side live (voice) presence |
| `live_model` | string | provider default | Live presence model |
| `live_context_window` | int | `32768` | Context window for live presence |

> The compiled-in default text presence model is `gemini-3-flash-preview`. Text
> presence auto-detection prefers Gemini when `GEMINI_API_KEY` is set.

### `[transcription]`

Server-side speech-to-text via the Whisper API (or a compatible endpoint). See
[Web Dashboard](./web-dashboard.md#server-side-transcription).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` (within a present section) | Enable server-side transcription |
| `provider` | string | `openai` | Transcription provider |
| `model` | string | `whisper-1` | Transcription model |
| `endpoint` | string | OpenAI default | Custom endpoint URL (e.g. self-hosted whisper.cpp) |
| `language` | string | auto-detect | ISO-639-1 language hint |
| `buffer_secs` | float | `3.0` | Audio buffered before each API call (seconds) |

> Note on the `enabled` default: when the entire `[transcription]` section is
> **absent**, the field's struct default applies. When the section is
> **present** but `enabled` is omitted, the bool defaults to `false`. The CLI
> `--transcription` flag and a present `enabled = true` both turn it on.

### `[recording]`

ffmpeg-based recording of agent displays (see
[Integrations](./integrations.md#recording-ffmpeg)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable display recording |
| `framerate` | int | `15` | Capture frames per second |
| `segment_duration_secs` | int | `60` | Length of each recording segment |
| `quality` | string | `medium` | `low` (CRF 35), `medium` (CRF 28), `high` (CRF 20) |
| `max_retention_hours` | int | unset | Auto-delete segments older than this |

### `[computer_use]`

Provider/model used for visual-grounding (computer-use) tasks. See
[Computer Use & Live Audio](./computer-use-and-audio.md). The separate
provider/model selection matters for frame-grounded CU dispatches and for
the vaulted CU-first routing (below); the dashboard hides the selection
rows while that routing is off.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `provider` | string | auto-detect | CU model provider |
| `model` | string | auto-detect | CU model |
| `backend` | string | `auto` | Input/screenshot backend: `x11`, `wayland`, `macos`, or `auto` |

### `[experimental]`

Vaulted features: kept runnable in the tree, all off by default, and
production behavior must not depend on them.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cu_first_routing` | bool | `false` | Intercept every non-direct task with a fast CU model that completes it on the display or escalates to the main agent (vaulted 2026-07-04: adds a model hop to every task and, under subscription-based external agents, an API-key model dependency) |

### `[agent]` and external backends

Routes coding tasks to an external CLI agent instead of the native loop (see
[Integrations](./integrations.md#external-coding-agent-clis)).

`[agent]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `default_backend` | string | unset (use native) | `codex` or `claude-code` |

`[agent.codex]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `command` | string | `codex` | Path or command name for the Codex binary |
| `model` | string | unset | Model override |
| `approval_policy` | string | `on-request` | `untrusted`, `on-request`, or `never` (UI set; `on-failure` is deprecated upstream) |
| `sandbox` | string | `workspace-write` | `read-only`, `workspace-write`, or `danger-full-access` |
| `reasoning_effort` | string | unset (model default) | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or `ultra` (model-dependent; `ultra` enables automatic task delegation on supporting Codex models) |
| `service_tier` | string | unset (inherit Codex default) | `priority` enables Fast, `flex` requests Flex, `standard` is a sentinel that sends an explicit `serviceTier: null` to opt managed sessions out of Fast |
| `web_search` | bool | `false` | Enable the Responses-API `web_search` tool (`codex --search`) |
| `network_access` | bool | `false` | Allow outbound network in `workspace-write` sandbox (ignored for `read-only` / `danger-full-access`) |
| `writable_roots` | array | `[]` | Extra writable roots, each passed as `--add-dir` (absolute, or resolved against project root) |
| `managed_context` | string | `vanilla` | `vanilla` for upstream/original-fork Codex; `managed` enables proactive Intendant context densification, rewind/backout tools, disables Codex auto-compaction, and requires the patched Codex app-server protocol with lineage prompt-cache-key support |
| `context_archive` | string | `summary` | Context snapshot archive mode ("Context replay" in the UI): `summary` records compact per-request visualization data with temporary provider traces, `exact` persists full provider request payloads for raw replay, `off` disables capture |

`context_recovery` is accepted as a deprecated TOML alias for
`managed_context`. New configs must use `managed_context`.

Codex `app-server` launches in `managed_context = "managed"` suppress inherited
user-global Codex MCP/plugin/app servers by default and inject Intendant's MCP
endpoint plus the explicit toggles above. Set
`INTENDANT_CODEX_INHERIT_MCP_SERVERS=1` only for a managed launch that should
inherit the user's configured Codex MCP servers and plugins. Vanilla launches
preserve Codex's normal user configuration inheritance.

`[agent.claude_code]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `command` | string | `claude` | Path or command name |
| `model` | string | unset | Model override — an alias (`fable`, `opus`, `sonnet`, `haiku`; the CLI resolves it to the latest model) or a full model id |
| `permission_mode` | string | `default` | `default` (alias `manual`), `acceptEdits`, `plan`, `auto` (classifier-based approvals), `dontAsk` (auto-deny anything that would prompt), `bypassPermissions`. **Semantics change:** before Claude Code 2.1.206 Intendant coerced `auto` to `default`; it now selects the CLI's real auto-approval mode, and the config load warns when it finds `auto` so an old config doesn't escalate silently |
| `effort` | string | unset | Reasoning effort passed as `--effort`: `low`, `medium`, `high`, `xhigh`, `max`, `ultracode` (unset omits the flag) |
| `allowed_tools` | array | `[]` (all) | Restrict the tool set |
| `max_budget_usd` | float | unset | CLI-enforced dollar backstop passed as `--max-budget-usd`; cumulative for the session, its resumes, AND its forks — a forked or `/btw` side child inherits the parent's counted spend (probed 2.1.206), so children of an exhausted parent fail immediately with the same hint. On exceed every further turn fails with `error_max_budget_usd` (surfaced as a backend error with a recovery hint). Must be positive: a zero/negative/non-finite value refuses the spawn instead of silently disarming (the CLI itself rejects `--max-budget-usd 0`) |

Unknown or empty values for `approval_policy`, `sandbox`, `reasoning_effort`,
are normalized to the safe default so a config typo cannot silently escalate
privileges.

### `[live_audio]`

Untrusted voice sub-agent (zero tools, schema-validated) used for phone calls
and live voice (see [Computer Use & Live Audio](./computer-use-and-audio.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable live-audio sessions |
| `default_timeout_secs` | int | `300` | Session timeout |
| `gemini_model` | string | unset | Gemini Live model |
| `openai_model` | string | unset | OpenAI Realtime model |
| `sample_rate` | int | `24000` | Audio sample rate (Hz) |

### `[sandbox]`

Filesystem sandboxing for the runtime — Landlock on Linux, Seatbelt on
macOS, restricted tokens on Windows. Also enabled by `--sandbox`.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable filesystem sandboxing |
| `extra_write_paths` | array | `[]` | Extra writable paths beyond project root, the OS scratch dir (`/tmp` / `%TEMP%`), the log dir, and `~/.intendant` |

On Linux kernels without Landlock support, sandboxing is silently skipped;
on macOS and Windows a sandbox that fails to apply fails the run rather
than continuing unconfined.

### `[webrtc]`

ICE servers for the WebRTC display transport (see
[Display Pipeline](./display-pipeline.md)).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ice_servers` | array of tables | `[]` (local-only) | STUN/TURN servers |
| `federation_allow_h264` | bool | `false` | Allow the federated (peer-to-peer) display path to negotiate H.264; default pins VP8 for lossy TURN-relayed paths. The local same-machine path is unaffected |

Each `ice_servers` entry: `urls` (array, required), optional `username`,
optional `credential`.

### `[connect]`

Intendant Connect client for public-origin account/route discovery. This is
disabled by default and does **not** replace local/offline dashboard mTLS.
When enabled, the daemon registers signed presence and route-code metadata and
polls the rendezvous mailbox. The default binary refuses every hosted-origin
control event before touching dashboard-control, IAM, or enrollment state. The
current service also returns `403` for browser offer/ICE/close before mutation.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable outbound Connect rendezvous polling |
| `rendezvous_url` | string | `https://connect.intendant.dev` when `enabled` | Base URL of the Connect/rendezvous service (the hosted instance unless overridden) |
| `daemon_id` | string | daemon identity public key | Public daemon id at the rendezvous service |
| `auth_token` | string | unset | Optional bearer token for daemon-to-service authentication; not dashboard authorization |
| `poll_timeout_ms` | integer | `15000` | Long-poll timeout per daemon `/next` request |
| `retry_delay_ms` | integer | `1000` | Delay after transient rendezvous errors |
| `relay_enabled` | bool | `false` | Hold a reachability-relay control channel to Connect so this daemon's NAT'd fleet name is reachable through the relay's SNI passthrough (docs/src/self-hosted-rendezvous.md). Requires `relay_endpoint` |
| `relay_endpoint` | string | unset | `host:port` of the relay's raw passthrough port, where this daemon dials back browser connections (e.g. `relay.example.com:443`) |

`INTENDANT_CONNECT_RELAY_ENDPOINT` force-enables the relay tunnel and sets
`relay_endpoint`. The relay is availability-only: it never terminates TLS,
holds no certificate, and grants no authority — a relayed browser connection
reaches this daemon bearing the fleet SNI and stays discovery-only.

No file editing is required for the common case: the dashboard's
**Access → Intendant Connect** card toggles `enabled` (persisting it
here), shows registration/link state, reveals the single-use twelve-word
locally minted claim code to trusted manage-grade sessions, and can release the
account/route link. The
`INTENDANT_CONNECT_RENDEZVOUS_URL` environment variable still force-enables
the client and overrides the file (the card reports when it does).

**Projectless daemons** (the bundled macOS app's normal shape — no
`.git`/`intendant.toml` at the launch directory) keep general defaults in the
daemon-scoped `intendant.toml`, but Connect's credential-bearing table remains
in its dedicated owner-only `connect.toml` beside it. The toggle and daemon boot
both use that dedicated file. Rooted daemons are unaffected.

Hosted Connect uses the same daemon-side settings:

```toml
[connect]
enabled = true
rendezvous_url = "https://connect.intendant.dev"
daemon_id = "vortex-deb-x11-intendant"
auth_token = "same daemon token configured on intendant-connect"
```

The service is a separate binary:

```bash
INTENDANT_CONNECT_TOKEN="shared daemon bearer token" \
  ./target/release/intendant-connect \
    --listen 127.0.0.1:9876 \
    --origin https://connect.intendant.dev \
    --rp-id intendant.dev \
    --data-file <state-file>
```

`--static-root PATH` and `INTENDANT_CONNECT_STATIC_ROOT` are deprecated
compatibility inputs. They are accepted (a missing flag value still errors)
but ignored. The default Connect binary serves only explicit, compile-time
embedded routes and assets; it never mounts a filesystem fallback. In
particular, a supplied directory cannot expose `app.html`, WASM, or
`vault-kernel.js` from the hosted origin.

The service-side reachability relay is off by default and gated on an
all-or-nothing flag group (mirroring the `--dns-*` fleet-DNS group):

| Flag / env | Description |
|-----|-----|
| `--relay-listen` / `INTENDANT_CONNECT_RELAY_LISTEN` | Raw TCP listen address for the SNI-passthrough relay (e.g. `0.0.0.0:443`). Must receive raw TLS — do not front it with a TLS-terminating proxy |
| `--relay-address` / `INTENDANT_CONNECT_RELAY_ADDRESS` | Comma-separated public address(es) the relay is reachable at, published in fleet DNS for relay-mode daemons |

Both are required together or neither. See
docs/src/self-hosted-rendezvous.md for the full deployment description.

`intendant-connect` is intended to sit behind public TLS for the configured
origin. It handles passkey-only account sessions, single-use account/route linking,
daemon list/release/label UI, account-backed fleet target metadata, and a capped
audit log. A live claim-linked daemon exposes no control URL or Open action;
`/app` always redirects to `/connect`. A separately remembered, client-signed
and passkey-decrypted direct route may show **Open direct route**, which merely
navigates away from Connect to that daemon's mTLS origin and grants nothing by
itself. A Connect account assertion never authenticates to the
daemon, and no grant or configuration edit can enable hosted control. The state file durably stores users/passkeys, daemon account
links, labels, hashed claim codes, account-scoped fleet navigation records, and
audit events. The daemon locally generates each single-use 12-word BIP39 code
and signs a fresh registration containing only its SHA-256 hash. Its printed
URL carries the phrase in `/connect#claim_code=...`; the browser scrubs the
fragment, hashes locally, and sends only the digest. Connect never receives or
returns plaintext, and its claim API rejects plaintext/query compatibility.
WebAuthn challenge state, rate limits, web sessions, and short-lived
daemon-session credentials are memory-only.

For new `*.intendant.dev` deployments, the default RP ID is `intendant.dev`, so
passkeys can be scoped to the owned parent domain. The current
`connect.intendant.dev` production-alpha instance was originally launched with
`INTENDANT_CONNECT_RP_ID=connect.intendant.dev`; keep that setting until the
alpha account is deliberately migrated, because changing RP ID invalidates
previously registered passkeys and requires users to register new credentials.

For production alpha, terminate public TLS at a reverse proxy and keep
`intendant-connect` bound to `127.0.0.1`. The proxy should forward `Host`, set
`X-Forwarded-For`/`X-Real-IP`, and strip any inbound copies of those headers
before setting them because the service uses them for simple rate-limit buckets.
Use `/healthz` for liveness and `/readyz` for readiness. Back up the state file
and store `INTENDANT_CONNECT_TOKEN` in the deployment secret store.

Cookie-backed user mutations require a same-origin request and the per-session
CSRF token returned by `/api/me`. The bundled Connect UI sets that header
automatically, including hosted fleet sync
through `/api/fleet/targets/sync` and fleet-record deletion through
`/api/fleet/targets/{target_id}/forget`.

Production-alpha operations are intentionally boring and repeatable, but live
target details are intentionally not tracked in this public repository. Keep the
host, SSH user, key path, remote source directory, systemd service name, state
file, and local readiness URL in a private operator env file or pass them with
the matching command-line flags:

```bash
cat > ~/.config/intendant/connect-prod-alpha.env <<'EOF'
CONNECT_HOST=<ssh-host>
CONNECT_SSH_USER=<ssh-user>
CONNECT_SSH_KEY=<private-ssh-key-path>
CONNECT_REMOTE_SOURCE=<remote-source-directory>
CONNECT_SERVICE=<systemd-service-name>
CONNECT_REMOTE_READYZ_URL=<local-readiness-url>
CONNECT_REMOTE_STATE=<remote-state-json-path>
CONNECT_PUBLIC_ORIGIN=https://connect.intendant.dev
EOF

CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/deploy-connect-prod-alpha.sh
```

The deploy script refuses to run without the private target values. It does not
copy or print the daemon bearer token; the token remains in the remote systemd
environment.

Backups should be encrypted because the state file is the authoritative account
and route registry. It stores account handles, passkey public-key records,
daemon account links and labels, hashed claim codes, and audit entries. It does
not store plaintext claim codes, WebAuthn challenges, active browser sessions,
pending offers, routing tokens, or rate-limit buckets. Create an encrypted
backup with:

```bash
CONNECT_OPS_ENV=~/.config/intendant/connect-prod-alpha.env \
  scripts/connect-state-backup.sh \
  --passphrase-file ~/.config/intendant/connect-backup.passphrase
```

The backup is written under
`~/.local/share/intendant/connect-backups/` by default, alongside a SHA-256
checksum file. Plaintext backup is available only with an explicit
`--allow-plaintext` flag for local diagnostics.

Restore is deliberately explicit because it replaces the production state file,
takes a pre-restore backup on the host, restarts the service, and verifies
readiness:

```bash
scripts/connect-state-restore.sh --yes \
  --passphrase-file ~/.config/intendant/connect-backup.passphrase \
  ~/.local/share/intendant/connect-backups/intendant-connect-state-YYYYMMDDTHHMMSSZ.json.enc
```

After a restore, existing Connect account sessions are gone because those web
sessions are memory-only. Linked daemon routes remain associated with the
restored account state. Direct daemon dashboards are separate and are not
Connect sessions. The restored association is not daemon ownership or IAM
authority.

The service accepts these deployment flags and equivalent environment variables:

| Env | Equivalent flag | Default | Description |
|-----|-----------------|---------|-------------|
| `INTENDANT_CONNECT_LISTEN` | `--listen` | `127.0.0.1:9876` | HTTP listen address |
| `INTENDANT_CONNECT_ORIGIN` | `--origin` | `http://localhost:<port>` | Public browser origin for redirects, install snippets, and WebAuthn origin checks |
| `INTENDANT_CONNECT_RP_ID` | `--rp-id` | origin host, or `intendant.dev` for `*.intendant.dev` | WebAuthn relying-party id |
| `INTENDANT_CONNECT_STATIC_ROOT` | `--static-root` | ignored | Deprecated compatibility input. Accepted but never read or mounted; Connect serves only its explicit embedded allowlist |
| `INTENDANT_CONNECT_DATA_FILE` | `--data-file` | platform data dir `intendant/connect/state.json` | JSON state file |
| `INTENDANT_CONNECT_TOKEN` | `--daemon-token` | unset | Shared deployment bearer for registration unless open registration is enabled; still guards admin surfaces. It does not replace the daemon's signed registration proof or rotating daemon-session credential |
| `INTENDANT_CONNECT_INVITE_REQUIRED` | `--invite-required` | `false` | Require a valid invite code for new account registration |
| `INTENDANT_CONNECT_OPEN_REGISTRATION` | `--open-registration` | `false` | Skip only the shared deployment bearer on registration. Daemons still sign a fresh key-possession proof, and successful registration rotates a short-lived daemon-session credential required by poll/answer/error/dry/claim-proof endpoints |

The service also accepts environment-only operational overrides for
self-hosting and tests:

| Env | Default | Description |
|-----|---------|-------------|
| `INTENDANT_CONNECT_DOH_URL` | `https://cloudflare-dns.com/dns-query` | DNS-over-HTTPS endpoint used for `_intendant.<domain>` TXT attestation |
| `INTENDANT_CONNECT_GIST_BASE` | `https://gist.githubusercontent.com/` | Allowed raw gist URL prefix for GitHub handle attestation |
| `INTENDANT_CONNECT_PRESENCE_OFFLINE_MS` | `180000` | Linked daemon polling gap that counts as offline for presence alerts |
| `INTENDANT_CONNECT_PRESENCE_POLL_MS` | `30000` | Presence-alert monitor poll interval |
| `INTENDANT_CONNECT_RECLAIM_AFTER_MS` | `0` (off) | Dormant-handle reclamation threshold for accounts with no linked daemon routes and no recent sign-in |
| `INTENDANT_CONNECT_RECLAIM_POLL_MS` | `21600000` | Dormant-handle reclamation poll interval |

For local E2E without editing `intendant.toml`, the daemon also accepts
environment overrides:

```bash
INTENDANT_CONNECT_RENDEZVOUS_URL=http://127.0.0.1:9876 \
INTENDANT_CONNECT_DAEMON_ID=connect-e2e-daemon \
INTENDANT_CONNECT_TOKEN=shared-dev-token \
  ./target/release/intendant --web 8876
```

There are three committed validators. The hosted production-alpha validator
starts `intendant-connect`, a daemon, and a browser virtual authenticator:

```bash
node scripts/validate-connect-hosted-mvp.cjs
```

The protocol-emulator validator starts a local rendezvous HTTP origin:

```bash
PLAYWRIGHT_NODE_PATH=/path/to/node_modules \
  node scripts/validate-connect-rendezvous.cjs
```

The emulator intentionally has no account signup, passkey ceremony, claim code,
durable device registry, or authorization policy of its own. Its hosted-origin
pass is now negative-only: it injects legacy control events and expects the
daemon to refuse them before mutation. Direct/local validators cover
authenticated dashboard-control transport separately. The hosted MVP validator
covers account/passkey flow, authority-free route linking, service-side `403`
with no enqueue, release, and audit.

The transport validator drives the fresh-box ceremony end to end — passkey
signup, fragment-only one-time discovery link, retired `/app` redirect, and
service/daemon refusal — and asserts that even a local browser-key grant cannot
create a hosted control session. Lower-level direct/local harnesses cover the ICE/DataChannel paths
(see
[Self-Hosted Rendezvous → End-to-end transport validation](./self-hosted-rendezvous.md#end-to-end-transport-validation)):

```bash
node scripts/connect-transport-e2e.cjs
```

### `[server]` (daemon and federation)

What this daemon advertises to peers and requires of inbound connections. Most
deployments only ever touch `[server.tls]`.

`[server]`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `bind` | string/IP | wildcard dual-stack, then `0.0.0.0` fallback | IP address the dashboard listens on. Use `127.0.0.1` or a specific interface for local/plaintext automation |
| `advertise` | array | `[]` (auto-detect) | WebSocket URLs to advertise in this daemon's Agent Card, preference order. Repeatable CLI `--advertise-url` replaces this list entirely. The selected CLI or config list is prepended to auto-detected fallback URLs |

`[server.tls]` — native TLS-only HTTPS/WSS for the dashboard (pure-Rust
`rustls` + `rcgen`, all platforms; ORed with the `--tls` flag). The dashboard
defaults to mTLS. This section supplies HTTPS/WSS and a browser secure context,
but it does not give a certless remote browser daemon authority: only loopback
certless requests receive the local-root posture, and protected remote
HTTP/WebSocket/control requests still require mTLS:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable HTTPS/WSS; certless access is local-root only on loopback, while remote protected routes still require mTLS |
| `cert` | string | installed access certs, then auto self-signed | PEM cert (chain) overriding the default cert selection; pair with `key` |
| `key` | string | — | PEM private key (PKCS#8, PKCS#1, or SEC1) matching `cert` |
| `hostname` | string | — | Extra SAN hostname for the self-signed cert (in addition to bind IP + `localhost`) |

When TLS-only mode is enabled and `cert`/`key` are omitted, Intendant first looks
for the installed access server certificate in the per-user platform cert
directory (`server.crt` / `server.key`, normally created by `intendant access
setup`). If that pair is absent, it falls back to an ephemeral self-signed
certificate.

`[server.mtls]` — native client-certificate authentication for the dashboard
(ORed with the `--mtls` flag; this is the default dashboard transport):

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Explicitly require browser/client certificates during the TLS handshake; default behavior already does this unless `--tls` / `[server.tls]` or `--no-tls` is used |
| `ca` | string | installed access CA | PEM CA bundle used to verify client certificates |

Use `[server.tls]` for a secure context on loopback or for public/bootstrap
content, not as a remote authentication bypass. Use default mTLS or
`[server.mtls]` for a remote dashboard with daemon authority.

Use default mTLS, `[server.tls]`, or `--tls` when a remote browser needs
secure-context-gated features: Station WebGPU,
microphone/camera, browser screen capture, or stricter clipboard APIs. An HTTPS
reverse proxy can provide a secure context for public bytes, but ordinary TLS
termination does **not** authenticate a controller. A proxy that forwards to
daemon loopback becomes a root trust boundary: remote control is safe only when
the proxy enforces an approved client identity and its upstream is protected
from other local callers. A local development build of the packaged macOS app
also supplies a secure context for its bundled daemon, but the current unsigned
artifact is not a distribution anchor. Plain `http://<host-ip>` is not enough for those APIs, and
`--no-tls` on a wildcard listener refuses startup when the host has a public
interface unless `--allow-public-plaintext` is passed. The macOS
app wrapper starts its bundled backend with native mTLS by default and fails
closed with setup guidance when access certs are missing; see
[Web Dashboard: Secure Browser Contexts](./web-dashboard.md#secure-browser-contexts).
Neither `--tls` nor `--allow-public-plaintext` synthesizes `TrustedLocal` for a
remote caller. A custom `Origin` is routing metadata, not authentication.

Peer access requests use the unauthenticated
`/api/peer-pairing/requests` doorbell endpoint so one daemon can ask another for
pairing approval. It is bounded and rate-limited, and approval still happens
locally before any client certificate is issued. Set
`INTENDANT_PEER_ACCESS_REQUESTS=0` to disable public request creation entirely.

`[server.peer_access_requests]` — public access-request hardening:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Allow unauthenticated callers to create bounded pending peer access requests; `INTENDANT_PEER_ACCESS_REQUESTS=0` still disables this at runtime |
| `body_limit_bytes` | integer | `4096` | Maximum body size for `POST /api/peer-pairing/requests` |
| `ttl_secs` | integer | `600` | Lifetime of a pending request before it expires |
| `max_pending` | integer | `32` | Global cap on simultaneously pending requests |
| `max_pending_per_source` | integer | `5` | Cap on simultaneously pending requests from one source IP/hint |
| `rate_limit_window_secs` | integer | `60` | Sliding-window duration for create-rate limits |
| `max_creates_per_window` | integer | `64` | Global request creations allowed per rate-limit window |
| `max_creates_per_source_per_window` | integer | `8` | Request creations allowed from one source IP/hint per rate-limit window |

`[server.auth]` — advanced compatibility auth for federation peers:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `advertised_transport` | string | `none` | What the Agent Card advertises: `none`, `mutual-tls`, or `pin-self-cert` |
| `bearer_token` | string | none | Legacy/advanced: require `Authorization: Bearer <token>` on inbound HTTP/WS; prefer mTLS/client certificates for normal access |

### `[[peer]]` — federated peers

Each `[[peer]]` block auto-registers a remote daemon at startup. Only `card_url`
is required.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `card_url` | string | (required) | URL of the peer's Agent Card (`.../.well-known/agent-card.json`) |
| `label` | string | from card | Display label override in the dashboard's Access targets |
| `bearer_token` | string | none | Legacy/advanced outbound token for peers that still require `[server.auth] bearer_token` |
| `via_urls` | array | `[]` | Connecting-side WebSocket URL overrides; when set, these replace the transports advertised by the peer's Agent Card |
| `client_cert` | string | installed access client cert when present | Peer-issued client certificate PEM for outbound mTLS; must be paired with `client_key` |
| `client_key` | string | installed access client key when present | Private key PEM for `client_cert`; must be paired with `client_cert` |
| `pinned_fingerprints` | array | `[]` | Operator-pinned SHA-256 cert fingerprints; when set, replaces the card's `auth.transport` claim |
| `browser_tcp_via_url` | string | from primary | Explicit URL the browser uses to reach this peer's HTTP port for WebRTC ICE-TCP |

Manual dashboard additions live only in the in-memory registry unless the
operator checks **Save to intendant.toml**. Pairing flows are durable by default:
`intendant peer join <invite>` and `intendant peer complete <request-id>` write
these fields plus `pinned_fingerprints` to `intendant.toml`. For independent
mTLS daemons, configure `client_cert` / `client_key` with a client identity
issued by the peer's access CA. The installed local access client cert fallback
is only sufficient when the peer trusts the same issuing CA.

These entries describe daemon-to-daemon peer routes. They do not grant browser
or Connect user access by themselves. Peer profiles use names such as
`peer-operator` and `peer-root`; older `operator`, `admin-peer`, and
`peer-daemon` values are still accepted as aliases.

In the Access UI and `/api/access/overview`, a `[[peer]]` entry appears as a
peer-daemon principal with a peer-profile grant to a daemon target. Browser mTLS
access is a user/client grant instead; browser-key rows are record-only in this
alpha, and a Connect passkey authenticates only the hosted account and route UI.
The same page shows both kinds of records, but the config entry only persists the
peer route and its daemon-to-daemon credentials. Peer profiles are not human IAM:
`peer-root` maps to peer inspection/management and access inspection, while
human/account access management remains owner/root user-client authority.

### Local Access/IAM state

Local IAM foundation state is stored beside the native access certificates,
normally `~/.intendant/access-certs/iam.json` on Unix-like platforms. It is not
part of `intendant.toml` because it belongs to the local daemon identity and may
later contain per-device/user audit metadata rather than project configuration.

Schema version 2 contains:

| Field | Meaning |
|---|---|
| `schema_version` | State schema version; currently `2` |
| `principals` | Local managed human/device principal records |
| `roles` | Built-in or local role templates |
| `grants` | Local IAM grant records targeting daemon IDs (optional `expires_at_unix_ms` stops enforcement after that instant; shown as `expired`) |
| `audit_events` | Local IAM audit metadata |
| `role_ceilings` | Per-binding-kind effective-role caps for low-provenance sessions (see below) |
| `hosted_origins` | Origins treated as hosted app sources when recorded on client-key bindings |
| `trusted_orgs` | Organizations whose signed grant documents this daemon accepts, each with a local `max_role` cap (implemented; see [Trust Architecture](./trust-architecture.md)) |

The daemon loads this file into `/api/access/overview` under the `iam` object
and exposes the raw state through `GET /api/access/iam/state`. Root dashboard
sessions, peer daemon profiles, and active scoped user/client grants pass
through the IAM operation evaluator. Shipped alpha browser authentication is
loopback/local presence or a browser mTLS certificate presented over an
independently verified direct daemon route.
`client_key` WebCrypto records remain in the schema for fleet signatures,
attribution, migration, and future identity work, but direct `/ws`, direct
dashboard-control offers, and the reserved future native-bridge code do not
authenticate them. A
combined `human_user` principal may carry those records plus optional
account/provider and organization metadata. `connect_account` records are
also inert compatibility/audit metadata and never authenticate.
Loopback sessions and the verified owner browser mTLS certificate keep the
root-compatible fallback so direct/self-hosted access remains first-class.
Certless remote TLS/plaintext requests do not. Connect has no dashboard-session
path at all: the service rejects browser signaling and the daemon drops legacy
events before key/grant resolution. A key record in `draft` or `revoked`
status denies instead of falling back to anything else. Root users can create
these records through the Access UI,
`POST /api/access/iam/user-client-grants`, or dashboard-control
`api_access_iam_upsert_user_client_grant`; existing records can be activated,
drafted, revoked, or role-changed through `POST /api/access/iam/grants/update`
or dashboard-control `api_access_iam_update_grant`.

The user-client grant upsert request accepts `kind = "client_key"`,
`"browser_certificate"`, `"human_user"`, `"agent_session"`, or
`"local_process"`. `human_user` is the local IAM shape for a real person. In the
alpha only its mTLS binding is an active browser authenticator; a browser-key
binding is record-only. `connect_account` remains readable in the IAM vocabulary
for compatibility and audit, but grant upsert rejects it without mutating IAM
state: Connect account links are metadata only and cannot receive a role.
`client_key` grant records take `client_key_fingerprint` (base64url, case-sensitive),
an optional `client_key` public key for audit, and an optional
`client_key_origin` recorded by the trusted session that creates the grant. The
optional `account_provider`, `verified_provider`, `handle`, `organization_id`, and
`organization_name` fields are local metadata today; the hosted Connect
service does not yet verify OAuth providers or organization membership.

**Hosted-provenance compatibility state.** `role_ceilings` retains the two
historical binding categories and serializes as

```json
{ "connect_account": "role:none", "client_key": "role:none" }
```

A hosted-origin key may have a grant record, but Connect cannot present or
exercise it in the default build. Legacy events are rejected before grant
resolution, and the compatibility map remains fail-closed. Client
keys whose recorded enrollment origin is in `hosted_origins` (default
`["https://connect.intendant.dev"]`) are also hosted, as is the retired
`connect-bootstrap` origin. Every load normalizes both entries to `role:none`;
missing, empty, or hand-edited entries cannot enable hosted control.

A typed direct address is trustless first contact, while a fleet-certificate
name is daemon-served *code* on a
rendezvous-named *route* ([first-contact rung
two](./trust-tiers.md#first-contact-three-rungs)). Adding an exact origin to
`hosted_origins` now refuses it at `role:none`; it is not a lower-authority
control mode.

The former hosted-control cap UI and mutation route are retired. `role:none` is
a zero-permission, ceiling-only builtin — it can never be granted to a
principal — and compiled policy is the authority for hosted provenance rather
than the persisted map.

IAM schema v2 migrates earlier alpha state fail-closed. It revokes every
active grant on a principal whose client-key origin is `connect-bootstrap`,
records a `revoke_legacy_connect_bootstrap` audit event, and restores both
hosted ceilings to `role:none`. Direct root grants survive, and the Connect
account/route link remains discovery metadata, but the legacy browser key
requires trusted re-enrollment.

During an alpha upgrade, restarting only the service cannot tear down an
already-established legacy P2P DataChannel. Upgrade/restart the daemon, close
old Connect tabs, and let the schema-v2 migration revoke legacy bootstrap
grants. New mixed-version attempts are blocked at both service and daemon.

The enforced built-in user/client roles are `role:scoped-human`,
`role:observer`, `role:session-reader`, `role:terminal`, `role:files-read`,
`role:files-write`, `role:operator`, and `role:root`. `role:peer-profile` is
visible in the same Access overview so daemon peer grants and human grants can
be compared, but it is daemon-to-daemon only and cannot be assigned to a
browser certificate or Connect account.

Terminal capability is three separate permissions: `terminal.view` (attach to
a visible session, scrollback + live output), `terminal.write` (type into,
resize, or close a visible session), and `shell.spawn` (create new shells).
`role:terminal` carries view+write only — a collaborator role; spawning is
reserved for `role:operator` and above. Shell sessions belong to the
principal that spawned them and are private by default; the owner (or a root
session) can mark one shared from the Terminal tab, which is what makes it
visible to other principals. The pre-split aggregate id `terminal.use` is
still honored in custom roles and org grant caps as implying all three.

### `mcp_servers`

External MCP servers to connect to as a client (see
[Integrations](./integrations.md#mcp-client) and the trust note below). Each is
an array-of-tables entry.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | (required) | Server name; tools are exposed as `mcp__<name>_<tool>` |
| `command` | string | (required) | Executable to spawn |
| `args` | array | `[]` | Arguments |
| `env` | table | `{}` | Environment for the child process |

> **Trust model:** an `mcp_servers` entry is spawned as a child process with
> your full privileges (`Command::new(command).args(args)`). Intendant performs
> **no** checksum, signature, or sandbox check on it — adding one is equivalent
> to adding a line to your `~/.zshrc` that runs a binary. The default is
> `mcp_servers = []`, and `intendant.toml` is git-ignored, so the repo ships no
> MCP servers. Treat copying an `intendant.toml` between machines like copying
> shell rc files: read it before sourcing. See [MCP Server](./mcp-server.md).

## Worked example

A reasonably full `intendant.toml`:

```toml
[memory]
enabled = true

[model]
context_window = 200000
max_output_tokens = 8192

[orchestrator]
max_parallel_agents = 4

[approval]
file_read = "auto"
file_write = "ask"
file_delete = "ask"
command_exec = "auto"
network = "auto"
destructive = "ask"
display_control = "ask"

[presence]
enabled = true
provider = "gemini"
model = "gemini-3-flash-preview"
context_window = 1048576
live_provider = "gemini"
live_model = "gemini-2.5-flash-native-audio-preview-12-2025"
live_context_window = 32768

[transcription]
enabled = false
provider = "openai"
model = "whisper-1"
language = "en"
# endpoint = "http://localhost:8080/v1/audio/transcriptions"

[recording]
enabled = false
framerate = 15
segment_duration_secs = 60
quality = "medium"
# max_retention_hours = 24

[computer_use]
provider = "gemini"
model = "gemini-2.5-flash"
backend = "auto"

[agent]
default_backend = "codex"

[agent.codex]
command = "codex"
# Intendant-aware Codex fork; spawned instead of `command` when
# managed_context = "managed" (managed mode only works with the fork).
# managed_command = "/path/to/intendant-aware-codex"
model = "gpt-5.5"
approval_policy = "on-request"
sandbox = "workspace-write"
reasoning_effort = "medium"
web_search = false
network_access = false
writable_roots = []

[agent.claude_code]
command = "claude"
permission_mode = "default"
allowed_tools = []

[live_audio]
enabled = false
default_timeout_secs = 300
gemini_model = "gemini-2.5-flash-native-audio-preview-12-2025"
openai_model = "gpt-4o-realtime-preview"
sample_rate = 24000

[sandbox]
enabled = false
extra_write_paths = ["/var/log"]

[webrtc]
federation_allow_h264 = false

[[webrtc.ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

# [[webrtc.ice_servers]]
# urls = ["turn:turn.example.com:3478"]
# username = "user"
# credential = "pass"

[server]
# bind = "127.0.0.1" # optional; use for local/plaintext automation
advertise = ["wss://192.168.1.42:8765/ws"]

[server.tls]
enabled = false
# cert = "/etc/intendant/server.crt"
# key  = "/etc/intendant/server.key"

[server.auth]
advertised_transport = "none"
# bearer_token = "legacy-advanced-only"

# [[peer]]
# card_url = "https://peer.example.com/.well-known/agent-card.json"
# via_urls = ["wss://peer.tailnet.example:8765/ws"]
# client_cert = "/etc/intendant/peers/peer-client.crt"
# client_key = "/etc/intendant/peers/peer-client.key"
# bearer_token = "legacy-token-if-the-peer-requires-one"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

## Autonomy and approval

Approval is decided by a three-layer model (full UI details in
[Autonomy & Approvals](./autonomy.md)):

1. **Global autonomy** — `--autonomy <low|medium|high|full>` (defaults to
   `medium`). `low` asks for everything except file reads; `full` keeps the
   human entirely out of the loop (auto-approve) except for `HumanInput`.
2. **Category rules** — the `[approval]` section above (`auto`/`ask`/`deny`) per
   category.
3. **Per-action approval** — `y` / `s` / `a` / `n` (approve / skip / approve-all
   / deny) prompts in any frontend.

The nine action categories are: `FileRead`, `FileWrite`, `FileDelete`,
`CommandExec`, `NetworkRequest`, `Destructive`, `HumanInput`, `LiveAudioSpawn`,
`DisplayControl`. `DisplayControl` uses a session-grant model — approve once and
subsequent display actions skip the prompt (revocable in-frontend).
`HumanInput` and `LiveAudioSpawn` always require a human regardless of autonomy
level or category rule.

## Skills

Skills are named instruction sets stored as `SKILL.md` files with YAML
frontmatter, discovered from two directories (project-scoped first):

1. `<project-root>/.intendant/skills/<name>/SKILL.md`
2. `~/.intendant/skills/<name>/SKILL.md`

```yaml
---
name: deploy
description: Deploy the application to production
autonomy: high
disable-auto-invocation: true
---

## Steps

1. Run tests
2. Build release binary
3. Deploy to server
```

Frontmatter fields: `name` (required), `description` (required), `autonomy`
(override session autonomy when active), `disable-auto-invocation` (only the
user can trigger it), `disable-model-invocation` (run without LLM calls),
`sandbox` (override the session sandbox setting), `compatibility` (required
system tools), `allowed-tools` (restrict the available tool set). Project skills
take precedence over personal skills of the same name.

## INTENDANT.md project instructions

Place `INTENDANT.md` in your project root or at
`~/.config/intendant/INTENDANT.md` for global instructions. Both are loaded if
present (global first, project-local second) and injected into the conversation
at session start, before knowledge/memory.

## System prompts

System prompts are compiled into the binary, so `intendant` works from any
directory. Two base variants exist:

- `SysPrompt.md` — full prompt with JSON schema and per-function docs (used with
  text-based JSON extraction).
- `SysPrompt_tools.md` — condensed prompt for native tool calling (function docs
  live in the API tool definitions).

The active variant is chosen automatically based on whether the provider has
native tool calling enabled. Prompts resolve via a 3-layer cascade (highest
priority first):

1. **Project root** — `<git-root>/SysPrompt.md` (or `SysPrompt_tools.md`)
2. **Global config** — `~/.config/intendant/SysPrompt.md`
3. **Compiled-in default** — always available, zero-config

Role-specific prompts (`SysPrompt_orchestrator.md`, `SysPrompt_research.md`,
`SysPrompt_implementation.md`) follow the same cascade and append to the base.
The presence layer uses `SysPrompt_presence.md`; the live-audio voice agent uses
`SysPromptLiveAudio.md` with `{PLAYBOOK}` / `{RESPONSE_SCHEMA}` placeholders
substituted at runtime.
