// ── ui-v2 Activity slice 1: approval-card semantics (user-approved) ────
// Under the flag, the approval card's bulk action becomes CATEGORY-
// SCOPED: "Approve all <category>" sets that category's rule to auto via
// the shipped set_approval_rule machinery, then approves the pending
// command. The old approve_all (which flips autonomy to Full) stays
// available but relabeled to say what it does. The `a` hotkey follows
// the new semantics under the flag (capture phase; v1 handler untouched
// and still the default without the flag).

function ui2ApprovalCategory() {
  // stationCurrentApproval is the cross-module source of truth for the
  // pending approval; the panel's category line is the fallback. Normalize
  // to the daemon's lowercase ActionCategory ids — set_approval_rule's
  // parser is exact-match ("destructive", not "Destructive").
  const fromGlobal = (typeof stationCurrentApproval !== 'undefined' && stationCurrentApproval &&
    (stationCurrentApproval.category || stationCurrentApproval.action_category)) || '';
  if (fromGlobal) return String(fromGlobal).trim().toLowerCase();
  const el = document.getElementById('approval-category');
  const m = /([a-z_]+)\s*$/i.exec((el && el.textContent) || '');
  return m ? m[1].toLowerCase() : '';
}

function ui2ApproveCategoryRule() {
  const category = ui2ApprovalCategory();
  if (category && typeof dispatchControlMsg === 'function') {
    dispatchControlMsg({ action: 'set_approval_rule', category, rule: 'auto' });
    // Re-pull the shared control state so an already-open Control pane /
    // Settings autonomy section shows the new rule (they otherwise
    // refresh only on open — pull-based by design).
    setTimeout(() => {
      if (typeof refreshControlPane === 'function') refreshControlPane();
    }, 400);
  }
  sendApproval('approve');
}

function ui2AugmentApprovalPanel() {
  const actions = document.querySelector('#approval-panel .approval-actions');
  if (!actions || document.querySelector('.ui2-approve-category')) return;
  const buttons = [...actions.querySelectorAll('button')];
  const allBtn = buttons.find((b) => /approve all/i.test(b.textContent));
  const approveBtn = buttons.find((b) => b.classList.contains('approve'));

  const catBtn = document.createElement('button');
  catBtn.type = 'button';
  catBtn.className = 'ui2-approve-category';
  catBtn.innerHTML = 'Approve all like this <kbd>a</kbd>';
  catBtn.title = 'Set this approval category to auto-approve (a shipped per-category rule), then approve this command. Narrower than switching autonomy.';
  catBtn.addEventListener('click', ui2ApproveCategoryRule);
  if (approveBtn && approveBtn.nextSibling) actions.insertBefore(catBtn, approveBtn.nextSibling);
  else actions.appendChild(catBtn);

  if (allBtn) {
    allBtn.classList.add('ui2-full-escape');
    allBtn.innerHTML = 'Switch to Full autonomy';
    allBtn.title = 'The previous "Approve all": flips autonomy to Full — everything runs unattended from here.';
  }

  // `a` follows the category semantics under the flag. Capture phase so
  // the v1 shortcut handler (which would call approve_all → Full) never
  // sees the key while an approval is pending.
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'a' || e.metaKey || e.ctrlKey || e.altKey) return;
    const tag = (e.target && e.target.tagName) || '';
    if (/INPUT|TEXTAREA|SELECT/.test(tag) || (e.target && e.target.isContentEditable)) return;
    const panel = document.getElementById('approval-panel');
    if (!panel || getComputedStyle(panel).display === 'none') return;
    e.preventDefault();
    e.stopPropagation();
    ui2ApproveCategoryRule();
  }, true);
}

// ── slice 2: Focus/Grid layout, composer dressing, vitals rail ─────────

// Layout state lives on <html> (data-ui2-layout; absent/focus = default)
// so CSS is the single reader. Switching remounts the concurrent stream:
// under grid+minimized v1 parks it in a detached fragment, and Focus must
// always show it (shouldDetachConcurrentLogStream carries the guard).
function ui2ApplyLayout(mode) {
  const grid = mode === 'grid';
  document.documentElement.dataset.ui2Layout = grid ? 'grid' : 'focus';
  try { localStorage.setItem('intendant.ui2.layout', grid ? 'grid' : 'focus'); } catch (e) { /* private mode */ }
  const fBtn = document.getElementById('ui2-layout-focus-btn');
  const gBtn = document.getElementById('ui2-layout-grid-btn');
  if (fBtn) fBtn.setAttribute('aria-pressed', String(!grid));
  if (gBtn) gBtn.setAttribute('aria-pressed', String(grid));
  if (typeof syncConcurrentLogStreamMount === 'function') syncConcurrentLogStreamMount();
  if (grid && typeof fitSessionWindowGridHeight === 'function') fitSessionWindowGridHeight();
  if (typeof ui2ApplyFocusFilter === 'function') ui2ApplyFocusFilter();
}

function ui2WireLayoutToggle() {
  let mode = 'focus';
  try { if (localStorage.getItem('intendant.ui2.layout') === 'grid') mode = 'grid'; } catch (e) { /* private mode */ }
  ui2ApplyLayout(mode);
  for (const id of ['ui2-layout-focus-btn', 'ui2-layout-grid-btn']) {
    const btn = document.getElementById(id);
    if (btn) btn.addEventListener('click', () => ui2ApplyLayout(btn.dataset.layout));
  }
}

// Composer dressing: placeholder copy + the attach glyph. The send
// button's TEXT is state (Send / ↗ Steer / Save & rerun) — left alone.
function ui2DressComposer() {
  const input = document.getElementById('activity-task-input');
  if (input) input.placeholder = 'Reply, steer, or describe a new task…';
  const attach = document.getElementById('upload-attach-btn');
  if (attach && typeof ui2Icon === 'function') attach.innerHTML = ui2Icon('attach', 15);
}

// ── Focus filter: Focus shows the FOREGROUND session's timeline ────────
// The original product decision: "Focus timeline + session switcher".
// Entries from other sessions hide; daemon-level entries (no session id)
// always show. No target selected ("all sessions") = the combined
// stream. Grid mode never filters — the concurrent view is combined by
// definition.
function ui2ApplyFocusFilter() {
  const stream = document.getElementById('log-stream');
  if (!stream) return;
  const focusMode = document.documentElement.dataset.ui2Layout !== 'grid';
  const sid = (focusMode && typeof resolvePromptTargetSessionId === 'function'
    && resolvePromptTargetSessionId()) || '';
  stream.querySelectorAll('.log-entry').forEach((e) => {
    const esid = e.dataset.sessionId || '';
    e.classList.toggle('hidden-by-focus', !!sid && !!esid && esid !== sid);
  });
}

let ui2FocusFilterQueued = false;
function ui2QueueFocusFilter() {
  if (ui2FocusFilterQueued) return;
  ui2FocusFilterQueued = true;
  requestAnimationFrame(() => {
    ui2FocusFilterQueued = false;
    ui2ApplyFocusFilter();
    ui2TagRawEntries();
  });
}

// Raw-payload hygiene: backend stderr lines render as compact, dimmed
// mono rows instead of full-voice paragraphs.
function ui2TagRawEntries() {
  const stream = document.getElementById('log-stream');
  if (!stream) return;
  stream.querySelectorAll('.log-entry:not(.ui2-raw-checked)').forEach((e) => {
    e.classList.add('ui2-raw-checked');
    const text = e.querySelector('.log-content')?.textContent || '';
    if (/^\s*\[(codex|claude(-code)?|backend) stderr\]/i.test(text)) {
      e.classList.add('ui2-stderr');
    }
  });
}

// The approval hero names its session — with several agents running,
// "Approval needed" alone is ambiguous.
function ui2StampApprovalSession() {
  const panel = document.getElementById('approval-panel');
  if (!panel || !panel.classList.contains('visible')) return;
  const title = panel.querySelector('.approval-title');
  if (!title) return;
  let chip = document.getElementById('ui2-approval-session');
  if (!chip) {
    chip = document.createElement('span');
    chip.id = 'ui2-approval-session';
    chip.className = 'ui2-approval-session';
    title.appendChild(chip);
  }
  const sid = (typeof pendingApprovalSessionId !== 'undefined' && pendingApprovalSessionId) || '';
  if (!sid) { chip.hidden = true; return; }
  chip.hidden = false;
  let label = sid.slice(0, 8);
  if (typeof sessionIdentityParts === 'function') {
    const parts = sessionIdentityParts(sid) || {};
    label = parts.name || parts.shortId || label;
  }
  chip.textContent = label;
  if (typeof applySessionBadgeStyle === 'function') applySessionBadgeStyle(chip, sid);
}

// ── vitals rail ────────────────────────────────────────────────────────

function ui2RailSetRow(id, text, titleLines, state) {
  const row = document.getElementById(id);
  if (!row) return;
  const value = row.querySelector('.ui2-rail-value');
  if (value) value.textContent = text || '—';
  row.title = Array.isArray(titleLines) ? titleLines.join('\n') : '';
  if (state) row.dataset.state = state;
  else delete row.dataset.state;
}

function ui2BuildVitalsRail() {
  const pane = document.getElementById('activity-log-pane');
  if (!pane || document.getElementById('ui2-vitals-rail')) return;
  const rail = document.createElement('aside');
  rail.id = 'ui2-vitals-rail';
  rail.setAttribute('aria-label', 'Session vitals');
  rail.innerHTML = `
    <div class="ui2-rail-session" id="ui2-rail-session">No session yet</div>
    <div class="ui2-rail-row" id="ui2-rail-git"><span class="ui2-rail-label">Working tree</span><span class="ui2-rail-value">—</span></div>
    <div class="ui2-rail-row" id="ui2-rail-ctx"><span class="ui2-rail-label">Context budget</span><span class="ui2-rail-value">—</span><div class="ui2-rail-meter"><i id="ui2-rail-ctx-fill"></i></div></div>
    <div class="ui2-rail-row" id="ui2-rail-cache"><span class="ui2-rail-label">Prompt cache</span><span class="ui2-rail-value">—</span></div>
    <div class="ui2-rail-row" id="ui2-rail-limits"><span class="ui2-rail-label">Rate limits</span><span class="ui2-rail-value">—</span></div>
    <button type="button" class="ui2-rail-row ui2-rail-changes" id="ui2-rail-changes" title="Open the Changes sub-tab"><span class="ui2-rail-label">Changes</span><span class="ui2-rail-value">—</span></button>
    <button type="button" class="ui2-rail-advanced" id="ui2-rail-advanced" title="Raw state and observers live on the Debug tab">Advanced &amp; raw state</button>`;
  pane.appendChild(rail);
  const changes = document.getElementById('ui2-rail-changes');
  if (changes) changes.addEventListener('click', () => {
    const btn = document.querySelector('#activity-subtabs [data-activity-tab="changes"]');
    if (btn) btn.click();
  });
  const advanced = document.getElementById('ui2-rail-advanced');
  if (advanced) advanced.addEventListener('click', () => {
    if (typeof routeTo === 'function') routeTo('debug');
    else if (typeof switchTab === 'function') switchTab('debug');
  });
}

// The rail describes the FOREGROUND session — prompt target, then the
// current session, then the daemon's own. Same fallback chain the v1
// composer targeting uses.
function ui2RailForegroundSessionId() {
  if (typeof resolvePromptTargetSessionId === 'function') {
    const sid = resolvePromptTargetSessionId();
    if (sid) return sid;
  }
  if (typeof currentSessionFullId !== 'undefined' && currentSessionFullId) return currentSessionFullId;
  if (typeof daemonSessionFullId !== 'undefined' && daemonSessionFullId) return daemonSessionFullId;
  return '';
}

function ui2RailTick(force) {
  const rail = document.getElementById('ui2-vitals-rail');
  if (!rail) return;
  // hidden: other tab/subtab, grid layout, or <1180px — skip the interval
  // work, but let the build-time call fill values so the rail never shows
  // placeholder dashes on its first paint.
  if (!force && !rail.offsetParent) return;

  const sid = ui2RailForegroundSessionId();
  const sessionEl = document.getElementById('ui2-rail-session');
  if (sessionEl) {
    let label = 'No session yet';
    if (sid) {
      label = sid.slice(0, 10);
      if (typeof sessionIdentityParts === 'function') {
        const parts = sessionIdentityParts(sid) || {};
        label = parts.name || parts.shortId || label;
      }
    }
    sessionEl.textContent = label;
    sessionEl.title = sid;
  }

  const meta = (sid && typeof sessionMetadataById !== 'undefined' && sessionMetadataById.get(sid)) || {};
  const vitals = meta.vitals || null;

  const git = (vitals && typeof sessionVitalsGitSegment === 'function')
    ? sessionVitalsGitSegment(vitals.git) : null;
  ui2RailSetRow('ui2-rail-git', git ? git.text : '', git ? git.titleLines : null,
    git && git.conflict ? 'crit' : null);

  const cache = (vitals && typeof sessionVitalsCacheSegment === 'function')
    ? sessionVitalsCacheSegment(vitals.cache) : null;
  // cold = no cache yet — neutral, not an alarm (rose on a fresh session
  // read as a fault); expiring joins crit like v1's red.
  ui2RailSetRow('ui2-rail-cache', cache ? cache.text : '', cache ? cache.titleLines : null,
    cache ? (/cache-crit|cache-expiring/.test(cache.cls) ? 'crit' : /cache-warn/.test(cache.cls) ? 'warn' : /cache-cold/.test(cache.cls) ? null : 'ok') : null);

  const limits = (vitals && typeof sessionVitalsLimitsSegment === 'function')
    ? sessionVitalsLimitsSegment(vitals.limits) : null;
  ui2RailSetRow('ui2-rail-limits', limits ? limits.text : '', limits ? limits.titleLines : null,
    limits ? (limits.severity || null) : null);

  // Context budget mirrors the status-bar meter (same source the
  // oversight-bar chip reads).
  const pctSrc = document.getElementById('sb-budget-pct');
  const pctText = pctSrc ? (pctSrc.textContent || '').trim() : '';
  ui2RailSetRow('ui2-rail-ctx', pctText || '—', null, null);
  const fill = document.getElementById('ui2-rail-ctx-fill');
  if (fill) fill.style.width = Math.max(0, Math.min(100, parseFloat(pctText) || 0)) + '%';

  // Changes count mirrors the sub-tab badge.
  const badge = document.getElementById('badge-changes');
  const badgeShown = badge && badge.style.display !== 'none' && (badge.textContent || '').trim();
  ui2RailSetRow('ui2-rail-changes', badgeShown ? `${badgeShown.trim()} — view diff` : 'none yet', null, null);
}

{
  const wire = () => {
    ui2AugmentApprovalPanel();
    ui2WireLayoutToggle();
    ui2DressComposer();
    ui2BuildVitalsRail();
    ui2RailTick(true);
    setInterval(() => ui2RailTick(), 1000);
    // Focus filter + raw-entry hygiene: follow stream appends, target
    // changes (the chip re-renders on every change), and layout flips.
    const stream = document.getElementById('log-stream');
    if (stream) new MutationObserver(ui2QueueFocusFilter).observe(stream, { childList: true });
    const chip = document.getElementById('task-target-chip');
    if (chip) new MutationObserver(ui2QueueFocusFilter).observe(chip, {
      childList: true, characterData: true, subtree: true, attributes: true,
    });
    ui2ApplyFocusFilter();
    ui2TagRawEntries();
    // Approval hero session identity.
    const panel = document.getElementById('approval-panel');
    if (panel) new MutationObserver(ui2StampApprovalSession).observe(panel, {
      attributes: true, attributeFilter: ['class'],
    });
  };
  // Every fragment shares ONE <script type="module"> scope (30-module-open),
  // which executes with readyState already 'interactive' — an immediate
  // wire() here would run mid-module and reach let-bindings later fragments
  // haven't initialized yet (ui2ApplyLayout → syncConcurrentLogStreamMount →
  // concurrentLogDetachedFragment, still in its TDZ), and the throw would
  // kill every fragment after this one. For a static module script,
  // DOMContentLoaded always fires after the whole module completes — wire
  // there, never inline.
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}
