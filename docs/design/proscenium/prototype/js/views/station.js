/* Proscenium — Station: the constellation vantage.
   A mock of the WebGPU scene, drawn from the same P.data the DOM rooms use —
   its place in the house, not a second brain. Click a session node to open
   its space. No timers: all motion is CSS. */
window.P = window.P || {};
P.views = P.views || {};

P.views.station = {
  title: 'Station',

  CX: 500, CY: 280, R: 175, AR: 52,

  render(el) {
    const working = P.data.sessions.filter(s => s.phase === 'working');
    const needsYou = P.data.sessions.filter(s => s.queue && s.queue.length);

    el.innerHTML = P.page({
      eyebrow: 'vantage',
      title: 'Station',
      sub: 'The whole house at one glance — every machine, every session, and the one thing glowing for you.',
      body:
        '<div class="stn-scene">' + this.scene() + '</div>' +
        '<div class="stn-hud">' +
          P.chip(P.data.machines.length + ' hosts', 'slate', 'machines') +
          P.chip(working.length + ' working', 'sage') +
          (needsYou.length ? P.chip(needsYou.length + ' needs you', 'attn', 'doorbell') : P.chip('nothing needs you', 'sage')) +
          P.chip('tunnel live', 'violet') +
          '<span class="grow"></span>' +
          '<span class="factline">' +
            '<span class="fact">' + P.dot('sage') + ' working</span>' +
            '<span class="fact">' + P.dot('slate') + ' idle / away</span>' +
            '<span class="fact">' + P.dot('attn') + ' needs you</span>' +
          '</span>' +
        '</div>' +
        '<div class="dim stn-note">The real Station renders this scene in WebGPU and routes every interaction through the same control plane — this is its place in the house, not a second brain. Click a session to open its space.</div>'
    });

    this.wire(el);
  },

  /* deterministic PRNG so the starfield doesn't jump between renders */
  rnd: (function () { let seed = 99; return function () { seed = (seed * 16807) % 2147483647; return seed / 2147483647; }; })(),

  scene() {
    const cx = this.CX, cy = this.CY, R = this.R;
    const hostAngles = { local: -90, dell: 30, samsung: 150 };
    const at = (ox, oy, r, deg) => {
      const a = deg * Math.PI / 180;
      return [ox + r * Math.cos(a), oy + r * Math.sin(a)];
    };

    /* starfield */
    let stars = '';
    for (let i = 0; i < 130; i++) {
      const x = (this.rnd() * 1000).toFixed(1), y = (this.rnd() * 560).toFixed(1);
      const r = (0.4 + this.rnd() * 0.9).toFixed(2), o = (0.1 + this.rnd() * 0.5).toFixed(2);
      stars += '<circle class="stn-star' + (i % 9 === 0 ? ' tw' : '') + '" cx="' + x + '" cy="' + y + '" r="' + r + '" opacity="' + o + '"/>';
    }

    /* orbit rings — the two dashed ones rotate, slowly */
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

    /* hosts + their agents */
    let hosts = '';
    P.data.machines.forEach(m => {
      const pos = at(cx, cy, R, hostAngles[m.id] != null ? hostAngles[m.id] : 0);
      const hx = pos[0].toFixed(1), hy = pos[1].toFixed(1);
      const agents = P.data.sessions.filter(s => s.machine === m.id && (s.phase === 'working' || s.phase === 'idle'));
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
        const needs = s.queue && s.queue.length;
        hosts += '<g class="stn-agent" data-sess="' + P.esc(s.id) + '">' +
          '<title>' + P.esc(s.name + ' — ' + s.sentence) + '</title>' +
          (needs ? '<circle class="stn-glow" cx="' + ax + '" cy="' + ay + '" r="13"/>' : '') +
          '<circle class="stn-agent-dot ' + (s.phase === 'working' ? 'work' : 'idle') + '" cx="' + ax + '" cy="' + ay + '" r="5.5"/>' +
          '<text x="' + lx + '" y="' + ly + '" text-anchor="' + anchor + '">' + P.esc(s.name) + '</text>' +
        '</g>';
      });
      hosts += '<g class="stn-host">' +
        '<circle class="stn-host-dot' + (m.status === 'online' ? '' : ' off') + '" cx="' + hx + '" cy="' + hy + '" r="9"/>' +
        '<text x="' + hx + '" y="' + (+hy + 28) + '" class="stn-label">' + P.esc(m.petname) + '</text>' +
        '<text x="' + hx + '" y="' + (+hy + 42) + '" class="stn-sublabel">' + P.esc(m.label + ' · via ' + m.route) + '</text>' +
      '</g>';
    });

    return '<svg class="stn-svg" viewBox="0 0 1000 560" preserveAspectRatio="xMidYMid slice" role="img" aria-label="The house constellation">' +
      stars + rings + core + hosts + '</svg>';
  },

  wire(el) {
    el.querySelectorAll('.stn-agent').forEach(g =>
      g.addEventListener('click', () => P.go('#/work/session/' + g.dataset.sess)));
  }
};
