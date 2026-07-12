// ── Debug Screen ──
let debugScreenActive = false;
let debugRecording = false;
const browserWorkspaces = new Map();

// Design-overhaul re-skin: the reference Debug page carries a page header
// the original DOM never had — injected here; the cards, ids, and handlers
// below predate it.
{
  const debugPane = document.querySelector('#tab-debug .debug-pane');
  if (debugPane) {
    const head = document.createElement('header');
    head.className = 'ui2-page-head';
    const title = document.createElement('h2');
    title.className = 'ui2-page-title';
    title.textContent = 'Debug';
    const sub = document.createElement('p');
    sub.className = 'ui2-page-sub';
    sub.textContent = 'Diagnostics, the headless observer display, and managed browser workspaces.';
    head.append(title, sub);
    debugPane.prepend(head);
  }

  // Sessions page header (same reference pattern): the design's in-page
  // title above the sub-tabs. v1 markup never had one — flag-gated only.
  const sessionsContainer = document.querySelector('#tab-sessions .sessions-container');
  if (sessionsContainer) {
    const head = document.createElement('header');
    head.className = 'ui2-page-head';
    const title = document.createElement('h2');
    title.className = 'ui2-page-title';
    title.textContent = 'Sessions';
    const sub = document.createElement('p');
    sub.className = 'ui2-page-sub';
    sub.textContent = 'Every run across all backends — browse, search, resume, or fork.';
    head.append(title, sub);
    sessionsContainer.prepend(head);
  }

  // Toolbar folds (design "power one reveal away"): the reference Recent
  // pane has no filter row, but every current control survives — behind a
  // per-pane "Filters & sort" toggle. DOM is only ever hidden/shown by a
  // header class; the toolbar's controls, ids, and menu logic are shared
  // with v1 untouched. Open state persists per pane; the count bubble
  // reads active refinements straight from the DOM (checked filter boxes,
  // non-empty quick search, non-default sort, subagent toggle).
  for (const fold of [
    { pane: 'sessions-pane-recent', key: 'intendant.ui2.sessionsTools.recent' },
    { pane: 'sessions-pane-deep', key: 'intendant.ui2.sessionsTools.deep' },
    { pane: 'sessions-pane-worktrees', key: 'intendant.ui2.sessionsTools.worktrees' },
  ]) {
    const pane = document.getElementById(fold.pane);
    const header = pane ? pane.querySelector('.sessions-header') : null;
    const titleLine = header ? header.querySelector('.sessions-title-line') : null;
    const toolbar = header ? header.querySelector('.sessions-toolbar') : null;
    if (!header || !titleLine || !toolbar) continue;
    let open = false;
    try { open = localStorage.getItem(fold.key) === '1'; } catch (e) { /* private mode */ }
    header.classList.toggle('ui2-tools-open', open);
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'ui-btn ui2-tools-toggle';
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    const label = document.createElement('span');
    label.textContent = 'Filters & sort';
    const badge = document.createElement('span');
    badge.className = 'ui2-tools-badge';
    const chev = document.createElement('span');
    chev.className = 'ui2-tools-chev';
    chev.innerHTML = ui2Icon('chev', 14);
    btn.append(label, badge, chev);
    const refreshBadge = () => {
      let n = toolbar.querySelectorAll('.sessions-multi-filter-menu input:checked').length;
      for (const input of toolbar.querySelectorAll('input[type="search"]')) {
        if (input.value.trim()) n += 1;
      }
      const subagents = toolbar.querySelector('#sessions-show-subagents');
      if (subagents && subagents.checked) n += 1;
      for (const sel of toolbar.querySelectorAll('.sessions-sort select')) {
        if (sel.selectedIndex > 0) n += 1;
      }
      // Worktree kind toggles count only when off their defaults.
      for (const id of ['filter-worktrees-active', 'filter-worktrees-dirty', 'filter-worktrees-unmerged']) {
        const t = toolbar.querySelector(`#${id}`);
        if (t && !t.checked) n += 1;
      }
      const main = toolbar.querySelector('#filter-worktrees-main');
      if (main && main.checked) n += 1;
      badge.textContent = n > 0 ? String(n) : '';
      badge.classList.toggle('hidden', n === 0);
    };
    btn.addEventListener('click', () => {
      const next = !header.classList.contains('ui2-tools-open');
      header.classList.toggle('ui2-tools-open', next);
      btn.setAttribute('aria-expanded', next ? 'true' : 'false');
      try { localStorage.setItem(fold.key, next ? '1' : '0'); } catch (e) { /* private mode */ }
      refreshBadge(); // restored-from-localStorage filters set no events
    });
    toolbar.addEventListener('change', refreshBadge);
    toolbar.addEventListener('input', refreshBadge);
    refreshBadge();
    const refreshBtn = titleLine.querySelector('.sessions-refresh');
    if (refreshBtn) titleLine.insertBefore(btn, refreshBtn);
    else titleLine.appendChild(btn);
  }
}

function toggleDebugScreen() {
  if (debugScreenActive) {
    dispatchDashboardActionMsg({ action: 'teardown_debug_screen' });
  } else {
    dispatchDashboardActionMsg({ action: 'setup_debug_screen' });
  }
}

function toggleDebugRecording() {
  if (debugRecording) {
    dispatchDashboardActionMsg({ action: 'stop_debug_recording' });
  } else {
    dispatchDashboardActionMsg({ action: 'start_debug_recording' });
  }
}
window.toggleDebugScreen = toggleDebugScreen;
window.toggleDebugRecording = toggleDebugRecording;

function renderBrowserWorkspaces() {
  const list = document.getElementById('browser-workspace-list');
  const status = document.getElementById('browser-workspace-status');
  if (!list) return;
  const rows = Array.from(browserWorkspaces.values())
    .filter(w => (w.status || '') !== 'closed')
    .sort((a, b) => String(b.updated_at || '').localeCompare(String(a.updated_at || '')));
  window.__stationBrowserWorkspaceCount = rows.length;
  // Zero-count wording lives in the list's empty state; repeating it here
  // stacked two "no workspaces" lines.
  if (status && status.dataset.error !== '1') status.textContent = rows.length ? `${rows.length} active workspace${rows.length === 1 ? '' : 's'}` : '';
  if (!rows.length) {
    list.innerHTML = '<div class="ui-empty compact"><div class="ui-empty-title">No browser workspaces</div>' +
      '<div class="ui-empty-hint">Create one above, or the agent will spawn its own when it needs a browser.</div></div>';
    return;
  }
  list.innerHTML = '';
  for (const w of rows) {
    const card = document.createElement('div');
    card.className = 'debug-workspace-row';
    // The status dot is CSS-keyed off this attribute.
    card.dataset.status = String(w.status || '');
    const meta = document.createElement('div');
    meta.className = 'debug-workspace-row-main';
    const lease = w.lease ? `leased by ${w.lease.holder_id || 'unknown'}` : 'unleased';
    const url = w.url || 'about:blank';
    const executableSource = w.browser_executable_source ? ` · ${w.browser_executable_source}` : '';
    meta.innerHTML =
      `<div class="debug-workspace-row-title">${escapeHtml(w.label || w.id)}</div>` +
      `<div class="debug-workspace-row-url">${escapeHtml(url)}</div>` +
      `<div class="debug-workspace-row-meta">${escapeHtml(w.id || '')} · ${escapeHtml(w.provider || '')} · ${escapeHtml(w.status || '')}${escapeHtml(executableSource)} · ${escapeHtml(lease)}</div>`;
    const actions = document.createElement('div');
    actions.className = 'debug-workspace-row-actions';
    const take = document.createElement('button');
    take.type = 'button';
    take.className = 'ui-btn';
    take.textContent = w.lease ? 'Take' : 'Acquire';
    take.title = 'Acquire this browser workspace lease';
    take.onclick = () => acquireBrowserWorkspace(w.id, true);
    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'ui-btn danger';
    close.textContent = 'Close';
    close.title = 'Close this browser workspace';
    close.onclick = () => closeBrowserWorkspace(w.id);
    actions.appendChild(take);
    actions.appendChild(close);
    card.appendChild(meta);
    card.appendChild(actions);
    list.appendChild(card);
  }
}

function handleBrowserWorkspaceMessage(d) {
  if (d.t === 'browser_workspace_snapshot' && Array.isArray(d.workspaces)) {
    browserWorkspaces.clear();
    for (const w of d.workspaces) if (w && w.id) browserWorkspaces.set(String(w.id), w);
    const snapStatus = document.getElementById('browser-workspace-status');
    if (snapStatus) delete snapStatus.dataset.error; // fresh truth supersedes a pinned error
    renderBrowserWorkspaces();
    return;
  }
  if (d.event !== 'browser_workspace_changed') return;
  if (d.workspace && d.workspace.id) {
    browserWorkspaces.set(String(d.workspace.id), d.workspace);
  } else if (d.workspace_id && d.kind === 'closed') {
    browserWorkspaces.delete(String(d.workspace_id));
  }
  const status = document.getElementById('browser-workspace-status');
  if (status && d.kind === 'error') {
    // Paint the error AFTER the re-render and pin it — renderBrowserWorkspaces
    // rewrites the status line and was clobbering the message same-tick.
    renderBrowserWorkspaces();
    status.dataset.error = '1';
    status.textContent = d.message || 'Browser workspace error';
    return;
  }
  if (status) delete status.dataset.error;
  renderBrowserWorkspaces();
}

// The dashboard-fronted session id for owner/holder stamping. The old
// `currentSessionId` global died in the AppWeb→PresenceWeb merge
// (9285c7b6); the bare reads below kept the dead name and threw
// ReferenceError, silently breaking Create and Take/Acquire since March.
// Same fallback chain the session-window code uses for self-identity.
function debugPaneSessionId() {
  return String(currentSessionFullId || daemonSessionFullId || '').trim();
}

function createBrowserWorkspaceFromDebug() {
  if (!app) return;
  const g = id => document.getElementById(id);
  dispatchDashboardActionMsg({
    action: 'create_browser_workspace',
    url: (g('browser-workspace-url')?.value || '').trim() || undefined,
    provider: g('browser-workspace-provider')?.value || 'auto',
    label: (g('browser-workspace-label')?.value || '').trim() || undefined,
    owner_session_id: debugPaneSessionId() || undefined,
  });
}

function acquireBrowserWorkspace(workspaceId, force = false) {
  if (!app || !workspaceId) return;
  dispatchDashboardActionMsg({
    action: 'acquire_browser_workspace',
    workspace_id: workspaceId,
    holder_id: debugPaneSessionId() || ((typeof connectionId !== 'undefined' && connectionId) ? connectionId : 'dashboard'),
    holder_kind: 'human',
    force: !!force,
  });
}

function closeBrowserWorkspace(workspaceId) {
  if (!app || !workspaceId) return;
  dispatchDashboardActionMsg({
    action: 'close_browser_workspace',
    workspace_id: workspaceId,
    reason: 'closed from dashboard',
  });
}

window.createBrowserWorkspaceFromDebug = createBrowserWorkspaceFromDebug;

function onDebugScreenReady(displayId) {
  debugScreenActive = true;
  document.getElementById('debug-status').textContent = 'Active';
  document.getElementById('debug-status').classList.add('ok');
  document.getElementById('debug-setup-btn').textContent = 'Tear down';
  document.getElementById('debug-record-btn').disabled = false;
  document.getElementById('debug-info').classList.remove('hidden');
  document.getElementById('debug-display-id').textContent = displayId;
}

// The daemon emits recording_started/stopped with stream `display_<id>[_n]`
// (recording.rs pick_next_display_stream_name); flip the debug Record button
// when the stream belongs to the debug display. Called from the central WS
// dispatch (36-voice-wasm-init) — declarations hoist module-wide, so the
// later-fragment definition is live by event time.
function debugRecordingStreamIsOurs(streamName) {
  const idEl = document.getElementById('debug-display-id');
  const id = ((idEl && idEl.textContent) || '').trim();
  if (!id) return false;
  const base = `display_${id}`;
  return streamName === base || streamName.startsWith(`${base}_`);
}

function handleDebugRecordingEvent(d) {
  if (!debugScreenActive || !d.stream_name) return;
  if (!debugRecordingStreamIsOurs(String(d.stream_name))) return;
  const btn = document.getElementById('debug-record-btn');
  if (d.event === 'recording_started') {
    debugRecording = true;
    if (btn) btn.textContent = 'Stop recording';
  } else if (d.event === 'recording_stopped') {
    debugRecording = false;
    if (btn) btn.textContent = 'Start recording';
  }
}

function onDebugScreenTornDown() {
  debugScreenActive = false;
  debugRecording = false;
  document.getElementById('debug-status').textContent = 'Not active';
  document.getElementById('debug-status').classList.remove('ok');
  document.getElementById('debug-setup-btn').textContent = 'Set up debug screen';
  document.getElementById('debug-record-btn').disabled = true;
  document.getElementById('debug-record-btn').textContent = 'Start recording';
  document.getElementById('debug-info').classList.add('hidden');
}

// ── Sessions Tab ──
// (State lives in the early client-state block near `let activeTab` —
// the #sessions deep link reaches this code during script evaluation.)

function sessionListLimitLabel(limit) {
  if (limit === 'all') return 'full history';
  const n = Number(limit);
  return Number.isFinite(n) && n > 0
    ? `${Math.floor(n).toLocaleString()}-session recent window`
    : `${SESSION_LIST_RECENT_LIMIT.toLocaleString()}-session recent window`;
}

function updateSessionsHydrationNotice() {
  const el = document.getElementById('sessions-hydration-status');
  if (!el) return;
  const state = _sessionsHydrationState;
  const visible = !!(state.active || state.done || state.error);
  el.classList.toggle('hidden', !visible);
  el.classList.toggle('done', !!state.done && !state.error);
  el.classList.toggle('error', !!state.error);
  if (!visible) {
    el.textContent = '';
    return;
  }

  el.textContent = '';
  const spinner = document.createElement('span');
  spinner.className = 'sessions-hydration-spinner';
  el.appendChild(spinner);

  const copy = document.createElement('div');
  copy.className = 'sessions-hydration-copy';
  const title = document.createElement('div');
  title.className = 'sessions-hydration-title';
  const detail = document.createElement('div');
  detail.className = 'sessions-hydration-detail';
  const received = Number(state.received || 0);
  const receivedText = received > 0
    ? `${received.toLocaleString()} session${received === 1 ? '' : 's'} visible`
    : 'Preparing visible rows';
  if (state.error) {
    title.textContent = 'Session details did not finish loading';
    detail.textContent = state.error;
  } else if (state.done) {
    title.textContent = 'Session details loaded';
    detail.textContent = `${receivedText}; tokens, costs, lineage, and sizes are current.`;
  } else if (state.phase === 'receiving') {
    title.textContent = 'Loading recent sessions';
    detail.textContent = `${receivedText}; details are still hydrating from the ${sessionListLimitLabel(state.limit)}.`;
  } else {
    title.textContent = 'Hydrating session details';
    detail.textContent = `${receivedText}; tokens, costs, lineage, and disk sizes are still loading.`;
  }
  copy.appendChild(title);
  copy.appendChild(detail);
  el.appendChild(copy);

  const meter = document.createElement('div');
  meter.className = 'sessions-hydration-meter';
  const fill = document.createElement('span');
  meter.appendChild(fill);
  el.appendChild(meter);
}

function setSessionsHydrationState(patch = {}) {
  if (_sessionsHydrationHideTimer) {
    clearTimeout(_sessionsHydrationHideTimer);
    _sessionsHydrationHideTimer = null;
  }
  _sessionsHydrationState = {
    ..._sessionsHydrationState,
    ...patch,
  };
  updateSessionsHydrationNotice();
}

function finishSessionsHydration(received, limit) {
  setSessionsHydrationState({
    active: false,
    done: true,
    error: '',
    phase: 'done',
    received,
    limit,
  });
  _sessionsHydrationHideTimer = setTimeout(() => {
    if (!_sessionsHydrationState.active && _sessionsHydrationState.done) {
      _sessionsHydrationState = {
        ..._sessionsHydrationState,
        done: false,
        phase: 'idle',
      };
      updateSessionsHydrationNotice();
    }
  }, SESSION_HYDRATION_DONE_HIDE_MS);
}

function failSessionsHydration(message, received, limit) {
  setSessionsHydrationState({
    active: false,
    done: false,
    error: message || 'Unknown session loading error',
    phase: 'error',
    received,
    limit,
  });
}

// '' and the (possibly not-yet-assigned) selfPeerId both mean "this
// daemon": selfPeerId arrives async, so a load started before identity
// resolved must still count as self/active once it lands.
function normalizeSessionsHostId(hostId) {
  return !hostId || hostId === selfPeerId ? '' : String(hostId);
}

function applyLoadedSessions(sessions, aggEl, hostId = currentSessionsHostId()) {
  const host = normalizeSessionsHostId(hostId);
  const isSelf = host === '';
  const isActiveView = host === normalizeSessionsHostId(currentSessionsHostId());
  if (isActiveView) {
    _cachedSessions = sessions;
    updateSessionProjectFilterOptions(sessions);
  }
  if (isSelf) {
    // Self-coupled side effects stay self-only: Station's session set,
    // window metadata, and New Session project prefills must not absorb a
    // browsed peer's rows — and a background SELF load (Station/managed
    // pane warming the index) must not stomp a peer view the user is
    // looking at, hence the isActiveView split.
    sessionsLoaded = true;
    stationInvalidateSessionSet();
    updateNewSessionProjectPrefills(sessions);
    cacheSessionWindowMetadata(sessions);
  }
  if (!isActiveView) return;
  if (activeActivitySubtab === 'managed') {
    renderManagedContextSessionSelect();
    renderManagedContextPane();
  }
  scheduleSessionsListRender();
}

// ── Sessions host strip: browse a connected peer's sessions in place ──
// Hidden until a peer is connected; power stays discoverable without
// cluttering the single-daemon experience.

function currentSessionsHostId() {
  return sessionsActiveHostId || selfPeerId;
}

function sessionsViewingPeer() {
  return currentSessionsHostId() !== selfPeerId;
}

function sessionsHostRowFor(hostId) {
  return (Array.isArray(daemons) ? daemons : []).find(d => d && d.host_id === hostId) || null;
}

function setSessionsHost(hostId) {
  const next = !hostId || hostId === selfPeerId ? '' : String(hostId);
  if (next === sessionsActiveHostId) return;
  sessionsActiveHostId = next;
  sessionsRenderWindow = SESSION_CARD_RENDER_PAGE;
  renderSessionsHostStrip();
  loadSessions({ force: true });
}

// Whole-card action while browsing a peer: hand off to the peer's own
// dashboard (its own auth applies there) rather than faking cross-daemon
// session control this daemon has no authority for.
function openPeerSessionExternally(s) {
  const host = sessionsHostRowFor(currentSessionsHostId());
  const base = String(host?.browser_tcp_via_url || host?.url || '').replace(/\/+$/, '');
  if (!base) {
    showControlToast('error', 'No reachable dashboard URL is known for this peer.');
    return;
  }
  window.open(`${base}/#sessions`, '_blank', 'noopener');
  const sid = String(s?.session_id || s?.resume_id || '').trim();
  if (sid) {
    showControlToast('info', `Opened ${host.label || 'peer'} dashboard — look for session ${sid.slice(0, 8)}…`);
  }
}

function renderSessionsHostStrip() {
  const strip = document.getElementById('sessions-host-strip');
  if (!strip) return;
  const peers = (Array.isArray(daemons) ? daemons : [])
    .filter(d => d && d.host_id && d.host_id !== selfPeerId && d.connected);
  // Keep a selected-but-now-disconnected peer visible so the active
  // selection never silently strands.
  if (sessionsActiveHostId && !peers.some(p => p.host_id === sessionsActiveHostId)) {
    const known = sessionsHostRowFor(sessionsActiveHostId);
    if (known) peers.push(known);
  }
  strip.classList.toggle('hidden', peers.length === 0);
  strip.innerHTML = '';
  if (peers.length === 0) {
    if (sessionsActiveHostId) setSessionsHost('');
    return;
  }
  const active = currentSessionsHostId();
  const addChip = (id, label, connected) => {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'sessions-host-chip'
      + (active === id ? ' active' : '')
      + (connected ? '' : ' offline');
    btn.setAttribute('role', 'tab');
    btn.setAttribute('aria-selected', active === id ? 'true' : 'false');
    btn.textContent = label;
    btn.title = connected
      ? (id === selfPeerId ? 'Sessions on this daemon' : `Browse sessions on ${label}`)
      : 'Peer is not connected';
    btn.onclick = () => setSessionsHost(id);
    strip.appendChild(btn);
  };
  addChip(selfPeerId, 'This daemon', true);
  for (const p of peers) addChip(p.host_id, p.label || p.host_id, !!p.connected);
}

// Rendering the sessions list is decoupled from data arrival: streamed
// batches and background refreshes merge into _cachedSessions eagerly, but
// the (expensive, full-list) DOM rebuild runs at most every 250ms, and only
// while the Sessions pane is actually visible — otherwise one render is
// deferred to the next pane entry. (Timer state sits in the early
// client-state block — deep-link TDZ.)
function renderSessionsListNow() {
  _sessionsRenderLastTs = Date.now();
  renderSessionsAggregate(_cachedSessions || [], document.getElementById('sessions-aggregate'));
  renderSessionsViews();
}
function scheduleSessionsListRender() {
  if (!paneIsVisible('sessions')) {
    renderOrDefer('sessions', 'list', renderSessionsListNow);
    return;
  }
  if (_sessionsRenderTimer) return;
  const wait = Math.max(0, SESSIONS_RENDER_MIN_INTERVAL_MS - (Date.now() - _sessionsRenderLastTs));
  _sessionsRenderTimer = setTimeout(() => {
    _sessionsRenderTimer = null;
    renderSessionsListNow();
  }, wait);
}

function loadSessions(options = {}) {
  const listEl = document.getElementById('sessions-list');
  const aggEl = document.getElementById('sessions-aggregate');
  // Complete retrieval by default: the stream's quick phase paints the
  // first ~600 immediately, then the full corpus replaces it (rendering
  // stays windowed via Show-more). A bounded Recent list silently hid
  // ~90% of history and made the aggregate tiles disagree with Stats.
  const requestedLimit = options.limit ?? 'all';
  const hostId = options.host || currentSessionsHostId();
  const cacheOptions = { limit: requestedLimit };
  const cached = sessionsListCache.get(sessionListCacheKey(hostId, cacheOptions));
  const cacheKey = sessionListCacheKey(hostId, cacheOptions);
  if (cached) {
    applyLoadedSessions(cached, aggEl, hostId);
  } else if (listEl) {
    // Fresh (uncached) load replaces the list — reset the Show-more window
    // and drop the render-pass state (the skeleton wipe orphans its DOM).
    sessionsRenderWindow = SESSION_CARD_RENDER_PAGE;
    resetSessionsListRenderState(listEl.id);
    listEl.innerHTML = '<div class="ui-skel sessions-skel-card"></div>'.repeat(6);
  }

  const token = ++_sessionsLoadToken;
  let streamedRowCount = 0;
  setSessionsHydrationState({
    active: true,
    done: false,
    error: '',
    phase: cached ? 'refreshing' : 'starting',
    received: Array.isArray(cached) ? cached.length : 0,
    limit: requestedLimit,
  });
  if (_sessionsStreamAbort) _sessionsStreamAbort.abort();
  const controller = new AbortController();
  _sessionsStreamAbort = controller;
  let pendingRows = [];
  let flushTimer = null;
  const scheduleFlush = () => {
    if (flushTimer) return;
    flushTimer = setTimeout(() => {
      flushTimer = null;
      if (token !== _sessionsLoadToken || pendingRows.length === 0) return;
      const next = mergeSessionRows(_cachedSessions, pendingRows);
      pendingRows = [];
      applyLoadedSessions(next, aggEl, hostId);
    }, 50);
  };
  const clearPendingFlush = () => {
    if (flushTimer) {
      clearTimeout(flushTimer);
      flushTimer = null;
    }
    pendingRows = [];
  };

  const loadJsonFallback = () => fetchSessionsForHost(hostId, {
    force: options.force !== false,
    limit: requestedLimit,
  })
    .then(sessions => {
      if (token !== _sessionsLoadToken) return;
      if (requestedLimit !== 'all') {
        sessionsRecentLimit = Math.max(sessionsRecentLimit, sessionListRequestLimit({ limit: requestedLimit }));
      }
      finishSessionsHydration(Array.isArray(sessions) ? sessions.length : 0, requestedLimit);
      applyLoadedSessions(sessions, aggEl, hostId);
      return sessions;
    });

  return streamSessionsForHost(hostId, {
    limit: requestedLimit,
    signal: controller.signal,
  }, {
    sessions(rows) {
      if (token !== _sessionsLoadToken) return;
      streamedRowCount += rows.length;
      setSessionsHydrationState({
        active: true,
        done: false,
        error: '',
        phase: 'receiving',
        received: streamedRowCount,
        limit: requestedLimit,
      });
      pendingRows.push(...rows);
      if (pendingRows.length >= 50) {
        if (flushTimer) {
          clearTimeout(flushTimer);
          flushTimer = null;
        }
        const rowsToFlush = pendingRows;
        pendingRows = [];
        applyLoadedSessions(mergeSessionRows(_cachedSessions, rowsToFlush), aggEl, hostId);
      } else {
        scheduleFlush();
      }
    },
    phase(phase) {
      if (token !== _sessionsLoadToken) return;
      setSessionsHydrationState({
        active: true,
        done: false,
        error: '',
        phase: phase || 'hydrating',
        received: Math.max(streamedRowCount, Array.isArray(_cachedSessions) ? _cachedSessions.length : 0),
        limit: requestedLimit,
      });
    },
    replace(sessions) {
      if (token !== _sessionsLoadToken) return;
      clearPendingFlush();
      sessionsListCache.set(cacheKey, sessions);
      if (requestedLimit !== 'all') {
        sessionsRecentLimit = Math.max(sessionsRecentLimit, sessionListRequestLimit({ limit: requestedLimit }));
      }
      finishSessionsHydration(Array.isArray(sessions) ? sessions.length : 0, requestedLimit);
      applyLoadedSessions(sessions, aggEl, hostId);
    },
  })
    .then(sessions => {
      if (_sessionsStreamAbort === controller) _sessionsStreamAbort = null;
      if (token === _sessionsLoadToken && Array.isArray(sessions) && _sessionsHydrationState.active) {
        finishSessionsHydration(sessions.length, requestedLimit);
      }
      return sessions || _cachedSessions;
    })
    .catch(err => {
      if (_sessionsStreamAbort === controller) _sessionsStreamAbort = null;
      // Facade aborts are DaemonApiError kind:'abort' (name differs from
      // the DOM AbortError the pre-facade stream threw).
      if (err?.name === 'AbortError' || err?.kind === 'abort') {
        if (token === _sessionsLoadToken) {
          setSessionsHydrationState({ active: false, done: false, error: '', phase: 'idle' });
        }
        return;
      }
      clearPendingFlush();
      return loadJsonFallback().catch(() => {
        failSessionsHydration(
          'The compatibility session list request failed.',
          Array.isArray(_cachedSessions) ? _cachedSessions.length : 0,
          requestedLimit,
        );
        if (!cached && listEl) {
          resetSessionsListRenderState(listEl.id);
          listEl.innerHTML = '<div class="empty-state">Failed to load sessions</div>';
        }
      });
    });
}

// One-shot classification stash: eventRefreshesSessionMetadata() and the
// scheduleSessionsMetadataRefresh() that follows it are called back to back
// (synchronously) by the fragment-36 server-message dispatcher, which only
// passes the event NAME — the stash carries "this trigger is session-scoped"
// across that pair. Direct schedule callers (session launch/config paths)
// find it false and get the historical full refresh.
let _sessionsRefreshEventTargeted = false;
// True while the pending coalesced tick is targeted-only; any full trigger
// inside the window upgrades the tick (full beats targeted).
let _sessionsMetadataRefreshTargeted = false;

function scheduleSessionsMetadataRefresh(delay = 700) {
  const targeted = _sessionsRefreshEventTargeted;
  _sessionsRefreshEventTargeted = false;
  if (!sessionsLoaded && !shouldPollSessionWindowMetadata()) return;
  if (sessionsMetadataRefreshTimer) {
    clearTimeout(sessionsMetadataRefreshTimer);
    _sessionsMetadataRefreshTargeted = _sessionsMetadataRefreshTargeted && targeted;
  } else {
    _sessionsMetadataRefreshTargeted = targeted;
  }
  sessionsMetadataRefreshTimer = setTimeout(() => {
    sessionsMetadataRefreshTimer = null;
    const wasTargeted = _sessionsMetadataRefreshTargeted;
    _sessionsMetadataRefreshTargeted = false;
    if (!sessionsLoaded) {
      // 39's window-metadata poll is already ids-scoped when windows exist.
      refreshSessionWindowMetadata(0, { force: true });
    } else if (wasTargeted) {
      refreshLiveSessionRows();
    } else {
      loadSessions({ force: true });
    }
  }, delay);
}

// Targeted lane for session-scoped progress events: refresh only the rows
// of live session windows via the api_sessions ids= filter (the same lane
// 39's hydrate and metadata poll use) instead of refetching the whole
// corpus with limit 'all' on every turn. The id population is 39's
// canonical one (windows + backend/wrapper twins + resolved relatives) so
// the targeted refresh sees exactly what the metadata poll sees.
function refreshLiveSessionRows() {
  const ids = typeof sessionWindowMetadataRequestIds === 'function'
    ? sessionWindowMetadataRequestIds()
    : [];
  // Nothing live to target: fall back to the window-metadata poll (cheap,
  // ids-scoped or bounded) rather than a silent no-op — a session-scoped
  // event without a window still implies row drift somewhere.
  if (!ids.length) {
    refreshSessionWindowMetadata(0, { force: true });
    return;
  }
  daemonApi.request('api_sessions', { ids })
    .then(resp => {
      if (!resp.ok || !Array.isArray(resp.body) || resp.body.length === 0) return;
      if (sessionsViewingPeer()) {
        // Browsing a peer's list: keep self rows out of the visible corpus
        // but still feed the window-metadata cache.
        cacheSessionWindowMetadata(resp.body);
        return;
      }
      applyLoadedSessions(
        mergeSessionRows(_cachedSessions, resp.body),
        document.getElementById('sessions-aggregate'),
        selfPeerId,
      );
    })
    .catch(() => { /* the next event or full refresh recovers */ });
}

function eventRefreshesSessionMetadata(eventName) {
  switch (eventName) {
    // Session-scoped progress: a targeted (ids=) row refresh is enough.
    // model_response is deliberately absent — it fires per model response
    // and used to trigger a full-corpus refetch each time.
    case 'turn_started':
    case 'done_signal':
    case 'task_complete':
    case 'round_complete':
    case 'interrupted':
      _sessionsRefreshEventTargeted = true;
      return true;
    // List-shape changes: the full refresh.
    case 'session_started':
    case 'session_ended':
      _sessionsRefreshEventTargeted = false;
      return true;
    default:
      return false;
  }
}

function sessionsShowSubagents() {
  return document.getElementById('sessions-show-subagents')?.checked === true;
}

function normalizeSessionThreadSourceValue(value) {
  return String(value || '').trim().toLowerCase().replace(/_/g, '-');
}

function sessionRelationshipKindForRow(session, meta = null) {
  const rowMeta = meta || sessionConfigMetadata(session);
  const relationshipRaw =
    rowMeta.relationshipKind ||
    rowMeta.relationship_kind ||
    session?.relationship_kind ||
    session?.relationshipKind ||
    session?.relationship ||
    '';
  if (relationshipRaw) return normalizeSessionRelationshipKind(relationshipRaw);
  const threadSource = normalizeSessionThreadSourceValue(
    rowMeta.threadSource ||
    rowMeta.thread_source ||
    session?.thread_source ||
    session?.threadSource ||
    ''
  );
  if (threadSource === 'subagent' || threadSource === 'sub-agent') return 'subagent';
  if (threadSource === 'side' || threadSource === 'fork') return threadSource;
  return '';
}

function sessionLineageParentId(session, meta = null) {
  const rowMeta = meta || sessionConfigMetadata(session);
  return compactSessionText(
    rowMeta.parentId ||
    rowMeta.parent_session_id ||
    rowMeta.parentSessionId ||
    session?.parent_session_id ||
    session?.parentSessionId ||
    session?.parent_id ||
    session?.parentId
  );
}

function sessionLineageIds(session, meta = null) {
  const rowMeta = meta || sessionConfigMetadata(session);
  return Array.from(new Set([
    session?.session_id,
    session?.resume_id,
    session?.backend_session_id,
    session?.backendSessionId,
    session?.intendant_session_id,
    session?.intendantSessionId,
    rowMeta.session_id,
    rowMeta.sessionId,
    rowMeta.resume_id,
    rowMeta.resumeId,
    rowMeta.backendSessionId,
    rowMeta.backend_session_id,
    rowMeta.intendantSessionId,
    rowMeta.intendant_session_id,
  ].map(id => compactSessionText(id)).filter(Boolean)));
}

function sessionLineageSource(session, fallback = null) {
  return normalizeAgentId(
    session?.source ||
    session?.backend_source ||
    session?.backendSource ||
    fallback?.source ||
    fallback?.backend_source ||
    fallback?.backendSource ||
    'intendant'
  ) || 'intendant';
}

function sessionLineageDisplayLabel(session, fallbackId = '') {
  if (!session) return shortSessionId(fallbackId);
  const meta = sessionConfigMetadata(session);
  return compactSessionText(
    session.name ||
    session.display_name ||
    session.thread_name ||
    meta.name ||
    session.task ||
    meta.task
  ) || shortSessionId(fallbackId || session.session_id || session.resume_id || meta.backendSessionId);
}

function sessionLineageRelationshipLabel(kind) {
  if (kind === 'subagent') return 'subagent';
  if (kind === 'side') return 'side';
  if (kind === 'fork') return 'fork';
  return kind || 'related';
}

// Single-slot memo: the index costs two sessionConfigMetadata sweeps over
// the whole corpus, and every render pass (each search keystroke included)
// wants it. The rowset array is replaced whenever data changes
// (mergeSessionRows), so array identity is the invalidation key.
let _sessionLineageIndexFor = null;
let _sessionLineageIndexMemo = null;

function buildSessionLineageIndex(sessions) {
  if (sessions && sessions === _sessionLineageIndexFor && _sessionLineageIndexMemo) {
    return _sessionLineageIndexMemo;
  }
  const byId = new Map();
  const childrenByParentId = new Map();
  const rows = Array.isArray(sessions) ? sessions : [];
  for (const session of rows) {
    if (!session || typeof session !== 'object') continue;
    const meta = sessionConfigMetadata(session);
    for (const id of sessionLineageIds(session, meta)) {
      if (!byId.has(id)) byId.set(id, session);
    }
  }
  for (const session of rows) {
    if (!session || typeof session !== 'object') continue;
    const meta = sessionConfigMetadata(session);
    const parentId = sessionLineageParentId(session, meta);
    if (!parentId) continue;
    if (!childrenByParentId.has(parentId)) childrenByParentId.set(parentId, []);
    childrenByParentId.get(parentId).push(session);
  }
  // Deterministic child ordering regardless of the caller's sort: newest
  // first (the index used to inherit the list's sort order).
  for (const children of childrenByParentId.values()) {
    children.sort((a, b) => sessionDateSortValue(b, 'updated_at') - sessionDateSortValue(a, 'updated_at'));
  }
  const index = { byId, childrenByParentId };
  _sessionLineageIndexFor = sessions;
  _sessionLineageIndexMemo = index;
  return index;
}

function sessionLineageParentForSession(index, session, meta = null) {
  const parentId = sessionLineageParentId(session, meta);
  if (!parentId) return { parentId: '', parent: null };
  return {
    parentId,
    parent: index?.byId?.get(parentId) || findCachedSessionByAnyId(parentId) || null,
  };
}

function sessionLineageChildrenForSession(index, session, meta = null) {
  if (!index || !session) return [];
  const selfIds = new Set(sessionLineageIds(session, meta));
  const seen = new Set();
  const children = [];
  for (const id of selfIds) {
    for (const child of index.childrenByParentId.get(id) || []) {
      const childKey = sessionLineageIds(child)[0] || child.session_id || child.resume_id || '';
      if (!childKey || selfIds.has(childKey) || seen.has(childKey)) continue;
      seen.add(childKey);
      children.push(child);
    }
  }
  return children;
}

function sessionLineageOpenSession(sourceSession, targetSession, targetId, ev = null) {
  if (ev) {
    ev.preventDefault();
    ev.stopPropagation();
  }
  const id = compactSessionText(targetId || targetSession?.session_id || targetSession?.resume_id);
  if (!id) return;
  // Re-resolve at click time: lineage chips can outlive the row snapshot
  // they were built from (cards are cached across stream refreshes).
  const target = findCachedSessionByAnyId(id) || targetSession || {
    session_id: id,
    source: sessionLineageSource(sourceSession),
    task: shortSessionId(id),
  };
  openSessionDetail(target);
}

function createSessionLineageListChip(sourceSession, targetSession, targetId, text, title) {
  const chip = document.createElement(targetId ? 'button' : 'span');
  chip.className = 'ui-chip muted sc-role sc-lineage-chip';
  chip.textContent = text;
  chip.title = title || text;
  if (targetId) {
    chip.type = 'button';
    chip.addEventListener('click', (ev) => sessionLineageOpenSession(sourceSession, targetSession, targetId, ev));
  }
  return chip;
}

function createSessionDetailLineageChip(sourceSession, targetSession, targetId, text, title, extraClass = '') {
  const chip = document.createElement(targetId ? 'button' : 'span');
  chip.className = `sd-lineage-chip${extraClass ? ` ${extraClass}` : ''}`;
  chip.textContent = text;
  chip.title = title || text;
  if (targetId) {
    chip.type = 'button';
    chip.addEventListener('click', (ev) => sessionLineageOpenSession(sourceSession, targetSession, targetId, ev));
  }
  return chip;
}

function renderSessionDetailLineage(titleEl, session) {
  if (!titleEl || !session) return;
  const index = buildSessionLineageIndex(_cachedSessions);
  const meta = sessionConfigMetadata(session);
  const relationshipKind = sessionRelationshipKindForRow(session, meta);
  const { parentId, parent } = sessionLineageParentForSession(index, session, meta);
  const children = sessionLineageChildrenForSession(index, session, meta);
  if (!relationshipKind && !parentId && children.length === 0) return;

  const sessionId = compactSessionText(session.session_id || session.resume_id || meta.backendSessionId);
  const line = document.createElement('div');
  line.className = 'sd-lineage';

  if (relationshipKind) {
    const kindEl = document.createElement('span');
    kindEl.className = 'sd-lineage-kind';
    kindEl.textContent = sessionLineageRelationshipLabel(relationshipKind);
    line.appendChild(kindEl);
  }

  const appendSeparator = () => {
    const sep = document.createElement('span');
    sep.className = 'sd-lineage-separator';
    sep.textContent = '->';
    line.appendChild(sep);
  };

  if (parentId) {
    const parentLabel = sessionLineageDisplayLabel(parent, parentId);
    line.appendChild(createSessionDetailLineageChip(
      session,
      parent,
      parentId,
      parentLabel,
      `Parent session\n${parentLabel}\n${parentId}`
    ));
    appendSeparator();
  }

  line.appendChild(createSessionDetailLineageChip(
    session,
    null,
    '',
    'current',
    `${sessionLineageDisplayLabel(session, sessionId)}${sessionId ? `\n${sessionId}` : ''}`,
    'sd-lineage-current'
  ));

  if (children.length > 0) {
    appendSeparator();
    const visibleChildren = children.slice(0, 4);
    for (const child of visibleChildren) {
      const childId = sessionLineageIds(child)[0] || child.session_id || child.resume_id || '';
      const childKind = sessionLineageRelationshipLabel(sessionRelationshipKindForRow(child));
      const childLabel = sessionLineageDisplayLabel(child, childId);
      line.appendChild(createSessionDetailLineageChip(
        session,
        child,
        childId,
        `${childKind} ${childLabel}`,
        `${childKind} child session\n${childLabel}${childId ? `\n${childId}` : ''}`
      ));
    }
    if (children.length > visibleChildren.length) {
      const more = document.createElement('span');
      more.className = 'sd-lineage-chip';
      more.textContent = `+${children.length - visibleChildren.length} more`;
      more.title = children
        .slice(visibleChildren.length)
        .map(child => {
          const childId = sessionLineageIds(child)[0] || child.session_id || '';
          const childKind = sessionLineageRelationshipLabel(sessionRelationshipKindForRow(child));
          return `${childKind} ${sessionLineageDisplayLabel(child, childId)}${childId ? ` (${childId})` : ''}`;
        })
        .join('\n');
      line.appendChild(more);
    }
  }

  titleEl.appendChild(line);
}

function renderSessionsViews() {
  if (activeSessionsSubtab === 'recent') {
    renderSessionsList(_cachedSessions, document.getElementById('sessions-list'), {
      mode: 'recent',
      query: sessionSearchQuery(),
      projectFilter: sessionProjectFilterValue(),
      sourceFilter: sessionSourceFilterValue(),
      statusFilter: sessionStatusFilterValue(),
      sortValue: document.getElementById('sort-sessions')?.value || 'updated-desc',
      deepSearchOnly: false,
      hideSubagents: !sessionsShowSubagents(),
    });
  } else if (activeSessionsSubtab === 'deep') {
    renderSessionsList(_cachedSessions, document.getElementById('sessions-deep-list'), {
      mode: 'deep',
      query: sessionDeepResultSearchQuery(),
      projectFilter: sessionDeepProjectFilterValue(),
      sourceFilter: sessionDeepSourceFilterValue(),
      statusFilter: sessionDeepStatusFilterValue(),
      sortValue: document.getElementById('sort-sessions-deep')?.value || 'updated-desc',
      deepSearchOnly: true,
    });
  }
}

// Data-change rerender: keeps the user's expanded Show-more window.
function _refilterSessions() {
  if (sessionsLoaded) {
    renderSessionsViews();
  }
}
// Filter/search/sort change: the visible set changes shape, so the
// Show-more window snaps back to the default page size.
function _refreshSessionsFilters() {
  sessionsRenderWindow = SESSION_CARD_RENDER_PAGE;
  _refilterSessions();
}

function sessionProjectDirectory(session) {
  return compactSessionText(
    session?.project_root ||
    session?.projectRoot ||
    session?.project_dir ||
    session?.projectDir ||
    session?.project ||
    session?.cwd ||
    session?.workdir ||
    session?.workDir
  );
}

function sessionChangedSortValue(session) {
  return Math.max(
    sessionDateSortValue(session, 'updated_at'),
    sessionDateSortValue(session, 'changed_at'),
    sessionDateSortValue(session, 'created_at')
  );
}

// Throwaway scratch paths (agent test homes, e2e rigs, OS temp) can easily
// outnumber real projects in the corpus. They collapse into one synthetic
// menu entry instead of ballooning the picker; selecting it matches every
// temp-shaped path.
const SESSION_TEMP_PROJECTS_FILTER_VALUE = '::temp-projects::';

function sessionPathLooksTemporary(path) {
  if (!path) return false;
  if (/^\/(private\/)?var\/folders\//.test(path)) return true;
  return String(path)
    .split(/[\\/]+/)
    .some(seg => seg === 'tmp' || seg === 'temp' || seg === 'Temp' || seg === 'TEMP');
}

function sessionProjectFilterOptions(sessions) {
  const buckets = new Map();
  for (const session of Array.isArray(sessions) ? sessions : []) {
    const path = sessionProjectDirectory(session);
    if (!path) continue;
    const existing = buckets.get(path) || {
      path,
      label: compactPathLabel(path, true) || path,
      count: 0,
      latestChanged: 0,
    };
    existing.count += 1;
    existing.latestChanged = Math.max(existing.latestChanged, sessionChangedSortValue(session));
    buckets.set(path, existing);
  }
  const options = [];
  let temp = null;
  for (const item of buckets.values()) {
    if (!sessionPathLooksTemporary(item.path)) {
      options.push(item);
      continue;
    }
    temp = temp || {
      path: SESSION_TEMP_PROJECTS_FILTER_VALUE,
      label: 'Temporary directories',
      title: 'Sessions whose project directory is a throwaway temp path (/tmp, /var/folders, %TEMP%)',
      count: 0,
      paths: 0,
      latestChanged: 0,
    };
    temp.count += item.count;
    temp.paths += 1;
    temp.latestChanged = Math.max(temp.latestChanged, item.latestChanged);
  }
  if (temp) options.push(temp);
  return options.sort((a, b) => {
    const byChanged = b.latestChanged - a.latestChanged;
    if (byChanged) return byChanged;
    const byLabel = a.label.localeCompare(b.label, undefined, { sensitivity: 'base' });
    return byLabel || a.path.localeCompare(b.path);
  });
}

function sessionProjectMultiFilterOptions(sessions) {
  return sessionProjectFilterOptions(sessions).map(item => ({
    value: item.path,
    label: item.paths
      ? `${item.label} (${item.paths.toLocaleString()} paths, ${item.count.toLocaleString()})`
      : `${item.label} (${item.count.toLocaleString()})`,
    title: item.title || item.path,
    plural: 'projects',
  }));
}

// The project menus render a bounded page of the full option set: every
// selected value (unchecking must always be possible), then the most
// recently active projects up to the cap, with an inline filter box that
// narrows against the whole set. An uncapped render once ballooned the
// hidden DOM with hundreds of scratch-path checkboxes per menu.
const SESSION_PROJECT_MENU_CAP = 40;
const _sessionProjectMenuFilterText = new Map(); // menuId → live filter text

function sessionProjectMenuScaffold(menu, kind, cfg) {
  let optionsWrap = menu.querySelector(':scope > .sessions-multi-filter-options');
  if (optionsWrap) {
    return { search: menu.querySelector(':scope > .sessions-multi-filter-search'), optionsWrap };
  }
  menu.textContent = '';
  const search = document.createElement('div');
  search.className = 'sessions-multi-filter-search';
  const input = document.createElement('input');
  input.type = 'search';
  input.addEventListener('input', () => {
    _sessionProjectMenuFilterText.set(cfg.menuId, input.value.trim());
    renderSessionProjectFilterMenu(kind);
  });
  input.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape' || !input.value) return;
    input.value = '';
    _sessionProjectMenuFilterText.set(cfg.menuId, '');
    renderSessionProjectFilterMenu(kind);
    ev.stopPropagation();
  });
  search.appendChild(input);
  optionsWrap = document.createElement('div');
  optionsWrap.className = 'sessions-multi-filter-options';
  menu.appendChild(search);
  menu.appendChild(optionsWrap);
  return { search, optionsWrap };
}

function renderSessionProjectFilterMenu(kind) {
  const cfg = sessionMultiFilterConfig(kind);
  const menu = document.getElementById(cfg.menuId);
  if (!menu) return;
  const selected = new Set(parseStoredSessionMultiFilter(kind));
  const { search, optionsWrap } = sessionProjectMenuScaffold(menu, kind, cfg);
  const filterText = (_sessionProjectMenuFilterText.get(cfg.menuId) || '').toLowerCase();
  const searchInput = search.querySelector('input');
  searchInput.placeholder = `Filter ${cfg.options.length.toLocaleString()} projects...`;
  search.classList.toggle('hidden', cfg.options.length <= SESSION_PROJECT_MENU_CAP && !filterText);
  optionsWrap.textContent = '';

  const matchesFilter = opt => !filterText
    || opt.label.toLowerCase().includes(filterText)
    || String(opt.value).toLowerCase().includes(filterText)
    || String(opt.title || '').toLowerCase().includes(filterText);
  const visible = [];
  for (const opt of cfg.options) {
    if (selected.has(opt.value)) visible.push(opt);
  }
  let overflow = 0;
  let shown = 0;
  for (const opt of cfg.options) {
    if (selected.has(opt.value) || !matchesFilter(opt)) continue;
    if (shown >= SESSION_PROJECT_MENU_CAP) {
      overflow += 1;
      continue;
    }
    visible.push(opt);
    shown += 1;
  }

  if (visible.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'sessions-multi-filter-empty';
    empty.textContent = filterText
      ? `No projects match "${filterText}"`
      : 'No project directories';
    optionsWrap.appendChild(empty);
  }

  for (const opt of visible) {
    const label = document.createElement('label');
    if (opt.title) label.title = opt.title;
    const input = document.createElement('input');
    input.type = 'checkbox';
    input.value = opt.value;
    input.checked = selected.has(opt.value);
    const text = document.createElement('span');
    text.className = 'project-option-text';
    text.textContent = opt.label;
    label.appendChild(input);
    label.appendChild(text);
    optionsWrap.appendChild(label);
  }

  if (overflow > 0) {
    const more = document.createElement('div');
    more.className = 'sessions-multi-filter-more';
    more.textContent = `+${overflow.toLocaleString()} more — type to narrow`;
    optionsWrap.appendChild(more);
  }

  const validSelected = Array.from(selected).filter(value =>
    cfg.options.some(opt => opt.value === value)
  );
  try {
    if (validSelected.length > 0) {
      localStorage.setItem(cfg.key, JSON.stringify(validSelected));
    } else {
      localStorage.removeItem(cfg.key);
    }
  } catch (_) { /* Safari private mode / quota — selection stays in-memory */ }
  setSessionMultiFilterValues(kind, validSelected);
}

let _sessionProjectOptionsSerialized = null;

function updateSessionProjectFilterOptions(sessions = _cachedSessions) {
  sessionProjectFilterOptionsCache = sessionProjectMultiFilterOptions(sessions);
  // Streamed refreshes call this on every flush; skip the DOM work when the
  // option set didn't change, and never rebuild a menu the user has open —
  // it re-renders on its next open instead.
  const serialized = JSON.stringify(sessionProjectFilterOptionsCache.map(o => [o.value, o.label]));
  const changed = serialized !== _sessionProjectOptionsSerialized;
  _sessionProjectOptionsSerialized = serialized;
  for (const kind of ['project', 'deep-project']) {
    const menu = document.getElementById(sessionMultiFilterConfig(kind).menuId);
    if (!menu) continue;
    const rendered = menu.dataset.optionsRendered === '1';
    if (rendered && !changed) continue;
    if (rendered && !menu.classList.contains('hidden')) {
      menu.dataset.staleOptions = '1';
      continue;
    }
    renderSessionProjectFilterMenu(kind);
    menu.dataset.optionsRendered = '1';
    delete menu.dataset.staleOptions;
  }
}

function sessionMultiFilterConfig(kind) {
  if (kind === 'project') {
    return {
      key: SESSIONS_FILTER_PROJECT_KEY,
      buttonId: 'filter-session-project',
      menuId: 'filter-session-project-menu',
      allLabel: 'All projects',
      options: sessionProjectFilterOptionsCache,
    };
  }
  if (kind === 'deep-project') {
    return {
      key: SESSIONS_DEEP_FILTER_PROJECT_KEY,
      buttonId: 'filter-session-deep-project',
      menuId: 'filter-session-deep-project-menu',
      allLabel: 'All projects',
      options: sessionProjectFilterOptionsCache,
    };
  }
  if (kind === 'source') {
    return {
      key: SESSIONS_FILTER_SOURCE_KEY,
      buttonId: 'filter-session-source',
      menuId: 'filter-session-source-menu',
      allLabel: 'All sources',
      options: SESSION_SOURCE_FILTER_OPTIONS,
    };
  }
  if (kind === 'deep-source') {
    return {
      key: SESSIONS_DEEP_FILTER_SOURCE_KEY,
      buttonId: 'filter-session-deep-source',
      menuId: 'filter-session-deep-source-menu',
      allLabel: 'All sources',
      options: SESSION_SOURCE_FILTER_OPTIONS,
    };
  }
  if (kind === 'deep-status') {
    return {
      key: SESSIONS_DEEP_FILTER_STATUS_KEY,
      buttonId: 'filter-session-deep-status',
      menuId: 'filter-session-deep-status-menu',
      allLabel: 'All statuses',
      options: SESSION_STATUS_FILTER_OPTIONS,
    };
  }
  return {
    key: SESSIONS_FILTER_STATUS_KEY,
    buttonId: 'filter-session-status',
    menuId: 'filter-session-status-menu',
    allLabel: 'All statuses',
    options: SESSION_STATUS_FILTER_OPTIONS,
  };
}

function parseStoredSessionMultiFilter(kind) {
  const cfg = sessionMultiFilterConfig(kind);
  const stored = localStorage.getItem(cfg.key);
  const allowed = new Set(cfg.options.map(opt => opt.value));
  if (!stored || stored === 'all') return [];
  let parsed = null;
  try { parsed = JSON.parse(stored); } catch (_) {}
  const rawValues = Array.isArray(parsed)
    ? parsed
    : String(stored).split(',').map(v => v.trim());
  const seen = new Set();
  const values = [];
  for (const value of rawValues) {
    if (!allowed.has(value) || seen.has(value)) continue;
    seen.add(value);
    values.push(value);
  }
  return values;
}

function sessionMultiFilterValues(kind) {
  const cfg = sessionMultiFilterConfig(kind);
  const menu = document.getElementById(cfg.menuId);
  if (!menu) return [];
  return Array.from(menu.querySelectorAll('input[type="checkbox"]:checked'))
    .map(input => input.value)
    .filter(Boolean);
}

function sessionMultiFilterKey(values) {
  return values && values.length > 0 ? values.join(',') : 'all';
}

function updateSessionMultiFilterSummary(kind) {
  const cfg = sessionMultiFilterConfig(kind);
  const button = document.getElementById(cfg.buttonId);
  if (!button) return;
  const values = sessionMultiFilterValues(kind);
  if (values.length === 0) {
    button.textContent = cfg.allLabel;
    button.title = cfg.allLabel;
    return;
  }
  const labels = values
    .map(value => cfg.options.find(opt => opt.value === value)?.label || value);
  const plural = cfg.options[0]?.plural || 'filters';
  button.textContent = labels.length === 1 ? labels[0] : `${labels.length} ${plural}`;
  button.title = labels.join(', ');
}

function setSessionMultiFilterValues(kind, values) {
  const cfg = sessionMultiFilterConfig(kind);
  const selected = new Set(values || []);
  const menu = document.getElementById(cfg.menuId);
  if (!menu) return;
  menu.querySelectorAll('input[type="checkbox"]').forEach(input => {
    input.checked = selected.has(input.value);
  });
  updateSessionMultiFilterSummary(kind);
}

function restoreSessionMultiFilter(kind) {
  setSessionMultiFilterValues(kind, parseStoredSessionMultiFilter(kind));
}

function closeSessionMultiFilterMenus(exceptKind = '') {
  ['project', 'source', 'status', 'deep-project', 'deep-source', 'deep-status'].forEach(kind => {
    if (kind === exceptKind) return;
    const cfg = sessionMultiFilterConfig(kind);
    document.getElementById(cfg.menuId)?.classList.add('hidden');
    document.getElementById(cfg.buttonId)?.setAttribute('aria-expanded', 'false');
  });
}

function setupSessionMultiFilter(kind, onChange) {
  const cfg = sessionMultiFilterConfig(kind);
  const button = document.getElementById(cfg.buttonId);
  const menu = document.getElementById(cfg.menuId);
  if (!button || !menu) return;
  button.addEventListener('click', (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    const willOpen = menu.classList.contains('hidden');
    closeSessionMultiFilterMenus(willOpen ? kind : '');
    if (willOpen && menu.dataset.staleOptions === '1') {
      // Option refreshes are deferred while the menu is open (see
      // updateSessionProjectFilterOptions) — catch up before showing it.
      renderSessionProjectFilterMenu(kind);
      delete menu.dataset.staleOptions;
    }
    menu.classList.toggle('hidden', !willOpen);
    button.setAttribute('aria-expanded', willOpen ? 'true' : 'false');
  });
  menu.addEventListener('click', ev => ev.stopPropagation());
  menu.addEventListener('change', (ev) => {
    // The project menus embed a text filter input; only checkbox toggles
    // are selection changes.
    if (ev.target && ev.target.type !== 'checkbox') return;
    const values = sessionMultiFilterValues(kind);
    // Guarded like the attention-center writes: Safari private mode (and
    // storage-quota states) throw here and would kill the filter handler.
    try { localStorage.setItem(cfg.key, JSON.stringify(values)); } catch (_) {}
    updateSessionMultiFilterSummary(kind);
    onChange?.();
  });
  button.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') closeSessionMultiFilterMenus();
  });
  updateSessionMultiFilterSummary(kind);
}

setupSessionMultiFilter('source', _refreshSessionsFilters);
setupSessionMultiFilter('status', _refreshSessionsFilters);
setupSessionMultiFilter('project', _refreshSessionsFilters);
setupSessionMultiFilter('deep-project', _refreshSessionsFilters);
setupSessionMultiFilter('deep-source', _refreshSessionsFilters);
setupSessionMultiFilter('deep-status', _refreshSessionsFilters);
document.addEventListener('click', () => closeSessionMultiFilterMenus());
document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape') closeSessionMultiFilterMenus();
});
document.getElementById('sort-sessions')?.addEventListener('change', _refreshSessionsFilters);
document.getElementById('sessions-search')?.addEventListener('input', _refreshSessionsFilters);
document.getElementById('sessions-show-subagents')?.addEventListener('change', (ev) => {
  // Same Safari-private-mode guard as the multi-filter persistence above.
  try { localStorage.setItem(SESSIONS_SHOW_SUBAGENTS_KEY, String(!!ev.target.checked)); } catch (_) {}
  _refreshSessionsFilters();
});
document.getElementById('sort-sessions-deep')?.addEventListener('change', _refreshSessionsFilters);
document.getElementById('sessions-deep-result-search')?.addEventListener('input', _refreshSessionsFilters);
document.getElementById('sessions-deep-search-run')?.addEventListener('click', () => runSessionDeepSearch());
document.getElementById('sessions-deep-search-cancel')?.addEventListener('click', () => cancelSessionDeepSearch('Deep search cancelled.'));
document.getElementById('sessions-deep-search-clear')?.addEventListener('click', clearSessionDeepSearch);
document.getElementById('sessions-deep-search-query')?.addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter') {
    ev.preventDefault();
    runSessionDeepSearch();
  } else if (ev.key === 'Escape') {
    ev.preventDefault();
    clearSessionDeepSearch();
  }
});
document.getElementById('session-detail-rename')?.addEventListener('click', (ev) => {
  ev.preventDefault();
  if (currentSessionDetail) requestSessionRename(currentSessionDetail);
});
document.getElementById('session-detail-resume')?.addEventListener('click', (ev) => {
  ev.preventDefault();
  if (currentSessionDetail) resumeSession(currentSessionDetail);
});
document.getElementById('session-detail-config')?.addEventListener('click', (ev) => {
  ev.preventDefault();
  if (currentSessionDetail) openSessionConfigModal(currentSessionDetail);
});
document.getElementById('worktrees-search')?.addEventListener('input', _refilterWorktrees);
document.getElementById('sort-worktrees')?.addEventListener('change', _refilterWorktrees);
document.getElementById('filter-worktrees-active')?.addEventListener('change', _refilterWorktrees);
document.getElementById('filter-worktrees-dirty')?.addEventListener('change', _refilterWorktrees);
document.getElementById('filter-worktrees-unmerged')?.addEventListener('change', _refilterWorktrees);
document.getElementById('filter-worktrees-main')?.addEventListener('change', _refilterWorktrees);
document.getElementById('worktrees-refresh-btn')?.addEventListener('click', () => loadWorktrees({ forceScan: false }));
document.getElementById('worktrees-scan-btn')?.addEventListener('click', () => loadWorktrees({ forceScan: true }));
document.getElementById('new-session-project-root')?.addEventListener('input', (ev) => {
  ev.currentTarget.title = ev.currentTarget.value.trim();
  scheduleNewSessionProjectStatusRefresh({ hideWhileChecking: true });
});
// Drift detection is event-driven, not a forever-poll (was a 500ms
// interval): 'change' fires on datalist picks and committed edits, 'blur'
// catches abandoned edits, 'focus' catches a programmatic write that
// landed while the field sat idle. Every programmatic writer schedules its
// own refresh today (setNewSessionProjectRoot, updateNewSessionProjectPrefills,
// ensureNewSessionProjectDirectory, the fs-picker apply path), so the
// remaining uncovered case is a silent .value write from future code while
// the field stays unfocused.
for (const evName of ['change', 'blur', 'focus']) {
  document.getElementById('new-session-project-root')
    ?.addEventListener(evName, refreshNewSessionProjectStatusOnValueDrift);
}
document.getElementById('new-session-create-project-dir')?.addEventListener('change', refreshNewSessionProjectStatus);
document.getElementById('new-session-agent')?.addEventListener('change', () => {
  renderNewSessionAgentControls({ replaceCommand: true });
});
document.getElementById('new-session-codex-managed-context')?.addEventListener('change', (ev) => {
  newSessionCodexManagedContext = ev.currentTarget.value === 'managed' ? 'managed' : 'vanilla';
  renderNewSessionAgentControls();
});
document.getElementById('new-session-codex-sandbox')?.addEventListener('change', (ev) => {
  newSessionCodexSandbox = normalizeCodexSandbox(ev.currentTarget.value);
  renderNewSessionAgentControls();
});
document.getElementById('new-session-codex-approval-policy')?.addEventListener('change', (ev) => {
  newSessionCodexApprovalPolicy = normalizeCodexApprovalPolicy(ev.currentTarget.value);
  renderNewSessionAgentControls();
});
document.getElementById('new-session-codex-context-archive')?.addEventListener('change', (ev) => {
  newSessionCodexContextArchive = normalizeContextArchiveMode(ev.currentTarget.value);
  renderNewSessionAgentControls();
});
document.getElementById('new-session-codex-fast')?.addEventListener('change', (ev) => {
  newSessionCodexFastModeTouched = true;
  newSessionCodexFastMode = !!ev.currentTarget.checked;
  renderNewSessionAgentControls();
});
document.getElementById('fs-picker-path')?.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Enter') return;
  ev.preventDefault();
  loadFsPickerPath();
});
document.getElementById('sessions-search')?.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Escape' || !ev.currentTarget.value) return;
  ev.currentTarget.value = '';
  _refreshSessionsFilters();
  ev.stopPropagation();
});
document.getElementById('sessions-deep-result-search')?.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Escape' || !ev.currentTarget.value) return;
  ev.currentTarget.value = '';
  _refreshSessionsFilters();
  ev.stopPropagation();
});

function sessionDateSortValue(session, field) {
  const raw = session[field] || (field === 'updated_at' ? session.created_at : '');
  if (!raw) return 0;
  const normalized = typeof raw === 'string' && raw.includes(' ') && !raw.includes('T')
    ? raw.replace(' ', 'T')
    : raw;
  const parsed = Date.parse(normalized);
  return Number.isNaN(parsed) ? 0 : parsed;
}

// Status tones are reserved meaning (see the .ui-* primitives): ok=finished,
// info=running, err=failed/interrupted, muted=idle/unknown.
function sessionStatusChipTone(status) {
  switch (status) {
    case 'completed':
    case 'done':
      return 'ok';
    case 'in_progress':
    case 'running':
    case 'active':
      return 'info';
    case 'failed':
    case 'error':
      return 'err';
    case 'interrupted':
      return 'warn';
    default:
      return 'muted';
  }
}

// "3m ago" style label for card meta rows; absolute timestamps stay in the
// tooltip. Falls back to '' (caller shows the raw value) if unparseable.
function sessionRelativeLabel(value) {
  if (!value) return '';
  const normalized = typeof value === 'string' && value.includes(' ') && !value.includes('T')
    ? value.replace(' ', 'T')
    : value;
  const parsed = Date.parse(normalized);
  if (Number.isNaN(parsed)) return '';
  const diff = Date.now() - parsed;
  if (diff < 0) return 'just now';
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo ago`;
  return `${Math.floor(days / 365)}y ago`;
}

function sessionSearchQuery() {
  return (document.getElementById('sessions-search')?.value || '').trim().toLowerCase();
}

function sessionDeepResultSearchQuery() {
  return (document.getElementById('sessions-deep-result-search')?.value || '').trim().toLowerCase();
}

function sessionProjectFilterValue() {
  return sessionMultiFilterValues('project');
}

function sessionSourceFilterValue() {
  return sessionMultiFilterValues('source');
}

function sessionStatusFilterValue() {
  return sessionMultiFilterValues('status');
}

function sessionDeepSourceFilterValue() {
  return sessionMultiFilterValues('deep-source');
}

function sessionDeepStatusFilterValue() {
  return sessionMultiFilterValues('deep-status');
}

function sessionDeepProjectFilterValue() {
  return sessionMultiFilterValues('deep-project');
}

function sessionMatchesProjectFilter(session, filter) {
  const filters = Array.isArray(filter) ? filter : (filter && filter !== 'all' ? [filter] : []);
  if (filters.length === 0) return true;
  const dir = sessionProjectDirectory(session);
  if (filters.includes(SESSION_TEMP_PROJECTS_FILTER_VALUE) && sessionPathLooksTemporary(dir)) return true;
  return filters.includes(dir);
}

function sessionMatchesSourceFilter(source, filter) {
  const filters = Array.isArray(filter) ? filter : (filter && filter !== 'all' ? [filter] : []);
  if (filters.length === 0) return true;
  if (filters.includes('external') && source !== 'intendant') return true;
  return filters.includes(source);
}

function sessionMatchesStatusFilter(status, filter) {
  const filters = Array.isArray(filter) ? filter : (filter && filter !== 'all' ? [filter] : []);
  if (filters.length === 0) return true;
  if (filters.includes('active') && (status === 'running' || status === 'in_progress')) return true;
  return filters.includes(status);
}

function sessionLogSearchKey(source, sessionId) {
  return `${source || 'intendant'}:${sessionId || ''}`;
}

function sessionDeepSearchHasMissingMetadata(results = _sessionDeepSearch.results) {
  if (!results || typeof results.keys !== 'function' || results.size === 0) return false;
  const known = new Set((_cachedSessions || []).map(session =>
    sessionLogSearchKey(session.source || 'intendant', session.session_id)
  ));
  for (const key of results.keys()) {
    if (!known.has(key)) return true;
  }
  return false;
}

function mergeDeepSearchResultSessions(results = _sessionDeepSearch.results) {
  if (!results || typeof results.values !== 'function' || results.size === 0) return;
  const known = new Set((_cachedSessions || []).map(session =>
    sessionLogSearchKey(session.source || 'intendant', session.session_id)
  ));
  const additions = [];
  for (const hit of results.values()) {
    if (!hit || typeof hit.session !== 'object' || hit.session === null) continue;
    const session = { ...hit.session };
    session.source = session.source || hit.source || 'intendant';
    session.session_id = session.session_id || hit.session_id;
    if (!session.session_id) continue;
    const key = sessionLogSearchKey(session.source || 'intendant', session.session_id);
    if (known.has(key)) continue;
    known.add(key);
    additions.push(session);
  }
  if (additions.length) {
    _cachedSessions = [...additions, ..._cachedSessions];
  }
}

function scheduleSessionMetadataRefreshForDeepSearch(token) {
  for (const delay of [0, 2000, 10000]) {
    setTimeout(() => {
      if (token !== _sessionDeepSearchToken) return;
      if (!_sessionDeepSearch.active || !_sessionDeepSearch.results?.size) return;
      if (sessionDeepSearchHasMissingMetadata()) {
        loadSessions({ force: true });
      }
    }, delay);
  }
}

function sessionDeepSearchQuery() {
  return (document.getElementById('sessions-deep-search-query')?.value || '').trim();
}

function sessionDeepSearchMode() {
  return document.getElementById('sessions-deep-search-mode')?.value || 'all_keywords';
}

function sessionDeepSearchModeLabel(mode) {
  switch (mode) {
    case 'exact_phrase': return 'exact phrase';
    case 'any_keyword_session': return 'any keyword anywhere in session';
    case 'user_message_all_keywords': return 'all keywords in one user message';
    case 'all_keywords':
    default:
      return 'all keywords in one entry';
  }
}

function sessionDeepSearchProjectScopeText(projectFilter) {
  const count = Array.isArray(projectFilter) ? projectFilter.length : 0;
  if (!count) return '';
  return ` in ${count.toLocaleString()} selected project${count === 1 ? '' : 's'}`;
}

function setSessionDeepSearchLoading(loading) {
  const runBtn = document.getElementById('sessions-deep-search-run');
  const cancelBtn = document.getElementById('sessions-deep-search-cancel');
  if (runBtn) runBtn.disabled = !!loading;
  if (cancelBtn) cancelBtn.classList.toggle('hidden', !loading);
}

function runSessionDeepSearch() {
  const query = sessionDeepSearchQuery();
  const mode = sessionDeepSearchMode();
  const projectFilter = sessionDeepProjectFilterValue();
  if (query.length < 2) {
    _sessionDeepSearch = {
      query,
      mode,
      sourceFilter: 'all',
      active: false,
      loading: false,
      waiting: false,
      loaded: false,
      error: 'Enter at least 2 characters for deep search.',
      projectFilter,
      searched: 0,
      truncated: false,
      limit: 0,
      truncatedFiles: 0,
      results: new Map(),
    };
    updateSessionsSearchStatus();
    return;
  }
  if (_sessionDeepSearchAbort) {
    _sessionDeepSearchAbort.abort();
  }
  const controller = new AbortController();
  _sessionDeepSearchAbort = controller;
  _sessionDeepSearch = {
    query,
    mode,
    sourceFilter: 'all',
    active: false,
    loading: true,
    waiting: false,
    loaded: false,
    error: null,
    projectFilter,
    searched: 0,
    truncated: false,
    limit: 0,
    truncatedFiles: 0,
    results: new Map(),
  };
  const token = ++_sessionDeepSearchToken;
  setSessionDeepSearchLoading(true);
  updateSessionsSearchStatus();
  _refilterSessions();

  const retryDelay = attempt => Math.min(500 + attempt * 250, 2000);
  const executeSearch = (attempt = 0) => {
    fetchSessionsSearchPayload({
      query,
      source: 'all',
      mode,
      projects: projectFilter,
      signal: controller.signal,
    })
    .then(data => {
      if (token !== _sessionDeepSearchToken) return;
      if (data.busy) {
        _sessionDeepSearch = {
          ..._sessionDeepSearch,
          loading: true,
          waiting: true,
          loaded: false,
          error: null,
        };
        updateSessionsSearchStatus();
        setTimeout(() => {
          if (token !== _sessionDeepSearchToken || _sessionDeepSearchAbort !== controller) return;
          executeSearch(attempt + 1);
        }, retryDelay(attempt));
        return;
      }
      if (data.error) throw new Error(data.error);
      const results = new Map();
      for (const hit of data.results || []) {
        const key = hit.key || sessionLogSearchKey(hit.source, hit.session_id);
        results.set(key, hit);
      }
      _sessionDeepSearch = {
        query,
        mode,
        sourceFilter: 'all',
        active: true,
        loading: false,
        waiting: false,
        loaded: true,
        error: null,
        projectFilter,
        searched: data.searched || 0,
        truncated: !!data.truncated,
        limit: data.limit || 0,
        truncatedFiles: data.truncated_files || data.skipped_large || 0,
        results,
      };
	      _sessionDeepSearchAbort = null;
	      setSessionDeepSearchLoading(false);
      mergeDeepSearchResultSessions(results);
      _refilterSessions();
	      if (results.size > 0) scheduleSessionMetadataRefreshForDeepSearch(token);
	    })
    .catch(err => {
      if (token !== _sessionDeepSearchToken) return;
      const aborted = err?.name === 'AbortError';
      _sessionDeepSearch = {
        query,
        mode,
        sourceFilter: 'all',
        active: false,
        loading: false,
        waiting: false,
        loaded: !aborted,
        error: aborted ? 'Deep search cancelled.' : (err.message || String(err)),
        projectFilter,
        searched: 0,
        truncated: false,
        limit: 0,
        truncatedFiles: 0,
        results: new Map(),
      };
      _sessionDeepSearchAbort = null;
      setSessionDeepSearchLoading(false);
      _refreshSessionsFilters();
    });
  };
  executeSearch();
}

function cancelSessionDeepSearch(message = '') {
  if (_sessionDeepSearchAbort) {
    _sessionDeepSearchAbort.abort();
    _sessionDeepSearchAbort = null;
  }
  _sessionDeepSearchToken += 1;
  _sessionDeepSearch = {
    ..._sessionDeepSearch,
    active: false,
    loading: false,
    waiting: false,
    loaded: false,
    error: message || null,
    projectFilter: _sessionDeepSearch.projectFilter || [],
    truncatedFiles: 0,
    results: new Map(),
  };
  setSessionDeepSearchLoading(false);
  _refreshSessionsFilters();
}

function clearSessionDeepSearch() {
  cancelSessionDeepSearch('');
  const input = document.getElementById('sessions-deep-search-query');
  if (input) input.value = '';
  _sessionDeepSearch = {
    query: '',
    mode: sessionDeepSearchMode(),
    sourceFilter: 'all',
    active: false,
    loading: false,
    waiting: false,
    loaded: false,
    error: null,
    projectFilter: [],
    searched: 0,
    truncated: false,
    limit: 0,
    truncatedFiles: 0,
    results: new Map(),
  };
  _refreshSessionsFilters();
}

function sessionDeepSearchHit(session, source) {
  if (!_sessionDeepSearch.active || !_sessionDeepSearch.query) return null;
  return _sessionDeepSearch.results.get(sessionLogSearchKey(source, session.session_id)) || null;
}

// ── Deep-search progress (additive) ──
// The daemon may emit {"type":"deep_search_progress","scanned":N,"total":M,
// "matched":K} lines on the deep-search lane (newer daemons only). This is
// the single entry point that folds one such line into the running banner —
// whatever transport surfaces the lines calls it by name at event time.
// Absence of progress lines changes nothing: the banner keeps its prose.
function applySessionDeepSearchProgress(progress) {
  if (!progress || typeof progress !== 'object') return;
  if (!_sessionDeepSearch || !_sessionDeepSearch.loading) return;
  const scanned = Number(progress.scanned);
  const total = Number(progress.total);
  const matched = Number(progress.matched);
  if (!Number.isFinite(scanned) || scanned < 0) return;
  _sessionDeepSearch.progress = {
    scanned,
    total: Number.isFinite(total) && total > 0 ? total : 0,
    matched: Number.isFinite(matched) && matched >= 0 ? matched : 0,
  };
  updateSessionsSearchStatus();
}

function appendSessionsDeepSearchProgressLine(el, progress) {
  const line = document.createElement('div');
  line.className = 'sessions-deep-search-progress';
  const text = document.createElement('span');
  text.className = 'sessions-deep-search-progress-text';
  text.textContent = progress.total > 0
    ? `Scanned ${progress.scanned.toLocaleString()} of ${progress.total.toLocaleString()} · ${progress.matched.toLocaleString()} matched`
    : `Scanned ${progress.scanned.toLocaleString()} · ${progress.matched.toLocaleString()} matched`;
  line.appendChild(text);
  if (progress.total > 0) {
    const track = document.createElement('div');
    track.className = 'sessions-deep-search-progress-track';
    const fill = document.createElement('div');
    fill.className = 'sessions-deep-search-progress-fill';
    const pct = Math.max(0, Math.min(100, (progress.scanned / progress.total) * 100));
    fill.style.width = `${pct}%`;
    track.appendChild(fill);
    line.appendChild(track);
  }
  el.appendChild(line);
}

function updateSessionsSearchStatus() {
  const el = document.getElementById('sessions-deep-search-status');
  if (!el) return;
  el.classList.remove('error');
  el.textContent = '';
  if (_sessionDeepSearch.loading) {
    const projectScope = sessionDeepSearchProjectScopeText(_sessionDeepSearch.projectFilter);
    const prefix = _sessionDeepSearch.waiting
      ? 'Deep search waiting for the previous search to finish'
      : 'Deep search running';
    el.textContent = `${prefix}${projectScope} for "${_sessionDeepSearch.query}" (${sessionDeepSearchModeLabel(_sessionDeepSearch.mode)}). It is scanning every matching session log and can take a while on large histories.`;
    // Progress lines are optional (old daemons never send them).
    if (_sessionDeepSearch.progress) {
      appendSessionsDeepSearchProgressLine(el, _sessionDeepSearch.progress);
    }
    return;
  }
  if (_sessionDeepSearch.error) {
    el.classList.add('error');
    el.textContent = _sessionDeepSearch.error;
    return;
  }
  if (_sessionDeepSearch.loaded) {
    const resultCount = _sessionDeepSearch.results.size;
    const projectScope = sessionDeepSearchProjectScopeText(_sessionDeepSearch.projectFilter);
    const suffix = _sessionDeepSearch.truncated
      ? ' Results were truncated before the exhaustive scan completed.'
      : '';
    el.textContent = `Deep search scanned all ${_sessionDeepSearch.searched.toLocaleString()} matching sessions${projectScope} with ${sessionDeepSearchModeLabel(_sessionDeepSearch.mode)}; ${resultCount.toLocaleString()} had log matches.${suffix}`;
  }
}

// Per-row haystack memo: without it every quick-search keystroke rebuilds
// a ~30-field string for every row in the corpus. Keyed by row object
// identity — mergeSessionRows keeps identity for untouched rows and swaps
// it when a row changes, so entries invalidate themselves; the two inputs
// that can drift independently of the row (current-session status override)
// revalidate explicitly. source/shortId derive from the row and need no key.
const _sessionSearchTextCache = new WeakMap();

function sessionSearchText(session, displayStatus, source, shortId, isCurrent) {
  const cached = _sessionSearchTextCache.get(session);
  if (cached && cached.status === displayStatus && cached.current === isCurrent) {
    return cached.text;
  }
  const totalBytes = session.total_bytes || 0;
  // Server-side conversation preview (first user/assistant messages) —
  // quick search reaches into conversation text without a Deep Search.
  const previewText = Array.isArray(session.preview)
    ? session.preview.map(p => (p && p.text) || '').filter(Boolean).join(' ')
    : '';
  const fields = [
    session.session_id,
    shortId,
    session.resume_id,
    session.name,
    session.task,
    displayStatus,
    session.status,
    source,
    source !== 'intendant' ? 'external' : '',
    session.source_label,
    prettyAgentName(source),
    session.role,
    session.relationship_kind,
    session.relationship,
    session.parent_session_id,
    session.parent_id,
    session.thread_source,
    session.agent_nickname,
    isCurrent ? 'current' : '',
    session.provider,
    session.model,
    session.created_at,
    session.updated_at,
    session.changed_at,
    session.project_root,
    session.cwd,
    session.path,
    session.turns != null ? `${session.turns} turns` : '',
    session.recordings ? `${session.recordings} recordings` : '',
    session.annotations ? `${session.annotations} annotations` : '',
    session.clips ? `${session.clips} clips` : '',
    totalBytes > 0 ? `${_fmtBytes(totalBytes)} size` : '',
    session.total_tokens != null ? `${session.total_tokens} tokens` : '',
    session.estimated_cost != null ? `$${Number(session.estimated_cost).toFixed(4)}` : '',
    previewText,
  ];
  const text = fields
    .filter(v => v !== null && v !== undefined && v !== '')
    .join(' ')
    .toLowerCase();
  _sessionSearchTextCache.set(session, { status: displayStatus, current: isCurrent, text });
  return text;
}

function sessionMatchesSearch(session, query, displayStatus, source, shortId, isCurrent) {
  if (!query) return true;
  const haystack = sessionSearchText(session, displayStatus, source, shortId, isCurrent);
  return query.split(/\s+/).every(term => haystack.includes(term));
}

// Compact tile value + the exact figure for the hover: 144,900,123,456
// renders as "144.9B" with the full number in the title attribute.
function sessionsAggregateTokenTile(label, value, loading) {
  return {
    label,
    value: loading ? 'Loading' : formatCompactNumber(value),
    title: loading ? '' : `${Number(value || 0).toLocaleString()} tokens`,
    loading,
  };
}

function renderSessionsAggregate(sessions, el) {
  if (!el) return;
  const totalSessions = sessions.length;
  const externalSessions = sessions.filter(s => (s.source || 'intendant') !== 'intendant').length;
  const detailsLoading = sessions.some(s => s && s.partial === true);
  const totalTokens = sessions.reduce((sum, s) => sum + (s.total_tokens || 0), 0);
  const totalInput = sessions.reduce((sum, s) => sum + (s.prompt_tokens || 0), 0);
  const totalOutput = sessions.reduce((sum, s) => sum + (s.completion_tokens || 0), 0);
  const totalCached = sessions.reduce((sum, s) => sum + (s.cached_tokens || 0), 0);
  // Truthful liveness: "active" means a session this dashboard supervises
  // in a live agent phase right now. The row status alone over-counts —
  // `in_progress` is the on-disk default any crashed/abandoned session
  // keeps forever — so the rest of the running/in_progress rows surface as
  // "open" (not finalized), which is all the status field actually says.
  const statusOpenSessions = sessions.filter(s => s.status === 'running' || s.status === 'in_progress');
  let liveActiveSessions = 0;
  if (typeof sessionWindows !== 'undefined' && typeof isAgentActivePhase === 'function') {
    liveActiveSessions = statusOpenSessions.filter(s => {
      const win = sessionWindows.get(String(s.session_id || ''));
      return win && !win.ended && isAgentActivePhase(win.phase);
    }).length;
  }
  const openSessions = statusOpenSessions.length - liveActiveSessions;

  el.innerHTML = '';

  const totalDiskBytes = sessions.reduce((sum, s) => sum + (s.total_bytes || 0), 0);
  const loadingValue = 'Loading';

  const sessionsSubParts = [];
  if (detailsLoading) sessionsSubParts.push('visible so far');
  if (externalSessions > 0) sessionsSubParts.push(`${externalSessions.toLocaleString()} external`);
  if (liveActiveSessions > 0) sessionsSubParts.push(`${liveActiveSessions.toLocaleString()} active`);
  if (openSessions > 0) sessionsSubParts.push(`${openSessions.toLocaleString()} open`);
  const sessionsTile = {
    label: 'Sessions',
    value: totalSessions.toLocaleString(),
    sub: sessionsSubParts.join(' · '),
    title: openSessions > 0
      ? 'active = supervised here in a live agent phase · open = not marked ended on disk'
      : '',
  };
  const inputTile = sessionsAggregateTokenTile('Input', totalInput, detailsLoading);
  const cachedTile = sessionsAggregateTokenTile('Cached', totalCached, detailsLoading);
  const outputTile = sessionsAggregateTokenTile('Output', totalOutput, detailsLoading);
  const diskTile = { label: 'Disk', value: detailsLoading ? loadingValue : _fmtBytes(totalDiskBytes), loading: detailsLoading };

  // The reference KPI set — Sessions · Tokens · Cost · Active days —
  // leads, with the breakdown tiles as a second row. All values are
  // real: cost sums per-row estimates; active days counts distinct
  // calendar days that saw session activity (created or last changed —
  // the honest client-side read; no per-day server metric exists).
  const totalCost = sessions.reduce((sum, s) => sum + (s.estimated_cost || 0), 0);
  const activeDayKeys = new Set();
  for (const s of sessions) {
    for (const value of [s.created_at, s.updated_at || s.changed_at]) {
      if (!value) continue;
      const t = Date.parse(value);
      if (Number.isNaN(t)) continue;
      const d = new Date(t);
      activeDayKeys.add(`${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`);
    }
  }
  const cards = [
    sessionsTile,
    sessionsAggregateTokenTile('Tokens', totalTokens, detailsLoading),
    {
      label: 'Cost',
      value: detailsLoading
        ? loadingValue
        : '$' + totalCost.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 }),
      loading: detailsLoading,
    },
    { label: 'Active days', value: activeDayKeys.size.toLocaleString(), sub: detailsLoading ? 'visible so far' : '' },
    inputTile, cachedTile, outputTile, diskTile,
  ];

  renderAggregateStatTiles(el, cards);
}

// Shared ui-stat tile builder for the Sessions/Worktrees aggregate strips.
// Tiles keep the legacy .agg-* classes (JS/QA hooks) alongside .ui-stat.
function renderAggregateStatTiles(el, cards) {
  for (const c of cards) {
    const card = document.createElement('div');
    card.className = 'ui-stat agg-card' + (c.loading ? ' loading' : '');
    if (c.title) card.title = c.title;
    const labelEl = document.createElement('div');
    labelEl.className = 'ui-stat-label agg-label';
    labelEl.textContent = c.label;
    const valueEl = document.createElement('div');
    valueEl.className = 'ui-stat-value agg-value';
    valueEl.textContent = c.value;
    card.appendChild(labelEl);
    card.appendChild(valueEl);
    if (c.sub) {
      const subEl = document.createElement('div');
      subEl.className = 'ui-stat-sub agg-sub';
      subEl.textContent = c.sub;
      card.appendChild(subEl);
    }
    el.appendChild(card);
  }
}

