// Session fork-points panel (session detail overlay): list the places a
// session can be forked from — the unified catalog GET
// /api/session/{id}/fork-points — and dispatch fork_session_at_anchor.
// Backend-agnostic: codex turn boundaries/item anchors (vanilla rounding
// labeled via effective_cut), native round boundaries, claude-code
// message anchors incl. inactive sibling branch tips (pre-compaction
// anchors flagged informationally — the chain-slice forks them fine).

const _sessionForkPending = new Map(); // request_id -> { statusEl, sessionId }

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
  body.textContent = 'Expand to load fork points…';
  panel.appendChild(body);
  let loaded = false;
  panel.addEventListener('toggle', () => {
    if (!panel.open || loaded) return;
    loaded = true;
    loadSessionForkPoints(session, sessionId, body);
  });
  titleEl.appendChild(panel);
}

async function loadSessionForkPoints(session, sessionId, body) {
  body.textContent = 'Loading fork points…';
  let catalog = null;
  try {
    catalog = await daemonApi.request('api_session_fork_points', { session_id: sessionId });
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
    const requestId = `fork-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const anchor = { kind: point.kind };
    if (point.turn != null) anchor.turn = point.turn;
    if (point.seq != null) anchor.seq = point.seq;
    if (point.item_id) anchor.item_id = point.item_id;
    if (point.position) anchor.position = point.position;
    if (point.message_uuid) anchor.message_uuid = point.message_uuid;
    const msg = {
      action: 'fork_session_at_anchor',
      source,
      session_id: sessionId,
      anchor,
      request_id: requestId,
    };
    const name = nameInput.value.trim();
    const task = taskInput.value.trim();
    if (name) msg.name = name;
    if (task) msg.task = task;
    _sessionForkPending.set(requestId, { statusEl, sessionId });
    sessionForkSetStatus(statusEl, `Forking (${anchorSummaryForUi(anchor)})…`, '');
    dispatchSessionControlMsg(msg);
    form.remove();
  });
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
    setTimeout(() => sessionLineageOpenSession(child), 700);
  }
}

function sessionForkSetStatus(statusEl, text, tone) {
  statusEl.textContent = text;
  statusEl.dataset.tone = tone || '';
}
