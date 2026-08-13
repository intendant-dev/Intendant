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
    // Same dispatch seam as the other rendered surface: actions carry the
    // dashboard's action vocabulary and route into the one control plane.
    // The router hookup lands with the input commits; log until then so
    // dev-gate testing shows the emissions.
    try {
      console.log('[xr] action', action);
    } catch { /* console absent: drop */ }
  });
  xrInstance.setOnSessionEnd(() => {
    xrStopSnapshotPump();
    xrSetChipState('idle', 'Enter XR');
  });
  await xrInstance.probeSupport();
  return xrInstance;
}

function xrSetChipState(state, title) {
  if (!xrChipEl) return;
  xrChipEl.dataset.state = state;
  if (title) xrChipEl.title = title;
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
  }, 300);
}

function xrStopSnapshotPump() {
  if (xrSnapshotTimer) {
    clearInterval(xrSnapshotTimer);
    xrSnapshotTimer = null;
  }
}

async function xrEnter() {
  xrSetChipState('busy', 'Starting immersive session…');
  try {
    const inst = await ensureXr();
    // Passthrough-first: prefer AR on hardware that has it (Quest 3),
    // fall back to VR (Vision Pro Safari has no immersive-ar).
    const mode = xrSupport.ar ? 'immersive-ar' : 'immersive-vr';
    await inst.enter(mode);
    xrStartSnapshotPump();
    xrSetChipState('active', 'Immersive session running');
  } catch (err) {
    xrSetChipState('error', 'XR entry failed: ' + (err && err.message ? err.message : err));
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
};
