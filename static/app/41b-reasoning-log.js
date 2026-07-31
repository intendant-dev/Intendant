// ── Reasoning ("Thinking") rows ────────────────────────────────────────
// First-class Activity rows for the agent's reasoning summaries. One
// grammar across all four backends (Claude Code/Kimi thinking blocks, Codex
// reasoning items, native provider reasoning summaries): the daemon emits
// them as log rows with level "model" + kind "reasoning" and the RAW
// reasoning text as content — no prefix, no markup. This fragment owns
// their rendering everywhere a log row can appear:
//
//   - live Activity feed        renderLogEntry → renderReasoningLogEntry
//   - session windows           buildSessionWindowLogEntry → buildReasoningLogEntryNode
//   - Sessions detail view      materializeSessionDetailRow → buildReasoningLogEntryNode
//
// The row is calm by default: a dimmed one-line summary under a plain
// "Thinking" label, expanding on tap (whole row is the tap target — no
// hover-only affordance) to the full reasoning text. The full text is
// NOT stuffed into the DOM eagerly — it lives in a WeakMap store and the
// body renders on first expand (same idea as the deferred command-output
// pattern in 41-session-window-actions.js). If a backend emits no
// reasoning, no row renders — honest absence, never a placeholder.

function isReasoningLog(c) {
  return String(c?.kind || '') === 'reasoning';
}

// One-line summary: the first non-blank line, cut at a sentence boundary
// when one lands inside the cap, hard-elided otherwise. CSS ellipsis
// handles narrower viewports; this cap only bounds what enters the DOM.
const REASONING_SUMMARY_CHAR_LIMIT = 200;
function reasoningLogSummaryText(text) {
  const firstLine = String(text || '')
    .split('\n')
    .map(line => line.trim())
    .find(line => line.length > 0) || '';
  if (firstLine.length <= REASONING_SUMMARY_CHAR_LIMIT) return firstLine;
  const head = firstLine.slice(0, REASONING_SUMMARY_CHAR_LIMIT);
  const sentenceEnd = head.lastIndexOf('. ');
  if (sentenceEnd > 40) return head.slice(0, sentenceEnd + 1);
  return head.replace(/\s+\S*$/, '') + '\u2026';
}

// Full reasoning text per rendered entry, for the lazy body and for the
// dedupe signature bridge below. Values: { text, body, rendered }.
const _reasoningLogStore = new WeakMap();

// Signature bridge: node-lane transcript signatures read the rendered
// .log-content textContent, but a reasoning row renders a label + elided
// summary (and defers the body), so the store supplies the raw text the
// record lane hashes. Consumed by sessionWindowTranscriptSignaturesForNode.
function reasoningLogNodeContent(node) {
  const state = node ? _reasoningLogStore.get(node) : null;
  if (state && state.text) return state.text;
  return node?.querySelector?.('.reasoning-log-text')?.textContent || '';
}

function renderReasoningLogBody(entry) {
  const state = entry ? _reasoningLogStore.get(entry) : null;
  if (!state || state.rendered || !state.body) return;
  state.rendered = true;
  // Plain pre-wrap text on purpose: reasoning is scratch prose, and the
  // calm read matters more than markdown fidelity. One text node keeps
  // even very long traces cheap.
  state.body.textContent = state.text;
}

// Build the DOM node for a reasoning row. `c` is any log-command/record
// shape that carries {level, source, kind: 'reasoning', content, ...}.
function buildReasoningLogEntryNode(c) {
  const text = String(c?.content || '').trim();
  if (!text) return null;
  const summaryText = reasoningLogSummaryText(text);
  const expandable = text !== summaryText;

  const { entry } = createLogScaffold(c, 'reasoning-log-entry');

  const wrap = document.createElement('span');
  wrap.className = 'log-content reasoning-log-wrap';
  const summary = document.createElement('span');
  summary.className = 'reasoning-log-summary';
  const label = document.createElement('span');
  label.className = 'reasoning-log-label';
  label.textContent = 'Thinking';
  label.title = "The agent's reasoning before it acted";
  const summaryTextEl = document.createElement('span');
  summaryTextEl.className = 'reasoning-log-text';
  summaryTextEl.textContent = summaryText;
  summary.appendChild(label);
  summary.appendChild(summaryTextEl);
  wrap.appendChild(summary);

  const body = document.createElement('span');
  body.className = 'reasoning-log-body';
  wrap.appendChild(body);
  entry.appendChild(wrap);
  _reasoningLogStore.set(entry, { text, body, rendered: false });

  appendCopyLogEntryButton(entry, text);

  if (expandable) {
    entry.classList.add('expandable');
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 more</span><span class="arrow-up">\u25BE less</span>';
    entry.appendChild(toggle);
    entry.addEventListener('click', (event) => {
      if (event.target?.closest?.('a, button')) return;
      const expanded = !entry.classList.contains('expanded');
      entry.classList.toggle('expanded', expanded);
      if (expanded) renderReasoningLogBody(entry);
    });
  }

  return entry;
}

// Session-window clones of a live entry are plain cloneNode copies: no
// listeners and no store entry. Re-arm both from the source entry (or,
// for a clone whose source is gone, from whatever body text the clone
// carried over). Dispatched from wireSessionWindowLogClone.
const _wiredReasoningLogEntries = new WeakSet();
function wireReasoningLogClone(clone, sourceEntry) {
  if (!clone || _wiredReasoningLogEntries.has(clone)) return;
  _wiredReasoningLogEntries.add(clone);
  const body = clone.querySelector?.('.reasoning-log-body');
  const sourceState = sourceEntry ? _reasoningLogStore.get(sourceEntry) : null;
  // Text priority: the source's store (full text), then a body the clone
  // carried over already rendered (also full), then the elided summary —
  // the last resort keeps the row usable, never broken.
  const text = sourceState?.text || body?.textContent || reasoningLogNodeContent(clone);
  if (body && text) {
    _reasoningLogStore.set(clone, {
      text,
      body,
      // A clone taken after the source expanded carries the rendered body
      // markup with it; don't re-render over it.
      rendered: !!body.textContent,
    });
  }
  if (!clone.classList.contains('expandable')) return;
  clone.addEventListener('click', (event) => {
    if (event.target?.closest?.('a, button')) return;
    const expanded = !clone.classList.contains('expanded');
    clone.classList.toggle('expanded', expanded);
    if (expanded) renderReasoningLogBody(clone);
  });
}

// Live-feed path (dispatched from renderLogEntry): append to the main
// stream and mirror into the owning session window, exactly like the
// other special row types. The injected-signature check guards the
// append: when the transcript-sync lane already forwarded this row into
// the feed, its delayed live/replayed copy must not double it. The match
// is CONSUMED — an identical text the model genuinely thinks again later
// renders like it always did; only the injected row's own copy skips.
function renderReasoningLogEntry(c) {
  finalizeSessionCommandOutputGroups(c);
  inferSessionPhaseFromLog(c);
  const record = sessionWindowRecordFromLogCommand(c);
  const signatures = liveFeedReasoningSignatures(record);
  if (signatures.some(signature => _injectedReasoningSignatures.has(signature))) {
    for (const signature of signatures) {
      _injectedReasoningSignatures.delete(signature);
    }
    return;
  }
  const entry = buildReasoningLogEntryNode(c);
  if (!entry) return;
  for (const signature of signatures) {
    _liveFeedReasoningSignatures.add(signature);
  }
  appendLogEntryElement(entry, record);
}

// ── Transcript → live-feed reasoning parity ────────────────────────────
// The live lane's only source is the wrapper's normalized event stream —
// and print-mode Claude Code (sdk-cli entrypoint, ≥ 2.1.217) withholds
// summarized-thinking TEXT everywhere that stream can see: the streamed
// thinking_deltas, the completed envelope block, and the native
// transcript all carry empty signature-only shells (wire capture
// 2026-07-30, CC 2.1.220 — {"thinking":"","estimated_tokens":N}). For
// spawned print-mode seats there is honestly nothing to render, and
// nothing is fabricated. But transcript-materialized thinking DOES exist
// for cli-entrypoint stretches (interactive-born sessions the daemon
// adopted or attached), and the session-window transcript sync already
// fetches those rows on a live cadence — into the window only. This lane
// forwards the FRESH ones into the main Activity feed too, so live
// viewers see the same rows the Sessions detail and rehydrated cards
// render: derived from the one transcript source (derive, don't mirror),
// same grammar (level model + kind reasoning), same renderer.
//
// Three structures, two lifetimes:
//   - _liveFeedReasoningSignatures is the UNION registry: every reasoning
//     row the MAIN feed currently shows, from either lane. The injector
//     checks it so a row the live lane already rendered (or a prior sync
//     already forwarded) is never forwarded again.
//   - _injectedReasoningSignatures holds the injected rows only. The
//     live-lane renderer checks (and consumes from) THIS set, so the one
//     delayed copy of an injected row skips while a genuinely repeated
//     identical thought still renders.
//   Both reset with the feed (clearLogs — a reconnect replay rebuilds the
//   feed from the session log, which never persisted injected rows).
//   - the per-(source, session) watermark makes the FIRST sync pass a
//     catch-up (history belongs to the card/window; the feed starts at
//     "now") and every later pass live. Page-lifetime: it tracks the
//     sync lane's coverage, not the feed's contents.
const _liveFeedReasoningSignatures = new Set();
const _injectedReasoningSignatures = new Set();
const _transcriptReasoningSyncWatermarks = new Map();

// Signatures under the window/session canonical id, so the live lane
// (backend or daemon sid on the row) and the transcript lane (window
// sid) hash identically. Empty when the record can't be signed — such
// rows render unguarded, exactly like before this lane existed.
function liveFeedReasoningSignatures(record) {
  if (!record || !record.content) return [];
  const sid = String(record.session_id || record.sessionId || '').trim();
  const targetSid = (sid && sessionWindowTargetForLogSession(sid)) || sid;
  if (!targetSid) return [];
  return sessionWindowTranscriptSignaturesForRecord(
    { ...record, session_id: targetSid },
    targetSid
  );
}

// The main feed was cleared (reconnect replay, explicit clear): its
// registries empty with it. Watermarks survive — the transcript's
// already-covered span is unchanged by what the feed shows.
function resetLiveFeedReasoningDedupe() {
  _liveFeedReasoningSignatures.clear();
  _injectedReasoningSignatures.clear();
}

// Forward fresh transcript-materialized reasoning rows into the main
// Activity feed. Called from syncExternalSessionWindowTranscript with
// the fetched detail entries BEFORE the window merge, so the mirrored
// copy is already in the window history and the merge's signature scan
// collapses the pair. Deliberately NOT renderReasoningLogEntry: these
// rows are late-materialized, so the live side-effects (phase inference,
// output-group finalization) must not fire from them — a post-round
// injection would flip a settled card back to "thinking", and a mid-turn
// one could clip a still-streaming output group. Returns the number of
// rows appended.
function injectTranscriptReasoningIntoLiveFeed(entries, targetSid, source) {
  const sid = String(targetSid || '').trim();
  if (!sid || !Array.isArray(entries)) return 0;
  const key = `${String(source || '').trim().toLowerCase()}\u001f${sid}`;
  let maxTs = null;
  for (const entry of entries) {
    const ts = sessionWindowTranscriptTimestampMs(
      entry?.ts_ms ?? entry?.tsMs ?? entry?.ts ?? entry?.timestamp ?? ''
    );
    if (ts !== null && (maxTs === null || ts > maxTs)) maxTs = ts;
  }
  if (!_transcriptReasoningSyncWatermarks.has(key)) {
    // First pass = catch-up. An empty transcript starts the watermark at
    // 0 so everything that materializes after we started watching counts
    // as live; a populated one starts at its newest row.
    _transcriptReasoningSyncWatermarks.set(key, maxTs ?? 0);
    return 0;
  }
  const watermark = _transcriptReasoningSyncWatermarks.get(key);
  const fresh = [];
  for (const entry of entries) {
    if (String(entry?.kind || '') !== 'reasoning') continue;
    if (String(entry?.level || '') !== 'model') continue;
    if (!String(entry?.content || '').trim()) continue;
    const ts = sessionWindowTranscriptTimestampMs(
      entry?.ts_ms ?? entry?.tsMs ?? entry?.ts ?? entry?.timestamp ?? ''
    );
    if (ts === null || ts <= watermark) continue;
    fresh.push({ entry, ts });
  }
  fresh.sort((a, b) => a.ts - b.ts);
  let injected = 0;
  for (const { entry } of fresh) {
    const record = sessionWindowRecordFromReplayEntry(entry, sid);
    if (!record) continue;
    const targeted = { ...record, session_id: sid };
    const signatures = liveFeedReasoningSignatures(targeted);
    // Unsignable rows stay out: an injected row the live lane could not
    // recognize later would double with its own delayed copy.
    if (signatures.length === 0) continue;
    if (signatures.some(signature => _liveFeedReasoningSignatures.has(signature))) continue;
    const node = buildReasoningLogEntryNode(targeted);
    if (!node) continue;
    for (const signature of signatures) {
      _liveFeedReasoningSignatures.add(signature);
      _injectedReasoningSignatures.add(signature);
    }
    appendLogEntryElement(node, targeted);
    injected += 1;
  }
  if (maxTs !== null && maxTs > watermark) {
    _transcriptReasoningSyncWatermarks.set(key, maxTs);
  }
  return injected;
}

// QA readback (window.qa convention): a self-contained behavioral probe
// of this lane against the REAL functions — watermark catch-up, the
// freshness gate, idempotent re-sync, and the cross-lane dedupe in both
// directions. Uses a unique synthetic session id per run and scrubs its
// rows and mirror window on the way out; meant for throwaway QA
// dashboards (scripts/validate-dashboard.cjs --wait-for-function
// "window.qa.reasoningLiveParity().pass").
window.qa = Object.assign(window.qa || {}, {
  reasoningLiveParity() {
    const sid = 'qa-reasoning-parity-' + Date.now().toString(36);
    const source = 'claude-code';
    const t0 = Date.now() - 60000;
    const mk = (content, tsMs) => ({
      level: 'model',
      source: 'Claude Code',
      kind: 'reasoning',
      content,
      ts: new Date(tsMs).toISOString(),
      ts_ms: tsMs,
    });
    const countRows = () => document.querySelectorAll(
      `#log-stream .reasoning-log-entry[data-session-id="${sid}"]`
    ).length;
    const live = content => renderReasoningLogEntry({
      level: 'model', source: 'Claude Code', kind: 'reasoning',
      content, session_id: sid, ts_ms: Date.now(),
    });
    const report = { pass: false, sid };
    try {
      report.firstPassInjected = injectTranscriptReasoningIntoLiveFeed(
        [mk('probe row A', t0)], sid, source);
      report.secondPassInjected = injectTranscriptReasoningIntoLiveFeed(
        [mk('probe row A', t0), mk('probe row B', t0 + 1000)], sid, source);
      report.resyncInjected = injectTranscriptReasoningIntoLiveFeed(
        [mk('probe row A', t0), mk('probe row B', t0 + 1000)], sid, source);
      report.rowsAfterInject = countRows();
      // The injected row's own delayed live copy skips (and is consumed)…
      live('probe row B');
      report.rowsAfterLiveDup = countRows();
      // …so the same text genuinely thought again still renders.
      live('probe row B');
      report.rowsAfterLiveRepeat = countRows();
      live('probe row C');
      report.rowsAfterLiveNew = countRows();
      report.postLiveResyncInjected = injectTranscriptReasoningIntoLiveFeed(
        [mk('probe row C', t0 + 2000)], sid, source);
      report.pass =
        report.firstPassInjected === 0 &&
        report.secondPassInjected === 1 &&
        report.resyncInjected === 0 &&
        report.rowsAfterInject === 1 &&
        report.rowsAfterLiveDup === 1 &&
        report.rowsAfterLiveRepeat === 2 &&
        report.rowsAfterLiveNew === 3 &&
        report.postLiveResyncInjected === 0;
    } finally {
      const synthetic = document.querySelectorAll(`.log-entry[data-session-id="${sid}"]`);
      const removedFromMain = Array.from(synthetic)
        .filter(el => el.closest('#log-stream')).length;
      synthetic.forEach(el => el.remove());
      if (typeof removeSessionWindow === 'function') removeSessionWindow(sid);
      if (typeof logEntryCount === 'number') {
        logEntryCount = Math.max(0, logEntryCount - removedFromMain);
        updateLogEmptyState();
      }
    }
    return report;
  },
});
