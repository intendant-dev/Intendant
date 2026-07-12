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
