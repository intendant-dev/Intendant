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

let xrWasmModule = null; // module namespace after first ensureXr()
let xrInstance = null;   // XrWeb handle after first ensureXr()
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
  if (xrInstance) return xrInstance;
  if (!xrWasmModule) {
    xrWasmModule = await import('/wasm-xr/xr_web.js');
    await xrWasmModule.default({ module_or_path: '/wasm-xr/xr_web_bg.wasm' });
  }
  xrInstance = new xrWasmModule.XrWeb();
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

// Agenda-rail probe hook (attached beside the facade so the object
// literal above stays untouched): the pump's agenda block, on demand.
globalThis.xrProbe.agenda = () => xrAgendaSummary();
