/* Proscenium — Screens: see and touch your machines.
   Live displays on stage, input authority in the rail, recordings,
   a terminal, and browser workspaces. The agent cursor drifts on its
   own and parks whenever you take the wheel. */
window.P = window.P || {};
P.views = P.views || {};

P.views.screens = {
  title: 'Screens',
  hands: 'agent',        /* who holds Display 1: 'agent' (fix-login) or 'you' */
  shared: false,         /* is your screen lent to the house? */
  shareDur: '15 minutes',

  render(el) {
    const d = {};
    P.data.displays.forEach(x => { d[x.id] = x; });
    el.innerHTML = P.page({
      eyebrow: 'see and touch your machines',
      title: 'Screens',
      sub: 'Every screen the house can see. Watch it work, take the wheel when you want to, or lend it yours.',
      body:
        '<div class="scr-stagegrid">' +
          '<div class="col" style="gap:var(--gap)">' +
            this.displayCard(d['disp-1']) +
            this.peerDisplayCard(d['disp-2']) +
          '</div>' +
          '<div class="col" style="gap:var(--gap)">' +
            this.authorityCard() +
            this.activityCard() +
            '<button class="btn btn-quiet" id="scr-new-vdisplay">' + P.ICON('plus', 15) + ' New virtual display</button>' +
          '</div>' +
        '</div>' +
        this.yourScreenCard(d['disp-0']) +
        P.section('Recordings & clips') +
        '<div class="grid grid-2">' + P.data.recordings.map(r => this.recordingCard(r)).join('') + '</div>' +
        P.section('Terminals') +
        this.terminalCard() +
        P.section('Browser workspaces') +
        this.workspaceCard()
    });
    this.wire(el);
  },

  /* ---------------- the stage ---------------- */
  displayCard(d) {
    const you = this.hands === 'you';
    return '<div class="card">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">' + P.esc(d.name) + '</h3>' +
        '<div class="card-sub">' + P.esc(P.machineName(d.machine)) + ' · ' + P.fact(d.res) +
          ' · <span id="scr-hands-line">hands: ' + (you ? 'you' : 'agent (fix-login)') + '</span></div>' +
      '</div>' +
      '<div class="card-actions">' + P.chip('live', 'sage') + '</div></div>' +
      '<div class="scr-screen" id="scr-screen-1">' +
        '<div class="scr-browser">' +
          '<div class="scr-browser-bar">' +
            '<span class="scr-stoplight"></span><span class="scr-stoplight"></span><span class="scr-stoplight"></span>' +
            '<span class="scr-url">https://staging.shopify.example/login</span>' +
          '</div>' +
          '<div class="scr-banner">' + P.ICON('warn', 14) + '<span>Too many redirects — the site sent you in a loop</span></div>' +
          '<div class="scr-page">' +
            '<div class="scr-page-title">Sign in</div>' +
            '<div class="scr-field"></div>' +
            '<div class="scr-field"></div>' +
            '<div class="scr-btn"></div>' +
            '<div class="scr-field scr-ghost"></div>' +
          '</div>' +
        '</div>' +
        '<div class="scr-cursor" id="scr-cursor"' + (you ? ' style="display:none"' : '') + '>' +
          '<span class="scr-cursor-dot"></span><span class="scr-cursor-tag">fix-login</span>' +
        '</div>' +
      '</div>' +
      '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
        '<button class="btn ' + (you ? 'btn-danger' : 'btn-safe') + ' btn-xs" id="scr-take-control">' +
          P.ICON('hand', 13) + ' <span>' + (you ? 'Release' : 'Take control') + '</span></button>' +
        ['Stream', 'Attach frame', 'Annotate', 'Record', 'Fullscreen'].map(t =>
          '<button class="btn btn-quiet btn-xs" data-scr-tool="' + t + '">' + t + '</button>').join('') +
      '</div></div>';
  },

  peerDisplayCard(d) {
    return '<div class="card">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">' + P.esc(d.name) + '</h3>' +
        '<div class="card-sub">' + P.esc(P.machineName(d.machine)) + ' · ' + P.fact(d.res) + ' · hands: agent (docs-sweep)</div>' +
      '</div>' +
      '<div class="card-actions">' + P.chip('live', 'sage') + P.routeChip('fleet name') + '</div></div>' +
      '<div class="scr-screen scr-cool"><div class="scr-term-screen">' +
        P.logLines([
          ['10:07:12', 'tool', 'mdbook-linkcheck docs/src — 96 chapters queued'],
          ['10:09:31', 'ok', 'chapters 1–41 clean'],
          ['10:12:44', 'err', 'stale route reference · docs/src/peer-federation.md:41'],
          ['10:15:20', 'tool', 'grep -r "http://" docs/src — 31 hits'],
          ['10:18:02', 'ok', '41 of 96 chapters scanned — no dead links yet']
        ]) +
        '<div style="margin-top:6px"><span class="scr-caret"></span></div>' +
      '</div></div>' +
      '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
        ['Stream', 'Attach frame', 'Record'].map(t =>
          '<button class="btn btn-quiet btn-xs" data-scr-tool="' + t + '">' + t + '</button>').join('') +
        '<span class="grow"></span>' +
        '<span class="dim" style="font-size:12px">Watching only — touching Workshop’s screen needs Workshop’s own say-so.</span>' +
      '</div></div>';
  },

  /* ---------------- the rail ---------------- */
  authorityCard() {
    const row = (what, who, id) =>
      '<div class="row" style="gap:8px">' +
        '<span style="flex:1;min-width:0;font-size:13px">' + P.esc(what) + '</span>' +
        '<span class="fact"' + (id ? ' id="' + id + '"' : '') + '>' + P.esc(who) + '</span></div>';
    return P.card({
      title: 'Who may touch what',
      sub: 'Input authority, with its session',
      body:
        '<div class="col" style="gap:8px;margin-top:4px">' +
          row('Display 1 · staging browser', this.hands === 'you' ? 'you' : 'agent · fix-login', 'scr-auth-disp-1') +
          row('Virtual display · Xvfb', 'agent · docs-sweep') +
          row('Your screen', 'you') +
        '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:10px">Taking the wheel parks the agent’s hands — it watches until you give them back.</div>'
    });
  },

  activityCard() {
    return P.card({
      title: 'Display activity',
      body: P.logLines([
        ['10:07', 'tool', 'agent clicked the login button'],
        ['10:04', '', 'screenshot taken — the error banner, read'],
        ['09:58', 'ok', 'stream started · 15 fps'],
        ['09:52', '', 'you took the wheel for two minutes'],
        ['09:41', '', 'display attached to fix-login']
      ])
    });
  },

  /* ---------------- your screen ---------------- */
  yourScreenCard(d) {
    const shared = this.shared;
    return '<div class="card" id="scr-your-screen">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">' + P.esc(d.name) + '</h3>' +
        '<div class="card-sub">Private by default — the house may look only when you say so, for as long as you say.</div>' +
      '</div>' +
      '<div class="card-actions">' +
        (shared ? P.chip('shared · ' + this.shareDur, 'attn', 'eye') : P.chip('private', 'slate', 'shield')) +
      '</div></div>' +
      '<div class="row" style="gap:8px;flex-wrap:wrap">' +
        '<button class="btn ' + (shared ? 'btn-danger' : 'btn-safe') + '" id="scr-share-screen">' +
          P.ICON(shared ? 'x' : 'eye', 15) + ' ' + (shared ? 'Stop sharing' : 'Share with the house…') + '</button>' +
        '<button class="btn btn-quiet" data-scr-tool="View privately">View privately</button>' +
        (shared ? '' :
          '<span class="seg" id="scr-share-dur">' +
            ['15 minutes', 'this session', 'until revoked'].map((t, i) =>
              '<button class="' + (i === 0 ? 'on' : '') + '" data-dur="' + t + '">' + t + '</button>').join('') +
          '</span>') +
      '</div>' +
      (shared
        ? '<div class="dim" style="margin-top:8px;font-size:12.5px">The house can look for ' + P.esc(this.shareDur) +
          ' — looking only, never touching. Stopping is always one tap away.</div>'
        : '') +
    '</div>';
  },

  /* ---------------- recordings & clips ---------------- */
  recordingCard(r) {
    return '<div class="card scr-player" style="--playdur:' + (r.id === 'rec-1' ? 12 : 7) + 's">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">' + P.esc(r.name) + '</h3>' +
        '<div class="card-sub">' + P.fact(r.len + ' · ' + r.when + ' · ' + r.size) + '</div>' +
      '</div></div>' +
      '<div class="scr-poster">' +
        '<button class="scr-play" data-play aria-label="Play">' + P.icon('play', 22) + '</button>' +
        '<div class="scr-timeline"><i></i></div>' +
      '</div>' +
      '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
        [['in', 'set in'], ['out', 'set out'], ['annotate', 'annotate'], ['save', 'save clip']].map(c =>
          '<button class="btn btn-quiet btn-xs" data-clip="' + c[0] + '">' + c[1] + '</button>').join('') +
      '</div></div>';
  },

  /* ---------------- terminals ---------------- */
  terminalCard() {
    return '<div class="card">' +
      '<div class="card-head"><div>' +
        '<h3 class="card-title">Terminal</h3>' +
        '<div class="card-sub">A shell is how you touch a machine — pin one here.</div>' +
      '</div>' +
      '<div class="card-actions">' + P.chip('Studio Mac', 'slate') +
        '<button class="btn btn-quiet btn-xs" id="scr-term-share">share…</button></div></div>' +
      '<div class="scr-xterm">' +
        '<div><span class="p">val@studio-mac</span> <span class="o">~/projects/shopify-theme</span> <span class="c">$ npm test</span></div>' +
        '<div class="o">✓ auth suite — 41 passing · 3.2s</div>' +
        '<div><span class="p">val@studio-mac</span> <span class="o">~/projects/shopify-theme</span> <span class="c">$ git status --short</span></div>' +
        '<div class="o"> M src/auth/session.ts · M src/middleware/redirect.ts · +10 more</div>' +
        '<div><span class="p">val@studio-mac</span> <span class="o">~/projects/shopify-theme</span> <span class="c">$ </span><span class="scr-caret"></span></div>' +
      '</div>' +
      '<div class="dim" style="margin-top:8px;font-size:12.5px">Sharing a terminal is a grant — ' +
        '<span class="mono">terminal.view</span> to watch, <span class="mono">terminal.write</span> to type. Yours to revoke, always.</div>' +
    '</div>';
  },

  /* ---------------- browser workspaces ---------------- */
  workspaceCard() {
    const providers = ['auto', 'cdp', 'system_cdp', 'playwright', 'agent_browser', 'stream'];
    return P.card({
      title: 'Browser workspaces',
      sub: 'Agent-driven browsers, leased by the session that drives them.',
      body:
        '<div class="row" style="gap:10px;flex-wrap:wrap">' +
          P.ICON('external', 15) +
          '<div style="flex:1;min-width:200px"><b>docs link-checker</b> <span class="fact">playwright · leased by docs-sweep</span></div>' +
          P.chip('leased', 'violet') +
          '<button class="btn btn-quiet btn-xs" data-ws="acquire">Acquire</button>' +
          '<button class="btn btn-quiet btn-xs" data-ws="release">Release</button>' +
        '</div>' +
        P.foldHtml({
          key: 'screens.workspace', title: 'New workspace', note: 'provider · lease',
          body:
            '<div class="row" style="gap:8px;flex-wrap:wrap;align-items:center">' +
              '<span class="dim" style="font-size:12.5px">Provider</span>' +
              '<span class="seg">' + providers.map((p, i) =>
                '<button class="' + (i === 0 ? 'on' : '') + '" data-provider="' + p + '">' + p + '</button>').join('') + '</span>' +
              '<button class="btn btn-safe btn-xs" id="scr-ws-create">Create workspace</button>' +
            '</div>' +
            '<div class="dim" style="margin-top:8px;font-size:12px">“auto” picks what the machine offers; the rest pin it. One workspace, one lease at a time.</div>'
        })
    });
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    const self = this;

    el.querySelectorAll('[data-scr-tool]').forEach(b => b.addEventListener('click', () =>
      P.toast(b.dataset.scrTool + ' — lives here in the real build', null)));

    const take = el.querySelector('#scr-take-control');
    if (take) take.addEventListener('click', () => {
      self.hands = self.hands === 'agent' ? 'you' : 'agent';
      const you = self.hands === 'you';
      take.className = 'btn ' + (you ? 'btn-danger' : 'btn-safe') + ' btn-xs';
      take.innerHTML = P.ICON('hand', 13) + ' <span>' + (you ? 'Release' : 'Take control') + '</span>';
      const line = document.getElementById('scr-hands-line');
      if (line) line.textContent = 'hands: ' + (you ? 'you' : 'agent (fix-login)');
      const rail = document.getElementById('scr-auth-disp-1');
      if (rail) rail.textContent = you ? 'you' : 'agent · fix-login';
      const cursor = document.getElementById('scr-cursor');
      if (cursor) cursor.style.display = you ? 'none' : '';
      P.toast(you ? 'You’re at the wheel — fix-login’s hands are parked' : 'Released — fix-login has the wheel again', you ? 'sage' : null);
    });

    this.wireShare(el);

    const newVd = el.querySelector('#scr-new-vdisplay');
    if (newVd) newVd.addEventListener('click', () =>
      P.toast('A fresh virtual display would spin up here — Xvfb, 1920×1080', 'sage'));

    el.querySelectorAll('[data-play]').forEach(b => b.addEventListener('click', () => {
      const player = b.closest('.scr-player');
      const playing = player.classList.toggle('playing');
      b.innerHTML = P.icon(playing ? 'pause' : 'play', 22);
    }));
    el.querySelectorAll('[data-clip]').forEach(b => b.addEventListener('click', () => {
      const msgs = {
        in: 'Clip starts here',
        out: 'Clip ends here — ready to cut',
        annotate: 'Pen, rect, and arrow live here in the real build',
        save: 'Clip saved — find it under Recordings'
      };
      P.toast(msgs[b.dataset.clip], b.dataset.clip === 'save' ? 'sage' : null);
    }));

    const termShare = el.querySelector('#scr-term-share');
    if (termShare) termShare.addEventListener('click', () =>
      P.toast('Terminal shared with the house — terminal.view, revocable any time', 'sage'));

    el.querySelectorAll('[data-ws]').forEach(b => b.addEventListener('click', () =>
      P.toast(b.dataset.ws === 'acquire'
        ? 'Workspace acquired — your session holds the lease'
        : 'Lease released — the workspace is free', 'sage')));
    el.querySelectorAll('[data-provider]').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
    }));
    const create = el.querySelector('#scr-ws-create');
    if (create) create.addEventListener('click', () => {
      const on = el.querySelector('[data-provider].on');
      P.toast('Workspace created — provider ' + (on ? on.dataset.provider : 'auto'), 'sage');
    });

    this.startCursor(el);
  },

  /* the share card re-renders itself, so it gets its own (re)wiring */
  wireShare(root) {
    const self = this;
    const shareBtn = root.querySelector('#scr-share-screen');
    if (shareBtn) shareBtn.addEventListener('click', () => {
      self.shared = !self.shared;
      if (self.shared) {
        const on = document.querySelector('#scr-share-dur button.on');
        self.shareDur = on ? on.dataset.dur : '15 minutes';
      }
      P.toast(self.shared
        ? 'Screen shared for ' + self.shareDur + ' — looking only, never touching'
        : 'Sharing stopped — your screen is yours again', self.shared ? 'sage' : null);
      const old = document.getElementById('scr-your-screen');
      if (old) {
        const tpl = document.createElement('template');
        tpl.innerHTML = self.yourScreenCard(P.data.displays.find(x => x.id === 'disp-0')).trim();
        old.replaceWith(tpl.content.firstElementChild);
        self.wireShare(document);
      }
    });
    root.querySelectorAll('#scr-share-dur button').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
    }));
  },

  /* the agent cursor: drifts, sometimes clicks, parks while you drive,
     and cleans itself up when the room is left */
  startCursor(el) {
    const screen = el.querySelector('#scr-screen-1');
    const cursor = el.querySelector('#scr-cursor');
    if (!screen || !cursor) return;
    const self = this;
    function drift() {
      if (!screen.isConnected) { clearInterval(moveT); clearInterval(clickT); return; }
      if (self.hands === 'you') return;
      cursor.style.left = (14 + Math.random() * 66) + '%';
      cursor.style.top = (30 + Math.random() * 52) + '%';
    }
    function click() {
      if (!screen.isConnected) { clearInterval(moveT); clearInterval(clickT); return; }
      if (self.hands === 'you' || Math.random() < 0.45) return;
      const r = document.createElement('span');
      r.className = 'scr-ripple';
      r.style.left = cursor.style.left;
      r.style.top = cursor.style.top;
      screen.appendChild(r);
      setTimeout(() => r.remove(), 720);
    }
    drift();
    const moveT = setInterval(drift, 2200);
    const clickT = setInterval(click, 3300);
  }
};
