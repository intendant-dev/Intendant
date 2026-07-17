/* V3 — Files: the desk. This daemon's scoped filesystem as a lazy tree,
   an 8 KB peek pane, humane denials, and the resumable-transfer shelf.
   Everything rides V3.transport — with one documented exception: file
   previews need the byte lane (GET /api/fs/read answers raw bytes, not
   JSON), taken in peekBytes() with the transport's own auth headers. */
window.V3 = window.V3 || {};
V3.views = V3.views || {};

V3.views.files = {
  title: 'Files',
  rootState: 'loading',   /* loading | ok | denied | error */
  rootError: '',
  rootPath: null,         /* canonical path the daemon resolved '~' to */
  home: null,             /* the daemon's home, from the first listing */
  dirs: {},               /* path → {entries, state, error} */
  expanded: {},           /* path → true */
  sel: null,              /* selected file path */
  preview: null,          /* {path, state, text, total, size, modified, error} */
  transfers: null,        /* null = loading · 'denied' · 'error' · [jobs] */
  transfersError: '',
  _rootRequested: false,
  _transfersRequested: false,

  PREVIEW_BYTES: 8191,    /* first 8 KB, zero-based range */

  render(el) {
    this.ensureRoot();
    this.ensureTransfers();
    el.innerHTML = V3.page({
      eyebrow: 'the desk',
      title: 'Files',
      sub: 'Files on this daemon’s disk, and what is moving under them.',
      body:
        '<div class="row" style="justify-content:space-between;flex-wrap:wrap;gap:8px">' +
          '<span class="fact">' + V3.esc(this.home ? 'this daemon’s disk · ' + this.home : 'this daemon’s disk') + '</span>' +
          V3.authline(V3.data.you.name, V3.data.you.role, V3.data.you.route) +
        '</div>' +
        '<div class="file-deskgrid">' +
          '<div class="card" style="padding:10px" id="file-tree-host"><div class="tree">' +
            this.treeHtml() +
          '</div></div>' +
          '<div id="file-viewer-host">' + this.viewerHtml() + '</div>' +
        '</div>' +
        V3.section('Transfers') +
        '<div id="file-transfers-host">' + this.transfersHtml() + '</div>'
    });
    this.wire(el);
  },

  /* ---------------- the tree (lazy) ---------------- */
  ensureRoot() {
    if (this._rootRequested) return;
    this._rootRequested = true;
    V3.transport.get('/api/fs/list?path=~').then(r => {
      this.home = r.home || null;
      this.rootPath = r.path;
      this.dirs[r.path] = { entries: r.entries || [], state: 'ok' };
      this.rootState = 'ok';
      this.refreshTree();
    }).catch(e => {
      this.rootState = /403/.test(e.message) ? 'denied' : 'error';
      this.rootError = e.message;
      this.refreshTree();
    });
  },

  treeHtml() {
    if (this.rootState === 'loading') {
      return V3.skeleton(18) + '<div style="height:6px"></div>' + V3.skeleton(18, '70%') +
        '<div style="height:6px"></div>' + V3.skeleton(18, '45%');
    }
    if (this.rootState === 'denied') {
      return '<div class="dim" style="font-size:12.5px;padding:8px 4px">The house may not read here yet — ' +
        'filesystem grants live in Access on the <a href="/">classic dashboard</a>.</div>';
    }
    if (this.rootState === 'error') {
      return '<div class="dim" style="font-size:12.5px;padding:8px 4px">The listing failed: ' +
        V3.esc(this.rootError) + '</div>';
    }
    return this.dirHtml(this.rootPath, 0, '~');
  },

  dirHtml(path, depth, label) {
    const open = !!this.expanded[path];
    const pad = 'padding-left:' + (8 + depth * 16) + 'px';
    let html = '<div class="tree-row file-folder' + (open ? ' open' : '') + '" data-folder="' + V3.esc(path) + '" style="' + pad + '">' +
      '<span class="icon file-chev">' + V3.icon('chev', 12) + '</span>' +
      '<span>' + V3.esc(label) + '</span></div>';
    if (!open) return html;
    const d = this.dirs[path];
    const childPad = 'padding-left:' + (8 + (depth + 1) * 16) + 'px';
    if (!d || d.state === 'loading') {
      return html + '<div class="tree-row" style="' + childPad + '"><span class="dim" style="font-size:12px">reading…</span></div>';
    }
    if (d.state === 'denied') {
      return html + '<div class="tree-row" style="' + childPad + '"><span class="dim" style="font-size:12px">locked — no grant here</span></div>';
    }
    if (d.state === 'error') {
      return html + '<div class="tree-row" style="' + childPad + '"><span class="dim" style="font-size:12px">' + V3.esc(d.error || 'unreadable') + '</span></div>';
    }
    const visible = (d.entries || []).filter(e => !e.hidden);
    if (!visible.length) {
      return html + '<div class="tree-row" style="' + childPad + '"><span class="dim" style="font-size:12px">empty</span></div>';
    }
    return html + visible.map(e => this.entryHtml(e, depth + 1)).join('');
  },

  entryHtml(e, depth) {
    if (e.is_dir) return this.dirHtml(e.path, depth, e.name + '/');
    const pad = 'padding-left:' + (8 + depth * 16) + 'px';
    return '<div class="tree-row' + (this.sel === e.path ? ' on' : '') + '" data-file="' + V3.esc(e.path) + '" style="' + pad + '">' +
      '<span class="icon">' + V3.icon('file', 12) + '</span>' +
      '<span>' + V3.esc(e.name) + '</span>' +
      (e.is_symlink ? ' <span class="fact">link</span>' : '') +
    '</div>';
  },

  refreshTree() {
    const host = document.getElementById('file-tree-host');
    if (!host) return;
    host.innerHTML = '<div class="tree">' + this.treeHtml() + '</div>';
    this.wireTree(host);
  },

  /* ---------------- the viewer pane ---------------- */
  loadPreview(path) {
    this.preview = { path, state: 'loading' };
    this.paintViewer();
    V3.transport.get('/api/fs/stat?path=' + encodeURIComponent(path)).then(st => {
      if (!st.exists) { this.preview = { path, state: 'note', note: 'It isn’t there anymore.' }; return this.paintViewer(); }
      if (!st.is_file) { this.preview = { path, state: 'note', note: 'Not a regular file — nothing to print.' }; return this.paintViewer(); }
      if (st.readable === false) { this.preview = { path, state: 'denied', size: st.size, modified: st.modified_ms }; return this.paintViewer(); }
      this.peekBytes(path, st);
    }).catch(e => {
      this.preview = { path, state: /403/.test(e.message) ? 'denied' : 'error', error: e.message };
      this.paintViewer();
    });
  },

  /* The byte lane: GET /api/fs/read answers raw bytes (200/206), not
     JSON, so V3.transport.get can't carry it. One fetch, same origin,
     the same bearer the transport would send, capped at the first 8 KB
     with a Range header. Every other call in this view rides transport. */
  peekBytes(path, st) {
    fetch('/api/fs/read?path=' + encodeURIComponent(path), {
      headers: Object.assign({ 'Range': 'bytes=0-' + this.PREVIEW_BYTES }, V3.transport.authHeaders())
    }).then(r => {
      if (!r.ok && r.status !== 206) throw new Error('GET /api/fs/read → ' + r.status);
      const total = this.rangeTotal(r.headers.get('Content-Range'));
      const ctype = r.headers.get('Content-Type') || '';
      return r.text().then(text => ({ total, ctype, text, status: r.status }));
    }).then(res => {
      if (/^image\//.test(res.ctype)) {
        this.preview = { path, state: 'image', size: st.size, modified: st.modified_ms };
      } else if (/[\x00-\x08\x0E-\x1F]/.test(res.text)) {
        this.preview = { path, state: 'binary', size: st.size, modified: st.modified_ms };
      } else {
        this.preview = {
          path, state: 'text', text: res.text,
          total: res.total != null ? res.total : st.size,
          truncated: res.status === 206 || (st.size != null && st.size > this.PREVIEW_BYTES + 1),
          size: st.size, modified: st.modified_ms
        };
      }
      this.paintViewer();
    }).catch(e => {
      this.preview = { path, state: /403/.test(e.message) ? 'denied' : 'error', error: e.message, size: st.size, modified: st.modified_ms };
      this.paintViewer();
    });
  },

  rangeTotal(cr) {
    const m = /\/(\d+)\s*$/.exec(cr || '');
    return m ? +m[1] : null;
  },

  paintViewer() {
    const host = document.getElementById('file-viewer-host');
    if (host) host.innerHTML = this.viewerHtml();
  },

  viewerHtml() {
    const p = this.preview;
    let inner;
    if (!p) {
      inner = V3.empty('files', 'Pick a file', 'Choose something on the left — I’ll show the first page of it here.');
    } else if (p.state === 'loading') {
      inner = '<div class="col" style="gap:8px">' + V3.skeleton(16, '60%') + V3.skeleton(120) + '</div>';
    } else if (p.state === 'denied') {
      inner = V3.empty('key', 'The house may not read here yet',
        'Filesystem grants live in Access on the classic dashboard — a person can say yes there; a rule cannot.',
        '<a class="btn btn-safe" href="/">' + V3.ICON('key', 15) + ' open Access →</a>');
    } else if (p.state === 'image') {
      inner = this.previewHead(p) +
        V3.empty('camera', 'A picture — not rendered in this draft',
          'The classic dashboard draws images; this room reads text.' + this.sizeWords(p));
    } else if (p.state === 'binary') {
      inner = this.previewHead(p) +
        V3.empty('file', 'Binary — nothing honest to print',
          'It isn’t text, so I won’t dump it.' + this.sizeWords(p));
    } else if (p.state === 'text') {
      inner = this.previewHead(p) +
        '<pre class="file-pre">' + V3.esc(p.text) + '</pre>' +
        (p.truncated
          ? '<div class="dim" style="margin-top:8px;font-size:12px">the first 8 KB' +
            (p.total != null ? ' of ' + this.fmtSize(p.total) : '') + ' — the rest stays on disk.</div>'
          : '');
    } else if (p.state === 'note') {
      inner = V3.empty('file', 'Nothing to show', p.note || '');
    } else {
      inner = V3.empty('warn', 'The read failed', p.error || 'the daemon said no');
    }
    return '<div class="card file-accent-slate" id="file-viewer">' + inner + '</div>';
  },

  previewHead(p) {
    const name = String(p.path).split('/').pop();
    return '<div class="row" style="gap:8px;margin-bottom:10px">' +
      V3.ICON('file', 15) + '<b>' + V3.esc(name) + '</b><span class="grow"></span>' +
      (p.size != null ? V3.fact(this.fmtSize(p.size)) : '') +
      (p.modified ? V3.fact('modified ' + V3.norm.ago(p.modified)) : '') +
    '</div>';
  },

  sizeWords(p) {
    return p.size != null ? ' It’s ' + this.fmtSize(p.size) + ' on disk.' : '';
  },

  fmtSize(bytes) {
    const b = +bytes;
    if (!(b >= 0)) return '—';
    if (b < 1024) return b + ' B';
    if (b < 1024 * 1024) return (b / 1024).toFixed(b < 10240 ? 1 : 0) + ' KB';
    if (b < 1024 * 1024 * 1024) return (b / (1024 * 1024)).toFixed(1) + ' MB';
    return (b / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
  },

  /* ---------------- transfers ---------------- */
  ensureTransfers() {
    if (this._transfersRequested) return;
    this._transfersRequested = true;
    V3.transport.get('/api/transfers').then(r => {
      this.transfers = (r && r.jobs) || [];
    }).catch(e => {
      this.transfers = /403/.test(e.message) ? 'denied' : 'error';
      this.transfersError = e.message;
    }).finally(() => {
      const host = document.getElementById('file-transfers-host');
      if (host) host.innerHTML = this.transfersHtml();
    });
  },

  transfersHtml() {
    if (this.transfers === null) {
      return '<div class="card">' + V3.skeleton(18) + '<div style="height:8px"></div>' + V3.skeleton(18, '65%') + '</div>';
    }
    if (this.transfers === 'denied') {
      return V3.card({ title: 'Transfers',
        body: '<div class="dim" style="font-size:12.5px">The daemon keeps this shelf closed to you — its IAM gates the transfer list.</div>' });
    }
    if (this.transfers === 'error') {
      return V3.card({ title: 'Transfers',
        body: '<div class="dim" style="font-size:12.5px">The shelf didn’t answer: ' + V3.esc(this.transfersError) + '</div>' });
    }
    if (!this.transfers.length) {
      return V3.empty('upload', 'Nothing moving right now',
        'When files move between you, the house, and peers, they land here — and they’re resumable: the house picks up where it left off after a restart.');
    }
    return '<div class="card"><div class="col" style="gap:12px">' +
        this.transfers.map(t => this.transferRow(t)).join('') +
      '</div>' +
      '<div class="dim" style="margin-top:10px;font-size:12px">Transfers survive restarts — the house picks up where it left off.</div></div>';
  },

  transferRow(t) {
    const done = t.status === 'completed';
    const total = t.total_size || 0;
    const pct = total ? Math.min(100, Math.round(100 * (t.completed_bytes || 0) / total)) : (done ? 100 : 0);
    const leaf = p => p ? String(p).split('/').filter(Boolean).pop() : null;
    const name = t.original_name || t.filename || t.source_label || leaf(t.final_path) || leaf(t.destination_path) || leaf(t.source_path) || t.id;
    const chipFor = {
      completed: ['done', 'sage'], running: ['moving', 'slate'], queued: ['queued', 'slate'],
      paused: ['paused · resumable', 'attn'], ready: ['ready', 'sage'],
      failed: ['failed', 'brick'], cancelled: ['cancelled', 'slate']
    }[t.status] || [t.status || '—', 'slate'];
    return '<div class="row" style="gap:10px;flex-wrap:wrap">' +
      V3.ICON(t.kind === 'upload' ? 'upload' : 'download', 15) +
      '<span style="flex:1;min-width:200px">' + V3.esc(name) + '</span>' +
      '<span class="row" style="gap:6px;min-width:160px;flex:0 0 220px">' +
        V3.meter(pct, done ? '' : 'warn') +
        '<span class="fact">' + pct + '%</span></span>' +
      V3.chip(chipFor[0], chipFor[1]) +
      (t.status === 'failed' && t.error ? '<span class="fact" title="' + V3.esc(t.error) + '">why</span>' : '') +
    '</div>';
  },

  /* ---------------- wiring ---------------- */
  wire(el) {
    this.wireTree(el);
  },

  wireTree(root) {
    const self = this;
    root.querySelectorAll('[data-folder]').forEach(r => r.addEventListener('click', () => {
      const p = r.dataset.folder;
      if (self.expanded[p]) { delete self.expanded[p]; self.refreshTree(); return; }
      self.expanded[p] = true;
      if (!self.dirs[p]) {
        self.dirs[p] = { entries: [], state: 'loading' };
        V3.transport.get('/api/fs/list?path=' + encodeURIComponent(p)).then(res => {
          self.dirs[p] = { entries: res.entries || [], state: 'ok' };
        }).catch(err => {
          self.dirs[p] = { entries: [], state: /403/.test(err.message) ? 'denied' : 'error', error: err.message };
        }).finally(() => self.refreshTree());
      }
      self.refreshTree();
    }));
    root.querySelectorAll('[data-file]').forEach(r => r.addEventListener('click', () => {
      self.sel = r.dataset.file;
      document.querySelectorAll('#file-tree-host [data-file]').forEach(x => x.classList.toggle('on', x === r));
      self.loadPreview(self.sel);
    }));
  }
};
