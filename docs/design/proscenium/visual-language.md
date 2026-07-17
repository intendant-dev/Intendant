# Proscenium — visual language: *House lights*

The current dashboard is a blue-black iris dev-tool. Proscenium is the
**theater at night, warmed by lamplight** — the owner's side of the
proscenium arch — with the glyph's golden baton as the accent. Two themes,
one token system, two type registers, one motif.

---

## 1. The color doctrine

> **Warm is yours. Cool is the machinery's.**

The house, the owner, authority, and anything needing a human decision live
in the **warm family** (brass, marigold, brick). Agents, machines, and
informational state live in the **cool family** (sage, slate). Provenance is
legible before a word is read. Chips stay labeled in words — color is
garnish, never the message (existing doctrine, kept verbatim).

### Lamplight (dark — default)

| Token | Value | Use |
|---|---|---|
| `--bg` | `#14120F` | app ground, warm near-black |
| `--bg-1` | `#191612` | sunken wells (composer, log) |
| `--surface` | `#1E1A15` | cards |
| `--surface-2` | `#26211A` | raised cards, popovers |
| `--surface-3` | `#2F2920` | hover, active wells |
| `--line` | `rgba(242,232,213,.09)` | hairlines |
| `--line-2` | `rgba(242,232,213,.16)` | stronger seams |
| `--text` | `#F1EADC` | warm off-white ink |
| `--text-2` | `#C6BAA4` | secondary |
| `--text-3` | `#91866F` | tertiary, timestamps |
| `--brass` | `#D2A24C` | primary accent — the baton; primary actions, focus rings, brand |
| `--brass-2` | `#E4BC72` | hover/highlight |
| `--on-brass` | `#241A08` | text on brass |
| `--attn` | `#E8A33D` | *needs you* — queue badges, decision accents |
| `--brick` | `#D97B66` | danger/deny/destructive |
| `--sage` | `#8AB894` | healthy work, success, *working* state |
| `--slate` | `#7FA6C9` | machine/agent information, links to instruments |
| `--violet` | `#A58BD4` | peer/federation provenance (kept from ui-v2's mauve peer rule) |

Each semantic ships an `-rgb` triplet for alpha tints (fill 8–14%, border
~25%, text full) — the ui-v2 pattern, kept.

### Daylight (light)

Ground `#F7F3EA` (warm paper), surface `#FFFDF7`, ink `#262018`,
secondary `#5F574A`, brass deepened to `#A67C1F` for contrast
(AA on paper), attn `#B97F0F`, brick `#B84B39`, sage `#3F7D4E`, slate
`#33608F`. Same tokens, remapped — the flip is pure CSS variables, as
today.

Both themes meet WCAG AA for body text (4.5:1) and UI chrome (3:1); the
brass/on-brass and sage/text pairings were chosen against both grounds.

---

## 2. Typography — the registers made literal

| Register | Face | Where |
|---|---|---|
| **Voice** | `Iowan Old Style, Palatino Linotype, Palatino, Georgia, serif` | greetings, briefing prose, decision-card headlines, space titles, empty states, quoted results |
| **UI** | `Hanken Grotesk` (self-hosted, as today) | chrome, labels, buttons, forms, chips |
| **Instrument** | `JetBrains Mono` (self-hosted) | logs, diffs, IDs, paths, JSON, metrics, token counts |

Scale (Standard density): 14px base / 1.5; voice prose 16–18px; display
(greeting) 28–34px; eyebrow 11px small-caps with .12em tracking (one recipe,
everywhere). Cozy +1px and +air; Studio −1px and −air, tabular numerals for
fact columns. Inputs 16px on touch devices (iOS zoom rule, kept).

---

## 3. Shape, elevation, the arch

- **Radii ladder:** 8 inset / 10 control / 14 card / 20 panel / 999 pill.
- **The arch** — the one motif, used three ways only:
  1. the brand glyph (already a proscenium arch with the baton),
  2. **stage cards** (Now Playing / Live work): the header band's top edge
     carries the arch curve (`border-radius: 46% 46% 0 0 / 18px 18px 0 0`
     on the band; a card that reads as a lit stage),
  3. the first-paint **curtain lift** (below).
  Nothing else arches. Restraint is what keeps it a signature.
- **Elevation:** warm-tinted shadows (`0 1px 0 rgba(255,240,214,.03)` inset
  + `0 8px 24px rgba(0,0,0,.35)` on overlays); no blur theatrics on
  persistent surfaces (the WebGPU recomposite rule, kept); glass reserved
  for floating chrome over the stage.
- **Hairlines over fills** for structure; fills reserved for state.

---

## 4. Motion

- **Curtain lift** — first paint only: a 500ms warm fade from `--bg` with
  the arch glyph settling; once per session, never on navigation.
- **Decisions arrive** — queue cards slide-settle from the top with a
  gentle 220ms ease; resolving one lifts it away and slides the rest up
  (the *you're free* moment should feel like a breath).
- **Spotlight** — stage cards carry a soft radial highlight that follows
  the pointer (8% brass tint); off at Cozy, off under
  `prefers-reduced-motion`.
- Everything else: 120–180ms ease-out transforms + opacity; no springs, no
  bounce, no parallax. `prefers-reduced-motion` collapses all of it to
  instant (existing global rule, kept).

---

## 5. Iconography

Inline 24×24 stroke SVGs, 1.7 stroke, round caps — the ui-v2 registry
pattern (`ui2Icon`), re-drawn where the house metaphor offers a better
glyph (doorbell for the queue, baton for actions, wings for machines, key
for People & Keys, ledger for Books, arch for Home). **No emoji** in chrome
(the v2 rule, kept). Status dots always paired with words.

---

## 6. The copy deck — how the house speaks

Voice-register copy rules: first person ("I'm working on it"), contractions
welcome, no exclamation marks, no robot apology, numbers in words under
ten, and *the honest consequence in every ask*. Samples the prototype
ships:

- Greeting: *"Good morning. Two things need you; the rest is humming."*
- Queue empty: *"You're free. Three sessions are working — I'll tap you if
  anything comes up."*
- Unfueled: *"This daemon has no fuel yet — the API keys it runs on. Fuel
  it from your vault and it's off."*
- Doorbell: *"The session 'fix-login' is asking to see this screen. It can
  look but not touch, for 15 minutes."*
- Denied (IAM): *"You asked as a spectator. That needs an operator key —
  ask the owner, or request it here."*
- Empty archive: *"Nothing finished yet. Give the house something to do —
  one sentence is enough."*
- Offline: *"The line to Studio Mac went quiet. Reconnecting…"*

---

## 7. Accessibility contract

- Keyboard-first: every flow in `power-model.md` §5 works without a
  pointer; visible `:focus-visible` brass ring (2px, offset 2).
- ARIA: rooms are `role=main` landmarks; the Queue is `role=log`
  `aria-live=polite`; decision cards expose their actions as a group with
  the safe default first in tab order; folds are real `<button>`
  disclosures with `aria-expanded`.
- Contrast: AA everywhere (§1); never color-only status (chips labeled,
  dots paired with words).
- Reduced motion honored (§4); the curtain lift is skipped.
- The DOM surface remains the accessibility floor as Station grows —
  Station's hotspot-mirror pattern stays canonical.
- Touch: 44px targets, safe-area insets, the composer above the keyboard,
  no hover-only affordances (folds are taps, not reveals).

---

## 8. Token implementation note

The prototype implements these as `tokens.css` (custom properties under
`html[data-theme]` + `html[data-density]`), the same alias-layer trick
ui-v2 uses — so if the concept is adopted, the token file drops into a
`ui3-tokens` fragment beside `16-styles-v2-tokens.css` and the two systems
can coexist during migration.
