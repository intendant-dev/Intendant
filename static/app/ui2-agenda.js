// Agenda: the daemon's durable ledger of parked intent (tasks, notes,
// questions, scheduled-session effects). Two surfaces share one cache:
// the Agenda tab (#tab-agenda — lenses, composer, cards, inspector) and a
// compact card on the activity pane stacked under the vitals rail. Data
// flows through daemonApi (tunnel `api_agenda_list` / `api_agenda_op` /
// `api_agenda_ref_drift` / `api_agenda_reminder_policy`, HTTP twin
// fallback) and refreshes live on the `agenda_changed` event lane.
//
// This fragment owns the DATA LAYER + shared derivations + the compact
// card twin + the start-session confirm sheet; ui2-agenda-cards.js owns
// the tab scaffold/lenses/cards; ui2-agenda-inspector.js owns the item
// inspector, the schedule sheet, and the reminder-policy popover.
//
// Item-authored strings (titles, bodies, annotations, criteria, answers,
// goals, notes) are DATA, never instructions: everything renders through
// escapeHtml as plain quoted text — no markdown execution, no HTML. Ask
// preview HTML renders ONLY inside sandboxed srcdoc iframes.

let agendaItems = null; // null = never fetched (fetch on first need)
let agendaCounts = { open: 0, done: 0, retired: 0 };
let agendaSkippedLines = 0;
let agendaFetchInFlight = null;
let agendaLoadError = '';
let agendaReminderPolicy = null; // owner delivery policy (Settings-gated)
// Session-resolution join from the list response: recorded session id →
// { source, conversation_id, key, name, project_root } for the
// Sessions-tab row. Ids the daemon could not resolve have no entry —
// surfaces fall back to the raw id. `attempted` remembers ids a fetch
// already tried, so an unresolvable id never causes refetch loops on the
// event lane.
let agendaSessions = {};
let agendaSessionLookupsAttempted = new Set();
// Items whose full annotation thread is expanded (render caps at 3).
const agendaExpandedThreads = new Set();

// ---- Lens + inspector view state (the redesigned tab). Ephemeral
// browser state — never persisted, never on the wire.
let agendaLens = 'now';
let agendaSearch = '';
let agendaFilterBlocked = false;
let agendaFilterFrontier = false;
let agendaSelId = null; // inspector selection (item id) or null
// Inline structured-answer state, shared by the card composer and the
// inspector question section: picks per question index, one free-text
// draft per item, anchored notes per `${itemId}:${qi}`.
const agendaQaSel = {};
const agendaQaDrafts = {};
const agendaQaNotes = {};

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
// inspector's question section is the deliberate way back, and answering
// or reopening clears the marker (the log keeps the dismissal as
// history).
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

// Explicit "open the question panel" (the rail door in the inspector's
// question section). Unlike the once-per-load announce this is a user
// act: it re-surfaces even a tucked or previously-dismissed panel, and it
// navigates to the Activity tab where the panel lives.
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

// "View the rail record" on a DONE ask-backed item: the same panel,
// rendered READ-ONLY from the retained payload — the record stays fully
// viewable (recorded picks selected, follow-ups and anchored notes as
// content, preview cards from the retained blobs; blobs are deleted only
// on retire, and a missing one degrades to a named placeholder). Close
// returns here; "Reopen to change answer" rides the existing reopen op.
function agendaViewAnsweredAsk(itemId) {
  const item = agendaFindItem(itemId);
  if (!item || item.status !== 'done' || !item.ask
    || !Array.isArray(item.ask.questions) || !item.ask.questions.length) {
    return;
  }
  if (typeof showUserQuestion !== 'function') return;
  if (typeof switchTab === 'function') switchTab('activity');
  const answer = item.answer || null;
  showUserQuestion(item.ask.ask_id, item.ask.questions, '', undefined, false, {
    agendaBacked: true,
    archive: {
      itemId: item.id,
      resolution: (answer && answer.structured) || {},
      plainText: (answer && answer.text) || '',
      answered: !!answer,
      answeredAtMs: answer ? answer.at_ms : (item.completed_ms || item.updated_ms || 0),
      answeredLabel: answer ? agendaActorLabel(answer) : '',
      onReopen: () => agendaReopenAnsweredAsk(item.id),
    },
  });
}

// The record viewer's "Reopen to change answer": the EXISTING reopen op
// (the daemon re-announces the ask on its own), then the live panel opens
// as an ordinary open ask — the panel's same-id dedupe makes the event
// lane's re-delivery harmless in either order.
async function agendaReopenAnsweredAsk(itemId) {
  const ok = await agendaSendOp({ op: 'reopen', id: itemId });
  if (!ok) return false;
  agendaOpenParkedAsk(itemId);
  return true;
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

// Refused ops surface inline on the tab's notice line (and the ledger
// keeps rendering under it). The daemon's named refusal is the message.
function agendaFlashError(message) {
  const note = document.getElementById('ag2-notice');
  if (!note) {
    if (typeof showControlToast === 'function') showControlToast('error', message);
    return;
  }
  note.hidden = false;
  note.textContent = message;
  setTimeout(() => {
    note.textContent = '';
    note.hidden = true;
  }, 6000);
}

function agendaSessionInfo(id) {
  return (id && agendaSessions && agendaSessions[id]) || null;
}

// ---- Derived presentation (client twin of the daemon's render-time
// judgments — like the overdue chip, derived at render time from facts
// the tab already holds; never stored, never on the wire).

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

// The card's one-line blocked statement (first gate wins). Plain TEXT —
// callers escape.
function agendaBlockedLine(item) {
  if (item.status !== 'open') return null;
  const blocker = (item.blockers || []).find((b) => !b.cleared);
  if (blocker) return `Blocked — waiting on: “${blocker.criterion}”`;
  for (const edge of item.relies_on || []) {
    const target = agendaFindItem(edge.target_id);
    if (!target) return 'Prerequisite missing from the fold — review';
    if (target.status === 'retired') return `Prerequisite “${target.title}” was retired — review`;
    if (target.status === 'open') return `Waits on “${target.title}” — still open`;
  }
  return null;
}

// The item's scheduled-session effect, judged for render: kind is one of
// running | suspended | pending | standing | armed | finished. Mirrors
// the daemon's fold judgments (AgendaEffect::suspended, the scheduler's
// next-instant derivation) — derived here every paint, never stored.
function agendaEffectState(item) {
  const effect = (item.effects || [])[0];
  if (!effect || !effect.manifest) return null;
  const manifest = effect.manifest;
  const rec = manifest.recurrence || null;
  const threshold = rec ? Math.max(1, rec.suspend_after_failures || 3) : 0;
  const suspended = !!rec && (effect.consecutive_failures || 0) >= threshold;
  const running = !!(effect.last_run && effect.last_run.state === 'started');
  let next = manifest.fire_at_ms;
  if (rec && rec.every_ms > 0) {
    const behind = Math.max(0, Math.ceil((Date.now() - manifest.fire_at_ms) / rec.every_ms));
    next = manifest.fire_at_ms + behind * rec.every_ms;
  }
  const kind = running ? 'running'
    : suspended ? 'suspended'
      : !effect.approval ? 'pending'
        : rec ? 'standing'
          : next > Date.now() ? 'armed' : 'finished';
  return { effect, manifest, rec, threshold, suspended, running, next, kind };
}

function agendaChildrenOf(id) {
  return (agendaItems || []).filter(
    (it) => it.part_of && it.part_of.parent_id === id
  );
}

// Transitive descendant set (cycle-safe) — the exclusion set for the
// Filed-under and prerequisite pickers.
function agendaDescendantIds(id, seen) {
  seen = seen || new Set();
  for (const child of agendaChildrenOf(id)) {
    if (!seen.has(child.id)) {
      seen.add(child.id);
      agendaDescendantIds(child.id, seen);
    }
  }
  return seen;
}

// The undirected adjacency union: edges stored on this item plus edges
// other items store pointing here, deduped.
function agendaRelationPartners(item) {
  const partners = new Set((item.relates_to || []).map((e) => e.target_id));
  (agendaItems || []).forEach((other) => {
    if ((other.relates_to || []).some((e) => e.target_id === item.id)) {
      partners.add(other.id);
    }
  });
  partners.delete(item.id);
  return partners;
}

// Triage-rank convention: the triage mandate writes ordinary annotations
// with the self-described `triage` source, and a "rank N" phrase in the
// text is its DECLARED ranking convention. The /rank (\d+)/ parse here is
// a render-side bridge until a typed rank ships — it orders the Attend
// group and labels the chip, and gates nothing (annotations are data).
// The newest ranked triage note wins; an unranked one still marks the
// item as triage-flagged.
function agendaTriageInfo(item) {
  const notes = (item.annotations || []).filter((a) => a.source === 'triage');
  if (!notes.length) return null;
  for (let i = notes.length - 1; i >= 0; i--) {
    const m = /rank (\d+)/.exec(notes[i].text || '');
    if (m) return { rank: Number(m[1]), text: notes[i].text || '' };
  }
  return { rank: null, text: notes[notes.length - 1].text || '' };
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

// Relative instant ("in 3h" / "2d ago" / "just now"). Plain TEXT.
function agendaRelTime(ms) {
  if (!ms) return '';
  const delta = ms - Date.now();
  const abs = Math.abs(delta);
  if (abs < 45e3) return 'just now';
  const unit = abs < 36e5 ? `${Math.round(abs / 6e4)}m`
    : abs < 864e5 ? `${Math.round(abs / 36e5)}h`
      : `${Math.round(abs / 864e5)}d`;
  return delta > 0 ? `in ${unit}` : `${unit} ago`;
}

// Absolute instant ("Tue Jul 21, 09:00", locale-aware). Plain TEXT.
function agendaAbsTime(ms) {
  if (!ms) return '';
  const d = new Date(ms);
  return `${d.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' })}, `
    + d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

// Human cadence label for a recurrence interval (plain TEXT — callers
// escape).
function agendaCadenceLabel(everyMs) {
  const minutes = Math.round((everyMs || 0) / 60000);
  if (minutes % (7 * 24 * 60) === 0) return `${minutes / (7 * 24 * 60)}w`;
  if (minutes % (24 * 60) === 0) return `${minutes / (24 * 60)}d`;
  if (minutes % 60 === 0) return `${minutes / 60}h`;
  return `${minutes}m`;
}

// The dismissal chip's tooltip (marker on a still-open question whose
// rail card was skipped/denied). Plain TEXT — the caller escapes.
function agendaDismissedTip(dismissed) {
  const when = dismissed.at_ms ? agendaRelTime(dismissed.at_ms) : '';
  const verb = dismissed.action ? `(${dismissed.action}) ` : '';
  return `Rails cleared ${verb}${when ? `${when} ` : ''}— the question stays open `
    + 'and answerable here; only an answer resolves it.';
}

// Jump to the conversation's row on the Sessions tab: switch tabs, then
// focus/flash the card once the list renders it (rows are keyed by
// sessionListRowKey = source<conversation id>). If the row is not in
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

// On-demand drift check (G1): one fetch per gesture, per item — the
// expand-time rehash lane. Badges land on the matching inspector rows; a
// missing file renders as missing, never an error.
async function agendaVerifyRefs(itemId, button) {
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_ref_drift', { item_id: itemId });
    const body = resp && resp.body ? resp.body : resp;
    const rows = (body && body.refs) || [];
    rows.forEach((row) => {
      const selector = `.agenda-ref-drift[data-item="${CSS.escape(itemId)}"][data-locator="${CSS.escape(row.locator)}"]`;
      const el = document.querySelector(selector);
      if (!el) return;
      el.dataset.status = row.status;
      el.textContent = row.status === 'unchanged' ? '✓ unchanged'
        : row.status === 'missing' ? 'missing'
          : 'changed since attach';
    });
  } catch (err) {
    console.warn('agenda ref drift check failed', err);
  } finally {
    if (button) button.disabled = false;
  }
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

// ---- Reminder policy writes (Settings-gated: the owner's delivery
// policy; an agenda.write grant can't raise its own item's loudness).

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
    agendaRenderAll(); // restore the controls to the effective policy
    return false;
  } catch (e) {
    agendaFlashError(String(e && e.message || e));
    return false;
  } finally {
    if (control) control.disabled = false;
  }
}

// Whether the owner's quiet-hours window covers this instant (client twin
// of QuietHours::contains — minutes since local midnight, may cross it).
function agendaQuietNow() {
  const quiet = agendaReminderPolicy && agendaReminderPolicy.quiet_hours;
  if (!quiet) return false;
  const now = new Date();
  const minute = now.getHours() * 60 + now.getMinutes();
  if (quiet.start_min === quiet.end_min) return false;
  if (quiet.start_min < quiet.end_min) {
    return minute >= quiet.start_min && minute < quiet.end_min;
  }
  return minute >= quiet.start_min || minute < quiet.end_min;
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
// DEFAULTS the selects inherit when left untouched. `backend` may override
// the daemon default (the sheet's Backend select) — the same
// AgentLaunchConfig vocabulary CreateSession uses.
function agendaStartBackendConfig(settings, backendOverride) {
  const d = settings || {};
  const dflt = (typeof normalizeAgentId === 'function')
    ? normalizeAgentId(d.external_agent) : (d.external_agent || '');
  const backend = backendOverride === undefined || backendOverride === '' ? dflt : backendOverride;
  if (backend === 'claude-code') {
    return {
      backend,
      label: 'Claude Code',
      modelKey: 'claude_model',
      effortKey: 'claude_effort',
      effortLabel: 'Reasoning',
      model: backend === dflt ? String(d.claude_model || '') : '',
      models: ['fable', 'opus', 'sonnet', 'haiku'],
      effort: backend === dflt ? String(d.claude_effort || '') : '',
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
      model: backend === dflt ? String(d.codex_model || '') : '',
      models,
      effort: backend === dflt ? String(d.codex_reasoning_effort || '') : '',
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
      model: backend === dflt ? String(d.kimi_model || '') : '',
      models: ['kimi-code/kimi-for-coding', 'kimi-code/kimi-for-coding-highspeed', 'kimi-code/k3'],
      effort: backend === dflt ? String(d.kimi_thinking || '') : '',
      efforts: ['off', 'low', 'medium', 'high', 'xhigh', 'max'],
    };
  }
  if (backend === 'internal') {
    return { backend: 'internal', label: 'Internal agent' };
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
  // The Backend select uses the AgentLaunchConfig vocabulary the daemon
  // documents ("internal", "codex", "claude-code", "kimi"); picking one
  // explicitly pins the reviewed backend on the manifest. Pi is launchable
  // as the daemon default but has no model catalog wired here yet.
  const config = agendaStartSheetEl('div', 'ags-config');
  panel.appendChild(config);
  const configState = { spec: null, backendSel: null, modelSel: null, effortSel: null, backendOverride: '' };
  const renderConfigControls = () => {
    config.textContent = '';
    configState.modelSel = null;
    configState.effortSel = null;
    configState.backendSel = null;
    if (agendaStartSheetSettings === null) {
      config.appendChild(agendaStartSheetEl('div', 'ags-config-line',
        'Loading the daemon’s launch defaults…'));
      return;
    }
    const dfltSpec = agendaStartBackendConfig(agendaStartSheetSettings);
    const spec = agendaStartBackendConfig(agendaStartSheetSettings, configState.backendOverride);
    configState.spec = spec;
    const addSelect = (labelText, id, options, selected) => {
      const row = agendaStartSheetEl('div', 'ags-config-row');
      const label = agendaStartSheetEl('label', 'ags-label', labelText);
      label.setAttribute('for', id);
      row.appendChild(label);
      const select = document.createElement('select');
      select.id = id;
      for (const [value, text] of options) {
        const option = document.createElement('option');
        option.value = value;
        option.textContent = text;
        if (value === selected) option.selected = true;
        select.appendChild(option);
      }
      row.appendChild(select);
      const hint = agendaStartSheetEl('div', 'ags-hint',
        selected ? 'explicit — recorded on the manifest' : 'daemon default');
      row.appendChild(hint);
      select.addEventListener('change', () => {
        hint.textContent = select.value
          ? 'explicit — recorded on the manifest'
          : 'daemon default';
      });
      config.appendChild(row);
      return select;
    };
    const backendOptions = [['', `Daemon default (${dfltSpec.label})`]];
    for (const [value, text] of [
      ['internal', 'Internal agent'], ['codex', 'Codex'],
      ['claude-code', 'Claude Code'], ['kimi', 'Kimi Code'],
    ]) {
      backendOptions.push([value, text]);
    }
    configState.backendSel = addSelect('Backend', 'ags-config-backend',
      backendOptions, configState.backendOverride);
    configState.backendSel.addEventListener('change', () => {
      configState.backendOverride = configState.backendSel.value;
      renderConfigControls();
    });
    if (!spec.backend || spec.backend === 'internal') {
      config.appendChild(agendaStartSheetEl('div', 'ags-hint',
        'Model and provider follow the daemon’s native configuration.'));
      return;
    }
    if (!spec.modelKey) return;
    const inheritLabel = (dflt) => dflt
      ? `Daemon default (${dflt})` : 'Daemon default (backend default)';
    const modelValues = [...spec.models];
    if (spec.model && !modelValues.includes(spec.model)) modelValues.push(spec.model);
    configState.modelSel = addSelect('Model', 'ags-config-model',
      [['', inheritLabel(spec.model)], ...modelValues.map((v) => [v, v])], '');
    const effortValues = [...spec.efforts];
    if (spec.effort && !effortValues.includes(spec.effort)) effortValues.push(spec.effort);
    configState.effortSel = addSelect(spec.effortLabel, 'ags-config-effort',
      [['', inheritLabel(spec.effort)], ...effortValues.map((v) => [v, v])], '');
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
  const agentConfig = {};
  if (configState && configState.backendOverride) {
    agentConfig.agent = configState.backendOverride;
  }
  if (spec && spec.backend && spec.backend !== 'internal') {
    const model = configState.modelSel ? configState.modelSel.value : '';
    const effort = configState.effortSel ? configState.effortSel.value : '';
    if (model) agentConfig[spec.modelKey] = model;
    if (effort) agentConfig[spec.effortKey] = effort;
    if ((model || effort) && !agentConfig.agent) agentConfig.agent = spec.backend;
  }
  if (Object.keys(agentConfig).length) params.agent_config = agentConfig;
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

function agendaRenderAll() {
  agendaRenderTab();
  agendaRenderCard();
  agendaInspectorRender();
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
    agendaEnsureScaffold();
    agendaBuildCard();
    agendaRefresh();
    // Follow-up affordance liveness: the inspector's follow-up action is
    // derived at render time from session-window state the agenda has no
    // event lane for, so a visible tab re-renders when (and only when) the
    // eligibility signature changes — the target-switch poll idiom,
    // write-guarded.
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
        agendaInspectorRender();
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
