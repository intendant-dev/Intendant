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
// Track AS S3 — the resume cursor: the daemon's op-log line seq as of
// the last full fetch, advanced by each `agenda_changed` (which carries
// its producing op's seq) and by each heal. null until a response
// carries one (a pre-seq daemon serves none — healing then falls back
// to the full refetch).
let agendaSeq = null;
let agendaReminderPolicy = null; // owner delivery policy (Settings-gated)
// Session-resolution join from the list response: recorded session id →
// { source, conversation_id, key, name, project_root } for the
// Sessions-tab row. Ids the daemon could not resolve have no entry —
// surfaces fall back to the raw id. `attempted` remembers ids a fetch
// already tried, so an unresolvable id never causes refetch loops on the
// event lane.
let agendaSessions = {};
// Tier-1 PR render join (Track PR): the snapshot's `pull_requests`
// sibling map — anchor url-ref locator → live open-PR state as the
// scanner's last poll served it. Never fields on items; a locator with
// no entry claims nothing.
let agendaPullRequests = {};
let agendaSessionLookupsAttempted = new Set();
// Items whose full annotation thread is expanded (render caps at 3).
const agendaExpandedThreads = new Set();

// ---- Lens + inspector view state (the redesigned tab). Ephemeral
// browser state — never persisted, never on the wire.
let agendaLens = 'now';

// Progressive-disclosure depth for the whole tab — a persisted
// presentation preference, never wire state: calm folds the machinery
// away (triage/must-read chips, tags, exec labels, manifest plumbing,
// the hood), standard is the working view, everything adds ids and raw
// op coordinates inline. Gates read agendaDepthCalm()/agendaDepthAll()
// at render time.
let agendaDepth = 'standard';
try {
  const savedDepth = localStorage.getItem('ui2.agenda.depth');
  if (['calm', 'standard', 'everything'].includes(savedDepth)) agendaDepth = savedDepth;
} catch { /* storage unavailable — session-local default */ }

function agendaDepthCalm() { return agendaDepth === 'calm'; }
function agendaDepthAll() { return agendaDepth === 'everything'; }

function agendaSetDepth(depth) {
  if (!['calm', 'standard', 'everything'].includes(depth) || depth === agendaDepth) return;
  agendaDepth = depth;
  try { localStorage.setItem('ui2.agenda.depth', depth); } catch { /* session-local */ }
  agendaDepthSyncSeg();
  agendaRenderTab();
  if (typeof agendaInspectorRender === 'function') agendaInspectorRender();
}
// ---- Manifest-digest vocabulary (one formatter, one chip) ----
// The digest is the approval ceremony's subject: an owner Approve binds
// exactly these bytes, and every re-propose mints a NEW digest while
// the item id stays. The one truncation length lives here — never
// per-surface — and stays above the ledger search's 8-hex-char
// digest-prefix floor, so the short form a surface shows always
// resolves to its owning item when pasted into search.
const AGENDA_DIGEST_SHORT_LEN = 10;

function agendaShortDigest(digest) {
  const d = String(digest || '');
  return d.length > AGENDA_DIGEST_SHORT_LEN ? `${d.slice(0, AGENDA_DIGEST_SHORT_LEN)}…` : d;
}

// The interactive form every approval surface renders: hover reveals
// the full digest, click copies it. Digest values are daemon-minted
// lowercase hex; escaped like every interpolation anyway.
function agendaDigestChipHtml(digest, tip, extraClass) {
  const d = String(digest || '');
  if (!d) return '';
  return `<button type="button" class="ag2-digest-chip${extraClass || ''}" data-copy-digest="${escapeHtml(d)}"`
    + ` title="${escapeHtml(`${tip ? `${tip} — ` : ''}sha256 ${d} — click to copy`)}">`
    + `${escapeHtml(`digest ${agendaShortDigest(d)}`)}</button>`;
}

// One copy wire for every chip, wherever it renders (cards, inspector,
// the workflow sheet on document.body) — document-level by design.
document.addEventListener('click', (e) => {
  const btn = e.target && e.target.closest && e.target.closest('[data-copy-digest]');
  if (btn) agendaCopyText(btn.dataset.copyDigest, 'the manifest digest');
});

// ---- Changed-since (the approval-time editor's honesty chip) ----
// The digest updates IN PLACE on the card when a manifest is revised —
// by the owner's own edit or by a session re-proposing underneath an
// open tab. Approve is mechanically safe either way (the button carries
// the rendered revision's digest and the daemon refuses stale bytes);
// this map makes the swap VISIBLE: per effect lineage, the digest the
// owner last looked at. Seeded silently at first render; acknowledged
// when they open the editor or inspector, click the chip, or save an
// edit themselves (their own revision needs no warning — the toast and
// the in-place pulse are the feedback). View-local by design: "since
// you last looked" is a property of this tab, not daemon state — the
// op log stays the durable revision history.
const agendaSeenEffectDigests = Object.create(null);
let agendaDigestPulse = null; // { effectId, at } — one-shot self-edit pulse

function agendaAckEffectDigest(effectId, digest) {
  if (effectId && digest) agendaSeenEffectDigests[effectId] = digest;
}

function agendaEffectRevisionChipHtml(effect) {
  if (!effect || !effect.effect_id || !effect.digest) return '';
  const seen = agendaSeenEffectDigests[effect.effect_id];
  if (!seen) {
    agendaSeenEffectDigests[effect.effect_id] = effect.digest;
    return '';
  }
  if (seen === effect.digest) return '';
  return `<button type="button" class="ag2-revised-chip" data-ack-effect="${escapeHtml(effect.effect_id)}"`
    + ` data-ack-digest="${escapeHtml(effect.digest)}"`
    + ` title="The manifest was revised since you last looked (you saw ${escapeHtml(agendaShortDigest(seen))};`
    + ` it is now ${escapeHtml(agendaShortDigest(effect.digest))}). Approve signs the new bytes — review, then click to dismiss">`
    + 'revised</button>';
}

// The one-shot pulse class for the render right after the owner's own
// edit — the card visibly acknowledges the in-place digest update.
function agendaDigestPulseClass(effectId) {
  const p = agendaDigestPulse;
  return p && p.effectId === effectId && Date.now() - p.at < 4000 ? ' is-pulse' : '';
}

document.addEventListener('click', (e) => {
  const ack = e.target && e.target.closest && e.target.closest('[data-ack-effect]');
  if (!ack) return;
  agendaAckEffectDigest(ack.dataset.ackEffect, ack.dataset.ackDigest);
  agendaRenderTab();
  if (typeof agendaInspectorRender === 'function') agendaInspectorRender();
});

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
      // Track AS S5: the list feed is SUMMARIES (titles, chips, edges,
      // served flags — ~9× lighter than full items); the inspector and
      // expansions fetch one full item on demand (agendaFullItemFor).
      const resp = await daemonApi.request('api_agenda_list', { shape: 'summary' });
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        agendaItems = resp.body.items;
        agendaCounts = resp.body.counts || agendaCounts;
        agendaSkippedLines = resp.body.skipped_lines || 0;
        if (typeof resp.body.seq === 'number') agendaSeq = resp.body.seq;
        agendaReminderPolicy = resp.body.reminder_policy || agendaReminderPolicy;
        agendaSessions = resp.body.sessions || {};
        agendaPullRequests = resp.body.pull_requests || {};
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

// Track AS S5 — the full-item side cache: the inspector and expansions
// read FULL items (body, annotation thread, ask questions, manifests)
// fetched one at a time from the item route; the list feed stays
// summaries. Entries are keyed by id and validated by updated_ms — a
// newer summary/event for the id invalidates a staler full copy at read
// time (no eviction sweep needed; the map is bounded by items ever
// inspected this page-load plus event arrivals).
const agendaFullItems = new Map();
const agendaFullFetchesInFlight = new Set();

// The freshest FULL item for `id` if the cache has one at least as new
// as the list's summary row; otherwise fires one background fetch
// (single-flight per id) and returns null — callers render the summary
// degraded and repaint on arrival.
function agendaFullItemFor(id) {
  if (!id) return null;
  const summary = agendaFindItem(id);
  const cached = agendaFullItems.get(id);
  if (cached && (!summary || (cached.updated_ms || 0) >= (summary.updated_ms || 0))) {
    return cached;
  }
  if (!agendaFullFetchesInFlight.has(id)) {
    agendaFullFetchesInFlight.add(id);
    (async () => {
      try {
        const resp = await daemonApi.request('api_agenda_item', { item_id: id });
        if (resp.ok && resp.body && resp.body.item) {
          agendaFullItems.set(id, resp.body.item);
          // The per-item join may resolve ids the list join skipped.
          Object.assign(agendaSessions, resp.body.sessions || {});
          Object.assign(agendaPullRequests, resp.body.pull_requests || {});
        }
      } catch (e) {
        console.warn('[agenda] full-item fetch failed', id, e);
      } finally {
        agendaFullFetchesInFlight.delete(id);
        agendaRenderAll();
        if (typeof agendaInspectorRender === 'function') agendaInspectorRender();
        // A parked-ask announce may have been waiting on this item's
        // question payload (announce dedupes by ask id — idempotent).
        agendaAnnounceParkedAsks();
        // The schedule editor waits on the full grain (its prefill
        // round-trips the whole manifest) — re-enter the opener now
        // that the item landed.
        if (typeof agendaSheetState !== 'undefined' && agendaSheetState
          && agendaSheetState.kind === 'sched-loading' && agendaSheetState.itemId === id
          && typeof agendaOpenSchedSheet === 'function') {
          agendaOpenSchedSheet(id);
        }
      }
    })();
  }
  return null;
}

// Adopt a FULL item arriving outside the summary feed (the
// `agenda_changed` event lane, op-response merges): file it in the
// full-item cache, and shape a summary-compatible row for the list —
// slim derivations only (counts), never predicate re-derivation. The
// served flags (blocked/frontier/triage) carry over from the replaced
// summary row and AGE until the next summary pull refreshes them — the
// ruled Q4 decoration-freshness contract; a brand-new item wears no
// flags until first summarized.
function agendaAdoptFullItem(full) {
  agendaFullItems.set(full.id, full);
  const prior = agendaFindItem(full.id);
  const row = Object.assign({}, full, {
    annotations_count: Array.isArray(full.annotations) ? full.annotations.length : 0,
    blocked: prior ? prior.blocked : undefined,
    frontier: prior ? prior.frontier : undefined,
    triage: prior ? prior.triage : undefined,
  });
  return row;
}

// Track AS S3 — the healing lane. A held cursor turns "did I miss
// anything?" into a delta pull (`since_seq`): the daemon returns only
// items changed by ops at or after the cursor, upserted over the cache
// exactly like the event lane's merges (append-only log ⇒ no deletions
// to reconcile). Wired into event_gap recovery, transport re-hydration
// (reconnects), and tab wake — the full refetch remains bootstrap-only.
// No cursor (never fetched, or a pre-seq daemon) falls back to the full
// refresh; a heal FAILURE keeps the cache and the cursor untouched (the
// next signal retries; the data on screen stays live-known-stale).
async function agendaHeal(reason) {
  if (agendaItems === null || typeof agendaSeq !== 'number') return agendaRefresh();
  if (agendaFetchInFlight) return agendaFetchInFlight;
  agendaFetchInFlight = (async () => {
    try {
      const resp = await daemonApi.request('api_agenda_list', {
        since_seq: agendaSeq,
        shape: 'summary',
      });
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        for (const item of resp.body.items) {
          const at = agendaItems.findIndex((x) => x.id === item.id);
          // An event that landed while this pull was in flight may be
          // newer than the pull's copy — never let the heal roll an
          // item backwards (updated_ms is op-time, monotonic per item;
          // ties take the incoming copy for its fresher decorations).
          if (at >= 0 && (agendaItems[at].updated_ms || 0) > (item.updated_ms || 0)) continue;
          if (at >= 0) agendaItems[at] = item;
          else agendaItems.push(item);
        }
        agendaCounts = resp.body.counts || agendaCounts;
        if (typeof resp.body.skipped_lines === 'number') agendaSkippedLines = resp.body.skipped_lines;
        if (typeof resp.body.seq === 'number') agendaSeq = resp.body.seq;
        // Delta joins cover the served (changed) set only — merge, never
        // replace, so entries for untouched items survive.
        Object.assign(agendaSessions, resp.body.sessions || {});
        Object.assign(agendaPullRequests, resp.body.pull_requests || {});
        resp.body.items
          .flatMap(agendaItemSessionIds)
          .forEach((id) => agendaSessionLookupsAttempted.add(id));
        agendaLoadError = '';
        // A parked ask that arrived while events were down re-surfaces
        // exactly like one that arrived live.
        agendaAnnounceParkedAsks();
      }
    } catch (e) {
      console.warn('[agenda] heal failed', reason || '', e);
    } finally {
      agendaFetchInFlight = null;
    }
    agendaRenderAll();
  })();
  return agendaFetchInFlight;
}

// Tab wake: a hidden tab's event socket may have quietly gapped or the
// browser throttled it — one cheap delta on return to visibility keeps
// the surfaces honest (empty delta in the common case).
document.addEventListener('visibilitychange', () => {
  if (!document.hidden && agendaItems !== null) agendaHeal('tab-wake');
});

// QA readback (window.qa convention — the whole SPA is one module
// scope, so the harness reaches state only through this deliberate
// seam). Serving-grain state for the Track AS QA gates: the resume
// cursor, cache grain, and the healing wiring. Read-only.
window.qa = Object.assign(window.qa || {}, {
  agendaServing() {
    const sample = Array.isArray(agendaItems) && agendaItems.length ? agendaItems[0] : null;
    return {
      seq: agendaSeq,
      items: Array.isArray(agendaItems) ? agendaItems.length : null,
      healWired: typeof agendaHeal === 'function',
      summaryFeed: agendaRefresh.toString().includes("shape: 'summary'"),
      servedFlagsAdopted: agendaItemIsBlocked.toString().includes('item.blocked'),
      fullItemLane: typeof agendaFullItemFor === 'function',
      fullItemsCached: agendaFullItems.size,
      sampleIsSummary: sample ? !Array.isArray(sample.annotations) : null,
      loadError: agendaLoadError || null,
    };
  },
  async agendaHealNow() {
    await agendaHeal('qa-probe');
    return window.qa.agendaServing();
  },
});

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
      && item.ask && item.ask.ask_id && !item.dismissed)
    // Oldest first, so with several parked asks the panel lands on the
    // newest — the same "latest ask surfaces" behavior live asks have.
    .sort((a, b) => (a.id < b.id ? -1 : 1));
  for (const item of open) {
    const askId = item.ask.ask_id;
    if (agendaAnnouncedAsks.has(askId)) continue;
    // Summary rows carry the ask id but not the question payload (S5);
    // the full copy comes from the item cache — a miss warms the fetch
    // and the arrival re-runs this announce (dedupe makes it safe).
    const full = Array.isArray(item.ask.questions) ? item : agendaFullItemFor(item.id);
    if (!full || !full.ask || !Array.isArray(full.ask.questions) || !full.ask.questions.length) {
      continue;
    }
    agendaAnnouncedAsks.add(askId);
    showUserQuestion(askId, full.ask.questions, '', undefined, false, { agendaBacked: true });
  }
}

// Explicit "open the question panel" (the rail door in the inspector's
// question section). Unlike the once-per-load announce this is a user
// act: it re-surfaces even a tucked or previously-dismissed panel, and it
// navigates to the Activity tab where the panel lives.
function agendaOpenParkedAsk(itemId, retriesLeft = 6) {
  let item = (agendaItems || []).find((candidate) => candidate.id === itemId);
  if (item && item.ask && item.ask.ask_id && !Array.isArray(item.ask.questions)) {
    // Summary row (S5): the click needs the question payload — warm the
    // full-item fetch and retry briefly (loopback resolves in ms).
    const full = agendaFullItemFor(itemId);
    if (!full) {
      if (retriesLeft > 0) setTimeout(() => agendaOpenParkedAsk(itemId, retriesLeft - 1), 300);
      return;
    }
    item = full;
  }
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
  const row = agendaAdoptFullItem(d.item);
  const at = agendaItems.findIndex((item) => item.id === d.item.id);
  if (at >= 0) agendaItems[at] = row;
  else agendaItems.push(row);
  if (d.counts) agendaCounts = d.counts;
  // Track AS S3: the event names its producing op — advance the resume
  // cursor past it. max() because a delta pull may already sit ahead.
  if (typeof d.seq === 'number') {
    agendaSeq = Math.max(typeof agendaSeq === 'number' ? agendaSeq : 0, d.seq + 1);
  }
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
  if (agendaItems === null) {
    // Build the scaffold immediately (replacing the legacy static markup
    // with the Loading state) — the fetch fills it in when it lands.
    agendaRenderTab();
    agendaRefresh();
  } else {
    agendaRenderAll();
  }
}

async function agendaSendOp(params, button) {
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (resp.ok && resp.body && resp.body.item) {
      // The event lane repaints too; merging here keeps the UI honest
      // even if this tab's event socket is briefly down. Returns the
      // item (truthy) so multi-op gestures can chain on the minted id.
      agendaObserveServerMessage({ item: resp.body.item });
      return resp.body.item;
    }
    const message = (resp.body && resp.body.error) || `agenda op failed (${resp.status})`;
    agendaFlashError(message);
    agendaPaintRefusal(button, message);
    return false;
  } catch (e) {
    const message = String(e && e.message || e);
    agendaFlashError(message);
    agendaPaintRefusal(button, message);
    return false;
  } finally {
    if (button) button.disabled = false;
  }
}

// The refusal painted AT the surface that refused (UX0 ruling; UX2) —
// the top-of-tab flash evaporates in 6 s and sits far from the card
// that asked. Transient by construction: the next render of the card
// list sweeps it.
function agendaPaintRefusal(button, message) {
  const card = button && button.closest && button.closest('.ag2-card');
  if (!card) return;
  const previous = card.querySelector('.ag2-card-refusal');
  if (previous) previous.remove();
  const main = card.querySelector('.ag2-card-main');
  if (!main) return;
  const line = document.createElement('div');
  line.className = 'ag2-card-refusal is-refusal';
  line.textContent = message;
  main.appendChild(line);
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

// One link's render judgment: { satisfied, review } where review is
// '' | 'target_retired' | 'target_missing'.
function agendaLinkState(link) {
  const target = agendaFindItem(link.target_id);
  if (!target) return { satisfied: false, review: 'target_missing' };
  if (target.status === 'done') return { satisfied: true, review: '' };
  if (target.status === 'retired') return { satisfied: false, review: 'target_retired' };
  return { satisfied: false, review: '' };
}

// Track AS S5: `blocked` is SERVED (the daemon's serving-seam predicate
// — one implementation, ruling §4.4; the client re-derivation is
// deleted). Event-lane rows carry the flag forward from their replaced
// summary and age until the next pull (the ruled Q4 freshness
// contract); a row that has never been summarized wears no flag.
function agendaItemIsBlocked(item) {
  return item.blocked === true;
}

// The card's one-line blocked statement (first gate wins). Plain TEXT —
// callers escape.
function agendaBlockedLine(item) {
  if (item.status !== 'open') return null;
  const blocker = (item.blockers || []).find((b) => !b.cleared);
  if (blocker) return `Blocked — waiting on: “${blocker.criterion}”`;
  for (const link of item.relies_on || []) {
    const target = agendaFindItem(link.target_id);
    if (!target) return 'Prerequisite missing from the fold — review';
    if (target.status === 'retired') return `Prerequisite “${target.title}” was retired — review`;
    if (target.status === 'open') return `Waits on “${target.title}” — still open`;
  }
  return null;
}

// The item's scheduled-session effect, judged for render: kind is one of
// running | suspended | pending | standing | armed | finished, plus the
// trigger vocabulary (Track T manifests carry `trigger` INSTEAD of a
// clock cadence): watching (on_item_match, approved), waiting
// (on_unblock, prerequisites still open), ready (on_unblock, every
// prerequisite complete — the scheduler dispatches within the minute).
// Mirrors the daemon's fold judgments (AgendaEffect::suspended, the
// scheduler's next-instant derivation, the trigger arm rules) — derived
// here every paint, never stored.
function agendaEffectState(item) {
  const effect = (item.effects || [])[0];
  if (!effect || !effect.manifest) return null;
  const manifest = effect.manifest;
  const rec = manifest.recurrence || null;
  const trig = manifest.trigger || null;
  // Failure-suspend covers standing series AND triggered mandates (the
  // C-floor guardrail); one-shot clock manifests never suspend.
  const threshold = rec || trig ? Math.max(1, (rec && rec.suspend_after_failures) || 3) : 0;
  const suspended = !!(rec || trig) && (effect.consecutive_failures || 0) >= threshold;
  const running = !!(effect.last_run && effect.last_run.state === 'started');
  // The daemon decorates each effect with the planner's REAL next firing
  // instant (`next_fire_ms`, absent when nothing will fire) — prefer it
  // over reimplementing the planner here; the local arithmetic remains
  // only as the fallback for undecorated data (stale caches, replays).
  let next = effect.next_fire_ms || manifest.fire_at_ms;
  if (!effect.next_fire_ms && rec && rec.every_ms > 0) {
    const behind = Math.max(0, Math.ceil((Date.now() - manifest.fire_at_ms) / rec.every_ms));
    next = manifest.fire_at_ms + behind * rec.every_ms;
  }
  const kind = running ? 'running'
    : suspended ? 'suspended'
      : !effect.approval ? 'pending'
        : trig ? (trig.kind === 'on_item_match' ? 'watching'
          : (item.relies_on || []).every((link) => agendaLinkState(link).satisfied)
            ? 'ready' : 'waiting')
          : rec ? 'standing'
            : next > Date.now() ? 'armed' : 'finished';
  return { effect, manifest, rec, trig, threshold, suspended, running, next, kind };
}

// One-line executor description for a manifest's `agent_config` block:
// backend · model · effort, or "native default" when the manifest
// inherits everything. Pure render vocabulary — the daemon's launch
// resolution chain is the authority on what actually applies at spawn.
function agendaExecutorLabel(config) {
  if (!config || typeof config !== 'object') return 'native default';
  const backend = config.agent === 'internal' ? 'native' : (config.agent || '');
  const model = config.claude_model || config.codex_model
    || config.kimi_model || config.pi_model || '';
  const effort = config.claude_effort || config.codex_reasoning_effort
    || config.kimi_thinking || config.pi_thinking || '';
  const bits = [backend, model, effort].filter(Boolean);
  if (!bits.length) return 'native default';
  if (!backend) bits.unshift('default backend');
  return bits.join(' · ');
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

// The undirected adjacency union: links stored on this item plus links
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
// Track AS S5: the rank/note now arrive SERVED (`item.triage`, derived
// once at the daemon's serving seam from the same convention); the
// client parse is deleted. Return shape unchanged for callers.
function agendaTriageInfo(item) {
  if (!item.triage) return null;
  return { rank: item.triage.rank ?? null, text: item.triage.note || '' };
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
  if (p.kind === 'daemon') return 'the daemon';
  if (p.kind === 'agent_session') return 'an agent session';
  return p.principal || '';
}

// The tier-1 join row for an item's PR url ref, if the snapshot served
// one (open PRs of watched repos only — absent claims nothing).
function agendaPrTier1(item) {
  for (const r of (item.refs || [])) {
    const t1 = agendaPullRequests[r.locator];
    if (t1) return t1;
  }
  return null;
}

// The PR url ref itself (github.com pull link), joined or not — the
// inspector's tier-2 fetch keys off its presence.
function agendaPrLocator(item) {
  for (const r of (item.refs || [])) {
    if (r.ref_type === 'url' && /^https:\/\/github\.com\/[^/]+\/[^/]+\/pull\/\d+/.test(r.locator || '')) {
      return r.locator;
    }
  }
  return null;
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
    const inheritLabel = (defaultValue) => defaultValue
      ? `Daemon default (${defaultValue})` : 'Daemon default (backend default)';
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

// ---- Served definition catalog (Track AW) ----
// The Automate surfaces' data source: the daemon-served catalog
// (GET /api/agenda/definitions). Every entry carries its validation
// state, advisories, and the FULL definition text — exactly the bytes
// a stamp of that entry would seal. Fetched on sheet open; cached so a
// reopen paints instantly and refreshes in place. Invalid and shadowed
// entries stay VISIBLE (disabled, with the reason) — the catalog never
// hides a refusal.
let agendaDefinitionCatalog = null; // null = never fetched; else the entries array
let agendaDefinitionCatalogError = '';
let agendaDefinitionCatalogLoading = false;

function agendaFetchDefinitionCatalog(onSettled) {
  if (agendaDefinitionCatalogLoading) return;
  agendaDefinitionCatalogLoading = true;
  daemonApi.request('api_agenda_definitions', {})
    .then((res) => {
      if (res.ok && res.body && Array.isArray(res.body.definitions)) {
        agendaDefinitionCatalog = res.body.definitions;
        agendaDefinitionCatalogError = '';
      } else {
        agendaDefinitionCatalogError =
          (res.body && res.body.error) || `catalog fetch failed (${res.status})`;
      }
    })
    .catch((e) => { agendaDefinitionCatalogError = String((e && e.message) || e); })
    .finally(() => {
      agendaDefinitionCatalogLoading = false;
      if (typeof onSettled === 'function') onSettled();
    });
}

// A catalog entry's stamp shape: 'workflow' (multi-node), 'triggered'
// (single node firing on matching items), or 'action' (single node on a
// cadence). Derived, never declared (the Q10 rule) — arity and the
// node's trigger decide.
function agendaDefinitionKind(entry) {
  if (!entry) return null;
  if (entry.workflow) return 'workflow';
  return (((entry.nodes || [])[0] || {}).trigger_kind) ? 'triggered' : 'action';
}

// One line of what stamping this definition sets up — the picker's and
// preview's kind caption, derived from the served nodes.
function agendaDefinitionKindLine(entry) {
  const kind = agendaDefinitionKind(entry);
  if (kind === 'workflow') {
    return `workflow · ${(entry.nodes || []).length} nodes, each its own approval`;
  }
  const node = (entry.nodes && entry.nodes[0]) || {};
  if (kind === 'triggered') {
    const tags = (node.trigger_tags || []).length ? `:${node.trigger_tags.join(',')}` : '';
    return `standing action — fires on new ${node.trigger_kind || 'item'}${tags}`;
  }
  return node.every_ms
    ? `standing action — every ${agendaCadenceLabel(node.every_ms)}`
    : 'standing action — cadence chosen at stamp time';
}

// Provenance chip: which library serves this definition. Discovery
// grants nothing either way — bindingness needs the stamp seal under an
// approval digest.
function agendaProvenanceChipEl(provenance) {
  const p = provenance === 'personal' ? 'personal' : 'house';
  const chip = agendaStartSheetEl('span', `agsx-prov agsx-prov-${p}`, p);
  chip.title = p === 'house'
    ? 'Ships with this daemon, materialized into the library root — stamping seals the file itself'
    : 'From this daemon’s personal library — shadows a house definition of the same name';
  return chip;
}

// Presentation split of a definition's bytes: drop the frontmatter fence
// and the per-node ```toml config blocks, keep the authored prose the
// fired session actually obeys. Display only — the exact sealed bytes
// stay authoritative, one expander away.
function agendaDefinitionProse(text) {
  const lines = String(text || '').replace(/\r\n/g, '\n').split('\n');
  let i = 0;
  if (lines[0] === '---') {
    i = 1;
    while (i < lines.length && lines[i] !== '---') i += 1;
    i += 1;
  }
  const out = [];
  let inConfig = false;
  for (; i < lines.length; i += 1) {
    const line = lines[i];
    if (!inConfig && line.trim().startsWith('```toml')) { inConfig = true; continue; }
    if (inConfig) {
      if (line.trim() === '```') inConfig = false;
      continue;
    }
    out.push(line);
  }
  return out.join('\n').replace(/\n{3,}/g, '\n\n').trim();
}

// ---- Create-from-definition: the Automate sheet (Track AU → AW) ----
// The ctl walkthrough as a guided flow: pick a definition from the
// served catalog, read the FULL text (data the owner reads — never
// hidden or truncated; exactly what a stamp seals), choose cadence,
// first fire, and executor, then STAMP through the daemon's stamp op —
// the daemon reads, validates, and seals the definition, PARKS the item
// and PROPOSES the standing effect with the sheet's choices as prefills
// into the ordinary manifest intake. The flow never approves — it lands
// the owner on the ordinary card whose Approve affordance binds the
// digest; the ceremony stays the owner's untouched final act (a parity
// test pins this fragment approve-free).

let agendaAutomationSheetOpen = false;

function agendaEnsureAutomationSheet() {
  let host = document.getElementById('agenda-automation-sheet');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'agenda-automation-sheet';
  host.hidden = true;
  const backdrop = document.createElement('div');
  backdrop.className = 'ags-backdrop';
  backdrop.addEventListener('click', agendaCloseAutomationSheet);
  const panel = document.createElement('div');
  panel.className = 'ags-panel agsx-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'New automation — stamp a definition');
  host.appendChild(backdrop);
  host.appendChild(panel);
  document.body.appendChild(host);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !host.hidden) agendaCloseAutomationSheet();
  });
  document.addEventListener('pointerdown', (event) => {
    if (host.hidden) return;
    if (event.target.closest?.('#agenda-automation-sheet .ags-panel, #ag2-automate')) return;
    agendaCloseAutomationSheet();
  }, true);
  return host;
}

function agendaCloseAutomationSheet() {
  const host = document.getElementById('agenda-automation-sheet');
  if (!host) return;
  host.hidden = true;
  host.classList.remove('sheet', 'popover');
  agendaAutomationSheetOpen = false;
}

// Default first fire: tomorrow 09:00 local, as a datetime-local value.
function agendaAutomationDefaultFire() {
  const at = new Date();
  at.setDate(at.getDate() + 1);
  at.setHours(9, 0, 0, 0);
  const pad = (n) => String(n).padStart(2, '0');
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}T${pad(at.getHours())}:${pad(at.getMinutes())}`;
}

const AGENDA_AUTOMATION_CADENCES = [
  { label: 'Daily', ms: 24 * 60 * 60 * 1000 },
  { label: 'Weekly', ms: 7 * 24 * 60 * 60 * 1000 },
  { label: 'Every two weeks', ms: 14 * 24 * 60 * 60 * 1000 },
  { label: 'Every 30 days', ms: 30 * 24 * 60 * 60 * 1000 },
];

function agendaOpenAutomationSheet(anchor) {
  const host = agendaEnsureAutomationSheet();
  const panel = host.querySelector('.ags-panel');
  panel.textContent = '';
  agendaAutomationSheetOpen = true;
  let entry = null; // the selected catalog entry, any kind
  let selectedName = '';

  const head = agendaStartSheetEl('div', 'ags-head');
  head.appendChild(agendaStartSheetEl('span', 'ags-title', 'New automation'));
  const close = agendaStartSheetEl('button', 'ags-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Cancel');
  close.addEventListener('click', agendaCloseAutomationSheet);
  head.appendChild(close);
  panel.appendChild(head);
  panel.appendChild(agendaStartSheetEl('div', 'ags-sub',
    'Pick a definition, review it, then stamp: stamping seals the definition file, parks it on the agenda, and proposes its session manifests — nothing runs until you approve each digest on its card.'));

  // Definition picker (served catalog) — every kind side by side, each
  // rendered as what it means: name, provenance, shape, description.
  const seg = agendaStartSheetEl('div', 'agsx-defs');
  panel.appendChild(seg);

  // The definition, rendered for reading: header, node summary, and the
  // authored prose the fired sessions obey — with the exact sealed
  // bytes one explicit expander away (textContent rendering throughout,
  // no markup execution).
  const preview = agendaStartSheetEl('div', 'agsx-def');
  panel.appendChild(preview);

  // Cadence + first fire + suspend threshold.
  const cadRow = agendaStartSheetEl('div', 'ags-config-row');
  const cadLabel = agendaStartSheetEl('label', 'ags-label', 'Cadence');
  cadLabel.setAttribute('for', 'agsx-cadence');
  cadRow.appendChild(cadLabel);
  const cadence = document.createElement('select');
  cadence.id = 'agsx-cadence';
  for (const c of AGENDA_AUTOMATION_CADENCES) {
    const option = document.createElement('option');
    option.value = String(c.ms);
    option.textContent = c.label;
    cadence.appendChild(option);
  }
  cadRow.appendChild(cadence);
  cadRow.appendChild(agendaStartSheetEl('div', 'ags-hint',
    'one approval covers the whole series'));
  panel.appendChild(cadRow);

  const fireRow = agendaStartSheetEl('div', 'ags-config-row');
  const fireLabel = agendaStartSheetEl('label', 'ags-label', 'First run');
  fireLabel.setAttribute('for', 'agsx-fire');
  fireRow.appendChild(fireLabel);
  const fire = document.createElement('input');
  fire.type = 'datetime-local';
  fire.id = 'agsx-fire';
  fire.value = agendaAutomationDefaultFire();
  fireRow.appendChild(fire);
  panel.appendChild(fireRow);

  // Project pin (T3c): digest-bound on the manifest — where fired
  // sessions run. Empty = the legacy resolution (parking session's
  // root, else the daemon default), which a picker-stamped manifest on
  // a projectless daemon does not have.
  const projRow = agendaStartSheetEl('div', 'ags-config-row');
  const projLabel = agendaStartSheetEl('label', 'ags-label', 'Project');
  projLabel.setAttribute('for', 'agsx-project');
  projRow.appendChild(projLabel);
  const project = document.createElement('input');
  project.type = 'text';
  project.id = 'agsx-project';
  project.placeholder = 'daemon default (absolute path to pin)';
  projRow.appendChild(project);
  projRow.appendChild(agendaStartSheetEl('div', 'ags-hint',
    'where fired sessions run — required on a projectless daemon'));
  panel.appendChild(projRow);

  const suspendRow = agendaStartSheetEl('div', 'ags-config-row');
  const suspendLabel = agendaStartSheetEl('label', 'ags-label', 'Suspend after');
  suspendLabel.setAttribute('for', 'agsx-suspend');
  suspendRow.appendChild(suspendLabel);
  const suspend = document.createElement('input');
  suspend.type = 'number';
  suspend.id = 'agsx-suspend';
  suspend.min = '1';
  suspend.max = '20';
  suspendRow.appendChild(suspend);
  suspendRow.appendChild(agendaStartSheetEl('div', 'ags-hint',
    'consecutive failures before the series suspends (surfaced, never silently re-fired)'));
  panel.appendChild(suspendRow);

  // Executor: the same settings-derived backend/model/effort controls the
  // start sheet uses — untouched selects inherit the daemon defaults and
  // send nothing; explicit picks are recorded on the digest-bound
  // manifest, so the approval covers WHO runs the mandate.
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
      config.appendChild(row);
      return select;
    };
    const backendOptions = [['', 'Daemon default'], ['internal', 'Internal agent'],
      ['claude-code', 'Claude Code'], ['codex', 'Codex'], ['kimi', 'Kimi Code']];
    configState.backendSel = addSelect('Backend', 'agsx-config-backend',
      backendOptions, configState.backendOverride);
    configState.backendSel.addEventListener('change', () => {
      configState.backendOverride = configState.backendSel.value;
      renderConfigControls();
    });
    if (spec.backend && spec.backend !== 'internal') {
      const modelOptions = [['', 'Daemon default']]
        .concat((spec.models || []).map((m) => [m, m]));
      configState.modelSel = addSelect('Model', 'agsx-config-model', modelOptions, spec.model || '');
      const effortOptions = [['', 'Daemon default']]
        .concat((spec.efforts || []).map((e) => [e, e]));
      configState.effortSel = addSelect(spec.effortLabel || 'Effort', 'agsx-config-effort',
        effortOptions, spec.effort || '');
    }
  };
  renderConfigControls();
  if (typeof fetchDashboardSettings === 'function') {
    fetchDashboardSettings()
      .then((d) => { if (d && !d.error) agendaStartSheetSettings = d; })
      .catch(() => {})
      .finally(() => {
        if (agendaAutomationSheetOpen) renderConfigControls();
      });
  }

  // Pre-stamp summary: exactly what the Stamp gesture will seal, park,
  // and propose — rendered before the button, re-rendered as the
  // controls change, so the gesture is never a surprise.
  const summary = agendaStartSheetEl('div', 'agsx-summary');
  summary.hidden = true;
  panel.appendChild(summary);

  const error = agendaStartSheetEl('div', 'ags-error', '');
  error.hidden = true;
  panel.appendChild(error);

  const foot = agendaStartSheetEl('div', 'ags-foot');
  foot.appendChild(agendaStartSheetEl('div', 'ags-hint',
    'You approve on the card afterwards — this flow cannot approve.'));
  foot.appendChild(agendaStartSheetEl('span', 'ag2-spacer'));
  const cancel = agendaStartSheetEl('button', 'ags-btn', 'Cancel');
  cancel.type = 'button';
  cancel.addEventListener('click', agendaCloseAutomationSheet);
  const stampBtn = agendaStartSheetEl('button', 'ags-btn ags-start', 'Stamp');
  stampBtn.type = 'button';
  stampBtn.title = 'Seals the definition, parks the item(s), proposes the manifest(s) — approves nothing';
  stampBtn.addEventListener('click', () => agendaAutomationSheetSubmit(
    { entry: () => entry, cadence, fire, suspend, project, configState, error, stampBtn }));
  foot.appendChild(cancel);
  foot.appendChild(stampBtn);
  panel.appendChild(foot);

  const nodeExecutorLabel = (node) =>
    [node.agent, node.model, node.effort].filter(Boolean).join(' · ') || 'daemon default';

  const renderPreview = () => {
    preview.textContent = '';
    if (!entry) {
      preview.appendChild(agendaStartSheetEl('div', 'ags-hint',
        agendaDefinitionCatalog === null && !agendaDefinitionCatalogError
          ? 'Loading the definition catalog…'
          : (agendaDefinitionCatalogError
            ? `Definition catalog unavailable: ${agendaDefinitionCatalogError}`
            : 'No stampable definitions in the catalog.')));
      return;
    }
    const dhead = agendaStartSheetEl('div', 'agsx-def-head');
    dhead.appendChild(agendaStartSheetEl('span', 'agsx-def-title', entry.title || entry.name));
    dhead.appendChild(agendaProvenanceChipEl(entry.provenance));
    dhead.appendChild(agendaStartSheetEl('span', 'agsx-def-kind', agendaDefinitionKindLine(entry)));
    preview.appendChild(dhead);
    if (entry.description) {
      preview.appendChild(agendaStartSheetEl('div', 'agsx-def-desc', entry.description));
    }
    if (entry.advisories && entry.advisories.length) {
      preview.appendChild(agendaStartSheetEl('div', 'agsx-def-adv',
        `Advisory: ${entry.advisories.join('; ')}`));
    }
    if (agendaDefinitionKind(entry) === 'workflow') {
      for (const node of entry.nodes || []) {
        const row = agendaStartSheetEl('div', 'agsx-def-node');
        row.appendChild(agendaStartSheetEl('span', 'agsx-def-node-id', node.title || node.id));
        const bits = [nodeExecutorLabel(node)];
        if ((node.relies_on || []).length) bits.push(`after ${node.relies_on.join(', ')}`);
        row.appendChild(agendaStartSheetEl('span', 'agsx-def-node-meta', bits.join(' · ')));
        preview.appendChild(row);
      }
    }
    const prose = agendaDefinitionProse(entry.text);
    if (prose) {
      const body = agendaStartSheetEl('pre', 'agsx-preview agsx-def-prose');
      body.textContent = prose;
      preview.appendChild(body);
    }
    // Verification honesty: the pretty rendering never replaces the
    // bytes — the exact revision a stamp seals stays one gesture away.
    const exact = document.createElement('details');
    exact.className = 'agsx-exact';
    const sum = document.createElement('summary');
    sum.textContent = `Exact bytes a stamp seals${entry.sha256 ? ` — sha256 ${entry.sha256.slice(0, 12)}…` : ''}`;
    exact.appendChild(sum);
    const raw = agendaStartSheetEl('pre', 'agsx-preview');
    raw.textContent = entry.text;
    exact.appendChild(raw);
    preview.appendChild(exact);
  };

  const renderSummary = () => {
    summary.textContent = '';
    if (!entry) {
      summary.hidden = true;
      return;
    }
    summary.hidden = false;
    const kind = agendaDefinitionKind(entry);
    const add = (text) => summary.appendChild(agendaStartSheetEl('div', 'agsx-summary-line', text));
    summary.appendChild(agendaStartSheetEl('div', 'agsx-summary-head', 'Stamp will'));
    add(`seal this exact revision${entry.sha256 ? ` (sha256 ${entry.sha256.slice(0, 12)}…)` : ''} — firings execute the sealed bytes, whatever happens to the live file`);
    if (kind === 'workflow') {
      const n = (entry.nodes || []).length;
      add(`park a hub + ${n} node tasks and propose ${n} manifests — each node fires when its prerequisites complete, each with its own approval`);
    } else if (kind === 'triggered') {
      const node = (entry.nodes && entry.nodes[0]) || {};
      const tags = (node.trigger_tags || []).length ? ` tagged ${node.trigger_tags.join(', ')}` : '';
      add(`park one item and propose one standing manifest — fires when a new open ${node.trigger_kind || 'item'}${tags} arrives`);
    } else {
      const everyMs = Number(cadence.value);
      const firstRun = fire.value ? new Date(fire.value) : null;
      add(`park one item and propose one standing manifest — every ${agendaCadenceLabel(everyMs)}, first run ${firstRun && !Number.isNaN(firstRun.getTime()) ? firstRun.toLocaleString() : 'to pick'}, suspends after ${Math.max(1, Number(suspend.value) || 3)} straight failures`);
    }
    if (kind !== 'workflow') {
      const picks = [];
      if (configState.backendOverride) picks.push(configState.backendOverride);
      if (configState.modelSel && configState.modelSel.value) picks.push(configState.modelSel.value);
      if (configState.effortSel && configState.effortSel.value) picks.push(configState.effortSel.value);
      add(`executor: ${picks.length ? `${picks.join(' · ')} — recorded on the manifest` : 'daemon defaults'} · project: ${project.value.trim() || 'daemon default'}`);
    } else {
      add(`per-node executors and edges come from the definition · project: ${project.value.trim() || 'daemon default'}`);
    }
  };

  const renderSelection = () => {
    seg.querySelectorAll('button').forEach((b) =>
      b.classList.toggle('active', !!entry && b.dataset.definition === entry.name));
    const kind = agendaDefinitionKind(entry);
    // Cadence and first-fire knobs exist only where the stamp op accepts
    // them: cadenced actions. Workflows declare per-node executors and
    // structural on_unblock edges in the definition; triggered actions
    // fire on their declared match (executor override still applies).
    cadRow.hidden = kind !== 'action';
    fireRow.hidden = kind !== 'action';
    suspendRow.hidden = kind !== 'action';
    config.hidden = !kind || kind === 'workflow';
    stampBtn.disabled = !entry;
    if (entry && kind === 'action') {
      const prefill = (entry.nodes && entry.nodes[0]) || {};
      if (prefill.every_ms) {
        const ms = String(prefill.every_ms);
        if (!Array.from(cadence.options).some((o) => o.value === ms)) {
          const option = document.createElement('option');
          option.value = ms;
          option.textContent = 'Definition default';
          cadence.appendChild(option);
        }
        cadence.value = ms;
      }
      if (prefill.suspend_after) suspend.value = String(prefill.suspend_after);
      else if (!suspend.value) suspend.value = '3';
    }
    renderPreview();
    renderSummary();
  };
  // The summary mirrors the controls live — a stale promise line would
  // be worse than none.
  for (const [control, event] of [[cadence, 'change'], [fire, 'change'],
    [suspend, 'input'], [project, 'input'], [config, 'change']]) {
    control.addEventListener(event, renderSummary);
  }

  const selectable = (d) => d.valid && !d.shadowed;
  const renderPicker = () => {
    seg.textContent = '';
    const catalog = agendaDefinitionCatalog || [];
    entry = catalog.find((d) => selectable(d) && d.name === selectedName)
      || catalog.find(selectable) || null;
    selectedName = entry ? entry.name : '';
    for (const d of catalog) {
      const usable = selectable(d);
      const btn = agendaStartSheetEl('button', 'agsx-def-btn');
      btn.type = 'button';
      btn.dataset.definition = d.name;
      const nameRow = agendaStartSheetEl('div', 'agsx-def-btn-name', d.title || d.name);
      nameRow.appendChild(agendaProvenanceChipEl(d.provenance));
      btn.appendChild(nameRow);
      btn.appendChild(agendaStartSheetEl('div', 'agsx-def-btn-kind',
        usable ? agendaDefinitionKindLine(d) : (d.shadowed ? 'shadowed' : 'invalid')));
      if (d.description) {
        btn.appendChild(agendaStartSheetEl('div', 'agsx-def-btn-desc', d.description));
      }
      if (!usable) {
        btn.disabled = true;
        btn.title = d.shadowed
          ? 'shadowed by a personal definition of the same name'
          : (d.reason || 'invalid definition');
      } else {
        if (d.advisories && d.advisories.length) btn.title = d.advisories.join('; ');
        btn.addEventListener('click', () => { entry = d; selectedName = d.name; renderSelection(); });
      }
      seg.appendChild(btn);
    }
    renderSelection();
  };
  renderPicker();
  agendaFetchDefinitionCatalog(() => {
    if (agendaAutomationSheetOpen) renderPicker();
  });
  agendaPresentStartSheet(host, panel, anchor);
}

async function agendaAutomationSheetSubmit(form) {
  const entry = form.entry();
  const showError = (message) => {
    form.error.textContent = message;
    form.error.hidden = false;
  };
  form.error.hidden = true;
  if (!entry) {
    showError('Pick a definition first.');
    return;
  }
  const kind = agendaDefinitionKind(entry);
  // Overrides the stamp op accepts for this kind — prefills into the
  // ordinary manifest intake, never around it. Workflows take none
  // (per-node executors and edges are the definition's, v1).
  const overrides = {};
  if (kind !== 'workflow') {
    // Explicit executor picks only — untouched selects inherit (the
    // start sheet's exact assembly, so both lanes speak one vocabulary).
    const spec = form.configState && form.configState.spec;
    const agentConfig = {};
    if (form.configState && form.configState.backendOverride) {
      agentConfig.agent = form.configState.backendOverride;
    }
    if (spec && spec.backend && spec.backend !== 'internal') {
      const model = form.configState.modelSel ? form.configState.modelSel.value : '';
      const effort = form.configState.effortSel ? form.configState.effortSel.value : '';
      if (model) agentConfig[spec.modelKey] = model;
      if (effort) agentConfig[spec.effortKey] = effort;
      if ((model || effort) && !agentConfig.agent) agentConfig.agent = spec.backend;
    }
    if (Object.keys(agentConfig).length) overrides.agent_config = agentConfig;
  }
  if (kind === 'action') {
    const fireAt = form.fire.value ? new Date(form.fire.value).getTime() : NaN;
    if (!Number.isFinite(fireAt) || fireAt <= Date.now()) {
      showError('Pick a first-run time in the future.');
      form.fire.focus();
      return;
    }
    overrides.fire_at_ms = fireAt;
    overrides.suspend_after = Math.max(1, Number(form.suspend.value) || 3);
    const everyMs = Number(form.cadence.value);
    if (Number.isFinite(everyMs) && everyMs > 0) overrides.every_ms = everyMs;
  }
  form.stampBtn.disabled = true;
  try {
    // THE stamp gesture — the only place this sheet stamps. One
    // daemon-side op: the daemon reads, validates, and SEALS the
    // definition, parks the instance graph, and proposes per node.
    // Parks + proposes ONLY; approval stays the owner's per-effect act
    // (the card, or the one-gesture sheet for a workflow's N nodes).
    const stamped = await agendaDefinitionStamp(entry,
      form.project ? form.project.value.trim() : '', overrides);
    agendaCloseAutomationSheet();
    if (kind === 'workflow') {
      agendaWorkflowOpenApprovalSheet(stamped);
      return;
    }
    // Land the owner on the ordinary Approve affordance.
    const landId = stamped.nodes && stamped.nodes[0] && stamped.nodes[0].item
      ? stamped.nodes[0].item.id : null;
    if (landId && typeof agendaOpenInspector === 'function') agendaOpenInspector(landId);
    if (typeof showControlToast === 'function') {
      showControlToast('success', kind === 'triggered'
        ? 'Stamped — sealed, parked, and proposed. Approve the digest on the card to arm the standing action.'
        : 'Stamped — sealed, parked, and proposed. Approve the digest on the card to arm the series.');
    }
  } catch (e) {
    showError(String(e && e.message || e));
  } finally {
    form.stampBtn.disabled = false;
  }
}
