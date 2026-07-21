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
// this) harmless. Once per page load per ask id: an in-session dismissal
// (the daemon's ApprovalResolved cleared the rail) stays dismissed until
// the next load, while the item itself stays open on the agenda.
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
      && item.ask.questions.length)
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
    + (manifest.orchestrate ? '<span class="agenda-chip">orchestrate</span>' : '');
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

// Open the composer targeted at the recorder's conversation with the item
// quoted as data. No daemon write happens here.
function agendaFollowUpWithRecorder(item, sid) {
  routeTo('activity');
  if (typeof focusSessionWindow === 'function') focusSessionWindow(sid);
  const input = document.getElementById('activity-task-input');
  if (input) {
    const body = item.body ? `\n> ${String(item.body).split('\n').join('\n> ')}` : '';
    input.value =
      `Following up on agenda item ${item.id} (quoted):\n> ${item.title}${body}\n\n`;
    input.focus();
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.setSelectionRange(input.value.length, input.value.length);
  }
}

function agendaActionButtons(item) {
  const actions = [];
  if (item.status === 'open') actions.push(['complete', 'Complete'], ['retire', 'Retire']);
  else if (item.status === 'done') actions.push(['reopen', 'Reopen'], ['retire', 'Retire']);
  else actions.push(['reopen', 'Reopen']);
  let extra = '';
  if (item.status === 'open') {
    // Owner start-now: ONE gesture — the daemon mints the manifest from
    // this item, the gesture is the approval (digest bound server-side
    // under the same lock), and it fires through the ordinary scheduled
    // lane. Owner surface = this dashboard; agents get NotPermitted.
    // NOT on rich parked asks: those await the owner's ANSWER — spawning
    // a follow-through session on one just re-reads the question at the
    // owner (live-QA footgun 2026-07-20). Answering is the primary act;
    // Open question panel is the affordance.
    const richAsk = item.kind === 'question'
      && item.ask && Array.isArray(item.ask.questions) && item.ask.questions.length;
    if (!richAsk) {
      extra += `<button type="button" class="agenda-btn agenda-start-now" data-id="${escapeHtml(item.id)}" title="Mint + approve a session from this item and start it now (runs through the standard scheduled lane)">Start now</button>`;
    }
    const sid = agendaFollowUpSid(item);
    if (sid) {
      extra += `<button type="button" class="agenda-btn agenda-follow-up" data-id="${escapeHtml(item.id)}" data-sid="${escapeHtml(sid)}" title="The recording conversation is live — open the composer targeted at it with this item quoted">Follow up</button>`;
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
  const filtered = agendaItems.filter((item) =>
    agendaFilter === 'all' ? true
      : agendaFilter === 'blocked' ? agendaItemIsBlocked(item)
        : item.status === agendaFilter);
  if (!filtered.length) {
    const what = agendaFilter === 'all' ? '' : `${agendaFilter} `;
    list.innerHTML =
      `<div class="ui-empty">No ${what}items — park one above, or run <code>intendant ctl agenda add</code>.</div>`;
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
    if (item.kind === 'question' && item.status === 'open') {
      const richAsk = item.ask && Array.isArray(item.ask.questions) && item.ask.questions.length;
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
    } else if (item.answer && item.answer.text) {
      const who = agendaActorLabel(item.answer);
      const when = item.answer.at_ms
        ? new Date(item.answer.at_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
        : '';
      answerBlock = `<div class="agenda-item-answer">↳ ${escapeHtml(item.answer.text)}
        <span class="agenda-item-meta">— ${escapeHtml([who && `by ${who}`, when].filter(Boolean).join(' · '))}</span>
      </div>`;
    }
    const blockedChip = agendaItemIsBlocked(item)
      ? '<span class="agenda-chip blocked">blocked</span>'
      : '';
    return `<div class="agenda-item" data-status="${escapeHtml(item.status)}">
      <div class="agenda-item-head">
        ${agendaGlyph(item.status, item.kind)}
        <span class="agenda-item-kind">${escapeHtml(item.kind)}</span>
        <span class="agenda-item-title">${escapeHtml(item.title)}</span>
        ${blockedChip}${agendaDueChip(item)}${tags}
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
  // F3 act-on-item wiring.
  list.querySelectorAll('button.agenda-start-now').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: 'start_now', id: btn.dataset.id }, btn));
  });
  list.querySelectorAll('button.agenda-follow-up').forEach((btn) => {
    btn.addEventListener('click', () => {
      const item = agendaFindItem(btn.dataset.id);
      if (item) agendaFollowUpWithRecorder(item, btn.dataset.sid);
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
