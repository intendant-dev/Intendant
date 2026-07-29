// ── Codex Cloud workers (Sessions → Cloud) ─────────────────────────────
//
// Display-only card over GET /api/codex-cloud/workers (tunnel twin
// api_codex_cloud_workers): the daemon's lease store for provider-owned
// Codex Cloud containers. The default paint is a cached read; "Sync with
// provider" re-syncs the store through the daemon host's authenticated
// Codex CLI (and the daemon parks agenda notes for terminal transitions
// it observes). Ephemeral worker leases, not peers — provider task state
// and live-attachment state render as independent chips.
//
// Deep-link TDZ rule: evaluates BEFORE the router (48) because a
// #sessions/cloud deep link makes the router's boot call
// cloudWorkersOnShown(), which reads this fragment's module-level lets.
// Top level declares only lets/consts/functions.

let cloudWorkersRows = [];
let cloudWorkersError = '';
let cloudWorkersRefreshError = '';
let cloudWorkersLoaded = false;
let cloudWorkersFetchInFlight = null;

function cloudWorkersOnShown() {
  if (cloudWorkersLoaded) renderCloudWorkers();
  else loadCloudWorkers(false);
}

async function loadCloudWorkers(refresh) {
  if (cloudWorkersFetchInFlight) return cloudWorkersFetchInFlight;
  const btn = document.getElementById('cloud-workers-refresh');
  if (btn) { btn.disabled = true; btn.textContent = refresh ? 'Syncing…' : 'Loading…'; }
  cloudWorkersFetchInFlight = (async () => {
    try {
      const resp = await daemonApi.request('api_codex_cloud_workers', refresh ? { refresh: true } : {});
      if (resp.ok && resp.body && Array.isArray(resp.body.workers)) {
        cloudWorkersRows = resp.body.workers;
        cloudWorkersRefreshError = resp.body.refresh_error || '';
        cloudWorkersError = '';
        cloudWorkersLoaded = true;
      } else {
        cloudWorkersError = (resp.body && resp.body.error) || `cloud workers unavailable (${resp.status})`;
      }
    } catch (e) {
      cloudWorkersError = String((e && e.message) || e);
    } finally {
      cloudWorkersFetchInFlight = null;
      if (btn) { btn.disabled = false; btn.textContent = 'Sync with provider'; }
    }
    renderCloudWorkers();
  })();
  return cloudWorkersFetchInFlight;
}

// The daemon's snake_case vocabularies, rendered as short chips. Unknown
// values pass through verbatim — the daemon is the source of truth.
const CLOUD_WORKER_ATTACHMENT_LABELS = {
  not_requested: 'no attachment',
  awaiting: 'awaiting',
  connected: 'connected',
  disconnected: 'disconnected',
  expired: 'expired',
};

function cloudWorkersAgo(unixMs) {
  const ms = Number(unixMs) || 0;
  if (!ms) return '';
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function renderCloudWorkers() {
  const status = document.getElementById('cloud-workers-status');
  if (status) {
    status.textContent = cloudWorkersError
      ? `Error: ${cloudWorkersError}`
      : cloudWorkersRefreshError
        ? `Provider sync failed (showing cached leases): ${cloudWorkersRefreshError}`
        : '';
    status.classList.toggle('cloud-workers-status-error',
      Boolean(cloudWorkersError || cloudWorkersRefreshError));
  }
  const list = document.getElementById('cloud-workers-list');
  if (!list) return;
  list.textContent = '';
  cloudSubmitSyncOptions();
  if (!cloudWorkersRows.length) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = cloudWorkersError
      ? 'Cloud workers unavailable'
      : 'No Codex Cloud tasks tracked yet — submit one above, or Sync with provider to pull the task list.';
    list.appendChild(empty);
    return;
  }
  for (const lease of cloudWorkersRows) {
    list.appendChild(cloudWorkerRow(lease));
  }
}

// ── Submit affordance ──────────────────────────────────────────────────
//
// The dashboard's own submit lane: POST /api/codex-cloud/submit (tunnel
// twin api_codex_cloud_submit) rides the same daemon-side submission path
// as `intendant codex-cloud exec` and the submit_codex_cloud_task MCP
// tool — one path, three frontends. A successful submit records the lease
// in the store before the response returns, so the immediate re-sync
// lists the task even when the provider window lags.

let cloudSubmitInFlight = false;

const CLOUD_SUBMIT_ENV_STORE = 'intendant.ui2.cloudSubmitEnv';

function cloudSubmitStatus(text, isError) {
  const el = document.getElementById('cloud-submit-status');
  if (!el) return;
  el.textContent = text || '';
  el.classList.toggle('cloud-submit-status-error', Boolean(isError));
}

// Environment suggestions derive from the daemon's tracked leases — there
// is no provider env-list verb, so environments the daemon has seen (plus
// the last one submitted from this browser) are the honest vocabulary.
function cloudSubmitSyncOptions() {
  const datalist = document.getElementById('cloud-submit-env-options');
  if (datalist) {
    const seen = new Map();
    for (const lease of cloudWorkersRows) {
      const id = String(lease.environment_id || '').trim();
      if (!id || seen.has(id)) continue;
      seen.set(id, String(lease.environment_label || '').trim());
    }
    datalist.replaceChildren();
    for (const [id, label] of seen) {
      const option = document.createElement('option');
      option.value = id;
      if (label) option.label = label;
      datalist.appendChild(option);
    }
  }
  const env = document.getElementById('cloud-submit-env');
  if (env && !env.value) {
    let remembered = '';
    try { remembered = localStorage.getItem(CLOUD_SUBMIT_ENV_STORE) || ''; } catch (e) { /* private mode */ }
    const fromRows = cloudWorkersRows
      .map((lease) => String(lease.environment_id || '').trim())
      .find(Boolean) || '';
    env.value = remembered || fromRows;
  }
}

async function submitCloudTask(ev) {
  if (ev && ev.preventDefault) ev.preventDefault();
  if (cloudSubmitInFlight) return false;
  const promptEl = document.getElementById('cloud-submit-prompt');
  const envEl = document.getElementById('cloud-submit-env');
  const prompt = ((promptEl && promptEl.value) || '').trim();
  const environment = ((envEl && envEl.value) || '').trim();
  if (!prompt) {
    cloudSubmitStatus('A task prompt is required.', true);
    if (promptEl) promptEl.focus();
    return false;
  }
  if (!environment) {
    cloudSubmitStatus('A Codex Cloud environment id is required.', true);
    if (envEl) envEl.focus();
    return false;
  }
  const params = { environment_id: environment, prompt };
  const branchEl = document.getElementById('cloud-submit-branch');
  const branch = ((branchEl && branchEl.value) || '').trim();
  if (branch) params.branch = branch;
  const attemptsEl = document.getElementById('cloud-submit-attempts');
  const attempts = parseInt((attemptsEl && attemptsEl.value) || '', 10);
  if (Number.isFinite(attempts) && attempts >= 1) params.attempts = attempts;
  const titleEl = document.getElementById('cloud-submit-title');
  const title = ((titleEl && titleEl.value) || '').trim();
  if (title) params.title = title;

  cloudSubmitInFlight = true;
  const go = document.getElementById('cloud-submit-go');
  if (go) { go.disabled = true; go.textContent = 'Submitting…'; }
  cloudSubmitStatus('Submitting to Codex Cloud…');
  try {
    const resp = await daemonApi.request('api_codex_cloud_submit', params);
    if (resp.ok && resp.body && resp.body.task_id) {
      try { localStorage.setItem(CLOUD_SUBMIT_ENV_STORE, environment); } catch (e) { /* private mode */ }
      if (promptEl) promptEl.value = '';
      if (titleEl) titleEl.value = '';
      cloudSubmitStatus(`Task ${resp.body.task_id} submitted — tracked below; syncing with the provider…`);
      await loadCloudWorkers(true);
      cloudSubmitStatus(`Task ${resp.body.task_id} submitted.`);
    } else if (resp.ok) {
      // The submission went through but the Codex CLI output carried no
      // task id — the provider sync is how the lease store finds it.
      cloudSubmitStatus('Submitted, but the Codex CLI did not report a task id — syncing to find it…');
      await loadCloudWorkers(true);
    } else {
      cloudSubmitStatus(
        (resp.body && resp.body.error) || `Submit failed (${resp.status}).`,
        true,
      );
    }
  } catch (e) {
    cloudSubmitStatus(String((e && e.message) || e), true);
  } finally {
    cloudSubmitInFlight = false;
    if (go) { go.disabled = false; go.textContent = 'Submit task'; }
  }
  return false;
}

function cloudWorkerRow(lease) {
  const row = document.createElement('div');
  row.className = 'cloud-worker-row ui-card';

  const head = document.createElement('div');
  head.className = 'cloud-worker-head';
  const title = document.createElement('span');
  title.className = 'cloud-worker-title';
  title.textContent = lease.title || 'untitled task';
  head.appendChild(title);
  const provider = document.createElement('span');
  provider.className = `cloud-worker-chip cloud-worker-provider is-${lease.provider_state || 'unknown'}`;
  provider.textContent = lease.provider_status || 'unknown';
  provider.title = 'Provider task state (from the Codex CLI)';
  head.appendChild(provider);
  const attachment = document.createElement('span');
  const attachState = lease.attachment_state || 'not_requested';
  attachment.className = `cloud-worker-chip cloud-worker-attachment is-${attachState}`;
  attachment.textContent = CLOUD_WORKER_ATTACHMENT_LABELS[attachState] || attachState;
  attachment.title = 'Live-attachment state (independent of provider state)';
  head.appendChild(attachment);
  // Daemon-derived warm-worker heuristic: an active turn holds its worker;
  // a warm worker keeps ignored build artifacts (measured 68x faster
  // identical rebuild), so follow-ups in this task reuse them.
  const warmth = lease.warmth || 'unknown';
  const warmChip = document.createElement('span');
  warmChip.className = `cloud-worker-chip cloud-worker-warmth is-${warmth}`;
  warmChip.textContent = warmth === 'warm' ? 'likely warm' : warmth === 'cold' ? 'cold likely' : 'warmth unknown';
  warmChip.title = 'Warm-worker heuristic: follow-ups in a warm task reuse its incremental build state';
  head.appendChild(warmChip);
  row.appendChild(head);

  const meta = document.createElement('div');
  meta.className = 'cloud-worker-meta';
  const id = document.createElement('span');
  id.className = 'cloud-worker-id';
  id.textContent = lease.task_id || '';
  meta.appendChild(id);
  const env = lease.environment_label || lease.environment_id || '';
  if (env) {
    const envEl = document.createElement('span');
    envEl.className = 'cloud-worker-env';
    envEl.textContent = env;
    meta.appendChild(envEl);
  }
  const ago = cloudWorkersAgo(lease.last_observed_unix_ms);
  if (ago) {
    const agoEl = document.createElement('span');
    agoEl.className = 'cloud-worker-ago';
    agoEl.textContent = `observed ${ago}`;
    meta.appendChild(agoEl);
  }
  if (Number(lease.turns_observed) > 1) {
    const turnsEl = document.createElement('span');
    turnsEl.className = 'cloud-worker-ago';
    turnsEl.textContent = `${lease.turns_observed} turns`;
    turnsEl.title = 'Completed turns observed (follow-ups reuse the warm worker)';
    meta.appendChild(turnsEl);
  }
  if (lease.worker && (lease.worker.hostname || lease.worker.boot_id)) {
    const workerEl = document.createElement('span');
    workerEl.className = 'cloud-worker-id';
    const boot = lease.worker.boot_id ? ` · boot ${String(lease.worker.boot_id).slice(0, 8)}` : '';
    const swaps = Number(lease.cold_replacements_observed) > 0
      ? ` · replaced ×${lease.cold_replacements_observed}`
      : '';
    workerEl.textContent = `worker ${lease.worker.hostname || '?'}${boot}${swaps}`;
    workerEl.title = 'Runtime fingerprint from a probe or pulled diff; re-probe with `codex-cloud probe --task` — a boot-identity mismatch is a detected cold replacement';
    meta.appendChild(workerEl);
  }
  if (lease.task_url) {
    const link = document.createElement('a');
    link.className = 'cloud-worker-link';
    link.href = lease.task_url;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.textContent = 'open in Codex ↗';
    meta.appendChild(link);
  }
  row.appendChild(meta);

  if (lease.attachment_state === 'connected' && lease.task_id) {
    const view = document.createElement('button');
    view.type = 'button';
    view.className = 'ui-btn ui-btn-sm cloud-worker-view';
    view.textContent = cloudDisplayTask === lease.task_id ? 'Viewing' : 'View';
    view.title = 'Watch the worker’s virtual display live (tiles bridged over the attachment)';
    view.addEventListener('click', () => openCloudWorkerDisplay(lease.task_id));
    head.appendChild(view);
    const term = document.createElement('button');
    term.type = 'button';
    term.className = 'ui-btn ui-btn-sm cloud-worker-terminal';
    term.textContent = 'Terminal';
    term.title = 'Open a shell inside the live worker (bridged over the attachment)';
    term.addEventListener('click', () => openCloudWorkerTerminal(lease.task_id));
    head.appendChild(term);
  }

  const isTerminal = ['finished', 'failed', 'cancelled'].includes(lease.provider_state);
  if (isTerminal && lease.task_id) {
    const pull = document.createElement('div');
    pull.className = 'cloud-worker-pull';
    pull.textContent = `intendant codex-cloud pull ${lease.task_id}`;
    pull.title = 'Bring this task’s diff home as a fresh branch in a new worktree';
    row.appendChild(pull);
    const followup = document.createElement('div');
    followup.className = 'cloud-worker-pull';
    followup.textContent = `intendant codex-cloud followup ${lease.task_id}`;
    followup.title = 'Send a follow-up turn into the same task — a warm worker reuses its incremental build state';
    row.appendChild(followup);
  }
  return row;
}

// Connected leases double as shell hosts: the picker lists them beside
// peers, and frames for `cloud:<task_id>` ride the LOCAL tunnel (the
// bridge lives on this daemon).
function cloudConnectedShellHosts() {
  return cloudWorkersRows
    .filter(lease => lease.attachment_state === 'connected' && lease.task_id)
    .map(lease => ({
      id: `cloud:${lease.task_id}`,
      label: `Cloud: ${lease.title || lease.task_id}`,
    }));
}

// ── Live worker display viewer (attach slice 3a) ───────────────────────
//
// One viewer at a time, rendered in the #cloud-worker-display panel ABOVE
// the card list (the list re-renders on refresh; the panel survives it).
// Frames ride the local dashboard tunnel: display_open subscribes, the
// worker's tile stream arrives as display_tiles (base64 tile wire
// frames), and the shipped transport-agnostic TileCompositor paints them.
// Input needs the same display.input floor a local display does — the
// daemon gates per-frame; the viewer just sends.

let cloudDisplayTask = null;
let cloudDisplayId = 0;
let cloudDisplayCompositor = null;
let cloudDisplayListener = null;
let cloudDisplayOpenTimer = null;
let cloudDisplayStatusEl = null;

function cloudDisplaySendFrame(frame) {
  // terminalFrame is the tunnel's generic frame sender (canUseRpc +
  // sendFrame); cloud terminal frames already ride it.
  return typeof dashboardTransport !== 'undefined'
    && dashboardTransport.terminalFrame
    && dashboardTransport.terminalFrame(frame);
}

function cloudDisplayStatus(text, kind = '') {
  if (!cloudDisplayStatusEl) return;
  cloudDisplayStatusEl.textContent = text;
  cloudDisplayStatusEl.className = `cloud-worker-display-status${kind ? ` ${kind}` : ''}`;
}

function closeCloudWorkerDisplay(sendClose = true) {
  if (cloudDisplayOpenTimer) {
    clearInterval(cloudDisplayOpenTimer);
    cloudDisplayOpenTimer = null;
  }
  if (sendClose && cloudDisplayTask) {
    cloudDisplaySendFrame({ t: 'display_close', host_id: `cloud:${cloudDisplayTask}` });
  }
  if (cloudDisplayListener) {
    window.removeEventListener('intendant-cloud-display-frame', cloudDisplayListener);
    cloudDisplayListener = null;
  }
  cloudDisplayCompositor = null;
  cloudDisplayStatusEl = null;
  cloudDisplayTask = null;
  cloudDisplayId = 0;
  const panel = document.getElementById('cloud-worker-display');
  if (panel) {
    panel.hidden = true;
    panel.replaceChildren();
  }
  renderCloudWorkers();
}

function cloudDisplayNormalizedCoords(canvas, ev) {
  const rect = canvas.getBoundingClientRect();
  if (!rect.width || !rect.height) return { x: 0, y: 0 };
  const x = Math.min(1, Math.max(0, (ev.clientX - rect.left) / rect.width));
  const y = Math.min(1, Math.max(0, (ev.clientY - rect.top) / rect.height));
  return { x, y };
}

function cloudDisplaySendInput(event) {
  if (!cloudDisplayTask) return;
  cloudDisplaySendFrame({
    t: 'display_input',
    host_id: `cloud:${cloudDisplayTask}`,
    display_id: cloudDisplayId,
    event,
  });
}

function cloudDisplayWireInput(stage) {
  stage.tabIndex = 0;
  const mods = (ev) => ({ shift: ev.shiftKey, ctrl: ev.ctrlKey, alt: ev.altKey, meta: ev.metaKey });
  const canvasOf = () => cloudDisplayCompositor && cloudDisplayCompositor.canvas;
  stage.addEventListener('mousedown', (ev) => {
    const canvas = canvasOf();
    if (!canvas) return;
    stage.focus();
    ev.preventDefault();
    const { x, y } = cloudDisplayNormalizedCoords(canvas, ev);
    cloudDisplaySendInput({ t: 'md', x, y, b: ev.button });
  });
  stage.addEventListener('mouseup', (ev) => {
    const canvas = canvasOf();
    if (!canvas) return;
    ev.preventDefault();
    const { x, y } = cloudDisplayNormalizedCoords(canvas, ev);
    cloudDisplaySendInput({ t: 'mu', x, y, b: ev.button });
  });
  stage.addEventListener('mousemove', (ev) => {
    const canvas = canvasOf();
    if (!canvas) return;
    const { x, y } = cloudDisplayNormalizedCoords(canvas, ev);
    cloudDisplaySendInput({ t: 'mm', x, y, buttons: ev.buttons });
  });
  stage.addEventListener('wheel', (ev) => {
    const canvas = canvasOf();
    if (!canvas) return;
    ev.preventDefault();
    const { x, y } = cloudDisplayNormalizedCoords(canvas, ev);
    cloudDisplaySendInput({ t: 'sc', x, y, dx: ev.deltaX, dy: ev.deltaY });
  }, { passive: false });
  stage.addEventListener('keydown', (ev) => {
    ev.preventDefault();
    cloudDisplaySendInput({ t: 'kd', code: ev.code, key: ev.key, ...mods(ev) });
  });
  stage.addEventListener('keyup', (ev) => {
    ev.preventDefault();
    cloudDisplaySendInput({ t: 'ku', code: ev.code, key: ev.key, ...mods(ev) });
  });
}

function openCloudWorkerDisplay(taskId) {
  if (cloudDisplayTask === taskId) {
    closeCloudWorkerDisplay();
    return;
  }
  closeCloudWorkerDisplay();
  const panel = document.getElementById('cloud-worker-display');
  if (!panel) return;
  if (typeof maybeStartDashboardControlTransport === 'function') {
    maybeStartDashboardControlTransport({ onDemand: true });
  }
  cloudDisplayTask = taskId;
  const host = `cloud:${taskId}`;
  panel.hidden = false;

  const head = document.createElement('div');
  head.className = 'cloud-worker-display-head';
  const title = document.createElement('span');
  title.className = 'cloud-worker-display-title';
  title.textContent = `Worker display · ${taskId}`;
  head.appendChild(title);
  cloudDisplayStatusEl = document.createElement('span');
  head.appendChild(cloudDisplayStatusEl);
  const close = document.createElement('button');
  close.type = 'button';
  close.className = 'ui-btn ui-btn-sm';
  close.textContent = 'Close';
  close.addEventListener('click', () => closeCloudWorkerDisplay());
  head.appendChild(close);
  panel.appendChild(head);

  const stage = document.createElement('div');
  stage.className = 'cloud-worker-display-stage';
  panel.appendChild(stage);
  cloudDisplayWireInput(stage);

  cloudDisplayListener = (ev) => {
    const msg = ev.detail || {};
    if (msg.host_id !== host) return;
    if (msg.t === 'display_opened') {
      cloudDisplayId = Number(msg.display_id) || 0;
      cloudDisplayStatus(`connected · display :${cloudDisplayId}`, 'ok');
      return;
    }
    if (msg.t === 'display_error') {
      cloudDisplayStatus(String(msg.error || 'display error'), 'error');
      return;
    }
    if (msg.t === 'display_closed') {
      cloudDisplayStatus('display closed', '');
      return;
    }
    if (msg.t === 'display_tiles' && typeof msg.data === 'string') {
      let bytes;
      try {
        bytes = Uint8Array.from(atob(msg.data), (c) => c.charCodeAt(0));
      } catch (_) {
        return;
      }
      try {
        if (!cloudDisplayCompositor) {
          // The stream opens with a Resize frame; construct the
          // compositor from it (placeholder dims — onResize below
          // reconfigures everything) and feed it the same frame.
          const parsed = parseTileWireFrame(bytes);
          if (parsed.type !== 'resize') return;
          cloudDisplayCompositor = new TileCompositor(stage, {
            tileSize: parsed.tile_size_px,
            gridW: parsed.grid_w_tiles,
            gridH: parsed.grid_h_tiles,
          });
        }
        cloudDisplayCompositor.onWireFrame(bytes);
      } catch (err) {
        cloudDisplayStatus(`tile decode failed: ${(err && err.message) || err}`, 'error');
      }
    }
  };
  window.addEventListener('intendant-cloud-display-frame', cloudDisplayListener);

  // The open frame needs the tunnel; retry until the on-demand start
  // connects (terminalFrame returns false while it cannot send).
  const tryOpen = () => {
    if (cloudDisplaySendFrame({ t: 'display_open', host_id: host })) {
      if (cloudDisplayOpenTimer) {
        clearInterval(cloudDisplayOpenTimer);
        cloudDisplayOpenTimer = null;
      }
      cloudDisplayStatus('opening worker display…', '');
      return true;
    }
    cloudDisplayStatus('connecting the dashboard tunnel…', '');
    return false;
  };
  if (!tryOpen()) {
    cloudDisplayOpenTimer = setInterval(() => {
      if (!cloudDisplayTask) return;
      tryOpen();
    }, 1000);
  }
  renderCloudWorkers();
}

function openCloudWorkerTerminal(taskId) {
  // Cloud terminal frames ride only the dashboard-control tunnel; start it
  // on demand so the affordance works even when the legacy /ws is the
  // event lane (the default browser posture) and the tunnel is idle.
  if (typeof maybeStartDashboardControlTransport === 'function') {
    maybeStartDashboardControlTransport({ onDemand: true });
  }
  if (typeof refreshShellHostOptions === 'function') refreshShellHostOptions();
  if (typeof setShellHost === 'function') setShellHost(`cloud:${taskId}`);
  if (typeof switchTab === 'function') switchTab('terminal');
}

window.cloudConnectedShellHosts = cloudConnectedShellHosts;
window.loadCloudWorkers = loadCloudWorkers;
window.cloudWorkersOnShown = cloudWorkersOnShown;
window.openCloudWorkerDisplay = openCloudWorkerDisplay;
window.closeCloudWorkerDisplay = closeCloudWorkerDisplay;
window.submitCloudTask = submitCloudTask;
