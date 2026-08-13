// ui2-xr.js — XR spatial surface: header entry chip + lazy WASM boot.
//
// The XR surface (docs/src/xr.md) is the immersive presentation of the
// regular dashboard: it consumes the same coalesced client state and
// dispatches through the same handlers as every other tab — never a
// second brain. This fragment owns only the browser-side seam: feature
// detection, the entry chip, lazy module load, and session entry.
//
// DEV GATE: until milestone 1 completes, the chip appears only with
// `?xr=dev` in the URL (same convention as `?station_panes=on`) — the
// scaffold's enter() intentionally rejects. Everything below is
// boot-callback or event-driven; top level declares only lets/consts/
// functions (deep-link TDZ rule).

let xrWasmModule = null; // module namespace after first ensureXr()
let xrInstance = null;   // XrWeb handle after first ensureXr()
let xrChipEl = null;
let xrSupport = { ar: false, vr: false };
let xrSnapshotTimer = null;
let xrCaptureOnly = false; // probe mode: record actions, don't route

function xrDevGateOn() {
  try {
    return new URLSearchParams(location.search).get('xr') === 'dev';
  } catch {
    return false;
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
    xrShowStatus('', '');
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
    // next attempt instead of a timer.
    xrShowStatus('error', 'XR entry failed: ' + detail);
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
  if (!xrDevGateOn()) return;
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
  devGate: xrDevGateOn,
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
