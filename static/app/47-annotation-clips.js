// ── Annotation Drawing ──
/** Draw an array of annotation shapes onto a canvas context.
 *  Optional scaleX/scaleY for rendering on a differently-sized canvas (e.g. thumbnails). */
function drawShapes(ctx, shapes, scaleX, scaleY) {
  scaleX = scaleX || 1;
  scaleY = scaleY || 1;
  for (const s of shapes) {
    ctx.save();
    ctx.strokeStyle = s.color || '#f38ba8';
    ctx.fillStyle = s.color || '#f38ba8';
    ctx.lineWidth = (s.thickness || 4) * scaleX;
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    if (s.type === 'freehand' && s.points && s.points.length > 1) {
      ctx.beginPath();
      ctx.moveTo(s.points[0][0] * scaleX, s.points[0][1] * scaleY);
      for (let i = 1; i < s.points.length; i++) ctx.lineTo(s.points[i][0] * scaleX, s.points[i][1] * scaleY);
      ctx.stroke();
    } else if (s.type === 'rect') {
      ctx.strokeRect(s.x * scaleX, s.y * scaleY, s.w * scaleX, s.h * scaleY);
    } else if (s.type === 'circle') {
      ctx.beginPath();
      ctx.arc(s.cx * scaleX, s.cy * scaleY, s.r * Math.min(scaleX, scaleY), 0, Math.PI * 2);
      ctx.stroke();
    } else if (s.type === 'arrow') {
      ctx.beginPath();
      ctx.moveTo(s.x1 * scaleX, s.y1 * scaleY);
      ctx.lineTo(s.x2 * scaleX, s.y2 * scaleY);
      ctx.stroke();
      const angle = Math.atan2((s.y2 - s.y1) * scaleY, (s.x2 - s.x1) * scaleX);
      const headLen = Math.max(12, (s.thickness || 4) * 3) * Math.min(scaleX, scaleY);
      ctx.beginPath();
      ctx.moveTo(s.x2 * scaleX, s.y2 * scaleY);
      ctx.lineTo(s.x2 * scaleX - headLen * Math.cos(angle - 0.4), s.y2 * scaleY - headLen * Math.sin(angle - 0.4));
      ctx.lineTo(s.x2 * scaleX - headLen * Math.cos(angle + 0.4), s.y2 * scaleY - headLen * Math.sin(angle + 0.4));
      ctx.closePath();
      ctx.fill();
    } else if (s.type === 'text') {
      const size = (s.size || 20) * Math.min(scaleX, scaleY);
      ctx.font = `${size}px sans-serif`;
      ctx.fillText(s.text, s.x * scaleX, s.y * scaleY);
    }
    ctx.restore();
  }
}

class AnnotationCanvas {
  constructor(overlayCanvas, sourceElement) {
    this.canvas = overlayCanvas;
    this.ctx = overlayCanvas.getContext('2d');
    this.source = sourceElement;
    this.shapes = [];
    this.undoStack = [];
    this.currentShape = null;
    this.tool = 'freehand';
    this.color = '#f38ba8';
    this.thickness = 4;
    this.active = false;
    this._onDown = (e) => this._pointerDown(e);
    this._onMove = (e) => this._pointerMove(e);
    this._onUp = (e) => this._pointerUp(e);
  }

  activate() {
    // Size canvas buffer to source resolution
    const src = this.source;
    const w = src.videoWidth || src.width;
    const h = src.videoHeight || src.height;
    if (w && h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    // Align overlay to the actual rendered video area (handle letterboxing)
    this._alignToSource();
    this.canvas.classList.add('annotation-active');
    this.canvas.addEventListener('pointerdown', this._onDown);
    this.canvas.addEventListener('pointermove', this._onMove);
    this.canvas.addEventListener('pointerup', this._onUp);
    this.active = true;
    this.render();
    this._prevTabIndex = this.canvas.getAttribute('tabindex');
    this.canvas.tabIndex = 0;
    try { this.canvas.focus({ preventScroll: true }); } catch (_) { this.canvas.focus(); }
  }

  deactivate() {
    this.canvas.classList.remove('annotation-active');
    this.canvas.removeEventListener('pointerdown', this._onDown);
    this.canvas.removeEventListener('pointermove', this._onMove);
    this.canvas.removeEventListener('pointerup', this._onUp);
    this.active = false;
    this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
    // Reset overlay to cover full parent
    this.canvas.style.position = '';
    this.canvas.style.left = '';
    this.canvas.style.top = '';
    this.canvas.style.width = '';
    this.canvas.style.height = '';
    if (this._prevTabIndex === null) this.canvas.removeAttribute('tabindex');
    else if (this._prevTabIndex !== undefined) this.canvas.setAttribute('tabindex', this._prevTabIndex);
    this._prevTabIndex = undefined;
  }

  _alignToSource() {
    // Position overlay exactly over the rendered video content within the wrapper
    const src = this.source;
    const wrap = this.canvas.parentElement;
    if (!wrap) return;
    const wrapRect = wrap.getBoundingClientRect();
    const srcRect = src.getBoundingClientRect();
    this.canvas.style.position = 'absolute';
    this.canvas.style.left = (srcRect.left - wrapRect.left) + 'px';
    this.canvas.style.top = (srcRect.top - wrapRect.top) + 'px';
    this.canvas.style.width = srcRect.width + 'px';
    this.canvas.style.height = srcRect.height + 'px';
  }

  _canvasCoords(e) {
    const rect = this.canvas.getBoundingClientRect();
    return [
      (e.clientX - rect.left) / rect.width * this.canvas.width,
      (e.clientY - rect.top) / rect.height * this.canvas.height
    ];
  }

  async _pointerDown(e) {
    e.preventDefault();
    this.canvas.setPointerCapture(e.pointerId);
    const [x, y] = this._canvasCoords(e);
    if (this.tool === 'text') {
      try { this.canvas.releasePointerCapture(e.pointerId); } catch (_) {}
      const text = await showDashboardPrompt({
        title: 'Annotation text',
        label: 'Text',
        rows: 2,
        submitLabel: 'Add text',
      });
      if (text) {
        this.shapes.push({ type: 'text', x, y, text, color: this.color, size: this.thickness * 5 });
        this.undoStack = [];
        this.render();
      }
      return;
    }
    if (this.tool === 'freehand') {
      this.currentShape = { type: 'freehand', points: [[x, y]], color: this.color, thickness: this.thickness };
    } else if (this.tool === 'rect') {
      this.currentShape = { type: 'rect', x, y, w: 0, h: 0, color: this.color, thickness: this.thickness };
    } else if (this.tool === 'circle') {
      this.currentShape = { type: 'circle', cx: x, cy: y, r: 0, color: this.color, thickness: this.thickness };
    } else if (this.tool === 'arrow') {
      this.currentShape = { type: 'arrow', x1: x, y1: y, x2: x, y2: y, color: this.color, thickness: this.thickness };
    }
  }

  _pointerMove(e) {
    if (!this.currentShape) return;
    const [x, y] = this._canvasCoords(e);
    const s = this.currentShape;
    if (s.type === 'freehand') {
      s.points.push([x, y]);
    } else if (s.type === 'rect') {
      s.w = x - s.x;
      s.h = y - s.y;
    } else if (s.type === 'circle') {
      s.r = Math.hypot(x - s.cx, y - s.cy);
    } else if (s.type === 'arrow') {
      s.x2 = x;
      s.y2 = y;
    }
    this.render();
    this._drawShape(this.ctx, s);
  }

  _pointerUp(e) {
    if (!this.currentShape) return;
    this.shapes.push(this.currentShape);
    this.currentShape = null;
    this.undoStack = [];
    this.render();
  }

  setTool(type) { this.tool = type; }
  setColor(color) { this.color = color; }
  setThickness(px) { this.thickness = px; }

  undo() {
    if (this.shapes.length === 0) return;
    this.undoStack.push(this.shapes.pop());
    this.render();
  }

  redo() {
    if (this.undoStack.length === 0) return;
    this.shapes.push(this.undoStack.pop());
    this.render();
  }

  clear() {
    this.shapes = [];
    this.undoStack = [];
    this.currentShape = null;
    this.render();
  }

  hasAnnotations() { return this.shapes.length > 0; }

  render() {
    this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
    drawShapes(this.ctx, this.shapes);
  }

  _drawShape(ctx, s) {
    drawShapes(ctx, [s]);
  }

  /** Composite source frame + annotations into a JPEG blob. */
  composite() {
    return new Promise((resolve) => {
      const w = this.canvas.width;
      const h = this.canvas.height;
      const offscreen = document.createElement('canvas');
      offscreen.width = w;
      offscreen.height = h;
      const octx = offscreen.getContext('2d');
      // Draw source (video or canvas)
      octx.drawImage(this.source, 0, 0, w, h);
      // Draw all annotations on top
      drawShapes(octx, this.shapes);
      offscreen.toBlob((blob) => resolve(blob), 'image/jpeg', 0.92);
    });
  }
}

// ── Annotation state and integration ──
let annotationCanvas = null;
let annotationMode = false;
let annotationCounter = 0;
let annotationContext = null;
// Captured once at boot: the toolbar ELEMENT rides along with its home
// position, and later moves resolve through this reference rather than
// getElementById. A live-annotation host container can be wiped while
// the toolbar is parked inside it (peer panes are innerHTML-rebuilt on
// daemons-list re-renders); a detached toolbar is unreachable by id, so
// an id-based lookup would lose it permanently — the element reference
// lets restore re-attach it to its home slot.
const annotationToolbarHome = (() => {
  const toolbar = document.getElementById('annotation-toolbar');
  return toolbar ? { toolbar, parent: toolbar.parentNode, nextSibling: toolbar.nextSibling } : null;
})();

function annotationToolbarEl() {
  return (annotationToolbarHome && annotationToolbarHome.toolbar)
    || document.getElementById('annotation-toolbar');
}

function moveAnnotationToolbar(parent, beforeNode = null) {
  const toolbar = annotationToolbarEl();
  if (!toolbar || !parent) return;
  if (toolbar.parentNode !== parent || toolbar.nextSibling !== beforeNode) {
    parent.insertBefore(toolbar, beforeNode);
  }
}

function restoreAnnotationToolbar() {
  if (!annotationToolbarHome) return;
  moveAnnotationToolbar(annotationToolbarHome.parent, annotationToolbarHome.nextSibling);
}

// ── Live-annotation surface provider ─────────────────────────────────
//
// enterLiveAnnotationMode used to take a DisplaySlot directly. The
// provider contract below is derived from every use the live-annotation
// path actually made of that slot (this module plus the two 45-displays
// lifecycle call sites) — exactly this list, nothing more:
//   1. slot.canvasEl    → stageEl(): the stage container the frozen
//      source canvas + drawing overlay are appended into; also the
//      wrapper rect for letterbox alignment and a ResizeObserver target.
//   2. slot.videoEl     → liveSurfaceEl(): the live <video> (or tile
//      <canvas>) whose rendered rect + intrinsic resolution drive
//      alignLiveAnnotationSource's letterbox math.
//   3. slot.annotateBtn → annotateBtn(): the toolbar button whose
//      active state / label setLiveAnnotationButton toggles.
//   4. slot.el          → toolbarHostEl(): the element the shared
//      annotation toolbar is reparented into while editing.
//   5. slot.displayId   → displayId + streamBase: wire naming — the
//      editor submits on stream `${streamBase}_annotation` with frame
//      ids `ann-${streamBase}-N`. streamBase is `display_<id>` locally
//      (byte-identical to the pre-provider strings) and
//      `peer_<host>_display_<id>` on peer panes so ids stay unique
//      across surfaces.
//   6. identity (annotationContext.slot === slot) → owner: the wrapping
//      DisplaySlot / PeerDisplayConnection instance, compared by
//      shouldSuppressDisplayInputForAnnotation (input suppression while
//      drawing) and teardownLiveSurfaceForOwner (removeDisplaySlot /
//      peer close lifecycle).
// (slot.statusEl is deliberately NOT in the contract — the
// live-annotation path never touched it.)
// The element getters are functions, not captured nodes, because peer
// pane DOM is rebuilt across daemons-list re-renders.

function setLiveAnnotationButton(provider, active) {
  const btn = provider && typeof provider.annotateBtn === 'function'
    ? provider.annotateBtn()
    : null;
  if (!btn) return;
  btn.classList.toggle('active', !!active);
  btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  btn.innerHTML = active ? '&#x2715; Annotating' : '&#9998; Annotate';
  btn.title = active ? 'Exit live annotation' : 'Freeze current frame and annotate it';
}

/** Letterbox math shared by the live-annotation editor and callout
 *  arming: the rect (stage-local px) the surface's rendered content
 *  actually occupies inside `stageEl`, accounting for the aspect-
 *  preserving bars. Same formula as 45-displays' renderSharedViewFocus.
 *  `fallbackW/H` substitute for intrinsic dimensions when the surface
 *  reports none (e.g. a <video> whose stream dropped mid-edit). */
function surfaceContentBox(stageEl, surfaceEl, fallbackW, fallbackH) {
  if (!stageEl || !surfaceEl) return null;
  const wrapRect = stageEl.getBoundingClientRect();
  const surfRect = surfaceEl.getBoundingClientRect();
  const intrinsicW = Number(surfaceEl.videoWidth) || Number(surfaceEl.width) || fallbackW || surfRect.width;
  const intrinsicH = Number(surfaceEl.videoHeight) || Number(surfaceEl.height) || fallbackH || surfRect.height;
  if (wrapRect.width <= 0 || wrapRect.height <= 0 || surfRect.width <= 0 || surfRect.height <= 0) return null;
  if (!intrinsicW || !intrinsicH) return null;
  const scale = Math.min(surfRect.width / intrinsicW, surfRect.height / intrinsicH);
  const w = intrinsicW * scale;
  const h = intrinsicH * scale;
  return {
    x: surfRect.left - wrapRect.left + ((surfRect.width - w) / 2),
    y: surfRect.top - wrapRect.top + ((surfRect.height - h) / 2),
    w,
    h,
  };
}

function alignLiveAnnotationSource(provider, source) {
  if (!provider || !source) return;
  const box = surfaceContentBox(
    provider.stageEl(), provider.liveSurfaceEl(), source.width, source.height
  );
  if (!box) return;
  source.style.left = box.x + 'px';
  source.style.top = box.y + 'px';
  source.style.width = box.w + 'px';
  source.style.height = box.h + 'px';
}

// `surfaceOwner` is the DisplaySlot / PeerDisplayConnection whose
// interactive input handlers are asking; both classes pass `this`.
function shouldSuppressDisplayInputForAnnotation(surfaceOwner) {
  return !!(
    annotationMode &&
    annotationContext &&
    annotationContext.kind === 'live' &&
    annotationContext.provider &&
    annotationContext.provider.owner === surfaceOwner
  );
}

function enterAnnotationMode() {
  if (annotationMode) return;
  const video = document.getElementById('recording-video');
  const overlay = document.getElementById('recording-overlay');
  if (!video || !overlay) return;

  // Pause video if playing
  if (recPlayer && recPlayer.playing) recPlayer.pause();

  // Wait for video to have dimensions
  if (!video.videoWidth) return;

  moveAnnotationToolbar(
    document.getElementById('recording-section'),
    document.getElementById('recording-timeline')
  );
  annotationCanvas = new AnnotationCanvas(overlay, video);
  annotationCanvas.activate();
  annotationMode = true;
  annotationContext = { kind: 'recording', stream: activeRecordingStream || 'annotation' };
  document.getElementById('annotation-toolbar').classList.remove('hidden');
  if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();

  // Re-align canvas when the player wrap resizes (split handle drag)
  if (window._annResizeObserver) window._annResizeObserver.disconnect();
  window._annResizeObserver = new ResizeObserver(() => {
    if (annotationCanvas && annotationMode) annotationCanvas._alignToSource();
  });
  window._annResizeObserver.observe(document.getElementById('recording-player-wrap'));
}

// `provider` is a live-annotation surface provider (contract enumerated
// above setLiveAnnotationButton). DisplaySlot and PeerDisplayConnection
// each wrap themselves via their `_annotationSurfaceProvider()`.
function enterLiveAnnotationMode(provider, frame) {
  if (!provider || !frame) return;
  const stage = provider.stageEl();
  if (!stage) return;
  if (annotationMode) {
    if (annotationContext && annotationContext.kind === 'live' &&
        annotationContext.provider && annotationContext.provider.owner === provider.owner) {
      exitAnnotationMode();
      return;
    }
    exitAnnotationMode();
  }
  // The editor and an armed callout are both drag layers on the stage —
  // the newest mode wins.
  disarmLiveCallout();

  const source = frame.canvas;
  source.className = 'live-annotation-source';
  source.setAttribute('aria-hidden', 'true');

  const overlay = document.createElement('canvas');
  overlay.className = 'live-annotation-overlay';
  overlay.width = frame.width;
  overlay.height = frame.height;

  stage.appendChild(source);
  stage.appendChild(overlay);
  alignLiveAnnotationSource(provider, source);

  annotationCanvas = new AnnotationCanvas(overlay, source);
  annotationCanvas.activate();
  annotationMode = true;
  annotationContext = {
    kind: 'live',
    provider,
    streamBase: provider.streamBase,
    stream: `${provider.streamBase}_annotation`,
    source,
    overlay,
  };
  setLiveAnnotationButton(provider, true);
  moveAnnotationToolbar(provider.toolbarHostEl(), null);
  annotationToolbarEl().classList.remove('hidden');
  if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();

  if (window._annResizeObserver) window._annResizeObserver.disconnect();
  window._annResizeObserver = new ResizeObserver(() => {
    if (annotationCanvas && annotationMode) {
      alignLiveAnnotationSource(provider, source);
      annotationCanvas._alignToSource();
    }
  });
  window._annResizeObserver.observe(stage);
  window._annResizeObserver.observe(source);
}

function exitAnnotationMode() {
  if (!annotationMode) return;
  if (window._annResizeObserver) { window._annResizeObserver.disconnect(); window._annResizeObserver = null; }
  const context = annotationContext;
  if (annotationCanvas) {
    annotationCanvas.deactivate();
    annotationCanvas = null;
  }
  if (context && context.kind === 'live') {
    if (context.source && context.source.parentNode) context.source.parentNode.removeChild(context.source);
    if (context.overlay && context.overlay.parentNode) context.overlay.parentNode.removeChild(context.overlay);
    setLiveAnnotationButton(context.provider, false);
  }
  annotationMode = false;
  annotationContext = null;
  annotationToolbarEl().classList.add('hidden');
  restoreAnnotationToolbar();
}

// Provider-level lifecycle teardown: end any live-annotation edit or
// armed callout owned by `owner`. removeDisplaySlot (45-displays) and
// PeerDisplayConnection.close (52-peer-display) call this so surface
// destruction always tears the editor / arm down with it.
function teardownLiveSurfaceForOwner(owner) {
  if (liveCalloutArm && liveCalloutArm.owner === owner) disarmLiveCallout();
  if (
    annotationMode &&
    annotationContext &&
    annotationContext.kind === 'live' &&
    annotationContext.provider &&
    annotationContext.provider.owner === owner
  ) {
    exitAnnotationMode();
  }
}

// Softer sibling for pane REBUILDS (peer panes are innerHTML-rebuilt on
// daemons-list re-renders): only tears down when the mode's overlay DOM
// actually died with the old pane — a rebuild of some other container
// for the same host leaves a still-connected editor alone.
function reconcileLiveSurfaceForOwner(owner) {
  if (
    liveCalloutArm && liveCalloutArm.owner === owner &&
    liveCalloutArm.overlayEl && !liveCalloutArm.overlayEl.isConnected
  ) {
    disarmLiveCallout();
  }
  if (
    annotationMode &&
    annotationContext &&
    annotationContext.kind === 'live' &&
    annotationContext.provider &&
    annotationContext.provider.owner === owner &&
    annotationContext.overlay &&
    !annotationContext.overlay.isConnected
  ) {
    exitAnnotationMode();
  }
}

// ── Toolbar-armed Callout mode ────────────────────────────────────────
//
// The safe variant of the reverse-callout idea (bare alt-drag was
// rejected — it collides with X11 window-manager gestures). The Callout
// toolbar button (local display slots + peer panes, enabled only while
// that surface's input authority is 'you') arms one-shot region
// flagging: the next pointer-drag on the frame draws a live rectangle,
// and a release covering >= ~1% of the frame area captures the current
// frame, strokes the rectangle onto the copy (amber), and ships it
// through the annotation-attach lane as a pending attachment (stream
// `${streamBase}_callout`, note 'user callout'). Firing, Escape, or a
// second button click disarms. While armed, the arm overlay swallows
// the drag's pointer events (and the surface input handlers belt-and-
// braces on liveCalloutArmedFor) so no md/mm/mu reaches the remote
// display; keyboard input keeps flowing.
let liveCalloutArm = null; // { owner, provider, button, captureFrame, overlayEl, onKeyDown }
let liveCalloutCounter = 0;
const LIVE_CALLOUT_MIN_AREA_FRAC = 0.01; // ~1% of the frame area
const LIVE_CALLOUT_STROKE = '#ffb454'; // amber; vivid on light and dark content

function liveCalloutArmedFor(owner) {
  return !!(liveCalloutArm && liveCalloutArm.owner === owner);
}

function disarmLiveCallout() {
  const arm = liveCalloutArm;
  if (!arm) return;
  liveCalloutArm = null;
  document.removeEventListener('keydown', arm.onKeyDown, true);
  if (arm.overlayEl && arm.overlayEl.parentNode) {
    arm.overlayEl.parentNode.removeChild(arm.overlayEl);
  }
  if (arm.button) {
    arm.button.classList.remove('armed');
    arm.button.setAttribute('aria-pressed', 'false');
  }
}

// `spec`: { provider, button, captureFrame(quality) → frame|null }.
// `provider` is the same surface-provider contract the annotation editor
// consumes; `captureFrame` is the class's own captureCurrentFrame.
function toggleLiveCallout(spec) {
  if (!spec || !spec.provider) return;
  if (liveCalloutArmedFor(spec.provider.owner)) {
    disarmLiveCallout();
    return;
  }
  disarmLiveCallout(); // only one armed surface at a time
  armLiveCallout(spec);
}

function armLiveCallout(spec) {
  const provider = spec.provider;
  const stage = provider.stageEl();
  const box = surfaceContentBox(stage, provider.liveSurfaceEl());
  if (!stage || !box) {
    if (typeof showControlToast === 'function') {
      showControlToast('error', 'No live frame to call out yet');
    }
    return;
  }
  // Arming while the live-annotation editor is open would stack two
  // drag layers on the stage — the newest mode wins.
  if (annotationMode && annotationContext && annotationContext.kind === 'live') {
    exitAnnotationMode();
  }

  const overlayEl = document.createElement('div');
  overlayEl.className = 'callout-arm-overlay';
  const rectEl = document.createElement('div');
  rectEl.className = 'callout-arm-rect';
  rectEl.style.display = 'none';
  const hintEl = document.createElement('div');
  hintEl.className = 'callout-arm-hint';
  hintEl.textContent = 'Drag to call out a region — Esc cancels';
  overlayEl.appendChild(rectEl);
  overlayEl.appendChild(hintEl);
  stage.appendChild(overlayEl);

  // Pin the overlay to the rendered frame's content box (letterbox math
  // shared with renderSharedViewFocus via surfaceContentBox), so drag
  // fractions ARE frame fractions.
  const align = () => {
    const b = surfaceContentBox(provider.stageEl(), provider.liveSurfaceEl());
    if (!b) return;
    overlayEl.style.left = b.x + 'px';
    overlayEl.style.top = b.y + 'px';
    overlayEl.style.width = b.w + 'px';
    overlayEl.style.height = b.h + 'px';
  };
  align();

  let drag = null;
  const fracPoint = (e) => {
    const r = overlayEl.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return null;
    return {
      x: Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)),
      y: Math.max(0, Math.min(1, (e.clientY - r.top) / r.height)),
    };
  };
  const renderDragRect = () => {
    if (!drag) return;
    rectEl.style.left = (Math.min(drag.x0, drag.x1) * 100) + '%';
    rectEl.style.top = (Math.min(drag.y0, drag.y1) * 100) + '%';
    rectEl.style.width = (Math.abs(drag.x1 - drag.x0) * 100) + '%';
    rectEl.style.height = (Math.abs(drag.y1 - drag.y0) * 100) + '%';
  };
  const resetDrag = () => {
    drag = null;
    rectEl.style.display = 'none';
    hintEl.style.display = '';
  };
  overlayEl.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (drag) return; // ignore extra pointers mid-drag
    align(); // layout may have shifted since arming
    const p = fracPoint(e);
    if (!p) return;
    drag = { x0: p.x, y0: p.y, x1: p.x, y1: p.y, pointerId: e.pointerId };
    try { overlayEl.setPointerCapture(e.pointerId); } catch (_) {}
    hintEl.style.display = 'none';
    rectEl.style.display = '';
    renderDragRect();
  });
  overlayEl.addEventListener('pointermove', (e) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    e.preventDefault();
    const p = fracPoint(e);
    if (!p) return;
    drag.x1 = p.x;
    drag.y1 = p.y;
    renderDragRect();
  });
  overlayEl.addEventListener('pointerup', (e) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    e.preventDefault();
    const p = fracPoint(e);
    if (p) { drag.x1 = p.x; drag.y1 = p.y; }
    const rect = {
      x: Math.min(drag.x0, drag.x1),
      y: Math.min(drag.y0, drag.y1),
      w: Math.abs(drag.x1 - drag.x0),
      h: Math.abs(drag.y1 - drag.y0),
    };
    if (rect.w * rect.h < LIVE_CALLOUT_MIN_AREA_FRAC) {
      // Too small to be a deliberate callout — stay armed, keep hinting.
      resetDrag();
      return;
    }
    disarmLiveCallout(); // one-shot: the gesture consumed the arm
    submitLiveCallout(spec, rect);
  });
  overlayEl.addEventListener('pointercancel', () => resetDrag());

  // Escape disarms. Capture phase so neither the surface's interactive
  // keydown forwarder nor the global annotation Escape handler sees it.
  const onKeyDown = (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      disarmLiveCallout();
    }
  };
  document.addEventListener('keydown', onKeyDown, true);

  if (spec.button) {
    spec.button.classList.add('armed');
    spec.button.setAttribute('aria-pressed', 'true');
  }
  liveCalloutArm = {
    owner: provider.owner,
    provider,
    button: spec.button || null,
    captureFrame: spec.captureFrame,
    overlayEl,
    onKeyDown,
  };
}

async function submitLiveCallout(spec, rect) {
  const frame = typeof spec.captureFrame === 'function' ? spec.captureFrame(0.92) : null;
  if (!frame || !frame.canvas) {
    if (typeof showControlToast === 'function') {
      showControlToast('error', 'No frame available to call out');
    }
    return;
  }
  // Stroke the rectangle onto the captured copy (never the live surface).
  const ctx = frame.canvas.getContext('2d');
  ctx.save();
  ctx.strokeStyle = LIVE_CALLOUT_STROKE;
  ctx.lineWidth = Math.max(4, frame.width / 300);
  ctx.strokeRect(
    rect.x * frame.width,
    rect.y * frame.height,
    Math.max(1, rect.w * frame.width),
    Math.max(1, rect.h * frame.height)
  );
  ctx.restore();
  const dataUrl = frame.canvas.toDataURL('image/jpeg', 0.92);
  const b64 = dataUrl.split(',')[1];
  liveCalloutCounter++;
  const stream = `${spec.provider.streamBase}_callout`;
  const frameId = `${stream}-f${String(liveCalloutCounter).padStart(5, '0')}`;
  const note = 'user callout';
  try {
    await sendDashboardMediaUpload(
      'api_media_annotation_attach',
      { frame_id: frameId, stream, note },
      dashboardControlBase64ToBytes(b64),
      { t: 'annotation_attach', frame_id: frameId, stream, data: b64, note },
      'callout attach'
    );
  } catch (err) {
    dashboardMediaTransferFailed(err, 'callout attach');
    return;
  }
  addPendingAttachment({ frameId, stream, note, dataUrl });
  if (spec.button) {
    const orig = spec.button.innerHTML;
    spec.button.innerHTML = '&#x2713; Attached';
    window.setTimeout(() => { spec.button.innerHTML = orig; }, 1500);
  }
}

// ── Pending attachments (frame IDs queued for the next task) ──
//
// Client-side only — not persisted across reloads. Each entry is
//   { frameId, stream, note, dataUrl }
// where `dataUrl` is a data:, blob:, or legacy raw HTTP URL for the chip thumbnail.
const ATTACHMENT_SOFT_LIMIT = 20;
const pendingAttachments = [];
const retainedAttachmentPreviewObjectUrls = new Set();

function addPendingAttachment(att) {
  if (!att || !att.frameId) return;
  // De-dupe by frame ID
  if (pendingAttachments.some(p => p.frameId === att.frameId)) {
    revokePendingAttachmentPreview(att);
    renderPendingAttachments();
    return;
  }
  pendingAttachments.push(att);
  renderPendingAttachments();
}

function revokePendingAttachmentPreview(att) {
  const objectUrl = String(att?.dataObjectUrl || '').trim();
  if (objectUrl) {
    try { URL.revokeObjectURL(objectUrl); } catch (_) {}
    retainedAttachmentPreviewObjectUrls.delete(objectUrl);
  }
}

function retainPendingAttachmentPreview(att) {
  const objectUrl = String(att?.dataObjectUrl || '').trim();
  if (objectUrl) retainedAttachmentPreviewObjectUrls.add(objectUrl);
}

window.addEventListener('beforeunload', () => {
  for (const objectUrl of retainedAttachmentPreviewObjectUrls) {
    try { URL.revokeObjectURL(objectUrl); } catch (_) {}
  }
  retainedAttachmentPreviewObjectUrls.clear();
});

function pendingAttachmentUploadId(att) {
  const explicit = String(att?.uploadId || '').trim();
  if (explicit) return explicit;
  const frameId = String(att?.frameId || '').trim();
  return frameId.startsWith('upload:') ? frameId.slice('upload:'.length) : '';
}

function deletePendingUploadRecord(uploadId) {
  const id = String(uploadId || '').trim();
  if (!id) return;
  // Transport F8a: facade DELETE twin — the verb-derived no-replay policy
  // is the legacy fallbackAfterRpcFailure:false semantics. The descriptor
  // row captures `upload_id` (the tunnel handler accepts both names).
  daemonApi.request('api_session_current_upload_delete', { upload_id: id })
    .then(resp => {
      if (!resp.ok && resp.status !== 404) {
        console.warn(`[upload] Delete failed (${id}): ${resp.status}`);
      }
    })
    .catch(err => console.warn(`[upload] Delete failed (${id}): ${err}`));
}

function removePendingAttachment(frameId, options = {}) {
  const idx = pendingAttachments.findIndex(p => p.frameId === frameId);
  if (idx >= 0) {
    const [removed] = pendingAttachments.splice(idx, 1);
    revokePendingAttachmentPreview(removed);
    renderPendingAttachments();
    if (options.deleteUpload) deletePendingUploadRecord(pendingAttachmentUploadId(removed));
  }
}

function clearPendingAttachments(options = {}) {
  const uploadIds = options.deleteUploads
    ? pendingAttachments.map(pendingAttachmentUploadId).filter(Boolean)
    : [];
  if (options.retainPreviewUrls) {
    pendingAttachments.forEach(retainPendingAttachmentPreview);
  } else {
    pendingAttachments.forEach(revokePendingAttachmentPreview);
  }
  pendingAttachments.length = 0;
  renderPendingAttachments();
  uploadIds.forEach(deletePendingUploadRecord);
}

// ── File upload ───────────────────────────────────────────────────────
//
// Flow: user clicks Attach (or drops files on the task input) → we stream
// each file over the verified dashboard control tunnel when available, falling
// back to POST /api/session/current/uploads otherwise. The server replies
// with an UploadDescriptor and also broadcasts UploadReady. Only the initiating
// browser queues a pending chip; broadcasts are informational so parallel
// dashboards do not accidentally attach each other's uploads. Submitting a
// task maps the chip's `frameId` (of the form "upload:<id>") directly into
// the `attachments` array — the server's `resolve_attachments` understands
// the prefix.

// Cap mirrors UPLOAD_MAX_BYTES on the server. Front-end check is UX only
// (the server bails with 413 even if the browser lies); nicer to catch it
// before the network round-trip.
const UPLOAD_MAX_BYTES = 100 * 1024 * 1024;
let activeUploadFlows = 0;

// humanBytes lives in 46-recording-replay.js (the canonical dashboard
// bytes formatter; `_fmtBytes` there is its legacy alias).

// "Can staged uploads ride the TUNNEL upload-frame / byte-stream lane?"
// Derived through the facade (F1b): reason 'connected' folds the live
// tunnel, the wire-lane feature (upload_frames / byte_streams via the
// descriptor's lane), and the per-method availability boolean into one
// answer. Deliberately not `.ok` — `http-only` means the direct HTTP twin
// could serve it, which these tunnel-lane gates must not conflate.
function dashboardUploadRpcAvailable() {
  return daemonApi.availability('api_session_current_upload').reason === 'connected';
}

function dashboardUploadRawRpcAvailable() {
  return daemonApi.availability('api_session_current_upload_raw').reason === 'connected';
}

async function uploadImagePreviewUrl(descriptor) {
  const id = String(descriptor?.id || '').trim();
  const mime = String(descriptor?.mime || 'application/octet-stream');
  if (!id || !mime.startsWith('image/')) return { dataUrl: null, dataObjectUrl: null };
  if (dashboardUploadRawRpcAvailable()) {
    try {
      const raw = await dashboardTransport.requestBytes('api_session_current_upload_raw', { id }, {
        timeoutMs: 60000,
      });
      if (raw?._httpOk === false) {
        throw new Error(raw.error || `upload raw returned ${raw._httpStatus || 'error'}`);
      }
      if (raw?.bytes instanceof Uint8Array && raw.bytes.byteLength > 0) {
        const blob = new Blob([raw.bytes], { type: raw.content_type || mime });
        const url = URL.createObjectURL(blob);
        return { dataUrl: url, dataObjectUrl: url };
      }
    } catch (err) {
      const suffix = dashboardConnectModeEnabled()
        ? '; no HTTP fallback in Connect mode'
        : '; using HTTP URL fallback';
      console.warn(`[upload] Failed to load image preview over dashboard control${suffix}`, err);
    }
  }
  if (dashboardConnectModeEnabled()) {
    return { dataUrl: null, dataObjectUrl: null };
  }
  return {
    dataUrl: `/api/session/current/uploads/${encodeURIComponent(id)}/raw`,
    dataObjectUrl: null,
  };
}

function dashboardTerminalFramesAvailable() {
  return Boolean(
    dashboardTransport &&
    dashboardTransport.canUseRpc &&
    dashboardTransport.canUseRpc() &&
    dashboardControlTransport?.lastStatus?.terminal_frames_available === true
  );
}

async function uploadOneFile(file, destination) {
  if (file.size > UPLOAD_MAX_BYTES) {
    logErrorToActivity(`File too large (${humanBytes(file.size)}): ${file.name}. Cap is ${humanBytes(UPLOAD_MAX_BYTES)}.`);
    return null;
  }
  const mime = file.type || 'application/octet-stream';
  const q = `destination=${encodeURIComponent(destination)}&name=${encodeURIComponent(file.name || 'upload.bin')}`;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 120000);
  try {
    let json;
    if (dashboardUploadRpcAvailable()) {
      json = await dashboardTransport.uploadBytes('api_session_current_upload', {
        destination,
        name: file.name || 'upload.bin',
        mime,
      }, file, {
        timeoutMs: 120000,
        signal: controller.signal,
      });
      if (json?._httpOk === false) {
        logErrorToActivity(`Upload failed (${file.name}): ${json.error || json._httpStatus || 'error'}`);
        return null;
      }
    } else {
      if (dashboardConnectModeEnabled()) {
        logErrorToActivity(`Upload failed (${file.name}): Hosted Connect upload access is unavailable.`);
        return null;
      }
      const resp = await fetch(`/api/session/current/uploads?${q}`, {
        method: 'POST',
        headers: { 'Content-Type': mime },
        body: file,
        signal: controller.signal,
      });
      json = await resp.json().catch(() => ({}));
      if (!resp.ok) {
        logErrorToActivity(`Upload failed (${file.name}): ${json.error || resp.statusText}`);
        return null;
      }
    }
    await onUploadReady(json);
    return json;
  } catch (e) {
    if (e && e.name === 'AbortError') {
      logErrorToActivity(`Upload timed out (${file.name}).`);
    } else {
      logErrorToActivity(`Upload failed (${file.name}): ${e}`);
    }
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

function logErrorToActivity(text) {
  // Best-effort — if the helper isn't available yet, use console.
  if (typeof renderLogEntry === 'function') {
    try {
      renderLogEntry({ ts: '', level: 'warn', source: 'upload', content: text });
      return;
    } catch (_) { /* fall through */ }
  }
  console.warn('[upload]', text);
}

function currentLogTime() {
  return new Date().toTimeString().slice(0, 8);
}

function formatSessionDetailTimestamp(value) {
  const raw = String(value || '').trim();
  if (!raw) return '';
  return formatLogTimestampLabel(raw);
}

const pendingAttachmentLogReceipts = [];
const ATTACHMENT_RECEIPT_DEDUPE_MS = 30000;

function normalizeAttachmentReceiptText(text) {
  return String(text || '').trim().replace(/\s+/g, ' ');
}

function pruneAttachmentReceiptDedupe(now = Date.now()) {
  for (let i = pendingAttachmentLogReceipts.length - 1; i >= 0; i--) {
    if (now - pendingAttachmentLogReceipts[i].createdAt > ATTACHMENT_RECEIPT_DEDUPE_MS) {
      pendingAttachmentLogReceipts.splice(i, 1);
    }
  }
  while (pendingAttachmentLogReceipts.length > 20) pendingAttachmentLogReceipts.shift();
}

function rememberAttachmentReceipt(text) {
  const normalized = normalizeAttachmentReceiptText(text);
  if (!normalized) return;
  pruneAttachmentReceiptDedupe();
  pendingAttachmentLogReceipts.push({ text: normalized, createdAt: Date.now() });
}

function shouldSuppressAttachmentReceiptDuplicate(c) {
  const source = String(c && c.source || '').toLowerCase();
  if (source !== 'user') return false;
  if (Array.isArray(c.attachment_previews) && c.attachment_previews.length > 0) return false;
  const text = normalizeAttachmentReceiptText(c.content);
  if (!text) return false;
  const now = Date.now();
  pruneAttachmentReceiptDedupe(now);
  const idx = pendingAttachmentLogReceipts.findIndex(receipt => receipt.text === text);
  if (idx < 0) return false;
  pendingAttachmentLogReceipts.splice(idx, 1);
  return true;
}

function renderAttachmentReceipt(text, attachments, verb, sessionId) {
  if (!attachments || attachments.length === 0 || typeof renderLogEntry !== 'function') return;
  const previews = attachments.map((att) => ({
    frameId: att.frameId,
    name: att.uploadName || att.frameId,
    note: att.note || att.uploadName || att.frameId,
    dataUrl: att.dataUrl || null,
  }));
  const count = attachments.length;
  const label = count === 1 ? '1 attachment' : count + ' attachments';
  const content = `${verb || 'Sent'} ${label}${text ? `: ${text}` : ''}`;
  renderLogEntry({
    ts: currentLogTime(),
    level: 'info',
    source: 'user',
    content,
    session_id: sessionId || undefined,
    attachment_previews: previews,
  });
  rememberAttachmentReceipt(text);
}

function uploadAttachButtons() {
  return [
    document.getElementById('upload-attach-btn'),
    document.getElementById('new-session-attach-btn'),
  ].filter(Boolean);
}

async function triggerUploadFlow(files) {
  if (!files || files.length === 0) return;
  const dest = 'task';
  const buttons = uploadAttachButtons();
  activeUploadFlows += 1;
  buttons.forEach(btn => btn.classList.add('uploading'));
  // Fire uploads sequentially to avoid hammering the gateway — it's a
  // hand-rolled server, not multiplexed. Can parallelise later if it
  // becomes a bottleneck.
  try {
    for (const f of files) {
      await uploadOneFile(f, dest);
    }
  } finally {
    activeUploadFlows = Math.max(0, activeUploadFlows - 1);
    if (activeUploadFlows === 0) {
      buttons.forEach(btn => btn.classList.remove('uploading'));
    }
  }
}

async function onUploadReady(descriptor, options = {}) {
  if (!descriptor || !descriptor.id) return;
  if (options.fromBroadcast) return;
  const frameId = `upload:${descriptor.id}`;
  // Thumbnail for images, a paperclip + name label for files.
  const isImage = typeof descriptor.mime === 'string' && descriptor.mime.startsWith('image/');
  const preview = isImage
    ? await uploadImagePreviewUrl(descriptor)
    : { dataUrl: null, dataObjectUrl: null };
  const safeName = String(descriptor.name || 'upload.bin');
  const originalName = String(descriptor.original_name || descriptor.originalName || '').trim();
  const displayName = originalName || safeName;
  const noteName = originalName && originalName !== safeName
    ? `${originalName} (stored as ${safeName})`
    : displayName;
  addPendingAttachment({
    frameId,
    stream: 'upload',
    note: `${noteName} (${humanBytes(descriptor.size || 0)}, ${descriptor.destination})`,
    dataUrl: preview.dataUrl,
    dataObjectUrl: preview.dataObjectUrl,
    uploadId: descriptor.id,
    uploadName: displayName,
    uploadSafeName: safeName,
    uploadMime: descriptor.mime,
    uploadDestination: descriptor.destination,
  });
}

function onUploadDeleted(id) {
  if (!id) return;
  removePendingAttachment(`upload:${id}`, { deleteUpload: false });
}

function wireUploadControls() {
  const input = document.getElementById('upload-file-input');
  const taskInput = document.getElementById('activity-task-input');
  const newSessionInput = document.getElementById('new-session-input');
  const buttons = uploadAttachButtons();
  if (!buttons.length || !input) return;
  buttons.forEach(btn => {
    if (btn._uploadWired) return;
    btn._uploadWired = true;
    btn.addEventListener('click', () => input.click());
  });
  if (input._uploadWired) return;
  input._uploadWired = true;
  input.addEventListener('change', (ev) => {
    const files = Array.from(ev.target.files || []);
    triggerUploadFlow(files);
    // Reset so selecting the same file twice in a row re-fires `change`.
    ev.target.value = '';
  });
  // Drag-and-drop on the task input AND the Attach button for a larger hit
  // target. The whole page would be nicer but has more false positives
  // (dragging text, other apps' files onto the tab).
  const setDragHover = (on) => buttons.forEach(btn => btn.classList.toggle('drag-hover', on));
  const dropTargets = buttons.slice();
  if (taskInput) dropTargets.push(taskInput);
  if (newSessionInput) dropTargets.push(newSessionInput);
  for (const el of dropTargets) {
    el.addEventListener('dragenter', (e) => { e.preventDefault(); setDragHover(true); });
    el.addEventListener('dragover', (e) => { e.preventDefault(); e.dataTransfer.dropEffect = 'copy'; });
    el.addEventListener('dragleave', () => { setDragHover(false); });
    el.addEventListener('drop', (e) => {
      e.preventDefault();
      setDragHover(false);
      const files = Array.from(e.dataTransfer?.files || []);
      triggerUploadFlow(files);
    });
  }
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', wireUploadControls);
} else {
  wireUploadControls();
}

function pendingAttachmentDisplayName(att) {
  return String(att?.uploadName || att?.uploadSafeName || '').trim();
}

function pendingAttachmentDuplicateSuffix(att, displayName) {
  if (!displayName || !att?.uploadId) return '';
  const peers = pendingAttachments.filter(p => pendingAttachmentDisplayName(p) === displayName);
  if (peers.length <= 1) return '';
  const idx = peers.findIndex(p => p.uploadId === att.uploadId || p.frameId === att.frameId);
  return idx >= 0 ? ` #${idx + 1}` : '';
}

function clippedAttachmentLabel(name, suffix) {
  const maxLen = suffix ? 20 : 24;
  const clipped = name.length > maxLen ? name.slice(0, Math.max(1, maxLen - 2)) + '…' : name;
  return '📎 ' + clipped + suffix;
}

function buildPendingAttachmentChip(att) {
  const chip = document.createElement('span');
  chip.className = 'pending-attachment-chip';
  const displayName = pendingAttachmentDisplayName(att);
  chip.title = att.note
    ? `${displayName || att.frameId} — ${att.note}`
    : (displayName || att.frameId);
  if (att.dataUrl) {
    const img = document.createElement('img');
    img.className = 'chip-thumb';
    img.src = att.dataUrl;
    img.alt = '';
    chip.appendChild(img);
  }
  const label = document.createElement('span');
  // Upload chips get a paperclip + filename; frame chips keep the ID
  // (it's already short — `ann-*`, `clip-*`, `display_*`).
  if (displayName) {
    const suffix = pendingAttachmentDuplicateSuffix(att, displayName);
    label.textContent = clippedAttachmentLabel(displayName, suffix);
  } else {
    label.textContent = att.frameId.length > 24 ? '…' + att.frameId.slice(-22) : att.frameId;
  }
  chip.appendChild(label);
  const x = document.createElement('button');
  x.className = 'chip-remove';
  x.title = 'Remove from attachments';
  x.textContent = '×';
  x.addEventListener('click', (e) => {
    e.stopPropagation();
    removePendingAttachment(att.frameId, { deleteUpload: true });
  });
  chip.appendChild(x);
  return chip;
}

function renderAttachmentList(list) {
  if (!list) return;
  list.innerHTML = '';
  for (const att of pendingAttachments) {
    list.appendChild(buildPendingAttachmentChip(att));
  }
}

function renderPendingAttachmentBar(bar, list, count) {
  if (!bar || !list || !count) return;
  if (pendingAttachments.length === 0) {
    bar.classList.add('hidden');
    list.innerHTML = '';
    count.textContent = '0';
    count.classList.remove('warn');
    count.title = '';
    return;
  }
  bar.classList.remove('hidden');
  count.textContent = String(pendingAttachments.length);
  count.classList.toggle('warn', pendingAttachments.length > ATTACHMENT_SOFT_LIMIT);
  if (pendingAttachments.length > ATTACHMENT_SOFT_LIMIT) {
    count.title = `${pendingAttachments.length} attachments — agents may fail with this many`;
  } else {
    count.title = `${pendingAttachments.length} pending attachment${pendingAttachments.length === 1 ? '' : 's'}`;
  }
  renderAttachmentList(list);
}

function renderPendingAttachments() {
  const bar = document.getElementById('pending-attachments-bar');
  const list = document.getElementById('pending-attachments-list');
  const count = document.getElementById('pending-attachments-count');
  const newSessionBar = document.getElementById('new-session-attachments-bar');
  const newSessionList = document.getElementById('new-session-attachments-list');
  const newSessionCount = document.getElementById('new-session-attachments-count');
  const globalList = document.getElementById('global-pending-attachments');
  if (globalList) {
    globalList.classList.toggle('hidden', pendingAttachments.length === 0);
    globalList.title = pendingAttachments.length === 0
      ? ''
      : `${pendingAttachments.length} pending attachment${pendingAttachments.length === 1 ? '' : 's'}`;
    renderAttachmentList(globalList);
  }
  renderPendingAttachmentBar(bar, list, count);
  renderPendingAttachmentBar(newSessionBar, newSessionList, newSessionCount);
}

window.addPendingAttachment = addPendingAttachment;
window.removePendingAttachment = removePendingAttachment;
window.clearPendingAttachments = clearPendingAttachments;

// ── Annotation Send button gating ──
//
// The Send button injects into the live presence layer. If no presence is
// connected (no voice model + no server-side text presence), there's nothing
// to inject into. Disable the button with a tooltip pointing the user at
// Attach instead.
function updateAnnotationSendState() {
  const annSend = document.getElementById('ann-submit-btn');
  const clipSend = document.getElementById('clip-send-btn');
  const presenceLive = !!modelConnected;
  for (const btn of [annSend, clipSend]) {
    if (!btn) continue;
    // Don't gate the button while it's repurposed as the annotation "Done"
    // button during clip annotation drawing — that uses the same id but a
    // different label and meaning.
    if (clipAnnotatingRange && btn === annSend) continue;
    btn.disabled = !presenceLive;
    if (presenceLive) {
      btn.title = btn === clipSend
        ? 'Save and inject into the live presence layer'
        : 'Save and inject into the live presence layer';
    } else {
      btn.title = 'No presence connected — use Attach instead';
    }
  }
}
window.updateAnnotationSendState = updateAnnotationSendState;

function wirePendingAttachmentClearButtons() {
  document.querySelectorAll('[data-pending-attachments-clear]').forEach(clearBtn => {
    if (clearBtn._wired) return;
    clearBtn._wired = true;
    clearBtn.addEventListener('click', () => clearPendingAttachments({ deleteUploads: true }));
  });
}

// Wire the bars' Clear buttons (after DOM is ready)
document.addEventListener('DOMContentLoaded', wirePendingAttachmentClearButtons);

// Hot-wire in case DOMContentLoaded already fired
wirePendingAttachmentClearButtons();

// Submit an annotation with one of three actions:
//   'save'   — register in frame registry, don't inject, don't attach
//   'attach' — register and queue as a pending attachment for the next task
//   'send'   — register and inject into the live presence layer immediately
async function submitAnnotation(action) {
  if (!annotationCanvas) return;
  // Backwards-compat: callers used to pass a bool (inject=true → 'send').
  if (action === true) action = 'send';
  if (action === false || action == null) action = 'save';

  const note = document.getElementById('ann-note').value.trim();
  const blob = await annotationCanvas.composite();
  if (!blob) return;

  const b64 = await new Promise((resolve) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result.split(',')[1]);
    reader.readAsDataURL(blob);
  });

  annotationCounter++;
  const stream = annotationContext && annotationContext.stream
    ? annotationContext.stream
    : (activeRecordingStream || 'annotation');
  const framePrefix = annotationContext && annotationContext.kind === 'live'
    ? `ann-${annotationContext.streamBase}`
    : `ann-${activeRecordingStream || 'recording'}`;
  const frameId = `${framePrefix}-${annotationCounter}`;

  if (action === 'attach') {
    // Attach: register the frame on the server, queue the ID locally.
    const payload = {
      t: 'annotation_attach',
      frame_id: frameId,
      stream: stream,
      data: b64,
      note: note,
    };
    try {
      await sendDashboardMediaUpload(
        'api_media_annotation_attach',
        { frame_id: frameId, stream, note },
        blob,
        payload,
        'annotation attach'
      );
    } catch (err) {
      dashboardMediaTransferFailed(err, 'annotation attach');
      return;
    }
    addPendingAttachment({ frameId, stream, note, dataUrl: 'data:image/jpeg;base64,' + b64 });
  } else {
    // save / send: existing annotation_submit handler. inject=true for 'send'.
    const payload = {
      t: 'annotation_submit',
      frame_id: frameId,
      stream: stream,
      data: b64,
      note: note,
      inject: action === 'send',
    };
    try {
      await sendDashboardMediaUpload(
        'api_media_annotation_submit',
        { frame_id: frameId, stream, note, inject: action === 'send' },
        blob,
        payload,
        action === 'send' ? 'annotation send' : 'annotation save'
      );
    } catch (err) {
      dashboardMediaTransferFailed(err, action === 'send' ? 'annotation send' : 'annotation save');
      return;
    }
    addAnnotationRef(frameId, action === 'send');
  }

  document.getElementById('ann-note').value = '';
  annotationCanvas.clear();
  exitAnnotationMode();
}

const savedAnnotations = [];

function addAnnotationRef(frameId, injected) {
  savedAnnotations.push({ frameId, injected });
  if (savedAnnotations.length > 100) savedAnnotations.splice(0, savedAnnotations.length - 100);
  renderAnnotationRefs();
}

function renderAnnotationRefs() {
  let panel = document.getElementById('annotation-refs-panel');
  if (!panel) {
    // Create panel below annotation toolbar
    panel = document.createElement('div');
    panel.id = 'annotation-refs-panel';
    panel.className = 'annotation-refs-panel';
    const section = document.getElementById('recording-section');
    section.appendChild(panel);
  }
  panel.style.display = savedAnnotations.length > 0 ? '' : 'none';

  // Daemon-derived strings never ride an inline onclick: rows carry the
  // frame id in a data attribute and get real listeners after the build.
  const items = savedAnnotations.map(a => {
    const badge = a.injected ? '<span style="color:var(--green);font-size:10px"> sent</span>' : '<span style="color:var(--overlay0);font-size:10px"> saved</span>';
    return `<span class="ann-ref-item" title="Click to copy frame ID" data-frame-id="${escapeHtml(a.frameId)}">${escapeHtml(a.frameId)}${badge}</span>`;
  }).join('');

  const framesPath = currentSessionFullId ? `~/.intendant/logs/${currentSessionFullId}/frames/` : '';
  panel.innerHTML = `
    <span style="font-size:11px;color:var(--subtext1);font-weight:600">Annotations:</span>
    ${items}
    <button class="ann-copy-all-btn" title="Copy all frame IDs to clipboard">Copy all</button>
    ${framesPath ? `<span class="ann-path-hint" style="display:block;font-size:10px;color:var(--overlay0);margin-top:4px;cursor:pointer" title="Click to copy">${escapeHtml(framesPath)}</span>` : ''}
  `;
  panel.querySelectorAll('.ann-ref-item').forEach(el => {
    el.addEventListener('click', () => copyAnnotationRef(el.dataset.frameId || ''));
  });
  panel.querySelector('.ann-copy-all-btn')?.addEventListener('click', copyAllAnnotationRefs);
  const pathHint = panel.querySelector('.ann-path-hint');
  if (pathHint) {
    pathHint.addEventListener('click', () => {
      navigator.clipboard.writeText(framesPath).then(() => {
        pathHint.textContent = 'Copied!';
        setTimeout(() => { pathHint.textContent = framesPath; }, 1000);
      });
    });
  }
}

window.copyAnnotationRef = function(frameId) {
  navigator.clipboard.writeText(frameId).then(() => {
    const el = [...document.querySelectorAll('#annotation-refs-panel .ann-ref-item')]
      .find(item => item.dataset.frameId === frameId);
    if (el) { const orig = el.innerHTML; el.textContent = 'Copied!'; setTimeout(() => { el.innerHTML = orig; }, 1000); }
  });
};

window.copyAllAnnotationRefs = function() {
  const text = savedAnnotations.map(a => a.frameId).join(', ');
  navigator.clipboard.writeText(text).then(() => {
    const btn = document.querySelector('.ann-copy-all-btn');
    if (btn) { btn.textContent = 'Copied!'; setTimeout(() => { btn.textContent = 'Copy all'; }, 1000); }
  });
};

function showAnnotationResult(path) {
  // Legacy — server response with file path
  const el = document.getElementById('ann-result');
  if (el) {
    el.textContent = path;
    el.title = 'Click to copy: ' + path;
    el.onclick = () => {
      navigator.clipboard.writeText(path).then(() => {
        el.textContent = 'Copied!';
        setTimeout(() => { el.textContent = path; }, 1500);
      });
    };
  }
}

// Wire up annotation toolbar — works for both single-frame and clip annotation
function _activeAnnCanvas() { return annotationCanvas || clipAnnCanvas; }

document.getElementById('annotation-toolbar').addEventListener('click', (e) => {
  const ac = _activeAnnCanvas();
  const btn = e.target.closest('[data-tool]');
  if (btn && ac) {
    ac.setTool(btn.dataset.tool);
    document.querySelectorAll('.ann-btn').forEach(b => b.classList.toggle('active', b === btn));
    return;
  }
  const colorBtn = e.target.closest('[data-color]');
  if (colorBtn && ac) {
    ac.setColor(colorBtn.dataset.color);
    document.querySelectorAll('.ann-color').forEach(b => b.classList.toggle('active', b === colorBtn));
    return;
  }
  const thickBtn = e.target.closest('[data-thick]');
  if (thickBtn && ac) {
    ac.setThickness(parseInt(thickBtn.dataset.thick));
    document.querySelectorAll('.ann-thick').forEach(b => b.classList.toggle('active', b === thickBtn));
    return;
  }
});
document.getElementById('ann-undo').addEventListener('click', () => { const ac = _activeAnnCanvas(); if (ac) ac.undo(); });
document.getElementById('ann-redo').addEventListener('click', () => { const ac = _activeAnnCanvas(); if (ac) ac.redo(); });
document.getElementById('ann-clear').addEventListener('click', () => { const ac = _activeAnnCanvas(); if (ac) ac.clear(); });
document.getElementById('ann-save-btn').addEventListener('click', () => submitAnnotation('save'));
document.getElementById('ann-attach-btn').addEventListener('click', () => submitAnnotation('attach'));
document.getElementById('ann-submit-btn').addEventListener('click', () => {
  // In clip annotation mode this becomes the "Done" button — handled separately below.
  if (clipAnnotatingRange) return;
  submitAnnotation('send');
});
document.getElementById('ann-close').addEventListener('click', exitAnnotationMode);
document.getElementById('rec-annotate-btn').addEventListener('click', enterAnnotationMode);

// Submit on Enter in note field (Attach by default — works without an agent)
document.getElementById('ann-note').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { e.preventDefault(); submitAnnotation('attach'); }
  e.stopPropagation(); // Don't let key events bubble to global handlers
});

// Keyboard shortcuts for annotation and clip mode
document.addEventListener('keydown', (e) => {
  // Don't fire in input fields (except our handled note fields)
  if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
  const section = document.getElementById('recording-section');
  const sectionVisible = section && !section.classList.contains('hidden');

  if (e.key === 'a' && !e.ctrlKey && !e.metaKey && !e.altKey) {
    if (sectionVisible) {
      if (clipMode && !clipAnnotatingRange) {
        // In clip mode, 'a' starts annotate range flow
        startClipAnnotateRange();
      } else if (!clipMode) {
        if (annotationMode) exitAnnotationMode();
        else enterAnnotationMode();
      }
    }
    return;
  }
  if (e.key === 'c' && !e.ctrlKey && !e.metaKey && !e.altKey) {
    if (sectionVisible && !annotationMode && !clipMode) {
      enterClipMode();
    }
    return;
  }
  if (e.key === 'Escape') {
    if (clipAnnotatingRange) {
      cancelClipAnnotateRange();
      return;
    }
    if (clipMode) {
      exitClipMode();
      return;
    }
    if (annotationMode) {
      exitAnnotationMode();
      return;
    }
  }
  if (annotationMode && annotationCanvas) {
    if (e.key === 'z' && (e.ctrlKey || e.metaKey) && e.shiftKey) {
      e.preventDefault();
      annotationCanvas.redo();
    } else if (e.key === 'z' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      annotationCanvas.undo();
    }
  }
  if (clipAnnotatingRange && clipAnnCanvas) {
    if (e.key === 'z' && (e.ctrlKey || e.metaKey) && e.shiftKey) {
      e.preventDefault();
      clipAnnCanvas.redo();
    } else if (e.key === 'z' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      clipAnnCanvas.undo();
    }
  }
});

// ── Clip Mode ──

const CLIP_ANN_COLORS = ['#f38ba8', '#89b4fa', '#a6e3a1', '#f9e2af', '#cba6f7', '#fab387', '#89dceb'];
let clipMode = false;
let clipInSecs = null;
let clipOutSecs = null;
let clipKeyframes = [];
let clipAnnotationLayers = [];
let clipKeyframeMode = false;
let clipAnnotatingRange = false;
let clipAnnRangeStart = null;
let clipAnnRangeEnd = null;
let clipAnnCanvas = null;
let clipAnnSettingRange = false; // true while user is clicking to set annotation sub-range
let clipAnnHoverTime = null; // mouse hover time during annotation range setting
let clipAbort = false;
let clipCounter = 0;
let clipSelectedLayerId = null;
let clipLiveOverlayRaf = null;

function enterClipMode() {
  if (clipMode) return;
  if (annotationMode) exitAnnotationMode();
  if (!recPlayer || recPlayer.totalDuration === 0) return;

  clipMode = true;
  clipInSecs = null;
  clipOutSecs = null;
  clipKeyframes = [];
  clipAnnotationLayers = [];
  clipKeyframeMode = false;
  clipAnnotatingRange = false;
  clipAbort = false;
  clipSelectedLayerId = null;

  if (recPlayer.playing) recPlayer.pause();
  document.getElementById('clip-toolbar').classList.remove('hidden');
  document.getElementById('clip-ann-prompt').classList.add('hidden');
  document.getElementById('clip-progress').classList.add('hidden');
  document.getElementById('clip-preview-strip').classList.add('hidden');
  document.getElementById('clip-status').textContent = '';
  document.getElementById('clip-in-label').textContent = 'In: --';
  document.getElementById('clip-out-label').textContent = 'Out: --';
  document.getElementById('clip-duration-label').textContent = '';
  document.getElementById('clip-frame-count').textContent = '';
  document.getElementById('clip-keyframe-btn').classList.remove('active');
  if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
  renderClipMarkers();
  startClipLiveOverlay();
}

function exitClipMode() {
  if (!clipMode) return;
  clipMode = false;
  clipAnnotatingRange = false;
  clipKeyframeMode = false;
  clipAnnSettingRange = false;
  if (window._clipAnnResizeObserver) { window._clipAnnResizeObserver.disconnect(); window._clipAnnResizeObserver = null; }
  if (clipAnnCanvas) {
    clipAnnCanvas.deactivate();
    clipAnnCanvas = null;
  }
  document.getElementById('clip-toolbar').classList.add('hidden');
  document.getElementById('clip-preview-strip').classList.add('hidden');
  document.getElementById('annotation-toolbar').classList.add('hidden');
  document.getElementById('clip-ann-prompt').classList.add('hidden');
  _removeClipAnnBanner();
  // Restore annotation toolbar buttons in case clip mode was exited during annotation
  document.getElementById('ann-save-btn').style.display = '';
  const _attachBtn0 = document.getElementById('ann-attach-btn');
  if (_attachBtn0) _attachBtn0.style.display = '';
  document.getElementById('ann-submit-btn').textContent = 'Send';
  document.getElementById('ann-submit-btn').title = 'Save and inject into the live presence layer';
  if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
  clearClipMarkers();
  stopClipLiveOverlay();
}

// ── Timeline click interception for clip mode ──

// We intercept clicks on the timeline element itself.
// The RecordingPlayer constructor already has a click listener for seeking.
// We add a capturing listener that runs first and can prevent the seek.
document.getElementById('recording-timeline').addEventListener('click', (e) => {
  if (!clipMode) return;

  const timeline = document.getElementById('recording-timeline');
  const rect = timeline.getBoundingClientRect();
  const pct = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  const secs = pct * recPlayer.totalDuration;

  // Check if click is on an annotation range bar
  const annBar = e.target.closest('.timeline-ann-range');
  if (annBar) {
    const layerId = parseInt(annBar.dataset.layerId);
    if (clipAnnotatingRange || clipAnnSettingRange) return; // ignore while annotating
    editClipAnnotationLayer(layerId);
    e.stopImmediatePropagation();
    return;
  }

  // Check if click is on a keyframe mark
  const kfMark = e.target.closest('.timeline-keyframe-mark');
  if (kfMark && clipKeyframeMode) {
    const kfTime = parseFloat(kfMark.dataset.time);
    clipKeyframes = clipKeyframes.filter(t => Math.abs(t - kfTime) > 0.05);
    renderClipMarkers();
    updateClipFrameCount();
    e.stopImmediatePropagation();
    return;
  }

  // Setting annotation sub-range
  if (clipAnnSettingRange) {
    if (clipAnnRangeStart === null) {
      clipAnnRangeStart = secs;
      _updateClipAnnBanner('Click timeline for annotation end');
      renderClipMarkers();
    } else {
      clipAnnRangeEnd = secs;
      if (clipAnnRangeEnd < clipAnnRangeStart) {
        [clipAnnRangeStart, clipAnnRangeEnd] = [clipAnnRangeEnd, clipAnnRangeStart];
      }
      // Clamp to clip range
      if (clipInSecs !== null) clipAnnRangeStart = Math.max(clipAnnRangeStart, clipInSecs);
      if (clipOutSecs !== null) clipAnnRangeEnd = Math.min(clipAnnRangeEnd, clipOutSecs);
      clipAnnSettingRange = false;
      document.getElementById('clip-ann-prompt').classList.add('hidden');
      beginClipAnnotationDraw();
    }
    e.stopImmediatePropagation();
    return;
  }

  // Keyframe mode
  if (clipKeyframeMode) {
    if (clipInSecs !== null && clipOutSecs !== null && secs >= clipInSecs && secs <= clipOutSecs) {
      // Check if near existing keyframe — remove it
      const existing = clipKeyframes.findIndex(t => Math.abs(t - secs) < 0.05);
      if (existing >= 0) {
        clipKeyframes.splice(existing, 1);
      } else {
        clipKeyframes.push(secs);
      }
      renderClipMarkers();
      updateClipFrameCount();
    }
    e.stopImmediatePropagation();
    return;
  }

  // Setting in/out
  if (clipInSecs === null) {
    clipInSecs = secs;
    document.getElementById('clip-in-label').textContent = 'In: ' + fmtTime(secs);
    renderClipMarkers();
    // Also seek to show the frame
    recPlayer.seekToGlobal(secs);
  } else if (clipOutSecs === null) {
    clipOutSecs = secs;
    if (clipOutSecs < clipInSecs) {
      [clipInSecs, clipOutSecs] = [clipOutSecs, clipInSecs];
      document.getElementById('clip-in-label').textContent = 'In: ' + fmtTime(clipInSecs);
    }
    document.getElementById('clip-out-label').textContent = 'Out: ' + fmtTime(clipOutSecs);
    const dur = clipOutSecs - clipInSecs;
    document.getElementById('clip-duration-label').textContent = `(${fmtTime(dur)})`;
    renderClipMarkers();
    updateClipFrameCount();
  } else {
    // Both set: move whichever is nearer
    const distIn = Math.abs(secs - clipInSecs);
    const distOut = Math.abs(secs - clipOutSecs);
    if (distIn <= distOut) {
      clipInSecs = secs;
      if (clipInSecs > clipOutSecs) [clipInSecs, clipOutSecs] = [clipOutSecs, clipInSecs];
    } else {
      clipOutSecs = secs;
      if (clipOutSecs < clipInSecs) [clipInSecs, clipOutSecs] = [clipOutSecs, clipInSecs];
    }
    document.getElementById('clip-in-label').textContent = 'In: ' + fmtTime(clipInSecs);
    document.getElementById('clip-out-label').textContent = 'Out: ' + fmtTime(clipOutSecs);
    const dur = clipOutSecs - clipInSecs;
    document.getElementById('clip-duration-label').textContent = `(${fmtTime(dur)})`;
    renderClipMarkers();
    updateClipFrameCount();
    recPlayer.seekToGlobal(secs);
  }
  e.stopImmediatePropagation();
}, true); // capturing phase — runs before RecordingPlayer's click listener

// ── Clip marker dragging ──
{
  let draggingMarker = null; // 'in' or 'out'
  const timeline = document.getElementById('recording-timeline');

  timeline.addEventListener('pointerdown', (e) => {
    if (!clipMode) return;
    const marker = e.target.closest('.timeline-clip-marker');
    if (!marker) return;
    draggingMarker = marker.dataset.label === 'I' ? 'in' : 'out';
    e.preventDefault();
    timeline.setPointerCapture(e.pointerId);
  });

  timeline.addEventListener('pointermove', (e) => {
    if (!draggingMarker) return;
    const rect = timeline.getBoundingClientRect();
    const pct = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    const secs = pct * recPlayer.totalDuration;
    if (draggingMarker === 'in') {
      clipInSecs = Math.min(secs, clipOutSecs !== null ? clipOutSecs - 0.1 : recPlayer.totalDuration);
      document.getElementById('clip-in-label').textContent = 'In: ' + fmtTime(clipInSecs);
    } else {
      clipOutSecs = Math.max(secs, clipInSecs !== null ? clipInSecs + 0.1 : 0);
      document.getElementById('clip-out-label').textContent = 'Out: ' + fmtTime(clipOutSecs);
    }
    if (clipInSecs !== null && clipOutSecs !== null) {
      document.getElementById('clip-duration-label').textContent = `(${fmtTime(clipOutSecs - clipInSecs)})`;
    }
    renderClipMarkers();
    updateClipFrameCount();
  });

  timeline.addEventListener('pointerup', () => {
    if (draggingMarker) {
      draggingMarker = null;
      recPlayer.seekToGlobal(draggingMarker === 'in' ? clipInSecs : clipOutSecs);
    }
  });
}

// ── Annotation range preview on mousemove ──
{
  const timeline = document.getElementById('recording-timeline');
  timeline.addEventListener('mousemove', (e) => {
    if (!clipAnnSettingRange || clipAnnRangeStart === null || !recPlayer) return;
    const rect = timeline.getBoundingClientRect();
    const pct = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    clipAnnHoverTime = pct * recPlayer.totalDuration;
    renderClipMarkers();
  });
  timeline.addEventListener('mouseleave', () => {
    if (clipAnnSettingRange) {
      clipAnnHoverTime = null;
      renderClipMarkers();
    }
  });
}

// ── Render clip markers on timeline ──

function renderClipMarkers() {
  const timeline = document.getElementById('recording-timeline');
  // Remove old clip markers
  timeline.querySelectorAll('.timeline-clip-region, .timeline-clip-marker, .timeline-keyframe-mark, .timeline-ann-range, .timeline-ann-range-preview, .timeline-ann-range-active').forEach(el => el.remove());

  if (!recPlayer || recPlayer.totalDuration === 0) return;
  const total = recPlayer.totalDuration;

  // In marker
  if (clipInSecs !== null) {
    const marker = document.createElement('div');
    marker.className = 'timeline-clip-marker';
    marker.dataset.label = 'I';
    marker.style.left = (clipInSecs / total * 100) + '%';
    timeline.appendChild(marker);
  }

  // Out marker
  if (clipOutSecs !== null) {
    const marker = document.createElement('div');
    marker.className = 'timeline-clip-marker';
    marker.dataset.label = 'O';
    marker.style.left = (clipOutSecs / total * 100) + '%';
    timeline.appendChild(marker);
  }

  // Highlight region
  if (clipInSecs !== null && clipOutSecs !== null) {
    const region = document.createElement('div');
    region.className = 'timeline-clip-region';
    region.style.left = (clipInSecs / total * 100) + '%';
    region.style.width = ((clipOutSecs - clipInSecs) / total * 100) + '%';
    timeline.appendChild(region);
  }

  // Keyframe diamonds
  for (const kf of clipKeyframes) {
    const mark = document.createElement('div');
    mark.className = 'timeline-keyframe-mark';
    mark.style.left = (kf / total * 100) + '%';
    mark.dataset.time = kf;
    timeline.appendChild(mark);
  }

  // Annotation range bars
  for (const layer of clipAnnotationLayers) {
    const bar = document.createElement('div');
    bar.className = 'timeline-ann-range';
    if (clipSelectedLayerId === layer.id) bar.classList.add('selected');
    bar.style.left = (layer.startSecs / total * 100) + '%';
    bar.style.width = ((layer.endSecs - layer.startSecs) / total * 100) + '%';
    bar.style.background = layer.color;
    bar.dataset.layerId = layer.id;
    bar.title = `Annotation layer (${fmtTime(layer.startSecs)}-${fmtTime(layer.endSecs)}, ${layer.shapes.length} shapes)`;
    timeline.appendChild(bar);
  }

  // Annotation range preview (growing region from start to mouse hover)
  if (clipAnnSettingRange && clipAnnRangeStart !== null) {
    const end = clipAnnHoverTime !== null ? clipAnnHoverTime : clipAnnRangeStart;
    const left = Math.min(clipAnnRangeStart, end);
    const right = Math.max(clipAnnRangeStart, end);
    if (right > left) {
      const preview = document.createElement('div');
      preview.className = 'timeline-ann-range-preview';
      preview.style.left = (left / total * 100) + '%';
      preview.style.width = ((right - left) / total * 100) + '%';
      timeline.appendChild(preview);
    }
    // Start point marker (peach dot, not a clip marker)
    const startMark = document.createElement('div');
    startMark.className = 'timeline-clip-marker';
    startMark.dataset.label = '';
    startMark.style.left = (clipAnnRangeStart / total * 100) + '%';
    startMark.style.background = 'var(--peach)';
    startMark.style.width = '2px';
    timeline.appendChild(startMark);
  }

  // Active annotation range (visible during drawing)
  if (clipAnnotatingRange && clipAnnRangeStart !== null && clipAnnRangeEnd !== null) {
    const bar = document.createElement('div');
    bar.className = 'timeline-ann-range-active';
    bar.style.left = (clipAnnRangeStart / total * 100) + '%';
    bar.style.width = ((clipAnnRangeEnd - clipAnnRangeStart) / total * 100) + '%';
    timeline.appendChild(bar);
  }
}

function clearClipMarkers() {
  const timeline = document.getElementById('recording-timeline');
  timeline.querySelectorAll('.timeline-clip-region, .timeline-clip-marker, .timeline-keyframe-mark, .timeline-ann-range, .timeline-ann-range-preview, .timeline-ann-range-active').forEach(el => el.remove());
}

function updateClipFrameCount() {
  if (clipInSecs === null || clipOutSecs === null) {
    document.getElementById('clip-frame-count').textContent = '';
    return;
  }
  const timestamps = computeClipTimestamps(clipInSecs, clipOutSecs,
    parseInt(document.getElementById('clip-fps').value), clipKeyframes);
  document.getElementById('clip-frame-count').textContent = `~${timestamps.length} frames`;
}

function computeClipTimestamps(inSecs, outSecs, fps, keyframes) {
  const timestamps = [];
  const interval = 1.0 / fps;
  for (let t = inSecs; t <= outSecs + 0.001; t += interval) {
    timestamps.push({ t: Math.min(t, outSecs), isKeyframe: false, annotated: false });
  }
  // Ensure the last frame (outSecs) is included
  if (timestamps.length === 0 || Math.abs(timestamps[timestamps.length - 1].t - outSecs) > 0.05) {
    timestamps.push({ t: outSecs, isKeyframe: false, annotated: false });
  }
  // Add manual keyframes
  for (const kf of keyframes) {
    if (kf >= inSecs && kf <= outSecs) {
      if (!timestamps.some(ts => Math.abs(ts.t - kf) < 0.05)) {
        timestamps.push({ t: kf, isKeyframe: true, annotated: false });
      }
    }
  }
  timestamps.sort((a, b) => a.t - b.t);
  // Mark which timestamps have annotation layers
  for (const ts of timestamps) {
    ts.annotated = clipAnnotationLayers.some(l => ts.t >= l.startSecs && ts.t <= l.endSecs);
  }
  return timestamps;
}

// ── Annotation Layers on Clips ──

let clipAnnLayerIdCounter = 0;

function _showClipAnnBanner(text) {
  _removeClipAnnBanner();
  const wrap = document.getElementById('recording-player-wrap');
  if (!wrap) return;
  const banner = document.createElement('div');
  banner.className = 'clip-ann-banner';
  banner.id = 'clip-ann-banner';
  banner.textContent = text;
  wrap.appendChild(banner);
}

function _updateClipAnnBanner(text) {
  const el = document.getElementById('clip-ann-banner');
  if (el) el.textContent = text;
  else _showClipAnnBanner(text);
}

function _removeClipAnnBanner() {
  const el = document.getElementById('clip-ann-banner');
  if (el) el.remove();
}

function startClipAnnotateRange() {
  if (clipAnnotatingRange || clipAnnSettingRange) return;
  if (clipInSecs === null || clipOutSecs === null) return;
  clipAnnSettingRange = true;
  clipAnnRangeStart = null;
  clipAnnRangeEnd = null;
  clipAnnHoverTime = null;
  _showClipAnnBanner('Click timeline for annotation start');
}

function cancelClipAnnotateRange() {
  clipAnnSettingRange = false;
  clipAnnotatingRange = false;
  clipAnnRangeStart = null;
  clipAnnRangeEnd = null;
  clipAnnHoverTime = null;
  if (window._clipAnnResizeObserver) { window._clipAnnResizeObserver.disconnect(); window._clipAnnResizeObserver = null; }
  if (clipAnnCanvas) {
    clipAnnCanvas.deactivate();
    clipAnnCanvas = null;
  }
  _removeClipAnnBanner();
  document.getElementById('clip-ann-prompt').classList.add('hidden');
  document.getElementById('annotation-toolbar').classList.add('hidden');
  renderClipMarkers();
}

function beginClipAnnotationDraw() {
  clipAnnotatingRange = true;
  clipAnnHoverTime = null;
  // Seek video to start of annotation range
  recPlayer.seekToGlobal(clipAnnRangeStart);

  const video = document.getElementById('recording-video');
  const overlay = document.getElementById('recording-overlay');
  if (!video.videoWidth) { cancelClipAnnotateRange(); return; }

  clipAnnCanvas = new AnnotationCanvas(overlay, video);
  clipAnnCanvas.activate();

  // Re-align canvas on resize (split handle drag, window resize)
  if (window._clipAnnResizeObserver) window._clipAnnResizeObserver.disconnect();
  window._clipAnnResizeObserver = new ResizeObserver(() => {
    if (clipAnnCanvas && clipAnnotatingRange) clipAnnCanvas._alignToSource();
  });
  window._clipAnnResizeObserver.observe(document.getElementById('recording-player-wrap'));

  // Show annotation toolbar (reuse existing one, but swap save/send buttons)
  document.getElementById('annotation-toolbar').classList.remove('hidden');
  document.getElementById('ann-save-btn').style.display = 'none';
  const _attachBtn1 = document.getElementById('ann-attach-btn');
  if (_attachBtn1) _attachBtn1.style.display = 'none';
  document.getElementById('ann-submit-btn').textContent = 'Done';
  document.getElementById('ann-submit-btn').title = 'Save annotation layer';
  // In clip annotation mode the Send button is repurposed as "Done" — re-enable
  // unconditionally so it works even with no presence connected.
  document.getElementById('ann-submit-btn').disabled = false;

  _updateClipAnnBanner(`Annotating ${fmtTime(clipAnnRangeStart)}\u2013${fmtTime(clipAnnRangeEnd)} \u2014 Draw, then click Done`);

  renderClipMarkers();
}

function finishClipAnnotationLayer() {
  if (!clipAnnCanvas || !clipAnnotatingRange) return;
  const shapes = [...clipAnnCanvas.shapes]; // copy
  if (shapes.length === 0) {
    cancelClipAnnotateRange();
    return;
  }

  clipAnnLayerIdCounter++;
  const colorIdx = (clipAnnotationLayers.length) % CLIP_ANN_COLORS.length;
  clipAnnotationLayers.push({
    id: clipAnnLayerIdCounter,
    startSecs: clipAnnRangeStart,
    endSecs: clipAnnRangeEnd,
    shapes: shapes,
    color: CLIP_ANN_COLORS[colorIdx],
  });

  if (window._clipAnnResizeObserver) { window._clipAnnResizeObserver.disconnect(); window._clipAnnResizeObserver = null; }
  clipAnnCanvas.deactivate();
  clipAnnCanvas = null;
  clipAnnotatingRange = false;
  clipAnnRangeStart = null;
  clipAnnRangeEnd = null;
  _removeClipAnnBanner();
  document.getElementById('clip-ann-prompt').classList.add('hidden');
  document.getElementById('annotation-toolbar').classList.add('hidden');
  // Restore annotation toolbar buttons
  document.getElementById('ann-save-btn').style.display = '';
  const _attachBtn2 = document.getElementById('ann-attach-btn');
  if (_attachBtn2) _attachBtn2.style.display = '';
  document.getElementById('ann-submit-btn').textContent = 'Send';
  document.getElementById('ann-submit-btn').title = 'Save and inject into the live presence layer';
  if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();

  renderClipMarkers();
  updateClipFrameCount();
}

function editClipAnnotationLayer(layerId) {
  const layer = clipAnnotationLayers.find(l => l.id === layerId);
  if (!layer) return;

  clipSelectedLayerId = layerId;
  clipAnnRangeStart = layer.startSecs;
  clipAnnRangeEnd = layer.endSecs;
  clipAnnotatingRange = true;

  recPlayer.seekToGlobal(layer.startSecs);

  const video = document.getElementById('recording-video');
  const overlay = document.getElementById('recording-overlay');
  if (!video.videoWidth) { cancelClipAnnotateRange(); return; }

  clipAnnCanvas = new AnnotationCanvas(overlay, video);
  clipAnnCanvas.activate();
  // Restore shapes
  clipAnnCanvas.shapes = [...layer.shapes];
  clipAnnCanvas.render();

  document.getElementById('annotation-toolbar').classList.remove('hidden');
  document.getElementById('ann-save-btn').style.display = 'none';
  const _attachBtn3 = document.getElementById('ann-attach-btn');
  if (_attachBtn3) _attachBtn3.style.display = 'none';
  document.getElementById('ann-submit-btn').textContent = 'Done';
  document.getElementById('ann-submit-btn').title = 'Save annotation layer';
  document.getElementById('ann-submit-btn').disabled = false;

  _updateClipAnnBanner(`Editing layer ${fmtTime(layer.startSecs)}\u2013${fmtTime(layer.endSecs)} \u2014 Draw, then click Done`);

  renderClipMarkers();
}

function deleteClipAnnotationLayer(layerId) {
  clipAnnotationLayers = clipAnnotationLayers.filter(l => l.id !== layerId);
  if (clipSelectedLayerId === layerId) clipSelectedLayerId = null;
  renderClipMarkers();
  updateClipFrameCount();
}

// ── Live annotation overlay during clip playback ──

/** Align the overlay canvas exactly over the rendered video (handles letterboxing). */
function _alignOverlayToVideo(overlay, video) {
  const wrap = overlay.parentElement;
  if (!wrap || !video.videoWidth) return;
  const wrapRect = wrap.getBoundingClientRect();
  const vidRect = video.getBoundingClientRect();
  overlay.style.position = 'absolute';
  overlay.style.left = (vidRect.left - wrapRect.left) + 'px';
  overlay.style.top = (vidRect.top - wrapRect.top) + 'px';
  overlay.style.width = vidRect.width + 'px';
  overlay.style.height = vidRect.height + 'px';
}

function startClipLiveOverlay() {
  stopClipLiveOverlay();
  const overlay = document.getElementById('recording-overlay');
  const video = document.getElementById('recording-video');
  if (!overlay || !video) return;

  const ctx = overlay.getContext('2d');
  const tick = () => {
    if (!clipMode) { stopClipLiveOverlay(); return; }
    // Only draw overlay when not in annotation draw mode
    if (!clipAnnotatingRange && recPlayer && clipAnnotationLayers.length > 0) {
      const currentTime = recPlayer.globalTime();
      // Size canvas buffer to video resolution
      if (video.videoWidth && (overlay.width !== video.videoWidth || overlay.height !== video.videoHeight)) {
        overlay.width = video.videoWidth;
        overlay.height = video.videoHeight;
      }
      // Align overlay CSS to rendered video area (handles letterboxing + resize)
      _alignOverlayToVideo(overlay, video);
      ctx.clearRect(0, 0, overlay.width, overlay.height);
      for (const layer of clipAnnotationLayers) {
        if (currentTime >= layer.startSecs && currentTime <= layer.endSecs) {
          drawShapes(ctx, layer.shapes);
        }
      }
    } else if (!clipAnnotatingRange) {
      ctx.clearRect(0, 0, overlay.width, overlay.height);
    }
    clipLiveOverlayRaf = requestAnimationFrame(tick);
  };
  clipLiveOverlayRaf = requestAnimationFrame(tick);
}

function stopClipLiveOverlay() {
  if (clipLiveOverlayRaf) {
    cancelAnimationFrame(clipLiveOverlayRaf);
    clipLiveOverlayRaf = null;
  }
  const overlay = document.getElementById('recording-overlay');
  if (overlay) {
    const ctx = overlay.getContext('2d');
    ctx.clearRect(0, 0, overlay.width, overlay.height);
    // Reset overlay to full-parent coverage
    overlay.style.position = '';
    overlay.style.left = '';
    overlay.style.top = '';
    overlay.style.width = '';
    overlay.style.height = '';
  }
}

// ── Frame Extraction ──

async function* extractClipFrames(player, inSecs, outSecs, fps, keyframes, annotationLayers, quality) {
  quality = quality || 0.92;
  const video = player.video;
  const wasPlaying = player.playing;
  if (wasPlaying) player.pause();

  const timestamps = computeClipTimestamps(inSecs, outSecs, fps, keyframes);
  const captureCanvas = document.createElement('canvas');
  captureCanvas.width = video.videoWidth || 1280;
  captureCanvas.height = video.videoHeight || 720;
  const ctx = captureCanvas.getContext('2d');

  for (let i = 0; i < timestamps.length; i++) {
    if (clipAbort) break;
    const { t, isKeyframe, annotated } = timestamps[i];

    await player.seekToGlobalAsync(t);
    // Small delay for frame to render
    await new Promise(r => setTimeout(r, 50));

    ctx.clearRect(0, 0, captureCanvas.width, captureCanvas.height);
    ctx.drawImage(video, 0, 0, captureCanvas.width, captureCanvas.height);

    // Composite matching annotation layers
    let hasAnnotation = false;
    for (const layer of annotationLayers) {
      if (t >= layer.startSecs && t <= layer.endSecs && layer.shapes.length > 0) {
        drawShapes(ctx, layer.shapes);
        hasAnnotation = true;
      }
    }

    const blob = await new Promise(r => captureCanvas.toBlob(r, 'image/jpeg', quality));
    const b64 = await new Promise(r => {
      const reader = new FileReader();
      reader.onload = () => r(reader.result.split(',')[1]);
      reader.readAsDataURL(blob);
    });

    yield { index: i, total: timestamps.length, t, isKeyframe, annotated: hasAnnotation, b64, blob };
  }
}

async function generateClipPreview() {
  if (clipInSecs === null || clipOutSecs === null || !recPlayer) return;
  const strip = document.getElementById('clip-preview-strip');
  strip.innerHTML = '';
  strip.classList.remove('hidden');

  const fps = parseInt(document.getElementById('clip-fps').value);
  const gen = extractClipFrames(recPlayer, clipInSecs, clipOutSecs, fps,
    clipKeyframes, clipAnnotationLayers, 0.6);

  for await (const frame of gen) {
    if (clipAbort) break;
    const thumb = document.createElement('div');
    thumb.className = 'clip-preview-thumb';
    if (frame.isKeyframe) thumb.classList.add('keyframe');
    if (frame.annotated) thumb.classList.add('annotated');
    const img = document.createElement('img');
    img.src = URL.createObjectURL(frame.blob);
    thumb.appendChild(img);
    const time = document.createElement('div');
    time.className = 'thumb-time';
    time.textContent = fmtTime(frame.t);
    thumb.appendChild(time);
    thumb.addEventListener('click', () => recPlayer.seekToGlobal(frame.t));
    strip.appendChild(thumb);
  }
}

// `action` is one of: 'save' | 'attach' | 'send'
//   save:   register frames, do not inject, do not attach
//   attach: register frames and queue each frame as a pending attachment
//   send:   register frames and inject as a Video Clip into the live presence layer
async function submitClip(action) {
  if (action === true) action = 'send';
  if (action === false || action == null) action = 'save';

  if (clipInSecs === null || clipOutSecs === null || !recPlayer) return;
  const fps = parseInt(document.getElementById('clip-fps').value);
  const timestamps = computeClipTimestamps(clipInSecs, clipOutSecs, fps, clipKeyframes);

  if (timestamps.length > 200) {
    const ok = await showDashboardConfirm({
      title: 'Create large clip',
      message: `This clip has ${timestamps.length} frames. Max recommended is 200.`,
      warning: 'Large clips can take longer to generate and attach.',
      confirmLabel: 'Continue',
      danger: false,
    });
    if (!ok) return;
  }

  clipAbort = false;
  const progress = document.getElementById('clip-progress');
  const progressFill = document.getElementById('clip-progress-fill');
  const progressText = document.getElementById('clip-progress-text');
  const status = document.getElementById('clip-status');
  progress.classList.remove('hidden');
  status.textContent = '';

  clipCounter++;
  const clipId = `clip-${activeRecordingStream || 'rec'}-${clipCounter}`;
  const note = document.getElementById('clip-note').value.trim();
  const inject = action === 'send';
  const stream = activeRecordingStream || 'recording';
  const useMediaTunnel = dashboardMediaEditorRpcAvailable();
  let mediaStarted = false;
  let frameIndex = 0;
  const sentFrames = []; // { frameId, dataUrl } for 'attach'
  if (!useMediaTunnel && dashboardConnectModeEnabled()) {
    progress.classList.add('hidden');
    status.textContent = 'Media access unavailable';
    status.style.color = 'var(--red)';
    setTimeout(() => { status.textContent = ''; status.style.color = ''; }, 5000);
    dashboardMediaTransferFailed(dashboardMediaTunnelUnavailableError('clip creation'), 'clip creation');
    return;
  }

  try {
    const startPayload = {
      t: 'clip_start',
      clip_id: clipId,
      stream: stream,
      in_secs: clipInSecs,
      out_secs: clipOutSecs,
      fps: fps,
      total_frames: timestamps.length,
      note: note,
      inject: inject,
      annotation_layer_count: clipAnnotationLayers.length,
    };
    if (useMediaTunnel) {
      await requestDashboardMediaTunnel('api_media_clip_start', {
        clip_id: clipId,
        stream,
        in_secs: clipInSecs,
        out_secs: clipOutSecs,
        fps,
        total_frames: timestamps.length,
        note,
        inject,
        annotation_layer_count: clipAnnotationLayers.length,
      }, 'clip start');
      mediaStarted = true;
    } else {
      sendLegacyMediaEditorMessage(startPayload);
    }

    // Extract and send frames. Track frame IDs so 'attach' can queue them.
    const gen = extractClipFrames(recPlayer, clipInSecs, clipOutSecs, fps,
      clipKeyframes, clipAnnotationLayers, 0.92);
    for await (const frame of gen) {
      if (clipAbort) break;
      const pct = ((frame.index + 1) / frame.total * 100);
      progressFill.style.width = pct + '%';
      progressText.textContent = `Extracting ${frame.index + 1}/${frame.total}...`;

      const frameId = `${clipId}-f${String(frame.index).padStart(3, '0')}`;
      const framePayload = {
        t: 'clip_frame',
        clip_id: clipId,
        frame_id: frameId,
        frame_index: frame.index,
        timestamp_secs: frame.t,
        is_keyframe: frame.isKeyframe,
        annotated: frame.annotated,
        data: frame.b64,
      };
      if (useMediaTunnel) {
        await uploadDashboardMediaTunnel('api_media_clip_frame', {
          clip_id: clipId,
          frame_id: frameId,
          frame_index: frame.index,
          timestamp_secs: frame.t,
          is_keyframe: frame.isKeyframe,
          annotated: frame.annotated,
        }, dashboardControlBase64ToBytes(frame.b64), 'clip frame');
      } else {
        sendLegacyMediaEditorMessage(framePayload);
      }
      if (action === 'attach') {
        sentFrames.push({ frameId, b64: frame.b64 });
      }
      frameIndex++;

      // Yield to UI between frames
      await new Promise(r => setTimeout(r, 10));
    }

    if (clipAbort) {
      if (useMediaTunnel && mediaStarted) {
        try {
          await requestDashboardMediaTunnel('api_media_clip_cancel', { clip_id: clipId }, 'clip cancel');
        } catch (err) {
          console.warn('[dashboard-control] clip cancel failed', err);
        }
      }
      progress.classList.add('hidden');
      status.textContent = 'Cancelled';
      status.style.color = 'var(--red)';
      setTimeout(() => { status.textContent = ''; status.style.color = ''; }, 3000);
      return;
    }

    // Send clip_end
    progressText.textContent = 'Finalizing...';
    if (useMediaTunnel) {
      await requestDashboardMediaTunnel('api_media_clip_end', {
        clip_id: clipId,
        frames_sent: frameIndex,
      }, 'clip end');
    } else {
      sendLegacyMediaEditorMessage({
        t: 'clip_end',
        clip_id: clipId,
        frames_sent: frameIndex,
      });
    }

    // Add to pending attachments (after the server has registered them)
    if (action === 'attach') {
      for (const f of sentFrames) {
        addPendingAttachment({
          frameId: f.frameId,
          stream: `clip:${clipId}`,
          note: note,
          dataUrl: 'data:image/jpeg;base64,' + f.b64,
        });
      }
    }
  } catch (err) {
    if (useMediaTunnel && mediaStarted) {
      try {
        await requestDashboardMediaTunnel('api_media_clip_cancel', { clip_id: clipId }, 'clip cancel');
      } catch (cancelErr) {
        console.warn('[dashboard-control] clip cancel after failure failed', cancelErr);
      }
    }
    progress.classList.add('hidden');
    status.textContent = 'Failed';
    status.style.color = 'var(--red)';
    setTimeout(() => { status.textContent = ''; status.style.color = ''; }, 5000);
    dashboardMediaTransferFailed(err, 'clip creation');
    return;
  }

  progress.classList.add('hidden');
  document.getElementById('clip-note').value = '';
  const verb = action === 'send' ? 'Sent' : (action === 'attach' ? 'Attached' : 'Saved');
  status.textContent = `${verb} (${frameIndex} frames)`;
  status.style.color = 'var(--green)';
  setTimeout(() => { status.textContent = ''; status.style.color = ''; }, 5000);

  // Add clip reference
  addClipRef(clipId, frameIndex, inject);

  // Reset for another clip (stay in clip mode)
  clipInSecs = null;
  clipOutSecs = null;
  clipKeyframes = [];
  clipAnnotationLayers = [];
  clipSelectedLayerId = null;
  document.getElementById('clip-in-label').textContent = 'In: --';
  document.getElementById('clip-out-label').textContent = 'Out: --';
  document.getElementById('clip-duration-label').textContent = '';
  document.getElementById('clip-frame-count').textContent = '';
  document.getElementById('clip-preview-strip').classList.add('hidden');
  renderClipMarkers();
}

// ── Clip References ──

const savedClips = [];

function addClipRef(clipId, frameCount, injected) {
  savedClips.push({ clipId, frameCount, injected });
  if (savedClips.length > 100) savedClips.splice(0, savedClips.length - 100);
  renderClipRefs();
}

function renderClipRefs() {
  let panel = document.getElementById('clip-refs-panel');
  if (!panel) {
    panel = document.createElement('div');
    panel.id = 'clip-refs-panel';
    panel.className = 'annotation-refs-panel';
    const section = document.getElementById('recording-section');
    section.appendChild(panel);
  }
  panel.style.display = savedClips.length > 0 ? '' : 'none';

  // Same discipline as renderAnnotationRefs: ids ride data attributes and
  // real listeners, never string-interpolated inline handlers.
  const items = savedClips.map(c => {
    const badge = c.injected
      ? '<span style="color:var(--green);font-size:10px"> sent</span>'
      : '<span style="color:var(--overlay0);font-size:10px"> saved</span>';
    const info = `${escapeHtml(c.clipId)} (${escapeHtml(String(c.frameCount))} frames)`;
    return `<span class="ann-ref-item" title="Click to copy clip ID" data-clip-id="${escapeHtml(c.clipId)}">${info}${badge}</span>`;
  }).join('');

  panel.innerHTML = `
    <span style="font-size:11px;color:var(--subtext1);font-weight:600">Clips:</span>
    ${items}
    <button class="ann-copy-all-btn" title="Copy all clip IDs to clipboard">Copy all</button>
  `;
  panel.querySelectorAll('.ann-ref-item').forEach(el => {
    el.addEventListener('click', () => copyClipRef(el.dataset.clipId || ''));
  });
  panel.querySelector('.ann-copy-all-btn')?.addEventListener('click', copyAllClipRefs);
}

window.copyClipRef = function(clipId) {
  navigator.clipboard.writeText(clipId).then(() => {
    const el = [...document.querySelectorAll('#clip-refs-panel .ann-ref-item')]
      .find(item => item.dataset.clipId === clipId);
    if (el) { const orig = el.innerHTML; el.textContent = 'Copied!'; setTimeout(() => { el.innerHTML = orig; }, 1000); }
  });
};

window.copyAllClipRefs = function() {
  const text = savedClips.map(c => c.clipId).join(', ');
  navigator.clipboard.writeText(text).then(() => {
    const btn = document.querySelector('#clip-refs-panel .ann-copy-all-btn');
    if (btn) { btn.textContent = 'Copied!'; setTimeout(() => { btn.textContent = 'Copy all'; }, 1000); }
  });
};

document.getElementById('clip-preview-btn').addEventListener('click', generateClipPreview);
document.getElementById('clip-save-btn').addEventListener('click', () => submitClip('save'));
document.getElementById('clip-attach-btn').addEventListener('click', () => submitClip('attach'));
document.getElementById('clip-send-btn').addEventListener('click', () => submitClip('send'));

// ── Clip toolbar event wiring ──

document.getElementById('rec-clip-btn').addEventListener('click', enterClipMode);
document.getElementById('clip-close').addEventListener('click', exitClipMode);
document.getElementById('clip-keyframe-btn').addEventListener('click', () => {
  clipKeyframeMode = !clipKeyframeMode;
  document.getElementById('clip-keyframe-btn').classList.toggle('active', clipKeyframeMode);
});
document.getElementById('clip-ann-range-btn').addEventListener('click', startClipAnnotateRange);
document.getElementById('clip-clear-range').addEventListener('click', (e) => {
  e.preventDefault();
  clipInSecs = null;
  clipOutSecs = null;
  clipKeyframes = [];
  clipAnnotationLayers = [];
  clipSelectedLayerId = null;
  document.getElementById('clip-in-label').textContent = 'In: --';
  document.getElementById('clip-out-label').textContent = 'Out: --';
  document.getElementById('clip-duration-label').textContent = '';
  document.getElementById('clip-frame-count').textContent = '';
  renderClipMarkers();
});
document.getElementById('clip-fps').addEventListener('change', updateClipFrameCount);
document.getElementById('clip-cancel-btn').addEventListener('click', () => { clipAbort = true; });
document.getElementById('clip-note').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { e.preventDefault(); submitClip('attach'); }
  e.stopPropagation();
});

// Intercept annotation toolbar's "Done" button in clip mode
document.getElementById('ann-submit-btn').addEventListener('click', () => {
  if (clipAnnotatingRange) {
    if (clipSelectedLayerId !== null) {
      // Editing existing layer — update it
      const layer = clipAnnotationLayers.find(l => l.id === clipSelectedLayerId);
      if (layer && clipAnnCanvas) {
        layer.shapes = [...clipAnnCanvas.shapes];
      }
      clipAnnCanvas.deactivate();
      clipAnnCanvas = null;
      clipAnnotatingRange = false;
      clipSelectedLayerId = null;
      document.getElementById('clip-ann-prompt').classList.add('hidden');
      document.getElementById('annotation-toolbar').classList.add('hidden');
      document.getElementById('ann-save-btn').style.display = '';
      const _attachBtn4 = document.getElementById('ann-attach-btn');
      if (_attachBtn4) _attachBtn4.style.display = '';
      document.getElementById('ann-submit-btn').textContent = 'Send';
      document.getElementById('ann-submit-btn').title = 'Save and inject into the live presence layer';
      if (typeof updateAnnotationSendState === 'function') updateAnnotationSendState();
      renderClipMarkers();
      updateClipFrameCount();
    } else {
      finishClipAnnotationLayer();
    }
  }
});

// Right-click on annotation range bar to delete
document.getElementById('recording-timeline').addEventListener('contextmenu', (e) => {
  const annBar = e.target.closest('.timeline-ann-range');
  if (annBar && clipMode) {
    e.preventDefault();
    const layerId = parseInt(annBar.dataset.layerId);
    deleteClipAnnotationLayer(layerId);
  }
});
