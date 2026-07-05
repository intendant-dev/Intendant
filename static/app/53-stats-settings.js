// ── Activity host filter ──
//
// Dropdown at the top of the Activity tab lets the user restrict the
// log stream to a single host. Options are rebuilt whenever the
// daemon list changes (renderDaemonsList → refreshHostFilterOptions).
// Filter application is a DOM sweep that toggles a hiding class on
// each log-entry, so past entries respect the new filter too.

function refreshHostFilterOptions() {
  const sel = document.getElementById('host-filter-select');
  if (!sel) return;

  // Hide the dropdown (and its label) entirely when there are no
  // secondaries — nothing to filter. Matches the host-badge show/hide
  // behavior. We don't clear `activeHostFilter` here because the user
  // may still have a previously-configured filter in localStorage that
  // they want back as soon as they re-add that secondary.
  sel.classList.toggle('hidden', daemons.length === 0);
  document.getElementById('host-filter-label')?.classList.toggle('hidden', daemons.length === 0);
  if (daemons.length === 0) {
    return;
  }

  const seen = new Set([selfPeerId, ...daemons.map(d => d.host_id)]);
  sel.innerHTML = '';
  const all = document.createElement('option');
  all.value = '';
  all.textContent = 'All hosts';
  sel.appendChild(all);
  for (const hostId of seen) {
    const opt = document.createElement('option');
    opt.value = hostId;
    opt.textContent = hostId;
    sel.appendChild(opt);
  }
  // Restore from the persisted `activeHostFilter` if the host still
  // exists in the set. If not (e.g. user removed that daemon), leave
  // the filter selection visible but set the DOM back to "All hosts"
  // — we keep the localStorage value as a sticky preference though,
  // so re-adding the same daemon resurfaces the filter.
  if (activeHostFilter && seen.has(activeHostFilter)) {
    sel.value = activeHostFilter;
    applyHostFilter();
  } else {
    sel.value = '';
  }
}

function applyHostFilter() {
  for (const stream of mainLogContainers()) {
    for (const entry of stream.querySelectorAll('.log-entry')) {
      const hostId = entry.dataset.hostId || selfPeerId;
      entry.classList.toggle(
        'hidden-by-filter',
        !!activeHostFilter && activeHostFilter !== hostId,
      );
    }
  }
}

// ── Stats host picker ──
//
// Dropdown at the top of the Stats tab switches between the primary's
// usage/cost/history view and each configured peer's. Payloads are
// captured from `update_usage` commands (primary) and `peer_usage`
// commands translated through `peerSnapshotToUpdateUsage` (peers)
// as they flow in via processCommands, so switching is instant from
// cache.

function isStatsShowingSelf() {
  return !activeStatsHost || activeStatsHost === selfPeerId;
}

function refreshStatsHostPicker() {
  const bar = document.getElementById('stats-host-bar');
  const sel = document.getElementById('stats-host-select');
  if (!bar || !sel) return;

  // Hide the picker on single-host setups — nothing to pick between.
  if (daemons.length === 0) {
    bar.style.display = 'none';
    if (activeStatsHost) {
      activeStatsHost = '';
      renderStatsForActiveHost();
    }
    return;
  }
  bar.style.display = 'flex';

  const current = sel.value;
  sel.innerHTML = '';
  const addOpt = (value, text) => {
    const opt = document.createElement('option');
    opt.value = value;
    opt.textContent = text;
    sel.appendChild(opt);
  };
  addOpt('', `${selfHostLabel} (self)`);
  for (const d of daemons) {
    addOpt(d.host_id, d.label || d.host_id);
  }

  // Preserve the previous selection if it's still present.
  const keepSelection = Array.from(sel.options).some(o => o.value === current);
  sel.value = keepSelection ? current : '';
  if (!keepSelection && activeStatsHost) {
    activeStatsHost = '';
    renderStatsForActiveHost();
  }
}

function switchStatsHost(hostId) {
  activeStatsHost = hostId || '';
  renderStatsForActiveHost();
}

// Render the Stats tab from whichever host is currently active,
// pulling from hostStatsCache for the live usage cards and from the
// per-host session list cache for the "All Sessions" + "Disk Usage"
// cards. Display metrics stay primary-only for now — they come from
// a display pipeline event stream, not an HTTP fetch, and the
// secondary WS forwards them but we don't have per-host DOM targets.
function renderStatsForActiveHost(options = {}) {
  const key = activeStatsHost || selfPeerId;
  const cached = hostStatsCache.get(key);
  if (cached) {
    renderUsageTab(cached);
  } else {
    // No cached payload yet — show the empty state. renderUsageTab's
    // "no main_json" branch does exactly this, so pass an empty command.
    renderUsageTab({ main_json: null });
  }

  // All Sessions + Disk Usage: fetch from the selected host's
  // /api/sessions. The loader caches per host so switching back is
  // instant, and guards against stale fetches winning a race against
  // the user switching away.
  renderCachedStatsSessionSections(key);
  loadAllSessionsUsage(key, { force: !!options.forceSessions });

  // Display metrics: primary-only for now. Hide when viewing a
  // secondary; restore when viewing self.
  const metricsEl = document.getElementById('display-metrics-container');
  if (metricsEl) {
    metricsEl.classList.toggle('hidden-by-secondary-host', !isStatsShowingSelf());
  }
}

// ── Settings ──
// settingsLoaded lives with the minimal JS state block (deep-link TDZ).

async function loadSettings() {
  try {
    const d = await fetchDashboardSettings();
    if (d.error) { console.warn('Settings load error:', d.error); return; }
    document.getElementById('set-cu-provider').value = d.cu_provider || '';
    document.getElementById('set-cu-model').value = d.cu_model || '';
    document.getElementById('set-cu-backend').value = d.cu_backend || 'auto';
    // Separate CU provider/model selection only means something while the
    // vaulted CU-first routing is enabled (file-only experimental flag).
    document.getElementById('cu-routing-rows').style.display = d.cu_first_routing ? '' : 'none';
    document.getElementById('set-presence-enabled').checked = d.presence_enabled;
    document.getElementById('set-presence-provider').value = d.presence_provider || '';
    document.getElementById('set-presence-model').value = d.presence_model || '';
    document.getElementById('set-presence-live-provider').value = d.presence_live_provider || '';
    document.getElementById('set-presence-live-model').value = d.presence_live_model || '';
    document.getElementById('set-transcription-enabled').checked = d.transcription_enabled;
    document.getElementById('set-transcription-provider').value = d.transcription_provider || '';
    document.getElementById('set-transcription-model').value = d.transcription_model || '';
    document.getElementById('set-transcription-endpoint').value = d.transcription_endpoint || '';
    document.getElementById('set-transcription-language').value = d.transcription_language || '';
    document.getElementById('set-recording-enabled').checked = d.recording_enabled;
    document.getElementById('set-recording-framerate').value = d.recording_framerate || 15;
    updateFramerateWarning();
    document.getElementById('set-recording-quality').value = d.recording_quality || 'medium';
    document.getElementById('set-live-audio-enabled').checked = d.live_audio_enabled;
    document.getElementById('set-live-audio-timeout').value = d.live_audio_timeout_secs || 300;
    document.getElementById('set-codex-command').value = d.codex_command || 'codex';
    document.getElementById('set-codex-managed-command').value = d.codex_managed_command || '';
    document.getElementById('set-claude-command').value = d.claude_command || 'claude';
    document.getElementById('set-codex-service-tier').value = normalizeCodexServiceTier(d.codex_service_tier || '');
    controlCodexConfig.command = d.codex_command || 'codex';
    controlCodexConfig.service_tier = normalizeCodexServiceTier(d.codex_service_tier || '');
    controlCodexConfig.managed_context = d.codex_managed_context || 'vanilla';
    controlCodexConfig.context_archive = d.codex_context_archive || 'summary';
    setNewSessionAgentDefaults(d);
    // External agent: persisted to intendant.toml via `[agent]
    // default_backend`. Sync the Settings dropdown and status bar
    // worker identity here — on a fresh daemon boot there are no
    // ExternalAgentChanged events yet, so updateStatusBar has never
    // been called with an external_agent field.
    //
    // Route through normalizeAgentId so legacy TOML files written in
    // alternate display or serde-enum forms still populate the dropdown.
    {
      const shortId = normalizeAgentId(d.external_agent);
      currentExternalAgent = shortId;
      document.getElementById('set-external-agent').value = shortId;
      newSessionConfiguredAgent = shortId;
      renderNewSessionAgentControls();
      applyMainBackendStatus();
    }
    const envDiv = document.getElementById('settings-env-list');
    const envWrap = document.getElementById('settings-env-overrides');
    const debugEmpty = document.getElementById('debug-empty');
    if (d.env_overrides && Object.keys(d.env_overrides).length > 0) {
      envDiv.innerHTML = Object.entries(d.env_overrides)
        .map(([k, v]) => '<div><code style="color:var(--peach)">' + k + '</code> = <code>' + v + '</code></div>')
        .join('');
      envWrap.classList.remove('hidden');
      if (debugEmpty) debugEmpty.classList.add('hidden');
    } else {
      envWrap.classList.add('hidden');
      if (debugEmpty) debugEmpty.classList.remove('hidden');
    }
    settingsLoaded = true;
  } catch (e) {
    console.error('Failed to load settings:', e);
  }
}

async function saveSettings() {
  const g = id => document.getElementById(id);
  const selectedCodexServiceTier = normalizeCodexServiceTier(g('set-codex-service-tier')?.value ?? controlCodexConfig.service_tier ?? '');
  const selectedClaudeCommand = (g('set-claude-command')?.value || '').trim() || 'claude';
  controlCodexConfig.service_tier = selectedCodexServiceTier;
  newSessionCodexDefaultServiceTier = selectedCodexServiceTier;
  newSessionAgentCommands['claude-code'] = selectedClaudeCommand;
  if (!newSessionCodexFastModeTouched) {
    newSessionCodexFastMode = codexServiceTierIsFast(selectedCodexServiceTier);
  }
  const payload = {
    cu_provider: g('set-cu-provider').value || null,
    cu_model: g('set-cu-model').value || null,
    cu_backend: g('set-cu-backend').value,
    presence_enabled: g('set-presence-enabled').checked,
    presence_provider: g('set-presence-provider').value || null,
    presence_model: g('set-presence-model').value || null,
    presence_live_provider: g('set-presence-live-provider').value || null,
    presence_live_model: g('set-presence-live-model').value || null,
    transcription_enabled: g('set-transcription-enabled').checked,
    transcription_provider: g('set-transcription-provider').value || 'openai',
    transcription_model: g('set-transcription-model').value || 'whisper-1',
    transcription_endpoint: g('set-transcription-endpoint').value || null,
    transcription_language: g('set-transcription-language').value || null,
    recording_enabled: g('set-recording-enabled').checked,
    recording_framerate: parseInt(g('set-recording-framerate').value) || 15,
    recording_quality: g('set-recording-quality').value,
    live_audio_enabled: g('set-live-audio-enabled').checked,
    live_audio_timeout_secs: parseInt(g('set-live-audio-timeout').value) || 300,
    // Persisted to intendant.toml so it survives daemon restart.
    external_agent: g('set-external-agent').value || null,
    codex_command: (g('set-codex-command').value || '').trim() || 'codex',
    // Empty string is meaningful: it clears the managed-fork override.
    codex_managed_command: (g('set-codex-managed-command').value || '').trim(),
    claude_command: selectedClaudeCommand,
    codex_sandbox: controlCodexConfig.sandbox || 'workspace-write',
    codex_approval_policy: controlCodexConfig.approval_policy || 'on-request',
    codex_model: controlCodexConfig.model || null,
    codex_reasoning_effort: controlCodexConfig.reasoning_effort || null,
    codex_service_tier: selectedCodexServiceTier,
    codex_web_search: !!controlCodexConfig.web_search,
    codex_network_access: !!controlCodexConfig.network_access,
    codex_writable_roots: Array.isArray(controlCodexConfig.writable_roots)
      ? controlCodexConfig.writable_roots
      : [],
    codex_managed_context: controlCodexConfig.managed_context || 'vanilla',
    codex_context_archive: controlCodexConfig.context_archive || 'summary',
  };
  try {
    const resp = await dashboardTransport.jsonFetch('api_settings_save', payload, () => fetch('/api/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    }), 'api_settings_save', { fallbackAfterRpcFailure: false });
    const data = await resp.json();
    const status = g('settings-status');
    if (data.ok) {
      status.textContent = 'Saved';
      status.style.color = 'var(--green)';
    } else {
      status.textContent = 'Error: ' + (data.error || 'Unknown');
      status.style.color = 'var(--red)';
    }
    setTimeout(() => { status.textContent = ''; }, 3000);
  } catch (e) {
    console.error('Failed to save settings:', e);
  }
  // Also emit the control messages so the in-memory shared state
  // updates immediately (without waiting for the next daemon restart
  // to re-read the TOML). The POST above persists — and the gateway
  // re-dispatches the codex fields server-side too — but keep this
  // list complete so the dashboard never depends on that, and cover
  // every codex field with a live control-plane setter.
  const agentVal = g('set-external-agent').value || null;
  dispatchControlMsg({ action: 'set_external_agent', agent: agentVal });
  dispatchControlMsg({
    action: 'set_codex_command',
    command: (g('set-codex-command').value || '').trim() || null,
  });
  dispatchControlMsg({
    action: 'set_codex_managed_command',
    command: (g('set-codex-managed-command').value || '').trim() || null,
  });
  dispatchControlMsg({
    action: 'set_codex_managed_context',
    mode: controlCodexConfig.managed_context || 'vanilla',
  });
  dispatchControlMsg({
    action: 'set_codex_context_archive',
    mode: controlCodexConfig.context_archive || 'summary',
  });
  dispatchControlMsg({
    action: 'set_codex_service_tier',
    service_tier: selectedCodexServiceTier || null,
  });
}

document.getElementById('settings-save-btn').addEventListener('click', saveSettings);
document.getElementById('settings-reset-btn').addEventListener('click', () => { settingsLoaded = false; loadSettings(); });
document.getElementById('download-session-report-btn')?.addEventListener('click', downloadSessionReportViaDashboardControl);
document.getElementById('connect-self-test-btn')?.addEventListener('click', () => {
  runConnectSelfTests().catch(err => {
    console.error('Connect self-test failed:', err);
    setConnectSelfTestStatus(err?.message || String(err), 'err');
  });
});
document.getElementById('files-download-path')?.addEventListener('input', refreshFilesDownloadAvailability);
document.getElementById('files-download-path')?.addEventListener('keydown', ev => {
  if (ev.key !== 'Enter') return;
  ev.preventDefault();
  startFilesDownload();
});
document.getElementById('files-upload-input')?.addEventListener('change', ev => {
  const files = Array.from(ev.target.files || []);
  queueFilesUploads(files);
  ev.target.value = '';
});
document.getElementById('files-upload-destination')?.addEventListener('input', () => {
  const input = document.getElementById('files-upload-destination');
  if (input) input.title = input.value || '';
});
{
  const dropzone = document.getElementById('files-upload-dropzone');
  if (dropzone) {
    dropzone.addEventListener('dragover', ev => {
      ev.preventDefault();
      dropzone.classList.add('dragover');
      if (ev.dataTransfer) ev.dataTransfer.dropEffect = 'copy';
    });
    dropzone.addEventListener('dragleave', () => {
      dropzone.classList.remove('dragover');
    });
    dropzone.addEventListener('drop', ev => {
      ev.preventDefault();
      dropzone.classList.remove('dragover');
      const files = Array.from(ev.dataTransfer?.files || []);
      queueFilesUploads(files);
    });
  }
}

// External agent dropdown is applied on Save click alongside other settings,
// not on change — matches the rest of the Settings tab UX.

function updateFramerateWarning() {
  const val = parseInt(document.getElementById('set-recording-framerate').value) || 0;
  const warn = document.getElementById('framerate-warning');
  if (warn) warn.classList.toggle('hidden', val <= 15);
}
document.getElementById('set-recording-framerate').addEventListener('input', updateFramerateWarning);

// ── API Keys ──
// apiKeyStatusLoaded lives with the minimal JS state block (deep-link TDZ).

async function loadApiKeyStatus() {
  try {
    const d = await fetchApiKeyStatus();
    if (d.error) { console.warn('API key status error:', d.error); return; }
    for (const [key, configured] of Object.entries(d)) {
      const el = document.getElementById('key-status-' + key);
      if (el) {
        el.textContent = configured ? '\u2713' : '';
        el.classList.toggle('configured', !!configured);
      }
    }
    apiKeyStatusLoaded = true;
  } catch (e) {
    console.error('Failed to load API key status:', e);
  }
}

async function saveApiKeys() {
  const keys = {};
  const openai = document.getElementById('settings-key-openai').value.trim();
  const anthropic = document.getElementById('settings-key-anthropic').value.trim();
  const gemini = document.getElementById('settings-key-gemini').value.trim();
  if (openai) keys.OPENAI_API_KEY = openai;
  if (anthropic) keys.ANTHROPIC_API_KEY = anthropic;
  if (gemini) keys.GEMINI_API_KEY = gemini;

  if (Object.keys(keys).length === 0) {
    const status = document.getElementById('settings-keys-status');
    status.textContent = 'Enter at least one key';
    status.style.color = 'var(--peach)';
    setTimeout(() => { status.textContent = ''; }, 3000);
    return;
  }

  try {
    const resp = await dashboardTransport.jsonFetch('api_api_keys_save', { keys }, () => fetch('/api/api-keys', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ keys }),
    }), 'api_api_keys_save', { fallbackAfterRpcFailure: false });
    const data = await resp.json();
    const status = document.getElementById('settings-keys-status');
    if (data.ok) {
      status.textContent = 'Saved';
      status.style.color = 'var(--green)';
      // Clear inputs after successful save
      document.getElementById('settings-key-openai').value = '';
      document.getElementById('settings-key-anthropic').value = '';
      document.getElementById('settings-key-gemini').value = '';
      // Refresh status indicators
      loadApiKeyStatus();
    } else {
      status.textContent = 'Error: ' + (data.error || 'Unknown');
      status.style.color = 'var(--red)';
    }
    setTimeout(() => { status.textContent = ''; }, 3000);
  } catch (e) {
    console.error('Failed to save API keys:', e);
    const status = document.getElementById('settings-keys-status');
    status.textContent = 'Network error';
    status.style.color = 'var(--red)';
    setTimeout(() => { status.textContent = ''; }, 3000);
  }
}

document.getElementById('settings-save-keys').addEventListener('click', saveApiKeys);

// ── All Sessions Usage (in Stats tab) ──

function fetchSessionsForHost(hostId, options = {}) {
  hostId = hostId || selfPeerId;
  const cacheSessionMetadata = options.cacheSessionMetadata !== false;
  const cacheKey = sessionListCacheKey(hostId, options);
  const cached = sessionsListCache.get(cacheKey);
  if (cached && !options.force) {
    return Promise.resolve(cached);
  }
  const inflight = sessionsListInflight.get(cacheKey);
  if (inflight) {
    return inflight;
  }

  let baseUrl = '';
  if (hostId !== selfPeerId) {
    const d = daemons.find(x => x.host_id === hostId);
    if (!d) return Promise.reject(new Error('Unknown daemon host'));
    baseUrl = d.url.replace(/\/$/, '');
  }

  const limit = sessionListRequestLimit(options);
  const view = options.view === 'usage' ? 'usage' : '';
  const url = sessionListUrl(baseUrl, limit, view);
  const rpcParams = view ? { limit, view } : { limit };
  // The whole-corpus usage view is a couple of MB; pulling it through the
  // RPC datachannel takes seconds while local HTTP delivers it instantly.
  // Only Connect-tunneled dashboards (no direct HTTP) keep using RPC.
  const preferHttp = view === 'usage' && hostId === selfPeerId && !dashboardConnectModeEnabled();
  const loadSessions = async () => {
    if (preferHttp) {
      const r = await authedFetch(url);
      if (!r.ok) throw new Error(`/api/sessions returned ${r.status}`);
      return r.json();
    }
    if (hostId === selfPeerId && dashboardTransport?.canUseRpc()) {
      try {
        return await dashboardTransport.request('api_sessions', rpcParams);
      } catch (err) {
        if (dashboardConnectModeEnabled()) throw err;
        console.warn('[dashboard-control] api_sessions RPC failed, falling back to HTTP', err);
      }
    }
    if (hostId === selfPeerId && dashboardConnectModeEnabled()) {
      throw new Error('Session list is unavailable until dashboard access reconnects');
    }
    if (hostId !== selfPeerId && peerDashboardControlSignalAvailable(hostId)) {
      try {
        const conn = await peerDashboardControlConnectionForHost(hostId, {
          signal: options.signal,
          timeoutMs: 30000,
        });
        return await conn.request('api_sessions', rpcParams, {
          signal: options.signal,
          timeoutMs: 60000,
        });
      } catch (err) {
        if (err?.name === 'AbortError') throw err;
        if (dashboardConnectModeEnabled()) throw err;
        console.warn('[peer-dashboard-control] api_sessions RPC failed, falling back to direct peer HTTP', err);
      }
    }
    if (hostId !== selfPeerId && dashboardConnectModeEnabled()) {
      throw new Error('Peer sessions are unavailable for this target');
    }
    const r = await (hostId === selfPeerId ? authedFetch(url) : fetch(url));
    if (!r.ok) throw new Error(`/api/sessions returned ${r.status}`);
    return r.json();
  };
  const promise = loadSessions()
    .then(sessions => {
      if (!Array.isArray(sessions)) {
        throw new Error('/api/sessions returned a non-array payload');
      }
      sessionsListCache.set(cacheKey, sessions);
      if (hostId === selfPeerId && cacheSessionMetadata) cacheSessionWindowMetadata(sessions);
      return sessions;
    })
    .finally(() => {
      if (sessionsListInflight.get(cacheKey) === promise) {
        sessionsListInflight.delete(cacheKey);
      }
    });
  sessionsListInflight.set(cacheKey, promise);
  return promise;
}

function sessionListRowKey(session) {
  const source = normalizeAgentId(session?.source || session?.backend_source || session?.backendSource || '') || 'intendant';
  const id = String(session?.session_id || session?.resume_id || session?.backend_session_id || session?.backendSessionId || '').trim();
  return id ? `${source}\u001f${id}` : '';
}

function mergeSessionRows(existing, incoming) {
  const rows = Array.isArray(existing) ? existing : [];
  const byKey = new Map();
  for (const row of rows) {
    const key = sessionListRowKey(row);
    if (key) byKey.set(key, row);
  }
  for (const row of incoming || []) {
    const key = sessionListRowKey(row);
    if (!key) continue;
    byKey.set(key, {
      ...(byKey.get(key) || {}),
      ...row,
    });
  }
  return Array.from(byKey.values());
}

function handleSessionStreamEvent(event, onEvent = {}, state = {}) {
  if (event.type === 'session' && event.session) {
    onEvent.sessions?.([event.session], !!event.partial);
  } else if (event.type === 'replace' && Array.isArray(event.sessions)) {
    state.finalSessions = event.sessions;
    onEvent.replace?.(event.sessions);
  } else if (event.type === 'phase') {
    onEvent.phase?.(event.phase || '');
  } else if (event.type === 'done') {
    onEvent.done?.();
  }
}

async function streamSessionsForHost(hostId, options = {}, onEvent = {}) {
  hostId = hostId || selfPeerId;
  let baseUrl = '';
  if (hostId !== selfPeerId) {
    const d = daemons.find(x => x.host_id === hostId);
    if (!d) throw new Error('Unknown daemon host');
    baseUrl = d.url.replace(/\/$/, '');
  }
  const limit = sessionListRequestLimit(options);
  if (hostId === selfPeerId && dashboardTransport?.canUseRpc()) {
    const state = { finalSessions: null };
    try {
      await dashboardTransport.stream('api_sessions_stream', { limit }, {
        signal: options.signal,
        timeoutMs: 120000,
      }, {
        event(event) {
          handleSessionStreamEvent(event, onEvent, state);
        },
      });
      return state.finalSessions;
    } catch (err) {
      if (err?.name === 'AbortError') throw err;
      if (dashboardConnectModeEnabled()) throw err;
      console.warn('[dashboard-control] api_sessions_stream RPC failed, falling back to HTTP stream', err);
    }
  }
  if (hostId === selfPeerId && dashboardConnectModeEnabled()) {
    throw new Error('Session stream is unavailable until dashboard access reconnects');
  }
  if (hostId !== selfPeerId && peerDashboardControlSignalAvailable(hostId)) {
    const state = { finalSessions: null };
    try {
      const conn = await peerDashboardControlConnectionForHost(hostId, {
        signal: options.signal,
        timeoutMs: 30000,
      });
      await conn.stream('api_sessions_stream', { limit }, {
        signal: options.signal,
        timeoutMs: 120000,
      }, {
        event(event) {
          handleSessionStreamEvent(event, onEvent, state);
        },
      });
      return state.finalSessions;
    } catch (err) {
      if (err?.name === 'AbortError') throw err;
      if (dashboardConnectModeEnabled()) throw err;
      console.warn('[peer-dashboard-control] api_sessions_stream RPC failed, falling back to direct peer HTTP stream', err);
    }
  }
  if (hostId !== selfPeerId && dashboardConnectModeEnabled()) {
    throw new Error('Peer session updates are unavailable for this target');
  }
  const url = sessionListStreamUrl(baseUrl, limit);
  const request = hostId === selfPeerId
    ? authedFetch(url, { signal: options.signal })
    : fetch(url, { signal: options.signal });
  const response = await request;
  if (!response.ok || !response.body) {
    throw new Error(`/api/sessions/stream returned ${response.status}`);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const state = { finalSessions: null };
  const handleLine = line => {
    const trimmed = line.trim();
    if (!trimmed) return;
    const event = JSON.parse(trimmed);
    handleSessionStreamEvent(event, onEvent, state);
  };
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';
    for (const line of lines) handleLine(line);
  }
  buffer += decoder.decode();
  if (buffer.trim()) handleLine(buffer);
  return state.finalSessions;
}

function currentStatsHostKey() {
  return activeStatsHost || selfPeerId;
}

function renderStatsSessionSections(sessions) {
  if (!paneIsVisible('stats')) {
    renderOrDefer('stats', 'session-sections', () => renderStatsSessionSections(sessions));
    return;
  }
  setStatsSessionLoading(currentStatsHostKey(), false);
  renderStatsKpiRow(sessions);
  renderTokenActivity(sessions);
  renderAllSessionsUsage(sessions);
  renderDailyUsage(sessions);
  renderDiskUsage(sessions);
}

// Headline KPI tiles at the top of the Stats tab, derived from the same
// per-session usage entries the section renderers consume.
function statsKpiTileHtml(label, value) {
  return `
    <div class="ui-stat">
      <span class="ui-stat-label">${escapeHtml(label)}</span>
      <span class="ui-stat-value">${escapeHtml(value)}</span>
    </div>`;
}

function renderStatsKpiRow(sessions) {
  const row = document.getElementById('stats-kpi-row');
  if (!row) return;
  if (!Array.isArray(sessions) || sessions.length === 0) {
    row.innerHTML = '';
    return;
  }
  const totals = summarizeSessionUsage(sessions);
  const activeDays = Array.from(buildDailyUsageBuckets(sessions, 'all').values())
    .filter(bucket => bucket.total > 0).length;
  row.innerHTML = [
    statsKpiTileHtml('Today cost', formatUsdCompact(totals.todayCost)),
    statsKpiTileHtml('7-day cost', formatUsdCompact(totals.weekCost)),
    statsKpiTileHtml('All-time cost', formatUsdCompact(totals.allCost)),
    statsKpiTileHtml('Lifetime tokens', formatCompactNumber(totals.allTokens)),
    statsKpiTileHtml('Active days', activeDays.toLocaleString()),
  ].join('');
}

function renderStatsKpiSkeleton() {
  const row = document.getElementById('stats-kpi-row');
  if (!row) return;
  row.innerHTML = Array.from({ length: 5 }, () => `
    <div class="ui-stat">
      <span class="ui-skel stats-kpi-skel-label"></span>
      <span class="ui-skel stats-kpi-skel-value"></span>
    </div>`).join('');
}

function clearStatsSessionSections() {
  for (const id of [
    'token-activity-section',
    'all-sessions-usage',
    'daily-usage-section',
    'agent-usage-section',
    'disk-usage-section',
  ]) {
    const el = document.getElementById(id);
    if (el) el.style.display = 'none';
  }
  for (const id of [
    'stats-kpi-row',
    'token-activity-stats',
    'token-activity-skyline',
    'token-activity-months',
    'token-activity-heatmap',
    'token-activity-detail',
    'all-sessions-grid',
    'daily-usage-grid',
    'agent-usage-grid',
    'disk-usage-grid',
  ]) {
    const el = document.getElementById(id);
    if (el) el.innerHTML = '';
  }
}

function cachedStatsSessions(hostId) {
  return sessionsListCache.get(sessionListCacheKey(hostId || selfPeerId, { limit: 'all', view: 'usage' }));
}

function setStatsSessionLoading(hostId, loading) {
  const el = document.getElementById('stats-session-loading');
  if (!el) return;
  const normalizedHost = hostId || selfPeerId;
  const currentHost = currentStatsHostKey();
  if (normalizedHost !== currentHost) return;
  el.style.display = loading ? 'flex' : 'none';
  // KPI tiles shimmer while the session fetch is in flight; the section
  // renderers (or the error path) replace them.
  if (loading) renderStatsKpiSkeleton();
  const container = document.getElementById('usage-container');
  if (container) {
    if (loading) container.setAttribute('aria-busy', 'true');
    else container.removeAttribute('aria-busy');
  }
}

function renderCachedStatsSessionSections(hostId) {
  const sessions = cachedStatsSessions(hostId);
  if (Array.isArray(sessions)) {
    renderStatsSessionSections(sessions);
    return true;
  }
  clearStatsSessionSections();
  return false;
}

function statsHostHasLiveUsage(hostId) {
  const cached = hostStatsCache.get(hostId || selfPeerId);
  return !!(cached && cached.main_json);
}

function sessionNumber(value) {
  const n = Number(value || 0);
  return Number.isFinite(n) ? n : 0;
}

function sessionCost(value) {
  const n = Number(value || 0);
  return Number.isFinite(n) ? n : 0;
}

function formatUsd(value, digits) {
  const n = Number(value);
  if (!Number.isFinite(n)) return '$--';
  const places = digits != null ? digits : (Math.abs(n) > 0 && Math.abs(n) < 1 ? 4 : 2);
  return '$' + n.toFixed(places);
}

// Stat-tile costs: compact above $1K ($93.2K), exact below ($264.82).
function formatUsdCompact(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return '$--';
  if (Math.abs(n) >= 1000) {
    return '$' + new Intl.NumberFormat(undefined, {
      notation: 'compact',
      maximumFractionDigits: 1,
    }).format(n);
  }
  return formatUsd(n, 2);
}

function sessionDate(session) {
  const raw = session && (session.updated_at || session.created_at);
  if (!raw) return null;
  const normalized = typeof raw === 'string' && raw.includes(' ') && !raw.includes('T')
    ? raw.replace(' ', 'T')
    : raw;
  const parsed = Date.parse(normalized);
  return Number.isNaN(parsed) ? null : new Date(parsed);
}

function localIsoDay(date) {
  if (!(date instanceof Date) || Number.isNaN(date.getTime())) return null;
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

function localDateFromIsoDay(iso) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(String(iso || ''))) return null;
  const [year, month, day] = iso.split('-').map(Number);
  const date = new Date(year, month - 1, day);
  return Number.isNaN(date.getTime()) ? null : date;
}

function tokenActivityIsoToday() {
  return localIsoDay(new Date());
}

function parseIsoDayMs(iso) {
  const date = localDateFromIsoDay(iso);
  return date ? date.getTime() : null;
}

function isoDayFromMs(ms) {
  return localIsoDay(new Date(ms));
}

function addIsoDays(iso, days) {
  const date = localDateFromIsoDay(iso);
  if (!date) return null;
  date.setDate(date.getDate() + days);
  return localIsoDay(date);
}

function startOfWeekIso(iso) {
  const date = localDateFromIsoDay(iso);
  if (!date) return tokenActivityIsoToday();
  date.setDate(date.getDate() - date.getDay());
  return localIsoDay(date);
}

function sessionActivityDay(session) {
  return localIsoDay(sessionDate(session));
}

function usageEntryDay(entry, fallbackDay) {
  const raw = entry && entry.day;
  if (/^\d{4}-\d{2}-\d{2}$/.test(String(raw || ''))) return raw;
  return fallbackDay || null;
}

function sessionUsageEntries(session) {
  const fallbackDay = sessionActivityDay(session);
  const daily = Array.isArray(session && session.daily_usage) ? session.daily_usage : [];
  const entries = daily
    .map(entry => ({
      day: usageEntryDay(entry, fallbackDay),
      input: sessionNumber(entry.prompt_tokens),
      cached: sessionNumber(entry.cached_tokens),
      output: sessionNumber(entry.completion_tokens),
      total: sessionNumber(entry.total_tokens) || sessionNumber(entry.prompt_tokens) + sessionNumber(entry.completion_tokens),
      cost: sessionCost(entry.estimated_cost),
      pricingKnown: entry.pricing_known === true,
    }))
    .filter(entry => entry.day && (
      entry.total > 0 || entry.input > 0 || entry.cached > 0 || entry.output > 0 || entry.cost > 0
    ));
  if (entries.length > 0) return entries;
  if (!fallbackDay) return [];
  const input = sessionNumber(session && session.prompt_tokens);
  const output = sessionNumber(session && session.completion_tokens);
  return [{
    day: fallbackDay,
    input,
    cached: sessionNumber(session && session.cached_tokens),
    output,
    total: sessionNumber(session && session.total_tokens) || input + output,
    cost: sessionCost(session && session.estimated_cost),
    pricingKnown: session && session.pricing_known === true,
  }];
}

// Rolling window: today plus the six preceding days, in local time.
// (Was a Sunday-anchored calendar week, which made the "week" numbers
// equal the "today" numbers every Sunday.)
function usageEntryInLast7Days(day, todayIso) {
  if (!day || !todayIso || day > todayIso) return false;
  const start = addIsoDays(todayIso, -6);
  return !!start && day >= start;
}

function summarizeSessionUsage(sessions) {
  const todayIso = tokenActivityIsoToday();
  const totals = {
    sessions: sessions.length,
    turns: 0,
    allInput: 0,
    allCached: 0,
    allOutput: 0,
    allTokens: 0,
    allCost: 0,
    todayCost: 0,
    weekCost: 0,
  };
  for (const s of sessions) {
    totals.turns += sessionNumber(s.turns);
    for (const entry of sessionUsageEntries(s)) {
      totals.allInput += entry.input;
      totals.allCached += entry.cached;
      totals.allOutput += entry.output;
      totals.allTokens += entry.total;
      totals.allCost += entry.cost;
      if (entry.day === todayIso) totals.todayCost += entry.cost;
      if (usageEntryInLast7Days(entry.day, todayIso)) totals.weekCost += entry.cost;
    }
  }
  return totals;
}

function agentUsageKey(session) {
  const source = (session && session.source) || 'intendant';
  if (source === 'claude-code') return 'claude';
  if (source === 'codex' || source === 'intendant') return source;
  return 'other';
}

function agentUsageLabel(key) {
  return {
    codex: 'Codex',
    claude: 'Claude Code',
    intendant: 'Intendant Internal',
    other: 'Other',
  }[key] || key;
}

function agentUsagePeriodBucket(key, periodKey, periodLabel) {
  return {
    key,
    label: agentUsageLabel(key),
    periodKey,
    periodLabel,
    sessions: 0,
    input: 0,
    cached: 0,
    output: 0,
    total: 0,
    cost: 0,
    unpricedTokens: 0,
    sessionIds: new Set(),
  };
}

function agentUsageSessionIdentity(session, index) {
  const id = (session && (session.session_id || session.resume_id || session.path || session.updated_at)) || index;
  return `${agentUsageKey(session)}:${id}`;
}

function addEntryToAgentUsageBucket(bucket, sessionId, entry) {
  if (!bucket.sessionIds.has(sessionId)) {
    bucket.sessionIds.add(sessionId);
    bucket.sessions += 1;
  }
  bucket.input += entry.input;
  bucket.cached += entry.cached;
  bucket.output += entry.output;
  bucket.total += entry.total;
  bucket.cost += entry.cost;
  if (entry.total > 0 && entry.cost === 0 && entry.pricingKnown !== true) {
    bucket.unpricedTokens += entry.total;
  }
}

function summarizeUsageByAgent(sessions) {
  const order = ['codex', 'claude', 'intendant', 'other'];
  const periods = [
    { key: 'today', label: 'Today' },
    { key: 'week', label: 'Last 7 Days' },
    { key: 'all', label: 'All Time' },
  ];
  const buckets = new Map();
  const todayIso = tokenActivityIsoToday();

  function bucketFor(agentKey, period) {
    const bucketKey = `${agentKey}:${period.key}`;
    if (!buckets.has(bucketKey)) {
      buckets.set(bucketKey, agentUsagePeriodBucket(agentKey, period.key, period.label));
    }
    return buckets.get(bucketKey);
  }

  sessions.forEach((s, index) => {
    const key = agentUsageKey(s);
    const sessionId = agentUsageSessionIdentity(s, index);
    for (const entry of sessionUsageEntries(s)) {
      for (const period of periods) {
        if (
          period.key === 'all' ||
          (period.key === 'today' && entry.day === todayIso) ||
          (period.key === 'week' && usageEntryInLast7Days(entry.day, todayIso))
        ) {
          addEntryToAgentUsageBucket(bucketFor(key, period), sessionId, entry);
        }
      }
    }
  });

  const agentRank = new Map(order.map((key, index) => [key, index]));
  const periodRank = new Map(periods.map((period, index) => [period.key, index]));
  return Array.from(buckets.values())
    .filter(b => b.sessions > 0)
    .sort((a, b) => {
      const byAgent = (agentRank.get(a.key) ?? order.length) - (agentRank.get(b.key) ?? order.length);
      if (byAgent !== 0) return byAgent;
      return (periodRank.get(a.periodKey) ?? periods.length) - (periodRank.get(b.periodKey) ?? periods.length);
    });
}

let tokenActivityAgent = 'all';
let tokenActivityView = 'daily';
let tokenActivitySelectedDay = null;
const TOKEN_ACTIVITY_WEEKS = 53;
const TOKEN_ACTIVITY_DAYS = TOKEN_ACTIVITY_WEEKS * 7;
const TOKEN_ACTIVITY_SKYLINE_DAYS = 84;
const DAILY_USAGE_ROW_LIMIT = 30;

function tokenActivityAgentLabel(key) {
  return {
    all: 'All',
    codex: 'Codex',
    claude: 'Claude',
    intendant: 'Intendant',
    other: 'Other',
  }[key] || key;
}

function formatCompactNumber(value, digits = 1) {
  const n = Number(value || 0);
  if (!Number.isFinite(n)) return '--';
  return new Intl.NumberFormat(undefined, {
    maximumFractionDigits: digits,
    notation: 'compact',
  }).format(Math.max(0, Math.round(n)));
}

function formatShortIsoDate(iso) {
  const ms = parseIsoDayMs(iso);
  if (ms == null) return '--';
  return new Intl.DateTimeFormat(undefined, {
    day: 'numeric',
    month: 'short',
  }).format(new Date(ms));
}

function formatMonthLabel(iso) {
  const ms = parseIsoDayMs(iso);
  if (ms == null) return '';
  return new Intl.DateTimeFormat(undefined, {
    month: 'short',
  }).format(new Date(ms));
}

function formatDailyUsageDay(iso, todayIso) {
  if (iso === todayIso) return 'Today';
  if (iso === addIsoDays(todayIso, -1)) return 'Yesterday';
  const date = localDateFromIsoDay(iso);
  if (!date) return iso || '--';
  const now = new Date();
  const options = date.getFullYear() === now.getFullYear()
    ? { day: 'numeric', month: 'short' }
    : { day: 'numeric', month: 'short', year: 'numeric' };
  return new Intl.DateTimeFormat(undefined, options).format(date);
}

function exactNumber(value) {
  const n = Number(value || 0);
  return Number.isFinite(n) ? Math.round(n).toLocaleString() : '0';
}

function tokenActivitySessionMatches(session, agentKey) {
  if (agentKey === 'all') return true;
  return agentUsageKey(session) === agentKey;
}

function tokenActivityAvailableAgents(sessions) {
  const available = new Set(['all']);
  for (const s of sessions) {
    if (sessionUsageEntries(s).some(entry => entry.total > 0)) available.add(agentUsageKey(s));
  }
  return available;
}

function ensureTokenActivityAgent(sessions) {
  const available = tokenActivityAvailableAgents(sessions);
  if (available.has(tokenActivityAgent)) return available;
  tokenActivityAgent = available.has('codex') ? 'codex' : 'all';
  return available;
}

function buildDailyUsageBuckets(sessions, agentKey = 'all') {
  const buckets = new Map();
  for (const s of sessions) {
    if (!tokenActivitySessionMatches(s, agentKey)) continue;
    const countedDays = new Set();
    for (const entry of sessionUsageEntries(s)) {
      if (!entry.day) continue;
      const hasUsage = entry.total > 0 || entry.input > 0 || entry.output > 0 || entry.cached > 0 || entry.cost > 0;
      if (!hasUsage) continue;
      if (!buckets.has(entry.day)) {
        buckets.set(entry.day, {
          day: entry.day,
          sessions: 0,
          input: 0,
          cached: 0,
          output: 0,
          total: 0,
          cost: 0,
        });
      }
      const bucket = buckets.get(entry.day);
      if (!countedDays.has(entry.day)) {
        bucket.sessions += 1;
        countedDays.add(entry.day);
      }
      bucket.input += entry.input;
      bucket.cached += entry.cached;
      bucket.output += entry.output;
      bucket.total += entry.total;
      bucket.cost += entry.cost;
    }
  }
  return buckets;
}

function buildTokenActivityDaily(sessions, agentKey) {
  const buckets = buildDailyUsageBuckets(sessions, agentKey);
  const daily = new Map();
  let lifetime = 0;
  for (const [day, bucket] of buckets) {
    daily.set(day, bucket.total);
    lifetime += bucket.total;
  }
  return { daily, lifetime, buckets };
}

function buildTokenActivityWeeks(daily) {
  const weeks = new Map();
  for (const [day, value] of daily) {
    const week = startOfWeekIso(day);
    weeks.set(week, (weeks.get(week) || 0) + value);
  }
  return weeks;
}

function tokenActivityCellLevel(value, max) {
  if (value <= 0 || max <= 0) return 0;
  const ratio = value / max;
  if (ratio > 0.75) return 5;
  if (ratio > 0.50) return 4;
  if (ratio > 0.25) return 3;
  if (ratio > 0.10) return 2;
  return 1;
}

function tokenActivityStatHtml(value, label) {
  return `
    <div class="token-activity-stat">
      <div class="value">${escapeHtml(value)}</div>
      <div class="label">${escapeHtml(label)}</div>
    </div>
  `;
}

function tokenActivityBuildSeries(daily, todayIso) {
  const startIso = addIsoDays(startOfWeekIso(todayIso), -(TOKEN_ACTIVITY_WEEKS - 1) * 7);
  const weeks = buildTokenActivityWeeks(daily);
  const days = [];
  let cumulative = 0;
  for (let i = 0; i < TOKEN_ACTIVITY_DAYS; i += 1) {
    const day = addIsoDays(startIso, i);
    const dailyValue = daily.get(day) || 0;
    cumulative += dailyValue;
    days.push({
      day,
      daily: dailyValue,
      weekly: weeks.get(startOfWeekIso(day)) || 0,
      cumulative,
      future: day > todayIso,
    });
  }
  return { startIso, days };
}

function tokenActivityValueForCell(cell) {
  if (!cell) return 0;
  if (tokenActivityView === 'weekly') return cell.weekly;
  if (tokenActivityView === 'cumulative') return cell.cumulative;
  return cell.daily;
}

function renderTokenActivityControls(sessions) {
  const available = ensureTokenActivityAgent(sessions);
  const agentEl = document.getElementById('token-activity-agent');
  if (agentEl) {
    for (const btn of agentEl.querySelectorAll('button[data-agent]')) {
      const agent = btn.dataset.agent;
      const isAvailable = available.has(agent);
      btn.disabled = !isAvailable;
      btn.classList.toggle('active', agent === tokenActivityAgent);
    }
  }
  const viewEl = document.getElementById('token-activity-view');
  if (viewEl) {
    for (const btn of viewEl.querySelectorAll('button[data-view]')) {
      btn.classList.toggle('active', btn.dataset.view === tokenActivityView);
    }
  }
  const title = document.getElementById('token-activity-title');
  if (title) title.textContent = `${tokenActivityAgentLabel(tokenActivityAgent)} Token Activity`;
}

function renderTokenActivityMonths(startIso) {
  const el = document.getElementById('token-activity-months');
  if (!el) return;
  el.innerHTML = '';
  let lastMonth = '';
  for (let col = 0; col < TOKEN_ACTIVITY_WEEKS; col += 1) {
    const day = addIsoDays(startIso, col * 7);
    const month = day.slice(0, 7);
    if (month === lastMonth && col !== 0) continue;
    lastMonth = month;
    const span = document.createElement('span');
    span.className = 'token-activity-month';
    span.style.gridColumn = `${col + 1} / span 4`;
    span.textContent = formatMonthLabel(day);
    el.appendChild(span);
  }
}

function renderTokenActivitySkyline(days, todayIso) {
  const el = document.getElementById('token-activity-skyline');
  if (!el) return;
  const startIso = addIsoDays(todayIso, -(TOKEN_ACTIVITY_SKYLINE_DAYS - 1));
  const startMs = parseIsoDayMs(startIso);
  const endMs = parseIsoDayMs(todayIso);
  const visible = days.filter(cell => {
    const ms = parseIsoDayMs(cell.day);
    return ms != null && ms >= startMs && ms <= endMs;
  });
  const values = visible.map(tokenActivityValueForCell);
  const max = Math.max(0, ...values);
  el.innerHTML = '';
  for (const cell of visible) {
    const value = tokenActivityValueForCell(cell);
    const bar = document.createElement('div');
    bar.className = 'token-activity-skyline-bar';
    if (value <= 0) bar.classList.add('empty');
    bar.style.height = `${Math.max(3, Math.round((value / Math.max(1, max)) * 86))}px`;
    bar.title = `${formatCompactNumber(value)} tokens, ${formatShortIsoDate(cell.day)}`;
    el.appendChild(bar);
  }
}

function renderTokenActivityHeatmap(days, startIso, todayIso) {
  const el = document.getElementById('token-activity-heatmap');
  if (!el) return;
  renderTokenActivityMonths(startIso);
  const values = days.filter(cell => !cell.future).map(tokenActivityValueForCell);
  const max = Math.max(0, ...values);
  el.innerHTML = '';
  for (const cell of days) {
    const value = tokenActivityValueForCell(cell);
    const div = document.createElement('div');
    const level = tokenActivityCellLevel(cell.future ? 0 : value, max);
    const selected = tokenActivitySelectedDay === cell.day && !cell.future;
    div.className = `token-activity-day level-${level}${cell.future ? ' future' : ''}${selected ? ' selected' : ''}`;
    div.title = cell.future
      ? formatShortIsoDate(cell.day)
      : `${formatCompactNumber(value)} tokens, ${formatShortIsoDate(cell.day)}`;
    div.dataset.day = cell.day;
    if (!cell.future) {
      div.role = 'button';
      div.tabIndex = 0;
      div.setAttribute('aria-label', `${formatDailyUsageDay(cell.day, todayIso)}: ${exactNumber(value)} tokens`);
      div.setAttribute('aria-pressed', selected ? 'true' : 'false');
    }
    el.appendChild(div);
  }
}

function tokenActivityMetricHtml(label, value) {
  return `
    <div class="token-activity-detail-metric">
      <div class="label">${escapeHtml(label)}</div>
      <div class="value">${escapeHtml(value)}</div>
    </div>
  `;
}

function renderTokenActivitySelectedDay(buckets, todayIso) {
  const el = document.getElementById('token-activity-detail');
  if (!el) return;
  const day = tokenActivitySelectedDay;
  const bucket = day ? buckets.get(day) : null;
  if (!bucket) {
    el.style.display = 'none';
    el.innerHTML = '';
    return;
  }

  el.innerHTML = `
    <div class="token-activity-detail-head">
      <div class="token-activity-detail-day">${escapeHtml(formatDailyUsageDay(day, todayIso))}</div>
      <div class="token-activity-detail-agent">${escapeHtml(tokenActivityAgentLabel(tokenActivityAgent))}</div>
    </div>
    <div class="token-activity-detail-grid">
      ${tokenActivityMetricHtml('Total', exactNumber(bucket.total))}
      ${tokenActivityMetricHtml('Input', exactNumber(bucket.input))}
      ${tokenActivityMetricHtml('Cached', exactNumber(bucket.cached))}
      ${tokenActivityMetricHtml('Output', exactNumber(bucket.output))}
      ${tokenActivityMetricHtml('Cost', formatUsd(bucket.cost, 4))}
      ${tokenActivityMetricHtml('Sessions', exactNumber(bucket.sessions))}
    </div>
  `;
  el.style.display = 'block';
}

function renderTokenActivity(sessions) {
  const section = document.getElementById('token-activity-section');
  const statsEl = document.getElementById('token-activity-stats');
  const heatmapEl = document.getElementById('token-activity-heatmap');
  const skylineEl = document.getElementById('token-activity-skyline');
  if (!section || !statsEl || !heatmapEl || !skylineEl) return;
  if (!Array.isArray(sessions) || sessions.length === 0) {
    section.style.display = 'none';
    return;
  }

  renderTokenActivityControls(sessions);
  const todayIso = tokenActivityIsoToday();
  const { daily, lifetime, buckets } = buildTokenActivityDaily(sessions, tokenActivityAgent);
  const activeDays = Array.from(daily.values()).filter(v => v > 0).length;
  if (activeDays === 0) {
    section.style.display = 'none';
    return;
  }

  let peakDay = null;
  let peakTokens = 0;
  let latestDay = null;
  let latestTokens = 0;
  for (const [day, tokens] of daily) {
    if (tokens > peakTokens) {
      peakTokens = tokens;
      peakDay = day;
    }
    if (day <= todayIso && (!latestDay || day > latestDay)) {
      latestDay = day;
      latestTokens = tokens;
    }
  }

  statsEl.innerHTML = [
    tokenActivityStatHtml(formatCompactNumber(lifetime), 'Lifetime'),
    tokenActivityStatHtml(formatCompactNumber(peakTokens), `Peak ${peakDay ? formatShortIsoDate(peakDay) : ''}`.trim()),
    tokenActivityStatHtml(activeDays.toLocaleString(), 'Active days'),
    tokenActivityStatHtml(formatCompactNumber(latestTokens), latestDay ? formatShortIsoDate(latestDay) : 'Latest day'),
  ].join('');

  const { startIso, days } = tokenActivityBuildSeries(daily, todayIso);
  renderTokenActivitySkyline(days, todayIso);
  renderTokenActivityHeatmap(days, startIso, todayIso);
  renderTokenActivitySelectedDay(buckets, todayIso);
  section.style.display = 'block';
}

function renderStatsSummaryCard(title, subtitle, rows) {
  const card = document.createElement('div');
  card.className = 'usage-card';
  const rowsHtml = rows.map(row => `
    <span class="label">${escapeHtml(row.label)}</span>
    <span class="value">${escapeHtml(row.value)}</span>
  `).join('');
  card.innerHTML = `
    <div class="card-title">${escapeHtml(title)}</div>
    <div class="card-model"><span class="provider">${escapeHtml(subtitle)}</span></div>
    <div class="token-breakdown">${rowsHtml}</div>
  `;
  return card;
}

function renderSessionStatsFallback(sessions) {
  if (statsHostHasLiveUsage(currentStatsHostKey())) return;
  const cardsEl = document.getElementById('usage-cards');
  const emptyEl = document.getElementById('usage-empty');
  const costEl = document.getElementById('cost-section');
  const historyEl = document.getElementById('token-history');
  if (!cardsEl || !Array.isArray(sessions) || sessions.length === 0) return;

  const totals = summarizeSessionUsage(sessions);
  const latest = sessions.find(s => (sessionNumber(s.total_tokens) || sessionNumber(s.prompt_tokens) || sessionNumber(s.completion_tokens) || sessionCost(s.estimated_cost)) > 0) || sessions[0];

  if (emptyEl) emptyEl.style.display = 'none';
  if (historyEl) historyEl.style.display = 'none';
  cardsEl.style.display = 'flex';
  cardsEl.innerHTML = '';
  cardsEl.appendChild(renderStatsSummaryCard('Session Usage', 'All sessions', [
    { label: 'Sessions', value: totals.sessions.toLocaleString() },
    { label: 'Turns', value: totals.turns.toLocaleString() },
    { label: 'Input tokens', value: totals.allInput.toLocaleString() },
    { label: 'Cached', value: totals.allCached.toLocaleString() },
    { label: 'Output tokens', value: totals.allOutput.toLocaleString() },
    { label: 'Total tokens', value: totals.allTokens.toLocaleString() },
  ]));

  if (latest) {
    const provider = latest.provider || latest.source_label || latest.source || 'Session';
    const model = latest.model || 'unknown model';
    const input = sessionNumber(latest.prompt_tokens);
    const cached = sessionNumber(latest.cached_tokens);
    const output = sessionNumber(latest.completion_tokens);
    const total = sessionNumber(latest.total_tokens) || input + output;
    cardsEl.appendChild(renderStatsSummaryCard('Latest Session', `${provider} / ${model}`, [
      { label: 'Input tokens', value: input.toLocaleString() },
      { label: 'Cached', value: cached.toLocaleString() },
      { label: 'Output tokens', value: output.toLocaleString() },
      { label: 'Total tokens', value: total.toLocaleString() },
      { label: 'Estimated cost', value: formatUsd(sessionCost(latest.estimated_cost), 4) },
    ]));
  }

  // Third card: cache economics. Prompt reuse is where this system's
  // token bill is won or lost, and the numbers are already in `totals` —
  // this completes the row as volume | recency | efficiency. (The live
  // usage view has its own third card, so only the fallback needs one.)
  if (totals.allInput > 0) {
    const pct = (part, whole) => (whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : '—');
    const latestInput = latest ? sessionNumber(latest.prompt_tokens) : 0;
    const latestCached = latest ? sessionNumber(latest.cached_tokens) : 0;
    const rows = [
      { label: 'Hit rate', value: pct(totals.allCached, totals.allInput) },
      { label: 'Cached tokens', value: totals.allCached.toLocaleString() },
      { label: 'Fresh input', value: Math.max(0, totals.allInput - totals.allCached).toLocaleString() },
    ];
    if (latestInput > 0) {
      rows.push({ label: 'Latest session', value: pct(latestCached, latestInput) });
    }
    cardsEl.appendChild(renderStatsSummaryCard('Cache', 'Prompt reuse across all sessions', rows));
  }

  if (costEl) {
    costEl.style.display = 'block';
    const grid = document.getElementById('cost-grid');
    if (grid) {
      grid.style.gridTemplateColumns = 'auto 1fr';
      grid.innerHTML = `
        <span class="label">Today</span><span class="value">${formatUsd(totals.todayCost, 2)}</span>
        <span class="label">Last 7 Days</span><span class="value">${formatUsd(totals.weekCost, 2)}</span>
        <span class="label strong">All Time</span><span class="value">${formatUsd(totals.allCost, 2)}</span>
      `;
    }
  }
}

// Load and render the "All Sessions" + "Disk Usage" cards for a
// specific host. Defaults to self (same-origin fetch). When `hostId`
// is a secondary, the fetch targets that daemon's base URL — which
// means the browser needs CORS on the remote's /api/sessions (we set
// `Access-Control-Allow-Origin: *` on that endpoint in web_gateway.rs)
// and, for HTTPS primaries, the remote must also be HTTPS because of
// mixed content. Same rules as fetching the remote's agent card on add.
function loadAllSessionsUsage(hostId, options = {}) {
  hostId = hostId || selfPeerId;
  if (!Array.isArray(cachedStatsSessions(hostId))) {
    clearStatsSessionSections();
    setStatsSessionLoading(hostId, true);
  }
  // The usage view fetches the same corpus at ~a tenth of the payload
  // (usage/cost/day-bucket/disk fields only) — the stats fold reads
  // nothing else. Older peer daemons ignore the param and send full rows.
  fetchSessionsForHost(hostId, { force: !!options.force, limit: 'all', view: 'usage', cacheSessionMetadata: false })
    .then(sessions => {
      // Only render if the user is still viewing this host. A slow
      // fetch that finishes after the user switched away would
      // otherwise stomp the current view.
      const currentHost = activeStatsHost || selfPeerId;
      if (currentHost === hostId) {
        setStatsSessionLoading(hostId, false);
        renderStatsSessionSections(sessions);
      }
    })
    .catch(err => {
      const currentHost = activeStatsHost || selfPeerId;
      if (currentHost !== hostId) return;
      setStatsSessionLoading(hostId, false);
      // A failed refresh must not tear down a view the user already has:
      // keep rendering the cached corpus and surface the failure as a
      // toast. Only fall back to the destructive error card when there
      // was never anything to show.
      const cached = cachedStatsSessions(hostId);
      if (Array.isArray(cached)) {
        renderStatsSessionSections(cached);
        showControlToast('error', 'Session usage refresh failed: ' + (err.message || 'network error'));
        return;
      }
      renderAllSessionsUsageError(err.message || 'Failed to load session usage');
    });
}

function renderAllSessionsUsageError(message) {
  const el = document.getElementById('all-sessions-usage');
  const grid = document.getElementById('all-sessions-grid');
  const emptyEl = document.getElementById('usage-empty');
  const diskEl = document.getElementById('disk-usage-section');
  const agentEl = document.getElementById('agent-usage-section');
  const dailyEl = document.getElementById('daily-usage-section');
  if (!el || !grid) return;
  if (emptyEl) emptyEl.style.display = 'none';
  const kpiRow = document.getElementById('stats-kpi-row');
  if (kpiRow) kpiRow.innerHTML = '';
  el.style.display = 'block';
  grid.style.gridTemplateColumns = '1fr';
  grid.innerHTML = `<span class="label err">${escapeHtml(message)}</span>`;
  if (dailyEl) dailyEl.style.display = 'none';
  if (agentEl) agentEl.style.display = 'none';
  if (diskEl) diskEl.style.display = 'none';
}

function renderAllSessionsUsage(sessions) {
  const el = document.getElementById('all-sessions-usage');
  const grid = document.getElementById('all-sessions-grid');
  const emptyEl = document.getElementById('usage-empty');
  if (!el || !grid) return;
  if (emptyEl) emptyEl.style.display = 'none';
  renderSessionStatsFallback(sessions);
  renderAgentUsage(sessions);

  const todayIso = tokenActivityIsoToday();

  let allTotal = 0, allInput = 0, allOutput = 0, allCached = 0, allCost = 0;
  let todayTotal = 0, todayInput = 0, todayOutput = 0, todayCached = 0, todayCost = 0;
  let weekTotal = 0, weekInput = 0, weekOutput = 0, weekCached = 0, weekCost = 0;

  for (const s of sessions) {
    for (const entry of sessionUsageEntries(s)) {
      allTotal += entry.total;
      allInput += entry.input;
      allOutput += entry.output;
      allCached += entry.cached;
      allCost += entry.cost;
      if (entry.day === todayIso) {
        todayTotal += entry.total;
        todayInput += entry.input;
        todayOutput += entry.output;
        todayCached += entry.cached;
        todayCost += entry.cost;
      }
      if (usageEntryInLast7Days(entry.day, todayIso)) {
        weekTotal += entry.total;
        weekInput += entry.input;
        weekOutput += entry.output;
        weekCached += entry.cached;
        weekCost += entry.cost;
      }
    }
  }

  el.style.display = 'block';
  grid.style.gridTemplateColumns = 'auto 1fr 1fr 1fr 1fr 1fr';

  const rows = [
    ['Today', todayTotal, todayInput, todayCached, todayOutput, todayCost],
    ['Last 7 Days', weekTotal, weekInput, weekCached, weekOutput, weekCost],
    ['All Time', allTotal, allInput, allCached, allOutput, allCost],
  ];

  const cells = [`
    <span class="label head"></span>
    <span class="value head">Total</span>
    <span class="value head">Input</span>
    <span class="value head">Cached</span>
    <span class="value head">Output</span>
    <span class="value head">Est. Cost</span>
  `];
  for (const [label, total, input, cached, output, cost] of rows) {
    cells.push(`
      <span class="label">${label}</span>
      <span class="value total">${total.toLocaleString()}</span>
      <span class="value">${input.toLocaleString()}</span>
      <span class="value">${cached.toLocaleString()}</span>
      <span class="value">${output.toLocaleString()}</span>
      <span class="value">${formatUsd(cost)}</span>
    `);
  }
  grid.innerHTML = cells.join('');
}

function renderDailyUsage(sessions) {
  const el = document.getElementById('daily-usage-section');
  const grid = document.getElementById('daily-usage-grid');
  if (!el || !grid) return;

  const todayIso = tokenActivityIsoToday();
  const rows = Array.from(buildDailyUsageBuckets(sessions, 'all').values())
    .filter(row => row.total > 0 || row.input > 0 || row.output > 0 || row.cached > 0 || row.cost > 0)
    .sort((a, b) => b.day.localeCompare(a.day))
    .slice(0, DAILY_USAGE_ROW_LIMIT);

  if (rows.length === 0) {
    el.style.display = 'none';
    grid.innerHTML = '';
    return;
  }

  el.style.display = 'block';
  grid.style.gridTemplateColumns = 'auto 0.75fr 1fr 1fr 1fr 1fr 1fr';

  const cells = [`
    <span class="label head"></span>
    <span class="value head">Sessions</span>
    <span class="value head">Total</span>
    <span class="value head">Input</span>
    <span class="value head">Cached</span>
    <span class="value head">Output</span>
    <span class="value head">Est. Cost</span>
  `];
  for (const row of rows) {
    const isToday = row.day === todayIso;
    cells.push(`
      <span class="label${isToday ? ' today' : ''}" title="${escapeHtml(row.day)}">${escapeHtml(formatDailyUsageDay(row.day, todayIso))}</span>
      <span class="value">${exactNumber(row.sessions)}</span>
      <span class="value total">${exactNumber(row.total)}</span>
      <span class="value">${exactNumber(row.input)}</span>
      <span class="value">${exactNumber(row.cached)}</span>
      <span class="value">${exactNumber(row.output)}</span>
      <span class="value">${formatUsd(row.cost, 4)}</span>
    `);
  }
  grid.innerHTML = cells.join('');
}

function renderAgentUsage(sessions) {
  const el = document.getElementById('agent-usage-section');
  const grid = document.getElementById('agent-usage-grid');
  if (!el || !grid) return;

  const buckets = summarizeUsageByAgent(sessions);
  if (buckets.length === 0) {
    el.style.display = 'none';
    grid.innerHTML = '';
    return;
  }

  el.style.display = 'block';
  grid.style.gridTemplateColumns = 'minmax(90px, auto) minmax(72px, auto) 0.75fr 1fr 1fr 1fr 1fr 1fr';

  const cells = [`
    <span class="label head">Agent</span>
    <span class="label head">Period</span>
    <span class="value head">Sessions</span>
    <span class="value head">Total</span>
    <span class="value head">Input</span>
    <span class="value head">Cached</span>
    <span class="value head">Output</span>
    <span class="value head">Est. Cost</span>
  `];
  for (const b of buckets) {
    const unpriced = b.unpricedTokens > 0
      ? `<span class="unpriced">+ ${b.unpricedTokens.toLocaleString()} tokens not priced</span>`
      : '';
    cells.push(`
      <span class="label">${escapeHtml(b.label)}</span>
      <span class="label period">${escapeHtml(b.periodLabel)}</span>
      <span class="value">${b.sessions.toLocaleString()}</span>
      <span class="value total">${b.total.toLocaleString()}</span>
      <span class="value">${b.input.toLocaleString()}</span>
      <span class="value">${b.cached.toLocaleString()}</span>
      <span class="value">${b.output.toLocaleString()}</span>
      <span class="value">${formatUsd(b.cost)}${unpriced}</span>
    `);
  }
  grid.innerHTML = cells.join('');
}

function renderDiskUsage(sessions) {
  const el = document.getElementById('disk-usage-section');
  const grid = document.getElementById('disk-usage-grid');
  if (!el || !grid) return;

  let totalRecordings = 0, totalFrames = 0, totalTurns = 0, totalLogs = 0, totalBytes = 0;
  for (const s of sessions) {
    totalRecordings += s.recording_bytes || 0;
    totalFrames += s.frames_bytes || 0;
    totalTurns += s.turns_bytes || 0;
    totalLogs += s.logs_bytes || 0;
    totalBytes += s.total_bytes || 0;
  }

  if (totalBytes === 0) {
    el.style.display = 'none';
    grid.innerHTML = '';
    return;
  }

  el.style.display = 'block';
  grid.style.gridTemplateColumns = 'auto 1fr';

  const rows = [
    ['Recordings', totalRecordings],
    ['Frames', totalFrames],
    ['Turns', totalTurns],
    ['Logs', totalLogs],
  ];

  const cells = [];
  for (const [label, bytes] of rows) {
    if (bytes === 0) continue;
    cells.push(`
      <span class="label">${label}</span>
      <span class="value">${_fmtBytes(bytes)}</span>
    `);
  }
  cells.push(`
    <span class="label total-row">Total</span>
    <span class="value total-row">${_fmtBytes(totalBytes)}</span>
  `);
  grid.innerHTML = cells.join('');
}

// Re-render the stats sections after an in-section interaction (view or
// agent toggle, heatmap day select). If the session cache has no entry
// for the current host — a reload or transport reconnect raced the click
// — refetch instead of letting the cache-miss path blank the whole
// section stack under the user's cursor.
function rerenderStatsSectionsOrReload() {
  const hostKey = currentStatsHostKey();
  if (renderCachedStatsSessionSections(hostKey)) return;
  loadAllSessionsUsage(hostKey);
}

function selectTokenActivityDayFromCell(cell) {
  if (!cell || cell.classList.contains('future')) return;
  const day = cell.dataset.day;
  if (!day) return;
  tokenActivitySelectedDay = day;
  rerenderStatsSectionsOrReload();
}

document.getElementById('token-activity-agent')?.addEventListener('click', (event) => {
  const btn = event.target.closest('button[data-agent]');
  if (!btn || btn.disabled) return;
  tokenActivityAgent = btn.dataset.agent || 'codex';
  rerenderStatsSectionsOrReload();
});

document.getElementById('token-activity-view')?.addEventListener('click', (event) => {
  const btn = event.target.closest('button[data-view]');
  if (!btn || btn.disabled) return;
  tokenActivityView = btn.dataset.view || 'daily';
  rerenderStatsSectionsOrReload();
});

document.getElementById('token-activity-heatmap')?.addEventListener('click', (event) => {
  selectTokenActivityDayFromCell(event.target.closest('.token-activity-day[data-day]'));
});

document.getElementById('token-activity-heatmap')?.addEventListener('keydown', (event) => {
  if (event.key !== 'Enter' && event.key !== ' ') return;
  const cell = event.target.closest('.token-activity-day[data-day]');
  if (!cell) return;
  event.preventDefault();
  selectTokenActivityDayFromCell(cell);
});

function focusActivityForSessionEvent(options = {}) {
  const force = !!options.force;
  const route = parseRoute();
  if (route.tab === 'activity') {
    if (activeTab !== 'activity') switchTab('activity');
    hideBadge('activity');
    return;
  }
  if (!force) {
    showBadge('activity', '\u2022');
    return;
  }
  if (document.visibilityState === 'visible' && document.hasFocus()) {
    routeTo('activity');
    hideBadge('activity');
  } else {
    showBadge('activity', '\u2022');
  }
}

