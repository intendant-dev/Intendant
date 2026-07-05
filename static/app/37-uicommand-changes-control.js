// ── UiCommand Processor ──
function processCommands(cmds) {
  if (!Array.isArray(cmds)) return;
  const previousLogReplayBatch = logReplayAppendBatch;
  const createdLogReplayBatch = processingLogReplay && !logReplayAppendBatch;
  if (createdLogReplayBatch) logReplayAppendBatch = createLogReplayAppendBatch();
  let pendingStatusPhaseGuard = null;
  try {
    for (const c of cmds) {
      if (c.cmd !== 'update_status_bar' && c.cmd !== 'set_phase') {
        pendingStatusPhaseGuard = null;
      }
      switch (c.cmd) {
      case 'add_log_entry':
        maybeFailNewSessionSpawnFromLog(c);
        stationPushLogEvent(c);
        renderLogEntry(c);
        applyThreadHistoryChangeSet(c);
        stationScheduleUpdate();
        break;
      case 'mark_activity_context_rewind':
        flushLogReplayAppendBatch();
        if (!applyThreadHistoryChangeSet(c)) {
          markActivityContextRewind(c.session_id, c.user_turn_index, c.turns_removed);
        }
        break;
      case 'user_message_edit_status':
        handleUserMessageEditStatus(c);
        break;
      case 'user_message_rewind':
        handleUserMessageRewind(c);
        break;
      case 'clear_logs':
        flushLogReplayAppendBatch();
        clearLogs({ clearSessionWindows: !processingLogReplay });
        stationClearLogState();
        stationScheduleUpdate();
        break;
      case 'add_turn_separator':
        flushLogReplayAppendBatch();
        renderTurnSeparator(c.turn);
        break;
      case 'update_status_bar':
        if (!processingLogReplay) {
          recordRecentSessionStatusPhase(c.session_id, c.phase);
          applyServerPhaseToSessionWindow(c.session_id, c.phase);
        }
        pendingStatusPhaseGuard = { allow: shouldApplyStatusPhaseForSession(c.session_id) };
        updateStatusBar(c);
        stationScheduleUpdate();
        break;
      case 'set_phase': {
        const guard = pendingStatusPhaseGuard;
        pendingStatusPhaseGuard = null;
        if (guard && !guard.allow) break;
        setPhase(c.phase);
        break;
      }
      case 'show_approval': showApproval(c.id, c.command, c.category, c.session_id); break;
      case 'hide_approval': clearPendingApproval(); hidePanel('approval-panel'); break;
      case 'show_human_input': showHumanInput(c.question); break;
      case 'hide_human_input': hidePanel('human-panel'); break;
      case 'hide_all_panels': hideAllPanels(); break;
      case 'update_usage':
        cacheSessionUsage(c);
        // Cache the payload under the self host so the Stats host
        // picker can re-render it on demand. Only drive the live
        // rendering path when the picker is on self (or unset) — when
        // the user is viewing a secondary's stats, primary updates
        // silently refresh the cache without stomping the view.
        hostStatsCache.set(selfPeerId, c);
        if (isStatsShowingSelf()) {
          renderUsageTab(c);
          if (activeTab === 'stats') {
            const statsHost = currentStatsHostKey();
            if (!Array.isArray(cachedStatsSessions(statsHost))) {
              loadAllSessionsUsage(statsHost, { force: false });
            }
          }
        }
        updateTickerFromUsage(c);
        renderForegroundSessionUsage();
        scheduleManagedContextRefresh(700);
        stationScheduleUpdate();
        break;
      case 'add_display':
        addDisplaySlot(c.display_id, c.width, c.height);
        stationScheduleUpdate();
        break;
      case 'add_recording': addRecordingStream(c.stream_name); break;
      case 'remove_recording': removeRecordingStream(c.stream_name); break;
      case 'delete_recording': deleteRecordingStream(c.stream_name); break;
      case 'recording_error': /* logged via add_log_entry */ break;
      case 'session_started':
        managedContextSessionManuallySelected = false;
        onSessionStarted(c.session_id, c.task);
        scheduleManagedContextRefresh(400);
        stationScheduleUpdate();
        break;
      case 'session_relationship':
        applySessionRelationship(c);
        stationPushSessionRelationshipActivity(c);
        break;
      case 'session_capabilities': applySessionCapabilities(c); break;
      case 'session_goal': applySessionGoal(c); break;
      case 'session_vitals': applySessionVitals(c); break;
      case 'session_attached':
        onSessionAttached(c.session_id, c.source);
        stationScheduleUpdate();
        break;
      case 'session_ended':
        onSessionEnded(c.session_id, c.reason);
        stationScheduleUpdate();
        break;
      case 'external_agent_changed': updateStatusBar({ external_agent: c.agent }); break;
      case 'debug_screen_ready': onDebugScreenReady(c.display_id); break;
      case 'debug_screen_torn_down': onDebugScreenTornDown(); break;
      case 'show_badge': showBadge(c.tab, c.text); break;
      case 'hide_badge': hideBadge(c.tab); break;
      case 'set_connected':
        if (dashboardConnectModeEnabled()) {
          setConnectEventStatus(
            dashboardTransport?.canUseRpc?.() ? 'ok' : 'warn',
            'Dashboard events are routed by the Connect WebRTC tunnel in public-origin mode'
          );
        } else {
          setServerWebSocketStatus(c.connected);
        }
        break;
      case 'file_changed':
        onFileChanged(c.path, c.kind, c.lines_added, c.lines_removed);
        break;
      case 'upload_ready':
        onUploadReady(c.descriptor, { fromBroadcast: true });
        break;
      case 'upload_deleted':
        onUploadDeleted(c.id);
        break;
      case 'history_changed':
        // Session history changed (snapshot / rollback / redo / prune).
        // Re-fetch the authoritative timeline and file list. Rollback/redo
        // rewrites files while suppressing duplicate watcher events, so the
        // Changes pane must refresh from the API instead of waiting for fs
        // notifications.
        refreshHistory();
        refreshChangesList({
          selectFirst: activeActivitySubtab === 'changes',
          refreshActive: !!activeChangesFile,
          quiet: true,
        });
        break;
      case 'claude_config_changed':
        applyClaudeConfigChanged(c);
        break;
      case 'codex_config_changed':
        applyCodexConfigChanged(c);
        break;
      case 'codex_thread_action_requested':
        handleCodexThreadActionRequested(c);
        break;
      case 'codex_thread_action_result':
        handleCodexThreadActionResult(c);
        scheduleManagedContextRefresh(700);
        break;
      case 'session_rename_result':
        handleSessionRenameResult(c);
        break;
      // Peer registry push events. The peer payload is a `PeerSnapshot`
      // (same shape `refreshPeersFromApi` builds entries from), so we
      // funnel through the shared upsert/remove helpers — one code path
      // applies API entries and pushed deltas identically.
      case 'peer_added':
        upsertDaemonFromSnapshot(c.peer);
        stationScheduleUpdate();
        break;
      // peer_state_changed must NOT insert an unknown id — see
      // updateDaemonSnapshot for why (race with remove_peer's
      // trailing observer events).
      case 'peer_state_changed':
        updateDaemonSnapshot(c.peer);
        stationScheduleUpdate();
        break;
      case 'peer_removed':
        removeDaemonById(c.id);
        stationScheduleUpdate();
        break;
      // Mid-turn steer lifecycle: pending (just sent), accepted
      // (backend/runtime accepted it), queued (Intendant fallback),
      // delivered (agent got it).
      case 'steer_status_update':
        onSteerStatusUpdate(c.id, c.text, c.status, c.reason, { sessionId: c.session_id || c.sessionId });
        break;
      case 'follow_up_status':
        handleFollowUpStatusUpdate(c);
        break;
      // Per-peer event stream (phase B of the secondary migration).
      // Each command carries `host_id` as a typed field so we route
      // straight to the per-peer surfaces without the legacy
      // `c.host_id = hostId` mutation hack from the secondary path.
      case 'peer_log':
        {
        const peerEntry = {
          cmd: 'add_log_entry',
          host_id: c.host_id,
          ts: c.ts,
          level: c.level,
          source: c.source,
          content: c.content,
        };
        stationPushLogEvent(peerEntry);
        renderLogEntry(peerEntry);
        stationScheduleUpdate();
        }
        break;
      case 'peer_usage': {
        // Translate the lean PeerEvent::Usage snapshot into the
        // legacy UpdateUsage shape that the Stats tab's existing
        // renderer (renderUsageTab) consumes. Same cache + render
        // call as the secondary path used to do, but sourced from
        // the push pipeline. This is what closes out phase B by
        // making Stats per-peer work without the WASM secondary
        // connection.
        const fakeUsage = peerSnapshotToUpdateUsage(c.snapshot);
        hostStatsCache.set(c.host_id, fakeUsage);
        if (activeStatsHost === c.host_id) renderUsageTab(fakeUsage);
        stationScheduleUpdate();
        break;
      }
      case 'peer_approval_requested':
        addPendingApproval(c.host_id, c.id, c.command, c.category);
        stationScheduleUpdate();
        break;
      case 'peer_approval_resolved':
        removePendingApproval(c.host_id, c.id);
        stationScheduleUpdate();
        break;
      case 'peer_webrtc_signal':
        // Per-peer WebRTC signaling: route to the matching
        // RTCPeerConnection (keyed by host_id|display_id|session_id)
        // so the browser can complete the offer/answer/ICE handshake
        // and start receiving the peer's display directly. See
        // PeerDisplayConnection / handlePeerWebRtcSignal below.
        handlePeerWebRtcSignal(c.host_id, c.display_id, c.session_id, c.signal);
        break;
      case 'peer_file_transfer_signal':
        handlePeerFileTransferSignal(c.host_id, c.session_id, c.signal);
        break;
      case 'peer_dashboard_control_signal':
        handlePeerDashboardControlSignal(c.host_id, c.session_id, c.signal);
        break;
      }
    }
  } finally {
    if (createdLogReplayBatch) {
      flushLogReplayAppendBatch();
      logReplayAppendBatch = previousLogReplayBatch;
      stationScheduleUpdate();
    }
  }
}

// ── DOM Helpers ──

// ── Changes sub-tab ──

const changedFiles = new Map(); // path -> {kind, lines_added, lines_removed, diff_available, reason}
let activeChangesFile = null;
let changesRefreshTimer = null;
let changesRefreshSeq = 0;
let changesRenderFrame = null;
let pendingChangesAutoSelect = null;

function onFileChanged(path, kind, linesAdded, linesRemoved) {
  changedFiles.set(path, { kind, lines_added: linesAdded, lines_removed: linesRemoved });
  if (isChangesSubtabActive() && !activeChangesFile) {
    pendingChangesAutoSelect = path;
  }
  queueChangesFileListRender();
  scheduleChangesRefresh();
  stationScheduleUpdate();
}

function queueChangesFileListRender() {
  if (changesRenderFrame) return;
  changesRenderFrame = requestAnimationFrame(() => {
    changesRenderFrame = null;
    renderChangesFileList();
    updateChangesBadge();
    stationScheduleUpdate();
    if (
      pendingChangesAutoSelect
      && isChangesSubtabActive()
      && !activeChangesFile
      && changedFiles.has(pendingChangesAutoSelect)
    ) {
      const path = pendingChangesAutoSelect;
      pendingChangesAutoSelect = null;
      selectChangesFile(path);
    } else {
      pendingChangesAutoSelect = null;
    }
  });
}

function isChangesSubtabActive() {
  const activeBtn = document.querySelector('#activity-subtabs .subtab-btn.active');
  return !!activeBtn && activeBtn.dataset.activityTab === 'changes';
}

function updateChangesBadge() {
  const badge = document.getElementById('badge-changes');
  if (!badge) return;
  if (changedFiles.size > 0 && !isChangesSubtabActive()) {
    badge.textContent = changedFiles.size;
    badge.style.display = '';
  } else {
    badge.textContent = '';
    badge.style.display = 'none';
  }
}

function changesStatsHtml(info) {
  if (info.diff_available === false) {
    return '<span class="changes-stats">no text diff</span>';
  }
  return `<span class="changes-stats">+${info.lines_added} &minus;${info.lines_removed}</span>`;
}

function renderChangesFileList(emptyMessage = 'No file changes yet') {
  const container = document.getElementById('changes-file-list');
  if (!container) return;
  if (changedFiles.size === 0) {
    container.innerHTML = `<div class="changes-empty">${escapeHtml(emptyMessage)}</div>`;
    return;
  }
  const sorted = [...changedFiles.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  container.innerHTML = sorted.map(([path, info]) => {
    const active = path === activeChangesFile ? ' active' : '';
    const initial = info.kind === 'created' ? 'A'
      : info.kind === 'deleted' ? 'D'
      : info.kind === 'external' ? 'X'
      : 'M';
    const badge = `<span class="kind-badge ${info.kind}">${initial}</span>`;
    const stats = changesStatsHtml(info);
    const parts = path.split('/');
    const name = parts.pop();
    const dir = parts.length > 0 ? parts.join('/') + '/' : '';
    return `<button type="button" class="changes-file-entry${active}" data-path="${escapeHtml(path)}" title="${escapeHtml(path)}">`
      + `${badge}<span class="changes-file-name">${escapeHtml(name)}</span>${stats}`
      + `<span class="changes-file-dir">${escapeHtml(dir)}</span></button>`;
  }).join('');
  container.querySelectorAll('.changes-file-entry').forEach(entry => {
    entry.addEventListener('click', () => selectChangesFile(entry.dataset.path || ''));
  });
}

function resetChangesPane(message = 'No file changes yet') {
  if (changesRenderFrame) {
    cancelAnimationFrame(changesRenderFrame);
    changesRenderFrame = null;
  }
  pendingChangesAutoSelect = null;
  changedFiles.clear();
  activeChangesFile = null;
  renderChangesFileList(message);
  renderChangesDiffHeader(null);
  const content = document.getElementById('changes-diff-content');
  if (content) content.innerHTML = `<span class="changes-empty">${escapeHtml(message)}</span>`;
  updateChangesBadge();
  stationScheduleUpdate();
}

function renderChangesDiffHeader(path, info, stateText = '') {
  const header = document.getElementById('changes-diff-header');
  if (!header) return;
  if (!path) {
    header.textContent = 'Select a file to view changes';
    return;
  }
  const meta = info ? `+${info.lines_added} / -${info.lines_removed}` : stateText;
  const resolvedMeta = info && info.diff_available === false ? 'No text diff' : meta;
  header.innerHTML = `<span class="changes-diff-path" title="${escapeHtml(path)}">${escapeHtml(path)}</span>`
    + (resolvedMeta ? `<span class="changes-diff-meta">${escapeHtml(resolvedMeta)}</span>` : '');
}

function encodeChangePath(path) {
  if (isAbsolutePath(path)) return encodeURIComponent(path);
  return String(path).split('/').map(part => encodeURIComponent(part)).join('/');
}

async function parseChangesResponse(resp) {
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok || data.error) {
    throw new Error(data.error || resp.statusText || `HTTP ${resp.status}`);
  }
  return data;
}

function normalizeChangesRootPath(value) {
  return String(value || '').trim().replace(/[\\/]+$/, '');
}

function currentChangesTargetMismatchReason() {
  const sid = String(currentSessionFullId || foregroundSessionFullId || '').trim();
  if (!sid) return '';
  const meta = sessionMetadataById.get(sid) || {};
  const win = sessionWindows.get(sid) || null;
  const source = externalSourceForSessionWindow(sid, win)
    || normalizeAgentId(meta.source || meta.backendSource || '');
  if (!source || source === 'intendant') return '';
  if (currentChangesTargetQuery()) return '';

  const trackedRoot = normalizeChangesRootPath(dashboardProjectRoot);
  const targetRoot = normalizeChangesRootPath(meta.projectRoot || meta.cwd);
  if (!trackedRoot || !targetRoot || trackedRoot === targetRoot) return '';

  const sourceLabel = meta.sourceLabel || meta.backendSource || source;
  return `Change tracking is attached to ${compactPathLabel(trackedRoot, true)}, but the selected ${sourceLabel} session is in ${compactPathLabel(targetRoot, true)}.`;
}

function currentChangesTargetQuery() {
  const sid = String(currentSessionFullId || foregroundSessionFullId || '').trim();
  if (!sid) return '';
  const meta = sessionMetadataById.get(sid) || {};
  const win = sessionWindows.get(sid) || null;
  const source = externalSourceForSessionWindow(sid, win)
    || normalizeAgentId(meta.source || meta.backendSource || meta.backend_source || '');
  if (!source || source === 'intendant') return '';

  const params = new URLSearchParams();
  params.set('session_id', sid);
  const backendId = String(meta.backendSessionId || meta.backend_session_id || '').trim();
  const intendantId = String(meta.intendantSessionId || meta.intendant_session_id || '').trim();
  if (backendId) params.set('backend_session_id', backendId);
  if (intendantId) params.set('intendant_session_id', intendantId);
  params.set('source', source);
  return params.toString();
}

function changesRequestUrl(path = '') {
  const suffix = path ? `/${encodeChangePath(path)}` : '';
  const query = currentChangesTargetQuery();
  return `/api/session/current/changes${suffix}${query ? `?${query}` : ''}`;
}

function changesRequestParams(path = '') {
  return {
    path: String(path || ''),
    query: currentChangesTargetQuery(),
  };
}

function fetchChangesResponse(path = '') {
  return dashboardJsonFetch('api_session_current_changes', changesRequestParams(path), () => (
    fetch(changesRequestUrl(path))
  ), 'api_session_current_changes');
}

function showChangesTargetMismatch(message) {
  activeChangesFile = null;
  changedFiles.clear();
  renderChangesFileList(message || 'Change tracking unavailable for selected target');
  const header = document.getElementById('changes-diff-header');
  if (header) header.textContent = 'Change tracking unavailable';
  const content = document.getElementById('changes-diff-content');
  if (content) {
    content.innerHTML = `<span class="changes-empty">${escapeHtml(message || 'Change tracking unavailable for selected target')}</span>`;
  }
  updateChangesBadge();
  stationScheduleUpdate();
}

async function refreshChangesList(options = {}) {
  const { selectFirst = false, refreshActive = false, quiet = false } = options;
  const seq = ++changesRefreshSeq;
  const targetMismatch = currentChangesTargetMismatchReason();
  if (targetMismatch) {
    showChangesTargetMismatch(targetMismatch);
    return;
  }
  try {
    const resp = await fetchChangesResponse();
    const data = await parseChangesResponse(resp);
    if (seq !== changesRefreshSeq) return;
    changedFiles.clear();
    if (Array.isArray(data)) {
      for (const item of data) {
        if (!item || !item.path) continue;
        changedFiles.set(item.path, {
          kind: item.kind || 'modified',
          lines_added: item.lines_added || 0,
          lines_removed: item.lines_removed || 0,
          diff_available: item.diff_available !== false,
          reason: item.reason || '',
        });
      }
    }

    if (activeChangesFile && !changedFiles.has(activeChangesFile)) {
      activeChangesFile = null;
    }
    renderChangesFileList();
    updateChangesBadge();
    stationScheduleUpdate();

    if (activeChangesFile && refreshActive) {
      await selectChangesFile(activeChangesFile, { skipListRender: true });
    } else if (!activeChangesFile && selectFirst && changedFiles.size > 0) {
      const first = [...changedFiles.keys()].sort((a, b) => a.localeCompare(b))[0];
      await selectChangesFile(first);
    } else if (!activeChangesFile) {
      renderChangesDiffHeader(null);
      const content = document.getElementById('changes-diff-content');
      if (content) {
        const msg = changedFiles.size ? 'Select a file to view changes' : 'No file changes yet';
        content.innerHTML = `<span class="changes-empty">${escapeHtml(msg)}</span>`;
      }
    }
    stationScheduleUpdate();
  } catch (e) {
    if (!quiet) {
      resetChangesPane(e.message === 'file watcher not active'
        ? 'Change tracking unavailable'
        : `Unable to load changes: ${e.message}`);
    }
  }
}

function scheduleChangesRefresh() {
  if (changesRefreshTimer) clearTimeout(changesRefreshTimer);
  changesRefreshTimer = setTimeout(() => {
    changesRefreshTimer = null;
    refreshChangesList({ refreshActive: !!activeChangesFile, quiet: true });
  }, 250);
}

async function selectChangesFile(path, options = {}) {
  if (!path) return;
  activeChangesFile = path;
  if (!options.skipListRender) renderChangesFileList();
  stationScheduleUpdate();
  const content = document.getElementById('changes-diff-content');
  if (!content) return;
  renderChangesDiffHeader(path, changedFiles.get(path), 'Loading');
  content.innerHTML = '<span class="changes-empty">Loading...</span>';
  try {
    const resp = await fetchChangesResponse(path);
    const data = await parseChangesResponse(resp);
    if (path !== activeChangesFile) return;
    if (changedFiles.has(path)) {
      const prev = changedFiles.get(path);
      changedFiles.set(path, {
        kind: data.kind || prev.kind,
        lines_added: data.lines_added || 0,
        lines_removed: data.lines_removed || 0,
        diff_available: data.diff_available !== false,
        reason: data.reason || '',
      });
      renderChangesFileList();
      stationScheduleUpdate();
    }
    renderChangesDiffHeader(path, changedFiles.get(path));
    if (data.diff_available === false) {
      const reason = data.reason || 'Textual diff is unavailable for this file.';
      content.innerHTML = `<span class="changes-empty">${escapeHtml(reason)}</span>`;
      return;
    }
    content.innerHTML = renderDiffLines(data.diff || '', path);
  } catch (e) {
    if (path !== activeChangesFile) return;
    renderChangesDiffHeader(path, changedFiles.get(path), 'Error');
    content.innerHTML = `<span class="changes-empty">Error: ${escapeHtml(e.message)}</span>`;
    stationScheduleUpdate();
  }
}
window.selectChangesFile = selectChangesFile;

// ── Timeline (rollback / redo / prune) ──
//
// Backend exposes the session's round-level history at
// `GET /api/session/current/history` and lets the user rewind /
// fast-forward / drop abandoned branches via POST endpoints. The UI is
// intentionally thin: we refetch-and-render on every change rather than
// trying to reconstruct the timeline from event deltas, so the server
// remains the single source of truth.
//
// Lifecycle:
// - WASM raises `UiCommand::HistoryChanged` in response to
//   snapshot_created / rolled_back / redone / history_pruned events.
// - processCommands -> refreshHistory() -> fetch + renderTimeline.
// - On initial Changes-tab open we also call refreshHistory() so a
//   session resumed mid-flight shows its existing timeline immediately.

let historyCache = null;

async function refreshHistory() {
  if (currentChangesTargetMismatchReason()) {
    historyCache = null;
    const wrap = document.getElementById('changes-timeline');
    if (wrap) wrap.style.display = 'none';
    return;
  }
  try {
    const r = await dashboardJsonFetch('api_session_current_history', {}, () => fetch('/api/session/current/history'), 'api_session_current_history');
    if (!r.ok) {
      // Older sessions or sessions without history support return 404.
      // Hide the timeline rather than showing a confusing error.
      historyCache = null;
      const wrap = document.getElementById('changes-timeline');
      if (wrap) wrap.style.display = 'none';
      return;
    }
    historyCache = await r.json();
    renderTimeline();
  } catch (e) {
    // Network error — stay silent; the WS event for the next mutation
    // will re-trigger us and the user can still navigate the UI.
    historyCache = null;
  }
}
window.refreshHistory = refreshHistory;

function renderTimeline() {
  const wrap = document.getElementById('changes-timeline');
  if (!wrap) return;
  if (!historyCache) {
    wrap.style.display = 'none';
    return;
  }
  const rounds = Array.isArray(historyCache.rounds) ? historyCache.rounds : [];
  if (rounds.length === 0) {
    wrap.style.display = 'none';
    return;
  }
  wrap.style.display = '';
  const current = historyCache.current_head_id;
  const currentIdx = rounds.findIndex(r => r.id === current);
  const container = document.getElementById('timeline-rounds');
  if (container) {
    container.innerHTML = rounds.map((r, i) => {
      const cls = i === currentIdx ? 'current'
                  : (currentIdx >= 0 && i > currentIdx) ? 'future'
                  : '';
      const filesN = Array.isArray(r.files_changed) ? r.files_changed.length : 0;
      const summary = (r.summary || '').toString();
      const tipSummary = summary.length > 80 ? summary.slice(0, 80) + '...' : summary;
      const sumPart = tipSummary ? ` \u2014 ${tipSummary}` : '';
      const tip = `Round ${r.id}${sumPart} \u2014 ${filesN} file${filesN === 1 ? '' : 's'} changed`;
      return `<div class="timeline-round ${cls}" title="${escapeHtml(tip)}" onclick="doRollback(${r.id})">`
        + `<span class="round-id">#${r.id}</span>`
        + `<span class="round-files">${filesN} file${filesN === 1 ? '' : 's'}</span>`
        + `</div>`;
    }).join('');
  }

  // Redo is only meaningful after a rollback — current_head must exist
  // and not be the last recorded round.
  const redoBtn = document.getElementById('redo-btn');
  if (redoBtn) {
    redoBtn.disabled = currentIdx < 0 || currentIdx >= rounds.length - 1;
  }

  // Abandoned branches (rolled-back rounds that later drifted from
  // current HEAD). Collapsed by default so the main strip stays compact.
  const abandoned = Array.isArray(historyCache.abandoned_branches)
    ? historyCache.abandoned_branches
    : [];
  const det = document.getElementById('timeline-abandoned');
  if (det) {
    if (abandoned.length === 0) {
      det.style.display = 'none';
    } else {
      det.style.display = '';
      const countEl = document.getElementById('abandoned-count');
      if (countEl) countEl.textContent = String(abandoned.length);
      const listEl = document.getElementById('abandoned-list');
      if (listEl) {
        listEl.innerHTML = abandoned.map((b, i) => {
          const n = Array.isArray(b.rounds) ? b.rounds.length : 0;
          const from = b.branched_from_id != null ? `#${b.branched_from_id}` : '?';
          return `<div class="abandoned-branch">`
            + `<span>Branch ${i + 1}: ${n} round${n === 1 ? '' : 's'} from ${escapeHtml(from)}</span>`
            + `</div>`;
        }).join('');
      }
    }
  }
}

// Clicking a round in the timeline strip routes here, which opens the
// rollback modal rather than immediately POSTing. The modal lets the user
// pick whether to revert files, the conversation, or both — the actual
// request is built and sent in confirmRollback() below.
let pendingRollbackRoundId = null;

function doRollback(roundId) {
  if (!Number.isFinite(roundId)) return;
  openRollbackModal(roundId, `#${roundId}`);
}
window.doRollback = doRollback;

function openRollbackModal(roundId, roundLabel) {
  pendingRollbackRoundId = roundId;
  const labelEl = document.getElementById('rollback-round-label');
  if (labelEl) {
    // Accept either a pre-formatted label or fall back to a short id.
    const fallback = typeof roundId === 'string' ? roundId.substring(0, 8) : String(roundId);
    labelEl.textContent = roundLabel || fallback;
  }
  const filesBox = document.getElementById('rollback-files');
  const convBox = document.getElementById('rollback-conversation');
  if (filesBox) filesBox.checked = true;
  if (convBox) convBox.checked = false;
  updateRollbackWarning();
  const modal = document.getElementById('rollback-modal');
  if (modal) modal.style.display = 'flex';
}
window.openRollbackModal = openRollbackModal;

function closeRollbackModal() {
  pendingRollbackRoundId = null;
  const modal = document.getElementById('rollback-modal');
  if (modal) modal.style.display = 'none';
}
window.closeRollbackModal = closeRollbackModal;

function updateRollbackWarning() {
  const filesEl = document.getElementById('rollback-files');
  const convEl = document.getElementById('rollback-conversation');
  const warn = document.getElementById('rollback-warning');
  const btn = document.getElementById('rollback-confirm-btn');
  if (!filesEl || !convEl || !warn || !btn) return;
  const files = filesEl.checked;
  const conv = convEl.checked;
  if (!files && !conv) {
    warn.textContent = 'Select at least one option to roll back.';
    warn.style.display = '';
    btn.disabled = true;
  } else if (conv && !files) {
    warn.innerHTML = '\u26A0 Reverting conversation without files: the agent will forget what it did, but the file changes remain. Agent may hallucinate about code state.';
    warn.style.display = '';
    btn.disabled = false;
  } else if (files && !conv) {
    warn.innerHTML = '\u26A0 Reverting files without conversation: the agent thinks changes are still applied but they\'re gone. May cause confusion on next turn.';
    warn.style.display = '';
    btn.disabled = false;
  } else {
    warn.style.display = 'none';
    btn.disabled = false;
  }
}
window.updateRollbackWarning = updateRollbackWarning;

// Wire the live warning/disable logic to the checkboxes. The elements live
// in the static modal markup above, so we can bind once at script load.
{
  const filesEl = document.getElementById('rollback-files');
  const convEl = document.getElementById('rollback-conversation');
  if (filesEl) filesEl.addEventListener('change', updateRollbackWarning);
  if (convEl) convEl.addEventListener('change', updateRollbackWarning);
}

let dashboardConfirmPending = null;

function closeDashboardConfirmModal(result = false) {
  const pending = dashboardConfirmPending;
  dashboardConfirmPending = null;
  const modal = document.getElementById('dashboard-confirm-modal');
  if (modal) modal.style.display = 'none';
  if (pending && typeof pending.resolve === 'function') pending.resolve(result);
}
window.closeDashboardConfirmModal = closeDashboardConfirmModal;

function showDashboardConfirm(opts = {}) {
  return new Promise(resolve => {
    if (dashboardConfirmPending) closeDashboardConfirmModal(false);
    dashboardConfirmPending = {
      resolve,
      confirmValue: opts.confirmValue === undefined ? true : opts.confirmValue,
      alternateValue: opts.alternateValue === undefined ? 'alternate' : opts.alternateValue,
    };

    const title = document.getElementById('dashboard-confirm-title');
    const message = document.getElementById('dashboard-confirm-message');
    const warning = document.getElementById('dashboard-confirm-warning');
    const accept = document.getElementById('dashboard-confirm-accept');
    const alternate = document.getElementById('dashboard-confirm-alternate');
    const cancel = document.getElementById('dashboard-confirm-cancel');
    const modal = document.getElementById('dashboard-confirm-modal');

    if (title) title.textContent = opts.title || 'Confirm action';
    if (message) message.textContent = opts.message || '';
    if (warning) {
      const warningText = opts.warning || '';
      warning.textContent = warningText;
      warning.style.display = warningText ? '' : 'none';
    }
    if (accept) {
      accept.textContent = opts.confirmLabel || 'Continue';
      accept.className = 'modal-confirm' + (opts.danger === false ? '' : ' danger');
      accept.title = opts.confirmTitleAttr || '';
    }
    if (alternate) {
      const alternateLabel = opts.alternateLabel || '';
      alternate.textContent = alternateLabel || 'Alternate';
      alternate.className = 'modal-confirm'
        + (opts.alternateDanger === false ? '' : ' danger')
        + (alternateLabel ? '' : ' hidden');
      alternate.title = opts.alternateTitle || '';
    }
    if (cancel) cancel.textContent = opts.cancelLabel || 'Cancel';
    if (modal) {
      modal.style.display = 'flex';
      setTimeout(() => accept?.focus(), 0);
    } else {
      closeDashboardConfirmModal(false);
    }
  });
}
window.showDashboardConfirm = showDashboardConfirm;

let dashboardPromptPending = null;

function closeDashboardPromptModal(result = null) {
  const pending = dashboardPromptPending;
  dashboardPromptPending = null;
  const modal = document.getElementById('dashboard-prompt-modal');
  if (modal) modal.style.display = 'none';
  if (pending && typeof pending.resolve === 'function') pending.resolve(result);
}
window.closeDashboardPromptModal = closeDashboardPromptModal;

function showDashboardPrompt(opts = {}) {
  return new Promise(resolve => {
    if (dashboardPromptPending) closeDashboardPromptModal(null);
    dashboardPromptPending = { resolve };

    const title = document.getElementById('dashboard-prompt-title');
    const label = document.getElementById('dashboard-prompt-label');
    const input = document.getElementById('dashboard-prompt-input');
    const submit = document.getElementById('dashboard-prompt-submit');
    const cancel = document.getElementById('dashboard-prompt-cancel');
    const modal = document.getElementById('dashboard-prompt-modal');

    if (title) title.textContent = opts.title || 'Enter value';
    if (label) label.textContent = opts.label || 'Value';
    if (input) {
      input.value = opts.initialValue || '';
      input.placeholder = opts.placeholder || '';
      input.rows = opts.multiline === false ? 1 : Number(opts.rows || 3);
      if (Number(opts.maxLength || 0) > 0) input.maxLength = Number(opts.maxLength);
      else input.removeAttribute('maxlength');
      input.dataset.multiline = opts.multiline === false ? 'false' : 'true';
    }
    if (submit) submit.textContent = opts.submitLabel || 'Continue';
    if (cancel) cancel.textContent = opts.cancelLabel || 'Cancel';
    if (modal) {
      modal.style.display = 'flex';
      setTimeout(() => {
        input?.focus();
        input?.select();
      }, 0);
    } else {
      closeDashboardPromptModal(null);
    }
  });
}
window.showDashboardPrompt = showDashboardPrompt;

async function confirmRollback() {
  if (pendingRollbackRoundId === null || pendingRollbackRoundId === undefined) return;
  const roundId = pendingRollbackRoundId;
  const revertFiles = document.getElementById('rollback-files').checked;
  const revertConv = document.getElementById('rollback-conversation').checked;
  closeRollbackModal();
  try {
    const payload = {
      round_id: roundId,
      revert_files: revertFiles,
      revert_conversation: revertConv,
    };
    const resp = await dashboardJsonFetch('api_session_current_rollback', payload, () => fetch('/api/session/current/rollback', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    }), 'api_session_current_rollback', { fallbackAfterRpcFailure: false });
    if (!resp.ok) {
      // Prefer the structured {error} shape the server already uses for
      // this endpoint; fall back to raw text for unexpected error bodies.
      let errMsg = resp.statusText;
      try {
        const j = await resp.clone().json();
        if (j && j.error) errMsg = j.error;
      } catch (_) {
        const t = await resp.text().catch(() => '');
        if (t) errMsg = t;
      }
      showControlToast('error', `Rollback failed: ${errMsg}`);
    }
    // Success: backend emits `rolled_back` / `conversation_rolled_back`
    // events, the WASM layer raises HistoryChanged, and the UI refreshes.
  } catch (e) {
    showControlToast('error', `Rollback failed: ${e.message || e}`);
  }
}
window.confirmRollback = confirmRollback;

async function doRedo() {
  try {
    const r = await dashboardJsonFetch('api_session_current_redo', {}, () => fetch('/api/session/current/redo', { method: 'POST' }), 'api_session_current_redo', { fallbackAfterRpcFailure: false });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      showControlToast('error', `Redo failed: ${err.error || r.statusText}`);
    }
  } catch (e) {
    showControlToast('error', `Redo error: ${e.message || e}`);
  }
}
window.doRedo = doRedo;

async function doPrune() {
  const ok = await showDashboardConfirm({
    title: 'Prune abandoned branches',
    message: 'Prune all abandoned branches?',
    warning: 'This frees disk space but cannot be undone.',
    confirmLabel: 'Prune',
  });
  if (!ok) return;
  try {
    const r = await dashboardJsonFetch('api_session_current_prune', {}, () => fetch('/api/session/current/prune', { method: 'POST' }), 'api_session_current_prune', { fallbackAfterRpcFailure: false });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      showControlToast('error', `Prune failed: ${err.error || r.statusText}`);
      return;
    }
    // History event will redraw; the console line is a belt-and-suspenders
    // confirmation so curious users can verify the server response shape.
    const data = await r.json().catch(() => ({}));
    if (typeof data.bytes_freed === 'number') {
      const mb = (data.bytes_freed / 1024 / 1024).toFixed(1);
      console.log(`Pruned ${data.branches_removed || 0} branches, ${mb} MB freed`);
    }
  } catch (e) {
    showControlToast('error', `Prune error: ${e.message || e}`);
  }
}
window.doPrune = doPrune;

// ── Control sub-tab ──
//
// Reads the live Codex runtime config from `/api/settings` and dispatches
// `SetCodex*` control messages on change. The backend's control plane
// persists to `intendant.toml` and broadcasts `CodexConfigChanged` back to
// all connected dashboards, so two browsers stay in sync.

let controlClaudeConfig = {
  model: '',
  permission_mode: 'default',
  allowed_tools: [],
};
let controlCodexConfig = {
  command: 'codex',
  managed_command: '',
  sandbox: null,
  approval_policy: null,
  model: null,
  reasoning_effort: '',
  service_tier: '',
  web_search: false,
  network_access: false,
  writable_roots: [],
  managed_context: 'vanilla',
  context_archive: 'summary',
};
// Per-category approval rules (internal-agent autonomy gates). Mirrored from
// /api/settings; live edits dispatch the `set_approval_rule` ControlMsg.
const CONTROL_APPROVAL_CATEGORIES = [
  'file_read', 'file_write', 'file_delete', 'command_exec',
  'network', 'destructive', 'display_control', 'tool_call',
];
let controlApprovalRules = {};
let controlCurrentBackend = null;
let controlBackendActive = false;
const controlAppliesBaseText = 'changes apply on next task';
let newSessionConfiguredAgent = '';
let newSessionAgentCommands = {
  codex: 'codex',
  'claude-code': 'claude',
};
let newSessionCodexManagedContext = 'vanilla';
let newSessionCodexContextArchive = 'summary';
let newSessionCodexSandbox = '';
let newSessionCodexApprovalPolicy = '';
let newSessionCodexDefaultServiceTier = '';
let newSessionCodexFastMode = false;
let newSessionCodexFastModeTouched = false;
let newSessionCodexLaunchDefaultsLoaded = false;
let newSessionSpawnPending = false;
let newSessionSpawnTask = '';
let newSessionSpawnName = '';
let newSessionSpawnTimeout = null;
let newSessionSpawnClearTimeout = null;
let newSessionSpawnRecent = null;
let newSessionSpawnRecentTimeout = null;
let sessionRenameEditing = null;
let sessionConfigEditing = null;
let sessionConfigSavePending = null;
let sessionDeletePending = null;
const NEW_SESSION_SPAWN_TIMEOUT_MS = 90000;
const NEW_SESSION_LAUNCH_FAILURE_GRACE_MS = 15000;

async function refreshControlPane() {
  // Deep links (#activity/control) route here during script evaluation,
  // before the dashboard transport is constructed — poll briefly instead
  // of throwing on a null transport and leaving the pane half-rendered.
  if (!dashboardTransport) {
    if (!refreshControlPane._waitingForTransport) {
      refreshControlPane._waitingForTransport = true;
      const retry = () => {
        if (dashboardTransport) {
          refreshControlPane._waitingForTransport = false;
          refreshControlPane();
        } else {
          setTimeout(retry, 250);
        }
      };
      setTimeout(retry, 250);
    }
    return;
  }
  try {
    const d = await fetchDashboardSettings();
    controlCurrentBackend = normalizeAgentId(d.external_agent);
    controlCodexConfig = {
      command: d.codex_command || 'codex',
      managed_command: d.codex_managed_command || '',
      sandbox: normalizeCodexSandbox(d.codex_sandbox || 'workspace-write'),
      approval_policy: normalizeCodexApprovalPolicy(d.codex_approval_policy || 'on-request'),
      model: d.codex_model || '',
      reasoning_effort: d.codex_reasoning_effort || '',
      service_tier: normalizeCodexServiceTier(d.codex_service_tier || ''),
      web_search: !!d.codex_web_search,
      network_access: !!d.codex_network_access,
      writable_roots: Array.isArray(d.codex_writable_roots) ? d.codex_writable_roots : [],
      managed_context: d.codex_managed_context || 'vanilla',
      context_archive: d.codex_context_archive || 'summary',
    };
    newSessionCodexSandbox = controlCodexConfig.sandbox || 'workspace-write';
    newSessionCodexApprovalPolicy = controlCodexConfig.approval_policy || 'on-request';
    newSessionCodexDefaultServiceTier = controlCodexConfig.service_tier || '';
    newSessionCodexLaunchDefaultsLoaded = true;
    if (!newSessionCodexFastModeTouched) {
      newSessionCodexFastMode = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
    }
    controlClaudeConfig = {
      model: d.claude_model || '',
      permission_mode: d.claude_permission_mode || 'default',
      allowed_tools: Array.isArray(d.claude_allowed_tools) ? d.claude_allowed_tools : [],
    };
    controlApprovalRules = {};
    for (const cat of CONTROL_APPROVAL_CATEGORIES) {
      const v = d['approval_' + cat];
      controlApprovalRules[cat] = (v === 'auto' || v === 'ask' || v === 'deny') ? v : 'ask';
    }
    renderControlPane();
  } catch (e) {
    console.warn('Failed to load control-pane config:', e);
  }
}

function renderControlPane() {
  const badge = document.getElementById('control-backend-badge');
  const codexSection = document.getElementById('control-codex-section');
  const claudeSection = document.getElementById('control-claude-section');
  const emptyMsg = document.getElementById('control-no-backend');
  const isCodex = controlCurrentBackend === 'codex';
  const isClaude = controlCurrentBackend === 'claude-code';
  if (badge) badge.textContent = controlCurrentBackend || 'none';
  if (codexSection) codexSection.style.display = isCodex ? '' : 'none';
  if (claudeSection) claudeSection.style.display = isClaude ? '' : 'none';
  if (emptyMsg) emptyMsg.style.display = (isCodex || isClaude) ? 'none' : '';
  const $ = id => document.getElementById(id);
  if (isClaude) {
    const modelInp = $('control-claude-model');
    const modeSel = $('control-claude-permission-mode');
    const toolsTA = $('control-claude-allowed-tools');
    if (modelInp) modelInp.value = controlClaudeConfig.model || '';
    if (modeSel) modeSel.value = controlClaudeConfig.permission_mode || 'default';
    if (toolsTA) toolsTA.value = (controlClaudeConfig.allowed_tools || []).join('\n');
  }
  if (isCodex) {
    const sandboxSel = $('control-codex-sandbox');
    const approvalSel = $('control-codex-approval');
    const modelInp = $('control-codex-model');
    const reasoningSel = $('control-codex-reasoning');
    const serviceTierSel = $('control-codex-service-tier');
    const webSearch = $('control-codex-web-search');
    const webSearchStatus = $('control-codex-web-search-status');
    const managedContextSel = $('control-codex-managed-context');
    const contextArchiveSel = $('control-codex-context-archive');
    const networkCb = $('control-codex-network');
    const networkStatus = $('control-codex-network-status');
    const networkRow = $('control-codex-network-row');
    const networkHint = $('control-codex-network-hint');
    const rootsTA = $('control-codex-writable-roots');
    if (sandboxSel) sandboxSel.value = controlCodexConfig.sandbox;
    if (approvalSel) approvalSel.value = controlCodexConfig.approval_policy;
    if (modelInp) modelInp.value = controlCodexConfig.model || '';
    if (reasoningSel) reasoningSel.value = controlCodexConfig.reasoning_effort || '';
    if (serviceTierSel) serviceTierSel.value = normalizeCodexServiceTier(controlCodexConfig.service_tier || '');
    if (webSearch) webSearch.checked = !!controlCodexConfig.web_search;
    if (webSearchStatus) webSearchStatus.textContent = controlCodexConfig.web_search ? 'on' : 'off';
    if (managedContextSel) managedContextSel.value = controlCodexConfig.managed_context || 'vanilla';
    if (contextArchiveSel) contextArchiveSel.value = controlCodexConfig.context_archive || 'summary';
    if (networkCb) networkCb.checked = !!controlCodexConfig.network_access;
    if (networkStatus) networkStatus.textContent = controlCodexConfig.network_access ? 'on' : 'off';
    // Network access only matters inside workspace-write. Grey out + annotate
    // otherwise so the toggle doesn't look like it's silently doing nothing.
    const netApplies = controlCodexConfig.sandbox === 'workspace-write';
    if (networkCb) networkCb.disabled = !netApplies;
    if (networkRow) networkRow.style.opacity = netApplies ? '' : '0.55';
    if (networkHint) {
      networkHint.style.color = netApplies ? '' : 'var(--overlay0)';
    }
    if (rootsTA) rootsTA.value = (controlCodexConfig.writable_roots || []).join('\n');
  }
  // Approval rules apply to the internal agent regardless of external backend,
  // so they're rendered unconditionally.
  for (const cat of CONTROL_APPROVAL_CATEGORIES) {
    const sel = $('control-approval-' + cat);
    if (sel && controlApprovalRules[cat]) sel.value = controlApprovalRules[cat];
  }
  updateControlAppliesNote();
}

function updateControlAppliesNote() {
  const note = document.getElementById('control-applies-note');
  if (!note) return;
  if (controlBackendActive) {
    note.textContent = 'task running — changes apply on next task';
    note.classList.add('pending');
  } else {
    note.textContent = controlAppliesBaseText;
    note.classList.remove('pending');
  }
}

function dispatchControlMsg(payload) {
  const action = String(payload?.action || '').trim();
  if (DASHBOARD_SESSION_CONTROL_MSG_RPC_ACTIONS.has(action)) {
    dispatchSessionControlMsg(payload);
    return;
  }
  if (DASHBOARD_ACTION_MSG_RPC_ACTIONS.has(action)) {
    dispatchDashboardActionMsg(payload);
    return;
  }
  if (
    action &&
    DASHBOARD_CONTROL_MSG_RPC_ACTIONS.has(action) &&
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.api_control_msg_available === true
  ) {
    dashboardTransport.request('api_control_msg', { message: payload }, { timeoutMs: 10000 })
      .catch(err => {
        console.warn(`[dashboard-control] ${action} ControlMsg RPC failed; not replaying over /ws`, err);
        if (typeof showControlToast === 'function') {
          showControlToast('error', err?.message || 'Dashboard control request failed');
        }
      });
    return;
  }
  if (dashboardConnectModeEnabled()) {
    dashboardConnectMutationUnavailable(action, 'Control request');
    return;
  }
  if (app && app.send_server_action) {
    app.send_server_action(payload);
  } else {
    console.warn('control pane: no app connection, dropped', payload);
  }
}

function onControlSandboxChange(ev) {
  const mode = ev.target.value;
  if (mode === controlCodexConfig.sandbox) return;
  controlCodexConfig.sandbox = mode;
  dispatchControlMsg({ action: 'set_codex_sandbox', mode });
}

function onControlApprovalChange(ev) {
  const policy = ev.target.value;
  if (policy === controlCodexConfig.approval_policy) return;
  controlCodexConfig.approval_policy = policy;
  dispatchControlMsg({ action: 'set_codex_approval_policy', policy });
}

function onControlApprovalRuleChange(ev) {
  const category = ev.target.dataset.approvalCategory;
  const rule = ev.target.value;
  if (!category) return;
  if (rule === controlApprovalRules[category]) return;
  controlApprovalRules[category] = rule;
  dispatchControlMsg({ action: 'set_approval_rule', category, rule });
}

function onControlModelCommit(ev) {
  const model = ev.target.value.trim();
  const current = (controlCodexConfig.model || '').trim();
  if (model === current) return;
  controlCodexConfig.model = model;
  dispatchControlMsg({ action: 'set_codex_model', model: model || null });
}

function onControlClaudeModelCommit(ev) {
  const model = ev.target.value.trim();
  const current = (controlClaudeConfig.model || '').trim();
  if (model === current) return;
  controlClaudeConfig.model = model;
  dispatchControlMsg({ action: 'set_claude_model', model: model || null });
}

function onControlClaudePermissionModeChange(ev) {
  const mode = ev.target.value;
  if (mode === (controlClaudeConfig.permission_mode || 'default')) return;
  controlClaudeConfig.permission_mode = mode;
  dispatchControlMsg({ action: 'set_claude_permission_mode', mode });
}

function onControlClaudeAllowedToolsCommit(ev) {
  const tools = ev.target.value
    .split('\n')
    .map(t => t.trim())
    .filter(Boolean);
  const current = controlClaudeConfig.allowed_tools || [];
  if (tools.length === current.length && tools.every((t, i) => t === current[i])) return;
  controlClaudeConfig.allowed_tools = tools;
  dispatchControlMsg({ action: 'set_claude_allowed_tools', tools });
}

function onControlReasoningChange(ev) {
  const effort = ev.target.value;
  if (effort === (controlCodexConfig.reasoning_effort || '')) return;
  controlCodexConfig.reasoning_effort = effort;
  dispatchControlMsg({ action: 'set_codex_reasoning_effort', effort: effort || null });
}

function onControlServiceTierChange(ev) {
  const serviceTier = normalizeCodexServiceTier(ev.target.value || '');
  if (serviceTier === normalizeCodexServiceTier(controlCodexConfig.service_tier || '')) return;
  controlCodexConfig.service_tier = serviceTier;
  newSessionCodexDefaultServiceTier = serviceTier;
  if (!newSessionCodexFastModeTouched) {
    newSessionCodexFastMode = codexServiceTierIsFast(serviceTier);
  }
  const settingsSel = document.getElementById('set-codex-service-tier');
  if (settingsSel) settingsSel.value = serviceTier;
  renderNewSessionAgentControls();
  dispatchControlMsg({ action: 'set_codex_service_tier', service_tier: serviceTier || null });
}

function onControlWebSearchChange(ev) {
  const enabled = ev.target.checked;
  controlCodexConfig.web_search = enabled;
  const s = document.getElementById('control-codex-web-search-status');
  if (s) s.textContent = enabled ? 'on' : 'off';
  dispatchControlMsg({ action: 'set_codex_web_search', enabled });
}

function onControlManagedContextChange(ev) {
  const mode = ev.target.value === 'managed' ? 'managed' : 'vanilla';
  if (mode === (controlCodexConfig.managed_context || 'vanilla')) return;
  controlCodexConfig.managed_context = mode;
  dispatchControlMsg({ action: 'set_codex_managed_context', mode });
}

function onControlContextArchiveChange(ev) {
  const mode = ['summary', 'exact', 'off'].includes(ev.target.value) ? ev.target.value : 'summary';
  if (mode === (controlCodexConfig.context_archive || 'summary')) return;
  controlCodexConfig.context_archive = mode;
  dispatchControlMsg({ action: 'set_codex_context_archive', mode });
}

function onControlNetworkChange(ev) {
  const enabled = ev.target.checked;
  controlCodexConfig.network_access = enabled;
  const s = document.getElementById('control-codex-network-status');
  if (s) s.textContent = enabled ? 'on' : 'off';
  dispatchControlMsg({ action: 'set_codex_network_access', enabled });
}

function onControlWritableRootsCommit(ev) {
  const raw = ev.target.value || '';
  const roots = raw
    .split('\n')
    .map(l => l.trim())
    .filter(l => l.length > 0);
  // Cheap dedupe-preserving-order so local state matches what the server will
  // normalize to; avoids a no-op reassign on the CodexConfigChanged reply.
  const deduped = [];
  for (const r of roots) if (!deduped.includes(r)) deduped.push(r);
  const current = controlCodexConfig.writable_roots || [];
  if (
    deduped.length === current.length &&
    deduped.every((v, i) => v === current[i])
  ) {
    return;
  }
  controlCodexConfig.writable_roots = deduped;
  dispatchControlMsg({ action: 'set_codex_writable_roots', roots: deduped });
}

// Apply a ClaudeConfigChanged event from the server — the Claude Code
// sibling of applyCodexConfigChanged below.
function applyClaudeConfigChanged(evt) {
  if (evt.model !== undefined && evt.model !== null) {
    controlClaudeConfig.model = evt.model;
  } else if (evt.model_cleared) {
    controlClaudeConfig.model = '';
  }
  if (evt.permission_mode) {
    controlClaudeConfig.permission_mode = evt.permission_mode;
  }
  if (Array.isArray(evt.allowed_tools)) {
    controlClaudeConfig.allowed_tools = evt.allowed_tools;
  }
  if (controlCurrentBackend === 'claude-code') renderControlPane();
}

// Apply a CodexConfigChanged event from the server, keeping the pane in
// sync with toggles from other browsers (and our own server-confirmed write).
function applyCodexConfigChanged(evt) {
  if (evt.command) {
    controlCodexConfig.command = evt.command;
    newSessionAgentCommands.codex = evt.command;
    const input = document.getElementById('set-codex-command');
    if (input) input.value = evt.command;
    renderNewSessionAgentControls();
  }
  if (evt.managed_command !== undefined && evt.managed_command !== null) {
    controlCodexConfig.managed_command = evt.managed_command;
  } else if (evt.managed_command_cleared) {
    controlCodexConfig.managed_command = '';
  }
  if (evt.managed_command !== undefined || evt.managed_command_cleared) {
    const input = document.getElementById('set-codex-managed-command');
    if (input) input.value = controlCodexConfig.managed_command || '';
  }
  // Keep the per-launch defaults in lockstep: stationStartSession / the
  // New Session pane send these as explicit per-session overrides, so a
  // stale value here silently defeats a just-changed global policy (the
  // managed_context/context_archive arms below already did this).
  if (evt.sandbox) {
    controlCodexConfig.sandbox = evt.sandbox;
    newSessionCodexSandbox = normalizeCodexSandbox(evt.sandbox) || newSessionCodexSandbox;
  }
  if (evt.approval_policy) {
    controlCodexConfig.approval_policy = evt.approval_policy;
    newSessionCodexApprovalPolicy =
      normalizeCodexApprovalPolicy(evt.approval_policy) || newSessionCodexApprovalPolicy;
  }
  if (evt.model !== undefined && evt.model !== null) {
    controlCodexConfig.model = evt.model;
  } else if (evt.model_cleared) {
    controlCodexConfig.model = '';
  }
  if (evt.reasoning_effort !== undefined && evt.reasoning_effort !== null) {
    controlCodexConfig.reasoning_effort = evt.reasoning_effort;
  } else if (evt.reasoning_effort_cleared) {
    controlCodexConfig.reasoning_effort = '';
  }
  if (evt.service_tier !== undefined && evt.service_tier !== null) {
    controlCodexConfig.service_tier = normalizeCodexServiceTier(evt.service_tier);
  } else if (evt.service_tier_cleared) {
    controlCodexConfig.service_tier = '';
  }
  if (evt.service_tier !== undefined || evt.service_tier_cleared) {
    const tier = normalizeCodexServiceTier(controlCodexConfig.service_tier || '');
    newSessionCodexDefaultServiceTier = tier;
    if (!newSessionCodexFastModeTouched) {
      newSessionCodexFastMode = codexServiceTierIsFast(tier);
    }
    const settingsSel = document.getElementById('set-codex-service-tier');
    if (settingsSel) settingsSel.value = tier;
    renderNewSessionAgentControls();
  }
  if (typeof evt.web_search === 'boolean') controlCodexConfig.web_search = evt.web_search;
  if (typeof evt.network_access === 'boolean') controlCodexConfig.network_access = evt.network_access;
  if (Array.isArray(evt.writable_roots)) controlCodexConfig.writable_roots = evt.writable_roots;
  if (evt.managed_context) {
    controlCodexConfig.managed_context = evt.managed_context;
    newSessionCodexManagedContext = evt.managed_context === 'managed' ? 'managed' : 'vanilla';
    renderNewSessionAgentControls();
  }
  if (evt.context_archive) {
    controlCodexConfig.context_archive = evt.context_archive;
    newSessionCodexContextArchive = normalizeContextArchiveMode(evt.context_archive);
    renderNewSessionAgentControls();
  }
  // Sandbox change may flip the network-access row's enabled state — full
  // re-render is the simplest way to keep disabled-styling in sync.
  renderControlPane();
}

// Track in-flight action dispatches so the UI can grey buttons out until a
// matching CodexThreadActionResult arrives (or a short timeout elapses).
const controlPendingActions = new Map(); // op -> { timeoutHandle, btnEl }
const DETACHED_CODEX_ACTION_ATTACH_NOTICE_MS = 15000;
const DETACHED_CODEX_ACTION_ATTACH_TIMEOUT_MS = 120000;
const pendingDetachedCodexThreadActions = new Map(); // session id -> [{ op, params, noticeHandle, timeoutHandle }]

function sessionIdsForPendingDetachedActions(sessionId) {
  return relatedSessionIdsForSession(sessionId);
}

function relatedSessionIdsForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  const ids = new Set();
  if (sid) ids.add(sid);
  for (const [id, meta] of sessionMetadataById) {
    if (!id || !meta) continue;
    if (
      meta.backendSessionId === sid ||
      meta.intendantSessionId === sid
    ) {
      ids.add(id);
    }
  }
  return ids;
}

function queueDetachedCodexThreadAction(sessionId, op, params = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  const resume = detachedSessionResumeMessage(sid, '', true, []);
  if (!resume || normalizeAgentId(resume.source) !== 'codex') {
    showControlToast('error', `/${op} failed: Codex session is not attached`);
    return false;
  }
  const item = { op, params: params || {}, noticeHandle: 0, timeoutHandle: 0 };
  item.noticeHandle = setTimeout(() => {
    const queue = pendingDetachedCodexThreadActions.get(sid) || [];
    if (queue.includes(item)) {
      showControlToast('info', `Still attaching Codex session before /${op}`);
    }
  }, DETACHED_CODEX_ACTION_ATTACH_NOTICE_MS);
  item.timeoutHandle = setTimeout(() => {
    const queue = pendingDetachedCodexThreadActions.get(sid) || [];
    const next = queue.filter(entry => entry !== item);
    if (next.length > 0) pendingDetachedCodexThreadActions.set(sid, next);
    else pendingDetachedCodexThreadActions.delete(sid);
    clearTimeout(item.noticeHandle);
    showControlToast('error', `/${op} failed: Codex session did not attach`);
  }, DETACHED_CODEX_ACTION_ATTACH_TIMEOUT_MS);
  const queue = pendingDetachedCodexThreadActions.get(sid) || [];
  const shouldRequestAttach = queue.length === 0;
  queue.push(item);
  pendingDetachedCodexThreadActions.set(sid, queue);
  if (shouldRequestAttach) dispatchControlMsg(resume);
  stationUpsertCodexThreadActionActivity(op, sid, 'attaching');
  showControlToast('info', `Attaching Codex session before /${op}`);
  return true;
}

function canAttachSessionWindow(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  if (sessionWindowIsSide(sid) || sessionWindowIsSubagent(sid)) return false;
  const source = externalSourceForSessionWindow(sid, win);
  return !!source && source !== 'intendant' && sessionWindowIsDetached(sid);
}

function canQueueDetachedCodexThreadAction(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || !sessionWindowIsCodex(sid) || !canAttachSessionWindow(sid)) return false;
  const resume = detachedSessionResumeMessage(sid, '', true, []);
  return !!resume && normalizeAgentId(resume.source) === 'codex';
}

function flushPendingDetachedCodexThreadActions(sessionId) {
  const ids = sessionIdsForPendingDetachedActions(sessionId);
  for (const sid of ids) {
    const queue = pendingDetachedCodexThreadActions.get(sid);
    if (!queue || queue.length === 0) continue;
    pendingDetachedCodexThreadActions.delete(sid);
    for (const item of queue) {
      clearTimeout(item.noticeHandle);
      clearTimeout(item.timeoutHandle);
      dispatchCodexThreadAction(item.op, item.params, sid, { skipDetachedAttach: true });
    }
  }
}

function dispatchCodexThreadAction(op, params, sessionId = '', options = {}) {
  const sid = String(sessionId || '').trim() || resolvePromptTargetSessionId();
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  if (!sid) {
    showControlToast('error', `/${op} needs a target session`);
    return false;
  }
  if (sid && sessionWindowIsSide(sid) && !codexThreadActionAllowedForSide(op)) {
    showControlToast('error', `/${op} is not available in a /side window; use the parent thread`);
    return false;
  }
  const actionState = options.internalSideClose && normalizedOp === 'side-close'
    ? { allowed: true, reason: '' }
    : (sid ? codexThreadActionStateForSession(sid, op) : { allowed: true, reason: '' });
  if (sid && !actionState.allowed) {
    showControlToast('error', `/${op} is not available here: ${actionState.reason}`);
    return false;
  }
  if (
    sid &&
    sessionWindowIsSide(sid) &&
    normalizedOp === 'undo' &&
    isSessionWindowEffectivelyActive(sid, currentPhase)
  ) {
    showControlToast('error', '/undo is available after the /side turn finishes or is stopped');
    return false;
  }
  if (sid && !options.skipDetachedAttach && sessionWindowIsDetached(sid)) {
    return queueDetachedCodexThreadAction(sid, op, params || {});
  }
  // Canonical backend-neutral action name; the server also accepts the
  // legacy `codex_thread_action` alias from older frontends.
  const payload = { action: 'thread_action', op, params: params || {} };
  if (sid) payload.session_id = sid;
  dispatchControlMsg(payload);
  stationUpsertCodexThreadActionActivity(op, sid, 'requested', '', { renderLog: false });
  return true;
}

function showControlToast(kind, text) {
  const controlBody = document.querySelector('#activity-control-pane .control-pane-body');
  const host = controlBody && controlBody.offsetParent !== null ? controlBody : document.body;
  if (!host) return;
  const existing = host.querySelector('.control-toast');
  if (existing) existing.remove();
  const toast = document.createElement('div');
  const toastKind = kind === 'error' ? 'error' : (kind === 'success' ? 'success' : 'info');
  toast.className = 'control-toast ' + toastKind;
  if (host === document.body) toast.classList.add('global-command-toast');
  toast.textContent = text;
  host.appendChild(toast);
  setTimeout(() => {
    if (toast.parentNode) toast.remove();
  }, 4500);
}

function unquoteSlashValue(value) {
  const s = String(value || '').trim();
  if (s.length >= 2) {
    const first = s[0];
    const last = s[s.length - 1];
    if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
      return s.slice(1, -1).trim();
    }
  }
  return s;
}

function takeGoalNumberOption(text, names) {
  for (const name of names) {
    const re = new RegExp(`(?:^|\\s)--${name}(?:=|\\s+)(\\d+)(?=\\s|$)`, 'i');
    const match = text.match(re);
    if (match) {
      const value = parseInt(match[1], 10);
      const nextText = (text.slice(0, match.index) + ' ' + text.slice(match.index + match[0].length)).replace(/\s+/g, ' ').trim();
      return { found: true, value, text: nextText };
    }
  }
  return { found: false, value: null, text };
}

function takeGoalFlag(text, names) {
  for (const name of names) {
    const re = new RegExp(`(?:^|\\s)--${name}(?=\\s|$)`, 'i');
    const match = text.match(re);
    if (match) {
      const nextText = (text.slice(0, match.index) + ' ' + text.slice(match.index + match[0].length)).replace(/\s+/g, ' ').trim();
      return { found: true, text: nextText };
    }
  }
  return { found: false, text };
}

function takeGoalStringOption(text, names) {
  for (const name of names) {
    const re = new RegExp(`(?:^|\\s)--${name}(?:=|\\s+)(\\S+)(?=\\s|$)`, 'i');
    const match = text.match(re);
    if (match) {
      const nextText = (text.slice(0, match.index) + ' ' + text.slice(match.index + match[0].length)).replace(/\s+/g, ' ').trim();
      return { found: true, value: match[1], text: nextText };
    }
  }
  return { found: false, value: null, text };
}

function parseGoalSlash(rest) {
  let text = String(rest || '').trim();
  if (!text) return { op: 'goal', params: {} };

  const exact = text.toLowerCase();
  const exactOps = {
    clear: 'goal-clear',
    reset: 'goal-clear',
    pause: 'goal-pause',
    paused: 'goal-pause',
    resume: 'goal-resume',
    active: 'goal-resume',
    edit: 'goal-edit',
    complete: 'goal-complete',
    completed: 'goal-complete',
    done: 'goal-complete',
    'budget-limited': 'goal-budget-limited',
    budget_limited: 'goal-budget-limited',
    status: 'goal',
    show: 'goal',
    get: 'goal',
  };
  if (exactOps[exact]) return { op: exactOps[exact], params: {} };

  const params = {};
  let op = 'goal';

  const clear = takeGoalFlag(text, ['clear']);
  if (clear.found) {
    return { op: 'goal-clear', params: {} };
  }
  text = clear.text;

  for (const [flag, action] of [
    ['pause', 'goal-pause'],
    ['resume', 'goal-resume'],
    ['complete', 'goal-complete'],
    ['budget-limited', 'goal-budget-limited'],
  ]) {
    const taken = takeGoalFlag(text, [flag]);
    if (taken.found) {
      op = action;
      text = taken.text;
      break;
    }
  }

  const status = takeGoalStringOption(text, ['status']);
  if (status.found) {
    params.status = status.value;
    text = status.text;
  }

  const budget = takeGoalNumberOption(text, ['budget', 'token-budget', 'tokens']);
  if (budget.found) {
    if (!Number.isFinite(budget.value) || budget.value <= 0) {
      return { error: '/goal failed: token budget must be a positive integer' };
    }
    params.tokenBudget = budget.value;
    text = budget.text;
  }

  const clearBudget = takeGoalFlag(text, ['clear-budget', 'no-budget']);
  if (clearBudget.found) {
    params.tokenBudget = null;
    text = clearBudget.text;
  }

  const objective = unquoteSlashValue(text);
  if (objective) {
    if ([...objective].length > 4000) {
      return { error: '/goal failed: objective must be 4000 characters or fewer' };
    }
    params.objective = objective;
  }

  return { op, params };
}

function parseCodexSlashCommand(text) {
  const trimmed = String(text || '').trim();
  if (!trimmed.startsWith('/')) return null;
  const match = trimmed.match(/^\/([A-Za-z][\w-]*)(?:\s+([\s\S]*))?$/);
  if (!match) return null;
  const name = match[1].toLowerCase();
  const rest = (match[2] || '').trim();
  if (name === 'fork') {
    const params = {};
    const forkName = unquoteSlashValue(rest);
    if (forkName) params.name = forkName;
    return { op: 'fork', params };
  }
  if (name === 'side' || name === 'btw') {
    const params = {};
    const prompt = unquoteSlashValue(rest);
    if (prompt) params.prompt = prompt;
    return { op: 'side', params };
  }
  if (name === 'fast') {
    if (rest) return { error: '/fast does not accept arguments' };
    return { op: 'fast', params: {} };
  }
  if (
    name === 'rewind-anchor' ||
    name === 'rewind-to-item' ||
    name === 'rollback-anchor' ||
    name === 'rollback-to-item'
  ) {
    const tokens = rest.split(/\s+/).map(s => s.trim()).filter(Boolean);
    if (tokens.length === 0) {
      return { error: `/${name} requires an item id` };
    }
    let position = 'after';
    const last = tokens[tokens.length - 1].toLowerCase();
    if (last === 'before' || last === 'after') {
      position = last;
      tokens.pop();
    }
    const itemId = unquoteSlashValue(tokens.join(' '));
    if (!itemId) return { error: `/${name} requires an item id` };
    return { op: 'rewind-anchor', params: { itemId, position } };
  }
  if (
    name === 'rewind-backout' ||
    name === 'rewind-inspect' ||
    name === 'context-rewind-backout'
  ) {
    const cacheReset = takeGoalFlag(rest, ['allow-cache-reset', 'allow-cache-breaking-fork']);
    const recordId = unquoteSlashValue(cacheReset.text);
    if (!recordId) return { error: `/${name} requires a rewind record id` };
    const params = { recordId, mode: 'inspect' };
    if (cacheReset.found) params.allowCacheReset = true;
    return { op: 'rewind-backout', params };
  }
  if (
    name === 'rewind-restore' ||
    name === 'context-rewind-restore'
  ) {
    const cacheReset = takeGoalFlag(rest, ['allow-cache-reset', 'allow-cache-breaking-fork']);
    const recordId = unquoteSlashValue(cacheReset.text);
    if (!recordId) return { error: `/${name} requires a rewind record id` };
    const params = { recordId, mode: 'restore' };
    if (cacheReset.found) params.allowCacheReset = true;
    return { op: 'rewind-backout', params };
  }
  if (name === 'rewind') {
    const tokens = rest.split(/\s+/).map(s => s.trim()).filter(Boolean);
    const mode = (tokens.shift() || '').toLowerCase();
    if (mode !== 'inspect' && mode !== 'backout' && mode !== 'fork' && mode !== 'restore') return null;
    const cacheReset = takeGoalFlag(tokens.join(' '), ['allow-cache-reset', 'allow-cache-breaking-fork']);
    const recordId = unquoteSlashValue(cacheReset.text);
    if (!recordId) return { error: `/rewind ${mode} requires a rewind record id` };
    const params = { recordId, mode: mode === 'backout' ? 'inspect' : mode };
    if (cacheReset.found) params.allowCacheReset = true;
    return { op: 'rewind-backout', params };
  }
  if (name === 'goal') return parseGoalSlash(rest);
  return null;
}

function dispatchCodexSlashCommand(parsed) {
  if (!parsed || parsed.error) {
    showControlToast('error', parsed?.error || 'Slash command failed');
    return false;
  }
  const sid = resolvePromptTargetSessionId();
  if (!sid) {
    if (parsed.op === 'fast') {
      dispatchControlMsg({ action: 'create_session', task: '/fast', agent: 'codex' });
      showControlToast('info', 'Starting a fast Codex session');
      return true;
    }
    showControlToast('error', `/${parsed.op}: select a target session first`);
    return false;
  }
  if (parsed.op === 'goal-edit') {
    const spec = codexThreadActionSpec('goal-edit');
    runCodexThreadActionFromUi('goal-edit', spec || {}, sid);
    return true;
  }
  if (dispatchCodexThreadAction(parsed.op, parsed.params || {}, sid)) {
    markActionPending(parsed.op, sid);
    return true;
  }
  return false;
}

function markActionPending(op, sessionId = '') {
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  if (!normalizedOp) return;
  releaseActionPending(normalizedOp);
  const btns = Array.from(document.querySelectorAll(
    `.control-action-btn[data-codex-action="${normalizedOp}"]`
  )).filter(el => el.offsetParent !== null);
  btns.forEach(btn => btn.classList.add('pending'));
  // Safety: if no result arrives, release the button ourselves but keep a
  // visible Activity row so the action does not silently disappear.
  const timeoutHandle = setTimeout(() => {
    controlPendingActions.delete(normalizedOp);
    btns.forEach(btn => btn.classList.remove('pending'));
    if (sessionId) stationUpsertCodexThreadActionActivity(normalizedOp, sessionId, 'timeout');
  }, 60000);
  controlPendingActions.set(normalizedOp, { timeoutHandle, btnEls: btns, sessionId });
}

function releaseActionPending(op) {
  const normalizedOp = String(op || '').trim().toLowerCase().replace(/_/g, '-');
  const entry = controlPendingActions.get(normalizedOp);
  if (!entry) return;
  clearTimeout(entry.timeoutHandle);
  // `btnEls` is the new field (plural, set by markActionPending); fall back
  // to `btnEl` for any stale in-flight entry from older builds.
  if (Array.isArray(entry.btnEls)) {
    entry.btnEls.forEach(btn => btn.classList.remove('pending'));
  } else if (entry.btnEl) {
    entry.btnEl.classList.remove('pending');
  }
  controlPendingActions.delete(normalizedOp);
}

function parseGoalElapsedText(text) {
  const raw = String(text || '').trim();
  if (!raw) return null;
  const match = raw.match(/(?:(\d+)d\s*)?(?:(\d+)h\s*)?(?:(\d+)m\s*)?(?:(\d+)s\s*)?$/i);
  if (!match || !match[0].trim()) return null;
  const days = parseInt(match[1] || '0', 10);
  const hours = parseInt(match[2] || '0', 10);
  const minutes = parseInt(match[3] || '0', 10);
  const seconds = parseInt(match[4] || '0', 10);
  return (((days * 24 + hours) * 60 + minutes) * 60) + seconds;
}

function goalFromCodexActionResult(evt = {}) {
  if (!evt.success || !String(evt.action || '').startsWith('goal')) return undefined;
  const message = String(evt.message || '').trim();
  if (/^(goal cleared|no goal to clear|no goal set)$/i.test(message)) return null;
  const match = message.match(/^(?:goal updated|current goal):\s+([\s\S]+)$/i);
  if (!match) return undefined;
  const body = match[1].trim();
  const detailsMatch = body.match(/^([\s\S]*)\s+\(([\s\S]*)\)$/);
  const objective = (detailsMatch ? detailsMatch[1] : body).trim();
  if (!objective) return undefined;
  const details = detailsMatch ? detailsMatch[2] : '';
  const status = details.match(/(?:^|,\s*)status\s+([^,]+)/i)?.[1]?.trim();
  const elapsedText = details.match(/(?:^|,\s*)elapsed\s+([^,]+)/i)?.[1]?.trim();
  return normalizeSessionGoal({
    objective,
    status: status || 'active',
    elapsedSeconds: parseGoalElapsedText(elapsedText),
  });
}

function codexFastModeFromActionResult(evt = {}) {
  const action = String(evt.action || '').trim().toLowerCase().replace(/_/g, '-');
  if (action !== 'fast' || evt.success !== true) return null;
  const message = String(evt.message || '').toLowerCase();
  if (/\benabled\b/.test(message)) return true;
  if (/\bdisabled\b/.test(message)) return false;
  return null;
}

function handleCodexThreadActionResult(evt) {
  releaseActionPending(evt.action);
  const goal = goalFromCodexActionResult(evt);
  const goalSessionId = evt.session_id || evt.sessionId;
  stationUpsertCodexThreadActionActivity(
    evt.action,
    goalSessionId,
    evt.success ? 'ok' : 'failed',
    evt.message
  );
  if (goal !== undefined && goalSessionId) {
    applySessionGoal({ session_id: goalSessionId, goal });
  }
  const fastMode = codexFastModeFromActionResult(evt);
  if (fastMode !== null && goalSessionId) {
    applyCodexFastModeToSession(goalSessionId, fastMode);
  }
  let successText = `/${evt.action}: ${evt.message}`;
  if (evt.success && evt.action === 'rename') {
    const sid = String(evt.session_id || '').trim();
    const name = String(evt.message || '').replace(/^Codex thread renamed to\s+/i, '').trim();
    if (sid && name && name !== evt.message) {
      applySessionRenameToUi(sid, 'codex', name);
      updateSessionWindow(sid, { name });
      successText = `Renamed session to ${name}`;
    }
    refreshSessionWindowMetadata(600);
    scheduleSessionsMetadataRefresh(200);
    closeSessionRenameModalIfMatches(sid);
  } else if (!evt.success && evt.action === 'rename') {
    showSessionRenameStatus(`Rename session failed: ${evt.message}`, 'error', evt.session_id);
  }
  showControlToast(
    evt.success ? 'success' : 'error',
    evt.success
      ? successText
      : `/${evt.action} failed: ${evt.message}`
  );
}

function handleCodexThreadActionRequested(evt) {
  const action = String(evt.action || '').trim().toLowerCase().replace(/_/g, '-');
  const sid = String(evt.session_id || evt.sessionId || '').trim();
  if (!action) return;
  markActionPending(action, sid);
  stationUpsertCodexThreadActionActivity(action, sid, 'requested');
}

function normalizeUserMessageEditStatus(status) {
  const s = String(status || '').trim().toLowerCase().replace(/_/g, '-');
  if (s === 'success' || s === 'succeeded' || s === 'applied') return 'ok';
  if (s === 'error' || s === 'failure') return 'failed';
  if (['requested', 'attaching', 'queued', 'running', 'ok', 'failed'].includes(s)) return s;
  return 'requested';
}

function userMessageEditStatusIsTerminal(status) {
  return status === 'ok' || status === 'failed';
}

function handleUserMessageEditStatus(evt = {}) {
  const sid = String(evt.session_id || evt.sessionId || '').trim();
  const turn = Number(evt.user_turn_index ?? evt.userTurnIndex ?? 0);
  const status = normalizeUserMessageEditStatus(evt.status);
  const message = String(evt.message || evt.reason || '').trim();
  if (!sid && (!Number.isInteger(turn) || turn <= 0)) return;

  stationUpsertUserMessageEditActivity(sid, turn, status, message, {
    // submitEditedUserMessage renders the local "requested" row immediately.
    renderLog: status !== 'requested',
  });

  if (sid && !processingLogReplay) {
    if (userMessageEditStatusIsTerminal(status)) {
      clearSessionWindowPendingActive(sid, 'idle');
    } else {
      markSessionWindowPendingActive(sid);
    }
  }
  if (status === 'failed') {
    const detail = compactSessionText(message || 'edit failed');
    showControlToast('error', detail.toLowerCase().startsWith('edit failed') ? detail : `Edit failed: ${detail}`);
  }
}

function handleUserMessageRewind(evt = {}) {
  const sid = String(evt.session_id || evt.sessionId || '').trim();
  const turn = Number(evt.user_turn_index ?? evt.userTurnIndex ?? 0);
  const turnsRemoved = Number(evt.turns_removed ?? evt.turnsRemoved ?? 0);
  if (sid && Number.isInteger(turn) && turn > 0) {
    if (!applyThreadHistoryChangeSet(evt)) {
      markActivityContextRewind(sid, turn, turnsRemoved);
    }
    stationUpsertUserMessageEditActivity(
      sid,
      turn,
      'ok',
      turnsRemoved > 0
        ? `rewound ${turnsRemoved} turn${turnsRemoved === 1 ? '' : 's'}`
        : 'rewound existing context',
      { renderLog: false }
    );
  }
}

function handleSessionRenameResult(evt) {
  const sid = String(evt.session_id || '').trim();
  if (evt.success) {
    const name = compactSessionText(evt.name);
    if (sid && name) applySessionRenameToUi(sid, evt.source || '', name);
    refreshSessionWindowMetadata(600);
    scheduleSessionsMetadataRefresh(200);
    closeSessionRenameModalIfMatches(sid);
  } else {
    showSessionRenameStatus(`Rename session failed: ${evt.message}`, 'error', sid);
  }
  stationScheduleUpdate();
  showControlToast(
    evt.success ? 'success' : 'error',
    evt.success ? evt.message : `Rename session failed: ${evt.message}`
  );
}

function currentGoalForSessionAction(sessionId) {
  const sid = String(sessionId || '').trim();
  for (const id of relatedSessionIdsForSession(sid)) {
    const meta = sessionMetadataById.get(id) || {};
    const goal = normalizeSessionGoal(meta.goal || meta.session_goal || meta.sessionGoal || null);
    if (goal) return goal;
  }
  return null;
}

async function promptCodexThreadActionParams(opts = {}) {
  let params = {};
  if (opts.promptName) {
    const name = await showDashboardPrompt({
      title: 'Fork thread',
      label: 'Fork name',
      placeholder: 'Leave empty for auto',
      multiline: false,
    });
    if (name === null) return null;
    if (name.trim()) params.name = name.trim();
  } else if (opts.promptSide) {
    const prompt = await showDashboardPrompt({
      title: 'Start side thread',
      label: 'Side prompt',
      rows: 5,
      submitLabel: 'Start side',
    });
    if (prompt === null) return null;
    if (!prompt.trim()) {
      showControlToast('error', '/side failed: prompt is required');
      return null;
    }
    params.prompt = prompt.trim();
  } else if (opts.promptReview) {
    const prompt = await showDashboardPrompt({
      title: 'Review prompt',
      label: 'Prompt',
      placeholder: 'Leave empty to review current changes with Codex defaults',
      rows: 5,
      submitLabel: 'Review',
    });
    if (prompt === null) return null;
    if (prompt.trim()) params.prompt = prompt.trim();
  } else if (opts.promptRename) {
    const name = await showDashboardPrompt({
      title: 'Rename thread',
      label: opts.promptRenameLabel || 'Thread name',
      multiline: false,
      submitLabel: 'Rename',
    });
    if (name === null) return null;
    if (!name.trim()) {
      showControlToast('error', '/rename failed: thread name is required');
      return null;
    }
    params.name = name.trim();
  } else if (opts.promptGoal) {
    const objective = await showDashboardPrompt({
      title: 'Set goal',
      label: 'Goal objective',
      rows: 5,
      maxLength: 4000,
      submitLabel: 'Continue',
    });
    if (objective === null) return null;
    if (!objective.trim()) {
      showControlToast('error', '/goal failed: objective is required');
      return null;
    }
    if ([...objective.trim()].length > 4000) {
      showControlToast('error', '/goal failed: objective must be 4000 characters or fewer');
      return null;
    }
    params.objective = objective.trim();
    const budget = await showDashboardPrompt({
      title: 'Set token budget',
      label: 'Token budget',
      placeholder: 'Optional',
      multiline: false,
      submitLabel: 'Set goal',
    });
    if (budget === null) return null;
    if (budget.trim()) {
      const parsed = parseInt(budget.trim(), 10);
      if (isNaN(parsed) || parsed <= 0) {
        showControlToast('error', '/goal failed: token budget must be a positive integer');
        return null;
      }
      params.tokenBudget = parsed;
    }
  } else if (opts.promptGoalEdit) {
    const goal = currentGoalForSessionAction(opts.sessionId);
    if (!goal?.objective) {
      showControlToast('error', '/goal edit failed: no goal is currently set');
      return null;
    }
    const objective = await showDashboardPrompt({
      title: 'Edit goal',
      label: 'Goal objective',
      rows: 5,
      maxLength: 4000,
      submitLabel: 'Update goal',
      initialValue: goal.objective,
    });
    if (objective === null) return null;
    if (!objective.trim()) {
      showControlToast('error', '/goal edit failed: objective is required');
      return null;
    }
    if ([...objective.trim()].length > 4000) {
      showControlToast('error', '/goal edit failed: objective must be 4000 characters or fewer');
      return null;
    }
    params.objective = objective.trim();
    if (goal.status === 'budget-limited' || goal.status === 'complete') {
      params.status = 'active';
    }
  } else if (opts.turns) {
    const n = parseInt(opts.turns, 10);
    if (!isNaN(n) && n > 0) params.turns = n;
  }
  if (opts.confirm) {
    const ok = await showDashboardConfirm({
      title: opts.confirmTitle || 'Confirm Codex action',
      message: opts.confirm,
      confirmLabel: opts.confirmLabel || 'Continue',
    });
    if (!ok) return null;
  }
  return params;
}

async function onControlActionBtnClick(ev) {
  const btn = ev.currentTarget;
  if (btn.classList.contains('pending') || btn.disabled) return;
  const op = btn.dataset.codexAction;
  if (!op) return;
  const spec = codexThreadActionSpec(op);
  if (!spec) {
    showControlToast('error', `Unknown Codex thread action /${op}`);
    return;
  }
  await runCodexThreadActionFromUi(op, spec);
}

async function runCodexThreadActionFromUi(op, opts = {}, sessionId = '') {
  const sid = String(sessionId || '').trim() || resolvePromptTargetSessionId();
  const params = await promptCodexThreadActionParams({
    promptName: opts.promptName,
    promptSide: opts.promptSide,
    promptReview: opts.promptReview,
    promptRename: opts.promptRename,
    promptGoal: opts.promptGoal,
    promptGoalEdit: opts.promptGoalEdit,
    sessionId: sid,
    turns: opts.turns,
    confirm: opts.confirm,
    confirmTitle: opts.confirmTitle,
    confirmLabel: opts.confirmLabel,
  });
  if (params === null) return;
  // Init is a task dispatch, not an RPC: send as a regular task instead so
  // the agent loop picks it up and AGENTS.md flows through the normal
  // approval path. The backend dispatcher also accepts /init and will
  // return an error if it doesn't recognise it.
  if (op === 'init') {
    const task = 'Create or update AGENTS.md at the project root with a concise project description, directory structure, important commands, and coding conventions. Base it on the existing repository files.';
    if (dispatchSessionControlMsg({ action: 'start_task', task, direct: true })) {
      showControlToast('success', '/init: dispatched AGENTS.md task');
    }
    return;
  }
  const dispatchOp = opts.dispatchOp || op;
  if (dispatchCodexThreadAction(dispatchOp, params, sid)) {
    markActionPending(dispatchOp, sid);
  }
}

function wireControlPaneListeners() {
  const $ = id => document.getElementById(id);
  const sandboxSel = $('control-codex-sandbox');
  const approvalSel = $('control-codex-approval');
  const modelInp = $('control-codex-model');
  const reasoningSel = $('control-codex-reasoning');
  const serviceTierSel = $('control-codex-service-tier');
  const webSearch = $('control-codex-web-search');
  const managedContextSel = $('control-codex-managed-context');
  const contextArchiveSel = $('control-codex-context-archive');
  const networkCb = $('control-codex-network');
  const rootsTA = $('control-codex-writable-roots');
  if (sandboxSel) sandboxSel.addEventListener('change', onControlSandboxChange);
  if (approvalSel) approvalSel.addEventListener('change', onControlApprovalChange);
  if (reasoningSel) reasoningSel.addEventListener('change', onControlReasoningChange);
  if (serviceTierSel) serviceTierSel.addEventListener('change', onControlServiceTierChange);
  if (webSearch) webSearch.addEventListener('change', onControlWebSearchChange);
  if (managedContextSel) managedContextSel.addEventListener('change', onControlManagedContextChange);
  if (contextArchiveSel) contextArchiveSel.addEventListener('change', onControlContextArchiveChange);
  if (networkCb) networkCb.addEventListener('change', onControlNetworkChange);
  // Commit text inputs on blur or Enter so every keystroke doesn't ping the server.
  if (modelInp) {
    modelInp.addEventListener('change', onControlModelCommit);
    modelInp.addEventListener('blur', onControlModelCommit);
    modelInp.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        onControlModelCommit(ev);
      }
    });
  }
  if (rootsTA) {
    rootsTA.addEventListener('change', onControlWritableRootsCommit);
    rootsTA.addEventListener('blur', onControlWritableRootsCommit);
    // Commit on Ctrl/Cmd+Enter; plain Enter is newline (textarea).
    rootsTA.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) {
        ev.preventDefault();
        onControlWritableRootsCommit(ev);
      }
    });
  }
  document.querySelectorAll('.control-action-btn[data-codex-action]').forEach(btn => {
    btn.addEventListener('click', onControlActionBtnClick);
  });

  // ── Claude Code section ──
  const claudeModelInp = $('control-claude-model');
  const claudeModeSel = $('control-claude-permission-mode');
  const claudeToolsTA = $('control-claude-allowed-tools');
  if (claudeModeSel) claudeModeSel.addEventListener('change', onControlClaudePermissionModeChange);
  if (claudeModelInp) {
    claudeModelInp.addEventListener('change', onControlClaudeModelCommit);
    claudeModelInp.addEventListener('blur', onControlClaudeModelCommit);
    claudeModelInp.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        onControlClaudeModelCommit(ev);
      }
    });
  }
  if (claudeToolsTA) {
    claudeToolsTA.addEventListener('change', onControlClaudeAllowedToolsCommit);
    claudeToolsTA.addEventListener('blur', onControlClaudeAllowedToolsCommit);
    claudeToolsTA.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) {
        ev.preventDefault();
        onControlClaudeAllowedToolsCommit(ev);
      }
    });
  }

  // ── Approval-rule listeners (internal-agent autonomy gates) ──
  document.querySelectorAll('select[data-approval-category]').forEach(sel => {
    sel.addEventListener('change', onControlApprovalRuleChange);
  });
}

// Wire up once at DOM ready. switchActivitySubtab('control') will
// refreshControlPane() to populate values on first open.
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', wireControlPaneListeners);
} else {
  wireControlPaneListeners();
}

