// ── Standalone shell output handler ──
// Called when the server sends {"t":"terminal_output", "data":"<base64>"}
// for this host/terminal pair.
function handleShellOutput(base64data) {
  if (!shellTerm) return;
  const bytes = base64ToBytes(base64data);
  shellOutputQueue.push(bytes);
  shellOutputQueuedBytes += bytes.byteLength;
  if (!shellOutputFlushScheduled) {
    shellOutputFlushScheduled = true;
    requestAnimationFrame(flushShellOutput);
  }
}

function flushShellOutput() {
  shellOutputFlushScheduled = false;
  if (!shellTerm) {
    shellOutputQueue = [];
    shellOutputQueuedBytes = 0;
    return;
  }
  if (shellOutputQueue.length === 0) return;

  const chunks = shellOutputQueue;
  const total = shellOutputQueuedBytes;
  shellOutputQueue = [];
  shellOutputQueuedBytes = 0;

  if (chunks.length === 1) {
    shellTerm.write(chunks[0]);
    return;
  }

  const merged = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  shellTerm.write(merged);
}

function handleShellExited(status) {
  if (!shellTerm) return;
  flushShellOutput();
  shellTerm.write(`\r\n\x1b[33m[shell exited with status ${status}]\x1b[0m\r\n`);
  setShellHostStatus(`Shell exited on ${shellHostLabel()}`, 'warn');
  shellOpenSent = false;
  shellOpenAcked = false;
  shellQueuedInput = '';
  shellWaitingNoticeShown = false;
}

function handleShellOpened(ack) {
  shellOpenSent = true;
  shellOpenAcked = true;
  shellWaitingNoticeShown = false;
  shellShared = !!(ack && ack.shared);
  shellCanShare = !!(ack && ack.can_share);
  renderShellShareState();
  const resize = shellPendingResize || {
    cols: shellTerm?.cols || 80,
    rows: shellTerm?.rows || 24,
  };
  shellPendingResize = null;
  setShellHostStatus(`Connected to ${shellHostLabel()}`, 'ok');
  sendShellResize(resize.cols, resize.rows, { allowBeforeAck: true });
  flushQueuedShellInput();
}

function handleShellShared(d) {
  shellShared = !!d.shared;
  renderShellShareState();
  showControlToast?.('success', shellShared ? 'Shell session shared' : 'Shell session unshared');
}

function renderShellShareState() {
  const btn = document.getElementById('shell-share-btn');
  if (!btn) return;
  btn.classList.toggle('hidden', !shellCanShare);
  btn.classList.toggle('is-shared', shellShared);
  btn.textContent = shellShared ? 'Shared' : 'Share';
  btn.title = shellShared
    ? 'Shared: scoped collaborators with terminal.view / terminal.write can attach. Click to make private.'
    : 'Share this shell session so scoped collaborators (terminal.view / terminal.write) can attach';
}

function toggleShellShare() {
  if (!shellCanShare) return;
  sendShellMessage({
    t: 'terminal_share',
    host_id: currentShellHostId(),
    terminal_id: SHELL_TERMINAL_ID,
    shared: !shellShared,
  });
}

function handleShellError(error) {
  shellCanShare = false;
  renderShellShareState();
  shellOpenSent = false;
  shellOpenAcked = false;
  setShellHostStatus(String(error || 'shell unavailable'), 'error');
  if (shellTerm) {
    const message = String(error || 'shell unavailable');
    shellTerm.write(`\r\n\x1b[31m[shell error: ${message}]\x1b[0m\r\n`);
  }
}

function showShellWaitingNotice() {
  if (shellWaitingNoticeShown || !shellTerm) return;
  shellWaitingNoticeShown = true;
  const label = currentShellHostId() === SHELL_HOST_ID
    ? 'shell access'
    : `${shellHostLabel()} shell access`;
  shellTerm.write(`\r\n\x1b[90m[waiting for ${label}]\x1b[0m\r\n`);
}

function setTerminalPaneAccessible(pane, active) {
  if (!pane) return;
  if (active) {
    pane.removeAttribute('aria-hidden');
    try { pane.inert = false; } catch (_) {}
  } else {
    pane.setAttribute('aria-hidden', 'true');
    try { pane.inert = true; } catch (_) {}
  }
}

function syncTerminalPaneAccessibility() {
  setTerminalPaneAccessible(document.getElementById('term-pane-shell'), activeTermSubtab === 'shell');
}

// Encode a string as base64 (UTF-8 safe).
function utf8ToBase64(s) {
  return btoa(unescape(encodeURIComponent(s)));
}

// Send a raw WS message object by going through the WASM app's send_raw.
// Falls back to send_server_action if send_raw isn't available.
function sendRawMessage(obj) {
  if (dashboardConnectModeEnabled()) {
    console.warn('[dashboard-control] legacy WebSocket message unavailable in Connect mode', obj?.t || obj);
    return false;
  }
  if (!app) return false;
  const payload = JSON.stringify(obj);
  if (app.send_raw) {
    app.send_raw(payload);
    return true;
  }
  if (app.send_server_action) {
    app.send_server_action(obj);
    return true;
  }
  return false;
}

const dashboardPresenceWarned = new Set();
let dashboardVoiceLogSeq = 0;

function dashboardWarnPresenceOnce(key, message, err = null) {
  if (dashboardPresenceWarned.has(key)) return;
  dashboardPresenceWarned.add(key);
  if (err) console.warn(message, err);
  else console.warn(message);
}

function dashboardPresenceFramesAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.presence_frames_available === true
  );
}

function dashboardPresenceVideoFrameAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.upload_frames_available === true &&
    dashboardControlTransport?.lastStatus?.api_presence_video_frame_available === true
  );
}

function sendDashboardPresenceFrame(frame, fallback = null, label = frame?.t || 'presence frame') {
  if (dashboardPresenceFramesAvailable() && dashboardTransport.presenceFrame(frame)) {
    return true;
  }
  if (dashboardConnectModeEnabled()) {
    dashboardWarnPresenceOnce(
      `presence:${label}`,
      `[dashboard-control] ${label} unavailable until dashboard access reconnects`
    );
    return false;
  }
  if (typeof fallback === 'function') return fallback() === true;
  return false;
}

const DASHBOARD_TUNNELED_SERVER_TYPES = new Set([
  'state_snapshot',
  'presence_welcome',
  'presence_checkpoint_ack',
  'force_disconnect_voice',
  'active_granted',
  'tool_response',
  'async_query_result',
  'display_input_authority_state',
]);

function maybeHandleDashboardTunneledServerMessage(message) {
  if (!dashboardConnectModeEnabled()) return false;
  if (!message || typeof message !== 'object') return false;
  if (!DASHBOARD_TUNNELED_SERVER_TYPES.has(String(message.t || ''))) return false;
  if (!app || typeof app.handle_tunneled_server_message !== 'function') return false;
  try {
    app.handle_tunneled_server_message(message);
    return true;
  } catch (err) {
    console.warn('[dashboard-control] tunneled presence message handling failed', err);
    return false;
  }
}

function dashboardControlServerSender(message) {
  if (!dashboardConnectModeEnabled()) return false;
  if (!message || typeof message !== 'object') return false;
  const t = String(message.t || '');
  if (
    t === 'presence_connect' ||
    t === 'presence_disconnect' ||
    t === 'voice_log' ||
    t === 'presence_checkpoint' ||
    t === 'voice_diagnostic' ||
    t === 'live_usage_update' ||
    t === 'tool_request' ||
    t === 'async_query' ||
    t === 'make_active'
  ) {
    return sendDashboardPresenceFrame(message, null, t);
  }
  if (t === 'video_frame') {
    sendDashboardVideoFrameToServer(message.data || '', message.frame_id || message.frameId || '', message.stream || 'cam0')
      .catch(err => console.warn('[dashboard-control] tunneled video frame failed', err));
    return true;
  }
  if (message.action) {
    if (!dashboardTransport || !dashboardTransport.canUseRpc || !dashboardTransport.canUseRpc()) {
      dashboardWarnPresenceOnce(
        'control_action',
        '[dashboard-control] voice action unavailable until dashboard access reconnects'
      );
      return false;
    }
    dashboardTransport.request('api_control_msg', { message }, { timeoutMs: 10000 })
      .catch(err => console.warn('[dashboard-control] voice action RPC failed', err));
    return true;
  }
  dashboardWarnPresenceOnce(
    `server_sender:${t || 'unknown'}`,
    `[dashboard-control] unsupported tunneled server message: ${t || 'unknown'}`
  );
  return false;
}

function installDashboardControlServerSender() {
  if (!dashboardConnectModeEnabled()) return;
  if (!app || typeof app.set_server_sender !== 'function') return;
  app.set_server_sender((message) => dashboardControlServerSender(message));
}

function activeVoiceModelForPresence(provider, explicitModel = null) {
  if (explicitModel) return explicitModel;
  if (gatewayConfig?.model) return gatewayConfig.model;
  if (provider === 'openai') return 'gpt-4o-realtime-preview';
  if (provider === 'gemini') return 'gemini-2.5-flash-native-audio-preview-12-2025';
  return 'unknown';
}

function sendDashboardPresenceConnect(provider, explicitModel = null) {
  const model = activeVoiceModelForPresence(provider, explicitModel);
  return sendDashboardPresenceFrame({
    t: 'presence_connect',
    server_session_id: null,
    last_event_seq: 0,
    provider,
    model,
    passive: localStorage.getItem('passive_mode') === 'true',
  }, () => true, 'presence_connect');
}

function sendDashboardPresenceDisconnect() {
  return sendDashboardPresenceFrame({ t: 'presence_disconnect' }, () => true, 'presence_disconnect');
}

function disconnectDashboardVoice() {
  if (!app) return;
  app.disconnect_voice();
}

function sendDashboardVoiceLog(text, toolContext) {
  dashboardVoiceLogSeq += 1;
  return sendDashboardPresenceFrame({
    t: 'voice_log',
    text,
    seq: dashboardVoiceLogSeq,
    tool_context: toolContext || null,
  }, () => {
    app?.send_voice_log(text, toolContext);
    return true;
  }, 'voice_log');
}

function sendDashboardVoiceDiagnostic(kind, detail) {
  return sendDashboardPresenceFrame({
    t: 'voice_diagnostic',
    kind,
    detail,
  }, () => {
    app?.send_voice_diagnostic(kind, detail);
    return true;
  }, `voice_diagnostic:${kind}`);
}

function sendDashboardLiveUsage(usage) {
  if (!usage || typeof usage !== 'object') return false;
  return sendDashboardPresenceFrame({
    t: 'live_usage_update',
    provider: usage.provider || '',
    model: usage.model || '',
    input_tokens: Number(usage.input_tokens || 0),
    output_tokens: Number(usage.output_tokens || 0),
    cached_tokens: Number(usage.cached_tokens || 0),
    total_tokens: Number(usage.total_tokens || 0),
    thinking_tokens: Number(usage.thinking_tokens || 0),
    input_text_tokens: Number(usage.input_text_tokens || 0),
    input_audio_tokens: Number(usage.input_audio_tokens || 0),
    input_image_tokens: Number(usage.input_image_tokens || 0),
    cached_text_tokens: Number(usage.cached_text_tokens || 0),
    cached_audio_tokens: Number(usage.cached_audio_tokens || 0),
    cached_image_tokens: Number(usage.cached_image_tokens || 0),
    output_text_tokens: Number(usage.output_text_tokens || 0),
    output_audio_tokens: Number(usage.output_audio_tokens || 0),
  }, null, 'live_usage_update');
}

function sendDashboardUserAudio(base64Pcm) {
  if (dashboardConnectModeEnabled()) {
    dashboardWarnPresenceOnce(
      'user_audio',
      '[dashboard-control] server-side transcription audio is not tunneled in Connect mode yet'
    );
    return false;
  }
  app?.send_user_audio(base64Pcm);
  return true;
}

async function sendDashboardVideoFrameToServer(base64Jpeg, frameId, stream) {
  if (dashboardPresenceVideoFrameAvailable()) {
    try {
      const bytes = dashboardControlBase64ToBytes(base64Jpeg);
      const result = await dashboardTransport.uploadBytes('api_presence_video_frame', {
        frame_id: frameId,
        stream,
        mime: 'image/jpeg',
      }, bytes, {
        timeoutMs: 120000,
      });
      if (result?._httpOk === false || result?.ok === false) {
        throw new Error(result.error || `presence video frame returned ${result._httpStatus || 'error'}`);
      }
      return true;
    } catch (err) {
      if (dashboardConnectModeEnabled()) {
        dashboardWarnPresenceOnce('video_frame_upload', '[dashboard-control] Connect video frame upload failed', err);
        return false;
      }
      console.warn('[dashboard-control] presence video frame upload failed; falling back to /ws', err);
    }
  }
  if (dashboardConnectModeEnabled()) {
    dashboardWarnPresenceOnce(
      'video_frame_unavailable',
      '[dashboard-control] video frame archival unavailable until dashboard access reconnects'
    );
    return false;
  }
  app?.send_video_frame_to_server(base64Jpeg, frameId, stream);
  return true;
}

function dashboardMediaEditorRpcAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.upload_frames_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_editor_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_annotation_attach_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_annotation_submit_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_clip_start_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_clip_frame_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_clip_end_available === true &&
    dashboardControlTransport?.lastStatus?.api_media_clip_cancel_available === true
  );
}

function dashboardMediaAssertOk(result, label) {
  if (result && result._httpOk === false) {
    throw new Error(result.error || `${label || 'media transfer'} failed`);
  }
  return result;
}

function dashboardMediaTransferFailed(err, label) {
  console.warn(`[dashboard-control] ${label} failed; not replaying over /ws`, err);
  showControlToast?.('error', err?.message || `${label} failed`);
}

function dashboardMediaTunnelUnavailableError(label) {
  return new Error(`Media access is unavailable for ${label || 'this transfer'}`);
}

async function uploadDashboardMediaTunnel(method, params, bytes, label, options = {}) {
  const result = await dashboardTransport.uploadBytes(method, params, bytes, {
    timeoutMs: options.timeoutMs || 120000,
    signal: options.signal,
    chunkBytes: options.chunkBytes,
  });
  return dashboardMediaAssertOk(result, label);
}

async function requestDashboardMediaTunnel(method, params, label, options = {}) {
  const result = await dashboardTransport.request(method, params, {
    timeoutMs: options.timeoutMs || 30000,
    signal: options.signal,
  });
  return dashboardMediaAssertOk(result, label);
}

async function sendDashboardMediaUpload(method, params, bytes, legacyObj, label, options = {}) {
  if (dashboardMediaEditorRpcAvailable()) {
    return uploadDashboardMediaTunnel(method, params, bytes, label, options);
  }
  if (dashboardConnectModeEnabled()) {
    throw dashboardMediaTunnelUnavailableError(label);
  }
  sendLegacyMediaEditorMessage(legacyObj);
  return null;
}

// Compatibility fallback for daemons that do not yet advertise the dedicated
// dashboard media/editor protocol.
function sendLegacyMediaEditorMessage(obj) {
  if (app && app.send_server_action) app.send_server_action(obj);
}

function sendShellMessage(obj) {
  const hostId = String(obj?.host_id || currentShellHostId()).trim() || SHELL_HOST_ID;
  if (hostId !== SHELL_HOST_ID && hostId !== selfPeerId) {
    if (!peerDashboardControlSignalAvailable(hostId)) {
      setShellHostStatus('Shell access is unavailable for the selected peer', 'error');
      if (obj?.t === 'terminal_open') showShellWaitingNotice();
      return false;
    }
    peerDashboardControlConnectionForHost(hostId, { timeoutMs: 30000 })
      .then(conn => {
        if (conn.lastStatus?.terminal_frames_available === false) {
          throw new Error('selected peer does not allow terminal frames');
        }
        if (!conn.terminalFrame({ ...obj, host_id: hostId })) {
          throw new Error('Peer shell access is not connected');
        }
        setShellHostStatus(`Connected to ${shellHostLabel(hostId)}`, 'ok');
      })
      .catch(err => {
        setShellHostStatus(err?.message || String(err), 'error');
        handleShellError(err?.message || String(err));
      });
    return true;
  }
  if (dashboardTerminalFramesAvailable() && dashboardTransport.terminalFrame(obj)) {
    return true;
  }
  if (dashboardConnectModeEnabled()) {
    if (obj?.t === 'terminal_open') showShellWaitingNotice();
    return false;
  }
  return sendRawMessage(obj);
}

function shellOpenFrame() {
  return {
    t: 'terminal_open',
    host_id: currentShellHostId(),
    terminal_id: SHELL_TERMINAL_ID,
    cols: shellTerm?.cols || 80,
    rows: shellTerm?.rows || 24,
  };
}

function flushQueuedShellInput() {
  if (!shellQueuedInput || !shellOpenAcked) return;
  const queued = shellQueuedInput;
  shellQueuedInput = '';
  sendShellBytes(queued);
}

function queueShellInput(bytes) {
  if (!bytes) return;
  const next = shellQueuedInput + bytes;
  if (next.length > SHELL_QUEUED_INPUT_MAX_BYTES) {
    shellQueuedInput = next.slice(next.length - SHELL_QUEUED_INPUT_MAX_BYTES);
  } else {
    shellQueuedInput = next;
  }
  showShellWaitingNotice();
}

function openShellSessionIfPossible(force = false) {
  if (!shellTerm) return false;
  if (shellOpenSent && !force) return true;
  const sent = sendShellMessage(shellOpenFrame());
  if (sent) {
    shellOpenSent = true;
    shellOpenAcked = false;
    shellWaitingNoticeShown = false;
    return true;
  }
  shellOpenSent = false;
  shellOpenAcked = false;
  return false;
}

function maybeOpenShellAfterTransportReady() {
  if (activeTab !== 'terminal' || activeTermSubtab !== 'shell') return;
  if (!shellInitialized || !shellTerm || shellOpenSent) return;
  openShellSessionIfPossible();
}

function sendShellResize(cols, rows, options = {}) {
  const next = {
    cols: Number(cols) || 80,
    rows: Number(rows) || 24,
  };
  if (!options.allowBeforeAck && !shellOpenAcked) {
    shellPendingResize = next;
    if (!shellOpenSent) openShellSessionIfPossible();
    return false;
  }
  return sendShellMessage({
    t: 'terminal_resize',
    host_id: currentShellHostId(),
    terminal_id: SHELL_TERMINAL_ID,
    cols: next.cols,
    rows: next.rows,
  });
}

// ── Standalone Shell (lazy xterm.js) ──
//
// Loads the CDN-hosted xterm.js on first use. Sends
// terminal_open / terminal_input / terminal_resize / terminal_close
// messages to the server; handleShellOutput/handleShellExited receive
// the reply events.
function initShell() {
  if (shellInitialized) return;
  shellInitialized = true;

  // Ensure the xterm.js stylesheet is active.
  document.getElementById('xterm-css').removeAttribute('disabled');

  const start = () => {
    shellTerm = new Terminal({
      theme: {
        background: '#1e1e2e', foreground: '#cdd6f4', cursor: '#f5e0dc', cursorAccent: '#1e1e2e',
        selectionBackground: '#45475a',
        black: '#45475a', red: '#f38ba8', green: '#a6e3a1', yellow: '#f9e2af',
        blue: '#89b4fa', magenta: '#cba6f7', cyan: '#94e2d5', white: '#bac2de',
        brightBlack: '#585b70', brightRed: '#f38ba8', brightGreen: '#a6e3a1',
        brightYellow: '#f9e2af', brightBlue: '#89b4fa', brightMagenta: '#cba6f7',
        brightCyan: '#94e2d5', brightWhite: '#a6adc8',
      },
      fontFamily: "'JetBrains Mono', 'Fira Code', Menlo, Monaco, monospace",
      fontSize: 13, allowProposedApi: true,
      scrollback: 5000,
    });
    shellFitAddon = new FitAddon.FitAddon();
    shellTerm.loadAddon(shellFitAddon);
    shellTerm.open(document.getElementById('shell-container'));
    syncTerminalPaneAccessibility();
    shellFitAddon.fit();

    // Forward every byte the user types straight to the PTY. We use
    // `onData` (not `onKey`) so sequences like arrow keys, Ctrl+C, and
    // paste events all come through as the raw bytes xterm decoded.
    // `applyShellModifiers` transforms the first char when the mobile
    // key bar has Ctrl/Alt armed, then clears the modifier state.
    shellTerm.onData((data) => {
      sendShellBytes(applyShellModifiers(data));
    });

    shellTerm.onResize(({ cols, rows }) => {
      sendShellResize(cols, rows);
    });

    new ResizeObserver(() => {
      if (shellFitAddon && activeTab === 'terminal' && activeTermSubtab === 'shell') {
        shellFitAddon.fit();
      }
    }).observe(document.getElementById('shell-container'));

    // Ask the server to open (or reattach to) the session. In Connect mode the
    // dashboard-control tunnel may still be negotiating; if so, this is retried
    // from dashboardUpdateTransportStatus() once terminal frames are ready.
    openShellSessionIfPossible();
  };

  // If xterm.js is already loaded, start immediately. Otherwise load the
  // vendored copies (embedded static assets — no external fetch) and start.
  if (typeof Terminal !== 'undefined' && typeof FitAddon !== 'undefined') {
    start();
    return;
  }
  const script = document.createElement('script');
  script.src = '/xterm.min.js';
  script.onload = () => {
    const fitScript = document.createElement('script');
    fitScript.src = '/xterm-addon-fit.min.js';
    fitScript.onload = start;
    document.head.appendChild(fitScript);
  };
  document.head.appendChild(script);
}

function switchTerminalSubtab(name) {
  if (activeTermSubtab === name) {
    syncTerminalPaneAccessibility();
    return;
  }
  activeTermSubtab = name;
  document.querySelectorAll('#tab-terminal .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.termTab === name);
  });
  document.querySelectorAll('#tab-terminal .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `term-pane-${name}`);
  });
  // Key bar toggle is only meaningful on the Shell sub-tab.
  const toggle = document.getElementById('keybar-toggle');
  if (toggle) toggle.style.display = (name === 'shell') ? '' : 'none';
  if (name === 'shell') {
    if (!shellInitialized) initShell();
    else {
      if (shellFitAddon) requestAnimationFrame(() => shellFitAddon.fit());
      openShellSessionIfPossible();
    }
  }
  syncTerminalPaneAccessibility();
}

// ── Shell key bar ──
//
// Sends bytes to the remote PTY via the same terminal_input message the
// soft keyboard uses, so behavior is identical to typing on a real
// keyboard. Two flavors of keys:
//
// - Fixed sequences (Esc, Tab, arrows, …) — `data-seq` contains the raw
//   bytes to send. Modifier state is NOT applied to these, because a
//   single tap on "↑" should send one arrow, not Ctrl+Up by accident.
//
// - Sticky modifiers (Ctrl, Alt) — `data-sticky` names the modifier.
//   Tapping arms it (highlight); the next character the user types on
//   the soft keyboard is transformed in xterm's onData handler before
//   being sent. One-shot: the modifier clears after one transform.

function sendShellBytes(bytes) {
  if (!bytes) return;
  if (!shellOpenAcked) {
    if (!shellOpenSent) openShellSessionIfPossible();
    queueShellInput(bytes);
    return;
  }
  const sent = sendShellMessage({
    t: 'terminal_input',
    host_id: currentShellHostId(),
    terminal_id: SHELL_TERMINAL_ID,
    data: utf8ToBase64(bytes),
  });
  if (!sent) {
    shellOpenSent = false;
    shellOpenAcked = false;
    queueShellInput(bytes);
  }
}

/// Apply any armed sticky modifiers to `data`, returning the transformed
/// bytes. Clears the modifier state after applying (one-shot behavior).
function applyShellModifiers(data) {
  if (!shellModifiers.ctrl && !shellModifiers.alt) return data;
  if (data.length !== 1) {
    // Multi-byte paste or similar — don't try to transform, just clear
    // modifiers and pass through unchanged.
    shellModifiers.ctrl = false;
    shellModifiers.alt = false;
    updateShellModifierUi();
    return data;
  }
  let out = data;
  if (shellModifiers.ctrl) {
    // ASCII control: 'a'→0x01, 'b'→0x02, …, '?'→0x7f. Only transforms
    // letters and a few symbols; pass anything else through.
    const code = data.charCodeAt(0);
    let ctrlCode = null;
    if (code >= 0x61 && code <= 0x7a) ctrlCode = code - 0x60;         // a–z → ^A..^Z
    else if (code >= 0x41 && code <= 0x5a) ctrlCode = code - 0x40;    // A–Z → ^A..^Z
    else if (code === 0x20) ctrlCode = 0;                              // ^Space
    else if (code >= 0x5b && code <= 0x5f) ctrlCode = code - 0x40;    // [,\,],^,_
    else if (code === 0x3f) ctrlCode = 0x7f;                           // ^? = DEL
    if (ctrlCode !== null) out = String.fromCharCode(ctrlCode);
  }
  if (shellModifiers.alt) {
    // Meta/Alt convention: prefix with ESC.
    out = '\u001b' + out;
  }
  shellModifiers.ctrl = false;
  shellModifiers.alt = false;
  updateShellModifierUi();
  return out;
}

function updateShellModifierUi() {
  const bar = document.getElementById('shell-keybar');
  if (!bar) return;
  bar.querySelectorAll('.shell-key.sticky').forEach(btn => {
    const name = btn.dataset.sticky;
    btn.classList.toggle('armed', !!shellModifiers[name]);
  });
}

function wireShellKeybar() {
  const bar = document.getElementById('shell-keybar');
  if (!bar) return;

  // Use `pointerdown` rather than `click` for two reasons:
  //  1. It fires immediately on both touch and mouse (no 300 ms tap delay
  //     on some older mobile browsers).
  //  2. preventDefault on pointerdown suppresses the focus shift that
  //     would otherwise dismiss the soft keyboard when tapping a key,
  //     without suppressing the action (unlike preventDefault on
  //     touchstart, which also cancels the synthesized click).
  const handlePress = (e) => {
    const btn = e.target.closest('.shell-key');
    if (!btn) return;
    e.preventDefault();

    const sticky = btn.dataset.sticky;
    if (sticky) {
      shellModifiers[sticky] = !shellModifiers[sticky];
      updateShellModifierUi();
    } else {
      const keyName = btn.dataset.key;
      const seq = keyName ? SHELL_KEY_SEQS[keyName] : null;
      if (seq != null) {
        // Fixed-sequence keys ignore modifiers — send raw bytes as-is.
        sendShellBytes(seq);
      }
    }

    // Make sure xterm keeps focus so the soft keyboard stays up.
    if (shellTerm) shellTerm.focus();
  };
  bar.addEventListener('pointerdown', handlePress);

  // Toggle visibility from the header button. The @media (pointer: coarse)
  // rule forces the bar visible on touch devices regardless of this class.
  const toggle = document.getElementById('keybar-toggle');
  if (toggle) {
    const isCoarse = window.matchMedia('(pointer: coarse)').matches;
    if (isCoarse) {
      toggle.classList.add('active');
    }
    toggle.addEventListener('click', () => {
      bar.classList.toggle('visible');
      toggle.classList.toggle('active', bar.classList.contains('visible'));
      if (shellFitAddon) requestAnimationFrame(() => shellFitAddon.fit());
    });
    // Hide the toggle button when the Shell sub-tab isn't active — we
    // only want it visible while the user can see the shell.
    toggle.style.display = (activeTermSubtab === 'shell') ? '' : 'none';
  }
}
// Wire on DOM load so the click handler is live even before the user
// has ever opened the Terminal tab.
document.addEventListener('DOMContentLoaded', wireShellKeybar);
document.addEventListener('DOMContentLoaded', () => {
  syncTerminalPaneAccessibility();
  refreshShellHostOptions();
});
document.addEventListener('DOMContentLoaded', () => {
  document.querySelectorAll('[data-shared-view-close]').forEach(btn => {
    btn.addEventListener('click', hideSharedView);
  });
  document.querySelectorAll('[data-shared-view-take-input]').forEach(btn => {
    btn.addEventListener('click', takeSharedViewInput);
  });
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  for (const slot of displaySlots.values()) {
    if (slot.el.classList.contains('display-fullscreen')) {
      slot.toggleFullscreen(false);
      return;
    }
  }
});

