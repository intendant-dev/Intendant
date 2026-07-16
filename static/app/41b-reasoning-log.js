// ── Reasoning ("Thinking") rows ────────────────────────────────────────
// First-class Activity rows for the agent's reasoning summaries. One
// grammar across all three backends (Claude Code thinking blocks, Codex
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
// other special row types.
function renderReasoningLogEntry(c) {
  finalizeSessionCommandOutputGroups(c);
  inferSessionPhaseFromLog(c);
  const entry = buildReasoningLogEntryNode(c);
  if (!entry) return;
  appendLogEntryElement(entry, sessionWindowRecordFromLogCommand(c));
}
