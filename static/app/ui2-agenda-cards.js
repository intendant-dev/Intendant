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
// The extensible seam: a lens is {id, label} plus either groups() (the
// shared card-list surface) or render(host) (a custom surface that owns
// #ag2-groups entirely — the graph lens's canvas in ui2-agenda-graph.js,
// the Upcoming timeline in ui2-agenda-plan.js), with an optional
// deactivate() the render pass calls on every lens that is NOT about to
// paint (the graph lens stops its rAF loop there; the plan lens its
// refresh timer). Later lenses land by ADDING entries here — nothing
// else changes.
const AGENDA_LENSES = [
  { id: 'now', label: 'Needs you', groups: () => agendaLensGroupsNow() },
  { id: 'open', label: 'Open', groups: () => agendaLensGroupsOpen() },
  { id: 'hubs', label: 'By hub', groups: () => agendaLensGroupsHubs() },
  {
    id: 'graph',
    label: 'Graph',
    render: (host) => agendaGraphRenderLens(host),
    deactivate: () => agendaGraphTeardown(),
  },
  {
    id: 'plan',
    label: 'Upcoming',
    render: (host) => agendaPlanRenderLens(host),
    deactivate: () => agendaPlanTeardown(),
  },
  {
    id: 'automations',
    label: 'Automations',
    groups: () => agendaLensGroupsAutomations(),
  },
  { id: 'questions', label: 'Questions', groups: () => agendaLensGroupsQuestions() },
  {
    id: 'diary',
    label: 'Diary',
    render: (host) => agendaDiaryRenderLens(host),
    deactivate: () => agendaDiaryTeardown(),
  },
  { id: 'archive', label: 'Archive', groups: () => agendaLensGroupsArchive() },
];

// Deactivate every lens surface except the one about to render — the
// seam that stops the graph lens's animation the moment any other lens
// (or an error/loading state, exceptId null) paints into #ag2-groups.
function agendaLensSurfacesDeactivate(exceptId) {
  AGENDA_LENSES.forEach((lens) => {
    if (lens.deactivate && lens.id !== exceptId) lens.deactivate();
  });
}

const AGENDA_COMPOSE_PLACEHOLDERS = {
  task: 'Park a task — one actionable line; details can follow in the item…',
  note: 'Park a note — an idea, a decision, anything worth keeping…',
  question: 'Park a question — non-blocking; answer it whenever…',
};

let agendaComposeKind = 'task';

// Rich-ask composing (question kind only): the owner builds pick-one /
// pick-many options inline and the compose bar parks `{op:'ask'}` — the
// same durable rich-ask lane sessions use; options render on every
// dashboard's question rail, nothing blocks, nothing expires.
let agendaRichOn = false;
let agendaRichOptions = [];
let agendaRichMulti = false;

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
          <div class="ag2-head-tools">
            <div class="ag2-seg ag2-depth" id="ag2-depth" role="group" aria-label="How much machinery to show">
              <button type="button" data-depth="calm" title="Just what needs you — machinery folded away">Calm</button>
              <button type="button" data-depth="standard" title="The working view">Standard</button>
              <button type="button" data-depth="everything" title="Ids, digests, raw op coordinates — the whole engine room">Everything</button>
            </div>
            <button type="button" class="ag2-bell" id="ag2-bell" title="Reminder delivery policy — owner authority (settings.manage)">
              ${typeof ui2Icon === 'function' ? ui2Icon('bell', 14) : ''}<span>Reminders</span><span class="ag2-bell-dot" id="ag2-bell-dot" hidden title="Quiet hours are active now"></span>
            </button>
          </div>
        </div>
        <div class="ag2-compose">
          <div class="ag2-seg" id="ag2-kind-seg" role="group" aria-label="Kind">
            <button type="button" data-kind="task" class="active">Task</button>
            <button type="button" data-kind="note">Note</button>
            <button type="button" data-kind="question">Question</button>
          </div>
          <input id="ag2-compose-title" type="text" maxlength="500" autocomplete="off"
                 placeholder="${escapeHtml(AGENDA_COMPOSE_PLACEHOLDERS.task)}" aria-label="New agenda item title" />
          <button type="button" class="ag2-rich-toggle" id="ag2-rich-toggle" hidden
                  title="Give the owner options to pick from — the question still parks durably and resolves everywhere">+ options</button>
          <select id="ag2-compose-due" aria-label="Reminder"
                  title="A due time delivers a reminder to you — it never authorizes work">
            <option value="">No reminder</option>
            <option value="3h">Remind in 3 hours</option>
            <option value="eve">This evening 18:00</option>
            <option value="tom">Tomorrow 09:00</option>
            <option value="mon">Next Monday 09:00</option>
          </select>
          <button type="button" class="ag2-park" id="ag2-park">Park</button>
          <div class="ag2-rich-row" id="ag2-rich-row" hidden></div>
        </div>
        <div class="ag2-lensbar">
          <div class="ag2-seg ag2-lenses" id="ag2-lenses" role="tablist" aria-label="Agenda lens"></div>
          <span class="ag2-spacer"></span>
          <button type="button" class="ag2-fchip" id="ag2-f-blocked"
                  title="Open items with an uncleared blocker or unmet prerequisite — derived at render, never stored"></button>
          <button type="button" class="ag2-fchip" id="ag2-f-frontier"
                  title="The un-triaged frontier: open items newer than the last triage summary, or unplaced with no triage note — the triage mandate’s scope">frontier</button>
          <button type="button" class="ag2-fchip" id="ag2-automate"
                  title="Stamp an automation definition — house or personal, action or workflow — sealing it, parking it, and proposing its schedule; you approve each digest on its card">automate…</button>
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

// ---- Rich-ask compose row (persistent scaffold; re-rendered on state) ----

function agendaRichRowRender() {
  const row = document.getElementById('ag2-rich-row');
  const toggle = document.getElementById('ag2-rich-toggle');
  if (!row || !toggle) return;
  const isQuestion = agendaComposeKind === 'question';
  toggle.hidden = !isQuestion;
  toggle.classList.toggle('on', agendaRichOn);
  row.hidden = !isQuestion || !agendaRichOn;
  if (row.hidden) {
    row.innerHTML = '';
    return;
  }
  const chips = agendaRichOptions.map((label, i) =>
    `<span class="ag2-rich-chip">${escapeHtml(label)}<button type="button" data-rich-remove="${i}" title="Remove option">×</button></span>`).join('');
  row.innerHTML = `<span class="ag2-rich-eyebrow">Options</span>
    ${chips}
    <input type="text" maxlength="80" id="ag2-rich-draft" placeholder="Add an option, press Enter…" aria-label="Add an option" />
    <button type="button" id="ag2-rich-multi" class="${agendaRichMulti ? 'on' : ''}" title="Allow picking more than one option">pick many</button>
    <span class="ag2-rich-hint">Parks as a rich ask — options render on every dashboard’s question rail; nothing blocks, nothing expires.</span>`;
}

// The depth seg's active state lives on the persistent scaffold, so it
// syncs directly (wire time and every agendaSetDepth) rather than
// through the groups re-render.
function agendaDepthSyncSeg() {
  const seg = document.getElementById('ag2-depth');
  if (!seg) return;
  seg.querySelectorAll('button[data-depth]').forEach((btn) =>
    btn.classList.toggle('active', btn.dataset.depth === agendaDepth));
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
      agendaRichRowRender();
    });
  });
  document.getElementById('ag2-rich-toggle').addEventListener('click', () => {
    agendaRichOn = !agendaRichOn;
    agendaRichRowRender();
    const draft = document.getElementById('ag2-rich-draft');
    if (draft) draft.focus();
  });
  const richRow = document.getElementById('ag2-rich-row');
  richRow.addEventListener('click', (e) => {
    const remove = e.target.closest('[data-rich-remove]');
    if (remove) {
      agendaRichOptions.splice(Number(remove.dataset.richRemove), 1);
      agendaRichRowRender();
      return;
    }
    if (e.target.closest('#ag2-rich-multi')) {
      agendaRichMulti = !agendaRichMulti;
      agendaRichRowRender();
    }
  });
  richRow.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' || !e.target.closest('#ag2-rich-draft')) return;
    e.preventDefault();
    const draft = e.target;
    const label = (draft.value || '').trim();
    if (!label || agendaRichOptions.includes(label)) return;
    if (agendaRichOptions.length >= 4) {
      agendaFlashError('Four options at most — the rail renders up to four.');
      return;
    }
    agendaRichOptions.push(label);
    draft.value = '';
    agendaRichRowRender();
    const next = document.getElementById('ag2-rich-draft');
    if (next) next.focus();
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
    agendaServerSearchKick();
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
  const automate = document.getElementById('ag2-automate');
  automate.addEventListener('click', (e) => {
    e.stopPropagation();
    agendaOpenAutomationSheet(automate);
  });
  document.getElementById('ag2-bell').addEventListener('click', (e) => {
    e.stopPropagation();
    agendaBellToggle();
  });
  document.getElementById('ag2-depth').addEventListener('click', (e) => {
    const btn = e.target.closest('[data-depth]');
    if (btn) agendaSetDepth(btn.dataset.depth);
  });
  agendaDepthSyncSeg();
  const groups = document.getElementById('ag2-groups');
  groups.addEventListener('click', agendaGroupsClick);
  groups.addEventListener('input', agendaGroupsInput);
  groups.addEventListener('keydown', agendaGroupsKeydown);
  // The status circle's press-and-hold reveal (self-explanation
  // without acting) — pointer type feeds the touch first-tap arm.
  groups.addEventListener('pointerdown', agendaCtlPointerDown);
  groups.addEventListener('pointerup', agendaCtlPointerEnd);
  groups.addEventListener('pointercancel', agendaCtlPointerEnd);
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
  const rich = agendaComposeKind === 'question' && agendaRichOn
    && agendaRichOptions.length > 0;
  const dueMs = agendaDuePresetMs(due.value);
  let params;
  if (rich) {
    // The same durable rich-ask lane sessions use (AgendaCommand::Ask):
    // the daemon mints the item and the rail ask_id; options land on
    // every dashboard's question rail. A due reminder rides a follow-up
    // patch — the ask command deliberately carries no reminder field.
    params = {
      op: 'ask',
      questions: [{
        question: text,
        options: agendaRichOptions.map((label) => ({ label })),
        pick_max: agendaRichMulti ? agendaRichOptions.length : 1,
      }],
    };
  } else {
    params = { op: 'add', kind: agendaComposeKind, title: text };
    if (dueMs) params.due_ms = dueMs;
  }
  const ok = await agendaSendOp(params, btn);
  if (ok) {
    if (rich && dueMs && ok.id) {
      // The reminder rides a follow-up patch on the minted item — the
      // ask command itself deliberately carries no reminder field.
      await agendaSendOp({ op: 'patch', id: ok.id, patch: { due_ms: dueMs } });
    }
    title.value = '';
    due.value = '';
    title.focus();
    if (rich) {
      agendaRichOptions = [];
      agendaRichMulti = false;
      agendaRichOn = false;
      agendaRichRowRender();
    }
    if (typeof showControlToast === 'function') {
      showControlToast('success', rich
        ? `Parked as a rich ask — ${params.questions[0].options.length} options on the question rail.`
        : `Parked${dueMs ? ' — reminder set' : ''}.`);
    }
  }
}

// ---- Lens group computation ----

// Every digest an item owns: manifest digests (one per effect — the
// bytes an Approve gesture signs), recorded approval digests, and
// file-ref digests minted at attach. A citation by any of them must
// resolve to this item.
function agendaItemDigests(item) {
  const out = [];
  (item.effects || []).forEach((e) => {
    if (e.digest) out.push(e.digest);
    if (e.approval && e.approval.digest) out.push(e.approval.digest);
  });
  (item.refs || []).forEach((r) => { if (r.digest) out.push(r.digest); });
  return out;
}

// Track AS S5 — server-reach search. Summaries carry no bodies, so the
// body reach moved to the daemon's `q=` (which also covers digests and
// everything local matching covers): a debounced fetch resolves the
// current query to an id set; local field matching stays as the
// instant-feedback lane and the union renders. A failed/stale server
// pass degrades to local-only — never a blank.
let agendaServerSearch = { q: '', ids: null };
let agendaServerSearchTimer = null;
function agendaServerSearchKick() {
  if (agendaServerSearchTimer) clearTimeout(agendaServerSearchTimer);
  const q = agendaSearch.trim().toLowerCase();
  if (!q) {
    agendaServerSearch = { q: '', ids: null };
    return;
  }
  agendaServerSearchTimer = setTimeout(async () => {
    agendaServerSearchTimer = null;
    try {
      const resp = await daemonApi.request('api_agenda_list', { shape: 'summary', q });
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        // Only adopt if the query is still current (races drop stale).
        if (agendaSearch.trim().toLowerCase() === q) {
          agendaServerSearch = { q, ids: new Set(resp.body.items.map((x) => x.id)) };
          agendaRenderTab();
        }
      }
    } catch (e) {
      console.warn('[agenda] server search failed', e);
    }
  }, 250);
}

function agendaSearchMatch(item, q) {
  if (!q) return true;
  // Server-resolved hits for the live query (body + digest reach).
  if (agendaServerSearch.ids && agendaServerSearch.q === q
    && agendaServerSearch.ids.has(item.id)) {
    return true;
  }
  // Digest-prefix search: >=8 hex chars (case-insensitive — q arrives
  // lowercased) match any digest the item owns, resolving to the
  // owning item exactly like an id search. The 8-char floor sits under
  // AGENDA_DIGEST_SHORT_LEN, so every short digest a surface renders
  // is findable by the characters it shows.
  if (/^[0-9a-f]{8,64}$/.test(q)
    && agendaItemDigests(item).some((d) => String(d).toLowerCase().startsWith(q))) {
    return true;
  }
  return String(item.title || '').toLowerCase().includes(q)
    || String(item.body || '').toLowerCase().includes(q)
    || (item.tags || []).some((t) => String(t).toLowerCase().includes(q))
    || String(item.id || '').toLowerCase().includes(q);
}

// The un-triaged frontier — the triage mandate's declared scope.
// Track AS S5: `frontier` is SERVED (the daemon's serving-seam
// predicate — one implementation replacing the ctl/SPA/triage-definition
// prose triple; ruling §4.4). Event-lane rows age per the Q4 contract.
function agendaFrontierPredicate() {
  return (x) => x.frontier === true;
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
  // The audience split (daemon-derived watched_by): questions an armed
  // automation covers are machinery's inbox, not the owner's — they
  // leave Answer for the FYI-grade Watched group below.
  const openQuestions = pool
    .filter((x) => x.kind === 'question' && x.status === 'open' && !x.dismissed)
    .sort(agendaByNew);
  const answer = take(openQuestions.filter((x) => !x.watched_by));
  const approve = take(pool.filter((x) => {
    const st = agendaEffectState(x);
    return x.status === 'open' && st && st.kind === 'pending';
  }));
  // OWED COMPLETIONS (owner finding #10): the daemon-served
  // classification (`completable` — one predicate drives this class,
  // the lens badge, and the blocked chips' target state; the client
  // never re-derives it). Freshest finished-work first.
  const complete = take(pool
    .filter((x) => x.completable === true)
    .sort((a, b) => (b.updated_ms || 0) - (a.updated_ms || 0)));
  const suspended = take(pool.filter((x) => {
    const st = agendaEffectState(x);
    return st && st.kind === 'suspended';
  }));
  const overdue = take(pool.filter((x) =>
    x.status === 'open' && x.due_ms && x.due_ms < Date.now()));
  const attend = take(pool
    .filter((x) => x.status === 'open' && agendaTriageInfo(x))
    .sort(agendaAttendOrder));
  // Watched questions take LAST: any needs-you-grade state above (a
  // pending approval, a suspension) outranks the FYI grouping. Calm
  // depth folds machinery-audience away entirely.
  const watched = agendaDepthCalm() ? []
    : take(openQuestions.filter((x) => x.watched_by));
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
  if (complete.length) {
    groups.push({
      label: 'Complete',
      hint: 'finished — every planned run completed and self-reported achieved; only your Complete tap remains, and anything waiting on these cards unblocks when it lands',
      rows: complete.map((x) => ({ item: x })),
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
  if (watched.length) {
    groups.push({
      label: 'Watched',
      hint: 'questions an armed automation will pick up — machinery’s inbox, not yours; anything it can’t deliver returns to Answer',
      rows: watched.map((x) => ({ item: x, composer: true })),
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
  // Track AS S6: `children` roll-ups are SERVED against the whole fold,
  // so a hub's totals stay honest while the live window carries only
  // recent closed child rows (owner-ratified By-hub values). The
  // in-feed derivation remains the fallback for rows without the field.
  const hubTotal = (x) => {
    const c = x.children;
    if (c) return (c.open || 0) + (c.done || 0) + (c.retired || 0);
    return agendaChildrenOf(x.id).length;
  };
  const hubs = (agendaItems || [])
    .filter((x) => x.status !== 'retired' && hubTotal(x) > 0)
    .sort((a, b) => hubTotal(b) - hubTotal(a));
  const groups = hubs.map((hub) => {
    const kids = agendaChildrenOf(hub.id)
      .filter((x) => x.status !== 'retired' && agendaSearchMatch(x, q))
      .filter((x) => !agendaFilterBlocked || agendaItemIsBlocked(x))
      .filter((x) => !agendaFilterFrontier || frontier(x))
      .sort((a, b) =>
        (a.status === 'open' ? -1 : 1) - (b.status === 'open' ? -1 : 1)
        || agendaByNew(a, b));
    const c = hub.children;
    const open = c ? (c.open || 0) : kids.filter((k) => k.status === 'open').length;
    const closed = c ? (c.done || 0) + (c.retired || 0) : kids.length - open;
    return {
      label: hub.title,
      hint: `${open} open · ${closed} closed — served roll-up; closed rows shown from the last 14 days`,
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

// Automations (Track AU): every item carrying a session-manifest effect,
// grouped by the effect's derived state. A VIEW over the agenda snapshot
// the tab already holds — no store, no ops, no routes of its own (the G3
// attention-queue ruling verbatim); every action on a row is an existing
// op the cards/inspector already send. Row order inside a group is
// newest-first; the group order is attention-shaped: what needs the owner
// first, then what's suspended, live, armed, and spent.
function agendaLensGroupsAutomations() {
  const q = agendaSearch.trim().toLowerCase();
  const pool = (agendaItems || [])
    .filter((x) => (x.effects || []).length && agendaSearchMatch(x, q))
    .filter((x) => !agendaFilterBlocked || agendaItemIsBlocked(x));
  const staged = { pending: [], suspended: [], running: [], standing: [], ended: [] };
  pool.forEach((item) => {
    const st = agendaEffectState(item);
    if (!st) return;
    if (item.status !== 'open') { staged.ended.push(item); return; }
    if (st.kind === 'pending') staged.pending.push(item);
    else if (st.kind === 'suspended') staged.suspended.push(item);
    else if (st.kind === 'running') staged.running.push(item);
    else if (st.kind === 'standing') {
      // A standing series with no next instant is spent (until/max
      // reached) — the approval is inert, the history remains.
      (st.effect.next_fire_ms ? staged.standing : staged.ended).push(item);
    } else if (st.kind === 'armed') staged.standing.push(item);
    else staged.ended.push(item);
  });
  Object.values(staged).forEach((arr) => arr.sort(agendaByNew));
  const rows = (arr) => arr.map((x) => ({ item: x, automation: true, noHub: true }));
  const groups = [];
  if (staged.pending.length) {
    groups.push({
      label: 'Needs approval',
      hint: 'nothing fires until you approve the digest — approval covers who runs, what, and when',
      rows: rows(staged.pending),
    });
  }
  if (staged.suspended.length) {
    groups.push({
      label: 'Suspended',
      hint: 'failure streak hit the threshold — re-approving the unchanged digest re-arms the series',
      rows: rows(staged.suspended),
    });
  }
  if (staged.running.length) {
    groups.push({
      label: 'Running',
      hint: 'one occurrence at a time — the outcome writes back to the item',
      rows: rows(staged.running),
    });
  }
  if (staged.standing.length) {
    groups.push({
      label: 'Armed',
      hint: 'approved and scheduled — Run now fires one extra occurrence of the approved digest',
      rows: rows(staged.standing),
    });
  }
  if (staged.ended.length) {
    groups.push({
      label: 'Ended',
      hint: 'spent series and finished one-shots — history stays on the item',
      rows: rows(staged.ended),
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

// Track AS S6: the live feed carries only the last 14 days of closed
// items (owner-ratified fixed window); the Archive lens shows those
// recent rows immediately and pages OLDER closed items from the server
// on demand (full grain — answer text renders; nothing preloaded).
function agendaLensGroupsArchive() {
  const q = agendaSearch.trim().toLowerCase();
  const pool = agendaFilteredPool();
  const done = pool
    .filter((x) => x.status === 'done')
    .sort((a, b) => (b.completed_ms || 0) - (a.completed_ms || 0));
  const retired = pool.filter((x) => x.status === 'retired').sort(agendaByNew);
  const groups = [];
  if (done.length) {
    groups.push({
      label: 'Done · last 14 days',
      hint: 'reopen resurrects — completing cancelled any pending reminder',
      rows: done.map((x) => ({ item: x, showAnswer: true })),
    });
  }
  if (retired.length) {
    groups.push({
      label: 'Retired · last 14 days',
      hint: 'hidden, never deleted — there is no destructive delete on this ledger',
      rows: retired.map((x) => ({ item: x })),
    });
  }
  // The paged older archive (server order: newest-closed first). Rows
  // whose item re-entered the live feed (a reopen) defer to the live
  // copy; the local search still applies so filtering stays one gesture.
  const older = agendaArchive.items
    .filter((x) => !agendaFindItem(x.id))
    .filter((x) => agendaSearchMatch(x, q));
  if (older.length) {
    groups.push({
      label: 'Older',
      hint: 'paged from the archive — the ledger keeps everything',
      rows: older.map((x) => ({ item: x, showAnswer: x.status === 'done' })),
    });
  }
  return groups;
}

// Distinct items the "Needs you" lens would show — the lens badge.
// Watched questions (an armed automation covers them — daemon-derived
// watched_by) are machinery's inbox and never count as needing you.
// Owed completions ride the served `completable` classification — the
// same daemon predicate that builds the Complete rail class.
function agendaNeedsYouCount() {
  const needs = new Set();
  (agendaItems || []).forEach((x) => {
    const st = agendaEffectState(x);
    if (x.kind === 'question' && x.status === 'open' && !x.dismissed
      && !x.watched_by) needs.add(x.id);
    if (x.status === 'open' && st && st.kind === 'pending') needs.add(x.id);
    if (x.completable === true) needs.add(x.id);
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

// ---- Blocked chip + per-gate detail (chip honesty) ----
// Derivations live in ui2-agenda.js (agendaBlockedExplain and friends);
// this section renders them. Advisory throughout — the chip and rows
// gate nothing, exactly like the served flag they explain.

// Cards whose in-flight waits the owner tapped open at calm depth
// (render-time presentation state; blockers and delivered waits never
// fold, so they need no entry here).
const agendaBlockedOpen = new Set();

// The blocked chip is a BUTTON (house law: every symbol self-explains
// on tap — no hover-only): the tip names each gate, the tap unfolds
// the per-gate rows under the card at any depth. When every
// unsatisfied prerequisite is served owed-completion the face stops
// crying wolf: 'finished · awaiting Complete' (sky), nothing is
// stuck — only Complete taps are missing.
function agendaBlockedChipHtml(item) {
  const explain = agendaBlockedExplain(item);
  const spec = agendaBlockedChipSpec(explain);
  return `<button type="button" class="ag2-chip ag2-chip-btn t-${spec.tone}" data-blocked-toggle="${escapeHtml(item.id)}"
    title="${escapeHtml(agendaBlockedTip(explain))}">${escapeHtml(spec.label)}</button>`;
}

// One prerequisite gate row: the linked target ("waiting on: <title>" —
// the link opens that card's pane), its live-status chip, and its named
// unblocking gesture — "complete it to proceed" on waits whose release
// is the target's completion, the Review-and-complete door on targets
// the daemon serves as finished-and-awaiting-Complete. In-flight waits
// name no gesture: the run is already doing the work.
function agendaBlockedPrereqRowHtml(p) {
  const chip = agendaChipHtml(p.status, p.tone,
    p.detail || 'Live status, derived at render — advisory; it gates nothing');
  const name = p.target
    ? `<a class="ag2-blk-link" data-open-item="${escapeHtml(p.id)}">waiting on: “${escapeHtml(p.title)}”</a>`
    : `<span>waiting on: ${escapeHtml(p.title)}</span>`;
  const remedy = p.kind === 'waiting' && p.remedy
    ? '<span class="ag2-blk-hint">— complete it to proceed</span>'
    : '';
  const go = p.kind === 'delivered' && p.target
    ? `<button type="button" class="ag2-btn ghost ag2-blk-go" data-open-item="${escapeHtml(p.id)}"
        title="Opens the finished prerequisite — its Mark done (Complete) is the tap that releases this wait">Review &amp; complete ›</button>`
    : '';
  return `<div class="ag2-blk-row${p.kind === 'delivered' ? ' delivered' : ''}">${chip}${name}${remedy}${go}</div>`;
}

// The card's per-gate rows (replacing the old single-line first-gate
// text): every uncleared blocker, every unsatisfied prerequisite with
// its live status, each a door to the prerequisite itself. Fold rules
// live in agendaBlockedDetailPlan; the chip toggles the calm-depth
// fold. Renders from locally visible gates even when the served flag
// is absent (never-summarized event rows), exactly like the old line.
function agendaBlockedDetailHtml(item) {
  if (item.status !== 'open') return '';
  const explain = agendaBlockedExplain(item);
  if (!explain.blockers.length && !explain.prereqs.length && !explain.unexplained) return '';
  const plan = agendaBlockedDetailPlan(explain, agendaDepth, agendaBlockedOpen.has(item.id));
  if (!plan.visible) return '';
  const rows = [];
  explain.blockers.forEach((b) => {
    rows.push(`<div class="ag2-blk-row"><span class="ag2-blk-mark" aria-hidden="true">●</span>
      <span>waiting on: “${escapeHtml(b.criterion)}”
        <span class="ag2-blk-hint">— stated blocker; nothing evaluates it, people do</span></span></div>`);
  });
  plan.rows.delivered.forEach((p) => rows.push(agendaBlockedPrereqRowHtml(p)));
  plan.rows.waits.forEach((p) => rows.push(agendaBlockedPrereqRowHtml(p)));
  if (explain.unexplained) {
    rows.push(`<div class="ag2-blk-row"><span class="ag2-blk-hint">the daemon judges this blocked,
      but no gate is visible in this window — it may have just changed</span></div>`);
  }
  if (plan.rows.hiddenWaits) {
    rows.push(`<button type="button" class="ag2-blk-more" data-blocked-toggle="${escapeHtml(item.id)}">+ ${plan.rows.hiddenWaits} in-flight wait${plan.rows.hiddenWaits === 1 ? '' : 's'} ›</button>`);
  }
  return `<div class="ag2-blocked-detail${explain.allDelivered ? ' delivered' : ''}">${rows.join('')}</div>`;
}

// ---- Attestation chips (Track AO — the Q8 labeling law, binding) ----
// The self-report axis renders BESIDE the transport verdict, never as
// it: every attestation surface says "self-reported" and carries the
// not-verified hover; the ◆ self-report mark is the axis's own glyph,
// never the transport palette's meaning (attested-achieved is sky —
// never the transport-success green or its chip), and
// blocked/abandoned/partial wear amber-class warning hues — never
// rose, which stays transport-failure. Unattested is the DERIVED
// absence state: a hollow neutral ◇ chip, "no self-report" — absence,
// never anomaly styling, never synthesized into achieved.
function agendaAttestTone(outcome) {
  return outcome === 'achieved' ? 'sky' : 'amber';
}

function agendaAttestHover(att) {
  const base = 'The session’s own report — not verified.';
  if (!att || !att.note) return base;
  const note = att.note.length > 200 ? `${att.note.slice(0, 200)}…` : att.note;
  return `${base} “${note}”`;
}

function agendaAttestChipHtml(att) {
  return agendaChipHtml(`◆ self-reported: ${att.outcome}`, agendaAttestTone(att.outcome),
    agendaAttestHover(att));
}

const AGENDA_UNATTESTED_HOVER =
  'No self-report exists for this run — the session ended without attesting. Absence, not failure.';

function agendaUnattestedChipHtml() {
  return agendaChipHtml('◇ no self-report', 'neutral', AGENDA_UNATTESTED_HOVER, true);
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
    chips.push(agendaBlockedChipHtml(item));
  }
  // OWED COMPLETION (served classification, finding #10 — mutually
  // exclusive with blocked by construction): the work is fully
  // finished; only your Complete tap remains. The status circle is the
  // tap; dependents waiting on this card unblock when it lands.
  if (item.status === 'open' && item.completable === true) {
    chips.push(agendaChipHtml('finished — tap Complete', 'sky',
      'Every planned run completed and the session self-reported achieved (not verified). Mark done — the status circle — completes it; anything waiting on this card unblocks.'));
  }
  // Tier-1 PR join chips (render-time only — never stored, never ops):
  // draft state and rename divergence come from the scanner's last
  // list poll; the anchor title stays as parked.
  const pr = agendaPrTier1(item);
  if (pr) {
    if (pr.draft) {
      chips.push(agendaChipHtml('draft', 'neutral',
        `Draft on GitHub — as of ${agendaRelTime(pr.fetched_at_ms)}`));
    }
    if (pr.title && item.title && !item.title.endsWith(pr.title)) {
      chips.push(agendaChipHtml('renamed', 'sky',
        `Now titled on GitHub: ${pr.title}`));
    }
  }
  if (item.kind === 'question' && item.status === 'open' && item.dismissed) {
    chips.push(agendaChipHtml('dismissed · still open', 'neutral',
      agendaDismissedTip(item.dismissed), true));
  }
  // Machinery-audience classification (daemon-derived, never stored):
  // an armed automation covers this item. Absence = needs you.
  if (item.status === 'open' && item.watched_by) {
    const w = item.watched_by;
    chips.push(agendaChipHtml(
      agendaDepthCalm() ? 'watched' : `watched by ${w.watcher_title}`, 'sky',
      w.due_ms
        ? `“${w.watcher_title}” picks this up ${agendaRelTime(w.due_ms)} — returns to Needs you if it can’t deliver`
        : `“${w.watcher_title}” is handling this now — returns to Needs you if it can’t deliver`));
  }
  // Answered ask whose delivery reached no session (the daemon-recorded
  // `delivered: false` marker; absent data claims nothing).
  if (item.status === 'done' && item.ask && item.answer
    && item.answer.delivered === false) {
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
    } else if (st.kind === 'watching') {
      const predicate = `${st.trig.item_kind || 'item'}${(st.trig.tags || []).length ? ` + ${st.trig.tags.join(',')}` : ''}`;
      chips.push(agendaChipHtml(agendaDepthCalm() ? 'watching' : `watching · ${predicate}`, 'sky',
        'Fires when a NEW open item matches — arrivals batch for a minute'));
    } else if (st.kind === 'waiting') {
      chips.push(agendaChipHtml('auto · on unblock', 'sky',
        'Armed — fires the moment every prerequisite completes; no one has to remember', true));
    } else if (st.kind === 'ready') {
      chips.push(agendaChipHtml('fires momentarily', 'iris',
        'Prerequisites complete — the scheduler dispatches within the minute'));
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
  // Machinery chips — folded away at calm depth (status, due, blocked,
  // and attention states above always show).
  const triage = agendaTriageInfo(item);
  if (triage && !agendaDepthCalm()) {
    chips.push(agendaChipHtml(
      triage.rank !== null ? `triage #${triage.rank}` : 'triage',
      'iris', triage.text, true));
  }
  const mustReads = (item.refs || []).filter((r) => r.must_read).length;
  if (mustReads && !agendaDepthCalm()) {
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
      : p.kind === 'daemon' ? 'the daemon'
        : p.source ? p.source
          : p.kind === 'local_process' ? 'local shell'
            : p.kind === 'peer' ? 'a peer daemon' : (p.kind || 'unattributed');
    by = `by <span title="${escapeHtml(tip)}">${escapeHtml(label)}</span>`;
  }
  const selfDesc = p.source && !agendaDepthCalm()
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
  (agendaDepthCalm() ? [] : (item.tags || []).slice(0, 3)).forEach((tag) => {
    bits.push(`<span class="ag2-tag">${escapeHtml(tag)}</span>`);
  });
  return bits.filter(Boolean).join(' ');
}

// ---- Inline effect strip (pending / suspended / running, open items) ----

function agendaCardEffectStrip(item) {
  const st = agendaEffectState(item);
  if (!st || item.status !== 'open') return '';
  if (!['pending', 'suspended', 'running', 'missed'].includes(st.kind)) return '';
  const e = st.effect;
  let line = '';
  let tone = 'amber';
  let actions = '';
  const id = escapeHtml(item.id);
  // The served fireability verdict (the daemon's own validator, decorated
  // per read): Approve/Re-arm is NEVER offered against it — the primary
  // affordance becomes fixing the plan, with the named field focused.
  const refusal = e.fireability_refusal || null;
  const fixBtn = (label) => `<button type="button" class="ag2-btn prim" data-edit-sched="${id}" data-focus="${escapeHtml((refusal && refusal.field) || '')}" title="${escapeHtml((refusal && refusal.reason) || 'Fix the plan')}">${label}</button>`;
  // The recorded "never" beside Approve/Fix-plan (withdraw_effect): the
  // proposal stops soliciting approval NOW; the item, its thread, and
  // any fired history stay — a fresh propose revives the lane anytime.
  const declineBtn = `<button type="button" class="ag2-btn ghost" data-op-btn="withdraw_effect" data-id="${id}" title="Withdraws this proposal — it stops asking for approval; the item and its history stay. Propose again anytime">Decline</button>`;
  if (st.kind === 'pending') {
    const proposer = e.proposed_kind === 'dashboard'
      ? 'You proposed'
      : `“${agendaActorLabel({ session_id: e.proposed_session_id, kind: e.proposed_kind, principal: e.proposed_principal }) || 'a session'}” proposes`;
    line = `${proposer}: runs ${agendaAbsTime(st.manifest.fire_at_ms)}`
      + (st.rec ? ` · every ${agendaCadenceLabel(st.rec.every_ms)}` : ' · once')
      + (refusal ? ` — not fireable as planned: ${refusal.reason}` : ' — needs your approval');
    actions = agendaEffectRevisionChipHtml(e)
      + agendaDigestChipHtml(e.digest, 'Approve binds exactly this manifest revision',
        agendaDigestPulseClass(e.effect_id))
      + (refusal
        ? fixBtn('Fix plan…')
        : `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${escapeHtml(e.digest || '')}" title="Binds this exact manifest digest — any edit voids it">Approve</button>`
          + `<button type="button" class="ag2-btn ghost" data-edit-sched="${id}" data-focus="" title="Small tweaks without ceremony — shape, executor, project, goal, cadence. Saving mints a new digest for you to approve">Edit…</button>`)
      + declineBtn
      + `<button type="button" class="ag2-btn ghost" data-open-item="${id}">Review</button>`;
  } else if (st.kind === 'missed') {
    // The missed-window terminal, self-explaining and carrying its one
    // remedy: the run resolved `missed` (floor passed while the daemon
    // was down), the approval is spent, and one tap re-proposes the
    // SAME plan to run now and re-approves it.
    line = 'Missed its window — the fire time passed while the daemon was down; nothing ran';
    actions = agendaDigestChipHtml(e.digest, 'The manifest revision whose window was missed')
      + `<button type="button" class="ag2-btn prim" data-resched-effect="${id}" title="Re-proposes this exact plan to run now and re-approves it in one tap">Re-approve to reschedule</button>`
      + `<button type="button" class="ag2-btn ghost" data-open-item="${id}">Review</button>`;
  } else if (st.kind === 'suspended') {
    line = `Standing run suspended after ${e.consecutive_failures} failures — never silently re-fired`;
    // The last self-reported reason, when one exists (Track AO): the
    // owner sees WHY it kept failing, not just that it did.
    const rep = e.last_run && e.last_run.attestation;
    if (rep && (rep.outcome === 'blocked' || rep.outcome === 'abandoned')) {
      const repNote = rep.note
        ? ` — ${rep.note.length > 140 ? `${rep.note.slice(0, 140)}…` : rep.note}` : '';
      line += ` · last self-report: ${rep.outcome}${repNote}`;
    }
    actions = agendaDigestChipHtml(e.digest, 'Re-arm re-approves exactly this unchanged manifest revision')
      + (refusal
        ? fixBtn('Fix plan…')
        : `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${escapeHtml(e.digest || '')}" title="Re-approve the unchanged digest — resets the streak">Re-arm</button>`)
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
  const semantic = st.kind === 'running' ? 'is-progress' : 'is-attention';
  return `<div class="ag2-eff t-${tone} ${semantic}">
    <span class="ag2-eff-line">${escapeHtml(line)}</span>
    <span class="ag2-spacer"></span>${actions}
  </div>`;
}

// The Automations lens's per-row strip: the full manifest-effect picture
// regardless of state (the inline strip above surfaces only what needs
// attention on ordinary lenses). Executor · cadence · last occurrence ·
// next instant · streak, then the existing actions — every button is an
// op the generic [data-op-btn] delegation already sends.
function agendaAutomationStripHtml(item) {
  const st = agendaEffectState(item);
  if (!st) return '';
  const e = st.effect;
  const id = escapeHtml(item.id);
  const meta = [];
  if (!agendaDepthCalm()) {
    meta.push(`<span class="ag2-auto-exec" title="Executor — digest-bound like the rest of the manifest: editing it voids the approval">${escapeHtml(agendaExecutorLabel(st.manifest.agent_config))}</span>`);
  }
  meta.push(`<span>${escapeHtml(st.trig
    ? (st.trig.kind === 'on_item_match' ? 'on matching items' : 'on unblock')
    : st.rec ? `every ${agendaCadenceLabel(st.rec.every_ms)}` : 'once')}</span>`);
  const last = e.last_run;
  if (last) {
    // The transport tip stays the transport's own line — state + last
    // words — never labeled or styled as a self-report (R6).
    const tip = `${last.state}${last.note ? ` — ${last.note}` : ''}`;
    const sess = last.session_id && agendaSessionInfo(last.session_id);
    const jump = sess && sess.key
      ? ` <a class="ag2-auto-run" data-jump-session="${escapeHtml(sess.key)}">open run</a>` : '';
    meta.push(`<span class="ag2-auto-last st-${escapeHtml(last.state)}" title="${escapeHtml(tip)}">last: ${escapeHtml(last.state)} ${escapeHtml(agendaRelTime(last.at_ms))}${jump}</span>`);
    if (last.attestation) {
      meta.push(`<span class="ag2-auto-attest att-${escapeHtml(last.attestation.outcome)}" title="${escapeHtml(agendaAttestHover(last.attestation))}">◆ self-reported: ${escapeHtml(last.attestation.outcome)}</span>`);
    } else if (last.state !== 'started') {
      meta.push(`<span class="ag2-auto-attest att-none" title="${escapeHtml(AGENDA_UNATTESTED_HOVER)}">◇ no self-report</span>`);
    }
    if ((e.last_run_attempt || 0) > 0) {
      meta.push(`<span title="A bounded auto-retry of the same spent cause — the cooldown floor spaces attempts, the failure streak suspends the series">attempt ${e.last_run_attempt} (auto-retry)</span>`);
    }
  }
  if ((e.consecutive_failures || 0) > 0 && !st.suspended) {
    meta.push(`<span class="ag2-auto-streak" title="Consecutive failed/unknown outcomes — the series suspends at ${st.threshold}">streak ${e.consecutive_failures}/${st.threshold}</span>`);
  }
  if (st.kind === 'withdrawn') {
    // The husk the daemon keeps for its fired history: the proposal was
    // taken back — nothing pends, nothing fires; the run record above
    // is why the entry still renders at all.
    const w = e.withdrawn || {};
    const wTip = `Withdrawn ${w.kind === 'dashboard' ? 'by you' : `by “${agendaActorLabel(w) || 'a session'}”`}`
      + `${w.reason ? ` — ${w.reason}` : ''} · the reason lives in the item thread; propose again anytime`;
    meta.push(`<span class="ag2-auto-streak" title="${escapeHtml(wTip)}">proposal withdrawn ${escapeHtml(agendaRelTime(w.at_ms || 0))}</span>`);
  } else if (st.suspended) {
    meta.push(`<span class="ag2-auto-streak" title="Suspended — never silently re-fired">suspended after ${e.consecutive_failures}</span>`);
  } else if (e.next_fire_ms) {
    meta.push(`<span title="The planner's real next instant, served with the item">next ${escapeHtml(agendaAbsTime(e.next_fire_ms))}</span>`);
  } else if (st.kind === 'standing') {
    meta.push('<span>series ended</span>');
  }
  // Sealed inputs, list-render cheap: the count comes from the manifest
  // already in hand — drift is judged on the item panel (expand-time,
  // the G1 doctrine), never here.
  const seals = (st.manifest.binding_refs || []).length;
  if (seals) {
    meta.push(`<span class="ag2-auto-sealed" title="Definition sealed under the approval digest — firings execute the sealed revision; drift and the sealed view live on the item panel">sealed ×${seals}</span>`);
  }
  // The manifest digest, always visible where the gesture lives — the
  // one thing depth never folds away, because it is what Approve signs
  // (and what a recorded approval covers once bound).
  const revised = agendaEffectRevisionChipHtml(e);
  if (revised) meta.push(revised);
  meta.push(agendaDigestChipHtml(e.digest,
    st.kind === 'pending' ? 'Approve binds exactly this manifest revision'
      : st.kind === 'suspended' ? 'Re-arm re-approves exactly this unchanged manifest revision'
        : e.approval && e.approval.digest === e.digest
          ? 'Your recorded approval covers exactly this manifest revision'
          : 'The manifest revision this row describes',
    agendaDigestPulseClass(e.effect_id)));
  let actions = '';
  const digest = escapeHtml(e.digest || '');
  // Same fireability gating as the inline strip: a served refusal
  // withholds Approve/Re-arm and offers the focused fix instead.
  const refusal = e.fireability_refusal || null;
  // Same decline lane as the inline strip: the recorded "never" beside
  // Approve/Fix-plan on every unapproved pending row.
  const declineBtn = `<button type="button" class="ag2-btn ghost" data-op-btn="withdraw_effect" data-id="${id}" title="Withdraws this proposal — it stops asking for approval; the item and its history stay. Propose again anytime">Decline</button>`;
  if (refusal && (st.kind === 'pending' || st.kind === 'suspended')) {
    meta.push(`<span class="ag2-auto-unfireable" title="${escapeHtml(refusal.reason)}">not fireable: ${escapeHtml(refusal.field)}</span>`);
    actions = `<button type="button" class="ag2-btn prim" data-edit-sched="${id}" data-focus="${escapeHtml(refusal.field)}" title="${escapeHtml(refusal.reason)}">Fix plan…</button>`
      + (st.kind === 'pending' ? declineBtn : '');
  } else if (st.kind === 'pending') {
    actions = `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${digest}" title="Binds this exact manifest digest — any edit voids it">Approve</button>`
      + `<button type="button" class="ag2-btn ghost" data-edit-sched="${id}" title="Small tweaks without ceremony — saving mints a new digest for you to approve">Edit…</button>`
      + declineBtn;
  } else if (st.kind === 'suspended') {
    actions = `<button type="button" class="ag2-btn prim" data-op-btn="approve_effect" data-id="${id}" data-digest="${digest}" title="Re-approve the unchanged digest — resets the streak">Re-arm</button>`;
  } else if (st.kind === 'missed') {
    actions = `<button type="button" class="ag2-btn prim" data-resched-effect="${id}" title="Re-proposes this exact plan to run now and re-approves it in one tap">Re-approve to reschedule</button>`;
  } else if (st.kind === 'running') {
    const sess = e.last_run && e.last_run.session_id && agendaSessionInfo(e.last_run.session_id);
    actions = sess && sess.key
      ? `<button type="button" class="ag2-btn ghost" data-jump-session="${escapeHtml(sess.key)}">Watch</button>` : '';
  } else if (st.kind === 'standing' && e.next_fire_ms) {
    actions = `<button type="button" class="ag2-btn ghost" data-op-btn="request_occurrence" data-id="${id}" title="One extra occurrence of the approved digest — the standing approval is untouched">Run now</button>`
      + `<button type="button" class="ag2-btn ghost" data-op-btn="revoke_effect" data-id="${id}" title="Withdraws the approval; the manifest and history stay">Revoke</button>`;
  } else if (['armed', 'watching', 'waiting', 'ready'].includes(st.kind)) {
    actions = `<button type="button" class="ag2-btn ghost" data-op-btn="revoke_effect" data-id="${id}" title="Withdraws the approval; the manifest and history stay">Revoke</button>`;
  }
  const autoTone = ['pending', 'suspended', 'missed'].includes(st.kind) ? 'amber'
    : st.kind === 'withdrawn' ? 'neutral'
      : st.kind === 'standing' ? 'green'
        : ['armed', 'watching', 'waiting'].includes(st.kind) ? 'sky' : 'iris';
  const autoSemantic = ['pending', 'suspended', 'missed'].includes(st.kind) ? 'is-attention'
    : ['running', 'ready'].includes(st.kind) ? 'is-progress' : '';
  return `<div class="ag2-eff ag2-auto t-${autoTone}${autoSemantic ? ` ${autoSemantic}` : ''}">
    <span class="ag2-auto-meta">${meta.join('<span class="ag2-auto-dot">·</span>')}</span>
    <span class="ag2-spacer"></span>${actions}
  </div>`;
}

// The explicit "now armed" moment (UX0 ruling; UX2): approving is never
// a silent state flip — one toast says what is now true and what
// happens next, mirroring the workflow sheet's proven pattern. Derives
// from the RESPONSE item, so the line states the daemon's post-approve
// truth, not a prediction.
function agendaApprovalMoment(item) {
  if (typeof showControlToast !== 'function') return;
  const st = agendaEffectState(item);
  let line = 'Approved — the manifest is armed. Revoke anytime on the card.';
  if (st) {
    if (st.kind === 'standing') {
      line = `Approved — standing series armed: every ${agendaCadenceLabel(st.rec.every_ms)}, next ${agendaAbsTime(st.next)}. Revoke anytime on the card.`;
    } else if (st.kind === 'armed') {
      line = `Approved — armed; fires ${agendaAbsTime(st.next)}. Watch it on the Automations lens; revoke anytime.`;
    } else if (st.kind === 'watching') {
      line = 'Approved — watching; fires when a new matching item arrives (arrivals batch for a minute). Revoke anytime.';
    } else if (st.kind === 'waiting') {
      line = 'Approved — armed; fires the moment every prerequisite completes. Revoke anytime.';
    } else if (st.kind === 'ready') {
      line = 'Approved — prerequisites already complete; the session fires within the minute.';
    } else if (st.kind === 'running') {
      line = 'Approved — an occurrence is already in flight.';
    }
  }
  showControlToast('success', line);
}

// ---- Workflow pipeline strip (hub cards + the hubs lens) ----

// A hub reads as a pipeline when at least two children carry
// on_unblock-triggered manifests — the workflow stamp's shape, DERIVED
// from the graph every paint; no workflow-level object or marker exists
// anywhere (Track T's contract).
function agendaPipelineNodes(hub) {
  const kids = agendaChildrenOf(hub.id).filter((k) =>
    (k.effects || []).some((e) => e.manifest && e.manifest.trigger
      && e.manifest.trigger.kind === 'on_unblock'));
  if (kids.length < 2) return null;
  // relies_on topological order within the set (bounded passes; a cycle
  // or cross-hub dependency appends in creation order at the tail).
  const inSet = new Set(kids.map((k) => k.id));
  const order = [];
  const placed = new Set();
  for (let pass = 0; pass < kids.length && order.length < kids.length; pass++) {
    kids.forEach((k) => {
      if (placed.has(k.id)) return;
      const deps = (k.relies_on || []).map((l) => l.target_id).filter((id) => inSet.has(id));
      if (deps.every((id) => placed.has(id))) {
        placed.add(k.id);
        order.push(k);
      }
    });
  }
  kids.forEach((k) => { if (!placed.has(k.id)) order.push(k); });
  return order;
}

function agendaPipelineStripHtml(hub) {
  const nodes = agendaPipelineNodes(hub);
  if (!nodes) return '';
  const cells = nodes.map((node, i) => {
    const st = agendaEffectState(node);
    let tone = 'neutral';
    let state = 'parked';
    let pulse = '';
    if (node.status === 'done') { tone = 'green'; state = 'done'; }
    else if (node.status === 'retired') { state = 'retired'; }
    else if (st) {
      if (st.kind === 'running') { tone = 'iris'; state = 'running'; pulse = 'fast'; }
      else if (st.kind === 'ready') { tone = 'iris'; state = 'fires momentarily'; pulse = 'slow'; }
      else if (st.kind === 'waiting') { tone = 'sky'; state = 'waiting on prerequisites'; }
      else if (st.kind === 'pending') { tone = 'amber'; state = 'needs approval'; }
      else if (st.kind === 'suspended') { tone = 'amber'; state = `suspended · ${st.effect.consecutive_failures} failures`; }
      else { state = st.kind; }
    }
    const exec = st && !agendaDepthCalm() ? agendaExecutorLabel(st.manifest.agent_config) : '';
    const arrow = i ? '<span class="ag2-pipe-arrow" aria-hidden="true">→</span>' : '';
    const title = String(node.title || '');
    return `${arrow}<button type="button" class="ag2-pipe-node" data-open-item="${escapeHtml(node.id)}" title="${escapeHtml(`${title} — ${state}`)}">
      <span class="ag2-pipe-head"><span class="ag2-pipe-dot t-${tone}${pulse ? ` pulse ${pulse}` : ''}"></span><span class="ag2-pipe-title">${escapeHtml(title.length > 22 ? `${title.slice(0, 21)}…` : title)}</span></span>
      <span class="ag2-pipe-state">${escapeHtml(state)}</span>
      ${exec ? `<span class="ag2-pipe-exec">${escapeHtml(exec)}</span>` : ''}
    </button>`;
  });
  return `<div class="ag2-pipeline">${cells.join('')}</div>`;
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
    // The ask-rail's recommendation convention — "(Recommended)" in the
    // label, no separate flag — gets the highlight (decision-card UX).
    const rec = /\(recommended\)\s*$/i.test(o.label);
    return `<button type="button" class="ag2-pill${on ? ' on' : ''}${answered ? ' recorded' : ''}${rec ? ' rec' : ''}"
      data-pill-item="${escapeHtml(item.id)}" data-pill-q="${qi}" data-pill-label="${escapeHtml(o.label)}"
      ${o.description ? ` title="${escapeHtml(o.description)}"` : ''}>${escapeHtml(o.label)}</button>`;
  });
  return `<div class="ag2-pills">${pills.join('')}</div>`;
}

// Preview thumbnails for one question — the "show, then ask" cards.
// SECURITY INVARIANT (same as the question rail): agent-authored html
// renders ONLY inside a sandboxed srcdoc iframe with an opaque origin —
// built exclusively by the shared createSandboxedPreviewFrame factory and
// filled by the shared fetchSandboxedPreviewInto writer (fragment 41; the
// pinned single-writer invariant). This renderer emits placeholder slots;
// agendaHydratePreviewFrames swaps them for real frames.
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
      media = `<span class="ag2-prev-slot" data-preview-url="${escapeHtml(p.url)}"
        data-preview-title="${escapeHtml(p.label || 'preview')}"></span>`;
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

// Hydrate preview slots after an innerHTML render: each slot becomes a
// sandboxed frame from the shared factory (fragment 41 — the pinned
// single-factory/single-writer pattern), filled from the blob store. A
// failed fetch degrades to a named unavailable chip, never a broken card.
function agendaHydratePreviewFrames(root) {
  root.querySelectorAll('.ag2-prev-slot[data-preview-url]').forEach((slot) => {
    const url = slot.dataset.previewUrl;
    if (!url) return;
    const full = slot.dataset.previewFull === '1';
    const frame = createSandboxedPreviewFrame(
      full ? 'ag2-prev-frame full' : 'ag2-prev-frame', slot.dataset.previewTitle);
    slot.replaceWith(frame);
    fetchSandboxedPreviewInto(frame, url, () => {
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

// The status circle's tappable self-explanation (house law: no
// hover-only): a press-and-hold on any pointer, or the first tap on a
// hover-less one, REVEALS the action as a labeled pill beside the
// circle instead of acting; the next tap (circle or pill) acts. Mouse
// clicks keep today's immediate action — the reveal is additive.
let agendaCtlReveal = null; // { id, until } — the one revealed circle
let agendaCtlHoldTimer = null;
let agendaCtlHoldFiredAt = 0;
let agendaCtlPointerType = 'mouse';

function agendaCtlAction(item) {
  if (item.kind === 'question' && item.status === 'open') return null;
  return item.status === 'open' ? 'Mark done' : 'Reopen';
}

function agendaCtlRevealShow(id) {
  agendaCtlReveal = { id, until: Date.now() + 6000 };
  agendaRenderTab();
  setTimeout(() => {
    if (agendaCtlReveal && agendaCtlReveal.until <= Date.now()) {
      agendaCtlReveal = null;
      agendaRenderTab();
    }
  }, 6200);
}

function agendaCtlPointerDown(e) {
  agendaCtlPointerType = e.pointerType || 'mouse';
  const ctl = e.target.closest('.ag2-ctl');
  if (!ctl || !ctl.dataset.ctl) return;
  const item = agendaFindItem(ctl.dataset.ctl);
  if (!item || !agendaCtlAction(item)) return;
  if (agendaCtlHoldTimer) clearTimeout(agendaCtlHoldTimer);
  agendaCtlHoldTimer = setTimeout(() => {
    agendaCtlHoldTimer = null;
    agendaCtlHoldFiredAt = Date.now();
    agendaCtlRevealShow(item.id);
  }, 450);
}

function agendaCtlPointerEnd() {
  if (agendaCtlHoldTimer) {
    clearTimeout(agendaCtlHoldTimer);
    agendaCtlHoldTimer = null;
  }
}

function agendaCtlHtml(item) {
  const id = escapeHtml(item.id);
  if (item.kind === 'question' && item.status === 'open') {
    return `<button type="button" class="ag2-ctl q" data-ctl="${id}" title="Open question — answering resolves it" aria-label="Open question">?</button>`;
  }
  const [cls, glyph, tip] = item.status === 'open'
    ? ['open', '', 'Mark done — completes this item; reopen any time']
    : item.status === 'done'
      ? ['done', '✓', 'Done — tap to reopen']
      : ['retired', '', 'Retired — tap to reopen'];
  const action = agendaCtlAction(item);
  const revealed = agendaCtlReveal && agendaCtlReveal.id === item.id;
  const pill = revealed
    ? `<button type="button" class="ag2-ctl-label" data-ctl="${id}">${escapeHtml(action)}</button>`
    : '';
  return `<button type="button" class="ag2-ctl ${cls}${revealed ? ' revealed' : ''}" data-ctl="${id}" title="${escapeHtml(tip)}" aria-label="${escapeHtml(action)}">${glyph}</button>${pill}`;
}

function agendaCardHtml(row) {
  const item = row.item;
  const opts = row;
  const id = escapeHtml(item.id);
  // Per-gate blocked rows: explicit blockers and delivered-awaiting
  // waits always render; calm depth folds only the genuinely in-flight
  // waits, and the blocked chip's tap unfolds them (agendaBlockedOpen).
  const blockedDetail = agendaBlockedDetailHtml(item);
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
        ${agendaDepthAll() ? `<span class="ag2-ulid" title="ulid prefix — creation-ordered; the full id is on the item panel">${escapeHtml(item.id.slice(0, 10).toLowerCase())}</span>` : ''}
      </div>
      <div class="ag2-card-meta">${agendaCardByline(item, opts)}</div>
      ${blockedDetail}
      ${agendaPipelineStripHtml(item)}
      ${opts.automation ? agendaAutomationStripHtml(item) : agendaCardEffectStrip(item)}
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
      // Lens interaction re-pulls the summaries (throttled) so served
      // cross-item flags on untouched items refresh — the ruled Q4
      // freshness affordance (gate fix F-AS1).
      agendaSummariesRepull('lens');
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

  // Ledger + load/loading states. A failed refresh with data in hand
  // renders the DATA plus a stale notice — never a blank over a live
  // cache (Track AS S3, the abort-resilience bridge: the since_seq
  // healing lane retries on the next gap/wake/reconnect signal).
  const ledger = document.getElementById('ag2-ledger');
  if (agendaLoadError && agendaItems === null) {
    agendaLensSurfacesDeactivate(null);
    groupsHost.innerHTML = `<div class="ui-empty">${escapeHtml(agendaLoadError)}</div>`;
    ledger.textContent = '';
    return;
  }
  if (agendaItems === null) {
    agendaLensSurfacesDeactivate(null);
    groupsHost.innerHTML = '<div class="ui-empty">Loading…</div>';
    ledger.textContent = '';
    return;
  }
  const skipped = agendaSkippedLines > 0
    ? ` · ${agendaSkippedLines} newer-build line${agendaSkippedLines === 1 ? '' : 's'} preserved unfolded (an older binary never destroys history it can’t read)`
    : '';
  const stale = agendaLoadError
    ? ` · last refresh failed (showing last-known data): ${agendaLoadError}`
    : '';
  // Real ops truth from GET /api/agenda/ops (slice D, ui2-agenda-hood.js):
  // the segment renders once fetched; the sync fetches only while this
  // tab is visible and the data signature moved.
  ledger.textContent = `agenda.jsonl · append-only op log · ${agendaCounts.open || 0} open · ${agendaCounts.done || 0} done · ${agendaCounts.retired || 0} retired${agendaLedgerOpsSegment()}${skipped}${stale}`;
  agendaLedgerOpsSync();

  const lens = AGENDA_LENSES.find((l) => l.id === agendaLens) || AGENDA_LENSES[0];
  agendaLensSurfacesDeactivate(lens.id);
  if (lens.render) {
    // Custom-surface lens (the graph): it owns #ag2-groups entirely and
    // manages its own lifecycle from here.
    lens.render(groupsHost);
    return;
  }
  const groups = lens.groups();
  if (!groups.length) {
    const filtered = agendaSearch.trim() || agendaFilterBlocked || agendaFilterFrontier;
    const title = filtered ? 'Nothing matches'
      : agendaLens === 'now' ? 'Nothing needs you'
        : agendaLens === 'automations' ? 'No automations yet' : 'Nothing here yet';
    const hint = filtered
      ? 'Loosen the search or filters — retire hides nothing from them.'
      : agendaLens === 'now'
        ? 'The agenda is quiet — everything parked is either moving or waiting politely.'
        : agendaLens === 'automations'
          ? 'Schedule a standing session on any item (its Schedule section, or ctl agenda schedule) and it appears here with its approval, cadence, and run history. PR cards appear once GitHub is connected and watching repositories — Vault → GitHub.'
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
      const hub = group.hubId ? agendaFindItem(group.hubId) : null;
      return `<div class="ag2-group">
        <div class="ag2-group-head">
          <span class="ag2-group-label">${escapeHtml(group.label)}</span>
          <span class="ag2-group-hint">${escapeHtml(group.hint)}</span>
          ${hubLink}
        </div>
        ${hub ? agendaPipelineStripHtml(hub) : ''}
        <div class="ag2-cards">${group.rows.map(agendaCardHtml).join('')}</div>
      </div>`;
    }).join('') + agendaLensFooterHtml(lens.id);
  });
  agendaHydratePreviewFrames(groupsHost);
  // Archive lens (S6): first activation pulls the first page — the
  // owner-ratified posture is fully paged, nothing preloaded.
  if (lens.id === 'archive' && agendaArchive.next === undefined && !agendaArchive.loading) {
    agendaArchiveLoadMore();
  }
}

// Per-lens footer under the groups (Track AS S6): the Archive lens's
// pager, and the see-archive door on the lenses whose closed rows are
// now windowed to 14 days (owner-ratified Questions/Automations values).
function agendaLensFooterHtml(lensId) {
  if (lensId === 'archive') {
    if (agendaArchive.loading) {
      return '<div class="ag2-group-hint" style="padding:8px 2px;">Loading older items…</div>';
    }
    if (agendaArchive.next === null && agendaArchive.items.length === 0) {
      return '<div class="ag2-group-hint" style="padding:8px 2px;">Nothing older than the last 14 days.</div>';
    }
    if (agendaArchive.next) {
      return '<div style="padding:8px 2px;"><button type="button" class="ag2-btn ghost" data-archive-more>Load older items…</button></div>';
    }
    return agendaArchive.items.length
      ? '<div class="ag2-group-hint" style="padding:8px 2px;">End of the archive — the ledger keeps everything.</div>'
      : '';
  }
  if (lensId === 'questions' || lensId === 'automations') {
    // Closed rows here cover the last 14 days; older history lives in
    // the Archive. Total closed comes from the whole-ledger counts.
    const closedTotal = (agendaCounts.done || 0) + (agendaCounts.retired || 0);
    const closedInFeed = (agendaItems || []).filter((x) => x.status !== 'open').length;
    if (closedTotal > closedInFeed) {
      return `<div class="ag2-group-hint" style="padding:8px 2px;">Closed rows show the last 14 days — <a href="#" data-open-archive>${closedTotal - closedInFeed} older in the Archive ›</a></div>`;
    }
  }
  return '';
}

// ---- List event delegation (wired once on #ag2-groups) ----

function agendaGroupsClick(e) {
  // The click a press-and-hold releases into already did its job (the
  // reveal) — swallow it whole before any branch can act on it.
  if (agendaCtlHoldFiredAt && Date.now() - agendaCtlHoldFiredAt < 800) {
    agendaCtlHoldFiredAt = 0;
    return;
  }
  // S6 footer affordances: the Archive pager and the see-archive door.
  if (e.target.closest('[data-archive-more]')) {
    e.preventDefault();
    agendaArchiveLoadMore();
    return;
  }
  if (e.target.closest('[data-open-archive]')) {
    e.preventDefault();
    agendaLens = 'archive';
    agendaRenderTab();
    return;
  }
  const sessionLink = e.target.closest('a.agenda-session-link');
  if (sessionLink) {
    e.preventDefault();
    agendaJumpToSession(sessionLink.dataset.sessionKey);
    return;
  }
  const ctl = e.target.closest('[data-ctl]');
  // A revealed circle disarms on any click that isn't its own control.
  if (agendaCtlReveal && !(ctl && ctl.dataset.ctl === agendaCtlReveal.id)) {
    agendaCtlReveal = null;
    agendaRenderTab();
  }
  if (ctl) {
    const item = agendaFindItem(ctl.dataset.ctl);
    if (!item) return;
    if (item.kind === 'question' && item.status === 'open') {
      agendaOpenInspector(item.id);
      return;
    }
    // Hover-less pointers: the first tap names the action; the second
    // (circle or pill, while revealed) acts. Hover pointers act at
    // once, exactly as today; keyboard activations (detail 0 — the
    // aria-label already names them) always act.
    const revealed = agendaCtlReveal && agendaCtlReveal.id === item.id
      && agendaCtlReveal.until > Date.now();
    if (agendaCtlPointerType === 'touch' && !revealed && e.detail !== 0) {
      agendaCtlRevealShow(item.id);
      return;
    }
    agendaCtlReveal = null;
    if (item.status === 'open') {
      agendaSendOp({ op: 'complete', id: item.id }, ctl);
    } else {
      agendaSendOp({ op: 'reopen', id: item.id }, ctl);
    }
    return;
  }
  const blockedToggle = e.target.closest('[data-blocked-toggle]');
  if (blockedToggle) {
    const id = blockedToggle.dataset.blockedToggle;
    if (agendaBlockedOpen.has(id)) agendaBlockedOpen.delete(id);
    else agendaBlockedOpen.add(id);
    agendaRenderTab();
    return;
  }
  const opBtn = e.target.closest('[data-op-btn]');
  if (opBtn) {
    const params = { op: opBtn.dataset.opBtn, id: opBtn.dataset.id };
    // Approve binds the digest of the revision this render showed.
    if (opBtn.dataset.digest) params.digest = opBtn.dataset.digest;
    agendaSendOp(params, opBtn).then((item) => {
      if (item && params.op === 'approve_effect') agendaApprovalMoment(item);
    });
    return;
  }
  const editSched = e.target.closest('[data-edit-sched]');
  if (editSched) {
    // The card's edit affordance OPENS the one editor — the schedule
    // sheet — whose save is the one re-propose emitter. No op here.
    // `data-focus` (a served fireability field) lands the sheet on the
    // named broken field.
    agendaOpenSchedSheet(editSched.dataset.editSched,
      editSched.dataset.focus ? { focus: editSched.dataset.focus } : undefined);
    return;
  }
  const resched = e.target.closest('[data-resched-effect]');
  if (resched) {
    // The missed-window card's one-tap remedy: re-propose the SAME plan
    // to run now + re-approve, in the reschedule lane's single emitter
    // (ui2-agenda-inspector.js). No op emitted from the card itself.
    agendaRescheduleMissed(resched.dataset.reschedEffect, resched);
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
      return;
    }
    // Focusable non-card openers (the Upcoming lens's timeline rows):
    // Enter mirrors their click-delegation open.
    const opener = e.target.closest('[data-open-item]');
    if (opener && e.target === opener) {
      e.preventDefault();
      agendaOpenInspector(opener.dataset.openItem);
    }
  }
}
