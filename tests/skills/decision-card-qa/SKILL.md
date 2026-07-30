---
name: decision-card-qa
description: >
  QA-harness leg for the Agenda decision-card UX: against a scratch daemon
  with one seeded structured question (options in the ask-rail vocabulary,
  a prose Recommendation sentence, one must-read file ref), the dashboard
  inspector must render the choices as pills with the "(Recommended)"
  highlight, surface the recommendation strip with its one-click answer
  prefill, and open the file ref in the in-dashboard reader — live bytes
  with the drift verdict first, sealed-snapshot precedence after the same
  bytes are pinned by a binding ref, drift honesty after the live file is
  mutated. Keyless (agenda ops need no provider); drives the real SPA over
  CDP via scripts/validate-dashboard.cjs.
compatibility: Operator hardware or any box with Chromium — never CI as-is
  (spawns a daemon and a browser). Keyless; no display capture involved.
allowed-tools: Bash Read
---

# Decision-card UX — QA harness leg

What ships under test: the ref-scoped reader route
(`GET /api/agenda/items/{item_id}/refs/content`, tunnel twin
`api_agenda_ref_content`), the structured `agenda ask --option` park lane,
and the inspector rendering (pills + `(Recommended)` highlight +
recommendation strip + openable refs). The SPA exposes the readback as
`window.qa.agendaDecisionCard()`.

## 1. Launch a scratch dashboard (own home — never the real agenda)

```bash
cargo build --bin intendant --bin intendant-runtime
export QA_HOME=$(mktemp -d) QA_PORT=18971
INTENDANT_HOME="$QA_HOME" node scripts/validate-dashboard.cjs \
  --launch-dashboard --hold-dashboard --port "$QA_PORT" \
  --dashboard-binary target/debug/intendant --selector body &
HOLD_PID=$!   # interrupt (SIGINT) when done — helper-owned cleanup
```

## 2. Seed the decision card through the real ctl lane

```bash
BRIEF=$(mktemp -d)/findings.md
printf 'ORIENTATION: two calls tonight. Recommendation: 14 days, fixed constant. Disposition: answer with one word.\n' > "$BRIEF"
env -u INTENDANT_MCP_URL -u INTENDANT_SESSION_ID ./target/debug/intendant ctl \
  --url "http://127.0.0.1:$QA_PORT/mcp" agenda ask "Live-window width?" \
  --option "14 days (Recommended):fixed daemon-side constant" --option "30 days" \
  --header Window --tag qa-decision \
  --body "Context wall. Recommendation: 14 days, fixed daemon-side constant. More prose." \
  --ref "$BRIEF" --must-read
# note the printed item id:
ITEM=<id from the park output>
```

Verify the park landed structured: `… agenda list --json` shows the item
with `ask.questions[0].options` (two labels) and one file ref with a
digest.

## 3. Drive the SPA — live serving + rendering assertions

One idempotent `--wait-for-function` driver walks route → inspector →
reader and returns true only when every rendering assertion holds:

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 30000 \
  --wait-for-function "(() => { const id='$ITEM', loc='$BRIEF';
    if (typeof routeTo==='function') routeTo('agenda');
    if (typeof agendaFindItem!=='function' || !agendaFindItem(id)) return false;
    if (typeof agendaSelId==='undefined' || agendaSelId!==id) { agendaOpenInspector(id); return false; }
    const qa = window.qa && window.qa.agendaDecisionCard && window.qa.agendaDecisionCard();
    if (!qa) return false;
    if (!(qa.optionLabels.flat().includes('14 days (Recommended)') && qa.recommendedPills>=1)) return false;
    if (!(qa.recommendations.length>=1 && qa.recommendations[0].indexOf('14 days')===0)) return false;
    if (!(qa.openableFileRefs>=1)) return false;
    if (!qa.refReader) { agendaOpenRefReader(id, loc); return false; }
    if (qa.refReader.loading) return false;
    return qa.refReader.source==='live' && qa.refReader.drift==='unchanged' && !qa.refReader.error;
  })()" \
  --probe-json "decision=window.qa.agendaDecisionCard()"
```

## 4. Sealed precedence + drift honesty

Pin the same bytes via a binding ref (propose seals them), then mutate the
live file; the reader must switch to the sealed snapshot and report the
live drift:

```bash
env -u INTENDANT_MCP_URL -u INTENDANT_SESSION_ID ./target/debug/intendant ctl \
  --url "http://127.0.0.1:$QA_PORT/mcp" agenda schedule "$ITEM" \
  --goal "qa: sealing carrier" --at +6h --binding-ref "file:$BRIEF"
printf 'amended after sealing\n' >> "$BRIEF"
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 30000 \
  --wait-for-function "(() => { const id='$ITEM', loc='$BRIEF';
    if (typeof routeTo==='function') routeTo('agenda');
    if (typeof agendaFindItem!=='function' || !agendaFindItem(id)) return false;
    if (typeof agendaSelId==='undefined' || agendaSelId!==id) { agendaOpenInspector(id); return false; }
    const qa = window.qa && window.qa.agendaDecisionCard && window.qa.agendaDecisionCard();
    if (!qa) return false;
    if (!qa.refReader || qa.refReader.error) { agendaOpenRefReader(id, loc); return false; }
    if (qa.refReader.loading) return false;
    return qa.refReader.source==='sealed' && qa.refReader.drift==='changed';
  })()" \
  --probe-json "reader=window.qa.agendaDecisionCard().refReader"
```

(The second driver re-opens the reader each poll until the fresh fetch
lands; `source:'sealed'` proves snapshot precedence, `drift:'changed'`
proves the live probe stays honest.)

## 5. One-click prefill (behavioral spot check)

```bash
node scripts/validate-dashboard.cjs --url "http://127.0.0.1:$QA_PORT" --timeout 15000 \
  --wait-for-function "(() => {
    const btn = document.querySelector('#ag2-inspector [data-rec-use]');
    if (!btn) return false; btn.click();
    const qa = window.qa.agendaDecisionCard();
    return qa && qa.answerDraft.indexOf('14 days') === 0;
  })()"
```

## 6. Teardown

`kill -INT "$HOLD_PID"` (the helper stops the daemon it launched), then
delete `$QA_HOME`.

## Acceptance

All three harness invocations exit 0. The probe JSON from step 3 shows
`optionLabels` carrying both labels, `recommendedPills ≥ 1`,
`recommendations[0]` starting with the recommended value, and
`refReader {source:'live', drift:'unchanged'}`; step 4's shows
`{source:'sealed', drift:'changed'}`.
