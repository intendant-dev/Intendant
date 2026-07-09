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
    removeSessionWindow(sid);
  } else if (restarting) {
    clearPendingFollowUpsForSession(sid, reason || 'restarting session');
    updateSessionWindow(sid, { phase: 'idle', ended: false });
    setSessionWindowDetached(sid, true, reason || 'restarting session');
  } else if (keepExternalDetached) {
    updateSessionWindow(sid, { phase: 'idle', ended: false });
    setSessionWindowDetached(sid, true, reason || 'session ended');
  } else if (sid) {
    updateSessionWindow(sid, { phase: 'done', ended: true });
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
  if (input && dashboardProjectRoot && !input.value.trim()) input.value = dashboardProjectRoot;
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

function setNewSessionProjectStatus(kind, text) {
  const el = document.getElementById('new-session-project-status');
  if (!el) return;
  el.className = 'sessions-project-status' + (kind ? ` ${kind}` : '');
  el.textContent = text || '';
  el.title = text || '';
}

function setNewSessionCreateVisible(visible) {
  const wrap = document.getElementById('new-session-create-project-dir-wrap');
  const box = document.getElementById('new-session-create-project-dir');
  if (wrap) wrap.classList.toggle('visible', !!visible);
  if (!visible && box) box.checked = false;
}

async function fetchProjectPathStatus(path) {
  const target = path || '';
  const resp = await dashboardJsonFetch('api_fs_stat', { path: target }, () => (
    authedFetch('/api/fs/stat?path=' + encodeURIComponent(target))
  ), 'api_fs_stat');
  const data = await resp.json().catch(() => ({}));
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
  const resp = await dashboardJsonFetch('api_fs_mkdir', { path: projectRoot }, () => (
    authedFetch('/api/fs/mkdir', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path: projectRoot }),
    })
  ), 'api_fs_mkdir', { fallbackAfterRpcFailure: false });
  const data = await resp.json().catch(() => ({}));
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

function addNewSessionProjectPrefill(options, seen, value, source, sessionId = '') {
  const path = String(value || '').trim();
  if (!path || seen.has(path)) return;
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
    if (!input.value.trim() && options.length > 0) input.value = options[0].path;
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

function setSettingsDownloadFileStatus(text, kind = '') {
  const el = document.getElementById('settings-download-file-status');
  if (!el) return;
  el.textContent = text || '';
  el.style.color = kind === 'error'
    ? 'var(--red)'
    : kind === 'ok'
      ? 'var(--green)'
      : kind === 'warn'
        ? 'var(--yellow)'
        : 'var(--subtext0)';
}

function filesDownloadPathValue() {
  return document.getElementById('files-download-path')?.value.trim() || '';
}

function filesDownloadSelectedPeerId() {
  return document.getElementById('files-download-host')?.value.trim() || '';
}

function filesDownloadSelectedPeer() {
  const id = filesDownloadSelectedPeerId();
  if (!id) return null;
  return daemons.find(d => d.host_id === id) || null;
}

function filesDownloadPeerLabel(peerId = filesDownloadSelectedPeerId()) {
  const id = String(peerId || '').trim();
  if (!id) return 'This daemon';
  const peer = daemons.find(d => d.host_id === id);
  return peer?.label || id;
}

function peerFileTransferSignalAvailable(peerId = filesDownloadSelectedPeerId()) {
  const id = String(peerId || '').trim();
  if (!id) return false;
  if (!window.RTCPeerConnection) return false;
  const peer = daemons.find(d => d.host_id === id);
  if (!peer || peer.connected === false) return false;
  if (dashboardConnectModeEnabled()) {
    return Boolean(
      dashboardTransport?.canUseRpc?.() &&
      dashboardControlTransport?.lastStatus?.api_peer_file_transfer_signal_available === true
    );
  }
  return true;
}

function refreshFilesDownloadHostOptions() {
  const select = document.getElementById('files-download-host');
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
  const stillPresent = options.some(option => option.id === previous);
  select.value = stillPresent ? previous : '';
  onFilesDownloadHostChanged({ preserveStatus: true });
  renderDashboardTargetSummary('files-target-summary', filesDownloadSelectedPeerId(), 'files');
  refreshFilesIdeHostOptions();
}

function onFilesDownloadHostChanged(options = {}) {
  const browse = document.getElementById('files-download-browse-btn');
  const selectedPeer = filesDownloadSelectedPeerId();
  if (browse) {
    browse.disabled = Boolean(filesDownloadAbort || selectedPeer);
    browse.title = selectedPeer
      ? 'Remote peer browsing is not available yet; enter a full path'
      : 'Choose a local file to download';
  }
  renderDashboardTargetSummary('files-target-summary', selectedPeer, 'files');
  refreshFilesDownloadAvailability();
  if (!options.preserveStatus) {
    setFilesDownloadStatus('', selectedPeer ? 'Enter a full path on the selected target' : 'Ready');
    setFilesDownloadProgress(0, 0);
  }
}

function filesUploadDestinationValue() {
  return document.getElementById('files-upload-destination')?.value.trim() || '';
}

function filesUploadConflictPolicy() {
  const value = document.getElementById('files-upload-conflict')?.value || 'fail';
  return ['fail', 'rename', 'overwrite'].includes(value) ? value : 'fail';
}

function filesDownloadTunnelAvailable() {
  const peerId = filesDownloadSelectedPeerId();
  if (peerId) return peerDashboardControlSignalAvailable(peerId) || peerFileTransferSignalAvailable(peerId);
  return dashboardTransferDownloadAvailable() ||
    dashboardByteStreamMethodAvailable('api_fs_read') ||
    !dashboardConnectModeEnabled();
}

function filesDownloadUnavailableMessage(peerIdOverride = undefined) {
  const peerId = peerIdOverride === undefined ? filesDownloadSelectedPeerId() : String(peerIdOverride || '').trim();
  if (peerId) {
    if (!window.RTCPeerConnection) return 'Peer downloads require WebRTC support in this browser';
    const peer = daemons.find(d => d.host_id === peerId);
    if (!peer) return 'Selected peer is no longer configured';
    if (peer.connected === false) return 'Selected peer is not connected';
    return dashboardConnectModeEnabled()
      ? 'Peer file access is unavailable until this dashboard reconnects'
      : 'Peer file access is unavailable for this target';
  }
  return dashboardConnectModeEnabled()
    ? 'File access is unavailable until the dashboard reconnects'
    : 'File downloads are not available from this dashboard';
}

function dashboardFilenameFromContentDisposition(value) {
  const text = String(value || '');
  if (!text) return '';
  const star = text.match(/filename\*=UTF-8''([^;]+)/i);
  if (star?.[1]) {
    try {
      return decodeURIComponent(star[1].trim().replace(/^"|"$/g, ''));
    } catch (_) {
      return star[1].trim().replace(/^"|"$/g, '');
    }
  }
  const quoted = text.match(/filename="([^"]+)"/i);
  if (quoted?.[1]) return quoted[1].trim();
  const bare = text.match(/filename=([^;]+)/i);
  return bare?.[1]?.trim()?.replace(/^"|"$/g, '') || '';
}

function filesDownloadFilenameFromPath(path) {
  return String(path || '').split(/[\\/]/).filter(Boolean).pop() || 'download.bin';
}

function dashboardParseHttpContentRange(value, expectedOffset, byteLength) {
  const text = String(value || '').trim();
  const match = text.match(/^bytes\s+(\d+)-(\d+)\/(\d+)$/i);
  if (!match) throw new Error('File download returned an invalid Content-Range header');
  const rangeStart = Number(match[1]);
  const inclusiveEnd = Number(match[2]);
  const totalSize = Number(match[3]);
  const rangeEnd = inclusiveEnd + 1;
  if (!Number.isSafeInteger(rangeStart) || rangeStart !== expectedOffset) {
    throw new Error('File download returned an unexpected range start');
  }
  if (!Number.isSafeInteger(rangeEnd) || rangeEnd < rangeStart || rangeEnd - rangeStart !== byteLength) {
    throw new Error('File download returned an inconsistent range length');
  }
  if (!Number.isSafeInteger(totalSize) || totalSize < rangeEnd) {
    throw new Error('File download returned an invalid total size');
  }
  return { rangeStart, rangeEnd, totalSize };
}

function dashboardParseHttpUnsatisfiedRangeTotal(value) {
  const match = String(value || '').trim().match(/^bytes\s+\*\/(\d+)$/i);
  return match ? Number(match[1]) : null;
}

function filesTransferDb() {
  if (!('indexedDB' in window)) return Promise.reject(new Error('IndexedDB is not available'));
  if (filesTransferDbPromise) return filesTransferDbPromise;
  filesTransferDbPromise = new Promise((resolve, reject) => {
    const req = indexedDB.open(FILES_TRANSFER_DB_NAME, FILES_TRANSFER_DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains('downloadParts')) db.createObjectStore('downloadParts', { keyPath: 'key' });
      if (!db.objectStoreNames.contains('uploadBlobs')) db.createObjectStore('uploadBlobs', { keyPath: 'id' });
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error || new Error('open transfer database failed'));
    req.onblocked = () => reject(new Error('transfer database upgrade blocked'));
  }).catch(err => {
    filesTransferDbPromise = null;
    throw err;
  });
  return filesTransferDbPromise;
}

async function filesTransferStorePut(storeName, value) {
  const db = await filesTransferDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(storeName, 'readwrite');
    tx.objectStore(storeName).put(value);
    tx.oncomplete = () => resolve(true);
    tx.onerror = () => reject(tx.error || new Error(`write ${storeName} failed`));
  });
}

async function filesTransferStoreGet(storeName, key) {
  const db = await filesTransferDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(storeName, 'readonly');
    const req = tx.objectStore(storeName).get(key);
    req.onsuccess = () => resolve(req.result || null);
    req.onerror = () => reject(req.error || new Error(`read ${storeName} failed`));
  });
}

async function filesTransferStoreDelete(storeName, key) {
  const db = await filesTransferDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(storeName, 'readwrite');
    tx.objectStore(storeName).delete(key);
    tx.oncomplete = () => resolve(true);
    tx.onerror = () => reject(tx.error || new Error(`delete ${storeName} failed`));
  });
}

function filesTransferPartKey(id, seq) {
  return `${String(id || '')}:${Number(seq) || 0}`;
}

async function filesTransferPutDownloadPart(transfer, seq, bytes) {
  if (!transfer?.id || !(bytes instanceof Uint8Array)) return false;
  try {
    await filesTransferStorePut('downloadParts', {
      key: filesTransferPartKey(transfer.id, seq),
      transferId: transfer.id,
      seq,
      bytes,
      createdAt: Date.now(),
    });
    return true;
  } catch (err) {
    console.warn('Persist download part failed:', err);
    return false;
  }
}

async function filesTransferLoadDownloadParts(transfer) {
  if (!transfer?.id || !Number(transfer.rangeCount || 0)) return false;
  const parts = [];
  let loaded = 0;
  try {
    for (let seq = 0; seq < Number(transfer.rangeCount || 0); seq += 1) {
      const record = await filesTransferStoreGet('downloadParts', filesTransferPartKey(transfer.id, seq));
      if (!record?.bytes) return false;
      const bytes = record.bytes instanceof Uint8Array ? record.bytes : new Uint8Array(record.bytes);
      parts.push(bytes);
      loaded += bytes.byteLength;
    }
    transfer.parts = parts;
    transfer.loaded = loaded;
    return true;
  } catch (err) {
    console.warn('Load download parts failed:', err);
    return false;
  }
}

async function filesTransferDeleteDownloadParts(id, count = 512) {
  if (!id) return;
  for (let seq = 0; seq < count; seq += 1) {
    try {
      await filesTransferStoreDelete('downloadParts', filesTransferPartKey(id, seq));
    } catch (_) {
      break;
    }
  }
}

async function filesTransferPutUploadBlob(transfer, file) {
  if (!transfer?.id || !(file instanceof Blob)) return false;
  try {
    await filesTransferStorePut('uploadBlobs', {
      id: transfer.id,
      blob: file,
      name: file.name || transfer.name || 'upload.bin',
      type: file.type || transfer.mime || 'application/octet-stream',
      size: Number(file.size || 0),
      createdAt: Date.now(),
    });
    transfer.uploadBlobStored = true;
    filesTransferPersistState();
    return true;
  } catch (err) {
    console.warn('Persist upload blob failed:', err);
    return false;
  }
}

async function filesTransferGetUploadBlob(transfer) {
  if (transfer?.file instanceof Blob) return transfer.file;
  if (!transfer?.id) return null;
  try {
    const record = await filesTransferStoreGet('uploadBlobs', transfer.id);
    if (!record?.blob) return null;
    const blob = record.blob;
    transfer.file = blob;
    transfer.name = record.name || transfer.name || 'upload.bin';
    transfer.mime = record.type || transfer.mime || blob.type || 'application/octet-stream';
    transfer.totalSize = Number(record.size || blob.size || transfer.totalSize || 0);
    return blob;
  } catch (err) {
    console.warn('Load upload blob failed:', err);
    return null;
  }
}

async function filesTransferDeleteUploadBlob(id) {
  if (!id) return;
  try {
    await filesTransferStoreDelete('uploadBlobs', id);
  } catch (_) {}
}

function filesTransferPersistable(transfer) {
  if (!transfer?.id) return null;
  if (transfer.kind === 'upload' && transfer.status === 'completed') return null;
  if (transfer.status === 'cancelled') return null;
  return {
    id: transfer.id,
    kind: transfer.kind,
    status: ['running', 'queued'].includes(transfer.status) ? 'paused' : (transfer.status || 'queued'),
    path: transfer.path || '',
    name: transfer.name || transfer.file?.name || transfer.filename || '',
    destination: transfer.destination || '',
    conflictPolicy: transfer.conflictPolicy || 'fail',
    artifact: transfer.artifact && typeof transfer.artifact === 'object' ? transfer.artifact : null,
    sourceKind: transfer.sourceKind || '',
    sourceLabel: transfer.sourceLabel || '',
    directMethod: transfer.directMethod || '',
    directParams: transfer.directParams && typeof transfer.directParams === 'object' ? transfer.directParams : null,
    peerId: transfer.peerId || '',
    peerLabel: transfer.peerLabel || '',
    loaded: Number(transfer.loaded || 0),
    totalSize: Number(transfer.totalSize || transfer.file?.size || 0),
    rangeCount: Number(transfer.rangeCount || 0),
    filename: transfer.filename || '',
    contentType: transfer.contentType || transfer.mime || 'application/octet-stream',
    chunkBytes: Number(transfer.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES),
    maxBytes: Number(transfer.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES),
    skipBrowserSave: Boolean(transfer.skipBrowserSave),
    serverJobId: transfer.serverJobId || transfer.serverJob?.id || '',
    resumeToken: transfer.resumeToken || transfer.serverJob?.resume_token || '',
    serverJob: transfer.serverJob || null,
    uploadBlobStored: Boolean(transfer.uploadBlobStored || transfer.file instanceof Blob),
    uploadChunkCount: Number(transfer.uploadChunkCount || 0),
    error: transfer.error || '',
  };
}

function filesTransferPersistState() {
  try {
    const state = filesTransfers
      .map(filesTransferPersistable)
      .filter(Boolean)
      .slice(0, 50);
    localStorage.setItem(FILES_TRANSFER_STATE_KEY, JSON.stringify(state));
  } catch (err) {
    console.warn('Persist transfer state failed:', err);
  }
}

function restoreFilesTransferState() {
  let state = [];
  try {
    state = JSON.parse(localStorage.getItem(FILES_TRANSFER_STATE_KEY) || '[]');
  } catch (_) {
    state = [];
  }
  if (!Array.isArray(state)) return;
  for (const item of state) {
    if (!item?.id || filesTransferById(item.id)) continue;
    const transfer = {
      id: String(item.id),
      kind: item.kind === 'upload' ? 'upload' : 'download',
      status: item.status || 'paused',
      path: item.path || '',
      name: item.name || '',
      destination: item.destination || '',
      conflictPolicy: item.conflictPolicy || 'fail',
      artifact: item.artifact && typeof item.artifact === 'object' ? item.artifact : null,
      sourceKind: item.sourceKind || '',
      sourceLabel: item.sourceLabel || '',
      directMethod: item.directMethod || '',
      directParams: item.directParams && typeof item.directParams === 'object' ? item.directParams : null,
      peerId: item.peerId || '',
      peerLabel: item.peerLabel || '',
      loaded: Number(item.loaded || 0),
      totalSize: Number(item.totalSize || 0),
      parts: [],
      rangeCount: Number(item.rangeCount || 0),
      filename: item.filename || item.name || '',
      contentType: item.contentType || 'application/octet-stream',
      chunkBytes: Math.max(1, Number(item.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES)),
      maxBytes: Math.max(1, Number(item.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES)),
      skipBrowserSave: Boolean(item.skipBrowserSave),
      serverJobId: item.serverJobId || item.serverJob?.id || '',
      resumeToken: item.resumeToken || item.serverJob?.resume_token || '',
      serverJob: item.serverJob || null,
      uploadBlobStored: Boolean(item.uploadBlobStored),
      uploadChunkCount: Number(item.uploadChunkCount || 0),
      result: null,
      error: item.error || '',
      pauseRequested: false,
      cancelRequested: false,
      abortController: null,
    };
    filesTransferFsmInit(transfer, { actor: 'restore' });
    filesTransfers.push(transfer);
  }
}

function filesTransferById(id) {
  const key = String(id || '').trim();
  return filesTransfers.find(transfer => transfer.id === key) || null;
}

function setFilesDownloadPath(path) {
  const input = document.getElementById('files-download-path');
  if (!input) return;
  input.value = String(path || '');
  input.title = input.value;
  refreshFilesDownloadAvailability();
}

function setFilesDownloadStatus(kind, text) {
  const el = document.getElementById('files-download-status');
  if (!el) return;
  el.className = 'files-download-status' + (kind ? ` ${kind}` : '');
  el.textContent = text || '';
  el.title = text || '';
}

function setFilesDownloadProgress(loaded = 0, total = 0) {
  const fill = document.getElementById('files-download-progress');
  if (!fill) return;
  const safeTotal = Number(total) > 0 ? Number(total) : 0;
  const percent = safeTotal > 0
    ? Math.max(0, Math.min(100, (Number(loaded) || 0) * 100 / safeTotal))
    : 0;
  fill.style.width = `${percent}%`;
}

function setFilesDownloadBusy(busy) {
  const input = document.getElementById('files-download-path');
  const browse = document.getElementById('files-download-browse-btn');
  const download = document.getElementById('files-download-btn');
  const host = document.getElementById('files-download-host');
  const meter = document.getElementById('files-download-meter');
  const peerSelected = !!filesDownloadSelectedPeerId();
  const hasPath = !!filesDownloadPathValue();
  if (input) input.disabled = !!busy;
  if (host) host.disabled = !!busy;
  if (browse) browse.disabled = !!busy || peerSelected;
  if (download) download.disabled = !!busy || !hasPath || !filesDownloadTunnelAvailable();
  // Idle meter reads as a stray gray bar under the card — show it only
  // while a download is actually running.
  if (meter) meter.classList.toggle('hidden', !busy);
}

function refreshFilesDownloadAvailability() {
  if (filesDownloadAbort) {
    setFilesDownloadBusy(true);
    return;
  }
  const available = filesDownloadTunnelAvailable();
  setFilesDownloadBusy(false);
  renderDashboardTargetSummary('files-target-summary', filesDownloadSelectedPeerId(), 'files');
  const statusEl = document.getElementById('files-download-status');
  if (!available && statusEl && !statusEl.textContent) {
    setFilesDownloadStatus('warn', filesDownloadUnavailableMessage());
  } else if (available && statusEl?.textContent === filesDownloadUnavailableMessage()) {
    setFilesDownloadStatus('', '');
  }
}

async function dashboardFetchHttpRangeBytes(path, offset, length, options = {}) {
  const target = String(path || '').trim();
  if (!target) throw new Error('Choose a file to download');
  const rangeStart = Math.max(0, Math.floor(Number(offset) || 0));
  const requested = Math.max(1, Math.floor(Number(length) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES));
  const rangeEndInclusive = rangeStart + requested - 1;
  const response = await authedFetch('/api/fs/read?path=' + encodeURIComponent(target), {
    cache: 'no-store',
    headers: { Range: `bytes=${rangeStart}-${rangeEndInclusive}` },
    signal: options.signal,
  });
  const contentType = response.headers.get('content-type') || 'application/octet-stream';
  const filename = dashboardFilenameFromContentDisposition(response.headers.get('content-disposition')) ||
    filesDownloadFilenameFromPath(target);
  if (response.status === 416) {
    const total = dashboardParseHttpUnsatisfiedRangeTotal(response.headers.get('content-range'));
    if (rangeStart === 0 && total === 0) {
      return {
        bytes: new Uint8Array(),
        rangeStart: 0,
        rangeEnd: 0,
        totalSize: 0,
        filename,
        contentType,
      };
    }
  }
  if (!response.ok) {
    let message = `File download failed (${response.status})`;
    try {
      const body = await response.json();
      if (body?.error) message = String(body.error);
    } catch (_) {}
    throw new Error(message);
  }
  const blob = await response.blob();
  const bytes = new Uint8Array(await blob.arrayBuffer());
  let parsed;
  if (response.status === 206) {
    parsed = dashboardParseHttpContentRange(response.headers.get('content-range'), rangeStart, bytes.byteLength);
  } else if (response.status === 200 && rangeStart === 0) {
    const declaredSize = Number(response.headers.get('content-length') || bytes.byteLength);
    parsed = {
      rangeStart: 0,
      rangeEnd: bytes.byteLength,
      totalSize: Number.isSafeInteger(declaredSize) ? declaredSize : bytes.byteLength,
    };
  } else {
    throw new Error(`File download returned unexpected HTTP ${response.status}`);
  }
  return {
    bytes,
    rangeStart: parsed.rangeStart,
    rangeEnd: parsed.rangeEnd,
    totalSize: parsed.totalSize,
    filename,
    contentType,
  };
}

async function dashboardFetchHttpRangeBytesWithRetry(path, offset, length, options = {}) {
  const retries = Number.isFinite(Number(options.retries)) ? Math.max(0, Number(options.retries)) : 2;
  let attempt = 0;
  for (;;) {
    if (options.signal?.aborted) throw dashboardControlAbortError();
    try {
      return await dashboardFetchHttpRangeBytes(path, offset, length, options);
    } catch (err) {
      if (err?.name === 'AbortError' || attempt >= retries) throw err;
      attempt += 1;
      await new Promise(resolve => setTimeout(resolve, 200 * attempt));
    }
  }
}

async function dashboardFetchPeerFileRangeBytesWithRetry(connection, path, offset, length, options = {}) {
  if (!connection || typeof connection.readRange !== 'function') {
    throw new Error('peer file-transfer connection is not available');
  }
  if (options.signal?.aborted) throw dashboardControlAbortError('peer file-transfer read aborted');
  const range = await connection.readRange(path, offset, length, options);
  if (!Number.isSafeInteger(range.rangeStart) || range.rangeStart !== offset) {
    throw new Error('peer file-transfer returned unexpected range start');
  }
  if (!Number.isSafeInteger(range.rangeEnd) || range.rangeEnd < range.rangeStart || range.rangeEnd - range.rangeStart !== range.bytes.byteLength) {
    throw new Error('peer file-transfer returned inconsistent range length');
  }
  if (!Number.isSafeInteger(range.totalSize) || range.totalSize < range.rangeEnd) {
    throw new Error('peer file-transfer returned invalid total size');
  }
  return range;
}

async function dashboardFetchPeerDashboardRangeBytesWithRetry(connection, path, offset, length, options = {}) {
  if (!connection || typeof connection.requestBytes !== 'function') {
    throw new Error('peer dashboard-control connection is not available');
  }
  if (options.signal?.aborted) throw dashboardControlAbortError('peer dashboard-control read aborted');
  const retries = Number.isFinite(Number(options.retries)) ? Math.max(0, Number(options.retries)) : 2;
  let attempt = 0;
  let raw;
  for (;;) {
    if (options.signal?.aborted) throw dashboardControlAbortError('peer dashboard-control read aborted');
    try {
      raw = await connection.requestBytes('api_fs_read', {
        path,
        offset,
        length,
      }, {
        signal: options.signal,
        timeoutMs: options.timeoutMs || rangedDownloadTimeoutMs(length),
      });
      break;
    } catch (err) {
      if (err?.name === 'AbortError' || attempt >= retries) throw err;
      attempt += 1;
      await new Promise(resolve => setTimeout(resolve, 200 * attempt));
    }
  }
  const range = dashboardNormalizeByteRangeResult('api_fs_read', raw, offset);
  if (!Number.isSafeInteger(range.rangeStart) || range.rangeStart !== offset) {
    throw new Error('peer dashboard-control returned unexpected range start');
  }
  if (!Number.isSafeInteger(range.rangeEnd) || range.rangeEnd < range.rangeStart || range.rangeEnd - range.rangeStart !== range.bytes.byteLength) {
    throw new Error('peer dashboard-control returned inconsistent range length');
  }
  if (!Number.isSafeInteger(range.totalSize) || range.totalSize < range.rangeEnd) {
    throw new Error('peer dashboard-control returned invalid total size');
  }
  return range;
}

async function fetchDashboardFilesystemDownload(path, options = {}) {
  const target = String(path || '').trim();
  if (!target) throw new Error('Choose a file to download');
  const peerId = options.peerId !== undefined ? String(options.peerId || '').trim() : filesDownloadSelectedPeerId();
  if (peerId) {
    const transfer = queueFilesDownload(target, {
      signal: options.signal,
      chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
      maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
      retries: options.retries,
      timeoutMs: options.timeoutMs,
      onProgress: options.onProgress,
      skipBrowserSave: true,
      peerId,
      peerLabel: options.peerLabel || filesDownloadPeerLabel(peerId),
    });
    if (!transfer) throw new Error('download was not queued');
    if (options.signal) {
      const abortQueuedTransfer = () => {
        transfer.cancelRequested = true;
        transfer.abortController?.abort();
      };
      if (options.signal.aborted) abortQueuedTransfer();
      else options.signal.addEventListener('abort', abortQueuedTransfer, { once: true });
    }
    return transfer.completion;
  }
  if (dashboardTransferDownloadAvailable()) {
    const transfer = queueFilesDownload(target, {
      signal: options.signal,
      chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
      maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
      retries: options.retries,
      timeoutMs: options.timeoutMs,
      onProgress: options.onProgress,
      skipBrowserSave: true,
    });
    if (!transfer) throw new Error('download was not queued');
    if (options.signal) {
      const abortQueuedTransfer = () => {
        transfer.cancelRequested = true;
        transfer.abortController?.abort();
      };
      if (options.signal.aborted) abortQueuedTransfer();
      else options.signal.addEventListener('abort', abortQueuedTransfer, { once: true });
    }
    return transfer.completion;
  }
  if (dashboardByteStreamMethodAvailable('api_fs_read')) {
    return dashboardFetchRangedBytes('api_fs_read', { path: target }, {
      signal: options.signal,
      chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
      maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
      retries: options.retries,
      timeoutMs: options.timeoutMs,
      onProgress: options.onProgress,
    });
  }
  if (!dashboardConnectModeEnabled()) {
    const transfer = queueFilesDownload(target, {
      signal: options.signal,
      chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
      maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
      retries: options.retries,
      timeoutMs: options.timeoutMs,
      onProgress: options.onProgress,
      skipBrowserSave: true,
    });
    if (!transfer) throw new Error('download was not queued');
    if (options.signal) {
      const abortQueuedTransfer = () => {
        transfer.cancelRequested = true;
        transfer.abortController?.abort();
      };
      if (options.signal.aborted) abortQueuedTransfer();
      else options.signal.addEventListener('abort', abortQueuedTransfer, { once: true });
    }
    return transfer.completion;
  }
  throw new Error(filesDownloadUnavailableMessage());
}

function filesTransferCompletion(transfer) {
  transfer.completion = new Promise((resolve, reject) => {
    transfer.resolve = resolve;
    transfer.reject = reject;
  });
  transfer.completion.catch(() => {});
  return transfer.completion;
}

function filesTransferProgressText(transfer) {
  const loaded = Number(transfer.loaded || 0);
  const total = Number(transfer.totalSize || transfer.file?.size || 0);
  const suffix = total > 0 ? ` / ${humanBytes(total)}` : '';
  return `${humanBytes(loaded)}${suffix}`;
}

function filesTransferTitle(transfer) {
  if (transfer.kind === 'upload') {
    return transfer.file?.name || transfer.name || 'upload.bin';
  }
  return transfer.sourceLabel || transfer.filename || transfer.path || 'download';
}

function filesTransferStatusLabel(status) {
  return {
    queued: 'queued',
    running: 'running',
    paused: 'paused',
    ready: 'ready',
    completed: 'done',
    failed: 'failed',
    cancelled: 'cancelled',
  }[status] || status || 'queued';
}

function filesTransferStatusFromJob(job) {
  const status = String(job?.status || 'queued');
  if (status === 'ready') return 'ready';
  return ['queued', 'running', 'paused', 'completed', 'failed', 'cancelled'].includes(status)
    ? status
    : 'queued';
}

function filesTransferKindFromJob(job) {
  return String(job?.kind || '').toLowerCase() === 'upload' ? 'upload' : 'download';
}

function filesTransferPathText(value) {
  if (typeof value === 'string') return value;
  if (value == null) return '';
  return String(value);
}

function filesTransferMergeServerJob(job) {
  if (!job || typeof job !== 'object') return null;
  const jobId = String(job.id || '').trim();
  const resumeToken = String(job.resume_token || '').trim();
  const existing = filesTransfers.find(item =>
    (jobId && (item.serverJobId === jobId || item.id === jobId)) ||
    (resumeToken && item.resumeToken === resumeToken)
  );
  const kind = filesTransferKindFromJob(job);
  const source = filesTransferPathText(job.source_path);
  const sourceLabel = filesTransferPathText(job.source_label || job.sourceLabel);
  const destination = filesTransferPathText(job.destination_path || job.final_path);
  const transfer = existing || {
    id: `server-${jobId || resumeToken || Date.now()}`,
    kind,
    status: filesTransferStatusFromJob(job),
    path: source,
    name: job.original_name || job.filename || sourceLabel || '',
    destination,
    conflictPolicy: job.conflict_policy || 'fail',
    artifact: job.artifact && typeof job.artifact === 'object' ? job.artifact : null,
    sourceKind: job.source_kind || job.sourceKind || '',
    sourceLabel,
    loaded: 0,
    totalSize: 0,
    parts: [],
    rangeCount: 0,
    filename: job.filename || '',
    contentType: job.mime || 'application/octet-stream',
    chunkBytes: DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
    maxBytes: DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
    result: null,
    error: '',
    pauseRequested: false,
    cancelRequested: false,
    abortController: null,
  };
  transfer.kind = kind;
  if (existing && !['queued', 'running', 'paused', 'failed'].includes(existing.status || '')) {
    // A row this tab does not actively own mirrors the server job's status.
    // (A brand-new row was already minted with it, so no self-transition.)
    filesTransferTransition(transfer, filesTransferStatusFromJob(job), {
      actor: 'server',
      error: null,     // job.error merges below, after the status move
      persist: false,  // refreshFilesTransferJobs persists + renders once per batch
      render: false,
    });
  }
  transfer.path = transfer.path || source;
  transfer.destination = transfer.destination || destination;
  transfer.filename = transfer.filename || job.filename || '';
  transfer.contentType = transfer.contentType || job.mime || 'application/octet-stream';
  transfer.name = transfer.name || job.original_name || job.filename || sourceLabel || '';
  transfer.sourceLabel = transfer.sourceLabel || sourceLabel;
  transfer.sourceKind = transfer.sourceKind || job.source_kind || job.sourceKind || '';
  if (!transfer.artifact && job.artifact && typeof job.artifact === 'object') transfer.artifact = job.artifact;
  transfer.error = job.error || transfer.error || '';
  filesTransferApplyServerJob(transfer, job);
  if (!existing) {
    filesTransferFsmInit(transfer, { actor: 'server' });
    filesTransfers.unshift(transfer);
  }
  return transfer;
}

async function refreshFilesTransferJobs() {
  if (dashboardControlTransport?.lastStatus?.api_transfer_jobs_available !== true) {
    renderFilesTransfers();
    return [];
  }
  try {
    const result = await dashboardTransport.request('api_transfer_jobs', {}, { timeoutMs: 60000 });
    if (result?.ok === false || result?._httpOk === false) {
      throw new Error(result.error || `transfer jobs returned ${result._httpStatus || 'error'}`);
    }
    const jobs = Array.isArray(result.jobs) ? result.jobs : [];
    for (const job of jobs) filesTransferMergeServerJob(job);
    filesTransferPersistState();
    renderFilesTransfers();
    return jobs;
  } catch (err) {
    console.warn('Refresh transfer jobs failed:', err);
    return [];
  }
}

// Finished transfers accumulate for the lifetime of the page; keep the
// list bounded (active/queued/paused transfers are never pruned).
// FILES_TRANSFER_TERMINAL_STATUSES lives in 53-transfer-fsm.js.
const FILES_TRANSFER_HISTORY_LIMIT = 100;
function pruneFinishedFilesTransfers() {
  let terminal = 0;
  for (let i = 0; i < filesTransfers.length; i++) {
    if (!FILES_TRANSFER_TERMINAL_STATUSES.has(filesTransfers[i].status)) continue;
    terminal++;
    if (terminal > FILES_TRANSFER_HISTORY_LIMIT) {
      // Newest sit at the front (unshift); everything from here back that
      // is terminal is the oldest history — drop it.
      for (let j = filesTransfers.length - 1; j >= i; j--) {
        if (FILES_TRANSFER_TERMINAL_STATUSES.has(filesTransfers[j].status)) {
          filesTransfers.splice(j, 1);
        }
      }
      break;
    }
  }
}

function renderFilesTransfers() {
  // Progress ticks re-render the whole list; while the Files pane is
  // hidden, remember one redraw for the next entry instead — and while
  // visible, coalesce bursts (each chunk of a download fires a render)
  // into one rebuild per frame.
  if (!paneIsVisible('files')) {
    renderOrDefer('files', 'transfers', renderFilesTransfersNow);
    return;
  }
  if (filesTransfersRenderScheduled) return;
  filesTransfersRenderScheduled = true;
  requestAnimationFrame(() => {
    filesTransfersRenderScheduled = false;
    renderFilesTransfersNow();
  });
}
function renderFilesTransfersNow() {
  pruneFinishedFilesTransfers();
  const list = document.getElementById('files-transfer-list');
  if (!list) return;
  list.innerHTML = '';
  if (filesTransfers.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'files-transfer-empty ui-empty compact';
    empty.innerHTML = '<div class="ui-empty-title">No transfers</div>' +
      '<div class="ui-empty-hint">Downloads and uploads show up here with live progress.</div>';
    list.appendChild(empty);
    return;
  }
  for (const transfer of filesTransfers) {
    const row = document.createElement('div');
    row.className = `files-transfer-row ${transfer.status || 'queued'}${transfer.serverJobId || transfer.resumeToken ? ' server' : ''}`;
    row.dataset.transferId = transfer.id;
    const main = document.createElement('div');
    main.className = 'files-transfer-main';
    const title = document.createElement('div');
    title.className = 'files-transfer-title';
    title.textContent = filesTransferTitle(transfer);
    title.title = transfer.sourceLabel || transfer.path || transfer.file?.name || '';
    const meta = document.createElement('div');
    meta.className = 'files-transfer-meta';
    const direction = transfer.kind === 'upload' ? 'upload' : 'download';
    const range = transfer.kind === 'download' && transfer.rangeCount
      ? ` · ${transfer.rangeCount} range${transfer.rangeCount === 1 ? '' : 's'}`
      : '';
    const error = transfer.error ? ` · ${transfer.error}` : '';
    const destination = transfer.kind === 'upload' && transfer.destination ? ` · ${transfer.destination}` : '';
    const sourceText = transfer.kind === 'download' ? (transfer.sourceLabel || transfer.path || '') : '';
    const source = sourceText ? ` · ${sourceText}` : '';
    meta.textContent = `${direction} · ${filesTransferStatusLabel(transfer.status)} · ${filesTransferProgressText(transfer)}${range}${destination}${source}${error}`;
    const meter = document.createElement('div');
    meter.className = 'files-transfer-meter';
    const fill = document.createElement('div');
    fill.className = 'files-transfer-meter-fill';
    const total = Number(transfer.totalSize || transfer.file?.size || 0);
    const loaded = Number(transfer.loaded || 0);
    fill.style.width = total > 0 ? `${Math.max(0, Math.min(100, loaded * 100 / total))}%` : '0%';
    meter.appendChild(fill);
    main.append(title, meta, meter);
    const actions = document.createElement('div');
    actions.className = 'files-transfer-actions';
    const addAction = (label, handler, className = '') => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.textContent = label;
      btn.className = className ? `ui-btn ${className}` : 'ui-btn';
      btn.addEventListener('click', () => handler(transfer.id));
      actions.appendChild(btn);
    };
    const canResumeUpload = transfer.kind !== 'upload' || transfer.file || transfer.uploadBlobStored;
    if (transfer.status === 'running' && transfer.kind === 'download') addAction('Pause', pauseFilesTransfer);
    if (transfer.status === 'running') addAction('Cancel', cancelFilesTransfer, 'danger');
    if (transfer.status === 'queued') addAction('Cancel', cancelFilesTransfer, 'danger');
    if (transfer.status === 'paused' && canResumeUpload) addAction('Resume', resumeFilesTransfer);
    if (transfer.status === 'failed' && transfer.kind === 'download' && transfer.loaded > 0) addAction('Resume', resumeFilesTransfer);
    if (['failed', 'cancelled'].includes(transfer.status)) addAction('Retry', retryFilesTransfer);
    if (transfer.status === 'completed' && transfer.kind === 'download' && transfer.result?.blob) {
      addAction('Save', () => downloadDashboardBlob(transfer.result.blob, transfer.result.filename, transfer.result.content_type));
    } else if (transfer.status === 'completed' && transfer.kind === 'download' && transfer.rangeCount > 0) {
      addAction('Save', saveCompletedFilesDownload);
    }
    row.append(main, actions);
    list.appendChild(row);
  }
}

function filesUpdateActiveDownloadSummary(transfer = null) {
  const active = transfer || filesTransfers.find(item => item.kind === 'download' && item.status === 'running');
  if (!active) {
    setFilesDownloadBusy(false);
    return;
  }
  const total = active.totalSize || 0;
  setFilesDownloadBusy(true);
  setFilesDownloadProgress(active.loaded, total);
  setFilesDownloadStatus('warn', `Downloading ${filesTransferProgressText(active)}`);
}

function queueFilesTransfer(transfer) {
  filesTransferFsmInit(transfer, { actor: 'user' });
  filesTransfers.unshift(transfer);
  filesTransferPersistState();
  renderFilesTransfers();
  pumpFilesTransfers();
  return transfer;
}

function queueFilesDownload(path, options = {}) {
  const target = String(path || '').trim();
  if (!target) {
    setFilesDownloadStatus('error', 'Choose a file to download');
    return null;
  }
  const peerId = String(options.peerId || '').trim();
  const peerLabel = peerId ? String(options.peerLabel || filesDownloadPeerLabel(peerId)).trim() : '';
  setFilesDownloadPath(target);
  setFilesDownloadStatus('warn', 'Queued download');
  setFilesDownloadProgress(0, 0);
  const transfer = {
    id: `download-${Date.now()}-${++filesTransferSeq}`,
    kind: 'download',
    status: 'queued',
    path: target,
    peerId,
    peerLabel,
    sourceLabel: peerId ? `${peerLabel || peerId}:${target}` : '',
    loaded: 0,
    totalSize: 0,
    parts: [],
    rangeCount: 0,
    filename: '',
    contentType: 'application/octet-stream',
    chunkBytes: Math.max(1, Math.floor(Number(options.chunkBytes) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES)),
    maxBytes: Math.max(1, Math.floor(Number(options.maxBytes) || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES)),
    retries: options.retries,
    timeoutMs: options.timeoutMs,
    onProgress: typeof options.onProgress === 'function' ? options.onProgress : null,
    skipBrowserSave: !!options.skipBrowserSave,
    debugFailAfterRanges: Math.max(0, Math.floor(Number(options.debugFailAfterRanges || options.failAfterRanges) || 0)),
    debugFailedOnce: false,
    result: null,
    error: '',
    pauseRequested: false,
    cancelRequested: false,
    abortController: null,
  };
  return queueFilesTransfer(transfer);
}

function queueDashboardArtifactDownload(artifact, options = {}) {
  if (!artifact || typeof artifact !== 'object') {
    setFilesDownloadStatus('error', 'Choose an artifact to download');
    return null;
  }
  const sourceLabel = String(options.sourceLabel || options.label || options.filename || 'Dashboard artifact').trim();
  setFilesDownloadStatus('warn', `Queued ${sourceLabel}`);
  setFilesDownloadProgress(0, 0);
  const transfer = {
    id: `artifact-download-${Date.now()}-${++filesTransferSeq}`,
    kind: 'download',
    status: 'queued',
    path: '',
    artifact,
    sourceKind: String(artifact.type || artifact.kind || options.sourceKind || 'artifact'),
    sourceLabel,
    directMethod: options.directMethod || '',
    directParams: options.directParams && typeof options.directParams === 'object' ? options.directParams : null,
    loaded: 0,
    totalSize: 0,
    parts: [],
    rangeCount: 0,
    filename: options.filename || '',
    contentType: options.contentType || options.mime || 'application/octet-stream',
    chunkBytes: Math.max(1, Math.floor(Number(options.chunkBytes) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES)),
    maxBytes: Math.max(1, Math.floor(Number(options.maxBytes) || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES)),
    retries: options.retries,
    timeoutMs: options.timeoutMs,
    onProgress: typeof options.onProgress === 'function' ? options.onProgress : null,
    skipBrowserSave: !!options.skipBrowserSave,
    debugFailAfterRanges: Math.max(0, Math.floor(Number(options.debugFailAfterRanges || options.failAfterRanges) || 0)),
    debugFailedOnce: false,
    result: null,
    error: '',
    pauseRequested: false,
    cancelRequested: false,
    abortController: null,
  };
  return queueFilesTransfer(transfer);
}

function resetFilesDownloadTransfer(transfer) {
  transfer.loaded = 0;
  transfer.totalSize = 0;
  transfer.parts = [];
  transfer.rangeCount = 0;
  transfer.filename = '';
  transfer.contentType = 'application/octet-stream';
  transfer.result = null;
  transfer.error = '';
  transfer.pauseRequested = false;
  transfer.cancelRequested = false;
  transfer.abortController = null;
  transfer.debugFailedOnce = false;
  filesTransferDeleteDownloadParts(transfer.id, 512);
  filesTransferPersistState();
}

function filesTransferApplyServerJob(transfer, job) {
  if (!transfer || !job || typeof job !== 'object') return;
  transfer.serverJob = job;
  transfer.serverJobId = String(job.id || transfer.serverJobId || '');
  transfer.resumeToken = String(job.resume_token || transfer.resumeToken || '');
  if (job.status) transfer.serverStatus = String(job.status);
  if (job.total_size != null) transfer.totalSize = Number(job.total_size) || transfer.totalSize || 0;
  if (job.completed_bytes != null) transfer.loaded = Math.max(Number(transfer.loaded || 0), Number(job.completed_bytes || 0));
  if (job.filename && !transfer.filename) transfer.filename = String(job.filename);
  if (job.mime && !transfer.contentType) transfer.contentType = String(job.mime);
  if (job.source_path && !transfer.path) transfer.path = String(job.source_path);
  if (job.source_label && !transfer.sourceLabel) transfer.sourceLabel = String(job.source_label);
  if (job.source_kind && !transfer.sourceKind) transfer.sourceKind = String(job.source_kind);
  if (!transfer.artifact && job.artifact && typeof job.artifact === 'object') transfer.artifact = job.artifact;
  if (job.destination_path && !transfer.destination) transfer.destination = String(job.destination_path);
  for (let i = filesTransfers.length - 1; i >= 0; i -= 1) {
    const other = filesTransfers[i];
    if (other === transfer) continue;
    const sameJob = transfer.serverJobId && other.serverJobId === transfer.serverJobId;
    const sameToken = transfer.resumeToken && other.resumeToken === transfer.resumeToken;
    if (sameJob || sameToken) {
      filesTransfers.splice(i, 1);
    }
  }
}

async function runFilesDownloadTransfer(transfer) {
  // Mark the transfer running before any transport checks: an early throw
  // that left the status at 'queued' would make pumpFilesTransfers re-pick
  // the same entry forever, so every failure must flow through the catch
  // below. The transport guards therefore live inside the try.
  filesTransferTransition(transfer, 'running', { actor: 'runner' });
  const controller = new AbortController();
  transfer.abortController = controller;
  transfer.transport = '';
  filesDownloadAbort = controller;
  filesUpdateActiveDownloadSummary(transfer);
  let peerConnection = null;
  let peerDashboardConnection = null;
  try {
    const peerId = String(transfer.peerId || '').trim();
    let usePeerDashboardControl = Boolean(peerId) && peerDashboardControlSignalAvailable(peerId);
    let usePeerFileTransfer = Boolean(peerId) && !usePeerDashboardControl;
    const useDurableTransfer = !peerId && dashboardTransferDownloadAvailable();
    const hasArtifact = Boolean(transfer.artifact && typeof transfer.artifact === 'object');
    const fallbackMethod = String(transfer.directMethod || (!hasArtifact ? 'api_fs_read' : '')).trim();
    const canUseFallbackMethod = !peerId && Boolean(fallbackMethod && dashboardByteStreamMethodAvailable(fallbackMethod));
    const useHttpFilesystemFallback = !useDurableTransfer &&
      !canUseFallbackMethod &&
      !peerId &&
      !hasArtifact &&
      !dashboardConnectModeEnabled();
    // Staged uploads are also served over plain HTTP, so a direct-connected
    // dashboard without the control channel can still download them.
    const useHttpStagedRawFallback = !useDurableTransfer &&
      !canUseFallbackMethod &&
      !peerId &&
      hasArtifact &&
      transfer.artifact.type === 'staged_upload' &&
      String(transfer.artifact.id || '').trim() !== '' &&
      !dashboardConnectModeEnabled();
    if (peerId && !usePeerDashboardControl && !peerFileTransferSignalAvailable(peerId)) {
      throw new Error(filesDownloadUnavailableMessage(peerId));
    }
    if (!peerId && !useDurableTransfer && !canUseFallbackMethod && !useHttpFilesystemFallback && !useHttpStagedRawFallback) {
      throw new Error(hasArtifact
        ? 'Artifact download is unavailable until resumable file access is ready'
        : filesDownloadUnavailableMessage());
    }
    if (useHttpStagedRawFallback) {
      await runFilesStagedRawHttpDownload(transfer, controller);
      return;
    }
    if (transfer.loaded > 0 && (!Array.isArray(transfer.parts) || transfer.parts.length === 0)) {
      const hydrated = await filesTransferLoadDownloadParts(transfer);
      if (!hydrated) resetFilesDownloadTransfer(transfer);
    }
    if (usePeerDashboardControl) {
      try {
        peerDashboardConnection = await peerDashboardControlConnectionForHost(peerId, {
          signal: controller.signal,
          timeoutMs: transfer.timeoutMs || 30000,
        });
        transfer.transport = 'peer-dashboard-control';
        transfer.peerDashboardControlSessionId = peerDashboardConnection.sessionId;
      } catch (err) {
        if (!peerFileTransferSignalAvailable(peerId)) throw err;
        console.warn('[peer-dashboard-control] file download tunnel failed, falling back to peer file-transfer', err);
        usePeerDashboardControl = false;
        usePeerFileTransfer = true;
      }
    }
    if (usePeerFileTransfer) {
      peerConnection = new PeerFileTransferConnection(peerId, generateSessionId());
      transfer.transport = 'peer-file-transfer';
      transfer.peerTransferSessionId = peerConnection.sessionId;
      await peerConnection.connect({
        signal: controller.signal,
        timeoutMs: transfer.timeoutMs || 30000,
      });
    }
    if (useDurableTransfer && !transfer.resumeToken && !transfer.serverJobId) {
      transfer.transport = 'durable-transfer';
      const payload = hasArtifact
        ? {
            kind: 'download',
            artifact: transfer.artifact,
          }
        : {
            kind: 'download',
            path: transfer.path,
          };
      const created = await dashboardTransport.request('api_transfer_job_create', payload, {
        timeoutMs: transfer.timeoutMs || 120000,
        signal: controller.signal,
      });
      if (created?.ok === false || created?._httpOk === false) {
        throw new Error(created.error || `transfer create returned ${created._httpStatus || 'error'}`);
      }
      filesTransferApplyServerJob(transfer, created.job);
      filesTransferPersistState();
    }
    for (;;) {
      if (transfer.pauseRequested) throw dashboardControlAbortError('download paused');
      if (transfer.cancelRequested) throw dashboardControlAbortError('download cancelled');
      const remaining = transfer.totalSize > 0 ? Math.max(0, transfer.totalSize - transfer.loaded) : transfer.chunkBytes;
      const requested = Math.min(transfer.chunkBytes, remaining || transfer.chunkBytes);
      if (requested <= 0) break;
      const method = useDurableTransfer ? 'api_transfer_download_read' : fallbackMethod;
      let range;
      if (useHttpFilesystemFallback) {
        transfer.transport = transfer.transport || 'http-filesystem';
        range = await dashboardFetchHttpRangeBytesWithRetry(transfer.path, transfer.loaded, requested, {
          signal: controller.signal,
          retries: transfer.retries,
          timeoutMs: transfer.timeoutMs || rangedDownloadTimeoutMs(requested),
        });
      } else if (usePeerDashboardControl) {
        transfer.transport = transfer.transport || 'peer-dashboard-control';
        range = await dashboardFetchPeerDashboardRangeBytesWithRetry(peerDashboardConnection, transfer.path, transfer.loaded, requested, {
          signal: controller.signal,
          retries: transfer.retries,
          timeoutMs: transfer.timeoutMs || rangedDownloadTimeoutMs(requested),
        });
      } else if (usePeerFileTransfer) {
        transfer.transport = transfer.transport || 'peer-file-transfer';
        range = await dashboardFetchPeerFileRangeBytesWithRetry(peerConnection, transfer.path, transfer.loaded, requested, {
          signal: controller.signal,
          retries: transfer.retries,
          timeoutMs: transfer.timeoutMs || rangedDownloadTimeoutMs(requested),
        });
      } else {
        transfer.transport = transfer.transport || 'dashboard-control';
        const params = useDurableTransfer
          ? {
              id: transfer.serverJobId || undefined,
              resume_token: transfer.resumeToken || undefined,
              offset: transfer.loaded,
              length: requested,
            }
          : {
              ...(transfer.directParams || {}),
              ...(!hasArtifact ? { path: transfer.path } : {}),
              offset: transfer.loaded,
              length: requested,
            };
        const raw = await dashboardRequestBytesWithRetry(method, params, {
          signal: controller.signal,
          retries: transfer.retries,
          timeoutMs: transfer.timeoutMs || rangedDownloadTimeoutMs(requested),
        });
        range = dashboardNormalizeByteRangeResult(method, raw, transfer.loaded);
      }
      if (range.job) filesTransferApplyServerJob(transfer, range.job);
      transfer.totalSize = range.totalSize;
      if (transfer.totalSize > transfer.maxBytes) {
        throw new Error(`Download too large (${humanBytes(transfer.totalSize)}; cap is ${humanBytes(transfer.maxBytes)})`);
      }
      if (!transfer.filename && range.filename) transfer.filename = range.filename;
      if (range.contentType) transfer.contentType = range.contentType;
      await filesTransferPutDownloadPart(transfer, transfer.rangeCount, range.bytes);
      transfer.parts.push(range.bytes);
      transfer.loaded = range.rangeEnd;
      transfer.rangeCount += 1;
      filesTransferPersistState();
      if (typeof transfer.onProgress === 'function') {
        transfer.onProgress({
          loaded: transfer.loaded,
          total: transfer.totalSize,
          offset: transfer.loaded,
          rangeCount: transfer.rangeCount,
          filename: transfer.filename,
          contentType: transfer.contentType,
        });
      }
      renderFilesTransfers();
      filesUpdateActiveDownloadSummary(transfer);
      if (
        transfer.debugFailAfterRanges > 0 &&
        !transfer.debugFailedOnce &&
        transfer.rangeCount >= transfer.debugFailAfterRanges
      ) {
        transfer.debugFailedOnce = true;
        throw new Error('synthetic download interruption');
      }
      if (transfer.loaded >= transfer.totalSize || range.bytes.byteLength === 0) break;
    }
    settleCompletedFilesDownload(transfer, { resumable: true });
  } catch (err) {
    const message = err?.message || String(err);
    if (transfer.cancelRequested) {
      filesTransferTransition(transfer, 'cancelled', { actor: 'runner', error: '', failure: err });
      setFilesDownloadStatus('warn', 'Download cancelled');
    } else if (transfer.pauseRequested || err?.name === 'AbortError') {
      // Non-terminal teardown: the in-flight attempt's promise still
      // rejects (failure) — Resume mints a fresh one.
      filesTransferTransition(transfer, 'paused', { actor: 'runner', error: '', failure: err });
      setFilesDownloadStatus('warn', `Paused at ${filesTransferProgressText(transfer)}`);
    } else {
      filesTransferTransition(transfer, 'failed', { actor: 'runner', error: message, failure: err });
      setFilesDownloadStatus('error', message);
      if (typeof showControlToast === 'function') showControlToast('error', message);
    }
  } finally {
    if (peerConnection) await peerConnection.close().catch(() => {});
    transfer.abortController = null;
    if (filesDownloadAbort === controller) filesDownloadAbort = null;
    renderFilesTransfers();
    filesUpdateActiveDownloadSummary(null);
  }
}

// Shared settle for a fully-downloaded transfer: build the result from the
// accumulated parts, mark completed, and surface it. Both the ranged runner
// and the one-shot staged-raw path end here — keep them from diverging.
function settleCompletedFilesDownload(transfer, { resumable }) {
  const blob = new Blob(transfer.parts, { type: transfer.contentType || 'application/octet-stream' });
  const result = {
    ok: true,
    blob,
    parts: transfer.parts.slice(),
    filename: transfer.filename || 'download.bin',
    content_type: transfer.contentType,
    size: blob.size,
    total_size: transfer.totalSize || blob.size,
    range_start: 0,
    range_end: transfer.loaded,
    range_count: transfer.rangeCount,
    resumable,
  };
  transfer.result = result;
  filesTransferTransition(transfer, 'completed', { actor: 'runner', result });
  setFilesDownloadProgress(result.size, result.total_size || result.size);
  setFilesDownloadStatus('ok', `Downloaded ${result.filename} (${humanBytes(result.size)})`);
  if (!transfer.skipBrowserSave) downloadDashboardBlob(result.blob, result.filename, result.content_type);
  return result;
}

// Server errors like "no project root" are accurate but unactionable in the
// transfers pane; translate the known ones before surfacing. Current daemons
// serve projectless staged uploads/transfers from a daemon-global store, so
// these strings only reach us from older daemons that still refuse.
function filesTransferFriendlyServerError(error, fallback) {
  const message = String(error || '').trim();
  if (!message) return fallback;
  if (message === 'no project root' || message === 'project root unavailable') {
    return 'This daemon predates projectless staged uploads and transfers — it needs a project open (or a daemon upgrade)';
  }
  return message;
}

// Staged-upload artifact download over plain HTTP: one-shot fetch of
// /api/session/current/uploads/{id}/raw (the route streams the whole file;
// staged uploads are bounded by the upload cap, so no ranged loop needed).
async function runFilesStagedRawHttpDownload(transfer, controller) {
  transfer.transport = 'http-staged-raw';
  if (transfer.loaded > 0 || (Array.isArray(transfer.parts) && transfer.parts.length > 0)) {
    resetFilesDownloadTransfer(transfer);
    // reset clears the abort handle; re-arm it so Cancel keeps working
    // while the one-shot fetch below is in flight.
    transfer.abortController = controller;
  }
  const id = String(transfer.artifact?.id || '').trim();
  const resp = await authedFetch(`/api/session/current/uploads/${encodeURIComponent(id)}/raw`, {
    cache: 'no-store',
    signal: dashboardComposeFetchSignal(controller.signal, transfer.timeoutMs || 300000),
  });
  if (!resp.ok) {
    const body = await resp.json().catch(() => ({}));
    throw new Error(filesTransferFriendlyServerError(body.error, `staged upload download returned ${resp.status}`));
  }
  const declared = Number(resp.headers.get('content-length') || 0);
  if (declared > transfer.maxBytes) {
    throw new Error(`Download too large (${humanBytes(declared)}; cap is ${humanBytes(transfer.maxBytes)})`);
  }
  const blob = await resp.blob();
  if (blob.size > transfer.maxBytes) {
    throw new Error(`Download too large (${humanBytes(blob.size)}; cap is ${humanBytes(transfer.maxBytes)})`);
  }
  const bytes = new Uint8Array(await blob.arrayBuffer());
  transfer.contentType = resp.headers.get('content-type') || transfer.contentType || 'application/octet-stream';
  if (!transfer.filename) {
    transfer.filename = dashboardFilenameFromContentDisposition(resp.headers.get('content-disposition')) ||
      transfer.name || 'download.bin';
  }
  await filesTransferPutDownloadPart(transfer, 0, bytes);
  transfer.parts = [bytes];
  transfer.totalSize = bytes.byteLength;
  transfer.loaded = bytes.byteLength;
  transfer.rangeCount = 1;
  settleCompletedFilesDownload(transfer, { resumable: false });
}

// Filesystem upload over plain HTTP: the durable transfer-job RPC needs the
// dashboard-control channel, but /api/fs/write covers the same size range
// (its body cap is sized for UPLOAD_MAX_BYTES of base64 plus envelope), so
// a single guarded write keeps direct-connected uploads working.
async function runFilesFilesystemUploadHttpFallback(transfer, controller) {
  transfer.transport = 'http-fs-write';
  const file = await filesTransferGetUploadBlob(transfer);
  if (!(file instanceof Blob)) {
    throw new Error('Upload file is unavailable after reload');
  }
  if (file.size > UPLOAD_MAX_BYTES) {
    throw new Error(`File too large (${humanBytes(file.size)}; cap is ${humanBytes(UPLOAD_MAX_BYTES)})`);
  }
  transfer.totalSize = Number(file.size || 0);
  transfer.name = transfer.name || file.name || 'upload.bin';
  const conflict = transfer.conflictPolicy || 'fail';
  if (conflict === 'rename') {
    throw new Error('Rename-on-conflict needs the dashboard control channel — choose "Fail on conflict" or "Overwrite" for direct connections');
  }
  const destination = await filesResolveHttpUploadDestinationPath(transfer, file);
  setFilesUploadStatus('warn', `Uploading ${transfer.name} to ${destination}`);
  const bytes = new Uint8Array(await file.arrayBuffer());
  const resp = await filesIdeWriteFile('', destination, bytes, {
    ...(conflict === 'overwrite' ? { force: true } : { create_new: true }),
    signal: dashboardComposeFetchSignal(controller.signal, transfer.timeoutMs || rangedDownloadTimeoutMs(file.size)),
  });
  if (!resp.ok) {
    throw new Error(filesTransferFriendlyServerError(resp.body?.error, `file write returned HTTP ${resp.status}`));
  }
  transfer.loaded = transfer.totalSize;
  filesTransferTransition(transfer, 'completed', {
    actor: 'runner',
    result: { ok: true, path: destination, transport: 'http-fs-write' },
  });
  await filesTransferDeleteUploadBlob(transfer.id);
  setFilesUploadStatus('ok', `Uploaded ${transfer.name} to ${destination}`);
}

// Mirror the durable-job destination semantics client-side: an existing
// directory receives the file under its own name; anything else is treated
// as the target file path.
async function filesResolveHttpUploadDestinationPath(transfer, file) {
  const raw = String(transfer.destination || '').trim();
  if (!raw) throw new Error('missing upload destination');
  const name = String(transfer.name || file.name || 'upload.bin');
  let status;
  try {
    status = await fetchProjectPathStatus(raw);
  } catch (err) {
    // A failed stat is not "does not exist": guessing here either rejects
    // a valid directory destination or writes the file over the directory
    // path itself. Fail retryably instead.
    throw new Error(`Could not verify upload destination ${raw}: ${err?.message || err}`);
  }
  if (status?.exists && status.is_dir) {
    return `${raw.replace(/\/+$/, '')}/${name}`;
  }
  if (!status?.exists && raw.endsWith('/')) {
    throw new Error(`Destination folder does not exist: ${raw}`);
  }
  return raw;
}

async function runFilesFilesystemUploadTransfer(transfer, controller) {
  if (!await ensureDashboardTransferUploadAvailable({ signal: controller.signal })) {
    if (transfer.serverJobId || transfer.resumeToken) {
      // A durable job already holds this upload's chunks server-side.
      // Rerouting to the whole-file fallback would orphan that job (no
      // GC; the Files tab re-merges it as a ghost row forever) and throw
      // away resumability — fail clearly and keep the resume for when
      // the control channel is back.
      throw new Error('Resuming this upload needs the dashboard control channel — retry when it reconnects, or cancel to discard the partial upload');
    }
    if (!dashboardConnectModeEnabled()) {
      return runFilesFilesystemUploadHttpFallback(transfer, controller);
    }
    throw new Error('Filesystem upload is unavailable until file-write access is ready');
  }
  const file = await filesTransferGetUploadBlob(transfer);
  if (!(file instanceof Blob)) {
    throw new Error('Upload file is unavailable after reload');
  }
  if (file.size > UPLOAD_MAX_BYTES) {
    throw new Error(`File too large (${humanBytes(file.size)}; cap is ${humanBytes(UPLOAD_MAX_BYTES)})`);
  }
  transfer.totalSize = Number(file.size || 0);
  transfer.loaded = Number(transfer.loaded || 0);
  transfer.mime = transfer.mime || file.type || 'application/octet-stream';
  transfer.name = transfer.name || file.name || 'upload.bin';
  if (!transfer.resumeToken && !transfer.serverJobId) {
    const created = await dashboardTransport.request('api_transfer_job_create', {
      kind: 'upload',
      destination: transfer.destination,
      name: transfer.name,
      mime: transfer.mime,
      total_size: transfer.totalSize,
      conflict: transfer.conflictPolicy || 'fail',
    }, {
      timeoutMs: transfer.timeoutMs || 120000,
      signal: controller.signal,
    });
    if (created?.ok === false || created?._httpOk === false) {
      throw new Error(created.error || `transfer create returned ${created._httpStatus || 'error'}`);
    }
    filesTransferApplyServerJob(transfer, created.job);
    filesTransferPersistState();
  }
  const chunkBytes = Math.max(1, Math.floor(Number(transfer.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES)));
  while (transfer.loaded < transfer.totalSize) {
    if (transfer.pauseRequested) throw dashboardControlAbortError('upload paused');
    if (transfer.cancelRequested) throw dashboardControlAbortError('upload cancelled');
    const offset = Number(transfer.loaded || 0);
    const end = Math.min(offset + chunkBytes, transfer.totalSize);
    const chunk = file.slice(offset, end);
    const result = await dashboardTransport.uploadBytes('api_transfer_upload_chunk', {
      id: transfer.serverJobId || undefined,
      resume_token: transfer.resumeToken || undefined,
      offset,
    }, chunk, {
      timeoutMs: transfer.timeoutMs || rangedDownloadTimeoutMs(chunk.size || chunkBytes),
      signal: controller.signal,
    });
    if (result?.ok === false || result?._httpOk === false) {
      throw new Error(result.error || `upload chunk returned ${result._httpStatus || 'error'}`);
    }
    filesTransferApplyServerJob(transfer, result.job);
    transfer.loaded = Math.max(Number(transfer.loaded || 0), end);
    transfer.uploadChunkCount = Number(transfer.uploadChunkCount || 0) + 1;
    filesTransferPersistState();
    renderFilesTransfers();
    setFilesUploadStatus('warn', `Uploading ${filesTransferProgressText(transfer)}`);
    if (
      transfer.debugFailAfterChunks > 0 &&
      !transfer.debugFailedOnce &&
      transfer.uploadChunkCount >= transfer.debugFailAfterChunks
    ) {
      transfer.debugFailedOnce = true;
      throw new Error('synthetic upload interruption');
    }
  }
  const committed = await dashboardTransport.request('api_transfer_upload_commit', {
    id: transfer.serverJobId || undefined,
    resume_token: transfer.resumeToken || undefined,
  }, {
    timeoutMs: transfer.timeoutMs || 120000,
    signal: controller.signal,
  });
  if (committed?.ok === false || committed?._httpOk === false) {
    throw new Error(committed.error || `upload commit returned ${committed._httpStatus || 'error'}`);
  }
  filesTransferApplyServerJob(transfer, committed.job);
  transfer.loaded = transfer.totalSize;
  transfer.descriptor = committed.job;
  filesTransferTransition(transfer, 'completed', { actor: 'runner', result: committed.job });
  await filesTransferDeleteUploadBlob(transfer.id);
  setFilesUploadStatus('ok', `Uploaded ${transfer.name || 'upload.bin'}`);
}

function filesDeleteServerTransfer(transfer) {
  if (!transfer?.serverJobId && !transfer?.resumeToken) return;
  if (dashboardControlTransport?.lastStatus?.api_transfer_job_delete_available !== true) return;
  dashboardTransport?.request?.('api_transfer_job_delete', {
    id: transfer.serverJobId || undefined,
    resume_token: transfer.resumeToken || undefined,
  }, { timeoutMs: 30000 }).catch(() => {});
}

async function runFilesUploadTransfer(transfer) {
  // Same rule as downloads: settle the status before any guard can throw,
  // so pumpFilesTransfers never re-picks a permanently 'queued' entry.
  filesTransferTransition(transfer, 'running', { actor: 'runner' });
  transfer.loaded = Number(transfer.loaded || 0);
  transfer.transport = '';
  const controller = new AbortController();
  transfer.abortController = controller;
  try {
    const filesystemUpload = Boolean(transfer.destination && transfer.destination !== 'task');
    if (filesystemUpload && transfer.uploadBlobPersistPromise) {
      setFilesUploadStatus('warn', `Preparing ${transfer.name || 'upload.bin'}`);
      const persisted = await transfer.uploadBlobPersistPromise;
      transfer.uploadBlobPersistPromise = null;
      if (!persisted) {
        throw new Error('Browser storage is unavailable for resumable uploads');
      }
    }
    if (!transfer.file && !(filesystemUpload && transfer.uploadBlobStored)) {
      throw new Error('missing upload file');
    }
    const initialSize = Number(transfer.file?.size || transfer.totalSize || 0);
    if (initialSize > UPLOAD_MAX_BYTES) {
      throw new Error(`File too large (${humanBytes(initialSize)}; cap is ${humanBytes(UPLOAD_MAX_BYTES)})`);
    }
    transfer.totalSize = initialSize;
    if (filesystemUpload) {
      await runFilesFilesystemUploadTransfer(transfer, controller);
    } else {
      if (!transfer.file) throw new Error('missing upload file');
      let descriptor;
      if (dashboardUploadRpcAvailable()) {
        descriptor = await dashboardTransport.uploadBytes('api_session_current_upload', {
          destination: 'task',
          name: transfer.file.name || transfer.name || 'upload.bin',
          mime: transfer.file.type || transfer.mime || 'application/octet-stream',
        }, transfer.file, {
          timeoutMs: transfer.timeoutMs || 120000,
          signal: controller.signal,
        });
        if (descriptor?._httpOk === false) {
          throw new Error(descriptor.error || `upload returned ${descriptor._httpStatus || 'error'}`);
        }
      } else if (!dashboardConnectModeEnabled()) {
        // Direct connection without the dashboard-control channel: POST to
        // the staged-upload HTTP route instead — it streams the body into a
        // tempfile and commits into the same store as the RPC path.
        transfer.transport = 'http-staged-upload';
        const name = transfer.file.name || transfer.name || 'upload.bin';
        setFilesUploadStatus('warn', `Uploading ${name}`);
        const resp = await authedFetch(
          `/api/session/current/uploads?name=${encodeURIComponent(name)}&destination=task`,
          {
            method: 'POST',
            headers: { 'Content-Type': transfer.file.type || transfer.mime || 'application/octet-stream' },
            body: transfer.file,
            signal: dashboardComposeFetchSignal(controller.signal, transfer.timeoutMs || rangedDownloadTimeoutMs(transfer.file.size)),
          }
        );
        const payload = await resp.json().catch(() => null);
        if (!resp.ok || !payload || typeof payload !== 'object') {
          throw new Error(filesTransferFriendlyServerError(payload?.error, `upload returned HTTP ${resp.status}`));
        }
        descriptor = payload;
      } else {
        throw new Error('Upload is unavailable until dashboard access reconnects');
      }
      transfer.loaded = transfer.file.size;
      transfer.descriptor = descriptor;
      filesTransferTransition(transfer, 'completed', { actor: 'runner', result: descriptor });
      filesStagedUploads.set(String(descriptor.id || transfer.id), descriptor);
      setFilesUploadStatus('ok', `Uploaded ${descriptor.name || transfer.file.name || 'upload.bin'}`);
      renderFilesStagedUploads();
    }
  } catch (err) {
    if (transfer.cancelRequested || err?.name === 'AbortError') {
      filesTransferTransition(transfer, 'cancelled', { actor: 'runner', error: '', failure: err });
    } else {
      filesTransferTransition(transfer, 'failed', { actor: 'runner', error: err?.message || String(err), failure: err });
      setFilesUploadStatus('error', transfer.error);
    }
  } finally {
    transfer.abortController = null;
    filesTransferPersistState();
    renderFilesTransfers();
  }
}

async function pumpFilesTransfers() {
  if (filesTransferRunnerActive) return;
  filesTransferRunnerActive = true;
  try {
    for (;;) {
      const transfer = filesTransfers.slice().reverse().find(item => item.status === 'queued');
      if (!transfer) break;
      const epoch = transfer.queueEpoch || 0;
      const failure = transfer.kind === 'upload'
        ? await runFilesUploadTransfer(transfer).then(() => null, err => err)
        : await runFilesDownloadTransfer(transfer).then(() => null, err => err);
      if (
        ['queued', 'running'].includes(transfer.status) &&
        (transfer.queueEpoch || 0) === epoch
      ) {
        // Forward-progress backstop: a runner that exits without settling
        // its transfer would be re-picked by the find() above forever — a
        // synchronous microtask spin that freezes the page and allocates
        // without bound. Force the entry failed so the pump always drains
        // (the transition also rejects the completion promise). The epoch
        // check exempts entries legitimately re-queued (Resume/Retry bump
        // it) while the runner was tearing down.
        filesTransferTransition(transfer, 'failed', {
          actor: 'pump',
          error: failure?.message || transfer.error || 'transfer runner exited without settling',
          failure: failure || undefined,
          reason: 'runner exited without settling its transfer',
        });
      }
      // Macrotask yield: even a misbehaving runner that settles instantly
      // can only busy the tab, never wedge the event loop.
      await new Promise(resolve => setTimeout(resolve, 0));
    }
  } finally {
    filesTransferRunnerActive = false;
  }
}

async function startFilesDownload(options = {}) {
  const selectedPeerId = options.peerId !== undefined ? String(options.peerId || '').trim() : filesDownloadSelectedPeerId();
  const transfer = queueFilesDownload(options.path || filesDownloadPathValue(), {
    ...options,
    peerId: selectedPeerId,
    peerLabel: selectedPeerId ? (options.peerLabel || filesDownloadPeerLabel(selectedPeerId)) : '',
  });
  if (!transfer) return null;
  return options.awaitCompletion || options.skipBrowserSave || options.throwOnError
    ? transfer.completion
    : transfer;
}

function pauseFilesTransfer(id) {
  const transfer = filesTransferById(id);
  if (!transfer) return;
  if (transfer.status === 'queued') {
    // Not started yet: park it in place. The completion promise stays
    // pending until Resume re-arms it — only a runner teardown rejects.
    filesTransferTransition(transfer, 'paused', { actor: 'user' });
    return;
  }
  if (transfer.status !== 'running') return;
  transfer.pauseRequested = true;
  transfer.abortController?.abort();
}

function cancelFilesTransfer(id) {
  const transfer = filesTransferById(id);
  if (!transfer) return;
  transfer.cancelRequested = true;
  filesDeleteServerTransfer(transfer);
  filesTransferDeleteDownloadParts(transfer.id, Number(transfer.rangeCount || 512));
  filesTransferDeleteUploadBlob(transfer.id);
  if (transfer.status === 'running') {
    transfer.abortController?.abort();
  } else if (['queued', 'paused'].includes(transfer.status)) {
    filesTransferTransition(transfer, 'cancelled', {
      actor: 'user',
      failure: new Error('transfer cancelled'),
    });
    pumpFilesTransfers();
  }
}

function resumeFilesTransfer(id) {
  const transfer = filesTransferById(id);
  if (!transfer || !['paused', 'failed'].includes(transfer.status)) return null;
  // Entering 'queued' re-arms the attempt (epoch bump, flags, completion).
  filesTransferTransition(transfer, 'queued', { actor: 'user' });
  pumpFilesTransfers();
  return transfer.completion;
}

function retryFilesTransfer(id) {
  const transfer = filesTransferById(id);
  if (!transfer || !['failed', 'cancelled', 'completed'].includes(transfer.status)) return null;
  if (transfer.kind === 'download') resetFilesDownloadTransfer(transfer);
  // Entering 'queued' re-arms the attempt (epoch bump, flags, completion).
  filesTransferTransition(transfer, 'queued', { actor: 'user' });
  pumpFilesTransfers();
  return transfer.completion;
}

function clearFilesTransferHistory() {
  for (let i = filesTransfers.length - 1; i >= 0; i -= 1) {
    if (FILES_TRANSFER_TERMINAL_STATUSES.has(filesTransfers[i].status)) {
      filesTransferDeleteDownloadParts(filesTransfers[i].id, Number(filesTransfers[i].rangeCount || 512));
      filesTransferDeleteUploadBlob(filesTransfers[i].id);
      filesTransfers.splice(i, 1);
    }
  }
  filesTransferPersistState();
  renderFilesTransfers();
}

async function saveCompletedFilesDownload(id) {
  const transfer = filesTransferById(id);
  if (!transfer || transfer.kind !== 'download') return null;
  if (!transfer.result?.blob) {
    const hydrated = await filesTransferLoadDownloadParts(transfer);
    if (!hydrated) {
      // Documented completed → failed exception: Save clicked after the
      // cached download chunks were evicted (typically post-reload).
      filesTransferTransition(transfer, 'failed', {
        actor: 'user',
        error: 'download chunks are no longer available',
        reason: 'saved download chunks were evicted',
      });
      return null;
    }
    const blob = new Blob(transfer.parts || [], { type: transfer.contentType || 'application/octet-stream' });
    transfer.result = {
      ok: true,
      blob,
      filename: transfer.filename || 'download.bin',
      content_type: transfer.contentType || 'application/octet-stream',
      size: blob.size,
      total_size: transfer.totalSize || blob.size,
      range_count: transfer.rangeCount || transfer.parts.length,
      resumable: true,
    };
  }
  downloadDashboardBlob(transfer.result.blob, transfer.result.filename, transfer.result.content_type);
  return transfer.result;
}

function cancelFilesDownload() {
  const active = filesTransfers.find(item => item.kind === 'download' && item.status === 'running');
  if (active) cancelFilesTransfer(active.id);
}

function setFilesUploadStatus(kind, text) {
  const el = document.getElementById('files-upload-status');
  if (!el) return;
  el.className = 'files-download-status' + (kind ? ` ${kind}` : '');
  el.textContent = text || '';
  el.title = text || '';
}

function filesStagedDescriptorName(descriptor) {
  return String(descriptor?.original_name || descriptor?.originalName || descriptor?.name || descriptor?.filename || 'upload.bin');
}

function renderFilesStagedUploads() {
  if (!paneIsVisible('files')) {
    renderOrDefer('files', 'staged', renderFilesStagedUploads);
    return;
  }
  const list = document.getElementById('files-staged-list');
  if (!list) return;
  list.innerHTML = '';
  const uploads = Array.from(filesStagedUploads.values())
    .sort((a, b) => filesStagedDescriptorName(a).localeCompare(filesStagedDescriptorName(b)));
  if (uploads.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'files-staged-empty ui-empty compact';
    empty.innerHTML = '<div class="ui-empty-title">No staged uploads</div>' +
      '<div class="ui-empty-hint">Choose or drop files above to stage them for the agent.</div>';
    list.appendChild(empty);
    return;
  }
  for (const upload of uploads) {
    const id = String(upload.id || '').trim();
    const row = document.createElement('div');
    row.className = 'files-staged-row';
    const main = document.createElement('div');
    main.className = 'files-staged-main';
    const title = document.createElement('div');
    title.className = 'files-staged-title';
    title.textContent = filesStagedDescriptorName(upload);
    const meta = document.createElement('div');
    meta.className = 'files-staged-meta';
    const destination = String(upload.destination || 'task');
    const mime = String(upload.mime || upload.content_type || 'application/octet-stream');
    meta.textContent = `${humanBytes(Number(upload.size || 0))} · ${destination} · ${mime}`;
    main.append(title, meta);
    const actions = document.createElement('div');
    actions.className = 'files-staged-actions';
    const addAction = (label, handler, className = '') => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.textContent = label;
      btn.className = className ? `ui-btn ${className}` : 'ui-btn';
      btn.disabled = !id;
      btn.addEventListener('click', () => handler(id));
      actions.appendChild(btn);
    };
    addAction('Download', downloadFilesStagedUpload);
    addAction('Remove', deleteFilesStagedUpload, 'danger');
    row.append(main, actions);
    list.appendChild(row);
  }
}

async function refreshFilesStagedUploads() {
  setFilesUploadStatus('warn', 'Loading staged uploads...');
  try {
    let uploads = [];
    if (
      dashboardTransport?.canUseRpc?.() &&
      dashboardControlTransport?.lastStatus?.api_session_current_uploads_available === true
    ) {
      uploads = await dashboardTransport.request('api_session_current_uploads', {}, { timeoutMs: 60000 });
    } else {
      if (dashboardConnectModeEnabled()) throw new Error('Staged uploads are unavailable until dashboard access reconnects');
      const resp = await fetch('/api/session/current/uploads');
      uploads = await resp.json().catch(() => []);
      if (!resp.ok) throw new Error(`staged uploads returned ${resp.status}`);
    }
    filesStagedUploads.clear();
    for (const upload of Array.isArray(uploads) ? uploads : []) {
      if (upload?.id) filesStagedUploads.set(String(upload.id), upload);
    }
    renderFilesStagedUploads();
    // Empty state lives in the staged list (renderFilesStagedUploads);
    // repeating it here printed "No staged uploads" twice.
    setFilesUploadStatus('ok', filesStagedUploads.size ? `${filesStagedUploads.size} staged upload${filesStagedUploads.size === 1 ? '' : 's'}` : '');
    return uploads;
  } catch (err) {
    const message = err?.message || String(err);
    setFilesUploadStatus('error', message);
    return [];
  }
}

async function downloadFilesStagedUpload(id) {
  const upload = filesStagedUploads.get(String(id || ''));
  if (!upload) return null;
  try {
    // One implementation for every transport: queue a transfer and let
    // runFilesDownloadTransfer pick durable RPC, byte-stream, or the
    // staged-raw HTTP fallback — the inline fetch this replaces had
    // drifted (no size cap, no cancel, raw server errors).
    const name = filesStagedDescriptorName(upload);
    const transfer = queueDashboardArtifactDownload({
      type: 'staged_upload',
      id: String(id || ''),
    }, {
      sourceLabel: `Staged upload: ${name}`,
      filename: name,
      contentType: upload.mime || upload.content_type || 'application/octet-stream',
      directMethod: 'api_session_current_upload_raw',
      directParams: { id: String(id || '') },
    });
    if (!transfer) throw new Error('Staged upload download was not queued');
    const result = await transfer.completion;
    setFilesUploadStatus('ok', `Downloaded ${result.filename || name}`);
    return result;
  } catch (err) {
    const message = err?.message || String(err);
    setFilesUploadStatus('error', message);
    return null;
  }
}

async function deleteFilesStagedUpload(id) {
  const key = String(id || '').trim();
  if (!key) return false;
  try {
    await dashboardTransport.request('api_session_current_upload_delete', { id: key }, { timeoutMs: 60000 });
    filesStagedUploads.delete(key);
    renderFilesStagedUploads();
    setFilesUploadStatus('ok', 'Upload removed');
    return true;
  } catch (err) {
    const message = err?.message || String(err);
    setFilesUploadStatus('error', message);
    return false;
  }
}

function queueFilesUpload(file, options = {}) {
  if (!file) return null;
  const destination = options.destination !== undefined
    ? String(options.destination || '').trim()
    : filesUploadDestinationValue();
  const transfer = {
    id: `upload-${Date.now()}-${++filesTransferSeq}`,
    kind: 'upload',
    status: 'queued',
    file,
    name: file.name || options.name || 'upload.bin',
    mime: file.type || options.mime || 'application/octet-stream',
    destination: destination || 'task',
    conflictPolicy: options.conflictPolicy || options.conflict || filesUploadConflictPolicy(),
    loaded: 0,
    totalSize: Number(file.size || 0),
    chunkBytes: Math.max(1, Math.floor(Number(options.chunkBytes) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES)),
    debugFailAfterChunks: Math.max(0, Math.floor(Number(options.debugFailAfterChunks || options.failAfterChunks) || 0)),
    debugFailedOnce: false,
    descriptor: null,
    error: '',
    cancelRequested: false,
    abortController: null,
    timeoutMs: options.timeoutMs,
  };
  if (transfer.destination !== 'task') {
    transfer.uploadBlobPersistPromise = filesTransferPutUploadBlob(transfer, file);
  }
  setFilesUploadStatus('warn', 'Queued upload');
  return queueFilesTransfer(transfer);
}

function queueFilesUploads(files, options = {}) {
  const queued = [];
  for (const file of Array.from(files || [])) {
    const transfer = queueFilesUpload(file, options);
    if (transfer) queued.push(transfer);
  }
  return queued;
}

	function chooseFilesForUpload() {
	  document.getElementById('files-upload-input')?.click();
	}

	function filesTransferSnapshot() {
	  return filesTransfers.map(transfer => ({
	    id: transfer.id,
	    kind: transfer.kind,
	    status: transfer.status,
	    name: filesTransferTitle(transfer),
	    path: transfer.path || '',
	    sourceKind: transfer.sourceKind || '',
	    sourceLabel: transfer.sourceLabel || '',
	    transport: transfer.transport || '',
	    peerId: transfer.peerId || '',
	    peerLabel: transfer.peerLabel || '',
	    artifact: transfer.artifact || null,
	    destination: transfer.destination || '',
	    conflictPolicy: transfer.conflictPolicy || '',
	    loaded: Number(transfer.loaded || 0),
	    totalSize: Number(transfer.totalSize || transfer.file?.size || 0),
	    rangeCount: Number(transfer.rangeCount || 0),
	    error: transfer.error || '',
	    uploadId: transfer.descriptor?.id || '',
	    serverJobId: transfer.serverJobId || transfer.serverJob?.id || '',
	    resumeToken: transfer.resumeToken || transfer.serverJob?.resume_token || '',
	    uploadBlobStored: Boolean(transfer.uploadBlobStored),
	  }));
	}

	function filesStagedUploadsSnapshot() {
	  return Array.from(filesStagedUploads.values()).map(upload => ({
	    id: String(upload.id || ''),
	    name: filesStagedDescriptorName(upload),
	    size: Number(upload.size || 0),
	    destination: String(upload.destination || 'task'),
	    mime: String(upload.mime || upload.content_type || 'application/octet-stream'),
	  }));
	}

function fsPathLooksAbsolute(path) {
  const value = String(path || '').trim();
  return value === '~' || value.startsWith('~/') || value.startsWith('/') || /^[A-Za-z]:[\\/]/.test(value);
}

