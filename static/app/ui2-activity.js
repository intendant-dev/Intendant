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
