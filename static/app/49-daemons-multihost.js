// ── Daemons (multi-host) ──
//
// The self host (the daemon serving this page) is always rendered
// first, followed by any configured secondaries. Connection state is
// tracked locally and updated from WASM's `on_secondary_state` callback.

// Restore pure-client UI toggles from localStorage and wire change
// listeners so subsequent flips are persisted immediately. Called
// once at startup.
function restoreClientToggles() {
  // Direct-mode checkbox — persists between refreshes instead of
  // snapping back to unchecked. Stored as "true" / "false" strings.
  const directToggle = document.getElementById('direct-mode-toggle');
  if (directToggle) {
    directToggle.checked = localStorage.getItem(DIRECT_MODE_KEY) === 'true';
    directToggle.addEventListener('change', () => {
      localStorage.setItem(DIRECT_MODE_KEY, String(directToggle.checked));
    });
  }

  // Log verbosity — the Activity-tab dropdown. WASM's AppState resets
  // to "normal" on every page load, so we restore the stored value
  // AND push it through to WASM via set_verbosity so log filtering
  // matches the dropdown immediately.
  const verbSelect = document.getElementById('verbosity-select');
  if (verbSelect) {
    const stored = localStorage.getItem(VERBOSITY_KEY);
    if (stored && ['normal', 'verbose', 'debug'].includes(stored)) {
      applyDashboardVerbosity(stored, { dispatch: false });
    }
    // Note: the 'change' listener that calls app.set_verbosity is
    // wired separately below (see existing handler). We extend it
    // rather than duplicate to keep a single source of truth.
  }

  // Sessions filters are UI preferences; restore them locally so reloads keep
  // the same list shape without writing anything to intendant.toml.
  restoreSessionMultiFilter('source');
  restoreSessionMultiFilter('status');
  restoreSessionMultiFilter('deep-source');
  restoreSessionMultiFilter('deep-status');
  const showSubagentsToggle = document.getElementById('sessions-show-subagents');
  if (showSubagentsToggle) {
    showSubagentsToggle.checked = localStorage.getItem(SESSIONS_SHOW_SUBAGENTS_KEY) === 'true';
  }

  // Host filter (Activity tab). The dropdown's option list is built
  // later by refreshHostFilterOptions() — that function reads from
  // `activeHostFilter` which we've already initialized from storage
  // above, so the correct option gets marked selected once the
  // options exist.
}

// Convert a base URL (what the user enters) to a WebSocket URL pointing
// at the same origin. `https://` → `wss://`, `http://` → `ws://`.
// Preserves host + port and strips any trailing slash. Returns null
// for anything that isn't a recognizable http(s) URL.
function baseToWsUrl(baseUrl) {
  const trimmed = baseUrl.trim().replace(/\/$/, '');
  if (trimmed.startsWith('https://')) return trimmed.replace(/^https:\/\//, 'wss://');
  if (trimmed.startsWith('http://'))  return trimmed.replace(/^http:\/\//,  'ws://');
  return null;
}

// Reverse of baseToWsUrl: derive the HTTP base from a WS transport
// URL. Used to construct API/card URLs from the ws_url the server
// returns in GET /api/peers.
function wsUrlToBaseUrl(wsUrl) {
  if (!wsUrl) return null;
  return wsUrl
    .replace(/^wss:\/\//, 'https://')
    .replace(/^ws:\/\//, 'http://')
    .replace(/\/ws$/, '');
}

// Build the `iceServers` array for `new RTCPeerConnection({ iceServers })`
// from `gatewayConfig.ice_servers` (the [webrtc].ice_servers TOML config,
// surfaced by the daemon via /.well-known/agent-card.json + /config). Used
// by both the local primary-display path (DisplaySlot.connect) and the
// peer-display federation path (PeerDisplayConnection.connect) so the two
// can't drift in what they advertise to the browser's ICE agent. Default
// is empty — trust-the-network, which is what local LAN deployments
// without STUN/TURN want.
//
// Empty-string username/credential are filtered out (matches the truthy
// check `if (s.username)` rather than a strict null check). Browsers don't
// treat empty-string TURN credentials as "no credential": depending on the
// implementation they silently fail auth or refuse to gather candidates
// from that server. This helper keeps the gating in one place.
//
// Canonical Rust mirror: `ice_servers_to_rtc_peer_connection_config` in
// `src/bin/caller/display/forward.rs` carries the unit tests for the
// corner cases this function must also handle (empty list, empty creds,
// multiple URLs, multiple servers, JSON-shape contract).
function buildIceServersFromGatewayConfig(config) {
  if (!config || !Array.isArray(config.ice_servers)) return [];
  return config.ice_servers.map(s => {
    const entry = { urls: s.urls };
    if (s.username) entry.username = s.username;
    if (s.credential) entry.credential = s.credential;
    return entry;
  });
}

// True when any iceServers entry has at least one `turn:` or `turns:`
// URL — i.e. an ACTUAL TURN relay is configured (vs STUN-only or empty).
// Used by the federated peer-display path to decide whether forcing
// `iceTransportPolicy: 'relay'` is safe: with a real TURN server we
// constrain ICE to relay candidates (the only path that completes
// DTLS in the current rtc-0.9-on-ICE-TCP environment); without one,
// forcing `relay` would guarantee ICE failure since no relay
// candidate exists to pair against. The local DisplaySlot.connect
// path does NOT call this helper — local display should never be
// forced through TURN.
function hasTurnInIceServers(iceServers) {
  for (const server of iceServers || []) {
    const urls = Array.isArray(server.urls)
      ? server.urls
      : (typeof server.urls === 'string' ? [server.urls] : []);
    for (const url of urls) {
      if (typeof url === 'string' &&
          (url.startsWith('turn:') || url.startsWith('turns:'))) {
        return true;
      }
    }
  }
  return false;
}

function dashboardShouldDropDuplicateServerMessage(message) {
  if (!dashboardControlEventsActive) return false;
  if (!message || typeof message !== 'object') return false;
  if (!dashboardMessageCanBeDeduped(message)) return false;
  const key = dashboardServerMessageDedupeKey(message);
  if (!key) return false;
  const now = Date.now();
  for (const [recentKey, seenAt] of dashboardRecentServerMessageKeys) {
    if (now - seenAt > DASHBOARD_EVENT_DEDUPE_MS) {
      dashboardRecentServerMessageKeys.delete(recentKey);
    }
  }
  if (dashboardRecentServerMessageKeys.has(key)) return true;
  dashboardRecentServerMessageKeys.set(key, now);
  return false;
}

function dashboardMessageCanBeDeduped(message) {
  if (!message || typeof message !== 'object') return false;
  const delivery = String(message.delivery || message.delivery_class || message.deliveryClass || '').trim().toLowerCase();
  if (delivery === 'lossless') return false;
  // Dedupe only serde-tagged OutboundEvent payloads. Transport/control frames
  // use `t` and may be streams or signaling messages where identical payloads
  // are meaningful.
  if (message.t !== undefined) return false;
  const eventName = String(message.event || '');
  return DASHBOARD_DEDUPABLE_EVENT_NAMES.has(eventName);
}

function dashboardServerMessageDedupeKey(message) {
  const eventId = String(message?.event_id || message?.eventId || '').trim();
  if (eventId) {
    const eventName = String(message?.event || '');
    const sessionId = String(message?.session_id || message?.sessionId || '');
    return ['event-id', eventName, sessionId, eventId].join('\u001f');
  }
  try {
    return JSON.stringify(message);
  } catch {
    return '';
  }
}

function dashboardControlTransportEnabled() {
  if (DASHBOARD_CONNECT_MODE) return true;
  // macOS app over mTLS: the legacy WebSocket cannot present a client
  // certificate, so the control transport is the only working event
  // path — always on (localStorage is also unreliable on the custom
  // intendant:// scheme, so this cannot be a stored preference there).
  if (window.__intendantPort && window.__intendantBackendTls) return true;
  try {
    return localStorage.getItem(DASHBOARD_TRANSPORT_KEY) === 'webrtc-control';
  } catch {
    return false;
  }
}

function dashboardConnectModeEnabled() {
  return DASHBOARD_CONNECT_MODE;
}

function dashboardConnectSignalUrl(path) {
  const normalized = String(path || '');
  if (!DASHBOARD_CONNECT_SIGNALING_BASE) return normalized;
  return `${DASHBOARD_CONNECT_SIGNALING_BASE}${normalized.startsWith('/') ? '' : '/'}${normalized}`;
}

function dashboardSetControlLastError(message = '', kind = '') {
  dashboardControlLastError = String(message || '').trim();
  dashboardControlLastErrorKind = dashboardControlLastError ? String(kind || '').trim() : '';
}

// ── Virtual display availability ──
// Virtual displays are a host capability (Xvfb-based, Linux-only): derive
// the "New virtual display" affordances from the daemon instead of offering
// a button that can only fail on macOS/Windows hosts. Two sources, because
// each covers a transport the other can't: the displays payload reaches
// direct dashboards over HTTP, and the dashboard-control status capability
// reaches Connect dashboards once the channel is up (the HTTP probe is
// impossible there, so dashboardUpdateTransportStatus re-applies the gate
// whenever transport state changes). Declared HERE, before that function's
// first eval-time call — a later-fragment `let` would be a TDZ trap that
// kills the whole module.
let daemonVirtualDisplaysAvailable = null;
function virtualDisplaysAvailableNow() {
  return daemonVirtualDisplaysAvailable === true ||
    dashboardControlTransport?.lastStatus?.virtual_displays_available === true;
}
function updateVirtualDisplayAvailabilityUi() {
  const btn = document.getElementById('displays-create-virtual');
  if (btn) btn.hidden = !virtualDisplaysAvailableNow();
}
async function refreshVirtualDisplayAvailability() {
  if (!dashboardConnectModeEnabled()) {
    try {
      const payload = await fetchLocalDisplaysPayload();
      daemonVirtualDisplaysAvailable = payload?.virtual_displays_available === true;
    } catch (_) {
      daemonVirtualDisplaysAvailable = null;
    }
  }
  updateVirtualDisplayAvailabilityUi();
}

function dashboardUpdateTransportStatus() {
  const group = document.getElementById('sb-dashboard-transport');
  const dot = document.getElementById('sb-dashboard-transport-dot');
  const label = document.getElementById('sb-dashboard-transport-label');
  const prefix = document.getElementById('sb-dashboard-transport-prefix');
  const status = dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: dashboardControlTransportEnabled(), connected: false };
  const summary = dashboardTransportStatusSummary(status);
  if (!group || !dot || !label) {
    renderConnectHealthPanel(status, summary);
    renderDashboardTargetSummaries();
    refreshFilesDownloadAvailability();
    if (activeTab === 'files') refreshFilesTransferJobs();
    maybeOpenShellAfterTransportReady();
    return;
  }
  dot.className = `conn-dot ${summary.kind}`;
  label.textContent = summary.label;
  // Name the actual route so "Ready" is never ambiguous: the user can
  // tell direct mTLS from a hosted Connect tunnel at a glance.
  if (prefix) {
    const route = accessCurrentRouteInfo();
    prefix.textContent = { connect: 'Connect', mtls: 'mTLS', webrtc: 'WebRTC', local: 'local' }[route.kind] || 'access';
  }
  group.title = summary.title;
  renderConnectHealthPanel(status, summary);
  renderDashboardTargetSummaries();
  refreshFilesDownloadAvailability();
  updateVirtualDisplayAvailabilityUi();
  if (activeTab === 'files') refreshFilesTransferJobs();
  maybeOpenShellAfterTransportReady();
}

function dashboardTransportStatusSummary(status = {}) {
  if (!status.enabled) {
    return {
      kind: 'ok',
      label: 'Ready',
      title: 'Dashboard access is ready. Open Connection Diagnostics for transport details.',
    };
  }

  const pcState = String(status.pcState || '').toLowerCase();
  const channelState = String(status.channelState || '').toLowerCase();
  const lastError = String(status.lastError || status.error || '').trim();
  if (status.reconnecting) {
    const reason = String(status.reconnectReason || '').trim();
    return {
      kind: 'warn',
      label: 'reconnecting',
      title: `Dashboard access is reconnecting${reason ? `: ${reason}` : ''}. Open Connection Diagnostics for transport details.`,
    };
  }
  if (lastError || pcState === 'failed' || pcState === 'closed' || channelState === 'closed') {
    return {
      kind: 'err',
      label: 'failed',
      title: `Dashboard access failed${lastError ? `: ${lastError}` : ''}. Open Connection Diagnostics for transport details.`,
    };
  }

  if (status.connected && status.verifiedBinding?.ok && channelState === 'open') {
    const route = String(status.iceRoute || status.route || '').toLowerCase();
    const relayed = route === 'relay';
    return {
      kind: relayed ? 'warn' : 'ok',
      label: relayed ? 'Relay' : 'Ready',
      title: `Dashboard access is ready${relayed ? ' through a relay' : ''}. Events ${status.eventsActive ? 'active' : 'not active'}. Open Connection Diagnostics for transport details.`,
    };
  }

  const phase = status.verifiedBinding?.ok
    ? 'verified; waiting for WebRTC connection'
    : (status.sessionId ? 'answer received; verifying binding' : 'signaling');
  return {
    kind: 'warn',
    label: 'checking',
    title: `Dashboard access is ${phase}. Open Connection Diagnostics for transport details.`,
  };
}

function connectHealthKindForBoolean(value, falseKind = 'warn') {
  if (value === true) return 'ok';
  if (value === false) return falseKind;
  return 'warn';
}

function connectHealthFeatureLabel(value) {
  if (value === true) return 'available';
  if (value === false) return 'unavailable';
  return 'unknown';
}

function connectHealthShortValue(value, head = 10, tail = 6) {
  const text = String(value || '').trim();
  if (!text) return '';
  if (text.length <= head + tail + 3) return text;
  return `${text.slice(0, head)}...${text.slice(-tail)}`;
}

function connectHealthState(statusArg = null, summaryArg = null) {
  const status = statusArg || (dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: dashboardControlTransportEnabled(), connected: false });
  const summary = summaryArg || dashboardTransportStatusSummary(status);
  const channelState = String(status.channelState || '').toLowerCase();
  const pcState = String(status.pcState || '').toLowerCase();
  const verifiedBindingOk = Boolean(status.verifiedBinding?.ok);
  const connected = Boolean(status.connected && verifiedBindingOk && channelState === 'open');
  const connectMode = dashboardConnectModeEnabled();
  const directMode = location.protocol === 'https:' ? 'mTLS' : 'HTTP';
  const modeLabel = connectMode
    ? 'Hosted Connect'
    : (status.enabled ? 'Local WebRTC' : `Direct ${directMode}`);
  const daemonPublicKey = String(
    status.verifiedBinding?.daemonPublicKey ||
    status.claimedDaemonPublicKey ||
    ''
  );
  const features = [
    { id: 'terminal', label: 'Terminal frames', value: status.terminalFramesAvailable },
    { id: 'byte-streams', label: 'Byte streams', value: status.byteStreamsAvailable },
    { id: 'uploads', label: 'Uploads', value: status.uploadFramesAvailable },
    { id: 'fs-read', label: 'Filesystem read', value: status.apiFsReadAvailable },
    { id: 'presence', label: 'Presence frames', value: status.presenceFramesAvailable },
  ];
  const rows = [
    { label: 'Mode', value: modeLabel, kind: connectMode || status.enabled ? 'ok' : 'warn' },
    { label: 'Signaling', value: status.signalingMode || (connectMode ? 'connect-rendezvous' : 'direct'), kind: status.enabled ? 'ok' : 'warn' },
    { label: 'Daemon', value: DASHBOARD_CONNECT_DAEMON_ID || 'self', kind: DASHBOARD_CONNECT_DAEMON_ID || !connectMode ? 'ok' : 'warn' },
    { label: 'Peer connection', value: pcState || 'n/a', kind: connected ? 'ok' : (status.enabled ? 'warn' : 'ok') },
    { label: 'DataChannel', value: channelState || 'n/a', kind: channelState === 'open' ? 'ok' : (status.enabled ? 'warn' : 'ok') },
    { label: 'Binding', value: verifiedBindingOk ? 'verified' : 'not verified', kind: verifiedBindingOk ? 'ok' : (status.enabled ? 'warn' : 'ok') },
    { label: 'Events', value: status.eventsActive ? 'active' : 'inactive', kind: connectHealthKindForBoolean(status.eventsActive) },
    { label: 'ICE route', value: status.iceRoute || 'unknown', kind: status.iceRoute === 'relay' ? 'warn' : (status.iceRoute ? 'ok' : 'warn') },
    { label: 'Candidate pair', value: status.iceCandidatePair || 'unknown', kind: status.iceCandidatePair ? 'ok' : 'warn' },
    { label: 'Pending RPC', value: String(Number(status.pendingRequests || 0)), kind: Number(status.pendingRequests || 0) === 0 ? 'ok' : 'warn' },
    { label: 'Pending bytes', value: String(Number(status.pendingByteStreams || 0)), kind: Number(status.pendingByteStreams || 0) === 0 ? 'ok' : 'warn' },
    { label: 'Completed bytes', value: String(Number(status.completedByteStreams || 0)), kind: 'ok' },
    { label: 'Daemon key', value: connectHealthShortValue(daemonPublicKey) || 'unknown', kind: daemonPublicKey ? 'ok' : 'warn' },
    { label: 'Grant hash', value: connectHealthShortValue(status.sessionGrantSha256) || 'unknown', kind: status.sessionGrantSha256 ? 'ok' : (connectMode ? 'warn' : 'ok') },
    ...features.map(feature => ({
      label: feature.label,
      value: connectHealthFeatureLabel(feature.value),
      kind: connectHealthKindForBoolean(feature.value),
    })),
  ];
  return {
    connectMode,
    enabled: Boolean(status.enabled),
    connected,
    verifiedBindingOk,
    summary,
    status,
    modeLabel,
    daemonPublicKey,
    rows,
    badges: [
      { label: 'Mode', value: modeLabel, kind: connectMode || status.enabled ? 'ok' : 'warn' },
      { label: 'Transport', value: summary.label || 'unknown', kind: summary.kind || 'warn' },
      { label: 'Binding', value: verifiedBindingOk ? 'verified' : 'pending', kind: verifiedBindingOk ? 'ok' : (status.enabled ? 'warn' : 'ok') },
      { label: 'Events', value: status.eventsActive ? 'active' : 'inactive', kind: connectHealthKindForBoolean(status.eventsActive) },
    ],
  };
}

function renderConnectHealthPanel(statusArg = null, summaryArg = null) {
  const summaryEl = document.getElementById('connect-health-summary');
  const gridEl = document.getElementById('connect-health-grid');
  if (!summaryEl || !gridEl) return null;
  // Redraws on every transport tick; while the Access pane is hidden defer
  // one argless redraw (status is re-derived fresh at flush time).
  if (!paneIsVisible('access')) {
    renderOrDefer('access', 'connect-health', () => renderConnectHealthPanel());
    return null;
  }
  const state = connectHealthState(statusArg, summaryArg);
  summaryEl.innerHTML = '';
  for (const badge of state.badges) {
    const el = document.createElement('span');
    el.className = `connect-health-badge ${badge.kind || 'warn'}`;
    const label = document.createElement('span');
    label.textContent = `${badge.label}:`;
    const value = document.createElement('strong');
    value.textContent = badge.value || 'unknown';
    el.append(label, value);
    summaryEl.appendChild(el);
  }
  gridEl.innerHTML = '';
  for (const row of state.rows) {
    const item = document.createElement('div');
    item.className = `connect-health-item ${row.kind || 'warn'}`;
    const label = document.createElement('div');
    label.className = 'connect-health-label';
    label.textContent = row.label;
    const value = document.createElement('div');
    value.className = 'connect-health-value';
    value.textContent = row.value || 'unknown';
    item.append(label, value);
    gridEl.appendChild(item);
  }
  return state;
}

function connectProbePairMatches(value, first, second, recentKeyCount) {
  return Boolean(
    value &&
    value.first === first &&
    value.second === second &&
    value.recentKeyCount === recentKeyCount
  );
}

function connectSelfTestSpecs() {
  return [
    {
      id: 'control-no-replay',
      label: 'Control RPC failures do not replay over WebSocket',
      probe: '_debugProbeControlNoReplay',
      pass: result => result.wsReplayCount === 0 && result.rpcAttempts >= 1 && result.rpcFailureWarnings >= 1,
    },
    {
      id: 'control-unavailable',
      label: 'Unavailable control mutations stay inside Connect',
      probe: '_debugProbeControlUnavailableConnectNoLegacy',
      pass: result => result.wsReplayCount === 0 && result.unavailableWarnings >= 3,
    },
    {
      id: 'media-unavailable',
      label: 'Unavailable media writes do not use legacy transport',
      probe: '_debugProbeMediaConnectNoLegacy',
      pass: result => result.threw === true && result.wsReplayCount === 0,
    },
    {
      id: 'peer-mutations',
      label: 'Peer mutations do not fall back to HTTP',
      probe: '_debugProbePeerMutationConnectNoHttp',
      pass: result => result.threw === true && result.rpcAttempts >= 1 && result.httpFallbackCount === 0,
    },
    {
      id: 'diagnostics',
      label: 'Diagnostics do not fall back to HTTP',
      probe: '_debugProbeDiagnosticsConnectNoHttp',
      pass: result => result.threw === true && result.httpFallbackCount === 0,
    },
    {
      id: 'display-signal',
      label: 'Display signaling does not use legacy fallback',
      probe: '_debugProbeDisplaySignalConnectNoLegacy',
      pass: result => result.wsReplayCount === 0 && result.httpFallbackCount === 0,
    },
    {
      id: 'display-authority',
      label: 'Display authority refuses unavailable legacy paths',
      probe: '_debugProbeDisplayAuthorityConnectNoLegacy',
      pass: result =>
        result.requestResult === false &&
        result.releaseResult === false &&
        result.requestReplayCount === 0 &&
        result.releaseReplayCount === 0,
    },
    {
      id: 'shell-open-ack',
      label: 'Shell input waits for terminal_opened',
      probe: '_debugProbeShellQueuesUntilOpened',
      pass: result =>
        JSON.stringify(result.framesBeforeAck) === JSON.stringify(['terminal_open']) &&
        JSON.stringify(result.framesAfterAck) === JSON.stringify(['terminal_open', 'terminal_resize', 'terminal_input']) &&
        result.queuedBeforeAck === 'queued-before-open' &&
        result.queuedAfterAck === '' &&
        result.ackedBeforeAck === false &&
        result.ackedAfterAck === true,
    },
    {
      id: 'terminal-output-dedupe',
      label: 'Terminal output bypasses event dedupe',
      probe: '_debugProbeTerminalOutputBypassesDedupe',
      pass: result => result.writtenBytes === 2 && result.recentKeyCount === 0,
    },
    {
      id: 'event-dedupe',
      label: 'Event dedupe is limited to replay-safe events',
      probe: '_debugProbeEventDedupeAllowlist',
      pass: result =>
        connectProbePairMatches(result.status, false, true, 1) &&
        connectProbePairMatches(result.sessionIdentity, false, true, 1) &&
        ['terminalOutput', 'displayIce', 'modelDelta', 'logEntry', 'peerEventForwarded', 'futureEvent']
          .every(name => connectProbePairMatches(result[name], false, false, 0)),
    },
    {
      id: 'duplicate-shell-input',
      label: 'Duplicate Shell input frames are both sent',
      probe: '_debugProbeDuplicateShellInputSends',
      pass: result =>
        JSON.stringify(result.inputFrames) === JSON.stringify(['eA==', 'eA==']) &&
        result.queuedAfterSend === '',
    },
    {
      id: 'presence-media',
      label: 'Presence media uses Connect frames',
      probe: '_debugProbePresenceMediaConnectNoLegacy',
      pass: result =>
        result.presenceFrameCount === 2 &&
        result.uploadCount === 1 &&
        result.legacyCount === 0,
    },
    {
      id: 'presence-sender',
      label: 'Presence server sender uses tunneled actions',
      probe: '_debugProbePresenceServerSenderConnectNoLegacy',
      pass: result => result.presenceFrameCount === 2 && result.actionRpcCount === 1,
    },
    {
      id: 'presence-callback',
      label: 'Tunneled presence callbacks reach the browser',
      probe: '_debugProbeTunneledPresenceServerCallback',
      pass: result => result.handled === true && result.diagnosticCount === 1,
    },
  ];
}

function connectSelfTestDetail(result) {
  if (!result || typeof result !== 'object') return '';
  const parts = [];
  for (const [key, value] of Object.entries(result)) {
    if (key === 'skipped' || value === null || value === undefined || typeof value === 'object') continue;
    parts.push(`${key}=${String(value)}`);
    if (parts.length >= 5) break;
  }
  const text = parts.join(' ');
  return text.length > 180 ? `${text.slice(0, 177)}...` : text;
}

function setConnectSelfTestStatus(text, kind = '') {
  const el = document.getElementById('connect-self-test-status');
  if (!el) return;
  el.className = kind || '';
  el.textContent = text || '';
}

function renderConnectSelfTestResults(results = []) {
  const container = document.getElementById('connect-self-test-results');
  if (!container) return;
  container.innerHTML = '';
  for (const result of results) {
    const row = document.createElement('div');
    row.className = `connect-self-test-row ${result.status || 'skip'}`;
    const state = document.createElement('div');
    state.className = 'connect-self-test-state';
    state.textContent = result.status === 'pass' ? 'pass' : (result.status === 'fail' ? 'fail' : 'warn');
    const body = document.createElement('div');
    body.className = 'connect-self-test-body';
    const name = document.createElement('div');
    name.className = 'connect-self-test-name';
    name.textContent = result.label || result.id || 'Probe';
    body.appendChild(name);
    if (result.detail) {
      const detail = document.createElement('div');
      detail.className = 'connect-self-test-detail';
      detail.textContent = result.detail;
      body.appendChild(detail);
    }
    row.append(state, body);
    container.appendChild(row);
  }
}

async function runConnectSelfTests(options = {}) {
  const render = options.render !== false;
  const control = window.intendantDashboardControl || {};
  const specs = connectSelfTestSpecs();
  const button = document.getElementById('connect-self-test-btn');
  if (render) {
    renderConnectHealthPanel();
    renderConnectSelfTestResults([]);
    setConnectSelfTestStatus('Running...', 'warn');
    if (button) button.disabled = true;
  }
  const results = [];
  try {
    for (const spec of specs) {
      let raw = null;
      let status = 'fail';
      let detail = '';
      if (typeof control[spec.probe] !== 'function') {
        detail = 'probe=missing';
      } else {
        try {
          raw = await control[spec.probe]();
          if (raw?.skipped === true) {
            status = 'skip';
            detail = connectSelfTestDetail(raw) || 'skipped=true';
          } else if (spec.pass(raw || {})) {
            status = 'pass';
            detail = connectSelfTestDetail(raw);
          } else {
            detail = connectSelfTestDetail(raw) || 'unexpected result';
          }
        } catch (err) {
          detail = err?.message || String(err);
        }
      }
      results.push({
        id: spec.id,
        label: spec.label,
        probe: spec.probe,
        status,
        detail,
        result: raw,
      });
      if (render) {
        const passedSoFar = results.filter(item => item.status === 'pass').length;
        setConnectSelfTestStatus(`${passedSoFar}/${specs.length} passed`, status === 'fail' ? 'err' : 'warn');
        renderConnectSelfTestResults(results);
      }
    }
  } finally {
    if (render && button) button.disabled = false;
  }
  const passed = results.filter(item => item.status === 'pass').length;
  const skipped = results.filter(item => item.status === 'skip').length;
  const failed = results.filter(item => item.status === 'fail').length;
  const summary = {
    ok: failed === 0 && skipped === 0,
    total: results.length,
    passed,
    skipped,
    failed,
    results,
    state: connectHealthState(),
  };
  if (render) {
    const kind = failed > 0 ? 'err' : (skipped > 0 ? 'warn' : 'ok');
    const suffix = skipped > 0 ? `, ${skipped} skipped` : '';
    setConnectSelfTestStatus(`${passed}/${results.length} passed${suffix}`, kind);
    renderConnectHealthPanel();
  }
  return summary;
}

class DashboardTransport {
  enabled() {
    return dashboardControlTransportEnabled();
  }

  startControl() {
    if (!this.enabled()) return Promise.resolve(false);
    if (!window.RTCPeerConnection) return Promise.reject(new Error('RTCPeerConnection is unavailable'));
    if (dashboardControlTransport) return this.controlStartPromise || Promise.resolve(true);
    dashboardControlTransport = new DashboardControlTransport();
    dashboardSetControlLastError('');
    dashboardUpdateTransportStatus();
    this.controlStartPromise = dashboardControlTransport.connect().then(() => true).catch(err => {
      console.warn('[dashboard-control] connect failed', err);
      dashboardSetControlLastError(err?.message || String(err), err?.controlErrorKind || '');
      if (dashboardControlTransport) {
        dashboardControlTransport.lastError = dashboardControlLastError;
        dashboardControlTransport.lastErrorKind = dashboardControlLastErrorKind;
      }
      dashboardUpdateTransportStatus();
      dashboardControlTransport = null;
      this.controlStartPromise = null;
      if (dashboardConnectModeEnabled()) throw err;
      return false;
    });
    return this.controlStartPromise;
  }

  enableControl() {
    dashboardSetControlLastError('');
    if (dashboardConnectModeEnabled()) return;
    localStorage.setItem(DASHBOARD_TRANSPORT_KEY, 'webrtc-control');
    location.reload();
  }

  disableControl() {
    dashboardSetControlLastError('');
    if (dashboardConnectModeEnabled()) return;
    localStorage.removeItem(DASHBOARD_TRANSPORT_KEY);
    dashboardControlTransport?.close();
    dashboardControlTransport = null;
    this.controlStartPromise = null;
    dashboardUpdateTransportStatus();
    location.reload();
  }

  status() {
    const reconnect = dashboardConnectReconnectStatus();
    return dashboardControlTransport?.debugStatus() || {
      enabled: this.enabled(),
      connected: false,
      lastError: dashboardControlLastError,
      lastErrorKind: dashboardControlLastErrorKind,
      ...reconnect,
    };
  }

  canUseRpc() {
    return Boolean(dashboardControlTransport?.canUseRpc());
  }

  canUseDisplayInputAuthority() {
    return Boolean(
      this.canUseRpc() &&
      dashboardControlTransport?.lastStatus?.api_display_input_authority_available === true
    );
  }

  canUseDisplayWebRtcSignal() {
    return Boolean(
      this.canUseRpc() &&
      dashboardControlTransport?.lastStatus?.api_display_webrtc_signal_available === true
    );
  }

  canUsePeerFileTransferSignal() {
    return Boolean(
      this.canUseRpc() &&
      dashboardControlTransport?.lastStatus?.api_peer_file_transfer_signal_available === true
    );
  }

  canUsePeerDashboardControlSignal() {
    return Boolean(
      this.canUseRpc() &&
      dashboardControlTransport?.lastStatus?.api_peer_dashboard_control_signal_available === true
    );
  }

  request(method, params = {}, options = {}) {
    if (!this.canUseRpc()) {
      return Promise.reject(new Error('dashboard control RPC is not connected'));
    }
    const request = dashboardControlTransport.request(method, params, options);
    if (method === 'status') {
      return request.then(status => {
        if (status && typeof status === 'object') {
          dashboardControlTransport.lastStatus = status;
          dashboardUpdateTransportStatus();
        }
        return status;
      });
    }
    return request;
  }

  requestBytes(method, params = {}, options = {}) {
    if (!this.canUseRpc()) {
      return Promise.reject(new Error('dashboard control byte stream is not connected'));
    }
    return dashboardControlTransport.requestBytes(method, params, options);
  }

  uploadBytes(method, params = {}, bytes, options = {}) {
    if (!this.canUseRpc()) {
      return Promise.reject(new Error('dashboard control upload is not connected'));
    }
    return dashboardControlTransport.uploadBytes(method, params, bytes, options);
  }

  terminalFrame(frame) {
    if (!this.canUseRpc()) return false;
    return dashboardControlTransport.terminalFrame(frame);
  }

  presenceFrame(frame) {
    if (!this.canUseRpc()) return false;
    return dashboardControlTransport.presenceFrame(frame);
  }

  displayInput(displayId, event) {
    if (!this.canUseDisplayInputAuthority()) return false;
    return dashboardControlTransport.displayInput(displayId, event);
  }

  displayAuthoritySnapshot(options = {}) {
    if (!this.canUseDisplayInputAuthority()) {
      return Promise.reject(new Error('dashboard control display authority is not available'));
    }
    return dashboardControlTransport.request(
      'api_display_input_authority_snapshot',
      {},
      options
    );
  }

  requestDisplayInputAuthority(displayId, options = {}) {
    if (!this.canUseDisplayInputAuthority()) {
      return Promise.reject(new Error('dashboard control display authority is not available'));
    }
    return dashboardControlTransport.request(
      'api_display_input_authority_request',
      { display_id: Number(displayId) || 0 },
      options
    );
  }

  releaseDisplayInputAuthority(displayId, options = {}) {
    if (!this.canUseDisplayInputAuthority()) {
      return Promise.reject(new Error('dashboard control display authority is not available'));
    }
    return dashboardControlTransport.request(
      'api_display_input_authority_release',
      { display_id: Number(displayId) || 0 },
      options
    );
  }

  displayWebRtcSignal(params = {}, options = {}) {
    if (!this.canUseDisplayWebRtcSignal()) {
      return Promise.reject(new Error('dashboard control display signaling is not available'));
    }
    return dashboardControlTransport.request(
      'api_display_webrtc_signal',
      params,
      options
    );
  }

  // The two peer signal relays (transport F5). Signaling is a
  // delivered-once mutation: the facade derives no-replay from the POST
  // verb (§3.7 — never replayed over HTTP after a tunnel attempt that MAY
  // have reached the daemon, the exact legacy fallbackAfterRpcFailure:false
  // semantics; Connect mode never uses HTTP), while a dashboard with no
  // tunnel at all still signals over direct HTTP — that pre-attempt lane
  // is how peer tunnels bootstrap on direct/mTLS dashboards. Both return
  // the facade envelope {ok, status, body}.
  peerFileTransferSignal(peerId, params = {}, options = {}) {
    const id = String(peerId || '').trim();
    if (!id) return Promise.reject(new Error('peer id is required'));
    return daemonApi.request(
      'api_peer_file_transfer_signal',
      { peer_id: id, ...params },
      { signal: options.signal }
    );
  }

  peerDashboardControlSignal(peerId, params = {}, options = {}) {
    const id = String(peerId || '').trim();
    if (!id) return Promise.reject(new Error('peer id is required'));
    return daemonApi.request(
      'api_peer_dashboard_control_signal',
      { peer_id: id, ...params },
      { signal: options.signal }
    );
  }

  stream(method, params = {}, options = {}, onEvent = {}) {
    if (!this.canUseRpc()) {
      return Promise.reject(new Error('dashboard control stream is not connected'));
    }
    return dashboardControlTransport.stream(method, params, options, onEvent);
  }

  async rpcOrHttp(method, params, fallback, label = method, options = {}) {
    if (this.canUseRpc()) {
      try {
        return await dashboardControlTransport.request(
          method,
          params || {},
          { signal: options.signal }
        );
      } catch (err) {
        if (err?.name === 'AbortError') throw err;
        if (dashboardConnectModeEnabled()) throw err;
        console.warn(`[dashboard-control] ${label} RPC failed, falling back to HTTP`, err);
      }
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error(`dashboard Connect RPC is not available for ${label}`);
    }
    return fallback();
  }

  async jsonFetch(method, params, fallback, label = method, options = {}) {
    if (this.canUseRpc()) {
      try {
        const payload = await dashboardControlTransport.request(
          method,
          params || {},
          { signal: options.signal }
        );
        return this.responseFromPayload(payload);
      } catch (err) {
        if (err?.name === 'AbortError') throw err;
        if (options.fallbackAfterRpcFailure === false) throw err;
        if (dashboardConnectModeEnabled()) throw err;
        console.warn(`[dashboard-control] ${label} RPC failed, falling back to HTTP`, err);
      }
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error(`dashboard Connect RPC is not available for ${label}`);
    }
    return fallback();
  }

  responseFromPayload(payload) {
    const rawStatus = Number(payload?._httpStatus);
    const status = Number.isFinite(rawStatus) && rawStatus >= 100 && rawStatus <= 599
      ? rawStatus
      : 200;
    const ok = typeof payload?._httpOk === 'boolean'
      ? payload._httpOk
      : status >= 200 && status < 300;
    let body = payload;
    if (body && typeof body === 'object' && !Array.isArray(body)) {
      body = { ...body };
      delete body._httpStatus;
      delete body._httpOk;
    }
    return {
      ok,
      status,
      statusText: ok ? 'OK' : 'Error',
      headers: typeof Headers === 'function'
        ? new Headers({ 'content-type': 'application/json' })
        : null,
      dashboardControl: true,
      json: async () => body,
      text: async () => {
        try { return JSON.stringify(body); } catch { return ''; }
      },
    };
  }
}

dashboardTransport = new DashboardTransport();
dashboardUpdateTransportStatus();
refreshVirtualDisplayAvailability();

function maybeStartDashboardControlTransport() {
  return dashboardTransport.startControl();
}

async function waitForDashboardControlReady(timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (dashboardTransport?.canUseRpc?.()) return true;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  // Signaling already succeeded by this point (the offer resolved to a
  // verified answer) — what never happened is the WebRTC transport itself.
  const err = new Error('dashboard Connect transport did not become ready');
  err.controlErrorKind = 'transport';
  throw err;
}

async function hydrateDashboardFromControl() {
  if (!dashboardTransport?.canUseRpc?.()) {
    const err = new Error('dashboard Connect transport is not connected');
    err.controlErrorKind = 'transport';
    throw err;
  }
  const [status, config, card, accessOverview, targets] = await Promise.all([
    dashboardTransport.request('status', {}, { timeoutMs: 15000 }).catch(() => null),
    dashboardTransport.request('config', {}, { timeoutMs: 15000 }).catch(() => ({})),
    dashboardTransport.request('api_agent_card', {}, { timeoutMs: 15000 }).catch(() => null),
    dashboardTransport.request('api_access_overview', {}, { timeoutMs: 15000 }).catch(() => null),
    dashboardTransport.request('api_dashboard_targets', {}, { timeoutMs: 15000 }).catch(() => null),
  ]);
  if (status && typeof status === 'object' && dashboardControlTransport) {
    dashboardControlTransport.lastStatus = status;
  }
  applyGatewayConfig(config || {});
  applyAgentCardIdentity(card);
  if (accessOverview) applyAccessOverview(accessOverview);
  else if (targets) applyDashboardAccessTargets(targets);
  const bootstrap = await dashboardTransport.request('api_dashboard_bootstrap', {}, {
    timeoutMs: 60000,
  }).catch(err => {
    console.warn('[dashboard-control] dashboard bootstrap RPC failed', err);
    return null;
  });
  const frames = Array.isArray(bootstrap?.frames) ? bootstrap.frames : [];
  for (const frame of frames) {
    if (dashboardServerMessageDispatcher) dashboardServerMessageDispatcher(frame);
  }
  dashboardUpdateTransportStatus();
}

function dashboardConnectReconnectStatus() {
  return {
    reconnecting: dashboardConnectReconnectInFlight || Boolean(dashboardConnectReconnectTimer),
    reconnectAttempt: dashboardConnectReconnectAttempt,
    reconnectReason: dashboardConnectReconnectReason,
    reconnectNextUnixMs: dashboardConnectReconnectNextUnixMs,
  };
}

function scheduleDashboardConnectReconnect(reason = 'dashboard transport disconnected', options = {}) {
  if (!dashboardConnectModeEnabled()) return;
  if (dashboardConnectReconnectTimer) return;
  const attempt = dashboardConnectReconnectAttempt;
  const backoffMs = Math.min(
    DASHBOARD_CONNECT_RECONNECT_MAX_MS,
    DASHBOARD_CONNECT_RECONNECT_MIN_MS * Math.pow(2, Math.min(attempt, 5))
  );
  const delayMs = Math.max(0, Number(options.delayMs ?? backoffMs) || 0);
  dashboardConnectReconnectAttempt = attempt + 1;
  dashboardConnectReconnectReason = String(reason || 'dashboard transport disconnected');
  dashboardConnectReconnectNextUnixMs = Date.now() + delayMs;
  dashboardSetControlLastError('');
  setConnectEventStatus('warn', 'Reconnecting dashboard events through Hosted Connect');
  dashboardUpdateTransportStatus();
  dashboardConnectReconnectTimer = window.setTimeout(() => {
    dashboardConnectReconnectTimer = null;
    dashboardConnectReconnectNextUnixMs = 0;
    runDashboardConnectReconnect(dashboardConnectReconnectReason).catch(err => {
      console.warn('[dashboard-control] Hosted Connect reconnect task failed', err);
    });
  }, delayMs);
}

async function runDashboardConnectReconnect(reason = 'dashboard transport disconnected') {
  if (!dashboardConnectModeEnabled() || dashboardConnectReconnectInFlight) return;
  dashboardConnectReconnectInFlight = true;
  dashboardConnectReconnectReason = String(reason || 'dashboard transport disconnected');
  let retryReason = '';
  dashboardSetControlLastError('');
  setConnectEventStatus('warn', 'Reconnecting dashboard events through Hosted Connect');
  dashboardUpdateTransportStatus();
  try {
    const previous = dashboardControlTransport;
    if (previous) {
      previous.suppressReconnect = true;
      previous.close({ signalRemote: true, suppressReconnect: true });
    }
    dashboardControlTransport = null;
    if (dashboardTransport) dashboardTransport.controlStartPromise = null;
    await maybeStartDashboardControlTransport();
    await waitForDashboardControlReady(30000);
    await hydrateDashboardFromControl();
    dashboardConnectReconnectAttempt = 0;
    dashboardConnectReconnectReason = '';
    dashboardConnectReconnectNextUnixMs = 0;
    dashboardSetControlLastError('');
    setConnectEventStatus('ok', 'Dashboard events are live through verified Hosted Connect');
  } catch (err) {
    retryReason = err?.message || String(err);
    console.warn('[dashboard-control] Hosted Connect reconnect failed', err);
    dashboardSetControlLastError(retryReason, err?.controlErrorKind || '');
    setConnectEventStatus('err', 'Hosted Connect dashboard reconnect failed');
  } finally {
    dashboardConnectReconnectInFlight = false;
    dashboardUpdateTransportStatus();
    if (retryReason) scheduleDashboardConnectReconnect(retryReason);
  }
}

window.intendantDashboardControl = {
  enable() {
    dashboardTransport.enableControl();
  },
  disable() {
    dashboardTransport.disableControl();
  },
  status() {
    return dashboardTransport.status();
  },
  _debugClientIdentity() {
    return clientIdentityGet();
  },
  _debugOrgGrantStore(doc) {
    return orgGrantStore(doc);
  },
  _debugFleetEncryptRecord(record) {
    return accessFleetEncryptRecord(record);
  },
  _debugFleetDecryptRecord(record) {
    return accessFleetDecryptRecord(record);
  },
  connectHealth() {
    return connectHealthState();
  },
  runConnectSelfTest(options = {}) {
    return runConnectSelfTests(options);
  },
  async _debugProbePeerDashboardControl(peerId, options = {}) {
    const id = String(peerId || '').trim();
    if (!id) throw new Error('peer id is required');
    const conn = await peerDashboardControlConnectionForHost(id, {
      timeoutMs: options.timeoutMs || 60000,
    });
    const status = await conn.request('status', {}, { timeoutMs: options.timeoutMs || 60000 });
    const sessions = await fetchSessionsForHost(id, {
      force: true,
      limit: options.limit || 2,
      cacheSessionMetadata: false,
    });
    const streamEvents = [];
    const streamedSessions = await streamSessionsForHost(id, { limit: options.limit || 2 }, {
      sessions(rows, partial) {
        streamEvents.push(partial ? 'session_partial' : 'session');
      },
      replace(rows) {
        streamEvents.push('replace');
      },
      phase(phase) {
        streamEvents.push('phase:' + String(phase || ''));
      },
      done() {
        streamEvents.push('done');
      },
    });

    const terminal = options.terminal === false ? { skipped: true } : await (async () => {
      const terminalId = `peer-dashboard-terminal-${Date.now()}`;
      const token = 'peer_dashboard_terminal_e2e';
      const frames = [];
      const handler = event => frames.push(event.detail || {});
      const waitForFrame = (predicate, label) => new Promise((resolve, reject) => {
        const started = Date.now();
        const tick = () => {
          const found = frames.find(predicate);
          if (found) {
            resolve(found);
            return;
          }
          if (Date.now() - started > (options.timeoutMs || 60000)) {
            reject(new Error(`peer terminal ${label} timed out`));
            return;
          }
          setTimeout(tick, 25);
        };
        tick();
      });
      window.addEventListener('intendant-dashboard-terminal-frame', handler);
      try {
        if (!conn.terminalFrame({
          t: 'terminal_open',
          host_id: id,
          terminal_id: terminalId,
          cols: 80,
          rows: 24,
        })) {
          throw new Error('peer terminal_open was not sent');
        }
        await waitForFrame(frame => (
          frame.t === 'terminal_opened' &&
          frame.host_id === id &&
          frame.terminal_id === terminalId
        ), 'open');
        if (!conn.terminalFrame({
          t: 'terminal_input',
          host_id: id,
          terminal_id: terminalId,
          data: btoa(`printf '${token}\\n'\r`),
        })) {
          throw new Error('peer terminal_input was not sent');
        }
        const output = await waitForFrame(frame => {
          if (
            frame.t !== 'terminal_output' ||
            frame.host_id !== id ||
            frame.terminal_id !== terminalId
          ) {
            return false;
          }
          return atob(String(frame.data || '')).includes(token);
        }, 'output');
        conn.terminalFrame({
          t: 'terminal_close',
          host_id: id,
          terminal_id: terminalId,
        });
        return {
          opened: true,
          sawToken: true,
          terminalId,
          outputBytes: atob(String(output.data || '')).length,
        };
      } finally {
        window.removeEventListener('intendant-dashboard-terminal-frame', handler);
      }
    })();

    return {
      connected: conn.canUseRpc(),
      sessionId: conn.sessionId,
      verifiedBindingOk: conn.verifiedBinding?.ok === true,
      status: {
        apiSessionsAvailable: status?.api_sessions_available === true,
        apiSessionsStreamAvailable: status?.api_sessions_stream_available === true,
        terminalFramesAvailable: status?.terminal_frames_available === true,
        grantKind: status?.grant_kind || '',
        grantProfile: status?.grant_profile || '',
      },
      sessionsCount: Array.isArray(sessions) ? sessions.length : -1,
      streamEvents,
      streamedSessionsCount: Array.isArray(streamedSessions) ? streamedSessions.length : -1,
      terminal,
    };
  },
  async _debugProbeConnectHealthPanel(options = {}) {
    const state = renderConnectHealthPanel();
    const result = options.runSelfTest === false
      ? null
      : await runConnectSelfTests({ render: true });
    return {
      state,
      result,
      summaryText: document.getElementById('connect-health-summary')?.textContent || '',
      resultText: document.getElementById('connect-self-test-results')?.textContent || '',
    };
  },
  async _debugProbeControlNoReplay() {
    if (
      typeof dispatchControlMsg !== 'function' ||
      !dashboardTransport ||
      typeof dashboardTransport.request !== 'function'
    ) {
      return {
        skipped: true,
        dispatchType: typeof dispatchControlMsg,
        hasDashboardTransport: Boolean(dashboardTransport),
        requestType: typeof dashboardTransport?.request,
        rpcAttempts: 0,
        wsReplayCount: 0,
        rpcFailureWarnings: 0,
      };
    }
    const previousRequest = dashboardTransport.request;
    const previousWarn = console.warn;
    const previousApp = app;
    const previousSend = app && typeof app.send_server_action === 'function'
      ? app.send_server_action
      : null;
    let rpcAttempts = 0;
    let wsReplayCount = 0;
    let rpcFailureWarnings = 0;
    if (app && typeof app === 'object') {
      app.send_server_action = function() {
        wsReplayCount += 1;
      };
    } else {
      app = {
        send_server_action() {
          wsReplayCount += 1;
        },
      };
    }
    console.warn = function(...args) {
      if (String(args[0] || '').includes('ControlMsg RPC failed; not replaying over /ws')) {
        rpcFailureWarnings += 1;
      }
      return previousWarn.apply(this, args);
    };
    dashboardTransport.request = function(method, params, options) {
      if (method === 'api_control_msg') {
        rpcAttempts += 1;
        return Promise.reject(new Error('synthetic api_control_msg failure'));
      }
      return previousRequest.call(this, method, params, options);
    };
    try {
      dispatchControlMsg({ action: 'set_codex_sandbox', mode: 'workspace-write' });
      await new Promise(resolve => setTimeout(resolve, 50));
      return { skipped: false, rpcAttempts, wsReplayCount, rpcFailureWarnings };
    } finally {
      dashboardTransport.request = previousRequest;
      console.warn = previousWarn;
      if (previousApp && previousSend) {
        previousApp.send_server_action = previousSend;
      }
      app = previousApp;
    }
  },
  async _debugProbeControlUnavailableConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof dispatchControlMsg !== 'function' ||
      typeof dispatchSessionControlMsg !== 'function' ||
      typeof dispatchDashboardActionMsg !== 'function' ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        dispatchType: typeof dispatchControlMsg,
        sessionDispatchType: typeof dispatchSessionControlMsg,
        dashboardDispatchType: typeof dispatchDashboardActionMsg,
        hasControlTransport: Boolean(dashboardControlTransport),
        wsReplayCount: 0,
        unavailableWarnings: 0,
      };
    }
    const previousApp = app;
    const previousStatus = dashboardControlTransport.lastStatus;
    const previousWarn = console.warn;
    let wsReplayCount = 0;
    let unavailableWarnings = 0;
    app = {
      send_server_action() {
        wsReplayCount += 1;
      },
    };
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      api_control_msg_available: false,
      api_session_control_msg_available: false,
      api_dashboard_action_msg_available: false,
    };
    console.warn = function(...args) {
      if (String(args[0] || '').includes('unavailable until dashboard access is ready')) {
        unavailableWarnings += 1;
      }
      return previousWarn.apply(this, args);
    };
    try {
      dispatchControlMsg({ action: 'set_codex_sandbox', mode: 'workspace-write' });
      dispatchSessionControlMsg({ action: 'approve', id: -1 });
      dispatchDashboardActionMsg({ action: 'take_display', display_id: 4294967295 });
      await new Promise(resolve => setTimeout(resolve, 0));
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      console.warn = previousWarn;
      app = previousApp;
    }
    return {
      skipped: false,
      wsReplayCount,
      unavailableWarnings,
    };
  },
  async _debugProbeMediaConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof sendDashboardMediaUpload !== 'function' ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        sendType: typeof sendDashboardMediaUpload,
        hasControlTransport: Boolean(dashboardControlTransport),
        threw: false,
        wsReplayCount: 0,
      };
    }
    const previousApp = app;
    const previousSend = app && typeof app.send_server_action === 'function'
      ? app.send_server_action
      : null;
    const previousStatus = dashboardControlTransport.lastStatus;
    let wsReplayCount = 0;
    if (app && typeof app === 'object') {
      app.send_server_action = function() {
        wsReplayCount += 1;
      };
    } else {
      app = {
        send_server_action() {
          wsReplayCount += 1;
        },
      };
    }
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      upload_frames_available: false,
      api_media_editor_available: false,
      api_media_annotation_attach_available: false,
      api_media_annotation_submit_available: false,
      api_media_clip_start_available: false,
      api_media_clip_frame_available: false,
      api_media_clip_end_available: false,
      api_media_clip_cancel_available: false,
    };
    let threw = false;
    let error = '';
    try {
      await sendDashboardMediaUpload(
        'api_media_annotation_attach',
        { frame_id: 'debug-media-unavailable', stream: 'debug' },
        new Uint8Array([1, 2, 3]),
        { t: 'annotation_attach', frame_id: 'debug-media-unavailable', stream: 'debug', data: 'AQID' },
        'annotation attach'
      );
    } catch (err) {
      threw = true;
      error = err?.message || String(err);
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      if (previousApp && previousSend) {
        previousApp.send_server_action = previousSend;
      }
      app = previousApp;
    }
    return { skipped: false, threw, error, wsReplayCount };
  },
  async _debugProbePeerMutationConnectNoHttp() {
    if (
      !dashboardConnectModeEnabled() ||
      !dashboardTransport ||
      !dashboardControlTransport ||
      typeof dashboardTransport.jsonFetch !== 'function' ||
      typeof dashboardControlTransport.request !== 'function'
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        jsonFetchType: typeof dashboardTransport?.jsonFetch,
        requestType: typeof dashboardControlTransport?.request,
        threw: false,
        rpcAttempts: 0,
        httpFallbackCount: 0,
      };
    }
    const previousRequest = dashboardControlTransport.request;
    const previousFetch = window.fetch;
    let rpcAttempts = 0;
    let httpFallbackCount = 0;
    dashboardControlTransport.request = function(method, params, options) {
      if (method === 'api_peer_add') {
        rpcAttempts += 1;
        return Promise.reject(new Error('synthetic api_peer_add failure'));
      }
      return previousRequest.call(this, method, params, options);
    };
    window.fetch = function(input, init) {
      const url = typeof input === 'string' ? input : (input?.url || '');
      if (String(url).includes('/api/peers')) {
        httpFallbackCount += 1;
      }
      return previousFetch.call(this, input, init);
    };
    let threw = false;
    let error = '';
    try {
      await dashboardTransport.jsonFetch('api_peer_add', {
        url: 'https://127.0.0.1:9/.well-known/agent-card.json',
        persist: false,
      }, () => authedFetch('/api/peers', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      }), 'api_peer_add', { fallbackAfterRpcFailure: false });
    } catch (err) {
      threw = true;
      error = err?.message || String(err);
    } finally {
      dashboardControlTransport.request = previousRequest;
      window.fetch = previousFetch;
    }
    return { skipped: false, threw, error, rpcAttempts, httpFallbackCount };
  },
  async _debugProbeResumableFsDownload(path, options = {}) {
    if (
      !dashboardConnectModeEnabled() ||
      typeof dashboardFetchRangedBytes !== 'function' ||
      !dashboardTransport ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        helperType: typeof dashboardFetchRangedBytes,
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        httpFallbackCount: 0,
        rangeCount: 0,
        size: 0,
      };
    }
    const previousFetch = window.fetch;
    let httpFallbackCount = 0;
    const progress = [];
    window.fetch = function(input, init) {
      const url = typeof input === 'string' ? input : (input?.url || '');
      if (String(url).includes('/api/fs')) {
        httpFallbackCount += 1;
      }
      return previousFetch.call(this, input, init);
    };
    try {
      const result = await dashboardFetchRangedBytes('api_fs_read', { path }, {
        chunkBytes: Math.max(1, Number(options.chunkBytes) || 17),
        maxBytes: Number(options.maxBytes) || 1024 * 1024,
        onProgress: info => progress.push({
          loaded: info.loaded,
          total: info.total,
          rangeCount: info.rangeCount,
        }),
      });
      const text = new TextDecoder().decode(await result.blob.arrayBuffer());
      return {
        skipped: false,
        httpFallbackCount,
        rangeCount: result.range_count,
        progressCount: progress.length,
        filename: result.filename,
        size: result.size,
        totalSize: result.total_size,
        text,
      };
    } finally {
      window.fetch = previousFetch;
    }
  },
  async _debugProbeDiagnosticsConnectNoHttp() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof postVisualFreshnessDiagnostics !== 'function' ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        postType: typeof postVisualFreshnessDiagnostics,
        hasControlTransport: Boolean(dashboardControlTransport),
        threw: false,
        httpFallbackCount: 0,
      };
    }
    const previousStatus = dashboardControlTransport.lastStatus;
    const previousFetch = window.fetch;
    let httpFallbackCount = 0;
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      api_diagnostics_visual_freshness_available: false,
    };
    window.fetch = function(input, init) {
      const url = typeof input === 'string' ? input : (input?.url || '');
      if (String(url).includes('/api/diagnostics/visual-freshness')) {
        httpFallbackCount += 1;
      }
      return previousFetch.call(this, input, init);
    };
    let threw = false;
    let error = '';
    try {
      await postVisualFreshnessDiagnostics('debug-visual-freshness', '{"ok":true}\n');
    } catch (err) {
      threw = true;
      error = err?.message || String(err);
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      window.fetch = previousFetch;
    }
    return { skipped: false, threw, error, httpFallbackCount };
  },
  async _debugProbeDisplaySignalConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      !dashboardTransport ||
      typeof dashboardTransport.displayWebRtcSignal !== 'function'
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        signalType: typeof dashboardTransport?.displayWebRtcSignal,
        threw: false,
        wsReplayCount: 0,
        httpFallbackCount: 0,
      };
    }
    const previousApp = app;
    const previousSendRaw = app && typeof app.send_raw === 'function'
      ? app.send_raw
      : null;
    const previousSendAction = app && typeof app.send_server_action === 'function'
      ? app.send_server_action
      : null;
    const previousFetch = window.fetch;
    let wsReplayCount = 0;
    let httpFallbackCount = 0;
    if (app && typeof app === 'object') {
      app.send_raw = function() { wsReplayCount += 1; };
      app.send_server_action = function() { wsReplayCount += 1; };
    } else {
      app = {
        send_raw() { wsReplayCount += 1; },
        send_server_action() { wsReplayCount += 1; },
      };
    }
    window.fetch = function(input, init) {
      const url = typeof input === 'string' ? input : (input?.url || '');
      if (String(url).includes('/api/display') || String(url).includes('/ws')) {
        httpFallbackCount += 1;
      }
      return previousFetch.call(this, input, init);
    };
    let threw = false;
    let error = '';
    try {
      await dashboardTransport.displayWebRtcSignal({
        signal: 'offer',
        display_id: 4294967295,
        sdp: 'synthetic-offer',
      }, { timeoutMs: 15000 });
    } catch (err) {
      threw = true;
      error = err?.message || String(err);
    } finally {
      window.fetch = previousFetch;
      if (previousApp) {
        if (previousSendRaw) previousApp.send_raw = previousSendRaw;
        else delete previousApp.send_raw;
        if (previousSendAction) previousApp.send_server_action = previousSendAction;
        else delete previousApp.send_server_action;
      }
      app = previousApp;
    }
    return { skipped: false, threw, error, wsReplayCount, httpFallbackCount };
  },
  async _debugProbeDisplayAuthorityConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof requestDisplayInputAuthorityForSlot !== 'function' ||
      typeof releaseDisplayInputAuthorityForSlot !== 'function' ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        requestType: typeof requestDisplayInputAuthorityForSlot,
        releaseType: typeof releaseDisplayInputAuthorityForSlot,
        hasControlTransport: Boolean(dashboardControlTransport),
        requestReplayCount: 0,
        releaseReplayCount: 0,
      };
    }
    const previousStatus = dashboardControlTransport.lastStatus;
    const previousApp = app;
    let requestReplayCount = 0;
    let releaseReplayCount = 0;
    app = {
      request_display_input_authority() {
        requestReplayCount += 1;
      },
      release_display_input_authority() {
        releaseReplayCount += 1;
      },
    };
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      api_display_input_authority_available: false,
    };
    let requestResult = null;
    let releaseResult = null;
    try {
      requestResult = await requestDisplayInputAuthorityForSlot(4294967295);
      releaseResult = await releaseDisplayInputAuthorityForSlot(4294967295);
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      app = previousApp;
    }
    return {
      skipped: false,
      requestResult,
      releaseResult,
      requestReplayCount,
      releaseReplayCount,
    };
  },
  _debugProbeShellQueuesUntilOpened() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof sendShellBytes !== 'function' ||
      typeof handleShellOpened !== 'function' ||
      !dashboardTransport ||
      !dashboardControlTransport ||
      typeof dashboardTransport.terminalFrame !== 'function' ||
      typeof dashboardTransport.canUseRpc !== 'function' ||
      !dashboardTransport.canUseRpc()
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        sendType: typeof sendShellBytes,
        handleOpenedType: typeof handleShellOpened,
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        canUseRpc: Boolean(dashboardTransport?.canUseRpc?.()),
        framesBeforeAck: [],
        framesAfterAck: [],
      };
    }
    const previousShellTerm = shellTerm;
    const previousShellOpenSent = shellOpenSent;
    const previousShellOpenAcked = shellOpenAcked;
    const previousShellQueuedInput = shellQueuedInput;
    const previousShellWaitingNoticeShown = shellWaitingNoticeShown;
    const previousShellPendingResize = shellPendingResize;
    const previousStatus = dashboardControlTransport.lastStatus;
    const hadOwnTerminalFrame = Object.prototype.hasOwnProperty.call(dashboardTransport, 'terminalFrame');
    const previousTerminalFrame = dashboardTransport.terminalFrame;
    const frames = [];
    let noticeWrites = 0;
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      terminal_frames_available: true,
    };
    dashboardTransport.terminalFrame = function(frame) {
      frames.push({ ...frame });
      return true;
    };
    shellTerm = {
      cols: 101,
      rows: 29,
      write() { noticeWrites += 1; },
    };
    shellOpenSent = false;
    shellOpenAcked = false;
    shellQueuedInput = '';
    shellWaitingNoticeShown = false;
    shellPendingResize = null;
    try {
      sendShellBytes('queued-before-open');
      const framesBeforeAck = frames.map(frame => frame.t);
      const queuedBeforeAck = shellQueuedInput;
      const sentBeforeAck = shellOpenSent;
      const ackedBeforeAck = shellOpenAcked;
      handleShellOpened();
      return {
        skipped: false,
        framesBeforeAck,
        framesAfterAck: frames.map(frame => frame.t),
        queuedBeforeAck,
        queuedAfterAck: shellQueuedInput,
        sentBeforeAck,
        ackedBeforeAck,
        ackedAfterAck: shellOpenAcked,
        noticeWrites,
      };
    } finally {
      shellTerm = previousShellTerm;
      shellOpenSent = previousShellOpenSent;
      shellOpenAcked = previousShellOpenAcked;
      shellQueuedInput = previousShellQueuedInput;
      shellWaitingNoticeShown = previousShellWaitingNoticeShown;
      shellPendingResize = previousShellPendingResize;
      dashboardControlTransport.lastStatus = previousStatus;
      if (hadOwnTerminalFrame) {
        dashboardTransport.terminalFrame = previousTerminalFrame;
      } else {
        delete dashboardTransport.terminalFrame;
      }
    }
  },
  _debugProbeTerminalOutputBypassesDedupe() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof dashboardServerMessageDispatcher !== 'function' ||
      typeof flushShellOutput !== 'function'
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        dispatcherType: typeof dashboardServerMessageDispatcher,
        flushType: typeof flushShellOutput,
        writeCalls: 0,
        writtenBytes: 0,
      };
    }
    const previousEventsActive = dashboardControlEventsActive;
    const previousRecentKeys = new Map(dashboardRecentServerMessageKeys);
    const previousShellTerm = shellTerm;
    const previousOutputQueue = shellOutputQueue;
    const previousOutputQueuedBytes = shellOutputQueuedBytes;
    const previousOutputFlushScheduled = shellOutputFlushScheduled;
    const previousRequestAnimationFrame = window.requestAnimationFrame;
    let rafCount = 0;
    let writeCalls = 0;
    let writtenBytes = 0;
    window.requestAnimationFrame = function() {
      rafCount += 1;
      return 0;
    };
    shellTerm = {
      write(data) {
        writeCalls += 1;
        if (typeof data === 'string') writtenBytes += data.length;
        else writtenBytes += Number(data?.byteLength || data?.length || 0);
      },
    };
    shellOutputQueue = [];
    shellOutputQueuedBytes = 0;
    shellOutputFlushScheduled = false;
    dashboardControlEventsActive = true;
    dashboardRecentServerMessageKeys.clear();
    try {
      const frame = {
        t: 'terminal_output',
        host_id: SHELL_HOST_ID,
        terminal_id: SHELL_TERMINAL_ID,
        data: 'YQ==',
      };
      dashboardServerMessageDispatcher(JSON.stringify(frame));
      dashboardServerMessageDispatcher(JSON.stringify(frame));
      flushShellOutput();
      return {
        skipped: false,
        writeCalls,
        writtenBytes,
        rafCount,
        recentKeyCount: dashboardRecentServerMessageKeys.size,
      };
    } finally {
      dashboardControlEventsActive = previousEventsActive;
      dashboardRecentServerMessageKeys.clear();
      for (const [key, value] of previousRecentKeys) dashboardRecentServerMessageKeys.set(key, value);
      shellTerm = previousShellTerm;
      shellOutputQueue = previousOutputQueue;
      shellOutputQueuedBytes = previousOutputQueuedBytes;
      shellOutputFlushScheduled = previousOutputFlushScheduled;
      window.requestAnimationFrame = previousRequestAnimationFrame;
    }
  },
  _debugProbeEventDedupeAllowlist() {
    if (typeof dashboardShouldDropDuplicateServerMessage !== 'function') {
      return {
        skipped: true,
        dedupeType: typeof dashboardShouldDropDuplicateServerMessage,
      };
    }
    const previousEventsActive = dashboardControlEventsActive;
    const previousRecentKeys = new Map(dashboardRecentServerMessageKeys);
    const check = (message) => {
      dashboardRecentServerMessageKeys.clear();
      const first = dashboardShouldDropDuplicateServerMessage(message);
      const second = dashboardShouldDropDuplicateServerMessage(message);
      const recentKeyCount = dashboardRecentServerMessageKeys.size;
      return { first, second, recentKeyCount };
    };
    dashboardControlEventsActive = true;
    try {
      return {
        skipped: false,
        status: check({ event: 'status', session_id: 's', phase: 'Idle', turn: 1, autonomy: 'ask', task: '' }),
        sessionIdentity: check({ event: 'session_identity', session_id: 's', source: 'codex', backend_session_id: 'thread' }),
        terminalOutput: check({ t: 'terminal_output', host_id: 'local', terminal_id: 'shell-0', data: 'YQ==' }),
        displayIce: check({ t: 'display_ice', display_id: 0, candidate: { candidate: 'candidate:debug' } }),
        modelDelta: check({ event: 'model_response_delta', session_id: 's', text: 'x' }),
        logEntry: check({ event: 'log_entry', level: 'info', source: 'test', content: 'repeat' }),
        peerEventForwarded: check({ event: 'peer_event_forwarded', peer_id: 'p', payload: { event: 'log', message: 'repeat' } }),
        futureEvent: check({ event: 'future_event_type', value: 'repeat' }),
      };
    } finally {
      dashboardControlEventsActive = previousEventsActive;
      dashboardRecentServerMessageKeys.clear();
      for (const [key, value] of previousRecentKeys) dashboardRecentServerMessageKeys.set(key, value);
    }
  },
  _debugProbeDuplicateShellInputSends() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof sendShellBytes !== 'function' ||
      !dashboardTransport ||
      !dashboardControlTransport ||
      typeof dashboardTransport.terminalFrame !== 'function' ||
      typeof dashboardTransport.canUseRpc !== 'function' ||
      !dashboardTransport.canUseRpc()
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        sendType: typeof sendShellBytes,
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        canUseRpc: Boolean(dashboardTransport?.canUseRpc?.()),
        inputFrames: [],
      };
    }
    const previousShellOpenSent = shellOpenSent;
    const previousShellOpenAcked = shellOpenAcked;
    const previousShellQueuedInput = shellQueuedInput;
    const previousStatus = dashboardControlTransport.lastStatus;
    const hadOwnTerminalFrame = Object.prototype.hasOwnProperty.call(dashboardTransport, 'terminalFrame');
    const previousTerminalFrame = dashboardTransport.terminalFrame;
    const frames = [];
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      terminal_frames_available: true,
    };
    dashboardTransport.terminalFrame = function(frame) {
      frames.push({ ...frame });
      return true;
    };
    shellOpenSent = true;
    shellOpenAcked = true;
    shellQueuedInput = '';
    try {
      sendShellBytes('x');
      sendShellBytes('x');
      return {
        skipped: false,
        inputFrames: frames
          .filter(frame => frame.t === 'terminal_input')
          .map(frame => frame.data),
        queuedAfterSend: shellQueuedInput,
      };
    } finally {
      shellOpenSent = previousShellOpenSent;
      shellOpenAcked = previousShellOpenAcked;
      shellQueuedInput = previousShellQueuedInput;
      dashboardControlTransport.lastStatus = previousStatus;
      if (hadOwnTerminalFrame) {
        dashboardTransport.terminalFrame = previousTerminalFrame;
      } else {
        delete dashboardTransport.terminalFrame;
      }
    }
  },
  async _debugProbePresenceMediaConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof sendDashboardVoiceLog !== 'function' ||
      typeof sendDashboardVoiceDiagnostic !== 'function' ||
      typeof sendDashboardVideoFrameToServer !== 'function' ||
      !dashboardTransport ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        voiceLogType: typeof sendDashboardVoiceLog,
        diagnosticType: typeof sendDashboardVoiceDiagnostic,
        videoFrameType: typeof sendDashboardVideoFrameToServer,
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        presenceFrameCount: 0,
        uploadCount: 0,
        legacyCount: 0,
      };
    }
    const previousStatus = dashboardControlTransport.lastStatus;
    const previousPresenceFrame = dashboardTransport.presenceFrame;
    const previousUploadBytes = dashboardTransport.uploadBytes;
    const previousApp = app;
    let presenceFrameCount = 0;
    let uploadCount = 0;
    let legacyCount = 0;
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      presence_frames_available: true,
      upload_frames_available: true,
      api_presence_video_frame_available: true,
    };
    dashboardTransport.presenceFrame = function(frame) {
      if (frame && (frame.t === 'voice_log' || frame.t === 'voice_diagnostic')) {
        presenceFrameCount += 1;
      }
      return true;
    };
    dashboardTransport.uploadBytes = async function(method, params, bytes, options) {
      if (method === 'api_presence_video_frame') uploadCount += 1;
      return { ok: true, _httpOk: true };
    };
    app = {
      send_voice_log() { legacyCount += 1; },
      send_voice_diagnostic() { legacyCount += 1; },
      send_video_frame_to_server() { legacyCount += 1; },
    };
    try {
      sendDashboardVoiceLog('connect probe', 'debug');
      sendDashboardVoiceDiagnostic('connect_probe', 'ok');
      await sendDashboardVideoFrameToServer('AQID', 'connect-probe-f00001', 'debug');
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      dashboardTransport.presenceFrame = previousPresenceFrame;
      dashboardTransport.uploadBytes = previousUploadBytes;
      app = previousApp;
    }
    return {
      skipped: false,
      presenceFrameCount,
      uploadCount,
      legacyCount,
    };
  },
  async _debugProbePresenceServerSenderConnectNoLegacy() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof dashboardControlServerSender !== 'function' ||
      !dashboardTransport ||
      !dashboardControlTransport
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        senderType: typeof dashboardControlServerSender,
        hasDashboardTransport: Boolean(dashboardTransport),
        hasControlTransport: Boolean(dashboardControlTransport),
        presenceFrameCount: 0,
        actionRpcCount: 0,
      };
    }
    const previousStatus = dashboardControlTransport.lastStatus;
    const previousPresenceFrame = dashboardTransport.presenceFrame;
    const previousRequest = dashboardTransport.request;
    let presenceFrameCount = 0;
    let actionRpcCount = 0;
    dashboardControlTransport.lastStatus = {
      ...(previousStatus || {}),
      presence_frames_available: true,
      presence_active_handoff_available: true,
      presence_tool_request_available: true,
    };
    dashboardTransport.presenceFrame = function(frame) {
      if (frame && (frame.t === 'make_active' || frame.t === 'async_query')) {
        presenceFrameCount += 1;
      }
      return true;
    };
    dashboardTransport.request = function(method, params, options) {
      if (method === 'api_control_msg') actionRpcCount += 1;
      return Promise.resolve({ ok: true, _httpOk: true });
    };
    try {
      dashboardControlServerSender({ t: 'make_active', provider: 'gemini', model: 'debug' });
      dashboardControlServerSender({ t: 'async_query', id: 'aq_debug', tool: 'recall_memory', args: {} });
      dashboardControlServerSender({ action: 'approve', id: 123 });
      await new Promise(resolve => setTimeout(resolve, 0));
    } finally {
      dashboardControlTransport.lastStatus = previousStatus;
      dashboardTransport.presenceFrame = previousPresenceFrame;
      dashboardTransport.request = previousRequest;
    }
    return {
      skipped: false,
      presenceFrameCount,
      actionRpcCount,
    };
  },
  async _debugProbeTunneledPresenceServerCallback() {
    if (
      !dashboardConnectModeEnabled() ||
      typeof maybeHandleDashboardTunneledServerMessage !== 'function' ||
      !app ||
      typeof app.handle_tunneled_server_message !== 'function'
    ) {
      return {
        skipped: true,
        connectMode: dashboardConnectModeEnabled(),
        hasApp: Boolean(app),
        hasHandler: Boolean(app && typeof app.handle_tunneled_server_message === 'function'),
        diagnosticCount: 0,
      };
    }
    const previousDiagnostic = sendDashboardVoiceDiagnostic;
    const previousHasVoiceCredentials = hasVoiceCredentials;
    const previousIsActiveBrowser = isActiveBrowser;
    const previousStoredConversationCtx = storedConversationCtx;
    let diagnosticCount = 0;
    sendDashboardVoiceDiagnostic = function(kind, detail) {
      if (kind === 'make_active_granted_client' && String(detail || '').includes('handover=yes')) {
        diagnosticCount += 1;
      }
      return true;
    };
    hasVoiceCredentials = function() {
      return false;
    };
    try {
      const handled = maybeHandleDashboardTunneledServerMessage({
        t: 'active_granted',
        handover_context: 'connect handoff probe',
        conversation_context: 'connect conversation probe',
      });
      await new Promise(resolve => setTimeout(resolve, 0));
      return {
        skipped: false,
        handled,
        diagnosticCount,
        isActiveBrowserAfter: isActiveBrowser,
      };
    } finally {
      sendDashboardVoiceDiagnostic = previousDiagnostic;
      hasVoiceCredentials = previousHasVoiceCredentials;
      isActiveBrowser = previousIsActiveBrowser;
      storedConversationCtx = previousStoredConversationCtx;
      updateActivePassiveUI();
    }
  },
  request(method, params = {}, options = {}) {
    return dashboardTransport.request(method, params, options);
  },
  requestBytes(method, params = {}, options = {}) {
    return dashboardTransport.requestBytes(method, params, options);
  },
  uploadBytes(method, params = {}, bytes, options = {}) {
    return dashboardTransport.uploadBytes(method, params, bytes, options);
  },
  terminalFrame(frame) {
    return dashboardTransport.terminalFrame(frame);
  },
  presenceFrame(frame) {
    return dashboardTransport.presenceFrame(frame);
  },
  agentCard(options = {}) {
    return dashboardTransport.request('api_agent_card', {}, options);
  },
  cachedBootstrapEvents(options = {}) {
    return dashboardTransport.request('api_cached_bootstrap_events', {}, options);
  },
  browserWorkspaceSnapshot(options = {}) {
    return dashboardTransport.request('api_browser_workspace_snapshot', {}, options);
  },
  stateSnapshot(options = {}) {
    return dashboardTransport.request('api_state_snapshot', {}, options);
  },
  displayBootstrap(options = {}) {
    return dashboardTransport.request('api_display_bootstrap', {}, options);
  },
  displayAuthoritySnapshot(options = {}) {
    return dashboardTransport.displayAuthoritySnapshot(options);
  },
  requestDisplayInputAuthority(displayId, options = {}) {
    return dashboardTransport.requestDisplayInputAuthority(displayId, options);
  },
  releaseDisplayInputAuthority(displayId, options = {}) {
    return dashboardTransport.releaseDisplayInputAuthority(displayId, options);
  },
  displayInput(displayId, event) {
    return dashboardTransport.displayInput(displayId, event);
  },
  sessionLogReplay(options = {}) {
    return dashboardTransport.request('api_session_log_replay', {}, options);
  },
  externalSessionActivityReplay(options = {}) {
    return dashboardTransport.request('api_external_session_activity_replay', {}, options);
  },
  dashboardBootstrap(options = {}) {
    return dashboardTransport.request('api_dashboard_bootstrap', {}, options);
  },
  peers() {
    return dashboardTransport.request('api_peers');
  },
  sessions(params = {}) {
    return dashboardTransport.request('api_sessions', params);
  },
  sessionsStream(params = {}, options = {}, onEvent = {}) {
    return dashboardTransport.stream('api_sessions_stream', params, options, onEvent);
  },
  sessionDetail(sessionId, params = {}) {
    return dashboardTransport.request('api_session_detail', { session_id: sessionId, ...params });
  },
  sessionAgentOutput(sessionId, ids = [], params = {}, options = {}) {
    return dashboardTransport.request('api_session_agent_output', { session_id: sessionId, ids, ...params }, options);
  },
  sessionDelete(sessionId, target = 'session', options = {}) {
    return dashboardTransport.request('api_session_delete', { session_id: sessionId, target }, options);
  },
  currentAgentOutput(ids = [], options = {}) {
    return dashboardTransport.request('api_session_current_agent_output', { ids }, options);
  },
  currentHistory(options = {}) {
    return dashboardTransport.request('api_session_current_history', {}, options);
  },
  rollbackCurrent(params = {}, options = {}) {
    return dashboardTransport.request('api_session_current_rollback', params, options);
  },
  redoCurrent(options = {}) {
    return dashboardTransport.request('api_session_current_redo', {}, options);
  },
  pruneCurrent(options = {}) {
    return dashboardTransport.request('api_session_current_prune', {}, options);
  },
  currentChanges(path = '', options = {}) {
    return dashboardTransport.request('api_session_current_changes', changesRequestParams(path), options);
  },
  contextSnapshot(params = {}, options = {}) {
    return dashboardTransport.request('api_session_context_snapshot', params, options);
  },
  deleteCurrentUpload(id, options = {}) {
    return dashboardTransport.request('api_session_current_upload_delete', { id }, options);
  },
  currentUploads(options = {}) {
    return dashboardTransport.request('api_session_current_uploads', {}, options);
  },
  recordings(options = {}) {
    return dashboardTransport.request('api_recordings', {}, options);
  },
  sessionRecordings(sessionId, options = {}) {
    return dashboardTransport.request('api_session_recordings', { session_id: sessionId }, options);
  },
  worktrees(options = {}) {
    return dashboardTransport.request('api_worktrees', {}, options);
  },
  worktreesInspect(params = {}, options = {}) {
    return dashboardTransport.request('api_worktrees_inspect', params, options);
  },
  worktreesScan(options = {}) {
    return dashboardTransport.request('api_worktrees_scan', {}, options);
  },
  worktreesRemove(params = {}, options = {}) {
    return dashboardTransport.request('api_worktrees_remove', params, options);
  },
  sessionSearch(params = {}, options = {}) {
    return dashboardTransport.request('api_sessions_search', params, options);
  },
  settings() {
    return dashboardTransport.request('api_settings');
  },
  apiKeyStatus() {
    return dashboardTransport.request('api_key_status');
  },
  projectRoot() {
    return dashboardTransport.request('api_project_root');
  },
  displays() {
    return dashboardTransport.request('api_displays');
  },
  managedContextRecords(query) {
    return dashboardTransport.request('api_managed_context_records', { query });
  },
  managedContextAnchors(query) {
    return dashboardTransport.request('api_managed_context_anchors', { query });
  },
  managedContextFission(query) {
    return dashboardTransport.request('api_managed_context_fission', { query });
  },
  displayWebRtcSignal(params = {}, options = {}) {
    return dashboardTransport.displayWebRtcSignal(params, options);
  },
  peerWebRtcSignal(peerId, params = {}, options = {}) {
    return dashboardTransport.request('api_peer_webrtc_signal', { peer_id: peerId, ...params }, options);
  },
  peerFileTransferSignal(peerId, params = {}, options = {}) {
    return dashboardTransport.peerFileTransferSignal(peerId, params, options);
  },
  peerDashboardControlSignal(peerId, params = {}, options = {}) {
    return dashboardTransport.peerDashboardControlSignal(peerId, params, options);
  },
};

