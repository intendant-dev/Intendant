// Item F4: pointer moves ('mm') deliberately dropped by displayInput's
// bufferedAmount watermark instead of queueing behind a congested shared
// tunnel. Page-session counter, surfaced via qa.liveDisplay().
let dashboardControlTunnelPointerMovesDropped = 0;

class DashboardControlTransport {
  constructor() {
    this.pc = null;
    this.channel = null;
    this.sessionId = '';
    this.binding = null;
    this.verifiedBinding = null;
    this.claimedDaemonPublicKey = '';
    this.sessionGrantSha256 = '';
    this.clientNonce = '';
    this.expiresUnixMs = 0;
    this.localOfferSdp = '';
    this.lastStatus = null;
    this.lastError = '';
    this.lastErrorKind = '';
    this.iceRoute = '';
    this.iceCandidatePair = '';
    this.pendingIce = [];
    this.pending = new Map();
    this.chunkedResponses = new Map();
    this.byteStreams = new Map();
    this.completedChunkedResponses = 0;
    this.completedByteStreams = 0;
    this.signalingMode = '';
    this.connectCsrfToken = '';
    this.seq = 0;
    this.primaryDashboardControl = true;
    this.suppressReconnect = false;
  }

  async connect() {
    this.lastError = '';
    this.lastErrorKind = '';
    dashboardSetControlLastError('');
    dashboardUpdateTransportStatus();
    const iceServers = buildIceServersFromGatewayConfig(gatewayConfig);
    this.pc = new RTCPeerConnection({ iceServers });
    this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
    this.channel.onopen = () => this.handleOpen();
    this.channel.onmessage = ev => this.handleMessage(ev.data);
    this.channel.onclose = () => {
      console.info('[dashboard-control] channel closed');
      dashboardUpdateTransportStatus();
      this.scheduleReconnect('DataChannel closed', { delayMs: 1000 });
    };
    this.channel.onerror = () => {
      this.lastError = 'DataChannel error';
      this.lastErrorKind = 'transport';
      dashboardSetControlLastError(this.lastError, this.lastErrorKind);
      dashboardUpdateTransportStatus();
      this.scheduleReconnect(this.lastError, { delayMs: 1000 });
    };
    this.pc.onicecandidate = ev => {
      if (!ev.candidate) return;
      const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
      if (!this.sessionId) {
        this.pendingIce.push(candidate);
        return;
      }
      this.sendIce(candidate).catch(err => console.warn('[dashboard-control] ICE signal failed', err));
    };
    this.pc.onconnectionstatechange = () => {
      const state = this.pc?.connectionState || 'closed';
      console.info('[dashboard-control] pc state', state);
      // A peer connection that reaches `failed` did get signaling through
      // (there was something to fail); the loss is ICE/DTLS-level. Record
      // that durably on this transport so status renders long after the
      // event still classify the failure honestly.
      if (state === 'failed' && !this.lastErrorKind) this.lastErrorKind = 'transport';
      this.refreshIceRoute().catch(err => console.debug('[dashboard-control] route stats failed', err));
      dashboardUpdateTransportStatus();
      if (state === 'failed' || state === 'closed') {
        this.scheduleReconnect(`WebRTC ${state}`, { delayMs: 1000 });
      } else if (state === 'disconnected') {
        this.scheduleReconnect('WebRTC disconnected', { delayMs: 3000 });
      }
    };
    this.pc.oniceconnectionstatechange = () => {
      this.refreshIceRoute().catch(err => console.debug('[dashboard-control] ICE route stats failed', err));
      dashboardUpdateTransportStatus();
    };
    this.clientNonce = dashboardRandomBase64Url(32);
    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    this.localOfferSdp = offer.sdp || '';
    dashboardUpdateTransportStatus();
    const answer = await this.sendOffer(this.localOfferSdp);
    if (answer) await this.handleAnswer(answer);
  }

  async handleAnswer(answer) {
    this.sessionId = String(answer.session_id || '');
    this.binding = answer.binding || null;
    const claimedDaemonPublicKey = String(answer.daemon_public_key || '');
    if (this.signalingMode === 'connect-rendezvous' && !claimedDaemonPublicKey) {
      this.lastError = 'Connect answer missing daemon public key';
      dashboardSetControlLastError(this.lastError);
      dashboardUpdateTransportStatus();
      this.close();
      throw new Error('dashboard control binding rejected: Connect answer missing daemon public key');
    }
    const sessionGrant = String(answer.session_grant || '');
    if (this.signalingMode === 'connect-rendezvous' && !sessionGrant) {
      this.lastError = 'Connect answer missing session grant';
      dashboardSetControlLastError(this.lastError);
      dashboardUpdateTransportStatus();
      this.close();
      throw new Error('dashboard control binding rejected: Connect answer missing session grant');
    }
    const verification = await verifyDashboardControlBinding(
      this.binding,
      this.sessionId,
      this.localOfferSdp,
      answer.sdp || '',
      sessionGrant,
      this.clientNonce
    );
    if (!verification.ok) {
      this.lastError = verification.error || 'binding verification failed';
      dashboardSetControlLastError(this.lastError);
      dashboardUpdateTransportStatus();
      this.close();
      throw new Error(`dashboard control binding rejected: ${verification.error || 'unknown'}`);
    }
    if (claimedDaemonPublicKey && String(verification.daemonPublicKey || '') !== claimedDaemonPublicKey) {
      this.lastError = 'Connect daemon public key mismatch';
      dashboardSetControlLastError(this.lastError);
      dashboardUpdateTransportStatus();
      this.close();
      throw new Error('dashboard control binding rejected: Connect daemon public key mismatch');
    }
    this.verifiedBinding = verification;
    this.claimedDaemonPublicKey = claimedDaemonPublicKey || String(verification.daemonPublicKey || '');
    this.sessionGrantSha256 = verification.sessionGrantSha256 || '';
    this.expiresUnixMs = verification.expiresUnixMs || 0;
    await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
    for (const candidate of this.pendingIce.splice(0)) {
      this.sendIce(candidate).catch(err => console.warn('[dashboard-control] queued ICE signal failed', err));
    }
    this.refreshIceRoute().catch(() => {});
    dashboardUpdateTransportStatus();
  }

  async handleIceCandidate(candidate) {
    if (!this.pc || !candidate) return;
    try {
      await this.pc.addIceCandidate(candidate);
    } catch (err) {
      console.warn('[dashboard-control] addIceCandidate failed', err);
    }
  }

  handleError(error) {
    console.warn('[dashboard-control] signaling error', error);
    this.lastError = String(error || 'signaling error');
    this.lastErrorKind = 'signaling';
    dashboardSetControlLastError(this.lastError, this.lastErrorKind);
    dashboardUpdateTransportStatus();
    this.close();
    this.scheduleReconnect(this.lastError, { delayMs: 1000 });
  }

  scheduleReconnect(reason, options = {}) {
    // Reconnect whenever this tunnel is WANTED: the primary event lane
    // (hosted Connect, the macOS-app mTLS posture, the capability
    // fallback — losing the tunnel there means losing events entirely)
    // or the explicit localStorage webrtc-control opt-in (the /ws still
    // carries events, but a lane the user asked for must self-heal
    // rather than sit dead behind a permanently red chip). Reconnect
    // status only narrates on the primary-event chip when the tunnel is
    // that lane. suppressReconnect marks explicit closes (user disable,
    // deliberate teardown before a replacement connect).
    if (!this.primaryDashboardControl || this.suppressReconnect || !dashboardControlTunnelWanted()) return;
    scheduleDashboardConnectReconnect(reason, options);
  }

  handleOpen() {
    this.lastError = '';
    this.lastErrorKind = '';
    dashboardSetControlLastError('');
    this.refreshIceRoute().catch(() => {});
    dashboardUpdateTransportStatus();
    this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'upload_frames', 'terminal_frames', 'presence_frames', 'presence_active_handoff', 'presence_tool_request'] });
    this.ping().catch(() => {});
    this.request('status').then(status => {
      if (status && typeof status === 'object') {
        this.lastStatus = status;
        console.info('[dashboard-control] status RPC ok', status.session_id || '');
        dashboardUpdateTransportStatus();
        // The status frame carries the aggregate `fueled` flag the
        // New Session preflight banner derives from.
        if (typeof updateNewSessionFuelBanner === 'function') updateNewSessionFuelBanner();
      }
    }).catch(err => console.warn('[dashboard-control] status RPC failed', err));
    this.request('config').then(config => {
      if (config && typeof config === 'object') {
        applyGatewayConfig(config);
        console.info('[dashboard-control] config RPC ok', config.provider || '(provider unset)');
      }
    }).catch(err => console.warn('[dashboard-control] config RPC failed', err));
    this.request('api_agent_card').then(card => {
      if (applyAgentCardIdentity(card)) {
        console.info('[dashboard-control] agent card RPC ok', card.id || '');
      }
    }).catch(err => console.warn('[dashboard-control] agent card RPC failed', err));
    this.request('api_dashboard_targets').then(targets => {
      if (targets) applyDashboardAccessTargets(targets);
    }).catch(err => console.warn('[dashboard-control] dashboard targets RPC failed', err));
    this.request('api_access_overview').then(overview => {
      if (overview) applyAccessOverview(overview);
    }).catch(err => console.warn('[dashboard-control] access overview RPC failed', err));
    this.request('subscribe_events').then(result => {
      if (result?.subscribed) {
        dashboardControlEventsActive = true;
        dashboardRecentServerMessageKeys.clear();
        console.info('[dashboard-control] event stream subscribed');
        // In the macOS-app mTLS posture nothing else drives the primary
        // event chip (the legacy WS callbacks never fire); mark the lane
        // live here. Connect mode owns its own chip transitions in the
        // bootstrap/reconnect paths.
        if (this.primaryDashboardControl && !dashboardConnectModeEnabled() && dashboardControlTunnelIsPrimaryEventLane()) {
          setConnectEventStatus('ok', 'Dashboard events are live through the control tunnel');
        }
        dashboardUpdateTransportStatus();
      }
    }).catch(err => console.warn('[dashboard-control] event subscribe failed', err));
  }

  handleMessage(data) {
    let msg;
    try {
      msg = JSON.parse(String(data));
    } catch {
      return;
    }
    this.handleFrame(msg);
  }

  handleFrame(msg) {
    if (msg.t === 'hello_ack') {
      console.info('[dashboard-control] hello_ack', msg.session_id || '');
      // The daemon advertises its control-RPC surface here; readiness
      // checks (vault leases, egress) consult it to distinguish
      // "daemon too old to support this" from "denied for this session".
      this.controlFeatures = Array.isArray(msg.features) ? msg.features : [];
      return;
    }
    if (msg.t === 'egress_request' || msg.t === 'egress_request_chunk' ||
        msg.t === 'egress_request_end' || msg.t === 'egress_ack' || msg.t === 'egress_cancel') {
      vaultEgressHandleFrame(msg);
      return;
    }
    if (msg.t === 'terminal_output' || msg.t === 'terminal_exited' || msg.t === 'terminal_opened' || msg.t === 'terminal_error' || msg.t === 'terminal_shared') {
      try {
        window.dispatchEvent(new CustomEvent('intendant-dashboard-terminal-frame', { detail: msg }));
      } catch (_) {}
      if (msg.t === 'terminal_opened' && shellFrameMatchesCurrent(msg.host_id, msg.terminal_id)) {
        handleShellOpened(msg);
        return;
      }
      if (msg.t === 'terminal_shared' && shellFrameMatchesCurrent(msg.host_id, msg.terminal_id)) {
        handleShellShared(msg);
        return;
      }
      if (msg.t === 'terminal_error' && shellFrameMatchesCurrent(msg.host_id, msg.terminal_id)) {
        handleShellError(msg.error);
        return;
      }
      if (msg.t === 'terminal_output' && shellFrameMatchesCurrent(msg.host_id, msg.terminal_id)) {
        handleShellOutput(msg.data);
        return;
      }
      if (msg.t === 'terminal_exited' && shellFrameMatchesCurrent(msg.host_id, msg.terminal_id)) {
        handleShellExited(msg.status);
        return;
      }
      if (dashboardServerMessageDispatcher) {
        // Pass the already-parsed frame: the dispatcher and the WASM
        // handoff accept objects (the tunnel event path below has always
        // passed payload objects) — re-stringifying here forced a
        // parse → stringify → parse round trip per PTY output frame.
        dashboardServerMessageDispatcher(msg);
      }
      return;
    }
    if (msg.t === 'event') {
      const payload = msg.payload || {};
      if (dashboardServerMessageDispatcher) {
        dashboardServerMessageDispatcher(payload);
      } else {
        console.debug('[dashboard-control] event', payload.event || payload.t || '', payload);
      }
      return;
    }
    if (msg.t === 'event_gap') {
      // Only the primary transport's gaps drive the recovery UX; a peer
      // tunnel's gap is that peer pane's concern, not the local chip's.
      if (this.primaryDashboardControl) {
        dashboardHandleEventGap(msg, 'tunnel');
      } else {
        console.warn('[dashboard-control] peer event gap', msg.skipped || 0);
      }
      dashboardUpdateTransportStatus();
      return;
    }
    if (msg.t === 'response_start') {
      this.handleResponseStart(msg);
      return;
    }
    if (msg.t === 'response_chunk') {
      this.handleResponseChunk(msg);
      return;
    }
    if (msg.t === 'response_end') {
      this.handleResponseEnd(msg);
      return;
    }
    if (msg.t === 'byte_stream_start') {
      this.handleByteStreamStart(msg);
      return;
    }
    if (msg.t === 'byte_stream_chunk') {
      this.handleByteStreamChunk(msg);
      return;
    }
    if (msg.t === 'byte_stream_end') {
      this.handleByteStreamEnd(msg);
      return;
    }
    if (msg.t === 'stream_start') {
      this.handleStreamStart(msg);
      return;
    }
    if (msg.t === 'stream_event') {
      this.handleStreamEvent(msg);
      return;
    }
    if (msg.t === 'stream_end') {
      this.handleStreamEnd(msg);
      return;
    }
    if (msg.t === 'pong' || msg.t === 'response') {
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) {
        pending.reject(dashboardControlAbortError(msg.error || 'dashboard control request cancelled'));
        return;
      }
      if (msg.t === 'response' && msg.ok === false) {
        pending.reject(new Error(msg.error || 'dashboard control request failed'));
      } else {
        pending.resolve(msg.t === 'pong' ? msg : msg.result);
      }
    }
  }

  handleResponseStart(msg) {
    const id = String(msg.id || '');
    const chunkKey = String(msg.chunk_id || id);
    if (!id || !chunkKey || !this.pending.has(id)) return;
    const totalBytes = Number(msg.total_bytes);
    const expectedChunks = Number(msg.chunks);
    if (
      msg.encoding !== 'base64-json-frame' ||
      !Number.isSafeInteger(totalBytes) ||
      totalBytes < 0 ||
      totalBytes > DASHBOARD_CONTROL_MAX_CHUNKED_RESPONSE_BYTES ||
      !Number.isSafeInteger(expectedChunks) ||
      expectedChunks < 0
    ) {
      this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response header');
      return;
    }
    this.chunkedResponses.set(chunkKey, {
      id,
      totalBytes,
      expectedChunks,
      receivedBytes: 0,
      chunks: new Map(),
      ended: false,
      finalChunks: null,
    });
    dashboardUpdateTransportStatus();
  }

  handleResponseChunk(msg) {
    const id = String(msg.id || '');
    const chunkKey = String(msg.chunk_id || id);
    const state = this.chunkedResponses.get(chunkKey);
    if (!state) return;
    const seq = Number(msg.seq);
    if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
      this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk sequence');
      return;
    }
    if (state.chunks.has(seq)) return;
    let bytes;
    try {
      bytes = dashboardControlBase64ToBytes(String(msg.data || ''));
    } catch {
      this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk encoding');
      return;
    }
    state.chunks.set(seq, bytes);
    state.receivedBytes += bytes.byteLength;
    if (state.receivedBytes > state.totalBytes) {
      this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response exceeded declared size');
      return;
    }
    const completed = this.maybeCompleteChunkedResponse(chunkKey);
    if (!completed && this.chunkedResponses.has(chunkKey)) {
      this.sendChunkCredit(id, 1, chunkKey === id ? null : chunkKey);
    }
    // Per-chunk progress: coalesced tick. Completion/rejection inside
    // maybeCompleteChunkedResponse already rendered immediately.
    dashboardScheduleTransportStatusUpdate();
  }

  handleResponseEnd(msg) {
    const id = String(msg.id || '');
    const chunkKey = String(msg.chunk_id || id);
    const state = this.chunkedResponses.get(chunkKey);
    if (!state) return;
    const finalChunks = Number(msg.chunks);
    if (
      !Number.isSafeInteger(finalChunks) ||
      finalChunks !== state.expectedChunks
    ) {
      this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response footer');
      return;
    }
    state.ended = true;
    state.finalChunks = finalChunks;
    this.maybeCompleteChunkedResponse(chunkKey);
    dashboardUpdateTransportStatus();
  }

  maybeCompleteChunkedResponse(chunkKey) {
    const state = this.chunkedResponses.get(chunkKey);
    if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
    const merged = new Uint8Array(state.totalBytes);
    let offset = 0;
    for (let seq = 0; seq < state.expectedChunks; seq += 1) {
      const chunk = state.chunks.get(seq);
      if (!chunk) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response missed a chunk');
        return;
      }
      merged.set(chunk, offset);
      offset += chunk.byteLength;
    }
    if (offset !== state.totalBytes) {
      this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response size mismatch');
      return;
    }
    this.chunkedResponses.delete(chunkKey);
    let frame;
    try {
      frame = JSON.parse(new TextDecoder().decode(merged));
    } catch {
      this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response was not valid JSON');
      return;
    }
    if (!['response', 'stream_event'].includes(frame.t) || String(frame.id || '') !== state.id) {
      this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response id mismatch');
      return;
    }
    this.completedChunkedResponses += 1;
    dashboardUpdateTransportStatus();
    this.handleFrame(frame);
    return true;
  }

  rejectChunkedResponse(chunkKey, message) {
    const state = this.chunkedResponses.get(chunkKey);
    const id = state?.id || chunkKey;
    this.chunkedResponses.delete(chunkKey);
    const pending = this.pending.get(id);
    if (pending) {
      this.pending.delete(id);
      pending.reject(new Error(message));
    }
    dashboardUpdateTransportStatus();
  }

  handleByteStreamStart(msg) {
    const id = String(msg.id || '');
    const streamId = String(msg.stream_id || id);
    if (!id || !streamId || !this.pending.has(id)) return;
    const totalBytes = Number(msg.total_bytes);
    const expectedChunks = Number(msg.chunks);
    if (
      msg.encoding !== 'base64' ||
      !Number.isSafeInteger(totalBytes) ||
      totalBytes < 0 ||
      totalBytes > DASHBOARD_CONTROL_MAX_BYTE_STREAM_BYTES ||
      !Number.isSafeInteger(expectedChunks) ||
      expectedChunks < 0
    ) {
      this.rejectByteStream(streamId, 'invalid dashboard control byte stream header', id);
      return;
    }
    this.byteStreams.set(streamId, {
      id,
      streamId,
      totalBytes,
      expectedChunks,
      receivedBytes: 0,
      chunks: new Map(),
      ended: false,
      finalChunks: null,
      result: null,
      contentType: String(msg.content_type || 'application/octet-stream'),
      filename: msg.filename ? String(msg.filename) : '',
    });
    dashboardUpdateTransportStatus();
  }

  handleByteStreamChunk(msg) {
    const id = String(msg.id || '');
    const streamId = String(msg.stream_id || id);
    const state = this.byteStreams.get(streamId);
    if (!state) return;
    const seq = Number(msg.seq);
    if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
      this.rejectByteStream(streamId, 'invalid dashboard control byte stream chunk sequence');
      return;
    }
    if (state.chunks.has(seq)) return;
    let bytes;
    try {
      bytes = dashboardControlBase64ToBytes(String(msg.data || ''));
    } catch {
      this.rejectByteStream(streamId, 'invalid dashboard control byte stream encoding');
      return;
    }
    state.chunks.set(seq, bytes);
    state.receivedBytes += bytes.byteLength;
    if (state.receivedBytes > state.totalBytes) {
      this.rejectByteStream(streamId, 'dashboard control byte stream exceeded declared size');
      return;
    }
    const completed = this.maybeCompleteByteStream(streamId);
    if (!completed && this.byteStreams.has(streamId)) {
      this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
    }
    // Per-chunk progress: coalesced tick (transitions render immediately).
    dashboardScheduleTransportStatusUpdate();
  }

  handleByteStreamEnd(msg) {
    const id = String(msg.id || '');
    const streamId = String(msg.stream_id || id);
    const state = this.byteStreams.get(streamId);
    if (!state) return;
    if (msg.ok === false) {
      this.rejectByteStream(streamId, msg.error || 'dashboard control byte stream failed');
      return;
    }
    const finalChunks = Number(msg.chunks);
    if (
      !Number.isSafeInteger(finalChunks) ||
      finalChunks !== state.expectedChunks
    ) {
      this.rejectByteStream(streamId, 'invalid dashboard control byte stream footer');
      return;
    }
    state.ended = true;
    state.finalChunks = finalChunks;
    state.result = msg.result || null;
    this.maybeCompleteByteStream(streamId);
    dashboardUpdateTransportStatus();
  }

  maybeCompleteByteStream(streamId) {
    const state = this.byteStreams.get(streamId);
    if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
    const merged = new Uint8Array(state.totalBytes);
    let offset = 0;
    for (let seq = 0; seq < state.expectedChunks; seq += 1) {
      const chunk = state.chunks.get(seq);
      if (!chunk) {
        this.rejectByteStream(streamId, 'dashboard control byte stream missed a chunk');
        return;
      }
      merged.set(chunk, offset);
      offset += chunk.byteLength;
    }
    if (offset !== state.totalBytes) {
      this.rejectByteStream(streamId, 'dashboard control byte stream size mismatch');
      return;
    }
    this.byteStreams.delete(streamId);
    const pending = this.pending.get(state.id);
    if (!pending) return true;
    const result = state.result && typeof state.result === 'object' && !Array.isArray(state.result)
      ? { ...state.result }
      : {};
    result.ok = result.ok !== false;
    result.bytes = merged;
    result.size = state.totalBytes;
    result.content_type = result.content_type || state.contentType;
    result.filename = result.filename || state.filename;
    result.stream_id = state.streamId;
    this.completedByteStreams += 1;
    this.pending.delete(state.id);
    this.deleteChunkedResponsesForRequest(state.id);
    pending.resolve(result);
    dashboardUpdateTransportStatus();
    return true;
  }

  rejectByteStream(streamId, message, requestId = '') {
    const state = this.byteStreams.get(streamId);
    const id = state?.id || requestId || streamId;
    this.byteStreams.delete(streamId);
    const pending = this.pending.get(id);
    if (pending) {
      this.pending.delete(id);
      pending.reject(new Error(message));
    }
    dashboardUpdateTransportStatus();
  }

  handleStreamStart(msg) {
    const pending = this.pending.get(String(msg.id || ''));
    const stream = pending?.stream;
    if (!stream) return;
    stream.started = true;
    this.callStreamCallback(stream, 'start', msg);
  }

  handleStreamEvent(msg) {
    const pending = this.pending.get(String(msg.id || ''));
    const stream = pending?.stream;
    if (!stream) return;
    stream.eventCount += 1;
    this.callStreamCallback(stream, 'event', msg.event, msg);
  }

  handleStreamEnd(msg) {
    const id = String(msg.id || '');
    const pending = this.pending.get(id);
    const stream = pending?.stream;
    if (!pending || !stream) return;
    this.pending.delete(id);
    if (msg.ok === false) {
      pending.reject(new Error(msg.error || 'dashboard control stream failed'));
      return;
    }
    this.callStreamCallback(stream, 'end', msg.result || null, msg);
    pending.resolve(msg.result || null);
  }

  callStreamCallback(stream, name, ...args) {
    const callbacks = stream.callbacks;
    try {
      if (typeof callbacks === 'function' && name === 'event') {
        callbacks(...args);
      } else if (callbacks && typeof callbacks[name] === 'function') {
        callbacks[name](...args);
      }
    } catch (err) {
      console.warn('[dashboard-control] stream callback failed', err);
    }
  }

  ping() {
    const id = this.nextId();
    const promise = this.waitFor(id, 5000, { label: 'ping' });
    this.sendFrame({ t: 'ping', id });
    return promise;
  }

  request(method, params = {}, options = {}) {
    if (options.signal?.aborted) {
      return Promise.reject(dashboardControlAbortError());
    }
    const id = this.nextId();
    const promise = this.waitFor(id, dashboardControlRequestTimeoutMs(method), {
      ...options,
      method,
    });
    this.sendFrame({ t: 'request', id, method, params });
    return promise;
  }

  requestBytes(method, params = {}, options = {}) {
    if (options.signal?.aborted) {
      return Promise.reject(dashboardControlAbortError());
    }
    const id = this.nextId();
    const promise = this.waitFor(id, options.timeoutMs || dashboardControlRequestTimeoutMs(method), {
      ...options,
      method,
    });
    const pending = this.pending.get(id);
    if (pending) pending.expectBytes = true;
    this.sendFrame({ t: 'request', id, method, params, bytes: true });
    return promise;
  }

  async uploadBytes(method, params = {}, bytes, options = {}) {
    if (options.signal?.aborted) {
      return Promise.reject(dashboardControlAbortError());
    }
    const id = this.nextId();
    const totalBytes = Number(bytes?.size ?? bytes?.byteLength ?? bytes?.length ?? 0);
    const chunkSize = options.chunkBytes || DASHBOARD_CONTROL_UPLOAD_CHUNK_BYTES;
    const chunks = Math.ceil(totalBytes / chunkSize);
    const promise = this.waitFor(id, options.timeoutMs || dashboardControlRequestTimeoutMs(method), {
      ...options,
      method,
    });
    this.sendFrame({
      t: 'upload_start',
      id,
      method,
      params,
      encoding: 'base64',
      total_bytes: totalBytes,
      chunks,
    });
    try {
      for (let seq = 0, offset = 0; offset < totalBytes; seq += 1, offset += chunkSize) {
        if (options.signal?.aborted) throw dashboardControlAbortError();
        if (!this.pending.has(id)) break;
        const end = Math.min(offset + chunkSize, totalBytes);
        let chunk;
        if (bytes instanceof Blob) {
          chunk = new Uint8Array(await bytes.slice(offset, end).arrayBuffer());
        } else if (bytes instanceof Uint8Array) {
          chunk = bytes.subarray(offset, end);
        } else {
          chunk = new Uint8Array(bytes.slice(offset, end));
        }
        this.sendFrame({
          t: 'upload_chunk',
          id,
          seq,
          data: dashboardControlBytesToBase64(chunk),
        });
        await this.waitForBufferedAmountLow(options.signal);
      }
      if (this.pending.has(id)) {
        this.sendFrame({ t: 'upload_end', id, chunks });
      }
    } catch (err) {
      if (this.pending.has(id)) {
        this.sendFrame({ t: 'cancel', id });
      }
      throw err;
    }
    return promise;
  }

  async waitForBufferedAmountLow(signal = null) {
    while (
      this.channel &&
      this.channel.readyState === 'open' &&
      this.channel.bufferedAmount > DASHBOARD_CONTROL_UPLOAD_BUFFER_HIGH_BYTES
    ) {
      if (signal?.aborted) throw dashboardControlAbortError();
      await new Promise(resolve => setTimeout(resolve, 10));
    }
  }

  displayInput(displayId, event) {
    if (!this.canUseRpc()) return false;
    // This channel is reliable+ordered and shared with every RPC,
    // upload, and terminal frame. Continuous latest-wins pointer moves
    // must never queue behind a backlog here — stale moves replayed in
    // order read as catastrophic remote-control lag. Above the
    // watermark, dropping the move is the honest choice (the next one
    // supersedes it); discrete events (kd/ku/md/mu) always send. The
    // per-display lossy `pointer` datachannel is the preferred mm/sc
    // lane anyway (DisplaySlot._enterInteractive) — this path is its
    // fallback.
    if (event?.t === 'mm' &&
        this.channel.bufferedAmount > DASHBOARD_CONTROL_INPUT_MOVE_DROP_BUFFERED_BYTES) {
      dashboardControlTunnelPointerMovesDropped += 1;
      return true; // handled: deliberately dropped — never reroute a stale move
    }
    try {
      this.sendFrame({
        t: 'display_input',
        display_id: Number(displayId) || 0,
        event,
      });
    } catch (_) {
      // Close race / full SCTP buffer: report unsent so the slot can
      // fall back to its own data channels.
      return false;
    }
    return true;
  }

  terminalFrame(frame) {
    if (!this.canUseRpc()) return false;
    this.sendFrame(frame);
    return true;
  }

  presenceFrame(frame) {
    if (!this.canUseRpc()) return false;
    this.sendFrame({ t: 'presence_frame', frame });
    return true;
  }

  stream(method, params = {}, options = {}, onEvent = {}) {
    if (options.signal?.aborted) {
      return Promise.reject(dashboardControlAbortError());
    }
    const id = this.nextId();
    const promise = this.waitFor(id, options.timeoutMs || dashboardControlRequestTimeoutMs(method), {
      ...options,
      method,
    });
    const pending = this.pending.get(id);
    if (pending) {
      pending.stream = {
        callbacks: onEvent,
        eventCount: 0,
        started: false,
      };
    }
    this.sendFrame({ t: 'request', id, method, params, stream: true });
    return promise;
  }

  canUseRpc() {
    return Boolean(
      this.verifiedBinding &&
      this.pc?.connectionState === 'connected' &&
      this.channel?.readyState === 'open'
    );
  }

  waitFor(id, timeoutMs = 5000, options = {}) {
    return new Promise((resolve, reject) => {
      let settled = false;
      const signal = options.signal || null;
      const fail = (err, sendCancel = false) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
        this.pending.delete(id);
        this.deleteChunkedResponsesForRequest(id);
        this.deleteByteStreamsForRequest(id);
        if (sendCancel) this.sendFrame({ t: 'cancel', id });
        reject(err);
      };
      const abortHandler = signal ? () => fail(dashboardControlAbortError(), true) : null;
      const label = String(options.label || options.method || id || 'request');
      const timer = setTimeout(() => {
        fail(new Error(`${label} dashboard control request timed out`), true);
      }, timeoutMs);
      if (signal && abortHandler) {
        signal.addEventListener('abort', abortHandler, { once: true });
      }
      this.pending.set(id, {
        resolve: value => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
          this.deleteChunkedResponsesForRequest(id);
          this.deleteByteStreamsForRequest(id);
          resolve(value);
        },
        reject: err => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
          this.deleteChunkedResponsesForRequest(id);
          this.deleteByteStreamsForRequest(id);
          reject(err);
        },
      });
    });
  }

  deleteChunkedResponsesForRequest(id) {
    for (const [chunkKey, state] of this.chunkedResponses) {
      if (chunkKey === id || state?.id === id) {
        this.chunkedResponses.delete(chunkKey);
      }
    }
  }

  deleteByteStreamsForRequest(id) {
    for (const [streamId, state] of this.byteStreams) {
      if (streamId === id || state?.id === id) {
        this.byteStreams.delete(streamId);
      }
    }
  }

  sendFrame(frame) {
    if (!this.channel || this.channel.readyState !== 'open') return;
    this.channel.send(JSON.stringify(frame));
  }

  sendChunkCredit(id, chunks, chunkId = null) {
    const frame = { t: 'credit', id, chunks };
    if (chunkId) frame.chunk_id = chunkId;
    this.sendFrame(frame);
  }

  async postLocalSignal(path, payload) {
    const resp = await fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const body = await resp.json().catch(() => ({}));
    if (!resp.ok || body.ok === false) {
      throw new Error(body.error || `${path} returned ${resp.status}`);
    }
    return body;
  }

  async postConnectSignal(path, payload) {
    const headers = await this.connectSignalHeaders();
    const resp = await fetch(dashboardConnectSignalUrl(path), {
      method: 'POST',
      headers,
      body: JSON.stringify(payload),
    });
    const body = await resp.json().catch(() => ({}));
    if (!resp.ok || body.ok === false) {
      const err = new Error(body.error || `${path} returned ${resp.status}`);
      // The rendezvous encodes who failed in the status: callers use it to
      // tell a daemon-authored refusal from "no answer ever came back".
      err.connectHttpStatus = resp.status;
      throw err;
    }
    return body;
  }

  /* One cached /api/me fetch serves both the CSRF header and the signed
     account claim on v2 offers. */
  async connectMe() {
    if (!this.connectMeInfo) {
      const resp = await fetch('/api/me');
      this.connectMeInfo = await resp.json().catch(() => ({}));
    }
    return this.connectMeInfo || {};
  }

  async connectSignalHeaders() {
    if (!this.connectCsrfToken) {
      const me = await this.connectMe();
      this.connectCsrfToken = String(me.csrf_token || '');
    }
    const headers = { 'content-type': 'application/json' };
    if (this.connectCsrfToken) headers['x-intendant-csrf'] = this.connectCsrfToken;
    return headers;
  }

  sendWsSignal(frame) {
    if (app && app.send_server_action) {
      // send_server_action reports whether the frame was handed to an OPEN
      // legacy /ws socket (false: socket missing, not open, or the send
      // threw). In the macOS-app mTLS posture that socket never connects,
      // so a refused send must fail the handshake immediately instead of
      // letting the caller wait out the 30 s readiness poll on an offer
      // that never left the browser. QA probes stub send_server_action
      // with undefined-returning counters; only an explicit false refuses.
      return app.send_server_action(frame) !== false;
    }
    return false;
  }

  async sendOffer(sdp) {
    if (dashboardConnectModeEnabled()) {
      if (!DASHBOARD_CONNECT_DAEMON_ID) {
        throw new Error('Connect dashboard missing daemon_id');
      }
      // Sign the browser's own account claim into the offer (v2) so the
      // daemon's pending-enrollment card can show an account attested by
      // this device key rather than asserted by the relay.
      const me = await this.connectMe().catch(() => ({}));
      const account = me?.authenticated && me?.user?.id
        ? { userId: String(me.user.id), name: String(me.user.account_name || '') }
        : null;
      const identity = await clientIdentityOfferFields(
        DASHBOARD_CONNECT_DAEMON_ID,
        this.clientNonce,
        sdp,
        account
      );
      // A stored org grant rides along so a daemon that trusts the org
      // materializes it before resolving this very offer (one-round-trip
      // first contact). The daemon re-verifies everything.
      const orgGrant = await orgGrantForOffer(DASHBOARD_CONNECT_DAEMON_ID);
      let answer;
      try {
        answer = await this.postConnectSignal('/api/browser/offer', {
          daemon_id: DASHBOARD_CONNECT_DAEMON_ID,
          sdp,
          client_nonce: this.clientNonce,
          tab_id: INTENDANT_TAB_ID,
          ...identity,
          ...(orgGrant ? { org_grant: orgGrant } : {}),
        });
      } catch (err) {
        // 502 relays the daemon's own words: it received the offer and
        // posted an explicit refusal back through the rendezvous (the one
        // exception is the service-internal "daemon answer channel
        // closed"). Everything else — 504 offer timeout, 404 unknown
        // daemon, a fetch failure — means no answer ever arrived.
        const daemonSpoke = err?.connectHttpStatus === 502 &&
          !/daemon answer channel closed/i.test(String(err?.message || ''));
        err.controlErrorKind = daemonSpoke ? 'refused' : 'signaling';
        throw err;
      }
      this.signalingMode = 'connect-rendezvous';
      dashboardUpdateTransportStatus();
      return answer;
    }
    try {
      const identity = await clientIdentityOfferFields('', this.clientNonce, sdp);
      const orgGrant = await orgGrantForOffer('');
      const answer = await this.postLocalSignal('/connect/dashboard/offer', {
        sdp,
        client_nonce: this.clientNonce,
        tab_id: INTENDANT_TAB_ID,
        ...identity,
        ...(orgGrant ? { org_grant: orgGrant } : {}),
      });
      if (answer?.org_grant_error) {
        console.warn('[dashboard-control] offer org grant not accepted:', answer.org_grant_error);
      }
      this.signalingMode = 'local-http';
      dashboardUpdateTransportStatus();
      return answer;
    } catch (err) {
      console.warn('[dashboard-control] local offer signaling failed', err);
      if (this.sendWsSignal({ t: 'dashboard_control_offer', sdp, client_nonce: this.clientNonce, tab_id: INTENDANT_TAB_ID })) {
        this.signalingMode = 'websocket-fallback';
        dashboardUpdateTransportStatus();
        return null;
      }
      // Both signaling lanes refused (local HTTP failed, no open /ws for
      // the fallback): the offer never left the browser. Classify as a
      // signaling failure — connect() rejects, startControl resolves
      // false, and the reconnect runner fails the cycle right away.
      if (!err.controlErrorKind) err.controlErrorKind = 'signaling';
      throw err;
    }
  }

  async sendIce(candidate) {
    if (!this.sessionId) return;
    if (this.signalingMode === 'connect-rendezvous') {
      await this.postConnectSignal('/api/browser/ice', {
        daemon_id: DASHBOARD_CONNECT_DAEMON_ID,
        session_id: this.sessionId,
        candidate,
      });
      return;
    }
    if (this.signalingMode !== 'websocket-fallback') {
      try {
        await this.postLocalSignal('/connect/dashboard/ice', {
          session_id: this.sessionId,
          candidate,
        });
        return;
      } catch (err) {
        console.warn('[dashboard-control] local ICE signaling failed', err);
        if (this.signalingMode === 'local-http') throw err;
      }
    }
    if (!this.sendWsSignal({ t: 'dashboard_control_ice', session_id: this.sessionId, candidate })) {
      throw new Error('dashboard control signaling unavailable');
    }
  }

  signalClose(sessionId) {
    if (!sessionId) return;
    if (this.signalingMode === 'connect-rendezvous') {
      const headers = { 'content-type': 'application/json' };
      if (this.connectCsrfToken) headers['x-intendant-csrf'] = this.connectCsrfToken;
      fetch(dashboardConnectSignalUrl('/api/browser/close'), {
        method: 'POST',
        headers,
        body: JSON.stringify({
          daemon_id: DASHBOARD_CONNECT_DAEMON_ID,
          session_id: sessionId,
        }),
      }).catch(() => {});
      return;
    }
    if (this.signalingMode !== 'websocket-fallback') {
      fetch('/connect/dashboard/close', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: sessionId }),
      }).catch(() => {});
      return;
    }
    this.sendWsSignal({ t: 'dashboard_control_close', session_id: sessionId });
  }

  async refreshIceRoute() {
    if (!this.pc || this.pc.connectionState !== 'connected') return;
    const stats = await this.pc.getStats();
    let selectedPair = null;
    stats.forEach(report => {
      if (report.type === 'transport' && report.selectedCandidatePairId) {
        selectedPair = stats.get(report.selectedCandidatePairId) || selectedPair;
      }
    });
    if (!selectedPair) {
      stats.forEach(report => {
        if (
          report.type === 'candidate-pair' &&
          (report.selected || report.nominated) &&
          (!report.state || report.state === 'succeeded')
        ) {
          selectedPair = report;
        }
      });
    }
    if (!selectedPair) return;
    const local = stats.get(selectedPair.localCandidateId);
    const remote = stats.get(selectedPair.remoteCandidateId);
    const localType = String(local?.candidateType || '').toLowerCase();
    const remoteType = String(remote?.candidateType || '').toLowerCase();
    const pair = [localType, remoteType].filter(Boolean).join(' -> ');
    this.iceCandidatePair = pair;
    this.iceRoute = localType === 'relay' || remoteType === 'relay' ? 'relay' : 'direct';
    dashboardUpdateTransportStatus();
  }

  close(options = {}) {
    if (options.suppressReconnect) this.suppressReconnect = true;
    for (const pending of this.pending.values()) {
      pending.reject(new Error('dashboard control transport closed'));
    }
    this.pending.clear();
    this.chunkedResponses.clear();
    this.byteStreams.clear();
    if (this.sessionId && options.signalRemote !== false) {
      this.signalClose(this.sessionId);
    }
    dashboardControlEventsActive = false;
    dashboardRecentServerMessageKeys.clear();
    try { this.channel?.close(); } catch {}
    try { this.pc?.close(); } catch {}
    dashboardUpdateTransportStatus();
  }

  debugStatus() {
    const connected = this.pc?.connectionState === 'connected';
    // Availability booleans, derived by iterating the daemon's status
    // frame instead of hand-copying every `*_available` flag (browser-side
    // "derive, don't mirror"): each snake_case `*_available` key the
    // daemon reports appears as its camelCase twin (dashboardControlCamelKey
    // preserves the historical `WebRtc` hump). Keys the daemon does not
    // report simply don't appear — consumers already treat missing and
    // null alike as "unknown".
    const availability = {};
    if (this.lastStatus && typeof this.lastStatus === 'object') {
      for (const [key, value] of Object.entries(this.lastStatus)) {
        if (!key.endsWith('_available')) continue;
        availability[dashboardControlCamelKey(key)] = value ?? null;
      }
    }
    return {
      enabled: dashboardControlTransportEnabled(),
      mode: connected && this.verifiedBinding ? 'webrtc-control' : 'checking',
      connected,
      pcState: this.pc?.connectionState || '',
      channelState: this.channel?.readyState || '',
      controlFeatures: this.controlFeatures || [],
      sessionId: this.sessionId,
      verifiedBinding: this.verifiedBinding,
      claimedDaemonPublicKey: this.claimedDaemonPublicKey,
      sessionGrantSha256: this.sessionGrantSha256,
      clientNonce: this.clientNonce,
      expiresUnixMs: this.expiresUnixMs,
      signalingMode: this.signalingMode,
      iceRoute: this.iceRoute,
      iceCandidatePair: this.iceCandidatePair,
      lastError: this.lastError,
      lastErrorKind: this.lastErrorKind,
      eventsActive: dashboardControlEventsActive,
      grantKind: this.lastStatus?.grant_kind ?? null,
      grantLabel: this.lastStatus?.grant_label ?? null,
      accessPrincipal: this.lastStatus?.access_principal ?? null,
      ...availability,
      pendingRequests: this.pending.size,
      pendingChunkedResponses: this.chunkedResponses.size,
      pendingByteStreams: this.byteStreams.size,
      completedChunkedResponses: this.completedChunkedResponses,
      completedByteStreams: this.completedByteStreams,
      ...dashboardConnectReconnectStatus(),
    };
  }

  nextId() {
    this.seq += 1;
    return `dc-${Date.now()}-${this.seq}`;
  }
}

class PeerDashboardControlConnection extends DashboardControlTransport {
  constructor(hostId, sessionId = '') {
    super();
    this.primaryDashboardControl = false;
    this.hostId = String(hostId || '').trim();
    this.sessionId = String(sessionId || generateSessionId());
    this.signalingMode = 'peer-primary';
    this.connectPromise = null;
    this._readyResolve = null;
    this._readyReject = null;
  }

  sessionKey() {
    return `${this.hostId}|${this.sessionId}`;
  }

  connect(options = {}) {
    if (this.connectPromise) return this.connectPromise;
    this.connectPromise = this._connect(options).catch(err => {
      this.lastError = err?.message || String(err);
      this.close({ signalRemote: false });
      throw err;
    });
    return this.connectPromise;
  }

  async _connect(options = {}) {
    if (!this.hostId) throw new Error('peer id is required');
    if (!window.RTCPeerConnection) throw new Error('RTCPeerConnection is unavailable');
    const peer = daemons.find(d => String(d.host_id || '') === this.hostId);
    if (!peer) throw new Error('Unknown daemon host');
    if (peer.connected === false) throw new Error('Selected peer is not connected');
    this.advertiseTcpViaUrl = resolveBrowserTcpViaUrl(peer) || '';

    peerDashboardControlConnections.set(this.sessionKey(), this);
    peerDashboardControlConnectionsByHost.set(this.hostId, this);

    const iceServers = buildIceServersFromGatewayConfig(gatewayConfig);
    this.pc = new RTCPeerConnection({ iceServers });
    this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
    const ready = this.waitForReady(options);

    this.channel.onopen = () => this.handleOpen();
    this.channel.onmessage = ev => this.handleMessage(ev.data);
    this.channel.onclose = () => {
      console.debug(`[peer-dashboard-control ${this.hostId}] channel closed`);
    };
    this.channel.onerror = () => {
      const err = new Error('peer dashboard-control DataChannel error');
      this.lastError = err.message;
      this._readyReject?.(err);
    };
    this.pc.onicecandidate = ev => {
      if (!ev.candidate) {
        console.debug(`[peer-dashboard-control ${this.hostId}] local ICE gathering complete`);
        return;
      }
      const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
      console.debug(`[peer-dashboard-control ${this.hostId}] local ICE candidate: ${this.describeCandidate(candidate)}`);
      this.sendIce(candidate).catch(err =>
        console.warn(`[peer-dashboard-control ${this.hostId}] ICE signal failed`, err)
      );
    };
    this.pc.onconnectionstatechange = () => {
      const state = this.pc?.connectionState || 'closed';
      console.debug(`[peer-dashboard-control ${this.hostId}] pc state ${state}`);
      this.refreshIceRoute().catch(() => {});
      if (state === 'failed') {
        const err = new Error('peer dashboard-control WebRTC connection failed');
        this.lastError = err.message;
        this._readyReject?.(err);
      }
    };
    this.pc.oniceconnectionstatechange = () => {
      console.debug(`[peer-dashboard-control ${this.hostId}] iceConnectionState=${this.pc?.iceConnectionState || 'closed'}`);
      this.refreshIceRoute().catch(() => {});
    };

    this.clientNonce = dashboardRandomBase64Url(32);
    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    this.localOfferSdp = offer.sdp || '';
    console.debug(`[peer-dashboard-control ${this.hostId}] offer advertiseTcpViaUrl=${this.advertiseTcpViaUrl || '(none)'}`);
    await this.sendOffer(this.localOfferSdp, options);
    await ready;
    return true;
  }

  waitForReady(options = {}) {
    const timeoutMs = Math.max(1000, Number(options.timeoutMs || 30000));
    return new Promise((resolve, reject) => {
      let settled = false;
      let abortHandler = null;
      const finish = (fn, value) => {
        if (settled) return;
        settled = true;
        window.clearTimeout(timeoutId);
        if (options.signal && abortHandler) {
          options.signal.removeEventListener('abort', abortHandler);
        }
        fn(value);
      };
      const timeoutId = window.setTimeout(
        () => finish(reject, new Error('peer dashboard-control connection timed out')),
        timeoutMs
      );
      abortHandler = () => finish(
        reject,
        dashboardControlAbortError('peer dashboard-control connection aborted')
      );
      if (options.signal) {
        if (options.signal.aborted) {
          abortHandler();
        } else {
          options.signal.addEventListener('abort', abortHandler, { once: true });
        }
      }
      this._readyResolve = value => finish(resolve, value);
      this._readyReject = err => finish(reject, err);
    });
  }

  handleOpen() {
    this.lastError = '';
    this.refreshIceRoute().catch(() => {});
    this.sendFrame({
      t: 'hello',
      id: this.nextId(),
      features: ['response_credit', 'byte_streams', 'upload_frames', 'terminal_frames'],
    });
    this.ping().catch(() => {});
    this.request('status').then(status => {
      if (status && typeof status === 'object') {
        this.lastStatus = status;
        console.info(`[peer-dashboard-control ${this.hostId}] status RPC ok`, status.session_id || '');
        renderDashboardTargetSummaries();
      }
    }).catch(err => console.warn(`[peer-dashboard-control ${this.hostId}] status RPC failed`, err));
    this._readyResolve?.(true);
  }

  async sendOffer(sdp, options = {}) {
    const signal = {
      kind: 'offer',
      sdp,
      client_nonce: this.clientNonce,
    };
    if (this.advertiseTcpViaUrl) {
      signal.advertise_tcp_via_url = this.advertiseTcpViaUrl;
    }
    // Delegation-lane attribution (trust-tiers § Two lanes): sign the
    // offer with this browser's identity key so the TARGET can record
    // who is behind the relayed channel (and refuse a spliced one).
    // daemonId = the target we mean; the target verifies it meant it.
    // No identity key (unsupported browser) → unattributed, admitted.
    try {
      const fields = await clientIdentityOfferFields(this.hostId, this.clientNonce, sdp);
      if (fields) Object.assign(signal, fields);
    } catch (err) {
      console.warn(`[peer-dashboard-control ${this.hostId}] offer attribution skipped:`, err?.message || err);
    }
    await this.sendSignal(signal, options);
    return null;
  }

  async sendIce(candidate) {
    if (!this.sessionId) return;
    await this.sendSignal({
      kind: 'ice_candidate',
      candidate_json: JSON.stringify(candidate || {}),
    });
  }

  async sendSignal(signal, options = {}) {
    // Facade envelope (transport F5): {ok, status, body} — a delivered
    // error response is final (no replay lane exists for signaling).
    const resp = await dashboardTransport.peerDashboardControlSignal(this.hostId, {
      session_id: this.sessionId,
      signal,
    }, {
      signal: options.signal,
    });
    if (!resp.ok) {
      throw new Error(`peer dashboard-control signal failed (${resp.status}): ${resp.body?.error || 'unknown'}`);
    }
  }

  signalClose(sessionId) {
    if (!sessionId) return;
    this.sendSignal({ kind: 'close' }).catch(() => {});
  }

  async handleSignalAnswer(signal) {
    const sdp = signal?.sdp || '';
    console.debug(`[peer-dashboard-control ${this.hostId}] answer received`);
    await this.handleAnswer({
      session_id: this.sessionId,
      sdp,
      binding: signal?.binding || null,
    });
  }

  handleSignalIce(candidateJson) {
    if (!candidateJson) return;
    let candidate = candidateJson;
    if (typeof candidateJson === 'string') {
      try {
        candidate = JSON.parse(candidateJson);
      } catch (err) {
        console.warn(`[peer-dashboard-control ${this.hostId}] remote ICE JSON parse failed`, err);
        return;
      }
    }
    this.handleIceCandidate(candidate);
  }

  close(options = {}) {
    const key = this.sessionKey();
    if (peerDashboardControlConnections.get(key) === this) {
      peerDashboardControlConnections.delete(key);
    }
    if (peerDashboardControlConnectionsByHost.get(this.hostId) === this) {
      peerDashboardControlConnectionsByHost.delete(this.hostId);
    }
    for (const pending of this.pending.values()) {
      pending.reject(new Error('peer dashboard-control transport closed'));
    }
    this.pending.clear();
    this.chunkedResponses.clear();
    this.byteStreams.clear();
    if (options.signalRemote !== false && this.sessionId) {
      this.signalClose(this.sessionId);
    }
    try { this.channel?.close(); } catch {}
    try { this.pc?.close(); } catch {}
    this.channel = null;
    this.pc = null;
  }

  describeCandidate(candidate) {
    const line = String(candidate?.candidate || '').trim();
    const parts = line.split(/\s+/);
    const protocol = (parts[2] || '').toLowerCase();
    const address = parts[4] || '';
    const port = parts[5] || '';
    const typeIndex = parts.indexOf('typ');
    const type = typeIndex >= 0 ? parts[typeIndex + 1] : '';
    const tcpTypeIndex = parts.indexOf('tcptype');
    const tcpType = tcpTypeIndex >= 0 ? ` ${parts[tcpTypeIndex + 1] || ''}` : '';
    return `${type || 'candidate'} ${protocol} ${address}:${port}${tcpType}`.trim();
  }
}

function peerDashboardControlSignalAvailable(peerId) {
  const id = String(peerId || '').trim();
  if (!id) return false;
  if (!window.RTCPeerConnection) return false;
  const peer = daemons.find(d => d.host_id === id);
  if (!peer || peer.connected === false) return false;
  if (dashboardConnectModeEnabled()) {
    return Boolean(
      dashboardTransport?.canUseRpc?.() &&
      dashboardControlTransport?.lastStatus?.api_peer_dashboard_control_signal_available === true
    );
  }
  return true;
}

async function peerDashboardControlConnectionForHost(hostId, options = {}) {
  const id = String(hostId || '').trim();
  if (!id || id === selfPeerId) return null;
  if (!peerDashboardControlSignalAvailable(id)) {
    throw new Error('peer dashboard-control signaling is not available');
  }
  const existing = peerDashboardControlConnectionsByHost.get(id);
  if (existing && existing.canUseRpc()) return existing;
  if (existing) existing.close({ signalRemote: true });
  const conn = new PeerDashboardControlConnection(id);
  await conn.connect(options);
  return conn;
}

function handlePeerDashboardControlSignal(hostId, sessionId, signal) {
  const sessionKey = `${hostId}|${sessionId}`;
  const conn = peerDashboardControlConnections.get(sessionKey);
  const kind = signal && signal.kind;
  if (!conn) {
    console.debug(`[peer-dashboard-control ${hostId}] received ${kind || '(no-kind)'} for unknown session ${sessionId}`);
    return;
  }
  if (kind === 'answer') {
    conn.handleSignalAnswer(signal).catch(err => {
      conn.lastError = err?.message || String(err);
      console.warn(`[peer-dashboard-control ${hostId}] answer failed`, err);
      conn.close({ signalRemote: false });
    });
  } else if (kind === 'ice_candidate') {
    conn.handleSignalIce(signal.candidate_json || '');
  } else if (kind === 'close') {
    conn.close({ signalRemote: false });
  } else {
    console.debug(`[peer-dashboard-control ${hostId}] unknown signal kind=${kind || '(none)'} for session ${sessionId}`);
  }
}

// snake_case → camelCase for status-frame keys (debugStatus derive).
// `webrtc` keeps its historical `WebRtc` hump: the QA harnesses
// (validate-dashboard-control-local-signaling.cjs) assert
// `apiPeerWebRtcSignalAvailable` by that exact name.
function dashboardControlCamelKey(key) {
  return String(key || '')
    .replace(/_([a-z0-9])/g, (_, ch) => ch.toUpperCase())
    .replace(/Webrtc/g, 'WebRtc');
}

// ── event_gap recovery ──
// Both event lanes can report dropped events: the dashboard-control tunnel
// emits {"t":"event_gap","skipped":N} when its outbound event queue
// overflows, and the /ws lane is gaining a frame with the same shape. One
// coalescing window (from the FIRST gap) turns any burst into: a pulse on
// the lane's status chip, a single "Recovering N missed events…" toast,
// and one state refresh (sessions metadata + timeline re-pull, plus a
// rate-limited full bootstrap re-pull over the tunnel when it is up).
const DASHBOARD_EVENT_GAP_COALESCE_MS = 1500;
const DASHBOARD_EVENT_GAP_HYDRATE_MIN_INTERVAL_MS = 30000;
let dashboardEventGapPendingSkipped = 0;
let dashboardEventGapToastTimer = null;
let dashboardEventGapLastHydrateAt = 0;

// Visual pulse on the oversight-bar connection chip for an event gap —
// Web Animations API only, so no stylesheet dependency. (Both lanes pulse
// the same chip: the oversight bar shows one transport indicator.)
function dashboardPulseEventLaneChip(_lane) {
  const el = document.getElementById('ui2-conn');
  if (!el || typeof el.animate !== 'function') return;
  try {
    el.animate(
      [{ opacity: 1 }, { opacity: 0.25 }, { opacity: 1 }],
      { duration: 380, iterations: 3, easing: 'ease-in-out' }
    );
  } catch (_) {}
}

function dashboardHandleEventGap(msg, lane = 'tunnel') {
  const skipped = Math.max(0, Number(msg?.skipped) || 0);
  dashboardEventGapPendingSkipped += skipped;
  console.warn('[server-msg] event_gap', lane, skipped || '(count unknown)');
  dashboardPulseEventLaneChip(lane);
  if (dashboardEventGapToastTimer) return;
  dashboardEventGapToastTimer = window.setTimeout(() => {
    dashboardEventGapToastTimer = null;
    const n = dashboardEventGapPendingSkipped;
    dashboardEventGapPendingSkipped = 0;
    const what = n > 0 ? `${n} missed event${n === 1 ? '' : 's'}` : 'missed events';
    if (typeof showControlToast === 'function') {
      showControlToast('info', `Recovering ${what}…`);
    }
    if (typeof scheduleSessionsMetadataRefresh === 'function') {
      scheduleSessionsMetadataRefresh();
    }
    if (typeof refreshHistory === 'function') {
      try { refreshHistory(); } catch (err) { console.warn('[server-msg] event_gap history refresh failed', err); }
    }
    const now = Date.now();
    if (
      typeof hydrateDashboardFromControl === 'function' &&
      dashboardTransport?.canUseRpc?.() &&
      now - dashboardEventGapLastHydrateAt > DASHBOARD_EVENT_GAP_HYDRATE_MIN_INTERVAL_MS
    ) {
      dashboardEventGapLastHydrateAt = now;
      hydrateDashboardFromControl().catch(err => {
        console.warn('[server-msg] event_gap bootstrap re-pull failed', err);
      });
    }
  }, DASHBOARD_EVENT_GAP_COALESCE_MS);
}

function dashboardControlAbortError(message = 'dashboard control request aborted') {
  try {
    return new DOMException(message, 'AbortError');
  } catch {
    const err = new Error(message);
    err.name = 'AbortError';
    return err;
  }
}

function dashboardControlBase64ToBytes(value) {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function dashboardControlBytesToBase64(bytes) {
  // Batch via fromCharCode.apply over bounded slices: the per-byte string
  // concatenation this replaces built ~16k intermediate strings per upload
  // chunk. 0x8000 stays comfortably under engine argument-count limits.
  const chunks = [];
  for (let i = 0; i < bytes.byteLength; i += 0x8000) {
    chunks.push(String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000)));
  }
  return btoa(chunks.join(''));
}

function dashboardControlRequestTimeoutMs(method) {
  switch (method) {
    case 'api_sessions':
      return 30000;
    case 'api_sessions_stream':
      return 120000;
    case 'api_sessions_search':
    case 'api_session_current_changes':
    case 'api_session_report':
    case 'api_recordings':
    case 'api_session_recordings':
    case 'api_worktrees':
    case 'api_worktrees_inspect':
    case 'api_worktrees_scan':
    case 'api_worktrees_remove':
    case 'api_fs_mkdir':
    case 'api_fs_read':
    case 'api_transfer_jobs':
    case 'api_transfer_job_create':
    case 'api_transfer_job_delete':
    case 'api_transfer_download_read':
    case 'api_transfer_upload_chunk':
    case 'api_transfer_upload_commit':
    case 'api_session_agent_output':
    case 'api_managed_context_records':
    case 'api_managed_context_anchors':
    case 'api_managed_context_fission':
    case 'api_mcp_tool_call':
      return 120000;
    case 'api_session_detail':
      return 15000;
    // Peer quick controls cross the federation transport and wait for the
    // remote daemon's ack — the generic 5 s default was sized for local
    // reads, not a peer round trip (transport F5's deliberate
    // normalization; the legacy HTTP lane ran signal-less). The same
    // budget covers the dials that reach a REMOTE daemon before
    // answering: peer add (card fetch + transport spawn), pairing
    // join/request-access/poll (doorbell round trips), and coordinator
    // routing (delegation ack).
    case 'api_peer_message':
    case 'api_peer_task':
    case 'api_peer_approval':
    case 'api_peer_add':
    case 'api_peer_pairing_join':
    case 'api_peer_pairing_request_access':
    case 'api_peer_pairing_request_access_poll':
    case 'api_coordinator_route':
      return 30000;
    // Credential custody (transport F6): the sealed vault blob rides the
    // chunked, credit-gated response lane and can run to hundreds of KiB
    // over a TURN link. The family's pre-facade caller asked for 15 s and
    // was silently clamped to the 5 s default (this verb ignores
    // options.timeoutMs) — this table is where that intent actually
    // lives.
    case 'api_credential_lease_grant':
    case 'api_credential_lease_renew':
    case 'api_credential_lease_revoke':
    case 'api_credential_lease_status':
    case 'api_credential_custody_trail':
    case 'api_credential_egress_register':
    case 'api_credential_egress_unregister':
    case 'api_daemon_vault_fetch':
    case 'api_daemon_vault_publish':
    case 'api_daemon_vault_deposit_key_fetch':
    case 'api_daemon_vault_deposit_key_publish':
    case 'api_daemon_vault_deposits_fetch':
    case 'api_daemon_vault_deposits_consume':
      return 15000;
    // The egress probe reaches beyond this daemon before answering:
    // daemon -> the relaying browser -> the provider's API -> back. Same
    // budget as the peer round trips above.
    case 'api_credential_egress_probe':
      return 30000;
    // The control-msg trio (transport F7): interrupts, session lifecycle,
    // and dashboard actions dispatched while a busy daemon is mid-turn.
    // The pre-facade call sites asked for 10-15 s and were silently
    // clamped to the 5 s default (this verb ignores options.timeoutMs) —
    // the table row is where that intent actually lives.
    case 'api_control_msg':
      return 10000;
    case 'api_session_control_msg':
    case 'api_dashboard_action_msg':
      return 15000;
    // Display WebRTC signaling (transport F7): the offer leg waits for
    // the server's full answer negotiation (encoder spawn included) and
    // asked for 30 s pre-facade; ICE posts share the method and resolve
    // fast, so the longer budget is harmless there. The visual-freshness
    // transcript flush asked for 10 s.
    case 'api_display_webrtc_signal':
      return 30000;
    case 'api_diagnostics_visual_freshness':
      return 10000;
    default:
      return 5000;
  }
}

async function verifyDashboardControlBinding(binding, sessionId, offerSdp, answerSdp, sessionGrant = '', clientNonce = '') {
  if (!binding || typeof binding !== 'object') {
    return { ok: false, error: 'missing binding' };
  }
  if (binding.protocol !== 'intendant-dashboard-control-v1') {
    return { ok: false, error: 'unexpected protocol' };
  }
  if (String(binding.session_id || '') !== String(sessionId || '')) {
    return { ok: false, error: 'session mismatch' };
  }
  if (!window.crypto?.subtle) {
    return { ok: false, error: 'WebCrypto unavailable' };
  }
  const createdUnixMs = Number(binding.created_unix_ms || 0);
  const expiresUnixMs = Number(binding.expires_unix_ms || 0);
  if (!Number.isFinite(createdUnixMs) || createdUnixMs <= 0) {
    return { ok: false, error: 'missing binding creation time' };
  }
  if (!Number.isFinite(expiresUnixMs) || expiresUnixMs <= 0) {
    return { ok: false, error: 'missing binding expiry' };
  }
  const nowUnixMs = Date.now();
  if (expiresUnixMs + DASHBOARD_CONTROL_BINDING_CLOCK_SKEW_MS < nowUnixMs) {
    return { ok: false, error: 'binding expired' };
  }
  if (createdUnixMs - DASHBOARD_CONTROL_BINDING_CLOCK_SKEW_MS > nowUnixMs) {
    return { ok: false, error: 'binding timestamp from future' };
  }
  const offerHash = await dashboardSha256B64u(offerSdp || '');
  if (binding.offer_sha256 !== offerHash) {
    return { ok: false, error: 'offer hash mismatch' };
  }
  const answerHash = await dashboardSha256B64u(answerSdp || '');
  if (binding.answer_sha256 !== answerHash) {
    return { ok: false, error: 'answer hash mismatch' };
  }
  const nonce = String(clientNonce || '');
  if (nonce) {
    if (String(binding.client_nonce || '') !== nonce) {
      return { ok: false, error: 'client nonce mismatch' };
    }
  } else if (binding.client_nonce) {
    return { ok: false, error: 'unexpected client nonce binding' };
  }
  const grant = String(sessionGrant || '');
  if (grant) {
    const grantHash = await dashboardSha256B64u(grant);
    if (binding.session_grant_sha256 !== grantHash) {
      return { ok: false, error: 'session grant hash mismatch' };
    }
  } else if (binding.session_grant_sha256) {
    return { ok: false, error: 'unexpected session grant binding' };
  }

  let verified = false;
  try {
    verified = await dashboardVerifyEd25519(
      dashboardBase64UrlToBytes(binding.daemon_public_key || ''),
      dashboardBase64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(dashboardControlBindingPayload(binding))
    );
  } catch (err) {
    return { ok: false, error: err?.message || 'signature verification unavailable' };
  }
  if (!verified) {
    return { ok: false, error: 'signature invalid' };
  }
  return {
    ok: true,
    daemonPublicKey: binding.daemon_public_key,
    createdUnixMs,
    expiresUnixMs,
    clientNonce: binding.client_nonce || '',
    sessionGrantSha256: binding.session_grant_sha256 || '',
  };
}

function sessionDetailErrorIsMissing(data) {
  return String(data?.error || '').trim().toLowerCase() === 'session not found';
}

// daemonApi (transport F2): tunnel first, direct HTTP per the GET-twin
// fallback policy. Callers keep the payload-with-error contract this
// helper always had — errors surface as data.error, never as throws for
// delivered responses (sessionDetailErrorIsMissing keys off that).
async function fetchSessionDetailPayload(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) throw new Error('missing session id');
  const source = String(options.source || 'intendant').trim() || 'intendant';
  const params = { session_id: sid, source };
  if (options.limit !== undefined && options.limit !== null) {
    params.limit = options.limit;
  }
  if (options.before !== undefined && options.before !== null) {
    params.before = options.before;
  }
  const resp = await daemonApi.request('api_session_detail', params, {
    signal: options.signal,
    cache: options.cache,
  });
  const data = (resp.body && typeof resp.body === 'object' && !Array.isArray(resp.body))
    ? resp.body
    : {};
  if (!resp.ok && !data.error) {
    data.error = `HTTP ${resp.status}`;
  }
  return data;
}

// daemonApi (transport F8a): tunnel first, HTTP twin fallback — a
// POST-shaped read (the body carries output ids; nothing is written), so
// the verb-derived policy never replays an attempted send. Callers keep
// the payload-with-error contract this helper always had — errors surface
// as data.error, never as throws for delivered responses (the same shape
// fetchSessionDetailPayload pins above).
async function fetchSessionAgentOutputPayload(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) throw new Error('missing session id');
  const source = String(options.source || 'intendant').trim() || 'intendant';
  const ids = Array.isArray(options.ids)
    ? options.ids.map(id => String(id || '').trim()).filter(Boolean)
    : [];
  if (!ids.length) throw new Error('missing output ids');
  const resp = await daemonApi.request('api_session_agent_output', {
    session_id: sid,
    source,
    ids,
  }, { signal: options.signal, cache: options.cache });
  const data = (resp.body && typeof resp.body === 'object' && !Array.isArray(resp.body))
    ? resp.body
    : {};
  if (!resp.ok && !data.error) {
    data.error = `HTTP ${resp.status}`;
  }
  return data;
}

async function fetchSessionsSearchPayload(options = {}) {
  if (options.signal?.aborted) {
    throw new DOMException('Aborted', 'AbortError');
  }
  const query = String(options.query || options.q || '').trim();
  const source = String(options.source || 'all').trim() || 'all';
  const mode = String(options.mode || '').trim();
  const projects = Array.isArray(options.projects)
    ? options.projects.map(value => String(value || '').trim()).filter(Boolean)
    : [];
  // Progress lane first: the HTTP route streams NDJSON progress lines
  // when asked (stream=ndjson) — one {"type":"deep_search_progress",...}
  // every ~250 scanned sessions, then the legacy body as the final line.
  // Hosted/tunnel-only dashboards have no direct HTTP origin: the fetch
  // fails before any bytes and we fall through to the buffered lane.
  const streamed = await fetchSessionsSearchStreaming({ query, source, mode, projects, signal: options.signal });
  if (streamed) return streamed;
  // daemonApi (transport F2): tunnel first, direct HTTP per the GET-twin
  // fallback policy; the descriptor JSON-encodes `projects` on the HTTP
  // lane exactly as the hand-built fallback did.
  const resp = await daemonApi.request('api_sessions_search', {
    q: query,
    source,
    mode,
    projects,
  }, { signal: options.signal });
  if (!resp.ok) throw new Error(`/api/sessions/search returned ${resp.status}`);
  return resp.body;
}

async function fetchSessionsSearchStreaming({ query, source, mode, projects, signal }) {
  if (typeof fetch !== 'function' || !/^https?:$/.test(location.protocol)) return null;
  const params = new URLSearchParams({ q: query, stream: 'ndjson' });
  if (source) params.set('source', source);
  if (mode) params.set('mode', mode);
  if (projects && projects.length) params.set('projects', JSON.stringify(projects));
  let resp;
  try {
    resp = await fetch('/api/sessions/search?' + params.toString(), {
      signal,
      credentials: 'same-origin',
      headers: { 'Accept': 'application/x-ndjson' },
    });
  } catch (e) {
    if (e && e.name === 'AbortError') throw e;
    return null; // no direct HTTP lane — buffered fallback
  }
  if (!resp.ok || !resp.body) return null;
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  let finalPayload = null;
  const consumeLine = (line) => {
    const text = line.trim();
    if (!text) return;
    let parsed;
    try { parsed = JSON.parse(text); } catch { return; }
    if (parsed && parsed.type === 'deep_search_progress') {
      if (typeof applySessionDeepSearchProgress === 'function') {
        try { applySessionDeepSearchProgress(parsed); } catch (err) { console.warn('[deep-search] progress render failed', err); }
      }
      return;
    }
    finalPayload = parsed;
  };
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      consumeLine(buf.slice(0, nl));
      buf = buf.slice(nl + 1);
    }
  }
  buf += decoder.decode();
  if (buf.trim()) consumeLine(buf);
  if (finalPayload === null) throw new Error('/api/sessions/search stream ended without a result');
  return finalPayload;
}

// The F3 settings/keys-family reads (below): daemonApi — tunnel first,
// direct HTTP per the GET-twin fallback policy. These tunnel results ride
// the body-only envelope (no injected status), so `resp.ok` reflects the
// HTTP lane exactly where the legacy fallbacks threw on !resp.ok, and the
// historical always-200 error bodies (settings GET's
// {"error":"No project root"}) still arrive as ok bodies for callers to
// inspect.
async function fetchDashboardSettings() {
  const resp = await daemonApi.request('api_settings');
  if (!resp.ok) throw new Error(`/api/settings returned ${resp.status}`);
  return resp.body;
}

async function fetchApiKeyStatus() {
  const resp = await daemonApi.request('api_key_status');
  if (!resp.ok) throw new Error(`/api/api-key-status returned ${resp.status}`);
  return resp.body;
}

async function fetchExternalAgentAvailability() {
  const resp = await daemonApi.request('api_external_agents');
  if (!resp.ok) throw new Error(`/api/external-agents returned ${resp.status}`);
  return resp.body;
}

async function fetchProjectRoot() {
  const resp = await daemonApi.request('api_project_root');
  if (!resp.ok) throw new Error(`/api/project-root returned ${resp.status}`);
  return resp.body;
}

function dashboardReportRpcAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.api_session_report_available === true
  );
}

// "Can this method be served over the TUNNEL byte-stream lane right now?"
// Derived through the facade (F1b): reason 'connected' requires a live
// tunnel plus the daemon's own lane + per-method availability booleans
// (server-derived from its method table). Deliberately NOT `.ok` — an
// `http-only` answer means the direct HTTP lane could serve it, which is
// not what this probe's consumers gate on. For descriptor methods the
// byte_streams lane check rides the descriptor's lane; tunnel-only
// methods (api_transfer_*) reduce to their per-method boolean — a daemon
// that advertises those booleans always has byte streams.
function dashboardByteStreamMethodAvailable(method) {
  return daemonApi.availability(method).reason === 'connected';
}

function dashboardTransferDownloadAvailable() {
  return dashboardByteStreamMethodAvailable('api_transfer_download_read') &&
    daemonApi.availability('api_transfer_job_create').reason === 'connected';
}

function dashboardTransferUploadAvailable() {
  return ['api_transfer_job_create', 'api_transfer_upload_chunk', 'api_transfer_upload_commit']
    .every(method => daemonApi.availability(method).reason === 'connected');
}

async function ensureDashboardTransferUploadAvailable(options = {}) {
  if (dashboardTransferUploadAvailable()) return true;
  if (
    !dashboardTransport ||
    !dashboardTransport.canUseRpc ||
    !dashboardTransport.canUseRpc() ||
    !dashboardControlTransport
  ) {
    return false;
  }
  try {
    const status = await dashboardTransport.request('status', {}, {
      timeoutMs: options.timeoutMs || 15000,
      signal: options.signal,
    });
    if (status && typeof status === 'object') {
      dashboardControlTransport.lastStatus = status;
      dashboardUpdateTransportStatus();
      if (typeof updateNewSessionFuelBanner === 'function') updateNewSessionFuelBanner();
    }
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    console.warn('[dashboard-control] transfer upload status refresh failed', err);
  }
  return dashboardTransferUploadAvailable();
}

function downloadDashboardBlob(blob, filename, contentType) {
  const finalBlob = blob instanceof Blob
    ? blob
    : new Blob([blob || new Uint8Array()], { type: contentType || 'application/octet-stream' });
  const url = URL.createObjectURL(finalBlob);
  const link = document.createElement('a');
  link.href = url;
  link.download = filename || 'download.bin';
  link.style.display = 'none';
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

function downloadDashboardBytes(bytes, filename, contentType) {
  downloadDashboardBlob(
    new Blob([bytes], { type: contentType || 'application/octet-stream' }),
    filename || 'intendant-session-report.zip',
    contentType || 'application/octet-stream'
  );
}

function rangedDownloadTimeoutMs(chunkBytes) {
  const size = Number(chunkBytes) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES;
  return Math.max(30000, Math.ceil(size / (256 * 1024)) * 10000);
}

// Compose a caller's abort signal with a hard timeout. fetch() has no
// default timeout, and the transfers pump runs entries strictly
// sequentially — a single hung request must fail rather than wedge the
// whole queue behind it.
function dashboardComposeFetchSignal(signal, timeoutMs) {
  const ms = Number(timeoutMs) || 0;
  if (!(ms > 0) || typeof AbortSignal?.timeout !== 'function') return signal || undefined;
  const timeout = AbortSignal.timeout(ms);
  if (!signal) return timeout;
  return typeof AbortSignal.any === 'function' ? AbortSignal.any([signal, timeout]) : signal;
}

async function dashboardRequestBytesWithRetry(method, params, options = {}) {
  const retries = Number.isFinite(Number(options.retries)) ? Math.max(0, Number(options.retries)) : 2;
  let attempt = 0;
  for (;;) {
    if (options.signal?.aborted) throw dashboardControlAbortError();
    try {
      return await dashboardTransport.requestBytes(method, params, {
        timeoutMs: options.timeoutMs,
        signal: options.signal,
      });
    } catch (err) {
      if (err?.name === 'AbortError' || attempt >= retries) throw err;
      attempt += 1;
      await new Promise(resolve => setTimeout(resolve, 200 * attempt));
    }
  }
}

function dashboardNormalizeByteRangeResult(method, result, expectedOffset) {
  if (result?.ok === false || result?._httpOk === false) {
    throw new Error(result.error || `${method} returned ${result._httpStatus || 'error'}`);
  }
  const bytes = result?.bytes instanceof Uint8Array
    ? result.bytes
    : dashboardControlBase64ToBytes(result?.data_base64 || '');
  const rangeStart = Number(result?.range_start ?? result?.offset ?? expectedOffset);
  const rangeEnd = Number(result?.range_end ?? (rangeStart + bytes.byteLength));
  const declaredTotal = Number(result?.total_size ?? result?.totalSize ?? rangeEnd);
  if (!Number.isSafeInteger(rangeStart) || rangeStart !== expectedOffset) {
    throw new Error(`${method} returned unexpected range start`);
  }
  if (!Number.isSafeInteger(rangeEnd) || rangeEnd < rangeStart || rangeEnd - rangeStart !== bytes.byteLength) {
    throw new Error(`${method} returned inconsistent range length`);
  }
  if (!Number.isSafeInteger(declaredTotal) || declaredTotal < rangeEnd) {
    throw new Error(`${method} returned invalid total size`);
  }
  return {
    bytes,
    rangeStart,
    rangeEnd,
    totalSize: declaredTotal,
    filename: result?.filename ? String(result.filename) : '',
    contentType: result?.content_type ? String(result.content_type) : '',
    job: result?.job && typeof result.job === 'object' ? result.job : null,
  };
}

async function dashboardFetchRangedBytes(method, params = {}, options = {}) {
  if (!dashboardByteStreamMethodAvailable(method)) {
    throw new Error(`${method} byte stream is not available through this dashboard connection`);
  }
  const chunkBytes = Math.max(1, Math.floor(Number(options.chunkBytes) || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES));
  const maxBytes = Math.max(1, Math.floor(Number(options.maxBytes) || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES));
  const startOffset = Math.max(0, Math.floor(Number(options.offset) || 0));
  let offset = startOffset;
  let totalSize = null;
  let filename = String(options.filename || '').trim();
  let contentType = String(options.contentType || '').trim() || 'application/octet-stream';
  const parts = [];
  let rangeCount = 0;
  for (;;) {
    if (options.signal?.aborted) throw dashboardControlAbortError();
    const requested = totalSize == null
      ? chunkBytes
      : Math.min(chunkBytes, Math.max(0, totalSize - offset));
    if (requested <= 0) break;
    const result = await dashboardRequestBytesWithRetry(method, {
      ...(params || {}),
      offset,
      length: requested,
    }, {
      signal: options.signal,
      retries: options.retries,
      timeoutMs: options.timeoutMs || rangedDownloadTimeoutMs(requested),
    });
    const range = dashboardNormalizeByteRangeResult(method, result, offset);
    const bytes = range.bytes;
    const rangeEnd = range.rangeEnd;
    totalSize = range.totalSize;
    if (totalSize - startOffset > maxBytes) {
      throw new Error(`Download too large (${humanBytes(totalSize - startOffset)}; cap is ${humanBytes(maxBytes)})`);
    }
    if (!filename && range.filename) filename = range.filename;
    if (range.contentType) contentType = range.contentType;
    parts.push(bytes);
    offset = rangeEnd;
    rangeCount += 1;
    if (typeof options.onProgress === 'function') {
      options.onProgress({
        loaded: offset - startOffset,
        total: Math.max(0, totalSize - startOffset),
        offset,
        rangeCount,
        filename,
        contentType,
      });
    }
    if (offset >= totalSize || bytes.byteLength === 0) break;
  }
  const blob = new Blob(parts, { type: contentType || 'application/octet-stream' });
  return {
    ok: true,
    blob,
    parts,
    filename: filename || 'download.bin',
    content_type: contentType,
    size: blob.size,
    total_size: totalSize ?? blob.size,
    range_start: startOffset,
    range_end: offset,
    range_count: rangeCount,
    resumable: true,
  };
}

async function dashboardFetchTransferArtifactBytes(artifact, options = {}) {
  if (!dashboardTransferDownloadAvailable()) return null;
  if (!artifact || typeof artifact !== 'object') {
    throw new Error('artifact descriptor is required');
  }
  const created = await dashboardTransport.request('api_transfer_job_create', {
    kind: 'download',
    artifact,
  }, {
    timeoutMs: options.timeoutMs || 120000,
    signal: options.signal,
  });
  if (created?.ok === false || created?._httpOk === false) {
    throw new Error(created.error || `transfer create returned ${created._httpStatus || 'error'}`);
  }
  const job = created?.job && typeof created.job === 'object' ? created.job : null;
  if (!job?.id && !job?.resume_token) throw new Error('transfer create did not return a readable job');
  const result = await dashboardFetchRangedBytes('api_transfer_download_read', {
    id: job.id || undefined,
    resume_token: job.resume_token || undefined,
  }, {
    signal: options.signal,
    chunkBytes: options.chunkBytes || DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES,
    maxBytes: options.maxBytes || DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES,
    retries: options.retries,
    timeoutMs: options.timeoutMs,
    filename: job.filename || options.filename,
    contentType: job.mime || options.contentType,
    onProgress: options.onProgress,
  });
  result.job = job;
  return result;
}

async function downloadSessionReportViaDashboardControl(event) {
  const useDurableTransfer = dashboardTransferDownloadAvailable();
  if (!useDurableTransfer && !dashboardReportRpcAvailable()) {
    if (dashboardConnectModeEnabled()) {
      event.preventDefault();
      if (typeof showControlToast === 'function') {
        showControlToast('error', 'Session report is unavailable until dashboard access reconnects');
      }
    }
    return;
  }
  event.preventDefault();
  const link = event.currentTarget;
  const previousText = link?.textContent || '';
  if (link) {
    link.textContent = 'Preparing...';
    link.setAttribute('aria-busy', 'true');
  }
  try {
    if (useDurableTransfer) {
      const transfer = queueDashboardArtifactDownload({
        type: 'session_report',
        session_id: 'current',
      }, {
        sourceLabel: 'Current session report',
        filename: 'intendant-session-report.zip',
        contentType: 'application/zip',
        timeoutMs: 120000,
      });
      if (!transfer) throw new Error('Session report was not queued');
      const result = await transfer.completion;
      if (link) link.textContent = previousText || 'Download session report';
      return result;
    }
    // daemonApi (transport F2): the bytes verb prefers the tunnel
    // byte-stream lane and still decodes the pre-byte-stream JSON shape
    // (data_base64 in the result) that old daemons answer with; a failed
    // lane may replay the GET twin over direct HTTP (never in Connect
    // mode). The un-intercepted direct case above keeps the native <a>
    // navigation.
    const { bytes, meta } = await daemonApi.bytes('api_session_report', {
      session_id: 'current',
    }, { timeoutMs: 120000 });
    if (!bytes.length) throw new Error('Session report was empty');
    downloadDashboardBytes(
      bytes,
      meta.filename || 'intendant-session-report.zip',
      meta.contentType || 'application/zip'
    );
  } catch (err) {
    console.warn('[dashboard-control] api_session_report RPC failed', err);
    if (typeof showControlToast === 'function') {
      showControlToast('error', err?.message || 'Session report download failed');
    }
  } finally {
    if (link) {
      link.textContent = previousText || 'Download session report';
      link.removeAttribute('aria-busy');
    }
  }
}

function normalizeDisplaysPayload(payload) {
  if (Array.isArray(payload)) return payload;
  if (Array.isArray(payload?.displays)) return payload.displays;
  return [];
}

async function fetchLocalDisplaysPayload() {
  // daemonApi (transport F3): tunnel first, direct HTTP per the GET-twin
  // fallback policy (the HTTP error body's message survives as before).
  const resp = await daemonApi.request('api_displays');
  if (!resp.ok) {
    throw new Error((resp.body && resp.body.error) || `HTTP ${resp.status}`);
  }
  return resp.body;
}

function dashboardControlBindingPayload(binding) {
  const parts = [
    binding.protocol || '',
    binding.session_id || '',
    binding.daemon_public_key || '',
    String(binding.created_unix_ms || ''),
    String(binding.expires_unix_ms || ''),
    binding.offer_sha256 || '',
    binding.answer_sha256 || '',
  ];
  if (binding.client_nonce) parts.push(binding.client_nonce);
  if (binding.session_grant_sha256) parts.push(binding.session_grant_sha256);
  return parts.join('\n');
}

async function dashboardSha256B64u(text) {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
  return dashboardBytesToBase64Url(new Uint8Array(digest));
}

async function dashboardVerifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
  let key;
  try {
    key = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, ['verify']);
  } catch (firstErr) {
    try {
      key = await crypto.subtle.importKey('raw', publicKeyBytes, 'Ed25519', false, ['verify']);
    } catch {
      throw firstErr;
    }
  }
  try {
    return await crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  } catch {
    return await crypto.subtle.verify('Ed25519', key, signatureBytes, payloadBytes);
  }
}

function dashboardBase64UrlToBytes(value) {
  const normalized = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
  const padded = normalized + '='.repeat((4 - normalized.length % 4) % 4);
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function dashboardBytesToBase64Url(bytes) {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}


function dashboardRandomBase64Url(byteLength = 32) {
  const bytes = new Uint8Array(byteLength);
  crypto.getRandomValues(bytes);
  return dashboardBytesToBase64Url(bytes);
}

// True when `url`'s hostname is loopback (127.0.0.1 / ::1 / localhost).
// Used to filter out loopback URLs before handing them to WebRTC as
// advertise_tcp_via_url: browsers silently drop remote loopback ICE
// candidates as an anti-rebinding mitigation, so advertising loopback
// breaks ICE-TCP pairing. When the browser omits the hint, slice 3b's
// primary-as-media-relay fallback kicks in and media flows via the
// primary's non-loopback address.
function isLoopbackUrl(url) {
  if (!url) return false;
  try {
    const u = new URL(url);
    const host = u.hostname.toLowerCase();
    return host === 'localhost' || host === '127.0.0.1' || host === '::1';
  } catch {
    return false;
  }
}

// Configured receive-RID list for browser-side DisplaySlot Offer SDPs.
// The server-side encoder pool advertises `a=rid:<rid> recv` +
// `a=simulcast:recv <rids>` here; the server's answer then carries
// `a=simulcast:send <rids>` (one or many). RIDs are part of the wire
// contract — values here MUST be drawn from the set
// `LayerSpec::vp8_simulcast` produces server-side
// (display/encode/pool.rs); drift between this constant and the
// encoder pool means the answer SDP advertises RIDs the browser
// doesn't recognize and the track never wires up.
//
// **Default (post-#58): single-RID `['f']`.** WKWebView typically
// negotiates H.264 → hardware VideoToolbox encoder on macOS; Chrome
// typically negotiates VP8 → one software encoder. The answer is
// plain sendonly with no `a=simulcast:send` line. CPU stays low
// (one encoder, hardware where available) and the path is robust
// across the browser fleet. This is the standard configuration for
// Intendant's remote-control / agentic computer-use workload, where
// crisp, high-resolution, low-latency pixels matter more than
// multi-viewer adaptive bandwidth.
//
// **Experimental / opt-in: multi-RID `['f','h','q']`.** Lights up
// the encoder pool's three-layer adaptive-bandwidth path — three
// software VP8 encoders running concurrently, with TWCC-driven
// per-peer layer selection. Worth it only when multiple viewers
// with diverse bandwidth share the same display and the per-deployment
// CPU budget can afford parallel encoders (pre-#58 this was the
// default; on a UTM macOS guest one local viewer cost 245 %+ CPU).
// Switch by editing this constant; no other code change required.
const DISPLAY_SIMULCAST_RIDS = ['f'];

// Inject `a=rid:<rid> recv` lines + `a=simulcast:recv <rids>` into
// the m=video section of a local DisplaySlot Offer SDP. The default
// `DISPLAY_SIMULCAST_RIDS` is `['f']`; switching it to `['f','h','q']`
// opts into the experimental multi-RID path. Federated
// `PeerDisplayConnection` offers intentionally omit recv-simulcast so the
// peer answers with a single floor RID.
//
// **Mirror of**: `inject_recv_simulcast_into_video_offer` in
// `src/bin/caller/display/forward.rs`. The Rust impl is the
// canonical source — it has the unit tests pinning the corner cases
// (video as final m= section with trailing CRLF; video followed by
// m=application; idempotent re-call). Drift here vs Rust means the
// browser advertises a different RID set than the answer-side test
// expects and simulcast doesn't wire up. Keep both in sync.
//
// Insertion-point logic (matches the Rust impl):
//   - If a later m= section exists, insert immediately before it
//     (canonical multi-m-section case — the section gets pushed down).
//   - If m=video is the LAST m= section (video-only SDP), back up
//     past trailing empty strings before splicing. An SDP ending
//     with CRLF makes `split('\r\n')` produce a trailing empty
//     string; splicing at `lines.length` would put the rid lines
//     AFTER the blank SDP body terminator, which some parsers treat
//     as out-of-band garbage and drop.
//
// Idempotent: returns `sdp` unchanged if the m=video section already
// declares `a=simulcast:` — avoids double-injection on the
// reconnect / re-offer paths that recreate the RTCPeerConnection
// from a previous offer's SDP.
//
// No-op cases (return input unchanged):
//   - `rids` empty (caller bug — pass at least one RID)
//   - no m=video section (audio-only offer, shouldn't happen for our
//     display flows but defensive)
//   - m=video already declares `a=simulcast:` (idempotent)
//
// SDP is split on CRLF (RFC 4566 line terminator) and rejoined the
// same way; works for the offer SDPs both Chrome and Safari emit.
function injectRecvSimulcastIntoVideoOffer(sdp, rids) {
  if (!rids || rids.length === 0) return sdp;
  const lines = sdp.split('\r\n');
  let videoStart = -1;
  let nextSection = -1;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].startsWith('m=video')) {
      videoStart = i;
    } else if (lines[i].startsWith('m=') && videoStart >= 0) {
      nextSection = i;
      break;
    }
  }
  if (videoStart < 0) return sdp;

  // Insertion point: before next m= section if found, otherwise
  // back up past any trailing empty lines from the SDP's CRLF
  // terminator(s) so we don't insert AFTER the blank SDP body
  // terminator. See the Rust impl's doc-comment for the full
  // rationale.
  let insertAt;
  if (nextSection >= 0) {
    insertAt = nextSection;
  } else {
    insertAt = lines.length;
    while (insertAt > videoStart + 1 && lines[insertAt - 1] === '') {
      insertAt--;
    }
  }

  // Idempotent: skip if m=video section already declares simulcast.
  for (let i = videoStart; i < insertAt; i++) {
    if (lines[i].startsWith('a=simulcast:')) return sdp;
  }

  const inject = rids.map(rid => `a=rid:${rid} recv`);
  inject.push(`a=simulcast:recv ${rids.join(';')}`);
  lines.splice(insertAt, 0, ...inject);
  return lines.join('\r\n');
}

// Return the URL the browser should advertise as `advertise_tcp_via_url`
// when opening a peer display, or '' when none is usable. Precedence:
// (1) explicit operator browser_tcp_via_url, (2) ws_url fallback, (3)
// empty to let slice 3b's primary-as-media-relay fallback take over.
// Loopback URLs at either position collapse to '' — browsers can't use
// a remote loopback ICE-TCP candidate, and falling back to ws_url when
// it's loopback would silently break federation in the single-machine-
// with-ssh-tunnel case. See renderDaemonRow / openPeerDisplay callers.
function resolveBrowserTcpViaUrl(d) {
  const explicit = d && d.browser_tcp_via_url;
  if (explicit && !isLoopbackUrl(explicit)) return explicit;
  const fallback = d && d.ws_url;
  if (fallback && !isLoopbackUrl(fallback)) return fallback;
  return '';
}

/// Fetch the peer list from /api/peers and update the in-memory
/// `daemons` array, re-register with WASM for live event streaming,
/// and re-render Access targets. Single source of truth for
/// peer state: called from initDaemons(), after addDaemon(), after
/// removeDaemon(), and on reconnect. No localStorage fallback —
/// the server-side PeerRegistry owns the peer list now.
// Build a local daemon entry from a server-side `PeerSnapshot`. The
// snapshot shape is the same whether it arrives via `GET /api/peers`
// Translate the lean PeerEvent::Usage snapshot into the legacy
// UpdateUsage shape that renderUsageTab expects.
//
// The PeerEvent vocabulary deliberately omits some fields that the
// local Stats renderer relies on (notably `context_window`, which a
// peer's UsageSnapshot doesn't carry). For those we substitute a
// generous placeholder so the percentage bar renders without
// dividing by zero — token counts and per-model breakdown stay
// accurate, only the bar's pct is approximate. When better fidelity
// is needed for a peer, that peer's own dashboard is the
// authoritative source.
const PEER_USAGE_PLACEHOLDER_CONTEXT = 200000;
function peerSnapshotToUpdateUsage(snap) {
  if (!snap || typeof snap !== 'object') snap = {};
  const tokensIn = snap.tokens_in || 0;
  const tokensOut = snap.tokens_out || 0;
  const cached = snap.tokens_cached || 0;
  const tokensUsed = tokensIn + tokensOut;
  const firstModel = (Array.isArray(snap.by_model) && snap.by_model[0]) || {};
  const mainData = {
    provider: firstModel.provider || 'peer',
    model: firstModel.model || '?',
    prompt_tokens: tokensIn,
    completion_tokens: tokensOut,
    cached_tokens: cached,
    tokens_used: tokensUsed,
    context_window: PEER_USAGE_PLACEHOLDER_CONTEXT,
    usage_pct: (tokensUsed / PEER_USAGE_PLACEHOLDER_CONTEXT) * 100,
  };
  let costJson = null;
  if (snap.cost_usd != null) {
    costJson = JSON.stringify({
      lines: [
        {
          label: `${mainData.provider} / ${mainData.model}`,
          cost: snap.cost_usd,
          input_cost: 0,
          output_cost: 0,
        },
      ],
      total: snap.cost_usd,
    });
  }
  return {
    cmd: 'update_usage',
    main_json: JSON.stringify(mainData),
    presence_json: null,
    live_json: null,
    cost_json: costJson,
    history_json: null,
  };
}

// (full list) or via a pushed `peer_added` / `peer_state_changed`
// event, so this is the single construction point for both surfaces.
function snapshotToDaemonEntry(p) {
  return {
    host_id: p.id,
    label: p.label,
    url: wsUrlToBaseUrl(p.ws_url) || '',
    connected: p.connection_state && p.connection_state.state === 'connected',
    version: p.version || '',
    git_sha: p.git_sha || '',
    ws_url: p.ws_url || '',
    // Browser-side TCP via URL (slice 3a.4). Operator-supplied URL the
    // browser uses to reach the peer's HTTP port for WebRTC ICE-TCP —
    // see PeerConfig.browser_tcp_via_url / AddPeerRequest.browser_tcp_via_url.
    // Takes precedence over ws_url in openPeerDisplay when set.
    browser_tcp_via_url: p.browser_tcp_via_url || '',
    capabilities: p.capabilities || [],
    server_connection_state: p.connection_state,
    server_status: p.status,
    // The peer's sessions as folded server-side from its event stream
    // (SessionInfo shape: session_id, label, phase, source, is_primary,
    // parent_session_id, tokens_used, needs_approval, goal, vitals).
    // Snapshots are authoritative — the same actor fold that feeds the
    // live peer_session_updated events produced this list, so wholesale
    // row replacement never loses session state.
    sessions: Array.isArray(p.sessions) ? p.sessions : [],
    // The peer's advertised displays as folded server-side from its
    // event stream ({display_id, width, height}, ascending display_id).
    // Snapshots are authoritative — the same actor fold that feeds the
    // live peer_display_ready/removed events produced this list, so
    // wholesale row replacement never loses display state.
    displays: Array.isArray(p.displays) ? p.displays : [],
  };
}

// Upsert one folded session snapshot into a peer row's session list
// (live peer_session_updated push). Unknown hosts are ignored — same
// stale-push reasoning as updateDaemonSnapshot.
function upsertPeerSession(hostId, session) {
  if (!hostId || !session || !session.session_id) return;
  const d = daemons.find(x => x.host_id === hostId);
  if (!d) return;
  if (!Array.isArray(d.sessions)) d.sessions = [];
  const idx = d.sessions.findIndex(s => s.session_id === session.session_id);
  if (idx >= 0) d.sessions[idx] = session;
  else d.sessions.push(session);
  stationScheduleUpdate();
}

function removePeerSession(hostId, sessionId) {
  if (!hostId || !sessionId) return;
  const d = daemons.find(x => x.host_id === hostId);
  if (!d || !Array.isArray(d.sessions)) return;
  const idx = d.sessions.findIndex(s => s.session_id === sessionId);
  if (idx < 0) return;
  d.sessions.splice(idx, 1);
  stationScheduleUpdate();
}

// Upsert one advertised display into a peer row's display list (live
// peer_display_ready push). Unknown hosts are ignored — same
// stale-push reasoning as updateDaemonSnapshot. display_id 0 is a
// legitimate id, so guard with == null rather than falsiness. Unlike
// sessions (Station-scene-only), displays also drive the header peer
// chips, so this re-renders the daemons list — the same path the peer
// add/remove/state pushes take, which refreshes the chips
// (stationRenderPeerChips) and ends with the stationScheduleUpdate the
// session helpers call.
function upsertPeerDisplay(hostId, display) {
  if (!hostId || !display || display.display_id == null) return;
  const d = daemons.find(x => x.host_id === hostId);
  if (!d) return;
  if (!Array.isArray(d.displays)) d.displays = [];
  const idx = d.displays.findIndex(x => x.display_id === display.display_id);
  if (idx >= 0) d.displays[idx] = display;
  else d.displays.push(display);
  // Keep ascending display_id order — snapshots arrive sorted, so the
  // live path must preserve the invariant for stable chip/lane order.
  d.displays.sort((a, b) => a.display_id - b.display_id);
  renderDaemonsList();
}

function removePeerDisplay(hostId, displayId) {
  if (!hostId || displayId == null) return;
  const d = daemons.find(x => x.host_id === hostId);
  if (!d || !Array.isArray(d.displays)) return;
  const idx = d.displays.findIndex(x => x.display_id === displayId);
  if (idx < 0) return;
  d.displays.splice(idx, 1);
  // A removed display is no longer viewable — tear down any open pane
  // still streaming it rather than leaving its RTCPeerConnection to
  // stall on ICE failure (same reasoning as removeDaemonById, scoped
  // to the one display).
  closePeerDisplay(hostId, displayId).catch(() => {});
  renderDaemonsList();
}

// Apply a pushed PeerSnapshot to the local daemons list — replace the
// matching entry, or insert a new one. New entries also open a WASM
// secondary connection so per-peer live events flow without waiting
// for the next /api/peers refresh.
//
// Used for `peer_added` events. For `peer_state_changed` use
// `updateDaemonSnapshot` instead — that variant must not insert
// unknown ids, since the per-peer state observer can race a remove
// and emit a trailing PeerStateChanged after the registry has already
// dropped the peer (and after the browser has already removed the row).
function upsertDaemonFromSnapshot(snap) {
  if (!snap || !snap.id) return;
  const entry = snapshotToDaemonEntry(snap);
  const idx = daemons.findIndex(d => d.host_id === entry.host_id);
  if (idx >= 0) {
    daemons[idx] = entry;
  } else {
    daemons.push(entry);
  }
  upsertDashboardAccessTarget(dashboardAccessTargetFromPeerSnapshot(snap));
  renderDaemonsList();
}

// Update an existing daemon entry from a pushed PeerSnapshot. Unknown
// ids are ignored — they're stale state pushes from a removed peer's
// observer racing the disconnect, and inserting them would resurrect
// the row the user just dismissed.
function updateDaemonSnapshot(snap) {
  if (!snap || !snap.id) return;
  const idx = daemons.findIndex(d => d.host_id === snap.id);
  if (idx < 0) return;
  daemons[idx] = snapshotToDaemonEntry(snap);
  upsertDashboardAccessTarget(dashboardAccessTargetFromPeerSnapshot(snap));
  renderDaemonsList();
  if (typeof renderSessionsHostStrip === 'function') renderSessionsHostStrip();
}

// Drop a peer from the local daemons list and tear down its WASM
// secondary connection. Tolerates unknown ids (a `peer_removed` may
// arrive after `refreshPeersFromApi` has already pruned the entry).
function removeDaemonById(id) {
  if (!id) return;
  const idx = daemons.findIndex(d => d.host_id === id);
  if (idx < 0) {
    removeDashboardAccessTarget(id);
    return;
  }
  daemons.splice(idx, 1);
  removeDashboardAccessTarget(id);
  // Drop the expanded-state record so it doesn't get reapplied
  // against a row that no longer exists. Same for any pending
  // approvals — once the peer is gone there's nothing to resolve.
  expandedDaemons.delete(id);
  peerPendingApprovals.delete(id);
  // Tear down any active per-peer display: the peer is gone, the
  // RTCPeerConnection has nowhere to send signaling, and leaving it
  // alive would just stall on ICE failure.
  closePeerDisplaysForHost(id).catch(() => {});
  renderDaemonsList();
}

async function refreshPeersFromApi() {
  try {
    // GET twin (transport F5): tunnel first, direct-HTTP fallback per the
    // verb-derived read policy, never HTTP in Connect mode — the exact
    // legacy tri-form this replaces. A delivered error response leaves
    // the stale list in place, like the legacy !resp.ok return did.
    const resp = await daemonApi.request('api_peers');
    if (!resp.ok) return;
    const data = resp.body || {};
    if (!data.peers || !Array.isArray(data.peers)) return;

    // Build the new daemon entries from the API response via the
    // shared snapshotToDaemonEntry helper so the full-list path and
    // the push-event path stay identical.
    const newDaemons = data.peers.map(snapshotToDaemonEntry);

    // The local snapshot replaces the previous one wholesale —
    // no per-peer add/remove plumbing to maintain anymore now that
    // per-peer events come through the primary /ws push pipeline.
    daemons = newDaemons;
    renderDaemonsList();
    if (typeof renderSessionsHostStrip === 'function') renderSessionsHostStrip();
    const overview = await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
    if (overview) {
      return;
    }
    if (data.targets) {
      applyDashboardAccessTargets(data);
    } else {
      const targets = await refreshDashboardTargetsFromApi({ silent: true });
      if (!targets) syncDashboardAccessTargetsFromDaemons();
    }
  } catch (e) {
    // Silently ignore fetch failures — the dashboard still works
    // from whatever state daemons was in before. Access targets
    // shows stale data until the next refresh, which is better than
    // blanking the panel on a transient network blip.
  }
}

// Fetch the remote daemon's Agent Card to learn its identity, version,
// and capabilities. Used both when adding a new daemon and when
// refreshing on reconnect so the skew-detection dot reflects what's
// actually running. Default credentials mode keeps the CORS handshake
// in the simple lane — the server answers with
// `Access-Control-Allow-Origin: *` instead of echoing the origin.
async function fetchRemoteAgentCard(baseUrl) {
  const url = baseUrl.trim().replace(/\/$/, '') + '/.well-known/agent-card.json';
  const r = await fetch(url);
  if (!r.ok) throw new Error(`fetch ${url} → ${r.status}`);
  const card = await r.json();
  if (!card.label || !card.id) {
    throw new Error('remote agent card missing id/label');
  }
  return card;
}

// Last-rendered snapshot guards: peer status flips (idle↔working) arrive per
// remote task activity and used to rebuild the whole daemons list — plus a
// seven-surface cascade (host filter sweep over up to 10k log entries, three
// select rebuilds) — even when nothing rendered had changed. The row HTML is
// its own signature for the list; the pickers' inputs (id/label/connected)
// get a dedicated one so a mid-row change (e.g. version skew) doesn't churn
// the selects.
let daemonsListHostOptionsSig = null;

function daemonsListHostOptionsSignature() {
  return JSON.stringify([
    selfPeerId,
    selfHostLabel,
    ...daemons.map(d => [d.host_id, d.label || '', d.connected !== false]),
  ]);
}

function renderDaemonsList() {
  const el = document.getElementById('daemons-list');
  if (!el) return;

  const rows = [];
  // Self entry first — always present, can't be removed.
  // host_id is the stable PeerId routing key; label is the display name.
  rows.push(renderDaemonRow({
    host_id: selfPeerId,
    label: selfHostLabel,
    url: location.origin,
    connected: true,
    version: selfVersion,
    git_sha: selfGitSha,
  }, true));
  for (const d of daemons) {
    rows.push(renderDaemonRow(d, false));
  }
  const html = rows.join('');
  if (el.__intendantRenderedHtml === html) {
    // Identical rendered list: the existing DOM (with its listeners,
    // expansion state, approvals section, and attached display panes)
    // stays — skip straight to the cheap always-on consumers below.
    renderDaemonsListTail();
    return;
  }
  el.__intendantRenderedHtml = html;
  el.innerHTML = html;

  // Wire remove buttons.
  el.querySelectorAll('.daemon-row .remove-btn').forEach(btn => {
    btn.addEventListener('click', () => removeDaemon(btn.dataset.hostId));
  });

  // Wire per-peer expand toggles. The list re-renders on every push
  // event, so the expansion state lives in `expandedDaemons` (a Set
  // outside this function) and is reapplied after each render below.
  el.querySelectorAll('.daemon-row .expand-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      const hostId = btn.dataset.hostId;
      const expanding = !expandedDaemons.has(hostId);
      if (expanding) expandedDaemons.add(hostId);
      else expandedDaemons.delete(hostId);
      applyDaemonExpandedState(hostId, expanding);
      if (expanding) {
        const input = document.querySelector(
          `.daemon-msg-input[data-host-id="${CSS.escape(hostId)}"]`
        );
        if (input) input.focus();
      }
    });
  });

  // Wire per-peer message send: button click + Enter in the input
  // both call sendPeerMessage with the row's host_id. Task button
  // delegates a fresh task via the same input — distinct endpoint,
  // distinct ControlMsg on the wire (FollowUp vs StartTask).
  el.querySelectorAll('.daemon-msg-send').forEach(btn => {
    btn.addEventListener('click', () => sendPeerMessage(btn.dataset.hostId));
  });
  el.querySelectorAll('.daemon-task-send').forEach(btn => {
    btn.addEventListener('click', () => sendPeerTask(btn.dataset.hostId));
  });
  el.querySelectorAll('.daemon-msg-input').forEach(input => {
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        // Shift-Enter starts a task; plain Enter sends a message.
        // Mirrors the visual order of the two buttons (Send, Task).
        if (e.shiftKey) {
          sendPeerTask(input.dataset.hostId);
        } else {
          sendPeerMessage(input.dataset.hostId);
        }
      }
    });
  });

  // Wire the "View display" button: lazy create on first click. The
  // button only exists for peers whose card advertises Capability::Display.
  el.querySelectorAll('.daemon-display-view').forEach(btn => {
    btn.addEventListener('click', () => {
      const displayId = parseInt(btn.dataset.displayId || '0', 10);
      // data-tcp-via-url is pre-resolved at render time (see renderDaemonRow):
      // d.browser_tcp_via_url when set, else d.ws_url.
      const tcpViaUrl = btn.dataset.tcpViaUrl || '';
      openPeerDisplay(btn.dataset.hostId, displayId, tcpViaUrl).catch(err => {
        console.error('openPeerDisplay failed:', err);
      });
    });
  });

  // Reapply expansion state for any rows that were open before this
  // re-render (e.g. user expanded a peer's controls, then a push
  // event triggered re-render). Without this the panel collapses on
  // every state change, which would feel broken.
  expandedDaemons.forEach(hostId => applyDaemonExpandedState(hostId, true));

  // Reapply pending-approval rendering for the same reason. The
  // controls panel's HTML was just regenerated, so any approvals
  // section we previously inserted is gone — rebuild it.
  peerPendingApprovals.forEach((_, hostId) => renderPeerApprovals(hostId));

  // Reapply per-peer WebRTC display panes so live MediaStreams stay
  // attached to the freshly-rendered <video> elements after re-render.
  reapplyPeerDisplayPanes();

  renderDaemonsListTail();
}

// The always-on consumers of a daemons-list render — run on every call,
// whether or not the row DOM was rebuilt. The four host pickers rebuild
// only when their actual inputs changed (see daemonsListHostOptionsSig):
// refreshHostFilterOptions ends in applyHostFilter, a class-toggle sweep
// over every retained log entry, and each picker resets its <select>.
function renderDaemonsListTail() {
  // Keep the Station header's peer-display chips in sync with the same
  // peer add/remove/state events that re-render this list. Cheap
  // (replaceChildren over a handful of peers) and display lists aren't
  // fully encoded in the row HTML, so this stays unconditional.
  stationRenderPeerChips();

  // Toggle host-badge visibility in the Activity tab. Single-host
  // setups stay visually clean; multi-host setups see the badges.
  const logStream = document.getElementById('log-stream');
  if (logStream) {
    logStream.classList.toggle('show-host-badges', daemons.length > 0);
  }

  const optionsSig = daemonsListHostOptionsSignature();
  if (daemonsListHostOptionsSig !== optionsSig) {
    daemonsListHostOptionsSig = optionsSig;

    // Keep the Activity host filter dropdown in sync with the current
    // daemon list (options may have been added/removed).
    refreshHostFilterOptions();

    // Same for the Stats tab host picker.
    refreshStatsHostPicker();

    // Same for the Files tab download source selector.
    refreshFilesDownloadHostOptions();

    // Same for the Terminal Shell target selector.
    refreshShellHostOptions();
  }

  // Target summaries reuse the same daemon list and should track
  // add/remove/offline transitions immediately.
  renderDashboardTargetSummaries();

  // Refresh the aggregate connection dot in the status bar.
  stationScheduleUpdate();
}


// Render the human-readable text for one Capability JSON value as it
// arrives in the /api/peers response. Built-in variants serialize as
// {kind: "computer-use"}; Custom serializes as {kind: "custom", name: "..."}.
function capabilityLabel(cap) {
  if (!cap || typeof cap !== 'object') return String(cap || '');
  if (cap.kind === 'custom') return cap.name || 'custom';
  return cap.kind || '';
}

// Subdued pill showing the server-side ConnectionState. Suppressed when
// the state is `connected`, since the conn-dot already paints that.
// Reconnecting carries an attempt counter that matters operationally
// ("attempt 3" tells you whether the peer is in a tight retry loop).
function renderStatePill(connState) {
  if (!connState || !connState.state) return '';
  const state = connState.state;
  if (state === 'connected') return '';
  const label = state === 'reconnecting' && typeof connState.attempt === 'number'
    ? `${state} · attempt ${connState.attempt}`
    : state;
  return `<span class="state-pill" title="connection state: ${escapeHtml(label)}">${escapeHtml(label)}</span>`;
}

// Subdued chip showing the peer's PeerStatus. Hidden in the steady-state
// values (`idle`, `working`) so the chip doesn't compete with the label
// for attention; only `needs_approval` and `error` surface visibly.
function renderStatusChip(status) {
  if (!status) return '';
  if (status === 'needs_approval') {
    return `<span class="status-chip needs-approval" title="peer needs approval">needs approval</span>`;
  }
  if (status === 'error') {
    return `<span class="status-chip error" title="peer is in error state">error</span>`;
  }
  return '';
}

// Render up to MAX_CAP_BADGES capability chips with a "+N" overflow
// indicator. The cap keeps a peer with many capabilities from pushing
// the row's url and version off the right side of the panel.
const MAX_CAP_BADGES = 4;
function renderCapBadges(caps) {
  if (!Array.isArray(caps) || caps.length === 0) return '';
  const visible = caps.slice(0, MAX_CAP_BADGES);
  const overflow = caps.length - visible.length;
  const chips = visible
    .map(c => {
      const label = capabilityLabel(c);
      return `<span class="cap-badge" title="${escapeHtml(label)}">${escapeHtml(label)}</span>`;
    })
    .join('');
  const more = overflow > 0
    ? `<span class="cap-overflow" title="${overflow} more capability${overflow === 1 ? '' : 'ies'}">+${overflow}</span>`
    : '';
  return `<div class="caps">${chips}${more}</div>`;
}

function renderDaemonRow(d, isSelf) {
  // Dot colors:
  //   green → connected + version matches self (or is self)
  //   yellow → connected but git SHA differs from self (version skew)
  //   gray/red → not connected
  const skewed = !isSelf && d.connected && selfGitSha && d.git_sha && d.git_sha !== selfGitSha;
  let dotClass, dotTitle;
  if (isSelf || (d.connected && !skewed)) {
    dotClass = 'ok';
    dotTitle = 'connected';
  } else if (skewed) {
    dotClass = 'warn';
    dotTitle = `version mismatch: self=${selfGitSha} remote=${d.git_sha} — rebuild the remote to match`;
  } else {
    dotClass = 'err';
    dotTitle = 'disconnected';
  }
  const selfMark = isSelf ? ' (this daemon)' : '';
  const removeBtn = isSelf
    ? ''
    : `<button class="remove-btn" data-host-id="${escapeHtml(d.host_id)}" title="Remove this daemon">Remove</button>`;
  const versionText = d.git_sha
    ? `${escapeHtml(d.version || '?')} · ${escapeHtml(d.git_sha)}`
    : '';
  // State pill and status chip are server-side fields; the self entry
  // is constructed inline in renderDaemonsList without them, so skip
  // for self. Capability badges render whenever the field is present
  // (so if the self entry ever gains them, no further change needed).
  const statePill = isSelf ? '' : renderStatePill(d.server_connection_state);
  const statusChip = isSelf ? '' : renderStatusChip(d.server_status);
  const capBadges = renderCapBadges(d.capabilities);
  // Per-peer outbound op controls. Skipped for self — federation ops
  // address other peers, never the daemon hosting the dashboard. The
  // expand toggle starts collapsed; expandedDaemons re-expands rows
  // that were already open before a re-render.
  const expandBtn = isSelf
    ? ''
    : `<button class="expand-btn" data-host-id="${escapeHtml(d.host_id)}" title="Show peer controls">▾</button>`;
  const peerLabel = escapeHtml(d.label || d.host_id);
  // Capability-gated "View display" button — only peers whose card
  // advertises Capability::Display get the affordance. Slice 3a opens
  // display_id 0 by default; multi-display picker is a follow-up.
  //
  // `data-tcp-via-url` carries the URL the browser would use to reach
  // the peer's HTTP port — passed as `advertise_tcp_via_url` in the
  // Offer so the peer can advertise an ICE-TCP candidate the browser
  // can actually dial (slice 3a.2, operator-configurable since 3a.4).
  // resolveBrowserTcpViaUrl applies the precedence (operator URL →
  // ws_url → empty) and filters loopback at either position so slice
  // 3b's primary-as-media-relay takes over when no non-loopback path
  // is known — same contract the peer-side loopback warn describes.
  const tcpViaUrl = resolveBrowserTcpViaUrl(d);
  const viewDisplayBtn = !isSelf && peerCanShareDisplay(d)
    ? `<button class="daemon-display-view" data-host-id="${escapeHtml(d.host_id)}" data-display-id="0" data-tcp-via-url="${escapeHtml(tcpViaUrl)}" title="Open this peer's display via WebRTC (browser↔peer direct, primary signals only)">View display</button>`
    : '';
  const controlsPanel = isSelf
    ? ''
    : `<div class="daemon-controls" id="daemon-controls-${escapeHtml(d.host_id)}" style="display:none">
         <div class="daemon-msg-row">
           <input class="daemon-msg-input" type="text" placeholder="Send a message or task to ${peerLabel}…" data-host-id="${escapeHtml(d.host_id)}">
           <button class="daemon-msg-send" data-host-id="${escapeHtml(d.host_id)}" title="Send as a follow-up message in the peer's current conversation">Send</button>
           <button class="daemon-task-send" data-host-id="${escapeHtml(d.host_id)}" title="Start a new task on the peer with these instructions">Task</button>
           ${viewDisplayBtn}
         </div>
         <div class="daemon-msg-status" data-host-id="${escapeHtml(d.host_id)}"></div>
         <div class="peer-display-container" id="daemon-peer-display-${escapeHtml(d.host_id)}" style="display:none"></div>
       </div>`;
  return `
    <div class="daemon-entry${isSelf ? ' self' : ''}">
      <div class="daemon-row${isSelf ? ' self' : ''}">
        <span class="conn-dot ${dotClass}" title="${escapeHtml(dotTitle)}"></span>
        <span class="label">${peerLabel}${selfMark}</span>
        ${statePill}
        ${statusChip}
        <span class="url" title="${escapeHtml(d.url)}">${escapeHtml(d.url)}</span>
        ${capBadges}
        <span class="version" title="version · git SHA">${versionText}</span>
        ${expandBtn}
        ${removeBtn}
      </div>
      ${controlsPanel}
    </div>
  `;
}

// Sync the visual state of a peer's expand toggle + controls panel
// to `expanded`. Used both by the click handler (when the user
// toggles) and at the end of renderDaemonsList (to reapply state
// across re-renders).
function applyDaemonExpandedState(hostId, expanded) {
  const target = document.getElementById(`daemon-controls-${hostId}`);
  if (target) target.style.display = expanded ? 'flex' : 'none';
  const btn = document.querySelector(
    `.expand-btn[data-host-id="${CSS.escape(hostId)}"]`
  );
  if (btn) {
    btn.classList.toggle('expanded', expanded);
    btn.textContent = expanded ? '▴' : '▾';
    btn.title = expanded ? 'Hide peer controls' : 'Show peer controls';
  }
}
