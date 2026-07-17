/* V3 — Books: the ledgers.
   Costs & usage → the List → scheduled sessions → what the house remembers
   → reports. Every number derives from V3.data (the sessions catalog, the
   agenda poll, provider usage snapshots) — nothing estimated here that the
   daemon didn't already book. The scheduled-sessions card is the first UI
   for the daemon's scheduler: propose is an agenda op, approve is an
   owner-surface act bound to the manifest digest. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.books = {
  title: 'Books',

  _mem: null,   /* last memory search: {q, results, durability, error} */

  clock(ms) { return V3.clock ? V3.clock(ms) : new Date(ms).toLocaleString(); },
  ago(ms) { return V3.norm && V3.norm.ago ? V3.norm.ago(ms) : ''; },

  fmtTokens(n) {
    if (!n) return '0';
    if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
    if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k';
    return String(n);
  },

  render(el) {
    const D = V3.data;
    const sessions = D.sessions || [];

    el.innerHTML = V3.page({
      eyebrow: 'the ledgers',
      title: 'Books',
      sub: 'What the house spent, what’s parked for later, and what it remembers. Every number is the daemon’s own bookkeeping — read off the sessions catalog, never invented here.',
      body:
        V3.section('Costs & usage') +
        this.kpisHtml() +
        '<div class="grid grid-2">' +
          this.byBackendHtml() +
          this.topSessionsHtml() +
        '</div>' +

        V3.section('The List — parked for later') +
        '<div class="card">' +
          '<div id="bks-list">' + this.listHtml() + '</div>' +
          '<div class="row" style="margin-top:10px;padding-top:12px;border-top:1px solid var(--line);flex-wrap:wrap">' +
            '<input class="input" id="bks-add" placeholder="Park a task, note, or question — the house will surface it…" style="flex:1;min-width:220px">' +
            '<select class="input" id="bks-add-kind">' +
              ['task', 'note', 'question'].map(k => '<option value="' + k + '">' + k + '</option>').join('') +
            '</select>' +
            '<button class="btn btn-quiet" id="bks-add-btn">' + V3.ICON('plus', 14) + ' Add</button>' +
          '</div>' +
        '</div>' +

        this.scheduledHtml() +

        V3.section('What the house remembers') +
        '<div class="card">' +
          '<div class="row" style="flex-wrap:wrap">' +
            '<input class="input" id="bks-mem-q" placeholder="Search the memory plane — a word, a name, a decision…" style="flex:1;min-width:240px" value="' + V3.esc(this._mem ? this._mem.q : '') + '">' +
            '<button class="btn btn-primary" id="bks-mem-btn">' + V3.ICON('search', 14) + ' Search</button>' +
          '</div>' +
          '<div id="bks-mem-results" style="margin-top:12px">' + this.memHtml() + '</div>' +
          '<div class="dim" style="margin-top:10px;font-size:12.5px">New claims are proposed through the agent in this draft — tell the house what to remember in the composer, and it lands as a candidate for the house to argue through.</div>' +
        '</div>' +

        V3.section('Reports & history') +
        this.reportsHtml()
    });

    this.wire(el);
  },

  /* ---------- costs & usage ---------- */
  kpisHtml() {
    const D = V3.data;
    const S = D.sessions || [];
    const cost = S.reduce((a, s) => a + (s.cost || 0), 0);
    const unknown = S.filter(s => s.costKnown === false).length;
    const tokens = Object.values(D.usage || {}).reduce((a, u) => a + ((u && u.tokens_used) || 0), 0);
    const working = S.filter(s => s.active).length;
    const kpi = (value, label, sub) =>
      '<div class="card"><div class="bks-kpi-value">' + V3.esc(value) + '</div>' +
      '<div class="bks-kpi-label">' + V3.esc(label) + '</div>' +
      '<div class="dim" style="font-size:12px;margin-top:2px">' + V3.esc(sub) + '</div></div>';
    return '<div class="bks-kpis">' +
      kpi('$' + cost.toFixed(2), 'estimated spend',
        unknown ? 'summed from the sessions catalog · ' + unknown + ' unpriced' : 'summed from the sessions catalog') +
      kpi(String(S.length), 'sessions', 'every session the catalog tracks') +
      kpi(this.fmtTokens(tokens), 'tokens', 'as the providers reported them') +
      kpi(String(working), 'working now', 'live on stage') +
    '</div>';
  },

  byBackendHtml() {
    const S = V3.data.sessions || [];
    const by = {};
    S.forEach(s => {
      const b = s.backend || 'intendant';
      by[b] = by[b] || { cost: 0, n: 0 };
      by[b].cost += s.cost || 0; by[b].n++;
    });
    const rows = Object.keys(by).sort((a, b) => by[b].cost - by[a].cost);
    return V3.card({
      title: 'By backend', sub: 'estimated cost · sessions — derived from the catalog',
      body: rows.length
        ? '<table class="table"><tbody>' + rows.map(b => {
            const max = by[rows[0]].cost || 1;
            return '<tr><td class="mono">' + V3.esc(b) + '</td>' +
              '<td class="mono">' + by[b].n + '</td>' +
              '<td class="mono">$' + by[b].cost.toFixed(2) + '</td>' +
              '<td style="width:34%">' + V3.meter(Math.round(by[b].cost / max * 100), '') + '</td></tr>';
          }).join('') + '</tbody></table>'
        : '<div class="dim" style="font-size:12.5px">No sessions yet — the ledger fills in as the house works.</div>'
    });
  },

  topSessionsHtml() {
    const S = (V3.data.sessions || []).filter(s => (s.cost || 0) > 0)
      .sort((a, b) => b.cost - a.cost).slice(0, 5);
    return V3.card({
      title: 'Priciest sessions', sub: 'top five · tap through to the session space',
      body: S.length
        ? '<table class="table"><tbody>' + S.map(s =>
            '<tr data-open-session="' + V3.esc(s.id) + '" style="cursor:pointer">' +
            '<td>' + V3.esc(s.name) + '<div class="dim" style="font-weight:400;font-size:12px">' + V3.esc((s.task || '').slice(0, 60)) + '</div></td>' +
            '<td>' + V3.chip(s.phase, s.active ? 'sage' : s.phase === 'failed' ? 'brick' : 'slate') + '</td>' +
            '<td class="mono">$' + s.cost.toFixed(2) + '</td>' +
            '<td>' + V3.chip('open', null, 'chev') + '</td></tr>').join('') + '</tbody></table>'
        : '<div class="dim" style="font-size:12.5px">No priced sessions yet — cost shows up once a provider reports usage.</div>'
    });
  },

  /* ---------- the List ---------- */
  listHtml() {
    const D = V3.data;
    const items = (D.agenda || []).filter(a => a.status !== 'retired');
    const counts = D.agendaCounts || {};
    if (!items.length) {
      return V3.empty('books', 'Nothing parked', 'Park a task, note, or question below — the house surfaces it when it’s due.') +
        (counts.retired ? '<div class="dim" style="font-size:12px;margin-top:6px">' + counts.retired + ' retired — hidden here.</div>' : '');
    }
    const open = items.filter(a => a.status !== 'done');
    const done = items.filter(a => a.status === 'done');
    const rows = open.concat(done).map(a => this.itemHtml(a)).join('');
    return rows +
      '<div class="dim" style="font-size:12px;margin-top:8px">' +
        V3.esc((counts.open != null ? counts.open : open.length) + ' open · ' + (counts.done != null ? counts.done : done.length) + ' done' +
        (counts.retired ? ' · ' + counts.retired + ' retired (hidden here)' : '')) + '</div>';
  },

  itemHtml(a) {
    const isDone = a.status === 'done';
    const hasFx = (a.effects || []).length > 0;
    const answer = a.answer && (typeof a.answer === 'string' ? a.answer : (a.answer.text || a.answer.answer || ''));
    return '<div class="bks-item" data-ag="' + V3.esc(a.id) + '">' +
      '<div class="row" data-ag-toggle="' + V3.esc(a.id) + '" title="click to ' + (isDone ? 'reopen' : 'complete') + '" style="cursor:pointer">' +
        '<span class="bks-check">' + (isDone ? V3.ICON('check', 12) : '') + '</span>' +
        V3.chip(a.kind, a.kind === 'task' ? 'slate' : a.kind === 'question' ? 'attn' : 'brass') +
        '<span class="bks-item-title' + (isDone ? ' done' : '') + '">' + V3.esc(a.title) + '</span>' +
        (a.due_ms ? V3.chip(this.clock(a.due_ms), 'brass', 'clock') : '') +
        (hasFx ? V3.chip('scheduled', 'brass', 'clock') : '') +
        '<span class="grow"></span>' +
        V3.chip(isDone ? 'done' : 'open', isDone ? 'sage' : 'slate') +
        (a.updated_ms ? V3.fact(this.ago(a.updated_ms)) : '') +
      '</div>' +
      (a.body ? '<div class="dim" style="font-size:12.5px;margin:2px 0 2px 28px">' + V3.esc(String(a.body).slice(0, 200)) + '</div>' : '') +
      ((a.tags || []).length ? '<div class="factline" style="margin-left:28px">' + a.tags.map(t => V3.fact('#' + t)).join('') + '</div>' : '') +
      (a.kind === 'question' && !isDone
        ? '<div class="row" style="margin:6px 0 2px 28px">' +
          '<input class="input" data-ag-answer="' + V3.esc(a.id) + '" placeholder="Answer the house…" style="flex:1;max-width:420px">' +
          '<button class="btn btn-quiet btn-xs" data-ag-answer-btn="' + V3.esc(a.id) + '">answer</button></div>'
        : '') +
      (a.kind === 'question' && isDone && answer
        ? '<div class="dim" style="font-size:12.5px;margin:2px 0 2px 28px">answered: “' + V3.esc(String(answer).slice(0, 160)) + '”</div>'
        : '') +
    '</div>';
  },

  /* ---------- scheduled sessions (the daemon's scheduler) ---------- */
  scheduledHtml() {
    const D = V3.data;
    const withFx = (D.agenda || []).filter(a => (a.effects || []).length);
    const openItems = (D.agenda || []).filter(a => a.status !== 'retired' && !(a.effects || []).length);

    const rows = withFx.map(a => (a.effects || []).map(fx => {
      const m = fx.manifest || {};
      const approved = !!(fx.approval && fx.approval.digest && fx.approval.digest === fx.digest);
      const last = fx.last_run || null;
      return '<div class="row" style="flex-wrap:wrap;padding:8px 0;border-bottom:1px solid var(--line)">' +
        V3.ICON('clock', 15) +
        '<b>' + V3.esc(m.goal || a.title || 'scheduled session') + '</b>' +
        (m.fire_at_ms ? V3.chip(this.clock(m.fire_at_ms), 'brass', 'clock') : V3.chip('no fire time', 'slate')) +
        (approved ? V3.chip('approved — will fire', 'sage', 'check') : V3.chip('proposed — needs your approval', 'attn')) +
        (last ? V3.fact('last run: ' + (last.state || '?') + (last.at_ms ? ' · ' + this.ago(last.at_ms) : '')) : '') +
        '<span class="grow"></span>' +
        (approved
          ? '<button class="btn btn-danger btn-xs" data-fx-revoke="' + V3.esc(a.id) + '">revoke</button>'
          : '<button class="btn btn-safe btn-xs" data-fx-approve="' + V3.esc(a.id) + '" data-fx-digest="' + V3.esc(fx.digest || '') + '">approve</button>') +
      '</div>';
    }).join('')).join('');

    return V3.card({
      title: 'Scheduled sessions', sub: 'the daemon’s scheduler fires these — an approval binds the exact manifest, byte for byte',
      body:
        (rows || '<div class="dim" style="font-size:12.5px;padding:4px 0 8px">Nothing scheduled. Pick a parked item below, give it a goal and a time — the house proposes it, you approve it, the daemon fires it.</div>') +
        '<div class="eyebrow" style="margin:14px 0 8px">Propose one</div>' +
        '<div class="row" style="flex-wrap:wrap;gap:8px">' +
          '<select class="input" id="bks-fx-item">' +
            (openItems.length
              ? openItems.map(a => '<option value="' + V3.esc(a.id) + '">' + V3.esc(a.title.slice(0, 48)) + '</option>').join('')
              : '<option value="">— park an item on the List first —</option>') +
          '</select>' +
          '<input class="input" id="bks-fx-goal" placeholder="What the session should do…" style="flex:1;min-width:200px">' +
          '<input class="input" type="datetime-local" id="bks-fx-when">' +
          '<button class="btn btn-primary" id="bks-fx-btn" ' + (openItems.length ? '' : 'disabled') + '>Propose</button>' +
        '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:8px">A proposal is data until you approve it — the daemon fires nothing unapproved, and re-proposing voids the old approval.</div>'
    });
  },

  /* ---------- what the house remembers ---------- */
  memHtml() {
    const mem = this._mem;
    if (!mem) {
      return '<div class="dim" style="font-size:12.5px">Search the plane — claims come back with their kind, sensitivity, and standing. Nothing is shown unprompted.</div>';
    }
    if (mem.error) {
      return '<div class="dim" style="font-size:12.5px">The memory plane said no: <b>' + V3.esc(mem.error) + '</b>' +
        (mem.error.includes('503') ? ' — it’s unavailable on this daemon.' : '') + '</div>';
    }
    const claims = mem.results || [];
    const durability = mem.durability ? '<div class="factline" style="margin-top:10px">' + V3.fact(mem.durability) + V3.fact('this plane is ephemeral in this build — a restart forgets') + '</div>' : '';
    if (!claims.length) {
      return '<div class="dim" style="font-size:12.5px">No claims match “' + V3.esc(mem.q) + '”.</div>' + durability;
    }
    return '<div class="grid grid-2">' + claims.map(c =>
      '<div class="card">' +
        '<div class="row" style="gap:6px;flex-wrap:wrap">' +
          V3.chip(c.kind || 'claim', 'slate') +
          V3.chip(c.sensitivity || '—', c.sensitivity === 'private' ? 'brick' : 'slate') +
          V3.chip(c.status || '—', c.status === 'accepted' ? 'sage' : c.status === 'candidate' ? 'attn' : 'slate') +
          '<span class="grow"></span>' +
          (c.id ? V3.fact(String(c.id).slice(0, 8)) : '') +
        '</div>' +
        '<div style="margin-top:8px;font-size:13.5px">' + V3.esc(c.statement || '') + '</div>' +
        ((c.session || (c.proposed_by && c.proposed_by.kind)) ? '<div class="factline" style="margin-top:8px">' +
          (c.session ? V3.fact('session: ' + c.session) : '') +
          (c.proposed_by && c.proposed_by.kind ? V3.fact('by: ' + c.proposed_by.kind) : '') + '</div>' : '') +
      '</div>').join('') + '</div>' + durability;
  },

  memSearch(el) {
    const input = el.querySelector('#bks-mem-q');
    const q = (input && input.value || '').trim();
    if (!q) { V3.toast('Type something to search for first', 'brick'); if (input) input.focus(); return; }
    const node = el.querySelector('#bks-mem-results');
    if (node) node.innerHTML = '<div class="col" style="gap:8px">' + V3.skeleton(14, '70%') + V3.skeleton(14, '100%') + V3.skeleton(14, '45%') + '</div>';
    V3.transport.get('/api/memory/search?q=' + encodeURIComponent(q) + '&limit=12&candidates=1').then(r => {
      this._mem = { q, results: r.results || r.claims || [], durability: r.durability || '' };
    }).catch(e => {
      this._mem = { q, results: [], durability: '', error: e.message };
    }).then(() => {
      const n = document.getElementById('bks-mem-results');
      if (n && V3.current === 'books') n.innerHTML = this.memHtml();
    });
  },

  /* ---------- reports & history ---------- */
  reportsHtml() {
    const S = (V3.data.sessions || []).filter(s => s.phase === 'done' || s.phase === 'failed');
    if (!S.length) {
      return V3.card({ title: 'Session reports', sub: 'the full audit trail, zipped — logs, frames, decisions',
        body: '<div class="dim" style="font-size:12.5px">Nothing finished yet — once a session wraps, its report zip lands here.</div>' });
    }
    return V3.card({
      title: 'Session reports', sub: 'the full audit trail, zipped — logs, frames, decisions',
      body: '<table class="table"><tbody>' +
        S.slice(0, 12).map(s =>
          '<tr>' +
          '<td><a href="#/work/session/' + V3.esc(s.id) + '" style="color:inherit">' + V3.esc(s.name) + '</a>' +
            '<div class="dim" style="font-weight:400;font-size:12px">' + V3.esc((s.task || '').slice(0, 60)) + '</div></td>' +
          '<td>' + V3.chip(s.phase, s.phase === 'done' ? 'sage' : 'brick') + '</td>' +
          '<td class="mono">' + V3.esc(s.backend || '') + '</td>' +
          '<td class="mono">' + (s.cost ? '$' + s.cost.toFixed(2) : '—') + '</td>' +
          '<td style="text-align:right"><a class="btn btn-quiet btn-xs" href="/api/session/' + encodeURIComponent(s.id) + '/report">' +
            V3.ICON('download', 13) + ' report .zip</a></td>' +
          '</tr>').join('') + '</tbody></table>'
    });
  },

  /* ---------- wiring ---------- */
  wire(el) {
    /* per-session cost rows → session space */
    el.querySelectorAll('[data-open-session]').forEach(r =>
      r.addEventListener('click', () => V3.go('#/work/session/' + r.dataset.openSession)));

    /* the List: complete / reopen */
    el.querySelectorAll('[data-ag-toggle]').forEach(row => row.addEventListener('click', () => {
      const a = (V3.data.agenda || []).find(x => x.id === row.dataset.agToggle);
      if (!a) return;
      const done = a.status === 'done';
      V3.actions.agendaOp({ op: done ? 'reopen' : 'complete', id: a.id })
        .then(() => V3.toast(done ? 'Reopened — back on the list' : 'Marked done — the house stops nudging', done ? null : 'sage'));
    }));

    /* add to the List */
    const addBtn = el.querySelector('#bks-add-btn');
    const addInput = el.querySelector('#bks-add');
    const addKind = el.querySelector('#bks-add-kind');
    const addItem = () => {
      const text = (addInput.value || '').trim();
      if (!text) { addInput.focus(); return; }
      const kind = addKind.value || 'task';
      addInput.value = '';
      V3.actions.agendaOp({ op: 'add', kind, title: text })
        .then(() => V3.toast('On the list — the house will surface it', 'sage'));
    };
    if (addBtn) addBtn.addEventListener('click', addItem);
    if (addInput) addInput.addEventListener('keydown', e => { if (e.key === 'Enter') addItem(); });

    /* answer a question */
    el.querySelectorAll('[data-ag-answer-btn]').forEach(btn => btn.addEventListener('click', () => {
      const id = btn.dataset.agAnswerBtn;
      const input = el.querySelector('[data-ag-answer="' + CSS.escape(id) + '"]');
      const text = (input && input.value || '').trim();
      if (!text) { if (input) input.focus(); return; }
      V3.actions.agendaOp({ op: 'answer', id, text })
        .then(() => V3.toast('Answered — the question resolves', 'sage'));
    }));
    el.querySelectorAll('[data-ag-answer]').forEach(input => input.addEventListener('keydown', e => {
      if (e.key === 'Enter') {
        const id = input.dataset.agAnswer;
        const text = (input.value || '').trim();
        if (!text) return;
        V3.actions.agendaOp({ op: 'answer', id, text })
          .then(() => V3.toast('Answered — the question resolves', 'sage'));
      }
    }));

    /* scheduled sessions: approve / revoke / propose */
    el.querySelectorAll('[data-fx-approve]').forEach(btn => btn.addEventListener('click', () => {
      V3.actions.agendaOp({ op: 'approve_effect', id: btn.dataset.fxApprove, digest: btn.dataset.fxDigest })
        .then(() => V3.toast('Approved — the daemon fires it at its time', 'sage'));
    }));
    el.querySelectorAll('[data-fx-revoke]').forEach(btn => btn.addEventListener('click', () => {
      V3.actions.agendaOp({ op: 'revoke_effect', id: btn.dataset.fxRevoke })
        .then(() => V3.toast('Approval revoked — it will not fire', 'brick'));
    }));
    const fxBtn = el.querySelector('#bks-fx-btn');
    if (fxBtn) fxBtn.addEventListener('click', () => {
      const itemSel = el.querySelector('#bks-fx-item');
      const goalIn = el.querySelector('#bks-fx-goal');
      const whenIn = el.querySelector('#bks-fx-when');
      const id = itemSel && itemSel.value;
      const goal = (goalIn.value || '').trim();
      const fire = whenIn.value ? new Date(whenIn.value).getTime() : NaN;
      if (!id) { V3.toast('Park an item on the List first', 'brick'); return; }
      if (!goal) { V3.toast('Say what the session should do', 'brick'); goalIn.focus(); return; }
      if (!isFinite(fire)) { V3.toast('Pick a fire time', 'brick'); whenIn.focus(); return; }
      V3.actions.agendaOp({ op: 'propose_effect', id, goal, fire_at_ms: Math.round(fire) })
        .then(() => V3.toast('Proposed — approve it here and the scheduler takes it', 'sage'));
    });

    /* memory search */
    const memBtn = el.querySelector('#bks-mem-btn');
    const memQ = el.querySelector('#bks-mem-q');
    if (memBtn) memBtn.addEventListener('click', () => this.memSearch(el));
    if (memQ) memQ.addEventListener('keydown', e => { if (e.key === 'Enter') this.memSearch(el); });
  },

  live(what) {
    if (!['agenda', 'sessions', 'ready'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA|SELECT/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
  }
};
