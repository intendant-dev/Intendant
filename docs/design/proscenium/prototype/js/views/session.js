/* Proscenium — the Session Space: one deep page per session.
   Timeline · Changes · Context · Controls · Vitals & lineage.
   Folds honor the disclosure contract; Studio density opens them all. */
window.P = window.P || {};
P.views = P.views || {};

P.views.session = {
  title: 'Session',

  render(el, params) {
    const id = params[0];
    const s = P.data.sessions.find(x => x.id === id) || P.data.sessions[0];
    const q = P.data.queue.filter(x => x.session === s.id && !P.queueStore.isResolved(x.id));

    el.innerHTML = P.page({
      body:
        /* header */
        '<div class="row" style="align-items:flex-start;gap:14px;flex-wrap:wrap">' +
          '<div style="min-width:280px;flex:1">' +
            '<div class="eyebrow"><a href="#/work" style="color:inherit">work</a> / ' + P.esc(s.name) + '</div>' +
            '<h1 class="page-title" style="font-size:26px">' + P.esc(s.name) + '</h1>' +
            '<div class="page-sub">' + P.esc(s.sentence) + '</div>' +
            '<div class="row" style="gap:6px;flex-wrap:wrap">' +
              P.chip(s.phase === 'working' ? 'working' : s.phase, s.phase === 'working' ? 'sage' : 'slate') +
              P.chip(s.backend, 'slate') + P.chip(s.model, 'slate') +
              P.chip(P.machineName(s.machine), s.peer ? 'violet' : null) +
              (s.queue && s.queue.length ? P.chip('needs you', 'attn', 'doorbell') : '') +
            '</div>' +
          '</div>' +
          '<div class="col" style="gap:8px;align-items:flex-end">' +
            '<div class="row" style="gap:6px">' +
              (s.phase === 'working' ? '<button class="btn btn-quiet" data-act="pause">' + P.ICON('pause', 15) + ' Pause</button>' : '') +
              '<button class="btn btn-danger" data-act="stop">' + P.ICON('stop', 15) + ' Stop</button>' +
              '<button class="btn btn-quiet" data-act="rename">Rename</button>' +
            '</div>' + P.authline('you', 'owner', 'direct') +
          '</div>' +
        '</div>' +

        /* vitals strip */
        '<div class="card"><div class="factline" style="gap:22px">' +
          '<span>' + P.fact('turn ' + s.turn) + '</span>' +
          '<span class="row" style="gap:6px">' + P.fact('context ' + s.tokens.pct + '%') + P.meter(s.tokens.pct) + '</span>' +
          '<span>' + P.fact('$' + s.cost.toFixed(2)) + '</span>' +
          '<span>' + P.fact('cache ' + s.cache.hit + '%' + (s.cache.ttl ? ' · ttl ' + s.cache.ttl : '')) + '</span>' +
          (s.limits.fiveHour != null ? '<span class="row" style="gap:6px">' + P.fact('5h ' + s.limits.fiveHour + '%') + P.meter(s.limits.fiveHour) + '</span>' : '') +
          '<span class="studio-only">' + P.fact('branch ' + s.branch + ' · ' + s.dirty + ' dirty · ' + (s.ahead || 0) + ' ahead') + '</span>' +
        '</div></div>' +

        /* its queue items */
        (q.length ? P.section('Waiting on you, here') + q.map(x => P.decisionCard(x)).join('') : '') +

        /* the folds */
        P.foldHtml({ key: 'session.timeline', title: 'Timeline', note: 'turns · tools · reasoning', open: true,
          body: this.timeline(s) }) +
        P.foldHtml({ key: 'session.changes', title: 'Changes', note: s.dirty + ' files · history & rollback',
          body: this.changes(s) }) +
        P.foldHtml({ key: 'session.context', title: 'Context', note: s.tokens.pct + '% of ' + Math.round(s.tokens.ctx / 1000) + 'k',
          body: this.context(s) }) +
        P.foldHtml({ key: 'session.controls', title: 'Controls', note: s.backend + ' knobs · approval rules',
          body: this.controls(s) }) +
        P.foldHtml({ key: 'session.vitals', title: 'Vitals & lineage', note: 'git · recordings · forks · data',
          body: this.vitals(s) })
    });

    this.wire(el, s);
  },

  timeline(s) {
    const steer = s.phase === 'working'
      ? '<div class="panel row" style="margin-bottom:12px">' + P.ICON('send', 14) +
        '<input class="input" placeholder="Steer mid-turn — it lands at the next safe point…" style="flex:1;border:0;background:none">' +
        '<button class="btn btn-quiet btn-xs" data-act="steer">steer</button></div>'
      : '';
    const log = s.id === 'fix-login' ? P.data.fixLoginLog : [
      ['10:07:00', 'sys', 'task received — ' + s.task.slice(0, 60)],
      ['10:07:12', 'tool', 'Read AGENTS.md (241 lines)'],
      ['10:09:31', 'tool', 'Grep "http://" across docs/src — 31 hits'],
      ['10:12:44', 'note', 'chapter map built; checking links in file order'],
      ['10:18:02', 'ok', '41 of 96 chapters clean so far']
    ];
    return steer + P.logLines(log) +
      '<div class="row" style="margin-top:10px;gap:8px">' +
        '<span class="dim" style="font-size:12px">Verbosity</span>' +
        '<span class="seg"><button class="on">normal</button><button>verbose</button><button>debug</button></span>' +
        '<span class="grow"></span>' +
        '<span class="fact">live · replays on reconnect</span>' +
      '</div>';
  },

  changes(s) {
    if (!s.dirty) return P.empty('check', 'A clean tree', 'This session left no uncommitted changes.');
    const files = ['src/auth/session.ts (+4 −1)', 'src/middleware/redirect.ts (+2 −1)', 'src/auth/callback.ts (+18 −6)', 'src/routes/login.ts (+9 −2)'];
    return '<div class="grid" style="grid-template-columns:200px 1fr;gap:12px">' +
      '<div class="col" style="gap:2px">' + files.map((f, i) =>
        '<div class="tree-row' + (i === 0 ? ' on' : '') + '">' + P.ICON('file', 13) + P.esc(f) + '</div>').join('') +
        '<div class="row" style="margin-top:10px;gap:6px;flex-wrap:wrap">' +
          '<button class="btn btn-quiet btn-xs" data-act="rollback">' + P.ICON('history', 13) + ' rollback…</button>' +
          '<button class="btn btn-quiet btn-xs">history</button>' +
        '</div></div>' +
      '<div>' + P.diffHtml(P.data.sampleDiff) + '</div>' +
      '</div>';
  },

  context(s) {
    const cats = [
      ['conversation', 41, 'slate'], ['code read', 26, 'sage'], ['tool output', 17, 'violet'],
      ['system + prompts', 9, 'brass'], ['free', 100 - s.tokens.pct, null]
    ];
    const bar = '<div class="row" style="gap:2px;height:22px;border-radius:6px;overflow:hidden">' +
      cats.filter(c => c[1] > 0).map(c =>
        '<span style="width:' + c[1] + '%;background:' + (c[2] ? 'rgba(var(--' + c[2] + '-rgb),.5)' : 'var(--surface-3)') + '" title="' + c[0] + ' ' + c[1] + '%"></span>').join('') + '</div>';
    const legend = '<div class="factline" style="margin-top:8px">' +
      cats.filter(c => c[2]).map(c => '<span class="fact"><span class="dot dot-' + c[2] + '"></span> ' + c[0] + ' ' + c[1] + '%</span>').join('') + '</div>';
    return bar + legend +
      '<div class="dim" style="margin:10px 0;font-size:12.5px">Largest consumers: the auth module read (turn 3), the test run output (turn 12), the system prompt. The 3D scene lives in Station — this is the quiet version.</div>' +
      P.foldHtml({ key: 'session.managed', title: 'Managed context', note: 'density · anchors · rewind · fission',
        body:
        '<div class="grid grid-2">' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Density</div>' +
            '<div class="kv"><span class="k">compaction pressure</span><span class="v">' + s.tokens.pct + '%</span>' +
            '<span class="k">anchors</span><span class="v">3 — latest at turn 9</span>' +
            '<span class="k">last compaction</span><span class="v">turn 11 · kept 62%</span></div></div>' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Rewind</div>' +
            '<div class="dim" style="font-size:12.5px;margin-bottom:8px">Roll the conversation back to an anchor; carry forward only what you name.</div>' +
            '<div class="row" style="gap:6px"><span class="chip">turn 9 · cookie fix found</span><span class="chip">turn 4 · map built</span></div>' +
            '<div class="row" style="margin-top:8px;gap:6px">' +
              '<button class="btn btn-quiet btn-xs" data-act="rewind">compose rewind…</button>' +
              '<button class="btn btn-quiet btn-xs">records & backout</button>' +
              '<button class="btn btn-quiet btn-xs">fission…</button></div></div>' +
        '</div>' });
  },

  controls(s) {
    const isCodex = s.backend === 'codex';
    const isClaude = s.backend === 'claude-code';
    let body = '';
    if (isClaude) {
      body = '<div class="kv">' +
        '<span class="k">Model</span><span class="v">claude-fable (override: off)</span>' +
        '<span class="k">Permission mode</span><span class="v">acceptEdits</span>' +
        '<span class="k">Allowed tools</span><span class="v">Read, Edit, Bash(npm test:*), Grep, Glob</span>' +
        '<span class="k">Managed context</span><span class="v">managed</span></div>';
    } else if (isCodex) {
      body = '<div class="kv">' +
        '<span class="k">Model</span><span class="v">gpt-5.2-codex · effort high</span>' +
        '<span class="k">Sandbox</span><span class="v">workspace-write · network off</span>' +
        '<span class="k">Writable roots</span><span class="v">~/projects · /tmp</span>' +
        '<span class="k">Service tier</span><span class="v">default</span></div>' +
        '<div class="row" style="margin:10px 0;gap:6px;flex-wrap:wrap">' +
        ['new', 'compact', 'fast', 'fork', 'side', 'undo', 'review', 'goal'].map(a =>
          '<button class="btn btn-quiet btn-xs" data-thread="' + a + '">/' + a + '</button>').join('') + '</div>';
    } else {
      body = '<div class="kv">' +
        '<span class="k">Execution</span><span class="v">' + s.model + '</span>' +
        '<span class="k">Sub-agents</span><span class="v">' + s.subagents.length + '</span></div>';
    }
    body += '<div class="eyebrow" style="margin:14px 0 6px">Approval rules — this session’s project</div>' +
      '<div class="col" style="gap:4px">' +
      [['Read files', 'auto'], ['Edit & write', 'ask'], ['Delete files', 'ask'], ['Run shell commands', 'ask'],
       ['Network egress', 'auto'], ['Destructive actions', 'deny'], ['Control your display', 'ask'], ['External-agent tool calls', 'ask']
      ].map(r => '<div class="row"><span style="width:190px">' + r[0] + '</span>' +
        '<span class="seg">' + ['auto', 'ask', 'deny'].map(v =>
          '<button class="' + (r[1] === v ? 'on' : '') + '" data-rule="' + r[0] + ':' + v + '">' + v + '</button>').join('') + '</span></div>').join('') +
      '</div>' +
      '<div class="dim" style="margin-top:8px;font-size:12px">Rules apply live and save to intendant.toml — “approve all like this” in the queue lands here.</div>';
    return body;
  },

  vitals(s) {
    return '<div class="grid grid-2">' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Git</div>' +
        '<div class="kv"><span class="k">branch</span><span class="v">' + s.branch + '</span>' +
        '<span class="k">working tree</span><span class="v">' + (s.dirty ? s.dirty + ' dirty files' : 'clean') + '</span>' +
        '<span class="k">ahead</span><span class="v">' + (s.ahead || 0) + ' commits</span>' +
        '<span class="k">merge parity</span><span class="v">with main · clean</span></div></div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Lineage</div>' +
        (s.subagents.length
          ? '<div class="col" style="gap:6px">' + s.subagents.map(sa =>
              '<div class="row">' + P.ICON('branch', 13) + '<b>' + P.esc(sa.name) + '</b>' + P.chip(sa.role, 'slate') + '</div>').join('') + '</div>'
          : '<div class="dim" style="font-size:12.5px">No sub-agents or forks. Fork points appear here when the backend offers them.</div>') +
        '<div class="row" style="margin-top:8px;gap:6px"><button class="btn btn-quiet btn-xs" data-act="fork">fork from anchor…</button>' +
        '<button class="btn btn-quiet btn-xs" data-act="delegate">delegate to sub-agent…</button></div></div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Recordings & frames</div>' +
        '<div class="col" style="gap:6px">' +
        (s.id === 'fix-login'
          ? '<div class="row">' + P.ICON('record', 14) + '<span>the redirect loop, captured</span><span class="fact">2:14 · 88 MB</span></div>'
          : '<div class="dim" style="font-size:12.5px">No recordings yet — the house records when you ask, or when a turn goes sideways.</div>') +
        '</div></div>' +
      '<div class="panel"><div class="eyebrow" style="margin-bottom:6px">Data</div>' +
        '<div class="row" style="gap:6px;flex-wrap:wrap">' +
          '<button class="btn btn-quiet btn-xs" data-act="report">download report (.zip)</button>' +
          '<button class="btn btn-danger btn-xs" data-act="delete">delete session data…</button>' +
        '</div>' +
        '<div class="dim" style="margin-top:8px;font-size:12px">Per-kind deletion: logs, frames, recordings — or everything. The report is the full audit trail.</div></div>' +
    '</div>';
  },

  wire(el, s) {
    el.querySelectorAll('[data-act]').forEach(b => b.addEventListener('click', () => {
      const act = b.dataset.act;
      const msgs = {
        stop: ['Interrupt sent — the turn stops at the next safe point', 'brick'],
        pause: ['Paused — the session holds its place', 'sage'],
        rename: ['Rename lives here in the real build', null],
        steer: ['Steer queued — lands at the next safe point', 'sage'],
        rollback: ['Rollback would open its confirm dialog — files and/or conversation', null],
        rewind: ['The rewind composer opens here — anchor, reason, carry-forward', null],
        fork: ['Fork from an anchor — a sibling session branches off', null],
        delegate: ['Delegate — spawn a sub-agent with its own brief and worktree', null],
        report: ['The report zip would download now', 'sage'],
        delete: ['Deletion is a typed-confirm in the real build — nothing happens here', 'brick']
      };
      const m = msgs[act] || ['Done', null];
      P.toast(m[0], m[1]);
    }));
    el.querySelectorAll('[data-thread]').forEach(b => b.addEventListener('click', () =>
      P.toast('/' + b.dataset.thread + ' sent to ' + s.name, 'sage')));
    el.querySelectorAll('[data-rule]').forEach(b => b.addEventListener('click', () => {
      const [rule, v] = b.dataset.rule.split(':');
      b.closest('.seg').querySelectorAll('button').forEach(x => x.classList.remove('on'));
      b.classList.add('on');
      P.toast(rule + ' → ' + v + ' — live, saved to intendant.toml', 'sage');
    }));
  }
};
