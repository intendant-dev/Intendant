async function stationSelectChange(path) {
  stationSelectedChangePath = path || stationSelectedChangePath || activeChangesFile || '';
  if (stationSelectedChangePath) {
    await selectChangesFile(stationSelectedChangePath);
  }
  stationScheduleUpdate();
}

function stationChangedPaths() {
  return [...changedFiles.keys()].sort((a, b) => a.localeCompare(b));
}

async function stationCopyChangedPaths() {
  const paths = stationChangedPaths();
  if (!paths.length) {
    showControlToast?.('info', 'No changed files to copy');
    return;
  }
  await copyTextToClipboard(paths.join('\n'));
  showControlToast?.('info', `Copied ${paths.length} changed file path${paths.length === 1 ? '' : 's'}`);
}

async function stationCopyChangeDiff(pathArg = '') {
  const path = String(pathArg || stationSelectedChangePath || activeChangesFile || '').trim();
  if (!path) {
    showControlToast?.('error', 'Select a changed file before copying a diff');
    return;
  }
  const resp = await fetchChangesResponse(path);
  const data = await parseChangesResponse(resp);
  if (data.diff_available === false) {
    throw new Error(data.reason || 'Textual diff is unavailable for this file.');
  }
  const diff = String(data.diff || '').trimEnd();
  if (!diff) {
    showControlToast?.('info', 'Selected file has no text diff to copy');
    return;
  }
  await copyTextToClipboard(diff);
  showControlToast?.('info', `Copied diff for ${path}`);
}

async function stationRefreshChangesHistory() {
  if (stationChangesHistoryLoading) return;
  stationChangesHistoryLoading = true;
  try {
    if (typeof refreshHistory === 'function') await refreshHistory();
  } catch (err) {
    showControlToast?.('error', err?.message || 'History refresh failed.');
  } finally {
    stationChangesHistoryLoading = false;
    stationScheduleUpdate();
  }
}

async function stationRunChangesRedo(selectedPath = stationSelectedChangePath || activeChangesFile || '') {
  if (typeof doRedo === 'function') await doRedo();
  await stationRefreshChangesHistory(selectedPath);
}

async function stationRunChangesPrune(selectedPath = stationSelectedChangePath || activeChangesFile || '') {
  if (typeof doPrune === 'function') await doPrune();
  await stationRefreshChangesHistory(selectedPath);
}

// Merging the session index, live windows, metadata, and the daemon session
// (plus the recency sort and byte/token totals) is too expensive to redo on
// every coalesced snapshot rebuild. Cache the derived set; session-list loads
// invalidate it explicitly (stationInvalidateSessionSet) and a short TTL
// covers window/metadata churn that has no single mutation choke point.
let stationSessionSetCache = null;
const STATION_SESSION_SET_TTL_MS = 1500;

function stationInvalidateSessionSet() {
  stationSessionSetCache = null;
}

function stationCollectSessionSet() {
  const now = Date.now();
  if (stationSessionSetCache && now - stationSessionSetCache.at < STATION_SESSION_SET_TTL_MS) {
    return stationSessionSetCache;
  }
  const indexedSessions = Array.isArray(_cachedSessions) && _cachedSessions.length
    ? _cachedSessions
    : (sessionsListCache.get(sessionListCacheKey(selfPeerId)) || []);
  const byId = new Map();
  const indexedIds = new Set();
  for (const session of indexedSessions) {
    const id = session?.session_id || session?.resume_id || session?.backend_session_id || JSON.stringify(session);
    byId.set(String(id), session);
    indexedIds.add(String(id));
  }
  for (const session of stationSessionWindowSummaries()) {
    const id = session?.session_id || session?.resume_id || session?.backend_session_id || JSON.stringify(session);
    if (!byId.has(String(id))) byId.set(String(id), session);
  }
  for (const session of stationSessionMetadataSummaries()) {
    const id = session?.session_id || session?.resume_id || session?.backend_session_id || JSON.stringify(session);
    if (!byId.has(String(id))) byId.set(String(id), session);
  }
  const currentDaemonSession = stationCurrentDaemonSessionSummary();
  if (currentDaemonSession) {
    const id = currentDaemonSession.session_id || JSON.stringify(currentDaemonSession);
    if (!byId.has(String(id))) byId.set(String(id), currentDaemonSession);
  }
  const sessions = [...byId.values()];
  const sortedSessions = [...sessions].sort((a, b) => stationSessionUpdatedMs(b) - stationSessionUpdatedMs(a));
  let totalTokens = 0;
  let diskBytes = 0;
  for (const session of sessions) {
    totalTokens += stationNum(session?.total_tokens);
    diskBytes += stationSessionBytes(session);
  }
  stationSessionSetCache = { at: now, sessions, indexedIds, sortedSessions, totalTokens, diskBytes };
  return stationSessionSetCache;
}

function stationSessionRow(session, indexedIds = new Set()) {
  const id = session?.session_id || session?.resume_id || session?.backend_session_id || '';
  const status = normalizeSessionPhase(session?.status || '') || String(session?.status || 'session').trim();
  const configMeta = sessionConfigMetadata(session);
  const configSource = sessionConfigSource(configMeta);
  const source = normalizeAgentId(configSource || stationSessionSource(session) || session?.source || '') || 'intendant';
  const project = sessionProjectDirectory(session);
  const rowId = String(id || '').trim();
  const backendId = String(configMeta.backendSessionId || configMeta.backend_session_id || '').trim();
  const intendantId = String(configMeta.intendantSessionId || configMeta.intendant_session_id || '').trim();
  const live = stationExternalLiveThreadDescriptor(session || rowId);
  const command = sessionConfigCommand(configMeta);
  const managedMode = source === 'codex' ? sessionConfigManagedMode(configMeta) : '';
  const archiveMode = source === 'codex' ? sessionConfigArchiveMode(configMeta) : '';
  const sandboxMode = source === 'codex' ? sessionLaunchSandboxMode(configMeta) : '';
  const approvalPolicy = source === 'codex' ? sessionLaunchApprovalPolicy(configMeta) : '';
  const launchPersistent = source !== 'intendant' && !!(
    command ||
    sessionLaunchManagedMode(configMeta) ||
    sessionLaunchArchiveMode(configMeta) ||
    sandboxMode ||
    approvalPolicy
  );
  const goal = normalizeSessionGoal(configMeta.goal || configMeta.session_goal || configMeta.sessionGoal || null);
  const threadActionSessionId = stationCodexThreadActionSessionId(session) || rowId;
  return {
    id: rowId,
    action: indexedIds.has(String(id)) ? 'detail' : '',
    label: stationSessionTask(session)
      || (session?.role === 'resident' || session?.status === 'resident' ? 'Daemon session' : 'Untitled session'),
    value: [status, source].filter(Boolean).join(' · '),
    detail: [
      String(id).slice(0, 8),
      project ? compactPathLabel(project, true) : '',
      formatContextTimestamp(session?.updated_at || session?.updatedAt || session?.changed_at || session?.changedAt || session?.created_at || session?.createdAt),
    ].filter(Boolean).join(' · '),
    tone: 'session',
    source,
    status,
    project,
    backendId,
    intendantId,
    liveId: live?.liveId || '',
    actionId: live?.actionId || '',
    attachId: live?.attachId || '',
    stopId: live?.stopId || '',
    externalStatus: live?.target ? 'target' : (live?.detached ? 'detached' : (live?.phase || status)),
    externalDetached: !!live?.detached,
    livePhase: live?.phase || status || '',
    command: source === 'intendant' ? 'internal' : (command || 'global default'),
    managedContext: managedMode,
    contextArchive: archiveMode,
    launchPersistent,
    isCodex: source === 'codex' || stationSessionLooksCodex(rowId),
    threadActionSessionId,
    goalStatus: goal?.status || '',
    goalObjective: goal?.objective || '',
    goalTokens: goal?.tokensUsed !== null && goal?.tokensUsed !== undefined
      ? String(goal.tokensUsed)
      : '',
    goalTokenBudget: goal?.tokenBudget !== null && goal?.tokenBudget !== undefined
      ? String(goal.tokenBudget)
      : '',
    canResume: !!rowId && session?.can_resume !== false,
    canConfig: !!rowId && !!configSource && configSource !== 'intendant',
    canRename: !!rowId,
    canFocus: !!rowId && (!!live?.liveId || sessionWindows.has(rowId)),
    canAttach: !!rowId && canAttachSessionWindow(rowId),
    canStop: !!rowId && sessionWindowStopAvailability(rowId).ok,
    canInterrupt: false,
    canRestart: !!rowId && !!configSource && configSource !== 'intendant',
    canOpenLog: !!rowId,
    canFork: !!rowId && (
      source === 'codex' ||
      stationSessionLooksCodex(threadActionSessionId || rowId) ||
      (sessionThreadActionOps(threadActionSessionId || rowId) || []).includes('fork')
    ),
  };
}

function stationLaunchDraftValue(key, fallback = '') {
  const value = stationLaunchDraft[key];
  return value === undefined || value === null || value === '' ? fallback : value;
}

function stationEffectiveLaunchAgent() {
  const selected = stationLaunchDraft.agent;
  if (selected === 'internal') return 'internal';
  return normalizeAgentId(selected) || newSessionConfiguredAgent || '';
}

// Execution shape (auto / orchestrate / direct) only applies to the
// internal agent — external CLIs run their own loops.
function stationLaunchExecutionApplies(agent = stationEffectiveLaunchAgent()) {
  return !agent || agent === 'internal' || agent === 'intendant';
}

function stationLaunchExecution() {
  if (!stationLaunchExecutionApplies()) return '';
  const mode = String(stationLaunchDraft.execution || '');
  return mode === 'orchestrate' || mode === 'direct' ? mode : '';
}


function stationLaunchCommandForAgent(agent) {
  const backend = normalizeAgentId(agent);
  if (!backend) return '';
  if (backend === 'codex') return controlCodexConfig.command || commandDefaultForNewSessionAgent('codex') || 'codex';
  return commandDefaultForNewSessionAgent(backend) || backend;
}

function stationLaunchDraftTask() {
  return String(stationLaunchDraftValue('task', document.getElementById('activity-task-input')?.value || '')).trim();
}

function stationLaunchDraftProject() {
  return String(stationLaunchDraftValue('project', newSessionProjectInputValue?.() || dashboardProjectRoot || '')).trim();
}

function stationLaunchDraftCommand(agent = stationEffectiveLaunchAgent()) {
  if (!agent || agent === 'internal') return '';
  return String(stationLaunchDraftValue('command', stationLaunchCommandForAgent(agent))).trim();
}

function stationLaunchNoticeText() {
  const notice = document.getElementById('new-session-spawn-notice');
  const visibleNotice = notice && !notice.classList.contains('hidden')
    ? document.getElementById('new-session-spawn-text')?.textContent?.trim()
    : '';
  if (visibleNotice) return visibleNotice;
  if (newSessionSpawnPending) {
    return `Spawning ${compactSessionText(newSessionSpawnName || newSessionSpawnTask || 'new session')}`;
  }
  if (newSessionSpawnRecent?.sessionId) {
    return `Last launch: ${shortSessionId(newSessionSpawnRecent.sessionId)}`;
  }
  return '';
}

function stationBuildLaunchReadiness() {
  const agent = stationEffectiveLaunchAgent() || 'internal';
  const command = stationLaunchDraftCommand(agent);
  const task = stationLaunchDraftTask();
  const project = stationLaunchDraftProject();
  const attachments = Array.isArray(pendingAttachments) ? pendingAttachments.length : 0;
  const missing = [];
  if (!task) missing.push('task');
  if (!app) missing.push('dashboard connection');
  if (newSessionSpawnPending) missing.push('pending spawn');
  if (agent && agent !== 'internal' && !command) missing.push('agent binary');
  const codex = agent === 'codex';
  return {
    ready: missing.length === 0,
    missing,
    agent,
    agentLabel: agent === 'internal' ? 'Internal agent' : prettyAgentName(agent),
    command,
    task,
    taskChars: task.length,
    project,
    attachments,
    executionApplies: stationLaunchExecutionApplies(agent),
    execution: stationLaunchExecution(),
    notice: stationLaunchNoticeText(),
    codex,
    managed: codex ? (stationLaunchDraftValue('managed', newSessionCodexManagedContext || controlCodexConfig.managed_context || 'vanilla')) : '',
    sandbox: codex ? normalizeCodexSandbox(stationLaunchDraftValue('sandbox', newSessionCodexSandbox || controlCodexConfig.sandbox || 'workspace-write')) : '',
    approval: codex ? normalizeCodexApprovalPolicy(stationLaunchDraftValue('approval', newSessionCodexApprovalPolicy || controlCodexConfig.approval_policy || 'on-request')) : '',
    archive: codex ? normalizeContextArchiveMode(stationLaunchDraftValue('archive', newSessionCodexContextArchive || controlCodexConfig.context_archive || 'summary')) : '',
    serviceTier: codex
      ? (stationLaunchDraft.fast ? 'priority' : normalizeCodexServiceTier(newSessionCodexDefaultServiceTier || controlCodexConfig.service_tier || '') || 'default')
      : '',
  };
}

async function stationStartSession() {
  if (!String(stationLaunchDraft.task || '').trim()) {
    const activityDraft = document.getElementById('activity-task-input')?.value || '';
    if (activityDraft.trim()) stationLaunchDraft.task = activityDraft;
  }
  const task = String(stationLaunchDraft.task || '').trim();
  if (!task) {
    showControlToast?.('error', 'Enter a task before starting a Station session');
    stationOpenPanel('system:controls', 'New-session launch needs a task');
    return;
  }
  if (newSessionSpawnPending) {
    showControlToast?.('info', 'A new session is already spawning.');
    return;
  }
  if (!app) {
    failNewSessionSpawnNotice?.('Dashboard is not connected to the server.');
    return;
  }
  const launchAgent = stationEffectiveLaunchAgent();
  if ((launchAgent === 'internal' || !launchAgent) && daemonInternalUnfueled()) {
    showControlToast?.('error', NEW_SESSION_UNFUELED_MESSAGE);
    setNewSessionSpawnNotice('error', NEW_SESSION_UNFUELED_MESSAGE, newSessionAddKeysAction());
    stationOpenPanel?.('system:controls', 'Internal launch needs credentials');
    return;
  }
  const name = String(stationLaunchDraft.name || '').trim();
  const requestedProjectRoot = String(stationLaunchDraft.project || '').trim();
  if (newSessionProjectlessBlocked(requestedProjectRoot)) {
    showControlToast?.('error', NEW_SESSION_NO_PROJECT_MESSAGE);
    stationOpenPanel?.('system:controls', 'Pick a project directory for the session');
    return;
  }
  beginNewSessionSpawnNotice(
    task,
    requestedProjectRoot ? 'Checking project directory...' : 'Spawning new session...',
    name
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

  const msg = { action: 'create_session', task };
  if (name) msg.name = name;
  if (projectRoot) msg.project_root = projectRoot;
  const selectedAgent = normalizeAgentId(stationLaunchDraft.agent);
  if (stationLaunchDraft.agent === 'internal') {
    msg.agent = 'internal';
  } else if (selectedAgent) {
    msg.agent = selectedAgent;
    const command = String(stationLaunchDraft.command || '').trim();
    if (command) msg.agent_command = command;
  }
  if (stationEffectiveLaunchAgent() === 'codex') {
    const hasCodexLaunchConfig = newSessionCodexLaunchDefaultsLoaded ||
      !!stationLaunchDraft.sandbox ||
      !!stationLaunchDraft.approval ||
      !!stationLaunchDraft.managed ||
      !!stationLaunchDraft.archive ||
      !!stationLaunchDraft.fast;
    if (hasCodexLaunchConfig) {
      const sandbox = normalizeCodexSandboxOptional(
        stationLaunchDraft.sandbox ||
          (newSessionCodexLaunchDefaultsLoaded ? (newSessionCodexSandbox || controlCodexConfig.sandbox) : '')
      );
      const approval = normalizeCodexApprovalPolicyOptional(
        stationLaunchDraft.approval ||
          (newSessionCodexLaunchDefaultsLoaded ? (newSessionCodexApprovalPolicy || controlCodexConfig.approval_policy) : '')
      );
      const managed = stationLaunchDraft.managed === 'managed' || stationLaunchDraft.managed === 'vanilla'
        ? stationLaunchDraft.managed
        : (newSessionCodexLaunchDefaultsLoaded
          ? (newSessionCodexManagedContext === 'managed' ? 'managed' : 'vanilla')
          : '');
      const archive = normalizeContextArchiveModeOptional(
        stationLaunchDraft.archive || (newSessionCodexLaunchDefaultsLoaded ? newSessionCodexContextArchive : '')
      );
      if (sandbox) msg.codex_sandbox = sandbox;
      if (approval) msg.codex_approval_policy = approval;
      if (managed) msg.codex_managed_context = managed;
      if (archive) msg.codex_context_archive = archive;
      if (stationLaunchDraft.fast) {
        msg.codex_service_tier = 'priority';
      } else if (codexServiceTierIsFast(newSessionCodexDefaultServiceTier)) {
        msg.codex_service_tier = 'standard';
      }
    }
  }
  // Execution shape: an explicit per-launch choice beats the global Direct
  // toggle; Auto (or an external agent — the pills are hidden and the draft
  // choice inert then) preserves the old behavior of the toggle forcing
  // direct.
  const execution = stationLaunchExecution();
  const globalDirect = document.getElementById('direct-mode-toggle')?.checked || false;
  if (execution === 'orchestrate') {
    msg.orchestrate = true;
  } else if (execution === 'direct' || globalDirect) {
    msg.direct = true;
  }
  if (Array.isArray(pendingAttachments) && pendingAttachments.length > 0) {
    msg.attachments = pendingAttachments.map(a => a.frameId).filter(Boolean);
  }
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
  showControlToast?.('info', 'Spawning new session...');
  if (Array.isArray(pendingAttachments) && pendingAttachments.length > 0) {
    renderAttachmentReceipt?.(task, pendingAttachments.slice(), 'Sent');
    clearPendingAttachments?.({ retainPreviewUrls: true });
  }
  stationScheduleUpdate();
}

function stationRouteLegacySessionDetail(session) {
  stationForceRouteTo('sessions', 'recent', () => openSessionDetail(session));
}

function stationExternalLiveThreadDescriptor(sessionOrId) {
  const session = typeof sessionOrId === 'object' ? sessionOrId : stationFindSessionById(sessionOrId);
  const meta = sessionConfigMetadata(session || sessionOrId);
  const sid = String(meta.session_id || meta.sessionId || stationSessionId(session) || sessionOrId || '').trim();
  const source = sessionConfigSource(meta) || stationSessionSource(session) || '';
  if (!sid || !source || source === 'intendant') return null;
  const backendId = String(meta.backendSessionId || meta.backend_session_id || '').trim();
  const wrapperId = String(meta.intendantSessionId || meta.intendant_session_id || '').trim();
  const ids = Array.from(new Set([
    sid,
    backendId,
    wrapperId,
    ...relatedSessionIdsForSession(sid),
    ...relatedSessionIdsForSession(backendId),
    ...relatedSessionIdsForSession(wrapperId),
  ].map(id => String(id || '').trim()).filter(Boolean)));
  const liveId = ids.find(id => sessionWindows.has(id)) || '';
  const targetId = resolvePromptTargetSessionId();
  const target = !!targetId && ids.includes(targetId);
  const canonicalId = backendId || sid;
  const attachId = ids.find(id => canAttachSessionWindow(id)) || '';
  const stopId = ids.find(id => sessionWindowStopAvailability(id).ok) || '';
  const capabilities = stationSessionCapabilities(canonicalId)
    || stationSessionCapabilities(sid)
    || meta.capabilities
    || {};
  const liveWin = liveId ? sessionWindows.get(liveId) : null;
  const liveMeta = liveId ? (sessionMetadataById.get(liveId) || {}) : {};
  const actionIds = Array.from(new Set([
    liveId,
    backendId,
    sid,
    wrapperId,
    ...ids,
  ].map(id => String(id || '').trim()).filter(Boolean)));
  const actionId = actionIds.find(id => sessionWindowIsCodex(id)) || '';
  return {
    sid,
    source,
    canonicalId,
    backendId,
    wrapperId,
    liveId,
    actionId,
    target,
    attachId,
    stopId,
    detached: ids.some(id => sessionWindowIsDetached(id)),
    phase: normalizeSessionPhase(liveWin?.phase || liveMeta.phase || meta.phase || session?.status || ''),
    steer: capabilities ? capabilities.steer !== false : true,
    interrupt: capabilities ? capabilities.interrupt !== false : true,
    threadActions: Array.isArray(capabilities?.codexThreadActions)
      ? capabilities.codexThreadActions.length
      : null,
  };
}

function stationCodexThreadActionSessionId(sessionOrId, live = null) {
  const session = typeof sessionOrId === 'object' ? sessionOrId : stationFindSessionById(sessionOrId);
  const sid = stationSessionId(session) || String(sessionOrId || '').trim();
  const descriptor = live || stationExternalLiveThreadDescriptor(session || sid);
  const meta = sessionConfigMetadata(session || sid);
  const candidates = Array.from(new Set([
    descriptor?.actionId,
    descriptor?.liveId,
    descriptor?.backendId,
    descriptor?.canonicalId,
    sid,
    descriptor?.wrapperId,
    meta.backendSessionId,
    meta.backend_session_id,
    meta.intendantSessionId,
    meta.intendant_session_id,
    ...relatedSessionIdsForSession(sid),
  ].map(id => String(id || '').trim()).filter(Boolean)));
  return candidates.find(id => stationSessionLooksCodex(id)) || (stationSessionLooksCodex(session || sid) ? sid : '');
}

function stationSessionConfigEditingFor(sessionOrId) {
  const meta = sessionConfigMetadata(sessionOrId);
  const sid = String(meta.session_id || meta.sessionId || sessionOrId || '').trim();
  const source = sessionConfigSource(meta);
  if (!sid || !source || source === 'intendant') return null;
  return {
    sessionId: sid,
    source,
    backendSessionId: String(meta.backendSessionId || meta.backend_session_id || '').trim(),
    intendantSessionId: String(meta.intendantSessionId || meta.intendant_session_id || '').trim(),
    meta,
  };
}

function stationSessionConfigDraftFor(editing) {
  // '' for managed/archive means "inherit the global default" — seeding the
  // draft from the effective mode here would re-pin it on the next dock save.
  if (!editing?.sessionId) return { command: '', managed: '', sandbox: '', approval: '', archive: '' };
  const existing = stationSessionConfigDrafts.get(editing.sessionId);
  if (existing) return existing;
  const draft = {
    command: sessionConfigCommand(editing.meta),
    managed: editing.source === 'codex' ? sessionConfigExplicitManagedMode(editing.meta) : '',
    sandbox: editing.source === 'codex' ? sessionConfigExplicitSandboxMode(editing.meta) : '',
    approval: editing.source === 'codex' ? sessionConfigExplicitApprovalPolicy(editing.meta) : '',
    archive: editing.source === 'codex' ? sessionConfigExplicitArchiveMode(editing.meta) : '',
  };
  stationSessionConfigDrafts.set(editing.sessionId, draft);
  return draft;
}

function stationSetSessionConfigResult(sessionId, text, kind = '') {
  stationSessionConfigResult = {
    sessionId: String(sessionId || '').trim(),
    text: String(text || ''),
    kind: String(kind || ''),
  };
}

function stationSessionConfigPayload(editing, draft) {
  const command = String(draft.command || '').trim();
  // '' = inherit the global default; only explicit 'managed'/'vanilla' pin.
  const modeValue = String(draft.managed || '').trim();
  const mode = modeValue === 'managed' || modeValue === 'vanilla' ? modeValue : '';
  const sandboxMode = normalizeOptionalCodexSandbox(draft.sandbox || '');
  const approvalPolicy = normalizeOptionalCodexApprovalPolicy(draft.approval || '');
  const archiveMode = normalizeContextArchiveModeOptional(draft.archive || '');
  return {
    payload: {
      action: 'configure_session_agent',
      session_id: editing.sessionId,
      source: editing.source,
      ...(editing.backendSessionId ? { backend_session_id: editing.backendSessionId } : {}),
      ...(editing.intendantSessionId ? { intendant_session_id: editing.intendantSessionId } : {}),
      agent_command: command,
      ...(editing.source === 'codex' ? {
        codex_sandbox: sandboxMode || 'inherit',
        codex_approval_policy: approvalPolicy || 'inherit',
        codex_managed_context: mode || 'inherit',
        codex_context_archive: archiveMode || 'inherit',
      } : {}),
    },
    command,
    sandboxMode,
    approvalPolicy,
    mode,
    archiveMode,
  };
}

function stationSaveSessionConfig(sessionOrId, options = {}) {
  const editing = stationSessionConfigEditingFor(sessionOrId);
  if (!editing) {
    showControlToast?.('error', 'Station launch config is only available for external-agent sessions');
    return;
  }
  if (!app) {
    stationSetSessionConfigResult(editing.sessionId, 'Dashboard is not connected to the server.', 'error');
    stationScheduleUpdate();
    return;
  }
  const draft = stationSessionConfigDraftFor(editing);
  const { payload, command, sandboxMode, approvalPolicy, mode, archiveMode } = stationSessionConfigPayload(editing, draft);
  try {
    if (sessionConfigSavePending?.timeoutHandle) {
      clearTimeout(sessionConfigSavePending.timeoutHandle);
    }
    const ids = Array.from(new Set([
      editing.sessionId,
      editing.backendSessionId,
      editing.intendantSessionId,
    ].map(id => String(id || '').trim()).filter(Boolean)));
    sessionConfigSavePending = {
      ids,
      sessionId: editing.sessionId,
      backendSessionId: editing.backendSessionId,
      intendantSessionId: editing.intendantSessionId,
      meta: {
        sessionId: editing.sessionId,
        source: editing.source,
        backendSessionId: editing.backendSessionId,
        intendantSessionId: editing.intendantSessionId,
      },
      command,
      sandboxMode,
      approvalPolicy,
      mode,
      archiveMode,
      restart: options.restart === true,
      station: true,
      timeoutHandle: setTimeout(() => {
        const pending = sessionConfigSavePending;
        if (!pending || pending.sessionId !== editing.sessionId) return;
        sessionConfigSavePending = null;
        setSessionConfigSaving(false);
        stationSetSessionConfigResult(editing.sessionId, 'Save timed out before the daemon confirmed the launch config.', 'error');
        stationScheduleUpdate();
        showControlToast?.('error', 'Launch config save timed out');
      }, 15000),
    };
    setSessionConfigSaving(true);
    stationSetSessionConfigResult(editing.sessionId, options.restart === true ? 'Saving launch config before restart...' : 'Saving launch config...', '');
    dispatchControlMsg(payload);
    stationScheduleUpdate();
  } catch (e) {
    if (sessionConfigSavePending?.timeoutHandle) {
      clearTimeout(sessionConfigSavePending.timeoutHandle);
    }
    sessionConfigSavePending = null;
    setSessionConfigSaving(false);
    stationSetSessionConfigResult(editing.sessionId, e?.message || 'Save failed', 'error');
    stationScheduleUpdate();
  }
}

async function stationLoadWorktrees(forceScan = false) {
  await loadWorktrees({ forceScan });
  stationScheduleUpdate();
}

function stationPendingApprovalRows() {
  const rows = [];
  if (stationCurrentApproval?.id) {
    rows.push({
      hostId: selfPeerId || 'local',
      approvalId: String(stationCurrentApproval.id),
      command: stationCurrentApproval.command || 'approval required',
      category: stationCurrentApproval.category || '',
      local: true,
    });
  }
  for (const [hostId, pending] of peerPendingApprovals.entries()) {
    for (const [approvalId, approval] of pending.entries()) {
      rows.push({
        hostId,
        approvalId: String(approvalId),
        command: approval.command || 'approval required',
        category: approval.category || '',
        local: false,
      });
    }
  }
  return rows;
}

function stationHumanQuestionText() {
  const panel = document.getElementById('human-panel');
  const visible = panel?.classList.contains('visible');
  return stationCurrentHumanQuestion || (visible ? (document.getElementById('human-question')?.textContent || '') : '');
}

function stationComputerUseValue(id, fallback = '') {
  return document.getElementById(id)?.value || fallback;
}

function stationExternalTargetRows(limit = 6) {
  const targetId = String(resolvePromptTargetSessionId() || '').trim();
  const rows = new Map();
  const addTarget = (sessionOrId) => {
    const session = typeof sessionOrId === 'object'
      ? sessionOrId
      : (stationFindSessionById(sessionOrId) || { session_id: sessionOrId });
    const meta = sessionConfigMetadata(session || sessionOrId);
    const sid = stationSessionId(session) || String(meta.session_id || meta.sessionId || sessionOrId || '').trim();
    const backendId = String(meta.backendSessionId || meta.backend_session_id || '').trim();
    const canonicalId = backendId || sid;
    if (!canonicalId || canonicalId === daemonSessionFullId || rows.has(canonicalId)) return;
    const source = sessionConfigSource(meta) || normalizeAgentId(stationSessionSource(session));
    if (!source || source === 'intendant') return;
    const live = stationExternalLiveThreadDescriptor(session || sid) || stationExternalLiveThreadDescriptor(canonicalId);
    const relatedIds = new Set([
      canonicalId,
      sid,
      live?.liveId,
      live?.backendId,
      live?.wrapperId,
      live?.actionId,
      ...relatedSessionIdsForSession(canonicalId),
      ...relatedSessionIdsForSession(sid),
    ].map(id => String(id || '').trim()).filter(Boolean));
    const liveId = live?.liveId || [...relatedIds].find(id => sessionWindows.has(id)) || '';
    const actionId = live?.actionId || '';
    const attachId = live?.attachId || [...relatedIds].find(id => canAttachSessionWindow(id)) || '';
    const stopId = live?.stopId || [...relatedIds].find(id => sessionWindowStopAvailability(id).ok) || '';
    const updated = Math.max(
      stationSessionUpdatedMs(session),
      stationTimestampMs(meta.updatedAt || meta.updated_at),
    );
    const active = liveId ? isSessionWindowEffectivelyActive(liveId) : false;
    const detached = live?.detached || [...relatedIds].some(id => sessionWindowIsDetached(id));
    const target = !!targetId && relatedIds.has(targetId);
    rows.set(canonicalId, {
      id: canonicalId,
      sid,
      source,
      label: stationSessionTask(session) || meta.name || stationSessionShortLabel(canonicalId) || canonicalId,
      detail: [
        shortSessionId(canonicalId),
        sessionProjectDirectory(session) ? compactPathLabel(sessionProjectDirectory(session), true) : '',
        updated ? formatContextTimestamp(meta.updatedAt || meta.updated_at || session?.updated_at || session?.updatedAt || session?.created_at || session?.createdAt) : '',
      ].filter(Boolean).join(' / '),
      status: normalizeSessionPhase(live?.phase || meta.phase || session?.status || '') || (active ? 'active' : (detached ? 'detached' : 'idle')),
      liveId,
      actionId,
      attachId,
      stopId,
      active,
      detached,
      target,
      updated,
      canResume: !session || session.can_resume !== false,
      canConfig: true,
      isCodex: source === 'codex' || stationSessionLooksCodex(actionId || canonicalId),
    });
  };
  const { sessions } = stationCollectSessionSet();
  for (const session of sessions) addTarget(session);
  for (const [id, meta] of sessionMetadataById) {
    addTarget({ session_id: id, ...meta });
  }
  return [...rows.values()]
    .sort((a, b) =>
      Number(b.target) - Number(a.target)
      || Number(b.active) - Number(a.active)
      || Number(!!b.liveId) - Number(!!a.liveId)
      || b.updated - a.updated
      || a.label.localeCompare(b.label)
    )
    .slice(0, limit);
}

function stationOperationsManagedReadiness(managed) {
  if (managed && !managed.sessionId && managed.session_id) {
    return managed.readiness || managed.status || 'known';
  }
  const ready = stationManagedReadiness(managed);
  if (ready.base) return ready.base;
  if (managed?.rewindOnly) return 'rewind-only pressure';
  if (ready.canRewind) return 'rewind draft ready';
  if (ready.canBackout) return 'backout record ready';
  if (ready.canInspect) return 'anchor selected';
  return 'needs managed draft';
}

function stationAttentionSteerRows() {
  const rows = [];
  for (const [id, entry] of steerRows.entries()) {
    const el = entry?.el;
    const status = [...(el?.classList || [])].find(cls => cls !== 'steer-row' && cls !== 'fading') || 'pending';
    rows.push({
      id,
      status,
      text: entry?.text || el?.querySelector?.('.steer-text')?.textContent || '',
      reason: el?.querySelector?.('.steer-reason')?.textContent?.replace(/^—\s*/, '') || '',
      sessionId: entry?.sessionId || '',
    });
  }
  return rows;
}

function stationAttentionAttachmentNames(limit = 4) {
  if (!Array.isArray(pendingAttachments)) return [];
  return pendingAttachments.slice(0, limit).map(att =>
    compactSessionText(pendingAttachmentDisplayName?.(att) || att?.name || att?.filename || att?.frameId || 'attachment')
  );
}

// Feeds snapshot.attentionQueue. The rendered Station consumes only the
// serializable fields (id/kind/level/title/meta/detail/sessionId/canCancel)
// and dispatches its own controls/approval ops for each item kind.
function stationBuildAttentionQueue(controls) {
  const items = [];
  for (const row of stationPendingApprovalRows()) {
    items.push({
      id: `approval:${row.hostId}:${row.approvalId}`,
      kind: 'approval',
      level: 'blocked',
      title: row.command || 'Approval required',
      meta: ['approval', row.local ? 'local' : stationHostLabel(row.hostId), row.category].filter(Boolean).join(' / '),
      detail: row.approvalId,
      // Clicking the item in the rendered controls panel selects the
      // agent node whose focus panel carries the approve/deny pills.
      target: row.local
        ? 'primary-agent'
        : `approval-${sanitizeStationId(row.hostId)}-${sanitizeStationId(row.approvalId)}`,
    });
  }

  const question = stationHumanQuestionText();
  if (question) {
    items.push({
      id: 'human-input',
      kind: 'human_input',
      level: 'blocked',
      title: 'Human input requested',
      meta: 'agent is waiting',
      detail: compactSessionText(question, 180),
      target: 'primary-agent',
    });
  }

  if (controls.sharedViewCanTakeInput) {
    items.push({
      id: 'shared-view-input',
      kind: 'shared_view_input',
      level: 'blocked',
      title: 'Shared view input request',
      meta: controls.sharedViewTarget || 'shared display',
      detail: controls.sharedViewNote || controls.sharedViewAction || 'Input authority is available.',
      target: 'system:peers',
    });
  }

  if (newSessionSpawnPending) {
    items.push({
      id: 'session-spawn',
      kind: 'session_spawn',
      level: 'warn',
      title: 'New session spawning',
      meta: newSessionSpawnName || 'pending start confirmation',
      detail: stationLaunchNoticeText() || compactSessionText(newSessionSpawnTask, 180) || 'Waiting for session start confirmation.',
      target: 'system:sessions',
    });
  }

  if (controls.pendingAttachments > 0) {
    const names = stationAttentionAttachmentNames();
    items.push({
      id: 'attachments',
      kind: 'attachments',
      level: 'warn',
      title: `${controls.pendingAttachments} pending attachment${controls.pendingAttachments === 1 ? '' : 's'}`,
      meta: names.join(' / ') || 'staged for next send',
      detail: controls.pendingAttachments > names.length ? `Showing ${names.length} of ${controls.pendingAttachments}` : 'Ready for next task or follow-up.',
      target: '',
    });
  }

  for (const row of stationAttentionSteerRows().slice(0, 6)) {
    const cancellable = row.id.startsWith('steer-') && !['delivered', 'cancelled', 'failed'].includes(row.status);
    items.push({
      id: row.id,
      kind: row.id.startsWith('follow-') ? 'follow_up' : 'steer',
      level: row.status === 'failed' ? 'blocked' : 'warn',
      title: `Steer ${row.status}`,
      meta: row.sessionId ? stationSessionShortLabel(row.sessionId) : row.id,
      detail: [compactSessionText(row.text, 160), row.reason].filter(Boolean).join(' / '),
      sessionId: row.sessionId || '',
      canCancel: cancellable,
      target: row.sessionId ? `log:${row.sessionId}` : 'system:sessions',
    });
  }

  if (!controls.activeBrowser) {
    items.push({
      id: 'passive-browser',
      kind: 'passive_browser',
      level: 'warn',
      title: 'Browser is passive',
      meta: 'voice/video control is elsewhere',
      detail: 'Make this browser active before using live voice or video controls from Station.',
      target: '',
    });
  }

  if (controls.sessionCanInterrupt) {
    items.push({
      id: 'active-turn',
      kind: 'active_turn',
      level: 'ready',
      title: 'Agent turn active',
      meta: controls.sessionLabel || controls.sessionSelection || 'current target',
      detail: controls.sessionCanSteer ? 'Steer is available from the execution composer.' : 'Follow-up may be queued until the turn ends.',
      sessionId: controls.sessionLiveId || controls.sessionId || '',
      canCancel: false,
      target: (controls.sessionLiveId || controls.sessionId) ? `log:${controls.sessionLiveId || controls.sessionId}` : '',
    });
  }

  return {
    // No wall-clock fields: embedded in the snapshot, would defeat the
    // JSON dedupe gate (see stationDisplayRunwayPayload).
    count: items.length,
    blocked: items.filter(item => item.level === 'blocked').length,
    warn: items.filter(item => item.level === 'warn').length,
    ready: items.filter(item => item.level === 'ready').length,
    // Slim wire rows: the header alert strip and the controls focus panel
    // render level/title/meta/detail for the top items; the counts above
    // cover the rest.
    items: items.slice(0, 10).map(item => ({
      level: String(item.level || ''),
      title: String(item.title || ''),
      meta: String(item.meta || ''),
      detail: String(item.detail || ''),
      target: String(item.target || ''),
    })),
  };
}

function stationFindSessionById(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  if (typeof findCachedSessionByAnyId === 'function') {
    const cached = findCachedSessionByAnyId(sid);
    if (cached) return cached;
  }
  const matches = session => {
    if (!session) return false;
    return [
      session.session_id,
      session.sessionId,
      session.resume_id,
      session.resumeId,
      session.backend_session_id,
      session.backendSessionId,
      session.intendant_session_id,
      session.intendantSessionId,
    ].some(id => String(id || '').trim() === sid);
  };
  for (const pool of [
    _cachedSessions || [],
    sessionsListCache.get(sessionListCacheKey(selfPeerId)) || [],
    stationSessionWindowSummaries(),
    [stationCurrentDaemonSessionSummary()].filter(Boolean),
  ]) {
    const found = Array.isArray(pool) ? pool.find(matches) : null;
    if (found) return found;
  }
  return null;
}

function stationHandleSessionAction(action) {
  const sessionId = String(action.session_id || '').trim();
  const op = String(action.action || 'detail').trim();
  if (op === 'refresh') {
    loadSessions({ force: true }).finally(() => stationScheduleUpdate());
    return;
  }
  if (op === 'search') {
    if (stationRenderedSelectPanel('system:sessions', 'Session search stays in Station')) return;
    stationForceRouteTo('sessions', 'recent', () => document.getElementById('sessions-search')?.focus());
    return;
  }
  if (op === 'deep-search') {
    if (stationRenderedSelectPanel('system:sessions', 'Deep search selected')) return;
    stationForceRouteTo('sessions', 'deep', () => document.getElementById('sessions-deep-search-query')?.focus());
    return;
  }
  if (op === 'worktrees') {
    stationLoadWorktrees(false);
    return;
  }
  if (op === 'worktrees-scan') {
    stationLoadWorktrees(true);
    return;
  }
  if (op === 'worktrees-cache') {
    stationLoadWorktrees(false);
    return;
  }
  if (op === 'worktree-search') {
    if (stationRenderedSelectPanel('system:worktrees', 'Worktree search stays in Station')) return;
    stationForceRouteTo('sessions', 'worktrees', () => document.getElementById('worktrees-search')?.focus());
    return;
  }
  if (op === 'new-session') {
    if (stationRenderedPrimaryActive()) {
      station?.set_composer?.(true, 'launch');
      stationHandleComposerEvent({ op: 'focus' });
      return;
    }
    stationForceRouteTo('sessions', 'new', () => document.getElementById('new-session-input')?.focus());
    return;
  }
  if (op === 'worktree') {
    stationFocusWorktree(sessionId);
    return;
  }
  if (op === 'worktree-copy') {
    if (sessionId) {
      copyTextToClipboard(sessionId)
        .then(() => showControlToast?.('success', `Copied worktree path ${sessionId}`))
        .catch(err => showControlToast?.('error', `Copy failed: ${err?.message || err}`));
    }
    return;
  }
  if (op === 'station-log') {
    stationOpenTranscript(sessionId);
    return;
  }
  if (!sessionId) return;
  const session = stationFindSessionById(sessionId) || { session_id: sessionId };
  const live = stationExternalLiveThreadDescriptor(session) || stationExternalLiveThreadDescriptor(sessionId);
  if (op === 'focus' || op === 'target') {
    focusSessionWindow(live?.liveId || sessionId);
    return;
  }
  if (op === 'interrupt') {
    // Target the session's own turn; the daemon supervisor routes by id
    // (a bare interrupt would broadcast to the foreground turn instead).
    // The stop also disarms any pending auto-attach escalation for the
    // session (see haltPendingFollowUpEscalations) — under both the window
    // id and the live action id frontends may have queued against.
    const actionId = live?.actionId || sessionId;
    haltPendingFollowUpEscalations(sessionId);
    if (actionId !== sessionId) haltPendingFollowUpEscalations(actionId);
    const msg = { action: 'interrupt' };
    if (sessionId) msg.session_id = actionId;
    dispatchSessionControlMsg(msg);
    showControlToast?.('info', `Interrupt sent to ${shortSessionId(sessionId)}`);
    return;
  }
  if (op === 'steer') {
    // Make this session the prompt target, then open the composer on it.
    focusSessionWindow(live?.liveId || sessionId);
    if (stationRenderedPrimaryActive()) {
      station?.set_composer?.(true, 'send');
      stationHandleComposerEvent({ op: 'focus' });
    }
    return;
  }
  if (op === 'thread-compact') {
    dispatchCodexThreadAction('compact', {}, live?.actionId || sessionId);
    showControlToast?.('info', `Compaction requested for ${shortSessionId(sessionId)}`);
    return;
  }
  if (op === 'thread-fork') {
    dispatchCodexThreadAction('fork', {}, live?.actionId || sessionId);
    showControlToast?.('info', `Fork requested for ${shortSessionId(sessionId)}`);
    return;
  }
  if (op === 'copy') {
    copyTextToClipboard(sessionId)
      .then(() => showControlToast('success', `Copied session ID ${shortSessionId(sessionId)}`))
      .catch(err => showControlToast('error', `Copy session ID failed: ${err?.message || err}`));
    return;
  }
  if (op === 'copy-backend') {
    const meta = sessionConfigMetadata(session || sessionId);
    const backendId = String(
      meta.backendSessionId ||
      meta.backend_session_id ||
      session?.backend_session_id ||
      session?.backendSessionId ||
      live?.actionId ||
      ''
    ).trim();
    if (backendId) {
      copyTextToClipboard(backendId)
        .then(() => showControlToast('success', `Copied backend ID ${shortSessionId(backendId)}`))
        .catch(err => showControlToast('error', `Copy backend ID failed: ${err?.message || err}`));
    } else {
      showControlToast('error', 'No backend ID is available for this session');
    }
    return;
  }
  if (op === 'copy-intendant') {
    const meta = sessionConfigMetadata(session || sessionId);
    const intendantId = String(
      meta.intendantSessionId ||
      meta.intendant_session_id ||
      session?.intendant_session_id ||
      session?.intendantSessionId ||
      ''
    ).trim() || sessionId;
    copyTextToClipboard(intendantId)
      .then(() => showControlToast('success', `Copied wrapper ID ${shortSessionId(intendantId)}`))
      .catch(err => showControlToast('error', `Copy wrapper ID failed: ${err?.message || err}`));
    return;
  }
  if (op === 'context-jump') {
    stationHandleContextAction({ action: 'live' });
    stationRenderedSelectPanel('system:context', 'Session context selected');
    return;
  }
  if (op === 'managed-jump') {
    stationSetManagedSession(sessionId);
    stationRenderedSelectPanel('system:managed', 'Session managed context selected');
    return;
  }
  if (op === 'resume') {
    resumeSession(session);
    return;
  }
  if (op === 'continue') {
    const msg = detachedSessionResumeMessage(sessionId, '', true, []);
    if (msg) {
      dispatchControlMsg(msg);
      markSessionWindowPendingActive(sessionId);
      showControlToast('info', `Continuing session ${shortSessionId(sessionId)}`);
    } else {
      focusSessionWindow(live?.liveId || sessionId);
      showControlToast('info', `Focused session ${shortSessionId(sessionId)}`);
    }
    return;
  }
  if (op === 'open-log') {
    stationRouteLegacySessionDetail(session);
    return;
  }
  if (op === 'fork') {
    const threadId = stationCodexThreadActionSessionId(session, live) || sessionId;
    if (threadId) stationHandleThreadAction({ op: 'fork', session_id: threadId });
    return;
  }
  if (op === 'attach') {
    attachSessionWindowAction(live?.attachId || sessionId);
    return;
  }
  if (op === 'config') {
    openSessionConfigModal(stationSessionId(session) || sessionId);
    return;
  }
  if (op === 'config-save') {
    stationSaveSessionConfig(stationSessionId(session) || sessionId);
    return;
  }
  if (op === 'config-save-restart') {
    stationSaveSessionConfig(stationSessionId(session) || sessionId, { restart: true });
    return;
  }
  if (op === 'restart') {
    restartSessionWindowAction(sessionId);
    return;
  }
  if (op === 'stop') {
    stopSessionWindowAction(live?.stopId || sessionId);
    return;
  }
  if (op === 'rename') {
    requestSessionRename(session);
    return;
  }
  if (op === 'hide') {
    hideSessionWindowAction(sessionId);
    return;
  }
  if (op === 'close-side') {
    closeSideSessionWindowAction(sessionId);
    return;
  }
  if (stationRenderedSelectPanel('system:sessions', 'Session selected')) return;
  stationRouteLegacySessionDetail(session);
}

function stationFocusWorktree(path) {
  const full = String(path || '').trim();
  if (stationRenderedSelectPanel('system:worktrees', full || 'Worktree selected')) {
    if (full) showControlToast?.('info', full);
    return;
  }
  stationForceRouteTo('sessions', 'worktrees', () => document.getElementById('worktrees-search')?.focus());
}

async function stationHandleThreadAction(action) {
  const op = String(action.op || action.action || '').trim();
  if (!op) return;
  const sessionId = String(action.session_id || action.sessionId || '').trim();
  const spec = codexThreadActionSpec(op);
  if (!spec) {
    showControlToast?.('error', `Unknown Codex thread action /${op}`);
    return;
  }
  await runCodexThreadActionFromUi(op, spec, sessionId);
}

function stationHandleManagedAction(action) {
  const op = String(action.action || '').trim();
  const id = String(action.id || '').trim();
  const sessionId = String(action.session_id || '').trim();
  const idOptionalOps = new Set(['rewind', 'backout', 'refresh', 'use-target', 'copy-status', 'seed-context', 'clear-activity-signal']);
  if (!id && !idOptionalOps.has(op)) return;
  if (sessionId) stationSetManagedSession(sessionId);
  if (op === 'clear-activity-signal') {
    stationManagedActivitySignal = null;
    stationRenderedSelectPanel('system:managed', 'Managed activity signal cleared');
    return;
  }
  if (op === 'seed-context') {
    stationSeedManagedDraftFromContext();
    stationOpenPanel('system:managed', 'Managed rewind draft seeded');
    return;
  }
  if (op === 'anchor-inspect') {
    stationApplyManagedAction('anchor', id, sessionId)
      .then(() => stationInspectManagedAnchor())
      .finally(() => {
        stationRenderedSelectPanel('system:managed', 'Managed anchor inspected');
      });
    return;
  }
  if (op === 'anchor-copy' || op === 'record-copy' || op === 'branch-copy') {
    copyTextToClipboard(id)
      .then(() => showControlToast?.('success', 'Copied managed context ID'));
    return;
  }
  if (op === 'record-inspect' || op === 'record-fork' || op === 'record-restore' || op === 'record-backout') {
    const mode = op === 'record-fork'
      ? 'fork'
      : op === 'record-restore'
        ? 'restore'
        : op === 'record-backout'
          ? 'backout'
          : 'inspect';
    stationApplyManagedAction('record', id, sessionId)
      .then(() => {
        stationRememberManagedDraftValue('backoutMode', mode);
        return stationSubmitManagedBackout(mode);
      })
      .finally(() => {
        stationRenderedSelectPanel('system:managed', `Managed record ${mode} requested`);
      });
    return;
  }
  if (op === 'dispatch-rewind') {
    Promise.resolve(id ? stationApplyManagedAction('anchor', id, sessionId) : undefined)
      .then(() => stationSubmitManagedRewind())
      .finally(() => {
        stationRenderedSelectPanel('system:managed', 'Managed rewind dispatched from rendered Station');
      });
    return;
  }
  if (op === 'run-backout') {
    Promise.resolve(id ? stationApplyManagedAction('record', id, sessionId) : undefined)
      .then(() => stationSubmitManagedBackout())
      .finally(() => {
        stationRenderedSelectPanel('system:managed', 'Managed backout requested from rendered Station');
      });
    return;
  }
  if (op === 'use-target') {
    managedContextEl?.('managed-context-use-target')?.click();
    stationOpenPanel('system:managed', 'Managed context target updated');
    return;
  }
  if (op === 'copy-status') {
    const context = stationBuildContextSummary();
    stationCopyManagedStatus(stationBuildManagedSummary(context), context)
      .catch(err => showControlToast?.('error', `Copy managed status failed: ${err?.message || err}`));
    return;
  }
  if (op === 'rewind' || op === 'backout') {
    stationOpenPanel('system:managed', `${op} controls remain in the managed panel`);
    return;
  }
  stationApplyManagedAction(op, id, sessionId)
    .finally(() => {
      stationOpenPanel('system:managed', `Managed action ${op || 'updated'}`);
    });
}

function stationHandleContextAction(action) {
  const op = String(action.action || '').trim();
  const id = String(action.id || '').trim();
  const actionButtonByOp = {
    live: 'context-live-btn',
    replay: 'context-replay-btn',
    reset: 'context-reset-view-btn',
    focus: 'context-open-focus-btn',
    raw: 'context-raw-toggle-btn',
  };
  if (op === 'part' && !id) return;
  if (op === 'copy-snapshot') {
    stationCopyContextSnapshot();
    return;
  }
  if (op === 'copy-continuity') {
    stationCopyContextContinuityPayload(stationBuildContextSummary())
      .catch(err => showControlToast?.('error', `Copy context continuity failed: ${err?.message || err}`));
    return;
  }
  if (op === 'seed-managed') {
    stationSeedManagedDraftFromContext();
    stationRenderedSelectPanel('system:managed', 'Managed rewind draft seeded from rendered context');
    return;
  }
  if (op === 'copy-part') {
    const part = stationSelectedContextPartDetail(id || stationSelectedContextPart);
    if (part) stationCopyContextPart(part);
    else showControlToast?.('error', 'No context item selected');
    return;
  }
  if (op === 'copy-lane') {
    if (id) stationCopyContextLane(id);
    else showControlToast?.('error', 'No context lane selected');
    return;
  }
  if (op === 'copy-id') {
    const partId = id || stationSelectedContextPart;
    if (partId) copyTextToClipboard(partId);
    return;
  }
  if (op === 'replay-prev' || op === 'replay-next' || op === 'replay-latest') {
    const { index, max, timeline } = stationContextTimelineState();
    if (!timeline.length) return;
    const next = op === 'replay-prev'
      ? index - 1
      : op === 'replay-next'
        ? index + 1
        : max;
    stationSetContextReplayIndex(next);
    stationRenderedSelectPanel('system:context', `Context ${op.replace('replay-', '')}`);
    return;
  }
  if (op === 'load-exact') {
    stationLoadExactContextSnapshot(id || stationSelectedContextPart)
      .catch(err => showControlToast?.('error', `Load exact context failed: ${err?.message || err}`));
    return;
  }
  if (op !== 'part' && !actionButtonByOp[op]) return;
  stationApplyContextAction(op, id);
  stationOpenPanel('system:context', `Context ${op || 'updated'}`);
}

function stationCssEscape(value) {
  const raw = String(value || '');
  if (window.CSS && typeof CSS.escape === 'function') return CSS.escape(raw);
  return raw.replace(/["\\]/g, '\\$&');
}

function stationHandleActivityAction(action) {
  const op = String(action.action || '').trim();
  const id = String(action.id || '').trim();
  const eventForId = () => stationActivityEvents().find(row => stationActivityEventKey(row) === id);
  if (op.startsWith('verbosity:')) {
    stationSetActivityVerbosity(op.slice('verbosity:'.length));
    stationScheduleUpdate();
    return;
  }
  if (op.startsWith('level:')) {
    stationActivityDockLevel = op.slice('level:'.length).trim();
    stationScheduleUpdate();
    if (typeof showControlToast === 'function') {
      showControlToast('success', stationActivityDockLevel ? `Activity level: ${stationActivityDockLevel}` : 'Activity level filter cleared');
    }
    return;
  }
  if (op.startsWith('source:')) {
    stationActivityDockSource = op.slice('source:'.length).trim();
    stationScheduleUpdate();
    if (typeof showControlToast === 'function') {
      showControlToast('success', stationActivityDockSource ? `Activity source: ${stationActivityDockSource}` : 'Activity source filter cleared');
    }
    return;
  }
  if (op === 'send') {
    // Rendered Station: the in-canvas composer takes the text. (This used
    // to focus the hidden Activity-tab input — invisible while Station
    // was the active tab.)
    if (stationRenderedPrimaryActive()) {
      station?.set_composer?.(true, 'send');
      stationHandleComposerEvent({ op: 'focus' });
      return;
    }
    const input = document.getElementById('activity-task-input');
    if (input && input.value.trim()) {
      window.submitActivityOrSteer?.();
    } else {
      input?.focus();
    }
    return;
  }
  if (op === 'stop') {
    window.sendInterrupt?.();
    return;
  }
  if (op === 'target') {
    window.focusForegroundSessionWindow?.();
    return;
  }
  if (op === 'new-session') {
    if (stationRenderedPrimaryActive()) {
      station?.set_composer?.(true, 'launch');
      stationHandleComposerEvent({ op: 'focus' });
      return;
    }
    stationForceRouteTo('sessions', 'new', () => document.getElementById('new-session-input')?.focus());
    return;
  }
  if (op === 'host:all') {
    stationClearActivityHostFilter();
    stationScheduleUpdate();
    return;
  }
  if (op === 'bottom') {
    if (stationRenderedSelectPanel('system:activity', 'Activity latest selected')) return;
    stationForceRouteTo('activity', 'log', () => document.getElementById('scroll-bottom')?.click());
    return;
  }
  if (op === 'copy-visible') {
    stationCopyActivityEvents(stationActivityFilteredEvents().events);
    return;
  }
  if (op === 'clear-triage') {
    stationClearActivityTriage();
    return;
  }
  if (op === 'clear-log') {
    stationClearActivityLog();
    return;
  }
  if (id && ['show-log', 'copy-id', 'copy-event', 'copy-event-json', 'activity-session', 'focus-session', 'activity-context', 'activity-managed', 'edit', 'branch'].includes(op)) {
    const ev = eventForId();
    if (!ev) return;
    if (op === 'show-log') {
      focusActivityLogEvent(id);
      return;
    }
    if (op === 'copy-id') {
      copyTextToClipboard(id)
        .then(() => showControlToast?.('success', 'Copied Station activity event ID'));
      return;
    }
    if (op === 'copy-event') {
      copyTextToClipboard(stationActivityEventCopyText(ev))
        .then(() => showControlToast?.('success', 'Copied Station activity event'));
      return;
    }
    if (op === 'copy-event-json') {
      stationCopyActivityEventJson(ev);
      return;
    }
    if (op === 'activity-session') {
      const sessionId = ev.sessionId || ev.session_id || '';
      if (!sessionId) return;
      stationOpenPanel('system:sessions', 'Activity session selected');
      return;
    }
    if (op === 'focus-session') {
      const sessionId = ev.sessionId || ev.session_id || '';
      if (!sessionId) return;
      focusSessionWindow(sessionId);
      return;
    }
    if (op === 'activity-context') {
      const sessionId = ev.sessionId || ev.session_id || '';
      if (sessionId) focusSessionWindow(sessionId);
      stationOpenPanel('system:context', 'Activity context selected');
      return;
    }
    if (op === 'activity-managed') {
      stationOpenManagedFromActivity(ev);
      return;
    }
    stationEditActivityEvent(id);
    return;
  }
  if (op !== 'log' || !id) return;
  if (stationRenderedSelectPanel('system:activity', 'Activity event selected')) return;
  focusActivityLogEvent(id);
}

function stationHandleControlsAction(action) {
  const op = String(action.action || '').trim();
  if (!op) return;
  if (op.startsWith('autonomy:')) {
    const level = op.slice('autonomy:'.length).trim().toLowerCase();
    if (!['low', 'medium', 'high', 'full'].includes(level)) return;
    updateStatusBar({ autonomy: level.charAt(0).toUpperCase() + level.slice(1) });
    dispatchControlMsg({ action: 'set_autonomy', level });
    showControlToast?.('success', `Autonomy: ${level}`);
    return;
  }
  if (op.startsWith('backend:')) {
    const agent = op.slice('backend:'.length).trim();
    const value = agent === 'internal' ? null : agent;
    if (value && !['codex', 'claude-code'].includes(value)) return;
    dispatchControlMsg({ action: 'set_external_agent', agent: value });
    showControlToast?.('success', `Backend: ${value || 'intendant'}`);
    return;
  }
  if (op.startsWith('codex-approval:')) {
    const policy = op.slice('codex-approval:'.length).trim();
    if (!['untrusted', 'on-request', 'never'].includes(policy)) return;
    dispatchControlMsg({ action: 'set_codex_approval_policy', policy });
    showControlToast?.('success', `Codex approval policy: ${policy}`);
    return;
  }
  if (op.startsWith('codex-managed:')) {
    const mode = op.slice('codex-managed:'.length).trim();
    if (!['vanilla', 'managed'].includes(mode)) return;
    dispatchControlMsg({ action: 'set_codex_managed_context', mode });
    showControlToast?.('success', `Codex managed context: ${mode}`);
    return;
  }
  if (op.startsWith('claude-model:')) {
    const alias = op.slice('claude-model:'.length).trim();
    if (!['default', 'fable', 'opus', 'sonnet', 'haiku'].includes(alias)) return;
    const model = alias === 'default' ? null : alias;
    dispatchControlMsg({ action: 'set_claude_model', model });
    showControlToast?.('success', `Claude model: ${alias}`);
    return;
  }
  if (op.startsWith('claude-permission:')) {
    const mode = op.slice('claude-permission:'.length).trim();
    if (!['default', 'acceptEdits', 'plan', 'auto', 'dontAsk', 'bypassPermissions'].includes(mode)) return;
    dispatchControlMsg({ action: 'set_claude_permission_mode', mode });
    showControlToast?.('success', `Claude permissions: ${mode}`);
    return;
  }
  if (op.startsWith('launch-agent:')) {
    stationLaunchDraft.agent = op.slice('launch-agent:'.length).trim();
    stationScheduleUpdate({ immediate: true });
    return;
  }
  if (op.startsWith('launch-execution:')) {
    const mode = op.slice('launch-execution:'.length).trim();
    if (!['auto', 'orchestrate', 'direct'].includes(mode)) return;
    stationLaunchDraft.execution = mode === 'auto' ? '' : mode;
    stationScheduleUpdate({ immediate: true });
    return;
  }
  if (op.startsWith('recording-open:')) {
    const name = op.slice('recording-open:'.length).trim();
    stationForceRouteTo('displays', undefined, () => {
      const select = document.getElementById('recording-stream-select');
      if (select && name) {
        select.value = name;
        select.dispatchEvent(new Event('change'));
      }
    });
    return;
  }
  if (op === 'display-toggle') {
    window.toggleUserDisplay?.();
    return;
  }
  if (op === 'display-list') {
    stationLoadLocalDisplays(true);
    return;
  }
  if (op === 'peer-status-copy') {
    stationCopyPeerDisplayStatus();
    return;
  }
  if (op === 'peer-refresh') {
    refreshPeersFromApi()
      .then(() => stationSetPeerStatus('Peer registry refreshed', 'ok'))
      .catch(err => stationSetPeerStatus(`Peer refresh failed: ${err?.message || err}`, 'error'))
      .finally(() => stationScheduleUpdate());
    return;
  }
  if (op === 'peer-open-selected') {
    const peer = stationSelectedPeer();
    if (!peer || !peerCanShareDisplay(peer)) {
      stationSetPeerStatus('Selected peer does not advertise display capability.', 'error');
      stationScheduleUpdate();
      return;
    }
    stationOpenDisplay(peer.host_id, stationPeerDisplayIdForHost(peer.host_id));
    stationScheduleUpdate();
    return;
  }
  if (op === 'display-target-open') {
    stationOperateSelectedDisplayTarget('open');
    return;
  }
  if (op === 'display-target-focus') {
    stationOperateSelectedDisplayTarget('focus');
    return;
  }
  if (op === 'display-target-input') {
    stationOperateSelectedDisplayTarget('input');
    return;
  }
  if (op === 'display-target-capture') {
    stationOperateSelectedDisplayTarget('capture');
    return;
  }
  if (op === 'display-target-copy') {
    stationOperateSelectedDisplayTarget('copy');
    return;
  }
  if (op === 'shared-view-focus') {
    stationFocusSharedViewDisplay();
    return;
  }
  if (op === 'voice-active') {
    document.getElementById('makeActiveBtn')?.click();
    return;
  }
  if (op === 'voice-toggle') {
    document.getElementById('micBtn')?.click();
    return;
  }
  if (op === 'video-toggle') {
    document.getElementById('videoBtn')?.click();
    return;
  }
  if (op === 'browser-open' || op === 'browser-new') {
    if (stationRenderedSelectPanel('system:controls', 'Browser controls stay in Station')) return;
    stationForceRouteTo('debug', undefined, () => document.getElementById('browser-workspace-url')?.focus());
    return;
  }
  if (op === 'browser-create') {
    stationOperateBrowserWorkspace('create');
    return;
  }
  if (op === 'browser-acquire') {
    stationOperateBrowserWorkspace('acquire');
    return;
  }
  if (op === 'browser-close') {
    stationOperateBrowserWorkspace('close');
    return;
  }
  if (op === 'browser-copy') {
    stationOperateBrowserWorkspace('copy');
    return;
  }
  if (op === 'recordings') {
    if (stationRenderedSelectPanel('system:controls', 'Recordings listed in the Controls panel')) return;
    stationForceRouteTo('displays', undefined, () => document.getElementById('recording-stream-select')?.focus());
    return;
  }
  if (op === 'launch-dock') {
    if (stationRenderedPrimaryActive()) {
      station?.set_composer?.(true, 'launch');
      stationHandleComposerEvent({ op: 'focus' });
      return;
    }
    stationForceRouteTo('sessions', 'new', () => document.getElementById('new-session-input')?.focus());
    return;
  }
  if (op === 'start-session') {
    stationStartSession();
    return;
  }
  if (op === 'debug-screen') {
    window.toggleDebugScreen?.();
    stationOpenPanel('system:controls', 'Debug screen toggled');
    return;
  }
  if (op === 'debug-record') {
    window.toggleDebugRecording?.();
    stationOpenPanel('system:controls', 'Debug recording toggled');
    return;
  }
  if (op === 'attachments-clear') {
    document.querySelector('[data-pending-attachments-clear]')?.click();
    return;
  }
  if (op.startsWith('queue-cancel:')) {
    cancelSteerRow(op.slice('queue-cancel:'.length));
    stationScheduleUpdate();
    return;
  }
  if (op === 'shared-view-take-input') {
    document.querySelector('[data-shared-view-take-input]')?.click();
    stationScheduleUpdate();
    return;
  }
  if (op === 'shared-view-hide') {
    document.querySelector('[data-shared-view-close]')?.click();
    stationScheduleUpdate();
    return;
  }
  if (op === 'cu-settings') {
    if (stationRenderedSelectPanel('system:controls', 'Computer-use settings stay in Station')) return;
    stationForceRouteTo('settings', 'agent', () => document.getElementById('set-cu-provider')?.focus());
  }
}

// ── Station composer ──
// The canvas draws the composer strip chrome; the real text editing
// happens in #station-composer-input, a transparent textarea positioned
// exactly over the drawn input slot (station.composer_state() geometry).
let stationComposerMode = 'send';

function stationComposerInputEl() {
  return document.getElementById('station-composer-input');
}

// One-time wiring of the composer overlay textarea (station init).
function stationSetupComposerInput() {
  const input = stationComposerInputEl();
  if (!input || input.dataset.stationWired) return;
  input.dataset.stationWired = '1';
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      stationComposerSubmit();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      station?.set_composer?.(false, stationComposerMode);
      stationSyncComposer();
      input.blur();
    }
    // Keep dashboard-global hotkeys away from composer typing.
    e.stopPropagation();
  });
  window.addEventListener('resize', () => stationSyncComposer());
}

function stationSyncComposer() {
  const input = stationComposerInputEl();
  if (!input) return;
  let state = null;
  try {
    state = station && typeof station.composer_state === 'function'
      ? JSON.parse(station.composer_state() || 'null')
      : null;
  } catch (_) {
    state = null;
  }
  if (!state || !state.open || !state.rect || !stationRenderedPrimaryActive()) {
    if (input.style.display !== 'none') {
      input.style.display = 'none';
      if (document.activeElement === input) input.blur();
    }
    return;
  }
  stationComposerMode = state.mode === 'launch' ? 'launch' : 'send';
  input.placeholder = stationComposerMode === 'launch'
    ? 'describe the task for the new session — enter launches'
    : 'type a task or steer — enter sends, esc closes';
  input.style.display = '';
  input.style.left = `${state.rect.x}px`;
  input.style.top = `${state.rect.y}px`;
  input.style.width = `${state.rect.w}px`;
  input.style.height = `${state.rect.h}px`;
}

function stationComposerSubmit() {
  const input = stationComposerInputEl();
  if (!input) return;
  const text = String(input.value || '').trim();
  if (stationComposerMode === 'launch') {
    if (!text) {
      showControlToast?.('error', 'Describe the task for the new session first');
      input.focus();
      return;
    }
    stationLaunchDraft.task = text;
    input.value = '';
    station?.set_composer?.(false, 'launch');
    stationSyncComposer();
    stationStartSession();
    return;
  }
  if (!text) {
    input.focus();
    return;
  }
  if (editMessageDraft) {
    showControlToast?.('info', 'Finish or cancel the message edit in the Activity tab first');
    return;
  }
  if (submitComposedText(text)) {
    input.value = '';
    stationStatus('Dispatched from Station composer');
    stationScheduleUpdate({ immediate: true });
    stationMaybeRefreshTranscript(true);
  }
}

function stationHandleComposerEvent(action) {
  const op = String(action.op || '').trim();
  if (op === 'focus') {
    // The strip just opened (pill, `/` key, or set_composer): the next
    // paint computes the slot rect, so place + focus on the next frame.
    requestAnimationFrame(() => {
      stationSyncComposer();
      stationComposerInputEl()?.focus();
    });
    return;
  }
  if (op === 'closed') {
    stationSyncComposer();
    return;
  }
  if (op === 'send' || op === 'launch') {
    stationComposerSubmit();
    return;
  }
  if (op === 'target') {
    stationRenderedSelectPanel('system:sessions', 'Pick a session — its focus pill makes it the prompt target');
  }
}

// ── Station transcript viewer ──
// Conversation tails are fetched from /api/session/{id} on demand and fed
// through the set_transcript side-channel (never the 300ms snapshot).
// While the viewer is open, new activity for that session triggers a
// coalesced refetch; the WASM rejects refreshes once the viewer closed.
let stationTranscriptLive = null;

function stationTranscriptKindForEntry(entry) {
  if (isSessionDetailUserEntry(entry)) return 'user';
  const level = String(entry?.level || '').toLowerCase();
  if (level === 'error') return 'error';
  if (level === 'warn') return 'warn';
  if (level === 'model') return 'model';
  if (level === 'agent' || level === 'subagent') return 'agent';
  const source = String(entry?.source || '').toLowerCase();
  if (source === 'worker') return 'model';
  if (source === 'agent' || source === 'orch' || source === 'sub') return 'agent';
  if (entry?.event === 'command' || entry?.command !== undefined || entry?.stdout !== undefined || entry?.stderr !== undefined) return 'tool';
  return 'info';
}

function stationTranscriptTextForEntry(entry) {
  for (const key of ['content', 'message', 'summary', 'text', 'stdout', 'stderr', 'task', 'reasoning_summary', 'reasoningSummary', 'command']) {
    const value = entry?.[key];
    if (typeof value === 'string' && value.trim()) return value;
  }
  if (typeof entry?.msg === 'string' && entry.msg.trim()) return entry.msg;
  return '';
}

function stationTranscriptTsForEntry(entry) {
  const raw = String(entry?.ts || entry?.timestamp || entry?.time || '').trim();
  if (!raw) return '';
  const t = formatContextTimestamp(raw);
  return String(t || raw).slice(-8);
}

function stationTranscriptRowsFromEntries(entries) {
  const rows = [];
  for (const entry of entries || []) {
    if (!entry || entry.event === 'context_snapshot' || entry.event === 'replay_start') continue;
    const text = stationTranscriptTextForEntry(entry);
    if (!text.trim()) continue;
    rows.push({
      kind: stationTranscriptKindForEntry(entry),
      ts: stationTranscriptTsForEntry(entry),
      // Cap pathological single entries; the viewer wraps the rest.
      text: text.length > 4000 ? `${text.slice(0, 4000)} …` : text,
    });
  }
  // The viewer follows the tail; keep the freshest entries.
  return rows.slice(-260);
}

function stationLatestEventKeyForSession(sessionId) {
  const events = stationActivityEvents();
  for (let i = events.length - 1; i >= 0; i--) {
    const ev = events[i];
    if (String(ev?.sessionId || ev?.session_id || '') === sessionId) {
      return stationActivityEventKey(ev);
    }
  }
  return '';
}

async function stationOpenTranscript(sessionId, opts = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid || !station || typeof station.set_transcript !== 'function') return false;
  const session = stationFindSessionById(sid);
  const source = String(
    opts.source ||
    sessionConfigSource(sessionConfigMetadata(session || sid)) ||
    session?.backend_source || session?.backendSource || session?.source ||
    'intendant'
  ).trim() || 'intendant';
  const label = stationSessionTask(session) || shortSessionId(sid);
  if (!opts.refresh) stationStatus(`Loading transcript ${shortSessionId(sid)}`);
  let payload;
  try {
    const data = await fetchSessionDetailPayload(sid, { source, limit: 300 });
    if (data.error) {
      payload = { sessionId: sid, source, label, mode: 'log', refresh: !!opts.refresh, error: String(data.error), total: 0, rows: [] };
    } else {
      const entries = Array.isArray(data.entries) ? data.entries : [];
      payload = {
        sessionId: sid,
        source,
        label,
        mode: 'log',
        refresh: !!opts.refresh,
        error: '',
        total: entries.length,
        rows: stationTranscriptRowsFromEntries(entries),
      };
    }
  } catch (e) {
    payload = { sessionId: sid, source, label, mode: 'log', refresh: !!opts.refresh, error: `transcript fetch failed: ${e?.message || e}`, total: 0, rows: [] };
  }
  let accepted = false;
  try {
    accepted = station.set_transcript(payload) === true;
  } catch (err) {
    console.warn('station set_transcript rejected:', err);
  }
  if (accepted) {
    stationTranscriptLive = {
      sessionId: sid,
      source,
      fetchedAt: Date.now(),
      lastEventKey: stationLatestEventKeyForSession(sid),
    };
    if (!opts.refresh) stationStatus(`Transcript ${shortSessionId(sid)} loaded`);
  } else if (opts.refresh && stationTranscriptLive?.sessionId === sid) {
    // Viewer closed (or moved to another session): stop live refresh.
    stationTranscriptLive = null;
  }
  return accepted;
}

// Coalesced live refresh, called from the snapshot apply path: refetch
// when fresh activity arrived for the open session (or every ~8s for
// sessions whose output doesn't surface in the activity stream).
function stationMaybeRefreshTranscript(force = false) {
  const live = stationTranscriptLive;
  if (!live || !stationRenderedPrimaryActive()) return;
  const age = Date.now() - live.fetchedAt;
  if (!force && age < 1500) return;
  const key = stationLatestEventKeyForSession(live.sessionId);
  if (!force && key && key === live.lastEventKey && age < 8000) return;
  if (!force && !key && age < 8000) return;
  live.fetchedAt = Date.now();
  live.lastEventKey = key;
  stationOpenTranscript(live.sessionId, { source: live.source, refresh: true });
}

