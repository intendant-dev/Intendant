/* Proscenium — Settings: questions, not subsystems.
   Every row is ⌘K-addressable (fold ids match data.js); the fine print
   unfolds to the raw intendant.toml. */
window.P = window.P || {};
P.views = P.views || {};

P.views.settings = {
  title: 'Settings',

  render(el) {
    const sections = {};
    P.data.settings.forEach(row => { (sections[row.section] = sections[row.section] || []).push(row); });

    el.innerHTML = P.page({
      eyebrow: 'how the house behaves',
      title: 'Settings',
      sub: 'Plain questions up front. Every knob the daemon honors is here — folded, findable, and one ⌘K away by its plain name (“writable roots”, “quiet hours”…).',
      body: Object.keys(sections).map(sec =>
        P.section(sec) + sections[sec].map(row => this.rowHtml(row)).join('')
      ).join('') +
      '<div class="row" style="margin-top:16px;gap:8px">' +
        '<button class="btn btn-primary" id="settings-save">Save</button>' +
        '<button class="btn btn-quiet">Reset to the daemon’s defaults</button>' +
        '<span class="grow"></span>' +
        '<span class="fact">writes intendant.toml · project first, daemon-wide as fallback</span>' +
      '</div>'
    });

    this.wire(el);
  },

  rowHtml(row) {
    switch (row.kind) {
      case 'dial': {
        const levels = [
          ['Low', 'reads only — it asks before it touches'],
          ['Medium', 'gate writes — the everyday default'],
          ['High', 'auto unless a rule says deny'],
          ['Full', 'ungated — you watch the log, not the queue']
        ];
        return '<div class="card"><div class="card-head"><div><h3 class="card-title">Autonomy</h3>' +
          '<div class="card-sub">' + row.gloss + ' Currently: <b>' + row.value + '</b>.</div></div></div>' +
          '<div class="grid" style="grid-template-columns:repeat(4,1fr);gap:8px">' +
          levels.map((l, i) =>
            '<button class="panel' + (i === 1 ? ' autonomy-on' : '') + '" data-autonomy="' + l[0] + '" style="text-align:left;cursor:pointer;border-color:' + (i === 1 ? 'rgba(var(--brass-rgb),.5)' : 'var(--line)') + '">' +
            '<b>' + l[0] + '</b><div class="dim" style="font-size:12px;margin-top:2px">' + l[1] + '</div></button>').join('') +
          '</div></div>';
      }
      case 'rules': {
        return P.foldHtml({ key: 'settings.rules', id: row.fold, title: row.name, note: 'live-applied · saves to intendant.toml',
          body: '<div class="col" style="gap:4px">' +
            row.rows.map(r => '<div class="row"><span style="width:210px">' + r[0] + '</span>' +
              '<span class="seg">' + ['auto', 'ask', 'deny'].map(v =>
                '<button class="' + (r[1] === v ? 'on' : '') + '" data-srule="' + r[0] + ':' + v + '">' + v + '</button>').join('') + '</span></div>').join('') +
          '</div>' });
      }
      case 'keys': {
        return P.foldHtml({ key: 'settings.keys', id: row.fold, title: row.name, note: 'presence checked, never shown',
          body: '<div class="col" style="gap:8px">' +
            row.rows.map(r => '<div class="row"><span style="width:110px">' + r[0] + '</span>' +
              '<input class="input mono" type="password" placeholder="••••••••" style="flex:1">' +
              P.chip(r[1].startsWith('set') ? 'set' : 'not set', r[1].startsWith('set') ? 'sage' : 'attn') + '</div>').join('') +
            '<div class="dim" style="font-size:12px">Saved to ~/.config/intendant/.env — or skip keys entirely and fuel from the vault (People & Keys).</div>' +
          '</div>' });
      }
      case 'rows': {
        return P.foldHtml({ key: 'settings.' + row.name, id: row.fold, title: row.name, note: row.rows.length + ' rows',
          body: '<div class="kv">' + row.rows.map(r =>
            '<span class="k">' + P.esc(r[0]) + '</span><span class="v">' + P.esc(r[1]) + '</span>').join('') + '</div>' });
      }
      case 'appearance': {
        return P.foldHtml({ key: 'settings.appearance', id: row.fold, title: row.name, note: 'applies instantly, this browser', open: true,
          body:
          '<div class="row" style="gap:20px;flex-wrap:wrap">' +
            '<div><div class="eyebrow" style="margin-bottom:6px">Theme</div>' +
              '<span class="seg" id="seg-theme">' +
                '<button data-theme-val="lamplight" class="' + (document.documentElement.dataset.theme === 'lamplight' ? 'on' : '') + '">Lamplight</button>' +
                '<button data-theme-val="daylight" class="' + (document.documentElement.dataset.theme === 'daylight' ? 'on' : '') + '">Daylight</button>' +
              '</span></div>' +
            '<div><div class="eyebrow" style="margin-bottom:6px">Density</div>' +
              '<span class="seg" id="seg-density">' +
                ['cozy', 'standard', 'studio'].map(d =>
                  '<button data-density-val="' + d + '" class="' + (P.density() === d ? 'on' : '') + '">' + d[0].toUpperCase() + d.slice(1) + '</button>').join('') +
              '</span></div>' +
            '<div class="dim" style="font-size:12px;max-width:36ch">Cozy speaks first. Studio shows the machinery first. Nothing is hidden at any density — only unfolded.</div>' +
          '</div>' });
      }
      case 'toml': {
        return P.foldHtml({ key: 'settings.toml', id: row.fold, title: row.name, note: 'the truth is a text file',
          body:
          '<div class="panel mono" style="font-size:12px;line-height:1.7;white-space:pre">[agent]\nautonomy = "medium"\napproval_rules = { file_read = "auto", file_write = "ask",\n  file_delete = "ask", command_exec = "ask", network = "auto",\n  destructive = "deny", display_control = "ask", tool_call = "ask" }\n\n[presence]\nenabled = true\ntext_model = "claude-sonnet"\nlive_model = "gemini-2.5-flash-live"\n\n[recording]\nframerate = 5\nquality = "high"</div>' +
          '<div class="row" style="margin-top:8px;gap:8px">' +
            '<button class="btn btn-quiet btn-xs" data-act="toml-validate">validate</button>' +
            '<span class="fact" id="toml-status">valid · matches what the daemon is running</span>' +
          '</div>' });
      }
    }
    return '';
  },

  wire(el) {
    el.querySelectorAll('[data-autonomy]').forEach(b => b.addEventListener('click', () => {
      el.querySelectorAll('[data-autonomy]').forEach(x => x.style.borderColor = 'var(--line)');
      b.style.borderColor = 'rgba(var(--brass-rgb),.5)';
      P.toast('Autonomy → ' + b.dataset.autonomy + ' — takes effect on the next task', 'sage');
    }));
    el.querySelectorAll('[data-srule]').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      const [rule, v] = b.dataset.srule.split(':');
      P.toast(rule + ' → ' + v + ' — live', 'sage');
    }));
    el.querySelectorAll('[data-theme-val]').forEach(b => b.addEventListener('click', () => {
      P.setTheme(b.dataset.themeVal);
      el.querySelectorAll('[data-theme-val]').forEach(x => x.classList.toggle('on', x === b));
    }));
    el.querySelectorAll('[data-density-val]').forEach(b => b.addEventListener('click', () => {
      P.setDensity(b.dataset.densityVal);
      el.querySelectorAll('[data-density-val]').forEach(x => x.classList.toggle('on', x === b));
    }));
    el.querySelector('#settings-save').addEventListener('click', () =>
      P.toast('Saved to intendant.toml — the daemon picked it up live', 'sage'));
    const v = el.querySelector('[data-act="toml-validate"]');
    if (v) v.addEventListener('click', () =>
      P.toast('Valid TOML — no drift from the running daemon', 'sage'));
  }
};
