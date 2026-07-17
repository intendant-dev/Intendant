/* V3 — Home: the owner's box.
   Queue → the Conversation → Now Playing → Today.
   Every section renders from V3.data; `live(what)` re-renders on store
   events without clobbering an in-progress answer or scroll position. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.home = {
  title: 'Home',

  render(el) {
    const D = V3.data;
    const hour = new Date().getHours();
    const greet = hour < 12 ? 'Good morning' : hour < 18 ? 'Good afternoon' : 'Good evening';
    const pending = V3.queueStore.pending();
    const fyis = V3.queueStore.fyis();
    const live = D.sessions.filter(s => s.active);
    const done = D.sessions.filter(s => !s.active && (s.phase === 'done' || s.phase === 'failed')).slice(0, 3);

    el.innerHTML = V3.page({
      title: greet + (D.you.name !== 'you' ? ', ' + D.you.name : '') + '.',
      sub: D.bootError
        ? 'The line to the house failed to open: <b>' + V3.esc(D.bootError) + '</b> — retrying in the background.'
        : pending.length
          ? '<b>' + pending.length + '</b> thing' + (pending.length > 1 ? 's' : '') + ' need' + (pending.length > 1 ? '' : 's') + ' you — the rest is humming.'
          : 'Nothing needs you. The house is humming.',
      body:
        V3.section('Needs you') +
        '<div id="home-queue" class="col" style="gap:12px">' +
          (pending.length || fyis.length
            ? pending.map(q => V3.decisionCard(q)).join('') + fyis.map(q => V3.decisionCard(q)).join('')
            : '<div class="queue-free"><span class="big">You’re free.</span>' +
              V3.esc(live.length ? live.length + ' session' + (live.length === 1 ? ' is' : 's are') + ' working — I’ll tap you if anything comes up.'
                                  : 'Nothing on stage either — give the house something to do below.') + '</div>') +
        '</div>' +

        V3.section('The conversation') +
        '<div class="thread" id="home-thread">' + this.threadHtml() + '</div>' +

        V3.section('Now playing') +
        '<div class="grid grid-3">' +
          (live.map(V3.stageCard).join('') ||
            V3.empty('stage', 'Nothing on stage', 'Give the house something to do — one sentence in the composer is enough.')) +
        '</div>' +
        (done.length ? '<div class="eyebrow" style="margin-top:10px">just finished</div>' +
          '<div class="grid grid-3">' + done.map(V3.stageCard).join('') + '</div>' : '') +

        V3.section('Today') +
        this.todayHtml()
    });
    V3.mountIcons(el);
    this.wire(el);
  },

  threadHtml() {
    const conv = V3.data.conversation;
    if (!conv.length) {
      return '<div class="queue-free" style="padding:18px"><span class="big">Say something to the house.</span>' +
        'It answers in the first person, and shows its work when you ask.</div>';
    }
    return conv.slice(-60).map(m => {
      if (m.kind === 'milestone') {
        return '<div class="milestone"><span class="line"></span><span>' + V3.esc(m.text) + '</span>' +
          (m.session ? '<button class="btn btn-quiet btn-xs" data-showwork="' + V3.esc(m.session) + '">show work</button>' : '') +
          '<span class="line"></span></div>';
      }
      if (m.from === 'you') {
        return '<div class="msg msg-you"><div class="avatar">' + V3.icon('arch', 15) + '</div>' +
          '<div class="msg-body"><div class="prose"><p>' + V3.esc(m.text) + '</p></div>' +
          '<div class="msg-meta"><span>' + (m.at || '') + '</span><span>you</span></div></div></div>';
      }
      const prose = (m.prose || []).map(p => '<p>' + V3.esc(p) + '</p>').join('');
      return '<div class="msg"><div class="avatar">' + V3.icon('arch', 15) + '</div>' +
        '<div class="msg-body"><div class="prose">' + prose + '</div>' +
        '<div class="msg-meta"><span>' + (m.at || '') + '</span><span>presence · the house</span></div></div></div>';
    }).join('');
  },

  todayHtml() {
    const D = V3.data;
    const items = [];
    const now = Date.now();
    const day = 86400000;
    /* agenda items with due times + scheduled-session effects (the
       daemon's scheduler, surfaced here for the first time) */
    D.agenda.forEach(a => {
      (a.effects || []).forEach(fx => {
        const when = (fx.manifest && (fx.manifest.fire_at_ms || fx.manifest.next_ms)) || fx.proposed_ms;
        /* approval state = presence of a matching approval digest (AgendaApproval) */
        const approved = !!(fx.approval && fx.approval.digest && fx.approval.digest === fx.digest);
        items.push({
          t: when, kind: approved ? 'scheduled' : 'proposed',
          label: (fx.manifest && fx.manifest.goal) || a.title || 'scheduled session',
          fxId: fx.effect_id, digest: fx.digest, approved, itemId: a.id
        });
      });
      if (a.due_ms) items.push({ t: a.due_ms, kind: 'reminder', label: a.title, itemId: a.id });
    });
    items.sort((a, b) => (a.t || now) - (b.t || now));
    const upcoming = items.filter(i => !i.t || i.t > now - day).slice(0, 10);

    let html = '<div class="ribbon">';
    html += '<div class="ribbon-item now"><span class="t">now</span><span>' +
      (V3.queueStore.pending().length ? V3.queueStore.pending().length + ' things need you' : 'all clear') + '</span></div>';
    html += upcoming.map(i =>
      '<div class="ribbon-item' + (i.kind === 'proposed' ? ' attn' : '') + '" ' + (i.fxId ? 'data-fx="' + i.itemId + ':' + i.fxId + ':' + (i.digest || '') + ':' + i.approved + '"' : '') + '>' +
      '<span class="t">' + (i.t ? V3.esc(V3.clock(i.t)) : '—') + '</span><span>' + V3.esc(i.label) + '</span>' +
      (i.kind === 'proposed' ? '<span class="dim" style="font-size:11px">scheduled session — tap to approve</span>'
        : i.kind === 'scheduled' ? '<span class="dim" style="font-size:11px">scheduled</span>' : '') +
      '</div>').join('');
    html += '</div>';
    if (!upcoming.length) {
      html += '<div class="dim" style="font-size:12.5px;margin-top:-4px">Nothing scheduled. Park something in Books → The List and give it a time — the daemon’s scheduler fires it.</div>';
    }
    return html;
  },

  wire(el) {
    el.querySelectorAll('[data-showwork]').forEach(b => b.addEventListener('click', function () {
      const ms = this.closest('.milestone');
      if (ms.nextElementSibling && ms.nextElementSibling.classList.contains('panel')) { ms.nextElementSibling.remove(); this.textContent = 'show work'; return; }
      const lines = (V3.data.logs[this.dataset.showwork] || []).slice(-8);
      const w = V3.h('div', 'panel', lines.length
        ? V3.logLines(lines.map(l => [l.t, l.kind, l.text]))
        : '<div class="dim" style="font-size:12px">No work lines replayed for this session yet.</div>');
      ms.after(w); this.textContent = 'hide work';
    }));
    el.querySelectorAll('[data-fx]').forEach(card => card.addEventListener('click', () => {
      const [itemId, fxId, digest, approved] = card.dataset.fx.split(':');
      if (approved === 'true') return;
      V3.actions.agendaOp({ op: 'approve_effect', id: itemId, digest })
        .then(ok => { if (ok) V3.toast('Scheduled — the daemon fires it', 'sage'); });
    }));
  },

  live(what) {
    if (!['queue', 'sessions', 'conversation', 'agenda', 'ready', 'conn', 'logs:main'].includes(what)) return;
    const active = document.activeElement;
    if (active && document.getElementById('main').contains(active) && /INPUT|TEXTAREA/.test(active.tagName)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
    if (what === 'conversation') main.scrollTop = main.scrollHeight;
  }
};

V3.clock = function (ms) {
  const d = new Date(ms);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const hm = String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
  if (sameDay) return hm;
  return d.toLocaleDateString(undefined, { weekday: 'short' }) + ' ' + hm;
};
