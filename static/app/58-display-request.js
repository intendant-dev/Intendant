// ── Display request rail (the user-display doorbell) ───────────────────
//
// A scoped agent called `request_user_display`: the daemon broadcasts
// `display_request_raised` and this panel asks the USER — the only
// authority that can grant it — to Allow (with a duration), Deny, or
// Deny-for-this-session. The decision goes back as the dedicated
// `resolve_display_request` control action; nothing here ever touches the
// approval rail (no Approve-all, no auto-approval, distinct id space).
//
// Handled end to end in JS (the WASM presence layer does not know these
// events), like session_note / shared_view: fragment 36's dispatcher calls
// handleDisplayRequestRaised / handleDisplayRequestResolved before WASM.

// { id, sessionId, access } while the panel is up.
let pendingDisplayRequest = null;
let displayRequestExpireTimer = null;

function clearPendingDisplayRequest() {
  pendingDisplayRequest = null;
  if (displayRequestExpireTimer) {
    clearTimeout(displayRequestExpireTimer);
    displayRequestExpireTimer = null;
  }
  try { attentionRepaint(); } catch (_) {}
}

function displayRequestSessionLabel(sessionId) {
  const sid = String(sessionId || '').trim();
  if (!sid) return 'the main session';
  let name = '';
  try {
    const meta = sessionMetadataById.get(sid) || {};
    name = meta.name || meta.display_name || '';
  } catch (_) {}
  const shortId = sid.length > 8 ? sid.slice(0, 8) : sid;
  return name ? `${name} (${shortId})` : `session ${shortId}`;
}

const DISPLAY_REQUEST_ACCESS_COPY = {
  view: {
    label: 'View only',
    detail: 'The agent can see your screen (live stream and frames). It cannot click, type, or take control.',
  },
  view_and_control: {
    label: 'View and control',
    detail: 'The agent can see your screen AND use keyboard/mouse on it (the full user-display grant).',
  },
};

function showDisplayRequest(d) {
  if (typeof processingLogReplay !== 'undefined' && processingLogReplay) return;
  const id = d.id;
  if (id === undefined || id === null) return;
  const access = d.access === 'view_and_control' ? 'view_and_control' : 'view';
  const expiresAt = Number(d.expires_unix_ms || 0);
  if (expiresAt && expiresAt <= Date.now()) return; // stale bootstrap replay

  hideAllPanels();
  pendingDisplayRequest = { id, sessionId: d.session_id || '', access };
  if (pendingDisplayRequest.sessionId) {
    ensureSessionWindow(pendingDisplayRequest.sessionId, { phase: 'waiting' });
    focusSessionWindow(pendingDisplayRequest.sessionId);
  }

  const content = document.getElementById('display-request-content');
  content.innerHTML = '';

  const title = document.createElement('div');
  title.className = 'approval-title';
  title.textContent = 'Agent asks to access your display';
  content.appendChild(title);

  const meta = document.createElement('div');
  meta.className = 'display-request-meta';
  const sessionChip = document.createElement('span');
  sessionChip.className = 'approval-category';
  sessionChip.style.display = '';
  sessionChip.textContent = displayRequestSessionLabel(d.session_id);
  meta.appendChild(sessionChip);
  const accessChip = document.createElement('span');
  accessChip.className = 'approval-category';
  accessChip.style.display = '';
  accessChip.style.marginLeft = '6px';
  accessChip.textContent = DISPLAY_REQUEST_ACCESS_COPY[access].label;
  accessChip.title = DISPLAY_REQUEST_ACCESS_COPY[access].detail;
  meta.appendChild(accessChip);
  content.appendChild(meta);

  // The agent's reason — textContent only, never markup.
  const reason = document.createElement('div');
  reason.className = 'approval-command';
  reason.textContent = String(d.reason || '');
  content.appendChild(reason);

  const accessDetail = document.createElement('div');
  accessDetail.className = 'question-text';
  accessDetail.style.opacity = '0.8';
  accessDetail.textContent = DISPLAY_REQUEST_ACCESS_COPY[access].detail;
  content.appendChild(accessDetail);

  const actions = document.createElement('div');
  actions.className = 'approval-actions';

  const durationSelect = document.createElement('select');
  durationSelect.id = 'display-request-duration';
  durationSelect.title = 'How long the grant lasts if you allow it';
  for (const [value, label] of [
    ['this_session', 'For this session'],
    ['15m', 'For 15 minutes'],
    ['until_revoked', 'Until I revoke it'],
  ]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    durationSelect.appendChild(option);
  }
  actions.appendChild(durationSelect);

  const allow = document.createElement('button');
  allow.className = 'approve';
  allow.textContent = 'Allow';
  allow.addEventListener('click', () => sendDisplayRequestDecision('approve'));
  actions.appendChild(allow);

  const deny = document.createElement('button');
  deny.className = 'deny';
  deny.textContent = 'Deny';
  deny.title = 'Decline; the agent may ask again after a cooldown';
  deny.addEventListener('click', () => sendDisplayRequestDecision('deny'));
  actions.appendChild(deny);

  const denySession = document.createElement('button');
  denySession.textContent = 'Deny for this session';
  denySession.title = 'Decline and suppress further display requests from this session';
  denySession.addEventListener('click', () => sendDisplayRequestDecision('deny_session'));
  actions.appendChild(denySession);

  content.appendChild(actions);

  // Auto-expire locally when the requesting tool stops waiting, so the
  // panel never offers a decision that can no longer land. The server's
  // timeout resolution normally closes it first.
  if (displayRequestExpireTimer) clearTimeout(displayRequestExpireTimer);
  if (expiresAt) {
    displayRequestExpireTimer = setTimeout(() => {
      if (pendingDisplayRequest && String(pendingDisplayRequest.id) === String(id)) {
        clearPendingDisplayRequest();
        hidePanel('display-request-panel');
      }
    }, Math.max(0, expiresAt - Date.now()));
  }

  revealActivityLogPanel();
  document.getElementById('display-request-panel').classList.add('visible');
  setApprovalIndicator(true);
}

window.sendDisplayRequestDecision = function (decision) {
  if (!pendingDisplayRequest) return;
  const msg = {
    action: 'resolve_display_request',
    id: pendingDisplayRequest.id,
    decision,
  };
  if (decision === 'approve') {
    const select = document.getElementById('display-request-duration');
    msg.duration = (select && select.value) || 'until_revoked';
  }
  if (pendingDisplayRequest.sessionId) msg.session_id = pendingDisplayRequest.sessionId;
  dispatchDashboardActionMsg(msg);
  clearPendingDisplayRequest();
  hidePanel('display-request-panel');
};

function handleDisplayRequestRaised(d) {
  showDisplayRequest(d);
}

function handleDisplayRequestResolved(d) {
  // Another dashboard (or the timeout / session end) resolved it.
  if (
    pendingDisplayRequest &&
    String(pendingDisplayRequest.id) === String(d.id) &&
    (!d.session_id || !pendingDisplayRequest.sessionId || d.session_id === pendingDisplayRequest.sessionId)
  ) {
    clearPendingDisplayRequest();
    hidePanel('display-request-panel');
  }
}
