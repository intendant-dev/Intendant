# Proscenium — the power model

How one app serves the owner and the operator without compromise. Four
mechanisms, each simple, together load-bearing.

---

## 1. The disclosure contract

> **Nothing is hidden. Everything unfolds.**

Every summary view carries its machinery exactly one unfold away, and the
unfold is *the same gesture everywhere*: a `┄ details ┄` / chevron row that
expands in place. Folds remember their state per user per object kind (a
power user who always opens Changes finds it open; grandma never sees it
until she asks). No capability exists only at some density, behind some
mode, or on some other screen — if you cannot find a thing, the index
(§4) takes you to its exact folded row.

The contract bans two failure modes the current dashboard and naive
redesigns both suffer: **clutter** (everything visible for everyone — the
cockpit) and **burial** (power removed to a separate "advanced" app, or
renamed per level so search and docs fork). Proscenium folds; it never
amputates and never renames.

---

## 2. The two registers

A rendering policy applied to every surface:

| | **Voice register** | **Instrument register** |
|---|---|---|
| Language | sentences ("Editing the login flow — 12 files changed") | facts (`turn 14 · 12 files · gpt-5.2-codex · $0.83`) |
| Nouns | stable noun + short gloss: "Fuel — the API keys the house runs on" | the bare noun: "credential lease" |
| Type | serif voice face, generous measure | mono, compact, tabular |
| Defaults | safe choice pre-selected, consequence stated | every choice visible, no pre-selection |
| Truth | a summary *of* the raw | the raw: IDs, routes, JSON |

Rules:

- **Stable nouns, glossed — never renamed per level.** A *session* is a
  session at Cozy and at Studio. Glosses are a layer over the real
  vocabulary (the codebase's own words: session, display, peer, lease,
  grant, vault, anchor, fuel), so docs, search, support, and the two
  registers all speak one language.
- **Voice never fabricates.** Every voice-register sentence is a rendering
  of daemon facts (`SessionActivity`, `StatusSnapshot`, events) and links
  its source. If the house can't know, it says so — the honest-seams
  doctrine applied to copy.
- **Instrument is always one gesture away** (unfold, ⌘K, or density notch)
  and requires no mode flip to *reach* — only to *see first*.

---

## 3. The density dial — Cozy / Standard / Studio

One global, per-browser preference. A rendering policy, not a feature gate.

| | **Cozy** | **Standard** (default) | **Studio** |
|---|---|---|---|
| Register | voice-first | voice summary + instrument folds | instrument-first |
| Type/spacing | 15px base, generous | 14px base | 13px, compact, tabular |
| Folds | at rest | first fold open on detail views | all open |
| Meta lines | hidden (words only) | summary facts | IDs, routes, token detail inline |
| Home layout | single column, queue → thread → now playing | single column + vitals rail | three-column ops grid |
| Shortcut hints | hidden | in tooltips | visible inline |
| Raw JSON | via ⌘K "inspect raw" | one fold | inline where useful |
| Nav | 6 rooms + overflow | all rooms | all rooms + Studio, section counts |

The dial never gates: ⌘K searches the full instrument vocabulary at every
density, and a Cozy machine operated by a power user loses nothing but
keystrokes to unfold. It persists in `localStorage`, is settable from
Settings → "How should it look and feel?", the header, and ⌘K
("density studio"), and is purely client-side.

---

## 4. The index — ⌘K

The power user's home row; the owner's polite concierge. One box, four
lanes, always in this order:

1. **Actions** — verb-first, context-aware: *Approve top queue item*, *Stop
   'fix-login'*, *New session*, *Pair a machine*, *Fuel from vault*, *Take
   control of Display 1*, *Flip theme*, *Set density Studio*. Actions know
   their preconditions (no active session → no *Stop*).
2. **Go to** — every room and every named object: sessions, machines,
   people, displays, worktrees, agenda items, files (shallow index), peers.
3. **Settings** — *every settings row, folded or not*, under plain aliases:
   "writable roots", "codex sandbox", "quiet hours", "approval rules",
   "reasoning effort". Enter jumps to the room, opens the fold, flashes the
   row. This lane is what makes "power users can find every little config"
   a guarantee rather than a hope.
4. **Search deeper** — the async lanes: session metadata search and the
   full-log message search (today's Deep Search), results unioned.

Behavior: fuzzy match (prefix > substring > subsequence), recency boost,
mouse-free by default (`↑↓` move, `↵` act, `→` peek details, `esc`), and
**register-blind**: the index always speaks instrument vocabulary even at
Cozy, because search is where precision belongs. Alias coverage is
hand-tended where plain words diverge from nouns ("fuel" → API keys +
leases + vault).

**Derivation, not mirroring:** lane 3's catalog is generated from the same
settings payload/schema the Settings room renders; lane 1's actions from a
declared table beside the room registry; object lanes from existing
snapshots. A settings row added without an index entry fails a parity
test — the index can never silently drift from the surface (the codebase's
derive-don't-mirror doctrine, applied to search).

---

## 5. The keyboard map

Global, discoverable (`?` opens the map), consistent with what the
dashboard already ships:

| Key | Where | Does |
|---|---|---|
| `⌘K` / `/` | everywhere | the index |
| `?` | everywhere | shortcut map |
| `g` then letter | everywhere | go to room: `h` Home, `w` Work, `s` Screens, `f` Files, `m` Machines, `p` People & Keys, `b` Books, `,` Settings, `t` Station, `u` Studio |
| `y` `s` `a` `n` | queue / approval | approve / skip / approve-all-like-this / deny (existing grammar) |
| `j` `k` | queue, lists | next / previous |
| `x` | queue | dismiss FYI |
| `↵` | cards | open / primary action |
| `e` | cards | unfold details |
| `⌘↵` | composer | send |
| `esc` | layered | close topmost layer (existing contract) |

Power flows: `⌘K writable roots ↵` (lands on the folded row, flashing);
`y y a n` (clear four queue items without the mouse); `g w` → `j j ↵`
(open the third live session).

---

## 6. Attention & escalation policy

The Queue's reach, in escalating order, each step opt-in and honest:

1. **In-app** — header badge (count + severity), Home pin, favicon dot and
   `(N)` title prefix (today's attention center, kept).
2. **Browser Notifications** — for hidden tabs, opt-in (existing).
3. **Web Push** — for closed tabs, via Connect, payloads never carrying
   work content (existing doctrine). Urgency mapping follows
   `NotificationUrgency` (Info/Attention/Urgent).
4. **Voice** — if a live voice session is active, presence *says* the
   queue item aloud, in the first person, and takes the answer
   conversationally (`approve_action`/`deny_action` presence tools already
   exist).

The owner should be able to run the house from the lock screen or the
garden: the Queue is the same items at every reach, resolving everywhere at
once.
