// ── Managed context dashboard ──
let sessionsLoaded = false;
let _cachedSessions = [];
let managedContextStatus = null;
let managedContextRecords = [];
let managedContextAnchors = [];
let managedContextFissionGroups = [];
let managedContextRecordsLoading = false;
let managedContextAnchorsLoading = false;
let managedContextFissionLoading = false;
let managedContextLastError = '';
let managedContextRefreshTimer = null;
let managedContextRpcSeq = 1;
let managedContextSessionManuallySelected = false;
let managedContextSessionsLoadInFlight = null;
let managedContextActiveSessionId = '';
let sessionsMetadataRefreshTimer = null;

function managedContextEl(id) {
  return document.getElementById(id);
}

async function ensureManagedContextSessionsLoaded() {
  if (typeof sessionsLoaded !== 'undefined' && sessionsLoaded) return;
  if (managedContextSessionsLoadInFlight) return managedContextSessionsLoadInFlight;
  managedContextSessionsLoadInFlight = fetchSessionsForHost(selfPeerId, { force: false })
    .then(sessions => {
      applyLoadedSessions(sessions, document.getElementById('sessions-aggregate'), selfPeerId);
    })
    .catch(() => {})
    .finally(() => {
      managedContextSessionsLoadInFlight = null;
    });
  return managedContextSessionsLoadInFlight;
}

function managedContextConfiguredMode(meta = {}) {
  return String(
    meta.codexManagedContext ||
    meta.codex_managed_context ||
    meta.capabilities?.codexManagedContext ||
    meta.capabilities?.codex_managed_context ||
    ''
  ).trim();
}

function managedContextEffectiveMode(sessionId = managedContextCurrentSessionId(), meta = null) {
  const sessionMeta = meta || managedContextSessionMeta(sessionId);
  const configuredMode = managedContextConfiguredMode(sessionMeta);
  const live = managedContextSessionIsLive(sessionId, sessionMeta);
  const pressure = managedContextStatus?.context_pressure || {};
  return live
    ? (pressure.managed_context || configuredMode || 'unknown')
    : (configuredMode || 'unknown');
}

function managedContextCurrentSessionId() {
  const sel = managedContextEl('managed-context-session');
  return String(sel?.value || '').trim();
}

function managedContextHistoryQueryString() {
  const sessionId = managedContextCurrentSessionId();
  if (!sessionId) return '';
  const meta = managedContextSessionMeta(sessionId);
  const params = new URLSearchParams();
  params.set('session_id', sessionId);
  const backendSessionId = managedContextBackendSessionId(meta);
  const intendantSessionId = managedContextIntendantSessionId(meta);
  if (backendSessionId) params.set('backend_session_id', backendSessionId);
  if (intendantSessionId) params.set('intendant_session_id', intendantSessionId);
  return params.toString();
}

function managedContextBackendSessionId(meta = {}) {
  return String(meta.backend_session_id || meta.backendSessionId || '').trim();
}

function managedContextIntendantSessionId(meta = {}) {
  return String(meta.intendant_session_id || meta.intendantSessionId || '').trim();
}

function managedContextBackendSource(meta = {}) {
  return String(meta.backend_source || meta.backendSource || '').trim();
}

function managedContextSourceLabel(meta = {}) {
  return String(
    meta.backend_source_label ||
    meta.backendSourceLabel ||
    meta.source_label ||
    meta.sourceLabel ||
    meta.backend_source ||
    meta.backendSource ||
    meta.source ||
    ''
  ).trim();
}

function managedContextCanonicalSource(meta = {}) {
  return normalizeAgentId(
    meta.backend_source ||
    meta.backendSource ||
    meta.source ||
    meta.source_label ||
    meta.sourceLabel ||
    meta.provider ||
    ''
  );
}

function managedContextNormalizeSessionMeta(meta = {}, fallbackSessionId = '') {
  const backendSessionId = managedContextBackendSessionId(meta);
  const fallbackId = String(fallbackSessionId || '').trim();
  const ownSessionId = String(meta.session_id || meta.sessionId || fallbackId || '').trim();
  const intendantSessionId =
    managedContextIntendantSessionId(meta) ||
    (backendSessionId && ownSessionId && ownSessionId !== backendSessionId ? ownSessionId : '');
  const backendSource = managedContextBackendSource(meta);
  const sourceLabel = managedContextSourceLabel(meta);
  return {
    ...meta,
    ...(backendSessionId ? { backendSessionId } : {}),
    ...(intendantSessionId ? { intendantSessionId } : {}),
    ...(backendSource ? { backendSource, source: backendSource } : {}),
    ...(sourceLabel ? { sourceLabel } : {}),
  };
}

function managedContextSessionIsLive(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  const backendSessionId = managedContextBackendSessionId(meta);
  const intendantSessionId = managedContextIntendantSessionId(meta);
  return [sid, backendSessionId, intendantSessionId].some(id => {
    if (!id) return false;
    const win = sessionWindows.get(id);
    if (!win) return false;
    // Read the managed/detached flags from sessionMetadataById, the same map
    // setSessionWindowDetached writes them to — the window object never carries them.
    const liveMeta = sessionMetadataById.get(id) || {};
    return liveMeta.detached !== true && liveMeta.managed !== false;
  });
}

function managedContextSessionMeta(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return {};
  const liveMeta = sessionMetadataById.get(sid);
  const cached = (_cachedSessions || []).find(session => {
    if (!session) return false;
    return String(session.session_id || '').trim() === sid ||
      String(session.resume_id || '').trim() === sid ||
      managedContextBackendSessionId(session) === sid ||
      managedContextIntendantSessionId(session) === sid;
  });
  if (liveMeta && cached) {
    return managedContextNormalizeSessionMeta({
      ...cached,
      ...liveMeta,
      capabilities: liveMeta.capabilities || cached.capabilities,
    }, sid);
  }
  if (liveMeta) return managedContextNormalizeSessionMeta(liveMeta, sid);
  return cached ? managedContextNormalizeSessionMeta(cached, sid) : {};
}

function managedContextIsCodexLike(meta = {}) {
  const source = managedContextCanonicalSource(meta);
  if (source === 'codex') return true;
  if (source && source !== 'codex') return false;
  const backendSessionId = managedContextBackendSessionId(meta);
  const label = managedContextSourceLabel(meta).toLowerCase();
  if (label.includes('codex')) return true;
  if (label && label !== 'intendant') return false;
  if (label === 'intendant' && !backendSessionId) return false;
  const configuredMode = managedContextConfiguredMode(meta);
  if ((configuredMode === 'managed' || configuredMode === 'vanilla') && (backendSessionId || !label)) {
    return true;
  }
  const command = String(meta.agentCommand || meta.agent_command || meta.codexCommand || meta.codex_command || meta.capabilities?.codexCommand || meta.capabilities?.codex_command || '').trim();
  if (command && (backendSessionId || !label)) return true;
  const actions = meta.capabilities?.codexThreadActions || meta.capabilities?.codex_thread_actions || [];
  return Array.isArray(actions) && actions.length > 0;
}

function managedContextShouldOfferSession(sessionId, meta = {}, target = '') {
  if (managedContextIsCodexLike(meta)) return true;
  if (String(sessionId || '').trim() !== String(target || '').trim()) return false;
  return !managedContextCanonicalSource(meta) && !managedContextSourceLabel(meta);
}

function managedContextSessionLabel(sessionId, meta = {}) {
  const sid = String(sessionId || '').trim();
  const name = compactSessionText(meta.name) || compactSessionText(meta.task);
  const source = managedContextSourceLabel(meta);
  const wrapper = managedContextIntendantSessionId(meta);
  const prefix = name ? `${name} ` : '';
  const suffix = source ? ` · ${source}` : '';
  const wrapperSuffix = wrapper && wrapper !== sid ? ` via ${shortSessionId(wrapper)}` : '';
  return `${prefix}${shortSessionId(sid)}${suffix}${wrapperSuffix}`;
}

function managedContextAddSessionOption(map, sessionId, meta = {}, preferred = false) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const existing = map.get(sid) || {};
  const normalized = managedContextNormalizeSessionMeta(meta, sid);
  map.set(sid, {
    ...existing,
    ...normalized,
    preferred: preferred || existing.preferred || false,
  });
}

function managedContextSessionOptions() {
  const options = new Map();
  const target = resolvePromptTargetSessionId();
  if (target) {
    const targetMeta = managedContextSessionMeta(target);
    if (managedContextShouldOfferSession(target, targetMeta, target)) {
      managedContextAddSessionOption(options, target, targetMeta, true);
    }
  }
  for (const [sid, win] of sessionWindows) {
    const meta = managedContextNormalizeSessionMeta(sessionMetadataById.get(sid) || win || {}, sid);
    if (managedContextShouldOfferSession(sid, meta, target) || win?.source === 'Codex') {
      managedContextAddSessionOption(options, sid, meta, sid === target);
    }
  }
  for (const [sid, meta] of sessionMetadataById) {
    const normalized = managedContextNormalizeSessionMeta(meta, sid);
    if (managedContextShouldOfferSession(sid, normalized, target)) {
      managedContextAddSessionOption(options, sid, normalized, sid === target);
    }
  }
  for (const session of _cachedSessions || []) {
    const meta = managedContextNormalizeSessionMeta(session, session.session_id);
    const backendSessionId = managedContextBackendSessionId(meta);
    const optionSessionId = backendSessionId || session.session_id;
    if (
      managedContextShouldOfferSession(optionSessionId, meta, target) ||
      managedContextShouldOfferSession(session.session_id, meta, target)
    ) {
      managedContextAddSessionOption(
        options,
        optionSessionId,
        meta,
        optionSessionId === target || session.session_id === target
      );
    }
  }
  return Array.from(options.entries()).sort((a, b) => {
    const pa = a[1].preferred ? 1 : 0;
    const pb = b[1].preferred ? 1 : 0;
    if (pa !== pb) return pb - pa;
    const ma = managedContextConfiguredMode(a[1]) === 'managed' ? 1 : 0;
    const mb = managedContextConfiguredMode(b[1]) === 'managed' ? 1 : 0;
    if (ma !== mb) return mb - ma;
    const la = managedContextSessionIsLive(a[0], a[1]) ? 1 : 0;
    const lb = managedContextSessionIsLive(b[0], b[1]) ? 1 : 0;
    if (la !== lb) return lb - la;
    const ta = Date.parse(a[1].updated_at || a[1].updatedAt || a[1].created_at || a[1].createdAt || '') || 0;
    const tb = Date.parse(b[1].updated_at || b[1].updatedAt || b[1].created_at || b[1].createdAt || '') || 0;
    if (ta !== tb) return tb - ta;
    return managedContextSessionLabel(a[0], a[1]).localeCompare(
      managedContextSessionLabel(b[0], b[1]),
      undefined,
      { sensitivity: 'base' }
    );
  });
}

function renderManagedContextSessionSelect() {
  const sel = managedContextEl('managed-context-session');
  if (!sel) return;
  const previous = sel.value;
  const target = resolvePromptTargetSessionId();
  const options = managedContextSessionOptions();
  sel.innerHTML = '';
  if (!options.length) {
    const opt = document.createElement('option');
    opt.value = '';
    opt.textContent = 'No Codex session';
    sel.appendChild(opt);
    return;
  }
  for (const [sid, meta] of options) {
    const opt = document.createElement('option');
    opt.value = sid;
    opt.textContent = managedContextSessionLabel(sid, meta);
    opt.title = sid;
    sel.appendChild(opt);
  }
  if (!managedContextSessionManuallySelected && target && options.some(([sid]) => sid === target)) {
    sel.value = target;
  } else if (previous && options.some(([sid]) => sid === previous)) {
    sel.value = previous;
  } else {
    const preferred = options.find(([, meta]) => meta.preferred) || options[0];
    sel.value = preferred[0];
  }
}

function resetManagedContextSessionDraft() {
  [
    'managed-context-anchor',
    'managed-context-reason',
    'managed-context-primer',
    'managed-context-preserve',
    'managed-context-discard',
    'managed-context-artifacts',
    'managed-context-next-steps',
    'managed-context-record-id',
    'managed-context-backout-name',
  ].forEach(id => {
    const el = managedContextEl(id);
    if (el) el.value = '';
  });
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = 'No action yet';
}

function syncManagedContextActiveSession() {
  const sessionId = managedContextCurrentSessionId();
  if (sessionId === managedContextActiveSessionId) return;
  managedContextActiveSessionId = sessionId;
  managedContextStatus = null;
  managedContextRecords = [];
  managedContextAnchors = [];
  managedContextRecordsLoading = false;
  managedContextAnchorsLoading = false;
  managedContextLastError = '';
  resetManagedContextSessionDraft();
}

function managedContextNumber(value) {
  if (value === null || value === undefined) return null;
  if (typeof value === 'string' && value.trim() === '') return null;
  const n = Number(value);
  return Number.isFinite(n) ? n : null;
}

function managedContextPositiveNumber(value) {
  const n = managedContextNumber(value);
  return n !== null && n > 0 ? n : null;
}

function managedContextFormatNumber(value) {
  const n = managedContextNumber(value);
  return n === null ? '--' : Math.round(n).toLocaleString();
}

function managedContextPercent(used, windowSize) {
  const u = managedContextNumber(used);
  const w = managedContextNumber(windowSize);
  if (u === null || w === null || w <= 0) return null;
  return Math.max(0, Math.min(999, (u / w) * 100));
}

function managedContextStat(label, value, cls = '') {
  const el = document.createElement('div');
  el.className = 'managed-context-stat' + (cls ? ' ' + cls : '');
  el.innerHTML =
    `<div class="label">${escapeHtml(label)}</div>` +
    `<div class="value">${escapeHtml(value)}</div>`;
  return el;
}

function renderManagedContextStats() {
  const host = managedContextEl('managed-context-stats');
  const fill = managedContextEl('managed-context-pressure-fill');
  if (!host) return;
  host.innerHTML = '';
  const sessionId = managedContextCurrentSessionId();
  const meta = managedContextSessionMeta(sessionId);
  const live = managedContextSessionIsLive(sessionId, meta);
  const pressure = managedContextStatus?.context_pressure || {};
  const status = live ? (pressure.status || 'unknown') : (sessionId ? 'historical' : 'unknown');
  const mode = managedContextEffectiveMode(sessionId, meta);
  const used = live ? pressure.used_tokens : null;
  const effectiveWindow = live
    ? managedContextPositiveNumber(pressure.effective_context_window || pressure.context_window)
    : null;
  const hardWindow = live
    ? managedContextPositiveNumber(pressure.hard_limit || pressure.hard_context_window)
    : null;
  const rewindOnlyLimit = live ? managedContextPositiveNumber(pressure.rewind_only_limit) : null;
  const pct = managedContextPercent(used, effectiveWindow);
  const hardPct = managedContextPercent(used, hardWindow);
  const effectiveValue = `${managedContextFormatNumber(used)} / ${managedContextFormatNumber(effectiveWindow)}${pct === null ? '' : ` (${pct.toFixed(1)}%)`}`;
  const hardValue = `${managedContextFormatNumber(used)} / ${managedContextFormatNumber(hardWindow)}${hardPct === null ? '' : ` (${hardPct.toFixed(1)}%)`}`;
  host.appendChild(managedContextStat('Mode', mode, mode === 'managed' ? 'ok' : 'vanilla'));
  host.appendChild(managedContextStat('Pressure', status, status));
  host.appendChild(managedContextStat('Effective pressure', effectiveValue, status));
  host.appendChild(managedContextStat('Hard limit usage', hardValue, status));
  host.appendChild(managedContextStat('Soft rewind at', managedContextFormatNumber(rewindOnlyLimit), status));
  host.appendChild(managedContextStat('Rewind-only', pressure.rewind_only ? 'on' : 'off', pressure.rewind_only ? 'critical' : 'ok'));
  if (pressure.required_action) {
    const actionTone = pressure.rewind_only ? 'critical' : 'ok';
    host.appendChild(managedContextStat('Action', pressure.required_action, actionTone));
  }
  if (fill) {
    fill.style.width = pct === null ? '0%' : Math.min(pct, 100).toFixed(1) + '%';
    fill.title = pct === null
      ? 'Context pressure unavailable'
      : `Effective pressure ${pct.toFixed(1)}%${hardPct === null ? '' : `; hard usage ${hardPct.toFixed(1)}%`}`;
    fill.style.background = status === 'critical'
      ? 'var(--red)'
      : (status === 'high' || status === 'watch' || status === 'recovery_required')
        ? 'var(--yellow)'
        : 'var(--green)';
  }
  const updated = managedContextEl('managed-context-updated');
  if (updated) updated.textContent = managedContextStatus ? new Date().toLocaleTimeString() : '--';
}

function renderManagedContextAlerts() {
  const host = managedContextEl('managed-context-alerts');
  if (!host) return;
  host.innerHTML = '';
  if (managedContextLastError) {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert error';
    alert.textContent = managedContextLastError;
    host.appendChild(alert);
    return;
  }
  const pressure = managedContextStatus?.context_pressure || {};
  const sessionId = managedContextCurrentSessionId();
  const meta = managedContextSessionMeta(sessionId);
  const mode = managedContextEffectiveMode(sessionId, meta);
  const live = managedContextSessionIsLive(sessionId, meta);
  if (sessionId && !managedContextIsCodexLike(meta)) {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert';
    alert.textContent = 'Selected session is not reported as Codex.';
    host.appendChild(alert);
  }
  if (sessionId && mode !== 'managed') {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert';
    alert.textContent = 'Managed context actions are disabled for this session.';
    host.appendChild(alert);
  }
  if (sessionId && !live) {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert';
    alert.textContent = 'Historical session: records are available, live managed actions require a running Codex session.';
    host.appendChild(alert);
  }
  if (pressure.last_rewind_insufficient?.message) {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert error';
    alert.textContent = pressure.last_rewind_insufficient.message;
    host.appendChild(alert);
  }
  const command = sessionConfigCommand(meta) || controlCodexConfig.command || '';
  if (mode === 'managed' && /(^|\/)codex$/.test(command) && !/target\/(debug|release)\/codex/.test(command)) {
    const alert = document.createElement('div');
    alert.className = 'managed-context-alert';
    alert.textContent = `Codex command is ${command}; managed mode requires the patched app-server build.`;
    host.appendChild(alert);
  }
}

function renderManagedContextAnchors() {
  const host = managedContextEl('managed-context-anchors');
  if (!host) return;
  const sid = managedContextCurrentSessionId();
  const seen = new Set();
  const rows = [];
  const target = String(sid || '').trim();
  for (let i = stationLogAnchorKeys.length - 1; i >= 0 && rows.length < 18; i--) {
    const row = stationLogAnchorRows.get(stationLogAnchorKeys[i]);
    if (!row) continue;
    if (stationAnchorSessionMatches(String(row.sessionId || '').trim(), target)) {
      rows.push({
        itemId: row.id,
        sessionId: row.sessionId || sid,
        content: compactSessionText(row.detail || ''),
      });
    }
  }
  for (const anchor of managedContextAnchors || []) {
    rows.push({
      itemId: anchor.item_id || anchor.itemId,
      sessionId: anchor.session_id || anchor.sessionId || anchor.intendant_session_id || anchor.intendantSessionId || sid,
      content: compactSessionText([
        anchor.tool_name || anchor.toolName || 'tool',
        anchor.status ? `(${anchor.status})` : '',
        anchor.preview || '',
      ].filter(Boolean).join(' ')),
    });
  }
  host.innerHTML = '';
  for (const rowInfo of rows) {
    if (host.childElementCount >= 18) break;
    const itemId = rowInfo.itemId;
    if (!itemId || seen.has(itemId)) continue;
    seen.add(itemId);
    const row = document.createElement('div');
    row.className = 'managed-context-anchor-item';
    const content = rowInfo.content || '';
    row.innerHTML =
      `<div><div class="managed-context-anchor-id">${escapeHtml(itemId)}</div>` +
      `<div class="managed-context-muted">${escapeHtml(content || 'tool call')}</div></div>`;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'managed-context-btn';
    btn.textContent = 'Use';
    btn.addEventListener('click', () => fillManagedContextAnchor(itemId, rowInfo.sessionId || sid));
    row.appendChild(btn);
    host.appendChild(row);
  }
  if (!host.childElementCount) {
    host.innerHTML = managedContextAnchorsLoading
      ? '<div class="managed-context-muted">Loading recent anchors...</div>'
      : '<div class="managed-context-muted">No item anchors found for this session yet.</div>';
  }
}

function fillManagedContextAnchor(itemId, sessionId = '') {
  const sid = String(sessionId || '').trim();
  if (sid) {
    renderManagedContextSessionSelect();
    const sel = managedContextEl('managed-context-session');
    if (sel && Array.from(sel.options).some(o => o.value === sid)) sel.value = sid;
    syncManagedContextActiveSession();
  }
  const anchor = managedContextEl('managed-context-anchor');
  if (anchor) anchor.value = itemId || '';
  // The composer lives in a closed-by-default fold; open it so the
  // just-selected anchor is visible.
  const fold = managedContextEl('managed-context-rewind-fold');
  if (fold) fold.open = true;
  showControlToast('info', 'Managed context anchor selected');
}
window.fillManagedContextAnchor = fillManagedContextAnchor;

function managedContextSplitLines(id) {
  return String(managedContextEl(id)?.value || '')
    .split('\n')
    .map(line => line.trim())
    .filter(Boolean);
}

async function managedContextMcpToolForSession(sessionId, name, args = {}) {
  if (!sessionId) throw new Error('Select a Codex session first.');
  const rpcId = managedContextRpcSeq++;
  // Transport F8a (mcp residue): api_mcp_tool_call is tunnel-only — no
  // HTTP row exists (CONTROL_ONLY_METHODS residue; the /mcp endpoint is
  // the MCP server's own gate, not a route twin), so the facade serves
  // the tunnel leg. A tunnel attempt is FINAL (the legacy
  // fallbackAfterRpcFailure:false semantics — a tool call is a mutation
  // and must never replay); only a dashboard with no tunnel at all takes
  // this site's legacy /mcp lane (never in Connect mode).
  let ok;
  let status;
  let payload;
  if (daemonApi.availability('api_mcp_tool_call').ok) {
    const r = await daemonApi.request('api_mcp_tool_call', {
      mcp_id: rpcId,
      session_id: sessionId,
      name,
      arguments: args,
    }, { fallback: 'never' });
    ok = r.ok;
    status = r.status;
    payload = (r.body && typeof r.body === 'object') ? r.body : {};
  } else if (dashboardConnectModeEnabled()) {
    throw new Error('dashboard Connect RPC is not available for api_mcp_tool_call');
  } else {
    const resp = await authedFetch(`/mcp?session_id=${encodeURIComponent(sessionId)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: rpcId,
        method: 'tools/call',
        params: { name, arguments: args },
      }),
    });
    ok = resp.ok;
    status = resp.status;
    payload = await resp.json().catch(() => ({}));
  }
  if (!ok) throw new Error(payload.error?.message || `MCP HTTP ${status}`);
  if (payload.error) throw new Error(payload.error.message || 'MCP tool failed');
  const result = payload.result || {};
  const text = Array.isArray(result.content)
    ? result.content.map(part => part.text || '').join('\n').trim()
    : '';
  if (result.isError) throw new Error(text || `${name} failed`);
  return text;
}

async function managedContextMcpTool(name, args = {}) {
  return managedContextMcpToolForSession(managedContextCurrentSessionId(), name, args);
}

async function fetchManagedContextStatus() {
  const text = await managedContextMcpTool('get_status', {});
  return JSON.parse(text || '{}');
}

async function fetchManagedContextRecords() {
  const query = managedContextHistoryQueryString();
  if (!query) return [];
  const payload = await fetchManagedContextHistoryJson('records', query);
  return Array.isArray(payload.records) ? payload.records : [];
}

async function fetchManagedContextAnchors() {
  const query = managedContextHistoryQueryString();
  if (!query) return [];
  const payload = await fetchManagedContextHistoryJson('anchors', query);
  return Array.isArray(payload.anchors) ? payload.anchors : [];
}

async function fetchManagedContextFission() {
  const query = managedContextHistoryQueryString();
  if (!query) return [];
  const payload = await fetchManagedContextHistoryJson('fission', query);
  return Array.isArray(payload.groups) ? payload.groups : [];
}

async function fetchManagedContextHistoryJson(kind, query) {
  // daemonApi (transport F2): tunnel first, direct HTTP per the GET-twin
  // fallback policy. The tunnel contract is one pre-encoded query string
  // (`{ query }`); the descriptor's rawQuery column turns it back into the
  // HTTP twin's URL query.
  const method = `api_managed_context_${kind}`;
  const resp = await daemonApi.request(method, { query });
  if (!resp.ok) throw new Error(resp.body?.error || `${kind} HTTP ${resp.status}`);
  return resp.body;
}

function settleManagedContextPromise(promise) {
  return promise.then(
    value => ({ status: 'fulfilled', value }),
    reason => ({ status: 'rejected', reason }),
  );
}

function managedContextDelay(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function scheduleManagedContextRefresh(delay = 500) {
  if (activeActivitySubtab !== 'managed') return;
  if (managedContextRefreshTimer) clearTimeout(managedContextRefreshTimer);
  managedContextRefreshTimer = setTimeout(() => {
    managedContextRefreshTimer = null;
    refreshManagedContextPane();
  }, delay);
}

async function refreshManagedContextPane(options = {}) {
  await ensureManagedContextSessionsLoaded();
  renderManagedContextSessionSelect();
  syncManagedContextActiveSession();
  renderManagedContextAnchors();
  const sessionId = managedContextCurrentSessionId();
  const meta = managedContextSessionMeta(sessionId);
  const live = managedContextSessionIsLive(sessionId, meta);
  if (!sessionId) {
    managedContextStatus = null;
    managedContextRecords = [];
    managedContextAnchors = [];
    managedContextFissionGroups = [];
    managedContextRecordsLoading = false;
    managedContextAnchorsLoading = false;
    managedContextFissionLoading = false;
    managedContextLastError = '';
    renderManagedContextPane();
    return;
  }
  managedContextLastError = '';
  managedContextRecordsLoading = true;
  managedContextAnchorsLoading = true;
  managedContextFissionLoading = true;
  renderManagedContextPane();
  const statusPromise = settleManagedContextPromise(live ? fetchManagedContextStatus() : Promise.resolve(null));
  const recordsPromise = settleManagedContextPromise(fetchManagedContextRecords());
  const anchorsPromise = settleManagedContextPromise(fetchManagedContextAnchors());
  const fissionPromise = settleManagedContextPromise(fetchManagedContextFission());
  const statusResult = await Promise.race([
    statusPromise,
    managedContextDelay(8000).then(() => ({
      status: 'rejected',
      reason: new Error('timed out waiting for live status'),
    })),
  ]);
  if (managedContextCurrentSessionId() !== sessionId) {
    return;
  }
  if (statusResult.status === 'fulfilled') {
    managedContextStatus = statusResult.value;
  } else {
    managedContextStatus = null;
    const message = statusResult.reason?.message || String(statusResult.reason || 'unknown error');
    managedContextLastError = `Live status unavailable: ${message}`;
    if (options.force) console.warn('Managed context status refresh failed:', statusResult.reason);
  }
  renderManagedContextPane();

  const [recordsResult, anchorsResult, fissionResult] = await Promise.all([
    recordsPromise,
    anchorsPromise,
    fissionPromise,
  ]);
  if (managedContextCurrentSessionId() !== sessionId) {
    return;
  }
  managedContextRecordsLoading = false;
  managedContextAnchorsLoading = false;
  managedContextFissionLoading = false;
  const errors = [];
  if (recordsResult.status === 'fulfilled') {
    managedContextRecords = recordsResult.value;
  } else {
    managedContextRecords = [];
    const message = recordsResult.reason?.message || String(recordsResult.reason || 'unknown error');
    errors.push(`Records unavailable: ${message}`);
    if (options.force) console.warn('Managed context records refresh failed:', recordsResult.reason);
  }
  if (anchorsResult.status === 'fulfilled') {
    managedContextAnchors = anchorsResult.value;
  } else {
    managedContextAnchors = [];
    const message = anchorsResult.reason?.message || String(anchorsResult.reason || 'unknown error');
    errors.push(`Anchors unavailable: ${message}`);
    if (options.force) console.warn('Managed context anchors refresh failed:', anchorsResult.reason);
  }
  if (fissionResult.status === 'fulfilled') {
    managedContextFissionGroups = fissionResult.value;
  } else {
    managedContextFissionGroups = [];
    const message = fissionResult.reason?.message || String(fissionResult.reason || 'unknown error');
    errors.push(`Fission groups unavailable: ${message}`);
    if (options.force) console.warn('Managed context fission refresh failed:', fissionResult.reason);
  }
  managedContextLastError = [managedContextLastError, ...errors].filter(Boolean).join(' ');
  renderManagedContextPane();
}

function renderManagedContextRecords() {
  const host = managedContextEl('managed-context-records');
  if (!host) return;
  host.innerHTML = '';
  const selected = String(managedContextEl('managed-context-record-id')?.value || '').trim();
  const selectedExists = selected && managedContextRecords.some(record => record.record_id === selected);
  if (selected && !selectedExists) {
    const input = managedContextEl('managed-context-record-id');
    if (input) input.value = '';
    const result = managedContextEl('managed-context-action-result');
    if (result) result.textContent = 'No action yet';
  }
  if (!managedContextRecords.length) {
    host.innerHTML = managedContextRecordsLoading
      ? '<div class="managed-context-muted">Loading rewind records...</div>'
      : '<div class="managed-context-muted">No rewind records for this session.</div>';
    return;
  }
  for (const record of managedContextRecords) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'managed-context-record-item' + (record.record_id === selected ? ' selected' : '');
    row.innerHTML =
      `<div class="managed-context-record-id">${escapeHtml(record.record_id || '')}</div>` +
      `<div class="managed-context-record-meta">` +
      `<span>${escapeHtml(record.created_at || '')}</span>` +
      `<span>${escapeHtml(record.position || 'after')} ${escapeHtml(record.item_id || '')}</span>` +
      managedContextRecordBadgesHtml(record) +
      `</div>` +
      `<div class="managed-context-muted">${escapeHtml(compactSessionText(record.reason) || 'no reason')}</div>`;
    row.addEventListener('click', () => selectManagedContextRecord(record));
    host.appendChild(row);
  }
}

function managedContextPressureBandChipClass(band) {
  // Mirrors the pressure-fill tones: red at/above the rewind-only limit,
  // yellow in the density-watch band, green below the threshold.
  switch (band) {
    case 'critical': case 'high': return ' failed';
    case 'watch': return ' blocked';
    case 'ok': return ' completed';
    default: return '';
  }
}

function managedContextTokensCompact(value) {
  // One decimal even above 10k ("26.0k/23.8k"): pressure-at-rewind pairs sit
  // close to each other, and whole-k rounding can collapse used and limit
  // into the same number.
  const n = Number(value);
  if (!Number.isFinite(n)) return '--';
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : Math.round(n).toString();
}

function managedContextRecordBadgesHtml(record) {
  const badges = [];
  if (record.surgical === true) {
    badges.push(
      '<span class="managed-context-status-chip failed" title="Supervisor backstop: Intendant chose the anchor and authored a synthetic primer after the model exhausted its recovery steps without rewinding">SURGICAL</span>'
    );
  }
  const band = String(record.pressure_band_at_rewind || '').trim();
  if (band) {
    const used = record.used_tokens_at_rewind;
    const windowAt = record.context_window_at_rewind;
    const counts = used != null && windowAt != null
      ? ` · ${managedContextTokensCompact(used)}/${managedContextTokensCompact(windowAt)}`
      : '';
    badges.push(
      `<span class="managed-context-status-chip${managedContextPressureBandChipClass(band)}"` +
      ` title="Backend-reported context pressure when this record was created (used/rewind-only limit)">` +
      `${escapeHtml(band)}${counts}</span>`
    );
  }
  return badges.join('');
}

function selectManagedContextRecord(record) {
  const input = managedContextEl('managed-context-record-id');
  if (input) input.value = record?.record_id || '';
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = JSON.stringify(record || {}, null, 2);
  renderManagedContextRecords();
}

function managedContextFissionStatusClass(status) {
  // Mirrors fission_ledger::normalize_branch_status so chip colors track the
  // canonical status vocabulary even for legacy/observed raw values.
  switch (String(status || '').trim()) {
    case 'blocked': return 'blocked';
    case 'completed': case 'ended': case 'shutdown': return 'completed';
    case 'failed': case 'errored': return 'failed';
    case 'detached': return 'detached';
    case 'cancelled': case 'canceled': case 'interrupted': return 'cancelled';
    default: return 'running';
  }
}

function managedContextFissionBranchHtml(group, branch) {
  const groupId = group.group_id || '';
  const branchId = branch.session_id || '';
  const canonicalId = group.canonical_session_id || '';
  const status = String(branch.status || '').trim() || 'running';
  const charter = branch.charter || null;
  const changedFiles = Array.isArray(branch.changed_files) ? branch.changed_files.length : 0;
  const chips = [
    `<span class="managed-context-status-chip ${managedContextFissionStatusClass(status)}">${escapeHtml(status)}</span>`,
    `<span class="managed-context-anchor-id" title="${escapeHtml(branchId)}">${escapeHtml(shortSessionId(branchId))}</span>`,
  ];
  if (branchId && branchId === canonicalId) {
    chips.push('<span class="managed-context-status-chip canonical">canonical</span>');
  }
  if (branch.imported_at) {
    chips.push(`<span class="managed-context-status-chip imported" title="imported ${escapeHtml(branch.imported_at)}">imported</span>`);
  }
  if (changedFiles) {
    chips.push(`<span class="managed-context-muted">${changedFiles} changed file${changedFiles === 1 ? '' : 's'}</span>`);
  }
  const details = [];
  if (charter?.objective) details.push(`objective: ${charter.objective}`);
  if (charter?.write_scope) details.push(`write scope: ${charter.write_scope}`);
  if (branch.worktree_path) details.push(`worktree: ${branch.worktree_path}`);
  const summary = compactSessionText(branch.summary || branch.task) || '';
  const actions = ['wait', 'import', 'cancel', 'detach'].map(op =>
    `<button type="button" class="managed-context-btn${op === 'cancel' || op === 'detach' ? ' danger' : ''}"` +
    ` data-fission-op="${op}" data-group-id="${escapeHtml(groupId)}" data-branch-id="${escapeHtml(branchId)}">` +
    `${op.charAt(0).toUpperCase()}${op.slice(1)}</button>`
  ).join('');
  const claim = `<button type="button" class="managed-context-btn" data-claim-fission="${escapeHtml(groupId)}" data-branch-id="${escapeHtml(branchId)}" data-expected-id="${escapeHtml(canonicalId)}">Claim</button>`;
  return `<div class="managed-context-fission-branch">` +
    `<div class="managed-context-inline">${chips.join('')}</div>` +
    (details.length ? `<div class="managed-context-muted">${escapeHtml(details.join(' · '))}</div>` : '') +
    (summary ? `<div class="managed-context-muted">${escapeHtml(summary)}</div>` : '') +
    `<div class="managed-context-inline">${actions}${claim}</div>` +
    `</div>`;
}

function managedContextFissionGroupNode(group) {
  const row = document.createElement('div');
  row.className = 'managed-context-ledger-item';
  const branches = Array.isArray(group.branches) ? group.branches : [];
  const detached = group.detached === true || !!group.detached_at;
  const header = [
    '<strong>fission</strong>',
    `<span class="managed-context-anchor-id">${escapeHtml(group.group_id || '')}</span>`,
    `<span class="managed-context-muted">${escapeHtml(group.tool || '')}</span>`,
    `<span class="managed-context-muted">anchor ${escapeHtml(group.anchor_item_id || '')}</span>`,
    `<span class="managed-context-muted">canonical ${escapeHtml(shortSessionId(group.canonical_session_id || '') || '--')}</span>`,
  ];
  if (detached) {
    header.push(`<span class="managed-context-status-chip detached"${group.detached_at ? ` title="detached ${escapeHtml(group.detached_at)}"` : ''}>detached</span>`);
    const reason = compactSessionText(group.detach_reason) || '';
    if (reason) header.push(`<span class="managed-context-muted">${escapeHtml(reason)}</span>`);
  }
  const objective = compactSessionText(group.objective) || '';
  row.innerHTML =
    `<div class="managed-context-inline">${header.join('')}</div>` +
    (objective ? `<div class="managed-context-muted" style="margin-top:4px">${escapeHtml(objective)}</div>` : '') +
    branches.map(branch => managedContextFissionBranchHtml(group, branch)).join('');
  return row;
}

function renderManagedContextLedgers() {
  const host = managedContextEl('managed-context-ledgers');
  if (!host) return;
  host.innerHTML = '';
  const lineage = managedContextStatus?.lineage_ledger?.groups || [];
  // The fission endpoint serves the merged ledger + extension view (and works
  // for historical sessions too); live-status groups are only a fallback for
  // sessions the endpoint has nothing for yet.
  const statusFission = managedContextStatus?.fission_ledger?.groups || [];
  const fission = managedContextFissionGroups.length ? managedContextFissionGroups : statusFission;
  const addGroup = (kind, group) => {
    const row = document.createElement('div');
    row.className = 'managed-context-ledger-item';
    const branches = Array.isArray(group.branches) ? group.branches : [];
    const branchHtml = branches.map(branch =>
      `<div class="managed-context-inline" style="margin-top:6px">` +
      `<span class="managed-context-anchor-id">${escapeHtml(shortSessionId(branch.session_id || ''))}</span>` +
      `<span class="managed-context-muted">${escapeHtml(branch.relationship || branch.status || '')}</span>` +
      `<span class="managed-context-muted">${escapeHtml(compactSessionText(branch.summary || branch.task) || '')}</span>` +
      `</div>`
    ).join('');
    row.innerHTML =
      `<div class="managed-context-inline">` +
      `<strong>${escapeHtml(kind)}</strong>` +
      `<span class="managed-context-anchor-id">${escapeHtml(group.group_id || '')}</span>` +
      `<span class="managed-context-muted">canonical ${escapeHtml(shortSessionId(group.canonical_session_id || '') || '--')}</span>` +
      `</div>` +
      branchHtml;
    host.appendChild(row);
  };
  lineage.forEach(group => addGroup('lineage', group));
  fission.forEach(group => host.appendChild(managedContextFissionGroupNode(group)));
  if (!host.childElementCount) {
    host.innerHTML = managedContextFissionLoading
      ? '<div class="managed-context-muted">Loading fission groups...</div>'
      : '<div class="managed-context-muted">No lineage or fission groups reported yet.</div>';
  }
}

function renderManagedContextPane() {
  renderManagedContextStats();
  renderManagedContextAlerts();
  renderManagedContextAnchors();
  renderManagedContextRecords();
  renderManagedContextLedgers();
  const sessionId = managedContextCurrentSessionId();
  const meta = managedContextSessionMeta(sessionId);
  const managed =
    managedContextSessionIsLive(sessionId, meta) &&
    managedContextEffectiveMode(sessionId, meta) === 'managed';
  ['managed-context-submit-rewind', 'managed-context-submit-backout', 'managed-context-fission-spawn'].forEach(id => {
    const btn = managedContextEl(id);
    if (btn) btn.disabled = !managed || !sessionId;
  });
  const inspectBtn = managedContextEl('managed-context-inspect-anchor');
  if (inspectBtn) inspectBtn.disabled = !managed || !sessionId;
}

async function inspectManagedContextAnchor() {
  const sessionId = managedContextCurrentSessionId();
  const itemId = String(managedContextEl('managed-context-anchor')?.value || '').trim();
  const result = managedContextEl('managed-context-action-result');
  if (!sessionId || !itemId) {
    showControlToast('error', 'Inspect needs session and anchor');
    return;
  }
  if (result) result.textContent = 'Inspecting anchor...';
  try {
    const text = await managedContextMcpTool('inspect_rewind_anchor', {
      session_id: sessionId,
      item_id: itemId,
      radius: 2,
    });
    if (result) result.textContent = text || 'ok';
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || 'Anchor inspect failed');
  }
}

async function submitManagedContextRewind() {
  const sessionId = managedContextCurrentSessionId();
  const itemId = String(managedContextEl('managed-context-anchor')?.value || '').trim();
  const reason = String(managedContextEl('managed-context-reason')?.value || '').trim();
  const primer = String(managedContextEl('managed-context-primer')?.value || '').trim();
  if (!sessionId || !itemId || !reason || !primer) {
    showControlToast('error', 'Rewind needs session, anchor, reason, and primer');
    return;
  }
  const args = {
    session_id: sessionId,
    anchor: {
      item_id: itemId,
      position: managedContextEl('managed-context-position')?.value === 'before' ? 'before' : 'after',
    },
    reason,
    primer,
    preserve: managedContextSplitLines('managed-context-preserve'),
    discard: managedContextSplitLines('managed-context-discard'),
    artifacts: managedContextSplitLines('managed-context-artifacts'),
    next_steps: managedContextSplitLines('managed-context-next-steps'),
  };
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = 'Dispatching rewind...';
  try {
    const text = await managedContextMcpTool('rewind_context', args);
    if (result) result.textContent = text || 'ok';
    showControlToast('success', 'Managed rewind dispatched');
    scheduleManagedContextRefresh(1000);
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || 'Managed rewind failed');
  }
}

async function submitManagedContextBackout() {
  const sessionId = managedContextCurrentSessionId();
  const recordId = String(managedContextEl('managed-context-record-id')?.value || '').trim();
  if (!sessionId || !recordId) {
    showControlToast('error', 'Backout needs session and record id');
    return;
  }
  const mode = managedContextEl('managed-context-backout-mode')?.value || 'inspect';
  const name = String(managedContextEl('managed-context-backout-name')?.value || '').trim();
  const args = {
    session_id: sessionId,
    record_id: recordId,
    mode,
  };
  if (name) args.name = name;
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = `Running ${mode}...`;
  try {
    const text = await managedContextMcpTool('rewind_backout', args);
    if (result) result.textContent = text || 'ok';
    showControlToast('success', `Managed ${mode} complete`);
    scheduleManagedContextRefresh(1000);
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || 'Managed backout failed');
  }
}

async function claimManagedContextFissionCanonical(groupId, branchId, expectedId) {
  const args = {
    group_id: groupId,
    branch_session_id: branchId,
  };
  if (expectedId) args.expected_canonical_session_id = expectedId;
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = 'Claiming canonical branch...';
  try {
    const text = await managedContextMcpTool('claim_fission_canonical', args);
    if (result) result.textContent = text || 'ok';
    showControlToast('success', 'Canonical branch claimed');
    scheduleManagedContextRefresh(500);
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || 'Canonical claim failed');
  }
}

async function runManagedContextFissionOp(op, groupId, branchId) {
  if (!op || !groupId) {
    showControlToast('error', 'Fission action needs a group');
    return;
  }
  if (op !== 'wait') {
    const verb = op === 'import' ? 'Import the result of' : op === 'cancel' ? 'Cancel' : 'Detach';
    const confirmed = await showDashboardConfirm({
      title: `Confirm fission ${op}`,
      message: `${verb} branch ${shortSessionId(branchId) || '?'} in group ${groupId}?`,
      confirmLabel: op.charAt(0).toUpperCase() + op.slice(1),
      danger: op !== 'import',
    });
    if (!confirmed) return;
  }
  const args = { group_id: groupId, op };
  const sessionId = managedContextCurrentSessionId();
  if (sessionId) args.session_id = sessionId;
  if (branchId) args.branch_session_id = branchId;
  if (op === 'wait') args.timeout_s = 60;
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = `Running fission ${op}...`;
  try {
    const text = await managedContextMcpTool('fission_control', args);
    let parsed = null;
    try { parsed = JSON.parse(text); } catch (_) { /* plain-text tool result */ }
    if (result) result.textContent = parsed ? JSON.stringify(parsed) : (text || 'ok');
    // `still_running` is a normal wait outcome, not a failure.
    const stillRunning = op === 'wait' &&
      (parsed?.status === 'still_running' || parsed?.result === 'still_running' || /\bstill_running\b/.test(text || ''));
    showControlToast(stillRunning ? 'info' : 'success', stillRunning ? 'Branch still running' : `Fission ${op} complete`);
    scheduleManagedContextRefresh(500);
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || `Fission ${op} failed`);
  }
}

const MANAGED_CONTEXT_FISSION_MAX_BRANCHES = 4;

function managedContextFissionRowCount() {
  return managedContextEl('managed-context-fission-rows')
    ?.querySelectorAll('.managed-context-fission-row').length || 0;
}

function addManagedContextFissionRow() {
  const host = managedContextEl('managed-context-fission-rows');
  if (!host) return;
  if (managedContextFissionRowCount() >= MANAGED_CONTEXT_FISSION_MAX_BRANCHES) {
    showControlToast('error', `Fission spawns at most ${MANAGED_CONTEXT_FISSION_MAX_BRANCHES} branches`);
    return;
  }
  const row = document.createElement('div');
  row.className = 'managed-context-fission-row';
  row.innerHTML =
    '<input type="text" class="fission-objective" placeholder="objective (required)" autocomplete="off" spellcheck="true">' +
    '<input type="text" class="fission-write-scope" placeholder="write scope, comma-separated" autocomplete="off" spellcheck="false">' +
    '<input type="text" class="fission-name" placeholder="name" autocomplete="off" spellcheck="false">' +
    '<button type="button" class="managed-context-btn" data-fission-remove-row title="Remove this branch row">&times;</button>';
  host.appendChild(row);
}

function ensureManagedContextFissionRow() {
  if (!managedContextFissionRowCount()) addManagedContextFissionRow();
}

async function submitManagedContextFissionSpawn() {
  const sessionId = managedContextCurrentSessionId();
  if (!sessionId) {
    showControlToast('error', 'Fission spawn needs a session');
    return;
  }
  const rows = Array.from(
    managedContextEl('managed-context-fission-rows')?.querySelectorAll('.managed-context-fission-row') || []
  );
  const branches = [];
  for (const row of rows) {
    const objective = String(row.querySelector('.fission-objective')?.value || '').trim();
    const writeScope = String(row.querySelector('.fission-write-scope')?.value || '').trim();
    const name = String(row.querySelector('.fission-name')?.value || '').trim();
    if (!objective) {
      if (writeScope || name) {
        showControlToast('error', 'Every fission branch row needs an objective');
        return;
      }
      continue; // fully empty row: ignore it
    }
    const branch = { objective };
    const scope = writeScope.split(',').map(part => part.trim()).filter(Boolean);
    if (scope.length) branch.write_scope = scope;
    if (name) branch.name = name;
    branches.push(branch);
  }
  if (!branches.length || branches.length > MANAGED_CONTEXT_FISSION_MAX_BRANCHES) {
    showControlToast('error', `Fission spawn needs 1-${MANAGED_CONTEXT_FISSION_MAX_BRANCHES} branch objectives`);
    return;
  }
  const args = { session_id: sessionId, branches };
  const worktree = String(managedContextEl('managed-context-fission-worktree')?.value || '');
  if (worktree === 'on') args.use_worktree = true;
  else if (worktree === 'off') args.use_worktree = false;
  const result = managedContextEl('managed-context-action-result');
  if (result) result.textContent = `Spawning ${branches.length} fission branch${branches.length === 1 ? '' : 'es'}...`;
  try {
    const text = await managedContextMcpTool('fission_spawn', args);
    if (result) result.textContent = text || 'ok';
    showControlToast('success', 'Fission spawn dispatched');
    scheduleManagedContextRefresh(1000);
  } catch (err) {
    if (result) result.textContent = err?.message || String(err);
    showControlToast('error', err?.message || 'Fission spawn failed');
  }
}

function wireManagedContextListeners() {
  managedContextEl('managed-context-session')?.addEventListener('change', () => {
    managedContextSessionManuallySelected = true;
    refreshManagedContextPane({ force: true });
  });
  managedContextEl('managed-context-use-target')?.addEventListener('click', () => {
    const target = resolvePromptTargetSessionId();
    managedContextSessionManuallySelected = false;
    renderManagedContextSessionSelect();
    const sel = managedContextEl('managed-context-session');
    if (target && sel && Array.from(sel.options).some(o => o.value === target)) sel.value = target;
    refreshManagedContextPane({ force: true });
  });
  managedContextEl('managed-context-refresh')?.addEventListener('click', () => refreshManagedContextPane({ force: true }));
  managedContextEl('managed-context-refresh-records')?.addEventListener('click', () => refreshManagedContextPane({ force: true }));
  managedContextEl('managed-context-inspect-anchor')?.addEventListener('click', inspectManagedContextAnchor);
  managedContextEl('managed-context-clear-anchor')?.addEventListener('click', () => {
    const anchor = managedContextEl('managed-context-anchor');
    if (anchor) anchor.value = '';
  });
  managedContextEl('managed-context-submit-rewind')?.addEventListener('click', submitManagedContextRewind);
  managedContextEl('managed-context-submit-backout')?.addEventListener('click', submitManagedContextBackout);
  managedContextEl('managed-context-copy-status')?.addEventListener('click', () => {
    const text = JSON.stringify(managedContextStatus || {}, null, 2);
    navigator.clipboard?.writeText(text)
      .then(() => showControlToast('success', 'Managed status copied'))
      .catch(err => showControlToast('error', 'Copy failed: ' + (err?.message || err)));
  });
  managedContextEl('managed-context-ledgers')?.addEventListener('click', ev => {
    const opBtn = ev.target?.closest?.('button[data-fission-op]');
    if (opBtn) {
      runManagedContextFissionOp(
        opBtn.dataset.fissionOp || '',
        opBtn.dataset.groupId || '',
        opBtn.dataset.branchId || ''
      );
      return;
    }
    const btn = ev.target?.closest?.('button[data-claim-fission]');
    if (!btn) return;
    claimManagedContextFissionCanonical(
      btn.dataset.claimFission || '',
      btn.dataset.branchId || '',
      btn.dataset.expectedId || ''
    );
  });
  managedContextEl('managed-context-fission-add-row')?.addEventListener('click', addManagedContextFissionRow);
  managedContextEl('managed-context-fission-spawn')?.addEventListener('click', submitManagedContextFissionSpawn);
  managedContextEl('managed-context-fission-rows')?.addEventListener('click', ev => {
    const btn = ev.target?.closest?.('button[data-fission-remove-row]');
    if (!btn) return;
    if (managedContextFissionRowCount() <= 1) {
      showControlToast('info', 'Fission spawn keeps at least one branch row');
      return;
    }
    btn.closest('.managed-context-fission-row')?.remove();
  });
  ensureManagedContextFissionRow();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', wireManagedContextListeners);
} else {
  wireManagedContextListeners();
}

const DIFF_LANGUAGE_EXTENSIONS = {
  js: 'javascript',
  jsx: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  ts: 'typescript',
  tsx: 'typescript',
  rs: 'rust',
  py: 'python',
  rb: 'ruby',
  go: 'go',
  java: 'java',
  c: 'c',
  h: 'c',
  cc: 'cpp',
  cpp: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  cs: 'csharp',
  swift: 'swift',
  kt: 'kotlin',
  kts: 'kotlin',
  php: 'php',
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  fish: 'shell',
  ps1: 'shell',
  json: 'json',
  jsonl: 'json',
  toml: 'toml',
  yaml: 'yaml',
  yml: 'yaml',
  md: 'markdown',
  markdown: 'markdown',
  css: 'css',
  scss: 'css',
  sass: 'css',
  html: 'html',
  htm: 'html',
  xml: 'xml',
  sql: 'sql',
};

const DIFF_LANGUAGE_LABELS = {
  javascript: 'JS',
  typescript: 'TS',
  rust: 'Rust',
  python: 'Python',
  ruby: 'Ruby',
  go: 'Go',
  java: 'Java',
  c: 'C',
  cpp: 'C++',
  csharp: 'C#',
  swift: 'Swift',
  kotlin: 'Kotlin',
  php: 'PHP',
  shell: 'Shell',
  json: 'JSON',
  toml: 'TOML',
  yaml: 'YAML',
  markdown: 'Markdown',
  css: 'CSS',
  html: 'HTML',
  xml: 'XML',
  sql: 'SQL',
};

const DIFF_COMMON_KEYWORDS = new Set([
  'as', 'async', 'await', 'break', 'case', 'catch', 'class', 'const',
  'continue', 'default', 'defer', 'delete', 'do', 'else', 'enum', 'export',
  'extends', 'false', 'finally', 'for', 'from', 'func', 'function', 'if',
  'import', 'in', 'interface', 'let', 'match', 'mod', 'mut', 'new', 'nil',
  'null', 'pub', 'return', 'self', 'static', 'struct', 'switch', 'this',
  'throw', 'trait', 'true', 'try', 'type', 'undefined', 'use', 'var', 'where',
  'while',
]);

function detectDiffLanguage(path) {
  const clean = String(path || '').split('?')[0].split('#')[0].toLowerCase();
  const base = clean.split(/[\\/]/).pop() || '';
  if (base === 'makefile' || base === 'dockerfile') return 'shell';
  if (base === 'cargo.toml') return 'toml';
  if (base === 'package.json' || base === 'tsconfig.json') return 'json';
  const m = base.match(/\.([a-z0-9]+)$/);
  return m ? (DIFF_LANGUAGE_EXTENSIONS[m[1]] || '') : '';
}

function diffLanguageLabel(language) {
  return DIFF_LANGUAGE_LABELS[language] || '';
}

function spanSyntax(cls, value) {
  return `<span class="${cls}">${escapeHtml(value)}</span>`;
}

function highlightMarkdownFragment(src) {
  let html = escapeHtml(src);
  html = html.replace(/^(\s{0,3}#{1,6})(\s.*)?$/, '<span class="syntax-md-heading">$1</span>$2');
  html = html.replace(/^(\s*(?:[-*+]|\d+\.|&gt;))(\s+)/, '<span class="syntax-md-marker">$1</span>$2');
  html = html.replace(/(`[^`]+`)/g, '<span class="syntax-md-code">$1</span>');
  html = html.replace(/(\*\*[^*]+\*\*)/g, '<span class="syntax-keyword">$1</span>');
  return html;
}

function highlightJsonLikeFragment(src) {
  let html = escapeHtml(src);
  html = html.replace(/(&quot;(?:\\.|[^&])*?&quot;)(\s*:)/g, '<span class="output-json-key">$1</span>$2');
  html = html.replace(/(:\s*)(&quot;(?:\\.|[^&])*?&quot;)/g, '$1<span class="syntax-string">$2</span>');
  html = html.replace(/\b(true|false|null)\b/g, '<span class="syntax-keyword">$1</span>');
  html = html.replace(/\b(-?\d+(?:\.\d+)?)\b/g, '<span class="syntax-number">$1</span>');
  return html;
}

function highlightProgrammingFragment(src, language) {
  let html = '';
  let i = 0;
  const hashComment = ['python', 'ruby', 'shell', 'toml', 'yaml'].includes(language);
  const sqlComment = language === 'sql';

  while (i < src.length) {
    const ch = src[i];
    const next = src[i + 1] || '';

    if (hashComment && ch === '#') {
      html += spanSyntax('syntax-comment', src.slice(i));
      break;
    }
    if (sqlComment && ch === '-' && next === '-') {
      html += spanSyntax('syntax-comment', src.slice(i));
      break;
    }
    if (ch === '/' && next === '/') {
      html += spanSyntax('syntax-comment', src.slice(i));
      break;
    }
    if (ch === '/' && next === '*') {
      const end = src.indexOf('*/', i + 2);
      const stop = end === -1 ? src.length : end + 2;
      html += spanSyntax('syntax-comment', src.slice(i, stop));
      i = stop;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      const quote = ch;
      let j = i + 1;
      while (j < src.length) {
        if (src[j] === '\\') { j += 2; continue; }
        if (src[j] === quote) { j++; break; }
        j++;
      }
      html += spanSyntax('syntax-string', src.slice(i, j));
      i = j;
      continue;
    }
    if (/[0-9]/.test(ch) && (i === 0 || !/[A-Za-z0-9_]/.test(src[i - 1] || ''))) {
      let j = i + 1;
      while (j < src.length && /[A-Za-z0-9_.]/.test(src[j])) j++;
      html += spanSyntax('syntax-number', src.slice(i, j));
      i = j;
      continue;
    }
    if (/[A-Za-z_$]/.test(ch)) {
      let j = i + 1;
      while (j < src.length && /[A-Za-z0-9_$]/.test(src[j])) j++;
      const token = src.slice(i, j);
      const rest = src.slice(j).trimStart();
      if (DIFF_COMMON_KEYWORDS.has(token)) {
        html += spanSyntax('syntax-keyword', token);
      } else if (/^[A-Z][A-Za-z0-9_$]*$/.test(token)) {
        html += spanSyntax('syntax-type', token);
      } else if (rest.startsWith('(')) {
        html += spanSyntax('syntax-fn', token);
      } else {
        html += escapeHtml(token);
      }
      i = j;
      continue;
    }
    if ('{}[]().,;:'.includes(ch)) {
      html += spanSyntax('syntax-punct', ch);
    } else {
      html += escapeHtml(ch);
    }
    i++;
  }
  return html;
}

function highlightDiffCodeFragment(src, language) {
  if (!language) return escapeHtml(src);
  if (language === 'markdown') return highlightMarkdownFragment(src);
  if (language === 'json') return highlightJsonLikeFragment(src);
  return highlightProgrammingFragment(src, language);
}

function renderDiffLineCode(line, cls, language) {
  if (!language || cls.includes('diff-line-file') || cls.includes('diff-line-hunk')) {
    return escapeHtml(line);
  }
  if (line.startsWith('+') || line.startsWith('-') || line.startsWith(' ')) {
    return escapeHtml(line[0]) + highlightDiffCodeFragment(line.slice(1), language);
  }
  return highlightDiffCodeFragment(line, language);
}

function renderDiffLines(diffText, path = '') {
  if (!diffText) return '<span class="changes-empty">No changes</span>';
  let oldLine = null;
  let newLine = null;
  const language = detectDiffLanguage(path);
  const lines = diffText.endsWith('\n') ? diffText.slice(0, -1).split('\n') : diffText.split('\n');
  return lines.map(line => {
    let cls = 'diff-line diff-line-ctx';
    let oldNo = '';
    let newNo = '';

    if (line.startsWith('@@')) {
      cls = 'diff-line diff-line-hunk';
      const m = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (m) {
        oldLine = Number(m[1]);
        newLine = Number(m[2]);
      }
    } else if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff --git')) {
      cls = 'diff-line diff-line-file';
    } else if (line.startsWith('+')) {
      cls = 'diff-line diff-line-add';
      if (newLine !== null) newNo = String(newLine++);
    } else if (line.startsWith('-')) {
      cls = 'diff-line diff-line-del';
      if (oldLine !== null) oldNo = String(oldLine++);
    } else {
      if (oldLine !== null) oldNo = String(oldLine++);
      if (newLine !== null) newNo = String(newLine++);
    }

    return `<span class="${cls}"><span class="diff-old">${oldNo}</span><span class="diff-new">${newNo}</span><span class="diff-code">${renderDiffLineCode(line, cls, language)}</span></span>`;
  }).join('');
}

function normalizeDiffPath(raw) {
  const path = String(raw || '').split('\t')[0].trim();
  if (!path || path === '/dev/null') return '';
  return path
    .replace(/^"|"$/g, '')
    .replace(/^(a|b)\//, '');
}

function parseDiffMetadataLine(line) {
  const m = String(line || '').match(/^# intendant-(project-root|cwd):\s*(.*)$/);
  if (!m) return null;
  const key = m[1] === 'project-root' ? 'projectRoot' : 'cwd';
  return { key, value: m[2].trim() };
}

function parseDiffGitLine(line) {
  const match = String(line || '').match(/^diff --git\s+(.+?)\s+(.+)$/);
  if (!match) return { oldPath: '', newPath: '' };
  return {
    oldPath: normalizeDiffPath(match[1]),
    newPath: normalizeDiffPath(match[2]),
  };
}

function createParsedDiffFile(oldPath = '', newPath = '') {
  return {
    oldPath,
    newPath,
    path: newPath || oldPath || 'diff',
    lines: [],
    added: 0,
    removed: 0,
    seenOldHeader: false,
    seenNewHeader: false,
  };
}

function parseUnifiedDiff(diffText) {
  const text = String(diffText || '');
  const files = [];
  const metadata = {};
  let current = null;
  const lines = text.endsWith('\n') ? text.slice(0, -1).split('\n') : text.split('\n');
  const pushFile = (oldPath = '', newPath = '') => {
    current = createParsedDiffFile(oldPath, newPath);
    files.push(current);
    return current;
  };
  const ensureFile = () => current || pushFile();

  for (const line of lines) {
    const meta = parseDiffMetadataLine(line);
    if (meta) {
      metadata[meta.key] = meta.value;
      continue;
    }
    if (line.startsWith('diff --git ')) {
      const parsed = parseDiffGitLine(line);
      pushFile(parsed.oldPath, parsed.newPath).lines.push(line);
      continue;
    }
    if (line.startsWith('--- ')) {
      if (!current || current.seenOldHeader || current.lines.some(l => l.startsWith('@@'))) {
        pushFile();
      }
      current.oldPath = normalizeDiffPath(line.slice(4));
      current.path = current.newPath || current.oldPath || current.path;
      current.seenOldHeader = true;
      current.lines.push(line);
      continue;
    }
    if (line.startsWith('+++ ')) {
      ensureFile();
      current.newPath = normalizeDiffPath(line.slice(4));
      current.path = current.newPath || current.oldPath || current.path;
      current.seenNewHeader = true;
      current.lines.push(line);
      continue;
    }

    ensureFile().lines.push(line);
    if (line.startsWith('+')) current.added += 1;
    else if (line.startsWith('-')) current.removed += 1;
  }

  const visibleFiles = files.filter(file => file.lines.length > 0);
  const totalAdded = visibleFiles.reduce((sum, file) => sum + file.added, 0);
  const totalRemoved = visibleFiles.reduce((sum, file) => sum + file.removed, 0);
  return {
    files: visibleFiles,
    added: totalAdded,
    removed: totalRemoved,
    projectRoot: metadata.projectRoot || '',
    cwd: metadata.cwd || '',
  };
}

function diffLogContent(c) {
  const content = String(c?.content || '');
  const source = String(c?.source || '').toLowerCase();
  if (source === 'diff') return content;
  if (!content.startsWith('External agent diff')) return content;
  const firstLineEnd = content.indexOf('\n');
  return firstLineEnd >= 0 ? content.slice(firstLineEnd + 1) : '';
}

function isDiffLog(c) {
  const content = String(c?.content || '');
  const source = String(c?.source || '').toLowerCase();
  if (source === 'diff') return true;
  if (!content.startsWith('External agent diff')) return false;
  const diffText = diffLogContent(c);
  return /(^|\n)(diff --git |--- |\+\+\+ |@@ )/.test(diffText);
}

function diffLogSummaryHtml(parsed) {
  const files = parsed.files || [];
  const fileCount = files.length;
  const displayFileCount = fileCount || 1;
  const fileLabel = `${displayFileCount} file${displayFileCount === 1 ? '' : 's'}`;
  const paths = files
    .map(file => file.path)
    .filter(Boolean);
  const pathText = paths.length > 3
    ? `${paths.slice(0, 3).join(', ')} +${paths.length - 3} more`
    : paths.join(', ');
  const stats = `<span class="diff-log-summary-add">+${parsed.added || 0}</span> <span class="diff-log-summary-del">-${parsed.removed || 0}</span>`;
  return `<span class="status-dot"></span><span class="diff-log-summary-main">diff &middot; ${escapeHtml(fileLabel)} &middot; ${stats}</span>`
    + (pathText ? `<span class="diff-log-summary-paths" title="${escapeHtml(paths.join(', '))}">${escapeHtml(pathText)}</span>` : '');
}

function isAbsolutePath(path) {
  const value = String(path || '');
  return value.startsWith('/') || /^[A-Za-z]:[\\/]/.test(value);
}

function joinPathForDisplay(root, path) {
  const base = String(root || '').replace(/[\\/]+$/, '');
  const rel = String(path || '').replace(/^[\\/]+/, '');
  if (!base || !rel) return '';
  const sep = /^[A-Za-z]:[\\/]/.test(base) ? '\\' : '/';
  return `${base}${sep}${rel}`;
}

function projectRootForDiff(parsed, options = {}) {
  if (parsed?.projectRoot) return parsed.projectRoot;
  if (options.projectRoot) return options.projectRoot;
  const sid = String(options.sessionId || '').trim();
  if (sid) {
    const meta = sessionMetadataById.get(sid) || {};
    if (meta.projectRoot) return meta.projectRoot;
    if (meta.cwd) return meta.cwd;
  }
  return '';
}

function absolutePathForDiffFile(file, parsed, options = {}) {
  const path = file.path || file.newPath || file.oldPath || '';
  if (!path) return '';
  if (isAbsolutePath(path)) return path;
  const root = projectRootForDiff(parsed, options);
  return root ? joinPathForDisplay(root, path) : '';
}

function diffFileHeaderInfo(file) {
  const lines = file.lines || [];
  const renameFrom = lines.find(line => line.startsWith('rename from '))?.slice('rename from '.length).trim() || '';
  const renameTo = lines.find(line => line.startsWith('rename to '))?.slice('rename to '.length).trim() || '';
  const hasNewFileMode = lines.some(line => line.startsWith('new file mode '));
  const hasDeletedFileMode = lines.some(line => line.startsWith('deleted file mode '));
  let status = 'modified';
  if (renameFrom || renameTo) {
    status = 'renamed';
  } else if (hasNewFileMode || (!file.oldPath && !!file.newPath)) {
    status = 'new';
  } else if (hasDeletedFileMode || (!!file.oldPath && !file.newPath)) {
    status = 'deleted';
  }
  return { status, renameFrom, renameTo };
}

function diffStatusLabel(status) {
  return {
    new: 'new file',
    deleted: 'deleted',
    renamed: 'renamed',
    modified: 'modified',
  }[status] || 'modified';
}

function isUnifiedDiffFileHeaderLine(line) {
  return line.startsWith('diff --git ')
    || line.startsWith('index ')
    || line.startsWith('new file mode ')
    || line.startsWith('deleted file mode ')
    || line.startsWith('old mode ')
    || line.startsWith('new mode ')
    || line.startsWith('similarity index ')
    || line.startsWith('dissimilarity index ')
    || line.startsWith('rename from ')
    || line.startsWith('rename to ')
    || line.startsWith('copy from ')
    || line.startsWith('copy to ')
    || line.startsWith('--- ')
    || line.startsWith('+++ ');
}

function renderDiffLogLines(file) {
  const visibleLines = (file.lines || []).filter(line => !isUnifiedDiffFileHeaderLine(line));
  return renderDiffLines(visibleLines.join('\n'), file.path || file.newPath || file.oldPath || '');
}

function appendDiffBadge(container, text, className = '') {
  if (!text) return;
  const badge = document.createElement('span');
  badge.className = `diff-log-file-badge${className ? ` ${className}` : ''}`;
  badge.textContent = text;
  container.appendChild(badge);
}

function absolutePathForDiffPath(path, parsed, options = {}) {
  const value = String(path || '').trim();
  if (!value) return '';
  if (isAbsolutePath(value)) return value;
  const root = projectRootForDiff(parsed, options);
  return root ? joinPathForDisplay(root, value) : value;
}

function renderDiffLogBody(body, parsed, options = {}) {
  body.innerHTML = '';
  const files = parsed.files && parsed.files.length ? parsed.files : [createParsedDiffFile('', '')];
  for (const file of files) {
    const fileEl = document.createElement('div');
    fileEl.className = 'diff-log-file';

    const header = document.createElement('div');
    header.className = 'diff-log-file-header';
    const pathWrap = document.createElement('span');
    pathWrap.className = 'diff-log-file-path-wrap';
    const mainRow = document.createElement('span');
    mainRow.className = 'diff-log-file-main-row';
    const path = document.createElement('span');
    path.className = 'diff-log-file-path';
    const absolutePath = absolutePathForDiffFile(file, parsed, options);
    path.textContent = absolutePath || file.path || 'diff';
    path.title = absolutePath || [file.oldPath, file.newPath].filter(Boolean).join(' -> ') || file.path || 'diff';
    mainRow.appendChild(path);

    const badges = document.createElement('span');
    badges.className = 'diff-log-file-badges';
    const language = detectDiffLanguage(file.path || file.newPath || file.oldPath || '');
    const languageLabel = diffLanguageLabel(language);
    const headerInfo = diffFileHeaderInfo(file);
    appendDiffBadge(badges, languageLabel, 'lang');
    appendDiffBadge(badges, diffStatusLabel(headerInfo.status), `status ${headerInfo.status}`);
    appendDiffBadge(badges, `+${file.added || 0}`, 'add');
    appendDiffBadge(badges, `-${file.removed || 0}`, 'del');
    mainRow.appendChild(badges);
    pathWrap.appendChild(mainRow);

    if (headerInfo.status === 'renamed' && headerInfo.renameFrom) {
      const detailRow = document.createElement('span');
      detailRow.className = 'diff-log-file-detail-row';
      const label = document.createElement('span');
      label.className = 'diff-log-file-detail-label';
      label.textContent = 'Renamed from';
      const oldPath = document.createElement('span');
      oldPath.className = 'diff-log-file-abs-path';
      const oldAbsolutePath = absolutePathForDiffPath(headerInfo.renameFrom, parsed, options);
      oldPath.textContent = oldAbsolutePath;
      oldPath.title = oldAbsolutePath;
      detailRow.appendChild(label);
      detailRow.appendChild(oldPath);
      pathWrap.appendChild(detailRow);
    }

    header.appendChild(pathWrap);
    fileEl.appendChild(header);

    const lines = document.createElement('pre');
    lines.className = 'diff-log-lines';
    lines.innerHTML = renderDiffLogLines(file);
    fileEl.appendChild(lines);
    body.appendChild(fileEl);
  }
}

function resetSessionWindowLog(win) {
  if (!win || !win.log) return;
  win.logHistory = [];
  win.renderStart = 0;
  win.renderEnd = 0;
  if (typeof renderSessionWindowLogPlaceholder === 'function') {
    renderSessionWindowLogPlaceholder(win);
  } else {
    win.log.innerHTML = '<div class="session-window-empty">Waiting for events...</div>';
  }
  win.followOutput = true;
  win.pendingOutput = false;
  updateSessionWindowJumpButton(win);
  scheduleSessionWindowGridFit();
}

function resetSessionWindowsForReplay(entries) {
  const ids = new Set();
  for (const entry of entries || []) {
    const sid = entry && entry.session_id;
    if (sid) ids.add(sessionWindowTargetForLogSession(sid));
  }
  for (const sid of ids) {
    const win = sessionWindows.get(sid);
    if (win) resetSessionWindowLog(win);
  }
}

function clearLogs(options = {}) {
  const clearSessionWindows = options.clearSessionWindows !== false;
  clearMainLogContainers();
  logEntryCount = 0;
  updateLogEmptyState();
  activeCommandOutputGroups.clear();
  commandOutputGroups.clear();
  if (clearSessionWindows) {
    for (const win of sessionWindows.values()) {
      resetSessionWindowLog(win);
    }
  }
}

function renderTurnSeparator(turn) {
  finalizeActiveCommandOutputGroup();
  const stream = currentMainLogContainer();
  if (!stream) return;
  const sep = document.createElement('div');
  sep.className = 'log-turn-sep';
  sep.textContent = '\u2500\u2500 Turn ' + turn + ' \u2500\u2500';
  stream.appendChild(sep);
}

// Stores base64 image data keyed by DOM element — lazy-loaded on expand
const _logImageStore = new WeakMap();
const _deferredCommandOutputStore = new WeakMap();
const _lazyCommandOutputStore = new WeakMap();
const _wiredCollapsibleLogEntries = new WeakSet();
const _wiredCommandOutputLogEntries = new WeakSet();
const _wiredDiffLogEntries = new WeakSet();

function appendDeferredLogImages(entry, cnt) {
  if (!entry || !cnt || !_logImageStore.has(entry)) return;
  const imgs = _logImageStore.get(entry);
  _logImageStore.delete(entry);
  const gallery = document.createElement('div');
  gallery.className = 'log-image-gallery';
  for (const b64 of imgs) {
    const img = document.createElement('img');
    img.src = 'data:image/png;base64,' + b64;
    img.className = 'log-screenshot';
    img.loading = 'lazy';
    gallery.appendChild(img);
  }
  cnt.appendChild(gallery);
}

function wireCollapsibleLogEntry(entry, cnt, sourceEntry = null) {
  if (!entry || !cnt || _wiredCollapsibleLogEntries.has(entry)) return;
  if (sourceEntry && _logImageStore.has(sourceEntry) && !_logImageStore.has(entry)) {
    _logImageStore.set(entry, _logImageStore.get(sourceEntry));
  }
  _wiredCollapsibleLogEntries.add(entry);
  entry.addEventListener('click', (event) => {
    if (event.target?.closest?.('.log-edit-message, .log-copy-entry')) return;
    const expanding = !entry.classList.contains('expanded');
    entry.classList.toggle('expanded', expanding);
    if (expanding) appendDeferredLogImages(entry, cnt);
  });
}

function wireSessionWindowLogClone(clone, sourceEntry) {
  if (!clone) return;
  copyLogEntryCopyText(sourceEntry, clone);
  wireLogCopyButton(clone);
  if (clone.classList.contains('command-output-group')) {
    wireCommandOutputGroupClone(clone, sourceEntry);
    return;
  }
  if (clone.classList.contains('diff-log-entry')) {
    wireDiffLogEntry(clone);
    return;
  }
  if (!clone.classList.contains('collapsible')) return;
  wireCollapsibleLogEntry(clone, clone.querySelector('.log-content'), sourceEntry);
}

function stableStringHash(value) {
  let hash = 0;
  const text = String(value || '');
  for (let i = 0; i < text.length; i++) {
    hash = ((hash << 5) - hash) + text.charCodeAt(i);
    hash |= 0;
  }
  return hash;
}

// Stable hash-to-hue mapping so each host gets its own visually
// distinct badge color. Saturation is intentionally low so the pills
// recede as backgrounds next to the log-level colors (info/model/
// agent/etc.) rather than competing with them for attention.
function hostBadgeColor(hostId) {
  const hash = stableStringHash(hostId);
  const hue = ((hash % 360) + 360) % 360;
  return `hsl(${hue}, 32%, 58%)`;
}

const SESSION_BADGE_PALETTE = [
  { bg: 'rgba(137, 180, 250, 0.20)', border: 'rgba(137, 180, 250, 0.65)', fg: '#b4c9ff' },
  { bg: 'rgba(166, 227, 161, 0.18)', border: 'rgba(166, 227, 161, 0.62)', fg: '#b8efb3' },
  { bg: 'rgba(249, 226, 175, 0.19)', border: 'rgba(249, 226, 175, 0.62)', fg: '#f6dfa8' },
  { bg: 'rgba(245, 194, 231, 0.18)', border: 'rgba(245, 194, 231, 0.62)', fg: '#f2bfe3' },
  { bg: 'rgba(148, 226, 213, 0.18)', border: 'rgba(148, 226, 213, 0.62)', fg: '#a7eee2' },
  { bg: 'rgba(250, 179, 135, 0.19)', border: 'rgba(250, 179, 135, 0.62)', fg: '#f7b486' },
  { bg: 'rgba(203, 166, 247, 0.18)', border: 'rgba(203, 166, 247, 0.62)', fg: '#d5b8ff' },
  { bg: 'rgba(137, 220, 235, 0.18)', border: 'rgba(137, 220, 235, 0.62)', fg: '#9beaff' },
  { bg: 'rgba(243, 139, 168, 0.18)', border: 'rgba(243, 139, 168, 0.62)', fg: '#f5a0bb' },
  { bg: 'rgba(180, 190, 254, 0.18)', border: 'rgba(180, 190, 254, 0.62)', fg: '#c3caff' },
  { bg: 'rgba(255, 213, 128, 0.18)', border: 'rgba(255, 213, 128, 0.62)', fg: '#ffd993' },
  { bg: 'rgba(120, 220, 170, 0.18)', border: 'rgba(120, 220, 170, 0.62)', fg: '#94e8bd' },
];
const sessionBadgeStyles = new Map();
const usedSessionBadgePaletteIndexes = new Set();

function allocateSessionBadgeStyle(sessionId) {
  const sid = String(sessionId || '');
  if (sessionBadgeStyles.has(sid)) return sessionBadgeStyles.get(sid);
  const hash = stableStringHash(sid);
  const palette = SESSION_BADGE_PALETTE;
  let style = null;
  if (usedSessionBadgePaletteIndexes.size < palette.length) {
    const start = ((hash % palette.length) + palette.length) % palette.length;
    for (let i = 0; i < palette.length; i++) {
      const idx = (start + i) % palette.length;
      if (usedSessionBadgePaletteIndexes.has(idx)) continue;
      usedSessionBadgePaletteIndexes.add(idx);
      style = palette[idx];
      break;
    }
  }
  if (!style) {
    const hue = ((hash % 360) + 360) % 360;
    style = {
      bg: `hsla(${hue}, 46%, 42%, 0.22)`,
      border: `hsla(${hue}, 58%, 64%, 0.64)`,
      fg: `hsl(${hue}, 76%, 78%)`,
    };
  }
  sessionBadgeStyles.set(sid, style);
  return style;
}

function clearSessionBadgeStyle(el) {
  if (!el) return;
  el.style.removeProperty('--session-badge-bg');
  el.style.removeProperty('--session-badge-border');
  el.style.removeProperty('--session-badge-fg');
}

function applySessionBadgeStyle(el, sessionId) {
  if (!el) return;
  if (!sessionId) {
    clearSessionBadgeStyle(el);
    return;
  }
  const style = allocateSessionBadgeStyle(sessionId);
  el.style.setProperty('--session-badge-bg', style.bg);
  el.style.setProperty('--session-badge-border', style.border);
  el.style.setProperty('--session-badge-fg', style.fg);
}

function shortSessionId(sessionId) {
  return String(sessionId || '').slice(0, 8);
}

function setDaemonSessionId(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const previous = daemonSessionFullId;
  daemonSessionFullId = sid;
  if (previous && previous !== sid) {
    markSessionWindowsDetachedAfterDaemonRestart();
  }
  const el = document.getElementById('sb-session');
  if (el) {
    el.textContent = shortSessionId(sid);
    el.title = sid;
  }
  updateTaskTargetChip();
  stationScheduleUpdate();
}

function applySessionIdentity(meta = {}) {
  const sid = String(meta.session_id || meta.intendant_session_id || '').trim();
  let backendSessionId = String(meta.backend_session_id || meta.backendSessionId || '').trim();
  const backendSource = String(meta.backend_source || meta.backendSource || meta.source || '').trim();
  // Placeholder thread ids (Claude Code before its stream announces the
  // real session id) must never become a backend alias: status routing
  // would retarget at a window that never materializes, and the ghost
  // window it conjures can steal the prompt target. Old session logs and
  // scraped debug lines still carry the placeholder, so filter centrally.
  if (backendSessionId === 'claude-code-session') backendSessionId = '';
  if (!sid || (!backendSessionId && !backendSource)) return;
  const wrapperMeta = sessionMetadataById.get(sid) || {};
  const next = {
    ...(backendSessionId ? { backendSessionId } : {}),
    ...(backendSource ? { backendSource, source: backendSource } : {}),
    ...(meta.backend_source_label || meta.backendSourceLabel ? { sourceLabel: meta.backend_source_label || meta.backendSourceLabel } : {}),
  };
  sessionMetadataById.set(sid, { ...wrapperMeta, ...next });
  if (backendSessionId) {
    const backendMeta = sessionMetadataById.get(backendSessionId) || {};
    const capabilities = backendMeta.capabilities || wrapperMeta.capabilities || null;
    sessionMetadataById.set(backendSessionId, {
      ...backendMeta,
      intendantSessionId: sid,
      ...(backendSource ? { backendSource, source: backendSource } : {}),
      ...(capabilities ? { capabilities } : {}),
    });
    if (sessionWindows.has(backendSessionId)) {
      updateSessionWindow(backendSessionId, {
        ...(backendSource ? { backendSource, source: backendSource } : {}),
        ...(capabilities ? { capabilities } : {}),
      });
    }
  }
  const externalSource = normalizeAgentId(backendSource);
  if (externalSource && backendSessionId && backendSessionId !== sid) {
    retireExternalWrapperSessionWindow(sid, backendSessionId);
  } else if (sessionWindows.has(sid)) {
    updateSessionWindow(sid, next);
  }
  persistSessionWindowState();
  updateTaskTargetChip();
  stationScheduleUpdate();
}

function applySessionIdentitiesFromReplayEntries(entries) {
  if (!Array.isArray(entries)) return;
  for (const entry of entries) {
    if (entry?.event !== 'session_identity') continue;
    applySessionIdentity(entry);
  }
}

function applyExternalIdentityFromLogEntry(entry = {}) {
  const message = String(entry?.message || entry?.data?.message || '').trim();
  if (!message) return false;
  let backendSessionId = '';
  let source = '';
  const modeMatch = message.match(/^Mode:\s*external agent\s*\(([^)]+)\).*?\bthread:\s*([^\s,]+)/i);
  if (modeMatch) {
    source = normalizeAgentId(modeMatch[1]);
    backendSessionId = modeMatch[2];
  } else {
    const threadMatch = message.match(/^External agent thread:\s*([^\s,]+)/i);
    if (threadMatch) backendSessionId = threadMatch[1];
  }
  if (!backendSessionId) return false;
  source = source || normalizeAgentId(
    entry?.backend_source ||
    entry?.backendSource ||
    entry?.source ||
    currentExternalAgent ||
    controlCurrentBackend ||
    ''
  );
  const wrapperSessionId = String(
    entry?.session_id ||
    entry?.sessionId ||
    entry?.intendant_session_id ||
    entry?.intendantSessionId ||
    daemonSessionFullId ||
    ''
  ).trim();
  if (!wrapperSessionId || !source) return false;
  applySessionIdentity({
    session_id: wrapperSessionId,
    backend_session_id: backendSessionId,
    source,
  });
  return true;
}

function applyExternalIdentitiesFromLogEntries(entries) {
  if (!Array.isArray(entries)) return;
  for (const entry of entries) {
    applyExternalIdentityFromLogEntry(entry);
  }
}

function externalSourceForSessionWindow(sessionId, win = null) {
  const sid = String(sessionId || '').trim();
  const meta = sid ? (sessionMetadataById.get(sid) || {}) : {};
  return normalizeAgentId(
    meta.backendSource ||
    meta.source ||
    meta.sourceLabel ||
    win?.source ||
    ''
  );
}

function sessionMetadataStatusIsTerminal(session = {}) {
  const status = String(
    session.status ||
    session.intendant_status ||
    session.intendantStatus ||
    ''
  ).trim().toLowerCase();
  return status === 'abandoned' || status === 'deleted' || status === 'missing';
}

function clearStaleSessionWindowDetached(sessionId, reason = '') {
  const sid = String(sessionId || '').trim();
  if (!sid || !sessionWindows.has(sid) || !sessionWindowIsDetached(sid)) return;
  setSessionWindowDetached(sid, false, reason || 'session is attached');
}

function setSessionWindowDetached(sessionId, detached, reason = '') {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const meta = sessionMetadataById.get(sid) || {};
  const next = {
    ...meta,
    detached: !!detached,
    managed: !detached,
    ...(reason ? { detachReason: reason } : {}),
  };
  if (detached) {
    if (!meta.detachedCapabilities) {
      next.detachedPreviousCapabilities = meta.capabilities || null;
    }
    next.capabilities = {
      ...(meta.capabilities || {}),
      steer: false,
      interrupt: false,
    };
    next.detachedCapabilities = true;
  } else if (next.detachReason) {
    delete next.detachReason;
  }
  if (!detached && next.detachedCapabilities) {
    if (next.detachedPreviousCapabilities) {
      next.capabilities = next.detachedPreviousCapabilities;
    } else {
      delete next.capabilities;
    }
    delete next.detachedCapabilities;
    delete next.detachedPreviousCapabilities;
  }
  sessionMetadataById.set(sid, next);
  updateSessionWindowActionMenuVisibility(sid);
  if (detached) clearSessionWindowPendingActive(sid, 'idle');
  updateStopButtonVisibility(currentPhase);
  updateSubmitButtonLabel(currentPhase);
  persistSessionWindowState();
}

function markSessionWindowsDetachedAfterDaemonRestart() {
  let detachedAny = false;
  for (const [sid, win] of sessionWindows) {
    if (!externalSourceForSessionWindow(sid, win)) continue;
    setSessionWindowDetached(sid, true, 'daemon restarted');
    detachedAny = true;
  }
  if (detachedAny) updateTaskTargetChip();
}

function sessionWindowIsDetached(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  const meta = sessionMetadataById.get(sid) || {};
  return meta.detached === true || meta.managed === false;
}

function externalBackendAliasForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return null;
  const meta = sessionMetadataById.get(sid) || {};
  const backendSessionId = String(meta.backendSessionId || '').trim();
  const source = externalSourceForSessionWindow(sid);
  if (!backendSessionId || backendSessionId === sid || !source) return null;
  return { backendSessionId, source };
}

function sessionWindowTargetForLogSession(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return '';
  const alias = externalBackendAliasForSession(sid);
  return alias?.backendSessionId || sid;
}

function sessionWindowRestoreIsInFlightFor(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return false;
  if (sessionWindowRestoreInFlight.has(sid)) return true;
  for (const inflightSid of sessionWindowRestoreInFlight) {
    const targetSid = sessionWindowTargetForLogSession(inflightSid);
    if (targetSid && targetSid === sid) return true;
  }
  return false;
}

function statusSessionWindowTarget(sessionId) {
  const sid = String(sessionId || '').trim();
  const target = sessionWindowTargetForLogSession(sid);
  // An alias with no materialized window (e.g. a backend placeholder id
  // recorded before the real native id was known) must not swallow phase
  // updates while a window for the raw session id is on screen.
  if (target !== sid && !sessionWindows.has(target) && sessionWindows.has(sid)) return sid;
  return target;
}

function shouldMaterializeStatusSessionWindow(sessionId) {
  const sid = statusSessionWindowTarget(sessionId);
  if (!sid || sid === daemonSessionFullId) return false;
  const win = sessionWindows.get(sid);
  if (win) return true;
  return !!externalSourceForSessionWindow(sid, win);
}

function shouldApplyStatusPhaseForSession(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid || sid === daemonSessionFullId) return true;
  const targetSid = statusSessionWindowTarget(sid);
  return !!targetSid
    && shouldMaterializeStatusSessionWindow(targetSid)
    && targetSid === resolvePromptTargetSessionId();
}

function recordRecentSessionStatusPhase(sessionId, phase) {
  const sid = String(sessionId || '').trim();
  if (!sid || sid === daemonSessionFullId || !phase) return;
  const targetSid = statusSessionWindowTarget(sid) || sid;
  const record = { phase: normalizeSessionPhase(phase), at: Date.now() };
  recentSessionStatusPhases.set(sid, record);
  if (targetSid && targetSid !== sid) recentSessionStatusPhases.set(targetSid, record);
}

function recentSessionStatusPhase(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return '';
  const record = recentSessionStatusPhases.get(sid);
  if (!record) return '';
  if (Date.now() - record.at > SESSION_ATTACH_STATUS_FRESH_MS) {
    recentSessionStatusPhases.delete(sid);
    return '';
  }
  return record.phase || '';
}

function attachedSessionPhase(sessionId) {
  return recentSessionStatusPhase(sessionId) || 'idle';
}

function hasSessionLifecycleTask(task) {
  return String(task || '').trim().length > 0;
}

function focusSessionWindowFromLifecycle(sessionId, options = {}) {
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  const currentTarget = String(
    options.currentTarget === undefined ? resolvePromptTargetSessionId() : options.currentTarget
  ).trim();
  const force = !!options.force;
  const shouldFocus = force || !currentTarget || currentTarget === sid;
  if (shouldFocus && (!processingLogReplay || !foregroundSessionFullId || force)) {
    focusSessionWindow(sid);
    return true;
  } else {
    updateTaskTargetChip();
    return false;
  }
}

function retargetSessionWindowLogEntry(entry, sessionId) {
  const sid = String(sessionId || '').trim();
  if (!entry || !sid) return;
  entry.dataset.sessionId = sid;
  const badge = entry.querySelector(':scope > .log-session');
  if (badge) {
    renderSessionIdentity(badge, sid, { showName: false });
    applySessionBadgeStyle(badge, sid);
  }
  const edit = entry.querySelector(':scope > .log-edit-message');
  if (edit) edit.dataset.sessionId = sid;
  applyPromptTargetLogSessionBadgeState(entry);
}

function transferSessionWindowHistory(fromWin, toWin, targetSessionId) {
  if (!fromWin || !toWin || fromWin === toWin) return;
  const sourceHistory = ensureSessionWindowHistory(fromWin);
  if (!sourceHistory.length) return;
  const targetHistory = ensureSessionWindowHistory(toWin);
  const seen = new Set(targetHistory);
  const shouldFollow = sessionWindowShouldFollowNextOutput(toWin);
  for (const entry of sourceHistory) {
    if (!entry || seen.has(entry)) continue;
    retargetSessionWindowHistoryItem(entry, targetSessionId);
    targetHistory.push(prepareSessionWindowHistoryItem(entry));
    seen.add(entry);
  }
  sourceHistory.length = 0;
  renderSessionWindowTail(toWin);
  applySessionWindowOutputScroll(toWin, shouldFollow);
}

function retireExternalWrapperSessionWindow(wrapperId, backendId) {
  const wrapperSid = String(wrapperId || '').trim();
  const backendSid = String(backendId || '').trim();
  if (!wrapperSid || !backendSid || wrapperSid === backendSid) return;
  const wrapperWin = sessionWindows.get(wrapperSid);
  if (!wrapperWin) return;

  const wrapperMeta = sessionMetadataById.get(wrapperSid) || {};
  const backendMeta = sessionMetadataById.get(backendSid) || {};
  const source = externalSourceForSessionWindow(wrapperSid, wrapperWin)
    || externalSourceForSessionWindow(backendSid);
  const phase = processingLogReplay
    ? 'idle'
    : (wrapperWin.phase || backendMeta.phase || 'idle');
  const backendWin = ensureSessionWindow(backendSid, {
    ...wrapperMeta,
    ...backendMeta,
    ...(source ? { source, backendSource: source } : {}),
    phase,
    ended: wrapperWin.ended,
  });
  transferSessionWindowHistory(wrapperWin, backendWin, backendSid);
  wrapperWin.el.remove();
  sessionWindows.delete(wrapperSid);
  if (maximizedSessionWindowId === wrapperSid) maximizedSessionWindowId = backendSid;
  const wasForeground = foregroundSessionFullId === wrapperSid;
  const wasCurrent = currentSessionFullId === wrapperSid;
  if (wasForeground) foregroundSessionFullId = backendSid;
  if (wasCurrent) currentSessionFullId = backendSid;
  if (wasForeground || wasCurrent) {
    focusSessionWindow(backendSid);
  } else {
    updateTaskTargetChip();
  }
  updateSessionWindowMaximizeState();
  syncSessionWindowGridControls();
  syncSessionWindowMetadataRefresh();
  scheduleSessionRelationshipRender();
  persistSessionWindowState();
}

function phaseKey(phase) {
  return String(phase || 'idle').toLowerCase().trim().replace(/-/g, '_');
}

function isWaitingFollowUpPhase(phase) {
  const p = phaseKey(phase);
  return p === 'waiting_followup' || p === 'waiting_follow_up';
}

function normalizeSessionPhase(phase) {
  const p = phaseKey(phase);
  if (p === 'running_agent') return 'running';
  if (isWaitingFollowUpPhase(p)) return 'idle';
  return p || 'idle';
}

function sessionPhaseClass(phase) {
  const p = normalizeSessionPhase(phase);
  if (isAgentActivePhase(p)) return 'active';
  if (p.startsWith('waiting')) return 'waiting';
  if (p === 'done' || p === 'idle' || p === 'interrupted') return 'done';
  return '';
}

function hasPendingActiveSessionWindow(sessionId) {
  const sid = String(sessionId || '').trim();
  const win = sid ? sessionWindows.get(sid) : null;
  if (!win || !win.pendingActiveUntil) return false;
  if (win.pendingActiveUntil <= Date.now()) {
    win.pendingActiveUntil = 0;
    return false;
  }
  return true;
}

function isSessionWindowEffectivelyActive(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return isAgentActivePhase(currentPhase);
  if (hasPendingActiveSessionWindow(sid)) return true;
  const win = sessionWindows.get(sid);
  if (!win || !win.phase) return false;
  const phase = win.phase;
  return isAgentActivePhase(phase);
}

function hasActiveSessionWindowExcept(sessionId) {
  const exceptSid = String(sessionId || '').trim();
  for (const [sid, win] of sessionWindows) {
    if (!win || sid === exceptSid || win.ended) continue;
    if (hasPendingActiveSessionWindow(sid) || isAgentActivePhase(win.phase)) return true;
  }
  return false;
}

function isSteerPhase(phase) {
  const p = phaseKey(phase);
  return isAgentActivePhase(p) && p !== 'interrupting';
}

function isSessionWindowSteerActive(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return isSteerPhase(currentPhase);
  if (hasPendingActiveSessionWindow(sid)) return true;
  const win = sessionWindows.get(sid);
  if (!win || !win.phase) return false;
  // An external session still parked on its optimistic 'thinking' after the
  // pending window expired is UNCERTAIN, not known-active: route the next
  // message as a plain follow-up instead of a steer. (A wrong follow-up
  // just starts the turn; a wrong steer stalls in the 2s fallback and is
  // logged with steer semantics — the confusing case users hit.)
  if (win.optimisticActiveExpired && normalizeSessionPhase(win.phase) === 'thinking') {
    return false;
  }
  return isSteerPhase(win.phase);
}

// Server status updates name their session: keep that window's phase
// truthful even when it is not the prompt target (the global banner stays
// gated on the target in the set_phase command). A server phase is
// authoritative — it also retires the optimistic-active guard.
//
// The session id must go through the identity translation: external
// sessions re-key their window to the backend-native id while server
// status events keep riding the wrapper/log id. A direct lookup missed
// the re-keyed window, so its phase stayed optimistic forever — which is
// what parked the composer in follow-up mode ("steering doesn't work")
// via the optimistic-expired steer gate.
function applyServerPhaseToSessionWindow(sessionId, phase) {
  const sid = String(sessionId || '').trim();
  if (!sid || !phase) return;
  const targetSid = (typeof sessionWindowTargetForLogSession === 'function'
    && sessionWindowTargetForLogSession(sid)) || sid;
  const win = sessionWindows.get(targetSid);
  if (!win) return;
  win.optimisticActiveExpired = false;
  if (!isAgentActivePhase(normalizeSessionPhase(phase))) win.pendingActiveUntil = 0;
  // A pending follow-up row's message became this turn's input the moment
  // the session went active — retire it as delivered. (The daemon lane's
  // task-envelope channel drops follow_up ids, so no FollowUpStatus echo
  // will ever come for these; without this the row sat at ⏳ forever.)
  if (isAgentActivePhase(normalizeSessionPhase(phase))
      && typeof retirePendingFollowUpRowsForSession === 'function') {
    retirePendingFollowUpRowsForSession(targetSid, sid);
  }
  updateSessionWindow(targetSid, { phase });
}

