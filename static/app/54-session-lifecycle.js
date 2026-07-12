// ── Session Lifecycle ──
function onSessionStarted(sessionId, task, opts = {}) {
  const sid = String(sessionId || '').trim();
  const hasTask = hasSessionLifecycleTask(task);
  // A replayed announcement (connect-time bootstrap for a session that
  // started before this page existed) rebuilds the window but must not
  // act like a live birth: no focus steal, no current-task clobber, no
  // thinking phase, no changes-pane reset.
  const replayed = !!opts.replayed;
  if (hasTask && !replayed) {
    stationCurrentTask = compactSessionText(task);
    stationCurrentApproval = null;
  }
  stationScheduleUpdate();
  const shouldFocusActivity = !replayed && newSessionSpawnPending;
  const currentTarget = resolvePromptTargetSessionId();
  if (!replayed) finishNewSessionSpawnNotice(sid, task);
  const alias = externalBackendAliasForSession(sid);
  const visibleSessionId = alias?.backendSessionId || sid;
  if (visibleSessionId) {
    setSessionWindowDetached(visibleSessionId, false);
    const meta = {
      phase: replayed || processingLogReplay || !hasTask ? 'idle' : 'thinking',
      ended: false,
      ...(alias?.source ? { source: alias.source, backendSource: alias.source } : {}),
    };
    if (hasTask) meta.task = task;
    ensureSessionWindow(visibleSessionId, meta);
    if (!replayed) {
      focusSessionWindowFromLifecycle(visibleSessionId, {
        force: shouldFocusActivity,
        currentTarget,
      });
    }
  } else {
    updateTaskTargetChip();
  }
  if (replayed) {
    refreshSessionWindowMetadata(250);
    return;
  }
  resetChangesPane();
  focusActivityForSessionEvent({ force: shouldFocusActivity });
  refreshChangesList({ selectFirst: activeActivitySubtab === 'changes', quiet: true });
  // New session: clear any stale timeline and let the first
  // `snapshot_created` event populate it, or populate from the current
  // server state if the session already has rounds recorded (reconnect
  // during an in-flight task).
  if (typeof refreshHistory === 'function') refreshHistory();
  if (sessionsLoaded) {
    loadSessions();
    setTimeout(loadSessions, 1000);
  } else if (visibleSessionId) {
    refreshSessionWindowMetadata(250);
    refreshSessionWindowMetadata(1250);
  }
}

function onSessionAttached(sessionId, source) {
  const currentTarget = resolvePromptTargetSessionId();
  if (sessionId) {
    setSessionWindowDetached(sessionId, false);
    ensureSessionWindow(sessionId, {
      source: source || 'session',
      phase: attachedSessionPhase(sessionId),
      ended: false,
    });
    focusSessionWindowFromLifecycle(sessionId, { currentTarget });
    flushPendingDetachedCodexThreadActions(sessionId);
  } else {
    updateTaskTargetChip();
  }
  resetChangesPane();
  focusActivityForSessionEvent();
  refreshChangesList({ selectFirst: activeActivitySubtab === 'changes', quiet: true });
  if (typeof refreshHistory === 'function') refreshHistory();
  if (sessionsLoaded) {
    loadSessions();
    setTimeout(loadSessions, 1000);
  } else if (sessionId) {
    refreshSessionWindowMetadata(250);
    refreshSessionWindowMetadata(1250);
  }
}

function isExplicitSessionStopReason(reason) {
  const text = String(reason || '').toLowerCase();
  return text.includes('stopped by user');
}

function isRestartingSessionReason(reason) {
  return String(reason || '').toLowerCase().includes('restarting session');
}

function onSessionEnded(sessionId, reason, errorKind) {
  const sid = String(sessionId || '').trim();
  stationCurrentTask = '';
  stationCurrentApproval = null;
  stationScheduleUpdate();
  maybeFailPendingNewSessionSpawnNoProject(errorKind);
  maybeFailRecentNewSessionSpawn(sid, reason, errorKind);
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  const win = sid ? sessionWindows.get(sid) : null;
  const externalSource = sid ? externalSourceForSessionWindow(sid, win) : '';
  const explicitStop = sid && isExplicitSessionStopReason(reason);
  const restarting = sid && isRestartingSessionReason(reason);
  const shouldRemoveSideWindow = sid && (
    meta.relationshipKind === 'side' ||
    String(reason || '').toLowerCase().includes('side conversation closed')
  );
  const keepExternalDetached = sid && !!externalSource && !shouldRemoveSideWindow && !explicitStop;
  if (shouldRemoveSideWindow) {
    clearPendingFollowUpsForSession(sid, 'side conversation closed');
    removeSessionRelationshipsForSession(sid);
    removeSessionWindow(sid);
  } else if (explicitStop) {
    clearPendingFollowUpsForSession(sid, reason || 'session stopped');
    removeSessionRelationshipsForSession(sid);
    // A worktree-backed session leaves a decision behind (merge / remove /
    // keep the worktree), so its window survives an explicit stop to host
    // the finish card; dismissing the card completes the removal.
    if (sessionWorktreeFinishInfo(sid)) {
      updateSessionWindow(sid, { phase: 'done', ended: true });
      maybeShowWorktreeFinishCard(sid, { removeWindowOnDismiss: true });
    } else {
      removeSessionWindow(sid);
    }
  } else if (restarting) {
    clearPendingFollowUpsForSession(sid, reason || 'restarting session');
    updateSessionWindow(sid, { phase: 'idle', ended: false });
    setSessionWindowDetached(sid, true, reason || 'restarting session');
  } else if (keepExternalDetached) {
    updateSessionWindow(sid, { phase: 'idle', ended: false });
    setSessionWindowDetached(sid, true, reason || 'session ended');
  } else if (sid) {
    updateSessionWindow(sid, { phase: 'done', ended: true });
    maybeShowWorktreeFinishCard(sid);
  }
  if (!keepExternalDetached && (!sessionId || sessionId === currentSessionFullId)) {
    currentSessionFullId = '';
  }
  if (!keepExternalDetached && (!sessionId || sessionId === foregroundSessionFullId)) {
    foregroundSessionFullId = '';
    const next = Array.from(sessionWindows.entries())
      .find(([id, win]) => id !== sessionId && win.phase !== 'done')?.[0] || '';
    if (next) focusSessionWindow(next);
    else {
      for (const win of sessionWindows.values()) win.el.classList.remove('foreground');
      setPhase('idle');
      updateTaskTargetChip();
    }
  } else if (keepExternalDetached) {
    updateTaskTargetChip();
    if (
      sid === currentSessionFullId ||
      sid === foregroundSessionFullId ||
      (!hasActiveSessionWindowExcept(sid) && isAgentActivePhase(currentPhase))
    ) {
      setPhase('idle');
    }
  }
  scheduleSessionRelationshipRender();
  // Refresh sessions list if already loaded
  if (sessionsLoaded) loadSessions();
}

// ── Worktree finish card ───────────────────────────────────────────────────
// When a session that ran in a git worktree ends, its window offers the
// three explicit outcomes for the branch it leaves behind: merge it back
// into the base checkout (and remove the checkout), remove the checkout
// via the same safety-checked path as the Worktrees tab, or keep it.
// Nothing is ever automatic — the worktree persists until a click.

const worktreeFinishCardDismissed = new Set();

function sessionWorktreeFinishInfo(sid) {
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  return meta.worktree && meta.worktree.branch && meta.worktree.path ? meta.worktree : null;
}

// The linkage rides the session catalog row (from session_meta.json); it is
// normally hydrated well before the session ends, but fetch once on demand
// for windows that never saw a metadata refresh.
async function hydrateSessionWorktreeFinishInfo(sid) {
  const existing = sessionWorktreeFinishInfo(sid);
  if (existing) return existing;
  let sessions = null;
  try {
    // daemonApi (transport F2): tunnel first, direct HTTP per the GET-twin
    // fallback policy — this helper always tolerated a missing list.
    const resp = await daemonApi.request('api_sessions', { ids: [sid] });
    if (!resp.ok) return null;
    sessions = resp.body;
  } catch (_) {
    return null;
  }
  if (Array.isArray(sessions)) cacheSessionWindowMetadata(sessions);
  return sessionWorktreeFinishInfo(sid);
}

function maybeShowWorktreeFinishCard(sid, options = {}) {
  if (!sid || worktreeFinishCardDismissed.has(sid)) return;
  const attach = info => {
    if (!info || worktreeFinishCardDismissed.has(sid)) return;
    const win = sessionWindows.get(sid);
    if (!win || win.worktreeCard) return;
    renderWorktreeFinishCard(win, sid, info, options);
  };
  const existing = sessionWorktreeFinishInfo(sid);
  if (existing) {
    attach(existing);
    return;
  }
  hydrateSessionWorktreeFinishInfo(sid).then(attach).catch(() => {});
}

function dismissWorktreeFinishCard(sid, options = {}) {
  worktreeFinishCardDismissed.add(sid);
  const win = sessionWindows.get(sid);
  if (win?.worktreeCard) {
    win.worktreeCard.remove();
    win.worktreeCard = null;
  }
  // An explicitly stopped session only kept its window to host this card;
  // finishing the decision completes the original removal.
  if (options.removeWindowOnDismiss) removeSessionWindow(sid);
}

function renderWorktreeFinishCard(win, sid, info, options = {}) {
  const card = document.createElement('div');
  card.className = 'session-worktree-card';
  card.setAttribute('role', 'status');

  const text = document.createElement('div');
  text.className = 'session-worktree-card-text';
  const title = document.createElement('div');
  title.className = 'session-worktree-card-title';
  title.textContent = `Session ended — worktree branch ${info.branch} is still checked out.`;
  const detail = document.createElement('div');
  detail.className = 'session-worktree-card-detail';
  detail.textContent = info.path;
  detail.title = info.path;
  text.appendChild(title);
  text.appendChild(detail);

  const statusLine = document.createElement('div');
  statusLine.className = 'session-worktree-card-status hidden';

  const actions = document.createElement('div');
  actions.className = 'session-worktree-card-actions';
  const mergeBtn = document.createElement('button');
  mergeBtn.type = 'button';
  mergeBtn.className = 'ui-btn session-worktree-card-btn primary';
  mergeBtn.textContent = info.baseBranch
    ? `Merge into ${info.baseBranch} & remove worktree`
    : 'Merge & remove worktree';
  mergeBtn.title = info.baseBranch
    ? `git merge ${info.branch} in ${info.baseRoot || 'the base checkout'} (on ${info.baseBranch}), then remove the worktree checkout. Aborts cleanly on conflict.`
    : 'Merge the worktree branch into the base checkout, then remove the checkout.';
  const removeBtn = document.createElement('button');
  removeBtn.type = 'button';
  removeBtn.className = 'ui-btn session-worktree-card-btn';
  removeBtn.textContent = 'Remove worktree';
  removeBtn.title = 'Remove the checkout without merging (refused if it has uncommitted or unmerged work). The branch ref is kept.';
  const keepBtn = document.createElement('button');
  keepBtn.type = 'button';
  keepBtn.className = 'ui-btn session-worktree-card-btn';
  keepBtn.textContent = 'Keep';
  keepBtn.title = 'Keep the worktree and dismiss. It stays available in Sessions -> Worktrees.';
  actions.appendChild(mergeBtn);
  actions.appendChild(removeBtn);
  actions.appendChild(keepBtn);

  const setBusy = busy => {
    for (const btn of [mergeBtn, removeBtn, keepBtn]) btn.disabled = busy;
    card.classList.toggle('busy', busy);
  };
  const showStatus = (kind, message) => {
    statusLine.className = `session-worktree-card-status ${kind}`;
    statusLine.textContent = message;
  };
  const finishResolved = message => {
    card.classList.remove('busy');
    showStatus('ok', message);
    actions.replaceChildren();
    const dismissBtn = document.createElement('button');
    dismissBtn.type = 'button';
    dismissBtn.className = 'ui-btn session-worktree-card-btn';
    dismissBtn.textContent = 'Dismiss';
    dismissBtn.addEventListener('click', () => dismissWorktreeFinishCard(sid, options));
    actions.appendChild(dismissBtn);
  };
  // daemonApi (transport F2): POST twins — the facade's no-replay policy
  // covers the fallbackAfterRpcFailure:false these calls passed by hand.
  // `url` survives only as the error label the card always showed.
  const callWorktreeAction = async (method, url, payload) => {
    const r = await daemonApi.request(method, payload);
    const result = (r.body && typeof r.body === 'object') ? r.body : {};
    if (!r.ok || result.ok === false) {
      throw new Error(result.error || `${url} returned ${r.status}`);
    }
    return result;
  };

  mergeBtn.addEventListener('click', async () => {
    setBusy(true);
    showStatus('pending', `Merging ${info.branch}...`);
    try {
      const result = await callWorktreeAction(
        'api_worktrees_merge',
        '/api/worktrees/merge',
        { session_id: sid },
      );
      showControlToast('success', `Merged ${info.branch} into ${result.merged_into || info.baseBranch || 'the base branch'}.`);
      finishResolved(result.removed
        ? `Merged into ${result.merged_into || 'the base branch'} and removed the worktree.`
        : `Merged into ${result.merged_into || 'the base branch'}. Worktree kept: ${result.removal_error || 'removal was refused'}.`);
    } catch (err) {
      setBusy(false);
      showStatus('error', err?.message || 'Worktree merge failed.');
    }
  });
  removeBtn.addEventListener('click', async () => {
    setBusy(true);
    showStatus('pending', 'Removing the worktree...');
    try {
      await callWorktreeAction(
        'api_worktrees_remove',
        '/api/worktrees/remove',
        { repo_root: info.baseRoot || '', path: info.path },
      );
      showControlToast('success', 'Worktree removed.');
      finishResolved(`Removed the worktree. Branch ${info.branch} is kept.`);
    } catch (err) {
      setBusy(false);
      showStatus('error', err?.message || 'Worktree removal was refused.');
    }
  });
  keepBtn.addEventListener('click', () => dismissWorktreeFinishCard(sid, options));

  card.appendChild(text);
  card.appendChild(statusLine);
  card.appendChild(actions);
  win.el.insertBefore(card, win.log);
  win.worktreeCard = card;
}

const TASK_TEXTAREA_MIN_HEIGHT_PX = 28;
const TASK_TEXTAREA_MAX_HEIGHT_PX = 120;
const TASK_TEXTAREA_ESTIMATED_LINE_HEIGHT_PX = 17;
const TASK_TEXTAREA_VERTICAL_PADDING_PX = 10;
const TASK_TEXTAREA_ESTIMATED_CHARS_PER_LINE = 72;

function estimateTaskTextareaRows(value) {
  const text = String(value || '');
  if (!text) return 1;
  return text.split('\n').reduce((rows, line) => {
    return rows + Math.max(1, Math.ceil(line.length / TASK_TEXTAREA_ESTIMATED_CHARS_PER_LINE));
  }, 0);
}

function resizeTaskTextarea(input) {
  if (!input || input.tagName !== 'TEXTAREA') return;
  input._taskResizeFrame = 0;
  const rows = estimateTaskTextareaRows(input.value);
  const height = Math.min(
    TASK_TEXTAREA_MAX_HEIGHT_PX,
    Math.max(
      TASK_TEXTAREA_MIN_HEIGHT_PX,
      rows * TASK_TEXTAREA_ESTIMATED_LINE_HEIGHT_PX + TASK_TEXTAREA_VERTICAL_PADDING_PX,
    ),
  );
  input.style.height = `${height}px`;
  input.style.overflowY = height >= TASK_TEXTAREA_MAX_HEIGHT_PX ? 'auto' : 'hidden';
}

function scheduleTaskTextareaResize(input) {
  if (!input || input.tagName !== 'TEXTAREA' || input._taskResizeFrame) return;
  input._taskResizeFrame = requestAnimationFrame(() => resizeTaskTextarea(input));
}

function clearTaskTextarea(input) {
  if (!input) return;
  input.value = '';
  resizeTaskTextarea(input);
}

function setTaskTextareaValue(input, value) {
  if (!input) return;
  input.value = value || '';
  resizeTaskTextarea(input);
  input.focus();
  if (typeof input.setSelectionRange === 'function') {
    const end = input.value.length;
    input.setSelectionRange(end, end);
  }
}

function renderEditMessageChip() {
  const chip = document.getElementById('edit-message-chip');
  const label = document.getElementById('edit-message-label');
  if (!chip || !label) return;
  if (!editMessageDraft) {
    chip.classList.add('hidden');
    label.textContent = 'Editing';
    chip.title = 'Editing a previous user message';
    return;
  }
  const sid = shortSessionId(editMessageDraft.sessionId);
  const action = editMessageDraft.historical ? 'Branching' : 'Editing';
  label.textContent = `${action} ${sid} #${editMessageDraft.userTurnIndex}`;
  chip.title = editMessageDraft.historical
    ? `Creating a managed branch from user turn ${editMessageDraft.userTurnIndex} in session ${editMessageDraft.sessionId}`
    : `Replacing user turn ${editMessageDraft.userTurnIndex} in session ${editMessageDraft.sessionId}`;
}

function cancelEditMessageDraft() {
  editMessageDraft = null;
  renderEditMessageChip();
  updateSubmitButtonLabel(currentPhase);
}

function beginEditUserMessage({ sessionId, userTurnIndex, userTurnRevision, text, historical }) {
  if (!sessionId || !userTurnIndex || !userTurnRevision) return;
  editMessageDraft = {
    sessionId,
    userTurnIndex: Number(userTurnIndex),
    userTurnRevision: Number(userTurnRevision),
    originalText: text || '',
    historical: !!historical,
  };
  ensureSessionWindow(sessionId);
  focusSessionWindow(sessionId);
  renderEditMessageChip();
  setTaskTextareaValue(document.getElementById('activity-task-input'), text || '');
  updateSubmitButtonLabel(currentPhase);
}

function editUserMessageResumeContext(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  const source = externalSourceForSessionWindow(sid, sessionWindows.get(sid) || null)
    || normalizeAgentId(meta.source || meta.sourceLabel || '');
  if (!source || source === 'intendant') return null;
  const resumeId = String(meta.backendSessionId || sid).trim() || sid;
  const ctx = {
    source,
    resume_id: resumeId,
    direct: true,
  };
  if (meta.projectRoot) ctx.project_root = meta.projectRoot;
  return ctx;
}

function submitEditedUserMessage(input, text) {
  if (!editMessageDraft || !app) return false;
  const attachments = pendingAttachments.map(a => a.frameId);
  const attachmentReceipt = pendingAttachments.slice();
  const targetSessionId = editMessageDraft.sessionId;
  const resumeContext = editUserMessageResumeContext(targetSessionId);
  const msg = {
    action: 'edit_user_message',
    session_id: targetSessionId,
    user_turn_index: editMessageDraft.userTurnIndex,
    user_turn_revision: editMessageDraft.userTurnRevision,
    original_text: editMessageDraft.originalText || '',
    text,
  };
  if (resumeContext) Object.assign(msg, resumeContext);
  if (attachments.length > 0) msg.attachments = attachments;
  if (!dispatchSessionControlMsg(msg)) return false;
  markSessionWindowPendingActive(targetSessionId);
  stationUpsertUserMessageEditActivity(
    targetSessionId,
    editMessageDraft.userTurnIndex,
    'requested'
  );
  if (attachments.length > 0) {
    renderAttachmentReceipt(text, attachmentReceipt, 'Edited', targetSessionId);
    clearPendingAttachments({ retainPreviewUrls: true });
  }
  clearTaskTextarea(input);
  editMessageDraft = null;
  renderEditMessageChip();
  updateSubmitButtonLabel(currentPhase);
  return true;
}

document.addEventListener('click', (e) => {
  const editBtn = e.target.closest?.('.log-edit-message');
  if (!editBtn) return;
  e.preventDefault();
  e.stopPropagation();
  beginEditUserMessage({
    sessionId: editBtn.dataset.sessionId || '',
    userTurnIndex: editBtn.dataset.userTurnIndex || '',
    userTurnRevision: editBtn.dataset.userTurnRevision || '',
    text: editBtn.dataset.message || '',
    historical: editBtn.dataset.historical === 'true',
  });
});

document.getElementById('edit-message-cancel')?.addEventListener('click', (e) => {
  e.preventDefault();
  cancelEditMessageDraft();
});

function wireTaskTextarea(id, submit) {
  const input = document.getElementById(id);
  if (!input) return;
  input.addEventListener('input', () => scheduleTaskTextareaResize(input));
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
    e.preventDefault();
    submit();
  });
  resizeTaskTextarea(input);
}

function detachedSessionResumeMessage(sessionId, task, direct, attachments = []) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  const source = externalSourceForSessionWindow(sid) || normalizeAgentId(meta.backendSource || meta.source) || 'intendant';
  const overrides = sessionLaunchOverridesForSession(sid);
  const msg = {
    action: 'resume_session',
    source,
    session_id: sid,
    resume_id: meta.backendSessionId || sid,
    project_root: meta.projectRoot || null,
    task,
    direct: direct !== false,
    ...overrides,
  };
  if (attachments.length > 0) msg.attachments = attachments;
  return msg;
}

function nextFollowUpId() {
  followUpCounter = (followUpCounter + 1) >>> 0;
  return 'follow-' + Date.now() + '-' + followUpCounter;
}

function rememberPendingFollowUp(id, payload) {
  const key = String(id || '').trim();
  if (!key) return;
  pendingFollowUpsById.set(key, {
    sessionId: String(payload.sessionId || '').trim(),
    text: String(payload.text || ''),
    direct: payload.direct === true,
    attachments: Array.isArray(payload.attachments) ? payload.attachments.slice() : [],
    attempts: Number(payload.attempts || 0),
  });
}

function forgetPendingFollowUp(id) {
  const key = String(id || '').trim();
  if (key) pendingFollowUpsById.delete(key);
}

function removeSteerRowSoon(id, delayMs = 1400) {
  const key = String(id || '').trim();
  if (!key) return;
  const entry = steerRows.get(key);
  if (!entry) return;
  if (entry.timeout) clearTimeout(entry.timeout);
  entry.timeout = setTimeout(() => {
    entry.el.classList.add('fading');
    entry.timeout = setTimeout(() => {
      entry.el.remove();
      steerRows.delete(key);
      const strip = document.getElementById('steer-strip');
      if (strip && strip.childElementCount === 0) strip.classList.add('empty');
      stationScheduleUpdate();
    }, 220);
  }, delayMs);
}

function clearPendingFollowUpsForSession(sessionId, reason = 'session closed') {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  for (const [id, pending] of Array.from(pendingFollowUpsById.entries())) {
    if (String(pending.sessionId || '').trim() !== sid) continue;
    pendingFollowUpsById.delete(id);
    onSteerStatusUpdate(id, pending.text || '', 'failed', reason, { sessionId: sid });
    removeSteerRowSoon(id);
  }
}

function autoAttachUnmanagedFollowUp(evt = {}) {
  const id = String(evt.id || '').trim();
  const pending = id ? pendingFollowUpsById.get(id) : null;
  const sessionId = String(evt.session_id || evt.sessionId || pending?.sessionId || '').trim();
  if (!sessionId || !pending || pending.attempts > 0) return false;
  const resume = detachedSessionResumeMessage(
    sessionId,
    pending.text,
    pending.direct,
    pending.attachments
  );
  if (!resume) return false;
  pending.attempts += 1;
  pendingFollowUpsById.set(id, pending);
  dispatchControlMsg(resume);
  markSessionWindowPendingActive(sessionId);
  onSteerStatusUpdate(id, pending.text, 'delivered', 'reattaching session', { sessionId });
  showControlToast('info', `Reattaching session ${shortSessionId(sessionId)} and retrying follow-up`);
  return true;
}

// Unified task dispatch: always send `start_task` regardless of current
// phase. The backend dispatcher routes appropriately (presence-mediated vs
// direct, task_tx vs follow_up_tx) based on mode and the `direct` flag. The
// old phase-based fork between start_task / follow_up was a TUI-centric
// artifact — all agents (native, Codex, Claude Code) treat
// subsequent messages as new turns in the existing conversation.
// Shared by the Activity composer and the Station composer.
function dispatchTaskText(text, options = {}) {
  if (!app) return false;
  text = String(text || '').trim();
  if (!text) return false;
  const attachments = pendingAttachments.map(a => a.frameId);
  const attachmentReceipt = pendingAttachments.slice();
  const direct = document.getElementById('direct-mode-toggle')?.checked || false;
  const targetSessionId = resolvePromptTargetSessionId();
  const msg = targetSessionId
    ? { action: 'start_task', task: text, session_id: targetSessionId }
    : { action: 'create_session', task: text };
  if (targetSessionId && msg.action === 'start_task') {
    const id = nextFollowUpId();
    msg.follow_up_id = id;
    rememberPendingFollowUp(id, {
      sessionId: targetSessionId,
      text,
      direct,
      attachments,
    });
    onSteerStatusUpdate(
      id,
      attachments.length > 0 ? `${text} (${attachments.length} attachment${attachments.length === 1 ? '' : 's'})` : text,
      options.queuedFollowUp ? 'queued' : 'pending',
      options.queuedFollowUp ? 'queued for next turn' : null,
      { sessionId: targetSessionId }
    );
  }
  if (direct) msg.direct = true;
  if (attachments.length > 0) msg.attachments = attachments;
  if (!dispatchSessionControlMsg(msg)) return false;
  if (targetSessionId) markSessionWindowPendingActive(targetSessionId);
  if (attachments.length > 0) {
    renderAttachmentReceipt(text, attachmentReceipt, 'Sent', targetSessionId);
    clearPendingAttachments({ retainPreviewUrls: true });
  }
  return true;
}

window.submitActivityTask = function(options = {}) {
  const input = document.getElementById('activity-task-input');
  if (!input || !app) return;
  const text = input.value.trim();
  if (!text) return;
  clearTaskTextarea(input);
  dispatchTaskText(text, options);
};

// Text-parametrized submit core shared by both composers: codex slash
// commands, mid-turn steer when the target turn accepts it, else the
// start_task / create_session path. Returns true when dispatched (the
// caller clears its own input).
function submitComposedText(text) {
  if (!app) return false;
  text = String(text || '').trim();
  if (!text) return false;
  const codexSlash = parseCodexSlashCommand(text);
  if (codexSlash) {
    return !!dispatchCodexSlashCommand(codexSlash);
  }
  const targetSessionId = resolvePromptTargetSessionId();
  if (
    targetSessionId &&
    !sessionWindowIsDetached(targetSessionId) &&
    isSessionWindowSteerActive(targetSessionId) &&
    sessionSupportsSteer(targetSessionId)
  ) {
    // Mid-turn steer path. JS generates the id locally and we render an
    // optimistic pending row immediately — the backend round-trips a
    // steer_requested echo that updates the same row by id (idempotent
    // onSteerStatusUpdate). If the echo never arrives (server dead /
    // WebSocket severed), the pending row remains visible as a signal
    // that something went wrong.
    steerCounter = (steerCounter + 1) >>> 0;
    const id = 'steer-' + Date.now() + '-' + steerCounter;
    const attachments = pendingAttachments.map(a => a.frameId);
    const attachmentReceipt = pendingAttachments.slice();
    const msg = { action: 'steer', text, id, session_id: targetSessionId };
    if (attachments.length > 0) msg.attachments = attachments;
    dispatchSessionControlMsg(msg);
    onSteerStatusUpdate(
      id,
      attachments.length > 0 ? `${text} (${attachments.length} attachment${attachments.length === 1 ? '' : 's'})` : text,
      'pending',
      null,
      { sessionId: targetSessionId }
    );
    if (attachments.length > 0) {
      renderAttachmentReceipt(text, attachmentReceipt, 'Steered', targetSessionId);
      clearPendingAttachments({ retainPreviewUrls: true });
    }
    return true;
  }
  return dispatchTaskText(text, {
    queuedFollowUp: !!(
      targetSessionId &&
      isSessionWindowEffectivelyActive(targetSessionId) &&
      (!sessionSupportsSteer(targetSessionId) || !isSessionWindowSteerActive(targetSessionId))
    ),
  });
}

// Phase-aware submit entrypoint.
//
// When the agent is idle / done / interrupted / waiting-follow-up, the
// click starts a new task (the existing submitActivityTask path).
// When the agent is actively working (thinking / running / orchestrating /
// waiting-approval / waiting-human), the click injects a
// mid-turn steer via a session-scoped server action, which round-trips
// through the backend as ControlMsg::Steer and comes back as steer_requested /
// steer_accepted / steer_queued / steer_delivered events.
// While an interrupt is already in flight, input is queued as the next
// follow-up instead of trying to steer a turn that is being cancelled.
//
// The submit button's label tracks the phase (Send vs ↗ Steer) so the
// user sees which behavior will fire before clicking.
window.submitActivityOrSteer = function() {
  if (!app) return;
  const input = document.getElementById('activity-task-input');
  if (!input) return;
  const text = input.value.trim();
  if (!text) return;
  if (editMessageDraft) {
    submitEditedUserMessage(input, text);
    return;
  }
  if (submitComposedText(text)) {
    clearTaskTextarea(input);
  }
};

// Update the submit button's label / styling based on the agent phase.
// Called from setPhase alongside updateStopButtonVisibility so the
// two active-state surfaces stay in sync.
function updateSubmitButtonLabel(phase) {
  const btn = document.getElementById('activity-submit-btn');
  if (!btn) return;
  if (editMessageDraft) {
    btn.textContent = 'Save & rerun';
    btn.classList.add('edit-mode');
    btn.classList.remove('steer-mode');
    btn.title = 'Replace the selected user message and rerun the session from that turn.';
    return;
  }
  btn.classList.remove('edit-mode');
  const targetSessionId = resolvePromptTargetSessionId();
  const active = targetSessionId
    ? isSessionWindowSteerActive(targetSessionId)
    : isSteerPhase(phase);
  const canSteer = targetSessionId ? sessionSupportsSteer(targetSessionId) : true;
  if (active && canSteer) {
    btn.innerHTML = '\u2197 Steer';
    btn.classList.add('steer-mode');
    btn.title = 'Send a mid-turn message to the agent. May be queued until the current turn ends.';
  } else {
    btn.textContent = 'Send';
    btn.classList.remove('steer-mode');
    btn.title = 'Start a new task or send a follow-up.';
  }
}

// ── Steer in-flight strip ──
//
// Renders one row per QueuedSteer. The backend flow is:
//   1. User types + clicks Steer → session-scoped server action → server ControlMsg::Steer
//   2. Server emits steer_requested (echo) → WASM → SteerStatusUpdate(pending)
//   3. Server emits steer_accepted / steer_queued, then steer_delivered when observed
//   4. Delivered row fades and is removed; accepted/queued rows stay until delivered.
//
// We stash the DOM node in a Map keyed by the steer id so a later
// SteerStatusUpdate for the same id updates the row in place. Row
// removal on delivered is delayed (~1.2s) so the user sees the
// checkmark before the entry disappears.
const steerRows = new Map(); // id -> { el, timeout }
function handleFollowUpStatusUpdate(evt = {}) {
  const status = String(evt.status || '').trim().toLowerCase();
  const reason = String(evt.reason || '').trim();
  const sessionId = String(evt.session_id || evt.sessionId || '').trim();
  onSteerStatusUpdate(evt.id, evt.text || '', status, reason, { sessionId });
  if (status === 'delivered') {
    forgetPendingFollowUp(evt.id);
    return;
  }
  if (status !== 'failed') return;

  if (sessionId) {
    clearSessionWindowPendingActive(sessionId, 'idle');
    if (reason.toLowerCase().includes('not managed by this daemon')) {
      setSessionWindowDetached(sessionId, true, reason);
      if (autoAttachUnmanagedFollowUp(evt)) {
        forgetPendingFollowUp(evt.id);
        return;
      }
    }
  } else {
    setPhase('idle');
  }
  forgetPendingFollowUp(evt.id);
  showControlToast('error', reason || 'Follow-up failed');
}

function cancelSteerRow(id) {
  const key = String(id || '').trim();
  if (!key) return;
  const entry = steerRows.get(key);
  const sessionId = String(entry?.sessionId || resolvePromptTargetSessionId() || '').trim();
  const text = entry?.text || '';
  const isSteer = key.startsWith('steer-');
  if (isSteer && app) {
    const msg = { action: 'cancel_steer', id: key, reason: 'cleared by user' };
    if (sessionId) msg.session_id = sessionId;
    dispatchSessionControlMsg(msg);
  } else if (app) {
    const msg = { action: 'cancel_follow_up', id: key, reason: 'cleared by user' };
    if (sessionId) msg.session_id = sessionId;
    dispatchSessionControlMsg(msg);
    forgetPendingFollowUp(key);
  } else {
    forgetPendingFollowUp(key);
  }
  onSteerStatusUpdate(key, text, 'cancelled', 'cleared by user', { sessionId });
  removeSteerRowSoon(key, 700);
}

function onSteerStatusUpdate(id, text, status, reason, options = {}) {
  const key = String(id || '').trim();
  if (!key) return;
  const strip = document.getElementById('steer-strip');
  if (!strip) return;
  const sessionId = String(options.sessionId || options.session_id || '').trim();
  let entry = steerRows.get(key);
  if (!entry) {
    const el = document.createElement('div');
    el.className = 'steer-row ' + status;
    el.dataset.steerId = key;
    el.innerHTML =
      '<span class="steer-icon"></span>'
      + '<span class="steer-text"></span>'
      + '<span class="steer-reason"></span>'
      + '<button type="button" class="steer-cancel" title="Clear queued steer" aria-label="Clear queued steer">&#10005;</button>';
    strip.appendChild(el);
    entry = { el, timeout: null, text: '', sessionId: '' };
    steerRows.set(key, entry);
    el.querySelector('.steer-cancel')?.addEventListener('click', () => cancelSteerRow(key));
  }
  if (sessionId) entry.sessionId = sessionId;
  const el = entry.el;
  el.className = 'steer-row ' + status;
  // Icons chosen to match the WASM-side log lines so the strip and the log tell the same story.
  const iconByStatus = { pending: '\u23F3', accepted: '\u21AA', queued: '\u23F0', delivered: '\u2713', cancelled: '\u2715', failed: '\u2715' };
  el.querySelector('.steer-icon').textContent = iconByStatus[status] || '';
  const textNode = el.querySelector('.steer-text');
  const reasonNode = el.querySelector('.steer-reason');
  const cancelBtn = el.querySelector('.steer-cancel');
  if (cancelBtn) {
    const terminal = ['delivered', 'cancelled', 'failed'].includes(status);
    cancelBtn.style.display = !terminal ? '' : 'none';
    cancelBtn.title = key.startsWith('steer-') ? 'Clear queued steer' : 'Clear queued follow-up';
    cancelBtn.setAttribute('aria-label', cancelBtn.title);
  }
  if (text) entry.text = text;
  textNode.textContent = entry.text || text || '';
  textNode.title = entry.text || text || '';
  reasonNode.textContent = reason ? ('— ' + reason) : '';
  reasonNode.title = reason || '';
  el.title = [entry.text || text, reason].filter(Boolean).join('\n\n');
  strip.classList.remove('empty');
  stationScheduleUpdate();

  // Clear any in-flight fade timer — a fresh status update preempts it.
  if (entry.timeout) { clearTimeout(entry.timeout); entry.timeout = null; }

  if (status === 'delivered' || status === 'cancelled') {
    // Brief visual confirmation, then remove the row.
    entry.timeout = setTimeout(() => {
      el.classList.add('fading');
      entry.timeout = setTimeout(() => {
        el.remove();
        steerRows.delete(key);
        if (strip.childElementCount === 0) strip.classList.add('empty');
        stationScheduleUpdate();
      }, 220);
    }, 1200);
  }
}

function setNewSessionProjectRoot(root) {
  dashboardProjectRoot = String(root || '').trim();
  const input = document.getElementById('new-session-project-root');
  if (input && dashboardProjectRoot && !input.value.trim()
      && newSessionPathPrefillable(dashboardProjectRoot)) {
    input.value = dashboardProjectRoot;
  }
  updateNewSessionProjectPrefills();
  if (input) input.title = input.value.trim() || dashboardProjectRoot;
  scheduleNewSessionProjectStatusRefresh({ hideWhileChecking: true });
}

// (newSessionProjectStatusTimer / newSessionProjectObservedValue live in
// the early client-state block — #sessions/new deep-link TDZ.)
let fsPickerCurrentPath = '';
let fsPickerSelectedPath = '';
let fsPickerMode = 'directory';
let fsPickerTarget = 'project';
let fsPickerDownloadAbort = null;

function newSessionProjectInputValue() {
  return document.getElementById('new-session-project-root')?.value.trim() || '';
}

// True when `path` is neither this daemon's project root nor inside it.
// Cross-project launches are legitimate (never blocked) — this only feeds
// the status line's neutral warning so a foreign path is a visible choice,
// not an accident (observed: a datalist prefill launched an agent into
// another agent's live worktree).
function newSessionPathOutsideProjectRoot(path) {
  const p = String(path || '').trim();
  const root = String(dashboardProjectRoot || '').trim();
  if (!p || !root) return false;
  if (p === root) return false;
  return !p.startsWith(root.endsWith('/') ? root : root + '/');
}

function setNewSessionProjectStatus(kind, text) {
  const el = document.getElementById('new-session-project-status');
  if (!el) return;
  const outside = !!text && newSessionPathOutsideProjectRoot(newSessionProjectInputValue());
  el.className = 'sessions-project-status' + (kind ? ` ${kind}` : '');
  el.textContent = text || '';
  if (outside) {
    // Own span so the warn tint doesn't repaint the status' ok/error tone.
    const note = document.createElement('span');
    note.className = 'sessions-project-outside-note';
    note.textContent = ' · Outside this daemon’s project root';
    el.appendChild(note);
  }
  el.title = (text || '') + (outside ? ' · Outside this daemon’s project root' : '');
}

function setNewSessionCreateVisible(visible) {
  const wrap = document.getElementById('new-session-create-project-dir-wrap');
  const box = document.getElementById('new-session-create-project-dir');
  if (wrap) wrap.classList.toggle('visible', !!visible);
  if (!visible && box) box.checked = false;
}

async function fetchProjectPathStatus(path) {
  const target = path || '';
  // Transport F8a: facade GET twin (tunnel first, HTTP fallback) — the
  // same api_fs_stat lane the Files IDE rides (F1).
  const resp = await daemonApi.request('api_fs_stat', { path: target });
  const data = (resp.body && typeof resp.body === 'object') ? resp.body : {};
  if (!resp.ok) throw new Error(data.error || `Path check failed (${resp.status})`);
  return data;
}

function renderNewSessionProjectStatus(status, statusPath = newSessionProjectInputValue()) {
  newSessionProjectObservedValue = String(statusPath || '');
  if (!status) {
    setNewSessionProjectStatus('', '');
    setNewSessionCreateVisible(false);
    return;
  }
  if (status.exists && status.is_dir) {
    setNewSessionProjectStatus('ok', 'Directory exists on this host');
    setNewSessionCreateVisible(false);
  } else if (status.exists) {
    setNewSessionProjectStatus('error', 'Path exists but is not a directory');
    setNewSessionCreateVisible(false);
  } else if (status.can_create) {
    setNewSessionProjectStatus('warn', 'Directory does not exist on this host');
    setNewSessionCreateVisible(true);
  } else {
    setNewSessionProjectStatus('error', 'Directory cannot be created from this path');
    setNewSessionCreateVisible(false);
  }
}

async function refreshNewSessionProjectStatus(options = {}) {
  const hideWhileChecking = !!options.hideWhileChecking;
  const path = newSessionProjectInputValue();
  if (!path) {
    renderNewSessionProjectStatus(null, path);
    return null;
  }
  newSessionProjectObservedValue = path;
  setNewSessionProjectStatus('warn', 'Checking directory on this host...');
  if (hideWhileChecking) setNewSessionCreateVisible(false);
  try {
    const status = await fetchProjectPathStatus(path);
    if (newSessionProjectInputValue() !== path) return null;
    renderNewSessionProjectStatus(status, path);
    return status;
  } catch (e) {
    if (newSessionProjectInputValue() === path) {
      newSessionProjectObservedValue = path;
      setNewSessionProjectStatus('error', e.message || 'Path check failed');
      setNewSessionCreateVisible(false);
    }
    return null;
  }
}

function scheduleNewSessionProjectStatusRefresh(options = {}) {
  const hideWhileChecking = !!options.hideWhileChecking;
  if (newSessionProjectStatusTimer) clearTimeout(newSessionProjectStatusTimer);
  if (hideWhileChecking && newSessionProjectInputValue()) {
    setNewSessionProjectStatus('warn', 'Checking directory on this host...');
    setNewSessionCreateVisible(false);
  }
  newSessionProjectStatusTimer = setTimeout(
    () => refreshNewSessionProjectStatus({ hideWhileChecking }),
    180,
  );
}

function refreshNewSessionProjectStatusOnValueDrift() {
  if (activeTab !== 'sessions' || activeSessionsSubtab !== 'new') return;
  const current = newSessionProjectInputValue();
  if (current === newSessionProjectObservedValue) return;
  scheduleNewSessionProjectStatusRefresh({ hideWhileChecking: true });
}

async function ensureNewSessionProjectDirectory(projectRoot) {
  if (!projectRoot) return '';
  let status;
  try {
    status = await fetchProjectPathStatus(projectRoot);
  } catch (e) {
    setNewSessionProjectStatus('error', e.message || 'Path check failed');
    setNewSessionCreateVisible(false);
    return null;
  }
  renderNewSessionProjectStatus(status);
  if (status.exists && status.is_dir) return status.path || projectRoot;
  if (status.exists) return null;

  const create = document.getElementById('new-session-create-project-dir')?.checked || false;
  if (!create) {
    setNewSessionProjectStatus('warn', 'Directory does not exist; enable Create to start here');
    setNewSessionCreateVisible(true);
    return null;
  }

  setNewSessionProjectStatus('warn', 'Creating directory on this host...');
  // Transport F8a: facade POST twin — the verb-derived no-replay policy
  // is the legacy fallbackAfterRpcFailure:false semantics.
  const resp = await daemonApi.request('api_fs_mkdir', { path: projectRoot });
  const data = (resp.body && typeof resp.body === 'object') ? resp.body : {};
  if (!resp.ok) {
    setNewSessionProjectStatus('error', data.error || `Create failed (${resp.status})`);
    return null;
  }
  const createdPath = data.path || projectRoot;
  const input = document.getElementById('new-session-project-root');
  if (input) input.value = createdPath;
  setNewSessionProjectStatus('ok', data.already_exists ? 'Directory already exists on this host' : 'Directory created on this host');
  setNewSessionCreateVisible(false);
  return createdPath;
}

// Temp-shaped paths (agent rigs, e2e homes, OS temp) dominate recent
// sessions on busy machines; never suggest one, and never auto-fill the
// project input with one. Typing a temp path stays possible — this only
// stops the dashboard from proposing it. (sessionPathLooksTemporary is
// declared in a later fragment; calls here run at event time, after all
// fragments are parsed.)
function newSessionPathPrefillable(path) {
  if (!path) return false;
  return !(typeof sessionPathLooksTemporary === 'function' && sessionPathLooksTemporary(path));
}

function addNewSessionProjectPrefill(options, seen, value, source, sessionId = '') {
  const path = String(value || '').trim();
  if (!path || seen.has(path)) return;
  if (!newSessionPathPrefillable(path)) return;
  seen.add(path);
  const sid = shortSessionId(sessionId);
  options.push({
    path,
    label: sid ? `${source} · ${sid}` : source,
  });
}

function updateNewSessionProjectPrefills(sessions = _cachedSessions) {
  const datalist = document.getElementById('new-session-project-options');
  if (!datalist) return;
  const options = [];
  const seen = new Set();
  addNewSessionProjectPrefill(options, seen, dashboardProjectRoot, 'current PROJ');

  const recent = Array.isArray(sessions)
    ? [...sessions].sort((a, b) => sessionDateSortValue(b, 'updated_at') - sessionDateSortValue(a, 'updated_at'))
    : [];
  for (const session of recent) {
    addNewSessionProjectPrefill(options, seen, session.project_root, 'recent PROJ', session.session_id);
    addNewSessionProjectPrefill(options, seen, session.cwd, 'recent CWD', session.session_id);
    if (options.length >= 12) break;
  }

  datalist.innerHTML = '';
  for (const option of options.slice(0, 12)) {
    const el = document.createElement('option');
    el.value = option.path;
    el.label = option.label;
    datalist.appendChild(el);
  }

  const input = document.getElementById('new-session-project-root');
  if (input) {
    if (!input.value.trim()) {
      // Auto-fill ONLY this daemon's own project root. options[0] used to
      // fill in unconditionally — when dashboardProjectRoot is excluded by
      // the temp-path filter, options[0] is a FOREIGN recent project, and
      // that silent prefill launched an agent into another agent's live
      // worktree. The datalist suggestions stay (they're labeled); an empty
      // input is an explicit choice the user has to make.
      const ownRoot = String(dashboardProjectRoot || '').trim();
      if (ownRoot && newSessionPathPrefillable(ownRoot)) {
        input.value = ownRoot;
      } else if (ownRoot) {
        input.placeholder = 'Pick a project directory — this daemon runs in a temporary root';
      }
    }
    input.title = input.value.trim() || '';
    scheduleNewSessionProjectStatusRefresh({ hideWhileChecking: true });
  }
}

function setFsPickerStatus(kind, text) {
  const el = document.getElementById('fs-picker-status');
  if (!el) return;
  el.className = 'fs-picker-status' + (kind ? ` ${kind}` : '');
  el.textContent = text || '';
}

function fsPathLooksAbsolute(path) {
  const value = String(path || '').trim();
  return value === '~' || value.startsWith('~/') || value.startsWith('/') || /^[A-Za-z]:[\\/]/.test(value);
}

