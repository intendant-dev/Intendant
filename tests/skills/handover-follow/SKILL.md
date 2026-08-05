---
name: handover-follow
description: Prove the drain follow watch in a real booted SPA — arming on own-drain, the composer-draft consent demotion (R-A3 over-cap copy BEFORE navigation), the grace toast, and the live two-daemon follow where a headed tab moves itself onto the successor with its state carried and lands with the honest completion line. NOT in CI — drives a real browser over CDP (probe leg) and a live co-homed drain the operator watches (follow leg).
---

# Drain follow watch — headed scenario

**NOT in CI.** The e2e suite pins the served contract
(`follow_contract_carries_the_scripted_tab_to_the_successor`) and the
unit pins freeze the fragment wiring (`spa_arms_the_follow_watch_on_own_drain`
and siblings in `src/bin/caller/handover/mod.rs`). This scenario is the
operator battery's browser half: the follow watch running inside a real
SPA — probe leg via `scripts/validate-dashboard.cjs` (headless CDP is
fine), follow leg with a real co-homed drain and a tab that visibly
moves (headed, eyes-on).

Preconditions: current worktree binaries (`cargo build --bin intendant
--bin intendant-runtime` — the validator rejects stale target binaries
and never rebuilds), scratch `INTENDANT_HOME` for every daemon in the
scenario (the follow watch reads real payloads; a real home would put
owner state behind the probes), and `jq` for the follow leg's approval
step.

## Probe leg — arming, consent, grace (tokenless posture)

One invocation, three probes. The tokenless validate-dashboard posture
cannot fetch the authed handover payload or the sibling token map, so
the probes feed synthetic bodies through `qa.handoverBanner.render`
(the established tokenless-QA door), read `qa.followState()`, and force
the decision point with `qa.__testFollowDecide()` (the
`__testNudgeDaemonBoot` precedent) — the token+probe ladder itself is
the follow leg's and the e2e leg's job. Each probe disarms via
`render({available:false})` so nothing leaks between probes and no
grace timer survives (a leaked timer is harmless anyway: an unresolved
target refuses navigation by guard).

```bash
QAHOME=$(mktemp -d /tmp/follow-qa-home.XXXXXX)
INTENDANT_HOME="$QAHOME" node scripts/validate-dashboard.cjs \
  --launch-dashboard --port 8933 --dashboard-binary ./target/debug/intendant \
  --wait-for-function "window.qa && window.qa.followState && window.qa.handoverBanner" \
  --probe-json "followArm=(() => { const body = { boot_id: 'boot-drain', draining: true, daemons: [ { boot_id: 'boot-drain', port: 8933, state: 'draining', live: true, version: { pkg: '0.0.0-qa', git_sha: 'aaaaaaaaaa', built_at: '2026-01-01T00:00:00Z' } }, { boot_id: 'boot-succ', port: 65500, state: 'active', live: true, version: { pkg: '0.0.0-qa', git_sha: 'bbbbbbbbbb', built_at: '2026-01-02T00:00:00Z' } } ], sidecar: { boot_id: 'boot-succ', port: 65500, state: 'active' }, holdouts: [] }; window.qa.handoverBanner.render(body); const s = window.qa.followState(); const out = { armed: s.armed === true, watching: s.phase === 'watching', direct: s.excluded === '', stampPair: !!(s.stamps && s.stamps.pred && s.stamps.pred.git_sha === 'aaaaaaaaaa' && s.stamps.pred.built_at === '2026-01-01T00:00:00Z'), predBoot: !!(s.stamps && s.stamps.pred_boot === 'boot-drain') }; window.qa.handoverBanner.render({ available: false }); out.disarmed = window.qa.followState().armed === false; return out; })()" \
  --probe-json "followConsent=(() => { const body = { boot_id: 'boot-drain', draining: true, daemons: [ { boot_id: 'boot-drain', port: 8933, state: 'draining', live: true, version: { pkg: '0.0.0-qa', git_sha: 'aaaaaaaaaa', built_at: '2026-01-01T00:00:00Z' } } ], sidecar: null, holdouts: [] }; const input = document.getElementById('new-session-input'); input.value = 'a draft the move must never clobber'; window.qa.handoverBanner.render(body); window.qa.__testFollowDecide(); const out = { consent: window.qa.followState().phase === 'consent' }; let section = document.querySelector('.handover-follow-consent'); out.button = !!(section && section.textContent.includes('Continue on the updated daemon')); out.carriesNamed = !!(section && section.textContent.includes('moves with you')); input.value = 'x'.repeat(20000); window.qa.handoverBanner.render(body); section = document.querySelector('.handover-follow-consent'); out.overCapNamedBeforeNavigation = !!(section && section.textContent.includes('too large to carry')); input.value = ''; window.qa.handoverBanner.render({ available: false }); out.disarmed = window.qa.followState().armed === false; return out; })()" \
  --probe-json "followGrace=(() => { const body = { boot_id: 'boot-drain', draining: true, daemons: [ { boot_id: 'boot-drain', port: 8933, state: 'draining', live: true, version: { pkg: '0.0.0-qa', git_sha: 'aaaaaaaaaa', built_at: '2026-01-01T00:00:00Z' } } ], sidecar: null, holdouts: [] }; window.qa.handoverBanner.render(body); window.qa.__testFollowDecide(); const s = window.qa.followState(); const toast = document.getElementById('handover-follow-toast'); const out = { grace: s.phase === 'grace', toast: !!(toast && toast.textContent.includes('Moving to the updated daemon')) }; window.qa.handoverBanner.render({ available: false }); out.cleaned = !document.getElementById('handover-follow-toast') && window.qa.followState().armed === false; return out; })()"
rm -rf "$QAHOME"
```

Expected: every probe field `true`. `followConsent` is the R-A3 bar —
the over-cap loss is named in the consent section BEFORE any
navigation, and the button label is the intake's verbatim "Continue on
the updated daemon". `followGrace` proves the visible-tab posture: the
decision lands in `grace` (never an immediate move) under the "Moving
to the updated daemon…" toast.

## Follow leg — the live two-daemon move (eyes-on)

The real thing: a headed tab on a draining daemon moves itself onto the
successor, authed, with its route carried — and, because both daemons
run the SAME binary, the completion line must be the honest
**"The restart didn't change builds."** (R-A1's pair-equality arm: the
classic false-"updated" narration is exactly what this leg proves
gone). Mock provider throughout — keyless, no API calls.

```bash
BIN=./target/debug/intendant
HOME_RIG=$(mktemp -d /tmp/follow-live-home.XXXXXX)
PROJ=$(mktemp -d /tmp/follow-live-proj.XXXXXX)
touch "$PROJ/intendant.toml"
cat > "$HOME_RIG/script.json" <<'EOF'
{ "profiles": [
  { "match": "handover rig follow-through",
    "steps": [
      { "content": "Working until released.", "wait_for_file": "BARRIER" },
      { "content": "All work finished.",
        "tool_calls": [{ "name": "signal_done", "arguments": { "message": "done" } }] } ] },
  { "steps": [ { "content": "fallback", "tool_calls": [{ "name": "signal_done", "arguments": { "message": "unexpected" } }] } ] }
] }
EOF
# The barrier placeholder must be the real path:
python3 - "$HOME_RIG" <<'EOF'
import json, sys, pathlib
home = pathlib.Path(sys.argv[1])
p = home / 'script.json'
s = json.loads(p.read_text())
s['profiles'][0]['steps'][0]['wait_for_file'] = str(home / 'barrier')
p.write_text(json.dumps(s))
EOF

# 1. Daemon A (the predecessor), from the project root:
cd "$PROJ" && INTENDANT_HOME="$HOME_RIG" PROVIDER=mock \
  INTENDANT_MOCK_SCRIPT="$HOME_RIG/script.json" INTENDANT_LEASE_POLL_MS=500 \
  "$OLDPWD/$BIN" --web 8934 --bind 127.0.0.1 --no-tls --no-tui --autonomy full & cd "$OLDPWD"

# 2. Open A's dashboard IN A HEADED BROWSER via its tokened URL:
sleep 3 && echo "open: http://127.0.0.1:8934/?token=$(cat "$HOME_RIG/.intendant/loopback-tokens/8934.token")"

# 3. Park a holdout on A so the drain has something to wait on
#    (add → schedule at +2s → approve with the exact digest):
CTL() { env -u INTENDANT_SESSION_ID INTENDANT_HOME="$HOME_RIG" "$BIN" ctl --url "http://127.0.0.1:8934" "$@"; }
ITEM=$(CTL --json agenda add "follow live park" --task | jq -r .item.id)
CTL agenda schedule "${ITEM:0:10}" --goal "handover rig follow-through" --at +2s
DIGEST=$(CTL --json agenda list --all | jq -r ".items[] | select(.id==\"$ITEM\") | .effects[0].digest")
CTL agenda approve "${ITEM:0:10}" --digest "$DIGEST"
sleep 5   # let the occurrence fire and park on the barrier

# 4. Daemon C (the successor), co-homed:
cd "$PROJ" && INTENDANT_HOME="$HOME_RIG" PROVIDER=mock \
  INTENDANT_MOCK_SCRIPT="$HOME_RIG/script.json" INTENDANT_LEASE_POLL_MS=500 \
  "$OLDPWD/$BIN" --web 8935 --bind 127.0.0.1 --no-tls --no-tui --autonomy full & cd "$OLDPWD"

# 5. Drain A:
curl -s -X POST "http://127.0.0.1:8934/api/daemon/takeover" \
  -H "x-intendant-loopback-token: $(cat "$HOME_RIG/.intendant/loopback-tokens/8934.token")" \
  -H 'Content-Type: application/json' -d '{"requested_by":"follow live leg"}'
```

Watch the tab (within one 30s handover poll, then a ~2.5s tick
cadence):

1. The drain banner appears with the grade-1 story ("Updating
   Intendant — your work continues."); the parked holdout's named row
   sits under the banner's **Advanced** fold (shared sticky key —
   opening it once keeps every handover surface's mechanics open).
2. The follow status/toast: "Moving to the updated daemon…" (about 5s,
   the visible-tab grace).
3. The tab **replaces itself onto `http://127.0.0.1:8935`**, lands
   authed (no named-401 wall — the token rode the URL and stripped
   itself), the route hash survives, and the completion toast reads
   **"The restart didn't change builds."** — same binary on both ends,
   said honestly.
4. `qa.followCarryApplied()` in the landed tab's console reports what
   the carry seeded (window records / sub-tabs / draft as exercised).
5. Draft variant (repeat with text typed in the New Session composer
   before step 5): no auto-move — the banner holds a one-button
   "Continue on the updated daemon"; clicking moves and the draft
   arrives in the successor's composer.

Cleanup:

```bash
touch "$HOME_RIG/barrier"   # release the parked session; A exits after it finishes
sleep 3; kill %1 %2 2>/dev/null; rm -rf "$HOME_RIG" "$PROJ"
```

Both daemons here are the same build, so this leg cannot show the
"All set — running <version>." arm; the e2e leg pins the stamp-pair
substrate and the unit pin freezes the compare expression — a live
different-build follow happens naturally on the dev fleet's next real
update and needs no rig.
