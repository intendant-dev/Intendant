/* V3 — Work (the Stage): Live · Archive · New · Worktrees. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.work = {
  title: 'Work',
  lane: 'live',
  worktrees: null,

  render(el, params) {
    const lane = params[0] && ['live', 'archive', 'new', 'worktrees'].includes(params[0]) ? params[0] : this.lane;
    this.lane = lane;
    const lanes = [['live', 'Live'], ['archive', 'Archive'], ['new', 'New'], ['worktrees', 'Worktrees']];
    el.innerHTML = V3.page({
      eyebrow: 'the stage',
      title: 'Work',
      sub: 'Everything the house is doing, has done, or could start. One deep page per session — power unfolds where you look for it.',
      body:
        '<div class="row" style="justify-content:space-between">' +
          '<div class="seg">' + lanes.map(l =>
            '<button class="' + (l[0] === lane ? 'on' : '') + '" data-lane="' + l[0] + '">' + l[1] + '</button>').join('') + '</div>' +
          V3.authline(V3.data.you.name, V3.data.you.role, V3.data.you.route) +
        '</div>' +
        '<div id="work-lane">' + this['lane_' + lane]() + '</div>'
    });
    el.querySelectorAll('[data-lane]').forEach(b => b.addEventListener('click', () => V3.go('#/work/' + b.dataset.lane)));
    this.wire(el, lane);
  },

  lane_live() {
    const S = V3.data.sessions;
    const live = S.filter(s => s.active);
    const done = S.filter(s => !s.active && ['done', 'failed'].includes(s.phase)).slice(0, 3);
    const idle = S.filter(s => !s.active && s.phase === 'idle').slice(0, 3);
    return '<div class="grid grid-3">' +
        (live.map(V3.stageCard).join('') || V3.empty('stage', 'Nothing on stage', 'Give the house something to do — the composer below, or Work → New.')) +
      '</div>' +
      (done.length ? V3.section('Just finished') + '<div class="grid grid-3">' + done.map(V3.stageCard).join('') + '</div>' : '') +
      (idle.length ? V3.section('Idle — waiting whenever you are') + '<div class="grid grid-3">' + idle.map(V3.stageCard).join('') + '</div>' : '');
  },

  lane_archive() {
    const rows = V3.data.sessions;
    if (!rows.length) return V3.empty('stage', 'Nothing finished yet', 'The archive fills in as the house works.');
    return '<div class="row" style="gap:8px;flex-wrap:wrap">' +
        '<input class="input" id="archive-q" placeholder="Filter sessions…" style="flex:1;min-width:220px">' +
        '<span class="fact">' + rows.length + ' sessions</span>' +
      '</div>' +
      '<div class="card" style="padding:6px"><table class="table"><thead><tr>' +
      '<th>Session</th><th>Source</th><th>Status</th><th>Turns</th><th>Cost</th><th></th></tr></thead><tbody id="archive-rows">' +
      rows.map(s => '<tr data-open-session="' + s.id + '" data-search="' + V3.esc((s.name + ' ' + s.task).toLowerCase()) + '" style="cursor:pointer">' +
        '<td>' + V3.esc(s.name) + '<div class="dim" style="font-weight:400;font-size:12px">' + V3.esc((s.task || '').slice(0, 70)) + '</div></td>' +
        '<td class="mono">' + V3.esc(s.backend) + '</td>' +
        '<td>' + V3.chip(s.phase, s.phase === 'done' ? 'sage' : s.phase === 'failed' ? 'brick' : s.active ? 'sage' : 'slate') + '</td>' +
        '<td class="mono">' + (s.turn || 0) + '</td>' +
        '<td class="mono">' + (s.cost ? '$' + s.cost.toFixed(2) : '—') + '</td>' +
        '<td>' + V3.chip('open', null, 'chev') + '</td></tr>').join('') +
      '</tbody></table></div>';
  },

  lane_new() {
    const fuel = V3.data.fuel || {};
    const fueled = fuel.openai || fuel.anthropic || fuel.gemini;
    const agents = V3.data.externalAgents || [];
    const agentOpts = [['', 'Internal (the house itself)']].concat(
      agents.map(a => [a.id, a.label + (a.installed === false ? ' — not installed' : '')]));
    return '<div class="card"><div class="card-head"><div>' +
        '<h3 class="card-title">What should the house do?</h3>' +
        '<div class="card-sub">One sentence is enough. The house asks if it needs more.</div></div></div>' +
      '<textarea class="input" id="new-task" rows="3" placeholder="e.g. Tidy my downloads folder — keep anything from this month, archive the rest by year" style="width:100%;font-size:15px"></textarea>' +
      '<div class="row" style="margin-top:10px;gap:8px;flex-wrap:wrap">' +
        (fueled ? V3.chip('fueled — model credentials active', 'sage', 'fuel')
                : V3.chip('no fuel yet — add API keys in Settings', 'attn', 'fuel')) +
      '</div>' +
      '<div class="row" style="margin-top:14px;gap:8px;align-items:center">' +
        '<button class="btn btn-primary" id="new-start">' + V3.ICON('send', 15) + ' Start</button>' +
        '<label class="row" style="gap:6px"><input type="checkbox" id="new-direct"> <span class="dim" style="font-size:12.5px">direct — skip the orchestrator</span></label>' +
      '</div></div>' +
      V3.foldHtml({ key: 'new.options', title: 'Options', note: 'the full launch panel — nothing removed',
        body:
        '<div class="grid grid-2">' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Who does it</div>' +
            '<div class="col" style="gap:6px">' +
            agentOpts.map(([v, label], i) =>
              '<label class="row" style="gap:8px"><input type="radio" name="new-agent" value="' + v + '" ' + (i === 0 ? 'checked' : '') + '><span>' + V3.esc(label) + '</span></label>').join('') +
            '</div></div>' +
          '<div class="panel"><div class="eyebrow" style="margin-bottom:8px">Where</div>' +
            '<div class="kv"><span class="k">Project</span><span class="v">' + V3.esc(V3.data.sessions[0] && V3.data.sessions[0].cwd || 'the daemon’s project') + '</span>' +
            '<span class="k">Worktree</span><span class="v">off — work in place</span></div></div>' +
        '</div>' });
  },

  lane_worktrees() {
    if (!this.worktrees) {
      V3.transport.get('/api/worktrees').then(r => {
        this.worktrees = r.worktrees || r || [];
        if (this.lane === 'worktrees') V3.rerender();
      }).catch(() => { this.worktrees = []; });
      return '<div class="col" style="gap:8px">' + V3.skeleton(18) + V3.skeleton(18, '70%') + V3.skeleton(18, '45%') + '</div>';
    }
    if (!this.worktrees.length) return V3.empty('branch', 'No worktrees', 'Sessions that ask for an isolated checkout appear here.');
    return '<div class="col" style="gap:10px">' + this.worktrees.map(w =>
      '<div class="card"><div class="row">' + V3.ICON('branch', 15) + '<b class="mono">' + V3.esc(w.branch || w.name || w.path || '?') + '</b>' +
        '<span class="grow"></span>' +
        (w.dirty ? V3.chip(w.dirty + ' dirty', 'attn') : V3.chip('clean', 'sage')) +
      '</div>' +
      '<div class="factline" style="margin-top:8px">' +
        (w.session_id ? V3.fact('session: ' + w.session_id) : '') +
        (w.ahead != null ? V3.fact(w.ahead + ' ahead') : '') +
        (w.path ? V3.fact(w.path) : '') +
      '</div></div>').join('') + '</div>';
  },

  wire(el, lane) {
    if (lane === 'archive') {
      el.querySelectorAll('[data-open-session]').forEach(r =>
        r.addEventListener('click', () => V3.go('#/work/session/' + r.dataset.openSession)));
      const q = el.querySelector('#archive-q');
      if (q) q.addEventListener('input', () => {
        const needle = q.value.toLowerCase();
        el.querySelectorAll('#archive-rows tr').forEach(r => {
          r.style.display = !needle || r.dataset.search.includes(needle) ? '' : 'none';
        });
      });
    }
    if (lane === 'new') {
      el.querySelector('#new-start').addEventListener('click', () => {
        const text = (el.querySelector('#new-task').value || '').trim();
        if (!text) { V3.toast('Give the house a sentence first', 'brick'); return; }
        const agent = (el.querySelector('input[name="new-agent"]:checked') || {}).value || null;
        const direct = el.querySelector('#new-direct').checked;
        const msg = { action: 'create_session', task: text, follow_up_id: 'v3-' + Date.now().toString(36) };
        if (agent) msg.agent = agent;
        if (direct) msg.direct = true;
        V3.transport.send(msg);
        V3.data.conversation.push({ from: 'you', text, at: V3.now(), ts: Date.now() });
        V3.toast('The house takes it on', 'sage');
        V3.go('#/home');
      });
      const ta = el.querySelector('#new-task');
      if (ta) ta.focus();
    }
  },

  live(what) {
    if (what === 'sessions' && V3.current === 'work' && (this.lane === 'live' || this.lane === 'archive')) {
      const main = document.getElementById('main');
      const lane = main.querySelector('#work-lane');
      if (lane) lane.innerHTML = this['lane_' + this.lane]();
      this.wire(main, this.lane);
    }
  }
};
