/* Proscenium — People & Keys: who may do what.
   You → people & devices → doors → keys & vault (⌘K lands on #fold-vault)
   → organizations (studio). Trust doctrine stays literal: petnames first,
   routes labeled, claiming grants no authority, hosted ceiling role:none. */
window.P = window.P || {};
P.views = P.views || {};

P.views.people = {
  title: 'People & Keys',
  vaultOpen: false,

  /* the 7 builtin roles, in plain words */
  ROLES: [
    ['owner', 'everything, including who else gets keys'],
    ['operator', 'runs work and touches screens; can’t hand out keys'],
    ['spectator', 'watches everything, touches nothing'],
    ['session-reader', 'reads finished work only'],
    ['terminal', 'a shared shell'],
    ['files-read', 'reads files, nothing more'],
    ['files-write', 'drops files in, can’t read them back']
  ],

  CLAIM: 'witch collapse practice feed harbor lantern orbit meadow velvet paper anchor stone',

  render(el) {
    const you = P.data.you;

    el.innerHTML = P.page({
      eyebrow: 'who may do what',
      title: 'People & Keys',
      sub: 'Every key the house honors, what it may do, and where the fuel lives. Petnames first — the route is always labeled, and no door mints authority.',
      body:
        /* ---------- You ---------- */
        '<div class="grid grid-2">' +
          '<div class="card">' +
            '<div class="row" style="gap:10px;align-items:baseline;flex-wrap:wrap">' +
              '<span class="voice" style="font-size:23px">' + P.esc(you.name) + '</span>' +
              P.chip(you.role, 'brass', 'shield') + P.routeChip(you.route.replace(' mTLS', '')) +
            '</div>' +
            '<div class="kv" style="margin-top:12px">' +
              '<span class="k">key</span><span class="v">' + P.esc(you.keyId) + '</span>' +
              '<span class="k">device</span><span class="v">' + P.esc(you.device) + '</span>' +
              '<span class="k">route</span><span class="v">' + P.esc(you.route) + '</span>' +
              '<span class="k">since</span><span class="v">' + P.esc(you.since) + '</span>' +
            '</div>' +
            '<div class="factline" style="margin-top:10px">' +
              P.fact('your open tabs: 2 dashboard · 1 holding voice') +
            '</div>' +
          '</div>' +
          P.card({
            title: 'Trust tier',
            sub: 'where this browser stands',
            body:
              '<div class="row">' + P.chip('integrated', 'sage', 'shield') + P.fact('the strongest tier') + '</div>' +
              '<p style="margin:10px 0 8px;font-size:13.5px">This machine is integrated — it holds real keys. ' +
              'What this key signs, the house trusts; a hosted page can never mint its way here.</p>' +
              '<div class="dim" style="font-size:12.5px">Lower tiers exist on purpose: a fleet-named device watches and works, a hosted tab only finds the door.</div>'
          }) +
        '</div>' +

        /* ---------- People & devices ---------- */
        P.section('People & devices') +
        '<div class="card" style="padding:6px"><table class="table"><thead><tr>' +
        '<th>Who</th><th>Key</th><th>Role</th><th>Route</th><th>Lifecycle</th><th>Since</th></tr></thead><tbody>' +
        P.data.people.map(p =>
          '<tr>' +
          '<td>' + P.esc(p.who) + '</td>' +
          '<td class="mono">' + P.esc(p.key) + '</td>' +
          '<td>' + P.chip(p.role, p.role === 'owner' ? 'brass' : 'slate') + '</td>' +
          '<td>' + (p.route === '—' ? '<span class="dim">—</span>' : P.routeChip(p.route.replace(' mTLS', ''))) + '</td>' +
          '<td>' + P.chip(p.lifecycle, p.lifecycle === 'active' ? 'sage' : 'brick') + '</td>' +
          '<td class="mono">' + P.esc(p.since) + '</td>' +
          '</tr>').join('') +
        '</tbody></table></div>' +

        P.foldHtml({ key: 'people.grant', title: 'Hand out a key', note: 'a promise you can revoke any time',
          body:
          '<div class="dim" style="font-size:13px;margin:4px 0 10px">Pick what the new key may do — plain words up front, the compiled role rides along underneath.</div>' +
          '<div class="ppl-roles">' +
            this.ROLES.map((r, i) =>
              '<label class="panel ppl-role"><input type="radio" name="ppl-role" value="' + r[0] + '"' + (i === 1 ? ' checked' : '') + '>' +
              '<b>' + P.esc(r[0]) + '</b><div class="dim">' + P.esc(r[1]) + '</div></label>').join('') +
          '</div>' +
          '<div class="row" style="margin-top:12px;gap:10px;flex-wrap:wrap">' +
            '<button class="btn btn-primary" id="ppl-issue">' + P.ICON('key', 15) + ' Issue this key</button>' +
            '<span class="dim" style="font-size:12.5px">A device that wants in asks first — enrollment requests arrive in <a href="#/home">the Queue</a>, and you decide there.</span>' +
          '</div>' }) +

        /* ---------- Doors ---------- */
        P.section('Doors — the ways in') +
        '<div class="grid grid-3">' +
          P.card({
            title: 'Connect', sub: 'the hosted rendezvous',
            body:
              '<div class="row">' + P.chip('linked', 'sage') + P.fact('route healthy · 42 ms') + '</div>' +
              '<div class="kv" style="margin-top:10px">' +
                '<span class="k">account</span><span class="v">val@example.com</span>' +
                '<span class="k">trusted for</span><span class="v">discovery & routes</span>' +
              '</div>' +
              '<div class="dim" style="margin-top:10px;font-size:12.5px">Connect knows your fleet’s names and where to knock. It can introduce — it mints no authority here, and it holds no keys.</div>'
          }) +
          P.card({
            title: 'Claim code', sub: 'a name tag, not a key — one use, twelve words',
            body:
              '<div class="panel mono ppl-claim">witch collapse practice feed<br>harbor lantern orbit meadow<br>velvet paper anchor stone</div>' +
              '<div class="row" style="margin-top:10px;gap:8px">' +
                '<button class="btn btn-quiet btn-xs" id="ppl-copy-claim">' + P.ICON('attach', 13) + ' copy</button>' +
                '<span class="grow"></span>' + P.fact('expires tonight') +
              '</div>' +
              '<div class="dim" style="margin-top:8px;font-size:12.5px">Claiming grants no authority. It lets a new browser find the fleet and say its name — then it still asks you for a key.</div>'
          }) +
          P.card({
            title: 'Hosted control', sub: 'reaching this house from anywhere',
            body:
              '<div class="seg" id="ppl-hosted">' +
                '<button class="on" data-hc="View">View</button><button data-hc="Tasks">Tasks</button><button data-hc="Operate">Operate</button>' +
              '</div>' +
              '<div class="dim" style="margin-top:10px;font-size:12.5px">The compiled floor: hosted sessions can never exceed <b>role:none</b> without an owner ceremony on this machine. Tasks and Operate mint time-boxed leases you can revoke.</div>' +
              '<div class="row" style="margin-top:10px">' +
                '<button class="btn btn-quiet btn-xs" id="ppl-mint">mint a hosted lease</button>' +
              '</div>'
          }) +
        '</div>' +

        /* ---------- Keys & vault (⌘K target) ---------- */
        P.foldHtml({ key: 'people.vault', id: 'fold-vault', title: 'Keys & vault', note: 'fuel · custody · ceremonies',
          body:
          '<div id="ppl-vault-state">' + this.vaultHtml() + '</div>' +

          '<div class="eyebrow" style="margin:18px 0 6px">Fuel — leases the vault feeds</div>' +
          '<table class="table"><thead><tr><th>Machine</th><th>Fuel</th><th>Standing</th><th></th></tr></thead><tbody>' +
            '<tr><td>Workshop</td><td class="mono">claude · max</td><td>' + P.chip('2 days left', 'attn') + '</td>' +
              '<td style="white-space:nowrap;text-align:right">' +
                '<button class="btn btn-quiet btn-xs" data-lease="renew:Workshop">renew</button> ' +
                '<button class="btn btn-danger btn-xs" data-lease="revoke:Workshop">revoke</button></td></tr>' +
            '<tr><td>Parlor PC</td><td class="mono">—</td><td>' + P.chip('dry — expired', 'brick') + '</td>' +
              '<td style="text-align:right"><button class="btn btn-quiet btn-xs" data-lease="renew:Parlor PC">renew</button></td></tr>' +
          '</tbody></table>' +
          '<div class="dim" style="font-size:12px;margin-top:6px">Leases live in memory only — a daemon restart politely goes dry; nothing credential-shaped touches the disk.</div>' +

          '<div class="eyebrow" style="margin:18px 0 6px">Custody trail — where secrets have been</div>' +
          '<div class="ppl-trail">' +
            P.data.custody.map(c =>
              '<div class="ppl-trail-row"><span class="ppl-trail-dot"></span>' +
              '<span class="fact" style="width:48px;flex:none">' + P.esc(c.at) + '</span>' +
              '<span style="flex:1;font-size:13px;color:var(--text-2)">' + P.esc(c.what) + '</span>' +
              '<span class="dim" style="font-size:12px">' + P.esc(c.by) + '</span></div>').join('') +
          '</div>' +

          '<div class="row" style="margin-top:14px">' + P.ICON('external', 14) +
            '<span style="font-size:13px">Client egress: <span class="mono" style="font-size:12px">api.anthropic.com</span> rides out through this browser — registered Monday.</span>' +
            '<span class="grow"></span>' + P.fact('one registration') + '</div>' +

          '<div style="margin-top:14px">' +
          P.foldHtml({ key: 'people.ceremonies', title: 'Agent sign-in ceremonies', note: 'Claude Code · Codex device auth',
            body:
            '<div class="grid grid-2">' +
              '<div class="panel"><div class="row"><b>Claude Code</b><span class="grow"></span>' + P.chip('not linked', 'slate') + '</div>' +
                '<div class="dim" style="font-size:12.5px;margin:6px 0 10px">The house runs the device-auth dance in a supervised terminal — you see the URL, the code, and exactly where the token lands.</div>' +
                '<button class="btn btn-quiet btn-xs" data-ceremony="Claude Code">start ceremony</button></div>' +
              '<div class="panel"><div class="row"><b>Codex</b><span class="grow"></span>' + P.chip('not linked', 'slate') + '</div>' +
                '<div class="dim" style="font-size:12.5px;margin:6px 0 10px">Same ceremony, different provider — a device code, your approval in the browser, the token sealed into the vault.</div>' +
                '<button class="btn btn-quiet btn-xs" data-ceremony="Codex">start ceremony</button></div>' +
            '</div>' }) +
          '</div>' }) +

        /* ---------- Organizations (studio) ---------- */
        '<div class="studio-only">' +
        P.foldHtml({ key: 'people.orgs', title: 'Organizations', note: 'trusted roots · role caps · revocation lists',
          body:
          '<table class="table"><thead><tr><th>Org root</th><th>Role cap</th><th>ORL</th></tr></thead><tbody>' +
            '<tr><td class="mono">acme-labs</td><td>' + P.chip('operator', 'slate') + '</td><td>' + P.chip('current', 'sage') + '</td></tr>' +
            '<tr><td class="mono">family</td><td>' + P.chip('spectator', 'slate') + '</td><td>' + P.chip('current', 'sage') + '</td></tr>' +
          '</tbody></table>' +
          '<div class="dim" style="font-size:12.5px;margin-top:8px">An org root signs grant documents and revocation lists; no org key can raise a grant above its cap. Joining with an org grant arrives in the Queue like any enrollment.</div>' }) +
        '</div>'
    });

    this.wire(el);
  },

  vaultHtml() {
    const v = P.data.vault;
    if (!this.vaultOpen) {
      return '<div class="row" style="flex-wrap:wrap">' +
        P.chip(v.state, 'attn', 'shield') +
        P.fact(v.entries + ' entries · ' + v.passkeys + ' passkeys · ' + v.recovery) +
        '<span class="grow"></span>' +
        '<button class="btn btn-safe btn-xs" id="ppl-vault-toggle">' + P.ICON('key', 13) + ' Unlock</button>' +
      '</div>';
    }
    return '<div class="row" style="flex-wrap:wrap">' +
        P.chip('Unlocked', 'sage', 'shield') +
        P.fact('locks again on sleep or 10 idle minutes') +
        '<span class="grow"></span>' +
        '<button class="btn btn-quiet btn-xs" id="ppl-vault-toggle">Lock</button>' +
      '</div>' +
      '<div class="panel" style="margin-top:10px"><div class="kv">' +
        '<span class="k">Anthropic key</span><span class="v">sk-ant-…3f9</span>' +
        '<span class="k">OpenAI key</span><span class="v">sk-…k21</span>' +
        '<span class="k">Gemini key</span><span class="v">AIza-…8qx</span>' +
        '<span class="k">NAS password</span><span class="v">••••••••••</span>' +
      '</div></div>';
  },

  wire(el) {
    const issue = el.querySelector('#ppl-issue');
    if (issue) issue.addEventListener('click', () => {
      const role = (el.querySelector('input[name="ppl-role"]:checked') || {}).value || 'operator';
      P.toast(role[0].toUpperCase() + role.slice(1) + ' key issued — revoke it here any time', 'sage');
    });

    const copy = el.querySelector('#ppl-copy-claim');
    if (copy) copy.addEventListener('click', () => {
      try { if (navigator.clipboard) navigator.clipboard.writeText(this.CLAIM).catch(() => {}); } catch (e) {}
      P.toast('Copied — twelve words, one use. It names; it never grants.', 'sage');
    });

    el.querySelectorAll('[data-hc]').forEach(b => b.addEventListener('click', () => {
      el.querySelectorAll('[data-hc]').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      const gloss = {
        View: 'View — watches dashboards and finished work; the floor stays role:none',
        Tasks: 'Tasks — may start and steer work under a time-boxed lease',
        Operate: 'Operate — adds screens and input, still under the lease, still revocable'
      };
      P.toast(gloss[b.dataset.hc] || 'Preset selected', null);
    }));
    const mint = el.querySelector('#ppl-mint');
    if (mint) mint.addEventListener('click', () =>
      P.toast('A hosted lease would be minted here — time-boxed, revocable, floored at role:none', null));

    el.querySelectorAll('[data-lease]').forEach(b => b.addEventListener('click', () => {
      const parts = b.dataset.lease.split(':');
      const act = parts[0], machine = parts.slice(1).join(':');
      if (act === 'renew') P.toast('Lease renewed — ' + machine + ' stays fueled', 'sage');
      else P.toast('Lease revoked — ' + machine + ' politely goes dry', 'brick');
    }));

    el.querySelectorAll('[data-ceremony]').forEach(b => b.addEventListener('click', () =>
      P.toast('The ' + b.dataset.ceremony + ' ceremony would open a supervised terminal here', null)));

    this.wireVault(el);
  },

  wireVault(el) {
    const t = el.querySelector('#ppl-vault-toggle');
    if (!t) return;
    t.addEventListener('click', () => {
      this.vaultOpen = !this.vaultOpen;
      el.querySelector('#ppl-vault-state').innerHTML = this.vaultHtml();
      this.wireVault(el);
      P.toast(this.vaultOpen ? 'Vault unlocked — entries stay masked until you ask' : 'Vault locked', this.vaultOpen ? 'sage' : null);
    });
  }
};
