function runSessionWindowAction(sessionId, op) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  if (op === 'copy-session-id') {
    copyTextToClipboard(sid)
      .then(() => showControlToast('success', `Copied session ID ${shortSessionId(sid)}`))
      .catch(err => showControlToast('error', `Copy session ID failed: ${err?.message || err}`));
    return;
  }
  if (op === 'rename-session') {
    const win = sessionWindows.get(sid);
    const meta = sessionMetadataById.get(sid) || {};
    requestSessionRename({
      session_id: sid,
      source: meta.backendSource || meta.source || win?.source || '',
      backendSessionId: meta.backendSessionId || '',
      name: meta.name || '',
    });
    return;
  }
  if (op === 'configure-launch') {
    openSessionConfigModal(sid);
    return;
  }
  if (op === 'restart-session') {
    restartSessionWindowAction(sid);
    return;
  }
  if (op === 'attach-session') {
    attachSessionWindowAction(sid);
    return;
  }
  if (op === 'stop-session') {
    stopSessionWindowAction(sid);
    return;
  }
  runSessionWindowCodexAction(sid, op);
}

async function runSessionWindowCodexAction(sessionId, op) {
  const sid = String(sessionId || '').trim();
  const spec = sessionWindowCodexActionByOp(op);
  if (!sid || !spec) return;
  // Sessions qualify by ADVERTISED thread-action ops (universal), with the
  // codex heuristic as the legacy fallback for capability-less replays.
  if (!Array.isArray(sessionThreadActionOps(sid)) && !sessionWindowIsCodex(sid)) {
    showControlToast('error', 'Thread actions are not available for this session');
    return;
  }
  const actionState = codexThreadActionStateForSession(sid, op);
  if (!actionState.allowed) {
    showControlToast('error', `/${op} is not available here: ${actionState.reason}`);
    return;
  }
  const params = await promptCodexThreadActionParams(spec);
  if (params === null) return;
  if (dispatchCodexThreadAction(op, params, sid)) {
    markActionPending(op);
  }
}

function appendSessionWindowActionMenuItem(parent, action, codex = false) {
  if (!parent || !action) return null;
  if (Array.isArray(action.children) && action.children.length > 0) {
    const submenu = document.createElement('div');
    submenu.className = 'session-window-submenu';
    if (codex) submenu.dataset.sessionWindowCodexSubmenu = '1';
    const trigger = document.createElement('button');
    trigger.type = 'button';
    trigger.className = 'session-window-submenu-trigger';
    trigger.textContent = action.label;
    trigger.title = action.title || action.label;
    trigger.dataset.sessionWindowSubmenuTrigger = '1';
    trigger.setAttribute('role', 'menuitem');
    trigger.setAttribute('aria-haspopup', 'menu');
    trigger.setAttribute('aria-expanded', 'false');
    const panel = document.createElement('div');
    panel.className = 'session-window-submenu-panel';
    panel.setAttribute('role', 'menu');
    action.children.forEach(child => appendSessionWindowActionMenuItem(panel, child, codex));
    submenu.appendChild(trigger);
    submenu.appendChild(panel);
    parent.appendChild(submenu);
    return submenu;
  }

  const item = document.createElement('button');
  item.type = 'button';
  item.textContent = action.label;
  item.title = action.title;
  item.dataset.sessionWindowAction = action.op;
  if (action.danger) item.classList.add('danger');
  if (codex) item.dataset.sessionWindowCodexAction = '1';
  else item.dataset.sessionWindowGenericAction = '1';
  item.setAttribute('role', 'menuitem');
  parent.appendChild(item);
  return item;
}

function ensureSessionWindow(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  meta = { ...(sessionMetadataById.get(sid) || {}), ...normalizeSessionWindowMeta(meta) };
  let win = sessionWindows.get(sid);
  if (win) {
    updateSessionWindow(sid, meta);
    return win;
  }

  const grid = document.getElementById('session-window-grid');
  if (!grid) return null;
  grid.classList.remove('hidden');
  applySessionWindowGridHeight();

  const el = document.createElement('div');
  el.className = 'session-window';
  el.dataset.sessionId = sid;
  el.tabIndex = 0;
  applySessionBadgeStyle(el, sid);

  const header = document.createElement('div');
  header.className = 'session-window-header';
  const title = document.createElement('div');
  title.className = 'session-window-title';
  const id = document.createElement('div');
  id.className = 'session-window-id';
  renderSessionIdentity(id, sid, { order: 'id-name' });
  applySessionBadgeStyle(id, sid);
  const relationStrip = document.createElement('div');
  relationStrip.className = 'session-window-relation-strip hidden';
  const project = document.createElement('div');
  project.className = 'session-window-project';
  setSessionWindowPathElement(
    project,
    meta.projectRoot,
    meta.projectLabel,
    'unknown',
    'Project directory not known yet'
  );
  const cwd = document.createElement('div');
  cwd.className = 'session-window-cwd';
  setSessionWindowPathElement(
    cwd,
    meta.cwd,
    meta.cwdLabel,
    'unknown',
    'Working directory not known yet'
  );
  const metaRow = document.createElement('div');
  metaRow.className = 'session-window-meta';
  metaRow.appendChild(id);
  metaRow.appendChild(relationStrip);
  metaRow.appendChild(project);
  const task = document.createElement('div');
  task.className = 'session-window-task';
  task.textContent = meta.task || 'initial message pending';
  task.title = meta.task || 'Initial user message not known yet';
  title.appendChild(metaRow);
  title.appendChild(cwd);
  title.appendChild(task);
  const status = document.createElement('span');
  status.className = 'session-window-status';
  status.textContent = normalizeSessionPhase(meta.phase || 'idle');
  const goal = document.createElement('span');
  goal.className = 'session-window-goal hidden';
  const goalText = document.createElement('span');
  goalText.className = 'session-window-goal-text';
  goal.appendChild(goalText);
  const tier = document.createElement('span');
  tier.className = 'session-window-tier hidden';
  const vitals = document.createElement('span');
  vitals.className = 'session-window-vitals hidden';
  header.title = 'Click header to collapse or expand';
  const menuControls = document.createElement('div');
  menuControls.className = 'session-window-action-cluster session-window-menu-cluster';
  const windowControls = document.createElement('div');
  windowControls.className = 'session-window-action-cluster session-window-state-cluster';
  const minimize = document.createElement('button');
  minimize.className = 'session-window-action session-window-minimize';
  minimize.type = 'button';
  minimize.title = 'Minimize window';
  minimize.setAttribute('aria-label', minimize.title);
  minimize.setAttribute('aria-pressed', 'false');
  minimize.textContent = '\u2212';
  const actions = document.createElement('button');
  actions.className = 'session-window-action session-window-actions';
  actions.type = 'button';
  actions.title = 'Session actions';
  actions.setAttribute('aria-label', actions.title);
  actions.setAttribute('aria-haspopup', 'menu');
  actions.setAttribute('aria-expanded', 'false');
  actions.textContent = '\u22EF';
  const actionMenu = document.createElement('div');
  actionMenu.className = 'session-window-menu hidden';
  actionMenu.setAttribute('role', 'menu');
  SESSION_WINDOW_GENERIC_ACTIONS.forEach(action => {
    appendSessionWindowActionMenuItem(actionMenu, action, false);
  });
  const codexSeparator = document.createElement('div');
  codexSeparator.className = 'session-window-menu-separator';
  codexSeparator.dataset.sessionWindowCodexSeparator = '1';
  codexSeparator.setAttribute('role', 'separator');
  actionMenu.appendChild(codexSeparator);
  SESSION_WINDOW_CODEX_ACTIONS.forEach(action => {
    appendSessionWindowActionMenuItem(actionMenu, action, true);
  });
  const maximize = document.createElement('button');
  maximize.className = 'session-window-action session-window-maximize';
  maximize.type = 'button';
  maximize.title = 'Maximize window';
  maximize.setAttribute('aria-label', maximize.title);
  maximize.setAttribute('aria-pressed', 'false');
  maximize.textContent = '\u26F6';
  const close = document.createElement('button');
  close.className = 'session-window-action session-window-close';
  close.type = 'button';
  close.title = 'Hide or stop session';
  close.setAttribute('aria-label', close.title);
  close.textContent = '\u00d7';
  menuControls.appendChild(actions);
  menuControls.appendChild(actionMenu);
  windowControls.appendChild(minimize);
  windowControls.appendChild(maximize);
  windowControls.appendChild(close);
  header.appendChild(title);
  header.appendChild(goal);
  header.appendChild(tier);
  header.appendChild(vitals);
  header.appendChild(status);
  header.appendChild(menuControls);
  header.appendChild(windowControls);

  const log = document.createElement('div');
  log.className = 'session-window-log';
  log.innerHTML = '<div class="session-window-empty">Waiting for events...</div>';
  const jumpBottom = document.createElement('button');
  jumpBottom.className = 'session-window-jump-bottom hidden';
  jumpBottom.type = 'button';
  jumpBottom.title = 'Jump to latest output';
  jumpBottom.setAttribute('aria-label', jumpBottom.title);
  jumpBottom.setAttribute('aria-hidden', 'true');
  jumpBottom.textContent = '\u2193';

  el.appendChild(header);
  el.appendChild(log);
  el.appendChild(jumpBottom);
  el.addEventListener('mousedown', () => focusSessionWindow(sid));
  el.addEventListener('focus', () => focusSessionWindow(sid));
  log.addEventListener('scroll', () => updateSessionWindowFollowFromScroll(sessionWindows.get(sid)));
  jumpBottom.addEventListener('click', (e) => {
    e.preventDefault();
    e.stopPropagation();
    focusSessionWindow(sid);
    scrollSessionWindowToBottom(sessionWindows.get(sid));
  });
  header.addEventListener('click', (e) => {
    if (e.target.closest?.('button, a, input, textarea, select, [role="menu"]')) return;
    if (sessionWindows.get(sid)?.minimized) return;
    e.preventDefault();
    e.stopPropagation();
    closeSessionWindowMenus();
    toggleSessionWindowHeaderCollapsed(sid);
  });
  actions.addEventListener('mousedown', (e) => e.stopPropagation());
  actions.addEventListener('click', (e) => {
    e.preventDefault();
    e.stopPropagation();
    focusSessionWindow(sid);
    toggleSessionWindowMenu(sid);
  });
  actionMenu.addEventListener('mousedown', (e) => e.stopPropagation());
  actionMenu.addEventListener('click', (e) => {
    const trigger = e.target.closest?.('[data-session-window-submenu-trigger]');
    if (trigger) {
      e.preventDefault();
      e.stopPropagation();
      const submenu = trigger.closest('.session-window-submenu');
      if (!submenu || trigger.disabled) return;
      const nextOpen = !submenu.classList.contains('open');
      actionMenu.querySelectorAll('.session-window-submenu.open').forEach(openSubmenu => {
        if (openSubmenu === submenu) return;
        openSubmenu.classList.remove('open');
        openSubmenu.querySelector('[data-session-window-submenu-trigger]')?.setAttribute('aria-expanded', 'false');
      });
      submenu.classList.toggle('open', nextOpen);
      trigger.setAttribute('aria-expanded', nextOpen ? 'true' : 'false');
      return;
    }
    const item = e.target.closest?.('[data-session-window-action]');
    if (!item) return;
    e.preventDefault();
    e.stopPropagation();
    setSessionWindowMenuOpen(sid, false);
    runSessionWindowAction(sid, item.dataset.sessionWindowAction);
  });
  minimize.addEventListener('click', (e) => {
    e.stopPropagation();
    closeSessionWindowMenus();
    toggleSessionWindowMinimized(sid);
  });
  maximize.addEventListener('click', (e) => {
    e.stopPropagation();
    closeSessionWindowMenus();
    toggleSessionWindowMaximized(sid);
  });
  close.addEventListener('click', (e) => {
    e.stopPropagation();
    setSessionWindowMenuOpen(sid, false);
    chooseSessionWindowCloseAction(sid);
  });

  grid.appendChild(el);
  const startHeaderCollapsed = sessionWindowShouldStartHeaderCollapsed(meta);
  win = {
    sessionId: sid,
    el,
    log,
    id,
    relationStrip,
    project,
    cwd,
    task,
    status,
    goal,
    goalText,
    tier,
    vitals,
    source: meta.source || '',
    actionMenuButton: actions,
    actionMenu,
    header,
    minimize,
    maximize,
    jumpBottom,
    phase: normalizeSessionPhase(meta.phase || 'idle'),
    pendingActiveUntil: 0,
    ended: false,
    minimized: false,
    followOutput: true,
    pendingOutput: false,
    logHistory: [],
    renderStart: 0,
    renderEnd: 0,
    headerCollapsed: startHeaderCollapsed,
  };
  sessionWindows.set(sid, win);
  updateSessionWindowHeaderCollapseState(sid);
  syncSessionWindowMetadataRefresh();
  updateSessionWindow(sid, meta);
  updateSessionWindowMaximizeState();
  applySessionWindowGridHeight();
  scheduleSessionRelationshipRender();
  return win;
}

function updateSessionWindow(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const win = sessionWindows.get(sid);
  if (!win) return;
  const merged = mergeSessionWindowMetadata(sid, meta);
  meta = merged.meta;
  updateSessionWindowRelationshipStyle(sid);
  const signature = sessionWindowMetadataSignature(meta);
  if (win.metadataSignature === signature) {
    persistSessionWindowState();
    return;
  }
  win.metadataSignature = signature;
  renderSessionIdentity(win.id, sid, { order: 'id-name' });
  if (meta.projectRoot) {
    setSessionWindowPathElement(
      win.project,
      meta.projectRoot,
      meta.projectLabel || compactPathLabel(meta.projectRoot, true),
      'unknown',
      'Project directory not known yet'
    );
  } else {
    setSessionWindowPathElement(
      win.project,
      '',
      '',
      'unknown',
      'Project directory not known yet'
    );
  }
  if (meta.cwd) {
    setSessionWindowPathElement(
      win.cwd,
      meta.cwd,
      meta.cwdLabel || compactPathLabel(meta.cwd, true),
      'unknown',
      'Working directory not known yet'
    );
  } else {
    setSessionWindowPathElement(
      win.cwd,
      '',
      '',
      'unknown',
      'Working directory not known yet'
    );
  }
  refreshSessionWindowPathLabels(win);
  if (meta.task) {
    win.task.textContent = meta.task;
    win.task.title = meta.task;
  }
  if (meta.source) {
    win.source = meta.source;
  }
  if (meta.ended !== undefined) {
    win.ended = !!meta.ended;
    if (win.ended) win.pendingActiveUntil = 0;
  }
  if (meta.phase) {
    const nextPhase = normalizeSessionPhase(meta.phase);
    // A real phase transition is fresh evidence about the session; it
    // retires the "optimistic thinking expired" steer demotion below.
    if (win.phase !== nextPhase) win.optimisticActiveExpired = false;
    win.phase = nextPhase;
    if (win.phase === 'idle' || win.phase === 'done' || win.phase === 'interrupted') {
      win.pendingActiveUntil = 0;
    }
    win.status.textContent = win.phase;
    win.status.className = 'session-window-status';
    const cls = sessionPhaseClass(win.phase);
    if (cls) win.status.classList.add(cls);
  }
  renderSessionWindowGoal(win, meta.goal || null);
  renderSessionWindowTier(win);
  renderSessionWindowVitals(win, (sessionMetadataById.get(sid) || {}).vitals || meta.vitals || null);
  refreshSessionGoalTicker();
  updateSessionWindowActionMenuVisibility(sid);
  updateControlFastButtonState();
  updateSessionRelationshipBadges(sid);
  if (meta.parentId) updateSessionRelationshipBadges(meta.parentId);
  scheduleSessionRelationshipRender();
  persistSessionWindowState();
}

function updateSessionWindowMinimizeState(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  const minimized = !!win.minimized;
  win.el.classList.toggle('minimized', minimized);
  if (win.minimize) {
    win.minimize.textContent = minimized ? '\u25A1' : '\u2212';
    win.minimize.title = minimized ? 'Restore window' : 'Minimize window';
    win.minimize.setAttribute('aria-label', win.minimize.title);
    win.minimize.setAttribute('aria-pressed', minimized ? 'true' : 'false');
  }
  updateSessionWindowJumpButton(win);
}

function updateSessionWindowHeaderCollapseState(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  const collapsed = !!win.headerCollapsed;
  win.el.classList.toggle('header-collapsed', collapsed);
  if (win.header) {
    win.header.title = collapsed ? 'Click header to expand' : 'Click header to collapse';
  }
  refreshSessionWindowPathLabels(win);
}

function setSessionWindowHeaderCollapsed(sessionId, collapsed) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  win.headerCollapsed = !!collapsed;
  updateSessionWindowHeaderCollapseState(sid);
  scheduleSessionRelationshipRender();
}

function toggleSessionWindowHeaderCollapsed(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  setSessionWindowHeaderCollapsed(sid, !win.headerCollapsed);
}

function setSessionWindowMinimized(sessionId, minimized) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  win.minimized = !!minimized;
  if (win.minimized && maximizedSessionWindowId === sid) {
    maximizedSessionWindowId = '';
    updateSessionWindowMaximizeState();
  }
  updateSessionWindowMinimizeState(sid);
  refreshSessionWindowPathLabels(win);
  applySessionWindowGridHeight();
  scheduleSessionRelationshipRender();
}

function toggleSessionWindowMinimized(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  setSessionWindowMinimized(sid, !win.minimized);
}

function updateSessionWindowMaximizeState() {
  const grid = document.getElementById('session-window-grid');
  if (!grid) return;
  const sid = maximizedSessionWindowId && sessionWindows.has(maximizedSessionWindowId)
    ? maximizedSessionWindowId
    : '';
  if (!sid) maximizedSessionWindowId = '';
  grid.classList.toggle('maximized', !!sid);
  for (const [id, win] of sessionWindows) {
    const maximized = id === sid;
    win.el.classList.toggle('maximized', maximized);
    if (win.maximize) {
      win.maximize.textContent = maximized ? '\u2750' : '\u26F6';
      win.maximize.title = maximized ? 'Restore window' : 'Maximize window';
      win.maximize.setAttribute('aria-label', win.maximize.title);
      win.maximize.setAttribute('aria-pressed', maximized ? 'true' : 'false');
    }
  }
  syncSessionWindowGridControls();
}

function setSessionWindowMaximized(sessionId, maximized) {
  const sid = String(sessionId || '').trim();
  if (maximized) {
    if (!sid || !sessionWindows.has(sid)) return;
    setSessionWindowMinimized(sid, false);
    maximizedSessionWindowId = sid;
    focusSessionWindow(sid);
  } else if (!sid || maximizedSessionWindowId === sid) {
    maximizedSessionWindowId = '';
  }
  updateSessionWindowMaximizeState();
  applySessionWindowGridHeight();
  scheduleSessionRelationshipRender();
}

function toggleSessionWindowMaximized(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  setSessionWindowMaximized(sid, maximizedSessionWindowId !== sid);
}

function shouldDeferSessionWindowEscape() {
  if (annotationMode || clipMode || clipAnnotatingRange || displayPickerVisible) return true;
  const rollbackModal = document.getElementById('rollback-modal');
  const fsPickerModal = document.getElementById('fs-picker-modal');
  const sessionConfigModal = document.getElementById('session-config-modal');
  return (!!rollbackModal && rollbackModal.style.display !== 'none')
    || (!!fsPickerModal && fsPickerModal.style.display !== 'none')
    || (!!sessionConfigModal && sessionConfigModal.style.display !== 'none');
}

function focusSessionWindow(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const win = ensureSessionWindow(sid);
  if (!win) return;
  foregroundSessionFullId = sid;
  currentSessionFullId = sid;
  if (app && typeof app.select_session === 'function') {
    const cmds = app.select_session(sid);
    if (cmds) processCommands(cmds);
  }
  for (const [id, other] of sessionWindows) {
    other.el.classList.toggle('foreground', id === sid);
  }
  scheduleSessionRelationshipRender();
  updateTaskTargetChip();
  renderForegroundSessionUsage();
  renderContextPane();
  if (win.phase) setPhase(win.phase);
  hydrateSessionWindowIfEmpty(sid);
}

window.focusForegroundSessionWindow = function() {
  const sid = resolvePromptTargetSessionId();
  if (!sid) return;
  const win = sessionWindows.get(sid);
  if (win) {
    win.el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    win.el.focus();
  }
};

document.addEventListener('click', () => {
  closeSessionWindowMenus();
});

document.getElementById('session-window-grid-resize-handle')?.addEventListener('pointerdown', startSessionWindowGridResize);
document.getElementById('session-window-grid-resize-handle')?.addEventListener('pointermove', updateSessionWindowGridResize);
document.getElementById('session-window-grid-resize-handle')?.addEventListener('pointerup', endSessionWindowGridResize);
document.getElementById('session-window-grid-resize-handle')?.addEventListener('pointercancel', endSessionWindowGridResize);
document.getElementById('session-window-grid-resize-handle')?.addEventListener('dblclick', resetSessionWindowGridHeight);
document.getElementById('concurrent-log-resize')?.addEventListener('click', (e) => {
  e.preventDefault();
  e.stopPropagation();
  toggleConcurrentLogFitToSessionWindows();
});
document.getElementById('concurrent-log-minimize')?.addEventListener('click', (e) => {
  e.preventDefault();
  e.stopPropagation();
  setConcurrentLogMode(
    concurrentLogMode === CONCURRENT_LOG_MODE_MINIMIZED
      ? CONCURRENT_LOG_MODE_NORMAL
      : CONCURRENT_LOG_MODE_MINIMIZED
  );
});
document.getElementById('concurrent-log-maximize')?.addEventListener('click', (e) => {
  e.preventDefault();
  e.stopPropagation();
  setConcurrentLogMode(
    concurrentLogMode === CONCURRENT_LOG_MODE_MAXIMIZED
      ? CONCURRENT_LOG_MODE_NORMAL
      : CONCURRENT_LOG_MODE_MAXIMIZED
  );
});
window.addEventListener('resize', () => {
  applySessionWindowGridHeight();
  scheduleSessionRelationshipRender();
});

function createLogReplayAppendBatch() {
  return {
    mainFragment: document.createDocumentFragment(),
    sessionFragments: new Map(),
    added: 0,
  };
}

function flushLogReplayAppendBatch() {
  const batch = logReplayAppendBatch;
  if (!batch) return;

  let hasSessionItems = false;
  for (const state of batch.sessionFragments.values()) {
    if (state?.items?.length > 0) {
      hasSessionItems = true;
      break;
    }
  }
  const hasMainItems = batch.mainFragment.childNodes.length > 0;
  if (!hasMainItems && !hasSessionItems) return;

  const stream = currentMainLogContainer();
  if (stream && hasMainItems) {
    stream.appendChild(batch.mainFragment);
    pruneMainLogContainer(stream);
    const liveStream = document.getElementById('log-stream');
    if (!concurrentLogDetachedFragment && autoScroll && liveStream) {
      liveStream.scrollTop = liveStream.scrollHeight;
    }
    batch.mainFragment = document.createDocumentFragment();
  }

  for (const state of batch.sessionFragments.values()) {
    if (!state.win || !state.win.log || !state.items || state.items.length === 0) continue;
    appendSessionWindowHistoryBatch(state.win, state.items, state.shouldFollow);
  }
  batch.sessionFragments.clear();
  batch.added = 0;
}

function sessionWindowRecordFromLogCommand(c) {
  const sid = String(c?.session_id || c?.sessionId || '').trim();
  if (!sid) return null;
  const content = c?.content ?? '';
  if (!content && c?.kind !== 'rollback_marker') return null;
  return {
    session_id: sid,
    ts: c?.ts || c?.timestamp || '',
    ts_ms: c?.ts_ms ?? c?.tsMs,
    event_id: c?.event_id || c?.eventId || '',
    delivery: c?.delivery || c?.delivery_class || c?.deliveryClass || '',
    level: c?.level || 'info',
    source: c?.source || c?.event || c?.level || 'system',
    content,
    kind: c?.kind || '',
    turn_id: c?.turn_id || c?.turnId || '',
    item_type: c?.item_type || c?.itemType || '',
    command_item_id: c?.command_item_id || c?.commandItemId || c?.command_execution?.id || c?.commandExecution?.id || '',
    thread_item: c?.thread_item || c?.threadItem || null,
    thread_history_change: c?.thread_history_change || c?.threadHistoryChange || null,
    changed_items: c?.changed_items || c?.changedItems || [],
    changed_turns: c?.changed_turns || c?.changedTurns || [],
    removed_turn_ids: c?.removed_turn_ids || c?.removedTurnIds || [],
    command_execution: c?.command_execution || c?.commandExecution || null,
    output_id: c?.output_id || c?.outputId || '',
    output_session_id: c?.output_session_id || c?.outputSessionId || sid,
    output_source: c?.output_source || c?.outputSource || c?.source || '',
    full_output_available: !!(c?.full_output_available ?? c?.fullOutputAvailable),
    full_output_bytes: c?.full_output_bytes ?? c?.fullOutputBytes,
    full_output_lines: c?.full_output_lines ?? c?.fullOutputLines,
    text_truncated: !!(c?.text_truncated ?? c?.textTruncated),
    truncated_fields: c?.truncated_fields || c?.truncatedFields || [],
    item_id: c?.item_id || c?.itemId || '',
    user_turn_index: c?.user_turn_index ?? c?.userTurnIndex,
    user_turn_revision: c?.user_turn_revision ?? c?.userTurnRevision,
    replacement_for_user_turn_index: c?.replacement_for_user_turn_index ?? c?.replacementForUserTurnIndex,
    superseded: !!c?.superseded,
    superseded_reason: c?.superseded_reason || c?.supersededReason || '',
    collapsible: !!c?.collapsible,
  };
}

function appendSessionWindowRecord(record, batch = null) {
  const sid = String(record?.session_id || record?.sessionId || '').trim();
  if (!sid) return;
  const targetSid = sessionWindowTargetForLogSession(sid);
  if (!targetSid) return;
  const win = sessionWindows.get(targetSid) || (processingLogReplay ? null : ensureSessionWindow(targetSid));
  if (!win) return;
  if (!processingLogReplay && sessionWindowIsDetached(targetSid) && externalSourceForSessionWindow(targetSid, win)) {
    clearStaleSessionWindowDetached(targetSid, 'live output observed');
  }
  const normalized = { ...record, session_id: targetSid };
  const shouldFollow = sessionWindowShouldFollowNextOutput(win);
  if (batch) {
    let state = batch.sessionFragments.get(targetSid);
    if (!state || state.win !== win) {
      state = { win, items: [], shouldFollow };
      batch.sessionFragments.set(targetSid, state);
    }
    state.items.push(normalized);
    return;
  }
  appendSessionWindowHistory(win, normalized, shouldFollow);
}

function appendLogCommandToSessionWindow(c, batch = null) {
  appendSessionWindowRecord(sessionWindowRecordFromLogCommand(c), batch);
}

function appendLogEntryToSessionWindow(entry, batch = null) {
  const sid = entry?.dataset?.sessionId || '';
  if (!sid) return;
  const targetSid = sessionWindowTargetForLogSession(sid);
  if (!targetSid) return;
  const win = ensureSessionWindow(targetSid);
  if (!win) return;
  if (!processingLogReplay && sessionWindowIsDetached(targetSid) && externalSourceForSessionWindow(targetSid, win)) {
    clearStaleSessionWindowDetached(targetSid, 'live output observed');
  }
  const empty = win.log.querySelector('.session-window-empty');
  if (empty) empty.remove();
  const shouldFollow = sessionWindowShouldFollowNextOutput(win);
  const clone = entry.cloneNode(true);
  clone.removeAttribute('id');
  clone.querySelectorAll('[id]').forEach(el => el.removeAttribute('id'));
  if (targetSid !== sid) retargetSessionWindowLogEntry(clone, targetSid);
  wireSessionWindowLogClone(clone, entry);
  if (batch) {
    let state = batch.sessionFragments.get(targetSid);
    if (!state || state.win !== win) {
      state = { win, items: [], shouldFollow };
      batch.sessionFragments.set(targetSid, state);
    }
    state.items.push(clone);
    return;
  }
  appendSessionWindowHistory(win, clone, shouldFollow);
}

function inferSessionPhaseFromLog(c) {
  const sid = sessionWindowTargetForLogSession(c && c.session_id);
  if (!sid) return;
  const content = String(c.content || '');
  const level = String(c.level || '').toLowerCase();
  let phase = '';
  if (content.startsWith('Turn ')) phase = 'thinking';
  else if (!processingLogReplay && level === 'model') phase = 'thinking';
  else if (level === 'agent' || content.startsWith('Running on display')) phase = 'running';
  else if (content.startsWith('Approval required') || content.startsWith('Question:')) phase = 'waiting';
  else if (content.startsWith('Task complete:')) phase = 'done';
  else if (content.startsWith('Round ') && content.includes(' complete')) phase = 'idle';
  else if (content.startsWith('Agent interrupted:')) phase = 'interrupted';
  else if (content.startsWith('Session ended:')) {
    phase = 'done';
    if (
      sid === currentSessionFullId ||
      sid === foregroundSessionFullId ||
      (!hasActiveSessionWindowExcept(sid) && isAgentActivePhase(currentPhase))
    ) {
      setPhase('idle');
    }
  }
  if (phase) {
    updateSessionWindow(sid, { phase });
    if (
      (phase === 'idle' || phase === 'done' || phase === 'interrupted') &&
      (
        sid === resolvePromptTargetSessionId() ||
        sid === currentSessionFullId ||
        sid === foregroundSessionFullId ||
        (!hasActiveSessionWindowExcept(sid) && isAgentActivePhase(currentPhase))
      )
    ) {
      setPhase(phase);
    }
  }
}

function renderInlineMarkdown(text) {
  const codeSpans = [];
  let escaped = escapeHtml(text || '').replace(/`([^`\n]+)`/g, (_, code) => {
    const key = `\u0000CODE${codeSpans.length}\u0000`;
    codeSpans.push(`<code>${code}</code>`);
    return key;
  });

  escaped = escaped
    .replace(
      /\[([^\]\n]+)\]\((?:&lt;([^\n]*?)&gt;|(https?:\/\/[^\s)]+|mailto:[^\s)]+|#[^\s)]+|\/[^\s)]+))\)/g,
      (match, label, bracketedTarget, bareTarget) => {
        const href = String(bracketedTarget || bareTarget || '').trim();
        if (!/^(https?:\/\/|mailto:|#|\/)/.test(href)) return match;
        return `<a href="${href}" target="_blank" rel="noreferrer">${label}</a>`;
      }
    )
    .replace(/\*\*([^*\n][\s\S]*?[^*\n])\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, '$1<em>$2</em>');

  return escaped.replace(/\u0000CODE(\d+)\u0000/g, (_, idx) => codeSpans[Number(idx)] || '');
}

function markdownIndentWidth(raw) {
  let width = 0;
  for (const ch of String(raw || '')) {
    if (ch === '\t') width += 4 - (width % 4);
    else width++;
  }
  return width;
}

function markdownListMarker(line) {
  const m = String(line || '').match(/^([ \t]*)(?:(\d+)\.\s+|([-*+])\s+)(.*)$/);
  if (!m) return null;
  return {
    indent: markdownIndentWidth(m[1]),
    ordered: !!m[2],
    number: m[2] ? Number(m[2]) : null,
    content: m[4] || '',
  };
}

function renderMarkdownList(lines, start) {
  const first = markdownListMarker(lines[start]);
  if (!first) return { html: '', next: start };
  const tag = first.ordered ? 'ol' : 'ul';
  const startAttr = first.ordered && first.number && first.number !== 1
    ? ` start="${first.number}"`
    : '';
  const items = [];
  let i = start;

  while (i < lines.length) {
    const marker = markdownListMarker(lines[i]);
    if (marker) {
      if (marker.indent < first.indent) break;
      if (marker.indent > first.indent) {
        if (!items.length) break;
        const nested = renderMarkdownList(lines, i);
        if (nested.next === i) break;
        items[items.length - 1].push(nested.html);
        i = nested.next;
        continue;
      }
      if (marker.ordered !== first.ordered) break;
      items.push([renderInlineMarkdown(marker.content.trim())]);
      i++;
      continue;
    }

    const line = lines[i];
    if (!line.trim()) break;
    if (/^(#{1,3})\s+/.test(line) || /^\s*>/.test(line)) break;
    if (!items.length) break;
    const indent = markdownIndentWidth((line.match(/^([ \t]*)/) || ['', ''])[1]);
    if (indent <= first.indent) break;
    items[items.length - 1].push(`<br>${renderInlineMarkdown(line.trim())}`);
    i++;
  }

  return {
    html: `<${tag}${startAttr}>` + items.map(parts => `<li>${parts.join('')}</li>`).join('') + `</${tag}>`,
    next: i,
  };
}

function renderMarkdownBlocks(text) {
  const lines = String(text || '').replace(/\r\n?/g, '\n').split('\n');
  let html = '';
  let i = 0;

  const isListLine = line => !!markdownListMarker(line);
  const isBlockStart = line => /^(#{1,3})\s+/.test(line) || isListLine(line) || /^\s*>/.test(line);

  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }

    const heading = line.match(/^(#{1,3})\s+(.+)$/);
    if (heading) {
      const level = heading[1].length;
      html += `<h${level}>${renderInlineMarkdown(heading[2].trim())}</h${level}>`;
      i++;
      continue;
    }

    if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) {
        quote.push(lines[i].replace(/^\s*>\s?/, ''));
        i++;
      }
      html += `<blockquote>${renderMarkdownBlocks(quote.join('\n'))}</blockquote>`;
      continue;
    }

    if (isListLine(line)) {
      const list = renderMarkdownList(lines, i);
      html += list.html;
      i = list.next;
      continue;
    }

    const para = [];
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines[i])) {
      para.push(lines[i]);
      i++;
    }
    html += `<p>${renderInlineMarkdown(para.join('\n')).replace(/\n/g, '<br>')}</p>`;
  }

  return html;
}

function renderMarkdown(text) {
  const src = String(text || '').replace(/\r\n?/g, '\n');
  const parts = [];
  let cursor = 0;
  const fenceRe = /```([a-zA-Z0-9_-]*)\n([\s\S]*?)```/g;
  let match;
  while ((match = fenceRe.exec(src)) !== null) {
    if (match.index > cursor) {
      parts.push(renderMarkdownBlocks(src.slice(cursor, match.index)));
    }
    const lang = (match[1] || '').toLowerCase();
    const code = match[2].replace(/\n$/, '');
    const renderedCode = /^(sh|shell|bash|zsh|console)$/.test(lang)
      ? highlightShellCommand(code)
      : escapeHtml(code);
    parts.push(`<pre><code>${renderedCode}</code></pre>`);
    cursor = fenceRe.lastIndex;
  }
  if (cursor < src.length) {
    parts.push(renderMarkdownBlocks(src.slice(cursor)));
  }
  return parts.join('');
}

function detectCommandLog(content) {
  const text = String(content || '').trim();
  const patterns = [
    /^(Auto-approved:\s*)?(exec|pty):\s*([\s\S]+)$/i,
    /^(Approval required:\s*)(exec|pty):\s*([\s\S]+)$/i,
    /^(command:\s*)([\s\S]+)$/i,
    /^(Run command:\s*)([\s\S]+)$/i,
  ];
  for (const pattern of patterns) {
    const m = text.match(pattern);
    if (!m) continue;
    if (pattern === patterns[0]) {
      return { label: `${m[1] || ''}${m[2]}`.trim(), command: m[3] };
    }
    if (pattern === patterns[1]) {
      return { label: `${m[1]}${m[2]}`.trim(), command: m[3] };
    }
    return { label: m[1].replace(/:\s*$/, ''), command: m[2] };
  }
  return null;
}

function detectCommandFailureLog(content) {
  const text = String(content || '').trim();
  const m = text.match(/^(Command failed(?:\s*\([^)]+\))?:\s*[^\n]+)\nCommand:\s*([\s\S]+)$/i);
  if (!m) return null;
  return { label: m[1], command: m[2] };
}

function highlightShellCommand(command) {
  const src = String(command || '');
  let html = '';
  let i = 0;
  let expectCommand = true;

  function span(cls, value) {
    return `<span class="${cls}">${escapeHtml(value)}</span>`;
  }

  while (i < src.length) {
    const ch = src[i];
    if (/\s/.test(ch)) {
      html += escapeHtml(ch);
      i++;
      continue;
    }
    if (ch === '#') {
      const end = src.indexOf('\n', i);
      const comment = end === -1 ? src.slice(i) : src.slice(i, end);
      html += span('syntax-comment', comment);
      i += comment.length;
      continue;
    }
    if (ch === '"' || ch === "'") {
      const quote = ch;
      let j = i + 1;
      while (j < src.length) {
        if (src[j] === '\\') { j += 2; continue; }
        if (src[j] === quote) { j++; break; }
        j++;
      }
      html += span('syntax-string', src.slice(i, j));
      i = j;
      expectCommand = false;
      continue;
    }
    if ('|&;()<>'.includes(ch)) {
      html += span('syntax-op', ch);
      expectCommand = !')<>'.includes(ch);
      i++;
      continue;
    }

    let j = i;
    while (j < src.length && !/\s/.test(src[j]) && !'|&;()<>"\''.includes(src[j])) j++;
    const token = src.slice(i, j);
    let cls = '';
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(token) || /^\$[{A-Za-z_]/.test(token)) cls = 'syntax-var';
    else if (/^--?[A-Za-z0-9][\w-]*(?:=.*)?$/.test(token)) cls = 'syntax-flag';
    else if (/^(?:\.{0,2}\/|~\/|\/|[A-Za-z0-9_.-]+\/)/.test(token)) cls = 'syntax-path';
    else if (expectCommand) cls = 'syntax-cmd';
    html += cls ? span(cls, token) : escapeHtml(token);
    expectCommand = false;
    i = j;
  }
  return html;
}

function stripAnsi(text) {
  return String(text || '').replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '');
}

function highlightCommandOutput(text) {
  const src = stripAnsi(text);
  if (!src) return '';
  return src.split('\n').map((line) => {
    let cls = 'output-line';
    if (/^\s*(error|failed|panic|fatal)\b/i.test(line) || /\b(ERROR|Error:|failed|panic)\b/.test(line)) {
      cls += ' output-line-error';
    } else if (/^\s*(warn|warning)\b/i.test(line) || /\b(WARN|warning)\b/i.test(line)) {
      cls += ' output-line-warn';
    } else if (/^\s*(ok|pass|passed|success|done)\b/i.test(line)) {
      cls += ' output-line-success';
    } else if (/^\+/.test(line) && !/^\+\+\+/.test(line)) {
      cls += ' output-line-diff-add';
    } else if (/^-/.test(line) && !/^---/.test(line)) {
      cls += ' output-line-diff-del';
    } else if (/^\s*(\$|>|#)\s/.test(line)) {
      cls += ' output-line-prompt';
    }

    let html = escapeHtml(line);
    html = html.replace(/(&quot;[A-Za-z0-9_.-]+&quot;)(\s*:)/g, '<span class="output-json-key">$1</span>$2');
    html = html.replace(/\b(https?:\/\/[^\s<>"']+)/g, '<span class="output-url">$1</span>');
    html = html.replace(/(^|[\s([,{])((?:~|\.{1,2}|\/)[A-Za-z0-9_./:@%+=,-]+)/g, '$1<span class="output-path">$2</span>');
    html = html.replace(/\b(\d+(?:\.\d+)?(?:ms|s|KiB|MiB|KB|MB|GB|%)?)\b/g, '<span class="output-number">$1</span>');
    return `<span class="${cls}">${html || ' '}</span>`;
  }).join('');
}

function commandOutputStats(text) {
  const s = String(text || '');
  if (!s) return { lines: 0, bytes: 0 };
  let newlines = 0;
  for (let i = 0; i < s.length; i++) {
    if (s.charCodeAt(i) === 10) newlines += 1;
  }
  const lines = s.endsWith('\n') ? newlines : newlines + 1;
  return { lines, bytes: utf8ByteLength(s) };
}

function formatCompactBytes(bytes) {
  const n = Number(bytes || 0);
  if (n >= 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + ' MB';
  if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
  return n + ' B';
}

function isCommandOutputLog(c) {
  if (!c) return false;
  if (
    c.kind === 'agent_output' ||
    c.kind === 'command_execution' ||
    c.item_type === 'command_execution' ||
    c.itemType === 'command_execution' ||
    c.command_execution ||
    c.commandExecution ||
    c.output_id
  ) return true;
  const level = String(c.level || '').toLowerCase();
  if (level !== 'agent' && level !== 'warn') return false;
  if (detectCommandLog(c.content)) return false;
  if (shouldRenderMarkdownLog(c)) return false;
  return level === 'agent';
}

function padTimestampPart(value, width = 2) {
  return String(Math.trunc(Math.abs(Number(value) || 0))).padStart(width, '0');
}

function parseTimestampWithExplicitZone(value) {
  const raw = String(value || '').trim();
  if (!raw) return null;
  const match = raw.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?)(Z|[+-]\d{2}:?\d{2})$/);
  if (!match) return null;
  const zone = match[3] === 'Z'
    ? 'Z'
    : match[3].replace(/^([+-]\d{2})(\d{2})$/, '$1:$2');
  const date = new Date(`${match[1]}T${match[2]}${zone}`);
  return Number.isNaN(date.getTime()) ? null : date;
}

function localTimestampOffsetLabel(date) {
  const offset = -date.getTimezoneOffset();
  const sign = offset >= 0 ? '+' : '-';
  const abs = Math.abs(offset);
  return `UTC${sign}${padTimestampPart(Math.floor(abs / 60))}:${padTimestampPart(abs % 60)}`;
}

function formatLocalTimestamp(date, options = {}) {
  const datePart = options.includeYear
    ? `${date.getFullYear()}-${padTimestampPart(date.getMonth() + 1)}-${padTimestampPart(date.getDate())}`
    : `${padTimestampPart(date.getMonth() + 1)}-${padTimestampPart(date.getDate())}`;
  let timePart = `${padTimestampPart(date.getHours())}:${padTimestampPart(date.getMinutes())}`;
  if (options.includeSeconds) {
    timePart += `:${padTimestampPart(date.getSeconds())}`;
    if (options.includeMillis && date.getMilliseconds()) {
      timePart += `.${padTimestampPart(date.getMilliseconds(), 3)}`;
    }
  }
  return `${datePart} ${timePart}`;
}

function isSameLocalDate(a, b) {
  return a instanceof Date && b instanceof Date &&
    !Number.isNaN(a.getTime()) && !Number.isNaN(b.getTime()) &&
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate();
}

function formatActivityTimestampDate(date, options = {}) {
  const now = options.now instanceof Date ? options.now : new Date();
  if (isSameLocalDate(date, now)) {
    return `${padTimestampPart(date.getHours())}:${padTimestampPart(date.getMinutes())}:${padTimestampPart(date.getSeconds())}`;
  }
  return formatLocalTimestamp(date, {
    includeYear: date.getFullYear() !== now.getFullYear(),
    includeSeconds: false,
  });
}

function parseTimestampWithoutExplicitZone(value) {
  const raw = String(value || '').trim();
  let match = raw.match(/^(\d{4})-(\d{2})-(\d{2})[T\s](\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?$/);
  if (match) {
    const date = new Date(
      Number(match[1]),
      Number(match[2]) - 1,
      Number(match[3]),
      Number(match[4]),
      Number(match[5]),
      Number(match[6] || 0),
      0
    );
    return Number.isNaN(date.getTime()) ? null : date;
  }
  match = raw.match(/^(\d{1,2})-(\d{1,2})\s+(\d{1,2}):(\d{2})(?::(\d{2}))?$/);
  if (match) {
    const now = new Date();
    const date = new Date(
      now.getFullYear(),
      Number(match[1]) - 1,
      Number(match[2]),
      Number(match[3]),
      Number(match[4]),
      Number(match[5] || 0),
      0
    );
    return Number.isNaN(date.getTime()) ? null : date;
  }
  return null;
}

function formatLogTimestampLabel(value) {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return formatActivityTimestampDate(new Date(value));
  }
  const raw = String(value || '').trim();
  if (!raw) return '';
  if (/^\d+$/.test(raw)) {
    const numeric = Number(raw);
    if (Number.isFinite(numeric) && numeric > 0) {
      return formatActivityTimestampDate(new Date(numeric));
    }
  }
  const zoned = parseTimestampWithExplicitZone(raw);
  if (zoned) return formatActivityTimestampDate(zoned);
  const local = parseTimestampWithoutExplicitZone(raw);
  if (local) return formatActivityTimestampDate(local);
  const time = raw.match(/^(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?$/);
  if (time) return `${time[1]}:${time[2]}:${time[3]}`;
  if (/^\d{4}-\d{2}-\d{2}$/.test(raw)) return raw;
  return raw.length > 12 ? raw.substring(0, 12) : raw;
}

function formatLogTimestampTitle(value, rawValue = '') {
  if (typeof value === 'number' && Number.isFinite(value)) {
    const date = new Date(value);
    const raw = String(rawValue || '').trim();
    const title = `${formatLocalTimestamp(date, { includeYear: true, includeSeconds: true, includeMillis: true })} ${localTimestampOffsetLabel(date)}`;
    return raw ? `${title} (${raw})` : title;
  }
  const raw = String(value || '').trim();
  if (/^\d+$/.test(raw)) {
    const numeric = Number(raw);
    if (Number.isFinite(numeric) && numeric > 0) {
      const date = new Date(numeric);
      return `${formatLocalTimestamp(date, { includeYear: true, includeSeconds: true, includeMillis: true })} ${localTimestampOffsetLabel(date)}`;
    }
  }
  const zoned = parseTimestampWithExplicitZone(raw);
  if (zoned) {
    return `${formatLocalTimestamp(zoned, { includeYear: true, includeSeconds: true, includeMillis: true })} ${localTimestampOffsetLabel(zoned)} (${raw})`;
  }
  return raw;
}

function createLogScaffold(c, extraClass) {
  const entry = document.createElement('div');
  entry.className = 'log-entry level-' + c.level + (extraClass ? ' ' + extraClass : '');
  entry.dataset.level = c.level;
  if (c.kind) entry.dataset.kind = c.kind;
  if (c.event_id || c.eventId) entry.dataset.eventId = c.event_id || c.eventId;
  if (c.delivery || c.delivery_class || c.deliveryClass) {
    entry.dataset.delivery = c.delivery || c.delivery_class || c.deliveryClass;
  }
  if (c.ts_ms !== undefined && c.ts_ms !== null) entry.dataset.tsMs = String(c.ts_ms);
  if (c.turn_id || c.turnId) entry.dataset.turnId = c.turn_id || c.turnId;
  if (c.item_type || c.itemType) entry.dataset.itemType = c.item_type || c.itemType;
  if (c.command_item_id || c.commandItemId || c.command_execution?.id || c.commandExecution?.id) {
    entry.dataset.commandItemId = c.command_item_id || c.commandItemId || c.command_execution?.id || c.commandExecution?.id;
  }
  if (c.superseded) {
    entry.classList.add('superseded');
    entry.dataset.superseded = 'true';
  }
  if (c.kind === 'rollback_marker') {
    entry.classList.add('rollback-marker');
  }
  if (c.replacement_for_user_turn_index !== undefined && c.replacement_for_user_turn_index !== null) {
    entry.classList.add('replacement-message');
    entry.dataset.replacementForUserTurnIndex = String(c.replacement_for_user_turn_index);
  }
  const sourceClass = String(c.source || '').toLowerCase().replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
  if (sourceClass) entry.classList.add('source-' + sourceClass);

  const hostId = c.host_id || selfPeerId;
  entry.dataset.hostId = hostId;
  entry.dataset.stationEventId = stationLogEventId(c);
  if (activeHostFilter && activeHostFilter !== hostId) {
    entry.classList.add('hidden-by-filter');
  }

  const icon = document.createElement('span');
  icon.className = 'log-icon';
  icon.textContent = LEVEL_ICONS[c.level] || '\u2022';
  icon.title = LEVEL_TOOLTIPS[c.level] || c.level;

  const ts = document.createElement('span');
  ts.className = 'log-ts';
  const timestampValue = c.ts_ms ?? c.tsMs ?? c.ts;
  ts.textContent = formatLogTimestampLabel(timestampValue);
  ts.title = formatLogTimestampTitle(timestampValue, c.ts);

  const host = document.createElement('span');
  host.className = 'log-host';
  host.textContent = hostId;
  host.style.background = hostBadgeColor(hostId);
  host.title = `source host: ${hostId}`;

  const session = document.createElement('span');
  session.className = 'log-session';
  const sid = c.session_id || '';
  renderSessionIdentity(session, sid, { showName: false });
  if (sid) {
    entry.dataset.sessionId = sid;
    applySessionBadgeStyle(session, sid);
  }
  if (c.user_turn_index !== undefined && c.user_turn_index !== null) {
    entry.dataset.userTurnIndex = String(c.user_turn_index);
  }
  if (c.user_turn_revision !== undefined && c.user_turn_revision !== null) {
    entry.dataset.userTurnRevision = String(c.user_turn_revision);
  }
  if (c.item_id) {
    entry.dataset.itemId = c.item_id;
  }
  if (c.output_id) {
    entry.dataset.outputId = c.output_id;
  }

  const lvl = document.createElement('span');
  lvl.className = 'log-level';
  lvl.textContent = c.source || '\u2139';

  entry.appendChild(icon);
  entry.appendChild(ts);
  entry.appendChild(host);
  if (sid) {
    entry.appendChild(session);
    applyPromptTargetLogSessionBadgeState(entry);
  }
  entry.appendChild(lvl);
  if (c.item_id) {
    const anchor = document.createElement('button');
    anchor.type = 'button';
    anchor.className = 'log-anchor-btn';
    anchor.textContent = 'anchor';
    anchor.title = c.item_id;
    anchor.dataset.itemId = c.item_id;
    anchor.dataset.sessionId = sid;
    anchor.addEventListener('click', (ev) => {
      ev.stopPropagation();
      fillManagedContextAnchor(c.item_id, sid);
    });
    entry.appendChild(anchor);
  }
  return { entry, hostId };
}

function appendLogEntryElement(entry, sessionRecord = null) {
  if (logReplayAppendBatch && !entry.classList.contains('command-output-group')) {
    logReplayAppendBatch.mainFragment.appendChild(entry);
    if (sessionRecord) {
      appendSessionWindowRecord(sessionRecord, logReplayAppendBatch);
    } else {
      appendLogEntryToSessionWindow(entry, logReplayAppendBatch);
    }
    logEntryCount++;
    updateLogEmptyState();
    logReplayAppendBatch.added++;
    return;
  }
  if (logReplayAppendBatch) flushLogReplayAppendBatch();

  const stream = document.getElementById('log-stream');
  const container = currentMainLogContainer() || stream;
  if (!container) return;
  container.appendChild(entry);
  if (processingLogReplay) {
    appendSessionWindowRecord(sessionRecord);
  } else {
    appendLogEntryToSessionWindow(entry);
  }
  logEntryCount++;
  updateLogEmptyState();
  pruneMainLogContainer(container);
  if (!concurrentLogDetachedFragment && autoScroll && stream) {
    stream.scrollTop = stream.scrollHeight;
  } else if (!processingLogReplay) {
    noteMainLogNewBelow();
  }
}

function commandOutputCloneViews(group) {
  if (!group || !Array.isArray(group.clones)) return [];
  group.clones = group.clones.filter(view => view.entry?.isConnected || view.entry?.dataset?.sessionWindowHistory === '1');
  return group.clones;
}

function commandOutputCloneViewForEntry(group, entry) {
  if (!group || !entry) return null;
  return commandOutputCloneViews(group).find(view => view.entry === entry) || null;
}

function registerCommandOutputCloneView(group, clone) {
  if (!group || !clone) return null;
  const existing = commandOutputCloneViewForEntry(group, clone);
  if (existing) return existing;

  const view = {
    entry: clone,
    summary: clone.querySelector('.command-output-summary'),
    body: clone.querySelector('.command-output-body'),
    loaded: false,
    loading: false,
  };
  if (!view.summary || !view.body) return null;

  if (!Array.isArray(group.clones)) group.clones = [];
  group.clones.push(view);
  view.summary.innerHTML = group.summary.innerHTML;
  clone.classList.toggle('finalized', group.finalized);
  clone.classList.toggle('expanded', group.entry.classList.contains('expanded'));
  if (group.finalized) {
    view.body.innerHTML = '';
  }
  return view;
}

function commandOutputGroupKey(c) {
  const sid = String(c?.session_id || '').trim();
  const targetSid = sid ? sessionWindowTargetForLogSession(sid) : '';
  const source = String(c?.source || '').trim().toLowerCase();
  const commandItemId = String(
    c?.command_item_id ||
    c?.commandItemId ||
    c?.command_execution?.id ||
    c?.commandExecution?.id ||
    c?.item_id ||
    c?.itemId ||
    ''
  ).trim();
  if (commandItemId) {
    return `${targetSid || sid || 'global'}\u001f${source}\u001f${commandItemId}`;
  }
  return `${targetSid || sid || 'global'}\u001f${source}`;
}

function commandOutputSummaryHtml(group) {
  const parts = [];
  if (group.chunks > 0) parts.push(group.chunks + ' chunk' + (group.chunks === 1 ? '' : 's'));
  if (group.lines > 0) parts.push(group.lines + ' line' + (group.lines === 1 ? '' : 's'));
  if (group.bytes > 0) parts.push(formatCompactBytes(group.bytes));
  if (group.warns > 0) parts.push(`<span class="output-warn">${group.warns} warning${group.warns === 1 ? '' : 's'}</span>`);
  const detail = parts.length ? parts.join(' · ') : 'ready';
  return `<span class="status-dot"></span><span>output · ${detail}</span>`;
}

function updateCommandOutputSummary(group) {
  const html = commandOutputSummaryHtml(group);
  group.summary.innerHTML = html;
  for (const view of commandOutputCloneViews(group)) {
    if (view.summary) view.summary.innerHTML = html;
  }
}

function appendCommandOutputText(body, text) {
  if (!body || !text) return;
  const pre = document.createElement('pre');
  pre.className = 'command-output-pre';
  const code = document.createElement('code');
  code.innerHTML = highlightCommandOutput(text);
  pre.appendChild(code);
  body.appendChild(pre);
}

async function appendCommandOutputTextProgressive(body, text, chunkChars = 65536) {
  if (!body || !text) return;
  const output = String(text || '');
  if (output.length <= chunkChars) {
    appendCommandOutputText(body, output);
    return;
  }
  for (let offset = 0; offset < output.length; offset += chunkChars) {
    appendCommandOutputText(body, output.slice(offset, offset + chunkChars));
    await new Promise(resolve => requestAnimationFrame(resolve));
  }
}

function setDeferredCommandOutputText(entry, body, text, stats = null) {
  if (!entry || !body) return;
  const outputText = String(text || '');
  if (!outputText) return;
  const outputStats = stats || commandOutputStats(outputText);
  if (outputText.length <= COMMAND_OUTPUT_EAGER_RENDER_CHAR_LIMIT) {
    appendCommandOutputText(body, outputText);
    return;
  }
  body.textContent = `Output is ${formatCompactBytes(outputStats.bytes)} across ${outputStats.lines} line${outputStats.lines === 1 ? '' : 's'}; expand to render it.`;
  _deferredCommandOutputStore.set(entry, { body, text: outputText, rendered: false });
}

function renderDeferredCommandOutputText(entry) {
  const state = entry ? _deferredCommandOutputStore.get(entry) : null;
  if (!state || state.rendered || !state.body) return;
  state.rendered = true;
  state.body.innerHTML = '';
  appendCommandOutputText(state.body, state.text);
}

function wireCommandOutputGroupClone(clone, sourceEntry) {
  if (!clone) return;
  const group = sourceEntry ? commandOutputGroupByEntry.get(sourceEntry) : null;
  if (!group) return;
  const view = registerCommandOutputCloneView(group, clone);
  if (!view || _wiredCommandOutputLogEntries.has(clone)) return;

  _wiredCommandOutputLogEntries.add(clone);
  clone.addEventListener('click', () => toggleCommandOutputView(group, view));
}

function wireDiffLogEntry(entry) {
  if (!entry || _wiredDiffLogEntries.has(entry)) return;
  _wiredDiffLogEntries.add(entry);
  entry.addEventListener('click', (event) => {
    if (event.target?.closest?.('a, button')) return;
    entry.classList.toggle('expanded');
  });
}

function commandOutputSessionCloneView(group, win) {
  if (!group || !win) return null;
  const history = ensureSessionWindowHistory(win);
  return commandOutputCloneViews(group).find(view => {
    const entry = view.entry;
    if (!entry) return false;
    if (history.includes(entry)) return true;
    return entry.closest?.('.session-window') === win.el;
  }) || null;
}

function ensureCommandOutputSessionClone(group, c) {
  if (processingLogReplay) return;
  if (!group || !group.entry) return;
  const sid = String(c?.session_id || '').trim();
  if (!sid) return;
  const targetSid = sessionWindowTargetForLogSession(sid);
  if (!targetSid) return;
  const win = ensureSessionWindow(targetSid);
  if (!win) return;
  if (commandOutputSessionCloneView(group, win)) return;

  const history = ensureSessionWindowHistory(win);
  const existing = history.find(entry => entry?.dataset?.outputGroupId === group.id);
  if (existing) {
    if (existing.dataset?.sessionId !== targetSid) retargetSessionWindowLogEntry(existing, targetSid);
    wireSessionWindowLogClone(existing, group.entry);
    return;
  }

  const shouldFollow = sessionWindowShouldFollowNextOutput(win);
  const clone = group.entry.cloneNode(true);
  clone.removeAttribute('id');
  clone.querySelectorAll('[id]').forEach(el => el.removeAttribute('id'));
  if (clone.dataset?.sessionId !== targetSid) retargetSessionWindowLogEntry(clone, targetSid);
  wireSessionWindowLogClone(clone, group.entry);
  appendSessionWindowHistory(win, clone, shouldFollow);
}

function appendCommandOutputChunk(group, c) {
  if (processingLogReplay) {
    appendLogCommandToSessionWindow(c, logReplayAppendBatch);
  } else {
    ensureCommandOutputSessionClone(group, c);
  }
  const text = c.content ?? '';
  const sessionScrollStates = new Map();
  for (const view of commandOutputCloneViews(group)) {
    const win = sessionWindowForLogDescendant(view.entry);
    if (win && !sessionScrollStates.has(win)) {
      sessionScrollStates.set(win, sessionWindowShouldFollowNextOutput(win));
    }
  }
  if (group.copyRef && text) group.copyRef.text += text;
  if (c.output_id && !group.outputIdSet.has(c.output_id)) {
    group.outputIdSet.add(c.output_id);
    group.outputIds.push(c.output_id);
  }
  const stats = commandOutputStats(text);
  group.lines += stats.lines;
  group.bytes += stats.bytes;
  group.chunks += 1;
  if ((c.level || '').toLowerCase() === 'warn') group.warns += 1;
  updateCommandOutputSummary(group);
  if (!group.finalized && text && !processingLogReplay) {
    appendCommandOutputText(group.body, text);
    for (const view of commandOutputCloneViews(group)) {
      if (view.entry.classList.contains('expanded')) {
        appendCommandOutputText(view.body, text);
      }
    }
  }
  for (const [win, shouldFollow] of sessionScrollStates.entries()) {
    applySessionWindowOutputScroll(win, shouldFollow);
  }
}

function renderCommandOutputEntry(c) {
  const groupKey = commandOutputGroupKey(c);
  let activeCommandOutputGroup = activeCommandOutputGroups.get(groupKey);
  if (!activeCommandOutputGroup || activeCommandOutputGroup.finalized) {
    const groupId = 'cmdout-' + (++commandOutputGroupSeq);
    const { entry } = createLogScaffold(c, 'command-output-group expanded');
    entry.dataset.outputGroupId = groupId;
    const wrap = document.createElement('span');
    wrap.className = 'log-content command-output-wrap';
    const summary = document.createElement('span');
    summary.className = 'command-output-summary';
    const body = document.createElement('span');
    body.className = 'command-output-body';
    wrap.appendChild(summary);
    wrap.appendChild(body);
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 output</span><span class="arrow-up">\u25BE hide</span>';
    entry.appendChild(wrap);
    entry.appendChild(toggle);
    const copyRef = setLogEntryCopyText(entry, '');
    appendCopyLogEntryButton(entry);

    const group = {
      id: groupId,
      key: groupKey,
      entry,
      summary,
      body,
      outputIds: [],
      outputIdSet: new Set(),
      chunks: 0,
      lines: 0,
      bytes: 0,
      warns: 0,
      finalized: false,
      loaded: false,
      loading: false,
      clones: [],
      copyRef,
    };
    commandOutputGroupByEntry.set(entry, group);
    entry.addEventListener('click', () => toggleCommandOutputGroup(group));
    commandOutputGroups.set(groupId, group);
    activeCommandOutputGroups.set(groupKey, group);
    activeCommandOutputGroup = group;
    updateCommandOutputSummary(group);
    appendLogEntryElement(entry);
  }
  appendCommandOutputChunk(activeCommandOutputGroup, c);
  const stream = document.getElementById('log-stream');
  if (!concurrentLogDetachedFragment && autoScroll && stream) stream.scrollTop = stream.scrollHeight;
}

function finalizeCommandOutputGroup(group) {
  if (!group || group.finalized) return;
  group.finalized = true;
  group.entry.classList.add('finalized');
  group.entry.classList.remove('expanded');
  group.body.innerHTML = '';
  group.loaded = false;
  group.loading = false;
  for (const view of commandOutputCloneViews(group)) {
    view.entry.classList.add('finalized');
    view.entry.classList.remove('expanded');
    view.body.innerHTML = '';
    view.loaded = false;
    view.loading = false;
  }
  if (group.key) activeCommandOutputGroups.delete(group.key);
}

function finalizeActiveCommandOutputGroup(groupKey = null) {
  if (groupKey !== null && groupKey !== undefined) {
    finalizeCommandOutputGroup(activeCommandOutputGroups.get(groupKey));
    return;
  }
  for (const group of Array.from(activeCommandOutputGroups.values())) {
    finalizeCommandOutputGroup(group);
  }
  activeCommandOutputGroups.clear();
}

async function loadCommandOutputIntoBody(group, body) {
  if (!group.outputIds.length) {
    body.textContent = 'Output is not available for lazy loading.';
    return;
  }
  const payload = { ids: group.outputIds };
  const resp = await dashboardJsonFetch('api_session_current_agent_output', payload, () => authedFetch('/api/session/current/agent-output', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  }), 'api_session_current_agent_output');
  const json = await resp.json();
  if (!resp.ok) throw new Error(json.error || resp.statusText || `HTTP ${resp.status}`);
  body.innerHTML = '';
  for (const out of (json.outputs || [])) {
    const text = [out.stdout || '', out.stderr || ''].filter(Boolean).join(out.stdout && out.stderr ? '\n' : '');
    await appendCommandOutputTextProgressive(body, text);
  }
  if (!body.childElementCount) {
    body.textContent = 'No persisted output found.';
  }
}

async function toggleCommandOutputView(group, view) {
  if (!group.finalized || !view?.entry || !view?.body) return;
  const expanding = !view.entry.classList.contains('expanded');
  view.entry.classList.toggle('expanded', expanding);
  if (!expanding) {
    view.body.innerHTML = '';
    view.loaded = false;
    return;
  }
  if (view.loaded || view.loading) return;
  view.loading = true;
  view.body.textContent = 'Loading output...';
  try {
    await loadCommandOutputIntoBody(group, view.body);
    view.loaded = true;
  } catch (e) {
    view.body.textContent = 'Could not load output: ' + e;
  } finally {
    view.loading = false;
  }
}

async function toggleCommandOutputGroup(group) {
  await toggleCommandOutputView(group, group);
}

function renderDiffLogEntry(c) {
  finalizeActiveCommandOutputGroup(commandOutputGroupKey(c));
  const diffText = diffLogContent(c);
  const parsed = parseUnifiedDiff(diffText);
  const { entry } = createLogScaffold(c, 'diff-log-entry');

  const wrap = document.createElement('span');
  wrap.className = 'log-content diff-log-wrap';
  const summary = document.createElement('span');
  summary.className = 'diff-log-summary';
  summary.innerHTML = diffLogSummaryHtml(parsed);
  const body = document.createElement('span');
  body.className = 'diff-log-body';
  renderDiffLogBody(body, parsed, { sessionId: c.session_id || '' });
  wrap.appendChild(summary);
  wrap.appendChild(body);

  const toggle = document.createElement('span');
  toggle.className = 'collapse-toggle';
  toggle.innerHTML = '<span class="arrow">\u25B8 diff</span><span class="arrow-up">\u25BE hide</span>';
  entry.appendChild(wrap);
  entry.appendChild(toggle);
  appendCopyLogEntryButton(entry, diffText);
  wireDiffLogEntry(entry);
  appendLogEntryElement(entry, sessionWindowRecordFromLogCommand(c));
}

function shouldRenderMarkdownLog(c) {
  const level = (c.level || '').toLowerCase();
  const source = (c.source || '').toLowerCase();
  if (level === 'model' || level === 'subagent' || level === 'presence') return true;
  if (source === 'user' || source === 'model' || source === 'prsnc' || source === 'live') return true;
  return false;
}

function renderLogContentElement(cnt, c) {
  const content = c.content || '';
  const failedCommand = detectCommandFailureLog(content);
  if (failedCommand) {
    cnt.classList.add('log-command', 'log-command-failed');
    cnt.innerHTML =
      `<span class="log-command-label">${escapeHtml(failedCommand.label)}</span>` +
      `<code class="log-command-code">${highlightShellCommand(failedCommand.command)}</code>`;
    return;
  }
  const command = detectCommandLog(content);
  if (command) {
    cnt.classList.add('log-command');
    cnt.innerHTML =
      `<span class="log-command-label">${escapeHtml(command.label || 'command')}</span>` +
      `<code class="log-command-code">${highlightShellCommand(command.command)}</code>`;
    return;
  }
  if (shouldRenderMarkdownLog(c)) {
    cnt.classList.add('log-markdown');
    cnt.innerHTML = renderMarkdown(content);
    return;
  }
  cnt.textContent = content;
}

function logUserTurnIndex(c) {
  const source = String(c?.source || '').toLowerCase();
  const turn = Number(c?.user_turn_index || 0);
  if (source !== 'user' || !Number.isInteger(turn) || turn <= 0) return null;
  return turn;
}

function appendLogStateBadges(cnt, c) {
  const badges = [];
  const userTurn = logUserTurnIndex(c);
  if (userTurn !== null) {
    badges.push({ cls: 'turn', text: `T${userTurn}`, title: `user turn ${userTurn}` });
  }
  if (c.kind === 'rollback_marker') badges.push({ cls: 'rewind', text: 'context rewind' });
  if (c.superseded) badges.push({ cls: 'overwritten', text: 'overwritten' });
  const replacementTurn = c.replacement_for_user_turn_index;
  if (replacementTurn !== undefined && replacementTurn !== null) {
    badges.push({
      cls: 'replacement',
      text: `replaces T${replacementTurn}`,
      title: `replacement for user turn ${replacementTurn}`,
    });
  }
  if (!badges.length) return;
  const wrap = document.createElement('span');
  wrap.className = 'log-state-badges';
  for (const badge of badges) {
    const el = document.createElement('span');
    el.className = 'log-state-badge ' + badge.cls;
    el.textContent = badge.text;
    if (badge.title) el.title = badge.title;
    wrap.appendChild(el);
  }
  cnt.appendChild(wrap);
}

function ensureLogStateBadge(cnt, cls, text, title) {
  if (!cnt) return;
  let wrap = cnt.querySelector(':scope > .log-state-badges');
  if (!wrap) {
    wrap = document.createElement('span');
    wrap.className = 'log-state-badges';
    cnt.appendChild(wrap);
  }
  if (wrap.querySelector('.log-state-badge.' + cls)) return;
  const el = document.createElement('span');
  el.className = 'log-state-badge ' + cls;
  el.textContent = text;
  if (title) el.title = title;
  wrap.appendChild(el);
}

function markLogEntrySuperseded(entry) {
  if (!entry || entry.classList.contains('rollback-marker')) return;
  entry.classList.add('superseded');
  entry.dataset.superseded = 'true';
  const sid = String(entry.dataset.sessionId || '').trim();
  const meta = sessionConfigMetadata(sid);
  const keepHistoricalEdit =
    sessionConfigSource(meta) === 'codex' && sessionConfigManagedMode(meta) === 'managed';
  entry.querySelectorAll('.log-edit-message').forEach(btn => {
    if (keepHistoricalEdit) {
      btn.dataset.historical = 'true';
      btn.title = 'Create a managed branch from this historical message';
    } else {
      btn.remove();
    }
  });
  ensureLogStateBadge(entry.querySelector('.log-content'), 'overwritten', 'overwritten');
}

function normalizedThreadHistoryChangeSet(c = {}) {
  const change = c.thread_history_change || c.threadHistoryChange || {};
  const removed = c.removed_turn_ids || c.removedTurnIds || change.removed_turn_ids || change.removedTurnIds || [];
  const changedItems = c.changed_items || c.changedItems || change.changed_items || change.changedItems || [];
  const changedTurns = c.changed_turns || c.changedTurns || change.changed_turns || change.changedTurns || [];
  return {
    removedTurnIds: Array.isArray(removed)
      ? removed.map(id => String(id || '').trim()).filter(Boolean)
      : [],
    changedItems: Array.isArray(changedItems) ? changedItems : [],
    changedTurns: Array.isArray(changedTurns) ? changedTurns : [],
  };
}

function markActivityContextRewindByTurnIds(sessionId, removedTurnIds) {
  const sid = String(sessionId || '').trim();
  const ids = new Set((removedTurnIds || []).map(id => String(id || '').trim()).filter(Boolean));
  if (!sid || ids.size === 0) return false;
  let marked = false;
  const streams = mainLogContainers();
  for (const win of sessionWindows.values()) {
    if (win?.log) streams.push(win.log);
  }
  for (const stream of streams.filter(Boolean)) {
    for (const entry of Array.from(stream.querySelectorAll('.log-entry'))) {
      if (entry.dataset.sessionId !== sid) continue;
      const turnId = String(entry.dataset.turnId || '').trim();
      if (!ids.has(turnId)) continue;
      markLogEntrySuperseded(entry);
      marked = true;
    }
  }
  for (const win of sessionWindows.values()) {
    for (const entry of ensureSessionWindowHistory(win)) {
      if (sessionWindowHistorySessionId(entry) !== sid) continue;
      if (!ids.has(sessionWindowHistoryTurnId(entry))) continue;
      markSessionWindowHistoryItemSuperseded(entry);
      marked = true;
    }
  }
  return marked;
}

function applyThreadHistoryChangeSet(c = {}) {
  const change = normalizedThreadHistoryChangeSet(c);
  if (!change.removedTurnIds.length) return false;
  return markActivityContextRewindByTurnIds(c.session_id || c.sessionId, change.removedTurnIds);
}

function markActivityContextRewind(sessionId, userTurnIndex, turnsRemoved) {
  const sid = String(sessionId || '').trim();
  const startTurn = Number(userTurnIndex || 0);
  const turnCount = Number(turnsRemoved || 0);
  if (!sid || !Number.isInteger(startTurn) || startTurn <= 0) return;
  const endTurn = Number.isInteger(turnCount) && turnCount > 0
    ? startTurn + turnCount - 1
    : Number.MAX_SAFE_INTEGER;
  const streams = mainLogContainers();
  for (const win of sessionWindows.values()) {
    if (win?.log) streams.push(win.log);
  }
  for (const stream of streams.filter(Boolean)) {
    let inRewoundRegion = false;
    const entries = Array.from(stream.querySelectorAll('.log-entry'));
    for (const entry of entries) {
      if (entry.dataset.sessionId !== sid) continue;
      const turn = Number(entry.dataset.userTurnIndex || 0);
      if (Number.isInteger(turn) && turn > 0) {
        if (!inRewoundRegion && turn >= startTurn && turn <= endTurn) {
          inRewoundRegion = true;
        } else if (inRewoundRegion && turn > endTurn) {
          break;
        }
      }
      if (inRewoundRegion) markLogEntrySuperseded(entry);
    }
  }
  for (const win of sessionWindows.values()) {
    let inRewoundRegion = false;
    const entries = ensureSessionWindowHistory(win);
    for (const entry of entries) {
      if (sessionWindowHistorySessionId(entry) !== sid) continue;
      const turn = Number(sessionWindowHistoryUserTurnIndex(entry) || 0);
      if (Number.isInteger(turn) && turn > 0) {
        if (!inRewoundRegion && turn >= startTurn && turn <= endTurn) {
          inRewoundRegion = true;
        } else if (inRewoundRegion && turn > endTurn) {
          break;
        }
      }
      if (inRewoundRegion) markSessionWindowHistoryItemSuperseded(entry);
    }
  }
}

function isEditableUserMessage(c) {
  const sid = String(c?.session_id || '').trim();
  if (c?.superseded) {
    const meta = sessionConfigMetadata(sid);
    if (sessionConfigSource(meta) !== 'codex' || sessionConfigManagedMode(meta) !== 'managed') return false;
  }
  const revision = Number(c?.user_turn_revision || 0);
  return !!sid && logUserTurnIndex(c) !== null && Number.isInteger(revision) && revision > 0;
}

function appendEditUserMessageButton(entry, c) {
  if (!isEditableUserMessage(c)) return;
  entry.classList.add('editable-user-message');
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'log-edit-message';
  btn.title = c.superseded
    ? 'Create a managed branch from this historical message'
    : 'Edit this user message and rerun from here';
  btn.innerHTML = '&#9998;';
  btn.dataset.sessionId = c.session_id || '';
  btn.dataset.userTurnIndex = String(c.user_turn_index);
  btn.dataset.userTurnRevision = String(c.user_turn_revision || '');
  btn.dataset.message = c.content || '';
  if (c.superseded) btn.dataset.historical = 'true';
  entry.appendChild(btn);
}

function renderLogEntry(c) {
  if (isCommandOutputLog(c)) {
    inferSessionPhaseFromLog(c);
    renderCommandOutputEntry(c);
    return;
  }
  if (isDiffLog(c)) {
    inferSessionPhaseFromLog(c);
    renderDiffLogEntry(c);
    return;
  }
  finalizeActiveCommandOutputGroup(commandOutputGroupKey(c));
  if (shouldSuppressAttachmentReceiptDuplicate(c)) return;
  inferSessionPhaseFromLog(c);

  const { entry } = createLogScaffold(c, '');

  const cnt = document.createElement('span');
  cnt.className = 'log-content';
  renderLogContentElement(cnt, c);
  appendLogStateBadges(cnt, c);

  const hasImages = c.images && c.images.length > 0;
  const attachmentPreviews = Array.isArray(c.attachment_previews) ? c.attachment_previews : [];
  if (hasImages) {
    const badge = document.createElement('span');
    badge.className = 'log-image-badge';
    badge.textContent = c.images.length === 1 ? '[screenshot]' : '[' + c.images.length + ' screenshots]';
    cnt.appendChild(badge);
  }
  if (attachmentPreviews.length > 0) {
    const strip = document.createElement('div');
    strip.className = 'log-attachment-strip';
    for (const att of attachmentPreviews) {
      if (att && att.dataUrl) {
        const img = document.createElement('img');
        img.className = 'log-attachment-thumb';
        img.loading = 'lazy';
        img.src = att.dataUrl;
        img.alt = '';
        img.title = att.note || att.name || att.frameId || 'attachment';
        strip.appendChild(img);
      } else {
        const chip = document.createElement('span');
        chip.className = 'log-attachment-file';
        chip.textContent = (att && (att.name || att.frameId)) || 'attachment';
        strip.appendChild(chip);
      }
    }
    cnt.appendChild(strip);
  }

  entry.appendChild(cnt);
  appendCopyLogEntryButton(entry, c.content ?? '');
  appendEditUserMessageButton(entry, c);

  if (c.collapsible || hasImages) {
    entry.classList.add('collapsible');
    if (hasImages) _logImageStore.set(entry, c.images);
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 more</span><span class="arrow-up">\u25BE less</span>';
    entry.appendChild(toggle);
    wireCollapsibleLogEntry(entry, cnt);
  }

  appendLogEntryElement(entry, sessionWindowRecordFromLogCommand(c));
}

function appendSessionWindowOnlyLogEntry(entry) {
  appendLogEntryToSessionWindow(entry, logReplayAppendBatch);
  if (logReplayAppendBatch) logReplayAppendBatch.added++;
}

function renderSessionWindowLogEntryOnly(c) {
  if (!c || !c.session_id) return;
  const content = c.content || '';
  if (!content && c.kind !== 'rollback_marker') return;
  inferSessionPhaseFromLog(c);
  if (processingLogReplay) {
    appendLogCommandToSessionWindow(c, logReplayAppendBatch);
    return;
  }
  const entry = buildSessionWindowLogEntry(c);
  if (entry) appendSessionWindowOnlyLogEntry(entry);
}

function setContextUsagePct(pct) {
  const fill = document.getElementById('sb-budget-fill');
  const label = document.getElementById('sb-budget-pct');
  if (!fill || !label) return;
  if (pct === undefined || pct === null || Number.isNaN(Number(pct))) {
    fill.style.width = '0%';
    fill.style.background = 'var(--overlay0)';
    label.textContent = '--';
    label.style.color = 'var(--overlay0)';
    return;
  }
  const value = Number(pct);
  fill.style.width = Math.min(value, 100) + '%';
  const color = value < 50 ? 'var(--green)' : value < 85 ? 'var(--yellow)' : 'var(--red)';
  fill.style.background = color;
  label.textContent = value.toFixed(1) + '%';
  label.style.color = color;
}

function parseUsageJson(raw) {
  if (!raw) return null;
  if (typeof raw === 'object') return raw;
  try { return JSON.parse(raw); } catch { return null; }
}

function cacheSessionUsage(c) {
  if (!c || typeof c !== 'object') return;
  const sid = String(c.session_id || '').trim();
  if (sid) {
    sessionUsageById.set(sid, c);
  } else {
    latestGlobalUsage = c;
  }
}

function usageForForegroundSession() {
  const sid = resolvePromptTargetSessionId();
  if (sid) {
    if (sessionUsageById.has(sid)) return sessionUsageById.get(sid);
    // External sessions re-key the foreground to the backend-native id
    // while usage commands keep riding the Intendant log id; look the
    // usage up across the identity group before concluding "no data" —
    // returning null here stomps the header meter to '--' on every
    // usage batch.
    for (const related of relatedSessionIdsForSession(sid)) {
      if (related !== sid && sessionUsageById.has(related)) {
        return sessionUsageById.get(related);
      }
    }
    if (sessionUsageById.size > 0) return null;
  }
  return latestGlobalUsage;
}

function renderForegroundSessionUsage() {
  const usageCommand = usageForForegroundSession();
  const main = usageCommand ? parseUsageJson(usageCommand.main_json) : null;
  setContextUsagePct(main && typeof main.usage_pct === 'number' ? main.usage_pct : null);
}

function applyMainBackendStatus() {
  const external = normalizeAgentId(
    currentExternalAgent !== null
      ? currentExternalAgent
      : ((gatewayConfig && gatewayConfig.external_agent) || controlCurrentBackend)
  );
  const providerEl = document.getElementById('sb-provider');
  const modelEl = document.getElementById('sb-model');
  if (!providerEl || !modelEl) return;

  if (external) {
    providerEl.textContent = prettyAgentName(external);
    modelEl.textContent = 'external agent';
    return;
  }

  providerEl.textContent = (gatewayConfig && gatewayConfig.provider) || '--';
  modelEl.textContent = (gatewayConfig && gatewayConfig.model) || '--';
}

function updateStatusBar(d) {
  if (d.provider) document.getElementById('sb-provider').textContent = d.provider;
  if (d.model) document.getElementById('sb-model').textContent = d.model;
  if (d.turn !== undefined && d.turn !== null) document.getElementById('sb-turn').textContent = 'T' + d.turn;
  if (d.budget_pct !== undefined && d.budget_pct !== null) {
    setContextUsagePct(d.budget_pct);
  }
  if (d.autonomy) {
    const el = document.getElementById('sb-autonomy');
    const autonomy = normalizeAutonomyLabel(d.autonomy);
    el.textContent = autonomy;
    const colors = { Low: 'var(--red)', Medium: 'var(--yellow)', High: 'var(--teal)', Full: 'var(--green)' };
    el.style.color = colors[autonomy] || 'var(--yellow)';
  }
  if (d.session_id) {
    const sid = String(d.session_id || '').trim();
    // Status events are session-scoped and may refer to dashboard-spawned
    // wrapper sessions. The status bar's session id represents the daemon
    // process session and is initialized only from the bootstrap snapshot.
    if (sid && sid !== daemonSessionFullId) {
      const targetSid = statusSessionWindowTarget(sid);
      if (shouldMaterializeStatusSessionWindow(targetSid)) {
        ensureSessionWindow(targetSid, { ended: false });
        updateTaskTargetChip();
      }
    }
  }
  if (d.external_agent !== undefined) {
    // Accept any of the serialization forms we might see on the wire:
    //   short:      "codex" | "claude-code"
    //   display:    "Codex" | "Claude Code"
    //   serde enum: "claude_code"
    // Normalize to the canonical short form used by the dropdown
    // <option value> attributes, then derive a pretty label from that.
    const shortId = normalizeAgentId(d.external_agent);
    currentExternalAgent = shortId;
    if (shortId && currentSessionFullId) {
      updateSessionWindow(currentSessionFullId, { source: prettyAgentName(shortId) });
    }
    const sel = document.getElementById('set-external-agent');
    if (sel) {
      sel.value = shortId || '';
    }
    // Keep the Control sub-tab in sync: backend badge + show/hide Codex
    // config knobs based on whether Codex is the active backend.
    if (controlCurrentBackend !== shortId) {
      controlCurrentBackend = shortId;
      renderControlPane();
    }
    newSessionConfiguredAgent = shortId;
    renderNewSessionAgentControls();
  }
  applyMainBackendStatus();
}

// Map any agent identifier form to the canonical short string used
// by the dashboard dropdown options and (now) the TOML field. Returns
// an empty string for `null`/empty/unknown values.
function normalizeAgentId(raw) {
  if (!raw) return '';
  const v = String(raw).toLowerCase().trim();
  if (v === 'codex') return 'codex';
  if (v === 'claude-code' || v === 'claude_code' || v === 'claudecode' ||
      v === 'cc' || v === 'claude code') return 'claude-code';
  return '';
}

function prettyAgentName(shortId) {
  return {
    'codex': 'Codex',
    'claude-code': 'Claude Code',
  }[shortId] || shortId;
}

function normalizeAutonomyLabel(raw) {
  const v = String(raw || '').toLowerCase().trim();
  if (v === 'low' || v === 'l' || v === '0') return 'Low';
  if (v === 'high' || v === 'h' || v === '2') return 'High';
  if (v === 'full' || v === 'f' || v === '3') return 'Full';
  return 'Medium';
}

function setPhase(phase) {
  const key = phaseKey(phase);
  currentPhase = key;
  const phaseSessionId = resolvePromptTargetSessionId();
  if (phaseSessionId) updateSessionWindow(phaseSessionId, { phase: key });
  const banner = document.getElementById('phase-banner');
  const text = document.getElementById('phase-text');
  banner.className = 'phase-banner';
  const np = isWaitingFollowUpPhase(key)
    ? 'waitingfollowup'
    : key.replace('waiting_', 'waiting').replace('running_', 'running');
  const labels = {
    idle: 'Idle', thinking: 'Thinking...', running: 'Running Agent', runningagent: 'Running Agent',
    waiting: 'Waiting for Input', waitingapproval: 'Waiting for Approval',
    waitinghuman: 'Waiting for Response', waitingfollowup: 'Waiting for Follow-up',
    orchestrating: 'Orchestrating', done: 'Done',
    interrupting: 'Interrupting...', interrupted: 'Interrupted',
  };
  const cat = np.startsWith('waiting') ? 'waiting' : np.startsWith('running') ? 'running' :
    np === 'orchestrating' ? 'thinking' :
    np === 'interrupting' ? 'waiting' :
    np === 'interrupted' ? 'done' : np;
  banner.classList.add('phase-' + cat);
  text.textContent = labels[np] || labels[key] || key;

  if (key === 'idle' || key === 'done' || key === 'interrupted') {
    if (spinnerInterval) { clearInterval(spinnerInterval); spinnerInterval = null; }
    const spinnerEl = document.getElementById('phase-spinner');
    spinnerEl.textContent = key === 'done' ? '\u2713'
      : key === 'interrupted' ? '\u25A0'
      : '';
  } else if (!spinnerInterval) {
    const el = document.getElementById('phase-spinner');
    spinnerInterval = setInterval(() => {
      spinnerIdx = (spinnerIdx + 1) % SPINNER_FRAMES.length;
      el.textContent = SPINNER_FRAMES[spinnerIdx];
    }, 80);
  }
  updateStopButtonVisibility(key);
  // Flip the submit button between "Send" (start task / follow-up) and
  // "↗ Steer" (inject mid-turn) so the user sees which action will fire
  // on the next click. Interrupting is intentionally Send/queue, not Steer.
  updateSubmitButtonLabel(key);
  // Control sub-tab: highlight the "applies on next task" note while a
  // task is genuinely active so the user knows their toggle is queued,
  // not live. Must match the Stop button's allowlist exactly — any
  // denylist here would misclassify `waiting` / `waiting_followup` /
  // similar between-task states as "running".
  const phaseIsActive = isAgentActivePhase(key);
  if (phaseIsActive !== controlBackendActive) {
    controlBackendActive = phaseIsActive;
    updateControlAppliesNote();
  }
  stationScheduleUpdate();
}

// Cross-tab pending-approval indicator: badge the favicon + prefix the page
// title so a pending approval is visible even when this tab is in the
// background. Browsers (esp. Chrome) ignore an in-place href change on the
// existing <link rel=icon>, so we remove it and append a fresh <link> each
// time. The badged icon is composited on a canvas (app icon + attention dot),
// with a solid-tile fallback if the base icon can't load; the title change
// signals it regardless of canvas support.
const _origDocTitle = document.title;
let _approvalIndicatorOn = false;

function _swapFavicon(href) {
  document.querySelectorAll("link[rel~='icon']").forEach(el => el.remove());
  const link = document.createElement('link');
  link.id = 'favicon';
  link.rel = 'icon';
  link.type = 'image/png';
  link.href = href;
  document.head.appendChild(link);
}

function _drawApprovalBadge(ctx, size) {
  // bottom-right attention dot with a dark ring so it reads on any icon
  const r = size * 0.30, cx = size - r - 1, cy = size - r - 1;
  ctx.beginPath(); ctx.arc(cx, cy, r + 3, 0, 2 * Math.PI);
  ctx.fillStyle = '#1e1e2e'; ctx.fill();
  ctx.beginPath(); ctx.arc(cx, cy, r, 0, 2 * Math.PI);
  ctx.fillStyle = '#f38ba0'; ctx.fill();
}

function setApprovalIndicator(pending) {
  if (pending === _approvalIndicatorOn) return;
  _approvalIndicatorOn = pending;
  document.title = pending ? '● Approval needed — ' + _origDocTitle : _origDocTitle;
  if (!pending) { _swapFavicon('/icon-128.png'); return; }
  const size = 64;
  const c = document.createElement('canvas');
  c.width = size; c.height = size;
  const ctx = c.getContext('2d');
  const finish = function() {
    try { if (_approvalIndicatorOn) _swapFavicon(c.toDataURL('image/png')); } catch (_) {}
  };
  const img = new Image();
  img.onload = function() {
    try { ctx.drawImage(img, 0, 0, size, size); } catch (_) {}
    _drawApprovalBadge(ctx, size);
    finish();
  };
  img.onerror = function() {
    ctx.fillStyle = '#313244';
    ctx.fillRect(0, 0, size, size);
    _drawApprovalBadge(ctx, size);
    finish();
  };
  img.src = '/icon-128.png';
}

function clearPendingApproval() {
  pendingApprovalId = null;
  pendingApprovalSessionId = '';
  stationCurrentApproval = null;
  stationScheduleUpdate();
  setApprovalIndicator(false);
}

function revealActivityLogPanel() {
  if (activeTab === 'activity' && activeActivitySubtab !== 'log') {
    switchActivitySubtab('log');
  }
}

function showApproval(id, command, category, sessionId) {
  if (processingLogReplay) return;
  hideAllPanels();
  pendingApprovalId = id;
  pendingApprovalSessionId =
    sessionId
    || approvalSessionIds.get(String(id))
    || currentSessionFullId
    || '';
  if (pendingApprovalSessionId) {
    ensureSessionWindow(pendingApprovalSessionId, { phase: 'waiting' });
    focusSessionWindow(pendingApprovalSessionId);
  }
  document.getElementById('approval-command').textContent = command;
  document.getElementById('approval-category').textContent = category || '';
  stationCurrentApproval = {
    id: String(id),
    command: command || '',
    category: category || '',
  };
  stationScheduleUpdate();
  revealActivityLogPanel();
  document.getElementById('approval-panel').classList.add('visible');
  setApprovalIndicator(true);
}

function showHumanInput(question) {
  hideAllPanels();
  stationCurrentHumanQuestion = question || '';
  document.getElementById('human-question').textContent = question;
  revealActivityLogPanel();
  document.getElementById('human-panel').classList.add('visible');
  stationScheduleUpdate();
}

function showPanel(id) { hideAllPanels(); document.getElementById(id).classList.add('visible'); }
function hidePanel(id) {
  if (id === 'approval-panel') clearPendingApproval();
  if (id === 'human-panel') {
    stationCurrentHumanQuestion = '';
    stationScheduleUpdate();
  }
  document.getElementById(id).classList.remove('visible');
}
function hideAllPanels() {
  clearPendingApproval();
  stationCurrentHumanQuestion = '';
  document.querySelectorAll('.bottom-panel').forEach(p => p.classList.remove('visible'));
  stationScheduleUpdate();
}

function showBadge(tab, text) {
  const badge = document.getElementById('badge-' + tab);
  if (badge) { badge.textContent = text; badge.classList.add('visible'); }
}
function hideBadge(tab) {
  const badge = document.getElementById('badge-' + tab);
  if (badge) { badge.classList.remove('visible'); badge.textContent = ''; }
}

