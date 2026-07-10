// ── Attention center: pending agent→user requests ──────────────────────
//
// A generic set of "attention items" — requests from the agent that block
// on the human — fed from the same server events the panels render
// (approval_required / user_question / approval_resolved, plus session
// terminations), keyed by (kind, session, id) so future kinds (display
// requests, agent notify) drop in cheaply.
//
// Surfaces, in escalation order:
//   1. document-title prefix `(N)` + favicon count badge — default ON
//      (harmless), toggleable. Supersedes fragment 41's single-approval
//      indicator (`setApprovalIndicator` now delegates here): this set
//      counts every pending request across sessions instead of tracking
//      one panel.
//   2. a browser Notification when an item arrives while the tab is
//      hidden — default OFF; permission is requested only from the
//      explicit settings toggle, never on load. Click focuses the tab and
//      the owning session. Bursts debounce into one notification.
//   3. (not this file) closed tabs entirely: the daemon nudges the Connect
//      rendezvous and opted-in browsers get a Web Push
//      (src/bin/caller/attention_nudge.rs, src/bin/connect/push.rs).
//
// Wire-in points (fragment 36): the server-message dispatcher calls
// attentionObserveServerMessage(d) after its dedupe check, and the WASM
// server-state callback calls attentionOnServerState(connected).

const ATTENTION_BADGE_KEY = 'intendant.attention.badge'; // 'off' disables; default on
const ATTENTION_NOTIFY_KEY = 'intendant.attention.notify'; // 'on' enables; default off
const ATTENTION_NOTIFY_DEBOUNCE_MS = 1500;

// key "kind:sessionKey:id" -> { kind, sessionId, id }
const attentionItems = new Map();
// Keys already announced via Notification in this page's lifetime — a WS
// flap (clear + bootstrap re-send of still-pending asks) must not
// re-notify about the same request.
const attentionNotifiedKeys = new Set();
let attentionTitleBase = null;
let attentionTitleComposed = null;
let attentionNotifyTimer = null;
let attentionNotifyPending = [];
let attentionOpenNotifications = [];

function attentionBadgeEnabled() {
  try { return localStorage.getItem(ATTENTION_BADGE_KEY) !== 'off'; } catch (_) { return true; }
}

function attentionNotifySupported() {
  return typeof Notification !== 'undefined';
}

function attentionNotifyEnabled() {
  if (!attentionNotifySupported()) return false;
  try {
    return localStorage.getItem(ATTENTION_NOTIFY_KEY) === 'on'
      && Notification.permission === 'granted';
  } catch (_) { return false; }
}

function attentionSessionKey(sessionId) {
  return (sessionId && String(sessionId)) || 'main';
}

function attentionKey(kind, sessionId, id) {
  return `${kind}:${attentionSessionKey(sessionId)}:${String(id)}`;
}

function attentionAdd(kind, sessionId, id, live) {
  if (id === undefined || id === null) return;
  const key = attentionKey(kind, sessionId, id);
  const isNew = !attentionItems.has(key);
  attentionItems.set(key, { kind, sessionId: sessionId || '', id });
  if (isNew && live && document.hidden && !attentionNotifiedKeys.has(key)) {
    attentionNotifiedKeys.add(key);
    attentionQueueNotification(key);
  }
  attentionRepaint();
}

function attentionRemove(kind, sessionId, id) {
  attentionItems.delete(attentionKey(kind, sessionId, id));
}

function attentionClearSession(sessionId) {
  const sessionKey = attentionSessionKey(sessionId);
  for (const [key, item] of [...attentionItems]) {
    if (attentionSessionKey(item.sessionId) === sessionKey) attentionItems.delete(key);
  }
}

function attentionClearAll() {
  attentionItems.clear();
  attentionRepaint();
}

// One observer for every server event, live or replayed. Replay
// (log_replay bootstrap) rebuilds the set silently — no notifications for
// history; the daemon's still-pending re-sends arrive as ordinary live
// events and take the normal path.
function attentionObserveServerMessage(d) {
  if (!d || typeof d !== 'object') return;
  if (d.t === 'log_replay' && Array.isArray(d.entries)) {
    for (const entry of d.entries) attentionApplyEvent(entry, false);
    attentionRepaint();
    return;
  }
  attentionApplyEvent(d, true);
}

function attentionApplyEvent(d, live) {
  const ev = d && d.event;
  if (!ev) return;
  if (ev === 'approval_required') {
    attentionAdd('approval', d.session_id, d.id, live);
  } else if (ev === 'user_question') {
    attentionAdd('question', d.session_id, d.id, live);
  } else if (ev === 'display_request_raised') {
    // The user-display doorbell: its ids live in their own registry
    // (never the approval id space), so the 'display' kind prefix keys
    // them apart from approvals/questions with the same number.
    attentionAdd('display', d.session_id, d.id, live);
  } else if (ev === 'display_request_resolved') {
    attentionRemove('display', d.session_id, d.id);
    attentionRepaint();
  } else if (ev === 'user_notification') {
    // Fire-and-forget notifications register only for the escalated
    // urgencies, only live (history never badges), and only while the tab
    // is hidden — a visible tab already rendered the toast. Unlike pending
    // requests nothing "resolves" them: they self-clear when the tab next
    // becomes visible (see the visibilitychange hook below).
    const urgency = d.urgency || 'info';
    if ((urgency === 'attention' || urgency === 'urgent') && live && document.hidden) {
      attentionAdd('notify', d.session_id, d.id, live);
    }
  } else if (ev === 'approval_resolved') {
    // Approvals and questions share the id space; resolve either.
    attentionRemove('approval', d.session_id, d.id);
    attentionRemove('question', d.session_id, d.id);
    attentionRepaint();
  } else if (ev === 'task_complete' || ev === 'interrupted') {
    // The blocked loop returned — approvals/questions in that session no
    // longer wait (some exit paths skip approval_resolved). A display
    // request survives: its waiter is the blocked MCP call, not the turn;
    // it clears via display_request_resolved or session_ended.
    const sessionKey = attentionSessionKey(d.session_id);
    for (const [key, item] of [...attentionItems]) {
      if (attentionSessionKey(item.sessionId) === sessionKey && item.kind !== 'display') {
        attentionItems.delete(key);
      }
    }
    attentionRepaint();
  } else if (ev === 'session_ended') {
    attentionClearSession(d.session_id);
    attentionRepaint();
  }
}

// Notifications deliver by being seen: when the tab becomes visible the
// toast/transcript row is on screen, so their attention items retire.
function attentionClearNotifyKind() {
  let changed = false;
  for (const [key, item] of [...attentionItems]) {
    if (item.kind === 'notify') { attentionItems.delete(key); changed = true; }
  }
  if (changed) attentionRepaint();
}

// Event-stream connection state (fragment 36's set_on_server_state): a
// dead stream can't retract items, so a stale badge would lie — clear and
// let the reconnect bootstrap rebuild what is still pending.
function attentionOnServerState(connected) {
  if (!connected) attentionClearAll();
}

// ── Title + favicon badge ──

function attentionRepaint() {
  const count = attentionBadgeEnabled() ? attentionItems.size : 0;
  // Re-capture the base whenever someone else last wrote the title.
  if (attentionTitleBase === null || document.title !== attentionTitleComposed) {
    attentionTitleBase = document.title.replace(/^\(\d+\+?\)\s+/, '');
  }
  const composed = count > 0 ? `(${count > 99 ? '99+' : count}) ${attentionTitleBase}` : attentionTitleBase;
  if (document.title !== composed) document.title = composed;
  attentionTitleComposed = composed;
  attentionPaintFavicon(count);
}

let attentionFaviconLastCount = 0;
function attentionPaintFavicon(count) {
  if (count === attentionFaviconLastCount) return;
  attentionFaviconLastCount = count;
  if (typeof _swapFavicon !== 'function') return;
  if (count === 0) { _swapFavicon('/icon-128.png'); return; }
  const size = 64;
  const canvas = document.createElement('canvas');
  canvas.width = size; canvas.height = size;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  const drawBadge = () => {
    const label = count > 9 ? '9+' : String(count);
    const r = size * 0.30, cx = size - r - 1, cy = size - r - 1;
    ctx.beginPath(); ctx.arc(cx, cy, r + 3, 0, 2 * Math.PI);
    ctx.fillStyle = '#1e1e2e'; ctx.fill();
    ctx.beginPath(); ctx.arc(cx, cy, r, 0, 2 * Math.PI);
    ctx.fillStyle = '#f38ba0'; ctx.fill();
    ctx.fillStyle = '#11111b';
    ctx.font = `bold ${label.length > 1 ? 22 : 27}px system-ui, sans-serif`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(label, cx, cy + 1);
    // Only install if a request is still pending by the time the base
    // image resolved.
    try { if (attentionFaviconLastCount > 0) _swapFavicon(canvas.toDataURL('image/png')); } catch (_) {}
  };
  const img = new Image();
  img.onload = () => { try { ctx.drawImage(img, 0, 0, size, size); } catch (_) {} drawBadge(); };
  img.onerror = () => { ctx.fillStyle = '#313244'; ctx.fillRect(0, 0, size, size); drawBadge(); };
  img.src = '/icon-128.png';
}

// ── Hidden-tab Notifications ──

function attentionQueueNotification(key) {
  if (!attentionNotifyEnabled()) return;
  attentionNotifyPending.push(key);
  if (attentionNotifyTimer) return;
  attentionNotifyTimer = setTimeout(() => {
    attentionNotifyTimer = null;
    const keys = attentionNotifyPending.splice(0);
    // Only announce what is still pending after the debounce window.
    const items = keys.map((k) => attentionItems.get(k)).filter(Boolean);
    if (!items.length || !document.hidden || !attentionNotifyEnabled()) return;
    attentionShowNotification(items);
  }, ATTENTION_NOTIFY_DEBOUNCE_MS);
}

function attentionShowNotification(items) {
  const approvals = items.filter((i) => i.kind === 'approval').length;
  const questions = items.filter((i) => i.kind === 'question').length;
  const displayRequests = items.filter((i) => i.kind === 'display').length;
  const notifies = items.filter((i) => i.kind === 'notify').length;
  let title;
  if (items.length === 1) {
    title = questions ? 'Intendant: the agent has a question'
      : displayRequests ? 'Intendant: agent asks to view your screen'
      : notifies ? 'Intendant: the agent sent a notification'
      : 'Intendant: approval needed';
  } else {
    const parts = [];
    if (approvals) parts.push(`${approvals} approval${approvals > 1 ? 's' : ''}`);
    if (questions) parts.push(`${questions} question${questions > 1 ? 's' : ''}`);
    if (displayRequests) parts.push(`${displayRequests} display request${displayRequests > 1 ? 's' : ''}`);
    if (notifies) parts.push(`${notifies} notification${notifies > 1 ? 's' : ''}`);
    title = `Intendant: ${parts.join(' and ')} waiting`;
  }
  const total = attentionItems.size;
  const body = total > items.length
    ? `${total} requests are waiting for you.`
    : notifies === items.length
      ? 'The agent wants your attention.'
      : 'The agent is waiting for you.';
  const focusSessionId = items[0].sessionId || '';
  try {
    const notification = new Notification(title, {
      body,
      // One stacked notification per dashboard: later bursts replace it.
      tag: 'intendant-attention',
      icon: '/icon-128.png',
    });
    notification.onclick = () => {
      try { window.focus(); } catch (_) {}
      if (focusSessionId && typeof focusSessionWindow === 'function') {
        try { focusSessionWindow(focusSessionId); } catch (_) {}
      }
      notification.close();
    };
    attentionOpenNotifications.push(notification);
  } catch (_) {
    // Constructor can throw on some platforms (e.g. Android Chrome
    // requires ServiceWorker notifications) — the badge still stands.
  }
}

document.addEventListener('visibilitychange', () => {
  if (!document.hidden) {
    for (const notification of attentionOpenNotifications.splice(0)) {
      try { notification.close(); } catch (_) {}
    }
    attentionClearNotifyKind();
  }
});

// ── Settings card (badge + notification toggles; browser-local) ──

function attentionToggleRow(labelText, subText, checkbox) {
  const row = document.createElement('div');
  row.className = 'settings-row attention-toggle-row';
  const label = document.createElement('label');
  label.style.display = 'flex';
  label.style.alignItems = 'center';
  label.style.gap = '8px';
  label.style.cursor = 'pointer';
  checkbox.type = 'checkbox';
  label.appendChild(checkbox);
  const meta = document.createElement('span');
  const title = document.createElement('span');
  title.textContent = labelText;
  const sub = document.createElement('small');
  sub.textContent = subText;
  sub.style.display = 'block';
  sub.style.opacity = '0.7';
  meta.append(title, sub);
  label.appendChild(meta);
  row.appendChild(label);
  return row;
}

function attentionBuildSettingsCard() {
  const card = document.createElement('section');
  card.className = 'ui-card attention-settings-card';
  const head = document.createElement('div');
  head.className = 'ui-section-head';
  const h3 = document.createElement('h3');
  h3.className = 'ui-section-title';
  h3.textContent = 'Notifications';
  const sub = document.createElement('div');
  sub.className = 'ui-section-sub';
  sub.textContent = 'When the agent needs you — approvals, questions, and agent notifications. These apply to this browser only.';
  head.append(h3, sub);
  card.appendChild(head);

  const badgeBox = document.createElement('input');
  badgeBox.id = 'attention-badge-toggle';
  badgeBox.checked = attentionBadgeEnabled();
  badgeBox.addEventListener('change', () => {
    try { localStorage.setItem(ATTENTION_BADGE_KEY, badgeBox.checked ? 'on' : 'off'); } catch (_) {}
    attentionRepaint();
  });
  card.appendChild(attentionToggleRow(
    'Tab alert badge',
    'Prefix the tab title and favicon with the number of pending requests.',
    badgeBox,
  ));

  const notifyBox = document.createElement('input');
  notifyBox.id = 'attention-notify-toggle';
  const hint = document.createElement('p');
  hint.className = 'settings-note';
  if (!attentionNotifySupported()) {
    notifyBox.disabled = true;
    hint.textContent = 'Browser notifications are unavailable in this context (they need a secure origin such as https or localhost).';
  } else {
    notifyBox.checked = attentionNotifyEnabled();
    hint.textContent = 'Shown when a request arrives while this tab is hidden; click one to jump to the session. For alerts when no tab is open at all, enable request push on your Connect account page (intendant.dev → Notifications).';
  }
  notifyBox.addEventListener('change', async () => {
    if (!notifyBox.checked) {
      try { localStorage.setItem(ATTENTION_NOTIFY_KEY, 'off'); } catch (_) {}
      return;
    }
    // Permission is requested HERE and only here — an explicit user act.
    let permission = Notification.permission;
    if (permission === 'default') {
      try { permission = await Notification.requestPermission(); } catch (_) { permission = 'denied'; }
    }
    if (permission === 'granted') {
      try { localStorage.setItem(ATTENTION_NOTIFY_KEY, 'on'); } catch (_) {}
    } else {
      notifyBox.checked = false;
      hint.textContent = 'Notification permission was not granted. Allow notifications for this site in your browser, then try again.';
    }
  });
  card.appendChild(attentionToggleRow(
    'Desktop notifications',
    'Notify when a request arrives while this tab is hidden.',
    notifyBox,
  ));
  card.appendChild(hint);
  return card;
}

// Mount: the v2 Appearance pane holds browser-local preferences (built by
// ui2-settings.js, which evaluates before this fragment); v1 falls back to
// the Account pane body.
(function attentionMountSettingsCard() {
  try {
    const host = document.getElementById('settings-pane-appearance')
      || document.querySelector('#settings-pane-account .settings-pane-body');
    if (host) host.appendChild(attentionBuildSettingsCard());
  } catch (_) {}
})();
