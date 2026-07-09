// ── Station diff viewer ──
// Changed-file rows feed the same viewer in diff mode (kind-colored
// +/-/@@ lines); diffs are static, so no live refresh.
function stationDiffRows(diffText) {
  return String(diffText || '').split('\n').slice(0, 1500).map(line => ({
    kind: line.startsWith('+') && !line.startsWith('+++')
      ? 'diff-add'
      : line.startsWith('-') && !line.startsWith('---')
        ? 'diff-del'
        : (line.startsWith('@@') || line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff '))
          ? 'diff-meta'
          : 'info',
    ts: '',
    text: line || ' ',
  }));
}

async function stationOpenDiff(path) {
  const p = String(path || '').trim();
  if (!p || !station || typeof station.set_transcript !== 'function') return;
  stationStatus(`Loading diff ${p}`);
  let rows = [];
  let error = '';
  try {
    const resp = await fetchChangesResponse(p);
    const data = await parseChangesResponse(resp);
    if (data.diff_available === false) {
      error = data.reason || 'Textual diff is unavailable for this file.';
    } else {
      rows = stationDiffRows(String(data.diff || '').trimEnd());
      if (!rows.length || (rows.length === 1 && !rows[0].text.trim())) {
        error = 'No text diff for this file.';
      }
    }
  } catch (e) {
    error = `diff fetch failed: ${e?.message || e}`;
  }
  try {
    station.set_transcript({
      sessionId: p,
      source: 'diff',
      label: p,
      mode: 'diff',
      refresh: false,
      error,
      total: rows.length,
      rows: error ? [] : rows,
    });
  } catch (err) {
    console.warn('station diff viewer rejected:', err);
  }
  stationTranscriptLive = null;
}

function stationSetActivityVerbosity(level) {
  const next = applyDashboardVerbosity(level, { dispatch: true });
  if (!next) return;
  if (typeof showControlToast === 'function') {
    showControlToast('success', `Activity log verbosity: ${next}`);
  }
}

function applyDashboardVerbosity(level, options = {}) {
  const next = String(level || '').trim();
  if (!['normal', 'verbose', 'debug'].includes(next)) return '';
  const select = document.getElementById('verbosity-select');
  if (select) select.value = next;
  if (app) processCommands(app.set_verbosity(next));
  localStorage.setItem(VERBOSITY_KEY, next);
  if (options.dispatch !== false) {
    dispatchControlMsg({ action: 'set_verbosity', level: next });
  }
  return next;
}

function stationClearActivityHostFilter() {
  activeHostFilter = '';
  stationActivityDockHost = '';
  const select = document.getElementById('host-filter-select');
  if (select) select.value = '';
  localStorage.setItem(HOST_FILTER_KEY, activeHostFilter);
  applyHostFilter();
  if (typeof showControlToast === 'function') {
    showControlToast('success', 'Activity host filter cleared');
  }
}

function stationHandleChangesAction(action) {
  const op = String(action.action || 'file').trim();
  const path = String(action.path || action.id || '').trim();
  if (op === 'station-diff') {
    stationOpenDiff(path);
    return;
  }
  if (op === 'refresh') {
    if (typeof refreshHistory === 'function') refreshHistory();
    refreshChangesList({ selectFirst: true, refreshActive: true, quiet: true })
      .finally(() => stationOpenPanel('system:changes', 'Changes refreshed'));
    return;
  }
  if (op === 'copy-paths') {
    stationCopyChangedPaths();
    return;
  }
  if (op === 'copy-diff') {
    stationCopyChangeDiff(path || activeChangesFile || stationSelectedChangePath)
      .catch(err => showControlToast?.('error', `Copy diff failed: ${err?.message || err}`));
    return;
  }
  if (op === 'history') {
    stationRefreshChangesHistory(path || activeChangesFile || stationSelectedChangePath);
    return;
  }
  if (op === 'redo') {
    stationRunChangesRedo(activeChangesFile || path);
    return;
  }
  if (op === 'prune') {
    stationRunChangesPrune(activeChangesFile || path);
    return;
  }
  if (op !== 'file' || !path) {
    refreshChangesList({ selectFirst: true, refreshActive: true, quiet: true })
      .finally(() => stationOpenPanel('system:changes', 'Changes refreshed'));
    return;
  }
  stationSelectChange(path);
  stationOpenPanel('system:changes', 'Change selected');
}

function focusActivityLogEvent(id) {
  const escaped = stationCssEscape(id);
  let entry = document.querySelector(`.log-entry[data-station-event-id="${escaped}"]`);
  if (!entry) entry = document.querySelector(`.log-entry[data-item-id="${escaped}"]`);
  if (!entry) return;
  entry.scrollIntoView({ block: 'center', behavior: 'smooth' });
  entry.classList.remove('station-focus');
  void entry.offsetWidth;
  entry.classList.add('station-focus');
  setTimeout(() => entry.classList.remove('station-focus'), 1800);
}

function stationOpenDisplay(hostId, displayId = 0) {
  const d = daemons.find(x => x.host_id === hostId);
  if (d) {
    const did = Number.isFinite(Number(displayId)) && Number(displayId) >= 0
      ? Math.trunc(Number(displayId))
      : 0;
    stationSetPeerTarget(hostId, did);
    const tcpViaUrl = resolveBrowserTcpViaUrl(d);
    openPeerDisplay(hostId, did, tcpViaUrl).catch(err => {
      console.error('Station openPeerDisplay failed:', err);
      stationStatus('Display open failed: ' + (err && err.message ? err.message : err));
    });
    return;
  }
  if (!hostId || hostId === selfPeerId || hostId === 'local') {
    if (displaySlots.size === 0 && !userDisplayGranted) {
      window.toggleUserDisplay();
    }
    return;
  }
  // Unknown peer id (stale snapshot, peer removed mid-click): tell the
  // operator instead of silently doing nothing.
  showControlToast?.('error', `Unknown peer ${hostId}; cannot open its display`);
  stationStatus(`Display open failed: unknown peer ${hostId}`);
}

function stationSnapshotForContext() {
  if (typeof contextSnapshotForForegroundSession === 'function') {
    return contextSnapshotForForegroundSession();
  }
  return latestContextSnapshot || null;
}

function stationSessionIdentitySummary(sessionId = '') {
  const sid = String(sessionId || '').trim();
  const session = sid ? stationFindSessionById(sid) : null;
  const meta = sid ? sessionConfigMetadata(session || sid) : {};
  const backendId = managedContextBackendSessionId(meta);
  const intendantId = managedContextIntendantSessionId(meta);
  const source = sessionConfigSource(meta) || stationSessionSource(session) || managedContextCanonicalSource(meta) || '';
  const sourceLabel = managedContextSourceLabel(meta) || prettyAgentName(source) || source || '';
  return {
    sessionId: sid,
    sessionLabel: stationSessionShortLabel(sid) || stationSessionTask(session) || shortSessionId(sid) || '',
    backendSource: source || '',
    backendLabel: sourceLabel || '',
    backendSessionId: backendId || '',
    intendantSessionId: intendantId || '',
    managedMode: sessionConfigManagedMode(meta) || managedContextConfiguredMode(meta) || '',
    contextArchive: sessionConfigArchiveMode(meta) || '',
  };
}

function stationBuildContextSummary() {
  const snapshot = stationSnapshotForContext();
  const replay = stationContextTimelineState();
  if (!snapshot) {
    const identity = stationSessionIdentitySummary(resolvePromptTargetSessionId?.() || stationManagedSessionCandidate?.() || '');
    return {
      available: false,
      label: '',
      source: '',
      sessionId: identity.sessionId || '',
      sessionLabel: identity.sessionLabel || '',
      backendSource: identity.backendSource || '',
      backendLabel: identity.backendLabel || '',
      backendSessionId: identity.backendSessionId || '',
      intendantSessionId: identity.intendantSessionId || '',
      managedMode: identity.managedMode || '',
      contextArchive: identity.contextArchive || '',
      format: '',
      turn: '',
      tokens: 0,
      effectiveWindow: 0,
      hardWindow: 0,
      itemCount: 0,
      categoryCount: 0,
      topCategories: [],
      topItems: [],
      replayMode: contextReplayMode || 'live',
      replayCount: replay.timeline.length,
      replayIndex: replay.timeline.length ? replay.index + 1 : 0,
      replayTime: '',
      exactStatus: 'none',
      pressureState: {},
    };
  }
  const analysis = analyzeContextSnapshotCached(snapshot);
  const effectiveWindow = Number(snapshot.effective_context_window || snapshot.context_window || analysis.effectiveWindow || 0);
  const hardWindow = Number(snapshot.hard_context_window || snapshot.hardContextWindow || analysis.hardWindow || effectiveWindow || 0);
  const sessionId = contextSessionKey(snapshot) === CONTEXT_GLOBAL_SESSION ? '' : contextSessionKey(snapshot);
  const identity = stationSessionIdentitySummary(sessionId || resolvePromptTargetSessionId?.() || '');
  // Slim wire rows: the context focus panel renders label/value/count/
  // detail for categories and label/value/detail/tone for items.
  const categories = Array.from(analysis.byCategory.entries())
    .map(([category, stats]) => {
      const def = CONTEXT_CATEGORY_DEFS[category] || CONTEXT_CATEGORY_DEFS.other;
      const largest = analysis.parts
        .filter(part => part.category === category)
        .sort((a, b) => (b.tokens || 0) - (a.tokens || 0))[0] || null;
      return {
        label: def.label,
        value: stationNum(stats.tokens),
        count: stationNum(stats.count),
        detail: largest
          ? `Largest: ${compactSessionText(largest.title || largest.subtitle || largest.path || largest.id || '')} (${stationCompactNumber(largest.tokens || largest.estimatedTokens || 0)} tok)`
          : `${stationCompactNumber(stats.count)} items`,
      };
    })
    .sort((a, b) => b.value - a.value)
    .slice(0, 3);
  const topItems = [...analysis.parts]
    .sort((a, b) => (b.tokens || 0) - (a.tokens || 0))
    .slice(0, 4)
    .map(part => {
      const def = CONTEXT_CATEGORY_DEFS[part.category] || CONTEXT_CATEGORY_DEFS.other;
      return {
        label: def.label,
        value: `${stationCompactNumber(part.tokens || part.estimatedTokens || 0)} tok`,
        detail: compactSessionText(part.title || part.subtitle || part.path || ''),
        tone: 'context',
      };
    });
  const pressurePct = effectiveWindow ? managedContextPercent(analysis.totalTokens || snapshot.token_count, effectiveWindow) : null;
  const pressureState = {
    id: sessionId || 'context',
    sessionId,
    action: 'pressure',
    label: pressurePct === null ? 'context pressure' : `${pressurePct.toFixed(1)}% context`,
    value: `${stationCompactNumber(analysis.totalTokens || snapshot.token_count)} / ${stationCompactNumber(effectiveWindow)} tokens`,
    detail: [
      categories[0]?.label ? `top ${categories[0].label}` : '',
      stationContextExactStatus(replay.snapshot || snapshot),
      replay.timeline.length ? `replay ${replay.index + 1}/${replay.timeline.length}` : '',
    ].filter(Boolean).join(' / '),
    tone: pressurePct !== null && pressurePct >= 90 ? 'warning' : 'context',
  };
  return {
    available: true,
    label: snapshot.label || snapshot.source || 'model',
    source: snapshot.label || snapshot.source || 'model',
    sessionId,
    sessionLabel: identity.sessionLabel || '',
    backendSource: identity.backendSource || '',
    backendLabel: identity.backendLabel || '',
    backendSessionId: identity.backendSessionId || '',
    intendantSessionId: identity.intendantSessionId || '',
    managedMode: identity.managedMode || '',
    contextArchive: identity.contextArchive || '',
    format: snapshot.format || '',
    turn: contextSnapshotRequestLabel(snapshot),
    tokens: stationNum(analysis.totalTokens || snapshot.token_count),
    effectiveWindow,
    hardWindow,
    itemCount: stationNum(snapshot.item_count || analysis.parts.length),
    categoryCount: stationNum(analysis.byCategory.size),
    topCategories: categories,
    topItems,
    replayMode: contextReplayMode || 'live',
    replayCount: replay.timeline.length,
    replayIndex: replay.timeline.length ? replay.index + 1 : 0,
    replayTime: formatContextTimestamp(replay.snapshot?.ts),
    exactStatus: stationContextExactStatus(replay.snapshot || snapshot),
    pressureState,
  };
}

function stationManagedSessionCandidate() {
  const selected = managedContextCurrentSessionId();
  if (selected) {
    const selectedMeta = managedContextSessionMeta(selected);
    if (managedContextSessionIsLive(selected, selectedMeta)) return selected;
  }
  const options = managedContextSessionOptions();
  const liveCodex = options.find(([sid, meta]) =>
    managedContextSessionIsLive(sid, meta) && managedContextIsCodexLike(meta)
  );
  if (liveCodex) return liveCodex[0];
  if (selected) return selected;
  if (options.length) return options[0][0];
  return resolvePromptTargetSessionId();
}

function stationAnchorSessionMatches(sid, target) {
  if (!target) return true;
  return sid === target || sessionWindowTargetForLogSession(sid) === target;
}

function stationManagedAnchorCount(sessionId) {
  const target = String(sessionId || '').trim();
  if (!target) {
    let count = stationAnchorIdRefs.size;
    for (const anchor of managedContextAnchors || []) {
      const id = String(anchor?.item_id || anchor?.itemId || '').trim();
      if (id && !stationAnchorIdRefs.has(id)) count += 1;
    }
    return count;
  }
  const seen = new Set();
  for (const anchor of managedContextAnchors || []) {
    const id = anchor?.item_id || anchor?.itemId;
    if (id) seen.add(String(id));
  }
  for (const [sid, entry] of stationAnchorsBySession) {
    if (!stationAnchorSessionMatches(sid, target)) continue;
    for (const id of entry.ids) seen.add(id);
  }
  return seen.size;
}

function stationManagedRecordRows() {
  return [...(managedContextRecords || [])]
    .sort((a, b) => stationTimestampMs(b.created_at) - stationTimestampMs(a.created_at))
    .slice(0, 8)
    .map(record => ({
      id: record.record_id || '',
      sessionId: record.session_id || record.sessionId || record.intendant_session_id || record.intendantSessionId || stationManagedSessionCandidate() || '',
      action: 'record',
      label: record.record_id || 'record',
      value: `${record.position || 'after'} ${record.item_id || ''}`.trim(),
      detail: compactSessionText(record.reason || record.primer || ''),
      tone: 'managed',
    }));
}

function stationManagedLedgerSummary() {
  const lineage = Array.isArray(managedContextStatus?.lineage_ledger?.groups)
    ? managedContextStatus.lineage_ledger.groups
    : [];
  const fission = Array.isArray(managedContextStatus?.fission_ledger?.groups)
    ? managedContextStatus.fission_ledger.groups
    : [];
  let branches = 0;
  for (const group of [...lineage, ...fission]) {
    const groupBranches = Array.isArray(group?.branches) ? group.branches : [];
    branches += groupBranches.length;
  }
  return {
    lineageGroups: lineage.length,
    fissionGroups: fission.length,
    branches,
  };
}

function stationBuildManagedSummary(contextSummary) {
  const sessionId = stationManagedSessionCandidate();
  const meta = managedContextSessionMeta(sessionId);
  const identity = stationSessionIdentitySummary(sessionId);
  const live = managedContextSessionIsLive(sessionId, meta);
  const pressure = live ? (managedContextStatus?.context_pressure || {}) : {};
  const ledgers = stationManagedLedgerSummary();
  const effectiveWindow = stationNum(
    pressure.effective_context_window ||
    pressure.context_window ||
    contextSummary.effectiveWindow
  );
  const hardWindow = stationNum(
    pressure.hard_limit ||
    pressure.hard_context_window ||
    contextSummary.hardWindow ||
    effectiveWindow
  );
  const usedTokens = stationNum(pressure.used_tokens || contextSummary.tokens);
  const rewindOnlyLimit = live ? managedContextPositiveNumber(pressure.rewind_only_limit) : null;
  const requiredAction = String(pressure.required_action || '').trim();
  const remainingToRewindOnly = rewindOnlyLimit === null ? null : Math.max(0, rewindOnlyLimit - usedTokens);
  const mode = sessionId
    ? managedContextEffectiveMode(sessionId, meta)
    : (controlCodexConfig.managed_context || 'vanilla');
  const recordRows = stationManagedRecordRows();
  const pressureState = {
    id: sessionId || 'managed-context',
    sessionId: sessionId || '',
    action: 'pressure',
    label: pressure.rewind_only ? 'rewind-only pressure' : (requiredAction || pressure.status || 'context pressure'),
    value: `${stationCompactNumber(usedTokens)} / ${stationCompactNumber(effectiveWindow)} effective`,
    detail: [
      hardWindow ? `${stationCompactNumber(usedTokens)} / ${stationCompactNumber(hardWindow)} hard` : '',
      remainingToRewindOnly !== null ? `${stationCompactNumber(remainingToRewindOnly)} to rewind-only` : '',
      requiredAction && requiredAction !== pressure.status ? requiredAction : '',
    ].filter(Boolean).join(' / '),
    tone: pressure.rewind_only ? 'red' : ((pressure.status === 'watch' || pressure.status === 'high' || pressure.status === 'critical') ? 'warning' : 'managed'),
  };
  const latestRewind = recordRows[0] || {};
  const latestBackout = summaryBackoutRow(stationManagedDraftValue('record', 'managed-context-record-id', '') || latestRewind.id || '', latestRewind, sessionId);
  const summary = {
    sessionId: sessionId || '',
    sessionLabel: identity.sessionLabel || '',
    backendSource: identity.backendSource || '',
    backendLabel: identity.backendLabel || '',
    backendSessionId: identity.backendSessionId || '',
    intendantSessionId: identity.intendantSessionId || '',
    contextArchive: identity.contextArchive || '',
    configuredMode: identity.managedMode || '',
    live,
    mode,
    status: live ? (pressure.status || 'unknown') : (sessionId ? 'historical' : 'unknown'),
    usedTokens,
    effectiveWindow,
    hardWindow,
    effectivePct: managedContextPercent(usedTokens, effectiveWindow),
    hardPct: managedContextPercent(usedTokens, hardWindow),
    rewindOnlyLimit,
    remainingToRewindOnly,
    rewindOnly: !!pressure.rewind_only,
    records: Array.isArray(managedContextRecords) ? managedContextRecords.length : 0,
    anchors: stationManagedAnchorCount(sessionId),
    lineageGroups: ledgers.lineageGroups,
    fissionGroups: ledgers.fissionGroups,
    branches: ledgers.branches,
    error: managedContextLastError || '',
    pressureState,
    latestRewind,
    latestBackout,
    // Actionable wire rows: the managed focus panel scrolls these and
    // the inspect/fork/restore pills act on the record ids. (Anchor and
    // branch row arrays were serialized but never rendered; their counts
    // above remain.)
    recentRecords: recordRows.slice(0, 12).map(row => ({
      ...stationSlimDetailRow(row),
      id: String(row?.id || ''),
    })),
    activitySignal: stationManagedActivitySignal || {},
  };
  const ready = stationManagedReadiness(summary);
  summary.actionState = {
    anchor: ready.anchor || '',
    record: ready.record || '',
    position: stationManagedDraftValue('position', 'managed-context-position', 'after') || 'after',
    backoutMode: stationManagedDraftValue('backoutMode', 'managed-context-backout-mode', 'inspect') || 'inspect',
    readiness: ready.base || ready.rewindTitle || ready.backoutTitle || 'ready',
    result: stationManagedResultText(),
    hasReason: !!ready.reason,
    hasPrimer: !!ready.primer,
    canInspect: !!ready.canInspect,
    canRewind: !!ready.canRewind,
    canBackout: !!ready.canBackout,
  };
  return summary;
}

function summaryBackoutRow(recordId, latestRewind, sessionId) {
  const id = String(recordId || '').trim();
  if (!id) return {};
  if (latestRewind?.id === id) return { ...latestRewind, action: 'backout', value: latestRewind.value || 'backout record' };
  return {
    id,
    sessionId: sessionId || '',
    action: 'backout',
    label: id,
    value: 'backout record',
    detail: 'selected managed rewind record',
    tone: 'managed',
  };
}

function stationChangesKindLabel(kind) {
  const value = String(kind || 'modified').trim();
  if (value === 'created') return 'added';
  if (value === 'deleted') return 'deleted';
  if (value === 'external') return 'external';
  return 'modified';
}

function stationChangesTone(kind) {
  const value = String(kind || 'modified').trim();
  if (value === 'created') return 'ok';
  if (value === 'deleted') return 'red';
  if (value === 'external') return 'warning';
  return 'changes';
}

function stationChangesFileName(path) {
  const parts = String(path || '').split(/[\\/]+/).filter(Boolean);
  return parts.pop() || path || 'file';
}

// Slim wire rows: the changes focus panel renders label/value/detail/tone.
function stationChangesRows(entries) {
  return entries.slice(0, 30).map(([path, info]) => {
    const kind = String(info?.kind || 'modified');
    const added = stationNum(info?.lines_added);
    const removed = stationNum(info?.lines_removed);
    const hasTextDiff = info?.diff_available !== false;
    return {
      id: String(path || ''),
      label: stationChangesFileName(path),
      value: hasTextDiff ? `+${stationCompactNumber(added)} -${stationCompactNumber(removed)}` : 'no text diff',
      detail: compactPathLabel(path, true) || stationChangesKindLabel(kind),
      tone: stationChangesTone(kind),
    };
  });
}

function stationBuildChangesSummary() {
  const mismatch = currentChangesTargetMismatchReason();
  if (mismatch) {
    return {
      status: 'mismatch',
      count: 0,
      added: 0,
      modified: 0,
      deleted: 0,
      external: 0,
      totalAdded: 0,
      totalRemoved: 0,
      latestPath: mismatch,
      latestKind: 'mismatch',
      recent: [{
        label: 'target mismatch',
        value: 'check root',
        detail: compactSessionText(mismatch),
        tone: 'warning',
      }],
    };
  }

  const entries = [...changedFiles.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  let added = 0;
  let modified = 0;
  let deleted = 0;
  let external = 0;
  let totalAdded = 0;
  let totalRemoved = 0;
  for (const [, info] of entries) {
    const kind = String(info?.kind || 'modified');
    if (kind === 'created') added += 1;
    else if (kind === 'deleted') deleted += 1;
    else if (kind === 'external') external += 1;
    else modified += 1;
    totalAdded += stationNum(info?.lines_added);
    totalRemoved += stationNum(info?.lines_removed);
  }

  const activeEntry = activeChangesFile && changedFiles.has(activeChangesFile)
    ? [activeChangesFile, changedFiles.get(activeChangesFile)]
    : null;
  const latest = activeEntry || entries[0] || null;
  return {
    status: entries.length ? 'dirty' : 'clean',
    count: entries.length,
    added,
    modified,
    deleted,
    external,
    totalAdded,
    totalRemoved,
    latestPath: latest ? latest[0] : '',
    latestKind: latest ? stationChangesKindLabel(latest[1]?.kind) : '',
    recent: stationChangesRows(entries),
  };
}

function stationSessionUpdatedMs(session) {
  const raw = session && (session.updated_at || session.updatedAt || session.changed_at || session.changedAt || session.created_at || session.createdAt);
  const ms = Date.parse(raw || '');
  return Number.isFinite(ms) ? ms : 0;
}

function stationSessionTask(session) {
  return compactSessionText(
    session?.name ||
    session?.display_name ||
    session?.thread_name ||
    session?.task ||
    session?.initial_message ||
    session?.initialMessage
  );
}

function stationSessionSource(session) {
  return String(
    session?.backend_source_label ||
    session?.backendSourceLabel ||
    session?.source_label ||
    session?.sourceLabel ||
    session?.backend_source ||
    session?.backendSource ||
    session?.source ||
    ''
  ).trim();
}

function stationSessionId(session) {
  return String(
    session?.session_id ||
    session?.sessionId ||
    session?.resume_id ||
    session?.resumeId ||
    session?.backend_session_id ||
    session?.backendSessionId ||
    ''
  ).trim();
}

function stationSessionBytes(session) {
  return stationNum(
    session?.disk_bytes ??
    session?.diskBytes ??
    session?.storage_bytes ??
    session?.storageBytes ??
    session?.log_bytes ??
    session?.logBytes ??
    session?.total_bytes ??
    session?.totalBytes ??
    session?.bytes ??
    0
  );
}

function stationSessionWindowSummaries() {
  const rows = [];
  for (const [sid, win] of sessionWindows) {
    if (!sid || !win || win.ended || sessionWindowIsDetached(sid)) continue;
    const meta = sessionMetadataById.get(sid) || {};
    const source = externalSourceForSessionWindow(sid, win) || meta.source || meta.backendSource || win.source || 'intendant';
    const phase = normalizeSessionPhase(win.phase || meta.phase || '');
    rows.push({
      session_id: sid,
      name: meta.name || meta.displayName || win.name || win.title || '',
      task: meta.task || meta.initialMessage || win.task || win.title || 'Live session',
      status: phase || 'idle',
      source,
      source_label: meta.sourceLabel || meta.source || source,
      backend_source: meta.backendSource || source,
      updated_at: meta.updatedAt || meta.updated_at || win.updatedAt || win.updated_at || new Date().toISOString(),
      provider: meta.provider || win.provider || '',
      model: meta.model || win.model || '',
      turns: stationNum(meta.turns || win.turns),
      total_tokens: stationNum(meta.totalTokens || meta.total_tokens || win.totalTokens || win.total_tokens),
      prompt_tokens: stationNum(meta.promptTokens || meta.prompt_tokens || win.promptTokens || win.prompt_tokens),
      completion_tokens: stationNum(meta.completionTokens || meta.completion_tokens || win.completionTokens || win.completion_tokens),
      cached_tokens: stationNum(meta.cachedTokens || meta.cached_tokens || win.cachedTokens || win.cached_tokens),
      total_bytes: stationNum(meta.totalBytes || meta.total_bytes || win.totalBytes || win.total_bytes),
    });
  }
  return rows;
}

function stationBuildWorktreesSummary() {
  const scan = _cachedWorktreeScan || {};
  const summary = scan.summary || {};
  const rows = Array.isArray(scan.worktrees) ? scan.worktrees : [];
  // Slim wire rows: the worktrees focus panel renders label/value/detail/
  // tone.
  const recent = [...rows]
    .sort((a, b) => worktreeRiskRank(b) - worktreeRiskRank(a) || stationNum(b?.size_bytes) - stationNum(a?.size_bytes))
    .slice(0, 16)
    .map(wt => {
      const path = String(wt?.path || '').trim();
      const branch = wt?.branch || wt?.head_short || '(unknown ref)';
      const status = wt?.safe_to_remove
        ? 'cleanup candidate'
        : ((wt?.active_sessions || 0) > 0
          ? `${wt.active_sessions} active`
          : (wt?.recommended_action || wt?.merge_status || 'review'));
      return {
        id: path,
        label: branch,
        value: status,
        detail: [
          wt?.repo_name || '',
          _fmtBytes(stationNum(wt?.size_bytes || 0)),
          wt?.merge_status || '',
        ].filter(Boolean).join(' · ') || path,
        tone: wt?.safe_to_remove ? 'ok' : (wt?.dirty || wt?.merge_status === 'unmerged' ? 'red' : 'warning'),
      };
    });
  return {
    worktrees: stationNum(summary.worktrees || rows.length),
    worktreeDirty: stationNum(summary.dirty),
    worktreeUnmerged: stationNum(summary.unmerged),
    worktreeActive: stationNum(summary.active),
    worktreeCleanup: stationNum(summary.cleanup_candidates),
    worktreeBytes: stationNum(summary.total_bytes),
    worktreeScanStatus: worktreesLoadInFlight
      ? worktreesLoadInFlight
      : (scan.scanned_at ? formatContextTimestamp(scan.scanned_at) : (worktreesLoaded ? 'empty' : 'cold')),
    recentWorktrees: recent,
  };
}

function stationSessionMetadataSummaries() {
  const rows = [];
  for (const [sid, meta] of sessionMetadataById) {
    if (!sid || sid === daemonSessionFullId || sessionWindowIsDetached(sid)) continue;
    const backendSessionId = String(meta?.backendSessionId || meta?.backend_session_id || '').trim();
    if (backendSessionId && backendSessionId !== sid) continue;
    const source = normalizeAgentId(
      meta?.backendSource ||
      meta?.backend_source ||
      meta?.source ||
      meta?.sourceLabel ||
      meta?.source_label ||
      ''
    );
    if (!source || source === 'intendant') continue;
    rows.push({
      session_id: sid,
      name: meta.name || meta.displayName || '',
      task: meta.task || meta.initialMessage || 'Live external backend',
      status: normalizeSessionPhase(meta.phase || '') || 'idle',
      source,
      source_label: meta.sourceLabel || meta.source_label || prettyAgentName(source) || source,
      backend_source: meta.backendSource || meta.backend_source || source,
      intendant_session_id: meta.intendantSessionId || meta.intendant_session_id || '',
      updated_at: meta.updatedAt || meta.updated_at || '',
      provider: meta.provider || '',
      model: meta.model || '',
      turns: stationNum(meta.turns),
      total_tokens: stationNum(meta.totalTokens || meta.total_tokens),
      prompt_tokens: stationNum(meta.promptTokens || meta.prompt_tokens),
      completion_tokens: stationNum(meta.completionTokens || meta.completion_tokens),
      cached_tokens: stationNum(meta.cachedTokens || meta.cached_tokens),
      total_bytes: stationNum(meta.totalBytes || meta.total_bytes),
      can_resume: true,
    });
  }
  return rows;
}

function stationCurrentDaemonSessionSummary() {
  const sid = String(daemonSessionFullId || currentSessionFullId || foregroundSessionFullId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  const backendSource = normalizeAgentId(
    meta.backendSource ||
    meta.backend_source ||
    meta.source ||
    ''
  );
  const phase = normalizeSessionPhase(currentPhase || meta.phase || '');
  return {
    session_id: sid,
    name: meta.name || meta.displayName || '',
    task: meta.task || meta.initialMessage || stationCurrentTask || 'Current daemon session',
    status: phase || 'idle',
    source: backendSource || 'intendant',
    source_label: meta.sourceLabel || meta.source || backendSource || 'intendant',
    backend_source: backendSource || '',
    updated_at: meta.updatedAt || meta.updated_at || new Date().toISOString(),
    provider: meta.provider || '',
    model: meta.model || '',
    turns: stationNum(meta.turns),
    total_tokens: stationNum(meta.totalTokens || meta.total_tokens),
    prompt_tokens: stationNum(meta.promptTokens || meta.prompt_tokens),
    completion_tokens: stationNum(meta.completionTokens || meta.completion_tokens),
    cached_tokens: stationNum(meta.cachedTokens || meta.cached_tokens),
    total_bytes: stationNum(meta.totalBytes || meta.total_bytes),
    can_resume: false,
  };
}

function stationBuildSessionsSummary() {
  const worktrees = stationBuildWorktreesSummary();
  const { sessions, indexedIds, sortedSessions, totalTokens, diskBytes } = stationCollectSessionSet();
  const externalIds = new Set();
  for (const session of sessions) {
    const source = normalizeAgentId(session?.backend_source || session?.backendSource || session?.source || '');
    if (source && source !== 'intendant') {
      const id = session?.session_id || session?.resume_id || session?.backend_session_id || JSON.stringify(session);
      externalIds.add(String(id));
    }
  }
  for (const [sid, win] of sessionWindows) {
    if (!win || win.ended || sessionWindowIsDetached(sid)) continue;
    if (externalSourceForSessionWindow(sid, win)) externalIds.add(sid);
  }
  let active = 0;
  for (const [sid, win] of sessionWindows) {
    if (!win || win.ended || sessionWindowIsDetached(sid)) continue;
    if (hasPendingActiveSessionWindow(sid) || isAgentActivePhase(win.phase)) active += 1;
  }
  if (!active && isAgentActivePhase(currentPhase)) active = 1;
  const latest = sortedSessions[0] || null;
  // Actionable wire rows: the sessions focus panel scrolls these and
  // its row pills act on the ids/capability flags.
  const recent = sortedSessions
    .slice(0, 20)
    .map(session => stationActionRow(stationSessionRow(session, indexedIds)));
  let externalTargets = [];
  try {
    externalTargets = stationExternalTargetRows(5).map(row => ({
      id: row.id || '',
      sessionId: row.sid || row.id || '',
      action: 'focus',
      label: row.label || shortSessionId(row.id) || 'external session',
      value: `${prettyAgentName(row.source) || row.source || 'external'} / ${row.target ? 'target' : (row.status || 'idle')}`,
      detail: row.detail || [row.liveId ? `window ${shortSessionId(row.liveId)}` : '', row.detached ? 'detached' : ''].filter(Boolean).join(' / '),
      tone: row.target ? 'ok' : (row.active ? 'peer' : 'session'),
      externalStatus: row.target ? 'target' : (row.status || ''),
      liveId: row.liveId || '',
      actionId: row.actionId || '',
      attachId: row.attachId || '',
      stopId: row.stopId || '',
      externalDetached: !!row.detached,
      isCodex: !!row.isCodex,
      threadActionSessionId: row.actionId || row.id || '',
      backendId: row.id || '',
      intendantId: row.sid && row.sid !== row.id ? row.sid : '',
      livePhase: row.status || '',
      command: sessionConfigCommand(sessionConfigMetadata(row.id)) || 'global default',
      managedContext: row.source === 'codex' ? sessionConfigManagedMode(sessionConfigMetadata(row.id)) : '',
      contextArchive: row.source === 'codex' ? sessionConfigArchiveMode(sessionConfigMetadata(row.id)) : '',
      launchPersistent: true,
      canFocus: !!row.liveId,
      canResume: !!row.canResume,
      canConfig: !!row.canConfig,
      canRename: !!row.id,
      canAttach: !!row.attachId,
      canStop: !!row.stopId,
      // Per-row capability: the prompt target with a live stop id can be
      // interrupted. (A previous version referenced an undefined `controls`
      // here, throwing whenever a prompt target existed and silently
      // emptying externalTargets via the catch below.)
      canInterrupt: !!row.target && !!row.stopId,
      canRestart: !!row.canConfig,
      canOpenLog: true,
      canFork: !!row.isCodex || (sessionThreadActionOps(row.id) || []).includes('fork'),
    }));
  } catch (err) {
    console.warn('Station external target snapshot failed:', err);
  }
  return {
    total: sessions.length,
    active,
    external: externalIds.size,
    totalTokens,
    diskBytes,
    latestTask: latest
      ? (stationSessionTask(latest)
        || (latest?.role === 'resident' || latest?.status === 'resident' ? 'Daemon session' : 'Untitled session'))
      : '',
    latestSource: latest
      ? (sessionConfigSource(sessionConfigMetadata(latest)) || stationSessionSource(latest) || latest?.source || 'session')
      : '',
    latestUpdated: latest ? formatContextTimestamp(latest.updated_at || latest.updatedAt || latest.changed_at || latest.changedAt || latest.created_at || latest.createdAt) : '',
    indexStatus: sessionsLoaded
      ? 'loaded'
      : (stationSessionsIndexLoading ? 'indexing' : (stationSessionsIndexError ? 'error' : 'cold')),
    // The dock-era station-local session filters are gone (nothing ever set
    // them); the scalar placeholders stay because the WASM model
    // deserializes them. The always-empty filteredSessions array was
    // removed from both sides. The legacy Sessions tab search box still
    // feeds searchQuery.
    searchQuery: sessionSearchQuery?.() || '',
    sourceFilter: '',
    statusFilter: '',
    projectFilter: '',
    filtered: sessions.length,
    externalTargets,
    recent,
    ...worktrees,
  };
}

function stationBuildControlsSummary() {
  const backend = controlCurrentBackend || currentExternalAgent || newSessionConfiguredAgent || '';
  const command = backend === 'codex'
    ? (controlCodexConfig.command || commandDefaultForNewSessionAgent('codex') || 'codex')
    : (backend ? (commandDefaultForNewSessionAgent(backend) || backend) : '');
  const promptTargetId = String(resolvePromptTargetSessionId() || '').trim();
  const rawWindowTargetId = String(currentSessionFullId || foregroundSessionFullId || '').trim();
  const windowTargetId = rawWindowTargetId && rawWindowTargetId !== daemonSessionFullId
    ? rawWindowTargetId
    : '';
  const fallbackTargetId = (!promptTargetId && !windowTargetId)
    ? stationLatestConfigurableExternalSessionId()
    : '';
  const sessionId = String(
    promptTargetId ||
    windowTargetId ||
    fallbackTargetId ||
    rawWindowTargetId ||
    daemonSessionFullId ||
    ''
  ).trim();
  const sessionSelection = promptTargetId
    ? 'prompt target'
    : (windowTargetId
      ? 'session window'
      : (fallbackTargetId ? 'latest external' : 'daemon'));
  const sessionMeta = sessionId ? sessionConfigMetadata(sessionId) : {};
  const sessionForStatus = sessionId ? stationFindSessionById(sessionId) : null;
  const rawSessionSource = sessionId ? sessionConfigSource(sessionMeta) : '';
  const sessionSource = rawSessionSource || (sessionId ? 'intendant' : '');
  const rawSessionCommand = sessionId ? sessionConfigCommand(sessionMeta) : '';
  const sessionCommand = !sessionId
    ? ''
    : (sessionSource === 'intendant' ? 'internal' : (rawSessionCommand || 'global default'));
  const sessionBackendId = String(sessionMeta.backendSessionId || sessionMeta.backend_session_id || '').trim();
  const sessionIntendantId = String(sessionMeta.intendantSessionId || sessionMeta.intendant_session_id || '').trim();
  const sessionManagedContext = sessionSource === 'codex' ? sessionConfigManagedMode(sessionMeta) : '';
  const sessionContextArchive = sessionSource === 'codex' ? sessionConfigArchiveMode(sessionMeta) : '';
  let sessionCanConfig = !!sessionId && !!rawSessionSource && rawSessionSource !== 'intendant';
  const externalLive = sessionId ? stationExternalLiveThreadDescriptor(sessionId) : null;
  if (externalLive?.source && externalLive.source !== 'intendant') sessionCanConfig = true;
  const sessionConfigEditing = sessionCanConfig ? stationSessionConfigEditingFor(sessionId) : null;
  const sessionConfigHadDraft = !!(sessionConfigEditing?.sessionId && stationSessionConfigDrafts.has(sessionConfigEditing.sessionId));
  const sessionConfigDraft = sessionConfigEditing ? stationSessionConfigDraftFor(sessionConfigEditing) : null;
  const sessionConfigPending = !!(sessionConfigSavePending?.station && sessionConfigEditing?.sessionId && sessionConfigSavePending.sessionId === sessionConfigEditing.sessionId);
  const sessionConfigResultActive = !!(sessionConfigEditing?.sessionId && stationSessionConfigResult.sessionId === sessionConfigEditing.sessionId);
  const sessionLaunchPersistent = !!(sessionConfigEditing?.sessionId && (
    rawSessionCommand ||
    sessionLaunchManagedMode(sessionMeta) ||
    sessionLaunchArchiveMode(sessionMeta) ||
    sessionLaunchSandboxMode(sessionMeta) ||
    sessionLaunchApprovalPolicy(sessionMeta)
  ));
  const sessionLiveId = externalLive?.liveId || '';
  const sessionActionId = externalLive?.actionId || '';
  const sessionAttachId = externalLive?.attachId || '';
  const sessionStopId = externalLive?.stopId || '';
  const sessionCapabilities = stationSessionCapabilities(sessionActionId || sessionId)
    || stationSessionCapabilities(sessionId)
    || {};
  const sessionGoal = normalizeSessionGoal(sessionMeta.goal || sessionMeta.session_goal || sessionMeta.sessionGoal || null);
  const sessionActive = sessionLiveId
    ? isSessionWindowEffectivelyActive(sessionLiveId)
    : (sessionId
      ? isSessionWindowEffectivelyActive(sessionId)
      : isAgentActivePhase(currentPhase));
  const sessionDetached = externalLive
    ? externalLive.detached
    : (sessionId ? sessionWindowIsDetached(sessionId) : false);
  const sessionCanSteer = externalLive
    ? externalLive.steer
    : (sessionId ? sessionSupportsSteer(sessionId) : true);
  const sessionCanInterrupt = externalLive
    ? !!sessionStopId
    : (sessionId
      ? sessionActive && sessionSupportsInterrupt(sessionId) && !sessionDetached
      : isAgentActivePhase(currentPhase));
  const sessionCanAttach = externalLive ? !!sessionAttachId : (sessionId ? canAttachSessionWindow(sessionId) : false);
  const sessionCanStop = externalLive ? !!sessionStopId : (sessionId ? sessionWindowStopAvailability(sessionId).ok : false);
  const sessionCanFocus = externalLive ? !!sessionLiveId : !!(sessionId && sessionWindows.has(sessionId));
  const sessionIsCodex = sessionId
    ? stationSessionLooksCodex(sessionActionId || sessionId)
      || stationSessionLooksCodex(sessionId)
      || sessionSource === 'codex'
    : backend === 'codex';
  const sessionLivePhase = externalLive?.phase || '';
  const sessionStatus = normalizeSessionPhase(
    sessionLivePhase ||
    sessionMeta.phase ||
    sessionMeta.status ||
    sessionForStatus?.status ||
    ''
  ) || (sessionDetached ? 'detached' : (sessionActive ? 'active' : (sessionId ? 'idle' : 'none')));
  const sessionServiceTier = sessionSource === 'codex'
    ? normalizeCodexServiceTier(
      sessionCapabilities.codexServiceTier ||
      (sessionCapabilities.codexFastMode === true ? 'priority' : '') ||
      controlCodexConfig.service_tier ||
      ''
    )
    : '';
  const draftChars = stationNum(document.getElementById('activity-task-input')?.value?.trim()?.length || 0);
  const promptMode = sessionActive && sessionCanSteer ? 'steer' : 'send';
  const voiceState = !isActiveBrowser
    ? 'passive'
    : (voiceConnecting ? 'connecting' : (modelConnected ? 'connected' : 'idle'));
  const displayAccessText = compactSessionText(document.getElementById('sb-display-access')?.textContent || 'off') || 'off';
  const debugStatus = compactSessionText(document.getElementById('debug-status')?.textContent || '');
  const debugRecordBtn = document.getElementById('debug-record-btn');
  const globalAttachmentChips = document.querySelectorAll('#global-pending-attachments .pending-attachment-chip').length;
  const pendingAttachmentText = document.getElementById('pending-attachments-count')?.textContent || '';
  const pendingAttachmentCount = globalAttachmentChips || stationNum(pendingAttachmentText);
  const activeBrowserWorkspaces = stationNum(window.__stationBrowserWorkspaceCount || 0);
  const sharedViewVisible = !!sharedViewState.visible;
  const sharedViewTarget = sharedViewVisible
    ? sharedViewDisplayLabel(sharedViewState.displayId, sharedViewState.displayTarget)
    : '';
  const sharedViewNote = [sharedViewState.reason, sharedViewState.note]
    .map(value => String(value || '').trim())
    .filter(Boolean)
    .join(' / ');
  const sharedViewCanTakeInput = sharedViewVisible
    && sharedViewState.action === 'input_request'
    && sharedViewState.displayId !== null
    && displaySlots.has(sharedViewState.displayId);
  const browserWorkspaceStatus = stationBrowserWorkspaceStatusPayload();
  const displayTarget = stationDisplayControlTargetPayload();
  const latestOperationalActivity = stationLatestOperationalActivityPayload();
  const cuValidationState = debugStatus || (displayTarget.kind !== 'none' ? 'display target ready' : 'idle');
  const cuValidationDetail = [
    stationComputerUseValue('set-cu-provider', '') || 'auto',
    stationComputerUseValue('set-cu-backend', 'auto') || 'auto',
    displayTarget.authority ? `input ${displayTarget.authority}` : '',
  ].filter(Boolean).join(' / ');
  const externalTurn = stationBuildExternalTurnSummary({
    backend,
    command,
    sessionId,
    sessionSelection,
    sessionSource,
    sessionStatus,
    sessionCommand,
    sessionLivePhase,
    sessionActive,
    sessionDetached,
    sessionCanFocus,
    sessionCanAttach,
    sessionCanStop,
    sessionCanConfig,
    sessionCanInterrupt,
  });
  const launchReadiness = stationBuildLaunchReadiness();
  return {
    backend: backend || 'none',
    command,
    sandbox: controlCodexConfig.sandbox || 'workspace-write',
    approvalPolicy: controlCodexConfig.approval_policy || 'on-request',
    model: controlCodexConfig.model || '',
    reasoningEffort: controlCodexConfig.reasoning_effort || '',
    serviceTier: controlCodexConfig.service_tier || '',
    managedContext: controlCodexConfig.managed_context || 'vanilla',
    managedCommand: controlCodexConfig.managed_command || '',
    contextArchive: controlCodexConfig.context_archive || 'summary',
    webSearch: !!controlCodexConfig.web_search,
    networkAccess: !!controlCodexConfig.network_access,
    writableRoots: Array.isArray(controlCodexConfig.writable_roots)
      ? controlCodexConfig.writable_roots.length
      : 0,
    claudeModel: controlClaudeConfig.model || '',
    claudePermissionMode: controlClaudeConfig.permission_mode || 'default',
    newSessionAgent: newSessionConfiguredAgent || backend || '',
    sessionId,
    sessionLabel: stationSessionShortLabel(sessionId),
    sessionSelection,
    sessionSource,
    sessionCommand,
    sessionBackendId,
    sessionIntendantId,
    sessionLiveId,
    sessionLivePhase,
    sessionActionId,
    sessionAttachId,
    sessionStopId,
    sessionManagedContext,
    sessionContextArchive,
    sessionSandbox: sessionSource === 'codex' ? sessionConfigSandboxMode(sessionMeta) : '',
    sessionApprovalPolicy: sessionSource === 'codex' ? sessionConfigApprovalPolicy(sessionMeta) : '',
    // Dock config-editor draft values — '' means "inherit the global
    // default". Don't fall back to the effective sessionManagedContext /
    // sessionContextArchive published above: that would surface (and on a
    // dock save re-pin) a value the session never pinned.
    sessionConfigManaged: sessionConfigDraft ? (sessionConfigDraft.managed || '') : '',
    sessionConfigArchive: sessionConfigDraft ? (sessionConfigDraft.archive || '') : '',
    sessionConfigResult: sessionConfigResultActive ? (stationSessionConfigResult.text || '') : '',
    sessionConfigResultKind: sessionConfigResultActive ? (stationSessionConfigResult.kind || '') : '',
    sessionConfigHasDraft: sessionConfigHadDraft,
    sessionConfigPending,
    sessionLaunchPersistent,
    sessionCanConfig,
    sessionCanFocus,
    sessionCanAttach,
    sessionCanStop,
    sessionCanRename: !!sessionId,
    sessionCanInterrupt,
    sessionCanSteer,
    sessionDetached,
    sessionActive,
    sessionIsCodex,
    sessionServiceTier,
    sessionGoalStatus: sessionGoal?.status || '',
    sessionGoalObjective: sessionGoal?.objective || '',
    sessionGoalTokens: sessionGoal?.tokensUsed !== null && sessionGoal?.tokensUsed !== undefined
      ? String(sessionGoal.tokensUsed)
      : '',
    externalTurnState: externalTurn.state,
    externalTurnBackend: externalTurn.backend,
    externalTurnLabel: externalTurn.label,
    externalTurnDetail: externalTurn.detail,
    externalTurnSessionId: externalTurn.sessionId,
    promptMode,
    directMode: !!document.getElementById('direct-mode-toggle')?.checked,
    draftChars,
    displayAccess: displayAccessText,
    voiceState,
    micActive: !!micActive,
    videoActive: !!videoActive,
    activeBrowser: !!isActiveBrowser,
    browserWorkspaces: activeBrowserWorkspaces,
    browserWorkspaceStatus: browserWorkspaceStatus.validation_state,
    browserWorkspaceDetail: browserWorkspaceStatus.validation_detail,
    browserWorkspaceLatest: browserWorkspaceStatus.latest_label || browserWorkspaceStatus.latest_id || '',
    browserWorkspaceLease: browserWorkspaceStatus.lease || '',
    browserWorkspaceId: browserWorkspaceStatus.latest_id || '',
    browserWorkspaceProvider: browserWorkspaceStatus.latest_provider || '',
    browserWorkspaceUrl: browserWorkspaceStatus.latest_url || '',
    browserWorkspaceUpdated: browserWorkspaceStatus.latest_updated || '',
    browserWorkspaceCanCreate: !!browserWorkspaceStatus.can_create,
    browserWorkspaceCanAcquire: !!browserWorkspaceStatus.can_acquire,
    browserWorkspaceCanClose: !!browserWorkspaceStatus.can_close,
    recordings: recordingStreams.size,
    activeRecording: activeRecordingStream || '',
    // Per-stream rows for the rendered controls panel (click → recording
    // browser). Sorted for snapshot stability.
    recordingStreams: [...recordingStreams.entries()]
      .sort((a, b) => a[0].localeCompare(b[0]))
      .slice(0, 12)
      .map(([name, info]) => ({
        label: name.startsWith('display_') ? `:${name.slice(8)}` : name,
        value: info?.active ? 'recording' : 'stored',
        detail: info?.totalDuration ? `${Math.round(info.totalDuration)}s` : (Array.isArray(info?.segments) ? `${info.segments.length} segments` : ''),
        tone: info?.active ? 'red' : 'session',
        actionId: name,
      })),
    autonomy: normalizeAutonomyLabel(document.getElementById('sb-autonomy')?.textContent || 'Medium').toLowerCase(),
    cuProvider: stationComputerUseValue('set-cu-provider', '') || 'auto',
    cuModel: stationComputerUseValue('set-cu-model', '') || 'default',
    cuBackend: stationComputerUseValue('set-cu-backend', 'auto') || 'auto',
    cuValidationState,
    cuValidationDetail,
    debugScreen: /^active$/i.test(debugStatus),
    debugRecording: !!debugRecordBtn && /stop/i.test(debugRecordBtn.textContent || ''),
    pendingAttachments: pendingAttachmentCount,
    sharedViewVisible,
    sharedViewTarget,
    sharedViewAction: sharedViewState.action || '',
    sharedViewNote,
    sharedViewCanTakeInput,
    selectedDisplayKind: displayTarget.kind || '',
    selectedDisplayLabel: displayTarget.label || '',
    selectedDisplayTarget: displayTarget.target || '',
    selectedDisplayHostId: displayTarget.host_id || '',
    selectedDisplayId: displayTarget.display_id,
    selectedDisplayLaneId: displayTarget.lane_id || '',
    selectedDisplayStatus: displayTarget.status || '',
    selectedDisplayAuthority: displayTarget.authority || '',
    selectedDisplayCapture: displayTarget.capture || '',
    selectedDisplayFreshness: displayTarget.freshness || '',
    selectedDisplayTelemetry: displayTarget.telemetry || '',
    selectedDisplayCanOpen: !!displayTarget.can_open,
    selectedDisplayCanFocus: !!displayTarget.can_focus,
    selectedDisplayCanTakeInput: !!displayTarget.can_take_input,
    selectedDisplayCanReleaseInput: !!displayTarget.can_release_input,
    selectedDisplayCanAttachFrame: !!displayTarget.can_attach_frame,
    selectedDisplayCanCapture: !!displayTarget.can_capture,
    latestOperationalActivity: latestOperationalActivity.detail || '',
    latestOperationalActivityLabel: latestOperationalActivity.label || '',
    launchReady: !!launchReadiness.ready,
    // 'task' is omitted: in the Station flow the task text lives in the
    // composer input until launch, so listing it as missing is noise.
    launchMissing: (launchReadiness.missing || []).filter(item => item !== 'task').join(', '),
    launchAgent: launchReadiness.agent || '',
    launchAgentLabel: launchReadiness.agentLabel || '',
    launchCommand: launchReadiness.command || '',
    launchTaskChars: stationNum(launchReadiness.taskChars),
    launchProject: launchReadiness.project ? compactPathLabel(launchReadiness.project, true) : '',
    // 'auto' | 'orchestrate' | 'direct' for the internal agent; empty when
    // execution does not apply (external agent) so the canvas hides the
    // pills.
    launchMode: launchReadiness.executionApplies ? (launchReadiness.execution || 'auto') : '',
    launchAttachments: stationNum(launchReadiness.attachments),
    launchNotice: launchReadiness.notice || '',
  };
}

function stationBuildExternalTurnSummary(input = {}) {
  const backend = normalizeAgentId(input.backend || '');
  const source = normalizeAgentId(input.sessionSource || '');
  const configured = backend && backend !== 'internal' && backend !== 'none' ? backend : '';
  const external = source && source !== 'intendant' ? source : configured;
  if (!external) {
    return {
      state: 'internal',
      backend: 'internal',
      label: 'Internal agent',
      detail: 'Intendant daemon loop',
      sessionId: String(input.sessionId || ''),
    };
  }

  const phase = normalizeSessionPhase(input.sessionLivePhase || '');
  const command = String(input.sessionCommand || input.command || '').trim();
  const sessionId = String(input.sessionId || '').trim();
  const sourceLabel = prettyAgentName(external) || external;
  let state = 'stopped';
  if (!command && !sessionId) {
    state = 'misconfigured';
  } else if (!sessionId) {
    state = 'queued';
  } else if (input.sessionDetached) {
    state = 'stopped';
  } else if (phase.startsWith('waiting')) {
    state = 'waiting';
  } else if (phase === 'thinking') {
    state = 'thinking';
  } else if (phase === 'running' || phase === 'orchestrating' || input.sessionActive) {
    state = 'running tools';
  } else if (phase === 'interrupted' || phase === 'done') {
    state = 'stopped';
  }

  const capabilities = [
    input.sessionCanInterrupt ? 'interrupt' : '',
    input.sessionCanAttach ? 'attach' : '',
    input.sessionCanFocus ? 'focus' : '',
    input.sessionCanConfig ? 'config' : '',
  ].filter(Boolean).join(' / ');
  const detail = [
    input.sessionSelection || '',
    phase || (input.sessionDetached ? 'detached' : ''),
    capabilities,
    command && command !== 'internal' ? command : '',
  ].filter(Boolean).join(' / ');
  return {
    state,
    backend: external,
    label: sourceLabel,
    detail: detail || (sessionId ? 'external session selected' : 'external backend selected'),
    sessionId,
  };
}

// stationLatestConfigurableExternalSessionId scans every session across four
// pools (each via sessionConfigMetadata's merge) to produce a single id that
// only changes when sessions come, go, or update. It runs inside every
// Station snapshot build, where the scan dominated build time (CDP-profiled).
// Memoize per sourceFilter, guarded by the session stores' identities/sizes
// plus a short TTL for in-place metadata updates.
const _stationLatestExternalMemo = new Map();

function stationLatestConfigurableExternalSessionId(sourceFilter = '') {
  const wantedSource = normalizeAgentId(sourceFilter || '');
  const memoKey = wantedSource;
  const memo = _stationLatestExternalMemo.get(memoKey);
  const now = performance.now();
  if (
    memo &&
    memo.cachedRef === _cachedSessions &&
    memo.metaSize === sessionMetadataById.size &&
    now - memo.ts < 2000
  ) {
    return memo.value;
  }
  const value = _stationLatestConfigurableExternalSessionIdUncached(wantedSource);
  _stationLatestExternalMemo.set(memoKey, {
    cachedRef: _cachedSessions,
    metaSize: sessionMetadataById.size,
    ts: now,
    value,
  });
  return value;
}

function _stationLatestConfigurableExternalSessionIdUncached(wantedSource) {
  const seen = new Set();
  const candidates = [];
  const pushCandidate = (id, session, meta = {}) => {
    const sid = String(id || '').trim();
    if (!sid || sid === daemonSessionFullId || seen.has(sid)) return;
    const backendSessionId = String(meta?.backendSessionId || meta?.backend_session_id || '').trim();
    if (backendSessionId && backendSessionId !== sid) return;
    seen.add(sid);
    const source = sessionConfigSource(meta) || normalizeAgentId(stationSessionSource(session));
    if (!source || source === 'intendant') return;
    if (wantedSource && source !== wantedSource) return;
    const intendantId = String(
      meta?.intendantSessionId ||
      meta?.intendant_session_id ||
      session?.intendant_session_id ||
      session?.intendantSessionId ||
      ''
    ).trim();
    candidates.push({
      id: sid,
      updated: stationSessionUpdatedMs(session),
      liveDaemon: !!daemonSessionFullId && intendantId === daemonSessionFullId,
    });
  };
  const pools = [
    _cachedSessions || [],
    sessionsListCache.get(sessionListCacheKey(selfPeerId)) || [],
    stationSessionWindowSummaries(),
    stationSessionMetadataSummaries(),
  ];
  for (const pool of pools) {
    if (!Array.isArray(pool)) continue;
    for (const session of pool) {
      if (!session) continue;
      const meta = sessionConfigMetadata(session);
      const sessionId = stationSessionId(session);
      const backendId = String(meta?.backendSessionId || meta?.backend_session_id || '').trim();
      const id = backendId || sessionId;
      pushCandidate(id, session, meta);
    }
  }
  for (const [id, meta] of sessionMetadataById) {
    if (sessionWindowIsDetached(id)) continue;
    pushCandidate(id, {
      session_id: id,
      source: meta?.source || meta?.backendSource || meta?.backend_source || '',
      source_label: meta?.sourceLabel || meta?.source_label || '',
      intendant_session_id: meta?.intendantSessionId || meta?.intendant_session_id || '',
      updated_at: meta?.updatedAt || meta?.updated_at || '',
    }, meta || {});
  }
  candidates.sort((a, b) => Number(b.liveDaemon) - Number(a.liveDaemon) || b.updated - a.updated);
  return candidates[0]?.id || '';
}

function stationSessionShortLabel(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return '';
  const parts = typeof sessionIdentityParts === 'function' ? sessionIdentityParts(sid) : null;
  return compactSessionText(parts?.name)
    ? `${parts.shortId} · ${parts.name}`
    : shortSessionId(sid);
}

function stationSessionCapabilities(sessionId) {
  if (!sessionId || typeof getSessionCapabilities !== 'function') return null;
  return getSessionCapabilities(sessionId);
}

function stationSessionLooksCodex(sessionOrId) {
  const sid = typeof sessionOrId === 'object'
    ? stationSessionId(sessionOrId)
    : String(sessionOrId || '').trim();
  if (sid && sessionWindowIsCodex(sid)) return true;
  return sessionConfigSource(sessionConfigMetadata(sessionOrId || sid)) === 'codex';
}

// Snapshot diet: detail rows cross the WASM boundary with exactly the
// fields the Station focus panels render (label / value / detail / tone).
// Richer row shapes stay dashboard-internal.
function stationSlimDetailRow(row) {
  return {
    label: String(row?.label || ''),
    value: String(row?.value || ''),
    detail: String(row?.detail || ''),
    tone: String(row?.tone || ''),
  };
}

// Actionable wire row: the slim display fields plus the ids and
// capability flags the rendered Station's row pills key off (the
// snapshot diet had stripped these along with the dead fields, leaving
// the canvas rows inert).
function stationActionRow(row) {
  return {
    ...stationSlimDetailRow(row),
    id: String(row?.id || ''),
    sessionId: String(row?.sessionId || row?.session_id || row?.id || ''),
    actionId: String(row?.actionId || ''),
    attachId: String(row?.attachId || ''),
    stopId: String(row?.stopId || ''),
    canResume: !!row?.canResume,
    canFocus: !!row?.canFocus,
    canAttach: !!row?.canAttach,
    canStop: !!row?.canStop,
    canInterrupt: !!row?.canInterrupt,
    canOpenLog: !!row?.canOpenLog,
    canFork: !!row?.canFork,
  };
}

function buildStationSnapshot() {
  const hosts = [];
  const selfUsage = stationUsageForHost(selfPeerId);
  hosts.push({
    id: selfPeerId,
    name: selfHostLabel || 'local',
    platform: navigator.platform || 'local',
    region: 'primary',
    connected: true,
    cpu: selfUsage.cpu,
    mem: selfUsage.mem,
  });
  for (const d of daemons) {
    const usage = stationUsageForHost(d.host_id);
    hosts.push({
      id: d.host_id,
      name: d.label || d.host_id,
      platform: daemonPlatformLabel(d),
      region: d.url || 'peer',
      connected: !!d.connected,
      cpu: usage.cpu,
      mem: usage.mem,
    });
  }

  const agents = [stationAgentForSelf(selfUsage)];
  for (const d of daemons) {
    const usage = stationUsageForHost(d.host_id);
    agents.push(stationAgentForPeer(d, usage));
    agents.push(...stationPeerSessionAgents(d));
    const pending = peerPendingApprovals.get(d.host_id);
    if (pending) {
      for (const [approvalId, approval] of pending.entries()) {
        agents.push(stationApprovalAgent(d.host_id, approvalId, approval));
      }
    }
  }
  if (stationCurrentApproval) {
    agents.push(stationApprovalAgent(selfPeerId, stationCurrentApproval.id, stationCurrentApproval));
  }
  agents.push(...stationSessionAgents());

  const context = stationBuildContextSummary();
  const controls = stationBuildControlsSummary();
  const activityEvents = stationActivityEvents();
  const activity = stationBuildActivitySummary(activityEvents);
  return {
    hosts,
    agents,
    events: activity.visibleEvents?.length || activity.retainedCount ? activity.visibleEvents : activityEvents,
    activity,
    context,
    managed: stationBuildManagedSummary(context),
    changes: stationBuildChangesSummary(),
    sessions: stationBuildSessionsSummary(),
    controls,
    attentionQueue: stationBuildAttentionQueue(controls),
    displayRunway: stationDisplayRunwaySnapshot(controls),
  };
}

// Projection of the internal display-runway payload onto the slim shape
// the Station peers panel renders. The full payload (per-lane ids,
// telemetry fields, capability flags) stays dashboard-internal for action
// routing; serializing it into every snapshot was pure dead weight.
function stationDisplayRunwaySnapshot(controls = null) {
  const payload = stationDisplayRunwayPayload(controls);
  return {
    selected_peer_id: payload.selected_peer_id,
    selected_peer_label: payload.selected_peer_label,
    selected_display_id: payload.selected_display_id,
    peer_status: payload.peer_status,
    peer_count: payload.peer_count,
    connected_peers: payload.connected_peers,
    display_peers: payload.display_peers,
    local_streams: payload.local_streams,
    remote_streams: payload.remote_streams,
    lanes: payload.lanes.slice(0, 10).map(lane => ({
      type: String(lane.type || ''),
      // Routing keys for the rendered peers panel's lane rows: the
      // dashboard's display_runway_action handlers resolve lanes by id.
      id: String(lane.id || ''),
      host_id: String(lane.host_id || ''),
      display_id: lane.display_id ?? null,
      session_id: String(lane.session_id || ''),
      title: String(lane.title || ''),
      meta: String(lane.meta || ''),
      detail: String(lane.detail || ''),
      selected: !!lane.selected,
    })),
  };
}

// StationAgent vitals fields from a session's normalized vitals: git and
// limits reuse the chip formatters (single source of truth for the text);
// the cache section passes raw numbers so the wasm HUD renders the TTL
// countdown live per frame.
function stationVitalsFields(vitals) {
  const out = {};
  if (!vitals || typeof vitals !== 'object') return out;
  try {
    const git = sessionVitalsGitSegment(vitals.git);
    if (git) {
      out.vitalsGit = git.text;
      out.vitalsGitConflict = !!git.conflict;
    }
    const cache = vitals.cache;
    if (cache && typeof cache === 'object') {
      out.cacheHitPct = cache.hitPct === null || cache.hitPct === undefined || !Number.isFinite(Number(cache.hitPct))
        ? -1
        : Number(cache.hitPct);
      out.cacheLastActivityEpoch = Number(cache.lastActivityEpoch) || 0;
      out.cacheTtlSeconds = Number(cache.ttlSeconds) || 0;
    }
    const limits = sessionVitalsLimitsSegment(vitals.limits);
    if (limits) {
      out.vitalsLimits = limits.text;
      out.vitalsLimitsState = limits.severity || '';
    }
  } catch (_) {}
  return out;
}

function stationAgentForSelf(usage) {
  const phase = normalizeStationPhase(currentPhase);
  // Goal fields light the scene's goal ring + the focus-panel goal row for
  // the primary session too (session_id stays unset on purpose: the
  // primary node keeps the classic agent focus panel, not session pills).
  // currentSessionFullId is only set once a window is focused — the
  // daemon's own session is the primary node's identity until then.
  let goal = null;
  let vitals = null;
  const selfSessionId = String(currentSessionFullId || daemonSessionFullId || '').trim();
  if (selfSessionId) {
    try {
      const meta = sessionMetadataById.get(selfSessionId) || {};
      goal = normalizeSessionGoal(meta.goal || meta.session_goal || meta.sessionGoal || null);
      vitals = meta.vitals || null;
    } catch (_) {}
  }
  return {
    ...stationVitalsFields(vitals),
    id: 'primary-agent',
    hostId: selfPeerId,
    role: 'direct',
    phase,
    status: isAgentActivePhase(currentPhase) ? 'in_progress' : (phase === 'done' ? 'done' : 'idle'),
    task: stationCurrentTask || 'primary daemon',
    provider: usage.provider || document.getElementById('sb-provider')?.textContent || '',
    model: usage.model || document.getElementById('sb-model')?.textContent || '',
    tokens: usage.tokens,
    tokenCap: usage.tokenCap,
    prompt: usage.prompt,
    completion: usage.completion,
    cached: usage.cached,
    cost: usage.cost,
    turns: usage.turns,
    turnCap: usage.turnCap,
    autonomy: document.getElementById('sb-autonomy')?.textContent || '',
    worktree: dashboardProjectRoot || '',
    parentId: null,
    needsApproval: !!stationCurrentApproval,
    approvalId: stationCurrentApproval && stationCurrentApproval.id,
    approvalCommand: stationCurrentApproval && stationCurrentApproval.command || '',
    approvalCategory: stationCurrentApproval && stationCurrentApproval.category || '',
    goalStatus: goal ? String(goal.status || '') : '',
    goalObjective: goal ? String(goal.objective || '') : '',
    goalTokens: goal && goal.tokensUsed !== null && goal.tokensUsed !== undefined
      ? String(goal.tokensUsed)
      : '',
  };
}

function stationAgentForPeer(d, usage) {
  const status = d.server_status || (d.connected ? 'idle' : 'error');
  const needsApproval = status === 'needs_approval';
  const phase = needsApproval ? 'waiting' : status === 'working' ? 'running' : d.connected ? 'idle' : 'waiting';
  // The peer daemon's own primary session (is_primary in the folded
  // sessions list) enriches this node — its vitals, goal, and label —
  // mirroring how stationAgentForSelf reads the local daemon session.
  // Non-primary peer sessions become their own orbiting nodes via
  // stationPeerSessionAgents.
  let primarySession = null;
  try {
    primarySession = (Array.isArray(d.sessions) ? d.sessions : []).find(s => s && s.is_primary) || null;
  } catch (_) {}
  const goal = primarySession ? normalizeSessionGoal(primarySession.goal || null) : null;
  return {
    ...stationVitalsFields(primarySession && primarySession.vitals || null),
    id: 'peer-' + sanitizeStationId(d.host_id),
    hostId: d.host_id,
    role: 'orchestrator',
    phase,
    status: status === 'working' ? 'in_progress' : status,
    task: needsApproval
      ? 'waiting for approval'
      : (primarySession && compactSessionText(primarySession.label || '') || 'peer daemon'),
    provider: usage.provider || '',
    model: usage.model || '',
    tokens: usage.tokens,
    tokenCap: usage.tokenCap,
    prompt: usage.prompt,
    completion: usage.completion,
    cached: usage.cached,
    cost: usage.cost,
    turns: usage.turns,
    turnCap: usage.turnCap,
    autonomy: '',
    worktree: '',
    parentId: null,
    needsApproval,
    approvalId: null,
    approvalCommand: '',
    approvalCategory: '',
    goalStatus: goal ? String(goal.status || '') : '',
    goalObjective: goal ? String(goal.objective || '') : '',
    goalTokens: goal && goal.tokensUsed !== null && goal.tokensUsed !== undefined
      ? String(goal.tokensUsed)
      : '',
  };
}

// Wire phase vocabulary (status_from_phase's input set: working /
// waiting_approval / done / idle / …) → the scene's phase set. The
// local normalizeStationPhase expects dashboard status labels and
// maps "working" to idle — wrong for remote sessions, hence this
// peer-specific mapper.
function stationPhaseFromPeerSession(phase) {
  const p = String(phase || '').toLowerCase();
  if (p.includes('thinking')) return 'thinking';
  if (p.includes('working') || p.includes('acting') || p.includes('executing') || p.includes('running')) return 'running';
  if (p.includes('approval') || p.includes('waiting')) return 'waiting';
  if (p.includes('done') || p.includes('complete')) return 'done';
  return 'idle';
}

// A peer's non-primary sessions as display-only scene nodes orbiting
// that peer's host (v1: no action pills — the SessionAction handlers
// assume local session ids). Newest first, capped: the scene is a
// bounded constellation by design (same doctrine as the recent-session
// nodes); the peer's own dashboard remains the exhaustive list.
const PEER_SESSION_SCENE_NODES = 12;
function stationPeerSessionAgents(d) {
  try {
    const sessions = (Array.isArray(d.sessions) ? d.sessions : [])
      .filter(s => s && s.session_id && !s.is_primary && !s.ephemeral)
      .sort((a, b) => String(b.started_at || '').localeCompare(String(a.started_at || '')))
      .slice(0, PEER_SESSION_SCENE_NODES);
    const hostKey = sanitizeStationId(d.host_id);
    const peerNodeId = 'peer-' + hostKey;
    const ids = new Set(sessions.map(s => String(s.session_id)));
    const out = [];
    for (const s of sessions) {
      const id = String(s.session_id);
      const source = normalizeAgentId(s.source || '') || '';
      const kind = String(s.relationship || '').trim();
      const goal = normalizeSessionGoal(s.goal || null);
      const phase = stationPhaseFromPeerSession(s.phase);
      const parentSid = String(s.parent_session_id || '').trim();
      const parentNodeId = parentSid && ids.has(parentSid)
        ? 'peer-session-' + hostKey + '-' + sanitizeStationId(parentSid)
        : peerNodeId;
      out.push({
        ...stationVitalsFields(s.vitals || null),
        id: 'peer-session-' + hostKey + '-' + sanitizeStationId(id),
        hostId: d.host_id,
        role: kind === 'subagent'
          ? 'sub-agent'
          : (source && source !== 'intendant' ? 'external' : 'session'),
        phase,
        status: phase === 'running' || phase === 'thinking'
          ? 'in_progress'
          : (phase === 'done' ? 'done' : phase),
        task: compactSessionText(s.label || '') || shortSessionId(id),
        provider: '',
        model: '',
        tokens: stationNum(s.tokens_used),
        tokenCap: 0,
        prompt: 0,
        completion: 0,
        cached: 0,
        cost: 0,
        turns: 0,
        turnCap: 0,
        autonomy: '',
        worktree: '',
        parentId: parentNodeId,
        needsApproval: !!s.needs_approval,
        approvalId: null,
        approvalCommand: '',
        approvalCategory: '',
        sessionId: id,
        source,
        relationshipKind: kind,
        goalStatus: goal ? String(goal.status || '') : '',
        goalObjective: goal ? String(goal.objective || '') : '',
        goalTokens: goal && goal.tokensUsed !== null && goal.tokensUsed !== undefined
          ? String(goal.tokensUsed)
          : '',
        threadActions: [],
        canInterrupt: false,
      });
    }
    return out;
  } catch (err) {
    // Same degradation contract as stationSessionAgents: a bad peer
    // row must not freeze the whole Station update loop.
    console.warn('Station peer-session feed failed:', err);
    return [];
  }
}

function stationApprovalAgent(hostId, approvalId, approval) {
  return {
    id: 'approval-' + sanitizeStationId(hostId) + '-' + sanitizeStationId(approvalId),
    hostId,
    role: 'sub-agent',
    phase: 'waiting',
    status: 'waiting_approval',
    task: approval.command || 'approval required',
    provider: '',
    model: '',
    tokens: 0,
    tokenCap: 100000,
    prompt: 0,
    completion: 0,
    cached: 0,
    cost: 0,
    turns: 0,
    turnCap: 0,
    autonomy: '',
    worktree: '',
    parentId: hostId === selfPeerId ? 'primary-agent' : 'peer-' + sanitizeStationId(hostId),
    needsApproval: true,
    approvalId: String(approvalId),
    approvalCommand: approval.command || '',
    approvalCategory: approval.category || '',
  };
}

// Phase B: project live session windows into the scene — one node per
// session, wired to its parent by the session_relationship data. The
// daemon's own main session stays represented by the 'primary-agent'
// node; everything else (supervisor sessions, fork children, in-band
// task-* sub-agents) becomes a session node orbiting this host, ringed
// by its context pressure and glowing when its approval is pending.
function stationSessionAgents() {
  try {
    return stationSessionAgentsInner();
  } catch (err) {
    // A feed failure must degrade to "no session nodes", never break the
    // whole Station update loop (buildStationSnapshot throwing would
    // freeze every panel).
    console.warn('Station session-node feed failed:', err);
    return [];
  }
}

function stationSessionAgentsInner() {
  const out = [];
  const primaryIds = new Set();
  if (currentSessionFullId) {
    primaryIds.add(String(currentSessionFullId));
    try {
      for (const id of relatedSessionIdsForSession(currentSessionFullId) || []) {
        primaryIds.add(String(id));
      }
    } catch (_) {}
  }
  const liveIds = new Set();
  for (const [sid, win] of sessionWindows) {
    const id = String(sid);
    if (!win || win.ended || sessionWindowIsDetached(id)) continue;
    if (primaryIds.has(id)) continue;
    liveIds.add(id);
  }
  for (const id of liveIds) {
    const win = sessionWindows.get(id);
    const meta = sessionMetadataById.get(id) || {};
    const usage = (sessionUsageById.get(id) || {}).main || {};
    const goal = meta.goal && typeof meta.goal === 'object' ? meta.goal : null;
    const phase = normalizeStationPhase(win.phase || 'idle');
    const active = isSessionWindowEffectivelyActive(id);
    const parentSid = String(meta.parentId || '').trim();
    let parentNodeId = null;
    if (parentSid) {
      if (primaryIds.has(parentSid)) parentNodeId = 'primary-agent';
      else if (liveIds.has(parentSid)) parentNodeId = 'session-' + sanitizeStationId(parentSid);
    }
    const kind = String(meta.relationshipKind || '').trim();
    const source = normalizeAgentId(
      externalSourceForSessionWindow(id, win) || meta.backendSource || meta.source || ''
    ) || '';
    let needsApproval = false;
    if (pendingApprovalSessionId) {
      needsApproval = pendingApprovalSessionId === id;
      if (!needsApproval) {
        try {
          needsApproval = (relatedSessionIdsForSession(id) || []).includes(pendingApprovalSessionId);
        } catch (_) {}
      }
    }
    const name = compactSessionText(
      meta.name || meta.display_name || meta.displayName || meta.title || ''
    );
    const task = name
      || compactSessionText(meta.initial_message || meta.initialMessage || meta.task || '')
      || shortSessionId(id);
    out.push({
      ...stationVitalsFields(meta.vitals || null),
      id: 'session-' + sanitizeStationId(id),
      hostId: selfPeerId,
      role: kind === 'subagent'
        ? 'sub-agent'
        : (source && source !== 'intendant' ? 'external' : 'session'),
      phase,
      status: active ? 'in_progress' : (phase === 'done' ? 'done' : phase),
      task,
      provider: usage.provider || '',
      model: usage.model || '',
      tokens: Number(usage.tokens_used) || 0,
      tokenCap: Number(usage.context_window) || 200000,
      prompt: Number(usage.prompt_tokens) || 0,
      completion: Number(usage.completion_tokens) || 0,
      cached: Number(usage.cached_tokens) || 0,
      cost: 0,
      turns: 0,
      turnCap: 0,
      autonomy: '',
      worktree: String(meta.projectRoot || meta.cwd || ''),
      parentId: parentNodeId,
      needsApproval,
      approvalId: needsApproval && pendingApprovalId !== null && pendingApprovalId !== undefined
        ? String(pendingApprovalId)
        : null,
      approvalCommand: '',
      approvalCategory: '',
      sessionId: id,
      source,
      relationshipKind: kind,
      goalStatus: goal ? String(goal.status || '') : '',
      goalObjective: goal ? String(goal.objective || '') : '',
      goalTokens: goal && goal.tokensUsed !== null && goal.tokensUsed !== undefined
        ? String(goal.tokensUsed)
        : '',
      threadActions: sessionThreadActionOps(id) || [],
      canInterrupt: active,
    });
  }
  // Recent (closed-window) sessions join as dim, inert nodes. The scene is
  // deliberately a bounded constellation — the freshest few, not the whole
  // archive; the sessions panel remains the exhaustive list.
  const RECENT_SCENE_NODES = 6;
  try {
    const { sortedSessions } = stationCollectSessionSet();
    let added = 0;
    for (const session of sortedSessions) {
      if (added >= RECENT_SCENE_NODES) break;
      const id = String(
        session?.session_id || session?.resume_id || session?.backend_session_id || ''
      ).trim();
      if (!id || primaryIds.has(id) || liveIds.has(id)) continue;
      // The daemon's own log id may differ from currentSessionFullId —
      // it is the primary node, not an archived session.
      if (id === String(daemonSessionFullId || '') || id === String(foregroundSessionFullId || '')) continue;
      const meta = sessionMetadataById.get(id) || {};
      const kind = String(meta.relationshipKind || '').trim();
      const source = normalizeAgentId(
        session?.backend_source || session?.source || meta.backendSource || meta.source || ''
      ) || '';
      const goal = meta.goal && typeof meta.goal === 'object' ? meta.goal : null;
      const parentSid = String(meta.parentId || '').trim();
      let parentNodeId = null;
      if (parentSid) {
        if (primaryIds.has(parentSid)) parentNodeId = 'primary-agent';
        else if (liveIds.has(parentSid)) parentNodeId = 'session-' + sanitizeStationId(parentSid);
      }
      const name = compactSessionText(session?.name || meta.name || meta.displayName || '');
      const task = name
        || compactSessionText(session?.task || meta.initial_message || meta.initialMessage || '')
        || shortSessionId(id);
      out.push({
        id: 'session-' + sanitizeStationId(id),
        hostId: selfPeerId,
        role: kind === 'subagent'
          ? 'sub-agent'
          : (source && source !== 'intendant' ? 'external' : 'session'),
        phase: 'idle',
        status: 'idle',
        task,
        provider: String(session?.provider || ''),
        model: String(session?.model || ''),
        tokens: stationNum(session?.total_tokens),
        tokenCap: 0,
        prompt: stationNum(session?.prompt_tokens),
        completion: stationNum(session?.completion_tokens),
        cached: stationNum(session?.cached_tokens),
        cost: 0,
        turns: stationNum(session?.turns),
        turnCap: 0,
        autonomy: '',
        worktree: String(sessionProjectDirectory(session) || ''),
        parentId: parentNodeId,
        needsApproval: false,
        approvalId: null,
        approvalCommand: '',
        approvalCategory: '',
        sessionId: id,
        source,
        relationshipKind: kind,
        goalStatus: goal ? String(goal.status || '') : '',
        goalObjective: goal ? String(goal.objective || '') : '',
        goalTokens: '',
        threadActions: [],
        canInterrupt: false,
        recent: true,
      });
      added += 1;
    }
  } catch (_) {}
  return out;
}

function stationUsageForHost(hostId) {
  const c = hostStatsCache.get(hostId) || (hostId === selfPeerId ? hostStatsCache.get('local') : null) || {};
  let main = {};
  let cost = {};
  try { main = c.main_json ? JSON.parse(c.main_json) : {}; } catch (_) {}
  try { cost = c.cost_json ? JSON.parse(c.cost_json) : {}; } catch (_) {}
  const tokens = stationNum(main.total_tokens ?? main.tokens_used ?? main.tokens ?? c.total_tokens ?? 0);
  return {
    provider: main.provider || c.provider || '',
    model: main.model || c.model || '',
    tokens,
    tokenCap: stationNum(main.context_window || main.token_cap || c.context_window || 200000),
    prompt: stationNum(main.prompt_tokens || main.input_tokens || c.prompt_tokens || 0),
    completion: stationNum(main.completion_tokens || main.output_tokens || c.completion_tokens || 0),
    cached: stationNum(main.cached_tokens || c.cached_tokens || 0),
    cost: stationNum(cost.total || c.cost_usd || 0),
    turns: stationNum(main.turn || main.turns || c.turns || 0),
    turnCap: stationNum(main.turn_cap || c.turn_cap || 0),
    cpu: stationNum(main.cpu || main.cpu_pct || c.cpu || c.cpu_pct || stableStationMetric(hostId, 18, 34)),
    mem: stationNum(main.mem || main.mem_pct || c.mem || c.mem_pct || stableStationMetric(hostId + ':mem', 32, 28)),
  };
}

function daemonPlatformLabel(d) {
  const caps = Array.isArray(d.capabilities) ? d.capabilities.map(capabilityLabel).join(', ') : '';
  return caps || d.version || 'peer';
}

function normalizeStationPhase(phase) {
  const p = (phase || '').toLowerCase();
  if (p.includes('thinking')) return 'thinking';
  if (p.includes('running') || p.includes('orchestrating') || p.includes('interrupting')) return 'running';
  if (p.includes('waiting')) return 'waiting';
  if (p.includes('done')) return 'done';
  return 'idle';
}

function sanitizeStationId(s) {
  return String(s || 'id').replace(/[^a-zA-Z0-9_-]/g, '_');
}

function stationNum(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

function stationTimestampMs(value) {
  const ms = Date.parse(value || '');
  return Number.isFinite(ms) ? ms : 0;
}

function stationCompactNumber(value) {
  const n = stationNum(value);
  const abs = Math.abs(n);
  if (abs >= 1000000) return `${(n / 1000000).toFixed(abs >= 10000000 ? 0 : 1)}m`;
  if (abs >= 1000) return `${(n / 1000).toFixed(abs >= 10000 ? 0 : 1)}k`;
  return Math.round(n).toString();
}

function stableStationMetric(seed, base, span) {
  let h = 0;
  const s = String(seed || '');
  for (let i = 0; i < s.length; i++) h = ((h << 5) - h + s.charCodeAt(i)) | 0;
  return base + Math.abs(h % span);
}

function stationActivityTimestampLabel() {
  return new Date().toTimeString().substring(0, 8);
}

function stationTrimActivityEvents() {
  if (stationLogEvents.length > STATION_ACTIVITY_EVENT_LIMIT) {
    stationLogEvents.splice(0, stationLogEvents.length - STATION_ACTIVITY_EVENT_LIMIT);
  }
}

function stationUpsertActivityEvent(event) {
  if (!event) return;
  const hostId = event.hostId || event.host_id || selfPeerId || 'local';
  const sessionId = event.sessionId || event.session_id || '';
  const id = stationActivityEventKey(event) || stationLogEventId({
    host_id: hostId,
    session_id: sessionId,
    ts: event.ts || '',
    source: event.source || event.event || event.level || 'info',
    content: event.msg || event.content || event.message || '',
  }, stationLogEvents.length);
  const next = {
    id,
    action: event.action || 'log',
    hostId,
    sessionId,
    agentId: event.agentId || (hostId === selfPeerId ? 'primary-agent' : 'peer-' + sanitizeStationId(hostId)),
    ts: event.ts || stationActivityTimestampLabel(),
    level: event.level || 'info',
    source: event.source || event.event || event.level || 'info',
    msg: compactSessionTextBounded(
      event.msg || event.content || event.message || '',
      STATION_ACTIVITY_TEXT_CHAR_LIMIT
    ),
  };
  const existingIndex = stationLogEvents.findIndex(row => stationActivityEventKey(row) === id);
  if (existingIndex >= 0) stationLogEvents[existingIndex] = { ...stationLogEvents[existingIndex], ...next };
  else stationLogEvents.push(next);
  stationTrimActivityEvents();
  stationScheduleUpdate();
}

function stationPublishActivityEvent(event, options = {}) {
  if (!event) return;
  stationUpsertActivityEvent(event);
  if (options.renderLog === false) return;
  renderLogEntry({
    level: event.level || 'info',
    source: event.source || event.event || event.level || 'info',
    content: event.msg || event.content || event.message || '',
    ts: event.ts || stationActivityTimestampLabel(),
    session_id: event.sessionId || event.session_id || '',
    host_id: event.hostId || event.host_id || selfPeerId || 'local',
    station_event_id: event.id || '',
  });
}

function stationPushLogEvent(c) {
  const hostId = c.host_id || selfPeerId;
  const eventId = stationLogEventId(c, stationLogEvents.length);
  stationLogEvents.push({
    id: eventId,
    action: 'log',
    hostId,
    sessionId: c.session_id || c.sessionId || '',
    agentId: hostId === selfPeerId ? 'primary-agent' : 'peer-' + sanitizeStationId(hostId),
    ts: (c.ts || new Date().toTimeString()).substring(0, 8),
    level: c.level || 'info',
    source: c.source || c.event || c.level || 'info',
    msg: compactSessionTextBounded(c.content || c.source || c.event || '', STATION_ACTIVITY_TEXT_CHAR_LIMIT),
  });
  stationTrimActivityEvents();
  stationTrackLogAnchor(c);
}

function stationThreadActionActivityId(sessionId, op) {
  const sid = String(sessionId || '').trim() || 'target';
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  return `codex-thread-action:${sid}:${normalizedOp || 'action'}`;
}

function stationThreadActionActivityMessage(op, sessionId, status, message = '') {
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-') || 'action';
  const sid = String(sessionId || '').trim();
  const target = sid ? ` for ${shortSessionId(sid)}` : '';
  const detail = compactSessionText(message || '').trim();
  if (status === 'attaching') return `/${normalizedOp} waiting to attach${target}`;
  if (status === 'timeout') return `/${normalizedOp} still pending${target}; no result yet`;
  if (status === 'ok') return `/${normalizedOp} ok${target}${detail ? `: ${detail}` : ''}`;
  if (status === 'failed') return `/${normalizedOp} failed${target}${detail ? `: ${detail}` : ''}`;
  return `/${normalizedOp} requested${target}`;
}

function stationUpsertCodexThreadActionActivity(op, sessionId, status = 'requested', message = '', options = {}) {
  const sid = String(sessionId || '').trim();
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  if (!normalizedOp) return;
  const level = status === 'failed' ? 'error' : (status === 'timeout' ? 'warn' : 'info');
  stationPublishActivityEvent({
    id: stationThreadActionActivityId(sid, normalizedOp),
    action: 'codex_thread_action',
    hostId: selfPeerId || 'local',
    sessionId: sid,
    level,
    source: 'codex',
    msg: stationThreadActionActivityMessage(normalizedOp, sid, status, message),
  }, options);
}

function stationUserMessageEditActivityId(sessionId, userTurnIndex) {
  const sid = String(sessionId || '').trim() || 'target';
  const turn = Number(userTurnIndex || 0);
  return `user-message-edit:${sid}:${Number.isFinite(turn) && turn > 0 ? turn : 'turn'}`;
}

function stationUserMessageEditActivityMessage(sessionId, userTurnIndex, status, message = '') {
  const sid = String(sessionId || '').trim();
  const turn = Number(userTurnIndex || 0);
  const turnText = Number.isFinite(turn) && turn > 0 ? ` user turn ${turn}` : ' selected user turn';
  const target = sid ? ` for ${shortSessionId(sid)}` : '';
  const detail = compactSessionText(message || '').trim();
  if (status === 'attaching') return `Edit waiting to attach${target} before${turnText}${detail ? `: ${detail}` : ''}`;
  if (status === 'queued') return `Edit queued${target} for${turnText}${detail ? `: ${detail}` : ''}`;
  if (status === 'running') return `Edit applying${target} to${turnText}${detail ? `: ${detail}` : ''}`;
  if (status === 'ok') return `Edit applied${target} to${turnText}${detail ? `: ${detail}` : ''}`;
  if (status === 'failed') return `Edit failed${target} for${turnText}${detail ? `: ${detail}` : ''}`;
  return `Edit requested${target} for${turnText}${detail ? `: ${detail}` : ''}`;
}

function stationUpsertUserMessageEditActivity(sessionId, userTurnIndex, status = 'requested', message = '', options = {}) {
  const sid = String(sessionId || '').trim();
  const normalizedStatus = String(status || '').trim().toLowerCase() || 'requested';
  const turn = Number(userTurnIndex || 0);
  const level = normalizedStatus === 'failed' ? 'error' : 'info';
  stationPublishActivityEvent({
    id: stationUserMessageEditActivityId(sid, turn),
    action: 'user_message_edit',
    hostId: selfPeerId || 'local',
    sessionId: sid,
    level,
    source: 'edit',
    msg: stationUserMessageEditActivityMessage(sid, turn, normalizedStatus, message),
  }, options);
}

function stationPushSessionRelationshipActivity(evt = {}, options = {}) {
  const parent = String(evt.parent_session_id || evt.parentSessionId || evt.parent_id || evt.parentId || '').trim();
  const child = String(evt.child_session_id || evt.childSessionId || evt.child_id || evt.childId || evt.session_id || evt.sessionId || '').trim();
  const relationship = String(evt.relationship || evt.kind || '').trim().toLowerCase() || 'relationship';
  if (!parent && !child) return;
  const label = relationship === 'fork' ? 'Fork' : (relationship === 'side' ? 'Side thread' : (relationship === 'subagent' ? 'Subagent' : 'Session relationship'));
  const parentLabel = parent ? shortSessionId(parent) : 'unknown';
  const childLabel = child ? shortSessionId(child) : 'unknown';
  stationPublishActivityEvent({
    id: `session-relationship:${relationship}:${parent || 'none'}:${child || 'none'}`,
    action: 'session_relationship',
    hostId: selfPeerId || 'local',
    sessionId: child || parent,
    level: 'info',
    source: 'session',
    msg: `${label}: ${parentLabel} -> ${childLabel}${evt.ephemeral ? ' (ephemeral)' : ''}`,
  }, options);
}

function stationTrackLogAnchorRow(itemId, sessionId, detail, value = 'log') {
  const id = String(itemId || '').trim();
  if (!id) return;
  const sid = String(sessionId || '').trim();
  const key = `${sid}\u001f${id}`;
  const isNew = !stationLogAnchorRows.has(key);
  if (isNew) stationLogAnchorKeys.push(key);
  const row = {
    id,
    sessionId: sid,
    value: value || 'log',
    detail: compactSessionTextBounded(detail || '', STATION_ANCHOR_DETAIL_CHAR_LIMIT),
    seq: isNew ? ++stationLogAnchorSeq : (stationLogAnchorRows.get(key)?.seq ?? ++stationLogAnchorSeq),
  };
  stationLogAnchorRows.set(key, row);
  stationAnchorIndexUpsert(row, isNew);
  while (stationLogAnchorKeys.length > STATION_LOG_ANCHOR_LIMIT) {
    const oldestKey = stationLogAnchorKeys.shift();
    if (!oldestKey) break;
    const evicted = stationLogAnchorRows.get(oldestKey);
    stationLogAnchorRows.delete(oldestKey);
    if (evicted) stationAnchorIndexEvict(evicted);
  }
}

function stationAnchorIndexUpsert(row, isNew) {
  let entry = stationAnchorsBySession.get(row.sessionId);
  if (!entry) {
    entry = { ids: new Set(), tail: [] };
    stationAnchorsBySession.set(row.sessionId, entry);
  }
  const tailIndex = entry.tail.findIndex(item => item.id === row.id);
  if (tailIndex >= 0) {
    entry.tail[tailIndex] = row;
    return;
  }
  if (!isNew) return; // updated row already aged out of the tail; keep order
  entry.ids.add(row.id);
  stationAnchorIdRefs.set(row.id, (stationAnchorIdRefs.get(row.id) || 0) + 1);
  entry.tail.push(row);
  if (entry.tail.length > STATION_ANCHOR_TAIL_LIMIT) entry.tail.shift();
}

function stationAnchorIndexEvict(row) {
  const entry = stationAnchorsBySession.get(row.sessionId);
  if (!entry) return;
  entry.ids.delete(row.id);
  const tailIndex = entry.tail.findIndex(item => item.id === row.id);
  if (tailIndex >= 0) entry.tail.splice(tailIndex, 1);
  if (!entry.ids.size) stationAnchorsBySession.delete(row.sessionId);
  const refs = (stationAnchorIdRefs.get(row.id) || 0) - 1;
  if (refs > 0) stationAnchorIdRefs.set(row.id, refs);
  else stationAnchorIdRefs.delete(row.id);
}

function stationTrackLogAnchor(c) {
  const itemId = c?.item_id || c?.itemId || '';
  if (!itemId) return;
  stationTrackLogAnchorRow(
    itemId,
    c?.session_id || c?.sessionId || '',
    c?.content || c?.summary || c?.message || c?.source || c?.event || '',
  );
}

function stationTrackSessionWindowHistoryAnchor(item, fallbackSessionId = '') {
  const record = sessionWindowHistoryRecord(item);
  if (!record) return;
  const itemId = record.item_id || record.itemId || '';
  if (!itemId) return;
  const sessionId = record.session_id || record.sessionId || fallbackSessionId || '';
  const detail = record.content || record.summary || record.message || record.source || record.event || '';
  stationTrackLogAnchorRow(itemId, sessionId, detail, 'log');
}

function stationClearLogState() {
  stationLogEvents.length = 0;
  stationLogAnchorRows.clear();
  stationLogAnchorKeys.length = 0;
  stationAnchorsBySession.clear();
  stationAnchorIdRefs.clear();
  stationLogAnchorSeq = 0;
}

function stationRegisterVideoSource(sourceId, hostId, displayId, label, kind, videoEl) {
  if (!station || !videoEl || !sourceId) return;
  // Videos parked in offscreen containers don't (re)start on autoplay;
  // a paused element renders as a frozen black pane in the Station. Kick
  // play() on every registration — covers local slots, peer panes, and
  // re-registrations after DOM re-renders alike.
  if (videoEl.paused && videoEl.srcObject) videoEl.play().catch(() => {});
  try {
    station.register_display_source(String(sourceId), String(hostId), String(displayId), String(label), String(kind || 'video'), videoEl);
    stationRegisteredSources.add(String(sourceId));
    // Coalesced: tab activation re-registers every source in a loop, and a
    // synchronous full rebuild per source multiplies the build cost.
    stationScheduleUpdate();
  } catch (e) {
    console.warn('station display registration failed:', e);
  }
}

function stationUnregisterVideoSource(sourceId) {
  if (!sourceId) return;
  stationRegisteredSources.delete(String(sourceId));
  if (station) station.unregister_display_source(String(sourceId));
}

function stationRegisterExistingSources() {
  if (!station) return;
  for (const slot of displaySlots.values()) {
    if (slot.videoEl) {
      stationRegisterVideoSource(
        `local:${slot.displayId}`,
        selfPeerId,
        String(slot.displayId),
        `${selfHostLabel || 'local'} :${slot.displayId}`,
        'local',
        slot.videoEl,
      );
    }
  }
  for (const conn of peerDisplayConnections.values()) {
    const container = stationPeerDisplayContainer(conn.hostId, false);
    const video = container && container.querySelector('.peer-display-video');
    if (video) {
      stationRegisterVideoSource(
        `peer:${conn.hostId}:${conn.displayId}:${conn.sessionId}`,
        conn.hostId,
        String(conn.displayId),
        `${stationHostLabel(conn.hostId)} :${conn.displayId}`,
        'peer',
        video,
      );
    }
  }
}

function stationHostLabel(hostId) {
  if (hostId === selfPeerId) return selfHostLabel || 'local';
  const d = daemons.find(x => x.host_id === hostId);
  return d && (d.label || d.host_id) || hostId;
}

function stationPeerDisplayContainer(hostId, createFallback, preferFallback = false) {
  const id = `peer-display-${hostId}`;
  const fallbackId = `station-peer-display-${hostId}`;
  let fallback = document.getElementById(fallbackId);
  const ensureFallback = () => {
    if (!fallback && createFallback) {
      const root = document.getElementById('station-display-endpoints') || document.body;
      fallback = document.createElement('div');
      fallback.id = fallbackId;
      fallback.className = 'peer-display-container station-peer-display-endpoint';
      fallback.style.cssText = 'position:absolute;left:-10000px;top:-10000px;width:320px;height:180px;opacity:0;pointer-events:none;overflow:hidden;';
      root.appendChild(fallback);
    }
    return fallback;
  };
  if (preferFallback) return ensureFallback();
  const existing = document.getElementById(id);
  if (existing) {
    if (fallback && !stationPeerDisplayPrefersStationEndpoint()) fallback.remove();
    return existing;
  }
  return ensureFallback();
}

function stationPeerDisplayPrefersStationEndpoint() {
  return activeTab === 'station'
    || !!document.getElementById('tab-station')?.classList.contains('active');
}

function stationPeerDisplayContainersForHost(hostId) {
  const containers = [];
  const seen = new Set();
  for (const id of [`peer-display-${hostId}`, `station-peer-display-${hostId}`]) {
    const el = document.getElementById(id);
    if (el && !seen.has(el)) {
      seen.add(el);
      containers.push(el);
    }
  }
  return containers;
}

// Activity display strip
const displayThumbs = new Map();
let stripExpanded = false;
let stripMinimized = false;
let stripHeight = 280;

// Recording replay
const recordingStreams = new Map(); // stream_name -> {segments, totalDuration, manifest}
let activeRecordingStream = null;
let recPlayer = null;

// Voice / Live mode
let gatewayConfig = null;
let currentExternalAgent = null;
let micActive = false;
let modelConnected = false;
let audioCtx = null;
let mediaStream = null;
let workletNode = null;
let workletReady = false;
let isActiveBrowser = true;
let storedConversationCtx = null;
let voiceConnecting = false;
let voiceHadPriorSession = false;
let audioDropLogCount = 0;
const audioQueue = [];
let isPlaying = false;

// Video / Frame capture
let videoActive = false;
let videoStream = null;
let videoIntervalId = null;
let frameCounter = 0;
const FRAME_STREAM = 'cam0';
const LIVE_RES = 768;  // 768x768 for live model
const VIDEO_FPS = 1;
let lastLiveFrameLen = 0; // For frame dedup — skip if JPEG size barely changed
let tickerFramesSent = 0;
let tickerFramesDropped = 0;
let tickerExpanded = false;

