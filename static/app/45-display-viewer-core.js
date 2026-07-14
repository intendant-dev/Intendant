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
// `_statsPrev`, `_attachCounter`, `interactive`, `_sampleStats()`, plus
// the liveness-guard state this file owns: `_pauseGuard`,
// `_resumeVideoPending`, `_freezeWatch`, `_freezeWatchGen`).
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

// ── Input-authority chip + buttons (shared vocabulary) ──────────────────
// Both viewers render the same three-state chip from the same server
// vocabulary; only the chip's base class and which DOM the buttons live
// in differ (fixed toolbar elements locally; per-container queries on the
// peer path). The state machines around these renderers stay in the
// classes — they differ deliberately (the local slot only auto-enters
// interactive mode on a pending user take; the peer re-enters on ANY
// 'you' so pane rebuilds rebind listeners).

// The server-resolved authority states. Forward-compat convention shared
// by both `setAuthority` paths: an unknown state string leaves the chip
// on its previous value rather than blanking it.
function isDisplayInputAuthorityState(state) {
  return state === 'you' || state === 'other' || state === 'unclaimed';
}

// Render the chip element for `state`. `baseClass` is the class list the
// chip keeps in every state ('display-input-authority' locally;
// 'peer-display-authority display-input-authority' on peer panes — the
// .you/.other/.unclaimed styling is shared CSS). The default arm is
// 'unknown' — the server hasn't told us yet. Hide the chip rather than
// show "shared" speculatively, per phase 5c spec: "do not show
// 'unclaimed' unless the server has actually told this browser the
// display is unclaimed."
function displayViewerRenderAuthorityChip(chipEl, state, baseClass) {
  if (!chipEl) return;
  switch (state) {
    case 'you':
      chipEl.style.display = '';
      chipEl.textContent = 'Input: you';
      chipEl.className = `${baseClass} you`;
      break;
    case 'other':
      chipEl.style.display = '';
      chipEl.textContent = 'Input: another viewer';
      chipEl.className = `${baseClass} other`;
      break;
    case 'unclaimed':
      chipEl.style.display = '';
      chipEl.textContent = 'Input: shared';
      chipEl.className = `${baseClass} unclaimed`;
      break;
    default:
      chipEl.style.display = 'none';
      chipEl.textContent = '';
      chipEl.className = baseClass;
      break;
  }
}

// Take/Release visibility + the callout arm gate, from the authority
// state. Callout is armable only while this browser holds input
// authority (the drag would otherwise be view-only theater; the
// arm/suppress semantics assume our pointer stream is what the remote
// receives).
function displayViewerApplyAuthorityButtons(takeBtn, releaseBtn, calloutBtn, state) {
  if (takeBtn && releaseBtn) {
    if (state === 'you') {
      takeBtn.style.display = 'none';
      releaseBtn.style.display = '';
    } else {
      takeBtn.style.display = '';
      releaseBtn.style.display = 'none';
    }
  }
  if (calloutBtn) {
    calloutBtn.disabled = state !== 'you';
  }
}

// Item 7b: how long a Take Control request may sit unanswered before the
// pending state resets itself with a toast instead of hanging armed
// forever. Same patience on both paths; the toast copy stays per-class.
const DISPLAY_VIEWER_TAKE_PENDING_TIMEOUT_MS = 5000;

// ── In-stage status overlay (shared DOM builder) ────────────────────────
// Render one stage overlay element. `overlay` is null to hide, or
// { mode: 'progress'|'error', text, retryLabel, onRetry } — 'progress'
// gets the spinner, 'error' the alarm styling, and a retry button
// appears only when retryLabel + a callable onRetry are both present.
// All dynamic text goes through textContent — never innerHTML. State
// ownership stays with the callers (the local slot renders into its one
// overlayEl; the peer keeps `_overlay` on the connection and re-applies
// into every container its host has, because pane DOM is rebuilt on
// every daemons-list re-render).
function displayViewerRenderStageOverlayInto(el, overlay) {
  el.textContent = '';
  if (!overlay) {
    el.style.display = 'none';
    el.classList.remove('error');
    return;
  }
  el.classList.toggle('error', overlay.mode === 'error');
  const inner = document.createElement('div');
  inner.className = 'stage-overlay-inner';
  if (overlay.mode !== 'error') {
    const spinner = document.createElement('span');
    spinner.className = 'stage-overlay-spinner';
    inner.appendChild(spinner);
  }
  const label = document.createElement('span');
  label.className = 'stage-overlay-text';
  label.textContent = overlay.text || '';
  inner.appendChild(label);
  if (overlay.retryLabel && typeof overlay.onRetry === 'function') {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'stage-overlay-retry';
    btn.textContent = overlay.retryLabel;
    btn.addEventListener('click', overlay.onRetry);
    inner.appendChild(btn);
  }
  el.appendChild(inner);
  el.style.display = '';
}

// ── Live metrics chip sampler (getStats driver) ─────────────────────────
// Both chips sample on the same cadence through the same summarizer so
// they can't drift; only where the text lands differs (the local slot's
// one metricsEl vs the peer's per-container fanout), which arrives as
// the `applyText` hook on the viewer's own `_sampleStats`.
const DISPLAY_VIEWER_STATS_SAMPLE_INTERVAL_MS = 3000;

function displayViewerStartStatsSampler(viewer) {
  if (viewer._statsTimer) return;
  viewer._statsPrev = null;
  viewer._statsTimer = window.setInterval(() => {
    viewer._sampleStats().catch(() => {});
  }, DISPLAY_VIEWER_STATS_SAMPLE_INTERVAL_MS);
  viewer._sampleStats().catch(() => {});
}

function displayViewerStopStatsSampler(viewer) {
  if (viewer._statsTimer) {
    window.clearInterval(viewer._statsTimer);
    viewer._statsTimer = null;
  }
  viewer._statsPrev = null;
}

async function displayViewerSampleRtcStats(viewer, applyText) {
  if (!viewer.pc || viewer.pc.connectionState !== 'connected') return;
  const stats = await viewer.pc.getStats();
  const summary = summarizeRtcStats(stats, viewer._statsPrev);
  viewer._statsPrev = summary.snapshot;
  if (summary.text) applyText(summary.text);
}

// ── No-track watchdog (shared driver) ───────────────────────────────────
// One patience budget for "the connection negotiated but no video ever
// arrived" on both paths (the peer path had it first; the local slot
// ported it). The verdict — status copy, overlay, side effects like the
// peer's Station activity event — is per-class and runs in `onTimeout`,
// which must ALSO re-check its own liveness guards (first frame seen /
// closed-by-user locally; `this.stream` on the peer), exactly like the
// original inline callbacks did. `timeoutMs` is passed from each class's
// public NO_TRACK_TIMEOUT_MS static so a QA override on the class keeps
// working.
const DISPLAY_VIEWER_NO_TRACK_TIMEOUT_MS = 10000;

function displayViewerArmNoTrackWatchdog(viewer, onTimeout, timeoutMs = DISPLAY_VIEWER_NO_TRACK_TIMEOUT_MS) {
  displayViewerClearNoTrackWatchdog(viewer);
  viewer._noTrackTimer = window.setTimeout(() => {
    viewer._noTrackTimer = null;
    onTimeout();
  }, timeoutMs);
}

function displayViewerClearNoTrackWatchdog(viewer) {
  if (viewer._noTrackTimer !== null && viewer._noTrackTimer !== undefined) {
    window.clearTimeout(viewer._noTrackTimer);
    viewer._noTrackTimer = null;
  }
}

// ── Live-video pause guard (shared driver) ──────────────────────────────
// WebKit pauses a muted live <video> on tab switches and on every DOM
// reparent (the local stage moves whole through the thumb / fullscreen /
// Station containers; peer pane DOM is rebuilt on every daemons-list
// re-render) and does NOT auto-resume; Chromium mostly resumes srcObject
// streams on its own, which is why this never surfaced before Safari
// became a supported consumer. A paused element under a live track shows
// a frozen frame while the input datachannels keep working — it reads as
// catastrophic remote-control lag (live incident, 2026-07-13). While the
// viewer is live, every pause is spurious: resume it.
//
// The guard state lives on `owner._pauseGuard` and is REBOUND by every
// `displayViewerArmPauseGuard` call: the local DisplaySlot arms once (its
// element lives for the slot's whole life), the peer path re-arms on
// every pane rebuild so the resume path always targets the CURRENT
// element. Stale listeners on replaced elements go inert via the
// owner/element identity checks rather than being removed (the old
// element is garbage as soon as the pane rebuild drops it).
const DISPLAY_VIEWER_PAUSE_RESUME_DELAY_MS = 120;

function displayViewerArmPauseGuard(owner, videoEl, isLive, onResume) {
  owner._pauseGuard = {
    videoEl: videoEl || null,
    isLive: isLive || (() => true),
    onResume: onResume || null,
  };
  if (!videoEl || videoEl._displayViewerPauseGuardFor === owner) return;
  videoEl._displayViewerPauseGuardFor = owner;
  videoEl.addEventListener('pause', () => {
    const guard = owner._pauseGuard;
    // Inert when the guard was rebound to a replacement element (peer
    // pane rebuild) or the element to a newer connection (peer retry
    // reuses the pane): the CURRENT binding owns resumes.
    if (!guard || guard.videoEl !== videoEl) return;
    if (videoEl._displayViewerPauseGuardFor !== owner) return;
    displayViewerResumeLiveVideoSoon(owner);
  });
}

function displayViewerResumeLiveVideoSoon(owner) {
  const guard = owner._pauseGuard;
  if (!guard || owner._resumeVideoPending) return;
  if (!guard.isLive() || !guard.videoEl || !guard.videoEl.srcObject) return;
  owner._resumeVideoPending = true;
  // Next macrotask: a reparent's pause fires between the removal and the
  // re-insert, and a play() issued while the element is out of the
  // document is voided by the move itself.
  setTimeout(() => {
    owner._resumeVideoPending = false;
    // Re-read the guard: a pane rebuild inside the delay retargets the
    // resume at the current element (the replaced one is garbage).
    const g = owner._pauseGuard;
    if (!g || !g.isLive() || !g.videoEl || !g.videoEl.srcObject) return;
    if (!g.videoEl.paused) return;
    const p = g.videoEl.play();
    if (p && p.catch) p.catch(() => {});
    if (g.onResume) g.onResume();
  }, DISPLAY_VIEWER_PAUSE_RESUME_DELAY_MS);
}

// Re-kick playback if the element sits paused under a live connection
// (tab return, missed pause event during a reparent). Safe to call any
// time; no-ops unless the owner's guard says live with a stream attached.
function displayViewerResumeLiveVideoIfPaused(owner) {
  const guard = owner._pauseGuard;
  if (guard && guard.videoEl && guard.videoEl.paused) {
    displayViewerResumeLiveVideoSoon(owner);
  }
}

// ── Post-first-frame freeze watchdog (shared driver) ────────────────────
// The no-track watchdog above covers "connected but no video EVER
// arrived"; this covers the rest of the session: a stream that rendered
// fine and then stopped advancing. Progress is measured as PRESENTED
// frames via a requestVideoFrameCallback pump where the engine supports
// it (the pump is the honest signal for the incident class — a paused or
// wedged element under a live track, where DECODE keeps advancing);
// engines without rVFC fall back to framesDecoded deltas from the shared
// 3s stats sampler (`viewer._statsPrev`), which catches stream-level
// freezes but cannot see a stalled element.
//
// Honesty of the timeout: the daemon-side capture bridge re-pushes the
// latest frame once per second on idle desktops (IDLE_HEARTBEAT in
// crates/intendant-display), so ≥1 fps reaches a healthy viewer even
// with nothing changing on screen — six missed heartbeats is evidence of
// a stall, not an idle desktop.
//
// Escalation ladder (never an auto-reconnect loop): after
// DISPLAY_VIEWER_FREEZE_TIMEOUT_MS without progress, ONE automatic
// resume attempt through the pause-guard path; if frames still don't
// advance within the grace window, `onStalled(seconds)` fires once so
// the class surfaces its stage overlay with the manual reconnect
// affordance. `onRecovered()` fires only on real frame progress after
// either step, so the class can clear that overlay.
//
// Gating: ticks are inert while the connection isn't live (retry /
// disconnect machinery owns messaging then — the watchdog never stomps
// their overlays), while the tab is hidden, and while the element isn't
// actually rendered (display:none pane, deselected `ui2-live-inactive`
// stage, tile-mode-hidden peer video, 0-dim containers) — rVFC
// legitimately stops firing in all of those, so freshness is unknowable
// and the pause-guard visibility sweep owns resume-on-return instead.
const DISPLAY_VIEWER_FREEZE_TIMEOUT_MS = 6000;
const DISPLAY_VIEWER_FREEZE_RESUME_GRACE_MS = 2000;
const DISPLAY_VIEWER_FREEZE_POLL_MS = 1000;

// `hooks`: { videoEl(), isLive(), tryResume(), onStalled(seconds),
// onRecovered() } — all required. Arm on first rendered frame; re-arming
// replaces the previous watchdog (fresh negotiation = fresh baseline).
function displayViewerArmFreezeWatchdog(viewer, hooks) {
  displayViewerClearFreezeWatchdog(viewer);
  const gen = (viewer._freezeWatchGen || 0) + 1;
  viewer._freezeWatchGen = gen;
  viewer._freezeWatch = {
    gen,
    hooks,
    lastProgressAt: performance.now(),
    lastFrames: viewer._statsPrev ? viewer._statsPrev.frames : null,
    resumeAttempted: false,
    overlayShown: false,
    pumpEl: null,
    timer: window.setInterval(
      () => displayViewerFreezeWatchTick(viewer, gen),
      DISPLAY_VIEWER_FREEZE_POLL_MS,
    ),
  };
  displayViewerFreezeWatchBindPump(viewer);
}

function displayViewerClearFreezeWatchdog(viewer) {
  const w = viewer._freezeWatch;
  if (!w) return;
  if (w.timer) window.clearInterval(w.timer);
  viewer._freezeWatch = null;
}

// (Re)bind the rVFC pump to the CURRENT video element. The local slot's
// element is constructor-owned so the arm-time bind is final; the peer
// path calls this again from attachToDom because its pane <video> is
// rebuilt on every daemons-list re-render. A pump on a replaced element
// stops re-arming itself the next time it fires and sees it lost the
// element identity check.
function displayViewerFreezeWatchBindPump(viewer) {
  const w = viewer._freezeWatch;
  if (!w) return;
  const el = w.hooks.videoEl();
  if (!el || typeof el.requestVideoFrameCallback !== 'function') return;
  if (w.pumpEl === el) return;
  w.pumpEl = el;
  const gen = w.gen;
  const pump = () => {
    const cur = viewer._freezeWatch;
    if (!cur || cur.gen !== gen) return; // watchdog cleared or re-armed
    if (cur.hooks.videoEl() !== el) {
      // Element replaced; the attachToDom rebind owns the new one. Fall
      // back to stats probing until it lands.
      if (cur.pumpEl === el) cur.pumpEl = null;
      return;
    }
    displayViewerFreezeWatchMarkProgress(viewer);
    el.requestVideoFrameCallback(pump);
  };
  el.requestVideoFrameCallback(pump);
}

function displayViewerFreezeWatchMarkProgress(viewer) {
  const w = viewer._freezeWatch;
  if (!w) return;
  w.lastProgressAt = performance.now();
  w.resumeAttempted = false;
  if (w.overlayShown) {
    w.overlayShown = false;
    w.hooks.onRecovered();
  }
}

// An element is "rendered" when it has a laid-out box with real area:
// covers detached nodes, display:none ancestors (deselected stages,
// hidden tab panels, the peer video hidden under tile mode), and 0-dim
// containers. visibility:hidden still measures — acceptable: engines
// keep presenting those, so the watchdog stays honest there.
function displayViewerElementRendered(el) {
  if (!el || !el.isConnected) return false;
  const rect = el.getBoundingClientRect();
  return rect.width > 1 && rect.height > 1;
}

function displayViewerFreezeWatchTick(viewer, gen) {
  const w = viewer._freezeWatch;
  if (!w || w.gen !== gen) return;
  const hooks = w.hooks;
  const now = performance.now();
  if (!hooks.isLive()) {
    // Retry/teardown machinery owns status + overlay from here; stand
    // down without touching them (a shown freeze overlay stays until
    // frames really resume or the class clears the watchdog).
    w.lastProgressAt = now;
    w.resumeAttempted = false;
    return;
  }
  // Fallback progress probe: only when no rVFC pump is driving (decode
  // progress cannot vouch for presentation, so it must not mask a
  // stalled element when the pump is available).
  if (!w.pumpEl) {
    const frames = viewer._statsPrev ? viewer._statsPrev.frames : null;
    if (frames !== null && frames !== undefined && frames !== w.lastFrames) {
      w.lastFrames = frames;
      displayViewerFreezeWatchMarkProgress(viewer);
      return;
    }
  }
  const el = hooks.videoEl();
  if (document.hidden || !displayViewerElementRendered(el)) {
    // Not being presented anywhere the user can see: freshness is
    // unknowable (rVFC legitimately idles), so reset the clock instead
    // of alarming. The visibilitychange/pageshow sweep owns resume.
    w.lastProgressAt = now;
    w.resumeAttempted = false;
    return;
  }
  const stalledMs = now - w.lastProgressAt;
  if (stalledMs < DISPLAY_VIEWER_FREEZE_TIMEOUT_MS) return;
  if (!w.resumeAttempted) {
    // Step 1, once per episode: the pause-guard resume path (a paused
    // element under a live track is the incident class; play() on an
    // already-playing element is harmless).
    w.resumeAttempted = true;
    hooks.tryResume();
    return;
  }
  if (stalledMs < DISPLAY_VIEWER_FREEZE_TIMEOUT_MS + DISPLAY_VIEWER_FREEZE_RESUME_GRACE_MS) return;
  if (!w.overlayShown) {
    // Step 2, once per episode: a visible, actionable state. The class
    // decides the copy + retry affordance; no automatic reconnects.
    w.overlayShown = true;
    hooks.onStalled(Math.max(1, Math.round(stalledMs / 1000)));
  }
}

// QA snapshot of a viewer's freeze-watchdog state (null when unarmed) —
// consumed by qa.liveDisplay() / qa.peerDisplays().
function displayViewerFreezeWatchQa(viewer) {
  const w = viewer._freezeWatch;
  if (!w) return null;
  return {
    armed: true,
    source: w.pumpEl ? 'rvfc' : 'stats',
    stalledMs: Math.max(0, Math.round(performance.now() - w.lastProgressAt)),
    resumeAttempted: Boolean(w.resumeAttempted),
    overlayShown: Boolean(w.overlayShown),
  };
}

// ── Frame capture + attach lane ─────────────────────────────────────────
// Rasterize a live surface (<video> or the peer tile canvas) at the given
// target size into { canvas, dataUrl, b64, width, height } — the frame
// shape 47-annotation-clips' editor, the callout arm, and the attach lane
// all consume. Sizing policy stays with the callers (the local slot
// optionally divides by devicePixelRatio for logical-resolution captures;
// the peer captures at intrinsic surface size).
function displayViewerRasterizeSurface(surface, width, height, quality) {
  const c = document.createElement('canvas');
  c.width = width;
  c.height = height;
  c.getContext('2d').drawImage(surface, 0, 0, width, height);
  const dataUrl = c.toDataURL('image/jpeg', quality);
  return { canvas: c, dataUrl, b64: dataUrl.split(',')[1], width, height };
}

// Ship a captured frame down the annotation-attach lane and queue it as a
// pending attachment. Owns the deterministic frame_id scheme (so
// attachments are distinguishable from streamed frames in the registry):
// `<streamBase>_attach-fNNNNN` with a per-viewer counter. `streamBase` is
// the policy-owned name — `display_<id>` locally,
// `peer_<safeHost>_display_<id>` on peer panes — so frame ids stay unique
// across hosts and never collide across surfaces. Returns false when the
// upload failed (already surfaced via dashboardMediaTransferFailed);
// callers gate their button confirmation on it.
async function displayViewerUploadAttachFrame(viewer, streamBase, frame) {
  if (!viewer._attachCounter) viewer._attachCounter = 0;
  viewer._attachCounter++;
  const stream = streamBase + '_attach';
  const frameId = stream + '-f' + String(viewer._attachCounter).padStart(5, '0');
  const payload = {
    t: 'annotation_attach',
    frame_id: frameId,
    stream: stream,
    data: frame.b64,
    note: '',
  };
  try {
    await sendDashboardMediaUpload(
      'api_media_annotation_attach',
      { frame_id: frameId, stream, note: '' },
      dashboardControlBase64ToBytes(frame.b64),
      payload,
      'annotation attach'
    );
  } catch (err) {
    dashboardMediaTransferFailed(err, 'annotation attach');
    return false;
  }
  if (typeof addPendingAttachment === 'function') {
    addPendingAttachment({
      frameId,
      stream,
      note: '',
      dataUrl: frame.dataUrl,
    });
  }
  return true;
}

// Toolbar-armed Callout: one-shot region flag shipped through the
// annotation-attach lane. Shared machinery lives in 47-annotation-clips
// (toggleLiveCallout); armable only while input authority is 'you'
// (button disabled otherwise via displayViewerApplyAuthorityButtons,
// disarmed on authority loss by both setAuthority paths).
function displayViewerToggleCallout(viewer, button) {
  toggleLiveCallout({
    provider: viewer._annotationSurfaceProvider(),
    button,
    captureFrame: (q) => viewer.captureCurrentFrame(q),
  });
}

// ── Bounded-retry budget (shared shape; mechanics are policy) ───────────
// Both viewers retry a failed connection at most 5 times with the same
// backoff and end in the same dead-end copy with a manual retry button.
// The MECHANICS deliberately differ and stay in the classes (see each
// policy object's `retrySemantics`): the local slot renegotiates in
// place — its server-side DisplaySession survives, so disconnect() +
// connect() on the same slot is a fresh offer the session can answer —
// while the peer path re-opens with a fresh session id via the full
// openPeerDisplay path, because re-offering on the same session id is
// not a wire shape the peer's WebRtcPeer lifecycle supports (its attempt
// counter therefore lives in a module-scope map keyed host|display,
// surviving connection replacement).
const DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS = 5;

function displayViewerRetryDelayMs(attempts) {
  return Math.min(2000 * attempts, 10000);
}

// Dead-end copy, shared verbatim by both paths (status line + stage
// overlay variant with the trailing period).
const DISPLAY_VIEWER_RETRY_DEAD_END_STATUS =
  `Connection failed after ${DISPLAY_VIEWER_RETRY_MAX_ATTEMPTS} attempts`;
const DISPLAY_VIEWER_RETRY_DEAD_END_OVERLAY =
  DISPLAY_VIEWER_RETRY_DEAD_END_STATUS + '.';
