/* Proscenium — Work (the Stage): Live · Archive · New · Worktrees. */
window.P = window.P || {};
P.views = P.views || {};

P.views.work = {
  title: 'Work',
  lane: 'live',

  render(el, params) {
    const lane = params[0] && ['live', 'archive', 'new', 'worktrees'].includes(params[0]) ? params[0] : this.lane;
    this.lane = lane;
    const lanes = [['live', 'Live'], ['archive', 'Archive'], ['new', 'New'], ['worktrees', 'Worktrees']];
    el.innerHTML = P.page({
      eyebrow: 'the stage',
      title: 'Work',
      sub: 'Everything the house is doing, has done, or could start. One deep page per session — power unfolds where you look for it.',
      body:
        '<div class="row" style="justify-content:space-between">' +
          '<div class="seg">' + lanes.map(l =>
            '<button class="' + (l[0] === lane ? 'on' : '') + '" data-lane="' + l[0] + '">' + l[1] + '</button>').join('') + '</div>' +
          P.authline('you', 'owner', 'direct') +
        '</div>' +
        '<div id="work-lane">' + this['lane_' + lane]() + '</div>'
    });
    el.querySelectorAll('[data-lane]').forEach(b => b.addEventListener('click', () => {
      P.go('#/work/' + b.dataset.lane);
    }));
    this.wire(el, lane);
  },

  /* ---------------- Live ---------------- */
  lane_live() {
    const live = P.data.sessions.filter(s => s.phase === 'working');
    const idle = P.data.sessions.filter(s => s.phase === 'idle');
    const machines = P.data.machines.filter(m => live.some(s => s.machine === m.id));
    const pb = P.data.sessions.find(s => s.id === 'photo-book');
    return '<div class="row" style="gap:6px;flex-wrap:wrap">' +
        P.chip('all machines', 'brass') + machines.map(m => P.chip(m.petname, m.id === 'local' ? 'slate' : 'violet')).join('') +
      '</div>' +
      '<div class="grid grid-3">' + live.map(P.stageCard).join('') + '</div>' +
      (pb ? P.section('Just finished — and its sub-agents') +
        '<div class="grid grid-3">' + P.stageCard(pb) +
          pb.subagents.map(sa =>
            '<div class="card"><div class="row">' + P.dot('slate') + '<b>' + P.esc(sa.name) + '</b>' +
            '<span class="grow"></span>' + P.chip(sa.role, 'slate') + '</div>' +
            '<div class="card-sub" style="margin-top:6px">' + P.esc(sa.result) + '</div>' +
            '<div class="factline" style="margin-top:8px">' + P.fact('sub-agent of photo-book') + P.fact('merged') + '</div></div>').join('') +
        '</div>' : '') +
      (idle.length ? P.section('Idle — waiting whenever you are') +
        '<div class="grid grid-3">' + idle.map(P.stageCard).join('') + '</div>' : '');
  },

  /* ---------------- Archive ---------------- */
  lane_archive() {
    const rows = P.data.sessions.filter(s => s.phase !== 'working');
    return '<div class="row" style="gap:8px;flex-wrap:wrap">' +
        '<input class="input" id="archive-q" placeholder="Search sessions and their logs…" style="flex:1;min-width:220px">' +
        P.chip('project: all', null, 'chevdown') + P.chip('source: all', null, 'chevdown') +
        P.chip('status: all', null, 'chevdown') + P.chip('sort: recent', null, 'chevdown') +
      '</div>' +
      '<div class="card" style="padding:6px"><table class="table"><thead><tr>' +
      '<th>Session</th><th>Source</th><th>Machine</th><th>Status</th><th>Cost</th><th></th></tr></thead><tbody>' +
      rows.map(s => '<tr data-open-session="' + s.id + '" style="cursor:pointer">' +
        '<td>' + P.esc(s.name) + '<div class="dim" style="font-weight:400;font-size:12px">' + P.esc(s.sentence) + '</div></td>' +
        '<td class="mono">' + s.backend + '</td><td>' + P.machineName(s.machine) + '</td>' +
        '<td>' + P.chip(s.phase === 'done' ? 'finished' : s.phase, s.phase === 'done' ? 'sage' : s.phase === 'idle' ? 'slate' : null) + '</td>' +
        '<td class="mono">$' + s.cost.toFixed(2) + '</td>' +
        '<td>' + P.chip('open', null, 'chev') + '</td></tr>').join('') +
      '</tbody></table></div>' +
      '<div class="dim" style="font-size:12px">Deep search across full logs lives in ⌘K — type three or more characters and look under “Search deeper”.</div>';
  },

  /* ---------------- New ---------------- */
  lane_new() {
    const fueled = P.data.machines.find(m => m.id === 'local').fueled;
    return '<div class="card"><div class="card-head"><div>' +
        '<h3 class="card-title">What should the house do?</h3>' +
        '<div class="card-sub">One sentence is enough. The house asks if it needs more.</div></div></div>' +
      '<textarea class="input" id="new-task" rows="3" placeholder="e.g. Tidy my downloads folder — keep anything from this month, archive the rest by year" style="width:100%;font-size:15px"></textarea>' +
      '<div class="row" style="margin-top:10px;gap:8px;flex-wrap:wrap">' +
        (fueled ? P.chip('fueled — model credentials active', 'sage', 'fuel')
                : P.chip('no fuel yet — add API keys', 'attn', 'fuel')) +
        P.chip('on Studio Mac', 'slate', 'machines') +
      '</div>' +
      '<div class="row" style="margin-top:14px;gap:8px">' +
        '<button class="btn btn-primary" id="new-start">' + P.ICON('send', 15) + ' Start</button>' +
        '<span class="dim" style="font-size:12.5px">@ aims it at a machine · options unfold below</span>' +
      '</div></div>' +

      P.foldHtml({ key: 'new.options', title: 'Options', note: 'the full launch panel — nothing removed',
        body:
        '<div class="grid grid-2">' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Where</div>' +
            '<div class="kv"><span class="k">Project</span><span class="v">~/projects/shopify-theme</span>' +
            '<span class="k">Worktree</span><span class="v">off — work in place</span></div>' +
            '<div class="row" style="margin-top:8px"><button class="btn btn-quiet btn-xs">browse…</button>' +
            '<button class="btn btn-quiet btn-xs">run in a git worktree</button></div></div>' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Who does it</div>' +
            '<div class="col" style="gap:6px">' +
            ['Internal (orchestrate)', 'Claude Code · claude-fable', 'Codex · gpt-5.2-codex'].map((a, i) =>
              '<label class="row" style="gap:8px"><input type="radio" name="new-agent" ' + (i === 0 ? 'checked' : '') + '><span>' + a + '</span></label>').join('') +
            '</div></div>' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Execution</div>' +
            '<div class="kv"><span class="k">Mode</span><span class="v">auto — the house decides</span>' +
            '<span class="k">Sandbox</span><span class="v">workspace-write</span>' +
            '<span class="k">Approval policy</span><span class="v">medium — gate writes</span></div></div>' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Backend pins</div>' +
            '<div class="kv"><span class="k">Codex effort</span><span class="v">high</span>' +
            '<span class="k">Managed context</span><span class="v">managed</span>' +
            '<span class="k">Context replay</span><span class="v">summary</span></div>' +
            '<div class="dim" style="margin-top:8px;font-size:12px">Every pin the daemon honors — one fold deeper per backend.</div></div>' +
        '</div>' });
  },

  /* ---------------- Worktrees ---------------- */
  lane_worktrees() {
    const wts = [
      { name: 'fix/login-redirect', session: 'fix-login', dirty: 12, ahead: 3, risk: 'review', note: '12 files changed — matches the OAuth scope' },
      { name: 'docs/link-sweep', session: 'docs-sweep', dirty: 6, ahead: 1, risk: 'low', note: 'docs only, no code paths' },
      { name: 'fix/station-glow', session: 'station-polish', dirty: 0, ahead: 0, risk: 'done', note: 'merged into parent — safe to remove' },
      { name: 'spike/webgpu-fallback', session: '—', dirty: 31, ahead: 9, risk: 'stale', note: '2 weeks untouched, unmerged commits' }
    ];
    return '<div class="row" style="gap:8px;flex-wrap:wrap">' +
        '<button class="btn btn-quiet btn-xs">' + P.ICON('refresh', 13) + ' scan</button>' +
        P.chip('active', 'sage') + P.chip('dirty', null) + P.chip('unmerged', null) + P.chip('risk first', 'attn') +
      '</div>' +
      '<div class="col" style="gap:10px">' + wts.map(w =>
        '<div class="card"><div class="row">' + P.ICON('branch', 15) + '<b class="mono">' + w.name + '</b>' +
          '<span class="grow"></span>' +
          P.chip(w.risk === 'review' ? 'worth a look' : w.risk === 'low' ? 'low risk' : w.risk === 'done' ? 'merged' : 'stale',
                 w.risk === 'low' || w.risk === 'done' ? 'sage' : w.risk === 'review' ? 'attn' : 'brick') +
        '</div>' +
        '<div class="card-sub" style="margin-top:6px">' + P.esc(w.note) + '</div>' +
        '<div class="factline" style="margin-top:8px">' + P.fact(w.dirty + ' dirty') + P.fact(w.ahead + ' ahead') +
          P.fact('session: ' + w.session) + '</div>' +
        '<div class="row" style="margin-top:10px;gap:6px">' +
          '<button class="btn btn-quiet btn-xs">inspect</button>' +
          '<button class="btn btn-quiet btn-xs">open shell</button>' +
          '<button class="btn btn-quiet btn-xs">open files</button>' +
          (w.risk === 'done' ? '<button class="btn btn-danger btn-xs">remove</button>' : '') +
        '</div></div>').join('') + '</div>';
  },

  wire(el, lane) {
    if (lane === 'archive') {
      el.querySelectorAll('[data-open-session]').forEach(r =>
        r.addEventListener('click', () => P.go('#/work/session/' + r.dataset.openSession)));
    }
    if (lane === 'new') {
      const start = el.querySelector('#new-start');
      const ta = el.querySelector('#new-task');
      start.addEventListener('click', () => {
        const text = (ta.value || '').trim() || 'Tidy my downloads folder';
        P.toast('The house takes it on', 'sage');
        P.go('#/home');
        setTimeout(() => P.views.home.onSend(text), 300);
      });
    }
  }
};
