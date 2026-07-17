/* V3 — Studio: the machinery, raw.
   Workbench (transport lanes, ping) → raw state → live event stream →
   reference (keyboard, what V3 doesn't rebuild yet) → the component field
   guide: every shared component, every state, labeled and inert.
   The transport has no off(), so the event tap registers ONCE and writes
   only while its DOM node is mounted — nothing outlives the view. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.studio = {
  title: 'Studio',

  _events: [],        /* ring buffer, last 30 bus events */
  _seen: 0,
  _paused: false,
  _tapped: false,
  _ping: null,        /* {ms, ok, at, error} */

  KEYS: [
    ['⌘K or /', 'the index — everything, one box'], ['?', 'this map'],
    ['g h / w / s / f / m / p / b', 'go: home · work · screens · files · machines · people · books'],
    ['g ,  ·  g t  ·  g u', 'settings · station · studio'],
    ['y s a n', 'queue: approve · skip · approve-all · deny'],
    ['x', 'dismiss an FYI'], ['e', 'unfold details'], ['⌘↵', 'send'], ['esc', 'close the top layer']
  ],

  MISSING: [
    ['Managed-context composer', 'anchors, rewinds, and fission for a session’s context'],
    ['Credential custody', 'vault leases, egress registrations, the custody trail — tunnel-only in this build'],
    ['WebRTC display stream', 'live frames and input — Screens shows state, not the video'],
    ['PTY terminal', 'the supervised shell runs on the classic dashboard'],
    ['Station WebGPU canvas', 'the free-camera constellation renders there, not here']
  ],

  render(el) {
    /* hygiene: no timers are owned by this view; keep the invariant explicit */
    if (this._timer) { clearInterval(this._timer); this._timer = null; }
    this._paused = false;

    const D = V3.data;
    const T = V3.transport;
    const snapshot = {
      sessions: (D.sessions || []).length,
      queue: (D.queue || []).length,
      machines: (D.machines || []).length,
      config: T.config || null
    };

    el.innerHTML = V3.page({
      eyebrow: 'vantage · the machinery, raw',
      title: 'Studio',
      sub: 'Everything the friendly rooms polish away: the lanes, the snapshot, the event stream — and the component contract itself, made visible.',
      body:
        /* ---------- Workbench ---------- */
        V3.section('Workbench') +
        '<div class="grid grid-2">' +
          V3.card({
            title: 'Transport lanes', sub: 'two lanes carry V3 in this draft — each labeled by what it carries',
            body: '<div class="col" style="gap:12px">' +
              this.lane('HTTP', '/api', T.config ? 'sage' : 'attn', T.config ? 'config loaded' : 'config not loaded yet',
                'JSON routes — sessions, agenda, people, the ledgers') +
              this.lane('WebSocket', '/ws',
                T.state === 'live' ? 'sage' : T.state === 'connecting' ? 'attn' : 'brick',
                T.state + (T.connectionId ? ' · conn ' + T.connectionId.slice(0, 8) : ''),
                'the event stream — vitals, queue, presence, ControlMsgs out') +
              this.lane('WebRTC', 'tunnel', 'slate', 'unused by V3 in this draft',
                'display frames & input travel the classic dashboard’s tunnel') +
            '</div>'
          }) +
          V3.card({
            title: 'Diagnostics', sub: 'a ping against the daemon’s own HTTP lane',
            body:
              '<div class="row">' +
                '<button class="btn btn-primary" id="std-ping">Ping /config</button>' +
                '<span class="fact" id="std-ping-result">' + this.pingFact() + '</span>' +
              '</div>' +
              '<div class="dim" style="font-size:12px;margin-top:10px">One GET, timed in the browser. The daemon’s deeper self-tests stay on the classic dashboard; this proves the lane this page actually rides.</div>'
          }) +
        '</div>' +

        /* ---------- Raw state ---------- */
        V3.foldHtml({ key: 'studio.raw', title: 'Raw state', note: 'the snapshot · live event stream', open: true,
          body:
          '<div class="eyebrow" style="margin-bottom:6px">Store snapshot (trimmed)</div>' +
          '<div class="panel mono std-json">' + V3.esc(JSON.stringify(snapshot, null, 2)) + '</div>' +
          '<div class="row" style="margin:16px 0 6px">' +
            '<span class="eyebrow">Event stream</span>' +
            '<span class="fact" id="std-evt-count">live · ' + this._seen + ' seen</span>' +
            '<span class="grow"></span>' +
            '<button class="btn btn-quiet btn-xs" id="std-evt-pause">pause</button>' +
          '</div>' +
          '<div class="log panel std-events" id="std-events">' + this.eventsHtml() + '</div>' }) +

        /* ---------- Reference ---------- */
        V3.section('Reference') +
        '<div class="grid grid-2">' +
          V3.card({
            title: 'The keyboard', sub: 'the same map the ? overlay shows',
            body: '<div class="kv">' + this.KEYS.map(r =>
              '<span class="k mono">' + V3.esc(r[0]) + '</span><span class="v" style="font-family:var(--sans)">' + V3.esc(r[1]) + '</span>').join('') + '</div>'
          }) +
          V3.card({
            title: 'What V3 doesn’t rebuild yet', sub: 'honest gaps — each opens the classic dashboard, where it lives',
            body: '<div class="col" style="gap:8px">' + this.MISSING.map(m =>
              '<div class="row" style="align-items:flex-start">' + V3.ICON('external', 13) +
                '<span style="font-size:13px"><b>' + V3.esc(m[0]) + '</b> — <span class="dim">' + V3.esc(m[1]) + '</span></span>' +
                '<span class="grow"></span><a class="chip chip-slate" href="/">classic →</a></div>').join('') + '</div>'
          }) +
        '</div>' +

        /* ---------- The component field guide ---------- */
        V3.section('The component field guide — every state, labeled') +
        '<div class="dim" style="font-size:12.5px;margin-top:-6px">The contract made visible. Specimens marked inert don’t act; everything else behaves.</div>' +
        '<div class="std-guide">' + this.guide() + '</div>'
    });

    this.wire(el);
    this.ensureTap();
  },

  lane(name, path, tone, state, carries) {
    return '<div class="row">' + V3.dot(tone, tone === 'sage') +
      '<span class="mono" style="width:150px;flex:none">' + V3.esc(name) + ' <span class="dim">' + V3.esc(path) + '</span></span>' +
      V3.chip(state, tone) +
      '<span class="dim" style="font-size:12.5px">' + V3.esc(carries) + '</span></div>';
  },

  pingFact() {
    const p = this._ping;
    if (!p) return 'idle — never pinged';
    if (!p.ok) return 'failed · ' + (p.error || '') + ' · ' + p.at;
    return p.ms + ' ms · ' + p.at;
  },

  /* ---------------- event stream ---------------- */
  ensureTap() {
    if (this._tapped) return;
    this._tapped = true;
    V3.transport.on('event:*', msg => {
      if (V3.views.studio._paused) return;
      const self = V3.views.studio;
      self._seen++;
      const brief = msg.session_id ? ' · ' + String(msg.session_id).slice(0, 8)
        : msg.phase ? ' · ' + msg.phase : '';
      self._events.push([self.stamp(), msg.event || '?', brief]);
      if (self._events.length > 30) self._events.splice(0, self._events.length - 30);
      const log = document.getElementById('std-events');
      if (log) { log.innerHTML = self.eventsHtml(); log.scrollTop = log.scrollHeight; }
      const count = document.getElementById('std-evt-count');
      if (count) count.textContent = 'live · ' + self._seen + ' seen';
    });
  },

  eventsHtml() {
    if (!this._events.length) {
      return '<div class="dim" style="font-size:12px">Quiet so far — the daemon’s events land here as they stream.</div>';
    }
    return this._events.map(l =>
      '<div><span class="lt">' + l[0] + '</span> <span>' + V3.esc(l[1]) + '</span><span class="dim">' + V3.esc(l[2]) + '</span></div>').join('');
  },

  stamp() {
    const d = new Date();
    const p = n => String(n).padStart(2, '0');
    return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
  },

  /* ---------------- field guide ---------------- */
  guide() {
    const spec = (label, html, wide) =>
      '<div class="std-spec' + (wide ? ' std-wide' : '') + '"><div class="std-spec-label">' + V3.esc(label) + '</div>' + html + '</div>';

    /* the live component returns '' for anything not in the queue (it hides
       resolved items), so the guide ships a static specimen in the same
       classes — a labeled mirror of the markup, never a component call.
       data-q/data-action stay on the buttons so the inert wrapper can
       demonstrate the guard; the id is never in the real queue, so the
       global delegation in v3-ui.js no-ops even if the wrapper is bypassed. */
    const decisionSpecimen =
      '<div class="decision">' +
        '<div class="decision-head">' + V3.chip('needs you · file_delete', 'attn', 'doorbell') +
          '<span class="grow"></span><span class="fact">now</span></div>' +
        '<div class="decision-title">Claude wants to delete 3 files</div>' +
        '<div class="decision-consequence">A static specimen of the decision card — the real ones live in the Queue. These buttons are inert.</div>' +
        '<div class="panel mono" style="margin-bottom:10px">$ rm exports/old.csv</div>' +
        '<div class="decision-actions">' +
          '<button class="btn btn-safe" data-q="demo-approve" data-action="approve">Allow once <span class="kbd-hint">y</span></button>' +
          '<button class="btn btn-quiet" data-q="demo-approve" data-action="always">Always allow <span class="kbd-hint">a</span></button>' +
          '<button class="btn btn-danger btn-default" data-q="demo-approve" data-action="deny">Deny <span class="kbd-hint">n</span></button>' +
        '</div>' +
        V3.authline('you', 'owner', 'direct') +
      '</div>';
    const demoStage = {
      id: 'demo-stage', name: 'demo-working', backend: 'claude-code', model: 'claude-fable',
      machine: 'local', phase: 'working', active: true, turn: 5,
      sentence: 'A specimen stage card — working, with its facts and meter',
      tokens: { used: 82000, pct: 41 }, cost: 0.42
    };
    const demoStagePeer = {
      id: 'demo-stage-peer', name: 'demo-peer', backend: 'codex', model: 'gpt-5.2-codex',
      machine: 'workshop', phase: 'working', active: true, turn: 2,
      sentence: 'The peer tone — a session reported by another machine',
      tokens: { used: 40000, pct: 15 }, cost: 0.11
    };

    return [
      spec('chips — every kind', '<div class="row" style="flex-wrap:wrap">' +
        V3.chip('default') + V3.chip('sage', 'sage') + V3.chip('slate', 'slate') +
        V3.chip('brass', 'brass') + V3.chip('attn', 'attn') + V3.chip('brick', 'brick') +
        V3.chip('violet', 'violet') + V3.chip('with icon', 'attn', 'doorbell') + '</div>'),

      spec('dots — always with a word', '<div class="factline">' +
        '<span class="fact">' + V3.dot('sage') + ' sage</span>' +
        '<span class="fact">' + V3.dot('sage', true) + ' pulsing</span>' +
        '<span class="fact">' + V3.dot('attn') + ' attn</span>' +
        '<span class="fact">' + V3.dot('brick') + ' brick</span>' +
        '<span class="fact">' + V3.dot('slate') + ' slate</span></div>'),

      spec('facts & meters — incl. warn / hot', '<div class="col" style="gap:8px">' +
        '<div>' + V3.fact('12 ms · the instrument register') + '</div>' +
        '<div class="row">' + V3.fact('34%') + V3.meter(34) + '</div>' +
        '<div class="row">' + V3.fact('74% warn') + V3.meter(74) + '</div>' +
        '<div class="row">' + V3.fact('92% hot') + V3.meter(92) + '</div></div>'),

      spec('buttons', '<div class="row" style="flex-wrap:wrap">' +
        '<button class="btn">default</button>' +
        '<button class="btn btn-primary">primary</button>' +
        '<button class="btn btn-safe">safe</button>' +
        '<button class="btn btn-danger">danger</button>' +
        '<button class="btn btn-quiet">quiet</button>' +
        '<button class="btn btn-xs">xs</button>' +
        '<button class="btn" disabled>disabled</button>' +
        '<button class="icon-btn">' + V3.ICON('sparkle', 15) + '</button></div>'),

      spec('segmented control', '<span class="seg"><button class="on">auto</button><button>ask</button><button>deny</button></span>' +
        '<div class="dim" style="font-size:12px;margin-top:6px">approval rules use these — live-applied; the specimen is unwired</div>'),

      spec('fold — closed & open', '<div class="col" style="gap:8px">' +
        V3.foldHtml({ title: 'A closed fold', note: 'keyless, forgets itself', open: false, body: '<div class="dim" style="font-size:13px">Power unfolds where you look for it.</div>' }) +
        V3.foldHtml({ title: 'An open fold', note: 'studio opens by default', open: true, body: '<div class="dim" style="font-size:13px">Same contract, opened. Keyless specimens don’t remember state.</div>' }) +
        '</div>'),

      spec('decision card — specimen, inert',
        '<div class="std-specimen" id="std-specimen">' + decisionSpecimen + '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:6px">A static markup specimen — V3.decisionCard renders only live queue items. Wrapped so its buttons toast instead of resolving.</div>', true),

      spec('stage cards — working & peer tones', '<div class="std-specimen" data-inert-links="1"><div class="grid grid-2">' +
        V3.stageCard(demoStage) + V3.stageCard(demoStagePeer) + '</div></div>' +
        '<div class="dim" style="font-size:12px;margin-top:6px">The attention tone isn’t staged — it glows when the real queue holds an item for the session. Links are inert here.</div>', true),

      spec('empty state', V3.empty('stage', 'Nothing on stage', 'Every room ships its empty state — a serif line and one honest next action.')),

      spec('skeleton — loading, never a lone spinner', '<div class="col" style="gap:8px">' +
        V3.skeleton(14, '72%') + V3.skeleton(14, '100%') + V3.skeleton(14, '45%') + '</div>'),

      spec('kv grid', '<div class="kv">' +
        '<span class="k">branch</span><span class="v">fix/login-redirect</span>' +
        '<span class="k">working tree</span><span class="v">12 dirty files</span>' +
        '<span class="k">cache</span><span class="v">94% · ttl 4m 12s</span></div>'),

      spec('table', '<table class="table"><thead><tr><th>Session</th><th>Cost</th><th>State</th></tr></thead><tbody>' +
        '<tr><td>fix-login</td><td class="mono">$0.83</td><td>' + V3.chip('working', 'sage') + '</td></tr>' +
        '<tr><td>docs-sweep</td><td class="mono">$0.31</td><td>' + V3.chip('working', 'sage') + '</td></tr>' +
        '<tr><td>q2-invoices</td><td class="mono">$1.12</td><td>' + V3.chip('idle', 'slate') + '</td></tr>' +
        '</tbody></table>', true),

      spec('log lines', V3.logLines([
        ['21:04', 'model', 'Reading src/auth/session.rs'],
        ['21:04', 'tool', '$ cargo test --bins'],
        ['21:05', 'ok', '42 passed · 0 failed'],
        ['21:05', 'err', 'warning: unused import'],
        ['21:06', 'info', 'turn 6 complete']
      ])),

      spec('authline — who acted, by what route', '<div class="col" style="gap:6px">' +
        V3.authline('you', 'owner', 'direct') +
        V3.authline('Workshop', 'operator', 'fleet name') +
        V3.authline('a hosted tab', 'role:none', 'hosted') + '</div>'),

      spec('toast', '<button class="btn btn-quiet" id="std-toast-demo">fire a toast</button>' +
        '<span class="dim" style="font-size:12px;margin-left:8px">bottom-right · 3.2 s · sage or brick</span>')
    ].join('');
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    /* ping */
    const pingBtn = el.querySelector('#std-ping');
    if (pingBtn) pingBtn.addEventListener('click', () => {
      const t0 = performance.now();
      const node = el.querySelector('#std-ping-result');
      if (node) node.textContent = 'pinging…';
      V3.transport.get('/config').then(() => {
        this._ping = { ok: true, ms: Math.round(performance.now() - t0), at: this.stamp() };
      }).catch(e => {
        this._ping = { ok: false, error: e.message, at: this.stamp() };
      }).then(() => {
        const n = document.getElementById('std-ping-result');
        if (n && V3.current === 'studio') n.textContent = this.pingFact();
        if (this._ping.ok) V3.toast('The house answered in ' + this._ping.ms + ' ms', 'sage');
      });
    });

    /* event stream pause */
    const pauseBtn = el.querySelector('#std-evt-pause');
    if (pauseBtn) pauseBtn.addEventListener('click', () => {
      this._paused = !this._paused;
      pauseBtn.textContent = this._paused ? 'resume' : 'pause';
    });

    /* field guide: toast demo */
    const td = el.querySelector('#std-toast-demo');
    if (td) td.addEventListener('click', () => V3.toast('The toast — brief, warm, and gone', 'sage'));

    /* field guide: specimens stay inert — no queue resolution, no navigation */
    const specimen = el.querySelector('#std-specimen');
    if (specimen) specimen.addEventListener('click', e => {
      if (e.target.closest('[data-q][data-action]')) {
        e.stopPropagation();
        e.preventDefault();
        V3.toast('A specimen — its buttons are inert. The real ones live in the Queue.', null);
      }
    });
    const links = el.querySelector('[data-inert-links]');
    if (links) links.addEventListener('click', e => {
      if (e.target.closest('a')) { e.preventDefault(); V3.toast('A specimen — the real cards navigate to their session.', null); }
    });
  },

  live(what) {
    if (!['conn', 'sessions', 'queue', 'machines', 'ready'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA|SELECT/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
  }
};
