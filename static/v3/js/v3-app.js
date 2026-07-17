/* V3 — app shell: router, rail, topbar, composer, keyboard, theme/density,
   queue drawer. Boot order: transport → data → subscribe → route.
   V3 is a renderer over the one control plane, never a second brain. */
window.V3 = window.V3 || {};

V3.ROOMS = [
  { id: 'home',     title: 'Home',          icon: 'arch',     gloss: 'the conversation · needs you' },
  { id: 'work',     title: 'Work',          icon: 'stage',    gloss: 'sessions on stage' },
  { id: 'screens',  title: 'Screens',       icon: 'screens',  gloss: 'see & touch your machines' },
  { id: 'files',    title: 'Files',         icon: 'files',    gloss: 'the desk · transfers' },
  { id: 'machines', title: 'Machines',      icon: 'machines', gloss: 'the fleet · pairing · delegation' },
  { id: 'people',   title: 'People & Keys', icon: 'key',      gloss: 'who may do what · vault' },
  { id: 'books',    title: 'Books',         icon: 'books',    gloss: 'costs · the list · memory' },
  { id: 'settings', title: 'Settings',      icon: 'settings', gloss: 'how the house behaves' }
];
V3.VANTAGES = [
  { id: 'station', title: 'Station', icon: 'station', gloss: 'the constellation' },
  { id: 'studio',  title: 'Studio',  icon: 'studio',  gloss: 'the machinery, raw' }
];

V3.current = null;
V3._subs = [];

/* tiny state bus: views and chrome subscribe to store changes */
V3.bus = {
  sub(fn) { V3._subs.push(fn); },
  emit(what) {
    for (const fn of V3._subs) { try { fn(what); } catch (e) { console.error('[v3] bus listener', e); } }
  }
};

V3.go = function (hash) {
  if (location.hash === hash) V3.route();
  else location.hash = hash;
};
V3.rerender = function () { V3.route(); };

V3.route = function () {
  const raw = (location.hash || '#/home').replace(/^#\/?/, '');
  const parts = raw.split('/').filter(Boolean);
  let viewId = parts[0] || 'home';
  let params = parts.slice(1);

  if (viewId === 'work' && params[0] === 'session') { viewId = 'session'; params = params.slice(1); }
  if (!V3.views[viewId]) { viewId = 'home'; params = []; }

  V3.current = viewId;
  const view = V3.views[viewId];
  const main = document.getElementById('main');
  main.innerHTML = '';
  view.render(main, params);
  V3.mountIcons(main);
  main.scrollTop = 0;

  document.querySelectorAll('#rail-rooms .rail-item, #rail-vantages .rail-item').forEach(a => {
    a.classList.toggle('on', a.dataset.room === viewId || (viewId === 'session' && a.dataset.room === 'work'));
  });

  const room = V3.ROOMS.concat(V3.VANTAGES).find(r => r.id === viewId);
  document.getElementById('topbar-title').textContent =
    viewId === 'session' ? 'Work' : room ? room.title : '';

  V3.renderTopFacts();
  V3.renderQueueBadge();
  V3.renderRailIdentity();
};

V3.renderTopFacts = function () {
  const d = V3.data;
  const working = d.sessions.filter(s => s.active).length;
  const conn = V3.transport.state; // 'connecting' | 'live' | 'offline'
  const facts = [
    '<span>' + working + ' working</span>',
    '<span>' + V3.queueStore.pending().length + ' need you</span>'
  ];
  if (conn !== 'live') facts.push('<span class="chip chip-attn">reconnecting…</span>');
  else facts.push('<span class="studio-only">' + V3.esc(d.connFact || 'live') + '</span>');
  document.getElementById('topbar-facts').innerHTML = facts.join('');
};

/* ---------------- rail ---------------- */
V3.buildRail = function () {
  document.getElementById('rail-rooms').innerHTML = V3.ROOMS.map(r =>
    '<a class="rail-item" data-room="' + r.id + '" href="#/' + r.id + '" title="' + V3.esc(r.gloss) + '">' +
    '<span class="icon">' + V3.icon(r.icon) + '</span><span class="rail-label">' + r.title + '</span></a>').join('');
  document.getElementById('rail-vantages').innerHTML =
    '<div class="rail-group-label">Vantages</div>' +
    V3.VANTAGES.map(r =>
      '<a class="rail-item" data-room="' + r.id + '" href="#/' + r.id + '" title="' + V3.esc(r.gloss) + '">' +
      '<span class="icon">' + V3.icon(r.icon) + '</span><span class="rail-label">' + r.title + '</span></a>').join('');
  V3.mountIcons(document.getElementById('rail'));
};

V3.renderRailIdentity = function () {
  const you = V3.data.you || {};
  document.getElementById('rail-identity').innerHTML =
    '<div class="authline"><span class="who">' + V3.esc(you.name || 'you') + ' · ' + V3.esc(you.role || 'owner') + '</span>' +
    (you.route ? V3.routeChip(you.route) : '') + '</div>';
};

/* ---------------- queue badge + drawer ---------------- */
V3.renderQueueBadge = function () {
  const n = V3.queueStore.pending().length;
  const badge = document.getElementById('rail-queue-count');
  badge.hidden = n === 0;
  badge.textContent = n;
  document.title = (n ? '(' + n + ') ' : '') + 'Intendant';
  V3.renderTopFacts();
};

V3.openQueueDrawer = function () {
  const drawer = document.getElementById('queue-drawer');
  const panel = document.getElementById('queue-panel');
  const pending = V3.queueStore.pending();
  const fyis = V3.queueStore.fyis();
  panel.innerHTML =
    '<div class="row"><h2 class="voice" style="margin:0;font-size:22px">Needs you</h2><span class="grow"></span>' +
    '<button class="icon-btn" id="queue-close">' + V3.icon('x') + '</button></div>' +
    (pending.length || fyis.length
      ? pending.map(q => V3.decisionCard(q)).join('') +
        (fyis.length ? '<div class="eyebrow" style="margin-top:8px">for your awareness</div>' + fyis.map(q => V3.decisionCard(q)).join('') : '')
      : '<div class="queue-free"><span class="big">You’re free.</span>Nothing needs you. The house will tap you the moment something does.</div>');
  drawer.hidden = false;
  panel.querySelector('#queue-close').onclick = () => drawer.hidden = true;
  drawer.onclick = e => { if (e.target === drawer) drawer.hidden = true; };
};

/* ---------------- theme & density ---------------- */
V3.setTheme = function (t) {
  document.documentElement.dataset.theme = t;
  V3.store.set('theme', t);
};
V3.cycleTheme = function () {
  V3.setTheme(document.documentElement.dataset.theme === 'lamplight' ? 'daylight' : 'lamplight');
};
V3.setDensity = function (d) {
  document.documentElement.dataset.density = d;
  V3.store.set('density', d);
  document.getElementById('density-label').textContent = d;
  V3.rerender();
};
V3.cycleDensity = function () {
  const order = ['cozy', 'standard', 'studio'];
  V3.setDensity(order[(order.indexOf(V3.density()) + 1) % order.length]);
};

/* ---------------- composer ---------------- */
V3.wireComposer = function () {
  const ta = document.getElementById('composer-input');
  const target = document.getElementById('composer-target');
  const syncTarget = () => {
    const t = V3.data.composerTarget;
    target.hidden = !t;
    if (t) target.textContent = 'to: ' + t;
  };
  V3.bus.sub(syncTarget);
  const send = () => {
    const text = ta.value.trim();
    if (!text) return;
    ta.value = ''; ta.style.height = 'auto';
    V3.actions.sendMessage(text);
  };
  ta.addEventListener('keydown', e => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); send(); }
    else if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });
  ta.addEventListener('input', () => { ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 140) + 'px'; });
  document.getElementById('composer-send').onclick = send;
  document.getElementById('composer-mic').onclick = () => V3.actions.toggleVoice();
  document.getElementById('composer-attach').onclick = () => V3.actions.attach();
  syncTarget();
};

/* ---------------- keyboard ---------------- */
V3.showShortcuts = function () {
  const panel = document.getElementById('queue-panel');
  const drawer = document.getElementById('queue-drawer');
  const rows = [
    ['⌘K or /', 'the index — everything, one box'], ['?', 'this map'],
    ['g h / w / s / f / m / p / b', 'go: home · work · screens · files · machines · people · books'],
    ['g ,  ·  g t  ·  g u', 'settings · station · studio'],
    ['y s a n', 'queue: approve · skip · approve-all · deny'],
    ['x', 'dismiss an FYI'], ['e', 'unfold details'], ['⌘↵', 'send'], ['esc', 'close the top layer']
  ];
  panel.innerHTML =
    '<div class="row"><h2 class="voice" style="margin:0;font-size:22px">The keyboard</h2><span class="grow"></span>' +
    '<button class="icon-btn" id="queue-close">' + V3.icon('x') + '</button></div>' +
    '<div class="card"><div class="kv">' + rows.map(r =>
      '<span class="k mono">' + r[0] + '</span><span class="v" style="font-family:var(--sans)">' + r[1] + '</span>').join('') +
    '</div></div>';
  drawer.hidden = false;
  panel.querySelector('#queue-close').onclick = () => drawer.hidden = true;
  drawer.onclick = e => { if (e.target === drawer) drawer.hidden = true; };
};

let gPending = false;
V3.wireKeyboard = function () {
  document.addEventListener('keydown', e => {
    const typing = /INPUT|TEXTAREA|SELECT/.test(document.activeElement.tagName);
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); V3.palette.isOpen ? V3.palette.close() : V3.palette.open(); return; }
    if (typing) { if (e.key === 'Escape') document.activeElement.blur(); return; }
    if (V3.palette.isOpen) return;
    const drawerOpen = !document.getElementById('queue-drawer').hidden;
    if (e.key === 'Escape') { if (drawerOpen) document.getElementById('queue-drawer').hidden = true; return; }
    if (e.key === '?') { V3.showShortcuts(); return; }
    if (e.key === '/') { e.preventDefault(); V3.palette.open(); return; }
    if (gPending) {
      gPending = false;
      const map = { h: 'home', w: 'work', s: 'screens', f: 'files', m: 'machines', p: 'people', b: 'books', ',': 'settings', t: 'station', u: 'studio' };
      if (map[e.key]) { e.preventDefault(); V3.go('#/' + map[e.key]); }
      return;
    }
    if (e.key === 'g') { gPending = true; setTimeout(() => gPending = false, 900); return; }
    if ('ysanx'.includes(e.key)) {
      const top = e.key === 'x' ? V3.queueStore.fyis()[0] : V3.queueStore.pending()[0];
      if (!top) return;
      const act = { y: 'approve', s: 'skip', a: 'always', n: 'deny', x: 'dismiss' }[e.key];
      V3.actions.resolveQueueItem(top, act);
    }
  });
};

/* ---------------- boot ---------------- */
V3.boot = function () {
  V3.setTheme(V3.store.get('theme', 'lamplight'));
  document.documentElement.dataset.density = V3.store.get('density', 'standard');
  document.getElementById('density-label').textContent = V3.density();

  V3.buildRail();
  V3.wireComposer();
  V3.wireKeyboard();

  document.getElementById('btn-palette').onclick = () => V3.palette.open();
  document.getElementById('btn-theme').onclick = V3.cycleTheme;
  document.getElementById('btn-density').onclick = V3.cycleDensity;
  document.getElementById('rail-queue').onclick = V3.openQueueDrawer;

  window.addEventListener('hashchange', V3.route);
  if (!location.hash) location.hash = '#/home';

  /* transport → data → first paint; every store change re-renders chrome
     and, when the visible room asked for live updates, the view itself */
  V3.bus.sub(what => {
    if (what === 'queue') V3.renderQueueBadge();
    if (what === 'conn') V3.renderTopFacts();
    const view = V3.views[V3.current];
    if (view && view.live) { try { view.live(what); } catch (e) { console.error('[v3] live render', e); } }
  });

  V3.transport.boot().then(() => V3.data.init()).catch(err => {
    console.error('[v3] boot failed', err);
    V3.data.bootError = String(err && err.message || err);
  }).finally(() => V3.route());
};

document.addEventListener('DOMContentLoaded', V3.boot);
