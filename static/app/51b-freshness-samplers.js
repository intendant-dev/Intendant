// ── Visual-freshness samplers + display test toggles ──
// The ?diag=1 marker-freshness rig: FreshnessSamplerBase and the video /
// canvas subclasses 52-peer-display.js instantiates on connect, their
// NDJSON diagnostics POST, and the ?tile-test / ?federation-h264 test
// toggles plus the tile-test harness loader. The live tile pipeline the
// harness exercises (TileWireReader / TileCompositor) stays in
// 51-peer-approvals.js.

// **Phase 0 visual-freshness sampler** (task #83).
//
// Reads the diagnostic marker the peer's pool-feed bridge stamps into
// the top-left 128x64 px of every captured frame, decodes the 32-bit
// timestamp, tracks transitions, and POSTs an NDJSON transcript to
// `/api/diagnostics/visual-freshness?session_id=<browser_uuid>`. Used
// to measure visual freshness (effective fps + freeze intervals) without
// depending on getStats packet counters that proved misleading on
// task #81 (frozen viewer + jump-cut despite framesDecoded advancing).
//
// Activation: a `?diag=1` URL query param. The marker itself must be
// independently enabled on the peer side -- send
// `{"action":"set_diagnostics_visual_marker","display_id":<id>,"enabled":true}`
// to the peer's /ws (e.g. via the operator script in
// docs/smoke-display.md). When the marker isn't on, the sampler runs
// happily but observes zero transitions; the resulting transcript is
// the "no marker" baseline (useful as a control; obvious to spot).
//
// Geometry constants must match `src/bin/caller/display/visual_marker.rs`.
// 8x4 tiles x 16 px = 128x64 px patch in the top-left; tile centers
// sample at (col*16+8, row*16+8); luma threshold 128 splits the
// limited-range 16/235 pair the peer writes.
// Transport F7: the transcript POST rides the daemonApi facade. The
// method is twinned (POST /api/diagnostics/visual-freshness — the
// descriptor's one rawBody entry: the tunnel carries the NDJSON as a
// `body` param, the HTTP twin takes it as its raw request body), so the
// facade keeps the legacy lane order by policy: tunnel first, the
// POST-derived mutation rule refuses HTTP after any tunnel attempt, a
// tunnel-less direct dashboard still POSTs pre-attempt, Connect mode
// never touches HTTP. The daemon's own refusals (denied session /
// too-old daemon) are consulted up front instead of firing an RPC that
// can only bounce (F6 pattern); a transport-down verdict falls through —
// the facade picks the honest lane.
async function postVisualFreshnessDiagnostics(sessionId, ndjson) {
  const avail = daemonApi.availability('api_diagnostics_visual_freshness');
  if (avail.reason === 'denied' || avail.reason === 'unsupported') {
    throw new daemonApi.Error(
      avail.reason === 'denied' ? 'denied' : 'unavailable',
      'api_diagnostics_visual_freshness',
      null,
      `visual diagnostics are ${avail.reason} on this daemon`
    );
  }
  const resp = await daemonApi.request('api_diagnostics_visual_freshness', {
    session_id: sessionId,
    body: ndjson,
  }, { timeoutMs: 10000 });
  if (!resp.ok) {
    throw new Error(resp.body?.error || `diagnostics upload failed (${resp.status})`);
  }
  return resp.body;
}

// Shared base for the two marker-freshness samplers (video + canvas
// sources). Owns everything that doesn't depend on the sampled surface:
// marker geometry, the offscreen scratch canvas, transition bookkeeping,
// the NDJSON record buffer + 5s flush loop, marker decode, the summary
// percentiles, and stop(). Subclasses provide the frame scheduler
// (`_scheduleFrame`), the source readiness check (`_sourceReady`), the
// draw into the scratch canvas (`_drawSource`), and the session_start
// record (`_sessionStartRecord`). This is the pure dedupe of the two
// previously copy-pasted implementations — record shapes, timing, and
// log tags are unchanged.
class FreshnessSamplerBase {
  constructor(hostId, displayId, idPrefix, logTag) {
    this.hostId = hostId;
    this.displayId = displayId;
    this.browserSessionId = (window.crypto && window.crypto.randomUUID)
      ? window.crypto.randomUUID()
      : `${idPrefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
    this._logTag = logTag;

    // Marker geometry -- MUST match the peer-side visual_marker
    // module's TILE_PX / COLS / ROWS / THRESHOLD constants.
    this.MARKER_W = 128;
    this.MARKER_H = 64;
    this.TILE_PX = 16;
    this.COLS = 8;
    this.ROWS = 4;
    this.THRESHOLD = 128;

    // Offscreen canvas sized exactly to the marker patch. We draw only
    // the top-left region of the source into it so getImageData is
    // bounded to ~32 KB per sample regardless of source resolution.
    // willReadFrequently asks the browser to keep the backing buffer
    // CPU-side rather than GPU-side; getImageData is then cheap.
    this.canvas = document.createElement('canvas');
    this.canvas.width = this.MARKER_W;
    this.canvas.height = this.MARKER_H;
    this.ctx = this.canvas.getContext('2d', { willReadFrequently: true });

    this.startMs = performance.now();
    this.lastValue = null;
    this.lastTransitionMs = this.startMs;
    this.firstTransitionAt = null;

    // Buffered records. Flushed every 5s and on stop(); each flush also
    // synthesizes a cumulative summary record so the transcript captures
    // rolling stats even if the browser crashes before stop() runs.
    this.records = [];
    this.transitions = 0;
    this.gaps = []; // for percentile computation across the session
    this.longestFreezeMs = 0;

    this.flushTimer = null;
    this.stopped = false;
  }

  start() {
    this._enqueue(this._sessionStartRecord());
    this._scheduleFrame();
    this.flushTimer = setInterval(() => this._flush(), 5000);
  }

  _onFrame() {
    if (this.stopped) return;
    if (!this._sourceReady()) {
      // Source too small -- peer-side stamp is also a no-op at these
      // dims (see visual_marker::stamp_y_plane bounds check). Try
      // again next frame.
      this._scheduleFrame();
      return;
    }
    try {
      // Source rect: top-left MARKER_W x MARKER_H of the source frame
      // in *frame* (not displayed) coordinates. Dest rect: full
      // scratch canvas (also MARKER_W x MARKER_H). One-to-one pixel
      // copy -- no scaling, so tile centers in the canvas match tile
      // centers in the source frame exactly.
      this._drawSource();
      const img = this.ctx.getImageData(0, 0, this.MARKER_W, this.MARKER_H);
      const value = this._decode(img);
      if (this.lastValue !== null && value !== this.lastValue) {
        const nowMs = performance.now();
        const gap = nowMs - this.lastTransitionMs;
        this.transitions += 1;
        this.gaps.push(gap);
        if (gap > this.longestFreezeMs) this.longestFreezeMs = gap;
        if (this.firstTransitionAt === null) this.firstTransitionAt = nowMs;
        this._enqueue({
          t: 'transition',
          browser_ms: nowMs - this.startMs,
          value: value,
          gap_ms: Math.round(gap),
        });
        this.lastTransitionMs = nowMs;
      } else if (this.lastValue === null) {
        // First decoded marker value -- anchor `lastTransitionMs` HERE
        // (at first sample) instead of leaving it pinned to startMs (the
        // sampler-instantiation moment, which precedes ontrack→first-
        // decoded-frame→peer-marker-propagation by ~1s on a typical
        // federated VP8-q path). Without this, the first emitted
        // transition's gap_ms would conflate stream-warmup time with
        // actual frame-cadence -- which is exactly the 1044ms outlier
        // observed in transcript b8e2b947 of the #83 acceptance run.
        // After this fix, the first transition reports a gap_ms equal
        // to the true encoder send cadence (~33ms at 30fps), and the
        // session-percentile triple no longer carries a startup spike.
        this.lastTransitionMs = performance.now();
      }
      this.lastValue = value;
    } catch (e) {
      // CORS-tainted canvas would throw on getImageData. The sources
      // are same-origin (intendant:// scheme handler proxies to local
      // backend), so this shouldn't happen in production -- but log
      // loudly if it does.
      console.warn(`${this._logTag} sample failed:`, e);
    }
    this._scheduleFrame();
  }

  // Decode the 32-bit marker by sampling each tile center pixel,
  // computing BT.601 luminance, thresholding at 128. Bit layout:
  // bit_idx = row * COLS + col, LSB at top-left tile, MSB at
  // bottom-right -- matches `visual_marker::stamp_y_plane` exactly.
  // Returns an unsigned 32-bit value (`>>> 0` coerces JS bitwise's
  // signed-i32 result back to u32).
  _decode(imageData) {
    const { data, width } = imageData; // RGBA, 4 bytes per pixel
    let v = 0;
    for (let row = 0; row < this.ROWS; row++) {
      const cy = row * this.TILE_PX + (this.TILE_PX >> 1);
      for (let col = 0; col < this.COLS; col++) {
        const cx = col * this.TILE_PX + (this.TILE_PX >> 1);
        const idx = (cy * width + cx) * 4;
        const r = data[idx];
        const g = data[idx + 1];
        const b = data[idx + 2];
        // BT.601 luma matches what the peer's bgra_to_i420 produced
        // (full-range Y = 0.299 R + 0.587 G + 0.114 B).
        const luma = 0.299 * r + 0.587 * g + 0.114 * b;
        if (luma >= this.THRESHOLD) {
          v |= 1 << (row * this.COLS + col);
        }
      }
    }
    return v >>> 0;
  }

  _enqueue(record) {
    this.records.push(record);
  }

  _flush(options = {}) {
    if (this.stopped && !options.allowStopped) return;
    const summary = this._buildSummary();
    this._enqueue(summary);
    this._postBatch();
  }

  _buildSummary() {
    const sorted = [...this.gaps].sort((a, b) => a - b);
    const percentile = (q) => {
      if (sorted.length === 0) return 0;
      const idx = Math.min(sorted.length - 1, Math.floor((sorted.length - 1) * q));
      return Math.round(sorted[idx]);
    };
    const elapsedMs = performance.now() - this.startMs;
    const fps = elapsedMs > 0
      ? Math.round((this.transitions * 100000 / elapsedMs)) / 100
      : 0;
    return {
      t: 'summary',
      browser_ms: Math.round(elapsedMs),
      transitions: this.transitions,
      p50_gap_ms: percentile(0.5),
      p95_gap_ms: percentile(0.95),
      max_gap_ms: sorted.length ? Math.round(sorted[sorted.length - 1]) : 0,
      longest_freeze_ms: Math.round(this.longestFreezeMs),
      effective_fps: fps,
    };
  }

  _postBatch() {
    if (this.records.length === 0) return;
    const ndjson = this.records.map(r => JSON.stringify(r)).join('\n') + '\n';
    this.records = [];
    postVisualFreshnessDiagnostics(this.browserSessionId, ndjson)
      .catch(err => console.warn(`${this._logTag} upload failed:`, err));
  }

  stop() {
    if (this.stopped) return;
    this.stopped = true;
    if (this.flushTimer) {
      clearInterval(this.flushTimer);
      this.flushTimer = null;
    }
    this._enqueue({
      t: 'session_end',
      browser_ms: Math.round(performance.now() - this.startMs),
    });
    // Final summary gets emitted as part of _flush() called by the
    // session_end record's flush path.
    this._flush({ allowStopped: true });
  }
}

class VisualFreshnessSampler extends FreshnessSamplerBase {
  constructor(videoEl, hostId, displayId) {
    super(hostId, displayId, 'vf', '[diag-vf]');
    this.videoEl = videoEl;
    // Use rVFC where available (Safari 16+, Chrome 83+) so the
    // callback fires once per actually-rendered frame instead of
    // once per display refresh. On rVFC-less browsers we fall back to
    // requestAnimationFrame which is good enough for Phase 0.
    this._useRVFC = 'requestVideoFrameCallback' in HTMLVideoElement.prototype;
  }

  _sessionStartRecord() {
    return {
      t: 'session_start',
      browser_ms: 0,
      browser_session_id: this.browserSessionId,
      host_id: this.hostId,
      display_id: this.displayId,
      video_width: this.videoEl.videoWidth || 0,
      video_height: this.videoEl.videoHeight || 0,
      ua: navigator.userAgent,
      uses_rvfc: this._useRVFC,
      source: 'video',
    };
  }

  _scheduleFrame() {
    if (this.stopped) return;
    if (this._useRVFC) {
      this.videoEl.requestVideoFrameCallback(() => this._onFrame());
    } else {
      requestAnimationFrame(() => this._onFrame());
    }
  }

  _sourceReady() {
    const w = this.videoEl.videoWidth || 0;
    const h = this.videoEl.videoHeight || 0;
    return w >= this.MARKER_W && h >= this.MARKER_H;
  }

  _drawSource() {
    this.ctx.drawImage(
      this.videoEl,
      0, 0, this.MARKER_W, this.MARKER_H,
      0, 0, this.MARKER_W, this.MARKER_H,
    );
  }
}

// True when the dashboard URL has `?diag=1` (or `?...&diag=1`). The
// peer-display sampler activates automatically on connect when this
// is true; otherwise no canvas / rVFC / fetch overhead. The flag is
// read once per page load.
function diagModeEnabled() {
  try {
    const params = new URLSearchParams(window.location.search);
    return params.get('diag') === '1';
  } catch {
    return false;
  }
}


// CanvasFreshnessSampler — reads the freshness marker off a canvas
// instead of a video element. Same marker geometry / decode / record
// schema as VisualFreshnessSampler (both via FreshnessSamplerBase) so
// the transcript is consumable by the same
// `/api/diagnostics/visual-freshness` sink. Uses requestAnimationFrame
// instead of requestVideoFrameCallback.
class CanvasFreshnessSampler extends FreshnessSamplerBase {
  constructor(sourceCanvas, hostId, displayId) {
    super(hostId, displayId, 'vf-canvas', '[diag-vf canvas]');
    this.sourceCanvas = sourceCanvas;
  }

  _sessionStartRecord() {
    return {
      t: 'session_start',
      browser_ms: 0,
      browser_session_id: this.browserSessionId,
      host_id: this.hostId,
      display_id: this.displayId,
      video_width: this.sourceCanvas.width,
      video_height: this.sourceCanvas.height,
      ua: navigator.userAgent,
      uses_rvfc: false, // canvas path is rAF-only by construction
      source: 'canvas',
    };
  }

  _scheduleFrame() {
    if (this.stopped) return;
    requestAnimationFrame(() => this._onFrame());
  }

  _sourceReady() {
    return this.sourceCanvas.width >= this.MARKER_W
      && this.sourceCanvas.height >= this.MARKER_H;
  }

  _drawSource() {
    this.ctx.drawImage(
      this.sourceCanvas,
      0, 0, this.MARKER_W, this.MARKER_H,
      0, 0, this.MARKER_W, this.MARKER_H,
    );
  }
}

// Activated via `?tile-test=1` OR `localStorage.tileTest === '1'`.
// The localStorage path lets you flip it from the WKWebView console
// without needing a launch-environment env var.
function tileTestEnabled() {
  try {
    const params = new URLSearchParams(window.location.search);
    if (params.get('tile-test') === '1') return true;
  } catch { /* fall through */ }
  try {
    if (window.localStorage && window.localStorage.getItem('tileTest') === '1') return true;
  } catch { /* fall through */ }
  return false;
}

// Per-session, per-viewer override that PREFERS H.264 for a federated
// `PeerDisplayConnection`, independent of the gateway-wide
// `[webrtc].federation_allow_h264` config flag. Activated via
// `?federation-h264=1` OR `localStorage.federationH264 === '1'`.
//
// Why a per-session toggle: the federated H.264-under-loss A/B needs to
// flip one viewer to H.264 without changing the daemon default (which
// stays VP8 unless the operator sets the config flag). The gateway flag
// is process-wide and applies to every federated viewer; this override is
// scoped to the browser/tab running the test, and OR's with the flag — so
// it only ever ADDS H.264 preference, never removes it, and leaves the
// VP8 default untouched when neither is set. Mirrors `tileTestEnabled`'s
// dual URL-param / localStorage shape.
function federationH264TestEnabled() {
  try {
    const params = new URLSearchParams(window.location.search);
    if (params.get('federation-h264') === '1') return true;
  } catch { /* fall through */ }
  try {
    if (window.localStorage && window.localStorage.getItem('federationH264') === '1') return true;
  } catch { /* fall through */ }
  return false;
}

// D-2 tile-test harness loader. The harness itself (SyntheticTileStream
// + startTileTestHarness + its auto-start) lives VERBATIM in
// static/tile-test-harness.js — a deliberately parked seed relocated out
// of this fragment so its ~450 lines stop shipping in every page load.
// Injected on demand only when the flag is active.
//
// Glue contract: the harness file is a plain classic script while this
// SPA is one module script, so the module-scoped pieces it drives are
// exported on window right before injection. Keep this list in sync
// with the header of static/tile-test-harness.js.
//
// Cache-busting note: dynamically injected embedded assets follow the
// /xterm.min.js and /codemirror-bundle.js convention — a bare path, with
// freshness handled by the gateway's ETag revalidation (`no-cache,
// must-revalidate` for unversioned asset requests). The server-side
// `?v=` rewrite applies only to URLs inside app.html.
if (tileTestEnabled()) {
  window.TileCompositor = TileCompositor;
  window.CanvasFreshnessSampler = CanvasFreshnessSampler;
  window.diagModeEnabled = diagModeEnabled;
  const tileTestScript = document.createElement('script');
  tileTestScript.src = '/tile-test-harness.js';
  tileTestScript.onerror = () => console.error(
    '[tile-test] failed to load /tile-test-harness.js — the daemon build must embed it (web_gateway/static_assets.rs)');
  document.head.appendChild(tileTestScript);
}
