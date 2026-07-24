// Agenda tab surface (redesign slice A): the lens bar, compose bar, card
// list, and footer ledger. Builds its DOM inside the existing #tab-agenda
// container (the shell fragment stays untouched; the legacy static markup
// is replaced at boot). Data + derivations live in ui2-agenda.js; the
// inspector, sheets, and reminder-policy popover in
// ui2-agenda-inspector.js. Same module, so cross-fragment function calls
// are plain hoisted calls; module-level lets stay within their fragment.
//
// Every item-authored string renders through escapeHtml; ask preview HTML
// renders only inside sandboxed srcdoc iframes (agendaHydratePreviewFrames).

// ---- Lens registry ----
// The extensible seam for later slices: a lens is {id, label, groups()}.
// The graph (constellation) and plan (Upcoming horizon) lenses land as
// follow-up slices by ADDING entries here — nothing else changes.
const AGENDA_LENSES = [
  { id: 'now', label: 'Needs you', groups: () => agendaLensGroupsNow() },
  { id: 'open', label: 'Open', groups: () => agendaLensGroupsOpen() },
  { id: 'hubs', label: 'By hub', groups: () => agendaLensGroupsHubs() },
  { id: 'questions', label: 'Questions', groups: () => agendaLensGroupsQuestions() },
  { id: 'archive', label: 'Archive', groups: () => agendaLensGroupsArchive() },
];

const AGENDA_COMPOSE_PLACEHOLDERS = {
  task: 'Park a task — one actionable line; details can follow in the item…',
  note: 'Park a note — an idea, a decision, anything worth keeping…',
  question: 'Park a question — non-blocking; answer it whenever…',
};

let agendaComposeKind = 'task';

// ---- Scaffold ----

function agendaEnsureScaffold() {
  const pane = document.getElementById('tab-agenda');
  if (!pane || document.getElementById('ag2-root')) return;
  pane.innerHTML = `
  <div class="ag2" id="ag2-root">
    <main class="ag2-main" id="ag2-main">
      <div class="ag2-inner">
        <div class="ag2-head">
          <div>
            <h2 class="ag2-title">Agenda</h2>
            <p class="ag2-sub">Parked intent that outlives any one session — one ledger for this daemon, every project.</p>
          </div>
          <button type="button" class="ag2-bell" id="ag2-bell" title="Reminder delivery policy — owner authority (settings.manage)">
            ${typeof ui2Icon === 'function' ? ui2Icon('bell', 14) : ''}<span>Reminders</span><span class="ag2-bell-dot" id="ag2-bell-dot" hidden title="Quiet hours are active now"></span>
          </button>
        </div>
        <div class="ag2-compose">
          <div class="ag2-seg" id="ag2-kind-seg" role="group" aria-label="Kind">
            <button type="button" data-kind="task" class="active">Task</button>
            <button type="button" data-kind="note">Note</button>
            <button type="button" data-kind="question">Question</button>
          </div>
          <input id="ag2-compose-title" type="text" maxlength="500" autocomplete="off"
                 placeholder="${escapeHtml(AGENDA_COMPOSE_PLACEHOLDERS.task)}" aria-label="New agenda item title" />
          <select id="ag2-compose-due" aria-label="Reminder"
                  title="A due time delivers a reminder to you — it never authorizes work">
            <option value="">No reminder</option>
            <option value="3h">Remind in 3 hours</option>
            <option value="eve">This evening 18:00</option>
            <option value="tom">Tomorrow 09:00</option>
            <option value="mon">Next Monday 09:00</option>
          </select>
          <button type="button" class="ag2-park" id="ag2-park">Park</button>
        </div>
        <div class="ag2-lensbar">
          <div class="ag2-seg ag2-lenses" id="ag2-lenses" role="tablist" aria-label="Agenda lens"></div>
          <span class="ag2-spacer"></span>
          <button type="button" class="ag2-fchip" id="ag2-f-blocked"
                  title="Open items with an uncleared blocker or unmet prerequisite — derived at render, never stored"></button>
          <button type="button" class="ag2-fchip" id="ag2-f-frontier"
                  title="The un-triaged frontier: open items newer than the last triage summary, or unplaced with no triage note — the triage mandate’s scope">frontier</button>
          <input id="ag2-search" class="ag2-search" type="text" autocomplete="off"
                 placeholder="Search the ledger — press /" aria-label="Search the agenda" />
        </div>
        <div class="ag2-notice" id="ag2-notice" hidden></div>
        <div id="ag2-groups"></div>
        <div class="ag2-ledger" id="ag2-ledger"></div>
      </div>
    </main>
    <div class="ag2-inspector-backdrop" id="ag2-inspector-backdrop" hidden></div>
    <aside class="ag2-inspector" id="ag2-inspector" aria-label="Agenda item inspector"></aside>
  </div>`;
  agendaWireScaffold();
}

function agendaWireScaffold() {
  const kindSeg = document.getElementById('ag2-kind-seg');
  const title = document.getElementById('ag2-compose-title');
  kindSeg.querySelectorAll('button[data-kind]').forEach((btn) => {
    btn.addEventListener('click', () => {
      agendaComposeKind = btn.dataset.kind;
      kindSeg.querySelectorAll('button').forEach((b) =>
        b.classList.toggle('active', b === btn));
      title.placeholder = AGENDA_COMPOSE_PLACEHOLDERS[agendaComposeKind]
        || AGENDA_COMPOSE_PLACEHOLDERS.task;
    });
  });
  document.getElementById('ag2-park').addEventListener('click', agendaComposePark);
  title.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      agendaComposePark();
    }
  });
  document.getElementById('ag2-search').addEventListener('input', (e) => {
    agendaSearch = e.target.value;
    agendaRenderTab();
  });
  document.getElementById('ag2-f-blocked').addEventListener('click', () => {
    agendaFilterBlocked = !agendaFilterBlocked;
    agendaRenderTab();
  });
  document.getElementById('ag2-f-frontier').addEventListener('click', () => {
    agendaFilterFrontier = !agendaFilterFrontier;
    agendaRenderTab();
  });
  document.getElementById('ag2-bell').addEventListener('click', (e) => {
    e.stopPropagation();
    agendaBellToggle();
  });
  const groups = document.getElementById('ag2-groups');
  groups.addEventListener('click', agendaGroupsClick);
  groups.addEventListener('input', agendaGroupsInput);
  groups.addEventListener('keydown', agendaGroupsKeydown);
  // Tab-scoped keyboard: '/' focuses search, 'n' the composer, Escape
  // closes overlays inspector-last. Skips typing contexts; the approval
  // rail's y/n shortcuts live on the Activity tab so there is no overlap.
  document.addEventListener('keydown', (e) => {
    if (!agendaTabVisible()) return;
    if (e.key === 'Escape') {
      // The start-now sheet owns its own Escape handler — never chain past
      // it into the inspector on the same keypress.
      const startSheet = document.getElementById('agenda-start-sheet');
      if (startSheet && !startSheet.hidden) return;
      if (agendaSheetClose() || agendaBellClose() || agendaCloseInspector()) {
        e.preventDefault();
      }
      return;
    }
    const t = e.target;
    const tag = (t && t.tagName) || '';
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT'
      || (t && t.isContentEditable)) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === '/') {
      e.preventDefault();
      document.getElementById('ag2-search')?.focus();
    } else if (e.key === 'n') {
      e.preventDefault();
      document.getElementById('ag2-compose-title')?.focus();
    }
  });
}

// Compose-bar reminder presets → an absolute due instant (ms).
function agendaDuePresetMs(value) {
  const now = new Date();
  if (value === '3h') return Date.now() + 3 * 36e5;
  if (value === 'eve') {
    const d = new Date();
    d.setHours(18, 0, 0, 0);
    if (d.getTime() <= Date.now()) d.setDate(d.getDate() + 1);
    return d.getTime();
  }
  if (value === 'tom') {
    const d = new Date(now);
    d.setDate(d.getDate() + 1);
    d.setHours(9, 0, 0, 0);
    return d.getTime();
  }
  if (value === 'mon') {
    const d = new Date(now);
    d.setHours(9, 0, 0, 0);
    do { d.setDate(d.getDate() + 1); } while (d.getDay() !== 1);
    return d.getTime();
  }
  return null;
}

async function agendaComposePark() {
  const title = document.getElementById('ag2-compose-title');
  const due = document.getElementById('ag2-compose-due');
  const btn = document.getElementById('ag2-park');
  const text = (title.value || '').trim();
  if (!text) {
    agendaFlashError('Give it a one-line title first.');
    title.focus();
    return;
  }
  const params = { op: 'add', kind: agendaComposeKind, title: text };
  const dueMs = agendaDuePresetMs(due.value);
  if (dueMs) params.due_ms = dueMs;
  const ok = await agendaSendOp(params, btn);
  if (ok) {
    title.value = '';
    due.value = '';
    title.focus();
    if (typeof showControlToast === 'function') {
      showControlToast('success', `Parked${dueMs ? ' — reminder set' : ''}.`);
    }
  }
}

// ---- Lens group computation ----

function agendaSearchMatch(item, q) {
  if (!q) return true;
  return String(item.title || '').toLowerCase().includes(q)
    || String(item.body || '').toLowerCase().includes(q)
    || (item.tags || []).some((t) => String(t).toLowerCase().includes(q))
    || String(item.id || '').toLowerCase().includes(q);
}

// The un-triaged frontier — the triage mandate's declared scope: open
// items newer than the newest triage summary (`triage:summary` tag), or
// unplaced with no triage annotation; summaries themselves excluded. A
// render-side convention over ordinary data, like the rank parse.
function agendaFrontierPredicate() {
  const newestSummary = Math.max(0, ...(agendaItems || [])
    .filter((x) => (x.tags || []).includes('triage:summary'))
    .map((x) => (x.provenance && x.provenance.created_ms) || 0));
  return (x) => x.status === 'open'
    && !(x.tags || []).includes('triage:summary')
    && (((x.provenance && x.provenance.created_ms) || 0) > newestSummary
      || (!x.part_of && !(x.annotations || []).some((a) => a.source === 'triage')));
}

function agendaFilteredPool() {
  const q = agendaSearch.trim().toLowerCase();
  let pool = (agendaItems || []).filter((item) => agendaSearchMatch(item, q));
  if (agendaFilterBlocked) pool = pool.filter((item) => agendaItemIsBlocked(item));
  if (agendaFilterFrontier) pool = pool.filter(agendaFrontierPredicate());
  return pool;
}

function agendaByNew(a, b) {
  const am = (a.provenance && a.provenance.created_ms) || 0;
  const bm = (b.provenance && b.provenance.created_ms) || 0;
  if (bm !== am) return bm - am;
  return a.id < b.id ? 1 : -1;
}

// The Attend ordering: ranked ascending first (the triage mandate's
// declared "rank N" convention, parsed in agendaTriageInfo), unranked
// after, ties newest-updated first.
function agendaAttendOrder(a, b) {
  const ra = agendaTriageInfo(a);
  const rb = agendaTriageInfo(b);
  const ka = ra && ra.rank !== null ? ra.rank : Infinity;
  const kb = rb && rb.rank !== null ? rb.rank : Infinity;
  if (ka !== kb) return ka - kb;
  return (b.updated_ms || 0) - (a.updated_ms || 0);
}

function agendaLensGroupsNow() {
  const pool = agendaFilteredPool();
  const seen = new Set();
  const take = (arr) => arr.filter((x) => !seen.has(x.id) && (seen.add(x.id), true));
  const answer = take(pool
    .filter((x) => x.kind === 'question' && x.status === 'open' && !x.dismissed)
    .sort(agendaByNew));
  const approve = take(pool.filter((x) => {
    const st = agendaEffectState(x);
    return x.status === 'open' && st && st.kind === 'pending';
  }));
  const suspended = take(pool.filter((x) => {
    const st = agendaEffectState(x);
    return st && st.kind === 'suspended';
  }));
  const overdue = take(pool.filter((x) =>
    x.status === 'open' && x.due_ms && x.due_ms < Date.now()));
  const attend = take(pool
    .filter((x) => x.status === 'open' && agendaTriageInfo(x))
    .sort(agendaAttendOrder));
  const groups = [];
  if (answer.length) {
    groups.push({
      label: 'Answer',
      hint: 'parked questions — nothing blocks, nothing expires; answering resolves them everywhere',
      rows: answer.map((x) => ({ item: x, composer: true })),
    });
  }
  if (approve.length) {
    groups.push({
      label: 'Approve',
      hint: 'proposed session manifests — nothing fires without you; approval binds the exact digest',
      rows: approve.map((x) => ({ item: x })),
    });
  }
  if (suspended.length) {
    groups.push({
      label: 'Suspended',
      hint: 'standing runs stopped after repeated failures — surfaced, never silently re-fired',
      rows: suspended.map((x) => ({ item: x })),
    });
  }
  if (overdue.length) {
    groups.push({
      label: 'Overdue',
      hint: 'reminders that already fired — still just notifications, never work orders',
      rows: overdue.map((x) => ({ item: x })),
    });
  }
  if (attend.length) {
    groups.push({
      label: 'Attend',
      hint: 'triage-flagged items, ranked — ordinary annotations from the triage mandate; ranking gates nothing',
      rows: attend.map((x) => ({ item: x })),
    });
  }
  return groups;
}

function agendaLensGroupsOpen() {
  const items = agendaFilteredPool()
    .filter((x) => x.status === 'open')
    .sort(agendaByNew);
  if (!items.length) return [];
  return [{
    label: 'Open items',
    hint: 'newest first — the flat lens; filing under hubs never hides anything here',
    rows: items.map((x) => ({ item: x, composer: true })),
  }];
}

function agendaLensGroupsHubs() {
  const q = agendaSearch.trim().toLowerCase();
  const frontier = agendaFrontierPredicate();
  const hubs = (agendaItems || [])
    .filter((x) => x.status !== 'retired' && agendaChildrenOf(x.id).length)
    .sort((a, b) => agendaChildrenOf(b.id).length - agendaChildrenOf(a.id).length);
  const groups = hubs.map((hub) => {
    const kids = agendaChildrenOf(hub.id)
      .filter((x) => x.status !== 'retired' && agendaSearchMatch(x, q))
      .filter((x) => !agendaFilterBlocked || agendaItemIsBlocked(x))
      .filter((x) => !agendaFilterFrontier || frontier(x))
      .sort((a, b) =>
        (a.status === 'open' ? -1 : 1) - (b.status === 'open' ? -1 : 1)
        || agendaByNew(a, b));
    const open = kids.filter((k) => k.status === 'open').length;
    return {
      label: hub.title,
      hint: `${open} open · ${kids.length - open} done — roll-ups derived at render`,
      hubId: hub.id,
      rows: kids.map((x) => ({ item: x, noHub: true, composer: true })),
    };
  }).filter((g) => g.rows.length);
  const unfiled = agendaFilteredPool()
    .filter((x) => x.status === 'open' && !x.part_of && !agendaChildrenOf(x.id).length)
    .sort(agendaByNew);
  if (unfiled.length) {
    groups.push({
      label: 'Not filed',
      hint: 'no placement yet — file from an item’s Organization section, or let the triage mandate do it',
      rows: unfiled.map((x) => ({ item: x, composer: true })),
    });
  }
  return groups;
}

function agendaLensGroupsQuestions() {
  const pool = agendaFilteredPool();
  const open = pool
    .filter((x) => x.kind === 'question' && x.status === 'open')
    .sort(agendaByNew);
  const answered = pool
    .filter((x) => x.kind === 'question' && x.status === 'done')
    .sort(agendaByNew);
  const groups = [];
  if (open.length) {
    groups.push({
      label: 'Open questions',
      hint: 'dismissed ones stay here — only an answer resolves a question',
      rows: open.map((x) => ({ item: x, composer: true })),
    });
  }
  if (answered.length) {
    groups.push({
      label: 'Answered',
      hint: 'resolved questions — structured breakdowns live in each item’s panel',
      rows: answered.map((x) => ({ item: x, showAnswer: true })),
    });
  }
  return groups;
}

function agendaLensGroupsArchive() {
  const pool = agendaFilteredPool();
  const done = pool
    .filter((x) => x.status === 'done')
    .sort((a, b) => (b.completed_ms || 0) - (a.completed_ms || 0));
  const retired = pool.filter((x) => x.status === 'retired').sort(agendaByNew);
  const groups = [];
  if (done.length) {
    groups.push({
      label: 'Done',
      hint: 'reopen resurrects — completing cancelled any pending reminder',
      rows: done.map((x) => ({ item: x, showAnswer: true })),
    });
  }
  if (retired.length) {
    groups.push({
      label: 'Retired',
      hint: 'hidden, never deleted — there is no destructive delete on this ledger',
      rows: retired.map((x) => ({ item: x })),
    });
  }
  return groups;
}

// Distinct items the "Needs you" lens would show — the lens badge.
function agendaNeedsYouCount() {
  const needs = new Set();
  (agendaItems || []).forEach((x) => {
    const st = agendaEffectState(x);
    if (x.kind === 'question' && x.status === 'open' && !x.dismissed) needs.add(x.id);
    if (x.status === 'open' && st && st.kind === 'pending') needs.add(x.id);
    if (st && st.kind === 'suspended') needs.add(x.id);
    if (x.status === 'open' && x.due_ms && x.due_ms < Date.now()) needs.add(x.id);
    if (x.status === 'open' && agendaTriageInfo(x)) needs.add(x.id);
  });
  return needs.size;
}

// ---- Chips ----

// One presentation chip. `tone` maps to the token families (green, amber,
// rose, sky, iris, neutral); `dashed` renders the outline-only variant.
function agendaChipHtml(label, tone, tip, dashed) {
  const cls = ['ag2-chip'];
  if (tone) cls.push(`t-${tone}`);
  if (dashed) cls.push('dashed');
  return `<span class="${cls.join(' ')}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(label)}</span>`;
}

function agendaCardChips(item) {
  const chips = [];
  const st = agendaEffectState(item);
  if (item.status === 'done') {
    chips.push(agendaChipHtml(`done ${agendaRelTime(item.completed_ms || item.updated_ms)}`,
      'green', 'Completed — reopen any time'));
  }
  if (item.status === 'retired') {
    chips.push(agendaChipHtml('retired', 'neutral', 'Hidden from open lenses; history preserved'));
  }
  if (item.status === 'open' && item.due_ms) {
    const overdue = item.due_ms < Date.now();
    chips.push(agendaChipHtml(
      overdue ? `overdue ${agendaRelTime(item.due_ms).replace(' ago', '')}` : `due ${agendaRelTime(item.due_ms)}`,
      overdue ? 'amber' : 'sky',
      `Reminder ${agendaAbsTime(item.due_ms)} — delivery follows your policy`));
  }
  if (agendaItemIsBlocked(item)) {
    chips.push(agendaChipHtml('blocked', 'rose',
      'Derived at render — an uncleared blocker or unmet prerequisite'));
  }
  if (item.kind === 'question' && item.status === 'open' && item.dismissed) {
    chips.push(agendaChipHtml('dismissed · still open', 'neutral',
      agendaDismissedTip(item.dismissed), true));
  }
  if (item.answer && item.answer.delivered === false) {
    chips.push(agendaChipHtml('answered · awaiting pickup', 'sky',
      'No live session heard the answer — the asker’s successor reads it at session start', true));
  }
  if (st) {
    if (st.kind === 'running') {
      chips.push(agendaChipHtml('running', 'iris', 'An occurrence is in flight'));
    } else if (st.kind === 'pending') {
      chips.push(agendaChipHtml('needs approval', 'amber',
        'A proposed manifest — nothing fires without an owner approval of its exact digest'));
    } else if (st.kind === 'suspended') {
      chips.push(agendaChipHtml('suspended', 'amber',
        `${st.effect.consecutive_failures} failures in a row — re-approve to re-arm`));
    } else if (st.kind === 'standing') {
      chips.push(agendaChipHtml(`standing · every ${agendaCadenceLabel(st.rec.every_ms)}`, 'green',
        `One approval covers the series · next ${agendaAbsTime(st.next)}`));
    } else if (st.kind === 'armed') {
      chips.push(agendaChipHtml('armed', 'sky', `Fires ${agendaAbsTime(st.next)}`));
    }
  }
  const kids = agendaChildrenOf(item.id);
  if (kids.length) {
    const open = kids.filter((k) => k.status === 'open').length;
    const blocked = kids.filter((k) => agendaItemIsBlocked(k)).length;
    chips.push(agendaChipHtml(
      `hub · ${open} open${blocked ? ` · ${blocked} blocked` : ''}`,
      'neutral',
      'A hub is just an item with children — grouping never hides or blocks anything'));
    if (item.status === 'done' && open) {
      chips.push(agendaChipHtml('done over open children', 'amber',
        'Render-level flag only — completion never cascades'));
    }
  }
  const triage = agendaTriageInfo(item);
  if (triage) {
    chips.push(agendaChipHtml(
      triage.rank !== null ? `triage #${triage.rank}` : 'triage',
      'iris', triage.text, true));
  }
  const mustReads = (item.refs || []).filter((r) => r.must_read).length;
  if (mustReads) {
    chips.push(agendaChipHtml(`${mustReads} must-read`, 'iris',
      'Typed pointers the reading agent weighs — never orders'));
  }
  return chips.join('');
}

// ---- Attribution line ----

function agendaCardByline(item, opts) {
  const p = item.provenance || {};
  const s = agendaSessionInfo(p.session_id);
  const tip = [
    p.principal ? `principal ${p.principal}` : 'principal —',
    p.kind || 'unattributed',
    p.session_id ? `session ${p.session_id}` : '',
  ].filter(Boolean).join(' · ');
  let by;
  if (s && s.key) {
    const label = s.name || `session ${String(s.conversation_id || p.session_id).slice(0, 8)}`;
    by = `by <a href="#sessions" class="agenda-session-link" data-session-key="${escapeHtml(s.key)}" title="${escapeHtml(tip)}">${escapeHtml(label)}</a>`;
  } else if (p.session_id) {
    by = `by <span title="${escapeHtml(tip)}">${escapeHtml(`session ${p.session_id.slice(0, 12)}…`)}</span>`;
  } else {
    const label = p.kind === 'dashboard' ? 'you'
      : p.source ? p.source
        : p.kind === 'local_process' ? 'local shell'
          : p.kind === 'peer' ? 'a peer daemon' : (p.kind || 'unattributed');
    by = `by <span title="${escapeHtml(tip)}">${escapeHtml(label)}</span>`;
  }
  const selfDesc = p.source
    ? '<span class="agenda-self-described" title="Self-described label — unverified, never an identity">· self-described</span>'
    : '';
  const bits = [
    `<span class="ag2-kind">${escapeHtml(item.kind)}</span>`,
    by,
    selfDesc,
    `<span>· ${escapeHtml(agendaRelTime(p.created_ms))}</span>`,
  ];
  const hub = item.part_of && agendaFindItem(item.part_of.parent_id);
  if (hub && !opts.noHub) {
    bits.push(`<span>· in</span> <a class="ag2-hub-link" data-open-item="${escapeHtml(hub.id)}">${escapeHtml(hub.title.length > 34 ? `${hub.title.slice(0, 33)}…` : hub.title)}</a>`);
  }
  (item.tags || []).slice(0, 3).forEach((tag) => {
    bits.push(`<span class="ag2-tag">${escapeHtml(tag)}</span>`);
  });
  return bits.filter(Boolean).join(' ');
}

// ---- Inline effect strip (pending / suspended / running, open items) ----

function agendaCardEffectStrip(item) {
  const st = agendaEffectState(item);
  if (!st || item.status !== 'open') return '';
  if (!['pending', 'suspended', 'running'].includes(st.kind)) return '';
  const e = st.effect;
  let line = '';
  let tone = 'amber';
  let actions = '';
  const id = escapeHtml(item.id);
  if (st.kind === 'pending') {
    const proposer = e.proposed_kind === 'dashboard'
      ? 'You proposed'
      : `“${agendaActorLabel({ session_id: e.proposed_session_id, kind: e.proposed_kind, principal: e.proposed_principal }) || 'a session'}” proposes`;
    line = `${proposer}: runs ${agendaAbsTime(st.manifest.fire_at_ms)}`
      + (st.rec ? ` · every ${agendaCadenceLabel(st.rec.every_ms)}` : ' · once')
      + ' — needs your approval';
    actions = `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${escapeHtml(e.digest || '')}" title="Binds this exact manifest digest — any edit voids it">Approve</button>`
      + `<button type="button" class="ag2-btn ghost" data-open-item="${id}">Review</button>`;
  } else if (st.kind === 'suspended') {
    line = `Standing run suspended after ${e.consecutive_failures} failures — never silently re-fired`;
    actions = `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${escapeHtml(e.digest || '')}" title="Re-approve the unchanged digest — resets the streak">Re-arm</button>`
      + `<button type="button" class="ag2-btn ghost" data-open-item="${id}">Review</button>`;
  } else {
    tone = 'iris';
    line = `Running now — started ${agendaRelTime(e.last_run.at_ms)}`;
    const run = e.last_run;
    const s = run.session_id && agendaSessionInfo(run.session_id);
    actions = s && s.key
      ? `<button type="button" class="ag2-btn ghost" data-jump-session="${escapeHtml(s.key)}">Watch</button>`
      : '';
  }
  return `<div class="ag2-eff t-${tone}">
    <span class="ag2-eff-line">${escapeHtml(line)}</span>
    <span class="ag2-spacer"></span>${actions}
  </div>`;
}

// ---- Inline question answering ----

function agendaQaPicks(itemId, qi) {
  const per = agendaQaSel[itemId];
  return (per && per[qi]) || [];
}

function agendaQaTogglePick(item, qi, label) {
  if (item.status !== 'open') return;
  const q = item.ask && item.ask.questions[qi];
  if (!q) return;
  // Real pick bounds (UserQuestion::pick_bounds): explicit pick_min/max
  // win; otherwise one, or any-number under multi_select.
  const optionCount = (q.options || []).length;
  const defaultMax = q.multi_select ? Math.max(1, optionCount) : 1;
  let max = Math.max(1, q.pick_max || defaultMax);
  if (optionCount > 0) max = Math.min(max, optionCount);
  const per = agendaQaSel[item.id] || (agendaQaSel[item.id] = {});
  const picks = per[qi] ? [...per[qi]] : [];
  const at = picks.indexOf(label);
  if (at >= 0) picks.splice(at, 1);
  else if (max === 1) { picks.length = 0; picks.push(label); }
  else if (picks.length < max) picks.push(label);
  else return; // at the bound — an explicit deselect must come first
  per[qi] = picks;
  agendaRenderTab();
  agendaInspectorRender();
}

function agendaQaPillsHtml(item, qi) {
  const q = item.ask && item.ask.questions[qi];
  if (!q || !(q.options || []).length) return '';
  const picks = agendaQaPicks(item.id, qi);
  const answered = item.answer && item.answer.structured
    ? (item.answer.structured.selections || {})[q.question] || null
    : null;
  const pills = (q.options || []).map((o) => {
    const on = answered ? answered.includes(o.label) : picks.includes(o.label);
    return `<button type="button" class="ag2-pill${on ? ' on' : ''}${answered ? ' recorded' : ''}"
      data-pill-item="${escapeHtml(item.id)}" data-pill-q="${qi}" data-pill-label="${escapeHtml(o.label)}"
      ${o.description ? ` title="${escapeHtml(o.description)}"` : ''}>${escapeHtml(o.label)}</button>`;
  });
  return `<div class="ag2-pills">${pills.join('')}</div>`;
}

// Preview thumbnails for one question — the "show, then ask" cards.
// SECURITY INVARIANT (same as the question rail): agent-authored html
// renders ONLY inside `<iframe sandbox="allow-scripts">` populated via
// srcdoc from an authenticated fetch — opaque origin, never same-origin,
// never blob:/createObjectURL. Bytes come from the agenda blob store
// route the payload names.
function agendaPreviewStripHtml(item, qi, ctx) {
  const q = item.ask && item.ask.questions[qi];
  const previews = (q && q.previews) || [];
  if (!previews.length) return '';
  const picks = agendaQaPicks(item.id, qi);
  const answered = item.answer && item.answer.structured
    ? (item.answer.structured.selections || {})[q.question] || null
    : null;
  const cards = previews.map((p, pi) => {
    const isOption = (q.options || []).some((o) => o.label === p.label);
    const selected = isOption && (answered ? answered.includes(p.label) : picks.includes(p.label));
    const tone = answered ? 'green' : 'iris';
    let media;
    if (p.kind === 'html' && p.url) {
      media = `<iframe class="ag2-prev-frame" sandbox="allow-scripts" referrerpolicy="no-referrer"
        title="${escapeHtml(p.label || 'preview')}" data-preview-url="${escapeHtml(p.url)}" tabindex="-1"></iframe>`;
    } else if (p.kind === 'image' && p.url) {
      media = `<img class="ag2-prev-img" loading="lazy" src="${escapeHtml(p.url)}" alt="${escapeHtml(p.label || 'preview')}" />`;
    } else if (p.kind === 'text' && p.content) {
      media = `<pre class="ag2-prev-text">${escapeHtml(p.content)}</pre>`;
    } else {
      media = `<span class="ag2-prev-missing">${escapeHtml(p.label || 'preview')} unavailable</span>`;
    }
    const state = selected ? (answered ? 'picked' : 'selected') : '';
    const tip = isOption
      ? (item.status === 'open' ? 'Click to pick this option — expand for the full render'
        : 'The rendered prototype this option shipped with')
      : 'A reference render, for contrast';
    return `<div class="ag2-prev${selected ? ` sel t-${tone}` : ''}" data-prev-item="${escapeHtml(item.id)}" data-prev-q="${qi}" data-prev-i="${pi}" title="${escapeHtml(tip)}">
      <div class="ag2-prev-media">${media}</div>
      <div class="ag2-prev-cap">
        <span class="ag2-prev-label${selected ? ' sel' : ''}">${escapeHtml(p.label || `#${pi + 1}`)}</span>
        ${state ? `<span class="ag2-prev-state">· ${state}</span>` : ''}
        <span class="ag2-spacer"></span>
        <button type="button" class="ag2-prev-expand" data-prev-expand="${pi}"
          title="Full size, in a sheet">expand ›</button>
      </div>
    </div>`;
  });
  return `<div class="ag2-prevs${ctx === 'insp' ? ' insp' : ''}">${cards.join('')}</div>`;
}

// Hydrate sandboxed preview frames after an innerHTML render: fetch the
// blob once (cached per url) and hand it to srcdoc. A failed fetch
// degrades to a named unavailable chip, never a broken card.
const agendaPreviewHtmlCache = new Map();
function agendaHydratePreviewFrames(root) {
  root.querySelectorAll('iframe.ag2-prev-frame[data-preview-url]').forEach((frame) => {
    const url = frame.dataset.previewUrl;
    if (!url || frame.dataset.loaded) return;
    frame.dataset.loaded = '1';
    const cached = agendaPreviewHtmlCache.get(url);
    if (cached !== undefined) {
      frame.srcdoc = cached;
      return;
    }
    fetch(url)
      .then((r) => (r.ok ? r.text() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((html) => {
        agendaPreviewHtmlCache.set(url, html);
        frame.srcdoc = html;
      })
      .catch(() => {
        const chip = document.createElement('span');
        chip.className = 'ag2-prev-missing';
        chip.textContent = 'preview unavailable (blob deleted from the store)';
        frame.replaceWith(chip);
      });
  });
}

// The card composer for open questions: pills + previews (first question
// inline — the inspector carries the full multi-question form), one
// note/answer input, Answer + Later.
function agendaCardQaHtml(item) {
  if (item.kind !== 'question' || item.status !== 'open') return '';
  const id = escapeHtml(item.id);
  const hasAsk = !!(item.ask && Array.isArray(item.ask.questions) && item.ask.questions.length);
  const pills = hasAsk ? agendaQaPillsHtml(item, 0) : '';
  const previews = hasAsk ? agendaPreviewStripHtml(item, 0, 'card') : '';
  const more = hasAsk && item.ask.questions.length > 1
    ? `<div class="ag2-qa-more">+ ${item.ask.questions.length - 1} more question${item.ask.questions.length > 2 ? 's' : ''} in the panel — open the item</div>`
    : '';
  const draft = agendaQaDrafts[item.id] || '';
  const placeholder = hasAsk
    ? 'Add a note with your pick (optional)…'
    : 'Type your answer — it lands on the item and reaches the asking session…';
  const later = hasAsk && !item.dismissed
    ? `<button type="button" class="ag2-btn ghost" data-later="${id}" title="Clears it from every rail now — the question stays open here; only an answer resolves it">Later</button>`
    : '';
  return `<div class="ag2-qa">
    ${pills}${previews}${more}
    <div class="ag2-qa-row">
      <input type="text" class="ag2-qa-input" maxlength="4000" data-qa-draft="${id}" data-fkey="qa:${id}"
             placeholder="${escapeHtml(placeholder)}" aria-label="Answer" value="${escapeHtml(draft)}" />
      <button type="button" class="ag2-btn prim" data-answer="${id}">Answer</button>
      ${later}
    </div>
  </div>`;
}

// Build the structured resolution from the shared pick/note/draft state —
// the same wire shapes the question rail records (AgendaAskResolution:
// answers/selections/followups keyed by question text, annotations as
// {preview, note} anchored to a picked card's label).
function agendaBuildStructuredAnswer(item) {
  const questions = (item.ask && item.ask.questions) || [];
  const draft = (agendaQaDrafts[item.id] || '').trim();
  if (!questions.length) {
    if (!draft) return { error: 'Type an answer first.' };
    return { text: draft, structured: null };
  }
  const answers = {};
  const selections = {};
  const followups = {};
  const annotations = {};
  const parts = [];
  questions.forEach((q, qi) => {
    const picks = agendaQaPicks(item.id, qi);
    if (picks.length) {
      selections[q.question] = [...picks];
      answers[q.question] = picks.join(', ');
      parts.push((q.header ? `${q.header}: ` : '') + picks.join(', '));
    }
    const note = (agendaQaNotes[`${item.id}:${qi}`] || '').trim();
    if (note && picks.length) {
      annotations[q.question] = [{ preview: picks[0], note }];
      parts.push(`note on “${picks[0]}”: ${note}`);
    }
  });
  if (draft) {
    followups[questions[0].question] = draft;
    parts.push(draft);
  }
  if (!parts.length) return { error: 'Pick an option or type an answer first.' };
  const structured = { answers, selections, followups };
  if (Object.keys(annotations).length) structured.annotations = annotations;
  return { text: parts.join(' — '), structured };
}

async function agendaSubmitAnswer(item, button) {
  const built = agendaBuildStructuredAnswer(item);
  if (built.error) {
    agendaFlashError(built.error);
    return false;
  }
  const params = { op: 'answer', id: item.id, text: built.text };
  if (built.structured) params.structured = built.structured;
  const ok = await agendaSendOp(params, button);
  if (ok) {
    delete agendaQaSel[item.id];
    delete agendaQaDrafts[item.id];
    Object.keys(agendaQaNotes).forEach((k) => {
      if (k.startsWith(`${item.id}:`)) delete agendaQaNotes[k];
    });
    if (typeof showControlToast === 'function') {
      showControlToast('success', 'Answer recorded on the item.');
    }
    agendaRenderAll();
  }
  return ok;
}

// "Later" on an ask-backed question: the rail's own skip verb — the
// daemon records the dismissal (the item stays open; only an answer
// resolves it). Plain questions have no rail card, so no Later.
function agendaDismissAsk(item) {
  if (!item.ask || !item.ask.ask_id) return;
  if (typeof dispatchControlMsg !== 'function') return;
  dispatchControlMsg({ action: 'skip', id: item.ask.ask_id });
  // If the rail panel is currently showing this ask, clear it like its
  // own Skip button would — the daemon-side dismissal is the record.
  if (typeof pendingQuestion !== 'undefined' && pendingQuestion
    && pendingQuestion.id === item.ask.ask_id) {
    if (typeof clearPendingQuestion === 'function') clearPendingQuestion();
    if (typeof hidePanel === 'function') hidePanel('question-panel');
  }
  if (typeof showControlToast === 'function') {
    showControlToast('info', 'Cleared from the rails — the question stays open here.');
  }
}

// ---- Card ----

function agendaCtlHtml(item) {
  const id = escapeHtml(item.id);
  if (item.kind === 'question' && item.status === 'open') {
    return `<button type="button" class="ag2-ctl q" data-ctl="${id}" title="Open question — answering resolves it">?</button>`;
  }
  if (item.status === 'open') {
    return `<button type="button" class="ag2-ctl open" data-ctl="${id}" title="Mark done"></button>`;
  }
  if (item.status === 'done') {
    return `<button type="button" class="ag2-ctl done" data-ctl="${id}" title="Reopen">✓</button>`;
  }
  return `<button type="button" class="ag2-ctl retired" data-ctl="${id}" title="Reopen (retired)"></button>`;
}

function agendaCardHtml(row) {
  const item = row.item;
  const opts = row;
  const id = escapeHtml(item.id);
  const blockedLine = agendaBlockedLine(item);
  const answerLine = opts.showAnswer && item.answer && item.answer.text
    ? `<div class="ag2-ansline">${escapeHtml(item.answer.text.length > 180 ? `${item.answer.text.slice(0, 180)}…` : item.answer.text)}</div>`
    : '';
  const qa = opts.composer ? agendaCardQaHtml(item) : '';
  const classes = ['ag2-card'];
  if (agendaSelId === item.id) classes.push('selected');
  if (item.status === 'retired') classes.push('retired');
  return `<div class="${classes.join(' ')}" data-item-id="${id}" role="button" tabindex="0">
    ${agendaCtlHtml(item)}
    <div class="ag2-card-main">
      <div class="ag2-card-titlerow">
        <span class="ag2-card-title${item.status === 'done' ? ' done' : ''}${item.status !== 'open' ? ' dim' : ''}">${escapeHtml(item.title)}</span>
        ${agendaCardChips(item)}
      </div>
      <div class="ag2-card-meta">${agendaCardByline(item, opts)}</div>
      ${blockedLine ? `<div class="ag2-blocked-line">${escapeHtml(blockedLine)}</div>` : ''}
      ${agendaCardEffectStrip(item)}
      ${qa}${answerLine}
    </div>
    <span class="ag2-card-chev" aria-hidden="true">›</span>
  </div>`;
}

// ---- Tab render ----

function agendaRenderTab() {
  const pane = document.getElementById('tab-agenda');
  if (!pane) return;
  agendaEnsureScaffold();
  const groupsHost = document.getElementById('ag2-groups');
  if (!groupsHost) return;

  // Lens tabs + filter chips reflect state.
  const lensesHost = document.getElementById('ag2-lenses');
  const needs = agendaNeedsYouCount();
  lensesHost.innerHTML = AGENDA_LENSES.map((lens) => {
    const label = lens.id === 'now' && needs
      ? `${lens.label} · ${needs}` : lens.label;
    return `<button type="button" role="tab" data-lens="${lens.id}"
      aria-selected="${agendaLens === lens.id}" class="${agendaLens === lens.id ? 'active' : ''}">${escapeHtml(label)}</button>`;
  }).join('');
  lensesHost.querySelectorAll('button[data-lens]').forEach((btn) => {
    btn.addEventListener('click', () => {
      agendaLens = btn.dataset.lens;
      agendaRenderTab();
    });
  });
  const blockedBtn = document.getElementById('ag2-f-blocked');
  const nBlocked = (agendaItems || []).filter((x) => agendaItemIsBlocked(x)).length;
  blockedBtn.textContent = `blocked · ${nBlocked}`;
  blockedBtn.classList.toggle('on-rose', agendaFilterBlocked);
  const frontierBtn = document.getElementById('ag2-f-frontier');
  frontierBtn.classList.toggle('on-iris', agendaFilterFrontier);
  const searchBox = document.getElementById('ag2-search');
  if (searchBox.value !== agendaSearch) searchBox.value = agendaSearch;
  const bellDot = document.getElementById('ag2-bell-dot');
  if (bellDot) bellDot.hidden = !agendaQuietNow();

  // Ledger + load/loading states.
  const ledger = document.getElementById('ag2-ledger');
  if (agendaLoadError) {
    groupsHost.innerHTML = `<div class="ui-empty">${escapeHtml(agendaLoadError)}</div>`;
    ledger.textContent = '';
    return;
  }
  if (agendaItems === null) {
    groupsHost.innerHTML = '<div class="ui-empty">Loading…</div>';
    ledger.textContent = '';
    return;
  }
  const skipped = agendaSkippedLines > 0
    ? ` · ${agendaSkippedLines} newer-build line${agendaSkippedLines === 1 ? '' : 's'} preserved unfolded (an older binary never destroys history it can’t read)`
    : '';
  ledger.textContent = `agenda.jsonl · append-only op log · ${agendaCounts.open || 0} open · ${agendaCounts.done || 0} done · ${agendaCounts.retired || 0} retired${skipped}`;

  const lens = AGENDA_LENSES.find((l) => l.id === agendaLens) || AGENDA_LENSES[0];
  const groups = lens.groups();
  if (!groups.length) {
    const filtered = agendaSearch.trim() || agendaFilterBlocked || agendaFilterFrontier;
    const title = filtered ? 'Nothing matches'
      : agendaLens === 'now' ? 'Nothing needs you' : 'Nothing here yet';
    const hint = filtered
      ? 'Loosen the search or filters — retire hides nothing from them.'
      : agendaLens === 'now'
        ? 'The agenda is quiet — everything parked is either moving or waiting politely.'
        : 'Park something above, or let your sessions park as they work.';
    groupsHost.innerHTML = `<div class="ag2-empty">
      <div class="ag2-empty-glyph">◍</div>
      <div class="ag2-empty-title">${escapeHtml(title)}</div>
      <div class="ag2-empty-hint">${escapeHtml(hint)}</div>
    </div>`;
    return;
  }
  agendaRenderPreservingFocus(groupsHost, () => {
    groupsHost.innerHTML = groups.map((group) => {
      const hubLink = group.hubId
        ? `<a class="ag2-hub-open" data-open-item="${escapeHtml(group.hubId)}">open the hub ›</a>`
        : '';
      return `<div class="ag2-group">
        <div class="ag2-group-head">
          <span class="ag2-group-label">${escapeHtml(group.label)}</span>
          <span class="ag2-group-hint">${escapeHtml(group.hint)}</span>
          ${hubLink}
        </div>
        <div class="ag2-cards">${group.rows.map(agendaCardHtml).join('')}</div>
      </div>`;
    }).join('');
  });
  agendaHydratePreviewFrames(groupsHost);
}

// ---- List event delegation (wired once on #ag2-groups) ----

function agendaGroupsClick(e) {
  const sessionLink = e.target.closest('a.agenda-session-link');
  if (sessionLink) {
    e.preventDefault();
    agendaJumpToSession(sessionLink.dataset.sessionKey);
    return;
  }
  const ctl = e.target.closest('[data-ctl]');
  if (ctl) {
    const item = agendaFindItem(ctl.dataset.ctl);
    if (!item) return;
    if (item.kind === 'question' && item.status === 'open') {
      agendaOpenInspector(item.id);
    } else if (item.status === 'open') {
      agendaSendOp({ op: 'complete', id: item.id }, ctl);
    } else {
      agendaSendOp({ op: 'reopen', id: item.id }, ctl);
    }
    return;
  }
  const opBtn = e.target.closest('[data-op-btn]');
  if (opBtn) {
    const params = { op: opBtn.dataset.opBtn, id: opBtn.dataset.id };
    // Approve binds the digest of the revision this render showed.
    if (opBtn.dataset.digest) params.digest = opBtn.dataset.digest;
    agendaSendOp(params, opBtn);
    return;
  }
  const jump = e.target.closest('[data-jump-session]');
  if (jump) {
    agendaJumpToSession(jump.dataset.jumpSession);
    return;
  }
  const pill = e.target.closest('.ag2-pill');
  if (pill) {
    const item = agendaFindItem(pill.dataset.pillItem);
    if (item && !(item.answer && item.answer.structured)) {
      agendaQaTogglePick(item, Number(pill.dataset.pillQ), pill.dataset.pillLabel);
    }
    return;
  }
  const expand = e.target.closest('[data-prev-expand]');
  if (expand) {
    const card = expand.closest('[data-prev-item]');
    if (card) {
      agendaOpenPreviewSheet(card.dataset.prevItem,
        Number(card.dataset.prevQ), Number(expand.dataset.prevExpand));
    }
    return;
  }
  const prev = e.target.closest('[data-prev-item]');
  if (prev) {
    agendaPreviewCardClick(prev);
    return;
  }
  const answerBtn = e.target.closest('[data-answer]');
  if (answerBtn) {
    const item = agendaFindItem(answerBtn.dataset.answer);
    if (item) agendaSubmitAnswer(item, answerBtn);
    return;
  }
  const laterBtn = e.target.closest('[data-later]');
  if (laterBtn) {
    const item = agendaFindItem(laterBtn.dataset.later);
    if (item) agendaDismissAsk(item);
    return;
  }
  const openItem = e.target.closest('[data-open-item]');
  if (openItem) {
    agendaOpenInspector(openItem.dataset.openItem);
    return;
  }
  const card = e.target.closest('.ag2-card');
  if (card && !e.target.closest('button, a, input, select, iframe')) {
    agendaOpenInspector(card.dataset.itemId);
  }
}

// A preview card body click: picking when the card mirrors an option and
// the question is open, expanding otherwise (matches the rail).
function agendaPreviewCardClick(cardEl) {
  const item = agendaFindItem(cardEl.dataset.prevItem);
  const qi = Number(cardEl.dataset.prevQ);
  const pi = Number(cardEl.dataset.prevI);
  if (!item || !item.ask) return;
  const q = item.ask.questions[qi];
  const p = q && (q.previews || [])[pi];
  if (!p) return;
  const isOption = (q.options || []).some((o) => o.label === p.label);
  if (isOption && item.status === 'open' && !(item.answer && item.answer.structured)) {
    agendaQaTogglePick(item, qi, p.label);
  } else {
    agendaOpenPreviewSheet(item.id, qi, pi);
  }
}

function agendaGroupsInput(e) {
  const draft = e.target.closest('[data-qa-draft]');
  if (draft) agendaQaDrafts[draft.dataset.qaDraft] = draft.value;
}

function agendaGroupsKeydown(e) {
  if (e.key === 'Enter') {
    const draft = e.target.closest('[data-qa-draft]');
    if (draft) {
      e.preventDefault();
      const item = agendaFindItem(draft.dataset.qaDraft);
      if (item) agendaSubmitAnswer(item);
      return;
    }
    const card = e.target.closest('.ag2-card');
    if (card && e.target === card) {
      e.preventDefault();
      agendaOpenInspector(card.dataset.itemId);
    }
  }
}
