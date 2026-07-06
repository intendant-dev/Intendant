// ── Debug Screen ──
let debugScreenActive = false;
let debugRecording = false;
const browserWorkspaces = new Map();

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
  if (status) status.textContent = rows.length ? `${rows.length} active workspace${rows.length === 1 ? '' : 's'}` : '';
  if (!rows.length) {
    list.innerHTML = '<div class="ui-empty compact"><div class="ui-empty-title">No browser workspaces</div>' +
      '<div class="ui-empty-hint">Create one above, or the agent will spawn its own when it needs a browser.</div></div>';
    return;
  }
  list.innerHTML = '';
  for (const w of rows) {
    const card = document.createElement('div');
    card.className = 'debug-workspace-row';
    const meta = document.createElement('div');
    meta.className = 'debug-workspace-row-main';
    const lease = w.lease ? ` leased by ${w.lease.holder_id || 'unknown'}` : ' unleased';
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
  if (status && d.kind === 'error') status.textContent = d.message || 'Browser workspace error';
  renderBrowserWorkspaces();
}

function createBrowserWorkspaceFromDebug() {
  if (!app) return;
  const g = id => document.getElementById(id);
  dispatchDashboardActionMsg({
    action: 'create_browser_workspace',
    url: (g('browser-workspace-url')?.value || '').trim() || undefined,
    provider: g('browser-workspace-provider')?.value || 'auto',
    label: (g('browser-workspace-label')?.value || '').trim() || undefined,
    owner_session_id: currentSessionId || undefined,
  });
}

function acquireBrowserWorkspace(workspaceId, force = false) {
  if (!app || !workspaceId) return;
  dispatchDashboardActionMsg({
    action: 'acquire_browser_workspace',
    workspace_id: workspaceId,
    holder_id: currentSessionId || ((typeof connectionId !== 'undefined' && connectionId) ? connectionId : 'dashboard'),
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

function applyLoadedSessions(sessions, aggEl, hostId = currentSessionsHostId()) {
  const isSelf = hostId === selfPeerId;
  const isActiveView = hostId === currentSessionsHostId();
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
  sessionsRenderWindow = SESSION_CARD_RENDER_LIMIT;
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
    // Fresh (uncached) load replaces the list — reset the Show-more window.
    sessionsRenderWindow = SESSION_CARD_RENDER_LIMIT;
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
      if (err?.name === 'AbortError') {
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
          listEl.innerHTML = '<div class="empty-state">Failed to load sessions</div>';
        }
      });
    });
}

function scheduleSessionsMetadataRefresh(delay = 700) {
  if (!sessionsLoaded && !shouldPollSessionWindowMetadata()) return;
  if (sessionsMetadataRefreshTimer) clearTimeout(sessionsMetadataRefreshTimer);
  sessionsMetadataRefreshTimer = setTimeout(() => {
    sessionsMetadataRefreshTimer = null;
    if (sessionsLoaded) {
      loadSessions({ force: true });
    } else {
      refreshSessionWindowMetadata(0, { force: true });
    }
  }, delay);
}

function eventRefreshesSessionMetadata(eventName) {
  switch (eventName) {
    case 'turn_started':
    case 'model_response':
    case 'done_signal':
    case 'task_complete':
    case 'round_complete':
    case 'interrupted':
    case 'session_ended':
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

function buildSessionLineageIndex(sessions) {
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
  return { byId, childrenByParentId };
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
  const target = targetSession || {
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
  sessionsRenderWindow = SESSION_CARD_RENDER_LIMIT;
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
  return Array.from(buckets.values()).sort((a, b) => {
    const byChanged = b.latestChanged - a.latestChanged;
    if (byChanged) return byChanged;
    const byLabel = a.label.localeCompare(b.label, undefined, { sensitivity: 'base' });
    return byLabel || a.path.localeCompare(b.path);
  });
}

function sessionProjectMultiFilterOptions(sessions) {
  return sessionProjectFilterOptions(sessions).map(item => ({
    value: item.path,
    label: `${item.label} (${item.count.toLocaleString()})`,
    title: item.path,
    plural: 'projects',
  }));
}

function renderSessionProjectFilterMenu(kind) {
  const cfg = sessionMultiFilterConfig(kind);
  const menu = document.getElementById(cfg.menuId);
  if (!menu) return;
  const selected = new Set(parseStoredSessionMultiFilter(kind));
  menu.innerHTML = '';

  if (cfg.options.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'sessions-multi-filter-empty';
    empty.textContent = 'No project directories';
    menu.appendChild(empty);
  }

  for (const opt of cfg.options) {
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
    menu.appendChild(label);
  }

  const validSelected = Array.from(selected).filter(value =>
    cfg.options.some(opt => opt.value === value)
  );
  if (validSelected.length > 0) {
    localStorage.setItem(cfg.key, JSON.stringify(validSelected));
  } else {
    localStorage.removeItem(cfg.key);
  }
  setSessionMultiFilterValues(kind, validSelected);
}

function updateSessionProjectFilterOptions(sessions = _cachedSessions) {
  sessionProjectFilterOptionsCache = sessionProjectMultiFilterOptions(sessions);
  renderSessionProjectFilterMenu('project');
  renderSessionProjectFilterMenu('deep-project');
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
    menu.classList.toggle('hidden', !willOpen);
    button.setAttribute('aria-expanded', willOpen ? 'true' : 'false');
  });
  menu.addEventListener('click', ev => ev.stopPropagation());
  menu.addEventListener('change', () => {
    const values = sessionMultiFilterValues(kind);
    localStorage.setItem(cfg.key, JSON.stringify(values));
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
  localStorage.setItem(SESSIONS_SHOW_SUBAGENTS_KEY, String(!!ev.target.checked));
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
window.setInterval(refreshNewSessionProjectStatusOnValueDrift, 500);
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
  return filters.includes(sessionProjectDirectory(session));
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

function sessionSearchText(session, displayStatus, source, shortId, isCurrent) {
  const totalBytes = session.total_bytes || 0;
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
  ];
  return fields
    .filter(v => v !== null && v !== undefined && v !== '')
    .join(' ')
    .toLowerCase();
}

function sessionMatchesSearch(session, query, displayStatus, source, shortId, isCurrent) {
  if (!query) return true;
  const haystack = sessionSearchText(session, displayStatus, source, shortId, isCurrent);
  return query.split(/\s+/).every(term => haystack.includes(term));
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
  const activeSessions = sessions.filter(s => s.status === 'running' || s.status === 'in_progress').length;

  el.innerHTML = '';

  const totalDiskBytes = sessions.reduce((sum, s) => sum + (s.total_bytes || 0), 0);
  const loadingValue = 'Loading';

  const sessionsSubParts = [];
  if (detailsLoading) sessionsSubParts.push('visible so far');
  if (externalSessions > 0) sessionsSubParts.push(`${externalSessions.toLocaleString()} external`);
  if (activeSessions > 0) sessionsSubParts.push(`${activeSessions.toLocaleString()} active`);
  const cards = [
    { label: 'Sessions', value: totalSessions.toLocaleString(), sub: sessionsSubParts.join(' · ') },
    { label: 'Total Tokens', value: detailsLoading ? loadingValue : totalTokens.toLocaleString(), loading: detailsLoading },
    { label: 'Input', value: detailsLoading ? loadingValue : totalInput.toLocaleString(), loading: detailsLoading },
    { label: 'Cached', value: detailsLoading ? loadingValue : totalCached.toLocaleString(), loading: detailsLoading },
    { label: 'Output', value: detailsLoading ? loadingValue : totalOutput.toLocaleString(), loading: detailsLoading },
    { label: 'Disk', value: detailsLoading ? loadingValue : _fmtBytes(totalDiskBytes), loading: detailsLoading },
  ];

  renderAggregateStatTiles(el, cards);
}

// Shared ui-stat tile builder for the Sessions/Worktrees aggregate strips.
// Tiles keep the legacy .agg-* classes (JS/QA hooks) alongside .ui-stat.
function renderAggregateStatTiles(el, cards) {
  for (const c of cards) {
    const card = document.createElement('div');
    card.className = 'ui-stat agg-card' + (c.loading ? ' loading' : '');
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

