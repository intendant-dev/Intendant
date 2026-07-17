// Session forking (session detail overlay). The primary surface is
// INLINE: the unified catalog — GET /api/session/{id}/fork-points —
// loads when the detail opens and is joined onto the rendered transcript
// rows (claude-code by message uuid via the catalog's at_message_uuid
// display anchor; codex turn boundaries by user_turn_index), so every
// eligible log entry grows a hover "fork from here" affordance and
// abandoned claude branches render as resumable off-chain rows. The
// "Fork points" panel remains as the complete-catalog fallback: native
// sessions (no exact row key in the daemon session log), codex item
// anchors (their rows are shared live-lane builders), ineligible
// anchors, and anchors older than the loaded transcript pages.

const _sessionForkPending = new Map(); // request_id -> { statusEl, sessionId }
// session_id -> { source, byRow: Map<rowKey, point> } from the newest
// loaded catalog page; bounded to the most recently viewed sessions.
const _sessionForkInlineIndexes = new Map();
const SESSION_FORK_INLINE_INDEX_CAP = 8;
// Short-lived shared-promise cache: the detail title re-renders on
// rename/identity refreshes, and each render would otherwise re-trigger
// the eager catalog fetch (a full rollout scan server-side for codex).
const _sessionForkCatalogCache = new Map(); // session_id -> { ts, promise }
const SESSION_FORK_CATALOG_TTL_MS = 20000;

function sessionForkFetchCatalog(sessionId) {
  const cached = _sessionForkCatalogCache.get(sessionId);
  const now = Date.now();
  if (cached && now - cached.ts < SESSION_FORK_CATALOG_TTL_MS) return cached.promise;
  const promise = daemonApi.request('api_session_fork_points', { session_id: sessionId });
  _sessionForkCatalogCache.set(sessionId, { ts: now, promise });
  while (_sessionForkCatalogCache.size > SESSION_FORK_INLINE_INDEX_CAP) {
    const oldest = _sessionForkCatalogCache.keys().next().value;
    _sessionForkCatalogCache.delete(oldest);
  }
  return promise;
}

function sessionForkSourceForCatalog(session) {
  const meta = typeof sessionConfigMetadata === 'function' ? sessionConfigMetadata(session) : session;
  const source = typeof sessionConfigSource === 'function' ? sessionConfigSource(meta) : session.source;
  return source || session.source || 'intendant';
}

function renderSessionForkPanel(titleEl, session) {
  const sessionId = session.session_id || session.resume_id;
  if (!titleEl || !sessionId || session.can_resume === false) return;
  const panel = document.createElement('details');
  panel.className = 'sd-fork-points';
  const summary = document.createElement('summary');
  summary.textContent = 'Fork points';
  panel.appendChild(summary);
  const body = document.createElement('div');
  body.className = 'sd-fork-body';
  panel.appendChild(body);
  // Eager load: the catalog feeds the inline per-row fork affordances in
  // the transcript below, not just this panel, so it cannot wait for the
  // panel to be expanded.
  loadSessionForkPoints(session, sessionId, body);
  titleEl.appendChild(panel);
}

async function loadSessionForkPoints(session, sessionId, body) {
  body.textContent = 'Loading fork points…';
  let catalog = null;
  try {
    // Facade contract: request() resolves {ok, status, body} on both
    // lanes — the catalog is the BODY (reading fields off the envelope
    // rendered every session as an empty catalog; found live 2026-07-17).
    const resp = await sessionForkFetchCatalog(sessionId);
    if (!resp || resp.ok === false) {
      const detail = resp && resp.body && resp.body.error;
      body.textContent = `Fork points unavailable: ${detail || `HTTP ${(resp && resp.status) || '?'}`}`;
      return;
    }
    catalog = resp.body;
  } catch (e) {
    body.textContent = `Fork points unavailable: ${e && e.message ? e.message : e}`;
    return;
  }
  if (!catalog || catalog.error) {
    body.textContent = `Fork points unavailable: ${(catalog && catalog.error) || 'empty response'}`;
    return;
  }
  if (catalog.supported === false) {
    body.textContent = catalog.unsupported_reason || 'Forking is not supported for this session.';
    return;
  }
  sessionForkStoreInlineIndex(sessionId, catalog);
  body.textContent = '';
  const status = document.createElement('div');
  status.className = 'sd-fork-status';
  body.appendChild(status);
  const list = document.createElement('div');
  list.className = 'sd-fork-list';
  body.appendChild(list);
  const points = Array.isArray(catalog.fork_points) ? catalog.fork_points : [];
  if (points.length === 0) {
    list.textContent =
      catalog.source === 'intendant'
        ? 'No fork points yet: native sessions expose them from the last persisted conversation (stop or complete a round first).'
        : 'No fork points yet: this session has no completed turns in its transcript.';
    return;
  }
  const source = catalog.source || sessionForkSourceForCatalog(session);
  for (const point of points) {
    list.appendChild(sessionForkPointRow(source, sessionId, point, status));
  }
  if (Array.isArray(catalog.notes)) {
    for (const note of catalog.notes) {
      const noteEl = document.createElement('div');
      noteEl.className = 'sd-fork-note';
      noteEl.textContent = note;
      body.appendChild(noteEl);
    }
  }
}

function sessionForkPointRow(source, sessionId, point, statusEl) {
  const row = document.createElement('div');
  row.className = 'sd-fork-point';
  const kind = document.createElement('span');
  kind.className = `sd-fork-kind sd-fork-kind-${point.kind || 'point'}`;
  kind.textContent =
    point.kind === 'turn-boundary' || point.kind === 'round'
      ? `turn ${point.turn ?? '?'}`
      : point.kind || 'point';
  row.appendChild(kind);
  if (point.pre_compaction) {
    const chip = document.createElement('span');
    chip.className = 'sd-fork-chip';
    chip.textContent = 'pre-compaction';
    chip.title = 'This anchor precedes the newest compaction; a fork resumes the full pre-compaction history.';
    row.appendChild(chip);
  }
  if (point.effective_cut) {
    const chip = document.createElement('span');
    chip.className = 'sd-fork-chip sd-fork-chip-muted';
    chip.textContent = `vanilla cuts at ${point.effective_cut}`;
    chip.title = 'On the vanilla codex binary this item anchor rounds down to the labeled turn boundary; the managed binary cuts it exactly.';
    row.appendChild(chip);
  }
  const preview = document.createElement('span');
  preview.className = 'sd-fork-preview';
  preview.textContent = point.preview || '';
  preview.title = point.preview || '';
  row.appendChild(preview);
  const forkBtn = document.createElement('button');
  forkBtn.className = 'mini-btn';
  forkBtn.textContent = 'Fork from here';
  if (point.eligible === false) {
    forkBtn.disabled = true;
    forkBtn.title = (point.eligibility_reasons || []).join('; ') || 'Not eligible';
  } else {
    forkBtn.addEventListener('click', () => {
      sessionForkInlineForm(row, forkBtn, source, sessionId, point, statusEl);
    });
  }
  row.appendChild(forkBtn);
  return row;
}

function sessionForkInlineForm(row, forkBtn, source, sessionId, point, statusEl) {
  if (row.querySelector('.sd-fork-form')) return;
  forkBtn.disabled = true;
  const form = document.createElement('span');
  form.className = 'sd-fork-form';
  const nameInput = document.createElement('input');
  nameInput.type = 'text';
  nameInput.placeholder = 'child name (optional)';
  const taskInput = document.createElement('input');
  taskInput.type = 'text';
  taskInput.placeholder = 'first prompt (optional)';
  const go = document.createElement('button');
  go.className = 'mini-btn';
  go.textContent = 'Fork';
  const cancel = document.createElement('button');
  cancel.className = 'mini-btn';
  cancel.textContent = 'Cancel';
  form.append(nameInput, taskInput, go, cancel);
  row.appendChild(form);
  cancel.addEventListener('click', () => {
    form.remove();
    forkBtn.disabled = false;
  });
  go.addEventListener('click', () => {
    sessionForkDispatch(source, sessionId, point, nameInput.value.trim(), taskInput.value.trim(), statusEl);
    form.remove();
  });
}

function sessionForkAnchorForPoint(point) {
  const anchor = { kind: point.kind };
  if (point.turn != null) anchor.turn = point.turn;
  if (point.seq != null) anchor.seq = point.seq;
  if (point.item_id) anchor.item_id = point.item_id;
  if (point.position) anchor.position = point.position;
  if (point.message_uuid) anchor.message_uuid = point.message_uuid;
  return anchor;
}

function sessionForkDispatch(source, sessionId, point, name, task, statusEl) {
  const requestId = `fork-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const anchor = sessionForkAnchorForPoint(point);
  const msg = {
    action: 'fork_session_at_anchor',
    source,
    session_id: sessionId,
    anchor,
    request_id: requestId,
  };
  if (name) msg.name = name;
  if (task) msg.task = task;
  _sessionForkPending.set(requestId, { statusEl, sessionId });
  sessionForkSetStatus(statusEl, `Forking (${anchorSummaryForUi(anchor)})…`, '');
  dispatchSessionControlMsg(msg);
}

// ── Inline per-row fork affordances (the transcript IS the lineage explorer) ──

function sessionForkStoreInlineIndex(sessionId, catalog) {
  if (!sessionId || !catalog || catalog.supported === false) return;
  const source = catalog.source || '';
  const byRow = new Map();
  for (const point of Array.isArray(catalog.fork_points) ? catalog.fork_points : []) {
    if (!point || point.eligible === false) continue;
    if (source === 'claude-code') {
      const at = point.at_message_uuid || point.message_uuid;
      if (at && !byRow.has(`uuid:${at}`)) byRow.set(`uuid:${at}`, point);
    } else if (source === 'codex') {
      // Only whole-turn boundaries join inline: their rows are the plain
      // user prompts. Item anchors live on shared live-lane row builders
      // (command output / reasoning) and stay in the panel.
      if (point.kind === 'turn-boundary' && point.turn != null && !byRow.has(`turn:${point.turn}`)) {
        byRow.set(`turn:${point.turn}`, point);
      }
    }
  }
  _sessionForkInlineIndexes.delete(sessionId);
  _sessionForkInlineIndexes.set(sessionId, { source, byRow });
  while (_sessionForkInlineIndexes.size > SESSION_FORK_INLINE_INDEX_CAP) {
    const oldest = _sessionForkInlineIndexes.keys().next().value;
    _sessionForkInlineIndexes.delete(oldest);
  }
  sessionForkRefreshDetailAffordances(sessionId);
}

// The catalog usually resolves after the transcript rendered (rollout
// scans on big sessions take seconds): re-materialize the current window
// so already-built rows pick up their affordances.
function sessionForkRefreshDetailAffordances(sessionId) {
  const view = typeof sessionDetailLogView !== 'undefined' ? sessionDetailLogView : null;
  if (!view || view.sessionId !== sessionId || !view.scroller?.isConnected) return;
  requestAnimationFrame(() => {
    if (sessionDetailLogView !== view || !view.scroller?.isConnected) return;
    renderSessionDetailRange(view, view.renderStart || 0);
  });
}

// A prompt row's fork point is the boundary just BEFORE it — "the child
// keeps everything before this message and redoes from here" — on both
// backends: claude-code points carry the row uuid as at_message_uuid;
// codex prompt rows for turn k map to the turn-boundary after turn k-1.
function sessionForkInlinePointForRecord(sessionId, record) {
  if (!sessionId || !record || record.superseded) return null;
  const idx = _sessionForkInlineIndexes.get(sessionId);
  if (!idx) return null;
  if (idx.source === 'claude-code') {
    const uuid = record.message_uuid;
    return uuid ? idx.byRow.get(`uuid:${uuid}`) || null : null;
  }
  if (idx.source === 'codex') {
    const turn = record.user_turn_index;
    if (turn == null || turn <= 1) return null;
    return idx.byRow.get(`turn:${turn - 1}`) || null;
  }
  return null;
}

function sessionForkRowHint(point) {
  if (point.kind === 'branch-tip') {
    return 'Fork this abandoned branch into a live session (the child keeps the branch through this message).';
  }
  if (point.kind === 'head') {
    return 'Fork at the current head (the child keeps the full history).';
  }
  return 'Fork from before this message: the child keeps everything above and redoes from here.';
}

function appendSessionForkRowAffordance(entry, record, view) {
  const sessionId = (view && view.sessionId) || record.session_id || '';
  const point = sessionForkInlinePointForRecord(sessionId, record);
  if (!point) return;
  const idx = _sessionForkInlineIndexes.get(sessionId);
  const source = (idx && idx.source) || (view && view.source) || '';
  if (point.kind === 'branch-tip') {
    const chip = document.createElement('span');
    chip.className = 'log-branch-chip';
    chip.textContent = 'branch tip';
    chip.title = 'Tip of an abandoned branch — fork here to resume it.';
    entry.appendChild(chip);
  }
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'log-fork-entry';
  btn.textContent = '⑂';
  btn.title = sessionForkRowHint(point);
  btn.setAttribute('aria-label', 'Fork the session from this point');
  btn.addEventListener('click', (ev) => {
    ev.stopPropagation();
    sessionForkToggleRowForm(entry, btn, source, sessionId, point);
  });
  entry.appendChild(btn);
}

function sessionForkToggleRowForm(entry, btn, source, sessionId, point) {
  const existing = entry.querySelector('.log-fork-form-host');
  if (existing) {
    existing.remove();
    entry.classList.remove('has-fork-form');
    btn.disabled = false;
    return;
  }
  btn.disabled = true;
  entry.classList.add('has-fork-form');
  const host = document.createElement('div');
  host.className = 'log-fork-form-host';
  host.addEventListener('click', (ev) => ev.stopPropagation());
  const hint = document.createElement('span');
  hint.className = 'log-fork-hint';
  hint.textContent = sessionForkRowHint(point);
  const nameInput = document.createElement('input');
  nameInput.type = 'text';
  nameInput.placeholder = 'child name (optional)';
  const taskInput = document.createElement('input');
  taskInput.type = 'text';
  taskInput.placeholder = 'first prompt (optional)';
  const go = document.createElement('button');
  go.className = 'mini-btn';
  go.textContent = 'Fork';
  const cancel = document.createElement('button');
  cancel.className = 'mini-btn';
  cancel.textContent = 'Cancel';
  const status = document.createElement('span');
  status.className = 'sd-fork-status';
  host.append(hint, nameInput, taskInput, go, cancel, status);
  cancel.addEventListener('click', () => {
    host.remove();
    entry.classList.remove('has-fork-form');
    btn.disabled = false;
  });
  go.addEventListener('click', () => {
    go.disabled = true;
    sessionForkDispatch(source, sessionId, point, nameInput.value.trim(), taskInput.value.trim(), status);
  });
  entry.appendChild(host);
}

function anchorSummaryForUi(anchor) {
  if (anchor.turn != null) return `turn ${anchor.turn}`;
  if (anchor.seq != null) return `seq ${anchor.seq}`;
  if (anchor.item_id) return `item ${anchor.item_id}`;
  if (anchor.message_uuid) return `message ${String(anchor.message_uuid).slice(0, 8)}`;
  return anchor.kind || 'anchor';
}

function handleSessionForkResult(evt) {
  const requestId = String(evt.request_id || '');
  const pending = _sessionForkPending.get(requestId);
  if (pending) _sessionForkPending.delete(requestId);
  const statusEl = pending && pending.statusEl && pending.statusEl.isConnected ? pending.statusEl : null;
  if (evt.error) {
    if (statusEl) sessionForkSetStatus(statusEl, `Fork failed: ${evt.error}`, 'error');
    return;
  }
  if (typeof scheduleSessionsMetadataRefresh === 'function') scheduleSessionsMetadataRefresh(300);
  const child = evt.child_session_id;
  if (statusEl) {
    sessionForkSetStatus(
      statusEl,
      child
        ? `Forked ${evt.anchor_summary || ''} → ${String(child).slice(0, 8)} (opening…)`
        : `Fork dispatched (${evt.anchor_summary || ''}) — the child appears when it announces.`,
      'ok'
    );
  }
  if (child && typeof sessionLineageOpenSession === 'function') {
    // Positional contract: (sourceSession, targetSession, targetId, ev) —
    // the id rides the third slot; the first two are row snapshots.
    setTimeout(() => sessionLineageOpenSession(null, null, child), 700);
  }
}

function sessionForkSetStatus(statusEl, text, tone) {
  statusEl.textContent = text;
  statusEl.dataset.tone = tone || '';
}
