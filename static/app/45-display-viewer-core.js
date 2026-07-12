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

// ── Input forwarder (interactive mode) ──────────────────────────────────
// The pointer/keyboard/wheel capture stack both interactive modes install.
// The wire format is the raw `InputEvent` JSON both server sides parse
// with one handler ({t:'kd'|'ku'|'md'|'mu'|'mm'|'sc', ...}); the policy
// differences stay in the callers: WHERE events go (local: /ws input lane
// with datachannel fallback; peer: datachannels only), WHICH surface they
// bind to (local: the slot's <video>; peer: live tile canvas or video),
// and the listener options (local passes { passive: false }; the peer
// path historically adds listeners without options — preserved as-is).

// Letterbox-aware pointer normalization: map a client-coordinate event to
// logical (0..1) display coords accounting for the rendered surface's
// preserved aspect ratio (pillarbox/letterbox bars). Canvas-aware superset
// of the two per-class copies: for a <video> it reads videoWidth/Height
// exactly like the local slot's `normalize` did; for the peer tile canvas
// it reads width/height.
function displayViewerNormalizePointerEvent(surface, e) {
  const rect = surface.getBoundingClientRect();
  const isCanvas = typeof HTMLCanvasElement !== 'undefined' && surface instanceof HTMLCanvasElement;
  const surfaceW = isCanvas ? (surface.width || rect.width) : (surface.videoWidth || rect.width);
  const surfaceH = isCanvas ? (surface.height || rect.height) : (surface.videoHeight || rect.height);
  const videoAspect = surfaceW / surfaceH;
  const elAspect = rect.width / rect.height;
  let contentW, contentH, offsetX, offsetY;
  if (elAspect > videoAspect) {
    // Element is wider than video -> pillarbox (black bars left/right)
    contentH = rect.height;
    contentW = contentH * videoAspect;
    offsetX = (rect.width - contentW) / 2;
    offsetY = 0;
  } else {
    // Element is taller than video -> letterbox (black bars top/bottom)
    contentW = rect.width;
    contentH = contentW / videoAspect;
    offsetX = 0;
    offsetY = (rect.height - contentH) / 2;
  }
  const relX = (e.clientX - rect.left - offsetX) / contentW;
  const relY = (e.clientY - rect.top - offsetY) / contentH;
  return {
    x: Math.max(0, Math.min(relX, 0.9999)),
    y: Math.max(0, Math.min(relY, 0.9999)),
  };
}

// Item 3: synthetic-keyup flusher for every currently-held key (not just
// the 8 modifiers — a latched non-modifier auto-repeats remotely forever).
// Returned closure is stored on `owner._flushHeldKeys` so exit paths that
// run outside the enter closure (server demotion, release, pane rebuild)
// can release held keys BEFORE the listeners are removed / authority is
// gone. Reads `owner._heldKeys` at flush time, like both originals did.
function displayViewerMakeHeldKeyFlusher(owner, sendControl) {
  return () => {
    if (!owner._heldKeys) return;
    for (const code of owner._heldKeys) {
      sendControl({ t: 'ku', code, key: '', shift: false, ctrl: false, alt: false, meta: false });
    }
    owner._heldKeys.clear();
  };
}

// Build the interactive-mode handler set. `owner` is the DisplaySlot /
// PeerDisplayConnection (annotation/callout suppression and the
// `interactive` re-focus check read it live); `target` is the bound
// surface; `sendControl` / `sendPointer` are the policy-owned transports
// (reliable-ordered vs lossy-unordered lanes).
//
// Suppression semantics (shared with 47-annotation-clips): a live-
// annotation edit on this owner suppresses ALL forwarding; an armed
// callout suppresses only the drag's md/mm/mu — keyboard and wheel keep
// flowing (the arm overlay swallows most pointer events already; these
// checks catch the letterbox bars).
//
// NOTE: Both `code` (physical key position) and `key` (logical character)
// are sent in KeyDown/KeyUp events. Backends currently use `code` only
// for physical key injection (xdotool key / CGEvent keycode). Using `key`
// for character-based text input is a follow-up.
function displayViewerBuildInputHandlers({ owner, target, sendControl, sendPointer }) {
  const handlers = {};
  handlers.keydown = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    owner._heldKeys.add(e.code);
    sendControl({ t: 'kd', code: e.code, key: e.key, shift: e.shiftKey, ctrl: e.ctrlKey, alt: e.altKey, meta: e.metaKey });
  };
  handlers.keyup = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    owner._heldKeys.delete(e.code);
    sendControl({ t: 'ku', code: e.code, key: e.key, shift: e.shiftKey, ctrl: e.ctrlKey, alt: e.altKey, meta: e.metaKey });
  };
  handlers.pointerdown = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner) || liveCalloutArmedFor(owner)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    target.focus();
    target.setPointerCapture(e.pointerId);
    const { x, y } = displayViewerNormalizePointerEvent(target, e);
    sendControl({ t: 'md', x, y, b: e.button });
  };
  handlers.pointerup = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner) || liveCalloutArmedFor(owner)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    target.releasePointerCapture(e.pointerId);
    const { x, y } = displayViewerNormalizePointerEvent(target, e);
    sendControl({ t: 'mu', x, y, b: e.button });
  };
  handlers.pointermove = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner) || liveCalloutArmedFor(owner)) {
      e.preventDefault();
      return;
    }
    const { x, y } = displayViewerNormalizePointerEvent(target, e);
    sendPointer({ t: 'mm', x, y, buttons: e.buttons });
  };
  handlers.wheel = (e) => {
    if (shouldSuppressDisplayInputForAnnotation(owner)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    const { x, y } = displayViewerNormalizePointerEvent(target, e);
    // Normalize pixel deltas to discrete scroll notches.
    // DOM_DELTA_PIXEL (0): divide by 100 to approximate notches.
    // DOM_DELTA_LINE (1): use as-is (already logical lines).
    // DOM_DELTA_PAGE (2): multiply by 3 (approximate lines per page).
    let dx = e.deltaX, dy = e.deltaY;
    if (e.deltaMode === 0) {
      dx = Math.round(dx / 100) || (dx > 0 ? 1 : dx < 0 ? -1 : 0);
      dy = Math.round(dy / 100) || (dy > 0 ? 1 : dy < 0 ? -1 : 0);
    } else if (e.deltaMode === 2) {
      dx *= 3; dy *= 3;
    }
    sendPointer({ t: 'sc', x, y, dx, dy });
  };
  handlers.contextmenu = (e) => e.preventDefault();
  // Release ALL held keys when the surface loses focus (e.g. Alt+Tab
  // away). Without this, the remote side thinks they are still held
  // because no keyup event ever fires for them.
  handlers.blur = () => {
    owner._flushHeldKeys?.();
  };
  // Re-focus the surface when the pointer enters it while interactive.
  // This restores keyboard input after Alt+Tab back to the dashboard.
  handlers.pointerenter = () => {
    if (owner.interactive) target.focus();
  };
  return handlers;
}

// ── Clipboard sync hooks (policy-gated: local DisplaySlot only today) ───
// Federated clipboard is a follow-up; PEER_DISPLAY_POLICY.clipboardSync
// is false and PeerDisplayConnection never calls these.

// Remote → browser: apply a `clipboard_update` payload to the local
// clipboard. Image payloads decode base64 into a ClipboardItem; text goes
// through writeText. Failures surface once per page session via
// noteDisplayClipboardWriteFailure (Item 8).
function displayViewerApplyRemoteClipboardUpdate(d) {
  const mime = d.mime || 'text/plain';
  if (mime.startsWith('image/') && d.data) {
    // Image clipboard: decode base64 and write as ClipboardItem.
    try {
      const binary = atob(d.data);
      const bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
      const blob = new Blob([bytes], { type: mime });
      const item = new ClipboardItem({ [mime]: blob });
      navigator.clipboard.write([item]).catch(noteDisplayClipboardWriteFailure);
    } catch { noteDisplayClipboardWriteFailure(); }
  } else if (d.text !== undefined) {
    navigator.clipboard.writeText(d.text).catch(noteDisplayClipboardWriteFailure);
  }
}

// Browser → remote: build the document-level paste interceptor that ships
// clipboard contents over the viewer's clipboard channel. `getChannel` is
// read at event time (and re-read inside the async FileReader callback,
// like the original field reads) so a renegotiated channel is honored.
function displayViewerBuildPasteHandler(getChannel) {
  return (e) => {
    if (getChannel()?.readyState !== 'open') return;
    // Check for image content first.
    if (e.clipboardData?.items) {
      for (const item of e.clipboardData.items) {
        if (item.type.startsWith('image/')) {
          const blob = item.getAsFile();
          if (!blob) continue;
          // 5 MB size limit.
          if (blob.size > 5 * 1024 * 1024) {
            console.warn('[clipboard] skipping image paste: exceeds 5 MB limit');
            e.preventDefault();
            return;
          }
          const mime = item.type;
          const reader = new FileReader();
          reader.onload = () => {
            const base64 = reader.result.split(',')[1];
            const channel = getChannel();
            if (base64 && channel?.readyState === 'open') {
              channel.send(JSON.stringify({
                t: 'clipboard_set', mime, data: base64
              }));
            }
          };
          reader.readAsDataURL(blob);
          e.preventDefault();
          return;
        }
      }
    }
    // Fall back to text.
    const text = e.clipboardData?.getData('text');
    if (text !== undefined) {
      getChannel().send(JSON.stringify({t: 'clipboard_set', mime: 'text/plain', text}));
      e.preventDefault();
    }
  };
}
