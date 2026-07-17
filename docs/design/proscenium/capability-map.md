# Proscenium — capability map

The coverage guarantee behind "nothing is deleted." Compiled against the
daemon's declared surface in this worktree @ `a5081f18`:

- `src/bin/caller/gateway_routes.rs` — the `ROUTES` table (~135 rows: every
  HTTP route + its tunnel twin),
- `src/bin/caller/dashboard_control/mod.rs` — `CONTROL_ONLY_METHODS`
  (tunnel-only residue),
- `src/bin/caller/event.rs` — the `ControlMsg` intent vocabulary and
  `AppEvent` stream.

Each entry: the capability → its Proscenium home. *Room* = nav destination;
*Queue* = the Needs-You inbox; *Space* = an object's deep page; *⌘K* = the
universal index. **[derived]** = computable client-side from existing
events; **[new projection]** = a daemon-side addition the design suggests
(optional, flagged, never assumed by v1).

---

## 1. Sessions & the agent loop

| Capability | Proscenium home |
|---|---|
| Catalog (`GET /api/sessions`, `/stream`, `/search`, `/message-search`) | Work → Live/Archive; ⌘K lanes 2 & 4 |
| Paged replay, agent-output, context-snapshot, report zip | Session Space → Timeline/Context/Vitals; Books → Reports |
| `CreateSession` / `StartTask` (all launch params: backend, model, sandbox, approval policy, managed context, worktree, project root) | Home composer; Work → New (Options folds) |
| `ResumeSession`, `StopSession`, `RestartSession`, `RenameSession`, `Interrupt` | Session Space header; stage-card ⋯; ⌘K actions |
| `FollowUp`/`CancelFollowUp`, `Steer`/`CancelSteer`, `EditUserMessage` | Session Space → Timeline (steer strip, edit chip; gated by `SessionCapabilities`) |
| `SpawnSubAgent` (delegate), `ForkSessionAtAnchor`, fork-points | Session Space → Vitals & lineage; Work → Live fan view |
| `ConfigureSessionAgent` + all Codex/Claude pins (`SetCodex*`, `SetClaude*`, `SetExternalAgent`) | Session Space → Controls; Settings → fine print (defaults) |
| `CodexThreadAction` (new/compact/fast/fork/side/undo/review/rename/goal/init/memory-reset) | Session Space → Controls; `/`-grammar in composer |
| Approvals (`ApprovalRequired` → approve/skip/deny, `SetApprovalRule` "like this") | **Queue**; inline in the Conversation; Session Space; ⌘K action |
| Questions (`UserQuestionRequired`, AskHuman, structured) | **Queue**; inline in the Conversation |
| Managed context (anchors/records/fission routes, rewind composer, backout, lineage) | Session Space → Context fold (rewind/records/fission); Studio raw |
| Worktrees (`/api/worktrees` inspect/scan/remove/clean/merge) | Work → Worktrees; Machine Space |
| Background tasks registry | Session Space → Vitals fold |
| History rollback/redo/prune, conversation rollback | Session Space → Changes |
| Session data delete (per-kind), `DELETE /api/session/{id}` | Session Space → Vitals & lineage → data |
| `SessionVitals` (git, cache TTL, rate limits), `UsageSnapshot` | stage-card meters; Session Space → Vitals; Books → Costs |
| `SessionActivity`/`StatusSnapshot`/`Tick` | the status sentences everywhere (voice register source) |
| `InvokeSkill` | composer `/`-grammar; ⌘K actions **[surfaces a daemon capability the SPA never exposed]** |
| Session notes (`post_session_note`/`SessionNote`) | Session Space → Timeline ("pin a note") **[new surface for a shipped tool]** |
| Controller-loop intents (halt/intervene/restart/status) | Studio → Workbench **[first-class home for MCP-only power]** |

## 2. External agents & fuel

| Capability | Proscenium home |
|---|---|
| `GET /api/external-agents` (installed/auth/quota posture) | Work → New (agent picker states); Settings → minds |
| Sign-in ceremonies (`/api/claude-auth/*`, `/api/codex-auth/*`) | People & Keys → Keys & vault fold; Work → New unfueled CTA |
| API keys (`POST /api/api-keys`, status) | Settings → minds; Home unfueled card ("Fuel") |
| Credential leases (grant/renew/revoke/status), custody trail | People & Keys → Keys & vault; **Queue** dry-lease FYI |
| Daemon vault blobs, deposits | People & Keys → Keys & vault (Studio folds) |
| Egress relay (register/unregister/probe + frames) | People & Keys → Keys & vault → egress fold |
| `POST /session` ephemeral live tokens | presence voice (composer mic); People & Keys diagnostics |

## 3. Displays, computer use, recordings

| Capability | Proscenium home |
|---|---|
| Inventory, signaling, WebRTC lanes | Screens → Stage |
| Input authority (request/release/snapshot, force-takeover) | Screens → Stage toolbar + authority card |
| `TakeDisplay`/`ReleaseDisplay`, virtual displays, debug screen | Screens → Stage / empty state; Studio → Workbench |
| `GrantUserDisplay`/`RevokeUserDisplay` (durations) | Screens → Your screen; Home shared-view banner |
| `DisplayRequestRaised` → `ResolveDisplayRequest` | **Queue** (doorbell, never auto-approvable) |
| Recordings (start/stop/delete, segments, m3u8, frames) | Screens → Recordings & clips; Session Space → Vitals fold |
| CU overlays (`CuActionExecuted`), shared view/focus | Screens → Stage; presence `inspect_frame(s)` renders in Conversation |
| Diagnostics visual markers, freshness sink | Studio → Workbench |

## 4. Presence, voice, audio

| Capability | Proscenium home |
|---|---|
| `presence_connect`/`make_active`/checkpoints | Home composer mic; voice is the Conversation's spoken register |
| Live audio (Gemini Live/OpenAI Realtime), `spawn_live_audio` | Home; the call skills' results render as Conversation artifacts |
| Transcription (`user_audio` → `UserTranscript`) | Conversation thread (voice messages land as text) |
| Presence tools (submit_task, approve/deny/skip, respond, set_autonomy…) | the Queue + Conversation (voice runs the same items) |
| Quarantined live-model tool calls | **Queue** FYI tier **[first surface — daemon ships, SPA silent]** |
| Presence camera frames | Conversation (presence video), Screens → Stage (presence source) |

## 5. Terminal, files, transfers, browser

| Capability | Proscenium home |
|---|---|
| PTY (`terminal_open/input/resize/close/share`, scrollback) | Screens → Terminals; Machine Space |
| Scoped fs (`/api/fs/*`, sha-guarded write, range reads) | Files; grant-denied humane state |
| Transfers (`/api/transfers/*`, chunks, resume, 206 downloads) | Files → Transfers; ⌘K "resume transfer" |
| Browser workspaces (create/acquire/release/close, providers) | Screens → Browser workspaces; Studio |
| Uploads store (`/api/session/current/uploads`) | composer attach; Session Space → Timeline attachments |

## 6. Peers / federation

| Capability | Proscenium home |
|---|---|
| Registry, agent card, capabilities, `PeerSnapshot` | Machines → cards & drill-down |
| Pairing (invite/join/request-access/poll, identities, decisions) | Machines → Link a machine; **Queue** (peer doorbell) |
| Message / delegate task / `POST /api/coordinator/route` | Machines → Delegate; composer `@machine` |
| Peer approvals (`/api/peers/{id}/approval`) | **Queue** (per-peer section) |
| Peer displays/sessions browsing, multi-host (`/api/dashboard/targets`) | Machine Space; Work/Screens host chips |
| Peer profiles (9) + revocation | Machines → pairing wizard; People & Keys → grants |

## 7. Access / IAM / trust

| Capability | Proscenium home |
|---|---|
| Overview, IAM state, user/client grants, grant updates | People & Keys → People & devices (+ full matrix fold) |
| Enrollment requests + decisions | **Queue**; People & Keys |
| Orgs (trust/revoke/issue/renew/ORL/issuers) | People & Keys → Organizations (Studio fold) |
| Connect (status/claim-code/config/unclaim) | People & Keys → Doors |
| Trust tier (`integrated`/`disposable`) | People & Keys → You |
| Fleet cert request, hosted anchor decisions | People & Keys → Doors (Studio fold) |
| Hosted control (bootstrap/requests/presets/leases) | **Queue** (asks); People & Keys → Doors |
| Tabs registry (`/api/dashboard/tabs`) | People & Keys → You ("your open tabs") |
| Roles (11 builtin) + 26 permissions | People & Keys, plain-language role previews; Studio matrix |
| The authority badge doctrine | every pane's authority line (rendering rule, not a route) |

## 8. Agenda, memory, stats, settings, misc

| Capability | Proscenium home |
|---|---|
| Agenda (`/api/agenda`, ops, reminders policy) | Home → Today ribbon; Books → The List |
| **Scheduled sessions** (`propose_scheduled_session`, approve/revoke, occurrences) | Home → Today; Books → The List **[first surface for the shipped scheduler]** |
| Memory (search/claim/propose, candidates, durability) | Books → What the house remembers |
| Usage/cost (`UsageSnapshot`, pricing, disk) | Books → Costs & usage; Machine Space |
| Settings payload (`GET/POST /api/settings`, all families) | Settings, question-sorted; every row in ⌘K lane 3 |
| `SetAutonomy`, `SetVerbosity`, env overrides | Settings → autonomy; Session Space ⋯; Settings → fine print |
| `GET /config`, `GET /debug`, stale-build compare | Studio → Workbench; the stale-build banner (kept) |
| MCP tools (whole list) incl. `api_mcp_tool_call` | Studio → Reference; all are also the presence tool surface |
| Attention (`UserNotification` urgency, title/favicon, push) | Queue FYI tier + escalation policy (`power-model.md` §6) |

---

## The two deliberate [new projection]s (optional)

1. **Unified attention feed** — v1 derives the Queue client-side from the
   event stream + polled routes (works today). Worth adding later: a
   control-plane `attention` projection (single writer, severity-ordered,
   expiry-aware) so every frontend (dashboard, Station, voice, MCP)
   inherits one queue instead of re-deriving it.
2. **Briefing digest** — v1 renders the briefing client-side from snapshot
   + agenda + recent events. Worth adding later: a presence-side digest
   query ("brief me") so the prose is presence's own voice on every
   frontend.

Neither blocks anything in this design.
