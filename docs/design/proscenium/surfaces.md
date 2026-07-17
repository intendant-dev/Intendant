# Proscenium — surfaces, specified

Every room of the house: its purpose, layout, components, states, the daemon
data it renders, and its interactions. Names in `code` are real routes,
events, methods, and vocabulary from the daemon (see `capability-map.md` for
the full derivation). Nothing in this file invents a backend capability;
where a surface would benefit from a projection the daemon doesn't publish
yet, it is marked **[derived]** (computable client-side from existing events)
or **[new projection]** (a daemon-side addition worth making, flagged as
such).

Conventions used below:

- *Decision card* — the Queue's unit (see §1.2).
- *Stage card* — the arch-topped live-work card (see §1.3).
- *Fold* — the system-wide disclosure contract (`power-model.md`): every
  summary carries its machinery one unfold away.
- *Authority line* — the trust-doctrine badge every pane carries:
  `you · owner` / `via dell-206 · operator`, plus route chip
  (`direct` / `fleet name` / `hosted`).

---

## 1. Home

The default screen. The owner's box. Three movements, top to bottom:
**Needs You → the Conversation → Now Playing & Today.** At Studio density the
same screen re-flows to a denser grid (queue left, thread center, vitals
right) — same content, instrument-first.

### 1.1 The briefing

First paint after >1h away (persisted per browser), presence opens the thread
with the briefing: 3–6 plain sentences covering *finished / failed & retried /
waiting on you / today*. **[derived]** from `StatusSnapshot`, session events
(`TaskComplete`, `LoopError`, `DoneSignal`), `AgendaChanged`, and the agenda
scheduler. A *"Tell me more"* affordance on any sentence drills into the
session Space or Books entry behind it. Dismissible; never repeated; Studio
density replaces prose with a compact fact table (same content).

### 1.2 Needs You — the Queue

Pinned above the thread whenever non-empty; a persistent header badge
(count + highest severity) reachable from every room.

**Item sources (all existing):**

| Queue item | Daemon source | Resolution |
|---|---|---|
| Command/tool approval | `ApprovalRequired` (+ category from the 8 shipped categories: `file_read`, `file_write`, `file_delete`, `command_exec`, `network`, `destructive`, `display_control`, `tool_call`) | Approve `y` / Skip `s` / Approve all like this `a` (category rule → `SetApprovalRule`) / Deny `n` |
| Agent question (free text) | `UserQuestionRequired` / AskHuman panel | Inline answer → respond intent |
| Structured question (external agents) | `UserQuestionRequired` (multi-question form) | Option buttons + free text → Submit/Skip |
| Display doorbell | `DisplayRequestRaised` (never auto-approvable) | Allow (15m / this session / until revoked → `ResolveDisplayRequest`) / Deny / Deny for session |
| Peer doorbell | `/api/peers/pairing/requests` (public knock) | Approve with profile picker (9 real profiles) / Deny |
| Peer session approvals | `POST /api/peers/{id}/approval` | Approve/Deny/Skip, per-peer |
| Enrollment request | `/api/access/enrollment-requests` | Grant role (plain-language role picker) / Deny |
| Hosted-control ask | `/api/hosted-control/requests` | Review ceremony link (presets view/tasks/operate, compiled floor) |
| Fuel (unfueled / lease dry) | `/api/api-key-status`, lease status events | "Fuel from your vault" → vault lease flow |

**FYI tier** (dismissible, never blocking): `BudgetWarning/Exhausted`,
watched-task completion, lease-expiry-soon, agenda reminders, stale-build
nudge, `UserNotification` by `NotificationUrgency` (Info/Attention/Urgent).

**Anatomy of a decision card:**

```
╭────────────────────────────────────────────────────────────╮
│ ● ATTENTION · approval                        2 min ago     │
│ Claude wants to delete 3 files                              │
│ In ~/projects/exports — this cannot be undone.              │
│ ┄┄ details ┄┄ (unfolds: exact paths, requesting session,    │
│    raw command, category, the rule it would set)            │
│ [Allow once]  [Always allow deletions here]  [Deny]         │
│ via Studio Mac · you · owner                                │
╰────────────────────────────────────────────────────────────╯
```

Safe default is visually primary and stated in words; destructive categories
(`file_delete`, `destructive`) default-select **Deny**, not Allow — the card
makes the safe path the easy path, per category policy shipped in Settings.
Keyboard: `y s a n` act on the top card; `j/k` move through the queue;
`x` dismisses an FYI; `enter` opens details.

**Empty state** (a feature, not an absence): serif line — *"You're free.
Three sessions are working; I'll tap you if anything comes up."*

### 1.3 The Conversation

The spine. A single ongoing thread with presence, rendered voice-register
first:

- **Presence messages** — prose, first person, in the voice face. May carry
  **inline artifacts**: a changes summary (files + diffstat, unfolds to the
  real diff), a frame/screenshot, a recording clip, a stat block, a file
  link, a mini table (e.g. cost of the finished task).
- **Milestone rows** — task started/steered/completed/failed, rendered as
  quiet one-liners with a *Show work* unfold that reveals the underlying
  activity-log lines (the same `AgentOutput`/`ModelResponse` stream Activity
  shows today). This is the register contract inside the thread.
- **Decision cards inline** — Queue items also land in the thread at the
  moment they arise (mirrored, not duplicated: resolving in one place
  resolves everywhere).
- **Owner messages** — right-aligned, quiet. `@` autocomplete aims at a
  session or machine (target chip shown above the composer, same resolution
  rules as today's prompt-target); `/` surfaces power grammar (real skills,
  thread actions, `/goal` for Codex); attach stages uploads
  (`/api/session/current/uploads`); the mic button starts live voice
  (`presence_connect` / `make_active`) with the transcript landing in the
  same thread (`UserTranscript`, `PresenceLog` — checkpoint continuity means
  voice and text share one conversation).
- **Composer placeholder** rotates honest examples: *"Ask the house
  anything… e.g. 'tidy my downloads folder', '@fix-login try the tests
  again', '/codex review the diff'."*

### 1.4 Now Playing & Today

- **Now Playing** — one *stage card* per active session (arch-headed):
  status **sentence** (from `SessionActivity` + `StatusSnapshot`, rendered
  as words: "Editing the login flow — turn 14, 12 files changed"), phase,
  model, a thin token/context meter, needs-you badge if it has Queue items.
  Primary action: **Open**. Hover/Studio: pause/stop/steer shortcuts. Peer
  sessions appear as cool-tinted, display-only cards (display-only per the
  trust model) with the peer's petname and route chip.
- **Today ribbon** — a horizontal day line: now marker; scheduled sessions
  (the agenda scheduler's `propose_scheduled_session` occurrences — the SPA's
  first surface for them **[fills a shipped-daemon gap]**); reminders;
  milestones as they land. Click an occurrence → its agenda item; the
  scheduler's approve/revoke owner actions live here.

---

## 2. Work — *the Stage*

Everything about sessions. Four lanes, one row of quiet tabs: **Live ·
Archive · New · Worktrees** (Deep Search folds into ⌘K and the Archive's
search field).

### 2.1 Live

The stage grid: stage cards for every active session (any host — the host
strip pattern from today's Sessions tab, now a *machine filter chip row*
with petnames). Orchestrations show their sub-agent fan as nested mini-cards
(parent edges, relationship kind: subagent/fork/side — `SessionRelationship`).
Sort: attention-first, then activity. Each card → the session Space.

### 2.2 Session Space (the deep surface — the heart of Work)

One page per session. Header: name (rename inline), status sentence, machine
+ route chips, model, cost so far, phase, Stop/Pause. Then five folds —
first three open at Standard, all open at Studio:

1. **Timeline** — the conversation/log (verbosity switch Normal/Verbose/Debug
   lives here, folded into the header's ⋯). Turns, tool calls with output
   previews, reasoning rows (one grammar across Claude thinking / Codex
   reasoning / native summaries), steer strip, follow-up queue, edit-user-
   message chip where `SessionCapabilities` allows. Infinite scroll +
   windowing, as today.
2. **Changes** — file list + full diff viewer; history timeline with
   Redo/Prune; rollback modal (files and/or conversation); abandoned-branch
   fold. (Today's Changes subtab, unchanged in capability.)
3. **Context** — the token-mass view. The Three.js scene remains available
   (Studio); Standard renders a quiet 2D category bar + "largest consumers"
   list (same snapshot data) — the 3D scene is a vantage, not a gate.
   Managed-context fold beneath: density stats, pressure meter, anchors,
   the rewind composer, records & backout, fission/lineage ledgers — the
   full Managed subtab, one unfold deeper.
4. **Controls** — launch config and backend knobs exactly as today's Control
   subtab (Codex: thread actions, sandbox, approval policy, model, effort,
   service tier, tools, writable roots; Claude: model, permission mode,
   allowed tools; native: approval rules per category). Gated by
   `SessionCapabilities` — never by backend identity.
5. **Vitals & lineage** — git branch/dirty/ahead-behind, prompt-cache
   receipt + TTL, rate-limit windows (`SessionVitals`); lineage fan
   (parent/children, fork points panel); recordings & frames; logs;
   background tasks; the report zip; delete-data modal.

Empty/idle/completed states each get real designs (archive resume card;
interrupted session with Resume/Rollback/Fork actions; abandoned with
why-it-was-abandoned).

### 2.3 Archive

The session list, calmer: search-first (quick search + message-search union,
as today), filter *tray* (Project / Source / Status / subagents / superseded)
collapsed into one control with active-filter chips, 10-way sort behind a
single "Sort" menu, windowed card list. Session detail overlay is replaced by
the session Space (2.2) — one deep surface, not two.

### 2.4 New

The launch form, voice-first: one big box ("What should the house do?") with
the full power folded behind **Options**: project path + Browse, worktree,
agent picker (Internal/Codex/Claude Code — with fueled/unfueled state inline:
*"Fueled — model credentials active"* / *"No model credentials — Add API
keys"*), execution mode, backend pins (per-backend folds), attachments.
Everything today's New Session form does; the calm version is one sentence.

### 2.5 Worktrees

Today's worktrees panel, re-carded: scan/cached, search, toggles
(Active/Dirty/Unmerged/Main), risk-first sort, inspect modal (metrics, review
reasons, dirty files, Open shell / Open files), remove, the session-linked
finish card. Same verbs, quieter room.

---

## 3. Screens — *see and touch your machines*

The stage-first workspace today's Live display tab built, rounded out with
the machine's other touch-surfaces:

- **Stage** — live display tiles (WebRTC), per-display toolbar (Take
  control / Release, Stream to voice, Attach frame, Annotate, Callout,
  Record, Fullscreen, Close), input-authority badge, CU overlays (agent
  cursor, ripples, key chips, screenshot flash — the honesty theater of
  watching it work). Empty state: *"No screens are live — screens appear
  when the house launches something graphical, or you share yours"* +
  New virtual display.
- **Your screen** — the private-view vs share-with-agent grant card
  (`GrantUserDisplay`/`RevokeUserDisplay`), duration-phrased.
- **Rail** — displays list, input-authority card with session attribution,
  display activity feed, peer displays.
- **Recordings & clips** — the player (MP4 segments + m3u8), annotation
  toolbar (pen/rect/circle/arrow/text), clip extraction (in/out, FPS,
  keyframes), Save/Attach/Send.
- **Terminals** — the PTY (xterm), target machine chip, share (terminal.view/
  write grants), mobile keybar. Lives here because a shell is how you *touch*
  a machine; power users pin it.
- **Browser workspaces** — the agent-driven browser: create (provider:
  auto/cdp/system_cdp/playwright/agent_browser/stream), lease
  (Acquire/Release), live view. (Today's Debug-tab surface, promoted to
  where an owner would look for it.)

---

## 4. Files

The mini-IDE, unchanged in capability, re-skinned: target machine chip
(blue = this daemon, mauve = peer — the existing whose-disk accent rule),
tree, tabs, CodeMirror, find-in-file, sha-guarded save, filesystem-grant
checks with a humane denial state ("The house may read here but not write —
ask for the key" + request affordance). **Transfers** beside it: downloads,
uploads (staged, conflict policy), resumable history.

---

## 5. Machines — *the house's wings*

The fleet, first-class:

- **Machine cards** — petname first, self-reported label muted (trust
  doctrine), status dot + words, route chip (`direct`/`fleet name`/`hosted`),
  role chip (`operator`, …), capability badges, pressure (CPU/mem from
  Station's host model), live sessions/displays fold (`PeerSnapshot`).
  This machine first, then peers.
- **Link a machine** — the pairing wizard, four modes (Request access /
  Join invite / Grant invite / Manual), plain-language steps, the honest
  consequence of each ("This creates a route, not authority — your IAM
  still decides").
- **Delegate** — message / delegate task / coordinator route (capability
  picker → eligible peers → route with instructions): the fleet's real
  superpower, today buried three folds deep in Access → Daemons.
- **Machine drill-down** — a machine opens its own Space: its sessions
  (browse-in-place), its screens, its files, its stats — the multi-host
  pattern today's host strips invented, given a room.

---

## 6. People & Keys — *who may do what*

Trust administration, humanized without lying:

- **You** — the identity hero: this browser's key, role, route, expiry;
  the trust-tier card with a one-line honest gloss.
- **People & devices** — who has access: grants with lifecycle chips
  (active/draft/revoked/expired), plain-language role previews ("An
  *operator* can run work and touch screens, but can't change who has
  keys"), enrollment requests (also in the Queue), join-with-org-grant fold.
- **Doors** — Connect status & account link, claim-code card (twelve words,
  one-time, "a name tag, not a key"), hosted-control leases (presets
  view/tasks/operate + the compiled floor, stated).
- **Keys & vault** — the vault (unlock/passkeys/entries), **fuel** (leases:
  grant/renew/revoke, memory-only stated), custody trail as a quiet ledger,
  client-egress registration, agent-account sign-in ceremonies
  (Claude/Codex PTY ceremonies, folded deep).
- **Organizations** — trusted roots, role caps, ORL. Studio-only fold.
- **Diagnostics** — route health grid, self-test. (The full IAM model —
  permission matrix, all-grants grid, model inspector — lives one fold down;
  Studio density opens here directly.)

---

## 7. Books — *the ledgers*

- **Costs & usage** — today's Usage tab, any machine: KPIs, estimated-cost
  grid, tokens-per-turn, activity skyline + month heatmap, disk usage.
- **The List** (agenda) — parked tasks/notes/questions, reminders policy
  (quiet hours), scheduled sessions + their occurrences (second home for the
  scheduler, with Today on Home), `ctl` hint for power users.
- **What the house remembers** (memory) — the claims explorer: kinds,
  sensitivity, candidate lane, durability honesty ("this plane is ephemeral
  in this build"), propose-a-claim.
- **Reports & history** — session report zips, the deep-search lane,
  per-session archives.

---

## 8. Settings — *how the house behaves*

Re-sorted from subsystems to **questions**:

1. **How much may the house do on its own?** — the autonomy dial (4 honest
   levels) + the 8 approval-rule rows (Auto/Ask/Deny, live-applied).
2. **Who provides the minds?** — API keys + status, provider/model mirrors,
   external-agent defaults.
3. **How does it reach you?** — presence (text/live providers), voice,
   transcription, notifications (title badge, browser notifications, push),
   quiet hours.
4. **What may it see and touch?** — computer use (backend, provider),
   recording (framerate/quality), display policy.
5. **How should it look and feel?** — theme, density, composer behavior.
6. **The fine print** — every remaining row, grouped, folded: env overrides,
   session report, advanced.

Every row — including folded ones — is in the ⌘K index under plain aliases.
At the very bottom, Studio density: **the raw file** — `intendant.toml`
rendered, validated on save, with a "what changed" diff line. The ultimate
power feature is admitting the truth is a text file.

---

## 9. Station *(vantage)*

The WASM constellation, framed as the house's war room: reached from the nav
(special glyph) and from any "constellation" link (session → see it in
Station). Proscenium chrome stays out of its canvas; the composer and Queue
badge overlay it. The DOM surfaces remain the accessibility floor — Station
is a presentation of the same control plane, as its own chapter demands.

---

## 10. Studio *(vantage)*

The expert layer, density Studio by default:

- **Workbench** — observer debug screen, diagnostics self-test, module/
  transport health (the three lanes: HTTP/WS/tunnel, with live status),
  `GET /debug` internals, visual-freshness rigs.
- **Raw state** — the control-plane snapshot, event stream (pauseable,
  filterable), IAM model inspector.
- **Reference** — the MCP tool list, the route table (derived from
  `gateway_routes::ROUTES`), keyboard map, the component field guide (every
  Proscenium component in every state — the prototype ships this as a real
  page).

---

## Cross-cutting states

Every room ships designed states, not afterthoughts: **loading** (skeleton
rows, never spinners alone), **empty** (serif line + one honest next action
— see the copy deck in `visual-language.md`), **denied** (IAM denial: what
you asked, whose key it needs, how to request it), **offline/reconnecting**
(the connection chip's story, per lane), **stale build** (the daemon
updated — reload), **module death** (the canary banner, kept). The
prototype's field guide renders each.
