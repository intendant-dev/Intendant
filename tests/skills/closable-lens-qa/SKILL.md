# Closable-at-a-glance lens — QA harness leg

Validates the grid's closable lens end to end in a real booted SPA:
the positive-only classifier (`sessionWindowClosableClaim`,
`static/app/41-session-window-actions.js`), the count chip + dim toggle
(`static/app/ui2-activity.js`), and the auto-disengage-on-empty rule —
without a live daemon session behind any window. Everything drives
through `window.qa` (the SPA is one module scope; `qa.closableLens` and
`qa.sessionWindowSweeps` are the only doors).

Preconditions: none beyond a buildable checkout — the validator's
`--launch-dashboard` boots a throwaway daemon (its stale-binary guard
rebuilds when sources changed; expect a compile on first run) and owns
its lifecycle. Pick a throwaway port. No API keys, no display: the
probes never dispatch a session.

Two probes, one invocation:

- **matrix** — drives the pure classifier with the explicit conjunction
  matrix. Every negative served claim must veto (even hard-done),
  `settled` must be positive only quiet (idle or hard-done — never a
  running phase), a linked occurrence without a served stop claim must
  claim nothing, and unknown claims must stay not-closable.
- **lens** — builds four throwaway windows via
  `qa.sessionWindowSweeps.build` (each built TWICE: the second call
  routes the metadata through `updateSessionWindow`, which stores the
  agenda envelope + phase the classifier reads — the first build only
  renders), then walks the chip/class/attribute lifecycle and removes
  them again.

```bash
node scripts/validate-dashboard.cjs --launch-dashboard --port 8931 \
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
  }; })()" \
  --probe-json "lens=(() => { const sw = window.qa.sessionWindowSweeps, L = window.qa.closableLens;
    const mk = (sid, meta) => { sw.build(sid, meta); sw.build(sid, meta); };
    mk('qa-lens-idle', {phase:'idle'});
    mk('qa-lens-run', {phase:'running'});
    mk('qa-lens-settled', {phase:'idle', agenda:{item_id:'01QALENSITEM', occurrence:{id:'01QAOCC1', state:'completed', stop:'settled'}}});
    mk('qa-lens-owed', {phase:'idle', agenda:{item_id:'01QALENSITEM2', occurrence:{id:'01QAOCC2', state:'started', stop:'owed_work'}}});
    const before = L.state();
    const on = L.set(true); const engaged = L.state();
    L.set(false);
    for (const sid of ['qa-lens-idle','qa-lens-run','qa-lens-settled','qa-lens-owed']) sw.remove(sid);
    const after = L.state();
    return { before, on, engaged, after }; })()" \
  --screenshot /tmp/closable-lens-qa.png
```

Pass bars (read the two `probe … = {…}` stdout lines):

- `matrix`: every key `true`.
- `lens.before`: `count: 2`, `chipHidden: false`, `closable` and
  `closableClassed` both exactly `["qa-lens-idle","qa-lens-settled"]`
  (order per insertion), `on: false`, `lensAttr: ""`.
- `lens.on`: `true`; `lens.engaged`: `lensAttr: "on"`, same two ids
  closable-classed; `qa-lens-run`/`qa-lens-owed` never appear in either
  list (running suppressed; the owed_work veto outranks the idle look).
- `lens.after` (all windows removed): `count: 0`, `chipHidden: true`,
  and — the strand-guard — `on: false`, `lensAttr: ""` even though the
  lens was last driven while engaged in an earlier step? No: the lens
  was explicitly set off above; re-run with the `L.set(false)` line
  removed to exercise the auto-disengage (`after.on` must STILL be
  `false` — the refresh disengages an engaged lens the moment the count
  empties; both variants are green as of the lens landing).

The `--screenshot` lands after the probes: with the scenario windows
removed the grid is empty again — the image documents the booted SPA,
not the lens. For a visual look, re-run with the removal loop deleted
and `--keep-browser`, then inspect the held page (two full-presence
cards, two dimmed at 0.35 opacity, chip pressed with count 2).
