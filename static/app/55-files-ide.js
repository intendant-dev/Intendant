// ── Files tab: editor ──
//
// A small IDE over the fs API family. Every fs call rides the daemonApi
// facade (32-daemon-api.js, transport program F1): target '' is this
// daemon (tunnel first, direct HTTP per the facade's verb-derived fallback
// policy), a hostId is that peer's dashboard-control tunnel. Peers enforce
// their own IAM profile + filesystem write roots server-side; this UI only
// decides *where* to send a request, never *whether* it is allowed.
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
  const accent = hostId ? 'var(--violet)' : 'var(--sky)';
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

// -- transport: every fs call rides the daemonApi facade (F1). hostId ''
//    targets this daemon — the facade prefers the dashboard-control tunnel
//    and applies the verb-derived fallback policy (GET twins may fall back
//    to HTTP and retry; POST twins never replay after an attempted send).
//    A non-empty hostId targets that peer's tunnel (peers have no HTTP
//    lane by design). Availability checks route through
//    daemonApi.availability; the read-only / reconnect envelopes below
//    keep the exact strings this UI surfaced before the migration.

function filesIdeRpc(hostId, method, params) {
  return daemonApi.request(method, params, { target: hostId || null });
}

function filesIdeList(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_list', { path });
}

function filesIdeStat(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_stat', { path });
}

function filesIdeMkdir(hostId, path) {
  return filesIdeRpc(hostId, 'api_fs_mkdir', { path });
}

function filesIdeRename(hostId, from, to) {
  return filesIdeRpc(hostId, 'api_fs_rename', { from, to });
}

function filesIdeDeleteRpc(hostId, path, recursive) {
  return filesIdeRpc(hostId, 'api_fs_delete', recursive ? { path, recursive: true } : { path });
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
/// the daemon when it offers one (byte-stream result.sha256 / the HTTP
/// X-Content-Sha256 header — both land in meta.sha256) so the conflict
/// baseline matches what the daemon will hash at save time; older peers
/// without it fall back to a local digest of the same bytes.
async function filesIdeReadFile(hostId, path) {
  if (!hostId && dashboardConnectModeEnabled() && !daemonApi.availability('api_fs_read').ok) {
    throw new Error('File reads are unavailable until this dashboard reconnects');
  }
  const { bytes, meta } = await daemonApi.bytes('api_fs_read', { path }, { target: hostId || null });
  const sha256 = meta.sha256 || (await filesIdeSha256Hex(bytes));
  return { bytes, sha256 };
}

/// Write a whole file. Returns the facade's {ok, status, body} envelope;
/// 409 bodies carry {code, current_sha256, ...} for the conflict banner.
/// api_fs_write is a POST twin, so the facade never replays it over HTTP
/// after a tunnel attempt that may have reached the daemon.
async function filesIdeWriteFile(hostId, path, bytes, opts = {}) {
  const params = { path };
  if (opts.expected_sha256) params.expected_sha256 = opts.expected_sha256;
  if (opts.create_new) params.create_new = true;
  if (opts.force) params.force = true;
  if (hostId) {
    // A peer granting read-only file access advertises
    // api_fs_write_available:false — surface the same friendly envelope
    // this UI always showed instead of the raw authorizer text.
    const avail = daemonApi.availability('api_fs_write', hostId);
    if (!avail.ok && avail.reason === 'denied') {
      return {
        ok: false,
        status: 403,
        body: { error: `${filesIdeHostLabel(hostId)} grants read-only file access to this daemon` },
      };
    }
  } else if (dashboardConnectModeEnabled() && !daemonApi.availability('api_fs_write').ok) {
    return { ok: false, status: 503, body: { error: 'File writes are unavailable until this dashboard reconnects' } };
  }
  return daemonApi.upload('api_fs_write', params, bytes, {
    target: hostId || null,
    signal: opts.signal,
    // Family default when the caller sets no deadline: scale with the
    // payload like the transfers lane does — the transport's flat
    // per-method default is sized for small JSON RPCs, not a fs write
    // that may carry up to UPLOAD_MAX_BYTES.
    timeoutMs: opts.timeoutMs ?? rangedDownloadTimeoutMs(Number(bytes?.byteLength ?? bytes?.size) || 0),
  });
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
  const path = filesIdeJoinPath(creating.dir, name);
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

/* ── Separator-aware path math ──
   The daemon serializes native paths: a Windows peer hands this UI
   `C:\Users\...` while Unix daemons hand `/home/...`. Every join/dirname
   below keys off the path's own separator (presence of a backslash or a
   drive prefix) instead of assuming POSIX. */
function filesIdePathSeparator(path) {
  const text = String(path || '');
  return (text.includes('\\') || /^[A-Za-z]:/.test(text)) ? '\\' : '/';
}

function filesIdeDirname(path) {
  const text = String(path || '');
  if (filesIdePathSeparator(text) === '\\') {
    const trimmed = text.replace(/[\\/]+$/, '');
    // A bare drive letter is drive-relative — keep the root form.
    const rootForm = value => (/^[A-Za-z]:$/.test(value) ? value + '\\' : value);
    const idx = Math.max(trimmed.lastIndexOf('\\'), trimmed.lastIndexOf('/'));
    if (idx <= 0) return rootForm(trimmed) || '\\';
    return rootForm(trimmed.slice(0, idx));
  }
  const trimmed = text.replace(/\/+$/, '');
  const idx = trimmed.lastIndexOf('/');
  return idx > 0 ? trimmed.slice(0, idx) : '/';
}

function filesIdeJoinPath(dir, name) {
  const base = String(dir || '');
  const sep = filesIdePathSeparator(base);
  const trimmed = base.replace(sep === '\\' ? /[\\/]+$/ : /\/+$/, '');
  if (!trimmed) return (base.startsWith('/') ? '/' : '') + name; // unix root
  if (/^[A-Za-z]:$/.test(trimmed)) return trimmed + '\\' + name; // drive root
  return trimmed + sep + name;
}

/* "Everything under `path`": the prefix descendants start with, in the
   path's own separator (rename/delete retargeting, subtree drops). */
function filesIdeChildPrefix(path) {
  const text = String(path || '');
  const sep = filesIdePathSeparator(text);
  return text.replace(sep === '\\' ? /[\\/]+$/ : /\/+$/, '') + sep;
}

// Legacy name kept for readability at the call sites that grew up with it.
function filesIdeParentDir(path) {
  return filesIdeDirname(path);
}

/// Forget cached tree state under a renamed or deleted directory: its own
/// listing, every descendant listing, and their expansion flags. (Called
/// from the rename/delete flows; without it a stale child listing
/// resurrects rows for paths that no longer exist.)
function filesIdeDropSubtreeState(state, path) {
  const prefix = filesIdeChildPrefix(path);
  for (const key of Array.from(state.listings.keys())) {
    if (key === path || key.startsWith(prefix)) state.listings.delete(key);
  }
  for (const key of Array.from(state.expanded)) {
    if (key === path || key.startsWith(prefix)) state.expanded.delete(key);
  }
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
  const to = filesIdeJoinPath(renaming.dir, name);
  try {
    const resp = await filesIdeRename(hostId, from, to);
    if (!resp.ok) throw new Error(resp.body.error || `Rename failed (${resp.status})`);
    const finalPath = typeof resp.body.path === 'string' && resp.body.path ? resp.body.path : to;
    filesIdeRetargetBuffers(hostId, from, finalPath, renaming.isDir);
    if (renaming.isDir) filesIdeDropSubtreeState(state, from);
    if (state.contextDir === from || (renaming.isDir && state.contextDir.startsWith(filesIdeChildPrefix(from)))) {
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
    if (state.contextDir === path || (isDir && state.contextDir.startsWith(filesIdeChildPrefix(path)))) {
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
  const prefix = filesIdeChildPrefix(oldPath);
  for (const buffer of Array.from(filesIdeBuffers.values())) {
    if ((buffer.host || '') !== host) continue;
    if (buffer.path === oldPath) {
      filesIdeRekeyBuffer(buffer, newPath);
    } else if (isDir && buffer.path.startsWith(prefix)) {
      filesIdeRekeyBuffer(buffer, filesIdeJoinPath(newPath, buffer.path.slice(prefix.length)));
    }
  }
}

function filesIdeRekeyBuffer(buffer, newPath) {
  const oldKey = buffer.key;
  const newKey = filesIdeBufferKey(buffer.host, newPath);
  // Carry any persisted draft along to the new identity.
  const draft = filesIdeDraftRead(oldKey);
  if (draft) {
    filesIdeDraftClear(oldKey);
    try {
      localStorage.setItem(filesIdeDraftStorageKey(newKey), JSON.stringify(draft));
    } catch (_) { /* best-effort */ }
  }
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
  const prefix = filesIdeChildPrefix(path);
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

/* ── Dirty-buffer drafts ──
   Unsaved edits persist to localStorage (debounced on change) so a tab
   crash or accidental navigation loses nothing the beforeunload guard
   could not stop. On reopening a file with a draft present: disk still
   at the draft's baseline sha → offer "Restore draft"; disk moved on →
   restore the draft and route through the existing conflict banner
   (Reload / Overwrite). Cleared on save and on close-clean/discard.
   Best-effort by design: storage failures never block editing. */
const FILES_IDE_DRAFT_PREFIX = 'intendant.ui2.draft.';
const FILES_IDE_DRAFT_MAX_CHARS = 200 * 1024; // ~200 KB per draft

function filesIdeDraftStorageKey(bufferKey) {
  return FILES_IDE_DRAFT_PREFIX + bufferKey;
}

function filesIdeDraftRead(bufferKey) {
  try {
    const raw = localStorage.getItem(filesIdeDraftStorageKey(bufferKey));
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed.text === 'string' ? parsed : null;
  } catch (_) {
    return null;
  }
}

function filesIdeDraftClear(bufferKey) {
  try { localStorage.removeItem(filesIdeDraftStorageKey(bufferKey)); } catch (_) {}
}

function filesIdeDraftWrite(buffer) {
  const text = buffer.doc.getValue('\n');
  if (text.length > FILES_IDE_DRAFT_MAX_CHARS) {
    // Too large to keep: drop any older (smaller) draft so a stale copy
    // can never masquerade as the current buffer, and say so once.
    filesIdeDraftClear(buffer.key);
    if (buffer.key === filesIdeActiveKey && !buffer.draftCapNoted) {
      buffer.draftCapNoted = true;
      filesIdeSetSaveStatus('', 'Draft not kept — buffer exceeds the 200 KB draft cap');
    }
    return;
  }
  try {
    localStorage.setItem(filesIdeDraftStorageKey(buffer.key), JSON.stringify({
      text,
      baseSha: buffer.baselineSha || '',
      eol: buffer.eol,
      savedAt: Date.now(),
    }));
  } catch (_) { /* storage full or blocked — drafts stay best-effort */ }
}

function filesIdeDraftSchedule(buffer) {
  clearTimeout(buffer._draftTimer);
  buffer._draftTimer = setTimeout(() => {
    buffer._draftTimer = null;
    if (filesIdeBuffers.get(buffer.key) !== buffer) return; // closed / re-keyed
    if (filesIdeBufferDirty(buffer)) filesIdeDraftWrite(buffer);
    else filesIdeDraftClear(buffer.key);
  }, 800);
}

function filesIdeRestoreDraftActive() {
  const buffer = filesIdeActiveBuffer();
  if (!buffer || !buffer.draftOffer) return;
  const draft = buffer.draftOffer;
  buffer.draftOffer = null;
  if (draft.eol === '\r\n' || draft.eol === '\n') buffer.eol = draft.eol;
  buffer.doc.setValue(draft.text); // change event marks dirty + re-schedules
  filesIdeRenderTabs();
  filesIdeRenderChrome();
}

function filesIdeDiscardDraftActive() {
  const buffer = filesIdeActiveBuffer();
  if (!buffer || !buffer.draftOffer) return;
  buffer.draftOffer = null;
  filesIdeDraftClear(buffer.key);
  filesIdeRenderChrome();
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
    const normalized = text.replace(/\r\n/g, '\n');
    const doc = window.CodeMirror.Doc(normalized, filesIdeModeSpecFor(name));
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
      draftOffer: null,
      lastError: '',
      cleanGeneration: doc.changeGeneration(),
    };
    // Draft recovery: a persisted dirty buffer for this exact file.
    if (!options.createNew) {
      const draft = filesIdeDraftRead(key);
      if (draft && draft.text === normalized) {
        filesIdeDraftClear(key); // stale leftover — disk already matches
      } else if (draft) {
        if (sha && (draft.baseSha || '') === sha) {
          buffer.draftOffer = draft; // disk unchanged: offer to restore
        } else {
          // Disk moved on (or no verifiable baseline): restore the draft
          // and route through the existing conflict banner. setValue after
          // cleanGeneration was captured marks the buffer dirty, and the
          // save baseline becomes the DRAFT's baseline so a plain Save
          // 409s into the banner instead of silently clobbering the newer
          // disk content — Overwrite stays the explicit click it is today.
          if (draft.eol === '\r\n' || draft.eol === '\n') buffer.eol = draft.eol;
          doc.setValue(draft.text);
          buffer.baselineSha = draft.baseSha || '';
          buffer.conflict = { code: 'conflict', currentSha: sha || '' };
        }
      }
    }
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
      filesIdeDraftSchedule(buffer);
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
    // Ln/Col statusbar indicator (filesIdeUpdateLnCol reads the cursor).
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
  // Close is clean or an explicit "Discard?" confirmation — either way
  // the persisted draft's job is over.
  clearTimeout(buffer._draftTimer);
  buffer._draftTimer = null;
  filesIdeDraftClear(key);
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

// Ln/Col cursor indicator in the editor statusbar (`host · Language ·
// LF · Ln 8, Col 24`), fed by CodeMirror's cursorActivity events; kept
// empty while no buffer is open so the flex gap contributes nothing.
function filesIdeUpdateLnCol() {
  const el = document.getElementById('files-ide-status-lncol');
  if (!el) return;
  const buffer = filesIdeActiveBuffer();
  if (!buffer || !filesIdeCm) {
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
    } else if (buffer?.draftOffer) {
      const savedAt = Number(buffer.draftOffer.savedAt || 0);
      const when = savedAt ? ` from ${new Date(savedAt).toLocaleString()}` : '';
      banner.className = 'files-ide-banner';
      banner.innerHTML =
        `<span>${escapeHtml(`An unsaved draft of this file${when} is stored in this browser.`)}</span>` +
        `<button type="button" class="ui-btn" onclick="filesIdeRestoreDraftActive()">Restore draft</button>` +
        `<button type="button" class="ui-btn" onclick="filesIdeDiscardDraftActive()">Discard draft</button>`;
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
    buffer.draftOffer = null;
    // Compare against the generation captured before serialize: keystrokes
    // that landed while the save was in flight keep the buffer dirty.
    buffer.cleanGeneration = generation;
    // The disk now holds this content — the browser draft is obsolete. A
    // still-pending draft timer is left alone: it re-checks dirtiness at
    // fire time, so keystrokes that landed mid-save re-draft themselves.
    filesIdeDraftClear(buffer.key);
    filesIdeSetSaveStatus('ok', `Saved ${new Date().toLocaleTimeString()}`);
    const state = filesIdeTreeState(buffer.host);
    const dir = filesIdeDirname(buffer.path);
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
const FILES_IDE_FIND_MATCH_CAP = 10000;
// True when the last scan hit FILES_IDE_FIND_MATCH_CAP — the count pill
// must say "first N shown" instead of implying completeness.
let filesIdeFindTruncated = false;

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
  filesIdeFindTruncated = false;
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
    el.title = '';
    el.classList.remove('none');
    return;
  }
  // Honest caps: the scan stops at FILES_IDE_FIND_MATCH_CAP matches and
  // only the first FILES_IDE_FIND_MARK_CAP are highlighted — a bare
  // "1 / 10000" would imply a complete count and full highlighting.
  const total = filesIdeFindMatches.length;
  const suffix = filesIdeFindTruncated ? `+ (first ${total} shown)` : '';
  el.textContent = total ? `${filesIdeFindIndex + 1} / ${total}${suffix}` : 'No matches';
  el.title = filesIdeFindTruncated
    ? `Search stopped after the first ${total} matches; refine the query to see the rest.`
    : (total > FILES_IDE_FIND_MARK_CAP
      ? `Highlighting the first ${FILES_IDE_FIND_MARK_CAP} matches; stepping still reaches all ${total}.`
      : '');
  el.classList.toggle('none', !total);
}

function filesIdeFindRecompute(options = {}) {
  if (!filesIdeCm || !filesIdeFindOpen) return;
  filesIdeFindClearMarks();
  filesIdeFindMatches = [];
  filesIdeFindIndex = -1;
  filesIdeFindTruncated = false;
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
    if (filesIdeFindMatches.length >= FILES_IDE_FIND_MATCH_CAP) {
      filesIdeFindTruncated = true;
      break;
    }
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
// Whose disk the picker lists: '' = this daemon, a host id = that peer's
// dashboard-control tunnel (the same daemonApi lane the IDE tree browses
// peers through). Set per-open by configureFsPicker.
let fsPickerHostId = '';

function configureFsPicker({ mode, target, title, placeholder, useLabel, showCreate, multiSelect, hostId }) {
  fsPickerMode = mode || 'directory';
  fsPickerTarget = target || 'project';
  fsPickerHostId = String(hostId || '');
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
  const resp = await filesIdeStat(fsPickerHostId, target);
  const status = resp.body;
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
    const resp = await filesIdeList(fsPickerHostId, resolved.listPath);
    const data = resp.body;
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

/* Peer browsing rides the same daemonApi fs lane the IDE tree already
   proves out (api_fs_list/api_fs_stat with target: hostId); 'never' is
   the only truly-unreachable availability — 'transport-down' means the
   request itself will dial the peer tunnel, exactly like the tree. The
   manual-path input stays as the escape hatch either way. */
function filesDownloadPeerBrowsable(peerId) {
  const availability = daemonApi.availability('api_fs_list', peerId);
  return availability.ok || availability.reason === 'transport-down';
}

function openFilesDownloadPicker() {
  const peerId = filesDownloadSelectedPeerId();
  if (peerId && !filesDownloadPeerBrowsable(peerId)) {
    setFilesDownloadStatus('warn', `Browsing ${filesDownloadPeerLabel(peerId)} is unavailable from this dashboard; enter a full path`);
    return;
  }
  configureFsPicker({
    mode: 'file',
    target: 'filesDownload',
    hostId: peerId,
    title: peerId ? `Choose files on ${filesDownloadPeerLabel(peerId)}` : 'Choose files to download',
    placeholder: '/path/to/file',
    useLabel: 'Download',
    showCreate: false,
    multiSelect: true,
  });
  const modal = document.getElementById('fs-picker-modal');
  if (modal) modal.style.display = 'flex';
  // dashboardProjectRoot is this daemon's disk — a peer starts at its
  // own home instead.
  loadFsPicker(filesDownloadPathValue() || (peerId ? '~' : (dashboardProjectRoot || '~')));
}

/* 54's transfer gates (onFilesDownloadHostChanged / setFilesDownloadBusy)
   now consult filesDownloadPeerBrowsable directly, so peer selection no
   longer force-disables Browse; no reconciliation needed here. */

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
  // api_fs_mkdir is a POST twin: the facade's no-replay policy covers the
  // fallbackAfterRpcFailure:false this call used to pass by hand.
  const resp = await filesIdeMkdir(fsPickerHostId, path);
  const data = resp.body;
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
	window.filesIdeRestoreDraftActive = filesIdeRestoreDraftActive;
	window.filesIdeDiscardDraftActive = filesIdeDiscardDraftActive;
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
