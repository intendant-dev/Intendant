// ── Access model + rendering ──
// The Access surface model + render domain: fleet targets (remembered +
// live, hosted sync, petnames), enrollments, Connect status/claim, the
// IAM overview model (roles/policies/permissions, fallback catalog),
// target rows/feature chips/lane badges, grant forms + fanout, the
// permission matrix, and the coordinator route preview. The Access pane
// scaffolding lives in 43-access-panes.js.

function normalizeDashboardAccessTarget(target) {
  if (!target || typeof target !== 'object') return null;
  const id = String(target.host_id || target.hostId || target.id || '').trim();
  if (!id) return null;
  const local = target.local === true || id === selfPeerId || id === SHELL_HOST_ID;
  const route = String(target.route || target.route_key || '').trim();
  const accessDomain = String(target.access_domain || target.accessDomain || '').trim();
  const capabilities = Array.isArray(target.capabilities) ? target.capabilities : [];
  return {
    ...target,
    id: String(target.id || id).trim() || id,
    host_id: id,
    local,
    label: String(target.label || '').trim() || (local ? (selfHostLabel || 'This daemon') : id),
    access_domain: accessDomain,
    access_domain_label: String(target.access_domain_label || target.accessDomainLabel || '').trim(),
    route,
    route_label: String(target.route_label || target.routeLabel || '').trim(),
    auth: String(target.auth || '').trim(),
    auth_label: String(target.auth_label || target.authLabel || '').trim(),
    effective_role: String(target.effective_role || target.effectiveRole || '').trim(),
    effective_role_label: String(target.effective_role_label || target.effectiveRoleLabel || '').trim(),
    profile: String(target.profile || '').trim(),
    // Owner-set trust tier, stamped by the daemon into its own targets
    // payload and carried in the signed v4 fleet record (never on the
    // public agent card — docs/src/trust-tiers.md § metadata carriers).
    tier: String(target.tier || '').trim(),
    // Owner-chosen petname (signed v5 record line): the name the OWNER
    // gave this identity — a lookalike daemon never inherits it.
    petname: String(target.petname || '').trim(),
    connected: target.connected !== false,
    capabilities,
  };
}

function accessFleetRead() {
  try {
    const parsed = JSON.parse(localStorage.getItem(ACCESS_FLEET_KEY) || '{}');
    if (!parsed || typeof parsed !== 'object') return { schema_version: 1, targets: [] };
    const targets = Array.isArray(parsed.targets) ? parsed.targets : [];
    return { schema_version: 1, targets };
  } catch (_) {
    return { schema_version: 1, targets: [] };
  }
}

function accessFleetWrite(targets, options = {}) {
  const normalized = [];
  const seen = new Set();
  for (const target of targets || []) {
    const item = normalizeDashboardAccessTarget(target);
    if (!item) continue;
    const key = accessFleetCanonicalTargetId({ ...target, ...item });
    if (!key || seen.has(key)) continue;
    seen.add(key);
    const connectDaemonId = String(target?.connect_daemon_id || target?.connectDaemonId || '').trim() ||
      (item.local && DASHBOARD_CONNECT_MODE ? DASHBOARD_CONNECT_DAEMON_ID : '');
    normalized.push({
      ...item,
      id: key,
      host_id: key,
      connect_daemon_id: connectDaemonId,
    });
  }
  normalized.sort((a, b) => {
    if (a.local !== b.local) return a.local ? -1 : 1;
    return String(a.label || a.host_id).localeCompare(String(b.label || b.host_id));
  });
  localStorage.setItem(ACCESS_FLEET_KEY, JSON.stringify({
    schema_version: 1,
    updated_unix_ms: Date.now(),
    targets: normalized.slice(0, 100),
  }));
  if (!options.skipHostedSync) accessFleetScheduleHostedSync();
}

function accessFleetHostedSyncEnabled() {
  return DASHBOARD_CONNECT_MODE;
}

function accessFleetCanonicalTargetId(target) {
  const connectDaemonId = String(
    target?.connect_daemon_id ||
    target?.connectDaemonId ||
    ''
  ).trim();
  if (connectDaemonId) return connectDaemonId;
  if (target?.local && DASHBOARD_CONNECT_MODE && DASHBOARD_CONNECT_DAEMON_ID) {
    return DASHBOARD_CONNECT_DAEMON_ID;
  }
  return String(target?.host_id || target?.hostId || target?.id || '').trim();
}

function accessFleetHostedUrl(path) {
  const normalized = String(path || '');
  if (!DASHBOARD_CONNECT_SIGNALING_BASE) return normalized;
  return `${DASHBOARD_CONNECT_SIGNALING_BASE}${normalized.startsWith('/') ? '' : '/'}${normalized}`;
}

async function accessFleetHostedHeaders(options = {}) {
  if (options.refresh) accessFleetHostedCsrfToken = '';
  if (!accessFleetHostedCsrfToken) {
    const resp = await fetch(accessFleetHostedUrl('/api/me'));
    const body = await resp.json().catch(() => ({}));
    if (!resp.ok || body.authenticated !== true) return null;
    accessFleetHostedCsrfToken = String(body.csrf_token || '');
  }
  const headers = { 'content-type': 'application/json' };
  if (accessFleetHostedCsrfToken) headers['x-intendant-csrf'] = accessFleetHostedCsrfToken;
  return headers;
}

/* Hosted-sync health, surfaced on the fleet strip: true after a hosted
   fleet write kept failing even through the CSRF-refresh retry below.
   Cleared by the next successful write. */
let accessFleetHostedSyncFailing = false;

function accessFleetHostedSetSyncFailing(failing) {
  const value = Boolean(failing);
  if (accessFleetHostedSyncFailing === value) return;
  accessFleetHostedSyncFailing = value;
  renderAccessFleetStrip();
}

/* POST to the hosted fleet store with one CSRF-expiry retry: hosted
   sessions rotate the CSRF token with the login session, and the old
   cached-forever copy turned every rotation into permanent silent
   failure. A 401/403 drops the cached token, re-fetches it once, and
   replays the call once. Returns the final Response, or null when the
   hosted session is signed out (not a sync failure). */
async function accessFleetHostedPost(path, body) {
  let headers = await accessFleetHostedHeaders();
  if (!headers) return null;
  let resp = await fetch(accessFleetHostedUrl(path), { method: 'POST', headers, body });
  if (resp.status === 401 || resp.status === 403) {
    headers = await accessFleetHostedHeaders({ refresh: true });
    if (!headers) return null;
    resp = await fetch(accessFleetHostedUrl(path), { method: 'POST', headers, body });
  }
  return resp;
}

function accessFleetMergeHostedTargets(targets) {
  if (!Array.isArray(targets) || targets.length === 0) return;
  const byId = new Map();
  for (const target of accessFleetRead().targets || []) {
    const normalized = normalizeDashboardAccessTarget(target);
    if (!normalized) continue;
    byId.set(accessFleetCanonicalTargetId({ ...target, ...normalized }), { ...target, ...normalized });
  }
  for (const target of targets) {
    const normalized = normalizeDashboardAccessTarget({
      ...target,
      source: target.source || 'hosted_access',
      connected: target.connected === true || target.online === true,
    });
    if (!normalized) continue;
    const key = accessFleetCanonicalTargetId({ ...target, ...normalized });
    const previous = byId.get(key) || {};
    const previousUpdated = Number(previous.updated_unix_ms || 0);
    const nextUpdated = Number(target.updated_unix_ms || Date.now());
    byId.set(key, {
      ...previous,
      ...target,
      ...normalized,
      id: key,
      host_id: key,
      connect_daemon_id: String(target.connect_daemon_id || target.connectDaemonId || '').trim() ||
        (normalized.local && DASHBOARD_CONNECT_MODE ? DASHBOARD_CONNECT_DAEMON_ID : ''),
      connected: target.connected === true || target.online === true,
      source: target.source || normalized.source || 'hosted_access',
      updated_unix_ms: Math.max(previousUpdated, nextUpdated),
      first_seen_unix_ms: previous.first_seen_unix_ms || target.first_seen_unix_ms || Date.now(),
    });
  }
  accessFleetWrite(Array.from(byId.values()), { skipHostedSync: true });
  const liveTargets = dashboardAccessTargets.filter(target => target.source !== 'browser_fleet');
  dashboardAccessTargets = accessMergeRememberedTargets(liveTargets);
  renderDashboardTargetSummaries();
}

async function accessFleetHydrateFromHosted() {
  if (!accessFleetHostedSyncEnabled()) return;
  try {
    const resp = await fetch(accessFleetHostedUrl('/api/fleet/targets'));
    const body = await resp.json().catch(() => ({}));
    if (resp.ok && body.ok !== false) {
      const targets = await Promise.all(
        (body.targets || []).map(target => accessFleetDecryptRecord(target))
      );
      const locked = targets.filter(t => t?.fleet_locked).length;
      if (locked) console.info(`[fleet-sync] ${locked} record(s) hold encrypted fields; sign in with the account passkey on this device to unlock`);
      accessFleetMergeHostedTargets(targets);
      accessFleetRefreshProvenance().catch(() => {});
    }
  } catch (err) {
    console.warn('[access] hosted fleet hydrate failed', err);
  }
}

function accessFleetScheduleHostedSync() {
  if (!accessFleetHostedSyncEnabled()) return;
  accessFleetHostedSyncDirty = true;
  if (accessFleetHostedSyncTimer || accessFleetHostedSyncInFlight) return;
  accessFleetHostedSyncTimer = window.setTimeout(() => {
    accessFleetHostedSyncTimer = null;
    accessFleetPushToHosted().catch(err => {
      accessFleetHostedSetSyncFailing(true);
      console.warn('[access] hosted fleet sync failed', err);
    });
  }, 400);
}

async function accessFleetPushToHosted() {
  if (!accessFleetHostedSyncEnabled()) return;
  if (accessFleetHostedSyncInFlight) {
    accessFleetHostedSyncDirty = true;
    return;
  }
  accessFleetHostedSyncInFlight = true;
  accessFleetHostedSyncDirty = false;
  try {
    // Sign just-in-time so the pushed record always covers its current
    // content; browsers without WebCrypto push unsigned (shown as such).
    const targets = await Promise.all(
      (accessFleetRead().targets || []).map(async target =>
        accessFleetSignRecord(await accessFleetEncryptRecord(target)))
    );
    const resp = await accessFleetHostedPost('/api/fleet/targets/sync', JSON.stringify({ targets }));
    if (!resp) return; // signed out — nothing to sync, not a failure
    const body = await resp.json().catch(() => ({}));
    if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${resp.status}`);
    accessFleetHostedSetSyncFailing(false);
    const merged = await Promise.all(
      (body.targets || []).map(target => accessFleetDecryptRecord(target))
    );
    accessFleetMergeHostedTargets(merged);
    accessFleetRefreshProvenance().catch(() => {});
  } finally {
    accessFleetHostedSyncInFlight = false;
    if (accessFleetHostedSyncDirty) accessFleetScheduleHostedSync();
  }
}

function accessFleetForgetHostedTarget(hostId) {
  if (!accessFleetHostedSyncEnabled()) return;
  const id = String(hostId || '').trim();
  if (!id) return;
  accessFleetHostedPost(`/api/fleet/targets/${encodeURIComponent(id)}/forget`, '{}')
    .then(resp => {
      if (!resp) return;
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      accessFleetHostedSetSyncFailing(false);
    })
    .catch(err => {
      accessFleetHostedSetSyncFailing(true);
      console.warn('[access] hosted fleet forget failed', err);
    });
}

function accessFleetTargetUrl(target) {
  const explicit = target?.url || target?.browser_tcp_via_url || target?.ws_url || '';
  if (explicit) return explicit;
  if (target?.local && DASHBOARD_CONNECT_MODE && DASHBOARD_CONNECT_DAEMON_ID) {
    const base = String(target?.connect_signaling_base || '').trim().replace(/\/+$/, '');
    const extra = base && base !== window.location.origin
      ? `&connect_base=${encodeURIComponent(base)}`
      : '';
    return `/app?connect=1&daemon_id=${encodeURIComponent(DASHBOARD_CONNECT_DAEMON_ID)}${extra}`;
  }
  if (target?.local && window.location.origin) return window.location.origin;
  return '';
}

function accessFleetRecordFromTarget(target) {
  const normalized = normalizeDashboardAccessTarget(target);
  if (!normalized) return null;
  const now = Date.now();
  const connectDaemonId = normalized.local && DASHBOARD_CONNECT_MODE
    ? DASHBOARD_CONNECT_DAEMON_ID
    : String(normalized.connect_daemon_id || normalized.connectDaemonId || '').trim();
  const canonicalId = accessFleetCanonicalTargetId({
    ...normalized,
    connect_daemon_id: connectDaemonId,
  });
  return {
    id: canonicalId || normalized.id,
    host_id: canonicalId || normalized.host_id,
    label: normalized.label,
    local: normalized.local,
    source: normalized.source || 'dashboard',
    access_domain: normalized.access_domain,
    access_domain_label: normalized.access_domain_label,
    route: normalized.route,
    route_label: normalized.route_label,
    auth: normalized.auth,
    auth_label: normalized.auth_label,
    effective_role: normalized.effective_role,
    effective_role_label: normalized.effective_role_label,
    profile: normalized.profile,
    connected: normalized.connected,
    url: accessFleetTargetUrl(normalized),
    ws_url: normalized.ws_url || '',
    browser_tcp_via_url: normalized.browser_tcp_via_url || '',
    capabilities: normalized.capabilities || [],
    origin: window.location.origin || '',
    connect_daemon_id: connectDaemonId,
    // Phase 7: the rendezvous this daemon is reachable through — from its
    // agent card (rendezvous_base) or, when browsing through a rendezvous
    // right now, the one in use.
    connect_signaling_base: String(
      normalized.connect_signaling_base || normalized.rendezvous_base ||
      (normalized.local && DASHBOARD_CONNECT_MODE
        ? (DASHBOARD_CONNECT_SIGNALING_BASE || window.location.origin)
        : '')
    ).trim().replace(/\/+$/, ''),
    first_seen_unix_ms: normalized.first_seen_unix_ms || now,
    last_seen_unix_ms: now,
  };
}

function accessFleetRememberTargets(targets) {
  const store = accessFleetRead();
  const byId = new Map();
  for (const target of store.targets || []) {
    const normalized = normalizeDashboardAccessTarget(target);
    if (!normalized) continue;
    byId.set(accessFleetCanonicalTargetId({ ...target, ...normalized }), { ...target, ...normalized });
  }
  for (const target of targets || []) {
    const record = accessFleetRecordFromTarget(target);
    if (!record) continue;
    const key = accessFleetCanonicalTargetId(record);
    const previous = byId.get(key) || {};
    byId.set(key, {
      ...previous,
      ...record,
      first_seen_unix_ms: previous.first_seen_unix_ms || record.first_seen_unix_ms,
    });
  }
  accessFleetWrite(Array.from(byId.values()));
}

function accessFleetTargets() {
  return accessFleetRead().targets
    .map(target => normalizeDashboardAccessTarget({
      ...target,
      source: target.source || 'browser_fleet',
      connected: target.connected === true,
    }))
    .filter(Boolean);
}

function accessFleetForgetTarget(hostId) {
  const id = String(hostId || '').trim();
  if (!id) return;
  const store = accessFleetRead();
  accessFleetWrite((store.targets || []).filter(target => {
    const normalized = normalizeDashboardAccessTarget(target);
    return normalized && normalized.host_id !== id && normalized.id !== id;
  }));
  accessFleetForgetHostedTarget(id);
}

function accessMergeRememberedTargets(liveTargets) {
  const out = [];
  const seen = new Set();
  for (const target of liveTargets || []) {
    const normalized = normalizeDashboardAccessTarget(target);
    if (!normalized) continue;
    const key = normalized.host_id || normalized.id;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(normalized);
  }
  for (const target of accessFleetTargets()) {
    const key = target.host_id || target.id;
    if (!key || seen.has(key)) continue;
    seen.add(key);
    out.push({
      ...target,
      source: 'browser_fleet',
      connected: false,
      route_label: target.route_label || 'Remembered route',
      auth_label: target.auth_label || 'Browser-local fleet record',
      effective_role_label: target.effective_role_label || 'Unknown',
    });
  }
  return out;
}

function applyDashboardAccessTargets(payload) {
  const rawTargets = Array.isArray(payload?.targets) ? payload.targets : [];
  const liveTargets = rawTargets
    .map(normalizeDashboardAccessTarget)
    .map(decorateDashboardAccessTarget)
    .filter(Boolean);
  accessFleetRememberTargets(liveTargets);
  dashboardAccessTargets = accessMergeRememberedTargets(liveTargets);
  renderDashboardTargetSummaries();
}

function normalizeAccessOverview(payload) {
  if (!payload || typeof payload !== 'object') return null;
  const normalizeArray = key => Array.isArray(payload[key]) ? payload[key].filter(Boolean) : [];
  return {
    ...payload,
    schema_version: Number(payload.schema_version || payload.schemaVersion || 1),
    scope: payload.scope && typeof payload.scope === 'object' ? payload.scope : {},
    targets: normalizeArray('targets'),
    principals: normalizeArray('principals'),
    grants: normalizeArray('grants'),
    policies: normalizeArray('policies'),
    permissions: normalizeArray('permissions'),
    transports: normalizeArray('transports'),
    supported_principal_kinds: normalizeArray('supported_principal_kinds'),
    architecture: payload.architecture && typeof payload.architecture === 'object'
      ? payload.architecture
      : {},
    iam: payload.iam && typeof payload.iam === 'object'
      ? payload.iam
      : {},
  };
}

function applyAccessOverview(payload) {
  const overview = normalizeAccessOverview(payload);
  if (!overview) return null;
  dashboardAccessOverview = overview;
  if (overview.targets.length) applyDashboardAccessTargets(overview);
  else renderAccessAdminSummaries();
  return overview;
}

function invalidateAccessOverview() {
  dashboardAccessOverview = null;
}

async function refreshAccessOverviewFromApi(options = {}) {
  try {
    // daemonApi (transport F4): GET twin — tunnel first, direct HTTP per
    // the read fallback policy; Connect mode never falls back.
    const resp = await daemonApi.request('api_access_overview', {}, {
      signal: options.signal,
      cache: 'no-store',
    });
    if (!resp.ok) throw new Error(`/api/access/overview returned ${resp.status}`);
    return applyAccessOverview(resp.body);
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    if (!options.silent) {
      console.warn('[access-overview] refresh failed; using derived access model', err);
    }
    return null;
  }
}

/* Pending browser-key enrollment requests (devices knocking on this daemon).
   Pull-based like the overview; renderers read the cached list. */
let accessPendingEnrollments = [];

async function refreshAccessEnrollments(options = {}) {
  try {
    // daemonApi (transport F4): GET twin — read fallback policy.
    const resp = await daemonApi.request('api_access_enrollment_requests', {}, {
      signal: options.signal,
      cache: 'no-store',
    });
    if (!resp.ok) throw new Error(`/api/access/enrollment-requests returned ${resp.status}`);
    const requests = Array.isArray(resp.body?.requests) ? resp.body.requests : [];
    const changed = JSON.stringify(requests) !== JSON.stringify(accessPendingEnrollments);
    accessPendingEnrollments = requests;
    if (changed) renderAccessAdminSummaries();
    return requests;
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    if (!options.silent) {
      console.warn('[access-enrollments] refresh failed', err);
    }
    return null;
  }
}

async function accessDecideEnrollment(fingerprint, approve, roleId) {
  const payload = { fingerprint, approve };
  if (approve && roleId) payload.role_id = roleId;
  try {
    // daemonApi (transport F4): POST twin — the fallback policy derives
    // no-replay from the verb, exactly the legacy
    // fallbackAfterRpcFailure:false semantics (a delivered tunnel attempt
    // is never replayed over HTTP; with no tunnel the write goes direct).
    // IAM-mutating write: the payload shape is unchanged, no retries.
    const resp = await daemonApi.request('api_access_enrollment_decide', payload);
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', approve ? 'Device approved — it can connect now' : 'Device request denied');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Enrollment decision failed');
  }
  // The role select's value was consumed by the decision — re-baseline it
  // so the shared render guard lets the pending-devices list rebuild.
  accessGuardStamp(document.getElementById('access-enrollment-requests'));
  await refreshAccessEnrollments({ silent: true }).catch(() => null);
  await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
  renderAccessAdminSummaries();
}

/* Intendant Connect status for THIS daemon (hosted rendezvous
   reachability + claim binding). Pull-based like the overview. The
   twelve-word claim phrase is NEVER in this payload — it has its own
   manage-gated fetch (`accessConnectFetchClaimCode`), pulled only on an
   explicit reveal click. */
let accessConnectStatus = null;

async function refreshAccessConnectStatus(options = {}) {
  try {
    // daemonApi (transport F4): GET twin — read fallback policy.
    const resp = await daemonApi.request('api_access_connect_status', {}, {
      signal: options.signal,
      cache: 'no-store',
    });
    if (!resp.ok) throw new Error(`/api/access/connect/status returned ${resp.status}`);
    const payload = resp.body;
    const changed = JSON.stringify(payload) !== JSON.stringify(accessConnectStatus);
    accessConnectStatus = payload;
    if (changed) renderAccessAdminSummaries();
    return payload;
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    if (!options.silent) console.warn('[access-connect] status refresh failed', err);
    return null;
  }
}

/* Live dashboard connections (tab presence): who else has this daemon's
   dashboard open, over which lane, and who holds voice / display input.
   Pull-based like the overview; renderers read the cached payload. */
let dashboardTabsPresence = null;

async function refreshDashboardTabs(options = {}) {
  try {
    // daemonApi (transport F4): GET twin — read fallback policy.
    const resp = await daemonApi.request('api_dashboard_tabs', {}, {
      signal: options.signal,
      cache: 'no-store',
    });
    if (!resp.ok) throw new Error(`/api/dashboard/tabs returned ${resp.status}`);
    const payload = resp.body;
    const changed = JSON.stringify(payload) !== JSON.stringify(dashboardTabsPresence);
    dashboardTabsPresence = payload;
    if (changed) renderAccessAdminSummaries();
    return payload;
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    if (!options.silent) console.warn('[dashboard-tabs] refresh failed', err);
    return null;
  }
}

// Presence changes when OTHER tabs connect or leave — nothing pushes that
// to this tab (v1 is pull-only), so poll gently, and only while the
// Access pane is actually on screen in a foreground tab.
setInterval(() => {
  if (typeof paneIsVisible === 'function' && paneIsVisible('access')
      && document.visibilityState === 'visible') {
    refreshDashboardTabs({ silent: true }).catch(() => {});
  }
}, 15000);

async function accessConnectFetchClaimCode() {
  // daemonApi (transport F4): GET twin, so the derived policy allows the
  // direct-HTTP retry after a failed tunnel attempt (the legacy call site
  // opted out via fallbackAfterRpcFailure:false, but a manage-gated
  // idempotent read replays safely — the policy is data, not per-site
  // judgment). Connect mode never falls back.
  const resp = await daemonApi.request('api_access_connect_claim_code', {}, { cache: 'no-store' });
  const data = resp.body;
  if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
  return data;
}

async function accessConnectSetEnabled(enabled, rendezvousUrl) {
  const payload = { enabled };
  if (rendezvousUrl) payload.rendezvous_url = rendezvousUrl;
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay, the
    // legacy fallbackAfterRpcFailure:false semantics.
    const resp = await daemonApi.request('api_access_connect_config', payload);
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', enabled
      ? 'Connect enabled — this daemon is registering with the rendezvous'
      : 'Connect disabled');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Connect config change failed');
  }
  await refreshAccessConnectStatus({ silent: true }).catch(() => null);
  renderAccessAdminSummaries();
}

async function accessSetTier(tier) {
  const payload = { tier: tier || null };
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay.
    const resp = await daemonApi.request('api_access_set_tier', payload);
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', tier
      ? `Trust tier set: ${tier}`
      : 'Trust tier cleared');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Trust tier change failed');
  }
  await refreshAccessOverviewFromApi().catch(() => null);
  renderAccessAdminSummaries();
}

async function accessSetHostedCeiling(roleId) {
  const payload = { role_id: roleId };
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay.
    const resp = await daemonApi.request('api_access_set_hosted_ceiling', payload);
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', roleId === 'role:none'
      ? 'Hosted-origin control refused. Reach this daemon directly, through the app, or from an enrolled peer — a hosted tab can no longer change this back.'
      : `Hosted control ceiling set to ${roleId.replace(/^role:/, '')}`);
  } catch (err) {
    showControlToast?.('error', err?.message || 'Hosted ceiling change failed');
  }
  await refreshAccessOverviewFromApi().catch(() => null);
  renderAccessAdminSummaries();
}

async function accessFleetCertRequest() {
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay. The
    // tunnel-only reject fallback died with the S6 ROW-NEW
    // (POST /api/access/fleet-cert/request): a dashboard with no tunnel
    // now starts the request over direct HTTP instead of erroring.
    const resp = await daemonApi.request('api_fleet_cert_request', {});
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', 'Certificate request started — publishing DNS records and answering the Let’s Encrypt challenge (usually under a minute).');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Certificate request failed');
  }
  // Poll the card a few times while the async flow runs.
  for (const delay of [5000, 15000, 40000]) {
    setTimeout(() => {
      refreshAccessConnectStatus({ silent: true }).catch(() => null);
      renderAccessAdminSummaries();
    }, delay);
  }
  await refreshAccessConnectStatus({ silent: true }).catch(() => null);
  renderAccessAdminSummaries();
}

async function accessConnectUnclaim() {
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay. The
    // empty params ride body-less on HTTP; the unclaim handler never
    // reads a body.
    const resp = await daemonApi.request('api_access_connect_unclaim', {});
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    showControlToast?.('success', 'Claim released — a fresh claim phrase will mint shortly');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Unclaim failed');
  }
  await refreshAccessConnectStatus({ silent: true }).catch(() => null);
  renderAccessAdminSummaries();
}

function upsertDashboardAccessTarget(target) {
  const normalized = normalizeDashboardAccessTarget(target);
  if (!normalized) return;
  invalidateAccessOverview();
  const idx = dashboardAccessTargets.findIndex(t =>
    t.host_id === normalized.host_id || t.id === normalized.id
  );
  if (idx >= 0) dashboardAccessTargets[idx] = normalized;
  else dashboardAccessTargets.push(normalized);
  accessFleetRememberTargets([normalized]);
}

function removeDashboardAccessTarget(hostId) {
  const id = String(hostId || '').trim();
  if (!id) return;
  invalidateAccessOverview();
  accessFleetForgetTarget(id);
  dashboardAccessTargets = dashboardAccessTargets.filter(t =>
    t.local || (t.host_id !== id && t.id !== id)
  );
}

function dashboardAccessTargetFromPeerSnapshot(snap) {
  if (!snap || !snap.id) return null;
  const connected = snap.connection_state && snap.connection_state.state === 'connected';
  return {
    id: snap.id,
    host_id: snap.id,
    label: snap.label || snap.id,
    local: false,
    source: 'peer-registry',
    access_domain: 'peer',
    access_domain_label: 'Peer access',
    route: 'peer_route',
    route_label: 'Peer route',
    auth: 'daemon_mutual_tls',
    auth_label: 'Daemon mTLS grant',
    effective_role: 'peer_profile',
    effective_role_label: 'Peer profile',
    connected,
    connection_state: snap.connection_state,
    operational_status: snap.status,
    url: snap.browser_tcp_via_url || snap.ws_url || '',
    ws_url: snap.ws_url || '',
    browser_tcp_via_url: snap.browser_tcp_via_url || '',
    capabilities: snap.capabilities || [],
  };
}

function dashboardAccessTargetForHost(hostId) {
  const id = String(hostId || '').trim();
  if (!id || id === SHELL_HOST_ID || id === selfPeerId) {
    return dashboardAccessTargets.find(t => t.local) || null;
  }
  return dashboardAccessTargets.find(t => t.host_id === id || t.id === id) || null;
}

async function refreshDashboardTargetsFromApi(options = {}) {
  try {
    // daemonApi (transport F4): GET twin — read fallback policy.
    const resp = await daemonApi.request('api_dashboard_targets', {}, {
      signal: options.signal,
      cache: 'no-store',
    });
    if (!resp.ok) throw new Error(`/api/dashboard/targets returned ${resp.status}`);
    const payload = resp.body;
    applyDashboardAccessTargets(payload);
    return payload;
  } catch (err) {
    if (err?.name === 'AbortError') throw err;
    if (!options.silent) {
      console.warn('[dashboard-targets] refresh failed; using derived targets', err);
    }
    return null;
  }
}

function dashboardTargetIsLocal(hostId) {
  const id = String(hostId || '').trim();
  return !id || id === SHELL_HOST_ID || id === selfPeerId;
}

function dashboardTargetLabel(hostId) {
  if (dashboardTargetIsLocal(hostId)) return selfHostLabel || 'This daemon';
  const id = String(hostId || '').trim();
  const peer = daemons.find(d => d.host_id === id);
  // Petname first (owner-chosen, identity-bound); the self-reported
  // label only names machines the owner hasn't named.
  const record = accessFleetTargets().find(t =>
    String(t.host_id || t.id || '').trim() === id);
  const petname = String(record?.petname || '').trim();
  if (petname) return petname;
  return peer?.label || id;
}

/* The owner's petname for a fleet target, '' when unnamed. */
function accessTargetPetname(target) {
  return String(target?.petname || '').trim();
}

/* Set (or clear, with '') the owner's petname for a fleet target and
   persist it: the record re-signs as payload v5 on the next hosted
   push, so the name is bound to this identity — a lookalike never
   inherits it. */
function accessFleetSetPetname(hostId, petname) {
  const id = String(hostId || '').trim();
  if (!id) return false;
  const cleaned = String(petname || '').trim().slice(0, 120);
  const targets = accessFleetRead().targets || [];
  let changed = false;
  for (const target of targets) {
    const key = String(target.host_id || target.id || '').trim();
    if (key !== id) continue;
    if (String(target.petname || '') === cleaned) break;
    target.petname = cleaned;
    target.updated_unix_ms = Date.now();
    changed = true;
    break;
  }
  if (changed) accessFleetWrite(targets);
  return changed;
}

function dashboardAccessRoleLabel(roleId) {
  const key = String(roleId || '').trim();
  return {
    'role:root': 'Root',
    'role:scoped-human': 'Scoped human',
    'role:observer': 'Observer',
    'role:session-reader': 'Session reader',
    'role:terminal': 'Terminal',
    'role:files-read': 'Files read',
    'role:files-write': 'Files write',
    'role:operator': 'Operator',
  }[key] || (key ? key.replace(/^role:/, '').replace(/-/g, ' ') : 'Root');
}

function dashboardCurrentAccessRouteInfo() {
  const status = dashboardControlTransport?.lastStatus || {};
  const principal = status.access_principal && typeof status.access_principal === 'object'
    ? status.access_principal
    : {};
  const source = String(principal.source || '').trim();
  const transport = String(principal.transport || '').trim();
  const kind = String(principal.kind || '').trim();
  const roleLabel = dashboardAccessRoleLabel(principal.role_id || 'role:root');
  const connectAccount = principal.account?.account_name
    ? `@${principal.account.account_name}`
    : (principal.account?.user_id ? 'Connect account' : 'Connect account');
  const isConnect = dashboardConnectModeEnabled() ||
    source === 'connect-account' ||
    transport.includes('connect') ||
    kind === 'connect_account';
  if (isConnect) {
    return {
      domain: 'User/client access',
      route: 'Intendant Connect',
      auth: 'connect_account',
      authLabel: connectAccount,
      role: roleLabel,
      detail: `${connectAccount} over the hosted dashboard tunnel`,
    };
  }
  const isBrowserCert = kind === 'browser_certificate' ||
    source === 'browser-mtls' ||
    transport === 'https' ||
    location.protocol === 'https:';
  if (isBrowserCert) {
    return {
      domain: 'User/client access',
      route: 'Browser mTLS',
      auth: 'browser_mtls_cert',
      authLabel: kind === 'browser_certificate' ? 'Browser certificate grant' : 'Trusted browser certificate',
      role: roleLabel,
      detail: 'trusted browser client certificate',
    };
  }
  return {
    domain: 'User/client access',
    route: 'Local/debug',
    auth: 'trusted_dashboard',
    authLabel: 'Trusted local dashboard',
    role: roleLabel,
    detail: 'local browser session',
  };
}

function decorateDashboardAccessTarget(target) {
  if (!target || !target.local) return target;
  const route = dashboardCurrentAccessRouteInfo();
  return {
    ...target,
    access_domain: 'user_client',
    access_domain_label: route.domain,
    route: route.route.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_|_$/g, '') || 'current_dashboard',
    route_label: route.route,
    auth: route.auth,
    auth_label: route.authLabel,
    effective_role: String((dashboardControlTransport?.lastStatus?.access_principal?.role_id) || 'role:root'),
    effective_role_label: route.role,
  };
}

function dashboardTargetAccessRoute(hostId) {
  const model = dashboardAccessTargetForHost(hostId);
  if (dashboardTargetIsLocal(hostId)) {
    const route = dashboardCurrentAccessRouteInfo();
    return {
      domain: route.domain,
      route: route.route,
      role: route.role,
      detail: route.detail,
    };
  }
  if (model) {
    return {
      domain: model.access_domain_label || 'Peer access',
      route: model.route_label || 'Peer route',
      role: model.effective_role_label || 'Peer profile',
      detail: model.auth_label || 'daemon-to-daemon grant',
    };
  }
  return {
    domain: 'Peer access',
    route: 'Peer route',
    role: 'Peer profile',
    detail: 'daemon-to-daemon grant',
  };
}

function dashboardTargetDescriptor(hostId) {
  const local = dashboardTargetIsLocal(hostId);
  const peerId = local ? '' : String(hostId || '').trim();
  const model = dashboardAccessTargetForHost(peerId);
  const route = dashboardTargetAccessRoute(peerId);
  const peer = local ? null : daemons.find(d => d.host_id === peerId) || null;
  return {
    targetId: model?.id || (local ? (selfPeerId || SHELL_HOST_ID) : peerId),
    hostId: peerId,
    displayName: model?.label || dashboardTargetLabel(peerId),
    local,
    peer,
    accessDomain: route.domain,
    routeType: route.route,
    accessRole: route.role,
    routeDetail: route.detail,
    model,
  };
}

function dashboardProfileLabel(profile) {
  const key = String(profile || '').trim().toLowerCase().replace(/_/g, '-');
  return {
    'peer-root': 'Peer root',
    'peer-daemon': 'Peer root',
    'admin-peer': 'Peer root',
    admin: 'Peer root',
    operator: 'Peer operator',
    'peer-operator': 'Peer operator',
    'terminal-operator': 'Terminal operator',
    'peer-terminal-operator': 'Terminal operator',
    terminal: 'Terminal operator',
    shell: 'Terminal operator',
    'session-reader': 'Session reader',
    'sessions-read': 'Session reader',
    'session-inspect': 'Session reader',
    'shared-session-spectator': 'Session spectator',
    spectator: 'Session spectator',
    'read-only-display': 'Read-only display',
    'task-runner': 'Task runner',
    stats: 'Stats only',
    'presence-only': 'Presence only',
  }[key] || (key ? key.replace(/-/g, ' ') : 'Peer access');
}

function dashboardCapabilityState(available, ready = false) {
  if (available === true) return 'ok';
  if (ready === true) return 'ready';
  return 'off';
}

function dashboardTargetFeatureChips(hostId, context = '') {
  const local = dashboardTargetIsLocal(hostId);
  const raw = local
    ? (dashboardControlTransport?.lastStatus || {})
    : (peerDashboardControlConnectionsByHost.get(String(hostId || '').trim())?.lastStatus || {});
  const directLocalFallback = local && !dashboardConnectModeEnabled();
  if (local) {
    return [
      {
        label: 'Sessions',
        state: dashboardCapabilityState(raw.api_sessions_available === true || directLocalFallback),
      },
      {
        label: 'Files',
        state: dashboardCapabilityState(
          raw.api_fs_read_available === true ||
          dashboardByteStreamMethodAvailable('api_fs_read') ||
          directLocalFallback
        ),
      },
      {
        label: 'Shell',
        state: dashboardCapabilityState(raw.terminal_frames_available === true || directLocalFallback),
      },
    ];
  }

  const peerId = String(hostId || '').trim();
  const peer = daemons.find(d => d.host_id === peerId);
  const online = Boolean(peer && peer.connected !== false);
  const controlReady = online && peerDashboardControlSignalAvailable(peerId);
  const fileReady = online && (controlReady || peerFileTransferSignalAvailable(peerId));
  const chips = [
    {
      label: context === 'files' ? 'Files' : 'Sessions',
      state: context === 'files'
        ? dashboardCapabilityState(raw.api_fs_read_available === true, fileReady)
        : dashboardCapabilityState(raw.api_sessions_available === true, controlReady),
    },
    {
      label: context === 'files' ? 'Sessions' : 'Shell',
      state: context === 'files'
        ? dashboardCapabilityState(raw.api_sessions_available === true, controlReady)
        : dashboardCapabilityState(raw.terminal_frames_available === true, controlReady),
    },
    {
      label: context === 'files' ? 'Shell' : 'Files',
      state: context === 'files'
        ? dashboardCapabilityState(raw.terminal_frames_available === true, controlReady)
        : dashboardCapabilityState(raw.api_fs_read_available === true, fileReady),
    },
  ];
  if (peer && peerCanShareDisplay(peer)) chips.push({ label: 'Video', state: online ? 'ok' : 'off' });
  return chips;
}

/* The two-lane badge (docs/src/trust-tiers.md § Two lanes): the ONE
   thing a user must understand about any fleet surface — whose
   authority the pane is spending on the target.
   - user lane:        { lane:'user',  text:'you · <role>' }
   - delegation lane:  { lane:'via',   text:'via <daemon> · <profile>'
                         (+ ' · you' when this browser's signed offer
                         was attributed by the target) }
   - warn: true only for the one case the doctrine warns about —
     reaching an INTEGRATED machine through the delegation lane. */
function dashboardLaneBadge(hostId) {
  const target = dashboardTargetDescriptor(hostId);
  if (target.local) {
    const role = String(target.accessRole || '').trim() || 'root';
    return { lane: 'user', text: `you · ${role}`, warn: false };
  }
  const conn = peerDashboardControlConnectionsByHost.get(String(hostId || '').trim());
  const status = conn?.lastStatus || {};
  const profile = String(status.grant_profile || target.profile || '').trim() || 'peer';
  const viaLabel = String(selfHostLabel || 'this daemon').trim();
  const attributed = Boolean(status.attributed_fingerprint);
  const record = accessFleetTargets().find(t =>
    String(t.host_id || t.id || '').trim() === String(hostId || '').trim());
  const warn = String(record?.tier || '').trim() === 'integrated';
  return {
    lane: 'via',
    text: `via ${viaLabel} · ${profile}${attributed ? ' · you' : ''}`,
    warn,
    title: warn
      ? 'This machine is marked integrated — its owner should be reached as themselves (direct tab or app), not through another daemon\u2019s grant.'
      : `Acting through ${viaLabel}\u2019s peer grant (profile: ${profile})${attributed ? ' — your identity key is attributed on the target' : ''}.`,
  };
}

function dashboardTargetSummary(hostId, context = '') {
  const target = dashboardTargetDescriptor(hostId);
  const local = target.local;
  if (local) {
    const status = dashboardTransport?.status
      ? dashboardTransport.status()
      : { enabled: dashboardControlTransportEnabled(), connected: false };
    const transport = dashboardTransportStatusSummary(status);
    const connected = !status.enabled || Boolean(status.connected && status.verifiedBinding?.ok);
    const detail = connected
      ? `${target.accessDomain}: ${target.accessRole} via ${target.routeType}`
      : `${target.accessDomain}: ${target.routeType} · ${transport.label || 'connecting'}`;
    return {
      state: transport.kind === 'err' ? 'err' : (connected ? 'ok' : 'checking'),
      name: target.displayName,
      kind: dashboardLaneBadge(hostId).text,
      detail,
      chips: dashboardTargetFeatureChips('', context),
      title: transport.title || detail,
    };
  }

  const laneBadge = dashboardLaneBadge(hostId);
  const peerId = target.hostId;
  const peer = target.peer;
  if (!peer) {
    return {
      state: 'err',
      name: peerId || 'Unknown peer',
      kind: 'Peer',
      detail: 'This peer is no longer configured',
      chips: dashboardTargetFeatureChips(peerId, context),
      title: 'The selected peer was removed from this daemon.',
    };
  }
  const online = peer.connected !== false;
  const conn = peerDashboardControlConnectionsByHost.get(peerId);
  const status = conn?.lastStatus || {};
  const profile = status.grant_profile || '';
  const grantKind = status.grant_kind || '';
  const hasExactGrant = Boolean(profile || grantKind);
  const access = hasExactGrant
    ? `Peer profile: ${dashboardProfileLabel(profile)}`
    : (online ? 'Approved peer access · connects when needed' : 'Peer offline');
  const via = conn?.canUseRpc?.()
    ? 'direct browser access'
    : (online ? 'ready for direct browser access' : 'not reachable');
  return {
    state: online ? (conn?.canUseRpc?.() ? 'ok' : 'checking') : 'err',
    name: dashboardTargetLabel(peerId),
    kind: laneBadge.text,
    kindWarn: laneBadge.warn,
    kindTitle: laneBadge.title || '',
    detail: `${target.accessDomain}: ${access} · ${via}`,
    chips: dashboardTargetFeatureChips(peerId, context),
    title: online
      ? `${access}. ${peer.url || peer.ws_url || ''}`.trim()
      : 'The peer is configured but not connected.',
  };
}

function renderDashboardTargetSummary(elementId, hostId, context = '') {
  const el = document.getElementById(elementId);
  if (!el) return;
  const summary = dashboardTargetSummary(hostId, context);
  el.className = `target-summary ${summary.state || 'checking'}`;
  el.title = summary.title || summary.detail || '';
  el.innerHTML = '';

  const main = document.createElement('div');
  main.className = 'target-summary-main';
  const dot = document.createElement('span');
  dot.className = 'target-summary-dot';
  dot.setAttribute('aria-hidden', 'true');
  const row = document.createElement('div');
  row.className = 'target-summary-name-row';
  const name = document.createElement('span');
  name.className = 'target-summary-name';
  name.textContent = summary.name || 'Target';
  const kind = document.createElement('span');
  kind.className = `target-summary-kind${summary.kindWarn ? ' lane-warn' : ''}`;
  kind.textContent = summary.kind || '';
  if (summary.kindTitle) kind.title = summary.kindTitle;
  row.append(name, kind);
  const detail = document.createElement('div');
  detail.className = 'target-summary-detail';
  detail.textContent = summary.detail || '';
  main.append(dot, row, detail);

  const chips = document.createElement('div');
  chips.className = 'target-summary-chips';
  for (const chip of summary.chips || []) {
    const item = document.createElement('span');
    item.className = `target-chip ${chip.state || 'off'}`;
    item.textContent = chip.label || '';
    chips.appendChild(item);
  }
  el.append(main, chips);
}

function renderDashboardTargetSummaries() {
  renderDashboardTargetSummary('files-target-summary', filesDownloadSelectedPeerId(), 'files');
  renderDashboardTargetSummary('files-ide-target-summary', filesIdeSelectedHostId(), 'files');
  renderDashboardTargetSummary('shell-target-summary', currentShellHostId(), 'shell');
  renderAccessAdminSummaries();
}

function accessOpenStats(hostId) {
  const target = String(hostId || '').trim();
  routeTo('stats');
  requestAnimationFrame(() => switchStatsHost(target));
}

function accessOpenFiles(hostId) {
  const target = String(hostId || '').trim();
  routeTo('files');
  requestAnimationFrame(() => {
    const select = document.getElementById('files-download-host');
    if (select) {
      const hasOption = Array.from(select.options).some(option => option.value === target);
      if (hasOption) {
        select.value = target;
        onFilesDownloadHostChanged({ preserveStatus: true });
      }
    }
  });
}

function accessOpenShell(hostId) {
  const target = String(hostId || '').trim();
  routeTo('terminal', 'shell');
  requestAnimationFrame(() => setShellHost(target || SHELL_HOST_ID));
}

function accessOpenDisplay(hostId) {
  const peerId = String(hostId || '').trim();
  const peer = daemons.find(d => d.host_id === peerId);
  if (!peer) {
    showControlToast('error', 'Peer target is no longer configured');
    return;
  }
  const tcpViaUrl = resolveBrowserTcpViaUrl(peer);
  openPeerDisplay(peerId, 0, tcpViaUrl).catch(err => {
    console.error('openPeerDisplay failed:', err);
    showControlToast('error', `Display failed: ${err?.message || err}`);
  });
}

function accessManageTargetAccess(hostId) {
  const peerId = String(hostId || '').trim();
  routeTo('access', peerId ? 'peers' : 'people');
}

async function accessRemoveTarget(hostId, label) {
  const peerId = String(hostId || '').trim();
  if (!peerId) return;
  const ok = await showDashboardConfirm({
    title: 'Remove access target',
    message: `Remove ${label || peerId} from this dashboard?`,
    warning: 'This removes the outbound peer route from this daemon. It does not revoke any inbound grant on the peer.',
    confirmLabel: 'Remove',
    danger: true,
  });
  if (!ok) return;
  await removeDaemon(peerId);
  showControlToast('success', `Removed ${label || peerId}`);
}

function accessCreateCard(card) {
  const node = document.createElement('div');
  node.className = 'access-card';
  if (card.title) node.title = card.title;

  const head = document.createElement('div');
  head.className = 'access-card-head';
  const title = document.createElement('div');
  title.className = 'access-card-title';
  title.textContent = card.title || 'Access';
  const kind = document.createElement('div');
  // Semantic state rides as a class (ok|warn) so the pill can carry tint;
  // cards without a state stay neutral.
  kind.className = 'access-card-kind' + (card.state ? ` ${card.state}` : '');
  kind.textContent = card.kind || '';
  head.append(title, kind);

  const detail = document.createElement('div');
  detail.className = 'access-card-detail';
  detail.textContent = card.detail || '';
  node.append(head, detail);

  const actions = Array.isArray(card.actions) ? card.actions.filter(Boolean) : [];
  if (actions.length) {
    const actionRow = document.createElement('div');
    actionRow.className = 'access-card-actions';
    for (const action of actions) {
      const button = document.createElement('button');
      button.type = 'button';
      if (action.primary) button.classList.add('primary');
      button.textContent = action.label || 'Open';
      button.addEventListener('click', action.onClick);
      actionRow.appendChild(button);
    }
    node.appendChild(actionRow);
  }

  return node;
}

function accessRenderCards(containerId, cards) {
  const el = document.getElementById(containerId);
  if (!el) return;
  el.innerHTML = '';
  if (!cards.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'Nothing configured';
    el.appendChild(empty);
    return;
  }
  for (const card of cards) el.appendChild(accessCreateCard(card));
}

function accessOverviewArray(overview, key) {
  return Array.isArray(overview?.[key]) ? overview[key] : [];
}

function accessDetailValue(value, fallback = 'None') {
  const text = String(value ?? '').trim();
  return text || fallback;
}

function accessCreateDetailCard({ title, status, lines = [], actions = [] }) {
  const card = document.createElement('div');
  card.className = 'access-detail-card';
  const head = document.createElement('div');
  head.className = 'access-detail-head';
  const titleEl = document.createElement('div');
  titleEl.className = 'access-detail-title';
  titleEl.textContent = title || 'Access detail';
  head.appendChild(titleEl);
  if (status) head.appendChild(accessModelCreateStatus(status));
  const body = document.createElement('div');
  body.className = 'access-detail-body';
  for (const [label, value, titleValue] of lines) {
    const line = document.createElement('div');
    line.className = 'access-detail-line';
    const key = document.createElement('strong');
    key.textContent = label || '';
    const val = document.createElement('span');
    val.textContent = accessDetailValue(value);
    val.title = accessDetailValue(titleValue ?? value);
    line.append(key, val);
    body.appendChild(line);
  }
  card.append(head, body);
  const usableActions = Array.isArray(actions) ? actions.filter(Boolean) : [];
  if (usableActions.length) {
    const actionRow = document.createElement('div');
    actionRow.className = 'access-card-actions access-detail-actions';
    for (const action of usableActions) {
      const button = document.createElement('button');
      button.type = 'button';
      if (action.primary) button.classList.add('primary');
      if (action.danger) button.classList.add('danger');
      button.disabled = Boolean(action.disabled);
      button.textContent = action.label || 'Open';
      button.addEventListener('click', action.onClick);
      actionRow.appendChild(button);
    }
    card.appendChild(actionRow);
  }
  return card;
}

function accessRenderDetailCards(containerId, cards) {
  const el = document.getElementById(containerId);
  if (!el) return;
  el.innerHTML = '';
  if (!cards.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'Nothing configured';
    el.appendChild(empty);
    return;
  }
  const grid = document.createElement('div');
  grid.className = 'access-admin-grid two';
  for (const card of cards) grid.appendChild(accessCreateDetailCard(card));
  el.appendChild(grid);
}

function dashboardPeerAccessTargets() {
  const modeled = dashboardAccessTargets.filter(t => !t.local);
  if (modeled.length) return modeled;
  return daemons.map(peer => ({
    id: peer.host_id || peer.id || '',
    host_id: peer.host_id || peer.id || '',
    label: peer.label || peer.host_id || peer.id || '',
    local: false,
    access_domain_label: 'Peer access',
    route_label: 'Peer route',
    effective_role_label: 'Peer profile',
    connected: peer.connected !== false,
    url: peer.url || peer.ws_url || '',
  })).filter(t => t.host_id);
}

function syncDashboardAccessTargetsFromDaemons() {
  invalidateAccessOverview();
  const localTargets = dashboardAccessTargets.filter(t => t.local);
  const peerTargets = daemons
    .map(peer => normalizeDashboardAccessTarget({
      id: peer.host_id || peer.id || '',
      host_id: peer.host_id || peer.id || '',
      label: peer.label || peer.host_id || peer.id || '',
      local: false,
      source: 'peer-registry',
      access_domain: 'peer',
      access_domain_label: 'Peer access',
      route: 'peer_route',
      route_label: 'Peer route',
      auth: 'daemon_mutual_tls',
      auth_label: 'Daemon mTLS grant',
      effective_role: 'peer_profile',
      effective_role_label: 'Peer profile',
      connected: peer.connected !== false,
      url: peer.browser_tcp_via_url || peer.url || peer.ws_url || '',
      ws_url: peer.ws_url || '',
      browser_tcp_via_url: peer.browser_tcp_via_url || '',
      capabilities: peer.capabilities || [],
    }))
    .filter(Boolean);
  const liveTargets = [...localTargets, ...peerTargets];
  accessFleetRememberTargets(liveTargets);
  dashboardAccessTargets = accessMergeRememberedTargets(liveTargets);
  renderDashboardTargetSummaries();
}

function accessLocalTargetModel() {
  return decorateDashboardAccessTarget(dashboardAccessTargets.find(t => t.local) || normalizeDashboardAccessTarget({
    id: selfPeerId || SHELL_HOST_ID,
    host_id: selfPeerId || SHELL_HOST_ID,
    label: selfHostLabel || 'This daemon',
    local: true,
    source: 'dashboard',
    access_domain: 'user_client',
    access_domain_label: 'User/client access',
    route: 'current_dashboard',
    route_label: 'Current dashboard',
    auth: 'trusted_dashboard',
    auth_label: 'Trusted dashboard session',
    effective_role: 'root',
    effective_role_label: 'Root',
    connected: true,
    capabilities: [],
  }));
}

function accessTargetRecords() {
  const records = [];
  const seen = new Set();
  const localTarget = accessLocalTargetModel();
  if (localTarget) {
    records.push(localTarget);
    seen.add(localTarget.host_id || localTarget.id);
  }
  for (const target of dashboardPeerAccessTargets()) {
    const id = target.host_id || target.id || '';
    if (!id || seen.has(id)) continue;
    records.push(target);
    seen.add(id);
  }
  for (const target of accessFleetTargets()) {
    const id = target.host_id || target.id || '';
    if (!id || seen.has(id)) continue;
    records.push({
      ...target,
      source: 'browser_fleet',
      connected: false,
      route_label: target.route_label || 'Remembered route',
      auth_label: target.auth_label || 'Browser-local fleet record',
      effective_role_label: target.effective_role_label || 'Unknown',
    });
    seen.add(id);
  }
  return records;
}

function accessPlural(count, singular, plural = `${singular}s`) {
  return `${count} ${count === 1 ? singular : plural}`;
}

function accessIamModel(overview = accessOverviewModel()) {
  return overview?.iam && typeof overview.iam === 'object' ? overview.iam : {};
}

function accessIamLoadLabel(iam) {
  const status = accessModelLabel(iam.load_status || iam.loadStatus, 'missing');
  return {
    loaded: 'Loaded',
    missing: 'Empty',
    error: 'Error',
    derived: 'Derived',
  }[status] || status;
}

function accessIamEnforcementReason(iam) {
  const enforcement = iam.enforcement && typeof iam.enforcement === 'object' ? iam.enforcement : {};
  return accessModelLabel(enforcement.reason, 'Active scoped user/client grants are enforced when requests bind to browser mTLS or Connect account identities.');
}

function accessFallbackIamRoles() {
  const rootPermissions = ['presence.read', 'stats.read', 'display.view', 'display.input', 'message.send', 'task.run', 'approval.resolve', 'access.inspect', 'access.manage', 'peer.inspect', 'peer.manage', 'peer.use', 'session.inspect', 'session.manage', 'terminal.view', 'terminal.write', 'shell.spawn', 'settings.manage', 'credentials.manage', 'runtime.control', 'filesystem.read', 'filesystem.write'];
  return [{
    id: 'role:root',
    label: 'Root',
    status: 'enforced',
    summary: 'Current owner/root dashboard authority.',
    permissions: rootPermissions,
    source: 'builtin',
  }, {
    id: 'role:peer-profile',
    label: 'Peer profile',
    status: 'enforced',
    summary: 'Daemon-to-daemon grants enforced by the approved peer identity profile.',
    permissions: ['presence.read', 'stats.read', 'display.view', 'display.input', 'message.send', 'task.run', 'approval.resolve', 'access.inspect', 'peer.inspect', 'peer.manage', 'session.inspect', 'session.manage', 'terminal.view', 'terminal.write', 'shell.spawn', 'settings.manage', 'runtime.control', 'filesystem.read', 'filesystem.write'],
    source: 'builtin',
  }, {
    id: 'role:none',
    label: 'No access',
    status: 'enforced',
    summary: 'Ceiling-only sentinel with no permissions: used in role_ceilings to refuse a binding kind (e.g. hosted-origin control) entirely. Never assigned to a principal.',
    permissions: [],
    source: 'builtin',
  }, {
    id: 'role:scoped-human',
    label: 'Scoped human',
    status: 'enforced',
    summary: 'Minimal user/client IAM role for stable browser mTLS and Connect account request bindings.',
    permissions: ['access.inspect'],
    source: 'builtin',
  }, {
    id: 'role:observer',
    label: 'Observer',
    status: 'enforced',
    summary: 'Read-only dashboard visibility without files, terminal, task control, or settings.',
    permissions: ['presence.read', 'stats.read', 'display.view', 'access.inspect', 'peer.inspect', 'session.inspect'],
    source: 'builtin',
  }, {
    id: 'role:session-reader',
    label: 'Session reader',
    status: 'enforced',
    summary: 'Read sessions, logs, reports, and status without controlling the daemon.',
    permissions: ['presence.read', 'stats.read', 'access.inspect', 'session.inspect'],
    source: 'builtin',
  }, {
    id: 'role:terminal',
    label: 'Terminal',
    status: 'enforced',
    summary: 'Collaborate in shared shell sessions (view and type) without spawning new shells or broader dashboard mutation rights.',
    permissions: ['presence.read', 'stats.read', 'access.inspect', 'session.inspect', 'terminal.view', 'terminal.write'],
    source: 'builtin',
  }, {
    id: 'role:files-read',
    label: 'Files read',
    status: 'enforced',
    summary: 'Browse metadata and download files without writing to disk.',
    permissions: ['presence.read', 'stats.read', 'access.inspect', 'filesystem.read'],
    source: 'builtin',
  }, {
    id: 'role:files-write',
    label: 'Files write',
    status: 'enforced',
    summary: 'Read files and upload/create file content through the dashboard.',
    permissions: ['presence.read', 'stats.read', 'access.inspect', 'filesystem.read', 'filesystem.write'],
    source: 'builtin',
  }, {
    id: 'role:peer-user',
    label: 'Peer user',
    status: 'enforced',
    summary: "Reach connected peers through this daemon (peer files, terminal, display tunnels); what each tunnel may do is decided by that peer's grants for this daemon. Combine with local roles as needed.",
    permissions: ['presence.read', 'stats.read', 'access.inspect', 'peer.inspect', 'peer.use'],
    source: 'builtin',
  }, {
    id: 'role:operator',
    label: 'Operator',
    status: 'enforced',
    summary: 'Operate sessions, display, shell, files, peers, and approvals without access/settings administration.',
    permissions: ['presence.read', 'stats.read', 'display.view', 'display.input', 'message.send', 'task.run', 'approval.resolve', 'access.inspect', 'peer.inspect', 'peer.use', 'session.inspect', 'session.manage', 'terminal.view', 'terminal.write', 'shell.spawn', 'credentials.manage', 'filesystem.read', 'filesystem.write'],
    source: 'builtin',
  }];
}

function accessFallbackPolicyIdForRole(roleId) {
  return {
    'role:root': 'policy:root',
    'role:peer-profile': 'policy:peer-profile',
    'role:scoped-human': 'policy:scoped-human',
    'role:observer': 'policy:observer',
    'role:session-reader': 'policy:session-reader',
    'role:terminal': 'policy:terminal',
    'role:files-read': 'policy:files-read',
    'role:files-write': 'policy:files-write',
    'role:peer-user': 'policy:peer-user',
    'role:operator': 'policy:operator',
  }[roleId] || `policy:${String(roleId || 'role').replace(/[^a-zA-Z0-9]+/g, '-').replace(/^-|-$/g, '').toLowerCase()}`;
}

function accessFallbackPolicies() {
  return accessFallbackIamRoles().map(role => ({
    id: accessFallbackPolicyIdForRole(role.id),
    label: role.label,
    status: role.status,
    summary: role.summary,
    role_id: role.id,
    permissions: role.permissions,
    source: role.source,
    assignment: role.id === 'role:peer-profile' ? 'daemon_peer_only' : (role.status === 'planned' ? 'planned' : 'user_client'),
  })).concat([{
    id: 'policy:public-share',
    label: 'Public share',
    status: 'planned',
    summary: 'Future explicit grants for publishing selected stats or artifacts.',
    permissions: [],
    source: 'builtin',
    assignment: 'planned',
  }]);
}

function accessFallbackPermissions() {
  const summaries = {
    'presence.read': ['Presence read', 'presence', 'Read live presence and basic daemon availability.'],
    'stats.read': ['Stats read', 'stats', 'Read daemon health, usage, and status summaries.'],
    'display.view': ['Display view', 'display', 'View display streams without injecting input.'],
    'display.input': ['Display input', 'display', 'Inject keyboard, pointer, or display-control input.'],
    'message.send': ['Message send', 'message', 'Send user messages or dashboard actions into a session.'],
    'task.run': ['Task run', 'task', 'Start or delegate agent tasks.'],
    'approval.resolve': ['Approval resolve', 'approval', 'Approve or deny pending supervised actions.'],
    'access.inspect': ['Access inspect', 'access', 'Read targets, principals, grants, policies, transports, and access architecture notes.'],
    'access.manage': ['Access manage', 'access', 'Approve, revoke, or change access grants.'],
    'peer.inspect': ['Peer inspect', 'peer', 'Read configured peer routes and peer eligibility.'],
    'peer.manage': ['Peer manage', 'peer', 'Administer peer relationships: create, remove, and pair daemon peer routes.'],
    'peer.use': ['Peer use', 'peer', "Act through connected peers (files, terminal, display tunnels, quick controls); the receiving peer's own grants bound each tunnel."],
    'session.inspect': ['Session inspect', 'session', 'Read session lists, logs, reports, recordings, and replay metadata.'],
    'session.manage': ['Session manage', 'session', 'Delete, rewind, prune, upload to, or otherwise mutate sessions.'],
    'terminal.use': ['Terminal (legacy)', 'terminal', 'Legacy aggregate: implies terminal.view, terminal.write, and shell.spawn.'],
    'terminal.view': ['Terminal view', 'terminal', 'Attach to shared shell sessions read-only (scrollback and live output).'],
    'terminal.write': ['Terminal write', 'terminal', 'Type into, resize, and close shell sessions you can see.'],
    'shell.spawn': ['Shell spawn', 'terminal', 'Create new shell sessions on this daemon.'],
    'settings.manage': ['Settings manage', 'settings', 'Read or write daemon settings and API keys.'],
    'credentials.manage': ['Credentials manage', 'credentials', 'Grant, renew, revoke, and inspect credential leases; register client egress.'],
    'runtime.control': ['Runtime control', 'runtime', 'Use runtime-control surfaces such as media and recording controls.'],
    'filesystem.read': ['Filesystem read', 'filesystem', 'Stat, list, and read files through dashboard APIs.'],
    'filesystem.write': ['Filesystem write', 'filesystem', 'Create directories or write uploaded file content.'],
  };
  return Object.entries(summaries).map(([id, [label, domain, summary]]) => ({
    id,
    label,
    domain,
    status: 'enforced',
    summary,
  }));
}

function accessOverviewModel() {
  if (dashboardAccessOverview) return dashboardAccessOverview;
  const targets = accessTargetRecords();
  const peerTargets = targets.filter(t => !t.local);
  const targetId = target => String(target?.host_id || target?.id || '').trim();
  const localTarget = targets.find(t => t.local) || {};
  const localTargetId = targetId(localTarget) || selfPeerId || SHELL_HOST_ID || 'local';
  const principals = [{
    id: 'principal:current-browser-session',
    kind: 'browser_session',
    kind_label: 'Current browser session',
    label: 'Current browser',
    source: 'trusted_dashboard_session',
    local: true,
    account: null,
    organization: null,
    authn: [{
      kind: 'trusted_dashboard_session',
      label: 'Trusted dashboard session',
    }],
  }, ...peerTargets.map(target => ({
    id: `principal:peer-daemon:${targetId(target)}`,
    kind: 'peer_daemon',
    kind_label: 'Peer daemon',
    label: target.label || target.host_id || target.id || 'Peer daemon',
    source: 'peer_registry',
    target_id: targetId(target),
    local: false,
    account: null,
    organization: null,
    authn: [{
      kind: 'daemon_mutual_tls',
      label: 'Daemon mTLS identity',
    }],
  }))];
  const grants = [{
    id: `grant:current-browser:${localTargetId}:root`,
    principal_id: 'principal:current-browser-session',
    target_id: localTargetId,
    kind: 'user_client_root',
    kind_label: 'User/client root',
    policy_id: 'policy:root',
    role: 'root',
    role_label: 'Root',
    transport_id: 'transport:current-dashboard',
    source: 'trusted_dashboard_session',
    status: 'active',
  }, ...peerTargets.map(target => ({
    id: `grant:peer-route:${targetId(target)}:profile`,
    principal_id: `principal:peer-daemon:${targetId(target)}`,
    target_id: targetId(target),
    kind: 'daemon_peer_profile',
    kind_label: 'Daemon peer profile',
    policy_id: 'policy:peer-profile',
    role: 'peer_profile',
    role_label: 'Peer profile',
    transport_id: `transport:peer-route:${targetId(target)}`,
    source: 'peer_registry',
    status: target.connected === false ? 'offline' : 'active',
  }))];
  const transports = [{
    id: 'transport:current-dashboard',
    kind: 'current_dashboard',
    kind_label: 'Current dashboard transport',
    label: 'Current dashboard',
    status: 'connected',
    implementation: 'local_mtls_or_hosted_tunnel',
    target_id: localTargetId,
  }, ...peerTargets.map(target => ({
    id: `transport:peer-route:${targetId(target)}`,
    kind: 'peer_route',
    kind_label: 'Peer route',
    label: target.label || target.host_id || target.id || 'Peer daemon',
    status: target.connected === false ? 'offline' : 'connected',
    implementation: 'daemon_mutual_tls_plus_optional_browser_datachannel',
    target_id: targetId(target),
  }))];
  return {
    scope: {
      kind: 'local_daemon',
      label: selfHostLabel || 'This daemon',
      target_id: localTargetId,
      account: null,
      organization: null,
      hosted_account_configured: false,
    },
    targets,
    principals,
    grants,
    transports,
    policies: accessFallbackPolicies(),
    permissions: accessFallbackPermissions(),
    architecture: {
      unresolved: [
        'external identity provider and Sybil-resistance policy',
        'organization ownership, billing, and recovery semantics',
        'final IAM policy language and editing UX',
      ],
    },
    iam: {
      schema_version: 1,
      load_status: 'derived',
      state_path: '',
      managed_principals: 0,
      managed_grants: 0,
      roles: accessFallbackIamRoles(),
      audit_events: [],
      capabilities: {
        state_file_supported: true,
        read_local_state: true,
        write_api_available: true,
        operation_evaluator: true,
        enforce_root_and_peer_grants: true,
        enforce_user_client_grants: true,
      },
      enforcement: {
        root_session_grants: true,
        peer_profile_grants: true,
        user_client_grants: true,
        principal_binding: 'root_peer_and_local_user_client',
        enforced_principal_kinds: ['root_session', 'peer_daemon', 'human_user', 'browser_certificate', 'client_key', 'connect_account'],
        reason: 'The daemon enforces trusted owner/root dashboard sessions, daemon peer profiles, and active local IAM user/client grants when requests bind to browser identity keys, browser mTLS certificates, or Connect account identities.',
      },
      role_ceilings: {
        connect_account: 'role:operator',
        client_key: 'role:operator',
      },
      hosted_origins: ['https://connect.intendant.dev'],
    },
  };
}

function accessTargetConnection(target, descriptor, summary) {
  if (descriptor.local) {
    const status = dashboardTransport?.status
      ? dashboardTransport.status()
      : { enabled: dashboardControlTransportEnabled(), connected: false };
    const transport = dashboardTransportStatusSummary(status);
    const connected = !status.enabled || Boolean(status.connected && status.verifiedBinding?.ok);
    return {
      state: transport.kind === 'err' ? 'err' : (connected ? 'ok' : 'checking'),
      route: descriptor.routeType || target.route_label || 'Current dashboard',
      detail: connected ? 'Connected' : (transport.label || 'Connecting'),
      title: transport.title || summary.title || '',
    };
  }

  const peerId = descriptor.hostId;
  const peer = descriptor.peer;
  const online = Boolean(peer && peer.connected !== false && target.connected !== false);
  const conn = peerDashboardControlConnectionsByHost.get(peerId);
  const direct = Boolean(conn?.canUseRpc?.());
  const signalReady = online && peerDashboardControlSignalAvailable(peerId);
  const label = direct
    ? 'Direct browser channel'
    : (signalReady ? 'Ready on demand' : (online ? 'Connected peer' : 'Offline'));
  return {
    state: online ? (direct ? 'ok' : 'checking') : 'err',
    route: descriptor.routeType || target.route_label || 'Peer route',
    detail: label,
    title: summary.title || target.url || peerId,
  };
}

function accessTargetSubtitle(target, descriptor) {
  const peerUrl = target.url || target.browser_tcp_via_url || target.ws_url || descriptor.peer?.url || descriptor.peer?.ws_url || '';
  if (peerUrl) return peerUrl;
  return descriptor.targetId || descriptor.hostId || '';
}

function accessAppendText(parent, className, text, tagName = 'div') {
  const node = document.createElement(tagName);
  node.className = className;
  node.textContent = text || '';
  parent.appendChild(node);
  return node;
}

function accessCreateTargetCell(label, value, detail) {
  const cell = document.createElement('div');
  cell.className = 'access-target-cell';
  accessAppendText(cell, 'access-target-label', label);
  accessAppendText(cell, 'access-target-value', value);
  accessAppendText(cell, 'access-target-detail', detail);
  return cell;
}

function accessCreateTargetButton(label, onClick, options = {}) {
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = label;
  if (options.primary) button.classList.add('primary');
  if (options.danger) button.classList.add('danger');
  if (options.title) button.title = options.title;
  if (options.disabled) button.disabled = true;
  button.addEventListener('click', onClick);
  return button;
}

function accessCreateTargetRow(target) {
  const hostId = target.local ? '' : String(target.host_id || target.id || '').trim();
  const descriptor = dashboardTargetDescriptor(hostId);
  const summary = dashboardTargetSummary(hostId, 'files');
  const connection = accessTargetConnection(target, descriptor, summary);
  const rememberedOnly = target.source === 'browser_fleet' && !descriptor.local && !descriptor.peer;
  const state = connection.state || summary.state || 'checking';
  const row = document.createElement('div');
  row.className = `access-target-row ${state}`;
  row.title = connection.title || summary.title || '';

  const main = document.createElement('div');
  main.className = 'access-target-main';
  const dot = document.createElement('span');
  dot.className = 'access-target-dot';
  dot.setAttribute('aria-hidden', 'true');
  const titleRow = document.createElement('div');
  titleRow.className = 'access-target-title-row';
  accessAppendText(titleRow, 'access-target-title', accessTargetDisplayName(target, descriptor));
  if (descriptor.local) {
    accessAppendText(titleRow, 'access-target-badge', 'this daemon', 'span');
  } else if (rememberedOnly) {
    accessAppendText(titleRow, 'access-target-badge', 'browser record', 'span');
  }
  const provenanceChip = accessTargetProvenanceChip(target);
  if (provenanceChip) titleRow.appendChild(provenanceChip);
  const subtitle = accessAppendText(main, 'access-target-subtitle', accessTargetSubtitle(target, descriptor));
  subtitle.title = subtitle.textContent;
  main.prepend(dot, titleRow);

  const authLabel = target.auth_label || descriptor.routeDetail || 'Trusted session';
  const roleLabel = descriptor.accessRole || target.effective_role_label || 'Role';
  const roleId = /peer/i.test(roleLabel) ? 'role:peer-profile' : (/root/i.test(roleLabel) ? 'role:root' : '');
  const accessCell = document.createElement('div');
  accessCell.className = 'access-target-cell';
  accessAppendText(accessCell, 'access-target-label', 'Your role');
  const roleValue = document.createElement('div');
  roleValue.className = 'access-target-value';
  roleValue.appendChild(accessRoleBadge(roleId, roleLabel));
  accessCell.appendChild(roleValue);
  accessAppendText(accessCell, 'access-target-detail', authLabel);

  const routeInfo = accessRouteInfoForTarget(target, descriptor, rememberedOnly);
  const routeCell = document.createElement('div');
  routeCell.className = 'access-target-cell';
  accessAppendText(routeCell, 'access-target-label', 'Route');
  const routeValue = document.createElement('div');
  routeValue.className = 'access-target-value';
  routeValue.appendChild(accessRouteChip(routeInfo.kind, routeInfo.label));
  routeCell.appendChild(routeValue);
  accessAppendText(routeCell, 'access-target-detail',
    rememberedOnly ? 'Not configured on this daemon' : (connection.detail || summary.detail || ''));

  const chips = document.createElement('div');
  chips.className = 'access-target-chips';
  for (const chip of dashboardTargetFeatureChips(hostId, 'files')) {
    const item = document.createElement('span');
    item.className = `target-chip ${chip.state || 'off'}`;
    item.textContent = chip.label || '';
    chips.appendChild(item);
  }

  const actions = document.createElement('div');
  actions.className = 'access-target-actions';
  const disabled = rememberedOnly || (!descriptor.local && state === 'err');
  actions.append(
    accessCreateTargetButton('Stats', () => accessOpenStats(hostId), { disabled }),
    accessCreateTargetButton('Files', () => accessOpenFiles(hostId), { disabled }),
    accessCreateTargetButton('Shell', () => accessOpenShell(hostId), { primary: true, disabled })
  );
  if (!descriptor.local && descriptor.peer && peerCanShareDisplay(descriptor.peer)) {
    actions.appendChild(accessCreateTargetButton('Display', () => accessOpenDisplay(hostId), { disabled }));
  }
  actions.appendChild(accessCreateTargetButton(
    descriptor.local ? 'Grant peer' : 'Manage',
    () => accessManageTargetAccess(hostId),
    { title: descriptor.local ? 'Open peer grant workflows' : 'Open peer access workflows' }
  ));
  if (!descriptor.local) {
    actions.appendChild(accessCreateTargetButton(
      'Remove',
      () => accessRemoveTarget(hostId, descriptor.displayName || target.label || hostId),
      { danger: true }
    ));
  }

  row.append(main, accessCell, routeCell, chips, actions);
  if (!descriptor.local) {
    const displaySlot = document.createElement('div');
    displaySlot.id = `peer-display-${hostId}`;
    displaySlot.className = 'peer-display-container access-peer-display-slot';
    displaySlot.style.display = 'none';
    row.appendChild(displaySlot);
  }
  return row;
}

function renderAccessTargetsSurface() {
  const el = document.getElementById('access-target-overview');
  if (!el) return;
  el.innerHTML = '';
  const targets = accessTargetRecords();
  if (!targets.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'No access targets';
    el.appendChild(empty);
    return;
  }
  for (const target of targets) {
    el.appendChild(accessCreateTargetRow(target));
  }
  reapplyPeerDisplayPanes();
}

function accessModelStatusText(value) {
  return String(value || 'active').trim().toLowerCase().replace(/_/g, '-') || 'active';
}

function accessModelLabel(value, fallback = '') {
  return String(value || fallback || '').trim();
}

function accessOverviewTargetLabelMap(overview) {
  const map = new Map();
  // First writer wins on BOTH keys. The daemon emits the local target as
  // the first targets[] row, and a manually added same-host peer can carry
  // the very same id/host_id (PeerId is `intendant:<host label>`, and host
  // labels collide when two daemons share a hostname). Letting a later peer
  // row overwrite the key relabelled every local-daemon grant with the
  // peer's name — "Root on qa-peer-b" for a grant on this daemon
  // (design-overhaul QA fleet, access finding FR-3).
  for (const target of Array.isArray(overview.targets) ? overview.targets : []) {
    const id = accessModelLabel(target.id || target.host_id);
    if (id && !map.has(id)) map.set(id, accessModelLabel(target.label, id));
    const hostId = accessModelLabel(target.host_id);
    if (hostId && !map.has(hostId)) map.set(hostId, accessModelLabel(target.label, hostId));
  }
  return map;
}

function accessOverviewPrincipalLabelMap(overview) {
  const map = new Map();
  for (const principal of Array.isArray(overview.principals) ? overview.principals : []) {
    const id = accessModelLabel(principal.id);
    if (id) map.set(id, accessModelLabel(principal.label, id));
  }
  return map;
}

function accessModelCreateStatus(status) {
  const value = accessModelStatusText(status);
  const node = document.createElement('span');
  node.className = `access-model-status ${value}`;
  node.textContent = value;
  return node;
}

function accessModelCreateRow({ main, meta, detail, status, title }) {
  const row = document.createElement('div');
  row.className = 'access-model-row';
  if (title) row.title = title;
  const mainEl = document.createElement('div');
  mainEl.className = 'access-model-main';
  mainEl.textContent = main || '';
  const metaEl = document.createElement('div');
  metaEl.className = 'access-model-meta';
  metaEl.textContent = meta || '';
  const detailEl = document.createElement('div');
  detailEl.className = 'access-model-detail';
  detailEl.textContent = detail || '';
  row.append(mainEl, metaEl, detailEl, accessModelCreateStatus(status));
  return row;
}

function accessModelCreateSection(title, rows) {
  const section = document.createElement('div');
  section.className = 'access-model-section';
  const head = document.createElement('div');
  head.className = 'access-model-section-head';
  const titleEl = document.createElement('div');
  titleEl.className = 'access-model-section-title';
  titleEl.textContent = title || 'Access';
  const count = document.createElement('div');
  count.className = 'access-model-section-count';
  count.textContent = accessPlural(rows.length, 'item');
  head.append(titleEl, count);
  const table = document.createElement('div');
  table.className = 'access-model-table';
  if (!rows.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'None';
    table.appendChild(empty);
  } else {
    for (const row of rows) table.appendChild(accessModelCreateRow(row));
  }
  section.append(head, table);
  return section;
}

function accessPrincipalRow(principal) {
  const authn = Array.isArray(principal.authn) ? principal.authn : [];
  const authLabel = authn
    .map(item => accessModelLabel(item.label || item.kind))
    .filter(Boolean)
    .join(', ');
  return {
    main: accessModelLabel(principal.label, principal.id || 'Principal'),
    meta: accessModelLabel(principal.kind_label || principal.kind, 'Principal'),
    detail: authLabel || accessModelLabel(principal.source, principal.id),
    status: principal.status || (principal.local ? 'current' : 'active'),
    title: accessModelLabel(principal.id),
  };
}

function accessPrincipalKindRow(kind) {
  return {
    main: accessModelLabel(kind.label, kind.kind || 'Principal type'),
    meta: accessModelLabel(kind.kind, 'Principal type'),
    detail: accessModelLabel(kind.summary),
    status: kind.status || 'planned',
    title: accessModelLabel(kind.kind),
  };
}

function accessGrantRow(grant, principalLabels, targetLabels) {
  const principal = principalLabels.get(accessModelLabel(grant.principal_id))
    || accessModelLabel(grant.principal_id, 'Principal');
  const target = targetLabels.get(accessModelLabel(grant.target_id))
    || accessModelLabel(grant.target_id, 'Target');
  const role = accessModelLabel(grant.role_label || grant.profile || grant.role, 'Role');
  return {
    main: principal,
    meta: `${role} -> ${target}`,
    detail: accessModelLabel(grant.kind_label || grant.source || grant.policy_id, grant.id),
    status: grant.status || 'active',
    title: accessModelLabel(grant.id),
  };
}

function accessPolicyRow(policy) {
  return {
    main: accessModelLabel(policy.label, policy.id || 'Policy'),
    meta: accessModelLabel(policy.id, 'Policy'),
    detail: accessModelLabel(policy.summary),
    status: policy.status || 'planned',
    title: accessModelLabel(policy.id),
  };
}

function accessPermissionRow(permission) {
  const domain = accessModelLabel(permission.domain, 'permission');
  return {
    main: accessModelLabel(permission.label, permission.id || 'Permission'),
    meta: `${domain} - ${accessModelLabel(permission.id, 'permission')}`,
    detail: accessModelLabel(permission.summary),
    status: permission.status || 'enforced',
    title: accessModelLabel(permission.id),
  };
}

function accessIamRoleRow(role) {
  const permissions = Array.isArray(role.permissions) ? role.permissions.filter(Boolean) : [];
  return {
    main: accessModelLabel(role.label, role.id || 'IAM role'),
    meta: accessModelLabel(role.id, 'IAM role'),
    detail: permissions.length ? permissions.join(', ') : accessModelLabel(role.summary),
    status: role.status || 'planned',
    title: accessModelLabel(role.summary || role.id),
  };
}

function accessIamAuditRow(event) {
  return {
    main: accessModelLabel(event.action, 'IAM event'),
    meta: accessModelLabel(event.actor_principal_id, event.id || 'actor'),
    detail: accessModelLabel(event.summary || event.target_id),
    status: event.status || 'active',
    title: accessModelLabel(event.id),
  };
}

function accessTransportRow(transport, targetLabels) {
  const target = targetLabels.get(accessModelLabel(transport.target_id))
    || accessModelLabel(transport.target_id, 'Target');
  return {
    main: accessModelLabel(transport.label, transport.id || 'Transport'),
    meta: accessModelLabel(transport.kind_label || transport.kind, 'Transport'),
    detail: `${target} - ${accessModelLabel(transport.implementation, transport.id)}`,
    status: transport.status || 'connected',
    title: accessModelLabel(transport.id),
  };
}

function accessGrantsBy(overview, key) {
  const map = new Map();
  for (const grant of accessOverviewArray(overview, 'grants')) {
    const id = accessModelLabel(grant[key]);
    if (!id) continue;
    if (!map.has(id)) map.set(id, []);
    map.get(id).push(grant);
  }
  return map;
}

function accessGrantWhy(grant) {
  const kind = accessModelLabel(grant.kind);
  const source = accessModelLabel(grant.source);
  if (kind === 'user_client_root') {
    return 'This browser is trusted as the owner/root client for this daemon. Connect and browser mTLS are transports for that authority.';
  }
  if (kind === 'daemon_peer_profile') {
    return 'This outbound daemon peer route was configured on this daemon. It gives this daemon peer-profile authority on the target daemon.';
  }
  if (kind === 'inbound_daemon_peer_profile') {
    return 'This inbound daemon mTLS identity was approved for this daemon. The peer profile bounds what the remote daemon may do here.';
  }
  if (kind === 'user_client_local_iam') {
    return 'This local IAM grant is enforced when a request binds to its browser certificate or Connect account metadata.';
  }
  return source ? `Authority comes from ${source}.` : 'Authority is described by this grant.';
}

function accessPeerTrustDetailCards() {
  const overview = accessOverviewModel();
  const principalLabels = accessOverviewPrincipalLabelMap(overview);
  const targetLabels = accessOverviewTargetLabelMap(overview);
  return accessOverviewArray(overview, 'grants')
    .filter(grant => /peer_profile/.test(accessModelLabel(grant.kind)))
    .map(grant => ({
      title: accessModelLabel(grant.role_label || grant.profile || grant.role, 'Peer profile'),
      status: grant.status || 'active',
      lines: [
        ['Principal', principalLabels.get(accessModelLabel(grant.principal_id)) || accessModelLabel(grant.principal_id)],
        ['Target', targetLabels.get(accessModelLabel(grant.target_id)) || accessModelLabel(grant.target_id)],
        ['Direction', accessModelLabel(grant.kind) === 'inbound_daemon_peer_profile' ? 'Inbound to this daemon' : 'Outbound from this daemon'],
        ['Source', accessModelLabel(grant.source, 'Peer registry')],
        ['Why access exists', accessGrantWhy(grant)],
        ['Grant id', accessModelLabel(grant.id)],
      ],
    }));
}

function accessAuditGrantCards() {
  const overview = accessOverviewModel();
  const principalLabels = accessOverviewPrincipalLabelMap(overview);
  const targetLabels = accessOverviewTargetLabelMap(overview);
  return accessOverviewArray(overview, 'grants').map(grant => {
    const grantId = accessModelLabel(grant.id);
    const status = accessModelLabel(grant.status, 'active');
    const localIamGrant = accessModelLabel(grant.kind) === 'user_client_local_iam';
    const lifecycleBusy = accessGrantLifecycleSubmitting.has(grantId);
    // daemonApi availability (transport F4): tunnel status boolean when
    // connected, HTTP-twin reachability otherwise — honest in Connect
    // mode, where a down tunnel means no lane at all.
    const canManage = daemonApi.availability('api_access_iam_update_grant').ok;
    const actions = [];
    if (localIamGrant && grantId) {
      if (status !== 'active') {
        actions.push({
          label: lifecycleBusy ? 'Saving' : 'Activate',
          primary: true,
          disabled: lifecycleBusy || !canManage,
          onClick: () => accessUpdateGrantLifecycle(grantId, { status: 'active' }),
        });
      }
      if (status !== 'draft') {
        actions.push({
          label: lifecycleBusy ? 'Saving' : 'Draft',
          disabled: lifecycleBusy || !canManage,
          onClick: () => accessUpdateGrantLifecycle(grantId, { status: 'draft' }),
        });
      }
      if (status !== 'revoked') {
        actions.push({
          label: lifecycleBusy ? 'Saving' : 'Revoke',
          danger: true,
          disabled: lifecycleBusy || !canManage,
          onClick: () => accessConfirmRevokeGrant(grantId),
        });
      }
    }
    return {
      title: accessModelLabel(grant.role_label || grant.profile || grant.role, 'Grant'),
      status: grant.status || 'active',
      lines: [
        ['Principal', principalLabels.get(accessModelLabel(grant.principal_id)) || accessModelLabel(grant.principal_id)],
        ['Target', targetLabels.get(accessModelLabel(grant.target_id)) || accessModelLabel(grant.target_id)],
        ['Kind', accessModelLabel(grant.kind_label || grant.kind)],
        ['Policy', accessModelLabel(grant.policy_id)],
        ['Transport', accessModelLabel(grant.transport_id)],
        ['Enforced', grant.enforced === false ? 'No' : 'Yes'],
        ['Reason', accessModelLabel(grant.reason)],
        ['Why access exists', accessGrantWhy(grant)],
        ['Grant id', grantId],
      ],
      actions,
    };
  });
}

function accessCurrentUserClientCandidate() {
  const principal = dashboardControlTransport?.lastStatus?.access_principal;
  const authnList = Array.isArray(principal?.authn) ? principal.authn : [];
  // This origin's identity key is the strongest, always-available binding:
  // prefer it whenever the keystore holds one (the cache is warmed at
  // startup, so this synchronous read is normally populated).
  const keyAuthn = authnList.find(item =>
    accessModelLabel(item?.kind) === 'client_key' && accessModelLabel(item?.fingerprint)
  );
  const localKey = clientIdentityCache;
  if (keyAuthn || localKey) {
    return {
      kind: 'client_key',
      label: accessModelLabel(principal?.label, 'This browser'),
      fingerprint: '',
      client_key_fingerprint: accessModelLabel(keyAuthn?.fingerprint, localKey?.fingerprint || ''),
      client_key: accessModelLabel(keyAuthn?.public_key, localKey?.publicRawB64u || ''),
      user_id: accessModelLabel(principal?.account?.user_id),
      account_name: accessModelLabel(principal?.account?.account_name || principal?.account?.handle),
      account_provider: accessModelLabel(principal?.account?.provider),
      verified_provider: accessModelLabel(principal?.account?.verified_provider),
      organization_id: accessModelLabel(principal?.organization?.id),
      organization_name: accessModelLabel(principal?.organization?.name),
    };
  }
  const browserAuthn = authnList.find(item =>
    accessModelLabel(item?.kind) === 'browser_mtls_cert' && accessModelLabel(item?.fingerprint)
  );
  if (browserAuthn) {
    const human = accessModelLabel(principal?.kind) === 'human_user';
    return {
      kind: human ? 'human_user' : 'browser_certificate',
      label: accessModelLabel(principal?.label, 'Current browser certificate'),
      fingerprint: accessModelLabel(browserAuthn.fingerprint),
      user_id: accessModelLabel(principal?.account?.user_id),
      account_name: accessModelLabel(principal?.account?.account_name || principal?.account?.handle),
      account_provider: accessModelLabel(principal?.account?.provider),
      verified_provider: accessModelLabel(principal?.account?.verified_provider),
      organization_id: accessModelLabel(principal?.organization?.id),
      organization_name: accessModelLabel(principal?.organization?.name),
    };
  }
  const connectAuthn = authnList.find(item =>
    accessModelLabel(item?.kind) === 'connect_account' &&
    (accessModelLabel(item?.user_id) || accessModelLabel(item?.account_name))
  );
  if (connectAuthn) {
    const accountName = accessModelLabel(connectAuthn.account_name || principal?.account?.account_name);
    const userId = accessModelLabel(connectAuthn.user_id || principal?.account?.user_id);
    return {
      kind: 'connect_account',
      label: accessModelLabel(principal?.label, accountName ? `@${accountName}` : 'Current Connect account'),
      fingerprint: '',
      user_id: userId,
      account_name: accountName,
      account_provider: accessModelLabel(principal?.account?.provider),
      verified_provider: accessModelLabel(principal?.account?.verified_provider),
      organization_id: accessModelLabel(principal?.organization?.id),
      organization_name: accessModelLabel(principal?.organization?.name),
    };
  }
  return null;
}

function accessSetField(parent, id, value) {
  const el = parent.querySelector(`#${id}`);
  if (!el) return;
  el.value = value == null ? '' : String(value);
  el.dataset.autofill = el.value;
}

function accessSelectedUserClientKind(form) {
  return form?.querySelector('input[name="access-user-client-kind"]:checked')?.value || 'browser_certificate';
}

function accessSyncUserClientGrantFields(form) {
  if (!form) return;
  const kind = accessSelectedUserClientKind(form);
  form.querySelectorAll('[data-access-kind-field]').forEach(node => {
    const visibleKinds = String(node.dataset.accessKindField || '')
      .split('|')
      .map(value => value.trim())
      .filter(Boolean);
    node.style.display = visibleKinds.includes(kind) ? '' : 'none';
  });
  form.querySelectorAll('.acc-choice-card').forEach(card => {
    const input = card.querySelector('input[name="access-user-client-kind"]');
    card.classList.toggle('selected', Boolean(input?.checked));
  });
  form.querySelectorAll('.acc-role-card').forEach(card => {
    const input = card.querySelector('input[name="access-user-client-role"]');
    const selected = Boolean(input?.checked);
    card.classList.toggle('selected', selected);
    card.classList.toggle('warn', selected && accessRoleMeta(input?.value).warn === true);
  });
  // Warn when the chosen role exceeds what the binding's ceiling will let a
  // session actually do (connect accounts always; client keys only matter
  // here when the key was enrolled from a hosted origin, which a manually
  // pasted fingerprint usually was not).
  const note = form.querySelector('#access-grant-ceiling-note');
  if (note) {
    const ceiling = kind === 'connect_account' ? accessRoleCeilingFor('connect_account') : null;
    const selectedRole = form.querySelector('input[name="access-user-client-role"]:checked')?.value || '';
    const ceilingRole = ceiling ? accessIamRoleById(ceiling) : null;
    const selectedRoleObj = selectedRole ? accessIamRoleById(selectedRole) : null;
    const exceeds = Boolean(ceiling && selectedRoleObj && ceilingRole
      && Array.isArray(selectedRoleObj.permissions) && Array.isArray(ceilingRole.permissions)
      && selectedRoleObj.permissions.some(perm => !ceilingRole.permissions.includes(perm)));
    note.style.display = exceeds ? '' : 'none';
    if (exceeds) {
      note.textContent = `Hosted-route sessions for this account are capped at ${ceiling.replace(/^role:/, '')} by this daemon's role ceiling — the grant saves as ${selectedRole.replace(/^role:/, '')}, but Connect sessions won't exceed the ceiling. The "Hosted tabs may" control on the Overview tab moves it.`;
    }
  }
}

function accessAssignableIamRoles(overview = accessOverviewModel()) {
  const iam = accessIamModel(overview);
  const roles = Array.isArray(iam.roles) ? iam.roles : [];
  const filtered = roles.filter(role => {
    const id = accessModelLabel(role?.id);
    const status = accessModelLabel(role?.status, 'enforced');
    if (!id || status === 'planned') return false;
    if (id === 'role:peer-profile') return false;
    // role:none is the ceiling-only sentinel; it is never granted.
    if (id === 'role:none') return false;
    return true;
  });
  if (filtered.length) return filtered;
  return [{
    id: 'role:scoped-human',
    label: 'Scoped human',
    status: 'enforced',
  }, {
    id: 'role:root',
    label: 'Root',
    status: 'enforced',
  }];
}

const ACCESS_GRANT_KIND_CHOICES = [{
  value: 'client_key',
  title: 'A browser identity key',
  desc: 'The key a browser holds in its own storage — signed into every session, no certificate install needed.',
}, {
  value: 'browser_certificate',
  title: 'A browser certificate',
  desc: 'Pin one browser’s mTLS client certificate to a role on this daemon.',
}, {
  value: 'connect_account',
  title: 'A Connect account',
  desc: 'Authorize an Intendant Connect account so its hosted dashboard can open this daemon.',
}, {
  value: 'human_user',
  title: 'A person',
  desc: 'One human record that can carry a key, a certificate, and a Connect account.',
}];

const ACCESS_ROLE_PICKER_ORDER = [
  'role:scoped-human', 'role:observer', 'role:session-reader', 'role:files-read',
  'role:files-write', 'role:terminal', 'role:peer-user', 'role:operator', 'role:root',
];

/* Grant fanout: the daemons this page can apply a grant to, each with the
   channel it would use. 'self' uses the normal session; 'remote' is a
   direct cross-origin call to a fleet daemon (mTLS in the browser, origin
   allowlisted daemon-side); 'connect-only' targets have no direct route
   from this page. Every write is still authorized by the target daemon. */
function accessGrantFanoutTargets() {
  const targets = [];
  for (const target of accessTargetRecords()) {
    const hostId = String(target.host_id || target.id || '').trim();
    const descriptor = dashboardTargetDescriptor(target.local ? '' : hostId);
    const label = accessTargetDisplayName(target, descriptor);
    if (target.local || descriptor.local) {
      targets.push({ key: 'local', label, mode: 'self' });
      continue;
    }
    const rawUrl = target.browser_tcp_via_url || target.url || target.ws_url
      || descriptor.peer?.browser_tcp_via_url || descriptor.peer?.url || descriptor.peer?.ws_url || '';
    let origin = '';
    try {
      if (rawUrl) {
        const url = new URL(rawUrl);
        const scheme = url.protocol === 'wss:' ? 'https:' : (url.protocol === 'ws:' ? 'http:' : url.protocol);
        origin = `${scheme}//${url.host}`;
      }
    } catch { origin = ''; }
    const hostedOnly = !origin || /\/app\?connect=1/.test(rawUrl);
    targets.push({
      key: hostId || label,
      label,
      mode: hostedOnly ? 'connect-only' : 'remote',
      origin: hostedOnly ? '' : origin,
      connected: target.connected !== false,
    });
  }
  return targets;
}

/* Results of the last fanout, kept across re-renders until dismissed. */
let accessGrantFanoutResults = null;

async function accessApplyGrantToTarget(target, payload) {
  if (target.mode === 'self') {
    // daemonApi (transport F4): POST twin — verb-derived no-replay, the
    // legacy fallbackAfterRpcFailure:false semantics. IAM-mutating
    // write: payload shape unchanged, no retries.
    const resp = await daemonApi.request('api_access_iam_upsert_user_client_grant', payload);
    if (!resp.ok) throw new Error(resp.body?.error || `request failed (${resp.status})`);
    return resp.body;
  }
  // Remote fleet daemon: the explicit remote-http target (transport F4).
  // Naming the target names the transport — direct cross-origin HTTP to
  // that daemon's fleet-CORS row, never a tunnel, never a fallback; the
  // target daemon's own IAM authorizes the write (browser mTLS identity).
  const resp = await daemonApi.request('api_access_iam_upsert_user_client_grant', payload, {
    target: { remoteHttp: target.origin },
  });
  if (!resp.ok) throw new Error(resp.body?.error || `request failed (${resp.status})`);
  return resp.body;
}

/* ── Shared render guard for the Access surface ──
   Background refreshes (transport ticks, peer events) re-run the whole
   17-renderer Access fanout, and each renderer rebuilds its mount with
   innerHTML — destroying whatever the user was mid-interaction with:
   the fleet strip's petname rename input, org trust/issue/renew fields,
   output textareas holding freshly signed documents, open selects.
   Generalized from this grant form's proven bespoke guard:

   - accessGuardStamp(mount) records, after a rebuild, the value every
     field was rendered with (dataset.accessGuardBase).
   - accessMountHoldsUserWork(mount) answers "would a rebuild destroy
     the user's work?": the focused element lives inside the mount, or
     any stamped field's value differs from its rendered baseline
     (inputs, selects, radios/checkboxes, textareas — including the
     readOnly outputs a signed document lands in). Fields created after
     the stamp (e.g. the inline rename input) are guarded by focus.
   - accessGuardedRender(mountId, fp, renderFn) applies both around one
     section's rebuild, plus a cheap state-fingerprint short-circuit
     (the enrollments refresher's pattern): when `fp` matches the last
     rendered fingerprint the section is provably unchanged and the
     rebuild is skipped outright. Pass null for time-driven sections
     that must repaint every tick. */
function accessGuardStamp(mount) {
  if (!mount) return;
  for (const el of mount.querySelectorAll('input, textarea, select')) {
    el.dataset.accessGuardBase = (el.type === 'checkbox' || el.type === 'radio')
      ? (el.checked ? '1' : '0')
      : el.value;
  }
}

function accessMountHoldsUserWork(mount) {
  if (!mount) return false;
  // Focus counts for FIELDS only (the vault section's own guard set the
  // precedent): a focused button must not block the rebuild its own click
  // handler asks for (e.g. Reveal claim phrase re-rendering to show it).
  const active = document.activeElement;
  if (active && mount.contains(active)
    && active.matches('input, textarea, select, [contenteditable]')) return true;
  for (const el of mount.querySelectorAll('input, textarea, select')) {
    const base = el.dataset.accessGuardBase;
    if (base === undefined) continue;
    const now = (el.type === 'checkbox' || el.type === 'radio')
      ? (el.checked ? '1' : '0')
      : el.value;
    if (now !== base) return true;
  }
  return false;
}

function accessGuardedRender(mountId, fp, renderFn) {
  const mount = document.getElementById(mountId);
  if (!mount) return;
  // Store a short digest, never the fingerprint itself: fp strings are
  // whole model slices (large, and some carry secrets like the claim
  // phrase) and must not land in a DOM attribute.
  const fpKey = fp === null ? null : accessFpHash(fp);
  if (fpKey !== null && mount.dataset.accessGuardFp === fpKey) return;
  if (accessMountHoldsUserWork(mount)) return;
  renderFn();
  accessGuardStamp(mount);
  if (fpKey !== null) mount.dataset.accessGuardFp = fpKey;
  else delete mount.dataset.accessGuardFp;
}

/* Fingerprint helper: JSON over the model slices a section reads. A
   stringify failure returns a unique value so the section still renders. */
function accessSectionFp(value) {
  try {
    return JSON.stringify(value) ?? '';
  } catch (_) {
    return `fp-err-${Date.now()}-${Math.random()}`;
  }
}

/* Two independent FNV-1a-style streams + length — cheap, and a collision
   only costs one deferred repaint (the next real state change renders). */
function accessFpHash(text) {
  let h1 = 0x811c9dc5;
  let h2 = 0x01234567;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    h1 = Math.imul(h1 ^ c, 0x01000193) >>> 0;
    h2 = Math.imul(h2 ^ c, 0x0100012d) >>> 0;
  }
  return `${text.length.toString(36)}.${h1.toString(36)}.${h2.toString(36)}`;
}

function renderAccessUserClientGrantForm() {
  const mount = document.getElementById('access-user-client-grant-form');
  if (!mount) return;
  // Background refreshes (peer events, transport status) re-render the whole
  // Access surface. Never clobber a grant the user is in the middle of
  // typing: skip the rebuild while the form is focused or holds edits (the
  // shared guard above — this form's old bespoke check is where it came
  // from). A submit cycle bypasses this via the submitting flag so button
  // state and the post-save reset still render.
  const existing = mount.querySelector('form');
  if (existing && !accessUserClientGrantSubmitting && existing.dataset.submitting !== 'true'
    && accessMountHoldsUserWork(mount)) {
    return;
  }
  const overview = accessOverviewModel();
  const iam = accessIamModel(overview);
  const writeApiAvailable = iam?.capabilities?.write_api_available !== false;
  // daemonApi availability (transport F4): folds the tunnel status
  // boolean and HTTP-twin reachability into one honest answer.
  const canSubmit = writeApiAvailable
    && daemonApi.availability('api_access_iam_upsert_user_client_grant').ok;
  const candidate = accessCurrentUserClientCandidate();
  mount.innerHTML = '';

  const form = document.createElement('form');
  form.className = 'acc-grant-flow';
  form.dataset.submitting = accessUserClientGrantSubmitting ? 'true' : 'false';
  form.addEventListener('submit', accessSubmitUserClientGrant);

  const head = document.createElement('div');
  head.className = 'acc-grant-flow-head';
  const title = document.createElement('div');
  title.className = 'acc-grant-flow-title';
  title.textContent = 'Grant access to this daemon';
  const sub = document.createElement('div');
  sub.className = 'acc-grant-flow-sub';
  sub.textContent = 'Bind an identity to a role. This daemon stores and enforces the grant locally.';
  const mode = document.createElement('div');
  mode.className = 'acc-grant-flow-mode';
  mode.textContent = canSubmit ? 'Manage' : 'Inspect only';
  head.append(title, sub, mode);
  form.appendChild(head);

  const stepLabel = text => {
    const label = document.createElement('div');
    label.className = 'acc-step-label';
    label.textContent = text;
    return label;
  };

  form.appendChild(stepLabel('1 · Who gets access'));
  const choiceRow = document.createElement('div');
  choiceRow.className = 'acc-choice-row';
  for (const choice of ACCESS_GRANT_KIND_CHOICES) {
    const card = document.createElement('label');
    card.className = 'acc-choice-card';
    const input = document.createElement('input');
    input.type = 'radio';
    input.name = 'access-user-client-kind';
    input.value = choice.value;
    input.addEventListener('change', () => accessSyncUserClientGrantFields(form));
    const t = document.createElement('div');
    t.className = 't';
    t.textContent = choice.title;
    const d = document.createElement('div');
    d.className = 'd';
    d.textContent = choice.desc;
    card.append(input, t, d);
    choiceRow.appendChild(card);
  }
  form.appendChild(choiceRow);
  if (candidate) {
    const detected = document.createElement('div');
    detected.className = 'acc-detected';
    detected.textContent = candidate.kind === 'client_key'
      ? `Detected: this browser’s identity key ${String(candidate.client_key_fingerprint || '').slice(0, 12)}… — fields are pre-filled`
      : (candidate.kind === 'connect_account'
        ? `Detected: your Connect account ${candidate.account_name ? `@${candidate.account_name}` : candidate.user_id} — fields are pre-filled`
        : (candidate.kind === 'human_user'
          ? 'Detected: your mTLS user — fields are pre-filled'
          : 'Detected: this browser’s certificate — fields are pre-filled'));
    form.appendChild(detected);
  }

  form.appendChild(stepLabel('2 · Identity details'));
  const grid = document.createElement('div');
  grid.className = 'acc-field-grid';
  const textFields = [{
    id: 'access-user-client-label',
    label: 'Label',
    placeholder: 'e.g. Alice’s laptop browser',
  }, {
    id: 'access-user-client-client-key-fingerprint',
    label: 'Browser key fingerprint',
    kind: 'client_key|human_user',
    placeholder: 'shown on the connecting browser’s Access page',
  }, {
    id: 'access-user-client-fingerprint',
    label: 'Certificate fingerprint',
    kind: 'browser_certificate|human_user',
    placeholder: 'sha256 fingerprint',
  }, {
    id: 'access-user-client-user-id',
    label: 'Connect user id',
    kind: 'connect_account|human_user',
    placeholder: 'from the Connect account page',
  }, {
    id: 'access-user-client-account-name',
    label: 'Connect handle',
    kind: 'connect_account|human_user',
    placeholder: 'name without the @',
  }];
  for (const field of textFields) {
    const label = document.createElement('label');
    label.textContent = field.label;
    if (field.kind) label.dataset.accessKindField = field.kind;
    const input = document.createElement('input');
    input.type = 'text';
    input.autocomplete = 'off';
    input.spellcheck = false;
    input.id = field.id;
    if (field.placeholder) input.placeholder = field.placeholder;
    label.appendChild(input);
    grid.appendChild(label);
  }
  form.appendChild(grid);

  const advanced = document.createElement('details');
  advanced.className = 'acc-fold';
  const advancedSummary = document.createElement('summary');
  advancedSummary.textContent = 'Advanced identity metadata';
  const advancedBody = document.createElement('div');
  advancedBody.className = 'acc-fold-body';
  const advancedGrid = document.createElement('div');
  advancedGrid.className = 'acc-field-grid';
  const advancedFields = [{
    id: 'access-user-client-account-provider',
    label: 'Account provider',
    kind: 'connect_account|human_user',
  }, {
    id: 'access-user-client-verified-provider',
    label: 'Verified by',
    kind: 'connect_account|human_user',
  }, {
    id: 'access-user-client-organization-id',
    label: 'Org id',
    kind: 'human_user',
  }, {
    id: 'access-user-client-organization-name',
    label: 'Org name',
    kind: 'human_user',
  }];
  for (const field of advancedFields) {
    const label = document.createElement('label');
    label.textContent = field.label;
    if (field.kind) label.dataset.accessKindField = field.kind;
    const input = document.createElement('input');
    input.type = 'text';
    input.autocomplete = 'off';
    input.spellcheck = false;
    input.id = field.id;
    label.appendChild(input);
    advancedGrid.appendChild(label);
  }
  advancedBody.appendChild(advancedGrid);
  advanced.append(advancedSummary, advancedBody);
  form.appendChild(advanced);

  form.appendChild(stepLabel('3 · Role'));
  const picker = document.createElement('div');
  picker.className = 'acc-role-picker';
  const assignable = accessAssignableIamRoles(overview);
  const rank = id => {
    const index = ACCESS_ROLE_PICKER_ORDER.indexOf(id);
    return index === -1 ? ACCESS_ROLE_PICKER_ORDER.length : index;
  };
  const sorted = assignable.slice().sort((a, b) => rank(accessModelLabel(a.id)) - rank(accessModelLabel(b.id)));
  for (const role of sorted) {
    const roleId = accessModelLabel(role.id);
    const meta = accessRoleMeta(roleId);
    const card = document.createElement('label');
    card.className = 'acc-role-card';
    const input = document.createElement('input');
    input.type = 'radio';
    input.name = 'access-user-client-role';
    input.value = roleId;
    input.addEventListener('change', () => accessSyncUserClientGrantFields(form));
    const top = document.createElement('div');
    top.className = 'acc-role-card-top';
    const t = document.createElement('div');
    t.className = 't';
    t.textContent = accessModelLabel(role.label, roleId.replace(/^role:/, ''));
    top.appendChild(t);
    const permCount = Array.isArray(role.permissions)
      ? role.permissions.length
      : accessIamRoleById(roleId)?.permissions?.length;
    top.appendChild(accessRoleBadge(roleId, meta.warn ? 'full control' : (permCount ? accessPlural(permCount, 'perm') : 'scoped')));
    const d = document.createElement('div');
    d.className = 'd';
    d.textContent = accessModelLabel(role.summary, meta.short);
    card.append(input, top, d);
    picker.appendChild(card);
  }
  form.appendChild(picker);

  form.appendChild(stepLabel('4 · Apply to'));
  const fanoutRow = document.createElement('div');
  fanoutRow.className = 'acc-choice-row';
  for (const target of accessGrantFanoutTargets()) {
    const card = document.createElement('label');
    card.className = 'acc-choice-card';
    const input = document.createElement('input');
    input.type = 'checkbox';
    input.name = 'access-grant-fanout';
    input.value = target.key;
    if (target.mode === 'self') input.defaultChecked = true;
    if (target.mode === 'connect-only' || (target.mode === 'remote' && target.connected === false)) {
      input.disabled = true;
    }
    input.addEventListener('change', () => card.classList.toggle('selected', input.checked));
    const t = document.createElement('div');
    t.className = 't';
    t.textContent = target.label + (target.mode === 'self' ? ' (this daemon)' : '');
    const d = document.createElement('div');
    d.className = 'd';
    d.textContent = target.mode === 'self'
      ? 'Applied through this session.'
      : (target.mode === 'remote'
        ? (target.connected === false
          ? `Offline — ${target.origin}`
          : `Direct call to ${target.origin}; that daemon authorizes it independently.`)
        : 'No direct route from this page — open it via Connect once and approve the key there.');
    card.classList.toggle('selected', input.checked);
    card.append(input, t, d);
    fanoutRow.appendChild(card);
  }
  form.appendChild(fanoutRow);

  // Filesystem scope (optional): roots the grant may read/write through
  // the mediated file surfaces. Leave both empty for no restriction.
  const fsFold = document.createElement('details');
  fsFold.className = 'acc-grant-fs-scope';
  const fsSummary = document.createElement('summary');
  fsSummary.textContent = 'Filesystem scope (optional)';
  fsSummary.title = 'Confine file browsing, reads, and writes to specific folders. Leave empty for no restriction. Note: a grant that also allows the terminal can still reach everything through the shell — scoped shells land in a later phase.';
  fsFold.appendChild(fsSummary);
  const fsHint = document.createElement('div');
  fsHint.className = 'acc-grant-flow-msg';
  fsHint.textContent = 'One absolute path per line. Read roots allow browsing and reading below them; write roots allow creating and modifying. Empty = unrestricted.';
  fsFold.appendChild(fsHint);
  for (const [id, label] of [
    ['access-user-client-fs-read', 'Read roots'],
    ['access-user-client-fs-write', 'Write roots'],
  ]) {
    const wrap = document.createElement('label');
    wrap.className = 'acc-grant-fs-roots';
    const caption = document.createElement('span');
    caption.textContent = label;
    const area = document.createElement('textarea');
    area.id = id;
    area.rows = 2;
    area.placeholder = label === 'Read roots' ? '/home/user/projects' : '/home/user/projects/scratch';
    area.spellcheck = false;
    wrap.append(caption, area);
    fsFold.appendChild(wrap);
  }
  form.appendChild(fsFold);

  const actions = document.createElement('div');
  actions.className = 'acc-grant-flow-actions';
  const statusToggle = document.createElement('div');
  statusToggle.className = 'acc-status-toggle';
  for (const [value, text] of [['active', 'Active immediately'], ['draft', 'Save as draft']]) {
    const label = document.createElement('label');
    const input = document.createElement('input');
    input.type = 'radio';
    input.name = 'access-user-client-status';
    input.value = value;
    if (value === 'active') input.checked = true;
    const span = document.createElement('span');
    span.textContent = text;
    label.append(input, span);
    statusToggle.appendChild(label);
  }
  const expirySelect = document.createElement('select');
  expirySelect.id = 'access-user-client-expiry';
  expirySelect.className = 'acc-btn';
  expirySelect.title = 'The grant stops enforcing after this and shows as expired';
  for (const [value, text] of [
    ['', 'Never expires'],
    [String(60 * 60 * 1000), 'Expires in 1 hour'],
    [String(24 * 60 * 60 * 1000), 'Expires in 1 day'],
    [String(7 * 24 * 60 * 60 * 1000), 'Expires in 7 days'],
    [String(30 * 24 * 60 * 60 * 1000), 'Expires in 30 days'],
  ]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = text;
    expirySelect.appendChild(option);
  }
  const button = document.createElement('button');
  button.type = 'submit';
  button.className = 'acc-btn primary';
  button.disabled = !canSubmit || accessUserClientGrantSubmitting;
  button.textContent = accessUserClientGrantSubmitting ? 'Saving…' : 'Save grant';
  const message = document.createElement('div');
  message.className = 'acc-grant-flow-msg';
  message.id = 'access-user-client-grant-message';
  message.textContent = canSubmit
    ? ''
    : 'This session may inspect access but not manage it.';
  actions.append(statusToggle, expirySelect, button, message);
  form.appendChild(actions);

  const ceilingNote = document.createElement('div');
  ceilingNote.className = 'acc-grant-flow-msg';
  ceilingNote.id = 'access-grant-ceiling-note';
  ceilingNote.style.display = 'none';
  ceilingNote.style.color = 'var(--yellow)';
  ceilingNote.style.marginTop = '8px';
  form.appendChild(ceilingNote);

  if (accessGrantFanoutResults?.length) {
    const results = document.createElement('div');
    results.className = 'acc-grant-rows';
    results.style.marginTop = '12px';
    for (const result of accessGrantFanoutResults) {
      const row = document.createElement('div');
      row.className = 'acc-grant-row';
      const label = document.createElement('span');
      label.className = 't';
      label.textContent = result.label;
      row.appendChild(label);
      row.appendChild(accessModelCreateStatus(result.ok ? 'active' : 'failed'));
      const message = document.createElement('span');
      message.className = 'why';
      message.textContent = result.message;
      row.appendChild(message);
      results.appendChild(row);
    }
    const dismiss = document.createElement('button');
    dismiss.type = 'button';
    dismiss.className = 'acc-btn';
    dismiss.textContent = 'Dismiss results';
    dismiss.addEventListener('click', () => {
      accessGrantFanoutResults = null;
      results.remove();
    });
    results.appendChild(dismiss);
    form.appendChild(results);
  }

  mount.appendChild(form);

  const setRadio = (name, value) => {
    const input = form.querySelector(`input[name="${name}"][value="${value}"]`);
    if (input) input.checked = true;
  };
  setRadio('access-user-client-kind', candidate?.kind || 'client_key');
  if (candidate) {
    accessSetField(form, 'access-user-client-label', candidate.label);
    accessSetField(form, 'access-user-client-client-key-fingerprint', candidate.client_key_fingerprint);
    accessSetField(form, 'access-user-client-fingerprint', candidate.fingerprint);
    accessSetField(form, 'access-user-client-user-id', candidate.user_id);
    accessSetField(form, 'access-user-client-account-name', candidate.account_name);
    accessSetField(form, 'access-user-client-account-provider', candidate.account_provider);
    accessSetField(form, 'access-user-client-verified-provider', candidate.verified_provider);
    accessSetField(form, 'access-user-client-organization-id', candidate.organization_id);
    accessSetField(form, 'access-user-client-organization-name', candidate.organization_name);
  }
  setRadio('access-user-client-role', 'role:scoped-human');
  accessSyncUserClientGrantFields(form);
  // Baseline for the shared guard: everything above (autofill included)
  // is the rendered state; anything differing from it is the user's.
  accessGuardStamp(mount);
}

async function accessSubmitUserClientGrant(event) {
  event?.preventDefault?.();
  const form = event?.currentTarget || document.querySelector('#access-user-client-grant-form form');
  if (!form || accessUserClientGrantSubmitting) return;
  const kind = accessSelectedUserClientKind(form);
  const kindHasFingerprint = kind === 'browser_certificate' || kind === 'human_user';
  const kindHasClientKey = kind === 'client_key' || kind === 'human_user';
  const kindHasAccount = kind === 'connect_account' || kind === 'human_user';
  const clientKeyFingerprint = kindHasClientKey
    ? (form.querySelector('#access-user-client-client-key-fingerprint')?.value?.trim() || null)
    : null;
  // Only attach the public key and enrollment origin when the fingerprint
  // being granted is this browser's own key — for a pasted foreign
  // fingerprint neither is known to this session.
  const grantingOwnKey = Boolean(
    clientKeyFingerprint && clientIdentityCache?.fingerprint === clientKeyFingerprint
  );
  const payload = {
    kind,
    label: form.querySelector('#access-user-client-label')?.value?.trim() || null,
    client_key_fingerprint: clientKeyFingerprint,
    client_key: grantingOwnKey ? clientIdentityCache.publicRawB64u : null,
    client_key_origin: grantingOwnKey ? location.origin : null,
    fingerprint: kindHasFingerprint
      ? (form.querySelector('#access-user-client-fingerprint')?.value?.trim() || null)
      : null,
    user_id: kindHasAccount
      ? (form.querySelector('#access-user-client-user-id')?.value?.trim() || null)
      : null,
    account_name: kindHasAccount
      ? (form.querySelector('#access-user-client-account-name')?.value?.trim() || null)
      : null,
    account_provider: kindHasAccount
      ? (form.querySelector('#access-user-client-account-provider')?.value?.trim() || null)
      : null,
    verified_provider: kindHasAccount
      ? (form.querySelector('#access-user-client-verified-provider')?.value?.trim() || null)
      : null,
    organization_id: kind === 'human_user'
      ? (form.querySelector('#access-user-client-organization-id')?.value?.trim() || null)
      : null,
    organization_name: kind === 'human_user'
      ? (form.querySelector('#access-user-client-organization-name')?.value?.trim() || null)
      : null,
    role_id: form.querySelector('input[name="access-user-client-role"]:checked')?.value || 'role:scoped-human',
    status: form.querySelector('input[name="access-user-client-status"]:checked')?.value || 'active',
    expires_at_unix_ms: (() => {
      const delta = Number(form.querySelector('#access-user-client-expiry')?.value || 0);
      return delta > 0 ? Date.now() + delta : null;
    })(),
    fs_read_roots: (form.querySelector('#access-user-client-fs-read')?.value || '')
      .split('\n').map(line => line.trim()).filter(Boolean),
    fs_write_roots: (form.querySelector('#access-user-client-fs-write')?.value || '')
      .split('\n').map(line => line.trim()).filter(Boolean),
  };
  if (kind === 'browser_certificate' && !payload.fingerprint) {
    showControlToast?.('error', 'Fingerprint is required');
    return;
  }
  if (kind === 'client_key' && !payload.client_key_fingerprint) {
    showControlToast?.('error', 'Browser key fingerprint is required');
    return;
  }
  if (kind === 'connect_account' && !payload.user_id && !payload.account_name) {
    showControlToast?.('error', 'User id or account name is required');
    return;
  }
  if (kind === 'human_user'
    && !payload.fingerprint
    && !payload.client_key_fingerprint
    && !payload.user_id
    && !payload.account_name) {
    showControlToast?.('error', 'A person needs at least one binding: key, certificate, or account');
    return;
  }
  if (payload.role_id === 'role:root') {
    const ok = await showDashboardConfirm({
      title: 'Grant root access',
      message: 'Root can control this daemon.',
      confirmLabel: 'Grant root',
      cancelLabel: 'Cancel',
      danger: true,
    });
    if (!ok) return;
  }
  // Fanout: apply the same grant to every selected daemon. Each target
  // authorizes independently — a refusal on one is reported, not fatal to
  // the others.
  const chosenKeys = Array.from(form.querySelectorAll('input[name="access-grant-fanout"]:checked'))
    .map(input => input.value);
  const fanoutTargets = accessGrantFanoutTargets()
    .filter(target => chosenKeys.includes(target.key) && target.mode !== 'connect-only');
  if (!fanoutTargets.length) {
    showControlToast?.('error', 'Pick at least one daemon to apply the grant to');
    return;
  }
  accessUserClientGrantSubmitting = true;
  renderAccessUserClientGrantForm();
  const results = [];
  for (const target of fanoutTargets) {
    try {
      const data = await accessApplyGrantToTarget(target, payload);
      if (target.mode === 'self' && data?.iam) {
        dashboardAccessOverview = { ...(dashboardAccessOverview || {}), iam: data.iam };
      }
      results.push({ label: target.label, ok: true, message: 'Grant saved' });
    } catch (err) {
      results.push({ label: target.label, ok: false, message: err?.message || 'save failed' });
    }
  }
  accessGrantFanoutResults = results;
  const failures = results.filter(result => !result.ok).length;
  if (failures === 0) {
    showControlToast?.('success', results.length === 1
      ? 'Access grant saved'
      : `Access grant saved on ${results.length} daemons`);
  } else {
    showControlToast?.('error', `Grant saved on ${results.length - failures}/${results.length} daemons — see results`);
  }
  await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
  accessUserClientGrantSubmitting = false;
  renderAccessAdminSummaries();
}

async function accessConfirmRevokeGrant(grantId) {
  const id = accessModelLabel(grantId);
  if (!id) return;
  const ok = await showDashboardConfirm({
    title: 'Revoke access grant',
    message: 'This immediately stops this user/client grant from authorizing new dashboard requests.',
    warning: id,
    confirmLabel: 'Revoke',
    cancelLabel: 'Cancel',
    danger: true,
  });
  if (!ok) return;
  await accessUpdateGrantLifecycle(id, { status: 'revoked' });
}

async function accessUpdateGrantLifecycle(grantId, changes = {}) {
  const id = accessModelLabel(grantId);
  if (!id || accessGrantLifecycleSubmitting.has(id)) return;
  const payload = {
    grant_id: id,
    ...changes,
  };
  accessGrantLifecycleSubmitting.add(id);
  renderAccessAdminSummaries();
  try {
    // daemonApi (transport F4): POST twin — verb-derived no-replay, the
    // legacy fallbackAfterRpcFailure:false semantics. IAM-mutating
    // write: payload shape unchanged, no retries.
    const resp = await daemonApi.request('api_access_iam_update_grant', payload);
    const data = resp.body;
    if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
    if (data?.iam) {
      dashboardAccessOverview = { ...(dashboardAccessOverview || {}), iam: data.iam };
    }
    await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
    showControlToast?.('success', 'Access grant updated');
  } catch (err) {
    showControlToast?.('error', err?.message || 'Access grant update failed');
  } finally {
    accessGrantLifecycleSubmitting.delete(id);
    renderAccessAdminSummaries();
  }
}

function accessPermissionMatrixCell(policy, permission) {
  const policyId = accessModelLabel(policy.id);
  const permissionId = accessModelLabel(permission.id);
  if (accessModelLabel(policy.status) === 'planned') return { text: 'Planned', state: 'planned' };
  if (policyId === 'policy:root') return { text: 'Yes', state: 'yes' };
  if (policyId === 'policy:peer-profile') {
    if (permissionId === 'access.manage') return { text: 'No', state: 'no' };
    if (permissionId === 'access.inspect') return { text: 'Peer root', state: 'conditional' };
    if (permissionId === 'peer.inspect' || permissionId === 'peer.manage') return { text: 'Profile', state: 'conditional' };
    const peerPermissions = Array.isArray(policy.permissions) ? policy.permissions.map(accessModelLabel) : [];
    if (peerPermissions.includes(permissionId)) return { text: 'Profile', state: 'conditional' };
    return { text: 'No', state: 'no' };
  }
  const policyPermissions = Array.isArray(policy.permissions) ? policy.permissions.map(accessModelLabel) : [];
  if (policyPermissions.includes(permissionId)) return { text: 'Yes', state: 'yes' };
  return { text: 'No', state: 'no' };
}

function renderAccessPermissionMatrix() {
  const mount = document.getElementById('access-permission-matrix');
  if (!mount) return;
  mount.innerHTML = '';
  const overview = accessOverviewModel();
  const policies = accessOverviewArray(overview, 'policies');
  const permissions = accessOverviewArray(overview, 'permissions');
  if (!policies.length || !permissions.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'No policy or permission model available';
    mount.appendChild(empty);
    return;
  }
  const wrap = document.createElement('div');
  wrap.className = 'access-matrix';
  const table = document.createElement('table');
  table.className = 'access-matrix-table';
  const thead = document.createElement('thead');
  const headRow = document.createElement('tr');
  const first = document.createElement('th');
  first.textContent = 'Policy';
  headRow.appendChild(first);
  for (const permission of permissions) {
    const th = document.createElement('th');
    th.textContent = accessModelLabel(permission.label, permission.id);
    th.title = accessModelLabel(permission.summary);
    headRow.appendChild(th);
  }
  thead.appendChild(headRow);
  table.appendChild(thead);
  const tbody = document.createElement('tbody');
  for (const policy of policies) {
    const row = document.createElement('tr');
    const policyCell = document.createElement('td');
    const policyName = document.createElement('div');
    policyName.className = 'access-matrix-policy';
    policyName.textContent = accessModelLabel(policy.label, policy.id);
    const policyDetail = document.createElement('div');
    policyDetail.textContent = accessModelLabel(policy.summary);
    policyCell.append(policyName, policyDetail);
    row.appendChild(policyCell);
    for (const permission of permissions) {
      const td = document.createElement('td');
      const cell = accessPermissionMatrixCell(policy, permission);
      const pill = document.createElement('span');
      pill.className = `access-matrix-cell ${cell.state}`;
      pill.textContent = cell.text;
      td.appendChild(pill);
      row.appendChild(td);
    }
    tbody.appendChild(row);
  }
  table.appendChild(tbody);
  wrap.appendChild(table);
  mount.appendChild(wrap);
}

function renderAccessModelDetails() {
  const el = document.getElementById('access-model-details');
  if (!el) return;
  el.innerHTML = '';
  const overview = accessOverviewModel();
  const targetLabels = accessOverviewTargetLabelMap(overview);
  const principalLabels = accessOverviewPrincipalLabelMap(overview);
  const principals = Array.isArray(overview.principals) ? overview.principals : [];
  const principalKinds = Array.isArray(overview.supported_principal_kinds) ? overview.supported_principal_kinds : [];
  const grants = Array.isArray(overview.grants) ? overview.grants : [];
  const policies = Array.isArray(overview.policies) ? overview.policies : [];
  const permissions = Array.isArray(overview.permissions) ? overview.permissions : [];
  const transports = Array.isArray(overview.transports) ? overview.transports : [];
  const iam = accessIamModel(overview);
  const iamRoles = Array.isArray(iam.roles) ? iam.roles : [];
  const iamAuditEvents = Array.isArray(iam.audit_events) ? iam.audit_events : [];
  el.append(
    accessModelCreateSection('Principals', principals.map(accessPrincipalRow)),
    accessModelCreateSection('Principal types', principalKinds.map(accessPrincipalKindRow)),
    accessModelCreateSection('Grants', grants.map(grant => accessGrantRow(grant, principalLabels, targetLabels))),
    accessModelCreateSection('Policies', policies.map(accessPolicyRow)),
    accessModelCreateSection('Permissions', permissions.map(accessPermissionRow)),
    accessModelCreateSection('Transports', transports.map(transport => accessTransportRow(transport, targetLabels))),
    accessModelCreateSection('IAM roles', iamRoles.map(accessIamRoleRow)),
    accessModelCreateSection('IAM audit', iamAuditEvents.map(accessIamAuditRow))
  );
}

function accessDiagnosticsCards() {
  const state = connectHealthState();
  return [
    {
      title: 'Dashboard route',
      kind: state.summary.kind === 'ok' ? 'Ready' : state.summary.label,
      state: state.summary.kind === 'ok' ? 'ok' : 'warn',
      detail: `${state.modeLabel} - ${state.summary.title || state.summary.label}`,
      actions: [{ label: 'Open', primary: true, onClick: () => routeTo('access', 'diagnostics') }],
    },
    {
      title: 'Event stream',
      kind: state.status.eventsActive ? 'Active' : 'Inactive',
      state: state.status.eventsActive ? 'ok' : 'warn',
      detail: state.status.eventsActive
        ? 'Dashboard events are flowing over the selected route.'
        : 'Event delivery is not active on the selected route.',
    },
    {
      title: 'Peer targets',
      kind: daemons.length ? `${daemons.length}` : 'None',
      detail: daemons.length
        ? `${daemons.filter(d => d.connected !== false).length} of ${daemons.length} configured peer target${daemons.length === 1 ? '' : 's'} online.`
        : 'No peer targets configured.',
    },
  ];
}

// ── Coordinator route preview ──
// The Route form dispatches blind to "the lexicographically-first peer
// that satisfies all required capabilities"; the eligibility probe
// (api_peer_eligible — the Find panel above the form uses it) can answer
// the same question before the user commits. On capability input,
// debounced, resolve eligibility and show which peer the coordinator
// would pick. Wired here (the dialog markup is 21-access-dialogs.html;
// the Route button's own handlers live in 52-peer-display.js — this
// listener is additive and touches only the preview line).
{
  const capsInput = document.getElementById('coord-route-caps');
  const preview = document.getElementById('coord-route-preview');
  if (capsInput && preview) {
    let previewTimer = 0;
    let previewSeq = 0;
    const setPreview = (text, kind) => {
      preview.textContent = text || '';
      preview.className = 'coord-result coord-route-preview' + (kind ? ` ${kind}` : '');
    };
    const resolveRoutePreview = async () => {
      const caps = String(capsInput.value || '')
        .split(/[\s,]+/)
        .map(s => s.trim())
        .filter(Boolean);
      const seq = ++previewSeq;
      if (!caps.length) {
        setPreview('');
        return;
      }
      if (!daemonApi.availability('api_peer_eligible').ok) {
        setPreview('');
        return;
      }
      try {
        const resp = await daemonApi.request('api_peer_eligible', { capabilities: caps });
        if (seq !== previewSeq) return; // a newer keystroke superseded this probe
        const result = resp.body || {};
        if (!resp.ok) {
          const hint = result.hint ? ` (${result.hint})` : '';
          setPreview(result.error ? `Eligibility: ${result.error}${hint}` : '', 'error');
          return;
        }
        const peers = Array.isArray(result.peers) ? result.peers : [];
        if (!peers.length) {
          setPreview('No eligible peer — no connected peer satisfies these capabilities.', 'error');
          return;
        }
        // Mirror the coordinator's deterministic choice: lexicographically
        // first peer id (plain code-unit order, not locale order).
        const chosen = peers.slice().sort((a, b) => {
          const idA = String(a.host_id || a.id || '');
          const idB = String(b.host_id || b.id || '');
          return idA < idB ? -1 : idA > idB ? 1 : 0;
        })[0];
        const id = String(chosen.host_id || chosen.id || '');
        const label = String(chosen.label || id);
        setPreview(`Will route to: ${label}${label !== id && id ? ` (${id})` : ''}`, 'ok');
      } catch (err) {
        if (seq !== previewSeq) return;
        setPreview('');
      }
    };
    capsInput.addEventListener('input', () => {
      clearTimeout(previewTimer);
      previewTimer = setTimeout(resolveRoutePreview, 350);
    });
  }
}
