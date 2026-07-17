/* Proscenium — app shell: router, rail, topbar, composer, keyboard,
   theme/density engines, queue drawer, demo event injection. */
window.P = window.P || {};

/* The rooms (palette + rail derive from this one table) */
P.ROOMS = [
  { id: 'home',     title: 'Home',          icon: 'arch',     gloss: 'the conversation · needs you' },
  { id: 'work',     title: 'Work',          icon: 'stage',    gloss: 'sessions on stage' },
  { id: 'screens',  title: 'Screens',       icon: 'screens',  gloss: 'see & touch your machines' },
  { id: 'files',    title: 'Files',         icon: 'files',    gloss: 'the desk · transfers' },
  { id: 'machines', title: 'Machines',      icon: 'machines', gloss: 'the fleet · pairing · delegation' },
  { id: 'people',   title: 'People & Keys', icon: 'key',      gloss: 'who may do what · vault' },
  { id: 'books',    title: 'Books',         icon: 'books',    gloss: 'costs · the list · memory' },
  { id: 'settings', title: 'Settings',      icon: 'settings', gloss: 'how the house behaves' }
];
P.VANTAGES = [
  { id: 'station', title: 'Station', icon: 'station', gloss: 'the constellation' },
  { id: 'studio',  title: 'Studio',  icon: 'studio',  gloss: 'the machinery, raw' }
];

P.current = null;

P.go = function (hash) {
  if (location.hash === hash) P.route();
  else location.hash = hash;
};
P.rerender = function () { P.route(); };

P.route = function () {
  const raw = (location.hash || '#/home').replace(/^#\/?/, '');
  const parts = raw.split('/').filter(Boolean);
  let viewId = parts[0] || 'home';
  let params = parts.slice(1);

  /* session space lives under work */
  if (viewId === 'work' && params[0] === 'session') { viewId = 'session'; params = params.slice(1); }
  if (!P.views[viewId]) { viewId = 'home'; params = []; }

  P.current = viewId;
  const view = P.views[viewId];
  const main = document.getElementById('main');
  main.innerHTML = '';
  view.render(main, params);
  P.mountIcons(main);
  main.scrollTop = 0;

  /* rail active state */
  document.querySelectorAll('#rail-rooms .rail-item, #rail-vantages .rail-item').forEach(a => {
    a.classList.toggle('on', a.dataset.room === viewId || (viewId === 'session' && a.dataset.room === 'work'));
  });

  /* topbar */
  const room = P.ROOMS.concat(P.VANTAGES).find(r => r.id === viewId);
  document.getElementById('topbar-title').textContent =
    viewId === 'session' ? 'Work' : room ? room.title : '';
  document.getElementById('topbar-facts').innerHTML =
    '<span>' + P.data.sessions.filter(s => s.phase === 'working').length + ' working</span>' +
    '<span>' + P.queueStore.pending().length + ' need you</span>' +
    '<span class="studio-only">direct mTLS · tunnel live</span>';

  P.renderQueueBadge();
  P.renderRailIdentity();
};

/* ---------------- rail ---------------- */
P.buildRail = function () {
  const rooms = document.getElementById('rail-rooms');
  rooms.innerHTML = P.ROOMS.map(r =>
    '<a class="rail-item" data-room="' + r.id + '" href="#/' + r.id + '" title="' + P.esc(r.gloss) + '">' +
    '<span class="icon">' + P.icon(r.icon) + '</span><span class="rail-label">' + r.title + '</span>' +
    (r.id === 'work' ? '<span class="rail-sub">' + P.data.sessions.filter(s => s.phase === 'working').length + '</span>' : '') +
    '</a>').join('');
  document.getElementById('rail-vantages').innerHTML =
    '<div class="rail-group-label">Vantages</div>' +
    P.VANTAGES.map(r =>
      '<a class="rail-item" data-room="' + r.id + '" href="#/' + r.id + '" title="' + P.esc(r.gloss) + '">' +
      '<span class="icon">' + P.icon(r.icon) + '</span><span class="rail-label">' + r.title + '</span></a>').join('');
  P.mountIcons(document.getElementById('rail'));
};

P.renderRailIdentity = function () {
  const you = P.data.you;
  document.getElementById('rail-identity').innerHTML =
    '<div class="authline"><span class="who">' + P.esc(you.name) + ' · ' + you.role + '</span>' +
    P.routeChip(you.route.replace(' mTLS', '')) + '</div>';
};

/* ---------------- queue badge + drawer ---------------- */
P.renderQueueBadge = function () {
  const n = P.queueStore.pending().length;
  const badge = document.getElementById('rail-queue-count');
  badge.hidden = n === 0;
  badge.textContent = n;
  document.title = (n ? '(' + n + ') ' : '') + 'Proscenium — the Intendant dashboard, reimagined';
  const facts = document.getElementById('topbar-facts');
  if (facts) facts.querySelectorAll('span')[1].textContent = n + ' need you';
};

P.openQueueDrawer = function () {
  const drawer = document.getElementById('queue-drawer');
  const panel = document.getElementById('queue-panel');
  const pending = P.queueStore.pending();
  const fyis = P.queueStore.fyis();
  panel.innerHTML =
    '<div class="row"><h2 class="voice" style="margin:0;font-size:22px">Needs you</h2><span class="grow"></span>' +
    '<button class="icon-btn" id="queue-close">' + P.icon('x') + '</button></div>' +
    (pending.length || fyis.length
      ? pending.map(q => P.decisionCard(q)).join('') +
        (fyis.length ? '<div class="eyebrow" style="margin-top:8px">for your awareness</div>' + fyis.map(q => P.decisionCard(q)).join('') : '')
      : '<div class="queue-free"><span class="big">You’re free.</span>Nothing needs you. The house will tap you the moment something does.</div>');
  drawer.hidden = false;
  panel.querySelector('#queue-close').onclick = () => drawer.hidden = true;
  drawer.onclick = e => { if (e.target === drawer) drawer.hidden = true; };
};

/* ---------------- theme & density ---------------- */
P.setTheme = function (t) {
  document.documentElement.dataset.theme = t;
  P.store.set('theme', t);
};
P.cycleTheme = function () {
  P.setTheme(document.documentElement.dataset.theme === 'lamplight' ? 'daylight' : 'lamplight');
};
P.setDensity = function (d) {
  document.documentElement.dataset.density = d;
  P.store.set('density', d);
  document.getElementById('density-label').textContent = d;
  P.rerender();
};
P.cycleDensity = function () {
  const order = ['cozy', 'standard', 'studio'];
  const next = order[(order.indexOf(P.density()) + 1) % order.length];
  P.setDensity(next);
  P.toast('Density → ' + next, null);
};

/* ---------------- composer ---------------- */
P.wireComposer = function () {
  const ta = document.getElementById('composer-input');
  const send = () => {
    const text = ta.value.trim();
    if (!text) return;
    ta.value = ''; ta.style.height = 'auto';
    if (P.current === 'home') P.views.home.onSend(text);
    else { P.go('#/home'); setTimeout(() => P.views.home.onSend(text), 250); }
  };
  ta.addEventListener('keydown', e => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); send(); }
    else if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });
  ta.addEventListener('input', () => { ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 140) + 'px'; });
  document.getElementById('composer-send').onclick = send;
  document.getElementById('composer-mic').onclick = () =>
    P.toast('Live voice would connect here — the thread is the same one you’re reading', null);
  document.getElementById('composer-attach').onclick = () =>
    P.toast('Attachments stage here and ride the next message', null);
};

/* ---------------- keyboard ---------------- */
P.showShortcuts = function () {
  const panel = document.getElementById('queue-panel');
  const drawer = document.getElementById('queue-drawer');
  const rows = [
    ['⌘K or /', 'the index — everything, one box'], ['?', 'this map'],
    ['g h / w / s / f / m / p / b', 'go: home · work · screens · files · machines · people · books'],
    ['g ,  ·  g t  ·  g u', 'settings · station · studio'],
    ['y s a n', 'queue: approve · skip · approve-all · deny'],
    ['j k', 'move through the queue'], ['x', 'dismiss an FYI'],
    ['e', 'unfold details'], ['⌘↵', 'send'], ['esc', 'close the top layer']
  ];
  panel.innerHTML =
    '<div class="row"><h2 class="voice" style="margin:0;font-size:22px">The keyboard</h2><span class="grow"></span>' +
    '<button class="icon-btn" id="queue-close">' + P.icon('x') + '</button></div>' +
    '<div class="card"><div class="kv">' + rows.map(r =>
      '<span class="k mono">' + r[0] + '</span><span class="v" style="font-family:var(--sans)">' + r[1] + '</span>').join('') +
    '</div></div>';
  drawer.hidden = false;
  panel.querySelector('#queue-close').onclick = () => drawer.hidden = true;
  drawer.onclick = e => { if (e.target === drawer) drawer.hidden = true; };
};

let gPending = false;
P.wireKeyboard = function () {
  document.addEventListener('keydown', e => {
    const typing = /INPUT|TEXTAREA/.test(document.activeElement.tagName);
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); P.palette.isOpen ? P.palette.close() : P.palette.open(); return; }
    if (typing) { if (e.key === 'Escape') document.activeElement.blur(); return; }
    if (P.palette.isOpen) return;
    const drawerOpen = !document.getElementById('queue-drawer').hidden;
    if (e.key === 'Escape') { if (drawerOpen) document.getElementById('queue-drawer').hidden = true; return; }
    if (e.key === '?') { P.showShortcuts(); return; }
    if (e.key === '/') { e.preventDefault(); P.palette.open(); return; }
    if (gPending) {
      gPending = false;
      const map = { h: 'home', w: 'work', s: 'screens', f: 'files', m: 'machines', p: 'people', b: 'books', ',': 'settings', t: 'station', u: 'studio' };
      if (map[e.key]) { e.preventDefault(); P.go('#/' + map[e.key]); }
      return;
    }
    if (e.key === 'g') { gPending = true; setTimeout(() => gPending = false, 900); return; }
    /* queue grammar */
    if ('ysanx'.includes(e.key) && P.current === 'home') {
      const top = e.key === 'x' ? P.queueStore.fyis()[0] : P.queueStore.pending()[0];
      if (!top) return;
      const act = { y: 'approve', s: 'skip', a: 'always', n: 'deny', x: 'dismiss' }[e.key];
      const valid = top.actions.find(a => a.id === act) ? act : (e.key === 'y' ? top.actions[0].id : null);
      if (valid) P.queueStore.resolve(top.id, valid);
    }
  });
};

/* ---------------- demo event injection ---------------- */
P.wireSimulate = function () {
  const scenarios = [
    {
      label: 'an approval arrives', run() {
        P.data.queue.unshift({
          id: 'q-sim-' + Date.now(), kind: 'approval', sev: 'attention', when: 'just now',
          category: 'command_exec', session: 'docs-sweep', machine: 'dell', backend: 'Codex',
          title: 'Codex wants to run the link fixer',
          consequence: 'On Workshop — it rewrites dead links in 41 chapters. Reversible via git; nothing leaves the machine.',
          details: { command: 'python3 scripts/fix_links.py --apply docs/src/', paths: [], rule: 'Always allowing would set command_exec → Auto for docs work.', raw: '{ "category": "command_exec", "tool": "local_bash" }' },
          actions: [
            { id: 'approve', label: 'Allow once', kind: 'safe', key: 'y' },
            { id: 'always', label: 'Always allow docs fixes', kind: 'quiet', key: 'a' },
            { id: 'deny', label: 'Deny', kind: 'danger', key: 'n' }
          ]
        });
      }
    },
    {
      label: 'a task finishes', run() {
        P.data.conversation.push(
          { kind: 'milestone', text: 'docs-sweep finished · 96 chapters, 3 dead links fixed' },
          { from: 'presence', at: 'now', prose: ['**docs-sweep** is done — three dead links fixed, one ambiguous one left in the list for you. Workshop is idle again.'] }
        );
        const s = P.data.sessions.find(x => x.id === 'docs-sweep');
        if (s) { s.phase = 'done'; s.sentence = 'Finished — 3 dead links fixed, 1 needs your call'; }
      }
    },
    {
      label: 'a doorbell rings', run() {
        P.data.queue.unshift({
          id: 'q-sim-' + Date.now(), kind: 'display', sev: 'attention', when: 'just now',
          session: 'photo-book', machine: 'local',
          title: '“photo-book” wants to show you something',
          consequence: 'Shared view — it presents the cover full-screen on your display. You can dismiss it any time; it never takes input.',
          durations: ['Until I dismiss it'],
          actions: [
            { id: 'allow', label: 'Show me', kind: 'safe', key: 'y' },
            { id: 'deny', label: 'Later', kind: 'quiet', key: 'n' }
          ]
        });
      }
    }
  ];
  let i = 0;
  document.getElementById('btn-simulate').onclick = () => {
    const sc = scenarios[i++ % scenarios.length];
    sc.run();
    P.toast('Demo: ' + sc.label, null);
    P.renderQueueBadge();
    if (P.current === 'home') P.rerender();
    else P.openQueueDrawer();
  };
};

/* ---------------- boot ---------------- */
P.boot = function () {
  P.setTheme(P.store.get('theme', 'lamplight'));
  document.documentElement.dataset.density = P.store.get('density', 'standard');
  document.getElementById('density-label').textContent = P.density();

  P.buildRail();
  P.wireComposer();
  P.wireKeyboard();
  P.wireSimulate();

  document.getElementById('btn-palette').onclick = () => P.palette.open();
  document.getElementById('btn-theme').onclick = P.cycleTheme;
  document.getElementById('btn-density').onclick = P.cycleDensity;
  document.getElementById('rail-queue').onclick = P.openQueueDrawer;
  document.getElementById('proto-banner-close').onclick = () => {
    document.getElementById('proto-banner').remove();
    document.getElementById('app').style.paddingTop = '0';
  };

  window.addEventListener('hashchange', P.route);
  if (!location.hash) location.hash = '#/home';
  P.route();
  P.mountIcons(document);

  /* first-paint composer greeting rotation */
  const hints = [
    'Ask the house anything…',
    '“tidy my downloads folder”',
    '“@fix-login try the tests again”',
    '“/codex review the diff”',
    '“what did the backup do last night?”'
  ];
  let hi = 0;
  setInterval(() => {
    const ta = document.getElementById('composer-input');
    if (ta && !ta.value) ta.placeholder = hints[hi = (hi + 1) % hints.length];
  }, 4200);
};

document.addEventListener('DOMContentLoaded', P.boot);
