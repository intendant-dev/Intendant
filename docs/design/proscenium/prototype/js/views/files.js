/* Proscenium — Files: the desk. Whose-disk accent, an expandable tree
   and a viewer pane, one humane denial, and resumable transfers. */
window.P = window.P || {};
P.views = P.views || {};

P.views.files = {
  title: 'Files',
  machine: 'local',
  sel: 'christmas-2025.pdf',
  expanded: null,

  /* Workshop's disk — the peer's own tree, seen from here (read-only) */
  peerTree: [
    { path: '~/intendant/docs/src/', kind: 'folder', children: [
      { path: 'SUMMARY.md', kind: 'file', size: '4 KB' },
      { path: 'architecture.md', kind: 'file', size: '38 KB' },
      { path: 'peer-federation.md', kind: 'file', size: '21 KB' }
    ] },
    { path: '~/intendant/scripts/', kind: 'folder', children: [] }
  ],

  render(el) {
    if (!this.expanded) {
      this.expanded = new Set(['~/Pictures/photo-book/', '~/projects/shopify-theme/', '~/intendant/docs/src/']);
    }
    const m = P.data.machines.find(x => x.id === this.machine);
    const peer = !m.thisMachine;
    el.innerHTML = P.page({
      eyebrow: 'the desk',
      title: 'Files',
      sub: 'Files across your machines, and what is moving between them.',
      body:
        '<div class="row" style="justify-content:space-between;flex-wrap:wrap;gap:8px">' +
          '<span class="seg" id="file-machines">' +
            P.data.machines.filter(x => x.status === 'online').map(x =>
              '<button class="' + (x.id === this.machine ? 'on' : '') + '" data-machine="' + x.id + '">' +
              '<span class="dot dot-' + (x.thisMachine ? 'slate' : 'violet') + '"></span> ' + P.esc(x.petname) + '</button>').join('') +
          '</span>' +
          P.authline('you', 'owner', peer ? m.route : 'direct') +
        '</div>' +
        (peer
          ? '<div class="panel dim" style="font-size:12.5px">Browsing ' + P.esc(m.petname) +
            '’s disk from here — looking, not touching. Its own IAM decides what you may see.</div>'
          : '') +
        '<div class="file-deskgrid">' +
          '<div class="card" style="padding:10px"><div class="tree">' +
            this.treeHtml(peer ? this.peerTree : P.data.files) +
            (peer ? '' :
              '<div class="tree-row" data-file="~/etc" style="padding-left:8px">' +
                '<span class="icon">' + P.icon('folder', 12) + '</span>~/etc <span class="fact">locked</span></div>') +
          '</div></div>' +
          '<div id="file-viewer-host">' + this.viewerHtml() + '</div>' +
        '</div>' +
        P.section('Transfers') +
        this.transfersHtml()
    });
    this.wire(el);
  },

  /* ---------------- the tree ---------------- */
  treeHtml(nodes, depth) {
    depth = depth || 0;
    return nodes.map(n => {
      const short = n.path.replace(/\/$/, '').split('/').pop() + (n.kind === 'folder' ? '/' : '');
      const label = depth === 0 ? n.path : short;
      const pad = 'padding-left:' + (8 + depth * 16) + 'px';
      if (n.kind === 'folder') {
        const open = this.expanded.has(n.path);
        return '<div class="tree-row file-folder' + (open ? ' open' : '') + '" data-folder="' + P.esc(n.path) + '" style="' + pad + '">' +
          '<span class="icon file-chev">' + P.icon('chev', 12) + '</span>' +
          '<span>' + P.esc(label) + '</span>' +
          (n.note ? ' <a class="chip chip-attn" href="#/home" title="see the queue">' + P.esc(n.note) + '</a>' : '') +
          '</div>' +
          (open && n.children && n.children.length ? this.treeHtml(n.children, depth + 1) : '');
      }
      return '<div class="tree-row' + (this.sel === n.path ? ' on' : '') + '" data-file="' + P.esc(n.path) + '" style="' + pad + '">' +
        '<span class="icon">' + P.icon('file', 12) + '</span>' +
        '<span>' + P.esc(label) + '</span>' +
        (n.size ? ' <span class="fact">' + P.esc(n.size) + '</span>' : '') +
        '</div>';
    }).join('');
  },

  /* ---------------- the viewer pane ---------------- */
  viewerHtml() {
    const m = P.data.machines.find(x => x.id === this.machine);
    const accent = m.thisMachine ? 'file-accent-slate' : 'file-accent-violet';
    const sel = this.sel;
    let inner;
    if (sel === '~/etc') {
      inner = P.empty('key', 'The house may look, but not touch, here',
        'This folder sits outside every grant the house holds — ask for the key. A person can say yes; a rule cannot.',
        '<button class="btn btn-safe" id="file-request">' + P.ICON('key', 15) + ' Request access</button>');
    } else if (sel === 'christmas-2025.pdf') {
      inner =
        '<div class="row" style="gap:16px;align-items:flex-start;flex-wrap:wrap">' +
          '<div style="flex:1;min-width:220px">' +
            '<div class="row" style="gap:8px">' + P.ICON('file', 16) + '<b>christmas-2025.pdf</b></div>' +
            '<div class="factline" style="margin:10px 0">' +
              P.fact('41 MB') + P.fact('38 pages') + P.fact('exported 06:12') + P.fact('PDF/X-4') + '</div>' +
            '<div class="dim" style="font-size:12.5px;margin-bottom:12px">The print-shop export from this morning’s photo-book run.</div>' +
            '<button class="btn btn-primary btn-xs" id="file-download">' + P.ICON('download', 13) + ' Download</button>' +
          '</div>' +
          '<div class="file-page" style="flex:none;width:230px">' +
            '<div class="file-page-line file-page-head"></div>' +
            '<div class="file-page-line"></div>' +
            '<div class="file-page-line w80"></div>' +
            '<div class="file-page-line"></div>' +
            '<div class="file-page-line w60"></div>' +
            '<div class="file-page-line w80"></div>' +
            '<div class="file-page-line w40"></div>' +
          '</div>' +
        '</div>';
    } else if (sel === 'cover.png') {
      inner =
        '<div class="file-cover">' +
          '<div class="s">the christmas book</div>' +
          '<div class="rule"></div>' +
          '<div class="t">Christmas<br>2025</div>' +
          '<div class="rule"></div>' +
          '<div class="s">sixty-two photographs</div>' +
        '</div>' +
        '<div class="factline" style="justify-content:center;margin-top:12px">' +
          P.fact('cover.png') + P.fact('3.2 MB') + P.fact('exported 06:12') + '</div>';
    } else if (sel && sel.indexOf('.md') > -1) {
      inner =
        '<div class="row" style="gap:8px">' + P.ICON('file', 15) + '<b>' + P.esc(sel) + '</b></div>' +
        '<div class="panel mono" style="margin-top:10px;font-size:12px"># ' + P.esc(sel.replace('.md', '')) +
          '<br><br>…the chapter text renders here — CodeMirror in the real build.</div>' +
        '<div class="dim" style="margin-top:8px;font-size:12px">Read-only from here — edits happen on ' + P.esc(m.petname) + ' itself.</div>';
    } else {
      inner = P.empty('files', 'Pick a file', 'Choose something on the left — I’ll show it here.');
    }
    return '<div class="card ' + accent + '" id="file-viewer">' + inner + '</div>';
  },

  /* ---------------- transfers ---------------- */
  transfersHtml() {
    return '<div class="card"><div class="col" style="gap:12px">' +
      P.data.transfers.map(t => {
        const done = t.state === 'done';
        return '<div class="row" style="gap:10px;flex-wrap:wrap">' +
          P.ICON(t.dir === 'up' ? 'upload' : 'download', 15) +
          '<span style="flex:1;min-width:200px">' + P.esc(t.what) + '</span>' +
          '<span class="row" style="gap:6px;min-width:160px;flex:0 0 220px">' +
            '<span class="meter" style="flex:1"><span class="meter-fill' + (done ? '' : ' warn') +
              '" id="file-meter-' + t.id + '" style="width:' + t.pct + '%"></span></span>' +
            '<span class="fact" id="file-pct-' + t.id + '">' + t.pct + '%</span></span>' +
          '<span id="file-state-' + t.id + '">' +
            (done ? P.chip('done', 'sage') : P.chip('paused · resumable', 'attn')) + '</span>' +
          (done ? '' : '<button class="btn btn-safe btn-xs" id="file-resume-' + t.id + '">Resume</button>') +
        '</div>';
      }).join('') +
      '</div>' +
      '<div class="dim" style="margin-top:10px;font-size:12px">Transfers survive restarts — the house picks up where it left off.</div></div>';
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    const self = this;

    el.querySelectorAll('[data-machine]').forEach(b => b.addEventListener('click', () => {
      self.machine = b.dataset.machine;
      self.sel = self.machine === 'local' ? 'christmas-2025.pdf' : 'SUMMARY.md';
      P.rerender();
    }));

    el.querySelectorAll('[data-folder]').forEach(r => r.addEventListener('click', e => {
      if (e.target.closest('a')) return;
      const p = r.dataset.folder;
      if (self.expanded.has(p)) self.expanded.delete(p); else self.expanded.add(p);
      P.rerender();
    }));

    el.querySelectorAll('[data-file]').forEach(r => r.addEventListener('click', () => {
      self.sel = r.dataset.file;
      el.querySelectorAll('[data-file]').forEach(x => x.classList.toggle('on', x === r));
      const host = document.getElementById('file-viewer-host');
      host.innerHTML = self.viewerHtml();
      self.wireViewer(host);
    }));

    this.wireViewer(el);

    const resume = el.querySelector('#file-resume-t2');
    if (resume) resume.addEventListener('click', () => {
      resume.disabled = true;
      const fill = document.getElementById('file-meter-t2');
      const pct = document.getElementById('file-pct-t2');
      const state = document.getElementById('file-state-t2');
      const t = P.data.transfers.find(x => x.id === 't2');
      let p = t.pct;
      const timer = setInterval(() => {
        if (!fill.isConnected) { clearInterval(timer); return; }
        p += 1;
        if (p >= 100) {
          p = 100;
          clearInterval(timer);
          t.pct = 100; t.state = 'done';
          fill.classList.remove('warn');
          state.innerHTML = P.chip('done', 'sage');
          resume.remove();
          P.toast('Transfer finished — bank-export-q2.csv is here', 'sage');
        }
        fill.style.width = p + '%';
        pct.textContent = p + '%';
      }, 55);
    });
  },

  wireViewer(root) {
    const dl = root.querySelector('#file-download');
    if (dl) dl.addEventListener('click', () =>
      P.toast('christmas-2025.pdf would download now — 41 MB', 'sage'));
    const req = root.querySelector('#file-request');
    if (req) req.addEventListener('click', () =>
      P.toast('Access requested — you’ll be asked on the machine itself', null));
  }
};
