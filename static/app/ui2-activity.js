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
    allBtn.title = 'The previous "Approve all": flips daemon autonomy to Full. Native explicit Deny rules and hard consent gates remain; external-agent policy has separate caveats.';
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
  // A layout flip moves the Arrange trigger (and leaving Grid hides the
  // whole Arrange surface) — fully close the menu so its stamped anchor
  // and aria-expanded never go stale (no-op while closed).
  ui2CloseArrangeMenu();
  const fBtn = document.getElementById('ui2-layout-focus-btn');
  const gBtn = document.getElementById('ui2-layout-grid-btn');
  if (fBtn) fBtn.setAttribute('aria-pressed', String(!grid));
  if (gBtn) gBtn.setAttribute('aria-pressed', String(grid));
  if (typeof syncConcurrentLogStreamMount === 'function') syncConcurrentLogStreamMount();
  // applySessionWindowGridHeight is the APPLIER (sets --session-window-grid-height,
  // the .resized class, and — via syncSessionWindowGridControls — unhides the drag
  // handle and stamps the pane's concurrent-log mode classes).
  // fitSessionWindowGridHeight, which used to be called here, is a pure getter: it
  // returns a number and touches nothing, so entering Grid left the grid unsized and
  // its controls unsynced until some unrelated path happened to apply them.
  if (grid && typeof applySessionWindowGridHeight === 'function') applySessionWindowGridHeight();
  if (typeof ui2ApplyFocusSurface === 'function') ui2ApplyFocusSurface();
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

// ── The "Arrange ▾" menu: every bulk session-window sweep ──────────────
// One compact toolbar button replacing the five standalone pills that had
// accumulated beside the layout toggle (Minimize done, Hide done,
// Collapse all/Expand all, Expand/Collapse details), plus the new
// "Collapse sub-agents" sweep. Chrome consolidation only: every row
// delegates to 41-session-window-actions.js's existing sweeps —
//   expand-all / collapse-all → setAllSessionWindowsMinimized
//   collapse-subs             → minimizeSubagentWindows (every sub-agent
//                               window, working or finished; top-level
//                               windows untouched)
//   minimize-done             → minimizeDoneSubagentWindows
//   hide-done                 → hideDoneSessionWindows
//   details                   → setAllSessionWindowHeadersCollapsed
// Grid-only like the pills it replaces (Focus hides the windows the
// sweeps act on) via the stylesheet layout gate, and hidden while no
// session window exists. Freshness: 40's badge refreshes and the
// relationship render pass call the same (rAF-deduped) refresher the
// pills rode, which now derives every row in one pass.

// One walk over sessionWindows for every count the menu renders (each
// retired pill used to run its own walk on the same freshness ticks).
// Predicates are 41/40's shared boundaries: sessionWindowIsSubagent (the
// relationship kind the badges use), sessionWindowIsDoneSubagent (the
// "N sub" active boundary), sessionWindowHasHardDoneEvidence (ended /
// done / interrupted — never bare idle).
function ui2ArrangeMenuModel() {
  const model = {
    windows: 0, expanded: 0, minimized: 0,
    subs: 0, subsExpanded: 0, doneSubsExpanded: 0,
    hardDone: 0, detailsExpanded: 0,
  };
  if (typeof sessionWindows === 'undefined') return model;
  const isSub = typeof sessionWindowIsSubagent === 'function' ? sessionWindowIsSubagent : null;
  const isDoneSub = typeof sessionWindowIsDoneSubagent === 'function' ? sessionWindowIsDoneSubagent : null;
  const isHardDone = typeof sessionWindowHasHardDoneEvidence === 'function' ? sessionWindowHasHardDoneEvidence : null;
  for (const [sid, win] of sessionWindows) {
    if (!win) continue;
    model.windows += 1;
    if (win.minimized) model.minimized += 1;
    else {
      model.expanded += 1;
      if (!win.headerCollapsed) model.detailsExpanded += 1;
    }
    if (isSub && isSub(sid)) {
      model.subs += 1;
      if (!win.minimized) {
        model.subsExpanded += 1;
        if (isDoneSub && isDoneSub(sid)) model.doneSubsExpanded += 1;
      }
    }
    if (isHardDone && isHardDone(sid)) model.hardDone += 1;
  }
  return model;
}

function ui2ArrangeSetRow(id, spec) {
  const row = document.getElementById(id);
  if (!row) return;
  row.hidden = !!spec.hidden;
  row.disabled = !!spec.disabled;
  if (spec.title) row.title = spec.title;
  if (spec.label) {
    const labelEl = row.querySelector('.ui2-arrange-item-label');
    if (labelEl) labelEl.textContent = spec.label;
  }
  if (spec.direction) row.dataset.direction = spec.direction;
  const countEl = row.querySelector('.ui2-arrange-count');
  if (countEl) {
    const show = typeof spec.count === 'number' && spec.count > 0;
    countEl.hidden = !show;
    countEl.textContent = String(show ? spec.count : 0);
  }
}

// The single-pass refresh behind every freshness tick. Row policy: the
// universal axes (windows, details) stay visible and disable when their
// direction has nothing to act on, so the menu never jumps around; the
// situational sweeps (sub-agents, finished-window hygiene) hide outright
// while their concept is absent — a no-sub-agent, nothing-finished
// session shows a three-row menu, not three dead rows.
function ui2RefreshArrangeMenu() {
  const btn = document.getElementById('ui2-arrange-btn');
  if (!btn) return;
  const m = ui2ArrangeMenuModel();
  btn.hidden = m.windows === 0;
  // A sweep can empty the grid under an open menu (Hide finished closes
  // every card) — retire the popover with its trigger.
  if (btn.hidden) ui2CloseArrangeMenu();
  ui2ArrangeSetRow('ui2-arrange-expand-all', {
    count: m.minimized,
    disabled: m.minimized === 0,
    title: 'Restore every minimized session window',
  });
  ui2ArrangeSetRow('ui2-arrange-collapse-all', {
    count: m.expanded,
    disabled: m.expanded === 0,
    title: m.expanded === 1
      ? 'Minimize the session window to its header bar'
      : `Minimize all ${m.expanded} session windows to their header bars`,
  });
  ui2ArrangeSetRow('ui2-arrange-collapse-subs', {
    count: m.subsExpanded,
    hidden: m.subs === 0,
    disabled: m.subsExpanded === 0,
    title: m.subsExpanded === 1
      ? 'Minimize the sub-agent window — top-level sessions stay in place'
      : `Minimize all ${m.subsExpanded} sub-agent windows — working or finished — leaving top-level sessions in place`,
  });
  ui2ArrangeSetRow('ui2-arrange-minimize-done', {
    count: m.doneSubsExpanded,
    hidden: m.doneSubsExpanded === 0,
    title: m.doneSubsExpanded === 1
      ? 'Minimize the finished sub-agent window'
      : `Minimize all ${m.doneSubsExpanded} finished sub-agent windows`,
  });
  ui2ArrangeSetRow('ui2-arrange-hide-done', {
    count: m.hardDone,
    hidden: m.hardDone === 0,
    title: m.hardDone === 1
      ? 'Hide the finished session card (its session stays in Sessions)'
      : `Hide all ${m.hardDone} finished session cards (their sessions stay in Sessions)`,
  });
  const collapseDetails = m.detailsExpanded > 0;
  ui2ArrangeSetRow('ui2-arrange-details', {
    label: collapseDetails ? 'Collapse all details' : 'Expand all details',
    direction: collapseDetails ? 'collapse' : 'expand',
    title: collapseDetails
      ? (m.detailsExpanded === 1
          ? 'Collapse header details on the expanded session window'
          : `Collapse header details on all ${m.detailsExpanded} expanded session windows`)
      : 'Expand header details on every session window',
  });
}

// Legacy seam: 40-session-launch.js's relationship-render pass still
// calls the refresher by its pill-era name (directly at the render pass,
// rAF-deduped from the badge refreshes) — keep both names routing into
// the single-pass menu refresh.
function ui2RefreshMinimizeDoneControl() {
  ui2RefreshArrangeMenu();
}

let ui2MinimizeDoneRefreshQueued = false;
function ui2QueueMinimizeDoneRefresh() {
  if (ui2MinimizeDoneRefreshQueued) return;
  ui2MinimizeDoneRefreshQueued = true;
  requestAnimationFrame(() => {
    ui2MinimizeDoneRefreshQueued = false;
    ui2RefreshArrangeMenu();
  });
}

// Menu close is callable from anywhere (ui2ApplyLayout runs it on every
// layout flip), so it re-queries the DOM instead of riding the wire
// closure. No-op while closed.
function ui2CloseArrangeMenu() {
  const menu = document.getElementById('ui2-arrange-menu');
  const btn = document.getElementById('ui2-arrange-btn');
  if (menu) menu.classList.remove('open');
  if (btn) btn.setAttribute('aria-expanded', 'false');
}

function ui2RunArrangeAction(action, row) {
  switch (action) {
    case 'expand-all':
    case 'collapse-all':
      if (typeof setAllSessionWindowsMinimized === 'function') {
        setAllSessionWindowsMinimized(action === 'collapse-all');
      }
      break;
    case 'collapse-subs':
      if (typeof minimizeSubagentWindows === 'function') minimizeSubagentWindows();
      break;
    case 'minimize-done':
      if (typeof minimizeDoneSubagentWindows === 'function') minimizeDoneSubagentWindows();
      break;
    case 'hide-done':
      if (typeof hideDoneSessionWindows === 'function') hideDoneSessionWindows();
      break;
    case 'details':
      // Direction rides the row's data-direction so the label the user
      // clicked and the sweep it runs can never disagree.
      if (typeof setAllSessionWindowHeadersCollapsed === 'function') {
        setAllSessionWindowHeadersCollapsed(row?.dataset.direction !== 'expand');
      }
      break;
    default:
      break;
  }
  ui2RefreshArrangeMenu();
}

// Popover mechanics copied from the Options wiring below: fixed-position
// anchor stamped at open, outside-pointerdown + Escape close, resize
// re-anchor. The off-log guard differs — the router only stamps .hidden
// on the Options panel — so leaving the Timeline is observed on the log
// sub-tab button's class instead (the stylesheet display gate already
// keeps a stale .open invisible; this keeps aria in step).
function ui2WireArrangeMenu() {
  const btn = document.getElementById('ui2-arrange-btn');
  const menu = document.getElementById('ui2-arrange-menu');
  if (!btn || !menu) return;
  const setOpen = (open) => {
    if (!open) {
      ui2CloseArrangeMenu();
      return;
    }
    const r = btn.getBoundingClientRect();
    menu.style.top = `${Math.round(r.bottom + 6)}px`;
    menu.style.left = '';
    menu.style.right = `${Math.max(8, Math.round(window.innerWidth - r.right))}px`;
    // Counts are fresh at the moment the menu opens, whatever tick last
    // painted them.
    ui2RefreshArrangeMenu();
    menu.classList.add('open');
    btn.setAttribute('aria-expanded', 'true');
  };
  btn.addEventListener('click', () => setOpen(!menu.classList.contains('open')));
  for (const row of menu.querySelectorAll('.ui2-arrange-item')) {
    row.addEventListener('click', () => {
      if (row.disabled) return;
      ui2RunArrangeAction(row.dataset.arrangeAction, row);
      // Menu semantics: an action dismisses the menu (counts re-derive on
      // the next open; the sweep's own render pass keeps the grid live).
      setOpen(false);
      btn.focus();
    });
  }
  document.addEventListener('pointerdown', (e) => {
    if (!menu.classList.contains('open')) return;
    if (menu.contains(e.target) || btn.contains(e.target)) return;
    setOpen(false);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || !menu.classList.contains('open')) return;
    setOpen(false);
    btn.focus();
  });
  const logBtn = document.querySelector('#activity-subtabs .subtab-btn[data-activity-tab="log"]');
  if (logBtn) {
    new MutationObserver(() => {
      if (!logBtn.classList.contains('active') && menu.classList.contains('open')) setOpen(false);
    }).observe(logBtn, { attributes: true, attributeFilter: ['class'] });
  }
  // position:fixed captures the anchor at open time — track resizes.
  window.addEventListener('resize', () => {
    if (menu.classList.contains('open')) setOpen(true);
  });
  ui2RefreshArrangeMenu();
}

// Composer dressing: placeholder copy + the attach glyph. The send
// button's TEXT is state (Send / ↗ Steer / Save & rerun) — left alone.
function ui2DressComposer() {
  const input = document.getElementById('activity-task-input');
  if (input) input.placeholder = 'Reply, steer, or describe a new task…';
  const attach = document.getElementById('upload-attach-btn');
  if (attach && typeof ui2Icon === 'function') attach.innerHTML = ui2Icon('attach', 15);
}

// ── Focus surface: Focus shows the FOREGROUND session's transcript ─────
// The product decision: "Focus timeline + session switcher".
//
// Focus used to implement that as a FILTER over #log-stream — the combined
// ("Concurrent view") stream — hiding entries belonging to other sessions.
// But that stream only ever carries entries the live event feed delivered.
// A resumed session's transcript is fetched and rendered straight into its
// session window (hydrateRestoredSessionWindow → renderRestoredSessionWindow-
// Entries, which touches the main stream nowhere), so it never enters the
// stream at all — and Focus rendered a blank page for precisely the sessions
// the user had just reopened, while Grid showed them fine.
//
// The session window already IS the per-session timeline: hydrated (external
// and restored sessions included), signature-deduped, virtualized, paginated,
// and live. So Focus PROMOTES that window rather than rendering a second copy
// of it — one renderer, one DOM, and a resumed session appears in Focus
// because it is the very node Grid draws.
//
// With no session targeted, Focus falls back to the combined stream (the
// "all sessions" reading) — which is also where the idle / unfueled empty
// state lives, so that notice now shows only when there is genuinely
// nothing to read.
function ui2FocusSessionId() {
  // The canonical target first — the same resolver the composer and the
  // session switcher use.
  if (typeof resolvePromptTargetSessionId === 'function') {
    const sid = resolvePromptTargetSessionId();
    if (sid) return sid;
  }
  const windows = typeof sessionWindows !== 'undefined' ? sessionWindows : null;
  if (!windows || windows.size === 0) return '';
  // That resolver gates every candidate on STEERABILITY (isPromptTargetSession-
  // Usable: may I send this session a message?). Focus asks the weaker
  // question — what am I READING? — and a session that has ended, or is still
  // spinning up, is perfectly readable while not being steerable. So fall back
  // past that gate: the window the user explicitly opened, and then an
  // unambiguous single window (the same "one candidate" rule the resolver
  // applies, minus the steerability filter). Without this, resuming a single
  // session left Focus on the all-sessions surface — i.e. blank, since a
  // resumed transcript never enters the combined stream.
  if (typeof foregroundSessionFullId !== 'undefined'
      && foregroundSessionFullId && windows.has(foregroundSessionFullId)) {
    return foregroundSessionFullId;
  }
  if (windows.size === 1) return windows.keys().next().value;
  return '';
}

function ui2ApplyFocusSurface() {
  const root = document.documentElement;
  const focusMode = root.dataset.ui2Layout !== 'grid';
  const sid = focusMode ? ui2FocusSessionId() : '';
  const windows = typeof sessionWindows !== 'undefined' ? sessionWindows : null;
  const win = (sid && windows) ? windows.get(sid) : null;
  if (windows) {
    for (const [id, w] of windows) {
      w?.el?.classList.toggle('ui2-focus-window', !!win && id === sid);
    }
  }
  // CSS is the single reader: "session" promotes the marked window and
  // retires the combined stream; "all" is the pre-existing combined view.
  root.dataset.ui2Focus = win ? 'session' : 'all';
  // A session promoted straight into Focus may never have been opened in
  // Grid, so its window can still be empty. Hydration is idempotent (it
  // no-ops on non-empty history and on an in-flight fetch), which makes
  // Focus on its own enough to reopen a session.
  if (win && typeof hydrateSessionWindowIfEmpty === 'function') {
    Promise.resolve(hydrateSessionWindowIfEmpty(sid)).catch(() => {});
  }
}

// QA drive hooks (window.qa convention): the dashboard validator runs
// outside the module scope, so the timeline probes need window-visible
// seams onto the transcript machinery — synthetic entries through the
// REAL pipelines (processCommands / renderRestoredSessionWindowEntries),
// plus the minimize/layout/placeholder controls the boot probe asserts.
// Side-effect-free readback in windowState; the drive hooks mutate only
// the session windows the probe itself seeds.
window.qa = Object.assign(window.qa || {}, {
  timeline: {
    seedLog: (c) => { processCommands([{ cmd: 'add_log_entry', ...c }]); return true; },
    seedReplay: (sid, entries) => {
      const win = sessionWindows.get(sid) || ensureSessionWindow(sid);
      if (!win) return -1;
      return renderRestoredSessionWindowEntries(win, entries, sid);
    },
    ensureWindow: (sid) => !!ensureSessionWindow(sid),
    windowState: (sid) => {
      const win = sessionWindows.get(sid);
      if (!win) return null;
      return {
        mounted: win.log ? win.log.childElementCount : -1,
        history: Array.isArray(win.logHistory) ? win.logHistory.length : 0,
        minimized: !!win.minimized,
        text: win.log ? (win.log.textContent || '') : '',
        rows: win.log
          ? [...win.log.querySelectorAll('.log-entry')].map(r => ({
              text: (r.textContent || '').slice(0, 160),
              userTurnIndex: r.dataset.userTurnIndex || '',
            }))
          : [],
      };
    },
    setMinimized: (sid, minimized) => { setSessionWindowMinimized(sid, minimized); return true; },
    // try/catch: focus repaints the Context pane, whose three.js renderer
    // throws in WebGL-less validator browsers — the focus/foreground state
    // this hook exists for is set before that repaint.
    focusSession: (sid) => { try { focusSessionWindow(sid); } catch (_) {} return true; },
    applyLayout: (mode) => { ui2ApplyLayout(mode); return true; },
    setHydrateError: (sid, message) => {
      const win = sessionWindows.get(sid);
      if (!win) return false;
      win.hydrateError = String(message || '');
      renderSessionWindowLogPlaceholder(win);
      return true;
    },
  },
});

// QA readback (window.qa convention): the Focus surface's inputs and its
// verdict. The interesting regression is silent — Focus renders an empty
// page — so the probe exposes what it resolved and what it promoted.
window.qa = Object.assign(window.qa || {}, {
  focusSurface: () => ({
    layout: document.documentElement.dataset.ui2Layout || 'focus',
    surface: document.documentElement.dataset.ui2Focus || '',
    sessionId: ui2FocusSessionId(),
    promptTarget: typeof resolvePromptTargetSessionId === 'function'
      ? (resolvePromptTargetSessionId() || '') : '',
    foreground: typeof foregroundSessionFullId !== 'undefined' ? (foregroundSessionFullId || '') : '',
    railSessionId: ui2RailForegroundSessionId(),
    railTicks: ui2RailTickCount,
    windows: typeof sessionWindows !== 'undefined' ? [...sessionWindows.keys()] : [],
    promotedEntries: (() => {
      const log = document.querySelector(
        '#session-window-grid .session-window.ui2-focus-window .session-window-log'
      );
      return log ? log.querySelectorAll('.log-entry').length : 0;
    })(),
  }),
  // The Arrange menu (successor of the pill-era qa.minimizeDone): the
  // one-pass model every row derives from, the trigger/menu open state,
  // each row's rendered label/count/state, and the per-subagent-window
  // flag state the sweeps mutate (the failure modes — a live window
  // collapsed, a user-restored window re-collapsed — are silent without
  // this).
  arrangeMenu: () => ({
    model: ui2ArrangeMenuModel(),
    buttonHidden: document.getElementById('ui2-arrange-btn')?.hidden ?? true,
    expanded: document.getElementById('ui2-arrange-btn')?.getAttribute('aria-expanded') === 'true',
    open: document.getElementById('ui2-arrange-menu')?.classList.contains('open') ?? false,
    rows: [...document.querySelectorAll('#ui2-arrange-menu .ui2-arrange-item')].map((row) => ({
      action: row.dataset.arrangeAction || '',
      label: row.querySelector('.ui2-arrange-item-label')?.textContent || '',
      count: row.querySelector('.ui2-arrange-count:not([hidden])')?.textContent || '',
      hidden: !!row.hidden,
      disabled: !!row.disabled,
      direction: row.dataset.direction || '',
    })),
    subagents: (typeof sessionWindows !== 'undefined'
      && typeof sessionWindowIsSubagent === 'function')
      ? [...sessionWindows.entries()]
        .filter(([sid]) => sessionWindowIsSubagent(sid))
        .map(([sid, w]) => ({
          sid,
          phase: w.phase || '',
          ended: !!w.ended,
          minimized: !!w.minimized,
          autoMinimized: !!w.autoMinimized,
          userRestoredWhileDone: !!w.userRestoredWhileDone,
        }))
      : [],
  }),
});

let ui2FocusSurfaceQueued = false;
function ui2QueueFocusSurface() {
  if (ui2FocusSurfaceQueued) return;
  ui2FocusSurfaceQueued = true;
  requestAnimationFrame(() => {
    ui2FocusSurfaceQueued = false;
    ui2ApplyFocusSurface();
  });
}

// Raw-payload hygiene + tier tagging: backend stderr lines render as
// compact, dimmed mono rows; session bookkeeping ("Round N complete",
// "Turn N started", "Session attached…") and steer receipts read as
// quiet meta hairlines instead of full-voice rows. Runs over BOTH the
// combined stream and the session windows — Focus promotes a window, so
// tagging the stream alone misses everything the user actually reads.
function ui2TagEntry(e) {
  if (e.classList.contains('ui2-raw-checked')) return;
  e.classList.add('ui2-raw-checked');
  const text = (e.querySelector('.log-content')?.textContent || '').trim();
  if (/^\[(codex|claude(-code)?|backend) stderr\]/i.test(text)) {
    e.classList.add('ui2-stderr');
  }
  if (e.classList.contains('source-steer')) {
    e.classList.add('ui2-meta');
    return;
  }
  const level = e.dataset.level || '';
  if (
    (level === 'info' || level === 'detail') &&
    /^(Round \d+ complete|Turn \d+ started|Session (started|attached)|Done signal)/.test(text)
  ) {
    e.classList.add('ui2-meta');
  }
}

function ui2TagEntriesIn(rootEl) {
  if (!rootEl) return;
  rootEl.querySelectorAll('.log-entry:not(.ui2-raw-checked)').forEach(ui2TagEntry);
}

function ui2TagRawEntries() {
  ui2TagEntriesIn(document.getElementById('log-stream'));
  ui2TagEntriesIn(document.getElementById('session-window-grid'));
}

// Added-node tagging: the tagger used to re-run the `.log-entry:not(...)`
// selector over the 10k-cap stream PLUS every window log on every grid
// mutation — including the 1 Hz goal/vitals ticker text writes, i.e. a
// full match-test of ~10-15k nodes every second while idle. The observers
// now hand over exactly what was ADDED; text/attribute churn never queues
// a pass (their addedNodes are text nodes), and a container insertion
// (replay fragment, re-rendered window range) scans only its own subtree.
// Clones inherit the source entry's classes, so re-tagging is a no-op skip.
// Bounded: MutationObservers keep firing in hidden tabs while rAF does not,
// so an unbounded queue would retain every appended node overnight —
// including entries the 10k prune already detached, pinned with their full
// subtrees. Past the cap the queue collapses to one "rescan everything"
// flag (the flush then runs ui2TagRawEntries, whose class guard makes the
// sweep idempotent), and hidden tabs flush via setTimeout since their rAF
// never fires (background timer throttling only delays it; the cap is the
// memory guarantee either way).
const UI2_TAG_QUEUE_CAP = 2048;
let ui2PendingTagNodes = new Set();
let ui2TagPassQueued = false;
let ui2TagQueueOverflowed = false;
function ui2RunEntryTagPass() {
  ui2TagPassQueued = false;
  const nodes = ui2PendingTagNodes;
  ui2PendingTagNodes = new Set();
  if (ui2TagQueueOverflowed) {
    ui2TagQueueOverflowed = false;
    ui2TagRawEntries();
    return;
  }
  for (const node of nodes) {
    if (!node.isConnected) continue;
    if (node.classList && node.classList.contains('log-entry')) ui2TagEntry(node);
    else ui2TagEntriesIn(node);
  }
}
function ui2QueueEntryTagging(records) {
  if (!ui2TagQueueOverflowed) {
    for (const record of records) {
      for (const node of record.addedNodes) {
        if (node.nodeType !== 1) continue;
        if (ui2PendingTagNodes.size >= UI2_TAG_QUEUE_CAP) {
          ui2PendingTagNodes.clear();
          ui2TagQueueOverflowed = true;
          break;
        }
        ui2PendingTagNodes.add(node);
      }
      if (ui2TagQueueOverflowed) break;
    }
  }
  if ((!ui2PendingTagNodes.size && !ui2TagQueueOverflowed) || ui2TagPassQueued) return;
  ui2TagPassQueued = true;
  if (document.hidden) setTimeout(ui2RunEntryTagPass, 250);
  else requestAnimationFrame(ui2RunEntryTagPass);
}
// A pass scheduled on rAF while visible never fires once the tab hides —
// convert it to a timer flush (ui2RunEntryTagPass tolerates the stale rAF
// firing later on an already-drained queue).
document.addEventListener('visibilitychange', () => {
  if (document.hidden && ui2TagPassQueued) setTimeout(ui2RunEntryTagPass, 250);
});

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
  rail.setAttribute('aria-label', 'Session inspector');
  // v3 inspector: grouped sections, same row ids — ui2RailTick writes by
  // id and ui2RailSetRow queries .ui2-rail-value inside each row, so the
  // 1 Hz refresh path is unchanged by the re-chrome.
  rail.innerHTML = `
    <div class="ui2-rail-head">
      <div class="ui2-rail-eyebrow">Inspector</div>
      <div class="ui2-rail-session" id="ui2-rail-session">No session yet</div>
    </div>
    <div class="ui2-rail-section">
      <div class="ui2-rail-row" id="ui2-rail-git"><span class="ui2-rail-label">Working tree</span><span class="ui2-rail-value">—</span></div>
      <div class="ui2-rail-row" id="ui2-rail-ctx"><span class="ui2-rail-label">Context budget</span><span class="ui2-rail-value">—</span><div class="ui2-rail-meter"><i id="ui2-rail-ctx-fill"></i></div></div>
      <div class="ui2-rail-row" id="ui2-rail-cache"><span class="ui2-rail-label">Prompt cache</span><span class="ui2-rail-value">—</span></div>
      <div class="ui2-rail-row" id="ui2-rail-limits"><span class="ui2-rail-label">Rate limits</span><span class="ui2-rail-value">—</span></div>
    </div>
    <div class="ui2-rail-foot">
      <button type="button" class="ui2-rail-row ui2-rail-changes" id="ui2-rail-changes" title="Open the Changes sub-tab"><span class="ui2-rail-label">Changes</span><span class="ui2-rail-value">—</span></button>
      <button type="button" class="ui2-rail-advanced" id="ui2-rail-advanced" title="Raw state and observers live on the Debug tab">Advanced &amp; raw state</button>
    </div>`;
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

// ── Timeline "Options" popover (verbosity + host filter) ───────────────
// The panel is #activity-log-controls — the router hides it off-log with
// .hidden (display:none !important), which deliberately beats .open, so a
// stale open state can never leak the panel onto another sub-tab.
function ui2WireViewOptions() {
  const btn = document.getElementById('ui2-view-options-btn');
  const panel = document.getElementById('activity-log-controls');
  if (!btn || !panel) return;
  const setOpen = (open) => {
    if (open) {
      // Anchor to the trigger's bottom-right (position:fixed — see CSS).
      const r = btn.getBoundingClientRect();
      panel.style.top = `${Math.round(r.bottom + 6)}px`;
      panel.style.left = '';
      panel.style.right = `${Math.max(8, Math.round(window.innerWidth - r.right))}px`;
    }
    panel.classList.toggle('open', open);
    btn.setAttribute('aria-expanded', String(open));
  };
  btn.addEventListener('click', () => setOpen(!panel.classList.contains('open')));
  document.addEventListener('pointerdown', (e) => {
    if (!panel.classList.contains('open')) return;
    if (panel.contains(e.target) || btn.contains(e.target)) return;
    setOpen(false);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || !panel.classList.contains('open')) return;
    setOpen(false);
    btn.focus();
  });
  // The router's off-log hide (.hidden) only STACKS over .open — fully
  // close instead, so aria-expanded and the stamped anchor don't go stale
  // and returning to the Timeline doesn't resurface an unrequested popover.
  // (Removing .open here re-fires the observer once; it no-ops on a panel
  // that is no longer .open.)
  new MutationObserver(() => {
    if (panel.classList.contains('hidden') && panel.classList.contains('open')) setOpen(false);
  }).observe(panel, { attributes: true, attributeFilter: ['class'] });
  // position:fixed captures the anchor at open time — track resizes.
  window.addEventListener('resize', () => {
    if (panel.classList.contains('open')) setOpen(true);
  });
}

// The rail describes the session Focus is SHOWING, so it derives from the
// same selector the Focus surface promotes with (and the session switcher
// displays) — one source, three readers. It previously stopped at
// resolvePromptTargetSessionId, whose steerability gate rejects an ended or
// still-starting session, and so read "No session yet" while a perfectly
// live session was on screen beside it. The daemon's own session stays the
// last resort.
function ui2RailForegroundSessionId() {
  const sid = ui2FocusSessionId();
  if (sid) return sid;
  if (typeof daemonSessionFullId !== 'undefined' && daemonSessionFullId) return daemonSessionFullId;
  return '';
}

// The rail repaints on a 1s interval, so any probe of it races that interval —
// reading it right after load shows the boot tick's values and looks exactly
// like a broken selector. The counter lets a probe wait deterministically
// (window.qa.focusSurface().railTicks) instead of sleeping and guessing.
let ui2RailTickCount = 0;
function ui2RailTick(force) {
  ui2RailTickCount++;
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
    ui2WireArrangeMenu();
    ui2WireViewOptions();
    ui2DressComposer();
    ui2BuildVitalsRail();
    ui2RailTick(true);
    setInterval(() => ui2RailTick(), 1000);
    // Focus surface: follow stream appends, target changes (the chip
    // re-renders on every change), layout flips, and the session-window
    // grid's own MEMBERSHIP — a session resumed from the Sessions tab
    // materializes its window there, and that is the only signal Focus
    // gets that the window it must promote now exists. Membership is
    // childList on the grid ROOT: appends inside a window's log (or the
    // per-second ticker text writes) cannot change what Focus promotes,
    // and routing them here re-ran the promote pass once per frame during
    // bursts and once per second while idle.
    const stream = document.getElementById('log-stream');
    if (stream) {
      new MutationObserver((records) => {
        ui2QueueFocusSurface();
        ui2QueueEntryTagging(records);
      }).observe(stream, { childList: true });
    }
    const chip = document.getElementById('task-target-chip');
    if (chip) new MutationObserver(ui2QueueFocusSurface).observe(chip, {
      childList: true, characterData: true, subtree: true, attributes: true,
    });
    const windowGrid = document.getElementById('session-window-grid');
    if (windowGrid) {
      new MutationObserver(ui2QueueFocusSurface).observe(windowGrid, { childList: true });
      // subtree: entries append INSIDE window logs — the meta/stderr tagger
      // must see them, not just window add/remove. It consumes only
      // addedNodes (see ui2QueueEntryTagging), so ticker text churn and
      // attribute writes never trigger a scan.
      new MutationObserver(ui2QueueEntryTagging).observe(windowGrid, { childList: true, subtree: true });
    }
    ui2ApplyFocusSurface();
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
