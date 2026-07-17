/* Proscenium — Books: the ledgers.
   Costs & usage → the List → what the house remembers → reports. */
window.P = window.P || {};
P.views = P.views || {};

P.views.books = {
  title: 'Books',

  render(el) {
    const u = P.data.usage;

    el.innerHTML = P.page({
      eyebrow: 'the ledgers',
      title: 'Books',
      sub: 'What the house spent, what’s parked for later, and what it remembers. Every number is the daemon’s own bookkeeping — no estimates invented here.',
      body:
        /* ---------- Costs & usage ---------- */
        P.section('Costs & usage') +
        '<div class="bks-kpis">' +
          u.kpis.map(k =>
            '<div class="card bks-kpi">' +
              '<div class="bks-kpi-value">' + P.esc(k.value) + '</div>' +
              '<div class="bks-kpi-label">' + P.esc(k.label) + '</div>' +
              '<div class="dim" style="font-size:12px;margin-top:2px">' + P.esc(k.sub) + '</div>' +
            '</div>').join('') +
        '</div>' +

        '<div class="grid grid-2">' +
          P.card({
            title: 'By agent — this month', sub: 'estimated cost · sessions', cls: 'bks-costbar',
            body: '<table class="table"><tbody>' +
              u.byAgent.map(a => {
                const max = u.byAgent[0].cost;
                return '<tr><td>' + P.esc(a.agent) + '</td>' +
                  '<td class="mono">' + a.sessions + '</td>' +
                  '<td class="mono">$' + a.cost.toFixed(2) + '</td>' +
                  '<td style="width:34%">' + P.row(P.meter(Math.round(a.cost / max * 100), 'flat')) + '</td></tr>';
              }).join('') + '</tbody></table>'
          }) +
          P.card({
            title: 'Disk', sub: 'what the house keeps on this machine',
            body: '<table class="table"><tbody>' +
              u.disk.map(d =>
                '<tr><td>' + P.esc(d.what) + '</td><td class="mono" style="text-align:right">' + P.esc(d.size) + '</td></tr>').join('') +
              '</tbody></table>' +
              '<div class="dim" style="font-size:12px;margin-top:8px">Per-kind deletion lives with each session — the report zips below are the audit trail.</div>'
          }) +
        '</div>' +

        P.card({
          title: 'Activity — the last 26 weeks', sub: 'sessions per day, any machine',
          body:
            '<div class="heat bks-heat">' +
              u.heat.map(v => '<i' + (v ? ' class="l' + v + '"' : '') + '></i>').join('') +
            '</div>' +
            '<div class="row bks-heat-legend">' +
              '<span class="fact">less</span><i></i><i class="l1"></i><i class="l2"></i><i class="l3"></i><i class="l4"></i><span class="fact">more</span>' +
              '<span class="grow"></span>' + P.fact('9 active today · 61% cache-served') +
            '</div>'
        }) +

        /* ---------- The List ---------- */
        P.section('The List — parked for later') +
        '<div class="card">' +
          '<div id="bks-list">' + this.listHtml() + '</div>' +
          '<div class="row" style="margin-top:10px;padding-top:12px;border-top:1px solid var(--line)">' +
            '<input class="input" id="bks-add" placeholder="Park a task, note, or question — the house will surface it…" style="flex:1;min-width:220px">' +
            '<button class="btn btn-quiet" id="bks-add-btn">' + P.ICON('plus', 14) + ' Add</button>' +
          '</div>' +
        '</div>' +

        P.card({
          title: 'Scheduled sessions', sub: 'the daemon’s scheduler fires these — Today on Home shows the same',
          body:
            '<div class="row" style="flex-wrap:wrap">' +
              P.ICON('clock', 15) + '<b>nightly-backup</b>' +
              P.chip('every day · 02:00', 'brass', 'clock') +
              P.fact('next tonight · ran clean at 02:00') +
              '<span class="grow"></span>' +
              '<button class="btn btn-quiet btn-xs" data-sched="pause">pause</button>' +
              '<button class="btn btn-danger btn-xs" data-sched="revoke">revoke</button>' +
            '</div>'
        }) +

        /* ---------- What the house remembers ---------- */
        P.section('What the house remembers') +
        '<div class="grid grid-2" id="bks-mem">' + this.memHtml() + '</div>' +
        '<div class="card">' +
          '<div class="row" style="gap:8px;flex-wrap:wrap">' +
            '<select class="input" id="bks-mem-kind">' +
              ['preference', 'decision', 'observation', 'procedure', 'episode'].map(k => '<option>' + k + '</option>').join('') +
            '</select>' +
            '<input class="input" id="bks-mem-text" placeholder="A claim the house should consider…" style="flex:1;min-width:240px">' +
            '<button class="btn btn-primary" id="bks-mem-btn">Propose</button>' +
          '</div>' +
          '<div class="dim" style="margin-top:10px;font-size:12.5px">New claims land as <b>candidates</b> — nothing is accepted until the house argues it through. And the honest caveat: this plane is ephemeral in this build — restart forgets.</div>' +
        '</div>' +

        /* ---------- Reports & history ---------- */
        P.section('Reports & history') +
        P.card({
          title: 'Session reports', sub: 'the full audit trail, zipped — logs, frames, decisions',
          body:
            '<div class="row" style="gap:8px;flex-wrap:wrap">' +
              P.data.sessions.filter(s => s.phase === 'done' || s.phase === 'archived').map(s =>
                '<button class="btn btn-quiet btn-xs" data-report="' + P.esc(s.id) + '">' +
                P.ICON('download', 13) + ' ' + P.esc(s.name) + ' .zip</button>').join('') +
            '</div>' +
            '<div class="dim" style="margin-top:10px;font-size:12px">Deep search across full logs lives in ⌘K — three or more characters, under “Search deeper”.</div>'
        })
    });

    this.wire(el);
  },

  listHtml() {
    return P.data.agenda.map(a =>
      '<div class="bks-item row" data-ag="' + P.esc(a.id) + '" title="click to ' + (a.status === 'done' ? 'reopen' : 'complete') + '">' +
        '<span class="bks-check">' + (a.status === 'done' ? P.ICON('check', 12) : '') + '</span>' +
        P.chip(a.kind, a.kind === 'task' ? 'slate' : a.kind === 'question' ? 'attn' : 'brass') +
        '<span class="bks-item-title' + (a.status === 'done' ? ' done' : '') + '">' + P.esc(a.title) + '</span>' +
        (a.scheduled ? P.chip(a.scheduled, 'brass', 'clock') : '') +
        '<span class="grow"></span>' +
        P.chip(a.status === 'done' ? 'done' : 'open', a.status === 'done' ? 'sage' : 'slate') +
        (a.added ? P.fact('added ' + a.added) : '') +
      '</div>').join('');
  },

  memHtml() {
    return P.data.memory.map(m =>
      '<div class="card">' +
        '<div class="row" style="gap:6px;flex-wrap:wrap">' +
          P.chip(m.kind, 'slate') +
          P.chip(m.sensitivity, m.sensitivity === 'private' ? 'brick' : 'slate') +
          P.chip(m.status, m.status === 'accepted' ? 'sage' : 'attn') +
          '<span class="grow"></span>' + P.fact('by ' + m.by) +
        '</div>' +
        '<div style="margin-top:8px;font-size:13.5px">' + P.esc(m.statement) + '</div>' +
      '</div>').join('');
  },

  wire(el) {
    const rewiredList = () => {
      el.querySelector('#bks-list').innerHTML = this.listHtml();
      wireList();
    };
    const wireList = () => {
      el.querySelectorAll('[data-ag]').forEach(r => r.addEventListener('click', () => {
        const a = P.data.agenda.find(x => x.id === r.dataset.ag);
        if (!a) return;
        a.status = a.status === 'done' ? 'open' : 'done';
        rewiredList();
        P.toast(a.status === 'done' ? 'Marked done — the house stops nudging' : 'Reopened — back on the list', a.status === 'done' ? 'sage' : null);
      }));
    };
    wireList();

    const addBtn = el.querySelector('#bks-add-btn');
    const addInput = el.querySelector('#bks-add');
    const addItem = () => {
      const text = (addInput.value || '').trim();
      if (!text) { addInput.focus(); return; }
      P.data.agenda.push({ id: 'a' + Date.now(), kind: 'task', title: text, status: 'open', added: 'now' });
      addInput.value = '';
      rewiredList();
      P.toast('On the list — the house will surface it', 'sage');
    };
    addBtn.addEventListener('click', addItem);
    addInput.addEventListener('keydown', e => { if (e.key === 'Enter') addItem(); });

    el.querySelectorAll('[data-sched]').forEach(b => b.addEventListener('click', () => {
      if (b.dataset.sched === 'pause') P.toast('nightly-backup paused — the scheduler holds its fire', null);
      else P.toast('Schedule revoked — 02:00 will come and go quietly', 'brick');
    }));

    el.querySelector('#bks-mem-btn').addEventListener('click', () => {
      const kind = el.querySelector('#bks-mem-kind').value;
      const text = (el.querySelector('#bks-mem-text').value || '').trim();
      if (!text) { el.querySelector('#bks-mem-text').focus(); return; }
      P.data.memory.unshift({ kind: kind, statement: text, sensitivity: 'private', status: 'candidate', by: 'you' });
      el.querySelector('#bks-mem').innerHTML = this.memHtml();
      el.querySelector('#bks-mem-text').value = '';
      P.toast('Proposed — a candidate now; the house argues it before accepting', 'sage');
    });

    el.querySelectorAll('[data-report]').forEach(b => b.addEventListener('click', () =>
      P.toast('The ' + b.dataset.report + ' report zip would download now', 'sage')));
  }
};
