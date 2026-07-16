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
  if (op === 'delegate-sub-agent') {
    openSessionDelegateModal(sid);
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

// ── Session-window log placeholder (three states) ─────────────────────
// Renders into an empty window log; removed by the first real entry (the
// append paths drop `.session-window-empty`). States:
//   error   — win.hydrateError holds a message (CONTRACT: the hydration
//             owner in fragment 39 sets/clears it); shows a Retry button
//             that re-runs hydrateSessionWindowIfEmpty by name.
//   loading — hydration in flight (restore-in-flight set, or win.metaStale
//             per the same contract).
//   empty   — hydration settled and found nothing: "No output yet".
// Callable by name from other fragments after they flip the contract
// fields; a no-op once real entries exist.
function renderSessionWindowLogPlaceholder(win) {
  if (!win || !win.log) return;
  const existing = win.log.querySelector('.session-window-empty');
  // Real entries present: never stomp them (placeholder is gone already).
  if (win.log.childElementCount > (existing ? 1 : 0)) return;
  const sid = String(win.sessionId || '').trim();
  const hydrating = (typeof sessionWindowRestoreIsInFlightFor === 'function'
    && sid && sessionWindowRestoreIsInFlightFor(sid)) || !!win.metaStale;
  const errorText = String(win.hydrateError || '').trim();
  const stateSig = errorText ? `error:${errorText}` : hydrating ? 'loading' : 'empty';
  if (existing && existing.dataset.phState === stateSig) return;

  const box = document.createElement('div');
  box.className = 'session-window-empty';
  box.dataset.phState = stateSig;
  if (errorText) {
    box.classList.add('session-window-empty-error');
    const line = document.createElement('span');
    line.textContent = 'Transcript failed to load';
    line.title = errorText;
    box.appendChild(line);
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.className = 'session-window-empty-retry';
    retry.textContent = 'Retry';
    retry.title = errorText;
    retry.addEventListener('click', (e) => {
      e.preventDefault();
      e.stopPropagation();
      win.hydrateError = '';
      renderSessionWindowLogPlaceholder(win);
      if (typeof hydrateSessionWindowIfEmpty === 'function' && sid) {
        Promise.resolve(hydrateSessionWindowIfEmpty(sid))
          .catch(() => {})
          .finally(() => renderSessionWindowLogPlaceholder(win));
      }
    });
    box.appendChild(retry);
  } else if (hydrating) {
    box.classList.add('session-window-empty-loading');
    box.textContent = 'Loading transcript…';
  } else {
    box.textContent = 'No output yet';
  }
  if (existing) existing.replaceWith(box);
  else win.log.appendChild(box);
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
  const worktreeBadge = document.createElement('span');
  worktreeBadge.className = 'session-window-worktree hidden';
  const metaRow = document.createElement('div');
  metaRow.className = 'session-window-meta';
  metaRow.appendChild(id);
  metaRow.appendChild(relationStrip);
  metaRow.appendChild(project);
  metaRow.appendChild(worktreeBadge);
  const task = document.createElement('div');
  task.className = 'session-window-task';
  task.textContent = meta.task || 'initial message pending';
  task.title = meta.task || 'Initial user message not known yet';
  title.appendChild(metaRow);
  title.appendChild(cwd);
  title.appendChild(task);
  const status = document.createElement('span');
  status.className = 'session-window-status';
  status.textContent = sessionPhaseLabel(normalizeSessionPhase(meta.phase || 'idle'));
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
    worktreeBadge,
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
    // Sub-agent auto-minimize bookkeeping (in-memory, like minimized):
    // autoMinimized marks a collapse the derivation applied (only those
    // auto-restore when the session goes active again); userRestoredWhileDone
    // marks an explicit user restore of a done sub-agent, which the auto
    // rule must never re-collapse.
    autoMinimized: false,
    userRestoredWhileDone: false,
    followOutput: true,
    pendingOutput: false,
    logHistory: [],
    renderStart: 0,
    renderEnd: 0,
    headerCollapsed: startHeaderCollapsed,
  };
  sessionWindows.set(sid, win);
  renderSessionWindowLogPlaceholder(win);
  updateSessionWindowHeaderCollapseState(sid);
  syncSessionWindowMetadataRefresh();
  updateSessionWindow(sid, meta);
  // A window can be (re)built for an already-finished sub-agent with no
  // phase in the build meta (metadata-only rebuilds, ended flag carried by
  // sessionMetadataById) — updateSessionWindow only reaches the phase
  // applier when meta.phase is set, so derive the auto-minimize here too.
  maybeAutoMinimizeSubagentWindow(sid);
  updateSessionWindowMaximizeState();
  applySessionWindowGridHeight();
  scheduleSessionRelationshipRender();
  return win;
}

// The status pill's honest upgrade: while a session is in an active phase
// AND its vitals carry a live activity section (wire facts from the
// backend's own stream), show the derived state — "Thinking" only while
// reasoning is actually streaming, "Stalled"/"Rate-limited" when that is
// the truth. Without wire activity the optimistic dispatch guess survives
// only as the generic "Awaiting model…" (PHASE_LABELS.thinking) until
// bytes confirm or the session settles.
function sessionWindowPhaseDisplayLabel(sessionId, phase) {
  const p = normalizeSessionPhase(phase);
  if (isAgentActivePhase(p)
      && typeof deriveSessionActivity === 'function'
      && typeof sessionWireActivity === 'function') {
    const act = deriveSessionActivity(sessionWireActivity(sessionId));
    if (act) {
      const label = (typeof ACTIVITY_STATE_LABELS === 'object' && ACTIVITY_STATE_LABELS[act.state])
        || act.state;
      return act.state === 'reasoning' && act.effort ? `${label} (${act.effort})` : label;
    }
  }
  return sessionPhaseLabel(phase);
}

// Applies a phase change to the window chrome: status chip repaint plus the
// approval-confirm hook. Shared by the wide render below and the phase-only
// fast path — the hook (maybeConfirmApprovalSendFromWindowPhase) must fire
// on EVERY phase application, both paths.
function applySessionWindowPhase(win, sid, phase) {
  const nextPhase = normalizeSessionPhase(phase);
  // A real phase transition is fresh evidence about the session; it
  // retires the "optimistic thinking expired" steer demotion below.
  if (win.phase !== nextPhase) win.optimisticActiveExpired = false;
  win.phase = nextPhase;
  if (win.phase === 'idle' || win.phase === 'done' || win.phase === 'interrupted') {
    win.pendingActiveUntil = 0;
  }
  const label = sessionWindowPhaseDisplayLabel(sid, win.phase);
  win.status.textContent = label;
  win.status.title = label;
  win.status.className = 'session-window-status';
  const cls = sessionPhaseClass(win.phase);
  if (cls) win.status.classList.add(cls);
  maybeConfirmApprovalSendFromWindowPhase(sid, win.phase);
  // Done sub-agents collapse on their own (and auto-applied collapses
  // reopen when the session goes active again). Rides every phase
  // application for the same reason the approval hook above does: the
  // active→done crossing arrives via the fast path AND the wide render.
  maybeAutoMinimizeSubagentWindow(sid);
}

function updateSessionWindow(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const win = sessionWindows.get(sid);
  if (!win) return;
  const merged = mergeSessionWindowMetadata(sid, meta);
  meta = merged.meta;
  updateSessionWindowRelationshipStyle(sid);
  const signature = merged.signature;
  if (win.metadataSignature === signature) {
    // Nothing rendered from — nothing persisted from. The unconditional
    // persist here was a synchronous localStorage write per rendered log
    // line (this early-out is the steady-state exit of the per-line
    // phase-inference path).
    return;
  }
  // Phase-only fast path: streaming log lines alternate thinking↔running
  // several times per turn (inferSessionPhaseFromLog), and phase rides the
  // metadata signature — so each alternation used to defeat the early-out
  // and re-run the full header render (identity DOM rebuild, path
  // elements, goal/tier/vitals, relationship badges, menu visibility).
  // When the signatures differ ONLY in phase, repaint the status chip and
  // fire the approval-confirm hook — the only phase consumers here.
  const signatureNoPhase = sessionWindowMetadataSignature({ ...meta, phase: '' });
  const phaseOnly = Boolean(win.metadataSignature) &&
    win.metadataSignatureNoPhase === signatureNoPhase;
  win.metadataSignature = signature;
  win.metadataSignatureNoPhase = signatureNoPhase;
  if (phaseOnly) {
    // Relationship badges derive from phases: a parent's "N sub" chip
    // counts children through sessionRelationshipSubagentIsActive, which
    // reads the child's win.phase — so a phase change that crosses the
    // active boundary (running→done, idle→thinking) must refresh this
    // window's badge AND the parent's. Same-side alternations
    // (thinking↔running) change no badge-visible state and skip it.
    const activeBefore = sessionRelationshipSubagentIsActive(sid);
    if (meta.phase) applySessionWindowPhase(win, sid, meta.phase);
    if (sessionRelationshipSubagentIsActive(sid) !== activeBefore) {
      updateSessionRelationshipBadges(sid);
      if (meta.parentId) updateSessionRelationshipBadges(meta.parentId);
    }
    schedulePersistSessionWindowState();
    return;
  }
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
  if (meta.worktree !== undefined) {
    // Worktree presentation moved into the vitals chip row (a single ⧉
    // glyph; tap reveals branch/path) — re-render so the chip appears as
    // soon as the metadata lands, before any vitals event.
    renderSessionWindowVitals(
      win,
      (sessionMetadataById.get(win.sessionId) || {}).vitals || null
    );
  }
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
    applySessionWindowPhase(win, sid, meta.phase);
  }
  // Arm-only: this window's goal was just rendered; the 1 s ticker owns
  // elapsed-time repaints for the rest (re-rendering EVERY window's goal
  // per metadata update was the waste).
  ensureSessionGoalTickerArmed(renderSessionWindowGoal(win, meta.goal || null));
  renderSessionWindowTier(win);
  renderSessionWindowVitals(win, (sessionMetadataById.get(sid) || {}).vitals || meta.vitals || null);
  updateSessionWindowActionMenuVisibility(sid);
  updateControlFastButtonState();
  updateSessionRelationshipBadges(sid);
  if (meta.parentId) updateSessionRelationshipBadges(meta.parentId);
  scheduleSessionRelationshipRender();
  // Keep the empty-log placeholder truthful (loading / error / "No output
  // yet") while no real entries have arrived — 39's hydration owner flips
  // win.hydrateError / win.metaStale and metadata ticks land here.
  if (win.log && win.log.firstElementChild?.classList?.contains('session-window-empty')) {
    renderSessionWindowLogPlaceholder(win);
  }
  schedulePersistSessionWindowState();
}

// Worktree badge next to the project path: branch name up front, the full
// linkage (checkout path, base branch/commit) in the tooltip.
// Retired into the vitals chip row (the ⧉ chip carries branch/path via
// its tap-to-explain popover; the folder name already shows in CWD/PROJ).
// The badge node stays in the DOM, permanently hidden, so old references
// stay null-safe.
function renderSessionWindowWorktreeBadge(win) {
  if (!win?.worktreeBadge) return;
  win.worktreeBadge.className = 'session-window-worktree hidden';
  win.worktreeBadge.textContent = '';
  win.worktreeBadge.title = '';
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
  const next = !win.minimized;
  // Explicit toggle: the user owns this window's minimize state now. A
  // restore of a done sub-agent is recorded so the auto rule never
  // re-collapses it; a manual minimize clears autoMinimized so the
  // done→active crossing never pops it back open.
  win.autoMinimized = false;
  win.userRestoredWhileDone = !next && sessionWindowIsDoneSubagent(sid);
  setSessionWindowMinimized(sid, next);
}

// ── Sub-agent auto-minimize (derived state) ────────────────────────────
// A finished sub-agent's window collapses on its own so the grid stays
// readable while a parent fans out work. The rule is a DERIVATION, not a
// one-shot event handler: it re-runs on every phase application (both
// updateSessionWindow paths route through applySessionWindowPhase), at
// window build, and when relationship metadata arrives late
// (applySessionRelationshipMetadata) — so the collapsed state reproduces
// after a reload even though win.minimized is never persisted (the
// replayed session_ended re-derives it).
//
// Scope: sub-agent windows ONLY — top-level sessions are never touched.

// The bulk-control predicate — the SAME active boundary the parent
// window's "N sub" badge counts with (sessionRelationshipSubagentIsActive),
// so the "Minimize done" pill and the badges always agree on what "done"
// means.
function sessionWindowIsDoneSubagent(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || !sessionWindows.has(sid)) return false;
  return sessionWindowIsSubagent(sid) && !sessionRelationshipSubagentIsActive(sid);
}

// The AUTO rule demands HARD done-evidence — ended, or phase
// done/interrupted. Bare 'idle' is deliberately NOT enough here: replayed
// windows are built at 'idle' before their real phase arrives
// (onSessionStarted during log replay, when update_status_bar is not
// applied), and waiting_followup normalizes to 'idle' — auto-collapsing
// either would hide a live window. The bulk pill uses the broader badge
// predicate above: an explicit click may collapse idle-parked sub-agents
// too.
//
// User intent always wins: a maximized window is never touched, and a
// window the user explicitly restored while done (userRestoredWhileDone —
// set by the minimize toggle, maximize, and worktree-card paths) stays
// open until the session goes active again. Only collapses THIS rule
// applied (autoMinimized) reopen on the done→active crossing — a manual
// minimize is never popped open.
function maybeAutoMinimizeSubagentWindow(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !sessionWindowIsSubagent(sid)) return;
  if (sessionRelationshipSubagentIsActive(sid)) {
    // Active again: retire the restore override so a future completion
    // re-derives, and reopen a window only the auto rule closed.
    win.userRestoredWhileDone = false;
    if (win.minimized && win.autoMinimized) {
      win.autoMinimized = false;
      setSessionWindowMinimized(sid, false);
    }
    return;
  }
  if (win.minimized || win.userRestoredWhileDone) return;
  if (maximizedSessionWindowId === sid) return;
  const meta = sessionMetadataById.get(sid) || {};
  const phase = normalizeSessionPhase(win.phase || meta.phase || '');
  const hardDone = !!(win.ended || meta.ended)
    || phase === 'done' || phase === 'interrupted';
  if (!hardDone) return;
  win.autoMinimized = true;
  setSessionWindowMinimized(sid, true);
}

// Bulk action behind the "Minimize done" pill (ui2-activity.js owns the
// button + live count): collapse every done sub-agent window. An explicit
// click is explicit intent — it overrides per-window restore flags (and
// re-arms them false), and marks the collapse auto-managed so a sub-agent
// that goes active again still pops back open. Returns how many windows
// it collapsed.
function minimizeDoneSubagentWindows() {
  let changed = 0;
  for (const [sid, win] of sessionWindows) {
    if (!win || win.minimized) continue;
    if (!sessionWindowIsDoneSubagent(sid)) continue;
    win.userRestoredWhileDone = false;
    win.autoMinimized = true;
    setSessionWindowMinimized(sid, true);
    changed += 1;
  }
  return changed;
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
    // Maximizing a minimized done sub-agent is an explicit restore: mark
    // it user-intent so the auto-minimize derivation doesn't re-collapse
    // the window the moment it is un-maximized.
    const win = sessionWindows.get(sid);
    if (win?.minimized && sessionWindowIsDoneSubagent(sid)) {
      win.userRestoredWhileDone = true;
      win.autoMinimized = false;
    }
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
  if (win.phase) {
    // Focus-origin phase injection re-paints the banner from the clicked
    // window; it is NOT wire truth and must not confirm an in-flight
    // approval send (see maybeConfirmApprovalSendFromPhase).
    _setPhaseFromWindowFocus = true;
    try { setPhase(win.phase); } finally { _setPhaseFromWindowFocus = false; }
  }
  // Placeholder lifecycle around hydration: the in-flight flag is set in
  // hydrate's first synchronous chunk, so the immediate repaint shows
  // "Loading transcript…"; the settle repaint covers the zero-entry
  // success (error/success-with-entries repaint via 39's contract calls).
  const hydration = hydrateSessionWindowIfEmpty(sid);
  renderSessionWindowLogPlaceholder(sessionWindows.get(sid));
  Promise.resolve(hydration).catch(() => {}).finally(() => {
    renderSessionWindowLogPlaceholder(sessionWindows.get(sid));
  });
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
    attachment_previews: Array.isArray(c?.attachment_previews) ? c.attachment_previews : [],
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
    // First live activity for the session = its queued follow-up became
    // the running turn's input (see retirePendingFollowUpRowsForSession).
    if ((phase === 'thinking' || phase === 'running') && !processingLogReplay
        && typeof retirePendingFollowUpRowsForSession === 'function') {
      retirePendingFollowUpRowsForSession(sid, String(c.session_id || ''));
    }
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
  // Tool-call announcements are command ROWS, not command output — the
  // level-'agent' fallback below used to swallow them into groups.
  if (c.kind === 'tool_call') return false;
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

// Tool-call previews by item id, captured from agent_started-shaped
// entries so output groups can name the command that produced them.
// Bounded: insertion-ordered Map, oldest evicted past the cap.
const commandPreviewsByItemId = new Map();
const COMMAND_PREVIEW_CAP = 2000;
function recordCommandPreviewFromLog(c) {
  const itemId = String(c?.item_id || c?.itemId || '').trim();
  if (!itemId) return;
  const kind = String(c?.kind || '').trim();
  if (kind === 'agent_output') return;
  if (String(c?.level || '') !== 'agent') return;
  const preview = String(c?.content || '').trim();
  if (!preview) return;
  commandPreviewsByItemId.delete(itemId);
  commandPreviewsByItemId.set(itemId, preview.slice(0, 160));
  while (commandPreviewsByItemId.size > COMMAND_PREVIEW_CAP) {
    commandPreviewsByItemId.delete(commandPreviewsByItemId.keys().next().value);
  }
}

function createLogScaffold(c, extraClass) {
  recordCommandPreviewFromLog(c);
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

// rAF-coalesced follow for the MAIN stream (finding: interleaved layout
// read/write per appended entry). Reading scrollHeight right after each
// append forces a synchronous layout of the 10k-node scroller — N entries
// in one frame cost N layout passes. One scrollTop write per frame, with
// the follow flags re-checked at flush so a user grab mid-burst wins.
let mainLogScrollScheduled = false;
function scheduleMainLogScrollToBottom() {
  if (mainLogScrollScheduled) return;
  mainLogScrollScheduled = true;
  requestAnimationFrame(() => {
    mainLogScrollScheduled = false;
    if (concurrentLogDetachedFragment || !autoScroll) return;
    const stream = document.getElementById('log-stream');
    if (stream) stream.scrollTop = stream.scrollHeight;
  });
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
    scheduleMainLogScrollToBottom();
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
    tail: clone.querySelector('.command-output-tail'),
    loaded: false,
    loading: false,
  };
  if (!view.summary || !view.body) return null;

  if (!Array.isArray(group.clones)) group.clones = [];
  group.clones.push(view);
  view.summary.innerHTML = group.summary.innerHTML;
  if (view.tail) view.tail.textContent = group.tail?.textContent || '';
  clone.classList.toggle('finalized', group.finalized);
  clone.classList.toggle('expanded', group.entry.classList.contains('expanded'));
  if (group.finalized) {
    // Re-fetchable output starts empty (lazy-loaded on expand); output with
    // no persisted ids keeps whatever the clone carried — it is the only copy.
    if (group.outputIds.length) {
      view.body.innerHTML = '';
    } else {
      view.loaded = view.body.childElementCount > 0;
    }
  }
  return view;
}

// Session scope for a log command — the same `targetSid || sid || 'global'`
// prefix commandOutputGroupKey embeds (keep the two in lockstep). Groups
// store it so finalize can sweep "every active group of THIS session"
// without string-parsing keys.
function commandOutputSessionScope(c) {
  const sid = String(c?.session_id || '').trim();
  const targetSid = sid ? sessionWindowTargetForLogSession(sid) : '';
  return targetSid || sid || 'global';
}

// The Activity-tab verbosity dropdown doubles as the output-visibility
// power knob: at Verbose/Debug, command-output groups default to
// expanded; Normal keeps them collapsed to their summary row. Read live
// (not cached) so finalize honors the level at the moment it runs.
function commandOutputDefaultExpanded() {
  const level = document.getElementById('verbosity-select')?.value
    || localStorage.getItem(VERBOSITY_KEY)
    || 'normal';
  return level === 'verbose' || level === 'debug';
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
  if (group.lines > 0) parts.push(group.lines + ' line' + (group.lines === 1 ? '' : 's'));
  else if (group.chunks > 0) parts.push(group.chunks + ' chunk' + (group.chunks === 1 ? '' : 's'));
  if (group.bytes > 0) parts.push(formatCompactBytes(group.bytes));
  if (group.warns > 0) parts.push(`<span class="output-warn">${group.warns} warning${group.warns === 1 ? '' : 's'}</span>`);
  const detail = parts.length ? parts.join(' · ') : 'ready';
  // Name the group by the command that produced it (the agent_started
  // preview for the same tool call) — "output · 6 chunks" said nothing
  // about WHOSE output it was once consecutive tools coalesced.
  const label = group.commandPreview
    ? `<span class="output-command">${escapeHtml(group.commandPreview)}</span>`
    : '<span>output</span>';
  return `<span class="status-dot"></span>${label}<span class="output-detail"> · ${detail}</span>`;
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

// ── Live streaming tail ──
// While a group streams (created, not yet finalized) and sits collapsed,
// a small dim region under the summary shows the LAST few output lines —
// proof the command is alive without the full body. CSS hides it once the
// group is expanded or finalized; finalize also empties it.
const COMMAND_OUTPUT_TAIL_LINES = 3;
const COMMAND_OUTPUT_TAIL_LINE_CHARS = 240;
const COMMAND_OUTPUT_TAIL_BUFFER_CHARS = 4096;

function updateCommandOutputTail(group, text) {
  if (!group?.tail) return;
  const merged = (group.tailText || '') + String(text || '');
  group.tailText = merged.length > COMMAND_OUTPUT_TAIL_BUFFER_CHARS
    ? merged.slice(-COMMAND_OUTPUT_TAIL_BUFFER_CHARS)
    : merged;
  const lines = group.tailText.replace(/\r/g, '').split('\n').filter(line => line.trim() !== '');
  const rendered = lines
    .slice(-COMMAND_OUTPUT_TAIL_LINES)
    .map(line => line.length > COMMAND_OUTPUT_TAIL_LINE_CHARS
      ? '…' + line.slice(-COMMAND_OUTPUT_TAIL_LINE_CHARS)
      : line)
    .join('\n');
  group.tail.textContent = rendered;
  for (const view of commandOutputCloneViews(group)) {
    if (view.tail) view.tail.textContent = rendered;
  }
}

function clearCommandOutputTail(group) {
  if (!group) return;
  group.tailText = '';
  if (group.tail) group.tail.textContent = '';
  for (const view of commandOutputCloneViews(group)) {
    if (view.tail) view.tail.textContent = '';
  }
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
  if (group.copyRef && text && !group.copyRefCapped) {
    if (group.copyRef.text.length + text.length > COMMAND_OUTPUT_COPY_TEXT_CAP) {
      // Stop retaining the concatenation of ALL streamed output in JS heap
      // for the session's lifetime. The copy button re-fetches the full
      // text lazily through the persisted-output lane (same source the
      // expand-after-finalize path loads); the captured prefix stays as
      // the offline fallback.
      group.copyRefCapped = true;
      group.copyRef.fetchText = () => fetchCommandOutputGroupText(group);
    } else {
      group.copyRef.text += text;
    }
  }
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
    updateCommandOutputTail(group, text);
    for (const view of commandOutputCloneViews(group)) {
      if (view.entry.classList.contains('expanded')) {
        appendCommandOutputText(view.body, text);
      }
    }
  }
  if (!processingLogReplay && !group.itemScoped) {
    // Uncorrelated (no item_id) output rides the native batch contract:
    // one wire event carries the COMPLETE stdout+stderr of a finished
    // command batch, delivered as one synchronous UiCommand burst. A
    // microtask after that burst is therefore the command's own
    // completion — finalize deterministically instead of waiting for
    // whatever entry happens to arrive next. Item-scoped (external)
    // groups stream across many wire events and finalize on their
    // session's next non-output entry / round completion instead.
    scheduleCommandOutputBatchFinalize(group);
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
    // Deterministic default: born COLLAPSED (like the replay lane) unless
    // the verbosity knob says outputs default open. The streaming tail
    // below keeps a collapsed live group visibly alive.
    const { entry } = createLogScaffold(
      c,
      'command-output-group' + (commandOutputDefaultExpanded() ? ' expanded' : '')
    );
    entry.dataset.outputGroupId = groupId;
    const wrap = document.createElement('span');
    wrap.className = 'log-content command-output-wrap';
    const summary = document.createElement('span');
    summary.className = 'command-output-summary';
    const tail = document.createElement('span');
    tail.className = 'command-output-tail';
    const body = document.createElement('span');
    body.className = 'command-output-body';
    wrap.appendChild(summary);
    wrap.appendChild(tail);
    wrap.appendChild(body);
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 output</span><span class="arrow-up">\u25BE hide</span>';
    entry.appendChild(wrap);
    entry.appendChild(toggle);
    const copyRef = setLogEntryCopyText(entry, '');
    appendCopyLogEntryButton(entry);

    const itemId = String(
      c.command_item_id || c.commandItemId || c.command_execution?.id ||
      c.commandExecution?.id || c.item_id || c.itemId || ''
    ).trim();
    const group = {
      id: groupId,
      key: groupKey,
      sessionScope: commandOutputSessionScope(c),
      // item-correlated groups stream across wire events (external
      // backends); uncorrelated ones complete within one event (native
      // batch contract) — see appendCommandOutputChunk.
      itemScoped: !!itemId,
      entry,
      summary,
      tail,
      tailText: '',
      body,
      outputIds: [],
      outputIdSet: new Set(),
      chunks: 0,
      lines: 0,
      bytes: 0,
      warns: 0,
      finalized: false,
      finalizeQueued: false,
      loaded: false,
      loading: false,
      clones: [],
      copyRef,
      commandPreview: itemId ? (commandPreviewsByItemId.get(itemId) || '') : '',
      userExpanded: false,
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
  if (!concurrentLogDetachedFragment && autoScroll && stream) scheduleMainLogScrollToBottom();
}

// Uncorrelated (native-batch) groups finalize one microtask after the
// synchronous UiCommand burst that streamed them — the wire event that
// carried the chunks IS the command's completion (stdout+stderr arrive
// together once the batch finished). Armed per chunk, idempotent.
function scheduleCommandOutputBatchFinalize(group) {
  if (!group || group.finalized || group.finalizeQueued) return;
  group.finalizeQueued = true;
  queueMicrotask(() => {
    group.finalizeQueued = false;
    finalizeCommandOutputGroup(group);
  });
}

function finalizeCommandOutputGroup(group) {
  if (!group || group.finalized) return;
  group.finalized = true;
  group.entry.classList.add('finalized');
  // The streaming tail retires with the stream — the collapsed summary
  // row carries the group from here.
  clearCommandOutputTail(group);
  // A group the USER opened stays open with its streamed body — yanking
  // the output shut mid-read because the command happened to finish was
  // the "collapsible doesn't work" experience. At Verbose/Debug the
  // default itself is open, so auto-expanded groups keep their body too.
  // Only groups whose body actually holds the stream stay open — replay
  // recreations stream no DOM text, and an open-but-empty body lies.
  const defaultExpanded = commandOutputDefaultExpanded();
  // Output with no persisted ids cannot be re-fetched on expand — keep
  // the streamed body (hidden while collapsed) instead of destroying the
  // only copy.
  const preserveBody = group.outputIds.length === 0;
  const keepOpen = group.entry.classList.contains('expanded')
    && (group.userExpanded || defaultExpanded)
    && group.body.childElementCount > 0;
  if (keepOpen) {
    group.loaded = true;
    group.loading = false;
  } else {
    group.entry.classList.remove('expanded');
    if (!preserveBody) group.body.innerHTML = '';
    group.loaded = preserveBody && group.body.childElementCount > 0;
    group.loading = false;
  }
  for (const view of commandOutputCloneViews(group)) {
    view.entry.classList.add('finalized');
    if (view.entry.classList.contains('expanded')
        && (group.userExpanded || defaultExpanded)
        && view.body.childElementCount > 0) {
      view.loaded = true;
      view.loading = false;
      continue;
    }
    view.entry.classList.remove('expanded');
    if (!preserveBody) view.body.innerHTML = '';
    view.loaded = preserveBody && view.body.childElementCount > 0;
    view.loading = false;
  }
  // Identity-guarded: a deferred (microtask) finalize must never evict a
  // NEWER group that re-registered under the same key.
  if (group.key && activeCommandOutputGroups.get(group.key) === group) {
    activeCommandOutputGroups.delete(group.key);
  }
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

// Deterministic cross-command close: any non-output entry rendering for a
// session means its in-flight command output is done (backends announce
// the next tool / model text only after the previous command's output was
// delivered). Exact-key finalize almost never fired for item-correlated
// groups — the next entry carries a different (or no) item id, so groups
// stayed open forever purely by timing.
function finalizeSessionCommandOutputGroups(c) {
  const scope = commandOutputSessionScope(c);
  for (const group of Array.from(activeCommandOutputGroups.values())) {
    if (group.sessionScope === scope) finalizeCommandOutputGroup(group);
  }
}

async function loadCommandOutputIntoBody(group, body) {
  if (!group.outputIds.length) {
    // Nothing persisted to re-fetch: finalize preserved the streamed body
    // on the main entry in that case — serve expands from it.
    if (body !== group.body && group.body?.childElementCount) {
      body.innerHTML = group.body.innerHTML;
      return;
    }
    if (body.childElementCount) return;
    body.textContent = 'Output is not available for lazy loading.';
    return;
  }
  const payload = { ids: group.outputIds };
  // Transport F8a: facade POST-shaped read — tunnel first, the HTTP twin
  // serves a dashboard with no tunnel (no replay after an attempt).
  const resp = await daemonApi.request('api_session_current_agent_output', payload);
  const json = (resp.body && typeof resp.body === 'object') ? resp.body : {};
  if (!resp.ok) throw new Error(json.error || `HTTP ${resp.status}`);
  body.innerHTML = '';
  for (const out of (json.outputs || [])) {
    const text = [out.stdout || '', out.stderr || ''].filter(Boolean).join(out.stdout && out.stderr ? '\n' : '');
    await appendCommandOutputTextProgressive(body, text);
  }
  if (!body.childElementCount) {
    body.textContent = 'No persisted output found.';
  }
}

// Copy-text retention cap for streaming command-output groups (2 MiB of
// UTF-16 units). Beyond it the copy ref switches to the lazy fetch below.
const COMMAND_OUTPUT_COPY_TEXT_CAP = 2 * 1024 * 1024;

// Full output text for a capped group's copy button — same persisted-output
// RPC the finalized expand path uses, joined in stream order.
async function fetchCommandOutputGroupText(group) {
  if (!group?.outputIds?.length) return '';
  const resp = await daemonApi.request('api_session_current_agent_output', { ids: group.outputIds });
  const json = (resp.body && typeof resp.body === 'object') ? resp.body : {};
  if (!resp.ok) throw new Error(json.error || `HTTP ${resp.status}`);
  return (json.outputs || [])
    .map(out => [out.stdout || '', out.stderr || ''].filter(Boolean).join(out.stdout && out.stderr ? '\n' : ''))
    .join('');
}

async function toggleCommandOutputView(group, view) {
  if (!view?.entry || !view?.body) return;
  const expanding = !view.entry.classList.contains('expanded');
  // The user's explicit intent survives finalization: a group opened by
  // hand stays open when the command completes; one closed by hand stays
  // closed while streaming.
  group.userExpanded = expanding;
  view.entry.classList.toggle('expanded', expanding);
  if (!group.finalized) {
    // Live group: the main body streams in place (CSS hides it while
    // collapsed). A clone view expanded mid-stream backfills what it
    // missed — chunk appends only reach expanded clones.
    if (expanding && view !== group && !view.body.childElementCount) {
      view.body.innerHTML = group.body.innerHTML;
    }
    return;
  }
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
  finalizeSessionCommandOutputGroups(c);
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

// Render a log entry's attachment previews as a thumbnail strip.
// Each preview is { dataUrl?, url?, name?, note?, frameId?, mime? }:
// `dataUrl` (a data: URL or a same-origin /raw URL) renders as an <img>;
// `url` additionally wraps the thumb in a click-through link that opens
// the blob in a new tab. A preview without pixels — or one whose blob was
// deleted from the upload store (the <img> error path) — degrades to a
// named chip instead of a broken image.
function appendLogAttachmentStrip(cnt, c) {
  const attachmentPreviews = Array.isArray(c?.attachment_previews) ? c.attachment_previews : [];
  if (attachmentPreviews.length === 0) return;
  const strip = document.createElement('div');
  strip.className = 'log-attachment-strip';
  const missingChip = (att) => {
    const chip = document.createElement('span');
    chip.className = 'log-attachment-file';
    chip.textContent = (att && (att.name || att.frameId)) || 'attachment';
    return chip;
  };
  for (const att of attachmentPreviews) {
    if (att && att.dataUrl) {
      const img = document.createElement('img');
      img.className = 'log-attachment-thumb';
      img.loading = 'lazy';
      img.src = att.dataUrl;
      img.alt = '';
      img.title = att.note || att.name || att.frameId || 'attachment';
      let holder = img;
      if (att.url) {
        const link = document.createElement('a');
        link.className = 'log-attachment-link';
        link.href = att.url;
        link.target = '_blank';
        link.rel = 'noopener';
        link.title = img.title;
        link.appendChild(img);
        holder = link;
      }
      img.addEventListener('error', () => {
        const chip = missingChip(att);
        chip.classList.add('log-attachment-missing');
        chip.title = 'attachment unavailable (blob deleted from the upload store)';
        holder.replaceWith(chip);
      }, { once: true });
      strip.appendChild(holder);
    } else {
      strip.appendChild(missingChip(att));
    }
  }
  cnt.appendChild(strip);
}

// ── Session notes (display-only transcript notes) ──
// Wire shape: { event: 'session_note', session_id, note_id, text,
// attachments: [{upload_id, name, mime, url}], source?, ts }.
// Rendered as an ordinary log entry with kind 'session_note' (distinct
// styling via .log-entry[data-kind=session_note]) and the attachment
// strip fed by the upload-store /raw URLs.

function sessionNoteAttachmentPreviews(d) {
  const attachments = Array.isArray(d?.attachments) ? d.attachments : [];
  return attachments.map(att => ({
    dataUrl: att?.url || '',
    url: att?.url || '',
    name: att?.name || 'image',
    mime: att?.mime || '',
  }));
}

// Normalize a session_note wire event (live WS or a raw replay/session-
// detail entry) into the log-command shape renderLogEntry consumes.
function sessionNoteLogCommand(d) {
  const text = String(d?.text ?? d?.content ?? '').trim();
  if (!text) return null;
  const tsMs = Number(d?.ts_ms ?? d?.tsMs ?? d?.ts);
  return {
    session_id: String(d?.session_id || d?.sessionId || '').trim(),
    level: 'info',
    source: String(d?.source || '').trim() || 'note',
    kind: 'session_note',
    content: text,
    note_id: d?.note_id || d?.noteId || '',
    event_id: d?.event_id || d?.eventId || (d?.note_id ? `session-note:${d.note_id}` : ''),
    delivery: d?.delivery || d?.delivery_class || d?.deliveryClass || '',
    // Raw wire events carry unix-ms in `ts`; replay entries carry the
    // session log's HH:MM:SS string there instead, so only accept numbers.
    ts_ms: Number.isFinite(tsMs) && tsMs > 0 ? tsMs : undefined,
    ts: typeof d?.ts === 'string' ? d.ts : '',
    attachment_previews: sessionNoteAttachmentPreviews(d),
  };
}

// Live-path handler for the session_note WS event (the WASM presence
// layer does not know this event; the JS owns its rendering end to end).
function handleSessionNoteEvent(d) {
  const c = sessionNoteLogCommand(d);
  if (!c) return;
  stationPushLogEvent(c);
  renderLogEntry(c);
  stationScheduleUpdate();
}

// QA readback (window.qa convention): the dashboard validator exercises
// the note rail's module-scoped pieces directly — the attachment strip
// (including its deleted-blob chip degradation) and the wire-event
// normalizers. Side-effect-free beyond the DOM node the caller passes in.
window.qa = Object.assign(window.qa || {}, {
  sessionNotes: {
    renderStrip: (target, c) => appendLogAttachmentStrip(target, c),
    logCommand: (d) => sessionNoteLogCommand(d),
  },
  userNotifications: {
    logCommand: (d) => userNotificationLogCommand(d),
    toastText: (d) => userNotificationToastText(d),
  },
});

// Bootstrap-replay adapter: the WASM activity-feed pipeline only carries
// log_entry-shaped replay rows (AddLogEntry has no attachment fields), so
// session_note entries replay as note-styled text entries; the session
// windows and the Sessions detail view re-attach the thumbnails from the
// raw entries. The content deliberately stays byte-identical to the raw
// note text so the transcript-signature dedupe collapses this row with
// the attachment-bearing record the external window sync later inserts.
// Everything else passes through untouched.
function sessionNoteReplayEntryToLogEntry(entry) {
  if (!entry || entry.event !== 'session_note') return entry;
  return {
    event: 'log_entry',
    level: 'info',
    source: String(entry.source || '').trim() || 'note',
    kind: 'session_note',
    content: String(entry.text || '').trim(),
    session_id: entry.session_id || '',
    ts: entry.ts,
    event_id: entry.event_id || '',
    delivery: entry.delivery || '',
  };
}

// ── Agent→user notifications (notify_user) ──
// Wire shape: { event: 'user_notification', session_id, id, title?, text,
// urgency: 'info'|'attention'|'urgent', ts }. Display-only like session
// notes: a transcript row (kind 'user_notification') plus a toast; the
// attention center (fragment 57) separately badges the escalated
// urgencies, and the daemon's attention monitor owns the urgent push.

function userNotificationToastText(d) {
  const title = String(d?.title || '').trim();
  const text = String(d?.text || '').trim();
  return title ? `${title}: ${text}` : text;
}

// Normalize a user_notification wire event (live WS or a raw
// replay/session-detail entry) into the log-command shape renderLogEntry
// consumes.
function userNotificationLogCommand(d) {
  const text = String(d?.text ?? d?.content ?? '').trim();
  if (!text) return null;
  const urgency = String(d?.urgency || 'info');
  const title = String(d?.title || '').trim();
  const tsMs = Number(d?.ts_ms ?? d?.tsMs ?? d?.ts);
  const id = d?.id || d?.notification_id || d?.notificationId || '';
  return {
    session_id: String(d?.session_id || d?.sessionId || '').trim(),
    level: urgency === 'urgent' ? 'warn' : 'info',
    source: 'notify',
    kind: 'user_notification',
    content: title ? `${title}: ${text}` : text,
    event_id: d?.event_id || d?.eventId || (id ? `user-notification:${id}` : ''),
    delivery: d?.delivery || d?.delivery_class || d?.deliveryClass || '',
    // Raw wire events carry unix-ms in `ts`; replay entries carry the
    // session log's HH:MM:SS string there instead, so only accept numbers.
    ts_ms: Number.isFinite(tsMs) && tsMs > 0 ? tsMs : undefined,
    ts: typeof d?.ts === 'string' ? d.ts : '',
  };
}

// Live-path handler for the user_notification WS event (the WASM presence
// layer does not know this event; the JS owns its rendering end to end).
function handleUserNotificationEvent(d) {
  const c = userNotificationLogCommand(d);
  if (!c) return;
  stationPushLogEvent(c);
  renderLogEntry(c);
  stationScheduleUpdate();
  // Toast for at-a-glance visibility; urgent renders in the alarm style.
  const urgency = String(d?.urgency || 'info');
  if (typeof showControlToast === 'function') {
    showControlToast(urgency === 'urgent' ? 'error' : 'info', userNotificationToastText(d));
  }
}

// Bootstrap-replay adapter (same contract as
// sessionNoteReplayEntryToLogEntry): notifications replay as plain
// notify-styled text rows; no toast for history.
function userNotificationReplayEntryToLogEntry(entry) {
  if (!entry || entry.event !== 'user_notification') return entry;
  const title = String(entry.title || '').trim();
  const text = String(entry.text || '').trim();
  return {
    event: 'log_entry',
    level: String(entry.urgency || '') === 'urgent' ? 'warn' : 'info',
    source: 'notify',
    kind: 'user_notification',
    content: title ? `${title}: ${text}` : text,
    session_id: entry.session_id || '',
    ts: entry.ts,
    event_id: entry.event_id || '',
    delivery: entry.delivery || '',
  };
}

function renderLogEntry(c) {
  if (isCommandOutputLog(c)) {
    inferSessionPhaseFromLog(c);
    renderCommandOutputEntry(c);
    return;
  }
  if (isReasoningLog(c)) {
    renderReasoningLogEntry(c);
    return;
  }
  if (isDiffLog(c)) {
    inferSessionPhaseFromLog(c);
    renderDiffLogEntry(c);
    return;
  }
  finalizeSessionCommandOutputGroups(c);
  if (shouldSuppressAttachmentReceiptDuplicate(c)) return;
  inferSessionPhaseFromLog(c);

  const { entry } = createLogScaffold(c, '');

  const cnt = document.createElement('span');
  cnt.className = 'log-content';
  renderLogContentElement(cnt, c);
  appendLogStateBadges(cnt, c);

  const hasImages = c.images && c.images.length > 0;
  if (hasImages) {
    const badge = document.createElement('span');
    badge.className = 'log-image-badge';
    badge.textContent = c.images.length === 1 ? '[screenshot]' : '[' + c.images.length + ' screenshots]';
    cnt.appendChild(badge);
  }
  appendLogAttachmentStrip(cnt, c);

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
  // #sb-budget-pct is a hidden data node; the oversight bar's context
  // meter mirrors its textContent (ui2-chrome.js) and the vitals rail
  // reads it (ui2-activity.js). Only the text matters now.
  const label = document.getElementById('sb-budget-pct');
  if (!label) return;
  if (pct === undefined || pct === null || Number.isNaN(Number(pct))) {
    label.textContent = '--';
    return;
  }
  const value = Number(pct);
  // Clamp: a stale context window can briefly over-report (backends now
  // clamp too, but replayed older sessions still carry raw values) and
  // "142.3%" is a lie either way — the title keeps the raw figure.
  const shown = Math.min(value, 100);
  label.textContent = shown.toFixed(1) + '%';
  label.title = value > 100 ? `raw reading ${value.toFixed(1)}% — context window estimate stale` : '';
}

function parseUsageJson(raw) {
  if (!raw) return null;
  if (typeof raw === 'object') return raw;
  try { return JSON.parse(raw); } catch { return null; }
}

// One full update_usage payload per session ever seen otherwise accumulates
// for the lifetime of the tab (days-long dashboards → MBs). LRU past the cap:
// re-inserting on write keeps the actively-updating sessions at the tail.
const SESSION_USAGE_CACHE_CAP = 500;
function cacheSessionUsage(c) {
  if (!c || typeof c !== 'object') return;
  const sid = String(c.session_id || '').trim();
  if (sid) {
    sessionUsageById.delete(sid);
    sessionUsageById.set(sid, c);
    while (sessionUsageById.size > SESSION_USAGE_CACHE_CAP) {
      sessionUsageById.delete(sessionUsageById.keys().next().value);
    }
  } else {
    latestGlobalUsage = c;
  }
}

function usageForForegroundSession() {
  // Explicit focus, not the prompt-target resolver: its "best usable
  // window" fallbacks would render some other session's usage while the
  // user looks at an unfocused/ended window (global usage is the honest
  // answer when nothing is focused).
  const sid = typeof explicitForegroundSessionId === 'function'
    ? explicitForegroundSessionId() : '';
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

  // No fallback to gatewayConfig here: its provider/model fields are the
  // browser live-audio VOICE pair (Gemini Live by default), not the chat
  // provider — using them made an idle or keyless daemon's status bar
  // claim a chat provider it does not run. Real values arrive with the
  // first session status event; until then the chips stay honest.
  providerEl.textContent = '--';
  modelEl.textContent = '--';
}

// Autonomy source of truth for the cycling control: the last level the
// DAEMON reported (status frames / autonomy_changed echoes), not whatever
// text happens to sit in the DOM chip. cycleAutonomy (fragment 58) writes
// optimistically with {optimisticAutonomy:true} and reverts to this value
// if no echo confirms within its timeout.
let lastConfirmedAutonomy = '';
let autonomyPendingLevel = '';
let autonomyRevertTimer = null;

function updateStatusBar(d, opts = {}) {
  // Status events are session-scoped: only the explicitly focused session
  // may drive the session-scoped header chips (provider, model, turn,
  // context meter). Unscoped ticks (daemon primary) apply only while no
  // session window is focused — a busy background session used to stomp
  // the chips on every status tick, freezing them across grid-window
  // switches. Explicit focus, not the prompt-target resolver: its
  // "best usable window" fallbacks would hand the header to a session the
  // user is not looking at.
  const statusSid = String(d.session_id || '').trim();
  const foreground = typeof explicitForegroundSessionId === 'function'
    ? explicitForegroundSessionId() : '';
  const drivesForeground = !foreground
    ? true
    : (statusSid
        ? (statusSid === foreground
           || (typeof relatedSessionIdsForSession === 'function'
               && relatedSessionIdsForSession(statusSid).has(foreground)))
        : false);
  if (drivesForeground) {
    // Null-guarded like the rest of the file: this runs mid-UiCommand
    // batch, and a missing chip must degrade to a skipped write, not an
    // exception that eats the remaining commands.
    const providerEl = document.getElementById('sb-provider');
    if (d.provider && providerEl) providerEl.textContent = d.provider;
    const modelEl = document.getElementById('sb-model');
    if (d.model && modelEl) modelEl.textContent = d.model;
    const turnEl = document.getElementById('sb-turn');
    if (d.turn !== undefined && d.turn !== null && turnEl) turnEl.textContent = 'T' + d.turn;
    if (d.budget_pct !== undefined && d.budget_pct !== null) setContextUsagePct(d.budget_pct);
  }
  const autonomyEl = document.getElementById('sb-autonomy');
  if (d.autonomy && autonomyEl) {
    const el = autonomyEl;
    const autonomy = normalizeAutonomyLabel(d.autonomy);
    el.textContent = autonomy;
    const colors = { Low: 'var(--red)', Medium: 'var(--yellow)', High: 'var(--teal)', Full: 'var(--green)' };
    el.style.color = colors[autonomy] || 'var(--yellow)';
    if (!opts.optimisticAutonomy) {
      // Wire-driven: the daemon's word. Confirm (or overrule) any pending
      // optimistic cycle — matching echo retires the revert timer; a
      // different echo just painted the daemon's truth over the guess.
      lastConfirmedAutonomy = autonomy;
      if (autonomyPendingLevel === autonomy) autonomyPendingLevel = '';
      if (autonomyRevertTimer && !autonomyPendingLevel) {
        clearTimeout(autonomyRevertTimer);
        autonomyRevertTimer = null;
      }
    }
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

// Human labels for the phase vocabulary, shared by the phase banner and
// the per-window status chips (keys are the collapsed "np" form: waiting_*
// \u2192 waiting*, running_* \u2192 running*). Extracted from setPhase so the chips
// stop rendering raw keys like `waiting_approval`.
// `thinking` is deliberately labeled "Awaiting model…": the phase is an
// optimistic dispatch guess, and the honest "Thinking" claim now comes
// only from the wire-fact activity section (sessionWindowPhaseDisplayLabel
// upgrades the pill when live evidence exists).
const PHASE_LABELS = {
  idle: 'Idle', thinking: 'Awaiting model…', running: 'Running Agent', runningagent: 'Running Agent',
  waiting: 'Waiting for Input', waitingapproval: 'Waiting for Approval',
  waitinghuman: 'Waiting for Response', waitingfollowup: 'Waiting for Follow-up',
  orchestrating: 'Orchestrating', done: 'Done',
  interrupting: 'Interrupting...', interrupted: 'Interrupted',
};

// Collapse a phase key to the PHASE_LABELS lookup form.
function phaseLabelKey(key) {
  return isWaitingFollowUpPhase(key)
    ? 'waitingfollowup'
    : phaseKey(key).replace('waiting_', 'waiting').replace('running_', 'running');
}

// Label for any phase value; unknown phases humanize (underscores \u2192 spaces)
// instead of leaking raw keys into the UI.
function sessionPhaseLabel(phase) {
  const key = phaseKey(phase);
  const np = phaseLabelKey(key);
  return PHASE_LABELS[np] || PHASE_LABELS[key] || key.replace(/_/g, ' ');
}

function setPhase(phase) {
  const key = phaseKey(phase);
  currentPhase = key;
  const phaseSessionId = resolvePromptTargetSessionId();
  if (phaseSessionId) updateSessionWindow(phaseSessionId, { phase: key });
  const banner = document.getElementById('phase-banner');
  const text = document.getElementById('phase-text');
  banner.className = 'phase-banner';
  const np = phaseLabelKey(key);
  const cat = np.startsWith('waiting') ? 'waiting' : np.startsWith('running') ? 'running' :
    np === 'orchestrating' ? 'thinking' :
    np === 'interrupting' ? 'waiting' :
    np === 'interrupted' ? 'done' : np;
  banner.classList.add('phase-' + cat);
  text.textContent = PHASE_LABELS[np] || PHASE_LABELS[key] || key;

  // The activity spinner is pure CSS (ui2-sessions.css keyframes stepping
  // braille glyphs through ::after content) \u2014 no 80ms JS interval doing
  // DOM writes. Terminal phases swap in a static glyph instead.
  const spinnerEl = document.getElementById('phase-spinner');
  if (spinnerEl) {
    if (key === 'idle' || key === 'done' || key === 'interrupted') {
      spinnerEl.classList.remove('phase-spinner-live');
      spinnerEl.textContent = key === 'done' ? '\u2713'
        : key === 'interrupted' ? '\u25A0'
        : '';
    } else {
      spinnerEl.textContent = '';
      spinnerEl.classList.add('phase-spinner-live');
    }
  }
  updateStopButtonVisibility(key);
  maybeConfirmApprovalSendFromPhase(key);
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

// Cross-tab pending-request indicator: SUPERSEDED by the attention center
// (57-attention-notifications.js), which tracks the full pending set
// (approvals + questions, across sessions, from the event stream) and owns
// the title prefix + favicon count badge. The panel show/clear paths still
// call this hook; it just nudges the center to repaint so panel-driven
// transitions repaint promptly.
function setApprovalIndicator(_pending) {
  try { attentionRepaint(); } catch (_) {}
}

// Browsers (esp. Chrome) ignore an in-place href change on the existing
// <link rel=icon>, so replace the element wholesale. Used by the attention
// center's favicon badge.
function _swapFavicon(href) {
  document.querySelectorAll("link[rel~='icon']").forEach(el => el.remove());
  const link = document.createElement('link');
  link.id = 'favicon';
  link.rel = 'icon';
  link.type = 'image/png';
  link.href = href;
  document.head.appendChild(link);
}

// ── Approval delivery tracking (fragment 58's sendApproval drives this) ──
// sendApproval no longer clears the panel optimistically: the panel goes
// into a disabled "Sending…" state and clears only on evidence the daemon
// consumed the approval — the attention set dropping the item (fed by the
// approval_resolved wire event in 57-attention-notifications.js), a phase
// transition out of the waiting state, or an external panel clear
// (hide_all_panels on task_complete/interrupted, another frontend's
// resolution). A 5s timeout re-enables the buttons and warns instead.
let approvalSendPending = null; // { id, sessionId, attnKey, timer, poll }
// Approvals that arrived while one was on screen (item: they used to swap
// silently). Shown in arrival order as the current one resolves.
const approvalDisplayQueue = [];
let approvalShowNextTimer = null;

function setApprovalPanelSending(sending) {
  const panel = document.getElementById('approval-panel');
  if (!panel) return;
  panel.classList.toggle('approval-sending', !!sending);
  panel.querySelectorAll('.approval-actions button').forEach(btn => { btn.disabled = !!sending; });
  let note = panel.querySelector('.approval-sending-note');
  if (sending) {
    if (!note) {
      note = document.createElement('div');
      note.className = 'approval-sending-note';
      panel.querySelector('.approval-actions')?.insertAdjacentElement('afterend', note);
    }
    note.textContent = 'Sending…';
  } else if (note) {
    note.remove();
  }
}

// The attention-center key for an approval, when the center tracks it
// (read-only cross-fragment access at event time; both live in this page).
function attentionApprovalKeyFor(id, sessionId) {
  if (typeof attentionItems === 'undefined' || typeof attentionKey !== 'function') return null;
  const withSession = attentionKey('approval', sessionId || '', id);
  if (attentionItems.has(withSession)) return withSession;
  const bare = attentionKey('approval', '', id);
  if (attentionItems.has(bare)) return bare;
  return null;
}

function approvalEventStreamLooksLive() {
  // #sb-conn carries the primary event-lane state on both transports
  // (ws + connect events). Used as a soft veto: a dead stream clears the
  // attention set wholesale, which must not read as "approval resolved".
  return document.getElementById('sb-conn')?.classList.contains('ok') === true;
}

function beginApprovalSend(id, sessionId) {
  if (approvalSendPending) {
    clearTimeout(approvalSendPending.timer);
    if (approvalSendPending.poll) clearInterval(approvalSendPending.poll);
  }
  const pending = {
    id: String(id),
    sessionId: String(sessionId || ''),
    attnKey: attentionApprovalKeyFor(id, sessionId),
    timer: null,
    poll: null,
  };
  pending.timer = setTimeout(() => {
    // Identity check: a later send must never be killed by this timer.
    if (approvalSendPending !== pending) return;
    if (pending.poll) clearInterval(pending.poll);
    approvalSendPending = null;
    setApprovalPanelSending(false);
    if (typeof showControlToast === 'function') {
      showControlToast('error', 'Approval may not have reached the daemon — retry');
    }
  }, 5000);
  if (pending.attnKey) {
    pending.poll = setInterval(() => {
      if (!approvalSendPending || approvalSendPending !== pending) return;
      if (!approvalEventStreamLooksLive()) return;
      if (typeof attentionItems !== 'undefined' && !attentionItems.has(pending.attnKey)) {
        confirmApprovalSendDelivered();
      }
    }, 250);
  }
  approvalSendPending = pending;
  setApprovalPanelSending(true);
}

// Ack observed or moot: stop timers and restore the panel's idle state.
function resolveApprovalSend() {
  if (!approvalSendPending) return;
  clearTimeout(approvalSendPending.timer);
  if (approvalSendPending.poll) clearInterval(approvalSendPending.poll);
  approvalSendPending = null;
  setApprovalPanelSending(false);
}

function confirmApprovalSendDelivered() {
  if (!approvalSendPending) return;
  approvalSessionIds.delete(String(approvalSendPending.id));
  resolveApprovalSend();
  // hidePanel → clearPendingApproval → the queued-approval pump. No
  // optimistic phase write here: the confirmation came from (or rides
  // just ahead of) real phase traffic.
  hidePanel('approval-panel');
}

// Any phase outside the waiting family means the loop moved on — the
// pending approval was consumed (approve/deny/skip, here or elsewhere).
function approvalPhaseMeansConsumed(phase) {
  const k = phaseKey(phase);
  return !!k && !k.startsWith('waiting');
}

// Global phase covers main-session (sessionless) approvals only — a
// session-scoped send must not be confirmed by an unrelated window focus
// flipping the global banner, and a focus-origin injection (the user
// clicking a window mid-send) is not wire truth either.
let _setPhaseFromWindowFocus = false;
function maybeConfirmApprovalSendFromPhase(key) {
  if (!approvalSendPending || approvalSendPending.sessionId) return;
  if (_setPhaseFromWindowFocus) return;
  if (approvalPhaseMeansConsumed(key)) confirmApprovalSendDelivered();
}

// Per-window phase covers session-scoped approvals (live status events
// land in updateSessionWindow).
function maybeConfirmApprovalSendFromWindowPhase(sid, phase) {
  if (!approvalSendPending || !approvalSendPending.sessionId) return;
  if (String(sid) !== approvalSendPending.sessionId) return;
  if (approvalPhaseMeansConsumed(phase)) confirmApprovalSendDelivered();
}

// "1 of N" over the panel title while more approvals wait. N reads the
// attention center's pending set (the approval_resolved-fed truth) with
// the local queue as the floor.
function updateApprovalPendingCount() {
  const titleEl = document.querySelector('#approval-panel .approval-title');
  if (!titleEl) return;
  let badge = document.getElementById('approval-pending-count');
  let count = 0;
  if (typeof attentionItems !== 'undefined' && attentionItems && attentionItems.size) {
    for (const item of attentionItems.values()) {
      if (item && item.kind === 'approval') count += 1;
    }
  }
  count = Math.max(count, 1 + approvalDisplayQueue.length);
  if (count <= 1) {
    if (badge) badge.remove();
    return;
  }
  if (!badge) {
    badge = document.createElement('span');
    badge.id = 'approval-pending-count';
    badge.className = 'approval-pending-count';
    titleEl.appendChild(badge);
  }
  badge.textContent = `1 of ${count}`;
  badge.title = `${count} approvals are waiting — this is the oldest; resolving it reveals the next.`;
}

// Show the next queued approval once the panel is free. Deferred a tick so
// the clear that triggered it finishes first; entries the attention set no
// longer tracks are dropped (resolved elsewhere / loop returned — a WS
// flap re-delivers still-pending approvals as fresh events).
function scheduleShowNextQueuedApproval() {
  if (approvalShowNextTimer || !approvalDisplayQueue.length) return;
  approvalShowNextTimer = setTimeout(() => {
    approvalShowNextTimer = null;
    if (pendingApprovalId !== null || approvalSendPending) return;
    // Another bottom panel (question / human input / display request) owns
    // the surface — keep the queue intact; hidePanel pumps again when it
    // clears.
    if (document.querySelector('.bottom-panel.visible')) return;
    const next = approvalDisplayQueue.shift();
    if (!next) return;
    if (typeof attentionItems !== 'undefined' && typeof attentionKey === 'function') {
      const tracked = attentionItems.has(attentionKey('approval', next.sessionId, next.id))
        || attentionItems.has(attentionKey('approval', '', next.id));
      if (!tracked) {
        scheduleShowNextQueuedApproval();
        return;
      }
    }
    showApproval(next.id, next.command, next.category, next.sessionId);
  }, 0);
}

function clearPendingApproval() {
  pendingApprovalId = null;
  pendingApprovalSessionId = '';
  stationCurrentApproval = null;
  stationScheduleUpdate();
  setApprovalIndicator(false);
  // Any path that clears the pending approval while a send is in flight is
  // resolution evidence (external hide, task_complete/interrupted panels
  // sweep, another frontend) — retire the "Sending…" state with it.
  resolveApprovalSend();
  scheduleShowNextQueuedApproval();
}

function revealActivityLogPanel() {
  if (activeTab === 'activity' && activeActivitySubtab !== 'log') {
    switchActivitySubtab('log');
  }
}

function showApproval(id, command, category, sessionId) {
  if (processingLogReplay) return;
  // Concurrent approvals: keep the one on screen, queue the newcomer, and
  // badge the count — a second approval used to silently replace the first.
  if (pendingApprovalId !== null && String(pendingApprovalId) !== String(id)) {
    const qid = String(id);
    if (!approvalDisplayQueue.some(q => String(q.id) === qid)) {
      approvalDisplayQueue.push({
        id,
        command,
        category,
        sessionId: sessionId || approvalSessionIds.get(qid) || '',
      });
    }
    updateApprovalPendingCount();
    return;
  }
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
  const approvalCategoryEl = document.getElementById('approval-category');
  approvalCategoryEl.textContent = category || '';
  // No category (older sessions, rehydrated approvals) → no empty pill.
  approvalCategoryEl.style.display = category ? '' : 'none';
  stationCurrentApproval = {
    id: String(id),
    command: command || '',
    category: category || '',
  };
  stationScheduleUpdate();
  revealActivityLogPanel();
  // A re-show of the same approval (bootstrap replay) lands here with the
  // sending state already retired by hideAllPanels — render actionable.
  setApprovalPanelSending(false);
  document.getElementById('approval-panel').classList.add('visible');
  setApprovalIndicator(true);
  updateApprovalPendingCount();
}

function showHumanInput(question) {
  hideAllPanels();
  stationCurrentHumanQuestion = question || '';
  document.getElementById('human-question').textContent = question;
  revealActivityLogPanel();
  document.getElementById('human-panel').classList.add('visible');
  stationScheduleUpdate();
}

// ── Structured user question (external agents' ask-the-user tool) ──
//
// pendingQuestion = { id, sessionId, questions: UserQuestion[] } while the
// panel is up. Selections/free text live in the DOM; sendQuestionAnswer()
// collects them into {question text → answer} and dispatches
// {action:'answer_question', id, answers} (session-scoped like approvals).
let pendingQuestion = null;

function questionOptionClicked(qIndex, optIndex) {
  if (!pendingQuestion) return;
  const q = pendingQuestion.questions[qIndex];
  if (!q) return;
  const block = document.querySelector(`#question-content .question-block[data-q="${qIndex}"]`);
  if (!block) return;
  const buttons = block.querySelectorAll('.question-option');
  const btn = buttons[optIndex];
  if (!btn) return;
  if (q.multi_select) {
    btn.classList.toggle('selected');
  } else {
    buttons.forEach((b) => b.classList.remove('selected'));
    btn.classList.add('selected');
  }
  updateQuestionProgress();
}

function collectQuestionAnswers() {
  if (!pendingQuestion) return null;
  const answers = {};
  for (let i = 0; i < pendingQuestion.questions.length; i++) {
    const q = pendingQuestion.questions[i];
    const block = document.querySelector(`#question-content .question-block[data-q="${i}"]`);
    if (!block) continue;
    const typed = (block.querySelector('.question-free-text')?.value || '').trim();
    const picked = Array.from(block.querySelectorAll('.question-option.selected'))
      .map((b) => b.dataset.label)
      .filter(Boolean);
    // Free text wins when both are present (mirrors the CLI's own picker,
    // where typing into "Other" overrides the highlighted option).
    const answer = typed || picked.join(', ');
    if (!answer) return { missing: q.question };
    answers[q.question] = answer;
  }
  return { answers };
}

// Scroll a question block to the top of the panel's internal scroll region.
// Deliberately NOT scrollIntoView: that also adjusts overflow:hidden
// ancestors (.tab-content), which would drag the whole pane around.
function scrollQuestionIntoView(qIndex) {
  const scroller = document.querySelector('#question-content .question-scroll');
  const block = document.querySelector(`#question-content .question-block[data-q="${qIndex}"]`);
  if (!scroller || !block) return;
  const top = block.getBoundingClientRect().top
    - scroller.getBoundingClientRect().top
    + scroller.scrollTop;
  scroller.scrollTo({ top: Math.max(0, top - 4), behavior: 'smooth' });
}

// Answered = a selected option or non-empty free text (same rule as
// collectQuestionAnswers). Drives the index chips' tick state and the
// "N of M answered" counter; both exist only for multi-question panels,
// so single-question panels no-op here.
function updateQuestionProgress() {
  if (!pendingQuestion) return;
  const content = document.getElementById('question-content');
  const progress = document.getElementById('question-progress');
  if (!progress && !content.querySelector('.question-index-chip')) return;
  const total = pendingQuestion.questions.length;
  let answered = 0;
  for (let i = 0; i < total; i++) {
    const block = content.querySelector(`.question-block[data-q="${i}"]`);
    const typed = (block?.querySelector('.question-free-text')?.value || '').trim();
    const done = !!(block && (typed || block.querySelector('.question-option.selected')));
    if (done) answered++;
    const chip = content.querySelector(`.question-index-chip[data-q="${i}"]`);
    if (chip) {
      chip.classList.toggle('answered', done);
      const header = pendingQuestion.questions[i]?.header || `Q${i + 1}`;
      chip.setAttribute('aria-label', done ? `${header} (answered)` : header);
    }
  }
  if (progress) progress.textContent = `${answered} of ${total} answered`;
}

function showUserQuestion(id, questions, sessionId) {
  if (processingLogReplay) return;
  const list = Array.isArray(questions) ? questions : [];
  if (!list.length) return;
  hideAllPanels();
  pendingQuestion = {
    id,
    sessionId:
      sessionId
      || approvalSessionIds.get(String(id))
      || currentSessionFullId
      || '',
    questions: list,
  };
  if (pendingQuestion.sessionId) {
    ensureSessionWindow(pendingQuestion.sessionId, { phase: 'waiting' });
    focusSessionWindow(pendingQuestion.sessionId);
  }

  const content = document.getElementById('question-content');
  content.innerHTML = '';
  const multi = list.length > 1;
  const title = document.createElement('div');
  title.className = 'approval-title';
  title.textContent = multi
    ? `The agent has ${list.length} questions`
    : 'The agent has a question';
  content.appendChild(title);

  // Pinned index (2+ questions): one chip per header, kept above the
  // scroll region; tapping a chip jumps to its question, and
  // updateQuestionProgress ticks chips as answers land.
  if (multi) {
    const index = document.createElement('div');
    index.className = 'question-index';
    list.forEach((q, qIndex) => {
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'question-index-chip';
      chip.dataset.q = String(qIndex);
      chip.textContent = q.header || `Q${qIndex + 1}`;
      chip.title = q.question;
      chip.addEventListener('click', () => scrollQuestionIntoView(qIndex));
      index.appendChild(chip);
    });
    content.appendChild(index);
  }

  // The question list is the only scrolling region: the panel caps at the
  // pane height (CSS max-height) and title/index/actions stay pinned, so
  // Submit remains reachable however many questions arrived.
  const scroll = document.createElement('div');
  scroll.className = 'question-scroll';

  list.forEach((q, qIndex) => {
    const block = document.createElement('div');
    block.className = 'question-block';
    block.dataset.q = String(qIndex);

    if (q.header) {
      const chip = document.createElement('span');
      chip.className = 'question-header-chip';
      chip.textContent = q.header;
      block.appendChild(chip);
    }
    const text = document.createElement('div');
    text.className = 'question-text';
    text.textContent = q.question;
    block.appendChild(text);

    const options = document.createElement('div');
    options.className = 'question-options';
    (q.options || []).forEach((opt, optIndex) => {
      const btn = document.createElement('button');
      btn.className = 'question-option';
      btn.type = 'button';
      btn.dataset.label = opt.label;
      btn.addEventListener('click', () => questionOptionClicked(qIndex, optIndex));
      const label = document.createElement('span');
      label.className = 'question-option-label';
      label.textContent = opt.label;
      btn.appendChild(label);
      if (opt.description) {
        const desc = document.createElement('span');
        desc.className = 'question-option-desc';
        desc.textContent = opt.description;
        btn.appendChild(desc);
      }
      options.appendChild(btn);
    });
    block.appendChild(options);

    const free = document.createElement('input');
    free.className = 'question-free-text';
    free.type = 'text';
    free.placeholder = (q.options || []).length
      ? 'Or type your own answer…'
      : 'Type your answer…';
    free.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); sendQuestionAnswer(); }
    });
    free.addEventListener('input', () => updateQuestionProgress());
    block.appendChild(free);
    scroll.appendChild(block);
  });
  content.appendChild(scroll);

  const actions = document.createElement('div');
  actions.className = 'approval-actions';
  const submit = document.createElement('button');
  submit.className = 'approve';
  submit.textContent = 'Submit answer';
  submit.addEventListener('click', () => sendQuestionAnswer());
  actions.appendChild(submit);
  const skip = document.createElement('button');
  skip.textContent = 'Skip';
  skip.title = 'Dismiss without answering (the agent proceeds on its own judgment)';
  skip.addEventListener('click', () => sendQuestionAnswer({ skip: true }));
  actions.appendChild(skip);
  if (multi) {
    const progress = document.createElement('span');
    progress.className = 'question-progress';
    progress.id = 'question-progress';
    actions.appendChild(progress);
  }
  content.appendChild(actions);
  updateQuestionProgress();

  // Station surfaces the question through the existing human-question rail.
  const extra = list.length > 1 ? ` [+${list.length - 1} more]` : '';
  stationCurrentHumanQuestion = `${list[0].question}${extra}`;
  stationScheduleUpdate();
  revealActivityLogPanel();
  document.getElementById('question-panel').classList.add('visible');
  setApprovalIndicator(true);
}

function clearPendingQuestion() {
  pendingQuestion = null;
  stationCurrentHumanQuestion = '';
  stationScheduleUpdate();
  setApprovalIndicator(false);
}

function showPanel(id) { hideAllPanels(); document.getElementById(id).classList.add('visible'); }
function hidePanel(id) {
  if (id === 'approval-panel') clearPendingApproval();
  if (id === 'question-panel') clearPendingQuestion();
  // Display-request state lives in 58-display-request.js (same module
  // scope — function declarations hoist across fragments).
  if (id === 'display-request-panel') clearPendingDisplayRequest();
  if (id === 'human-panel') {
    stationCurrentHumanQuestion = '';
    stationScheduleUpdate();
  }
  document.getElementById(id).classList.remove('visible');
  // Any panel clearing frees the surface for a queued approval (the pump
  // defers itself while another bottom panel is visible).
  scheduleShowNextQueuedApproval();
}
function hideAllPanels() {
  clearPendingApproval();
  clearPendingQuestion();
  clearPendingDisplayRequest();
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

