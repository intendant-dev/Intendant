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

// ── LOCAL display viewer policy ─────────────────────────────────────────
// The named home for every deliberate difference between this class and
// the federated PeerDisplayConnection (whose counterpart is
// PEER_DISPLAY_POLICY in 52-peer-display.js). The shared mechanics live
// in 45-display-viewer-core; each field below cites the decision it
// carries. Pure consolidation: each method's behavior is byte-identical
// to the inline code it replaced.
const DISPLAY_SLOT_POLICY = {
  name: 'local-display-slot',

  // ICE config — STUN/TURN servers from [webrtc].ice_servers TOML config,
  // default empty for local LAN deployments. Goes through the shared
  // helper so the peer-display path can't drift in what it hands to the
  // browser's ICE agent. NO relay pinning here — that is the FEDERATED
  // policy (#41–#45): local display must never be forced through TURN.
  buildRtcConfig() {
    return { iceServers: buildIceServersFromGatewayConfig(gatewayConfig) };
  },

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
  //
  // (The FEDERATED policy is the opposite: an explicit VP8 pin, #67.)
  applyCodecPreferences(_videoTransceiver) {},

  // **#58** (LOCAL ONLY): inject `a=rid:<rid> recv` lines +
  // `a=simulcast:recv <rids>` into the m=video section before
  // setLocalDescription. `<rids>` is `DISPLAY_SIMULCAST_RIDS` (default
  // `['f']` — single-RID receive post-#58; opt-in `['f','h','q']` for
  // the experimental multi-encoding adaptive-bandwidth path). The
  // federated path deliberately SKIPS this injection (#46: rtc 0.9
  // answers a recv-simulcast hint on a single-encoding track with a
  // malformed multi-RID/single-SSRC shape the browser refuses to
  // decode).
  mungeOfferSdp(sdp) {
    return injectRecvSimulcastIntoVideoOffer(sdp, DISPLAY_SIMULCAST_RIDS);
  },

  // Retry semantics: renegotiate IN PLACE on the same slot — the
  // server-side DisplaySession survives an ICE failure, so disconnect()
  // + connect() issues a fresh offer the same session answers. The
  // attempt counter lives on the instance (`_reconnectAttempts`). The
  // peer path instead re-opens with a fresh session id (its WebRtcPeer
  // lifecycle cannot re-offer). Budget/backoff/dead-end copy are the
  // shared DISPLAY_VIEWER_RETRY_* constants.
  retrySemantics: 'renegotiate-in-place',

  // Signaling transport: the verified dashboard-control tunnel
  // (displayWebRtcSignal) first, legacy /ws lane frames
  // (display_offer / display_ice / display_answer) as the direct-origin
  // fallback — see sendDisplayOffer / sendDisplayIceCandidate. The peer
  // path signals through the daemon HTTP/tunnel facade
  // (api_peer_webrtc_signal) instead.
  signalingLane: 'dashboard-control-or-ws',

  // Container resolution: a fixed stage — the slot owns its DOM for its
  // whole life (canvasEl/overlayEl/metricsEl created once in the
  // constructor and reparented as a unit). The peer path re-resolves
  // Station-aware containers on every render because its pane DOM is
  // rebuilt by daemons-list re-renders.
  containerResolution: 'fixed-stage',

  // Clipboard sync: LOCAL ONLY today (paste interceptor + remote
  // clipboard_update applier). Federated clipboard is a follow-up.
  clipboardSync: true,

  // Attach/annotation stream naming: `display_<id>` (byte-identical to
  // the pre-provider strings; the peer path namespaces by host —
  // `peer_<safeHost>_display_<id>` — so ids stay unique across hosts).
  streamBase(slot) {
    return 'display_' + slot.displayId;
  },
};

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
    this._recordingPendingAction = null;
    this._recordingPendingTimer = null;
    this._recordingStartedAt = null;  // client clock at server-confirmed start
    this._recordingTimerId = null;    // 1 Hz elapsed-label ticker while recording
    this._recordingDeletePending = false;
    this._recordingDeleteStream = null;
    this._recordingDeleteTimer = null;
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
    // Item F6: parked-on-transport-outage poll. Non-null while this slot
    // is waiting for display signaling to return instead of burning its
    // bounded retry budget on offers that cannot leave the browser.
    this._transportWaitTimer = null;
    this._streamCanvas = document.createElement('canvas');
    this._focusResizeObserver = null;
    this._boundHandlers = {};
    this._fullscreenInertRecords = [];
    this._fullscreenReturnFocus = null;
    this.el = document.createElement('div');
    this.el.className = 'display-slot';
    const label = displayLabel(displayId);
    this.el.innerHTML = `
      <div class="display-toolbar" role="group">
        <div class="display-toolbar-meta">
          <span class="display-label"></span>
          <span class="display-visibility" id="ds-visibility-${displayId}" style="display:none"></span>
          <span class="display-status" id="ds-status-${displayId}" role="status" aria-live="polite" aria-atomic="true">Connecting...</span>
          <span class="display-input-authority" id="ds-authority-${displayId}" style="display:none" title="Input authority for this display: who can drive keyboard and pointer input."></span>
        </div>
        <div class="display-toolbar-actions">
          <button class="take-control-btn" id="ds-take-${displayId}" type="button" title="Take interactive control of this display (keyboard and mouse)">Take control</button>
          <button class="release-control-btn" id="ds-release-${displayId}" type="button" style="display:none" title="Release control and return display to view-only mode">Release</button>
          <input class="release-note" id="ds-note-${displayId}" aria-label="Note to the agent when releasing this display" placeholder="Note (optional)" style="display:none">
          <button class="stream-btn" id="ds-stream-${displayId}" type="button" aria-pressed="false" title="Continuously send screenshots of this display to the live presence (voice) model. Main agents are not affected.">Stream</button>
          <button class="ann-attach-btn" id="ds-attach-${displayId}" type="button" title="Capture current frame and attach to next task">Attach</button>
          <button class="annotate-btn" id="ds-annotate-${displayId}" type="button" aria-pressed="false" title="Freeze current frame and annotate it">&#9998; Annotate</button>
          <button class="callout-btn" id="ds-callout-${displayId}" type="button" aria-pressed="false" disabled title="Call out a region: arm, then drag a rectangle on the frame to attach it to the next task (needs input control)">&#x2316; Callout</button>
          <button class="record-btn" id="ds-record-${displayId}" type="button" aria-pressed="false" title="Record this display (ffmpeg)">Record</button>
          <button class="delete-recording-btn" id="ds-delete-rec-${displayId}" type="button" style="display:none" title="Delete recording files for this display">Delete</button>
          <button class="display-fullscreen-btn" id="ds-fullscreen-${displayId}" type="button" aria-label="Open display full screen" aria-pressed="false" title="Full screen">&#x26F6;</button>
          <button class="display-close-btn" id="ds-close-${displayId}" type="button" aria-label="Close this display stream" title="Close this display stream">&times;</button>
          <span class="stream-frame-id" id="ds-frame-${displayId}" style="display:none;font-size:10px;color:var(--overlay0)"></span>
        </div>
      </div>
      <div class="display-canvas" id="display-canvas-${displayId}"></div>`;
    this.el.setAttribute('aria-label', label);
    const labelEl = this.el.querySelector('.display-label');
    if (labelEl) labelEl.textContent = label;
    const toolbarEl = this.el.querySelector('.display-toolbar');
    if (toolbarEl) toolbarEl.setAttribute('aria-label', `${label} controls`);
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
    // The IDL property is camelCase — the lowercase assignment set a dead
    // expando, so the element never actually carried playsinline. Set the
    // attribute too for engines that read it directly.
    this.videoEl.playsInline = true;
    this.videoEl.setAttribute('playsinline', '');
    this.videoEl.muted = true;
    this.videoEl.style.width = '100%';
    this.videoEl.style.backgroundColor = '#000';
    this.videoEl.setAttribute('aria-label', `Live view of ${label}`);
    this.videoEl.setAttribute('aria-describedby', `ds-status-${displayId} ds-authority-${displayId}`);
    this.canvasEl.appendChild(this.videoEl);
    // Live-video pause guard (shared driver in 45-display-viewer-core —
    // WebKit pauses muted live video on tab switches / DOM reparents and
    // never auto-resumes; full rationale there). Armed once: this element
    // lives for the slot's whole life. "Live" is pc + connected; the
    // breadcrumb rides the display activity rail so a pause-storm is
    // visible instead of masquerading as network lag.
    displayViewerArmPauseGuard(
      this,
      this.videoEl,
      () => Boolean(this.pc && this.connected),
      () => {
        if (typeof window.noteLiveDisplayLifecycle === 'function') {
          window.noteLiveDisplayLifecycle(this.displayId, 'attention',
            'Playback auto-resumed after a browser pause');
        }
      },
    );
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
    this.metricsEl.setAttribute('aria-hidden', 'true');
    this.canvasEl.appendChild(this.metricsEl);
    this.controlBannerEl = document.createElement('div');
    this.controlBannerEl.className = 'display-control-banner';
    this.controlBannerEl.textContent = 'You have control — keyboard and pointer input drive this display.';
    this.canvasEl.appendChild(this.controlBannerEl);
    // Host identity chip (bottom-right stage chrome): "{host} · {display}"
    // with the gradient identity square. Text is kept fresh by the live
    // workspace projection (selfHostLabel arrives async via the agent
    // card); hidden in thumb/fullscreen contexts by ui2-live.css.
    this.hostChipEl = document.createElement('div');
    this.hostChipEl.className = 'cu-host-chip';
    this.hostChipEl.setAttribute('aria-hidden', 'true');
    const hostMark = document.createElement('span');
    hostMark.className = 'cu-host-chip-mark';
    const hostText = document.createElement('span');
    hostText.className = 'cu-host-chip-text';
    this.hostChipEl.appendChild(hostMark);
    this.hostChipEl.appendChild(hostText);
    this.canvasEl.appendChild(this.hostChipEl);
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
    this.el.addEventListener('keydown', event => this._handleFullscreenKeydown(event));
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
        if (slot.el.classList.contains('display-fullscreen')) slot.toggleFullscreen(false);
      }
    }
    if (want && !this.el.classList.contains('display-fullscreen')) {
      this._enterFullscreenA11y();
    } else if (!want && this.el.classList.contains('display-fullscreen')) {
      this._exitFullscreenA11y();
    }
    this.el.classList.toggle('display-fullscreen', want);
    const anyFullscreen = want || Array.from(displaySlots.values()).some(slot =>
      slot !== this && slot.el.classList.contains('display-fullscreen')
    );
    document.body.classList.toggle('display-fullscreen-open', anyFullscreen);
    if (this.fullscreenBtn) {
      this.fullscreenBtn.innerHTML = want ? '&times;' : '&#x26F6;';
      this.fullscreenBtn.title = want ? 'Exit full screen' : 'Full screen';
      this.fullscreenBtn.setAttribute('aria-label', want ? 'Exit display full screen' : 'Open display full screen');
      this.fullscreenBtn.setAttribute('aria-pressed', want ? 'true' : 'false');
    }
    if (want && this.fullscreenBtn) this.fullscreenBtn.focus();
  }

  _enterFullscreenA11y() {
    this._fullscreenReturnFocus = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    this._fullscreenInertRecords = [];
    let branch = this.el;
    while (branch && branch.parentElement) {
      const parent = branch.parentElement;
      for (const sibling of parent.children) {
        if (sibling === branch || !(sibling instanceof HTMLElement)) continue;
        this._fullscreenInertRecords.push({
          element: sibling,
          inert: sibling.inert,
          ariaHidden: sibling.getAttribute('aria-hidden'),
        });
        sibling.inert = true;
        sibling.setAttribute('aria-hidden', 'true');
      }
      branch = parent;
      if (parent === document.body) break;
    }
    this.el.setAttribute('role', 'dialog');
    this.el.setAttribute('aria-modal', 'true');
  }

  _exitFullscreenA11y() {
    for (const record of this._fullscreenInertRecords) {
      record.element.inert = record.inert;
      if (record.ariaHidden === null) record.element.removeAttribute('aria-hidden');
      else record.element.setAttribute('aria-hidden', record.ariaHidden);
    }
    this._fullscreenInertRecords = [];
    this.el.removeAttribute('role');
    this.el.removeAttribute('aria-modal');
    // A responsive breakpoint may have changed while fullscreen owned the
    // background. Re-project the drawer's current media-query state instead
    // of leaving stale inert/aria-hidden snapshots on its rail or stage.
    if (typeof window.syncLiveDisplayDrawerState === 'function') {
      window.syncLiveDisplayDrawerState();
    }
    const returnFocus = this._fullscreenReturnFocus;
    this._fullscreenReturnFocus = null;
    if (returnFocus && returnFocus.isConnected && typeof returnFocus.focus === 'function') {
      requestAnimationFrame(() => returnFocus.focus());
    }
  }

  _handleFullscreenKeydown(event) {
    if (!this.el.classList.contains('display-fullscreen')) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      this.toggleFullscreen(false);
      return;
    }
    if (event.key !== 'Tab') return;
    const focusable = Array.from(this.el.querySelectorAll(
      'button:not([disabled]), input:not([disabled]), video[tabindex], [tabindex]:not([tabindex="-1"])'
    )).filter(element => element.getClientRects().length > 0);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
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
    return displayViewerRasterizeSurface(this.videoEl, w, h, quality);
  }

  /// Capture the currently-rendered video frame and queue it as a pending
  /// attachment. Works whether or not the display is currently streaming —
  /// just rasterizes whatever the <video> element is showing right now.
  /// The frame-id scheme and upload live in the shared attach lane
  /// (45-display-viewer-core); the stream name is the LOCAL policy
  /// (`display_<id>`).
  async attachCurrentFrame() {
    const frame = this.captureCurrentFrame(0.85, { logicalResolution: true });
    if (!frame) {
      this.attachBtn.title = 'No frame available yet';
      setTimeout(() => { this.attachBtn.title = 'Capture current frame and attach to next task'; }, 2000);
      return;
    }
    if (!(await displayViewerUploadAttachFrame(this, DISPLAY_SLOT_POLICY.streamBase(this), frame))) {
      return;
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
      streamBase: DISPLAY_SLOT_POLICY.streamBase(this),
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
  // annotation-attach lane (shared wiring in 45-display-viewer-core;
  // machinery in 47-annotation-clips). Armable only while input
  // authority is 'you' (button disabled otherwise, disarmed on loss).
  toggleCallout() {
    displayViewerToggleCallout(this, this.calloutBtn);
  }

  sendLegacyDisplaySignal(payload) {
    if (!app) return false;
    // Refused-send contract (see sendRawMessage): only an explicit false
    // means the frame never reached an open /ws.
    if (app.send_raw) {
      return app.send_raw(JSON.stringify(payload)) !== false;
    }
    if (app.send_server_action) {
      return app.send_server_action(payload) !== false;
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
    // ICE config: DISPLAY_SLOT_POLICY.buildRtcConfig — no relay pinning
    // on the local path (that's the federated policy).
    this.pc = new RTCPeerConnection(DISPLAY_SLOT_POLICY.buildRtcConfig());

    // Add a recvonly video transceiver so the SDP offer includes a video
    // media section. Without this, the server can't attach its video track
    // because the answerer can't introduce new media lines.
    const videoTransceiver = this.pc.addTransceiver('video', { direction: 'recvonly' });

    // Codec order: **#58** — deliberately a no-op (browser default order;
    // WKWebView lands hardware H.264). Full rationale on
    // DISPLAY_SLOT_POLICY.applyCodecPreferences.
    DISPLAY_SLOT_POLICY.applyCodecPreferences(videoTransceiver);

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
        // From here on, "frames stopped advancing" is the failure mode
        // the no-track watchdog can't see — hand over to the freeze
        // watchdog for the rest of this negotiation.
        this._armFreezeWatchdog();
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
        // Item F6: the bounded budget measures THIS DISPLAY's failures.
        // While the signaling transport itself is down (daemon restart,
        // event-lane reconnect), an offer cannot even leave the browser
        // — park on the transport instead of burning attempts into a
        // void and dead-ending every open slot.
        if (!this._displaySignalingAvailable()) {
          this._waitForDisplaySignaling();
          return;
        }
        const attempts = (this._reconnectAttempts || 0) + 1;
        this._reconnectAttempts = attempts;
        if (attempts <= DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS) {
          const delay = displayViewerRetryDelayMs(attempts);
          // disconnect() first: it clears watchdogs/samplers and hides
          // the overlay, so set the retry copy AFTER it runs.
          this.disconnect();
          this.statusEl.textContent = `Connection failed, reconnecting in ${delay/1000}s (attempt ${attempts})...`;
          this.statusEl.className = 'display-status error';
          this._setStageOverlay('progress', `Connection failed — reconnecting in ${delay/1000}s (attempt ${attempts} of ${DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS})…`);
          setTimeout(() => {
            if (this._closedByUser) return;
            // The transport can die during the backoff — re-check at
            // fire time so this attempt parks instead of failing into
            // the dead-end overlay while nothing can be signaled.
            if (!this._displaySignalingAvailable()) {
              this._waitForDisplaySignaling();
              return;
            }
            this.connect();
          }, delay);
        } else {
          // Dead end used to be terminal with no control; now it alarms
          // and offers a manual Reconnect that restarts the retry budget.
          this.statusEl.textContent = DISPLAY_VIEWER_RETRY_DEAD_END_STATUS;
          this.statusEl.className = 'display-status error';
          this._stopStatsSampler();
          this._setStageOverlay('error', DISPLAY_VIEWER_RETRY_DEAD_END_OVERLAY, {
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
    // Simulcast injection is the LOCAL-ONLY munge policy (#58 single-RID
    // receive; rationale on DISPLAY_SLOT_POLICY.mungeOfferSdp). Munge
    // BEFORE setLocalDescription so the localDescription matches what's
    // sent on the wire — server-side SDP-validation tests parse the
    // received offer/local-description and assume the recv-RID list
    // matches the configured constant.
    this.pc.createOffer().then(offer => {
      const munged = {
        type: offer.type,
        sdp: DISPLAY_SLOT_POLICY.mungeOfferSdp(offer.sdp),
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
      // Item F6: an offer that failed while the signaling transport is
      // down is the transport's failure, not this display's — park until
      // it returns rather than dead-ending on a Reconnect button whose
      // click couldn't signal either.
      if (!this._closedByUser && !this._displaySignalingAvailable()) {
        this._waitForDisplaySignaling();
        return;
      }
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

  // Re-kick playback if the element sits paused under a live connection
  // (tab return, missed pause event during a reparent). Safe to call any
  // time; no-ops unless connected with a stream attached. Shared driver
  // in 45-display-viewer-core (the constructor arms the guard; behavior —
  // 120ms macrotask delay, pending-dedupe, activity breadcrumb — is the
  // pre-extraction #299 semantics unchanged).
  resumeLiveVideoIfPaused() {
    displayViewerResumeLiveVideoIfPaused(this);
  }

  // Post-first-frame freeze watchdog (shared driver in
  // 45-display-viewer-core): armed by the first rendered frame of each
  // negotiation, cleared by disconnect(). Presented-frame progress via
  // rVFC (framesDecoded fallback); on a stall it first re-kicks playback
  // through the pause-guard path, then surfaces the stage overlay with
  // the manual Reconnect — never an automatic reconnect loop.
  _armFreezeWatchdog() {
    displayViewerArmFreezeWatchdog(this, {
      videoEl: () => this.videoEl,
      isLive: () => Boolean(this.pc && this.connected),
      tryResume: () => this.resumeLiveVideoIfPaused(),
      onStalled: (seconds) => {
        this.statusEl.textContent = `No new frames for ${seconds}s`;
        this.statusEl.className = 'display-status error';
        this._setStageOverlay('error',
          `No new frames for ${seconds}s — the connection reports connected but the video has stopped advancing. Reconnecting usually fixes it.`, {
            retryLabel: 'Reconnect',
            onRetry: () => this.manualReconnect(),
          });
        if (typeof window.noteLiveDisplayLifecycle === 'function') {
          window.noteLiveDisplayLifecycle(this.displayId, 'attention',
            `No new frames for ${seconds}s — stream looks frozen`);
        }
      },
      onRecovered: () => {
        this._setStageOverlay(null);
        if (this.connected) {
          const res = this.width > 0 ? ` ${this.width}x${this.height}` : '';
          this.statusEl.textContent = this.interactive
            ? `Interactive${res}`
            : `Connected (view-only)${res}`;
          this.statusEl.className = 'display-status connected';
        }
        if (typeof window.noteLiveDisplayLifecycle === 'function') {
          window.noteLiveDisplayLifecycle(this.displayId, 'live',
            'Stream frames resumed');
        }
      },
    });
  }

  // User-facing recovery entry point (overlay Reconnect button, revived
  // slots). Resets the bounded-retry budget and renegotiates from scratch.
  manualReconnect() {
    if (this._closedByUser) return;
    this._reconnectAttempts = 0;
    this.disconnect();
    this.connect();
  }

  // ── Item F6: don't burn the retry budget on transport outages ───────

  // Whether a display offer/ICE signal can leave the browser right now:
  // the verified dashboard-control tunnel (availability-driven —
  // daemonApi reports 'transport-down' during an outage and recovers
  // when the tunnel reconnects), else the legacy /ws bridge on
  // direct-origin dashboards. The /ws socket's own liveness is only
  // observable at send time, so bridge presence is the honest static
  // signal there — a refused send still surfaces through the offer
  // path's failure handling.
  _displaySignalingAvailable() {
    if (dashboardTransport?.canUseDisplayWebRtcSignal?.()) return true;
    if (dashboardConnectModeEnabled()) return false;
    return Boolean(app && (app.send_raw || app.send_server_action));
  }

  // Park this slot until display signaling returns, WITHOUT consuming
  // `_reconnectAttempts` — a daemon restart used to burn the whole
  // bounded budget on offers that could never leave the browser and
  // dead-end every open slot. Cleared by disconnect() (manual
  // Reconnect, user close, and the display_ready revive path all run
  // it); self-clears into connect() when the transport comes back.
  _waitForDisplaySignaling() {
    if (this._transportWaitTimer) return;
    // disconnect() first (mirrors the retry arm): it clears watchdogs
    // and samplers and hides the overlay — write the waiting copy after.
    this.disconnect();
    this.statusEl.textContent = 'Dashboard link to the daemon is down — waiting to reconnect…';
    this.statusEl.className = 'display-status warn';
    this._setStageOverlay('progress',
      'Dashboard link to the daemon is down — this display reconnects when it returns…');
    this._transportWaitTimer = window.setInterval(() => {
      if (this._closedByUser) {
        window.clearInterval(this._transportWaitTimer);
        this._transportWaitTimer = null;
        return;
      }
      if (!this._displaySignalingAvailable()) return;
      window.clearInterval(this._transportWaitTimer);
      this._transportWaitTimer = null;
      this.connect();
    }, 2000);
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
      this.takeBtn.textContent = pending ? 'Requesting…' : 'Take control';
      this.takeBtn.setAttribute('aria-busy', pending ? 'true' : 'false');
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
    // Activity thumbnails and hidden single-stage projections are
    // deliberately view-only. A late authority reply must never bind
    // keyboard/pointer/paste listeners after navigation hid its surface.
    if (activeTab !== 'displays' || this.el.classList.contains('ui2-live-inactive')) {
      this._releaseAuthority();
      return;
    }
    this.interactive = true;
    this.el.classList.add('is-interactive');
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

    // Input transport — the LOCAL policy, split by event class:
    //
    // DISCRETE events (kd/ku/md/mu — loss- and reorder-intolerant)
    // prefer the verified dashboard-control input lane and fall back to
    // this pc's reliable `control` channel. Never dropped.
    //
    // CONTINUOUS latest-wins events (mm/sc) go the other way round: the
    // purpose-built lossy `pointer` channel first (ordered:false,
    // maxRetransmits:0 — drop beats head-of-line blocking), because the
    // dashboard-control tunnel is reliable+ordered and SHARED with all
    // RPC/upload traffic — a pointer firehose queued behind a congested
    // tunnel replays stale moves in order and reads as catastrophic
    // remote-control lag. The tunnel remains the fallback while the
    // pointer channel isn't open (early negotiation, channel loss), and
    // it drops above a bufferedAmount watermark there (see
    // DashboardControlTransport.displayInput) rather than queueing.
    //
    // The daemon dispatches the 'control' and 'pointer' channel labels
    // through the same gated_input_handler as the tunnel lane, so only
    // the transport preference changes here — input-authority gating is
    // identical on every path. All raw sends are wrapped against throw
    // (close races, full SCTP buffers).
    const sendControl = (msg) => {
      try {
        if (sendDisplayInputForSlot(this.displayId, msg)) return true;
      } catch (_) { /* fall through to the data channel */ }
      if (this.controlChannel && this.controlChannel.readyState === 'open') {
        try {
          this.controlChannel.send(JSON.stringify(msg));
          return true;
        } catch (_) {}
      }
      return false;
    };
    const sendPointer = (msg) => {
      if (this.pointerChannel && this.pointerChannel.readyState === 'open') {
        try {
          this.pointerChannel.send(JSON.stringify(msg));
          return;
        } catch (_) { /* closing race / full buffer — try the tunnel */ }
      }
      try { sendDisplayInputForSlot(this.displayId, msg); } catch (_) {}
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
    this.el.classList.remove('is-interactive');
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
    vid.removeAttribute('tabindex');
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
    this.streamBtn.setAttribute('aria-pressed', 'true');
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
    this.streamBtn.setAttribute('aria-pressed', 'false');
    this.streamBtn.innerHTML = '&#x1F441; Stream';
    this.frameIdEl.style.display = 'none';
    this.frameIdEl.textContent = '';
  }
  toggleRecording() {
    if (!app || this._recordingPendingAction || this._recordingDeletePending) return;
    const baseStream = 'display_' + this.displayId;
    const stream = this.recordingStreamName || baseStream;
    const action = this.recording ? 'stop_recording' : 'start_recording';
    const targetStream = this.recording ? stream : baseStream;
    this._setRecordingPending(action);
    const fail = error => {
      // A late RPC failure must not roll back or toast over a newer server
      // event that already confirmed and cleared this exact command.
      if (this._recordingPendingAction !== action) return;
      this._failRecordingCommand(
        error?.message || `Could not ${this.recording ? 'stop' : 'start'} recording`
      );
    };
    const sent = dispatchDashboardActionMsg(
      { action, stream_name: targetStream },
      { onError: fail }
    );
    if (!sent) {
      fail(new Error('Dashboard control connection is unavailable'));
      return;
    }
    this._recordingPendingTimer = window.setTimeout(() => {
      this._recordingPendingTimer = null;
      if (!this._recordingPendingAction) return;
      this._failRecordingCommand('No recording confirmation arrived — check the display activity and retry.');
    }, 10000);
  }

  _setRecordingPending(action) {
    this._recordingPendingAction = action;
    this._renderRecordingControls();
  }

  _clearRecordingPending(render = true) {
    if (this._recordingPendingTimer) {
      window.clearTimeout(this._recordingPendingTimer);
      this._recordingPendingTimer = null;
    }
    this._recordingPendingAction = null;
    if (render) this._renderRecordingControls();
  }

  _clearRecordingDeletePending(render = true) {
    if (this._recordingDeleteTimer) {
      window.clearTimeout(this._recordingDeleteTimer);
      this._recordingDeleteTimer = null;
    }
    this._recordingDeletePending = false;
    this._recordingDeleteStream = null;
    if (render) this._renderRecordingControls();
  }

  // mm:ss since the server-confirmed recording start (applyRecordingState
  // stamps `_recordingStartedAt` on the false→true confirmation).
  _recordingElapsedLabel() {
    const base = this._recordingStartedAt || Date.now();
    const sec = Math.max(0, Math.floor((Date.now() - base) / 1000));
    const m = String(Math.floor(sec / 60)).padStart(2, '0');
    const s = String(sec % 60).padStart(2, '0');
    return `${m}:${s}`;
  }

  // Single writer for the 1 Hz elapsed ticker: runs exactly while the
  // confirmed state is "recording" with no pending command. Idempotent —
  // called from the render path so every state transition self-heals.
  _syncRecordingTimer() {
    const want = this.recording && !this._recordingPendingAction;
    if (want && !this._recordingTimerId) {
      this._recordingTimerId = window.setInterval(() => {
        this._renderRecordingControls();
      }, 1000);
    } else if (!want && this._recordingTimerId) {
      window.clearInterval(this._recordingTimerId);
      this._recordingTimerId = null;
    }
  }

  _renderRecordingControls() {
    const action = this._recordingPendingAction;
    const deleting = this._recordingDeletePending;
    this.recordBtn.disabled = Boolean(action || deleting);
    this.recordBtn.setAttribute('aria-busy', action ? 'true' : 'false');
    this.recordBtn.innerHTML = action
      ? (action === 'stop_recording' ? 'Stopping…' : 'Starting…')
      : (this.recording ? '&#x23F9; ' + this._recordingElapsedLabel() : '&#x23FA; Record');
    this.recordBtn.classList.toggle('active', this.recording);
    this.recordBtn.setAttribute('aria-pressed', this.recording ? 'true' : 'false');
    this.deleteRecBtn.disabled = Boolean(deleting || action);
    this.deleteRecBtn.setAttribute('aria-busy', deleting ? 'true' : 'false');
    this.deleteRecBtn.textContent = deleting ? 'Deleting…' : 'Delete';
    this.deleteRecBtn.style.display = this.recording || !this.recordingStreamName ? 'none' : '';
    this._syncRecordingTimer();
  }

  applyRecordingState(recording, streamName, deleted = false) {
    const sameAsCurrent = !this.recordingStreamName || !streamName ||
      this.recordingStreamName === streamName;
    // Recording events are base-mapped to slots, so a late stop/delete for
    // an older suffixed stream must not turn off a newer active recording.
    // Likewise, a negative event cannot confirm an in-flight Start.
    if ((!recording && this.recordingStreamName && !sameAsCurrent) ||
        (!recording && this._recordingPendingAction === 'start_recording')) {
      if (deleted && this._recordingDeletePending &&
          this._recordingDeleteStream === streamName) {
        this._clearRecordingDeletePending();
      }
      return false;
    }

    const confirmsRecordingCommand = recording
      ? this._recordingPendingAction === 'start_recording'
      : this._recordingPendingAction === 'stop_recording';
    if (confirmsRecordingCommand) this._clearRecordingPending(false);
    if (deleted && this._recordingDeletePending &&
        this._recordingDeleteStream === streamName) {
      this._clearRecordingDeletePending(false);
    }
    const wasRecording = this.recording;
    this.recording = Boolean(recording);
    // Elapsed base = this confirmation's arrival (a bootstrap replay of an
    // older recording restarts the visible counter — a known, honest
    // limitation: the daemon does not publish the start timestamp).
    if (this.recording && !wasRecording) this._recordingStartedAt = Date.now();
    if (!this.recording) this._recordingStartedAt = null;
    this.recordingStreamName = deleted ? null : (streamName || this.recordingStreamName);
    this._renderRecordingControls();
    return true;
  }

  _failRecordingCommand(message) {
    this._clearRecordingPending(false);
    this._clearRecordingDeletePending(false);
    // The last server-confirmed state remains authoritative.
    this._renderRecordingControls();
    if (typeof showControlToast === 'function') showControlToast('error', message);
  }
  async deleteRecording() {
    if (!app || this._recordingDeletePending || this._recordingPendingAction || this.recording) return;
    const stream = this.recordingStreamName || ('display_' + this.displayId);
    const ok = await showDashboardConfirm({
      title: 'Delete recording',
      message: `Delete recording for ${stream}?`,
      warning: 'Recording files will be removed from this session.',
      confirmLabel: 'Delete',
    });
    if (!ok) return;
    // State can change while the confirmation dialog is open.
    if (this._recordingDeletePending || this._recordingPendingAction || this.recording ||
        this.recordingStreamName !== stream) return;
    this._recordingDeletePending = true;
    this._recordingDeleteStream = stream;
    this._renderRecordingControls();
    const fail = error => {
      if (!this._recordingDeletePending) return;
      this._failRecordingCommand(error?.message || 'Could not delete the recording');
    };
    const sent = dispatchDashboardActionMsg(
      { action: 'delete_recording', stream_name: stream },
      { onError: fail }
    );
    if (!sent) {
      fail(new Error('Dashboard control connection is unavailable'));
      return;
    }
    this._recordingDeleteTimer = window.setTimeout(() => {
      this._recordingDeleteTimer = null;
      if (!this._recordingDeletePending) return;
      this._failRecordingCommand('No delete confirmation arrived — the recording is still listed.');
    }, 10000);
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
    displayViewerClearFreezeWatchdog(this);
    if (this._transportWaitTimer) {
      window.clearInterval(this._transportWaitTimer);
      this._transportWaitTimer = null;
    }
    this._stopStatsSampler();
    this._setStageOverlay(null);
    this._clearRecordingPending();
    this._clearRecordingDeletePending();
    // Transient disconnects keep the elapsed ticker (the server-side
    // recording continues); a user close removes the slot, so the ticker
    // must die with it.
    if (userInitiated && this._recordingTimerId) {
      window.clearInterval(this._recordingTimerId);
      this._recordingTimerId = null;
    }
    this.stopStreaming();
    // Flip `connected` BEFORE `_exitInteractive` so its status-text
    // ternary writes 'Disconnected' (not the stale 'Connected (view-
    // only)') on the way out — otherwise the chip flickers through
    // the connected text for a microtask before the post-cleanup
    // assignment overwrote it.
    this.connected = false;
    this._exitInteractive(userInitiated);
    if (userInitiated) {
      // Held-key keyups from `_exitInteractive` must cross the input gate
      // before the authority release closes it. This is the same ordering
      // as releaseControl(); close/remove paths cannot safely reverse it.
      this._releaseAuthority();
    }
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
  if (typeof window.retireLiveDisplayWorkspaceSlot === 'function') {
    window.retireLiveDisplayWorkspaceSlot(displayId);
  }
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
    const noteEl = document.createElement('div');
    noteEl.className = 'shared-view-focus-note';
    noteEl.hidden = true;
    focus.appendChild(noteEl);
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
  // Box geometry in stage-local px (kept for the note clamp below even on
  // the percentage fallback path).
  let boxLeft; let boxTop; let boxW; let boxH;
  if (videoRect && videoRect.width > 0 && videoRect.height > 0 && canvasRect.width > 0 && canvasRect.height > 0 && videoWidth > 0 && videoHeight > 0) {
    const scale = Math.min(videoRect.width / videoWidth, videoRect.height / videoHeight);
    const frameW = videoWidth * scale;
    const frameH = videoHeight * scale;
    const frameX = videoRect.left - canvasRect.left + ((videoRect.width - frameW) / 2);
    const frameY = videoRect.top - canvasRect.top + ((videoRect.height - frameH) / 2);
    boxLeft = frameX + x * frameW;
    boxTop = frameY + y * frameH;
    boxW = w * frameW;
    boxH = h * frameH;
    focus.style.left = boxLeft.toFixed(1) + 'px';
    focus.style.top = boxTop.toFixed(1) + 'px';
    focus.style.width = boxW.toFixed(1) + 'px';
    focus.style.height = boxH.toFixed(1) + 'px';
  } else {
    boxLeft = x * canvasRect.width;
    boxTop = y * canvasRect.height;
    boxW = w * canvasRect.width;
    boxH = h * canvasRect.height;
    focus.style.left = (x * 100).toFixed(3) + '%';
    focus.style.top = (y * 100).toFixed(3) + '%';
    focus.style.width = (w * 100).toFixed(3) + '%';
    focus.style.height = (h * 100).toFixed(3) + '%';
  }
  positionSharedViewFocusNote(
    focus, note, { left: boxLeft, top: boxTop, width: boxW, height: boxH },
    canvasRect.width, canvasRect.height);
}

// Keep the focus note readable wherever the region lands: below the box
// when that fits inside the stage, flipped above when it doesn't, and
// always clamped into the stage box (the canvas clips at its edges, so an
// unclamped chip near a corner renders as a cut-off sliver or nothing).
const SHARED_FOCUS_NOTE_PAD = 8;   // stage-edge breathing room
const SHARED_FOCUS_NOTE_GAP = 6;   // box ↔ chip spacing

function positionSharedViewFocusNote(focus, note, box, canvasW, canvasH) {
  const noteEl = focus.querySelector('.shared-view-focus-note');
  if (!noteEl) return;
  const text = String(note || '');
  if (noteEl.textContent !== text) noteEl.textContent = text;
  noteEl.hidden = text === '';
  if (text === '' || !(canvasW > 0) || !(canvasH > 0)) return;
  const pad = SHARED_FOCUS_NOTE_PAD;
  const gap = SHARED_FOCUS_NOTE_GAP;
  const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), Math.max(lo, hi));
  // Cap the chip's width to the stage before measuring it.
  noteEl.style.maxWidth = Math.round(clamp(canvasW - 2 * pad, 60, 360)) + 'px';
  const noteW = noteEl.offsetWidth;
  const noteH = noteEl.offsetHeight;
  const left = clamp(box.left, pad, canvasW - noteW - pad);
  let top = box.top + box.height + gap;
  if (top + noteH > canvasH - pad) top = box.top - gap - noteH; // flip above
  top = clamp(top, pad, canvasH - noteH - pad);
  // The chip is positioned relative to the focus box (its offset parent).
  noteEl.style.left = (left - box.left).toFixed(1) + 'px';
  noteEl.style.top = (top - box.top).toFixed(1) + 'px';
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
  // Current daemons resolve auto-detect to a concrete target. A legacy null
  // event is only safe to bind when handleSharedViewEvent saw exactly one
  // existing stream; never guess here while bootstrap slots are still arriving.
  if (sharedViewState.displayId === null) return;
  if (sharedViewState.displayId !== null && Number(slot.displayId) !== sharedViewState.displayId) {
    return;
  }
  // The Live workspace is a selected-display stage. Foreground a new
  // advisory target only when doing so will not discard active human work;
  // the banner and row decoration still make a deferred target discoverable.
  let foregrounded = true;
  if (typeof window.selectLiveDisplay === 'function') {
    foregrounded = window.selectLiveDisplay(slot.displayId, {
      source: 'shared-view',
      advisory: true,
    });
  }
  updateSharedViewBanner();
  slot.el.classList.add('shared-view-active');
  renderSharedViewFocus(slot, sharedViewState.region, sharedViewState.note);
  if (activeTab === 'displays' && foregrounded) {
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

// CU-05 (docs/cu-e2e-findings-2026-07-13.md): retract the focus overlay +
// note WITHOUT dismissing the shared view. Fired by the explicit
// clear_shared_view_focus verb and by the daemon's lifecycle auto-clears
// (display revoked, owning session ended). Idempotent: with nothing shown
// (or after hide) it is a no-op.
function clearSharedViewFocusAnnotation(evt) {
  if (!sharedViewState.visible) return;
  sharedViewState.region = null;
  sharedViewState.note = '';
  // Demote a "Focus" banner back to plain viewing; other action labels
  // (input_request's Take-input affordance, capture) are not the
  // annotation's and stay.
  if (sharedViewState.action === 'focus') sharedViewState.action = 'show';
  // A lifecycle clear names its cause ("display access revoked", "owning
  // session ended") — surface it as the banner detail.
  const reason = String((evt && evt.reason) || '').trim();
  if (reason) sharedViewState.reason = reason;
  for (const slot of displaySlots.values()) {
    const focus = slot.canvasEl && slot.canvasEl.querySelector('.shared-view-focus-box');
    if (focus) focus.remove();
  }
  updateSharedViewBanner();
}

function takeSharedViewInput() {
  if (sharedViewState.displayId === null) return;
  const slot = displaySlots.get(sharedViewState.displayId);
  if (!slot) return;
  // This click is an explicit human decision, unlike the agent's advisory
  // shared-view event: select the requested surface (safely releasing the
  // previous one) before asking for its input authority.
  if (activeTab !== 'displays' && typeof routeTo === 'function') {
    if (routeTo('displays') === false) return;
  }
  if (typeof window.selectLiveDisplay === 'function') {
    const selected = window.selectLiveDisplay(slot.displayId, {
      source: 'shared-view-input',
      focusStage: true,
    });
    if (!selected) return;
  }
  slot.takeControl();
}

function handleSharedViewEvent(evt) {
  // Native shared_view historically emitted `input`; MCP already emits
  // the presentation-level `input_request`. Canonicalize at the browser
  // boundary so mixed-version daemons still expose the real Take input
  // affordance without ever granting authority automatically.
  const rawAction = String(evt.action || 'show');
  const action = rawAction === 'input' ? 'input_request' : rawAction;
  if (action === 'hide') {
    hideSharedView();
    return;
  }
  if (action === 'focus_clear') {
    clearSharedViewFocusAnnotation(evt);
    return;
  }
  sharedViewState.visible = true;
  sharedViewState.action = action;
  sharedViewState.displayId = normalizeSharedViewDisplayId(evt);
  sharedViewState.displayTarget = String(evt.display_target || '');
  sharedViewState.reason = String(evt.reason || '');
  sharedViewState.note = String(evt.note || '');
  sharedViewState.region = evt.region || null;

  const slot = sharedViewState.displayId !== null
    ? displaySlots.get(sharedViewState.displayId)
    : displaySlots.size === 1
      ? displaySlots.values().next().value
      : null;
  if (slot && sharedViewState.displayId === null) {
    sharedViewState.displayId = Number(slot.displayId);
  }
  clearSharedViewDecorations();
  updateSharedViewBanner();
  if (activeTab !== 'displays' && (activeTab !== 'activity' || activeActivitySubtab !== 'log')) {
    routeTo('activity', 'log');
  }
  const activityStrip = document.getElementById('activity-display-strip');
  if (activeTab === 'activity' && activeActivitySubtab === 'log' && activityStrip) {
    activityStrip.classList.remove('hidden');
  }
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
  const thumbLabel = document.createElement('span');
  thumbLabel.className = 'thumb-label';
  // Window titles come from the OS and are not trusted markup.
  thumbLabel.textContent = displayLabel(displayId, true);
  thumb.appendChild(thumbLabel);
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


// ── Live display workspace ──────────────────────────────────────────────
// The standalone CU concept is the presentation target; the production
// displaySlots map remains the source of truth. This projection adds one
// selected local stage, selected-slot authority controls, responsive rail
// access, and a per-display feed of REAL events: browser-observed display
// lifecycle changes plus daemon-reported CU actions (the `cu_action` wire
// lane, appended via window.noteCuDisplayActivity from 45b-cu-overlays.js).
// It still never invents holder identities, and display-scoped approval
// attribution only renders when the daemon reported which session drives
// the display (45b's attribution map from cu_action events).
//
// Key safety rule: a hidden interactive slot would keep its document-level
// paste handler. Selecting another display therefore releases the old slot
// through DisplaySlot.releaseControl(), which flushes held keys before
// releasing server authority. Live annotation/callout ownership is torn
// down through the existing provider lifecycle before that slot is hidden.
(() => {
  const tab = document.getElementById('tab-displays');
  const main = document.getElementById('ui2-live-main');
  const container = document.getElementById('displays-container');
  const rail = document.getElementById('ui2-live-rail');
  const displaysList = document.getElementById('ui2-live-displays-list');
  const authorityCard = document.getElementById('ui2-live-authority-card');
  const activityList = document.getElementById('ui2-live-activity');
  const peerList = document.getElementById('ui2-live-peer-list');
  const yourScreen = document.getElementById('ui2-live-yourscreen');
  const mobileSummary = document.getElementById('ui2-live-mobile-summary');
  const railToggle = document.getElementById('ui2-live-rail-toggle');
  const railClose = document.getElementById('ui2-live-rail-close');
  const railScrim = document.getElementById('ui2-live-rail-scrim');
  if (!tab || !container || !rail || !displaysList || !authorityCard ||
      !activityList || !peerList || !yourScreen) return;

  const AUTH_LABEL = {
    you: 'you',
    other: 'another viewer',
    unclaimed: 'available',
    unknown: 'connecting',
  };
  const displayRows = new Map();
  const peerRows = new Map();
  const peerSources = new Map();
  const slotSnapshots = new Map();
  const activityByDisplay = new Map();
  const activityRows = new Map();
  const drawerMedia = window.matchMedia('(max-width: 1279px)');
  let selectedDisplayId = null;
  let railOpen = false;
  let railRaf = 0;
  let activitySeq = 0;
  let activityRenderedDisplayId = null;
  let lastActivitySignature = '';
  let lastAuthoritySignature = '';
  let lastScreenSignature = '';

  function slotLabel(slot) {
    const labelEl = slot && slot.el && slot.el.querySelector('.display-label');
    return (labelEl && labelEl.textContent) || displayLabel(slot && slot.displayId);
  }

  function selectedSlot() {
    return selectedDisplayId === null ? null : displaySlots.get(selectedDisplayId) || null;
  }

  function slotHasActiveUserWork(slot) {
    if (!slot) return false;
    return Boolean(
      slot.interactive ||
      slot._takeControlPending ||
      slot.authorityState === 'you' ||
      slot.el?.classList.contains('display-fullscreen') ||
      slot.el?.contains(document.activeElement) ||
      rail.contains(document.activeElement) ||
      (typeof shouldSuppressDisplayInputForAnnotation === 'function' &&
        shouldSuppressDisplayInputForAnnotation(slot)) ||
      (typeof liveCalloutArmedFor === 'function' && liveCalloutArmedFor(slot))
    );
  }

  function slotHasBlockingSurfaceWork(slot) {
    if (!slot) return false;
    return Boolean(
      (typeof shouldSuppressDisplayInputForAnnotation === 'function' &&
        shouldSuppressDisplayInputForAnnotation(slot)) ||
      (typeof liveCalloutArmedFor === 'function' && liveCalloutArmedFor(slot))
    );
  }

  function announceBlockedSurfaceSwitch() {
    if (typeof showControlToast === 'function') {
      showControlToast(
        'info',
        'Finish or close the current annotation/callout before changing displays.'
      );
    }
  }

  function emptyHint(textValue) {
    const div = document.createElement('div');
    div.className = 'ui2-live-rail-empty';
    div.textContent = textValue;
    return div;
  }

  function setSelectedProjection() {
    const slots = Array.from(displaySlots.values());
    const selectedExists = selectedDisplayId !== null && displaySlots.has(selectedDisplayId);
    container.classList.toggle('ui2-live-single-stage', slots.length > 0);
    container.dataset.activeDisplayId = selectedExists ? String(selectedDisplayId) : '';
    for (const slot of slots) {
      const active = selectedExists && Number(slot.displayId) === selectedDisplayId;
      slot.el.classList.toggle('ui2-live-selected', active);
      slot.el.classList.toggle('ui2-live-inactive', !active);
      slot.el.setAttribute('aria-hidden', active ? 'false' : 'true');
      slot.el.inert = !active;
    }
  }

  function teardownSelectedSurface(slot) {
    if (!slot) return;
    // releaseControl is the only safe ordering: held-key keyups are sent
    // before the server-side authority release closes the input gate. A
    // pending Take must also be cancelled: its late `you` reply would
    // otherwise install document-level paste and input listeners on the
    // display after this projection has hidden it. Releasing an already-
    // held but locally unbound authority is idempotent and avoids leaving
    // a hidden display reserved during the server round trip.
    if (slot.interactive || slot._takeControlPending || slot.authorityState === 'you') {
      slot.releaseControl();
    }
    if (slot.el?.classList.contains('display-fullscreen')) {
      slot.toggleFullscreen(false);
    }
    if (typeof teardownLiveSurfaceForOwner === 'function') {
      teardownLiveSurfaceForOwner(slot);
    }
  }

  function selectLiveDisplay(displayId, options) {
    const opts = options || {};
    const id = Number(displayId);
    const next = Number.isFinite(id) ? displaySlots.get(id) : null;
    if (!next) return false;
    if (selectedDisplayId !== id) {
      const current = selectedSlot();
      if (opts.advisory && slotHasActiveUserWork(current)) return false;
      if (slotHasBlockingSurfaceWork(current)) {
        announceBlockedSurfaceSwitch();
        return false;
      }
      teardownSelectedSurface(current);
      selectedDisplayId = id;
    }
    setSelectedProjection();
    scheduleWorkspace();
    if (opts.focusStage && next.el && next.el.isConnected) {
      requestAnimationFrame(() => {
        try { next.el.scrollIntoView({ block: 'nearest', inline: 'nearest' }); } catch (_) {}
      });
    }
    return true;
  }
  window.selectLiveDisplay = selectLiveDisplay;
  window.retireLiveDisplayWorkspaceSlot = function(displayId) {
    const id = Number(displayId);
    const record = displayRows.get(id);
    if (record) record.row.remove();
    displayRows.delete(id);
    slotSnapshots.delete(id);
    activityByDisplay.delete(id);
    if (selectedDisplayId === id) selectedDisplayId = null;
    lastActivitySignature = '';
    lastAuthoritySignature = '';
    scheduleWorkspace();
  };
  window.canDeactivateLiveDisplayWorkspace = function(options) {
    const blocking = Array.from(displaySlots.values()).find(slotHasBlockingSurfaceWork);
    if (!blocking) return true;
    if (!options || options.announce !== false) announceBlockedSurfaceSwitch();
    return false;
  };
  window.deactivateLiveDisplayWorkspace = function() {
    if (!window.canDeactivateLiveDisplayWorkspace()) return false;
    for (const slot of displaySlots.values()) {
      if (slot.interactive || slot._takeControlPending || slot.authorityState === 'you') {
        slot.releaseControl();
      }
      if (slot.el?.classList.contains('display-fullscreen')) {
        slot.toggleFullscreen(false);
      }
      if (typeof teardownLiveSurfaceForOwner === 'function') {
        teardownLiveSurfaceForOwner(slot);
      }
    }
    scheduleWorkspace();
    return true;
  };

  function reconcileSelectedDisplay(slots) {
    if (selectedDisplayId !== null && displaySlots.has(selectedDisplayId)) {
      setSelectedProjection();
      return;
    }
    // applySharedViewToSlot selects a newly requested shared-view target
    // once. If the workspace initializes after that event, prefer its
    // decorated slot only for this initial/fallback choice. Never force it
    // again: the user must remain free to inspect another live display.
    const shared = slots.find(slot => slot.el && slot.el.classList.contains('shared-view-active'));
    const fallback = shared || slots[0];
    selectedDisplayId = fallback ? Number(fallback.displayId) : null;
    setSelectedProjection();
  }

  function createDisplayRow(id) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'ui2-live-row';
    row.dataset.displayId = String(id);
    row.innerHTML =
      '<span class="ui2-live-row-dot" aria-hidden="true"></span>' +
      '<span class="ui2-live-row-main">' +
        '<span class="ui2-live-row-title"></span>' +
        '<span class="ui2-live-row-meta"></span>' +
      '</span>' +
      '<span class="ui2-live-row-tags" aria-hidden="true">' +
        '<span class="ui2-live-row-tag ui2-live-row-privacy"></span>' +
        '<span class="ui2-live-row-tag ui2-live-row-media"></span>' +
        '<span class="ui2-live-row-tag ui2-live-row-current"></span>' +
      '</span>';
    row.addEventListener('click', () => {
      const selected = selectLiveDisplay(id, { focusStage: true, source: 'rail' });
      if (selected && drawerMedia.matches) setRailOpen(false, true);
    });
    return {
      row,
      title: row.querySelector('.ui2-live-row-title'),
      meta: row.querySelector('.ui2-live-row-meta'),
      privacy: row.querySelector('.ui2-live-row-privacy'),
      media: row.querySelector('.ui2-live-row-media'),
      current: row.querySelector('.ui2-live-row-current'),
    };
  }

  function orderRows(parent, records) {
    records.forEach((record, index) => {
      const current = parent.children[index] || null;
      if (current !== record.row) parent.insertBefore(record.row, current);
    });
  }

  function syncDisplayRows(slots) {
    const liveIds = new Set(slots.map(slot => Number(slot.displayId)));
    for (const [id, record] of displayRows) {
      if (liveIds.has(id)) continue;
      record.row.remove();
      displayRows.delete(id);
      slotSnapshots.delete(id);
      activityByDisplay.delete(id);
    }

    let empty = displaysList.querySelector('.ui2-live-rail-empty');
    if (!slots.length) {
      if (!empty) {
        empty = emptyHint('No displays active. Agent workspaces and screens you choose to view appear here.');
        displaysList.appendChild(empty);
      }
      return;
    }
    if (empty) empty.remove();

    const ordered = [];
    for (const slot of slots) {
      const id = Number(slot.displayId);
      let record = displayRows.get(id);
      if (!record) {
        record = createDisplayRow(id);
        displayRows.set(id, record);
      }
      const label = slotLabel(slot);
      const state = slot.authorityState || 'unknown';
      const status = (slot.statusEl && slot.statusEl.textContent.trim()) || 'Connecting';
      const error = Boolean(slot.statusEl && slot.statusEl.classList.contains('error'));
      const active = id === selectedDisplayId;
      const privateView = displayAgentVisibility.get(id) === false;
      const presenceStreaming = Boolean(slot.streaming);
      const recording = Boolean(slot.recording);
      const current = selectedSlot();
      const releasesControl = !active && Boolean(
        current && (current.interactive || current._takeControlPending || current.authorityState === 'you')
      );
      const blocksForEdit = !active && slotHasBlockingSurfaceWork(current);
      const visibilityLabel = privateView
        ? 'private dashboard view; agent cannot see it'
        : (displayAgentVisibility.get(id) === true && userDisplayIds.has(id))
          ? 'shared with the agent'
          : 'agent workspace';
      const stateLabels = [
        visibilityLabel,
        presenceStreaming ? 'streaming frames to the presence model' : '',
        recording ? 'recording' : '',
        releasesControl ? 'switching here releases current input control' : '',
        blocksForEdit ? 'finish the current annotation or callout before switching' : '',
      ].filter(Boolean);

      record.row.classList.toggle('ok', Boolean(slot.connected));
      record.row.classList.toggle('err', error);
      record.row.classList.toggle('viewing', active);
      record.row.classList.toggle('selected', active);
      record.row.setAttribute('aria-pressed', active ? 'true' : 'false');
      if (active) record.row.setAttribute('aria-current', 'true');
      else record.row.removeAttribute('aria-current');
      record.row.title = active
        ? label + ' is shown on the stage'
        : blocksForEdit
          ? 'Finish the current annotation or callout before showing ' + label
          : 'Show ' + label + ' on the stage' +
            (releasesControl ? ' and release current input control' : '');
      record.row.setAttribute('aria-label',
        label + ', ' + status + ', input ' + (AUTH_LABEL[state] || AUTH_LABEL.unknown) +
        ', ' + stateLabels.join(', '));
      record.title.textContent = label;
      record.meta.textContent = status + ' · input ' + (AUTH_LABEL[state] || AUTH_LABEL.unknown);
      record.privacy.textContent = privateView ? 'PRIVATE' : '';
      record.privacy.title = privateView ? 'The agent cannot see this private view' : '';
      record.media.textContent = [presenceStreaming ? 'STREAM' : '', recording ? 'REC' : '']
        .filter(Boolean).join(' · ');
      record.media.classList.toggle('streaming', presenceStreaming);
      record.media.classList.toggle('recording', recording);
      record.current.textContent = active ? 'VIEWING' : (slot.connected ? 'LIVE' : '');
      ordered.push(record);
    }
    orderRows(displaysList, ordered);
  }

  authorityCard.innerHTML =
    '<div class="ui2-live-card ui2-live-authority-card">' +
      '<div class="ui2-live-authority-head">' +
        '<span class="ui2-live-authority-dot" aria-hidden="true"></span>' +
        '<div class="ui2-live-authority-copy">' +
          '<div class="ui2-live-card-title"></div>' +
          '<div class="ui2-live-card-sub"></div>' +
        '</div>' +
        '<span class="ui2-live-state-pill"></span>' +
      '</div>' +
      '<div class="ui2-live-auth-row">' +
        '<span class="ui2-live-auth-avatar" aria-hidden="true"></span>' +
        '<span class="ui2-live-auth-name"></span>' +
      '</div>' +
      '<button class="ui2-live-card-btn" type="button"></button>' +
    '</div>';
  const authorityBox = authorityCard.querySelector('.ui2-live-card');
  const authorityTitle = authorityCard.querySelector('.ui2-live-card-title');
  const authoritySub = authorityCard.querySelector('.ui2-live-card-sub');
  const authorityPill = authorityCard.querySelector('.ui2-live-state-pill');
  const authorityAvatar = authorityCard.querySelector('.ui2-live-auth-avatar');
  const authorityName = authorityCard.querySelector('.ui2-live-auth-name');
  const authorityButton = authorityCard.querySelector('.ui2-live-card-btn');
  authorityButton.addEventListener('click', () => {
    const slot = selectedSlot();
    if (!slot) return;
    if (slot.authorityState === 'you' && !slot.interactive) {
      // Selection-away releases bound input immediately. During the
      // authority round-trip (or after a failed release), let the holder
      // safely re-bind listeners instead of trapping it behind a Release-
      // only toolbar state.
      if (drawerMedia.matches) setRailOpen(false, true);
      slot.takeControl();
      return;
    }
    const source = slot.authorityState === 'you' ? slot.releaseBtn : slot.takeBtn;
    if (source === slot.takeBtn && drawerMedia.matches) setRailOpen(false, true);
    if (source && !source.disabled) source.click();
  });

  function syncAuthorityCard() {
    const slot = selectedSlot();
    const signature = slot
      ? [
          selectedDisplayId,
          slotLabel(slot),
          slot.authorityState || 'unknown',
          Boolean(slot.interactive),
          Boolean(slot._takeControlPending),
          Boolean(slot.pc),
        ].join('|')
      : 'none';
    if (signature === lastAuthoritySignature) return;
    lastAuthoritySignature = signature;
    authorityBox.className = 'ui2-live-card ui2-live-authority-card';
    authorityPill.className = 'ui2-live-state-pill';
    authorityButton.className = 'ui2-live-card-btn';
    if (!slot) {
      authorityTitle.textContent = 'No live display';
      authoritySub.textContent = 'Choose or share a display to see and control it here.';
      authorityPill.textContent = 'offline';
      authorityAvatar.hidden = true;
      authorityName.textContent = 'Input is scoped to one display at a time.';
      authorityButton.hidden = true;
      return;
    }

    const state = slot.authorityState || 'unknown';
    // Holder avatar: YOU when this dashboard holds input; another viewer's
    // identity is deliberately not on the wire, so "other" stays abstract;
    // agent-side / unclaimed / connecting reads AI.
    authorityAvatar.hidden = false;
    authorityAvatar.textContent = state === 'you' ? 'YOU' : state === 'other' ? '···' : 'AI';
    const pending = Boolean(slot._takeControlPending);
    authorityName.textContent = slotLabel(slot);
    authorityButton.hidden = false;
    authorityButton.disabled = !slot.pc || pending;
    authorityButton.setAttribute('aria-busy', pending ? 'true' : 'false');
    authorityBox.classList.add('state-' + state);
    if (state === 'you') {
      authorityTitle.textContent = slot.interactive
        ? 'You are driving this display'
        : 'You hold input authority';
      authoritySub.textContent =
        'Keyboard, pointer, and paste input are scoped to this display. Release it before handing control back.';
      authorityPill.classList.add('you');
      authorityPill.textContent = 'you';
      if (slot.interactive) {
        authorityButton.classList.add('release');
        authorityButton.textContent = 'Release control';
        authorityButton.title = 'Release input and return this display to view-only';
      } else {
        authorityButton.textContent = 'Resume control';
        authorityButton.title = 'Bind keyboard, pointer, and paste input to this display again';
      }
    } else if (state === 'other') {
      authorityTitle.textContent = 'Another viewer has input';
      authoritySub.textContent =
        'Taking control is immediate and displaces the current viewer. Last take wins; there is no approval step.';
      authorityPill.classList.add('other');
      authorityPill.textContent = 'another viewer';
      authorityButton.textContent = pending ? 'Requesting…' : 'Take control anyway';
      authorityButton.title = 'Take input immediately and displace the current viewer';
    } else if (state === 'unclaimed') {
      authorityTitle.textContent = 'Input is available';
      authoritySub.textContent =
        'No dashboard viewer holds exclusive input. Take control to bind keyboard, pointer, and paste input here.';
      authorityPill.textContent = 'available';
      authorityButton.textContent = pending ? 'Requesting…' : 'Take control';
      authorityButton.title = 'Take interactive control of this display';
    } else {
      authorityTitle.textContent = 'View only while input connects';
      authoritySub.textContent =
        'The stream can be watched now. Input controls become available when the authority state arrives.';
      authorityPill.textContent = 'connecting';
      authorityButton.textContent = pending ? 'Requesting…' : 'Take control';
      authorityButton.title = 'Input authority is not available yet';
    }
  }

  function visibilityMode(id) {
    const visible = displayAgentVisibility.get(id);
    if (visible === false) return 'private';
    if (visible === true && userDisplayIds.has(id)) return 'agent';
    return 'workspace';
  }

  function snapshotSlot(slot) {
    const statusEl = slot.statusEl;
    const connection = slot.connected
      ? 'connected'
      : statusEl && statusEl.classList.contains('error')
        ? 'error'
        : statusEl && statusEl.classList.contains('warn')
          ? 'warn'
          : 'connecting';
    return {
      connection,
      interactive: Boolean(slot.interactive),
      authority: slot.authorityState || 'unknown',
      streaming: Boolean(slot.streaming),
      recording: Boolean(slot.recording),
      visibility: visibilityMode(Number(slot.displayId)),
      shared: Boolean(slot.el && slot.el.classList.contains('shared-view-active')),
      annotating: Boolean(slot.annotateBtn && slot.annotateBtn.classList.contains('active')),
      callout: Boolean(slot.calloutBtn && slot.calloutBtn.getAttribute('aria-pressed') === 'true'),
    };
  }

  // Per-display feed cap. Raised from 10 for the action stream — lifecycle
  // events alone rarely pass 10, but real CU action traffic does.
  const ACTIVITY_MAX_ENTRIES = 50;

  function addDisplayActivity(displayId, kind, textValue, extra) {
    const id = Number(displayId);
    const entries = activityByDisplay.get(id) || [];
    const last = entries[entries.length - 1];
    // Consecutive-duplicate guard is for lifecycle transitions only: two
    // identical CU actions in a row (extra.action) are distinct real events
    // and must both append.
    if (!extra && last && last.kind === kind && last.text === textValue) return;
    entries.push({
      seq: ++activitySeq,
      kind,
      text: textValue,
      at: new Date(),
      raw: extra && extra.raw ? String(extra.raw) : '',
      action: Boolean(extra && extra.action),
    });
    if (entries.length > ACTIVITY_MAX_ENTRIES) {
      entries.splice(0, entries.length - ACTIVITY_MAX_ENTRIES);
    }
    activityByDisplay.set(id, entries);
  }

  // Entry point for the CU action-visualization layer (45b-cu-overlays.js):
  // appends a daemon-reported action to this display's feed using the
  // concept's two-line grammar (friendly sentence + raw mono call).
  window.noteCuDisplayActivity = function(displayId, kind, friendly, raw) {
    const id = Number(displayId);
    if (!Number.isFinite(id)) return;
    addDisplayActivity(id, kind, String(friendly || ''), {
      raw: String(raw || ''),
      action: true,
    });
    scheduleWorkspace();
  };

  // Lifecycle breadcrumbs from the slot layer (one-line rows, deduped by
  // addDisplayActivity's consecutive-repeat check) — e.g. the live-video
  // pause guard's auto-resume note.
  window.noteLiveDisplayLifecycle = function(displayId, kind, text) {
    const id = Number(displayId);
    if (!Number.isFinite(id)) return;
    addDisplayActivity(id, String(kind || 'neutral'), String(text || ''));
    scheduleWorkspace();
  };

  // Safari parks media in hidden tabs and can defer the element's own
  // pause event until nobody is listening usefully; returning to the tab
  // (or a bfcache restore) must therefore sweep every live slot. The
  // per-element pause guard in DisplaySlot covers reparents; this covers
  // backgrounding.
  const resumeAllLiveDisplayVideos = () => {
    for (const slot of displaySlots.values()) {
      if (typeof slot.resumeLiveVideoIfPaused === 'function') slot.resumeLiveVideoIfPaused();
    }
    // Federated peer panes (and the Station HUD thumbnails drawImage-ing
    // their video elements) have the same WebKit-parking exposure; their
    // registry lives beside PeerDisplayConnection (52-peer-display.js).
    for (const conn of peerDisplayConnections.values()) {
      if (typeof conn.resumeLiveVideoIfPaused === 'function') conn.resumeLiveVideoIfPaused();
    }
  };
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) resumeAllLiveDisplayVideos();
  });
  window.addEventListener('pageshow', resumeAllLiveDisplayVideos);

  function captureSlotActivity(slot) {
    const id = Number(slot.displayId);
    const next = snapshotSlot(slot);
    const prev = slotSnapshots.get(id);
    if (!prev) {
      addDisplayActivity(id, 'neutral', 'Display became available');
      if (next.connection === 'connected') addDisplayActivity(id, 'live', 'Live stream connected');
      else if (next.connection === 'error') addDisplayActivity(id, 'error', 'The stream needs attention');
      if (next.interactive) addDisplayActivity(id, 'control', 'Interactive input is active');
      if (next.visibility === 'private') addDisplayActivity(id, 'private', 'Private dashboard view started');
      if (next.visibility === 'agent') addDisplayActivity(id, 'share', 'Display shared with the agent');
      if (next.authority === 'you') addDisplayActivity(id, 'control', 'This dashboard holds input authority');
      if (next.authority === 'other') addDisplayActivity(id, 'attention', 'Another viewer holds input authority');
      if (next.shared) addDisplayActivity(id, 'focus', 'Agent shared this display with you');
      slotSnapshots.set(id, next);
      return;
    }

    if (prev.connection !== next.connection) {
      if (next.connection === 'connected') addDisplayActivity(id, 'live', 'Live stream connected');
      else if (next.connection === 'error') addDisplayActivity(id, 'error', 'The stream needs attention');
      else if (next.connection === 'warn') addDisplayActivity(id, 'attention', 'The stream is reconnecting');
      else addDisplayActivity(id, 'neutral', 'Connecting to the display');
    }
    if (prev.interactive !== next.interactive) {
      addDisplayActivity(id, next.interactive ? 'control' : 'neutral',
        next.interactive ? 'Interactive input is active' : 'Interactive input ended');
    }
    if (prev.authority !== next.authority) {
      if (next.authority === 'you') addDisplayActivity(id, 'control', 'You took input control');
      else if (next.authority === 'other') addDisplayActivity(id, 'attention', 'Another viewer took input control');
      else if (next.authority === 'unclaimed') addDisplayActivity(id, 'neutral', 'Input control was released');
      else addDisplayActivity(id, 'neutral', 'Input authority is reconnecting');
    }
    if (prev.streaming !== next.streaming) {
      addDisplayActivity(id, next.streaming ? 'share' : 'neutral',
        next.streaming ? 'Presence stream started' : 'Presence stream stopped');
    }
    if (prev.recording !== next.recording) {
      addDisplayActivity(id, next.recording ? 'recording' : 'neutral',
        next.recording ? 'Recording started' : 'Recording stopped');
    }
    if (prev.visibility !== next.visibility) {
      if (next.visibility === 'private') addDisplayActivity(id, 'private', 'Display changed to a private view');
      else if (next.visibility === 'agent') addDisplayActivity(id, 'share', 'Display shared with the agent');
      else addDisplayActivity(id, 'neutral', 'Agent display workspace active');
    }
    if (prev.shared !== next.shared) {
      addDisplayActivity(id, next.shared ? 'focus' : 'neutral',
        next.shared ? 'Agent shared this display with you' : 'Shared view ended');
    }
    if (prev.annotating !== next.annotating) {
      addDisplayActivity(id, next.annotating ? 'focus' : 'neutral',
        next.annotating ? 'Annotation editor opened' : 'Annotation editor closed');
    }
    if (prev.callout !== next.callout) {
      addDisplayActivity(id, next.callout ? 'focus' : 'neutral',
        next.callout ? 'Region callout armed' : 'Region callout cleared');
    }
    slotSnapshots.set(id, next);
  }

  // Raw CU payloads can be huge — a type() of a percent-encoded data: URL
  // runs to thousands of characters. Collapsed rows show a readable
  // preview (URL-decoded when the text is percent-encoded) plus the total
  // length; the full literal call is one click away.
  const CU_RAW_COLLAPSE_MIN = 160;  // rows at or under this render inline
  const CU_RAW_PREVIEW_CHARS = 96;  // collapsed preview length

  function cuRawPreview(raw) {
    let head = raw.slice(0, CU_RAW_PREVIEW_CHARS);
    // Percent-encoded blobs read better decoded (`%20name%3D` → " name=").
    // Only the preview decodes; expanding always shows the literal call.
    if (/%[0-9A-Fa-f]{2}/.test(head)) {
      try {
        head = decodeURIComponent(head.replace(/%(?![0-9A-Fa-f]{2})/g, '%25'));
      } catch (_) { /* not valid percent-encoding — keep the literal prefix */ }
    }
    // Keep the preview one tidy run: fold control chars and space runs.
    head = head.replace(/[\u0000-\u001F\u007F]+/g, ' ').replace(/ {2,}/g, ' ');
    return head + '… (' + raw.length + ' chars)';
  }

  function buildCuRawDetail(raw) {
    const el = document.createElement('div');
    el.className = 'cu-action-raw';
    if (raw.length <= CU_RAW_COLLAPSE_MIN) {
      el.textContent = raw;
      return el;
    }
    const preview = cuRawPreview(raw);
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'cu-action-raw-toggle';
    toggle.setAttribute('aria-expanded', 'false');
    toggle.title = 'Show the full input';
    toggle.textContent = preview;
    toggle.addEventListener('click', () => {
      const expand = toggle.getAttribute('aria-expanded') !== 'true';
      toggle.setAttribute('aria-expanded', expand ? 'true' : 'false');
      toggle.textContent = expand ? raw : preview;
      toggle.title = expand ? 'Show less' : 'Show the full input';
    });
    el.appendChild(toggle);
    return el;
  }

  function syncActivityList() {
    const entries = selectedDisplayId === null
      ? []
      : activityByDisplay.get(selectedDisplayId) || [];
    const signature = String(selectedDisplayId) + ':' +
      entries.map(entry => String(entry.seq)).join(',');
    if (signature === lastActivitySignature) return;
    lastActivitySignature = signature;
    if (activityRenderedDisplayId !== selectedDisplayId) {
      activityRenderedDisplayId = selectedDisplayId;
      activityRows.clear();
      activityList.replaceChildren();
    }
    if (!entries.length) {
      if (!activityList.querySelector('.ui2-live-rail-empty')) {
        activityList.appendChild(emptyHint(
          selectedDisplayId === null
            ? 'Select a display to see its connection, authority, sharing, and recording events.'
            : 'Display events will appear here as its real state changes.'));
      }
      return;
    }
    activityList.querySelector('.ui2-live-rail-empty')?.remove();
    const liveSeqs = new Set(entries.map(entry => entry.seq));
    for (const [seq, row] of activityRows) {
      if (liveSeqs.has(seq)) continue;
      row.remove();
      activityRows.delete(seq);
    }
    // Auto-follow policy (same as the main log): stick to the bottom only
    // when the user is already reading the bottom. Measure BEFORE appends;
    // a fresh fill (display switch) always lands on the latest entries.
    const freshFill = activityRows.size === 0;
    const nearBottom = freshFill ||
      (activityList.scrollHeight - activityList.scrollTop - activityList.clientHeight) <= 30;
    let appended = false;
    // role=log expects chronological DOM order so only the newly appended
    // row is announced. Rebuilding newest-first made every state change
    // sound like ten new events to screen readers.
    for (const entry of entries) {
      if (activityRows.has(entry.seq)) continue;
      const row = document.createElement('div');
      const dot = document.createElement('span');
      dot.className = 'ui2-live-activity-dot';
      dot.setAttribute('aria-hidden', 'true');
      const time = document.createElement('time');
      time.className = 'ui2-live-activity-time';
      time.dateTime = entry.at.toISOString();
      if (entry.action) {
        // Daemon-reported CU action: the concept's two-line grammar —
        // friendly sentence + seconds-precision ts, raw mono call below.
        row.className = 'ui2-live-activity-row cu-action-row kind-' + entry.kind;
        time.textContent = entry.at.toLocaleTimeString([], {
          hour: '2-digit', minute: '2-digit', second: '2-digit',
        });
        const main = document.createElement('div');
        main.className = 'cu-action-main';
        const head = document.createElement('div');
        head.className = 'cu-action-head';
        const friendly = document.createElement('span');
        friendly.className = 'cu-action-friendly';
        friendly.textContent = entry.text;
        head.appendChild(friendly);
        head.appendChild(time);
        main.appendChild(head);
        if (entry.raw) {
          main.appendChild(buildCuRawDetail(entry.raw));
        }
        row.appendChild(dot);
        row.appendChild(main);
      } else {
        row.className = 'ui2-live-activity-row kind-' + entry.kind;
        time.textContent = entry.at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        const textEl = document.createElement('span');
        textEl.className = 'ui2-live-activity-text';
        textEl.textContent = entry.text;
        row.appendChild(dot);
        row.appendChild(textEl);
        row.appendChild(time);
      }
      activityList.appendChild(row);
      activityRows.set(entry.seq, row);
      appended = true;
    }
    if (appended && nearBottom) {
      activityList.scrollTop = activityList.scrollHeight;
    }
  }

  yourScreen.innerHTML =
    '<div class="ui2-live-card ui2-live-screen-card">' +
      '<div class="ui2-live-screen-head">' +
        '<div class="ui2-live-card-title">Your screen</div>' +
        '<span class="ui2-live-state-pill"></span>' +
      '</div>' +
      '<div class="ui2-live-card-sub"></div>' +
      '<div class="ui2-live-screen-actions">' +
        '<button class="ui2-live-card-btn secondary" type="button"></button>' +
        '<button class="ui2-live-card-btn secondary" type="button"></button>' +
      '</div>' +
    '</div>';
  const screenPill = yourScreen.querySelector('.ui2-live-state-pill');
  const screenSub = yourScreen.querySelector('.ui2-live-card-sub');
  const screenButtons = Array.from(yourScreen.querySelectorAll('.ui2-live-card-btn'));

  function runScreenAction(action) {
    if (action === 'view' && typeof startUserDisplayGrantFlow === 'function') {
      startUserDisplayGrantFlow('view');
    } else if (action === 'share' && typeof startUserDisplayGrantFlow === 'function') {
      startUserDisplayGrantFlow('share');
    } else if (action === 'upgrade' && typeof shareUserDisplayWithAgent === 'function') {
      shareUserDisplayWithAgent();
    } else if (action === 'revoke' && typeof revokeUserDisplayNow === 'function') {
      revokeUserDisplayNow();
    }
  }
  screenButtons.forEach(button => {
    button.addEventListener('click', event => {
      event.stopPropagation();
      runScreenAction(button.dataset.action || '');
    });
  });

  function setScreenButton(button, config) {
    if (!config) {
      button.hidden = true;
      button.dataset.action = '';
      return;
    }
    button.hidden = false;
    button.dataset.action = config.action;
    button.textContent = config.label;
    button.title = config.title;
    button.className = 'ui2-live-card-btn ' + config.className;
  }

  function syncYourScreen() {
    const granted = userDisplayGranted;
    const shared = granted && userDisplayAgentVisible;
    const signature = granted ? (shared ? 'agent' : 'private') : 'off';
    if (signature === lastScreenSignature) return;
    lastScreenSignature = signature;
    screenPill.className = 'ui2-live-state-pill' + (shared ? ' other' : granted ? ' you' : '');
    screenPill.textContent = shared ? 'agent can see this' : granted ? 'private view' : 'off';
    if (!granted) {
      screenSub.textContent =
        'View this machine privately, or explicitly share a chosen screen with the agent for computer-use tasks.';
      setScreenButton(screenButtons[0], {
        action: 'view',
        label: 'View this machine',
        title: 'Private dashboard view. The agent cannot see this screen.',
        className: 'secondary',
      });
      setScreenButton(screenButtons[1], {
        action: 'share',
        label: 'Share with agent…',
        title: 'Choose a screen to share with the agent. Revocable at any time.',
        className: 'secondary',
      });
    } else if (!shared) {
      screenSub.textContent =
        'This is a private dashboard view. The agent cannot enumerate, capture, or drive it.';
      setScreenButton(screenButtons[0], {
        action: 'revoke',
        label: 'Stop viewing',
        title: 'Close this private display view.',
        className: 'danger',
      });
      setScreenButton(screenButtons[1], {
        action: 'upgrade',
        label: 'Share with agent',
        title: 'Make this display visible to the agent for computer-use tasks.',
        className: 'secondary',
      });
    } else {
      screenSub.textContent =
        'The agent can see and drive this screen for computer-use tasks until you revoke access.';
      setScreenButton(screenButtons[0], {
        action: 'revoke',
        label: 'Revoke access',
        title: 'Stop sharing this screen with the agent.',
        className: 'danger',
      });
      setScreenButton(screenButtons[1], null);
    }
  }

  function peerKey(chip, index) {
    const host = chip.dataset.hostId || '';
    const display = chip.dataset.displayId || '';
    return host || display
      ? host + ':' + display
      : (chip.getAttribute('aria-label') || chip.textContent || String(index));
  }

  function createPeerRow(key) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'ui2-live-row';
    row.innerHTML =
      '<span class="ui2-live-row-dot" aria-hidden="true"></span>' +
      '<span class="ui2-live-row-main">' +
        '<span class="ui2-live-row-title"></span>' +
        '<span class="ui2-live-row-meta">peer · opens in Station</span>' +
      '</span>' +
      '<span class="ui2-live-row-chev" aria-hidden="true">›</span>';
    row.addEventListener('click', () => {
      const chip = peerSources.get(key);
      if (!chip || chip.disabled) return;
      const hostId = chip.dataset.hostId || '';
      const displayId = Number(chip.dataset.displayId || 0);
      if (typeof routeTo === 'function' && routeTo('station') === false) return;
      if (drawerMedia.matches) setRailOpen(false, false);
      if (hostId && typeof stationOpenDisplay === 'function') {
        stationOpenDisplay(hostId, displayId);
      } else {
        chip.click();
      }
    });
    return {
      row,
      title: row.querySelector('.ui2-live-row-title'),
    };
  }

  function syncPeerRows() {
    const chips = Array.from(document.querySelectorAll('#station-peer-chips .station-peer-chip'));
    peerSources.clear();
    const liveKeys = new Set();
    const ordered = [];
    chips.forEach((chip, index) => {
      const key = peerKey(chip, index);
      liveKeys.add(key);
      peerSources.set(key, chip);
      let record = peerRows.get(key);
      if (!record) {
        record = createPeerRow(key);
        peerRows.set(key, record);
      }
      record.row.disabled = chip.disabled;
      record.row.classList.toggle('ok', !chip.disabled);
      record.row.title = chip.disabled
        ? (chip.title || 'Peer unavailable')
        : (chip.title || 'Open this peer display in Station');
      record.row.setAttribute('aria-label', chip.getAttribute('aria-label') || chip.textContent || 'Peer display');
      record.title.textContent = chip.textContent || 'Peer display';
      ordered.push(record);
    });
    for (const [key, record] of peerRows) {
      if (liveKeys.has(key)) continue;
      record.row.remove();
      peerRows.delete(key);
    }

    let empty = peerList.querySelector('.ui2-live-rail-empty');
    if (!ordered.length) {
      if (!empty) {
        empty = emptyHint('No peer displays advertised. Connected peers open in the Station workspace.');
        peerList.appendChild(empty);
      }
      return;
    }
    if (empty) empty.remove();
    orderRows(peerList, ordered);
  }

  function syncMobileSummary() {
    if (!mobileSummary) return;
    const slot = selectedSlot();
    if (!slot) {
      mobileSummary.textContent = 'No display selected';
      return;
    }
    const state = slot.authorityState || 'unknown';
    const slots = Array.from(displaySlots.values());
    const streamingCount = slots.filter(item => item.streaming).length;
    const recordingCount = slots.filter(item => item.recording).length;
    const parts = [
      slotLabel(slot),
      'input ' + (AUTH_LABEL[state] || AUTH_LABEL.unknown),
      streamingCount ? streamingCount + ' presence stream' + (streamingCount === 1 ? '' : 's') : '',
      recordingCount ? recordingCount + ' recording' + (recordingCount === 1 ? '' : 's') : '',
    ].filter(Boolean);
    mobileSummary.textContent = parts.join(' · ');
    mobileSummary.title = parts.join(', ');
  }

  function drawerFocusable() {
    return Array.from(rail.querySelectorAll(
      'button:not([disabled]):not([hidden]), input:not([disabled]):not([hidden]), select:not([disabled]):not([hidden]), [tabindex]:not([tabindex="-1"])'
    )).filter(element => element.getClientRects().length > 0);
  }

  function syncDrawerState(restoreFocus, force) {
    if (!force && document.body.classList.contains('display-fullscreen-open')) return;
    const drawer = drawerMedia.matches;
    const visible = !drawer || railOpen;
    tab.classList.toggle('ui2-live-rail-open', drawer && railOpen);
    rail.inert = !visible;
    if (visible) rail.removeAttribute('aria-hidden');
    else rail.setAttribute('aria-hidden', 'true');
    if (drawer) {
      rail.setAttribute('role', 'dialog');
      rail.setAttribute('aria-modal', railOpen ? 'true' : 'false');
    } else {
      rail.removeAttribute('role');
      rail.removeAttribute('aria-modal');
    }
    if (main) {
      main.inert = drawer && railOpen;
      if (drawer && railOpen) main.setAttribute('aria-hidden', 'true');
      else main.removeAttribute('aria-hidden');
    }
    if (railToggle) railToggle.setAttribute('aria-expanded', drawer && railOpen ? 'true' : 'false');
    if (railScrim) {
      railScrim.tabIndex = -1;
      railScrim.setAttribute('aria-hidden', drawer && railOpen ? 'false' : 'true');
    }
    if (restoreFocus && railToggle && drawer) railToggle.focus();
  }
  window.syncLiveDisplayDrawerState = () => syncDrawerState(false, true);

  function setRailOpen(open, restoreFocus) {
    railOpen = drawerMedia.matches && Boolean(open);
    syncDrawerState(Boolean(restoreFocus));
    if (railOpen) {
      requestAnimationFrame(() => {
        const focusable = drawerFocusable();
        (railClose || focusable[0] || rail).focus();
      });
    }
  }

  if (railToggle) railToggle.addEventListener('click', () => setRailOpen(!railOpen, false));
  if (railClose) railClose.addEventListener('click', () => setRailOpen(false, true));
  if (railScrim) railScrim.addEventListener('click', () => setRailOpen(false, true));
  rail.addEventListener('keydown', event => {
    if (!drawerMedia.matches || !railOpen || event.key !== 'Tab') return;
    const focusable = drawerFocusable();
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  });
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && drawerMedia.matches && railOpen) {
      // The portalled display picker is above the drawer and owns the
      // first Escape. Its later fragment closes it and restores focus;
      // leave the underlying controls drawer in place.
      if (document.getElementById('display-picker')?.classList.contains('visible')) return;
      event.preventDefault();
      setRailOpen(false, true);
    }
  });
  const onDrawerMediaChange = () => {
    const focusWasInRail = rail.contains(document.activeElement);
    if (typeof hideDisplayPicker === 'function' && displayPickerVisible) {
      hideDisplayPicker(false);
    }
    railOpen = false;
    syncDrawerState(false);
    if (drawerMedia.matches && focusWasInRail && railToggle) railToggle.focus();
  };
  if (typeof drawerMedia.addEventListener === 'function') {
    drawerMedia.addEventListener('change', onDrawerMediaChange);
  } else if (typeof drawerMedia.addListener === 'function') {
    drawerMedia.addListener(onDrawerMediaChange);
  }

  // Host identity chip text: "{host} · {display label}". The host label is
  // the daemon's agent-card display name (status bar's source of truth),
  // which arrives asynchronously — re-projected on every render.
  function syncHostChips(slots) {
    const host = (typeof selfHostLabel === 'string' && selfHostLabel) ? selfHostLabel : 'local';
    for (const slot of slots) {
      const textEl = slot.hostChipEl && slot.hostChipEl.querySelector('.cu-host-chip-text');
      if (!textEl) continue;
      const label = host + ' · ' + slotLabel(slot);
      if (textEl.textContent !== label) textEl.textContent = label;
    }
  }

  function renderWorkspace() {
    railRaf = 0;
    const slots = Array.from(displaySlots.values());
    reconcileSelectedDisplay(slots);
    for (const slot of slots) captureSlotActivity(slot);
    syncDisplayRows(slots);
    syncHostChips(slots);
    syncAuthorityCard();
    syncActivityList();
    syncPeerRows();
    syncYourScreen();
    syncMobileSummary();
  }

  function scheduleWorkspace() {
    if (railRaf) return;
    railRaf = requestAnimationFrame(renderWorkspace);
  }

  function observe(element, options) {
    if (!element) return;
    new MutationObserver(scheduleWorkspace).observe(element, options);
  }
  // The observer catches legacy direct DOM writes without rebuilding the
  // rail. Keyed rows and stable authority/screen controls preserve focus;
  // activity rendering is signature-gated, so the 3s metrics sampler is a
  // no-op unless a real display state changed.
  observe(container, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['class', 'style', 'aria-pressed', 'disabled'],
  });
  observe(document.getElementById('station-peer-chips'), {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['class', 'disabled', 'title', 'aria-label', 'data-host-id', 'data-display-id'],
  });
  observe(document.getElementById('sb-display-access'), {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['class'],
  });

  // Stable, side-effect-free browser QA surface. CDP cannot read the
  // module-scoped displaySlots map directly, so dashboard acceptance
  // probes use this documented window.qa convention.
  window.qa = Object.assign(window.qa || {}, {
    liveDisplay() {
      return {
        activeTab,
        selectedDisplayId,
        layout: drawerMedia.matches ? 'drawer' : 'rail',
        railOpen: !drawerMedia.matches || railOpen,
        userDisplayMode: userDisplayGranted
          ? (userDisplayAgentVisible ? 'agent' : 'private')
          : 'off',
        // Item F4: pointer moves deliberately dropped by the shared
        // reliable tunnel's bufferedAmount watermark (the fallback lane
        // for mm when the per-display pointer channel isn't open).
        inputTunnelMovesDropped: dashboardControlTunnelPointerMovesDropped,
        slots: Array.from(displaySlots.values()).map(slot => {
          const id = Number(slot.displayId);
          return {
            displayId: id,
            selected: id === selectedDisplayId,
            connected: Boolean(slot.connected),
            firstFrameSeen: Boolean(slot._firstFrameSeen),
            paused: Boolean(slot.videoEl && slot.videoEl.paused),
            // Freeze-watchdog snapshot (null until the first frame arms
            // it): { armed, source: 'rvfc'|'stats', stalledMs,
            // resumeAttempted, overlayShown }.
            freeze: displayViewerFreezeWatchQa(slot),
            pointerChannelOpen: Boolean(
              slot.pointerChannel && slot.pointerChannel.readyState === 'open'),
            reconnectAttempts: Number(slot._reconnectAttempts) || 0,
            waitingForSignalTransport: Boolean(slot._transportWaitTimer),
            intrinsicWidth: Number(slot.videoEl && slot.videoEl.videoWidth) || 0,
            intrinsicHeight: Number(slot.videoEl && slot.videoEl.videoHeight) || 0,
            authorityState: slot.authorityState || 'unknown',
            interactive: Boolean(slot.interactive),
            agentVisible: displayAgentVisibility.has(id)
              ? displayAgentVisibility.get(id)
              : null,
            streaming: Boolean(slot.streaming),
            recording: Boolean(slot.recording),
            activityCount: (activityByDisplay.get(id) || []).length,
          };
        }),
      };
    },
  });

  syncDrawerState(false);
  renderWorkspace();
})();
