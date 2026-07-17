/* Proscenium — mock control-plane state.
   Shapes mirror the daemon's own vocabulary (StatusSnapshot, SessionVitals,
   PeerSnapshot, the 8 approval categories, builtin roles, peer profiles,
   NotificationUrgency). Nothing here is fantasy data: every field has a
   source in gateway_routes::ROUTES, the AppEvent stream, or the IAM model. */
window.P = window.P || {};

P.data = {

  you: {
    name: 'Val', role: 'owner', device: 'This browser · Safari',
    route: 'direct mTLS', since: 'Mar 2026', keyId: 'k_9f2c…a41e'
  },

  /* ------------------------------------------------ machines (fleet) --- */
  machines: [
    {
      id: 'local', petname: 'Studio Mac', label: 'macbook-a', thisMachine: true,
      route: 'direct', role: 'owner', os: 'macOS 15.5', status: 'online',
      pressure: { cpu: 34, mem: 61 }, fueled: true,
      capabilities: ['computer-use', 'display', 'voice', 'terminal'],
      version: '0.42.3 · a5081f18'
    },
    {
      id: 'dell', petname: 'Workshop', label: 'dell-206', thisMachine: false,
      route: 'fleet name', role: 'operator', os: 'Debian 13', status: 'online',
      pressure: { cpu: 12, mem: 38 }, fueled: true,
      capabilities: ['computer-use', 'terminal', 'headless'],
      version: '0.42.3 · a5081f18', lease: 'fuel lease · 2 days left'
    },
    {
      id: 'samsung', petname: 'Parlor PC', label: 'samsung-win', thisMachine: false,
      route: 'hosted', role: 'observer', os: 'Windows 11', status: 'away',
      pressure: { cpu: 0, mem: 0 }, fueled: false,
      capabilities: ['display'],
      version: '0.41.0 · 9d65f325', note: 'Away 3 days — fuel expired'
    }
  ],

  /* ------------------------------------------------ sessions (work) ---- */
  sessions: [
    {
      id: 'fix-login', name: 'fix-login', backend: 'claude-code', model: 'claude-fable',
      machine: 'local', phase: 'working', turn: 14,
      sentence: 'Editing the login flow — 12 files changed so far',
      task: 'Fix the OAuth redirect loop on staging. Users bounce between /login and /auth/callback when the session cookie is cross-site.',
      started: '09:41', cost: 0.83, tokens: { used: 184200, ctx: 200000, pct: 82 },
      branch: 'fix/login-redirect', dirty: 12, ahead: 3,
      cache: { hit: 94, ttl: '4m 12s' }, limits: { fiveHour: 31, week: 12 },
      queue: ['q-approve-delete'],
      caps: { follow_up: true, steer: true, interrupt: true, thread_actions: false },
      subagents: []
    },
    {
      id: 'docs-sweep', name: 'docs-sweep', backend: 'codex', model: 'gpt-5.2-codex',
      machine: 'dell', phase: 'working', turn: 8, peer: true,
      sentence: 'Running the link checker across the mdBook — 41 of 96 chapters scanned',
      task: 'Sweep docs/src for dead links and stale route references; fix what is safe, list the rest.',
      started: '10:07', cost: 0.31, tokens: { used: 96400, ctx: 272000, pct: 35 },
      branch: 'docs/link-sweep', dirty: 6, ahead: 1,
      cache: { hit: 88, ttl: '1m 3s' }, limits: { fiveHour: 18, week: 9 },
      queue: [], caps: { follow_up: true, steer: true, interrupt: true, thread_actions: true },
      subagents: []
    },
    {
      id: 'photo-book', name: 'photo-book', backend: 'internal', model: 'orchestrate',
      machine: 'local', phase: 'done', turn: 46, finishedAgo: '6:12 this morning',
      sentence: 'Finished — the photo book is laid out and exported',
      task: 'Make a printed photo book from last Christmas — pick the best 60 photos, lay them out, export a PDF for the print shop.',
      cost: 2.41, tokens: { used: 612000, ctx: 200000, pct: 100 },
      branch: 'main', dirty: 0, cache: { hit: 91 }, limits: {},
      queue: [], caps: { follow_up: true, steer: false, interrupt: false, thread_actions: false },
      subagents: [
        { name: 'photo-cull', role: 'research', result: '62 picks from 1,840 photos' },
        { name: 'book-layout', role: 'implementation', result: '38-page spread, PDF exported' }
      ],
      resultFiles: ['~/Pictures/photo-book/christmas-2025.pdf', '~/Pictures/photo-book/cover.png']
    },
    {
      id: 'nightly-backup', name: 'nightly-backup', backend: 'internal', model: 'direct',
      machine: 'local', phase: 'done', turn: 3, finishedAgo: '02:00 (scheduled)',
      sentence: 'Finished — one retry, then clean. 41.2 GB to the NAS.',
      task: 'Nightly backup of ~/Documents and ~/Pictures to the NAS.', cost: 0.04,
      tokens: { used: 12400, ctx: 200000, pct: 6 }, branch: '—', dirty: 0,
      cache: { hit: 0 }, limits: {}, queue: [], scheduled: 'every day · 02:00',
      caps: { follow_up: true, steer: false, interrupt: false, thread_actions: false },
      subagents: [], note: 'rsync failed once (NAS asleep) — retried after wake-on-LAN, clean.'
    },
    {
      id: 'q2-invoices', name: 'q2-invoices', backend: 'claude-code', model: 'claude-sonnet',
      machine: 'local', phase: 'idle', idleSince: 'Tuesday',
      sentence: 'Idle since Tuesday — waiting for you, whenever',
      task: 'Reconcile Q2 invoices against the bank export and flag anything over $500 without a PO.',
      cost: 1.12, tokens: { used: 288000, ctx: 200000, pct: 44 },
      branch: 'main', dirty: 2, cache: { hit: 76 }, limits: {},
      queue: [], caps: { follow_up: true, steer: true, interrupt: true, thread_actions: false },
      subagents: []
    },
    {
      id: 'station-polish', name: 'station-polish', backend: 'internal', model: 'sub-agent',
      machine: 'local', phase: 'archived', finishedAgo: 'last week',
      sentence: 'Archived — merged into its parent',
      task: 'Sub-agent: soften the Station approval-glow ring animation.', cost: 0.19,
      tokens: { used: 41000, ctx: 200000, pct: 21 }, branch: 'fix/station-glow', dirty: 0,
      cache: { hit: 85 }, limits: {}, queue: [], parent: 'dashboard-everything-concept',
      caps: { follow_up: false, steer: false, interrupt: false, thread_actions: false },
      subagents: []
    }
  ],

  /* ------------------------------------------------ the Queue ---------- */
  queue: [
    {
      id: 'q-approve-delete', kind: 'approval', sev: 'attention', when: '2 min ago',
      category: 'file_delete', session: 'fix-login', machine: 'local', backend: 'Claude Code',
      title: 'Claude wants to delete 3 files',
      consequence: 'In ~/projects/shopify-theme/exports — old CSV dumps it says are regenerated each run. This cannot be undone.',
      details: {
        command: 'rm exports/2026-05-orders.csv exports/2026-06-orders.csv exports/backup-orders.csv',
        paths: ['exports/2026-05-orders.csv', 'exports/2026-06-orders.csv', 'exports/backup-orders.csv'],
        rule: 'Always allowing would set file_delete → Auto for this project.',
        raw: '{ "category": "file_delete", "tool": "Bash", "command": "rm …", "session": "fix-login", "turn": 14 }'
      },
      actions: [
        { id: 'approve', label: 'Allow once', kind: 'safe', key: 'y' },
        { id: 'always', label: 'Always allow here', kind: 'quiet', key: 'a' },
        { id: 'deny', label: 'Deny', kind: 'danger', key: 'n', default: true }
      ]
    },
    {
      id: 'q-callback-url', kind: 'question', sev: 'attention', when: '6 min ago',
      session: 'fix-login', machine: 'local',
      title: 'Which callback URL should win?',
      consequence: 'The OAuth provider allows exactly one. The redirect loop lives in this choice.',
      options: ['/auth/callback (keep, register with provider)', '/callback (shorter, update provider)'],
      freeText: true,
      actions: [{ id: 'answer', label: 'Answer', kind: 'primary', key: '↵' }, { id: 'skip', label: 'Let Claude decide', kind: 'quiet', key: 's' }]
    },
    {
      id: 'q-doorbell', kind: 'display', sev: 'attention', when: 'just now',
      session: 'fix-login', machine: 'local',
      title: '“fix-login” is asking to see this screen',
      consequence: 'It can look but not touch — to read the staging error page you left open. Never approved automatically.',
      durations: ['15 minutes', 'This session', 'Until I revoke it'],
      actions: [
        { id: 'allow', label: 'Allow 15 min', kind: 'safe', key: 'y' },
        { id: 'deny', label: 'Not now', kind: 'quiet', key: 'n' },
        { id: 'deny-session', label: 'Never this session', kind: 'danger' }
      ]
    },
    {
      id: 'q-ipad', kind: 'enrollment', sev: 'attention', when: '28 min ago',
      machine: 'local',
      title: 'A new device wants in: “Val’s iPad”',
      consequence: 'Approving hands it a key. Spectator can watch; Operator can run work. You can revoke either, any time.',
      roles: ['spectator', 'operator', 'session-reader'],
      actions: [
        { id: 'spectator', label: 'Spectator key', kind: 'safe', key: 'y' },
        { id: 'operator', label: 'Operator key', kind: 'quiet' },
        { id: 'deny', label: 'Deny', kind: 'danger', key: 'n', default: true }
      ]
    },
    {
      id: 'q-context', kind: 'fyi', sev: 'info', when: '12 min ago',
      session: 'fix-login',
      title: '“fix-login” has used 82% of its context',
      consequence: 'It will compact soon, or you can rewind to the anchor at turn 9 if it drifts.',
      actions: [{ id: 'open', label: 'Open session', kind: 'quiet' }, { id: 'dismiss', label: 'Got it', kind: 'quiet', key: 'x' }]
    },
    {
      id: 'q-lease', kind: 'fyi', sev: 'info', when: '1 h ago',
      machine: 'dell',
      title: 'Workshop’s fuel lease runs out in 2 days',
      consequence: 'It will politely go dry — nothing lost, it just stops thinking until you renew.',
      actions: [{ id: 'renew', label: 'Renew lease', kind: 'primary' }, { id: 'dismiss', label: 'Later', kind: 'quiet', key: 'x' }]
    }
  ],

  /* ------------------------------------------------ the Conversation --- */
  conversation: [
    {
      from: 'presence', kind: 'briefing', at: '07:58',
      prose: [
        'Good morning, Val. Two things finished overnight, four things need you, and one machine is thirsty.',
        'The **photo book** is done — sixty-two picks from your Christmas dump, laid out and exported. The **backup** failed once (the NAS was asleep), woke it, retried, clean. **fix-login** is fourteen turns in and has a question only you can answer.'
      ],
      artifacts: [
        {
          type: 'changes', title: 'photo-book — finished at 06:12',
          stat: '+38 pages · 62 photos · $2.41',
          files: ['christmas-2025.pdf (38 pages, 41 MB)', 'cover.png'],
          link: 'files'
        }
      ]
    },
    { from: 'you', at: '08:02', text: 'lovely. show me the cover when i’m done with the queue' },
    {
      from: 'presence', at: '08:02',
      prose: ['It’s waiting in Files, and I’ve pinned the cover to the top of the session. I’ll keep the queue warm for you — the deletion ask is the only one I’d read carefully; the rest are one tap each.']
    },
    { kind: 'milestone', text: 'docs-sweep started on Workshop · 10:07', detail: 'log lines 1–214' },
    {
      from: 'presence', at: '10:07',
      prose: ['Workshop is sweeping the docs now — forty-one chapters in, no dead routes so far. It’s on the fleet name route, so it watches; it can’t touch anything here.']
    }
  ],

  /* ------------------------------------------------ Today ribbon ------- */
  today: [
    { t: '02:00', label: 'nightly-backup ran', kind: 'done', note: '1 retry · clean' },
    { t: '06:12', label: 'photo-book finished', kind: 'done' },
    { t: '09:41', label: 'fix-login started', kind: 'done' },
    { t: 'now', label: '4 things need you', kind: 'now' },
    { t: '13:30', label: 'reminder: dentist 15:00', kind: 'reminder' },
    { t: '18:00', label: 'q2-invoices check-in (scheduled)', kind: 'scheduled' },
    { t: '02:00', label: 'nightly-backup (scheduled)', kind: 'scheduled' }
  ],

  /* ------------------------------------------------ agenda (The List) -- */
  agenda: [
    { id: 'a1', kind: 'task', title: 'Renew Workshop’s fuel lease', status: 'open', added: 'yesterday' },
    { id: 'a2', kind: 'question', title: 'Should photo-book v2 use the lay-flat binding?', status: 'open', added: '06:15' },
    { id: 'a3', kind: 'task', title: 'Nightly backup of ~/Documents + ~/Pictures', status: 'open', scheduled: 'every day · 02:00' },
    { id: 'a4', kind: 'note', title: 'Print shop wants PDF/X-4, not PDF 1.7', status: 'open', added: 'Tue' },
    { id: 'a5', kind: 'task', title: 'Rotate the fleet claim code after the iPad joins', status: 'done', added: 'Mon' }
  ],

  /* ------------------------------------------------ memory ------------- */
  memory: [
    { kind: 'preference', statement: 'Val prefers plain-language summaries; no jargon in briefings.', sensitivity: 'private', status: 'accepted', by: 'owner' },
    { kind: 'decision', statement: 'The print shop takes PDF/X-4 only — always export both.', sensitivity: 'internal', status: 'accepted', by: 'photo-book' },
    { kind: 'observation', statement: 'The NAS sleeps after 20 min; wake it before rsync.', sensitivity: 'internal', status: 'accepted', by: 'nightly-backup' },
    { kind: 'procedure', statement: 'For OAuth bugs on staging, check the callback registry first.', sensitivity: 'internal', status: 'candidate', by: 'fix-login' },
    { kind: 'episode', statement: '2026-07-16: the redirect loop turned out to be a SameSite cookie mismatch.', sensitivity: 'private', status: 'candidate', by: 'fix-login' }
  ],

  /* ------------------------------------------------ usage (Books) ------ */
  usage: {
    kpis: [
      { label: 'This month', value: '$41.20', sub: 'est. across 2 fueled machines' },
      { label: 'Tokens', value: '18.4M', sub: '61% served from prompt cache' },
      { label: 'Sessions', value: '126', sub: '9 active today' },
      { label: 'Cache saved', value: '$63.80', sub: 'what caching didn’t spend' }
    ],
    byAgent: [
      { agent: 'Claude Code · fable', cost: 18.40, sessions: 41 },
      { agent: 'Codex · gpt-5.2', cost: 12.10, sessions: 33 },
      { agent: 'Native · orchestrate', cost: 7.90, sessions: 30 },
      { agent: 'Presence · voice', cost: 2.80, sessions: 22 }
    ],
    heat: null, /* generated: 26 weeks × 7 */
    disk: [
      { what: 'Recordings', size: '12.4 GB' }, { what: 'Frames', size: '3.1 GB' },
      { what: 'Session logs', size: '812 MB' }, { what: 'Transfers', size: '240 MB' }
    ]
  },

  /* ------------------------------------------------ people & keys ------ */
  people: [
    { who: 'Val (you)', key: 'k_9f2c…a41e', role: 'owner', route: 'direct mTLS', lifecycle: 'active', since: 'Mar 2026' },
    { who: 'Val’s iPhone', key: 'k_77b1…c902', role: 'operator', route: 'fleet name', lifecycle: 'active', since: 'Apr 2026' },
    { who: 'Sam (partner)', key: 'k_3dd8…0f77', role: 'spectator', route: 'hosted', lifecycle: 'active', since: 'May 2026' },
    { who: 'old laptop', key: 'k_51aa…9e0b', role: 'operator', route: '—', lifecycle: 'revoked', since: 'revoked Jun 2026' }
  ],
  custody: [
    { at: '09:12', what: 'Fuel lease granted → Workshop (claude · 3 days)', by: 'you' },
    { at: 'Tue', what: 'Vault unlocked (passkey)', by: 'you' },
    { at: 'Tue', what: 'Deposit consumed: gemini-live ephemeral token', by: 'presence' },
    { at: 'Mon', what: 'Lease revoked → Parlor PC (went dry)', by: 'you' },
    { at: 'Mon', what: 'Egress relay registered: api.anthropic.com via this browser', by: 'this browser' }
  ],
  vault: { state: 'Locked', entries: 4, passkeys: 2, recovery: 'BIP39 · written down' },

  /* ------------------------------------------------ screens ------------ */
  displays: [
    { id: 'disp-1', name: 'Display 1 · staging browser', machine: 'local', live: true, authority: 'agent (fix-login)', res: '2560×1440', stream: true },
    { id: 'disp-2', name: 'Virtual display · Xvfb', machine: 'dell', live: true, authority: 'agent (docs-sweep)', res: '1920×1080', peer: true },
    { id: 'disp-0', name: 'Your screen', machine: 'local', live: false, authority: 'you', private: true, res: '3024×1964' }
  ],
  recordings: [
    { id: 'rec-1', name: 'fix-login — the redirect loop, captured', len: '2:14', when: 'yesterday', size: '88 MB' },
    { id: 'rec-2', name: 'photo-book — layout timelapse', len: '0:48', when: '06:02', size: '31 MB' }
  ],

  /* ------------------------------------------------ files -------------- */
  files: [
    { path: '~/Pictures/photo-book/', kind: 'folder', children: [
      { path: 'christmas-2025.pdf', kind: 'file', size: '41 MB', note: 'exported 06:12' },
      { path: 'cover.png', kind: 'file', size: '3.2 MB' },
      { path: 'picks/', kind: 'folder', children: [] }
    ]},
    { path: '~/projects/shopify-theme/', kind: 'folder', children: [
      { path: 'exports/', kind: 'folder', note: '3 files pending decision', children: [] },
      { path: 'src/auth/', kind: 'folder', children: [] }
    ]},
    { path: '~/Documents/', kind: 'folder', children: [] }
  ],
  transfers: [
    { id: 't1', what: 'christmas-2025.pdf → Print shop upload', dir: 'up', pct: 100, state: 'done' },
    { id: 't2', what: 'bank-export-q2.csv ← Workshop', dir: 'down', pct: 62, state: 'resumable' }
  ],

  /* ------------------------------------------------ settings rows ------ */
  /* Every row is ⌘K-addressable; `fold` is the jump target. */
  settings: [
    { q: 'autonomy', section: 'How much may the house do on its own?', name: 'Autonomy', kind: 'dial',
      aliases: 'autonomy independence freedom self-driving', value: 'Medium — gate writes',
      gloss: 'How far the house goes before it asks.' },
    { q: 'approvals', section: 'How much may the house do on its own?', name: 'Approval rules', kind: 'rules',
      aliases: 'approval rules gates ask auto deny permissions', fold: 'fold-approvals',
      rows: [
        ['Read files', 'auto'], ['Edit & write', 'ask'], ['Delete files', 'ask'],
        ['Run shell commands', 'ask'], ['Network egress', 'auto'], ['Destructive actions', 'deny'],
        ['Control your display', 'ask'], ['External-agent tool calls', 'ask']
      ] },
    { q: 'providers', section: 'Who provides the minds?', name: 'API keys', kind: 'keys',
      aliases: 'api keys openai anthropic gemini fuel credentials', fold: 'fold-keys',
      rows: [['Anthropic', 'set · sk-ant-…3f9'], ['OpenAI', 'set · sk-…k21'], ['Gemini', 'not set']] },
    { q: 'providers', section: 'Who provides the minds?', name: 'External agent defaults', kind: 'rows',
      aliases: 'codex claude backend default agent', fold: 'fold-backends',
      rows: [['Default backend', 'Claude Code'], ['Codex model', 'gpt-5.2-codex'], ['Codex reasoning effort', 'high'], ['Claude model', 'claude-fable']] },
    { q: 'providers', section: 'Who provides the minds?', name: 'Codex sandbox', kind: 'rows',
      aliases: 'writable roots sandbox workspace-write codex permissions network access', fold: 'fold-codex-sandbox',
      rows: [['Sandbox mode', 'workspace-write'], ['Writable roots', '~/projects · /tmp'], ['Network access', 'off'], ['Web search', 'on']] },
    { q: 'reach', section: 'How does it reach you?', name: 'Presence', kind: 'rows',
      aliases: 'presence voice model text live', fold: 'fold-presence',
      rows: [['Text presence', 'on · claude-sonnet'], ['Live voice', 'on · gemini-2.5-flash-live'], ['Idle timeout', '10 min']] },
    { q: 'reach', section: 'How does it reach you?', name: 'Notifications', kind: 'rows',
      aliases: 'notifications push badge sounds quiet hours', fold: 'fold-notify',
      rows: [['Title badge', 'on'], ['Browser notifications', 'on'], ['Web push', 'on'], ['Quiet hours', '22:00 – 07:00']] },
    { q: 'see', section: 'What may it see and touch?', name: 'Computer use', kind: 'rows',
      aliases: 'computer use display screenshot recording framerate backend', fold: 'fold-cu',
      rows: [['Backend', 'auto (SCK)'], ['Recording', 'on · 5 fps · high'], ['Your screen', 'always ask first']] },
    { q: 'feel', section: 'How should it look and feel?', name: 'Theme & density', kind: 'appearance',
      aliases: 'theme dark light density cozy studio appearance', fold: 'fold-appearance' },
    { q: 'fine', section: 'The fine print', name: 'Env overrides', kind: 'rows',
      aliases: 'env environment variables overrides debug', fold: 'fold-env',
      rows: [['INTENDANT_MOCK_DISPLAY', '— (unset)'], ['RUST_LOG', 'info']] },
    { q: 'fine', section: 'The fine print', name: 'Raw intendant.toml', kind: 'toml',
      aliases: 'toml raw config file edit', fold: 'fold-toml' }
  ],

  /* ------------------------------------------------ session log sample - */
  fixLoginLog: [
    ['09:41:03', 'sys', 'task received — fix the OAuth redirect loop on staging'],
    ['09:41:31', 'tool', 'Read src/auth/callback.ts (214 lines)'],
    ['09:42:02', 'tool', 'Grep "SameSite" across src/ — 6 hits'],
    ['09:44:20', 'note', 'found it: session cookie is SameSite=Lax; provider bounces through a cross-site POST'],
    ['09:46:11', 'tool', 'Edit src/auth/session.ts — SameSite=None; Secure for the OAuth handshake'],
    ['09:48:55', 'tool', 'Edit src/middleware/redirect.ts — drop the loop-back on 302 chains'],
    ['09:52:14', 'ok', 'npm test — auth suite green (41/41)'],
    ['10:01:37', 'ask', 'wants to delete 3 regenerated CSV dumps in exports/'],
    ['10:03:12', 'ask', 'which callback URL should win? /auth/callback or /callback'],
    ['10:07:44', 'doorbell', 'asking to see your screen — the staging error page you left open']
  ],

  /* ------------------------------------------------ diff sample -------- */
  sampleDiff: [
    ['hunk', '@@ src/auth/session.ts:41 @@'],
    ['ctx', '  export function sessionCookie(req) {'],
    ['del', '-   return cookie("sid", token, { sameSite: "lax", secure: true })'],
    ['add', '+   return cookie("sid", token, { sameSite: pickSameSite(req), secure: true })'],
    ['ctx', '  }'],
    ['hunk', '@@ src/middleware/redirect.ts:12 @@'],
    ['del', '-   if (seen.has(url)) return res.redirect("/login")'],
    ['add', '+   if (seen.has(url)) return next(new LoopDetected(url))'],
    ['ctx', '   seen.add(url)']
  ]
};

/* Generate the 26-week activity heat deterministically */
(function () {
  const heat = []; let seed = 42;
  for (let w = 0; w < 26 * 7; w++) {
    seed = (seed * 16807) % 2147483647;
    const r = seed % 100;
    heat.push(r < 22 ? 0 : r < 50 ? 1 : r < 72 ? 2 : r < 90 ? 3 : 4);
  }
  P.data.usage.heat = heat;
})();
