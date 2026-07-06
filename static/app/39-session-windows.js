function clearSessionWindowPendingActive(sessionId, phase = '') {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const win = sessionWindows.get(sid);
  if (win) {
    win.pendingActiveUntil = 0;
    const current = normalizeSessionPhase(win.phase || 'idle');
    const nextPhase = phase || (current === 'thinking' ? 'idle' : current);
    updateSessionWindow(sid, { phase: nextPhase, ended: false });
  }
  if (sid === resolvePromptTargetSessionId()) {
    const nextPhase = phase || sessionWindows.get(sid)?.phase || 'idle';
    setPhase(nextPhase);
  } else {
    updateSubmitButtonLabel(currentPhase);
    updateStopButtonVisibility(currentPhase);
    updateTaskTargetChip();
  }
}

function shouldKeepSilentExternalSessionActive(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  if (!sid || sessionWindowIsDetached(sid)) return false;
  const source = externalSourceForSessionWindow(sid, win);
  return !!source && source !== 'intendant';
}

function markSessionWindowPendingActive(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const expiresAt = Date.now() + SESSION_PENDING_ACTIVE_MS;
  const win = ensureSessionWindow(sid, { phase: 'thinking', ended: false });
  if (!win) return;
  win.pendingActiveUntil = expiresAt;
  win.optimisticActiveExpired = false;
  updateSessionWindow(sid, { phase: 'thinking', ended: false });
  if (sid === resolvePromptTargetSessionId()) {
    setPhase('thinking');
  } else {
    updateTaskTargetChip();
  }
  setTimeout(() => {
    const latest = sessionWindows.get(sid);
    if (!latest || latest.pendingActiveUntil !== expiresAt) return;
    if (shouldKeepSilentExternalSessionActive(sid, latest)) {
      // Keep DISPLAYING the window as thinking (silent external runtimes
      // are often mid-turn without log lines), but flag the phase as
      // unconfirmed so the steer/follow-up decision stops trusting it.
      latest.pendingActiveUntil = 0;
      latest.optimisticActiveExpired = true;
      persistSessionWindowState();
      return;
    }
    clearSessionWindowPendingActive(sid);
  }, SESSION_PENDING_ACTIVE_MS + 50);
}

function updateTaskTargetChip() {
  const sid = resolvePromptTargetSessionId();
  updatePromptTargetSessionHighlight(sid);
  schedulePromptTargetLogSessionBadgeRefresh(sid);
  updateControlFastButtonState();
  const chip = document.getElementById('task-target-chip');
  if (!chip) return;
  clearSessionBadgeStyle(chip);
  if (!sid) {
    chip.style.display = 'none';
    chip.classList.remove('has-target');
    chip.replaceChildren();
    chip.title = '';
    chip.removeAttribute('aria-label');
    return;
  }
  chip.style.display = '';
  chip.classList.add('has-target');
  const label = document.createElement('span');
  label.className = 'task-target-label';
  label.textContent = 'Target:';
  const badge = document.createElement('span');
  badge.className = 'task-target-session-badge';
  renderSessionIdentity(badge, sid, { showName: false });
  applySessionBadgeStyle(badge, sid);
  chip.replaceChildren(label, badge);
  const title = `Prompt target: ${sid}`;
  chip.title = title;
  chip.setAttribute('aria-label', title);
}

function updatePromptTargetSessionHighlight(target = resolvePromptTargetSessionId()) {
  for (const [sid, win] of sessionWindows) {
    win.el.classList.toggle('prompt-target', !!target && sid === target && !win.ended);
  }
}

function schedulePromptTargetLogSessionBadgeRefresh(target = resolvePromptTargetSessionId(), options = {}) {
  const normalizedTarget = String(target || '').trim();
  if (!options.force && !promptTargetLogBadgeRefreshFrame && promptTargetLogBadgeRenderedTarget === normalizedTarget) return;
  promptTargetLogBadgeScheduledTarget = normalizedTarget;
  if (promptTargetLogBadgeRefreshFrame) return;
  promptTargetLogBadgeRefreshFrame = requestAnimationFrame(() => {
    promptTargetLogBadgeRefreshFrame = 0;
    updatePromptTargetLogSessionBadges(promptTargetLogBadgeScheduledTarget);
  });
}

function updatePromptTargetLogSessionBadges(target = resolvePromptTargetSessionId()) {
  const normalizedTarget = String(target || '').trim();
  const previousTarget = promptTargetLogBadgeRenderedTarget === null
    ? ''
    : String(promptTargetLogBadgeRenderedTarget || '').trim();
  if (promptTargetLogBadgeRefreshFrame) {
    cancelAnimationFrame(promptTargetLogBadgeRefreshFrame);
    promptTargetLogBadgeRefreshFrame = 0;
  }
  promptTargetLogBadgeScheduledTarget = normalizedTarget;
  if (previousTarget && previousTarget !== normalizedTarget) {
    updatePromptTargetLogSessionBadgesForSession(previousTarget, normalizedTarget);
  }
  updatePromptTargetLogSessionBadgesForSession(normalizedTarget, normalizedTarget);
  promptTargetLogBadgeRenderedTarget = normalizedTarget;
}

function updatePromptTargetLogSessionBadgesForSession(sessionId, target) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const selector = window.CSS?.escape
    ? `.log-entry[data-session-id="${CSS.escape(sid)}"]`
    : '.log-entry[data-session-id]';
  const apply = entry => {
    if (entry.dataset.sessionId !== sid) return;
    applyPromptTargetLogSessionBadgeState(entry, target);
  };
  document.querySelectorAll(selector).forEach(apply);
  if (concurrentLogDetachedFragment) {
    concurrentLogDetachedFragment.querySelectorAll(selector).forEach(apply);
  }
}

function applyPromptTargetLogSessionBadgeState(entry, target = resolvePromptTargetSessionId()) {
  const sid = String(entry?.dataset?.sessionId || '').trim();
  const highlighted = !!target && sid === target;
  for (const child of entry?.children || []) {
    if (child.classList?.contains('log-session')) {
      child.classList.toggle('prompt-target-log', highlighted);
    }
  }
}

function promptTargetTerminalStatus(meta = {}) {
  const status = String(meta.status || meta.intendantStatus || '').trim().toLowerCase();
  return status === 'abandoned' || status === 'deleted' || status === 'missing';
}

function isPromptTargetSessionUsable(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid || sid === daemonSessionFullId) return false;
  const win = sessionWindows.get(sid);
  if (!win || win.ended) return false;
  const allowDetached = options.allowDetached !== false;
  if (sessionWindowIsDetached(sid) && !allowDetached) return false;
  const meta = sessionMetadataById.get(sid) || {};
  if (meta.ended || promptTargetTerminalStatus(meta)) return false;
  return true;
}

function isPromptTargetMetadataUsable(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || sid === daemonSessionFullId) return false;
  const meta = sessionMetadataById.get(sid) || {};
  if (meta.ended || promptTargetTerminalStatus(meta)) return false;
  const source = normalizeAgentId(
    meta.backendSource ||
    meta.backend_source ||
    meta.source ||
    meta.sourceLabel ||
    meta.source_label ||
    ''
  );
  if (!source || source === 'intendant') return false;
  const intendantSessionId = String(
    meta.intendantSessionId ||
    meta.intendant_session_id ||
    ''
  ).trim();
  return !intendantSessionId || !daemonSessionFullId || intendantSessionId === daemonSessionFullId;
}

function discardPromptTargetReference(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  if (foregroundSessionFullId === sid) foregroundSessionFullId = '';
  if (currentSessionFullId === sid) currentSessionFullId = '';
}

function resolvePromptTargetSessionId() {
  for (const sid of [foregroundSessionFullId, currentSessionFullId]) {
    if (!sid) continue;
    if (isPromptTargetSessionUsable(sid)) return sid;
    discardPromptTargetReference(sid);
  }
  const fallback = stationLatestConfigurableExternalSessionId();
  if (isPromptTargetMetadataUsable(fallback)) return fallback;
  const candidates = Array.from(sessionWindows.entries())
    .filter(([id]) => isPromptTargetSessionUsable(id))
    .map(([id]) => id);
  return candidates.length === 1 ? candidates[0] : '';
}

function compactSessionText(value) {
  if (value === null || value === undefined) return '';
  if (value instanceof Element) return compactSessionText(value.textContent || '');
  if (typeof value === 'object') {
    for (const key of ['text', 'content', 'message', 'task', 'title', 'name']) {
      if (typeof value[key] === 'string' && value[key].trim()) return compactSessionText(value[key]);
    }
    return '';
  }
  return String(value || '').replace(/\s+/g, ' ').trim();
}

function compactSessionTextBounded(value, limit = SESSION_TEXT_SIGNATURE_CHAR_LIMIT, options = {}) {
  if (value === null || value === undefined) return '';
  if (value instanceof Element) return compactSessionTextBounded(value.textContent || '', limit, options);
  if (typeof value === 'object') {
    for (const key of ['text', 'content', 'message', 'task', 'title', 'name']) {
      const candidate = value[key];
      if (typeof candidate === 'string' && /\S/.test(candidate)) {
        return compactSessionTextBounded(candidate, limit, options);
      }
    }
    return '';
  }
  const raw = String(value || '');
  const safeLimit = Math.max(0, Number(limit) || 0);
  const truncated = safeLimit > 0 && raw.length > safeLimit;
  const src = truncated ? raw.slice(0, safeLimit) : raw;
  const compact = src.replace(/\s+/g, ' ').trim();
  if (!truncated || options.suffix === false) return compact;
  const suffix = options.signature ? ` [len:${raw.length}]` : `... (${raw.length} chars)`;
  return compact ? `${compact}${suffix}` : suffix.trim();
}

function utf8ByteLength(value) {
  const s = String(value || '');
  let bytes = 0;
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && i + 1 < s.length) {
      const next = s.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        i += 1;
      } else {
        bytes += 3;
      }
    } else {
      bytes += 3;
    }
  }
  return bytes;
}

function sessionIdentityParts(sessionId) {
  const sid = String(sessionId || '').trim();
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  const name = compactSessionText(meta.name || meta.display_name || meta.displayName || meta.thread_name || meta.threadName || meta.title);
  return { sid, shortId: shortSessionId(sid), name };
}

function appendSessionIdentityPart(el, className, text) {
  if (!text) return;
  const part = document.createElement('span');
  part.className = className;
  part.textContent = text;
  el.appendChild(part);
}

function renderSessionIdentity(el, sessionId, options = {}) {
  if (!el) return;
  const { sid, shortId, name } = sessionIdentityParts(sessionId);
  el.replaceChildren();
  if (!sid) {
    el.title = '';
    el.removeAttribute('aria-label');
    return;
  }

  const showName = options.showName !== false && !!name;
  const titleText = showName ? `${name} \u00b7 ${sid}` : sid;
  const prefix = compactSessionText(options.prefix);
  if (prefix) appendSessionIdentityPart(el, 'session-identity-prefix', prefix);

  const shortClass = options.includeFullId
    ? 'session-identity-short session-identity-short-collapsed'
    : 'session-identity-short';
  const appendId = () => {
    appendSessionIdentityPart(el, shortClass, shortId);
    if (options.includeFullId) appendSessionIdentityPart(el, 'session-identity-full', sid);
  };

  if (options.order === 'name-id' && showName) {
    appendSessionIdentityPart(el, 'session-identity-name', name);
    appendSessionIdentityPart(el, 'session-identity-separator', '\u00b7');
    appendId();
  } else {
    appendId();
    if (showName) {
      appendSessionIdentityPart(el, 'session-identity-separator', '\u00b7');
      appendSessionIdentityPart(el, 'session-identity-name', name);
    }
  }

  const title = options.titlePrefix ? `${options.titlePrefix}: ${titleText}` : titleText;
  el.title = title;
  el.setAttribute('aria-label', title);
}

function setSessionWindowPathElement(el, fullValue, compactValue, fallback, titleFallback) {
  if (!el) return;
  const full = compactSessionText(fullValue);
  const compact = compactSessionText(compactValue) || full || fallback;
  el.dataset.fullText = full || '';
  el.dataset.compactText = compact || fallback;
  el.dataset.fallbackText = fallback;
  el.title = full || titleFallback;
}

function refreshSessionWindowPathElement(el, expanded) {
  if (!el) return;
  const full = el.dataset.fullText || '';
  const compact = el.dataset.compactText || '';
  const fallback = el.dataset.fallbackText || 'unknown';
  el.textContent = expanded && full ? full : (compact || full || fallback);
}

function refreshSessionWindowPathLabels(win) {
  if (!win) return;
  const expanded = !win.headerCollapsed && !win.minimized;
  refreshSessionWindowPathElement(win.project, expanded);
  refreshSessionWindowPathElement(win.cwd, expanded);
}

function refreshSessionIdentityLabels(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const target = resolvePromptTargetSessionId();
  if (sid === target) updateTaskTargetChip();
  const selector = window.CSS?.escape
    ? `.log-entry[data-session-id="${CSS.escape(sid)}"]`
    : '.log-entry[data-session-id]';
  document.querySelectorAll(selector).forEach(entry => {
    if (entry.dataset.sessionId !== sid) return;
    for (const child of entry.children) {
      if (child.classList?.contains('log-session')) {
        renderSessionIdentity(child, sid, { showName: false });
        applyPromptTargetLogSessionBadgeState(entry, target);
      }
    }
  });
}

function compactPathLabel(value, preserveAbsolute = false) {
  const path = compactSessionText(value);
  if (!path) return '';
  const isUnixAbsolute = path.startsWith('/');
  const isWindowsAbsolute = /^[A-Za-z]:[\\/]/.test(path);
  const parts = path.split(/[\\/]+/).filter(Boolean);
  if (preserveAbsolute && isUnixAbsolute) {
    if (parts.length <= 4) return path;
    return `/${parts[0]}/${parts[1]}/.../${parts.slice(-2).join('/')}`;
  }
  if (preserveAbsolute && isWindowsAbsolute) {
    if (parts.length <= 4) return path;
    return `${parts[0]}\\${parts[1]}\\...\\${parts.slice(-2).join('\\')}`;
  }
  if (parts.length >= 2) return parts.slice(-2).join('/');
  return path;
}

function numberOrNull(value) {
  if (value === null || value === undefined || value === '') return null;
  const n = Number(value);
  return Number.isFinite(n) && n >= 0 ? Math.floor(n) : null;
}

function normalizeGoalStatus(value) {
  const raw = String(value || '').trim();
  if (!raw) return 'active';
  if (raw === 'budgetLimited' || raw === 'budget_limited') return 'budget-limited';
  if (raw === 'usageLimited' || raw === 'usage_limited') return 'usage-limited';
  return raw.replace(/_/g, '-').toLowerCase();
}

function normalizeSessionGoal(raw = null) {
  if (raw === null || raw === false) return null;
  const goal = raw && typeof raw === 'object' && raw.goal !== undefined ? raw.goal : raw;
  if (goal === null || goal === false) return null;
  if (!goal || typeof goal !== 'object') return null;
  const objective = compactSessionText(goal.objective || goal.goal || goal.title);
  if (!objective) return null;
  const elapsedSeconds = numberOrNull(
    goal.elapsed_seconds ??
    goal.elapsedSeconds ??
    goal.timeUsedSeconds ??
    goal.time_used_seconds
  );
  return {
    objective,
    status: normalizeGoalStatus(goal.status),
    elapsedSeconds,
    tokensUsed: numberOrNull(goal.tokens_used ?? goal.tokensUsed),
    tokenBudget: numberOrNull(goal.token_budget ?? goal.tokenBudget),
    observedAtMs: Number.isFinite(goal.observedAtMs) ? goal.observedAtMs : performance.now(),
  };
}

function sessionGoalSignature(goal) {
  if (!goal) return '';
  return [
    goal.objective || '',
    goal.status || '',
    goal.elapsedSeconds ?? '',
    goal.tokensUsed ?? '',
    goal.tokenBudget ?? '',
  ].join('\u001f');
}

function normalizeSessionWindowMeta(meta = {}) {
  const out = {};
  const name = compactSessionText(meta.name || meta.display_name || meta.displayName || meta.thread_name || meta.threadName || meta.title);
  if (name) out.name = name;
  const task = compactSessionText(meta.initial_message || meta.initialMessage || meta.task);
  if (task) out.task = task;
  const project = compactSessionText(meta.project_root || meta.projectRoot || meta.project_dir || meta.projectDir || meta.project);
  if (project) {
    out.projectRoot = project;
    out.projectLabel = compactPathLabel(project, true);
  }
  const cwd = compactSessionText(meta.cwd || meta.workdir || meta.workDir || meta.worktree || project);
  if (cwd) {
    out.cwd = cwd;
    out.cwdLabel = compactPathLabel(cwd, true);
  }
  const source = compactSessionText(meta.source || meta.source_id || meta.sourceId);
  if (source) out.source = source;
  const status = compactSessionText(meta.status);
  if (status) out.status = status.toLowerCase();
  const intendantStatus = compactSessionText(meta.intendant_status || meta.intendantStatus);
  if (intendantStatus) out.intendantStatus = intendantStatus.toLowerCase();
  const sourceLabel = compactSessionText(meta.source_label || meta.sourceLabel);
  if (sourceLabel) out.sourceLabel = sourceLabel;
  const backendSource = compactSessionText(meta.backend_source || meta.backendSource);
  if (backendSource) out.backendSource = backendSource;
  const backendSessionId = compactSessionText(meta.backend_session_id || meta.backendSessionId || meta.thread_id || meta.threadId);
  if (backendSessionId) out.backendSessionId = backendSessionId;
  const intendantSessionId = compactSessionText(meta.intendant_session_id || meta.intendantSessionId);
  if (intendantSessionId) out.intendantSessionId = intendantSessionId;
  const agentCommand = compactSessionText(meta.agent_command || meta.agentCommand || meta.codex_command || meta.codexCommand);
  if (agentCommand) out.agentCommand = agentCommand;
  const managedMode = String(meta.codex_managed_context || meta.codexManagedContext || '').trim();
  if (managedMode === 'managed' || managedMode === 'vanilla') out.codexManagedContext = managedMode;
  const archiveMode = String(meta.codex_context_archive || meta.codexContextArchive || '').trim();
  if (['summary', 'exact', 'off'].includes(archiveMode)) out.codexContextArchive = archiveMode;
  if (meta.capabilities) out.capabilities = normalizeSessionCapabilities(meta.capabilities);
  if (
    Object.prototype.hasOwnProperty.call(meta, 'goal') ||
    Object.prototype.hasOwnProperty.call(meta, 'session_goal') ||
    Object.prototype.hasOwnProperty.call(meta, 'sessionGoal')
  ) {
    const rawGoal = Object.prototype.hasOwnProperty.call(meta, 'goal')
      ? meta.goal
      : (Object.prototype.hasOwnProperty.call(meta, 'session_goal') ? meta.session_goal : meta.sessionGoal);
    out.goal = normalizeSessionGoal(rawGoal);
  }
  const parentId = compactSessionText(meta.parent_session_id || meta.parentSessionId || meta.parent_id || meta.parentId);
  if (parentId) out.parentId = parentId;
  const relationshipRaw = meta.relationship_kind || meta.relationshipKind || meta.relationship;
  if (relationshipRaw) out.relationshipKind = normalizeSessionRelationshipKind(relationshipRaw);
  const threadSource = compactSessionText(meta.thread_source || meta.threadSource);
  if (threadSource) out.threadSource = threadSource.toLowerCase().replace(/_/g, '-');
  const agentNickname = compactSessionText(meta.agent_nickname || meta.agentNickname);
  if (agentNickname) out.agentNickname = agentNickname;
  const relationshipEphemeral = meta.relationship_ephemeral ?? meta.relationshipEphemeral ?? meta.ephemeral;
  if (relationshipEphemeral !== undefined) out.relationshipEphemeral = !!relationshipEphemeral;
  if (meta.phase) out.phase = meta.phase;
  if (meta.ended !== undefined) out.ended = !!meta.ended;
  return out;
}

function sessionWindowMetadataSignature(meta = {}) {
  return [
    meta.name || '',
    meta.task || '',
    meta.projectRoot || '',
    meta.projectLabel || '',
    meta.cwd || '',
    meta.cwdLabel || '',
    meta.source || '',
    meta.sourceLabel || '',
    meta.backendSource || '',
    meta.status || '',
    meta.intendantStatus || '',
    meta.backendSessionId || '',
    meta.intendantSessionId || '',
    meta.agentCommand || '',
    meta.codexManagedContext || '',
    meta.capabilities?.codexCommand || '',
    meta.capabilities?.codexManagedContext || '',
    meta.capabilities?.codexFastMode === undefined || meta.capabilities?.codexFastMode === null
      ? ''
      : (meta.capabilities.codexFastMode ? 'fast' : 'normal'),
    meta.capabilities?.codexServiceTier || '',
    sessionGoalSignature(meta.goal),
    meta.parentId || '',
    meta.relationshipKind || '',
    meta.threadSource || '',
    meta.agentNickname || '',
    meta.relationshipEphemeral === undefined ? '' : (meta.relationshipEphemeral ? '1' : '0'),
    meta.phase || '',
    meta.ended === undefined ? '' : (meta.ended ? '1' : '0'),
  ].join('\u001f');
}

function mergeSessionWindowMetadata(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  const normalized = normalizeSessionWindowMeta(meta);
  const previous = sid ? (sessionMetadataById.get(sid) || {}) : {};
  const merged = Object.keys(normalized).length > 0
    ? { ...previous, ...normalized }
    : previous;
  const changed = sessionWindowMetadataSignature(previous) !== sessionWindowMetadataSignature(merged);
  if (sid && changed) sessionMetadataById.set(sid, merged);
  return { meta: merged, changed };
}

function currentSessionGoalElapsedSeconds(goal) {
  if (!goal) return null;
  const base = numberOrNull(goal.elapsedSeconds) ?? 0;
  if (goal.status !== 'active' || !Number.isFinite(goal.observedAtMs)) return base;
  return base + Math.max(0, Math.floor((performance.now() - goal.observedAtMs) / 1000));
}

function formatGoalElapsed(seconds) {
  const n = numberOrNull(seconds) ?? 0;
  const days = Math.floor(n / 86400);
  const hours = Math.floor((n % 86400) / 3600);
  const minutes = Math.floor((n % 3600) / 60);
  const secs = n % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

function goalStatusClass(status) {
  if (status === 'budget-limited') return 'budget-limited';
  if (status === 'usage-limited') return 'usage-limited';
  if (status === 'complete') return 'complete';
  if (status === 'paused') return 'paused';
  return 'active';
}

function renderSessionWindowGoal(win, goal) {
  if (!win?.goal || !win?.goalText) return false;
  if (!goal?.objective) {
    win.goal.className = 'session-window-goal hidden';
    win.goalText.textContent = '';
    win.goal.title = '';
    return false;
  }
  const status = normalizeGoalStatus(goal.status);
  const elapsed = formatGoalElapsed(currentSessionGoalElapsedSeconds(goal));
  win.goal.className = `session-window-goal ${goalStatusClass(status)}`;
  win.goalText.textContent = status === 'active'
    ? `${goal.objective} · ${elapsed}`
    : `${status} · ${goal.objective} · ${elapsed}`;
  const tokenText = goal.tokensUsed !== null && goal.tokensUsed !== undefined
    ? `\nTokens: ${goal.tokensUsed.toLocaleString()}${goal.tokenBudget ? ` / ${goal.tokenBudget.toLocaleString()}` : ''}`
    : (goal.tokenBudget ? `\nBudget: ${goal.tokenBudget.toLocaleString()} tokens` : '');
  win.goal.title = `Goal: ${goal.objective}\nStatus: ${status}\nElapsed: ${elapsed}${tokenText}`;
  return status === 'active';
}

function renderSessionWindowTier(win) {
  if (!win?.tier) return;
  const sid = win.sessionId || '';
  if (!sid || !sessionWindowIsCodex(sid, win)) {
    win.tier.className = 'session-window-tier hidden';
    win.tier.textContent = '';
    win.tier.title = '';
    return;
  }
  const fastMode = sessionCodexFastMode(sid);
  if (fastMode === null) {
    win.tier.className = 'session-window-tier hidden';
    win.tier.textContent = '';
    win.tier.title = '';
    return;
  }
  win.tier.className = `session-window-tier ${fastMode ? 'fast' : 'normal'}`;
  win.tier.textContent = fastMode ? 'Fast' : 'Normal';
  win.tier.title = sessionCodexServiceTierTitle(sid);
}

function refreshSessionGoalTicker() {
  let hasActiveGoal = false;
  for (const [sid, win] of sessionWindows) {
    const goal = (sessionMetadataById.get(sid) || {}).goal || null;
    if (renderSessionWindowGoal(win, goal)) hasActiveGoal = true;
  }
  if (hasActiveGoal && !sessionGoalTicker) {
    sessionGoalTicker = setInterval(refreshSessionGoalTicker, 1000);
  } else if (!hasActiveGoal && sessionGoalTicker) {
    clearInterval(sessionGoalTicker);
    sessionGoalTicker = null;
  }
}

function sessionGoalUpdateIds(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return [];
  const ids = new Set([sid]);
  const direct = sessionMetadataById.get(sid) || {};
  if (direct.backendSessionId) ids.add(direct.backendSessionId);
  if (direct.intendantSessionId) ids.add(direct.intendantSessionId);
  for (const [id, meta] of sessionMetadataById) {
    if (meta?.backendSessionId === sid || meta?.intendantSessionId === sid) ids.add(id);
  }
  return Array.from(ids);
}

function applySessionGoal(raw = {}) {
  const data = raw?.data && typeof raw.data === 'object' ? raw.data : raw;
  const sid = String(data?.session_id || data?.sessionId || '').trim();
  if (!sid) return;
  const hasGoalField =
    data && (
      Object.prototype.hasOwnProperty.call(data, 'goal') ||
      Object.prototype.hasOwnProperty.call(data, 'session_goal') ||
      Object.prototype.hasOwnProperty.call(data, 'sessionGoal')
    );
  const rawGoal = Object.prototype.hasOwnProperty.call(data, 'goal')
    ? data.goal
    : (Object.prototype.hasOwnProperty.call(data, 'session_goal') ? data.session_goal : data.sessionGoal);
  const goal = hasGoalField ? normalizeSessionGoal(rawGoal) : normalizeSessionGoal(data);
  for (const id of sessionGoalUpdateIds(sid)) {
    const meta = { ...(sessionMetadataById.get(id) || {}), goal };
    sessionMetadataById.set(id, meta);
    if (sessionWindows.has(id)) {
      const win = sessionWindows.get(id);
      win.metadataSignature = '';
      updateSessionWindow(id, meta);
    }
  }
  refreshSessionGoalTicker();
}

function applySessionGoalsFromReplayEntries(entries) {
  if (!Array.isArray(entries)) return;
  for (const entry of entries) {
    if (entry?.event !== 'session_goal') continue;
    applySessionGoal(entry);
  }
}

// ── Session vitals (git / cache / limits chips — the statusline port) ──

function sessionVitalsGitSegment(git) {
  if (!git || typeof git !== 'object') return null;
  const parts = [];
  const titleLines = [];
  const branch = String(git.branch || '').trim();
  if (branch) {
    parts.push(`⎇ ${branch}`);
    titleLines.push(`Branch: ${branch}`);
  }
  const dirty = Number(git.dirtyFiles) || 0;
  if (dirty > 0) {
    parts.push(`●${dirty}`);
    titleLines.push(`Dirty files: ${dirty}`);
  }
  const primaryRef = String(git.primaryRef || '').trim();
  if (primaryRef) {
    const ahead = Number(git.ahead) || 0;
    const behind = Number(git.behind) || 0;
    parts.push(`+${ahead}/−${behind}`);
    titleLines.push(`vs ${primaryRef}: ${ahead} ahead, ${behind} behind`);
  }
  const parity = String(git.mergeParity || '').trim();
  if (parity === 'clean') {
    parts.push('✓');
    titleLines.push(`Merge with ${primaryRef || 'primary'}: clean`);
  } else if (parity === 'conflict') {
    parts.push('⚠');
    titleLines.push(`Merge with ${primaryRef || 'primary'}: WOULD CONFLICT`);
  }
  if (git.unpushed !== null && git.unpushed !== undefined) {
    parts.push(`⇡${git.unpushed}`);
    titleLines.push(`Unpushed on ${branch || 'branch'}: ${git.unpushed}`);
  }
  if (git.primaryUnpushed !== null && git.primaryUnpushed !== undefined && primaryRef) {
    const primaryName = primaryRef.replace(/^origin\//, '');
    parts.push(`${primaryName}⇡${git.primaryUnpushed}`);
    titleLines.push(`Unpushed on ${primaryName}: ${git.primaryUnpushed}`);
  }
  if (!parts.length) return null;
  return { text: parts.join(' '), titleLines, conflict: parity === 'conflict' };
}

// Seconds until the prompt cache goes cold, from the server-stamped
// activity anchor; null when the provider's TTL is unknown (receipt-only).
function sessionCacheCountdownSeconds(cache) {
  const ttl = Number(cache?.ttlSeconds);
  const anchor = Number(cache?.lastActivityEpoch);
  if (!ttl || !anchor) return null;
  return Math.round(ttl - (Date.now() / 1000 - anchor));
}

function formatCacheCountdown(seconds) {
  const s = Math.max(0, Number(seconds) || 0);
  return `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
}

function sessionVitalsCacheSegment(cache) {
  if (!cache || typeof cache !== 'object') return null;
  const parts = [];
  const titleLines = [];
  let cls = 'vit-cache';
  const hit = Number.isFinite(Number(cache.hitPct)) && cache.hitPct !== null && cache.hitPct !== undefined
    ? Math.max(0, Math.min(100, Number(cache.hitPct)))
    : null;
  if (hit !== null) {
    parts.push(`⚡${hit}%`);
    titleLines.push(`Prompt-cache hit (latest request): ${hit}%`);
    cls += hit >= 90 ? ' cache-ok' : hit >= 50 ? ' cache-warn' : ' cache-crit';
  }
  const remaining = sessionCacheCountdownSeconds(cache);
  let ticking = false;
  if (remaining !== null) {
    if (remaining > 0) {
      parts.push(`⏳${formatCacheCountdown(remaining)}`);
      titleLines.push(`Cache expires in ${formatCacheCountdown(remaining)} (TTL ${formatCacheCountdown(cache.ttlSeconds)})`);
      if (remaining <= 60) cls += ' cache-expiring';
      ticking = true;
    } else {
      parts.push('✗');
      titleLines.push('Prompt cache is cold — the next request rebuilds it');
      cls += ' cache-cold';
    }
  }
  if (!parts.length) return null;
  return { text: parts.join(' '), titleLines, cls, ticking };
}

function formatLimitReset(resetsAtEpoch) {
  const remaining = Number(resetsAtEpoch) - Date.now() / 1000;
  if (!Number.isFinite(remaining) || remaining <= 0) return '';
  if (remaining >= 86400) return `${Math.round(remaining / 86400)}d`;
  if (remaining >= 3600) return `${Math.round(remaining / 3600)}h`;
  return `${Math.max(1, Math.round(remaining / 60))}m`;
}

// The gauge shows the most-used window; the tooltip lists them all.
function sessionVitalsLimitsSegment(limits) {
  if (!Array.isArray(limits) || !limits.length) return null;
  const windows = limits
    .map((w) => ({
      label: String(w?.label || '').trim() || 'window',
      usedPct: Math.max(0, Math.min(100, Number(w?.usedPct) || 0)),
      resetsAtEpoch: Number(w?.resetsAtEpoch) || null,
    }));
  const top = windows.reduce((a, b) => (b.usedPct > a.usedPct ? b : a));
  let text = `▮${top.usedPct}% ${top.label}`;
  let cls = 'vit-limits';
  let severity = '';
  if (top.usedPct >= 90) {
    severity = 'crit';
    cls += ' limits-crit';
    const reset = top.resetsAtEpoch ? formatLimitReset(top.resetsAtEpoch) : '';
    if (reset) text += ` ↻${reset}`;
  } else if (top.usedPct >= 70) {
    severity = 'warn';
    cls += ' limits-warn';
  }
  const titleLines = windows.map((w) => {
    const reset = w.resetsAtEpoch ? ` — resets in ${formatLimitReset(w.resetsAtEpoch) || 'moments'}` : '';
    return `Rate limit ${w.label}: ${w.usedPct}% used${reset}`;
  });
  return { text, titleLines, cls, severity };
}

// Renders the chip; returns true while a cache countdown is live (the
// vitals ticker keeps re-rendering until everything is cold or gone).
function renderSessionWindowVitals(win, vitals) {
  if (!win?.vitals) return false;
  const git = sessionVitalsGitSegment(vitals && typeof vitals === 'object' ? vitals.git : null);
  const cache = sessionVitalsCacheSegment(vitals && typeof vitals === 'object' ? vitals.cache : null);
  const limits = sessionVitalsLimitsSegment(vitals && typeof vitals === 'object' ? vitals.limits : null);
  if (!git && !cache && !limits) {
    win.vitals.className = 'session-window-vitals hidden';
    win.vitals.replaceChildren();
    win.vitals.title = '';
    return false;
  }
  const segments = [];
  for (const seg of [
    git && { cls: 'vit-git', text: git.text },
    cache && { cls: cache.cls, text: cache.text },
    limits && { cls: limits.cls, text: limits.text },
  ]) {
    if (!seg) continue;
    const span = document.createElement('span');
    span.className = seg.cls;
    span.textContent = seg.text;
    segments.push(span);
  }
  win.vitals.className = `session-window-vitals${git?.conflict ? ' conflict' : ''}`;
  win.vitals.replaceChildren(...segments);
  win.vitals.title = [
    ...(git?.titleLines || []),
    ...(cache?.titleLines || []),
    ...(limits?.titleLines || []),
  ].join('\n');
  return !!cache?.ticking;
}

// One toast (plus a browser notification when permission is already
// granted and the tab is hidden) per idle period, keyed by the activity
// anchor — new provider activity re-arms it. Fires only while there is
// still time to act, never after the cache went cold.
function maybeAlertCacheExpiry(sid, win, vitals) {
  const cache = vitals && typeof vitals === 'object' ? vitals.cache : null;
  if (!cache) return;
  const remaining = sessionCacheCountdownSeconds(cache);
  if (remaining === null || remaining <= 0 || remaining > 60) return;
  const key = String(cache.lastActivityEpoch);
  if (sessionCacheExpiryAlerts.get(sid) === key) return;
  sessionCacheExpiryAlerts.set(sid, key);
  if (sessionCacheExpiryAlerts.size > 200) {
    sessionCacheExpiryAlerts.delete(sessionCacheExpiryAlerts.keys().next().value);
  }
  const identity = sessionIdentityParts(sid);
  const label = identity.name || identity.shortId || 'session';
  const text = `Prompt cache for ${label} expires in ${formatCacheCountdown(remaining)} — a follow-up now reuses it`;
  if (typeof showControlToast === 'function') showControlToast('info', text);
  if (typeof Notification !== 'undefined' && Notification.permission === 'granted' && document.hidden) {
    try { new Notification('Intendant', { body: text }); } catch (_) { /* notification blocked */ }
  }
}

function refreshSessionVitalsTicker() {
  let ticking = false;
  for (const [sid, win] of sessionWindows) {
    const vitals = (sessionMetadataById.get(sid) || {}).vitals || null;
    if (renderSessionWindowVitals(win, vitals)) ticking = true;
    maybeAlertCacheExpiry(sid, win, vitals);
  }
  if (ticking && !sessionVitalsTicker) {
    sessionVitalsTicker = setInterval(refreshSessionVitalsTicker, 1000);
  } else if (!ticking && sessionVitalsTicker) {
    clearInterval(sessionVitalsTicker);
    sessionVitalsTicker = null;
  }
}

function normalizeSessionVitals(raw) {
  if (!raw || typeof raw !== 'object') return null;
  const src = raw.vitals && typeof raw.vitals === 'object' ? raw.vitals : raw;
  const git = src.git && typeof src.git === 'object' ? src.git : null;
  const cache = src.cache && typeof src.cache === 'object' ? src.cache : null;
  const limits = Array.isArray(src.limits) ? src.limits : [];
  if (!git && !cache && !limits.length) return null;
  return { git, cache, limits };
}

function applySessionVitals(raw = {}) {
  const data = raw?.data && typeof raw.data === 'object' ? raw.data : raw;
  const sid = String(data?.session_id || data?.sessionId || '').trim();
  if (!sid) return;
  const vitals = normalizeSessionVitals(data);
  for (const id of sessionGoalUpdateIds(sid)) {
    const meta = { ...(sessionMetadataById.get(id) || {}), vitals };
    sessionMetadataById.set(id, meta);
    if (sessionWindows.has(id)) {
      renderSessionWindowVitals(sessionWindows.get(id), vitals);
    }
  }
  refreshSessionVitalsTicker();
}

function applySessionVitalsFromReplayEntries(entries) {
  if (!Array.isArray(entries)) return;
  for (const entry of entries) {
    if (entry?.event !== 'session_vitals') continue;
    applySessionVitals(entry);
  }
}

function sessionWindowMetaFromSession(session) {
  if (!session || typeof session !== 'object') return {};
  const meta = {
    name: session.name || session.display_name || session.thread_name,
    task: session.task,
    cwd: session.cwd || session.workdir || session.workDir,
    project_root: session.project_root,
    source: session.source,
    source_label: session.backend_source_label || session.source_label || prettyAgentName(session.backend_source || session.source || '') || session.backend_source || session.source,
    backend_source: session.backend_source,
    backend_session_id: session.backend_session_id,
    intendant_session_id: session.intendant_session_id,
    agent_command: session.agent_command || session.codex_command,
    codex_managed_context: session.codex_managed_context,
    codex_context_archive: session.codex_context_archive,
    capabilities: session.capabilities,
    parent_session_id: session.parent_session_id || session.parent_id,
    relationship_kind: session.relationship_kind || session.relationship,
    thread_source: session.thread_source,
    agent_nickname: session.agent_nickname,
  };
  if (
    Object.prototype.hasOwnProperty.call(session, 'goal') ||
    Object.prototype.hasOwnProperty.call(session, 'session_goal') ||
    Object.prototype.hasOwnProperty.call(session, 'sessionGoal')
  ) {
    meta.goal = session.goal ?? session.session_goal ?? session.sessionGoal;
  }
  return normalizeSessionWindowMeta(meta);
}

function applySessionRelationshipsFromSession(session) {
  if (!session || typeof session !== 'object') return;
  const relationships = [
    ...(Array.isArray(session.relationships) ? session.relationships : []),
    ...(Array.isArray(session.related_sessions) ? session.related_sessions : []),
  ];
  for (const rel of relationships) {
    if (!rel || typeof rel !== 'object') continue;
    applySessionRelationship(rel);
  }
}

function cacheSessionWindowMetadata(sessions) {
  if (!Array.isArray(sessions)) return;
  for (const session of sessions) {
    if (!session || typeof session !== 'object') continue;
    const meta = sessionWindowMetaFromSession(session);
    const ids = Array.from(new Set([
      session.session_id,
      session.resume_id,
      session.backend_session_id,
      session.intendant_session_id,
    ].map(id => String(id || '').trim()).filter(Boolean)));
    for (const id of ids) {
      sessionRowSeenIds.add(id);
      sessionRelationshipHydrationUnresolved.delete(id);
      const { meta: merged, changed } = mergeSessionWindowMetadata(id, meta);
      if (changed && sessionWindows.has(id)) updateSessionWindow(id, merged);
      if (changed) refreshSessionIdentityLabels(id);
    }
    if (session.backend_session_id && session.session_id) {
      applySessionIdentity({
        session_id: session.session_id,
        backend_session_id: session.backend_session_id,
        source: session.backend_source,
        backend_source_label: session.backend_source_label,
      });
    }
    if (!sessionMetadataStatusIsTerminal(session)) {
      const rawSource = String(session.source || '').trim().toLowerCase();
      const source = rawSource === 'intendant'
        ? 'intendant'
        : normalizeAgentId(session.source || session.backend_source || '');
      const backendSessionId = String(session.backend_session_id || '').trim();
      const intendantSessionId = String(session.intendant_session_id || '').trim();
      const sessionId = String(session.session_id || '').trim();
      if (source && source !== 'intendant' && intendantSessionId) {
        clearStaleSessionWindowDetached(sessionId, 'session metadata reports an attached wrapper');
      }
      if (source === 'intendant' && backendSessionId) {
        clearStaleSessionWindowDetached(backendSessionId, 'wrapper metadata reports this session is attached');
      }
    }
    scheduleExternalSessionWindowTranscriptSyncFromMetadata(session);
  }
  for (const session of sessions) {
    applySessionRelationshipsFromSession(session);
  }
  persistSessionWindowState();
}

function scheduleExternalSessionWindowTranscriptSyncFromMetadata(session) {
  if (!session || typeof session !== 'object' || sessionMetadataStatusIsTerminal(session)) return;
  const ids = new Set([
    session.session_id,
    session.resume_id,
    session.backend_session_id,
    session.backendSessionId,
    session.intendant_session_id,
    session.intendantSessionId,
  ].map(id => String(id || '').trim()).filter(Boolean));
  for (const id of ids) {
    const targetSid = sessionWindowTargetForLogSession(id) || id;
    if (!targetSid || !sessionWindows.has(targetSid)) continue;
    const win = sessionWindows.get(targetSid);
    const meta = sessionMetadataById.get(targetSid) || {};
    const source = externalSourceForSessionWindow(targetSid, win) ||
      normalizeAgentId(session.source || session.backend_source || session.backendSource || '');
    if (!source || source === 'intendant') continue;
    const status = String(session.status || meta.status || '').trim().toLowerCase();
    const goal = meta.goal || session.goal || session.session_goal || session.sessionGoal || null;
    const goalStatus = goal ? normalizeGoalStatus(goal.status || '') : '';
    const sessionComplete = status === 'complete' || status === 'completed';
    if (sessionComplete && goalStatus !== 'active') continue;
    if ((win?.ended || win?.phase === 'done') && goalStatus !== 'active') continue;
    scheduleExternalSessionWindowTranscriptSync(targetSid, 0);
  }
}

function sessionWindowMetadataRequestIds() {
  const ids = new Set();
  const add = value => {
    const id = String(value || '').trim();
    if (id) ids.add(id);
  };
  for (const [sid, win] of sessionWindows) {
    add(sid);
    const meta = sessionMetadataById.get(sid) || {};
    add(meta.backendSessionId);
    add(meta.intendantSessionId);
    for (const rel of sessionRelationships.values()) {
      if (rel.parentId === sid || rel.childId === sid) {
        // Negative-cached endpoints (no daemon row exists) stay out of the
        // periodic metadata poll — they made every 15s refresh a triple
        // store scan server-side.
        if (!sessionRelationshipHydrationUnresolved.has(rel.parentId)) add(rel.parentId);
        if (!sessionRelationshipHydrationUnresolved.has(rel.childId)) add(rel.childId);
      }
    }
    if (win?.source && sid) add(sid);
  }
  return Array.from(ids);
}

function sessionWindowMetadataUrl() {
  const ids = sessionWindowMetadataRequestIds();
  if (!ids.length) return '/api/sessions';
  return `/api/sessions?ids=${encodeURIComponent(ids.join(','))}`;
}

function readPersistedSessionWindowState() {
  try {
    const raw = localStorage.getItem(SESSION_WINDOW_STATE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed?.windows) ? parsed.windows : [];
  } catch (_) {
    return [];
  }
}

function sessionWindowPersistedSource(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  return normalizeAgentId(
    meta.backendSource ||
    meta.source ||
    meta.sourceLabel ||
    win?.source ||
    ''
  );
}

function persistedSessionWindowRecord(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  const source = sessionWindowPersistedSource(sid, win);
  if (!source) return null;
  const record = { session_id: sid, source };
  if (meta.sourceLabel) record.source_label = meta.sourceLabel;
  if (meta.backendSource) record.backend_source = meta.backendSource;
  if (meta.backendSessionId) record.backend_session_id = meta.backendSessionId;
  if (meta.intendantSessionId) record.intendant_session_id = meta.intendantSessionId;
  if (meta.agentCommand) record.agent_command = meta.agentCommand;
  if (meta.codexManagedContext) record.codex_managed_context = meta.codexManagedContext;
  if (meta.codexContextArchive) record.codex_context_archive = meta.codexContextArchive;
  if (meta.projectRoot) record.project_root = meta.projectRoot;
  if (meta.cwd) record.cwd = meta.cwd;
  if (meta.parentId && meta.relationshipKind) {
    record.parent_session_id = meta.parentId;
    record.relationship = meta.relationshipKind;
    record.ephemeral = !!meta.relationshipEphemeral;
  }
  return record;
}

function persistSessionWindowState() {
  if (restoringPersistedSessionWindows) return;
  let windows = [];
  for (const [sid, win] of sessionWindows) {
    const record = persistedSessionWindowRecord(sid, win);
    if (record) windows.push(record);
  }
  if (windows.length > SESSION_WINDOW_RESTORE_LIMIT) {
    const preferredIds = new Set([
      resolvePromptTargetSessionId(),
      currentSessionFullId,
      foregroundSessionFullId,
    ].filter(Boolean));
    const preferred = windows.filter(record => preferredIds.has(record.session_id));
    const rest = windows.filter(record => !preferredIds.has(record.session_id));
    windows = [...preferred, ...rest].slice(0, SESSION_WINDOW_RESTORE_LIMIT);
  }
  try {
    if (windows.length) {
      localStorage.setItem(SESSION_WINDOW_STATE_KEY, JSON.stringify({ windows }));
    } else {
      localStorage.removeItem(SESSION_WINDOW_STATE_KEY);
    }
  } catch (_) {}
}

function normalizePersistedSessionWindowRecord(record = {}) {
  const sessionId = String(record?.session_id || record?.sessionId || record?.id || '').trim();
  if (!sessionId) return null;
  const meta = normalizeSessionWindowMeta(record);
  const source = normalizeAgentId(record?.source || record?.backendSource || record?.backend_source || meta.backendSource || meta.source || '');
  if (!source) return null;
  meta.source = source;
  if (!meta.sourceLabel) meta.sourceLabel = prettyAgentName(source);
  return { sessionId, source, meta };
}

function restorePersistedSessionWindowsSoon(delay = 1200) {
  if (restoredPersistedSessionWindows) return;
  restoredPersistedSessionWindows = true;
  setTimeout(() => { restorePersistedSessionWindows(); }, delay);
}

async function restorePersistedSessionWindows() {
  const records = readPersistedSessionWindowState()
    .map(normalizePersistedSessionWindowRecord)
    .filter(Boolean)
    .slice(0, SESSION_WINDOW_RESTORE_LIMIT);
  if (!records.length) return;
  restoringPersistedSessionWindows = true;
  try {
    for (const record of records) {
      sessionMetadataById.set(record.sessionId, {
        ...(sessionMetadataById.get(record.sessionId) || {}),
        ...record.meta,
      });
    }
    for (const record of records) {
      if (record.meta.parentId && record.meta.relationshipKind) {
        applySessionRelationship({
          parent_session_id: record.meta.parentId,
          child_session_id: record.sessionId,
          relationship: record.meta.relationshipKind,
          ephemeral: !!record.meta.relationshipEphemeral,
        });
      }
    }
    await Promise.all(records.map(record => restorePersistedSessionWindow(record)));
  } finally {
    restoringPersistedSessionWindows = false;
    persistSessionWindowState();
    stationScheduleUpdate();
  }
}

function sessionWindowRecordFromReplayEntry(entry = {}, fallbackSessionId = '') {
  const event = String(entry.event || '').trim();
  const sessionId = String(entry.session_id || entry.sessionId || fallbackSessionId || '').trim();
  const base = {
    ...entry,
    session_id: sessionId,
    ts: entry.ts || entry.timestamp || '',
    ts_ms: entry.ts_ms ?? entry.tsMs,
    event_id: entry.event_id || entry.eventId || '',
    delivery: entry.delivery || entry.delivery_class || entry.deliveryClass || '',
    turn_id: entry.turn_id || entry.turnId || '',
    item_type: entry.item_type || entry.itemType || '',
    command_item_id: entry.command_item_id || entry.commandItemId || '',
    thread_item: entry.thread_item || entry.threadItem || null,
    thread_history_change: entry.thread_history_change || entry.threadHistoryChange || null,
    changed_items: entry.changed_items || entry.changedItems || [],
    changed_turns: entry.changed_turns || entry.changedTurns || [],
    removed_turn_ids: entry.removed_turn_ids || entry.removedTurnIds || [],
    command_execution: entry.command_execution || entry.commandExecution || null,
    user_turn_index: entry.user_turn_index ?? entry.userTurnIndex,
    user_turn_revision: entry.user_turn_revision ?? entry.userTurnRevision,
    replacement_for_user_turn_index: entry.replacement_for_user_turn_index ?? entry.replacementForUserTurnIndex,
    superseded: !!entry.superseded,
    superseded_reason: entry.superseded_reason || entry.supersededReason || '',
    full_output_available: !!(entry.full_output_available ?? entry.fullOutputAvailable),
    full_output_bytes: entry.full_output_bytes ?? entry.fullOutputBytes,
    full_output_lines: entry.full_output_lines ?? entry.fullOutputLines,
    text_truncated: !!(entry.text_truncated ?? entry.textTruncated),
    truncated_fields: entry.truncated_fields || entry.truncatedFields || [],
    output_session_id: entry.output_session_id || entry.outputSessionId || entry.session_id || entry.sessionId || fallbackSessionId,
    output_source: entry.output_source || entry.outputSource || entry.source || '',
  };
  let content = String(entry.content || entry.summary || entry.message || '').trim();
  let level = String(entry.level || '').trim();
  let source = String(entry.source || '').trim();
  let kind = entry.kind || '';

  if (event === 'replay_start' || event === 'context_snapshot') return null;
  if (event === 'log_entry' || !event) {
    if (!content) return null;
    return { ...base, level: level || 'info', source: source || 'system', content, kind, output_id: entry.output_id || entry.outputId || '' };
  }
  if (event === 'model_response') {
    if (!content) {
      const reasoning = String(entry.reasoning_summary || entry.reasoningSummary || '').trim();
      if (!reasoning) return null;
      content = `Reasoning: ${reasoning}`;
      level = 'detail';
    } else {
      level = 'model';
    }
    return { ...base, level, source: source || 'model', content, kind };
  }
  if (event === 'agent_started') {
    content = String(entry.commands_preview || entry.commandsPreview || '').trim();
    if (!content) return null;
    return { ...base, level: 'agent', source: source || 'agent', content, item_id: entry.item_id || entry.itemId || '', kind };
  }
  if (event === 'agent_output') {
    const stdout = String(entry.stdout || '').trim();
    const stderr = String(entry.stderr || '').trim();
    content = stdout || stderr;
    if (!content) return null;
    return { ...base, level: stdout ? 'agent' : 'warn', source: source || 'agent', content, kind: kind || 'agent_output', output_id: entry.output_id || entry.outputId || '' };
  }
  if (event === 'presence_log') {
    if (!content) return null;
    return { ...base, level: level || 'info', source: 'presence', content, kind };
  }
  if (event === 'turn_started') {
    return { ...base, level: 'info', source: 'system', content: `Turn ${entry.turn || 0} started`, kind };
  }
  if (event === 'round_complete') {
    return { ...base, level: 'info', source: 'system', content: `Round ${entry.round || 0} complete (${entry.turns_in_round || entry.turnsInRound || 0} turns)`, kind };
  }
  if (event === 'done_signal') {
    content = content ? `Done signal: ${content}` : 'Done signal';
    return { ...base, level: 'detail', source: source || 'worker', content, kind };
  }
  if (event === 'session_started') {
    return { ...base, level: 'info', source: 'system', content: `Session started: ${sessionId}`, kind };
  }
  if (event === 'session_attached') {
    return { ...base, level: 'info', source: 'system', content: `Session attached: ${sessionId} (${source || entry.backend_source || ''})`, kind };
  }
  if (event === 'session_relationship') {
    const parent = String(entry.parent_session_id || entry.parentSessionId || '').trim();
    const child = String(entry.child_session_id || entry.childSessionId || '').trim();
    const relationship = String(entry.relationship || '').trim().toLowerCase() || 'relationship';
    const label = relationship === 'fork' ? 'Fork' : (relationship === 'side' ? 'Side thread' : (relationship === 'subagent' ? 'Subagent' : 'Session relationship'));
    return {
      ...base,
      session_id: child || parent || sessionId,
      level: 'info',
      source: 'session',
      content: `${label}: ${parent ? shortSessionId(parent) : 'unknown'} -> ${child ? shortSessionId(child) : 'unknown'}${entry.ephemeral ? ' (ephemeral)' : ''}`,
      kind,
    };
  }
  if (event === 'codex_thread_action_result') {
    const action = String(entry.action || '').trim().toLowerCase().replace(/_/g, '-') || 'action';
    const message = String(entry.message || '').trim();
    const success = entry.success !== false;
    return {
      ...base,
      level: success ? 'info' : 'error',
      source: 'codex',
      content: `/${action} ${success ? 'ok' : 'failed'}${sessionId ? ` for ${shortSessionId(sessionId)}` : ''}${message ? `: ${message}` : ''}`,
      kind,
    };
  }
  if (event === 'session_ended') {
    const reason = String(entry.reason || '').trim();
    return { ...base, level: 'info', source: 'system', content: `Session ended: ${sessionId}${reason ? ` - ${reason}` : ''}`, kind };
  }
  return null;
}

function renderRestoredSessionWindowEntries(win, entries, fallbackSessionId) {
  if (!win || !Array.isArray(entries)) return 0;
  const records = entries
    .map(entry => sessionWindowRecordFromReplayEntry(entry, fallbackSessionId))
    .filter(Boolean);
  if (!records.length) return 0;
  resetSessionWindowLog(win);
  appendSessionWindowHistoryBatch(win, records, true);
  return records.length;
}

function updateSessionWindowRemotePageState(win, data = {}, source = '', sessionId = '') {
  if (!win || !data || data.error) return;
  const pageStart = Number(data.page_start ?? data.pageStart);
  const pageEnd = Number(data.page_end ?? data.pageEnd);
  const totalEntries = Number(data.total_entries ?? data.totalEntries);
  if (!Number.isFinite(pageStart) || pageStart < 0) return;
  win.remoteSource = normalizeAgentId(source || win.remoteSource || win.source || '');
  win.remoteSessionId = String(sessionId || win.remoteSessionId || win.sessionId || '').trim();
  win.remotePageStart = Number.isFinite(win.remotePageStart)
    ? Math.min(win.remotePageStart, pageStart)
    : pageStart;
  if (Number.isFinite(pageEnd) && pageEnd >= 0) {
    win.remotePageEnd = Number.isFinite(win.remotePageEnd)
      ? Math.max(win.remotePageEnd, pageEnd)
      : pageEnd;
  }
  if (Number.isFinite(totalEntries) && totalEntries >= 0) {
    win.remoteTotalEntries = totalEntries;
  }
  win.remoteHasOlder = win.remotePageStart > 0;
}

function sessionWindowTranscriptSignatureContent(value) {
  return compactSessionTextBounded(value, SESSION_TEXT_SIGNATURE_CHAR_LIMIT, { signature: true });
}

function sessionWindowTranscriptRenderedContentForRecord(record = {}) {
  const raw = String(record.content || record.summary || record.message || '');
  if (!raw) return '';
  if (raw.length > SESSION_RENDERED_SIGNATURE_CHAR_LIMIT || isSessionWindowCommandOutputRecord(record)) {
    return '';
  }
  try {
    const cnt = document.createElement('span');
    cnt.className = 'log-content';
    renderLogContentElement(cnt, record);
    return sessionWindowTranscriptContentForElement(cnt);
  } catch {
    return '';
  }
}

function sessionWindowTranscriptContentAliasesForRecord(record = {}) {
  const aliases = [];
  const seen = new Set();
  const add = (value) => {
    const content = sessionWindowTranscriptSignatureContent(value);
    if (!content || seen.has(content)) return;
    seen.add(content);
    aliases.push(content);
  };
  add(record.content || record.summary || record.message || '');
  add(sessionWindowTranscriptRenderedContentForRecord(record));
  return aliases;
}

function sessionWindowTranscriptSignatureParts(record = {}, fallbackSessionId = '') {
  const sessionId = String(record.session_id || record.sessionId || fallbackSessionId || '').trim();
  const contentAliases = sessionWindowTranscriptContentAliasesForRecord(record);
  const content = contentAliases[0] || '';
  const eventId = String(record.event_id || record.eventId || '').trim();
  const itemId = String(record.item_id || record.itemId || '').trim();
  const outputId = String(record.output_id || record.outputId || '').trim();
  const commandItemId = String(record.command_item_id || record.commandItemId || record.command_execution?.id || record.commandExecution?.id || '').trim();
  const turnId = String(record.turn_id || record.turnId || '').trim();
  const itemType = String(record.item_type || record.itemType || record.thread_item?.type || record.threadItem?.type || '').trim().toLowerCase();
  if (!sessionId || (!content && !eventId && !itemId && !outputId && !commandItemId)) return null;
  return {
    sessionId,
    ts: String(record.ts || record.timestamp || '').trim(),
    tsMs: record.ts_ms ?? record.tsMs,
    level: String(record.level || '').trim().toLowerCase(),
    source: String(record.source || '').trim().toLowerCase(),
    kind: String(record.kind || '').trim().toLowerCase(),
    eventId,
    itemId,
    outputId,
    commandItemId,
    turnId,
    itemType,
    userTurnIndex: record.user_turn_index ?? record.userTurnIndex ?? '',
    userTurnRevision: record.user_turn_revision ?? record.userTurnRevision ?? '',
    replacementTurnIndex: record.replacement_for_user_turn_index ?? record.replacementForUserTurnIndex ?? '',
    content,
    contentAliases,
  };
}

function sessionWindowTranscriptSignaturesFromParts(parts, options = {}) {
  if (!parts) return [];
  const signatures = [];
  if (parts.eventId) signatures.push(['event', parts.sessionId, parts.eventId].join('\u001f'));
  if (parts.commandItemId) signatures.push(['command-item', parts.sessionId, parts.commandItemId].join('\u001f'));
  if (parts.itemId) signatures.push(['item', parts.sessionId, parts.itemId].join('\u001f'));
  if (parts.outputId) signatures.push(['output', parts.sessionId, parts.kind, parts.outputId].join('\u001f'));
  if (parts.turnId && parts.itemType && parts.itemId) {
    signatures.push(['thread-item', parts.sessionId, parts.turnId, parts.itemType, parts.itemId].join('\u001f'));
  }
  const hasUserTurn = String(parts.userTurnIndex || '').trim() !== '';
  const isUserMessage = parts.source === 'user' || parts.level === 'user';
  const canUseTextSignature = parts.level === 'model' ||
    parts.level === 'detail' ||
    parts.kind === 'agent_output' ||
    (isUserMessage && hasUserTurn);
  const contentAliases = Array.isArray(parts.contentAliases) && parts.contentAliases.length
    ? parts.contentAliases
    : [parts.content];
  for (const content of contentAliases) {
    const textParts = [
      parts.sessionId,
      parts.level,
      parts.source,
      parts.kind,
      parts.userTurnIndex,
      parts.userTurnRevision,
      parts.replacementTurnIndex,
      content,
    ];
    if (canUseTextSignature && (options.includeText !== false || !parts.ts)) {
      signatures.push(['text', ...textParts].join('\u001f'));
    }
    if (parts.ts) signatures.push(['exact', parts.ts, ...textParts].join('\u001f'));
    if (parts.tsMs !== undefined && parts.tsMs !== null && parts.tsMs !== '') {
      signatures.push(['exact-ms', String(parts.tsMs), ...textParts].join('\u001f'));
    }
    if (isUserMessage && options.includeUserNearTime) {
      const timeBucket = sessionWindowTranscriptTimeBucket(parts.ts);
      if (timeBucket) {
        signatures.push([
          'user-near-time-text',
          parts.sessionId,
          parts.level,
          parts.source,
          parts.kind,
          parts.replacementTurnIndex,
          timeBucket,
          content,
        ].join('\u001f'));
      }
    }
  }
  return signatures;
}

function sessionWindowTranscriptSignaturesForRecord(record = {}, fallbackSessionId = '', options = {}) {
  return sessionWindowTranscriptSignaturesFromParts(
    sessionWindowTranscriptSignatureParts(record, fallbackSessionId),
    options
  );
}

function sessionWindowTranscriptContentForElement(element) {
  if (!element) return '';
  const clone = element.cloneNode(true);
  clone.querySelectorAll?.('.log-state-badges').forEach(el => el.remove());
  return clone.textContent || '';
}

function sessionWindowTranscriptContentForNode(node) {
  if (!node) return '';
  const contentEl = node.querySelector?.('.log-content');
  if (!contentEl) return node.textContent || '';
  return sessionWindowTranscriptContentForElement(contentEl);
}

function sessionWindowTranscriptSignaturesForNode(node, fallbackSessionId = '') {
  if (!node) return [];
  const isCommandOutput = !!node.dataset?.outputId || node.dataset?.kind === 'agent_output';
  const content = isCommandOutput
    ? (node.querySelector?.('.command-output-summary')?.textContent || '')
    : sessionWindowTranscriptContentForNode(node);
  const parts = sessionWindowTranscriptSignatureParts({
    session_id: node.dataset?.sessionId || fallbackSessionId,
    ts: node.querySelector?.('.log-ts')?.title || node.querySelector?.('.log-ts')?.textContent || '',
    ts_ms: node.dataset?.tsMs || '',
    level: node.dataset?.level || '',
    source: node.querySelector?.('.log-level')?.textContent || '',
    kind: node.dataset?.kind || '',
    event_id: node.dataset?.eventId || '',
    item_id: node.dataset?.itemId || '',
    output_id: node.dataset?.outputId || '',
    command_item_id: node.dataset?.commandItemId || '',
    turn_id: node.dataset?.turnId || '',
    item_type: node.dataset?.itemType || '',
    user_turn_index: node.dataset?.userTurnIndex,
    user_turn_revision: node.dataset?.userTurnRevision,
    replacement_for_user_turn_index: node.dataset?.replacementForUserTurnIndex,
    content,
  }, fallbackSessionId);
  return sessionWindowTranscriptSignaturesFromParts(parts);
}

function sessionWindowTranscriptSignaturesForHistoryItem(item, fallbackSessionId = '', options = {}) {
  const node = sessionWindowHistoryNode(item);
  if (node) return sessionWindowTranscriptSignaturesForNode(node, fallbackSessionId);
  const record = sessionWindowHistoryRecord(item);
  return record ? sessionWindowTranscriptSignaturesForRecord(record, fallbackSessionId, options) : [];
}

function findSessionWindowHistoryIndexBySignatures(history, signatures, fallbackSessionId = '') {
  if (!Array.isArray(history) || !Array.isArray(signatures) || signatures.length === 0) return -1;
  const wanted = new Set(signatures);
  for (let i = 0; i < history.length; i++) {
    const itemSignatures = sessionWindowTranscriptSignaturesForHistoryItem(history[i], fallbackSessionId);
    if (itemSignatures.some(signature => wanted.has(signature))) return i;
  }
  return -1;
}

function sessionWindowTranscriptTimestampMs(value) {
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  const raw = String(value || '').trim();
  if (!raw) return null;
  if (/^\d+$/.test(raw)) {
    const numeric = Number(raw);
    if (Number.isFinite(numeric) && numeric > 0) return numeric;
  }
  const parenthetical = raw.match(/\(([^()]+)\)\s*$/);
  if (parenthetical) {
    const parsed = Date.parse(parenthetical[1]);
    if (Number.isFinite(parsed)) return parsed;
  }
  const direct = Date.parse(raw);
  if (Number.isFinite(direct)) return direct;
  let match = raw.match(/^(\d{1,2}):(\d{2})(?::(\d{2}))?$/);
  if (match) {
    const date = new Date();
    date.setHours(Number(match[1]), Number(match[2]), Number(match[3] || 0), 0);
    return date.getTime();
  }
  match = raw.match(/^(\d{1,2})-(\d{1,2})\s+(\d{1,2}):(\d{2})(?::(\d{2}))?$/);
  if (match) {
    const date = new Date();
    date.setMonth(Number(match[1]) - 1, Number(match[2]));
    date.setHours(Number(match[3]), Number(match[4]), Number(match[5] || 0), 0);
    return date.getTime();
  }
  return null;
}

function sessionWindowTranscriptTimestampForHistoryItem(item) {
  const node = sessionWindowHistoryNode(item);
  if (node) {
    if (node.dataset?.tsMs) return sessionWindowTranscriptTimestampMs(node.dataset.tsMs);
    const ts = node.querySelector?.('.log-ts');
    return sessionWindowTranscriptTimestampMs(ts?.title || ts?.textContent || '');
  }
  const record = sessionWindowHistoryRecord(item);
  return sessionWindowTranscriptTimestampMs(record?.ts_ms ?? record?.tsMs ?? record?.ts ?? record?.timestamp ?? '');
}

function sessionWindowTranscriptTimeBucket(value) {
  const ms = sessionWindowTranscriptTimestampMs(value);
  if (ms === null) return '';
  return String(Math.floor(ms / 5000));
}

function insertSessionWindowHistoryRecords(win, records, shouldFollow) {
  if (!win || !Array.isArray(records) || records.length === 0) return;
  deduplicateSessionWindowHistory(win, shouldFollow);
  const history = ensureSessionWindowHistory(win);
  const wasRenderingTail = sessionWindowIsRenderingTail(win, history.length);
  const signatures = sessionWindowHistorySignatureSet(win, win.sessionId);
  let inserted = 0;
  for (const record of records) {
    const item = retargetSessionWindowHistoryItem(
      prepareSessionWindowHistoryItem(record),
      win.sessionId
    );
    if (sessionWindowHistoryHasMatchingSignature(signatures, item, win.sessionId)) continue;
    stationTrackSessionWindowHistoryAnchor(item, win.sessionId);
    const recordTs = sessionWindowTranscriptTimestampMs(record.ts_ms ?? record.tsMs ?? record.ts ?? record.timestamp ?? '');
    let index = history.length;
    if (recordTs !== null) {
      for (let i = 0; i < history.length; i++) {
        const itemTs = sessionWindowTranscriptTimestampForHistoryItem(history[i]);
        if (itemTs !== null && itemTs > recordTs) {
          index = i;
          break;
        }
      }
    }
    history.splice(index, 0, item);
    addSessionWindowHistorySignatures(signatures, item, win.sessionId);
    inserted += 1;
  }
  if (inserted === 0) return;
  deduplicateSessionWindowHistory(win, shouldFollow);
  if (shouldFollow || wasRenderingTail) {
    renderSessionWindowTail(win);
  } else {
    renderSessionWindowRange(win, win.renderStart);
  }
  applySessionWindowOutputScroll(win, shouldFollow);
}

function appendMissingRestoredSessionWindowEntries(win, entries, fallbackSessionId) {
  if (!win || !Array.isArray(entries)) return 0;
  const records = entries
    .map(entry => sessionWindowRecordFromReplayEntry(entry, fallbackSessionId))
    .filter(Boolean);
  if (!records.length) return 0;
  if (!sessionWindowHasStreamedHistory(win)) {
    resetSessionWindowLog(win);
    appendSessionWindowHistoryBatch(win, records, true);
    return records.length;
  }
  const removedDuplicates = deduplicateSessionWindowHistory(
    win,
    sessionWindowShouldFollowNextOutput(win)
  );
  const existing = new Set();
  for (const item of ensureSessionWindowHistory(win)) {
    for (const signature of sessionWindowTranscriptSignaturesForHistoryItem(item, fallbackSessionId)) {
      existing.add(signature);
    }
  }
  const missing = [];
  for (const record of records) {
    const signatures = sessionWindowTranscriptSignaturesForRecord(record, fallbackSessionId);
    if (signatures.length > 0 && signatures.some(signature => existing.has(signature))) continue;
    missing.push(record);
    for (const signature of signatures) {
      existing.add(signature);
    }
  }
  if (!missing.length) return removedDuplicates;
  insertSessionWindowHistoryRecords(win, missing, sessionWindowShouldFollowNextOutput(win));
  return missing.length + removedDuplicates;
}

function externalSessionWindowSyncRecord(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const targetSid = sessionWindowTargetForLogSession(sid) || sid;
  const win = sessionWindows.get(targetSid) || sessionWindows.get(sid) || null;
  if (!win) return null;
  const source = externalSourceForSessionWindow(targetSid, win) ||
    externalSourceForSessionWindow(sid, win);
  if (!source || source === 'intendant') return null;
  return { sessionId: targetSid, source, win };
}

function scheduleExternalSessionWindowTranscriptSync(sessionId, delay = 0) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const targetSid = sessionWindowTargetForLogSession(sid) || sid;
  if (!targetSid) return;
  if (externalSessionWindowSyncTimers.has(targetSid)) {
    clearTimeout(externalSessionWindowSyncTimers.get(targetSid));
  }
  const timeout = setTimeout(() => {
    externalSessionWindowSyncTimers.delete(targetSid);
    syncExternalSessionWindowTranscript(targetSid);
  }, Math.max(0, Number(delay) || 0));
  externalSessionWindowSyncTimers.set(targetSid, timeout);
}

async function syncExternalSessionWindowTranscript(sessionId) {
  const record = externalSessionWindowSyncRecord(sessionId);
  if (!record) return;
  const key = `${record.source}:${record.sessionId}`;
  if (externalSessionWindowSyncInFlight.has(key)) return;
  const now = Date.now();
  const lastAt = externalSessionWindowSyncLastAt.get(key) || 0;
  if (now - lastAt < EXTERNAL_SESSION_WINDOW_SYNC_COOLDOWN_MS) {
    scheduleExternalSessionWindowTranscriptSync(
      record.sessionId,
      EXTERNAL_SESSION_WINDOW_SYNC_COOLDOWN_MS - (now - lastAt)
    );
    return;
  }
  externalSessionWindowSyncInFlight.add(key);
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10000);
  try {
    let data = null;
    let lastError = null;
    const sources = record.source === 'intendant' ? [record.source] : [record.source, 'intendant'];
    for (const source of sources) {
      const candidate = await fetchSessionDetailPayload(record.sessionId, {
        source,
        limit: SESSION_WINDOW_RESTORE_LOG_LIMIT,
        signal: controller.signal,
        cache: 'no-store',
      });
      if (!candidate.error) {
        data = candidate;
        break;
      }
      lastError = new Error(candidate.error || 'session detail fetch failed');
      if (!sessionDetailErrorIsMissing(candidate)) break;
    }
    if (!data) throw lastError || new Error('session detail fetch failed');
    const entries = Array.isArray(data.entries) ? data.entries : [];
    if (!entries.length) return;
    applySessionIdentitiesFromReplayEntries(entries);
    applyExternalIdentitiesFromLogEntries(entries);
    applySessionGoalsFromReplayEntries(entries);
    const targetSid = sessionWindowTargetForLogSession(record.sessionId) || record.sessionId;
    const targetWin = sessionWindows.get(targetSid) || record.win;
    updateSessionWindowRemotePageState(targetWin, data, record.source, record.sessionId);
    const rendered = appendMissingRestoredSessionWindowEntries(targetWin, entries, targetSid);
    if (rendered > 0) stationScheduleUpdate();
  } catch (err) {
    console.warn('Failed to sync external session window transcript', record.sessionId, err);
  } finally {
    clearTimeout(timeout);
    externalSessionWindowSyncLastAt.set(key, Date.now());
    externalSessionWindowSyncInFlight.delete(key);
  }
}

function sessionWindowHasStreamedHistory(win) {
  return ensureSessionWindowHistory(win).some(item => {
    const node = sessionWindowHistoryNode(item);
    if (node) return !node.classList.contains('session-window-empty');
    const record = sessionWindowHistoryRecord(item);
    if (!record) return false;
    return String(record.content || '').trim() || record.kind === 'rollback_marker';
  });
}

async function hydrateRestoredSessionWindow(win, record) {
  const sid = String(record?.sessionId || win?.sessionId || '').trim();
  const source = normalizeAgentId(record?.source || win?.source || '');
  if (!sid || !source || !win || ensureSessionWindowHistory(win).length > 0) return;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10000);
  try {
    let lastError = null;
    let data = null;
    const sources = source === 'intendant' ? [source] : [source, 'intendant'];
    for (const candidateSource of sources) {
      const candidate = await fetchSessionDetailPayload(sid, {
        source: candidateSource,
        limit: SESSION_WINDOW_RESTORE_LOG_LIMIT,
        signal: controller.signal,
        cache: 'no-store',
      });
      if (!candidate.error) {
        data = candidate;
        break;
      }
      lastError = new Error(candidate.error || 'session detail fetch failed');
      if (!sessionDetailErrorIsMissing(candidate)) break;
    }
    if (!data) throw lastError || new Error('session detail fetch failed');
    const entries = Array.isArray(data.entries) ? data.entries : [];
    applySessionIdentitiesFromReplayEntries(entries);
    applyExternalIdentitiesFromLogEntries(entries);
    applySessionGoalsFromReplayEntries(entries);
    const targetSid = sessionWindowTargetForLogSession(sid) || sid;
    const targetWin = sessionWindows.get(targetSid) || (win.el?.isConnected ? win : null);
    updateSessionWindowRemotePageState(targetWin, data, source, sid);
    const rendered = renderRestoredSessionWindowEntries(targetWin, entries, targetSid);
    if (rendered > 0) {
      updateSessionWindow(targetSid, { phase: 'idle', ended: false });
      stationScheduleUpdate();
    }
  } catch (err) {
    console.warn('Failed to hydrate restored session window', sid, err);
  } finally {
    clearTimeout(timeout);
  }
}

function sessionWindowHydrationRecord(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const win = sessionWindows.get(sid) || null;
  const meta = sessionMetadataById.get(sid) || {};
  let source = normalizeAgentId(
    meta.backendSource ||
    meta.backend_source ||
    meta.source ||
    meta.sourceLabel ||
    win?.source ||
    ''
  );
  const backendSessionId = String(meta.backendSessionId || meta.backend_session_id || '').trim();
  if (backendSessionId && backendSessionId !== sid) source = 'intendant';
  if (!source) return null;
  return { sessionId: sid, source, meta };
}

async function hydrateSessionWindowIfEmpty(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!sid || !win || ensureSessionWindowHistory(win).length > 0) return;
  if (sessionWindowRestoreIsInFlightFor(sid)) return;
  sessionWindowRestoreInFlight.add(sid);
  try {
    let record = sessionWindowHydrationRecord(sid);
    if (!record) {
      const resp = await dashboardJsonFetch('api_sessions', { ids: [sid] }, () => fetch(`/api/sessions?ids=${encodeURIComponent(sid)}`, { cache: 'no-store' }), 'api_sessions_ids');
      if (resp.ok) {
        const sessions = await resp.json();
        cacheSessionWindowMetadata(sessions);
      }
      record = sessionWindowHydrationRecord(sid);
    }
    if (record) await hydrateRestoredSessionWindow(win, record);
  } catch (err) {
    console.warn('Failed to hydrate session window', sid, err);
  } finally {
    sessionWindowRestoreInFlight.delete(sid);
  }
}

async function restorePersistedSessionWindow(record) {
  const sid = String(record?.sessionId || '').trim();
  const source = normalizeAgentId(record?.source || '');
  if (!sid || !source || sessionWindowRestoreIsInFlightFor(sid)) return;
  const existing = sessionWindows.get(sid);
  if (existing && ensureSessionWindowHistory(existing).length > 0) return;
  sessionWindowRestoreInFlight.add(sid);
  try {
    const win = ensureSessionWindow(sid, {
      ...(record.meta || {}),
      source,
      source_label: prettyAgentName(source),
      phase: 'idle',
      ended: false,
    });
    if (!win) return;
    setSessionWindowDetached(sid, true, 'restored from browser state');
    updateSessionWindow(sid, {
      source,
      source_label: prettyAgentName(source),
      phase: 'idle',
      ended: false,
    });
    stationScheduleUpdate();
    await hydrateRestoredSessionWindow(win, record);
  } catch (err) {
    console.warn('Failed to restore session window', sid, err);
  } finally {
    sessionWindowRestoreInFlight.delete(sid);
  }
}

function shouldPollSessionWindowMetadata() {
  return sessionWindows.size > 0 && activeTab === 'activity' && !document.hidden;
}

function refreshSessionWindowMetadata(delay = 0, options = {}) {
  const run = () => {
    if (!options.force && !shouldPollSessionWindowMetadata()) return null;
    if (sessionMetadataRefreshInFlight) return sessionMetadataRefreshInFlight;
    const ids = sessionWindowMetadataRequestIds();
    const url = sessionWindowMetadataUrl();
    const loadMetadata = async () => {
      if (dashboardTransport?.canUseRpc()) {
        try {
          return await dashboardTransport.request('api_sessions', ids.length ? { ids } : { limit: 'all' });
        } catch (err) {
          console.warn('[dashboard-control] api_sessions metadata RPC failed, falling back to HTTP', err);
        }
      }
      const r = await authedFetch(url);
      if (!r.ok) throw new Error(`${url} returned ${r.status}`);
      return r.json();
    };
    sessionMetadataRefreshInFlight = loadMetadata()
      .then(sessions => cacheSessionWindowMetadata(sessions))
      .catch(() => {})
      .finally(() => {
        sessionMetadataRefreshInFlight = null;
      });
    return sessionMetadataRefreshInFlight;
  };
  if (delay > 0) setTimeout(run, delay);
  else run();
}

function syncSessionWindowMetadataRefresh() {
  if (shouldPollSessionWindowMetadata()) {
    if (!sessionMetadataRefreshTimer) {
      refreshSessionWindowMetadata();
      sessionMetadataRefreshTimer = setInterval(() => {
        if (!shouldPollSessionWindowMetadata()) {
          syncSessionWindowMetadataRefresh();
          return;
        }
        refreshSessionWindowMetadata();
      }, SESSION_METADATA_REFRESH_MS);
    }
    return;
  }
  if (sessionMetadataRefreshTimer) {
    clearInterval(sessionMetadataRefreshTimer);
    sessionMetadataRefreshTimer = null;
  }
}

document.addEventListener('visibilitychange', syncSessionWindowMetadataRefresh);

function readStoredSessionWindowGridHeight() {
  try {
    const raw = localStorage.getItem(SESSION_WINDOW_GRID_HEIGHT_KEY);
    const n = Number(raw);
    return Number.isFinite(n) && n > 0 ? n : null;
  } catch {
    return null;
  }
}

function storeSessionWindowGridHeight(heightPx) {
  try {
    if (heightPx) localStorage.setItem(SESSION_WINDOW_GRID_HEIGHT_KEY, String(Math.round(heightPx)));
    else localStorage.removeItem(SESSION_WINDOW_GRID_HEIGHT_KEY);
  } catch {}
}

function readStoredConcurrentLogMode() {
  try {
    const mode = localStorage.getItem(CONCURRENT_LOG_MODE_KEY);
    return [CONCURRENT_LOG_MODE_NORMAL, CONCURRENT_LOG_MODE_MINIMIZED, CONCURRENT_LOG_MODE_MAXIMIZED].includes(mode)
      ? mode
      : CONCURRENT_LOG_MODE_NORMAL;
  } catch {
    return CONCURRENT_LOG_MODE_NORMAL;
  }
}

function storeConcurrentLogMode(mode) {
  try {
    if (mode === CONCURRENT_LOG_MODE_NORMAL) localStorage.removeItem(CONCURRENT_LOG_MODE_KEY);
    else localStorage.setItem(CONCURRENT_LOG_MODE_KEY, mode);
  } catch {}
}

function mainLogContainers() {
  const containers = [];
  if (concurrentLogDetachedFragment) containers.push(concurrentLogDetachedFragment);
  const stream = document.getElementById('log-stream');
  if (stream) containers.push(stream);
  return containers;
}

function currentMainLogContainer() {
  return concurrentLogDetachedFragment || document.getElementById('log-stream');
}

function clearMainLogContainers() {
  const stream = document.getElementById('log-stream');
  if (stream) stream.replaceChildren();
  if (concurrentLogDetachedFragment) concurrentLogDetachedFragment.replaceChildren();
}

function pruneMainLogContainer(container = currentMainLogContainer()) {
  if (!container) return;
  const removeExcess = () => {
    while (logEntryCount > 10000) {
      const first = container.querySelector('.log-entry, .log-turn-sep');
      if (!first) break;
      first.remove();
      logEntryCount--;
    }
  };
  // When the user is scrolled up in the live stream, pruning from the top
  // must not drag their view — compensate scrollTop for the removed height.
  const scroller = container.nodeType === 1 && container.id === 'log-stream' && !autoScroll
    ? container
    : null;
  if (scroller) {
    adjustScrollForRemovedAbove(scroller, removeExcess);
  } else {
    removeExcess();
  }
}

function shouldDetachConcurrentLogStream() {
  const grid = document.getElementById('session-window-grid');
  return !!grid
    && !grid.classList.contains('hidden')
    && concurrentLogMode === CONCURRENT_LOG_MODE_MINIMIZED;
}

function detachConcurrentLogStream() {
  const stream = document.getElementById('log-stream');
  if (!stream || concurrentLogDetachedFragment) return;
  concurrentLogDetachedScrollTop = stream.scrollTop || 0;
  const fragment = document.createDocumentFragment();
  while (stream.firstChild) fragment.appendChild(stream.firstChild);
  concurrentLogDetachedFragment = fragment;
}

function attachConcurrentLogStream() {
  const stream = document.getElementById('log-stream');
  if (!stream || !concurrentLogDetachedFragment) return;
  const fragment = concurrentLogDetachedFragment;
  concurrentLogDetachedFragment = null;
  stream.appendChild(fragment);
  applyHostFilter();
  updatePromptTargetLogSessionBadges();
  if (autoScroll) {
    stream.scrollTop = stream.scrollHeight;
  } else {
    stream.scrollTop = Math.min(concurrentLogDetachedScrollTop, stream.scrollHeight);
  }
}

function syncConcurrentLogStreamMount() {
  if (shouldDetachConcurrentLogStream()) detachConcurrentLogStream();
  else attachConcurrentLogStream();
}

function sessionWindowGridAvailableHeight() {
  const pane = document.getElementById('activity-log-pane');
  const grid = document.getElementById('session-window-grid');
  const paneHeight = pane?.clientHeight || window.innerHeight || 600;
  if (!pane || !grid) return paneHeight;
  const paneRect = pane.getBoundingClientRect();
  const gridRect = grid.getBoundingClientRect();
  const gridTop = gridRect.height > 0 ? gridRect.top : paneRect.top;
  const topOffset = Math.max(0, gridTop - paneRect.top);
  return Math.max(
    SESSION_WINDOW_GRID_MIN_HEIGHT + CONCURRENT_LOG_MIN_HEIGHT,
    paneRect.height - topOffset
  );
}

function maxSessionWindowGridHeight() {
  const availableHeight = sessionWindowGridAvailableHeight();
  return Math.max(
    SESSION_WINDOW_GRID_MIN_HEIGHT,
    Math.min(
      Math.round(availableHeight * SESSION_WINDOW_GRID_MAX_RATIO),
      availableHeight - CONCURRENT_LOG_MIN_HEIGHT
    )
  );
}

function clampSessionWindowGridHeight(heightPx) {
  const n = Number(heightPx);
  if (!Number.isFinite(n)) return null;
  return Math.round(Math.max(
    SESSION_WINDOW_GRID_MIN_HEIGHT,
    Math.min(n, maxSessionWindowGridHeight())
  ));
}

function defaultSessionWindowGridHeight() {
  const availableHeight = sessionWindowGridAvailableHeight();
  return clampSessionWindowGridHeight(Math.round(availableHeight * SESSION_WINDOW_GRID_DEFAULT_RATIO));
}

function measureSessionWindowGridNaturalHeight() {
  const grid = document.getElementById('session-window-grid');
  if (!grid || grid.classList.contains('hidden')) return null;
  const previousHeight = grid.style.height;
  const previousMaxHeight = grid.style.maxHeight;
  const wasResized = grid.classList.contains('resized');
  const overlay = grid.querySelector(':scope > .session-relationship-wires');
  const previousOverlayDisplay = overlay?.style.display || '';

  grid.classList.remove('resized');
  grid.style.height = 'auto';
  grid.style.maxHeight = 'none';
  if (overlay) overlay.style.display = 'none';
  const naturalHeight = grid.scrollHeight;

  if (overlay) overlay.style.display = previousOverlayDisplay;
  grid.style.height = previousHeight;
  grid.style.maxHeight = previousMaxHeight;
  grid.classList.toggle('resized', wasResized);

  return Number.isFinite(naturalHeight) && naturalHeight > 0 ? naturalHeight : null;
}

function fitSessionWindowGridHeight() {
  const naturalHeight = measureSessionWindowGridNaturalHeight();
  const maxHeight = maxSessionWindowGridHeight();
  const height = naturalHeight === null
    ? defaultSessionWindowGridHeight()
    : Math.min(Math.round(naturalHeight), maxHeight);
  return Math.round(Math.max(
    SESSION_WINDOW_GRID_FIT_MIN_HEIGHT,
    Math.min(height, maxHeight)
  ));
}

function syncSessionWindowGridControls() {
  const grid = document.getElementById('session-window-grid');
  const handle = document.getElementById('session-window-grid-resize-handle');
  const logLabel = document.getElementById('concurrent-log-label');
  if (!grid || !handle) return;
  const showGrid = !grid.classList.contains('hidden');
  syncConcurrentLogModeUi();
  const showResize =
    showGrid
    && !grid.classList.contains('maximized')
    && concurrentLogMode === CONCURRENT_LOG_MODE_NORMAL;
  if (logLabel) logLabel.classList.toggle('hidden', !showGrid);
  handle.classList.toggle('hidden', !showResize);
}

function syncConcurrentLogModeUi() {
  const pane = document.getElementById('activity-log-pane');
  const grid = document.getElementById('session-window-grid');
  const label = document.getElementById('concurrent-log-label');
  const resize = document.getElementById('concurrent-log-resize');
  const minimize = document.getElementById('concurrent-log-minimize');
  const maximize = document.getElementById('concurrent-log-maximize');
  const showGrid = !!grid && !grid.classList.contains('hidden');
  if (!pane) return;
  const mode = showGrid ? concurrentLogMode : CONCURRENT_LOG_MODE_NORMAL;
  pane.classList.toggle('concurrent-log-minimized', mode === CONCURRENT_LOG_MODE_MINIMIZED);
  pane.classList.toggle('concurrent-log-maximized', mode === CONCURRENT_LOG_MODE_MAXIMIZED);
  if (label) label.classList.toggle('hidden', !showGrid);
  if (resize) {
    const fitted = showGrid && concurrentLogFitToSessionWindows && mode === CONCURRENT_LOG_MODE_NORMAL;
    resize.title = fitted
      ? 'Restore previous concurrent view size'
      : 'Resize concurrent view around session windows';
    resize.setAttribute('aria-label', resize.title);
    resize.setAttribute('aria-pressed', fitted ? 'true' : 'false');
  }
  if (minimize) {
    const minimized = mode === CONCURRENT_LOG_MODE_MINIMIZED;
    minimize.textContent = minimized ? '+' : '-';
    minimize.title = minimized ? 'Restore concurrent view' : 'Minimize concurrent view';
    minimize.setAttribute('aria-label', minimize.title);
    minimize.setAttribute('aria-pressed', minimized ? 'true' : 'false');
  }
  if (maximize) {
    const maximized = mode === CONCURRENT_LOG_MODE_MAXIMIZED;
    maximize.innerHTML = maximized ? '&#x2750;' : '&#x26F6;';
    maximize.title = maximized ? 'Restore concurrent view' : 'Maximize concurrent view';
    maximize.setAttribute('aria-label', maximize.title);
    maximize.setAttribute('aria-pressed', maximized ? 'true' : 'false');
  }
  syncConcurrentLogStreamMount();
}

function setConcurrentLogMode(mode) {
  const next = [CONCURRENT_LOG_MODE_MINIMIZED, CONCURRENT_LOG_MODE_MAXIMIZED].includes(mode)
    ? mode
    : CONCURRENT_LOG_MODE_NORMAL;
  concurrentLogMode = next;
  storeConcurrentLogMode(next);
  if (next === CONCURRENT_LOG_MODE_NORMAL) {
    applySessionWindowGridHeight();
  } else {
    syncSessionWindowGridControls();
  }
}

function setConcurrentLogFitToSessionWindows(fitted) {
  concurrentLogFitToSessionWindows = !!fitted;
  if (concurrentLogFitToSessionWindows) {
    concurrentLogMode = CONCURRENT_LOG_MODE_NORMAL;
    storeConcurrentLogMode(concurrentLogMode);
  }
  applySessionWindowGridHeight();
}

function toggleConcurrentLogFitToSessionWindows() {
  const active = concurrentLogFitToSessionWindows && concurrentLogMode === CONCURRENT_LOG_MODE_NORMAL;
  setConcurrentLogFitToSessionWindows(!active);
}

function scheduleSessionWindowGridFit() {
  if (!concurrentLogFitToSessionWindows || concurrentLogMode !== CONCURRENT_LOG_MODE_NORMAL) return;
  if (sessionWindowGridFitRenderHandle) return;
  sessionWindowGridFitRenderHandle = requestAnimationFrame(() => {
    sessionWindowGridFitRenderHandle = 0;
    applySessionWindowGridHeight();
  });
}

function applySessionWindowGridHeight() {
  const grid = document.getElementById('session-window-grid');
  if (!grid) return;
  const hasCustomHeight = sessionWindowGridHeightPx !== null;
  const useFitHeight = concurrentLogFitToSessionWindows
    && concurrentLogMode === CONCURRENT_LOG_MODE_NORMAL;
  const height = useFitHeight
    ? fitSessionWindowGridHeight()
    : (hasCustomHeight
      ? clampSessionWindowGridHeight(sessionWindowGridHeightPx)
      : defaultSessionWindowGridHeight());
  if (!useFitHeight && hasCustomHeight) sessionWindowGridHeightPx = height;
  if (height) {
    grid.style.setProperty('--session-window-grid-height', `${height}px`);
    grid.classList.add('resized');
  } else {
    grid.style.removeProperty('--session-window-grid-height');
    grid.classList.remove('resized');
  }
  syncSessionWindowGridControls();
  scheduleSessionRelationshipRender();
}

function setSessionWindowGridHeight(heightPx, persist = true) {
  concurrentLogFitToSessionWindows = false;
  sessionWindowGridHeightPx = clampSessionWindowGridHeight(heightPx);
  if (persist) storeSessionWindowGridHeight(sessionWindowGridHeightPx);
  applySessionWindowGridHeight();
}

function resetSessionWindowGridHeight() {
  concurrentLogFitToSessionWindows = true;
  sessionWindowGridHeightPx = null;
  storeSessionWindowGridHeight(null);
  applySessionWindowGridHeight();
}

function startSessionWindowGridResize(ev) {
  const grid = document.getElementById('session-window-grid');
  const handle = document.getElementById('session-window-grid-resize-handle');
  if (!grid || !handle || grid.classList.contains('hidden') || grid.classList.contains('maximized')) return;
  ev.preventDefault();
  sessionWindowGridResizeDrag = {
    pointerId: ev.pointerId,
    startY: ev.clientY,
    startHeight: grid.getBoundingClientRect().height,
  };
  handle.classList.add('dragging');
  handle.setPointerCapture?.(ev.pointerId);
}

function updateSessionWindowGridResize(ev) {
  if (!sessionWindowGridResizeDrag || ev.pointerId !== sessionWindowGridResizeDrag.pointerId) return;
  ev.preventDefault();
  const nextHeight = sessionWindowGridResizeDrag.startHeight + (ev.clientY - sessionWindowGridResizeDrag.startY);
  setSessionWindowGridHeight(nextHeight, false);
}

function endSessionWindowGridResize(ev) {
  if (!sessionWindowGridResizeDrag || ev.pointerId !== sessionWindowGridResizeDrag.pointerId) return;
  const handle = document.getElementById('session-window-grid-resize-handle');
  handle?.classList.remove('dragging');
  handle?.releasePointerCapture?.(ev.pointerId);
  storeSessionWindowGridHeight(sessionWindowGridHeightPx);
  sessionWindowGridResizeDrag = null;
}

function copyTextToClipboard(text) {
  const value = String(text || '');
  if (!value) return Promise.reject(new Error('Nothing to copy'));
  if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
    return navigator.clipboard.writeText(value);
  }
  return new Promise((resolve, reject) => {
    const ta = document.createElement('textarea');
    ta.value = value;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.left = '-9999px';
    ta.style.top = '0';
    document.body.appendChild(ta);
    ta.select();
    try {
      if (document.execCommand('copy')) resolve();
      else reject(new Error('Copy command failed'));
    } catch (err) {
      reject(err);
    } finally {
      ta.remove();
    }
  });
}

function setLogEntryCopyText(entry, textOrRef) {
  if (!entry) return null;
  const ref = textOrRef && typeof textOrRef === 'object' && Object.prototype.hasOwnProperty.call(textOrRef, 'text')
    ? textOrRef
    : { text: String(textOrRef ?? '') };
  logEntryCopyTextByEntry.set(entry, ref);
  return ref;
}

function copyLogEntryCopyText(sourceEntry, targetEntry) {
  const ref = sourceEntry ? logEntryCopyTextByEntry.get(sourceEntry) : null;
  if (ref && targetEntry) logEntryCopyTextByEntry.set(targetEntry, ref);
}

function getLogEntryCopyText(entry) {
  const ref = entry ? logEntryCopyTextByEntry.get(entry) : null;
  if (ref) return String(ref.text ?? '');
  return entry?.querySelector?.('.log-content')?.innerText || '';
}

function resetLogCopyButton(btn) {
  if (!btn) return;
  btn.classList.remove('copied');
  btn.innerHTML = '&#x29C9;';
  btn.title = 'Copy raw log entry';
  btn.setAttribute('aria-label', btn.title);
}

async function onLogCopyButtonClick(ev) {
  ev.preventDefault();
  ev.stopPropagation();
  const btn = ev.currentTarget;
  const entry = btn.closest('.log-entry');
  try {
    await copyTextToClipboard(getLogEntryCopyText(entry));
    btn.classList.add('copied');
    btn.innerHTML = '&#x2713;';
    btn.title = 'Copied';
    btn.setAttribute('aria-label', btn.title);
    clearTimeout(btn._copyResetTimer);
    btn._copyResetTimer = setTimeout(() => resetLogCopyButton(btn), 1200);
  } catch (e) {
    showControlToast('error', 'Copy failed: ' + (e?.message || e));
  }
}

function wireLogCopyButton(entry) {
  const btn = entry?.querySelector?.(':scope > .log-copy-entry');
  if (!btn || wiredLogCopyButtons.has(btn)) return;
  wiredLogCopyButtons.add(btn);
  btn.addEventListener('click', onLogCopyButtonClick);
}

function appendCopyLogEntryButton(entry, textOrRef) {
  if (!entry) return null;
  if (textOrRef !== undefined) setLogEntryCopyText(entry, textOrRef);
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'log-copy-entry';
  resetLogCopyButton(btn);
  entry.appendChild(btn);
  wireLogCopyButton(entry);
  return btn;
}

function sessionAliasIds(session) {
  if (!session) return [];
  return [session.session_id, session.resume_id, session.backend_session_id, session.intendant_session_id]
    .map(id => String(id || '').trim())
    .filter(Boolean);
}

function normalizeSessionSource(raw) {
  const v = String(raw || '').trim().toLowerCase();
  if (!v) return '';
  if (v === 'session' || v === 'intendant') return 'intendant';
  return normalizeAgentId(v) || v;
}

function sessionIdentityMatches(session, sessionId, source = '') {
  if (!session || !sessionId) return false;
  const sid = String(sessionId || '').trim();
  if (!sessionAliasIds(session).includes(sid)) {
    return false;
  }
  const normalizedSource = normalizeSessionSource(source);
  if (!normalizedSource) return true;
  return [
    session.source || 'intendant',
    session.backend_source,
    session.backendSource,
  ]
    .map(normalizeSessionSource)
    .some(candidate => candidate === normalizedSource);
}

function applySessionRenameToUi(sessionId, source, name) {
  const sid = String(sessionId || '').trim();
  const displayName = compactSessionText(name);
  if (!sid || !displayName) return;
  const normalizedSource = normalizeSessionSource(source);
  const aliasIds = new Set([sid]);
  for (const session of _cachedSessions) {
    if (sessionIdentityMatches(session, sid, normalizedSource)) {
      session.name = displayName;
      sessionAliasIds(session).forEach(id => {
        const alias = String(id || '').trim();
        if (alias) aliasIds.add(alias);
      });
    }
  }
  if (currentSessionDetail && sessionIdentityMatches(currentSessionDetail, sid, normalizedSource)) {
    sessionAliasIds(currentSessionDetail).forEach(id => {
      if (id) aliasIds.add(id);
    });
  }
  aliasIds.forEach(id => {
    const previous = sessionMetadataById.get(id) || {};
    sessionMetadataById.set(id, {
      ...previous,
      name: displayName,
      ...(normalizedSource ? { source: normalizedSource } : {}),
    });
    updateSessionWindow(id, { name: displayName, source: normalizedSource || undefined });
    refreshSessionIdentityLabels(id);
  });
  if (currentSessionDetail && sessionIdentityMatches(currentSessionDetail, sid, normalizedSource)) {
    currentSessionDetail = { ...currentSessionDetail, name: displayName };
    renderSessionDetailTitle(currentSessionDetail);
  }
  if (sessionsLoaded) _refilterSessions();
}

function sessionRenameIds() {
  if (!sessionRenameEditing) return [];
  return [
    sessionRenameEditing.sessionId,
    sessionRenameEditing.backendSessionId,
  ].map(id => String(id || '').trim()).filter(Boolean);
}

function closeSessionRenameModalIfMatches(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || !sessionRenameIds().includes(sid)) return;
  closeSessionRenameModal();
}

function showSessionRenameStatus(message, kind = '', sessionId = '') {
  if (sessionId) {
    const sid = String(sessionId || '').trim();
    if (sid && !sessionRenameIds().includes(sid)) return;
  }
  const status = document.getElementById('session-rename-status');
  if (!status) return;
  status.className = 'session-config-status' + (kind ? ` ${kind}` : '');
  status.textContent = message || '';
}

function closeSessionRenameModal() {
  const modal = document.getElementById('session-rename-modal');
  if (modal) modal.style.display = 'none';
  sessionRenameEditing = null;
  showSessionRenameStatus('');
}

function requestSessionRename(sessionOrId, sourceArg = '') {
  const session = typeof sessionOrId === 'object'
    ? sessionOrId
    : { session_id: sessionOrId, source: sourceArg };
  const sid = String(session?.session_id || session?.resume_id || '').trim();
  if (!sid) {
    showControlToast('error', 'Rename session failed: session ID is missing');
    return false;
  }
  const source = String(sourceArg || session.source || 'intendant').trim();
  const cached = sessionMetadataById.get(sid) || {};
  const backendSessionId = String(
    session.backend_session_id ||
    session.backendSessionId ||
    cached.backendSessionId ||
    ''
  ).trim();
  const backendSource = String(
    session.backend_source ||
    session.backendSource ||
    cached.backendSource ||
    ''
  ).trim();
  const effectiveSource = backendSource || source;
  const currentName = compactSessionText(session.name || cached.name);
  sessionRenameEditing = {
    sessionId: sid,
    source: effectiveSource,
    backendSessionId,
  };
  const sessionInput = document.getElementById('session-rename-session');
  if (sessionInput) {
    sessionInput.value = sid;
    sessionInput.title = sid;
  }
  const nameInput = document.getElementById('session-rename-name');
  if (nameInput) {
    nameInput.value = currentName;
    nameInput.title = currentName;
  }
  showSessionRenameStatus('');
  const modal = document.getElementById('session-rename-modal');
  if (modal) modal.style.display = 'flex';
  setTimeout(() => {
    const input = document.getElementById('session-rename-name');
    if (!input) return;
    input.focus();
    input.select();
  }, 0);
  return true;
}

function saveSessionRenameModal() {
  if (!sessionRenameEditing || !app) return;
  const name = document.getElementById('session-rename-name')?.value.trim() || '';
  if (!name) {
    showSessionRenameStatus('Session name is required.', 'error');
    return;
  }
  showSessionRenameStatus('Saving...');
  dispatchControlMsg({
    action: 'rename_session',
    session_id: sessionRenameEditing.sessionId,
    source: sessionRenameEditing.source,
    ...(sessionRenameEditing.backendSessionId ? { backend_session_id: sessionRenameEditing.backendSessionId } : {}),
    name,
  });
}

// Index over _cachedSessions for O(1) any-id lookups. Every mutation site
// reassigns the array (never splices in place), so the array's reference
// identity doubles as the invalidation key. Station snapshot builds call
// this hundreds of times per pass; the previous linear scan made each
// build O(sessions²) — multi-second stalls at a few hundred sessions.
let _cachedSessionIndex = null;
let _cachedSessionIndexFor = null;

function findCachedSessionByAnyId(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  if (_cachedSessionIndexFor !== _cachedSessions) {
    _cachedSessionIndex = new Map();
    for (const session of _cachedSessions || []) {
      if (!session) continue;
      for (const id of [
        session.session_id,
        session.resume_id,
        session.backend_session_id,
        session.intendant_session_id,
      ]) {
        const key = String(id || '').trim();
        // First match wins, matching the old Array.find semantics.
        if (key && !_cachedSessionIndex.has(key)) _cachedSessionIndex.set(key, session);
      }
    }
    _cachedSessionIndexFor = _cachedSessions;
  }
  return _cachedSessionIndex.get(sid) || null;
}

function sessionConfigSource(meta = {}) {
  return normalizeAgentId(
    meta.backendSource ||
    meta.backend_source ||
    meta.configuredSource ||
    meta.configured_source ||
    meta.backendSourceLabel ||
    meta.backend_source_label ||
    meta.source ||
    meta.source_label ||
    meta.sourceLabel ||
    ''
  );
}

function sessionConfigCommand(meta = {}) {
  return String(
    meta.agentCommand ||
    meta.agent_command ||
    meta.codexCommand ||
    meta.codex_command ||
    meta.capabilities?.codexCommand ||
    meta.capabilities?.codex_command ||
    ''
  ).trim();
}

function sessionConfigManagedMode(meta = {}) {
  const mode = String(
    meta.codexManagedContext ||
    meta.codex_managed_context ||
    meta.capabilities?.codexManagedContext ||
    meta.capabilities?.codex_managed_context ||
    ''
  ).trim();
  return mode === 'managed' ? 'managed' : 'vanilla';
}

function normalizeOptionalCodexSandbox(mode) {
  const v = String(mode || '').trim();
  return ['workspace-write', 'danger-full-access', 'read-only'].includes(v) ? v : '';
}

function normalizeOptionalCodexApprovalPolicy(policy) {
  const v = String(policy || '').trim();
  return ['on-request', 'never', 'untrusted'].includes(v) ? v : '';
}

function sessionConfigExplicitSandboxMode(meta = {}) {
  return normalizeOptionalCodexSandbox(
    meta.codexSandbox ||
    meta.codex_sandbox ||
    ''
  );
}

function sessionConfigExplicitApprovalPolicy(meta = {}) {
  return normalizeOptionalCodexApprovalPolicy(
    meta.codexApprovalPolicy ||
    meta.codex_approval_policy ||
    ''
  );
}

// Explicitly pinned per-session managed-context value ('' = no pin, i.e.
// inherit the global default). Unlike sessionConfigManagedMode this never
// falls back to capabilities or coerces unknown to 'vanilla' — it seeds the
// launch-config selects, where defaulting to 'vanilla' would re-pin vanilla
// into the session overlay on the next save.
function sessionConfigExplicitManagedMode(meta = {}) {
  const mode = String(
    meta.codexManagedContext ||
    meta.codex_managed_context ||
    ''
  ).trim();
  return mode === 'managed' || mode === 'vanilla' ? mode : '';
}

function sessionConfigExplicitArchiveMode(meta = {}) {
  return normalizeContextArchiveModeOptional(
    meta.codexContextArchive ||
    meta.codex_context_archive ||
    ''
  );
}

function sessionConfigSandboxMode(meta = {}) {
  return normalizeCodexSandbox(
    sessionConfigExplicitSandboxMode(meta) ||
    meta.capabilities?.codexSandbox ||
    meta.capabilities?.codex_sandbox ||
    controlCodexConfig.sandbox ||
    'workspace-write'
  );
}

function sessionConfigApprovalPolicy(meta = {}) {
  return normalizeCodexApprovalPolicy(
    sessionConfigExplicitApprovalPolicy(meta) ||
    meta.capabilities?.codexApprovalPolicy ||
    meta.capabilities?.codex_approval_policy ||
    controlCodexConfig.approval_policy ||
    'on-request'
  );
}

