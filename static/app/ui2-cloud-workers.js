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
  if (!cloudWorkersRows.length) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = cloudWorkersError
      ? 'Cloud workers unavailable'
      : 'No Codex Cloud tasks tracked yet — submit one with `intendant codex-cloud exec`, then Sync.';
    list.appendChild(empty);
    return;
  }
  for (const lease of cloudWorkersRows) {
    list.appendChild(cloudWorkerRow(lease));
  }
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
    workerEl.textContent = `worker ${lease.worker.hostname || '?'}${boot}`;
    workerEl.title = 'Runtime fingerprint from a probe or pulled diff; identity change across turns = cold replacement';
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

window.loadCloudWorkers = loadCloudWorkers;
window.cloudWorkersOnShown = cloudWorkersOnShown;
