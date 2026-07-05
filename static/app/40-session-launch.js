function sessionConfigArchiveMode(meta = {}) {
  return normalizeContextArchiveMode(String(
    meta.codexContextArchive ||
    meta.codex_context_archive ||
    meta.capabilities?.codexContextArchive ||
    meta.capabilities?.codex_context_archive ||
    ''
  ).trim() || controlCodexConfig.context_archive || 'summary');
}

function sessionLaunchManagedMode(meta = {}) {
  const mode = String(
    meta.codexManagedContext ||
    meta.codex_managed_context ||
    meta.capabilities?.codexManagedContext ||
    meta.capabilities?.codex_managed_context ||
    ''
  ).trim();
  return mode === 'managed' || mode === 'vanilla' ? mode : '';
}

function sessionLaunchSandboxMode(meta = {}) {
  return normalizeCodexSandboxOptional(
    meta.codexSandbox ||
    meta.codex_sandbox ||
    ''
  );
}

function sessionLaunchApprovalPolicy(meta = {}) {
  return normalizeCodexApprovalPolicyOptional(
    meta.codexApprovalPolicy ||
    meta.codex_approval_policy ||
    ''
  );
}

function sessionLaunchArchiveMode(meta = {}) {
  return normalizeContextArchiveModeOptional(
    meta.codexContextArchive ||
    meta.codex_context_archive ||
    meta.capabilities?.codexContextArchive ||
    meta.capabilities?.codex_context_archive ||
    ''
  );
}

function sessionConfigMetadata(sessionOrId) {
  const raw = typeof sessionOrId === 'object'
    ? sessionOrId
    : (findCachedSessionByAnyId(sessionOrId) || { session_id: sessionOrId });
  const sid = String(raw?.session_id || raw?.resume_id || sessionOrId || '').trim();
  const cached = findCachedSessionByAnyId(sid) || {};
  const live = sessionMetadataById.get(sid) || {};
  const backendId = String(raw?.backend_session_id || raw?.backendSessionId || cached.backend_session_id || cached.backendSessionId || live.backendSessionId || '').trim();
  const intendantId = String(raw?.intendant_session_id || raw?.intendantSessionId || cached.intendant_session_id || cached.intendantSessionId || live.intendantSessionId || '').trim();
  const backendLive = backendId ? (sessionMetadataById.get(backendId) || {}) : {};
  const merged = {
    ...cached,
    ...raw,
    ...live,
    ...backendLive,
    session_id: sid || raw?.session_id || raw?.resume_id || backendId,
    backendSessionId: backendId || live.backendSessionId || backendLive.backendSessionId || '',
    intendantSessionId: intendantId || live.intendantSessionId || backendLive.intendantSessionId || '',
    capabilities: live.capabilities || backendLive.capabilities || raw?.capabilities || cached.capabilities || null,
  };
  return managedContextNormalizeSessionMeta(merged, sid || backendId);
}

function sessionLaunchOverridesForSession(sessionOrId) {
  const meta = sessionConfigMetadata(sessionOrId);
  const source = sessionConfigSource(meta);
  const overrides = {};
  const command = sessionConfigCommand(meta);
  if (command) overrides.agent_command = command;
  if (source === 'codex') {
    const sandbox = sessionLaunchSandboxMode(meta);
    const approvalPolicy = sessionLaunchApprovalPolicy(meta);
    const managedMode = sessionLaunchManagedMode(meta);
    const archiveMode = sessionLaunchArchiveMode(meta);
    if (sandbox) overrides.codex_sandbox = sandbox;
    if (approvalPolicy) overrides.codex_approval_policy = approvalPolicy;
    if (managedMode) overrides.codex_managed_context = managedMode;
    if (archiveMode) overrides.codex_context_archive = archiveMode;
  }
  return overrides;
}

function openSessionConfigModal(sessionOrId) {
  const meta = sessionConfigMetadata(sessionOrId);
  const sid = String(meta.session_id || meta.sessionId || sessionOrId || '').trim();
  const source = sessionConfigSource(meta);
  if (!sid || !source || source === 'intendant') {
    showControlToast('error', 'Session launch config is only available for external-agent sessions');
    return false;
  }
  sessionConfigEditing = {
    sessionId: sid,
    source,
    backendSessionId: String(meta.backendSessionId || meta.backend_session_id || '').trim(),
    intendantSessionId: String(meta.intendantSessionId || meta.intendant_session_id || '').trim(),
  };
  setSessionConfigSaving(false);
  const title = document.getElementById('session-config-title');
  if (title) title.textContent = `${prettyAgentName(source)} launch config`;
  const description = document.getElementById('session-config-description');
  if (description) {
    description.textContent = 'Save changes for future attaches, or save and immediately restart this external backend with the new binary and mode.';
  }
  const sessionInput = document.getElementById('session-config-session');
  if (sessionInput) {
    sessionInput.value = sid;
    sessionInput.title = sid;
  }
  const sourceInput = document.getElementById('session-config-source');
  if (sourceInput) sourceInput.value = prettyAgentName(source) || source;
  const commandInput = document.getElementById('session-config-command');
  const command = sessionConfigCommand(meta);
  if (commandInput) {
    commandInput.value = command;
    commandInput.placeholder = commandDefaultForNewSessionAgent(source) || 'Use global setting';
    commandInput.title = command || commandInput.placeholder;
  }
  const managedRow = document.getElementById('session-config-managed-row');
  const managedSel = document.getElementById('session-config-managed-context');
  const sandboxRow = document.getElementById('session-config-sandbox-row');
  const sandboxSel = document.getElementById('session-config-sandbox');
  const approvalRow = document.getElementById('session-config-approval-row');
  const approvalSel = document.getElementById('session-config-approval-policy');
  const archiveRow = document.getElementById('session-config-archive-row');
  const archiveSel = document.getElementById('session-config-context-archive');
  if (managedRow) managedRow.style.display = source === 'codex' ? '' : 'none';
  if (managedSel) managedSel.value = sessionConfigExplicitManagedMode(meta);
  if (sandboxRow) sandboxRow.style.display = source === 'codex' ? '' : 'none';
  if (sandboxSel) sandboxSel.value = sessionConfigExplicitSandboxMode(meta);
  if (approvalRow) approvalRow.style.display = source === 'codex' ? '' : 'none';
  if (approvalSel) approvalSel.value = sessionConfigExplicitApprovalPolicy(meta);
  if (archiveRow) archiveRow.style.display = source === 'codex' ? '' : 'none';
  if (archiveSel) archiveSel.value = sessionConfigExplicitArchiveMode(meta);
  const isClaude = source === 'claude-code';
  for (const rowId of ['session-config-claude-model-row', 'session-config-claude-permission-row',
    'session-config-claude-tools-row', 'session-config-claude-effort-row']) {
    const row = document.getElementById(rowId);
    if (row) row.style.display = isClaude ? '' : 'none';
  }
  if (isClaude) {
    // Pins prefill; missing pin = inherit. A pinned model that isn't one
    // of the aliases lands in the Custom row.
    const aliases = ['fable', 'opus', 'sonnet', 'haiku'];
    const pinnedModel = String(meta.claude_model || meta.claudeModel || '').trim();
    const modelSel = document.getElementById('session-config-claude-model');
    const customInput = document.getElementById('session-config-claude-model-custom');
    if (modelSel) {
      modelSel.value = !pinnedModel ? 'inherit'
        : (aliases.includes(pinnedModel) ? pinnedModel : '__custom__');
    }
    if (customInput) {
      customInput.value = pinnedModel && !aliases.includes(pinnedModel) ? pinnedModel : '';
    }
    const modeSel = document.getElementById('session-config-claude-permission-mode');
    if (modeSel) {
      modeSel.value = String(meta.claude_permission_mode || meta.claudePermissionMode || '').trim() || 'inherit';
    }
    const toolsInput = document.getElementById('session-config-claude-allowed-tools');
    if (toolsInput) {
      const tools = meta.claude_allowed_tools ?? meta.claudeAllowedTools;
      toolsInput.value = Array.isArray(tools)
        ? (tools.length ? tools.join(', ') : 'all')
        : '';
    }
    const effortSel = document.getElementById('session-config-claude-effort');
    if (effortSel) {
      effortSel.value = String(meta.claude_effort || meta.claudeEffort || '').trim() || 'inherit';
    }
  }
  updateSessionConfigClaudeCustomRow();
  const status = document.getElementById('session-config-status');
  if (status) {
    status.className = 'session-config-status';
    status.textContent = source === 'codex'
      ? 'Managed requires a patched Codex binary. Save & restart applies changes now.'
      : (isClaude
        ? 'Save applies model & permissions to the live session; tools & effort apply at the next launch (or Save & restart).'
        : 'Save & restart applies changes now.');
  }
  const modal = document.getElementById('session-config-modal');
  if (modal) modal.style.display = 'flex';
  return true;
}

function closeSessionConfigModal() {
  if (sessionConfigSavePending?.timeoutHandle) {
    clearTimeout(sessionConfigSavePending.timeoutHandle);
  }
  sessionConfigSavePending = null;
  setSessionConfigSaving(false);
  const modal = document.getElementById('session-config-modal');
  if (modal) modal.style.display = 'none';
  sessionConfigEditing = null;
}

function setSessionConfigSaving(saving) {
  for (const id of ['session-config-save', 'session-config-save-restart']) {
    const btn = document.getElementById(id);
    if (btn) btn.disabled = !!saving;
  }
}

function sessionConfigResultMatchesPending(result, pending) {
  if (!result || !pending) return false;
  const resultIds = new Set([
    result.session_id,
    result.sessionId,
    result.backend_session_id,
    result.backendSessionId,
    result.intendant_session_id,
    result.intendantSessionId,
    ...(Array.isArray(result.persisted_session_ids) ? result.persisted_session_ids : []),
  ].map(id => String(id || '').trim()).filter(Boolean));
  return pending.ids.some(id => resultIds.has(id));
}

function handleSessionConfigResult(result) {
  const pending = sessionConfigSavePending;
  if (!sessionConfigResultMatchesPending(result, pending)) return;
  if (pending.timeoutHandle) clearTimeout(pending.timeoutHandle);
  sessionConfigSavePending = null;
  setSessionConfigSaving(false);
  const status = document.getElementById('session-config-status');
  const ok = result.success !== false;
  const message = String(result.message || (ok ? 'Session launch config saved' : 'Session launch config failed'));
  if (!ok) {
    if (status) {
      status.className = 'session-config-status error';
      status.textContent = message;
    }
    if (pending.station) {
      stationSetSessionConfigResult(pending.sessionId, message, 'error');
      stationScheduleUpdate();
    }
    showControlToast('error', message);
    return;
  }

  const persisted = Array.isArray(result.persisted_session_ids) ? result.persisted_session_ids : [];
  const expectedBackend = String(pending.backendSessionId || '').trim();
  const missingBackend = expectedBackend && !persisted.some(id => String(id || '').trim() === expectedBackend);
  if (missingBackend && status) {
    status.className = 'session-config-status error';
    status.textContent = `Saved, but the backend thread id ${shortSessionId(expectedBackend)} was not confirmed.`;
  }
  if (missingBackend) {
    if (pending.station) {
      stationSetSessionConfigResult(
        pending.sessionId,
        `Saved, but the backend thread id ${shortSessionId(expectedBackend)} was not confirmed.`,
        'error'
      );
      stationScheduleUpdate();
    }
    showControlToast('error', `Launch config saved without backend id ${shortSessionId(expectedBackend)}`);
    return;
  }

  applySessionConfigLocal(
    pending.meta,
    pending.command,
    pending.sandboxMode,
    pending.approvalPolicy,
    pending.mode,
    pending.archiveMode,
    pending.claudeForm
  );
  scheduleSessionsMetadataRefresh(200);
  showControlToast('success', pending.restart ? 'Launch config saved; restarting session' : 'Launch config saved');
  const restartSessionId = pending.meta.sessionId;
  if (pending.station) {
    stationSetSessionConfigResult(
      pending.sessionId,
      pending.restart ? 'Launch config saved; restarting session.' : 'Launch config saved.',
      'ok'
    );
  }
  closeSessionConfigModal();
  if (pending.station) stationScheduleUpdate();
  if (pending.restart) restartSessionWindowAction(restartSessionId);
}

// Mirror the just-saved claude pins onto a cached row / live meta with the
// wire's sentinel semantics ('inherit' clears; tools 'all' pins []).
function applyClaudePinsLocal(target, claudeForm, { snake }) {
  if (!claudeForm) return;
  const keys = snake
    ? { model: 'claude_model', mode: 'claude_permission_mode', tools: 'claude_allowed_tools', effort: 'claude_effort' }
    : { model: 'claudeModel', mode: 'claudePermissionMode', tools: 'claudeAllowedTools', effort: 'claudeEffort' };
  const setOrClear = (key, value) => {
    if (value === 'inherit' || value === '') delete target[key];
    else target[key] = value;
  };
  setOrClear(keys.model, claudeForm.model);
  setOrClear(keys.mode, claudeForm.permissionMode);
  setOrClear(keys.effort, claudeForm.effort);
  if (claudeForm.allowedTools === 'inherit' || claudeForm.allowedTools === '') {
    delete target[keys.tools];
  } else if (claudeForm.allowedTools === 'all' || claudeForm.allowedTools === '*') {
    target[keys.tools] = [];
  } else {
    target[keys.tools] = claudeForm.allowedTools
      .split(',').map(rule => rule.trim()).filter(Boolean);
  }
}

function applySessionConfigLocal(meta, command, sandboxMode, approvalPolicy, mode, archiveMode, claudeForm) {
  const ids = Array.from(new Set([
    meta.sessionId,
    meta.backendSessionId,
    meta.intendantSessionId,
  ].map(id => String(id || '').trim()).filter(Boolean)));
  for (const id of ids) {
    const existing = sessionMetadataById.get(id) || {};
    const next = {
      ...existing,
      source: existing.source || meta.source,
      backendSource: existing.backendSource || meta.source,
      ...(meta.backendSessionId ? { backendSessionId: meta.backendSessionId } : {}),
      ...(meta.intendantSessionId ? { intendantSessionId: meta.intendantSessionId } : {}),
      ...(command ? { agentCommand: command } : {}),
      ...(meta.source === 'codex' ? {
        codexSandbox: sandboxMode ? normalizeCodexSandbox(sandboxMode) : '',
        codexApprovalPolicy: approvalPolicy ? normalizeCodexApprovalPolicy(approvalPolicy) : '',
        codexManagedContext: mode || '',
        codexContextArchive: archiveMode || '',
      } : {}),
    };
    if (meta.source === 'claude-code') {
      applyClaudePinsLocal(next, claudeForm, { snake: false });
    }
    sessionMetadataById.set(id, next);
    if (sessionWindows.has(id)) updateSessionWindow(id, next);
  }
  for (const session of _cachedSessions || []) {
    if (!session) continue;
    const matches = [
      session.session_id,
      session.resume_id,
      session.backend_session_id,
      session.intendant_session_id,
    ].some(id => ids.includes(String(id || '').trim()));
    if (!matches) continue;
    if (command) {
      session.agent_command = command;
      if (meta.source === 'codex') session.codex_command = command;
    } else {
      delete session.agent_command;
      delete session.codex_command;
    }
    if (meta.source === 'codex') {
      if (sandboxMode) {
        session.codex_sandbox = normalizeCodexSandbox(sandboxMode);
      } else {
        delete session.codex_sandbox;
      }
      if (approvalPolicy) {
        session.codex_approval_policy = normalizeCodexApprovalPolicy(approvalPolicy);
      } else {
        delete session.codex_approval_policy;
      }
      if (mode) {
        session.codex_managed_context = mode;
      } else {
        delete session.codex_managed_context;
      }
      if (archiveMode) {
        session.codex_context_archive = archiveMode;
      } else {
        delete session.codex_context_archive;
      }
    }
    if (meta.source === 'claude-code') {
      applyClaudePinsLocal(session, claudeForm, { snake: true });
    }
  }
  persistSessionWindowState();
}

// The Claude launch-config form, read with the wire's sentinel semantics.
function sessionConfigClaudeFormValues() {
  const modelChoice = document.getElementById('session-config-claude-model')?.value || 'inherit';
  const custom = document.getElementById('session-config-claude-model-custom')?.value.trim() || '';
  const model = modelChoice === '__custom__' ? (custom || 'inherit') : modelChoice;
  const permissionMode = document.getElementById('session-config-claude-permission-mode')?.value || 'inherit';
  const toolsRaw = document.getElementById('session-config-claude-allowed-tools')?.value.trim() || '';
  const allowedTools = toolsRaw === '' ? 'inherit' : toolsRaw;
  const effort = document.getElementById('session-config-claude-effort')?.value || 'inherit';
  return { model, permissionMode, allowedTools, effort };
}

function updateSessionConfigClaudeCustomRow() {
  const sel = document.getElementById('session-config-claude-model');
  const row = document.getElementById('session-config-claude-model-custom-row');
  if (!sel || !row) return;
  const claudeVisible = document.getElementById('session-config-claude-model-row')?.style.display !== 'none';
  row.style.display = claudeVisible && sel.value === '__custom__' ? '' : 'none';
}
function onSessionConfigClaudeModelChange() {
  updateSessionConfigClaudeCustomRow();
}
window.onSessionConfigClaudeModelChange = onSessionConfigClaudeModelChange;

function saveSessionConfigModal(options = {}) {
  if (!sessionConfigEditing || !app) return;
  const command = document.getElementById('session-config-command')?.value.trim() || '';
  const sandboxMode = normalizeOptionalCodexSandbox(
    document.getElementById('session-config-sandbox')?.value || ''
  );
  const approvalPolicy = normalizeOptionalCodexApprovalPolicy(
    document.getElementById('session-config-approval-policy')?.value || ''
  );
  // '' = inherit the global default (un-pins the per-session override);
  // only explicit 'managed'/'vanilla' pin a value into the session overlay.
  const modeValue = String(document.getElementById('session-config-managed-context')?.value || '').trim();
  const mode = modeValue === 'managed' || modeValue === 'vanilla' ? modeValue : '';
  const archiveMode = normalizeContextArchiveModeOptional(
    document.getElementById('session-config-context-archive')?.value || ''
  );
  // Claude pins reflect the form exactly: every field is sent as a value or
  // the explicit 'inherit' clear sentinel (a missing field would silently
  // KEEP an existing pin — see the supervisor's clear-before-merge dance).
  const claudeForm = sessionConfigClaudeFormValues();
  const payload = {
    action: 'configure_session_agent',
    session_id: sessionConfigEditing.sessionId,
    source: sessionConfigEditing.source,
    ...(sessionConfigEditing.backendSessionId ? { backend_session_id: sessionConfigEditing.backendSessionId } : {}),
    ...(sessionConfigEditing.intendantSessionId ? { intendant_session_id: sessionConfigEditing.intendantSessionId } : {}),
    agent_command: command,
    ...(sessionConfigEditing.source === 'codex' ? {
      codex_sandbox: sandboxMode || 'inherit',
      codex_approval_policy: approvalPolicy || 'inherit',
      codex_managed_context: mode || 'inherit',
      codex_context_archive: archiveMode || 'inherit',
    } : {}),
    ...(sessionConfigEditing.source === 'claude-code' ? {
      claude_model: claudeForm.model,
      claude_permission_mode: claudeForm.permissionMode,
      claude_allowed_tools: claudeForm.allowedTools,
      claude_effort: claudeForm.effort,
    } : {}),
  };
  try {
    if (sessionConfigSavePending?.timeoutHandle) {
      clearTimeout(sessionConfigSavePending.timeoutHandle);
    }
    const ids = Array.from(new Set([
      sessionConfigEditing.sessionId,
      sessionConfigEditing.backendSessionId,
      sessionConfigEditing.intendantSessionId,
    ].map(id => String(id || '').trim()).filter(Boolean)));
    sessionConfigSavePending = {
      ids,
      sessionId: sessionConfigEditing.sessionId,
      backendSessionId: sessionConfigEditing.backendSessionId,
      intendantSessionId: sessionConfigEditing.intendantSessionId,
      meta: { ...sessionConfigEditing },
      command,
      sandboxMode,
      approvalPolicy,
      mode,
      archiveMode,
      claudeForm: sessionConfigEditing.source === 'claude-code' ? claudeForm : null,
      restart: options.restart === true,
      timeoutHandle: setTimeout(() => {
        const pending = sessionConfigSavePending;
        if (!pending || pending.sessionId !== sessionConfigEditing?.sessionId) return;
        sessionConfigSavePending = null;
        setSessionConfigSaving(false);
        const status = document.getElementById('session-config-status');
        if (status) {
          status.className = 'session-config-status error';
          status.textContent = 'Save timed out before the daemon confirmed the launch config.';
        }
        showControlToast('error', 'Launch config save timed out');
      }, 15000),
    };
    setSessionConfigSaving(true);
    const status = document.getElementById('session-config-status');
    if (status) {
      status.className = 'session-config-status';
      status.textContent = options.restart === true ? 'Saving launch config before restart...' : 'Saving launch config...';
    }
    dispatchControlMsg(payload);
    // Model & permission pins also apply LIVE to an attached claude session
    // (verified set_model / set_permission_mode control requests) — unless a
    // restart is coming anyway, which respawns with the saved pins.
    if (
      sessionConfigEditing.source === 'claude-code' &&
      options.restart !== true &&
      !sessionWindowIsDetached(sessionConfigEditing.sessionId)
    ) {
      if (claudeForm.model !== 'inherit') {
        dispatchCodexThreadAction('model', { model: claudeForm.model }, sessionConfigEditing.sessionId);
      }
      if (claudeForm.permissionMode !== 'inherit') {
        dispatchCodexThreadAction(
          'permission-mode',
          { mode: claudeForm.permissionMode },
          sessionConfigEditing.sessionId,
        );
      }
    }
  } catch (e) {
    if (sessionConfigSavePending?.timeoutHandle) {
      clearTimeout(sessionConfigSavePending.timeoutHandle);
    }
    sessionConfigSavePending = null;
    setSessionConfigSaving(false);
    const status = document.getElementById('session-config-status');
    if (status) {
      status.className = 'session-config-status error';
      status.textContent = e?.message || 'Save failed';
    }
  }
}

window.openSessionConfigModal = openSessionConfigModal;
window.closeSessionRenameModal = closeSessionRenameModal;
window.saveSessionRenameModal = saveSessionRenameModal;
window.closeSessionConfigModal = closeSessionConfigModal;
window.saveSessionConfigModal = saveSessionConfigModal;
window.closeSessionDeleteModal = closeSessionDeleteModal;
window.confirmSessionDeleteModal = confirmSessionDeleteModal;

// ── Delegate to a sub-agent (internal sessions) ──
// Sends ControlMsg::SpawnSubAgent; the daemon spawns a supervised child
// under the parent, tracks it in the parent's wait_sub_agents registry,
// and wakes the parent with a notification follow-up.

let sessionDelegateTarget = '';

function sessionWindowIsInternal(sessionId, win = null) {
  return !externalSourceForSessionWindow(sessionId, win);
}

function openSessionDelegateModal(sessionOrId) {
  const sid = String(sessionOrId || '').trim();
  if (!sid) return false;
  if (!sessionWindowIsInternal(sid)) {
    showControlToast('error', 'Delegate targets internal-agent sessions; ask an external agent to delegate via a follow-up instead');
    return false;
  }
  sessionDelegateTarget = sid;
  const parent = document.getElementById('session-delegate-parent');
  if (parent) {
    parent.value = sid;
    parent.title = sid;
  }
  const task = document.getElementById('session-delegate-task');
  if (task) task.value = '';
  const name = document.getElementById('session-delegate-name');
  if (name) name.value = '';
  const role = document.getElementById('session-delegate-role');
  if (role) role.value = '';
  const agent = document.getElementById('session-delegate-agent');
  if (agent) agent.value = '';
  const worktree = document.getElementById('session-delegate-worktree');
  if (worktree) worktree.checked = false;
  const modal = document.getElementById('session-delegate-modal');
  if (modal) modal.style.display = 'flex';
  task?.focus();
  return true;
}

function closeSessionDelegateModal() {
  sessionDelegateTarget = '';
  const modal = document.getElementById('session-delegate-modal');
  if (modal) modal.style.display = 'none';
}

function confirmSessionDelegateModal() {
  const sid = sessionDelegateTarget;
  if (!sid) {
    closeSessionDelegateModal();
    return;
  }
  const task = document.getElementById('session-delegate-task')?.value.trim() || '';
  if (!task) {
    showControlToast('error', 'Describe the task to delegate first');
    return;
  }
  const msg = { action: 'spawn_sub_agent', session_id: sid, task };
  const name = document.getElementById('session-delegate-name')?.value.trim() || '';
  if (name) msg.name = name;
  const role = document.getElementById('session-delegate-role')?.value || '';
  if (role) msg.role = role;
  const agent = document.getElementById('session-delegate-agent')?.value || '';
  if (agent) msg.agent = agent;
  if (document.getElementById('session-delegate-worktree')?.checked) msg.worktree = true;
  if (!dispatchSessionControlMsg(msg)) {
    showControlToast('error', 'Dashboard is not connected to the server.');
    return;
  }
  showControlToast('info', `Delegating to a sub-agent under ${shortSessionId(sid)}...`);
  closeSessionDelegateModal();
}

window.openSessionDelegateModal = openSessionDelegateModal;
window.closeSessionDelegateModal = closeSessionDelegateModal;
window.confirmSessionDelegateModal = confirmSessionDelegateModal;

const SESSION_WINDOW_GENERIC_ACTIONS = [
  { op: 'copy-session-id', label: 'Copy full session ID', title: 'Copy the full session ID' },
  { op: 'rename-session', label: 'Rename session...', title: 'Rename this session' },
  { op: 'delegate-sub-agent', label: 'Delegate...', title: 'Spawn a supervised sub-agent under this session' },
  { op: 'configure-launch', label: 'Launch config...', title: 'Configure the binary and managed-context mode used on next attach/resume' },
  { op: 'attach-session', label: 'Attach session', title: 'Attach this backend without sending a prompt' },
  { op: 'restart-session', label: 'Restart with saved config', title: 'Stop this backend and resume it with saved launch config' },
  { op: 'stop-session', label: 'Stop session...', title: 'Stop the live backend and remove it from active dashboards', danger: true },
];

// Single source of truth for Codex thread-action metadata (prompt and
// confirmation requirements). The legacy Control-pane buttons, the session
// window action menus, and Station thread actions all read this registry so
// an action can never be dispatched without its confirm/prompt config.
const CODEX_THREAD_ACTION_SPECS = Object.freeze({
  'new': Object.freeze({}),
  fast: Object.freeze({}),
  compact: Object.freeze({}),
  undo: Object.freeze({ turns: '1' }),
  fork: Object.freeze({ promptName: true }),
  side: Object.freeze({ promptSide: true }),
  review: Object.freeze({ promptReview: true }),
  rename: Object.freeze({ promptRename: true }),
  goal: Object.freeze({ promptGoal: true }),
  'goal-get': Object.freeze({}),
  'goal-edit': Object.freeze({ promptGoalEdit: true }),
  'goal-pause': Object.freeze({}),
  'goal-resume': Object.freeze({}),
  'goal-clear': Object.freeze({ confirm: 'Clear the current Codex goal?' }),
  init: Object.freeze({}),
  'memory-reset': Object.freeze({}),
});

function codexThreadActionSpec(op) {
  return CODEX_THREAD_ACTION_SPECS[String(op || '').trim()] || null;
}

const SESSION_WINDOW_CODEX_ACTIONS = [
  { op: 'fork', label: 'Fork...', title: 'Fork this thread into a new session', ...CODEX_THREAD_ACTION_SPECS.fork },
  { op: 'side', label: 'Side...', title: 'Ask a side question in an ephemeral Codex fork', ...CODEX_THREAD_ACTION_SPECS.side },
  { op: 'fast', label: 'Fast', title: 'Toggle Codex Fast service tier for future turns' },
  {
    label: 'Configure goal...',
    title: 'Configure this thread goal',
    children: [
      { op: 'goal', label: 'Set goal...', title: 'Create or update this thread goal', ...CODEX_THREAD_ACTION_SPECS.goal },
      { op: 'goal-get', label: 'Goal status', title: 'Fetch this thread goal' },
      { op: 'goal-edit', label: 'Edit goal...', title: 'Edit this thread goal objective', ...CODEX_THREAD_ACTION_SPECS['goal-edit'] },
      { op: 'goal-pause', label: 'Pause goal', title: 'Pause this thread goal' },
      { op: 'goal-resume', label: 'Resume goal', title: 'Resume this thread goal' },
      { op: 'goal-clear', label: 'Clear goal', title: 'Clear this thread goal', ...CODEX_THREAD_ACTION_SPECS['goal-clear'] },
    ],
  },
  { op: 'compact', label: 'Compact', title: 'Compact this thread history' },
];

function flattenSessionWindowActions(actions, out = []) {
  for (const action of actions || []) {
    if (action?.op) out.push(action);
    if (Array.isArray(action?.children)) flattenSessionWindowActions(action.children, out);
  }
  return out;
}

const SESSION_WINDOW_CODEX_ACTION_BY_OP = new Map(
  flattenSessionWindowActions(SESSION_WINDOW_CODEX_ACTIONS).map(action => [action.op, action])
);

function sessionWindowCodexActionByOp(op) {
  return SESSION_WINDOW_CODEX_ACTION_BY_OP.get(String(op || '').trim()) || null;
}

function sessionWindowIsCodex(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  const current = win || (sid ? sessionWindows.get(sid) : null);
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  if (normalizeAgentId(meta.backendSource || current?.source || meta.source || meta.sourceLabel || '') === 'codex') {
    return true;
  }
  const capabilities = meta.capabilities || {};
  if (typeof capabilities.codexFastMode === 'boolean') return true;
  return Array.isArray(capabilities.codexThreadActions) && capabilities.codexThreadActions.length > 0;
}

function sessionWindowIsSide(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  const meta = sessionMetadataById.get(sid) || {};
  return meta.relationshipKind === 'side';
}

function sessionWindowIsSubagent(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  const meta = sessionMetadataById.get(sid) || {};
  return meta.relationshipKind === 'subagent';
}

function sessionWindowShouldStartHeaderCollapsed(meta = {}) {
  return true;
}

const SESSION_WINDOW_BOTTOM_THRESHOLD_PX = 28;

function sessionWindowLogIsAtBottom(log) {
  if (!log) return true;
  return log.scrollHeight - log.scrollTop - log.clientHeight <= SESSION_WINDOW_BOTTOM_THRESHOLD_PX;
}

// Keep the reader's place when nodes are removed ABOVE the viewport
// (render-window trims, log prunes): shrink scrollTop by exactly the
// removed height. Native scroll anchoring is unreliable for removals and
// absent in WebKit, so the log containers set `overflow-anchor: none`
// and this manual adjustment is the single source of truth.
function adjustScrollForRemovedAbove(scroller, mutate) {
  if (!scroller || typeof scroller.scrollHeight !== 'number') {
    mutate();
    return;
  }
  const before = scroller.scrollHeight;
  mutate();
  const delta = before - scroller.scrollHeight;
  if (delta > 0) {
    scroller.scrollTop = Math.max(0, scroller.scrollTop - delta);
  }
}

function sessionWindowShouldFollowNextOutput(win) {
  if (!win || !win.log) return true;
  return !!win.followOutput || (sessionWindowIsRenderingTail(win) && sessionWindowLogIsAtBottom(win.log));
}

function ensureSessionWindowHistory(win) {
  if (!win) return [];
  if (!Array.isArray(win.logHistory)) win.logHistory = [];
  if (!Number.isInteger(win.renderStart) || win.renderStart < 0) win.renderStart = 0;
  if (!Number.isInteger(win.renderEnd) || win.renderEnd < win.renderStart) win.renderEnd = win.renderStart;
  return win.logHistory;
}

// Drop the history head once it outgrows the cap, rebasing the render
// indices and the rendered nodes' data-history-index. Skipped while the
// reader is paging inside the region that would be dropped (they can only
// be there after deep back-paging; the trim resumes when they move on).
function trimSessionWindowHistoryIfNeeded(win) {
  const history = ensureSessionWindowHistory(win);
  if (history.length <= SESSION_WINDOW_HISTORY_LIMIT) return;
  const drop = history.length - SESSION_WINDOW_HISTORY_RETAIN;
  if (win.renderStart < drop) return;
  // Dropped entries stay in the replay-dedup set as bare signatures, so a
  // reconnect replay can't re-append them as fresh content.
  if (!(win.trimmedHistorySignatures instanceof Set)) win.trimmedHistorySignatures = new Set();
  for (let i = 0; i < drop; i++) {
    addSessionWindowHistorySignatures(win.trimmedHistorySignatures, history[i], win.sessionId);
  }
  if (win.trimmedHistorySignatures.size > 40000) {
    let excess = win.trimmedHistorySignatures.size - 30000;
    for (const sig of win.trimmedHistorySignatures) {
      if (excess-- <= 0) break;
      win.trimmedHistorySignatures.delete(sig);
    }
  }
  win.logHistory = history.slice(drop);
  win.renderStart -= drop;
  win.renderEnd -= drop;
  if (win.log) {
    for (const child of win.log.children) {
      const idx = Number(child.dataset?.historyIndex);
      if (Number.isInteger(idx)) child.dataset.historyIndex = String(idx - drop);
    }
  }
}

function sessionWindowHistoryNode(item) {
  return item instanceof Node ? item : null;
}

function sessionWindowHistoryRecord(item) {
  if (!item || item instanceof Node) return null;
  if (item.record && typeof item.record === 'object') return item.record;
  return typeof item === 'object' ? item : null;
}

function sessionWindowHistorySessionId(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) return node.dataset?.sessionId || '';
  return String(sessionWindowHistoryRecord(item)?.session_id || '').trim();
}

function sessionWindowHistoryUserTurnIndex(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) return node.dataset?.userTurnIndex || '';
  return sessionWindowHistoryRecord(item)?.user_turn_index;
}

function sessionWindowHistoryTurnId(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) return node.dataset?.turnId || '';
  const record = sessionWindowHistoryRecord(item);
  return String(record?.turn_id || record?.turnId || '').trim();
}

function markSessionWindowHistoryItemSuperseded(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) {
    markLogEntrySuperseded(node);
    return;
  }
  const record = sessionWindowHistoryRecord(item);
  if (record) record.superseded = true;
}

function prepareSessionWindowHistoryItem(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) node.dataset.sessionWindowHistory = '1';
  return item;
}

function retargetSessionWindowHistoryItem(item, sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return item;
  const node = sessionWindowHistoryNode(item);
  if (node) {
    retargetSessionWindowLogEntry(node, sid);
    return item;
  }
  const record = sessionWindowHistoryRecord(item);
  if (record) record.session_id = sid;
  return item;
}

function isSessionWindowCommandOutputRecord(c) {
  return !!c && (
    c.kind === 'agent_output' ||
    c.kind === 'command_execution' ||
    c.item_type === 'command_execution' ||
    c.itemType === 'command_execution' ||
    !!(c.command_execution || c.commandExecution) ||
    !!c.output_id
  );
}

function commandOutputRecordOutputIds(c = {}) {
  const ids = [];
  const add = (value) => {
    const id = String(value || '').trim();
    if (id && !ids.includes(id)) ids.push(id);
  };
  add(c.output_id || c.outputId);
  add(c.command_execution?.output_id || c.commandExecution?.outputId);
  if (Array.isArray(c.output_ids || c.outputIds)) {
    for (const id of (c.output_ids || c.outputIds)) add(id);
  }
  return ids;
}

function commandOutputRecordFetchSessionId(c = {}) {
  return String(c.output_session_id || c.outputSessionId || c.session_id || c.sessionId || '').trim();
}

function commandOutputRecordFetchSource(c = {}) {
  return normalizeAgentId(c.output_source || c.outputSource || c.source || '') || 'intendant';
}

function commandOutputRecordHasFullFetch(c = {}) {
  return !!(
    (c.full_output_available ?? c.fullOutputAvailable) ||
    (c.text_truncated ?? c.textTruncated) ||
    (Array.isArray(c.truncated_fields || c.truncatedFields) && (c.truncated_fields || c.truncatedFields).length > 0)
  ) && commandOutputRecordOutputIds(c).length > 0;
}

function commandOutputRecordStats(c = {}, content = '') {
  const stats = commandOutputStats(content);
  const bytes = Number(c.full_output_bytes ?? c.fullOutputBytes);
  const lines = Number(c.full_output_lines ?? c.fullOutputLines);
  return {
    bytes: Number.isFinite(bytes) && bytes > 0 ? bytes : stats.bytes,
    lines: Number.isFinite(lines) && lines > 0 ? lines : stats.lines,
  };
}

async function loadReplayCommandOutputIntoBody(c, body, copyRef = null) {
  const ids = commandOutputRecordOutputIds(c);
  const sessionId = commandOutputRecordFetchSessionId(c);
  const source = commandOutputRecordFetchSource(c);
  if (!ids.length || !sessionId) {
    body.textContent = 'Full output is not available for lazy loading.';
    return;
  }
  const json = await fetchSessionAgentOutputPayload(sessionId, {
    source,
    ids,
    cache: 'no-store',
  });
  if (json?.error) throw new Error(json.error);
  const outputs = Array.isArray(json.outputs) ? json.outputs : [];
  body.innerHTML = '';
  const chunks = [];
  for (const out of outputs) {
    const text = [out.stdout || '', out.stderr || ''].filter(Boolean).join(out.stdout && out.stderr ? '\n' : '');
    if (!text) continue;
    chunks.push(text);
    await appendCommandOutputTextProgressive(body, text);
  }
  if (copyRef && chunks.length) copyRef.text = chunks.join('\n');
  if (!body.childElementCount) {
    body.textContent = 'No persisted output found.';
  }
}

function buildSessionWindowCommandOutputEntry(c) {
  const content = String(c?.content || '');
  const { entry } = createLogScaffold(c, 'command-output-group finalized');
  const wrap = document.createElement('span');
  wrap.className = 'log-content command-output-wrap';
  const summary = document.createElement('span');
  summary.className = 'command-output-summary';
  const body = document.createElement('span');
  body.className = 'command-output-body';
  wrap.appendChild(summary);
  wrap.appendChild(body);
  const stats = commandOutputRecordStats(c, content);
  const canFetchFull = commandOutputRecordHasFullFetch(c);
  summary.innerHTML = commandOutputSummaryHtml({
    chunks: 1,
    lines: stats.lines,
    bytes: stats.bytes,
    warns: String(c?.level || '').toLowerCase() === 'warn' ? 1 : 0,
  });
  if (content && !canFetchFull) {
    setDeferredCommandOutputText(entry, body, content, stats);
  } else if (canFetchFull) {
    body.textContent = content
      ? 'Preview loaded; expand to fetch full output.'
      : 'Expand to fetch full output.';
    _lazyCommandOutputStore.set(entry, { c, body, loaded: false, loading: false });
  }
  const toggle = document.createElement('span');
  toggle.className = 'collapse-toggle';
  toggle.innerHTML = '<span class="arrow">\u25B8 output</span><span class="arrow-up">\u25BE hide</span>';
  entry.appendChild(wrap);
  entry.appendChild(toggle);
  const copyRef = setLogEntryCopyText(entry, content);
  appendCopyLogEntryButton(entry);
  entry.addEventListener('click', async (event) => {
    if (event.target?.closest?.('a, button')) return;
    const expanded = !entry.classList.contains('expanded');
    entry.classList.toggle('expanded', expanded);
    if (!expanded) return;
    const lazy = _lazyCommandOutputStore.get(entry);
    if (lazy) {
      if (lazy.loaded || lazy.loading) return;
      lazy.loading = true;
      body.textContent = 'Loading full output...';
      try {
        await loadReplayCommandOutputIntoBody(c, body, copyRef);
        lazy.loaded = true;
      } catch (err) {
        body.textContent = 'Could not load full output: ' + (err?.message || err);
      } finally {
        lazy.loading = false;
      }
      return;
    }
    renderDeferredCommandOutputText(entry);
  });
  return entry;
}

function makeSessionWindowLogEntryCollapsible(entry, cnt, expanded = false) {
  if (!entry || !cnt) return;
  entry.classList.add('collapsible');
  entry.classList.toggle('expanded', !!expanded);
  const toggle = document.createElement('span');
  toggle.className = 'collapse-toggle';
  toggle.innerHTML = '<span class="arrow">\u25B8 more</span><span class="arrow-up">\u25BE less</span>';
  entry.appendChild(toggle);
  wireCollapsibleLogEntry(entry, cnt);
}

function buildSessionWindowLogEntry(c) {
  if (!c || !c.session_id) return null;
  const content = c.content || '';
  if (!content && c.kind !== 'rollback_marker') return null;
  if (isSessionWindowCommandOutputRecord(c)) {
    return buildSessionWindowCommandOutputEntry(c);
  }
  if (isDiffLog(c)) {
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
    return entry;
  }

  const { entry } = createLogScaffold(c, '');
  const cnt = document.createElement('span');
  cnt.className = 'log-content';
  renderLogContentElement(cnt, c);
  appendLogStateBadges(cnt, c);
  entry.appendChild(cnt);
  appendCopyLogEntryButton(entry, c.content ?? '');
  appendEditUserMessageButton(entry, c);
  if (c.collapsible || content.split('\n').length > 3 || content.length > 300) {
    makeSessionWindowLogEntryCollapsible(entry, cnt);
  }
  return entry;
}

function materializeSessionWindowHistoryItem(win, item, index) {
  let node = sessionWindowHistoryNode(item);
  if (!node) {
    const record = sessionWindowHistoryRecord(item);
    if (!record) return null;
    node = buildSessionWindowLogEntry(record);
  }
  if (!node) return null;
  node.dataset.sessionWindowHistory = '1';
  node.dataset.historyIndex = String(index);
  if (win?.sessionId && node.dataset?.sessionId && node.dataset.sessionId !== win.sessionId) {
    retargetSessionWindowLogEntry(node, win.sessionId);
  }
  return node;
}

function sessionWindowTailStart(historyLength) {
  return Math.max(0, Number(historyLength || 0) - SESSION_WINDOW_RENDER_LIMIT);
}

function sessionWindowIsRenderingTail(win, historyLength = null) {
  if (!win) return true;
  const history = ensureSessionWindowHistory(win);
  const len = historyLength === null ? history.length : Number(historyLength || 0);
  return win.renderStart >= sessionWindowTailStart(len);
}

function renderSessionWindowRange(win, start) {
  if (!win || !win.log) return;
  const history = ensureSessionWindowHistory(win);
  if (history.length === 0) {
    win.renderStart = 0;
    win.renderEnd = 0;
    win.log.innerHTML = '<div class="session-window-empty">Waiting for events...</div>';
    scheduleSessionWindowGridFit();
    return;
  }
  const safeStart = Math.max(0, Math.min(Number(start) || 0, sessionWindowTailStart(history.length)));
  const safeEnd = Math.min(history.length, safeStart + SESSION_WINDOW_RENDER_LIMIT);
  const nodes = [];
  for (let i = safeStart; i < safeEnd; i++) {
    const node = materializeSessionWindowHistoryItem(win, history[i], i);
    if (node) nodes.push(node);
  }
  win.renderStart = safeStart;
  win.renderEnd = safeEnd;
  win.log.replaceChildren(...nodes);
  scheduleSessionWindowGridFit();
}

function renderSessionWindowTail(win) {
  if (!win) return;
  const history = ensureSessionWindowHistory(win);
  const start = sessionWindowTailStart(history.length);
  if (
    win.renderStart === start &&
    win.renderEnd === history.length &&
    !win.log?.firstElementChild?.classList?.contains('session-window-empty')
  ) {
    return;
  }
  // A tail re-render rebuilds the whole window. If the reader is scrolled
  // up, anchor on their first visible entry and restore its on-screen
  // offset afterwards — otherwise the rebuild dumps them at an arbitrary
  // (usually much older) position.
  const log = win.log;
  let anchorIndex = -1;
  let anchorTop = 0;
  if (log && !sessionWindowShouldFollowNextOutput(win)) {
    for (const child of log.children) {
      const idx = Number(child.dataset?.historyIndex);
      if (!Number.isInteger(idx)) continue;
      if (child.offsetTop + child.offsetHeight > log.scrollTop) {
        anchorIndex = idx;
        anchorTop = child.offsetTop - log.scrollTop;
        break;
      }
    }
  }
  renderSessionWindowRange(win, start);
  if (log && anchorIndex >= 0) {
    const nextAnchor = log.querySelector(`[data-history-index="${anchorIndex}"]`);
    if (nextAnchor) log.scrollTop = Math.max(0, nextAnchor.offsetTop - anchorTop);
  }
}

function appendSessionWindowRenderedTailItem(win, item, index, historyLength) {
  if (!win || !win.log || win.renderEnd !== index) return false;
  const node = materializeSessionWindowHistoryItem(win, item, index);
  if (!node) return false;
  if (win.log.firstElementChild?.classList?.contains('session-window-empty')) {
    win.log.replaceChildren();
  }
  win.log.appendChild(node);
  win.renderEnd = index + 1;
  adjustScrollForRemovedAbove(win.log, () => {
    const targetStart = sessionWindowTailStart(historyLength);
    while (win.renderStart < targetStart && win.log.firstElementChild) {
      win.log.firstElementChild.remove();
      win.renderStart += 1;
    }
  });
  scheduleSessionWindowGridFit();
  return true;
}

function appendSessionWindowRenderedTailItems(win, items, startIndex, historyLength) {
  if (!win || !win.log || !Array.isArray(items) || items.length === 0) return false;
  if (win.renderEnd !== startIndex) return false;
  if (win.log.firstElementChild?.classList?.contains('session-window-empty')) {
    win.log.replaceChildren();
  }
  let appended = 0;
  for (let offset = 0; offset < items.length; offset++) {
    const node = materializeSessionWindowHistoryItem(win, items[offset], startIndex + offset);
    if (!node) continue;
    win.log.appendChild(node);
    appended += 1;
  }
  if (appended === 0) return false;
  win.renderEnd = startIndex + items.length;
  adjustScrollForRemovedAbove(win.log, () => {
    const targetStart = sessionWindowTailStart(historyLength);
    while (win.renderStart < targetStart && win.log.firstElementChild) {
      win.log.firstElementChild.remove();
      win.renderStart += 1;
    }
  });
  scheduleSessionWindowGridFit();
  return true;
}

function addSessionWindowHistorySignatures(set, item, fallbackSessionId = '', options = {}) {
  if (!set) return;
  for (const signature of sessionWindowTranscriptSignaturesForHistoryItem(item, fallbackSessionId, options)) {
    set.add(signature);
  }
}

function sessionWindowHistorySignatureSet(win, fallbackSessionId = '') {
  const signatures = new Set();
  for (const item of ensureSessionWindowHistory(win)) {
    addSessionWindowHistorySignatures(signatures, item, fallbackSessionId);
  }
  return signatures;
}

function sessionWindowHistoryHasMatchingSignature(signatures, item, fallbackSessionId = '') {
  if (!signatures) return false;
  const itemSignatures = sessionWindowTranscriptSignaturesForHistoryItem(item, fallbackSessionId);
  return itemSignatures.length > 0 && itemSignatures.some(signature => signatures.has(signature));
}

function sessionWindowHistoryItemPriority(item) {
  const record = sessionWindowHistoryRecord(item);
  const node = sessionWindowHistoryNode(item);
  let score = record ? 4 : 0;
  const ts = String(record?.ts || record?.timestamp || node?.querySelector?.('.log-ts')?.title || '');
  if (/\d{4}-\d{2}-\d{2}T/.test(ts) || /\([^()]*\d{4}-\d{2}-\d{2}T/.test(ts)) score += 2;
  const source = String(record?.source || node?.querySelector?.('.log-level')?.textContent || '').trim();
  if (source && source === source.toLowerCase()) score += 1;
  return score;
}

function deduplicateSessionWindowHistory(win, shouldFollow = null) {
  if (!win) return 0;
  const history = ensureSessionWindowHistory(win);
  if (history.length < 2) return 0;
  const wasRenderingTail = sessionWindowIsRenderingTail(win, history.length);
  const kept = [];
  let signatureToIndex = new Map();
  let removed = 0;
  const rebuildIndex = () => {
    signatureToIndex = new Map();
    kept.forEach((item, index) => {
      for (const signature of sessionWindowTranscriptSignaturesForHistoryItem(item, win.sessionId, { includeUserNearTime: true })) {
        if (!signatureToIndex.has(signature)) signatureToIndex.set(signature, index);
      }
    });
  };
  for (const item of history) {
    const itemSignatures = sessionWindowTranscriptSignaturesForHistoryItem(item, win.sessionId, { includeUserNearTime: true });
    const existingIndex = itemSignatures
      .map(signature => signatureToIndex.get(signature))
      .find(index => index !== undefined);
    if (existingIndex !== undefined) {
      if (sessionWindowHistoryItemPriority(item) > sessionWindowHistoryItemPriority(kept[existingIndex])) {
        kept[existingIndex] = item;
        rebuildIndex();
      }
      removed += 1;
      continue;
    }
    kept.push(item);
    for (const signature of itemSignatures) {
      if (!signatureToIndex.has(signature)) signatureToIndex.set(signature, kept.length - 1);
    }
  }
  if (removed === 0) return 0;
  history.splice(0, history.length, ...kept);
  const follow = shouldFollow === null ? wasRenderingTail : !!shouldFollow;
  if (follow || wasRenderingTail) {
    renderSessionWindowTail(win);
  } else {
    renderSessionWindowRange(win, Math.min(win.renderStart, sessionWindowTailStart(history.length)));
  }
  applySessionWindowOutputScroll(win, follow);
  return removed;
}

function appendSessionWindowHistory(win, entry, shouldFollow) {
  if (!win || !entry) return;
  const history = ensureSessionWindowHistory(win);
  const wasRenderingTail = sessionWindowIsRenderingTail(win, history.length);
  const item = retargetSessionWindowHistoryItem(
    prepareSessionWindowHistoryItem(entry),
    win.sessionId
  );
  const signatures = sessionWindowHistorySignatureSet(win, win.sessionId);
  if (
    sessionWindowHistoryHasMatchingSignature(signatures, item, win.sessionId)
    || sessionWindowHistoryHasMatchingSignature(win.trimmedHistorySignatures, item, win.sessionId)
  ) return;
  stationTrackSessionWindowHistoryAnchor(item, win.sessionId);
  history.push(item);
  const renderedIncrementally = wasRenderingTail
    && appendSessionWindowRenderedTailItem(win, item, history.length - 1, history.length);
  if ((shouldFollow || wasRenderingTail) && !renderedIncrementally) {
    renderSessionWindowTail(win);
  }
  applySessionWindowOutputScroll(win, shouldFollow);
  trimSessionWindowHistoryIfNeeded(win);
}

function appendSessionWindowHistoryBatch(win, entries, shouldFollow) {
  if (!win || !Array.isArray(entries) || entries.length === 0) return;
  const history = ensureSessionWindowHistory(win);
  const startIndex = history.length;
  const wasRenderingTail = sessionWindowIsRenderingTail(win, history.length);
  const signatures = sessionWindowHistorySignatureSet(win, win.sessionId);
  const items = [];
  for (const entry of entries) {
    if (!entry) continue;
    const item = retargetSessionWindowHistoryItem(
      prepareSessionWindowHistoryItem(entry),
      win.sessionId
    );
    if (
      sessionWindowHistoryHasMatchingSignature(signatures, item, win.sessionId)
      || sessionWindowHistoryHasMatchingSignature(win.trimmedHistorySignatures, item, win.sessionId)
    ) continue;
    stationTrackSessionWindowHistoryAnchor(item, win.sessionId);
    history.push(item);
    items.push(item);
    addSessionWindowHistorySignatures(signatures, item, win.sessionId);
  }
  if (items.length === 0) return;
  const renderedIncrementally = wasRenderingTail
    && items.length <= SESSION_WINDOW_PREPEND_CHUNK
    && appendSessionWindowRenderedTailItems(win, items, startIndex, history.length);
  if ((shouldFollow || wasRenderingTail) && !renderedIncrementally) {
    renderSessionWindowTail(win);
  }
  applySessionWindowOutputScroll(win, shouldFollow);
  trimSessionWindowHistoryIfNeeded(win);
}

function loadOlderSessionWindowEntries(win) {
  if (!win || !win.log) return false;
  const history = ensureSessionWindowHistory(win);
  if (win.log.scrollTop > SESSION_WINDOW_TOP_LOAD_THRESHOLD_PX) return false;
  if (win.renderStart <= 0) {
    loadOlderRemoteSessionWindowEntries(win);
    return !!win.remoteLoadingOlder;
  }
  if (history.length === 0) return false;
  const anchorIndex = win.renderStart;
  const anchor = win.log.querySelector(`[data-history-index="${anchorIndex}"]`);
  const anchorTop = anchor ? anchor.offsetTop - win.log.scrollTop : 0;
  renderSessionWindowRange(win, Math.max(0, win.renderStart - SESSION_WINDOW_PREPEND_CHUNK));
  const nextAnchor = win.log.querySelector(`[data-history-index="${anchorIndex}"]`);
  if (nextAnchor) win.log.scrollTop = Math.max(0, nextAnchor.offsetTop - anchorTop);
  return true;
}

function loadOlderRemoteSessionWindowEntries(win) {
  if (!win || !win.log || win.remoteLoadingOlder) return false;
  const before = Number(win.remotePageStart);
  if (!Number.isFinite(before) || before <= 0) {
    win.remoteHasOlder = false;
    return false;
  }
  const source = normalizeAgentId(win.remoteSource || externalSourceForSessionWindow(win.sessionId, win) || win.source || '');
  const sessionId = String(win.remoteSessionId || win.sessionId || '').trim();
  if (!source || !sessionId) return false;

  const history = ensureSessionWindowHistory(win);
  const anchorIndex = Math.max(0, win.renderStart);
  const anchorItem = history[anchorIndex] || history[0] || null;
  const anchorSignatures = anchorItem
    ? sessionWindowTranscriptSignaturesForHistoryItem(anchorItem, win.sessionId)
    : [];
  const anchor = win.log.querySelector(`[data-history-index="${anchorIndex}"]`);
  const anchorTop = anchor ? anchor.offsetTop - win.log.scrollTop : 0;

  win.remoteLoadingOlder = true;
  fetchSessionDetailPayload(sessionId, {
    source,
    limit: SESSION_WINDOW_RESTORE_LOG_LIMIT,
    before,
    cache: 'no-store',
  })
    .then(data => {
      if (!win || !sessionWindows.has(win.sessionId) || data?.error) return;
      updateSessionWindowRemotePageState(win, data, source, sessionId);
      const entries = Array.isArray(data.entries) ? data.entries : [];
      if (!entries.length) return;
      applySessionIdentitiesFromReplayEntries(entries);
      applyExternalIdentitiesFromLogEntries(entries);
      applySessionGoalsFromReplayEntries(entries);
      const records = entries
        .map(entry => sessionWindowRecordFromReplayEntry(entry, win.sessionId))
        .filter(Boolean);
      if (!records.length) return;
      insertSessionWindowHistoryRecords(win, records, false);
      const nextHistory = ensureSessionWindowHistory(win);
      const nextAnchorIndex = findSessionWindowHistoryIndexBySignatures(
        nextHistory,
        anchorSignatures,
        win.sessionId
      );
      if (nextAnchorIndex >= 0) {
        renderSessionWindowRange(win, Math.max(0, nextAnchorIndex - SESSION_WINDOW_PREPEND_CHUNK));
        const nextAnchor = win.log.querySelector(`[data-history-index="${nextAnchorIndex}"]`);
        if (nextAnchor) win.log.scrollTop = Math.max(0, nextAnchor.offsetTop - anchorTop);
      } else {
        renderSessionWindowRange(win, 0);
      }
      stationScheduleUpdate();
    })
    .catch(err => {
      console.warn('Failed to load older session window transcript page', sessionId, err);
    })
    .finally(() => {
      if (win) win.remoteLoadingOlder = false;
    });
  return true;
}

function loadNewerSessionWindowEntries(win) {
  if (!win || !win.log) return false;
  const history = ensureSessionWindowHistory(win);
  if (win.renderEnd >= history.length || history.length === 0) return false;
  if (!sessionWindowLogIsAtBottom(win.log)) return false;
  const anchorIndex = Math.max(win.renderStart, win.renderEnd - 1);
  const anchor = win.log.querySelector(`[data-history-index="${anchorIndex}"]`);
  const anchorBottom = anchor
    ? anchor.offsetTop + anchor.offsetHeight - win.log.scrollTop
    : win.log.clientHeight;
  const nextStart = Math.min(
    sessionWindowTailStart(history.length),
    win.renderStart + SESSION_WINDOW_PREPEND_CHUNK
  );
  renderSessionWindowRange(win, nextStart);
  const nextAnchor = win.log.querySelector(`[data-history-index="${anchorIndex}"]`);
  if (nextAnchor) {
    win.log.scrollTop = Math.max(0, nextAnchor.offsetTop + nextAnchor.offsetHeight - anchorBottom);
  }
  return true;
}

function updateSessionWindowJumpButton(win) {
  if (!win || !win.jumpBottom) return;
  const show = !!win.pendingOutput && !win.followOutput && !win.minimized;
  win.jumpBottom.classList.toggle('hidden', !show);
  win.jumpBottom.setAttribute('aria-hidden', show ? 'false' : 'true');
}

function scrollSessionWindowToBottom(win) {
  if (!win || !win.log) return;
  const history = ensureSessionWindowHistory(win);
  if (!sessionWindowIsRenderingTail(win, history.length) || win.renderEnd !== history.length) {
    renderSessionWindowTail(win);
  }
  win.log.scrollTop = win.log.scrollHeight;
  win.followOutput = true;
  win.pendingOutput = false;
  updateSessionWindowJumpButton(win);
}

function applySessionWindowOutputScroll(win, shouldFollow) {
  if (!win || !win.log) return;
  if (shouldFollow) {
    scrollSessionWindowToBottom(win);
  } else {
    win.followOutput = false;
    win.pendingOutput = true;
    updateSessionWindowJumpButton(win);
  }
}

function updateSessionWindowFollowFromScroll(win) {
  if (!win || !win.log) return;
  loadOlderSessionWindowEntries(win);
  loadNewerSessionWindowEntries(win);
  if (sessionWindowIsRenderingTail(win) && sessionWindowLogIsAtBottom(win.log)) {
    win.followOutput = true;
    win.pendingOutput = false;
  } else {
    win.followOutput = false;
  }
  updateSessionWindowJumpButton(win);
}

function sessionWindowForLogDescendant(node) {
  const root = node?.closest?.('.session-window');
  const sid = root?.dataset?.sessionId || '';
  return sid ? sessionWindows.get(sid) : null;
}

function codexThreadActionAllowedForSide(op) {
  const normalized = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  return normalized === 'side-close' || normalized === 'undo';
}

function normalizeSessionCapabilities(raw = null) {
  if (!raw || typeof raw !== 'object') return null;
  const normalizeOps = (list) => Array.isArray(list)
    ? list.map(op => String(op || '').trim().toLowerCase().replace(/_/g, '-')).filter(Boolean)
    : null;
  const universalActions = normalizeOps(raw.thread_actions ?? raw.threadActions);
  const actions = raw.codex_thread_actions || raw.codexThreadActions;
  const rawFastMode = raw.codex_fast_mode ?? raw.codexFastMode;
  let codexFastMode = null;
  if (typeof rawFastMode === 'boolean') {
    codexFastMode = rawFastMode;
  } else if (typeof rawFastMode === 'string') {
    const normalized = rawFastMode.trim().toLowerCase();
    if (normalized === 'true') codexFastMode = true;
    if (normalized === 'false') codexFastMode = false;
  }
  const rawServiceTier = raw.codex_service_tier ?? raw.codexServiceTier;
  const codexServiceTier = rawServiceTier === null || rawServiceTier === undefined
    ? null
    : String(rawServiceTier).trim() || null;
  return {
    followUp: raw.follow_up ?? raw.followUp ?? true,
    steer: raw.steer ?? false,
    interrupt: raw.interrupt ?? false,
    threadActions: universalActions,
    codexThreadActions: normalizeOps(actions),
    codexManagedContext: raw.codex_managed_context ?? raw.codexManagedContext ?? null,
    codexSandbox: raw.codex_sandbox ?? raw.codexSandbox ?? null,
    codexApprovalPolicy: raw.codex_approval_policy ?? raw.codexApprovalPolicy ?? null,
    codexContextArchive: raw.codex_context_archive ?? raw.codexContextArchive ?? null,
    codexCommand: raw.codex_command ?? raw.codexCommand ?? null,
    codexFastMode,
    codexServiceTier,
  };
}

function mergeSessionCapabilities(existing = null, incoming = null) {
  if (!existing) return incoming;
  if (!incoming) return existing;
  const merged = { ...existing, ...incoming };
  for (const key of ['threadActions', 'codexThreadActions', 'codexManagedContext', 'codexSandbox', 'codexApprovalPolicy', 'codexContextArchive', 'codexCommand', 'codexFastMode', 'codexServiceTier']) {
    if (
      key === 'codexServiceTier' &&
      incoming.codexFastMode === false &&
      (incoming[key] === null || incoming[key] === undefined)
    ) {
      merged[key] = null;
      continue;
    }
    if (incoming[key] === null || incoming[key] === undefined) {
      merged[key] = existing[key] ?? null;
    }
  }
  return merged;
}

function applySessionCapabilities(evt = {}) {
  const sid = String(evt.session_id || evt.sessionId || '').trim();
  if (!sid) return;
  const capabilities = normalizeSessionCapabilities(evt.capabilities || evt);
  if (!capabilities) return;
  const related = relatedSessionIdsForSession(sid);
  for (const id of related) {
    const existing = sessionMetadataById.get(id) || {};
    const mergedCapabilities = mergeSessionCapabilities(existing.capabilities, capabilities);
    sessionMetadataById.set(id, { ...existing, capabilities: mergedCapabilities });
    if (sessionWindows.has(id)) {
      updateSessionWindow(id, { capabilities: mergedCapabilities });
    }
  }
  updateSessionWindowActionMenuVisibility(sid);
  updateStopButtonVisibility(currentPhase);
  updateSubmitButtonLabel(currentPhase);
  updateControlFastButtonState();
}

function applyCodexFastModeToSession(sessionId, fastMode) {
  const sid = String(sessionId || '').trim();
  if (!sid || typeof fastMode !== 'boolean') return;
  const serviceTier = fastMode ? 'priority' : null;
  for (const id of relatedSessionIdsForSession(sid)) {
    const existing = sessionMetadataById.get(id) || {};
    const capabilities = {
      followUp: true,
      steer: true,
      interrupt: true,
      ...(existing.capabilities || {}),
      codexFastMode: fastMode,
      codexServiceTier: serviceTier,
    };
    sessionMetadataById.set(id, { ...existing, capabilities });
    if (sessionWindows.has(id)) {
      updateSessionWindow(id, { capabilities });
    }
  }
  updateSessionWindowActionMenuVisibility(sid);
  updateStopButtonVisibility(currentPhase);
  updateSubmitButtonLabel(currentPhase);
  updateControlFastButtonState();
}

function getSessionCapabilities(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  if (meta.capabilities) return meta.capabilities;
  if (meta.relationshipKind === 'subagent') {
    return { followUp: true, steer: false, interrupt: false, threadActions: [], codexThreadActions: [] };
  }
  return null;
}

// Backend-neutral per-session thread-action vocabulary. The universal
// `threadActions` list wins; `codexThreadActions` is the legacy alias so
// Codex sessions and old replays keep working. `null` means unknown (a
// legacy session that never advertised capabilities) — callers fall back
// to the codex heuristic for those.
function sessionThreadActionOps(sessionId) {
  const capabilities = getSessionCapabilities(sessionId);
  if (!capabilities) return null;
  if (Array.isArray(capabilities.threadActions)) return capabilities.threadActions;
  if (Array.isArray(capabilities.codexThreadActions)) return capabilities.codexThreadActions;
  return null;
}

function sessionCodexFastMode(sessionId) {
  const capabilities = getSessionCapabilities(sessionId);
  if (!capabilities) return null;
  if (typeof capabilities.codexFastMode === 'boolean') return capabilities.codexFastMode;
  const tier = String(capabilities.codexServiceTier || '').trim().toLowerCase();
  if (!tier) return null;
  return tier === 'priority' || tier === 'fast';
}

function sessionCodexServiceTierTitle(sessionId) {
  const fastMode = sessionCodexFastMode(sessionId);
  if (fastMode === null) return 'Codex service tier has not been reported for this session yet';
  const capabilities = getSessionCapabilities(sessionId) || {};
  const rawTier = String(capabilities.codexServiceTier || '').trim();
  const suffix = rawTier ? ` (${rawTier})` : '';
  return fastMode
    ? `Codex Fast is enabled for future turns${suffix}; active turns continue unchanged`
    : `Codex is using the normal service tier${suffix}`;
}

function updateControlFastButtonState() {
  const btn = document.querySelector('.control-action-btn[data-codex-action="fast"]');
  const status = document.getElementById('control-fast-status');
  const sid = resolvePromptTargetSessionId();
  const fastMode = sid ? sessionCodexFastMode(sid) : null;
  if (btn) {
    btn.classList.toggle('active', fastMode === true);
    if (fastMode === null) {
      btn.removeAttribute('aria-pressed');
      btn.title = 'Toggle Codex Fast service tier for future turns (/fast)';
    } else {
      btn.setAttribute('aria-pressed', fastMode ? 'true' : 'false');
      btn.title = `${sessionCodexServiceTierTitle(sid)}. Click to toggle (/fast).`;
    }
  }
  if (status) {
    if (fastMode === null) {
      status.className = 'control-fast-status hidden';
      status.textContent = '';
      status.title = '';
    } else {
      status.className = `control-fast-status ${fastMode ? 'fast' : 'normal'}`;
      status.textContent = fastMode ? 'Fast' : 'Normal';
      status.title = sessionCodexServiceTierTitle(sid);
    }
  }
}

function sessionSupportsSteer(sessionId) {
  const capabilities = getSessionCapabilities(sessionId);
  return capabilities ? capabilities.steer !== false : true;
}

function sessionSupportsInterrupt(sessionId) {
  const capabilities = getSessionCapabilities(sessionId);
  return capabilities ? capabilities.interrupt !== false : true;
}

function codexThreadActionStateForSession(sessionId, op) {
  const normalized = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  if (sessionWindowIsSide(sessionId)) {
    return codexThreadActionAllowedForSide(normalized)
      ? { allowed: true, reason: '' }
      : { allowed: false, reason: 'Unsupported in a /side window' };
  }
  const ops = sessionThreadActionOps(sessionId);
  if (Array.isArray(ops) && !ops.includes(normalized)) {
    return { allowed: false, reason: 'Unsupported for this session' };
  }
  if (sessionWindowIsSubagent(sessionId)) {
    return { allowed: false, reason: 'Unsupported for subagent threads' };
  }
  // Thread actions operate on the attached backend thread, not on an
  // active turn. An attached idle backend can fork/side/compact; only
  // detached history cards need the attach-before-dispatch path.
  if (sessionWindowIsDetached(sessionId)) {
    if (canQueueDetachedCodexThreadAction(sessionId)) {
      return { allowed: true, reason: 'Attach session to run this action' };
    }
    return { allowed: false, reason: 'Attach this session before running thread actions' };
  }
  return { allowed: true, reason: '' };
}

function closeSessionWindowMenus(exceptSessionId = '') {
  const except = String(exceptSessionId || '').trim();
  let closed = false;
  for (const [sid, win] of sessionWindows) {
    if (sid === except || !win.actionMenu) continue;
    if (!win.actionMenu.classList.contains('hidden')) closed = true;
    win.actionMenu.classList.add('hidden');
    win.actionMenu.querySelectorAll('.session-window-submenu.open').forEach(submenu => {
      submenu.classList.remove('open');
      submenu.querySelector('[data-session-window-submenu-trigger]')?.setAttribute('aria-expanded', 'false');
    });
    win.el?.classList.remove('menu-open');
    win.actionMenuButton?.setAttribute('aria-expanded', 'false');
  }
  return closed;
}

function setSessionWindowMenuOpen(sessionId, open) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !win.actionMenu || !win.actionMenuButton) return false;
  const nextOpen = !!open;
  closeSessionWindowMenus(nextOpen ? sid : '');
  win.actionMenu.classList.toggle('hidden', !nextOpen);
  if (!nextOpen) {
    win.actionMenu.querySelectorAll('.session-window-submenu.open').forEach(submenu => {
      submenu.classList.remove('open');
      submenu.querySelector('[data-session-window-submenu-trigger]')?.setAttribute('aria-expanded', 'false');
    });
  }
  win.el.classList.toggle('menu-open', nextOpen);
  win.actionMenuButton.setAttribute('aria-expanded', nextOpen ? 'true' : 'false');
  return nextOpen;
}

function toggleSessionWindowMenu(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !win.actionMenu) return;
  setSessionWindowMenuOpen(sid, win.actionMenu.classList.contains('hidden'));
}

function updateSessionWindowActionMenuVisibility(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !win.actionMenuButton) return;
  const isCodex = sessionWindowIsCodex(sid, win);
  const isSide = sessionWindowIsSide(sid);
  win.actionMenuButton.classList.remove('hidden');
  win.actionMenuButton.disabled = false;
  win.actionMenuButton.title = 'Session actions';
  win.actionMenuButton.setAttribute('aria-label', win.actionMenuButton.title);
  win.actionMenu?.querySelectorAll('[data-session-window-generic-action]').forEach(item => {
    const op = item.dataset.sessionWindowAction || '';
    if (op === 'delegate-sub-agent') {
      // Internal sessions only; external agents delegate through their
      // own start_task tool. A detached session has no live loop to
      // notify, so the item disables with the reason.
      const internal = sessionWindowIsInternal(sid, win);
      const detached = sessionWindowIsDetached(sid);
      item.classList.toggle('hidden', !internal);
      item.disabled = !internal || detached;
      item.title = !internal
        ? 'Delegate targets internal-agent sessions'
        : detached
          ? 'Session is not attached; send it a message first'
          : 'Spawn a supervised sub-agent under this session';
      item.setAttribute('aria-disabled', item.disabled ? 'true' : 'false');
      return;
    }
    if (op === 'attach-session') {
      const visible = canAttachSessionWindow(sid, win);
      item.classList.toggle('hidden', !visible);
      item.disabled = !visible;
      item.title = visible
        ? 'Attach this backend without sending a prompt'
        : 'Session is already attached';
      item.setAttribute('aria-disabled', item.disabled ? 'true' : 'false');
      return;
    }
    if (op === 'stop-session') {
      const availability = sessionWindowStopAvailability(sid);
      item.disabled = !availability.ok;
      item.title = availability.ok
        ? 'Stop the live backend and remove it from active dashboards'
        : availability.reason;
      item.setAttribute('aria-disabled', item.disabled ? 'true' : 'false');
    }
  });
  // Thread-action items gate on the session's ADVERTISED op vocabulary
  // (universal `thread_actions`, legacy `codexThreadActions` alias). Legacy
  // sessions that never advertised capabilities fall back to the codex
  // heuristic so old replays keep their menu.
  const advertisedOps = sessionThreadActionOps(sid);
  const opVisible = (op) => (Array.isArray(advertisedOps)
    ? advertisedOps.includes(String(op || '').trim())
    : isCodex);
  win.actionMenu?.querySelectorAll('[data-session-window-codex-action]').forEach(item => {
    const op = item.dataset.sessionWindowAction || '';
    const spec = sessionWindowCodexActionByOp(op);
    const state = codexThreadActionStateForSession(sid, op);
    const visible = opVisible(op) && !isSide;
    item.classList.toggle('hidden', !visible);
    item.disabled = visible ? !state.allowed : false;
    if (op === 'fast' && state.allowed) {
      const fastMode = sessionCodexFastMode(sid);
      item.classList.toggle('active', fastMode === true);
      if (fastMode === null) item.removeAttribute('aria-pressed');
      else item.setAttribute('aria-pressed', fastMode ? 'true' : 'false');
      item.title = `${sessionCodexServiceTierTitle(sid)}. Click to toggle.`;
    } else {
      item.classList.remove('active');
      item.removeAttribute('aria-pressed');
      item.title = state.allowed ? (spec?.title || '') : state.reason;
    }
    item.setAttribute('aria-disabled', item.disabled ? 'true' : 'false');
  });
  win.actionMenu?.querySelectorAll('[data-session-window-codex-submenu]').forEach(submenu => {
    const childActions = Array.from(submenu.querySelectorAll('[data-session-window-codex-action]'));
    const anyChildVisible = childActions.some(item => !item.classList.contains('hidden'));
    submenu.classList.toggle('hidden', !anyChildVisible || isSide);
    if (!anyChildVisible || isSide) submenu.classList.remove('open');
    const trigger = submenu.querySelector('[data-session-window-submenu-trigger]');
    const hasEnabledChild = childActions.some(item => !item.disabled && !item.classList.contains('hidden'));
    if (trigger) {
      trigger.disabled = anyChildVisible && !isSide ? !hasEnabledChild : false;
      trigger.setAttribute('aria-disabled', trigger.disabled ? 'true' : 'false');
      trigger.setAttribute('aria-expanded', submenu.classList.contains('open') ? 'true' : 'false');
    }
  });
  win.actionMenu?.querySelectorAll('[data-session-window-codex-separator]').forEach(item => {
    const anyThreadActionVisible = !!win.actionMenu.querySelector(
      '[data-session-window-codex-action]:not(.hidden), [data-session-window-codex-submenu]:not(.hidden)'
    );
    item.classList.toggle('hidden', !anyThreadActionVisible || isSide);
  });
}

function normalizeSessionRelationshipKind(value) {
  const kind = String(value || '').trim().toLowerCase().replace(/_/g, '-');
  if (kind === 'sub-agent') return 'subagent';
  return SESSION_RELATIONSHIP_KINDS.has(kind) ? kind : 'fork';
}

function sessionRelationshipKey(parentId, childId, kind) {
  return `${kind}\u001f${parentId}\u001f${childId}`;
}

function sessionRowIds(session) {
  if (!session || typeof session !== 'object') return [];
  return [
    session.session_id,
    session.resume_id,
    session.backend_session_id,
    session.backendSessionId,
    session.intendant_session_id,
    session.intendantSessionId,
  ].map(id => String(id || '').trim()).filter(Boolean);
}

function sessionRowMatchesId(session, id) {
  const target = String(id || '').trim();
  return !!target && sessionRowIds(session).includes(target);
}

function cachedSessionRowForId(id) {
  const target = String(id || '').trim();
  if (!target || !Array.isArray(_cachedSessions)) return null;
  return _cachedSessions.find(session => sessionRowMatchesId(session, target)) || null;
}

function sessionRowsReferToSameSession(a, b) {
  const ids = new Set(sessionRowIds(a));
  return sessionRowIds(b).some(id => ids.has(id));
}

function mergeSessionRowsIntoLoadedSessions(rows) {
  if (!Array.isArray(rows) || rows.length === 0 || !sessionsLoaded) return;
  const next = Array.isArray(_cachedSessions) ? _cachedSessions.slice() : [];
  let changed = false;
  for (const row of rows) {
    if (!row || typeof row !== 'object') continue;
    const index = next.findIndex(existing => sessionRowsReferToSameSession(existing, row));
    if (index >= 0) {
      next[index] = { ...next[index], ...row };
    } else {
      next.unshift(row);
    }
    changed = true;
  }
  if (!changed) return;
  _cachedSessions = next;
  sessionsListCache.set(sessionListCacheKey(selfPeerId), next);
  updateSessionProjectFilterOptions(next);
  updateNewSessionProjectPrefills(next);
  renderSessionsAggregate(next, document.getElementById('sessions-aggregate'));
  renderSessionsViews();
}

function hydrateSessionRelationshipRows(rel) {
  if (!rel || processingLogReplay) return;
  const ids = Array.from(new Set([rel.parentId, rel.childId].filter(Boolean)));
  if (ids.length === 0) return;
  if (ids.every(id => cachedSessionRowForId(id))) return;
  const key = ids.slice().sort().join('\u001f');
  if (sessionRelationshipHydrationInFlight.has(key)) return;
  sessionRelationshipHydrationInFlight.add(key);
  const url = `/api/sessions?ids=${encodeURIComponent(ids.join(','))}`;
  dashboardJsonFetch('api_sessions', { ids }, () => authedFetch(url), 'api_sessions_relationships')
    .then(r => r.ok ? r.json() : Promise.reject(new Error(`${url} returned ${r.status}`)))
    .then(rows => {
      if (!Array.isArray(rows)) return;
      cacheSessionWindowMetadata(rows);
      mergeSessionRowsIntoLoadedSessions(rows);
    })
    .catch(() => {})
    .finally(() => {
      sessionRelationshipHydrationInFlight.delete(key);
    });
}

function applySessionRelationship(evt = {}) {
  const parentId = String(evt.parent_session_id || evt.parentSessionId || '').trim();
  const childId = String(evt.child_session_id || evt.childSessionId || '').trim();
  if (!parentId || !childId || parentId === childId) return;
  const kind = normalizeSessionRelationshipKind(evt.relationship || evt.kind);
  const relationship = {
    parentId,
    childId,
    kind,
    ephemeral: !!evt.ephemeral,
  };
  sessionRelationships.set(sessionRelationshipKey(parentId, childId, kind), relationship);
  applySessionRelationshipMetadata(relationship);
  hydrateSessionRelationshipRows(relationship);
  scheduleSessionRelationshipRender();
}

function applySessionRelationshipMetadata(rel) {
  if (!rel) return;
  const parentMeta = sessionMetadataById.get(rel.parentId) || {};
  const parentChildren = Array.isArray(parentMeta.children) ? parentMeta.children.slice() : [];
  if (!parentChildren.some(child => child.id === rel.childId && child.kind === rel.kind)) {
    parentChildren.push({ id: rel.childId, kind: rel.kind, ephemeral: rel.ephemeral });
  }
  sessionMetadataById.set(rel.parentId, { ...parentMeta, children: parentChildren });

  const childMeta = sessionMetadataById.get(rel.childId) || {};
  const childHadRelationship = !!childMeta.relationshipKind;
  const inheritedMeta = {};
  for (const key of ['projectRoot', 'projectLabel', 'cwd', 'cwdLabel', 'source', 'sourceLabel', 'backendSource']) {
    if (!childMeta[key] && parentMeta[key]) inheritedMeta[key] = parentMeta[key];
  }
  sessionMetadataById.set(rel.childId, {
    ...childMeta,
    ...inheritedMeta,
    parentId: rel.parentId,
    relationshipKind: rel.kind,
    relationshipEphemeral: rel.ephemeral,
  });

  updateSessionRelationshipBadges(rel.parentId);
  updateSessionRelationshipBadges(rel.childId);
  if (sessionWindows.has(rel.childId) && Object.keys(inheritedMeta).length > 0) {
    updateSessionWindow(rel.childId, inheritedMeta);
  }
  if (rel.kind === 'subagent' && !childHadRelationship && sessionWindows.has(rel.childId)) {
    setSessionWindowHeaderCollapsed(rel.childId, true);
  }
  updateSessionWindowActionMenuVisibility(rel.childId);
  updateSessionWindowRelationshipStyle(rel.childId);
}

function activeSessionRelationshipIds() {
  const focused = foregroundSessionFullId || currentSessionFullId || '';
  if (!focused) return { parents: new Set(), children: new Set(), activeKeys: new Set() };
  const parents = new Set();
  const children = new Set();
  const activeKeys = new Set();
  for (const [key, rel] of sessionRelationships) {
    if (rel.childId === focused) {
      parents.add(rel.parentId);
      children.add(rel.childId);
      activeKeys.add(key);
    } else if (rel.parentId === focused) {
      parents.add(rel.parentId);
      children.add(rel.childId);
      activeKeys.add(key);
    }
  }
  return { parents, children, activeKeys };
}

function pruneClosedSideChildrenForParent(parentId, meta) {
  const sid = String(parentId || '').trim();
  if (!sid || !meta || !Array.isArray(meta.children)) return meta || {};
  let changed = false;
  const children = [];
  for (const child of meta.children) {
    const childId = String(child?.id || '').trim();
    const kind = String(child?.kind || '').trim().toLowerCase();
    const childWin = childId ? sessionWindows.get(childId) : null;
    const closedSideChild = kind === 'side' && (!childId || !childWin || childWin.ended);
    if (closedSideChild) {
      changed = true;
      if (childId) {
        sessionRelationships.delete(sessionRelationshipKey(sid, childId, kind));
        const childMeta = sessionMetadataById.get(childId);
        if (childMeta && childMeta.parentId === sid && childMeta.relationshipKind === kind) {
          const nextChildMeta = { ...childMeta };
          delete nextChildMeta.parentId;
          delete nextChildMeta.relationshipKind;
          delete nextChildMeta.relationshipEphemeral;
          sessionMetadataById.set(childId, nextChildMeta);
        }
      }
      continue;
    }
    children.push(child);
  }
  if (!changed) return meta;
  const nextMeta = { ...meta, children };
  sessionMetadataById.set(sid, nextMeta);
  scheduleSessionRelationshipRender();
  return nextMeta;
}

function sessionRelationshipSubagentIsActive(childId) {
  const sid = String(childId || '').trim();
  if (!sid) return false;
  const win = sessionWindows.get(sid);
  const meta = sessionMetadataById.get(sid) || {};
  if (win?.ended || meta.ended) return false;
  if (hasPendingActiveSessionWindow(sid)) return true;
  const phase = normalizeSessionPhase(win?.phase || meta.phase || '');
  return isAgentActivePhase(phase);
}

function updateSessionRelationshipBadges(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !win.relationStrip) return;
  const meta = pruneClosedSideChildrenForParent(sid, sessionMetadataById.get(sid) || {});
  const chips = [];
  if (meta.relationshipKind && meta.parentId) {
    chips.push({
      kind: meta.relationshipKind,
      text: meta.relationshipKind === 'subagent' ? 'sub' : meta.relationshipKind,
      title: `${meta.relationshipKind} of ${shortSessionId(meta.parentId)}. Click to focus parent.`,
      target: meta.parentId,
    });
  }
  const children = Array.isArray(meta.children) ? meta.children : [];
  const activeChildren = children.filter(child => {
    const kind = String(child?.kind || '').trim().toLowerCase();
    return kind !== 'subagent' || sessionRelationshipSubagentIsActive(child.id);
  });
  if (activeChildren.length > 0) {
    const sides = activeChildren.filter(child => child.kind === 'side').length;
    const forks = activeChildren.filter(child => child.kind === 'fork').length;
    const subs = activeChildren.filter(child => child.kind === 'subagent').length;
    const label = [
      sides ? `${sides} side` : '',
      forks ? `${forks} fork` : '',
      subs ? `${subs} sub` : '',
    ].filter(Boolean).join(' ');
    if (label) {
      chips.push({
        kind: 'children',
        text: label,
        title: `${activeChildren.length} active related session${activeChildren.length === 1 ? '' : 's'}. Click to highlight children.`,
        target: '',
      });
    }
  }

  win.relationStrip.replaceChildren();
  win.relationStrip.classList.toggle('hidden', chips.length === 0);
  for (const chip of chips) {
    const el = document.createElement('button');
    el.type = 'button';
    el.className = `session-window-relation-chip ${chip.kind}`;
    el.textContent = chip.text;
    el.title = chip.title;
    el.setAttribute('aria-label', chip.title);
    el.addEventListener('click', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      if (chip.target && sessionWindows.has(chip.target)) {
        focusSessionWindow(chip.target);
      } else {
        focusSessionWindow(sid);
      }
    });
    win.relationStrip.appendChild(el);
  }
}

function updateSessionWindowRelationshipStyle(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win) return;
  const meta = sessionMetadataById.get(sid) || {};
  win.el.classList.toggle('relationship-ephemeral', !!meta.relationshipEphemeral);
}

function removeSessionRelationshipsForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const affected = new Set([sid]);
  for (const [key, rel] of Array.from(sessionRelationships.entries())) {
    if (rel.parentId !== sid && rel.childId !== sid) continue;
    sessionRelationships.delete(key);
    affected.add(rel.parentId);
    affected.add(rel.childId);

    const parentMeta = sessionMetadataById.get(rel.parentId);
    if (parentMeta && Array.isArray(parentMeta.children)) {
      sessionMetadataById.set(rel.parentId, {
        ...parentMeta,
        children: parentMeta.children.filter(child => !(child.id === rel.childId && child.kind === rel.kind)),
      });
    }

    if (rel.childId === sid) {
      const childMeta = sessionMetadataById.get(rel.childId);
      if (childMeta) {
        const nextChildMeta = { ...childMeta };
        delete nextChildMeta.parentId;
        delete nextChildMeta.relationshipKind;
        delete nextChildMeta.relationshipEphemeral;
        sessionMetadataById.set(rel.childId, nextChildMeta);
      }
    }
  }
  for (const id of affected) {
    updateSessionRelationshipBadges(id);
    updateSessionWindowRelationshipStyle(id);
  }
  scheduleSessionRelationshipRender();
}

function sideCloseParamsForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  if (meta.relationshipKind !== 'side' || !meta.parentId) return null;
  const parentMeta = sessionMetadataById.get(meta.parentId) || {};
  const parentThreadId = parentMeta.backendSessionId || meta.parentId;
  if (!parentThreadId) return null;
  return {
    parentSessionId: meta.parentId,
    params: {
      threadId: sid,
      parentThreadId,
    },
  };
}

function maybeDispatchSideClose(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!sid || win?.ended) return false;
  const close = sideCloseParamsForSession(sid);
  if (!close) return false;
  return dispatchCodexThreadAction('side-close', close.params, close.parentSessionId, {
    internalSideClose: true,
  });
}

function removeSessionWindow(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  const win = sessionWindows.get(sid);
  if (win) {
    for (const entry of win.logHistory || []) {
      if (entry?.dataset) delete entry.dataset.sessionWindowHistory;
    }
    win.el.remove();
    sessionWindows.delete(sid);
  }
  if (maximizedSessionWindowId === sid) maximizedSessionWindowId = '';
  if (foregroundSessionFullId === sid) {
    const next = sessionWindows.keys().next().value || '';
    if (next) focusSessionWindow(next);
    else {
      foregroundSessionFullId = '';
      if (currentSessionFullId === sid) {
        currentSessionFullId = '';
      }
      setPhase('idle');
      updateTaskTargetChip();
    }
  } else if (currentSessionFullId === sid) {
    currentSessionFullId = '';
    if (!resolvePromptTargetSessionId()) setPhase('idle');
    updateTaskTargetChip();
  }
  const grid = document.getElementById('session-window-grid');
  if (grid && !grid.querySelector('.session-window')) grid.classList.add('hidden');
  updateSessionWindowMaximizeState();
  applySessionWindowGridHeight();
  syncSessionWindowMetadataRefresh();
  scheduleSessionRelationshipRender();
  persistSessionWindowState();
  return !!win;
}

function scheduleSessionRelationshipRender() {
  if (sessionRelationshipRenderHandle) return;
  sessionRelationshipRenderHandle = requestAnimationFrame(() => {
    sessionRelationshipRenderHandle = 0;
    renderSessionRelationships();
  });
}

function ensureSessionRelationshipOverlay(grid) {
  let svg = grid.querySelector(':scope > svg.session-relationship-wires');
  if (!svg) {
    svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.classList.add('session-relationship-wires');
    svg.setAttribute('aria-hidden', 'true');
    grid.prepend(svg);
  }
  return svg;
}

function sessionWindowAnchor(rect, side) {
  switch (side) {
    case 'left': return { x: rect.left, y: rect.top + rect.height / 2 };
    case 'right': return { x: rect.right, y: rect.top + rect.height / 2 };
    case 'bottom': return { x: rect.left + rect.width / 2, y: rect.bottom };
    case 'top': return { x: rect.left + rect.width / 2, y: rect.top };
    default: return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
  }
}

function sessionWindowRectInGrid(grid, win) {
  const gridRect = grid.getBoundingClientRect();
  const rect = win.el.getBoundingClientRect();
  return {
    left: rect.left - gridRect.left + grid.scrollLeft,
    top: rect.top - gridRect.top + grid.scrollTop,
    right: rect.right - gridRect.left + grid.scrollLeft,
    bottom: rect.bottom - gridRect.top + grid.scrollTop,
    width: rect.width,
    height: rect.height,
  };
}

function relationshipPath(parentRect, childRect, kind, direction, trunkValue) {
  if (kind === 'subagent') {
    const parent = sessionWindowAnchor(parentRect, 'bottom');
    const child = sessionWindowAnchor(childRect, 'top');
    const trunkY = trunkValue ?? ((parent.y + child.y) / 2);
    return `M ${parent.x} ${parent.y} V ${trunkY} H ${child.x} V ${child.y}`;
  }
  const childIsRight = direction === 'right';
  const parent = sessionWindowAnchor(parentRect, childIsRight ? 'right' : 'left');
  const child = sessionWindowAnchor(childRect, childIsRight ? 'left' : 'right');
  const trunkX = trunkValue ?? ((parent.x + child.x) / 2);
  return `M ${parent.x} ${parent.y} H ${trunkX} V ${child.y} H ${child.x}`;
}

function relationshipEndpointForChild(childRect, kind, direction) {
  if (kind === 'subagent') return sessionWindowAnchor(childRect, 'top');
  return sessionWindowAnchor(childRect, direction === 'right' ? 'left' : 'right');
}

function renderSessionRelationships() {
  const grid = document.getElementById('session-window-grid');
  if (!grid) return;
  const svg = ensureSessionRelationshipOverlay(grid);
  svg.replaceChildren();
  const previousDisplay = svg.style.display || '';
  svg.style.display = 'none';
  const overlayWidth = Math.max(grid.scrollWidth, grid.clientWidth);
  const overlayHeight = Math.max(grid.scrollHeight, grid.clientHeight);
  svg.style.display = previousDisplay;
  svg.setAttribute('width', overlayWidth);
  svg.setAttribute('height', overlayHeight);
  svg.setAttribute('viewBox', `0 0 ${overlayWidth} ${overlayHeight}`);

  for (const [sid, win] of sessionWindows) {
    win.el.classList.remove('relationship-parent-highlight', 'relationship-child-highlight');
    updateSessionRelationshipBadges(sid);
  }

  const active = activeSessionRelationshipIds();
  for (const sid of active.parents) sessionWindows.get(sid)?.el.classList.add('relationship-parent-highlight');
  for (const sid of active.children) sessionWindows.get(sid)?.el.classList.add('relationship-child-highlight');

  const rects = new Map();
  for (const [sid, win] of sessionWindows) {
    if (win.minimized && win.el.offsetParent === null) continue;
    rects.set(sid, sessionWindowRectInGrid(grid, win));
  }

  const visible = Array.from(sessionRelationships.entries())
    .filter(([, rel]) => rects.has(rel.parentId) && rects.has(rel.childId));
  const trunkByGroup = new Map();
  for (const [, rel] of visible) {
    const parentRect = rects.get(rel.parentId);
    const childRect = rects.get(rel.childId);
    const kind = rel.kind;
    const direction = kind === 'subagent'
      ? 'down'
      : ((childRect.left + childRect.width / 2) >= (parentRect.left + parentRect.width / 2) ? 'right' : 'left');
    const groupKey = `${rel.parentId}\u001f${kind}\u001f${direction}`;
    if (kind === 'subagent') {
      const candidate = Math.min(childRect.top - 14, parentRect.bottom + 22);
      trunkByGroup.set(groupKey, Math.max(parentRect.bottom + 12, candidate));
    } else if (direction === 'right') {
      const candidate = Math.min(childRect.left - 14, parentRect.right + 28);
      const current = trunkByGroup.get(groupKey);
      trunkByGroup.set(groupKey, current === undefined ? Math.max(parentRect.right + 12, candidate) : Math.min(current, Math.max(parentRect.right + 12, candidate)));
    } else {
      const candidate = Math.max(childRect.right + 14, parentRect.left - 28);
      const current = trunkByGroup.get(groupKey);
      trunkByGroup.set(groupKey, current === undefined ? Math.min(parentRect.left - 12, candidate) : Math.max(current, Math.min(parentRect.left - 12, candidate)));
    }
  }

  for (const [key, rel] of visible) {
    const parentRect = rects.get(rel.parentId);
    const childRect = rects.get(rel.childId);
    const kind = rel.kind;
    const direction = kind === 'subagent'
      ? 'down'
      : ((childRect.left + childRect.width / 2) >= (parentRect.left + parentRect.width / 2) ? 'right' : 'left');
    const groupKey = `${rel.parentId}\u001f${kind}\u001f${direction}`;
    const activeClass = active.activeKeys.has(key) ? ' active' : '';
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.classList.add('session-relationship-wire', kind);
    if (activeClass) path.classList.add('active');
    path.setAttribute('d', relationshipPath(parentRect, childRect, kind, direction, trunkByGroup.get(groupKey)));
    svg.appendChild(path);

    const endpoint = relationshipEndpointForChild(childRect, kind, direction);
    const dot = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
    dot.classList.add('session-relationship-endpoint', kind);
    if (activeClass) dot.classList.add('active');
    dot.setAttribute('cx', endpoint.x);
    dot.setAttribute('cy', endpoint.y);
    dot.setAttribute('r', kind === 'side' ? '3.5' : '4');
    svg.appendChild(dot);
  }
}

function sessionWindowExternalActionContext(sessionId) {
  const sid = String(sessionId || '').trim();
  const availability = sessionWindowExternalActionAvailability(sid);
  if (!availability.ok) {
    if (availability.toast) showControlToast('error', availability.toast);
    return null;
  }
  return availability.msg;
}

function sessionWindowExternalActionAvailability(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return { ok: false, reason: 'No session selected.', toast: '' };
  if (sessionWindowIsSide(sid) || sessionWindowIsSubagent(sid)) {
    return {
      ok: false,
      reason: 'This is a related session. Stop the parent session instead.',
      toast: 'Stop the parent session instead',
    };
  }
  const source = externalSourceForSessionWindow(sid);
  if (!source || source === 'intendant') {
    return {
      ok: false,
      reason: 'Stop session is only available for external-agent sessions.',
      toast: 'This action is only available for external-agent sessions',
    };
  }
  const msg = detachedSessionResumeMessage(sid, null, true, []);
  if (!msg || msg.source === 'intendant') {
    return {
      ok: false,
      reason: 'Could not resolve the external session launch context.',
      toast: 'Could not resolve the external session launch context',
    };
  }
  return { ok: true, reason: '', toast: '', msg };
}

function sessionWindowStopAvailability(sessionId) {
  const sid = String(sessionId || '').trim();
  const availability = sessionWindowExternalActionAvailability(sid);
  if (!availability.ok) return availability;
  if (sessionWindowIsDetached(sid)) {
    return {
      ok: false,
      reason: 'This session is not attached to a live backend. Hide the card, or attach it before stopping.',
      toast: 'Attach this session before stopping it',
    };
  }
  return availability;
}

async function stopSessionWindowAction(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  const availability = sessionWindowStopAvailability(sid);
  if (!sid || !availability.ok) {
    if (availability.toast) showControlToast('error', availability.toast);
    return;
  }
  if (!options.skipConfirm) {
    const ok = await showDashboardConfirm({
      title: 'Stop session',
      message: 'Stop this live backend? Its history remains available in Sessions, but this card will not be restored after refresh or in other browsers until you resume it.',
      confirmLabel: 'Stop session',
    });
    if (!ok) return;
  }
  dispatchControlMsg({ action: 'stop_session', session_id: sid });
  clearPendingFollowUpsForSession(sid, 'session stopped');
  showControlToast('info', `Stopping session ${shortSessionId(sid)}`);
}

function hideSessionWindowAction(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  if ((sessionMetadataById.get(sid) || {}).relationshipKind === 'side') {
    removeSessionRelationshipsForSession(sid);
  }
  removeSessionWindow(sid);
}

function closeSideSessionWindowAction(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  if (maybeDispatchSideClose(sid)) {
    showControlToast('info', `Closing side ${shortSessionId(sid)}`);
  }
}

async function chooseSessionWindowCloseAction(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const stopAvailability = sessionWindowStopAvailability(sid);
  const canCloseSide = !stopAvailability.ok && !!sideCloseParamsForSession(sid);
  const alternateLabel = stopAvailability.ok
    ? 'Stop session'
    : canCloseSide
      ? 'Close side'
      : '';
  const result = await showDashboardConfirm({
    title: stopAvailability.ok
      ? 'Hide or stop session?'
      : canCloseSide
        ? 'Hide or close side?'
        : 'Hide session card?',
    message: stopAvailability.ok
      ? 'Hide card only removes this card from this dashboard. Stop session ends the live backend and removes it from active dashboards.'
      : canCloseSide
      ? 'Hide card only removes this card from this dashboard. Close side ends the side conversation in the parent Codex thread.'
      : 'Hide card only removes this card from this dashboard.',
    warning: stopAvailability.ok
      ? 'Session history remains available either way.'
      : canCloseSide
      ? 'The parent session remains available either way.'
      : stopAvailability.reason,
    confirmLabel: 'Hide card',
    confirmValue: 'hide',
    confirmTitleAttr: 'Remove this card from this dashboard only',
    cancelLabel: 'Cancel',
    danger: false,
    ...(alternateLabel ? {
      alternateLabel,
      alternateValue: stopAvailability.ok ? 'stop' : 'close-side',
      alternateTitle: stopAvailability.ok ? 'Stop the live backend' : 'Close the side conversation',
    } : {}),
  });
  if (result === 'hide') {
    hideSessionWindowAction(sid);
  } else if (result === 'stop') {
    await stopSessionWindowAction(sid, { skipConfirm: true });
  } else if (result === 'close-side') {
    closeSideSessionWindowAction(sid);
  }
}

function restartSessionWindowAction(sessionId) {
  const sid = String(sessionId || '').trim();
  const msg = sessionWindowExternalActionContext(sid);
  if (!msg) return;
  msg.action = 'restart_session';
  delete msg.task;
  delete msg.attachments;
  dispatchControlMsg(msg);
  setSessionWindowDetached(sid, true, 'restarting session');
  showControlToast('info', `Restarting session ${shortSessionId(sid)} with saved config`);
}

function attachSessionWindowAction(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!canAttachSessionWindow(sid)) {
    showControlToast('info', `Session ${shortSessionId(sid)} is already attached`);
    return;
  }
  const msg = sessionWindowExternalActionContext(sid);
  if (!msg) return;
  delete msg.task;
  delete msg.attachments;
  dispatchControlMsg(msg);
  showControlToast('info', `Attaching session ${shortSessionId(sid)}`);
}

