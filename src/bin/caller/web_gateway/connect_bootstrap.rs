//! Connect-relayed dashboard bootstrap: offer/ice/close responses for
//! browsers arriving via the rendezvous, and the self-contained bootstrap
//! HTML page.

use super::*;

pub(crate) async fn connect_dashboard_offer_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
    agent_card: &serde_json::Value,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let sdp = body.get("sdp").and_then(|v| v.as_str()).unwrap_or("");
    if sdp.trim().is_empty() {
        return json_error("400 Bad Request", "missing sdp");
    }
    let client_nonce = body
        .get("client_nonce")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|nonce| !nonce.is_empty())
        .map(str::to_string);
    // Reaching this path already required a trusted transport (mTLS or local
    // loopback), so sessions stay root-compatible. A signed browser identity
    // key refines that: a key with a local IAM grant binds the session to
    // that scoped principal, and an ungranted key keeps root authority but
    // surfaces its fingerprint so the Access UI can offer enrollment. An
    // invalid signature fails closed.
    let client_key_fields: crate::access::client_key::ClientKeyOfferFields =
        serde_json::from_value(body.clone()).unwrap_or_default();
    let verified_client_key = match client_key_fields.verify(
        "",
        client_nonce.as_deref().unwrap_or(""),
        sdp,
        crate::access::client_key::now_unix_ms(),
    ) {
        Ok(verified) => verified,
        Err(e) => {
            return json_error(
                "400 Bad Request",
                format!("client key verification failed: {e}"),
            )
        }
    };
    // Org-grant ride-along (phase 6 step 4), same as the rendezvous path:
    // materialize before grant resolution so a member's first offer binds
    // its scoped principal instead of falling back to trusted-transport
    // root. Failure changes nothing here — the transport already earned
    // its authority — but the error is surfaced for the console.
    let org_grant_error = body
        .get("org_grant")
        .filter(|doc| !doc.is_null())
        .and_then(|doc| {
            crate::access::org::present_org_grant_value(
                &crate::access::backend::select_backend().cert_dir(),
                doc,
                &org_target_agent_card_ids(agent_card),
                crate::access::client_key::now_unix_ms() as u64,
            )
            .err()
        });
    let grant = match verified_client_key {
        Some(key) => {
            let cert_dir = crate::access::backend::select_backend().cert_dir();
            let loaded = load_local_iam_state_for_request(&cert_dir).ok().flatten();
            let bound = loaded.as_ref().and_then(|state| {
                crate::access::iam::principal_for_client_key(
                    state,
                    &key.fingerprint,
                    "local-dashboard-control",
                )
                .or_else(|| {
                    crate::access::iam::principal_for_client_key_any_status(
                        state,
                        &key.fingerprint,
                        "local-dashboard-control",
                    )
                })
                .map(|principal| (principal, state.clone()))
            });
            match bound {
                Some((principal, iam_state)) => {
                    crate::dashboard_control::DashboardControlGrant::UserClient {
                        principal,
                        iam_state,
                    }
                }
                None => crate::dashboard_control::DashboardControlGrant::UserClientRoot {
                    principal:
                        crate::access::iam::AccessPrincipal::root_dashboard_session_with_client_key(
                            "dashboard-control",
                            "local-dashboard-control",
                            &key.fingerprint,
                            &key.public_key_b64u,
                        ),
                },
            }
        }
        None => crate::dashboard_control::DashboardControlGrant::TrustedLocal,
    };
    match dashboard_control
        .answer_offer_with_grant(sdp.to_string(), None, client_nonce, grant)
        .await
    {
        Ok(answer) => {
            // Tab presence: annotate the session with the offer's
            // client-declared tab id, when sent.
            if let Some(tab) = body.get("tab_id").and_then(|v| v.as_str()) {
                dashboard_control.note_tab_id(&answer.session_id, tab);
            }
            let mut response = serde_json::json!({
                "ok": true,
                "signaling": "connect-bootstrap-local",
                "session_id": answer.session_id,
                "sdp": answer.sdp,
                "binding": answer.binding,
            });
            if let Some(org_error) = org_grant_error {
                response["org_grant_error"] = serde_json::Value::String(org_error);
            }
            json_ok(response)
        }
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

pub(crate) async fn connect_dashboard_ice_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    let candidate = body
        .get("candidate")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match dashboard_control
        .add_ice_candidate(session_id, &candidate)
        .await
    {
        Ok(true) => json_ok(serde_json::json!({ "ok": true })),
        Ok(false) => json_error("404 Not Found", "dashboard control session not found"),
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

pub(crate) async fn connect_dashboard_close_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    dashboard_control.close(session_id).await;
    json_ok(serde_json::json!({ "ok": true }))
}

pub(crate) fn connect_bootstrap_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant Connect Bootstrap</title>
  <style>
    :root { color-scheme: dark; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #11111b; color: #cdd6f4; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; }
    main { width: min(760px, calc(100vw - 32px)); }
    h1 { font-size: 22px; margin: 0 0 16px; }
    pre { white-space: pre-wrap; overflow-wrap: anywhere; padding: 16px; background: #181825; border: 1px solid #45475a; border-radius: 8px; }
    .ok { color: #a6e3a1; }
    .err { color: #f38ba8; }
  </style>
</head>
<body>
  <main>
    <h1>Intendant Connect Bootstrap</h1>
    <pre id="status">starting</pre>
  </main>
  <script>
(() => {
	  const statusEl = document.getElementById('status');
	  const MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
	  const MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
	  const UPLOAD_CHUNK_BYTES = 16 * 1024;
	  const UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
	  function paint(message, kind = '') {
	    statusEl.textContent = typeof message === 'string' ? message : JSON.stringify(message, null, 2);
	    statusEl.className = kind;
	  }

  function abortError(message = 'dashboard control request aborted') {
    try { return new DOMException(message, 'AbortError'); } catch {
      const err = new Error(message);
      err.name = 'AbortError';
      return err;
    }
  }

  function bytesToBase64Url(bytes) {
    let binary = '';
    for (const b of bytes) binary += String.fromCharCode(b);
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
  }

  function base64UrlToBytes(value) {
    const padded = String(value || '').replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(String(value || '').length / 4) * 4, '=');
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

	  function base64ToBytes(value) {
	    const binary = atob(String(value || ''));
	    const bytes = new Uint8Array(binary.length);
	    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	    return bytes;
	  }

	  function bytesToBase64(bytes) {
	    let binary = '';
	    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
	    return btoa(binary);
	  }

  async function sha256B64u(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
    return bytesToBase64Url(new Uint8Array(digest));
  }

  function randomB64u(byteLength = 32) {
    const bytes = new Uint8Array(byteLength);
    crypto.getRandomValues(bytes);
    return bytesToBase64Url(bytes);
  }

  function bindingPayload(binding) {
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

  async function verifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
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
    return crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  }

  async function verifyBinding(binding, sessionId, offerSdp, answerSdp, clientNonce = '') {
    if (!binding || typeof binding !== 'object') return { ok: false, error: 'missing binding' };
    if (binding.protocol !== 'intendant-dashboard-control-v1') return { ok: false, error: 'unexpected protocol' };
    if (String(binding.session_id || '') !== String(sessionId || '')) return { ok: false, error: 'session mismatch' };
    if (!crypto?.subtle) return { ok: false, error: 'WebCrypto unavailable' };
    const createdUnixMs = Number(binding.created_unix_ms || 0);
    const expiresUnixMs = Number(binding.expires_unix_ms || 0);
    if (!Number.isFinite(createdUnixMs) || createdUnixMs <= 0) return { ok: false, error: 'missing binding creation time' };
    if (!Number.isFinite(expiresUnixMs) || expiresUnixMs <= 0) return { ok: false, error: 'missing binding expiry' };
    const nowUnixMs = Date.now();
    if (expiresUnixMs + 30000 < nowUnixMs) return { ok: false, error: 'binding expired' };
    if (createdUnixMs - 30000 > nowUnixMs) return { ok: false, error: 'binding timestamp from future' };
    if (binding.offer_sha256 !== await sha256B64u(offerSdp || '')) return { ok: false, error: 'offer hash mismatch' };
    if (binding.answer_sha256 !== await sha256B64u(answerSdp || '')) return { ok: false, error: 'answer hash mismatch' };
    const nonce = String(clientNonce || '');
    if (nonce) {
      if (String(binding.client_nonce || '') !== nonce) return { ok: false, error: 'client nonce mismatch' };
    } else if (binding.client_nonce) {
      return { ok: false, error: 'unexpected client nonce binding' };
    }
    const verified = await verifyEd25519(
      base64UrlToBytes(binding.daemon_public_key || ''),
      base64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(bindingPayload(binding))
    );
    if (!verified) return { ok: false, error: 'signature invalid' };
    return {
      ok: true,
      daemonPublicKey: binding.daemon_public_key,
      createdUnixMs,
      expiresUnixMs,
      clientNonce: binding.client_nonce || '',
    };
  }

  const connect = {
    pc: null,
    channel: null,
    sessionId: '',
    binding: null,
    verifiedBinding: null,
    clientNonce: '',
    expiresUnixMs: 0,
    localOfferSdp: '',
	    pendingIce: [],
	    pending: new Map(),
	    chunkedResponses: new Map(),
	    byteStreams: new Map(),
	    completedChunkedResponses: 0,
	    completedByteStreams: 0,
	    seq: 0,
	    started: false,

    async start() {
      if (this.started) return this.status();
      this.started = true;
	      this.pc = new RTCPeerConnection({});
	      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
	      this.channel.onopen = () => {
	        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'upload_frames'] });
	        paint(this.status(), 'ok');
	      };
      this.channel.onmessage = ev => this.handleMessage(ev.data);
      this.channel.onclose = () => paint(this.status());
      this.pc.onconnectionstatechange = () => paint(this.status(), this.pc.connectionState === 'failed' ? 'err' : '');
      this.pc.onicecandidate = ev => {
        if (!ev.candidate) return;
        const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
        if (!this.sessionId) {
          this.pendingIce.push(candidate);
          return;
        }
        this.sendIce(candidate).catch(err => console.warn('connect ice failed', err));
      };
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      this.localOfferSdp = offer.sdp || '';
      this.clientNonce = randomB64u(32);
      const answer = await fetch('/connect/dashboard/offer', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sdp: this.localOfferSdp, client_nonce: this.clientNonce }),
      }).then(async resp => {
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok) throw new Error(body.error || `HTTP ${resp.status}`);
        return body;
      });
      this.sessionId = String(answer.session_id || '');
      this.binding = answer.binding || null;
      const verified = await verifyBinding(this.binding, this.sessionId, this.localOfferSdp, answer.sdp || '', this.clientNonce);
      if (!verified.ok) throw new Error(`binding rejected: ${verified.error || 'unknown'}`);
      this.verifiedBinding = verified;
      this.expiresUnixMs = verified.expiresUnixMs || 0;
      await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
      for (const candidate of this.pendingIce.splice(0)) await this.sendIce(candidate);
      paint(this.status(), 'ok');
      return this.status();
    },

    async sendIce(candidate) {
      await fetch('/connect/dashboard/ice', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: this.sessionId, candidate }),
      });
    },

    handleMessage(data) {
      let msg;
      try { msg = JSON.parse(String(data)); } catch { return; }
      this.handleFrame(msg);
    },

    handleFrame(msg) {
      if (msg.t === 'hello_ack') {
        paint(this.status(), 'ok');
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
	      if (msg.t !== 'pong' && msg.t !== 'response') return;
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) pending.reject(abortError(msg.error || 'dashboard control request cancelled'));
      else if (msg.t === 'response' && msg.ok === false) pending.reject(new Error(msg.error || 'dashboard control request failed'));
      else pending.resolve(msg.t === 'pong' ? msg : msg.result);
    },

    handleResponseStart(msg) {
      const id = String(msg.id || '');
      if (!id || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64-json-frame' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_CHUNKED_RESPONSE_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response header');
        return;
      }
      this.chunkedResponses.set(id, {
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
      });
      paint(this.status(), 'ok');
    },

    handleResponseChunk(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteChunkedResponse(id);
      if (!completed && this.chunkedResponses.has(id)) {
        this.sendChunkCredit(id, 1);
      }
      paint(this.status(), 'ok');
    },

    handleResponseEnd(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response footer');
        return;
      }
      state.ended = true;
      this.maybeCompleteChunkedResponse(id);
      paint(this.status(), 'ok');
    },

    maybeCompleteChunkedResponse(id) {
      const state = this.chunkedResponses.get(id);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectChunkedResponse(id, 'connect dashboard chunked response missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response size mismatch');
        return false;
      }
      this.chunkedResponses.delete(id);
      let frame;
      try {
        frame = JSON.parse(new TextDecoder().decode(merged));
      } catch {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response was not valid JSON');
        return false;
      }
      if (frame.t !== 'response' || String(frame.id || '') !== id) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response id mismatch');
        return false;
      }
      this.completedChunkedResponses += 1;
      this.handleFrame(frame);
      return true;
    },

    rejectChunkedResponse(id, message) {
      this.chunkedResponses.delete(id);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
	      paint(this.status(), 'err');
	    },

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
	        totalBytes > MAX_BYTE_STREAM_BYTES ||
	        !Number.isSafeInteger(expectedChunks) ||
	        expectedChunks < 0
	      ) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream header', id);
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
	        result: null,
	        contentType: String(msg.content_type || 'application/octet-stream'),
	        filename: msg.filename ? String(msg.filename) : '',
	      });
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamChunk(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      const seq = Number(msg.seq);
	      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream chunk sequence');
	        return;
	      }
	      if (state.chunks.has(seq)) return;
	      let bytes;
	      try {
	        bytes = base64ToBytes(msg.data);
	      } catch {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream encoding');
	        return;
	      }
	      state.chunks.set(seq, bytes);
	      state.receivedBytes += bytes.byteLength;
	      if (state.receivedBytes > state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream exceeded declared size');
	        return;
	      }
	      const completed = this.maybeCompleteByteStream(streamId);
	      if (!completed && this.byteStreams.has(streamId)) {
	        this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
	      }
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamEnd(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      if (msg.ok === false) {
	        this.rejectByteStream(streamId, msg.error || 'connect dashboard byte stream failed');
	        return;
	      }
	      const finalChunks = Number(msg.chunks);
	      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream footer');
	        return;
	      }
	      state.ended = true;
	      state.result = msg.result || null;
	      this.maybeCompleteByteStream(streamId);
	      paint(this.status(), 'ok');
	    },

	    maybeCompleteByteStream(streamId) {
	      const state = this.byteStreams.get(streamId);
	      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
	      const merged = new Uint8Array(state.totalBytes);
	      let offset = 0;
	      for (let seq = 0; seq < state.expectedChunks; seq++) {
	        const chunk = state.chunks.get(seq);
	        if (!chunk) {
	          this.rejectByteStream(streamId, 'connect dashboard byte stream missed a chunk');
	          return false;
	        }
	        merged.set(chunk, offset);
	        offset += chunk.byteLength;
	      }
	      if (offset !== state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream size mismatch');
	        return false;
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
	      this.chunkedResponses.delete(state.id);
	      pending.resolve(result);
	      paint(this.status(), 'ok');
	      return true;
	    },

	    rejectByteStream(streamId, message, requestId = '') {
	      const state = this.byteStreams.get(streamId);
	      const id = state?.id || requestId || streamId;
	      this.byteStreams.delete(streamId);
	      const pending = this.pending.get(id);
	      if (pending) {
	        this.pending.delete(id);
	        pending.reject(new Error(message));
	      }
	      paint(this.status(), 'err');
	    },

	    request(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard RPC is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params });
	      return promise;
	    },

	    requestBytes(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard byte stream is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params, bytes: true });
	      return promise;
	    },

	    async uploadBytes(method, params = {}, bytes, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard upload is not connected'));
	      const id = this.nextId();
	      const totalBytes = Number(bytes?.size ?? bytes?.byteLength ?? bytes?.length ?? 0);
	      const chunkSize = options.chunkBytes || UPLOAD_CHUNK_BYTES;
	      const chunks = Math.ceil(totalBytes / chunkSize);
	      const promise = this.waitFor(id, options);
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
	          if (options.signal?.aborted) throw abortError();
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
	            data: bytesToBase64(chunk),
	          });
	          await this.waitForBufferedAmountLow(options.signal);
	        }
	        if (this.pending.has(id)) this.sendFrame({ t: 'upload_end', id, chunks });
	      } catch (err) {
	        if (this.pending.has(id)) this.sendFrame({ t: 'cancel', id });
	        throw err;
	      }
	      return promise;
	    },

	    async waitForBufferedAmountLow(signal = null) {
	      while (
	        this.channel &&
	        this.channel.readyState === 'open' &&
	        this.channel.bufferedAmount > UPLOAD_BUFFER_HIGH_BYTES
	      ) {
	        if (signal?.aborted) throw abortError();
	        await new Promise(resolve => setTimeout(resolve, 10));
	      }
	    },

    waitFor(id, options = {}) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const signal = options.signal || null;
        const fail = (err, cancel = false) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	          this.pending.delete(id);
	          this.chunkedResponses.delete(id);
	          this.deleteByteStreamsForRequest(id);
	          if (cancel) this.sendFrame({ t: 'cancel', id });
	          reject(err);
	        };
        const abortHandler = signal ? () => fail(abortError(), true) : null;
        const timeoutMs = Number.isFinite(Number(options.timeoutMs)) ? Number(options.timeoutMs) : 10000;
        const timer = setTimeout(() => fail(new Error('connect dashboard request timed out'), true), timeoutMs);
        if (signal && abortHandler) signal.addEventListener('abort', abortHandler, { once: true });
        this.pending.set(id, {
	          resolve: value => {
	            if (settled) return;
	            settled = true;
	            clearTimeout(timer);
	            if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	            this.chunkedResponses.delete(id);
	            this.deleteByteStreamsForRequest(id);
	            resolve(value);
	          },
          reject: err => fail(err),
        });
      });
    },

    canUseRpc() {
      return Boolean(this.verifiedBinding && this.pc?.connectionState === 'connected' && this.channel?.readyState === 'open');
    },

    sendFrame(frame) {
      if (this.channel?.readyState === 'open') this.channel.send(JSON.stringify(frame));
    },

	    sendChunkCredit(id, chunks, chunkId = null) {
	      const frame = { t: 'credit', id, chunks };
	      if (chunkId) frame.chunk_id = chunkId;
	      this.sendFrame(frame);
	    },

	    deleteByteStreamsForRequest(id) {
	      for (const [streamId, state] of this.byteStreams) {
	        if (streamId === id || state?.id === id) this.byteStreams.delete(streamId);
	      }
	    },

    status() {
      return {
        connected: this.pc?.connectionState === 'connected',
        pcState: this.pc?.connectionState || '',
	        channelState: this.channel?.readyState || '',
	        sessionId: this.sessionId,
	        verifiedBinding: this.verifiedBinding,
	        clientNonce: this.clientNonce,
	        expiresUnixMs: this.expiresUnixMs,
	        pendingRequests: this.pending.size,
	        pendingChunkedResponses: this.chunkedResponses.size,
	        pendingByteStreams: this.byteStreams.size,
	        completedChunkedResponses: this.completedChunkedResponses,
	        completedByteStreams: this.completedByteStreams,
	      };
	    },

    close() {
      if (this.sessionId) {
        fetch('/connect/dashboard/close', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ session_id: this.sessionId }),
        }).catch(() => {});
	      }
	      this.chunkedResponses.clear();
	      this.byteStreams.clear();
	      try { this.channel?.close(); } catch {}
      try { this.pc?.close(); } catch {}
    },

    nextId() {
      this.seq += 1;
      return `connect-${Date.now()}-${this.seq}`;
    },
  };

  window.intendantConnectDashboard = connect;
  connect.start().catch(err => {
    console.error(err);
    paint(err?.message || String(err), 'err');
  });
})();
  </script>
</body>
</html>"#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_bootstrap_html_exposes_debug_api() {
        let html = connect_bootstrap_html();
        assert!(html.contains("Intendant Connect Bootstrap"));
        assert!(html.contains("window.intendantConnectDashboard"));
        assert!(html.contains("/connect/dashboard/offer"));
        assert!(html.contains("intendant-dashboard-control-v1"));
    }
}
