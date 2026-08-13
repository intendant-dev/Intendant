// ui2-xr.js — XR spatial surface: header entry chip + lazy WASM boot.
//
// The XR surface (docs/src/xr.md) is the immersive presentation of the
// regular dashboard: it consumes the same coalesced client state and
// dispatches through the same handlers as every other tab — never a
// second brain. This fragment owns only the browser-side seam: feature
// detection, the entry chip, lazy module load, and session entry.
//
// SHIPPED DEFAULT: the chip appears on any browser reporting immersive
// support (milestone 1 graduated on the owner's Quest 3, 2026-08-13);
// `?xr=off` is the opt-out escape. Everything below is boot-callback or
// event-driven; top level declares only lets/consts/functions
// (deep-link TDZ rule).

let xrEnsurePromise = null; // single-flight init; cleared on failure
let xrInstance = null;      // XrWeb handle after first ensureXr()
let xrChipEl = null;
let xrSupport = { ar: false, vr: false };
let xrSnapshotTimer = null;
let xrCaptureOnly = false; // probe mode: record actions, don't route

function xrEnabled() {
  try {
    return new URLSearchParams(location.search).get('xr') !== 'off';
  } catch {
    return true;
  }
}

// Feature probe without loading the WASM: chip visibility must cost
// nothing on the overwhelmingly common non-XR browser.
async function xrProbeSupportLight() {
  const xr = navigator.xr;
  if (!xr || typeof xr.isSessionSupported !== 'function') {
    return { ar: false, vr: false };
  }
  const probe = async (mode) => {
    try {
      return await xr.isSessionSupported(mode) === true;
    } catch {
      return false;
    }
  };
  return { ar: await probe('immersive-ar'), vr: await probe('immersive-vr') };
}

async function ensureXr() {
  // Single-flight: the chip-mount preload and a click-time call race
  // otherwise — the loser constructed XrWeb against a module whose
  // wasm init had not resolved ("reading 'xrweb_new'" TypeError). The
  // PROMISE is the memo; a failed init clears it so retry restarts.
  if (xrInstance) return xrInstance;
  if (!xrEnsurePromise) {
    xrEnsurePromise = xrBootModule().catch((err) => {
      xrEnsurePromise = null;
      throw err;
    });
  }
  return xrEnsurePromise;
}

async function xrBootModule() {
  const mod = await import('/wasm-xr/xr_web.js');
  await mod.default({ module_or_path: '/wasm-xr/xr_web_bg.wasm' });
  xrInstance = new mod.XrWeb();
  xrInstance.setActionCallback((action) => {
    // Same dispatch seam as the other rendered surface: the emitted
    // objects carry the dashboard's action vocabulary and route through
    // the SAME router (approvals → send_approval / resolvePeerApproval).
    // The validator's probe flips captureOnly so asserted actions never
    // hit a live daemon.
    globalThis.xrProbe.lastAction = action;
    if (xrCaptureOnly) return;
    try {
      if (typeof handleStationAction === 'function') {
        handleStationAction(action);
      } else {
        console.warn('[xr] no action router in this build', action);
      }
    } catch (err) {
      console.warn('[xr] action dispatch failed', err, action);
    }
  });
  xrInstance.setOnSessionEnd(() => {
    xrStopSnapshotPump();
    xrSetChipState('idle', 'Enter XR');
    // Leave a readable trace of how the session went: frames=0 means the
    // loop never presented (entry-path failure), frames>0 means a live
    // session ended normally. This line is the whole diagnosis when the
    // headset has no devtools.
    try {
      const d = JSON.parse(xrInstance.debugJson());
      const frames = (d.engine && d.engine.framesRendered) || 0;
      xrShowStatus(
        frames > 0 ? 'busy' : 'error',
        `session ended — frames=${frames} views=${(d.engine && d.engine.views) || 0}`,
      );
    } catch {
      xrShowStatus('', '');
    }
  });
  await xrInstance.probeSupport();
  return xrInstance;
}

function xrSetChipState(state, title) {
  if (!xrChipEl) return;
  xrChipEl.dataset.state = state;
  if (title) xrChipEl.title = title;
}

// Mirror the dashboard's live display slots into the XR surface as
// floating monitors. Registrations are idempotent per source id, so the
// pump can re-scan cheaply; parked <video> elements need a play() kick
// exactly like the other rendered surface's registration path.
function xrSyncDisplaySources() {
  if (!xrInstance) return;
  try {
    if (typeof displaySlots === 'undefined' || !(displaySlots instanceof Map)) return;
    for (const slot of displaySlots.values()) {
      if (!slot || !slot.videoEl) continue;
      if (slot.videoEl.paused && slot.videoEl.srcObject) {
        slot.videoEl.play().catch(() => {});
      }
      const displayId = String(slot.displayId);
      const hostLabel = (typeof selfHostLabel !== 'undefined' && selfHostLabel) || 'local';
      const hostId = (typeof selfPeerId !== 'undefined' && selfPeerId) || 'local';
      xrInstance.registerDisplaySource(
        `local:${displayId}`,
        String(hostId),
        displayId,
        `${hostLabel} :${displayId}`,
        'video',
        slot.videoEl,
      );
    }
  } catch (err) {
    console.warn('[xr] display sync error', err);
  }
}

// Feed the XR surface the same coalesced client state the other rendered
// surface consumes (fragment 35's buildStationSnapshot) while a session
// is live. Fail-soft: a feed error must never take the session down.
function xrStartSnapshotPump() {
  xrStopSnapshotPump();
  xrSnapshotTimer = setInterval(() => {
    if (!xrInstance) return;
    try {
      if (typeof buildStationSnapshot === 'function') {
        xrInstance.updateSnapshot(buildStationSnapshot());
      }
    } catch (err) {
      console.warn('[xr] snapshot pump error', err);
    }
    xrSyncDisplaySources();
  }, 300);
}

function xrStopSnapshotPump() {
  if (xrSnapshotTimer) {
    clearInterval(xrSnapshotTimer);
    xrSnapshotTimer = null;
  }
}

// Visible status line beside the chip. Tooltips don't exist inside a
// headset, so entry progress and failures must be rendered text.
function xrShowStatus(state, text) {
  if (!xrChipEl) return;
  let el = document.getElementById('ui2-xr-status');
  if (!text) {
    if (el) el.remove();
    return;
  }
  if (!el) {
    el = document.createElement('span');
    el.id = 'ui2-xr-status';
    el.className = 'ui2-xr-status';
    xrChipEl.insertAdjacentElement('afterend', el);
  }
  el.dataset.state = state;
  el.textContent = text;
}

async function xrEnter() {
  xrSetChipState('busy', 'Starting immersive session…');
  xrShowStatus('busy', 'starting…');
  // The permission dialog can take a while to be noticed in-headset; an
  // unsettled entry is almost always the consent prompt waiting, not a
  // hang. Say so where the operator can read it.
  const consentHint = setTimeout(() => {
    xrShowStatus('busy', 'waiting for the headset permission dialog — look for an Allow prompt');
  }, 6000);
  try {
    // The module is preloaded at chip mount so this click reaches
    // requestSession while its user activation is still fresh — a slow
    // module fetch here used to burn the activation window.
    const inst = xrInstance || await ensureXr();
    // Passthrough-first: prefer AR on hardware that has it (Quest 3),
    // fall back to VR (Vision Pro Safari has no immersive-ar).
    const mode = xrSupport.ar ? 'immersive-ar' : 'immersive-vr';
    await inst.enter(mode);
    clearTimeout(consentHint);
    xrStartSnapshotPump();
    xrSetChipState('active', 'Immersive session running');
    xrShowStatus('', '');
  } catch (err) {
    clearTimeout(consentHint);
    const detail = (err && (err.message || err.name)) || String(err);
    xrSetChipState('error', 'XR entry failed: ' + detail);
    // Persist the failure where a headset user can read it; clear on the
    // next attempt instead of a timer. Mirror it to the console and the
    // QA facade for every surface that CAN read those.
    xrShowStatus('error', 'XR entry failed: ' + detail);
    console.error('[xr] entry failed:', err);
    globalThis.xrProbe.lastError = detail;
    setTimeout(() => xrSetChipState('idle', 'Enter XR'), 4000);
  }
}

function xrMountChip() {
  if (xrChipEl) return;
  const host = document.getElementById('ui2-oversight');
  if (!host) return;
  const chip = document.createElement('button');
  chip.type = 'button';
  chip.id = 'ui2-xr-chip';
  chip.className = 'ui2-xr-chip';
  chip.dataset.state = 'idle';
  chip.title = 'Enter XR';
  chip.setAttribute('aria-label', 'Enter immersive XR session');
  chip.innerHTML = '<span class="ui2-xr-chip-glyph" aria-hidden="true">◈</span><span class="ui2-xr-chip-label">XR</span>';
  chip.addEventListener('click', () => { xrEnter(); });
  host.appendChild(chip);
  xrChipEl = chip;
}

(async () => {
  if (!xrEnabled()) return;
  xrSupport = await xrProbeSupportLight();
  if (!xrSupport.ar && !xrSupport.vr) return;
  xrMountChip();
  // Preload the WASM module now so the click handler reaches
  // requestSession within its transient user-activation window — the
  // module fetch must never sit between the tap and the session request.
  try {
    await ensureXr();
  } catch (err) {
    xrShowStatus('error', 'XR module failed to load: ' + ((err && err.message) || err));
  }
})();

// QA facade, mirroring the stationProbe convention: the validator's
// --xr-probe drives the surface through here.
globalThis.xrProbe = {
  enabled: xrEnabled,
  support: () => ({ ...xrSupport }),
  chip: () => xrChipEl,
  ensure: ensureXr,
  enter: xrEnter,
  debugJson: async () => JSON.parse((await ensureXr()).debugJson()),
  // Direct snapshot push (bypasses the pump) for deterministic probes.
  update: (snapshot) => { if (xrInstance) xrInstance.updateSnapshot(snapshot); },
  activate: (name) => (xrInstance ? xrInstance.activate(name) : false),
  captureOnly: (on) => { xrCaptureOnly = !!on; },
  lastAction: null,
};

// ── XR terminal pane (slice 1: read-only watching) ──
//
// Appended section: mirrors the flat Terminal tab's standalone shell
// into the XR scene. The dashboard machinery (44-shell-frames.js) owns
// the PTY attach and the xterm buffer; this section only *watches* —
// it paints that buffer onto an offscreen canvas and registers it
// through the WASM facade's canvas seam. XR never sends terminal_open
// (the daemon's open_or_attach would SPAWN a shell when none exists —
// a watch surface must not create PTYs), so a page with no terminal
// session gets the honest in-scene empty state instead. Everything here
// is fail-soft: a painter error degrades the pane, never the session.

const XR_TERM_SOURCE_ID = 'term:shell';
const XR_TERM_FONT_PX = 17;
let xrTermCanvas = null;
let xrTermCtx = null;
let xrTermCellW = 0;
let xrTermCellH = 0;
let xrTermAspect = 0;      // canvas height/width after the first paint
let xrTermDirty = false;   // buffer advanced since the last paint
let xrTermBound = false;   // onWriteParsed/onResize hooks installed
let xrTermRegistered = false;
let xrTermLastPushedKey = '';
let xrTermWarned = false;
let xrTermPaletteCache = null;
let xrTermPaletteKey = '';

// ui-v2 fallback palette for the degenerate case where the token theme
// is unavailable (mirrors ui2ShellTheme()'s source tokens).
function xrTermFallbackTheme() {
  return {
    background: '#0B0D12', foreground: '#EAECF2',
    black: '#232834', red: '#EC6A85', green: '#58C08C', yellow: '#E4A85B',
    blue: '#7E8CFA', magenta: '#9B7CF2', cyan: '#5DA9E6', white: '#A7AEBE',
    brightBlack: '#7E8896', brightRed: '#EC6A85', brightGreen: '#58C08C',
    brightYellow: '#E4A85B', brightBlue: '#A6AEFF', brightMagenta: '#9B7CF2',
    brightCyan: '#5DA9E6', brightWhite: '#EAECF2',
  };
}

// ANSI-256 palette: the 16 theme colors (same token mapping the flat
// terminal resolves), then the 6x6x6 cube and the grayscale ramp.
function xrTermPalette(theme) {
  const key = [theme.background, theme.foreground, theme.black, theme.white].join('|');
  if (xrTermPaletteCache && xrTermPaletteKey === key) return xrTermPaletteCache;
  const p = [
    theme.black, theme.red, theme.green, theme.yellow,
    theme.blue, theme.magenta, theme.cyan, theme.white,
    theme.brightBlack, theme.brightRed, theme.brightGreen, theme.brightYellow,
    theme.brightBlue, theme.brightMagenta, theme.brightCyan, theme.brightWhite,
  ];
  const lv = [0, 95, 135, 175, 215, 255];
  for (let i = 16; i < 232; i++) {
    const j = i - 16;
    p.push(`rgb(${lv[(j / 36) | 0]},${lv[((j / 6) | 0) % 6]},${lv[j % 6]})`);
  }
  for (let i = 232; i < 256; i++) {
    const v = 8 + (i - 232) * 10;
    p.push(`rgb(${v},${v},${v})`);
  }
  xrTermPaletteCache = p;
  xrTermPaletteKey = key;
  return p;
}

// Derive the pane's state from the flat tab's own machinery: presence,
// the PTY's real id + host label, and the status line verbatim (the
// status vocabulary is the flat tab's, ported — never re-invented).
function xrTermPageState() {
  let present = false;
  let live = false;
  let label = 'terminal';
  let status = '';
  let statusKind = '';
  try {
    present = typeof shellInitialized !== 'undefined' && shellInitialized === true;
    live = present && typeof shellOpenAcked !== 'undefined' && shellOpenAcked === true;
    if (present) {
      const termId = (typeof SHELL_TERMINAL_ID !== 'undefined' && SHELL_TERMINAL_ID) || 'shell-0';
      const host = (typeof shellHostLabel === 'function' && shellHostLabel()) || 'this daemon';
      label = `${termId} · ${host}`;
    }
    const el = document.getElementById('shell-host-status');
    if (el) {
      status = (el.textContent || '').trim();
      if (el.classList.contains('ok')) statusKind = 'ok';
      else if (el.classList.contains('warn')) statusKind = 'warn';
      else if (el.classList.contains('error')) statusKind = 'error';
    }
  } catch (_) { /* partial state is fine — fail-soft */ }
  return { present, live, label, status, statusKind, aspect: xrTermAspect };
}

// Hook the live xterm instance once it exists: buffer writes and
// resizes mark the painter dirty (xterm keeps its buffer current even
// while the flat tab is hidden, so the mirror stays live off-tab).
function xrTermBind() {
  if (xrTermBound || typeof shellTerm === 'undefined' || !shellTerm) return;
  xrTermBound = true;
  xrTermDirty = true;
  try {
    shellTerm.onWriteParsed(() => { xrTermDirty = true; });
    shellTerm.onResize(() => { xrTermDirty = true; });
    // Theme flips repaint with the freshly resolved tokens.
    new MutationObserver(() => { xrTermDirty = true; })
      .observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  } catch (err) {
    console.warn('[xr] terminal hook failed; falling back to timed repaints', err);
  }
}

// Paint the live tail of the shell buffer (the bottom screen — watching
// follows the PTY, not the dashboard user's scrollback position) onto
// the offscreen canvas: cell backgrounds, glyphs with the cell's SGR
// attributes, and a hollow cursor block (read-only — not an input
// caret). Registers the canvas on first paint, then marks dirty so the
// encoder re-uploads exactly once per painted frame.
function xrTermPaint() {
  if (typeof shellTerm === 'undefined' || !shellTerm || !xrInstance) return;
  const buf = shellTerm.buffer && shellTerm.buffer.active;
  if (!buf) return;
  const cols = shellTerm.cols || 80;
  const rows = shellTerm.rows || 24;
  const theme = (typeof ui2ShellTheme === 'function' && ui2ShellTheme()) || xrTermFallbackTheme();
  const palette = xrTermPalette(theme);

  if (!xrTermCanvas) {
    xrTermCanvas = document.createElement('canvas');
    xrTermCtx = xrTermCanvas.getContext('2d');
    if (!xrTermCtx) { xrTermCanvas = null; return; }
  }
  const mono = (getComputedStyle(document.documentElement).getPropertyValue('--mono') || '').trim()
    || "'JetBrains Mono', ui-monospace, Menlo, monospace";
  const baseFont = `${XR_TERM_FONT_PX}px ${mono}`;
  if (!xrTermCellW) {
    xrTermCtx.font = baseFont;
    xrTermCellW = xrTermCtx.measureText('M').width || XR_TERM_FONT_PX * 0.6;
    xrTermCellH = Math.ceil(XR_TERM_FONT_PX * 1.4);
  }
  const wantW = Math.round(cols * xrTermCellW);
  const wantH = rows * xrTermCellH;
  if (xrTermCanvas.width !== wantW || xrTermCanvas.height !== wantH) {
    xrTermCanvas.width = wantW;
    xrTermCanvas.height = wantH;
  }
  const ctx = xrTermCtx;
  ctx.font = baseFont;
  ctx.textBaseline = 'middle';
  ctx.fillStyle = theme.background;
  ctx.fillRect(0, 0, wantW, wantH);

  const nullCell = typeof buf.getNullCell === 'function' ? buf.getNullCell() : undefined;
  for (let y = 0; y < rows; y++) {
    const line = buf.getLine(buf.baseY + y);
    if (!line) continue;
    const cy = y * xrTermCellH + xrTermCellH / 2;
    for (let x = 0; x < cols; x++) {
      const cell = nullCell ? line.getCell(x, nullCell) : line.getCell(x);
      if (!cell) continue;
      const width = cell.getWidth();
      if (width === 0) continue; // continuation of a wide glyph
      const cx = x * xrTermCellW;

      let fg;
      let fgIdx = -1;
      if (cell.isFgDefault()) fg = theme.foreground;
      else if (cell.isFgRGB()) {
        const v = cell.getFgColor();
        fg = `rgb(${(v >> 16) & 255},${(v >> 8) & 255},${v & 255})`;
      } else {
        fgIdx = cell.getFgColor();
        fg = palette[fgIdx] || theme.foreground;
      }
      let bg = null;
      if (cell.isBgRGB()) {
        const v = cell.getBgColor();
        bg = `rgb(${(v >> 16) & 255},${(v >> 8) & 255},${v & 255})`;
      } else if (!cell.isBgDefault()) {
        bg = palette[cell.getBgColor()] || null;
      }
      const bold = cell.isBold && cell.isBold() !== 0;
      if (bold && fgIdx >= 0 && fgIdx < 8) fg = palette[fgIdx + 8] || fg;
      if (cell.isInverse && cell.isInverse() !== 0) {
        const swapped = bg || theme.background;
        bg = fg;
        fg = swapped;
      }
      if (bg) {
        ctx.fillStyle = bg;
        ctx.fillRect(cx, y * xrTermCellH, xrTermCellW * width, xrTermCellH);
      }
      const chars = cell.getChars();
      const invisible = cell.isInvisible && cell.isInvisible() !== 0;
      const underline = cell.isUnderline && cell.isUnderline() !== 0;
      const drawGlyph = !!chars && chars !== ' ' && !invisible;
      if (!drawGlyph && !underline) continue;
      const italic = cell.isItalic && cell.isItalic() !== 0;
      const dim = cell.isDim && cell.isDim() !== 0;
      if (bold || italic) {
        ctx.font = `${italic ? 'italic ' : ''}${bold ? 'bold ' : ''}${baseFont}`;
      }
      if (dim) ctx.globalAlpha = 0.55;
      ctx.fillStyle = fg;
      if (drawGlyph) ctx.fillText(chars, cx, cy);
      if (underline) {
        // Underline spans the cell (spaces included), like the flat
        // terminal's renderer.
        ctx.fillRect(cx, y * xrTermCellH + xrTermCellH - 2, xrTermCellW * width, 1);
      }
      if (dim) ctx.globalAlpha = 1;
      if (bold || italic) ctx.font = baseFont;
    }
  }

  // Hollow cursor block: the session is being watched, not driven.
  if (typeof shellOpenAcked !== 'undefined' && shellOpenAcked
      && buf.cursorY >= 0 && buf.cursorY < rows && buf.cursorX < cols) {
    ctx.strokeStyle = theme.foreground;
    ctx.lineWidth = 1;
    ctx.strokeRect(
      buf.cursorX * xrTermCellW + 0.5,
      buf.cursorY * xrTermCellH + 0.5,
      xrTermCellW - 1,
      xrTermCellH - 1,
    );
  }

  xrTermAspect = wantW > 0 ? wantH / wantW : 0;
  if (!xrTermRegistered) {
    // Registration counts as painted — no extra dirty mark needed.
    xrInstance.registerTerminalCanvas(XR_TERM_SOURCE_ID, xrTermCanvas);
    xrTermRegistered = true;
  } else {
    xrInstance.markTerminalCanvasDirty(XR_TERM_SOURCE_ID);
  }
}

// Terminal sync pump: push pane state on change (cheap key compare —
// change-only pushes also keep probe-injected state stable), and paint
// at most ~4 Hz while immersive and dirty. Painting lives on the page
// interval, never inside the XR frame loop.
setInterval(() => {
  try {
    if (!xrInstance || typeof xrInstance.updateTerminal !== 'function') return;
    xrTermBind();
    const state = xrTermPageState();
    const key = JSON.stringify(state);
    if (key !== xrTermLastPushedKey) {
      xrTermLastPushedKey = key;
      xrInstance.updateTerminal(state);
    }
    const immersive = !!(xrChipEl && xrChipEl.dataset.state === 'active');
    if (immersive && xrTermDirty && typeof shellTerm !== 'undefined' && shellTerm) {
      xrTermDirty = false;
      xrTermPaint();
    }
  } catch (err) {
    if (!xrTermWarned) {
      xrTermWarned = true;
      console.warn('[xr] terminal sync error', err);
    }
  }
}, 250);

// QA facade extension (same conventions as the base xrProbe surface):
// direct pushes through the terminal seam for deterministic probes.
globalThis.xrProbe.terminal = {
  update: (state) => { if (xrInstance) xrInstance.updateTerminal(state); },
  registerCanvas: (id, canvas) => { if (xrInstance) xrInstance.registerTerminalCanvas(id, canvas); },
  markDirty: (id) => { if (xrInstance) xrInstance.markTerminalCanvasDirty(id); },
  unregister: (id) => { if (xrInstance) xrInstance.unregisterTerminalCanvas(id); },
  repaint: () => { xrTermDirty = true; },
};
