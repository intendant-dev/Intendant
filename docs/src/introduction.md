# Introduction

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop to work in — shell access, file editing, a graphical display it can see and control, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system. Beyond running its own agent loop, Intendant also **supervises external coding agents** (Codex, Claude Code) as managed backends and **federates with peer machines** for multi-host display and task routing.

It runs on **macOS, Linux, and Windows**, is **provider-agnostic** (OpenAI, Anthropic, Gemini), and is designed so that every capability is reachable from any interface — CLI, web dashboard, MCP, or voice. The client side is deliberately just a browser — nothing to install, identity held in browser keys protected by your passkeys — so the same dashboard works from a workstation or a phone.

Beyond the single machine, the goal is a **network of agentic networks**: daemons owned by people and organizations, where an owner grants other people — and other daemons — scoped IAM access to their machines, infrastructure, and resources ([Trust Architecture](./trust-architecture.md)). Credentials follow the same discipline: a daemon borrows time-boxed leases from a passkey-sealed vault, or relays model calls through the owner's browser, so the machine itself holds nothing durable to steal ([Credential Custody](./credential-custody.md)).

The name is the thesis. In a theater, performers play and conductors orchestrate — but the *Intendant* runs the house: who gets the stage, which productions run, on whose authority, with the books open. Here agents perform; orchestrators conduct (the native orchestrator, or supervised Codex / Claude Code as guest conductors); the Intendant runs the house and answers to you.

> **About this book.** These docs are verified against the source periodically, but Intendant moves fast and active areas — the dashboard, external-agent orchestration, federation — can drift ahead of the prose between verifications. **When the docs and the code disagree, the code is authoritative.** Every chapter cites real file and module paths so you can check; the explanations focus on the *shape and the why* of each subsystem, which changes more slowly than exact line numbers.

## Design Philosophy

Intendant is built around a few core ideas:

**Security through process isolation.** The runtime and controller form the command-execution trust boundary. The *runtime* that executes arbitrary commands runs under OS filesystem restrictions (Landlock on Linux, Seatbelt on macOS, restricted tokens on Windows) and never holds API keys. The *controller* that manages model conversations never executes user-requested shell commands directly. See [Architecture](./architecture.md).

**Authority is always local.** Every session's authority — human or agent, browser or peer daemon — is minted by the *target daemon's own IAM*. Hosted services carry introductions, ciphertext, and signatures, nothing else; organizations are root keys whose signed grant documents materialize into local IAM under each owner's ceilings, never a central directory that could mint access. See [Trust Architecture](./trust-architecture.md).

**Daemons borrow credentials, never own them.** Provider keys and subscription OAuth live in a passkey-sealed, end-to-end-encrypted vault. A daemon holds time-boxed in-memory leases, or relays calls through the owner's browser and holds nothing at all — either way its disk has nothing worth stealing, which is what makes disposable agent boxes safe to stand up and throw away. See [Credential Custody](./credential-custody.md).

**The client is a plain browser.** No native app, no extension, and on the rendezvous path no certificate ceremony: browser-held identity keys (protected by passkeys), twelve-word claiming, and end-to-end-verified tunnels make any browser — the one on your phone included — a full client, with role ceilings capping low-provenance origins. See [Trust Architecture](./trust-architecture.md#anchor-daemons).

**Provider agnosticism.** OpenAI, Anthropic, and Gemini are all first-class providers with native tool calling, streaming, prompt caching, and computer use. The system is not locked to any single vendor — and through [external-agent orchestration](./external-agent-orchestration.md) it can also drive whole third-party coding CLIs.

**A single-writer control plane.** Shared mutable state (autonomy level, the active agent backend, runtime config) has exactly one writer: the control plane. Frontends are *display-only* — they render state and emit intents, but never mutate state directly. See [Control Plane & Daemon](./control-plane-and-daemon.md).

**Shared frontend vocabulary.** Frontends exchange state and intents through `AppEvent` and `ControlMsg`: the web dashboard, MCP server, and control socket render events and submit control messages to the single-writer control plane. See [Architecture](./architecture.md) and [Autonomy & Approvals](./autonomy.md).

**Presence as a separate AI.** Rather than a chat wrapper, the presence layer is an independent (usually fast) model with its own conversation, tools, and state awareness. It mediates between the user and the working agent, turning intent into tasks and narrating progress back. See [Presence Layer](./presence.md).

**Layered human oversight.** A three-layer autonomy system — global level, per-category rules, and per-action approval — keeps the user in control at whatever granularity they prefer, from approving every command to fully autonomous operation. See [Autonomy & Approvals](./autonomy.md).

## Architecture at a Glance

```
  ┌──────────────────────── intendant (controller) ─────────────────────────┐
  │                                                                          │
  │  Frontends ──intents──►  control plane (single writer) ──► EventBus      │
  │  (Web ·                  session supervisor · task dispatch              │
  │   MCP · socket)               │                │                         │
  │      ▲                        │                │                         │
  │      │ render          ┌──────┴──────┐   ┌─────┴───────────────┐         │
  │   presence ◄───────────┤ native loop │   │ external-agent       │        │
  │   (mediator AI)        │ + sub-agents│   │ (Codex/Claude Code)  │        │
  │                        └──────┬──────┘   └─────┬───────────────┘         │
  └───────────────────────────────┼────────────────┼────────────────────────┘
              │                    │                │
              ▼                    ▼                ▼
        Voice / Model APIs   intendant-runtime   external CLI subprocess
        (live + streaming)   (sandboxed exec)    (wired to Intendant's
                                                   MCP server)

        ◄─── WebRTC display + peer federation ───►  browsers / peer daemons
```

See [Architecture](./architecture.md) for the full picture.

## Key Capabilities

- **Multi-provider LLM integration** — native tool calling, streaming, prompt caching, and computer use across OpenAI, Anthropic, and Gemini ([Runtime Protocol](./runtime-protocol.md), [Multi-Agent Orchestration](./multi-agent.md))
- **External-agent orchestration** — supervise Codex or Claude Code as managed backends with steering, approvals, rollback, and cost accounting ([External-Agent Orchestration](./external-agent-orchestration.md))
- **WebRTC display pipeline** — a shared encoder pool (VP8 baseline + on-demand hardware H.264), tile-based dirty-region streaming, multi-monitor, and bidirectional clipboard ([Display Pipeline](./display-pipeline.md))
- **Peer federation** — Agent Cards, capability-based task routing, and cross-machine display sharing *with granted input* over mTLS, so an agent can use a computer on a peer machine when IAM allows ([Peer Federation](./peer-federation.md))
- **Trust architecture** — daemon-local IAM (principals, grants, roles, ceilings), browser identity keys under passkey protection, device enrollment, org root keys signing grant documents and revocation lists, and a transparency-logged directory ([Trust Architecture](./trust-architecture.md), [Self-Hosted Rendezvous](./self-hosted-rendezvous.md))
- **Credential custody** — the passkey-sealed vault, time-boxed credential leases, the client-egress relay, and the one-command keyless daemon bootstrap ([Credential Custody](./credential-custody.md))
- **Computer use** — a provider-agnostic abstraction over X11, Wayland, macOS, and Windows backends ([Computer Use & Live Audio](./computer-use-and-audio.md))
- **Live voice & phone calls** — Gemini Live / OpenAI Realtime via a WASM browser client, and outbound SIP calls ([Presence Layer](./presence.md), [Computer Use & Live Audio](./computer-use-and-audio.md))
- **Persistent daemon** — long-lived session supervision, a multi-session dashboard, and content-addressed file snapshots with rewind ([Control Plane & Daemon](./control-plane-and-daemon.md), [Web Dashboard](./web-dashboard.md))
- **MCP server and client** — expose Intendant's control surface as MCP tools, and connect to external MCP servers ([MCP Server](./mcp-server.md))
- **Filesystem sandboxing** via Landlock (Linux), Seatbelt (macOS), and restricted tokens (Windows); session persistence with structured JSONL logging and resume ([Session Logging](./session-logging.md)), and a skills system for named instruction sets

## Where to Go Next

- New here? Start with [Getting Started](./getting-started.md), then [Architecture](./architecture.md).
- Running a fleet, a team, or an organization? Read [Trust Architecture](./trust-architecture.md) and [Credential Custody](./credential-custody.md).
- Deploying or tuning? See [Configuration](./configuration.md) and [Windows Support](./windows-support.md).
- Building on a specific subsystem? Jump to its chapter via the sidebar.
