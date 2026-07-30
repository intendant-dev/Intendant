---
name: fireability-qa
description: >
  QA-harness leg for propose-time fireability (card 01KYSZAGQVHAAYS7BK9H3QFM3C):
  against a scratch daemon seeded with (a) an APPROVED one-shot whose floor
  passed more than a staleness window ago — the missed-window terminal —
  and (b) a pending manifest pinned to a directory that no longer exists,
  the dashboard must render the missed card state with its one-tap
  Re-approve-to-reschedule affordance (tapping it re-proposes the plan at
  now and re-approves, leaving the effect armed), and must render the
  pending card WITHOUT an Approve button — Fix plan… instead, opening the
  schedule sheet focused on the named field; `ctl agenda approve` on the
  same manifest is refused with the `unfireable(project): …` grammar.
  Keyless for the render legs; the post-reschedule spawn uses the mock
  provider. Drives the real SPA over CDP via scripts/validate-dashboard.cjs.
compatibility: Operator hardware or any box with Chromium — never CI as-is
  (spawns a daemon and a browser). No display capture involved.
allowed-tools: Bash Read
---

# Propose-time fireability — QA harness leg

What ships under test: the ONE validator (`agenda/fireability.rs`) run in
the propose/approve intake, the served `fireability_refusal` decoration,
the missed-window card state with the one-tap reschedule
(`agendaRescheduleMissed`), and the approve-refusal edit prompt. The SPA
exposes the readback as `window.qa.agendaFireability()`.

## 1. Seed the agenda log BEFORE the daemon boots

The missed terminal needs a floor that passed while no daemon was up —
seed the op log directly (the fold loads history untouched; approval
digests bind `sha256("agenda-effect\0item\0effect\0" + manifest JSON)`,
first 16 bytes hex):

```bash
cargo build --bin intendant --bin intendant-runtime
export QA_HOME=$(mktemp -d) QA_PORT=18973
mkdir -p "$QA_HOME/agenda"
python3 - "$QA_HOME/agenda/agenda.jsonl" <<'EOF'
import hashlib, json, sys, time
now = int(time.time() * 1000)
stale = now - 26 * 3600_000          # 26h ago — past the 12h staleness default
mf_m = {"goal": "qa: missed window", "fire_at_ms": stale}
digest = hashlib.sha256(
    b"agenda-effect\0it-missed\0ef-qa-missed00\0"
    + json.dumps(mf_m, separators=(",", ":")).encode()
).hexdigest()[:32]
mf_u = {"goal": "qa: unfireable plan", "fire_at_ms": now + 3600_000,
        "project_root": "/nonexistent-fireability-qa"}
ops = [
    {"v": 1, "at_ms": stale - 2, "op": {"type": "add", "id": "it-missed",
     "kind": "task", "title": "qa missed window", "body": "", "tags": []}},
    {"v": 1, "at_ms": stale - 1, "op": {"type": "propose_effect", "id": "it-missed",
     "effect_id": "ef-qa-missed00", "manifest": mf_m}},
    {"v": 1, "at_ms": stale, "op": {"type": "approve_effect", "id": "it-missed",
     "effect_id": "ef-qa-missed00", "digest": digest}},
    {"v": 1, "at_ms": now - 2, "op": {"type": "add", "id": "it-unfire",
     "kind": "task", "title": "qa unfireable", "body": "", "tags": []}},
    {"v": 1, "at_ms": now - 1, "op": {"type": "propose_effect", "id": "it-unfire",
     "effect_id": "ef-qa-unfire00", "manifest": mf_u}},
]
with open(sys.argv[1], "w") as f:
    f.writelines(json.dumps(op) + "\n" for op in ops)
EOF
```

(`it-unfire`'s pin names a directory that does not exist, so the served
verdict is `unfireable(project)` even though the scratch daemon itself
has a default project — the repo checkout it launches from.)

## 2. Launch the scratch dashboard over the seeded home

```bash
cat > "$QA_HOME/mock.json" <<'EOF'
{ "model": "mock-1", "profiles": [{ "match": "", "steps": [
  { "content": "Done.", "tool_calls": [{ "name": "signal_done",
    "arguments": { "message": "qa run complete" } }] }
] }] }
EOF
INTENDANT_HOME="$QA_HOME" PROVIDER=mock INTENDANT_MOCK_SCRIPT="$QA_HOME/mock.json" \
  INTENDANT_MOCK_DISPLAY=synthetic node scripts/validate-dashboard.cjs \
  --launch-dashboard --hold-dashboard --port "$QA_PORT" \
  --dashboard-binary target/debug/intendant --selector body &
HOLD_PID=$!
export INTENDANT_LOOPBACK_TOKEN=$(cat "$QA_HOME"/loopback-tokens/$QA_PORT.token)
```

Give the first scheduler pass a few seconds: it resolves the seeded
approved instant as `missed` (journal + write-back) — verify with
`env -u INTENDANT_MCP_URL -u INTENDANT_SESSION_ID ./target/debug/intendant ctl \
  --url "http://127.0.0.1:$QA_PORT/mcp" agenda show it-missed --json | grep '"state": "missed"'`.

## 3. The card states (the acceptance's QA leg)

One probe asserts both cards: the missed state with its reschedule
affordance and no Approve; the unfireable pending with the served
refusal, Fix plan…, and no Approve (the class law's render half):

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 30000 \
  --wait-for-function "(() => { const qa = window.qa && window.qa.agendaFireability
      && window.qa.agendaFireability({ route: true });
    if (!qa) return false;
    const m = qa.effects.find((e) => e.id === 'it-missed');
    const u = qa.effects.find((e) => e.id === 'it-unfire');
    if (!m || m.kind !== 'missed' || !m.hasReschedule || m.hasApprove) return false;
    return !!u && u.kind === 'pending' && !!u.refusal
      && u.refusal.field === 'project' && !u.hasApprove && u.hasFixPlan;
  })()" \
  --probe-json "fireability=window.qa.agendaFireability()"
```

## 4. The approve refusal is an edit prompt, never a silent failure

CLI half — the daemon's named refusal with the pinned grammar:

```bash
DIGEST=$(env -u INTENDANT_MCP_URL -u INTENDANT_SESSION_ID ./target/debug/intendant ctl \
  --url "http://127.0.0.1:$QA_PORT/mcp" agenda show it-unfire --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["item"]["effects"][0]["digest"])')
env -u INTENDANT_MCP_URL -u INTENDANT_SESSION_ID ./target/debug/intendant ctl \
  --url "http://127.0.0.1:$QA_PORT/mcp" agenda approve it-unfire --digest "$DIGEST" 2>&1 \
  | grep 'unfireable(project): '   # MUST refuse, named
```

SPA half — Fix plan… opens the one schedule sheet focused on the named
field with the refusal shown inline:

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 30000 \
  --wait-for-function "(() => {
    const btn = document.querySelector('[data-edit-sched=\"it-unfire\"][data-focus=\"project\"]');
    if (!btn) return false;
    if (!(window.qa.agendaFireability().sheet || {}).itemId) btn.click();
    const s = window.qa.agendaFireability().sheet;
    if (!s || s.itemId !== 'it-unfire') return false;
    if (s.kind !== 'sched') return true && false; // loading — keep polling
    return s.focusField === 'project' && s.error.indexOf('unfireable(project)') === 0;
  })()"
```

## 5. The one-tap re-approve-to-reschedule

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 45000 \
  --wait-for-function "(() => { const qa = window.qa && window.qa.agendaFireability
      && window.qa.agendaFireability({ route: true });
    if (!qa) return false;
    const m = qa.effects.find((e) => e.id === 'it-missed');
    if (!m) return false;
    if (m.kind === 'missed') { window.qa.agendaFireability({ reschedule: 'it-missed' }); return false; }
    return m.kind !== 'missed' && !m.hasReschedule;
  })()" \
  --probe-json "after=window.qa.agendaFireability()"
```

Then confirm daemon-side that the tap minted a FRESH approved revision:
`… agenda show it-missed --json` shows `effects[0].approval` bound to a
digest ≠ the seeded one and `last_run` no longer `missed` (cleared by the
re-propose; under the mock provider the rescheduled run then starts and
completes).

## 6. Teardown

`kill -INT "$HOLD_PID"` (the helper stops the daemon it launched), then
delete `$QA_HOME`.

## Acceptance

Steps 3–5's harness invocations exit 0 and the ctl approve in step 4
prints the `unfireable(project): …` refusal. The step-3 probe JSON shows
`it-missed {kind:'missed', hasReschedule:true, hasApprove:false}` and
`it-unfire {kind:'pending', refusal.field:'project', hasApprove:false,
hasFixPlan:true}`; after step 5 the `it-missed` effect is armed under a
new digest.
