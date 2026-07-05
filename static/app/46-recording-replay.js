// ── Recording Replay ──

/** Parse MP4 box structure from an ArrayBuffer. Returns [{type, offset, size}]. */
function parseMp4Boxes(buffer) {
  const view = new DataView(buffer);
  const boxes = [];
  let offset = 0;
  while (offset + 8 <= buffer.byteLength) {
    let size = view.getUint32(offset);
    const type = String.fromCharCode(
      view.getUint8(offset + 4), view.getUint8(offset + 5),
      view.getUint8(offset + 6), view.getUint8(offset + 7)
    );
    // Handle extended size (size == 1 → 64-bit size in next 8 bytes)
    if (size === 1 && offset + 16 <= buffer.byteLength) {
      const hi = view.getUint32(offset + 8);
      const lo = view.getUint32(offset + 12);
      size = hi * 0x100000000 + lo;
    }
    if (size < 8) break; // Invalid box
    boxes.push({ type, offset, size });
    offset += size;
  }
  return boxes;
}

/** Extract and concatenate specific MP4 boxes by type from an ArrayBuffer. */
function extractMp4Boxes(buffer, types) {
  const boxes = parseMp4Boxes(buffer);
  const matched = boxes.filter(b => types.includes(b.type));
  if (matched.length === 0) return new ArrayBuffer(0);
  let totalSize = 0;
  for (const b of matched) totalSize += b.size;
  const result = new Uint8Array(totalSize);
  let pos = 0;
  for (const b of matched) {
    result.set(new Uint8Array(buffer, b.offset, b.size), pos);
    pos += b.size;
  }
  return result.buffer;
}

class RecordingPlayer {
  constructor(videoEl, timelineEl, cursorEl, progressEl, timeLabel, playBtn, baseUrl) {
    this.video = videoEl;
    this.timelineEl = timelineEl;
    this.cursorEl = cursorEl;
    this.baseUrl = baseUrl || '/recordings';
    this.progressEl = progressEl;
    this.timeLabel = timeLabel;
    this.playBtn = playBtn;
    this.segments = [];
    this.totalDuration = 0;
    this.currentSegIdx = -1;
    this.playing = false;
    this.rafId = null;
    this.streamName = null;
    this.useMSE = false;
    this._msObjectUrl = null;
    this._mediaSource = null;
    this._mseReady = false;
    this._mseAbort = null;
    this._mseGeneration = 0;
    this._segmentObjectUrl = null;
    this._segmentLoadGeneration = 0;
    this._hlsObjectUrls = [];
    this._hlsFallbackHandler = null;
    this._hlsLoadGeneration = 0;

    // Bound handlers — stored so destroy() can remove them
    this._onEnded = () => this._onSegmentEnd();
    this._onTimeUpdate = () => this._updateDisplay();
    this._onTimelineClick = (e) => {
      const rect = this.timelineEl.getBoundingClientRect();
      const pct = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
      this.seekToGlobal(pct * this.totalDuration);
    };
    this._onPlayClick = () => this.togglePlayback();

    this.video.addEventListener('ended', this._onEnded);
    this.video.addEventListener('timeupdate', this._onTimeUpdate);
    this.timelineEl.addEventListener('click', this._onTimelineClick);
    playBtn.addEventListener('click', this._onPlayClick);
  }

  _assetRpcTarget() {
    if (!dashboardTransport?.canUseRpc?.()) return null;
    if (dashboardControlTransport?.lastStatus?.byte_streams_available !== true) return null;
    if (this.baseUrl === '/recordings') {
      if (dashboardControlTransport?.lastStatus?.api_recording_asset_available !== true) return null;
      return { method: 'api_recording_asset', params: {} };
    }
    const match = String(this.baseUrl || '').match(/^\/api\/session\/([^/]+)\/recordings$/);
    if (!match) return null;
    if (dashboardControlTransport?.lastStatus?.api_session_recording_asset_available !== true) return null;
    return {
      method: 'api_session_recording_asset',
      params: { session_id: decodeURIComponent(match[1]) },
    };
  }

  _assetTransferArtifact(asset) {
    if (!this.streamName) return null;
    if (this.baseUrl === '/recordings') {
      return {
        type: 'recording_asset',
        stream_name: this.streamName,
        asset,
      };
    }
    const match = String(this.baseUrl || '').match(/^\/api\/session\/([^/]+)\/recordings$/);
    if (!match) return null;
    return {
      type: 'session_recording_asset',
      session_id: decodeURIComponent(match[1]),
      stream_name: this.streamName,
      asset,
    };
  }

  async _requestAssetBytes(asset, options = {}) {
    const artifact = this._assetTransferArtifact(asset);
    if (artifact) {
      try {
        const transferResult = await dashboardFetchTransferArtifactBytes(artifact, {
          timeoutMs: options.timeoutMs || 120000,
          signal: options.signal,
          chunkBytes: options.chunkBytes,
          maxBytes: options.maxBytes,
        });
        if (transferResult?.blob) {
          return new Uint8Array(await transferResult.blob.arrayBuffer());
        }
      } catch (err) {
        console.warn('[dashboard-control] recording asset transfer failed', err);
      }
    }
    const target = this._assetRpcTarget();
    if (!target || !this.streamName) return null;
    const result = await dashboardTransport.requestBytes(target.method, {
      ...target.params,
      stream_name: this.streamName,
      asset,
    }, { timeoutMs: options.timeoutMs || 120000 });
    if (result?.ok === false || result?._httpOk === false) {
      throw new Error(result.error || `Recording asset failed (${result._httpStatus || 'error'})`);
    }
    if (result?.bytes instanceof Uint8Array) return result.bytes;
    if (result?.data_base64) return dashboardControlBase64ToBytes(result.data_base64);
    return null;
  }

  async _loadSegmentsList() {
    try {
      const bytes = await this._requestAssetBytes('segments', { timeoutMs: 60000 });
      if (bytes) return JSON.parse(new TextDecoder().decode(bytes));
    } catch (err) {
      console.warn('[dashboard-control] recording segments RPC failed', err);
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error('Recording segments are unavailable until dashboard access reconnects');
    }
    const resp = await fetch(`${this.baseUrl}/${this.streamName}/segments`);
    return resp.json();
  }

  async _fetchSegmentArrayBuffer(filename, fetchOptions = {}) {
    if (String(filename || '').endsWith('.mp4')) {
      try {
        const bytes = await this._requestAssetBytes(filename, { timeoutMs: 120000 });
        if (bytes) return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
      } catch (err) {
        console.warn('[dashboard-control] recording segment RPC failed', err);
      }
    }
    if (dashboardConnectModeEnabled()) {
      throw new Error('Recording segment bytes are unavailable until dashboard access reconnects');
    }
    const url = `${this.baseUrl}/${this.streamName}/${filename}`;
    const resp = await fetch(url, { cache: 'no-store', ...fetchOptions });
    return resp.arrayBuffer();
  }

  _revokeSegmentObjectUrl() {
    if (!this._segmentObjectUrl) return;
    try { URL.revokeObjectURL(this._segmentObjectUrl); } catch (_) {}
    this._segmentObjectUrl = null;
  }

  _clearHlsFallbackHandler() {
    if (!this._hlsFallbackHandler) return;
    this.video.removeEventListener('error', this._hlsFallbackHandler);
    this._hlsFallbackHandler = null;
  }

  _revokeHlsObjectUrls() {
    this._clearHlsFallbackHandler();
    for (const url of this._hlsObjectUrls) {
      try { URL.revokeObjectURL(url); } catch (_) {}
    }
    this._hlsObjectUrls = [];
  }

  _setVideoSrc(src, options = {}) {
    const objectUrl = String(options.objectUrl || '').trim();
    const hlsObjectUrls = Array.isArray(options.hlsObjectUrls) ? options.hlsObjectUrls : null;
    if (this._segmentObjectUrl && this._segmentObjectUrl !== objectUrl) {
      this._revokeSegmentObjectUrl();
    }
    this._segmentObjectUrl = objectUrl || null;
    if (hlsObjectUrls) {
      this._clearHlsFallbackHandler();
      this._hlsObjectUrls = hlsObjectUrls;
    } else {
      this._revokeHlsObjectUrls();
    }
    this.video.src = src;
  }

  _installHlsFallback(primarySrc, fallbackSrc, generation) {
    if (!fallbackSrc) return;
    if (dashboardConnectModeEnabled()) return;
    this._clearHlsFallbackHandler();
    const handler = () => {
      if (this._hlsLoadGeneration !== generation) return;
      const currentSrc = this.video.currentSrc || this.video.src || '';
      if (currentSrc && currentSrc !== primarySrc && !this._hlsObjectUrls.includes(currentSrc)) return;
      console.warn('[dashboard-control] HLS blob playlist failed; using HTTP playlist fallback');
      this._hlsLoadGeneration++;
      this._setVideoSrc(fallbackSrc);
    };
    this._hlsFallbackHandler = handler;
    this.video.addEventListener('error', handler, { once: true });
  }

  async _loadHlsBlobPlaylist(fallbackSrc = '') {
    const target = this._assetRpcTarget();
    if (!this.streamName || (!target && !dashboardTransferDownloadAvailable())) return false;
    const generation = ++this._hlsLoadGeneration;
    this._revokeHlsObjectUrls();
    const objectUrls = [];
    try {
      const playlistBytes = await this._requestAssetBytes('playlist.m3u8', { timeoutMs: 60000 });
      if (!playlistBytes) return false;
      const playlistText = new TextDecoder().decode(playlistBytes);
      const rewritten = [];
      for (const line of playlistText.split(/\r?\n/)) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith('#')) {
          rewritten.push(line);
          continue;
        }
        if (!/^seg_[A-Za-z0-9_.-]+\.ts$/.test(trimmed)) {
          rewritten.push(line);
          continue;
        }
        const segmentBytes = await this._requestAssetBytes(trimmed, { timeoutMs: 120000 });
        if (!segmentBytes) return false;
        const segmentUrl = URL.createObjectURL(new Blob([segmentBytes], { type: 'video/mp2t' }));
        objectUrls.push(segmentUrl);
        rewritten.push(segmentUrl);
      }
      const playlistUrl = URL.createObjectURL(new Blob([rewritten.join('\n')], {
        type: 'application/vnd.apple.mpegurl',
      }));
      objectUrls.push(playlistUrl);
      if (this._hlsLoadGeneration !== generation) {
        for (const url of objectUrls) {
          try { URL.revokeObjectURL(url); } catch (_) {}
        }
        return true;
      }
      this._setVideoSrc(playlistUrl, { hlsObjectUrls: objectUrls });
      this._installHlsFallback(playlistUrl, fallbackSrc, generation);
      return true;
    } catch (err) {
      for (const url of objectUrls) {
        try { URL.revokeObjectURL(url); } catch (_) {}
      }
      console.warn('[dashboard-control] HLS playlist RPC failed', err);
      return false;
    }
  }

  async load(streamName) {
    this.streamName = streamName;
    this.useMSE = false;
    this._mseReady = false;
    try {
      this.segments = await this._loadSegmentsList();
    } catch { this.segments = []; }
    this.totalDuration = this.segments.length > 0
      ? this.segments[this.segments.length - 1].end_secs
      : 0;
    this._renderSegmentMarks();
    this._updateDisplay();
    if (this.segments.length > 0) {
      // Use MSE for fMP4 segments if supported
      const isMp4 = this.segments[0].filename && this.segments[0].filename.endsWith('.mp4');
      const mseOk = window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E"');
      if (isMp4 && mseOk) {
        this._loadAllMSE();
      } else {
        this._loadSegment(0, 0);
      }
    }
  }

  async refresh() {
    if (!this.streamName) return;
    const oldLen = this.segments.length;
    try {
      this.segments = await this._loadSegmentsList();
    } catch { return; }
    const newTotal = this.segments.length > 0
      ? this.segments[this.segments.length - 1].end_secs
      : 0;
    if (newTotal !== this.totalDuration) {
      const prevTotal = this.totalDuration;
      this.totalDuration = newTotal;
      this._renderSegmentMarks();
      this._updateDisplay();
      if (this.useMSE && this._mseReady && this._mediaSource) {
        // Append new segments incrementally
        this._appendNewMSESegments(oldLen);
      } else if (oldLen === 0 && this.segments.length > 0) {
        // Auto-load first segment if we went from 0 to having segments
        const isMp4 = this.segments[0].filename && this.segments[0].filename.endsWith('.mp4');
        const mseOk = window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E"');
        if (isMp4 && mseOk) {
          this._loadAllMSE();
        } else {
          this._loadSegment(0, 0);
        }
      }
    }
  }

  async _appendNewMSESegments(fromIdx) {
    if (!this._mediaSource || this._mediaSource.readyState !== 'open') return;
    const sb = this._mediaSource.sourceBuffers[0];
    if (!sb) return;

    // Re-open the stream if it was ended
    if (this._mediaSource.readyState === 'ended') {
      // Cannot reopen ended MediaSource — skip incremental append
      return;
    }

    const appendAndWait = (data) => new Promise((resolve, reject) => {
      const onEnd = () => { sb.removeEventListener('updateend', onEnd); sb.removeEventListener('error', onErr); resolve(); };
      const onErr = () => { sb.removeEventListener('updateend', onEnd); sb.removeEventListener('error', onErr); reject(new Error('SourceBuffer error')); };
      sb.addEventListener('updateend', onEnd);
      sb.addEventListener('error', onErr);
      sb.appendBuffer(data);
    });

    for (let i = fromIdx; i < this.segments.length; i++) {
      try {
        const seg = this.segments[i];
        const buf = await this._fetchSegmentArrayBuffer(seg.filename);
        const mediaData = extractMp4Boxes(buf, ['moof', 'mdat']);
        if (mediaData.byteLength === 0) continue;
        sb.timestampOffset = seg.start_secs;
        await appendAndWait(mediaData);
      } catch { break; }
    }
  }

  togglePlayback() {
    if (this.playing) this.pause(); else this.play();
  }

  play() {
    if (this.totalDuration === 0) return;
    if (!this.useMSE && this.currentSegIdx < 0) {
      this._loadSegment(0, 0).catch(err => {
        console.warn('[dashboard-control] recording segment load failed', err);
      });
    }
    this.video.play().catch(() => {});
    this.playing = true;
    this.playBtn.textContent = '\u23F8';
    this._startAnimLoop();
  }

  pause() {
    this.video.pause();
    this.playing = false;
    this.playBtn.textContent = '\u25B6';
    this._stopAnimLoop();
  }

  seekToGlobal(secs) {
    secs = Math.max(0, Math.min(secs, this.totalDuration));
    if (this.useMSE) {
      this.video.currentTime = secs;
      this._updateDisplay();
      return;
    }
    const idx = this._findSegmentIndex(secs);
    if (idx < 0) return;
    const seg = this.segments[idx];
    const offset = secs - seg.start_secs;
    if (idx !== this.currentSegIdx) {
      this._loadSegment(idx, offset).catch(err => {
        console.warn('[dashboard-control] recording segment load failed', err);
      });
    } else {
      this.video.currentTime = offset;
    }
    this._updateDisplay();
  }

  /** Async seek — resolves when the frame at `secs` is ready to draw. */
  seekToGlobalAsync(secs) {
    return new Promise((resolve) => {
      secs = Math.max(0, Math.min(secs, this.totalDuration));

      if (this.useMSE) {
        const onSeeked = () => {
          this.video.removeEventListener('seeked', onSeeked);
          this._updateDisplay();
          resolve();
        };
        this.video.addEventListener('seeked', onSeeked);
        this.video.currentTime = secs;
        return;
      }

      const idx = this._findSegmentIndex(secs);
      if (idx < 0) { resolve(); return; }
      const seg = this.segments[idx];
      const offset = secs - seg.start_secs;

      const onSeeked = () => {
        this.video.removeEventListener('seeked', onSeeked);
        this._updateDisplay();
        resolve();
      };

      if (idx !== this.currentSegIdx) {
        const onLoaded = () => {
          this.video.removeEventListener('loadeddata', onLoaded);
          this.video.addEventListener('seeked', onSeeked);
          this.video.currentTime = offset;
        };
        this.video.addEventListener('loadeddata', onLoaded);
        this._loadSegment(idx, 0).catch(() => {
          this.video.removeEventListener('loadeddata', onLoaded);
          resolve();
        });
      } else {
        this.video.addEventListener('seeked', onSeeked);
        this.video.currentTime = offset;
      }
    });
  }

  setSpeed(rate) {
    this.video.playbackRate = rate;
  }

  globalTime() {
    if (this.useMSE) return this.video.currentTime || 0;
    if (this.currentSegIdx < 0 || this.currentSegIdx >= this.segments.length) return 0;
    return this.segments[this.currentSegIdx].start_secs + this.video.currentTime;
  }

  _findSegmentIndex(secs) {
    for (let i = 0; i < this.segments.length; i++) {
      if (secs >= this.segments[i].start_secs && secs < this.segments[i].end_secs) return i;
    }
    return this.segments.length > 0 ? this.segments.length - 1 : -1;
  }

  async _loadSegment(idx, offsetSecs) {
    if (idx < 0 || idx >= this.segments.length) return;
    const generation = ++this._segmentLoadGeneration;
    this.currentSegIdx = idx;
    const seg = this.segments[idx];
    // Native HLS wants a URL. Prefer a blob playlist built from tunneled bytes;
    // only daemon-origin dashboard pages can fall back to the daemon playlist.
    if (seg.filename.endsWith('.ts')) {
      const playlistUrl = dashboardConnectModeEnabled()
        ? ''
        : `${this.baseUrl}/${this.streamName}/playlist.m3u8`;
      const tunneled = await this._loadHlsBlobPlaylist(playlistUrl);
      if (!tunneled) this._setVideoSrc(playlistUrl);
    } else {
      try {
        const buf = await this._fetchSegmentArrayBuffer(seg.filename);
        if (generation !== this._segmentLoadGeneration) return;
        if (buf && buf.byteLength > 0) {
          const objectUrl = URL.createObjectURL(new Blob([buf], { type: 'video/mp4' }));
          this._setVideoSrc(objectUrl, { objectUrl });
        } else if (!dashboardConnectModeEnabled()) {
          this._setVideoSrc(`${this.baseUrl}/${this.streamName}/${seg.filename}`);
        }
      } catch (err) {
        if (generation !== this._segmentLoadGeneration) return;
        console.warn('[dashboard-control] recording segment byte-stream fallback failed', err);
        if (!dashboardConnectModeEnabled()) {
          this._setVideoSrc(`${this.baseUrl}/${this.streamName}/${seg.filename}`);
        }
      }
    }
    this.video.currentTime = offsetSecs || 0;
    if (this.playing) this.video.play().catch(() => {});
  }

  _onSegmentEnd() {
    if (this.useMSE) { this.pause(); return; }
    if (this.currentSegIdx + 1 < this.segments.length) {
      this._loadSegment(this.currentSegIdx + 1, 0).catch(err => {
        console.warn('[dashboard-control] recording segment load failed', err);
      });
    } else {
      this.pause();
    }
  }

  _updateDisplay() {
    const current = this.globalTime();
    const total = this.totalDuration;
    this.timeLabel.textContent = `${fmtTime(current)} / ${fmtTime(total)}`;
    if (total > 0) {
      const pct = (current / total) * 100;
      this.cursorEl.style.left = pct + '%';
      this.progressEl.style.width = pct + '%';
    }
  }

  _renderSegmentMarks() {
    // Remove old marks
    this.timelineEl.querySelectorAll('.timeline-segment-mark, .timeline-time-mark').forEach(el => el.remove());
    if (this.totalDuration === 0) return;
    // Add segment boundary markers (skip first)
    for (let i = 1; i < this.segments.length; i++) {
      const pct = (this.segments[i].start_secs / this.totalDuration) * 100;
      const mark = document.createElement('div');
      mark.className = 'timeline-segment-mark';
      mark.style.left = pct + '%';
      this.timelineEl.appendChild(mark);
    }
    // Add time markers at regular intervals
    const interval = this.totalDuration <= 30 ? 5 : this.totalDuration <= 120 ? 10 : 30;
    for (let t = interval; t < this.totalDuration; t += interval) {
      const pct = (t / this.totalDuration) * 100;
      const mark = document.createElement('div');
      mark.className = 'timeline-time-mark';
      mark.style.left = pct + '%';
      mark.dataset.time = fmtTime(t);
      this.timelineEl.appendChild(mark);
    }
  }

  _startAnimLoop() {
    if (this.rafId) return;
    const tick = () => {
      this._updateDisplay();
      this.rafId = requestAnimationFrame(tick);
    };
    this.rafId = requestAnimationFrame(tick);
  }

  _stopAnimLoop() {
    if (this.rafId) { cancelAnimationFrame(this.rafId); this.rafId = null; }
  }

  destroy() {
    this._stopAnimLoop();
    this.video.pause();
    // Remove event listeners to prevent stale handlers on the shared video element
    this.video.removeEventListener('ended', this._onEnded);
    this.video.removeEventListener('timeupdate', this._onTimeUpdate);
    this.timelineEl.removeEventListener('click', this._onTimelineClick);
    this.playBtn.removeEventListener('click', this._onPlayClick);
    // Abort any in-flight MSE loading
    this._mseGeneration++;
    if (this._mseAbort) {
      this._mseAbort.abort();
      this._mseAbort = null;
    }
    if (this._msObjectUrl) {
      URL.revokeObjectURL(this._msObjectUrl);
      this._msObjectUrl = null;
    }
    this._revokeSegmentObjectUrl();
    this._hlsLoadGeneration++;
    this._revokeHlsObjectUrls();
    this._mediaSource = null;
    this.useMSE = false;
    this._mseReady = false;
    // Remove any leftover loading overlay
    const overlay = this.video.parentElement?.querySelector('.mse-loading-overlay');
    if (overlay) overlay.remove();
    this.video.removeAttribute('src');
    this.video.load();
  }

  /** Load all fMP4 segments into MSE for instant seeking. */
  async _loadAllMSE() {
    const generation = ++this._mseGeneration;
    this._revokeSegmentObjectUrl();
    this._revokeHlsObjectUrls();
    const abortCtrl = new AbortController();
    this._mseAbort = abortCtrl;
    const signal = abortCtrl.signal;

    // Check if this load is still current
    const alive = () => this._mseGeneration === generation && !signal.aborted;

    this.useMSE = true;
    const ms = new MediaSource();
    this._mediaSource = ms;
    this._msObjectUrl = URL.createObjectURL(ms);
    this.video.src = this._msObjectUrl;

    // Show loading overlay
    const wrap = this.video.parentElement;
    const overlay = document.createElement('div');
    overlay.className = 'mse-loading-overlay';
    overlay.innerHTML = '<div class="mse-loading-inner"><div class="bar-track"><div class="bar-fill" id="_mse-bar"></div></div><span id="_mse-text">Loading segments...</span></div>';
    wrap.appendChild(overlay);

    try {
      await new Promise((r, rej) => {
        ms.addEventListener('sourceopen', r, { once: true });
        signal.addEventListener('abort', () => rej(new DOMException('Aborted', 'AbortError')), { once: true });
      });
      if (!alive()) return;

      const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.42E01E"');

      // Fetch all segments with controlled concurrency
      const total = this.segments.length;
      const buffers = new Array(total);
      let loaded = 0;
      const barEl = overlay.querySelector('#_mse-bar');
      const textEl = overlay.querySelector('#_mse-text');

      const fetchSeg = async (i) => {
        const seg = this.segments[i];
        buffers[i] = await this._fetchSegmentArrayBuffer(seg.filename, { signal });
        loaded++;
        if (barEl) {
          barEl.style.width = (loaded / total * 100) + '%';
          textEl.textContent = `Loading ${loaded}/${total} segments...`;
        }
      };

      // Parallel fetch with concurrency limit of 4
      const pool = [];
      for (let i = 0; i < total; i++) {
        if (!alive()) return;
        const p = fetchSeg(i);
        pool.push(p);
        if (pool.length >= 4) {
          await Promise.race(pool);
          for (let j = pool.length - 1; j >= 0; j--) {
            const status = await Promise.race([pool[j].then(() => 'done'), Promise.resolve('pending')]);
            if (status === 'done') pool.splice(j, 1);
          }
        }
      }
      await Promise.all(pool);
      if (!alive()) return;

      // Helper to append to SourceBuffer and wait for completion
      const appendAndWait = (data) => new Promise((resolve, reject) => {
        if (!alive()) { reject(new DOMException('Aborted', 'AbortError')); return; }
        const onEnd = () => { sb.removeEventListener('updateend', onEnd); sb.removeEventListener('error', onErr); resolve(); };
        const onErr = () => { sb.removeEventListener('updateend', onEnd); sb.removeEventListener('error', onErr); reject(new Error('SourceBuffer error')); };
        sb.addEventListener('updateend', onEnd);
        sb.addEventListener('error', onErr);
        sb.appendBuffer(data);
      });

      // Append init segment from first file (ftyp + moov boxes)
      const initData = extractMp4Boxes(buffers[0], ['ftyp', 'moov']);
      if (initData.byteLength > 0) {
        await appendAndWait(initData);
      }

      // Append media data from each segment with timestamp offset
      if (textEl) textEl.textContent = 'Buffering...';
      for (let i = 0; i < total; i++) {
        if (!alive()) return;
        const mediaData = extractMp4Boxes(buffers[i], ['moof', 'mdat']);
        if (mediaData.byteLength === 0) continue;
        sb.timestampOffset = this.segments[i].start_secs;
        await appendAndWait(mediaData);
        if (barEl) barEl.style.width = ((i + 1) / total * 100) + '%';
      }

      if (!alive()) return;
      ms.endOfStream();
      this._mseReady = true;

      // Clean up overlay
      overlay.remove();
      buffers.length = 0;
    } catch (e) {
      // Aborted or error — clean up overlay if still present
      if (overlay.parentElement) overlay.remove();
      if (e.name !== 'AbortError') console.warn('MSE load error:', e);
    }
  }
}

window.RecordingPlayer = RecordingPlayer;

function fmtTime(secs) {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return m + ':' + String(s).padStart(2, '0');
}

function _fmtBytes(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
  return (bytes / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
}

function _getOrCreateOptgroup(select, label) {
  for (const child of select.children) {
    if (child.tagName === 'OPTGROUP' && child.label === label) return child;
  }
  const group = document.createElement('optgroup');
  group.label = label;
  select.appendChild(group);
  return group;
}

function _streamGroup(name) {
  if (name.startsWith('display_')) return 'Displays';
  if (name.startsWith('cam')) return 'Cameras';
  return 'Other';
}

function _streamLabel(name) {
  // display_0 -> Display 0 #1, display_0_2 -> Display 0 #2
  if (name.startsWith('display_')) {
    const rest = name.slice(8);
    const m = rest.match(/^(\d+)_(\d+)$/);
    if (m) return 'Display ' + m[1] + ' #' + m[2];
    // First recording for this display — label as #1
    const m2 = rest.match(/^(\d+)$/);
    if (m2) return 'Display ' + m2[1] + ' #1';
    return 'Display ' + rest;
  }
  return name;
}

function _recordingBaseStream(name) {
  const m = String(name || '').match(/^(display_\d+)(?:_\d+)?$/);
  return m ? m[1] : String(name || '');
}

function slotForRecordingStream(streamName) {
  const base = _recordingBaseStream(streamName);
  return [...displaySlots.values()].find(s => 'display_' + s.displayId === base);
}

function resetRecordingReplayUi() {
  if (clipMode) exitClipMode();
  if (annotationMode) exitAnnotationMode();
  if (recPlayer) {
    recPlayer.destroy();
    recPlayer = null;
  }
  activeRecordingStream = null;
  const section = document.getElementById('recording-section');
  const select = document.getElementById('recording-stream-select');
  const timeline = document.getElementById('recording-timeline');
  section.classList.add('hidden');
  section.style.flex = '';
  section.style.minHeight = '';
  select.value = '';
  document.getElementById('displays-split-handle').style.display = 'none';
  document.getElementById('displays-collapse-bar').classList.add('hidden');
  document.getElementById('rec-status').textContent = '';
  document.getElementById('rec-time').textContent = '0:00 / 0:00';
  document.getElementById('rec-play-btn').textContent = '\u25B6';
  document.getElementById('timeline-cursor').style.left = '0%';
  document.getElementById('timeline-progress').style.width = '0%';
  timeline.querySelectorAll('.timeline-segment-mark, .timeline-time-mark').forEach(el => el.remove());
}

function removeRecordingOption(streamName) {
  const select = document.getElementById('recording-stream-select');
  const opt = [...select.querySelectorAll('option')].find(o => o.value === streamName);
  if (opt) {
    const parent = opt.parentElement;
    opt.remove();
    if (parent && parent.tagName === 'OPTGROUP' && parent.children.length === 0) parent.remove();
  }
}

function addRecordingStream(streamName) {
  if (recordingStreams.has(streamName)) return;
  recordingStreams.set(streamName, { active: true });

  const section = document.getElementById('recording-section');
  section.classList.remove('hidden');
  document.getElementById('displays-split-handle').style.display = '';
  document.getElementById('displays-collapse-bar').classList.remove('hidden');

  const select = document.getElementById('recording-stream-select');
  const group = _getOrCreateOptgroup(select, _streamGroup(streamName));
  const opt = document.createElement('option');
  opt.value = streamName;
  opt.textContent = _streamLabel(streamName);
  group.appendChild(opt);

  document.getElementById('rec-status').textContent = 'Recording';

  // If this is the first stream, auto-select it
  if (!activeRecordingStream) {
    select.value = streamName;
    switchRecordingStream(streamName);
  }
}

function removeRecordingStream(streamName) {
  const info = recordingStreams.get(streamName);
  if (info) info.active = false;
  document.getElementById('rec-status').textContent = 'Stopped';
  // Refresh segments after recording stops — the final segment is now available
  if (recPlayer && activeRecordingStream === streamName) {
    setTimeout(() => recPlayer.refresh(), 500);
  }
}

function deleteRecordingStream(streamName) {
  if (!recordingStreams.has(streamName) && activeRecordingStream !== streamName) return;
  recordingStreams.delete(streamName);
  removeRecordingOption(streamName);
  for (const slot of displaySlots.values()) {
    if (slot.recordingStreamName === streamName) {
      slot.recordingStreamName = null;
      slot.recording = false;
      slot.recordBtn.innerHTML = '&#x23FA; Record';
      slot.recordBtn.classList.remove('active');
      slot.deleteRecBtn.style.display = 'none';
    }
  }

  const select = document.getElementById('recording-stream-select');
  if (activeRecordingStream === streamName) {
    if (recPlayer) {
      recPlayer.destroy();
      recPlayer = null;
    }
    activeRecordingStream = null;
    const next = select.querySelector('option');
    if (next) {
      select.value = next.value;
      switchRecordingStream(next.value);
    }
  }

  if (recordingStreams.size === 0) {
    select.innerHTML = '';
    resetRecordingReplayUi();
  }
}

function switchRecordingStream(streamName) {
  // Exit clip/annotation mode when switching streams — state is per-recording
  if (clipMode) exitClipMode();
  if (annotationMode) exitAnnotationMode();

  activeRecordingStream = streamName;
  if (recPlayer) recPlayer.destroy();

  recPlayer = new RecordingPlayer(
    document.getElementById('recording-video'),
    document.getElementById('recording-timeline'),
    document.getElementById('timeline-cursor'),
    document.getElementById('timeline-progress'),
    document.getElementById('rec-time'),
    document.getElementById('rec-play-btn')
  );

  const speedSelect = document.getElementById('rec-speed');
  speedSelect.value = '1';
  speedSelect.onchange = () => recPlayer.setSpeed(parseFloat(speedSelect.value));

  recPlayer.load(streamName);

  // Periodically refresh segment list for active recordings
  if (recPlayer._refreshInterval) clearInterval(recPlayer._refreshInterval);
  recPlayer._refreshInterval = setInterval(() => {
    const info = recordingStreams.get(streamName);
    if (info && info.active) recPlayer.refresh();
  }, 5000);
}

// ── Displays collapse toggle ──
{
  let displaysCollapsed = false;
  let savedHeight = null;
  document.getElementById('displays-collapse-btn').addEventListener('click', () => {
    const container = document.getElementById('displays-container');
    const handle = document.getElementById('displays-split-handle');
    const btn = document.getElementById('displays-collapse-btn');
    displaysCollapsed = !displaysCollapsed;
    if (displaysCollapsed) {
      savedHeight = container.style.height || null;
      container.classList.add('collapsed');
      handle.style.display = 'none';
      btn.innerHTML = '&#x25BE; Displays';
      btn.title = 'Expand display viewer';
      // Give recording section full space
      document.getElementById('recording-section').style.flex = '1';
    } else {
      container.classList.remove('collapsed');
      if (savedHeight) container.style.height = savedHeight;
      else { container.style.flex = ''; container.style.height = ''; }
      handle.style.display = '';
      btn.innerHTML = '&#x25B4; Displays';
      btn.title = 'Collapse display viewer';
      document.getElementById('recording-section').style.flex = '';
    }
  });
}

// ── Displays split handle (drag to resize displays vs recording) ──
{
  const handle = document.getElementById('displays-split-handle');
  const tab = document.getElementById('tab-displays');
  const displays = document.getElementById('displays-container');
  const recording = document.getElementById('recording-section');
  let dragging = false;
  handle.addEventListener('mousedown', (e) => {
    dragging = true;
    handle.classList.add('dragging');
    document.body.style.cursor = 'row-resize';
    document.body.style.userSelect = 'none';
    // Prevent video element from capturing mouse during drag
    displays.style.pointerEvents = 'none';
    e.preventDefault();
  });
  document.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const tabRect = tab.getBoundingClientRect();
    const y = e.clientY - tabRect.top;
    const total = tabRect.height;
    const minPx = 100;
    const topH = Math.max(minPx, Math.min(total - minPx, y));
    displays.style.flex = 'none';
    displays.style.height = topH + 'px';
    recording.style.flex = '1';
    recording.style.minHeight = '0';
  });
  document.addEventListener('mouseup', () => {
    if (dragging) {
      dragging = false;
      handle.classList.remove('dragging');
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      displays.style.pointerEvents = '';
    }
  });
}

// Wire up stream selector
document.getElementById('recording-stream-select').addEventListener('change', (e) => {
  if (e.target.value) switchRecordingStream(e.target.value);
});

// Reconcile replay UI with the backend. Historical log replay can mention a
// recording that has since been deleted, so the disk/API list is authoritative.
async function reconcileRecordingStreams() {
  try {
    const resp = await dashboardJsonFetch('api_recordings', {}, () => fetch('/recordings'), 'api_recordings');
    const streams = await resp.json();
    const liveNames = new Set();
    for (const s of streams) {
      if (s.stream_name) liveNames.add(s.stream_name);
      if (s.stream_name && !recordingStreams.has(s.stream_name)) {
        addRecordingStream(s.stream_name);
      }
    }
    for (const name of [...recordingStreams.keys()]) {
      const info = recordingStreams.get(name);
      if (!liveNames.has(name) && (!info || !info.active)) {
        deleteRecordingStream(name);
      }
    }
  } catch { /* no recordings available */ }
}

