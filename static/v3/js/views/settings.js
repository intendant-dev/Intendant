/* V3 — Settings: questions, not subsystems.
   The catalog below is THE one table: this view renders from it and the
   ⌘K palette indexes from it (derive, don't mirror — a row added here is
   searchable there, automatically). Save is merge-then-POST. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

/* key = SettingsPayload field; fold = jump target id; kind drives the row UI */
V3.settingsCatalog = [
  { key: '_autonomy', section: 'How much may the house do on its own?', name: 'Autonomy', kind: 'dial',
    aliases: 'autonomy independence freedom self-driving', gloss: 'How far the house goes before it asks.' },
  { key: '_approvals', section: 'How much may the house do on its own?', name: 'Approval rules', kind: 'rules', fold: 'fold-approvals',
    aliases: 'approval rules gates ask auto deny permissions' },

  { key: '_keys', section: 'Who provides the minds?', name: 'API keys', kind: 'keys', fold: 'fold-keys',
    aliases: 'api keys openai anthropic gemini fuel credentials' },
  { key: 'external_agent', section: 'Who provides the minds?', name: 'Default external agent', kind: 'select',
    options: ['', 'codex', 'claude-code'], aliases: 'codex claude backend default agent' },
  { key: 'codex_model', section: 'Who provides the minds?', name: 'Codex model', kind: 'text', aliases: 'codex model gpt' },
  { key: 'codex_reasoning_effort', section: 'Who provides the minds?', name: 'Codex reasoning effort', kind: 'select',
    options: ['minimal', 'low', 'medium', 'high', 'xhigh'], aliases: 'codex reasoning effort thinking' },
  { key: 'claude_model', section: 'Who provides the minds?', name: 'Claude model', kind: 'text', aliases: 'claude model fable opus sonnet haiku' },

  { key: 'presence_enabled', section: 'How does it reach you?', name: 'Presence', kind: 'toggle', fold: 'fold-presence',
    aliases: 'presence voice text live' },
  { key: 'presence_model', section: 'How does it reach you?', name: 'Presence text model', kind: 'text', aliases: 'presence text model' },
  { key: 'presence_live_model', section: 'How does it reach you?', name: 'Live voice model', kind: 'text', aliases: 'live voice model gemini realtime' },
  { key: 'transcription_enabled', section: 'How does it reach you?', name: 'Transcription', kind: 'toggle', aliases: 'transcription whisper speech' },
  { key: 'live_audio_timeout_secs', section: 'How does it reach you?', name: 'Live audio idle timeout (s)', kind: 'number', aliases: 'live audio timeout idle' },

  { key: 'cu_backend', section: 'What may it see and touch?', name: 'Computer-use backend', kind: 'select', fold: 'fold-cu',
    options: ['auto', 'x11', 'wayland', 'macos'], aliases: 'computer use display screenshot backend' },
  { key: 'recording_enabled', section: 'What may it see and touch?', name: 'Recording', kind: 'toggle', aliases: 'recording video capture' },
  { key: 'recording_framerate', section: 'What may it see and touch?', name: 'Recording framerate', kind: 'number', aliases: 'framerate fps recording' },
  { key: 'recording_quality', section: 'What may it see and touch?', name: 'Recording quality', kind: 'select',
    options: ['low', 'medium', 'high'], aliases: 'recording quality' },

  { key: '_appearance', section: 'How should it look and feel?', name: 'Theme & density', kind: 'appearance', fold: 'fold-appearance',
    aliases: 'theme dark light density cozy studio appearance' },

  { key: 'codex_sandbox', section: 'The fine print', name: 'Codex sandbox', kind: 'select', fold: 'fold-codex-sandbox',
    options: ['read-only', 'workspace-write', 'danger-full-access'], aliases: 'codex sandbox permissions' },
  { key: 'codex_writable_roots', section: 'The fine print', name: 'Codex writable roots', kind: 'text',
    aliases: 'writable roots sandbox workspace-write paths' },
  { key: 'codex_network_access', section: 'The fine print', name: 'Codex network access', kind: 'toggle', aliases: 'codex network internet egress' },
  { key: 'codex_web_search', section: 'The fine print', name: 'Codex web search', kind: 'toggle', aliases: 'codex web search tool' },
  { key: 'codex_service_tier', section: 'The fine print', name: 'Codex service tier', kind: 'text', aliases: 'codex service tier priority' },
  { key: 'codex_managed_context', section: 'The fine print', name: 'Codex managed context', kind: 'select',
    options: ['off', 'vanilla', 'managed'], aliases: 'codex managed context rewind' },
  { key: 'claude_permission_mode', section: 'The fine print', name: 'Claude permission mode', kind: 'select',
    options: ['default', 'acceptEdits', 'plan', 'bypassPermissions'], aliases: 'claude permission mode acceptEdits' },
  { key: 'claude_allowed_tools', section: 'The fine print', name: 'Claude allowed tools', kind: 'text', aliases: 'claude allowed tools' },
  { key: '_env', section: 'The fine print', name: 'Env overrides', kind: 'ro', aliases: 'env environment variables overrides debug' },
  { key: '_raw', section: 'The fine print', name: 'Raw settings payload', kind: 'toml', fold: 'fold-toml', aliases: 'raw json toml config file' }
];

V3.views.settings = {
  title: 'Settings',

  render(el) {
    const D = V3.data;
    const payload = D.settings || {};
    const sections = {};
    V3.settingsCatalog.forEach(row => { (sections[row.section] = sections[row.section] || []).push(row); });

    el.innerHTML = V3.page({
      eyebrow: 'how the house behaves',
      title: 'Settings',
      sub: 'Plain questions up front. Every knob the daemon honors is here — folded, findable, and one ⌘K away by its plain name. Changes save to the daemon, not the browser.',
      body: Object.keys(sections).map(sec =>
        V3.section(sec) + sections[sec].map(row => this.rowHtml(row, payload)).join('')
      ).join('') +
      '<div class="row" style="margin-top:16px;gap:8px">' +
        '<button class="btn btn-primary" id="settings-save">Save</button>' +
        '<span class="dim" id="settings-dirty" style="font-size:12.5px">no unsaved changes</span>' +
        '<span class="grow"></span>' +
        '<span class="fact">writes through the daemon — approval rules apply live</span>' +
      '</div>'
    });
    this.wire(el);
  },

  rowHtml(row, payload) {
    const val = payload[row.key];
    switch (row.kind) {
      case 'dial': {
        const cur = V3.data.autonomy || 'medium';
        const levels = [
          ['low', 'reads only — it asks before it touches'],
          ['medium', 'gate writes — the everyday default'],
          ['high', 'auto unless a rule says deny'],
          ['full', 'ungated — you watch the log, not the queue']
        ];
        return '<div class="card" id="set-_autonomy"><div class="card-head"><div><h3 class="card-title">Autonomy</h3>' +
          '<div class="card-sub">' + row.gloss + ' Currently: <b>' + V3.esc(cur) + '</b>.</div></div></div>' +
          '<div class="grid" style="grid-template-columns:repeat(4,1fr);gap:8px">' +
          levels.map(l =>
            '<button class="panel" data-autonomy="' + l[0] + '" style="text-align:left;cursor:pointer;border-color:' + (l[0] === cur ? 'rgba(var(--brass-rgb),.5)' : 'var(--line)') + '">' +
            '<b style="text-transform:capitalize">' + l[0] + '</b><div class="dim" style="font-size:12px;margin-top:2px">' + l[1] + '</div></button>').join('') +
          '</div></div>';
      }
      case 'rules': {
        const cats = [
          ['file_read', 'Read files'], ['file_write', 'Edit & write'], ['file_delete', 'Delete files'],
          ['command_exec', 'Run shell commands'], ['network', 'Network egress'], ['destructive', 'Destructive actions'],
          ['display_control', 'Control your display'], ['tool_call', 'External-agent tool calls']
        ];
        return V3.foldHtml({ key: 'settings.rules', id: row.fold, title: row.name, note: 'live-applied, saved by the daemon',
          body: '<div class="col" style="gap:4px">' +
            cats.map(([k, label]) => {
              const cur = payload['approval_' + k] || 'ask';
              return '<div class="row"><span style="width:210px">' + label + '</span>' +
                '<span class="seg">' + ['auto', 'ask', 'deny'].map(v =>
                  '<button class="' + (cur === v ? 'on' : '') + '" data-srule="' + k + ':' + v + '">' + v + '</button>').join('') + '</span></div>';
            }).join('') +
          '</div>' });
      }
      case 'keys': {
        const fuel = V3.data.fuel || {};
        const rows = [['Anthropic', 'anthropic'], ['OpenAI', 'openai'], ['Gemini', 'gemini']];
        return V3.foldHtml({ key: 'settings.keys', id: row.fold, title: row.name, note: 'presence checked, never shown',
          body: '<div class="col" style="gap:8px">' +
            rows.map(([label, k]) => '<div class="row"><span style="width:110px">' + label + '</span>' +
              '<input class="input mono" type="password" data-key="' + k + '" placeholder="••••••••" style="flex:1">' +
              V3.chip(fuel[k] ? 'set' : 'not set', fuel[k] ? 'sage' : 'attn') + '</div>').join('') +
            '<div class="dim" style="font-size:12px">Saved by the daemon to its .env — the page never reads existing keys back.</div>' +
          '</div>' });
      }
      case 'appearance': {
        return V3.foldHtml({ key: 'settings.appearance', id: row.fold, title: row.name, note: 'this browser, instant', open: true,
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
                  '<button data-density-val="' + d + '" class="' + (V3.density() === d ? 'on' : '') + '">' + d[0].toUpperCase() + d.slice(1) + '</button>').join('') +
              '</span></div>' +
            '<div class="dim" style="font-size:12px;max-width:36ch">Cozy speaks first. Studio shows the machinery first. Nothing is hidden at any density — only unfolded.</div>' +
          '</div>' });
      }
      case 'toggle': {
        return this.setRow(row, '<label class="row" style="gap:8px;justify-content:flex-start">' +
          '<input type="checkbox" data-set="' + row.key + '" ' + (val ? 'checked' : '') + '>' +
          '<span>' + (val ? 'on' : 'off') + '</span></label>');
      }
      case 'select': {
        return this.setRow(row, '<span class="seg">' + row.options.map(o =>
          '<button data-set="' + row.key + '" data-val="' + o + '" class="' + ((val || '') === o ? 'on' : '') + '">' + (o || 'none') + '</button>').join('') + '</span>');
      }
      case 'number': {
        return this.setRow(row, '<input class="input mono" type="number" data-set="' + row.key + '" value="' + V3.esc(val == null ? '' : val) + '" style="width:110px">');
      }
      case 'text': {
        return this.setRow(row, '<input class="input mono" data-set="' + row.key + '" value="' + V3.esc(val == null ? '' : val) + '" style="flex:1">');
      }
      case 'ro': {
        const env = payload.env_overrides || {};
        const keys = Object.keys(env);
        return this.setRow(row, keys.length
          ? '<div class="kv">' + keys.map(k => '<span class="k">' + V3.esc(k) + '</span><span class="v">' + V3.esc(env[k]) + '</span>').join('') + '</div>'
          : '<span class="dim" style="font-size:12.5px">none set — the daemon runs its defaults</span>');
      }
      case 'toml': {
        return V3.foldHtml({ key: 'settings.toml', id: row.fold, title: row.name, note: 'the truth, unstyled',
          body: '<div class="panel mono" style="font-size:12px;line-height:1.7;white-space:pre;max-height:320px;overflow:auto">' +
            V3.esc(JSON.stringify(payload, null, 2)) + '</div>' });
      }
    }
    return '';
  },

  setRow(row, controlHtml) {
    return '<div class="card" id="set-' + row.key + '" style="padding:12px 16px"><div class="row">' +
      '<span style="min-width:240px">' + row.name + '</span>' + controlHtml + '</div></div>';
  },

  wire(el) {
    const dirty = el.querySelector('#settings-dirty');
    const patch = {};
    const mark = () => { dirty.textContent = Object.keys(patch).length ? Object.keys(patch).length + ' unsaved change' + (Object.keys(patch).length > 1 ? 's' : '') : 'no unsaved changes'; };

    el.querySelectorAll('[data-autonomy]').forEach(b => b.addEventListener('click', () => {
      el.querySelectorAll('[data-autonomy]').forEach(x => x.style.borderColor = 'var(--line)');
      b.style.borderColor = 'rgba(var(--brass-rgb),.5)';
      V3.actions.setAutonomy(b.dataset.autonomy);
      V3.data.autonomy = b.dataset.autonomy;
    }));
    el.querySelectorAll('[data-srule]').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      const [cat, rule] = b.dataset.srule.split(':');
      V3.actions.setApprovalRule(cat, rule);
    }));
    el.querySelectorAll('[data-set]').forEach(inp => {
      if (inp.type === 'checkbox') inp.addEventListener('change', () => { patch[inp.dataset.set] = inp.checked; mark(); });
      else if (inp.tagName === 'BUTTON') inp.addEventListener('click', () => {
        inp.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
        inp.classList.add('on');
        patch[inp.dataset.set] = inp.dataset.val; mark();
      });
      else inp.addEventListener('change', () => {
        patch[inp.dataset.set] = inp.type === 'number' ? Number(inp.value) : inp.value; mark();
      });
    });
    el.querySelectorAll('[data-key]').forEach(inp => inp.addEventListener('change', () => {
      if (!inp.value.trim()) return;
      V3.transport.post('/api/api-keys', { [inp.dataset.key]: inp.value.trim() })
        .then(() => V3.toast('Key saved by the daemon', 'sage'))
        .catch(e => V3.toast('Key save failed: ' + e.message, 'brick'));
      inp.value = '';
    }));
    el.querySelectorAll('[data-theme-val]').forEach(b => b.addEventListener('click', () => {
      V3.setTheme(b.dataset.themeVal);
      el.querySelectorAll('[data-theme-val]').forEach(x => x.classList.toggle('on', x === b));
    }));
    el.querySelectorAll('[data-density-val]').forEach(b => b.addEventListener('click', () => {
      V3.setDensity(b.dataset.densityVal);
      el.querySelectorAll('[data-density-val]').forEach(x => x.classList.toggle('on', x === b));
    }));
    el.querySelector('#settings-save').addEventListener('click', () => {
      if (!Object.keys(patch).length) { V3.toast('Nothing to save', null); return; }
      V3.actions.saveSettings(patch).then(() => {
        Object.keys(patch).forEach(k => delete patch[k]); mark();
      });
    });
  }
};
