// ── Chapter navigation ─────────────────────────────────────────────────
// "Chapter" jumps through a session's log: previous/next USER message
// (mode 'user'), or previous/next agent PROSE reply (mode 'agent' —
// model text only, never reasoning rows, tool calls, or output). Two
// surfaces share the logic:
//
//   - session-window panes (Activity grid / Focus / maximized): one
//     floating singleton cluster attaches to the hovered or focused
//     pane — ZERO per-pane DOM, zero per-message state. The chapter
//     index lives on the pane (win.chapterNav), is built lazily on
//     first use from win.logHistory (the in-memory model, NOT the DOM:
//     rendering is windowed and most rows are off-DOM), appended
//     incrementally while the shape of the history array proves a pure
//     append, and rebuilt otherwise (trim/prepend/dedup all break the
//     shape check; a rebuild is one linear scan).
//   - the Sessions detail Logs view: a second cluster instance in the
//     pager toolbar, indexing view.rows (already verbosity- and
//     user-filter-applied, so every target is a visible row).
//
// Classification derives from the vocabulary the renderers already
// style rows with — never a parallel list:
//   user  = the row the renderer marks `source-user` (the YOU row),
//           minus external tool results, using the SAME predicate the
//           detail view relabels them with (sessionDetailToolResultShape;
//           external sources only, exactly like buildSessionDetailRows).
//           In the detail lane the row's own displaySource IS the
//           decision, so it is read directly.
//   agent = level 'model' rows minus kind 'reasoning' (41b's thinking
//           rows) — the renderer's "model prose" styling class.
//
// Older history beyond the loaded page: a prev-jump at the first loaded
// chapter triggers the surface's existing remote loader and parks a
// single-shot intent; the loaders call the chapterNav*HistoryLoaded
// seams after merging, which completes the jump onto the newly revealed
// chapter (or reports the true start).
//
// Shortcuts (routed from 58-shortcuts-boot.js's keydown listener):
//   Alt+ArrowUp / Alt+ArrowDown             previous / next user message
//   Alt+Shift+ArrowUp / Alt+Shift+ArrowDown previous / next agent reply

const CHAPTER_NAV_MODES = ['user', 'agent'];
const CHAPTER_NAV_PENDING_TTL_MS = 15000;
const CHAPTER_NAV_JUMP_PAD_PX = 8;
const CHAPTER_NAV_TOOL_SHAPE_HEAD_CHARS = 600;
let chapterNavMode = 'user';
let chapterNavPaneCluster = null; // { el, modeBtn, prevBtn, nextBtn, count, host }
let chapterNavDetailCluster = null;

const chapterNavIsMacPlatform = /Mac|iP/.test(
  (typeof navigator !== 'undefined' && navigator.platform) || ''
);

function chapterNavModeLabel(mode) {
  return mode === 'agent' ? 'Agent' : 'You';
}

function chapterNavShortcutHint(mode, dir) {
  const arrow = dir < 0 ? '↑' : '↓';
  const mods = mode === 'agent'
    ? (chapterNavIsMacPlatform ? '⌥⇧' : 'Alt+Shift+')
    : (chapterNavIsMacPlatform ? '⌥' : 'Alt+');
  return `${mods}${arrow}`;
}

function chapterNavStepTitle(mode, dir) {
  const what = mode === 'agent' ? 'agent reply' : 'user message';
  return `${dir < 0 ? 'Previous' : 'Next'} ${what} (${chapterNavShortcutHint(mode, dir)})`;
}

// ── Classification (derived predicates) ────────────────────────────────

function chapterNavToolShapedContent(kind, contentHead) {
  return sessionDetailToolResultShape({ kind, content: contentHead });
}

// Pane history items are rendered .log-entry nodes (live lane) or plain
// records (restore/replay lane). `external` = the pane supervises an
// external backend, the only lane whose user-role rows can carry tool
// results (same scoping as the detail view's Tool relabel).
function chapterNavItemMode(item, external) {
  const node = sessionWindowHistoryNode(item);
  if (node) {
    if (!node.classList || !node.classList.contains('log-entry')) return null;
    if (node.classList.contains('source-user')) {
      if (external) {
        const head = (node.querySelector('.log-content')?.textContent || '')
          .slice(0, CHAPTER_NAV_TOOL_SHAPE_HEAD_CHARS);
        if (chapterNavToolShapedContent(node.dataset.kind || '', head)) return null;
      }
      return 'user';
    }
    if (node.dataset.level === 'model' && node.dataset.kind !== 'reasoning') return 'agent';
    return null;
  }
  const record = sessionWindowHistoryRecord(item);
  if (!record) return null;
  const content = String(record.content || '');
  if (!content.trim()) return null; // materializes to no row
  const source = String(record.source || '').toLowerCase();
  if (source === 'user') {
    if (record.tool_use_id) return null;
    if (external
        && chapterNavToolShapedContent(record.kind, content.slice(0, CHAPTER_NAV_TOOL_SHAPE_HEAD_CHARS))) {
      return null;
    }
    return 'user';
  }
  if (record.level === 'model' && record.kind !== 'reasoning') return 'agent';
  return null;
}

// Detail rows already carry the renderer's own source decision
// (displaySource: 'User' vs the Tool relabel), so read it directly.
function chapterNavDetailRowMode(row) {
  if (!row || row.kind !== 'entry') return null;
  if (row.displaySource === sessionDetailSourceLabels.user) return 'user';
  const record = row.record || {};
  if (record.level === 'model' && record.kind !== 'reasoning') return 'agent';
  return null;
}

// ── Index build / validation ───────────────────────────────────────────

function chapterNavBlankIndex(arr) {
  return { arr, len: 0, lastItem: null, user: [], agent: [], cursor: null, pending: null };
}

// Pane index over win.logHistory. Validation: same array ref, length did
// not shrink, and the previously-last item still sits at its old slot —
// anything else (trim's re-slice, remote prepend, dedup splice) forces a
// full rebuild. A pure live append extends the scan incrementally.
function chapterNavPaneIndex(win) {
  const history = ensureSessionWindowHistory(win);
  let ix = win.chapterNav;
  const appendOnly = ix && ix.arr === history && history.length >= ix.len
    && (ix.len === 0 || history[ix.len - 1] === ix.lastItem);
  if (!appendOnly) {
    const pending = ix?.pending || null;
    ix = win.chapterNav = chapterNavBlankIndex(history);
    ix.pending = pending;
  }
  if (ix.len < history.length) {
    const external = typeof externalSourceForSessionWindow === 'function'
      ? !!externalSourceForSessionWindow(win.sessionId, win)
      : false;
    for (let i = ix.len; i < history.length; i++) {
      const mode = chapterNavItemMode(history[i], external);
      if (mode) ix[mode].push(i);
    }
    ix.len = history.length;
    ix.lastItem = history[history.length - 1] || null;
    ix.cursor = null;
  }
  return ix;
}

// Detail index over view.rows — that array is rebuilt wholesale on any
// change (rebuildSessionDetailViewRows), so identity + length pin it.
function chapterNavDetailIndex(view) {
  const rows = Array.isArray(view.rows) ? view.rows : [];
  let ix = view.chapterNav;
  if (!ix || ix.arr !== rows || ix.len !== rows.length) {
    const pending = ix?.pending || null;
    ix = view.chapterNav = chapterNavBlankIndex(rows);
    ix.pending = pending;
    for (let i = 0; i < rows.length; i++) {
      const mode = chapterNavDetailRowMode(rows[i]);
      if (mode) ix[mode].push(i);
    }
    ix.len = rows.length;
    ix.lastItem = rows.length ? rows[rows.length - 1] : null;
  }
  return ix;
}

// ── Stepping ───────────────────────────────────────────────────────────

// Largest mark < idx (dir -1) / smallest mark > idx (dir +1), by binary
// search over the sorted mark list. Returns -1 at the edge.
function chapterNavStepMark(marks, idx, dir) {
  if (!marks.length) return -1;
  let lo = 0;
  let hi = marks.length - 1;
  if (dir < 0) {
    if (marks[0] >= idx) return -1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (marks[mid] < idx) lo = mid; else hi = mid - 1;
    }
    return marks[lo];
  }
  if (marks[hi] <= idx) return -1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (marks[mid] > idx) hi = mid; else lo = mid + 1;
  }
  return marks[lo];
}

// The reader's current position: the last jump target while the scroll
// position is untouched since (deterministic repeat-stepping), otherwise
// the first row overlapping the viewport top.
function chapterNavAnchorIndex(scroller, ix, indexAttr, fallback) {
  if (ix.cursor && Math.abs(scroller.scrollTop - ix.cursor.scrollTop) <= 2) {
    return { idx: ix.cursor.mark, atCursor: true };
  }
  const top = scroller.scrollTop + 1;
  for (const child of scroller.children) {
    const idx = Number(child.dataset?.[indexAttr]);
    if (!Number.isInteger(idx)) continue;
    if (child.offsetTop + child.offsetHeight > top) return { idx, atCursor: false };
  }
  return { idx: fallback, atCursor: false };
}

// Anchor semantics for stepping: standing exactly ON a mark (cursor)
// excludes it in both directions; a free-scroll anchor row itself counts
// for prev (its top is above you only if you scrolled past it — treat
// "row at viewport top" as current) but not for next.
function chapterNavTargetFrom(marks, anchor, dir) {
  if (anchor.atCursor) return chapterNavStepMark(marks, anchor.idx, dir);
  if (dir < 0) return chapterNavStepMark(marks, anchor.idx, -1);
  return chapterNavStepMark(marks, anchor.idx, 1);
}

function chapterNavMarkPosition(marks, target) {
  let lo = 0;
  let hi = marks.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (marks[mid] === target) return mid + 1;
    if (marks[mid] < target) lo = mid + 1; else hi = mid - 1;
  }
  return 0;
}

// ── Row highlight ──────────────────────────────────────────────────────

function chapterNavHighlightRow(node, mode) {
  if (!node) return;
  node.style.setProperty(
    '--chapter-hit-rgb',
    mode === 'agent' ? 'var(--iris-rgb)' : 'var(--amber-rgb)'
  );
  node.classList.remove('chapter-nav-hit');
  // Restart the animation when re-targeting the same row.
  void node.offsetWidth;
  node.classList.add('chapter-nav-hit');
  const clear = () => {
    node.classList.remove('chapter-nav-hit');
    node.style.removeProperty('--chapter-hit-rgb');
  };
  node.addEventListener('animationend', clear, { once: true });
  setTimeout(clear, 1600);
}

// ── Pane jumps ─────────────────────────────────────────────────────────

function chapterNavJumpPane(win, mode, dir) {
  if (!win || !win.log || win.minimized) return false;
  const ix = chapterNavPaneIndex(win);
  const marks = ix[mode] || [];
  const fallback = Math.max(win.renderStart || 0, (win.renderEnd || 1) - 1);
  const anchor = chapterNavAnchorIndex(win.log, ix, 'historyIndex', fallback);
  const target = chapterNavTargetFrom(marks, anchor, dir);
  if (target < 0) {
    if (dir < 0 && win.remoteHasOlder && !win.remoteLoadingOlder
        && typeof loadOlderRemoteSessionWindowEntries === 'function') {
      ix.pending = {
        mode,
        firstMarkItem: marks.length ? ix.arr[marks[0]] : null,
        at: Date.now(),
      };
      chapterNavSetCountLoading(chapterNavClusterForPane(win));
      loadOlderRemoteSessionWindowEntries(win);
      return true;
    }
    chapterNavFlashEdge(chapterNavClusterForPane(win));
    return false;
  }
  if (target < win.renderStart || target >= win.renderEnd) {
    renderSessionWindowRange(win, Math.max(0, target - 3));
  }
  const node = win.log.querySelector(`[data-history-index="${target}"]`);
  if (!node) return false;
  win.followOutput = false;
  win.log.scrollTop = Math.max(0, node.offsetTop - CHAPTER_NAV_JUMP_PAD_PX);
  ix.cursor = { mark: target, scrollTop: win.log.scrollTop };
  chapterNavHighlightRow(node, mode);
  const cluster = chapterNavClusterForPane(win);
  chapterNavUpdateCount(cluster, mode, marks, target);
  chapterNavPulseVisible(cluster);
  return true;
}

// Seam called by loadOlderRemoteSessionWindowEntries after a fetched
// page merged and re-rendered: finish a parked prev-jump onto the
// newest chapter older than the one the reader was stuck on.
function chapterNavPaneHistoryLoaded(win) {
  const pending = win?.chapterNav?.pending;
  if (!pending) return;
  win.chapterNav.pending = null;
  if (Date.now() - pending.at > CHAPTER_NAV_PENDING_TTL_MS) return;
  const ix = chapterNavPaneIndex(win);
  const marks = ix[pending.mode] || [];
  if (!marks.length) return;
  // Boundary: the previously-first chapter's item (identity is stable
  // through the loader's splice). No prior chapters → step back from the
  // end; boundary deduped away → land on the oldest loaded chapter (the
  // reader was walking backward).
  const boundary = pending.firstMarkItem ? ix.arr.indexOf(pending.firstMarkItem) : ix.arr.length;
  const target = boundary >= 0 ? chapterNavStepMark(marks, boundary, -1) : marks[0];
  if (target < 0) {
    chapterNavFlashEdge(chapterNavClusterForPane(win));
    chapterNavUpdateCount(chapterNavClusterForPane(win), pending.mode, marks, -1);
    return;
  }
  if (target < win.renderStart || target >= win.renderEnd) {
    renderSessionWindowRange(win, Math.max(0, target - 3));
  }
  const node = win.log.querySelector(`[data-history-index="${target}"]`);
  if (!node) return;
  win.followOutput = false;
  win.log.scrollTop = Math.max(0, node.offsetTop - CHAPTER_NAV_JUMP_PAD_PX);
  ix.cursor = { mark: target, scrollTop: win.log.scrollTop };
  chapterNavHighlightRow(node, pending.mode);
  const cluster = chapterNavClusterForPane(win);
  chapterNavUpdateCount(cluster, pending.mode, marks, target);
  chapterNavPulseVisible(cluster);
}

// ── Detail jumps ───────────────────────────────────────────────────────

function chapterNavJumpDetail(view, mode, dir) {
  if (!view || !view.scroller) return false;
  const ix = chapterNavDetailIndex(view);
  const marks = ix[mode] || [];
  const fallback = Math.max(view.renderStart || 0, (view.renderEnd || 1) - 1);
  const anchor = chapterNavAnchorIndex(view.scroller, ix, 'detailRowIndex', fallback);
  const target = chapterNavTargetFrom(marks, anchor, dir);
  if (target < 0) {
    if (dir < 0 && view.hasOlder && !view.loadingOlder
        && typeof loadOlderRemoteSessionDetailRows === 'function') {
      ix.pending = {
        mode,
        firstMarkSignatures: marks.length
          ? sessionDetailRowSignatures(ix.arr[marks[0]], view.sessionId || '')
          : [],
        at: Date.now(),
      };
      chapterNavSetCountLoading(chapterNavDetailCluster);
      loadOlderRemoteSessionDetailRows(view);
      return true;
    }
    chapterNavFlashEdge(chapterNavDetailCluster);
    return false;
  }
  chapterNavScrollDetailTo(view, ix, marks, target, mode);
  return true;
}

function chapterNavScrollDetailTo(view, ix, marks, target, mode) {
  if (target < view.renderStart || target >= view.renderEnd) {
    renderSessionDetailRange(view, Math.max(0, target - 3));
  }
  const node = view.scroller.querySelector(`[data-detail-row-index="${target}"]`);
  if (!node) return;
  view.scroller.scrollTop = Math.max(0, node.offsetTop - CHAPTER_NAV_JUMP_PAD_PX);
  ix.cursor = { mark: target, scrollTop: view.scroller.scrollTop };
  chapterNavHighlightRow(node, mode);
  chapterNavUpdateCount(chapterNavDetailCluster, mode, marks, target);
}

// Seam called by loadOlderRemoteSessionDetailRows after merge+rebuild.
// Row objects are rebuilt wholesale there, so the parked boundary rides
// the existing transcript-signature machinery instead of identity.
function chapterNavDetailHistoryLoaded(view) {
  const pending = view?.chapterNav?.pending;
  if (!pending) return;
  view.chapterNav.pending = null;
  if (Date.now() - pending.at > CHAPTER_NAV_PENDING_TTL_MS) return;
  const ix = chapterNavDetailIndex(view);
  const marks = ix[pending.mode] || [];
  if (!marks.length) return;
  // Same boundary semantics as the pane seam: no prior chapters → step
  // back from the end; boundary rows rebuilt beyond signature recovery →
  // land on the oldest loaded chapter.
  let target;
  if (!pending.firstMarkSignatures?.length) {
    target = chapterNavStepMark(marks, ix.arr.length, -1);
  } else {
    const boundary = findSessionDetailRowIndexBySignatures(
      ix.arr, pending.firstMarkSignatures, view.sessionId || ''
    );
    target = boundary >= 0 ? chapterNavStepMark(marks, boundary, -1) : marks[0];
  }
  if (target < 0) {
    chapterNavFlashEdge(chapterNavDetailCluster);
    chapterNavUpdateCount(chapterNavDetailCluster, pending.mode, marks, -1);
    return;
  }
  chapterNavScrollDetailTo(view, ix, marks, target, pending.mode);
}

// ── Cluster (shared control) ───────────────────────────────────────────

function chapterNavBuildCluster(surface) {
  const el = document.createElement('div');
  el.className = `chapter-nav chapter-nav-${surface}`;
  el.setAttribute('role', 'toolbar');
  el.setAttribute('aria-label', 'Chapter navigation');

  const modeBtn = document.createElement('button');
  modeBtn.type = 'button';
  modeBtn.className = 'chapter-nav-mode';

  const prevBtn = document.createElement('button');
  prevBtn.type = 'button';
  prevBtn.className = 'chapter-nav-step chapter-nav-prev';
  prevBtn.textContent = '▴';

  const count = document.createElement('span');
  count.className = 'chapter-nav-count';
  count.textContent = '–';

  const nextBtn = document.createElement('button');
  nextBtn.type = 'button';
  nextBtn.className = 'chapter-nav-step chapter-nav-next';
  nextBtn.textContent = '▾';

  el.appendChild(modeBtn);
  el.appendChild(prevBtn);
  el.appendChild(count);
  el.appendChild(nextBtn);

  const cluster = { el, modeBtn, prevBtn, nextBtn, count, host: null, surface };
  modeBtn.addEventListener('click', (ev) => {
    ev.stopPropagation();
    chapterNavSetMode(chapterNavMode === 'user' ? 'agent' : 'user');
    chapterNavRefreshCluster(cluster);
  });
  const step = (dir) => (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    chapterNavStepFromCluster(cluster, dir);
  };
  prevBtn.addEventListener('click', step(-1));
  nextBtn.addEventListener('click', step(1));
  // A pane-cluster click must not blur/steal the pane's focus styling.
  el.addEventListener('mousedown', (ev) => ev.stopPropagation());
  chapterNavApplyModeLabels(cluster);
  return cluster;
}

function chapterNavApplyModeLabels(cluster) {
  if (!cluster) return;
  const mode = chapterNavMode;
  cluster.modeBtn.textContent = chapterNavModeLabel(mode);
  cluster.modeBtn.dataset.mode = mode;
  cluster.modeBtn.title = mode === 'agent'
    ? 'Jumping between the agent’s replies — click for your messages'
    : 'Jumping between your messages — click for agent replies';
  cluster.prevBtn.title = chapterNavStepTitle(mode, -1);
  cluster.nextBtn.title = chapterNavStepTitle(mode, 1);
}

function chapterNavSetMode(mode) {
  if (!CHAPTER_NAV_MODES.includes(mode)) return;
  chapterNavMode = mode;
  chapterNavRefreshCluster(chapterNavPaneCluster);
  chapterNavRefreshCluster(chapterNavDetailCluster);
}

// Re-derive labels + total for the cluster's current surface target.
function chapterNavRefreshCluster(cluster) {
  if (!cluster || !cluster.el.isConnected) return;
  chapterNavApplyModeLabels(cluster);
  const marks = chapterNavClusterMarks(cluster);
  chapterNavUpdateCount(cluster, chapterNavMode, marks, -1);
}

function chapterNavClusterMarks(cluster) {
  if (!cluster) return [];
  if (cluster.surface === 'detail') {
    const view = chapterNavResolveDetailView();
    return view ? chapterNavDetailIndex(view)[chapterNavMode] || [] : [];
  }
  const win = chapterNavClusterWindow(cluster);
  return win ? chapterNavPaneIndex(win)[chapterNavMode] || [] : [];
}

function chapterNavClusterWindow(cluster) {
  const sid = cluster?.host?.dataset?.sessionId || '';
  if (!sid || typeof sessionWindows === 'undefined') return null;
  return sessionWindows.get(sid) || null;
}

// The pane cluster, but only while it is attached to THIS pane — a
// readout must never land on another pane's pill.
function chapterNavClusterForPane(win) {
  const cluster = chapterNavPaneCluster;
  return cluster && win && cluster.host === win.el ? cluster : null;
}

function chapterNavStepFromCluster(cluster, dir) {
  if (cluster.surface === 'detail') {
    const view = chapterNavResolveDetailView();
    if (view) chapterNavJumpDetail(view, chapterNavMode, dir);
    return;
  }
  const win = chapterNavClusterWindow(cluster);
  if (win) chapterNavJumpPane(win, chapterNavMode, dir);
}

function chapterNavUpdateCount(cluster, mode, marks, target) {
  if (!cluster || mode !== chapterNavMode) return;
  cluster.count.classList.remove('loading');
  const total = marks.length;
  const pos = target >= 0 ? chapterNavMarkPosition(marks, target) : 0;
  cluster.count.textContent = total === 0 ? '0' : (pos > 0 ? `${pos}/${total}` : String(total));
  const what = mode === 'agent' ? 'agent replies' : 'user messages';
  cluster.count.title = `${total} ${what} in loaded history`;
  cluster.el.classList.toggle('empty', total === 0);
}

function chapterNavSetCountLoading(cluster) {
  if (!cluster) return;
  cluster.count.textContent = '…';
  cluster.count.classList.add('loading');
  cluster.count.title = 'Loading older history…';
  chapterNavPulseVisible(cluster);
}

function chapterNavFlashEdge(cluster) {
  if (!cluster || !cluster.el.isConnected) return;
  cluster.el.classList.remove('at-edge');
  void cluster.el.offsetWidth;
  cluster.el.classList.add('at-edge');
  setTimeout(() => cluster.el.classList.remove('at-edge'), 500);
  chapterNavPulseVisible(cluster);
}

// Keyboard jumps on a non-hovered pane still deserve the readout: the
// pane cluster stays visible for a beat after any keyboard-driven action
// (CSS `.kbd-active`; hover/focus visibility is pure CSS).
function chapterNavPulseVisible(cluster) {
  if (!cluster || cluster.surface !== 'pane') return;
  cluster.el.classList.add('kbd-active');
  clearTimeout(cluster.kbdActiveTimer);
  cluster.kbdActiveTimer = setTimeout(
    () => cluster.el.classList.remove('kbd-active'),
    1600
  );
}

// ── Pane attach (singleton, hover/focus driven) ────────────────────────

function chapterNavAttachToPane(paneEl) {
  if (!paneEl || paneEl.classList.contains('minimized')) return;
  if (!chapterNavPaneCluster) chapterNavPaneCluster = chapterNavBuildCluster('pane');
  const cluster = chapterNavPaneCluster;
  if (cluster.host === paneEl && cluster.el.parentElement === paneEl) return;
  cluster.host = paneEl;
  paneEl.appendChild(cluster.el);
  chapterNavRefreshCluster(cluster);
}

function chapterNavInitPaneCluster() {
  const grid = document.getElementById('session-window-grid');
  if (!grid) return;
  grid.addEventListener('pointerover', (ev) => {
    const pane = ev.target?.closest?.('.session-window');
    if (pane && pane !== chapterNavPaneCluster?.host) chapterNavAttachToPane(pane);
  });
  grid.addEventListener('focusin', (ev) => {
    const pane = ev.target?.closest?.('.session-window');
    if (pane && pane !== chapterNavPaneCluster?.host) chapterNavAttachToPane(pane);
  });
}

// ── Detail toolbar mount (called from renderSessionDetailLogs) ─────────

function chapterNavMountDetailCluster(controls) {
  if (!controls) return;
  if (!chapterNavDetailCluster) chapterNavDetailCluster = chapterNavBuildCluster('detail');
  controls.insertBefore(chapterNavDetailCluster.el, controls.firstChild);
  chapterNavRefreshCluster(chapterNavDetailCluster);
}

// ── Keyboard routing (from 58-shortcuts-boot.js) ───────────────────────

function chapterNavResolveDetailView() {
  const view = typeof sessionDetailLogView !== 'undefined' ? sessionDetailLogView : null;
  if (!view || !view.scroller || !view.scroller.isConnected) return null;
  if (view.scroller.offsetParent === null) return null; // hidden tab/pane
  return view;
}

function chapterNavPaneUsable(win) {
  return !!(win && !win.minimized && win.el?.isConnected && win.el.offsetParent !== null);
}

function chapterNavResolvePaneWindow() {
  if (typeof sessionWindows === 'undefined' || sessionWindows.size === 0) return null;
  if (typeof maximizedSessionWindowId !== 'undefined' && maximizedSessionWindowId) {
    const win = sessionWindows.get(maximizedSessionWindowId);
    if (chapterNavPaneUsable(win)) return win;
  }
  const host = chapterNavPaneCluster?.host;
  if (host && host.isConnected && host.matches(':hover')) {
    const win = sessionWindows.get(host.dataset?.sessionId || '');
    if (chapterNavPaneUsable(win)) return win;
  }
  if (typeof foregroundSessionFullId !== 'undefined' && foregroundSessionFullId) {
    const win = sessionWindows.get(foregroundSessionFullId);
    if (chapterNavPaneUsable(win)) return win;
  }
  if (sessionWindows.size === 1) {
    const win = sessionWindows.values().next().value;
    if (chapterNavPaneUsable(win)) return win;
  }
  return null;
}

// Returns true when the event was consumed. Callers already filtered
// INPUT/TEXTAREA targets; SELECT and contenteditable are re-checked here
// because Alt+arrows drive native controls too.
function chapterNavHandleShortcut(e) {
  if (!e.altKey || e.metaKey || e.ctrlKey) return false;
  if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return false;
  const tag = e.target?.tagName || '';
  if (tag === 'SELECT' || e.target?.isContentEditable) return false;
  const mode = e.shiftKey ? 'agent' : 'user';
  const dir = e.key === 'ArrowUp' ? -1 : 1;
  if (chapterNavMode !== mode) chapterNavSetMode(mode);

  const view = chapterNavResolveDetailView();
  if (view) {
    e.preventDefault();
    chapterNavJumpDetail(view, mode, dir);
    return true;
  }
  const win = chapterNavResolvePaneWindow();
  if (win) {
    e.preventDefault();
    // Keyboard implies intent on this pane: show the cluster there so the
    // position readout has somewhere to land.
    chapterNavAttachToPane(win.el);
    chapterNavJumpPane(win, mode, dir);
    return true;
  }
  return false;
}

chapterNavInitPaneCluster();

// ── QA readback (window.qa convention) ─────────────────────────────────
window.qa = Object.assign(window.qa || {}, {
  chapterNav: {
    mode: () => chapterNavMode,
    setMode: (mode) => chapterNavSetMode(mode),
    paneState: (sid) => {
      const win = typeof sessionWindows !== 'undefined' ? sessionWindows.get(String(sid || '')) : null;
      if (!win) return null;
      const ix = chapterNavPaneIndex(win);
      return {
        user: ix.user.slice(),
        agent: ix.agent.slice(),
        len: ix.len,
        cursor: ix.cursor ? { ...ix.cursor } : null,
        renderStart: win.renderStart,
        renderEnd: win.renderEnd,
        scrollTop: win.log?.scrollTop ?? -1,
      };
    },
    detailState: () => {
      const view = chapterNavResolveDetailView();
      if (!view) return null;
      const ix = chapterNavDetailIndex(view);
      return {
        user: ix.user.slice(),
        agent: ix.agent.slice(),
        len: ix.len,
        cursor: ix.cursor ? { ...ix.cursor } : null,
        renderStart: view.renderStart,
        renderEnd: view.renderEnd,
        scrollTop: view.scroller?.scrollTop ?? -1,
      };
    },
    jumpPane: (sid, mode, dir) => {
      const win = typeof sessionWindows !== 'undefined' ? sessionWindows.get(String(sid || '')) : null;
      return win ? chapterNavJumpPane(win, mode, dir) : false;
    },
    jumpDetail: (mode, dir) => {
      const view = chapterNavResolveDetailView();
      return view ? chapterNavJumpDetail(view, mode, dir) : false;
    },
    clusterHost: () => chapterNavPaneCluster?.host?.dataset?.sessionId || '',
  },
});
