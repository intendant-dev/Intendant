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
      dashboardSetControlLastError(this.lastError);
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
    dashboardSetControlLastError(this.lastError);
    dashboardUpdateTransportStatus();
    this.close();
    this.scheduleReconnect(this.lastError, { delayMs: 1000 });
  }

  scheduleReconnect(reason, options = {}) {
    if (!this.primaryDashboardControl || this.suppressReconnect || !dashboardConnectModeEnabled()) return;
    scheduleDashboardConnectReconnect(reason, options);
  }

  handleOpen() {
    this.lastError = '';
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
        dashboardServerMessageDispatcher(JSON.stringify(msg));
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
      console.warn('[dashboard-control] event gap', msg.skipped || 0);
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
    dashboardUpdateTransportStatus();
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
    dashboardUpdateTransportStatus();
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
    this.sendFrame({
      t: 'display_input',
      display_id: Number(displayId) || 0,
      event,
    });
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
      throw new Error(body.error || `${path} returned ${resp.status}`);
    }
    return body;
  }

  async connectSignalHeaders() {
    if (!this.connectCsrfToken) {
      const resp = await fetch('/api/me');
      const body = await resp.json().catch(() => ({}));
      this.connectCsrfToken = String(body.csrf_token || '');
    }
    const headers = { 'content-type': 'application/json' };
    if (this.connectCsrfToken) headers['x-intendant-csrf'] = this.connectCsrfToken;
    return headers;
  }

  sendWsSignal(frame) {
    if (app && app.send_server_action) {
      app.send_server_action(frame);
      return true;
    }
    return false;
  }

  async sendOffer(sdp) {
    if (dashboardConnectModeEnabled()) {
      if (!DASHBOARD_CONNECT_DAEMON_ID) {
        throw new Error('Connect dashboard missing daemon_id');
      }
      const identity = await clientIdentityOfferFields(
        DASHBOARD_CONNECT_DAEMON_ID,
        this.clientNonce,
        sdp
      );
      // A stored org grant rides along so a daemon that trusts the org
      // materializes it before resolving this very offer (one-round-trip
      // first contact). The daemon re-verifies everything.
      const orgGrant = await orgGrantForOffer(DASHBOARD_CONNECT_DAEMON_ID);
      const answer = await this.postConnectSignal('/api/browser/offer', {
        daemon_id: DASHBOARD_CONNECT_DAEMON_ID,
        sdp,
        client_nonce: this.clientNonce,
        ...identity,
        ...(orgGrant ? { org_grant: orgGrant } : {}),
      });
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
      if (this.sendWsSignal({ t: 'dashboard_control_offer', sdp, client_nonce: this.clientNonce })) {
        this.signalingMode = 'websocket-fallback';
        dashboardUpdateTransportStatus();
        return null;
      }
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
      eventsActive: dashboardControlEventsActive,
      grantKind: this.lastStatus?.grant_kind ?? null,
      grantLabel: this.lastStatus?.grant_label ?? null,
      accessPrincipal: this.lastStatus?.access_principal ?? null,
      apiAgentCardAvailable: this.lastStatus?.api_agent_card_available ?? null,
      apiCachedBootstrapEventsAvailable: this.lastStatus?.api_cached_bootstrap_events_available ?? null,
      apiBrowserWorkspaceSnapshotAvailable: this.lastStatus?.api_browser_workspace_snapshot_available ?? null,
      apiStateSnapshotAvailable: this.lastStatus?.api_state_snapshot_available ?? null,
      apiDisplayBootstrapAvailable: this.lastStatus?.api_display_bootstrap_available ?? null,
      apiDisplayInputAuthorityAvailable: this.lastStatus?.api_display_input_authority_available ?? null,
      apiDisplayWebRtcSignalAvailable: this.lastStatus?.api_display_webrtc_signal_available ?? null,
      apiSessionLogReplayAvailable: this.lastStatus?.api_session_log_replay_available ?? null,
      apiExternalSessionActivityReplayAvailable: this.lastStatus?.api_external_session_activity_replay_available ?? null,
      apiDashboardBootstrapAvailable: this.lastStatus?.api_dashboard_bootstrap_available ?? null,
      apiPeersAvailable: this.lastStatus?.api_peers_available ?? null,
      apiSessionsAvailable: this.lastStatus?.api_sessions_available ?? null,
      apiSessionsStreamAvailable: this.lastStatus?.api_sessions_stream_available ?? null,
      byteStreamsAvailable: this.lastStatus?.byte_streams_available ?? null,
      uploadFramesAvailable: this.lastStatus?.upload_frames_available ?? null,
      terminalFramesAvailable: this.lastStatus?.terminal_frames_available ?? null,
      presenceFramesAvailable: this.lastStatus?.presence_frames_available ?? null,
      presenceActiveHandoffAvailable: this.lastStatus?.presence_active_handoff_available ?? null,
      presenceToolRequestAvailable: this.lastStatus?.presence_tool_request_available ?? null,
      accessInspectAvailable: this.lastStatus?.access_inspect_available ?? null,
      accessManageAvailable: this.lastStatus?.access_manage_available ?? null,
      apiAccessIamUpsertUserClientGrantAvailable: this.lastStatus?.api_access_iam_upsert_user_client_grant_available ?? null,
      apiAccessIamUpdateGrantAvailable: this.lastStatus?.api_access_iam_update_grant_available ?? null,
      peerInspectAvailable: this.lastStatus?.peer_inspect_available ?? null,
      peerManageAvailable: this.lastStatus?.peer_manage_available ?? null,
      apiPresenceVideoFrameAvailable: this.lastStatus?.api_presence_video_frame_available ?? null,
      apiSessionDetailAvailable: this.lastStatus?.api_session_detail_available ?? null,
      apiSessionReportAvailable: this.lastStatus?.api_session_report_available ?? null,
      apiSessionDeleteAvailable: this.lastStatus?.api_session_delete_available ?? null,
      apiSessionAgentOutputAvailable: this.lastStatus?.api_session_agent_output_available ?? null,
      apiSessionCurrentAgentOutputAvailable: this.lastStatus?.api_session_current_agent_output_available ?? null,
      apiSessionCurrentHistoryAvailable: this.lastStatus?.api_session_current_history_available ?? null,
      apiSessionCurrentRollbackAvailable: this.lastStatus?.api_session_current_rollback_available ?? null,
      apiSessionCurrentRedoAvailable: this.lastStatus?.api_session_current_redo_available ?? null,
      apiSessionCurrentPruneAvailable: this.lastStatus?.api_session_current_prune_available ?? null,
      apiSessionCurrentChangesAvailable: this.lastStatus?.api_session_current_changes_available ?? null,
      apiSessionContextSnapshotAvailable: this.lastStatus?.api_session_context_snapshot_available ?? null,
      apiSessionCurrentUploadAvailable: this.lastStatus?.api_session_current_upload_available ?? null,
      apiSessionCurrentUploadsAvailable: this.lastStatus?.api_session_current_uploads_available ?? null,
      apiSessionCurrentUploadRawAvailable: this.lastStatus?.api_session_current_upload_raw_available ?? null,
      apiSessionCurrentUploadDeleteAvailable: this.lastStatus?.api_session_current_upload_delete_available ?? null,
      apiTransferJobsAvailable: this.lastStatus?.api_transfer_jobs_available ?? null,
      apiTransferJobCreateAvailable: this.lastStatus?.api_transfer_job_create_available ?? null,
      apiTransferJobDeleteAvailable: this.lastStatus?.api_transfer_job_delete_available ?? null,
      apiTransferDownloadReadAvailable: this.lastStatus?.api_transfer_download_read_available ?? null,
      apiTransferUploadChunkAvailable: this.lastStatus?.api_transfer_upload_chunk_available ?? null,
      apiTransferUploadCommitAvailable: this.lastStatus?.api_transfer_upload_commit_available ?? null,
      apiMediaEditorAvailable: this.lastStatus?.api_media_editor_available ?? null,
      apiMediaAnnotationAttachAvailable: this.lastStatus?.api_media_annotation_attach_available ?? null,
      apiMediaAnnotationSubmitAvailable: this.lastStatus?.api_media_annotation_submit_available ?? null,
      apiMediaClipStartAvailable: this.lastStatus?.api_media_clip_start_available ?? null,
      apiMediaClipFrameAvailable: this.lastStatus?.api_media_clip_frame_available ?? null,
      apiMediaClipEndAvailable: this.lastStatus?.api_media_clip_end_available ?? null,
      apiMediaClipCancelAvailable: this.lastStatus?.api_media_clip_cancel_available ?? null,
      apiFsStatAvailable: this.lastStatus?.api_fs_stat_available ?? null,
      apiFsListAvailable: this.lastStatus?.api_fs_list_available ?? null,
      apiFsMkdirAvailable: this.lastStatus?.api_fs_mkdir_available ?? null,
      apiFsReadAvailable: this.lastStatus?.api_fs_read_available ?? null,
      apiSessionsSearchAvailable: this.lastStatus?.api_sessions_search_available ?? null,
      apiSettingsAvailable: this.lastStatus?.api_settings_available ?? null,
      apiSettingsSaveAvailable: this.lastStatus?.api_settings_save_available ?? null,
      apiControlMsgAvailable: this.lastStatus?.api_control_msg_available ?? null,
      apiSessionControlMsgAvailable: this.lastStatus?.api_session_control_msg_available ?? null,
      apiDashboardActionMsgAvailable: this.lastStatus?.api_dashboard_action_msg_available ?? null,
      apiDiagnosticsVisualFreshnessAvailable: this.lastStatus?.api_diagnostics_visual_freshness_available ?? null,
      apiKeyStatusAvailable: this.lastStatus?.api_key_status_available ?? null,
      apiApiKeysSaveAvailable: this.lastStatus?.api_api_keys_save_available ?? null,
      apiVoiceSessionAvailable: this.lastStatus?.api_voice_session_available ?? null,
      apiProjectRootAvailable: this.lastStatus?.api_project_root_available ?? null,
      apiDisplaysAvailable: this.lastStatus?.api_displays_available ?? null,
      apiRecordingsAvailable: this.lastStatus?.api_recordings_available ?? null,
      apiRecordingAssetAvailable: this.lastStatus?.api_recording_asset_available ?? null,
      apiSessionRecordingsAvailable: this.lastStatus?.api_session_recordings_available ?? null,
      apiSessionRecordingAssetAvailable: this.lastStatus?.api_session_recording_asset_available ?? null,
      apiSessionFrameAssetAvailable: this.lastStatus?.api_session_frame_asset_available ?? null,
      apiWorktreesAvailable: this.lastStatus?.api_worktrees_available ?? null,
      apiWorktreesInspectAvailable: this.lastStatus?.api_worktrees_inspect_available ?? null,
      apiWorktreesScanAvailable: this.lastStatus?.api_worktrees_scan_available ?? null,
      apiWorktreesRemoveAvailable: this.lastStatus?.api_worktrees_remove_available ?? null,
      apiManagedContextAvailable: this.lastStatus?.api_managed_context_available ?? null,
      apiMcpToolCallAvailable: this.lastStatus?.api_mcp_tool_call_available ?? null,
      apiPeerMutationsAvailable: this.lastStatus?.api_peer_mutations_available ?? null,
      apiPeerPairingAvailable: this.lastStatus?.api_peer_pairing_available ?? null,
      apiPeerPairingInviteAvailable: this.lastStatus?.api_peer_pairing_invite_available ?? null,
      apiPeerPairingJoinAvailable: this.lastStatus?.api_peer_pairing_join_available ?? null,
      apiPeerPairingRequestAccessAvailable: this.lastStatus?.api_peer_pairing_request_access_available ?? null,
      apiPeerPairingRequestDecisionAvailable: this.lastStatus?.api_peer_pairing_request_decision_available ?? null,
      apiPeerPairingRequestsAvailable: this.lastStatus?.api_peer_pairing_requests_available ?? null,
      apiPeerPairingIdentitiesAvailable: this.lastStatus?.api_peer_pairing_identities_available ?? null,
      apiPeerPairingIdentityRevokeAvailable: this.lastStatus?.api_peer_pairing_identity_revoke_available ?? null,
      apiPeerWebRtcSignalAvailable: this.lastStatus?.api_peer_webrtc_signal_available ?? null,
      apiPeerFileTransferSignalAvailable: this.lastStatus?.api_peer_file_transfer_signal_available ?? null,
      apiPeerDashboardControlSignalAvailable: this.lastStatus?.api_peer_dashboard_control_signal_available ?? null,
      apiCoordinatorAvailable: this.lastStatus?.api_coordinator_available ?? null,
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
    const resp = await dashboardTransport.peerDashboardControlSignal(this.hostId, {
      session_id: this.sessionId,
      signal,
    }, {
      signal: options.signal,
    });
    if (!resp.ok) {
      const detail = await resp.json().catch(() => ({}));
      throw new Error(`peer dashboard-control signal failed (${resp.status}): ${detail.error || 'unknown'}`);
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
  let binary = '';
  for (let i = 0; i < bytes.byteLength; i += 1) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
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
  return dashboardTransport.rpcOrHttp('api_session_detail', params, async () => {
    const query = new URLSearchParams();
    query.set('source', source);
    if (options.limit !== undefined && options.limit !== null) {
      query.set('limit', String(options.limit));
    }
    if (options.before !== undefined && options.before !== null) {
      query.set('before', String(options.before));
    }
    const fetchOptions = {};
    if (options.signal) fetchOptions.signal = options.signal;
    if (options.cache) fetchOptions.cache = options.cache;
    const resp = await fetch(`/api/session/${encodeURIComponent(sid)}?${query.toString()}`, fetchOptions);
    let data = {};
    try {
      data = await resp.json();
    } catch {
      data = {};
    }
    if (!resp.ok && !data.error) {
      data.error = resp.statusText || `HTTP ${resp.status}`;
    }
    data._httpStatus = resp.status;
    return data;
  }, 'api_session_detail', { signal: options.signal });
}

async function fetchSessionAgentOutputPayload(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) throw new Error('missing session id');
  const source = String(options.source || 'intendant').trim() || 'intendant';
  const ids = Array.isArray(options.ids)
    ? options.ids.map(id => String(id || '').trim()).filter(Boolean)
    : [];
  if (!ids.length) throw new Error('missing output ids');
  const params = { session_id: sid, source, ids };
  return dashboardTransport.rpcOrHttp('api_session_agent_output', params, async () => {
    const query = new URLSearchParams();
    query.set('source', source);
    const fetchOptions = {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ids }),
    };
    if (options.signal) fetchOptions.signal = options.signal;
    if (options.cache) fetchOptions.cache = options.cache;
    const resp = await fetch(`/api/session/${encodeURIComponent(sid)}/agent-output?${query.toString()}`, fetchOptions);
    let data = {};
    try {
      data = await resp.json();
    } catch {
      data = {};
    }
    if (!resp.ok && !data.error) {
      data.error = resp.statusText || `HTTP ${resp.status}`;
    }
    data._httpStatus = resp.status;
    data._httpOk = resp.ok;
    return data;
  }, 'api_session_agent_output', { signal: options.signal });
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
  return dashboardTransport.rpcOrHttp('api_sessions_search', {
    q: query,
    source,
    mode,
    projects,
  }, async () => {
    const params = new URLSearchParams({ q: query, source, mode });
    if (projects.length > 0) {
      params.set('projects', JSON.stringify(projects));
    }
    const resp = await authedFetch(`/api/sessions/search?${params.toString()}`, {
      signal: options.signal,
    });
    if (!resp.ok) throw new Error(`/api/sessions/search returned ${resp.status}`);
    return resp.json();
  }, 'api_sessions_search', { signal: options.signal });
}

async function fetchDashboardSettings() {
  return dashboardTransport.rpcOrHttp('api_settings', {}, async () => {
    const resp = await fetch('/api/settings');
    if (!resp.ok) throw new Error(`/api/settings returned ${resp.status}`);
    return resp.json();
  }, 'api_settings');
}

async function fetchApiKeyStatus() {
  return dashboardTransport.rpcOrHttp('api_key_status', {}, async () => {
    const resp = await fetch('/api/api-key-status');
    if (!resp.ok) throw new Error(`/api/api-key-status returned ${resp.status}`);
    return resp.json();
  }, 'api_key_status');
}

async function fetchExternalAgentAvailability() {
  return dashboardTransport.rpcOrHttp('api_external_agents', {}, async () => {
    const resp = await fetch('/api/external-agents');
    if (!resp.ok) throw new Error(`/api/external-agents returned ${resp.status}`);
    return resp.json();
  }, 'api_external_agents');
}

async function fetchProjectRoot() {
  return dashboardTransport.rpcOrHttp('api_project_root', {}, async () => {
    const resp = await fetch('/api/project-root');
    if (!resp.ok) throw new Error(`/api/project-root returned ${resp.status}`);
    return resp.json();
  }, 'api_project_root');
}

function dashboardReportRpcAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.api_session_report_available === true
  );
}

function dashboardByteStreamMethodAvailable(method) {
  const status = dashboardControlTransport?.lastStatus || {};
  const field = {
    api_fs_read: 'api_fs_read_available',
    api_transfer_download_read: 'api_transfer_download_read_available',
    api_recording_asset: 'api_recording_asset_available',
    api_session_recording_asset: 'api_session_recording_asset_available',
    api_session_frame_asset: 'api_session_frame_asset_available',
    api_session_current_upload_raw: 'api_session_current_upload_raw_available',
    api_session_report: 'api_session_report_available',
  }[method];
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    status.byte_streams_available === true &&
    (!field || status[field] === true)
  );
}

function dashboardTransferDownloadAvailable() {
  const status = dashboardControlTransport?.lastStatus || {};
  return dashboardByteStreamMethodAvailable('api_transfer_download_read') &&
    status.api_transfer_job_create_available === true;
}

function dashboardTransferUploadAvailable() {
  const status = dashboardControlTransport?.lastStatus || {};
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    status.upload_frames_available === true &&
    status.api_transfer_job_create_available === true &&
    status.api_transfer_upload_chunk_available === true &&
    status.api_transfer_upload_commit_available === true
  );
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
    const useByteStream = dashboardControlTransport?.lastStatus?.byte_streams_available === true;
    const report = useByteStream
      ? await dashboardTransport.requestBytes('api_session_report', {
          session_id: 'current',
        }, { timeoutMs: 120000 })
      : await dashboardTransport.request('api_session_report', {
          session_id: 'current',
        }, { timeoutMs: 120000 });
    if (report?.ok === false || report?._httpOk === false) {
      throw new Error(report.error || `Session report failed (${report._httpStatus || 'error'})`);
    }
    const bytes = report?.bytes instanceof Uint8Array
      ? report.bytes
      : dashboardControlBase64ToBytes(report?.data_base64 || '');
    if (!bytes.length) throw new Error('Session report was empty');
    downloadDashboardBytes(
      bytes,
      report.filename || 'intendant-session-report.zip',
      report.content_type || 'application/zip'
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
  return dashboardTransport.rpcOrHttp('api_displays', {}, async () => {
    const resp = await authedFetch('/api/displays');
    const data = await resp.json().catch(() => ({}));
    if (!resp.ok) throw new Error(data.error || `HTTP ${resp.status}`);
    return data;
  }, 'api_displays');
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
    let data = null;
    if (dashboardTransport?.canUseRpc()) {
      try {
        data = await dashboardTransport.request('api_peers');
      } catch (err) {
        if (dashboardConnectModeEnabled()) throw err;
        console.warn('[dashboard-control] api_peers RPC failed, falling back to HTTP', err);
      }
    }
    if (!data && dashboardConnectModeEnabled()) {
      throw new Error('Peer list is unavailable until dashboard access reconnects');
    }
    if (!data) {
      const resp = await authedFetch('/api/peers');
      if (!resp.ok) return;
      data = await resp.json();
    }
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
  el.innerHTML = rows.join('');

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

  // Keep the Station header's peer-display chips in sync with the same
  // peer add/remove/state events that re-render this list.
  stationRenderPeerChips();

  // Toggle host-badge visibility in the Activity tab. Single-host
  // setups stay visually clean; multi-host setups see the badges.
  const logStream = document.getElementById('log-stream');
  if (logStream) {
    logStream.classList.toggle('show-host-badges', daemons.length > 0);
  }

  // Keep the Activity host filter dropdown in sync with the current
  // daemon list (options may have been added/removed).
  refreshHostFilterOptions();

  // Same for the Stats tab host picker.
  refreshStatsHostPicker();

  // Same for the Files tab download source selector.
  refreshFilesDownloadHostOptions();

  // Same for the Terminal Shell target selector.
  refreshShellHostOptions();

  // Target summaries reuse the same daemon list and should track
  // add/remove/offline transitions immediately.
  renderDashboardTargetSummaries();

  // Refresh the aggregate connection dot in the status bar.
  updateHostsAggregateDot();
  stationScheduleUpdate();
}

// Update the always-visible status-bar dot summarizing multi-host
// connection state. Priority: red (any disconnected) > yellow (any
// SHA mismatch) > green. The whole group (separator + label + dot) is
// hidden when no secondaries are configured so single-host setups
// stay clean.
function updateHostsAggregateDot() {
  const group = document.getElementById('sb-hosts-group');
  const dot = document.getElementById('sb-hosts-dot');
  if (!dot || !group) return;
  if (daemons.length === 0) {
    group.classList.add('hidden');
    return;
  }
  group.classList.remove('hidden');

  let anyDisconnected = false;
  let anySkewed = false;
  const skewedHosts = [];
  const disconnectedHosts = [];
  for (const d of daemons) {
    if (!d.connected) {
      anyDisconnected = true;
      disconnectedHosts.push(d.label || d.host_id);
      continue;
    }
    if (selfGitSha && d.git_sha && d.git_sha !== selfGitSha) {
      anySkewed = true;
      skewedHosts.push(`${d.label || d.host_id} (${d.git_sha} vs ${selfGitSha})`);
    }
  }

  dot.classList.remove('ok', 'warn', 'err');
  if (anyDisconnected) {
    dot.classList.add('err');
    dot.title = `Disconnected: ${disconnectedHosts.join(', ')}`;
  } else if (anySkewed) {
    dot.classList.add('warn');
    dot.title = `Version mismatch: ${skewedHosts.join(', ')}. Rebuild to match self (${selfGitSha}).`;
  } else {
    dot.classList.add('ok');
    dot.title = `All ${daemons.length} secondary daemon${daemons.length === 1 ? '' : 's'} connected and on matching version`;
  }
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

// POST a message to a peer via /api/peers/{id}/message and surface
// the result in the row's status line. The peer id is embedded in
// the URL path verbatim — colons are valid path chars per RFC 3986
// and the server-side parser splits on the literal `:` to recover
// the kind prefix; URL-encoding `:` as `%3A` would break the lookup
// because the registry keys on the un-encoded id.
