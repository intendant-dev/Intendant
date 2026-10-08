# MCP Control Lane — design research

**Date:** 2026-08-28 · **Status:** design research — a proposal for discussion; nothing here is scheduled or commissioned · **Base:** `main` @ `8b390e99`
**Prompt:** an original early-Intendant idea, revisited — *"an agent that only speaks MCP can fully control Intendant"*, preferably as a wrapper around the CLI (progressive discovery, context efficiency, skills), including over the Internet, possibly through Connect.

Provenance: a source survey against `8b390e99` plus an external review of the MCP specification and client ecosystem, both on 2026-08-28. File:line anchors below are against `8b390e99`.

---

## 0. Thesis

**Make the `ctl` grammar the canonical control-plane API, and make MCP one of its transports.**

The inversion in the prompt ("MCP wrapping CLI") is not a new subsystem — it is three bounded moves on machinery that already exists:

1. **The facade** — a new MCP tool profile exposing ~4 meta-tools (`run`, `help`, `docs`, `events`) whose grammar *is* the `ctl` verb tree, executed in-process against the same dispatch and the same per-tool IAM gate. Context cost drops from ~17–29k tokens of tool schemas to ~1–1.5k, with everything else discovered lazily through help text and skills — exactly the doctrine `ctl` already encodes.
2. **A machine identity for full-fidelity remote control** (lane R1) — an enrolled, daemon-minted agent identity (peer-class mTLS cert with an owner-chosen profile ceiling), carried by a thin sidecar because MCP clients cannot present client certificates. Rides the existing Connect relay under `relay_peer_admission`. This is the "fully control over the Internet" lane; every ceremony it needs already ships.
3. **A zero-install hosted lane** (lane R2) — daemon-terminated OAuth 2.1 on the fleet name through the ciphertext relay, minting a new *agent-lease* credential class that reuses the hosted-control doorbell → trusted-surface-approval → compiled preset/floor/wall machinery. This is what lets claude.ai/ChatGPT/Cursor-class clients connect with nothing installed — and it is capped exactly the way the hosted browser lane is capped, on purpose.

Connect never terminates MCP, never sees plaintext, never mints authority. The two-layer trust stance survives verbatim: `role:none` hosted floor; daemon-minted, trusted-surface-approved leases as the convenience carve-out; full authority only for locally-enrolled identities.

---

## 1. Where we actually are (survey)

### 1.1 The daemon is already an MCP server — an under-shaped one

- `POST /mcp` on the web gateway: **stateless JSON-RPC** — `initialize`, `tools/list`, `tools/call`, notifications acked with 202; `GET`/`DELETE` are **405 by design** (`gateway_routes.rs:2963–2992`, `mcp_gate.rs:415–536,680`). No SSE, no pagination, no resources/prompts on HTTP. `initialize` **hardcodes protocol `2024-11-05`** and capabilities `{"tools":{}}` (`mcp_gate.rs:455–462`).
- **Two disjoint tool tables**: 13 tools in the rmcp `#[tool_router]` block (`mcp/mod.rs:1570–2340`) + 43 hand-written HTTP-only definitions (`tool_gate.rs:404–757`). Net: **56 advertised / 75 callable / 19 callable-but-advertised-nowhere** (controller-restart family, browser workspaces, `take_display`, `spawn_live_audio`, …). `--mcp` stdio serves **only the 13 router tools** (rmcp `2025-06-18`, with resources+subscribe that HTTP lacks).
- **Context cost, measured/estimated**: names+descriptions alone = 25,277 B exact; full unfiltered `tools/list` ≈ **95–115 KB ≈ 26–29k tokens** (±25%, schema sizes estimated from source). The `core` profile every supervised backend gets (26 tools) ≈ **60–75 KB ≈ 17–19k tokens**. `agenda_op` alone ≈ 31–41 KB — one tool outweighs most entire MCP servers. The repo already ratified the countermeasure in miniature: `remote_command` was pulled from `core` explicitly for "context rent" (`tool_gate.rs:146–169`, 2026-08-07).
- **IAM is the solid part**: one principal ladder on `/mcp` (`mcp_http_access_context`, `mcp_gate.rs:759` — peer mTLS → session-scoped token → process token → own/app browser origin → mTLS cert → tokenless loopback + per-boot admission token), one per-tool map `mcp_tool_operation` (`tool_gate.rs:234–357`), enforced at call time and mirrored into `tools/list` filtering so advertised == callable. Pinned invariant: **no MCP tool reaches `AccessManage`, `CredentialsManage`, `Terminal*`, `Filesystem*`, `Settings`** (`tool_gate.rs:1429` et al.).
- Four ingresses share one dispatcher (`call_tool_by_name_as_caller`, `mcp/mod.rs:543`): gateway `/mcp`, stdio, the dedicated session-MCP loopback listener (session-scoped tokens only, serves only `/mcp` — `listener.rs:49,133`, `mcp_gate.rs:707`), and the dashboard tunnel's `api_mcp_tool_call` (`dashboard_control/api_control.rs:1196`).

### 1.2 `ctl` is already the context-efficient control surface — and already an MCP client

- `src/bin/caller/ctl.rs` (7,953 lines) + `ctl/remote.rs`: hand-rolled parser (no clap), **24 verb families, ~100 leaves**, dispatching as a stateless JSON-RPC client of `/mcp` (`rpc()` at `ctl.rs:5252`). Mapping is shaped, not 1:1 — 27 agenda leaves compress onto 3 tools; ~64 of the 75 tools are fronted; 2 leaves ride a loopback `/api` lane instead; `takeover` is HTTP-only.
- **The discovery doctrine is already written down**: top-level help ≈ 2.5 KB, ~37 KB of family help total, and `skills/intendant-cli/SKILL.md` (15.5 KB) teaches *"exposes broad capabilities lazily through subcommand help"* and — verbatim front-matter — *"Prefer `intendant ctl` over broad MCP tools to keep model context small."* Twelve embedded operate-skills (~151 KB corpus) carry the judgment layer. **The fleet already votes with its feet: supervised sessions operate through `ctl`, and the 17–19k-token `core` MCP profile is mostly rent.**
- **Nothing derives from anything**: verb dispatch, arg parsing, and help strings are three hand-maintained layers, plus the separate MCP tool tables and the hand-written IAM match. Adding a verb touches 3–4 files and no test catches a skipped one. (Contrast: `gateway_routes::ROUTES` — 185 rows, five derived consumers, drift-tested — is the house exemplar the MCP/ctl surface never joined.)

### 1.3 Remote reachability: transport solved, identity unsolved

- **Sanctioned lanes to `/mcp` today**: (a) loopback (token), (b) mTLS with an enrolled identity — including **peer daemons** (fail-closed profiles; `ctl --peer` drives a remote daemon's `/mcp` over pinned mTLS today, `ctl.rs:385–431`), and — since 2026-07-31 — (c) **Approved peer identities through the Connect relay** behind `[connect] relay_peer_admission` (default off, boot-pinned; `http_dispatch.rs:356–373`).
- **Hosted leases are excluded from `/mcp` twice over, by name**: the proof-verification block skips `/mcp` (`http_dispatch.rs:411–416`) and the compiled wall's first check is `path == "/mcp"` → deny (`access/hosted_control/policy.rs:197–211`); pinned by test (`web_gateway/mod.rs:2895–2914`). Sovereign (custom-domain) keeps every cap. The lease machinery itself is exactly what a machine lane wants to reuse: machine-scriptable signed doorbell POST (`model.rs:614–643`), trusted-surface-only approval that can reduce but never raise (`runtime.rs:624–711`), TTL 60 s–24 h with **no renewal path**, per-request P-256 proof-of-possession (not bearer), compiled preset matrix View/Tasks/Operate + immutable floor (approvals, IAM, vault, org ops, self-escalation — `policy.rs:31–71`).
- **The gap is identity, not transport**: there is **no machine-principal concept** — every authority-bearing credential is an owner cert, a peer-daemon identity, a supervised-session token, or a tab-bound browser lease. And the two credential classes that work remotely are unspeakable by MCP clients: **no major MCP client supports client certificates** (confirmed: open unshipped issues in Claude Code #9869, OpenAI Apps SDK #102, Cursor, opencode), and none can produce Intendant's custom P-256 request proofs. Off-the-shelf remote MCP auth reality = **OAuth 2.1 / bearer at the server**.
- Headless enrollment precedents already ship: peer `request → approve → complete` (requester's key never leaves home, `peer-federation.md:1069–1114`) and the Codex-Cloud attach broker (single-use minted token → keypair → daemon-signed expiring cert, `codex_cloud_attach.rs`; profile `cloud-worker` = zero authority — an attachment credential, but the *shape* is the machine-enrollment pattern).

### 1.4 The spec and the ecosystem moved toward exactly this design

- **MCP 2026-07-28** (current, ratified): the protocol went **stateless** — sessions and `initialize` handshake deleted, POST-only Streamable HTTP with optional per-request SSE for progress, MRTR replacing server-initiated elicitation, `server/discover`, required `ttlMs` + deterministic ordering on list results (prompt-cache-friendliness as a spec concern). Intendant's "stateless JSON-RPC POST, 405 on GET" posture — a version-skewed oddity yesterday — is now roughly the spec's shape. Auth: OAuth 2.1 + RFC 9728 protected-resource metadata mandatory for servers, RFC 8707 resource indicators, CIMD replacing dynamic client registration; **no device-code flow** in core. Extensions framework carries **Tasks** (long-running work) and **Skills over MCP** (SEP-2640, `skill://` resources) — the standards-track hook for Intendant's skills corpus. Deprecated: sampling, roots, logging.
- **Client reality (Aug 2026)**: all majors do Streamable HTTP + OAuth end-to-end (Claude Code/claude.ai, ChatGPT dev mode, Codex CLI, Gemini CLI, Cursor, VS Code); most still speak 2025-era wire semantics — serve both. Only Anthropic clients defer tool loading; **assume worst-case clients that inject every schema**. claude.ai "custom connectors" dial **from Anthropic's cloud** — a zero-install lane must be Internet-reachable (relay). Claude Code caps MCP tool output (~25k tokens default) and auto-backgrounds 2-min-plus calls.
- **Progressive disclosure won the argument**: Anthropic's code-execution-with-MCP numbers (tool definitions 300–500 tokens each; 150k → 2k tokens, 98.7%), Tool Search (~85% reduction), Cloudflare Code Mode (single code tool over generated APIs), Stainless (exactly two tools: exec + docs-search, "more accurate and token-efficient than one tool per API method"). A CLI-shaped meta-tool facade is the 2026 mainstream, not an eccentricity.
- **Exec-style tools are also the most-flagged security pattern of 2025–26** — the design must argue its case precisely (§6): closed verb grammar ≠ shell passthrough.

---

## 2. Design principles

1. **One grammar, many transports.** The `ctl` verb tree is the canonical control-plane API. The terminal binary, the MCP facade, stdio, and peer lanes are carriers of the same grammar, same help, same dispatch, same IAM classification. No second vocabulary.
2. **The facade is a router, not a privilege.** A facade call is authorized as the *resolved* underlying operation, per argv, against the caller's principal — never as a blanket operation. Parse failure ⇒ error, never dispatch. The facade adds **no new operation class** and lifts **no ceiling**.
3. **Authority is minted only locally.** Connect remains ciphertext relay + discovery + DNS. It never terminates MCP or OAuth, never holds tokens that grant daemon authority. (A Connect-terminated MCP gateway would make the rendezvous an authority mint and a bearer-token forwarder — the breach shape the hosted-control design exists to refuse; spec-side, token passthrough is MUST NOT.)
4. **Two-layer trust stance, never compressed.** Immutable `role:none` hosted floor; daemon-minted, trusted-surface-approved, expiring leases as the capped convenience lane; full authority only for locally-enrolled identities. The MCP lanes slot into these layers; they do not add a third.
5. **Worst-case client assumptions.** Every tool schema lands in the model's context; no resources, no prompts, no elicitation, bearer-only auth, output arbitrarily truncated by the client. Anything better (tool search, skills, SSE progress, tasks) is progressive enhancement.
6. **Derive, don't mirror.** The facade becomes a consumer of a declared verb surface, not a fourth hand-maintained copy. Where full table-ization is too big a bite now, the seam (parser-as-library) is cut so the table can arrive family-by-family.
7. **Fail closed, stay honest.** Unknown argv, unknown principal, unknown profile ⇒ refuse with guidance. (Today unknown *profiles* fail open — `tool_gate.rs:215` — and unmapped tools default to `RuntimeControl`; the facade hardens both on its path.)

---

## 3. The facade (`tool_profile=facade`)

### 3.1 Tool set — four tools, ~3–6 KB total definitions (~1–1.5k tokens)

| Tool | Args | Op class | Purpose |
|---|---|---|---|
| `run` | `argv: string[]` | resolved per-argv | Execute one `ctl` command under the caller's principal. Description carries the condensed 24-family map (~1.2 KB) + the doctrine line: discover with `run(["<family>","--help"])`; JSON is the default output. |
| `help` | `topic?: string` | read-only | Top-level map or family help. Separate from `run` so approval-gating clients can free-list discovery (`readOnlyHint: true`). |
| `docs` | `skill?: string, query?: string` | read-only | List/fetch the operate-skills corpus (intendant-cli, agenda, memory, peer-displays, …) and reference sheets. The Stainless second-tool pattern. Later also exposed as `skill://` resources per SEP-2640 for clients that speak it. |
| `events` | `since?: cursor, wait_s?: ≤60, filter?` | `SessionInspect` | Cursor long-poll over the session/daemon event stream (M2; §5). |

Everything else is grammar: approvals, sessions/tasks, displays, computer use, shared views, agenda, memory, peers, managed context, remote compute — reached as argv, discovered as help, taught as skills. Blocking verbs (`ask`, `display request --wait`) keep their held-POST semantics (≤900 s; Claude Code auto-backgrounds long calls, and per-request SSE progress covers the rest once implemented).

**Budget:** ~1–1.5k tokens of schemas up front, +~600 tokens for the first `help`, +300–800 per family drilled into, + skill text strictly on demand — versus 17–19k (`core`) or 26–29k (full) before any work starts. A ~90–95 % cut, consistent with the published ecosystem numbers, and trivially prompt-cacheable (static definitions, deterministic order, spec `ttlMs`).

### 3.2 Execution: in-process, not subprocess

The facade does **not** fork `intendant ctl` server-side. `ctl`'s parser and dispatch live in the same binary as the daemon; the seam is to carve **ctl-core** — `parse(argv) → PlannedCall { tool, args | api_route, help_text | parse_error }` — out of `ctl.rs`, callable from (a) the terminal binary with an HTTP executor, and (b) the facade with an in-process executor bound to the *caller's* `AccessPrincipal`. One grammar, both carriers. (Client-side sidecars — §5 — may exec the real CLI; that is the client's own credential and box.)

The two `/api`-lane leaves (`agenda ops|occurrences`) and `takeover` are declared in the table with their route's IAM op; `takeover` stays refused for non-local principals by that op.

### 3.3 IAM law for the facade

- Resolve argv → underlying tool/route **before any side effect**; authorize the resolved `PeerOperation` via the existing gate; emit the standard permission-denied tool result on refusal.
- Unknown/unparseable argv ⇒ parse error with the relevant help snippet. Never "unknown ⇒ `RuntimeControl`" — the facade's resolver is fail-closed even though the legacy tool-name map keeps its permissive default for old callers.
- `tools/list` for the facade profile is static and tiny; `help` output may later be principal-trimmed (show only families the caller could exercise) — cosmetic, not authorization.
- Session/state handles inside results (session ids, display ids, lease ids) remain **names, not capabilities** — every use re-authorizes against the principal (this is already the house pattern: agent-session scoping, cross-session approval containment, `mcp/mod.rs:1057–1119`; it matches the 2026 spec's state-handle-hijacking guidance verbatim).
- Output discipline: server-side result cap (~32–64 KB default) with explicit truncation notice + cursor guidance; images return as native MCP image content blocks (better than the CLI's save-to-file valve); `--raw`/`--json` grammar flags keep working.

### 3.4 Who gets the facade

- **External MCP agents** — the target audience: local stdio, LAN/mTLS via sidecar, Internet via R1/R2.
- **`--mcp` stdio mode** — converge it onto the facade (today's odd 13-tool subset becomes facade + owner authority; agenda/memory honestly report unavailable where the control-plane facade handles are absent in that mode).
- **Supervised backends** — follow-on experiment, owner's call: sessions already operate through `ctl` per doctrine, so dropping `core` (17–19k tokens/session) to facade-or-minimal is a real internal win for Codex/Kimi-class backends that inject schemas wholesale; Claude Code already defers via tool search, so its rent is lower. Not part of the initial cut.

---

## 4. Coverage: what "fully control" means

The facade inherits `ctl`'s reach (~64 tools + the `/api` leaves) on day one, and every future `ctl` verb lands on MCP for free — that is the point of one grammar. Honest deltas against "full":

- **Terminal, filesystem, sessions-catalog/transcript-read, plugins/skills management, settings, access/IAM** have neither ctl verbs nor MCP tools today (dashboard/tunnel only). Recommendation: grow them **as ctl verbs first** where genuinely wanted (sessions-catalog and transcript search are the obvious next families), and keep the standing invariant that `AccessManage`/`CredentialsManage` never enter the MCP dispatcher for any principal — trust-critical administration stays on local/dashboard-direct surfaces. Terminal/raw-fs over MCP is a deliberate open question (§10): the house model is that the *controlled session* does the shell/file work under the autonomy/approval regime; the controlling agent orchestrates. A second remote-shell lane is a decision, not a default.
- **13 non-ctl top-level namespaces** (`org`, `vault`, `access`, `auth`, `custody`, `service`, `setup`, …) are administrative ceremonies and should stay CLI-local.
- **Hygiene debts the facade work should sweep**: the 19 callable-but-unadvertised tools; the `screen` profile allowlisting six tools that have no definitions; unknown-profile fail-open; the `2024-11-05` hardcoded handshake; `docs/src/mcp-server.md` lagging ~5 weeks (missing `memory_judge`, `agenda_item`, `create_virtual_display`, `remote_command`, Codex-Cloud tools, 20 of 29 `agenda_op` verbs, the session-MCP listener, the loopback admission token).

---

## 5. Remote lanes

### L0 — local (exists today, gains the facade)
Loopback + admission token; session-MCP listener for supervised children; stdio for owner-configured clients. No new trust surface.

### R1 — enrolled agent identity (full fidelity; the sovereign answer)
**What:** a first-class machine principal: a daemon-minted mTLS client identity with an owner-chosen **profile ceiling** (new `agent-operator` profile family alongside the peer matrix; precedent: the ratified cautious default-profile doctrine for agentic peer control, and `AdminPeer`'s everything-but-`AccessManage`/`CredentialsManage` ceiling as the upper bound).
**Enrollment:** reuse the shipped headless ceremonies — peer-style `request → owner approves on the daemon CLI/dashboard → complete`, or the Codex-Cloud minted-token attach shape for one-shot provisioning. No browser required; requester's key never leaves its box.
**Transport:** because MCP clients cannot present client certs, the credential is carried by a **sidecar**: `intendant mcp serve --peer <id>` (name TBD) — a stdio MCP server printing the facade, executing over the existing pinned-mTLS peer client (`ctl --peer` machinery, `ctl.rs:385–431`). Works over LAN, direct WAN, or the Connect relay under `[connect] relay_peer_admission` + peer identity attestation — **all already built**. Requires the intendant binary on the agent's machine; that is the honest cost of full fidelity, and it is how "an agent next to a peer daemon" and fleet topologies already work.
**Ceiling:** whatever the owner grants — up to approvals and everything short of IAM/credential administration. This is the only lane where "fully control over the Internet" is literally true, and that is by design.

### R2 — daemon-terminated OAuth agent-lease (zero install; the hosted answer)
**What:** implement MCP-spec OAuth 2.1 **on the daemon**, published at the fleet (or sovereign) name through the ciphertext relay: RFC 9728 protected-resource metadata, authorization-code + PKCE, CIMD client identity, RFC 8707 audience binding. The access token names a new **`agent_lease`** credential class — a sibling of the hosted browser lease, minted by the same doorbell core, approved on the same trusted surfaces, compiled to the same preset matrix (View / Tasks / **Operate**) under the same immutable floor and TTL discipline (≤24 h, no silent renewal; refresh = re-doorbell or short refresh within lease lifetime — pick in review).
**The consent flow is the doorbell.** The authorization endpoint parks a signed pending request (client id, requested preset-as-scope, TTL, label) and serves a *display-only* page: "approve this on your Intendant dashboard / app / phone." Approval happens out-of-band on a trusted surface (existing `decide_request` core; approval may reduce, never raise); the auth page long-polls the decision and only then redeems the code. Hosted surfaces display; trusted surfaces decide — unchanged.
**Enforcement:** lift the `/mcp` fence **only** for verified `agent_lease` bearers when a new restart-pinned opt-in is set (sibling of `hosted_control_enabled`; separate knob, same discipline), and gate per-call as `preset_allows_operation(preset, mcp_tool_operation(resolved tool))` — the wall composes with the facade resolver, so approvals/IAM/vault remain unreachable at every preset, exactly as the browser lane. `ActorKind` stays `Unattributed` ⇒ owner-grade agenda/memory verbs stay owner-only automatically.
**Bearer honesty:** this lane is weaker than the browser lease's per-request P-256 proofs — a bearer is a bearer. Mitigations: TLS terminates only at the daemon (relay is ciphertext), short TTLs, audience binding, scope minimization + step-up (spec pattern maps 1:1 onto presets), full IAM audit per call, and the compiled ceiling meaning a stolen token is a time-boxed *Tasks/Operate* capability, never approvals or IAM. Upgrade path if the ecosystem grows proof-of-possession (DPoP-class): adopt it; the lease core doesn't change.
**Reach:** this is the lane for claude.ai custom connectors (which dial from Anthropic's cloud), ChatGPT dev-mode, Cursor, and anything else that can only do "URL + OAuth". It requires the Connect deployment actually running the relay — activation territory, not new architecture.

### Rejected: Connect-terminated MCP
Connect terminating TLS/OAuth and forwarding tool calls would make the rendezvous an authority mint and a bearer-forwarder — the breach shape the hosted-control design analysis rejected (and spec-forbidden token passthrough besides). Not negotiable under principle 3.

### Positioning vs peers/A2A
Doors, not rivals: **MCP facade** = foreign agents in; **peer federation** = daemons to daemons; the facade *composes* with federation (`run(["peer","task",…])`, `--peer`-shaped verbs) so one reachable daemon fronts a fleet. A2A (v1.0, now under the same Linux-Foundation roof as MCP) stays a discovery-card slot (`agent_card.rs` already carries `mcp`/`a2a` transport kinds) — worth a card, not a runtime, until a real non-MCP orchestrator shows up.

---

## 6. Security posture of an exec-shaped tool (the rebuttal)

Exec-style MCP tools are 2026's most-flagged pattern; this design is not that pattern, and the doc should argue it head-on:

1. **Closed grammar, not a shell.** `argv: string[]` into the ctl parser — no shell, no PATH, no env influence, no cwd semantics, no arbitrary binaries. The reachable set is the verb table, enumerable and testable.
2. **Per-leaf authorization.** Every call authorizes as its resolved `PeerOperation` against a bound principal — the same gate, audit, and containment as today's typed tools (agent-session scoping, cross-session approval refusal). A facade call is never "run as the daemon".
3. **Fail-closed resolution.** Unknown argv is an error; the facade never inherits the legacy unmapped→`RuntimeControl` default.
4. **Static descriptions.** Tool definitions are compiled constants — no dynamically-generated description text, hence no tool-poisoning channel from daemon state into the client's context. Help/docs output is data the client requested, clearly framed as content.
5. **Handles are names.** Session/display/lease ids confer nothing; possession-is-not-authentication is already pinned in code and now also in spec guidance.
6. **Injection realism.** The controlling agent reads session output, agenda text, memory claims — attacker-influenced content. That is Intendant's existing quarantine/approval problem, unchanged by transport; the facade adds no new sink. What it must not do: let result content smuggle *new tool definitions or instructions* — results are content blocks, never definitions.
7. **Availability.** Authenticated principals escape the anonymous-relay budget problem; R2 bearers get per-lease rate/budget accounting (the browser-lease precedent), and the doorbell itself stays bounded pre-verification work.
8. **Auditability.** Facade calls log resolved argv + principal into the session timeline (the `/mcp` serve-milestone plumbing already exists); R2 lease admissions/uses are IAM audit events like the browser lane.

---

## 7. Protocol posture

- **Now (with the facade):** negotiate `initialize` honestly instead of hardcoding `2024-11-05`; echo the client's requested era (2025-03/2025-06/2025-11 semantics are all satisfiable by the current stateless POST server); advertise only-true capabilities. Serve both eras Cloudflare-style; the 2026-07-28 stateless model is *already* Intendant's shape (405-on-GET is now spec-blessed; per-request SSE is additive).
- **Soon:** per-request SSE for progress on long verbs (`ask`, waits, remote compute); deterministic ordering + `ttlMs` on list results (free — tables are `OnceLock`-static).
- **Later, as clients arrive:** Tasks extension for `remote_command`/long tasks; MRTR for `ask` (replacing the 900 s held POST); Skills-over-MCP (`skill://`) fronting the embedded skills corpus; `Mcp-Method`/`Mcp-Name` headers feeding the pre-dispatch classifier (the gateway's route-table idiom extends naturally).
- **Streaming/attach:** the real gap is product-level, not transport-level — ctl never solved follow/attach either. `events` (cursor long-poll, ≤60 s chunks — the `remote wait` idiom) is the transport-agnostic answer and improves the CLI too; SSE/Tasks layer on top.

---

## 8. What this buys internally (side payoffs)

- The **ctl-core seam** kills the grammar/help/dispatch triple-maintenance and gives the drift test that today doesn't exist (facade ↔ CLI ↔ tool tables pinned to one declaration; ROUTES-style).
- **Supervised-backend rent**: migrating externals off `core` onto facade/minimal reclaims ~17–19k tokens per session for schema-injecting backends.
- **stdio mode stops being a third, stranger surface.**
- The **docs/mcp-server.md** catch-up and hygiene sweep (dark tools, fail-open profile) ride along.
- R2's OAuth machinery is generic: future non-MCP integrations (plain HTTPS automations) get the same doorbell-approved, preset-capped credential class instead of inventing bespoke tokens.

---

## 9. Milestones

- **M0 — hygiene (small, standalone PRs):** initialize version negotiation + honest capabilities; docs catch-up; unknown-profile fail-closed; decide the 19 dark tools (advertise under `full` or retire); `screen`-profile dead entries.
- **M1 — the facade, local:** ctl-core seam (parser-as-library returning PlannedCall); `run`/`help`/`docs` tools + `tool_profile=facade`; resolved-op IAM law + fail-closed resolver; output caps; timeline audit of argv; stdio convergence. Acceptance: a stock MCP client (Claude Code) drives sessions/approvals/agenda end-to-end through three tools at ≤2k tokens of definitions.
- **M2 — `events` + long-verb polish:** the cursor long-poll verb (CLI + facade); per-request SSE progress; result-cap + cursor conventions.
- **M3 — R1 remote:** `agent-operator` profile family + headless enrollment reuse; the stdio sidecar over the peer mTLS client; relay leg via `relay_peer_admission`; skills teaching the sidecar setup. Acceptance: an MCP-only agent on another machine fully operates this daemon through the relay with an owner-granted ceiling.
- **M4 — R2 hosted:** OAuth AS/RS on the daemon (PRM, auth-code+PKCE, CIMD, RFC 8707); `agent_lease` credential class through the doorbell/preset/floor compiler; the `/mcp` carve-out behind a new restart-pinned opt-in; per-lease budgets + audit; activation notes for the relay deployment. Acceptance: claude.ai custom connector completes doorbell-approved OAuth and operates at Tasks preset; approvals/IAM provably unreachable at every preset.
- **M5 — extensions:** Tasks, MRTR-ask, skill:// — as client support materializes.

M1/M2 are pure-win regardless of the remote story. M3 needs no new trust machinery. M4 is the only genuinely new credential surface and should get the same review depth as the hosted-control lease landing (multiple independent blind reviews).

---

## 10. Open questions for the owner

1. **R2 default ceiling** — Tasks (matches hosted default) or View? And is Operate offerable to agent-leases at all, or browser-lease-only?
2. **Terminal/raw-fs over MCP** — keep excluded (orchestrate-the-session-instead doctrine), or add under Operate-class ceilings for R1?
3. **Supervised backends onto the facade** — commission the experiment (per-backend, Codex first), or leave `core` alone for now?
4. **`--mcp` stdio convergence** — fold into facade in M1, or keep the 13-tool legacy surface until a deprecation window?
5. **Naming** — `tool_profile=facade` vs reusing `cli`; tool names `run/help/docs/events`; sidecar verb name (`intendant mcp serve`?).
6. **R2 refresh semantics** — hard re-doorbell on expiry (purest) vs refresh-token within the original lease TTL (friendlier for connectors).
7. **Program placement** — track as an agenda program with M0/M1 commissioned first?

---

## Appendix A — source map (anchors @ 8b390e99)

- MCP dispatch/tables: `src/bin/caller/mcp/mod.rs:459,543,1570–2340`; `mcp/tool_gate.rs:83–357,404–757` (profiles, IAM map, manual table); params/docs: `mcp/tool_params.rs`.
- Gateway MCP gate: `web_gateway/mcp_gate.rs:415–536,680,707,759–884`; routes: `gateway_routes.rs:2963–2992`; body cap `:198`; session-MCP listener: `web_gateway/listener.rs:49,133`; tunnel twin: `dashboard_control/api_control.rs:1196`.
- ctl: `src/bin/caller/ctl.rs` (parser `137–273`, dispatch `65–131`, rpc `5252–5313`, help `5666–6345`, peer mode `385–431`), `ctl/remote.rs`; discovery descriptor: `cli_descriptor.rs`; doctrine: `skills/intendant-cli/SKILL.md`.
- Hosted-control: `access/hosted_control/policy.rs:31–71,197–361` (presets/walls; `/mcp` fence `:198`), `model.rs:16–19,175–224,614–685`, `runtime.rs:624–711`; dispatch fences: `web_gateway/http_dispatch.rs:343–493` (lease-skip `:411–416`, relay-peer carve-out `:356–373`).
- Identity/enrollment: `access/mod.rs:167,403`, `access/iam.rs:112–117,857–865,2600–2653`, `access/actor.rs:236–249`; peer ceremony `docs/src/peer-federation.md:1069–1114`; Codex-Cloud attach `codex_cloud_attach.rs`; profiles `access/access_policy.rs:491–540`.
- Relay: `src/bin/connect/relay.rs`, `relay_tunnel.rs`, `agent_card.rs:237–324` (attestation + relay candidate).
- Docs lagging: `docs/src/mcp-server.md` (2026-07-22 vintage), `docs/src/hosted-control.md:183–254`, `docs/src/trust-architecture.md:59–100,823–829` (the deliberate "ctl-over-HTTPS unbuilt" note — the R1 sidecar is the custody-respecting answer it anticipated).

## Appendix B — external references (verified 2026-08-28)

MCP 2026-07-28 spec + changelog (modelcontextprotocol.io; release post blog.modelcontextprotocol.io 2026-07-28); authorization: OAuth 2.1 draft-13, RFC 9728/8414/8707/9207, CIMD; ext-auth repo (enterprise-managed stable, client-credentials draft; no device-code). Anthropic: code-execution-with-MCP + advanced-tool-use posts (Nov 2025), tool-search docs. Cloudflare Code Mode + mcp-v2 post (2026-08-06). Stainless two-tool changelog. Client docs: Claude Code MCP (tool search default-on; 25k output cap), claude.ai custom connectors, ChatGPT developer mode, Codex CLI rmcp client, Gemini CLI, Cursor, VS Code. Security: spec security-best-practices (2026-07-28: state-handle hijacking, token-passthrough MUST NOT, local-exec consent), CVE-2025-6514 (mcp-remote), CSA tool-poisoning note 2026-07-01, OWASP MCP Tool Poisoning. mTLS absence: claude-code#9869, openai-apps-sdk#102, Cursor forum, opencode#14696. A2A: v1.0 (Mar 2026), joined AAIF alongside MCP 2026-08-17.
