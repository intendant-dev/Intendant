# Proscenium — a design concept for the Intendant dashboard

> **Status:** design concept + interactive prototype, 2026-07-17.
> Self-contained, no build step: open `prototype/index.html` in any browser.
> Nothing here touches the shipped dashboard (`static/app/`); this is a
> proposal to be evaluated on its own terms, then adopted incrementally if it
> earns it. A sibling concept, **Atrium**, lives at `docs/design/atrium/` in the
> `design/dashboard-everything-concept` worktree — see *Relationship to Atrium*
> below for an honest comparison.

The name is already in the codebase: the Intendant brand glyph is annotated
*Proscenium arch* (`static/logo-glyph.svg`). The proscenium is the frame
between the audience and the stage — the place the owner sits to watch,
applaud, and occasionally stand up and say something. The owner never has to
go backstage. Backstage is fully lit if they want it.

---

## The observation

The product's own thesis (`AGENTS.md`, `docs/src/introduction.md`):

> Agents perform, orchestrators conduct, the Intendant runs the house — and
> answers to the owner.

The current dashboard is a superb **operator's cockpit**: thirteen tabs named
after subsystems (Activity, Sessions, Agenda, Memory, Live display, Station,
Terminal, Files, Usage, Access, Vault, Settings, Debug), every gauge visible,
every knob one navigation away. It is built for the person who flies the
plane. But the thesis says the owner doesn't fly the plane — the Intendant
does. The owner *answers questions, sets policy, and enjoys the show*.

Two people open this app:

- **The owner** (the thesis's human; "grandma" in the brief) wants three
  things: *talk to the house, answer what needs answering, see that things
  are going well.* She should never need to know what a "managed-context
  rewind anchor" is.
- **The operator** (the power user) wants the full control plane — every
  route, grant, launch pin, lease, and ledger line — fast, without the calm
  layer taxing every interaction.

Today both get the cockpit. Proscenium is one app that is a home for the
first person and a cockpit for the second, mediated by the one character the
codebase already cast for exactly this role: **presence**.

---

## The concept in one paragraph

The dashboard stops being a console of subsystems and becomes **the owner's
relationship with their house**: a first-person conversation with the
Intendant as the home screen; a single *Needs You* queue that collects every
decision the house cannot make alone; a formal two-register rendering policy
(*voice* for humans, *instrument* for machinery) with a density dial; and the
entire control plane — every route, tunnel method, and ControlMsg the daemon
exposes — organized into intent-named rooms, every one of them one ⌘K search
away. Calm enough to hand to your grandmother. Nothing deleted, nothing
dumbed down: power is *folded*, not removed.

---

## The four load-bearing ideas

### 1. The Conversation is the home screen

`docs/src/presence.md` already defines the soul of this surface:

> The user talks to presence, not directly to the worker agent. Presence
> speaks as Intendant in the first person ("I'm working on that now", not
> "the agent is working on that").

Proscenium takes that sentence literally and makes the **presence
conversation the default screen** — not a chat drawer beside the cockpit, the
home. Presence greets you, narrates ("Three turns into the login fix — the
tests pass; one thing needs you"), renders **work artifacts inline** (a diff
summary, a screenshot, a recording clip, a cost figure, a finished file), and
takes your next wish in the composer. The composer is the same one the
codebase already ships globally — talk, `@`-aim at a session or machine,
`/`-command for power grammar, attach, or hold the mic and speak (live voice
already exists; here it is simply first-class rather than an oversight-bar
popover).

Crucially, the conversation is **honest about its machinery**: every presence
milestone carries a *Show work* disclosure that unfolds the underlying
activity-log lines it summarizes. The voice register is a rendering of the
truth, never a replacement for it — one unfold, and you're looking at the raw
turn-by-turn log. (This is the two-register contract of idea 3, applied to
the thread itself.)

**The briefing.** Because presence mediates, the app gains a daily rhythm no
console has: open it after time away and the first message is a *briefing* —
what finished, what failed and was retried, what is waiting on you, what's
scheduled today. Every input to the briefing already exists daemon-side
(session events, agenda ledger, scheduler, usage snapshots); rendering it is
a presence query away. The agenda subsystem's **scheduled sessions** shipped
daemon-side (`agenda/scheduler.rs`, `propose_scheduled_session`) and were
never surfaced in the SPA — Proscenium's *Today* ribbon finally gives them a
home.

### 2. The Queue — *Needs You* is the owner's inbox

The owner's one job in this architecture is to decide what only the owner may
decide. Today those decisions are scattered across at least seven surfaces:
the approval panel (Activity), the AskHuman and structured-question panels,
the display-request doorbell, peer approvals (Access → Daemons), enrollment
requests (Access → People & Devices), the hosted-control gate, and unfueled /
dry-lease warnings.

Proscenium unifies every one of them into **the Queue** — one severity-ordered
inbox, pinned to the top of Home and badged from every screen:

- **Each item is a decision card in plain language**: *"Claude wants to
  delete 3 files in `~/projects/exports`"* — with the honest consequence
  stated, the **safe default pre-selected**, a *Details* unfold carrying the
  raw payload (command, paths, diff, requesting session, expiry), and the
  provenance line the trust doctrine mandates (*"via Studio Mac · you ·
  owner"* — every pane states whose authority it spends).
- **Resolving is one tap or one key** (the existing `y` / `s` / `a` / `n`
  grammar). Approvals keep their full existing semantics — Approve, Skip,
  Approve all like this (category rule), Deny — nothing is lost.
- **Two tiers**: *Decisions* (blocking — approvals, questions, doorbells,
  enrollments, fuel) and *For your awareness* (budget warnings, finished
  watched tasks, lease-expiry notices, reminders). Awareness never blocks;
  it dismisses.
- **Empty is a feature**: *"You're free."* Inbox zero for your house. The
  badge disappears; the house hums.

Every Queue item is a renderer over events that already exist
(`ApprovalRequired`, `UserQuestionRequired`, `DisplayRequestRaised`, peer
`ApprovalRequested`, enrollment-request routes, hosted-control requests,
`BudgetWarning`, api-key status). No new backend is required for v1; a
daemon-side unified attention feed can come later as a derived control-plane
projection (see `capability-map.md`).

### 3. Two Registers — every surface speaks human first, machine on demand

Proscenium's progressive disclosure is not a mode that hides things; it is a
**rendering policy** applied everywhere, with a hard contract:

> **Nothing is hidden. Everything unfolds.**

- **Voice register** — plain sentences, stable nouns with short glosses
  ("Fuel — the API keys the house runs on"), generous type (a serif voice
  face for prose), safe defaults, words over enums. This is how the house
  *speaks*.
- **Instrument register** — the exact machinery: IDs, routes, roles, counts,
  JSON, mono type. Always one unfold, one ⌘K, or one density notch away —
  never absent, never requiring a mode flip to *reach*, only to *see first*.

A global **density dial** — **Cozy / Standard / Studio** — retunes which
register greets you: Cozy is voice-first with folds at rest; Standard is the
balanced default; Studio is instrument-first (folds open, IDs and routes
visible inline, dense grids, shortcut hints shown). It persists per browser
and changes presentation only: every capability exists at every density. A
power user on a Cozy-configured machine loses nothing — ⌘K searches the full
instrument vocabulary regardless.

One deliberate rule, differing from the sibling Atrium concept: **nouns are
never renamed per level.** A session is a session at every density; Cozy adds
a gloss, it does not swap in a euphemism. Two vocabularies would be two
languages to learn and would break search, docs, and support. Plain language
is a *layer over* stable names, not a replacement for them.

### 4. The House — depth organized by intent — and one search that finds everything

Behind the Conversation, the thirteen subsystem tabs become **eight rooms
plus two vantage points**, named for what you came to do:

| Room | Answers | Absorbs (today) |
|---|---|---|
| **Home** | "Talk to the house. What needs me? What's happening today?" | Activity (log), approvals/questions/doorbells, Agenda (today), presence voice |
| **Work** | "How's the job going? Give it work." | Sessions (Recent/Deep Search/Worktrees/New), Activity subtabs per session (Log/Context/Managed/Changes/Control), sub-agents, forks, lineage |
| **Screens** | "Show me a machine. Let me touch it." | Live display, recordings/clips, annotations, your-screen sharing, Terminal, browser workspaces, virtual displays |
| **Files** | "Get me that file. Move these." | Files editor + Transfers |
| **Machines** | "What machines do I have? Link another. Send work there." | Access → Daemons/Peers (pairing, routes, petnames, delegation, coordinator), multi-host browsing |
| **People & Keys** | "Who can do what? Hand out a key. See the keys I hold." | Access → Overview/People & Devices/Diagnostics/Advanced, orgs, Vault, leases, custody trail, hosted control |
| **Books** | "What's it costing? What did it do? What does it remember?" | Usage, Agenda (the List), Memory, session reports/archives |
| **Settings** | "Change how the house behaves." | Settings (re-sorted as questions), appearance, autonomy & approval rules |
| **Station** *(vantage)* | "Immerse me." | Station — the WASM constellation stays the war-room view, reached from anywhere |
| **Studio** *(vantage)* | "Open the machinery." | Debug (observer display, diagnostics), raw state, transport lanes, MCP tool reference, component field guide |

The mapping is a **relocation, never a deletion** — `capability-map.md`
carries the full route-by-route guarantee, compiled against
`gateway_routes::ROUTES`, the tunnel method table, and the ControlMsg
vocabulary.

And because no taxonomy survives contact with a power user: **⌘K is the
universal index** — rooms, objects (sessions, machines, people, displays,
files), every individual setting including folded ones ("writable roots" →
the Codex sandbox row), every action (approve, pair a machine, fuel, stop
session, flip theme), help explainers, and the session deep-search lane.
Enter jumps to the exact control — opening its fold and flashing it. If you
know what you want, you never navigate at all.

---

## A day with Proscenium

**The owner.** Morning push: *"2 things need you."* She opens the app. The
briefing, in the serif voice: *"Good morning. The photo-book layout finished
overnight — it's in Files. The backup failed once and I retried it; fine now.
Claude wants to delete old exports, and a new laptop is asking for access."*
Two decision cards: she reads the consequence, taps **Allow** on the first,
**Not now** on the second. *"You're free."* She types *"show me the photo
book"* — presence opens the Files room at the finished folder. She never saw
the words *session*, *approval category*, or *enrollment*.

**The operator.** Opens, ⌘K → `codex writable roots`, lands on the folded
row, flips it. `y` `a` `n` through the Queue without touching the mouse.
Density to Studio: the Work room becomes a dense grid with IDs, routes, and
token facts inline. Into a session's rewind composer; out to the
constellation in Station; a glance at the custody trail; a raw-JSON inspection
of a grant. The calm layer never taxed a single keystroke — it simply wasn't
in the way.

Same app. Same routes. Same control plane. Two registers.

---

## Visual language — *"House lights"* (summary; full spec in `visual-language.md`)

The current UI is a blue-black iris dev-tool; Atrium goes to paper and
botanical green. Proscenium takes the third road the brand itself offers —
the **theater at night, warmed by lamplight**, with the conductor's baton as
the accent:

- **Lamplight (dark, default):** warm brown-charcoal surfaces (never
  blue-black), brass-gold accent drawn from the glyph's baton gradient
  (`#f9e2af → #a68a5b`), warm off-white ink.
- **Daylight (light):** warm paper and true ink, brass deepened for contrast.
- **The color doctrine: warm is yours, cool is the machinery's.** Authority,
  attention, and the house's own chrome live in the warm family (brass,
  marigold, brick). Agent/machine state lives in the cool family (sage for
  healthy work, slate for information). A glance at any screen reads the
  provenance of its content before a word is read. Chips stay labeled in
  words, always — color remains garnish, never the message (existing
  doctrine, kept).
- **Typography makes the two registers literal:** a serif *voice* face
  (Iowan Old Style / Palatino / Georgia stack) for greetings, briefings,
  decision headlines, and empty states; Hanken Grotesk (already self-hosted)
  for UI; JetBrains Mono (already self-hosted) for the instrument register.
- **The arch** is the quiet motif: the brand glyph; the *stage cards* of
  Now Playing, whose header band carries the arch curve; a one-time
  curtain-lift on first paint (motion-safe, `prefers-reduced-motion`
  honored). No skeuomorphism beyond that — this is a tool, not a theme park.

---

## A note on time

Consoles show state; relationships happen over time. Proscenium adds the time
dimension the codebase already has data for: the **briefing** (what happened
while you were away), the **Today ribbon** (now marker, scheduled sessions
and reminders from the agenda scheduler, milestones as they land), and
**history as a first-class citizen** (Books). The daemon already owns
long-running work, reminders, and a scheduler the SPA never surfaced — the
design catches the UI up to the backend.

---

## Relationship to Atrium

Atrium (`docs/design/atrium/`, sibling worktree) is a good concept and this
one borrows none of it accidentally. The honest differences:

| | **Atrium** | **Proscenium** |
|---|---|---|
| Center of gravity | The **object model** — everything is a card; cards open Spaces; one Inspector holds details | The **relationship** — presence conversation as home; the Queue as the owner's inbox; objects serve the thread |
| Daily loop | A dashboard you visit | A house that briefs you; inbox-zero as the goal state; time (Today, schedule) as structure |
| Disclosure model | Simple/Balanced/Expert dial that renames vocabulary per level ("Screens" vs "Displays") | Two **registers** over **stable nouns** + Cozy/Standard/Studio density; nothing renamed, everything unfolded |
| Presence | A composer row | The spine — first-person voice, inline artifacts, briefing, voice parity |
| Prototype depth | Static single page | Fully interactive multi-room build: working ⌘K index, live queue resolution, density + theme engines, simulated event stream |

They agree on the disease (subsystem IA, cockpit-for-everyone) and on several
remedies (one search, one inspector-like depth container, a level dial that
is a rendering policy). They are two different cures; if the fleet prefers
Atrium's object grammar, Proscenium's Queue, briefing, and register contract
still apply wholesale.

---

## What this does not change (invariants)

- **Frontends are renderers, never a second brain.** Every mutation here is
  an existing ControlMsg / tunnel method / route; the prototype's data model
  mirrors daemon snapshot shapes for exactly this reason.
- **The trust doctrine stands.** Authority badges on every pane, route
  provenance chips, petnames first, ceilings and ceremonies untouched —
  presented more quietly, never less truthfully. `role:none` hosted
  provenance remains `role:none`.
- **Derive, don't mirror.** The ⌘K settings index, the Queue, and the room
  catalogs are all derivations over existing declared tables (ROUTES, IAM
  catalog, settings payload) — the implementation plan keeps them derived
  with parity tests, in the spirit of `tunnel_method_partition_is_pinned`.
- **Accessibility floor.** Keyboard-first flows, visible focus, labeled
  chips, reduced motion, the DOM surface as the a11y floor even as Station
  grows.
- **Station stays** the immersive successor surface; Proscenium gives it a
  place in the IA rather than competing with it.

---

## Adoption path (if the concept earns it)

1. **Tokens + registers** land as a `ui3-*` fragment family beside ui-v2
   (the alias-layer trick the v2 tokens already use makes this cheap).
2. **Home + Queue** ship as a new default destination beside the existing
   rail — no removals — fed from the existing snapshot/event stream; the
   seven scattered attention surfaces gain a shared item model.
3. **Rooms** absorb tabs one at a time (Work first — the session Space is
   the deepest), each behind the hash router, old routes redirecting.
4. **⌘K universal index** (settings + actions + objects) with jump-and-flash.
5. **Density dial** last, once Studio demonstrably covers the old surface —
   pinned by a parity test against the ROUTES/tunnel derivation.

Each step is independently shippable and independently revertable.

---

## The doc suite

- `README.md` — this file: the concept.
- `surfaces.md` — every room specified: layout, contents, states, data.
- `power-model.md` — the two registers, density dial, ⌘K spec, keyboard map,
  the disclosure contract.
- `visual-language.md` — tokens, color doctrine, type, arch motif, motion,
  accessibility.
- `capability-map.md` — the coverage guarantee: every route, tunnel method,
  and ControlMsg, and its Proscenium home.
- `prototype/` — the interactive build. Open `prototype/index.html`.

---

## Prototype tour

`prototype/` is a clickable, self-contained build (no server, no build step —
open `prototype/index.html` in any browser; mock data modeled on the real
ontology: the fleet machines, the eight approval categories, builtin roles,
peer profiles, claim codes, leases, custody). Things to try, in an order that
tells the story:

1. **The owner's morning** — Home opens on the briefing and the Queue. Read
   a decision card, unfold its *details*, then press `y` (or click) to
   resolve it. Watch the badge count down to *"You're free."*
2. **Talk to the house** — in the composer, type `show me the cover`, then
   `tidy my downloads folder`. The thread answers, and a new stage card
   appears under Now Playing.
3. **A live event arrives** — the ✦ button in the top bar injects an event
   (an approval from another machine, a task finishing, a doorbell). Try it
   from a room other than Home: the badge and drawer carry it.
4. **The deep surface** — open Work → the `fix-login` stage card: timeline
   with steer strip, real diff, context budget, backend controls, approval
   rules, vitals, lineage, managed-context rewind. This is where power
   users live.
5. **⌘K is the index** — press `⌘K` (or `/`) and type `writable roots`.
   Enter lands on the exact folded row in Settings and flashes it. Try
   `quiet hours`, `pair a machine`, `approve`, `fix-login`.
6. **The density dial** — top bar, `standard` → click to cycle Cozy /
   Standard / Studio (or ⌘K → "cycle density"). Studio opens every fold,
   shows IDs inline, tightens the grid. Cozy speaks first. Same routes,
   same capabilities — three depths.
7. **House lights** — the theme button flips Lamplight ⇄ Daylight.
8. **The keyboard** — press `?` for the map; `g w` then `g h` to move
   between rooms without the mouse.
9. **The rooms** — Screens (a "live" display with the agent's cursor at
   work, input authority, your-screen grant), Files (the desk, transfers,
   a grant-denied state), Machines (the fleet, pairing wizard, delegate),
   People & Keys (grants, the vault, fuel, custody trail), Books (costs,
   the List, memory, reports), Settings (questions → the raw TOML),
   Station (the constellation), Studio (the machinery, raw + the component
   field guide).

Everything resolves, unfolds, searches, and navigates client-side; nothing
phones home. Resolved queue items persist per browser (clear
`localStorage['proscenium.queue.resolved']` to reset the morning).
