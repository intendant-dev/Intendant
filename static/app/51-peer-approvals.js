// ── Per-peer pending approvals ──
//
// Approval requests arrive over each peer's secondary WebSocket as
// `approval_required` events. They're surfaced inline in the peer's
// controls panel with approve / deny / skip buttons that POST to
// /api/peers/{id}/approval. Buttons map to the four-way ApprovalDecision
// vocabulary (accept / accept_for_session / decline / cancel) — the
// dashboard surfaces three of them today; AcceptForSession can be added
// when there's a use case. Wire-level encoding is in
// peer::transport::intendant (ResolveApproval → ControlMsg::Approve|Deny|Skip).

function addPendingApproval(hostId, approvalId, command, category) {
  let m = peerPendingApprovals.get(hostId);
  if (!m) { m = new Map(); peerPendingApprovals.set(hostId, m); }
  m.set(String(approvalId), { command: command || '', category: category || '' });
  renderPeerApprovals(hostId);
  stationScheduleUpdate();
}

function removePendingApproval(hostId, approvalId) {
  const m = peerPendingApprovals.get(hostId);
  if (!m) return;
  m.delete(String(approvalId));
  if (m.size === 0) peerPendingApprovals.delete(hostId);
  renderPeerApprovals(hostId);
  stationScheduleUpdate();
}

// Render the approvals section inside a peer's controls panel. Called
// when an approval is added/removed and at the tail of renderDaemonsList
// so the section survives the full row re-renders that happen on every
// push event.
function renderPeerApprovals(hostId) {
  const panel = document.getElementById(`daemon-controls-${hostId}`);
  if (!panel) return;
  let section = panel.querySelector('.daemon-approvals-section');
  const pending = peerPendingApprovals.get(hostId);
  if (!pending || pending.size === 0) {
    if (section) section.remove();
    return;
  }
  if (!section) {
    section = document.createElement('div');
    section.className = 'daemon-approvals-section';
    panel.insertBefore(section, panel.firstChild);
  }
  const rows = [];
  for (const [approvalId, { command }] of pending.entries()) {
    rows.push(`
      <div class="daemon-approval-row" data-approval-id="${escapeHtml(approvalId)}">
        <span class="daemon-approval-id" title="approval id">#${escapeHtml(approvalId)}</span>
        <span class="daemon-approval-cmd" title="${escapeHtml(command)}">${escapeHtml(command)}</span>
        <div class="daemon-approval-actions">
          <button class="approve" data-host-id="${escapeHtml(hostId)}" data-approval-id="${escapeHtml(approvalId)}" data-decision="accept">Approve</button>
          <button class="deny"    data-host-id="${escapeHtml(hostId)}" data-approval-id="${escapeHtml(approvalId)}" data-decision="decline">Deny</button>
          <button class="skip"    data-host-id="${escapeHtml(hostId)}" data-approval-id="${escapeHtml(approvalId)}" data-decision="cancel">Skip</button>
        </div>
      </div>
    `);
  }
  section.innerHTML = rows.join('');
  // Wire approve/deny/skip buttons. Event delegation would also work,
  // but binding each row keeps the data-flow obvious and matches the
  // pattern used by the message-send wiring above.
  section.querySelectorAll('button[data-decision]').forEach(btn => {
    btn.addEventListener('click', () =>
      resolvePeerApproval(btn.dataset.hostId, btn.dataset.approvalId, btn.dataset.decision)
    );
  });
}

async function resolvePeerApproval(hostId, approvalId, decision) {
  if (!hostId || !approvalId || !decision) return;
  // Disable all buttons in the row to prevent double-click while
  // the POST is in flight. Re-enable on failure so the user can retry.
  //
  // Look up the panel via getElementById (the id field permits
  // colons, e.g. `daemon-controls-intendant:alpha`) and then scope
  // the data-attribute query to the panel — embedding the id in a
  // CSS selector would parse the colon as a pseudo-class prefix and
  // throw a SyntaxError before the fetch() ever fires.
  const panel = document.getElementById(`daemon-controls-${hostId}`);
  const row = panel
    ? panel.querySelector(`.daemon-approval-row[data-approval-id="${CSS.escape(approvalId)}"]`)
    : null;
  if (row) row.querySelectorAll('button').forEach(b => b.disabled = true);
  try {
    // Approval decisions are mutations (transport F5): the facade derives
    // no-replay from the POST verb — never re-delivered over HTTP after a
    // tunnel attempt that may have reached the daemon, no retries, params
    // unchanged (peer_id lifts into the HTTP twin's path).
    const resp = await daemonApi.request('api_peer_approval', {
      peer_id: hostId,
      request_id: approvalId,
      decision,
    });
    if (resp.ok) {
      // Optimistic removal. The peer will eventually emit
      // `approval_resolved` over the secondary stream which would
      // also remove it; doing it here keeps the UI snappy.
      removePendingApproval(hostId, approvalId);
    } else {
      console.error(`approval failed for ${hostId}#${approvalId}: ${resp.body?.error || resp.status}`);
      if (row) row.querySelectorAll('button').forEach(b => b.disabled = false);
    }
  } catch (e) {
    console.error(`approval error for ${hostId}#${approvalId}: ${e.message}`);
    if (row) row.querySelectorAll('button').forEach(b => b.disabled = false);
  }
}

// Shared submission helper for the two outbound op verbs that drive
// off the same input: message (FollowUp) and task (StartTask). Differ
// in HTTP path, request body shape, response field name, and the
// label shown in the success status — everything else is identical.
async function submitPeerInput(hostId, kind) {
  if (!hostId) return;
  const input = document.querySelector(
    `.daemon-msg-input[data-host-id="${CSS.escape(hostId)}"]`
  );
  const buttons = document.querySelectorAll(
    `[data-host-id="${CSS.escape(hostId)}"].daemon-msg-send, [data-host-id="${CSS.escape(hostId)}"].daemon-task-send`
  );
  const statusEl = document.querySelector(
    `.daemon-msg-status[data-host-id="${CSS.escape(hostId)}"]`
  );
  if (!input) return;

  const text = input.value.trim();
  if (!text) {
    if (statusEl) {
      statusEl.textContent = kind === 'task' ? 'Task instructions empty.' : 'Message is empty.';
      statusEl.className = 'daemon-msg-status error';
    }
    return;
  }

  // Disable both buttons during the in-flight POST. Re-enabling them
  // both on completion is the simplest correct behavior — disabling
  // only the clicked button leaves the other one usable in a way that
  // could double-submit.
  buttons.forEach(b => b.disabled = true);
  if (statusEl) {
    statusEl.textContent = kind === 'task' ? 'Starting task…' : 'Sending…';
    statusEl.className = 'daemon-msg-status';
  }

  const rpcMethod = kind === 'task' ? 'api_peer_task' : 'api_peer_message';
  const rpcParams = kind === 'task'
    ? { peer_id: hostId, instructions: text }
    : { peer_id: hostId, text };
  try {
    // Outbound peer ops are mutations (transport F5): verb-derived
    // no-replay, no retries; peer_id lifts into the HTTP twin's path so
    // the body keeps its legacy `{instructions}` / `{text}` shape.
    const resp = await daemonApi.request(rpcMethod, rpcParams);
    const result = resp.body || {};
    if (!resp.ok) {
      if (statusEl) {
        statusEl.textContent = `Failed: ${result.error || `HTTP ${resp.status}`}`;
        statusEl.className = 'daemon-msg-status error';
      }
    } else {
      if (statusEl) {
        const id = kind === 'task'
          ? (result.task_id || '?')
          : (result.message_id || '?');
        const verb = kind === 'task' ? 'Task started' : 'Sent';
        statusEl.textContent = `${verb} (id ${id}).`;
        statusEl.className = 'daemon-msg-status ok';
      }
      input.value = '';
    }
  } catch (e) {
    if (statusEl) {
      statusEl.textContent = `Error: ${e.message}`;
      statusEl.className = 'daemon-msg-status error';
    }
  } finally {
    buttons.forEach(b => b.disabled = false);
  }
}

function sendPeerMessage(hostId) { return submitPeerInput(hostId, 'message'); }
function sendPeerTask(hostId)    { return submitPeerInput(hostId, 'task'); }

// ── Per-peer WebRTC display (slice 3a) ──
//
// Browser opens a direct WebRTC connection to a peer's display, with
// the primary acting as signaling middleman only — encoded video flows
// browser↔peer, never through primary. Lazy: created on "View display"
// click. Single focused pane per host (slice 3a scope; thumbnail strip
// + multi-pane mosaic deferred). View-only — no input/clipboard data
// channels yet (the local-display flow has them; federation parity is
// a follow-up).
//
// Lifecycle:
// - openPeerDisplay: generate session_id, build PeerDisplayConnection,
//   create offer, POST to /api/peers/{id}/webrtc, await answer
//   asynchronously via the peer_webrtc_signal UiCommand path.
// - handlePeerWebRtcSignal: routes Answer / IceCandidate / Close to
//   the matching connection by (host_id, display_id, session_id).
// - closePeerDisplaysForHost: explicit close button + auto-cleanup on
//   peer removal. Sends a Close signal to the peer so it tears down
//   its WebRtcPeer (otherwise it'd leak until the federation transport
//   disconnects).
// - On daemon-list re-render (peer_state_changed etc.), the video
//   element gets regenerated. reapplyPeerDisplayPanes finds the new
//   element and re-attaches the live MediaStream so the stream
//   doesn't stutter through the DOM swap.

function generateSessionId() {
  if (window.crypto && window.crypto.randomUUID) {
    return window.crypto.randomUUID();
  }
  return `sess-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function peerCanShareDisplay(d) {
  if (!Array.isArray(d.capabilities)) return false;
  return d.capabilities.some(c => c && c.kind === 'display');
}

// =====================================================================
// D-2: tile wire parsing + compositor + freshness samplers.
//
// Everything from here through CanvasFreshnessSampler is LIVE peer-
// display machinery: 52-peer-display.js drives TileWireReader /
// parseTileWireFrame / TileCompositor on the real WebRTC tile
// datachannels (D-3b+), and the two freshness samplers under `?diag=1`.
//
// The browser-only SYNTHETIC harness for #82 (SyntheticTileStream +
// startTileTestHarness, activated via `?tile-test=1` OR
// `localStorage.tileTest='1'`) was relocated verbatim to
// static/tile-test-harness.js and is injected on demand by the loader
// at the bottom of this file — see that file's header for what it
// exercises and the window-global glue contract.
//
// See docs/design-tile-streaming.md for the full architecture.
// =====================================================================

const TILE_WIRE_VERSION = 0x01;
const TILE_FRAME_SNAPSHOT_CHUNK = 0x01;
const TILE_FRAME_TILE_UPDATE = 0x02;
const TILE_FRAME_RESIZE = 0x03;
const TILE_FRAME_EPOCH_ADVANCE = 0x04;
const TILE_FRAME_FALLBACK_TO_VIDEO = 0x05;
const TILE_FRAME_FALLBACK_TO_TILE = 0x06;
const TILE_FRAME_CURSOR_STATE = 0x07;
const TILE_FRAME_SUBSCRIBE = 0x10;
const TILE_FRAME_SNAPSHOT_REQUEST = 0x11;
const TILE_FRAME_GAP_REPORT = 0x12;
const TILE_FRAME_ERROR = 0xff;
const TILE_SNAPSHOT_REASON_STARTUP = 0;
const TILE_SNAPSHOT_REASON_RESIZE = 1;
const TILE_SNAPSHOT_REASON_GAP = 2;
const TILE_SNAPSHOT_REASON_MANUAL = 3;
const TILE_ENCODING_RAW_BGRA = 0;
const TILE_ENCODING_RLE_BGRA = 1;
const TILE_ENCODING_WEBP_LOSSLESS = 2;

class TileWireReader {
  constructor(bytes) {
    if (bytes instanceof ArrayBuffer) {
      this.bytes = new Uint8Array(bytes);
    } else if (ArrayBuffer.isView(bytes)) {
      this.bytes = new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    } else {
      throw new Error('tile wire frame must be ArrayBuffer or typed-array view');
    }
    this.view = new DataView(this.bytes.buffer, this.bytes.byteOffset, this.bytes.byteLength);
    this.pos = 0;
  }

  remaining() {
    return this.bytes.length - this.pos;
  }

  take(n) {
    if (this.remaining() < n) throw new Error('tile wire frame truncated');
    const out = this.bytes.subarray(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }

  u8() {
    return this.take(1)[0];
  }

  u16() {
    if (this.remaining() < 2) throw new Error('tile wire frame truncated');
    const v = this.view.getUint16(this.pos, true);
    this.pos += 2;
    return v;
  }

  u32() {
    if (this.remaining() < 4) throw new Error('tile wire frame truncated');
    const v = this.view.getUint32(this.pos, true);
    this.pos += 4;
    return v;
  }

  i32() {
    if (this.remaining() < 4) throw new Error('tile wire frame truncated');
    const v = this.view.getInt32(this.pos, true);
    this.pos += 4;
    return v;
  }

  records(count) {
    const records = [];
    for (let i = 0; i < count; i++) {
      const tile_x = this.u16();
      const tile_y = this.u16();
      const encoding = this.u8();
      const payloadLen = this.u32();
      if (
        encoding !== TILE_ENCODING_RAW_BGRA &&
        encoding !== TILE_ENCODING_RLE_BGRA &&
        encoding !== TILE_ENCODING_WEBP_LOSSLESS
      ) {
        throw new Error(`unsupported tile encoding ${encoding}`);
      }
      records.push({
        tile_x,
        tile_y,
        encoding,
        payload: this.take(payloadLen),
      });
    }
    return records;
  }

  finish() {
    if (this.remaining() !== 0) {
      throw new Error(`tile wire frame has ${this.remaining()} trailing bytes`);
    }
  }
}

function parseTileWireFrame(bytes) {
  const r = new TileWireReader(bytes);
  const version = r.u8();
  if (version !== TILE_WIRE_VERSION) {
    throw new Error(`unsupported tile wire version ${version}`);
  }
  const frameType = r.u8();
  r.u16(); // flags, reserved for v1
  let frame;
  switch (frameType) {
    case TILE_FRAME_SNAPSHOT_CHUNK:
      frame = {
        type: 'snapshot_chunk',
        epoch: r.u32(),
        snapshot_id: r.u32(),
        chunk_index: r.u16(),
        chunk_count: r.u16(),
        grid_w_tiles: r.u16(),
        grid_h_tiles: r.u16(),
        tile_size_px: r.u16(),
      };
      frame.records = r.records(r.u32());
      break;
    case TILE_FRAME_TILE_UPDATE:
      frame = {
        type: 'tile_update',
        epoch: r.u32(),
        seq: r.u32(),
      };
      frame.records = r.records(r.u16());
      break;
    case TILE_FRAME_RESIZE:
      frame = {
        type: 'resize',
        new_epoch: r.u32(),
        grid_w_tiles: r.u16(),
        grid_h_tiles: r.u16(),
        tile_size_px: r.u16(),
      };
      break;
    case TILE_FRAME_EPOCH_ADVANCE:
      frame = { type: 'epoch_advance', new_epoch: r.u32() };
      break;
    case TILE_FRAME_FALLBACK_TO_VIDEO:
      frame = { type: 'fallback_to_video', new_epoch: r.u32() };
      break;
    case TILE_FRAME_FALLBACK_TO_TILE:
      frame = { type: 'fallback_to_tile', new_epoch: r.u32() };
      break;
    case TILE_FRAME_CURSOR_STATE:
      frame = {
        type: 'cursor_state',
        epoch: r.u32(),
        seq: r.u32(),
        x_px: r.i32(),
        y_px: r.i32(),
        visible: r.u8() !== 0,
      };
      break;
    case TILE_FRAME_SUBSCRIBE:
      frame = { type: 'subscribe', client_id: r.u32() };
      break;
    case TILE_FRAME_SNAPSHOT_REQUEST:
      frame = { type: 'snapshot_request', epoch: r.u32(), reason: r.u8() };
      break;
    case TILE_FRAME_GAP_REPORT:
      frame = {
        type: 'gap_report',
        epoch: r.u32(),
        last_seen_seq: r.u32(),
        expected_seq: r.u32(),
      };
      break;
    case TILE_FRAME_ERROR: {
      const code = r.u16();
      const msgLen = r.u16();
      frame = {
        type: 'error',
        code,
        message: new TextDecoder().decode(r.take(msgLen)),
      };
      break;
    }
    default:
      throw new Error(`unsupported tile wire frame type 0x${frameType.toString(16)}`);
  }
  r.finish();
  return frame;
}

function encodeTileSubscribeFrame(clientId) {
  const buf = new ArrayBuffer(8);
  const v = new DataView(buf);
  v.setUint8(0, TILE_WIRE_VERSION);
  v.setUint8(1, TILE_FRAME_SUBSCRIBE);
  v.setUint16(2, 0, true); // flags
  v.setUint32(4, clientId >>> 0, true);
  return buf;
}

function encodeTileSnapshotRequestFrame(epoch, reason) {
  const buf = new ArrayBuffer(9);
  const v = new DataView(buf);
  v.setUint8(0, TILE_WIRE_VERSION);
  v.setUint8(1, TILE_FRAME_SNAPSHOT_REQUEST);
  v.setUint16(2, 0, true); // flags
  v.setUint32(4, epoch >>> 0, true);
  v.setUint8(8, reason & 0xff);
  return buf;
}

function encodeTileGapReportFrame(epoch, lastSeenSeq, expectedSeq) {
  const buf = new ArrayBuffer(16);
  const v = new DataView(buf);
  v.setUint8(0, TILE_WIRE_VERSION);
  v.setUint8(1, TILE_FRAME_GAP_REPORT);
  v.setUint16(2, 0, true); // flags
  v.setUint32(4, epoch >>> 0, true);
  v.setUint32(8, lastSeenSeq >>> 0, true);
  v.setUint32(12, expectedSeq >>> 0, true);
  return buf;
}

class TileCompositor {
  constructor(container, { tileSize, gridW, gridH, sendControlFrame = null }) {
    this.container = container;
    this.frameEl = document.createElement('div');
    this.frameEl.className = 'tile-compositor-frame';
    this.canvas = document.createElement('canvas');
    this.canvas.className = 'tile-compositor-canvas';
    this.canvas.style.cssText =
      'display:block; image-rendering:pixelated; background:#222;';
    this.canvas.width = tileSize * gridW;
    this.canvas.height = tileSize * gridH;
    this.cursorEl = document.createElement('div');
    this.cursorEl.className = 'tile-compositor-cursor';
    this.frameEl.appendChild(this.canvas);
    this.frameEl.appendChild(this.cursorEl);
    const controls = container.querySelector && container.querySelector('.peer-display-controls');
    if (controls) {
      container.insertBefore(this.frameEl, controls);
    } else {
      container.appendChild(this.frameEl);
    }
    this.ctx = this.canvas.getContext('2d', { alpha: false, willReadFrequently: true });
    this.tileSize = tileSize;
    this.gridW = gridW;
    this.gridH = gridH;
    this.epoch = 0;
    this.lastSeenSeq = null;
    this.lastGapReportKey = null;
    this.sendControlFrame =
      typeof sendControlFrame === 'function' ? sendControlFrame : null;
    // tileMap key: (tile_y << 16) | tile_x ; value: { epoch, seq }
    this.tileMap = new Map();
    // snapshotChunkBuffers key: snapshot_id ; value: { chunks Map, expected, ... }
    this.snapshotChunkBuffers = new Map();
    this.lastAppliedSnapshotId = -1;
    this.metrics = {
      snapshotsApplied: 0,
      tileUpdatesApplied: 0,
      tilesApplied: 0,
      tilesDroppedStaleEpoch: 0,
      tilesDroppedStaleSeq: 0,
      gapDetections: 0,
      gapReportsSent: 0,
      snapshotRequestsSent: 0,
      resizes: 0,
    };
  }

  onWireFrame(bytes) {
    const frame = parseTileWireFrame(bytes);
    switch (frame.type) {
      case 'snapshot_chunk':
        this.onSnapshotChunk(frame);
        break;
      case 'tile_update':
        this.onTileUpdate(frame);
        break;
      case 'resize':
        this.onResize(frame);
        break;
      case 'epoch_advance':
        this.epoch = frame.new_epoch;
        this.lastSeenSeq = null;
        this.tileMap.clear();
        this._sendSnapshotRequest(TILE_SNAPSHOT_REASON_GAP);
        break;
      case 'fallback_to_video':
        this.onFallbackToVideo(frame);
        break;
      case 'fallback_to_tile':
        this.onFallbackToTile(frame);
        break;
      case 'subscribe':
      case 'snapshot_request':
      case 'gap_report':
      case 'error':
        // Parsed here so the channel remains forward-compatible with
        // recovery/control messages that are handled by later slices.
        break;
      case 'cursor_state':
        this.onCursorState(frame);
        break;
      default:
        throw new Error(`unhandled tile wire frame ${frame.type}`);
    }
    return frame;
  }

  onSnapshotChunk(frame) {
    let buf = this.snapshotChunkBuffers.get(frame.snapshot_id);
    if (!buf) {
      buf = {
        chunks: new Map(),
        expected: frame.chunk_count,
        epoch: frame.epoch,
        gridW: frame.grid_w_tiles ?? this.gridW,
        gridH: frame.grid_h_tiles ?? this.gridH,
        tileSize: frame.tile_size_px ?? this.tileSize,
      };
      this.snapshotChunkBuffers.set(frame.snapshot_id, buf);
    }
    buf.chunks.set(frame.chunk_index, frame.records);
    if (buf.chunks.size === buf.expected) {
      this._applySnapshot(frame.snapshot_id, buf);
    }
  }

  _applySnapshot(snapshotId, buf) {
    if (snapshotId <= this.lastAppliedSnapshotId) {
      // Already-applied snapshot (e.g. duplicate after recovery). Drop.
      this.snapshotChunkBuffers.delete(snapshotId);
      return;
    }
    if (
      buf.gridW !== this.gridW ||
      buf.gridH !== this.gridH ||
      buf.tileSize !== this.tileSize
    ) {
      this.canvas.width = buf.tileSize * buf.gridW;
      this.canvas.height = buf.tileSize * buf.gridH;
      this.gridW = buf.gridW;
      this.gridH = buf.gridH;
      this.tileSize = buf.tileSize;
    }
    this.ctx.fillStyle = '#222';
    this.ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    this.epoch = buf.epoch;
    this.lastSeenSeq = null;
    this.tileMap.clear();
    // Apply chunks in chunk_index order so tile-record dependencies
    // (none today, but design preserves ordering) are deterministic.
    const indices = [...buf.chunks.keys()].sort((a, b) => a - b);
    for (const idx of indices) {
      for (const r of buf.chunks.get(idx)) {
        this._applyRecord(r, buf.epoch, 0);
      }
    }
    this.lastAppliedSnapshotId = snapshotId;
    this.snapshotChunkBuffers.delete(snapshotId);
    this.metrics.snapshotsApplied++;
  }

  onTileUpdate(frame) {
    if (frame.epoch < this.epoch) {
      this.metrics.tilesDroppedStaleEpoch += frame.records.length;
      return;
    }
    if (frame.epoch > this.epoch) {
      this.metrics.gapDetections++;
      this._sendSnapshotRequest(TILE_SNAPSHOT_REASON_GAP);
      console.warn(
        '[tile-compositor] TileUpdate from epoch',
        frame.epoch,
        '> current',
        this.epoch,
        '— dropping; expecting snapshot',
      );
      return;
    }
    if (this.lastSeenSeq !== null && frame.seq > this.lastSeenSeq + 1) {
      this.metrics.gapDetections++;
      this._sendGapReport(this.lastSeenSeq, frame.seq);
    }
    if (this.lastSeenSeq === null || frame.seq > this.lastSeenSeq) {
      this.lastSeenSeq = frame.seq;
    }
    for (const r of frame.records) {
      const key = (r.tile_y << 16) | r.tile_x;
      const prev = this.tileMap.get(key);
      if (prev && prev.seq >= frame.seq) {
        this.metrics.tilesDroppedStaleSeq++;
        continue;
      }
      this._applyRecord(r, frame.epoch, frame.seq);
    }
    this.metrics.tileUpdatesApplied++;
  }

  onResize({ new_epoch, grid_w_tiles, grid_h_tiles, tile_size_px }) {
    this.canvas.width = tile_size_px * grid_w_tiles;
    this.canvas.height = tile_size_px * grid_h_tiles;
    this.gridW = grid_w_tiles;
    this.gridH = grid_h_tiles;
    this.tileSize = tile_size_px;
    this.epoch = new_epoch;
    this.lastSeenSeq = null;
    this.tileMap.clear();
    this.ctx.fillStyle = '#222';
    this.ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    this.metrics.resizes++;
    this._sendSnapshotRequest(TILE_SNAPSHOT_REASON_RESIZE);
  }

  onFallbackToVideo({ new_epoch }) {
    if (new_epoch >= this.epoch) {
      this.epoch = new_epoch;
      this.lastSeenSeq = null;
      this.tileMap.clear();
      this.snapshotChunkBuffers.clear();
    }
    if (this.cursorEl) this.cursorEl.style.display = 'none';
    this._showVideoSurface();
  }

  onFallbackToTile({ new_epoch }) {
    if (new_epoch >= this.epoch) {
      this.epoch = new_epoch;
      this.lastSeenSeq = null;
      this.tileMap.clear();
      this.snapshotChunkBuffers.clear();
    }
    this.ctx.fillStyle = '#222';
    this.ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    this._showTileSurface();
  }

  _sendSnapshotRequest(reason) {
    if (!this.sendControlFrame) return;
    this.metrics.snapshotRequestsSent++;
    this.sendControlFrame(encodeTileSnapshotRequestFrame(this.epoch, reason));
  }

  _sendGapReport(lastSeenSeq, expectedSeq) {
    if (!this.sendControlFrame) return;
    const key = `${this.epoch}:${lastSeenSeq}:${expectedSeq}`;
    if (key === this.lastGapReportKey) return;
    this.lastGapReportKey = key;
    this.metrics.gapReportsSent++;
    this.sendControlFrame(
      encodeTileGapReportFrame(this.epoch, lastSeenSeq, expectedSeq),
    );
  }

  onCursorState(frame) {
    if (!this.cursorEl) return;
    if (frame.epoch < this.epoch) return;
    if (!frame.visible) {
      this.cursorEl.style.display = 'none';
      return;
    }
    const width = this.canvas.width || 1;
    const height = this.canvas.height || 1;
    const x = Math.max(0, Math.min(frame.x_px / width, 1));
    const y = Math.max(0, Math.min(frame.y_px / height, 1));
    this.cursorEl.style.left = `${x * 100}%`;
    this.cursorEl.style.top = `${y * 100}%`;
    this.cursorEl.style.display = 'block';
  }

  _videoElement() {
    return this.container && this.container.querySelector
      ? this.container.querySelector('.peer-display-video')
      : null;
  }

  _showVideoSurface() {
    const video = this._videoElement();
    if (video) video.style.display = '';
    this.frameEl.style.display = 'none';
  }

  _showTileSurface() {
    const video = this._videoElement();
    if (video) video.style.display = 'none';
    this.frameEl.style.display = '';
  }

  _applyRecord(r, epoch, seq) {
    const px = r.tile_x * this.tileSize;
    const py = r.tile_y * this.tileSize;
    if (r.encoding === TILE_ENCODING_RAW_BGRA) {
      this._drawImageDataRecord(r, epoch, seq, this._decodeRawBgra(r.payload), px, py);
    } else if (r.encoding === TILE_ENCODING_RLE_BGRA) {
      this._drawImageDataRecord(r, epoch, seq, this._decodeRleBgra(r.payload), px, py);
    } else if (r.encoding === TILE_ENCODING_WEBP_LOSSLESS) {
      this._decodeWebpLossless(r.payload)
        .then((decoded) => {
          this._drawBitmapRecord(r, epoch, seq, decoded, px, py);
        })
        .catch((err) => {
          console.warn('[tile-compositor] WebP tile decode failed', err);
        });
    } else {
      console.warn('[tile-compositor] unknown encoding', r.encoding);
    }
  }

  _recordIsStale(r, epoch, seq) {
    if (epoch < this.epoch) return true;
    const prev = this.tileMap.get((r.tile_y << 16) | r.tile_x);
    return !!(prev && prev.epoch === epoch && prev.seq >= seq);
  }

  _drawImageDataRecord(r, epoch, seq, imageData, px, py) {
    if (this._recordIsStale(r, epoch, seq)) return;
    this.ctx.putImageData(imageData, px, py);
    this.tileMap.set((r.tile_y << 16) | r.tile_x, { epoch, seq });
    this.metrics.tilesApplied++;
  }

  _drawBitmapRecord(r, epoch, seq, decoded, px, py) {
    try {
      if (this._recordIsStale(r, epoch, seq)) return;
      this.ctx.drawImage(decoded, px, py, this.tileSize, this.tileSize);
      this.tileMap.set((r.tile_y << 16) | r.tile_x, { epoch, seq });
    } finally {
      if (decoded && typeof decoded.close === 'function') decoded.close();
    }
    this.metrics.tilesApplied++;
  }

  // raw_bgra: payload is `tileSize * tileSize * 4` BGRA bytes.
  // Browser canvas wants RGBA, so swap B↔R per pixel into a fresh buffer.
  _decodeRawBgra(payload) {
    const ts = this.tileSize;
    const expected = ts * ts * 4;
    if (payload.length !== expected) {
      throw new Error(
        `tile payload length ${payload.length} != expected ${expected} for ${ts}x${ts} BGRA`,
      );
    }
    const out = new Uint8ClampedArray(expected);
    for (let i = 0; i < payload.length; i += 4) {
      out[i] = payload[i + 2];     // R from B
      out[i + 1] = payload[i + 1]; // G
      out[i + 2] = payload[i];     // B from R
      out[i + 3] = payload[i + 3]; // A
    }
    return new ImageData(out, ts, ts);
  }

  // rle_bgra: payload is a sequence of `[B, G, R, A, run_length]` records.
  // run_length=0 is illegal (treated as 1 to avoid infinite loops on
  // garbage). Decoder fills the tile in row-major order.
  _decodeRleBgra(payload) {
    const ts = this.tileSize;
    const dst = new Uint8ClampedArray(ts * ts * 4);
    let dpos = 0;
    let spos = 0;
    while (spos + 5 <= payload.length && dpos < dst.length) {
      const b = payload[spos];
      const g = payload[spos + 1];
      const r = payload[spos + 2];
      const a = payload[spos + 3];
      const run = payload[spos + 4] || 1;
      spos += 5;
      for (let k = 0; k < run && dpos < dst.length; k++) {
        dst[dpos] = r;
        dst[dpos + 1] = g;
        dst[dpos + 2] = b;
        dst[dpos + 3] = a;
        dpos += 4;
      }
    }
    return new ImageData(dst, ts, ts);
  }

  _decodeWebpLossless(payload) {
    const blob = new Blob([payload], { type: 'image/webp' });
    if (typeof createImageBitmap === 'function') {
      return createImageBitmap(blob);
    }
    return new Promise((resolve, reject) => {
      const img = new Image();
      const url = URL.createObjectURL(blob);
      img.onload = () => {
        URL.revokeObjectURL(url);
        resolve(img);
      };
      img.onerror = () => {
        URL.revokeObjectURL(url);
        reject(new Error('image/webp decode failed'));
      };
      img.src = url;
    });
  }
}
