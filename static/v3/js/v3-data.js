/* V3 — data: the normalized store + every intent.
   Views consume V3.data.* (never raw wire shapes); mutations go through
   V3.actions.* (never a fetch from a view). One control plane — this file
   is a renderer and an intent emitter, never a second writer.

   Queue model keys on (kind, session, id), following the SPA's proven
   attention-center pattern; resolution is optimistic with server events
   (approval_resolved / display_request_resolved / poll refresh) as truth. */
window.V3 = window.V3 || {};

V3.data = {
  config: null,
  you: { name: 'you', role: 'owner', route: 'direct' },
  connFact: 'live',
  sessions: [],            // normalized session rows (live first)
  queue: [],               // normalized attention items
  conversation: [],        // {from:'presence'|'you'|'milestone', ...}
  logs: {},                // session_id → rolling log lines [{t, kind, text}]
  machines: [],            // this daemon first, then peers
  agenda: [], agendaCounts: { open: 0, done: 0, retired: 0 }, reminderPolicy: null,
  settings: null,
  people: [], grants: [], accessOverview: null,
  usage: {},               // session_id → ModelUsageSnapshot (+ 'main')
  fuel: null,              // {openai, anthropic, gemini} → all false = dry
  externalAgents: [],
  displays: [],
  composerTarget: null,    // session id the composer is aimed at (null = new work)
  bootError: null
};

/* ============================== normalization ============================== */

V3.norm = {
  phase(status) {
    return { running: 'working', in_progress: 'working', idle: 'idle', resident: 'idle', completed: 'done',
             failed: 'failed', abandoned: 'abandoned', interrupted: 'interrupted' }[status] || status || 'idle';
  },
  sessionName(row) {
    if (row.name) return row.name;
    if (row.task) {
      const words = String(row.task).replace(/[^\w\s-]/g, '').trim().split(/\s+/).slice(0, 3).join('-').toLowerCase();
      return words || String(row.session_id || '').slice(0, 8);
    }
    return String(row.session_id || '').slice(0, 8);
  },
  session(row) {
    const id = row.session_id;
    const existing = V3.data.sessions.find(s => s.id === id) || {};
    return Object.assign(existing, {
      id,
      name: V3.norm.sessionName(row),
      backend: row.backend_source || row.source || 'intendant',
      model: row.model || row.provider || '',
      phase: V3.norm.phase(row.status),
      active: row.status === 'running',
      turn: row.turns || 0,
      cost: row.estimated_cost || 0,
      costKnown: row.pricing_known !== false,
      tokens: Object.assign(existing.tokens || {}, { used: row.total_tokens || 0 }),
      task: row.task || '',
      cwd: row.project_root || row.cwd || '',
      worktree: row.worktree || null,
      recordings: row.recordings || 0,
      canResume: row.can_resume !== false,
      relationships: row.relationships || [],
      machine: 'local',
      updatedAt: row.updated_at || null,
      sentence: existing.sentence || (row.status === 'running' ? 'Working…' : V3.norm.phase(row.status))
    });
  },
  ago(ts) {
    const s = Math.max(0, (Date.now() - ts) / 1000);
    if (s < 60) return 'just now';
    if (s < 3600) return Math.round(s / 60) + ' min ago';
    if (s < 86400) return Math.round(s / 3600) + ' h ago';
    return Math.round(s / 86400) + ' d ago';
  },
  commandTitle(command, actor) {
    const c = String(command || '').trim();
    const who = actor || 'The house';
    /* tool display names arrive as e.g. "editFile: /path" or "execAsAgent: …" */
    const tool = c.match(/^([A-Za-z_]+)\s*:/)?.[1] || '';
    if (/^edit_?file$/i.test(tool) || /^editFile$/.test(tool)) return who + ' wants to edit a file';
    if (/^write/i.test(tool)) return who + ' wants to write a file';
    if (/^delete/i.test(tool)) return who + ' wants to delete a file';
    if (/^exec(AsAgent|_command)?$/i.test(tool) || /^execAsAgent$/.test(tool)) {
      const rest = c.slice(c.indexOf(':') + 1).trim();
      return who + ' wants to run “' + (rest.split(/\s+/)[0] || 'a command') + '”';
    }
    const first = c.split(/\s+/)[0] || 'run';
    const verbs = { rm: 'delete files', mv: 'move files', cp: 'copy files', mkdir: 'create folders',
                    git: 'run git', npm: 'run npm', cargo: 'run cargo', curl: 'reach the network',
                    sudo: 'run with elevated rights', python3: 'run a script', python: 'run a script' };
    const what = verbs[first] || 'run “' + first + '”';
    return who + ' wants to ' + what;
  }
};

/* ============================== the queue ============================== */

V3.queueStore = {
  pending() { return V3.data.queue.filter(q => q.kind !== 'fyi'); },
  fyis() { return V3.data.queue.filter(q => q.kind === 'fyi'); },
  isResolved(id) { return !V3.data.queue.find(q => q.id === id); },
  resolve(id, action) {
    const q = V3.data.queue.find(x => x.id === id);
    if (q) V3.actions.resolveQueueItem(q, action);
  },
  upsert(item) {
    const i = V3.data.queue.findIndex(q => q.id === item.id);
    if (i >= 0) V3.data.queue[i] = Object.assign(V3.data.queue[i], item);
    else V3.data.queue.unshift(item);
    V3.bus.emit('queue');
  },
  remove(id) {
    const i = V3.data.queue.findIndex(q => q.id === id);
    if (i >= 0) { V3.data.queue.splice(i, 1); V3.bus.emit('queue'); }
  },
  removeRef(pred) {
    const before = V3.data.queue.length;
    V3.data.queue = V3.data.queue.filter(q => !pred(q));
    if (V3.data.queue.length !== before) V3.bus.emit('queue');
  }
};

V3.queueDerive = {
  approval(msg) {
    const s = V3.data.sessions.find(x => x.id === msg.session_id);
    const actor = s ? (s.backend === 'claude-code' ? 'Claude' : s.backend === 'codex' ? 'Codex' : 'The house') : 'The house';
    const cmd = String(msg.command || '');
    V3.queueStore.upsert({
      id: 'approval-' + (msg.session_id || 'main') + '-' + msg.id,
      kind: 'approval', sev: 'attention', ts: Date.now(), when: 'just now',
      category: msg.category || 'command', session: msg.session_id, machine: 'local',
      title: V3.norm.commandTitle(cmd, actor),
      consequence: 'In ' + (s && s.cwd || 'the project') + ' — approve once is the safe default; “always” writes a rule.',
      details: { command: cmd, paths: [], rule: '“Always” sets this category → Auto for the project.', raw: JSON.stringify(msg) },
      actions: [
        { id: 'approve', label: 'Allow once', kind: 'safe', key: 'y' },
        { id: 'always', label: 'Always allow', kind: 'quiet', key: 'a' },
        { id: 'skip', label: 'Skip', kind: 'quiet', key: 's' },
        { id: 'deny', label: 'Deny', kind: 'danger', key: 'n' }
      ],
      ref: { id: msg.id, sessionId: msg.session_id }
    });
  },
  question(msg) {
    const qs = msg.questions || [];
    const first = qs[0] || {};
    const opts = (first.options || []).map(o => typeof o === 'string' ? o : (o.label + (o.description ? ' — ' + o.description : '')));
    V3.queueStore.upsert({
      id: 'question-' + (msg.session_id || 'main') + '-' + msg.id,
      kind: 'question', sev: 'attention', ts: Date.now(), when: 'just now',
      session: msg.session_id, machine: 'local',
      title: first.question || 'The house has a question',
      consequence: qs.length > 1 ? (qs.length + ' questions — answer what you can; it fills in the rest.') : 'It can’t decide this one for you.',
      options: opts.length ? opts : null,
      freeText: true,
      questions: qs,
      actions: [
        { id: 'answer', label: 'Answer', kind: 'primary', key: '↵' },
        { id: 'skip', label: 'Let it decide', kind: 'quiet', key: 's' }
      ],
      ref: { id: msg.id, sessionId: msg.session_id }
    });
  },
  displayRequest(msg) {
    const s = V3.data.sessions.find(x => x.id === msg.session_id);
    const name = s ? '“' + s.name + '”' : 'A session';
    const touch = msg.access === 'view_and_control';
    V3.queueStore.upsert({
      id: 'display-' + (msg.session_id || 'main') + '-' + msg.id,
      kind: 'display', sev: 'attention', ts: Date.now(), when: 'just now',
      session: msg.session_id, machine: 'local',
      title: name + ' is asking to ' + (touch ? 'see and touch' : 'see') + ' this screen',
      consequence: (msg.reason ? '“' + msg.reason + '”. ' : '') + (touch ? 'It could click and type while you watch.' : 'It can look but not touch.') + ' Never approved automatically.',
      durations: ['15 minutes', 'This session', 'Until I revoke it'],
      durationMap: ['15m', 'this_session', 'until_revoked'],
      actions: [
        { id: 'allow', label: 'Allow 15 min', kind: 'safe', key: 'y' },
        { id: 'deny', label: 'Not now', kind: 'quiet', key: 'n' },
        { id: 'deny-session', label: 'Never this session', kind: 'danger' }
      ],
      ref: { id: msg.id, sessionId: msg.session_id, access: msg.access }
    });
  },
  notify(msg) {
    if (!msg.urgency || msg.urgency === 'info') return;
    V3.queueStore.upsert({
      id: 'notify-' + (msg.id || msg.ts || Date.now()),
      kind: 'fyi', sev: msg.urgency === 'urgent' ? 'attention' : 'info', ts: msg.ts || Date.now(), when: 'just now',
      session: msg.session_id,
      title: msg.title || 'The house says',
      consequence: msg.text || '',
      actions: [{ id: 'dismiss', label: 'Got it', kind: 'quiet', key: 'x' }],
      ref: {}
    });
  },
  budget(msg, exhausted) {
    V3.queueStore.upsert({
      id: 'budget', kind: 'fyi', sev: exhausted ? 'attention' : 'info', ts: Date.now(), when: 'just now',
      title: exhausted ? 'The budget is spent' : 'The budget is at ' + Math.round(msg.pct || 0) + '%',
      consequence: exhausted ? 'The house stops mid-thought until you top up.' : 'It will stop when it runs out — Settings → fine print holds the dial.',
      actions: [{ id: 'dismiss', label: 'Got it', kind: 'quiet', key: 'x' }],
      ref: {}
    });
  },
  fuel(status) {
    const dry = status && !status.openai && !status.anthropic && !status.gemini;
    V3.queueStore.removeRef(q => q.id === 'fuel');
    if (dry) {
      V3.queueStore.upsert({
        id: 'fuel', kind: 'fyi', sev: 'attention', ts: Date.now(), when: 'now',
        title: 'This daemon has no fuel yet',
        consequence: 'The API keys it runs on. Add one and it’s off — Settings → Who provides the minds?',
        actions: [{ id: 'fuel', label: 'Add API keys', kind: 'primary' }, { id: 'dismiss', label: 'Later', kind: 'quiet', key: 'x' }],
        ref: {}
      });
    }
  }
};

/* ============================== the store init ============================== */

V3.data.init = function () {
  const T = V3.transport;
  const D = V3.data;

  /* ----- wire subscriptions ----- */
  T.on('t:state_snapshot', msg => {
    D.connFact = 'live · ' + (msg.state && msg.state.phase || 'idle');
    const st = msg.state || {};
    if (st.pending_approval) V3.queueDerive.approval({ id: st.pending_approval.id, command: st.pending_approval.command_preview || st.pending_approval.command, session_id: msg.session_id, category: st.pending_approval.category });
    if (st.pending_question) V3.queueDerive.question({ id: st.pending_question.id, questions: st.pending_question.questions, session_id: msg.session_id });
    V3.bus.emit('conn');
  });
  T.on('t:log_replay', msg => {
    (msg.entries || []).forEach(e => V3.data.ingestLog(e));
    V3.bus.emit('conversation');
  });
  T.on('event:approval_required', V3.queueDerive.approval);
  T.on('event:user_question', V3.queueDerive.question);
  T.on('event:display_request_raised', V3.queueDerive.displayRequest);
  T.on('event:approval_resolved', msg => V3.queueStore.removeRef(q => q.ref && q.ref.id === msg.id && (q.kind === 'approval' || q.kind === 'question')));
  T.on('event:display_request_resolved', msg => V3.queueStore.removeRef(q => q.ref && q.ref.id === msg.id && q.kind === 'display'));
  T.on('event:user_notification', V3.queueDerive.notify);
  T.on('event:budget_warning', m => V3.queueDerive.budget(m, false));
  T.on('event:budget_exhausted', m => V3.queueDerive.budget(m, true));

  T.on('event:session_started', msg => {
    D.sessions.unshift({
      id: msg.session_id, name: V3.norm.sessionName(msg), backend: msg.backend || 'intendant',
      model: '', phase: 'working', active: true, turn: 0, cost: 0, costKnown: true,
      tokens: { used: 0, pct: 0 }, task: msg.task || '', cwd: '', relationships: [],
      machine: 'local', sentence: 'Starting up…'
    });
    D.conversation.push({ kind: 'milestone', text: '“' + V3.norm.sessionName(msg) + '” started', ts: Date.now() });
    V3.bus.emit('sessions'); V3.bus.emit('conversation');
  });
  T.on('event:session_ended', msg => {
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s) { s.active = false; s.phase = msg.error_kind ? 'failed' : 'done'; s.sentence = msg.reason || 'Finished'; }
    V3.queueStore.removeRef(q => q.session === msg.session_id && q.kind !== 'fyi');
    V3.bus.emit('sessions'); V3.bus.emit('queue');
  });
  T.on('event:status', msg => {
    if (msg.session_id) {
      const s = D.sessions.find(x => x.id === msg.session_id);
      if (s) {
        s.turn = msg.turn || s.turn;
        s.phase = msg.phase === 'idle' ? 'idle' : 'working';
        s.active = msg.phase !== 'idle';
        if (msg.task) s.task = msg.task;
      }
    }
    if (msg.autonomy) D.autonomy = msg.autonomy;
    D.connFact = (msg.provider || '') + (msg.model ? ' · ' + msg.model : '') || 'live';
    V3.bus.emit('sessions'); V3.bus.emit('conn');
  });
  T.on('event:autonomy_changed', msg => { if (msg.level || msg.autonomy) D.autonomy = msg.level || msg.autonomy; });
  T.on('event:model_response', msg => {
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s && msg.summary) { s.sentence = msg.summary; s.turn = msg.turn || s.turn; V3.bus.emit('sessions'); }
    V3.data.appendLog(msg.session_id, msg.turn, 'model', msg.summary || msg.reasoning_summary || '');
  });
  T.on('event:agent_output', msg => {
    V3.data.appendLog(msg.session_id, null, 'tool', (msg.stdout || msg.stderr || '').slice(0, 400));
  });
  T.on('event:log_entry', msg => {
    V3.data.appendLog(msg.session_id, msg.turn, msg.level || 'info', msg.content || '');
  });
  T.on('event:task_received', msg => {
    D.conversation.push({ kind: 'milestone', text: 'task received — ' + (msg.task || '').slice(0, 60), ts: Date.now(), session: msg.session_id });
    V3.bus.emit('conversation');
  });
  T.on('event:task_complete', msg => {
    D.conversation.push({ kind: 'milestone', text: 'finished — ' + (msg.summary || msg.reason || '').slice(0, 80), ts: Date.now(), session: msg.session_id });
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s) { s.phase = 'done'; s.active = false; s.sentence = 'Finished — ' + (msg.summary || msg.reason || ''); }
    V3.bus.emit('conversation'); V3.bus.emit('sessions');
  });
  T.on('event:done_signal', msg => {
    /* the loop's own completion fact — task_complete may not follow */
    const text = 'finished — ' + (msg.message || msg.reason || 'done').slice(0, 80);
    D.conversation.push({ kind: 'milestone', text, ts: Date.now(), session: msg.session_id });
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s) { s.phase = 'done'; s.active = false; s.sentence = 'Finished — ' + (msg.message || ''); }
    V3.bus.emit('conversation'); V3.bus.emit('sessions');
  });
  T.on('event:presence_log', msg => {
    const text = msg.message || '';
    /* internal wire chatter is not the house's voice */
    if (text.startsWith('[ws]') || text.includes('ControlMsg')) return;
    D.conversation.push({ from: 'presence', prose: [text], level: msg.level, at: V3.now(), ts: Date.now() });
    V3.bus.emit('conversation');
  });
  T.on('event:usage_update', msg => {
    if (msg.session_id && msg.main) {
      D.usage[msg.session_id] = msg.main;
      const s = D.sessions.find(x => x.id === msg.session_id);
      if (s) { s.tokens.pct = Math.round(msg.main.usage_pct || 0); if (msg.main.model) s.model = msg.main.model; }
      V3.bus.emit('sessions');
    } else if (msg.main) { D.usage.main = msg.main; }
  });
  T.on('event:session_vitals', msg => {
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s) {
      s.vitals = Object.assign(s.vitals || {}, msg.vitals || msg);
      V3.bus.emit('sessions');
    }
  });
  T.on('event:session_identity', msg => {
    const s = D.sessions.find(x => x.id === msg.session_id);
    if (s && msg.name) { s.name = msg.name; V3.bus.emit('sessions'); }
  });
  T.on('event:agenda_changed', () => V3.data.refresh.agenda());
  T.on('event:display_ready', msg => {
    if (!D.displays.find(d => d.id === msg.display_id)) D.displays.push({ id: msg.display_id, w: msg.width, h: msg.height, live: true });
    V3.bus.emit('displays');
  });
  T.on('event:peer_event_forwarded', msg => {
    const inner = msg.event || msg.peer_event || {};
    if (inner.approval_required || inner.event === 'approval_required') {
      const a = inner.approval_required || inner;
      V3.queueStore.upsert({
        id: 'peer-approval-' + msg.peer_id + '-' + a.id,
        kind: 'peer-approval', sev: 'attention', ts: Date.now(), when: 'just now',
        machine: msg.peer_id,
        title: V3.norm.commandTitle(a.command, 'A peer session'),
        consequence: 'On ' + (V3.machineName(msg.peer_id) || msg.peer_id) + ' — its IAM asked you to decide.',
        details: { command: a.command || '', raw: JSON.stringify(a) },
        actions: [
          { id: 'approve', label: 'Allow', kind: 'safe', key: 'y' },
          { id: 'deny', label: 'Decline', kind: 'danger', key: 'n' }
        ],
        ref: { id: a.id, peerId: msg.peer_id }
      });
    }
  });

  /* ----- initial polls ----- */
  const polls = [
    ['sessions', '/api/sessions', rows => {
      D.sessions = (Array.isArray(rows) ? rows : []).map(r => V3.norm.session(r));
      V3.bus.emit('sessions');
    }],
    ['agenda', '/api/agenda', r => {
      D.agenda = r.items || []; D.agendaCounts = r.counts || D.agendaCounts; D.reminderPolicy = r.reminder_policy || null;
      V3.bus.emit('agenda');
    }],
    ['peers', '/api/peers', r => {
      const peers = (r.peers || []).map(p => ({
        id: p.id, petname: p.label || p.id, label: p.id, route: p.browser_tcp_via_url ? 'fleet name' : 'direct',
        role: p.role || 'peer', status: p.connection_state === 'connected' ? 'online' : 'away',
        capabilities: (p.capabilities || []).map(c => c.kind === 'computer-use' ? 'computer-use' : (c.name || c.kind)),
        sessions: p.sessions || [], displays: p.displays || [], version: p.version || ''
      }));
      D.machines = [{
        id: 'local', petname: 'This machine', label: 'this daemon', route: 'direct', role: 'owner',
        status: 'online', capabilities: ['computer-use', 'display', 'voice', 'terminal'],
        version: (T.config && T.config.app_build) || '', pressure: null, fueled: true
      }].concat(peers);
      V3.bus.emit('machines');
    }],
    ['fuel', '/api/api-key-status', r => { D.fuel = r; V3.queueDerive.fuel(r); }],
    ['settings', '/api/settings', r => { D.settings = r; V3.bus.emit('settings'); }],
    ['external-agents', '/api/external-agents', r => { D.externalAgents = r.external_agents || []; V3.bus.emit('agents'); }],
    ['access', '/api/access/overview', r => {
      D.accessOverview = r;
      D.people = (r.principals || []).map(p => ({
        who: p.label || p.id, key: p.key_id || p.id, role: p.role_id || p.role || '—',
        route: p.route || '—', lifecycle: p.revoked ? 'revoked' : 'active', since: p.created || ''
      }));
      D.grants = r.grants || [];
      V3.bus.emit('people');
    }],
    ['enrollment', '/api/access/enrollment-requests', r => {
      (r.requests || []).forEach(req => {
        V3.queueStore.upsert({
          id: 'enroll-' + (req.fingerprint || req.id || req.label),
          kind: 'enrollment', sev: 'attention', ts: Date.now(), when: 'just now',
          title: 'A new device wants in: “' + (req.label || 'unnamed') + '”',
          consequence: 'Approving hands it a key. Spectator watches; Operator runs work. Revoke any time.',
          roles: ['spectator', 'operator', 'session-reader'],
          actions: [
            { id: 'spectator', label: 'Spectator key', kind: 'safe', key: 'y' },
            { id: 'operator', label: 'Operator key', kind: 'quiet' },
            { id: 'deny', label: 'Deny', kind: 'danger', key: 'n', default: true }
          ],
          ref: { fingerprint: req.fingerprint || req.id }
        });
      });
    }],
    ['pairing', '/api/peers/pairing/requests', r => {
      (r.requests || []).filter(x => x.status === 'pending').forEach(req => {
        V3.queueStore.upsert({
          id: 'pair-' + req.code,
          kind: 'peer-pair', sev: 'attention', ts: (req.created_at_unix || 0) * 1000 || Date.now(), when: 'just now',
          title: '“' + (req.requester_label || 'A daemon') + '” wants to pair',
          consequence: 'It asks for the “' + (req.requested_profile || 'peer') + '” profile. Pairing creates a route — your IAM still decides what it may do.',
          actions: [
            { id: 'approve', label: 'Pair', kind: 'safe', key: 'y' },
            { id: 'deny', label: 'Decline', kind: 'danger', key: 'n', default: true }
          ],
          ref: { code: req.code }
        });
      });
    }],
    ['hosted', '/api/access/hosted-control', r => {
      (r.pending_requests || []).forEach(req => {
        V3.queueStore.upsert({
          id: 'hosted-' + req.request_id,
          kind: 'hosted', sev: 'attention', ts: Date.now(), when: 'just now',
          title: '“' + (req.requester_label || 'Someone') + '” asks to borrow this daemon',
          consequence: 'A hosted lease — preset “' + (req.requested_preset || 'view') + '”, time-boxed, revocable. The compiled floor stands.',
          actions: [
            { id: 'approve', label: 'Grant lease', kind: 'safe', key: 'y' },
            { id: 'deny', label: 'Decline', kind: 'danger', key: 'n', default: true }
          ],
          ref: { requestId: req.request_id }
        });
      });
    }]
  ];

  /* polls are best-effort: a 403/404 marks the lane gated, never fatal */
  const results = polls.map(([name, path, ok]) =>
    V3.transport.get(path).then(ok).catch(e => console.warn('[v3] poll ' + name + ' gated: ' + e.message)));

  /* periodic refresh of the poll-only queue lanes + fuel */
  V3.data._pollTimer = setInterval(() => {
    if (document.hidden) return;
    ['fuel', 'enrollment', 'pairing', 'hosted'].forEach((name, i) => {
      const p = polls.find(x => x[0] === name);
      if (p) V3.transport.get(p[1]).then(p[2]).catch(() => {});
    });
  }, 30000);

  return Promise.allSettled(results).then(() => V3.bus.emit('ready'));
};

V3.now = function () {
  const d = new Date();
  return String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
};

V3.data.refresh = {
  agenda() {
    V3.transport.get('/api/agenda').then(r => {
      V3.data.agenda = r.items || []; V3.data.agendaCounts = r.counts || V3.data.agendaCounts;
      V3.bus.emit('agenda');
    }).catch(() => {});
  },
  sessions() {
    V3.transport.get('/api/sessions').then(rows => {
      V3.data.sessions = (Array.isArray(rows) ? rows : []).map(r => V3.norm.session(r));
      V3.bus.emit('sessions');
    }).catch(() => {});
  }
};

/* rolling per-session log buffer (timeline + show-work) */
V3.data.appendLog = function (sessionId, turn, kind, text) {
  if (!text) return;
  const key = sessionId || 'main';
  const buf = V3.data.logs[key] = V3.data.logs[key] || [];
  buf.push({ t: V3.now(), turn, kind, text: String(text) });
  if (buf.length > 300) buf.splice(0, buf.length - 300);
  V3.bus.emit('logs:' + key);
};
V3.data.ingestLog = function (e) {
  const sessionId = e.session_id || (e.data && e.data.session_id) || 'main';
  V3.data.appendLog(sessionId, e.turn, e.level || e.event || 'info', e.message || e.summary || (e.data && e.data.message) || '');
};

/* ============================== actions (intents out) ============================== */

V3.actions = {
  sendMessage(text) {
    const target = V3.data.composerTarget;
    const msg = target
      ? { action: 'start_task', task: text, session_id: target }
      : { action: 'create_session', task: text };
    msg.follow_up_id = 'v3-' + Date.now().toString(36);
    if (V3.transport.send(msg)) {
      V3.data.conversation.push({ from: 'you', text, at: V3.now(), ts: Date.now() });
      V3.bus.emit('conversation');
    }
  },

  resolveQueueItem(item, act) {
    const T = V3.transport, ref = item.ref || {};
    let sent = false;
    switch (item.kind) {
      case 'approval': {
        const map = { approve: 'approve', skip: 'skip', always: 'approve_all', deny: 'deny' };
        if (!map[act]) return;
        sent = T.send({ action: map[act], id: ref.id, session_id: ref.sessionId });
        break;
      }
      case 'question': {
        if (act === 'answer') {
          const answers = V3.actions._collectAnswers(item);
          sent = T.send({ action: 'answer_question', id: ref.id, session_id: ref.sessionId, answers });
        } else {
          sent = T.send({ action: 'skip', id: ref.id });
        }
        break;
      }
      case 'display': {
        const map = { allow: 'approve', deny: 'deny', 'deny-session': 'deny_session' };
        if (!map[act]) return;
        const dur = act === 'allow' ? '15m' : undefined;
        sent = T.send({ action: 'resolve_display_request', id: ref.id, decision: map[act], duration: dur, session_id: ref.sessionId });
        break;
      }
      case 'enrollment': {
        if (act === 'deny') { V3.transport.post('/api/access/enrollment-requests/decide', { fingerprint: ref.fingerprint, approve: false }).catch(V3.actions._err); }
        else { V3.transport.post('/api/access/enrollment-requests/decide', { fingerprint: ref.fingerprint, approve: true, role_id: 'role:' + act }).catch(V3.actions._err); }
        sent = true;
        break;
      }
      case 'peer-pair': {
        V3.transport.post('/api/peers/pairing/requests/' + encodeURIComponent(ref.code) + '/' + (act === 'approve' ? 'approve' : 'deny'), {}).catch(V3.actions._err);
        sent = true;
        break;
      }
      case 'peer-approval': {
        V3.transport.post('/api/peers/' + encodeURIComponent(ref.peerId) + '/approval', { request_id: ref.id, decision: act === 'approve' ? 'accept' : 'decline' }).catch(V3.actions._err);
        sent = true;
        break;
      }
      case 'hosted': {
        V3.transport.post('/api/access/hosted-control/requests/decide', { request_id: ref.requestId, approve: act === 'approve' }).catch(V3.actions._err);
        sent = true;
        break;
      }
      case 'fyi': {
        if (act === 'fuel') { V3.go('#/settings'); return; }
        sent = true; // dismiss
        break;
      }
    }
    if (sent) {
      V3.queueStore.remove(item.id);
      V3.toast(act === 'deny' || act === 'deny-session' ? 'Denied — nothing was touched' : 'Done — the house carries on',
               act === 'deny' || act === 'deny-session' ? 'brick' : 'sage');
    }
  },

  _collectAnswers(item) {
    const answers = {};
    const card = document.querySelector('[data-qcard="' + item.id + '"]');
    (item.questions || []).forEach((q, i) => {
      const qt = q.question || String(i);
      if (card) {
        const radio = card.querySelector('input[name="' + item.id + '-opt-' + i + '"]:checked');
        const free = card.querySelector('input[data-free="' + i + '"]');
        if (free && free.value.trim()) { answers[qt] = free.value.trim(); return; }
        if (radio) { answers[qt] = radio.value; return; }
      }
      answers[qt] = '';
    });
    return answers;
  },

  interrupt(sessionId) { V3.transport.send({ action: 'interrupt', session_id: sessionId }); },
  stopSession(sessionId) { V3.transport.send({ action: 'stop_session', session_id: sessionId }); },
  resumeSession(sessionId) { V3.transport.send({ action: 'resume_session', session_id: sessionId }); },
  steer(sessionId, text) { V3.transport.send({ action: 'steer', session_id: sessionId, text, id: 'v3s-' + Date.now().toString(36) }); },
  followUp(sessionId, text) { V3.transport.send({ action: 'follow_up', session_id: sessionId, text, follow_up_id: 'v3f-' + Date.now().toString(36) }); },

  setApprovalRule(category, rule) {
    V3.transport.send({ action: 'set_approval_rule', category, rule });
    V3.toast(category + ' → ' + rule + ' — live', 'sage');
  },
  setAutonomy(level) {
    V3.transport.send({ action: 'set_autonomy', level });
    V3.toast('Autonomy → ' + level + ' — next task onward', 'sage');
  },
  saveSettings(patch) {
    /* merge-then-POST: the daemon expects the full payload, and a partial
       POST must never reset a neighbor */
    return V3.transport.get('/api/settings')
      .then(cur => V3.transport.post('/api/settings', Object.assign({}, cur, patch)))
      .then(() => {
        V3.toast('Saved — the daemon picked it up live', 'sage');
        Object.assign(V3.data.settings || {}, patch);
      })
      .catch(V3.actions._err);
  },
  agendaOp(cmd) {
    return V3.transport.post('/api/agenda/op', cmd)
      .then(() => { V3.data.refresh.agenda(); return true; })
      .catch(e => { V3.actions._err(e); return false; });
  },
  grantUserDisplay(duration) { V3.transport.send({ action: 'grant_user_display', duration }); },
  revokeUserDisplay() { V3.transport.send({ action: 'revoke_user_display' }); },
  takeDisplay(id) { V3.transport.send({ action: 'take_display', display_id: id }); },
  releaseDisplay(id) { V3.transport.send({ action: 'release_display', display_id: id }); },

  toggleVoice() {
    V3.transport.send({ t: 'presence_connect' });
    V3.toast('Asking the house to pick up… (live voice connects when fueled)', null);
  },
  attach() { V3.toast('Attachments stage here and ride the next message', null); },

  setTarget(sessionId) {
    V3.data.composerTarget = sessionId || null;
    V3.bus.emit('target');
  },

  _err(e) { V3.toast('The daemon said no: ' + e.message, 'brick'); console.warn('[v3] action failed', e); }
};
