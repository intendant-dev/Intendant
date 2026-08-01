// ── Attention center: pending agent→user requests ──────────────────────
//
// A generic set of "attention items" — requests from the agent that block
// on the human — fed from the same server events the panels render
// (approval_required / user_question / approval_resolved, plus session
// terminations), keyed by (kind, session, id) so future kinds (display
// requests, agent notify, the coordination radar's retractable overlap
// flag) drop in cheaply.
//
// Surfaces, in escalation order:
//   1. document-title prefix `(N)` + favicon count badge — default ON
//      (harmless), toggleable — and their always-on on-page twin, the
//      oversight-bar inbox chip + popover, which names each item (kind,
//      session, timestamp) and deep-links to its handling surface. One
//      map renders all of them: the title count and the inbox are two
//      renderings of the same set, never two sets. Supersedes fragment
//      41's single-approval indicator (`setApprovalIndicator` now
//      delegates here).
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

// key "kind:host:sessionKey:id" -> { kind, sessionId, hostId, id, ts, title, text }
// (`ts` is the item's arrival time in ms — the wire `ts` when the event
// carries one, else receipt time; `title`/`text` are kept for `notify`
// items so the inbox can still name the cause after the toast expired.)
const attentionItems = new Map();
// Retired items, newest first: a short tail so a badge that just cleared
// (answered on another device, session ended, notification acknowledged)
// stays explainable from the inbox instead of vanishing without a trace.
// Live retirements only — replayed history and transport-loss clears of
// items the bootstrap will replay never land here.
const ATTENTION_RECENT_MAX = 20;
const attentionRecent = [];
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

// Keys carry a host dimension (RC-C2): peer approvals share the rail,
// and approval ids are per-daemon counters that collide across daemons
// — without the host a session-less peer approval and a session-less
// local one with the same id would merge into one item.
function attentionKey(kind, sessionId, id, hostId) {
  return `${kind}:${hostId || 'local'}:${attentionSessionKey(sessionId)}:${String(id)}`;
}

function attentionTailPush(item, reason) {
  attentionRecent.unshift({ ...item, retiredAt: Date.now(), reason });
  if (attentionRecent.length > ATTENTION_RECENT_MAX) attentionRecent.length = ATTENTION_RECENT_MAX;
}

function attentionAdd(kind, sessionId, id, live, meta) {
  if (id === undefined || id === null) return;
  const hostId = (meta && meta.hostId) || '';
  const key = attentionKey(kind, sessionId, id, hostId);
  const prior = attentionItems.get(key);
  const wireTs = meta && Number(meta.ts);
  attentionItems.set(key, {
    kind,
    sessionId: sessionId || '',
    hostId,
    id,
    ts: (prior && prior.ts)
      || (Number.isFinite(wireTs) && wireTs > 0 ? wireTs : Date.now()),
    title: (meta && meta.title) || (prior && prior.title) || '',
    text: (meta && meta.text) || (prior && prior.text) || '',
  });
  if (!prior && live && document.hidden && !attentionNotifiedKeys.has(key)) {
    attentionNotifiedKeys.add(key);
    attentionQueueNotification(key);
  }
  attentionRepaint();
}

// Removal driven by a wire event (`live` false while rebuilding from
// replayed history, which never feeds the recent tail). Peer removals
// (RC-C2) may arrive session-blind — `peer_approval_resolved` carries
// only (host, id) — so a host-scoped removal with a null sessionId
// scans for the item instead of keying directly.
function attentionRemove(kind, sessionId, id, live, reason, hostId) {
  const host = hostId || '';
  if (host && (sessionId === undefined || sessionId === null)) {
    const sid = String(id);
    for (const [key, item] of [...attentionItems]) {
      if (item.kind === kind && item.hostId === host && String(item.id) === sid) {
        attentionItems.delete(key);
        if (live) attentionTailPush(item, reason || 'resolved');
      }
    }
    attentionRepaint();
    return;
  }
  const key = attentionKey(kind, sessionId, id, host);
  const item = attentionItems.get(key);
  if (!item) return;
  attentionItems.delete(key);
  if (live) attentionTailPush(item, reason || 'resolved');
}

function attentionClearSession(sessionId, live) {
  const sessionKey = attentionSessionKey(sessionId);
  for (const [key, item] of [...attentionItems]) {
    // Local-lane sweep: peer items (RC-C2) retire on their own peer
    // events / link transitions, never on local session lifecycle.
    if (!item.hostId && attentionSessionKey(item.sessionId) === sessionKey) {
      attentionItems.delete(key);
      if (live) attentionTailPush(item, 'session ended');
    }
  }
}

function attentionClearAll() {
  // Transport loss, not resolution: pending requests and radar flags drop
  // silently — the reconnect bootstrap replays whatever still stands.
  // Escalated notifications have no daemon-side replay line, so they
  // retire to the recent tail instead of vanishing without a trace.
  for (const [key, item] of [...attentionItems]) {
    attentionItems.delete(key);
    if (item.kind === 'notify') attentionTailPush(item, 'connection lost');
  }
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
    attentionRemove('display', d.session_id, d.id, live, 'resolved');
    attentionRepaint();
  } else if (ev === 'user_notification') {
    // Fire-and-forget notifications register only for the escalated
    // urgencies and only live (history never badges) — visible tab
    // included: the toast is a 4.5s single slot, so without an inbox item
    // an escalated notification had no enduring surface at all. Nothing
    // on the wire "resolves" them: each retires when the user clicks or
    // dismisses it in the attention inbox — never on tab visibility,
    // which used to erase the badge before its cause was findable. The
    // wire event's title/text/ts ride along so the inbox can name it.
    const urgency = d.urgency || 'info';
    if ((urgency === 'attention' || urgency === 'urgent') && live) {
      attentionAdd('notify', d.session_id, d.id, live, { ts: d.ts, title: d.title, text: d.text });
    }
  } else if (ev === 'coordination_radar') {
    // Collision-radar flag (Track C §2.8): retractable BY DESIGN —
    // 'raised' adds, 'resolved' retracts the same (session, id) item,
    // which is why the radar has its own kind instead of riding
    // fire-and-forget notifications. Like 'display', the flag's
    // lifecycle is the radar's, not the turn's.
    if (d.state === 'raised') {
      attentionAdd('radar', d.session_id, d.id, live);
    } else if (d.state === 'resolved') {
      attentionRemove('radar', d.session_id, d.id, live, 'resolved');
      attentionRepaint();
    }
  } else if (ev === 'approval_resolved') {
    // Approvals and questions share the id space; resolve either.
    attentionRemove('approval', d.session_id, d.id, live, 'resolved');
    attentionRemove('question', d.session_id, d.id, live, 'answered');
    attentionRepaint();
  } else if (ev === 'task_complete' || ev === 'interrupted') {
    // The blocked loop returned — approvals/questions in that session no
    // longer wait (some exit paths skip approval_resolved). A display
    // request survives: its waiter is the blocked MCP call, not the turn;
    // it clears via display_request_resolved or session_ended. A radar
    // flag survives for the same reason: the files still overlap until
    // the radar says 'resolved' (or session_ended). A notification
    // survives too: the turn returning says nothing about the user having
    // seen it — it retires only from the inbox. (Session-less agenda
    // reminders share the 'main' session key, so this sweep used to erase
    // them whenever the main session finished any turn.)
    const sessionKey = attentionSessionKey(d.session_id);
    for (const [key, item] of [...attentionItems]) {
      if (!item.hostId
          && attentionSessionKey(item.sessionId) === sessionKey
          && item.kind !== 'display' && item.kind !== 'radar' && item.kind !== 'notify') {
        attentionItems.delete(key);
        if (live) attentionTailPush(item, 'turn ended');
      }
    }
    attentionRepaint();
  } else if (ev === 'session_ended') {
    attentionClearSession(d.session_id, live);
    attentionRepaint();
  }
}

// Event-stream connection state (fragment 36's set_on_server_state): a
// dead stream can't retract items, so a stale badge would lie — clear and
// let the reconnect bootstrap rebuild what is still pending.
function attentionOnServerState(connected) {
  if (!connected) {
    attentionClearAll();
    return;
  }
  // Local-WS reconnect (RC-C2): the clear dropped peer approval items,
  // but their source of truth — the browser-side peer fold — survived a
  // local transport flap. Rebuild them; peer-link transitions own their
  // real invalidation (clearPeerApprovalsForHost).
  if (typeof peerPendingApprovals !== 'undefined') {
    for (const [hostId, pending] of peerPendingApprovals) {
      for (const [id, entry] of pending) {
        attentionAdd('approval', entry.sessionId || '', id, false, {
          hostId,
          text: entry.command || '',
        });
      }
    }
  }
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
  attentionPaintChip();
  if (attentionInboxOpen) attentionRenderInbox();
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

// ── Attention inbox (oversight-bar chip + popover) ──
//
// The on-page rendering of the same `attentionItems` map the title badge
// counts: the chip is the count's always-visible twin, and the popover
// lists every live item — kind label, session, timestamp — with a
// deep-link to its handling surface, plus the recent tail of retired
// items. Markup skeleton lives in ui2-shell.html; styles in
// ui2-chrome.css.

const ATTENTION_KIND_LABELS = {
  approval: 'Approval',
  question: 'Question',
  display: 'Display request',
  notify: 'Notification',
  radar: 'Overlap alert',
};

let attentionInboxOpen = false;

function attentionPaintChip() {
  const countEl = document.getElementById('ui2-attention-count');
  if (!countEl) return;
  // The chip always shows the real set size: the badge toggle governs the
  // tab title/favicon, never the in-page inbox.
  const count = attentionItems.size;
  countEl.hidden = count === 0;
  countEl.textContent = count > 99 ? '99+' : String(count);
}

function attentionSessionLabel(item) {
  const sid = String(item.sessionId || '').trim();
  // Peer items (RC-C2): name the governing daemon — the session id is
  // the PEER's and means nothing to local session metadata. Explicit
  // target provenance on every peer-attributed item.
  if (item.hostId) {
    let label = item.hostId;
    try {
      const entry = typeof peerEntryForHost === 'function' ? peerEntryForHost(item.hostId) : null;
      if (entry && entry.label) label = entry.label;
    } catch (_) {}
    return sid ? `on ${label} · ${sid.slice(0, 8)}` : `on ${label}`;
  }
  // Session-less notifications are the agenda scheduler's (reminders,
  // scheduled-session outcomes) — name their home, not a session.
  if (!sid) return item.kind === 'notify' ? 'Agenda' : 'main session';
  let name = '';
  try {
    const meta = sessionMetadataById.get(sid) || {};
    name = meta.name || meta.display_name || '';
  } catch (_) {}
  return name || `session ${sid.length > 8 ? sid.slice(0, 8) : sid}`;
}

function attentionAgoLabel(ms) {
  const s = Math.max(0, Math.round((Date.now() - Number(ms)) / 1000));
  if (s < 45) return 'just now';
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

function attentionDefaultPrimary(kind) {
  if (kind === 'approval') return 'Approval needed';
  if (kind === 'question') return 'The agent has a question';
  if (kind === 'display') return 'Agent asks to view your screen';
  if (kind === 'radar') return 'Sessions overlapping on files';
  return 'Notification';
}

// Deep-link: land the user on the item's handling surface. The bottom
// panels force-surface themselves and their pump owns which one is up —
// never call showPanel/hideAllPanels here (both clear pending ask state).
function attentionNavigate(item) {
  attentionInboxSetOpen(false);
  // Peer approvals (RC-C2): the handling surface is the merged approval
  // panel — surface that entry (or its queue slot) and reveal the panel.
  // The session id is the PEER's; never resolve it against local windows.
  if (item.hostId) {
    const pending = typeof peerPendingApprovals !== 'undefined'
      ? peerPendingApprovals.get(item.hostId)
      : null;
    const entry = pending ? pending.get(String(item.id)) : null;
    if (entry && typeof showApproval === 'function') {
      showApproval(item.id, entry.command, entry.category, entry.sessionId || '', item.hostId);
      if (typeof routeTo === 'function') { try { routeTo('activity'); } catch (_) {} }
    } else if (typeof routeTo === 'function') {
      // Not in the fold anymore (resolved elsewhere / link reset) — the
      // peer row panel is the fallback surface.
      try { routeTo('access'); } catch (_) {}
    }
    return;
  }
  const sid = String(item.sessionId || '').trim();
  if (item.kind === 'notify') {
    // Click IS the acknowledgement — retire to the recent tail, then go
    // where the cause lives.
    attentionRetire(item, 'clicked');
  }
  if (sid) {
    if (typeof focusSessionWindow === 'function') {
      try { focusSessionWindow(sid); } catch (_) {}
    }
    if (item.kind === 'question' && typeof pendingQuestion !== 'undefined'
        && pendingQuestion && String(pendingQuestion.id) === String(item.id)) {
      // A tucked-away question restores; an untucked one is already up.
      try { setQuestionMinimized(false); } catch (_) {}
    }
    return;
  }
  // Session-less: agenda notifications and parked asks live on the
  // Agenda tab.
  if ((item.kind === 'notify' || item.kind === 'question') && typeof routeTo === 'function') {
    try { routeTo('agenda'); } catch (_) {}
  }
}

// Inbox-driven retirement (notify click/dismiss): the one removal path
// that is a user act rather than a wire event.
function attentionRetire(item, reason) {
  const key = attentionKey(item.kind, item.sessionId, item.id, item.hostId);
  const found = attentionItems.get(key);
  if (!found) return;
  attentionItems.delete(key);
  attentionTailPush(found, reason);
  attentionRepaint();
}

function attentionInboxItem(item, recent) {
  const wrap = document.createElement('div');
  wrap.className = 'ui2-attn-item';
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'ui2-attn-row';
  row.dataset.kind = item.kind;
  const kindEl = document.createElement('span');
  kindEl.className = 'ui2-attn-kind';
  kindEl.textContent = ATTENTION_KIND_LABELS[item.kind] || item.kind;
  const meta = document.createElement('span');
  meta.className = 'ui2-attn-meta';
  const primary = document.createElement('span');
  primary.className = 'ui2-attn-primary';
  primary.textContent = item.title || attentionDefaultPrimary(item.kind);
  const secondary = document.createElement('span');
  secondary.className = 'ui2-attn-secondary';
  const when = recent
    ? `${item.reason} · ${attentionAgoLabel(item.retiredAt)}`
    : attentionAgoLabel(item.ts);
  const where = attentionSessionLabel(item);
  secondary.textContent = item.text ? `${item.text} — ${where} · ${when}` : `${where} · ${when}`;
  secondary.title = secondary.textContent;
  meta.append(primary, secondary);
  row.append(kindEl, meta);
  row.addEventListener('click', () => {
    if (recent) {
      // A tail entry has nothing left to retire but still navigates —
      // the cleared cause may need follow-up.
      attentionInboxSetOpen(false);
      const sid = String(item.sessionId || '').trim();
      if (item.hostId) {
        if (typeof routeTo === 'function') { try { routeTo('access'); } catch (_) {} }
      } else if (sid && typeof focusSessionWindow === 'function') {
        try { focusSessionWindow(sid); } catch (_) {}
      } else if (item.kind === 'notify' && typeof routeTo === 'function') {
        try { routeTo('agenda'); } catch (_) {}
      }
      return;
    }
    attentionNavigate(item);
  });
  wrap.appendChild(row);
  if (!recent && item.kind === 'notify') {
    const dismiss = document.createElement('button');
    dismiss.type = 'button';
    dismiss.className = 'ui2-attn-dismiss';
    dismiss.title = 'Dismiss this notification';
    dismiss.innerHTML = typeof ui2Icon === 'function' ? ui2Icon('close', 12) : '&#215;';
    dismiss.addEventListener('click', (e) => {
      e.stopPropagation();
      attentionRetire(item, 'dismissed');
    });
    wrap.appendChild(dismiss);
  }
  return wrap;
}

function attentionRenderInbox() {
  const list = document.getElementById('ui2-attention-list');
  if (!list) return;
  const items = [...attentionItems.values()].sort((a, b) => b.ts - a.ts);
  const popCount = document.getElementById('ui2-attention-pop-count');
  if (popCount) {
    popCount.hidden = items.length === 0;
    popCount.textContent = String(items.length);
  }
  list.textContent = '';
  if (!items.length) {
    const empty = document.createElement('div');
    empty.className = 'ui2-attn-empty';
    empty.textContent = 'Nothing needs you right now.';
    list.appendChild(empty);
  }
  for (const item of items) list.appendChild(attentionInboxItem(item, false));
  const recentHead = document.getElementById('ui2-attention-recent-head');
  const recentList = document.getElementById('ui2-attention-recent');
  if (recentHead) recentHead.hidden = attentionRecent.length === 0;
  if (recentList) {
    recentList.hidden = attentionRecent.length === 0;
    recentList.textContent = '';
    for (const entry of attentionRecent) recentList.appendChild(attentionInboxItem(entry, true));
  }
}

function attentionInboxSetOpen(open) {
  const pop = document.getElementById('ui2-attention-pop');
  if (!pop) return;
  attentionInboxOpen = open;
  pop.hidden = !open;
  const btn = document.getElementById('ui2-attention-btn');
  if (btn) {
    btn.classList.toggle('open', open);
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
  }
  if (open) attentionRenderInbox();
}

(function attentionWireInbox() {
  const btn = document.getElementById('ui2-attention-btn');
  if (!btn) return;
  const icon = document.getElementById('ui2-attention-icon');
  if (icon && typeof ui2Icon === 'function') icon.innerHTML = ui2Icon('bell', 15);
  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    attentionInboxSetOpen(!attentionInboxOpen);
  });
  const clear = document.getElementById('ui2-attention-recent-clear');
  if (clear) {
    clear.addEventListener('click', () => {
      attentionRecent.length = 0;
      attentionRenderInbox();
    });
  }
  document.addEventListener('mousedown', (e) => {
    if (!attentionInboxOpen) return;
    const pop = document.getElementById('ui2-attention-pop');
    if (pop && !pop.contains(e.target) && !btn.contains(e.target)) {
      attentionInboxSetOpen(false);
    }
  });
  // Capture phase, like the palette: while the popover is open it owns
  // Escape and nothing leaks into the v1 Escape cascade.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && attentionInboxOpen) {
      e.preventDefault();
      e.stopPropagation();
      attentionInboxSetOpen(false);
    }
  }, true);
  attentionPaintChip();
})();

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
  const radars = items.filter((i) => i.kind === 'radar').length;
  let title;
  if (items.length === 1) {
    title = questions ? 'Intendant: the agent has a question'
      : displayRequests ? 'Intendant: agent asks to view your screen'
      : notifies ? 'Intendant: the agent sent a notification'
      : radars ? 'Intendant: sessions are overlapping on files'
      : 'Intendant: approval needed';
  } else {
    const parts = [];
    if (approvals) parts.push(`${approvals} approval${approvals > 1 ? 's' : ''}`);
    if (questions) parts.push(`${questions} question${questions > 1 ? 's' : ''}`);
    if (displayRequests) parts.push(`${displayRequests} display request${displayRequests > 1 ? 's' : ''}`);
    if (notifies) parts.push(`${notifies} notification${notifies > 1 ? 's' : ''}`);
    if (radars) parts.push(`${radars} overlap alert${radars > 1 ? 's' : ''}`);
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

// Focusing the tab closes any open desktop Notifications — but retires
// nothing: `notify` attention items leave the set only from the inbox
// (click or dismiss), so the badge keeps a findable target.
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) {
    for (const notification of attentionOpenNotifications.splice(0)) {
      try { notification.close(); } catch (_) {}
    }
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
