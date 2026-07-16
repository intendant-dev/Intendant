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

// Last target the chip rendered — lets updateTaskTargetChip tell an
// automatic resolver rebind (pulse it) apart from a repeat render.
let taskTargetChipRenderedTarget = null;

function updateTaskTargetChip() {
  const sid = resolvePromptTargetSessionId();
  const previousTarget = taskTargetChipRenderedTarget;
  taskTargetChipRenderedTarget = sid;
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
  // An explicit pick lands in foreground/current before this runs; a
  // target that differs from those came from the resolver's fallbacks
  // (latest supervised external / single usable window) — pulse the chip
  // so the silent rebind is visible. element.animate keeps this out of
  // the stylesheets; skip the initial render (previousTarget === null).
  if (
    previousTarget !== null &&
    previousTarget !== sid &&
    sid !== explicitForegroundSessionId()
  ) {
    try {
      chip.animate([
        { transform: 'scale(1)', filter: 'brightness(1)' },
        { transform: 'scale(1.12)', filter: 'brightness(1.6)', offset: 0.35 },
        { transform: 'scale(1)', filter: 'brightness(1)' },
      ], { duration: 600, easing: 'ease-in-out' });
    } catch (_) { /* Web Animations unavailable — rebind stays silent */ }
  }
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
  // Supervision gate: the composer may only auto-target sessions this
  // daemon supervises. Evidence is a session window on this dashboard
  // (daemon lifecycle events or an explicit restore created it) or the
  // wrapper linkage — intendant_session_id is only minted when an
  // intendant session wraps the backend thread (session_catalog's
  // wrapper merge / wrapper index). Catalog-only foreign CLI sessions
  // carry neither and must never steal the prompt target.
  if (!intendantSessionId && !sessionWindows.has(sid)) return false;
  return !intendantSessionId || !daemonSessionFullId || intendantSessionId === daemonSessionFullId;
}

function discardPromptTargetReference(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  if (foregroundSessionFullId === sid) foregroundSessionFullId = '';
  if (currentSessionFullId === sid) currentSessionFullId = '';
}

// Explicit focus only — none of resolvePromptTargetSessionId's routing
// fallbacks: the status-bar gate must never let a "best guess" session
// drive the header chips while the user is looking at a different window.
// Empty when no session window is explicitly focused.
function explicitForegroundSessionId() {
  return String(foregroundSessionFullId || currentSessionFullId || '').trim();
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
  // Worktree linkage (an OBJECT — branch/path/base) is distinct from the
  // legacy string `worktree` cwd alias some callers pass; only the string
  // form may feed the cwd fallback below.
  const worktreeInfo = normalizeSessionWorktreeInfo(meta.worktree);
  if (worktreeInfo) out.worktree = worktreeInfo;
  const worktreeCwdAlias = typeof meta.worktree === 'string' ? meta.worktree : '';
  const cwd = compactSessionText(meta.cwd || meta.workdir || meta.workDir || worktreeCwdAlias || project);
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

// Session worktree linkage from the catalog row / session_meta.json —
// `{branch, path, base_root, base_branch?, base_sha?}` — normalized to a
// compact object, or null when absent/malformed.
function normalizeSessionWorktreeInfo(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const branch = compactSessionText(raw.branch);
  const path = compactSessionText(raw.path);
  if (!branch || !path) return null;
  const info = { branch, path };
  const baseRoot = compactSessionText(raw.base_root || raw.baseRoot);
  if (baseRoot) info.baseRoot = baseRoot;
  const baseBranch = compactSessionText(raw.base_branch || raw.baseBranch);
  if (baseBranch) info.baseBranch = baseBranch;
  const baseSha = compactSessionText(raw.base_sha || raw.baseSha);
  if (baseSha) info.baseSha = baseSha;
  return info;
}

function sessionWindowMetadataSignature(meta = {}) {
  return [
    meta.name || '',
    meta.task || '',
    meta.projectRoot || '',
    meta.projectLabel || '',
    meta.cwd || '',
    meta.cwdLabel || '',
    meta.worktree ? `${meta.worktree.branch}${meta.worktree.path}${meta.worktree.baseBranch || ''}` : '',
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
  // The merged signature rides the return value so updateSessionWindow —
  // reached per rendered log line via inferSessionPhaseFromLog — doesn't
  // recompute the ~35-field join a third time per call.
  const previousSignature = sessionWindowMetadataSignature(previous);
  const signature = merged === previous
    ? previousSignature
    : sessionWindowMetadataSignature(merged);
  const changed = previousSignature !== signature;
  if (sid && changed) sessionMetadataById.set(sid, merged);
  return { meta: merged, changed, signature };
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
  // Write-guards throughout: this runs for EVERY window on the 1 Hz goal
  // ticker, and unconditional textContent/className assignment churns text
  // nodes (and feeds every subtree MutationObserver watching the grid)
  // even when the rendered second hasn't rolled over.
  const setText = (el, prop, value) => { if (el[prop] !== value) el[prop] = value; };
  if (!goal?.objective) {
    setText(win.goal, 'className', 'session-window-goal hidden');
    setText(win.goalText, 'textContent', '');
    setText(win.goal, 'title', '');
    return false;
  }
  const status = normalizeGoalStatus(goal.status);
  const elapsed = formatGoalElapsed(currentSessionGoalElapsedSeconds(goal));
  setText(win.goal, 'className', `session-window-goal ${goalStatusClass(status)}`);
  setText(win.goalText, 'textContent', status === 'active'
    ? `${goal.objective} · ${elapsed}`
    : `${status} · ${goal.objective} · ${elapsed}`);
  const tokenText = goal.tokensUsed !== null && goal.tokensUsed !== undefined
    ? `\nTokens: ${goal.tokensUsed.toLocaleString()}${goal.tokenBudget ? ` / ${goal.tokenBudget.toLocaleString()}` : ''}`
    : (goal.tokenBudget ? `\nBudget: ${goal.tokenBudget.toLocaleString()} tokens` : '');
  setText(win.goal, 'title', `Goal: ${goal.objective}\nStatus: ${status}\nElapsed: ${elapsed}${tokenText}`);
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

// Arm-only twin for per-window update paths: updateSessionWindow already
// rendered ITS window's goal — re-rendering every other window's goal per
// metadata tick (the old refreshSessionGoalTicker call) was pure waste.
// The 1 s ticker still owns elapsed-time repaints and disarms itself.
function ensureSessionGoalTickerArmed(hasActiveGoal) {
  if (hasActiveGoal && !sessionGoalTicker) {
    sessionGoalTicker = setInterval(refreshSessionGoalTicker, 1000);
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

// ── Session vitals (git / cache / limits / worktree — the statusline port)
//
// VITALS_SYMBOLS is THE catalog: every vitals surface — session-window
// header chips, the Focus rail tooltips, the tap-to-explain popovers, the
// glossary sheet — derives its glyph, label, plain-language explanation,
// severity, and action from this one map ("derive, don't mirror"). Never
// hand-write a vitals sentence anywhere else. Explanations are written
// for the least technical reader first: say what the symbol MEANS for
// them, never how it is computed. Chips are real <button>s — the header's
// collapse-on-click guard exempts buttons, and they get keyboard and
// screen-reader semantics for free. The wire fields consumed between the
// markers are pinned by
// session_vitals.rs::vitals_symbol_catalog_covers_wire_fields.
// VITALS_SYMBOLS_BEGIN
const VITALS_SEVERITY_RANK = { '': 0, ok: 0, warn: 1, crit: 2 };

function vitalsLimitLabelLong(label) {
  if (label === '5h') return '5-hour';
  if (label === '7d') return '7-day';
  return label;
}

const VITALS_SYMBOLS = {
  health: {
    label: 'Session health',
    priority: 100,
    chip: () => '',
    explain: (v) => (v.severity === 'crit'
      ? ['Something needs your attention — the highlighted symbols say what.']
      : v.severity === 'warn'
        ? ['Worth a look soon — the highlighted symbols say why.']
        : ['Everything looks good.', 'Tap any symbol to learn what it tracks.']),
  },
  worktree: {
    label: 'Worktree',
    priority: 20,
    unavailable: 'Working directly in the project folder (its main checkout) — not in an isolated worktree copy.',
    chip: () => '⧉',
    explain: (v) => {
      const lines = [
        'This agent works in its own copy of the project (a git worktree), so agents working in parallel never disturb each other.',
        `Branch: ${v.branch}`,
      ];
      if (v.path) lines.push(`Folder: ${v.path}`);
      if (v.baseBranch) lines.push(`Started from ${v.baseBranch}${v.baseSha ? ` @ ${v.baseSha.slice(0, 12)}` : ''}`);
      return lines;
    },
    action: (v) => (v.path ? { label: 'Copy folder path', run: () => vitalsCopyText(v.path) } : null),
  },
  branch: {
    label: 'Branch',
    priority: 30,
    chip: (v) => `⎇ ${v.branch}`,
    explain: (v) => [`“${v.branch}” is the git branch this agent is working on.`],
    action: (v) => ({ label: 'Copy branch name', run: () => vitalsCopyText(v.branch) }),
  },
  dirty: {
    label: 'Uncommitted changes',
    priority: 60,
    quiet: 'Nothing uncommitted — the working folder is clean.',
    chip: (v) => `●${v.count}`,
    explain: (v) => [
      `${v.count} file${v.count === 1 ? ' has' : 's have'} changes that aren't committed to git yet.`,
      'Normal while the agent works — they become permanent history once committed.',
    ],
    action: () => ({ label: 'View the changes', run: () => vitalsOpenChangesTab() }),
  },
  divergence: {
    label: 'Ahead / behind',
    priority: 40,
    quiet: 'No primary branch to compare with.',
    chip: (v) => `+${v.ahead}/−${v.behind}`,
    explain: (v) => [
      `Compared with ${v.primaryRef}: this branch has ${v.ahead} commit${v.ahead === 1 ? '' : 's'} of its own and is missing ${v.behind} from there.`,
    ],
  },
  parity: {
    label: 'Merge readiness',
    priority: 90,
    quiet: 'No merge check needed — this branch is not diverged from the primary.',
    chip: (v) => (v.conflict ? '⚠' : '✓'),
    explain: (v) => (v.conflict
      ? [`Merging this work into ${v.primaryRef} would CONFLICT — overlapping edits need a decision before it can land.`]
      : [`Merging this work into ${v.primaryRef} would apply cleanly right now.`]),
    action: (v, sessionId) => (v.conflict
      ? { label: 'Open the merge card', run: () => openSessionWorktreeMergeCard(sessionId) }
      : null),
  },
  unpushed: {
    label: 'Unpushed commits',
    priority: 35,
    quiet: 'Nothing waiting to be pushed (or no upstream is configured).',
    chip: (v) => `⇡${v.count}`,
    explain: (v) => [
      `${v.count} commit${v.count === 1 ? '' : 's'} on this branch exist${v.count === 1 ? 's' : ''} only on this machine — not pushed to the shared repository yet.`,
    ],
  },
  'primary-unpushed': {
    label: 'Unpushed on primary',
    priority: 25,
    quiet: 'Nothing unpushed on the primary branch.',
    chip: (v) => `${v.primaryName}⇡${v.count}`,
    explain: (v) => [
      `The primary branch ${v.primaryName} has ${v.count} commit${v.count === 1 ? '' : 's'} that ${v.count === 1 ? 'hasn’t' : 'haven’t'} been pushed yet.`,
    ],
  },
  'cache-hit': {
    label: 'Prompt cache',
    priority: 15,
    quiet: 'No cache reading on the last request.',
    chip: (v) => `⚡${v.hitPct}%`,
    explain: (v) => [
      `${v.hitPct}% of the last request was answered from the provider's prompt cache.`,
      'Cached input is far cheaper and faster than fresh input.',
    ],
  },
  'cache-ttl': {
    label: 'Cache freshness',
    priority: 45,
    quiet: "This provider doesn't report how long its cache stays warm.",
    chip: (v) => (v.cold ? '✗' : `⏳${v.countdown}`),
    explain: (v) => (v.cold
      ? [
        'The prompt cache went cold — the next request rebuilds it.',
        'Nothing is broken; the next turn just costs a little more once.',
      ]
      : [
        `The prompt cache stays warm for another ${v.countdown}.`,
        'If the session idles past that, the next request rebuilds it — slower and pricier.',
      ]),
  },
  limit: {
    label: 'Rate limit',
    priority: 80,
    chip: (v) => (v.usedPct !== null
      ? `▮${v.usedPct}% ${v.label}`
      : `${v.label} ${v.statusWord}${v.reset ? ` ↻${v.reset}` : ''}`),
    explain: (v) => {
      const lines = [];
      if (v.usedPct !== null) {
        lines.push(`${v.usedPct}% of the provider's ${vitalsLimitLabelLong(v.label)} usage allowance is used.`);
      } else {
        lines.push(`The provider reports its ${vitalsLimitLabelLong(v.label)} allowance as “${v.statusWord}” — it doesn't share an exact percentage.`);
      }
      if (v.reset) lines.push(`The window resets in ~${v.reset}.`);
      if (v.severity === 'crit') lines.push('When an allowance runs out, the provider pauses this agent until the window resets.');
      return lines;
    },
  },
};

// Fixed display order for chips and the glossary (semantic, not priority:
// priority only governs which chips stay visible when space is scarce).
const VITALS_SYMBOL_ORDER = [
  'health', 'worktree', 'branch', 'dirty', 'divergence', 'parity',
  'unpushed', 'primary-unpushed', 'cache-hit', 'cache-ttl', 'limit',
];

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

function formatLimitReset(resetsAtEpoch) {
  const remaining = Number(resetsAtEpoch) - Date.now() / 1000;
  if (!Number.isFinite(remaining) || remaining <= 0) return '';
  if (remaining >= 86400) return `${Math.round(remaining / 86400)}d`;
  if (remaining >= 3600) return `${Math.round(remaining / 3600)}h`;
  return `${Math.max(1, Math.round(remaining / 60))}m`;
}

function limitStatusSeverity(status) {
  const s = String(status || '').trim().toLowerCase();
  if (!s || s === 'allowed') return '';
  if (s === 'allowed_warning') return 'warn';
  return 'crit'; // rejected / limited / queueing / anything non-allowed
}

function limitStatusWord(status, severity) {
  if (severity === 'crit') return 'limited';
  if (severity === 'warn') return 'getting high';
  return 'ok';
}

// Chip models for one session: [{ id, key, label, text, severity,
// elevated, priority, explainLines, action, ticking }]. `vitals` may be
// null (a worktree session with no vitals yet still gets health +
// worktree). The health model is synthesized last and leads the list.
function vitalsChipModels(vitals, meta, sessionId) {
  const models = [];
  const push = (key, id, value, extra = {}) => {
    const def = VITALS_SYMBOLS[key];
    if (!def) return;
    // severity = attention (elevates the chip, feeds the health dot);
    // tone = cosmetic tint only. A fresh session's 0% cache hit is
    // expensive (red TEXT) but not wrong (never a red health dot).
    const severity = extra.severity || '';
    const tone = extra.tone !== undefined ? extra.tone : severity;
    models.push({
      id: id || key,
      key,
      label: def.label,
      priority: def.priority,
      text: def.chip(value),
      severity,
      tone,
      elevated: severity === 'warn' || severity === 'crit',
      explainLines: def.explain({ ...value, severity }),
      action: def.action ? (def.action(value, sessionId) || null) : null,
      ticking: !!extra.ticking,
    });
  };

  const worktree = meta?.worktree && typeof meta.worktree === 'object' ? meta.worktree : null;
  if (worktree?.branch) {
    push('worktree', 'worktree', {
      branch: worktree.branch,
      path: worktree.path || '',
      baseBranch: worktree.baseBranch || '',
      baseSha: worktree.baseSha || '',
    });
  }

  const git = vitals?.git && typeof vitals.git === 'object' ? vitals.git : null;
  if (git) {
    const branch = String(git.branch || '').trim();
    if (branch) push('branch', 'branch', { branch });
    const dirty = Number(git.dirtyFiles) || 0;
    if (dirty > 0) push('dirty', 'dirty', { count: dirty });
    const primaryRef = String(git.primaryRef || '').trim();
    if (primaryRef) {
      push('divergence', 'divergence', {
        ahead: Number(git.ahead) || 0,
        behind: Number(git.behind) || 0,
        primaryRef,
      });
    }
    const parity = String(git.mergeParity || '').trim();
    if (parity === 'clean' || parity === 'conflict') {
      const conflict = parity === 'conflict';
      push('parity', 'parity', { conflict, primaryRef: primaryRef || 'the primary branch' },
        { severity: conflict ? 'crit' : 'ok' });
    }
    if (git.unpushed !== null && git.unpushed !== undefined) {
      push('unpushed', 'unpushed', { count: Number(git.unpushed) || 0 });
    }
    if (git.primaryUnpushed !== null && git.primaryUnpushed !== undefined && primaryRef) {
      push('primary-unpushed', 'primary-unpushed', {
        primaryName: primaryRef.replace(/^origin\//, ''),
        count: Number(git.primaryUnpushed) || 0,
      });
    }
  }

  const cache = vitals?.cache && typeof vitals.cache === 'object' ? vitals.cache : null;
  if (cache) {
    const hitRaw = Number(cache.hitPct);
    if (Number.isFinite(hitRaw) && cache.hitPct !== null && cache.hitPct !== undefined) {
      const hitPct = Math.max(0, Math.min(100, hitRaw));
      push('cache-hit', 'cache-hit', { hitPct }, {
        severity: hitPct >= 90 ? 'ok' : '',
        tone: hitPct >= 90 ? 'ok' : hitPct >= 50 ? 'warn' : 'crit',
      });
    }
    const remaining = sessionCacheCountdownSeconds(cache);
    if (remaining !== null) {
      if (remaining > 0) {
        push('cache-ttl', 'cache-ttl', { cold: false, countdown: formatCacheCountdown(remaining) },
          { severity: '', tone: remaining <= 60 ? 'crit' : '', ticking: true });
      } else {
        push('cache-ttl', 'cache-ttl', { cold: true, countdown: '' });
      }
    }
  }

  const limits = Array.isArray(vitals?.limits) ? vitals.limits : [];
  for (const w of limits) {
    const label = String(w?.label || '').trim() || 'window';
    const pctRaw = Number(w?.usedPct);
    const usedPct = Number.isFinite(pctRaw) && w?.usedPct !== null && w?.usedPct !== undefined
      ? Math.max(0, Math.min(100, pctRaw))
      : null;
    const statusSeverity = limitStatusSeverity(w?.status);
    let severity = statusSeverity;
    if (usedPct !== null) {
      if (usedPct >= 90) severity = 'crit';
      else if (usedPct >= 70 && severity !== 'crit') severity = 'warn';
    }
    push('limit', `limit:${label}`, {
      label,
      usedPct,
      statusWord: limitStatusWord(w?.status, statusSeverity),
      reset: w?.resetsAtEpoch ? formatLimitReset(w.resetsAtEpoch) : '',
      severity,
    }, { severity });
  }

  const order = new Map(VITALS_SYMBOL_ORDER.map((key, index) => [key, index]));
  models.sort((a, b) => (order.get(a.key) ?? 99) - (order.get(b.key) ?? 99));

  const worst = models.reduce(
    (acc, m) => (VITALS_SEVERITY_RANK[m.severity] > VITALS_SEVERITY_RANK[acc] ? m.severity : acc),
    ''
  );
  const healthDef = VITALS_SYMBOLS.health;
  models.unshift({
    id: 'health',
    key: 'health',
    label: healthDef.label,
    priority: healthDef.priority,
    text: '',
    severity: worst === 'ok' ? '' : worst,
    tone: worst === 'ok' ? '' : worst,
    elevated: true,
    explainLines: healthDef.explain({ severity: worst }),
    action: null,
    ticking: false,
  });
  return models;
}
// VITALS_SYMBOLS_END

const VITALS_GIT_KEYS = new Set([
  'branch', 'dirty', 'divergence', 'parity', 'unpushed', 'primary-unpushed',
]);

// Legacy family segments — the Focus rail (ui2RailTick) and Station rows
// consume these. Thin catalog derivations: same glyph strings, same
// sentences as the chip popovers, so no surface can drift.
function sessionVitalsGitSegment(git) {
  if (!git || typeof git !== 'object') return null;
  const models = vitalsChipModels({ git }, null, '')
    .filter((m) => VITALS_GIT_KEYS.has(m.key));
  if (!models.length) return null;
  return {
    text: models.map((m) => m.text).join(' '),
    titleLines: models.flatMap((m) => m.explainLines),
    conflict: models.some((m) => m.key === 'parity' && m.severity === 'crit'),
  };
}

function sessionVitalsCacheSegment(cache) {
  if (!cache || typeof cache !== 'object') return null;
  const models = vitalsChipModels({ cache }, null, '')
    .filter((m) => m.key === 'cache-hit' || m.key === 'cache-ttl');
  if (!models.length) return null;
  let cls = 'vit-cache';
  const hit = models.find((m) => m.key === 'cache-hit');
  if (hit) cls += hit.tone === 'ok' ? ' cache-ok' : hit.tone === 'warn' ? ' cache-warn' : ' cache-crit';
  const ttl = models.find((m) => m.key === 'cache-ttl');
  if (ttl) cls += ttl.tone === 'crit' ? ' cache-expiring' : ttl.text === '✗' ? ' cache-cold' : '';
  return {
    text: models.map((m) => m.text).join(' '),
    titleLines: models.flatMap((m) => m.explainLines),
    cls,
    ticking: models.some((m) => m.ticking),
  };
}

// The rail gauge shows the most-used window; the tooltip lists them all.
function sessionVitalsLimitsSegment(limits) {
  if (!Array.isArray(limits) || !limits.length) return null;
  const models = vitalsChipModels({ limits }, null, '')
    .filter((m) => m.key === 'limit');
  if (!models.length) return null;
  const severity = models.reduce(
    (acc, m) => (VITALS_SEVERITY_RANK[m.severity] > VITALS_SEVERITY_RANK[acc] ? m.severity : acc),
    ''
  );
  const top = models.reduce((a, b) =>
    (VITALS_SEVERITY_RANK[b.severity] > VITALS_SEVERITY_RANK[a.severity] ? b : a));
  let cls = 'vit-limits';
  if (severity === 'crit') cls += ' limits-crit';
  else if (severity === 'warn') cls += ' limits-warn';
  return {
    text: top.text,
    titleLines: models.flatMap((m) => m.explainLines),
    cls,
    severity: severity === 'ok' ? '' : severity,
  };
}

// Renders the chip row; returns true while a cache countdown is live (the
// vitals ticker keeps re-rendering until everything is cold or gone).
//
// Truncation policy: NEVER mid-string ellipsis. Expanded headers wrap the
// chips onto extra lines so nothing is ever cut; collapsed headers show
// the health dot + severity-elevated chips + a "+N" overflow chip that
// opens the glossary (CSS keys off data-elevated / .header-collapsed).
function renderSessionWindowVitals(win, vitals) {
  if (!win?.vitals) return false;
  const meta = sessionMetadataById.get(win.sessionId) || {};
  const models = vitalsChipModels(vitals ?? meta.vitals ?? null, meta, win.sessionId);
  // health alone means no tracked symbols at all — a verdict about
  // nothing is noise, hide the row entirely.
  if (models.length <= 1) {
    win.vitals.className = 'session-window-vitals hidden';
    win.vitals.replaceChildren();
    win.vitals.removeAttribute('title');
    delete win.vitals.dataset.vitSig;
    return false;
  }
  wireVitalsChipRow(win);
  // Stable-DOM fast path: the 1s countdown ticker must not rebuild the
  // row (replaced buttons detach mid-tap). When only the cache countdown
  // moved, update that chip's text in place.
  const signature = models
    .map((m) => `${m.id}${m.key === 'cache-ttl' ? (m.text === '✗' ? 'cold' : 'warm') : m.text}${m.tone}${m.elevated ? 1 : 0}`)
    .join('|');
  if (win.vitals.dataset.vitSig === signature) {
    const ttl = models.find((m) => m.key === 'cache-ttl');
    const ttlChip = win.vitals.querySelector('[data-chip="cache-ttl"]');
    if (ttl && ttlChip) {
      ttlChip.textContent = ttl.text;
      ttlChip.title = ttl.explainLines[0] || ttl.label;
    }
    return models.some((m) => m.ticking);
  }
  win.vitals.dataset.vitSig = signature;
  win.vitals.className = 'session-window-vitals';
  win.vitals.setAttribute('role', 'group');
  win.vitals.setAttribute('aria-label', 'Session vitals — select a symbol for its meaning');
  win.vitals.removeAttribute('title');
  const nodes = [];
  let foldedWhenCollapsed = 0;
  for (const m of models) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'vit-chip' + (m.key === 'health' ? ' vit-health' : '');
    chip.dataset.chip = m.id;
    if (m.tone) chip.dataset.severity = m.tone;
    const elevated = m.key === 'health' || m.elevated;
    if (elevated) chip.dataset.elevated = '1';
    else foldedWhenCollapsed++;
    chip.textContent = m.text;
    chip.title = m.explainLines[0] || m.label;
    chip.setAttribute('aria-label', m.text ? `${m.label}: ${m.text}` : m.label);
    nodes.push(chip);
  }
  if (foldedWhenCollapsed > 0) {
    const more = document.createElement('button');
    more.type = 'button';
    more.className = 'vit-chip vit-overflow';
    more.dataset.chip = 'overflow';
    more.dataset.elevated = '1';
    more.textContent = `+${foldedWhenCollapsed}`;
    more.title = 'Show all vitals';
    more.setAttribute('aria-label', `${foldedWhenCollapsed} more vitals — show all`);
    nodes.push(more);
  }
  win.vitals.replaceChildren(...nodes);
  return models.some((m) => m.ticking);
}

// One delegated listener per window (chips are rebuilt every render).
// Chips are buttons, so the header's collapse-on-click guard already
// ignores them — stopPropagation only spares the focus/menu side effects.
function wireVitalsChipRow(win) {
  if (!win?.vitals || win.vitals.dataset.vitWired) return;
  win.vitals.dataset.vitWired = '1';
  win.vitals.addEventListener('click', (event) => {
    const chip = event.target.closest?.('.vit-chip');
    if (!chip) return;
    event.preventDefault();
    event.stopPropagation();
    const id = chip.dataset.chip || '';
    if (id === 'health' || id === 'overflow') openVitalsGlossary(win.sessionId, chip);
    else openVitalsExplainer(win.sessionId, id, chip);
  });
}

// ── Tap-to-explain: popover on fine pointers, bottom sheet on coarse /
// narrow — tooltips are mobile-hostile, so every symbol answers a tap.
function vitalsModelsForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  const meta = sessionMetadataById.get(sid) || {};
  return vitalsChipModels(meta.vitals || null, meta, sid);
}

function vitalsExplainerUsesSheet() {
  return window.matchMedia('(max-width: 720px)').matches
    || window.matchMedia('(pointer: coarse)').matches;
}

function ensureVitalsExplainerHost() {
  let host = document.getElementById('vitals-explainer');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'vitals-explainer';
  host.hidden = true;
  const backdrop = document.createElement('div');
  backdrop.className = 'vx-backdrop';
  backdrop.addEventListener('click', closeVitalsExplainer);
  const panel = document.createElement('div');
  panel.className = 'vx-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Vitals explanation');
  host.appendChild(backdrop);
  host.appendChild(panel);
  document.body.appendChild(host);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !host.hidden) closeVitalsExplainer();
  });
  // Capture phase: dismiss on any outside press without racing the
  // chip handlers (a chip tap re-opens with fresh content instead).
  document.addEventListener('pointerdown', (event) => {
    if (host.hidden) return;
    if (event.target.closest?.('#vitals-explainer .vx-panel, .vit-chip')) return;
    closeVitalsExplainer();
  }, true);
  return host;
}

function closeVitalsExplainer() {
  const host = document.getElementById('vitals-explainer');
  if (!host) return;
  host.hidden = true;
  host.classList.remove('sheet', 'popover');
}

function vxEl(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function vxHeader(label, valueText) {
  const head = vxEl('div', 'vx-head');
  head.appendChild(vxEl('span', 'vx-title', label));
  if (valueText) head.appendChild(vxEl('span', 'vx-value', valueText));
  return head;
}

function vxActionButton(action) {
  const btn = vxEl('button', 'vx-action', action.label);
  btn.type = 'button';
  btn.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    closeVitalsExplainer();
    try { action.run(); } catch (_) { /* action surfaces its own errors */ }
  });
  return btn;
}

function presentVitalsPanel(host, panel, anchor) {
  const sheet = vitalsExplainerUsesSheet();
  host.hidden = false;
  host.classList.toggle('sheet', sheet);
  host.classList.toggle('popover', !sheet);
  panel.style.left = '';
  panel.style.top = '';
  if (sheet || !anchor?.getBoundingClientRect) return;
  const rect = anchor.getBoundingClientRect();
  // Measure after content mount, then clamp inside the viewport;
  // prefer below the chip, flip above when there's no room.
  const pw = Math.min(panel.offsetWidth || 320, window.innerWidth - 16);
  const ph = panel.offsetHeight || 160;
  let left = Math.max(8, Math.min(rect.left, window.innerWidth - pw - 8));
  let top = rect.bottom + 6;
  if (top + ph > window.innerHeight - 8) top = Math.max(8, rect.top - ph - 6);
  panel.style.left = `${Math.round(left)}px`;
  panel.style.top = `${Math.round(top)}px`;
}

function openVitalsExplainer(sessionId, chipId, anchor) {
  const models = vitalsModelsForSession(sessionId);
  const model = models.find((m) => m.id === chipId);
  if (!model) {
    openVitalsGlossary(sessionId, anchor);
    return;
  }
  const host = ensureVitalsExplainerHost();
  const panel = host.querySelector('.vx-panel');
  panel.replaceChildren(
    vxHeader(model.label, model.text),
    ...model.explainLines.map((line) => vxEl('p', 'vx-line', line)),
    ...(model.action ? [vxActionButton(model.action)] : [])
  );
  presentVitalsPanel(host, panel, anchor);
}

// The glossary is the parity surface: EVERY catalog symbol is listed —
// present ones with their live value and sentence (tap to drill in),
// absent ones dimmed with "not reported", so what an agent lacks is
// visible instead of silently missing. The health dot opens this.
function openVitalsGlossary(sessionId, anchor) {
  const models = vitalsModelsForSession(sessionId);
  const health = models.find((m) => m.key === 'health');
  const host = ensureVitalsExplainerHost();
  const panel = host.querySelector('.vx-panel');
  const rows = [];
  // Quiet is not unavailable: a clean repo REPORTS zero dirty files. When
  // the symbol's family has data but this symbol is at rest, say so in
  // its own words; only families the agent never reported read as such.
  const hasGit = models.some((m) => VITALS_GIT_KEYS.has(m.key));
  const hasCache = models.some((m) => m.key === 'cache-hit' || m.key === 'cache-ttl');
  const familyPresent = (key) => (VITALS_GIT_KEYS.has(key) ? hasGit
    : key === 'cache-hit' || key === 'cache-ttl' ? hasCache
    : false);
  // Absent-family wording states who actually owns the reading: git is
  // probed by the daemon from the session's folder (agent-independent);
  // cache and limits come from the agent's provider responses.
  const familyAbsentText = (key) => (VITALS_GIT_KEYS.has(key)
    ? "No git reading for this session — its folder may not be a git project."
    : key === 'cache-hit' || key === 'cache-ttl'
      ? "Nothing measured yet — appears after the agent's first reply."
      : key === 'limit'
        ? 'No usage limits reported by this provider yet.'
        : 'Not reported for this session yet.');
  const addRow = (model, def, label, key) => {
    const row = vxEl('div', `vx-row ${model ? 'present' : 'absent'}`);
    row.appendChild(vxEl('span', 'vx-glyph', model ? (model.text || '●') : '—'));
    const body = vxEl('div', 'vx-body');
    body.appendChild(vxEl('div', 'vx-label', label || def.label));
    const absentText = familyPresent(key) && def.quiet
      ? def.quiet
      : (def.unavailable || familyAbsentText(key));
    body.appendChild(vxEl('div', 'vx-desc',
      model ? (model.explainLines[0] || '') : absentText));
    row.appendChild(body);
    if (model) {
      row.setAttribute('role', 'button');
      row.tabIndex = 0;
      const open = () => openVitalsExplainer(sessionId, model.id, anchor);
      row.addEventListener('click', open);
      row.addEventListener('keydown', (event) => {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); open(); }
      });
    }
    rows.push(row);
  };
  for (const key of VITALS_SYMBOL_ORDER) {
    if (key === 'health') continue;
    if (key === 'limit') {
      const limitModels = models.filter((m) => m.key === 'limit');
      if (!limitModels.length) addRow(null, VITALS_SYMBOLS.limit, 'Rate limits', key);
      for (const m of limitModels) addRow(m, VITALS_SYMBOLS.limit, `Rate limit ${m.id.slice('limit:'.length)}`, key);
      continue;
    }
    addRow(models.find((m) => m.key === key) || null, VITALS_SYMBOLS[key], undefined, key);
  }
  panel.replaceChildren(
    vxHeader('Session vitals',
      health?.severity === 'crit' ? 'needs attention' : health?.severity === 'warn' ? 'worth a look' : 'all good'),
    vxEl('p', 'vx-line', health?.explainLines[0] || ''),
    ...rows
  );
  presentVitalsPanel(host, panel, anchor);
}

function vitalsCopyText(text) {
  const value = String(text || '');
  const done = () => {
    if (typeof showControlToast === 'function') showControlToast('info', 'Copied');
  };
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(value).then(done).catch(() => {});
    return;
  }
  const scratch = document.createElement('textarea');
  scratch.value = value;
  document.body.appendChild(scratch);
  scratch.select();
  try { document.execCommand('copy'); done(); } catch (_) {}
  scratch.remove();
}

function vitalsOpenChangesTab() {
  const btn = document.querySelector('#activity-subtabs [data-activity-tab="changes"]');
  if (btn) btn.click();
}

// Vitals arrive keyed to the wrapper/log id and fan out through the
// identity group AS CACHED AT ARRIVAL (applySessionVitals). A window
// keyed by the backend-native id from birth missed every emission sent
// before the SessionIdentity linkage landed — and the change-only hub
// sends nothing again until something changes. Called from
// applySessionIdentity the moment the linkage is recorded. Sections that
// accumulated under different ids pre-linkage (git under the wrapper,
// usage under the native id) union here rather than first-found-wins.
function refanSessionVitalsForIdentityGroup(sessionId) {
  const ids = sessionGoalUpdateIds(sessionId);
  let union = null;
  for (const id of ids) {
    const cached = sessionMetadataById.get(id)?.vitals;
    if (cached) union = mergeSessionVitals(cached, union);
  }
  if (!union) return;
  for (const id of ids) {
    const meta = sessionMetadataById.get(id) || {};
    sessionMetadataById.set(id, { ...meta, vitals: union });
    if (sessionWindows.has(id)) {
      renderSessionWindowVitals(sessionWindows.get(id), union);
    }
  }
  refreshSessionVitalsTicker();
}

// QA readback (window.qa convention): the chip models a session renders,
// serializable — e2e probes assert backend parity on this.
window.qa = Object.assign(window.qa || {}, {
  vitalsChips: (sessionId) => vitalsModelsForSession(sessionId).map((m) => ({
    id: m.id,
    key: m.key,
    text: m.text,
    severity: m.severity,
    tone: m.tone,
    elevated: m.key === 'health' || m.elevated,
    ticking: m.ticking,
  })),
});

// The vitals conflict chip opens the same worktree finish/merge card the
// session-lifecycle flow renders (54-session-lifecycle.js — called by
// name at event time, never edited here). The card is normally gated by
// worktreeFinishCardDismissed; an explicit click re-requests it, so drop
// the dismissal first. When no worktree linkage resolves, fall back to
// routing to Sessions → Worktrees.
function openSessionWorktreeMergeCard(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const fallbackToWorktreesTab = () => {
    const branch = (sessionMetadataById.get(sid) || {}).worktree?.branch || '';
    if (typeof routeTo === 'function') routeTo('sessions', 'worktrees');
    if (typeof showControlToast === 'function') {
      showControlToast('info', branch
        ? `Worktree card unavailable — find branch ${branch} under Sessions → Worktrees`
        : 'Worktree card unavailable — see Sessions → Worktrees');
    }
  };
  const open = () => {
    const win = sessionWindows.get(sid);
    if (!win) {
      fallbackToWorktreesTab();
      return;
    }
    if (win.minimized && typeof setSessionWindowMinimized === 'function') {
      setSessionWindowMinimized(sid, false);
    }
    try { worktreeFinishCardDismissed.delete(sid); } catch (_) {}
    maybeShowWorktreeFinishCard(sid);
    sessionWindows.get(sid)?.worktreeCard?.scrollIntoView?.({ block: 'nearest' });
  };
  if (typeof maybeShowWorktreeFinishCard !== 'function' ||
      typeof hydrateSessionWorktreeFinishInfo !== 'function') {
    fallbackToWorktreesTab();
    return;
  }
  Promise.resolve(hydrateSessionWorktreeFinishInfo(sid))
    .then(info => { if (info) open(); else fallbackToWorktreesTab(); })
    .catch(() => fallbackToWorktreesTab());
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
  showCacheExpiryToast(sid, text);
  if (typeof Notification !== 'undefined' && Notification.permission === 'granted' && document.hidden) {
    try { new Notification('Intendant', { body: text }); } catch (_) { /* notification blocked */ }
  }
}

// Minimal keep-warm follow-up to a specific session, mirroring
// dispatchTaskText's start_task path (follow-up bookkeeping included) but
// pinned to the alerted session instead of the resolved prompt target.
// Deliberately spends one small model call — only ever sent on an
// explicit user click.
function sendCacheKeepWarmPing(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || !sessionWindows.has(sid)) return false;
  const text = 'ack (cache keep-warm)';
  const id = nextFollowUpId();
  const msg = { action: 'start_task', task: text, session_id: sid, follow_up_id: id };
  rememberPendingFollowUp(id, { sessionId: sid, text, direct: false, attachments: [] });
  onSteerStatusUpdate(id, text, 'pending', null, { sessionId: sid });
  if (!dispatchSessionControlMsg(msg)) return false;
  markSessionWindowPendingActive(sid);
  return true;
}

// Cache-expiry toast with an inline action. Mirrors showControlToast's
// DOM contract (single .control-toast, same host selection) because the
// shared helper is text-only; longer lifetime than the default 4.5s since
// this toast carries a button and references a sub-minute window.
function showCacheExpiryToast(sid, text) {
  const controlBody = document.querySelector('#activity-control-pane .control-pane-body');
  const host = controlBody && controlBody.offsetParent !== null ? controlBody : document.body;
  if (!host) return;
  host.querySelector('.control-toast')?.remove();
  const toast = document.createElement('div');
  toast.className = 'control-toast info';
  if (host === document.body) toast.classList.add('global-command-toast');
  const label = document.createElement('span');
  label.textContent = text;
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'ui-btn';
  btn.textContent = 'Send keep-warm ping';
  btn.style.marginLeft = '0.6em';
  btn.addEventListener('click', (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    toast.remove();
    if (sendCacheKeepWarmPing(sid)) {
      showControlToast('info', `Keep-warm ping sent to ${shortSessionId(sid)}`);
    } else {
      showControlToast('error', 'Keep-warm ping could not be sent');
    }
  });
  toast.appendChild(label);
  toast.appendChild(btn);
  host.appendChild(toast);
  setTimeout(() => { if (toast.parentNode) toast.remove(); }, 12000);
}

// Cheap per-tick predicate: does this session's vitals row need a 1 Hz
// repaint at all? True for the warm-cache countdown (the only `ticking`
// model vitalsChipModels emits) and for status-only rate-limit chips whose
// "resets in ~Xm" text is time-derived. Everything else changes only on
// real vitals/metadata events, which re-render directly — so the ticker no
// longer rebuilds every window's full chip-model array each second just to
// hit the stable-DOM fast path.
function sessionVitalsNeedsTick(vitals) {
  if (!vitals || typeof vitals !== 'object') return false;
  const remaining = sessionCacheCountdownSeconds(vitals.cache);
  if (remaining !== null && remaining > 0) return true;
  const limits = Array.isArray(vitals.limits) ? vitals.limits : [];
  return limits.some((w) => {
    // Mirrors the chip model: the reset countdown only renders when the
    // provider reports no percentage (status-only limits).
    const pctRaw = Number(w?.usedPct);
    const hasPct = Number.isFinite(pctRaw) && w?.usedPct !== null && w?.usedPct !== undefined;
    return !hasPct && Number(w?.resetsAtEpoch) > Date.now() / 1000;
  });
}

function refreshSessionVitalsTicker() {
  let ticking = false;
  for (const [sid, win] of sessionWindows) {
    const vitals = (sessionMetadataById.get(sid) || {}).vitals || null;
    const needsTick = sessionVitalsNeedsTick(vitals);
    // One trailing render after ticking stops (win.vitalsNeededTick holds
    // the previous pass's verdict) settles the row — the cache chip flips
    // to its cold glyph and a passed limit-reset drops its "↻~Xm" text —
    // instead of freezing mid-count.
    if (!needsTick && !win.vitalsNeededTick) continue;
    win.vitalsNeededTick = needsTick;
    renderSessionWindowVitals(win, vitals);
    // Arm from the predicate, not the render result: the render reports
    // only cache-ttl models as ticking, which left percentless limit-reset
    // countdowns frozen because nothing kept the interval alive for them.
    if (needsTick) ticking = true;
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

// Merge an arriving vitals snapshot over what a session already has. A
// missing section means "this emission's producer doesn't own it", not
// "the section went away": the daemon folds identity groups server-side
// now, but replayed logs and older daemons still emit git and cache
// under different ids of one session — wholesale overwrite made them
// blank each other (last writer won; the git family read "not reported"
// on every supervised session, 2026-07-15).
function mergeSessionVitals(incoming, existing) {
  if (!incoming) return existing || null;
  if (!existing) return incoming;
  return {
    git: incoming.git || existing.git || null,
    cache: incoming.cache || existing.cache || null,
    limits: incoming.limits?.length ? incoming.limits : existing.limits || [],
  };
}

function applySessionVitals(raw = {}) {
  const data = raw?.data && typeof raw.data === 'object' ? raw.data : raw;
  const sid = String(data?.session_id || data?.sessionId || '').trim();
  if (!sid) return;
  const vitals = normalizeSessionVitals(data);
  if (!vitals) return;
  for (const id of sessionGoalUpdateIds(sid)) {
    const meta = sessionMetadataById.get(id) || {};
    const merged = mergeSessionVitals(vitals, meta.vitals);
    sessionMetadataById.set(id, { ...meta, vitals: merged });
    if (sessionWindows.has(id)) {
      renderSessionWindowVitals(sessionWindows.get(id), merged);
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
    worktree: session.worktree,
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

function _foldSessionWindowMetadataRow(session, changedIds) {
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
    if (changed) changedIds.add(id);
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

// Full-corpus list applies fold thousands of rows; running that
// synchronously on every stream flush stalled the main thread. Small
// batches still fold inline (event-driven single-row refreshes stay
// immediate); large ones fold in idle-time slices. A batch arriving
// mid-fold queues behind it — latest wins, so every row is eventually
// folded with the newest data — and the DOM label refresh plus the
// persist run once per completed fold, not once per flush.
const SESSION_WINDOW_META_FOLD_SYNC_MAX = 400;
const SESSION_WINDOW_META_FOLD_CHUNK = 400;
let _sessionWindowMetaFold = null; // { rows, index, phase, changedIds, queued }

function cacheSessionWindowMetadata(sessions) {
  if (!Array.isArray(sessions)) return;
  if (sessions.length <= SESSION_WINDOW_META_FOLD_SYNC_MAX && !_sessionWindowMetaFold) {
    const changedIds = new Set();
    for (const session of sessions) {
      if (!session || typeof session !== 'object') continue;
      _foldSessionWindowMetadataRow(session, changedIds);
    }
    for (const session of sessions) {
      if (!session || typeof session !== 'object') continue;
      applySessionRelationshipsFromSession(session);
    }
    refreshSessionIdentityLabelsBulk(changedIds);
    persistSessionWindowState();
    return;
  }
  if (_sessionWindowMetaFold) {
    // Union-merge rather than replace: a small event-driven batch queued
    // behind an in-flight fold must not drop a queued full-corpus apply
    // (mergeSessionRows keeps newest-per-row).
    _sessionWindowMetaFold.queued = _sessionWindowMetaFold.queued
      ? mergeSessionRows(_sessionWindowMetaFold.queued, sessions)
      : sessions;
    return;
  }
  _sessionWindowMetaFold = { rows: sessions, index: 0, phase: 'meta', changedIds: new Set(), queued: null };
  _scheduleSessionWindowMetaFoldSlice();
}

function _scheduleSessionWindowMetaFoldSlice() {
  if (typeof requestIdleCallback === 'function') {
    requestIdleCallback(deadline => _runSessionWindowMetaFoldSlice(deadline), { timeout: 300 });
  } else {
    setTimeout(() => _runSessionWindowMetaFoldSlice(null), 16);
  }
}

function _runSessionWindowMetaFoldSlice(deadline) {
  const fold = _sessionWindowMetaFold;
  if (!fold) return;
  const hasBudget = () => deadline && typeof deadline.timeRemaining === 'function'
    ? deadline.timeRemaining() > 2
    : true;
  let processed = 0;
  while (processed < SESSION_WINDOW_META_FOLD_CHUNK && hasBudget()) {
    if (fold.index >= fold.rows.length) {
      if (fold.phase === 'meta') {
        fold.phase = 'relationships';
        fold.index = 0;
        continue;
      }
      _sessionWindowMetaFold = null;
      refreshSessionIdentityLabelsBulk(fold.changedIds);
      persistSessionWindowState();
      if (fold.queued) cacheSessionWindowMetadata(fold.queued);
      return;
    }
    const session = fold.rows[fold.index];
    fold.index += 1;
    if (!session || typeof session !== 'object') continue;
    if (fold.phase === 'meta') _foldSessionWindowMetadataRow(session, fold.changedIds);
    else applySessionRelationshipsFromSession(session);
    processed += 1;
  }
  _scheduleSessionWindowMetaFoldSlice();
}

// One DOM pass for a whole metadata batch. The per-id
// refreshSessionIdentityLabels (a document-wide querySelectorAll each)
// scanned the document once per changed id — a full-corpus list apply
// (~4.7k rows × ids) froze the tab for tens of seconds.
function refreshSessionIdentityLabelsBulk(changedIds) {
  if (!changedIds || changedIds.size === 0) return;
  const target = resolvePromptTargetSessionId();
  if (changedIds.has(target)) updateTaskTargetChip();
  document.querySelectorAll('.log-entry[data-session-id]').forEach(entry => {
    const sid = entry.dataset.sessionId;
    if (!changedIds.has(sid)) return;
    for (const child of entry.children) {
      if (child.classList?.contains('log-session')) {
        renderSessionIdentity(child, sid, { showName: false });
        applyPromptTargetLogSessionBadgeState(entry, target);
      }
    }
  });
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
  if (meta.worktree) record.worktree = meta.worktree;
  if (meta.parentId && meta.relationshipKind) {
    record.parent_session_id = meta.parentId;
    record.relationship = meta.relationshipKind;
    record.ephemeral = !!meta.relationshipEphemeral;
  }
  return record;
}

// Trailing-debounced twin for the streaming path. updateSessionWindow runs
// per rendered log line (inferSessionPhaseFromLog flips phase on model/agent
// lines) and used to re-derive every window's record, JSON.stringify it, and
// synchronously localStorage.setItem — tens of disk-backed writes per second
// under load, forever. Metadata churn now coalesces into at most one write
// per second; structural transitions (open/close/layout ops) keep calling
// the immediate form, and pagehide/hidden flush a pending write so a closing
// tab still persists its newest state.
let sessionWindowPersistTimer = 0;
function schedulePersistSessionWindowState() {
  if (restoringPersistedSessionWindows || sessionWindowPersistTimer) return;
  sessionWindowPersistTimer = window.setTimeout(() => {
    sessionWindowPersistTimer = 0;
    persistSessionWindowState();
  }, 1000);
}

function flushPendingSessionWindowPersist() {
  if (!sessionWindowPersistTimer) return;
  clearTimeout(sessionWindowPersistTimer);
  sessionWindowPersistTimer = 0;
  persistSessionWindowState();
}
window.addEventListener('pagehide', flushPendingSessionWindowPersist);
document.addEventListener('visibilitychange', () => {
  if (document.hidden) flushPendingSessionWindowPersist();
});

function persistSessionWindowState() {
  if (restoringPersistedSessionWindows) return;
  // A direct persist supersedes any scheduled one.
  if (sessionWindowPersistTimer) {
    clearTimeout(sessionWindowPersistTimer);
    sessionWindowPersistTimer = 0;
  }
  let windows = [];
  for (const [sid, win] of sessionWindows) {
    const record = persistedSessionWindowRecord(sid, win);
    if (record) windows.push(record);
  }
  // The cap drops records at PERSIST time, so the count of what was cut
  // rides the payload — restore has no other way to know it should tell
  // the user some windows will not come back.
  let dropped = 0;
  if (windows.length > SESSION_WINDOW_RESTORE_LIMIT) {
    const preferredIds = new Set([
      resolvePromptTargetSessionId(),
      currentSessionFullId,
      foregroundSessionFullId,
    ].filter(Boolean));
    const preferred = windows.filter(record => preferredIds.has(record.session_id));
    const rest = windows.filter(record => !preferredIds.has(record.session_id));
    dropped = windows.length - SESSION_WINDOW_RESTORE_LIMIT;
    windows = [...preferred, ...rest].slice(0, SESSION_WINDOW_RESTORE_LIMIT);
  }
  try {
    if (windows.length) {
      localStorage.setItem(
        SESSION_WINDOW_STATE_KEY,
        JSON.stringify(dropped > 0 ? { windows, dropped } : { windows })
      );
    } else {
      localStorage.removeItem(SESSION_WINDOW_STATE_KEY);
    }
  } catch (_) {}
}

function readPersistedSessionWindowDroppedCount() {
  try {
    const parsed = JSON.parse(localStorage.getItem(SESSION_WINDOW_STATE_KEY) || 'null');
    const n = Number(parsed?.dropped);
    return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
  } catch (_) {
    return 0;
  }
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
  const normalized = readPersistedSessionWindowState()
    .map(normalizePersistedSessionWindowRecord)
    .filter(Boolean);
  const records = normalized.slice(0, SESSION_WINDOW_RESTORE_LIMIT);
  const dropped = readPersistedSessionWindowDroppedCount() +
    Math.max(0, normalized.length - records.length);
  if (dropped > 0 && typeof showControlToast === 'function') {
    showControlToast(
      'info',
      `${dropped} session window${dropped === 1 ? '' : 's'} not restored — reopen them from Sessions`
    );
  }
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

// ── Named session-window layouts ───────────────────────────────────────────
// Contract: window.intendantLayouts = { save, apply, list, remove } — the
// command palette binds these verbs; keep the shape exact. Snapshots reuse
// the persisted session-window record shape, and apply() restores through
// the same code path as page-load restore (replace semantics: windows not
// in the layout are hidden first; open windows in the layout keep their
// streamed history).
const SESSION_WINDOW_LAYOUT_KEY_PREFIX = 'intendant.ui2.layout.';

function sessionWindowLayoutKey(name) {
  const trimmed = String(name || '').trim();
  return trimmed ? SESSION_WINDOW_LAYOUT_KEY_PREFIX + trimmed : '';
}

function saveSessionWindowLayout(name) {
  const key = sessionWindowLayoutKey(name);
  if (!key) return null;
  const windows = [];
  for (const [sid, win] of sessionWindows) {
    const record = persistedSessionWindowRecord(sid, win);
    if (record) windows.push(record);
  }
  try {
    localStorage.setItem(key, JSON.stringify({ windows, saved_at: new Date().toISOString() }));
  } catch (_) {
    return null;
  }
  return windows.length;
}

function readSessionWindowLayout(name) {
  const key = sessionWindowLayoutKey(name);
  if (!key) return null;
  try {
    const parsed = JSON.parse(localStorage.getItem(key) || 'null');
    return Array.isArray(parsed?.windows) ? parsed : null;
  } catch (_) {
    return null;
  }
}

async function applySessionWindowLayout(name) {
  const layout = readSessionWindowLayout(name);
  if (!layout) return null;
  const records = layout.windows
    .map(normalizePersistedSessionWindowRecord)
    .filter(Boolean);
  const keep = new Set(records.map(record => record.sessionId));
  for (const sid of Array.from(sessionWindows.keys())) {
    if (!keep.has(sid)) removeSessionWindow(sid);
  }
  if (!records.length) return 0;
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
  return records.length;
}

function listSessionWindowLayouts() {
  const layouts = [];
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key || !key.startsWith(SESSION_WINDOW_LAYOUT_KEY_PREFIX)) continue;
      const name = key.slice(SESSION_WINDOW_LAYOUT_KEY_PREFIX.length);
      if (!name) continue;
      const layout = readSessionWindowLayout(name);
      layouts.push({
        name,
        windows: Array.isArray(layout?.windows) ? layout.windows.length : 0,
        saved_at: layout?.saved_at || '',
      });
    }
  } catch (_) {}
  layouts.sort((a, b) => a.name.localeCompare(b.name));
  return layouts;
}

function removeSessionWindowLayout(name) {
  const key = sessionWindowLayoutKey(name);
  if (!key) return false;
  try {
    if (localStorage.getItem(key) === null) return false;
    localStorage.removeItem(key);
    return true;
  } catch (_) {
    return false;
  }
}

window.intendantLayouts = {
  save: (name) => saveSessionWindowLayout(name),
  apply: (name) => applySessionWindowLayout(name),
  list: () => listSessionWindowLayouts(),
  remove: (name) => removeSessionWindowLayout(name),
};

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
  if (event === 'session_note') {
    const noteText = String(entry.text || content || '').trim();
    if (!noteText) return null;
    return {
      ...base,
      level: 'info',
      source: source || 'note',
      content: noteText,
      kind: 'session_note',
      attachment_previews: sessionNoteAttachmentPreviews(entry),
    };
  }
  if (event === 'user_notification') {
    const command = userNotificationLogCommand(entry);
    if (!command) return null;
    return { ...base, ...command };
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
    // kind tool_call = command announcement (parity with the live WASM
    // path) — keeps replayed calls out of isCommandOutputLog's grouping.
    return { ...base, level: 'agent', source: source || 'agent', content, item_id: entry.item_id || entry.itemId || '', kind: kind || 'tool_call' };
  }
  if (event === 'agent_output') {
    const stdout = String(entry.stdout || '').trim();
    const stderr = String(entry.stderr || '').trim();
    content = stdout || stderr;
    if (!content) return null;
    return { ...base, level: stdout ? 'agent' : 'warn', source: source || 'agent', content, kind: kind || 'agent_output', output_id: entry.output_id || entry.outputId || '', item_id: entry.item_id || entry.itemId || '' };
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
  // The emptiness the caller checked can be stale by now: the fetch
  // awaited while the live replay/stream filled the window. Resetting
  // here wiped the streamed rows and re-rendered the fetched copy NEXT
  // TO their clones — merge into the streamed timeline instead.
  if (sessionWindowHasStreamedHistory(win)) {
    return appendMissingRestoredSessionWindowEntries(win, entries, fallbackSessionId);
  }
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
  // kind joins the item identity: a tool CALL row and its OUTPUT rows now
  // share the tool_use id (output attribution), and an id-only signature
  // would dedupe one against the other.
  if (parts.itemId) signatures.push(['item', parts.sessionId, parts.kind, parts.itemId].join('\u001f'));
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

// Per-item signature cache (log-append hot path). Computing a node item's
// signatures clones the node and walks it (sessionWindowTranscriptContentForElement),
// which made every streamed line O(retained history × cloneNode) through
// the full-history signature scans. Signatures are stable for an item's
// lifetime except for its session id, which retargets rewrite in place —
// so entries are keyed by the effective session id and recomputed when it
// changes. Node signatures ignore options (as before); record signatures
// cache per options variant ('near' = includeUserNearTime dedup passes).
// Item content mutations (live command-output summaries) intentionally do
// not invalidate: their identity signatures (event/output/item ids) are
// immutable and their summary-text signatures never matched replayed
// records in the first place.
const _sessionWindowItemSignatureCache = new WeakMap();

function sessionWindowTranscriptSignaturesForHistoryItem(item, fallbackSessionId = '', options = {}) {
  if (!item || typeof item !== 'object') return [];
  const node = sessionWindowHistoryNode(item);
  const record = node ? null : sessionWindowHistoryRecord(item);
  if (!node && !record) return [];
  const ownSid = node
    ? String(node.dataset?.sessionId || '').trim()
    : String(record.session_id || record.sessionId || '').trim();
  const sid = ownSid || String(fallbackSessionId || '').trim();
  const variant = node
    ? 'node'
    : (options.includeUserNearTime ? 'near' : (options.includeText === false ? 'noText' : 'def'));
  let cached = _sessionWindowItemSignatureCache.get(item);
  if (cached && cached.sid === sid && cached[variant]) return cached[variant];
  if (!cached || cached.sid !== sid) {
    cached = { sid };
    _sessionWindowItemSignatureCache.set(item, cached);
  }
  const signatures = node
    ? sessionWindowTranscriptSignaturesForNode(node, fallbackSessionId)
    : sessionWindowTranscriptSignaturesForRecord(record, fallbackSessionId, options);
  cached[variant] = signatures;
  return signatures;
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
  // Fractional seconds ride the session log's HH:MM:SS.mmm stamps — an
  // unmatched fraction made every replayed row timestamp-less, which
  // broke ordered merges (hydration landed as a tail block).
  let match = raw.match(/^(\d{1,2}):(\d{2})(?::(\d{2})(?:\.(\d{1,9}))?)?$/);
  if (match) {
    const date = new Date();
    const fraction = match[4] ? Number(('0.' + match[4])) * 1000 : 0;
    date.setHours(Number(match[1]), Number(match[2]), Number(match[3] || 0), Math.round(fraction));
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

// Timestamp lookups for the insert-position scan cost a querySelector +
// parse per node item; an item's timestamp never changes once built, so
// cache it for the item's lifetime.
const _sessionWindowItemTimestampCache = new WeakMap();

function sessionWindowTranscriptTimestampForHistoryItemCached(item) {
  if (!item || typeof item !== 'object') return sessionWindowTranscriptTimestampForHistoryItem(item);
  if (_sessionWindowItemTimestampCache.has(item)) return _sessionWindowItemTimestampCache.get(item);
  const ts = sessionWindowTranscriptTimestampForHistoryItem(item);
  _sessionWindowItemTimestampCache.set(item, ts);
  return ts;
}

function insertSessionWindowHistoryRecords(win, records, shouldFollow) {
  if (!win || !Array.isArray(records) || records.length === 0) return;
  deduplicateSessionWindowHistory(win, shouldFollow);
  const history = ensureSessionWindowHistory(win);
  const wasRenderingTail = sessionWindowIsRenderingTail(win, history.length);
  const signatures = sessionWindowHistorySignatureSet(win, win.sessionId);
  let inserted = 0;
  // Monotonic scan boundary: every item before the previous insertion
  // point has a timestamp <= the previous record's (that is why the
  // previous scan passed it), so an ascending record can resume there —
  // placement is identical to the full scan. A descending record (or one
  // following a timestamp-less insert) restarts from 0.
  let scanFrom = 0;
  let lastInsertedTs = null;
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
      const from = lastInsertedTs !== null && recordTs >= lastInsertedTs ? scanFrom : 0;
      for (let i = from; i < history.length; i++) {
        const itemTs = sessionWindowTranscriptTimestampForHistoryItemCached(history[i]);
        if (itemTs !== null && itemTs > recordTs) {
          index = i;
          break;
        }
      }
    }
    history.splice(index, 0, item);
    addSessionWindowHistorySignatures(signatures, item, win.sessionId);
    if (recordTs !== null) {
      scanFrom = index + 1;
      lastInsertedTs = recordTs;
    }
    inserted += 1;
  }
  if (inserted === 0) return;
  commitSessionWindowHistorySignatureAppend(win, signatures);
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

// Hydration outcome flag (contract with the session-window placeholder
// renderer): win.hydrateError distinguishes "hydration failed" from
// "genuinely empty". Set on failure, cleared on any successful hydration
// pass; the signature reset forces updateSessionWindow past its
// no-metadata-change early return so renderers can react immediately.
function setSessionWindowHydrateError(win, err) {
  if (!win) return;
  const message = String(err?.message || err || 'hydration failed');
  if (win.hydrateError === message) return;
  win.hydrateError = message;
  win.metadataSignature = '';
  updateSessionWindow(win.sessionId, {});
}

function clearSessionWindowHydrateError(win) {
  if (!win || !win.hydrateError) return;
  win.hydrateError = '';
  win.metadataSignature = '';
  updateSessionWindow(win.sessionId, {});
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
    clearSessionWindowHydrateError(targetWin || win);
    if (rendered > 0) {
      updateSessionWindow(targetSid, { phase: 'idle', ended: false });
      stationScheduleUpdate();
    }
  } catch (err) {
    console.warn('Failed to hydrate restored session window', sid, err);
    setSessionWindowHydrateError(
      sessionWindows.get(sessionWindowTargetForLogSession(sid) || sid) || win,
      err
    );
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
      // daemonApi (transport F2): tunnel first, direct HTTP per the
      // GET-twin fallback policy (cache posture preserved).
      const resp = await daemonApi.request('api_sessions', { ids: [sid] }, { cache: 'no-store' });
      if (resp.ok) {
        cacheSessionWindowMetadata(resp.body);
      }
      record = sessionWindowHydrationRecord(sid);
    }
    // No hydration record is NOT an error: internal sessions (source
    // 'intendant' normalizes to '') are filled by the live stream, never
    // by this path — only real fetch failures set the flag.
    if (record) await hydrateRestoredSessionWindow(win, record);
  } catch (err) {
    console.warn('Failed to hydrate session window', sid, err);
    setSessionWindowHydrateError(sessionWindows.get(sid) || win, err);
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

// Forever-failing polls must not be silent, but they must stay quiet
// (no toast spam): warn once per cause on the console and flag the
// affected windows (win.metaStale) so placeholder logic can distinguish
// "no data yet" from "polling is failing". Recovery clears the flags and
// re-arms the warn for the next failure streak.
const _sessionPollFailureWarned = new Set();

function noteSessionMetadataPollFailure(cause, err, sessionIds = null) {
  if (!_sessionPollFailureWarned.has(cause)) {
    _sessionPollFailureWarned.add(cause);
    console.warn(`[dashboard] ${cause} failing; retrying quietly (warned once per streak)`, err);
  }
  const ids = Array.isArray(sessionIds) ? sessionIds : Array.from(sessionWindows.keys());
  for (const id of ids) {
    const win = sessionWindows.get(String(id || '').trim());
    if (win) win.metaStale = true;
  }
}

function noteSessionMetadataPollRecovery(cause, sessionIds = null) {
  _sessionPollFailureWarned.delete(cause);
  const ids = Array.isArray(sessionIds) ? sessionIds : Array.from(sessionWindows.keys());
  for (const id of ids) {
    const win = sessionWindows.get(String(id || '').trim());
    if (win && win.metaStale) win.metaStale = false;
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
      // daemonApi (transport F2): tunnel first, direct HTTP per the
      // GET-twin fallback policy (`url` survives as the error label).
      const resp = await daemonApi.request('api_sessions', ids.length ? { ids } : { limit: 'all' });
      if (!resp.ok) throw new Error(`${url} returned ${resp.status}`);
      return resp.body;
    };
    sessionMetadataRefreshInFlight = loadMetadata()
      .then(sessions => {
        cacheSessionWindowMetadata(sessions);
        noteSessionMetadataPollRecovery('session metadata poll');
      })
      .catch(err => noteSessionMetadataPollFailure('session metadata poll', err))
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
      // A pruned command-output group must leave the strong registry too:
      // commandOutputGroups otherwise pins the detached entry DOM, its
      // clone views, and the full accumulated copy text for the lifetime
      // of the page (clearLogs was the only eviction). Live clones keep
      // the group reachable through their own click closures.
      const groupId = first.dataset ? first.dataset.outputGroupId : '';
      if (groupId) commandOutputGroups.delete(groupId);
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
  // ui-v2 Focus layout: the combined stream IS the visible surface (the
  // grid is CSS-hidden without touching its .hidden class) — never park
  // the stream in the detached fragment there, or Focus shows nothing.
  const root = document.documentElement;
  if (root.dataset.ui2Layout !== 'grid') return false;
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

// Async twin for capped copy refs: a command-output group whose streamed
// text outgrew COMMAND_OUTPUT_COPY_TEXT_CAP carries a fetchText resolver
// (the persisted-output lane) instead of retaining megabytes in the ref.
// Falls back to the captured prefix if the fetch fails.
async function resolveLogEntryCopyText(entry) {
  const ref = entry ? logEntryCopyTextByEntry.get(entry) : null;
  if (ref && typeof ref.fetchText === 'function') {
    try {
      const text = await ref.fetchText();
      if (text) return String(text);
    } catch (err) {
      console.warn('lazy copy-text fetch failed; copying captured prefix', err);
    }
  }
  return getLogEntryCopyText(entry);
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
    await copyTextToClipboard(await resolveLogEntryCopyText(entry));
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
  const command = String(meta.agentCommand || meta.agent_command || '').trim();
  if (command) return command;
  // The codex_command fields are codex-only; for any other source they can
  // only be cross-session contamination, never this session's binary.
  if (sessionConfigSource(meta) !== 'codex') return '';
  return String(
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

