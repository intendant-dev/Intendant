/* V3 — Machines: the fleet. Cards first (this machine, then peers),
   a drill-down per machine, the pairing fold (⌘K lands on
   #fold-pairing), and delegation across the fence. Everything renders
   from V3.data.machines (this daemon first, then /api/peers); what the
   daemon doesn't publish shows as an honest dash, never a made-up number. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.machines = {
  title: 'Machines',
  cap: 'computer-use',

  render(el, params) {
    if (params && params[0]) { this.renderMachine(el, params[0]); return; }
    this.renderFleet(el);
  },

  /* ---------------- the fleet ---------------- */
  renderFleet(el) {
    const ms = V3.data.machines;
    const knocks = V3.data.queue.filter(q =>
      (q.kind === 'enrollment' || q.kind === 'peer-pair') && !V3.queueStore.isResolved(q.id));
    el.innerHTML = V3.page({
      eyebrow: 'the fleet',
      title: 'Machines',
      sub: 'The house’s wings — your machines, the peers it can reach, and how new ones join.',
      body:
        (knocks.length
          ? '<div class="panel row" style="gap:10px">' + V3.ICON('doorbell', 15) +
            '<span style="flex:1">' + V3.esc(knocks[0].title) + ' — it’s waiting in the queue.</span>' +
            '<a class="chip chip-attn" href="#/home">answer it →</a></div>'
          : '') +
        (ms.length
          ? '<div class="grid grid-3">' + ms.map(m => this.machineCard(m)).join('') + '</div>'
          : V3.empty('machines', 'No machines yet', 'This daemon hasn’t checked in, and no peers are linked.')) +
        this.pairingFold() +
        this.delegateFold()
    });
    this.wireFleet(el);
  },

  /* local fuel comes from /api/api-key-status; peers don't publish
     theirs to this daemon — an honest dash, not a guess */
  fueledChip(m) {
    if (m.id === 'local') {
      const f = V3.data.fuel || {};
      return (f.openai || f.anthropic || f.gemini)
        ? V3.chip('fueled', 'sage', 'fuel')
        : V3.chip('dry — no API keys', 'attn', 'fuel');
    }
    return V3.fact('fuel: —');
  },

  machineCard(m) {
    const online = m.status === 'online';
    const local = m.id === 'local';
    return '<div class="card">' +
      '<div class="row" style="align-items:baseline;gap:8px">' +
        '<span class="mach-petname">' + V3.esc(m.petname) + '</span>' +
        '<span class="dim mono" style="font-size:11.5px;white-space:nowrap">' + V3.esc(m.label) + '</span>' +
        '<span class="grow"></span>' +
        (local ? V3.chip('this machine', 'brass') : '') +
      '</div>' +
      '<div class="row" style="gap:8px;margin-top:6px;flex-wrap:wrap">' +
        V3.dot(online ? 'sage' : 'slate', online) +
        '<span style="font-size:13px">' + (online ? 'online' : 'away') + '</span>' +
        V3.routeChip(m.route) +
        V3.chip(m.role || 'peer', m.role === 'owner' ? 'brass' : 'slate') +
      '</div>' +
      ((m.capabilities || []).length
        ? '<div class="row" style="gap:6px;margin-top:10px;flex-wrap:wrap">' +
          m.capabilities.map(c => V3.chip(c, null)).join('') + '</div>'
        : '') +
      '<div class="row" style="gap:8px;margin-top:12px;flex-wrap:wrap">' + this.fueledChip(m) + '</div>' +
      '<div class="factline" style="margin-top:10px">' + V3.fact(m.version ? 'v' + m.version : 'version —') + '</div>' +
      '<div class="row" style="margin-top:12px">' +
        '<a class="btn btn-quiet btn-xs" href="#/machines/' + encodeURIComponent(m.id) + '">Open ' + V3.ICON('chev', 13) + '</a>' +
      '</div>' +
    '</div>';
  },

  /* ---------------- link a machine (⌘K lands here) ---------------- */
  pairingFold() {
    const mode = (title, expl, cons) =>
      '<div class="panel">' +
        '<div style="margin-bottom:6px"><b>' + V3.esc(title) + '</b></div>' +
        '<div style="font-size:13px;color:var(--text-2)">' + V3.esc(expl) + '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:8px;font-style:italic">' + V3.esc(cons) + '</div>' +
      '</div>';
    return V3.foldHtml({
      key: 'machines.pairing', id: 'fold-pairing', title: 'Link a machine', note: 'four ways in',
      body:
        '<div class="grid grid-2">' +
          mode('Request access',
            'Knock on another house’s door. You introduce yourself; its owner decides what you may do.',
            'Pairing creates a route, not authority — its IAM still decides.') +
          mode('Join invite',
            'Someone handed you twelve words. They name a door — once — and then they are spent.',
            'A claim code is a name tag, not a key — the other house still decides your role.') +
          mode('Grant invite',
            'Mint twelve words for a machine you want to let in. Single use, and they wilt in a day.',
            'You stay the authority — revoke the moment you like.') +
          mode('Manual add',
            'Paste an address and a key by hand — for networks that don’t do ceremonies.',
            'Pairing creates a route, not authority — your IAM still decides.') +
        '</div>' +
        '<div class="dim" style="font-size:12.5px;margin-top:10px">The full pairing wizard lives in the classic dashboard for this draft — <a href="/">open it there →</a></div>'
    });
  },

  /* ---------------- delegate ---------------- */
  delegateFold() {
    const peers = V3.data.machines.filter(m => m.id !== 'local');
    const caps = [];
    peers.forEach(m => (m.capabilities || []).forEach(c => { if (caps.indexOf(c) === -1) caps.push(c); }));
    if (!caps.length) caps.push('computer-use', 'terminal', 'headless');
    if (caps.indexOf(this.cap) === -1) this.cap = caps[0];
    return V3.foldHtml({
      key: 'machines.delegate', title: 'Delegate', note: 'send work across the fleet',
      body:
        '<div class="dim" style="font-size:12.5px;margin-bottom:10px">Pick what the work needs — I’ll show you who can take it.</div>' +
        '<div class="row" style="gap:6px;flex-wrap:wrap">' +
          caps.map(c =>
            '<button class="chip' + (c === this.cap ? ' chip-sage' : '') + '" data-cap="' + V3.esc(c) + '">' + V3.esc(c) + '</button>').join('') +
        '</div>' +
        '<div id="mach-eligible" style="margin-top:10px">' + this.eligibleHtml() + '</div>'
    });
  },

  eligibleHtml() {
    const cap = this.cap;
    const peers = V3.data.machines.filter(m => m.id !== 'local');
    if (!peers.length) {
      return V3.empty('machines', 'No peers linked yet',
        'Pair a machine above and it can take work from here — its own IAM re-decides everything there.');
    }
    const eligible = peers.filter(m => m.status === 'online' && (m.capabilities || []).indexOf(cap) > -1);
    if (!eligible.length) {
      return V3.empty('machines', 'Nobody can take that right now',
        'A peer needs the capability and a pulse — nothing online advertises “' + cap + '”.');
    }
    return eligible.map(m =>
      '<div class="panel">' +
        '<div class="row" style="gap:8px;flex-wrap:wrap">' +
          V3.dot('sage') + '<b>' + V3.esc(m.petname) + '</b>' +
          V3.routeChip(m.route) + V3.chip(m.role || 'peer', 'slate') +
          '<span class="fact">' + V3.esc(m.label) + '</span>' +
        '</div>' +
        '<textarea class="input" rows="2" style="width:100%;margin-top:8px" placeholder="What should ' +
          V3.esc(m.petname) + ' do? One sentence is plenty…"></textarea>' +
        '<div class="row" style="margin-top:8px;gap:8px;flex-wrap:wrap">' +
          '<button class="btn btn-primary btn-xs" data-route-task="' + V3.esc(m.id) + '">' + V3.ICON('send', 13) + ' Route the task</button>' +
          '<span class="dim" style="font-size:12px">Its IAM re-decides everything there — your route is a request, not a skeleton key.</span>' +
        '</div>' +
      '</div>').join('');
  },

  wireFleet(el) {
    const self = this;
    el.querySelectorAll('[data-cap]').forEach(b => b.addEventListener('click', () => {
      self.cap = b.dataset.cap;
      el.querySelectorAll('[data-cap]').forEach(x => x.classList.toggle('chip-sage', x === b));
      const host = document.getElementById('mach-eligible');
      if (host) { host.innerHTML = self.eligibleHtml(); self.wireEligible(host); }
    }));
    this.wireEligible(el);
  },

  wireEligible(root) {
    root.querySelectorAll('[data-route-task]').forEach(b => b.addEventListener('click', () => {
      const id = b.dataset.routeTask;
      const ta = b.closest('.panel').querySelector('textarea');
      const text = (ta && ta.value || '').trim();
      if (!text) { V3.toast('Give the task a sentence first', 'brick'); return; }
      b.disabled = true;
      /* the daemon's DelegateTaskRequest keys on `instructions` */
      V3.transport.post('/api/peers/' + encodeURIComponent(id) + '/task', {
        instructions: text,
        client_correlation_id: 'v3-' + Date.now().toString(36)
      }).then(r => {
        if (ta) ta.value = '';
        V3.toast('Routed to ' + V3.machineName(id) +
          (r && r.delivery === 'acknowledged' ? ' — acknowledged' : ' — sent, unconfirmed'), 'sage');
      }).catch(V3.actions._err).finally(() => { b.disabled = false; });
    }));
  },

  /* ---------------- the drill-down ---------------- */
  renderMachine(el, rawId) {
    let id = rawId;
    try { id = decodeURIComponent(rawId); } catch (e) {}
    const m = V3.data.machines.find(x => String(x.id) === String(id));
    if (!m) {
      el.innerHTML = V3.page({
        body:
          '<div class="eyebrow"><a href="#/machines" style="color:inherit">machines</a></div>' +
          V3.empty('machines', 'No such machine', 'It may have unpaired — the fleet list is one tap back.')
      });
      return;
    }
    const local = m.id === 'local';
    const online = m.status === 'online';
    const sessions = local ? V3.data.sessions.filter(s => (s.machine || 'local') === 'local') : [];
    el.innerHTML = V3.page({
      body:
        '<div class="row" style="align-items:flex-start;gap:14px;flex-wrap:wrap">' +
          '<div style="min-width:280px;flex:1">' +
            '<div class="eyebrow"><a href="#/machines" style="color:inherit">machines</a> / ' + V3.esc(m.petname) + '</div>' +
            '<h1 class="page-title" style="font-size:26px">' + V3.esc(m.petname) + '</h1>' +
            '<div class="page-sub">' +
              (local ? 'This machine — where the house lives.'
                     : 'A peer, reached ' + V3.esc(m.route) + '.') + '</div>' +
            '<div class="row" style="gap:6px;flex-wrap:wrap">' +
              V3.dot(online ? 'sage' : 'slate', online) +
              '<span style="font-size:13px">' + V3.esc(m.status) + '</span>' +
              V3.routeChip(m.route) +
              V3.chip(m.role || 'peer', m.role === 'owner' ? 'brass' : 'slate') +
              this.fueledChip(m) +
              (local ? V3.chip('this machine', 'brass') : '') +
            '</div>' +
          '</div>' +
          '<div class="col" style="gap:8px;align-items:flex-end">' +
            (local ? V3.authline(V3.data.you.name, V3.data.you.role, V3.data.you.route)
                   : V3.authline('via ' + m.petname, m.role || 'peer', m.route)) +
          '</div>' +
        '</div>' +
        (!local
          ? '<div class="panel dim" style="font-size:12.5px">You’re browsing ' + V3.esc(m.petname) +
            ' from across the fence — its sessions and screens show here display-only, as its IAM allows.</div>'
          : '') +
        '<div class="card"><div class="factline" style="gap:22px">' +
          '<span>' + V3.fact(m.version ? 'v' + m.version : 'version —') + '</span>' +
          '<span>' + V3.fact((m.capabilities || []).join(' · ') || 'no capabilities advertised') + '</span>' +
          '<span class="studio-only">' + V3.fact('via ' + m.route) + '</span>' +
        '</div></div>' +
        V3.section('Its sessions') +
        (local
          ? (sessions.length
            ? '<div class="grid grid-3">' + sessions.map(V3.stageCard).join('') + '</div>'
            : V3.empty('stage', 'Nothing on stage', 'Give the house something to do — one sentence in the composer is enough.'))
          : '<div class="card"><div class="row" style="gap:10px;flex-wrap:wrap">' +
            V3.ICON('stage', 15) +
            '<span style="flex:1;min-width:220px;font-size:13px">A peer’s sessions browse in the classic dashboard in this draft — nothing is hidden, just not rebuilt here yet.</span>' +
            '<a class="btn btn-quiet btn-xs" href="/">open the classic dashboard →</a></div></div>') +
        V3.section('Its screens') +
        this.machineDisplaysHtml(m, local) +
        (!local
          ? '<div class="dim" style="font-size:12.5px">Work on ' + V3.esc(m.petname) +
            ' happens from its own dashboard — the trust model gives you a window, not a key.</div>'
          : '')
    });
  },

  machineDisplaysHtml(m, local) {
    if (local) {
      const displays = V3.data.displays;
      if (!displays.length) {
        return V3.empty('screens', 'No screens here',
          'Screens appear when the house launches something graphical, or its owner shares one.');
      }
      return '<div class="card"><div class="col" style="gap:10px">' +
        displays.map(d =>
          '<div class="row" style="gap:10px;flex-wrap:wrap">' +
            V3.ICON('screens', 15) +
            '<span style="flex:1;min-width:180px">Display ' + V3.esc(d.id) + '</span>' +
            V3.fact(d.w + '×' + d.h) +
            (d.live ? V3.chip('live', 'sage') : V3.chip('off', 'slate')) +
            '<a class="chip" href="#/screens">open in screens →</a>' +
          '</div>').join('') +
        '</div></div>';
    }
    const reported = (m.displays || []).length;
    if (!reported) {
      return V3.empty('screens', 'No screens here', 'Nothing ' + m.petname + ' has shared right now.');
    }
    return '<div class="card"><div class="dim" style="font-size:12.5px">' +
      V3.esc(m.petname) + ' reports ' + reported + ' display' + (reported === 1 ? '' : 's') +
      ' — watching them lives in the classic dashboard in this draft. <a href="/">open it →</a></div></div>';
  },

  live(what) {
    if (!['machines', 'sessions', 'queue', 'ready', 'displays'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    V3.route();
    main.scrollTop = y;
  }
};
