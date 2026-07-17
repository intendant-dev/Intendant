/* Proscenium — shared UI helpers (the component contract).
   Every view renders through these; improvise nothing per-room. */
window.P = window.P || {};

/* ---------- tiny DOM ---------- */
P.h = function (tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
};
P.esc = function (s) {
  return String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
};
P.ICON = function (name, size) { return '<span class="icon">' + P.icon(name, size) + '</span>'; };

/* ---------- preferences (theme/density handled in app.js) ---------- */
P.store = {
  get(k, dflt) { try { const v = localStorage.getItem('proscenium.' + k); return v == null ? dflt : JSON.parse(v); } catch { return dflt; } },
  set(k, v) { try { localStorage.setItem('proscenium.' + k, JSON.stringify(v)); } catch {} }
};
P.density = () => document.documentElement.dataset.density || 'standard';

/* ---------- chips, dots, facts ---------- */
P.chip = function (label, kind, iconName) {
  return '<span class="chip' + (kind ? ' chip-' + kind : '') + '">' +
    (iconName ? P.ICON(iconName, 13) : '') + P.esc(label) + '</span>';
};
P.dot = function (kind, pulse) { return '<span class="dot dot-' + kind + (pulse ? ' dot-pulse' : '') + '"></span>'; };
P.fact = function (s) { return '<span class="fact">' + P.esc(s) + '</span>'; };
P.meter = function (pct, cls) {
  const tone = cls || (pct >= 90 ? 'hot' : pct >= 70 ? 'warn' : '');
  return '<span class="meter" title="' + pct + '%"><span class="meter-fill ' + tone + '" style="width:' + pct + '%"></span></span>';
};
P.machineName = function (id) {
  const m = P.data.machines.find(m => m.id === id);
  return m ? m.petname : id;
};
P.routeChip = function (route) {
  const kind = route === 'direct' ? 'sage' : route === 'fleet name' ? 'slate' : 'violet';
  return P.chip('via ' + route, kind);
};
P.authline = function (who, role, route) {
  return '<div class="authline">' + P.ICON('shield', 13) +
    '<span class="who">' + P.esc(who) + ' · ' + P.esc(role) + '</span>' +
    (route ? P.routeChip(route) : '') + '</div>';
};

/* ---------- folds (disclosure contract; remembers state; studio opens) ---------- */
P.fold = function (opts) {
  // opts: {key, title, note, body, open}
  const key = opts.key ? 'fold.' + opts.key : null;
  const remembered = key ? P.store.get(key, null) : null;
  const studio = P.density() === 'studio';
  const open = remembered != null ? remembered : (opts.open != null ? opts.open : studio);
  const d = P.h('details', 'fold');
  if (key) d.dataset.key = key;
  if (opts.id) d.id = opts.id;
  if (open) d.setAttribute('open', '');
  d.innerHTML = '<summary>' + P.ICON('chev', 15).replace('class="icon"', 'class="icon chev"') +
    '<span>' + P.esc(opts.title) + '</span>' +
    (opts.note ? '<span class="fold-note">' + P.esc(opts.note) + '</span>' : '') + '</summary>' +
    '<div class="fold-body">' + opts.body + '</div>';
  d.addEventListener('toggle', function () {
    if (key) P.store.set(key, d.open);
  });
  return d;
};
P.foldHtml = function (opts) { const d = P.fold(opts); return d.outerHTML; };

/* jump-and-flash: open a fold by id and flash it (⌘K landing) */
P.flashTarget = function (id) {
  requestAnimationFrame(() => {
    const el = document.getElementById(id);
    if (!el) return;
    if (el.tagName === 'DETAILS') el.open = true;
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
  });
};

/* ---------- cards ---------- */
P.card = function (opts) {
  // {title, sub, actions, body, cls}
  return '<div class="card ' + (opts.cls || '') + '">' +
    (opts.title ? '<div class="card-head"><div><h3 class="card-title">' + opts.title + '</h3>' +
      (opts.sub ? '<div class="card-sub">' + opts.sub + '</div>' : '') + '</div>' +
      (opts.actions ? '<div class="card-actions">' + opts.actions + '</div>' : '') + '</div>' : '') +
    (opts.body || '') + '</div>';
};

/* ---------- stage card (live work) ---------- */
P.stageCard = function (s) {
  const tone = s.queue && s.queue.length ? 'attention' : s.peer ? 'peer' : s.phase === 'done' ? 'done' : '';
  const facts = [];
  facts.push(P.fact(s.backend + ' · ' + s.model));
  if (s.turn) facts.push(P.fact('turn ' + s.turn));
  if (s.tokens) facts.push('<span class="fact">' + s.tokens.pct + '% ctx</span>' + P.meter(s.tokens.pct));
  if (s.cost != null) facts.push(P.fact('$' + s.cost.toFixed(2)));
  return '<a class="stage-card ' + tone + '" href="#/work/session/' + s.id + '">' +
    '<div class="stage-body">' +
    '<div class="stage-name">' + (s.phase === 'working' ? P.dot('sage', true) : s.phase === 'idle' ? P.dot('slate') : P.dot('slate')) +
      P.esc(s.name) +
      (s.peer ? P.chip(P.machineName(s.machine), 'violet') : '') +
      (s.queue && s.queue.length ? P.chip('needs you', 'attn', 'doorbell') : '') + '</div>' +
    '<div class="stage-sentence">' + P.esc(s.sentence) + '</div>' +
    '<div class="stage-facts">' + facts.join('') + '</div>' +
    '</div></a>';
};

/* ---------- decision card (the Queue) ---------- */
P.decisionCard = function (q, opts) {
  opts = opts || {};
  if (P.queueStore.isResolved(q.id)) return '';
  const isFyi = q.kind === 'fyi';
  let actions = q.actions.map(a =>
    '<button class="btn ' + (a.kind === 'primary' ? 'btn-primary' : a.kind === 'safe' ? 'btn-safe' : a.kind === 'danger' ? 'btn-danger' : 'btn-quiet') +
    (a.default && !isFyi ? ' btn-default' : '') + '" data-q="' + q.id + '" data-action="' + a.id + '">' +
    P.esc(a.label) + (a.key ? ' <span class="kbd-hint">' + a.key + '</span>' : '') + '</button>').join('');
  let extra = '';
  if (q.options) {
    extra += '<div class="col" style="gap:6px;margin:4px 0 10px">' + q.options.map((o, i) =>
      '<label class="row" style="gap:8px"><input type="radio" name="' + q.id + '-opt" ' + (i === 0 ? 'checked' : '') + '> <span>' + P.esc(o) + '</span></label>').join('') +
      (q.freeText ? '<input class="input" placeholder="or say it in your own words…" style="margin-top:4px">' : '') + '</div>';
  }
  if (q.durations) {
    extra += '<div class="row" style="gap:6px;margin:2px 0 10px">' + q.durations.map((d, i) =>
      '<span class="chip' + (i === 0 ? ' chip-sage' : '') + '">' + d + '</span>').join('') + '</div>';
  }
  if (q.roles) {
    extra += '<div class="dim" style="font-size:12.5px;margin:2px 0 8px">' +
      'A <b>spectator</b> watches. An <b>operator</b> runs work and touches screens, but can’t hand out keys.</div>';
  }
  const details = q.details ? '<details class="fold" style="margin:8px 0 10px"><summary>' +
    '<span class="icon chev">' + P.icon('chev', 15) + '</span><span>details</span>' +
    '<span class="fold-note">' + P.esc(q.category || q.kind) + '</span></summary><div class="fold-body">' +
    (q.details.command ? '<div class="panel mono" style="margin-bottom:8px">$ ' + P.esc(q.details.command) + '</div>' : '') +
    (q.details.paths ? '<div class="col" style="gap:3px;margin-bottom:8px">' + q.details.paths.map(p => '<span class="mono" style="font-size:12px">✕ ' + P.esc(p) + '</span>').join('') + '</div>' : '') +
    (q.details.rule ? '<div class="dim" style="font-size:12.5px;margin-bottom:8px">' + P.esc(q.details.rule) + '</div>' : '') +
    '<div class="log dim" style="font-size:11.5px">' + P.esc(q.details.raw) + '</div>' +
    '</div></details>' : '';
  return '<div class="decision' + (isFyi ? ' fyi' : '') + '" data-qcard="' + q.id + '">' +
    '<div class="decision-head">' +
      (isFyi ? P.chip('for your awareness', 'slate', 'info') : P.chip('needs you · ' + (q.category || q.kind), 'attn', 'doorbell')) +
      '<span class="grow"></span><span class="fact">' + q.when + '</span></div>' +
    '<div class="decision-title">' + P.esc(q.title) + '</div>' +
    '<div class="decision-consequence">' + P.esc(q.consequence) + '</div>' +
    extra + details +
    '<div class="decision-actions">' + actions + '</div>' +
    P.authline('you', 'owner', 'direct') +
    '</div>';
};

/* ---------- queue store (shared: Home, drawer, badge) ---------- */
P.queueStore = {
  resolved: P.store.get('queue.resolved', {}),
  isResolved(id) { return !!this.resolved[id]; },
  resolve(id, action) {
    this.resolved[id] = { action: action, at: Date.now() };
    P.store.set('queue.resolved', this.resolved);
    const q = P.data.queue.find(x => x.id === id);
    P.toast(this.toastFor(q, action), q && (action === 'deny' || action === 'deny-session') ? 'brick' : 'sage');
    document.querySelectorAll('[data-qcard="' + id + '"]').forEach(c => {
      c.classList.add('resolved'); setTimeout(() => c.remove(), 320);
    });
    setTimeout(() => P.renderQueueBadge(), 300);
  },
  toastFor(q, action) {
    if (!q) return 'Done';
    const map = {
      approve: 'Allowed once — the house carries on', always: 'Rule set: ' + (q.category || '') + ' → Auto for this project',
      deny: 'Denied — nothing was touched', 'deny-session': 'Denied for this session',
      answer: 'Answer sent', skip: 'The house will decide', spectator: 'Spectator key issued', operator: 'Operator key issued',
      dismiss: 'Noted', open: 'Opening…', renew: 'Lease renewed — Workshop stays fueled', allow: 'Screen shared for 15 minutes'
    };
    return map[action] || 'Resolved';
  },
  pending() { return P.data.queue.filter(q => !this.isResolved(q.id) && q.kind !== 'fyi'); },
  fyis() { return P.data.queue.filter(q => !this.isResolved(q.id) && q.kind === 'fyi'); }
};

/* ---------- empty state & skeleton ---------- */
P.empty = function (glyph, title, hint, actionHtml) {
  return '<div class="empty"><div class="empty-glyph">' + P.ICON(glyph, 34) + '</div>' +
    '<div class="empty-title">' + P.esc(title) + '</div>' +
    '<div class="empty-hint">' + P.esc(hint) + '</div>' + (actionHtml || '') + '</div>';
};
P.skeleton = function (h, w) {
  return '<div class="skeleton" style="height:' + (h || 14) + 'px;width:' + (w || '100%') + '"></div>';
};

/* ---------- toast ---------- */
P.toast = function (msg, tone) {
  const t = P.h('div', 'toast ' + (tone || ''), P.ICON(tone === 'brick' ? 'x' : 'check', 15) + '<span>' + P.esc(msg) + '</span>');
  document.getElementById('toasts').appendChild(t);
  setTimeout(() => { t.style.opacity = '0'; t.style.transition = 'opacity .3s'; setTimeout(() => t.remove(), 320); }, 3200);
};

/* ---------- page scaffold ---------- */
P.page = function (opts) {
  // {eyebrow, title, sub, body}
  return '<div class="page">' +
    (opts.eyebrow ? '<div class="eyebrow">' + P.esc(opts.eyebrow) + '</div>' : '') +
    (opts.title ? '<h1 class="page-title">' + opts.title + '</h1>' : '') +
    (opts.sub ? '<div class="page-sub">' + opts.sub + '</div>' : '') +
    opts.body + '</div>';
};
P.section = function (label) {
  return '<div class="section-head"><span class="eyebrow">' + P.esc(label) + '</span><span class="line"></span></div>';
};

/* ---------- global click delegation for queue actions ---------- */
document.addEventListener('click', function (e) {
  const b = e.target.closest('[data-q][data-action]');
  if (!b) return;
  const qid = b.dataset.q, action = b.dataset.action;
  if (action === 'open') { location.hash = '#/work/session/fix-login'; return; }
  P.queueStore.resolve(qid, action);
});

/* helpers */
P.row = function (html, gap) { return '<div class="row" style="display:flex;align-items:center;gap:' + (gap || 8) + 'px">' + html + '</div>'; };
P.col = function (html, gap) { return '<div class="col" style="display:flex;flex-direction:column;gap:' + (gap || 8) + 'px">' + html + '</div>'; };
P.logLines = function (lines) {
  return '<div class="log">' + lines.map(l =>
    '<div><span class="lt">' + l[0] + '</span> <span class="' +
    (l[1] === 'tool' ? 'tool' : l[1] === 'ok' ? 'ok' : l[1] === 'err' || l[1] === 'ask' ? 'err' : '') + '">' +
    P.esc(l[2]) + '</span></div>').join('') + '</div>';
};
P.diffHtml = function (rows) {
  return '<div class="diff">' + rows.map(r =>
    '<span class="diff-' + r[0] + '">' + P.esc(r[1]) + '</span>').join('') + '</div>';
};
