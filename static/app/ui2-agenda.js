// Agenda: the daemon's durable ledger of parked intent (tasks, notes,
// deferred follow-ups). Two surfaces share one cache: the Agenda tab
// (#tab-agenda — list, filters, composer) and a compact card on the
// activity pane stacked under the vitals rail. Data flows through
// daemonApi (tunnel `api_agenda_list` / `api_agenda_op`, HTTP twin
// fallback) and refreshes live on the `agenda_changed` event lane.
//
// Item bodies are DATA, never instructions: everything renders through
// escapeHtml as plain quoted text — no markdown execution, no HTML.

let agendaItems = null; // null = never fetched (fetch on first need)
let agendaCounts = { open: 0, done: 0, retired: 0 };
let agendaSkippedLines = 0;
let agendaFilter = 'open';
let agendaFetchInFlight = null;
let agendaLoadError = '';
let agendaReminderPolicy = null; // owner delivery policy (Settings-gated)
// Session-resolution join from the list response: recorded session id →
// { source, conversation_id, key, name } for the Sessions-tab row. Ids the
// daemon could not resolve have no entry — surfaces fall back to the raw
// id. `attempted` remembers ids a fetch already tried, so an unresolvable
// id never causes refetch loops on the event lane.
let agendaSessions = {};
let agendaSessionLookupsAttempted = new Set();
// Items whose full annotation thread is expanded (render caps at 3).
const agendaExpandedThreads = new Set();

async function agendaRefresh() {
  if (agendaFetchInFlight) return agendaFetchInFlight;
  agendaFetchInFlight = (async () => {
    try {
      const resp = await daemonApi.request('api_agenda_list', {});
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        agendaItems = resp.body.items;
        agendaCounts = resp.body.counts || agendaCounts;
        agendaSkippedLines = resp.body.skipped_lines || 0;
        agendaReminderPolicy = resp.body.reminder_policy || agendaReminderPolicy;
        agendaSessions = resp.body.sessions || {};
        agendaSessionLookupsAttempted = new Set(
          agendaItems.flatMap(agendaItemSessionIds));
        agendaLoadError = '';
        agendaAnnounceParkedAsks();
      } else {
        agendaLoadError = (resp.body && resp.body.error) || `agenda unavailable (${resp.status})`;
      }
    } catch (e) {
      agendaLoadError = String(e && e.message || e);
    } finally {
      agendaFetchInFlight = null;
    }
    agendaRenderAll();
  })();
  return agendaFetchInFlight;
}

// Parked rich asks (ask↔agenda unification, slice 1) re-surface on the
// question rail after a FRESH load — a daemon restart wipes the
// state-line replay cache, and a parked question must not evaporate with
// it. Dispatch the exact show_user_question path live asks ride; the
// same-id re-show dedupe makes double delivery (state-line replay racing
// this) harmless. Once per page load per ask id — and never for a
// DISMISSED item (`item.dismissed`, still open): the owner cleared it
// from the rails deliberately, so it stays cleared across loads; the
// card's "Open question panel" button is the deliberate way back, and
// answering or reopening clears the marker (the log keeps the dismissal
// as history).
const agendaAnnouncedAsks = new Set();
function agendaAnnounceParkedAsks() {
  if (!Array.isArray(agendaItems)) return;
  if (typeof showUserQuestion !== 'function') return;
  if (typeof processingLogReplay !== 'undefined' && processingLogReplay) {
    // Replay is momentary (session selection); retry shortly rather than
    // losing the announce until the next full fetch.
    setTimeout(agendaAnnounceParkedAsks, 500);
    return;
  }
  const open = agendaItems
    .filter((item) => item.status === 'open'
      && item.ask && item.ask.ask_id && Array.isArray(item.ask.questions)
      && item.ask.questions.length && !item.dismissed)
    // Oldest first, so with several parked asks the panel lands on the
    // newest — the same "latest ask surfaces" behavior live asks have.
    .sort((a, b) => (a.id < b.id ? -1 : 1));
  for (const item of open) {
    const askId = item.ask.ask_id;
    if (agendaAnnouncedAsks.has(askId)) continue;
    agendaAnnouncedAsks.add(askId);
    showUserQuestion(askId, item.ask.questions, '', undefined, false, { agendaBacked: true });
  }
}

// Explicit "open the question panel" from an agenda item card. Unlike the
// once-per-load announce this is a user act: it re-surfaces even a tucked
// or previously-dismissed panel, and it navigates to the Activity tab
// where the panel lives (opening invisibly confused live QA 2026-07-20 —
// the item card only offered the blind plain-text input).
function agendaOpenParkedAsk(itemId) {
  const item = (agendaItems || []).find((candidate) => candidate.id === itemId);
  if (!item || !item.ask || !Array.isArray(item.ask.questions) || !item.ask.questions.length) {
    return;
  }
  if (typeof showUserQuestion !== 'function') return;
  const askId = item.ask.ask_id;
  if (typeof switchTab === 'function') switchTab('activity');
  agendaAnnouncedAsks.add(askId);
  if (typeof pendingQuestion !== 'undefined' && pendingQuestion?.id === askId) {
    // Already the pending panel (maybe tucked): an explicit open always
    // brings it back.
    setQuestionMinimized(false);
    return;
  }
  showUserQuestion(askId, item.ask.questions, '', undefined, false, { agendaBacked: true });
  // A rebuild after dismissal starts untucked; make sure a stale tucked
  // state never survives an explicit open.
  if (typeof setQuestionMinimized === 'function') setQuestionMinimized(false);
}

// Live update from the event lane: merge the changed item, adopt counts.
function agendaObserveServerMessage(d) {
  if (!d || !d.item || !d.item.id) return;
  if (agendaItems === null) {
    // Card/tab never fetched; only bother if either surface is live.
    if (document.getElementById('ui2-agenda-card') || agendaTabVisible()) agendaRefresh();
    return;
  }
  const at = agendaItems.findIndex((item) => item.id === d.item.id);
  if (at >= 0) agendaItems[at] = d.item;
  else agendaItems.push(d.item);
  if (d.counts) agendaCounts = d.counts;
  // A session id this tab has never tried to resolve (a fresh session
  // parked something): refetch once to pick up the join entry. Ids that
  // already failed resolution stay raw — no loops.
  const unresolved = agendaItemSessionIds(d.item).some(
    (id) => !(id in agendaSessions) && !agendaSessionLookupsAttempted.has(id));
  if (unresolved) agendaRefresh();
  agendaRenderAll();
}

// Every session id an item's attribution views reference (provenance,
// answer, effect proposals and runs) — the daemon-side twin drives the
// join map in the list response.
function agendaItemSessionIds(item) {
  const ids = [];
  if (item.provenance && item.provenance.session_id) ids.push(item.provenance.session_id);
  if (item.answer && item.answer.session_id) ids.push(item.answer.session_id);
  (item.effects || []).forEach((effect) => {
    if (effect.proposed_session_id) ids.push(effect.proposed_session_id);
    if (effect.last_run && effect.last_run.session_id) ids.push(effect.last_run.session_id);
  });
  return ids;
}

function agendaTabVisible() {
  const pane = document.getElementById('tab-agenda');
  return !!(pane && pane.classList.contains('active'));
}

function agendaOnTabShown() {
  if (agendaItems === null) agendaRefresh();
  else agendaRenderAll();
}

async function agendaSendOp(params, button) {
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (resp.ok && resp.body && resp.body.item) {
      // The event lane repaints too; merging here keeps the UI honest
      // even if this tab's event socket is briefly down.
      agendaObserveServerMessage({ item: resp.body.item });
      return true;
    }
    const message = (resp.body && resp.body.error) || `agenda op failed (${resp.status})`;
    agendaFlashError(message);
    return false;
  } catch (e) {
    agendaFlashError(String(e && e.message || e));
    return false;
  } finally {
    if (button) button.disabled = false;
  }
}

function agendaFlashError(message) {
  const note = document.getElementById('agenda-tab-skipped');
  if (!note) return;
  note.style.display = '';
  note.textContent = message;
  setTimeout(() => {
    note.textContent = '';
    agendaRenderAll(); // restores the skipped-lines note if one applies
  }, 6000);
}

function agendaGlyph(status, kind) {
  if (status === 'done') return '<span class="agenda-glyph done" aria-label="done">✓</span>';
  if (status === 'retired') return '<span class="agenda-glyph retired" aria-label="retired">⊘</span>';
  if (kind === 'question') return '<span class="agenda-glyph question" aria-label="open question">?</span>';
  return '<span class="agenda-glyph open" aria-label="open">○</span>';
}

function agendaDueChip(item) {
  if (!item.due_ms) return '';
  const due = new Date(item.due_ms);
  const overdue = item.status === 'open' && item.due_ms < Date.now();
  const label = due.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    + (due.getHours() || due.getMinutes()
      ? ' ' + due.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
      : '');
  return `<span class="agenda-chip due${overdue ? ' overdue' : ''}">due ${escapeHtml(label)}</span>`;
}

function agendaSessionInfo(id) {
  return (id && agendaSessions && agendaSessions[id]) || null;
}

// ---- F2 derived presentation (client twin of the daemon's is_blocked /
// dependency_state — like the overdue chip, derived at render time from
// facts the tab already holds; never stored, never on the wire).

function agendaFindItem(id) {
  return (agendaItems || []).find((item) => item.id === id) || null;
}

// One edge's render judgment: { satisfied, review } where review is
// '' | 'target_retired' | 'target_missing'.
function agendaEdgeState(edge) {
  const target = agendaFindItem(edge.target_id);
  if (!target) return { satisfied: false, review: 'target_missing' };
  if (target.status === 'done') return { satisfied: true, review: '' };
  if (target.status === 'retired') return { satisfied: false, review: 'target_retired' };
  return { satisfied: false, review: '' };
}

function agendaItemIsBlocked(item) {
  if (item.status !== 'open') return false;
  if ((item.blockers || []).some((b) => !b.cleared)) return true;
  return (item.relies_on || []).some((edge) => !agendaEdgeState(edge).satisfied);
}

function agendaActorLabel(p) {
  // Gate-attributed actor (A2), rendered for humans. Session ids resolve
  // through the join map to the conversation's human name; unresolved ids
  // degrade to the raw truncated id. Plain TEXT only — callers escape.
  if (p.session_id) {
    const s = agendaSessionInfo(p.session_id);
    if (s && s.name) return `session “${s.name}”`;
    if (s) {
      const prefix = s.source && s.source !== 'intendant' ? `${s.source} ` : '';
      return `${prefix}session ${String(s.conversation_id || p.session_id).slice(0, 8)}`;
    }
    return `session ${p.session_id.slice(0, 12)}`;
  }
  if (p.kind === 'dashboard') return 'you';
  if (p.kind === 'local_process') return 'local ctl';
  if (p.kind === 'peer') return 'a peer daemon';
  if (p.kind === 'agent_session') return 'an agent session';
  return p.principal || '';
}

// Full attribution HTML: the resolved session name as a jump link to its
// Sessions-tab conversation row, raw ids + principal + kind in the tooltip
// (no more suppression — they moved, not vanished), and self-described
// `--source` labels rendered visibly AS self-described. Everything is
// data: each fragment is escaped, none of it is ever executed.
function agendaActorHtml(p) {
  const bits = [];
  if (p.session_id) {
    const s = agendaSessionInfo(p.session_id);
    const label = agendaActorLabel(p);
    const tip = [
      `session id: ${p.session_id}`,
      s && s.conversation_id && s.conversation_id !== p.session_id
        ? `conversation: ${s.conversation_id}` : '',
      p.principal ? `principal: ${p.principal}` : '',
      p.kind ? `kind: ${p.kind}` : '',
    ].filter(Boolean).join('\n');
    if (s && s.key) {
      bits.push(`<a href="#sessions" class="agenda-session-link" data-session-key="${escapeHtml(s.key)}" title="${escapeHtml(tip)}">${escapeHtml(label)}</a>`);
    } else {
      bits.push(`<span title="${escapeHtml(tip)}">${escapeHtml(label)}</span>`);
    }
  } else {
    const label = agendaActorLabel(p);
    if (label) {
      bits.push(p.principal && label !== p.principal
        ? `<span title="${escapeHtml(`principal: ${p.principal}`)}">${escapeHtml(label)}</span>`
        : escapeHtml(label));
    }
  }
  if (p.source) {
    bits.push(`<span class="agenda-self-described" title="self-described label — UNVERIFIED, never attribution">— self-described: ${escapeHtml(p.source)}</span>`);
  }
  return bits.join(' ');
}

function agendaProvenanceLine(item) {
  const p = item.provenance || {};
  const created = p.created_ms
    ? new Date(p.created_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : '';
  const who = agendaActorHtml(p);
  const parts = [];
  if (created) parts.push(escapeHtml(`parked ${created}`));
  if (who) parts.push(`by ${who}`);
  return parts.join(' · ');
}

// Jump to the conversation's row on the Sessions tab: switch tabs, then
// focus/flash the card once the list renders it (rows are keyed by
// sessionListRowKey = source<conversation id>). If the row is not in
// the loaded window the jump degrades to just opening the tab.
function agendaJumpToSession(key) {
  if (!key) return;
  routeTo('sessions');
  const deadline = Date.now() + 4000;
  const selector = `[data-session-key="${window.CSS && CSS.escape ? CSS.escape(key) : key}"]`;
  const seek = () => {
    const card = document.querySelector(selector);
    if (card) {
      card.scrollIntoView({ block: 'center', behavior: 'smooth' });
      card.classList.add('agenda-jump-flash');
      setTimeout(() => card.classList.remove('agenda-jump-flash'), 2400);
      return;
    }
    if (Date.now() < deadline) setTimeout(seek, 200);
  };
  seek();
}

// Scheduled-session effect (A5): render the manifest under review, its
// digest, and the approval state. Approval is an owner-surface act — the
// dashboard is one, so the Approve button lives here; it carries the digest
// of exactly the revision rendered, so what you approve is what you read
// (a concurrent re-propose makes the click fail with "digest mismatch").
function agendaEffectBlock(item) {
  const effect = (item.effects || [])[0];
  if (!effect || !effect.manifest) return '';
  const manifest = effect.manifest;
  const when = manifest.fire_at_ms ? new Date(manifest.fire_at_ms) : null;
  const whenLabel = when
    ? when.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
      + ' ' + when.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
    : '';
  const chips =
    (whenLabel ? `<span class="agenda-chip due">at ${escapeHtml(whenLabel)}</span>` : '')
    + (manifest.orchestrate ? '<span class="agenda-chip">orchestrate</span>' : '')
    + (manifest.interactive ? '<span class="agenda-chip" title="Opens the session with the goal as its first message, then waits for you">interactive</span>' : '')
    + (manifest.project_root
      ? `<span class="agenda-chip" title="${escapeHtml(`project: ${manifest.project_root}`)}">${escapeHtml(agendaShortPath(manifest.project_root))}</span>`
      : '');
  const proposer = agendaActorLabel({
    kind: effect.proposed_kind,
    session_id: effect.proposed_session_id,
    principal: effect.proposed_principal,
  });
  const digestShort = escapeHtml(String(effect.digest || '').slice(0, 12));
  const run = effect.last_run;
  let stateHtml = '';
  let noteHtml = '';
  let actions = '';
  if (run) {
    const glyphs = { completed: '✓', failed: '✗', missed: '⊘', unknown: '?', started: '▶' };
    const classes = { completed: 'completed', failed: 'failed', started: 'armed' };
    const bits = [`${glyphs[run.state] || '·'} ${run.state}`];
    if (run.session_id) bits.push(agendaActorLabel({ session_id: run.session_id }));
    stateHtml = `<span class="agenda-effect-state ${classes[run.state] || 'attention'}">${escapeHtml(bits.join(' · '))}</span>`;
    if (run.note) {
      // Session summaries / failure reasons are quoted data, like bodies.
      noteHtml = `<div class="agenda-effect-note">${escapeHtml(run.note)}</div>`;
    }
  } else if (effect.approval) {
    const who = agendaActorLabel(effect.approval);
    stateHtml = `<span class="agenda-effect-state armed">approved${who ? ` by ${escapeHtml(who)}` : ''}</span>`;
    if (item.status === 'open') {
      actions = `<button type="button" class="agenda-btn" data-op="revoke_effect" data-id="${escapeHtml(item.id)}">Revoke</button>`;
    }
  } else {
    stateHtml = '<span class="agenda-effect-state pending">awaiting your approval</span>';
    if (item.status === 'open') {
      actions = `<button type="button" class="agenda-btn approve" data-op="approve_effect" data-id="${escapeHtml(item.id)}" data-digest="${escapeHtml(effect.digest || '')}">Approve</button>`;
    }
  }
  return `<div class="agenda-effect">
    <div class="agenda-effect-head">
      <span class="agenda-effect-icon" aria-hidden="true">⏵</span>
      <span class="agenda-effect-label">Scheduled session</span>
      ${chips}${stateHtml}
    </div>
    <div class="agenda-effect-goal">${escapeHtml(manifest.goal || '')}</div>
    ${noteHtml}
    <div class="agenda-item-foot">
      <span class="agenda-item-meta">digest ${digestShort}${proposer ? ` · proposed by ${escapeHtml(proposer)}` : ''}</span>
      <span class="agenda-item-actions">${actions}</span>
    </div>
  </div>`;
}

// The dismissal chip's tooltip (marker on a still-open question whose
// rail card was skipped/denied). Plain TEXT — the caller escapes.
function agendaDismissedTip(dismissed) {
  const when = dismissed.at_ms
    ? new Date(dismissed.at_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : '';
  const verb = dismissed.action ? `(${dismissed.action}) ` : '';
  return `Dismissed ${verb}${when ? `on ${when} ` : ''}from the question rail — it stays open `
    + 'and answerable here, but is not auto-announced again; Open question panel brings it back.';
}

// The recorded reply on a resolved question. Plain answers render the
// joined text; rich (ask-backed) answers render the STRUCTURED breakdown
// (`item.answer.structured` — maps keyed by question text), walked in the
// ITEM's question order exactly like the daemon's text summary: a
// "Header: answer" line per engaged question, the picked option labels,
// follow-ups, and anchored preview notes. All of it is quoted data —
// escaped, never executed, never instructions.
function agendaAnswerBlock(item) {
  const answer = item.answer;
  const who = agendaActorLabel(answer);
  const when = answer.at_ms
    ? new Date(answer.at_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : '';
  const meta = `<span class="agenda-item-meta">— ${escapeHtml([who && `by ${who}`, when].filter(Boolean).join(' · '))}</span>`;
  const questions = (item.ask && Array.isArray(item.ask.questions)) ? item.ask.questions : [];
  const rows = answer.structured
    ? agendaStructuredAnswerRows(questions, answer.structured)
    : [];
  if (!rows.length) {
    return `<div class="agenda-item-answer">↳ ${escapeHtml(answer.text || '')}
      ${meta}</div>`;
  }
  return `<div class="agenda-item-answer agenda-answer-structured">${rows.join('')}
    <div>${meta}</div></div>`;
}

// One row per engaged question of a structured resolution. A question is
// engaged when any of the maps mention it; a follow-up may stand in for
// an answer, so the answer text is optional on the line.
function agendaStructuredAnswerRows(questions, s) {
  const answers = s.answers || {};
  const selections = s.selections || {};
  const followups = s.followups || {};
  const annotations = s.annotations || {};
  const engaged = questions.filter((q) =>
    q.question in answers || q.question in selections
    || q.question in followups || q.question in annotations);
  return engaged.map((q) => {
    const name = q.header || q.question;
    const answerText = answers[q.question];
    const head = answerText !== undefined
      ? `<span class="agenda-answer-qname" title="${escapeHtml(q.question)}">${escapeHtml(name)}:</span> ${escapeHtml(answerText)}`
      : `<span class="agenda-answer-qname" title="${escapeHtml(q.question)}">${escapeHtml(name)}</span>`;
    const picks = (selections[q.question] || [])
      .map((label) => `<span class="agenda-chip pick">✓ ${escapeHtml(label)}</span>`)
      .join(' ');
    const extras = [];
    if (followups[q.question] !== undefined) {
      extras.push(`<div class="agenda-answer-extra">follow-up: ${escapeHtml(followups[q.question])}</div>`);
    }
    for (const note of annotations[q.question] || []) {
      extras.push(`<div class="agenda-answer-extra">note on ${escapeHtml(note.preview)}: ${escapeHtml(note.note)}</div>`);
    }
    return `<div class="agenda-answer-q">
      <div class="agenda-answer-line">${head}</div>
      ${picks ? `<div class="agenda-answer-picks">${picks}</div>` : ''}
      ${extras.join('')}
    </div>`;
  });
}

// The item's thread + gates (F2): annotations (capped with an expander),
// blockers with their clear affordance and cleared history, dependency
// chips with satisfied/review markers, and the note/block composer.
// Everything is data rendered escaped; nothing here executes or obeys.
function agendaThreadBlock(item) {
  const parts = [];
  const notes = item.annotations || [];
  if (notes.length) {
    const cap = 3;
    const shown = agendaExpandedThreads.has(item.id) ? notes : notes.slice(-cap);
    const rows = shown.map((note) => {
      const when = note.at_ms
        ? new Date(note.at_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
        : '';
      const who = agendaActorHtml(note);
      const meta = [who, escapeHtml(when)].filter(Boolean).join(' · ');
      return `<div class="agenda-note">↳ ${escapeHtml(note.text)}
        <span class="agenda-item-meta">— ${meta}</span></div>`;
    });
    const more = notes.length > shown.length
      ? `<button type="button" class="agenda-thread-more" data-id="${escapeHtml(item.id)}">show all ${notes.length} notes</button>`
      : '';
    parts.push(`<div class="agenda-thread">${rows.join('')}${more}</div>`);
  }
  const blockers = item.blockers || [];
  if (blockers.length) {
    const rows = blockers.map((blocker) => {
      const who = agendaActorHtml(blocker);
      if (blocker.cleared) {
        const clearedBy = agendaActorHtml(blocker.cleared);
        return `<div class="agenda-blocker cleared">✓ <s>${escapeHtml(blocker.criterion)}</s>
          <span class="agenda-item-meta">— cleared${clearedBy ? ` by ${clearedBy}` : ''}</span></div>`;
      }
      const clear = item.status === 'open'
        ? `<button type="button" class="agenda-btn agenda-clear-blocker" data-id="${escapeHtml(item.id)}" data-blocker="${escapeHtml(blocker.blocker_id)}">Clear</button>`
        : '';
      return `<div class="agenda-blocker">
        <span class="agenda-blocker-text" title="${escapeHtml(blocker.blocker_id)}">⛔ ${escapeHtml(blocker.criterion)}</span>
        <span class="agenda-item-meta">${who ? `— set by ${who}` : ''}</span>${clear}
      </div>`;
    });
    parts.push(`<div class="agenda-blockers">${rows.join('')}</div>`);
  }
  const edges = item.relies_on || [];
  if (edges.length) {
    const chips = edges.map((edge) => {
      const state = agendaEdgeState(edge);
      const target = agendaFindItem(edge.target_id);
      const label = target ? target.title : edge.target_id.slice(0, 10);
      const marker = state.review === 'target_retired'
        ? ' · prerequisite retired — review'
        : state.review === 'target_missing'
          ? ' · prerequisite missing'
          : '';
      const cls = state.satisfied ? 'satisfied' : state.review ? 'review' : 'waiting';
      const remove = item.status === 'open'
        ? `<button type="button" class="agenda-edge-remove" data-id="${escapeHtml(item.id)}" data-target="${escapeHtml(edge.target_id)}" aria-label="Remove dependency" title="Remove dependency">×</button>`
        : '';
      return `<span class="agenda-edge ${cls}" title="${escapeHtml(edge.target_id)}">
        ${state.satisfied ? '✓' : '…'} needs ${escapeHtml(label)}${escapeHtml(marker)}${remove}</span>`;
    });
    parts.push(`<div class="agenda-edges">${chips.join('')}</div>`);
  }
  // Composer: annotate any non-retired item; block only open ones.
  if (item.status !== 'retired') {
    const blockBtn = item.status === 'open'
      ? `<button type="button" class="agenda-btn agenda-thread-block" data-id="${escapeHtml(item.id)}">Block</button>`
      : '';
    parts.push(`<div class="agenda-thread-add">
      <input type="text" class="agenda-thread-input" maxlength="4000"
             placeholder="Add a note — or state a blocker…" aria-label="Note or blocker" data-id="${escapeHtml(item.id)}" />
      <button type="button" class="agenda-btn agenda-thread-note" data-id="${escapeHtml(item.id)}">Note</button>
      ${blockBtn}
    </div>`);
  }
  return parts.length ? `<div class="agenda-item-thread">${parts.join('')}</div>` : '';
}

// F3 follow-up affordance: the live, composer-targetable session window
// carrying the item's recorded conversation, if one exists RIGHT NOW.
// Purely a navigation affordance — sessions die, so items must stand
// alone; this appears only when following up happens to be possible.
function agendaFollowUpSid(item) {
  const recorded = item.provenance && item.provenance.session_id;
  if (!recorded) return null;
  if (typeof sessionWindows === 'undefined'
    || typeof isPromptTargetSessionUsable !== 'function') return null;
  const s = agendaSessionInfo(recorded);
  const conversationId = (s && s.conversation_id) || recorded;
  for (const sid of sessionWindows.keys()) {
    if (!isPromptTargetSessionUsable(sid)) continue;
    if (sid === recorded || sid === conversationId) return sid;
    const meta = (typeof sessionMetadataById !== 'undefined'
      && sessionMetadataById.get(sid)) || {};
    const backend = String(meta.backend_session_id || meta.backendSessionId || '').trim();
    if (backend && backend === conversationId) return sid;
  }
  return null;
}

// The ORIGIN conversation when it is not live but still resolvable on this
// daemon (the list response's sessions join): ended-but-resumable. The
// follow-up then rides the EXISTING resume path — never an unrelated new
// session (owner ruling, 2026-07-21).
function agendaFollowUpResumable(item) {
  const recorded = item.provenance && item.provenance.session_id;
  if (!recorded) return null;
  const info = agendaSessionInfo(recorded);
  if (!info || !info.conversation_id) return null;
  if (typeof resumeSession !== 'function') return null;
  return info;
}

// Prefill the activity composer with the item quoted as data. No daemon
// write happens here; the user sends when ready.
function agendaQuoteIntoComposer(item) {
  const input = document.getElementById('activity-task-input');
  if (!input) return;
  const body = item.body ? `\n> ${String(item.body).split('\n').join('\n> ')}` : '';
  input.value =
    `Following up on agenda item ${item.id} (quoted):\n> ${item.title}${body}\n\n`;
  input.focus();
  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.setSelectionRange(input.value.length, input.value.length);
}

// Open the composer targeted at the recorder's LIVE conversation with the
// item quoted as data. No daemon write happens here.
function agendaFollowUpWithRecorder(item, sid) {
  routeTo('activity');
  if (typeof focusSessionWindow === 'function') focusSessionWindow(sid);
  agendaQuoteIntoComposer(item);
}

// Follow up on an ENDED origin conversation: resume it through the same
// path the Sessions tab uses (the daemon applies the session's persisted
// launch config; the recorded project root rides along for external
// CLIs), then target the composer at it with the item quoted. The resume
// attaches to the SAME conversation — never a fresh unrelated session.
function agendaFollowUpResume(item) {
  const recorded = item.provenance && item.provenance.session_id;
  const info = agendaSessionInfo(recorded);
  if (!info || typeof resumeSession !== 'function') return;
  const conversationId = info.conversation_id || recorded;
  resumeSession({
    session_id: conversationId,
    source: info.source || 'intendant',
    backend_session_id: conversationId,
    project_root: info.project_root || null,
  });
  agendaQuoteIntoComposer(item);
}

// Leading-component path truncation for chips (the tail is the
// informative part). Reuses the vitals helper when loaded.
function agendaShortPath(path) {
  if (typeof vitalsLeadingTruncatedPath === 'function') {
    return vitalsLeadingTruncatedPath(path, 28);
  }
  const raw = String(path || '');
  return raw.length > 28 ? `…${raw.slice(-27)}` : raw;
}

// ---- Start-now confirm sheet ----
// The explanation IS the surface (owner ruling 2026-07-21): before anything
// runs, the sheet shows what will run — editable goal text, the resolved
// project, the config the spawn inherits — with an Interactive/Goal-run
// toggle defaulting to Interactive. Bottom sheet on coarse pointers /
// narrow viewports, anchored popover-card on desktop (the #vitals-explainer
// house mechanics); tooltips may remain but nothing DEPENDS on hover.

let agendaStartSheetItemId = null;
let agendaStartSheetMode = 'interactive';
let agendaStartSheetGoalDirty = false;
// Daemon default project root: null = not fetched yet, '' = projectless
// daemon, non-empty = the default. Same source the New Session pane uses
// (api_project_root via fetchProjectRoot).
let agendaDaemonDefaultProject = null;
// Daemon settings snapshot for the start sheet's config controls: null =
// not fetched yet. Refetched on every sheet open so the daemon defaults
// shown (backend, model, effort) are current, not boot-time stale.
let agendaStartSheetSettings = null;

const AGENDA_START_MODES = [
  {
    value: 'interactive',
    label: 'Interactive',
    note: 'Opens the session with this text as its first message, then waits for you — like a session started from the composer.',
  },
  {
    value: 'goal',
    label: 'Goal run',
    note: 'Runs the text autonomously as a supervised goal; follow-through instructions are appended and the outcome is written back to this item.',
  },
];

function agendaSheetFormFactor() {
  if (typeof vitalsExplainerUsesSheet === 'function') return vitalsExplainerUsesSheet();
  return window.matchMedia('(max-width: 720px)').matches
    || window.matchMedia('(pointer: coarse)').matches;
}

// The sheet's default goal statement: the item quoted as data with its id
// (the daemon composes the same statement for parameterless callers; the
// sheet always SENDS its editable text, so what you read is what runs —
// plus the selected mode's fixed coda, named by the mode note).
function agendaStartGoalStatement(item) {
  let statement = `Agenda follow-through for item ${item.id}: ${item.title}`;
  if (item.body && item.body.trim()) {
    statement += `\n\nItem body (quoted):\n${item.body}`;
  }
  return statement;
}

// Project prefill resolution, mirroring the daemon's ratified order:
// the parking session's recorded root (from the list response's sessions
// join) → the daemon default → an explicit pick is REQUIRED.
function agendaStartProjectResolution(item) {
  const recorded = item.provenance && item.provenance.session_id;
  const info = agendaSessionInfo(recorded);
  if (info && info.project_root) {
    return { value: info.project_root, source: 'provenance' };
  }
  if (agendaDaemonDefaultProject) {
    return { value: agendaDaemonDefaultProject, source: 'daemon_default' };
  }
  return { value: '', source: agendaDaemonDefaultProject === null ? 'unknown' : 'none' };
}

function agendaStartProjectHint(source) {
  if (source === 'provenance') return 'from the parking session';
  if (source === 'daemon_default') return 'daemon default';
  if (source === 'none') {
    return 'required — this daemon runs without a default project';
  }
  return 'checking the daemon default…';
}

function agendaEnsureStartSheet() {
  let host = document.getElementById('agenda-start-sheet');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'agenda-start-sheet';
  host.hidden = true;
  const backdrop = document.createElement('div');
  backdrop.className = 'ags-backdrop';
  backdrop.addEventListener('click', agendaCloseStartSheet);
  const panel = document.createElement('div');
  panel.className = 'ags-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Start a session from this agenda item');
  host.appendChild(backdrop);
  host.appendChild(panel);
  document.body.appendChild(host);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !host.hidden) agendaCloseStartSheet();
  });
  // Capture phase, mirroring the vitals explainer: any outside press
  // dismisses; a press on another item's Start now re-opens fresh.
  document.addEventListener('pointerdown', (event) => {
    if (host.hidden) return;
    if (event.target.closest?.('#agenda-start-sheet .ags-panel, .agenda-start-now')) return;
    agendaCloseStartSheet();
  }, true);
  return host;
}

function agendaCloseStartSheet() {
  const host = document.getElementById('agenda-start-sheet');
  if (!host) return;
  host.hidden = true;
  host.classList.remove('sheet', 'popover');
  agendaStartSheetItemId = null;
}

function agendaStartSheetEl(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

// The execution line under the config block (the backend/model/effort rows
// above it are live controls, not a summary).
function agendaStartExecutionSummary(mode) {
  const execution = mode === 'interactive'
    ? 'composer defaults (waits for you after opening)'
    : 'direct goal run';
  return `${execution} · supervised, normal approvals`;
}

// The sheet's per-backend config vocabulary, from the daemon's served
// settings (derive-don't-mirror: models and efforts come from the settings
// payload where the daemon serves them; the static kimi/claude-alias lists
// mirror the pinned settings-pane markup). `model`/`effort` are the daemon
// DEFAULTS the selects inherit when left untouched.
function agendaStartBackendConfig(settings) {
  const d = settings || {};
  const backend = (typeof normalizeAgentId === 'function')
    ? normalizeAgentId(d.external_agent) : (d.external_agent || '');
  if (backend === 'claude-code') {
    return {
      backend,
      label: 'Claude Code',
      modelKey: 'claude_model',
      effortKey: 'claude_effort',
      effortLabel: 'Reasoning',
      model: String(d.claude_model || ''),
      models: ['fable', 'opus', 'sonnet', 'haiku'],
      effort: String(d.claude_effort || ''),
      efforts: Array.isArray(d.claude_efforts) ? d.claude_efforts : [],
    };
  }
  if (backend === 'codex') {
    const models = Array.isArray(d.codex_models) ? d.codex_models.map(m => m.id) : [];
    return {
      backend,
      label: 'Codex',
      modelKey: 'codex_model',
      effortKey: 'codex_reasoning_effort',
      effortLabel: 'Reasoning',
      model: String(d.codex_model || ''),
      models,
      effort: String(d.codex_reasoning_effort || ''),
      efforts: Array.isArray(d.codex_reasoning_efforts) ? d.codex_reasoning_efforts : [],
    };
  }
  if (backend === 'kimi') {
    return {
      backend,
      label: 'Kimi Code',
      modelKey: 'kimi_model',
      effortKey: 'kimi_thinking',
      effortLabel: 'Thinking',
      model: String(d.kimi_model || ''),
      models: ['kimi-code/kimi-for-coding', 'kimi-code/kimi-for-coding-highspeed', 'kimi-code/k3'],
      effort: String(d.kimi_thinking || ''),
      efforts: ['off', 'low', 'medium', 'high', 'xhigh', 'max'],
    };
  }
  return { backend: '', label: 'Internal agent' };
}

function agendaPresentStartSheet(host, panel, anchor) {
  const sheet = agendaSheetFormFactor();
  host.hidden = false;
  host.classList.toggle('sheet', sheet);
  host.classList.toggle('popover', !sheet);
  panel.style.left = '';
  panel.style.top = '';
  if (sheet || !anchor?.getBoundingClientRect) return;
  const rect = anchor.getBoundingClientRect();
  const pw = Math.min(panel.offsetWidth || 380, window.innerWidth - 16);
  const ph = panel.offsetHeight || 300;
  const left = Math.max(8, Math.min(rect.left, window.innerWidth - pw - 8));
  let top = rect.bottom + 6;
  if (top + ph > window.innerHeight - 8) top = Math.max(8, rect.top - ph - 6);
  panel.style.left = `${Math.round(left)}px`;
  panel.style.top = `${Math.round(top)}px`;
}

function agendaOpenStartSheet(itemId, anchor) {
  const item = agendaFindItem(itemId);
  if (!item || item.status !== 'open') return;
  const host = agendaEnsureStartSheet();
  const panel = host.querySelector('.ags-panel');
  panel.textContent = '';
  agendaStartSheetItemId = itemId;
  agendaStartSheetMode = 'interactive';
  agendaStartSheetGoalDirty = false;

  // Header: the explanation leads.
  const head = agendaStartSheetEl('div', 'ags-head');
  head.appendChild(agendaStartSheetEl('span', 'ags-title', 'Start a session'));
  const close = agendaStartSheetEl('button', 'ags-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Cancel');
  close.addEventListener('click', agendaCloseStartSheet);
  head.appendChild(close);
  panel.appendChild(head);
  panel.appendChild(agendaStartSheetEl('div', 'ags-sub',
    'Runs a supervised session to work this item.'));
  panel.appendChild(agendaStartSheetEl('div', 'ags-item',
    `${item.kind}: ${item.title}`));

  // Editable goal text (what the session receives).
  const goalLabel = agendaStartSheetEl('label', 'ags-label', 'Goal — the session’s opening text');
  goalLabel.setAttribute('for', 'ags-goal');
  panel.appendChild(goalLabel);
  const goal = document.createElement('textarea');
  goal.id = 'ags-goal';
  goal.className = 'ags-goal';
  goal.rows = 5;
  goal.value = agendaStartGoalStatement(item);
  goal.addEventListener('input', () => { agendaStartSheetGoalDirty = true; });
  panel.appendChild(goal);

  // Project row: prefilled by the ratified resolution, always editable.
  const projLabel = agendaStartSheetEl('label', 'ags-label', 'Project directory');
  projLabel.setAttribute('for', 'ags-project');
  panel.appendChild(projLabel);
  const project = document.createElement('input');
  project.type = 'text';
  project.id = 'ags-project';
  project.className = 'ags-project';
  project.placeholder = 'Absolute path, e.g. /home/you/projects/thing';
  project.autocomplete = 'off';
  project.spellcheck = false;
  panel.appendChild(project);
  const projHint = agendaStartSheetEl('div', 'ags-hint', '');
  panel.appendChild(projHint);
  const applyResolution = () => {
    const resolved = agendaStartProjectResolution(item);
    // Never clobber a user edit; only fill while untouched.
    if (!project.dataset.touched && !project.value) {
      project.value = resolved.value;
    }
    // An empty box means "let the daemon resolve" (provenance → default),
    // so its hint honestly names that fallback; a typed value is an
    // explicit pick the manifest records verbatim.
    const source = project.value
      ? (project.dataset.touched ? 'explicit' : resolved.source)
      : resolved.source;
    projHint.textContent = source === 'explicit'
      ? 'explicit pick — recorded on the manifest'
      : agendaStartProjectHint(source);
    projHint.classList.toggle('ags-hint-required', !project.value && source === 'none');
  };
  project.addEventListener('input', () => {
    project.dataset.touched = '1';
    applyResolution();
  });
  applyResolution();
  // The daemon default arrives async on first open — same source as the
  // New Session pane (api_project_root); re-resolve when it lands.
  if (agendaDaemonDefaultProject === null && typeof fetchProjectRoot === 'function') {
    fetchProjectRoot()
      .then((d) => { agendaDaemonDefaultProject = (d && d.project_root) || ''; })
      .catch(() => { agendaDaemonDefaultProject = ''; })
      .finally(() => {
        if (agendaStartSheetItemId === itemId) applyResolution();
      });
  }

  // Config the spawn runs with: editable controls prefilled from the
  // DAEMON defaults (fetched fresh on open), with honest provenance —
  // an untouched select inherits ("daemon default (max)") and sends
  // nothing; an explicit pick is recorded on the manifest and applied.
  const config = agendaStartSheetEl('div', 'ags-config');
  panel.appendChild(config);
  const configState = { spec: null, modelSel: null, effortSel: null };
  const renderConfigControls = () => {
    config.textContent = '';
    configState.modelSel = null;
    configState.effortSel = null;
    if (agendaStartSheetSettings === null) {
      config.appendChild(agendaStartSheetEl('div', 'ags-config-line',
        'Loading the daemon’s launch defaults…'));
      return;
    }
    const spec = agendaStartBackendConfig(agendaStartSheetSettings);
    configState.spec = spec;
    const backendLine = agendaStartSheetEl('div', 'ags-config-line',
      `${spec.label} · daemon default backend`);
    config.appendChild(backendLine);
    if (!spec.backend) {
      config.appendChild(agendaStartSheetEl('div', 'ags-hint',
        'Model and provider follow the daemon’s native configuration.'));
      return;
    }
    const addSelect = (labelText, id, defaultValue, options) => {
      const row = agendaStartSheetEl('div', 'ags-config-row');
      const label = agendaStartSheetEl('label', 'ags-label', labelText);
      label.setAttribute('for', id);
      row.appendChild(label);
      const select = document.createElement('select');
      select.id = id;
      const inherit = document.createElement('option');
      inherit.value = '';
      inherit.textContent = defaultValue
        ? `Daemon default (${defaultValue})`
        : 'Daemon default (backend default)';
      select.appendChild(inherit);
      const values = [...options];
      if (defaultValue && !values.includes(defaultValue)) values.push(defaultValue);
      for (const value of values) {
        const option = document.createElement('option');
        option.value = value;
        option.textContent = value;
        select.appendChild(option);
      }
      row.appendChild(select);
      const hint = agendaStartSheetEl('div', 'ags-hint', 'daemon default');
      row.appendChild(hint);
      select.addEventListener('change', () => {
        hint.textContent = select.value
          ? 'explicit — recorded on the manifest'
          : 'daemon default';
      });
      config.appendChild(row);
      return select;
    };
    configState.modelSel = addSelect('Model', 'ags-config-model', spec.model, spec.models);
    configState.effortSel = addSelect(
      spec.effortLabel, 'ags-config-effort', spec.effort, spec.efforts);
  };
  renderConfigControls();
  // Fetch fresh daemon defaults on every open (the settings snapshot ages
  // while the tab sits); re-render when they land if the sheet is still
  // showing this item.
  if (typeof fetchDashboardSettings === 'function') {
    fetchDashboardSettings()
      .then((d) => { if (d && !d.error) agendaStartSheetSettings = d; })
      .catch(() => {})
      .finally(() => {
        if (agendaStartSheetItemId === itemId) renderConfigControls();
      });
  }

  // Interactive / Goal-run toggle (Interactive is the ratified default).
  const seg = agendaStartSheetEl('div', 'ags-seg');
  seg.setAttribute('role', 'group');
  seg.setAttribute('aria-label', 'Session mode');
  const note = agendaStartSheetEl('div', 'ags-note', AGENDA_START_MODES[0].note);
  const execution = agendaStartSheetEl('div', 'ags-config-line ags-execution',
    agendaStartExecutionSummary('interactive'));
  const syncSeg = () => {
    for (const btn of seg.querySelectorAll('button[data-mode]')) {
      const active = btn.dataset.mode === agendaStartSheetMode;
      btn.classList.toggle('active', active);
      btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    }
    const choice = AGENDA_START_MODES.find((m) => m.value === agendaStartSheetMode)
      || AGENDA_START_MODES[0];
    note.textContent = choice.note;
    execution.textContent = agendaStartExecutionSummary(agendaStartSheetMode);
  };
  for (const choice of AGENDA_START_MODES) {
    const btn = agendaStartSheetEl('button', 'ags-seg-btn', choice.label);
    btn.type = 'button';
    btn.dataset.mode = choice.value;
    btn.addEventListener('click', () => {
      agendaStartSheetMode = choice.value;
      syncSeg();
    });
    seg.appendChild(btn);
  }
  panel.appendChild(seg);
  panel.appendChild(note);
  panel.appendChild(execution);

  // Errors render inline — the sheet is the surface, not a toast race.
  const error = agendaStartSheetEl('div', 'ags-error', '');
  error.hidden = true;
  panel.appendChild(error);

  const foot = agendaStartSheetEl('div', 'ags-foot');
  const cancel = agendaStartSheetEl('button', 'ags-btn', 'Cancel');
  cancel.type = 'button';
  cancel.addEventListener('click', agendaCloseStartSheet);
  const start = agendaStartSheetEl('button', 'ags-btn ags-start', 'Start session');
  start.type = 'button';
  start.addEventListener('click', () =>
    agendaStartSheetSubmit(item, goal, project, error, start, configState));
  foot.appendChild(cancel);
  foot.appendChild(start);
  panel.appendChild(foot);

  syncSeg();
  agendaPresentStartSheet(host, panel, anchor);
  goal.focus();
}

async function agendaStartSheetSubmit(item, goal, project, error, startBtn, configState) {
  const goalText = (goal.value || '').trim();
  const projectText = (project.value || '').trim();
  const showError = (message) => {
    error.textContent = message;
    error.hidden = false;
  };
  error.hidden = true;
  if (!goalText) {
    showError('The goal text must not be empty.');
    goal.focus();
    return;
  }
  if (!projectText && agendaDaemonDefaultProject === '') {
    // Known-projectless with nothing resolved: the daemon would refuse —
    // say so here, pointing at the field (the daemon's named refusal
    // remains the backstop for every other caller).
    showError('Pick a project directory — this daemon runs without a default project.');
    project.focus();
    return;
  }
  const params = {
    op: 'start_now',
    id: item.id,
    goal: goalText,
    interactive: agendaStartSheetMode === 'interactive',
  };
  if (projectText) params.project_root = projectText;
  // Explicit config picks bind on the manifest; untouched selects send
  // NOTHING so the daemon's resolution chain fills them (honest inherit).
  // An explicit pick also pins the reviewed backend — the approved config
  // must not silently re-target if the daemon default changes.
  const spec = configState && configState.spec;
  if (spec && spec.backend) {
    const agentConfig = {};
    const model = configState.modelSel ? configState.modelSel.value : '';
    const effort = configState.effortSel ? configState.effortSel.value : '';
    if (model) agentConfig[spec.modelKey] = model;
    if (effort) agentConfig[spec.effortKey] = effort;
    if (Object.keys(agentConfig).length) {
      agentConfig.agent = spec.backend;
      params.agent_config = agentConfig;
    }
  }
  startBtn.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (resp.ok && resp.body && resp.body.item) {
      agendaObserveServerMessage({ item: resp.body.item });
      agendaCloseStartSheet();
      if (typeof showControlToast === 'function') {
        showControlToast('success', params.interactive
          ? 'Session starting — it opens with the item and waits for you.'
          : 'Goal run starting — the outcome writes back to the item.');
      }
      return;
    }
    showError((resp.body && resp.body.error) || `start failed (${resp.status})`);
  } catch (e) {
    showError(String(e && e.message || e));
  } finally {
    startBtn.disabled = false;
  }
}

function agendaActionButtons(item) {
  const actions = [];
  if (item.status === 'open') actions.push(['complete', 'Complete'], ['retire', 'Retire']);
  else if (item.status === 'done') actions.push(['reopen', 'Reopen'], ['retire', 'Retire']);
  else actions.push(['reopen', 'Reopen']);
  let extra = '';
  if (item.status === 'open') {
    // Owner start-now: opens the CONFIRM SHEET (bottom sheet on coarse
    // pointers, popover on desktop) — what will run (goal, project,
    // config, Interactive/Goal-run) is reviewed there; the daemon mints
    // + approves the manifest from the confirmed parameters and fires
    // through the ordinary scheduled lane. The one-click instant fire is
    // retired on dashboard surfaces (owner ruling, 2026-07-21). Owner
    // surface = this dashboard; agents get NotPermitted.
    // NOT on rich parked asks: those await the owner's ANSWER — spawning
    // a follow-through session on one just re-reads the question at the
    // owner (live-QA footgun 2026-07-20). Answering is the primary act;
    // Open question panel is the affordance.
    const richAsk = item.kind === 'question'
      && item.ask && Array.isArray(item.ask.questions) && item.ask.questions.length;
    if (!richAsk) {
      extra += `<button type="button" class="agenda-btn agenda-start-now" data-id="${escapeHtml(item.id)}" title="Review and start a supervised session from this item">Start now</button>`;
    }
    // Follow-up targets the ORIGIN conversation: live window → composer;
    // ended but resumable → the existing resume path. Never a silent
    // unrelated new session.
    const sid = agendaFollowUpSid(item);
    if (sid) {
      extra += `<button type="button" class="agenda-btn agenda-follow-up" data-id="${escapeHtml(item.id)}" data-sid="${escapeHtml(sid)}" title="The recording conversation is live — open the composer targeted at it with this item quoted">Follow up</button>`;
    } else if (agendaFollowUpResumable(item)) {
      extra += `<button type="button" class="agenda-btn agenda-follow-up-resume" data-id="${escapeHtml(item.id)}" title="The recording conversation has ended — resume it (same conversation, its recorded project) and open the composer with this item quoted">Follow up (resumes session)</button>`;
    }
  }
  const buttons = extra + actions
    .map(([op, label]) =>
      `<button type="button" class="agenda-btn" data-op="${op}" data-id="${escapeHtml(item.id)}">${label}</button>`)
    .join('');
  // The per-item reminder loudness control (owner policy) — only where a
  // reminder can still fire.
  if (item.status !== 'open' || !item.due_ms) return buttons;
  const level = agendaItemUrgency(item.id);
  const options = ['default', 'mute', 'info', 'attention', 'urgent']
    .map((value) => {
      const label = value === 'default'
        ? `default (${(agendaReminderPolicy && agendaReminderPolicy.default_urgency) || 'attention'})`
        : value;
      return `<option value="${value}"${value === level ? ' selected' : ''}>${label}</option>`;
    })
    .join('');
  return `<select class="agenda-bell" data-id="${escapeHtml(item.id)}" title="Reminder loudness for this item (owner policy)" aria-label="Reminder loudness">${options}</select>${buttons}`;
}

function agendaItemUrgency(id) {
  const overrides = (agendaReminderPolicy && agendaReminderPolicy.item_urgency) || {};
  return overrides[id] || 'default';
}

async function agendaSetItemUrgency(id, value, control) {
  const patch = { item_urgency: { [id]: value === 'default' ? null : value } };
  await agendaSendPolicyPatch(patch, control);
}

async function agendaSendPolicyPatch(patch, control) {
  if (control) control.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_reminder_policy', patch);
    if (resp.ok && resp.body && resp.body.reminder_policy) {
      agendaReminderPolicy = resp.body.reminder_policy;
      agendaRenderAll();
      return true;
    }
    agendaFlashError((resp.body && resp.body.error) || `policy update failed (${resp.status})`);
    agendaRenderAll(); // restore the control to the effective policy
    return false;
  } catch (e) {
    agendaFlashError(String(e && e.message || e));
    return false;
  } finally {
    if (control) control.disabled = false;
  }
}

function agendaRenderReminderBar() {
  const bar = document.getElementById('agenda-reminders-bar');
  if (!bar) return;
  const policy = agendaReminderPolicy;
  if (!policy) {
    bar.innerHTML = '';
    return;
  }
  const quiet = policy.quiet_hours;
  const minToHhmm = (min) =>
    `${String(Math.floor(min / 60)).padStart(2, '0')}:${String(min % 60).padStart(2, '0')}`;
  const quietLabel = quiet
    ? `${minToHhmm(quiet.start_min)}–${minToHhmm(quiet.end_min)}`
    : 'off';
  bar.innerHTML = `
    <span class="agenda-rem-label">Reminders</span>
    <button type="button" class="agenda-btn" id="agenda-rem-toggle">${policy.enabled ? 'on' : 'off'}</button>
    <span class="agenda-rem-label">quiet hours</span>
    <button type="button" class="agenda-btn" id="agenda-rem-quiet" title="Deliveries inside the window wait for its end">${escapeHtml(quietLabel)}</button>
    <span class="agenda-rem-label">default</span>
    <select id="agenda-rem-default" aria-label="Default reminder urgency">
      ${['info', 'attention', 'urgent'].map((value) =>
        `<option value="${value}"${value === policy.default_urgency ? ' selected' : ''}>${value}</option>`).join('')}
    </select>`;
  const toggle = document.getElementById('agenda-rem-toggle');
  if (toggle) toggle.addEventListener('click', () =>
    agendaSendPolicyPatch({ enabled: !policy.enabled }, toggle));
  const dflt = document.getElementById('agenda-rem-default');
  if (dflt) dflt.addEventListener('change', () =>
    agendaSendPolicyPatch({ default_urgency: dflt.value }, dflt));
  const quietBtn = document.getElementById('agenda-rem-quiet');
  if (quietBtn) quietBtn.addEventListener('click', () => {
    const current = quiet ? `${minToHhmm(quiet.start_min)}-${minToHhmm(quiet.end_min)}` : '';
    const raw = prompt('Quiet hours (HH:MM-HH:MM local; may cross midnight; empty = off)', current);
    if (raw === null) return;
    const trimmed = raw.trim();
    if (!trimmed) {
      agendaSendPolicyPatch({ quiet_hours: null }, quietBtn);
      return;
    }
    const m = trimmed.match(/^(\d{1,2}):(\d{2})\s*-\s*(\d{1,2}):(\d{2})$/);
    if (!m) {
      agendaFlashError('quiet hours must look like 22:00-08:00');
      return;
    }
    const start = Number(m[1]) * 60 + Number(m[2]);
    const end = Number(m[3]) * 60 + Number(m[4]);
    if (start > 23 * 60 + 59 || end > 23 * 60 + 59) {
      agendaFlashError('quiet hours must use 00:00–23:59');
      return;
    }
    agendaSendPolicyPatch({ quiet_hours: { start_min: start, end_min: end } }, quietBtn);
  });
}

function agendaRenderAll() {
  agendaRenderTab();
  agendaRenderCard();
  agendaRenderReminderBar();
}

function agendaRenderTab() {
  const list = document.getElementById('agenda-tab-list');
  if (!list) return;
  const counts = document.getElementById('agenda-tab-counts');
  if (counts) {
    counts.textContent =
      `${agendaCounts.open || 0} open · ${agendaCounts.done || 0} done · ${agendaCounts.retired || 0} retired`;
  }
  const note = document.getElementById('agenda-tab-skipped');
  if (note && !note.textContent) {
    if (agendaSkippedLines > 0) {
      note.style.display = '';
      note.textContent =
        `${agendaSkippedLines} history line(s) from another build are preserved but not shown.`;
    } else {
      note.style.display = 'none';
    }
  }
  if (agendaLoadError) {
    list.innerHTML = `<div class="ui-empty">${escapeHtml(agendaLoadError)}</div>`;
    return;
  }
  if (agendaItems === null) {
    list.innerHTML = '<div class="ui-empty">Loading…</div>';
    return;
  }
  // Questions is a KIND view, not a status: the open questions (rich
  // parked asks lead with their panel button) plus the answered archive.
  // Retired questions stay reachable under Retired / All.
  const filtered = agendaItems.filter((item) =>
    agendaFilter === 'all' ? true
      : agendaFilter === 'blocked' ? agendaItemIsBlocked(item)
        : agendaFilter === 'questions'
          ? item.kind === 'question' && item.status !== 'retired'
          : item.status === agendaFilter);
  if (!filtered.length) {
    list.innerHTML = agendaFilter === 'questions'
      ? '<div class="ui-empty">No open or answered questions — park one with <code>intendant ctl ask --park</code>.</div>'
      : `<div class="ui-empty">No ${agendaFilter === 'all' ? '' : `${agendaFilter} `}items — park one above, or run <code>intendant ctl agenda add</code>.</div>`;
    return;
  }
  // Newest first reads best in a review list; ULIDs sort by creation.
  const rows = filtered.slice().sort((a, b) => (a.id < b.id ? 1 : -1)).map((item) => {
    const tags = (item.tags || [])
      .map((tag) => `<span class="agenda-chip">#${escapeHtml(tag)}</span>`)
      .join('');
    const body = item.body
      ? `<div class="agenda-item-body">${escapeHtml(item.body)}</div>`
      : '';
    // The ask-seam reply affordance: open questions take a durable reply
    // right here; answered ones show it (data, rendered escaped). Rich
    // (ask-backed) questions lead with the panel — options and previews
    // live there; the inline input stays as an explicit plain-text path,
    // never the only door.
    let answerBlock = '';
    const openRichAsk = item.kind === 'question' && item.status === 'open'
      && item.ask && Array.isArray(item.ask.questions) && item.ask.questions.length;
    if (item.kind === 'question' && item.status === 'open') {
      const richAsk = openRichAsk;
      const openBtn = richAsk
        ? `<div class="agenda-answer-row agenda-ask-open-row">
            <button type="button" class="agenda-btn agenda-open-ask-btn" data-id="${escapeHtml(item.id)}">Open question panel</button>
            <span class="agenda-ask-hint">${item.ask.questions.length > 1
    ? `${item.ask.questions.length} structured questions`
    : 'structured question'} with options${item.ask.questions.some((q) => (q.previews || []).length) ? ' + previews' : ''}</span>
          </div>`
        : '';
      answerBlock = `${openBtn}<div class="agenda-answer-row">
        <input type="text" class="agenda-answer-input" maxlength="4000"
               placeholder="${richAsk ? 'Or type a plain-text answer…' : 'Answer this question…'}" aria-label="Answer" data-id="${escapeHtml(item.id)}" />
        <button type="button" class="agenda-btn agenda-answer-btn" data-id="${escapeHtml(item.id)}">Answer</button>
      </div>`;
    } else if (item.answer && (item.answer.text || item.answer.structured)) {
      // Answered: the archive keeps the full structured breakdown for
      // rich asks, the joined text otherwise (agendaAnswerBlock).
      answerBlock = agendaAnswerBlock(item);
    }
    const blockedChip = agendaItemIsBlocked(item)
      ? '<span class="agenda-chip blocked">blocked</span>'
      : '';
    // Dismissed-but-open questions wear a quiet marker: the rails were
    // cleared deliberately and stay cleared (no auto re-announce); the
    // item itself remains open and answerable right here.
    const dismissedChip = item.status === 'open' && item.dismissed
      ? `<span class="agenda-chip dismissed" title="${escapeHtml(agendaDismissedTip(item.dismissed))}">dismissed · still open</span>`
      : '';
    // Answered ask whose delivery reached no session (the daemon recorded
    // `answer.delivered: false`): a quiet marker that the reply sits here
    // unheard — the next session's agenda check is the pickup path.
    // Pre-marker history (no `delivered` field) claims nothing.
    const pickupChip = item.status === 'done' && item.ask && item.answer
      && item.answer.delivered === false
      ? '<span class="agenda-chip pickup" title="The answer was recorded, but the asking session was gone and no successor was live. The next session’s agenda check picks it up.">answered · awaiting pickup</span>'
      : '';
    // Open rich asks: the whole head is the affordance — clicking the
    // question opens its panel (the obvious gesture; the explicit button
    // below remains for discoverability). role/tabindex make it a real
    // control for keyboard and assistive tech.
    const headOpenAttrs = openRichAsk
      ? ` agenda-item-head-openable" data-open-ask="${escapeHtml(item.id)}" role="button" tabindex="0" title="Open the question panel`
      : '';
    return `<div class="agenda-item" data-status="${escapeHtml(item.status)}">
      <div class="agenda-item-head${headOpenAttrs}">
        ${agendaGlyph(item.status, item.kind)}
        <span class="agenda-item-kind">${escapeHtml(item.kind)}</span>
        <span class="agenda-item-title">${escapeHtml(item.title)}</span>
        ${blockedChip}${dismissedChip}${pickupChip}${agendaDueChip(item)}${tags}
      </div>
      ${body}${answerBlock}${agendaThreadBlock(item)}${agendaEffectBlock(item)}
      <div class="agenda-item-foot">
        <span class="agenda-item-meta">${agendaProvenanceLine(item)}</span>
        <span class="agenda-item-actions">${agendaActionButtons(item)}</span>
      </div>
    </div>`;
  });
  list.innerHTML = rows.join('');
  list.querySelectorAll('a.agenda-session-link').forEach((link) => {
    link.addEventListener('click', (e) => {
      e.preventDefault();
      agendaJumpToSession(link.dataset.sessionKey);
    });
  });
  list.querySelectorAll('.agenda-open-ask-btn').forEach((btn) => {
    btn.addEventListener('click', () => agendaOpenParkedAsk(btn.dataset.id));
  });
  list.querySelectorAll('.agenda-item-head-openable').forEach((head) => {
    const open = (e) => {
      // Chips/links inside the head keep their own behavior.
      if (e.target.closest('button, a, input, select')) return;
      e.preventDefault();
      agendaOpenParkedAsk(head.dataset.openAsk);
    };
    head.addEventListener('click', open);
    head.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') open(e);
    });
  });
  list.querySelectorAll('button[data-op]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const params = { op: btn.dataset.op, id: btn.dataset.id };
      // Approve binds the digest of the revision this render showed.
      if (btn.dataset.digest) params.digest = btn.dataset.digest;
      agendaSendOp(params, btn);
    });
  });
  list.querySelectorAll('select.agenda-bell').forEach((sel) => {
    sel.addEventListener('change', () =>
      agendaSetItemUrgency(sel.dataset.id, sel.value, sel));
  });
  // F3 act-on-item wiring: Start now opens the confirm sheet (never an
  // instant fire); follow-up routes to the origin conversation.
  list.querySelectorAll('button.agenda-start-now').forEach((btn) => {
    btn.addEventListener('click', () => agendaOpenStartSheet(btn.dataset.id, btn));
  });
  list.querySelectorAll('button.agenda-follow-up').forEach((btn) => {
    btn.addEventListener('click', () => {
      const item = agendaFindItem(btn.dataset.id);
      if (item) agendaFollowUpWithRecorder(item, btn.dataset.sid);
    });
  });
  list.querySelectorAll('button.agenda-follow-up-resume').forEach((btn) => {
    btn.addEventListener('click', () => {
      const item = agendaFindItem(btn.dataset.id);
      if (item) agendaFollowUpResume(item);
    });
  });
  // F2 thread + gates wiring.
  list.querySelectorAll('button.agenda-thread-more').forEach((btn) => {
    btn.addEventListener('click', () => {
      agendaExpandedThreads.add(btn.dataset.id);
      agendaRenderTab();
    });
  });
  list.querySelectorAll('button.agenda-clear-blocker').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: 'clear_blocker', id: btn.dataset.id, blocker_id: btn.dataset.blocker }, btn));
  });
  list.querySelectorAll('button.agenda-edge-remove').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: 'remove_relies_on', id: btn.dataset.id, target_id: btn.dataset.target }, btn));
  });
  const submitThread = async (id, input, op, control) => {
    const text = (input.value || '').trim();
    if (!text) return;
    input.disabled = true;
    const params = op === 'set_blocker'
      ? { op, id, criterion: text }
      : { op, id, text };
    const ok = await agendaSendOp(params, control);
    input.disabled = false;
    if (!ok) input.focus();
  };
  list.querySelectorAll('button.agenda-thread-note').forEach((btn) => {
    const input = list.querySelector(`input.agenda-thread-input[data-id="${btn.dataset.id}"]`);
    if (!input) return;
    btn.addEventListener('click', () => submitThread(btn.dataset.id, input, 'annotate', btn));
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submitThread(btn.dataset.id, input, 'annotate', btn);
      }
    });
  });
  list.querySelectorAll('button.agenda-thread-block').forEach((btn) => {
    const input = list.querySelector(`input.agenda-thread-input[data-id="${btn.dataset.id}"]`);
    if (!input) return;
    btn.addEventListener('click', () => submitThread(btn.dataset.id, input, 'set_blocker', btn));
  });
  const submitAnswer = async (id, input, control) => {
    const text = (input.value || '').trim();
    if (!text) return;
    input.disabled = true;
    const ok = await agendaSendOp({ op: 'answer', id, text }, control);
    input.disabled = false;
    if (!ok) input.focus();
  };
  list.querySelectorAll('button.agenda-answer-btn').forEach((btn) => {
    const input = list.querySelector(`input.agenda-answer-input[data-id="${btn.dataset.id}"]`);
    if (!input) return;
    btn.addEventListener('click', () => submitAnswer(btn.dataset.id, input, btn));
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submitAnswer(btn.dataset.id, input, btn);
      }
    });
  });
}

// ---- Compact card on the activity pane (stacked under the vitals rail).

function agendaBuildCard() {
  const pane = document.getElementById('activity-log-pane');
  if (!pane || document.getElementById('ui2-agenda-card')) return;
  const card = document.createElement('aside');
  card.id = 'ui2-agenda-card';
  card.setAttribute('aria-label', 'Agenda');
  card.innerHTML = `
    <div class="agenda-card-head">
      <span class="agenda-card-title">Agenda</span>
      <button type="button" class="agenda-card-open" id="agenda-card-open">open</button>
    </div>
    <div class="agenda-card-list" id="agenda-card-list"><div class="agenda-card-empty">…</div></div>
    <form class="agenda-card-add" id="agenda-card-add">
      <input type="text" id="agenda-card-input" maxlength="500" placeholder="Park a task…" aria-label="Park a task" />
    </form>`;
  pane.appendChild(card);
  const open = card.querySelector('#agenda-card-open');
  if (open) open.addEventListener('click', () => routeTo('agenda'));
  const form = card.querySelector('#agenda-card-add');
  const input = card.querySelector('#agenda-card-input');
  if (form && input) {
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const title = input.value.trim();
      if (!title) return;
      input.disabled = true;
      const ok = await agendaSendOp({ op: 'add', kind: 'task', title });
      input.disabled = false;
      if (ok) input.value = '';
      input.focus();
    });
  }
}

function agendaRenderCard() {
  const list = document.getElementById('agenda-card-list');
  if (!list) return;
  const title = document.querySelector('#ui2-agenda-card .agenda-card-title');
  if (title) {
    const open = agendaCounts.open || 0;
    title.textContent = open > 0 ? `Agenda · ${open} open` : 'Agenda';
  }
  if (agendaItems === null) {
    list.innerHTML = `<div class="agenda-card-empty">${agendaLoadError ? escapeHtml(agendaLoadError) : '…'}</div>`;
    return;
  }
  const open = agendaItems.filter((item) => item.status === 'open');
  if (!open.length) {
    list.innerHTML = '<div class="agenda-card-empty">Nothing parked.</div>';
    return;
  }
  // Oldest first: long-parked intent stays visible instead of scrolling away.
  const rows = open.slice(0, 5).map((item) => {
    const p = item.provenance || {};
    // Agent-parked items carry their session provenance right on the card,
    // by resolved name when the join map has one (raw id in the tooltip).
    const s = agendaSessionInfo(p.session_id);
    const who = p.session_id
      ? `<span class="agenda-card-row-who" title="${escapeHtml(p.session_id)}">· ${escapeHtml(s && s.name ? s.name : `sess ${p.session_id.slice(0, 8)}`)}</span>`
      : (p.source
        ? `<span class="agenda-card-row-who" title="self-described label — unverified">· ${escapeHtml(p.source)}</span>`
        : '');
    const q = item.kind === 'question'
      ? '<span class="agenda-card-q" aria-label="question">?</span>'
      : '';
    return `<div class="agenda-card-row" data-id="${escapeHtml(item.id)}">
      <button type="button" class="agenda-card-done" data-id="${escapeHtml(item.id)}" aria-label="Complete">○</button>
      ${q}<span class="agenda-card-row-title" title="${escapeHtml(item.title)}">${escapeHtml(item.title)}</span>${who}
    </div>`;
  });
  const more = open.length > 5
    ? `<div class="agenda-card-more">+${open.length - 5} more…</div>`
    : '';
  list.innerHTML = rows.join('') + more;
  list.querySelectorAll('.agenda-card-done').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: 'complete', id: btn.dataset.id }, btn));
  });
  list.querySelectorAll('.agenda-card-row-title').forEach((el) => {
    el.addEventListener('click', () => routeTo('agenda'));
  });
  const moreEl = list.querySelector('.agenda-card-more');
  if (moreEl) moreEl.addEventListener('click', () => routeTo('agenda'));
}

// The vitals rail owns the top-right column; stack the card just under
// its live height (both hide together below 1180px / in grid layout).
// Write-guarded: the 1 Hz reposition mostly re-derives the same state, and
// re-stamping data-rail-hidden / style.top with unchanged values fed a
// style-invalidation pass per second (the `:has()` before-mutation walk)
// for nothing.
function agendaPositionCard() {
  const card = document.getElementById('ui2-agenda-card');
  const rail = document.getElementById('ui2-vitals-rail');
  if (!card) return;
  if (!rail || !rail.offsetParent) {
    if (card.dataset.railHidden !== '1') card.dataset.railHidden = '1';
    return;
  }
  if ('railHidden' in card.dataset) delete card.dataset.railHidden;
  const top = `${rail.offsetTop + rail.offsetHeight + 12}px`;
  if (card.style.top !== top) card.style.top = top;
}

{
  const wire = () => {
    const filters = document.getElementById('agenda-filters');
    if (filters) {
      filters.querySelectorAll('.agenda-filter').forEach((btn) => {
        btn.addEventListener('click', () => {
          agendaFilter = btn.dataset.filter || 'open';
          filters.querySelectorAll('.agenda-filter').forEach((b) =>
            b.classList.toggle('active', b === btn));
          agendaRenderTab();
        });
      });
    }
    const addBtn = document.getElementById('agenda-add-btn');
    const addTitle = document.getElementById('agenda-add-title');
    const addKind = document.getElementById('agenda-add-kind');
    const submitAdd = async () => {
      const title = (addTitle && addTitle.value || '').trim();
      if (!title) return;
      const picked = (addKind && addKind.value) || 'task';
      const kind = ['task', 'note', 'question'].includes(picked) ? picked : 'task';
      const ok = await agendaSendOp({ op: 'add', kind, title }, addBtn);
      if (ok && addTitle) {
        addTitle.value = '';
        addTitle.focus();
      }
    };
    if (addBtn) addBtn.addEventListener('click', submitAdd);
    if (addTitle) {
      addTitle.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          submitAdd();
        }
      });
    }
    agendaBuildCard();
    agendaRefresh();
    // Follow-up affordance liveness: the button is derived at render time
    // from session-window state the agenda has no event lane for, so a
    // visible tab re-renders when (and only when) the eligibility
    // signature changes — the target-switch poll idiom, write-guarded.
    let followUpSig = '';
    setInterval(() => {
      if (!agendaTabVisible() || !Array.isArray(agendaItems)) return;
      const sig = agendaItems
        .filter((item) => item.status === 'open')
        .map((item) => `${item.id}:${agendaFollowUpSid(item) || ''}`)
        .join('|');
      if (sig !== followUpSig) {
        followUpSig = sig;
        agendaRenderTab();
      }
    }, 2000);
    // Pane-gated: the card lives in #activity-log-pane, so with the
    // Activity tab parked (another tab, or document.hidden) the reposition
    // tick used to write data-rail-hidden into a display:none subtree once
    // per second. renderOrDefer keeps only the latest reposition thunk
    // while parked; flushPaneRenders runs it on pane re-entry.
    setInterval(() => renderOrDefer('activity', 'ui2-agenda-card', agendaPositionCard), 1000);
    agendaPositionCard();
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}
