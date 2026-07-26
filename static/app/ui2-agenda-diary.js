// Agenda tab Diary lens: the whole ledger's append-only op log as a
// day-grouped narrative — what happened, day by day, attributed. The
// daemon-wide sibling of the inspector hood's per-item History
// (ui2-agenda-hood.js): same served envelopes, same verb/dot/who
// vocabulary (agendaHoodOpVerb / agendaHoodOpDot / agendaHoodOpWho —
// ONE presentation vocabulary, never a second map), same honesty rules
// (unparseable lines render as preserved, never hidden). Registered in
// AGENDA_LENSES (ui2-agenda-cards.js) as a custom-surface lens:
// render() owns #ag2-groups, deactivate() stops the drift timer.
//
// Data: the TAIL of GET /api/agenda/ops (tunnel twin api_agenda_ops) —
// a 1-op probe reads log_len, then pages from max(0, log_len − WINDOW)
// to the end, so the newest ops are always present and a long-lived
// ledger never streams whole. The window cap renders honestly in the
// footer ("the last N of L ops"). Freshness derives from the items the
// tab already holds: every fold op bumps its item's updated_ms and the
// agenda_changed lane merges it, so the cache keys on (item count, max
// updated_ms) and the ordinary render pass refetches exactly when the
// log grew — no polling, no background work. Actor filters are
// client-side over the loaded window.
//
// Every op field, actor string, and title interpolated here renders
// through escapeHtml — op-log content is DATA, never markup and never
// instructions. Titles resolve through agendaFindItem (retired items
// are served, so nearly all resolve); rows for resolvable items carry
// data-open-item and ride the cards fragment's existing #ag2-groups
// click delegation into the inspector.

// Tail window: enough for days of ordinary traffic, small enough to
// land in one or two pages (the route clamps limit to its page cap).
const AGENDA_DIARY_WINDOW = 400;

let agendaDiaryTimer = null;
let agendaDiaryFilter = 'all';
// {stampCount, stampUpdated, loading, error, entries, logLen, truncated}
let agendaDiaryCache = null;

// ---- Freshness stamp (derived from state the tab already holds) ----

function agendaDiaryStamp() {
  let updated = 0;
  (agendaItems || []).forEach((item) => {
    if ((item.updated_ms || 0) > updated) updated = item.updated_ms || 0;
  });
  return { count: (agendaItems || []).length, updated };
}

function agendaDiaryCacheFresh() {
  const cache = agendaDiaryCache;
  if (!cache) return false;
  const stamp = agendaDiaryStamp();
  return cache.stampCount === stamp.count && cache.stampUpdated === stamp.updated;
}

// ---- Tail fetch (probe for log_len, then page the window to the end) ----

async function agendaDiaryFetchTail() {
  const page = async (params) => {
    const resp = await daemonApi.request('api_agenda_ops', params);
    if (!resp.ok || !resp.body || !Array.isArray(resp.body.ops)) {
      throw new Error((resp.body && resp.body.error) || `unavailable (${resp.status})`);
    }
    return resp.body;
  };
  try {
    const probe = await page({ since: 0, limit: 1 });
    const logLen = probe.log_len || 0;
    const start = Math.max(0, logLen - AGENDA_DIARY_WINDOW);
    const entries = [];
    let since = start;
    // Ops appended between the probe and these pages just extend the
    // tail — follow next_since to the served end, bounded like the hood.
    for (let i = 0; i < 4; i++) {
      const body = await page({ since, limit: 500 });
      entries.push(...body.ops);
      if (body.next_since >= body.log_len) {
        return { error: '', entries, logLen: body.log_len, truncated: start > 0 };
      }
      since = body.next_since;
    }
    return { error: '', entries, logLen, truncated: start > 0 };
  } catch (e) {
    return { error: String((e && e.message) || e), entries: [], logLen: 0, truncated: false };
  }
}

function agendaDiaryEnsureData() {
  // An errored fetch counts as fresh for its stamp: retry only when the
  // ledger moves or the lens is re-entered — never a render→fetch loop.
  if (agendaDiaryCacheFresh()) return;
  const stamp = agendaDiaryStamp();
  // Stale-while-revalidate: a ledger move repaints over the previous
  // window, never through a loading flash — `loading` styles only the
  // first, empty-handed fetch.
  const prev = agendaDiaryCache;
  agendaDiaryCache = {
    stampCount: stamp.count,
    stampUpdated: stamp.updated,
    loading: !prev || (prev.loading && !prev.entries.length),
    error: prev ? prev.error : '',
    entries: prev ? prev.entries : [],
    logLen: prev ? prev.logLen : 0,
    truncated: prev ? prev.truncated : false,
  };
  agendaDiaryFetchTail().then((got) => {
    const cache = agendaDiaryCache;
    if (!cache || cache.stampCount !== stamp.count
      || cache.stampUpdated !== stamp.updated) return; // superseded
    agendaDiaryCache = { ...cache, loading: false, ...got };
    if (agendaLens === 'diary' && agendaTabVisible()) agendaRenderTab();
  });
}

// ---- Filters (client-side over the loaded window) ----

const AGENDA_DIARY_FILTERS = [
  ['all', 'All', 'Every op in the loaded window — unreadable lines included'],
  ['owner', 'Your acts', 'Ops the dashboard (you) wrote'],
  ['runs', 'Runs', 'Occurrence records and run requests'],
  ['agents', 'Agents & daemon', 'Ops written by sessions and the daemon itself'],
];

// Ops only the daemon authors — attributed to it even without an actor
// (mirrors agendaHoodOpWho's fallback).
const AGENDA_DIARY_DAEMON_OPS = ['record_occurrence', 'record_ask_delivery'];

function agendaDiaryKeep(entry) {
  if (agendaDiaryFilter === 'all') return true;
  if (entry.unparseable || !entry.known) return false;
  const envelope = entry.op || {};
  const op = envelope.op || {};
  const kind = (envelope.actor && envelope.actor.kind) || '';
  if (agendaDiaryFilter === 'owner') return kind === 'dashboard';
  if (agendaDiaryFilter === 'runs') {
    return op.type === 'record_occurrence' || op.type === 'request_occurrence';
  }
  return kind === 'agent_session' || kind === 'daemon'
    || (!kind && AGENDA_DIARY_DAEMON_OPS.includes(String(op.type || '')));
}

// ---- Day grouping (newest first — the diary is the past) ----

function agendaDiaryDayLabel(ms) {
  const d = new Date(ms);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(yesterday.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return 'Today';
  if (d.toDateString() === yesterday.toDateString()) return 'Yesterday';
  return d.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
}

function agendaDiaryDays() {
  const entries = (agendaDiaryCache ? agendaDiaryCache.entries : [])
    .filter(agendaDiaryKeep)
    .slice()
    .reverse(); // the log serves oldest-first; the diary reads newest-first
  const days = [];
  entries.forEach((entry) => {
    const envelope = entry.unparseable ? {} : (entry.op || {});
    const at = envelope.at_ms || 0;
    const key = at ? new Date(at).toDateString() : 'unreadable';
    let day = days.find((d) => d.key === key);
    if (!day) {
      day = { key, label: at ? agendaDiaryDayLabel(at) : 'Undated', rows: [] };
      days.push(day);
    }
    day.rows.push(entry);
  });
  days.forEach((day) => {
    day.hint = `${day.rows.length} op${day.rows.length === 1 ? '' : 's'}`;
  });
  return days;
}

// ---- Render ----

function agendaDiaryRowHtml(entry) {
  if (entry.unparseable) {
    return `<div class="ag2-plan-row ag2-diary-row">
      <span class="ag2-plan-dot t-neutral" aria-hidden="true"></span>
      <div class="ag2-plan-line">
        <span class="ag2-plan-time">—</span>
        <span class="ag2-diary-verb">unreadable line · preserved in the log</span>
      </div>
    </div>`;
  }
  const envelope = entry.op || {};
  const op = envelope.op || {};
  const verb = entry.known
    ? agendaHoodOpVerb(op, envelope)
    : `op · ${String(op.type || '—')}`;
  const who = agendaHoodOpWho(envelope);
  const dot = agendaHoodOpDot(op, !!entry.known);
  const item = op.id ? agendaFindItem(op.id) : null;
  const title = item ? String(item.title || '') : '';
  const shown = title.length > 46 ? `${title.slice(0, 45)}…` : title;
  const target = item
    ? `<button type="button" class="ag2-diary-item" data-open-item="${escapeHtml(item.id)}" title="Open the item">${escapeHtml(shown)}</button>`
    : (op.id ? `<span class="ag2-diary-item plain" title="Not in the served ledger">${escapeHtml(String(op.id).slice(0, 8))}…</span>` : '');
  const raw = `${String(op.type || '—')} · line #${entry.seq}`;
  return `<div class="ag2-plan-row ag2-diary-row" title="${escapeHtml(raw)}">
    <span class="ag2-plan-dot t-${dot}" aria-hidden="true"></span>
    <div class="ag2-plan-line">
      <span class="ag2-plan-time">${escapeHtml(envelope.at_ms ? agendaPlanHm(envelope.at_ms) : '—')}</span>
      <span class="ag2-diary-verb">${escapeHtml(verb)}</span>
      ${target}
      ${who ? `<span class="ag2-diary-who">${escapeHtml(`by ${who}`)}</span>` : ''}
    </div>
  </div>`;
}

function agendaDiaryDayHtml(day) {
  return `<div class="ag2-plan-day">
    <div class="ag2-plan-day-head">
      <span class="ag2-plan-day-label">${escapeHtml(day.label)}</span>
      <span class="ag2-plan-day-hint">${escapeHtml(day.hint)}</span>
    </div>
    <div class="ag2-plan-rail">${day.rows.map(agendaDiaryRowHtml).join('')}</div>
  </div>`;
}

function agendaDiaryFootHtml() {
  const cache = agendaDiaryCache;
  const windowNote = cache && cache.truncated
    ? ` Showing the last ${cache.entries.length} of ${cache.logLen} ops.`
    : '';
  return `<div class="ag2-plan-foot">Read straight from the append-only log — GET /api/agenda/ops. Nothing here can be edited or deleted; corrections are new ops.${escapeHtml(windowNote)}</div>`;
}

function agendaDiarySegHtml() {
  const buttons = AGENDA_DIARY_FILTERS.map(([id, label, tip]) =>
    `<button type="button" data-diary-filter="${id}" title="${escapeHtml(tip)}"
       class="${agendaDiaryFilter === id ? 'active' : ''}">${escapeHtml(label)}</button>`).join('');
  return `<div class="ag2-diary-bar">
    <div class="ag2-seg ag2-diary-seg" role="group" aria-label="Diary filter">${buttons}</div>
    <span class="ag2-diary-hint">attributed, append-only — dismissals, failures, and revocations included</span>
  </div>`;
}

function agendaDiaryRenderLens(host) {
  agendaDiaryEnsureData();
  const cache = agendaDiaryCache;
  let body;
  if (!cache || cache.loading) {
    body = `<div class="ag2-empty">
      <div class="ag2-empty-glyph">◍</div>
      <div class="ag2-empty-title">Reading agenda.jsonl…</div>
      <div class="ag2-empty-hint">The append-only op log, tail first.</div>
    </div>`;
  } else if (cache.error) {
    body = `<div class="ag2-empty">
      <div class="ag2-empty-glyph">◍</div>
      <div class="ag2-empty-title">The op log is unavailable</div>
      <div class="ag2-empty-hint">${escapeHtml(cache.error)}</div>
    </div>`;
  } else {
    const days = agendaDiaryDays();
    body = days.length
      ? days.map(agendaDiaryDayHtml).join('')
      : `<div class="ag2-empty">
          <div class="ag2-empty-glyph">◍</div>
          <div class="ag2-empty-title">${escapeHtml(agendaDiaryFilter === 'all' ? 'Nothing in the log yet' : 'No ops match this filter')}</div>
          <div class="ag2-empty-hint">${escapeHtml(agendaDiaryFilter === 'all' ? 'Every act on the ledger lands here, attributed.' : 'The loaded window holds none — All shows everything.')}</div>
        </div>`;
  }
  host.innerHTML = `<div class="ag2-diary">
    ${agendaDiarySegHtml()}
    ${body}
    ${agendaDiaryFootHtml()}
  </div>`;
  const seg = host.querySelector('.ag2-diary-seg');
  if (seg) {
    seg.addEventListener('click', (e) => {
      const btn = e.target.closest('[data-diary-filter]');
      if (!btn) return;
      agendaDiaryFilter = btn.dataset.diaryFilter;
      agendaRenderTab();
    });
  }
  agendaDiaryEnsureTimer();
}

function agendaDiaryTeardown() {
  if (agendaDiaryTimer !== null) {
    clearInterval(agendaDiaryTimer);
    agendaDiaryTimer = null;
  }
}

// ---- Drift timer (day labels flip at midnight; same conduit as the
// Upcoming lens — armed only while this lens is the visible surface) ----

function agendaDiaryShouldRun() {
  return agendaLens === 'diary' && !document.hidden && agendaTabVisible();
}

function agendaDiaryEnsureTimer() {
  if (agendaDiaryTimer !== null || !agendaDiaryShouldRun()) return;
  agendaDiaryTimer = setInterval(() => {
    if (!agendaDiaryShouldRun()) {
      agendaDiaryTeardown();
      return;
    }
    agendaRenderTab();
  }, 60000);
}

// ---- Wire (the stop/resume conduit; see the fragment header) ----

{
  const wire = () => {
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        agendaDiaryTeardown();
        return;
      }
      if (agendaLens === 'diary' && agendaTabVisible()) {
        agendaRenderTab();
      }
    });
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}
