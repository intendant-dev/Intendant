# V3 implementation plan — Proscenium becomes the dashboard

> Status: **landed as a draft** on branch `design/dashboard-north-star`
> (2026-07-17) — `/v3` serves the full room set, verified end-to-end
> against a mock-provider daemon (composer task → approval in the Queue →
> approve in-page → completion milestone, plus every room rendered
> error-free). Decisions taken with the owner on 2026-07-17:
>
> - **Architecture: full replacement trajectory.** V3 is a *new, standalone
>   front-end codebase* — not a re-skin of the ui-v2 DOM and not a wrapper
>   around its JS. It talks to the daemon directly (HTTP routes + `/ws`
>   events; the datachannel tunnel only where HTTP/WS genuinely can't go).
>   When it proves itself, `static/app.html` gets replaced; until then the
>   two coexist.
> - **Default: opt-in.** V2 stays the default at `/`. V3 lives at `/v3`
>   (embedded like app.html, with an `INTENDANT_V3_HTML_PATH` dev override
>   mirroring `INTENDANT_APP_HTML_PATH`), linked from V2 ("Try V3"). One
>   line flips the default when the time comes.
> - **Depth: deep core + every room re-presented.** Home/Queue/Conversation,
>   ⌘K, Work/Session Space, Settings, density/theme engines at full depth;
>   Screens, Files, Machines, People & Keys, Books, Station, Studio as real
>   V3 rooms over the same routes — plus beyond-parity surfaces the daemon
>   already supports but the SPA never exposed (the agenda scheduler,
>   quarantine, invoke_skill, unified doorbells).
>
> The concept and its design language: `README.md`, `surfaces.md`,
> `power-model.md`, `visual-language.md`, `capability-map.md` in this
> directory. The interactive concept prototype (mock data):
> `prototype/index.html`.

## Why a clean-room front-end (and what we still borrow)

The ui-v2 chrome re-presents v1's DOM; V3's information architecture
(conversation-first Home, the unified Queue, intent-named rooms, two
registers, density) is a different *shape*, not a different skin — re-
presentation would fight the old DOM at every turn. So V3 is clean-room:

- **No build step, like the target it replaces.** `static/v3/` is plain
  HTML/CSS/JS (classic scripts, one `V3` namespace), embedded into the
  binary via the existing `static_assets.rs` table and served at `/v3/*`.
  No bundler, no framework, no new dependencies.
- **Everything mutating rides the existing rails.** Tasks, approvals,
  questions, display grants, autonomy, approval rules, agenda ops — all go
  out as the same ControlMsgs over `/ws` and the same HTTP routes the V2
  SPA and MCP use. V3 is a renderer, never a second brain (the control-
  plane invariant, unchanged).
- **Derive, don't mirror.** The ⌘K settings index derives from
  `GET /api/settings`; the queue derives from the event stream + polled
  routes; the rooms catalog is one declared table. Parity tests pin what
  must not drift.

## The work, in phases

| # | Phase | Proof |
|---|---|---|
| 1 | Wire-protocol recon (auth, WS frames, JSON shapes, serving) | this plan's appendix |
| 2 | Rust plumbing: embed `static/v3/*`, serve `/v3` + `/v3/*`, dev override, asset-table tests, "Try V3" link in V2 | `cargo test --bins` |
| 3 | V3 shell: tokens, chrome (rail/topbar/composer), density+theme engines, boot/auth, transport (WS + HTTP + reconnect) | boots against mock daemon |
| 4 | Home: unified Queue (approvals, questions, display doorbells, enrollment, peer approvals, fuel, FYI), Conversation (presence + milestones + show-work), Now Playing, Today (agenda + **scheduler — first UI**) | Playwright flows vs mock |
| 5 | ⌘K universal index: rooms/objects/actions/**every settings row** + deep search, jump-and-flash | flows |
| 6 | Work + Session Space (timeline, changes, context, controls, vitals/lineage) over the real catalog + replay routes | flows |
| 7 | Settings V3: question-sorted, real GET/POST round-trip, raw TOML view | flows |
| 8 | Rooms re-presented: Screens, Files, Machines, People & Keys, Books, Station (link to the WASM canvas), Studio | flows |
| 9 | Beyond-parity: scheduler card, quarantine FYI, invoke_skill in ⌘K, unified peer/hosted doorbells | flows |
| 10 | Narrative coherence: `docs/src/web-dashboard.md` V3 chapter, this plan's status, CLAUDE.md/AGENTS.md sync (byte parity) | docs gate |
| 11 | Battery: `cargo test --bins`, `cargo clippy`, mock-daemon E2E, `scripts/validate-dashboard.cjs` boot probe unaffected | green |
| 12 | **Draft PR** (`gh pr create --draft`), no auto-merge | URL |

## Guardrails

- V2 (`static/app/`, `static/app.html`) is **untouched** except the "Try
  V3" link; the app-html regen gate stays green.
- No `unsafe`, no new crates, no changes to the IAM/trust model — V3 is a
  client of it, badges and all.
- The V3 page never ships secrets; auth is exactly what the daemon grants
  the serving context (same as app.html today).
- File-size budget and code conventions per the repo's AGENTS.md.
