// ── Audio Helpers ──
function downsample(buffer, fromRate, toRate) {
  if (fromRate === toRate) return buffer;
  const ratio = fromRate / toRate;
  const len = Math.round(buffer.length / ratio);
  const result = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    const srcIdx = i * ratio;
    const lo = Math.floor(srcIdx);
    const hi = Math.min(lo + 1, buffer.length - 1);
    const frac = srcIdx - lo;
    result[i] = buffer[lo] * (1 - frac) + buffer[hi] * frac;
  }
  return result;
}

function arrayBufferToBase64(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = '';
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
  return btoa(binary);
}

function getStorageKey() {
  const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
  return provider === 'openai' ? 'openai_api_key' : 'gemini_api_key';
}

// ── Voice Status UI ──
function showVoiceStatus(msg, isError) {
  const el = document.getElementById('voiceStatus');
  el.textContent = msg;
  el.className = 'voice-status' + (isError ? ' error' : '');
  el.classList.remove('hidden');
}
function shouldSuppressServerWebSocketError(msg) {
  if (msg !== 'Server WebSocket error') return false;
  const conn = document.getElementById('sb-conn');
  const connected = conn?.classList.contains('ok') === true;
  if (connected) clearServerWebSocketErrorStatus();
  return connected;
}
function clearServerWebSocketErrorStatus() {
  const el = document.getElementById('voiceStatus');
  if (!el || el.textContent !== 'Server WebSocket error') return;
  el.textContent = '';
  el.className = 'voice-status hidden';
}

function setPrimaryEventStatus(kind, label, title) {
  const group = document.getElementById('sb-conn-group');
  const conn = document.getElementById('sb-conn');
  const connLabel = document.getElementById('sb-conn-label');
  if (group && title) group.title = title;
  if (conn) {
    conn.className = `conn-dot ${kind || 'err'}`;
    if (title) conn.title = title;
  }
  if (connLabel && label) connLabel.textContent = label;
}

function setServerWebSocketStatus(connected) {
  if (dashboardConnectModeEnabled()) return;
  setPrimaryEventStatus(
    connected ? 'ok' : 'err',
    'ws',
    connected
      ? 'WebSocket connection to this daemon is connected'
      : 'WebSocket connection to this daemon is disconnected'
  );
  if (connected) clearServerWebSocketErrorStatus();
}

function setConnectEventStatus(kind, title) {
  setPrimaryEventStatus(kind, 'events', title);
  if (kind === 'ok') clearServerWebSocketErrorStatus();
}

function hideVoiceStatus() {
  document.getElementById('voiceStatus').classList.add('hidden');
}

function requestMakeActive() {
  if (!app || isActiveBrowser) return;
  // Clear passive mode so we can become active
  localStorage.setItem('passive_mode', 'false');
  app.set_passive_mode(false);
  const sent = app.send_make_active();
  sendDashboardVoiceDiagnostic(
    'make_active_request_client',
    sent ? 'request sent to server' : 'request NOT sent (server socket not open)',
  );
  if (sent) {
    document.getElementById('makeActiveBtn').disabled = true;
    showVoiceStatus('Requesting active...');
  } else {
    showVoiceStatus('Takeover request failed', true);
  }
}

function updateActivePassiveUI() {
  const micBtn = document.getElementById('micBtn');
  const makeActiveBtn = document.getElementById('makeActiveBtn');
  const videoBtn = document.getElementById('videoBtn');
  const badge = document.getElementById('sb-active-badge');
  micBtn.classList.toggle('is-disabled', !isActiveBrowser);
  videoBtn.classList.toggle('is-disabled', !isActiveBrowser || !modelConnected);
  makeActiveBtn.classList.toggle('hidden', isActiveBrowser);
  if (isActiveBrowser) {
    badge.textContent = 'Active';
    badge.className = 'active-badge is-active';
    badge.title = 'This browser controls voice/video — click to switch to passive (observe-only) mode';
  } else {
    badge.textContent = 'Passive';
    badge.className = 'active-badge is-passive';
    badge.title = 'This browser is observe-only — click to request voice/video control';
    document.getElementById('sb-voice-label').textContent = 'Passive';
    if (videoActive) stopVideo();
  }
}

// ── Audio Playback ──
function playAudioChunk(b64data) {
  if (!audioCtx) return;
  const outRate = gatewayConfig ? gatewayConfig.output_sample_rate : 24000;
  const binary = atob(b64data);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  const pcm16 = new Int16Array(bytes.buffer);
  const float32 = new Float32Array(pcm16.length);
  for (let i = 0; i < pcm16.length; i++) float32[i] = pcm16[i] / 32768.0;
  const nativeRate = audioCtx.sampleRate;
  const resampled = (outRate !== nativeRate) ? downsample(float32, outRate, nativeRate) : float32;
  const buffer = audioCtx.createBuffer(1, resampled.length, nativeRate);
  buffer.copyToChannel(resampled, 0);
  audioQueue.push(buffer);
  if (!isPlaying) playNext();
}

function playNext() {
  if (audioQueue.length === 0) { isPlaying = false; return; }
  isPlaying = true;
  const buffer = audioQueue.shift();
  const src = audioCtx.createBufferSource();
  src.buffer = buffer;
  src.connect(audioCtx.destination);
  src.onended = playNext;
  src.start();
}

// ── Mic Control ──
async function startMic() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showVoiceStatus('Mic requires HTTPS or localhost', true);
    return;
  }
  if (!audioCtx) audioCtx = new AudioContext();
  if (audioCtx.state === 'suspended') await audioCtx.resume();
  if (!workletReady) {
    try {
      await audioCtx.audioWorklet.addModule('/audio-processor.js');
      workletReady = true;
    } catch (e) {
      showVoiceStatus(`AudioWorklet failed: ${e.message}`, true);
      return;
    }
  }
  if (!mediaStream) {
    try {
      mediaStream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true } });
    } catch (e) {
      showVoiceStatus(`Mic denied: ${e.message}`, true);
      return;
    }
  }
  const source = audioCtx.createMediaStreamSource(mediaStream);
  const targetSR = gatewayConfig ? gatewayConfig.input_sample_rate : 16000;
  const nativeSR = audioCtx.sampleRate;
  workletNode = new AudioWorkletNode(audioCtx, 'audio-capture-processor', { processorOptions: { bufferSize: 4096 } });
  workletNode.port.onmessage = (e) => {
    if (e.data.type !== 'audio') return;
    if (!micActive || !modelConnected || !app) { audioDropLogCount++; return; }
    audioDropLogCount = 0;
    const resampled = downsample(e.data.data, nativeSR, targetSR);
    const pcm16 = new Int16Array(resampled.length);
    for (let i = 0; i < resampled.length; i++)
      pcm16[i] = Math.max(-32768, Math.min(32767, Math.floor(resampled[i] * 32768)));
    app.send_audio(arrayBufferToBase64(pcm16.buffer));
    if (gatewayConfig && gatewayConfig.transcription_enabled) {
      sendDashboardUserAudio(arrayBufferToBase64(pcm16.buffer));
    }
  };
  source.connect(workletNode);
  workletNode.connect(audioCtx.destination);
  showVoiceStatus('Mic active \u2014 speak now');
}

function stopMic() {
  if (workletNode) {
    workletNode.port.postMessage({ type: 'mute' });
    workletNode.disconnect();
    workletNode = null;
  }
  hideVoiceStatus();
}

// ── Video Capture ──
function makeFrameId() {
  frameCounter++;
  return FRAME_STREAM + '-f' + String(frameCounter).padStart(5, '0');
}

async function startVideo() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showVoiceStatus('Camera requires HTTPS or localhost', true);
    return;
  }
  try {
    videoStream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: 'environment', width: { ideal: 1920 }, height: { ideal: 1080 } }
    });
  } catch (e) {
    showVoiceStatus('Camera denied: ' + e.message, true);
    return;
  }
  const videoEl = document.getElementById('videoPreviewEl');
  videoEl.srcObject = videoStream;
  document.getElementById('videoPreviewWrap').classList.add('visible');

  const canvas = document.getElementById('videoCanvas');
  const ctx = canvas.getContext('2d');

  // Capture loop at VIDEO_FPS
  videoIntervalId = setInterval(() => {
    if (!videoActive || !app || !modelConnected) return;
    const vw = videoEl.videoWidth;
    const vh = videoEl.videoHeight;
    if (!vw || !vh) return;

    const frameId = makeFrameId();

    // Live-res: square crop (center) and scale to LIVE_RES x LIVE_RES
    const side = Math.min(vw, vh);
    const sx = (vw - side) / 2;
    const sy = (vh - side) / 2;
    canvas.width = LIVE_RES;
    canvas.height = LIVE_RES;
    ctx.drawImage(videoEl, sx, sy, side, side, 0, 0, LIVE_RES, LIVE_RES);
    const liveJpeg = canvas.toDataURL('image/jpeg', 0.8);
    const liveB64 = liveJpeg.split(',')[1];

    // Skip duplicate frames for voice model — if JPEG size barely changed, screen is static.
    // Still send HQ to server for archival regardless.
    const sizeDelta = Math.abs(liveB64.length - lastLiveFrameLen) / (lastLiveFrameLen || 1);
    const frameDup = sizeDelta < 0.02 && lastLiveFrameLen > 0;
    if (!frameDup) {
      lastLiveFrameLen = liveB64.length;
      app.send_frame(liveB64, frameId);
      tickerFramesSent++;
    } else {
      tickerFramesDropped++;
      sendDashboardVoiceDiagnostic('frame_skip', 'duplicate frame skipped (delta=' + (sizeDelta * 100).toFixed(1) + '%)');
    }
    updateTickerFrames();

    // Send HQ frame (logical resolution) to server for archival (always)
    const camMax = 1920;
    const camScale = Math.min(1, camMax / Math.max(vw, vh));
    canvas.width = Math.round(vw * camScale);
    canvas.height = Math.round(vh * camScale);
    ctx.drawImage(videoEl, 0, 0, canvas.width, canvas.height);
    const hqJpeg = canvas.toDataURL('image/jpeg', 0.80);
    const hqB64 = hqJpeg.split(',')[1];
    sendDashboardVideoFrameToServer(hqB64, frameId, FRAME_STREAM);

    // Update preview UI
    document.getElementById('videoFrameId').textContent = frameId;
  }, 1000 / VIDEO_FPS);

  videoActive = true;
  document.getElementById('videoBtn').classList.add('active');
  showVoiceStatus('Video active — ' + LIVE_RES + 'x' + LIVE_RES + ' @ ' + VIDEO_FPS + ' fps');
  setTimeout(hideVoiceStatus, 3000);
}

function stopVideo() {
  videoActive = false;
  if (videoIntervalId) { clearInterval(videoIntervalId); videoIntervalId = null; }
  if (videoStream) {
    videoStream.getTracks().forEach(t => t.stop());
    videoStream = null;
  }
  document.getElementById('videoPreviewEl').srcObject = null;
  document.getElementById('videoPreviewWrap').classList.remove('visible');
  document.getElementById('videoBtn').classList.remove('active');
  document.getElementById('videoFrameId').textContent = '--';
}

function hasVoiceCredentials() {
  const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
  if (provider === 'openai') return true; // server mints token from OPENAI_API_KEY
  return !!voiceApiKeyGet();
}

// ── Voice Connection ──
async function connectVoice() {
  if (!app || voiceConnecting || !isActiveBrowser) return;
  const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
  const model = (gatewayConfig && gatewayConfig.model) || null;
  const inputRate = (gatewayConfig && gatewayConfig.input_sample_rate) || null;

  let token;
  if (provider === 'openai') {
    // OpenAI: fetch ephemeral client secret from server (uses OPENAI_API_KEY server-side)
    showVoiceStatus('Requesting session token...');
    try {
      const resp = await dashboardJsonFetch('api_voice_session', {}, () => (
        fetch('/session', { method: 'POST' })
      ), 'api_voice_session');
      const data = await resp.json();
      if (data.error) { showVoiceStatus('Token error: ' + data.error, true); return; }
      token = data.client_secret?.value || data.client_secret;
      if (!token) { showVoiceStatus('No client_secret in response', true); return; }
    } catch (e) {
      showVoiceStatus('Failed to get session token', true);
      return;
    }
  } else {
    // Gemini: vault entry when unlocked, else the per-origin localStorage key
    token = voiceApiKeyGet();
    if (!token) { showFirstRunDialog(); return; }
  }

  voiceConnecting = true;
  if (!audioCtx) audioCtx = new AudioContext();
  if (audioCtx.state === 'suspended') audioCtx.resume();
  showVoiceStatus('Connecting...');
  app.connect_voice(provider, token, model, inputRate);
  voiceConnecting = false;
}

// ── Dialogs ──
function showFirstRunDialog() {
  const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
  document.getElementById('firstRunKeyLabel').textContent = provider.charAt(0).toUpperCase() + provider.slice(1) + ' API Key';
  document.getElementById('firstRunDialog').classList.remove('hidden');
}

function showSettingsDialog() {
  const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
  document.getElementById('apiKeyLabel').textContent = provider.charAt(0).toUpperCase() + provider.slice(1) + ' API Key';
  document.getElementById('apiKeyInput').value = voiceApiKeyGet() || '';
  document.getElementById('passiveModeCheckbox').checked = localStorage.getItem('passive_mode') === 'true';
  document.getElementById('settingsDialog').classList.remove('hidden');
}

// ── WASM Init ──
async function main() {
  await init({ module_or_path: '/wasm-web/presence_web_bg.wasm' });
  app = new PresenceWeb();
  installDashboardControlServerSender();

  // Intercept all server messages for AppState routing.
  // The WebRTC dashboard-control dev path reuses this same dispatcher for
  // DataChannel event frames so both transports exercise identical UI code.
  dashboardServerMessageDispatcher = (msg) => {
    // Handle annotation responses before WASM.
    // msg is a JS object (from WASM serde), not a raw string.
    try {
      const d = typeof msg === 'string' ? JSON.parse(msg) : msg;
      if (dashboardShouldDropDuplicateServerMessage(d)) return;
      if (d.t === 'ws_denied') {
        const frame = String(d.frame || '');
        if (!wsDeniedToastShown.has(frame)) {
          wsDeniedToastShown.add(frame);
          showControlToast?.('error', 'Not allowed by your access grant: ' + (d.permission || frame || 'operation'));
        }
        return;
      }
      if (d.event === 'context_snapshot') {
        const contextSid = sessionWindowTargetForLogSession(d.session_id);
        if (contextSid) markSessionWindowPendingActive(contextSid);
        handleContextSnapshot(d);
        scheduleExternalSessionWindowTranscriptSync(d.session_id, 700);
        return;
      }
      if (d.event === 'shared_view') {
        handleSharedViewEvent(d);
        return;
      }
      if (d.event === 'session_note') {
        // Display-only transcript note: rendered end to end in JS (the
        // WASM presence layer does not know this event).
        handleSessionNoteEvent(d);
        return;
      }
      if (d.t === 'browser_workspace_snapshot' || d.event === 'browser_workspace_changed') {
        handleBrowserWorkspaceMessage(d);
        return;
      }
      if (d.t === 'log_replay' && Array.isArray(d.entries)) {
        resetChangesPane();
        const snapshots = d.entries
          .filter(entry => entry && entry.event === 'context_snapshot');
        handleContextReplaySnapshots(snapshots);
        const filtered = {
          ...d,
          entries: d.entries.filter(entry => !entry || entry.event !== 'context_snapshot'),
        };
        applySessionIdentitiesFromReplayEntries(filtered.entries);
        applyExternalIdentitiesFromLogEntries(filtered.entries);
        applySessionGoalsFromReplayEntries(filtered.entries);
        applySessionVitalsFromReplayEntries(filtered.entries);
        resetSessionWindowsForReplay(filtered.entries);
        const wasProcessingLogReplay = processingLogReplay;
        processingLogReplay = true;
        try {
          // session_note entries ride the WASM pipeline as note-styled
          // log_entry rows (see sessionNoteReplayEntryToLogEntry) so the
          // replayed transcript keeps chronological order.
          const cmds = app.handle_server_message({
            ...filtered,
            entries: filtered.entries.map(sessionNoteReplayEntryToLogEntry),
          });
          if (cmds) processCommands(cmds);
        } finally {
          processingLogReplay = wasProcessingLogReplay;
        }
        finalizeActiveCommandOutputGroup();
        // Historical replay can emit lifecycle commands such as session_ended
        // that move the live dashboard to Sessions. The URL remains the
        // navigation source of truth, so re-apply it after replay settles.
        applyCurrentRoute();
        reconcileRecordingStreams();
        return;
      }
      if (d.t === 'state_snapshot' && d.session_id) {
        setDaemonSessionId(d.session_id);
      }
      if (d.event === 'session_identity') {
        applySessionIdentity(d);
        scheduleExternalSessionWindowTranscriptSync(d.backend_session_id || d.backendSessionId || d.session_id, 600);
        // `session_identity` arrives before the external worker subscribes for
        // thread actions; queued detached actions wait for `session_attached`.
      }
      if (
        (d.event === 'done_signal' || d.event === 'round_complete' || d.event === 'task_complete') &&
        d.session_id
      ) {
        scheduleExternalSessionWindowTranscriptSync(d.session_id, 300);
      }
      applyExternalIdentityFromLogEntry(d);
      if (d.event === 'session_relationship') {
        applySessionRelationship(d);
        stationPushSessionRelationshipActivity(d, { renderLog: false });
      }
      if (d.event === 'session_capabilities') {
        applySessionCapabilities(d);
      }
      if (d.event === 'session_attached') {
        const sid = String(d.session_id || d.sessionId || '').trim();
        if (sid) {
          setSessionWindowDetached(sid, false);
          flushPendingDetachedCodexThreadActions(sid);
        }
      }
      if (d.event === 'status' && d.session_id && d.phase) {
        recordRecentSessionStatusPhase(d.session_id, d.phase);
        const sid = statusSessionWindowTarget(d.session_id);
        if (sid && shouldMaterializeStatusSessionWindow(sid)) {
          updateSessionWindow(sid, { phase: d.phase, ended: false });
        }
      }
      if (d.event === 'session_goal') {
        applySessionGoal(d);
      }
      if (d.event === 'session_vitals') {
        applySessionVitals(d);
      }
      if (d.event === 'session_agent_config_result') {
        handleSessionConfigResult(d);
      }
      if (d.event === 'user_message_edit_status') {
        handleUserMessageEditStatus(d);
      }
      if (d.event === 'user_message_rewind') {
        handleUserMessageRewind(d);
      }
      if (d.event === 'codex_thread_action_requested') {
        handleCodexThreadActionRequested(d);
      }
      if (d.event === 'follow_up_status') {
        handleFollowUpStatusUpdate(d);
        return;
      }
      if (d.event === 'approval_required' && d.id !== undefined && d.session_id) {
        approvalSessionIds.set(String(d.id), d.session_id);
      }
      if (d.event === 'user_question' && d.id !== undefined && d.session_id) {
        approvalSessionIds.set(String(d.id), d.session_id);
      }
      if (d.event === 'approval_resolved' && d.id !== undefined
          && typeof pendingQuestion !== 'undefined' && pendingQuestion
          && String(pendingQuestion.id) === String(d.id)
          && (!d.session_id || !pendingQuestion.sessionId || d.session_id === pendingQuestion.sessionId)) {
        // Another frontend answered/dismissed this question — drop our panel.
        clearPendingQuestion();
        hidePanel('question-panel');
      }
      if (d.event === 'autonomy_changed') {
        updateStatusBar({ autonomy: d.autonomy });
        return;
      }
      if (d.t === 'annotation_saved' && d.path) {
        showAnnotationResult(d.path);
        return;
      }
      if (d.t === 'annotation_attached') {
        // Server confirmed the attach landed in the registry. Our local
        // pending list was already populated optimistically — nothing to do
        // unless registration failed, in which case the chip would lie.
        if (d.ok === false) {
          removePendingAttachment(d.frame_id);
        }
        return;
      }
      if (d.t === 'clip_saved') {
        const el = document.getElementById('clip-status');
        if (el) {
          const verb = d.injected ? 'Sent' : 'Saved';
          el.textContent = `${verb} ${d.frames_registered} frames`;
          el.style.color = 'var(--green)';
          setTimeout(() => { el.textContent = ''; el.style.color = ''; }, 5000);
        }
        return;
      }
      // WebRTC signaling — intercept before WASM (not a UiCommand)
      if (d.t === 'display_answer') {
        const s = displaySlots.get(Number(d.display_id));
        if (s) s.handleAnswer(d.sdp);
        return;
      }
      if (d.t === 'display_ice') {
        const s = displaySlots.get(Number(d.display_id));
        if (s) s.handleIceCandidate(d.candidate);
        return;
      }
      if (d.t === 'dashboard_control_answer') {
        if (dashboardControlTransport) {
          dashboardControlTransport
            .handleAnswer(d)
            .catch(err => dashboardControlTransport.handleError(err?.message || String(err)));
        }
        return;
      }
      if (d.t === 'dashboard_control_ice') {
        if (dashboardControlTransport) dashboardControlTransport.handleIceCandidate(d.candidate);
        return;
      }
      if (d.t === 'dashboard_control_error') {
        if (dashboardControlTransport) dashboardControlTransport.handleError(d.error || 'unknown');
        return;
      }
      // Standalone shell fallback for older WASM bundles. Current bundles
      // dispatch these through set_on_terminal_output/exited below so they
      // don't take the generic raw-message bridge.
      if (d.t === 'terminal_output') {
        if (shellFrameMatchesCurrent(d.host_id, d.terminal_id)) {
          handleShellOutput(d.data);
        }
        return;
      }
      if (d.t === 'terminal_exited') {
        if (shellFrameMatchesCurrent(d.host_id, d.terminal_id)) {
          handleShellExited(d.status);
        }
        return;
      }
      if (d.t === 'terminal_opened') {
        if (shellFrameMatchesCurrent(d.host_id, d.terminal_id)) {
          handleShellOpened(d);
        }
        return;
      }
      if (d.t === 'terminal_shared') {
        if (shellFrameMatchesCurrent(d.host_id, d.terminal_id)) {
          handleShellShared(d);
        }
        return;
      }
      if (d.t === 'terminal_error') {
        if (shellFrameMatchesCurrent(d.host_id, d.terminal_id)) {
          handleShellError(d.error);
        }
        return;
      }
      // Display resize — update stored dimensions and status text
      if (d.t === 'display_resize' || d.event === 'display_resize') {
        const slot = displaySlots.get(Number(d.display_id));
        if (slot) {
          slot.width = Number(d.width);
          slot.height = Number(d.height);
          if (slot.statusEl) {
            const res = slot.width > 0 ? ` ${slot.width}x${slot.height}` : '';
            const mode = slot.interactive ? 'interactive' : 'view-only';
            slot.statusEl.textContent = slot.connected
              ? `Connected (${mode})${res}`
              : `Connecting...${res}`;
          }
        }
      }
      // Track user display grant/revoke events. `agent_visible` is
      // absent on wires older than the private-view split; absent
      // means the classic agent share.
      if (d.event === 'user_display_granted') {
        grantedDisplayId = Number(d.display_id || 0);
        const agentVisible = d.agent_visible !== false;
        userDisplayIds.add(grantedDisplayId);
        setDisplayAgentVisibility(grantedDisplayId, agentVisible);
        setUserDisplayState(true, agentVisible);
      } else if (d.event === 'user_display_revoked') {
        const revokedId = Number(d.display_id || 0);
        if (Number(grantedDisplayId) === revokedId) setUserDisplayState(false);
        clearDisplayAgentVisibility(revokedId);
        removeDisplaySlot(revokedId);
        const banner = document.getElementById('display-approval-banner');
        if (banner) banner.classList.add('hidden');
      }
      // Handle display capture lost — disconnect but keep slot for possible re-grant
      if (d.event === 'display_capture_lost') {
        const id = Number(d.display_id || 0);
        const reason = d.reason || 'capture ended';
        const slot = displaySlots.get(id);
        if (slot) {
          slot.disconnect();
          slot.statusEl.textContent = 'Display lost: ' + reason;
          slot.statusEl.className = 'display-status error';
        } else {
          // Capture died before any slot existed — a grant that failed
          // immediately, e.g. "Your display" on a headless box with no
          // display server. Without this branch the failure is invisible:
          // the toggle stays on and no tile ever appears (the daemon-side
          // reason lands only in its journal).
          if (Number(grantedDisplayId) === id && typeof setUserDisplayState === 'function') {
            setUserDisplayState(false);
          }
          if (typeof showControlToast === 'function') {
            showControlToast('error', 'Display unavailable: ' + reason);
          }
        }
        // Hide the approval banner — capture lost supersedes any pending grant.
        const banner = document.getElementById('display-approval-banner');
        if (banner) banner.classList.add('hidden');
      }
      // Approval pending: server has raised the OS portal dialog and is
      // waiting for the user to click Allow on the guest desktop.
      if (d.event === 'display_approval_pending') {
        const banner = document.getElementById('display-approval-banner');
        if (banner) banner.classList.remove('hidden');
      }
      // DisplayReady (or add_display via processCommands) clears the banner.
      if (d.event === 'display_ready') {
        const banner = document.getElementById('display-approval-banner');
        if (banner) banner.classList.add('hidden');
        // Record the display's agent-visibility mode for the tile chip
        // (live events and the gateway's bootstrap replay both carry it).
        if (d.agent_visible !== undefined) {
          setDisplayAgentVisibility(Number(d.display_id || 0), d.agent_visible !== false);
        }
      }
      // Track recording state on display slots
      if (d.event === 'recording_started' && d.stream_name) {
        const slot = slotForRecordingStream(d.stream_name);
        if (slot) { slot.recordingStreamName = d.stream_name; slot.recording = true; slot.recordBtn.innerHTML = '&#x23F9; Stop'; slot.recordBtn.classList.add('active'); slot.deleteRecBtn.style.display = 'none'; }
        handleDebugRecordingEvent(d); // debug tab's Record button tracks its display's streams
      } else if (d.event === 'recording_stopped' && d.stream_name) {
        const slot = slotForRecordingStream(d.stream_name);
        if (slot) { slot.recordingStreamName = d.stream_name; slot.recording = false; slot.recordBtn.innerHTML = '&#x23FA; Record'; slot.recordBtn.classList.remove('active'); slot.deleteRecBtn.style.display = ''; }
        handleDebugRecordingEvent(d);
      } else if (d.event === 'recording_deleted' && d.stream_name) {
        const slot = slotForRecordingStream(d.stream_name);
        if (slot) { if (slot.recordingStreamName === d.stream_name) slot.recordingStreamName = null; slot.recording = false; slot.recordBtn.innerHTML = '&#x23FA; Record'; slot.recordBtn.classList.remove('active'); slot.deleteRecBtn.style.display = 'none'; }
        deleteRecordingStream(d.stream_name);
      }
      // Display transport metrics (per-display sections)
      if (d.event === 'display_metrics') {
        updateDisplayMetrics(d);
      }
      if (eventRefreshesSessionMetadata(d.event)) {
        scheduleSessionsMetadataRefresh();
      }
      maybeHandleDashboardTunneledServerMessage(d);
    } catch(_) {}
    const cmds = app.handle_server_message(msg);
    if (cmds) processCommands(cmds);
  };
  app.set_on_raw_message((msg) => {
    if (dashboardServerMessageDispatcher) dashboardServerMessageDispatcher(msg);
  });

  // Apply deep links as soon as the WASM app exists. Peer/settings/session
  // hydration happens later and can be slow or fail independently; it should
  // not leave a refreshed dashboard showing the default Activity pane while
  // the URL says `#station`, `#sessions`, etc.
  applyCurrentRoute();

  if (app.set_on_terminal_output) {
    app.set_on_terminal_output((hostId, terminalId, data) => {
      if (shellFrameMatchesCurrent(hostId, terminalId)) {
        handleShellOutput(data);
      }
    });
  }

  if (app.set_on_terminal_exited) {
    app.set_on_terminal_exited((hostId, terminalId, status) => {
      if (shellFrameMatchesCurrent(hostId, terminalId)) {
        handleShellExited(status);
      }
    });
  }

  // Connection state indicator
  app.set_on_server_state((connected) => {
    setServerWebSocketStatus(connected);
    dashboardUpdateTransportStatus();
  });

  // ── Voice Callbacks ──
  app.set_on_voice_ready(() => {
    const isReconnect = voiceHadPriorSession;
    voiceHadPriorSession = false;
    modelConnected = true;
    if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
    document.getElementById('sb-voice').className = 'voice-dot ok';
    document.getElementById('videoBtn').classList.remove('is-disabled');
    const provider = (gatewayConfig && gatewayConfig.provider) || 'gemini';
    document.getElementById('sb-voice-label').textContent = provider.charAt(0).toUpperCase() + provider.slice(1);
    showVoiceStatus(isReconnect ? 'Reconnected' : 'Voice ready');
    setTimeout(hideVoiceStatus, 3000);
    if (isReconnect) {
      // Reconnected after a disconnect — this is a fresh session with no prior
      // context. Send a strong grounding message to prevent the model from
      // confabulating a continuation of a conversation that no longer exists.
      app.send_text('[System: Voice session reconnected after a disconnection. This is a fresh session — you have NO memory of any prior conversation, frames, or analysis. Briefly tell the user you reconnected, then wait for them to speak. Do NOT describe the screen or reference prior context.]');
      sendDashboardVoiceDiagnostic('reconnected', provider + ' voice model reconnected after disconnect');
    } else if (storedConversationCtx) {
      app.send_text('[System: conversation so far]\n' + storedConversationCtx);
      storedConversationCtx = null;
    } else {
      // Send a grounding message so Gemini enters tool-calling mode.
      // Without any client_content after setupComplete, Gemini Live in
      // audio-only mode may narrate tool calls in speech instead of
      // issuing them as protocol-level toolCall messages.
      app.send_text('[System: Ready. Waiting for user.]');
    }
    app.inject_pending_approval_if_any();
    sendDashboardVoiceDiagnostic('connected', provider + ' voice model ready');
  });

  app.set_on_voice_audio((b64) => playAudioChunk(b64));

  app.set_on_voice_text((text) => {
    sendDashboardVoiceLog(text, undefined);
  });

  app.set_on_voice_transcript((text) => {
    sendDashboardVoiceLog(text, 'transcript');
  });

  app.set_on_voice_tool_call((call) => {
    app.handle_voice_tool_call(call);
  });

  app.set_on_voice_interrupted(() => {
    sendDashboardVoiceDiagnostic('interrupted', 'voice model interrupted by user');
  });

  app.set_on_live_usage((usage) => {
    if (!usage) return;
    const cmds = app.handle_live_usage(usage);
    if (cmds) processCommands(cmds);
  });

  app.set_on_error((msg) => {
    if (shouldSuppressServerWebSocketError(msg)) return;
    if (!msg.startsWith('Server')) {
      if (modelConnected) voiceHadPriorSession = true;
      modelConnected = false;
      if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
      document.getElementById('sb-voice').className = 'voice-dot err';
      if (videoActive) stopVideo();
      document.getElementById('videoBtn').classList.add('is-disabled');
    }
    showVoiceStatus(msg, true);
    sendDashboardVoiceDiagnostic('error', msg);
  });

  app.set_on_diagnostic((kind, detail) => {
    sendDashboardVoiceDiagnostic(kind, detail);
  });

  app.set_on_session_changed(() => {
    if (modelConnected) {
      if (videoActive) stopVideo();
      disconnectDashboardVoice();
      modelConnected = false;
      if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
      document.getElementById('sb-voice').className = 'voice-dot err';
      document.getElementById('videoBtn').classList.add('is-disabled');
      connectVoice();
    }
  });

  app.set_on_inject_voice_text((text) => {
    app.send_text(text);
  });

  app.set_on_inject_voice_text_passive((text) => {
    app.send_text_passive(text);
  });

  app.set_on_tool_response((call, result) => {
    app.send_voice_tool_response(call, result);
  });

  app.set_on_inject_voice_image((base64Data, label) => {
    // Cap at logical resolution (not Retina) before injecting into voice model.
    // The stored HQ frames are already at logical res after our fix, but guard
    // against older/oversized frames that could crash Gemini (~1MB limit).
    const MAX_INJECT = 1920;
    const img = new Image();
    img.onload = () => {
      const scale = Math.min(1, MAX_INJECT / Math.max(img.width, img.height));
      const w = Math.round(img.width * scale);
      const h = Math.round(img.height * scale);
      const c = document.createElement('canvas');
      c.width = w; c.height = h;
      c.getContext('2d').drawImage(img, 0, 0, w, h);
      const b64 = c.toDataURL('image/jpeg', 0.8).split(',')[1];
      app.send_frame(b64, label);
    };
    img.src = 'data:image/jpeg;base64,' + base64Data;
  });

  app.set_on_state_snapshot((msg) => {
    if (msg && msg.is_active !== undefined) {
      isActiveBrowser = msg.is_active;
      updateActivePassiveUI();
    }
    if (msg && msg.conversation_context) storedConversationCtx = msg.conversation_context;
    if (modelConnected) app.inject_pending_approval_if_any();
    // Bootstrap/reconnect: populate the timeline from whatever the
    // server has now so users coming in mid-session see it immediately.
    if (typeof refreshHistory === 'function') refreshHistory();
    restorePersistedSessionWindowsSoon();
  });

  app.set_on_server_event((evt) => {
    if (modelConnected) app.handle_server_event(evt);
  });

  // Phase 5c: per-display input-authority state from the server.  Fired
  // for both the bootstrap snapshot at WS connect (one event per active
  // display) and live transitions (Request/Release/WS-close elsewhere,
  // plus DisplayReady for fresh sessions starting at unclaimed).  The
  // server has already resolved its holder ID against this connection's
  // ID, so `state` is one of `'you' | 'other' | 'unclaimed'` — connection
  // IDs never reach JS.  The DisplaySlot itself owns the chip + button +
  // interactive-mode promotion logic; we just route by display_id.
  app.set_on_display_input_authority_change((displayId, state) => {
    const did = Number(displayId);
    const slot = displaySlots.get(did);
    if (slot) {
      slot.setAuthority(state);
    } else {
      // Slot may not exist yet: when the server grants a fresh display,
      // it broadcasts both `display_ready` (which addDisplaySlot consumes
      // to create the slot) and `display_input_authority_state` (this
      // message), and the two travel through different channels in the
      // per-WS outbound select — they can race.  If state arrives first,
      // queue it; addDisplaySlot drains the queue right after creating
      // the slot.  Without this buffer the chip would stay at `unknown`
      // until the next authority transition fires.
      pendingAuthorityStates.set(did, state);
    }
  });

  app.set_on_force_disconnect((reason) => {
    sendDashboardVoiceDiagnostic('make_active_force_disconnect_client', 'reason=' + reason);
    if (modelConnected) {
      if (videoActive) stopVideo();
      disconnectDashboardVoice();
      modelConnected = false;
      if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
      micActive = false;
      document.getElementById('micBtn').classList.remove('active');
      document.getElementById('videoBtn').classList.add('is-disabled');
      stopMic();
    }
    isActiveBrowser = false;
    document.getElementById('sb-voice').className = 'voice-dot';
    document.getElementById('sb-voice-label').textContent = 'Passive';
    updateActivePassiveUI();
    showVoiceStatus('Voice handed to another browser');
    setTimeout(hideVoiceStatus, 5000);
  });

  app.set_on_active_granted((handoverContext, conversationContext) => {
    sendDashboardVoiceDiagnostic(
      'make_active_granted_client',
      'handover=' + (handoverContext ? 'yes' : 'no') + ', conversation=' + (conversationContext ? 'yes' : 'no'),
    );
    isActiveBrowser = true;
    document.getElementById('makeActiveBtn').disabled = false;
    updateActivePassiveUI();
    if (conversationContext) storedConversationCtx = conversationContext;
    showVoiceStatus('Active');
    setTimeout(hideVoiceStatus, 3000);
    // Only auto-connect voice if credentials are available.
    if (hasVoiceCredentials()) {
      connectVoice();
      if (handoverContext) {
        const waitForVoice = () => {
          if (modelConnected) {
            app.send_text('[System: handover context] ' + handoverContext + '. Full history available via recall_memory.');
          } else { setTimeout(waitForVoice, 200); }
        };
        setTimeout(waitForVoice, 500);
      }
    }
  });

  // Store app globally for reconnect/debug hooks.
  window.__presenceWeb = app;

  // Fire-and-forget: the vault only needs the hosted origin (same-origin
  // fetch + the login PRF secret), not the control transport — and it
  // must never block or be blocked by dashboard bootstrap.
  vaultInit();

  let connectBootstrapReady = !dashboardConnectModeEnabled();
  if (dashboardConnectModeEnabled()) {
    setConnectEventStatus('warn', 'Connecting dashboard events through Hosted Connect');
    try {
      await maybeStartDashboardControlTransport();
      await waitForDashboardControlReady(30000);
      await hydrateDashboardFromControl();
      await accessFleetHydrateFromHosted();
      connectBootstrapReady = true;
      setConnectEventStatus('ok', 'Dashboard events are live through verified Hosted Connect');
    } catch (err) {
      console.warn('[dashboard-control] Connect dashboard bootstrap failed', err);
      dashboardSetControlLastError(err?.message || String(err), err?.controlErrorKind || '');
      dashboardUpdateTransportStatus();
      setConnectEventStatus('err', 'Hosted Connect dashboard events failed');
      scheduleDashboardConnectReconnect(err?.message || String(err), { delayMs: 1000 });
    }
  } else if (window.__intendantPort && window.__intendantBackendTls) {
    // macOS app over mTLS: a browser WebSocket cannot present the client
    // certificate (the intendant:// proxy can't intercept WS upgrades),
    // so the legacy event stream can never connect — the WebRTC control
    // transport, whose signaling rides the proxy, carries events instead.
    console.info('[app] mTLS bundle: skipping legacy WebSocket; events flow through the control transport');
    try {
      const [cfg, card] = await Promise.all([
        fetch('/config').then(r => r.json()).catch(() => ({})),
        fetch('/.well-known/agent-card.json').then(r => r.json()).catch(() => null),
      ]);
      applyGatewayConfig(cfg);
      applyAgentCardIdentity(card);
    } catch {}
    maybeStartDashboardControlTransport();
  } else {
    // Connect to server over the normal daemon-origin WebSocket.
    const wsUrl = buildWsUrl();
    app.connect_server(wsUrl);

    // Fetch runtime config (/config) and identity (agent card) in
    // parallel. /config is voice/WebRTC-scoped now; identity lives on
    // the Agent Card at /.well-known/agent-card.json — served by every
    // Intendant daemon as its canonical "who am I" surface. We tolerate
    // either fetch failing independently so a misconfigured daemon
    // still gets partial UI rather than a blank dashboard.
    try {
      const [cfg, card] = await Promise.all([
        fetch('/config').then(r => r.json()).catch(() => ({})),
        fetch('/.well-known/agent-card.json').then(r => r.json()).catch(() => null),
      ]);
      applyGatewayConfig(cfg);
      applyAgentCardIdentity(card);
    } catch {}

    maybeStartDashboardControlTransport();
  }

  // Initialize multi-host state now that self-label is resolved and the
  // WASM app is ready. Hydrates the peer list from the server-side
  // PeerRegistry via /api/peers (declarative [[peer]] sections from
  // intendant.toml + any peers added through the dashboard at runtime).
  await initDaemons();

  // Fetch settings once at startup so the status bar badge (external
  // agent) reflects the persisted value without waiting for the user
  // to open the Settings tab. Idempotent — visiting Settings after
  // this won't refetch.
  if (connectBootstrapReady) {
    loadSettings();
    loadNewSessionProjectRoot();
  }

  // Restore pure-client toggles from localStorage. These are UI
  // preferences that don't belong in intendant.toml but also shouldn't
  // reset on every browser refresh.
  restoreClientToggles();

  // Apply the URL hash to navigate to the right tab/sub-tab. This
  // runs AFTER the Settings sub-tab localStorage fallback has been
  // applied (inside initDaemons), so an explicit sub-tab in the hash
  // wins but an empty hash falls through to the remembered value.
  applyCurrentRoute();

  // Check for existing recordings (late-connecting browser)
  if (connectBootstrapReady) {
    reconcileRecordingStreams();
  }

  // Passive mode from localStorage or ?passive=1 URL param
  const urlPassive = new URLSearchParams(window.location.search).get('passive') === '1';
  if (urlPassive || localStorage.getItem('passive_mode') === 'true') {
    isActiveBrowser = false;
    app.set_passive_mode(true);
    if (urlPassive) localStorage.setItem('passive_mode', 'true');
    updateActivePassiveUI();
  }

  // ── Mic Button ──
  document.getElementById('micBtn').addEventListener('click', async () => {
    if (!isActiveBrowser) return;
    if (!modelConnected) {
      if (!hasVoiceCredentials()) { showFirstRunDialog(); return; }
      connectVoice();
    }
    micActive = !micActive;
    document.getElementById('micBtn').classList.toggle('active', micActive);
    if (micActive) await startMic(); else stopMic();
  });

  // ── Video Button ──
  // Video requires voice to be active first — enable/disable accordingly.
  document.getElementById('videoBtn').addEventListener('click', async () => {
    if (!isActiveBrowser || !modelConnected) return;
    if (videoActive) {
      stopVideo();
    } else {
      await startVideo();
    }
  });

  // Long-press mic button opens settings
  let micLongPress = null;
  document.getElementById('micBtn').addEventListener('contextmenu', (e) => {
    e.preventDefault();
    showSettingsDialog();
  });

  // Make Active button
  document.getElementById('makeActiveBtn').addEventListener('click', () => {
    requestMakeActive();
  });

  // Active/Passive badge in status bar — click to toggle
  document.getElementById('sb-active-badge').addEventListener('click', () => {
    if (isActiveBrowser) {
      // Switch to passive: enable passive mode and disconnect voice
      localStorage.setItem('passive_mode', 'true');
      app.set_passive_mode(true);
      if (modelConnected) {
        if (videoActive) stopVideo();
        disconnectDashboardVoice();
        modelConnected = false;
        if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
        micActive = false;
        document.getElementById('micBtn').classList.remove('active');
        document.getElementById('videoBtn').classList.add('is-disabled');
        stopMic();
      }
      isActiveBrowser = false;
      document.getElementById('sb-voice').className = 'voice-dot';
      updateActivePassiveUI();
    } else {
      // Request active — same as Make Active button
      requestMakeActive();
    }
  });

  // ── Dialog Handlers ──
  document.getElementById('firstRunSave').addEventListener('click', () => {
    const key = document.getElementById('firstRunKeyInput').value.trim();
    if (key) {
      voiceApiKeySet(key);
      document.getElementById('firstRunDialog').classList.add('hidden');
      connectVoice();
    }
  });
  document.getElementById('firstRunSkip').addEventListener('click', () => {
    document.getElementById('firstRunDialog').classList.add('hidden');
  });
  document.getElementById('settingsSave').addEventListener('click', () => {
    const key = document.getElementById('apiKeyInput').value.trim();
    if (key) voiceApiKeySet(key);
    const passive = document.getElementById('passiveModeCheckbox').checked;
    localStorage.setItem('passive_mode', passive);
    app.set_passive_mode(passive);
    if (passive) { isActiveBrowser = false; updateActivePassiveUI(); }
    document.getElementById('settingsDialog').classList.add('hidden');
  });
  document.getElementById('settingsCancel').addEventListener('click', () => {
    document.getElementById('settingsDialog').classList.add('hidden');
  });

  // Clean up on unload
  window.addEventListener('beforeunload', () => {
    if (modelConnected && app) disconnectDashboardVoice();
  });
}

