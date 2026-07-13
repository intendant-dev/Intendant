// ── Keyboard Shortcuts ──
document.addEventListener('keydown', (e) => {
  if (e.target.tagName === 'TEXTAREA' || e.target.tagName === 'INPUT') return;
  if (e.key === 'Escape' && closeSessionWindowMenus()) {
    e.preventDefault();
    return;
  }
  if (e.key === 'Escape' && maximizedSessionWindowId && !shouldDeferSessionWindowEscape()) {
    setSessionWindowMaximized(maximizedSessionWindowId, false);
    e.preventDefault();
    return;
  }
  if (activeTab === 'activity' && app && pendingApprovalId !== null) {
    if (e.key === 'y') { sendApproval('approve'); e.preventDefault(); }
    else if (e.key === 's') { sendApproval('skip'); e.preventDefault(); }
    else if (e.key === 'a') { sendApproval('approve_all'); e.preventDefault(); }
    else if (e.key === 'n') { sendApproval('deny'); e.preventDefault(); }
  }
});

// ── Approval / Input / Follow-up actions (from HTML onclick) ──
// Not fire-and-forget anymore: the panel flips to a disabled "Sending…"
// state and clears only when the resolution is observed (fragment 41's
// beginApprovalSend hooks — attention-set drop on approval_resolved, a
// phase transition out of waiting, or an external panel clear). After 5s
// with no evidence the buttons re-enable and a toast says the approval may
// not have reached the daemon.
window.sendApproval = function(action) {
  if (!app || pendingApprovalId === null) return;
  // A send is already in flight for the shown approval — don't double-fire
  // (keyboard shortcuts stay wired; they just no-op while disabled).
  if (approvalSendPending) return;
  if (pendingApprovalSessionId && sessionWindowIsDetached(pendingApprovalSessionId)) {
    showControlToast('error', 'Approval is no longer live; attach the session and retry if needed');
    approvalSessionIds.delete(String(pendingApprovalId));
    clearPendingApproval();
    hidePanel('approval-panel');
    return;
  }
  const msg = { action, id: pendingApprovalId };
  if (pendingApprovalSessionId) msg.session_id = pendingApprovalSessionId;
  beginApprovalSend(pendingApprovalId, pendingApprovalSessionId);
  let failed = false;
  const failSend = (err) => {
    // Transport said no (sync or async) — don't wait out the ack timeout.
    if (failed) return;
    failed = true;
    resolveApprovalSend();
    showControlToast('error', err?.message || 'Approval could not be sent — retry');
  };
  const sent = dispatchSessionControlMsg(msg, { onError: failSend });
  if (sent === false) failSend(new Error('Approval could not be sent — no daemon connection; retry'));
};
window.sendHumanResponse = function() {
  const input = document.getElementById('human-input');
  const text = input.value.trim();
  if (!text || !app) return;
  processCommands(app.send_human_response(text));
  input.value = '';
};

// Submit (or skip) the structured user-question panel. Answers ride the
// approval rail as {action:'answer_question', id, answers}; Skip sends
// {action:'skip', id}, which the backend maps to a dismissal the agent
// handles gracefully.
window.sendQuestionAnswer = function(opts) {
  if (!app || !pendingQuestion) return;
  const id = pendingQuestion.id;
  const sessionId = pendingQuestion.sessionId;
  if (sessionId && sessionWindowIsDetached(sessionId)) {
    showControlToast('error', 'Question is no longer live; attach the session and retry if needed');
    approvalSessionIds.delete(String(id));
    clearPendingQuestion();
    hidePanel('question-panel');
    return;
  }
  let msg;
  if (opts && opts.skip) {
    msg = { action: 'skip', id };
  } else {
    const collected = collectQuestionAnswers();
    if (!collected) return;
    if (collected.missing) {
      showControlToast('error', `Answer "${collected.missing}" first (or Skip)`);
      return;
    }
    msg = { action: 'answer_question', id, answers: collected.answers };
  }
  if (sessionId) msg.session_id = sessionId;
  dispatchSessionControlMsg(msg);
  approvalSessionIds.delete(String(id));
  clearPendingQuestion();
  hidePanel('question-panel');
  setPhase('running');
};

// Request interruption of the current agent turn. Disables the button and
// flips the label to "Interrupting..." until the phase leaves `interrupting`
// — updateStopButtonVisibility resets the label when it hides/re-enables.
window.sendInterrupt = function() {
  if (!app) return;
  const targetSessionId = resolvePromptTargetSessionId();
  if (targetSessionId && sessionWindowIsDetached(targetSessionId)) {
    showControlToast('error', 'Attach the session before interrupting it');
    updateStopButtonVisibility(currentPhase);
    return;
  }
  const btn = document.getElementById('stop-btn');
  if (btn) {
    btn.disabled = true;
    btn.innerHTML = '\u23F9 Interrupting...';
  }
  const msg = { action: 'interrupt' };
  if (targetSessionId) msg.session_id = targetSessionId;
  dispatchSessionControlMsg(msg);
};

// Show/hide the Stop button based on phase.
// Hide on: idle, done, waiting_followup, interrupted, "" (empty/initial)
// Show on: thinking, running, running_agent, orchestrating, waiting_approval,
//          waiting_human, interrupting
// Phases that mean "the agent is actively doing something the user might
// want to interrupt". Shared between the Stop button and the Control-tab
// "task running" indicator so they stay in sync. Explicit allowlist; a
// denylist would wrongly include `waiting` / `waiting_followup` / `idle`-
// adjacent states that happen between tasks.
function isAgentActivePhase(phase) {
  return AGENT_ACTIVE_PHASES.has(phaseKey(phase));
}

function updateStopButtonVisibility(phase) {
  const btn = document.getElementById('stop-btn');
  if (!btn) return;
  const key = phaseKey(phase);
  const targetSessionId = resolvePromptTargetSessionId();
  // Trust the phase the user is LOOKING at: the per-window activity flag
  // can lag the banner (identity re-keying, optimistic-expiry) and hid
  // Stop during live turns — an unusable interrupt affordance mid-turn is
  // the worst failure mode this button has.
  const show = targetSessionId
    ? sessionSupportsInterrupt(targetSessionId)
      && (isSessionWindowEffectivelyActive(targetSessionId) || isAgentActivePhase(key))
    : isAgentActivePhase(key);
  btn.style.display = show ? '' : 'none';
  // Only `interrupting` should show the disabled "Interrupting..." label.
  // Every other phase (active or hidden) resets the button to its default.
  if (key === 'interrupting') {
    btn.disabled = true;
    btn.innerHTML = '\u23F9 Interrupting...';
  } else {
    btn.disabled = false;
    btn.innerHTML = '\u23F9 Stop';
  }
}

// ── Display metrics (per-display sections) ──
const displayMetricsIds = new Set();
function updateDisplayMetrics(d) {
  const container = document.getElementById('display-metrics-container');
  if (!container) return;
  const did = d.display_id;
  const sectionId = 'display-metrics-' + did;
  let section = document.getElementById(sectionId);
  if (!section) {
    displayMetricsIds.add(did);
    section = document.createElement('div');
    section.id = sectionId;
    section.className = 'cost-section ui-card';
    const head = document.createElement('div');
    head.className = 'ui-section-head';
    const title = document.createElement('div');
    title.className = 'ui-section-title';
    title.id = sectionId + '-title';
    title.textContent = 'Display Transport';
    head.appendChild(title);
    section.appendChild(head);
    const table = document.createElement('table');
    table.className = 'display-metrics-table';
    table.innerHTML =
      '<tbody>' +
      '<tr><td>Capture</td><td class="dm-value" id="' + sectionId + '-capture-fps">&mdash;</td><td class="dm-unit">fps</td></tr>' +
      '<tr><td>Encode</td><td class="dm-value" id="' + sectionId + '-encode-fps">&mdash;</td><td class="dm-unit">fps</td></tr>' +
      '<tr><td>Encode freshness</td><td class="dm-value" id="' + sectionId + '-encode-freshness">&mdash;</td><td class="dm-unit">ms avg</td></tr>' +
      '<tr><td>Peers</td><td class="dm-value" id="' + sectionId + '-peer-count">&mdash;</td><td class="dm-unit"></td></tr>' +
      '<tr><td>Resolution</td><td class="dm-value" id="' + sectionId + '-resolution">&mdash;</td><td class="dm-unit"></td></tr>' +
      '<tr><td>Drops</td><td class="dm-value" id="' + sectionId + '-drops">&mdash;</td><td class="dm-unit">cap / enc / peer</td></tr>' +
      '<tr><td>Tiles</td><td class="dm-value" id="' + sectionId + '-tiles">&mdash;</td><td class="dm-unit">dirty / delta / snap</td></tr>' +
      '</tbody>';
    section.appendChild(table);
    container.appendChild(section);
    // Update all titles when multiple displays exist
    if (displayMetricsIds.size > 1) {
      for (const id of displayMetricsIds) {
        const t = document.getElementById('display-metrics-' + id + '-title');
        if (t) t.textContent = 'Display :' + id;
      }
    }
  }
  document.getElementById(sectionId + '-capture-fps').textContent = d.capture_fps.toFixed(1);
  document.getElementById(sectionId + '-encode-fps').textContent = d.encode_fps.toFixed(1);
  document.getElementById(sectionId + '-encode-freshness').textContent = d.encode_freshness_avg_ms.toFixed(1);
  document.getElementById(sectionId + '-peer-count').textContent = d.peer_count;
  document.getElementById(sectionId + '-resolution').textContent = d.resolution_width + '\u00d7' + d.resolution_height;
  document.getElementById(sectionId + '-drops').textContent = d.capture_drops + ' / ' + d.encode_drops + ' / ' + d.peer_drops;
  const dirtyPct = ((d.tile_dirty_fraction_avg || 0) * 100).toFixed(1) + '%';
  const deltaFps = (d.tile_delta_fps || 0).toFixed(1) + 'fps';
  const deltaKbps = (d.tile_delta_kbps || 0).toFixed(0) + 'kbps';
  const snapKbps = (d.tile_snapshot_kbps || 0).toFixed(0) + 'kbps';
  document.getElementById(sectionId + '-tiles').textContent =
    `${dirtyPct} (${d.tile_dirty_tiles || 0}t) / ${deltaFps} ${deltaKbps} / ${d.tile_snapshot_frames || 0}f ${snapKbps}`;
}

// ── User display access toggle ──
// (userDisplayGranted / grantedDisplayId / userDisplayAgentVisible are
// declared in fragment 32 next to the display maps: earlier fragments
// render against them at load time.)
let displayPickerVisible = false;
let displayPickerReturnFocus = null;
let displayPickerPositionRaf = 0;
let displayPickerDrawerModal = false;
// Which action the open picker performs: 'share' (agent access) or
// 'view' (private remote view; the agent cannot see the display).
let displayPickerMode = 'share';
var cachedDisplays = null;

function hideDisplayPicker(restoreFocus = true) {
  const picker = document.getElementById('display-picker');
  picker.classList.remove('visible');
  picker.setAttribute('aria-hidden', 'true');
  displayPickerVisible = false;
  if (displayPickerDrawerModal) {
    const rail = document.getElementById('ui2-live-rail');
    const railOpen = document.getElementById('tab-displays')?.classList.contains('ui2-live-rail-open');
    if (rail) {
      rail.inert = !railOpen;
      if (railOpen) rail.removeAttribute('aria-hidden');
      else rail.setAttribute('aria-hidden', 'true');
      rail.setAttribute('aria-modal', railOpen ? 'true' : 'false');
    }
    displayPickerDrawerModal = false;
  }
  const returnFocus = displayPickerReturnFocus;
  displayPickerReturnFocus = null;
  if (restoreFocus && returnFocus && returnFocus.isConnected &&
      typeof returnFocus.focus === 'function' && !returnFocus.closest('[inert]')) {
    returnFocus.focus();
  }
}

function positionDisplayPicker() {
  const picker = document.getElementById('display-picker');
  if (!displayPickerVisible || picker.parentElement !== document.body) return;
  const anchor = document.getElementById('ui2-live-yourscreen');
  const rect = anchor ? anchor.getBoundingClientRect() : null;
  const margin = 12;
  const gap = 8;
  const width = picker.offsetWidth;
  const height = picker.offsetHeight;
  picker.style.position = 'fixed';
  picker.style.zIndex = '95';
  picker.style.right = 'auto';
  picker.style.transform = 'none';
  if (rect && rect.width) {
    const maxLeft = Math.max(margin, window.innerWidth - width - margin);
    const left = Math.min(Math.max(margin, rect.left), maxLeft);
    let top = rect.bottom + gap;
    if (top + height > window.innerHeight - margin && rect.top - height - gap >= margin) {
      top = rect.top - height - gap;
    }
    const maxTop = Math.max(margin, window.innerHeight - height - margin);
    picker.style.left = `${Math.round(left)}px`;
    picker.style.top = `${Math.round(Math.min(Math.max(margin, top), maxTop))}px`;
  } else {
    picker.style.left = `${Math.round(Math.max(margin, (window.innerWidth - width) / 2))}px`;
    picker.style.top = `${Math.round(Math.min(96, Math.max(margin, window.innerHeight - height - margin)))}px`;
  }
}

function scheduleDisplayPickerPosition() {
  if (!displayPickerVisible || displayPickerPositionRaf) return;
  displayPickerPositionRaf = requestAnimationFrame(() => {
    displayPickerPositionRaf = 0;
    positionDisplayPicker();
  });
}

function showDisplayPicker(displays, mode) {
  displayPickerMode = mode === 'view' ? 'view' : 'share';
  const picker = document.getElementById('display-picker');
  displayPickerReturnFocus = document.activeElement instanceof HTMLElement
    ? document.activeElement
    : null;
  picker.innerHTML = '';
  picker.setAttribute('role', 'dialog');
  picker.setAttribute('aria-modal', 'false');
  picker.setAttribute('aria-label', displayPickerMode === 'view'
    ? 'Choose a screen for your private dashboard view'
    : 'Choose a screen to share with the agent');
  for (const d of displays) {
    const item = document.createElement('button');
    item.type = 'button';
    item.className = 'display-picker-item';
    const label = d.name + ' (' + d.width + 'x' + d.height + ')';
    item.textContent = label;
    if (d.is_primary) {
      const badge = document.createElement('span');
      badge.className = 'dp-primary';
      badge.textContent = 'primary';
      item.appendChild(badge);
    } else if (d.kind === 'window') {
      const badge = document.createElement('span');
      badge.className = 'dp-primary';
      badge.textContent = 'window';
      item.appendChild(badge);
    }
    item.addEventListener('click', (e) => {
      e.stopPropagation();
      hideDisplayPicker(true);
      if (!app) return;
      grantUserDisplayTarget(d, displayPickerMode !== 'view');
    });
    picker.appendChild(item);
  }
  if (displayPickerMode !== 'view' && virtualDisplaysAvailableNow()) {
    // Virtual displays are agent workspaces — offering one from the
    // private "View this machine" flow would conflate the two concepts.
    const createItem = document.createElement('button');
    createItem.type = 'button';
    createItem.className = 'display-picker-item dp-action';
    createItem.textContent = 'New virtual display';
    createItem.title = 'Launch a virtual display (Xvfb) on the daemon host — no agent or API key needed.';
    createItem.addEventListener('click', (e) => {
      e.stopPropagation();
      hideDisplayPicker(true);
      createVirtualDisplay();
    });
    picker.appendChild(createItem);
  }
  // ui-v2 hides #status-bar (the picker's v1 anchor subtree), so an
  // in-place open can never render — portal to <body> and pin the popover
  // to the live rail's "Your screen" card (the control that proxies the
  // grant under v2). Single-display hosts grant instantly and never open
  // the picker, which is why this only bit multi-display setups.
  if (picker.closest('#status-bar')) {
    document.body.appendChild(picker);
  }
  if (picker.parentElement === document.body) {
    const drawerOpen = document.getElementById('tab-displays')?.classList.contains('ui2-live-rail-open');
    picker.setAttribute('aria-modal', drawerOpen ? 'true' : 'false');
    displayPickerDrawerModal = Boolean(drawerOpen);
    if (displayPickerDrawerModal) {
      const rail = document.getElementById('ui2-live-rail');
      if (rail) {
        rail.inert = true;
        rail.setAttribute('aria-hidden', 'true');
        rail.setAttribute('aria-modal', 'false');
      }
    }
  }
  picker.classList.add('visible');
  picker.setAttribute('aria-hidden', 'false');
  displayPickerVisible = true;
  positionDisplayPicker();
  requestAnimationFrame(() => picker.querySelector('.display-picker-item')?.focus());
}

// Keyless "new virtual display": the daemon launches an Xvfb and registers
// a capture session — the tile arrives via the normal display_ready
// broadcast, and failures come back as display_capture_lost (toasted by
// the slot-less branch of that handler). Works with zero API keys, which
// is what makes a freshly claimed headless box show a display at all.
window.createVirtualDisplay = function(event) {
  if (event?.stopPropagation) event.stopPropagation();
  const sent = dispatchDashboardActionMsg({ action: 'create_virtual_display' }, {
    onError: (err) => {
      if (typeof showControlToast === 'function') {
        showControlToast('error', err?.message || 'Virtual display create failed');
      }
    },
  });
  if (sent && typeof showControlToast === 'function') {
    showControlToast('info', 'Creating virtual display… the tile appears when its stream is ready');
  }
};

function grantUserDisplayTarget(display, agentVisible = true) {
  const displayId = Number(display?.id);
  const msg = { action: 'grant_user_display' };
  if (Number.isFinite(displayId) && displayId !== 0) msg.display_id = displayId;
  // The bare message is the legacy wire shape and means "share with the
  // agent"; only the private view sends the new field.
  if (!agentVisible) msg.agent_visible = false;
  dispatchDashboardActionMsg(msg);
  grantedDisplayId = Number.isFinite(displayId) ? displayId : 0;
  userDisplayIds.add(grantedDisplayId);
  setDisplayAgentVisibility(grantedDisplayId, agentVisible);
  setUserDisplayState(true, agentVisible);
}

// Start a user-display flow in the given mode: 'share' makes the chosen
// display visible to the agent for computer use (the classic grant);
// 'view' opens a private remote view of this machine that the agent
// cannot see. Single physical display grants instantly; multiple open
// the picker in that mode.
function startUserDisplayGrantFlow(mode) {
  if (!app) return;
  const agentVisible = mode !== 'view';
  const applyDisplayList = (displays) => {
    cachedDisplays = displays;
    const rows = Array.isArray(displays) ? displays : [];
    const physicalDisplays = rows.filter((display) => display?.kind !== 'window');
    if (rows.length <= 1 || physicalDisplays.length === 1) {
      grantUserDisplayTarget(physicalDisplays[0] || rows[0], agentVisible);
    } else {
      showDisplayPicker(rows, mode);
    }
  };
  if (cachedDisplays && cachedDisplays.length > 0) {
    applyDisplayList(cachedDisplays);
    return;
  }
  fetchLocalDisplaysPayload()
    .then(normalizeDisplaysPayload)
    .then(applyDisplayList)
    .catch(() => {
      // Fetch failed: fall back to the default display.
      grantUserDisplayTarget(null, agentVisible);
    });
}

// Revoke the active user-display session (share or private view).
function revokeUserDisplayNow() {
  if (!app || !userDisplayGranted) return;
  // Tear the display slot down locally before the round-trip -- see
  // toggleUserDisplay for why (decode-saturated main thread delays the
  // server's revoke broadcast by seconds).
  removeDisplaySlot(Number(grantedDisplayId));
  dispatchDashboardActionMsg({ action: 'revoke_user_display', display_id: Number(grantedDisplayId) || 0 });
  clearDisplayAgentVisibility(Number(grantedDisplayId));
  setUserDisplayState(false);
}
window.revokeUserDisplayNow = revokeUserDisplayNow;

// Upgrade the active private view to an agent share (or start a share
// flow when nothing is active). Never downgrades an existing share.
function shareUserDisplayWithAgent() {
  if (!app) return;
  if (userDisplayGranted && !userDisplayAgentVisible) {
    const msg = { action: 'grant_user_display' };
    if (Number(grantedDisplayId) !== 0) msg.display_id = Number(grantedDisplayId);
    dispatchDashboardActionMsg(msg);
    userDisplayIds.add(Number(grantedDisplayId));
    setDisplayAgentVisibility(Number(grantedDisplayId), true);
    setUserDisplayState(true, true);
    return;
  }
  if (!userDisplayGranted) startUserDisplayGrantFlow('share');
}
window.shareUserDisplayWithAgent = shareUserDisplayWithAgent;
window.startUserDisplayGrantFlow = startUserDisplayGrantFlow;

// Cycle reads the daemon-confirmed level (fragment 41 tracks it off status
// frames / autonomy_changed echoes), not the DOM chip text; the optimistic
// label reverts if no echo confirms within the window. Rapid clicks chain
// off the pending guess so Low→…→Full still takes three clicks.
window.cycleAutonomy = function() {
  const levels = ['Low', 'Medium', 'High', 'Full'];
  const domLevel = document.getElementById('sb-autonomy')?.textContent?.trim() || '';
  const current = normalizeAutonomyLabel(autonomyPendingLevel || lastConfirmedAutonomy || domLevel);
  const idx = levels.indexOf(current);
  const next = levels[(idx + 1) % levels.length];
  autonomyPendingLevel = next;
  updateStatusBar({ autonomy: next }, { optimisticAutonomy: true });
  if (autonomyRevertTimer) clearTimeout(autonomyRevertTimer);
  autonomyRevertTimer = setTimeout(() => {
    autonomyRevertTimer = null;
    if (!autonomyPendingLevel) return;
    autonomyPendingLevel = '';
    // No echo arrived: paint back the last level the daemon actually
    // reported (when we have one) and say so.
    if (lastConfirmedAutonomy && lastConfirmedAutonomy !== normalizeAutonomyLabel(
      document.getElementById('sb-autonomy')?.textContent?.trim() || '')) {
      updateStatusBar({ autonomy: lastConfirmedAutonomy });
      showControlToast('error', 'The daemon did not confirm the autonomy change');
    }
  }, 5000);
  dispatchControlMsg({ action: 'set_autonomy', level: next.toLowerCase() });
};

// The v1 status-bar chip stays a pure on/off toggle with its historical
// agent-share semantics: off -> start a SHARE flow; anything active
// (share or private view) -> revoke. Upgrading a private view to an
// agent share is only ever an explicit button on the ui2 "Your screen"
// card -- never a side effect of clicking a toggle.
window.toggleUserDisplay = function(event) {
  if (!app) return;
  if (event?.stopPropagation) event.stopPropagation();
  if (userDisplayGranted) {
    // Tear the display slot down locally before the round-trip. The
    // server processes the revoke instantly, but when the browser is
    // busy decoding a live WebRTC video (toggling off the very stream
    // it's rendering), the JS main thread is saturated and the
    // inbound `user_display_revoked` broadcast waits behind video-
    // render work for tens of seconds — observed as "button flips but
    // streaming continues" on the peer's guest UI. Mirroring user
    // intent client-side frees the main thread (pc.close() stops the
    // decoder), and the server's subsequent broadcast becomes a
    // confirmation of something we've already done rather than a gate
    // on the UI response. removeDisplaySlot is idempotent — if the
    // broadcast does arrive later, re-running it is a no-op.
    revokeUserDisplayNow();
    return;
  }
  // startUserDisplayGrantFlow reuses the cached /api/displays result
  // between toggles: the route runs xrandr against the X server, which
  // blocks behind in-flight XShmGetImage calls from any concurrent
  // capture — so right after a revoke (while the backgrounded capture
  // thread is still joining), the call can take 1-1.5s and stalls the
  // indicator flip by the same margin. Displays rarely change
  // mid-session; a hotplug surfaces as a stale picker entry (benign)
  // or a failed grant the user can retry (also benign).
  startUserDisplayGrantFlow('share');
};
function setUserDisplayState(granted, agentVisible = true) {
  userDisplayGranted = granted;
  userDisplayAgentVisible = granted ? agentVisible : true;
  const el = document.getElementById('sb-display-access');
  // 'on' keeps its historical meaning: the AGENT has the display. A
  // private view shows 'view' and never lights the granted style.
  el.textContent = granted ? (agentVisible ? 'on' : 'view') : 'off';
  el.classList.toggle('granted', granted && agentVisible);
  el.classList.toggle('view-only', granted && !agentVisible);
}

// Dismiss display picker on click outside or Escape
document.addEventListener('click', (e) => {
  if (!displayPickerVisible) return;
  const picker = document.getElementById('display-picker');
  if (!picker.contains(e.target)) hideDisplayPicker(false);
});
document.addEventListener('keydown', (e) => {
  if (!displayPickerVisible) return;
  const picker = document.getElementById('display-picker');
  if (e.key === 'Escape') {
    // Capture owns the first Escape so annotation/callout/drawer layers
    // underneath cannot consume the same keystroke.
    e.preventDefault();
    e.stopImmediatePropagation();
    hideDisplayPicker(true);
    return;
  }
  if (e.key !== 'Tab' || picker.getAttribute('aria-modal') !== 'true') return;
  const focusable = Array.from(picker.querySelectorAll('button:not([disabled])'))
    .filter(element => element.getClientRects().length > 0);
  if (!focusable.length) return;
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  if (e.shiftKey && (document.activeElement === first || !picker.contains(document.activeElement))) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && (document.activeElement === last || !picker.contains(document.activeElement))) {
    e.preventDefault();
    first.focus();
  }
}, true);
window.addEventListener('resize', scheduleDisplayPickerPosition);
document.addEventListener('scroll', scheduleDisplayPickerPosition, true);

// Rollback modal: Escape closes, click on the backdrop (outside the
// dialog) also closes — follows the same dismissal pattern as the
// display picker so keyboard flows feel consistent.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  const modal = document.getElementById('rollback-modal');
  if (modal && modal.style.display !== 'none') {
    closeRollbackModal();
    return;
  }
  const fsPickerModal = document.getElementById('fs-picker-modal');
  if (fsPickerModal && fsPickerModal.style.display !== 'none') {
    closeFsPicker();
    return;
  }
  const sessionRenameModal = document.getElementById('session-rename-modal');
  if (sessionRenameModal && sessionRenameModal.style.display !== 'none') {
    closeSessionRenameModal();
    return;
  }
  const sessionConfigModal = document.getElementById('session-config-modal');
  if (sessionConfigModal && sessionConfigModal.style.display !== 'none') {
    closeSessionConfigModal();
    return;
  }
  const sessionDeleteModal = document.getElementById('session-delete-modal');
  if (sessionDeleteModal && sessionDeleteModal.style.display !== 'none') {
    closeSessionDeleteModal();
    return;
  }
  const sessionDelegateModal = document.getElementById('session-delegate-modal');
  if (sessionDelegateModal && sessionDelegateModal.style.display !== 'none') {
    closeSessionDelegateModal();
    return;
  }
  const worktreeInspectModal = document.getElementById('worktree-inspect-modal');
  if (worktreeInspectModal && worktreeInspectModal.style.display !== 'none') {
    closeWorktreeInspectModal();
    return;
  }
  const dashboardConfirmModal = document.getElementById('dashboard-confirm-modal');
  if (dashboardConfirmModal && dashboardConfirmModal.style.display !== 'none') {
    closeDashboardConfirmModal(false);
    return;
  }
  const dashboardPromptModal = document.getElementById('dashboard-prompt-modal');
  if (dashboardPromptModal && dashboardPromptModal.style.display !== 'none') {
    closeDashboardPromptModal(null);
  }
});
document.getElementById('rollback-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'rollback-modal') closeRollbackModal();
});
document.getElementById('fs-picker-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'fs-picker-modal') closeFsPicker();
});
document.getElementById('session-rename-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'session-rename-modal') closeSessionRenameModal();
});
document.getElementById('session-rename-name')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    saveSessionRenameModal();
  }
});
document.getElementById('session-config-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'session-config-modal') closeSessionConfigModal();
});
document.getElementById('session-delete-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'session-delete-modal') closeSessionDeleteModal();
});
document.getElementById('session-delegate-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'session-delegate-modal') closeSessionDelegateModal();
});
document.getElementById('worktree-inspect-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'worktree-inspect-modal') closeWorktreeInspectModal();
});
document.getElementById('dashboard-confirm-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'dashboard-confirm-modal') closeDashboardConfirmModal(false);
});
document.getElementById('dashboard-confirm-cancel')?.addEventListener('click', () => closeDashboardConfirmModal(false));
document.getElementById('dashboard-confirm-accept')?.addEventListener('click', () => {
  closeDashboardConfirmModal(dashboardConfirmPending?.confirmValue ?? true);
});
document.getElementById('dashboard-confirm-alternate')?.addEventListener('click', () => {
  closeDashboardConfirmModal(dashboardConfirmPending?.alternateValue ?? 'alternate');
});
document.getElementById('dashboard-prompt-modal')?.addEventListener('click', (e) => {
  if (e.target && e.target.id === 'dashboard-prompt-modal') closeDashboardPromptModal(null);
});
document.getElementById('dashboard-prompt-cancel')?.addEventListener('click', () => closeDashboardPromptModal(null));
document.getElementById('dashboard-prompt-submit')?.addEventListener('click', () => {
  const input = document.getElementById('dashboard-prompt-input');
  closeDashboardPromptModal(input ? input.value : '');
});
document.getElementById('dashboard-prompt-input')?.addEventListener('keydown', (e) => {
  const multiline = e.currentTarget?.dataset?.multiline !== 'false';
  if (e.key === 'Enter' && (!multiline || e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    closeDashboardPromptModal(e.currentTarget.value || '');
  }
});

// Enter to submit
document.querySelector('#human-input')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    window.sendHumanResponse();
  }
});
// Enter uses the same phase-aware dispatcher as the button so keyboard
// users get the same Send vs Steer behavior. Shift+Enter is left to the
// textarea so multiline task instructions work naturally.
wireTaskTextarea('activity-task-input', () => submitActivityOrSteer());
wireTaskTextarea('new-session-input', () => startNewSession());
updateTaskTargetChip();

// Verbosity toggle
document.getElementById('verbosity-select').addEventListener('change', (e) => {
  applyDashboardVerbosity(e.target.value);
});

// Host filter (Activity tab)
document.getElementById('host-filter-select').addEventListener('change', (e) => {
  activeHostFilter = e.target.value;
  // Guarded like the neighboring filter persistence: Safari private mode
  // throws on setItem and would kill the handler before applying.
  try { localStorage.setItem(HOST_FILTER_KEY, activeHostFilter); } catch (_) {}
  applyHostFilter();
});

// Stats host picker — switches between self and each secondary.
document.getElementById('stats-host-select').addEventListener('change', (e) => {
  switchStatsHost(e.target.value);
});

// Hosts aggregate dot (and its surrounding label group) in the status
// bar - click jumps to Access targets so the user can investigate
// which host is disconnected/skewed.
document.getElementById('sb-hosts-group').addEventListener('click', () => {
  if (DASHBOARD_ACCESS_PAGE_MODE) routeTo('access', 'daemons');
  else window.location.href = accessHomeHref('daemons');
});
document.getElementById('sb-access-page-link')?.addEventListener('click', openAccessHome);
document.getElementById('sb-dashboard-transport')?.addEventListener('click', () => openConnectionDiagnostics());
document.getElementById('access-link-daemon-btn')?.addEventListener('click', () => routeTo('access', 'peers'));

// Join-with-an-org-grant: present the pasted document to this daemon. The
// HTTP row is public by design (the document is the authorization); the
// facade rides the tunnel when one is bound and the public row otherwise.
document.getElementById('access-org-join-btn')?.addEventListener('click', async () => {
  const status = document.getElementById('access-org-join-status');
  const raw = document.getElementById('access-org-join-doc')?.value?.trim();
  if (!raw) {
    if (status) status.textContent = 'Paste a signed org grant document first.';
    return;
  }
  let doc;
  try { doc = JSON.parse(raw); } catch {
    if (status) status.textContent = 'That is not valid JSON.';
    return;
  }
  // Keep the document even if this daemon refuses it (it may simply not
  // trust the org yet) — future offers to org daemons carry it.
  const stored = orgGrantStore(doc);
  if (status) status.textContent = 'Presenting…';
  try {
    const data = await accessOrgCall('api_access_org_present', doc);
    const summary = data.peer_identity
      ? `peer profile ${data.peer_identity.profile} for ${data.peer_identity.label} until ${new Date(Number(data.peer_identity.expires_at_unix || 0) * 1000).toLocaleString()}`
      : `${String(data.grant?.role_id || '').replace(/^role:/, '')} until ${new Date(Number(data.grant?.expires_at_unix_ms || 0)).toLocaleString()}`;
    if (status) status.textContent = `Granted via @${data.org_handle} — ${summary}.${stored ? ' Kept for automatic presentation on future connections.' : ''}`;
    showControlToast?.('success', 'Org grant accepted');
  } catch (err) {
    const kept = stored ? ' The document is kept in this browser and will be presented automatically once a daemon trusts the org.' : '';
    if (status) status.textContent = `${err?.message || 'Presentation failed.'}${kept}`;
  }
  await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
  renderAccessAdminSummaries();
});

// Warm the browser identity key early: transports sign offers with it, and
// the Access renderers read the cache synchronously once it resolves.
if (clientIdentitySupported()) {
  clientIdentityGet().then(identity => {
    if (identity) renderAccessAdminSummaries();
  }).catch(() => {});
}
// Carry published org revocations to this daemon shortly after load —
// after the control transport has had a chance to come up in connect mode.
setTimeout(() => { orgRevocationCourier().catch(() => {}); }, 4000);
syncAccessPageNavLink();

// Auto-scroll detection
(function() {
  const stream = document.getElementById('log-stream');
  const btn = document.getElementById('scroll-bottom');
  stream.addEventListener('scroll', () => {
    const atBottom = stream.scrollHeight - stream.scrollTop - stream.clientHeight < 30;
    autoScroll = atBottom;
    btn.classList.toggle('visible', !atBottom);
    if (atBottom) resetMainLogNewBelow();
  }, { passive: true });
  btn.addEventListener('click', () => {
    stream.scrollTop = stream.scrollHeight;
    autoScroll = true; btn.classList.remove('visible');
    resetMainLogNewBelow();
  });
})();

// Toggle display strip — header click or button click
document.getElementById('activity-display-strip').querySelector('.activity-display-header')
  .addEventListener('click', toggleDisplayStrip);
document.getElementById('strip-minimize')?.addEventListener('click', (e) => {
  e.stopPropagation();
  setDisplayStripMinimized(!stripMinimized);
});

// Drag handle for display strip resize
(function() {
  const handle = document.getElementById('activity-split-handle');
  const strip = document.getElementById('activity-display-strip');
  let dragging = false;
  let startY = 0;
  let startH = 0;

  handle.addEventListener('mousedown', (e) => {
    if (!stripExpanded || stripMinimized) return;
    dragging = true;
    startY = e.clientY;
    startH = strip.offsetHeight;
    handle.classList.add('dragging');
    document.body.style.cursor = 'row-resize';
    document.body.style.userSelect = 'none';
    e.preventDefault();
  });

  document.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const delta = e.clientY - startY;
    const newH = Math.max(100, Math.min(startH + delta, window.innerHeight * 0.7));
    strip.style.height = newH + 'px';
    stripHeight = newH;
  });

  document.addEventListener('mouseup', () => {
    if (!dragging) return;
    dragging = false;
    handle.classList.remove('dragging');
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
  });
})();

// ── Start ──
main();
