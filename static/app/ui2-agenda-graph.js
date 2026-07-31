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
let agendaGraphCam = { yaw: 0.6, pitch: -0.34, auto: true };
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
// layered left→right), 'time' (age as radius, newest center), 'actor'
// (clustered by parking provenance), 'attention' (needs-you gravity).
// Modes bias the layout and emphasis only; pool, links, picking, caps,
// and card-lens parity are untouched. Build computes the active mode's
// per-node targets; relax applies them as one extra spring.
let agendaGraphMode = 'orbit';
const AGENDA_GRAPH_MODES = [
  ['flow', 'flow', 'dependency flow — prerequisites left, dependents right'],
  ['time', 'time', 'age as radius — newest at the center'],
  ['actor', 'actor', 'clustered by who parked it'],
  ['attention', 'attention', 'needs-you gravity — attention items pull center'],
];
let agendaGraphTimeMarks = [];
let agendaGraphActorMarks = [];
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

function agendaGraphEnsureEsc() {
  if (agendaGraphEscHook) return;
  agendaGraphEscHook = (e) => {
    if (e.key === 'Escape' && !e.defaultPrevented) {
      agendaGraphSetFullscreen(false);
    }
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
          : 'drag to orbit · click a node to open it · double-click a hub to focus');
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
        agendaGraphCam.yaw += (nx - m.x) * 0.005;
        agendaGraphCam.pitch = Math.max(-1.2, Math.min(1.2,
          agendaGraphCam.pitch + (ny - m.y) * 0.004));
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
      // space to clear an active focus.
      if (agendaGraphHover && agendaChildrenOf(agendaGraphHover).length) {
        agendaGraphFocus = agendaGraphHover;
        agendaRenderTab();
      } else if (!agendaGraphHover && agendaGraphFocus) {
        agendaGraphFocus = null;
        agendaRenderTab();
      }
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
  agendaGraphMouse.down = false;
  agendaGraphMouse.moved = 0;
  agendaGraphHover = null;
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
    const born = (x) => (x.provenance && x.provenance.created_ms) || x.updated_ms || 0;
    const times = items.map(born);
    const newest = Math.max(...times, 1);
    const oldest = Math.min(...times, newest);
    const span = Math.max(1, newest - oldest);
    agendaGraphNodes.forEach((n) => {
      if (n.sat) return;
      const x = byIdBuild.get(n.id);
      n.tr = 36 + ((newest - born(x)) / span) * 150;
    });
    const day = (ms) => new Date(ms).toISOString().slice(5, 10);
    agendaGraphTimeMarks = [
      { r: 36, label: `newest · ${day(newest)}` },
      { r: 36 + 75, label: day(oldest + span / 2) },
      { r: 186, label: `oldest · ${day(oldest)}` },
    ];
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
      const k = link.t === 'place' ? 0.014 : link.t === 'terr' ? 0.012 : 0.007;
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
        const cur = Math.hypot(n.p[0], n.p[2]) + 0.01;
        const f = ((n.tr - cur) * 0.035) / cur;
        n.p[0] += n.p[0] * f;
        n.p[2] += n.p[2] * f;
        n.p[1] *= 0.97;
      }
      if (n.anchor) {
        n.p[0] += (n.anchor[0] - n.p[0]) * 0.025;
        n.p[1] += (n.anchor[1] - n.p[1]) * 0.02;
        n.p[2] += (n.anchor[2] - n.p[2]) * 0.025;
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
  if (agendaGraphSettleLeft > 0) {
    const step = Math.min(agendaGraphSettleLeft,
      agendaGraphSettleBudget(agendaGraphNodes.length));
    agendaGraphRelax(step);
    agendaGraphSettleLeft -= step;
  }
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
    return { x: cx + rx * s * 1.35, y: cyy + ry * s * 1.35, s, z: rz };
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
      g.setLineDash([]);
      g.strokeStyle = `rgba(${pal.iris},${depth * (hot ? 0.75 : 0.38)})`;
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
  if (agendaGraphMode === 'time' && agendaGraphTimeMarks.length) {
    g.font = '9px "JetBrains Mono", monospace';
    agendaGraphTimeMarks.forEach((mark) => {
      const sr = mark.r * 1.28;
      g.beginPath();
      g.arc(cx, cyy, sr, 0, Math.PI * 2);
      g.strokeStyle = `rgba(${pal.text},.07)`;
      g.lineWidth = 1;
      g.stroke();
      g.fillStyle = pal.t3;
      g.fillText(mark.label, cx + sr * 0.72, cyy - sr * 0.72);
    });
  }
  if (agendaGraphMode === 'actor') {
    g.font = '600 9.5px "JetBrains Mono", monospace';
    agendaGraphActorMarks.forEach((mark) => {
      const q = project([Math.cos(mark.angle) * 172, -34, Math.sin(mark.angle) * 172]);
      g.fillStyle = pal.t3;
      g.fillText(`${mark.name} · ${mark.count}`, q.x - 20, q.y);
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
    const r = (3.4 + Math.min(kids, 4) * 1.3 + (hot ? 1.4 : 0)) * q.s;
    let alpha = Math.max(0.35, Math.min(1, q.s * 1.15 - 0.1));
    // Attention mode: needs-you items glow, the rest recede.
    if (agendaGraphMode === 'attention' && !node.attn && !hot) alpha *= 0.45;
    const glow = agendaGraphGlowSprite(rgb);
    const gr = r * (hot ? 6.5
      : agendaGraphMode === 'attention' && node.attn ? 7.5 : 4.5);
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
    // refs — the declared working set made visible.
    const territory = agendaGraphProjection === 'hubs'
      ? agendaGraphSubtreeTerr.get(node.id) || 0
      : (item.refs || []).filter(
        (r) => r.ref_type === 'file' || r.ref_type === 'dir',
      ).length;
    if (territory) {
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
  // Projection badge (painted, inert pixels like every label): what the
  // constellation is currently showing.
  const terrBadge = agendaGraphTerrStats.shown
    ? ` · territory ${agendaGraphTerrStats.shown}${agendaGraphTerrStats.total > agendaGraphTerrStats.shown ? ` of ${agendaGraphTerrStats.total}` : ''}`
    : '';
  const modeBadge = agendaGraphMode === 'orbit' ? ''
    : agendaGraphMode === 'attention'
      ? ` · attention — ${agendaGraphAttnCount} need you`
      : ` · ${agendaGraphMode}`;
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
    g.fillText(badge + modeBadge + terrBadge, 14, 64);
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
