/* Proscenium — Machines: the fleet. Cards first (this machine, then
   peers), a drill-down per machine, the pairing fold (⌘K lands on
   #fold-pairing), and delegation across the fence. */
window.P = window.P || {};
P.views = P.views || {};

P.views.machines = {
  title: 'Machines',
  cap: 'computer-use',

  render(el, params) {
    if (params && params[0]) { this.renderMachine(el, params[0]); return; }
    this.renderFleet(el);
  },

  /* ---------------- the fleet ---------------- */
  renderFleet(el) {
    const ms = P.data.machines.slice().sort((a, b) =>
      (b.thisMachine ? 1 : 0) - (a.thisMachine ? 1 : 0));
    const knocks = P.data.queue.filter(q => q.kind === 'enrollment' && !P.queueStore.isResolved(q.id));
    el.innerHTML = P.page({
      eyebrow: 'the fleet',
      title: 'Machines',
      sub: 'The house’s wings — your machines, the peers it can reach, and how new ones join.',
      body:
        (knocks.length
          ? '<div class="panel row" style="gap:10px">' + P.ICON('doorbell', 15) +
            '<span style="flex:1">' + P.esc(knocks[0].title) + ' — it’s waiting in the queue.</span>' +
            '<a class="chip chip-attn" href="#/home">answer it →</a></div>'
          : '') +
        '<div class="grid grid-3">' + ms.map(m => this.machineCard(m)).join('') + '</div>' +
        this.pairingFold() +
        this.delegateFold()
    });
    this.wireFleet(el);
  },

  machineCard(m) {
    const online = m.status === 'online';
    return '<div class="card">' +
      '<div class="row" style="align-items:baseline;gap:8px">' +
        '<span class="mach-petname">' + P.esc(m.petname) + '</span>' +
        '<span class="dim mono" style="font-size:11.5px;white-space:nowrap">' + P.esc(m.label) + '</span>' +
        '<span class="grow"></span>' +
        (m.thisMachine ? P.chip('this machine', 'brass') : '') +
      '</div>' +
      '<div class="row" style="gap:8px;margin-top:6px;flex-wrap:wrap">' +
        P.dot(online ? 'sage' : 'slate', online) +
        '<span style="font-size:13px">' + (online ? 'online' : 'away') + '</span>' +
        P.routeChip(m.route) +
        P.chip(m.role, m.role === 'owner' ? 'brass' : 'slate') +
      '</div>' +
      '<div class="row" style="gap:6px;margin-top:10px;flex-wrap:wrap">' +
        m.capabilities.map(c => P.chip(c, null)).join('') +
      '</div>' +
      '<div class="mach-pressure" style="margin-top:12px">' +
        '<span class="k">cpu</span>' + P.meter(m.pressure.cpu) +
        '<span class="p">' + (online ? m.pressure.cpu + '%' : '—') + '</span>' +
        '<span class="k">mem</span>' + P.meter(m.pressure.mem) +
        '<span class="p">' + (online ? m.pressure.mem + '%' : '—') + '</span>' +
      '</div>' +
      '<div class="row" style="gap:8px;margin-top:12px;flex-wrap:wrap">' +
        (m.fueled ? P.chip('fueled', 'sage', 'fuel') : P.chip('dry', 'attn', 'fuel')) +
        (m.lease ? P.fact(m.lease) : '') +
        (m.note ? P.fact(m.note) : '') +
      '</div>' +
      '<div class="factline" style="margin-top:10px">' + P.fact(m.os) + P.fact(m.version) + '</div>' +
      '<div class="row" style="margin-top:12px">' +
        '<a class="btn btn-quiet btn-xs" href="#/machines/' + m.id + '">Open ' + P.ICON('chev', 13) + '</a>' +
      '</div>' +
    '</div>';
  },

  /* ---------------- link a machine (⌘K lands here) ---------------- */
  pairingFold() {
    const mode = (title, expl, cons, extra) =>
      '<div class="panel">' +
        '<div style="margin-bottom:6px"><b>' + title + '</b></div>' +
        '<div style="font-size:13px;color:var(--text-2)">' + expl + '</div>' +
        (extra || '') +
        '<div class="dim" style="font-size:12px;margin-top:8px;font-style:italic">' + cons + '</div>' +
      '</div>';
    return P.foldHtml({
      key: 'machines.pairing', id: 'fold-pairing', title: 'Link a machine', note: 'four ways in',
      body:
        '<div class="grid grid-2">' +
          mode('Request access',
            'Knock on another house’s door. You introduce yourself; its owner decides what you may do.',
            'This creates a route, not authority — its IAM still decides.',
            '<div style="margin-top:10px"><button class="btn btn-safe btn-xs" data-pair="request">Send a request</button></div>') +
          mode('Join invite',
            'Someone handed you twelve words. They name a door — once — and then they are spent.',
            'A claim code is a name tag, not a key — the other house still decides your role.',
            '<div class="row" style="margin-top:10px;gap:8px">' +
              '<input class="input mono" id="mach-claim" placeholder="broom candle harbor … twelve words" style="flex:1;min-width:180px">' +
              '<button class="btn btn-safe btn-xs" data-pair="join">Join</button></div>') +
          mode('Grant invite',
            'Mint twelve words for a machine you want to let in. Single use, and they wilt in a day.',
            'You stay the authority — revoke the moment you like.',
            '<div style="margin-top:10px"><button class="btn btn-safe btn-xs" data-pair="grant">Mint a code</button></div>') +
          mode('Manual add',
            'Paste an address and a key by hand — for networks that don’t do ceremonies.',
            'This creates a route, not authority — your IAM still decides.',
            '<div style="margin-top:10px"><button class="btn btn-quiet btn-xs" data-pair="manual">Add manually</button></div>') +
        '</div>'
    });
  },

  /* ---------------- delegate ---------------- */
  delegateFold() {
    return P.foldHtml({
      key: 'machines.delegate', title: 'Delegate', note: 'send work across the fleet',
      body:
        '<div class="dim" style="font-size:12.5px;margin-bottom:10px">Pick what the work needs — I’ll show you who can take it.</div>' +
        '<div class="row" style="gap:6px;flex-wrap:wrap">' +
          ['computer-use', 'terminal', 'headless'].map(c =>
            '<button class="chip' + (c === this.cap ? ' chip-sage' : '') + '" data-cap="' + c + '">' + c + '</button>').join('') +
        '</div>' +
        '<div id="mach-eligible" style="margin-top:10px">' + this.eligibleHtml() + '</div>'
    });
  },

  eligibleHtml() {
    const cap = this.cap;
    const peers = P.data.machines.filter(m =>
      !m.thisMachine && m.capabilities.indexOf(cap) > -1 && m.status === 'online' && m.fueled);
    if (!peers.length) {
      return P.empty('machines', 'Nobody can take that right now',
        'A peer needs the capability, fuel, and a pulse — Parlor PC is away and dry.');
    }
    return peers.map(m =>
      '<div class="panel">' +
        '<div class="row" style="gap:8px;flex-wrap:wrap">' +
          P.dot('sage') + '<b>' + P.esc(m.petname) + '</b>' +
          P.routeChip(m.route) + P.chip(m.role, 'slate') +
          '<span class="fact">' + P.esc(m.label) + '</span>' +
        '</div>' +
        '<textarea class="input" rows="2" style="width:100%;margin-top:8px" placeholder="What should ' +
          P.esc(m.petname) + ' do? One sentence is plenty…"></textarea>' +
        '<div class="row" style="margin-top:8px;gap:8px;flex-wrap:wrap">' +
          '<button class="btn btn-primary btn-xs" data-route-task="' + m.id + '">' + P.ICON('send', 13) + ' Route the task</button>' +
          '<span class="dim" style="font-size:12px">Its IAM re-decides everything there — your route is a request, not a skeleton key.</span>' +
        '</div>' +
      '</div>').join('');
  },

  wireFleet(el) {
    const self = this;
    el.querySelectorAll('[data-pair]').forEach(b => b.addEventListener('click', () => {
      const msgs = {
        request: 'Request sent — the other house’s owner will see it knock',
        join: 'The twelve words would be checked and spent here — welcome in',
        grant: 'A fresh claim code would appear here — twelve words, single use',
        manual: 'The address-and-key form lives here in the real build'
      };
      P.toast(msgs[b.dataset.pair], b.dataset.pair === 'manual' ? null : 'sage');
    }));
    el.querySelectorAll('[data-cap]').forEach(b => b.addEventListener('click', () => {
      self.cap = b.dataset.cap;
      el.querySelectorAll('[data-cap]').forEach(x => x.classList.toggle('chip-sage', x === b));
      document.getElementById('mach-eligible').innerHTML = self.eligibleHtml();
      self.wireEligible(el);
    }));
    this.wireEligible(el);
  },

  wireEligible(el) {
    el.querySelectorAll('[data-route-task]').forEach(b => b.addEventListener('click', () =>
      P.toast('Task routed to ' + P.machineName(b.dataset.routeTask) + ' — receipt tracked', 'sage')));
  },

  /* ---------------- the drill-down ---------------- */
  renderMachine(el, id) {
    const m = P.data.machines.find(x => x.id === id) || P.data.machines[0];
    const peer = !m.thisMachine;
    const online = m.status === 'online';
    const rank = { working: 0, idle: 1, done: 2, archived: 3 };
    const sessions = P.data.sessions.filter(s => s.machine === m.id)
      .sort((a, b) => (rank[a.phase] || 9) - (rank[b.phase] || 9));
    const displays = P.data.displays.filter(d => d.machine === m.id);
    el.innerHTML = P.page({
      body:
        '<div class="row" style="align-items:flex-start;gap:14px;flex-wrap:wrap">' +
          '<div style="min-width:280px;flex:1">' +
            '<div class="eyebrow"><a href="#/machines" style="color:inherit">machines</a> / ' + P.esc(m.petname) + '</div>' +
            '<h1 class="page-title" style="font-size:26px">' + P.esc(m.petname) + '</h1>' +
            '<div class="page-sub">' +
              (m.thisMachine ? 'This machine — where the house lives.'
                : P.esc(m.note || 'A peer, reached ' + m.route + '.')) + '</div>' +
            '<div class="row" style="gap:6px;flex-wrap:wrap">' +
              P.dot(online ? 'sage' : 'slate', online) +
              '<span style="font-size:13px">' + P.esc(m.status) + '</span>' +
              P.routeChip(m.route) +
              P.chip(m.role, 'slate') +
              (m.fueled ? P.chip('fueled', 'sage', 'fuel') : P.chip('dry', 'attn', 'fuel')) +
              (m.thisMachine ? P.chip('this machine', 'brass') : '') +
            '</div>' +
          '</div>' +
          '<div class="col" style="gap:8px;align-items:flex-end">' +
            P.authline(m.thisMachine ? 'you' : m.petname, m.role, m.route) +
          '</div>' +
        '</div>' +
        (peer
          ? '<div class="panel dim" style="font-size:12.5px">You’re browsing ' + P.esc(m.petname) +
            ' from across the fence — its sessions and screens are shown display-only, as its IAM allows. Work on it happens from its own dashboard.</div>'
          : '') +
        /* stats mini-panel */
        '<div class="card"><div class="factline" style="gap:22px">' +
          '<span>' + P.fact(m.os) + '</span>' +
          '<span>' + P.fact(m.version) + '</span>' +
          '<span class="row" style="gap:6px">' + P.fact('cpu ' + m.pressure.cpu + '%') + P.meter(m.pressure.cpu) + '</span>' +
          '<span class="row" style="gap:6px">' + P.fact('mem ' + m.pressure.mem + '%') + P.meter(m.pressure.mem) + '</span>' +
          (m.lease ? '<span>' + P.fact(m.lease) + '</span>' : '') +
          '<span class="studio-only">' + P.fact(m.capabilities.join(' · ')) + '</span>' +
        '</div></div>' +
        P.section('Its sessions') +
        (sessions.length
          ? '<div class="grid grid-3">' + sessions.map(P.stageCard).join('') + '</div>'
          : P.empty('stage', 'Nothing on stage',
              m.thisMachine ? 'Give the house something to do — one sentence in the composer is enough.'
                : 'This machine is idle.')) +
        P.section('Its screens') +
        (displays.length
          ? '<div class="card"><div class="col" style="gap:10px">' +
            displays.map(d =>
              '<div class="row" style="gap:10px;flex-wrap:wrap">' +
                P.ICON('screens', 15) +
                '<span style="flex:1;min-width:180px">' + P.esc(d.name) + '</span>' +
                P.fact(d.res) +
                (d.live ? P.chip('live', 'sage') : P.chip('private', 'slate')) +
                (d.peer ? P.routeChip('fleet name') : '') +
                '<a class="chip" href="#/screens">open in screens →</a>' +
              '</div>').join('') +
            '</div></div>'
          : P.empty('screens', 'No screens here',
              'Screens appear when the house launches something graphical, or its owner shares one.')) +
        (peer
          ? '<div class="dim" style="font-size:12.5px">Browsing its files and sessions stays display-only — the trust model gives you a window, not a key.</div>'
          : '')
    });
  }
};
