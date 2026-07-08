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
window.sendApproval = function(action) {
  if (!app || pendingApprovalId === null) return;
  if (pendingApprovalSessionId && sessionWindowIsDetached(pendingApprovalSessionId)) {
    showControlToast('error', 'Approval is no longer live; attach the session and retry if needed');
    approvalSessionIds.delete(String(pendingApprovalId));
    clearPendingApproval();
    hidePanel('approval-panel');
    return;
  }
  const msg = { action, id: pendingApprovalId };
  if (pendingApprovalSessionId) msg.session_id = pendingApprovalSessionId;
  dispatchSessionControlMsg(msg);
  approvalSessionIds.delete(String(pendingApprovalId));
  clearPendingApproval();
  hidePanel('approval-panel');
  setPhase('running');
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
  const show = targetSessionId
    ? isSessionWindowEffectivelyActive(targetSessionId) && sessionSupportsInterrupt(targetSessionId)
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
let userDisplayGranted = false;
let grantedDisplayId = 0;
let displayPickerVisible = false;
var cachedDisplays = null;

function hideDisplayPicker() {
  const picker = document.getElementById('display-picker');
  picker.classList.remove('visible');
  displayPickerVisible = false;
}

function showDisplayPicker(displays) {
  const picker = document.getElementById('display-picker');
  picker.innerHTML = '';
  for (const d of displays) {
    const item = document.createElement('div');
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
      hideDisplayPicker();
      if (!app) return;
      dispatchDashboardActionMsg({ action: 'grant_user_display', display_id: d.id });
      grantedDisplayId = d.id;
      setUserDisplayState(true);
    });
    picker.appendChild(item);
  }
  const createItem = document.createElement('div');
  createItem.className = 'display-picker-item dp-action';
  createItem.textContent = 'New virtual display';
  createItem.title = 'Launch a virtual display (Xvfb) on the daemon host — no agent or API key needed. Linux hosts only.';
  createItem.addEventListener('click', (e) => {
    e.stopPropagation();
    hideDisplayPicker();
    createVirtualDisplay();
  });
  picker.appendChild(createItem);
  // ui-v2 hides #status-bar (the picker's v1 anchor subtree), so an
  // in-place open can never render — portal to <body> and pin the popover
  // to the live rail's "Your screen" card (the control that proxies the
  // grant under v2). Single-display hosts grant instantly and never open
  // the picker, which is why this only bit multi-display setups.
  if (typeof ui2Enabled === 'function' && ui2Enabled() && picker.closest('#status-bar')) {
    document.body.appendChild(picker);
  }
  if (typeof ui2Enabled === 'function' && ui2Enabled() && picker.parentElement === document.body) {
    const anchor = document.getElementById('ui2-live-yourscreen');
    const rect = anchor ? anchor.getBoundingClientRect() : null;
    picker.style.position = 'fixed';
    picker.style.zIndex = '95';
    if (rect && rect.width) {
      picker.style.left = `${Math.round(rect.left)}px`;
      picker.style.top = `${Math.round(rect.bottom + 8)}px`;
      picker.style.right = 'auto';
      picker.style.transform = 'none';
    } else {
      picker.style.left = '50%';
      picker.style.top = '96px';
      picker.style.right = 'auto';
      picker.style.transform = 'translateX(-50%)';
    }
  }
  picker.classList.add('visible');
  displayPickerVisible = true;
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

function grantUserDisplayTarget(display) {
  const displayId = Number(display?.id);
  const msg = { action: 'grant_user_display' };
  if (Number.isFinite(displayId) && displayId !== 0) msg.display_id = displayId;
  dispatchDashboardActionMsg(msg);
  grantedDisplayId = Number.isFinite(displayId) ? displayId : 0;
  setUserDisplayState(true);
}

window.cycleAutonomy = function() {
  const levels = ['Low', 'Medium', 'High', 'Full'];
  const el = document.getElementById('sb-autonomy');
  const current = normalizeAutonomyLabel(el.textContent.trim());
  const idx = levels.indexOf(current);
  const next = levels[(idx + 1) % levels.length];
  updateStatusBar({ autonomy: next });
  dispatchControlMsg({ action: 'set_autonomy', level: next.toLowerCase() });
};

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
    removeDisplaySlot(Number(grantedDisplayId));
    dispatchDashboardActionMsg({ action: 'revoke_user_display', display_id: Number(grantedDisplayId) || 0 });
    setUserDisplayState(false);
    return;
  }
  // If we've already enumerated displays this session, use the cached
  // result and short-circuit the fetch. /api/displays runs xrandr
  // against the X server, which blocks behind in-flight XShmGetImage
  // calls from any concurrent capture — so right after a revoke
  // (while the backgrounded capture thread is still joining), the
  // call can take 1-1.5s and stalls the indicator flip by the same
  // margin. Displays rarely change mid-session, so honoring the
  // cache between toggles makes ON#2 as fast as ON#1 without trading
  // correctness for speed: we still fetch on first toggle to
  // populate the cache, and a hotplug that invalidates the cache
  // would surface either as a stale picker entry (benign) or a
  // failed grant that user can retry (also benign).
  const applyDisplayList = (displays) => {
    cachedDisplays = displays;
    const rows = Array.isArray(displays) ? displays : [];
    const physicalDisplays = rows.filter((display) => display?.kind !== 'window');
    if (rows.length <= 1 || physicalDisplays.length === 1) {
      grantUserDisplayTarget(physicalDisplays[0] || rows[0]);
    } else {
      showDisplayPicker(rows);
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
      // Fetch failed: fall back to granting default display
      grantUserDisplayTarget(null);
    });
};
function setUserDisplayState(granted) {
  userDisplayGranted = granted;
  const el = document.getElementById('sb-display-access');
  el.textContent = granted ? 'on' : 'off';
  el.classList.toggle('granted', granted);
}

// Dismiss display picker on click outside or Escape
document.addEventListener('click', (e) => {
  if (!displayPickerVisible) return;
  const picker = document.getElementById('display-picker');
  if (!picker.contains(e.target)) hideDisplayPicker();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && displayPickerVisible) hideDisplayPicker();
});

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
  localStorage.setItem(HOST_FILTER_KEY, activeHostFilter);
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
document.getElementById('sb-dashboard-transport')?.addEventListener('click', () => routeTo('access', 'diagnostics'));
document.getElementById('access-link-daemon-btn')?.addEventListener('click', () => routeTo('access', 'peers'));

// Join-with-an-org-grant: present the pasted document to this daemon. The
// endpoint is public by design (the document is the authorization); using
// authedFetch keeps it working identically on the trusted paths.
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
    const data = await accessOrgCall('api_access_org_present', '/api/access/org-grants', doc);
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
