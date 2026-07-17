/* Proscenium — Studio: the machinery, raw.
   Workbench (lanes, observer screen, self-test) → raw state (snapshot +
   live event stream) → reference (keyboard, MCP tools) → the component
   field guide: every shared component, every state, labeled.
   Every timer is cleared at the top of render and self-clears when its
   node leaves the DOM. */
window.P = window.P || {};
P.views = P.views || {};

P.views.studio = {
  title: 'Studio',
  obsUp: false,

  render(el) {
    /* timer hygiene — re-render starts clean */
    if (this._evt) { clearInterval(this._evt); this._evt = null; }
    if (this._diag) { this._diag.forEach(clearTimeout); this._diag = null; }

    const snapshot = {
      sessions: P.data.sessions.length,
      queue: P.data.queue.length,
      machines: P.data.machines.length,
      you: P.data.you.name + ' · ' + P.data.you.role + ' · ' + P.data.you.route
    };

    el.innerHTML = P.page({
      eyebrow: 'vantage · the machinery, raw',
      title: 'Studio',
      sub: 'Everything the friendly rooms polish away: the lanes, the snapshot, the event stream — and the component contract itself, made visible.',
      body:
        /* ---------- Workbench ---------- */
        P.section('Workbench') +
        '<div class="grid grid-2">' +
          P.card({
            title: 'Transport lanes', sub: 'three lanes, one control plane — each labeled by what it carries',
            body: '<div class="col" style="gap:12px">' +
              this.lane('HTTP', '/api', '12 ms', 'JSON routes — sessions, files, people, the ledgers') +
              this.lane('WebSocket', '/ws', '8 ms · 2 clients', 'the event stream — vitals, queue, presence') +
              this.lane('WebRTC', 'tunnel', '31 ms · DTLS', 'display frames & input — the Screens room rides this') +
            '</div>'
          }) +
          P.card({
            title: 'Observer debug screen', sub: 'a throwaway virtual display the pipeline can capture while you watch',
            body:
              '<div class="row" id="std-obs-row">' + this.obsChip() +
                '<span class="grow"></span>' +
                '<button class="btn btn-quiet btn-xs" id="std-obs-up"' + (this.obsUp ? ' disabled' : '') + '>set up</button>' +
                '<button class="btn btn-quiet btn-xs" id="std-obs-down"' + (this.obsUp ? '' : ' disabled') + '>tear down</button>' +
              '</div>' +
              '<div class="dim" style="font-size:12px;margin-top:10px">Xvfb :99 · 1280×720 · captured by the tile encoder — the observer sees exactly what the agent would.</div>'
          }) +
        '</div>' +
        P.card({
          title: 'Diagnostics', sub: 'the self-test walks every trust and transport assertion, in order',
          body:
            '<div class="row">' +
              '<button class="btn btn-primary" id="std-diag">Run self-test</button>' +
              '<span class="fact" id="std-diag-status">idle — last run clean, 9 checks</span>' +
            '</div>' +
            '<div class="log panel" id="std-diag-log" style="margin-top:12px;min-height:22px"></div>'
        }) +

        /* ---------- Raw state ---------- */
        P.foldHtml({ key: 'studio.raw', title: 'Raw state', note: 'the snapshot · live event stream', open: true,
          body:
          '<div class="eyebrow" style="margin-bottom:6px">StatusSnapshot (trimmed)</div>' +
          '<div class="panel mono std-json">' + P.esc(JSON.stringify(snapshot, null, 2)) + '</div>' +
          '<div class="row" style="margin:16px 0 6px">' +
            '<span class="eyebrow">AppEvent stream</span>' +
            '<span class="fact">live · mock fan-out</span>' +
            '<span class="grow"></span>' +
            '<button class="btn btn-quiet btn-xs" id="std-evt-pause">pause</button>' +
          '</div>' +
          '<div class="log panel std-events" id="std-events"></div>' }) +

        /* ---------- Reference ---------- */
        P.section('Reference') +
        '<div class="grid grid-2">' +
          P.card({
            title: 'The keyboard', sub: 'the same map the ? overlay shows',
            body: '<div class="kv">' + this.KEYS.map(r =>
              '<span class="k mono">' + P.esc(r[0]) + '</span><span class="v" style="font-family:var(--sans)">' + P.esc(r[1]) + '</span>').join('') + '</div>'
          }) +
          P.card({
            title: 'MCP tools', sub: 'the same tools power presence — voice and text call these',
            body: '<div class="col" style="gap:5px">' + this.TOOLS.map(t =>
              '<div class="row"><span class="mono" style="width:170px;font-size:12px;flex:none">' + P.esc(t[0]) + '</span>' +
              '<span class="dim" style="font-size:12.5px">' + P.esc(t[1]) + '</span></div>').join('') + '</div>'
          }) +
        '</div>' +

        /* ---------- The component field guide ---------- */
        P.section('The component field guide — every state, labeled') +
        '<div class="dim" style="font-size:12.5px;margin-top:-6px">The contract made visible. Specimens marked inert don’t act; everything else behaves.</div>' +
        '<div class="std-guide">' + this.guide() + '</div>'
    });

    this.wire(el);
    this.startEvents(el);
  },

  lane(name, path, latency, carries) {
    return '<div class="row">' + P.dot('sage', true) +
      '<span class="mono" style="width:150px;flex:none">' + P.esc(name) + ' <span class="dim">' + P.esc(path) + '</span></span>' +
      P.fact(latency) +
      '<span class="dim" style="font-size:12.5px">' + P.esc(carries) + '</span></div>';
  },

  obsChip() {
    return this.obsUp ? P.chip('up · streaming', 'sage') : P.chip('down', 'slate');
  },

  KEYS: [
    ['⌘K or /', 'the index — everything, one box'], ['?', 'the keyboard map'],
    ['g h / w / s / f / m / p / b', 'go: home · work · screens · files · machines · people · books'],
    ['g ,  ·  g t  ·  g u', 'settings · station · studio'],
    ['y s a n', 'queue: approve · skip · approve-all · deny'],
    ['j k', 'move through the queue'], ['x', 'dismiss an FYI'],
    ['e', 'unfold details'], ['⌘↵', 'send'], ['esc', 'close the top layer']
  ],

  TOOLS: [
    ['start_task', 'give the house work'],
    ['approve', 'allow a queued action'],
    ['deny', 'refuse it — nothing was touched'],
    ['take_screenshot', 'see a display, one frame'],
    ['list_peers', 'who is federated'],
    ['send_input', 'type or click on a display'],
    ['rewind_context', 'compose a rewind to an anchor'],
    ['agenda_op', 'add, complete, reopen on the List'],
    ['memory_propose', 'offer a claim to the house'],
    ['spawn_live_audio', 'open a live voice session'],
    ['get_status', 'the trimmed snapshot'],
    ['list_sessions', 'what is on stage']
  ],

  /* ---------------- event stream ---------------- */
  startEvents(el) {
    const kinds = [
      () => 'SessionVitals fix-login cache_ttl=4m12s ctx=82%',
      () => 'QueueDepth pending=' + P.queueStore.pending().length,
      () => 'PeerHeartbeat Workshop rtt=88ms route=fleet',
      () => 'DisplayFrames disp-1 fps=5 tiles=3',
      () => 'LeaseTick Workshop fuel=2d',
      () => 'GatewayConn tabs=2 voice=1',
      () => 'CacheStats claude hit=94%',
      () => 'SchedulerTick nightly-backup next=02:00',
      () => 'SessionVitals docs-sweep ctx=35% turn=8',
      () => 'VaultState sealed entries=4'
    ];
    let i = 0;
    const stamp = () => {
      const d = new Date();
      const p = n => String(n).padStart(2, '0');
      return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
    };
    const push = () => {
      const log = document.getElementById('std-events');
      if (!log) { clearInterval(this._evt); this._evt = null; return; }
      const line = P.h('div', null,
        '<span class="lt">' + stamp() + '</span> ' + P.esc(kinds[i++ % kinds.length]()));
      log.appendChild(line);
      while (log.children.length > 28) log.removeChild(log.firstChild);
      log.scrollTop = log.scrollHeight;
    };
    for (let k = 0; k < 6; k++) push();
    this._evt = setInterval(push, 1600);
  },

  /* ---------------- field guide ---------------- */
  guide() {
    const spec = (label, html, wide) =>
      '<div class="std-spec' + (wide ? ' std-wide' : '') + '"><div class="std-spec-label">' + P.esc(label) + '</div>' + html + '</div>';

    const demoQ = {
      id: 'demo-approve', kind: 'approval', sev: 'attention', when: 'now',
      category: 'file_delete', session: 'demo', machine: 'local', backend: 'Claude Code',
      title: 'Claude wants to delete 3 files',
      consequence: 'A specimen of the decision card — the real ones live in the Queue. These buttons are inert.',
      details: { command: 'rm exports/old.csv', paths: ['exports/old.csv'], rule: 'Always allowing would set file_delete → Auto.', raw: '{ "category": "file_delete", "specimen": true }' },
      actions: [
        { id: 'approve', label: 'Allow once', kind: 'safe', key: 'y' },
        { id: 'always', label: 'Always allow here', kind: 'quiet', key: 'a' },
        { id: 'deny', label: 'Deny', kind: 'danger', key: 'n', default: true }
      ]
    };
    const demoStage = {
      id: 'demo-stage', name: 'demo-working', backend: 'claude-code', model: 'claude-fable',
      machine: 'local', phase: 'working', turn: 5,
      sentence: 'A specimen stage card — working, with its facts and meter',
      tokens: { used: 82000, ctx: 200000, pct: 41 }, cost: 0.42, queue: []
    };
    const demoStageAttn = {
      id: 'demo-stage-attn', name: 'demo-needs-you', backend: 'codex', model: 'gpt-5.2-codex',
      machine: 'dell', phase: 'working', turn: 2, peer: true,
      sentence: 'The same card, attention tone — the queue chip glows',
      tokens: { used: 40000, ctx: 272000, pct: 15 }, cost: 0.11, queue: ['demo']
    };

    return [
      spec('chips — every kind', '<div class="row" style="flex-wrap:wrap">' +
        P.chip('default') + P.chip('sage', 'sage') + P.chip('slate', 'slate') +
        P.chip('brass', 'brass') + P.chip('attn', 'attn') + P.chip('brick', 'brick') +
        P.chip('violet', 'violet') + P.chip('with icon', 'attn', 'doorbell') + '</div>'),

      spec('dots — always with a word', '<div class="factline">' +
        '<span class="fact">' + P.dot('sage') + ' sage</span>' +
        '<span class="fact">' + P.dot('sage', true) + ' pulsing</span>' +
        '<span class="fact">' + P.dot('attn') + ' attn</span>' +
        '<span class="fact">' + P.dot('brick') + ' brick</span>' +
        '<span class="fact">' + P.dot('slate') + ' slate</span></div>'),

      spec('facts & meters', '<div class="col" style="gap:8px">' +
        '<div>' + P.fact('12 ms · the instrument register') + '</div>' +
        '<div class="row">' + P.fact('34%') + P.meter(34) + '</div>' +
        '<div class="row">' + P.fact('74% warn') + P.meter(74) + '</div>' +
        '<div class="row">' + P.fact('92% hot') + P.meter(92) + '</div></div>'),

      spec('buttons', '<div class="row" style="flex-wrap:wrap">' +
        '<button class="btn">default</button>' +
        '<button class="btn btn-primary">primary</button>' +
        '<button class="btn btn-safe">safe</button>' +
        '<button class="btn btn-danger">danger</button>' +
        '<button class="btn btn-quiet">quiet</button>' +
        '<button class="btn btn-xs">xs</button>' +
        '<button class="btn" disabled>disabled</button>' +
        '<button class="icon-btn">' + P.ICON('sparkle', 15) + '</button></div>'),

      spec('segmented control', '<span class="seg"><button class="on">auto</button><button>ask</button><button>deny</button></span>' +
        '<div class="dim" style="font-size:12px;margin-top:6px">approval rules use these — live-applied</div>'),

      spec('badge & kbd hint', '<div class="row">' +
        '<span class="badge-count">4</span><span class="dim" style="font-size:12.5px">unread queue depth</span>' +
        '<span class="grow"></span><span class="kbd-hint">⌘K</span><span class="kbd-hint">y</span></div>'),

      spec('fold — closed & open', '<div class="col" style="gap:8px">' +
        P.foldHtml({ title: 'A closed fold', note: 'remembers state', open: false, body: '<div class="dim" style="font-size:13px">Power unfolds where you look for it.</div>' }) +
        P.foldHtml({ title: 'An open fold', note: 'studio opens by default', open: true, body: '<div class="dim" style="font-size:13px">Same contract, opened.</div>' }) +
        '</div>'),

      spec('decision card — specimen, inert',
        '<div class="std-specimen" id="std-specimen">' + P.decisionCard(demoQ) + '</div>' +
        '<div class="dim" style="font-size:12px;margin-top:6px">Wrapped so its buttons toast instead of resolving.</div>', true),

      spec('stage cards — working & needs-you', '<div class="grid grid-2">' +
        P.stageCard(demoStage) + P.stageCard(demoStageAttn) + '</div>', true),

      spec('empty state', P.empty('stage', 'Nothing on stage', 'Every room ships its empty state — a serif line and one honest next action.')),

      spec('skeleton — loading, never a lone spinner', '<div class="col" style="gap:8px">' +
        P.skeleton(14, '72%') + P.skeleton(14, '100%') + P.skeleton(14, '45%') + '</div>'),

      spec('diff block', P.diffHtml(P.data.sampleDiff), true),

      spec('kv grid', '<div class="kv">' +
        '<span class="k">branch</span><span class="v">fix/login-redirect</span>' +
        '<span class="k">working tree</span><span class="v">12 dirty files</span>' +
        '<span class="k">cache</span><span class="v">94% · ttl 4m 12s</span></div>'),

      spec('panel — the sunken well', '<div class="panel mono" style="font-size:12px">$ intendant ctl sessions --working</div>'),

      spec('table', '<table class="table"><thead><tr><th>Session</th><th>Cost</th><th>State</th></tr></thead><tbody>' +
        '<tr><td>fix-login</td><td class="mono">$0.83</td><td>' + P.chip('working', 'sage') + '</td></tr>' +
        '<tr><td>docs-sweep</td><td class="mono">$0.31</td><td>' + P.chip('working', 'sage') + '</td></tr>' +
        '<tr><td>q2-invoices</td><td class="mono">$1.12</td><td>' + P.chip('idle', 'slate') + '</td></tr>' +
        '</tbody></table>', true),

      spec('log lines', P.logLines(P.data.fixLoginLog.slice(0, 5))),

      spec('tree rows', '<div class="tree">' +
        '<div class="tree-row on">' + P.ICON('folder', 13) + ' ~/projects/shopify-theme/</div>' +
        '<div class="tree-row">' + P.ICON('file', 13) + ' exports/</div>' +
        '<div class="tree-row">' + P.ICON('file', 13) + ' src/auth/</div></div>'),

      spec('authline — who acted, by what route', '<div class="col" style="gap:6px">' +
        P.authline('you', 'owner', 'direct') +
        P.authline('Workshop', 'operator', 'fleet name') +
        P.authline('a hosted tab', 'role:none', 'hosted') + '</div>'),

      spec('toast', '<button class="btn btn-quiet" id="std-toast-demo">fire a toast</button>' +
        '<span class="dim" style="font-size:12px;margin-left:8px">bottom-right · 3.2 s · sage or brick</span>'),

      spec('today ribbon', '<div class="ribbon">' +
        P.data.today.map(t =>
          '<div class="ribbon-item' + (t.kind === 'now' ? ' now' : '') + '">' +
          '<span class="t">' + P.esc(t.t) + '</span><span>' + P.esc(t.label) + '</span></div>').join('') + '</div>', true),

      spec('queue-free — the resolved queue', '<div class="queue-free"><span class="big">You’re free.</span>Nothing needs you. The house will tap you the moment something does.</div>', true)
    ].join('');
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    /* observer debug screen */
    const obsUp = el.querySelector('#std-obs-up'), obsDown = el.querySelector('#std-obs-down');
    const obsPaint = () => {
      el.querySelector('#std-obs-row').querySelector('.chip').outerHTML = this.obsChip();
      obsUp.disabled = this.obsUp; obsDown.disabled = !this.obsUp;
    };
    obsUp.addEventListener('click', () => { this.obsUp = true; obsPaint(); P.toast('Observer screen up — Xvfb :99 feeding the tile encoder', 'sage'); });
    obsDown.addEventListener('click', () => { this.obsUp = false; obsPaint(); P.toast('Observer screen torn down', null); });

    /* diagnostics self-test */
    el.querySelector('#std-diag').addEventListener('click', () => this.runDiag(el));

    /* event stream pause */
    const pauseBtn = el.querySelector('#std-evt-pause');
    pauseBtn.addEventListener('click', () => {
      if (this._evt) { clearInterval(this._evt); this._evt = null; pauseBtn.textContent = 'resume'; }
      else { this.startEvents(el); pauseBtn.textContent = 'pause'; }
    });

    /* field guide: toast demo */
    const td = el.querySelector('#std-toast-demo');
    if (td) td.addEventListener('click', () => P.toast('The toast — brief, warm, and gone', 'sage'));

    /* field guide: decision specimen stays inert */
    const specimen = el.querySelector('#std-specimen');
    if (specimen) specimen.addEventListener('click', e => {
      if (e.target.closest('[data-q][data-action]')) {
        e.stopPropagation();
        P.toast('A specimen — its buttons are inert. The real ones live in the Queue.', null);
      }
    });
  },

  runDiag(el) {
    if (this._diag) { this._diag.forEach(clearTimeout); this._diag = null; }
    const log = el.querySelector('#std-diag-log');
    const status = el.querySelector('#std-diag-status');
    log.innerHTML = '';
    status.textContent = 'running…';
    const steps = [
      'route direct mTLS ✓ 12 ms',
      'route fleet name (Workshop) ✓ 88 ms',
      'tunnel ICE/DTLS ✓ 204 ms',
      'vault sealed & reachable ✓',
      'IAM model compile ✓ 14 ops',
      'encoder pool vp8 ✓ warm',
      'hosted provenance pin ✓ role:none',
      'event bus fan-out ✓ 2 clients',
      'scheduler tick ✓ next 02:00'
    ];
    this._diag = steps.map((s, i) => setTimeout(() => {
      const target = document.getElementById('std-diag-log');
      if (!target) return;
      target.appendChild(P.h('div', null,
        '<span class="lt">check ' + (i + 1) + '/9</span> <span class="ok">' + P.esc(s) + '</span>'));
      if (i === steps.length - 1) {
        const st = document.getElementById('std-diag-status');
        if (st) st.textContent = 'clean — 9/9 · ' + new Date().toLocaleTimeString();
        P.toast('Self-test clean — 9 of 9', 'sage');
      }
    }, 320 * (i + 1)));
  }
};
