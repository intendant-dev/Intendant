/* V3 — Station: the constellation vantage.
   The whole house at one glance, drawn from V3.data — machines on the ring,
   sessions as their satellites, attention glowing. All motion is CSS (the
   rings and glow are animations), so no timer ever outlives the view; the
   store events re-render the scene through live(). Click an agent node to
   open its space. The real WebGPU canvas renders on the classic dashboard. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.station = {
  title: 'Station',

  CX: 500, CY: 280, R: 175, AR: 52,

  render(el) {
    /* timer hygiene — this view holds none by design; keep the invariant explicit */
    if (this._timer) { clearInterval(this._timer); this._timer = null; }

    const D = V3.data;
    const working = D.sessions.filter(s => s.active);
    const needsYou = D.sessions.filter(s =>
      V3.data.queue.some(q => q.kind !== 'fyi' && q.session === s.id));
    const conn = V3.transport.state;

    el.innerHTML = V3.page({
      eyebrow: 'vantage',
      title: 'Station',
      sub: 'The whole house at one glance — every machine, every session, and the one thing glowing for you.',
      body:
        '<div class="stn-scene">' + this.scene() + '</div>' +
        '<div class="stn-hud">' +
          V3.chip(D.machines.length + ' host' + (D.machines.length === 1 ? '' : 's'), 'slate', 'machines') +
          V3.chip(working.length + ' working', 'sage') +
          (needsYou.length ? V3.chip(needsYou.length + ' need' + (needsYou.length === 1 ? 's' : '') + ' you', 'attn', 'doorbell')
                           : V3.chip('nothing needs you', 'sage')) +
          V3.chip(conn === 'live' ? 'ws live' : conn === 'connecting' ? 'connecting…' : 'offline',
                  conn === 'live' ? 'sage' : conn === 'connecting' ? 'attn' : 'brick') +
          '<span class="grow"></span>' +
          '<span class="factline">' +
            '<span class="fact">' + V3.dot('sage') + ' working</span>' +
            '<span class="fact">' + V3.dot('slate') + ' idle / away</span>' +
            '<span class="fact">' + V3.dot('attn') + ' needs you</span>' +
          '</span>' +
        '</div>' +
        V3.card({
          title: 'The full canvas', sub: 'WebGPU, on the classic dashboard',
          body:
            '<p style="margin:0 0 10px;font-size:13.5px">The real Station renders this scene in WebGPU — free camera, tile streams, the works. It lives on <a href="/">the classic dashboard</a> for now: same control plane, same trust, nothing here is a second brain.</p>' +
            '<a class="btn btn-quiet btn-xs" href="/">' + V3.ICON('external', 13) + ' open the classic Station</a>'
        })
    });

    this.wire(el);
  },

  scene() {
    const cx = this.CX, cy = this.CY, R = this.R;
    const at = (ox, oy, r, deg) => {
      const a = deg * Math.PI / 180;
      return [ox + r * Math.cos(a), oy + r * Math.sin(a)];
    };

    /* starfield — seeded per render so re-renders don't reshuffle the sky */
    let seed = 99;
    const rnd = () => { seed = (seed * 16807) % 2147483647; return seed / 2147483647; };
    let stars = '';
    for (let i = 0; i < 130; i++) {
      const x = (rnd() * 1000).toFixed(1), y = (rnd() * 560).toFixed(1);
      const r = (0.4 + rnd() * 0.9).toFixed(2), o = (0.1 + rnd() * 0.5).toFixed(2);
      stars += '<circle class="stn-star' + (i % 9 === 0 ? ' tw' : '') + '" cx="' + x + '" cy="' + y + '" r="' + r + '" opacity="' + o + '"/>';
    }

    const rings =
      '<g class="stn-spin"><circle class="stn-ring" cx="' + cx + '" cy="' + cy + '" r="' + R + '"/></g>' +
      '<g class="stn-spin-rev"><circle class="stn-ring" cx="' + cx + '" cy="' + cy + '" r="' + (R + 74) + '"/></g>' +
      '<circle class="stn-ring-faint" cx="' + cx + '" cy="' + cy + '" r="' + (R - 74) + '"/>';

    /* the operator core — the brass arch */
    const core =
      '<g transform="translate(' + cx + ',' + cy + ') scale(0.22) translate(-256,-256)" class="stn-core">' +
        '<path d="M128 384 L128 240 Q128 128 256 128 Q384 128 384 240 L384 384" fill="none" stroke-width="30" stroke-linecap="round"/>' +
        '<line x1="256" y1="220" x2="256" y2="330" stroke-width="30" stroke-linecap="round"/>' +
        '<line x1="236" y1="300" x2="330" y2="180" stroke-width="22" stroke-linecap="round" class="stn-core-accent"/>' +
      '</g>' +
      '<text x="' + cx + '" y="' + (cy + 44) + '" class="stn-label">the house</text>' +
      '<text x="' + cx + '" y="' + (cy + 59) + '" class="stn-sublabel">operator core · you</text>';

    /* hosts evenly spaced on the ring, this daemon at the top */
    const machines = V3.data.machines || [];
    let hosts = '';
    machines.forEach((m, mi) => {
      const deg = machines.length === 1 ? -90 : -90 + mi * (360 / machines.length);
      const pos = at(cx, cy, R, deg);
      const hx = pos[0].toFixed(1), hy = pos[1].toFixed(1);
      const agents = (V3.data.sessions || []).filter(s =>
        (s.machine || 'local') === m.id && (s.active || s.phase === 'working' || s.phase === 'idle'));
      hosts += '<line class="stn-link" x1="' + cx + '" y1="' + cy + '" x2="' + hx + '" y2="' + hy + '"/>' +
        '<circle class="stn-ring-faint" cx="' + hx + '" cy="' + hy + '" r="' + this.AR + '"/>';
      agents.forEach((s, i) => {
        const ang = 160 + i * 140;
        const ap = at(+hx, +hy, this.AR, ang);
        const ax = ap[0].toFixed(1), ay = ap[1].toFixed(1);
        /* label rides radially outward from the host so it never crosses the host's own name */
        const rad = ang * Math.PI / 180, cos = Math.cos(rad), sin = Math.sin(rad);
        const anchor = cos > 0.3 ? 'start' : cos < -0.3 ? 'end' : 'middle';
        const lx = (+ax + 11 * cos).toFixed(1), ly = (+ay + 11 * sin + 4).toFixed(1);
        const needs = V3.data.queue.some(q => q.kind !== 'fyi' && q.session === s.id);
        const tone = needs ? 'attn' : (s.active || s.phase === 'working') ? 'work' : 'idle';
        hosts += '<g class="stn-agent" data-sess="' + V3.esc(s.id) + '">' +
          '<title>' + V3.esc(s.name + ' — ' + (s.sentence || s.phase)) + '</title>' +
          (needs ? '<circle class="stn-glow" cx="' + ax + '" cy="' + ay + '" r="13"/>' : '') +
          '<circle class="stn-agent-dot ' + tone + '" cx="' + ax + '" cy="' + ay + '" r="5.5"/>' +
          '<text x="' + lx + '" y="' + ly + '" text-anchor="' + anchor + '">' + V3.esc(s.name) + '</text>' +
        '</g>';
      });
      hosts += '<g class="stn-host">' +
        '<circle class="stn-host-dot' + (m.status === 'online' ? '' : ' off') + '" cx="' + hx + '" cy="' + hy + '" r="9"/>' +
        '<text x="' + hx + '" y="' + (+hy + 28) + '" class="stn-label">' + V3.esc(m.petname) + '</text>' +
        '<text x="' + hx + '" y="' + (+hy + 42) + '" class="stn-sublabel">' + V3.esc((m.label || '') + ' · via ' + (m.route || '?')) + '</text>' +
      '</g>';
    });

    return '<svg class="stn-svg" viewBox="0 0 1000 560" preserveAspectRatio="xMidYMid slice" role="img" aria-label="The house constellation">' +
      stars + rings + core + hosts + '</svg>';
  },

  wire(el) {
    el.querySelectorAll('.stn-agent').forEach(g =>
      g.addEventListener('click', () => V3.go('#/work/session/' + g.dataset.sess)));
  },

  live(what) {
    if (!['sessions', 'queue', 'machines', 'conn', 'ready'].includes(what)) return;
    const main = document.getElementById('main');
    const y = main.scrollTop;
    this.render(main);
    main.scrollTop = y;
  }
};
