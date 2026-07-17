/* V3 — People & Keys: who may do what.
   You → people & devices → doors → keys & vault (⌘K lands on #fold-vault).
   Everything renders from V3.data (the /api/access/* polls); the only fetch
   the room makes itself is the hosted-control snapshot, cached per visit.
   Trust doctrine stays literal: petnames first, routes labeled, no door
   mints authority, and the hosted ceiling stands without an owner ceremony. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.people = {
  title: 'People & Keys',

  /* the four keys the house hands out, in plain words */
  ROLES: [
    ['owner', 'everything, including who else gets keys — hand this one out like a spare house key'],
    ['operator', 'runs work and touches screens; can’t hand out keys'],
    ['spectator', 'watches everything, touches nothing'],
    ['session-reader', 'reads finished work only']
  ],

  _hc: undefined,   /* hosted-control snapshot: undefined = not fetched, null = gated/absent */

  render(el) {
    const D = V3.data;
    const you = D.you || { name: 'you', role: 'owner', route: 'direct' };
    const ov = D.accessOverview || {};
    const fuel = D.fuel || {};
    const agents = D.externalAgents || [];

    el.innerHTML = V3.page({
      eyebrow: 'who may do what',
      title: 'People & Keys',
      sub: 'Every key the house honors, what it may do, and where the fuel lives. Petnames first — the route is always labeled, and no door mints authority.',
      body:
        /* ---------- You ---------- */
        '<div class="card">' +
          '<div class="row" style="gap:10px;align-items:baseline;flex-wrap:wrap">' +
            '<span class="voice" style="font-size:23px">' + V3.esc(you.name) + '</span>' +
            V3.chip(you.role, 'brass', 'shield') + V3.routeChip(you.route) +
          '</div>' +
          '<p style="margin:10px 0 0;font-size:13.5px;max-width:64ch">This page is served by the daemon itself, and it treats whoever reached it as <b>' +
            V3.esc(you.role) + '</b> — on loopback, that’s the owner. Your identity here is the serving context, not a login: the day you open this page through a key of your own, the house will name it here.</p>' +
        '</div>' +

        /* ---------- People & devices ---------- */
        V3.section('People & devices') +
        (D.people.length
          ? '<div class="card" style="padding:6px"><table class="table"><thead><tr>' +
            '<th>Who</th><th>Key</th><th>Role</th><th>Route</th><th>Lifecycle</th><th>Since</th></tr></thead><tbody>' +
            D.people.map(p =>
              '<tr>' +
              '<td>' + V3.esc(p.who) + '</td>' +
              '<td class="mono">' + V3.esc(p.key || '—') + '</td>' +
              '<td>' + V3.chip(p.role, p.role === 'owner' ? 'brass' : 'slate') + '</td>' +
              '<td>' + (!p.route || p.route === '—' ? '<span class="dim">—</span>' : V3.routeChip(p.route)) + '</td>' +
              '<td>' + V3.chip(p.lifecycle, p.lifecycle === 'active' ? 'sage' : 'brick') + '</td>' +
              '<td class="mono">' + V3.esc(p.since || '—') + '</td>' +
              '</tr>').join('') +
            '</tbody></table></div>' +
            '<div class="factline" style="margin-top:-4px">' +
              V3.fact((ov.principals || []).length + ' principals · ' + (ov.grants || []).length + ' grants · ' + (ov.permissions || []).length + ' permissions in the IAM model') +
              (ov.scope ? V3.fact('scope: ' + (ov.scope.label || ov.scope.kind || 'daemon')) : '') +
            '</div>'
          : V3.empty('key', 'No other keys yet', 'This daemon honors only the identity that served this page. When you enroll another device or hand out a key, it appears here.')) +

        V3.foldHtml({ key: 'people.grant', title: 'Hand out a key', note: 'a promise you can revoke any time',
          body:
          '<div class="dim" style="font-size:13px;margin:4px 0 10px">What a new key may do — plain words up front, the compiled role rides along underneath.</div>' +
          '<div class="ppl-roles">' +
            this.ROLES.map(r =>
              '<div class="panel ppl-role"><b>' + V3.esc(r[0]) + '</b><div class="dim">' + V3.esc(r[1]) + '</div></div>').join('') +
          '</div>' +
          '<div class="dim" style="font-size:12.5px;margin-top:12px">A device that wants in asks first — enrollment requests arrive in <a href="#/home">the Queue</a>, and you decide there. The deep grant forms (custom roles, org grants, ORLs) stay on <a href="/">the classic dashboard</a> for this draft.</div>' }) +

        /* ---------- Doors ---------- */
        V3.section('Doors — the ways in') +
        '<div class="grid grid-2">' +
          '<div id="ppl-hc">' + this.hostedHtml() + '</div>' +
          V3.card({
            title: 'Connect & claim codes', sub: 'the hosted rendezvous',
            body:
              '<div class="row">' + V3.chip('a name tag, not a key', 'slate', 'info') + '</div>' +
              '<p style="margin:10px 0 8px;font-size:13.5px">Connect introduces: it knows the fleet’s names and where to knock. A twelve-word claim code lets a new browser find this daemon and say its name — <b>claiming grants no authority</b>; the newcomer still asks you for a key.</p>' +
              '<div class="dim" style="font-size:12.5px">The claim ceremony — minting and redeeming codes — lives in <a href="/">the classic dashboard</a> in this draft.</div>'
          }) +
        '</div>' +

        /* ---------- Keys & vault (⌘K target) ---------- */
        V3.foldHtml({ key: 'people.vault', id: 'fold-vault', title: 'Keys & vault', note: 'fuel · sign-ins · custody',
          body:
          '<div class="eyebrow" style="margin:4px 0 8px">Fuel — the API keys the house runs on</div>' +
          '<div class="row" style="flex-wrap:wrap">' +
            [['Anthropic', 'anthropic'], ['OpenAI', 'openai'], ['Gemini', 'gemini']].map(([label, k]) =>
              V3.chip(label + ': ' + (fuel[k] ? 'set' : 'not set'), fuel[k] ? 'sage' : 'attn', 'fuel')).join('') +
            '<span class="grow"></span>' +
            '<button class="btn btn-primary btn-xs" id="ppl-add-keys">' + V3.ICON('key', 13) + ' Add API keys</button>' +
          '</div>' +
          '<div class="dim" style="font-size:12px;margin-top:6px">Presence checked, never shown — the daemon holds them in its .env and the page never reads them back.</div>' +

          '<div class="eyebrow" style="margin:18px 0 8px">Agent sign-ins — who’s logged in locally</div>' +
          (agents.length
            ? '<div class="col" style="gap:6px">' + agents.map(a =>
                '<div class="row" style="flex-wrap:wrap">' +
                  '<b style="min-width:130px">' + V3.esc(a.label || a.id) + '</b>' +
                  (a.installed === false ? V3.chip('not installed', 'slate')
                    : V3.chip('installed', 'sage')) +
                  (a.local_login ? V3.chip('signed in locally', 'sage') : V3.chip('not signed in', 'slate')) +
                  (a.leased ? V3.chip('leased', 'violet') : '') +
                '</div>').join('') + '</div>'
            : '<div class="dim" style="font-size:12.5px">No external agents reported — install Codex or Claude Code and the house can supervise them.</div>') +
          '<div class="dim" style="font-size:12px;margin-top:6px">Sign-in ceremonies (device auth, tokens) run on <a href="/">the classic dashboard</a>, where you watch exactly where a token lands.</div>' +

          '<div class="eyebrow" style="margin:18px 0 8px">Custody trail — where secrets have been</div>' +
          '<div class="dim" style="font-size:12.5px;max-width:64ch">The trail — vault leases, egress registrations, token landings — rides the dashboard-control tunnel in this build, so it renders on <a href="/">the classic dashboard</a>. Nothing credential-shaped is ever shown here; presence and state are all a page may say.</div>'
        })
    });

    this.wire(el);
    if (this._hc === undefined) this.fetchHosted();
  },

  /* ---------- hosted control (the one honest door the room fetches itself) ---------- */
  hostedHtml() {
    const hc = this._hc;
    if (hc === undefined) {
      return V3.card({ title: 'Hosted control', sub: 'reaching this house from anywhere',
        body: '<div class="col" style="gap:8px">' + V3.skeleton(14, '60%') + V3.skeleton(14, '90%') + '</div>' });
    }
    if (hc === null || hc.configured === false) {
      return V3.card({ title: 'Hosted control', sub: 'reaching this house from anywhere',
        body:
          '<div class="row">' + V3.chip('not configured', 'slate') + '</div>' +
          '<p style="margin:10px 0 0;font-size:13.5px">This daemon isn’t enrolled with a hosted rendezvous, so nobody can borrow it from a browser. The compiled floor would still apply if it were: hosted sessions can never exceed the preset ceiling without an owner ceremony on this machine.</p>' });
    }
    const pol = hc.policy || {};
    const leases = (hc.active_leases || []).length;
    const pending = (hc.pending_requests || []).length;
    const ttl = this.fmtTtl(pol.max_ttl_secs);
    return V3.card({ title: 'Hosted control', sub: 'reaching this house from anywhere',
      actions: '<button class="icon-btn" id="ppl-hc-refresh" title="Refresh">' + V3.ICON('refresh', 14) + '</button>',
      body:
        '<div class="row" style="flex-wrap:wrap">' +
          V3.chip(hc.enabled === false ? 'paused' : 'enabled', hc.enabled === false ? 'slate' : 'sage') +
          V3.chip('ceiling: ' + (pol.ceiling || '—'), 'brass', 'shield') +
          V3.fact('leases live: ' + leases) +
          (pending ? V3.chip(pending + ' asking in the Queue', 'attn', 'doorbell') : '') +
        '</div>' +
        '<div class="kv" style="margin-top:10px">' +
          '<span class="k">max lease</span><span class="v">' + V3.esc(ttl) + '</span>' +
          '<span class="k">floor</span><span class="v">the ceiling stands without an owner ceremony</span>' +
        '</div>' +
        '<div class="dim" style="margin-top:10px;font-size:12.5px">Hosted sessions are time-boxed leases you can revoke, and they can never exceed the preset ceiling — a hosted page mints nothing on its own.</div>' });
  },

  fmtTtl(secs) {
    const s = Number(secs) || 0;
    if (!s) return '—';
    if (s % 86400 === 0) return (s / 86400) + ' day' + (s / 86400 > 1 ? 's' : '');
    if (s % 3600 === 0) return (s / 3600) + ' h';
    if (s % 60 === 0) return (s / 60) + ' min';
    return s + ' s';
  },

  fetchHosted() {
    V3.transport.get('/api/access/hosted-control').then(r => {
      this._hc = r || null;
    }).catch(() => {
      this._hc = null;   /* 404 = not configured, 403 = gated — both render the same honest card */
    }).then(() => {
      const node = document.getElementById('ppl-hc');
      if (node && V3.current === 'people') { node.innerHTML = this.hostedHtml(); this.wireHosted(node); }
    });
  },

  wireHosted(node) {
    const btn = (node || document).querySelector('#ppl-hc-refresh');
    if (btn) btn.addEventListener('click', () => { this._hc = undefined; this.fetchHosted(); const n = document.getElementById('ppl-hc'); if (n) n.innerHTML = this.hostedHtml(); });
  },

  wire(el) {
    const add = el.querySelector('#ppl-add-keys');
    if (add) add.addEventListener('click', () => {
      V3.go('#/settings');
      setTimeout(() => V3.flashTarget('fold-keys'), 80);
    });
    this.wireHosted(el);
  },

  live(what) {
    if (!['people', 'agents', 'ready'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA|SELECT/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
  }
};
