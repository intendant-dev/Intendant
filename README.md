<p align="center">
  <img src="static/logo-glyph.svg" width="120" alt="Intendant" />
</p>

<h1 align="center">Intendant</h1>

<p align="center">
  Give an AI agent a full machine — under your oversight.
</p>

<p align="center">
  <a href="https://intendant.dev"><b>intendant.dev</b></a> ·
  <a href="https://lovon-spec.github.io/Intendant/">Docs</a> ·
  <a href="https://intendant.dev/trust">How trust works</a>
</p>

Intendant is an open-source operating environment for autonomous AI agents, written in Rust. The agent gets a real machine — shell, file editing, a graphical desktop it can see and control, voice, and phone calls — under layered human oversight: an autonomy dial, per-category rules, and per-action approval gates, with every command, diff, and decision logged and replayable. It runs its own agent loop, supervises **Codex and Claude Code** as managed backends, and is portable across OpenAI, Anthropic, and Gemini — on macOS, Linux, and Windows, all first-class.

Your side of the glass is a browser tab. No client software, no extension — identity is a key held in your browser and protected by your passkeys. Approve a diff from your phone, watch the agent's live desktop from a tablet, administer the fleet from any laptop.

And it doesn't stop at one box. Daemons federate into fleets, and people and organizations grant other people — and other agents — scoped access to their machines, infrastructure, and resources. Every session's authority is minted by the target machine's own IAM, never by a hosted service. The goal is a **network of agentic networks**.

<p align="center">
  <img src="src/bin/connect/assets/landing-hero.webp" alt="The Intendant dashboard's Activity feed: an agent diagnoses a failing job with an auto-approved command, proposes a diff, waits for an approval-gated run, and reports the verified result." />
</p>

## A daemon in ninety seconds

Stand up an owned, keyless daemon on a fresh box with one command (registration on the hosted rendezvous is invite-only during the pre-alpha; self-hosting is never gated):

```bash
# macOS / Linux
curl -fsSL https://intendant.dev/install.sh | sh -s -- --owner <your-key>
```

```powershell
# Windows
& ([scriptblock]::Create((irm https://intendant.dev/install.ps1))) -Owner <your-key>
```

Add `--service` / `-Service` on an unattended box to register the daemon with the platform's native supervisor (systemd, launchd, Task Scheduler — no init system is a dependency) so it outlives the SSH session and restarts on failure. Then:

1. **Claim** — the daemon prints a twelve-word claim phrase; paste it into the browser you're already holding. Root authority pins to your browser's passkey-protected key from first boot.
2. **Fuel** — grant time-boxed credential leases from your end-to-end-encrypted vault — or don't, and relay model calls through your browser instead.
3. **Work** — submit tasks, watch the live desktop, approve what you chose to gate.

## Nothing to install on your side

Most agent environments start by installing software on the device in front of you. Intendant never does: the whole client is a browser tab — no app, no extension, and on the hosted rendezvous nothing to set up at all (fully self-hosted, the one-time cost is trusting a certificate). From that tab you watch the live desktops of every daemon in your fleet over WebRTC, hand input over and take it back, annotate what you see, record what happened — and agents can see and drive displays on federated peer machines the same way, exactly as far as each machine's IAM allows. Same daemons, same authority, from a workstation or held in one hand.

<p align="center">
  <img src="src/bin/connect/assets/landing-video.webp" alt="The dashboard's Video tab streaming a live agent desktop over WebRTC: a browser and a terminal scrolling a build, with view-only, annotate, record, and take-control affordances." />
</p>

## No secrets on the box

Provider credentials are something a daemon **borrows, never owns**:

- **The vault** — API keys and subscription OAuth (Codex, Claude Code) live end-to-end encrypted behind your passkeys, synced blind through the rendezvous with rollback protection. No server can read it.
- **Leases** — a connected dashboard session grants credentials that live in daemon memory only: auto-renewed while you're attached, expiring on their own after you leave (the offline window is per-daemon and *is* the autonomy/security dial — `0` means fueled only while you watch). Revocation is one click from any signed-in device, and every grant, expiry, and revocation lands in a custody audit trail the daemon cannot forge.
- **Client egress** — the strictest mode routes model calls through your browser: the daemon ships the request over the verified tunnel, your browser attaches the key (against a fixed per-provider host allowlist) and streams the response back. The credential never touches the machine at all.

So a disposable VPS can run a fully capable agent while its disk holds nothing worth stealing: snapshots, backups, and idle-box compromises come up empty, and even a compromise during an active lease is bounded by TTLs and per-daemon scoping instead of costing you a durable key. [How custody works →](https://lovon-spec.github.io/Intendant/credential-custody.html)

<p align="center">
  <img src="src/bin/connect/assets/landing-vault.webp" alt="The credential vault panel: three credentials with masked secrets, two active leases expiring in 15 minutes, re-fuel buttons, and a client-egress relay option." />
</p>

## A network of agentic networks

Every daemon is its own authority island. Access — human or agent — is enforced by that machine's local IAM: principals (browser keys, peer daemons, org members), grants, and roles over a fine-grained permission catalog, carried over mTLS and end-to-end-verified tunnels. The hosted rendezvous is deliberately powerless: it introduces browsers to daemons, relays ciphertext, and stores client-signed metadata — it can deny service, but it cannot impersonate a daemon, read a tunnel, or mint authority. It is open source and [self-hostable](https://lovon-spec.github.io/Intendant/self-hosted-rendezvous.html), and an append-only transparency log makes its name directory tamper-evident.

Organizations are a root keypair, not a row in someone's database. The org signs grant documents; members present them to any daemon that trusts the org key, where they materialize into ordinary local grants — capped by the role ceiling that daemon's owner set, expiring by default, revocable by signed revocation lists, always overridable locally. An org grants a person (scoped browser sessions) or that person's daemon (agent-to-agent peer profiles) as separate, separately-auditable decisions. That is how an organization runs a network of agents over its own infrastructure without surrendering it: scoped, auditable, key-first — with passkeys and one-phrase claiming keeping the ergonomics human. [The full trust model →](https://lovon-spec.github.io/Intendant/trust-architecture.html)

## Why "Intendant"

In a theater, performers play and conductors orchestrate. Above them stands the **Intendant** — the general director who runs the house: who gets the stage, which productions run, on whose authority, with the books open. The Intendant doesn't play a note; it makes the performance possible and accountable. The older sense of the word reaches further: royal intendants administered provinces on behalf of the crown — authority delegated, scoped, and revocable.

That is the shape of this system. Agents perform. Orchestrators conduct — the native orchestrator decomposing work across sub-agents, or Codex and Claude Code as guest conductors bringing their own ensembles. The Intendant runs the house — the machine, the schedule, the stage door, the ledger — and answers to you. And houses federate: your companies can tour other houses on signed contracts, honored at the stage door but always subordinate to the house's own rules. A network of agentic networks.

## Architecture

```
  ┌──────────────────────── intendant (controller) ─────────────────────────┐
  │                                                                          │
  │  Frontends ──intents──►  control plane (single writer) ──► EventBus      │
  │  (Web · MCP ·            session supervisor · task dispatch              │
  │   socket)                     │                │                         │
  │      ▲                        │                │                         │
  │      │ render          ┌──────┴──────┐   ┌─────┴───────────────┐         │
  │   presence ◄───────────┤ native loop │   │ external-agent       │        │
  │   (mediator AI)        │ + sub-agents│   │ (Codex/Claude Code)  │        │
  │                        └──────┬──────┘   └─────┬───────────────┘         │
  └───────────────────────────────┼────────────────┼────────────────────────┘
              │                    │                │
              ▼                    ▼                ▼
        Voice / Model APIs   intendant-runtime   external CLI subprocess
        (live + streaming)   (sandboxed exec,    (wired to Intendant's
                              never holds keys)    MCP server)

        ◄─── WebRTC display + peer federation ───►  browsers / peer daemons
```

**Two binaries, one boundary** — the sandboxed *runtime* executes commands under OS filesystem restrictions (Landlock on Linux, Seatbelt on macOS, restricted tokens on Windows) and never holds API keys; the *controller* talks to model APIs and never executes user-requested commands directly.

**Presence layer** — a separate AI that mediates between user and agent. Handles conversation, dispatches tasks, narrates events, manages approval gates. Runs as server-side text or browser-side voice (Gemini Live / OpenAI Realtime via WASM).

**WebRTC display pipeline** — agents see and interact with graphical displays through a custom WebRTC transport (built on rtc-rs): a shared encoder pool with a VP8 baseline plus on-demand hardware H264 (VideoToolbox on macOS, VA-API/x264 on Linux, Media Foundation on Windows), tile-based dirty-region streaming, bidirectional clipboard, multi-monitor, and peer-to-peer display sharing across federated machines.

**External-agent orchestration** — supervise Codex or Claude Code as managed backends, with mid-turn steering, approval gates, rewind, and per-session cost accounting surfaced through the dashboard.

**Persistent daemon** — a control plane supervises many concurrent sessions and is the single writer of shared state; an idle web server runs headless. Federate with peer daemons for multi-host display and capability-based task routing.

**Phone calls** — outbound SIP calls via pjsua with a voice model conducting the conversation, returning structured data.

Four execution modes: *direct* (single agent), *user* (orchestrator + sub-agents in git worktrees), *sub-agent* (scoped child task), and *external-agent* (supervise a third-party coding CLI).

## Dependencies

- **Rust** toolchain (stable)
- **wasm-pack** — `cargo install wasm-pack`
- **ffmpeg** — display recording and H264 encoding
- **macOS**: `./scripts/setup-macos.sh` installs everything (cliclick, ffmpeg, Vortex Audio, wasm-pack, app bundle)
- **Linux**: `./scripts/setup-linux.sh` installs everything (build-essential/binutils, libvpx, libxcb, xdotool, PipeWire, ffmpeg, PulseAudio, Xvfb)
- **Windows**: `./scripts/setup-windows.ps1` (`x86_64-pc-windows-msvc`) — see the [Windows support](https://lovon-spec.github.io/Intendant/windows-support.html) docs

## Quick Start

On a fresh box, use the [installer one-liner](#a-daemon-in-ninety-seconds) above. From a checkout:

```bash
cargo build --release
./target/release/intendant
```

That starts the persistent daemon and prints the dashboard URL (port 8765 by default). The dashboard is the canonical way to drive Intendant — submit and steer tasks, watch the live desktop, approve gated actions, manage access, fuel the daemon. Fuel with credential leases from your vault, or keep keys local in `.env` (`~/.config/intendant/.env` for global use).

The same binary is the ops toolbox. Each subcommand stands alone — no project, no API key:

| Subcommand | What it does |
|---|---|
| `intendant service install \| uninstall \| status` | Register the daemon with the platform's native supervisor (systemd / launchd / Task Scheduler) so it survives reboots |
| `intendant access setup \| list \| recert \| remove \| serve-certs` | Browser mTLS certificates and device enrollment for the dashboard |
| `intendant peer invite \| join \| approve \| identities \| revoke \| …` | Pair daemons: peer-issued mTLS identities and access requests |
| `intendant org init <handle>` | Mint an organization root key on this daemon |
| `intendant ctl status \| logs \| task start \| tools call \| …` | Drive a running daemon from scripts and agents (MCP under the hood) |
| `intendant setup browser` | Provision or repair the agent's managed browser |

One-shot and headless invocations, when you want them:

```bash
./target/release/intendant "Fix the flaky CI job"        # submit a task straight from the CLI
./target/release/intendant --continue "now the docs"     # resume the most recent session
./target/release/intendant --agent codex "task"          # supervise an external CLI (codex | claude-code)
./target/release/intendant --mcp                         # MCP server on stdio (for Claude Code, etc.)
./target/release/intendant --direct --no-web --json "t"  # headless single agent, JSONL to stdout
```

The full flag reference (providers, models, sandboxing, resume) lives in [Getting Started](https://lovon-spec.github.io/Intendant/getting-started.html).

## Web Dashboard

The web dashboard is the canonical frontend — on by default (port 8765; `--no-web` disables it), served to any browser, phone included — with ten tabs:

- **Activity** — live event log with context/changes views, approval buttons, follow-up input
- **Stats** — token usage per model with cost estimates, disk usage
- **Terminal** — embedded xterm.js live shells on this daemon and peers
- **Video** — WebRTC display viewers with remote control, annotations, recording replay
- **Station** — WebGPU mission-control canvas rendering the whole fleet live: sessions, approvals, context budgets, changes, peers, worktrees. Still prototype-stage, but this is the surface the dashboard is growing toward
- **Sessions** — browse, search, resume, and fork sessions across all backends
- **Files** — editor workbench over local and peer filesystems, IAM-scoped
- **Access** — the trust surface: people & devices, peers, organizations, credential custody
- **Settings** — provider/model, autonomy, external-agent backend, approval rules
- **Debug** — diagnostics and internal state

Optional **live voice** via Gemini Live or OpenAI Realtime — the browser connects directly to the model's realtime API through WASM with presence tools for approving actions, submitting tasks, and querying status by voice.

Late-connecting browsers receive the full session replay and cached state.

## Testing

```bash
cargo test --bins         # Unit tests (fast, no API keys needed)
cargo test -- --list      # List all test names
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/Intendant/)** — covers the architecture, the trust architecture and credential custody, peer federation and the self-hosted rendezvous, multi-agent and external-agent orchestration, the display pipeline, computer use and live audio, the web dashboard and Station, autonomy & approvals, MCP, configuration, session logging, and Windows support.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
