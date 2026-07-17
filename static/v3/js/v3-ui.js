/* Proscenium — shared UI helpers (the component contract).
   Every view renders through these; improvise nothing per-room. */
window.P = window.P || {};

/* ---------- tiny DOM ---------- */
V3.h = function (tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
};
V3.esc = function (s) {
  return String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
};
V3.ICON = function (name, size) { return '<span class="icon">' + V3.icon(name, size) + '</span>'; };

/* ---------- preferences (theme/density handled in app.js) ---------- */
V3.store = {
  get(k, dflt) { try { const v = localStorage.getItem('intendant.v3.' + k); return v == null ? dflt : JSON.parse(v); } catch { return dflt; } },
  set(k, v) { try { localStorage.setItem('intendant.v3.' + k, JSON.stringify(v)); } catch {} }
};
V3.density = () => document.documentElement.dataset.density || 'standard';

/* ---------- chips, dots, facts ---------- */
V3.chip = function (label, kind, iconName) {
  return '<span class="chip' + (kind ? ' chip-' + kind : '') + '">' +
    (iconName ? V3.ICON(iconName, 13) : '') + V3.esc(label) + '</span>';
};
V3.dot = function (kind, pulse) { return '<span class="dot dot-' + kind + (pulse ? ' dot-pulse' : '') + '"></span>'; };
V3.fact = function (s) { return '<span class="fact">' + V3.esc(s) + '</span>'; };
V3.meter = function (pct, cls) {
  const tone = cls || (pct >= 90 ? 'hot' : pct >= 70 ? 'warn' : '');
  return '<span class="meter" title="' + pct + '%"><span class="meter-fill ' + tone + '" style="width:' + pct + '%"></span></span>';
};
V3.machineName = function (id) {
  const m = V3.data.machines.find(m => m.id === id);
  return m ? m.petname : id;
};
V3.routeChip = function (route) {
  const kind = route === 'direct' ? 'sage' : route === 'fleet name' ? 'slate' : 'violet';
  return V3.chip('via ' + route, kind);
};
V3.authline = function (who, role, route) {
  return '<div class="authline">' + V3.ICON('shield', 13) +
    '<span class="who">' + V3.esc(who) + ' · ' + V3.esc(role) + '</span>' +
    (route ? V3.routeChip(route) : '') + '</div>';
};

/* ---------- folds (disclosure contract; remembers state; studio opens) ---------- */
V3.fold = function (opts) {
  // opts: {key, title, note, body, open}
  const key = opts.key ? 'fold.' + opts.key : null;
  const remembered = key ? V3.store.get(key, null) : null;
  const studio = V3.density() === 'studio';
  const open = remembered != null ? remembered : (opts.open != null ? opts.open : studio);
  const d = V3.h('details', 'fold');
  if (key) d.dataset.key = key;
  if (opts.id) d.id = opts.id;
  if (open) d.setAttribute('open', '');
  d.innerHTML = '<summary>' + V3.ICON('chev', 15).replace('class="icon"', 'class="icon chev"') +
    '<span>' + V3.esc(opts.title) + '</span>' +
    (opts.note ? '<span class="fold-note">' + V3.esc(opts.note) + '</span>' : '') + '</summary>' +
    '<div class="fold-body">' + opts.body + '</div>';
  d.addEventListener('toggle', function () {
    if (key) V3.store.set(key, d.open);
  });
  return d;
};
V3.foldHtml = function (opts) { const d = V3.fold(opts); return d.outerHTML; };

/* jump-and-flash: open a fold by id and flash it (⌘K landing) */
V3.flashTarget = function (id) {
  requestAnimationFrame(() => {
    const el = document.getElementById(id);
    if (!el) return;
    if (el.tagName === 'DETAILS') el.open = true;
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
  });
};

/* ---------- cards ---------- */
V3.card = function (opts) {
  // {title, sub, actions, body, cls}
  return '<div class="card ' + (opts.cls || '') + '">' +
    (opts.title ? '<div class="card-head"><div><h3 class="card-title">' + opts.title + '</h3>' +
      (opts.sub ? '<div class="card-sub">' + opts.sub + '</div>' : '') + '</div>' +
      (opts.actions ? '<div class="card-actions">' + opts.actions + '</div>' : '') + '</div>' : '') +
    (opts.body || '') + '</div>';
};

/* ---------- stage card (live work) ---------- */
V3.stageCard = function (s) {
  const needsYou = V3.data.queue.some(q => q.kind !== 'fyi' && q.session === s.id);
  const isPeer = s.machine && s.machine !== 'local';
  const tone = needsYou ? 'attention' : isPeer ? 'peer' : s.phase === 'done' ? 'done' : '';
  const facts = [];
  facts.push(V3.fact((s.backend || '') + (s.model ? ' · ' + s.model : '')));
  if (s.turn) facts.push(V3.fact('turn ' + s.turn));
  if (s.tokens && s.tokens.pct != null && s.tokens.pct > 0) facts.push('<span class="fact">' + s.tokens.pct + '% ctx</span>' + V3.meter(s.tokens.pct));
  if (s.cost != null && s.cost > 0) facts.push(V3.fact('$' + s.cost.toFixed(2)));
  return '<a class="stage-card ' + tone + '" href="#/work/session/' + s.id + '">' +
    '<div class="stage-body">' +
    '<div class="stage-name">' + (s.phase === 'working' ? V3.dot('sage', true) : V3.dot('slate')) +
      V3.esc(s.name) +
      (isPeer ? V3.chip(V3.machineName(s.machine), 'violet') : '') +
      (needsYou ? V3.chip('needs you', 'attn', 'doorbell') : '') + '</div>' +
    '<div class="stage-sentence">' + V3.esc(s.sentence || '') + '</div>' +
    '<div class="stage-facts">' + facts.join('') + '</div>' +
    '</div></a>';
};

/* ---------- decision card (the Queue) ---------- */
V3.decisionCard = function (q, opts) {
  opts = opts || {};
  if (V3.queueStore.isResolved(q.id)) return '';
  const isFyi = q.kind === 'fyi';
  let actions = q.actions.map(a =>
    '<button class="btn ' + (a.kind === 'primary' ? 'btn-primary' : a.kind === 'safe' ? 'btn-safe' : a.kind === 'danger' ? 'btn-danger' : 'btn-quiet') +
    (a.default && !isFyi ? ' btn-default' : '') + '" data-q="' + q.id + '" data-action="' + a.id + '">' +
    V3.esc(a.label) + (a.key ? ' <span class="kbd-hint">' + a.key + '</span>' : '') + '</button>').join('');
  let extra = '';
  if (q.options) {
    extra += '<div class="col" style="gap:6px;margin:4px 0 10px">' + q.options.map((o, i) =>
      '<label class="row" style="gap:8px"><input type="radio" name="' + q.id + '-opt-0" value="' + V3.esc(o) + '" ' + (i === 0 ? 'checked' : '') + '> <span>' + V3.esc(o) + '</span></label>').join('') +
      (q.freeText ? '<input class="input" data-free="0" placeholder="or say it in your own words…" style="margin-top:4px">' : '') + '</div>';
  } else if (q.freeText) {
    extra += '<div class="col" style="gap:6px;margin:4px 0 10px"><input class="input" data-free="0" placeholder="your answer…"></div>';
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
    '<span class="icon chev">' + V3.icon('chev', 15) + '</span><span>details</span>' +
    '<span class="fold-note">' + V3.esc(q.category || q.kind) + '</span></summary><div class="fold-body">' +
    (q.details.command ? '<div class="panel mono" style="margin-bottom:8px">$ ' + V3.esc(q.details.command) + '</div>' : '') +
    (q.details.paths ? '<div class="col" style="gap:3px;margin-bottom:8px">' + q.details.paths.map(p => '<span class="mono" style="font-size:12px">✕ ' + V3.esc(p) + '</span>').join('') + '</div>' : '') +
    (q.details.rule ? '<div class="dim" style="font-size:12.5px;margin-bottom:8px">' + V3.esc(q.details.rule) + '</div>' : '') +
    '<div class="log dim" style="font-size:11.5px">' + V3.esc(q.details.raw) + '</div>' +
    '</div></details>' : '';
  return '<div class="decision' + (isFyi ? ' fyi' : '') + '" data-qcard="' + q.id + '">' +
    '<div class="decision-head">' +
      (isFyi ? V3.chip('for your awareness', 'slate', 'info') : V3.chip('needs you · ' + (q.category || q.kind), 'attn', 'doorbell')) +
      '<span class="grow"></span><span class="fact">' + q.when + '</span></div>' +
    '<div class="decision-title">' + V3.esc(q.title) + '</div>' +
    '<div class="decision-consequence">' + V3.esc(q.consequence) + '</div>' +
    extra + details +
    '<div class="decision-actions">' + actions + '</div>' +
    (q.machine && q.machine !== 'local'
      ? V3.authline('via ' + V3.machineName(q.machine), q.role || 'peer', 'fleet name')
      : V3.authline(V3.data.you.name, V3.data.you.role, V3.data.you.route)) +
    '</div>';
};

/* ---------- queue store lives in v3-data.js (server-truth + actions) ---------- */

/* ---------- empty state & skeleton ---------- */
V3.empty = function (glyph, title, hint, actionHtml) {
  return '<div class="empty"><div class="empty-glyph">' + V3.ICON(glyph, 34) + '</div>' +
    '<div class="empty-title">' + V3.esc(title) + '</div>' +
    '<div class="empty-hint">' + V3.esc(hint) + '</div>' + (actionHtml || '') + '</div>';
};
V3.skeleton = function (h, w) {
  return '<div class="skeleton" style="height:' + (h || 14) + 'px;width:' + (w || '100%') + '"></div>';
};

/* ---------- toast ---------- */
V3.toast = function (msg, tone) {
  const t = V3.h('div', 'toast ' + (tone || ''), V3.ICON(tone === 'brick' ? 'x' : 'check', 15) + '<span>' + V3.esc(msg) + '</span>');
  document.getElementById('toasts').appendChild(t);
  setTimeout(() => { t.style.opacity = '0'; t.style.transition = 'opacity .3s'; setTimeout(() => t.remove(), 320); }, 3200);
};

/* ---------- page scaffold ---------- */
V3.page = function (opts) {
  // {eyebrow, title, sub, body}
  return '<div class="page">' +
    (opts.eyebrow ? '<div class="eyebrow">' + V3.esc(opts.eyebrow) + '</div>' : '') +
    (opts.title ? '<h1 class="page-title">' + opts.title + '</h1>' : '') +
    (opts.sub ? '<div class="page-sub">' + opts.sub + '</div>' : '') +
    opts.body + '</div>';
};
V3.section = function (label) {
  return '<div class="section-head"><span class="eyebrow">' + V3.esc(label) + '</span><span class="line"></span></div>';
};

/* ---------- global click delegation for queue actions ---------- */
document.addEventListener('click', function (e) {
  const b = e.target.closest('[data-q][data-action]');
  if (!b) return;
  const q = V3.data.queue.find(x => x.id === b.dataset.q);
  if (q) V3.actions.resolveQueueItem(q, b.dataset.action);
});

/* helpers */
V3.row = function (html, gap) { return '<div class="row" style="display:flex;align-items:center;gap:' + (gap || 8) + 'px">' + html + '</div>'; };
V3.col = function (html, gap) { return '<div class="col" style="display:flex;flex-direction:column;gap:' + (gap || 8) + 'px">' + html + '</div>'; };
V3.logLines = function (lines) {
  return '<div class="log">' + lines.map(l =>
    '<div><span class="lt">' + l[0] + '</span> <span class="' +
    (l[1] === 'tool' ? 'tool' : l[1] === 'ok' ? 'ok' : l[1] === 'err' || l[1] === 'ask' ? 'err' : '') + '">' +
    V3.esc(l[2]) + '</span></div>').join('') + '</div>';
};
V3.diffHtml = function (rows) {
  return '<div class="diff">' + rows.map(r =>
    '<span class="diff-' + r[0] + '">' + V3.esc(r[1]) + '</span>').join('') + '</div>';
};
