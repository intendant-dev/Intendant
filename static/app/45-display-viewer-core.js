// ── Display viewer core (shared by DisplaySlot + PeerDisplayConnection) ──
// One home for the logic the local display path (45-displays-webrtc.js,
// class DisplaySlot) and the federated peer-display path
// (52-peer-display.js, class PeerDisplayConnection — whose
// PeerFileTransferConnection rides the same signaling scaffold) used to
// carry as near-verbatim copies. Pure consolidation: every helper here is
// behavior-identical to the per-class code it replaced. The DELIBERATE
// local-vs-federated differences stay OUT of this file and live in the
// named policy objects beside each class (DISPLAY_SLOT_POLICY in
// 45-displays-webrtc.js, PEER_DISPLAY_POLICY in 52-peer-display.js):
// codec preference order (#58 local / #67 federated), simulcast injection
// (#58 local-only; #46 federated skip), TURN relay pinning (#41–#45
// federated-only), retry semantics (renegotiate-in-place vs
// reopen-fresh-session), signaling lanes, container resolution, clipboard
// sync, and attach/annotation stream naming.
//
// Contract: `viewer` arguments are DisplaySlot / PeerDisplayConnection /
// PeerFileTransferConnection instances; helpers touch only fields the
// callers already share (`pc`, `_answerApplied`, `_pendingCandidates`,
// `_heldKeys`, `_flushHeldKeys`, `_noTrackTimer`, `_statsTimer`,
// `_statsPrev`, `_attachCounter`, `interactive`, `_sampleStats()`).
// Per-class UX (status vocabulary, toasts, chip DOM ids, log copy) stays
// in the classes and reaches the core through the hook parameters.

// Item 8: remote-clipboard write failures were fully silent (nested empty
// catches). Success stays quiet; the FIRST write failure per page session
// raises one actionable toast, then the path goes quiet again.
let displayClipboardToastShown = false;
function noteDisplayClipboardWriteFailure() {
  if (displayClipboardToastShown) return;
  displayClipboardToastShown = true;
  if (typeof showControlToast === 'function') {
    showControlToast('error', "Remote clipboard couldn't sync — click the page and retry");
  }
}

// Shared getStats() summarizer for the live metrics chip ("LIVE · fps ·
// kbps · relay"). Used by both the local DisplaySlot and the federated
// PeerDisplayConnection samplers so the two chips can't drift. `prev` is
// the snapshot returned by the previous call (or null on the first
// sample); rates are computed from deltas against it. kbps reads the
// selected candidate-pair's bytesReceived so it covers the tile
// datachannel lane as well as RTP video; `relay` is true when the local
// selected candidate is a TURN relay.
function summarizeRtcStats(stats, prev) {
  let inbound = null;
  let pair = null;
  let transport = null;
  const byId = new Map();
  stats.forEach((r) => {
    byId.set(r.id, r);
    if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
      inbound = r;
    } else if (
      r.type === 'candidate-pair' &&
      (r.selected || (r.nominated && r.state === 'succeeded'))
    ) {
      pair = r;
    } else if (r.type === 'transport') {
      transport = r;
    }
  });
  // Chrome never sets `selected`; resolve through the transport's pair id.
  if (!pair && transport && transport.selectedCandidatePairId) {
    pair = byId.get(transport.selectedCandidatePairId) || null;
  }
  const now = (inbound && inbound.timestamp) || (pair && pair.timestamp) || performance.now();
  const snapshot = {
    t: now,
    frames: inbound ? Number(inbound.framesDecoded || 0) : null,
    bytes: pair ? Number(pair.bytesReceived || 0)
      : (inbound ? Number(inbound.bytesReceived || 0) : null),
  };
  let fps = (inbound && inbound.framesPerSecond !== undefined)
    ? Math.round(inbound.framesPerSecond)
    : null;
  let kbps = null;
  if (prev && now > prev.t) {
    const dt = (now - prev.t) / 1000;
    if (fps === null && snapshot.frames !== null && prev.frames !== null) {
      fps = Math.max(0, Math.round((snapshot.frames - prev.frames) / dt));
    }
    if (snapshot.bytes !== null && prev.bytes !== null) {
      kbps = Math.max(0, Math.round(((snapshot.bytes - prev.bytes) * 8) / dt / 1000));
    }
  }
  let relay = false;
  if (pair) {
    const local = byId.get(pair.localCandidateId);
    relay = !!(local && local.candidateType === 'relay');
  }
  const parts = ['LIVE'];
  if (fps !== null) parts.push(`${fps} fps`);
  if (kbps !== null) parts.push(`${kbps} kbps`);
  if (relay) parts.push('relay');
  // Nothing but "LIVE" to say yet (first sample, no rates): don't render.
  const text = parts.length > 1 ? parts.join(' · ') : null;
  return { text, snapshot };
}

// ── Signaling scaffold (offer/answer + pending-ICE buffering) ───────────
// The invariant all three WebRTC lanes (local display, peer display, peer
// file transfer) share: remote ICE candidates that arrive before the
// answer is applied are queued on `viewer._pendingCandidates`, and
// `setRemoteDescription(answer)` flips `viewer._answerApplied` then
// flushes the queue exactly once. Status text, log copy, and post-apply
// staging are per-class UX and arrive through the hooks:
//   beforeFlush(count)          — after the answer applies, before the queue
//                                 flushes (local: status line with count;
//                                 peer: debug log)
//   onFlushCandidateError(err)  — per queued candidate addIceCandidate
//                                 rejection (default: swallow, the local
//                                 path's historical behavior)
//   afterFlush()                — queue drained (overlay staging, watchdog)
//   onError(err)                — setRemoteDescription rejected (or a hook
//                                 above threw inside the .then chain —
//                                 same coverage as the original per-class
//                                 promise chains)
function displayViewerApplyRemoteAnswer(viewer, sdp, hooks = {}) {
  return viewer.pc.setRemoteDescription({ type: 'answer', sdp }).then(() => {
    viewer._answerApplied = true;
    if (hooks.beforeFlush) hooks.beforeFlush(viewer._pendingCandidates.length);
    for (const c of viewer._pendingCandidates) {
      viewer.pc.addIceCandidate(c).catch(hooks.onFlushCandidateError || (() => {}));
    }
    viewer._pendingCandidates = [];
    if (hooks.afterFlush) hooks.afterFlush();
  }).catch((err) => {
    if (hooks.onError) hooks.onError(err);
  });
}

// Queue-or-add for a remote ICE candidate (the receive half of the
// scaffold). Hooks keep the per-class log copy byte-identical:
//   onQueued(candidate) — about to buffer (answer not applied yet)
//   onAdd(candidate)    — about to addIceCandidate on the live pc
//   onAddError(err)     — addIceCandidate rejected
function displayViewerIngestRemoteIceCandidate(viewer, candidate, hooks = {}) {
  if (!viewer._answerApplied) {
    if (hooks.onQueued) hooks.onQueued(candidate);
    viewer._pendingCandidates.push(candidate);
    return;
  }
  if (hooks.onAdd) hooks.onAdd(candidate);
  viewer.pc.addIceCandidate(candidate).catch(hooks.onAddError || (() => {}));
}

// One-line summary of an RTCIceCandidate / candidate-JSON for logs.
// `candidate` is the SDP line and already carries address + port +
// protocol + type — extract and format so we don't dump the full JSON
// every tick. (Was duplicated verbatim on PeerDisplayConnection and
// PeerFileTransferConnection as `_describeCandidate`.)
function describePeerIceCandidateForLog(cand) {
  const s = cand && (cand.candidate || JSON.stringify(cand));
  if (!s) return '(empty)';
  // SDP candidate lines look like:
  //   candidate:1 1 udp 2113937151 192.168.1.10 5000 typ host ...
  const m = s.match(/candidate:\S+\s+\d+\s+(\S+)\s+\S+\s+(\S+)\s+(\d+)\s+typ\s+(\S+)/);
  if (m) return `${m[4]} ${m[1]} ${m[2]}:${m[3]}`;
  return s;
}

// Scoped console logger shared by the peer-side `_log` methods: the
// prefix (`[webrtc-peer <host>]`, `[peer-file-transfer <host>/<sid>]`)
// keeps Safari Web Inspector filters one-shot per connection and matches
// the server-side source tags for cross-side investigations.
function displayViewerScopedConsoleLog(prefix, level, message) {
  const fn = level === 'error' ? console.error
           : level === 'warn'  ? console.warn
           : level === 'info'  ? console.info
           :                     console.debug;
  fn(`${prefix} ${message}`);
}

// Run `cb` once the <video> renders its first frame. rVFC where available
// (fires per decoded frame), 'loadeddata' otherwise; no element at all
// fires immediately (the peer path's tile-only panes). `isStale` is the
// connect-epoch guard: the local slot passes an epoch comparison so
// callbacks from a previous RTCPeerConnection bail; the peer path passes
// a `pc` liveness check (each retry is a whole fresh connection object,
// so object identity is its epoch).
function displayViewerOnFirstFrame(videoEl, isStale, cb) {
  const fire = () => {
    if (isStale && isStale()) return; // stale negotiation
    cb();
  };
  if (videoEl && typeof videoEl.requestVideoFrameCallback === 'function') {
    videoEl.requestVideoFrameCallback(() => fire());
  } else if (videoEl) {
    if (videoEl.readyState >= 2) fire();
    else videoEl.addEventListener('loadeddata', fire, { once: true });
  } else {
    fire();
  }
}
