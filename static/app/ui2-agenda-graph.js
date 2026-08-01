// Agenda tab Graph lens (redesign slice B): the constellation — a canvas
// force layout of the non-retired ledger drawing placement (part_of),
// adjacency (relates_to), and dependency (relies_on) as one orbitable
// structure. Registered in AGENDA_LENSES (ui2-agenda-cards.js) between
// "By hub" and "Questions" as a custom-surface lens: render() owns
// #ag2-groups, deactivate() stops the loop. Data and derivations come
// from ui2-agenda.js (agendaItems, agendaItemIsBlocked, agendaEffectState);
// clicking a node opens the slice-A inspector (agendaOpenInspector).
//
// Accessibility: the canvas is a decorative projection, never sole
// access — every node it draws stays reachable as an ordinary card via
// the other lenses (Open / By hub / Questions / Archive), so the canvas
// carries aria-hidden="true" and screen readers lose nothing. Under
// prefers-reduced-motion the auto-orbit, the animated dependency dashes,
// and the suspended-ring pulse are all disabled (static rendering only).
//
// Item-authored text (titles, kinds, statuses) is drawn with fillText
// only — inherently inert pixels. The panel chrome around the canvas is
// static markup carrying no item text, so nothing here renders item text
// as HTML.
//
// Lifecycle contract (the ratified hard gates): the rAF loop stops
// completely — zero background frames — on lens switch away (the render
// pass's deactivate sweep), on agenda tab hide (a class observer on
// #tab-agenda; the router has no per-tab hide hook), and on
// document.visibilitychange → hidden. Per-activation listeners, timers,
// and observers are all removed by agendaGraphTeardown; the single
// module-level visibilitychange listener below is the stop/resume
// conduit itself and must outlive any one activation to resume it.

// NO node cap (owner-ruled 2026-07-31, after 180 and then 420 both got
// hit within a day): density is a feature, and every projection always
// renders. Cost stays flat through level of detail instead of refusal —
// node glow comes from pre-baked sprites (drawImage, ~two orders
// cheaper than per-arc shadowBlur), and past REPULSION_LOD nodes the
// settle pass strides its O(n²) pair loop with a rotating offset
// (approximate physics, same look, same 260-iteration budget).
const AGENDA_GRAPH_REPULSION_LOD = 600;
// Design-parity settle budget, amortized: the prototype ran 260
// synchronous O(n²) relaxation iterations per relayout; here the same
// total is spread over frames (agendaGraphSettleBudget per rAF) so a
// relayout never blocks a paint.
const AGENDA_GRAPH_SETTLE_ITERATIONS = 260;

// Layout + interaction state. Positions and the camera live at module
// level so re-renders (inspector open, event-lane merges) never reset
// the orbit or re-scatter surviving nodes.
let agendaGraphNodes = [];
let agendaGraphLinks = [];
let agendaGraphKey = '';
let agendaGraphSettleLeft = 0;
let agendaGraphRaf = null;
let agendaGraphCanvas = null;
let agendaGraphCanvasHooks = null;
let agendaGraphPaneObserver = null;
let agendaGraphAutoTimer = null;
let agendaGraphCam = {
  yaw: 0.6, pitch: -0.34, auto: true, zoom: 1, panX: 0, panY: 0,
};
// Projection state: 'all' (whole non-retired ledger), 'hubs' (the hub
// overview — automatic past the node cap, or chosen), 'focus' (one
// hub's placed subtree). The render pass decides the projection from
// the chip row + focus + cap; build/draw only read it. `territory`
// overlays derived file/dir satellite nodes on the 'all'/'focus' pools.
let agendaGraphFocus = null;
let agendaGraphProjection = 'all';
let agendaGraphChosen = null; // null = 'all'; 'hubs' only when chosen
let agendaGraphTerritory = false;
let agendaGraphSubtreeTerr = new Map();
let agendaGraphTerrStats = { shown: 0, total: 0 };
// Arrangement mode over the current pool: 'orbit' (the default force
// constellation) or one of the ontology lenses — 'flow' (relies_on
// layered left→right), 'time' (tree rings: log-scaled age as radius,
// now at the center), 'files' (clustered by directory-prefix
// territory), 'actor' (clustered by parking provenance),
// 'attention' (needs-you gravity).
// Modes bias the layout and emphasis only; pool, links, picking, caps,
// and card-lens parity are untouched. Build computes the active mode's
// per-node targets; relax applies them as one extra spring.
let agendaGraphMode = 'orbit';
const AGENDA_GRAPH_MODES = [
  ['flow', 'flow', 'dependency flow — prerequisites left, dependents right'],
  ['time', 'time', 'tree rings of age — now at the center, log rings out to the oldest'],
  ['files', 'files', 'clustered by the code they touch — territory shows the files themselves'],
  ['actor', 'actor', 'clustered by who parked it'],
  ['attention', 'attention', 'needs-you gravity — attention items pull center'],
];
let agendaGraphTimeMarks = [];
let agendaGraphTimeOldest = 0;
let agendaGraphTimeCheckAt = 0;
let agendaGraphActorMarks = [];
let agendaGraphFilesMarks = [];
let agendaGraphAttnCount = 0;
let agendaGraphFullscreen = false;
let agendaGraphEscHook = null;

// Theater-mode fullscreen: CSS-only (`position: fixed` on the panel) —
// the packaged app's WKWebView does not reliably grant the
// element-fullscreen API, and the CSS route behaves identically on
// every frontend. The draw loop re-measures the canvas per frame, so
// no canvas code changes; the wire pass re-applies the state across
// re-renders. ESC exits (deferring to overlays that already consumed
// the key).
function agendaGraphSetFullscreen(on) {
  agendaGraphFullscreen = on;
  if (!on) agendaGraphRemoveEsc();
  agendaRenderTab();
}

// While graph-fullscreen is active a marker class on <html> lifts the
// agenda inspector above the fixed panel (see ui2-agenda-inspector.css)
// so node-click keeps its full behavior — inspector over constellation.
function agendaGraphSyncFsMarker() {
  document.documentElement.classList.toggle('ag2-graph-fs', agendaGraphFullscreen);
}

function agendaGraphEnsureEsc() {
  if (agendaGraphEscHook) return;
  agendaGraphEscHook = (e) => {
    if (e.key !== 'Escape' || e.defaultPrevented) return;
    // Polite ordering: an open inspector consumes the first ESC;
    // fullscreen exits on the next.
    const insp = document.getElementById('ag2-inspector');
    if (insp && insp.classList.contains('open')) {
      agendaCloseInspector();
      return;
    }
    agendaGraphSetFullscreen(false);
  };
  document.addEventListener('keydown', agendaGraphEscHook);
}

function agendaGraphRemoveEsc() {
  if (agendaGraphEscHook) {
    document.removeEventListener('keydown', agendaGraphEscHook);
    agendaGraphEscHook = null;
  }
}
let agendaGraphMouse = { x: -1e4, y: -1e4, down: false, moved: 0 };
let agendaGraphHover = null;
let agendaGraphPalCache = null;
let agendaGraphPalAt = 0;
let agendaGraphMotionQuery = null;

function agendaGraphReducedMotion() {
  if (!agendaGraphMotionQuery) {
    agendaGraphMotionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
  }
  return agendaGraphMotionQuery.matches;
}

// The graph's pool: every non-retired item, deliberately unfiltered —
// the constellation shows the whole topology; search and the lens-bar
// filter chips keep applying to the card lenses only. A focused hub
// narrows the pool to its placed subtree (self included); a focus whose
// item left the ledger clears itself.
function agendaGraphPoolItems() {
  const all = (agendaItems || []).filter((item) => item.status !== 'retired');
  if (agendaGraphFocus) {
    if (!all.some((item) => item.id === agendaGraphFocus)) {
      agendaGraphFocus = null;
    } else {
      const desc = agendaDescendantIds(agendaGraphFocus);
      return all.filter(
        (item) => item.id === agendaGraphFocus || desc.has(item.id),
      );
    }
  }
  return all;
}

// The chosen hubs projection: the constellation
// becomes a hub overview — only items with placed children — and a
// click focuses one hub's subtree. The projection stays functional at
// any ledger size a real taxonomy produces.
function agendaGraphHubOverviewItems(all) {
  const parents = new Set();
  all.forEach((item) => {
    if (item.part_of) parents.add(item.part_of.parent_id);
  });
  return all.filter((item) => parents.has(item.id));
}

// The active projection's node pool — the one truth build and draw use.
function agendaGraphProjectionItems() {
  const pool = agendaGraphPoolItems();
  if (agendaGraphProjection === 'hubs' && !agendaGraphFocus) {
    return agendaGraphHubOverviewItems(pool);
  }
  return pool;
}

// ---- Lens surface (the AGENDA_LENSES render/deactivate pair) ----

function agendaGraphRenderLens(host) {
  // Resolve the projection: an explicit chip choice wins; focus
  // overrides the pool. Every projection renders at any size — cost is
  // handled by LOD (sprite glow, strided repulsion), never refusal.
  let items = agendaGraphPoolItems();
  if (agendaGraphFocus) {
    agendaGraphProjection = 'focus';
  } else if (agendaGraphChosen === 'hubs') {
    const hubs = agendaGraphHubOverviewItems(items);
    if (hubs.length) {
      items = hubs;
      agendaGraphProjection = 'hubs';
    } else {
      agendaGraphProjection = 'all';
    }
  } else {
    agendaGraphProjection = 'all';
  }
  if (!items.length) {
    agendaGraphFullscreen = false;
    agendaGraphTeardown();
    host.innerHTML = `<div class="ag2-empty">
      <div class="ag2-empty-glyph">◍</div>
      <div class="ag2-empty-title">Nothing to map yet</div>
      <div class="ag2-empty-hint">Park something above — the constellation draws every non-retired item.</div>
    </div>`;
    return;
  }
  let canvas = host.querySelector('#ag2-graph-canvas');
  if (!canvas) {
    host.innerHTML = agendaGraphPanelHtml();
    canvas = host.querySelector('#ag2-graph-canvas');
  }
  agendaGraphWireChrome(host);
  agendaGraphBindCanvas(canvas);
  agendaGraphEnsureLoop();
}

// Per-projection static chrome: the projection chip row and the hint
// line. Static strings only — no item text ever lands in this markup
// (the painted badge carries titles as inert pixels).
function agendaGraphWireChrome(host) {
  const panel = host.querySelector('.ag2-graph-panel');
  if (panel) panel.classList.toggle('fullscreen', agendaGraphFullscreen);
  agendaGraphSyncFsMarker();
  if (agendaGraphFullscreen) agendaGraphEnsureEsc();
  else agendaGraphRemoveEsc();
  const hintLine = host.querySelector('#ag2-graph-hint');
  if (hintLine) {
    const modeHint = (AGENDA_GRAPH_MODES.find(([m]) => m === agendaGraphMode) || [])[2];
    hintLine.textContent = modeHint
      || (agendaGraphProjection === 'hubs'
        ? 'hub overview — click a hub to focus its subtree'
        : agendaGraphProjection === 'focus'
          ? (agendaGraphTerritory
            ? 'focused territory — squares are files/dirs; click one to open its newest carrier'
            : 'focused subtree — double-click empty space to clear')
          : 'drag to orbit · wheel to zoom · shift-drag to pan · click a node to open it · double-click a hub to focus');
  }
  const inHubs = agendaGraphProjection === 'hubs';
  const chips = host.querySelectorAll('.ag2-graph-projchip');
  chips.forEach((chip) => {
    const kind = chip.dataset.proj;
    if (kind === 'hubs') {
      chip.classList.toggle('active', inHubs);
      chip.disabled = false;
      chip.title = 'only the hubs — click one to focus its subtree';
      chip.onclick = () => {
        agendaGraphChosen = 'hubs';
        agendaGraphFocus = null;
        agendaRenderTab();
      };
    } else if (kind === 'all') {
      chip.classList.toggle('active', !inHubs && agendaGraphProjection === 'all');
      chip.disabled = false;
      chip.title = 'every non-retired item';
      chip.onclick = () => {
        agendaGraphChosen = 'all';
        agendaGraphFocus = null;
        agendaRenderTab();
      };
    } else if (kind === 'terr') {
      chip.classList.toggle('active', agendaGraphTerritory && !inHubs);
      chip.disabled = inHubs;
      chip.title = inHubs
        ? 'territory needs a concrete pool — focus a hub or pick everything'
        : 'overlay file/dir satellites from the pool’s refs';
      chip.onclick = () => {
        agendaGraphTerritory = !agendaGraphTerritory;
        agendaRenderTab();
      };
    } else if (kind === 'clearfocus') {
      chip.hidden = !agendaGraphFocus;
      chip.classList.add('active');
      chip.disabled = false;
      chip.title = 'clear the focused hub';
      chip.onclick = () => {
        agendaGraphFocus = null;
        agendaRenderTab();
      };
    } else if (kind && kind.startsWith('mode-')) {
      const mode = kind.slice(5);
      chip.classList.toggle('active', agendaGraphMode === mode);
      chip.disabled = false;
      chip.title = agendaGraphMode === mode
        ? 'back to the orbit arrangement'
        : (AGENDA_GRAPH_MODES.find(([m]) => m === mode) || [])[2] || mode;
      chip.onclick = () => {
        agendaGraphMode = agendaGraphMode === mode ? 'orbit' : mode;
        agendaRenderTab();
      };
    } else if (kind === 'full') {
      chip.textContent = agendaGraphFullscreen ? '✕ exit full' : '⛶ full';
      chip.classList.toggle('active', agendaGraphFullscreen);
      chip.disabled = false;
      chip.title = agendaGraphFullscreen
        ? 'exit fullscreen (esc works too)'
        : 'fill the window with the map';
      chip.onclick = () => {
        agendaGraphSetFullscreen(!agendaGraphFullscreen);
      };
    }
  });
}

function agendaGraphLegendChip(swatchClass, label) {
  return `<span class="ag2-graph-chip"><span class="ag2-graph-swatch ${swatchClass}"></span>${label}</span>`;
}

function agendaGraphPanelHtml() {
  // Static chrome only — no item text lands in this markup, ever; the
  // canvas is aria-hidden because every node stays reachable through the
  // card lenses (see the fragment header).
  return `<div class="ag2-graph-panel">
    <canvas id="ag2-graph-canvas" aria-hidden="true"></canvas>
    <div class="ag2-graph-eyebrow">
      <div class="ag2-graph-eyebrow-title">Constellation</div>
      <div class="ag2-graph-eyebrow-sub">placement · adjacency · dependencies</div>
    </div>
    <div class="ag2-graph-hint" id="ag2-graph-hint">drag to orbit · click a node to open it</div>
    <div class="ag2-graph-projrow">
      <button type="button" class="ag2-graph-projchip" data-proj="hubs">hubs</button>
      <button type="button" class="ag2-graph-projchip" data-proj="all">everything</button>
      <button type="button" class="ag2-graph-projchip" data-proj="terr">territory</button>
      <button type="button" class="ag2-graph-projchip" data-proj="clearfocus" hidden>focused ✕</button>
      <span class="ag2-graph-projdiv"></span>
      ${AGENDA_GRAPH_MODES.map(([mode, label]) =>
    `<button type="button" class="ag2-graph-projchip" data-proj="mode-${mode}">${label}</button>`).join('\n      ')}
      <span class="ag2-graph-projdiv"></span>
      <button type="button" class="ag2-graph-projchip" data-proj="full">⛶ full</button>
    </div>
    <div class="ag2-graph-legend">
      ${agendaGraphLegendChip('s-dot t-iris', 'open')}
      ${agendaGraphLegendChip('s-dot t-amber', 'question')}
      ${agendaGraphLegendChip('s-dot t-green', 'done')}
      ${agendaGraphLegendChip('s-ring t-rose', 'blocked')}
      ${agendaGraphLegendChip('s-ring t-green', 'standing run')}
      ${agendaGraphLegendChip('s-ring t-terr', 'territory')}
      ${agendaGraphLegendChip('s-sq t-file', 'file')}
      ${agendaGraphLegendChip('s-sq t-dir', 'dir')}
      ${agendaGraphLegendChip('s-line t-place', 'filed under')}
      ${agendaGraphLegendChip('s-line t-rel', 'see-also')}
      ${agendaGraphLegendChip('s-line t-typed', 'typed →')}
      ${agendaGraphLegendChip('s-line t-dep', 'waits on')}
    </div>
  </div>`;
}

// ---- Canvas interaction (drag to orbit, click to open) ----

function agendaGraphBindCanvas(canvas) {
  if (!canvas || (agendaGraphCanvas === canvas && agendaGraphCanvasHooks)) return;
  agendaGraphUnbindCanvas();
  agendaGraphCanvas = canvas;
  agendaGraphMouse.x = -1e4;
  agendaGraphMouse.y = -1e4;
  agendaGraphMouse.down = false;
  agendaGraphMouse.moved = 0;
  const hooks = {
    move: (e) => {
      const rect = canvas.getBoundingClientRect();
      const m = agendaGraphMouse;
      const nx = e.clientX - rect.left;
      const ny = e.clientY - rect.top;
      if (m.down) {
        if (e.shiftKey) {
          // Shift-drag pans in screen space (zoom's natural partner).
          agendaGraphCam.panX += nx - m.x;
          agendaGraphCam.panY += ny - m.y;
        } else {
          agendaGraphCam.yaw += (nx - m.x) * 0.005;
          agendaGraphCam.pitch = Math.max(-1.2, Math.min(1.2,
            agendaGraphCam.pitch + (ny - m.y) * 0.004));
        }
        m.moved += Math.abs(nx - m.x) + Math.abs(ny - m.y);
        agendaGraphCam.auto = false;
      }
      m.x = nx;
      m.y = ny;
    },
    down: () => {
      agendaGraphMouse.down = true;
      agendaGraphMouse.moved = 0;
    },
    up: () => {
      const m = agendaGraphMouse;
      // A press that traveled under ~6px is a click. In the hub
      // overview a click focuses the hub's subtree; everywhere else it
      // opens the node in the slice-A inspector.
      if (m.down && m.moved < 6 && agendaGraphHover) {
        const satNode = agendaGraphHover.startsWith('terr|')
          ? agendaGraphNodes.find((n) => n.id === agendaGraphHover)
          : null;
        if (satNode && satNode.sat) {
          // Satellites are view-materialized, not store items: the
          // click opens the newest carrying item.
          agendaOpenInspector(satNode.sat.carrier);
        } else if (agendaGraphProjection === 'hubs') {
          agendaGraphFocus = agendaGraphHover;
          agendaRenderTab();
        } else {
          agendaOpenInspector(agendaGraphHover);
        }
      }
      m.down = false;
      agendaGraphArmAutoResume();
    },
    dbl: () => {
      // Double-click a hub to focus its subtree; double-click empty
      // space to reset a zoom/pan first, then to clear an active focus.
      if (agendaGraphHover && agendaChildrenOf(agendaGraphHover).length) {
        agendaGraphFocus = agendaGraphHover;
        agendaRenderTab();
      } else if (!agendaGraphHover) {
        const cam = agendaGraphCam;
        if (cam.zoom !== 1 || cam.panX || cam.panY) {
          cam.zoom = 1;
          cam.panX = 0;
          cam.panY = 0;
        } else if (agendaGraphFocus) {
          agendaGraphFocus = null;
          agendaRenderTab();
        }
      }
    },
    wheel: (e) => {
      // Wheel / trackpad-pinch (ctrl-wheel) zoom, anchored to the
      // cursor: the world point under the pointer stays put via the
      // screen-space pan. preventDefault keeps the page from scrolling
      // — scoped to the canvas, removed on unbind.
      e.preventDefault();
      const rect = canvas.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const cam = agendaGraphCam;
      const factor = Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0022));
      const next = Math.max(0.35, Math.min(3.5, cam.zoom * factor));
      const ratio = next / cam.zoom;
      const ax = rect.width / 2;
      const ay = rect.height / 2 + 8;
      cam.panX = mx - ax - (mx - ax - cam.panX) * ratio;
      cam.panY = my - ay - (my - ay - cam.panY) * ratio;
      cam.zoom = next;
      cam.auto = false;
      agendaGraphArmAutoResume();
    },
    leave: () => {
      const wasDown = agendaGraphMouse.down;
      agendaGraphMouse.x = -1e4;
      agendaGraphMouse.y = -1e4;
      agendaGraphMouse.down = false;
      agendaGraphHover = null;
      // A drag that ran off the panel still hands the orbit back.
      if (wasDown) agendaGraphArmAutoResume();
    },
  };
  canvas.addEventListener('mousemove', hooks.move);
  canvas.addEventListener('mousedown', hooks.down);
  canvas.addEventListener('mouseup', hooks.up);
  canvas.addEventListener('mouseleave', hooks.leave);
  canvas.addEventListener('dblclick', hooks.dbl);
  canvas.addEventListener('wheel', hooks.wheel, { passive: false });
  agendaGraphCanvasHooks = hooks;
}

function agendaGraphUnbindCanvas() {
  if (agendaGraphCanvas && agendaGraphCanvasHooks) {
    const canvas = agendaGraphCanvas;
    const hooks = agendaGraphCanvasHooks;
    canvas.removeEventListener('mousemove', hooks.move);
    canvas.removeEventListener('mousedown', hooks.down);
    canvas.removeEventListener('mouseup', hooks.up);
    canvas.removeEventListener('mouseleave', hooks.leave);
    canvas.removeEventListener('dblclick', hooks.dbl);
    canvas.removeEventListener('wheel', hooks.wheel);
  }
  agendaGraphCanvas = null;
  agendaGraphCanvasHooks = null;
}

// Auto-orbit resumes ~4s after the last interaction — never under
// reduced motion, where there is no auto-orbit to resume.
function agendaGraphArmAutoResume() {
  if (agendaGraphReducedMotion()) return;
  if (agendaGraphAutoTimer) clearTimeout(agendaGraphAutoTimer);
  agendaGraphAutoTimer = setTimeout(() => {
    agendaGraphAutoTimer = null;
    agendaGraphCam.auto = true;
  }, 4000);
}

// ---- Loop lifecycle ----

function agendaGraphShouldRun() {
  return agendaLens === 'graph'
    && !document.hidden
    && agendaTabVisible()
    && !!agendaGraphCanvas
    && agendaGraphCanvas.isConnected;
}

function agendaGraphEnsureLoop() {
  if (agendaGraphRaf !== null || !agendaGraphShouldRun()) return;
  const pane = document.getElementById('tab-agenda');
  if (pane && !agendaGraphPaneObserver) {
    // Tab-hide stop: the router only toggles pane classes on tab
    // switches (no per-tab hide callback exists), so the pane's class
    // list is the authoritative hide signal.
    agendaGraphPaneObserver = new MutationObserver(() => {
      if (!agendaTabVisible()) agendaGraphTeardown();
    });
    agendaGraphPaneObserver.observe(pane, { attributes: true, attributeFilter: ['class'] });
  }
  const loop = (ts) => {
    // Failsafe backstop — the event-driven stops below are the real
    // teardown paths; this guarantees a stray frame can never re-arm.
    if (!agendaGraphShouldRun()) {
      agendaGraphTeardown();
      return;
    }
    agendaGraphRaf = requestAnimationFrame(loop);
    agendaGraphDraw(ts);
  };
  agendaGraphRaf = requestAnimationFrame(loop);
}

// The full stop: cancels the loop and removes every per-activation
// listener, timer, and observer (the panel DOM itself belongs to the
// render pass, which replaces it on the next paint). Safe to call
// repeatedly.
function agendaGraphTeardown() {
  if (agendaGraphRaf !== null) {
    cancelAnimationFrame(agendaGraphRaf);
    agendaGraphRaf = null;
  }
  if (agendaGraphAutoTimer) {
    clearTimeout(agendaGraphAutoTimer);
    agendaGraphAutoTimer = null;
  }
  if (agendaGraphPaneObserver) {
    agendaGraphPaneObserver.disconnect();
    agendaGraphPaneObserver = null;
  }
  agendaGraphUnbindCanvas();
  agendaGraphRemoveEsc();
  document.documentElement.classList.remove('ag2-graph-fs');
  agendaGraphMouse.down = false;
  agendaGraphMouse.moved = 0;
  agendaGraphHover = null;
}

// ---- Time lens (tree rings of age, anchored at NOW) ----

// Radius is log-scaled age: r = INNER + STEP·log2(1 + age/1h). The busy
// recent hours and days get generous space and a years-old tail still
// fits on the same disc; the mapping is fixed (never fitted to the
// pool), so a given ring always means the same age and the empty bands
// between rings are information, not waste.
const AGENDA_GRAPH_TIME_HOUR = 3600e3;
const AGENDA_GRAPH_TIME_INNER = 30;
const AGENDA_GRAPH_TIME_STEP = 15;
// The age boundaries worth naming: [hours, label, minor]. Minor rings
// paint fainter and their labels yield first when zoom packs them.
const AGENDA_GRAPH_TIME_LADDER = [
  [1, '1h', false], [6, '6h', true], [24, '1d', false], [72, '3d', true],
  [168, '1w', false], [336, '2w', true], [720, '1mo', false],
  [2190, '3mo', false], [4380, '6mo', true], [8760, '1y', false],
  [17520, '2y', false], [43800, '5y', false],
];

// Density-adaptive scale: the log mapping is fixed in SHAPE, but the
// whole disc dilates with population (area ~ item count) so a
// 300-item fortnight gets the room a 50-item fortnight never needed.
// Set once per build; marks, targets, and the drift tick all read the
// radius through this one function, so everything dilates together.
let agendaGraphTimeScale = 1;
function agendaGraphTimeRadius(ageMs) {
  return (AGENDA_GRAPH_TIME_INNER + AGENDA_GRAPH_TIME_STEP
    * Math.log2(1 + Math.max(0, ageMs) / AGENDA_GRAPH_TIME_HOUR)) * agendaGraphTimeScale;
}

// The ladder rings inside the pool's age span, plus a dashed ring at
// the oldest item itself labeled with its real date. Ladder rings that
// would hug the oldest ring are dropped so the outer labels never
// stack.
function agendaGraphTimeRebuildMarks(now) {
  const oldestAge = Math.max(0, now - agendaGraphTimeOldest);
  const outerR = agendaGraphTimeRadius(oldestAge);
  const marks = [];
  AGENDA_GRAPH_TIME_LADDER.forEach(([hours, label, minor]) => {
    const age = hours * AGENDA_GRAPH_TIME_HOUR;
    const r = agendaGraphTimeRadius(age);
    if (age <= oldestAge && outerR - r > 6) {
      marks.push({ r, label, minor, oldest: false });
    }
  });
  const d = new Date(agendaGraphTimeOldest || now);
  const sameYear = d.getFullYear() === new Date(now).getFullYear();
  marks.push({
    r: outerR,
    label: `oldest · ${d.toISOString().slice(sameYear ? 5 : 0, 10)}`,
    minor: false,
    oldest: true,
  });
  agendaGraphTimeMarks = marks;
}

// Ages advance while the tab sits open: every few seconds compare each
// node's ASSIGNED ring against its current age, and once the fastest
// mover has drifted a visible amount, re-target the springs, refresh
// the marks, and re-arm a small settle budget. A fresh item migrates
// off the center honestly; a quiet ledger re-arms roughly never.
// (Assigned-vs-computed keeps this independent of the springs'
// equilibrium error, which would otherwise re-arm every check.)
function agendaGraphTimeTick() {
  const now = Date.now();
  if (now - agendaGraphTimeCheckAt < 5000) return;
  agendaGraphTimeCheckAt = now;
  let drift = 0;
  agendaGraphNodes.forEach((n) => {
    if (n.sat || n.born === undefined) return;
    const d = Math.abs(agendaGraphTimeRadius(now - n.born) - n.tr);
    if (d > drift) drift = d;
  });
  if (drift <= 2.5) return;
  agendaGraphNodes.forEach((n) => {
    if (n.sat || n.born === undefined) return;
    n.tr = agendaGraphTimeRadius(now - n.born);
  });
  agendaGraphTimeRebuildMarks(now);
  agendaGraphSettleLeft = Math.max(agendaGraphSettleLeft, 32);
}

// Humane span for the projection badge: how far back the disc reaches.
function agendaGraphTimeSpanLabel() {
  const h = Math.max(0, Date.now() - agendaGraphTimeOldest) / AGENDA_GRAPH_TIME_HOUR;
  const span = h < 1 ? 'under an hour'
    : h < 48 ? `${Math.round(h)}h`
      : h < 24 * 90 ? `${Math.round(h / 24)}d`
        : h < 24 * 730 ? `${Math.round(h / (24 * 30.44))}mo`
          : `${(h / (24 * 365.25)).toFixed(1)}y`;
  return `spans ${span}`;
}

// ---- Layout (topology-keyed force relaxation) ----

// Rebuild nodes/links only when the topology key changes; surviving
// nodes keep their positions and the settle budget re-arms so the new
// shape relaxes in over the following frames.
function agendaGraphBuild() {
  const items = agendaGraphProjectionItems();
  // Territory satellites: one node per distinct file/dir locator across
  // the pool's refs (DECLARED territory; observed affinity joins later
  // via the gardener), newest attach first, capped into the node-cap
  // room so items + satellites never exceed the ratified bound.
  const territoryOn = agendaGraphTerritory && agendaGraphProjection !== 'hubs';
  let satellites = [];
  if (territoryOn) {
    const best = new Map();
    items.forEach((x) => (x.refs || []).forEach((r) => {
      if (r.ref_type !== 'file' && r.ref_type !== 'dir') return;
      const skey = `${r.ref_type}:${r.locator}`;
      let entry = best.get(skey);
      if (!entry) {
        entry = {
          key: skey, t: r.ref_type, locator: r.locator,
          newest: 0, carrier: x.id, owners: [],
        };
        best.set(skey, entry);
      }
      entry.owners.push(x.id);
      if ((r.added_ms || 0) >= entry.newest) {
        entry.newest = r.added_ms || 0;
        entry.carrier = x.id;
      }
    }));
    const all = [...best.values()].sort((a, b) =>
      b.newest - a.newest || (a.locator < b.locator ? -1 : 1));
    satellites = all.slice(0, 60);
    agendaGraphTerrStats = { shown: satellites.length, total: all.length };
  } else {
    agendaGraphTerrStats = { shown: 0, total: 0 };
  }
  // Hub overview: halo hubs by their SUBTREE's declared territory — a
  // client-side walk over the full pool (no daemon change), so the map
  // shows where the files live before diving in.
  agendaGraphSubtreeTerr = new Map();
  if (agendaGraphProjection === 'hubs') {
    const pool = agendaGraphPoolItems();
    const kids = new Map();
    pool.forEach((x) => {
      if (x.part_of) {
        const list = kids.get(x.part_of.parent_id) || [];
        list.push(x.id);
        kids.set(x.part_of.parent_id, list);
      }
    });
    const own = new Map(pool.map((x) => [x.id,
      (x.refs || []).filter((r) => r.ref_type === 'file' || r.ref_type === 'dir').length]));
    items.forEach((hub) => {
      let sum = 0;
      const seen = new Set();
      const queue = [hub.id];
      while (queue.length) {
        const id = queue.pop();
        if (seen.has(id)) continue;
        seen.add(id);
        sum += own.get(id) || 0;
        (kids.get(id) || []).forEach((k) => queue.push(k));
      }
      agendaGraphSubtreeTerr.set(hub.id, sum);
    });
  }
  const key = `${agendaGraphProjection}|${agendaGraphFocus || ''}|${agendaGraphMode}|terr:${satellites.map((s) => s.key).join(',')};` + items.map((x) => [
    x.id,
    x.status,
    x.part_of ? x.part_of.parent_id : '',
    // Adjacency keyed per-link (target + kind) so a re-typed link
    // rebuilds even when the count is unchanged.
    (x.relates_to || []).map((l) => `${l.target_id}~${l.link_kind || ''}`).join(','),
    (x.relies_on || []).length,
    x.kind,
    // Files mode lays out from refs, so ref changes must re-key; the
    // other modes ignore refs (territory satellites carry their own
    // key segment above).
    agendaGraphMode === 'files'
      ? (x.refs || []).map((r) => `${r.ref_type}:${r.locator}`).join('+')
      : '',
  ].join('|')).join(';');
  if (key === agendaGraphKey && agendaGraphNodes.length) return items;
  agendaGraphKey = key;
  const previous = new Map(agendaGraphNodes.map((n) => [n.id, n.p]));
  agendaGraphNodes = items.map((x, i) => {
    const a = i * 2.4;
    const r = 90 + (i % 5) * 22;
    return {
      id: x.id,
      p: previous.get(x.id)
        || [Math.cos(a) * r, (Math.random() - 0.5) * 90, Math.sin(a) * r],
    };
  });
  const idx = new Map(agendaGraphNodes.map((n, i) => [n.id, i]));
  satellites.forEach((sat, i) => {
    const nodeId = `terr|${sat.key}`;
    const near = idx.get(sat.carrier);
    const base = near !== undefined ? agendaGraphNodes[near].p : [0, 0, 0];
    const a = i * 1.7;
    agendaGraphNodes.push({
      id: nodeId,
      sat,
      p: previous.get(nodeId) || [
        base[0] + Math.cos(a) * 26,
        base[1] + (Math.random() - 0.5) * 22,
        base[2] + Math.sin(a) * 26,
      ],
    });
    idx.set(nodeId, agendaGraphNodes.length - 1);
  });
  // The active mode's per-node layout targets (item nodes only;
  // satellites keep riding their carrier springs). Node objects are
  // rebuilt fresh above, so stale mode fields cannot linger.
  const byIdBuild = new Map(items.map((x) => [x.id, x]));
  agendaGraphTimeMarks = [];
  agendaGraphActorMarks = [];
  agendaGraphFilesMarks = [];
  agendaGraphAttnCount = 0;
  if (agendaGraphMode === 'flow') {
    // relies_on rank: prerequisites left of dependents. Bounded passes
    // instead of recursion so a foreign log's cycle stays total.
    const rank = new Map(items.map((x) => [x.id, 0]));
    for (let pass = 0; pass < 16; pass++) {
      let moved = false;
      items.forEach((x) => (x.relies_on || []).forEach((l) => {
        if (!rank.has(l.target_id)) return;
        const want = rank.get(l.target_id) + 1;
        if (want > rank.get(x.id) && want < 40) {
          rank.set(x.id, want);
          moved = true;
        }
      }));
      if (!moved) break;
    }
    const maxRank = Math.max(1, ...rank.values());
    agendaGraphNodes.forEach((n) => {
      if (!n.sat) n.tx = (rank.get(n.id) - maxRank / 2) * 72;
    });
  } else if (agendaGraphMode === 'time') {
    // Anchored at NOW, not at the newest item: the center is the
    // present, and an item's ring is its actual age — an untouched
    // ledger reads old at a glance instead of being stretched to fill.
    const now = Date.now();
    const born = (x) => (x.provenance && x.provenance.created_ms) || x.updated_ms || 0;
    let oldest = now;
    items.forEach((x) => {
      const b = born(x);
      if (b > 0 && b < oldest) oldest = b;
    });
    agendaGraphTimeOldest = oldest;
    agendaGraphTimeScale = Math.max(1, Math.sqrt(items.length / 90));
    agendaGraphNodes.forEach((n) => {
      if (n.sat) return;
      const b = born(byIdBuild.get(n.id)) || oldest;
      n.born = b;
      n.tr = agendaGraphTimeRadius(now - b);
    });
    agendaGraphTimeRebuildMarks(now);
    agendaGraphTimeCheckAt = now;
  } else if (agendaGraphMode === 'files') {
    // Cluster by the AREA of the codebase an item touches: each
    // file/dir locator collapses to its directory prefix (≤3 path
    // segments; a file drops its basename first, a dir keeps its leaf),
    // the top eight prefixes by item count take anchors on a wide
    // circle, and an item springs to the AVERAGE of its prefixes'
    // anchors — work spanning two areas honestly sits between them.
    // Items with no declared territory gather in a neutral core at the
    // center; with no territory anywhere the mode degrades to the plain
    // orbit (no anchors assigned at all).
    const prefixOf = (r) => {
      const parts = String(r.locator).replace(/\/+$/, '').split('/').filter(Boolean);
      if (r.ref_type === 'file' && parts.length) parts.pop();
      return parts.slice(0, 3).join('/') || '/';
    };
    const perItem = new Map();
    const counts = new Map();
    items.forEach((x) => {
      // Per-item ref tally by prefix; the DOMINANT prefix places the
      // item. (Averaging multi-area anchors was tried first and lands
      // spanning items near the center — where the midpoint of far-
      // apart rim anchors falls — which reads as "no territory".)
      const tally = new Map();
      (x.refs || []).forEach((r) => {
        if (r.ref_type !== 'file' && r.ref_type !== 'dir') return;
        const p = prefixOf(r);
        tally.set(p, (tally.get(p) || 0) + 1);
      });
      if (tally.size) {
        perItem.set(x.id, tally);
        tally.forEach((_, p) => counts.set(p, (counts.get(p) || 0) + 1));
      }
    });
    const top = [...counts.entries()].sort((a, b) =>
      b[1] - a[1] || (a[0] < b[0] ? -1 : 1)).slice(0, 8);
    if (top.length) {
      // The anchor circle clears the neutral core's own footprint (the
      // core is usually the majority — items with no declared refs), so
      // clusters read as satellites around it instead of bleeding in.
      const coreCount = items.length - perItem.size;
      const anchorR = 120 + 9 * Math.sqrt(Math.max(1, coreCount));
      const anchors = new Map(top.map(([name], i) => {
        const angle = (i / top.length) * Math.PI * 2;
        return [name, [Math.cos(angle) * anchorR, 0, Math.sin(angle) * anchorR]];
      }));
      agendaGraphFilesMarks = top.map(([name, count], i) => ({
        name, count, angle: (i / top.length) * Math.PI * 2, r: anchorR,
      }));
      agendaGraphNodes.forEach((n) => {
        if (n.sat) return;
        const tally = perItem.get(n.id);
        if (!tally) {
          // The neutral core must stay compact enough to leave a gap
          // under the rim clusters — a loose leash smears it into the
          // same annulus and the areas stop reading as areas.
          n.anchor = [0, 0, 0];
          n.anchorK = 0.03;
          return;
        }
        let bestPrefix = null;
        let bestScore = -1;
        tally.forEach((score, p) => {
          if (anchors.has(p)
            && (score > bestScore
              || (score === bestScore && counts.get(p) > counts.get(bestPrefix)))) {
            bestPrefix = p;
            bestScore = score;
          }
        });
        // Territory entirely outside the top eight areas parks in the
        // neutral core too, rather than inventing a ninth cluster.
        n.anchor = bestPrefix ? anchors.get(bestPrefix) : [0, 0, 0];
        n.anchorK = bestPrefix ? 0.07 : 0.012;
      });
    }
  } else if (agendaGraphMode === 'actor') {
    const who = (x) => (x.provenance
      && (x.provenance.source || x.provenance.kind
        || (x.provenance.principal || '').split(':').pop())) || 'unknown';
    const counts = new Map();
    items.forEach((x) => counts.set(who(x), (counts.get(who(x)) || 0) + 1));
    const top = [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
    const anchor = new Map(top.map(([name], i) => [name, (i / top.length) * Math.PI * 2]));
    agendaGraphActorMarks = top.map(([name, count], i) => ({
      name, count, angle: (i / top.length) * Math.PI * 2,
    }));
    agendaGraphNodes.forEach((n) => {
      if (n.sat) return;
      const a = anchor.get(who(byIdBuild.get(n.id)));
      n.anchor = a === undefined
        ? [0, 0, 0]
        : [Math.cos(a) * 132, 0, Math.sin(a) * 132];
    });
  } else if (agendaGraphMode === 'attention') {
    const now = Date.now();
    agendaGraphNodes.forEach((n) => {
      if (n.sat) return;
      const x = byIdBuild.get(n.id);
      const st = agendaEffectState(x);
      const attn = x.status === 'open' && (
        agendaItemIsBlocked(x)
        || (x.kind === 'question' && !x.answer && !x.dismissed)
        || (st && (st.kind === 'pending' || st.kind === 'suspended'))
        || (x.due_ms && x.due_ms < now));
      n.attn = !!attn;
      n.tr = attn ? 28 : 172;
      if (attn) agendaGraphAttnCount += 1;
    });
  }
  const links = [];
  const seenRel = new Set();
  items.forEach((x) => {
    if (x.part_of && idx.has(x.part_of.parent_id)) {
      links.push({ a: idx.get(x.id), b: idx.get(x.part_of.parent_id), t: 'place' });
    }
    (x.relies_on || []).forEach((link) => {
      if (idx.has(link.target_id)) {
        links.push({ a: idx.get(x.id), b: idx.get(link.target_id), t: 'dep' });
      }
    });
    // relates_to renders undirected and deduped across the two stored
    // directions; a typed link keeps its stored direction (a = storer,
    // b = target — "A supersedes B" draws its arrow at B).
    (x.relates_to || []).forEach((link) => {
      if (!idx.has(link.target_id)) return;
      const pair = [x.id, link.target_id].sort().join(':');
      if (seenRel.has(pair)) return;
      seenRel.add(pair);
      links.push({
        a: idx.get(x.id),
        b: idx.get(link.target_id),
        t: 'rel',
        k: link.link_kind || null,
      });
    });
  });
  satellites.forEach((sat) => {
    const b = idx.get(`terr|${sat.key}`);
    sat.owners.forEach((owner) => {
      if (idx.has(owner)) links.push({ a: idx.get(owner), b, t: 'terr' });
    });
  });
  // Degree (link endpoints of every kind) drives node SIZE in the draw
  // pass — connectivity is visual mass. Children keep deciding which
  // nodes carry labels; the two notions overlap on hubs but diverge on
  // heavily-cited leaves, which is the point.
  const degrees = new Array(agendaGraphNodes.length).fill(0);
  links.forEach((link) => {
    degrees[link.a] += 1;
    degrees[link.b] += 1;
  });
  agendaGraphNodes.forEach((nd, i) => { nd.deg = degrees[i]; });
  agendaGraphLinks = links;
  agendaGraphSettleLeft = AGENDA_GRAPH_SETTLE_ITERATIONS;
  return items;
}

// Bounded relaxation iterations for one frame, scaled down as the pair
// count grows so a settling frame stays well under a paint budget.
function agendaGraphSettleBudget(count) {
  if (count <= 60) return 30;
  if (count <= 120) return 12;
  if (count <= 240) return 6;
  // Together with the strided repulsion past REPULSION_LOD this keeps
  // per-frame settle work roughly flat as n² grows; the settle just
  // takes more frames to spend its 260-iteration total.
  return 4;
}

function agendaGraphRelax(iterations) {
  const nodes = agendaGraphNodes;
  const links = agendaGraphLinks;
  // Past the LOD bound the pair loop strides with a rotating offset:
  // each iteration touches 1/stride of the pairs, different ones each
  // time, so big boards converge to the same shape at flat frame cost.
  const stride = 1 + Math.floor(nodes.length / AGENDA_GRAPH_REPULSION_LOD);
  for (let it = 0; it < iterations; it++) {
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1 + ((it + i) % stride); j < nodes.length; j += stride) {
        const A = nodes[i].p;
        const B = nodes[j].p;
        let dx = A[0] - B[0];
        let dy = A[1] - B[1];
        let dz = A[2] - B[2];
        const d2 = dx * dx + dy * dy + dz * dz + 1;
        const f = Math.min(4, 5200 / d2);
        const d = Math.sqrt(d2);
        dx /= d; dy /= d; dz /= d;
        A[0] += dx * f; A[1] += dy * f; A[2] += dz * f;
        B[0] -= dx * f; B[1] -= dy * f; B[2] -= dz * f;
      }
    }
    links.forEach((link) => {
      const A = nodes[link.a].p;
      const B = nodes[link.b].p;
      const rest = link.t === 'place' ? 74
        : link.t === 'terr' ? 52
          : link.t === 'dep' ? 96 : 116;
      let k = link.t === 'place' ? 0.014 : link.t === 'terr' ? 0.012 : 0.007;
      // Files mode: territory anchors, not links, own the geometry — a
      // cluster member's springs to its (usually refs-less, core-
      // parked) hub and to cross-field see-also partners would
      // otherwise streak every cluster toward the middle. The links
      // still draw; they just pull less here.
      if (agendaGraphMode === 'files') k *= link.t === 'place' ? 0.25 : 0.5;
      const dx = B[0] - A[0];
      const dy = B[1] - A[1];
      const dz = B[2] - A[2];
      const d = Math.sqrt(dx * dx + dy * dy + dz * dz) + 0.01;
      const f = ((d - rest) * k) / d;
      A[0] += dx * f; A[1] += dy * f; A[2] += dz * f;
      B[0] -= dx * f; B[1] -= dy * f; B[2] -= dz * f;
    });
    nodes.forEach((n) => {
      n.p[0] *= 0.9965;
      n.p[1] *= 0.994;
      n.p[2] *= 0.9965;
      // Mode springs (one per node, fields set by the active mode's
      // build pass): tx = layered x (flow), tr = radial xz target
      // (time/attention), anchor = cluster point (actor).
      if (n.tx !== undefined) {
        n.p[0] += (n.tx - n.p[0]) * 0.03;
      }
      if (n.tr !== undefined) {
        // In the time lens the radius IS the datum (age), so it is
        // enforced as a near-pin: links and repulsion may only slide a
        // node around its ring or off-plane, never lie about its age.
        // Attention keeps the soft pull — there the radius is emphasis,
        // not measurement.
        const strength = agendaGraphMode === 'time' ? 0.5 : 0.035;
        const cur = Math.hypot(n.p[0], n.p[2]) + 0.01;
        const f = ((n.tr - cur) * strength) / cur;
        n.p[0] += n.p[0] * f;
        n.p[2] += n.p[2] * f;
        // With the radius pinned, repulsion would escape into the free
        // vertical axis and smear the disc — clamp y hard in time mode
        // so crowding spreads AROUND a ring instead (beads, not fuzz).
        // The clamp eases as population grows: a dense board earns back
        // a little shimmer thickness as an overflow valve.
        n.p[1] *= agendaGraphMode === 'time'
          ? Math.min(0.82, 0.6 + nodes.length / 1200) : 0.97;
      }
      if (n.anchor) {
        // anchorK: per-node cluster-spring strength (files mode sets a
        // firm pull for clustered items, a loose leash for the neutral
        // core); actor mode leaves it unset and keeps the default.
        const ak = n.anchorK || 0.025;
        n.p[0] += (n.anchor[0] - n.p[0]) * ak;
        n.p[1] += (n.anchor[1] - n.p[1]) * 0.02;
        n.p[2] += (n.anchor[2] - n.p[2]) * ak;
      }
    });
  }
}

// ---- Palette (computed-style reads, cached ~500ms so theme flips
// repaint without a per-frame style query) ----

function agendaGraphPalette() {
  const now = Date.now();
  if (!agendaGraphPalCache || now - agendaGraphPalAt > 500) {
    const cs = getComputedStyle(document.documentElement);
    const v = (name) => cs.getPropertyValue(name).trim();
    agendaGraphPalCache = {
      iris: v('--iris-rgb'),
      green: v('--green-rgb'),
      amber: v('--amber-rgb'),
      rose: v('--rose-rgb'),
      text: v('--text-rgb'),
      t3: v('--text-3'),
    };
    agendaGraphPalAt = now;
  }
  return agendaGraphPalCache;
}

// One territory satellite: a small square (dirs slightly larger), the
// basename labeled while hot, full locator + carrier count on the
// subtitle. Locators are data — fillText only, inert pixels.
function agendaGraphDrawSatellite(g, node, q, hot, pal, w) {
  const sat = node.sat;
  const size = (sat.t === 'dir' ? 3.4 : 2.6) * q.s + (hot ? 0.9 : 0);
  const alpha = Math.max(0.3, Math.min(0.85, q.s * 1.05 - 0.1));
  g.beginPath();
  g.rect(q.x - size, q.y - size, size * 2, size * 2);
  g.fillStyle = `rgba(${pal.text},${alpha * (hot ? 0.85 : 0.45)})`;
  g.fill();
  if (hot) {
    const name = sat.locator.replace(/\/+$/, '').split('/').pop() || sat.locator;
    const label = sat.t === 'dir' ? `${name}/` : name;
    g.font = '700 10px "JetBrains Mono", monospace';
    const tw = g.measureText(label).width;
    let lx = q.x + size + 7;
    if (lx + tw > w - 10) lx = q.x - size - 7 - tw;
    g.fillStyle = `rgba(${pal.text},.95)`;
    g.fillText(label, lx, q.y + 3.5);
    g.font = '9px "JetBrains Mono", monospace';
    g.fillStyle = pal.t3;
    g.fillText(
      `${sat.locator} · ${sat.owners.length} item${sat.owners.length === 1 ? '' : 's'} — click opens newest carrier`,
      lx, q.y + 16);
  }
}

// Pre-baked glow sprites: one radial-gradient canvas per palette color,
// drawImage-scaled per node. This is what removes the node cap — the
// shadowBlur look at ~1/100th its cost, so a thousand glowing nodes
// draw inside a frame. Keyed by the palette's rgb string, so theme
// flips mint fresh sprites and stale ones just idle in the map.
const agendaGraphGlowSprites = new Map();
function agendaGraphGlowSprite(rgb) {
  let sprite = agendaGraphGlowSprites.get(rgb);
  if (!sprite) {
    sprite = document.createElement('canvas');
    sprite.width = 64;
    sprite.height = 64;
    const sg = sprite.getContext('2d');
    const grad = sg.createRadialGradient(32, 32, 2, 32, 32, 30);
    grad.addColorStop(0, `rgba(${rgb},.85)`);
    grad.addColorStop(0.35, `rgba(${rgb},.28)`);
    grad.addColorStop(1, `rgba(${rgb},0)`);
    sg.fillStyle = grad;
    sg.fillRect(0, 0, 64, 64);
    agendaGraphGlowSprites.set(rgb, sprite);
  }
  return sprite;
}

// Time-lens chrome, drawn through the SAME projection as the nodes so
// the rings, their labels, and the "now" anchor track orbit, zoom, and
// pan exactly. (The first painter drew screen-space circles at the
// fixed canvas center — correct only while the camera never zoomed or
// panned; the moment it did, the disc of nodes left the rings behind
// and the present stopped being centered.)
const AGENDA_GRAPH_TIME_RING_STEPS = 72;
function agendaGraphDrawTimeChrome(g, project, pal, w, h, reduced, ts) {
  // Rings: world-space polylines on the layout plane (y = 0),
  // depth-faded per segment so the far side recedes like the nodes do.
  agendaGraphTimeMarks.forEach((mark) => {
    const pts = [];
    for (let i = 0; i <= AGENDA_GRAPH_TIME_RING_STEPS; i++) {
      const a = (i / AGENDA_GRAPH_TIME_RING_STEPS) * Math.PI * 2;
      pts.push(project([Math.cos(a) * mark.r, 0, Math.sin(a) * mark.r]));
    }
    g.setLineDash(mark.oldest ? [2, 4] : []);
    g.lineWidth = 1;
    const base = mark.minor ? 0.05 : 0.1;
    for (let i = 0; i < AGENDA_GRAPH_TIME_RING_STEPS; i++) {
      const a = pts[i];
      const b = pts[i + 1];
      const depth = Math.max(0.35, Math.min(1, ((a.s + b.s) / 2) * 1.1 - 0.18));
      g.beginPath();
      g.moveTo(a.x, a.y);
      g.lineTo(b.x, b.y);
      g.strokeStyle = `rgba(${pal.text},${base * depth})`;
      g.stroke();
    }
  });
  g.setLineDash([]);
  // The "now" beacon at the world origin: a small iris core with a
  // breathing pulse (static halo under reduced motion). The pulse is a
  // screen-space emitter, not plane geometry, so plain arcs are right.
  const q0 = project([0, 0, 0]);
  const pr = Math.max(1.7, 2.4 * q0.s);
  if (!reduced) {
    for (let k = 0; k < 2; k++) {
      const t = (ts / 2800 + k * 0.5) % 1;
      g.beginPath();
      g.arc(q0.x, q0.y, pr + 2 + t * 30 * q0.s, 0, Math.PI * 2);
      g.strokeStyle = `rgba(${pal.iris},${(1 - t) * 0.3})`;
      g.lineWidth = 1.1;
      g.stroke();
    }
  } else {
    g.beginPath();
    g.arc(q0.x, q0.y, pr + 4, 0, Math.PI * 2);
    g.strokeStyle = `rgba(${pal.iris},.35)`;
    g.lineWidth = 1.1;
    g.stroke();
  }
  g.beginPath();
  g.arc(q0.x, q0.y, pr, 0, Math.PI * 2);
  g.fillStyle = `rgba(${pal.iris},.9)`;
  g.fill();
  g.font = '700 9px "JetBrains Mono", monospace';
  g.fillStyle = `rgba(${pal.text},.75)`;
  g.fillText('now', q0.x + pr + 5, q0.y + 3);
  // Ring labels ride the ray from the center toward screen-right: the
  // world point at angle yaw projects to the ellipse's right extreme at
  // mid-depth, so every label shares one baseline that pans and zooms
  // with the disc. Collision is resolved on that shared axis — "now"
  // first, then the oldest date, then majors inner→outer, minors last —
  // so when zoom packs rings together the least important labels yield,
  // and they return as soon as there is room again.
  g.font = '9px "JetBrains Mono", monospace';
  const yaw = agendaGraphCam.yaw;
  const placed = [[q0.x + pr + 3, q0.x + pr + 5 + g.measureText('now').width + 4]];
  const candidates = [...agendaGraphTimeMarks].sort(
    (a, b) => (b.oldest - a.oldest) || (a.minor - b.minor));
  candidates.forEach((mark) => {
    const q = project([Math.cos(yaw) * mark.r, 0, Math.sin(yaw) * mark.r]);
    if (q.y < 12 || q.y > h - 6) return;
    const width = g.measureText(mark.label).width;
    let lx = Math.min(q.x + 5, w - 8 - width);
    if (lx < 8 || q.x > w - 8 || q.x < 8) return;
    const x0 = lx - 2;
    const x1 = lx + width + 4;
    if (placed.some(([a, b]) => x0 < b && x1 > a)) return;
    placed.push([x0, x1]);
    g.fillStyle = mark.oldest ? `rgba(${pal.text},.6)`
      : mark.minor ? `rgba(${pal.text},.34)` : pal.t3;
    g.fillText(mark.label, lx, q.y - 5);
    // A short tick from the baseline up to the label roots it to its
    // ring even when labels sit close.
    g.fillRect(q.x - 0.5, q.y - 3.5, 1, 3.5);
  });
}

// ---- Draw (3D-ish projection, hover picking, rings, labels) ----

function agendaGraphDraw(ts) {
  const canvas = agendaGraphCanvas;
  if (!canvas) return;
  const items = agendaGraphBuild();
  if (!items.length) {
    // The ledger emptied between renders (event-lane merge): re-enter
    // through the render pass, which owns projection choice and the
    // empty state (it tears this loop down).
    agendaRenderTab();
    return;
  }
  if (agendaGraphMode === 'time') agendaGraphTimeTick();
  if (agendaGraphSettleLeft > 0) {
    const step = Math.min(agendaGraphSettleLeft,
      agendaGraphSettleBudget(agendaGraphNodes.length));
    agendaGraphRelax(step);
    agendaGraphSettleLeft -= step;
  }
  const nowMs = Date.now();
  const reduced = agendaGraphReducedMotion();
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (!w || !h) return;
  // DPR-aware bitmap sizing, re-checked every frame so window resizes
  // and monitor moves never leave a blurry or letterboxed canvas.
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const g = canvas.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, h);
  const pal = agendaGraphPalette();
  const glow = g.createRadialGradient(w * 0.74, h * 0.1, 0, w * 0.74, h * 0.1, w * 0.55);
  glow.addColorStop(0, `rgba(${pal.iris},.055)`);
  glow.addColorStop(1, `rgba(${pal.iris},0)`);
  g.fillStyle = glow;
  g.fillRect(0, 0, w, h);
  const cam = agendaGraphCam;
  if (!reduced && cam.auto && !agendaGraphMouse.down) cam.yaw += 0.0016;
  const cy = Math.cos(cam.yaw);
  const sy = Math.sin(cam.yaw);
  const cp = Math.cos(cam.pitch);
  const sp = Math.sin(cam.pitch);
  const focal = 760;
  const cx = w / 2;
  const cyy = h / 2 + 8;
  const project = (p) => {
    const rx = p[0] * cy + p[2] * sy;
    let rz = -p[0] * sy + p[2] * cy;
    const ry = p[1] * cp - rz * sp;
    rz = p[1] * sp + rz * cp;
    const s = focal / (focal + rz + 40);
    const k = s * 1.35 * cam.zoom;
    return {
      x: cx + cam.panX + rx * k,
      y: cyy + cam.panY + ry * k,
      s: s * cam.zoom,
      z: rz,
    };
  };
  const nodes = agendaGraphNodes;
  const pts = nodes.map((n) => project(n.p));
  // Hover pick: the nearest node within ~16 css px of the pointer.
  let hover = null;
  let best = 16;
  nodes.forEach((n, i) => {
    const q = pts[i];
    const d = Math.hypot(q.x - agendaGraphMouse.x, q.y - agendaGraphMouse.y);
    if (d < best) {
      best = d;
      hover = n.id;
    }
  });
  agendaGraphHover = hover;
  canvas.style.cursor = hover ? 'pointer' : agendaGraphMouse.down ? 'grabbing' : 'grab';
  const byId = new Map(items.map((x) => [x.id, x]));
  const childCount = new Map();
  items.forEach((x) => {
    if (x.part_of && byId.has(x.part_of.parent_id)) {
      childCount.set(x.part_of.parent_id, (childCount.get(x.part_of.parent_id) || 0) + 1);
    }
  });
  // Links under nodes: placement solid iris, see-also dashed neutral,
  // waits-on rose with the animated dash (static under reduced motion).
  agendaGraphLinks.forEach((link) => {
    const a = pts[link.a];
    const b = pts[link.b];
    const depth = Math.max(0.25, Math.min(1, ((a.s + b.s) / 2) * 1.1 - 0.18));
    const hot = hover && (nodes[link.a].id === hover || nodes[link.b].id === hover);
    g.beginPath();
    g.moveTo(a.x, a.y);
    g.lineTo(b.x, b.y);
    if (link.t === 'place') {
      // Files mode: the six hub fans would visually unify the whole
      // field and bury the territory clusters — recede unless hot.
      const placeAlpha = agendaGraphMode === 'files' && !hot ? 0.13 : hot ? 0.75 : 0.38;
      g.setLineDash([]);
      g.strokeStyle = `rgba(${pal.iris},${depth * placeAlpha})`;
      g.lineWidth = hot ? 1.6 : 1.1;
    } else if (link.t === 'terr') {
      g.setLineDash([1.5, 3.5]);
      g.lineDashOffset = 0;
      g.strokeStyle = `rgba(${pal.text},${depth * (hot ? 0.45 : 0.14)})`;
      g.lineWidth = 1;
    } else if (link.t === 'rel') {
      if (link.k) {
        // Typed adjacency: solid and slightly stronger than see-also.
        g.setLineDash([]);
        g.strokeStyle = `rgba(${pal.text},${depth * (hot ? 0.6 : 0.26)})`;
        g.lineWidth = hot ? 1.4 : 1.05;
      } else {
        g.setLineDash([3, 5]);
        g.lineDashOffset = 0;
        g.strokeStyle = `rgba(${pal.text},${depth * (hot ? 0.5 : 0.16)})`;
        g.lineWidth = 1;
      }
    } else {
      g.setLineDash([2, 6]);
      g.lineDashOffset = reduced ? 0 : -ts * 0.02;
      g.strokeStyle = `rgba(${pal.rose},${depth * (hot ? 0.85 : 0.45)})`;
      g.lineWidth = hot ? 1.6 : 1.2;
    }
    g.stroke();
    g.setLineDash([]);
    if (agendaGraphMode === 'flow' && link.t === 'dep') {
      // Flow mode: the dependency direction becomes explicit — the
      // arrowhead sits at the PREREQUISITE end ("waits on →").
      const ang = Math.atan2(b.y - a.y, b.x - a.x);
      const ax = b.x - Math.cos(ang) * 11;
      const ay = b.y - Math.sin(ang) * 11;
      const sz = 3.4 + (hot ? 0.9 : 0);
      g.beginPath();
      g.moveTo(ax + Math.cos(ang) * sz, ay + Math.sin(ang) * sz);
      g.lineTo(ax + Math.cos(ang + 2.5) * sz, ay + Math.sin(ang + 2.5) * sz);
      g.lineTo(ax + Math.cos(ang - 2.5) * sz, ay + Math.sin(ang - 2.5) * sz);
      g.closePath();
      g.fillStyle = `rgba(${pal.rose},${depth * (hot ? 0.85 : 0.5)})`;
      g.fill();
    }
    if (link.t === 'rel' && link.k) {
      // A typed link reads storer → target: arrowhead at the target
      // end, the kind labeled at the midpoint while an endpoint is hot.
      const ang = Math.atan2(b.y - a.y, b.x - a.x);
      const ax = b.x - Math.cos(ang) * 11;
      const ay = b.y - Math.sin(ang) * 11;
      const sz = 3.2 + (hot ? 0.9 : 0);
      g.beginPath();
      g.moveTo(ax + Math.cos(ang) * sz, ay + Math.sin(ang) * sz);
      g.lineTo(ax + Math.cos(ang + 2.5) * sz, ay + Math.sin(ang + 2.5) * sz);
      g.lineTo(ax + Math.cos(ang - 2.5) * sz, ay + Math.sin(ang - 2.5) * sz);
      g.closePath();
      g.fillStyle = `rgba(${pal.text},${depth * (hot ? 0.7 : 0.32)})`;
      g.fill();
      if (hot) {
        g.font = '9px "JetBrains Mono", monospace';
        g.fillStyle = `rgba(${pal.text},.8)`;
        g.fillText(
          link.k.replace(/_/g, ' '),
          (a.x + b.x) / 2 + 5,
          (a.y + b.y) / 2 - 5,
        );
      }
    }
  });
  // Mode orientation marks: painted, inert pixels like every label.
  if (agendaGraphMode === 'time') {
    agendaGraphDrawTimeChrome(g, project, pal, w, h, reduced, ts);
  }
  if (agendaGraphMode === 'actor') {
    g.font = '600 9.5px "JetBrains Mono", monospace';
    agendaGraphActorMarks.forEach((mark) => {
      const q = project([Math.cos(mark.angle) * 172, -34, Math.sin(mark.angle) * 172]);
      g.fillStyle = pal.t3;
      g.fillText(`${mark.name} · ${mark.count}`, q.x - 20, q.y);
    });
  }
  if (agendaGraphMode === 'files') {
    // Each area gets a dotted boundary circle sized to its population
    // (world-space, depth-faded like the time rings) with its label
    // floating above — the wash that makes a cluster read as a place.
    agendaGraphFilesMarks.forEach((mark) => {
      const ax = Math.cos(mark.angle) * mark.r;
      const az = Math.sin(mark.angle) * mark.r;
      const cr = 16 + 9 * Math.sqrt(mark.count);
      const steps = 36;
      // A barely-there wash inside the boundary makes the area read as
      // a region (a country on a map), not just a line.
      g.beginPath();
      for (let i = 0; i <= steps; i++) {
        const a = (i / steps) * Math.PI * 2;
        const p = project([ax + Math.cos(a) * cr, 0, az + Math.sin(a) * cr]);
        if (i === 0) g.moveTo(p.x, p.y);
        else g.lineTo(p.x, p.y);
      }
      g.closePath();
      g.fillStyle = `rgba(${pal.iris},.045)`;
      g.fill();
      g.setLineDash([2, 4]);
      g.lineWidth = 1;
      for (let i = 0; i < steps; i++) {
        const a0 = (i / steps) * Math.PI * 2;
        const a1 = ((i + 1) / steps) * Math.PI * 2;
        const p0 = project([ax + Math.cos(a0) * cr, 0, az + Math.sin(a0) * cr]);
        const p1 = project([ax + Math.cos(a1) * cr, 0, az + Math.sin(a1) * cr]);
        const depth = Math.max(0.3, Math.min(1, ((p0.s + p1.s) / 2) * 1.1 - 0.18));
        g.beginPath();
        g.moveTo(p0.x, p0.y);
        g.lineTo(p1.x, p1.y);
        g.strokeStyle = `rgba(${pal.text},${0.22 * depth})`;
        g.stroke();
      }
      g.setLineDash([]);
    });
  }
  // Nodes far → near.
  const order = nodes.map((n, i) => i).sort((a, b) => pts[b].z - pts[a].z);
  order.forEach((i) => {
    const node = nodes[i];
    const q = pts[i];
    if (node.sat) {
      agendaGraphDrawSatellite(g, node, q, hover === node.id, pal, w);
      return;
    }
    const item = byId.get(node.id);
    if (!item) return;
    const kids = childCount.get(node.id) || 0;
    const rgb = item.status === 'done' ? pal.green
      : item.kind === 'question' ? pal.amber : pal.iris;
    const hot = hover === node.id;
    // Radius by degree, sub-linear and capped: landmarks, not planets.
    const r = (3.0 + Math.min(6, Math.sqrt(node.deg || 0) * 1.35)
      + (hot ? 1.4 : 0)) * q.s;
    let alpha = Math.max(0.35, Math.min(1, q.s * 1.15 - 0.1));
    // Attention mode: needs-you items glow, the rest recede.
    if (agendaGraphMode === 'attention' && !node.attn && !hot) alpha *= 0.45;
    // Files mode: the no-territory core is context, not subject — it
    // recedes so the area clusters carry the figure-ground. (Core
    // nodes are exactly the ones on the loose 0.03 leash.)
    if (agendaGraphMode === 'files' && !hot && node.anchorK === 0.03) {
      alpha *= 0.5;
    }
    const glow = agendaGraphGlowSprite(rgb);
    // Halo budget shrinks on crowded boards — past ~150 nodes the full
    // glow tiles the disc into one nebula and drowns the geometry.
    let glowK = hot ? 6.5
      : agendaGraphMode === 'attention' && node.attn ? 7.5
        : agendaGraphNodes.length > 150 ? 3.1 : 4.5;
    if (!hot && agendaGraphMode === 'time' && node.born) {
      // Recency emphasis: items parked within the last hour breathe a
      // little brighter around the "now" beacon, fading out as they age.
      glowK += Math.max(0, 1 - (nowMs - node.born) / AGENDA_GRAPH_TIME_HOUR) * 2.5;
    }
    const gr = r * glowK;
    g.globalAlpha = alpha * (hot ? 0.95 : 0.6);
    g.drawImage(glow, q.x - gr, q.y - gr, gr * 2, gr * 2);
    g.globalAlpha = 1;
    g.beginPath();
    g.arc(q.x, q.y, r, 0, Math.PI * 2);
    g.fillStyle = `rgba(${rgb},${alpha})`;
    g.fill();
    // Rings reuse slice A's derivations: blocked (uncleared blocker or
    // unmet prerequisite) rose; approved standing/armed green; suspended
    // amber with a slow pulse (static under reduced motion); pending
    // approval amber.
    const st = agendaEffectState(item);
    const ring = (col, off, ringAlpha) => {
      g.beginPath();
      g.arc(q.x, q.y, r + off, 0, Math.PI * 2);
      g.strokeStyle = `rgba(${col},${ringAlpha})`;
      g.lineWidth = 1.3;
      g.stroke();
    };
    if (agendaItemIsBlocked(item)) ring(pal.rose, 3.4, 0.8 * alpha);
    if (st && (st.kind === 'standing' || st.kind === 'armed')) {
      ring(pal.green, 5.4, 0.7 * alpha);
    }
    if (st && st.kind === 'suspended') {
      ring(pal.amber, 5.4,
        (reduced ? 0.65 : 0.45 + 0.4 * Math.sin(ts / 280)) * alpha);
    }
    if (st && st.kind === 'pending') ring(pal.amber, 5.4, 0.75 * alpha);
    // Territory halo: a dotted outer ring on nodes carrying file/dir
    // refs — the declared working set made visible. Suppressed in
    // files mode, where nearly every clustered node would carry one
    // and the area boundary circles already say it.
    const territory = agendaGraphProjection === 'hubs'
      ? agendaGraphSubtreeTerr.get(node.id) || 0
      : (item.refs || []).filter(
        (r) => r.ref_type === 'file' || r.ref_type === 'dir',
      ).length;
    if (territory && agendaGraphMode !== 'files') {
      g.setLineDash([1.5, 3.2]);
      ring(pal.text, 7.6, 0.3 * alpha);
      g.setLineDash([]);
    }
    if (kids || hot) {
      g.font = `${hot ? '700' : '600'} 11px "Hanken Grotesk", sans-serif`;
      const title = String(item.title || '');
      const label = title.length > 34 ? `${title.slice(0, 33)}…` : title;
      const tw = g.measureText(label).width;
      let lx = q.x + r + 8;
      const ly = q.y + 3.5;
      // Flip the label to the node's left when it would clip the right
      // edge.
      if (lx + tw > w - 10) lx = q.x - r - 8 - tw;
      g.fillStyle = hot
        ? `rgba(${pal.text},.95)`
        : `rgba(${pal.text},${0.6 * alpha + 0.12})`;
      g.fillText(label, lx, ly);
      if (hot) {
        g.font = '9.5px "JetBrains Mono", monospace';
        g.fillStyle = pal.t3;
        const open = agendaGraphProjection === 'hubs'
          ? 'click to focus'
          : 'click to open';
        g.fillText(
          `${item.kind} · ${item.status}${kids ? ` · hub, ${kids} filed` : ''}${territory ? ` · ${territory} territory` : ''} — ${open}`,
          lx, ly + 13);
      }
    }
  });
  // Files-mode area labels paint over the nodes, on the outer rim at
  // each area's own angle with a short leader to its wash — outside is
  // where the empty pixels are, and the leader keeps the association
  // unambiguous at any camera.
  if (agendaGraphMode === 'files') {
    g.font = '600 9.5px "JetBrains Mono", monospace';
    const origin = project([0, 0, 0]);
    agendaGraphFilesMarks.forEach((mark) => {
      const cr = 16 + 9 * Math.sqrt(mark.count);
      const cosA = Math.cos(mark.angle);
      const sinA = Math.sin(mark.angle);
      const rim = project([cosA * (mark.r + cr), 0, sinA * (mark.r + cr)]);
      // Extend the leader in SCREEN space: world-radial extension
      // forshortens to nothing at the disc's top and bottom under
      // pitch, stranding those labels inside the node field.
      let dx = rim.x - origin.x;
      let dy = rim.y - origin.y;
      const dl = Math.hypot(dx, dy) || 1;
      dx /= dl;
      dy /= dl;
      const tip = { x: rim.x + dx * 20, y: rim.y + dy * 20 };
      g.beginPath();
      g.moveTo(rim.x, rim.y);
      g.lineTo(tip.x, tip.y);
      g.strokeStyle = `rgba(${pal.text},.25)`;
      g.lineWidth = 1;
      g.stroke();
      const label = `${mark.name} · ${mark.count}`;
      const tw = g.measureText(label).width;
      const leftSide = dx < -0.25;
      g.fillStyle = pal.t3;
      g.fillText(
        label,
        leftSide ? tip.x - tw - 4 : Math.abs(dx) <= 0.25 ? tip.x - tw / 2 : tip.x + 4,
        tip.y + (dy > 0.3 ? 10 : dy < -0.3 ? -4 : 3),
      );
    });
  }
  // Projection badge (painted, inert pixels like every label): what the
  // constellation is currently showing.
  const terrBadge = agendaGraphTerrStats.shown
    ? ` · territory ${agendaGraphTerrStats.shown}${agendaGraphTerrStats.total > agendaGraphTerrStats.shown ? ` of ${agendaGraphTerrStats.total}` : ''}`
    : '';
  const modeBadge = agendaGraphMode === 'orbit' ? ''
    : agendaGraphMode === 'attention'
      ? ` · attention — ${agendaGraphAttnCount} need you`
      : agendaGraphMode === 'time'
        ? ` · time — ${agendaGraphTimeSpanLabel()}`
        : agendaGraphMode === 'files'
          ? ` · files — ${agendaGraphFilesMarks.length
            ? `${agendaGraphFilesMarks.length} areas`
            : 'no declared territory yet'}`
          : ` · ${agendaGraphMode}`;
  const zoomBadge = Math.abs(cam.zoom - 1) > 0.01
    ? ` · ${cam.zoom.toFixed(1)}× (double-click empty to reset)`
    : '';
  {
    const allCount = (agendaItems || []).filter(
      (x) => x.status !== 'retired',
    ).length;
    const itemCount = nodes.filter((n) => !n.sat).length;
    let badge = '';
    if (agendaGraphProjection === 'hubs') {
      badge = `hub overview · ${itemCount} hubs of ${allCount} items`;
    } else if (agendaGraphProjection === 'focus') {
      const focused = byId.get(agendaGraphFocus);
      const title = focused ? String(focused.title || '') : '';
      const short = title.length > 40 ? `${title.slice(0, 39)}…` : title;
      badge = `focused: ${short} · ${itemCount} of ${allCount} items`;
    } else {
      badge = `everything · ${itemCount} items`;
    }
    g.font = '10px "JetBrains Mono", monospace';
    g.fillStyle = pal.t3;
    g.fillText(badge + modeBadge + terrBadge + zoomBadge, 14, 64);
  }
}

// ---- Wire (the one permanent listener; see the fragment header) ----

{
  const wire = () => {
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        // Hidden document: full stop, zero background frames.
        agendaGraphTeardown();
        return;
      }
      if (agendaLens === 'graph' && agendaTabVisible()) {
        // Resume through the render pass so the cap and empty states
        // re-apply before any frame is scheduled.
        agendaRenderTab();
      }
    });
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}
