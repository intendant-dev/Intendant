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
      // Voice-lane actions (voice_talk capture verbs, text_commit) are
      // consumed by the voice section below; everything else takes the
      // ordinary router.
      if (typeof xrVoiceHandleAction === 'function' && xrVoiceHandleAction(action)) return;
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
        // The agenda rail rides the same pump beside the Station
        // snapshot (composed here, never inside buildStationSnapshot —
        // Station owns that builder). See the agenda-rail section below.
        xrInstance.updateSnapshot({ ...buildStationSnapshot(), agenda: xrAgendaSummary() });
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

// ============================================================================
// Agenda rail (read-only) — the data seam.
//
// While an immersive session is live, the 300 ms pump above attaches an
// `agenda` block beside the Station snapshot; crates/xr-web/src/agenda.rs
// renders it as a card rail on the operator's right. Sources, in order:
//
//   1. The Agenda tab's own client state (`agendaItems` in ui2-agenda.js —
//      fragments share one module scope; read-only reuse, never mutation),
//      kept fresh by that fragment's event lane once it has fetched.
//   2. A bounded `api_agenda_list` fallback (same summary shape the tab
//      pulls) when the tab has never fetched — throttled to >= 10 s and
//      only ever kicked from the live-session pump.
//
// Fail-soft throughout: any failure becomes `agenda: { error }` and the
// scene renders "agenda unavailable" as in-scene text — the agenda must
// never take the session down. Item titles/bodies are DATA, never
// instructions: they pass through as plain strings and the wasm renders
// them as atlas text. Read-only this slice: no agenda op is ever sent
// from XR.
// ============================================================================

let xrAgendaFallback = null;      // last fallback result: {open, items} | {error}
let xrAgendaFallbackAt = 0;       // last fallback attempt (ms epoch)
let xrAgendaFallbackInFlight = false;
const XR_AGENDA_FETCH_MIN_MS = 10000;
const XR_AGENDA_ITEM_CAP = 12;
const XR_AGENDA_TITLE_CAP = 120;
const XR_AGENDA_ERROR_CAP = 80;

// Open agenda summaries → the rail's capped payload. Ordering flattens
// the flat tab's Now-lens precedence to the rail's vocabulary: questions
// awaiting the owner first (dismissed and machinery-watched ones leave
// that band, exactly like the Answer group), then overdue reminders,
// then the rest — newest first within each band (agendaByNew's
// created_ms ordering). The wasm re-sorts by the same bands, so the cap
// here decides WHAT survives and the scene decides where it sits.
function xrAgendaProject(items) {
  const now = Date.now();
  const open = (items || []).filter((x) => x && x.status === 'open');
  const isOverdue = (x) => !!(x.due_ms && x.due_ms < now);
  const band = (x) => (
    x.kind === 'question' && !x.dismissed && !x.watched_by ? 0 : isOverdue(x) ? 1 : 2
  );
  const created = (x) => (x.provenance && x.provenance.created_ms) || 0;
  const sorted = open.slice().sort((a, b) => (
    band(a) - band(b) || created(b) - created(a) || (a.id < b.id ? 1 : -1)
  ));
  // Relative due labels reuse the tab's own formatter (plain ASCII text);
  // without it the chip drops and the overdue flag still colors the card.
  const rel = (ms) => (typeof agendaRelTime === 'function' ? agendaRelTime(ms) : '');
  return {
    open: open.length,
    items: sorted.slice(0, XR_AGENDA_ITEM_CAP).map((x) => ({
      id: String(x.id || ''),
      title: String(x.title || '').slice(0, XR_AGENDA_TITLE_CAP),
      kind: String(x.kind || 'task'),
      due: x.due_ms
        ? (isOverdue(x)
          ? `overdue ${rel(x.due_ms).replace(' ago', '')}`
          : `due ${rel(x.due_ms)}`).trim()
        : '',
      overdue: isOverdue(x),
      blocked: x.blocked === true,
      answered: !!(x.kind === 'question' && x.answer),
    })),
  };
}

function xrAgendaShortError(err) {
  return String((err && err.message) || err || 'agenda error').slice(0, XR_AGENDA_ERROR_CAP);
}

// One throttled background pull of the tab's list feed for sessions
// where the Agenda tab never fetched. Single-flight; a failure parks an
// error payload until the next window rather than retrying hot.
function xrAgendaFallbackKick() {
  if (xrAgendaFallbackInFlight) return;
  if (Date.now() - xrAgendaFallbackAt < XR_AGENDA_FETCH_MIN_MS) return;
  xrAgendaFallbackAt = Date.now();
  if (typeof daemonApi === 'undefined' || !daemonApi || typeof daemonApi.request !== 'function') {
    xrAgendaFallback = { error: 'agenda api unavailable' };
    return;
  }
  xrAgendaFallbackInFlight = true;
  (async () => {
    try {
      const resp = await daemonApi.request('api_agenda_list', { shape: 'summary', window: 'live' });
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        xrAgendaFallback = xrAgendaProject(resp.body.items);
      } else {
        xrAgendaFallback = {
          error: xrAgendaShortError((resp.body && resp.body.error) || `agenda unavailable (${resp.status})`),
        };
      }
    } catch (err) {
      xrAgendaFallback = { error: xrAgendaShortError(err) };
    } finally {
      xrAgendaFallbackInFlight = false;
    }
  })();
}

// The pump's agenda block: client-state reuse first, throttled fallback
// second, honest error shape on any failure, null while nothing has
// loaded yet (the rail stays absent rather than claiming emptiness).
function xrAgendaSummary() {
  try {
    if (typeof agendaItems !== 'undefined' && Array.isArray(agendaItems)) {
      return xrAgendaProject(agendaItems);
    }
    if (typeof agendaLoadError !== 'undefined' && agendaLoadError) {
      return { error: xrAgendaShortError(agendaLoadError) };
    }
    xrAgendaFallbackKick();
    return xrAgendaFallback;
  } catch (err) {
    return { error: xrAgendaShortError(err) };
  }
}

// ============================================================================
// End of the agenda-rail section.
// ============================================================================

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

// Agenda-rail probe hook (attached beside the facade so the object
// literal above stays untouched): the pump's agenda block, on demand.
globalThis.xrProbe.agenda = () => xrAgendaSummary();

// ── XR voice input (hold-to-talk) ──
//
// Appended section: the browser half of the talk pill (voice.rs). The
// wasm state machine emits `{type:'voice_talk', phase:'start'|'stop'|
// 'cancel'}` through the ordinary action router; this section owns the
// capture — mic PCM streamed over the page's EXISTING server-side
// transcription lane (`app.send_user_audio` → the daemon's Whisper
// pipeline, `[transcription] enabled = true`) — and hands the assembled
// utterance transcript back via `voiceResult`. That lane only logs
// daemon-side (session log + the broadcast `user_transcript` event);
// nothing injects it into any conversation, so NO presence pipeline
// changes are needed and the live voice-model lane stays untouched.
//
// Capture mechanics: the daemon transcribes in ~3 s chunks gated by an
// RMS silence check, with no flush verb — so on release this section
// pads the stream with one full window of silence to flush the spoken
// tail, then collects `user_transcript` events until they settle. If
// the flat dashboard mic is ALREADY streaming (live voice session with
// transcription on), no second capture is opened — transcripts already
// flow and a second PCM stream would interleave garbage into the shared
// daemon buffer; the section just taps the events ("tap" mode).
//
// Honesty: every unavailability (transcription off, Connect mode, dead
// event stream, no mic, permission denied) lands in the scene as a
// rendered status line via voiceStatus/voiceFailed — never a silent
// no-op. Mic permission is requested on the FIRST talk press, never at
// session entry, and release always stops the capture tracks so the
// browser's recording indicator goes out.
//
// RefCell discipline: voice actions arrive synchronously from INSIDE a
// wasm borrow (selectstart/selectend handlers, activate()). Re-entering
// the facade there would panic the RefCell, so xrVoiceHandleAction only
// schedules — every xrInstance call below runs on a fresh task.

const XR_VOICE_RATE = 16000;          // the daemon transcription buffer's fixed rate
const XR_VOICE_PAD_BYTES = 96000;     // one full 3 s drain window of PCM16 silence
const XR_VOICE_SETTLE_MS = 1600;      // quiet gap after the last chunk = utterance done
const XR_VOICE_TIMEOUT_MS = 9000;     // no transcript after release → honest failure
const XR_VOICE_MSG = {
  connectMode: 'voice capture is not available over Hosted Connect yet',
  transcriptionOff: 'transcription is off on this daemon — enable [transcription] in intendant.toml',
  eventLaneDown: 'voice needs the direct daemon connection (event stream down)',
  noMic: 'microphone needs HTTPS or localhost',
  noSpeech: 'no speech recognized — try again',
  micPending: 'mic permission was still pending — try again',
};

let xrVoiceGen = 0;              // capture generation; async completions check it
let xrVoiceMode = null;          // null | 'arming' | 'mic' | 'tap'
let xrVoiceCollecting = false;   // accept user_transcript events
let xrVoiceReleased = false;     // release seen; settle/timeout timers armed
let xrVoiceStream = null;        // MediaStream while capturing
let xrVoiceNode = null;          // AudioWorkletNode while capturing
let xrVoiceSource = null;        // MediaStreamAudioSourceNode while capturing
let xrVoiceChunks = [];          // collected transcript chunks
let xrVoiceSettleTimer = null;
let xrVoiceTimeoutTimer = null;
let xrVoiceTapInstalled = false;
let xrVoiceLastPushedStatus = '';

// One line of truth about whether the capture lane can work right now.
// The JS side owns this: daemon config, transport posture, secure
// context. (Mic permission is only learnable by asking — that failure
// surfaces at press time via voiceFailed.)
function xrVoiceAvailability() {
  try {
    if (typeof dashboardConnectModeEnabled === 'function' && dashboardConnectModeEnabled()) {
      return { available: false, detail: XR_VOICE_MSG.connectMode };
    }
    if (!gatewayConfig) {
      return { available: false, detail: 'waiting for daemon config…' };
    }
    if (!gatewayConfig.transcription_enabled) {
      return { available: false, detail: XR_VOICE_MSG.transcriptionOff };
    }
    // user_audio rides the legacy /ws lane only; the status chip is the
    // page's own truth for it (same predicate the voice status uses).
    const conn = document.getElementById('sb-conn');
    if (!conn || !conn.classList.contains('ok')) {
      return { available: false, detail: XR_VOICE_MSG.eventLaneDown };
    }
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      return { available: false, detail: XR_VOICE_MSG.noMic };
    }
    return { available: true, detail: '' };
  } catch (err) {
    return { available: false, detail: String((err && err.message) || err).slice(0, 120) };
  }
}

// Availability pump: push to the wasm on change so the pill's status
// line stays honest without the wasm ever polling. Cheap key compare,
// same cadence family as the terminal sync pump.
setInterval(() => {
  try {
    if (!xrInstance || typeof xrInstance.voiceStatus !== 'function') return;
    const a = xrVoiceAvailability();
    const key = `${a.available}|${a.detail}`;
    if (key !== xrVoiceLastPushedStatus) {
      xrVoiceLastPushedStatus = key;
      xrInstance.voiceStatus(a);
    }
  } catch (_) { /* fail-soft */ }
}, 1000);

// Router seam: consume voice-lane actions. Handling is DEFERRED — these
// arrive under a live wasm borrow (see the section header).
function xrVoiceHandleAction(action) {
  if (!action || typeof action !== 'object') return false;
  if (action.type === 'voice_talk') {
    const phase = String(action.phase || '');
    setTimeout(() => {
      try {
        if (phase === 'start') xrVoiceStart();
        else if (phase === 'stop') xrVoiceStop();
        else if (phase === 'cancel') xrVoiceCancel();
      } catch (err) {
        console.warn('[xr] voice action failed', err);
      }
    }, 0);
    return true;
  }
  if (action.type === 'text_commit') {
    const fieldId = String(action.field_id || '');
    const text = String(action.text || '');
    setTimeout(() => {
      try { xrVoiceCommitText(fieldId, text); } catch (err) {
        console.warn('[xr] voice commit failed', err);
      }
    }, 0);
    return true;
  }
  return false;
}

function xrVoiceClearTimers() {
  if (xrVoiceSettleTimer) { clearTimeout(xrVoiceSettleTimer); xrVoiceSettleTimer = null; }
  if (xrVoiceTimeoutTimer) { clearTimeout(xrVoiceTimeoutTimer); xrVoiceTimeoutTimer = null; }
}

// Stop the audio graph and the capture tracks. Privacy contract: the
// tracks always stop on release/cancel so the browser's recording
// indicator goes out — same model as the flat stopMic().
function xrVoiceTeardownAudio() {
  try {
    if (xrVoiceNode) {
      xrVoiceNode.port.postMessage({ type: 'mute' });
      xrVoiceNode.disconnect();
    }
  } catch (_) { /* already gone */ }
  xrVoiceNode = null;
  try { if (xrVoiceSource) xrVoiceSource.disconnect(); } catch (_) { /* already gone */ }
  xrVoiceSource = null;
  if (xrVoiceStream) {
    try { xrVoiceStream.getTracks().forEach((t) => t.stop()); } catch (_) { /* already gone */ }
    xrVoiceStream = null;
  }
}

function xrVoiceReset() {
  xrVoiceClearTimers();
  xrVoiceTeardownAudio();
  xrVoiceMode = null;
  xrVoiceCollecting = false;
  xrVoiceReleased = false;
  xrVoiceChunks = [];
}

function xrVoiceFail(message) {
  xrVoiceReset();
  try { xrInstance?.voiceFailed(String(message || 'voice capture failed')); } catch (_) { /* fail-soft */ }
}

// Tap the page's server-message stream for `user_transcript` events
// (the daemon broadcast carrying server-side transcription results).
// Installed once, on first use; inert while not collecting.
function xrVoiceInstallTap() {
  if (xrVoiceTapInstalled) return;
  if (typeof dashboardServerMessageDispatcher !== 'function') return;
  const prev = dashboardServerMessageDispatcher;
  dashboardServerMessageDispatcher = (msg) => {
    try {
      if (xrVoiceCollecting) {
        const d = typeof msg === 'string' ? JSON.parse(msg) : msg;
        if (d && d.event === 'user_transcript' && typeof d.text === 'string' && d.text.trim()) {
          xrVoiceChunks.push(d.text.trim());
          if (xrVoiceReleased) xrVoiceArmSettle();
        }
      }
    } catch (_) { /* fail-soft: never break the page dispatcher */ }
    prev(msg);
  };
  xrVoiceTapInstalled = true;
}

// After release: a fresh chunk re-arms the settle window; silence for
// XR_VOICE_SETTLE_MS finalizes the utterance.
function xrVoiceArmSettle() {
  if (xrVoiceSettleTimer) clearTimeout(xrVoiceSettleTimer);
  xrVoiceSettleTimer = setTimeout(() => xrVoiceFinalize(), XR_VOICE_SETTLE_MS);
}

function xrVoiceFinalize() {
  const joined = xrVoiceChunks.join(' ').replace(/\s+/g, ' ').trim();
  xrVoiceReset();
  try {
    if (joined) xrInstance?.voiceResult(joined);
    else xrInstance?.voiceFailed(XR_VOICE_MSG.noSpeech);
  } catch (_) { /* fail-soft */ }
}

// Talk-press: open the capture lane. Availability is re-checked fresh
// on every press (config can change under a long session); the mic
// permission prompt happens HERE on first use, never at entry.
async function xrVoiceStart() {
  const avail = xrVoiceAvailability();
  if (!avail.available) { xrVoiceFail(avail.detail); return; }
  xrVoiceInstallTap();
  if (!xrVoiceTapInstalled) { xrVoiceFail('transcript stream unavailable in this build'); return; }
  xrVoiceGen += 1;
  const gen = xrVoiceGen;
  xrVoiceChunks = [];
  xrVoiceCollecting = true;
  xrVoiceReleased = false;

  // The flat dashboard mic already streams this page's audio into the
  // transcription lane while a live voice session is up — a second PCM
  // stream would interleave into the daemon's shared buffer. Tap the
  // flowing transcripts instead of double-capturing.
  if (typeof micActive !== 'undefined' && micActive
      && typeof modelConnected !== 'undefined' && modelConnected) {
    xrVoiceMode = 'tap';
    return;
  }

  xrVoiceMode = 'arming';
  try {
    if (!audioCtx) audioCtx = new AudioContext();
    if (audioCtx.state === 'suspended') await audioCtx.resume();
    if (!workletReady) {
      await audioCtx.audioWorklet.addModule('/audio-processor.js');
      workletReady = true;
    }
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: { channelCount: 1, echoCancellation: true },
    });
    if (gen !== xrVoiceGen || xrVoiceMode !== 'arming') {
      // Released or cancelled while the permission prompt was up: the
      // capture never ran. Drop the tracks immediately.
      try { stream.getTracks().forEach((t) => t.stop()); } catch (_) { /* gone */ }
      return;
    }
    xrVoiceStream = stream;
    xrVoiceSource = audioCtx.createMediaStreamSource(stream);
    xrVoiceNode = new AudioWorkletNode(audioCtx, 'audio-capture-processor', {
      processorOptions: { bufferSize: 4096 },
    });
    const nativeSR = audioCtx.sampleRate;
    xrVoiceNode.port.onmessage = (e) => {
      if (e.data.type !== 'audio') return;
      if (gen !== xrVoiceGen || xrVoiceMode !== 'mic' || !app) return;
      const resampled = downsample(e.data.data, nativeSR, XR_VOICE_RATE);
      const pcm16 = new Int16Array(resampled.length);
      for (let i = 0; i < resampled.length; i++) {
        pcm16[i] = Math.max(-32768, Math.min(32767, Math.floor(resampled[i] * 32768)));
      }
      app.send_user_audio(arrayBufferToBase64(pcm16.buffer));
    };
    xrVoiceSource.connect(xrVoiceNode);
    xrVoiceNode.connect(audioCtx.destination);
    xrVoiceMode = 'mic';
  } catch (err) {
    if (gen !== xrVoiceGen) return;
    const name = (err && err.name) || '';
    const msg = (err && err.message) || String(err);
    xrVoiceFail(name === 'NotAllowedError' || name === 'SecurityError'
      ? `mic denied: ${msg}`
      : `mic capture failed: ${msg}`);
  }
}

// Talk-release: stop the mic NOW (tracks off, indicator out), flush the
// daemon's chunk buffer with one window of silence so the spoken tail
// transcribes, then wait for the transcripts to settle.
function xrVoiceStop() {
  if (!xrVoiceMode) return;
  if (xrVoiceMode === 'arming') {
    // Permission prompt outlived the hold — nothing was captured.
    xrVoiceGen += 1;
    xrVoiceFail(XR_VOICE_MSG.micPending);
    return;
  }
  xrVoiceReleased = true;
  if (xrVoiceMode === 'mic') {
    xrVoiceTeardownAudio();
    // The daemon drains its transcription buffer in fixed ~3 s windows
    // with no flush verb: pad one full window of PCM16 silence so the
    // tail chunk (real speech + zeros) drains now. Pure-silence windows
    // are RMS-gated daemon-side, so the pad itself transcribes nothing.
    try {
      if (app) {
        const chunk = new ArrayBuffer(XR_VOICE_PAD_BYTES / 3);
        const b64 = arrayBufferToBase64(chunk);
        for (let i = 0; i < 3; i++) app.send_user_audio(b64);
      }
    } catch (err) {
      console.warn('[xr] voice pad flush failed', err);
    }
  }
  // 'tap' mode: the flat mic keeps streaming on its own cadence —
  // nothing to stop, nothing to pad; just wait for the window's chunks.
  if (xrVoiceChunks.length > 0) xrVoiceArmSettle();
  if (xrVoiceTimeoutTimer) clearTimeout(xrVoiceTimeoutTimer);
  xrVoiceTimeoutTimer = setTimeout(() => {
    if (xrVoiceChunks.length > 0) xrVoiceFinalize();
    else xrVoiceFail(XR_VOICE_MSG.noSpeech);
  }, XR_VOICE_TIMEOUT_MS);
}

// Quick-release cancel: the wasm already rendered the teaching hint;
// tear everything down silently (no result, no failure line).
function xrVoiceCancel() {
  xrVoiceGen += 1;
  xrVoiceReset();
}

// The strip's "use" landed: route the reviewed transcript through the
// dashboard's EXISTING send path — focus the target session (the same
// primitive clicking its window uses; it retargets the composer) and
// submit through the shared composer core (steer / follow-up / task
// phase logic included). field_id `composer:<sessionId>` is the
// reconcile contract with the ray-keyboard seat's TextEntry facade:
// when that lands, this routing collapses into the field buffer's own
// commit path and the shape must keep matching.
function xrVoiceCommitText(fieldId, text) {
  const t = String(text || '').trim();
  if (!t) return;
  const prefix = 'composer:';
  const sid = String(fieldId || '').startsWith(prefix)
    ? String(fieldId).slice(prefix.length).trim()
    : '';
  let dispatched = false;
  try {
    if (sid && typeof focusSessionWindow === 'function') focusSessionWindow(sid);
    if (typeof submitComposedText === 'function') dispatched = submitComposedText(t) === true;
  } catch (err) {
    console.warn('[xr] voice text dispatch failed', err);
  }
  if (!dispatched) {
    // Say so in-scene — the operator is in a headset, not at the toast.
    try { xrInstance?.voiceFailed('send failed — try again from the dashboard'); } catch (_) { /* fail-soft */ }
  }
}

// QA facade extension (same conventions as the terminal seam): direct
// pushes through the voice facade for deterministic probes — the probe
// injects fake transcripts here and never depends on a real mic or ASR.
globalThis.xrProbe.voice = {
  status: (s) => { if (xrInstance) xrInstance.voiceStatus(s); },
  result: (text) => { if (xrInstance) xrInstance.voiceResult(String(text)); },
  failed: (msg) => { if (xrInstance) xrInstance.voiceFailed(String(msg)); },
  availability: () => xrVoiceAvailability(),
  capture: () => ({
    mode: xrVoiceMode,
    collecting: xrVoiceCollecting,
    released: xrVoiceReleased,
    chunks: xrVoiceChunks.length,
  }),
};
