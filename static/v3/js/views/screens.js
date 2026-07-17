/* V3 — Screens: see and touch your machines.
   Real daemon displays render as honest placeholder tiles (the WebRTC
   stream stays in the classic dashboard in this draft); input authority
   is wired to take_display/release_display, your-screen grants ride
   grant/revoke_user_display, and the activity feed is seeded from the
   store's rolling logs and fed live by cu_action / display_* events.
   Terminals and the agent browser are honest link cards, not fakes. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.screens = {
  title: 'Screens',
  hands: {},            /* displayId → 'agent' | 'you' — who holds the wheel */
  shared: false,        /* is your screen lent to the house? */
  shareDur: '15m',
  activity: [],         /* rolling feed: {t, kind, text} */
  _subscribed: false,

  DURS: [['15m', '15 minutes'], ['this_session', 'this session'], ['until_revoked', 'until revoked']],
  DUR_WORDS: { '15m': '15 minutes', 'this_session': 'this session', 'until_revoked': 'until I revoke it' },

  render(el) {
    this.subscribe();
    const displays = V3.data.displays;
    el.innerHTML = V3.page({
      eyebrow: 'see and touch your machines',
      title: 'Screens',
      sub: 'Every screen the house can see. Watch it work, take the wheel when you want to, or lend it yours.',
      body:
        '<div class="scr-stagegrid">' +
          '<div class="col" style="gap:var(--gap)">' +
            (displays.length ? displays.map(d => this.displayCard(d)).join('') : this.noDisplaysCard()) +
          '</div>' +
          '<div class="col" style="gap:var(--gap)">' +
            this.authorityCard() +
            this.activityCard() +
            '<button class="btn btn-quiet" id="scr-new-vdisplay">' + V3.ICON('plus', 15) + ' New virtual display</button>' +
          '</div>' +
        '</div>' +
        this.yourScreenCard() +
        V3.section('Recordings & clips') +
        this.recordingsHtml() +
        V3.section('Terminals') +
        this.terminalCard() +
        V3.section('Browser workspaces') +
        this.workspaceCard()
    });
    this.wire(el);
  },

  /* ---------------- the stage ---------------- */
  displayCard(d) {
    const you = (this.hands[d.id] || 'agent') === 'you';
    return '<div class="card">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">Display ' + V3.esc(d.id) + '</h3>' +
        '<div class="card-sub">this machine · ' + V3.fact(d.w + '×' + d.h) +
          ' · hands: ' + (you ? 'you' : 'the house') + '</div>' +
      '</div>' +
      '<div class="card-actions">' + V3.chip('live', 'sage') + '</div></div>' +
      '<div class="scr-screen"><div class="scr-placeholder">' +
        V3.ICON('screens', 30) +
        '<span class="scr-res">' + V3.esc(d.w + ' × ' + d.h) + '</span>' +
        '<span class="scr-note">The live stream lives in the classic dashboard for now — <a href="/">watch it there →</a></span>' +
      '</div></div>' +
      '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
        '<button class="btn ' + (you ? 'btn-danger' : 'btn-safe') + ' btn-xs" data-hands="' + V3.esc(d.id) + '">' +
          V3.ICON('hand', 13) + ' <span>' + (you ? 'Release' : 'Take control') + '</span></button>' +
        '<span class="dim" style="font-size:12px">Taking the wheel parks the house’s hands — it watches until you give them back.</span>' +
      '</div></div>';
  },

  noDisplaysCard() {
    return '<div class="card">' + V3.empty('screens', 'No live displays',
      'They appear when the house opens something graphical — or spin up a fresh virtual one from the rail.') + '</div>';
  },

  /* ---------------- the rail ---------------- */
  authorityCard() {
    const rows = V3.data.displays.map(d =>
      ['Display ' + d.id + ' · ' + d.w + '×' + d.h,
       (this.hands[d.id] || 'agent') === 'you' ? 'you' : 'the house']);
    rows.push(['Your screen', this.shared ? 'lent to the house · looking only' : 'you']);
    const row = (what, who) =>
      '<div class="row" style="gap:8px">' +
        '<span style="flex:1;min-width:0;font-size:13px">' + V3.esc(what) + '</span>' +
        '<span class="fact">' + V3.esc(who) + '</span></div>';
    return V3.card({
      title: 'Who may touch what',
      sub: 'Input authority, live from the daemon',
      body:
        '<div class="col" style="gap:8px;margin-top:4px">' +
          rows.map(r => row(r[0], r[1])).join('') +
        '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:10px">Taking the wheel parks the house’s hands on that screen until you release it.</div>'
    });
  },

  activityInner() {
    return this.activity.length
      ? V3.logLines(this.activity.slice(-8).map(l => [l.t, l.kind, l.text]))
      : '<div class="dim" style="font-size:12.5px;padding:6px 0">No display activity yet — when the house clicks, types, or reads a screen, it streams here.</div>';
  },
  activityCard() {
    return V3.card({
      title: 'Display activity',
      body: '<div id="scr-activity-body">' + this.activityInner() + '</div>'
    });
  },

  /* ---------------- your screen ---------------- */
  yourScreenCard() {
    const shared = this.shared;
    return '<div class="card" id="scr-your-screen">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">Your screen</h3>' +
        '<div class="card-sub">Private by default — the house may look only when you say so, for as long as you say.</div>' +
      '</div>' +
      '<div class="card-actions">' +
        (shared ? V3.chip('shared · ' + this.DUR_WORDS[this.shareDur], 'attn', 'eye')
                : V3.chip('private', 'slate', 'shield')) +
      '</div></div>' +
      '<div class="row" style="gap:8px;flex-wrap:wrap">' +
        (shared
          ? '<button class="btn btn-danger" id="scr-share-stop">' + V3.ICON('x', 15) + ' Stop sharing</button>'
          : '<button class="btn btn-safe" id="scr-share-start">' + V3.ICON('eye', 15) + ' Share with the house</button>' +
            '<span class="seg" id="scr-share-dur">' +
              this.DURS.map(d =>
                '<button class="' + (d[0] === this.shareDur ? 'on' : '') + '" data-dur="' + d[0] + '">' + d[1] + '</button>').join('') +
            '</span>') +
      '</div>' +
      (shared
        ? '<div class="dim" style="margin-top:8px;font-size:12.5px">The house can look — ' + V3.esc(this.DUR_WORDS[this.shareDur]) +
          '. Looking only, never touching; stopping is always one tap away.</div>'
        : '<div class="dim" style="margin-top:8px;font-size:12.5px">When the house asks to see your screen, the doorbell rings in the ' +
          '<a href="#/home">Queue</a> — nothing is ever shared silently.</div>') +
    '</div>';
  },

  /* ---------------- recordings & clips ---------------- */
  recordingsHtml() {
    const recs = V3.data.sessions.filter(s => (s.recordings || 0) > 0);
    if (!recs.length) {
      return V3.empty('record', 'No recordings yet',
        'When a session records its screen, the shelf fills in here.');
    }
    return '<div class="grid grid-2">' + recs.map(s =>
      '<div class="card">' +
        '<div class="card-head"><div>' +
          '<h3 class="card-title">' + V3.esc(s.name) + '</h3>' +
          '<div class="card-sub">' + V3.fact(s.recordings + ' recording' + (s.recordings === 1 ? '' : 's')) +
            (s.task ? ' · ' + V3.esc(String(s.task).slice(0, 60)) : '') + '</div>' +
        '</div></div>' +
        '<a class="scr-poster" href="/" title="The player lives in the classic dashboard for now">' +
          '<span class="scr-play">' + V3.icon('play', 22) + '</span>' +
        '</a>' +
        '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
          '<a class="btn btn-quiet btn-xs" href="/">' + V3.ICON('play', 13) + ' play in the classic dashboard</a>' +
          '<a class="btn btn-quiet btn-xs" href="/api/session/' + encodeURIComponent(s.id) + '/report" download>' +
            V3.ICON('download', 13) + ' report (.zip)</a>' +
        '</div>' +
        '<div class="dim" style="margin-top:6px;font-size:12px">The in-page player isn’t rebuilt in this draft — the classic dashboard plays these today.</div>' +
      '</div>').join('') + '</div>';
  },

  /* ---------------- honest link cards ---------------- */
  terminalCard() {
    return V3.card({
      title: 'Terminals',
      sub: 'A shell is how you touch a machine.',
      body:
        '<div class="row" style="gap:10px;flex-wrap:wrap">' +
          V3.ICON('terminal', 16) +
          '<span style="flex:1;min-width:220px;font-size:13px">The PTY rides the classic dashboard in this draft — pin one, share it, and type there.</span>' +
          '<a class="btn btn-quiet btn-xs" href="/">open terminals →</a>' +
        '</div>'
    });
  },
  workspaceCard() {
    return V3.card({
      title: 'Browser workspaces',
      sub: 'Agent-driven browsers, leased by the session that drives them.',
      body:
        '<div class="row" style="gap:10px;flex-wrap:wrap">' +
          V3.ICON('external', 16) +
          '<span style="flex:1;min-width:220px;font-size:13px">The agent browser rides the classic dashboard in this draft — leases and providers live there.</span>' +
          '<a class="btn btn-quiet btn-xs" href="/">open workspaces →</a>' +
        '</div>'
    });
  },

  /* ---------------- live subscriptions ----------------
     These events aren't normalized into V3.data (it's the shared store
     and stays lean), so the room listens on the transport itself and
     keeps the state view-local. */
  subscribe() {
    if (this._subscribed) return;
    this._subscribed = true;
    this.seedActivity();
    const self = this;

    this.push = function (kind, text) {
      self.activity.push({ t: V3.now(), kind, text });
      if (self.activity.length > 60) self.activity.splice(0, self.activity.length - 60);
      if (V3.current === 'screens') {
        const host = document.getElementById('scr-activity-body');
        if (host) host.innerHTML = self.activityInner();
      }
    };
    const rerender = function () { if (V3.current === 'screens') V3.rerender(); };

    V3.transport.on('event:cu_action', msg => {
      const s = V3.data.sessions.find(x => x.id === msg.session_id);
      const who = s ? '“' + s.name + '”' : 'the house';
      const at = (msg.x != null && msg.y != null) ? ' at (' + msg.x + ', ' + msg.y + ')' : '';
      self.push('tool', who + ' · ' + (msg.kind || 'act') + at + ' · display ' + msg.display_id);
    });
    V3.transport.on('event:display_taken', msg => {
      self.hands[msg.display_id] = 'you';
      self.push('ok', 'you took the wheel · display ' + msg.display_id);
      rerender();
    });
    V3.transport.on('event:display_released', msg => {
      self.hands[msg.display_id] = 'agent';
      self.push('', 'the house has the wheel again · display ' + msg.display_id);
      rerender();
    });
    V3.transport.on('event:display_ready', msg => {
      self.push('ok', 'display ' + msg.display_id + ' ready · ' + msg.width + '×' + msg.height);
    });
    V3.transport.on('event:user_display_granted', () => {
      self.shared = true;
      self.push('ok', 'your screen is lent — looking only');
      rerender();
    });
    V3.transport.on('event:user_display_revoked', () => {
      self.shared = false;
      self.push('', 'your screen is yours again');
      rerender();
    });
  },

  seedActivity() {
    const cuish = /click|typed|keystroke|scroll|screenshot|screen|display|cursor|computer.use/i;
    const lines = [];
    Object.keys(V3.data.logs).forEach(sid => {
      (V3.data.logs[sid] || []).forEach(l => {
        if (cuish.test(l.text || '')) {
          lines.push({ t: l.t, kind: l.kind === 'err' ? 'err' : 'tool', text: String(l.text).slice(0, 140) });
        }
      });
    });
    this.activity = lines.slice(-40);
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    const self = this;

    el.querySelectorAll('[data-hands]').forEach(b => b.addEventListener('click', () => {
      const raw = b.dataset.hands;
      const id = /^\d+$/.test(raw) ? +raw : raw;
      const you = (self.hands[raw] || 'agent') === 'you';
      if (you) V3.actions.releaseDisplay(id);
      else V3.actions.takeDisplay(id);
      self.hands[raw] = you ? 'agent' : 'you';
      V3.toast(you ? 'Released — the house has the wheel again'
                   : 'You’re at the wheel — the house’s hands are parked', you ? null : 'sage');
      V3.rerender();
    }));

    const newVd = el.querySelector('#scr-new-vdisplay');
    if (newVd) newVd.addEventListener('click', () => {
      V3.transport.send({ action: 'create_virtual_display' });
      V3.toast('Asking the house for a fresh virtual display…', 'sage');
    });

    const start = el.querySelector('#scr-share-start');
    if (start) start.addEventListener('click', () => {
      V3.actions.grantUserDisplay(self.shareDur);
      self.shared = true;
      V3.toast('Screen shared — ' + self.DUR_WORDS[self.shareDur] + ', looking only', 'sage');
      V3.rerender();
    });
    const stop = el.querySelector('#scr-share-stop');
    if (stop) stop.addEventListener('click', () => {
      V3.actions.revokeUserDisplay();
      self.shared = false;
      V3.toast('Sharing stopped — your screen is yours again', null);
      V3.rerender();
    });
    el.querySelectorAll('#scr-share-dur button').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      self.shareDur = b.dataset.dur;
    }));
  },

  live(what) {
    if (!['displays', 'sessions', 'ready'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
  }
};
