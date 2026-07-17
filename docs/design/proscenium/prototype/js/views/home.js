/* Proscenium — Home: the owner's box.
   Queue → the Conversation → Now Playing → Today. */
window.P = window.P || {};
P.views = P.views || {};

P.views.home = {
  title: 'Home',

  render(el) {
    const hour = new Date().getHours();
    const greet = hour < 12 ? 'Good morning' : hour < 18 ? 'Good afternoon' : 'Good evening';
    const pending = P.queueStore.pending();
    const fyis = P.queueStore.fyis();
    const live = P.data.sessions.filter(s => s.phase === 'working');

    el.innerHTML = P.page({
      title: greet + ', ' + P.data.you.name + '.',
      sub: pending.length
        ? '<b>' + pending.length + '</b> thing' + (pending.length > 1 ? 's' : '') + ' need' + (pending.length > 1 ? '' : 's') + ' you — the rest is humming.'
        : 'Nothing needs you. The house is humming.',
      body:
        /* ---------- the Queue ---------- */
        P.section('Needs you') +
        '<div id="home-queue" class="col" style="gap:12px">' +
          (pending.length || fyis.length
            ? pending.map(q => P.decisionCard(q)).join('') +
              (fyis.length ? fyis.map(q => P.decisionCard(q)).join('') : '')
            : '<div class="queue-free"><span class="big">You’re free.</span>' +
              P.esc(live.length + ' session' + (live.length === 1 ? ' is' : 's are') + ' working — I’ll tap you if anything comes up.') + '</div>') +
        '</div>' +

        /* ---------- the Conversation ---------- */
        P.section('The conversation') +
        '<div class="thread" id="home-thread">' + P.views.home.threadHtml() + '</div>' +

        /* ---------- Now Playing ---------- */
        P.section('Now playing') +
        '<div class="grid grid-3" id="now-playing">' +
          (live.map(P.stageCard).join('') ||
            P.empty('stage', 'Nothing on stage', 'Give the house something to do — one sentence in the composer is enough.')) +
        '</div>' +

        /* ---------- Today ---------- */
        P.section('Today') +
        '<div class="ribbon">' + P.data.today.map(t =>
          '<div class="ribbon-item' + (t.kind === 'now' ? ' now' : '') + '">' +
          '<span class="t">' + P.esc(t.t) + '</span><span>' + P.esc(t.label) + '</span>' +
          (t.note ? '<span class="dim" style="font-size:11px">' + P.esc(t.note) + '</span>' : '') +
          '</div>').join('') + '</div>'
    });

    P.views.home.wireThread(el);
  },

  threadHtml() {
    return P.data.conversation.map(m => {
      if (m.kind === 'milestone') {
        return '<div class="milestone"><span class="line"></span><span>' + P.esc(m.text) + '</span>' +
          '<button class="btn btn-quiet btn-xs" data-showwork="' + P.esc(m.detail || '') + '">show work</button>' +
          '<span class="line"></span></div>';
      }
      if (m.from === 'you') {
        return '<div class="msg msg-you"><div class="avatar">' + P.icon('arch', 15) + '</div>' +
          '<div class="msg-body"><div class="prose"><p>' + P.esc(m.text) + '</p></div>' +
          '<div class="msg-meta"><span>' + m.at + '</span><span>you</span></div></div></div>';
      }
      const prose = (m.prose || []).map(p =>
        '<p>' + P.esc(p).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>') + '</p>').join('');
      const artifacts = (m.artifacts || []).map(a =>
        '<div class="artifact"><div class="artifact-head">' + P.ICON('branch', 14) + P.esc(a.title) +
        '<span class="grow"></span><span class="fact">' + P.esc(a.stat) + '</span></div>' +
        '<div class="col" style="gap:3px">' + a.files.map(f => '<span class="mono" style="font-size:12px">' + P.esc(f) + '</span>').join('') + '</div>' +
        '<div style="margin-top:8px"><a class="chip chip-slate" href="#/' + a.link + '">open in ' + a.link + ' →</a></div></div>'
      ).join('');
      const badge = m.kind === 'briefing' ? P.chip('the briefing', 'brass', 'sparkle') + ' ' : '';
      return '<div class="msg"><div class="avatar">' + P.icon('arch', 15) + '</div>' +
        '<div class="msg-body">' + badge + '<div class="prose">' + prose + '</div>' + artifacts +
        '<div class="msg-meta"><span>' + m.at + '</span><span>presence · the house</span></div></div></div>';
    }).join('');
  },

  wireThread(el) {
    el.querySelectorAll('[data-showwork]').forEach(b => b.addEventListener('click', function () {
      const ms = this.closest('.milestone');
      if (ms.nextElementSibling && ms.nextElementSibling.classList.contains('panel')) { ms.nextElementSibling.remove(); this.textContent = 'show work'; return; }
      const w = P.h('div', 'panel', P.logLines(P.data.fixLoginLog.slice(0, 5)));
      ms.after(w); this.textContent = 'hide work';
    }));
  },

  /* The composer talks here (called by app.js on send) */
  onSend(text) {
    const t = text.toLowerCase();
    P.data.conversation.push({ from: 'you', at: 'now', text: text });
    let reply;
    if (t.includes('cover') || t.includes('photo') || t.includes('book')) {
      reply = {
        from: 'presence', at: 'now',
        prose: ['Here it is — the book and the cover, fresh from this morning. The print-ready PDF is the **PDF/X-4** variant, as the shop asked.'],
        artifacts: [{ type: 'files', title: 'photo-book — exported', stat: '2 files', files: ['christmas-2025.pdf (41 MB)', 'cover.png'], link: 'files' }]
      };
    } else if (t.includes('approve') || t.includes('allow') || t.includes('yes')) {
      const top = P.queueStore.pending()[0];
      reply = { from: 'presence', at: 'now', prose: top ? ['Done — I’ve resolved “' + top.title + '”. ' + (P.queueStore.pending().length - 1 || 'No') + ' left in the queue.'] : ['Nothing waiting on you — the queue is clear.'] };
      if (top) setTimeout(() => P.queueStore.resolve(top.id, top.actions[0].id), 400);
    } else if (t.includes('queue') || t.includes('need')) {
      const n = P.queueStore.pending().length;
      reply = { from: 'presence', at: 'now', prose: [n ? n + ' things need you — the deletion ask is the one to read. They’re pinned above.' : 'Nothing needs you. You’re free.'] };
    } else {
      /* a new task — the house takes it on */
      const id = 'task-' + Math.random().toString(36).slice(2, 6);
      P.data.sessions.unshift({
        id: id, name: text.split(' ').slice(0, 2).join('-').toLowerCase().replace(/[^a-z-]/g, '') || id,
        backend: 'internal', model: 'orchestrate', machine: 'local', phase: 'working', turn: 1,
        sentence: 'Starting up — reading the brief', task: text, started: 'now', cost: 0.00,
        tokens: { used: 1200, ctx: 200000, pct: 1 }, branch: 'main', dirty: 0,
        cache: { hit: 0 }, limits: {}, queue: [],
        caps: { follow_up: true, steer: true, interrupt: true, thread_actions: false }, subagents: []
      });
      reply = { from: 'presence', at: 'now', prose: ['I’ll take that on. I’m spinning up a session now — it’s on stage under **Now playing**, and I’ll narrate as it goes. Anything it can’t decide, it’ll raise in the queue.'] };
      P.data.conversation.push({ kind: 'milestone', text: 'task received — session starting', detail: 'log' });
      setTimeout(() => { if (P.current === 'home') P.rerender(); }, 1800);
    }
    P.data.conversation.push(reply);
    if (P.current === 'home') P.rerender();
  }
};
