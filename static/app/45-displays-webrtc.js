// ── Displays (WebRTC — video track + data channels for input) ──

function dashboardApplyServerFrames(frames) {
  if (!Array.isArray(frames) || !dashboardServerMessageDispatcher) return;
  for (const frame of frames) {
    if (frame && typeof frame === 'object') {
      dashboardServerMessageDispatcher(frame);
    }
  }
}

async function requestDisplayInputAuthorityForSlot(displayId) {
  if (dashboardTransport?.canUseDisplayInputAuthority?.()) {
    try {
      const result = await dashboardTransport.requestDisplayInputAuthority(displayId, {
        timeoutMs: 5000,
      });
      if (result?.ok === false) throw new Error(result.error || 'display authority request failed');
      dashboardApplyServerFrames(result?.frames);
      return true;
    } catch (err) {
      if (dashboardConnectModeEnabled()) {
        console.warn('[dashboard-control] display authority request failed', err);
        if (typeof showControlToast === 'function') {
          showControlToast('error', err?.message || 'Display input authority is unavailable');
        }
        return false;
      }
      console.warn('[dashboard-control] display authority request failed, falling back to /ws', err);
    }
  }
  if (dashboardConnectModeEnabled()) {
    if (typeof showControlToast === 'function') {
      showControlToast('error', 'Display input is unavailable until dashboard access reconnects');
    }
    return false;
  }
  if (typeof app !== 'undefined' && app &&
      typeof app.request_display_input_authority === 'function') {
    app.request_display_input_authority(displayId);
    return true;
  }
  return false;
}

async function releaseDisplayInputAuthorityForSlot(displayId) {
  if (dashboardTransport?.canUseDisplayInputAuthority?.()) {
    try {
      const result = await dashboardTransport.releaseDisplayInputAuthority(displayId, {
        timeoutMs: 5000,
      });
      if (result?.ok === false) throw new Error(result.error || 'display authority release failed');
      dashboardApplyServerFrames(result?.frames);
      return true;
    } catch (err) {
      if (dashboardConnectModeEnabled()) {
        console.warn('[dashboard-control] display authority release failed', err);
        if (typeof showControlToast === 'function') {
          showControlToast('error', err?.message || 'Display input authority release is unavailable');
        }
        return false;
      }
      console.warn('[dashboard-control] display authority release failed, falling back to /ws', err);
    }
  }
  if (dashboardConnectModeEnabled()) {
    if (typeof showControlToast === 'function') {
      showControlToast('error', 'Display input release is unavailable until dashboard access reconnects');
    }
    return false;
  }
  if (typeof app !== 'undefined' && app &&
      typeof app.release_display_input_authority === 'function') {
    app.release_display_input_authority(displayId);
    return true;
  }
  return false;
}

function sendDisplayInputForSlot(displayId, msg) {
  return Boolean(dashboardTransport?.displayInput?.(displayId, msg));
}

// The clipboard-failure toast (`noteDisplayClipboardWriteFailure`) and the
// shared getStats() summarizer (`summarizeRtcStats`) moved verbatim to
// 45-display-viewer-core.js — the shared viewer core both this class and
// PeerDisplayConnection (52-peer-display.js) compose over.

class DisplaySlot {
  constructor(displayId, width, height) {
    this.displayId = displayId;
    this.width = width || 0;
    this.height = height || 0;
    this.pc = null;
    this.controlChannel = null;  // reliable, ordered — keys, mouse buttons
    this.pointerChannel = null;  // unreliable, maxRetransmits:0 — mouse move, scroll
    this.clipboardChannel = null; // reliable, ordered — clipboard sync
    this.videoEl = null;
    this.interactive = false;
    // Phase 5c: per-display input-authority state, populated by the WASM
    // callback wired to `set_on_display_input_authority_change`. Starts at
    // `'unknown'` and is replaced with one of `'you' | 'other' | 'unclaimed'`
    // on the first `display_input_authority_state` frame the gateway sends
    // (bootstrap on WS connect, then live on every authority transition).
    // The chip + button visibility renders against this; `interactive` only
    // ever flips to `true` while state === 'you'.  Source of truth is the
    // server gate at `web_gateway::gated_input_handler` — JS gating below
    // is UX consistency only.
    this.authorityState = 'unknown';
    // Set when `takeControl` requests authority and is waiting for the
    // server's `'you'` confirmation; on arrival, `setAuthority` promotes
    // us into interactive mode rather than just rendering the chip.
    this._takeControlPending = false;
    this.connected = false;
    this.streaming = false;
    this.recordingStreamName = null;
    this._answerApplied = false;     // true after setRemoteDescription completes
    this._pendingCandidates = [];    // queued until answer is applied
    this._reconnectAttempts = 0;     // ICE failure reconnect counter
    // True once the user has intentionally closed this slot (display
    // toggled off, slot removed by user_display_revoked, etc). Gates the
    // `onconnectionstatechange` 'failed' retry path so we don't spam
    // offers at a server that deliberately tore the session down. Without
    // this flag, revoke → server stops session → browser sees ICE failed
    // → retry loop, which visually manifests as "off doesn't stay off"
    // even though the server keeps (correctly) dropping each new offer.
    this._closedByUser = false;
    this._streamIntervalId = null;
    this._streamFrameCounter = 0;
    // Negotiation epoch: incremented on every connect() and disconnect()
    // so stale async callbacks (first-frame hooks, watchdogs) from a
    // previous RTCPeerConnection can detect they are outdated and bail.
    this._connectEpoch = 0;
    // True once the current pc has rendered a real frame; gates the
    // no-track watchdog and the "Waiting for first frame…" overlay stage.
    this._firstFrameSeen = false;
    this._noTrackTimer = null;      // no-video watchdog (ported from peer path)
    this._statsTimer = null;        // live metrics chip sampler
    this._statsPrev = null;
    this._takePendingTimer = null;  // Take Control 5s no-answer timeout
    this._streamCanvas = document.createElement('canvas');
    this._focusResizeObserver = null;
    this._boundHandlers = {};
    this.el = document.createElement('div');
    this.el.className = 'display-slot';
    const label = displayLabel(displayId);
    this.el.innerHTML = `
      <div class="display-toolbar">
        <span class="display-label">${label}</span>
        <span class="display-visibility" id="ds-visibility-${displayId}" style="display:none"></span>
        <span class="display-status" id="ds-status-${displayId}">Connecting...</span>
        <span class="display-input-authority" id="ds-authority-${displayId}" style="display:none" title="Input authority for this display: who can drive keyboard/mouse. Phase 5c."></span>
        <input class="release-note" id="ds-note-${displayId}" placeholder="Note (optional)" style="display:none">
        <button class="stream-btn" id="ds-stream-${displayId}" title="Continuously send screenshots of this display to the live presence (voice) model. Main agents are not affected.">Stream</button>
        <button class="ann-attach-btn" id="ds-attach-${displayId}" title="Capture current frame and attach to next task">Attach</button>
        <button class="annotate-btn" id="ds-annotate-${displayId}" title="Freeze current frame and annotate it">&#9998; Annotate</button>
        <button class="callout-btn" id="ds-callout-${displayId}" aria-pressed="false" disabled title="Call out a region: arm, then drag a rectangle on the frame to attach it to the next task (needs input control)">&#x2316; Callout</button>
        <button class="record-btn" id="ds-record-${displayId}" title="Record this display (ffmpeg)">Record</button>
        <button class="display-fullscreen-btn" id="ds-fullscreen-${displayId}" title="Full screen">&#x26F6;</button>
        <button class="display-close-btn" id="ds-close-${displayId}" title="Close this display stream">&times;</button>
        <button class="take-control-btn" id="ds-take-${displayId}" title="Take interactive control of this display (keyboard and mouse)">Take Control</button>
        <button class="release-control-btn" id="ds-release-${displayId}" style="display:none" title="Release control and return display to view-only mode">Release</button>
        <button class="delete-recording-btn" id="ds-delete-rec-${displayId}" style="display:none" title="Delete recording files for this display">Delete</button>
        <span class="stream-frame-id" id="ds-frame-${displayId}" style="display:none;font-size:10px;color:var(--overlay0);margin-left:auto"></span>
      </div>
      <div class="display-canvas" id="display-canvas-${displayId}"></div>`;
    this.statusEl = this.el.querySelector(`#ds-status-${displayId}`);
    this.visibilityEl = this.el.querySelector(`#ds-visibility-${displayId}`);
    this.authorityEl = this.el.querySelector(`#ds-authority-${displayId}`);
    this.noteInput = this.el.querySelector(`#ds-note-${displayId}`);
    this.takeBtn = this.el.querySelector(`#ds-take-${displayId}`);
    this.releaseBtn = this.el.querySelector(`#ds-release-${displayId}`);
    this.streamBtn = this.el.querySelector(`#ds-stream-${displayId}`);
    this.frameIdEl = this.el.querySelector(`#ds-frame-${displayId}`);
    this.canvasEl = this.el.querySelector(`#display-canvas-${displayId}`);
    this.recordBtn = this.el.querySelector(`#ds-record-${displayId}`);
    this.fullscreenBtn = this.el.querySelector(`#ds-fullscreen-${displayId}`);
    this.closeBtn = this.el.querySelector(`#ds-close-${displayId}`);
    this.deleteRecBtn = this.el.querySelector(`#ds-delete-rec-${displayId}`);
    this.attachBtn = this.el.querySelector(`#ds-attach-${displayId}`);
    this.annotateBtn = this.el.querySelector(`#ds-annotate-${displayId}`);
    this.calloutBtn = this.el.querySelector(`#ds-callout-${displayId}`);
    this.recording = false;

    // Video element for WebRTC media track
    this.videoEl = document.createElement('video');
    this.videoEl.autoplay = true;
    this.videoEl.playsinline = true;
    this.videoEl.muted = true;
    this.videoEl.style.width = '100%';
    this.videoEl.style.backgroundColor = '#000';
    this.canvasEl.appendChild(this.videoEl);
    // In-stage connection status overlay (time-to-first-frame): staged
    // copy while negotiating ("Negotiating…" → "Waiting for first
    // frame…"), and a visible error state with a Reconnect button on
    // dead ends. Lives inside canvasEl so it follows the stage through
    // thumb/fullscreen reparenting.
    this.overlayEl = document.createElement('div');
    this.overlayEl.className = 'display-stage-overlay';
    this.overlayEl.style.display = 'none';
    this.canvasEl.appendChild(this.overlayEl);
    // Live metrics chip ("LIVE · fps · kbps · relay"), fed by the
    // getStats sampler while connected. Replaces the pure-CSS LIVE pill
    // whenever it has data (see ui2-live.css `:has(.display-live-metrics
    // .active)` rule).
    this.metricsEl = document.createElement('div');
    this.metricsEl.className = 'display-live-metrics';
    this.metricsEl.style.display = 'none';
    this.canvasEl.appendChild(this.metricsEl);
    const rerenderSharedFocus = () => {
      if (!sharedViewState.visible) return;
      if (sharedViewState.displayId !== null && Number(this.displayId) !== sharedViewState.displayId) return;
      renderSharedViewFocus(this, sharedViewState.region, sharedViewState.note);
    };
    this.videoEl.addEventListener('loadedmetadata', rerenderSharedFocus);
    this.videoEl.addEventListener('resize', rerenderSharedFocus);
    if (typeof ResizeObserver !== 'undefined') {
      this._focusResizeObserver = new ResizeObserver(rerenderSharedFocus);
      this._focusResizeObserver.observe(this.canvasEl);
      this._focusResizeObserver.observe(this.videoEl);
    }

    this.takeBtn.addEventListener('click', () => this.takeControl());
    this.releaseBtn.addEventListener('click', () => this.releaseControl());
    this.streamBtn.addEventListener('click', () => this.toggleStreaming());
    this.recordBtn.addEventListener('click', () => this.toggleRecording());
    this.fullscreenBtn.addEventListener('click', () => this.toggleFullscreen());
    this.closeBtn.addEventListener('click', () => this.closeDisplay());
    this.deleteRecBtn.addEventListener('click', () => this.deleteRecording());
    this.attachBtn.addEventListener('click', () => this.attachCurrentFrame());
    this.annotateBtn.addEventListener('click', () => this.annotateCurrentFrame());
    this.calloutBtn.addEventListener('click', () => this.toggleCallout());
  }

  // Same budget as PeerDisplayConnection.NO_TRACK_TIMEOUT_MS — both are
  // the shared viewer-core constant, so the two paths' patience can't
  // drift. The static stays public (QA overrides keep working: the
  // watchdog arms with the static, not the constant).
  static NO_TRACK_TIMEOUT_MS = DISPLAY_VIEWER_NO_TRACK_TIMEOUT_MS;

  toggleFullscreen(force) {
    const want = force === undefined
      ? !this.el.classList.contains('display-fullscreen')
      : !!force;
    if (want) {
      for (const slot of displaySlots.values()) {
        if (slot === this) continue;
        slot.el.classList.remove('display-fullscreen');
        if (slot.fullscreenBtn) {
          slot.fullscreenBtn.innerHTML = '&#x26F6;';
          slot.fullscreenBtn.title = 'Full screen';
        }
      }
    }
    this.el.classList.toggle('display-fullscreen', want);
    const anyFullscreen = want || Array.from(displaySlots.values()).some(slot =>
      slot !== this && slot.el.classList.contains('display-fullscreen')
    );
    document.body.classList.toggle('display-fullscreen-open', anyFullscreen);
    if (this.fullscreenBtn) {
      this.fullscreenBtn.innerHTML = want ? '&times;' : '&#x26F6;';
      this.fullscreenBtn.title = want ? 'Exit full screen' : 'Full screen';
    }
  }

  closeDisplay() {
    const displayId = Number(this.displayId);
    dispatchDashboardActionMsg({ action: 'revoke_user_display', display_id: displayId });
    removeDisplaySlot(displayId);
    clearDisplayAgentVisibility(displayId);
    if (Number(grantedDisplayId) === displayId) {
      setUserDisplayState(false);
    }
  }

  captureCurrentFrame(quality = 0.85, options = {}) {
    if (!this.connected || !this.videoEl || !this.videoEl.videoWidth) {
      return null;
    }
    const sw = this.videoEl.videoWidth;
    const sh = this.videoEl.videoHeight;
    const dpr = window.devicePixelRatio || 1;
    const logicalResolution = options.logicalResolution === true;
    const w = logicalResolution ? Math.round(sw / dpr) : sw;
    const h = logicalResolution ? Math.round(sh / dpr) : sh;
    const c = document.createElement('canvas');
    c.width = w;
    c.height = h;
    c.getContext('2d').drawImage(this.videoEl, 0, 0, w, h);
    const dataUrl = c.toDataURL('image/jpeg', quality);
    const b64 = dataUrl.split(',')[1];
    return { canvas: c, dataUrl, b64, width: w, height: h };
  }

  /// Capture the currently-rendered video frame and queue it as a pending
  /// attachment. Works whether or not the display is currently streaming —
  /// just rasterizes whatever the <video> element is showing right now.
  async attachCurrentFrame() {
    const frame = this.captureCurrentFrame(0.85, { logicalResolution: true });
    if (!frame) {
      this.attachBtn.title = 'No frame available yet';
      setTimeout(() => { this.attachBtn.title = 'Capture current frame and attach to next task'; }, 2000);
      return;
    }
    const dataUrl = frame.dataUrl;
    const b64 = dataUrl.split(',')[1];
    // Use a deterministic frame_id scheme so attachments are distinguishable
    // from streamed frames in the registry.
    if (!this._attachCounter) this._attachCounter = 0;
    this._attachCounter++;
    const stream = 'display_' + this.displayId + '_attach';
    const frameId = stream + '-f' + String(this._attachCounter).padStart(5, '0');
    const payload = {
      t: 'annotation_attach',
      frame_id: frameId,
      stream: stream,
      data: b64,
      note: '',
    };
    try {
      await sendDashboardMediaUpload(
        'api_media_annotation_attach',
        { frame_id: frameId, stream, note: '' },
        dashboardControlBase64ToBytes(b64),
        payload,
        'annotation attach'
      );
    } catch (err) {
      dashboardMediaTransferFailed(err, 'annotation attach');
      return;
    }
    if (typeof addPendingAttachment === 'function') {
      addPendingAttachment({
        frameId,
        stream,
        note: '',
        dataUrl,
      });
    }
    // Brief visual confirmation
    const orig = this.attachBtn.innerHTML;
    this.attachBtn.innerHTML = '&#x2713; Attached';
    setTimeout(() => { this.attachBtn.innerHTML = orig; }, 1500);
  }

  // The surface-provider contract consumed by 47-annotation-clips'
  // live-annotation editor and callout arming — the field-by-field
  // enumeration lives above setLiveAnnotationButton there. Zero behavior
  // change vs. the pre-provider slot coupling: every getter returns
  // exactly the member the editor used to reach into directly.
  _annotationSurfaceProvider() {
    return {
      owner: this,
      displayId: this.displayId,
      streamBase: 'display_' + this.displayId,
      stageEl: () => this.canvasEl,
      liveSurfaceEl: () => this.videoEl,
      annotateBtn: () => this.annotateBtn,
      toolbarHostEl: () => this.el,
    };
  }

  annotateCurrentFrame() {
    const frame = this.captureCurrentFrame(0.92);
    if (!frame) {
      this.annotateBtn.title = 'No frame available yet';
      setTimeout(() => { this.annotateBtn.title = 'Freeze current frame and annotate it'; }, 2000);
      return;
    }
    enterLiveAnnotationMode(this._annotationSurfaceProvider(), frame);
  }

  // Toolbar-armed Callout: one-shot region flag shipped through the
  // annotation-attach lane. Shared machinery lives in 47-annotation-clips
  // (toggleLiveCallout); armable only while input authority is 'you'
  // (button disabled otherwise, disarmed on authority loss).
  toggleCallout() {
    toggleLiveCallout({
      provider: this._annotationSurfaceProvider(),
      button: this.calloutBtn,
      captureFrame: (q) => this.captureCurrentFrame(q),
    });
  }

  sendLegacyDisplaySignal(payload) {
    if (!app) return false;
    if (app.send_raw) {
      app.send_raw(JSON.stringify(payload));
      return true;
    }
    if (app.send_server_action) {
      app.send_server_action(payload);
      return true;
    }
    return false;
  }

  async sendDisplayIceCandidate(candidate) {
    const payload = {
      signal: 'ice',
      display_id: this.displayId,
      candidate,
    };
    if (dashboardTransport?.canUseDisplayWebRtcSignal?.()) {
      await dashboardTransport.displayWebRtcSignal(payload, { timeoutMs: 15000 });
      return;
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error('dashboard control display signaling is not available');
    }
    if (!this.sendLegacyDisplaySignal({
      t: 'display_ice',
      display_id: this.displayId,
      candidate,
    })) {
      throw new Error('display signaling is not available');
    }
  }

  async sendDisplayOffer(sdp) {
    if (dashboardTransport?.canUseDisplayWebRtcSignal?.()) {
      const answer = await dashboardTransport.displayWebRtcSignal({
        signal: 'offer',
        display_id: this.displayId,
        sdp,
      }, { timeoutMs: 30000 });
      const answerSdp = answer?.sdp || answer?.answer_sdp || '';
      if (!answerSdp) throw new Error('display signaling answer missing SDP');
      this.handleAnswer(answerSdp);
      return;
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error('dashboard control display signaling is not available');
    }
    if (!this.sendLegacyDisplaySignal({
      t: 'display_offer',
      display_id: this.displayId,
      sdp,
    })) {
      throw new Error('display signaling is not available');
    }
  }

  connect() {
    // Fresh negotiation: reset the answer/candidate bookkeeping (retries
    // used to inherit the previous session's `_answerApplied = true` and
    // feed early candidates into a pc with no remote description) and
    // bump the epoch so stale first-frame/watchdog callbacks bail.
    const epoch = ++this._connectEpoch;
    this._answerApplied = false;
    this._pendingCandidates = [];
    this._firstFrameSeen = false;
    this.statusEl.textContent = 'Connecting...';
    this.statusEl.className = 'display-status';
    this._setStageOverlay('progress', 'Negotiating…');
    // ICE config — STUN/TURN servers from [webrtc].ice_servers TOML config,
    // default empty for local LAN deployments. Goes through the shared
    // helper so the peer-display path (PeerDisplayConnection.connect) can't
    // drift in what it hands to the browser's ICE agent.
    const config = { iceServers: buildIceServersFromGatewayConfig(gatewayConfig) };
    this.pc = new RTCPeerConnection(config);

    // Add a recvonly video transceiver so the SDP offer includes a video
    // media section. Without this, the server can't attach its video track
    // because the answerer can't introduce new media lines.
    const videoTransceiver = this.pc.addTransceiver('video', { direction: 'recvonly' });

    // **#58**: NO `setCodecPreferences` reorder. WKWebView's default
    // codec order puts H.264 PTs (96/98/100) before VP8 (107) — let
    // it. On macOS the server then negotiates H.264, which spawns a
    // hardware-accelerated VideoToolbox encoder
    // ([`crate::display::encode::h264_macos`]) — single-encoding,
    // single thread, ~5-10 % CPU at full resolution.
    //
    // Pre-#58 this path force-reordered VP8 first because the local
    // DisplaySlot also injected `a=simulcast:recv f;h;q` for
    // multi-encoding receive, and rtc 0.9's SDP writer mishandles
    // multi-RID H.264 (single SSRC covering all RIDs → browser
    // chokes). #58 also drops to `a=simulcast:recv f` (single-RID
    // receive — see `DISPLAY_SIMULCAST_RIDS`), so the rtc 0.9
    // multi-RID-H.264 bug is no longer reachable: with single-RID
    // receive the answer is plain sendonly, identical for VP8 and
    // H.264. Restoring default codec order = restoring the
    // hardware-accelerated path the macOS UTM guest needs to stay
    // usable. Pre-#58 idle dashboard pegged the guest at 245 %+
    // CPU on three software VP8 encoders for one viewer.
    //
    // Chrome viewers on macOS still default VP8 first; they get
    // single-encoding VP8 (libvpx software, 1 encoder) at ~80 % CPU
    // — also a substantial drop from ~245 %, just less dramatic
    // than WKWebView's hardware H.264 path.

    // Create data channels BEFORE offer (browser is the offerer)
    this.controlChannel = this.pc.createDataChannel('control', { ordered: true });
    this.pointerChannel = this.pc.createDataChannel('pointer', {
      ordered: false,
      maxRetransmits: 0
    });
    this.clipboardChannel = this.pc.createDataChannel('clipboard', { ordered: true });

    // Handle incoming clipboard updates from the remote display.
    // Clipboard sync is a LOCAL-ONLY policy today (federated clipboard is
    // a follow-up); the applier itself is shared viewer-core code.
    this.clipboardChannel.onmessage = (e) => {
      try {
        const d = JSON.parse(e.data);
        if (d.t === 'clipboard_update' && this.interactive) {
          displayViewerApplyRemoteClipboardUpdate(d);
        }
      } catch {}
    };

    // Handle incoming video track
    this.pc.ontrack = (e) => {
      this.videoEl.srcObject = e.streams[0];
      // The slot's element can already live in the offscreen Station
      // endpoint container, where autoplay doesn't fire on attach; a
      // paused element renders as a frozen black Station pane.
      this.videoEl.play().catch(() => {});
      this.connected = true;
      const res = this.width > 0 ? ` ${this.width}x${this.height}` : '';
      this.statusEl.textContent = `Connected (view-only)${res}`;
      this.statusEl.className = 'display-status connected';
      // Track arrived, frames haven't: keep the stage honest until the
      // first frame actually renders, then drop the overlay and the
      // no-video watchdog together.
      if (!this._firstFrameSeen) {
        this._setStageOverlay('progress', 'Waiting for first frame…');
      }
      this._onFirstFrame(epoch, () => {
        this._firstFrameSeen = true;
        this._clearNoTrackWatchdog();
        this._setStageOverlay(null);
      });
    };

    // ICE candidates — prefer the verified dashboard-control tunnel, falling
    // back to the daemon WebSocket only on direct daemon-origin dashboards.
    this.pc.onicecandidate = (e) => {
      if (!e.candidate) return;
      const candidate = e.candidate.toJSON ? e.candidate.toJSON() : e.candidate;
      this.sendDisplayIceCandidate(candidate).catch(err => {
        console.warn(`[DisplaySlot ${this.displayId}] ICE signal failed`, err);
      });
    };

    // Connection state changes — auto-reconnect on failure
    this.pc.onconnectionstatechange = () => {
      const state = this.pc.connectionState;
      if (state === 'connected') {
        this.connected = true;
        this._reconnectAttempts = 0;
        const res = this.width > 0 ? ` ${this.width}x${this.height}` : '';
        this.statusEl.textContent = `Connected (view-only)${res}`;
        this.statusEl.className = 'display-status connected';
        this._startStatsSampler();
      } else if (state === 'failed') {
        // ICE negotiation failed — tear down and create a fresh
        // offer/answer exchange.  The server-side DisplaySession stays
        // alive; only the WebRTC peer is recreated.
        //
        // Exception: if the user has explicitly closed this slot (toggle
        // off, user_display_revoked), don't retry — the server has
        // torn the session down deliberately, our offers would find no
        // session to bind to, and the visible effect is "off doesn't
        // stay off" because every retry briefly flips the UI back to
        // reconnecting. `_closedByUser` sticks until the slot is
        // destroyed; re-granting the display creates a fresh slot
        // with the flag cleared.
        if (this._closedByUser) return;
        this.connected = false;
        const attempts = (this._reconnectAttempts || 0) + 1;
        this._reconnectAttempts = attempts;
        if (attempts <= 5) {
          const delay = Math.min(2000 * attempts, 10000);
          // disconnect() first: it clears watchdogs/samplers and hides
          // the overlay, so set the retry copy AFTER it runs.
          this.disconnect();
          this.statusEl.textContent = `Connection failed, reconnecting in ${delay/1000}s (attempt ${attempts})...`;
          this.statusEl.className = 'display-status error';
          this._setStageOverlay('progress', `Connection failed — reconnecting in ${delay/1000}s (attempt ${attempts} of 5)…`);
          setTimeout(() => {
            if (this._closedByUser) return;
            this.connect();
          }, delay);
        } else {
          // Dead end used to be terminal with no control; now it alarms
          // and offers a manual Reconnect that restarts the retry budget.
          this.statusEl.textContent = 'Connection failed after 5 attempts';
          this.statusEl.className = 'display-status error';
          this._stopStatsSampler();
          this._setStageOverlay('error', 'Connection failed after 5 attempts.', {
            retryLabel: 'Reconnect',
            onRetry: () => this.manualReconnect(),
          });
        }
      } else if (state === 'disconnected') {
        // 'disconnected' is transient more often than not (ICE keeps
        // probing and usually recovers) — don't alarm, don't tear down.
        // 'failed' is the alarming state with the Reconnect offer.
        this.connected = false;
        this._stopStatsSampler();
        this.statusEl.textContent = 'Connection interrupted — recovering…';
        this.statusEl.className = 'display-status warn';
      }
    };

    // Create offer and send to server.
    //
    // Inject `a=rid:<rid> recv` lines + `a=simulcast:recv <rids>` into
    // the m=video section before setLocalDescription. `<rids>` is
    // `DISPLAY_SIMULCAST_RIDS` (default `['f']` — single-RID receive
    // post-#58; opt-in `['f','h','q']` for the experimental
    // multi-encoding adaptive-bandwidth path). Munge BEFORE
    // setLocalDescription so the localDescription matches what's sent
    // on the wire — server-side SDP-validation tests parse the
    // received offer/local-description and assume the recv-RID list
    // matches the configured constant.
    this.pc.createOffer().then(offer => {
      const munged = {
        type: offer.type,
        sdp: injectRecvSimulcastIntoVideoOffer(offer.sdp, DISPLAY_SIMULCAST_RIDS),
      };
      // Diagnostic: log the first video codec in the emitted offer.
      // Codec order is intentionally left to the browser — WKWebView
      // typically negotiates H.264 (and the server then spawns a
      // hardware-accelerated VideoToolbox encoder), Chrome/Chromium
      // typically negotiates VP8. Any codec the browser put first is
      // valid; this log just makes the negotiated outcome visible.
      const firstCodec = (() => {
        const lines = munged.sdp.split(/\r?\n/);
        const mLine = lines.find(l => l.startsWith('m=video'));
        if (!mLine) return '(no m=video)';
        const pts = mLine.trim().split(/\s+/).slice(3);
        const firstPt = pts[0];
        const rtpmap = lines.find(l => l.startsWith(`a=rtpmap:${firstPt} `));
        return rtpmap ? rtpmap.replace(`a=rtpmap:${firstPt} `, '') : `pt=${firstPt}`;
      })();
      console.info(
        `[DisplaySlot ${this.displayId}] offer first codec: ${firstCodec}`
      );
      return this.pc.setLocalDescription(munged);
    }).then(async () => {
      await this.sendDisplayOffer(this.pc.localDescription.sdp);
    }).catch(err => {
      this.statusEl.textContent = 'Offer FAILED: ' + err.message;
      this.statusEl.className = 'display-status error';
      // Offer/signaling failure used to be a quiet toolbar note on a dark
      // stage; surface it where the user is looking, with a way out.
      this._setStageOverlay('error', 'Connection setup failed: ' + err.message, {
        retryLabel: 'Reconnect',
        onRetry: () => this.manualReconnect(),
      });
    });
  }

  // ── In-stage status overlay + reconnect helpers ─────────────────────

  // Render the stage overlay. `mode` is 'progress' (spinner + copy),
  // 'error' (alarming copy + optional retry button), or null to hide.
  // Shared DOM builder in 45-display-viewer-core; this slot renders into
  // its single fixed overlayEl (the peer path re-applies per container —
  // its pane DOM is rebuilt on daemons-list re-renders, ours is not).
  _setStageOverlay(mode, text, { retryLabel = null, onRetry = null } = {}) {
    const el = this.overlayEl;
    if (!el) return;
    displayViewerRenderStageOverlayInto(el, mode ? { mode, text, retryLabel, onRetry } : null);
  }

  // Run `cb` once the <video> renders its first frame for THIS
  // negotiation epoch (shared cascade in 45-display-viewer-core; the
  // epoch comparison is this class's staleness guard).
  _onFirstFrame(epoch, cb) {
    displayViewerOnFirstFrame(this.videoEl, () => epoch !== this._connectEpoch, cb);
  }

  // User-facing recovery entry point (overlay Reconnect button, revived
  // slots). Resets the bounded-retry budget and renegotiates from scratch.
  manualReconnect() {
    if (this._closedByUser) return;
    this._reconnectAttempts = 0;
    this.disconnect();
    this.connect();
  }

  // No-video watchdog, ported from the peer path (shared driver in
  // 45-display-viewer-core): armed when the answer applies, cleared by
  // the first rendered frame. Catches the "answer accepted, ICE/DTLS
  // fine, but no frames ever arrive" black-stage case.
  _armNoTrackWatchdog() {
    displayViewerArmNoTrackWatchdog(this, () => {
      if (this._firstFrameSeen || this._closedByUser) return;
      this.statusEl.textContent = 'No video received';
      this.statusEl.className = 'display-status error';
      this._setStageOverlay('error',
        'No video within 10s — the server accepted the connection but sent no frames. The capture may have stalled; reconnecting usually fixes it.', {
          retryLabel: 'Reconnect',
          onRetry: () => this.manualReconnect(),
        });
    }, DisplaySlot.NO_TRACK_TIMEOUT_MS);
  }

  _clearNoTrackWatchdog() {
    displayViewerClearNoTrackWatchdog(this);
  }

  // ── Live metrics chip (getStats sampler) ────────────────────────────
  // Shared cadence + summarizer (45-display-viewer-core); only where the
  // text lands is ours: the slot's single fixed metricsEl.

  _startStatsSampler() {
    displayViewerStartStatsSampler(this);
  }

  _stopStatsSampler() {
    displayViewerStopStatsSampler(this);
    if (this.metricsEl) {
      this.metricsEl.style.display = 'none';
      this.metricsEl.classList.remove('active');
      this.metricsEl.textContent = '';
    }
  }

  async _sampleStats() {
    await displayViewerSampleRtcStats(this, (text) => {
      if (!this.metricsEl) return;
      this.metricsEl.textContent = text;
      this.metricsEl.style.display = '';
      this.metricsEl.classList.add('active');
    });
  }

  handleAnswer(sdp) {
    if (!this.pc) return;
    this.statusEl.textContent = 'Answer received, applying...';
    // Diagnostic: log the negotiated codec + simulcast direction.
    // With single-RID receive (post-#58 default), the answer is
    // plain sendonly and `(no a=simulcast)` is expected. WKWebView
    // typically lands on H.264 (hardware VideoToolbox on macOS);
    // Chrome typically lands on VP8 (single software encoder). An
    // `a=simulcast:send f;h;q` line here is the signature of the
    // opt-in multi-RID path — only expected when
    // `DISPLAY_SIMULCAST_RIDS` has been switched to `['f','h','q']`.
    {
      const lines = sdp.split(/\r?\n/);
      const mLine = lines.find(l => l.startsWith('m=video'));
      const firstPt = mLine ? mLine.trim().split(/\s+/)[3] : null;
      const negotiated = firstPt
        ? (lines.find(l => l.startsWith(`a=rtpmap:${firstPt} `)) || '').replace(`a=rtpmap:${firstPt} `, '')
        : '(unknown)';
      const simulcast = (lines.find(l => l.startsWith('a=simulcast:')) || '(no a=simulcast)').trim();
      console.info(
        `[DisplaySlot ${this.displayId}] answer negotiated codec: ${negotiated}; ${simulcast}`
      );
    }
    displayViewerApplyRemoteAnswer(this, sdp, {
      beforeFlush: (count) => {
        this.statusEl.textContent = `Answer applied, ICE: ${this.pc.iceConnectionState}, flushing ${count} candidates`;
      },
      afterFlush: () => {
        // Answer accepted: the only thing left is media. Stage the copy
        // and arm the no-video watchdog (cleared by the first frame).
        if (!this._firstFrameSeen) {
          this._setStageOverlay('progress', 'Waiting for first frame…');
        }
        this._armNoTrackWatchdog();
      },
      onError: (err) => {
        this.statusEl.textContent = `Answer FAILED: ${err.message}`;
        this.statusEl.className = 'display-status error';
        this._setStageOverlay('error', 'Answer failed: ' + err.message, {
          retryLabel: 'Reconnect',
          onRetry: () => this.manualReconnect(),
        });
        console.error('Failed to set remote description:', err);
      },
    });
  }

  handleIceCandidate(candidate) {
    if (!this.pc) return;
    // Queue until setRemoteDescription(answer) completes (shared scaffold).
    displayViewerIngestRemoteIceCandidate(this, candidate, {
      onAddError: (err) => console.error('Failed to add ICE candidate:', err),
    });
  }

  // Phase 5c: split into authority-aware entry + UI-only interactive
  // mode.  `takeControl` is now the user-intent entry point; the actual
  // listener installation lives in `_enterInteractive`, which is called
  // either immediately (when this connection already holds authority)
  // or asynchronously after the server's `'you'` callback arrives.
  takeControl() {
    if (!this.pc) return;
    if (this.authorityState === 'you') {
      // Server says we're already the holder — enter interactive mode now.
      this._enterInteractive();
      return;
    }
    // Otherwise request authority and wait for the `'you'` notification
    // to arrive via `setAuthority`.  Marker so `setAuthority('you')` knows
    // to promote us into interactive mode rather than just rendering the
    // chip.  The button shows a pending state while the request is in
    // flight; a 5s no-answer timeout resets it with a toast instead of
    // leaving a silently armed flag.
    this._setTakeControlPending(true);
    requestDisplayInputAuthorityForSlot(this.displayId);
  }

  // Item 7b: single writer for the Take Control pending state — flag,
  // button spinner/disabled state, and the 5s no-answer timeout that
  // resets everything with a toast.
  _setTakeControlPending(pending) {
    this._takeControlPending = pending;
    if (this._takePendingTimer) {
      window.clearTimeout(this._takePendingTimer);
      this._takePendingTimer = null;
    }
    if (this.takeBtn) {
      this.takeBtn.disabled = pending;
      this.takeBtn.classList.toggle('is-pending', pending);
      this.takeBtn.textContent = pending ? 'Requesting…' : 'Take Control';
    }
    if (pending) {
      this._takePendingTimer = window.setTimeout(() => {
        this._takePendingTimer = null;
        if (!this._takeControlPending) return;
        this._setTakeControlPending(false);
        if (typeof showControlToast === 'function') {
          showControlToast('error', 'No response to the input-control request — try again');
        }
      }, DISPLAY_VIEWER_TAKE_PENDING_TIMEOUT_MS);
    }
  }

  // Phase 5c: enter interactive mode (UI + listeners).  Called from
  // `takeControl` synchronously when state === 'you', or from
  // `setAuthority` asynchronously after the server promotes us.  Idempotent
  // so a double-fire (rare race between user click and server bootstrap)
  // doesn't double-install listeners.
  //
  // Lifecycle: still emits the legacy `take_display` ControlMsg so the
  // worker agent yields the display to the human user — that's a separate
  // signal from input authority and must keep firing on user-intent
  // entry into interactive mode.
  _enterInteractive() {
    if (this.interactive) return;
    this.interactive = true;
    this.noteInput.style.display = '';
    const res = this.width > 0 ? ` ${this.width}x${this.height}` : '';
    this.statusEl.textContent = `Interactive${res}`;
    this.statusEl.className = 'display-status connected';
    dispatchDashboardActionMsg({ action: 'take_display', display_id: Number(this.displayId) || 0 });
    // _renderAuthority handles take/release button visibility from the
    // authority state (so we don't duplicate the toggle here).
    this._renderAuthority();

    const vid = this.videoEl;
    vid.tabIndex = 0;
    vid.focus();
    // Item 3: track EVERY held key code (not just the 8 modifiers) so
    // blur and demotion can release everything the remote side thinks
    // is down. A latched non-modifier (e.g. a held arrow key when the
    // server demotes us) otherwise auto-repeats remotely forever.
    this._heldKeys = new Set();

    // Input transport — the LOCAL policy: prefer the verified
    // dashboard-control input lane (sendDisplayInputForSlot), fall back
    // to this pc's data channels. The federated path sends over its
    // data channels only.
    const sendControl = (msg) => {
      if (sendDisplayInputForSlot(this.displayId, msg)) return;
      if (this.controlChannel && this.controlChannel.readyState === 'open') {
        this.controlChannel.send(JSON.stringify(msg));
      }
    };
    const sendPointer = (msg) => {
      if (sendDisplayInputForSlot(this.displayId, msg)) return;
      if (this.pointerChannel && this.pointerChannel.readyState === 'open') {
        this.pointerChannel.send(JSON.stringify(msg));
      }
    };

    // Held-key flusher, stored on the instance so `_exitInteractive`
    // (which runs outside this closure) can release held keys BEFORE the
    // listeners are removed — a server-side authority demotion otherwise
    // latches keys down remotely. Cleared by `_exitInteractive`.
    this._flushHeldKeys = displayViewerMakeHeldKeyFlusher(this, sendControl);
    // The shared capture stack (letterbox normalize, kd/ku/md/mu/mm/sc,
    // blur flush, pointerenter refocus) — 45-display-viewer-core.
    this._boundHandlers = displayViewerBuildInputHandlers({
      owner: this,
      target: vid,
      sendControl,
      sendPointer,
    });

    // Clipboard: intercept paste events and send to remote display.
    // LOCAL-ONLY policy (federated clipboard is a follow-up).
    this._boundHandlers.paste = displayViewerBuildPasteHandler(() => this.clipboardChannel);
    document.addEventListener('paste', this._boundHandlers.paste);

    for (const [evt, handler] of Object.entries(this._boundHandlers)) {
      if (evt === 'paste') continue; // already added to document
      vid.addEventListener(evt, handler, { passive: false });
    }
  }

  // Phase 5c: send the authority-release control message to the server.
  // Pure send — no UI changes, no `_exitInteractive` call, no
  // `app.release_display` lifecycle event.  Composed by `releaseControl`
  // (user click) and `disconnect({ userInitiated: true })` (user-close
  // paths like `removeDisplaySlot` after `_closedByUser = true`).  The
  // server is idempotent: a release from a non-holder is a silent no-op,
  // so calling this when authority isn't held is harmless.
  _releaseAuthority() {
    releaseDisplayInputAuthorityForSlot(this.displayId);
    // Cancel any pending take so a server `'you'` answer arriving after
    // the release doesn't re-promote into interactive.
    this._setTakeControlPending(false);
  }

  // Phase 5c: user-intent release of interactive control via the Release
  // button.  Sends authority release to the server AND exits interactive
  // mode locally so the UI is consistent immediately rather than waiting
  // on the server's round-trip; the legacy `release_display` lifecycle
  // ControlMsg fires alongside (via `_exitInteractive(true)`).  This is
  // ONLY for the explicit Release-button path — non-user cleanup
  // (transport reconnect, capture lost) goes through `disconnect()` with
  // `userInitiated: false`, which calls neither this method nor
  // `_releaseAuthority`.  Server demotion (someone else takes over) goes
  // through `_exitInteractive(false)` from `setAuthority` instead.
  releaseControl() {
    // Exit interactive FIRST: `_exitInteractive` flushes synthetic keyups
    // for every held key, and those must reach the server while this
    // connection still holds input authority (the release below removes
    // our slot at the server gate, after which held-key keyups would be
    // dropped and the keys would stay latched down remotely).
    this._exitInteractive(true);
    this._releaseAuthority();
  }

  // Phase 5c: exit interactive mode (UI + listeners).  `userInitiated`
  // gates the legacy `release_display` lifecycle ControlMsg:
  // `true` for explicit user release / disconnect / move-to-thumb (the
  // user navigated away from interactive mode), `false` for server-driven
  // demotion when another browser takes authority (the user didn't ask
  // for this — display is still visible, they just lost input control).
  // Idempotent so a server `'unclaimed'` arriving right after a user
  // release-click doesn't re-fire the lifecycle release.
  _exitInteractive(userInitiated) {
    if (!this.interactive) return;
    this.interactive = false;
    // Item 3: flush synthetic keyups for every held key BEFORE the
    // listeners are removed. Server-driven demotion never fires blur, so
    // without this any held key (modifier or not) stays latched down on
    // the remote display. Best-effort: on a demotion the gate may already
    // have dropped us, but the flush is the only recovery available.
    if (this._flushHeldKeys) {
      try { this._flushHeldKeys(); } catch (_) {}
      this._flushHeldKeys = null;
    }
    if (this._heldKeys) this._heldKeys.clear();
    const vid = this.videoEl;
    for (const [evt, handler] of Object.entries(this._boundHandlers)) {
      if (evt === 'paste') {
        document.removeEventListener('paste', handler);
      } else {
        vid.removeEventListener(evt, handler);
      }
    }
    this._boundHandlers = {};
    const note = this.noteInput.value.trim() || undefined;
    this.noteInput.value = ''; this.noteInput.style.display = 'none';
    this.statusEl.textContent = this.connected ? 'Connected (view-only)' : 'Disconnected';
    // Take/release button visibility is driven by `_renderAuthority` from
    // `authorityState` — but since exiting interactive doesn't itself flip
    // authority, re-render to clear any toolbar state that the interactive
    // mode set (note input shown, etc.).
    this._renderAuthority();
    if (userInitiated) {
      const msg = { action: 'release_display', display_id: Number(this.displayId) || 0 };
      if (note) msg.note = note;
      dispatchDashboardActionMsg(msg);
    }
  }

  // Phase 5c: server-driven authority state callback.  Called from the
  // WASM `set_on_display_input_authority_change` dispatcher (wired in the
  // app init in connect_voice / set_on_display_input_authority_change
  // callsite) with the resolved `'you' | 'other' | 'unclaimed'` for THIS
  // browser's connection — never any holder ID, never personalization on
  // this side.  The server gate at `web_gateway::gated_input_handler` is
  // the source of truth for input enforcement; this UI logic exists
  // strictly to keep the chip + buttons + interactive mode consistent
  // with what the gate is doing.
  // Render the agent-visibility chip: "Private view" (the agent cannot
  // see this display) vs "Agent can see this" (a user display shared for
  // computer use). Agent-owned virtual displays get no chip -- they are
  // the agent's own workspaces, so annotating them is noise.
  setAgentVisibility(visible) {
    if (!this.visibilityEl) return;
    const id = Number(this.displayId);
    if (visible === false) {
      this.visibilityEl.textContent = 'Private view';
      this.visibilityEl.title =
        'Only your dashboard can see this display. It is hidden from the ' +
        "agent's screenshot, computer-use, and display-listing paths.";
      this.visibilityEl.classList.add('private');
      this.visibilityEl.classList.remove('agent');
      this.visibilityEl.style.display = '';
    } else if (visible === true && userDisplayIds.has(id)) {
      this.visibilityEl.textContent = 'Agent can see this';
      this.visibilityEl.title =
        'This screen is shared with the agent for computer-use tasks ' +
        'until you revoke access.';
      this.visibilityEl.classList.add('agent');
      this.visibilityEl.classList.remove('private');
      this.visibilityEl.style.display = '';
    } else {
      this.visibilityEl.style.display = 'none';
    }
    // The ui2 live rail re-renders via its MutationObserver on
    // #displays-container (class/style/text changes on this chip).
  }

  setAuthority(state) {
    if (!isDisplayInputAuthorityState(state)) {
      // Forward-compat: future state strings (we don't expect any) leave
      // the chip on its previous value rather than blanking it.
      return;
    }
    this.authorityState = state;
    this._renderAuthority();

    // Callout arming requires held input authority; losing it disarms.
    if (state !== 'you' && liveCalloutArmedFor(this)) {
      disarmLiveCallout();
    }

    // Promote pending take into interactive mode the moment the server
    // confirms we hold it.  Without this, the user would click Take
    // Control and see the chip flip but the listeners would not install.
    if (state === 'you' && this._takeControlPending) {
      this._setTakeControlPending(false);
      this._enterInteractive();
    }

    // Demote silently on loss of authority.  The user did not initiate
    // this exit — server picked up that another browser took control —
    // so no `release_display` lifecycle event fires (per phase 5c spec:
    // "if state changes from you to other while interactive, exit
    // interactive silently").
    if (this.interactive && state !== 'you') {
      this._exitInteractive(false);
    }
  }

  // Phase 5c: render the chip + take/release button visibility from the
  // current `authorityState`.  Single-source UI projection so any code
  // that mutates state goes through `setAuthority` and never has to
  // remember to update DOM directly.  Chip text/classes and the button
  // toggle are the shared renderers in 45-display-viewer-core — the same
  // vocabulary the federated chip renders.  Button visibility tracks
  // state, not the `interactive` flag, so the user can click Take
  // Control even before listeners install (the request flow handles
  // waiting for the `'you'` callback).
  _renderAuthority() {
    displayViewerRenderAuthorityChip(
      this.authorityEl, this.authorityState, 'display-input-authority');
    displayViewerApplyAuthorityButtons(
      this.takeBtn, this.releaseBtn, this.calloutBtn, this.authorityState);
  }

  toggleStreaming() {
    if (this.streaming) { this.stopStreaming(); } else { this.startStreaming(); }
  }
  startStreaming() {
    if (this.streaming || !this.connected) return;
    this.streaming = true;
    this.streamBtn.classList.add('active');
    this.streamBtn.innerHTML = '&#x1F441; Streaming';
    this.frameIdEl.style.display = '';
    const streamName = 'display_' + this.displayId;
    const ctx = this._streamCanvas.getContext('2d');
    this._streamIntervalId = setInterval(() => {
      if (!this.streaming || !this.connected || !app) return;
      const vid = this.videoEl;
      if (!vid.videoWidth || !vid.videoHeight) return;
      const sw = vid.videoWidth;
      const sh = vid.videoHeight;
      this._streamFrameCounter++;
      const frameId = streamName + '-f' + String(this._streamFrameCounter).padStart(5, '0');
      // Live-res: scale to LIVE_RES maintaining aspect ratio within a square
      const scale = Math.min(LIVE_RES / sw, LIVE_RES / sh);
      const lw = Math.round(sw * scale);
      const lh = Math.round(sh * scale);
      this._streamCanvas.width = lw;
      this._streamCanvas.height = lh;
      ctx.drawImage(vid, 0, 0, lw, lh);
      const liveJpeg = this._streamCanvas.toDataURL('image/jpeg', 0.8);
      const liveB64 = liveJpeg.split(',')[1];
      // Skip duplicate frames for voice model — still send HQ to server for archival
      const sizeDelta = Math.abs(liveB64.length - (this._lastFrameLen || 0)) / (this._lastFrameLen || 1);
      const frameDup = sizeDelta < 0.02 && this._lastFrameLen > 0;
      if (!frameDup) {
        this._lastFrameLen = liveB64.length;
        app.send_frame(liveB64, frameId);
        this.frameIdEl.textContent = frameId.split('-').pop();
        this.frameIdEl.style.color = 'var(--overlay0)';
        tickerFramesSent++;
      } else {
        this.frameIdEl.textContent = frameId.split('-').pop() + ' dropped';
        this.frameIdEl.style.color = 'var(--yellow)';
        tickerFramesDropped++;
        sendDashboardVoiceDiagnostic('frame_skip', 'duplicate frame skipped (delta=' + (sizeDelta * 100).toFixed(1) + '%)');
      }
      updateTickerFrames();
      // HQ: logical resolution — always sent for archival
      const dpr = window.devicePixelRatio || 1;
      this._streamCanvas.width = Math.round(sw / dpr);
      this._streamCanvas.height = Math.round(sh / dpr);
      ctx.drawImage(vid, 0, 0, this._streamCanvas.width, this._streamCanvas.height);
      const hqJpeg = this._streamCanvas.toDataURL('image/jpeg', 0.80);
      const hqB64 = hqJpeg.split(',')[1];
      sendDashboardVideoFrameToServer(hqB64, frameId, streamName);
    }, 1000);
  }
  stopStreaming() {
    this.streaming = false;
    if (this._streamIntervalId) { clearInterval(this._streamIntervalId); this._streamIntervalId = null; }
    this.streamBtn.classList.remove('active');
    this.streamBtn.innerHTML = '&#x1F441; Stream';
    this.frameIdEl.style.display = 'none';
    this.frameIdEl.textContent = '';
  }
  toggleRecording() {
    if (!app) return;
    const baseStream = 'display_' + this.displayId;
    const stream = this.recordingStreamName || baseStream;
    if (this.recording) {
      dispatchDashboardActionMsg({ action: 'stop_recording', stream_name: stream });
      this.recording = false;
      this.recordBtn.innerHTML = '&#x23FA; Record';
      this.recordBtn.classList.remove('active');
      this.deleteRecBtn.style.display = '';
    } else {
      dispatchDashboardActionMsg({ action: 'start_recording', stream_name: baseStream });
      this.recordingStreamName = baseStream;
      this.recording = true;
      this.recordBtn.innerHTML = '&#x23F9; Stop';
      this.recordBtn.classList.add('active');
      this.deleteRecBtn.style.display = 'none';
    }
  }
  async deleteRecording() {
    if (!app) return;
    const stream = this.recordingStreamName || ('display_' + this.displayId);
    const ok = await showDashboardConfirm({
      title: 'Delete recording',
      message: `Delete recording for ${stream}?`,
      warning: 'Recording files will be removed from this session.',
      confirmLabel: 'Delete',
    });
    if (!ok) return;
    dispatchDashboardActionMsg({ action: 'delete_recording', stream_name: stream });
    this.deleteRecBtn.style.display = 'none';
    this.recordingStreamName = null;
  }
  // Phase 5c: teardown.  `userInitiated` separates the user-close path
  // (display toggled off, `removeDisplaySlot`, etc.) from transient
  // cleanup (ICE failure reconnect, display_capture_lost — both keep
  // the slot in case the session comes back).  Without this split,
  // every transient reconnect would silently release input authority
  // and fire the legacy `release_display` lifecycle event as if the
  // user clicked Release.
  //
  // - `userInitiated: true`  → release authority + exit interactive
  //   (with `release_display` lifecycle event firing if interactive).
  //   Used by `removeDisplaySlot` and any path where the user has
  //   actually closed the display (not just lost the underlying
  //   transport).
  // - `userInitiated: false` → just exit interactive locally; do NOT
  //   release authority and do NOT fire `release_display`.  The server
  //   gate stays as-is so a re-grant or transport reconnect resumes
  //   without the UX surprise of "I had control before, now I don't."
  //   The WS-close cleanup at the gateway is the safety net for
  //   genuinely-dropped connections.
  disconnect({ userInitiated = false } = {}) {
    // Invalidate in-flight async callbacks (first-frame hooks) and stop
    // every per-connection timer — no leaked intervals across teardowns.
    this._connectEpoch++;
    this._clearNoTrackWatchdog();
    this._stopStatsSampler();
    this._setStageOverlay(null);
    this.stopStreaming();
    if (userInitiated) {
      this._releaseAuthority();
    }
    // Flip `connected` BEFORE `_exitInteractive` so its status-text
    // ternary writes 'Disconnected' (not the stale 'Connected (view-
    // only)') on the way out — otherwise the chip flickers through
    // the connected text for a microtask before the post-cleanup
    // assignment overwrote it.
    this.connected = false;
    this._exitInteractive(userInitiated);
    if (this.controlChannel) { this.controlChannel.close(); this.controlChannel = null; }
    if (this.pointerChannel) { this.pointerChannel.close(); this.pointerChannel = null; }
    if (this.clipboardChannel) { this.clipboardChannel.close(); this.clipboardChannel = null; }
    if (this.pc) { this.pc.close(); this.pc = null; }
    if (this._focusResizeObserver) { this._focusResizeObserver.disconnect(); this._focusResizeObserver = null; }
    this.statusEl.className = 'display-status error';
  }
}

function removeDisplaySlot(displayId) {
  displayId = Number(displayId);
  const slot = displaySlots.get(displayId);
  if (!slot) return;
  // Provider-level teardown (47-annotation-clips): ends a live-annotation
  // edit or armed callout owned by this slot before its DOM goes away.
  teardownLiveSurfaceForOwner(slot);
  // Mark as user-intent-closed BEFORE disconnect so any in-flight
  // reconnect timer short-circuits and any subsequent `failed` event
  // skips the retry path. See DisplaySlot._closedByUser.
  slot._closedByUser = true;
  // Phase 5c: user-close path — release input authority + fire the
  // legacy `release_display` lifecycle event.  Distinct from the
  // transient `disconnect()` paths (ICE retry at the
  // `oniceconnectionstatechange` handler; display_capture_lost) which
  // call `disconnect()` with the `userInitiated: false` default and
  // therefore preserve authority for a possible re-grant.
  slot.toggleFullscreen(false);
  slot.disconnect({ userInitiated: true });
  if (slot.el && slot.el.parentNode) slot.el.parentNode.removeChild(slot.el);
  displaySlots.delete(displayId);
  if (sharedViewState.displayId === displayId) {
    clearSharedViewDecorations();
  }
  removeDisplayThumb(displayId);
  // Retire the Stats "Display Transport" card for this display; the set
  // and sections used to leak across display close/reopen cycles.
  displayMetricsIds.delete(displayId);
  document.getElementById('display-metrics-' + displayId)?.remove();
  stationUnregisterVideoSource(`local:${displayId}`);
  stationScheduleUpdate();
  if (displaySlots.size === 0) {
    const placeholder = document.getElementById('displays-placeholder');
    const container = document.getElementById('displays-container');
    if (placeholder) placeholder.style.display = '';
    if (container) container.classList.remove('has-displays');
  }
}

function normalizeSharedViewDisplayId(evt) {
  if (evt.display_id !== undefined && evt.display_id !== null) {
    const n = Number(evt.display_id);
    if (Number.isFinite(n)) return n;
  }
  const target = String(evt.display_target || '').trim();
  if (!target) return null;
  if (target === 'user_session' || target === 'primary') return 0;
  const stripped = target.startsWith(':') ? target.slice(1)
    : target.startsWith('display_') ? target.slice('display_'.length)
    : target;
  const n = Number(stripped);
  return Number.isFinite(n) ? n : null;
}

function sharedViewDisplayLabel(displayId, displayTarget) {
  if (displayId !== null && displayId !== undefined) {
    const n = Number(displayId);
    if (Number.isFinite(n)) return displayLabel(n, false);
  }
  const target = String(displayTarget || '').trim();
  if (!target) return 'display';
  if (target === 'user_session' || target === 'user' || target === 'primary') {
    return 'primary display';
  }
  const stripped = target.startsWith(':') ? target.slice(1)
    : target.startsWith('display_') ? target.slice('display_'.length)
    : target;
  const n = Number(stripped);
  if (Number.isFinite(n)) return displayLabel(n, false);
  return target;
}

function displayInfoForId(displayId) {
  const n = Number(displayId);
  if (!Number.isFinite(n)) return null;
  const lists = [cachedDisplays, stationLocalDisplays];
  for (const list of lists) {
    if (!Array.isArray(list)) continue;
    const match = list.find(d => Number(d?.id) === n);
    if (match) return match;
  }
  return null;
}

function displayLabel(displayId, compact = false) {
  const n = Number(displayId);
  const info = displayInfoForId(n);
  if (info?.kind === 'window') {
    if (compact) return info.application_name || info.window_title || 'Window';
    return info.name || info.window_title || info.application_name || 'Window';
  }
  if (Number.isFinite(n)) {
    if (n === 0) return compact ? 'Primary' : 'Primary display';
    return compact ? `Display ${n}` : `Display ${n}`;
  }
  return compact ? 'Display' : 'Display';
}

function clearSharedViewDecorations() {
  for (const slot of displaySlots.values()) {
    slot.el.classList.remove('shared-view-active');
    const focus = slot.canvasEl && slot.canvasEl.querySelector('.shared-view-focus-box');
    if (focus) focus.remove();
  }
}

function renderSharedViewFocus(slot, region, note) {
  if (!slot || !slot.canvasEl) return;
  let focus = slot.canvasEl.querySelector('.shared-view-focus-box');
  if (!region) {
    if (focus) focus.remove();
    return;
  }
  if (!focus) {
    focus = document.createElement('div');
    focus.className = 'shared-view-focus-box';
    slot.canvasEl.appendChild(focus);
  }
  const x = Math.max(0, Math.min(1, Number(region.x) || 0));
  const y = Math.max(0, Math.min(1, Number(region.y) || 0));
  const w = Math.max(0.01, Math.min(1 - x, Number(region.width) || 0.01));
  const h = Math.max(0.01, Math.min(1 - y, Number(region.height) || 0.01));
  const video = slot.videoEl;
  const canvasRect = slot.canvasEl.getBoundingClientRect();
  const videoRect = video ? video.getBoundingClientRect() : null;
  const videoWidth = video && Number(video.videoWidth);
  const videoHeight = video && Number(video.videoHeight);
  if (videoRect && videoRect.width > 0 && videoRect.height > 0 && canvasRect.width > 0 && canvasRect.height > 0 && videoWidth > 0 && videoHeight > 0) {
    const scale = Math.min(videoRect.width / videoWidth, videoRect.height / videoHeight);
    const frameW = videoWidth * scale;
    const frameH = videoHeight * scale;
    const frameX = videoRect.left - canvasRect.left + ((videoRect.width - frameW) / 2);
    const frameY = videoRect.top - canvasRect.top + ((videoRect.height - frameH) / 2);
    focus.style.left = (frameX + x * frameW).toFixed(1) + 'px';
    focus.style.top = (frameY + y * frameH).toFixed(1) + 'px';
    focus.style.width = (w * frameW).toFixed(1) + 'px';
    focus.style.height = (h * frameH).toFixed(1) + 'px';
  } else {
    focus.style.left = (x * 100).toFixed(3) + '%';
    focus.style.top = (y * 100).toFixed(3) + '%';
    focus.style.width = (w * 100).toFixed(3) + '%';
    focus.style.height = (h * 100).toFixed(3) + '%';
  }
  focus.dataset.note = note || '';
}

function updateSharedViewBanner() {
  const banners = Array.from(document.querySelectorAll('[data-shared-view-banner]'));
  if (!banners.length) return;
  for (const banner of banners) {
    banner.classList.toggle('hidden', !sharedViewState.visible);
  }
  if (!sharedViewState.visible) {
    document.querySelectorAll('[data-shared-view-take-input]').forEach(btn => {
      btn.style.display = 'none';
    });
    return;
  }
  const target = sharedViewDisplayLabel(sharedViewState.displayId, sharedViewState.displayTarget);
  const detail = sharedViewState.reason || sharedViewState.note || '';
  const action = sharedViewState.action === 'input_request'
    ? 'Input requested'
    : sharedViewState.action === 'focus'
      ? 'Focus'
      : sharedViewState.action === 'capture'
        ? 'Captured'
        : 'Viewing';
  const text = detail ? `${action} ${target}: ${detail}` : `${action} ${target}`;
  document.querySelectorAll('[data-shared-view-message]').forEach(message => {
    message.textContent = text;
  });
  document.querySelectorAll('[data-shared-view-take-input]').forEach(takeInput => {
    const canTake = sharedViewState.action === 'input_request'
      && sharedViewState.displayId !== null
      && displaySlots.has(sharedViewState.displayId);
    takeInput.style.display = canTake ? '' : 'none';
  });
}

function applySharedViewToSlot(slot) {
  if (!sharedViewState.visible || !slot) return;
  if (sharedViewState.displayId !== null && Number(slot.displayId) !== sharedViewState.displayId) {
    return;
  }
  updateSharedViewBanner();
  slot.el.classList.add('shared-view-active');
  renderSharedViewFocus(slot, sharedViewState.region, sharedViewState.note);
  if (activeTab === 'displays') {
    requestAnimationFrame(() => {
      try { slot.el.scrollIntoView({ block: 'nearest', inline: 'nearest' }); } catch (_) {}
    });
  } else if (activeTab === 'activity' && activeActivitySubtab === 'log') {
    setDisplayStripExpanded(true);
    requestAnimationFrame(() => {
      const strip = document.getElementById('activity-display-strip');
      try { if (strip) strip.scrollIntoView({ block: 'nearest', inline: 'nearest' }); } catch (_) {}
    });
  }
}

function hideSharedView() {
  sharedViewState.visible = false;
  sharedViewState.action = 'hide';
  sharedViewState.region = null;
  updateSharedViewBanner();
  clearSharedViewDecorations();
  if (displayThumbs.size === 0) {
    const strip = document.getElementById('activity-display-strip');
    if (strip) strip.classList.add('hidden');
  }
}

function takeSharedViewInput() {
  if (sharedViewState.displayId === null) return;
  const slot = displaySlots.get(sharedViewState.displayId);
  if (slot) slot.takeControl();
}

function handleSharedViewEvent(evt) {
  const action = String(evt.action || 'show');
  if (action === 'hide') {
    hideSharedView();
    return;
  }
  sharedViewState.visible = true;
  sharedViewState.action = action;
  sharedViewState.displayId = normalizeSharedViewDisplayId(evt);
  sharedViewState.displayTarget = String(evt.display_target || '');
  sharedViewState.reason = String(evt.reason || '');
  sharedViewState.note = String(evt.note || '');
  sharedViewState.region = evt.region || null;

  clearSharedViewDecorations();
  updateSharedViewBanner();
  if (activeTab !== 'displays' && (activeTab !== 'activity' || activeActivitySubtab !== 'log')) {
    routeTo('activity', 'log');
  }
  const activityStrip = document.getElementById('activity-display-strip');
  if (activeTab === 'activity' && activeActivitySubtab === 'log' && activityStrip) {
    activityStrip.classList.remove('hidden');
  }
  const slot = sharedViewState.displayId !== null
    ? displaySlots.get(sharedViewState.displayId)
    : displaySlots.values().next().value;
  if (slot) applySharedViewToSlot(slot);
}

function addDisplaySlot(displayId, width, height) {
  // WASM serializes u64 as BigInt; normalize to Number for Map keys and JSON.stringify.
  displayId = Number(displayId);
  width = Number(width);
  height = Number(height);
  // **#59**: idempotent re-entry. The server emits `display_ready`
  // both on the bootstrap snapshot for currently-active displays
  // (`web_gateway.rs` bootstrap path) and via `log_replay` of the
  // historical `display_ready` from session.jsonl. Both arrive on the
  // same WS connection, so this function gets called twice for one
  // live grant. The slot's lifecycle is owned exclusively by:
  //   - `user_display_revoked` (line 5408) → `removeDisplaySlot`
  //   - explicit user close → `removeDisplaySlot`
  //   - `display_capture_lost` (line 5417) → `slot.disconnect()`
  // If a slot exists when a duplicate `display_ready` arrives, the
  // live grant is still valid — return early. Destroying + recreating
  // would spawn a second RTCPeerConnection, which the server treats
  // as a second viewer (peers=2 transiently, second encoder spawn —
  // observed as 2× `Using H264 (VideoToolbox)` per local viewer
  // pre-fix).
  //
  // Resolution change: dimensions on `display_ready` come from the
  // active grant; an X11/Wayland root resize during a live grant
  // emits `display_metrics` (and a fresh capture pipeline), not
  // `display_ready`. The "different dims for the same display_id"
  // case therefore only fires on a real grant cycle, which the
  // explicit revoke / capture-lost handlers above already serialize.
  //
  // Item 1b exception: a duplicate `display_ready` is only benign when
  // the existing slot is actually alive. After `display_capture_lost`
  // (slot kept, pc torn down → pc === null) or after the bounded ICE
  // retry gave up (pc stuck at 'failed'), this `display_ready` IS the
  // re-grant — early-returning here made revival impossible: the slot
  // sat disconnected forever and the only recovery was closing it by
  // hand. Revive the slot in place instead of spawning a second one
  // (which the server would treat as a second viewer).
  if (displaySlots.has(displayId)) {
    const existing = displaySlots.get(displayId);
    const state = existing.pc ? existing.pc.connectionState : null;
    if (!existing.pc || state === 'failed' || state === 'closed') {
      existing._closedByUser = false;
      existing._reconnectAttempts = 0;
      if (width > 0) {
        existing.width = width;
        existing.height = height;
      }
      existing.disconnect();
      existing.connect();
    }
    return;
  }
  const slot = new DisplaySlot(displayId, width, height);
  displaySlots.set(displayId, slot);
  // Apply the recorded agent-visibility mode (from display_ready /
  // user_display_granted events, which may precede slot creation).
  if (displayAgentVisibility.has(displayId)) {
    slot.setAgentVisibility(displayAgentVisibility.get(displayId));
  }
  // Phase 5c: drain any authority state that arrived before this slot
  // existed.  See pendingAuthorityStates docs for the race rationale.
  const pendingState = pendingAuthorityStates.get(displayId);
  if (pendingState !== undefined) {
    pendingAuthorityStates.delete(displayId);
    slot.setAuthority(pendingState);
  }
  const container = document.getElementById('displays-container');
  const placeholder = document.getElementById('displays-placeholder');
  if (placeholder) placeholder.style.display = 'none';
  container.classList.add('has-displays');
  container.appendChild(slot.el);
  slot.connect();
  addDisplayThumb(displayId);
  applySharedViewToSlot(slot);
  stationRegisterVideoSource(
    `local:${displayId}`,
    selfPeerId,
    String(displayId),
    `${selfHostLabel || 'local'} :${displayId}`,
    'local',
    slot.videoEl,
  );
  stationScheduleUpdate();
}

// ── Activity Display Strip ──
function addDisplayThumb(displayId) {
  if (displayThumbs.has(displayId)) return;
  const strip = document.getElementById('activity-display-strip');
  const row = document.getElementById('activity-display-row');
  strip.classList.remove('hidden');
  document.getElementById('strip-count').textContent = displayThumbs.size + 1;

  const thumb = document.createElement('div');
  thumb.className = 'activity-display-thumb';
  thumb.innerHTML = `<span class="thumb-label">${displayLabel(displayId, true)}</span>`;
  thumb.addEventListener('click', (e) => { e.stopPropagation(); toggleDisplayStrip(); }, true);

  displayThumbs.set(displayId, thumb);
  row.appendChild(thumb);

  // If on the activity tab, move the canvas here immediately
  if (activeTab === 'activity') {
    const slot = displaySlots.get(displayId);
    if (slot) moveCanvasToThumb(slot);
  }
}

function removeDisplayThumb(displayId) {
  displayId = Number(displayId);
  const thumb = displayThumbs.get(displayId);
  if (!thumb) return;
  if (thumb.parentNode) thumb.parentNode.removeChild(thumb);
  displayThumbs.delete(displayId);
  document.getElementById('strip-count').textContent = displayThumbs.size;
  if (displayThumbs.size === 0) {
    stripExpanded = false;
    stripMinimized = false;
    applyDisplayStripState();
    document.getElementById('activity-display-strip').classList.add('hidden');
  }
}

function moveCanvasToThumb(slot) {
  const thumb = displayThumbs.get(slot.displayId);
  if (!thumb || !slot.canvasEl) return;
  // Auto-release control when moving to view-only strip
  if (slot.interactive) {
    slot.releaseControl();
  }
  thumb.appendChild(slot.canvasEl);
}

function moveCanvasToSlot(slot) {
  if (!slot.canvasEl) return;
  slot.el.appendChild(slot.canvasEl);
}

function relocateDisplays(tabId) {
  if (
    tabId !== 'displays' &&
    annotationMode &&
    annotationContext &&
    annotationContext.kind === 'live'
  ) {
    exitAnnotationMode();
  }
  for (const slot of displaySlots.values()) {
    if (tabId === 'activity') moveCanvasToThumb(slot);
    else if (tabId === 'displays') moveCanvasToSlot(slot);
  }
}

function toggleDisplayStrip() {
  if (stripMinimized) {
    setDisplayStripMinimized(false);
    return;
  }
  setDisplayStripExpanded(!stripExpanded);
}

function setDisplayStripExpanded(wantExpanded) {
  stripExpanded = !!wantExpanded;
  stripMinimized = false;
  applyDisplayStripState();
}

function setDisplayStripMinimized(wantMinimized) {
  stripMinimized = !!wantMinimized;
  applyDisplayStripState();
}

function applyDisplayStripState() {
  const strip = document.getElementById('activity-display-strip');
  const handle = document.getElementById('activity-split-handle');
  const toggle = document.getElementById('strip-toggle');
  const minimize = document.getElementById('strip-minimize');
  if (!strip || !handle || !toggle) return;

  strip.classList.toggle('minimized', stripMinimized);
  strip.classList.toggle('expanded', stripExpanded && !stripMinimized);

  if (stripMinimized) {
    strip.style.height = '';
    handle.classList.add('hidden');
  } else if (stripExpanded) {
    strip.style.height = stripHeight + 'px';
    handle.classList.remove('hidden');
  } else {
    strip.style.height = '';
    handle.classList.add('hidden');
  }

  if (stripExpanded) {
    toggle.innerHTML = '&#x25B4;';
    toggle.title = 'Collapse displays';
  } else {
    toggle.innerHTML = '&#x25BE;';
    toggle.title = 'Expand displays';
  }

  if (minimize) {
    minimize.innerHTML = stripMinimized ? '+' : '&minus;';
    minimize.title = stripMinimized ? 'Restore displays' : 'Minimize displays';
    minimize.setAttribute('aria-label', minimize.title);
    minimize.setAttribute('aria-pressed', stripMinimized ? 'true' : 'false');
  }
}

// ── Activity display strip split-drag ──
{
  const handle = document.getElementById('activity-split-handle');
  const strip = document.getElementById('activity-display-strip');
  const logStream = document.getElementById('log-stream');
  let dragging = false;

  handle.addEventListener('mousedown', (e) => {
    if (!stripExpanded || stripMinimized) return;
    dragging = true;
    handle.classList.add('dragging');
    document.body.style.cursor = 'row-resize';
    document.body.style.userSelect = 'none';
    e.preventDefault();
  });

  document.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const tab = document.getElementById('tab-activity');
    if (!tab) return;
    const tabRect = tab.getBoundingClientRect();
    const y = e.clientY - tabRect.top;
    const minPx = 60;
    const maxPx = tabRect.height - 100;
    const h = Math.max(minPx, Math.min(maxPx, y));
    strip.style.height = h + 'px';
    stripHeight = h;
  });

  document.addEventListener('mouseup', () => {
    if (dragging) {
      dragging = false;
      handle.classList.remove('dragging');
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    }
  });
}


// ── ui-v2 Live-display right rail (design-overhaul P2) ─────────────────
// Display-only mirror chrome, active only under the ?ui=v2 flag: renders
// rail rows FROM existing state (displaySlots + slot DOM, the Station
// peer-display chips, the sb-display-access grant chip) via
// MutationObservers, and proxies every click to the existing controls.
// It owns no state, sends no messages, and never touches the WebRTC
// slots, the single-reparented <video> elements, or v1 markup. Honest
// authority copy: taking control is last-take-wins displacement between
// viewers (no request/approve ceremony), and "another viewer has input"
// is a first-class rendered state.
(() => {
  const tab = document.getElementById('tab-displays');
  if (!tab) return;
  const rail = document.createElement('aside');
  rail.className = 'ui2-live-rail';
  rail.id = 'ui2-live-rail';
  rail.innerHTML = `
    <section>
      <div class="ui2-live-rail-eyebrow">Displays</div>
      <div class="ui2-live-rail-list" id="ui2-live-displays-list"></div>
    </section>
    <section>
      <div class="ui2-live-rail-eyebrow">Input authority</div>
      <div id="ui2-live-authority-card"></div>
    </section>
    <section>
      <div class="ui2-live-rail-eyebrow">Peer displays</div>
      <div class="ui2-live-rail-list" id="ui2-live-peer-list"></div>
    </section>
    <section>
      <div class="ui2-live-rail-eyebrow">Your screen</div>
      <div id="ui2-live-yourscreen"></div>
    </section>`;
  tab.appendChild(rail);
  const displaysList = rail.querySelector('#ui2-live-displays-list');
  const authorityCard = rail.querySelector('#ui2-live-authority-card');
  const peerList = rail.querySelector('#ui2-live-peer-list');
  const yourScreen = rail.querySelector('#ui2-live-yourscreen');

  const emptyHint = (text) => {
    const div = document.createElement('div');
    div.className = 'ui2-live-rail-empty';
    div.textContent = text;
    return div;
  };

  function renderDisplayRows() {
    displaysList.textContent = '';
    const slots = Array.from(displaySlots.values());
    if (!slots.length) {
      displaysList.appendChild(emptyHint('No displays active. They appear here when the agent launches a GUI or you share your screen.'));
      return;
    }
    for (const slot of slots) {
      const row = document.createElement('button');
      row.type = 'button';
      const err = slot.statusEl && slot.statusEl.classList.contains('error');
      row.className = 'ui2-live-row' + (slot.connected ? ' ok viewing' : (err ? ' err' : ''));
      const dot = document.createElement('span');
      dot.className = 'ui2-live-row-dot';
      const main = document.createElement('span');
      main.className = 'ui2-live-row-main';
      const title = document.createElement('span');
      title.className = 'ui2-live-row-title';
      const labelEl = slot.el && slot.el.querySelector('.display-label');
      title.textContent = (labelEl && labelEl.textContent) || `Display ${slot.displayId}`;
      const meta = document.createElement('span');
      meta.className = 'ui2-live-row-meta';
      meta.textContent = (slot.statusEl && slot.statusEl.textContent) || '';
      main.appendChild(title);
      main.appendChild(meta);
      row.appendChild(dot);
      row.appendChild(main);
      if (displayAgentVisibility.get(Number(slot.displayId)) === false) {
        const tag = document.createElement('span');
        tag.className = 'ui2-live-row-tag';
        tag.textContent = 'PRIVATE';
        tag.title = 'Private view — the agent cannot see this display';
        row.appendChild(tag);
      }
      if (slot.connected) {
        const tag = document.createElement('span');
        tag.className = 'ui2-live-row-tag';
        tag.textContent = 'VIEWING';
        row.appendChild(tag);
      }
      row.title = 'Scroll this display into view';
      row.addEventListener('click', () => {
        if (slot.el && slot.el.isConnected) slot.el.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      });
      displaysList.appendChild(row);
    }
  }

  const AUTH_LABEL = {
    you: 'you',
    other: 'another viewer',
    unclaimed: 'shared',
    unknown: 'view-only',
  };
  function renderAuthorityCard() {
    authorityCard.textContent = '';
    const card = document.createElement('div');
    card.className = 'ui2-live-card';
    const title = document.createElement('div');
    title.className = 'ui2-live-card-title';
    const sub = document.createElement('div');
    sub.className = 'ui2-live-card-sub';
    const slots = Array.from(displaySlots.values());
    if (!slots.length) {
      title.textContent = 'No live display';
      sub.textContent = 'Input authority appears here once a display is streaming.';
      card.appendChild(title);
      card.appendChild(sub);
      authorityCard.appendChild(card);
      return;
    }
    const anyYou = slots.some(s => s.authorityState === 'you');
    const anyOther = slots.some(s => s.authorityState === 'other');
    title.textContent = anyYou
      ? 'You have input control'
      : anyOther
        ? 'Another viewer has input'
        : 'You have view-only';
    sub.textContent = 'Take control forwards your mouse and keyboard to the display. It displaces whoever holds input — last take wins, there is no approval step. Release hands the display back.';
    card.appendChild(title);
    card.appendChild(sub);
    for (const slot of slots) {
      const row = document.createElement('div');
      row.className = 'ui2-live-auth-row';
      const name = document.createElement('span');
      name.className = 'ui2-live-auth-name';
      const labelEl = slot.el && slot.el.querySelector('.display-label');
      name.textContent = (labelEl && labelEl.textContent) || `Display ${slot.displayId}`;
      const pill = document.createElement('span');
      const st = slot.authorityState || 'unknown';
      pill.className = 'ui2-live-state-pill' + (st === 'you' ? ' you' : st === 'other' ? ' other' : '');
      pill.textContent = AUTH_LABEL[st] || AUTH_LABEL.unknown;
      row.appendChild(name);
      row.appendChild(pill);
      card.appendChild(row);
      const btn = document.createElement('button');
      btn.type = 'button';
      if (st === 'you') {
        btn.className = 'ui2-live-card-btn release';
        btn.textContent = 'Release control';
        btn.title = 'Release input and return this display to view-only';
        btn.addEventListener('click', () => {
          const b = document.getElementById(`ds-release-${slot.displayId}`);
          if (b) b.click();
        });
      } else {
        btn.className = 'ui2-live-card-btn';
        btn.textContent = st === 'other' ? 'Take control anyway' : 'Take control';
        btn.title = st === 'other'
          ? 'Takes input immediately and displaces the current viewer'
          : 'Take interactive control of this display (keyboard and mouse)';
        btn.addEventListener('click', () => {
          const b = document.getElementById(`ds-take-${slot.displayId}`);
          if (b) b.click();
        });
      }
      card.appendChild(btn);
    }
    authorityCard.appendChild(card);
  }

  function renderPeerRows() {
    peerList.textContent = '';
    const chips = document.querySelectorAll('#station-peer-chips .station-peer-chip');
    if (!chips.length) {
      peerList.appendChild(emptyHint('No peer displays advertised. Paired daemons that share a display appear here.'));
      return;
    }
    chips.forEach((chip) => {
      const row = document.createElement('button');
      row.type = 'button';
      row.className = 'ui2-live-row' + (chip.disabled ? '' : ' ok');
      row.disabled = chip.disabled;
      row.title = chip.title || '';
      const dot = document.createElement('span');
      dot.className = 'ui2-live-row-dot';
      const main = document.createElement('span');
      main.className = 'ui2-live-row-main';
      const title = document.createElement('span');
      title.className = 'ui2-live-row-title';
      title.textContent = chip.textContent || '';
      const meta = document.createElement('span');
      meta.className = 'ui2-live-row-meta';
      meta.textContent = chip.disabled ? 'peer offline' : 'peer · view display';
      main.appendChild(title);
      main.appendChild(meta);
      const chev = document.createElement('span');
      chev.className = 'ui2-live-row-chev';
      chev.textContent = '›';
      row.appendChild(dot);
      row.appendChild(main);
      row.appendChild(chev);
      row.addEventListener('click', () => chip.click());
      peerList.appendChild(row);
    });
  }

  function renderYourScreen() {
    yourScreen.textContent = '';
    const card = document.createElement('div');
    card.className = 'ui2-live-card';
    const head = document.createElement('div');
    head.style.display = 'flex';
    head.style.alignItems = 'center';
    head.style.gap = '8px';
    const title = document.createElement('div');
    title.className = 'ui2-live-card-title';
    title.style.flex = '1';
    title.textContent = 'Your screen';
    // Two distinct things can be active here, and the card never
    // conflates them:
    //  - a PRIVATE VIEW ("View this machine"): remote view/control of
    //    this machine from the dashboard; the agent cannot see it;
    //  - an AGENT SHARE ("Share with agent"): the screen is visible to
    //    the agent for computer-use tasks.
    // (Streaming frames to the live presence/voice model is a third,
    // separate control -- the Stream button on the display tile.)
    const granted = userDisplayGranted;
    const shared = granted && userDisplayAgentVisible;
    const pill = document.createElement('span');
    pill.className = 'ui2-live-state-pill'
      + (shared ? ' other' : granted ? ' you' : '');
    pill.textContent = shared ? 'agent can see this' : granted ? 'private view' : 'off';
    head.appendChild(title);
    head.appendChild(pill);
    const sub = document.createElement('div');
    sub.className = 'ui2-live-card-sub';
    sub.textContent = shared
      ? 'The agent can see and drive this screen for computer-use tasks until you revoke access.'
      : granted
        ? 'Streaming to your dashboard only. The agent cannot see this display.'
        : 'View and control this machine’s display from here, or share it with the agent for computer-use tasks. You choose the display and can stop at any time.';
    card.appendChild(head);
    card.appendChild(sub);
    const addBtn = (label, cls, title, onClick) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'ui2-live-card-btn ' + cls;
      btn.textContent = label;
      btn.title = title;
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        onClick();
      });
      card.appendChild(btn);
      return btn;
    };
    if (!granted) {
      // Primary: private remote view. Secondary: the agent share.
      addBtn('View this machine', 'secondary',
        'Watch and control this machine’s display from the dashboard. Private: the agent cannot see it.',
        () => { if (typeof startUserDisplayGrantFlow === 'function') startUserDisplayGrantFlow('view'); });
      addBtn('Share with agent…', 'secondary',
        'Make this screen visible to the agent for computer-use tasks. Revocable at any time.',
        () => { if (typeof startUserDisplayGrantFlow === 'function') startUserDisplayGrantFlow('share'); });
    } else if (!shared) {
      addBtn('Stop viewing', 'danger',
        'Close the private view of this machine.',
        () => { if (typeof revokeUserDisplayNow === 'function') revokeUserDisplayNow(); });
      addBtn('Share with agent', 'secondary',
        'Upgrade this private view: make the display visible to the agent for computer-use tasks.',
        () => { if (typeof shareUserDisplayWithAgent === 'function') shareUserDisplayWithAgent(); });
    } else {
      addBtn('Revoke access', 'danger',
        'Take the display away from the agent and stop streaming it.',
        () => { if (typeof revokeUserDisplayNow === 'function') revokeUserDisplayNow(); });
    }
    yourScreen.appendChild(card);
  }

  let railRaf = 0;
  function renderRail() {
    railRaf = 0;
    renderDisplayRows();
    renderAuthorityCard();
    renderPeerRows();
    renderYourScreen();
  }
  function scheduleRail() {
    if (railRaf) return;
    railRaf = requestAnimationFrame(renderRail);
  }
  const observe = (el, opts) => {
    if (!el) return;
    new MutationObserver(scheduleRail).observe(el, opts);
  };
  // Slots come and go / status text + authority chips restyle in place.
  observe(document.getElementById('displays-container'),
    { subtree: true, childList: true, attributes: true, attributeFilter: ['class', 'style'], characterData: true });
  // Station peer chips re-render on peer_display_ready/removed.
  observe(document.getElementById('station-peer-chips'),
    { subtree: true, childList: true, attributes: true, attributeFilter: ['class', 'disabled', 'title'] });
  // user_session grant chip flips text + .granted.
  observe(document.getElementById('sb-display-access'),
    { childList: true, attributes: true, attributeFilter: ['class'], characterData: true, subtree: true });
  renderRail();
})();
