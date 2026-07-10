// ── Files tab: editor ──
//
// A small IDE over the fs API family. Reads ride GET /api/fs/* (or the
// api_fs_* tunnel methods), writes ride POST /api/fs/write (or api_fs_write
// upload frames). Peers are reached over their dashboard-control tunnel and
// enforce their own IAM profile + filesystem write roots server-side; this
// UI only decides *where* to send a request, never *whether* it is allowed.
// Saves are optimistic-concurrency: every read keeps the content sha256 and
// every save sends it back as expected_sha256; a 409 opens the
// reload/overwrite banner instead of clobbering.

function switchFilesSubtab(name) {
  if (!VALID_FILES_SUBTABS.includes(name)) name = 'editor';
  activeFilesSubtab = name;
  document.querySelectorAll('#tab-files .subtab-btn[data-files-tab]').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.filesTab === name);
  });
  document.querySelectorAll('#tab-files .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `files-pane-${name}`);
  });
  if (name === 'editor') filesIdeOnTabShown();
}

// The machine tint: one accent answers "whose disk is this?" — blue for
// this daemon, mauve for a peer (ui-v2 maps the same rule onto the design
// palette: sky = this daemon, violet = peer). Applied to the active editor
// tab, tree selection, and the statusbar host chip via --files-accent.
function filesIdeApplyAccent(hostId) {
  const ui2 = typeof ui2Enabled === 'function' && ui2Enabled();
  const accent = hostId
    ? (ui2 ? 'var(--violet)' : 'var(--mauve)')
    : (ui2 ? 'var(--sky)' : 'var(--blue)');
  document.getElementById('tab-files')?.style.setProperty('--files-accent', accent);
}

function filesIdeSelectedHostId() {
  return document.getElementById('files-ide-host')?.value.trim() || '';
}

function filesIdeHostLabel(hostId) {
  if (!hostId) return 'This daemon';
  const peer = daemons.find(d => d.host_id === hostId);
  return peer?.label || hostId;
}

function filesIdeBufferKey(hostId, path) {
  // The separator must never occur in a host id and must survive the
  // innerHTML → dataset.key round-trip in filesIdeRenderTabs — an HTML
  // attribute value cannot hold a NUL (the parser substitutes U+FFFD).
  return `${hostId || ''}\uFFFD${path}`;
}

function filesIdeTreeState(hostId) {
  const key = hostId || '';
  let state = filesIdeTreeStates.get(key);
  if (!state) {
    state = {
      root: '',
      rootParent: null,
      listings: new Map(), // dir path -> {entries, truncated}
      expanded: new Set(),
      showHidden: false,
      contextDir: '',
      creating: null, // {kind: 'file'|'folder', dir}
      renaming: null, // {path, dir, name, isDir}
      deleteArming: null, // {path, recursive, timer}
      focusedPath: '', // roving-tabindex row (WAI-ARIA tree keyboard pattern)
    };
    filesIdeTreeStates.set(key, state);
  }
  return state;
}

// -- editor library (vendored CodeMirror bundle, lazy-loaded on first use)

function filesIdeEnsureEditorLib() {
  if (window.CodeMirror) return Promise.resolve();
  if (filesIdeLibPromise) return filesIdeLibPromise;
  filesIdeLibPromise = new Promise((resolve, reject) => {
    const fail = why => {
      filesIdeLibPromise = null;
      reject(new Error(why));
    };
    const css = document.createElement('link');
    css.rel = 'stylesheet';
    css.href = '/codemirror-bundle.css';
    css.onerror = () => fail('Editor stylesheet failed to load');
    document.head.appendChild(css);
    const script = document.createElement('script');
    script.src = '/codemirror-bundle.js';
    script.onload = () => (window.CodeMirror ? resolve() : fail('Editor library loaded without CodeMirror'));
    script.onerror = () => fail('Editor library failed to load');
    document.head.appendChild(script);
  });
  return filesIdeLibPromise;
}

// -- transport: one call surface for "this daemon" (HTTP or connect tunnel)
//    and peers (their dashboard-control tunnel)

function filesIdeNormalizePayload(payload) {
  const rawStatus = Number(payload?._httpStatus);
  const status = Number.isFinite(rawStatus) && rawStatus >= 100 && rawStatus <= 599 ? rawStatus : 200;
  const ok = typeof payload?._httpOk === 'boolean' ? payload._httpOk : status >= 200 && status < 300;
  let body = payload;
  if (body && typeof body === 'object' && !Array.isArray(body)) {
    body = { ...body };
    delete body._httpStatus;
    delete body._httpOk;
  }
  return { ok, status, body: body || {} };
}

async function filesIdePeerConnection(hostId) {
  const conn = await peerDashboardControlConnectionForHost(hostId, { timeoutMs: 30000 });
  if (!conn) throw new Error('Peer tunnel unavailable');
  return conn;
}

async function filesIdeRpc(hostId, method, params, httpFallback) {
  if (hostId) {
    const conn = await filesIdePeerConnection(hostId);
    const payload = await conn.request(method, params);
    return filesIdeNormalizePayload(payload);
  }
  const resp = await dashboardJsonFetch(method, params, httpFallback, method);
  const body = await resp.json().catch(() => ({}));
  return { ok: resp.ok, status: resp.status, body: body || {} };
}

function filesIdeList(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_list', { path }, () =>
    authedFetch('/api/fs/list?path=' + encodeURIComponent(path))
  );
}

function filesIdeStat(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_stat', { path }, () =>
    authedFetch('/api/fs/stat?path=' + encodeURIComponent(path))
  );
}

function filesIdeMkdir(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_mkdir', { path }, () =>
    authedFetch('/api/fs/mkdir', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    })
  );
}

function filesIdeRename(hostId, from, to) {
  return filesIdeRpc(hostId, 'api_fs_rename', { from, to }, () =>
    authedFetch('/api/fs/rename', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ from, to }),
    })
  );
}

function filesIdeDeleteRpc(hostId, path, recursive) {
  const params = recursive ? { path, recursive: true } : { path };
  return filesIdeRpc(hostId, 'api_fs_delete', params, () =>
    authedFetch('/api/fs/delete', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(params),
    })
  );
}

async function filesIdeSha256Hex(bytes) {
  try {
    if (window.crypto?.subtle) {
      const digest = await crypto.subtle.digest('SHA-256', bytes);
      return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
    }
  } catch (_) { /* fall through */ }
  return '';
}

/// Read a whole file: {bytes: Uint8Array, sha256: string}. The sha comes from
/// the daemon when it offers one (X-Content-Sha256 / result.sha256) so the
/// conflict baseline matches what the daemon will hash at save time; older
/// peers without it fall back to a local digest of the same bytes.
async function filesIdeReadFile(hostId, path) {
  if (hostId) {
    const conn = await filesIdePeerConnection(hostId);
    const result = await conn.requestBytes('api_fs_read', { path });
    if (result?.ok === false) throw new Error(result?.error || 'File read failed');
    const bytes = result?.bytes instanceof Uint8Array ? result.bytes : new Uint8Array(0);
    const sha = typeof result?.sha256 === 'string' && result.sha256
      ? result.sha256
      : await filesIdeSha256Hex(bytes);
    return { bytes, sha256: sha };
  }
  if (dashboardConnectModeEnabled()) {
    if (!dashboardByteStreamMethodAvailable('api_fs_read')) {
      throw new Error('File reads are unavailable until this dashboard reconnects');
    }
    const result = await dashboardControlTransport.requestBytes('api_fs_read', { path });
    if (result?.ok === false) throw new Error(result?.error || 'File read failed');
    const bytes = result?.bytes instanceof Uint8Array ? result.bytes : new Uint8Array(0);
    const sha = typeof result?.sha256 === 'string' && result.sha256
      ? result.sha256
      : await filesIdeSha256Hex(bytes);
    return { bytes, sha256: sha };
  }
  const resp = await authedFetch('/api/fs/read?path=' + encodeURIComponent(path));
  if (!resp.ok) {
    const detail = await resp.json().catch(() => ({}));
    throw new Error(detail.error || `File read failed (${resp.status})`);
  }
  const buffer = await resp.arrayBuffer();
  const bytes = new Uint8Array(buffer);
  const sha = resp.headers?.get?.('x-content-sha256') || (await filesIdeSha256Hex(bytes));
  return { bytes, sha256: sha };
}

/// Write a whole file. Returns the normalized {ok, status, body} response;
/// 409 bodies carry {code, current_sha256, ...} for the conflict banner.
async function filesIdeWriteFile(hostId, path, bytes, opts = {}) {
  const params = { path };
  if (opts.expected_sha256) params.expected_sha256 = opts.expected_sha256;
  if (opts.create_new) params.create_new = true;
  if (opts.force) params.force = true;
  const requestOpts = { signal: opts.signal, timeoutMs: opts.timeoutMs };
  if (hostId) {
    const conn = await filesIdePeerConnection(hostId);
    if (conn.lastStatus && conn.lastStatus.api_fs_write_available === false) {
      return {
        ok: false,
        status: 403,
        body: { error: `${filesIdeHostLabel(hostId)} grants read-only file access to this daemon` },
      };
    }
    const payload = await conn.uploadBytes('api_fs_write', params, bytes, requestOpts);
    return filesIdeNormalizePayload(payload);
  }
  if (dashboardConnectModeEnabled()) {
    if (!(dashboardTransport?.canUseRpc?.() && dashboardControlTransport?.lastStatus?.api_fs_write_available !== false)) {
      return { ok: false, status: 503, body: { error: 'File writes are unavailable until this dashboard reconnects' } };
    }
    const payload = await dashboardControlTransport.uploadBytes('api_fs_write', params, bytes, requestOpts);
    return filesIdeNormalizePayload(payload);
  }
  const resp = await authedFetch('/api/fs/write', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ ...params, content_b64: dashboardControlBytesToBase64(bytes) }),
    signal: opts.signal,
  });
  const body = await resp.json().catch(() => ({}));
  return { ok: resp.ok, status: resp.status, body: body || {} };
}

// -- tree pane

function filesIdeSetTreeStatus(kind, text) {
  const el = document.getElementById('files-ide-tree-status');
  if (!el) return;
  el.textContent = text || '';
  el.classList.toggle('error', kind === 'error');
}

async function filesIdeLoadListing(hostId, path) {
  const state = filesIdeTreeState(hostId);
  const resp = await filesIdeList(hostId, path);
  if (!resp.ok) throw new Error(resp.body.error || `Directory load failed (${resp.status})`);
  const canonical = resp.body.path || path;
  state.listings.set(canonical, {
    entries: Array.isArray(resp.body.entries) ? resp.body.entries : [],
    truncated: Boolean(resp.body.truncated),
    parent: resp.body.parent || null,
  });
  return { canonical, parent: resp.body.parent || null };
}

async function filesIdeSetRoot(rawPath) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  const target = String(rawPath || '').trim() || state.root || (hostId ? '~' : (dashboardProjectRoot || '~'));
  filesIdeSetTreeStatus('', 'Loading…');
  try {
    // A file path opens the file and roots its directory.
    const stat = await filesIdeStat(hostId, target);
    let rootPath = target;
    if (stat.ok && stat.body.exists && stat.body.is_file) {
      rootPath = stat.body.parent || target;
      filesIdeOpenFile(hostId, stat.body.path || target);
    } else if (stat.ok && stat.body.exists && stat.body.is_dir) {
      rootPath = stat.body.path || target;
    } else if (stat.ok && !stat.body.exists) {
      rootPath = stat.body.nearest_existing_parent || target;
    }
    const { canonical, parent } = await filesIdeLoadListing(hostId, rootPath);
    state.root = canonical;
    state.rootParent = parent;
    state.expanded.clear();
    state.contextDir = canonical;
    state.creating = null;
    filesIdeSetTreeStatus('', '');
    renderFilesIdeTree();
  } catch (e) {
    filesIdeSetTreeStatus('error', e.message || 'Directory load failed');
  }
}

function filesIdeTreeUp() {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  if (state.rootParent) filesIdeSetRoot(state.rootParent);
}

function filesIdeToggleHidden() {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  state.showHidden = !state.showHidden;
  const btn = document.getElementById('files-ide-hidden-btn');
  if (btn) btn.setAttribute('aria-pressed', state.showHidden ? 'true' : 'false');
  renderFilesIdeTree();
}

function filesIdeRefreshTree() {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  state.listings.clear();
  filesIdeSetRoot(state.root);
}

async function filesIdeToggleDir(path) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  state.contextDir = path;
  if (state.expanded.has(path)) {
    state.expanded.delete(path);
    renderFilesIdeTree();
    return;
  }
  try {
    if (!state.listings.has(path)) {
      filesIdeSetTreeStatus('', 'Loading…');
      await filesIdeLoadListing(hostId, path);
      filesIdeSetTreeStatus('', '');
    }
    state.expanded.add(path);
    renderFilesIdeTree();
  } catch (e) {
    filesIdeSetTreeStatus('error', e.message || 'Directory load failed');
  }
}

function filesIdeTreeRows(state, dir, depth, rows) {
  const listing = state.listings.get(dir);
  if (!listing) return;
  if (state.creating && state.creating.dir === dir) {
    rows.push({ create: true, depth });
  }
  for (const entry of listing.entries) {
    if (entry.hidden && !state.showHidden) continue;
    rows.push({ entry, depth });
    if (entry.is_dir && state.expanded.has(entry.path)) {
      const before = rows.length;
      filesIdeTreeRows(state, entry.path, depth + 1, rows);
      // A loaded-but-childless expansion renders nothing — say so (the
      // root-empty case already has this notice at the container level).
      if (rows.length === before && state.listings.get(entry.path)) {
        rows.push({ notice: 'Empty directory', depth: depth + 1 });
      }
    }
  }
  if (listing.truncated) {
    rows.push({ notice: 'Showing first 500 entries', depth });
  }
}

function renderFilesIdeTree() {
  const container = document.getElementById('files-ide-tree');
  if (!container) return;
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  const rootInput = document.getElementById('files-ide-root');
  if (rootInput && document.activeElement !== rootInput) rootInput.value = state.root;
  const upBtn = document.getElementById('files-ide-up-btn');
  if (upBtn) upBtn.disabled = !state.rootParent;
  // Rebuilding innerHTML destroys the focused row; remember whether focus
  // was inside the tree so it can be restored onto the roving row after.
  const hadFocus = container.contains(document.activeElement);
  if (!state.root) {
    container.innerHTML = '<div class="files-ide-tree-notice">Loading…</div>';
    return;
  }
  const rows = [];
  filesIdeTreeRows(state, state.root, 0, rows);
  // Roving tabindex (WAI-ARIA tree): exactly one row sits in the page tab
  // order; arrow keys move between rows (filesIdeTreeKeydown). Fall back
  // to the first row when the remembered path is no longer rendered.
  const entryPaths = rows
    .filter(row => row.entry && !(state.renaming && state.renaming.path === row.entry.path))
    .map(row => row.entry.path);
  state.focusedPath = entryPaths.includes(state.focusedPath) ? state.focusedPath : (entryPaths[0] || '');
  const html = [];
  for (const row of rows) {
    const pad = 8 + row.depth * 14;
    if (row.create) {
      const kind = state.creating.kind;
      html.push(
        `<div class="files-ide-create-row" style="padding-left:${pad}px">` +
        `<span class="files-ide-tree-caret">${kind === 'folder' ? '▸' : ''}</span>` +
        `<input id="files-ide-create-input" type="text" autocomplete="off" spellcheck="false" placeholder="${kind === 'folder' ? 'folder name' : 'file name'}">` +
        `</div>`
      );
      continue;
    }
    if (row.notice) {
      html.push(`<div class="files-ide-tree-notice" style="padding-left:${pad + 18}px">${escapeHtml(row.notice)}</div>`);
      continue;
    }
    const entry = row.entry;
    const isDir = Boolean(entry.is_dir);
    if (state.renaming && state.renaming.path === entry.path) {
      html.push(
        `<div class="files-ide-create-row" style="padding-left:${pad}px">` +
        `<span class="files-ide-tree-caret">${isDir ? '▸' : ''}</span>` +
        `<input id="files-ide-rename-input" type="text" autocomplete="off" spellcheck="false" value="${escapeHtml(state.renaming.name)}">` +
        `</div>`
      );
      continue;
    }
    const expanded = isDir && state.expanded.has(entry.path);
    const caret = isDir ? (expanded ? '▾' : '▸') : '';
    const arming = state.deleteArming && state.deleteArming.path === entry.path
      ? state.deleteArming
      : null;
    const classes = ['files-ide-tree-row', isDir ? 'dir' : 'file'];
    if (entry.hidden) classes.push('hidden-entry');
    if (!isDir && filesIdeBufferKey(hostId, entry.path) === filesIdeActiveKey) classes.push('active');
    if (isDir && state.contextDir === entry.path) classes.push('active');
    if (arming) classes.push('arming');
    const name = escapeHtml(entry.name || '');
    const pathAttr = escapeHtml(entry.path || '');
    const suffix = entry.is_symlink ? ' ⇢' : '';
    const deleteLabel = arming
      ? (arming.recursive ? 'Delete all?' : 'Delete?')
      : '✕';
    const deleteClass = arming ? 'files-ide-row-act delete armed' : 'files-ide-row-act delete';
    const deleteTitle = arming
      ? (arming.recursive ? 'Folder is not empty — click again to delete everything inside' : 'Click again to delete')
      : `Delete ${escapeHtml(entry.name || '')}`;
    // The row-action buttons are tabindex="-1" hover/pointer affordances:
    // the tree is one tab stop (roving row), and the keyboard paths to the
    // same actions are F2 (rename) and Delete on the focused row.
    html.push(
      `<div class="${classes.join(' ')}" role="treeitem" tabindex="${entry.path === state.focusedPath ? '0' : '-1'}"` +
      ` aria-level="${row.depth + 1}"${isDir ? ` aria-expanded="${expanded ? 'true' : 'false'}"` : ''}` +
      ` data-path="${pathAttr}" data-dir="${isDir ? '1' : '0'}" style="padding-left:${pad}px" title="${pathAttr}">` +
      `<span class="files-ide-tree-caret">${caret}</span>` +
      `<span class="files-ide-tree-name">${name}${suffix}</span>` +
      `<span class="files-ide-row-acts">` +
      `<button type="button" class="files-ide-row-act" data-act="rename" tabindex="-1" title="Rename ${name} (F2)" aria-label="Rename ${name}">✎</button>` +
      `<button type="button" class="${deleteClass}" data-act="delete" tabindex="-1" title="${deleteTitle}" aria-label="Delete ${name}">${deleteLabel}</button>` +
      `</span>` +
      `</div>`
    );
  }
  container.innerHTML = html.join('') || '<div class="files-ide-tree-notice">Empty directory</div>';
  container.querySelectorAll('.files-ide-tree-row').forEach(rowEl => {
    const path = rowEl.dataset.path || '';
    const isDir = rowEl.dataset.dir === '1';
    const open = () => (isDir ? filesIdeToggleDir(path) : filesIdeOpenFile(filesIdeSelectedHostId(), path));
    rowEl.addEventListener('click', ev => {
      if (ev.target.closest('.files-ide-row-act')) return;
      state.focusedPath = path; // keep the roving tab stop on the clicked row
      open();
    });
    // Keyboard activation (Enter/Space) rides the delegated container
    // listener (filesIdeTreeKeydown), not a per-row handler.
    rowEl.querySelector('[data-act="rename"]')?.addEventListener('click', () => filesIdeBeginRename(path, isDir));
    rowEl.querySelector('[data-act="delete"]')?.addEventListener('click', () => filesIdeDeleteRequested(path, isDir));
  });
  // Re-renders triggered from the keyboard (expand/collapse/arming) must
  // not dump focus onto <body>: put it back on the roving row. The create
  // and rename inputs are focused below and win when present.
  if (hadFocus) container.querySelector('.files-ide-tree-row[tabindex="0"]')?.focus();
  const createInput = document.getElementById('files-ide-create-input');
  if (createInput) {
    createInput.focus();
    createInput.addEventListener('keydown', ev => {
      if (ev.key === 'Enter') filesIdeCommitCreate(createInput.value);
      if (ev.key === 'Escape') filesIdeCancelCreate();
    });
    createInput.addEventListener('blur', () => {
      // Give Enter a beat to commit first.
      setTimeout(() => {
        const state = filesIdeTreeState(filesIdeSelectedHostId());
        if (state.creating) filesIdeCancelCreate();
      }, 150);
    });
  }
  const renameInput = document.getElementById('files-ide-rename-input');
  if (renameInput) {
    renameInput.focus();
    // Preselect the stem so typing replaces the name but keeps the extension.
    const stem = renameInput.value.lastIndexOf('.');
    renameInput.setSelectionRange(0, stem > 0 ? stem : renameInput.value.length);
    renameInput.addEventListener('keydown', ev => {
      if (ev.key === 'Enter') filesIdeCommitRename(renameInput.value);
      if (ev.key === 'Escape') filesIdeCancelRename();
    });
    renameInput.addEventListener('blur', () => {
      setTimeout(() => {
        const state = filesIdeTreeState(filesIdeSelectedHostId());
        if (state.renaming) filesIdeCancelRename();
      }, 150);
    });
  }
}

// Tree keyboard support (WAI-ARIA tree pattern, pragmatic subset), as ONE
// delegated listener on the container: rows are re-rendered wholesale via
// innerHTML, so per-row key listeners would cost a listener per row and a
// re-attach per render. Arrows move focus / expand / collapse, Enter and
// Space activate, Home/End jump, F2 renames and Delete deletes (the
// pointer-only row buttons are tabindex="-1"; these are their keyboard
// equivalents — Delete keeps the same two-press arming as the button).
function filesIdeTreeKeydown(ev) {
  if (ev.altKey || ev.ctrlKey || ev.metaKey) return;
  if (ev.target.closest('input, textarea')) return; // create/rename fields own their keys
  // A mouse-focused row-action button keeps its native Enter/Space
  // activation — don't preventDefault it into a row open.
  if (ev.target.closest('.files-ide-row-act')) return;
  const row = ev.target.closest('.files-ide-tree-row');
  if (!row) return;
  const rows = Array.from(document.querySelectorAll('#files-ide-tree .files-ide-tree-row'));
  const idx = rows.indexOf(row);
  if (idx < 0) return;
  const path = row.dataset.path || '';
  const isDir = row.dataset.dir === '1';
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  const level = el => Number(el.getAttribute('aria-level')) || 1;
  const focusRow = target => {
    if (!target) return;
    state.focusedPath = target.dataset.path || '';
    row.setAttribute('tabindex', '-1');
    target.setAttribute('tabindex', '0');
    target.focus();
  };
  state.focusedPath = path;
  switch (ev.key) {
    case 'ArrowDown':
      focusRow(rows[idx + 1]);
      break;
    case 'ArrowUp':
      focusRow(rows[idx - 1]);
      break;
    case 'ArrowRight':
      // Collapsed dir expands (focus stays); expanded dir steps into its
      // first child; files ignore the key.
      if (!isDir) break;
      if (!state.expanded.has(path)) filesIdeToggleDir(path);
      else if (rows[idx + 1] && level(rows[idx + 1]) === level(row) + 1) focusRow(rows[idx + 1]);
      break;
    case 'ArrowLeft': {
      // Expanded dir collapses; otherwise climb to the parent row.
      if (isDir && state.expanded.has(path)) {
        filesIdeToggleDir(path);
        break;
      }
      let parent = null;
      for (let i = idx - 1; i >= 0 && !parent; i--) {
        if (level(rows[i]) < level(row)) parent = rows[i];
      }
      focusRow(parent);
      break;
    }
    case 'Home':
      focusRow(rows[0]);
      break;
    case 'End':
      focusRow(rows[rows.length - 1]);
      break;
    case 'Enter':
    case ' ':
      if (isDir) filesIdeToggleDir(path);
      else filesIdeOpenFile(filesIdeSelectedHostId(), path);
      break;
    case 'F2':
      filesIdeBeginRename(path, isDir);
      break;
    case 'Delete':
      filesIdeDeleteRequested(path, isDir);
      break;
    default:
      return; // unhandled — leave the event alone
  }
  ev.preventDefault();
}

// The container survives renders (only its innerHTML is rebuilt), so this
// attaches exactly once. DOMContentLoaded per the shared-module-script
// convention for wiring.
document.addEventListener('DOMContentLoaded', () => {
  document.getElementById('files-ide-tree')?.addEventListener('keydown', filesIdeTreeKeydown);
});

function filesIdeBeginCreate(kind) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  if (!state.root) {
    showControlToast('error', 'Open a folder first');
    return;
  }
  // Create inside the last clicked directory when its listing is visible;
  // otherwise fall back to the tree root.
  const contextVisible = state.contextDir
    && state.listings.has(state.contextDir)
    && (state.contextDir === state.root || state.expanded.has(state.contextDir));
  const dir = contextVisible ? state.contextDir : state.root;
  state.creating = { kind: kind === 'folder' ? 'folder' : 'file', dir };
  renderFilesIdeTree();
}

function filesIdeCancelCreate() {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  state.creating = null;
  renderFilesIdeTree();
}

async function filesIdeCommitCreate(rawName) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  const creating = state.creating;
  const name = String(rawName || '').trim();
  state.creating = null;
  if (!creating || !name) {
    renderFilesIdeTree();
    return;
  }
  if (name.includes('/') || name.includes('\\')) {
    filesIdeSetTreeStatus('error', 'Name must not contain path separators');
    renderFilesIdeTree();
    return;
  }
  const path = creating.dir.replace(/\/+$/, '') + '/' + name;
  if (creating.kind === 'folder') {
    try {
      const resp = await filesIdeMkdir(hostId, path);
      if (!resp.ok) throw new Error(resp.body.error || `Folder create failed (${resp.status})`);
      state.listings.delete(creating.dir);
      await filesIdeLoadListing(hostId, creating.dir);
      filesIdeSetTreeStatus('', '');
    } catch (e) {
      filesIdeSetTreeStatus('error', e.message || 'Folder create failed');
    }
    renderFilesIdeTree();
    return;
  }
  renderFilesIdeTree();
  filesIdeOpenFile(hostId, path, { createNew: true });
}

function filesIdeParentDir(path) {
  const trimmed = String(path || '').replace(/\/+$/, '');
  const idx = trimmed.lastIndexOf('/');
  return idx > 0 ? trimmed.slice(0, idx) : '/';
}

function filesIdeBeginRename(path, isDir) {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  state.creating = null;
  if (state.deleteArming) {
    clearTimeout(state.deleteArming.timer);
    state.deleteArming = null;
  }
  state.renaming = {
    path,
    dir: filesIdeParentDir(path),
    name: String(path).split(/[\\/]/).filter(Boolean).pop() || path,
    isDir: Boolean(isDir),
  };
  renderFilesIdeTree();
}

function filesIdeCancelRename() {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  state.renaming = null;
  renderFilesIdeTree();
}

async function filesIdeCommitRename(rawName) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  const renaming = state.renaming;
  const name = String(rawName || '').trim();
  state.renaming = null;
  if (!renaming || !name || name === renaming.name) {
    renderFilesIdeTree();
    return;
  }
  if (name.includes('/') || name.includes('\\')) {
    filesIdeSetTreeStatus('error', 'Name must not contain path separators');
    renderFilesIdeTree();
    return;
  }
  const from = renaming.path;
  const to = renaming.dir.replace(/\/+$/, '') + '/' + name;
  try {
    const resp = await filesIdeRename(hostId, from, to);
    if (!resp.ok) throw new Error(resp.body.error || `Rename failed (${resp.status})`);
    const finalPath = typeof resp.body.path === 'string' && resp.body.path ? resp.body.path : to;
    filesIdeRetargetBuffers(hostId, from, finalPath, renaming.isDir);
    if (renaming.isDir) filesIdeDropSubtreeState(state, from);
    if (state.contextDir === from || (renaming.isDir && state.contextDir.startsWith(from + '/'))) {
      state.contextDir = renaming.dir;
    }
    state.listings.delete(renaming.dir);
    await filesIdeLoadListing(hostId, renaming.dir);
    filesIdeSetTreeStatus('', '');
  } catch (e) {
    filesIdeSetTreeStatus('error', e.message || 'Rename failed');
  }
  renderFilesIdeTree();
  filesIdeRenderTabs();
  filesIdeRenderChrome();
}

/// Two-step delete, no page-blocking confirm: first click arms the row's
/// button ("Delete?"), a second click within the window executes. A 409
/// not_empty re-arms as "Delete all?" so recursive removal is always its
/// own explicit click.
function filesIdeDeleteRequested(path, isDir) {
  const state = filesIdeTreeState(filesIdeSelectedHostId());
  const arming = state.deleteArming;
  if (arming && arming.path === path) {
    clearTimeout(arming.timer);
    state.deleteArming = null;
    filesIdeExecuteDelete(path, isDir, arming.recursive);
    return;
  }
  filesIdeArmDelete(state, path, false);
}

function filesIdeArmDelete(state, path, recursive) {
  if (state.deleteArming) clearTimeout(state.deleteArming.timer);
  const arming = { path, recursive, timer: 0 };
  arming.timer = setTimeout(() => {
    if (state.deleteArming === arming) {
      state.deleteArming = null;
      renderFilesIdeTree();
    }
  }, 3500);
  state.deleteArming = arming;
  renderFilesIdeTree();
}

async function filesIdeExecuteDelete(path, isDir, recursive) {
  const hostId = filesIdeSelectedHostId();
  const state = filesIdeTreeState(hostId);
  const dir = filesIdeParentDir(path);
  filesIdeSetTreeStatus('', 'Deleting…');
  try {
    const resp = await filesIdeDeleteRpc(hostId, path, recursive);
    if (!resp.ok) {
      if (resp.status === 409 && resp.body.code === 'not_empty') {
        filesIdeSetTreeStatus('error', 'Folder is not empty — Delete all? removes everything inside');
        filesIdeArmDelete(state, path, true);
        return;
      }
      throw new Error(resp.body.error || `Delete failed (${resp.status})`);
    }
    filesIdeOrphanOrCloseBuffers(hostId, path, isDir);
    if (isDir) filesIdeDropSubtreeState(state, path);
    if (state.contextDir === path || (isDir && state.contextDir.startsWith(path + '/'))) {
      state.contextDir = dir;
    }
    state.listings.delete(dir);
    await filesIdeLoadListing(hostId, dir);
    filesIdeSetTreeStatus('', '');
  } catch (e) {
    filesIdeSetTreeStatus('error', e.message || 'Delete failed');
  }
  renderFilesIdeTree();
}

/// Rewire open buffers after a rename: same buffer, same undo history, new
/// key/path/name. Directory renames retarget every buffer underneath.
function filesIdeRetargetBuffers(hostId, oldPath, newPath, isDir) {
  const host = hostId || '';
  const prefix = oldPath.replace(/\/+$/, '') + '/';
  const newBase = newPath.replace(/\/+$/, '');
  for (const buffer of Array.from(filesIdeBuffers.values())) {
    if ((buffer.host || '') !== host) continue;
    if (buffer.path === oldPath) {
      filesIdeRekeyBuffer(buffer, newPath);
    } else if (isDir && buffer.path.startsWith(prefix)) {
      filesIdeRekeyBuffer(buffer, newBase + '/' + buffer.path.slice(prefix.length));
    }
  }
}

function filesIdeRekeyBuffer(buffer, newPath) {
  const oldKey = buffer.key;
  const newKey = filesIdeBufferKey(buffer.host, newPath);
  buffer.key = newKey;
  buffer.path = newPath;
  buffer.name = String(newPath).split(/[\\/]/).filter(Boolean).pop() || newPath;
  // Rebuild the map with the new key in place, keeping tab order stable.
  const entries = Array.from(filesIdeBuffers.entries());
  filesIdeBuffers.clear();
  for (const [key, value] of entries) {
    filesIdeBuffers.set(key === oldKey ? newKey : key, value);
  }
  if (filesIdeActiveKey === oldKey) filesIdeActiveKey = newKey;
  // Re-highlight for the new extension: live when active, on next
  // activation otherwise (a swapped-out doc keeps its mode until then).
  const spec = filesIdeModeSpecFor(buffer.name);
  if (filesIdeActiveKey === newKey && filesIdeCm) {
    filesIdeCm.setOption('mode', spec);
  } else {
    buffer.pendingMode = { spec };
  }
}

/// After a delete, a clean buffer just closes; a dirty one stays open and
/// flips to the missing-file banner so unsaved work survives — Overwrite
/// recreates the file from the buffer.
function filesIdeOrphanOrCloseBuffers(hostId, path, isDir) {
  const host = hostId || '';
  const prefix = path.replace(/\/+$/, '') + '/';
  for (const buffer of Array.from(filesIdeBuffers.values())) {
    if ((buffer.host || '') !== host) continue;
    const hit = buffer.path === path || (isDir && buffer.path.startsWith(prefix));
    if (!hit) continue;
    if (filesIdeBufferDirty(buffer)) {
      buffer.conflict = { code: 'missing' };
      buffer.baselineSha = '';
    } else {
      filesIdeCloseTab(buffer.key, true);
    }
  }
  filesIdeRenderTabs();
  filesIdeRenderChrome();
}

// -- buffers, tabs, editor

function filesIdeActiveBuffer() {
  return filesIdeBuffers.get(filesIdeActiveKey) || null;
}

function filesIdeDetectEol(text) {
  return /\r\n/.test(text) ? '\r\n' : '\n';
}

function filesIdeModeSpecFor(name) {
  const CM = window.CodeMirror;
  if (!CM || typeof CM.findModeByFileName !== 'function') return null;
  const info = CM.findModeByFileName(String(name || ''));
  if (!info) return null;
  return info.mime && info.mime !== 'null' ? info.mime : info.mode || null;
}

async function filesIdeOpenFile(hostId, path, options = {}) {
  const key = filesIdeBufferKey(hostId, path);
  const existing = filesIdeBuffers.get(key);
  if (existing) {
    filesIdeActivate(key);
    return;
  }
  if (filesIdeBuffers.size >= FILES_IDE_MAX_TABS) {
    showControlToast('error', `Too many open files (cap is ${FILES_IDE_MAX_TABS}) — close a tab first`);
    return;
  }
  const name = String(path).split(/[\\/]/).filter(Boolean).pop() || path;
  filesIdeSetSaveStatus('', options.createNew ? '' : `Opening ${name}…`);
  try {
    await filesIdeEnsureEditorLib();
    let text = '';
    let sha = '';
    if (!options.createNew) {
      const stat = await filesIdeStat(hostId, path);
      if (stat.ok && stat.body.exists && !stat.body.is_file) {
        throw new Error(`${name} is not a regular file`);
      }
      if (stat.ok && stat.body.exists && Number(stat.body.size) > FILES_IDE_MAX_EDIT_BYTES) {
        throw new Error(`${name} is ${humanBytes(Number(stat.body.size))} — too large to edit here (cap ${humanBytes(FILES_IDE_MAX_EDIT_BYTES)}); use Downloads below`);
      }
      const read = await filesIdeReadFile(hostId, path);
      if (read.bytes.byteLength > FILES_IDE_MAX_EDIT_BYTES) {
        throw new Error(`${name} is ${humanBytes(read.bytes.byteLength)} — too large to edit here (cap ${humanBytes(FILES_IDE_MAX_EDIT_BYTES)}); use Downloads below`);
      }
      if (read.bytes.includes(0)) {
        throw new Error(`${name} looks binary — use Downloads below instead`);
      }
      try {
        text = new TextDecoder('utf-8', { fatal: true }).decode(read.bytes);
      } catch (_) {
        throw new Error(`${name} is not valid UTF-8 — use Downloads below instead`);
      }
      sha = read.sha256 || '';
    }
    const eol = filesIdeDetectEol(text);
    const doc = window.CodeMirror.Doc(text.replace(/\r\n/g, '\n'), filesIdeModeSpecFor(name));
    const buffer = {
      key,
      host: hostId || '',
      path,
      name,
      doc,
      baselineSha: sha,
      eol,
      isNew: Boolean(options.createNew),
      saving: false,
      conflict: null,
      lastError: '',
      cleanGeneration: doc.changeGeneration(),
    };
    filesIdeBuffers.set(key, buffer);
    // Keystroke-frequency handler: only touch the DOM when the dirty flag
    // actually flips (the tab strip re-render re-attaches listeners).
    doc.on('change', () => {
      const dirty = filesIdeBufferDirty(buffer);
      if (dirty !== buffer.renderedDirty) filesIdeRenderTabs();
      if (buffer.key === filesIdeActiveKey) {
        const saveBtn = document.getElementById('files-ide-save-btn');
        if (saveBtn && !buffer.saving) saveBtn.disabled = !dirty && !buffer.conflict;
        filesIdeFindOnDocChange();
      }
    });
    filesIdeSetSaveStatus('', '');
    filesIdeActivate(key);
  } catch (e) {
    filesIdeSetSaveStatus('error', e.message || 'Open failed');
    showControlToast('error', e.message || 'Open failed');
  }
}

function filesIdeBufferDirty(buffer) {
  if (!buffer) return false;
  if (buffer.isNew) return true;
  return !buffer.doc.isClean(buffer.cleanGeneration);
}

function filesIdeActivate(key) {
  const buffer = filesIdeBuffers.get(key);
  if (!buffer) return;
  if (filesIdeFindOpen && key !== filesIdeActiveKey) filesIdeCloseFind(false);
  filesIdeActiveKey = key;
  const host = document.getElementById('files-ide-editor-host');
  const empty = document.getElementById('files-ide-empty');
  if (!filesIdeCm && host && window.CodeMirror) {
    filesIdeCm = window.CodeMirror(host, {
      value: window.CodeMirror.Doc('', null),
      theme: 'intendant',
      lineNumbers: true,
      matchBrackets: true,
      autoCloseBrackets: true,
      styleActiveLine: true,
      indentUnit: 4,
      tabSize: 4,
      indentWithTabs: false,
      viewportMargin: 30,
      extraKeys: {
        'Cmd-S': () => filesIdeSaveActive(),
        'Ctrl-S': () => filesIdeSaveActive(),
        'Cmd-/': 'toggleComment',
        'Ctrl-/': 'toggleComment',
        'Cmd-F': () => filesIdeOpenFind(),
        'Ctrl-F': () => filesIdeOpenFind(),
        'Cmd-G': () => filesIdeFindStep(1),
        'Ctrl-G': () => filesIdeFindStep(1),
        'Shift-Cmd-G': () => filesIdeFindStep(-1),
        'Shift-Ctrl-G': () => filesIdeFindStep(-1),
        'F3': () => filesIdeFindStep(1),
        'Shift-F3': () => filesIdeFindStep(-1),
        Esc: () => {
          if (!filesIdeFindOpen) return window.CodeMirror.Pass;
          filesIdeCloseFind();
          return undefined;
        },
      },
    });
    // Ln/Col statusbar indicator (ui-v2 design add; span is hidden and
    // stays empty under v1).
    filesIdeCm.on('cursorActivity', filesIdeUpdateLnCol);
  }
  if (empty) empty.classList.add('hidden');
  if (filesIdeCm) {
    filesIdeCm.swapDoc(buffer.doc);
    if (buffer.pendingMode) {
      filesIdeCm.setOption('mode', buffer.pendingMode.spec);
      buffer.pendingMode = null;
    }
    requestAnimationFrame(() => {
      filesIdeCm.refresh();
      filesIdeCm.focus();
    });
  }
  filesIdeRenderTabs();
  filesIdeRenderChrome();
  renderFilesIdeTree();
}

function filesIdeCloseTab(key, confirmed) {
  const buffer = filesIdeBuffers.get(key);
  if (!buffer) return;
  if (filesIdeBufferDirty(buffer) && !confirmed) {
    // Native confirm dialogs block the page; use a two-step close instead.
    buffer.closeArmed = true;
    filesIdeRenderTabs();
    setTimeout(() => {
      const still = filesIdeBuffers.get(key);
      if (still) {
        still.closeArmed = false;
        filesIdeRenderTabs();
      }
    }, 3000);
    return;
  }
  filesIdeBuffers.delete(key);
  if (filesIdeActiveKey === key) {
    filesIdeActiveKey = '';
    const next = filesIdeBuffers.keys().next();
    if (!next.done) {
      filesIdeActivate(next.value);
      return;
    }
    if (filesIdeFindOpen) filesIdeCloseFind(false);
    if (filesIdeCm && window.CodeMirror) filesIdeCm.swapDoc(window.CodeMirror.Doc('', null));
    const empty = document.getElementById('files-ide-empty');
    if (empty) empty.classList.remove('hidden');
    filesIdeRenderChrome();
  }
  filesIdeRenderTabs();
  renderFilesIdeTree();
}

function filesIdeRenderTabs() {
  const container = document.getElementById('files-ide-tabs');
  if (!container) return;
  const html = [];
  for (const buffer of filesIdeBuffers.values()) {
    const dirty = filesIdeBufferDirty(buffer);
    buffer.renderedDirty = dirty;
    const classes = ['files-ide-tab'];
    if (buffer.key === filesIdeActiveKey) classes.push('active');
    if (dirty) classes.push('dirty');
    const hostBadge = buffer.host ? `${escapeHtml(filesIdeHostLabel(buffer.host))} · ` : '';
    const closeLabel = buffer.closeArmed ? 'Discard?' : '×';
    const closeClass = buffer.closeArmed ? 'files-ide-tab-close confirm-discard' : 'files-ide-tab-close';
    html.push(
      `<div class="${classes.join(' ')}" role="tab" data-key="${escapeHtml(buffer.key)}" title="${hostBadge}${escapeHtml(buffer.path)}">` +
      `<span class="files-ide-tab-dirty">●</span>` +
      `<span class="files-ide-tab-name">${escapeHtml(buffer.name)}</span>` +
      `<button type="button" class="${closeClass}" aria-label="Close ${escapeHtml(buffer.name)}">${closeLabel}</button>` +
      `</div>`
    );
  }
  container.innerHTML = html.join('');
  container.querySelectorAll('.files-ide-tab').forEach(tab => {
    const key = tab.dataset.key || '';
    tab.addEventListener('click', ev => {
      if (ev.target.closest('.files-ide-tab-close')) return;
      filesIdeActivate(key);
    });
    tab.querySelector('.files-ide-tab-close')?.addEventListener('click', () => {
      const buffer = filesIdeBuffers.get(key);
      filesIdeCloseTab(key, Boolean(buffer?.closeArmed));
    });
  });
}

function filesIdeSetSaveStatus(kind, text) {
  const el = document.getElementById('files-ide-save-status');
  if (!el) return;
  el.textContent = text || '';
  el.className = 'files-ide-save-status' + (kind ? ` ${kind}` : '');
}

// Ln/Col cursor indicator, ui-v2 only: the design statusbar reads
// `host · Language · LF · Ln 8, Col 24`. Under v1 the span is display:none
// and kept empty so the flex gap contributes nothing.
function filesIdeUpdateLnCol() {
  const el = document.getElementById('files-ide-status-lncol');
  if (!el) return;
  const ui2 = typeof ui2Enabled === 'function' && ui2Enabled();
  const buffer = filesIdeActiveBuffer();
  if (!ui2 || !buffer || !filesIdeCm) {
    if (el.textContent) el.textContent = '';
    return;
  }
  const pos = filesIdeCm.getCursor();
  el.textContent = `Ln ${pos.line + 1}, Col ${pos.ch + 1}`;
}

function filesIdeRenderChrome() {
  const buffer = filesIdeActiveBuffer();
  const pathEl = document.getElementById('files-ide-status-path');
  const metaEl = document.getElementById('files-ide-status-meta');
  const saveBtn = document.getElementById('files-ide-save-btn');
  const banner = document.getElementById('files-ide-banner');
  const hostChip = document.getElementById('files-ide-status-host');
  const hostLabel = document.getElementById('files-ide-status-host-label');
  // The tint and chip follow what Save would touch: the active buffer's
  // machine, or the browse target while nothing is open.
  const chipHost = buffer ? buffer.host : filesIdeSelectedHostId();
  filesIdeApplyAccent(chipHost);
  if (hostChip && hostLabel) {
    hostLabel.textContent = filesIdeHostLabel(chipHost);
    hostChip.title = chipHost ? `Editing on peer ${filesIdeHostLabel(chipHost)}` : 'Editing on this daemon';
    hostChip.classList.add('visible');
  }
  if (pathEl) {
    pathEl.textContent = buffer ? buffer.path : '';
    pathEl.title = buffer ? buffer.path : '';
  }
  if (metaEl) {
    if (buffer) {
      const CM = window.CodeMirror;
      const info = CM && CM.findModeByFileName ? CM.findModeByFileName(buffer.name) : null;
      const lang = info?.name || 'Plain text';
      const eol = buffer.eol === '\r\n' ? 'CRLF' : 'LF';
      metaEl.textContent = `${lang} · ${eol}${buffer.isNew ? ' · new file' : ''}`;
    } else {
      metaEl.textContent = '';
    }
  }
  if (saveBtn) {
    saveBtn.disabled = !buffer || buffer.saving || (!filesIdeBufferDirty(buffer) && !buffer.conflict);
    saveBtn.textContent = buffer?.saving ? 'Saving…' : 'Save';
  }
  if (banner) {
    if (buffer?.conflict) {
      const kind = buffer.conflict.code;
      const message = kind === 'missing'
        ? 'This file no longer exists on disk.'
        : kind === 'exists'
          ? 'A file with this name appeared on disk since you started.'
          : 'This file changed on disk since it was opened.';
      banner.className = 'files-ide-banner';
      banner.innerHTML =
        `<span>${escapeHtml(message)}</span>` +
        `<button type="button" class="ui-btn" onclick="filesIdeReloadActive()">Reload from disk</button>` +
        `<button type="button" class="ui-btn" onclick="filesIdeOverwriteActive()">Overwrite</button>`;
    } else if (buffer?.lastError) {
      banner.className = 'files-ide-banner error';
      banner.innerHTML = `<span>${escapeHtml(buffer.lastError)}</span>`;
    } else {
      banner.className = 'files-ide-banner hidden';
      banner.innerHTML = '';
    }
  }
  filesIdeUpdateLnCol();
}

function filesIdeSerializeBuffer(buffer) {
  let text = buffer.doc.getValue('\n');
  if (buffer.eol === '\r\n') text = text.replace(/\n/g, '\r\n');
  return new TextEncoder().encode(text);
}

async function filesIdeSaveActive(saveOptions = {}) {
  const buffer = filesIdeActiveBuffer();
  if (!buffer || buffer.saving) return;
  buffer.saving = true;
  buffer.lastError = '';
  filesIdeRenderChrome();
  filesIdeSetSaveStatus('', 'Saving…');
  try {
    const bytes = filesIdeSerializeBuffer(buffer);
    const generation = buffer.doc.changeGeneration();
    const opts = {};
    if (saveOptions.force) {
      opts.force = true;
    } else if (buffer.isNew) {
      opts.create_new = true;
    } else if (buffer.baselineSha) {
      opts.expected_sha256 = buffer.baselineSha;
    } else {
      // No baseline (e.g. a peer daemon predating read hashes): a plain save
      // must not silently clobber, so route through the conflict banner.
      buffer.conflict = { code: 'conflict' };
      throw new Error('No conflict baseline for this file — use Overwrite to save anyway');
    }
    const resp = await filesIdeWriteFile(buffer.host, buffer.path, bytes, opts);
    if (resp.status === 409) {
      buffer.conflict = { code: resp.body.code || 'conflict', currentSha: resp.body.current_sha256 || '' };
      throw new Error(resp.body.error || 'File changed on disk');
    }
    if (!resp.ok) {
      throw new Error(resp.body.error || `Save failed (${resp.status})`);
    }
    buffer.baselineSha = resp.body.sha256 || '';
    buffer.isNew = false;
    buffer.conflict = null;
    // Compare against the generation captured before serialize: keystrokes
    // that landed while the save was in flight keep the buffer dirty.
    buffer.cleanGeneration = generation;
    filesIdeSetSaveStatus('ok', `Saved ${new Date().toLocaleTimeString()}`);
    const state = filesIdeTreeState(buffer.host);
    const dir = buffer.path.slice(0, buffer.path.lastIndexOf('/')) || '/';
    if (state.listings.has(dir)) {
      filesIdeLoadListing(buffer.host, dir).then(() => renderFilesIdeTree()).catch(() => {});
    }
  } catch (e) {
    buffer.lastError = buffer.conflict ? '' : (e.message || 'Save failed');
    filesIdeSetSaveStatus('error', e.message || 'Save failed');
  } finally {
    buffer.saving = false;
    filesIdeRenderTabs();
    filesIdeRenderChrome();
  }
}

async function filesIdeReloadActive() {
  const buffer = filesIdeActiveBuffer();
  if (!buffer) return;
  try {
    filesIdeSetSaveStatus('', 'Reloading…');
    const read = await filesIdeReadFile(buffer.host, buffer.path);
    let text;
    try {
      text = new TextDecoder('utf-8', { fatal: true }).decode(read.bytes);
    } catch (_) {
      throw new Error('File is no longer valid UTF-8');
    }
    const cursor = buffer.doc.getCursor();
    buffer.eol = filesIdeDetectEol(text);
    buffer.doc.setValue(text.replace(/\r\n/g, '\n'));
    buffer.doc.setCursor(cursor);
    buffer.baselineSha = read.sha256 || '';
    buffer.isNew = false;
    buffer.conflict = null;
    buffer.lastError = '';
    buffer.cleanGeneration = buffer.doc.changeGeneration();
    filesIdeSetSaveStatus('', 'Reloaded from disk');
  } catch (e) {
    filesIdeSetSaveStatus('error', e.message || 'Reload failed');
  }
  filesIdeRenderTabs();
  filesIdeRenderChrome();
}

function filesIdeOverwriteActive() {
  const buffer = filesIdeActiveBuffer();
  if (!buffer) return Promise.resolve();
  buffer.conflict = null;
  return filesIdeSaveActive({ force: true });
}

// -- host wiring + tab lifecycle

function refreshFilesIdeHostOptions() {
  const select = document.getElementById('files-ide-host');
  if (!select) return;
  const previous = select.value || '';
  const options = [{ id: '', label: 'This daemon', connected: true }];
  for (const peer of daemons) {
    options.push({
      id: peer.host_id,
      label: peer.label || peer.host_id,
      connected: peer.connected !== false,
    });
  }
  select.innerHTML = '';
  for (const option of options) {
    const el = document.createElement('option');
    el.value = option.id;
    el.textContent = option.connected ? option.label : `${option.label} (offline)`;
    select.appendChild(el);
  }
  select.value = options.some(option => option.id === previous) ? previous : '';
  renderDashboardTargetSummary('files-ide-target-summary', filesIdeSelectedHostId(), 'files');
}

function onFilesIdeHostChanged() {
  const hostId = filesIdeSelectedHostId();
  renderDashboardTargetSummary('files-ide-target-summary', hostId, 'files');
  const state = filesIdeTreeState(hostId);
  const hiddenBtn = document.getElementById('files-ide-hidden-btn');
  if (hiddenBtn) hiddenBtn.setAttribute('aria-pressed', state.showHidden ? 'true' : 'false');
  if (state.root) {
    renderFilesIdeTree();
  } else {
    filesIdeSetRoot('');
  }
  filesIdeRenderChrome();
}

// -- find in file (vendored searchcursor addon; smart case, live count)

const FILES_IDE_FIND_MARK_CAP = 300;

function filesIdeFindInput() {
  return document.getElementById('files-ide-find-input');
}

function filesIdeOpenFind() {
  if (!filesIdeCm || !filesIdeActiveBuffer()) return;
  const bar = document.getElementById('files-ide-find');
  const input = filesIdeFindInput();
  if (!bar || !input) return;
  bar.classList.remove('hidden');
  filesIdeFindOpen = true;
  const selected = filesIdeCm.getSelection();
  if (selected && !selected.includes('\n')) input.value = selected;
  input.focus();
  input.select();
  filesIdeFindRecompute({ jump: true });
}

function filesIdeCloseFind(focusEditor = true) {
  const bar = document.getElementById('files-ide-find');
  if (bar) bar.classList.add('hidden');
  filesIdeFindOpen = false;
  filesIdeFindClearMarks();
  filesIdeFindMatches = [];
  filesIdeFindIndex = -1;
  filesIdeFindSetCount();
  if (focusEditor && filesIdeCm && filesIdeActiveBuffer()) filesIdeCm.focus();
}

function filesIdeFindClearMarks() {
  for (const mark of filesIdeFindMarks) {
    try { mark.clear(); } catch (_) { /* detached doc */ }
  }
  filesIdeFindMarks = [];
}

function filesIdeFindSetCount() {
  const el = document.getElementById('files-ide-find-count');
  if (!el) return;
  const query = filesIdeFindInput()?.value || '';
  if (!filesIdeFindOpen || !query) {
    el.textContent = '';
    el.classList.remove('none');
    return;
  }
  el.textContent = filesIdeFindMatches.length
    ? `${filesIdeFindIndex + 1} / ${filesIdeFindMatches.length}`
    : 'No matches';
  el.classList.toggle('none', !filesIdeFindMatches.length);
}

function filesIdeFindRecompute(options = {}) {
  if (!filesIdeCm || !filesIdeFindOpen) return;
  filesIdeFindClearMarks();
  filesIdeFindMatches = [];
  filesIdeFindIndex = -1;
  const query = filesIdeFindInput()?.value || '';
  if (!query) {
    filesIdeFindSetCount();
    return;
  }
  const CM = window.CodeMirror;
  // Smart case: an all-lowercase query matches case-insensitively.
  const caseFold = !/[A-Z]/.test(query);
  const cursor = filesIdeCm.getSearchCursor(query, CM.Pos(0, 0), { caseFold });
  while (cursor.findNext()) {
    filesIdeFindMatches.push({ from: cursor.from(), to: cursor.to() });
    if (filesIdeFindMatches.length >= 10000) break;
  }
  const markCount = Math.min(filesIdeFindMatches.length, FILES_IDE_FIND_MARK_CAP);
  for (let i = 0; i < markCount; i++) {
    const match = filesIdeFindMatches[i];
    filesIdeFindMarks.push(
      filesIdeCm.markText(match.from, match.to, { className: 'files-ide-find-match' })
    );
  }
  if (!filesIdeFindMatches.length) {
    filesIdeFindSetCount();
    return;
  }
  // Land on the first match at or after the editor cursor.
  const at = filesIdeCm.getCursor();
  let index = filesIdeFindMatches.findIndex(
    m => m.from.line > at.line || (m.from.line === at.line && m.from.ch >= at.ch)
  );
  if (index < 0) index = 0;
  filesIdeFindSelect(index, { scroll: options.jump !== false });
}

function filesIdeFindSelect(index, options = {}) {
  if (!filesIdeCm || !filesIdeFindMatches.length) return;
  const total = filesIdeFindMatches.length;
  filesIdeFindIndex = ((index % total) + total) % total;
  const match = filesIdeFindMatches[filesIdeFindIndex];
  filesIdeCm.setSelection(match.from, match.to);
  if (options.scroll !== false) filesIdeCm.scrollIntoView({ from: match.from, to: match.to }, 80);
  filesIdeFindSetCount();
}

function filesIdeFindStep(dir) {
  if (!filesIdeFindOpen) return;
  if (!filesIdeFindMatches.length) {
    filesIdeFindRecompute({ jump: true });
    return;
  }
  filesIdeFindSelect(filesIdeFindIndex + (dir < 0 ? -1 : 1));
}

function filesIdeFindOnInput() {
  clearTimeout(filesIdeFindTimer);
  filesIdeFindTimer = setTimeout(() => filesIdeFindRecompute({ jump: true }), 120);
}

function filesIdeFindOnDocChange() {
  if (!filesIdeFindOpen) return;
  clearTimeout(filesIdeFindTimer);
  filesIdeFindTimer = setTimeout(() => filesIdeFindRecompute({ jump: false }), 200);
}

function filesIdeOnTabShown() {
  if (!filesIdeInitialized) {
    filesIdeInitialized = true;
    refreshFilesIdeHostOptions();
    filesIdeSetRoot('');
    window.addEventListener('beforeunload', ev => {
      for (const buffer of filesIdeBuffers.values()) {
        if (filesIdeBufferDirty(buffer)) {
          ev.preventDefault();
          ev.returnValue = '';
          return;
        }
      }
    });
  } else {
    renderDashboardTargetSummary('files-ide-target-summary', filesIdeSelectedHostId(), 'files');
  }
  filesIdeRenderChrome();
  if (filesIdeCm) requestAnimationFrame(() => filesIdeCm.refresh());
}

// Multi-select state for the fs picker. Only download-source pickers opt in
// (configureFsPicker's multiSelect flag); every other target stays
// single-select. fsPickerSelectedPaths always mirrors fsPickerSelectedPath:
// single-select pickers hold [] or [fsPickerSelectedPath], multi-select
// pickers hold the selection in click order with fsPickerSelectedPath
// tracking the most recently selected entry. fsPickerAnchorPath is the
// shift-click range anchor within the current directory listing.
let fsPickerMultiSelect = false;
let fsPickerSelectedPaths = [];
let fsPickerAnchorPath = '';
let fsPickerUseLabelBase = 'Use path';

function configureFsPicker({ mode, target, title, placeholder, useLabel, showCreate, multiSelect }) {
  fsPickerMode = mode || 'directory';
  fsPickerTarget = target || 'project';
  fsPickerCurrentPath = '';
  fsPickerSelectedPath = '';
  fsPickerMultiSelect = fsPickerMode === 'file' && !!multiSelect;
  fsPickerSelectedPaths = [];
  fsPickerAnchorPath = '';
  fsPickerUseLabelBase = useLabel || 'Use path';
  fsPickerDownloadAbort = null;
  const dialog = document.querySelector('#fs-picker-modal .fs-picker-dialog');
  const titleEl = document.getElementById('fs-picker-title');
  const pathInput = document.getElementById('fs-picker-path');
  const createBtn = document.getElementById('fs-picker-create-btn');
  const useBtn = document.getElementById('fs-picker-use-btn');
  const downloadCancelBtn = document.getElementById('fs-picker-download-cancel-btn');
  if (dialog) dialog.classList.toggle('file-mode', fsPickerMode === 'file');
  if (titleEl) titleEl.textContent = title || 'Choose path';
  if (pathInput) pathInput.placeholder = placeholder || '/path';
  if (createBtn) createBtn.style.display = showCreate ? '' : 'none';
  if (downloadCancelBtn) downloadCancelBtn.classList.add('hidden');
  if (useBtn) {
    useBtn.textContent = fsPickerUseLabelBase;
    useBtn.disabled = true;
  }
}

function fsPickerMultiSelectHint() {
  const mod = /Mac|iP(hone|ad|od)/.test(navigator.platform || '') ? 'cmd' : 'ctrl';
  return `${mod}-click to select multiple`;
}

function fsPickerSelectionStatusText() {
  const count = fsPickerSelectedPaths.length;
  if (!count) return fsPickerMultiSelect ? fsPickerMultiSelectHint() : '';
  if (!fsPickerMultiSelect) return 'File selected';
  return count === 1 ? `1 file selected — ${fsPickerMultiSelectHint()}` : `${count} files selected`;
}

function fsPickerEntryIsSelected(path) {
  if (!path) return false;
  return fsPickerSelectedPaths.includes(path) || path === fsPickerSelectedPath;
}

function setFsPickerUseButtonState() {
  const useBtn = document.getElementById('fs-picker-use-btn');
  if (!useBtn) return;
  const downloading = Boolean(fsPickerDownloadAbort);
  const hasSelection = fsPickerMode === 'file'
    ? (fsPickerMultiSelect ? fsPickerSelectedPaths.length > 0 : !!fsPickerSelectedPath)
    : !!fsPickerCurrentPath;
  useBtn.disabled = downloading || !hasSelection;
  if (fsPickerMultiSelect) {
    const count = fsPickerSelectedPaths.length;
    useBtn.textContent = count > 1
      ? `${fsPickerUseLabelBase} ${count} files`
      : count === 1
        ? `${fsPickerUseLabelBase} 1 file`
        : fsPickerUseLabelBase;
  }
}

// Central selection setter: dedupes, keeps fsPickerSelectedPath pointing at
// the most recent entry, and refreshes row highlights, the confirm button,
// and the status line.
function fsPickerApplySelection(paths) {
  const unique = [];
  for (const path of paths) {
    const clean = String(path || '').trim();
    if (clean && !unique.includes(clean)) unique.push(clean);
  }
  fsPickerSelectedPaths = unique;
  fsPickerSelectedPath = unique.length ? unique[unique.length - 1] : '';
  const input = document.getElementById('fs-picker-path');
  if (input && fsPickerSelectedPath) input.value = fsPickerSelectedPath;
  setFsPickerUseButtonState();
  document.querySelectorAll('#fs-picker-list .fs-picker-entry.file').forEach(btn => {
    btn.classList.toggle('selected', fsPickerSelectedPaths.includes(btn.dataset.path || ''));
  });
  if (fsPickerSelectedPaths.length || fsPickerMultiSelect) {
    setFsPickerStatus('', fsPickerSelectionStatusText());
  }
}

function selectFsPickerFile(path) {
  const target = String(path || '').trim();
  fsPickerAnchorPath = target;
  fsPickerApplySelection(target ? [target] : []);
}

function handleFsPickerFileClick(ev, path) {
  const target = String(path || '').trim();
  if (!target) return;
  if (!fsPickerMultiSelect) {
    selectFsPickerFile(target);
    return;
  }
  if (ev.shiftKey && fsPickerAnchorPath) {
    // Range within the current listing, file rows only (directories are
    // never multi-selectable). The anchor survives so a follow-up
    // shift-click re-ranges from the same starting row.
    const listed = Array.from(document.querySelectorAll('#fs-picker-list .fs-picker-entry.file'))
      .map(btn => btn.dataset.path || '')
      .filter(Boolean);
    const from = listed.indexOf(fsPickerAnchorPath);
    const to = listed.indexOf(target);
    if (from !== -1 && to !== -1) {
      fsPickerApplySelection(listed.slice(Math.min(from, to), Math.max(from, to) + 1));
      return;
    }
  }
  if (ev.metaKey || ev.ctrlKey) {
    const next = fsPickerSelectedPaths.includes(target)
      ? fsPickerSelectedPaths.filter(item => item !== target)
      : [...fsPickerSelectedPaths, target];
    fsPickerAnchorPath = target;
    fsPickerApplySelection(next);
    return;
  }
  selectFsPickerFile(target);
}

function setFsPickerDownloadBusy(busy) {
  const useBtn = document.getElementById('fs-picker-use-btn');
  const closeBtn = document.querySelector('#fs-picker-modal .fs-picker-close');
  const cancelBtn = document.getElementById('fs-picker-download-cancel-btn');
  if (useBtn) useBtn.disabled = busy || (fsPickerMode === 'file' ? !fsPickerSelectedPath : !fsPickerCurrentPath);
  if (closeBtn) closeBtn.disabled = !!busy;
  if (cancelBtn) cancelBtn.classList.toggle('hidden', !busy);
}

function renderFsPicker(data) {
  const list = document.getElementById('fs-picker-list');
  const useBtn = document.getElementById('fs-picker-use-btn');
  if (!list) return;
  fsPickerCurrentPath = data.path || '';
  setFsPickerUseButtonState();

  const rows = [];
  if (data.parent) {
    rows.push(
      `<button type="button" class="fs-picker-entry dir" data-path="${escapeHtml(data.parent)}">` +
      `<span class="fs-picker-entry-meta">up</span>` +
      `<span class="fs-picker-entry-name" title="${escapeHtml(data.parent)}">..</span>` +
      `<span class="fs-picker-entry-meta">parent</span>` +
      `</button>`
    );
  }
  for (const entry of data.entries || []) {
    const cls = entry.is_dir ? 'dir' : 'file';
    const title = escapeHtml(entry.path || entry.name || '');
    const selectableFile = fsPickerMode === 'file' && entry.is_file;
    const selected = selectableFile && fsPickerEntryIsSelected(entry.path || '') ? ' selected' : '';
    const disabled = entry.is_dir || selectableFile ? '' : 'disabled';
    rows.push(
      `<button type="button" class="fs-picker-entry ${cls}${selected}" data-path="${title}" ${disabled}>` +
      `<span class="fs-picker-entry-meta">${entry.is_dir ? 'dir' : 'file'}</span>` +
      `<span class="fs-picker-entry-name" title="${title}">${escapeHtml(entry.name || '')}</span>` +
      `<span class="fs-picker-entry-meta">${entry.hidden ? 'hidden' : ''}</span>` +
      `</button>`
    );
  }
  list.innerHTML = rows.length ? rows.join('') : '<div class="empty-state">No entries</div>';
  list.querySelectorAll('.fs-picker-entry.dir').forEach(btn => {
    btn.addEventListener('click', () => loadFsPicker(btn.dataset.path || ''));
  });
  if (fsPickerMode === 'file') {
    list.querySelectorAll('.fs-picker-entry.file').forEach(btn => {
      btn.addEventListener('click', ev => handleFsPickerFileClick(ev, btn.dataset.path || ''));
      btn.addEventListener('dblclick', ev => {
        // In multi-select mode a modified double-click is part of toggling /
        // ranging, not a confirm gesture.
        if (fsPickerMultiSelect && (ev.metaKey || ev.ctrlKey || ev.shiftKey)) return;
        selectFsPickerFile(btn.dataset.path || '');
        useFsPickerSelection();
      });
    });
  }
  if (useBtn) setFsPickerUseButtonState();
}

async function resolveFsPickerListTarget(target) {
  if (fsPickerMode !== 'file' || !fsPathLooksAbsolute(target)) {
    return { listPath: target, selectedPath: '' };
  }
  const resp = await dashboardJsonFetch('api_fs_stat', { path: target }, () => (
    authedFetch('/api/fs/stat?path=' + encodeURIComponent(target))
  ), 'api_fs_stat');
  const status = await resp.json().catch(() => ({}));
  if (!resp.ok) return { listPath: target, selectedPath: '' };
  if (status.exists && status.is_file && status.parent) {
    return { listPath: status.parent, selectedPath: status.path || target };
  }
  if (status.exists && status.is_dir) {
    return { listPath: status.path || target, selectedPath: '' };
  }
  if (!status.exists && status.parent_is_dir && status.parent) {
    return { listPath: status.parent, selectedPath: '' };
  }
  return {
    listPath: status.nearest_existing_parent || target,
    selectedPath: '',
  };
}

async function loadFsPicker(path) {
  const input = document.getElementById('fs-picker-path');
  let target = String(path || input?.value || '').trim();
  if (!target) target = fsPickerMode === 'directory' ? (dashboardProjectRoot || '~') : '~';
  if (input) input.value = target;
  fsPickerCurrentPath = '';
  fsPickerSelectedPath = '';
  fsPickerSelectedPaths = [];
  fsPickerAnchorPath = '';
  setFsPickerUseButtonState();
  setFsPickerStatus('', 'Loading...');
  try {
    const resolved = await resolveFsPickerListTarget(target);
    fsPickerSelectedPath = resolved.selectedPath || '';
    fsPickerSelectedPaths = fsPickerSelectedPath ? [fsPickerSelectedPath] : [];
    fsPickerAnchorPath = fsPickerSelectedPath;
    const resp = await dashboardJsonFetch('api_fs_list', { path: resolved.listPath }, () => (
      authedFetch('/api/fs/list?path=' + encodeURIComponent(resolved.listPath))
    ), 'api_fs_list');
    const data = await resp.json().catch(() => ({}));
    if (!resp.ok) throw new Error(data.error || `Directory load failed (${resp.status})`);
    if (input) input.value = fsPickerSelectedPath || data.path || resolved.listPath;
    const statusText = fsPickerSelectedPath
      ? fsPickerSelectionStatusText()
      : data.truncated
        ? 'Showing first 500 entries'
        : fsPickerSelectionStatusText();
    setFsPickerStatus('', statusText);
    renderFsPicker(data);
  } catch (e) {
    setFsPickerStatus('error', e.message || 'Directory load failed');
    const list = document.getElementById('fs-picker-list');
    if (list) list.innerHTML = '<div class="empty-state">Directory unavailable</div>';
  }
}

function loadFsPickerPath() {
  loadFsPicker(document.getElementById('fs-picker-path')?.value || '');
}

function openProjectDirectoryPicker() {
  configureFsPicker({
    mode: 'directory',
    target: 'project',
    title: 'Choose project directory',
    placeholder: '/path/to/project',
    useLabel: 'Use directory',
    showCreate: true,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  loadFsPicker(newSessionProjectInputValue() || dashboardProjectRoot || '~');
}

function openAgentBinaryPicker() {
  const agentId = normalizeAgentId(document.getElementById('new-session-agent')?.value || '');
  if (!agentId) return;
  configureFsPicker({
    mode: 'file',
    target: 'agentBinary',
    title: `Choose ${prettyAgentName(agentId)} binary`,
    placeholder: '/path/to/binary',
    useLabel: 'Use binary',
    showCreate: false,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  const current = document.getElementById('new-session-agent-command')?.value.trim()
    || commandDefaultForNewSessionAgent(agentId)
    || '';
  loadFsPicker(fsPathLooksAbsolute(current) ? current : '~');
}

function openDownloadFilePicker() {
  configureFsPicker({
    mode: 'file',
    target: 'downloadFile',
    title: 'Download file',
    placeholder: '/path/to/file',
    useLabel: 'Download',
    showCreate: false,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  loadFsPicker(dashboardProjectRoot || '~');
}

function openFilesDownloadPicker() {
  if (filesDownloadSelectedPeerId()) {
    setFilesDownloadStatus('warn', 'Peer browsing is not available yet; enter a full path');
    return;
  }
  configureFsPicker({
    mode: 'file',
    target: 'filesDownload',
    title: 'Choose files to download',
    placeholder: '/path/to/file',
    useLabel: 'Download',
    showCreate: false,
    multiSelect: true,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  loadFsPicker(filesDownloadPathValue() || dashboardProjectRoot || '~');
}

function openFilesUploadDestinationPicker() {
  configureFsPicker({
    mode: 'directory',
    target: 'filesUploadDestination',
    title: 'Choose upload destination',
    placeholder: '/destination/folder-or-file',
    useLabel: 'Use destination',
    showCreate: true,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  loadFsPicker(filesUploadDestinationValue() || dashboardProjectRoot || '~');
}

function openSessionConfigBinaryPicker() {
  if (!sessionConfigEditing) return;
  configureFsPicker({
    mode: 'file',
    target: 'sessionAgentBinary',
    title: `Choose ${prettyAgentName(sessionConfigEditing.source)} binary`,
    placeholder: '/path/to/binary',
    useLabel: 'Use binary',
    showCreate: false,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  const current = document.getElementById('session-config-command')?.value.trim()
    || commandDefaultForNewSessionAgent(sessionConfigEditing.source)
    || '';
  loadFsPicker(fsPathLooksAbsolute(current) ? current : '~');
}

function closeFsPicker() {
  if (fsPickerDownloadAbort) return;
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'none';
}

function closeProjectDirectoryPicker() {
  closeFsPicker();
}

async function downloadFsPickerSelectedFile() {
  const path = fsPickerSelectedPath || document.getElementById('fs-picker-path')?.value.trim() || '';
  if (!path) return;
  if (!filesDownloadTunnelAvailable()) {
    const message = filesDownloadUnavailableMessage();
    setFsPickerStatus('error', message);
    setSettingsDownloadFileStatus(message, 'error');
    if (typeof showControlToast === 'function') showControlToast('error', message);
    return;
  }
  const controller = new AbortController();
  fsPickerDownloadAbort = controller;
  setFsPickerDownloadBusy(true);
  setFsPickerStatus('warn', 'Preparing download...');
  setSettingsDownloadFileStatus('Downloading...', 'warn');
  try {
    const result = await fetchDashboardFilesystemDownload(path, {
      signal: controller.signal,
      chunkBytes: DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
      onProgress: progress => {
        const total = progress.total ? humanBytes(progress.total) : '?';
        setFsPickerStatus('warn', `Downloading ${humanBytes(progress.loaded)} / ${total}`);
        setSettingsDownloadFileStatus(`${humanBytes(progress.loaded)} / ${total}`, 'warn');
      },
    });
    downloadDashboardBlob(result.blob, result.filename, result.content_type);
    const done = `Downloaded ${result.filename} (${humanBytes(result.size)})`;
    setFsPickerStatus('ok', done);
    setSettingsDownloadFileStatus(done, 'ok');
  } catch (err) {
    const aborted = err?.name === 'AbortError';
    const message = aborted ? 'Download stopped' : (err?.message || 'Download failed');
    setFsPickerStatus(aborted ? 'warn' : 'error', message);
    setSettingsDownloadFileStatus(message, aborted ? 'warn' : 'error');
    if (!aborted && typeof showControlToast === 'function') showControlToast('error', message);
  } finally {
    fsPickerDownloadAbort = null;
    setFsPickerDownloadBusy(false);
    setFsPickerUseButtonState();
  }
}

function cancelFsPickerDownload() {
  fsPickerDownloadAbort?.abort();
}

function useFsPickerSelection() {
  if (fsPickerTarget === 'downloadFile') {
    downloadFsPickerSelectedFile();
    return;
  }
  if (fsPickerTarget === 'filesDownload') {
    const paths = fsPickerSelectedPaths.length
      ? fsPickerSelectedPaths.slice()
      : (fsPickerSelectedPath ? [fsPickerSelectedPath] : []);
    if (!paths.length) return;
    if (!filesDownloadTunnelAvailable()) {
      setFsPickerStatus('error', filesDownloadUnavailableMessage());
      return;
    }
    // One transfer per selected file: the transfers pump runs them
    // sequentially and each completed download saves through the existing
    // per-transfer browser-save flow. No bundling.
    for (const path of paths) startFilesDownload({ path });
    if (paths.length > 1) setFilesDownloadStatus('warn', `Queued ${paths.length} downloads`);
    closeFsPicker();
    return;
  }
  if (fsPickerTarget === 'filesUploadDestination') {
    const input = document.getElementById('files-upload-destination');
    if (input) {
      input.value = fsPickerCurrentPath || fsPickerSelectedPath || '';
      input.title = input.value;
    }
    closeFsPicker();
    return;
  }
  if (fsPickerTarget === 'agentBinary') {
    if (!fsPickerSelectedPath) return;
    const input = document.getElementById('new-session-agent-command');
    if (input) {
      input.value = fsPickerSelectedPath;
      input.title = fsPickerSelectedPath;
    }
    closeFsPicker();
    return;
  }
  if (fsPickerTarget === 'sessionAgentBinary') {
    if (!fsPickerSelectedPath) return;
    const input = document.getElementById('session-config-command');
    if (input) {
      input.value = fsPickerSelectedPath;
      input.title = fsPickerSelectedPath;
    }
    closeFsPicker();
    return;
  }
  if (!fsPickerCurrentPath) return;
  const input = document.getElementById('new-session-project-root');
  if (input) {
    input.value = fsPickerCurrentPath;
    input.title = fsPickerCurrentPath;
  }
  closeFsPicker();
  refreshNewSessionProjectStatus();
}

function useProjectDirectoryPickerSelection() {
  useFsPickerSelection();
}

function loadProjectDirectoryPicker(path) {
  loadFsPicker(path);
}

function loadProjectDirectoryPickerPath() {
  loadFsPickerPath();
}

async function createPickerDirectory() {
  if (fsPickerMode !== 'directory') return;
  const path = document.getElementById('fs-picker-path')?.value.trim() || '';
  if (!path) return;
  setFsPickerStatus('', 'Creating directory...');
  const resp = await dashboardJsonFetch('api_fs_mkdir', { path }, () => (
    authedFetch('/api/fs/mkdir', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    })
  ), 'api_fs_mkdir', { fallbackAfterRpcFailure: false });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) {
    setFsPickerStatus('error', data.error || `Create failed (${resp.status})`);
    return;
  }
  setFsPickerStatus('', data.already_exists ? 'Directory already exists' : 'Directory created');
  await loadProjectDirectoryPicker(data.path || path);
}

window.openProjectDirectoryPicker = openProjectDirectoryPicker;
window.closeProjectDirectoryPicker = closeProjectDirectoryPicker;
window.loadProjectDirectoryPickerPath = loadProjectDirectoryPickerPath;
window.useProjectDirectoryPickerSelection = useProjectDirectoryPickerSelection;
window.closeFsPicker = closeFsPicker;
window.loadFsPickerPath = loadFsPickerPath;
window.useFsPickerSelection = useFsPickerSelection;
window.openAgentBinaryPicker = openAgentBinaryPicker;

// The Claude model picker offers version-safe aliases (the CLI resolves
// them to the latest model); the free-text id input only appears behind
// the explicit "Custom model id…" choice.
function updateNewSessionClaudeCustomModelRow() {
  const select = document.getElementById('new-session-claude-model-select');
  const row = document.getElementById('new-session-claude-model-custom-row');
  if (!select || !row) return;
  const custom = select.value === '__custom__' && !select.disabled;
  row.classList.toggle('hidden', !custom);
}
function onNewSessionClaudeModelSelectChange() {
  updateNewSessionClaudeCustomModelRow();
}
window.onNewSessionClaudeModelSelectChange = onNewSessionClaudeModelSelectChange;
	window.openDownloadFilePicker = openDownloadFilePicker;
	window.openFilesDownloadPicker = openFilesDownloadPicker;
	window.onFilesDownloadHostChanged = onFilesDownloadHostChanged;
	window.startFilesDownload = startFilesDownload;
	window.cancelFilesDownload = cancelFilesDownload;
	window.pauseFilesTransfer = pauseFilesTransfer;
	window.resumeFilesTransfer = resumeFilesTransfer;
	window.retryFilesTransfer = retryFilesTransfer;
	window.cancelFilesTransfer = cancelFilesTransfer;
	window.clearFilesTransferHistory = clearFilesTransferHistory;
	window.refreshFilesTransferJobs = refreshFilesTransferJobs;
	window.chooseFilesForUpload = chooseFilesForUpload;
	window.openFilesUploadDestinationPicker = openFilesUploadDestinationPicker;
	window.refreshFilesStagedUploads = refreshFilesStagedUploads;
	window.downloadFilesStagedUpload = downloadFilesStagedUpload;
	window.deleteFilesStagedUpload = deleteFilesStagedUpload;
	window.cancelFsPickerDownload = cancelFsPickerDownload;
	window.openSessionConfigBinaryPicker = openSessionConfigBinaryPicker;
	window.createPickerDirectory = createPickerDirectory;
	window.onFilesIdeHostChanged = onFilesIdeHostChanged;
	window.filesIdeBeginCreate = filesIdeBeginCreate;
	window.filesIdeTreeUp = filesIdeTreeUp;
	window.filesIdeSetRoot = filesIdeSetRoot;
	window.filesIdeToggleHidden = filesIdeToggleHidden;
	window.filesIdeRefreshTree = filesIdeRefreshTree;
	window.filesIdeSaveActive = filesIdeSaveActive;
	window.filesIdeReloadActive = filesIdeReloadActive;
	window.filesIdeOverwriteActive = filesIdeOverwriteActive;
	window.filesIdeFindOnInput = filesIdeFindOnInput;
	window.filesIdeFindStep = filesIdeFindStep;
	window.filesIdeCloseFind = filesIdeCloseFind;
	window.intendantDashboardFilesIde = {
	  _debugSnapshot() {
	    const buffer = filesIdeActiveBuffer();
	    return {
	      host: filesIdeSelectedHostId(),
	      root: filesIdeTreeState(filesIdeSelectedHostId()).root,
	      openTabs: Array.from(filesIdeBuffers.values()).map(b => ({
	        host: b.host,
	        path: b.path,
	        dirty: filesIdeBufferDirty(b),
	        isNew: b.isNew,
	        conflict: b.conflict ? b.conflict.code : null,
	      })),
	      active: buffer
	        ? {
	            path: buffer.path,
	            host: buffer.host,
	            dirty: filesIdeBufferDirty(buffer),
	            baselineSha: buffer.baselineSha,
	            eol: buffer.eol,
	            text: buffer.doc.getValue('\n'),
	          }
	        : null,
	      saveStatus: document.getElementById('files-ide-save-status')?.textContent || '',
	      banner: document.getElementById('files-ide-banner')?.textContent || '',
	    };
	  },
	  async _debugOpen(path, options = {}) {
	    await filesIdeOpenFile(filesIdeSelectedHostId(), path, options);
	    return this._debugSnapshot();
	  },
	  _debugSetText(text) {
	    const buffer = filesIdeActiveBuffer();
	    if (!buffer) throw new Error('no active buffer');
	    buffer.doc.setValue(String(text));
	    return this._debugSnapshot();
	  },
	  async _debugSave(options = {}) {
	    await filesIdeSaveActive(options);
	    return this._debugSnapshot();
	  },
	  async _debugSetRoot(path) {
	    await filesIdeSetRoot(path);
	    return this._debugSnapshot();
	  },
	  async _debugRename(path, newName, isDir = false) {
	    filesIdeBeginRename(path, isDir);
	    await filesIdeCommitRename(newName);
	    return this._debugSnapshot();
	  },
	  // Raw rename RPC (no UI): the inline rename can only ever target the
	  // same directory, so cross-root denial tests need the direct lane.
	  _debugRawRename(from, to) {
	    return filesIdeRename(filesIdeSelectedHostId(), from, to);
	  },
	  async _debugDelete(path, options = {}) {
	    await filesIdeExecuteDelete(path, Boolean(options.isDir), Boolean(options.recursive));
	    return {
	      ...this._debugSnapshot(),
	      treeStatus: document.getElementById('files-ide-tree-status')?.textContent || '',
	      deleteArming: filesIdeTreeState(filesIdeSelectedHostId()).deleteArming
	        ? { ...filesIdeTreeState(filesIdeSelectedHostId()).deleteArming, timer: null }
	        : null,
	    };
	  },
	  async _debugFind(query) {
	    filesIdeOpenFind();
	    const input = filesIdeFindInput();
	    if (input) input.value = String(query ?? '');
	    filesIdeFindRecompute({ jump: true });
	    return {
	      open: filesIdeFindOpen,
	      total: filesIdeFindMatches.length,
	      index: filesIdeFindIndex,
	      count: document.getElementById('files-ide-find-count')?.textContent || '',
	    };
	  },
	  _debugFindStep(dir) {
	    filesIdeFindStep(dir);
	    return { total: filesIdeFindMatches.length, index: filesIdeFindIndex };
	  },
	};
	window.intendantDashboardFiles = {
	  _debugTransferSnapshot() {
	    return filesTransferSnapshot();
	  },
	  _debugStagedUploadsSnapshot() {
	    return filesStagedUploadsSnapshot();
	  },
	  async _debugProbeDownloadPath(path, options = {}) {
	    const progress = [];
	    setFilesDownloadPath(path);
    const result = await startFilesDownload({
      path,
      chunkBytes: options.chunkBytes,
      maxBytes: options.maxBytes,
      skipBrowserSave: true,
      throwOnError: true,
      onProgress: item => {
        progress.push({
          loaded: item.loaded,
          total: item.total,
          rangeCount: item.rangeCount,
        });
      },
    });
    const bytes = new Uint8Array(await result.blob.arrayBuffer());
    let text = '';
    try {
      text = new TextDecoder().decode(bytes);
    } catch (_) {}
    return {
      ok: true,
      path: filesDownloadPathValue(),
      filename: result.filename,
      size: result.size,
      totalSize: result.total_size,
      rangeCount: result.range_count,
      progress,
      transfers: filesTransferSnapshot(),
      statusText: document.getElementById('files-download-status')?.textContent || '',
      progressWidth: document.getElementById('files-download-progress')?.style.width || '',
	      text,
	    };
	  },
	  async _debugRefreshPeerList() {
	    if (typeof refreshPeersFromApi === 'function') {
	      await refreshPeersFromApi();
	    }
	    refreshFilesDownloadHostOptions();
	    return daemons.map(peer => ({
	      id: String(peer.host_id || ''),
	      label: String(peer.label || ''),
	      connected: peer.connected !== false,
	      url: peer.url || null,
	    }));
	  },
	  async _debugProbePeerDownloadPath(peerId, path, options = {}) {
	    const progress = [];
	    const select = document.getElementById('files-download-host');
	    if (select) {
	      refreshFilesDownloadHostOptions();
	      select.value = String(peerId || '');
	      onFilesDownloadHostChanged({ preserveStatus: true });
	    }
	    setFilesDownloadPath(path);
	    const result = await startFilesDownload({
	      path,
	      peerId,
	      peerLabel: options.peerLabel,
	      chunkBytes: options.chunkBytes,
	      maxBytes: options.maxBytes,
	      skipBrowserSave: true,
	      throwOnError: true,
	      onProgress: item => {
	        progress.push({
	          loaded: item.loaded,
	          total: item.total,
	          rangeCount: item.rangeCount,
	        });
	      },
	    });
	    const bytes = new Uint8Array(await result.blob.arrayBuffer());
	    let text = '';
	    try {
	      text = new TextDecoder().decode(bytes);
	    } catch (_) {}
	    return {
	      ok: true,
	      peerId,
	      path,
	      filename: result.filename,
	      size: result.size,
	      totalSize: result.total_size,
	      rangeCount: result.range_count,
	      progress,
	      text,
	      transfer: filesTransferSnapshot().find(item => item.peerId === String(peerId || '') && item.path === path) || null,
	      statusText: document.getElementById('files-download-status')?.textContent || '',
	    };
	  },
	  async _debugProbeArtifactDownload(artifact, options = {}) {
	    const progress = [];
	    const transfer = queueDashboardArtifactDownload(artifact, {
	      sourceLabel: options.sourceLabel,
	      filename: options.filename,
	      contentType: options.contentType,
	      chunkBytes: options.chunkBytes,
	      maxBytes: options.maxBytes,
	      skipBrowserSave: true,
	      directMethod: options.directMethod,
	      directParams: options.directParams,
	      onProgress: item => {
	        progress.push({
	          loaded: item.loaded,
	          total: item.total,
	          rangeCount: item.rangeCount,
	        });
	      },
	    });
	    if (!transfer) throw new Error('artifact download was not queued');
	    const result = await transfer.completion;
	    const bytes = new Uint8Array(await result.blob.arrayBuffer());
	    let text = '';
	    try {
	      text = new TextDecoder().decode(bytes);
	    } catch (_) {}
	    return {
	      ok: true,
	      filename: result.filename,
	      size: result.size,
	      totalSize: result.total_size,
	      rangeCount: result.range_count,
	      progress,
	      text,
	      transfer: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	      statusText: document.getElementById('files-download-status')?.textContent || '',
	    };
	  },
	  async _debugProbeInterruptedDownload(path, options = {}) {
	    const transfer = queueFilesDownload(path, {
	      chunkBytes: options.chunkBytes,
	      maxBytes: options.maxBytes,
	      skipBrowserSave: true,
	      failAfterRanges: options.failAfterRanges || 1,
	    });
	    if (!transfer) throw new Error('download was not queued');
	    let firstError = '';
	    try {
	      await transfer.completion;
	    } catch (err) {
	      firstError = err?.message || String(err);
	    }
	    const failedLoaded = Number(transfer.loaded || 0);
	    const failedRangeCount = Number(transfer.rangeCount || 0);
	    const failedStatus = transfer.status;
	    const result = await resumeFilesTransfer(transfer.id);
	    const bytes = new Uint8Array(await result.blob.arrayBuffer());
	    let text = '';
	    try {
	      text = new TextDecoder().decode(bytes);
	    } catch (_) {}
	    return {
	      ok: true,
	      path: filesDownloadPathValue(),
	      filename: result.filename,
	      text,
	      size: result.size,
	      totalSize: result.total_size,
	      failedStatus,
	      failedLoaded,
	      failedRangeCount,
	      firstError,
	      finalStatus: transfer.status,
	      finalLoaded: Number(transfer.loaded || 0),
	      finalRangeCount: Number(transfer.rangeCount || 0),
	      transfer: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	      statusText: document.getElementById('files-download-status')?.textContent || '',
	      progressWidth: document.getElementById('files-download-progress')?.style.width || '',
	    };
	  },
	  async _debugStartInterruptedDownload(path, options = {}) {
	    const transfer = queueFilesDownload(path, {
	      chunkBytes: options.chunkBytes,
	      maxBytes: options.maxBytes,
	      skipBrowserSave: true,
	      failAfterRanges: options.failAfterRanges || 1,
	    });
	    if (!transfer) throw new Error('download was not queued');
	    let firstError = '';
	    try {
	      await transfer.completion;
	    } catch (err) {
	      firstError = err?.message || String(err);
	    }
	    return {
	      ok: true,
	      transferId: transfer.id,
	      status: transfer.status,
	      loaded: Number(transfer.loaded || 0),
	      rangeCount: Number(transfer.rangeCount || 0),
	      firstError,
	      snapshot: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	    };
	  },
	  async _debugProbeUploadText(text, options = {}) {
	    const previousFetch = window.fetch;
	    let httpFallbackCount = 0;
	    window.fetch = function(input, init) {
	      const url = typeof input === 'string' ? input : (input && input.url || '');
	      if (String(url).includes('/api/session/current/uploads')) httpFallbackCount += 1;
	      return previousFetch.call(this, input, init);
	    };
	    try {
	      const bytes = new TextEncoder().encode(String(text || ''));
	      const file = new File([bytes], options.name || 'connect-files-upload.txt', {
	        type: options.mime || 'text/plain',
	      });
	      const transfer = queueFilesUpload(file, {
	        destination: options.destination || 'task',
	        timeoutMs: options.timeoutMs,
	      });
	      if (!transfer) throw new Error('upload was not queued');
	      const descriptor = await transfer.completion;
	      const raw = await dashboardFetchRangedBytes('api_session_current_upload_raw', { id: descriptor.id }, {
	        chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
	        maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
	      });
	      const rawBytes = new Uint8Array(await raw.blob.arrayBuffer());
	      let rawText = '';
	      try {
	        rawText = new TextDecoder().decode(rawBytes);
	      } catch (_) {}
	      return {
	        ok: true,
	        httpFallbackCount,
	        uploadId: descriptor.id || '',
	        uploadName: descriptor.name || '',
	        uploadSize: Number(descriptor.size || 0),
	        transferId: transfer.id,
	        transferStatus: transfer.status,
	        staged: filesStagedUploadsSnapshot().find(item => item.id === String(descriptor.id || '')) || null,
	        rawText,
	        rawSize: raw.size,
	        rawTotalSize: raw.total_size,
	        rawRangeCount: raw.range_count,
	        statusText: document.getElementById('files-upload-status')?.textContent || '',
	      };
	    } finally {
	      window.fetch = previousFetch;
	    }
	  },
	  async _debugProbeFilesystemUploadText(text, options = {}) {
	    const destination = String(options.destination || '').trim();
	    if (!destination) throw new Error('destination is required');
	    const bytes = new TextEncoder().encode(String(text || ''));
	    const file = new File([bytes], options.name || 'connect-files-upload.txt', {
	      type: options.mime || 'text/plain',
	    });
	    const transfer = queueFilesUpload(file, {
	      destination,
	      conflict: options.conflict || 'fail',
	      timeoutMs: options.timeoutMs,
	      chunkBytes: options.chunkBytes,
	    });
	    if (!transfer) throw new Error('upload was not queued');
	    const job = await transfer.completion;
	    const finalPath = String(job?.final_path || job?.destination_path || transfer.destination || '');
	    const raw = await dashboardFetchRangedBytes('api_fs_read', { path: finalPath }, {
	      chunkBytes: options.readChunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
	      maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
	    });
	    const rawBytes = new Uint8Array(await raw.blob.arrayBuffer());
	    let rawText = '';
	    try {
	      rawText = new TextDecoder().decode(rawBytes);
	    } catch (_) {}
	    return {
	      ok: true,
	      finalPath,
	      rawText,
	      rawSize: raw.size,
	      transferId: transfer.id,
	      transferStatus: transfer.status,
	      serverJobId: transfer.serverJobId || '',
	      resumeToken: transfer.resumeToken || '',
	      transfer: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	      statusText: document.getElementById('files-upload-status')?.textContent || '',
	    };
	  },
	  async _debugStartInterruptedFilesystemUploadText(text, options = {}) {
	    const destination = String(options.destination || '').trim();
	    if (!destination) throw new Error('destination is required');
	    const bytes = new TextEncoder().encode(String(text || ''));
	    const file = new File([bytes], options.name || 'connect-files-upload.txt', {
	      type: options.mime || 'text/plain',
	    });
	    const transfer = queueFilesUpload(file, {
	      destination,
	      conflict: options.conflict || 'fail',
	      timeoutMs: options.timeoutMs,
	      chunkBytes: options.chunkBytes,
	      failAfterChunks: options.failAfterChunks || 1,
	    });
	    if (!transfer) throw new Error('upload was not queued');
	    let firstError = '';
	    try {
	      await transfer.completion;
	    } catch (err) {
	      firstError = err?.message || String(err);
	    }
	    return {
	      ok: true,
	      transferId: transfer.id,
	      status: transfer.status,
	      loaded: Number(transfer.loaded || 0),
	      totalSize: Number(transfer.totalSize || 0),
	      uploadChunkCount: Number(transfer.uploadChunkCount || 0),
	      firstError,
	      snapshot: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	    };
	  },
	  async _debugResumeTransfer(id, options = {}) {
	    const transfer = filesTransferById(id);
	    if (!transfer) throw new Error('transfer not found');
	    const result = await resumeFilesTransfer(id);
	    let rawText = '';
	    let finalPath = '';
	    if (transfer.kind === 'upload') {
	      finalPath = String(result?.final_path || result?.destination_path || transfer.destination || '');
	      const raw = await dashboardFetchRangedBytes('api_fs_read', { path: finalPath }, {
	        chunkBytes: options.readChunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
	        maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
	      });
	      const rawBytes = new Uint8Array(await raw.blob.arrayBuffer());
	      try {
	        rawText = new TextDecoder().decode(rawBytes);
	      } catch (_) {}
	    } else if (result?.blob) {
	      const rawBytes = new Uint8Array(await result.blob.arrayBuffer());
	      try {
	        rawText = new TextDecoder().decode(rawBytes);
	      } catch (_) {}
	    }
	    return {
	      ok: true,
	      transferId: transfer.id,
	      kind: transfer.kind,
	      status: transfer.status,
	      loaded: Number(transfer.loaded || 0),
	      totalSize: Number(transfer.totalSize || 0),
	      rangeCount: Number(transfer.rangeCount || 0),
	      uploadChunkCount: Number(transfer.uploadChunkCount || 0),
	      finalPath,
	      rawText,
	      snapshot: filesTransferSnapshot().find(item => item.id === transfer.id) || null,
	    };
	  },
	};

restoreFilesTransferState();
renderFilesTransfers();

function normalizeContextArchiveMode(mode) {
  return ['summary', 'exact', 'off'].includes(mode) ? mode : 'summary';
}

function normalizeContextArchiveModeOptional(mode) {
  const v = String(mode || '').trim();
  return ['summary', 'exact', 'off'].includes(v) ? v : '';
}

function normalizeCodexSandbox(mode) {
  const v = String(mode || '').trim();
  return ['workspace-write', 'danger-full-access', 'read-only'].includes(v) ? v : 'workspace-write';
}

function normalizeCodexSandboxOptional(mode) {
  const v = String(mode || '').trim();
  return ['workspace-write', 'danger-full-access', 'read-only'].includes(v) ? v : '';
}

function normalizeCodexApprovalPolicy(policy) {
  const v = String(policy || '').trim();
  return ['on-request', 'never', 'untrusted'].includes(v) ? v : 'on-request';
}

function normalizeCodexApprovalPolicyOptional(policy) {
  const v = String(policy || '').trim();
  return ['on-request', 'never', 'untrusted'].includes(v) ? v : '';
}

function normalizeCodexServiceTier(tier) {
  const v = String(tier || '').trim().toLowerCase();
  if (!v || v === 'inherit' || v === 'default' || v === 'auto' || v === 'codex') return '';
  if (v === 'fast' || v === 'priority') return 'priority';
  if (['standard', 'normal', 'none', 'off', 'clear', 'disabled', 'false', '0'].includes(v)) return 'standard';
  if (v === 'flex') return 'flex';
  return v;
}

function codexServiceTierIsFast(tier) {
  return normalizeCodexServiceTier(tier) === 'priority';
}

function resetNewSessionCodexFastModeToDefault() {
  newSessionCodexFastModeTouched = false;
  newSessionCodexFastMode = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
}

function setNewSessionAgentDefaults(settings) {
  newSessionAgentCommands = {
    codex: settings.codex_command || 'codex',
    'claude-code': settings.claude_command || 'claude',
  };
  newSessionCodexManagedContext =
    settings.codex_managed_context === 'managed' ? 'managed' : 'vanilla';
  newSessionCodexContextArchive = normalizeContextArchiveMode(settings.codex_context_archive || 'summary');
  newSessionCodexSandbox = normalizeCodexSandbox(settings.codex_sandbox || 'workspace-write');
  newSessionCodexApprovalPolicy = normalizeCodexApprovalPolicy(settings.codex_approval_policy || 'on-request');
  newSessionCodexDefaultServiceTier = normalizeCodexServiceTier(settings.codex_service_tier || '');
  newSessionCodexLaunchDefaultsLoaded = true;
  if (!newSessionCodexFastModeTouched) {
    newSessionCodexFastMode = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
  }
  renderNewSessionAgentControls();
}

function commandDefaultForNewSessionAgent(agentId) {
  return newSessionAgentCommands[agentId] || ({
    codex: 'codex',
    'claude-code': 'claude',
  }[agentId] || '');
}

function effectiveNewSessionAgentId() {
  const select = document.getElementById('new-session-agent');
  const raw = select?.value || '';
  if (raw === 'internal') return 'internal';
  return normalizeAgentId(raw) || newSessionConfiguredAgent || '';
}

function renderNewSessionAgentControls(options = {}) {
  const select = document.getElementById('new-session-agent');
  const commandInput = document.getElementById('new-session-agent-command');
  const browseBtn = document.getElementById('new-session-agent-command-browse');
  const sandboxSel = document.getElementById('new-session-codex-sandbox');
  const approvalSel = document.getElementById('new-session-codex-approval-policy');
  const managedContextSel = document.getElementById('new-session-codex-managed-context');
  const contextArchiveSel = document.getElementById('new-session-codex-context-archive');
  const fastToggle = document.getElementById('new-session-codex-fast');
  const fastWrap = document.getElementById('new-session-codex-fast-wrap');
  const managedContextNote = document.getElementById('new-session-managed-context-note');
  if (!select || !commandInput) return;

  // Grey out backends whose CLI is missing on the daemon host; kick the
  // probe on first render and re-apply when it lands.
  if (Array.isArray(externalAgentAvailability)) {
    applyExternalAgentAvailabilityToNewSessionPicker();
  } else {
    refreshExternalAgentAvailability();
  }

  const currentOption = select.querySelector('option[value=""]');
  if (currentOption) {
    currentOption.textContent = newSessionConfiguredAgent
      ? `Current setting (${prettyAgentName(newSessionConfiguredAgent)})`
      : 'Current setting (internal agent)';
  }

  const selectedAgent = normalizeAgentId(select.value);
  const effectiveAgent = effectiveNewSessionAgentId();
  const hasExternalAgent = !!selectedAgent;
  // The external-options fold follows the backend choice: open while an
  // external agent is selected (or is the configured default), closed for
  // the internal agent.
  const externalFold = document.getElementById('new-session-external-fold');
  if (externalFold) {
    externalFold.open = hasExternalAgent ||
      (!!effectiveAgent && effectiveAgent !== 'internal' && effectiveAgent !== 'intendant');
  }
  // Execution shape (auto / orchestrate / direct) only applies to the
  // internal agent — external CLIs run their own loops.
  const executionSel = document.getElementById('new-session-execution');
  if (executionSel) {
    const appliesToInternal =
      !effectiveAgent || effectiveAgent === 'internal' || effectiveAgent === 'intendant';
    executionSel.disabled = !appliesToInternal;
    if (!appliesToInternal) executionSel.value = '';
    document
      .getElementById('new-session-execution-wrap')
      ?.classList.toggle('disabled', !appliesToInternal);
  }
  commandInput.disabled = !hasExternalAgent;
  if (browseBtn) browseBtn.disabled = !hasExternalAgent;
  commandInput.placeholder = hasExternalAgent
    ? commandDefaultForNewSessionAgent(selectedAgent)
    : 'Select an external agent';
  if (!hasExternalAgent || options.replaceCommand) {
    commandInput.value = '';
  }
  const claudeModelSel = document.getElementById('new-session-claude-model-select');
  const claudeModelInp = document.getElementById('new-session-claude-model');
  const claudeModeSel = document.getElementById('new-session-claude-permission-mode');
  const claudeEffortSel = document.getElementById('new-session-claude-effort');
  const appliesToClaude = effectiveAgent === 'claude-code';
  if (claudeModelSel) {
    claudeModelSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModelSel.value = '';
  }
  if (claudeModelInp) {
    claudeModelInp.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModelInp.value = '';
  }
  updateNewSessionClaudeCustomModelRow();
  if (claudeModeSel) {
    claudeModeSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModeSel.value = '';
  }
  if (claudeEffortSel) {
    claudeEffortSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeEffortSel.value = '';
  }
  if (managedContextSel) {
    const appliesToCodex = effectiveAgent === 'codex';
    managedContextSel.disabled = !appliesToCodex;
    managedContextSel.value = newSessionCodexManagedContext;
  }
  if (sandboxSel) {
    const appliesToCodex = effectiveAgent === 'codex';
    sandboxSel.disabled = !appliesToCodex;
    sandboxSel.value = normalizeCodexSandbox(newSessionCodexSandbox);
  }
  if (approvalSel) {
    const appliesToCodex = effectiveAgent === 'codex';
    approvalSel.disabled = !appliesToCodex;
    approvalSel.value = normalizeCodexApprovalPolicy(newSessionCodexApprovalPolicy);
  }
  if (contextArchiveSel) {
    const appliesToCodex = effectiveAgent === 'codex';
    contextArchiveSel.disabled = !appliesToCodex;
    contextArchiveSel.value = normalizeContextArchiveMode(newSessionCodexContextArchive);
  }
  if (fastToggle) {
    const appliesToCodex = effectiveAgent === 'codex';
    fastToggle.disabled = !appliesToCodex;
    fastToggle.checked = appliesToCodex && !!newSessionCodexFastMode;
    if (fastWrap) {
      fastWrap.classList.toggle('disabled', !appliesToCodex);
      fastWrap.classList.toggle('active', appliesToCodex && !!newSessionCodexFastMode);
      const defaultFast = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
      fastWrap.title = appliesToCodex
        ? (defaultFast
          ? 'Global default is Fast; uncheck to force this new session to normal'
          : 'Start the new Codex session with Fast service tier')
        : 'Fast service tier applies to Codex sessions';
    }
  }
  if (managedContextNote) {
    const mode = managedContextSel?.value || newSessionCodexManagedContext;
    const appliesToCodex = effectiveAgent === 'codex';
    managedContextNote.classList.toggle('warn', appliesToCodex && mode === 'managed');
    managedContextNote.textContent = appliesToCodex && mode === 'managed'
      ? 'Managed requires a patched Codex binary with the managed app-server protocol.'
      : '';
  }
  updateNewSessionFuelBanner();
}

function setNewSessionStartButtonPending(pending) {
  const btn = document.getElementById('new-session-start-btn');
  if (!btn) return;
  btn.disabled = !!pending;
  btn.classList.toggle('pending', !!pending);
  btn.textContent = pending ? 'Spawning...' : 'Start session';
  if (!pending) updateNewSessionFuelBanner();
}

// ── Unfueled preflight ──
// The status frame's aggregate `fueled` flag (presence-level, no settings
// permission needed) gates internal launches before they spawn a session
// that can only die with "No API key found". Strict === false: an unknown
// state (no status frame yet, older daemon) never blocks.

// Layered like refreshUnfueledEmptyState: the status frame's aggregate
// wins when present; otherwise a one-shot HTTP key-status probe fills in
// (the control transport may not be connected yet — or ever, for some
// bindings). Unknown never blocks.
let daemonUnfueledCached = null;
let daemonFuelProbeInFlight = false;
// ui-v2 fueled-banner detail: which providers the key-status probe saw.
// null = never probed; [] = probed and none (or probe failed — generic
// copy, no re-hammering).
let daemonFuelProviders = null;

function daemonInternalUnfueled() {
  const status = dashboardControlTransport?.lastStatus;
  if (status && typeof status.fueled === 'boolean') return status.fueled === false;
  return daemonUnfueledCached === true;
}

function refreshFuelStateForBanner() {
  const status = dashboardControlTransport?.lastStatus;
  // ui-v2's green Fueled banner names the fueled providers, so under the
  // flag the one-shot probe also runs when the status frame already
  // answered the boolean; v1 keeps the original short-circuits.
  const wantProviders = typeof ui2Enabled === 'function' && ui2Enabled() && daemonFuelProviders === null;
  if (status && typeof status.fueled === 'boolean' && !wantProviders) return;
  if ((daemonUnfueledCached !== null && !wantProviders) || daemonFuelProbeInFlight) return;
  if (typeof fetchApiKeyStatus !== 'function') return;
  daemonFuelProbeInFlight = true;
  fetchApiKeyStatus()
    .then(d => {
      if (d && !d.error) {
        if (daemonUnfueledCached === null) daemonUnfueledCached = !(d.openai || d.anthropic || d.gemini);
        daemonFuelProviders = [
          d.anthropic ? 'Anthropic' : '',
          d.openai ? 'OpenAI' : '',
          d.gemini ? 'Gemini' : '',
        ].filter(Boolean);
      } else if (daemonFuelProviders === null) {
        daemonFuelProviders = [];
      }
    })
    .catch(() => { if (daemonFuelProviders === null) daemonFuelProviders = []; })
    .finally(() => {
      daemonFuelProbeInFlight = false;
      updateNewSessionFuelBanner();
    });
}

function newSessionAddKeysAction() {
  return { label: 'Add API keys', onClick: () => focusSettingsApiKeys() };
}

const NEW_SESSION_UNFUELED_MESSAGE =
  'This daemon has no model credentials, so the internal agent can’t start. ' +
  'External agents (Codex, Claude Code) sign in with their own accounts and still work.';

// ── Projectless preflight ──
// A daemon launched outside any project reports project_root: null and has
// no default project — a session cannot start without one. Mirrors the
// unfueled preflight: known-projectless blocks submit with a pointer at the
// Project field; unknown (fetch failed, older daemon) never blocks — the
// daemon's structured no_project failure is the backstop.
let daemonProjectless = null; // null = unknown

const NEW_SESSION_NO_PROJECT_MESSAGE =
  'This daemon has no project open. Pick a project directory in the Project field to start a session.';

function newSessionPickProjectAction() {
  return {
    label: 'Pick project',
    onClick: () => {
      const input = document.getElementById('new-session-project-root');
      input?.focus();
      input?.scrollIntoView?.({ block: 'center' });
    },
  };
}

// Shared submit guard (Sessions pane + Station launch): true = blocked.
function newSessionProjectlessBlocked(requestedProjectRoot) {
  if (requestedProjectRoot || daemonProjectless !== true) return false;
  setNewSessionSpawnNotice('error', NEW_SESSION_NO_PROJECT_MESSAGE, newSessionPickProjectAction());
  return true;
}

// A no_project SessionEnded can only come from a failed create (no session
// ever starts under it), so one arriving while a spawn is pending is ours:
// fail the pending notice with the structured class instead of leaving it
// to the timeout or prose-matched log entries.
function maybeFailPendingNewSessionSpawnNoProject(errorKind) {
  if (errorKind !== 'no_project' || !newSessionSpawnPending) return false;
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  setNewSessionSpawnNotice('error', NEW_SESSION_NO_PROJECT_MESSAGE, newSessionPickProjectAction());
  showControlToast('error', NEW_SESSION_NO_PROJECT_MESSAGE);
  return true;
}

// QA readback (window.qa convention): the preflight inputs the
// validate-dashboard harness asserts on — module scope hides them.
// Probe functions stay cheap and side-effect-free.
window.qa = Object.assign(window.qa || {}, {
  sessionsFuel: () => ({
    fueled: dashboardControlTransport?.lastStatus?.fueled ?? null,
    haveStatus: !!dashboardControlTransport?.lastStatus,
    unfueledCached: daemonUnfueledCached,
    projectless: daemonProjectless,
    effectiveAgent: effectiveNewSessionAgentId(),
    configuredAgent: newSessionConfiguredAgent || '',
    bannerHidden: !!document.getElementById('new-session-unfueled-banner')?.classList.contains('hidden'),
    startDisabled: !!document.getElementById('new-session-start-btn')?.disabled,
  }),

});

// ── ui-v2 execution segmented control (design overhaul) ──
// The reference's Auto / Orchestrate / Direct segmented choice with a
// per-choice note (execInfo copy, verbatim — it matches the current
// semantics). The v1 <select id="new-session-execution"> stays the source
// of truth (startNewSession reads it; updateNewSessionAgentFields drives
// its disabled state) — the segments only proxy value + disabled, and the
// select is hidden by ui2-sessions.css under the flag. v1 DOM untouched.
const UI2_EXEC_CHOICES = [
  { value: '', label: 'Auto', note: 'The task-size heuristic decides between a single agent and supervised sub-agents.' },
  { value: 'orchestrate', label: 'Orchestrate', note: 'Delegates the task to supervised sub-agents working in isolated git worktrees.' },
  { value: 'direct', label: 'Direct', note: 'A single agent handles the whole task — no delegation.' },
];
let ui2ExecSegEl = null;
let ui2ExecNoteEl = null;

function ui2SyncExecSeg() {
  if (!ui2ExecSegEl) return;
  const sel = document.getElementById('new-session-execution');
  if (!sel) return;
  const value = sel.value || '';
  const disabled = !!sel.disabled;
  ui2ExecSegEl.classList.toggle('disabled', disabled);
  for (const btn of ui2ExecSegEl.querySelectorAll('button[data-exec]')) {
    const active = (btn.dataset.exec || '') === value;
    btn.classList.toggle('is-accent', active && !disabled);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    btn.disabled = disabled;
  }
  if (ui2ExecNoteEl) {
    const choice = UI2_EXEC_CHOICES.find(c => c.value === value) || UI2_EXEC_CHOICES[0];
    ui2ExecNoteEl.textContent = disabled
      ? 'Execution shape applies to the internal agent — external CLIs run their own loops.'
      : choice.note;
  }
}

if (typeof ui2Enabled === 'function' && ui2Enabled()) {
  const wrap = document.getElementById('new-session-execution-wrap');
  if (wrap && wrap.parentElement) {
    const field = document.createElement('div');
    field.className = 'sessions-new-session-field ui2-exec-field';
    const label = document.createElement('span');
    label.className = 'ui2-exec-label';
    label.textContent = 'Execution';
    const seg = document.createElement('div');
    seg.className = 'ui2-seg ui2-exec-seg';
    seg.setAttribute('role', 'group');
    seg.setAttribute('aria-label', 'Execution shape');
    for (const choice of UI2_EXEC_CHOICES) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.dataset.exec = choice.value;
      btn.textContent = choice.label;
      btn.title = choice.note;
      btn.addEventListener('click', () => {
        const sel = document.getElementById('new-session-execution');
        if (!sel || sel.disabled) return;
        sel.value = choice.value;
        sel.dispatchEvent(new Event('change', { bubbles: true }));
        ui2SyncExecSeg();
      });
      seg.appendChild(btn);
    }
    const note = document.createElement('div');
    note.className = 'sessions-agent-note ui2-exec-note';
    field.append(label, seg, note);
    ui2ExecSegEl = seg;
    ui2ExecNoteEl = note;
    wrap.after(field);
    document.getElementById('new-session-execution')?.addEventListener('change', ui2SyncExecSeg);
    ui2SyncExecSeg();
  }
}

function updateNewSessionFuelBanner() {
  const banner = document.getElementById('new-session-unfueled-banner');
  if (!banner) return;
  refreshFuelStateForBanner();
  const effective = effectiveNewSessionAgentId();
  const internalSelected = effective === 'internal' || !effective;
  const show = internalSelected && daemonInternalUnfueled();
  banner.classList.toggle('hidden', !show);
  const btn = document.getElementById('new-session-start-btn');
  if (btn && !newSessionSpawnPending) {
    btn.disabled = show;
    btn.title = show ? 'Internal sessions need an API key or a vault credential lease' : '';
  }

  // ui-v2 only: the design's green happy-path banner. Shown exclusively
  // when fuel is positively known (status frame `fueled === true` or the
  // key probe found a provider) — an unknown state shows neither banner,
  // never a claimed one. v1 never unhides this element.
  const fueledBanner = document.getElementById('new-session-fueled-banner');
  if (fueledBanner) {
    const ui2 = typeof ui2Enabled === 'function' && ui2Enabled();
    const status = dashboardControlTransport?.lastStatus;
    const knownFueled = (status && status.fueled === true) || daemonUnfueledCached === false;
    const showFueled = ui2 && internalSelected && !show && knownFueled;
    fueledBanner.classList.toggle('hidden', !showFueled);
    if (showFueled) {
      const textEl = document.getElementById('new-session-fueled-text');
      const names = Array.isArray(daemonFuelProviders) && daemonFuelProviders.length > 0
        ? daemonFuelProviders.join(' + ')
        : '';
      if (textEl) {
        textEl.textContent = names
          ? `Fueled — ${names} credentials active, ready to launch.`
          : 'Fueled — model credentials active, ready to launch.';
      }
    }
  }
  ui2SyncExecSeg();
}

function setNewSessionSpawnNotice(kind, text, action) {
  const notice = document.getElementById('new-session-spawn-notice');
  const textEl = document.getElementById('new-session-spawn-text');
  if (!notice || !textEl) return;
  const hasText = !!String(text || '').trim();
  const noticeKind = ['ok', 'warn', 'error'].includes(kind) ? kind : 'pending';
  notice.className = `sessions-spawn-notice ${noticeKind}` + (hasText ? '' : ' hidden');
  textEl.textContent = text || '';
  notice.title = text || '';
  let actionBtn = document.getElementById('new-session-spawn-action');
  if (action && hasText) {
    if (!actionBtn) {
      actionBtn = document.createElement('button');
      actionBtn.id = 'new-session-spawn-action';
      actionBtn.type = 'button';
      actionBtn.className = 'sessions-spawn-action';
      notice.appendChild(actionBtn);
    }
    actionBtn.textContent = action.label;
    actionBtn.onclick = action.onClick;
  } else if (actionBtn) {
    actionBtn.remove();
  }
  stationScheduleUpdate();
}

function clearNewSessionSpawnTimers() {
  if (newSessionSpawnTimeout) clearTimeout(newSessionSpawnTimeout);
  if (newSessionSpawnClearTimeout) clearTimeout(newSessionSpawnClearTimeout);
  newSessionSpawnTimeout = null;
  newSessionSpawnClearTimeout = null;
}

function clearNewSessionSpawnRecent() {
  if (newSessionSpawnRecentTimeout) clearTimeout(newSessionSpawnRecentTimeout);
  newSessionSpawnRecent = null;
  newSessionSpawnRecentTimeout = null;
}

function rememberNewSessionSpawnRecent(sessionId, task) {
  clearNewSessionSpawnRecent();
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  newSessionSpawnRecent = {
    sessionId: sid,
    task: String(task || '').trim(),
    expiresAt: Date.now() + NEW_SESSION_LAUNCH_FAILURE_GRACE_MS,
  };
  newSessionSpawnRecentTimeout = setTimeout(() => {
    newSessionSpawnRecent = null;
    newSessionSpawnRecentTimeout = null;
  }, NEW_SESSION_LAUNCH_FAILURE_GRACE_MS);
}

function isNewSessionLaunchFailureReason(reason) {
  const text = String(reason || '').toLowerCase();
  return text.includes('error') || text.includes('failed') || text.includes('failure');
}

function formatNewSessionLaunchFailureReason(reason) {
  const text = String(reason || '').trim();
  if (!text) return 'Session failed shortly after it started.';
  return `Session failed: ${text.replace(/^error:\s*/i, '')}`;
}

function maybeFailRecentNewSessionSpawn(sessionId, reason, errorKind) {
  const sid = String(sessionId || '').trim();
  if (!sid || !newSessionSpawnRecent) return false;
  if (newSessionSpawnRecent.sessionId !== sid) return false;
  if (Date.now() > Number(newSessionSpawnRecent.expiresAt || 0)) {
    clearNewSessionSpawnRecent();
    return false;
  }
  if (!isNewSessionLaunchFailureReason(reason)) return false;

  const message = formatNewSessionLaunchFailureReason(reason);
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  // Structured failure classes carry an action instead of prose-parsing.
  const action = errorKind === 'unfueled'
    ? newSessionAddKeysAction()
    : errorKind === 'no_project'
      ? newSessionPickProjectAction()
      : null;
  setNewSessionSpawnNotice('error', message, action);
  showControlToast('error', message);
  return true;
}

function beginNewSessionSpawnNotice(task, text, name = '') {
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = true;
  newSessionSpawnTask = String(task || '').trim();
  newSessionSpawnName = String(name || '').trim();
  setNewSessionStartButtonPending(true);
  setNewSessionSpawnNotice('pending', text || 'Spawning new session...');
  newSessionSpawnTimeout = setTimeout(() => {
    if (!newSessionSpawnPending) return;
    newSessionSpawnPending = false;
    newSessionSpawnTask = '';
    newSessionSpawnName = '';
    setNewSessionStartButtonPending(false);
    setNewSessionSpawnNotice('warn', 'No start confirmation yet. Check the Activity log before retrying.');
    showControlToast('info', 'No new-session start confirmation yet.');
  }, NEW_SESSION_SPAWN_TIMEOUT_MS);
}

function updateNewSessionSpawnNotice(kind, text) {
  if (!newSessionSpawnPending) return;
  setNewSessionSpawnNotice(kind, text);
}

function failNewSessionSpawnNotice(text) {
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  setNewSessionSpawnNotice('error', text || 'New session did not start.');
  showControlToast('error', text || 'New session did not start.');
}

function clearNewSessionDraftIfUnchanged(task, name) {
  const expectedTask = String(task || '').trim();
  const input = document.getElementById('new-session-input');
  if (input && expectedTask && input.value.trim() === expectedTask) {
    clearTaskTextarea(input);
  }
  const expectedName = String(name || '').trim();
  const nameInput = document.getElementById('new-session-name');
  if (nameInput && expectedName && nameInput.value.trim() === expectedName) {
    nameInput.value = '';
  }
}

function finishNewSessionSpawnNotice(sessionId, task) {
  if (!newSessionSpawnPending) return;
  const expectedTask = newSessionSpawnTask;
  const expectedName = newSessionSpawnName;
  const actualTask = String(task || '').trim();
  if (expectedTask && actualTask && expectedTask !== actualTask) return;
  clearNewSessionSpawnTimers();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  clearNewSessionDraftIfUnchanged(actualTask || expectedTask, expectedName);
  setNewSessionStartButtonPending(false);
  const shortId = sessionId ? ` (${shortSessionId(sessionId)})` : '';
  setNewSessionSpawnNotice('ok', `Session started${shortId}. Activity is ready.`);
  showControlToast('success', `Session started${shortId}`);
  rememberNewSessionSpawnRecent(sessionId, actualTask || expectedTask);
  newSessionSpawnClearTimeout = setTimeout(() => {
    setNewSessionSpawnNotice('', '');
    newSessionSpawnClearTimeout = null;
  }, 2500);
}

function maybeFailNewSessionSpawnFromLog(c) {
  if (!newSessionSpawnPending || !c) return;
  const level = String(c.level || '').toLowerCase();
  if (level !== 'error') return;
  const content = String(c.content || '').trim();
  if (!/^(Session create failed|Project load failed):/.test(content)) return;
  failNewSessionSpawnNotice(content);
}

async function loadNewSessionProjectRoot() {
  try {
    const d = await fetchProjectRoot();
    // project_root: null = projectless daemon (a rooted daemon always
    // reports a non-empty string). On fetch failure the flag stays
    // unknown and never blocks.
    daemonProjectless = !d.project_root;
    setNewSessionProjectRoot(d.project_root || '');
  } catch (e) {
    console.warn('Failed to load project root:', e);
  }
}

async function startNewSession() {
  const input = document.getElementById('new-session-input');
  if (!input) return;
  const task = input.value.trim();
  if (!task) return;
  if (newSessionSpawnPending) {
    showControlToast('info', 'A new session is already spawning.');
    return;
  }
  if (!app) {
    failNewSessionSpawnNotice('Dashboard is not connected to the server.');
    return;
  }
  const effectiveAgent = effectiveNewSessionAgentId();
  if ((effectiveAgent === 'internal' || !effectiveAgent) && daemonInternalUnfueled()) {
    // Belt-and-braces behind the banner: the fueled flag may have flipped
    // since the last render.
    updateNewSessionFuelBanner();
    setNewSessionSpawnNotice('error', NEW_SESSION_UNFUELED_MESSAGE, newSessionAddKeysAction());
    return;
  }

  const nameInput = document.getElementById('new-session-name');
  const sessionName = nameInput?.value.trim() || '';
  const direct = document.getElementById('direct-mode-toggle')?.checked || false;
  const attachments = pendingAttachments.map(a => a.frameId);
  const attachmentReceipt = pendingAttachments.slice();
  const requestedProjectRoot = document.getElementById('new-session-project-root')?.value.trim() || '';
  if (newSessionProjectlessBlocked(requestedProjectRoot)) return;
  beginNewSessionSpawnNotice(
    task,
    requestedProjectRoot ? 'Checking project directory...' : 'Spawning new session...',
    sessionName
  );

  let projectRoot = '';
  try {
    projectRoot = await ensureNewSessionProjectDirectory(requestedProjectRoot);
  } catch (e) {
    failNewSessionSpawnNotice(e?.message || 'Project directory check failed.');
    return;
  }
  if (requestedProjectRoot && !projectRoot) {
    failNewSessionSpawnNotice('Project directory needs attention before the session can start.');
    return;
  }

  const msg = { action: 'create_session', task: task };
  if (sessionName) msg.name = sessionName;
  if (projectRoot) msg.project_root = projectRoot;
  const agentValue = document.getElementById('new-session-agent')?.value || '';
  const selectedAgent = normalizeAgentId(agentValue);
  if (agentValue === 'internal') {
    msg.agent = 'internal';
  } else if (selectedAgent) {
    msg.agent = selectedAgent;
    const agentCommand = document.getElementById('new-session-agent-command')?.value.trim() || '';
    if (agentCommand) msg.agent_command = agentCommand;
  }
  if (effectiveNewSessionAgentId() === 'claude-code') {
    const modelChoice = document.getElementById('new-session-claude-model-select')?.value || '';
    const model = modelChoice === '__custom__'
      ? (document.getElementById('new-session-claude-model')?.value.trim() || '')
      : modelChoice;
    if (model) msg.claude_model = model;
    const mode = document.getElementById('new-session-claude-permission-mode')?.value || '';
    if (mode) msg.claude_permission_mode = mode;
    const effort = document.getElementById('new-session-claude-effort')?.value || '';
    if (effort) msg.claude_effort = effort;
  }
  if (effectiveNewSessionAgentId() === 'codex') {
    if (newSessionCodexLaunchDefaultsLoaded) {
      msg.codex_sandbox = normalizeCodexSandbox(
        document.getElementById('new-session-codex-sandbox')?.value || newSessionCodexSandbox
      );
      msg.codex_approval_policy = normalizeCodexApprovalPolicy(
        document.getElementById('new-session-codex-approval-policy')?.value || newSessionCodexApprovalPolicy
      );
      const mode = document.getElementById('new-session-codex-managed-context')?.value === 'managed'
        ? 'managed'
        : 'vanilla';
      msg.codex_managed_context = mode;
      const archiveMode = normalizeContextArchiveMode(
        document.getElementById('new-session-codex-context-archive')?.value || newSessionCodexContextArchive
      );
      msg.codex_context_archive = archiveMode;
      const fastChecked = !!document.getElementById('new-session-codex-fast')?.checked;
      if (fastChecked) {
        msg.codex_service_tier = 'priority';
      } else if (codexServiceTierIsFast(newSessionCodexDefaultServiceTier)) {
        msg.codex_service_tier = 'standard';
      }
    }
  }
  // Execution shape: an explicit per-launch choice beats the global Direct
  // toggle; Auto (or an external agent — the select is disabled and cleared
  // then) preserves the old behavior of the toggle forcing direct.
  const executionSel = document.getElementById('new-session-execution');
  const execution = executionSel && !executionSel.disabled ? executionSel.value : '';
  if (execution === 'orchestrate') {
    msg.orchestrate = true;
  } else if (execution === 'direct' || direct) {
    msg.direct = true;
  }
  if (attachments.length > 0) msg.attachments = attachments;

  try {
    const sent = dispatchSessionControlMsg(msg, {
      onError: err => failNewSessionSpawnNotice(err?.message || 'Failed to send new-session request.'),
    });
    if (!sent) throw new Error('Dashboard is not connected to the server.');
  } catch (e) {
    failNewSessionSpawnNotice(e?.message || 'Failed to send new-session request.');
    return;
  }

  updateNewSessionSpawnNotice('pending', 'Spawning new session...');
  showControlToast('info', 'Spawning new session...');
  resetNewSessionCodexFastModeToDefault();
  renderNewSessionAgentControls();
  if (attachments.length > 0) {
    renderAttachmentReceipt(task, attachmentReceipt, 'Sent');
    clearPendingAttachments({ retainPreviewUrls: true });
  }
}
window.startNewSession = startNewSession;

