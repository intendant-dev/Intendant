// ── D-2 tile-test harness (synthetic tile stream + bootstrap) ─────────
//
// Relocated VERBATIM from static/app/51-peer-approvals.js so its ~450
// lines stop riding every dashboard page load. It is a deliberately
// parked seed for the #82 dirty-region/tile streaming work — do not
// delete or shrink its behavior.
//
// Loading contract: this file is a plain CLASSIC script (the dashboard
// SPA is one module script). It is injected on demand by the loader at
// the bottom of static/app/51-peer-approvals.js when the flag is active
// (`?tile-test=1` or `localStorage.tileTest === '1'`). Because the SPA's
// bindings are module-scoped, the loader exports the pieces this file
// drives onto `window` immediately before injection:
//
//   window.TileCompositor          (class, 51-peer-approvals.js)
//   window.CanvasFreshnessSampler  (class, 51-peer-approvals.js)
//   window.diagModeEnabled         (function, 51-peer-approvals.js)
//
// Keep that list in sync with the loader. Everything else this file
// needs is defined below.
//
// What this exercises (see docs/design-tile-streaming.md):
// - SnapshotChunk reassembly + epoch/snapshot_id tracking
// - TileUpdate per-tile staleness check (drops out-of-order tiles)
// - Resize → epoch advance → snapshot reseed
// - raw_bgra and rle_bgra payload decoding
// - requestAnimationFrame-based marker freshness sampling on canvas
//
// What it deliberately does NOT exercise (per D-2 scope):
// - WebRTC data-channel transport (D-3)
// - Backpressure / chunked snapshot pacing on the wire (D-3 / D-4)
// - Real X11 capture → tile encoder pipeline (D-3)
// - Fallback-to-VP8 policy (D-4)

// SyntheticTileStream — generates a tile stream that exercises every
// compositor codepath. Drives at the configured fps. Includes:
// - Initial chunked snapshot (covers all tiles + cursor + marker).
// - Per-tick TileUpdate with cursor erase/redraw + marker bit-pattern.
// - Periodic stale TileUpdate (older seq) to verify per-tile staleness.
// - Periodic resize event (every ~30s) to verify epoch reset + snapshot.
class SyntheticTileStream {
  constructor(compositor, opts = {}) {
    this.compositor = compositor;
    this.tileSize = opts.tileSize ?? 64;
    this.gridW = opts.gridW ?? 24;
    this.gridH = opts.gridH ?? 14;
    this.fps = opts.fps ?? 30;
    this.epoch = 1;
    this.seq = 0;
    this.snapshotIdCounter = 1;
    this.cursorPos = { x: 200, y: 200 };
    this.cursorVel = { x: 7, y: 5 };
    this.markerValue = 0;
    this.timer = null;
    this.frameCount = 0;
    this.metrics = {
      framesEmitted: 0,
      stalesInjected: 0,
      resizesEmitted: 0,
      snapshotsEmitted: 0,
      snapshotsChunked: 0,
    };
  }

  start() {
    this._emitSnapshot();
    const intervalMs = 1000 / this.fps;
    this.timer = setInterval(() => this._tick(), intervalMs);
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  _emitSnapshot() {
    const snapshotId = this.snapshotIdCounter++;
    const records = [];
    // Background: checkerboard of two greys so individual tiles are
    // visually distinguishable.
    for (let ty = 0; ty < this.gridH; ty++) {
      for (let tx = 0; tx < this.gridW; tx++) {
        const dark = (tx + ty) % 2 === 0;
        records.push({
          tile_x: tx,
          tile_y: ty,
          encoding: 0,
          payload: this._solidTileBgra(dark ? [40, 40, 50] : [60, 60, 75]),
        });
      }
    }
    // Cursor area.
    for (const rec of this._cursorTileRecords(this.cursorPos)) {
      records.push(rec);
    }
    // Marker tiles.
    for (const rec of this._markerTileRecords()) {
      records.push(rec);
    }

    // Chunk: cap each chunk at 32 records so the compositor's
    // snapshot reassembly path actually sees multiple chunks per
    // logical snapshot. Real D-3 will cap by byte size; for D-2,
    // by-record is good enough to exercise the assembly logic.
    const chunkSize = 32;
    const chunkCount = Math.ceil(records.length / chunkSize);
    for (let i = 0; i < chunkCount; i++) {
      const chunkRecords = records.slice(i * chunkSize, (i + 1) * chunkSize);
      this.compositor.onSnapshotChunk({
        epoch: this.epoch,
        snapshot_id: snapshotId,
        chunk_index: i,
        chunk_count: chunkCount,
        grid_w_tiles: this.gridW,
        grid_h_tiles: this.gridH,
        tile_size_px: this.tileSize,
        records: chunkRecords,
      });
      this.metrics.snapshotsChunked++;
    }
    this.metrics.snapshotsEmitted++;
  }

  _tick() {
    this.frameCount++;
    this.seq++;

    const records = [];

    // Cursor motion: bounce off canvas edges.
    const oldCursor = { x: this.cursorPos.x, y: this.cursorPos.y };
    this.cursorPos.x += this.cursorVel.x;
    this.cursorPos.y += this.cursorVel.y;
    const margin = 20;
    if (this.cursorPos.x < margin || this.cursorPos.x > this.gridW * this.tileSize - margin) {
      this.cursorVel.x *= -1;
      this.cursorPos.x = Math.max(margin, Math.min(this.gridW * this.tileSize - margin, this.cursorPos.x));
    }
    if (this.cursorPos.y < margin || this.cursorPos.y > this.gridH * this.tileSize - margin) {
      this.cursorVel.y *= -1;
      this.cursorPos.y = Math.max(margin, Math.min(this.gridH * this.tileSize - margin, this.cursorPos.y));
    }

    // Erase the old cursor area (paint background tiles).
    for (const rec of this._cursorTileRecords(oldCursor, /*erase*/ true)) {
      records.push(rec);
    }
    // Paint the new cursor area.
    for (const rec of this._cursorTileRecords(this.cursorPos)) {
      records.push(rec);
    }

    // Marker tile bits change every frame so the freshness sampler
    // sees a transition per tick.
    this.markerValue = (this.markerValue + 1) >>> 0;
    for (const rec of this._markerTileRecords()) {
      records.push(rec);
    }

    this.compositor.onTileUpdate({
      epoch: this.epoch,
      seq: this.seq,
      records,
    });
    this.metrics.framesEmitted++;

    // Stale-check: every 100 frames, send an out-of-order TileUpdate
    // touching a far-away tile. The compositor's per-tile staleness
    // check should drop this against the most recent same-tile seq;
    // it would otherwise paint a bright red square at bottom-left.
    if (this.frameCount % 100 === 50) {
      this.compositor.onTileUpdate({
        epoch: this.epoch,
        seq: Math.max(1, this.seq - 50),
        records: [
          {
            tile_x: 0,
            tile_y: this.gridH - 1,
            encoding: 0,
            payload: this._solidTileBgra([255, 0, 0]),
          },
        ],
      });
      // Then immediately a current-seq update for the same tile so
      // the stale-vs-current contrast is visible if staleness check
      // breaks (red would appear) vs working (background restored).
      this.compositor.onTileUpdate({
        epoch: this.epoch,
        seq: this.seq + 1,
        records: [
          {
            tile_x: 0,
            tile_y: this.gridH - 1,
            encoding: 0,
            payload: this._solidTileBgra([60, 60, 75]),
          },
        ],
      });
      this.seq += 1; // keep our next emission ahead of this overwrite
      this.metrics.stalesInjected++;
    }

    // Resize every 30s: bump epoch + emit snapshot for new grid.
    // Toggle between 24x14 and 20x12 to exercise both shrinking and
    // re-growing the canvas.
    if (this.frameCount % (this.fps * 30) === 0) {
      const newGridW = this.gridW === 24 ? 20 : 24;
      const newGridH = this.gridH === 14 ? 12 : 14;
      this.gridW = newGridW;
      this.gridH = newGridH;
      this.epoch += 1;
      this.compositor.onResize({
        new_epoch: this.epoch,
        grid_w_tiles: this.gridW,
        grid_h_tiles: this.gridH,
        tile_size_px: this.tileSize,
      });
      this._emitSnapshot();
      this.metrics.resizesEmitted++;
      // Reset cursor inside new canvas bounds in case it was off-grid.
      this.cursorPos = { x: 200, y: 200 };
    }

    // Periodic snapshot every 30s INDEPENDENT of resize, on a phase
    // offset so they don't always coincide. Mirrors D-3 design's
    // SNAPSHOT_PERIOD.
    if (this.frameCount % (this.fps * 30) === Math.floor(this.fps * 15)) {
      this._emitSnapshot();
    }
  }

  // Cursor area is the set of tiles whose bounds intersect a 24-px
  // box around the cursor center. erase=true paints background only
  // (no cursor pixel). erase=false paints background + cursor.
  _cursorTileRecords({ x, y }, erase = false) {
    const radius = 16;
    const x0 = Math.max(0, Math.floor((x - radius) / this.tileSize));
    const y0 = Math.max(0, Math.floor((y - radius) / this.tileSize));
    const x1 = Math.min(this.gridW - 1, Math.floor((x + radius) / this.tileSize));
    const y1 = Math.min(this.gridH - 1, Math.floor((y + radius) / this.tileSize));
    const out = [];
    for (let ty = y0; ty <= y1; ty++) {
      for (let tx = x0; tx <= x1; tx++) {
        const dark = (tx + ty) % 2 === 0;
        const bg = dark ? [40, 40, 50] : [60, 60, 75];
        // Cursor over the marker tiles (0,0) and (1,0) is annoying
        // for the freshness sampler — skip cursor draw there. The
        // erase path also skips, so the marker tiles remain marker-
        // owned for the duration.
        if (ty === 0 && (tx === 0 || tx === 1)) continue;
        out.push({
          tile_x: tx,
          tile_y: ty,
          encoding: 0,
          payload: erase
            ? this._solidTileBgra(bg)
            : this._cursorTileBgra(tx, ty, { x, y }, [255, 200, 80], bg),
        });
      }
    }
    return out;
  }

  // Marker tiles render the visual-freshness marker pattern across
  // tiles (0,0) and (1,0) — together they form a 128×64 patch
  // matching VisualFreshnessSampler's MARKER_W / MARKER_H. Same
  // 16×16 sub-tile grid (TILE_PX=16, COLS=8, ROWS=4) as the video
  // marker so CanvasFreshnessSampler can use byte-identical decode
  // logic.
  _markerTileRecords() {
    const TILE_PX = 16;
    const COLS = 8;
    const ROWS = 4;
    const ts = this.tileSize;
    const out = [];
    for (let tileX = 0; tileX < 2; tileX++) {
      const buf = new Uint8ClampedArray(ts * ts * 4);
      // Fill background dark.
      for (let i = 0; i < buf.length; i += 4) {
        buf[i] = 35; buf[i + 1] = 30; buf[i + 2] = 30; buf[i + 3] = 255;
      }
      // Each marker tile carries 4 cols × 4 rows of sub-tiles.
      // Left tile (tileX=0): cols 0..3 of the 8-col marker.
      // Right tile (tileX=1): cols 4..7.
      const colOffset = tileX * 4;
      for (let row = 0; row < ROWS; row++) {
        for (let col = 0; col < 4; col++) {
          const globalCol = colOffset + col;
          const bit = row * COLS + globalCol;
          const set = ((this.markerValue >>> bit) & 1) === 1;
          if (!set) continue;
          // Fill the 16×16 sub-tile with bright pixels.
          for (let dy = 0; dy < TILE_PX; dy++) {
            for (let dx = 0; dx < TILE_PX; dx++) {
              const px = col * TILE_PX + dx;
              const py = row * TILE_PX + dy;
              const idx = (py * ts + px) * 4;
              buf[idx] = 230;
              buf[idx + 1] = 230;
              buf[idx + 2] = 230;
              buf[idx + 3] = 255;
            }
          }
        }
      }
      out.push({
        tile_x: tileX,
        tile_y: 0,
        encoding: 0,
        payload: this._rgbaToBgra(buf),
      });
    }
    return out;
  }

  // Solid color BGRA tile of `[R, G, B]` color (caller-friendly RGB
  // order; we swap to BGRA inside).
  _solidTileBgra([r, g, b]) {
    const ts = this.tileSize;
    const buf = new Uint8ClampedArray(ts * ts * 4);
    for (let i = 0; i < buf.length; i += 4) {
      buf[i] = b;
      buf[i + 1] = g;
      buf[i + 2] = r;
      buf[i + 3] = 255;
    }
    return buf;
  }

  // Cursor tile: background fill + amber cursor disc within radius
  // of the cursor center. Returns BGRA bytes.
  _cursorTileBgra(tileX, tileY, cursor, [cR, cG, cB], [bR, bG, bB]) {
    const ts = this.tileSize;
    const buf = new Uint8ClampedArray(ts * ts * 4);
    const tx0 = tileX * ts;
    const ty0 = tileY * ts;
    for (let dy = 0; dy < ts; dy++) {
      const py = ty0 + dy;
      for (let dx = 0; dx < ts; dx++) {
        const px = tx0 + dx;
        const inCursor = Math.hypot(px - cursor.x, py - cursor.y) < 12;
        const idx = (dy * ts + dx) * 4;
        if (inCursor) {
          buf[idx] = cB; buf[idx + 1] = cG; buf[idx + 2] = cR;
        } else {
          buf[idx] = bB; buf[idx + 1] = bG; buf[idx + 2] = bR;
        }
        buf[idx + 3] = 255;
      }
    }
    return buf;
  }

  _rgbaToBgra(rgba) {
    const out = new Uint8ClampedArray(rgba.length);
    for (let i = 0; i < rgba.length; i += 4) {
      out[i] = rgba[i + 2];
      out[i + 1] = rgba[i + 1];
      out[i + 2] = rgba[i];
      out[i + 3] = rgba[i + 3];
    }
    return out;
  }
}

// Bootstrap the synthetic harness — appended to body as a fixed-
// position pane so it doesn't collide with the existing dashboard
// chrome. CanvasFreshnessSampler activates only if `?diag=1` is also
// set, mirroring the existing video-path gating.
function startTileTestHarness() {
  const container = document.createElement('div');
  container.id = 'tile-test-harness';
  container.style.cssText =
    'position:fixed; right:16px; bottom:16px; background:#16161e;' +
    ' border:1px solid #444; padding:8px; border-radius:4px;' +
    ' font:11px ui-monospace,monospace; color:#ddd; z-index:99999;';

  const header = document.createElement('div');
  header.textContent = 'D-2 tile-test compositor (synthetic)';
  header.style.cssText = 'margin-bottom:6px; color:#8c8;';
  container.appendChild(header);

  const opts = { tileSize: 64, gridW: 24, gridH: 14, fps: 30 };
  const compositor = new TileCompositor(container, opts);
  const stream = new SyntheticTileStream(compositor, opts);

  document.body.appendChild(container);
  stream.start();

  let sampler = null;
  if (diagModeEnabled()) {
    sampler = new CanvasFreshnessSampler(compositor.canvas, 'tile-test', 0);
    sampler.start();
  }

  const metricsEl = document.createElement('pre');
  metricsEl.style.cssText = 'margin:6px 0 0; color:#aaa; font-size:10px; white-space:pre;';
  container.appendChild(metricsEl);
  setInterval(() => {
    const obj = {
      compositor: compositor.metrics,
      stream: stream.metrics,
    };
    if (sampler) {
      obj.sampler = {
        transitions: sampler.transitions,
        last_max_freeze_ms: Math.round(sampler.longestFreezeMs),
        gaps_buffered: sampler.gaps.length,
      };
    }
    metricsEl.textContent = JSON.stringify(obj, null, 2);
  }, 1000);

  return { compositor, stream, sampler };
}

// The flag gate lives in the loader (51-peer-approvals.js) — this file
// only ever loads when the flag is active, so start unconditionally.
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => {
    window.__tileTest = startTileTestHarness();
  });
} else {
  window.__tileTest = startTileTestHarness();
}
