// ── Station status metrics chip ──
// Always-on `renderer · fps · displays` readout in #station-status-metrics.
// Prefers the structured station.debug_json() export when the loaded WASM
// build provides it; otherwise scrapes the debug_state() string. Polled at
// 1 Hz (cheap string work, one textContent write only on change) and only
// while the Station tab is visible — never per frame.
let stationMetricsTimer = null;
let stationMetricsLastText = '';

function stationParsedMetrics() {
  let renderer = '';
  let fps = '';
  let displays = '';
  if (station && typeof station.debug_json === 'function') {
    try {
      const info = JSON.parse(String(station.debug_json() || '{}')) || {};
      renderer = String(info.renderer || '');
      if (Number.isFinite(Number(info.fps))) fps = String(Number(info.fps));
      if (Number.isFinite(Number(info.displays))) displays = String(Number(info.displays));
    } catch (_) { /* fall through to debug_state scraping */ }
  }
  const state = station?.debug_state?.() || '';
  if (!renderer) renderer = (/renderer=(\S+)/.exec(state) || [])[1] || stationRendererLabel();
  if (!fps) fps = (/fps=([0-9.]+)/.exec(state) || [])[1] || '';
  if (!displays) displays = (/displays=(\d+)/.exec(state) || [])[1] || '';
  return {
    renderer: renderer || 'Canvas',
    fps: fps ? String(Math.round(Number(fps))) : '',
    displays: displays || '0',
  };
}

function stationUpdateMetricsChip() {
  if (!station || activeTab !== 'station') return;
  const el = document.getElementById('station-status-metrics');
  if (!el) return;
  const m = stationParsedMetrics();
  const text = `${m.renderer} · ${m.fps || '--'} fps · ${m.displays} display${m.displays === '1' ? '' : 's'}`;
  if (text === stationMetricsLastText) return;
  stationMetricsLastText = text;
  el.textContent = text;
}

function stationEnsureMetricsTimer() {
  if (stationMetricsTimer) return;
  stationMetricsTimer = window.setInterval(stationUpdateMetricsChip, 1000);
  stationUpdateMetricsChip();
}

// Render-liveness watchdog: while the Station is the active tab and the
// page is visible, the WebGPU loop never parks (surface keepalive), so a
// sustained fps=0 means the renderer's active flag desynced from the DOM
// tab state (e.g. a legacy flow toggled tabs mid-action) or the loop lost
// its rAF. Re-assert active + resize + repaint; cheap (one debug_state
// parse every 5s) and a no-op when healthy.
let stationLivenessTimer = null;
let stationLivenessZeroStreak = 0;

function stationEnsureLivenessWatchdog() {
  if (stationLivenessTimer) return;
  stationLivenessTimer = window.setInterval(() => {
    if (!station || activeTab !== 'station' || document.hidden) {
      stationLivenessZeroStreak = 0;
      return;
    }
    const state = station.debug_state?.() || '';
    const fpsMatch = state.match(/fps=(\d+)/);
    const gpuActive = state.includes('gpu=true');
    const fps = fpsMatch ? Number(fpsMatch[1]) : 0;
    if (gpuActive && fps === 0) {
      stationLivenessZeroStreak += 1;
    } else {
      stationLivenessZeroStreak = 0;
      return;
    }
    if (stationLivenessZeroStreak >= 2) {
      stationLivenessZeroStreak = 0;
      console.warn('station liveness watchdog: renderer parked while tab active — recovering');
      stationStatus('Renderer paused unexpectedly — recovering');
      station.set_active(true);
      station.resize();
      stationScheduleUpdate({ immediate: true });
    }
  }, 5000);
}

function stationWebgpuRequested() {
  return !window.location.href.includes('station_gpu=canvas')
    && !window.location.href.includes('station_gpu=off');
}

// Read by scripts/validate-dashboard.cjs station probes via globalThis —
// keep the name and signature stable even though app.html no longer calls it.
function stationWebgpuStatusLabel() {
  if (!stationWebgpuRequested()) return 'off';
  if (stationWebgpuUnavailable) return 'unavailable';
  const state = station?.debug_state?.() || '';
  if (state.includes('gpu=true')) return 'active';
  if (state.includes('gpu=false')) return 'unavailable';
  return 'requested';
}

// Read by scripts/validate-dashboard.cjs station probes via globalThis.
function stationRendererLabel() {
  if (station) {
    const state = station.debug_state?.() || '';
    if (state.includes('gpu=true')) return 'WebGPU';
    if (state.includes('gpu=false')) return 'Canvas';
    return state || 'Canvas';
  }
  return 'Canvas';
}

// This dashboard runs as a module script, so the validator's globalThis
// lookups need explicit exposure. stationHotspotBoxes intentionally stays
// module-scoped: the validator then measures the live overlay buttons,
// which track the WASM hotspot_rects() truth instead of the legacy mirror.
globalThis.stationRendererLabel = stationRendererLabel;
globalThis.stationWebgpuStatusLabel = stationWebgpuStatusLabel;

function stationRendererStateLabel() {
  return station?.debug_state?.() || 'not initialized';
}

// WebGPU -> canvas-renderer fallback ladder. When the WASM reports gpu=false
// it keeps rendering through its 2D canvas path; surface a clear explanation
// so degraded users know why the scene looks flat.
function stationFallbackFromUnavailableWebgpu(instance) {
  if (!instance || instance !== station || !stationWebgpuRequested()) return;
  const state = instance.debug_state?.() || '';
  if (!state.includes('gpu=false')) return;
  stationWebgpuUnavailable = true;
  stationStatus(state);
  stationSyncHotspots();
  stationUpdateSnapshot();
  if (activeTab === 'station') stationRefreshSharedCaches();
}

function stationSnapshotVisible() {
  return activeTab === 'station';
}

async function ensureStation() {
  if (station) return station;
  if (stationInitPromise) return stationInitPromise;
  stationInitPromise = (async () => {
    if (!stationWasmReady) {
      stationStatus('Loading Station WASM');
      await stationInit('/wasm-station/station_web_bg.wasm');
      stationWasmReady = true;
    }
    const scene = document.getElementById('station-scene-canvas');
    const hud = document.getElementById('station-hud-canvas');
    if (!scene || !hud) return null;
    station = new StationWeb(scene, hud);
    stationLastSnapshotJson = '';
    station.set_action_callback((action) => handleStationAction(action));
    stationSetupComposerInput();
    stationApplyViewSettings(false);
    const active = activeTab === 'station';
    station.set_active(active);
    stationStatus('Station online');
    window.setTimeout(() => stationStatus(station?.debug_state?.() || 'Station online'), 1500);
    window.setTimeout(() => {
      stationFallbackFromUnavailableWebgpu(station);
      stationStatus(station?.debug_state?.() || 'Station online');
    }, 3200);
    stationSyncHotspots();
    stationUpdateSnapshot();
    stationRegisterExistingSources();
    stationRenderPeerChips();
    stationEnsureMetricsTimer();
    stationEnsureLivenessWatchdog();
    if (active) stationRefreshSharedCaches();
    return station;
  })().catch(err => {
    stationInitPromise = null;
    // stationWasmReady is deliberately NOT reset here: it only becomes
    // true after stationInit resolves, and re-running wasm-bindgen init
    // on an already-loaded module (because StationWeb construction or a
    // later setup step threw) would double-initialize the module on
    // retry. A load failure leaves it false on its own.
    throw err;
  });
  return stationInitPromise;
}

function stationSetActive(active) {
  if (station) {
    station.set_active(active);
    // Only resize while visible: on deactivation the pane is already
    // hidden and client sizes read 0, which would collapse the canvases
    // (and the WebGPU surface) to 1x1 on every tab switch away.
    if (active) station.resize();
    if (active) {
      stationSyncHotspots();
      stationRefreshSharedCaches();
      stationUpdateSnapshot();
      stationRegisterExistingSources();
      stationUpdateMetricsChip();
    } else {
      // The composer overlay must never float over another tab.
      stationSyncComposer();
    }
  } else if (active) {
    ensureStation().catch(err => {
      console.error('Station init failed:', err);
      stationStatus('Station failed: ' + (err && err.message ? err.message : err));
    });
  }
}

function stationRefreshSharedCaches() {
  if (!station) return;
  if (!sessionsLoaded) {
    stationSessionsIndexLoading = true;
    stationSessionsIndexError = '';
    fetchSessionsForHost(selfPeerId, { force: false })
      .then(sessions => applyLoadedSessions(sessions, document.getElementById('sessions-aggregate'), selfPeerId))
      .catch(err => {
        stationSessionsIndexError = err && err.message ? err.message : String(err || 'session index failed');
        console.warn('Station sessions refresh failed:', err);
      })
      .finally(() => {
        stationSessionsIndexLoading = false;
        stationScheduleUpdate();
      });
  }
  refreshControlPane()
    .then(stationScheduleUpdate)
    .catch(err => console.warn('Station control refresh failed:', err));
  refreshManagedContextPane({ force: false })
    .then(stationScheduleUpdate)
    .catch(err => console.warn('Station managed-context refresh failed:', err));
  refreshChangesList({ refreshActive: false, quiet: true })
    .then(stationScheduleUpdate)
    .catch(err => console.warn('Station changes refresh failed:', err));
}

// Coalesce snapshot rebuilds: dashboard event streams (log lines, status,
// usage, peer logs) call this per event, but while Station is visible we
// rebuild at most once per window on the trailing edge. Tab activation
// (stationSetActive) and local user actions (handleStationAction /
// stationRenderedSelectPanel) rebuild immediately. Hidden tab: skip
// entirely; activation rebuilds unconditionally.
const STATION_SNAPSHOT_COALESCE_MS = 300;

function stationScheduleUpdate(opts = {}) {
  if (!station) return;
  if (!stationSnapshotVisible()) return;
  if (opts.immediate) {
    if (stationScheduleUpdate._timer) {
      clearTimeout(stationScheduleUpdate._timer);
      stationScheduleUpdate._timer = null;
    }
    stationUpdateSnapshot();
    return;
  }
  if (stationScheduleUpdate._timer) return;
  stationScheduleUpdate._timer = setTimeout(() => {
    stationScheduleUpdate._timer = null;
    stationUpdateSnapshot();
  }, STATION_SNAPSHOT_COALESCE_MS);
}

let stationLastSnapshotError = '';

function stationUpdateSnapshot() {
  if (!station) return;
  if (!stationSnapshotVisible()) return;
  try {
    const snapshot = buildStationSnapshot();
    const snapshotJson = JSON.stringify(snapshot);
    if (snapshotJson !== stationLastSnapshotJson) {
      station.update_snapshot(snapshot);
      stationLastSnapshotJson = snapshotJson;
      // A new snapshot can reflow the rendered HUD, so refresh the overlay
      // geometry and the live aria-label counts here (≤ once per coalesced
      // 300ms window), keeping both event-driven instead of per-frame.
      stationSyncHotspots();
      stationUpdateHotspotAria(snapshot);
    }
    // Live transcript follow: new activity for the open session triggers
    // a coalesced refetch (no-op when the viewer is closed).
    stationMaybeRefreshTranscript();
    stationLastSnapshotError = '';
    stationStatus(station.debug_state());
  } catch (e) {
    // Surface rejections where the operator looks (#station-status) —
    // once per distinct error, not per coalesced tick; success clears the
    // latch so a recurrence shows again.
    const message = e && e.message ? e.message : String(e || 'unknown error');
    if (message !== stationLastSnapshotError) {
      stationLastSnapshotError = message;
      stationStatus(`snapshot rejected: ${message}`);
    }
    console.warn('station snapshot failed:', e);
  }
}

function stationRenderedPrimaryActive() {
  return activeTab === 'station' && !!station;
}

function stationRenderedSelectPanel(target, message = '') {
  if (!stationRenderedPrimaryActive()) return false;
  if (target && !stationActivateTarget(target)) station.select_by_id?.(target);
  stationStatus(message || (station.debug_state?.() || 'Station online'));
  stationScheduleUpdate({ immediate: true });
  return true;
}

// Legacy dashboard surface for each Station panel, used when the rendered
// canvas panel cannot take the selection (Station tab inactive or the WASM
// is not initialized yet).
const STATION_LEGACY_PANEL_ROUTES = Object.freeze({
  'system:activity': ['activity', 'log'],
  'system:context': ['activity', 'context'],
  'system:managed': ['activity', 'managed'],
  'system:changes': ['activity', 'changes'],
  'system:controls': ['activity', 'control'],
  'system:sessions': ['sessions', 'recent'],
  'system:worktrees': ['sessions', 'worktrees'],
  'system:peers': ['settings', 'network'],
});

// Open a Station panel: prefer the rendered canvas panel, then the legacy
// tab that hosts the same capability, and report when neither exists.
function stationOpenPanel(target, message = '') {
  if (stationRenderedSelectPanel(target, message)) return true;
  const route = STATION_LEGACY_PANEL_ROUTES[String(target || '')];
  if (route) {
    stationForceRouteTo(route[0], route[1]);
    return true;
  }
  const label = String(target || '').replace('system:', '') || 'panel';
  stationStatus(`Opening ${label}`);
  showControlToast?.('info', `${label} lives in the rendered Station tab`);
  return false;
}

window.stationProbe = Object.assign(window.stationProbe || {}, {
  renderedPrimary: () => stationRenderedPrimaryActive(),
  state: () => station?.debug_state?.() || stationRendererStateLabel(),
  debugJson: () => {
    try {
      return station && typeof station.debug_json === 'function' ? String(station.debug_json() || '') : '';
    } catch (_) {
      return '';
    }
  },
  hotspots: () => {
    const rect = stationHudRect();
    return stationResolvedHotspotBoxes(rect.width || 1, rect.height || 1);
  },
  snapshot: () => buildStationSnapshot(),
  // Composer + transcript accessors for agents / the validator: drive the
  // full in-canvas paths programmatically.
  composer: () => {
    try {
      return station && typeof station.composer_state === 'function'
        ? JSON.parse(station.composer_state() || 'null')
        : null;
    } catch (_) {
      return null;
    }
  },
  composerType: (text) => {
    const input = stationComposerInputEl();
    if (!input) return false;
    input.value = String(text ?? '');
    return true;
  },
  composerSubmit: () => {
    stationComposerSubmit();
    return true;
  },
  composerOpen: (mode) => {
    if (!station || typeof station.set_composer !== 'function') return false;
    station.set_composer(true, mode === 'launch' ? 'launch' : 'send');
    stationHandleComposerEvent({ op: 'focus' });
    return true;
  },
  composerClose: () => {
    if (!station || typeof station.set_composer !== 'function') return false;
    station.set_composer(false, stationComposerMode);
    stationSyncComposer();
    return true;
  },
  openTranscript: (sessionId, source) => stationOpenTranscript(sessionId, source ? { source } : {}),
  openDiff: (path) => stationOpenDiff(path),
  // Read-only session-store accessors for probes: the app script is
  // module-scoped, so validators cannot reach these directly.
  sessionMeta: (sid) => {
    try {
      return JSON.parse(JSON.stringify(sessionMetadataById.get(String(sid)) || null));
    } catch (_) {
      return null;
    }
  },
  primarySessionId: () => {
    try {
      return String(currentSessionFullId || '');
    } catch (_) {
      return '';
    }
  },
  select: (target) => {
    station?.select_by_id?.(target ? String(target) : null);
    return station?.debug_state?.() || '';
  },
  // Programmatic activation for automation: the WASM handle is
  // module-scoped, so the validator's wasm-activate driver reaches
  // activate()/debug_json() through this facade.
  activate: (target) => {
    if (!target) return false;
    try {
      return stationActivateTarget(String(target));
    } catch (_) {
      return false;
    }
  },
  update: (snapshot) => {
    if (!station || !snapshot) return '';
    station.update_snapshot(snapshot);
    try {
      stationLastSnapshotJson = JSON.stringify(snapshot);
    } catch (_) {
      stationLastSnapshotJson = '';
    }
    return station.debug_state?.() || '';
  },
});

function stationLogEventId(c, fallbackIndex = '') {
  const hostId = String(c?.host_id || c?.hostId || selfPeerId || 'local');
  const sessionId = String(c?.session_id || c?.sessionId || '');
  const itemId = String(c?.item_id || c?.itemId || '');
  const explicit = String(c?.station_event_id || c?.stationEventId || c?.event_id || c?.eventId || c?.id || '');
  if (itemId) return itemId;
  if (explicit) return explicit;
  const ts = String(c?.ts || c?.timestamp || '');
  const source = String(c?.source || c?.level || c?.event || '');
  const content = compactSessionTextBounded(
    c?.content || c?.msg || c?.message || '',
    STATION_ACTIVITY_TEXT_CHAR_LIMIT,
    { suffix: false }
  )
    .slice(0, 80);
  const parts = [hostId, sessionId, ts, source, content, String(fallbackIndex || '')].filter(Boolean);
  return parts.length ? parts.join(':') : `event:${Date.now()}`;
}

function stationSessionWindowHistoryEvent(item, win, index) {
  const record = sessionWindowHistoryRecord(item);
  if (!record) return null;
  const hostId = record.host_id || record.hostId || selfPeerId;
  const sessionId = record.session_id || record.sessionId || win?.sessionId || '';
  const source = record.source || record.level || 'info';
  const content = record.content || record.summary || record.message || '';
  const eventId = stationLogEventId({
    ...record,
    host_id: hostId,
    session_id: sessionId,
    source,
    content,
  });
  const msg = compactSessionTextBounded(content, STATION_ACTIVITY_TEXT_CHAR_LIMIT);
  return {
    id: eventId,
    action: 'log',
    hostId,
    sessionId,
    agentId: hostId === selfPeerId ? 'primary-agent' : 'peer-' + sanitizeStationId(hostId),
    ts: formatLogTimestampLabel(record.ts || record.timestamp || ''),
    level: record.level || 'info',
    source,
    msg: msg || source,
  };
}

function stationSessionWindowHistoryEvents(limit = 80) {
  const safeLimit = Math.max(1, Number(limit) || 80);
  const events = [];
  for (const win of sessionWindows.values()) {
    const history = ensureSessionWindowHistory(win);
    const start = Math.max(0, history.length - safeLimit);
    for (let index = start; index < history.length; index++) {
      const item = history[index];
      events.push(stationSessionWindowHistoryEvent(item, win, `${win.sessionId || 'session'}:${index}`));
    }
  }
  return events.filter(Boolean).slice(-safeLimit);
}

function stationActivityEvents() {
  const events = [];
  const seen = new Set();
  const add = event => {
    if (!event) return;
    const id = stationActivityEventKey(event);
    if (seen.has(id)) return;
    seen.add(id);
    events.push(event.id ? event : { ...event, id });
  };
  for (const event of stationLogEvents) add(event);
  for (const event of stationSessionWindowHistoryEvents()) add(event);
  return events.slice(-80);
}

function stationBuildActivitySummary(allEventsInput = null) {
  const state = stationActivityFilteredEvents(allEventsInput);
  const allEvents = state.allEvents || [];
  const events = state.events || [];
  const latest = events.length ? events[events.length - 1] : null;
  const threadKeys = new Set(events.map(ev => {
    const sessionId = String(ev.sessionId || ev.session_id || '').trim();
    if (sessionId) return `session:${sessionId}`;
    const hostId = String(ev.hostId || ev.host_id || selfPeerId || 'local').trim();
    return `source:${hostId}:${stationActivitySourceFor(ev) || 'activity'}`;
  }));
  return {
    retainedCount: allEvents.length,
    shownCount: events.length,
    managedCount: events.filter(stationActivityEventIsManaged).length,
    threadCount: threadKeys.size,
    hostFilter: state.hostFilter || '',
    levelFilter: state.levelFilter || '',
    sourceFilter: state.sourceFilter || '',
    query: state.query || '',
    verbosity: document.getElementById('verbosity-select')?.value || 'normal',
    latestId: latest ? stationActivityEventKey(latest) : '',
    latestLevel: latest ? stationActivityLevelFor(latest) : '',
    latestSource: latest ? stationActivitySourceFor(latest) : '',
    latestHost: latest ? (latest.hostId || latest.host_id || selfPeerId || 'local') : '',
    latestSessionId: latest ? (latest.sessionId || latest.session_id || '') : '',
    latestText: latest ? stationActivityEventCopyText(latest) : '',
    visibleEvents: events.slice(-60),
    topLevels: stationActivityTopSummary(events, stationActivityLevelFor),
    topSources: stationActivityTopSummary(events, stationActivitySourceFor),
    topHosts: stationActivityTopSummary(events, ev => {
      const hostId = ev.hostId || ev.host_id || selfPeerId || 'local';
      return stationHostLabel(hostId) || hostId;
    }),
  };
}

function handleStationAction(action) {
  if (!action || typeof action !== 'object') return;
  if (action.type === 'approval') {
    const hostId = action.host_id || selfPeerId;
    const decision = action.decision || 'approve';
    if (hostId === selfPeerId || hostId === 'local') {
      const map = { approve: 'approve', skip: 'skip', approve_all: 'approve_all', deny: 'deny' };
      if (app) processCommands(app.send_approval(map[decision] || 'approve'));
    } else {
      const map = { approve: 'accept', skip: 'cancel', approve_all: 'accept', deny: 'decline' };
      resolvePeerApproval(hostId, action.approval_id, map[decision] || decision);
    }
  } else if (action.type === 'open_display') {
    stationOpenDisplay(action.host_id, action.display_id ?? action.displayId ?? stationPeerDisplayId());
  } else if (action.type === 'display_runway_action') {
    stationHandleDisplayRunwayAction(action);
  } else if (action.type === 'thread_action') {
    stationHandleThreadAction(action);
  } else if (action.type === 'session_action') {
    stationHandleSessionAction(action);
  } else if (action.type === 'managed_action') {
    stationHandleManagedAction(action);
  } else if (action.type === 'context_action') {
    stationHandleContextAction(action);
  } else if (action.type === 'activity_action') {
    stationHandleActivityAction(action);
  } else if (action.type === 'controls_action') {
    stationHandleControlsAction(action);
  } else if (action.type === 'composer') {
    stationHandleComposerEvent(action);
  } else if (action.type === 'view_set') {
    // Canvas View-panel control (mood pill or slider release): persist the
    // draft and re-apply through set_visuals. The renderer already shows
    // the scrubbed value; this round-trip makes it durable.
    stationViewSet(String(action.key || ''), action.value);
    return;
  } else if (action.type === 'changes_action') {
    stationHandleChangesAction(action);
  } else if (action.type === 'navigate') {
    const tab = String(action.tab || '').trim();
    const subtab = action.subtab ? String(action.subtab).trim() : undefined;
    if (!VALID_TABS.includes(tab)) return;
    if (tab === 'activity' && subtab && !VALID_ACTIVITY_SUBTABS.includes(subtab)) return;
    if (tab === 'terminal' && subtab && !VALID_TERM_SUBTABS.includes(subtab)) return;
    if (tab === 'settings' && subtab && !VALID_SETTINGS_SUBTABS.includes(subtab)) return;
    if (tab === 'sessions' && subtab && !VALID_SESSIONS_SUBTABS.includes(subtab)) return;
    stationForceRouteTo(tab, subtab);
    return;
  }
  // Local user action: reflect the result in the rendered panel immediately
  // instead of waiting out the coalescing window.
  stationScheduleUpdate({ immediate: true });
}

function stationForceRouteTo(tab, subtab, after) {
  const target = subtab ? `${tab}/${subtab}` : tab;
  const nextHash = `#${target}`;
  if (window.location.hash !== nextHash) {
    history.pushState(null, '', nextHash);
  }
  switchTab(tab);
  if (tab === 'activity' && subtab) {
    if (activeActivitySubtab === subtab) activeActivitySubtab = '';
    switchActivitySubtab(subtab);
  } else if (tab === 'terminal' && subtab) {
    switchTerminalSubtab(subtab);
  } else if (tab === 'settings' && subtab) {
    switchSettingsSubtab(subtab);
  } else if (tab === 'sessions') {
    switchSessionsSubtab(subtab || 'recent');
  }
  if (typeof after === 'function') {
    setTimeout(after, 0);
  }
}

let stationActivityDockFilter = '';
let stationActivityDockHost = activeHostFilter || '';
let stationActivityDockLevel = '';
let stationActivityDockSource = '';
let stationSelectedContextPart = '';
let stationSelectedChangePath = '';
let stationChangesHistoryLoading = false;
let stationManagedActionResult = '';
let stationManagedActivitySignal = null;
let stationSessionConfigResult = { sessionId: '', text: '', kind: '' };
const stationSessionConfigDrafts = new Map();
let stationManagedDraft = {
  anchor: '',
  position: '',
  reason: '',
  primer: '',
  preserve: '',
  discard: '',
  artifacts: '',
  nextSteps: '',
  record: '',
  backoutMode: '',
  backoutName: '',
};
let stationSelectedPeerHost = '';
const stationPeerDisplayTargets = new Map();
let stationPeerStatus = '';
let stationPeerStatusKind = '';
const stationDisplayTelemetry = new Map();
let stationLocalDisplays = [];
let stationLocalDisplaysLoading = false;
let stationPeerDraft = {
  url: '',
  label: '',
  via: '',
  browserTcpVia: '',
  selectedHostId: '',
  displayId: '0',
  message: '',
  capabilityFilter: 'display',
  routeCapabilities: 'display',
  routeInstructions: '',
};
const STATION_VIEW_KEY = 'intendant_station_view';
const STATION_VIEW_DEFAULTS = Object.freeze({
  layout: 'orbital',
  mood: 'cockpit',
  fov: 55,
  motion: 1,
  ar: 0.45,
  density: 1,
});

function stationClampNumber(value, fallback, min, max) {
  const n = Number(value);
  if (!Number.isFinite(n)) return fallback;
  return Math.max(min, Math.min(max, n));
}

function stationNormalizeViewDraft(raw = {}) {
  return {
    layout: raw.layout === 'constellation' ? 'constellation' : STATION_VIEW_DEFAULTS.layout,
    mood: raw.mood === 'calm' ? 'calm' : STATION_VIEW_DEFAULTS.mood,
    fov: stationClampNumber(raw.fov, STATION_VIEW_DEFAULTS.fov, 35, 85),
    motion: stationClampNumber(raw.motion, STATION_VIEW_DEFAULTS.motion, 0, 2),
    ar: stationClampNumber(raw.ar, STATION_VIEW_DEFAULTS.ar, 0, 1),
    density: stationClampNumber(raw.density, STATION_VIEW_DEFAULTS.density, 0.5, 1.8),
  };
}

function stationLoadViewDraft() {
  try {
    return stationNormalizeViewDraft(JSON.parse(localStorage.getItem(STATION_VIEW_KEY) || '{}'));
  } catch (_) {
    return stationNormalizeViewDraft();
  }
}

function stationSaveViewDraft() {
  try {
    localStorage.setItem(STATION_VIEW_KEY, JSON.stringify(stationViewDraft));
  } catch (_) {}
}

let stationViewDraft = stationLoadViewDraft();
let stationLaunchDraft = {
  task: '',
  name: '',
  project: '',
  agent: '',
  command: '',
  managed: '',
  archive: '',
  fast: false,
  // Execution shape for the internal agent: '' (auto) | 'orchestrate' |
  // 'direct'.
  execution: '',
};

const STATION_SYSTEM_TARGETS = Object.freeze([
  ['system:activity', 'Activity'],
  ['system:context', 'Context'],
  ['system:managed', 'Managed'],
  ['system:changes', 'Changes'],
  ['system:sessions', 'Sessions'],
  ['system:worktrees', 'Worktrees'],
  ['system:peers', 'Peers'],
  ['system:controls', 'Controls'],
  ['system:view', 'View'],
]);

function stationHudRect() {
  return document.getElementById('station-hud-canvas')?.getBoundingClientRect()
    || document.querySelector('.station-pane')?.getBoundingClientRect()
    || new DOMRect(0, 0, 1, 1);
}

// LEGACY FALLBACK geometry, hand-mirrored from crates/station-web/src/lib.rs
// StationInner::draw_station_*. Only consulted when the loaded WASM build
// does not export station.hotspot_rects() (see stationWasmHotspotBoxes) —
// newer builds report the rendered control regions directly and this mirror
// no longer needs to track Rust byte-for-byte.
const STATION_HUD_GEOMETRY = Object.freeze({
  layoutButtons: Object.freeze([
    Object.freeze({ name: 'layout:orbital', x: 96, y: 10, w: 78, h: 23 }),
    Object.freeze({ name: 'layout:constellation', x: 182, y: 10, w: 116, h: 23 }),
  ]),
  renderedDesktopMin: 820,
  margin: 24,
  topY: 58,
  gap: 14,
  compactX: 18,
  compactY: 130,
  compactGapX: 16,
});

function stationHotspotBoxes(cssW, cssH) {
  const boxes = [];
  const add = (name, x, y, w, h) => boxes.push({ name, x, y, w, h });
  for (const box of STATION_HUD_GEOMETRY.layoutButtons) {
    add(box.name, box.x, box.y, box.w, box.h);
  }
  const g = STATION_HUD_GEOMETRY;
  if (cssW < g.renderedDesktopMin) {
    // Mirrors hud.rs compact_grid: all nine targets wrap two per row when
    // the panel has the rows, with a density-scaled pitch (58px / 9 tiles
    // at the 1.0 default). The old slice(0, 8) dropped system:view.
    const tileW = ((cssW - 36) - 44) * 0.5;
    const density = Number.isFinite(Number(stationViewDraft?.density)) ? Number(stationViewDraft.density) : 1;
    const panelH = Math.max(180, cssH - 92);
    const pitch = Math.min(72, Math.max(40, 58 / Math.max(0.5, density)));
    const rows = Math.max(1, Math.floor((panelH - 66) / pitch));
    const preferred = Math.min(9, Math.max(4, Math.round(9 * density)));
    const count = Math.min(preferred, rows * 2);
    STATION_SYSTEM_TARGETS.slice(0, count).forEach(([name], idx) => {
      const col = idx % 2;
      const row = Math.floor(idx / 2);
      add(
        name,
        g.compactX + 14 + col * (tileW + g.compactGapX),
        g.compactY + row * pitch,
        tileW,
        pitch - 10,
      );
    });
    return boxes;
  }
  const margin = g.margin;
  const gap = g.gap;
  const availableW = Math.max(760, cssW - margin * 2);
  const availableH = Math.max(420, cssH - g.topY - 24);
  const commandH = cssH < 640 ? 78 : 92;
  const laneH = cssH < 640 ? 68 : 78;
  const mainH = Math.max(250, availableH - commandH - laneH - gap * 2);
  const centerW = availableW;
  const centerX = margin;
  const mainY = g.topY + commandH + gap;
  const coreH = Math.min(560, Math.max(330, mainH));
  const matrixY = mainY + coreH - 118;
  const matrixW = (centerW - 72) / 3;
  ['system:activity', 'system:context', 'system:managed', 'system:sessions', 'system:peers', 'system:changes', 'system:worktrees', 'system:controls', 'system:view']
    .forEach((name, idx) => {
      const col = idx % 3;
      const row = Math.floor(idx / 3);
      add(name, centerX + 30 + col * matrixW, matrixY + 25 + row * 31, matrixW - 8, 25);
    });
  return boxes;
}

// Live hotspot geometry from the WASM HUD when the build exports it.
// Returns null (→ caller falls back to the legacy mirrored math) when the
// export is missing, returns malformed data, or reports no usable boxes.
function stationWasmHotspotBoxes() {
  if (!station || typeof station.hotspot_rects !== 'function') return null;
  try {
    const raw = station.hotspot_rects();
    const list = typeof raw === 'string' ? JSON.parse(raw) : raw;
    if (!Array.isArray(list)) return null;
    const boxes = [];
    for (const entry of list) {
      const name = String(entry?.name || '').trim();
      const x = Number(entry?.x);
      const y = Number(entry?.y);
      const w = Number(entry?.w);
      const h = Number(entry?.h);
      if (!name || !Number.isFinite(x) || !Number.isFinite(y)) continue;
      if (!(w > 0) || !(h > 0)) continue;
      boxes.push({ name, x, y, w, h });
    }
    return boxes.length ? boxes : null;
  } catch (_) {
    return null;
  }
}

function stationResolvedHotspotBoxes(cssW, cssH) {
  return stationWasmHotspotBoxes() || stationHotspotBoxes(cssW, cssH);
}

// Repositions the invisible hotspot buttons over the rendered HUD controls.
// Event-driven only: snapshot updates (already coalesced to 300ms), resize,
// and tab activation — never per frame. Diffed so unchanged geometry costs
// one JSON.stringify of ≤11 tiny boxes and zero style writes.
let stationLastHotspotBoxesJson = '';

function stationSyncHotspots() {
  // The composer overlay tracks the drawn strip independently of the
  // hotspot boxes (which cover only system/layout zones and may be
  // unchanged while the composer opens/closes).
  stationSyncComposer();
  const layer = document.getElementById('station-hotspots');
  if (!layer) return;
  const rect = stationHudRect();
  const boxes = stationResolvedHotspotBoxes(rect.width || 1, rect.height || 1);
  const boxesJson = JSON.stringify(boxes);
  if (boxesJson === stationLastHotspotBoxesJson) return;
  stationLastHotspotBoxesJson = boxesJson;
  for (const box of boxes) {
    const el = layer.querySelector(`[data-station-hotspot="${box.name}"]`);
    if (!el) continue;
    el.style.left = `${box.x}px`;
    el.style.top = `${box.y}px`;
    el.style.width = `${box.w}px`;
    el.style.height = `${box.h}px`;
  }
}

// ── Hotspot accessibility ──
// The hotspot buttons are invisible (the WASM canvas draws the visuals) but
// stay in the tree for screen readers, keyboards, and automation. Refresh
// their aria-labels with live counts from each applied snapshot; writes are
// cached so an unchanged label costs one Map lookup.
const stationHotspotAriaApplied = new Map();

function stationSetHotspotLabel(name, label) {
  if (stationHotspotAriaApplied.get(name) === label) return;
  const el = document.querySelector(`[data-station-hotspot="${name}"]`);
  if (!el) return;
  el.setAttribute('aria-label', label);
  stationHotspotAriaApplied.set(name, label);
}

function stationUpdateHotspotAria(snapshot) {
  const s = snapshot || {};
  const sessions = s.sessions || {};
  const changes = s.changes || {};
  const activity = s.activity || {};
  const managed = s.managed || {};
  const context = s.context || {};
  const peerHosts = (Array.isArray(s.hosts) ? s.hosts : []).filter(h => h && h.region !== 'primary');
  const peersConnected = peerHosts.filter(h => h.connected).length;
  // attentionQueue is a summary object ({count, blocked, ...}), not an
  // array — the old Array.isArray read meant this label never showed a
  // count.
  const attention = stationNum(s.attentionQueue?.count);
  const layout = stationViewDraft.layout === 'constellation' ? 'constellation' : 'orbital';
  const n = stationNum;
  stationSetHotspotLabel('layout:orbital', `Orbital layout${layout === 'orbital' ? ' (active)' : ''}`);
  stationSetHotspotLabel('layout:constellation', `Constellation layout${layout === 'constellation' ? ' (active)' : ''}`);
  stationSetHotspotLabel('system:activity', `Activity: ${n(activity.shownCount)} of ${n(activity.retainedCount)} events`);
  stationSetHotspotLabel('system:context', context.available
    ? `Context: ${stationCompactNumber(n(context.tokens))} tokens`
    : 'Context: no live snapshot');
  stationSetHotspotLabel('system:managed', `Managed context: ${managed.mode || 'vanilla'}, ${n(managed.records)} records, ${n(managed.anchors)} anchors`);
  stationSetHotspotLabel('system:changes', `Changes: ${n(changes.count)} files (${n(changes.added)} added, ${n(changes.modified)} modified, ${n(changes.deleted)} deleted)`);
  stationSetHotspotLabel('system:sessions', `Sessions: ${n(sessions.total)} total, ${n(sessions.active)} active, ${n(sessions.external)} external`);
  stationSetHotspotLabel('system:worktrees', `Worktrees: ${n(sessions.worktrees)}`);
  stationSetHotspotLabel('system:peers', `Peers and displays: ${peersConnected} of ${peerHosts.length} peer${peerHosts.length === 1 ? '' : 's'} connected, ${displaySlots.size + peerDisplayConnections.size} streams`);
  stationSetHotspotLabel('system:controls', attention
    ? `Control surfaces: ${attention} item${attention === 1 ? '' : 's'} awaiting attention`
    : 'Control surfaces');
  stationSetHotspotLabel('system:view', `Station view: ${layout} layout`);
}

// Prefer the WASM activate() export (full hotspot activation) when the
// loaded station build provides it; callers fall back to select_by_id when
// it is missing or declines the target.
function stationActivateTarget(target) {
  if (!station || typeof station.activate !== 'function') return false;
  try {
    return station.activate(String(target || '')) === true;
  } catch (_) {
    return false;
  }
}

// ── Station header peer chips ──
// One glass chip per display-capable peer, rendered in the band above the
// canvas and wired to the same openPeerDisplay flow as the Settings →
// Network daemons "View display" buttons. Re-rendered from
// renderDaemonsList (peer add/remove/state push events) and station init —
// not on any frame or snapshot path.
function stationRenderPeerChips() {
  const row = document.getElementById('station-peer-chips');
  if (!row) return;
  const peers = daemons.filter(d => peerCanShareDisplay(d));
  if (!peers.length) {
    row.replaceChildren();
    row.classList.add('hidden');
    return;
  }
  const frag = document.createDocumentFragment();
  const label = document.createElement('span');
  label.className = 'station-peer-chips-label';
  label.textContent = 'peer displays';
  frag.appendChild(label);
  for (const d of peers) {
    const hostId = String(d.host_id || '');
    const name = compactSessionText(d.label || hostId) || hostId;
    const displayId = stationPeerDisplayIdForHost(hostId);
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'station-peer-chip';
    chip.disabled = !d.connected;
    chip.title = d.connected
      ? `Open a live view of display ${displayId} on ${name}`
      : `${name} is disconnected`;
    chip.setAttribute('aria-label', d.connected
      ? `View display ${displayId} on ${name}`
      : `${name} (disconnected)`);
    const dot = document.createElement('span');
    dot.className = d.connected ? 'chip-dot ok' : 'chip-dot';
    chip.appendChild(dot);
    chip.appendChild(document.createTextNode(`${name} · d${displayId}`));
    chip.addEventListener('click', () => stationOpenDisplay(hostId, stationPeerDisplayIdForHost(hostId)));
    frag.appendChild(chip);
  }
  row.replaceChildren(frag);
  row.classList.remove('hidden');
}

function stationApplyViewSettings(showToast = false, persist = true) {
  const layout = stationViewDraft.layout === 'constellation' ? 'constellation' : 'orbital';
  stationViewDraft.layout = layout;
  if (station?.set_layout) station.set_layout(layout);
  if (station?.set_visuals) {
    // Fall back to STATION_VIEW_DEFAULTS only for missing/invalid values so a
    // legitimate user-chosen 0 (motion or AR) survives the round trip.
    const visualNumber = (value, fallback) =>
      (Number.isFinite(Number(value)) ? Number(value) : fallback);
    station.set_visuals(
      stationViewDraft.mood === 'calm' ? 'calm' : 'cockpit',
      visualNumber(stationViewDraft.fov, STATION_VIEW_DEFAULTS.fov),
      visualNumber(stationViewDraft.motion, STATION_VIEW_DEFAULTS.motion),
      visualNumber(stationViewDraft.ar, STATION_VIEW_DEFAULTS.ar),
      visualNumber(stationViewDraft.density, STATION_VIEW_DEFAULTS.density),
    );
  }
  if (persist) stationSaveViewDraft();
  stationScheduleUpdate({ immediate: true });
  if (showToast) showControlToast?.('success', 'Station view updated');
}

function stationViewSet(key, value) {
  if (key === 'layout') stationViewDraft.layout = value === 'constellation' ? 'constellation' : 'orbital';
  else if (key === 'mood') stationViewDraft.mood = value === 'calm' ? 'calm' : 'cockpit';
  else if (key === 'fov') stationViewDraft.fov = stationClampNumber(value, STATION_VIEW_DEFAULTS.fov, 35, 85);
  else if (key === 'motion') stationViewDraft.motion = stationClampNumber(value, STATION_VIEW_DEFAULTS.motion, 0, 2);
  else if (key === 'ar') stationViewDraft.ar = stationClampNumber(value, STATION_VIEW_DEFAULTS.ar, 0, 1);
  else if (key === 'density') stationViewDraft.density = stationClampNumber(value, STATION_VIEW_DEFAULTS.density, 0.5, 1.8);
  stationApplyViewSettings(false);
}

function stationHotspotAt(clientX, clientY) {
  const rect = stationHudRect();
  const x = clientX - rect.left;
  const y = clientY - rect.top;
  const boxes = stationResolvedHotspotBoxes(rect.width || 1, rect.height || 1);
  for (let i = boxes.length - 1; i >= 0; i -= 1) {
    const box = boxes[i];
    if (x >= box.x && x <= box.x + box.w && y >= box.y && y <= box.y + box.h) {
      return box.name;
    }
  }
  return '';
}

function stationSetPeerStatus(message = '', kind = '') {
  stationPeerStatus = message || '';
  stationPeerStatusKind = kind || '';
  if (typeof setDaemonsStatus === 'function') {
    setDaemonsStatus(stationPeerStatus, stationPeerStatusKind);
  }
}

function stationLocalDisplayRows() {
  const rows = Array.isArray(cachedDisplays) && cachedDisplays.length
    ? cachedDisplays
    : stationLocalDisplays;
  return Array.isArray(rows) ? rows : [];
}

function stationLocalDisplayLabel(display) {
  const id = Number(display?.id ?? 0);
  const name = display?.name || displayLabel(id);
  const size = display?.width && display?.height ? `${display.width}x${display.height}` : '';
  return [name, size].filter(Boolean).join(' ');
}

async function stationLoadLocalDisplays(force = false) {
  if (stationLocalDisplaysLoading) return;
  if (!force && stationLocalDisplayRows().length) {
    stationScheduleUpdate();
    return;
  }
  stationLocalDisplaysLoading = true;
  stationSetPeerStatus('Listing local displays...', '');
  stationScheduleUpdate();
  try {
    const data = await fetchLocalDisplaysPayload();
    stationLocalDisplays = normalizeDisplaysPayload(data);
    cachedDisplays = stationLocalDisplays;
    stationSetPeerStatus(stationLocalDisplays.length
      ? `Found ${stationLocalDisplays.length} local display${stationLocalDisplays.length === 1 ? '' : 's'}`
      : 'No local displays reported', 'ok');
  } catch (err) {
    stationSetPeerStatus(`Display list failed: ${err?.message || err}`, 'error');
  } finally {
    stationLocalDisplaysLoading = false;
    stationScheduleUpdate();
  }
}

function stationPeerDisplayId() {
  const raw = String(stationPeerDraft.displayId ?? '0').trim();
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
}

function stationPeerDisplayIdForHost(hostId, fallback = stationPeerDisplayId()) {
  const id = String(hostId || '').trim();
  const stored = id ? stationPeerDisplayTargets.get(id) : undefined;
  const parsed = Number.parseInt(String(stored ?? fallback ?? 0), 10);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
}

function stationRememberPeerDisplayTarget(hostId = stationPeerDraft.selectedHostId, displayId = stationPeerDisplayId()) {
  const id = String(hostId || '').trim();
  if (!id) return;
  const parsed = Number.parseInt(String(displayId ?? 0), 10);
  const did = Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
  stationPeerDisplayTargets.set(id, did);
  if (id === String(stationPeerDraft.selectedHostId || '').trim()) {
    stationPeerDraft.displayId = String(did);
  }
}

function stationSetPeerDisplayTarget(hostId, displayId) {
  stationSetPeerTarget(hostId, displayId);
  stationRememberPeerDisplayTarget(hostId, displayId);
  stationScheduleUpdate();
}

function stationSetPeerTarget(hostId, displayId = undefined) {
  const id = String(hostId || '').trim();
  if (id) {
    stationSelectedPeerHost = id;
    stationPeerDraft.selectedHostId = id;
  }
  const did = displayId === undefined
    ? stationPeerDisplayIdForHost(id || stationPeerDraft.selectedHostId)
    : stationPeerDisplayIdForHost(id || stationPeerDraft.selectedHostId, displayId);
  stationPeerDraft.displayId = String(did);
  if (id) stationPeerDisplayTargets.set(id, did);
}

function stationPeerDisplayName(peer) {
  if (!peer) return '';
  return compactSessionText(peer.label || peer.host_id || peer.id || '') || '';
}

function stationPeerCapabilityLabels(peer) {
  return (Array.isArray(peer?.capabilities) ? peer.capabilities : [])
    .map(cap => cap && (cap.kind || cap.name || cap.capability || cap))
    .map(cap => String(cap || '').trim())
    .filter(Boolean);
}

function stationPeerDisplayStatusPayload() {
  return {
    generated_at: new Date().toISOString(),
    self: {
      host_id: selfPeerId || 'local',
      label: selfHostLabel || '',
      display_access: document.getElementById('sb-display-access')?.textContent || 'off',
      shared_display: userDisplayGranted ? Number(grantedDisplayId) : null,
      local_streams: displaySlots.size,
      remote_streams: peerDisplayConnections.size,
    },
    selected: {
      peer_id: stationPeerDraft.selectedHostId || stationSelectedPeerHost || '',
      display_id: stationPeerDisplayId(),
    },
    peers: daemons.map(peer => ({
      host_id: peer.host_id || peer.id || '',
      label: peer.label || '',
      url: peer.url || '',
      connected: !!peer.connected,
      capabilities: stationPeerCapabilityLabels(peer),
    })),
    local_displays: stationLocalDisplayRows().map(display => ({
      id: Number(display?.id ?? 0),
      label: stationLocalDisplayLabel(display),
      width: display?.width || 0,
      height: display?.height || 0,
      primary: !!display?.is_primary,
    })),
    local_streams: Array.from(displaySlots.values()).map(slot => ({
      display_id: slot.displayId,
      connected: !!slot.connected,
      input: slot.authorityState || 'unknown',
      width: slot.width || 0,
      height: slot.height || 0,
      interactive: !!slot.interactive,
      recording: !!slot.recording,
    })),
    remote_streams: Array.from(peerDisplayConnections.values()).map(conn => ({
      host_id: conn.hostId || '',
      display_id: conn.displayId,
      session_id: conn.sessionId || '',
      status: conn.displayStatusText || 'Connecting...',
      input: conn.peerAuthorityState || 'unknown',
      via: conn.advertiseTcpViaUrl || '',
    })),
  };
}

async function stationCopyPeerDisplayStatus() {
  await copyTextToClipboard(JSON.stringify(stationPeerDisplayStatusPayload(), null, 2));
  stationSetPeerStatus('Peer/display status JSON copied', 'ok');
  stationScheduleUpdate();
}

function stationSelectedPeer() {
  const selected = String(stationPeerDraft.selectedHostId || stationSelectedPeerHost || '').trim();
  return daemons.find(d => d.host_id === selected) || daemons[0] || null;
}

function stationSharedViewStatusPayload() {
  return {
    visible: !!sharedViewState.visible,
    action: sharedViewState.action || '',
    display_id: sharedViewState.displayId,
    display_target: sharedViewState.displayTarget || '',
    target: sharedViewState.visible
      ? sharedViewDisplayLabel(sharedViewState.displayId, sharedViewState.displayTarget)
      : '',
    reason: sharedViewState.reason || '',
    note: sharedViewState.note || '',
    region: sharedViewState.region || null,
    can_take_input: sharedViewState.visible
      && sharedViewState.action === 'input_request'
      && sharedViewState.displayId !== null
      && displaySlots.has(sharedViewState.displayId),
  };
}

function stationFocusSharedViewDisplay() {
  if (sharedViewState.displayId === null) {
    stationSetPeerStatus('No shared-view display is selected.', 'error');
    stationScheduleUpdate();
    return;
  }
  const slot = displaySlots.get(sharedViewState.displayId);
  if (!slot) {
    stationSetPeerStatus('Shared-view display is not currently streamed in this browser.', 'error');
    stationScheduleUpdate();
    return;
  }
  slot.el?.scrollIntoView?.({ block: 'center', inline: 'center', behavior: 'smooth' });
  slot.el?.classList.add('shared-view-active');
  renderSharedViewFocus(slot, sharedViewState.region, sharedViewState.note);
}

function stationBrowserWorkspaceStatusPayload() {
  const rows = Array.from(browserWorkspaces.values())
    .filter(w => (w.status || '') !== 'closed')
    .sort((a, b) => String(b.updated_at || '').localeCompare(String(a.updated_at || '')));
  const latest = rows[0] || null;
  return {
    count: rows.length,
    latest_id: latest?.id || '',
    latest_label: latest ? (latest.label || latest.id || '') : '',
    latest_status: latest?.status || '',
    latest_provider: latest?.provider || '',
    latest_url: latest?.url || '',
    latest_updated: latest?.updated_at || '',
    lease: latest?.lease ? (latest.lease.holder_id || 'leased') : '',
    can_create: !!app,
    can_acquire: !!latest?.id,
    can_close: !!latest?.id,
    validation_state: latest ? (latest.status || 'unknown') : 'idle',
    validation_detail: latest
      ? [latest.provider || 'browser', latest.lease ? 'leased' : 'unleased', latest.browser_executable_source || '']
        .filter(Boolean)
        .join(' / ')
      : 'no workspace',
  };
}

function stationBrowserWorkspaceDraftPayload() {
  return {
    url: (document.getElementById('browser-workspace-url')?.value || '').trim(),
    label: (document.getElementById('browser-workspace-label')?.value || '').trim(),
    provider: (document.getElementById('browser-workspace-provider')?.value || 'auto').trim() || 'auto',
  };
}

function stationOperateBrowserWorkspace(op) {
  const status = stationBrowserWorkspaceStatusPayload();
  if (op === 'copy') {
    copyTextToClipboard(JSON.stringify(status, null, 2))
      .then(() => stationSetPeerStatus('Browser workspace status copied', 'ok'))
      .catch(err => stationSetPeerStatus(`Browser workspace copy failed: ${err?.message || err}`, 'error'))
      .finally(stationScheduleUpdate);
    return;
  }
  if (op === 'create') {
    const draft = stationBrowserWorkspaceDraftPayload();
    dispatchDashboardActionMsg({
      action: 'create_browser_workspace',
      url: draft.url || undefined,
      provider: draft.provider || 'auto',
      label: draft.label || undefined,
      owner_session_id: currentSessionId || undefined,
    });
    stationSetPeerStatus('Browser workspace create requested', 'ok');
    stationScheduleUpdate();
    return;
  }
  if (!status.latest_id) {
    stationSetPeerStatus('No active browser workspace selected.', 'error');
    stationScheduleUpdate();
    return;
  }
  if (op === 'acquire') {
    acquireBrowserWorkspace(status.latest_id, true);
    stationScheduleUpdate();
    return;
  }
  if (op === 'close') {
    closeBrowserWorkspace(status.latest_id);
    stationScheduleUpdate();
  }
}

function stationLatestOperationalActivityPayload() {
  const needles = ['display', 'shared_view', 'shared view', 'browser', 'computer use', 'cu_', 'take_screenshot', 'execute_cu'];
  const events = stationActivityEvents().slice().reverse();
  const ev = events.find(item => {
    const text = [
      item?.source || '',
      item?.msg || item?.message || '',
      item?.detail || '',
      item?.tool || '',
      item?.event || '',
    ].join(' ').toLowerCase();
    return needles.some(needle => text.includes(needle));
  });
  if (!ev) return { label: '', detail: '' };
  return {
    label: compactSessionText([ev.level || '', ev.source || 'activity'].filter(Boolean).join(' / ')),
    detail: compactSessionText(ev.msg || ev.message || ev.detail || ev.id || ''),
  };
}

function stationDisplayControlTargetPayload() {
  const selectedPeer = stationSelectedPeer();
  const selectedHostId = selectedPeer?.host_id || stationPeerDraft.selectedHostId || stationSelectedPeerHost || '';
  const selectedDisplayId = selectedHostId
    ? stationPeerDisplayIdForHost(selectedHostId)
    : stationPeerDisplayId();
  const now = performance.now ? performance.now() : Date.now();
  const freshnessLabel = (stamp) => {
    const at = Number(stamp) || 0;
    if (!at) return '';
    const age = Math.max(0, now - at);
    if (age < 2000) return 'fresh';
    if (age < 30000) return `${Math.round(age / 1000)}s old`;
    return 'stale';
  };
  const selectedIsLocal = !selectedHostId || selectedHostId === selfPeerId || selectedHostId === 'local';
  const localSlot = selectedIsLocal && Number.isFinite(Number(selectedDisplayId))
    ? displaySlots.get(Number(selectedDisplayId))
    : null;
  if (localSlot) {
    const key = stationDisplayTelemetryKey('local', selfPeerId, localSlot.displayId);
    const telemetry = stationDisplayTelemetryFor(key, localSlot.videoEl, localSlot.pc);
    const cached = stationDisplayTelemetry.get(key) || {};
    return {
      kind: 'local_stream',
      label: `${selfHostLabel || 'local'} :${localSlot.displayId}`,
      target: `local:${localSlot.displayId}`,
      host_id: selfPeerId || 'local',
      display_id: localSlot.displayId,
      lane_id: `local:${localSlot.displayId}`,
      status: localSlot.connected ? 'connected' : 'connecting',
      authority: localSlot.authorityState || 'unknown',
      capture: localSlot.streaming
        ? (localSlot.frameIdEl?.textContent || `${localSlot._streamFrameCounter || 0} frames`)
        : (localSlot.videoEl?.videoWidth ? 'preview ready' : 'no frame'),
      freshness: freshnessLabel(cached.playbackAt || cached.statsAt),
      telemetry: telemetry.label || '',
      can_open: true,
      can_focus: true,
      can_take_input: (localSlot.authorityState || '') !== 'you',
      can_release_input: (localSlot.authorityState || '') === 'you',
      can_attach_frame: !!localSlot.videoEl?.videoWidth,
      can_capture: !!localSlot.videoEl?.videoWidth,
    };
  }
  const remoteConn = stationPeerDisplayConnection(selectedHostId, selectedDisplayId);
  if (remoteConn) {
    const key = stationDisplayTelemetryKey('remote', remoteConn.hostId, remoteConn.displayId, remoteConn.sessionId);
    const container = stationPeerDisplayContainer(remoteConn.hostId, false);
    const video = container && container.querySelector('.peer-display-video');
    const telemetry = stationDisplayTelemetryFor(key, video, remoteConn.pc);
    const cached = stationDisplayTelemetry.get(key) || {};
    return {
      kind: 'remote_stream',
      label: `${stationHostLabel(remoteConn.hostId)} :${remoteConn.displayId}`,
      target: `remote:${remoteConn.hostId}:${remoteConn.displayId}`,
      host_id: remoteConn.hostId || '',
      display_id: remoteConn.displayId,
      lane_id: `remote:${remoteConn.hostId}:${remoteConn.displayId}:${remoteConn.sessionId || ''}`,
      status: remoteConn.displayStatusText || 'connecting',
      authority: remoteConn.peerAuthorityState || 'unknown',
      capture: telemetry.label || 'stream telemetry pending',
      freshness: freshnessLabel(cached.playbackAt || cached.statsAt),
      telemetry: telemetry.label || '',
      can_open: true,
      can_focus: true,
      can_take_input: (remoteConn.peerAuthorityState || '') !== 'you',
      can_release_input: (remoteConn.peerAuthorityState || '') === 'you',
      can_attach_frame: false,
      can_capture: false,
    };
  }
  if (selectedPeer && peerCanShareDisplay(selectedPeer)) {
    return {
      kind: 'peer_target',
      label: `${stationPeerDisplayName(selectedPeer) || selectedPeer.host_id} :${selectedDisplayId}`,
      target: `target:${selectedPeer.host_id}:${selectedDisplayId}`,
      host_id: selectedPeer.host_id || '',
      display_id: selectedDisplayId,
      lane_id: `target:${selectedPeer.host_id}:${selectedDisplayId}`,
      status: selectedPeer.connected ? 'ready' : 'offline',
      authority: 'unknown',
      capture: 'not open',
      freshness: '',
      telemetry: '',
      can_open: true,
      can_focus: false,
      can_take_input: false,
      can_release_input: false,
      can_attach_frame: false,
      can_capture: false,
    };
  }
  const fallbackSlot = displaySlots.values().next().value;
  if (fallbackSlot) {
    const key = stationDisplayTelemetryKey('local', selfPeerId, fallbackSlot.displayId);
    const telemetry = stationDisplayTelemetryFor(key, fallbackSlot.videoEl, fallbackSlot.pc);
    const cached = stationDisplayTelemetry.get(key) || {};
    return {
      kind: 'local_stream',
      label: `${selfHostLabel || 'local'} :${fallbackSlot.displayId}`,
      target: `local:${fallbackSlot.displayId}`,
      host_id: selfPeerId || 'local',
      display_id: fallbackSlot.displayId,
      lane_id: `local:${fallbackSlot.displayId}`,
      status: fallbackSlot.connected ? 'connected' : 'connecting',
      authority: fallbackSlot.authorityState || 'unknown',
      capture: fallbackSlot.streaming
        ? (fallbackSlot.frameIdEl?.textContent || `${fallbackSlot._streamFrameCounter || 0} frames`)
        : (fallbackSlot.videoEl?.videoWidth ? 'preview ready' : 'no frame'),
      freshness: freshnessLabel(cached.playbackAt || cached.statsAt),
      telemetry: telemetry.label || '',
      can_open: true,
      can_focus: true,
      can_take_input: (fallbackSlot.authorityState || '') !== 'you',
      can_release_input: (fallbackSlot.authorityState || '') === 'you',
      can_attach_frame: !!fallbackSlot.videoEl?.videoWidth,
      can_capture: !!fallbackSlot.videoEl?.videoWidth,
    };
  }
  return {
    kind: 'none',
    label: '',
    target: '',
    host_id: '',
    display_id: null,
    lane_id: '',
    status: 'idle',
    authority: 'unknown',
    capture: 'no stream',
    freshness: '',
    telemetry: '',
    can_open: false,
    can_focus: false,
    can_take_input: false,
    can_release_input: false,
    can_attach_frame: false,
    can_capture: false,
  };
}

function stationOperateSelectedDisplayTarget(op) {
  const target = stationDisplayControlTargetPayload();
  if (!target || !target.kind || target.kind === 'none') return;
  if (op === 'copy') {
    copyTextToClipboard(JSON.stringify(target, null, 2))
      .then(() => stationSetPeerStatus('Selected display target copied', 'ok'))
      .catch(err => stationSetPeerStatus(`Display target copy failed: ${err?.message || err}`, 'error'))
      .finally(stationScheduleUpdate);
    return;
  }
  if (target.kind === 'peer_target') {
    if (op === 'open' || op === 'focus') {
      stationSetPeerTarget(target.host_id, target.display_id);
      stationOpenDisplay(target.host_id, target.display_id);
    }
    return;
  }
  if (target.kind === 'remote_stream') {
    const conn = stationPeerDisplayConnection(target.host_id, target.display_id);
    if (op === 'open' || op === 'focus') {
      stationSetPeerTarget(target.host_id, target.display_id);
      stationOpenDisplay(target.host_id, target.display_id);
    } else if (op === 'input') {
      if ((conn?.peerAuthorityState || '') === 'you') conn?.releaseControl?.();
      else conn?.takeControl?.();
    }
    stationScheduleUpdate();
    return;
  }
  if (target.kind === 'local_stream') {
    const slot = stationLocalDisplaySlot(target.display_id);
    if (op === 'open' || op === 'focus') {
      stationForceRouteTo('displays', '');
      slot?.el?.scrollIntoView?.({ block: 'center', inline: 'center', behavior: 'smooth' });
    } else if (op === 'input') {
      if ((slot?.authorityState || '') === 'you') slot?.releaseBtn?.click();
      else slot?.takeBtn?.click();
    } else if (op === 'capture') {
      slot?.attachBtn?.click();
    }
    stationScheduleUpdate();
  }
}

function stationDisplayTelemetryKey(kind, hostId, displayId, sessionId = '') {
  return [kind || 'display', hostId || selfPeerId || 'local', displayId ?? 0, sessionId || ''].join('|');
}

function stationDisplayTelemetryLabel(tel) {
  if (!tel) return '';
  return [tel.resolution, tel.fps ? `${Math.round(tel.fps)}fps` : '', tel.codec, tel.quality]
    .filter(Boolean)
    .join(' / ');
}

function stationDisplayTelemetryFor(key, videoEl, pc) {
  const now = performance.now ? performance.now() : Date.now();
  const cached = stationDisplayTelemetry.get(key) || {};
  if (videoEl) {
    const width = Number(videoEl.videoWidth) || cached.width || 0;
    const height = Number(videoEl.videoHeight) || cached.height || 0;
    cached.width = width;
    cached.height = height;
    cached.resolution = width && height ? `${width}x${height}` : cached.resolution || '';
    if (typeof videoEl.getVideoPlaybackQuality === 'function') {
      const quality = videoEl.getVideoPlaybackQuality();
      const frames = Number(quality.totalVideoFrames) || 0;
      const dropped = Number(quality.droppedVideoFrames) || 0;
      if (cached.playbackFrames !== undefined && frames >= cached.playbackFrames && now > cached.playbackAt) {
        const fps = ((frames - cached.playbackFrames) * 1000) / (now - cached.playbackAt);
        if (Number.isFinite(fps) && fps >= 0 && fps < 240) cached.fps = fps;
      }
      cached.playbackFrames = frames;
      cached.playbackAt = now;
      if (frames > 0 && dropped > 0) {
        cached.quality = `${Math.round((dropped / frames) * 100)}% dropped`;
      } else if (frames > 0 && !cached.quality) {
        cached.quality = 'steady';
      }
    }
  }
  if (pc && (!cached.lastStatsPollAt || now - cached.lastStatsPollAt > 1500)) {
    cached.lastStatsPollAt = now;
    stationDisplayTelemetry.set(key, cached);
    stationRefreshDisplayTelemetry(key, pc).catch(err => {
      const latest = stationDisplayTelemetry.get(key) || {};
      latest.quality = `stats unavailable: ${err?.message || err}`;
      stationDisplayTelemetry.set(key, latest);
    });
  } else {
    stationDisplayTelemetry.set(key, cached);
  }
  return {
    resolution: cached.resolution || '',
    fps: Number(cached.fps) || 0,
    codec: cached.codec || '',
    quality: cached.quality || '',
    label: stationDisplayTelemetryLabel(cached),
  };
}

async function stationRefreshDisplayTelemetry(key, pc) {
  if (!pc || typeof pc.getStats !== 'function') return;
  const stats = await pc.getStats();
  const cached = stationDisplayTelemetry.get(key) || {};
  const codecs = new Map();
  const inbound = [];
  stats.forEach(report => {
    if (report.type === 'codec') codecs.set(report.id, report);
    if (report.type === 'inbound-rtp' && (report.kind === 'video' || report.mediaType === 'video')) {
      inbound.push(report);
    }
  });
  const report = inbound
    .filter(item => !item.isRemote)
    .sort((a, b) => (Number(b.framesDecoded) || 0) - (Number(a.framesDecoded) || 0))[0];
  if (!report) return;
  const frames = Number(report.framesDecoded ?? report.framesReceived) || 0;
  const ts = Number(report.timestamp) || (performance.now ? performance.now() : Date.now());
  if (cached.statsFrames !== undefined && frames >= cached.statsFrames && ts > cached.statsAt) {
    const fps = ((frames - cached.statsFrames) * 1000) / (ts - cached.statsAt);
    if (Number.isFinite(fps) && fps >= 0 && fps < 240) cached.fps = fps;
  }
  cached.statsFrames = frames;
  cached.statsAt = ts;
  const codec = codecs.get(report.codecId);
  if (codec?.mimeType) {
    cached.codec = String(codec.mimeType).replace(/^video\//i, '').toUpperCase();
  }
  const dropped = Number(report.framesDropped) || 0;
  if (frames > 0 && dropped > 0) {
    cached.quality = `${Math.round((dropped / frames) * 100)}% dropped`;
  } else if (frames > 0 && !cached.quality) {
    cached.quality = 'steady';
  }
  stationDisplayTelemetry.set(key, cached);
  stationScheduleUpdate();
}

function stationDisplayRunwayPayload(controlsSummary = null) {
  const controls = controlsSummary || stationBuildControlsSummary();
  const selectedPeer = stationSelectedPeer();
  const selectedHostId = selectedPeer?.host_id || stationPeerDraft.selectedHostId || stationSelectedPeerHost || '';
  const selectedDisplayId = selectedHostId
    ? stationPeerDisplayIdForHost(selectedHostId)
    : stationPeerDisplayId();
  const peerCount = daemons.length;
  const connectedPeers = daemons.filter(peer => !!peer.connected).length;
  const displayPeers = daemons.filter(peer => peerCanShareDisplay(peer)).length;
  const lanes = [];
  if (controls.sessionId) {
    lanes.push({
      type: 'operator_target',
      id: `session:${controls.sessionId}`,
      title: controls.sessionLabel || shortSessionId(controls.sessionId) || 'Operator target',
      meta: `${controls.sessionSource || 'intendant'} / ${controls.sessionActive ? 'active' : (controls.sessionDetached ? 'detached' : 'idle')}`,
      detail: controls.sessionGoalObjective || controls.sessionCommand || controls.sessionSelection || '',
      session_id: controls.sessionId,
      live_id: controls.sessionLiveId || '',
      can_focus: !!controls.sessionCanFocus,
      can_interrupt: !!controls.sessionCanInterrupt,
    });
  }
  const shared = stationSharedViewStatusPayload();
  if (shared.visible) {
    lanes.push({
      type: 'shared_view',
      id: 'shared-view',
      title: shared.target || 'Shared view',
      meta: `${shared.action || 'view'}${shared.can_take_input ? ' / input available' : ''}`,
      detail: [shared.reason, shared.note].filter(Boolean).join(' / '),
      display_id: shared.display_id,
      can_take_input: !!shared.can_take_input,
    });
  }
  for (const slot of displaySlots.values()) {
    const telemetryKey = stationDisplayTelemetryKey('local', selfPeerId, slot.displayId);
    const telemetry = stationDisplayTelemetryFor(telemetryKey, slot.videoEl, slot.pc);
    lanes.push({
      type: 'local_stream',
      id: `local:${slot.displayId}`,
      title: `${selfHostLabel || 'local'} :${slot.displayId}`,
      meta: `${slot.connected ? 'connected' : 'connecting'} / input ${slot.authorityState || 'unknown'}`,
      detail: [
        stationDisplayTelemetryLabel(telemetry) || `${slot.width || 0}x${slot.height || 0}`,
        slot.interactive ? 'interactive' : 'view only',
        slot.recording ? 'recording' : '',
      ].filter(Boolean).join(' / '),
      host_id: selfPeerId || 'local',
      host_label: selfHostLabel || 'local',
      display_id: slot.displayId,
      lane_label: `local:${slot.displayId}`,
      resolution: telemetry.resolution,
      fps: telemetry.fps,
      codec: telemetry.codec,
      quality: telemetry.quality,
      telemetry_label: telemetry.label,
      input_authority: slot.authorityState || '',
      selected: userDisplayGranted && Number(grantedDisplayId) === Number(slot.displayId),
    });
  }
  for (const conn of peerDisplayConnections.values()) {
    const telemetryKey = stationDisplayTelemetryKey('remote', conn.hostId, conn.displayId, conn.sessionId);
    const container = stationPeerDisplayContainer(conn.hostId, false);
    const video = container && container.querySelector('.peer-display-video');
    const telemetry = stationDisplayTelemetryFor(telemetryKey, video, conn.pc);
    lanes.push({
      type: 'remote_stream',
      id: `remote:${conn.hostId}:${conn.displayId}:${conn.sessionId || ''}`,
      title: `${stationHostLabel(conn.hostId)} :${conn.displayId}`,
      meta: `${conn.displayStatusText || 'Connecting...'} / input ${conn.peerAuthorityState || 'unknown'}`,
      detail: [
        stationDisplayTelemetryLabel(telemetry),
        conn.sessionId || '',
        conn.advertiseTcpViaUrl ? `via ${conn.advertiseTcpViaUrl}` : '',
      ].filter(Boolean).join(' / '),
      host_id: conn.hostId || '',
      host_label: stationHostLabel(conn.hostId),
      display_id: conn.displayId,
      lane_label: `remote:${conn.hostId}:${conn.displayId}`,
      resolution: telemetry.resolution,
      fps: telemetry.fps,
      codec: telemetry.codec,
      quality: telemetry.quality,
      telemetry_label: telemetry.label,
      session_id: conn.sessionId || '',
      input_authority: conn.peerAuthorityState || '',
      selected: String(conn.hostId || '') === String(selectedHostId || '') && Number(conn.displayId) === Number(selectedDisplayId),
    });
  }
  const selectedStreamActive = lanes.some(lane =>
    lane.type === 'remote_stream'
    && String(lane.host_id || '') === String(selectedHostId || '')
    && Number(lane.display_id) === Number(selectedDisplayId)
  );
  if (selectedPeer && peerCanShareDisplay(selectedPeer) && !selectedStreamActive) {
    lanes.push({
      type: 'peer_target',
      id: `target:${selectedPeer.host_id}:${selectedDisplayId}`,
      title: `${stationPeerDisplayName(selectedPeer) || selectedPeer.host_id} :${selectedDisplayId}`,
      meta: selectedPeer.connected ? 'ready to open' : 'peer offline',
      detail: stationPeerCapabilityLabels(selectedPeer).join(', ') || 'selected peer display target',
      host_id: selectedPeer.host_id,
      host_label: stationPeerDisplayName(selectedPeer) || selectedPeer.host_id,
      display_id: selectedDisplayId,
      lane_label: `target:${selectedPeer.host_id}:${selectedDisplayId}`,
      selected: true,
    });
  }
  return {
    // No wall-clock fields here: this payload is embedded in the Station
    // snapshot, and the JSON dedupe that gates WASM update_snapshot calls
    // must see identical bytes when nothing actually changed.
    selected_peer_id: selectedHostId || '',
    selected_peer_label: stationPeerDisplayName(selectedPeer),
    selected_display_id: Number(selectedDisplayId) || 0,
    selected_peer_connected: !!selectedPeer?.connected,
    selected_peer_can_display: !!selectedPeer && peerCanShareDisplay(selectedPeer),
    peer_status: stationPeerStatus || '',
    peer_count: peerCount,
    connected_peers: connectedPeers,
    display_peers: displayPeers,
    operator_session_id: controls.sessionId || '',
    local_streams: displaySlots.size,
    remote_streams: peerDisplayConnections.size,
    shared_view_visible: !!shared.visible,
    lanes,
  };
}

function stationLocalDisplaySlot(displayId) {
  return displaySlots.get(Number(displayId));
}

function stationPeerDisplayConnection(hostId, displayId, sessionId = '') {
  for (const conn of peerDisplayConnections.values()) {
    if (String(conn.hostId || '') !== String(hostId || '')) continue;
    if (Number(conn.displayId) !== Number(displayId)) continue;
    if (sessionId && String(conn.sessionId || '') !== String(sessionId || '')) continue;
    return conn;
  }
  return null;
}

function stationHandleDisplayRunwayAction(action) {
  const laneId = String(action.lane_id || action.laneId || '').trim();
  const op = String(action.action || '').trim();
  if (!laneId || !op) return;
  const lane = stationDisplayRunwayPayload().lanes.find(item => String(item.id || '') === laneId);
  if (!lane) return;
  if (op === 'open' || op === 'focus') {
    if (lane.type === 'remote_stream' || lane.type === 'peer_target') {
      stationSetPeerTarget(lane.host_id, lane.display_id);
      stationOpenDisplay(lane.host_id, lane.display_id);
      return;
    }
    if (lane.type === 'local_stream') {
      stationForceRouteTo('displays', '');
      return;
    }
    if (lane.type === 'shared_view') {
      stationFocusSharedViewDisplay();
      return;
    }
    if (lane.type === 'operator_target' && lane.session_id) {
      focusSessionWindow(lane.live_id || lane.session_id);
      return;
    }
  }
  if (op === 'session' && lane.session_id) {
    stationOpenPanel('system:sessions', 'Display runway session selected');
    return;
  }
  if (op === 'copy') {
    const id = [
      lane.id || '',
      lane.host_id ? `host ${lane.host_id}` : '',
      lane.display_id !== undefined && lane.display_id !== null ? `display ${lane.display_id}` : '',
      lane.session_id ? `session ${lane.session_id}` : '',
    ].filter(Boolean).join(' / ');
    if (id) {
      copyTextToClipboard(id)
        .then(() => stationSetPeerStatus('Display lane ID copied', 'ok'))
        .catch(err => stationSetPeerStatus(`Display lane copy failed: ${err?.message || err}`, 'error'))
        .finally(() => stationScheduleUpdate());
    }
    return;
  }
  if (op === 'stop' && lane.type === 'operator_target') {
    stationHandleActivityAction({ action: 'stop' });
    return;
  }
  if (op === 'select' && lane.type === 'peer_target') {
    stationSetPeerDisplayTarget(lane.host_id, lane.display_id);
    stationRenderedSelectPanel('system:peers', 'Peer display target selected');
    return;
  }
  if (lane.type === 'shared_view') {
    if (op === 'input') {
      document.querySelector('[data-shared-view-take-input]')?.click();
      stationScheduleUpdate();
    } else if (op === 'hide') {
      document.querySelector('[data-shared-view-close]')?.click();
      stationScheduleUpdate();
    }
    return;
  }
  if (lane.type === 'local_stream') {
    const slot = stationLocalDisplaySlot(lane.display_id);
    if (!slot) return;
    if (op === 'input') {
      if ((slot.authorityState || '') === 'you') slot.releaseBtn?.click();
      else slot.takeBtn?.click();
    } else if (op === 'attach') {
      slot.attachBtn?.click();
    } else if (op === 'record') {
      slot.recordBtn?.click();
    } else if (op === 'fullscreen') {
      slot.fullscreenBtn?.click();
    }
    stationScheduleUpdate();
    return;
  }
  if (lane.type === 'remote_stream') {
    const conn = stationPeerDisplayConnection(lane.host_id, lane.display_id, lane.session_id);
    if (op === 'input') {
      if ((conn?.peerAuthorityState || '') === 'you') conn?.releaseControl?.();
      else conn?.takeControl?.();
    } else if (op === 'close') {
      closePeerDisplaysForHost(lane.host_id);
    }
    stationScheduleUpdate();
  }
}

function stationHandleHotspot(name) {
  const target = String(name || '');
  if (!target) return;
  if (target.startsWith('layout:')) {
    // Layout keeps the legacy path unconditionally: stationViewSet also
    // persists the draft to localStorage, which activate() would bypass.
    stationViewSet('layout', target.slice('layout:'.length));
    stationStatus(station?.debug_state?.() || 'Station online');
    return;
  }
  if (!target.startsWith('system:')) return;
  if (!stationActivateTarget(target)) station?.select_by_id?.(target);
  stationStatus(`Opening ${target.replace('system:', '')}`);
}

let stationLastHotspotDispatch = { target: '', at: 0 };

function stationDispatchHotspot(target, ev) {
  const name = String(target || '');
  if (!name) return;
  const keyboardActivation = ev && ev.detail === 0;
  if (station && !keyboardActivation) return;
  const at = performance.now();
  if (stationLastHotspotDispatch.target === name && at - stationLastHotspotDispatch.at < 220) {
    ev?.preventDefault?.();
    ev?.stopPropagation?.();
    return;
  }
  stationLastHotspotDispatch = { target: name, at };
  ev?.preventDefault?.();
  ev?.stopPropagation?.();
  stationHandleHotspot(name);
}

function stationHotspotLayerEvent(ev) {
  const button = ev.target?.closest?.('[data-station-hotspot]');
  if (button) {
    stationDispatchHotspot(button.dataset.stationHotspot, ev);
    return;
  }
  const target = stationHotspotAt(ev.clientX, ev.clientY);
  if (!target) return;
  stationDispatchHotspot(target, ev);
}

document.getElementById('station-hotspots')?.addEventListener('click', stationHotspotLayerEvent);
document.getElementById('station-hud-canvas')?.addEventListener('click', stationHotspotLayerEvent);
window.addEventListener('resize', stationSyncHotspots);
if ('ResizeObserver' in window) {
  const stationHotspotResizeObserver = new ResizeObserver(() => {
    station?.resize?.();
    stationSyncHotspots();
  });
  const stationPaneForHotspots = document.querySelector('.station-pane');
  if (stationPaneForHotspots) stationHotspotResizeObserver.observe(stationPaneForHotspots);
}

function stationActivityLevelFor(ev) {
  return String(ev?.level || 'info').trim().toLowerCase() || 'info';
}

function stationActivitySourceFor(ev) {
  return compactSessionText(ev?.source || ev?.event || ev?.level || 'info') || 'info';
}

function stationActivityEventKey(ev) {
  return String(ev?.id || '').trim() ||
    `${ev?.hostId || ev?.host_id || selfPeerId || 'local'}:${ev?.ts || ''}:${ev?.msg || ''}`;
}

function stationActivityCountOptions(events, accessor, allLabel) {
  const counts = new Map();
  for (const ev of events || []) {
    const value = String(accessor(ev) || '').trim();
    if (!value) continue;
    counts.set(value, (counts.get(value) || 0) + 1);
  }
  return [[ '', allLabel ]].concat(
    [...counts.entries()]
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .slice(0, 18)
      .map(([value, count]) => [value, `${value} (${count})`])
  );
}

function stationActivityTopSummary(events, accessor) {
  return stationActivityCountOptions(events, accessor, '')
    .slice(1, 6)
    .map(([, label]) => label)
    .join(' / ') || '--';
}

function stationActivityLogEntryForEventId(eventId) {
  const id = String(eventId || '').trim();
  if (!id) return null;
  const escaped = stationCssEscape(id);
  return document.querySelector(`.log-entry[data-station-event-id="${escaped}"]`) ||
    document.querySelector(`.log-entry[data-item-id="${escaped}"]`);
}

function stationEditActivityEvent(eventId) {
  const entry = stationActivityLogEntryForEventId(eventId);
  const editButton = entry?.querySelector(':scope > .log-edit-message');
  if (!editButton) {
    showControlToast?.('error', 'This Station activity row is not editable');
    return;
  }
  focusActivityLogEvent(eventId);
  editButton.click();
}

function stationActivityEventCopyText(ev) {
  const entry = stationActivityLogEntryForEventId(ev?.id || '');
  if (entry && typeof getLogEntryCopyText === 'function') {
    const text = getLogEntryCopyText(entry);
    if (text) return text;
  }
  return [
    ev?.ts || '',
    ev?.level || 'info',
    ev?.hostId || ev?.host_id || selfPeerId || 'local',
    ev?.msg || '',
  ].filter(Boolean).join('  ');
}

function stationActivityVisibleText(events) {
  return (events || [])
    .map(stationActivityEventCopyText)
    .filter(Boolean)
    .join('\n');
}

async function stationCopyActivityEvents(events) {
  const text = stationActivityVisibleText(events);
  if (!text) {
    showControlToast?.('error', 'No Station activity rows to copy');
    return;
  }
  await copyTextToClipboard(text);
  showControlToast?.('success', 'Copied Station activity rows');
}

function stationClearActivityLog() {
  clearLogs();
  stationClearLogState();
  stationScheduleUpdate();
  showControlToast?.('success', 'Activity log cleared');
}

function stationActivityFilteredEvents(allEventsInput = null) {
  const allEvents = Array.isArray(allEventsInput) ? allEventsInput : stationActivityEvents();
  const hostFilter = String(stationActivityDockHost || activeHostFilter || '').trim();
  const levelFilter = String(stationActivityDockLevel || '').trim();
  const sourceFilter = String(stationActivityDockSource || '').trim();
  const query = stationActivityDockFilter.trim().toLowerCase();
  const events = allEvents.filter(ev => {
    if (hostFilter && String(ev.hostId || ev.host_id || '') !== hostFilter) return false;
    if (levelFilter && stationActivityLevelFor(ev) !== levelFilter) return false;
    if (sourceFilter && stationActivitySourceFor(ev) !== sourceFilter) return false;
    if (!query) return true;
    return [ev.id, ev.hostId, ev.host_id, ev.sessionId, ev.source, ev.level, ev.ts, ev.msg]
      .join(' ')
      .toLowerCase()
      .includes(query);
  });
  return { allEvents, events, hostFilter, levelFilter, sourceFilter, query };
}

function stationClearActivityTriage() {
  stationActivityDockFilter = '';
  stationActivityDockLevel = '';
  stationActivityDockSource = '';
  stationScheduleUpdate();
}

function stationActivityEventIsManaged(ev) {
  return /\b(managed|context|rewind|backout|anchor|lineage|fission)\b/i
    .test([ev?.source, ev?.msg, ev?.level].filter(Boolean).join(' '));
}

function stationOpenManagedFromActivity(ev) {
  const sessionId = String(ev?.sessionId || ev?.session_id || '').trim();
  if (sessionId) stationSetManagedSession(sessionId);
  if (ev) {
    stationManagedActivitySignal = {
      id: stationActivityEventKey(ev),
      sessionId,
      action: 'activity-managed',
      label: stationActivitySourceFor(ev) || 'activity',
      value: `${stationActivityLevelFor(ev)} ${ev.ts || ''}`.trim(),
      detail: compactSessionText(stationActivityEventCopyText(ev)),
      tone: 'managed',
    };
  }
  stationOpenPanel('system:managed', 'Managed activity selected');
}

async function stationCopyActivityEventJson(ev) {
  if (!ev) return;
  const payload = {
    ...ev,
    id: stationActivityEventKey(ev),
    source: stationActivitySourceFor(ev),
    level: stationActivityLevelFor(ev),
    copy_text: stationActivityEventCopyText(ev),
  };
  await copyTextToClipboard(JSON.stringify(payload, null, 2));
  showControlToast?.('success', 'Copied Station activity event JSON');
}

function stationApplyContextAction(op, id = '') {
  const actionButtonByOp = {
    live: 'context-live-btn',
    replay: 'context-replay-btn',
    reset: 'context-reset-view-btn',
    focus: 'context-open-focus-btn',
    raw: 'context-raw-toggle-btn',
  };
  if (typeof renderContextPane === 'function') renderContextPane();
  if (op === 'part') {
    stationSelectedContextPart = id;
    if (id && typeof focusContextPart === 'function') focusContextPart(id);
    return;
  }
  const buttonId = actionButtonByOp[op];
  if (buttonId) document.getElementById(buttonId)?.click();
}

function stationContextTimelineState() {
  const fallback = { key: '', timeline: [] };
  const state = typeof contextTimelineForForegroundSession === 'function'
    ? contextTimelineForForegroundSession()
    : fallback;
  const key = state?.key || '';
  const timeline = Array.isArray(state?.timeline) ? state.timeline : [];
  const snapshot = stationSnapshotForContext();
  const max = Math.max(0, timeline.length - 1);
  let index = timeline.indexOf(snapshot);
  if (index < 0 && timeline.length) index = contextReplayIndexBySession.get(key) ?? max;
  index = Math.max(0, Math.min(max, Number(index) || 0));
  return { key, timeline, snapshot, index, max };
}

function stationSetContextReplayIndex(index) {
  const { key, timeline, max } = stationContextTimelineState();
  if (!timeline.length) return;
  const next = Math.max(0, Math.min(max, Number(index) || 0));
  contextReplayMode = 'replay';
  contextReplayIndexBySession.set(key, next);
  contextSelectedPartId = null;
  stationSelectedContextPart = '';
  if (typeof renderContextPane === 'function') renderContextPane();
}

function stationContextExactStatus(snapshot) {
  if (!snapshot) return 'none';
  if (typeof contextSnapshotNeedsExact === 'function' && contextSnapshotNeedsExact(snapshot)) {
    if (typeof contextSnapshotExactFetchFailed === 'function' && contextSnapshotExactFetchFailed(snapshot)) {
      return 'exact failed';
    }
    return 'compact; exact available';
  }
  if (snapshot.__exact_loaded || (typeof contextSnapshotHasExactRaw === 'function' && contextSnapshotHasExactRaw(snapshot))) {
    return 'exact loaded';
  }
  return 'compact';
}

function stationContextSnapshotText(snapshot) {
  if (!snapshot) return '';
  return contextFullText(snapshot);
}

function stationContextPartText(part) {
  if (!part) return '';
  return contextFullText(part.value ?? part.preview ?? part.text ?? part.title ?? '');
}

function stationCopyContextSnapshot(snapshot = stationSnapshotForContext()) {
  const text = stationContextSnapshotText(snapshot);
  if (!text) {
    showControlToast?.('error', 'No context snapshot to copy');
    return;
  }
  copyTextToClipboard(text)
    .then(() => showControlToast?.('success', 'Copied context snapshot'))
    .catch(err => showControlToast?.('error', `Copy context snapshot failed: ${err?.message || err}`));
}

function stationCopyContextPart(part) {
  const text = stationContextPartText(part);
  if (!text) {
    showControlToast?.('error', 'No context item text to copy');
    return;
  }
  copyTextToClipboard(text)
    .then(() => showControlToast?.('success', 'Copied context item'))
    .catch(err => showControlToast?.('error', `Copy context item failed: ${err?.message || err}`));
}

function stationContextLaneParts(category) {
  const snapshot = stationSnapshotForContext();
  if (!snapshot) return [];
  const normalized = typeof contextNormalizeCategory === 'function'
    ? contextNormalizeCategory(category)
    : String(category || 'other').trim();
  const analysis = analyzeContextSnapshotCached(snapshot);
  return analysis.parts.filter(part => part.category === normalized);
}

function stationCopyContextLane(category) {
  const text = stationContextLaneParts(category)
    .map(part => stationContextPartText(part))
    .filter(Boolean)
    .join('\n\n');
  if (!text) return;
  copyTextToClipboard(text);
}

async function stationLoadExactContextSnapshot(partId = stationSelectedContextPart) {
  const snapshot = stationSnapshotForContext();
  if (!snapshot || typeof ensureExactContextSnapshot !== 'function') return;
  await ensureExactContextSnapshot(snapshot);
  stationScheduleUpdate();
}

function stationSelectedContextPartDetail(partId = stationSelectedContextPart) {
  const id = String(partId || '').trim();
  const snapshot = stationSnapshotForContext();
  if (!id || !snapshot) return null;
  const analysis = analyzeContextSnapshotCached(snapshot);
  return analysis.parts.find(part => part.id === id) || null;
}

function stationContextContinuitySession(ctx, managed) {
  const candidates = [
    ctx?.sessionId,
    managed?.sessionId,
    resolvePromptTargetSessionId?.(),
    currentSessionFullId,
    foregroundSessionFullId,
  ].map(id => String(id || '').trim()).filter(Boolean);
  const sessionId = candidates.find(id => id && id !== CONTEXT_GLOBAL_SESSION) || '';
  const session = sessionId ? stationFindSessionById(sessionId) : null;
  const meta = sessionConfigMetadata(session || sessionId);
  const source = sessionConfigSource(meta) || stationSessionSource(session) || '';
  return {
    sessionId,
    session,
    label: stationSessionShortLabel(sessionId) || stationSessionTask(session) || shortSessionId(sessionId) || '',
    source: source || session?.source || '',
    canFocus: !!sessionId && sessionWindows.has(sessionId),
  };
}

function stationContextContinuityPayload(ctx) {
  const managed = stationBuildManagedSummary(ctx);
  const session = stationContextContinuitySession(ctx, managed);
  const topLanes = (ctx.topCategories || []).slice(0, 4).map(item => ({
    label: item.label || item.category || '',
    tokens: item.value || 0,
    count: item.count || 0,
    part_id: item.partId || '',
  }));
  const topItems = (ctx.topItems || []).slice(0, 4).map(item => ({
    id: item.id || '',
    label: item.label || '',
    value: item.value || '',
    detail: item.detail || '',
  }));
  return {
    generated_at: new Date().toISOString(),
    context: {
      available: !!ctx.available,
      source: ctx.source || '',
      session_id: ctx.sessionId || '',
      turn: ctx.turn || '',
      format: ctx.format || '',
      used_tokens: ctx.tokens || 0,
      effective_window: ctx.effectiveWindow || 0,
      hard_window: ctx.hardWindow || 0,
      item_count: ctx.itemCount || 0,
      category_count: ctx.categoryCount || 0,
      top_lanes: topLanes,
      top_items: topItems,
    },
    managed: {
      session_id: managed.sessionId || '',
      mode: managed.mode || '',
      status: managed.status || '',
      readiness: stationOperationsManagedReadiness(managed),
      rewind_only: !!managed.rewindOnly,
      anchors: managed.anchors || 0,
      records: managed.records || 0,
      remaining_to_rewind_only: managed.remainingToRewindOnly,
    },
    session: {
      id: session.sessionId || '',
      label: session.label || '',
      source: session.source || '',
      can_focus: !!session.canFocus,
    },
  };
}

async function stationCopyContextContinuityPayload(ctx) {
  await copyTextToClipboard(JSON.stringify(stationContextContinuityPayload(ctx), null, 2));
  showControlToast?.('success', 'Context continuity JSON copied');
}

function stationSetManagedSession(sessionId) {
  if (!sessionId) return;
  if (typeof renderManagedContextSessionSelect === 'function') renderManagedContextSessionSelect();
  const sel = managedContextEl?.('managed-context-session');
  if (sel && Array.from(sel.options).some(option => option.value === sessionId)) {
    const previous = managedContextCurrentSessionId?.() || '';
    sel.value = sessionId;
    managedContextSessionManuallySelected = true;
    if (typeof syncManagedContextActiveSession === 'function') syncManagedContextActiveSession();
    if (previous && previous !== sessionId) stationResetManagedDraft();
  }
}

function stationResetManagedDraft() {
  stationManagedDraft = {
    anchor: '',
    position: '',
    reason: '',
    primer: '',
    preserve: '',
    discard: '',
    artifacts: '',
    nextSteps: '',
    record: '',
    backoutMode: '',
    backoutName: '',
  };
}

function stationManagedDraftValue(key, legacyId = '', fallback = '') {
  const draftValue = stationManagedDraft[key];
  if (draftValue !== undefined && draftValue !== null && draftValue !== '') return draftValue;
  const legacyValue = legacyId ? String(managedContextEl?.(legacyId)?.value || '') : '';
  if (legacyValue) return legacyValue;
  return fallback || '';
}

function stationRememberManagedDraftValue(key, value) {
  stationManagedDraft[key] = String(value || '');
}

function stationCopyManagedFormToLegacy() {
  const copyValue = (key, to) => {
    const value = String(stationManagedDraft[key] || '');
    const target = managedContextEl?.(to);
    if (target && value) target.value = value;
  };
  copyValue('anchor', 'managed-context-anchor');
  copyValue('position', 'managed-context-position');
  copyValue('reason', 'managed-context-reason');
  copyValue('primer', 'managed-context-primer');
  copyValue('preserve', 'managed-context-preserve');
  copyValue('discard', 'managed-context-discard');
  copyValue('artifacts', 'managed-context-artifacts');
  copyValue('nextSteps', 'managed-context-next-steps');
  copyValue('record', 'managed-context-record-id');
  copyValue('backoutMode', 'managed-context-backout-mode');
  copyValue('backoutName', 'managed-context-backout-name');
}

function stationManagedResultText() {
  return stationManagedActionResult || managedContextEl?.('managed-context-action-result')?.textContent || '';
}

function stationManagedStatusExportPayload(managed, contextSummary) {
  return {
    generated_at: new Date().toISOString(),
    session_id: managed.sessionId || '',
    mode: managed.mode || '',
    status: managed.status || '',
    live: !!managed.live,
    context: {
      source: contextSummary?.source || '',
      used_tokens: managed.usedTokens || 0,
      effective_window: managed.effectiveWindow || 0,
      hard_window: managed.hardWindow || 0,
      effective_pct: managed.effectivePct,
      hard_pct: managed.hardPct,
      rewind_only_limit: managed.rewindOnlyLimit,
      remaining_to_rewind_only: managed.remainingToRewindOnly,
      rewind_only: !!managed.rewindOnly,
    },
    catalog: {
      anchors: managed.anchors || 0,
      records: managed.records || 0,
      lineage_groups: managed.lineageGroups || 0,
      fission_groups: managed.fissionGroups || 0,
      branches: managed.branches || 0,
    },
    recent: {
      // Anchor/branch row arrays no longer ride on the summary (snapshot
      // diet); raw_status below still carries the full ledgers.
      records: managed.recentRecords || [],
    },
    raw_status: managedContextStatus || {},
  };
}

async function stationCopyManagedStatus(managed, contextSummary) {
  const payload = stationManagedStatusExportPayload(managed, contextSummary);
  await copyTextToClipboard(JSON.stringify(payload, null, 2));
  showControlToast?.('success', 'Managed status JSON copied');
}

function stationSetManagedResult(text) {
  stationManagedActionResult = text || 'No action yet';
  const result = managedContextEl?.('managed-context-action-result');
  if (result) result.textContent = stationManagedActionResult;
}

function stationManagedActionReadyText(managed) {
  if (!managed?.sessionId) return 'Select a session before running managed-context actions.';
  if (String(managed.mode || '').toLowerCase() !== 'managed') return 'This session is in vanilla context mode; managed actions are read-only here.';
  return '';
}

function stationManagedFormValue(key, legacyId) {
  return stationManagedDraftValue(key, legacyId).trim();
}

function stationManagedReadiness(managed) {
  const base = stationManagedActionReadyText(managed);
  const anchor = stationManagedFormValue('anchor', 'managed-context-anchor');
  const reason = stationManagedFormValue('reason', 'managed-context-reason');
  const primer = stationManagedFormValue('primer', 'managed-context-primer');
  const record = stationManagedFormValue('record', 'managed-context-record-id');
  const rewindMissing = [];
  if (!anchor) rewindMissing.push('anchor');
  if (!reason) rewindMissing.push('reason');
  if (!primer) rewindMissing.push('primer');
  return {
    base,
    anchor,
    reason,
    primer,
    record,
    canInspect: !base && !!anchor,
    canRewind: !base && rewindMissing.length === 0,
    canBackout: !base && !!record,
    inspectTitle: base || (anchor ? '' : 'Select an anchor first'),
    rewindTitle: base || (rewindMissing.length ? `Missing ${rewindMissing.join(', ')}` : ''),
    backoutTitle: base || (record ? '' : 'Select a rewind record first'),
  };
}

function stationSetManagedDraftValueIfEmpty(key, value) {
  if (String(stationManagedDraft[key] || '').trim()) return;
  stationRememberManagedDraftValue(key, value || '');
}

function stationSeedManagedDraftFromContext() {
  const context = stationBuildContextSummary();
  const managed = stationBuildManagedSummary(context);
  const lines = [
    `Session: ${managed.sessionId || context.sessionId || 'unknown'}`,
    `Context: ${stationCompactNumber(context.tokens)} used of ${stationCompactNumber(context.effectiveWindow)} effective tokens (${managed.status || 'unknown'}).`,
    context.topCategories?.length
      ? `Top lanes: ${context.topCategories.map(item => `${item.label} ${stationCompactNumber(item.value)}`).join(', ')}.`
      : '',
    context.topItems?.length
      ? `Largest items: ${context.topItems.map(item => [item.label, item.value, item.detail].filter(Boolean).join(' ')).join('; ')}.`
      : '',
  ].filter(Boolean);
  stationSetManagedDraftValueIfEmpty('reason', managed.rewindOnly ? 'Context is in rewind-only pressure.' : 'Crystallize current Station context before continuing.');
  stationSetManagedDraftValueIfEmpty('primer', lines.join('\n'));
  stationSetManagedDraftValueIfEmpty('preserve', [
    `Current Station context source: ${context.source || 'unknown'}`,
    `Managed mode/status: ${managed.mode || 'unknown'} / ${managed.status || 'unknown'}`,
  ].join('\n'));
  stationSetManagedDraftValueIfEmpty('nextSteps', 'Continue from the preserved Station context summary.');
  stationCopyManagedFormToLegacy();
  stationScheduleUpdate();
  showControlToast?.('success', 'Station managed-context draft seeded from current context');
}

function stationManagedDraftLines(key) {
  return String(stationManagedDraftValue(key) || '')
    .split('\n')
    .map(line => line.trim())
    .filter(Boolean);
}

function stationManagedCurrentActionSession() {
  return stationManagedSessionCandidate();
}

async function stationInspectManagedAnchor() {
  const sessionId = stationManagedCurrentActionSession();
  const itemId = stationManagedFormValue('anchor', 'managed-context-anchor');
  if (!sessionId || !itemId) {
    showControlToast?.('error', 'Inspect needs session and anchor');
    return;
  }
  stationCopyManagedFormToLegacy();
  stationSetManagedResult('Inspecting anchor...');
  try {
    const text = await managedContextMcpToolForSession(sessionId, 'inspect_rewind_anchor', {
      session_id: sessionId,
      item_id: itemId,
      radius: 2,
    });
    stationSetManagedResult(text || 'ok');
  } catch (err) {
    stationSetManagedResult(err?.message || String(err));
    showControlToast?.('error', err?.message || 'Anchor inspect failed');
  }
}

async function stationSubmitManagedRewind() {
  const sessionId = stationManagedCurrentActionSession();
  const itemId = stationManagedFormValue('anchor', 'managed-context-anchor');
  const reason = stationManagedFormValue('reason', 'managed-context-reason');
  const primer = stationManagedFormValue('primer', 'managed-context-primer');
  if (!sessionId || !itemId || !reason || !primer) {
    showControlToast?.('error', 'Rewind needs session, anchor, reason, and primer');
    return;
  }
  stationCopyManagedFormToLegacy();
  stationSetManagedResult('Dispatching rewind...');
  try {
    const text = await managedContextMcpToolForSession(sessionId, 'rewind_context', {
      session_id: sessionId,
      anchor: {
        item_id: itemId,
        position: stationManagedDraftValue('position', 'managed-context-position', 'after') === 'before' ? 'before' : 'after',
      },
      reason,
      primer,
      preserve: stationManagedDraftLines('preserve'),
      discard: stationManagedDraftLines('discard'),
      artifacts: stationManagedDraftLines('artifacts'),
      next_steps: stationManagedDraftLines('nextSteps'),
    });
    stationSetManagedResult(text || 'ok');
    showControlToast?.('success', 'Managed rewind dispatched');
    scheduleManagedContextRefresh(1000);
  } catch (err) {
    stationSetManagedResult(err?.message || String(err));
    showControlToast?.('error', err?.message || 'Managed rewind failed');
  }
}

async function stationSubmitManagedBackout(modeOverride = '') {
  const sessionId = stationManagedCurrentActionSession();
  const recordId = stationManagedFormValue('record', 'managed-context-record-id');
  if (!sessionId || !recordId) {
    showControlToast?.('error', 'Backout needs session and record id');
    return;
  }
  stationCopyManagedFormToLegacy();
  const mode = modeOverride || stationManagedDraftValue('backoutMode', 'managed-context-backout-mode', 'inspect') || 'inspect';
  const name = stationManagedDraftValue('backoutName', 'managed-context-backout-name');
  stationSetManagedResult(`Running ${mode}...`);
  try {
    const args = { session_id: sessionId, record_id: recordId, mode };
    if (name) args.name = name;
    const text = await managedContextMcpToolForSession(sessionId, 'rewind_backout', args);
    stationSetManagedResult(text || 'ok');
    showControlToast?.('success', `Managed ${mode} complete`);
    scheduleManagedContextRefresh(1000);
  } catch (err) {
    stationSetManagedResult(err?.message || String(err));
    showControlToast?.('error', err?.message || 'Managed backout failed');
  }
}

async function stationApplyManagedAction(op, id = '', sessionId = '') {
  if (sessionId) stationSetManagedSession(sessionId);
  if (op === 'refresh') {
    await refreshManagedContextPane({ force: true });
    return;
  }
  if (op === 'anchor') {
    stationRememberManagedDraftValue('anchor', id);
    fillManagedContextAnchor(id, sessionId);
    return;
  }
  if (op === 'record') {
    stationRememberManagedDraftValue('record', id);
    const record = (managedContextRecords || []).find(candidate => candidate && candidate.record_id === id);
    if (record) selectManagedContextRecord(record);
    else {
      const input = managedContextEl?.('managed-context-record-id');
      if (input) input.value = id;
    }
    return;
  }
  if (op === 'branch') {
    const [groupId, branchId, expectedId] = id.split('\u001f');
    if (groupId && branchId && typeof claimManagedContextFissionCanonical === 'function') {
      await claimManagedContextFissionCanonical(groupId, branchId, expectedId || '');
    }
  }
}

