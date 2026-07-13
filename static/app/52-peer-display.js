// Item 2: bounded auto-retry bookkeeping for peer displays, keyed
// `${hostId}|${displayId}`. Lives at module scope (not on the connection)
// because every retry replaces the PeerDisplayConnection with a fresh one
// via the full open path — the budget must survive that. Reset on a
// successful connect and by the manual Retry button.
const peerDisplayReconnectAttempts = new Map();

// ── FEDERATED peer-display viewer policy ────────────────────────────────
// The named home for every deliberate difference between
// PeerDisplayConnection and the local DisplaySlot (whose counterpart is
// DISPLAY_SLOT_POLICY in 45-displays-webrtc.js). The shared mechanics
// live in 45-display-viewer-core; each field below cites the decision it
// carries. Pure consolidation: each method's behavior is byte-identical
// to the inline code it replaced.
const PEER_DISPLAY_POLICY = {
  name: 'federated-peer-display',

  // ICE config — same shared helper as the local primary-display path
  // (DisplaySlot.connect). Default is empty (trust-the-network LAN
  // deployment); operators wanting STUN/TURN-relay-style ICE on the
  // federated path configure a real server through the daemon's
  // [webrtc].ice_servers TOML and both paths pick it up automatically.
  // Earlier hardcoded `iceServers: []` blocked any STUN/TURN config
  // from reaching this path even when one was set on the gateway.
  //
  // When a real TURN server (turn: / turns:) is configured we ALSO
  // pin `iceTransportPolicy: 'relay'` so the browser only uses
  // relay candidates. Diagnosed in #41/#42/#43 (commits 364f34b,
  // 84fcdc5, 3156534, all with diagnostic+revert dance): the rtc
  // 0.9 crate (Cargo.toml `rtc = "=0.9.0"`) does NOT advance DTLS
  // handshake over an ICE-TCP candidate — peer's server-role state
  // machine receives ClientHello but never emits ServerHello over
  // the TCP selected pair. The browser stays at dtlsState=connecting
  // with bytesReceived=0 from peer indefinitely. Forcing relay
  // routes media over UDP via the operator's TURN server (host
  // coturn at 192.168.1.223:3478 in the smoke topology) where rtc
  // handles DTLS normally. NOT applied to the local DisplaySlot
  // path — local display should never be forced through TURN.
  //
  // Without a TURN server, `iceTransportPolicy: 'relay'` would
  // guarantee ICE failure (no relay candidate to pair against), so
  // we leave the policy unset and emit a clear warn so the
  // operator sees it instead of a silent hang at dtlsState=
  // connecting.
  buildRtcConfig(log) {
    const iceServers = buildIceServersFromGatewayConfig(gatewayConfig);
    const pcConfig = { iceServers };
    if (hasTurnInIceServers(iceServers)) {
      pcConfig.iceTransportPolicy = 'relay';
      log('info',
        `iceTransportPolicy=relay (TURN configured: ${iceServers.length} server(s))`);
    } else {
      log('warn',
        `no TURN server in [webrtc].ice_servers — leaving iceTransportPolicy ` +
        `default. rtc 0.9 doesn't drive DTLS over ICE-TCP, so direct paths ` +
        `may stall at dtlsState=connecting; configure a turn:/turns: URL in ` +
        `intendant.toml to enable the only verified-working path.`);
    }
    return pcConfig;
  },

  // **#67 (federated VP8 A/B)**: pin codec preference to VP8 only.
  // Distinct from the local DisplaySlot path (#58) which deliberately
  // lets WKWebView default H.264 first to get the hardware-accelerated
  // VideoToolbox encoder on the macOS Mac viewing its own display.
  //
  // Federation's encoder is the *peer's* libx264 (software) — there
  // is no hardware-accel argument for H.264 here. And the H.264 path
  // is currently broken end-to-end on the federated smoke topology
  // (browser → host coturn → Debian UTM peer, all on one MacBook):
  // 13-22 % local TURN/virtio loss combined with full-res H.264 IDRs
  // of ~291 RTP packets makes IDR reassembly statistically impossible
  // (P(complete IDR) = 0.78^291 ≈ 1.5e-30). VP8 IDRs are smaller per
  // packet and survive better; this A/B confirms whether the block is
  // H.264-specific or lower in RTP/media.
  //
  // **Flag gate (`[webrtc].federation_allow_h264`)**: opt-in H.264 is
  // viable only through the current loss-resilience policy: federated
  // H.264 uses a quarter-resolution / capped-bitrate layer, periodic
  // IDRs with same-SSRC NACK retransmit, and small slices to keep
  // recovery bounded under relay loss. When the operator opts in via
  // `federation_allow_h264=true`, PREFER H.264 by reordering the
  // receiver's codec list so every `video/H264` variant comes FIRST
  // (then VP8 and the rest, preserved in their original relative order
  // as a fallback) and applying it via `setCodecPreferences`. Simply
  // skipping the pin is not enough: the browser default order is not
  // uniform across platforms — a Linux Chrome happened to put VP8 first,
  // so federation kept landing on VP8 and never exercised the peer's
  // federated H.264 layer. Putting H.264 first in the offer makes the
  // peer answer with H.264 whenever it can encode it. Default false
  // keeps VP8 the federation default (explicit VP8 pin, unchanged below).
  //
  // `RTCRtpReceiver.getCapabilities('video')` returns null on browsers
  // that don't implement it (rare in 2026 — Safari/WebKit, Chrome,
  // Firefox all support it); guard so a no-op fallback leaves the
  // transceiver at its browser default.
  // Gateway-wide config flag OR the per-session test override
  // (`?federation-h264=1` / `localStorage.federationH264='1'`). The
  // override only ADDS H.264 preference for this tab — it never disables
  // the VP8 default, so federation stays VP8 unless one of the two is
  // explicitly set. See `federationH264TestEnabled`.
  applyCodecPreferences(videoTransceiver, log) {
    const sessionH264Override = federationH264TestEnabled();
    const allowFederationH264 =
      !!(gatewayConfig && gatewayConfig.federation_allow_h264) || sessionH264Override;
    if (sessionH264Override) {
      log('info',
        'federation H.264 enabled for this session via ' +
        '?federation-h264=1 / localStorage.federationH264 (per-viewer test override)');
    }
    if (allowFederationH264 && videoTransceiver && typeof videoTransceiver.setCodecPreferences === 'function') {
      const caps = (typeof RTCRtpReceiver !== 'undefined' && RTCRtpReceiver.getCapabilities)
        ? RTCRtpReceiver.getCapabilities('video')
        : null;
      const allCodecs = caps && caps.codecs ? caps.codecs : [];
      const isH264 = (c) => c && c.mimeType && c.mimeType.toLowerCase() === 'video/h264';
      const h264 = allCodecs.filter(isH264);
      const rest = allCodecs.filter(c => !isH264(c));
      if (h264.length > 0) {
        const reordered = h264.concat(rest);
        try {
          videoTransceiver.setCodecPreferences(reordered);
          log('info',
            `federation_allow_h264=true — codec preference reordered to prefer ` +
            `H.264 first (${h264.length} H.264 variant(s), ${rest.length} other(s))`);
        } catch (e) {
          log('warn', `setCodecPreferences(H264-first) failed: ${e.message} — falling back to browser default`);
        }
      } else {
        log('warn',
          'federation_allow_h264=true but no H.264 in RTCRtpReceiver capabilities — ' +
          'leaving codec order at browser default');
      }
    } else if (videoTransceiver && typeof videoTransceiver.setCodecPreferences === 'function') {
      const caps = (typeof RTCRtpReceiver !== 'undefined' && RTCRtpReceiver.getCapabilities)
        ? RTCRtpReceiver.getCapabilities('video')
        : null;
      const vp8 = caps && caps.codecs
        ? caps.codecs.filter(c => c.mimeType && c.mimeType.toLowerCase() === 'video/vp8')
        : [];
      if (vp8.length > 0) {
        try {
          videoTransceiver.setCodecPreferences(vp8);
          log('info', `codec preference pinned to VP8 (${vp8.length} variant(s))`);
        } catch (e) {
          log('warn', `setCodecPreferences(VP8) failed: ${e.message} — falling back to browser default`);
        }
      } else {
        log('warn', 'no VP8 in RTCRtpReceiver capabilities — leaving codec order at browser default');
      }
    }
  },

  // **#46** (FEDERATED-ONLY SKIP): do NOT inject `a=simulcast:recv` for
  // the federated peer-display path — the munge policy is the identity.
  // When the offer carries a recv-simulcast hint and the peer's
  // negotiated codec is single-encoding (H.264 over the TURN-relay path
  // forced by #45), rtc 0.9's SDP generator emits an answer with three
  // a=rid:* send lines + a=simulcast:send f;h;q but only ONE a=ssrc
  // covering all three RIDs. The browser sees a malformed simulcast
  // track and silently refuses to decode — DTLS healthy, ICE healthy,
  // RTP flowing, video black.
  //
  // Empirical proof: #46's diagnostic (commit 3bc3b8e, reverted
  // in edea37c) forced active_rids = [SimulcastRid::full()]
  // server-side; the answer SDP shape did not change → the bug
  // is in rtc-crate SDP emission, not active_rids derivation.
  // Same diagnostic confirmed video renders end-to-end as soon
  // as the offer requests a single-encoding track.
  //
  // Local DisplaySlot.connect still injects recv-simulcast through
  // `injectRecvSimulcastIntoVideoOffer`: default `f`, opt-in `f;h;q`.
  // This skip is federated-only.
  //
  // Long-term: patch rtc 0.9 SDP generator to emit per-RID
  // SSRCs (or upgrade rtc), then restore the injection here.
  mungeOfferSdp(sdp) {
    return sdp;
  },

  // Retry semantics: re-open with a FRESH session id via the full
  // openPeerDisplay path — re-offering on the same session id is not a
  // wire shape the peer's WebRtcPeer lifecycle supports. The attempt
  // counter lives in the module-scope `peerDisplayReconnectAttempts`
  // map keyed host|display because every retry replaces the connection
  // object. The local slot instead renegotiates in place (its
  // server-side DisplaySession survives). Budget/backoff/dead-end copy
  // are the shared DISPLAY_VIEWER_RETRY_* constants.
  retrySemantics: 'reopen-fresh-session',

  // Signaling transport: the daemon facade's api_peer_webrtc_signal
  // (dashboard-control tunnel with its HTTP twin) — see
  // PeerDisplayConnection._sendSignal / handlePeerWebRtcSignal. The
  // local slot signals over displayWebRtcSignal / legacy /ws frames.
  signalingLane: 'daemon-facade-peer-signal',

  // Container resolution: Station-aware multi-container — the pane DOM
  // is rebuilt on every daemons-list re-render and may live in the
  // daemons panel, the Station endpoint, or the Station fallback, so
  // every render re-resolves via stationPeerDisplayContainersForHost /
  // _resolveContainer. The local slot owns one fixed stage instead.
  containerResolution: 'station-aware-multi-container',

  // Clipboard sync: LOCAL ONLY today — federated clipboard is a
  // follow-up; this class wires no clipboard channel or paste hooks.
  clipboardSync: false,

  // Attach/annotation stream naming: `peer_<safeHost>_display_<id>` so
  // frame ids stay unique across hosts and never collide with local
  // `display_<id>` streams.
  streamBase(conn) {
    const safeHost = String(conn.hostId).replace(/[^a-zA-Z0-9_.-]/g, '_');
    return `peer_${safeHost}_display_${conn.displayId}`;
  },
};

class PeerDisplayConnection {
  constructor(hostId, displayId, sessionId, advertiseTcpViaUrl) {
    this.hostId = hostId;
    this.displayId = displayId;
    this.sessionId = sessionId;
    // URL the browser uses to reach the peer's HTTP port. Included
    // in the Offer signal as `advertise_tcp_via_url` so the peer can
    // advertise a matching ICE-TCP candidate (slice 3a.2). Empty
    // string → UDP-only path (3a baseline).
    this.advertiseTcpViaUrl = advertiseTcpViaUrl || '';
    this.pc = null;
    this.stream = null;
    this._pendingCandidates = [];
    this._answerApplied = false;
    // F-1.3c: federated input authority state for THIS browser's
    // view of THIS peer-display. Mirrors local DisplaySlot's
    // `authorityState` semantics ('unknown' | 'you' | 'other' |
    // 'unclaimed') but rendered into the peer-display panel via
    // [`_renderAuthorityChip`]. Source of truth is the peer's
    // server-side gate; F-1 only renders the chip + Take/Release
    // buttons. F-2 wires actual input on top of this same state.
    this.peerAuthorityState = 'unknown';
    // True after `takeControl()` clicks Request and is waiting for
    // the peer's `'you'` confirmation; cleared on arrival in
    // `setPeerAuthorityState('you')`. F-1 doesn't enter "interactive
    // mode" the way local DisplaySlot does — that's F-2's job — so
    // this flag is currently used only as a debug signal in logs
    // and as the contract for how the chip transitions from "Take
    // pressed" to "you holds it." Kept here so F-2's input wiring
    // can reuse it without changing the F-1 wire surface.
    this._takeControlPending = false;
    // The browser-created `display_input_authority` data channel.
    // Created BEFORE `createOffer()` in [`Self::connect`] so the
    // channel appears in the SDP and the peer's
    // `OnDataChannel(OnOpen)` handler registers the label. Used
    // for both directions: outbound `display_input_authority_request` /
    // `_release` from Take/Release clicks, inbound
    // `display_input_authority_state` from the peer's per-subscriber
    // fanout task.
    this.authorityChannel = null;
    // F-2: input data channels. Same wire format as the local
    // DisplaySlot path — raw `InputEvent` JSON ({t:'kd', ...},
    // {t:'mm', ...}, etc.) so the peer's existing
    // `display/webrtc.rs::handle_message` parser dispatches them
    // through one input handler regardless of whether they came from
    // a local browser's WS or a federated peer's WebRTC. F-2's
    // server-side gate
    // ([`build_federated_input_authorizer`]) drops events that
    // arrive without a matching federated holder.
    //
    // `control` is ordered + reliable (key events, mouse buttons —
    // can't tolerate reorder or loss); `pointer` is unreliable +
    // unordered (mouse-move, scroll — high-rate, latest-wins
    // semantics, drop is preferable to head-of-line block).
    this.controlChannel = null;
    this.pointerChannel = null;
    // D-3b: tile-stream data channels. D-3b only negotiates the
    // channels and parses inbound binary frames; D-3c creates the
    // real compositor for peer desktop tiles.
    this.tileControlChannel = null;
    this.tileSnapshotChannel = null;
    this.tileDeltasChannel = null;
    this.tileCompositor = null;
    // F-2: interactive mode flag. True after `_enterInteractive`
    // installs the pointer / keyboard listeners on the peer-display
    // video element; cleared by `_exitInteractive`. Only flips to
    // true when `peerAuthorityState === 'you'`. The peer-side gate
    // is the security boundary; this flag is UX consistency only.
    this.interactive = false;
    this._boundHandlers = {};
    // Item 3: every held key code (not just modifiers) — see the local
    // DisplaySlot's `_heldKeys` for the latched-key rationale.
    this._heldKeys = new Set();
    // Set by `_enterInteractive`; lets `_exitInteractive` /
    // `releaseControl` flush synthetic keyups while the send closure
    // still exists (and, for release, while we still hold authority).
    this._flushHeldKeys = null;
    // Item 2: bounded auto-retry state. The attempt COUNTER lives in the
    // module-level `peerDisplayReconnectAttempts` map keyed by
    // host|display, because each retry replaces this object with a fresh
    // PeerDisplayConnection (fresh session id) via the full open path.
    this._retryTimer = null;
    // Item 5: in-stage status overlay state, persisted across the
    // daemons-list re-renders that rebuild the pane DOM.
    this._overlay = null;
    this._firstFrameSeen = false;
    // Item 6: live metrics chip sampler.
    this._statsTimer = null;
    this._statsPrev = null;
    this._metricsText = '';
    // Item 7a: revert timer for the transient "Stream fell back to …" note.
    this._fallbackNoteTimer = null;
    // Item 7b: 5s no-answer timeout for Take Control.
    this._takePendingTimer = null;
    // **Phase 0 visual-freshness sampler** (task #83). `null` unless
    // the dashboard URL is opened with `?diag=1`. Activated in
    // `ontrack` once the video stream is attached (so videoWidth /
    // videoHeight report real numbers) and torn down by `close()`
    // alongside the rest of the per-connection state.
    this._diagSampler = null;
    this.displayStatusText = 'Connecting...';
    this.displayStatusKind = '';
    // No-track watchdog: a viewed display that never delivers a video
    // track (peer dropped the offer — e.g. no capture grant — or ICE
    // stalled) previously sat in 'Offer sent…' forever while the Station
    // thumbnail showed 'linking display' indefinitely. Armed in
    // connect(), cleared by ontrack and close(); a retry click opens a
    // fresh connection, which arms a fresh watchdog.
    this._noTrackTimer = null;
  }

  // Same budget as DisplaySlot.NO_TRACK_TIMEOUT_MS — both are the shared
  // viewer-core constant, so the two paths' patience can't drift. The
  // static stays public (QA overrides keep working: the watchdog arms
  // with the static, not the constant).
  static NO_TRACK_TIMEOUT_MS = DISPLAY_VIEWER_NO_TRACK_TIMEOUT_MS;

  _armNoTrackWatchdog() {
    displayViewerArmNoTrackWatchdog(this, () => {
      if (this.stream) return;
      const message = 'peer did not answer — its display may need a capture grant';
      this._log('warn', `no video track within ${PeerDisplayConnection.NO_TRACK_TIMEOUT_MS}ms — ${message}`);
      this.setStatus(message, 'error');
      // Item 5: the watchdog verdict belongs on the stage too, with a
      // Retry that re-runs the open path.
      this._setStageOverlay('error', `No video within ${Math.round(PeerDisplayConnection.NO_TRACK_TIMEOUT_MS / 1000)}s — ${message}.`, true);
      stationPublishActivityEvent({
        id: `peer-display-timeout:${this.hostId}:${this.displayId}:${this.sessionId}`,
        hostId: this.hostId,
        level: 'warn',
        source: 'display',
        msg: `Display ${this.displayId} on ${stationHostLabel(this.hostId)} sent no video within ${Math.round(PeerDisplayConnection.NO_TRACK_TIMEOUT_MS / 1000)}s — ${message}`,
      });
      // Drop the Station source so the HUD stops drawing a 'linking
      // display' thumbnail for a stream that will never arrive.
      stationUnregisterVideoSource(`peer:${this.hostId}:${this.displayId}:${this.sessionId}`);
      stationScheduleUpdate();
    }, PeerDisplayConnection.NO_TRACK_TIMEOUT_MS);
  }

  _clearNoTrackWatchdog() {
    displayViewerClearNoTrackWatchdog(this);
  }

  sessionKey() {
    return `${this.hostId}|${this.displayId}|${this.sessionId}`;
  }

  // Attach to the DOM. Called on initial open AND after each
  // renderDaemonsList re-render so the live MediaStream (held on
  // `this.stream`) stays connected to the user-visible <video>.
  //
  // F-1.3c: the controls row also carries the federated input
  // authority chip + Take Control / Release buttons. On re-render,
  // the freshly-built pane needs to reflect the LATEST
  // `peerAuthorityState`, not the default 'unknown' — so this
  // function calls `_renderAuthorityChip()` after binding handlers.
  // Resolve the container that hosts (or should host) this connection's
  // pane: the Station endpoint when the Station tab is active, else the
  // daemons-list panel, else the Station fallback. This is the single
  // resolution rule — `ontrack` and `_handleTileWireMessage` reuse it so
  // Station-hosted panes get the same treatment as list-hosted ones
  // (they used to query only `peer-display-${hostId}` and dead-ended on
  // Station). `createFallback: false` makes it a pure query.
  _resolveContainer(createFallback = true) {
    const preferStation = stationPeerDisplayPrefersStationEndpoint();
    return (preferStation && stationPeerDisplayContainer(this.hostId, createFallback, true))
      || document.getElementById(`peer-display-${this.hostId}`)
      || stationPeerDisplayContainer(this.hostId, preferStation && createFallback);
  }

  attachToDom() {
    const container = this._resolveContainer();
    if (!container) return;
    if (!container.querySelector('.peer-display-pane')) {
      container.innerHTML = `
        <div class="peer-display-pane" data-host-id="${escapeHtml(this.hostId)}">
          <video class="peer-display-video" autoplay playsinline muted></video>
          <div class="display-stage-overlay" style="display:none"></div>
          <div class="display-live-metrics" style="display:none"></div>
          <div class="peer-display-controls">
            <span class="peer-display-status">${escapeHtml(this.displayStatusText)}</span>
            <span class="peer-display-authority display-input-authority"
                  data-host-id="${escapeHtml(this.hostId)}"
                  style="display:none"
                  title="Federated input authority for this peer-display: who can drive keyboard/mouse."></span>
            <button class="take-control-btn"
                    data-host-id="${escapeHtml(this.hostId)}"
                    title="Take federated input control of this peer's display">Take Control</button>
            <button class="release-control-btn"
                    data-host-id="${escapeHtml(this.hostId)}"
                    style="display:none"
                    title="Release federated input control and return to view-only">Release</button>
            <button class="ann-attach-btn peer-display-attach"
                    data-host-id="${escapeHtml(this.hostId)}"
                    title="Capture current frame and attach to next task">Attach</button>
            <button class="annotate-btn peer-display-annotate"
                    data-host-id="${escapeHtml(this.hostId)}"
                    title="Freeze current frame and annotate it">&#9998; Annotate</button>
            <button class="callout-btn peer-display-callout"
                    data-host-id="${escapeHtml(this.hostId)}"
                    aria-pressed="false"
                    disabled
                    title="Call out a region: arm, then drag a rectangle on the frame to attach it to the next task (needs input control)">&#x2316; Callout</button>
            <button class="peer-display-close" data-host-id="${escapeHtml(this.hostId)}">Close</button>
          </div>
        </div>`;
      const closeBtn = container.querySelector('.peer-display-close');
      if (closeBtn) {
        closeBtn.addEventListener('click', () =>
          closePeerDisplaysForHost(this.hostId)
        );
      }
      const takeBtn = container.querySelector('.take-control-btn');
      if (takeBtn) {
        takeBtn.addEventListener('click', () => this.takeControl());
      }
      const releaseBtn = container.querySelector('.release-control-btn');
      if (releaseBtn) {
        releaseBtn.addEventListener('click', () => this.releaseControl());
      }
      const attachBtn = container.querySelector('.peer-display-attach');
      if (attachBtn) {
        attachBtn.addEventListener('click', () => this.attachCurrentFrame(attachBtn));
      }
      const annotateBtn = container.querySelector('.peer-display-annotate');
      if (annotateBtn) {
        annotateBtn.addEventListener('click', () => this.annotateCurrentFrame());
      }
      const calloutBtn = container.querySelector('.peer-display-callout');
      if (calloutBtn) {
        calloutBtn.addEventListener('click', () => this.toggleCallout(calloutBtn));
      }
    }
    container.style.display = 'block';
    const videoEl = container.querySelector('.peer-display-video');
    if (videoEl && this.stream) {
      videoEl.srcObject = this.stream;
      // autoplay doesn't re-fire when a stream is (re)attached to an
      // element living in the offscreen endpoint container; without an
      // explicit play() the pane sits paused on a black first frame.
      videoEl.play().catch(() => {});
    }
    stationRegisterVideoSource(
      `peer:${this.hostId}:${this.displayId}:${this.sessionId}`,
      this.hostId,
      String(this.displayId),
      `${stationHostLabel(this.hostId)} :${this.displayId}`,
      'peer',
      videoEl,
    );
    stationScheduleUpdate();
    // F-1.3c: re-render the chip from the latest state so a daemons-
    // list re-render that destroyed the prior pane doesn't reset the
    // chip to 'unknown' — the live `peerAuthorityState` survives the
    // DOM swap on the connection object. Same for the stage overlay and
    // the live metrics chip, whose state also lives on the connection.
    this._renderAuthorityChip();
    this._applyStageOverlayDom();
    this._applyMetricsDom(this._metricsText);
    // A pane rebuild wiped any live-annotation editor / armed-callout
    // overlays hosted in the previous pane DOM — end those modes if
    // their overlays actually died (47-annotation-clips owns the modes;
    // this is the provider-level lifecycle hook for pane rebuilds).
    reconcileLiveSurfaceForOwner(this);
    // F-2 lifecycle: if the panel was rebuilt while we still hold
    // input authority, our `_boundHandlers` are bound to the
    // now-detached prior video element. Force-clear interactive
    // state and re-enter against the freshly-attached video so
    // input keeps flowing across daemons-list re-renders.
    //
    // We can't just call `_exitInteractive()` then
    // `_enterInteractive()` because exit's `removeEventListener`
    // would target the old DOM node (harmless but pointless), and
    // exit guards on `this.interactive` so it'd be a no-op anyway
    // if the bind failed earlier (see the no-video bail-out doc).
    // Direct field reset is the honest accounting: the bindings to
    // the prior element are unrecoverable; let the new
    // `_enterInteractive` install fresh bindings on the current
    // video element.
    if (this.peerAuthorityState === 'you') {
      // Flush held keys through the still-open channels before dropping
      // the stale bindings — the destroyed pane never fired blur.
      if (this._flushHeldKeys) {
        try { this._flushHeldKeys(); } catch (_) {}
        this._flushHeldKeys = null;
      }
      this.interactive = false;
      this._boundHandlers = {};
      if (this._heldKeys) this._heldKeys.clear();
      this._enterInteractive();
    }
  }

  setStatus(text, kind) {
    this.displayStatusText = text || '';
    this.displayStatusKind = kind || '';
    for (const container of stationPeerDisplayContainersForHost(this.hostId)) {
      const statusEl = container.querySelector('.peer-display-status');
      if (statusEl) {
        statusEl.textContent = this.displayStatusText;
        statusEl.className = `peer-display-status ${this.displayStatusKind}`;
      }
    }
  }

  // ── Item 5: in-stage status overlay (peer pane) ─────────────────────
  // State lives on the connection (`this._overlay`) and is re-applied
  // into every pane the host currently has (daemons list + Station
  // endpoint) — the pane DOM is rebuilt on every daemons-list re-render.
  _setStageOverlay(mode, text, retry = false) {
    this._overlay = mode ? { mode, text: text || '', retry: !!retry } : null;
    this._applyStageOverlayDom();
  }

  _applyStageOverlayDom() {
    // Shared DOM builder (45-display-viewer-core); the per-container
    // fanout is this class's container-resolution policy, and the retry
    // affordance is always the 'Retry' button re-running the full open
    // path (see the retry() doc — re-offering in place is not a wire
    // shape the peer's WebRtcPeer lifecycle supports).
    for (const container of stationPeerDisplayContainersForHost(this.hostId)) {
      const el = container.querySelector('.display-stage-overlay');
      if (!el) continue;
      displayViewerRenderStageOverlayInto(el, this._overlay ? {
        mode: this._overlay.mode,
        text: this._overlay.text,
        retryLabel: this._overlay.retry ? 'Retry' : null,
        onRetry: this._overlay.retry ? () => this.retry() : null,
      } : null);
    }
  }

  // ── Item 2: retry / bounded auto-reconnect ──────────────────────────
  // A retry re-runs the FULL open path (fresh session id, fresh
  // PeerDisplayConnection) — re-offering on the same session id is not a
  // wire shape the peer's WebRtcPeer lifecycle supports. Manual retry
  // (button) resets the attempt budget; auto-retry consumes it.
  retry() {
    peerDisplayReconnectAttempts.delete(`${this.hostId}|${this.displayId}`);
    openPeerDisplay(this.hostId, this.displayId, this.advertiseTcpViaUrl)
      .catch(err => this._log('warn', `manual retry failed: ${err.message}`));
  }

  _scheduleAutoRetry() {
    const key = `${this.hostId}|${this.displayId}`;
    const attempts = (peerDisplayReconnectAttempts.get(key) || 0) + 1;
    peerDisplayReconnectAttempts.set(key, attempts);
    if (attempts > DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS) {
      this.setStatus(DISPLAY_VIEWER_RETRY_DEAD_END_STATUS, 'error');
      this._setStageOverlay('error', DISPLAY_VIEWER_RETRY_DEAD_END_OVERLAY, true);
      return;
    }
    const delay = displayViewerRetryDelayMs(attempts);
    this.setStatus(`Connection failed — retrying in ${delay / 1000}s (attempt ${attempts} of ${DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS})…`, 'error');
    this._setStageOverlay('progress', `Connection failed — retrying in ${delay / 1000}s (attempt ${attempts} of ${DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS})…`);
    this._log('warn', `connection failed — auto-retry ${attempts}/${DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS} in ${delay}ms`);
    if (this._retryTimer) window.clearTimeout(this._retryTimer);
    this._retryTimer = window.setTimeout(() => {
      this._retryTimer = null;
      // Bail if the user closed the pane (close() deregisters us) or a
      // manual retry already replaced this connection.
      if (peerDisplayConnections.get(this.sessionKey()) !== this) return;
      openPeerDisplay(this.hostId, this.displayId, this.advertiseTcpViaUrl)
        .catch(err => this._log('warn', `auto-retry open failed: ${err.message}`));
    }, delay);
  }

  // ── Item 6: live metrics chip (getStats sampler) ────────────────────
  // Shared cadence + summarizer with the local DisplaySlot chip
  // (45-display-viewer-core); only where the text lands is ours — the
  // per-container fanout below.
  _startStatsSampler() {
    displayViewerStartStatsSampler(this);
  }

  _stopStatsSampler() {
    displayViewerStopStatsSampler(this);
    this._applyMetricsDom('');
  }

  async _sampleStats() {
    await displayViewerSampleRtcStats(this, (text) => this._applyMetricsDom(text));
  }

  _applyMetricsDom(text) {
    this._metricsText = text || '';
    for (const container of stationPeerDisplayContainersForHost(this.hostId)) {
      const el = container.querySelector('.display-live-metrics');
      if (!el) continue;
      el.textContent = this._metricsText;
      el.style.display = this._metricsText ? '' : 'none';
      el.classList.toggle('active', !!this._metricsText);
    }
  }

  // ── Item 11: capture + attach (peer parity with the local Attach) ───
  // Rasterizes whichever surface is live (tile canvas or video) and
  // queues it as a pending attachment through the same transport-
  // abstracted annotation lane the local slot uses.
  captureCurrentFrame(quality = 0.85) {
    const surface = this._interactiveSurfaceCandidate();
    if (!surface) return null;
    const isCanvas = typeof HTMLCanvasElement !== 'undefined' && surface instanceof HTMLCanvasElement;
    const w = isCanvas ? surface.width : surface.videoWidth;
    const h = isCanvas ? surface.height : surface.videoHeight;
    if (!w || !h) return null;
    return displayViewerRasterizeSurface(surface, w, h, quality);
  }

  // The frame-id scheme and upload live in the shared attach lane
  // (45-display-viewer-core); the stream name is the FEDERATED policy
  // (`peer_<safeHost>_display_<id>`) so ids stay unique across hosts.
  async attachCurrentFrame(btn = null) {
    const frame = this.captureCurrentFrame(0.85);
    if (!frame) {
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'No frame available from this peer display yet');
      }
      return;
    }
    if (!(await displayViewerUploadAttachFrame(this, PEER_DISPLAY_POLICY.streamBase(this), frame))) {
      return;
    }
    if (btn) {
      const orig = btn.textContent;
      btn.textContent = '✓ Attached';
      window.setTimeout(() => { btn.textContent = orig; }, 1500);
    }
  }

  // ── Annotate + Callout (peer parity with the local display slot) ────
  // The stage a live-annotation edit or callout arm anchors to: the
  // pane hosting whichever surface is live (tile canvas or video).
  _annotationStageEl() {
    const surface = this._interactiveSurfaceCandidate();
    const pane = surface && surface.closest ? surface.closest('.peer-display-pane') : null;
    if (pane) return pane;
    for (const container of this._containerCandidates()) {
      const p = container.querySelector('.peer-display-pane');
      if (p) return p;
    }
    return null;
  }

  // The surface-provider contract consumed by 47-annotation-clips'
  // live-annotation editor + callout arming — the field-by-field
  // enumeration lives above setLiveAnnotationButton there. streamBase
  // follows the peer attach-lane convention (`peer_<host>_display_<id>`)
  // so frame ids stay unique across hosts and never collide with local
  // `display_<id>` streams.
  _annotationSurfaceProvider() {
    return {
      owner: this,
      displayId: this.displayId,
      streamBase: PEER_DISPLAY_POLICY.streamBase(this),
      stageEl: () => this._annotationStageEl(),
      liveSurfaceEl: () => this._interactiveSurfaceCandidate(),
      annotateBtn: () => {
        const stage = this._annotationStageEl();
        return stage ? stage.querySelector('.peer-display-annotate') : null;
      },
      toolbarHostEl: () => this._annotationStageEl(),
    };
  }

  annotateCurrentFrame() {
    const frame = this.captureCurrentFrame(0.92);
    if (!frame) {
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'No frame available from this peer display yet');
      }
      return;
    }
    enterLiveAnnotationMode(this._annotationSurfaceProvider(), frame);
  }

  // Toolbar-armed Callout: one-shot region flag shipped through the
  // annotation-attach lane (shared wiring in 45-display-viewer-core;
  // machinery in 47-annotation-clips). Armable only while federated
  // input authority is 'you' (button disabled otherwise, disarmed on
  // authority loss).
  toggleCallout(btn) {
    displayViewerToggleCallout(this, btn || null);
  }

  async connect() {
    // Item 5: stage the time-to-first-frame copy where the user is
    // looking (the pane), not just the small status text.
    this._firstFrameSeen = false;
    this._setStageOverlay('progress', 'Negotiating…');
    // ICE config + TURN relay pinning: PEER_DISPLAY_POLICY.buildRtcConfig
    // (#41–#45 — FEDERATED-ONLY; the local slot never pins relay). Full
    // rationale on the policy field.
    this.pc = new RTCPeerConnection(
      PEER_DISPLAY_POLICY.buildRtcConfig((level, message) => this._log(level, message)));
    const videoTransceiver = this.pc.addTransceiver('video', { direction: 'recvonly' });

    // Codec preference: **#67** VP8 pin (opt-in H.264-first via
    // `[webrtc].federation_allow_h264` / the per-session override) — the
    // FEDERATED policy, opposite of the local slot's #58 browser-default
    // order. Full rationale on PEER_DISPLAY_POLICY.applyCodecPreferences.
    PEER_DISPLAY_POLICY.applyCodecPreferences(
      videoTransceiver, (level, message) => this._log(level, message));
    this._log('debug', `connect: sessionKey=${this.sessionKey()} advertiseTcpViaUrl=${this.advertiseTcpViaUrl || '(none)'}`);

    // F-1.3c: federated authority data channel. MUST be created
    // BEFORE `createOffer()` so the channel ends up in the SDP and
    // the peer's `OnDataChannel(OnOpen)` handler registers the label
    // when the answer applies. Without this, the peer's
    // `display_input_authority_state` send path
    // (`Command::SendAuthorityState`) queues forever in
    // F-1.2's pending_authority_state buffer and the chip stays at
    // `unknown` indefinitely.
    //
    // Channel name matches `AUTHORITY_CHANNEL_LABEL` on the peer
    // side (see `display/webrtc.rs::AUTHORITY_CHANNEL_LABEL`). Wire
    // format pinned by `parse_authority_channel_message_round_trip`:
    //   { "t": "display_input_authority_request", "display_id": N }
    //   { "t": "display_input_authority_release", "display_id": N }
    //   { "t": "display_input_authority_state",   "display_id": N, "state": "you|other|unclaimed" }
    //
    // Default ordered+reliable — authority state must not be
    // reordered (a `you → other → you` flap arriving as
    // `you → you → other` would leave the chip on the wrong state).
    this.authorityChannel = this.pc.createDataChannel('display_input_authority');
    this.authorityChannel.onopen = () => {
      this._log('info', 'authority data channel open');
    };
    this.authorityChannel.onclose = () => {
      this._log('debug', 'authority data channel closed');
    };
    this.authorityChannel.onmessage = (e) => {
      let frame;
      try {
        frame = JSON.parse(e.data);
      } catch (err) {
        this._log('warn', `authority frame JSON parse failed: ${err.message}`);
        return;
      }
      if (frame && frame.t === 'display_input_authority_state'
          && typeof frame.display_id === 'number'
          && frame.display_id === this.displayId
          && typeof frame.state === 'string') {
        this.setPeerAuthorityState(frame.state);
      } else {
        this._log('debug', `unhandled authority frame: ${e.data && e.data.slice(0, 80)}`);
      }
    };

    // F-2: input data channels. Same labels + reliability semantics
    // as the local DisplaySlot path so the peer's existing
    // `handle_message` parser dispatches both transports through
    // one input handler. Created BEFORE `createOffer()` so the
    // channels appear in the SDP. Channels open after the peer's
    // answer; the input listeners installed by `_enterInteractive`
    // gate-check `readyState === 'open'` before sending.
    this.controlChannel = this.pc.createDataChannel('control', { ordered: true });
    this.pointerChannel = this.pc.createDataChannel('pointer', {
      ordered: false,
      maxRetransmits: 0,
    });
    this.controlChannel.onopen = () =>
      this._log('info', 'control data channel open');
    this.pointerChannel.onopen = () =>
      this._log('info', 'pointer data channel open');

    // D-3b: tile-stream channels. The browser creates them before
    // `createOffer()` so the peer can passively observe and write to
    // them by label. D-3b does not enable real tile rendering yet;
    // `_handleTileWireMessage` accepts frames so D-3c can attach the
    // compositor without changing negotiation.
    this.tileControlChannel = this.pc.createDataChannel('tile-control', { ordered: true });
    this.tileSnapshotChannel = this.pc.createDataChannel('tile-snapshot', { ordered: true });
    this.tileDeltasChannel = this.pc.createDataChannel('tile-deltas', {
      ordered: false,
      maxRetransmits: 0,
    });
    for (const [label, channel] of [
      ['tile-control', this.tileControlChannel],
      ['tile-snapshot', this.tileSnapshotChannel],
      ['tile-deltas', this.tileDeltasChannel],
    ]) {
      channel.binaryType = 'arraybuffer';
      channel.onopen = () => {
        this._log('info', `${label} data channel open`);
        if (label === 'tile-control') {
          const clientId = Math.floor(Math.random() * 0xffffffff) >>> 0;
          channel.send(encodeTileSubscribeFrame(clientId));
        }
      };
      channel.onclose = () => this._log('debug', `${label} data channel closed`);
      channel.onmessage = (e) => this._handleTileWireMessage(label, e);
    }

    this.pc.ontrack = (e) => {
      this._clearNoTrackWatchdog();
      this.stream = e.streams[0];
      const container = this._resolveContainer();
      const videoEl = container && container.querySelector('.peer-display-video');
      if (videoEl) {
        videoEl.srcObject = this.stream;
        // See the reapply path above: explicit play() because autoplay
        // doesn't re-fire on (re)attached offscreen elements.
        videoEl.play().catch(() => {});
      }
      // Item 5: track arrived, frames haven't — stage the last step and
      // drop the overlay on the first actually-rendered frame.
      if (!this._firstFrameSeen) {
        this._setStageOverlay('progress', 'Waiting for first frame…');
      }
      // Shared first-frame cascade (45-display-viewer-core); `!this.pc`
      // is this class's staleness guard — closed before the first frame.
      displayViewerOnFirstFrame(videoEl, () => !this.pc, () => {
        this._firstFrameSeen = true;
        this._setStageOverlay(null);
      });
      if (videoEl) {
        stationRegisterVideoSource(
          `peer:${this.hostId}:${this.displayId}:${this.sessionId}`,
          this.hostId,
          String(this.displayId),
          `${stationHostLabel(this.hostId)} :${this.displayId}`,
          'peer',
          videoEl,
        );
      }
      stationScheduleUpdate();
      this.setStatus('Connected (view-only)', 'connected');
      this._log('info', 'ontrack fired — video stream attached');

      // Phase 0 visual-freshness sampler. Wait one tick for videoWidth /
      // videoHeight to populate (rVFC fires once the first frame is
      // actually decoded; videoWidth is reliably non-zero by then). On
      // browsers without rVFC the sampler tolerates 0x0 dims and skips
      // until the next `requestAnimationFrame` finds real numbers.
      if (videoEl && diagModeEnabled() && !this._diagSampler) {
        this._diagSampler = new VisualFreshnessSampler(
          videoEl, this.hostId, this.displayId,
        );
        this._diagSampler.start();
        this._log(
          'info',
          `[diag-vf] sampler started (browser_session_id=${this._diagSampler.browserSessionId}); ` +
          `confirm peer marker is enabled via /ws set_diagnostics_visual_marker`,
        );
      }
    };

    this.pc.onicecandidate = (e) => {
      if (e.candidate) {
        this._log('debug', `local ICE candidate: ${this._describeCandidate(e.candidate)}`);
        this._sendSignal({
          kind: 'ice_candidate',
          candidate_json: JSON.stringify(e.candidate.toJSON()),
        }).catch(err => this._log('warn', `forwarding local ICE candidate failed: ${err.message}`));
      } else {
        this._log('debug', 'local ICE gathering complete (null candidate)');
      }
    };

    this.pc.oniceconnectionstatechange = async () => {
      if (!this.pc) return;
      const state = this.pc.iceConnectionState;
      this._log('debug', `iceConnectionState=${state}`);
      // When ICE pairs, log the selected pair so future smoke tests
      // can distinguish which ICE path won — direct host UDP vs the
      // primary's TCP relay candidate (slice-3a.2 at the peer's
      // browser_tcp_via_url) vs an actual TURN relay (when a real
      // TURN server is configured in [webrtc].ice_servers). Without
      // this, "connection works" doesn't tell you whether you're on
      // the cheap direct path or paying TURN bandwidth costs.
      if (state === 'connected' || state === 'completed') {
        try {
          const stats = await this.pc.getStats();
          const candidates = new Map();
          let pair = null;
          stats.forEach((r) => {
            if (r.type === 'local-candidate' || r.type === 'remote-candidate') {
              candidates.set(r.id, r);
            } else if (
              r.type === 'candidate-pair' &&
              (r.selected || (r.nominated && r.state === 'succeeded'))
            ) {
              pair = r;
            }
          });
          const fmt = (c) =>
            c
              ? `${c.candidateType || '?'} ${c.protocol || '?'} ` +
                `${c.address || c.ip || '?'}:${c.port}`
              : '?';
          if (pair) {
            const local = candidates.get(pair.localCandidateId);
            const remote = candidates.get(pair.remoteCandidateId);
            this._log(
              'info',
              `selected candidate pair: ` +
                `local=[${fmt(local)}] remote=[${fmt(remote)}] ` +
                `state=${pair.state} nominated=${pair.nominated}`
            );
          } else {
            this._log('debug', 'no selected candidate-pair in getStats() yet');
          }
        } catch (err) {
          this._log('warn', `getStats() failed: ${err.message}`);
        }
      }
    };

    this.pc.onconnectionstatechange = () => {
      if (!this.pc) return;
      const state = this.pc.connectionState;
      this._log('debug', `connectionState=${state}`);
      if (state === 'connected') {
        peerDisplayReconnectAttempts.delete(`${this.hostId}|${this.displayId}`);
        this.setStatus('Connected (view-only)', 'connected');
        this._startStatsSampler();
      } else if (state === 'failed') {
        // Item 2: port of the local retry policy — bounded attempts with
        // backoff, then a dead-end state that offers a Retry button
        // (previously 'failed' was terminal with only Close available).
        this._stopStatsSampler();
        this._scheduleAutoRetry();
      } else if (state === 'disconnected') {
        // Transient more often than not — matches the local slot's copy.
        // If it hardens into 'failed', the auto-retry above kicks in.
        this._stopStatsSampler();
        this.setStatus('Connection interrupted — recovering…', 'warn');
      }
    };

    try {
      // Simulcast: **#46** — the FEDERATED munge policy is the identity
      // (NO recv-simulcast injection; rtc 0.9 answers the hint on a
      // single-encoding track with a malformed multi-RID/single-SSRC
      // shape the browser silently refuses to decode). Full rationale +
      // the restore condition on PEER_DISPLAY_POLICY.mungeOfferSdp; the
      // local slot's opposite policy is DISPLAY_SLOT_POLICY.mungeOfferSdp.
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription({
        type: offer.type,
        sdp: PEER_DISPLAY_POLICY.mungeOfferSdp(offer.sdp),
      });
      const offerSignal = {
        kind: 'offer',
        sdp: this.pc.localDescription.sdp,
      };
      // Only attach the URL hint when we actually have one. The
      // server's serde setup uses `skip_serializing_if = None` on
      // the field, but on the browser side we enforce the same
      // invariant here so the wire frame stays minimal and older
      // peers don't see an unexpected field.
      if (this.advertiseTcpViaUrl) {
        offerSignal.advertise_tcp_via_url = this.advertiseTcpViaUrl;
      }
      await this._sendSignal(offerSignal);
      this._log('info', `offer sent (sdp_len=${offerSignal.sdp.length})`);
      this.setStatus('Offer sent, awaiting answer…', '');
      this._armNoTrackWatchdog();
    } catch (err) {
      this._log('error', `offer failed: ${err.message}`);
      this.setStatus(`Offer failed: ${err.message}`, 'error');
      // Item 5: offer failure was a quiet status line — surface it on
      // the stage with a way out.
      this._setStageOverlay('error', `Offer failed: ${err.message}`, true);
    }
  }

  handleAnswer(sdp) {
    if (!this.pc) {
      this._log('warn', 'handleAnswer called but pc is null — ignored');
      return;
    }
    this._log('info', `answer received (sdp_len=${sdp.length})`);
    displayViewerApplyRemoteAnswer(this, sdp, {
      beforeFlush: (count) =>
        this._log('debug', `answer applied, flushing ${count} buffered ICE candidate(s)`),
      onFlushCandidateError: (err) =>
        this._log('warn', `buffered addIceCandidate failed: ${err.message}`),
      afterFlush: () => {
        if (!this._firstFrameSeen && !this.stream) {
          this._setStageOverlay('progress', 'Waiting for first frame…');
        }
      },
      onError: (err) => {
        this._log('error', `setRemoteDescription(answer) failed: ${err.message}`);
        this.setStatus(`Answer failed: ${err.message}`, 'error');
        this._setStageOverlay('error', `Answer failed: ${err.message}`, true);
      },
    });
  }

  handleIceCandidate(candidateJson) {
    if (!candidateJson || !this.pc) {
      this._log('warn', 'handleIceCandidate called with empty payload or no pc — ignored');
      return;
    }
    let candidate;
    try {
      candidate = JSON.parse(candidateJson);
    } catch (err) {
      this._log('warn', `remote ICE candidate JSON parse failed: ${err.message}`);
      return;
    }
    // Buffer candidates that arrive before the answer is applied (shared
    // scaffold). Mirrors the local display flow; the peer-side ICE
    // forwarder can produce candidates as soon as handle_offer returns.
    displayViewerIngestRemoteIceCandidate(this, candidate, {
      onQueued: (c) =>
        this._log('debug', `buffering remote ICE candidate (answer not yet applied): ${this._describeCandidate(c)}`),
      onAdd: (c) =>
        this._log('debug', `remote ICE candidate: ${this._describeCandidate(c)}`),
      onAddError: (err) =>
        this._log('warn', `addIceCandidate failed: ${err.message}`),
    });
  }

  async _handleTileWireMessage(label, event) {
    try {
      let data = event.data;
      if (data instanceof Blob) {
        data = await data.arrayBuffer();
      }
      if (!this.tileCompositor) {
        const parsed = parseTileWireFrame(data);
        if (parsed.type !== 'snapshot_chunk') {
          this._log('debug', `${label} tile frame before snapshot: ${parsed.type}`);
          return;
        }
        // Item 4: resolve the live container (Station endpoint included)
        // — the getElementById-only lookup dead-ended tile streams on
        // Station-hosted panes.
        const container = this._resolveContainer();
        if (!container) return;
        this.tileCompositor = new TileCompositor(
          container.querySelector('.peer-display-pane') || container,
          {
            tileSize: parsed.tile_size_px,
            gridW: parsed.grid_w_tiles,
            gridH: parsed.grid_h_tiles,
            sendControlFrame: (bytes) => this._sendTileControlFrame(bytes),
          },
        );
        // Tile mode's "first frame": the compositor exists and is about
        // to paint the snapshot — drop the connecting overlay.
        this._firstFrameSeen = true;
        this._setStageOverlay(null);
        if (diagModeEnabled()) {
          if (this._diagSampler) {
            try { this._diagSampler.stop(); } catch (e) {
              this._log('warn', `[diag-vf] video sampler stop before tile sampler failed: ${e.message}`);
            }
          }
          this._diagSampler = new CanvasFreshnessSampler(
            this.tileCompositor.canvas,
            this.hostId,
            this.displayId,
          );
          this._diagSampler.start();
          this._log(
            'info',
            `[diag-vf canvas] sampler started (browser_session_id=${this._diagSampler.browserSessionId})`,
          );
        }
        const video = container.querySelector('.peer-display-video');
        if (video) video.style.display = 'none';
      }
      const parsed = this.tileCompositor.onWireFrame(data);
      this._syncDiagSamplerForTileSurface(parsed);
      this._noteTileFallback(parsed);
      this._log('debug', `${label} tile frame applied: ${parsed.type}`);
    } catch (err) {
      this._log('warn', `${label} tile frame failed: ${err.message}`);
    }
  }

  // Item 7a: tile↔video fallback transitions used to swap surfaces
  // silently (the TileCompositor stays UI-framework-agnostic — it is
  // also driven by the synthetic tile-test harness — so the user-facing
  // note lives here, on the connection). Brief: revert to the steady
  // state copy after a few seconds.
  _noteTileFallback(parsed) {
    if (!parsed) return;
    if (parsed.type !== 'fallback_to_video' && parsed.type !== 'fallback_to_tile') return;
    const surface = parsed.type === 'fallback_to_video' ? 'video' : 'tiles';
    this.setStatus(`Stream fell back to ${surface}`, 'connected');
    if (this._fallbackNoteTimer) window.clearTimeout(this._fallbackNoteTimer);
    this._fallbackNoteTimer = window.setTimeout(() => {
      this._fallbackNoteTimer = null;
      if (this.pc && this.pc.connectionState === 'connected') {
        this.setStatus('Connected (view-only)', 'connected');
      }
    }, 4000);
  }

  _stopDiagSampler() {
    if (!this._diagSampler) return;
    try { this._diagSampler.stop(); } catch (e) {
      this._log('warn', `[diag-vf] sampler stop failed: ${e.message}`);
    }
    this._diagSampler = null;
  }

  // Item 4: ordered container candidates for read-only surface lookups —
  // the resolved (live) container first, then every other container this
  // host has. Never creates the Station fallback.
  _containerCandidates() {
    const candidates = [];
    const resolved = this._resolveContainer(false);
    if (resolved) candidates.push(resolved);
    for (const container of stationPeerDisplayContainersForHost(this.hostId)) {
      if (!candidates.includes(container)) candidates.push(container);
    }
    return candidates;
  }

  // The surface input listeners should bind on / captures rasterize from:
  // the live tile canvas when tile streaming is active, else the first
  // video element across the host's containers (Station panes included).
  _interactiveSurfaceCandidate() {
    if (this.tileCompositor && this.tileCompositor.canvas && this.tileCompositor.canvas.isConnected) {
      return this.tileCompositor.canvas;
    }
    for (const container of this._containerCandidates()) {
      const surface = container.querySelector('.tile-compositor-canvas')
        || container.querySelector('.peer-display-video');
      if (surface) return surface;
    }
    return null;
  }

  _startVideoDiagSampler() {
    if (!diagModeEnabled()) return;
    let videoEl = null;
    for (const container of this._containerCandidates()) {
      videoEl = container.querySelector('.peer-display-video');
      if (videoEl) break;
    }
    if (!videoEl) return;
    this._stopDiagSampler();
    this._diagSampler = new VisualFreshnessSampler(videoEl, this.hostId, this.displayId);
    this._diagSampler.start();
    this._log(
      'info',
      `[diag-vf video] sampler started (browser_session_id=${this._diagSampler.browserSessionId})`,
    );
  }

  _startCanvasDiagSampler() {
    if (!diagModeEnabled() || !this.tileCompositor) return;
    this._stopDiagSampler();
    this._diagSampler = new CanvasFreshnessSampler(
      this.tileCompositor.canvas,
      this.hostId,
      this.displayId,
    );
    this._diagSampler.start();
    this._log(
      'info',
      `[diag-vf canvas] sampler started (browser_session_id=${this._diagSampler.browserSessionId})`,
    );
  }

  _syncDiagSamplerForTileSurface(parsed) {
    if (!diagModeEnabled() || !parsed) return;
    if (parsed.type === 'fallback_to_video') {
      this._startVideoDiagSampler();
    } else if (parsed.type === 'fallback_to_tile') {
      this._startCanvasDiagSampler();
    }
  }

  _sendTileControlFrame(bytes) {
    if (!this.tileControlChannel || this.tileControlChannel.readyState !== 'open') {
      this._log('debug', 'tile-control frame dropped before channel open');
      return false;
    }
    this.tileControlChannel.send(bytes);
    return true;
  }

  // F-1.3c: peer-side authority state callback. Called from the
  // `authorityChannel.onmessage` parser with the resolved
  // `'you' | 'other' | 'unclaimed'` for THIS browser's
  // (federation_connection_id, session_id) — the peer personalizes
  // server-side, so this code never sees holder identities.
  //
  // Mirrors local DisplaySlot's `setAuthority` semantics. F-1
  // doesn't enter "interactive mode" (input wiring is F-2) so the
  // promotion-on-`'you'` arm just clears `_takeControlPending`
  // without installing pointer / keyboard listeners; F-2 will hook
  // its `_enterInteractive` equivalent in here.
  setPeerAuthorityState(state) {
    if (!isDisplayInputAuthorityState(state)) {
      // Forward-compat: an unknown state string leaves the chip
      // on its previous value rather than blanking it. Same
      // policy as DisplaySlot's setAuthority.
      this._log('debug', `unknown authority state '${state}' — keeping previous`);
      return;
    }
    this.peerAuthorityState = state;
    this._renderAuthorityChip();
    // Callout arming requires held input authority; losing it disarms.
    if (state !== 'you' && liveCalloutArmedFor(this)) {
      disarmLiveCallout();
    }
    // Item 7b: any authoritative state answers an in-flight take —
    // disarm the 5s no-answer timeout.
    if (this._takePendingTimer) {
      window.clearTimeout(this._takePendingTimer);
      this._takePendingTimer = null;
    }
    if (state === 'you' && this._takeControlPending) {
      this._takeControlPending = false;
      this._log('info', 'authority granted: you');
    } else if (state !== 'you') {
      // Demoted (or never had it): clear pending so a stale
      // 'you' arriving later doesn't promote us into a state the
      // user no longer wants.
      this._takeControlPending = false;
    }
    // F-2: enter / exit interactive mode based on the authoritative
    // state. Mirrors local DisplaySlot's `setAuthority` arms exactly:
    //   - state == 'you' AND not yet interactive → install listeners.
    //   - state != 'you' AND currently interactive → silently exit
    //     (the user didn't ask to leave; another browser took
    //     control or the peer revoked).
    if (state === 'you' && !this.interactive) {
      this._enterInteractive();
    } else if (this.interactive && state !== 'you') {
      this._exitInteractive();
    }
  }

  // F-2: install pointer / keyboard listeners on the visible peer-display
  // surface so user input flows over the `control` /
  // `pointer` data channels as raw `InputEvent` JSON. Mirrors local
  // DisplaySlot's `_enterInteractive` exactly except for what's
  // out-of-scope for F-2: no `take_display` ControlMsg (peer's
  // worker-agent coordination is future work), no clipboard
  // (federated clipboard is a follow-up).
  _enterInteractive() {
    if (this.interactive) return;
    // Item 4: bind on whichever surface is live — Station-hosted panes
    // included. The old getElementById-only lookup meant Take Control on
    // a Station pane silently installed no listeners.
    const target = this._interactiveSurfaceCandidate();
    if (!target) {
      // Pane was destroyed between authority grant and entry. Bail
      // WITHOUT flipping `interactive` so a subsequent
      // `attachToDom` (which calls back into _enterInteractive when
      // peerAuthorityState === 'you') can rebind cleanly. Earlier
      // versions set the flag here and got stuck — `setPeerAuthorityState`
      // would skip on the redundant `if (this.interactive) return`
      // branch and the rebuilt pane never got listeners.
      this._log('warn', 'enterInteractive: display surface missing — pane torn down?');
      return;
    }
    target.tabIndex = 0;
    target.focus();
    this._interactiveTarget = target;
    // Item 3: track EVERY held key code, not just the 8 modifiers — a
    // latched non-modifier auto-repeats on the peer forever otherwise.
    this._heldKeys = new Set();

    // Wire format identical to local DisplaySlot's: raw `InputEvent`
    // JSON. The peer's `display/webrtc.rs::handle_message` already
    // dispatches `control` and `pointer` channels through the same
    // `serde_json::from_str::<InputEvent>` parser. F-2 changes
    // nothing about the wire shape — only the gate that decides
    // whether the parsed event reaches `inject_input`.
    //
    // Input transport — the FEDERATED policy: data channels only (the
    // local slot prefers its verified dashboard-control input lane and
    // falls back to channels; no such lane exists to a peer).
    const sendControl = (msg) => {
      if (this.controlChannel && this.controlChannel.readyState === 'open') {
        this.controlChannel.send(JSON.stringify(msg));
      }
    };
    const sendPointer = (msg) => {
      if (this.pointerChannel && this.pointerChannel.readyState === 'open') {
        this.pointerChannel.send(JSON.stringify(msg));
      }
    };

    // Item 3: synthetic-keyup flusher, stored on the instance so
    // `_exitInteractive` / `releaseControl` / the attachToDom rebind
    // path can release held keys after this closure's listeners are
    // otherwise unreachable. Shared with local DisplaySlot
    // (45-display-viewer-core), as is the whole capture stack below —
    // letterbox normalize (canvas-aware for the tile surface),
    // kd/ku/md/mu/mm/sc handlers with annotation/callout suppression,
    // blur flush, pointerenter refocus.
    this._flushHeldKeys = displayViewerMakeHeldKeyFlusher(this, sendControl);
    this._boundHandlers = displayViewerBuildInputHandlers({
      owner: this,
      target,
      sendControl,
      sendPointer,
    });

    // (No clipboard hooks here: clipboard sync is a LOCAL-ONLY policy —
    // federated clipboard is a follow-up. Listener options also differ
    // deliberately: the local slot passes { passive: false }; this path
    // has always installed without options.)
    for (const [evt, handler] of Object.entries(this._boundHandlers)) {
      target.addEventListener(evt, handler);
    }
    // Flag flips to `true` ONLY after the install succeeds — see
    // the doc on the early-return-on-missing-video branch above for
    // why this isn't moved earlier.
    this.interactive = true;
    this._log('info', 'entered interactive mode — input listeners installed');
  }

  // F-2: tear down pointer / keyboard listeners. Idempotent so a
  // race between user-driven Release and server-driven demotion
  // doesn't double-fire. Called from `setPeerAuthorityState` on
  // `state !== 'you'`, and from `close()` for cleanup.
  _exitInteractive() {
    if (!this.interactive) return;
    this.interactive = false;
    // Item 3: flush synthetic keyups BEFORE listener removal — a
    // peer-side authority demotion never fires blur, and without this
    // any held key stays latched down on the peer's display.
    if (this._flushHeldKeys) {
      try { this._flushHeldKeys(); } catch (_) {}
      this._flushHeldKeys = null;
    }
    if (this._heldKeys) this._heldKeys.clear();
    const target = this._interactiveTarget;
    if (target) {
      for (const [evt, handler] of Object.entries(this._boundHandlers)) {
        target.removeEventListener(evt, handler);
      }
    }
    this._interactiveTarget = null;
    this._boundHandlers = {};
    this._log('debug', 'exited interactive mode — input listeners removed');
  }

  // F-1.3c: render the chip + Take/Release button visibility from
  // `peerAuthorityState`. Mirrors local DisplaySlot's
  // `_renderAuthority` exactly so the federated panel looks and
  // behaves the same. Reuses the `display-input-authority` CSS
  // classes (.you/.other/.unclaimed) defined for local 5c.
  _renderAuthorityChip() {
    // Item 4: render into EVERY container this host currently has
    // (daemons-list panel + Station endpoint) — mirroring setStatus —
    // instead of only the getElementById panel, which left Station-
    // hosted panes with a stale chip and the wrong Take/Release button.
    // Chip text/classes ('unknown' hides rather than speculating
    // 'unclaimed', same convention as local 5c), the Take/Release
    // toggle, and the callout arm gate are the shared renderers in
    // 45-display-viewer-core; the multi-container fanout is this
    // class's container-resolution policy.
    for (const container of stationPeerDisplayContainersForHost(this.hostId)) {
      displayViewerRenderAuthorityChip(
        container.querySelector('.peer-display-authority'),
        this.peerAuthorityState,
        'peer-display-authority display-input-authority');
      displayViewerApplyAuthorityButtons(
        container.querySelector('.take-control-btn'),
        container.querySelector('.release-control-btn'),
        container.querySelector('.peer-display-callout'),
        this.peerAuthorityState);
    }
  }

  // F-1.3c: user-intent claim. Sends Request on the authority
  // channel and marks the take pending; the peer's per-subscriber
  // fanout pushes `'you'` back when the registry has updated, and
  // `setPeerAuthorityState('you')` clears the pending flag. F-1
  // does NOT enter interactive input mode here — F-2 layers that
  // on top.
  takeControl() {
    if (this.peerAuthorityState === 'you') {
      // Already holding it — no-op. Same idempotency as local
      // DisplaySlot.takeControl.
      return;
    }
    this._takeControlPending = true;
    if (!this._sendAuthorityFrame('display_input_authority_request')) {
      // Frame never left the browser — don't leave a silently armed
      // pending flag behind (the drop itself already toasted).
      this._takeControlPending = false;
      return;
    }
    // Item 7b: parity with the local slot — a request the peer never
    // answers resets itself with a toast instead of hanging forever.
    if (this._takePendingTimer) window.clearTimeout(this._takePendingTimer);
    this._takePendingTimer = window.setTimeout(() => {
      this._takePendingTimer = null;
      if (!this._takeControlPending) return;
      this._takeControlPending = false;
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'The peer did not answer the input-control request — try again');
      }
    }, DISPLAY_VIEWER_TAKE_PENDING_TIMEOUT_MS);
  }

  // F-1.3c: user-intent release. Sends Release on the authority
  // channel; the peer's identity-matched
  // `apply_release_input_authority_federated` removes the slot and
  // the broadcast pushes `'unclaimed'` back. Idempotent on the
  // peer side (release with no holder is a no-op there too).
  releaseControl() {
    // Item 3: flush held-key keyups BEFORE the release frame — the
    // channels are open and we still hold authority here; after the
    // peer processes the release, our keyups would be gated away and
    // the keys would stay latched down. (`_exitInteractive`, driven by
    // the peer's `unclaimed` push, re-flushes an already-empty set.)
    if (this._flushHeldKeys) {
      try { this._flushHeldKeys(); } catch (_) {}
    }
    this._sendAuthorityFrame('display_input_authority_release');
    this._takeControlPending = false;
  }

  // F-1.3c: serialize and send a Request/Release frame on the
  // authority data channel. Channel-readiness check is required
  // because the user can click Take Control before the channel
  // finishes negotiating; in that case we silently drop and let
  // the user retry once the chip resolves out of 'unknown'. Same
  // forgive-the-race policy as local DisplaySlot's
  // `request_display_input_authority` WS path.
  _sendAuthorityFrame(t) {
    if (!this.authorityChannel || this.authorityChannel.readyState !== 'open') {
      this._log('warn',
        `${t} dropped — authority channel not open ` +
        `(readyState=${this.authorityChannel ? this.authorityChannel.readyState : '(no channel)'})`);
      // Item 7b: the user clicked a button and nothing happened — say so
      // instead of confessing only to the console.
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'Peer display control channel is still connecting — try again in a moment');
      }
      return false;
    }
    try {
      this.authorityChannel.send(JSON.stringify({
        t,
        display_id: this.displayId,
      }));
      this._log('debug', `sent ${t} (display_id=${this.displayId})`);
      return true;
    } catch (err) {
      this._log('warn', `${t} send failed: ${err.message}`);
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'Sending the input-control request to the peer failed — try again');
      }
      return false;
    }
  }

  // Internal: scoped console logger. All diagnostic output from this
  // connection carries `[webrtc-peer ${hostId}]` so the Safari Web
  // Inspector filter can find everything from one peer session in
  // one shot. Mirrors the server-side `source: "webrtc-peer"` tag
  // so cross-side investigations match up by text.
  _log(level, message) {
    displayViewerScopedConsoleLog(`[webrtc-peer ${this.hostId}]`, level, message);
  }

  // Internal: one-line ICE-candidate summary for logs (shared formatter
  // in 45-display-viewer-core).
  _describeCandidate(cand) {
    return describePeerIceCandidateForLog(cand);
  }

  async close() {
    this._clearNoTrackWatchdog();
    // Stop every per-connection timer — no leaked intervals/timeouts
    // across close/retry cycles.
    if (this._retryTimer) {
      window.clearTimeout(this._retryTimer);
      this._retryTimer = null;
    }
    if (this._takePendingTimer) {
      window.clearTimeout(this._takePendingTimer);
      this._takePendingTimer = null;
    }
    if (this._fallbackNoteTimer) {
      window.clearTimeout(this._fallbackNoteTimer);
      this._fallbackNoteTimer = null;
    }
    this._stopStatsSampler();
    stationUnregisterVideoSource(`peer:${this.hostId}:${this.displayId}:${this.sessionId}`);
    stationScheduleUpdate();
    // Provider-level teardown (47-annotation-clips): ends a live-
    // annotation edit or armed callout on this pane — lifecycle parity
    // with removeDisplaySlot on the local path.
    teardownLiveSurfaceForOwner(this);
    // F-2: tear down input listeners FIRST. The `pc.close()` chain
    // below would close the data channels anyway, but uninstalling
    // listeners eagerly prevents a final mousemove from racing the
    // close and landing on a half-closed channel.
    this._exitInteractive();
    // Phase 0: stop the freshness sampler (if active) BEFORE closing
    // the pc / nulling the stream. stop() emits a `session_end`
    // record + final summary and POSTs the last batch synchronously
    // (via authedFetch which is best-effort — the caller doesn't
    // await it). On unload-time close paths the in-flight POST may
    // be cancelled by the browser; that's an acceptable Phase 0
    // limitation. Future: switch to navigator.sendBeacon for the
    // unload path.
    if (this._diagSampler) {
      try { this._diagSampler.stop(); } catch (e) {
        this._log('warn', `[diag-vf] sampler stop failed: ${e.message}`);
      }
      this._diagSampler = null;
    }
    // Best-effort tell peer to tear down its WebRtcPeer.
    await this._sendSignal({ kind: 'close' }).catch(() => {});
    if (this.pc) {
      try { this.pc.close(); } catch {}
      this.pc = null;
    }
    // F-1.3c + F-2: data channels close implicitly when `pc.close()`
    // runs. Null the references so stale post-close calls fail the
    // `readyState === 'open'` check rather than throwing on a freed
    // channel.
    this.authorityChannel = null;
    this.controlChannel = null;
    this.pointerChannel = null;
    this.tileControlChannel = null;
    this.tileSnapshotChannel = null;
    this.tileDeltasChannel = null;
    this.tileCompositor = null;
    this.stream = null;
  }

  async _sendSignal(signal) {
    // Display signaling relay (transport F5): a delivered-once mutation —
    // the facade derives no-replay from the POST verb; peer_id lifts into
    // the HTTP twin's path.
    const resp = await daemonApi.request('api_peer_webrtc_signal', {
      peer_id: this.hostId,
      display_id: this.displayId,
      session_id: this.sessionId,
      signal,
    });
    if (!resp.ok) {
      throw new Error(
        `webrtc signal failed (${resp.status}): ${resp.body?.error || 'unknown'}`
      );
    }
  }
}

async function openPeerDisplay(hostId, displayId, advertiseTcpViaUrl) {
  // One session per host in slice 3a — close any previous before
  // opening a fresh one. Avoids accumulating stale RTCPeerConnections
  // when the user clicks "View display" repeatedly.
  await closePeerDisplaysForHost(hostId);
  const sessionId = generateSessionId();
  // Fall back to the daemon-list lookup when the caller didn't
  // supply the URL explicitly (e.g., future programmatic callers).
  // The button click handler passes the pre-resolved
  // d.browser_tcp_via_url || d.ws_url via data-tcp-via-url, so this
  // fallback only fires for programmatic callers. Same precedence:
  // explicit browser-side URL wins, ws_url is the fallback.
  let effectiveUrl = advertiseTcpViaUrl || '';
  if (!effectiveUrl) {
    const d = daemons.find(x => x.host_id === hostId);
    if (d) {
      effectiveUrl = resolveBrowserTcpViaUrl(d);
    }
  }
  const conn = new PeerDisplayConnection(hostId, displayId, sessionId, effectiveUrl);
  peerDisplayConnections.set(conn.sessionKey(), conn);
  conn.attachToDom();
  await conn.connect();
  stationScheduleUpdate();
}

async function closePeerDisplaysForHost(hostId) {
  const closes = [];
  for (const [key, conn] of peerDisplayConnections.entries()) {
    if (conn.hostId === hostId) {
      closes.push(conn.close().catch(() => {}));
      peerDisplayConnections.delete(key);
    }
  }
  await Promise.all(closes);
  for (const container of stationPeerDisplayContainersForHost(hostId)) {
    container.innerHTML = '';
    container.style.display = 'none';
  }
  stationScheduleUpdate();
}

// Close only the connection(s) streaming one specific display of a
// host — used when the peer retires a display (peer_display_removed)
// while its other displays may stay viewable. No-op when nothing is
// streaming that display. Containers are keyed per host, not per
// display, so they're only cleared when no connections remain for the
// host (with slice 3a's one-session-per-host that's every close).
async function closePeerDisplay(hostId, displayId) {
  const closes = [];
  for (const [key, conn] of peerDisplayConnections.entries()) {
    if (conn.hostId === hostId && Number(conn.displayId) === Number(displayId)) {
      closes.push(conn.close().catch(() => {}));
      peerDisplayConnections.delete(key);
    }
  }
  if (!closes.length) return;
  await Promise.all(closes);
  const hostStillStreaming = [...peerDisplayConnections.values()].some(c => c.hostId === hostId);
  if (!hostStillStreaming) {
    for (const container of stationPeerDisplayContainersForHost(hostId)) {
      container.innerHTML = '';
      container.style.display = 'none';
    }
  }
  stationScheduleUpdate();
}

function handlePeerWebRtcSignal(hostId, displayId, sessionId, signal) {
  const sessionKey = `${hostId}|${displayId}|${sessionId}`;
  const conn = peerDisplayConnections.get(sessionKey);
  const kind = signal && signal.kind;
  if (!conn) {
    // Late straggler (after close) or a session belonging to another
    // dashboard tab. Silent-drop in production was fine but made
    // "why no answer?" debugging hard — log at debug so the operator
    // can tell "signal arrived but no session" apart from "signal
    // never arrived at all."
    console.debug(
      `[webrtc-peer ${hostId}] received ${kind || '(no-kind)'} for unknown sessionKey=${sessionKey} — dropping`
    );
    return;
  }
  if (kind === 'answer') {
    conn.handleAnswer(signal.sdp || '');
  } else if (kind === 'ice_candidate') {
    conn.handleIceCandidate(signal.candidate_json || '');
  } else if (kind === 'close') {
    console.info(`[webrtc-peer ${hostId}] peer sent close — tearing down session ${sessionKey}`);
    closePeerDisplaysForHost(hostId);
  } else {
    console.debug(
      `[webrtc-peer ${hostId}] received unknown signal kind=${kind} for ${sessionKey} — ignoring (forward-compat)`
    );
  }
}

// After a renderDaemonsList re-render rebuilds the controls panels
// from scratch, walk the live PeerDisplayConnection set and re-attach
// each one's MediaStream to the freshly-rendered <video> element.
// The RTCPeerConnection itself stays alive across re-renders — only
// the DOM nodes get regenerated.
function reapplyPeerDisplayPanes() {
  for (const conn of peerDisplayConnections.values()) {
    conn.attachToDom();
  }
}

class PeerFileTransferConnection {
  constructor(hostId, sessionId) {
    this.hostId = String(hostId || '').trim();
    this.sessionId = String(sessionId || generateSessionId());
    this.advertiseTcpViaUrl = '';
    this.pc = null;
    this.channel = null;
    this.connectPromise = null;
    this._answerApplied = false;
    this._pendingCandidates = [];
    this._readyResolve = null;
    this._readyReject = null;
    this._activeReadId = '';
    this._pendingReads = new Map();
  }

  sessionKey() {
    return `${this.hostId}|${this.sessionId}`;
  }

  connect(options = {}) {
    if (this.connectPromise) return this.connectPromise;
    this.connectPromise = this._connect(options);
    return this.connectPromise;
  }

  async _connect(options = {}) {
    if (!this.hostId) throw new Error('peer id is required');
    if (!window.RTCPeerConnection) throw new Error('RTCPeerConnection is unavailable');
    peerFileTransferConnections.set(this.sessionKey(), this);
    const peer = daemons.find(d => String(d.host_id || '') === this.hostId);
    this.advertiseTcpViaUrl = peer ? resolveBrowserTcpViaUrl(peer) : '';
    const iceServers = buildIceServersFromGatewayConfig(gatewayConfig);
    this.pc = new RTCPeerConnection({ iceServers });
    this.channel = this.pc.createDataChannel('intendant-peer-file-transfer', { ordered: true });
    this.channel.binaryType = 'arraybuffer';
    const ready = new Promise((resolve, reject) => {
      this._readyResolve = resolve;
      this._readyReject = reject;
    });
    this.channel.onopen = () => {
      this._log('info', 'data channel open');
      this._readyResolve?.(true);
    };
    this.channel.onclose = () => {
      this._log('debug', 'data channel closed');
    };
    this.channel.onerror = () => {
      const err = new Error('peer file-transfer DataChannel error');
      this._readyReject?.(err);
      this._rejectAll(err);
    };
    this.channel.onmessage = (event) => {
      this._handleMessage(event.data).catch((err) => {
        this._log('warn', `message handling failed: ${err.message}`);
      });
    };
    this.pc.onicecandidate = (event) => {
      if (!event.candidate) {
        this._log('debug', 'local ICE gathering complete (null candidate)');
        return;
      }
      const candidate = event.candidate.toJSON ? event.candidate.toJSON() : event.candidate;
      this._log('debug', `local ICE candidate: ${this._describeCandidate(candidate)}`);
      this._sendSignal({
        kind: 'ice_candidate',
        candidate_json: JSON.stringify(candidate),
      }, options).catch((err) => this._log('warn', `ICE signal failed: ${err.message}`));
    };
    this.pc.onconnectionstatechange = () => {
      const state = this.pc?.connectionState || 'closed';
      this._log(state === 'failed' ? 'warn' : 'debug', `connectionState=${state}`);
      if (state === 'failed') {
        const err = new Error('peer file-transfer WebRTC connection failed');
        this._readyReject?.(err);
        this._rejectAll(err);
      }
    };
    this.pc.oniceconnectionstatechange = () => {
      this._log('debug', `iceConnectionState=${this.pc?.iceConnectionState || 'closed'}`);
    };
    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    const offerSignal = {
      kind: 'offer',
      sdp: this.pc.localDescription?.sdp || offer.sdp || '',
    };
    if (this.advertiseTcpViaUrl) {
      offerSignal.advertise_tcp_via_url = this.advertiseTcpViaUrl;
    }
    this._log('debug', `offer advertiseTcpViaUrl=${this.advertiseTcpViaUrl || '(none)'}`);
    await this._sendSignal(offerSignal, options);
    await this._waitForReady(ready, options);
    return true;
  }

  async _waitForReady(ready, options = {}) {
    const timeoutMs = Math.max(1000, Number(options.timeoutMs || 30000));
    let timeoutId = null;
    let abortHandler = null;
    const timeout = new Promise((_, reject) => {
      timeoutId = window.setTimeout(
        () => reject(new Error('peer file-transfer connection timed out')),
        timeoutMs
      );
    });
    const abort = new Promise((_, reject) => {
      const signal = options.signal;
      if (!signal) return;
      abortHandler = () => reject(dashboardControlAbortError('peer file-transfer connection aborted'));
      if (signal.aborted) abortHandler();
      else signal.addEventListener('abort', abortHandler, { once: true });
    });
    try {
      await Promise.race([ready, timeout, abort]);
    } finally {
      if (timeoutId) window.clearTimeout(timeoutId);
      if (options.signal && abortHandler) options.signal.removeEventListener('abort', abortHandler);
    }
  }

  async _sendSignal(signal, options = {}) {
    // Facade envelope (transport F5): {ok, status, body} — a delivered
    // error response is final (no replay lane exists for signaling).
    const resp = await dashboardTransport.peerFileTransferSignal(this.hostId, {
      session_id: this.sessionId,
      signal,
    }, {
      signal: options.signal,
    });
    if (!resp.ok) {
      throw new Error(`peer file-transfer signal failed (${resp.status}): ${resp.body?.error || 'unknown'}`);
    }
  }

  handleAnswer(sdp) {
    if (!this.pc) return;
    this._log('debug', 'answer received');
    displayViewerApplyRemoteAnswer(this, String(sdp || ''), {
      beforeFlush: (count) =>
        this._log('debug', `answer applied, flushing ${count} buffered ICE candidate(s)`),
      onFlushCandidateError: (err) =>
        this._log('warn', `buffered addIceCandidate failed: ${err.message}`),
      onError: (err) => {
        this._readyReject?.(err);
        this._rejectAll(err);
      },
    });
  }

  handleIceCandidate(candidateJson) {
    if (!candidateJson || !this.pc) return;
    let candidate;
    try {
      candidate = typeof candidateJson === 'string' ? JSON.parse(candidateJson) : candidateJson;
    } catch (err) {
      this._log('warn', `remote ICE JSON parse failed: ${err.message}`);
      return;
    }
    displayViewerIngestRemoteIceCandidate(this, candidate, {
      onQueued: (c) =>
        this._log('debug', `buffering remote ICE candidate: ${this._describeCandidate(c)}`),
      onAdd: (c) =>
        this._log('debug', `remote ICE candidate: ${this._describeCandidate(c)}`),
      onAddError: (err) =>
        this._log('warn', `addIceCandidate failed: ${err.message}`),
    });
  }

  async readRange(path, offset, length, options = {}) {
    if (!this.channel || this.channel.readyState !== 'open') {
      await this.connect(options);
    }
    if (this._activeReadId) {
      throw new Error('peer file-transfer channel already has an active read');
    }
    if (options.signal?.aborted) throw dashboardControlAbortError('peer file-transfer read aborted');
    const id = `read-${Date.now().toString(36)}-${dashboardRandomBase64Url(8)}`;
    this._activeReadId = id;
    const request = {
      t: 'read',
      id,
      path: String(path || ''),
      offset: Math.max(0, Math.floor(Number(offset) || 0)),
      length: Math.max(1, Math.floor(Number(length) || 1)),
    };
    return new Promise((resolve, reject) => {
      let timeoutId = null;
      let abortHandler = null;
      const cleanup = () => {
        if (timeoutId) window.clearTimeout(timeoutId);
        if (options.signal && abortHandler) options.signal.removeEventListener('abort', abortHandler);
        this._pendingReads.delete(id);
        if (this._activeReadId === id) this._activeReadId = '';
      };
      const fail = (err) => {
        cleanup();
        reject(err);
      };
      const timeoutMs = Math.max(1000, Number(options.timeoutMs || rangedDownloadTimeoutMs(request.length)));
      timeoutId = window.setTimeout(
        () => fail(new Error('peer file-transfer read timed out')),
        timeoutMs
      );
      if (options.signal) {
        abortHandler = () => {
          try {
            if (this.channel?.readyState === 'open') {
              this.channel.send(JSON.stringify({ t: 'cancel', id }));
            }
          } catch {}
          fail(dashboardControlAbortError('peer file-transfer read aborted'));
        };
        if (options.signal.aborted) {
          abortHandler();
          return;
        }
        options.signal.addEventListener('abort', abortHandler, { once: true });
      }
      this._pendingReads.set(id, {
        id,
        request,
        chunks: [],
        start: null,
        resolve: (value) => {
          cleanup();
          resolve(value);
        },
        reject: fail,
      });
      try {
        this.channel.send(JSON.stringify(request));
      } catch (err) {
        fail(err);
      }
    });
  }

  async _handleMessage(data) {
    if (typeof data === 'string') {
      this._handleTextFrame(data);
      return;
    }
    let bytes;
    if (data instanceof Blob) {
      bytes = new Uint8Array(await data.arrayBuffer());
    } else if (data instanceof ArrayBuffer) {
      bytes = new Uint8Array(data);
    } else if (ArrayBuffer.isView(data)) {
      bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    } else {
      return;
    }
    const pending = this._activeReadId ? this._pendingReads.get(this._activeReadId) : null;
    if (!pending) return;
    pending.chunks.push(bytes);
  }

  _handleTextFrame(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch (err) {
      this._log('warn', `JSON frame parse failed: ${err.message}`);
      return;
    }
    const id = String(frame?.id || '');
    const pending = id ? this._pendingReads.get(id) : null;
    if (!pending) return;
    if (frame.t === 'start') {
      pending.start = frame;
      return;
    }
    if (frame.t === 'error') {
      pending.reject(new Error(String(frame.error || 'peer file-transfer read failed')));
      return;
    }
    if (frame.t !== 'end') return;
    const byteLength = pending.chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
    const bytes = new Uint8Array(byteLength);
    let cursor = 0;
    for (const chunk of pending.chunks) {
      bytes.set(chunk, cursor);
      cursor += chunk.byteLength;
    }
    const start = pending.start || {};
    const rangeStart = Number(start.offset ?? pending.request.offset);
    const totalSize = Number(start.total_size ?? frame.total_size ?? (rangeStart + byteLength));
    pending.resolve({
      bytes,
      rangeStart,
      rangeEnd: rangeStart + byteLength,
      totalSize,
      filename: start.filename ? String(start.filename) : filesDownloadFilenameFromPath(pending.request.path),
      contentType: start.content_type ? String(start.content_type) : 'application/octet-stream',
    });
  }

  async close() {
    peerFileTransferConnections.delete(this.sessionKey());
    await this._sendSignal({ kind: 'close' }).catch(() => {});
    const err = dashboardControlAbortError('peer file-transfer connection closed');
    this._rejectAll(err);
    if (this.pc) {
      try { this.pc.close(); } catch {}
      this.pc = null;
    }
    this.channel = null;
  }

  _rejectAll(err) {
    for (const pending of this._pendingReads.values()) {
      pending.reject(err);
    }
    this._pendingReads.clear();
    this._activeReadId = '';
  }

  _log(level, message) {
    displayViewerScopedConsoleLog(
      `[peer-file-transfer ${this.hostId}/${this.sessionId}]`, level, message,
    );
  }

  _describeCandidate(candidate) {
    return describePeerIceCandidateForLog(candidate);
  }
}

function handlePeerFileTransferSignal(hostId, sessionId, signal) {
  const sessionKey = `${hostId}|${sessionId}`;
  const conn = peerFileTransferConnections.get(sessionKey);
  const kind = signal && signal.kind;
  if (!conn) {
    console.debug(`[peer-file-transfer ${hostId}] received ${kind || '(no-kind)'} for unknown session ${sessionId}`);
    return;
  }
  if (kind === 'answer') {
    conn.handleAnswer(signal.sdp || '');
  } else if (kind === 'ice_candidate') {
    conn.handleIceCandidate(signal.candidate_json || '');
  } else if (kind === 'close') {
    conn.close().catch(() => {});
  } else {
    console.debug(`[peer-file-transfer ${hostId}] unknown signal kind=${kind || '(none)'} for session ${sessionId}`);
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

function setDaemonsStatus(msg, kind) {
  const el = document.getElementById('daemons-status');
  if (!el) return;
  el.textContent = msg || '';
  el.className = 'daemons-status' + (kind ? ' ' + kind : '');
}

function setDaemonPairingStatus(id, msg, kind) {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = msg || '';
  el.className = 'daemon-pairing-status' + (kind ? ' ' + kind : '');
}

const PEER_PROFILE_OPTIONS = [
  {
    profile: 'read-only-display',
    label: 'Read-only display',
    summary: 'Presence, stats, and display viewing. No input control. The default when no profile is stated.',
  },
  {
    profile: 'peer-operator',
    label: 'Peer operator',
    summary: 'Daemon-to-daemon display input, messages, tasks, and approvals. No settings or runtime control.',
  },
  {
    profile: 'shared-session-spectator',
    label: 'Spectator',
    summary: 'Presence, stats, and display viewing. No input control.',
  },
  {
    profile: 'task-runner',
    label: 'Task runner',
    summary: 'Presence, stats, messages, and task delegation.',
  },
  {
    profile: 'session-reader',
    label: 'Session reader',
    summary: 'Presence, stats, and session inspection (lists, logs, reports). Read-only.',
  },
  {
    profile: 'file-reader',
    label: 'File reader',
    summary: 'Presence, stats, and read-only filesystem access.',
  },
  {
    profile: 'file-operator',
    label: 'File operator',
    summary: 'Presence, stats, and filesystem read/write.',
  },
  {
    profile: 'terminal-operator',
    label: 'Terminal operator',
    summary: 'Presence, stats, session inspection, and shared shells (view, type, spawn).',
  },
  {
    profile: 'stats',
    label: 'Stats',
    summary: 'Presence and usage statistics only.',
  },
  {
    profile: 'presence-only',
    label: 'Presence only',
    summary: 'Basic presence only.',
  },
  {
    profile: 'peer-root',
    label: 'Peer root',
    summary: 'All daemon-to-daemon peer operations, including settings, sessions, files, shell, and runtime control.',
  },
];

function peerProfileMeta(profile) {
  const value = String(profile || 'presence-only').trim().toLowerCase();
  const aliases = {
    presence: 'presence-only',
    'stats-only': 'stats',
    'display-read-only': 'read-only-display',
    spectator: 'shared-session-spectator',
    'sessions-read': 'session-reader',
    'session-inspect': 'session-reader',
    'logs-read': 'session-reader',
    'files-read': 'file-reader',
    'filesystem-read-only': 'file-reader',
    files: 'file-operator',
    'filesystem-operator': 'file-operator',
    'peer-terminal-operator': 'terminal-operator',
    terminal: 'terminal-operator',
    shell: 'terminal-operator',
    operator: 'peer-operator',
    admin: 'peer-root',
    'admin-peer': 'peer-root',
    'peer-daemon': 'peer-root',
  };
  const canonical = aliases[value] || value;
  return PEER_PROFILE_OPTIONS.find(item => item.profile === canonical) || {
    profile: canonical,
    label: canonical || 'Presence only',
    summary: 'Unknown profiles are treated as presence-only by this build.',
  };
}

function renderPeerProfileOptions(selected) {
  // Mirrors the daemon's DEFAULT_PROFILE (access_policy.rs): an approval
  // with no stated profile yields read-only-display.
  const value = String(selected || 'read-only-display').toLowerCase();
  const selectedMeta = peerProfileMeta(value);
  return PEER_PROFILE_OPTIONS.map(({ profile, label, summary }) => (
    `<option value="${escapeHtml(profile)}" ${profile === selectedMeta.profile ? 'selected' : ''} title="${escapeHtml(summary)}">${escapeHtml(label)}</option>`
  )).join('');
}

function setDaemonPairingMode(mode) {
  const next = mode || 'request';
  document.querySelectorAll('[data-daemon-pairing-mode]').forEach(button => {
    button.classList.toggle('active', button.getAttribute('data-daemon-pairing-mode') === next);
  });
  document.querySelectorAll('[data-daemon-pairing-pane]').forEach(pane => {
    pane.classList.toggle('active', pane.getAttribute('data-daemon-pairing-pane') === next);
  });
}

async function createDaemonInvite() {
  const cardUrlInput = document.getElementById('daemon-invite-card-url');
  const labelInput = document.getElementById('daemon-invite-label');
  const clientNameInput = document.getElementById('daemon-invite-client-name');
  const output = document.getElementById('daemon-invite-output');
  const copyBtn = document.getElementById('daemon-invite-copy-btn');
  const btn = document.getElementById('daemon-invite-create-btn');
  if (!output || !btn) return;

  const cardUrl = cardUrlInput ? cardUrlInput.value.trim() : '';
  if (cardUrl && !/^(https?|wss?):\/\//.test(cardUrl)) {
    setDaemonPairingStatus('daemon-invite-status', 'URL must start with http://, https://, ws://, or wss://', 'error');
    return;
  }

  const body = {};
  if (cardUrl) body.card_url = cardUrl;
  if (labelInput && labelInput.value.trim()) body.label = labelInput.value.trim();
  if (clientNameInput && clientNameInput.value.trim()) body.client_name = clientNameInput.value.trim();

  btn.disabled = true;
  setDaemonPairingStatus('daemon-invite-status', 'Creating...', '');
  try {
    // Pairing mutations (transport F5): verb-derived no-replay, params
    // unchanged — same policy for every POST in this dialog set.
    const resp = await daemonApi.request('api_peer_pairing_invite', body);
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-invite-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    output.value = result.invite || '';
    if (copyBtn) copyBtn.disabled = !output.value;
    const target = result.card_url ? ` for ${result.card_url}` : '';
    setDaemonPairingStatus('daemon-invite-status', `Inbound grant invite ready${target}`, 'ok');
    await loadPeerIdentities();
  } catch (e) {
    setDaemonPairingStatus('daemon-invite-status', `Request failed: ${e.message}`, 'error');
  } finally {
    btn.disabled = false;
  }
}

async function copyDaemonInvite() {
  const output = document.getElementById('daemon-invite-output');
  if (!output || !output.value.trim()) return;
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(output.value.trim());
    } else {
      output.focus();
      output.select();
      document.execCommand('copy');
    }
    setDaemonPairingStatus('daemon-invite-status', 'Copied', 'ok');
  } catch (e) {
    setDaemonPairingStatus('daemon-invite-status', `Copy failed: ${e.message}`, 'error');
  }
}

async function joinDaemonInvite() {
  const inviteInput = document.getElementById('daemon-join-invite');
  const labelInput = document.getElementById('daemon-join-label');
  const btn = document.getElementById('daemon-join-btn');
  const invite = inviteInput ? inviteInput.value.trim() : '';
  if (!invite) {
    setDaemonPairingStatus('daemon-join-status', 'Invite is required', 'error');
    return;
  }

  const body = { invite };
  if (labelInput && labelInput.value.trim()) body.label = labelInput.value.trim();

  if (btn) btn.disabled = true;
  setDaemonPairingStatus('daemon-join-status', 'Joining...', '');
  try {
    const resp = await daemonApi.request('api_peer_pairing_join', body);
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-join-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    const action = result.updated_existing ? 'Updated outbound connection' : 'Saved outbound connection';
    setDaemonPairingStatus('daemon-join-status', `${action}; connecting in background`, 'ok');
    if (inviteInput) inviteInput.value = '';
    if (labelInput) labelInput.value = '';
    await refreshPeersFromApi();
    window.setTimeout(() => {
      refreshPeersFromApi().catch(() => {});
    }, 1200);
  } catch (e) {
    setDaemonPairingStatus('daemon-join-status', `Request failed: ${e.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function requestDaemonAccess() {
  const targetInput = document.getElementById('daemon-request-target-url');
  const labelInput = document.getElementById('daemon-request-label');
  const profileInput = document.getElementById('daemon-request-profile');
  const requestIdInput = document.getElementById('daemon-request-id');
  const codeEl = document.getElementById('daemon-request-code');
  const btn = document.getElementById('daemon-request-btn');
  const targetUrl = targetInput ? targetInput.value.trim() : '';

  if (!targetUrl) {
    setDaemonPairingStatus('daemon-request-status', 'Target URL is required', 'error');
    return;
  }
  if (!/^https?:\/\//.test(targetUrl)) {
    setDaemonPairingStatus('daemon-request-status', 'URL must start with http:// or https://', 'error');
    return;
  }

  const body = { target_url: targetUrl };
  if (labelInput && labelInput.value.trim()) body.label = labelInput.value.trim();
  if (profileInput && profileInput.value.trim()) body.profile = profileInput.value.trim();

  if (btn) btn.disabled = true;
  if (codeEl) codeEl.textContent = '';
  setDaemonPairingStatus('daemon-request-status', 'Requesting...', '');
  try {
    const resp = await daemonApi.request('api_peer_pairing_request_access', body);
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-request-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    if (requestIdInput) requestIdInput.value = result.request_id || '';
    if (codeEl) codeEl.textContent = result.code ? `Approval code ${result.code}` : '';
    setDaemonPairingStatus('daemon-request-status', 'Waiting for target approval', 'ok');
  } catch (e) {
    setDaemonPairingStatus('daemon-request-status', `Request failed: ${e.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function completeDaemonAccessRequest() {
  const requestIdInput = document.getElementById('daemon-request-id');
  const labelInput = document.getElementById('daemon-request-label');
  const btn = document.getElementById('daemon-request-complete-btn');
  const requestId = requestIdInput ? requestIdInput.value.trim() : '';
  if (!requestId) {
    setDaemonPairingStatus('daemon-request-status', 'Request id is required', 'error');
    return;
  }

  if (btn) btn.disabled = true;
  setDaemonPairingStatus('daemon-request-status', 'Checking approval...', '');
  const body = {
    request_id: requestId,
    ...(labelInput && labelInput.value.trim() ? { label: labelInput.value.trim() } : {}),
  };
  try {
    const resp = await daemonApi.request('api_peer_pairing_request_access_poll', body);
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-request-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    const status = String(result.status || '').toLowerCase();
    if (status === 'approved' && result.card_url) {
      const action = result.updated_existing ? 'Updated outbound connection' : 'Saved outbound connection';
      setDaemonPairingStatus('daemon-request-status', `${action}; connecting in background`, 'ok');
      await refreshPeersFromApi();
      window.setTimeout(() => {
        refreshPeersFromApi().catch(() => {});
      }, 1200);
    } else if (status === 'pending') {
      setDaemonPairingStatus('daemon-request-status', 'Still waiting for approval', '');
    } else if (status === 'denied') {
      setDaemonPairingStatus('daemon-request-status', 'Request denied on target daemon', 'error');
    } else if (status === 'expired') {
      setDaemonPairingStatus('daemon-request-status', 'Request expired', 'error');
    } else {
      setDaemonPairingStatus('daemon-request-status', `Status: ${status || 'unknown'}`, '');
    }
  } catch (e) {
    setDaemonPairingStatus('daemon-request-status', `Request failed: ${e.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function loadPeerAccessRequests() {
  const list = document.getElementById('daemon-access-requests-list');
  if (!list) return;
  setDaemonPairingStatus('daemon-access-requests-status', 'Loading...', '');
  try {
    // GET twin (transport F5): tunnel first, direct-HTTP fallback per the
    // verb-derived read policy — same for the identities read below.
    const resp = await daemonApi.request('api_peer_pairing_requests');
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-access-requests-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    renderPeerAccessRequests(Array.isArray(result.requests) ? result.requests : []);
    setDaemonPairingStatus('daemon-access-requests-status', 'Updated', 'ok');
  } catch (e) {
    setDaemonPairingStatus('daemon-access-requests-status', `Request failed: ${e.message}`, 'error');
  }
}

function accessRequestTimeLabel(unixSeconds) {
  if (!Number.isFinite(Number(unixSeconds)) || Number(unixSeconds) <= 0) return '';
  const date = new Date(Number(unixSeconds) * 1000);
  return date.toLocaleString([], {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

function renderPeerAccessRequests(requests) {
  const list = document.getElementById('daemon-access-requests-list');
  if (!list) return;
  if (!requests.length) {
    list.innerHTML = '<div class="daemon-access-request-meta">No incoming requests.</div>';
    renderAccessAdminSummaries();
    return;
  }

  // Upward-grant guard (docs/src/trust-tiers.md): approving a peer here
  // grants that daemon authority ON this machine. On an integrated-tier
  // machine that is the alarm condition — advisory, never a refusal.
  const integratedTier = (typeof accessIamModel === 'function' && typeof accessOverviewModel === 'function')
    && String(accessIamModel(accessOverviewModel())?.tier || '') === 'integrated';

  list.innerHTML = requests.map(req => {
    const requestId = String(req.request_id || '');
    const label = escapeHtml(req.requester_label || 'Unnamed daemon');
    const code = escapeHtml(req.code || '');
    const status = String(req.status || 'unknown').toLowerCase();
    // The || fallback mirrors the daemon's DEFAULT_PROFILE: a request with
    // no stated profile is what an unadorned approval will grant.
    const role = peerProfileMeta(req.approved_profile || req.requested_profile || 'read-only-display');
    const source = req.source_hint ? `Source ${escapeHtml(req.source_hint)}` : '';
    const expires = req.expires_at_unix ? `Expires ${escapeHtml(accessRequestTimeLabel(req.expires_at_unix))}` : '';
    const timing = [source, expires].filter(Boolean).join(' - ');
    const pending = status === 'pending';
    const requestedProfile = req.approved_profile || req.requested_profile || 'read-only-display';
    const roleLabel = req.approved_profile ? 'Approved peer profile' : 'Requested peer profile';
    // Cross-owner tier claim (docs/src/trust-tiers.md § Where fleet
    // metadata rides): the daemon stores requester_tier only when the
    // claim was signed inside a verified caller-ID, so presence here
    // already means "pinned to a proven daemon key". Unverified and
    // legacy callers carry no claim and render exactly as before.
    const requesterTier = String(req.requester_tier || '').trim();
    // First-contact naming cue (trust-tiers § lookalike names): a
    // VERIFIED caller key that matches a fleet record you have petnamed
    // is a machine you know — say so by your name for it. A verified
    // key with no petname is honest first contact: no name on file.
    const callerId = String(req.requester_daemon_id || '').trim();
    let callerLine = '';
    if (callerId) {
      const named = (typeof accessFleetTargets === 'function' ? accessFleetTargets() : [])
        .find(t => String(t.connect_daemon_id || '').trim() === callerId && String(t.petname || '').trim());
      callerLine = named
        ? `caller: your “${escapeHtml(String(named.petname).trim())}” (verified key)`
        : `caller: verified key ${escapeHtml(callerId.slice(0, 12))}… — no machine you’ve named`;
    }
    // THE upward-grant alarm case: a self-declared disposable machine
    // asking for authority on this integrated one.
    const upwardAlarm = pending && integratedTier && requesterTier === 'disposable';
    return `
      <div class="daemon-access-request-card">
        <div class="daemon-access-request-head">
          <span>${label}</span>
          <span class="daemon-access-request-code">${code}</span>
        </div>
        <div class="daemon-access-request-meta">Status ${escapeHtml(status)} - ${roleLabel} ${escapeHtml(role.label)}</div>
        <div class="daemon-access-request-meta daemon-role-summary">${escapeHtml(role.summary)}</div>
        ${callerLine ? `<div class="daemon-access-request-meta" title="The requesting daemon proved this identity key over the doorbell. A petname means it matches a machine YOU named; ‘no machine you’ve named’ means first contact — a lookalike never inherits a familiar name.">${callerLine}</div>` : ''}
        ${requesterTier ? `<div class="daemon-access-request-meta" title="Stated by the requesting daemon inside its verified caller-ID signature — stored and shown only because the signature checked out.">requester says: ${escapeHtml(requesterTier)}</div>` : ''}
        ${upwardAlarm
          ? '<div class="daemon-access-request-meta daemon-access-request-warn" title="Grants flow toward disposable machines, never up. Approving would hand a disposable box authority on this integrated one — the tier bridge the doctrine warns about.">⚠ Upward grant: this is a disposable machine asking for authority on an integrated one. Approving bridges your tiers.</div>'
          : (pending && integratedTier ? '<div class="daemon-access-request-meta daemon-access-request-warn" title="Grants flow toward disposable machines, never up. If that daemon is lower-trust than this one, approving bridges your tiers — see the Trust tier card in Access.">⚠ Integrated-tier machine: approving grants a peer daemon authority here. Make sure trust flows downward.</div>' : '')}
        ${timing ? `<div class="daemon-access-request-meta">${timing}</div>` : ''}
        ${pending ? `<div class="daemon-pairing-row"><select data-access-request-profile="${escapeHtml(requestId)}">${renderPeerProfileOptions(requestedProfile)}</select></div>` : ''}
        <div class="daemon-access-request-actions">
          <button class="approve" type="button" data-access-request-action="approve" data-request-id="${escapeHtml(requestId)}" ${pending ? '' : 'disabled'}>Approve</button>
          <button class="deny" type="button" data-access-request-action="deny" data-request-id="${escapeHtml(requestId)}" ${pending ? '' : 'disabled'}>Deny</button>
        </div>
      </div>
    `;
  }).join('');

  list.querySelectorAll('[data-access-request-action]').forEach(button => {
    button.addEventListener('click', () => {
      const requestId = button.getAttribute('data-request-id') || '';
      const action = button.getAttribute('data-access-request-action') || '';
      decidePeerAccessRequest(requestId, action);
    });
  });
  renderAccessAdminSummaries();
}

async function decidePeerAccessRequest(requestId, op) {
  if (!requestId || !/^(approve|deny)$/.test(op)) return;
  setDaemonPairingStatus('daemon-access-requests-status', `${op === 'approve' ? 'Approving' : 'Denying'}...`, '');
  try {
    const body = {};
    if (op === 'approve') {
      const profileSelect = document.querySelector(`[data-access-request-profile="${CSS.escape(requestId)}"]`);
      if (profileSelect && profileSelect.value) body.profile = profileSelect.value;
    }
    // Access-request decisions are mutations (transport F5): verb-derived
    // no-replay, params unchanged — request_id/op lift into the HTTP
    // twin's path captures, the optional profile stays as the body.
    const resp = await daemonApi.request('api_peer_pairing_request_decision', {
      request_id: requestId,
      op,
      ...body,
    });
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-access-requests-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    setDaemonPairingStatus(
      'daemon-access-requests-status',
      op === 'approve' ? 'Granted inbound access' : 'Denied inbound access',
      op === 'approve' ? 'ok' : ''
    );
    await loadPeerAccessRequests();
    if (op === 'approve') await loadPeerIdentities();
  } catch (e) {
    setDaemonPairingStatus('daemon-access-requests-status', `Request failed: ${e.message}`, 'error');
  }
}

async function loadPeerIdentities() {
  const list = document.getElementById('daemon-peer-identities-list');
  if (!list) return;
  setDaemonPairingStatus('daemon-peer-identities-status', 'Loading...', '');
  try {
    const resp = await daemonApi.request('api_peer_pairing_identities');
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-peer-identities-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    renderPeerIdentities(Array.isArray(result.identities) ? result.identities : []);
    setDaemonPairingStatus('daemon-peer-identities-status', 'Updated', 'ok');
  } catch (e) {
    setDaemonPairingStatus('daemon-peer-identities-status', `Request failed: ${e.message}`, 'error');
  }
}

function renderPeerIdentities(identities) {
  const list = document.getElementById('daemon-peer-identities-list');
  if (!list) return;
  if (!identities.length) {
    list.innerHTML = '<div class="daemon-access-request-meta">No inbound access grants.</div>';
    renderAccessAdminSummaries();
    return;
  }

  list.innerHTML = identities.map(identity => {
    const fingerprint = String(identity.fingerprint || '');
    const label = escapeHtml(identity.label || 'Unnamed daemon');
    const status = String(identity.status || 'unknown').toLowerCase();
    const role = peerProfileMeta(identity.profile || 'presence-only');
    const request = identity.request_id ? `Request ${escapeHtml(identity.request_id)}` : '';
    const card = identity.card_url ? escapeHtml(identity.card_url) : '';
    const active = status === 'approved';
    return `
      <div class="daemon-identity-card ${escapeHtml(status)}">
        <div class="daemon-identity-head">
          <span>${label}</span>
          <span class="daemon-identity-status ${escapeHtml(status)}">${escapeHtml(status)}</span>
        </div>
        <div class="daemon-identity-role" title="Peer profile ${escapeHtml(identity.profile || 'presence-only')}">Peer profile ${escapeHtml(role.label)}</div>
        <div class="daemon-identity-profile">${escapeHtml(role.summary)}${request ? ` - ${request}` : ''}</div>
        ${card ? `<div class="daemon-identity-profile">${card}</div>` : ''}
        <div class="daemon-identity-fingerprint" title="${escapeHtml(fingerprint)}">${escapeHtml(fingerprint)}</div>
        <div class="daemon-identity-actions">
          <button type="button" data-peer-identity-revoke="${escapeHtml(fingerprint)}" ${active ? '' : 'disabled'}>Revoke</button>
        </div>
      </div>
    `;
  }).join('');

  list.querySelectorAll('[data-peer-identity-revoke]').forEach(button => {
    button.addEventListener('click', () => revokePeerIdentity(button.getAttribute('data-peer-identity-revoke') || ''));
  });
  renderAccessAdminSummaries();
}

async function revokePeerIdentity(identity) {
  const value = String(identity || '').trim();
  if (!value) return;
  setDaemonPairingStatus('daemon-peer-identities-status', 'Revoking...', '');
  try {
    const resp = await daemonApi.request('api_peer_pairing_identity_revoke', { identity: value });
    const result = resp.body || {};
    if (!resp.ok) {
      setDaemonPairingStatus('daemon-peer-identities-status', `Failed: ${result.error || resp.status}`, 'error');
      return;
    }
    setDaemonPairingStatus('daemon-peer-identities-status', 'Revoked inbound access', 'ok');
    await loadPeerIdentities();
  } catch (e) {
    setDaemonPairingStatus('daemon-peer-identities-status', `Request failed: ${e.message}`, 'error');
  }
}

async function addDaemon() {
  const urlInput = document.getElementById('daemon-add-url');
  const labelInput = document.getElementById('daemon-add-label');
  const viaInput = document.getElementById('daemon-add-via');
  const browserTcpViaInput = document.getElementById('daemon-add-browser-tcp-via');
  const persistInput = document.getElementById('daemon-add-persist');
  const baseUrl = urlInput.value.trim();
  const userLabel = labelInput.value.trim();
  // Per-peer connecting-side override: replaces the card's transports
  // when non-empty. Useful when this daemon knows about a path the
  // advertising peer's card doesn't list (port-forwards, proxies,
  // named tunnels, Tailscale tailnet on this side only).
  const viaUrls = parseTokenList(viaInput ? viaInput.value : '');
  // Browser-side TCP via URL: what URL the browser uses to reach the
  // peer's HTTP port for WebRTC ICE-TCP. Decoupled from via_urls for
  // the topology where browser and primary see different addresses
  // (primary-side localhost tunnel, split-machine setup, etc.).
  // Empty → server falls back to d.ws_url, matching slice 3a.2 behavior.
  const browserTcpViaUrl = browserTcpViaInput
    ? browserTcpViaInput.value.trim()
    : '';
  const persist = !!(persistInput && persistInput.checked);

  if (!baseUrl) { setDaemonsStatus('URL is required', 'error'); return; }
  if (!/^https?:\/\//.test(baseUrl)) {
    setDaemonsStatus('URL must start with http:// or https://', 'error');
    return;
  }

  // Mixed-content guard: an HTTPS page can't fetch() or open ws:// to
  // an http:// target. Catch this here with a clearer message than
  // the browser's cryptic CORS/mixed-content error.
  if (location.protocol === 'https:' && baseUrl.startsWith('http://')) {
    setDaemonsStatus(
      'This dashboard is served over HTTPS, so secondaries must also be HTTPS (browser mixed-content rule). Set up access certificates on the remote daemon with `intendant access setup`, or use an explicit local/debug HTTP setup for both daemons.',
      'error'
    );
    return;
  }

  setDaemonsStatus('Connecting...', '');

  // POST to /api/peers — the server fetches the card, picks a
  // supported transport, spawns the peer actor, and returns the
  // peer_id. The browser doesn't need to fetch the card itself
  // anymore; the server is the single source of truth.
  const cardUrl = baseUrl.replace(/\/$/, '') + '/.well-known/agent-card.json';
  // Body always carries via_urls (possibly empty). Server-side serde
  // `#[default]` makes the field optional on the wire so older clients
  // continue to work without it.
  const body = { card_url: cardUrl };
  if (userLabel) body.label = userLabel;
  if (persist) body.persist = true;
  if (viaUrls.length > 0) body.via_urls = viaUrls;
  if (browserTcpViaUrl) body.browser_tcp_via_url = browserTcpViaUrl;
  try {
    // Peer add is a mutation (transport F5): verb-derived no-replay —
    // the hosted validator's peer-mutations self-test probes exactly this
    // (a failed tunnel attempt must throw, never re-POST over HTTP).
    const resp = await daemonApi.request('api_peer_add', body);
    const result = resp.body || {};
    if (!resp.ok) {
      const errorText = result.error || String(resp.status);
      if (/peer added for this run/i.test(errorText)) {
        setDaemonsStatus(errorText, 'error');
        return;
      }
      const hint = baseUrl.startsWith('https://')
        ? 'This daemon may not have a trusted client certificate for that peer yet.'
        : 'Is the remote daemon running and reachable?';
      setDaemonsStatus(`Failed to add outbound connection: ${errorText}. ${hint}`, 'error');
      return;
    }
    setDaemonsStatus(
      result.persisted ? `Added and saved ${result.peer_id}` : `Added for this run ${result.peer_id}`,
      'ok'
    );
  } catch (e) {
    setDaemonsStatus(`Request failed: ${e.message}`, 'error');
    return;
  }

  // Refresh the peer list from the server and re-register WASM
  // connections so the new peer shows up immediately.
  await refreshPeersFromApi();
  urlInput.value = '';
  labelInput.value = '';
  if (viaInput) viaInput.value = '';
  if (browserTcpViaInput) browserTcpViaInput.value = '';
  if (persistInput) persistInput.checked = false;
}

async function removeDaemon(hostId) {
  // DELETE from the server-side registry. The server drives the
  // explicit disconnect on the peer's actor (Disconnecting →
  // Disconnected state machine).
  try {
    // DELETE twin (transport F5): the descriptor carries the leftover
    // {peer_id} as the JSON body — the endpoint's historical shape.
    await daemonApi.request('api_peer_remove', { peer_id: hostId });
  } catch { /* best-effort */ }
  removeDashboardAccessTarget(hostId);
  await refreshPeersFromApi();
  setDaemonsStatus(`Removed ${hostId}`, '');
}

// Parse a comma-or-whitespace separated token list. Used by the
// Coordinator forms (capability lists like "display, computer-use,
// custom:foo") and by the Add Peer form (via URL list). Empty entries
// are dropped; surrounding whitespace is trimmed.
function parseTokenList(text) {
  return String(text || '')
    .split(/[\s,]+/)
    .map(s => s.trim())
    .filter(s => s.length > 0);
}

// Find the connected peers that satisfy all listed capabilities.
// GET /api/peers/eligible?capability=display&capability=computer-use
// returns {peers: [PeerSnapshot]} on 200; 400 on missing/unknown
// capability with {error, hint?}.
async function findEligiblePeers() {
  const input = document.getElementById('coord-eligible-caps');
  const btn = document.getElementById('coord-eligible-btn');
  const out = document.getElementById('coord-eligible-result');
  if (!input || !out) return;

  const caps = parseTokenList(input.value);
  if (caps.length === 0) {
    out.textContent = 'Enter at least one capability.';
    out.className = 'coord-result error';
    return;
  }

  btn.disabled = true;
  out.textContent = 'Searching…';
  out.className = 'coord-result';
  try {
    // GET twin (transport F5): the descriptor's queryRepeat rebuilds the
    // repeated ?capability= keys from the tunnel's capabilities array.
    const resp = await daemonApi.request('api_peer_eligible', { capabilities: caps });
    const result = resp.body || {};
    if (!resp.ok) {
      const msg = result.error || `HTTP ${resp.status}`;
      const hint = result.hint ? ` (${result.hint})` : '';
      out.textContent = `Failed: ${msg}${hint}`;
      out.className = 'coord-result error';
      return;
    }
    const peers = Array.isArray(result.peers) ? result.peers : [];
    if (peers.length === 0) {
      out.textContent = `No connected peer satisfies: ${caps.join(', ')}.`;
      out.className = 'coord-result';
      return;
    }
    out.className = 'coord-result ok';
    out.innerHTML =
      `<div>Found ${peers.length} eligible peer${peers.length === 1 ? '' : 's'}:</div>` +
      peers
        .map(p => {
          const id = escapeHtml(p.host_id || p.id || '');
          const label = escapeHtml(p.label || p.host_id || p.id || '');
          return `<span class="coord-peer"><span class="coord-peer-id">${label}</span> <span class="coord-considered">${id}</span></span>`;
        })
        .join('');
  } catch (e) {
    out.textContent = `Error: ${e.message}`;
    out.className = 'coord-result error';
  } finally {
    btn.disabled = false;
  }
}

// Delegate a task to whichever connected peer satisfies all required
// capabilities. POST /api/coordinator/route returns {peer_id, task_id}
// on 200; 404 on no-route with {error, considered: [peer_id]}; 502 on
// delegation failure with {error, peer_id}.
async function routeTask() {
  const capsInput = document.getElementById('coord-route-caps');
  const instrInput = document.getElementById('coord-route-instructions');
  const btn = document.getElementById('coord-route-btn');
  const out = document.getElementById('coord-route-result');
  if (!capsInput || !instrInput || !out) return;

  const caps = parseTokenList(capsInput.value);
  const instructions = instrInput.value.trim();
  if (caps.length === 0) {
    out.textContent = 'Enter at least one required capability.';
    out.className = 'coord-result error';
    return;
  }
  if (!instructions) {
    out.textContent = 'Enter task instructions.';
    out.className = 'coord-result error';
    return;
  }

  btn.disabled = true;
  out.textContent = 'Routing…';
  out.className = 'coord-result';
  try {
    const payload = {
      required_capabilities: caps,
      task: { instructions },
    };
    // Coordinator routing is a mutation (transport F5): verb-derived
    // no-replay. Both lanes gate on peer.use (owner decision 2026-07-11:
    // the coordinator spends this daemon's peer identity, like the
    // per-peer task quick control) — the facade only names the twin.
    const resp = await daemonApi.request('api_coordinator_route', payload);
    const result = resp.body || {};
    if (resp.ok) {
      out.className = 'coord-result ok';
      out.innerHTML =
        `Routed to <span class="coord-peer-id">${escapeHtml(result.peer_id || '?')}</span> ` +
        `(task id <span class="coord-peer-id">${escapeHtml(result.task_id || '?')}</span>).`;
      instrInput.value = '';
    } else if (resp.status === 404 && Array.isArray(result.considered)) {
      out.className = 'coord-result error';
      const considered = result.considered;
      out.innerHTML =
        `No connected peer satisfies all required capabilities.` +
        (considered.length === 0
          ? ` (No peers connected.)`
          : `<div>Considered:${considered
              .map(p => `<span class="coord-considered">${escapeHtml(p)}</span>`)
              .join('')}</div>`);
    } else {
      const msg = result.error || `HTTP ${resp.status}`;
      out.textContent = `Failed: ${msg}`;
      out.className = 'coord-result error';
    }
  } catch (e) {
    out.textContent = `Error: ${e.message}`;
    out.className = 'coord-result error';
  } finally {
    btn.disabled = false;
  }
}

async function initDaemons() {
  // Hydrate the peer list from the server-side PeerRegistry via
  // GET /api/peers. The server already loaded [[peer]] sections
  // from intendant.toml at startup; peers added through the
  // dashboard at runtime are also there. No localStorage fallback
  // — the server is the single source of truth for the peer list
  // now.
  await refreshPeersFromApi();

  // Apply remembered Settings sub-tab now that the DOM is ready.
  applyInitialSettingsSubtab();

  document.querySelectorAll('[data-daemon-pairing-mode]').forEach(button => {
    button.addEventListener('click', () => {
      setDaemonPairingMode(button.getAttribute('data-daemon-pairing-mode') || 'request');
    });
  });
  setDaemonPairingMode('request');

  const inviteCreateBtn = document.getElementById('daemon-invite-create-btn');
  if (inviteCreateBtn) inviteCreateBtn.addEventListener('click', createDaemonInvite);
  const inviteCopyBtn = document.getElementById('daemon-invite-copy-btn');
  if (inviteCopyBtn) inviteCopyBtn.addEventListener('click', copyDaemonInvite);
  const joinBtn = document.getElementById('daemon-join-btn');
  if (joinBtn) joinBtn.addEventListener('click', joinDaemonInvite);
  for (const id of ['daemon-invite-card-url', 'daemon-invite-label', 'daemon-invite-client-name']) {
    const el = document.getElementById(id);
    if (el) {
      el.addEventListener('keydown', e => {
        if (e.key === 'Enter') createDaemonInvite();
      });
    }
  }
  const joinInviteEl = document.getElementById('daemon-join-invite');
  if (joinInviteEl) {
    joinInviteEl.addEventListener('keydown', e => {
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) joinDaemonInvite();
    });
  }
  const joinLabelEl = document.getElementById('daemon-join-label');
  if (joinLabelEl) {
    joinLabelEl.addEventListener('keydown', e => {
      if (e.key === 'Enter') joinDaemonInvite();
    });
  }
  const requestBtn = document.getElementById('daemon-request-btn');
  if (requestBtn) requestBtn.addEventListener('click', requestDaemonAccess);
  const requestCompleteBtn = document.getElementById('daemon-request-complete-btn');
  if (requestCompleteBtn) requestCompleteBtn.addEventListener('click', completeDaemonAccessRequest);
  for (const id of ['daemon-request-target-url', 'daemon-request-label', 'daemon-request-profile']) {
    const el = document.getElementById(id);
    if (el) {
      el.addEventListener('keydown', e => {
        if (e.key === 'Enter') requestDaemonAccess();
      });
    }
  }
  const requestIdEl = document.getElementById('daemon-request-id');
  if (requestIdEl) {
    requestIdEl.addEventListener('keydown', e => {
      if (e.key === 'Enter') completeDaemonAccessRequest();
    });
  }
  const requestsRefreshBtn = document.getElementById('daemon-access-requests-refresh-btn');
  if (requestsRefreshBtn) requestsRefreshBtn.addEventListener('click', loadPeerAccessRequests);
  loadPeerAccessRequests().catch(() => {});
  const identitiesRefreshBtn = document.getElementById('daemon-peer-identities-refresh-btn');
  if (identitiesRefreshBtn) identitiesRefreshBtn.addEventListener('click', loadPeerIdentities);
  loadPeerIdentities().catch(() => {});

  // Wire the add form.
  document.getElementById('daemon-add-btn').addEventListener('click', addDaemon);
  document.getElementById('daemon-add-url').addEventListener('keydown', e => {
    if (e.key === 'Enter') addDaemon();
  });
  document.getElementById('daemon-add-label').addEventListener('keydown', e => {
    if (e.key === 'Enter') addDaemon();
  });
  // Optional via URL row — Enter from this field also fires Add.
  const viaEl = document.getElementById('daemon-add-via');
  if (viaEl) {
    viaEl.addEventListener('keydown', e => {
      if (e.key === 'Enter') addDaemon();
    });
  }
  // Optional browser-TCP-via URL row — same submit-on-Enter. Without
  // this, Enter in the last field of the form quietly does nothing,
  // which users keep interpreting as a broken submit path.
  const browserViaEl = document.getElementById('daemon-add-browser-tcp-via');
  if (browserViaEl) {
    browserViaEl.addEventListener('keydown', e => {
      if (e.key === 'Enter') addDaemon();
    });
  }

  // Legacy bearer controls were removed from the default dashboard UX, but
  // this stays null-safe so older/custom pages with these IDs can still set
  // the localStorage compatibility token.
  const fedInput = document.getElementById('federation-token-input');
  const fedSave = document.getElementById('federation-token-save');
  const fedClear = document.getElementById('federation-token-clear');
  const fedStatus = document.getElementById('federation-token-status');
  if (fedInput && fedSave && fedClear) {
    if (getFederationToken()) {
      fedInput.placeholder = '(token set — type to replace)';
    }
    fedSave.addEventListener('click', () => {
      const v = fedInput.value.trim();
      if (!v) {
        if (fedStatus) {
          fedStatus.textContent = 'Token cannot be empty (use Clear to remove).';
          fedStatus.className = 'daemons-status error';
        }
        return;
      }
      setFederationToken(v);
      fedInput.value = '';
      fedInput.placeholder = '(token set — type to replace)';
      if (fedStatus) {
        fedStatus.textContent = 'Saved. Reload the dashboard to reconnect /ws with the new token.';
        fedStatus.className = 'daemons-status ok';
      }
    });
    fedClear.addEventListener('click', () => {
      setFederationToken('');
      fedInput.value = '';
      fedInput.placeholder = '(no token configured)';
      if (fedStatus) {
        fedStatus.textContent = 'Cleared. Reload the dashboard to reconnect /ws unauthenticated.';
        fedStatus.className = 'daemons-status';
      }
    });
    fedInput.addEventListener('keydown', e => {
      if (e.key === 'Enter') fedSave.click();
    });
  }

  // Wire the Coordinator panel. Sibling of the daemon-add wiring above:
  // both belong to the Network settings pane and are wired here so the
  // listeners attach exactly once after the DOM is ready.
  document.getElementById('coord-eligible-btn').addEventListener('click', findEligiblePeers);
  document.getElementById('coord-eligible-caps').addEventListener('keydown', e => {
    if (e.key === 'Enter') findEligiblePeers();
  });
  document.getElementById('coord-route-btn').addEventListener('click', routeTask);
  document.getElementById('coord-route-caps').addEventListener('keydown', e => {
    if (e.key === 'Enter') routeTask();
  });
  // Cmd/Ctrl+Enter in the multi-line instructions textarea fires Route
  // — plain Enter is reserved for newlines so the user can format the
  // instructions across multiple lines.
  document.getElementById('coord-route-instructions').addEventListener('keydown', e => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      routeTask();
    }
  });
}

