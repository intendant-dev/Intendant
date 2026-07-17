/* V3 — the Session Space: one deep page per session.
   Timeline · Context · Controls · Vitals & lineage. Live via the store's
   rolling log buffer + session events; history paged in from
   GET /api/session/{id}. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.session = {
  title: 'Session',
  historyLoaded: {},

  render(el, params) {
    const id = params[0];
    const s = V3.data.sessions.find(x => x.id === id) || { id, name: id, phase: 'idle', sentence: 'Loading…', backend: '', model: '', tokens: {} };
    this.current = s;
    const q = V3.data.queue.filter(x => x.session === id);
    const usage = V3.data.usage[id] || {};
    this.loadHistory(id);

    el.innerHTML = V3.page({
      body:
        '<div class="row" style="align-items:flex-start;gap:14px;flex-wrap:wrap">' +
          '<div style="min-width:280px;flex:1">' +
            '<div class="eyebrow"><a href="#/work" style="color:inherit">work</a> / ' + V3.esc(s.name) + '</div>' +
            '<h1 class="page-title" style="font-size:26px">' + V3.esc(s.name) + '</h1>' +
            '<div class="page-sub">' + V3.esc(s.sentence || s.task || '') + '</div>' +
            '<div class="row" style="gap:6px;flex-wrap:wrap">' +
              V3.chip(s.phase, s.active ? 'sage' : 'slate') +
              (s.backend ? V3.chip(s.backend, 'slate') : '') +
              (s.model ? V3.chip(s.model, 'slate') : '') +
              V3.chip(V3.machineName(s.machine || 'local'), s.machine && s.machine !== 'local' ? 'violet' : null) +
            '</div>' +
          '</div>' +
          '<div class="col" style="gap:8px;align-items:flex-end">' +
            '<div class="row" style="gap:6px">' +
              '<button class="btn btn-quiet" data-act="target">' + V3.ICON('send', 15) + ' Aim composer</button>' +
              (s.active ? '<button class="btn btn-danger" data-act="interrupt">' + V3.ICON('stop', 15) + ' Stop</button>' : '') +
              (!s.active && s.canResume !== false ? '<button class="btn btn-safe" data-act="resume">Resume</button>' : '') +
            '</div>' + V3.authline(V3.data.you.name, V3.data.you.role, V3.data.you.route) +
          '</div>' +
        '</div>' +

        '<div class="card"><div class="factline" style="gap:22px">' +
          '<span>' + V3.fact('turn ' + (s.turn || 0)) + '</span>' +
          (s.tokens && s.tokens.pct ? '<span class="row" style="gap:6px">' + V3.fact('context ' + s.tokens.pct + '%') + V3.meter(s.tokens.pct) + '</span>' : '') +
          (s.cost ? '<span>' + V3.fact('$' + s.cost.toFixed(2)) + '</span>' : '') +
          (usage.cached_tokens ? '<span>' + V3.fact('cache ' + Math.round(100 * usage.cached_tokens / Math.max(1, usage.prompt_tokens || 1)) + '%') + '</span>' : '') +
          (s.cwd ? '<span class="studio-only">' + V3.fact(s.cwd) + '</span>' : '') +
        '</div></div>' +

        (q.length ? V3.section('Waiting on you, here') + q.map(x => V3.decisionCard(x)).join('') : '') +

        V3.foldHtml({ key: 'session.timeline', title: 'Timeline', note: 'live · replayed on reload', open: true,
          body: this.timelineHtml(s) }) +
        V3.foldHtml({ key: 'session.context', title: 'Context', note: (s.tokens && s.tokens.pct ? s.tokens.pct + '%' : '—'),
          body: this.contextHtml(s, usage) }) +
        V3.foldHtml({ key: 'session.controls', title: 'Controls', note: 'approval rules · steering',
          body: this.controlsHtml(s) }) +
        V3.foldHtml({ key: 'session.vitals', title: 'Vitals & lineage', note: 'git · relationships · data',
          body: this.vitalsHtml(s) })
    });
    this.wire(el, s);
  },

  loadHistory(id) {
    if (this.historyLoaded[id]) return;
    this.historyLoaded[id] = true;
    V3.transport.get('/api/session/' + encodeURIComponent(id) + '?limit=80').then(r => {
      (r.entries || []).forEach(e => V3.data.ingestLog(Object.assign({ session_id: id }, e)));
      if (V3.current === 'session') V3.rerender();
    }).catch(() => {});
  },

  timelineHtml(s) {
    const lines = (V3.data.logs[s.id] || []).slice(-120);
    const composer = s.active
      ? '<div class="panel row" style="margin-bottom:12px">' + V3.ICON('send', 14) +
        '<input class="input" id="steer-input" placeholder="Steer mid-turn — it lands at the next safe point…" style="flex:1">' +
        '<button class="btn btn-quiet btn-xs" data-act="steer">steer</button>' +
        '<button class="btn btn-quiet btn-xs" data-act="followup">queue follow-up</button></div>'
      : '';
    return composer +
      (lines.length
        ? V3.logLines(lines.map(l => [l.t, l.kind, (l.text || '').slice(0, 300)]))
        : '<div class="dim" style="font-size:12.5px;padding:8px 0">No work lines yet — they stream in as the house works, and replay when you return.</div>');
  },

  contextHtml(s, usage) {
    const pct = s.tokens && s.tokens.pct || (usage.usage_pct ? Math.round(usage.usage_pct) : 0);
    return '<div class="grid grid-2">' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Budget</div>' +
        '<div class="row" style="gap:10px">' + V3.meter(pct) + '<span class="fact">' + pct + '% of the window</span></div>' +
        '<div class="kv" style="margin-top:10px">' +
          (usage.prompt_tokens ? '<span class="k">prompt tokens</span><span class="v">' + usage.prompt_tokens.toLocaleString() + '</span>' : '') +
          (usage.cached_tokens ? '<span class="k">served from cache</span><span class="v">' + usage.cached_tokens.toLocaleString() + '</span>' : '') +
        '</div></div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Managed context</div>' +
        '<div class="dim" style="font-size:12.5px">Anchors, rewinds, and fission for this session live in the daemon’s managed-context plane — the deep composer is on the V2 roadmap for this room; nothing here is hidden, it’s just not rebuilt yet.</div>' +
        '<div style="margin-top:8px"><a class="chip chip-slate" href="/">open it in the classic dashboard →</a></div></div>' +
    '</div>';
  },

  controlsHtml(s) {
    const cats = [
      ['file_read', 'Read files'], ['file_write', 'Edit & write'], ['file_delete', 'Delete files'],
      ['command_exec', 'Run shell commands'], ['network', 'Network egress'], ['destructive', 'Destructive actions'],
      ['display_control', 'Control your display'], ['tool_call', 'External-agent tool calls']
    ];
    const payload = V3.data.settings || {};
    return '<div class="eyebrow" style="margin-bottom:6px">Approval rules — applied live, saved by the daemon</div>' +
      '<div class="col" style="gap:4px">' +
      cats.map(([k, label]) => {
        const cur = payload['approval_' + k] || 'ask';
        return '<div class="row"><span style="width:190px">' + label + '</span>' +
          '<span class="seg">' + ['auto', 'ask', 'deny'].map(v =>
            '<button class="' + (cur === v ? 'on' : '') + '" data-rule="' + k + ':' + v + '">' + v + '</button>').join('') + '</span></div>';
      }).join('') + '</div>' +
      '<div class="dim" style="margin-top:8px;font-size:12px">Per-backend launch pins (sandbox, model, effort, writable roots) ride along on the next resume — set the defaults in Settings → The fine print.</div>';
  },

  vitalsHtml(s) {
    const v = s.vitals || {};
    const rel = s.relationships || [];
    return '<div class="grid grid-2">' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Git</div>' +
        (v.branch || s.worktree
          ? '<div class="kv"><span class="k">branch</span><span class="v">' + V3.esc(v.branch || s.worktree || '—') + '</span>' +
            (v.dirty != null ? '<span class="k">working tree</span><span class="v">' + (v.dirty ? v.dirty + ' dirty' : 'clean') + '</span>' : '') +
            (v.ahead != null ? '<span class="k">ahead</span><span class="v">' + v.ahead + '</span>' : '') + '</div>'
          : '<div class="dim" style="font-size:12.5px">No git facts reported for this session.</div>') + '</div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Lineage</div>' +
        (rel.length
          ? '<div class="col" style="gap:6px">' + rel.map(r =>
              '<div class="row">' + V3.ICON('branch', 13) + '<span class="mono" style="font-size:12px">' + V3.esc(r.session_id || r.id || r.kind || JSON.stringify(r)) + '</span>' +
              (r.kind ? V3.chip(r.kind, 'slate') : '') + '</div>').join('') + '</div>'
          : '<div class="dim" style="font-size:12.5px">No sub-agents or forks on record.</div>') + '</div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Recordings & data</div>' +
        '<div class="factline">' + V3.fact((s.recordings || 0) + ' recordings') + '</div>' +
        '<div class="row" style="margin-top:8px;gap:6px">' +
          '<a class="btn btn-quiet btn-xs" href="/api/session/' + encodeURIComponent(s.id) + '/report" download>' + V3.ICON('download', 13) + ' report (.zip)</a>' +
        '</div></div>' +
    '</div>';
  },

  wire(el, s) {
    el.querySelectorAll('[data-act]').forEach(b => b.addEventListener('click', () => {
      const act = b.dataset.act;
      if (act === 'interrupt') { V3.actions.interrupt(s.id); V3.toast('Interrupt sent — the turn stops at the next safe point', 'brick'); }
      if (act === 'resume') { V3.actions.resumeSession(s.id); V3.toast('Resuming…', 'sage'); }
      if (act === 'target') {
        V3.actions.setTarget(s.id);
        V3.toast('Composer aimed at ' + s.name, 'sage');
        document.getElementById('composer-input').focus();
      }
      if (act === 'steer' || act === 'followup') {
        const inp = el.querySelector('#steer-input');
        const text = (inp && inp.value || '').trim();
        if (!text) return;
        if (act === 'steer') { V3.actions.steer(s.id, text); V3.toast('Steer queued', 'sage'); }
        else { V3.actions.followUp(s.id, text); V3.toast('Follow-up queued', 'sage'); }
        inp.value = '';
      }
    }));
    el.querySelectorAll('[data-rule]').forEach(b => b.addEventListener('click', () => {
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      const [cat, rule] = b.dataset.rule.split(':');
      V3.actions.setApprovalRule(cat, rule);
    }));
  },

  live(what) {
    const s = this.current;
    if (!s) return;
    if (what === 'sessions' || what === 'queue' || what === 'logs:' + s.id) {
      const active = document.activeElement;
      if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA/.test(active.tagName)) return;
      const main = document.getElementById('main');
      const y = main.scrollTop;
      this.render(main, [s.id]);
      main.scrollTop = y;
    }
  }
};
