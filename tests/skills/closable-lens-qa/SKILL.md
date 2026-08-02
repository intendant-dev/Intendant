# Closable-at-a-glance lens — QA harness leg

Validates the grid's closable lens end to end in a real booted SPA:
the positive-only classifier (`sessionWindowClosableClaim`,
`static/app/41-session-window-actions.js`), the count chip + dim toggle
(`static/app/ui2-activity.js`), and the auto-disengage-on-empty rule —
without a live daemon session behind any window. Everything drives
through `window.qa` (the SPA is one module scope; `qa.closableLens` and
`qa.sessionWindowSweeps` are the only doors).

Preconditions: current worktree binaries (`cargo build --bin intendant
--bin intendant-runtime` — the validator REJECTS stale target binaries
and never rebuilds). Two isolation rules make the readbacks
deterministic and safe:

- **Scratch `INTENDANT_HOME`** — the launched daemon inherits the
  environment; without this it runs against the real home and the real
  session catalog populates the grid, breaking the exact-count and
  exact-id bars below (and pointing probes at owner state).
- **Explicit `--dashboard-binary`** — under Intendant supervision
  `$INTENDANT` names the *user's* installed binary; auto-discovery would
  validate the wrong tree. Point at this worktree's build.

No API keys, no display: the probes never dispatch a session, and the
throwaway windows are pure SPA state (map + DOM), never daemon
mutations.

Two probes, one invocation:

- **matrix** — drives the pure classifier with the explicit conjunction
  matrix. Every negative served claim must veto (even hard-done),
  `settled` must be positive only quiet (idle or hard-done — never a
  running phase), a linked occurrence without a served stop claim must
  claim nothing, unknown claims must stay not-closable, and an absent
  phase must claim nothing (`normalizeSessionPhase` defaults `''` to
  `'idle'`; the classifier must not inherit that widening).
- **lens** — builds four throwaway windows via
  `qa.sessionWindowSweeps.build` (each built TWICE: the second call
  routes the metadata through `updateSessionWindow`, which stores the
  agenda envelope + phase the classifier reads — the first build only
  renders), walks the chip/class/attribute lifecycle, then removes all
  four **while the lens is engaged** to exercise the strand-guard.

```bash
QAHOME=$(mktemp -d /tmp/closable-lens-home.XXXXXX)
INTENDANT_HOME="$QAHOME" node scripts/validate-dashboard.cjs \
  --launch-dashboard --port 8931 --dashboard-binary ./target/debug/intendant \
  --wait-for-function "window.qa && window.qa.closableLens && window.qa.sessionWindowSweeps" \
  --probe-json "matrix=(() => { const c = window.qa.closableLens.claim; return {
    killsVeto: c({stop:'kills_live_run', hardDone:true}) === false,
    owedVeto: c({stop:'owed_work', hardDone:true}) === false,
    settledRunningSuppressed: c({stop:'settled', phase:'running'}) === false,
    settledIdle: c({stop:'settled', phase:'idle'}) === true,
    settledHardDone: c({stop:'settled', hardDone:true}) === true,
    linkedNoClaim: c({linked:true, phase:'idle'}) === false,
    idleLinkless: c({phase:'idle'}) === true,
    waitingSuppressed: c({phase:'waiting'}) === false,
    thinkingSuppressed: c({phase:'thinking'}) === false,
    hardDoneLinkless: c({hardDone:true}) === true,
    unknownClaim: c({stop:'someday_new_claim'}) === false,
    phaselessClaimsNothing: c({}) === false && c({phase:''}) === false,
  }; })()" \
  --probe-json "lens=(() => { const sw = window.qa.sessionWindowSweeps, L = window.qa.closableLens;
    const mk = (sid, meta) => { sw.build(sid, meta); sw.build(sid, meta); };
    mk('qa-lens-idle', {phase:'idle'});
    mk('qa-lens-run', {phase:'running'});
    mk('qa-lens-settled', {phase:'idle', agenda:{item_id:'01QALENSITEM', occurrence:{id:'01QAOCC1', state:'completed', stop:'settled'}}});
    mk('qa-lens-owed', {phase:'idle', agenda:{item_id:'01QALENSITEM2', occurrence:{id:'01QAOCC2', state:'started', stop:'owed_work'}}});
    const before = L.state();
    const on = L.set(true); const engaged = L.state();
    const removedWhileOn = (() => {
      for (const sid of ['qa-lens-idle','qa-lens-run','qa-lens-settled','qa-lens-owed']) sw.remove(sid);
      return L.state(); })();
    return { before, on, engaged, removedWhileOn }; })()" \
  --screenshot /tmp/closable-lens-qa.png
rm -rf "$QAHOME"
```

Pass bars (read the two `probe … = {…}` stdout lines):

- `matrix`: every key `true`.
- `lens.before`: `count: 2`, `chipHidden: false`, `closable` and
  `closableClassed` both exactly `["qa-lens-idle","qa-lens-settled"]`
  (insertion order), `on: false`, `lensAttr: ""` — `qa-lens-run`
  (running suppressed) and `qa-lens-owed` (the owed_work veto outranks
  the idle look) never appear in either list.
- `lens.on`: `true`; `lens.engaged`: `lensAttr: "on"`, same two ids in
  both lists.
- `lens.removedWhileOn` — the strand-guard: with every window removed
  while the lens was engaged, `count: 0`, `chipHidden: true`, and
  `on: false` / `lensAttr: ""` — the refresh disengages an engaged lens
  the moment the count empties, so a fully-dimmed grid can never sit
  behind a hidden toggle.

Recorded green 2026-07-30 on the lens landing (PR #678): both probes
exactly as above, `PASS dashboard-validation … functions=1`, ~20 s
against a debug binary.

The `--screenshot` lands after the probes: with the scenario windows
removed the grid is empty again — the image documents the booted SPA,
not the lens. For a visual look, re-run with the removal step deleted
and `--keep-browser`, then inspect the held page (two full-presence
cards with the green ring, two dimmed at 0.35 opacity, chip pressed
with count 2).

## Legibility leg (the lens-direction copy, 2026-08-01)

The lens-legibility landing added on-canvas direction statements and the
phase-guarded × title (the 2026-07-31 specimen: a returning viewer read
the dim as "closable" — the exact inversion). New behaviors, all
display-only over the same claims:

- engaged, the chip's own text flips to `N safe to close · rest dimmed`
  and the grid legend (`#ui2-closable-lens-legend`, keyed on the same
  html attribute as the dim) states the direction with a `Show all`
  disengage; `state()` now also reports `chipWord` / `chipTitle` /
  `legendShown`;
- the lens auto-disengages on navigation away from the Timeline grid
  (main tab, sub-tab, or Focus-layout flip) — a stale engaged lens can
  no longer greet a re-entry unexplained;
- the agenda-settled × title is phase-guarded: on a non-quiet window it
  leads with the live pill label ("Running Agent — stopping interrupts
  it; no agenda-owed work remains (the linked occurrence is settled)"),
  and it re-derives on every phase application (the phase-only fast path
  skips the wide render).

One self-checking probe covers the whole copy matrix —
`qa.closableLens.vectors()` pins tooltip rows (the × title claim matrix:
veto/settled×quiet/settled×live/linkless) and the chip + legend copy;
add it to the invocation above as a third `--probe-json`:

```bash
  --probe-json "copy=(() => { const v = window.qa.closableLens.vectors();
    return { rows: v.length, pass: v.every(r => r.pass),
             failed: v.filter(r => !r.pass).map(r => r.name) }; })()"
```

Pass bar: `pass: true`, `failed: []` (any named row = the copy and its
pinned vector drifted apart — fix whichever side is wrong). The engaged
lens leg above additionally reports `chipWord: "safe to close · rest
dimmed"` and `legendShown: true` inside `lens.engaged`, and
`legendShown: false` once disengaged.
