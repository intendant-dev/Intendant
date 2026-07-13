// ── Tab Switching ──
document.querySelectorAll('.tab-btn').forEach(btn => {
  btn.addEventListener('click', () => routeTo(btn.dataset.tab));
});

// Terminal sub-tab switching (Shell).
document.querySelectorAll('#tab-terminal .subtab-btn[data-term-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('terminal', btn.dataset.termTab));
});

// Files sub-tab switching (Editor | Transfers).
document.querySelectorAll('#tab-files .subtab-btn[data-files-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('files', btn.dataset.filesTab));
});
document.getElementById('shell-host-select')?.addEventListener('change', ev => {
  setShellHost(ev.target?.value || SHELL_HOST_ID);
});

document.getElementById('shell-share-btn')?.addEventListener('click', toggleShellShare);

// Settings sub-tab switching (Account | Agent | Debug).
document.querySelectorAll('#tab-settings .subtab-btn[data-settings-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('settings', btn.dataset.settingsTab));
});

// Access sub-tab switching (Overview | People & Devices | Daemons | Peer Trust | Policies | Audit).
document.querySelectorAll('#access-subtabs .subtab-btn[data-access-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('access', btn.dataset.accessTab));
});

// Sessions sub-tab switching (Recent | Deep Search | Worktrees | New Session).
document.querySelectorAll('#sessions-subtabs .subtab-btn[data-sessions-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('sessions', btn.dataset.sessionsTab));
});

// Activity sub-tab switching (Log | Context | Managed | Changes | Control).
document.querySelectorAll('#activity-subtabs .subtab-btn[data-activity-tab]').forEach(btn => {
  btn.addEventListener('click', () => routeTo('activity', btn.dataset.activityTab));
});

// ── Hash-based router ──
//
// URL is the source of truth for which tab + sub-tab is active:
//   #activity
//   #stats
//   #terminal           → the interactive shell
//   #terminal/shell     → opens the Shell sub-tab directly
//   #access/overview    → opens unified access administration
//   #settings/agent     → opens the Settings tab on the Agent sub-tab
//
// This gives us three things for free:
//   1. Browser back/forward navigates between tabs
//   2. Refresh preserves the current tab (no localStorage hack needed
//      for main/sub-tab persistence)
//   3. Deep links like `http://host:8765#stats` or
//      `http://host:8765#access/overview` open the dashboard at a
//      specific view - useful for bookmarking multi-host daemons.
//
// We still use localStorage for preferences that aren't navigation
// (verbosity, direct-mode, host filter, sessions filters).

const VALID_TABS = ['activity', 'stats', 'terminal', 'displays', 'station', 'sessions', 'files', 'access', 'vault', 'debug', 'settings'];
const VALID_ACTIVITY_SUBTABS = ['log', 'context', 'managed', 'changes', 'control'];
const VALID_TERM_SUBTABS = ['shell'];
const VALID_SETTINGS_SUBTABS = ['account', 'agent', 'network', 'debug', 'autonomy', 'providers', 'presence', 'advanced', 'appearance'];
// ui-v2 (design-overhaul) remaps the three legacy Settings sub-tabs onto
// the four design sections — and back when the flag is off — so both
// generations of deep link keep landing somewhere sensible. 'network'
// stays out of both maps: its Access redirect below handles it.
// Legacy v1-era subtab names in old bookmarks/deep links map onto the
// sections that absorbed them.
const SETTINGS_SUBTAB_ALIASES = { account: 'providers', agent: 'providers', debug: 'advanced' };
function normalizeSettingsSubtab(name) {
  const raw = String(name || '').trim();
  return SETTINGS_SUBTAB_ALIASES[raw] || raw;
}
const ACCESS_SUBTAB_ALIASES = {
  targets: 'daemons',
  invitations: 'peers',
  policies: 'advanced',
  audit: 'advanced',
  grants: 'advanced',
  public: 'advanced',
};
const ACCESS_CANONICAL_SUBTABS = ['overview', 'daemons', 'people', 'peers', 'diagnostics', 'advanced'];
const VALID_ACCESS_SUBTABS = ACCESS_CANONICAL_SUBTABS.concat(Object.keys(ACCESS_SUBTAB_ALIASES));
const VALID_SESSIONS_SUBTABS = ['recent', 'deep', 'worktrees', 'new'];

function normalizeAccessSubtab(name) {
  const raw = String(name || 'overview').trim();
  return ACCESS_SUBTAB_ALIASES[raw] || (ACCESS_CANONICAL_SUBTABS.includes(raw) ? raw : 'overview');
}

function parseRoute() {
  const raw = window.location.hash.slice(1); // strip leading '#'
  if (!raw) {
    return DASHBOARD_ACCESS_PAGE_MODE
      ? { tab: 'access', subtab: 'overview' }
      : { tab: 'activity', subtab: null };
  }
  const [tab, subtab] = raw.split('/');
  if (DASHBOARD_ACCESS_PAGE_MODE && tab !== 'access') {
    return { tab: 'access', subtab: 'overview' };
  }
  if (!VALID_TABS.includes(tab)) return { tab: 'activity', subtab: null };
  if (tab === 'settings' && subtab === 'network') {
    return { tab: 'access', subtab: 'overview' };
  }
  if (tab === 'activity' && subtab && !VALID_ACTIVITY_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  if (tab === 'terminal' && subtab && !VALID_TERM_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  if (tab === 'files' && subtab && !VALID_FILES_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  if (tab === 'settings' && subtab && !VALID_SETTINGS_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  if (tab === 'access' && subtab && !VALID_ACCESS_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  if (tab === 'sessions' && subtab && !VALID_SESSIONS_SUBTABS.includes(subtab)) {
    return { tab, subtab: null };
  }
  return { tab, subtab: tab === 'access' && subtab ? normalizeAccessSubtab(subtab) : (subtab || null) };
}

function daemonDashboardHref(hash = '#activity') {
  if (DASHBOARD_CONNECT_MODE && DASHBOARD_CONNECT_DAEMON_ID) {
    return `/app?connect=1&daemon_id=${encodeURIComponent(DASHBOARD_CONNECT_DAEMON_ID)}${hash || ''}`;
  }
  return `/${hash || ''}`;
}

function accessHomeHref(subtab = 'overview') {
  if (DASHBOARD_CONNECT_MODE) return '/access';
  return `/access#access/${normalizeAccessSubtab(subtab)}`;
}

function openAccessHome() {
  if (DASHBOARD_ACCESS_PAGE_MODE) {
    window.location.href = daemonDashboardHref('#activity');
  } else {
    window.location.href = accessHomeHref(activeAccessSubtab || 'overview');
  }
}

function syncAccessPageNavLink() {
  const prefix = document.getElementById('sb-access-page-prefix');
  const label = document.getElementById('sb-access-page-label');
  const group = document.getElementById('sb-access-page-link');
  if (!prefix || !label || !group) return;
  if (DASHBOARD_ACCESS_PAGE_MODE) {
    prefix.textContent = 'back';
    label.textContent = 'Dashboard';
    group.title = 'Return to the daemon dashboard';
  } else {
    prefix.textContent = 'admin';
    label.textContent = 'Access';
    group.title = 'Open fleet access administration';
  }
}

function tabDomMatches(tab) {
  const pane = document.getElementById('tab-' + tab);
  const activeButton = document.querySelector('.tab-btn.active');
  const activePane = document.querySelector('.tab-pane.active');
  return Boolean(
    pane &&
    pane.classList.contains('active') &&
    activeButton?.dataset?.tab === tab &&
    activePane === pane
  );
}

// Navigate the dashboard. Updates the URL via history.pushState (so
// back/forward works) and calls the existing switch* functions.
// `subtab` is optional; when omitted, the target tab's default sub-tab
// is used (or preserved if it's already the active one).
function routeTo(tab, subtab) {
  if (tab === 'access' && subtab) subtab = normalizeAccessSubtab(subtab);
  // A live annotation/callout is editable state, not navigation chrome.
  // Refuse the route before changing the URL so a tab click cannot silently
  // discard it or leave the hash claiming a pane we did not open.
  if (activeTab === 'displays' && tab !== 'displays' &&
      typeof window.canDeactivateLiveDisplayWorkspace === 'function' &&
      !window.canDeactivateLiveDisplayWorkspace()) {
    return false;
  }
  const target = subtab ? `${tab}/${subtab}` : tab;
  if (DASHBOARD_ACCESS_PAGE_MODE && tab !== 'access') {
    window.location.href = daemonDashboardHref(`#${target}`);
    return;
  }
  const nextHash = `#${target}`;
  if (window.location.hash !== nextHash) {
    history.pushState(null, '', nextHash);
  }
  // Main tab first so sub-tab logic runs in the correct pane.
  const switched = tab !== activeTab || !tabDomMatches(tab);
  if (switched) {
    switchTab(tab);
  }
  if (tab === 'activity' && subtab) {
    switchActivitySubtab(subtab);
  } else if (tab === 'terminal' && subtab) {
    switchTerminalSubtab(subtab);
  } else if (tab === 'files' && subtab) {
    switchFilesSubtab(subtab);
  } else if (tab === 'access') {
    switchAccessSubtab(subtab || activeAccessSubtab || 'overview');
  } else if (tab === 'settings' && subtab) {
    switchSettingsSubtab(subtab);
  } else if (tab === 'sessions') {
    switchSessionsSubtab(subtab || 'recent');
  }
  if (tab === 'stats' && !switched) {
    renderStatsForActiveHost({ forceSessions: true });
  }
  return true;
}

// Apply the URL's current hash to the actual DOM. Called on initial
// load, and on popstate (back/forward) and hashchange (manual hash
// edit) events.
function applyCurrentRoute() {
  const { tab, subtab } = parseRoute();
  // Normalize the URL if an invalid or partially-valid hash got
  // parsed to a different route. `replaceState` avoids polluting
  // browser history with bogus entries.
  const normalized = subtab ? `#${tab}/${subtab}` : `#${tab}`;
  if (window.location.hash !== normalized && window.location.hash !== '') {
    history.replaceState(null, '', normalized);
  }
  const switched = tab !== activeTab;
  const needsDomApply = !tabDomMatches(tab);
  if (switched || needsDomApply) {
    if (switchTab(tab) === false) {
      // Back/forward or a manually edited hash can bypass routeTo's
      // preflight. Keep the editable Live surface active and restore the
      // URL without creating another history entry.
      history.replaceState(null, '', '#displays');
      return;
    }
  }
  if (tab === 'activity' && subtab) {
    switchActivitySubtab(subtab);
  } else if (tab === 'terminal' && subtab) {
    switchTerminalSubtab(subtab);
  } else if (tab === 'files' && subtab) {
    switchFilesSubtab(subtab);
  } else if (tab === 'access') {
    switchAccessSubtab(subtab || activeAccessSubtab || 'overview');
  } else if (tab === 'settings' && subtab) {
    switchSettingsSubtab(subtab);
  } else if (tab === 'sessions') {
    switchSessionsSubtab(subtab || 'recent');
  }
  if (tab === 'stats' && !switched) {
    renderStatsForActiveHost({ forceSessions: true });
  }
}

window.addEventListener('popstate', applyCurrentRoute);
window.addEventListener('hashchange', applyCurrentRoute);

function switchTab(tabId) {
  // Interactive display input includes a document-level paste handler.
  // Never leave it bound to a stage that is about to become hidden on
  // Station, Sessions, Settings, etc. Activity already moves the canvas
  // into a thumbnail and historically released there; this single entry
  // point makes every navigation path follow the same held-key-flush →
  // authority-release ordering.
  if (activeTab === 'displays' && tabId !== 'displays' &&
      typeof window.deactivateLiveDisplayWorkspace === 'function') {
    if (window.deactivateLiveDisplayWorkspace() === false) return false;
  }
  activeTab = tabId;
  document.querySelectorAll('.tab-btn').forEach(b => b.classList.toggle('active', b.dataset.tab === tabId));
  document.querySelectorAll('.tab-pane').forEach(p => p.classList.toggle('active', p.id === 'tab-' + tabId));
  if (tabId === 'activity') hideBadge('activity');
  if (app) processCommands(app.set_active_tab(tabId));
  stationSetActive(tabId === 'station');
  if (tabId === 'activity' && pendingApprovalId !== null && activeActivitySubtab !== 'log') {
    switchActivitySubtab('log');
  }
  if (tabId === 'terminal') {
    if (activeTermSubtab === 'shell') {
      if (!shellInitialized) initShell();
      if (shellTerm) requestAnimationFrame(() => shellFitAddon && shellFitAddon.fit());
    }
    syncTerminalPaneAccessibility();
  }
  if (tabId === 'activity' || tabId === 'displays') {
    relocateDisplays(tabId);
  }
  syncSessionWindowMetadataRefresh();
  if (tabId === 'sessions' && !sessionsLoaded) {
    loadSessions();
  }
  if (tabId === 'stats') {
    // The explicit refresh below supersedes any render deferred while
    // the pane was hidden (same for files/access) — drop it so the
    // trailing flushPaneRenders doesn't paint twice.
    paneDeferredRenders.delete('stats');
    renderStatsForActiveHost({ forceSessions: true });
  }
  if (tabId === 'files') {
    paneDeferredRenders.delete('files');
    filesIdeOnTabShown();
    refreshFilesDownloadAvailability();
    renderFilesTransfers();
    refreshFilesTransferJobs();
    renderFilesStagedUploads();
    refreshFilesStagedUploads();
  }
  if (tabId === 'settings' && !settingsLoaded) {
    loadSettings();
  }
  if (tabId === 'settings' && !apiKeyStatusLoaded) {
    loadApiKeyStatus();
  }
  if (tabId === 'vault') {
    // ui-v2 destination (the vault sections are re-parented here at boot;
    // under v1 this pane is only reachable by a hand-typed #vault and the
    // render below is a harmless repaint of the sections in Access →
    // Advanced). Vault state changes render eagerly on their own; entering
    // the pane repaints once so lease countdowns are fresh — the renderer
    // itself kicks the lease + custody refreshes.
    paneDeferredRenders.delete('vault');
    renderAccessVaultSection();
  }
  if (tabId === 'access') {
    paneDeferredRenders.delete('access');
    renderDaemonsList();
    renderAccessAdminSummaries();
    refreshAccessOverviewFromApi({ silent: true }).catch(() => {});
    refreshAccessEnrollments({ silent: true }).catch(() => {});
    refreshAccessConnectStatus({ silent: true }).catch(() => {});
    if (activeAccessSubtab === 'diagnostics') renderConnectHealthPanel();
  }
  flushPaneRenders(tabId);
  return true;
}

// ── Settings sub-tabs ──
//
// The Settings tab is split into four sub-tabs (Account, Agent,
// Network, Debug). Switching is purely local DOM — no data reload
// happens when the sub-tab changes, because all settings load in one
// shot via `loadSettings()` on tab open. The save/reset row lives
// outside the panes and always persists the full form.

// Remember the last active sub-tab across page reloads so users don't
// lose their place when the page auto-refreshes.
let activeActivitySubtab = 'log';
let latestContextSnapshot = null;
const contextSnapshotsBySession = new Map();
const contextSnapshotTimelinesBySession = new Map();
const contextReplayIndexBySession = new Map();
let contextReplayMode = 'live';
let contextSelectedPartId = null;
let contextSnapshotSeq = 1;
let contextLastAnalysis = null;
let contextListenersWired = false;
let contextFocusMode = false;
let contextRawOpen = false;
let contextRawRenderedKey = null;
let contextRenderScheduled = false;
const contextSnapshotExactFetches = new Map();
const contextSnapshotExactFailures = new Set();

const CONTEXT_GLOBAL_SESSION = '__global__';
const CONTEXT_CATEGORY_ORDER = [
  'instructions',
  'user',
  'assistant',
  'tool_call',
  'tool_output',
  'reasoning',
  'media',
  'schema',
  'config',
  'other',
];
/* Category colors resolve from the ui-v2 design tokens
   (16-styles-v2-tokens.css) — the visualizer paints into WebGL and canvas
   sprites, where CSS custom properties cannot cascade in, so they are
   resolved here at init and re-resolved on every data-theme flip (the
   terminal's ui2ShellTheme pattern in 44-shell-frames.js). `fallback`
   carries the dark-theme value for a token-less document; `color` is the
   resolved value every consumer reads. */
const CONTEXT_CATEGORY_DEFS = {
  instructions: { label: 'Instructions', token: '--iris', fallback: '#7E8CFA', color: '#7E8CFA' },
  user: { label: 'User', token: '--green', fallback: '#58C08C', color: '#58C08C' },
  assistant: { label: 'Assistant', token: '--violet', fallback: '#9B7CF2', color: '#9B7CF2' },
  tool_call: { label: 'Tool calls', token: '--amber', fallback: '#E4A85B', color: '#E4A85B' },
  tool_output: { label: 'Tool output', token: '--rose', fallback: '#EC6A85', color: '#EC6A85' },
  reasoning: { label: 'Reasoning', token: '--iris-2', fallback: '#A6AEFF', color: '#A6AEFF' },
  media: { label: 'Media', token: '--sky', fallback: '#5DA9E6', color: '#5DA9E6' },
  schema: { label: 'Tool schema', token: '--viz-ramp-3', fallback: '#6470D0', color: '#6470D0' },
  config: { label: 'Request config', token: '--viz-ramp-2', fallback: '#454C86', color: '#454C86' },
  other: { label: 'Other', token: '--neutral-dot', fallback: '#6c7086', color: '#6c7086' },
};

/* Chrome colors of the 3D scene (label sprites, lanes, reservoir frame,
   pressure ladder), resolved alongside the categories. The pressure
   ladder follows the v2 semantic triple green/amber/rose — the alias
   layer itself collapsed the old yellow/peach split into --amber. */
const CONTEXT_VIZ_THEME = {
  labelBg: 'rgba(14, 16, 21, .7)',
  labelText: '#EAECF2',
  laneBase: '#232834',
  frame: '#6c7086',
  railLabel: '#A7AEBE',
  selected: '#EAECF2',
  pressure: { high: '#EC6A85', mid: '#E4A85B', ok: '#58C08C' },
};

function contextResolveVizTheme() {
  const styles = getComputedStyle(document.documentElement);
  const token = (name, fallback) => (styles.getPropertyValue(name) || '').trim() || fallback;
  for (const def of Object.values(CONTEXT_CATEGORY_DEFS)) {
    def.color = token(def.token, def.fallback);
  }
  CONTEXT_VIZ_THEME.labelBg = token('--glass', 'rgba(14, 16, 21, .7)');
  CONTEXT_VIZ_THEME.labelText = token('--text', '#EAECF2');
  CONTEXT_VIZ_THEME.laneBase = token('--surface-3', '#232834');
  CONTEXT_VIZ_THEME.frame = token('--neutral-dot', '#6c7086');
  CONTEXT_VIZ_THEME.railLabel = token('--text-2', '#A7AEBE');
  CONTEXT_VIZ_THEME.selected = token('--text', '#EAECF2');
  CONTEXT_VIZ_THEME.pressure = {
    high: token('--rose', '#EC6A85'),
    mid: token('--amber', '#E4A85B'),
    ok: token('--green', '#58C08C'),
  };
}
contextResolveVizTheme();
// Live theme flips re-resolve and, when the Context pane is on screen,
// rebuild the scene so light mode never keeps dark-theme geometry.
new MutationObserver(() => {
  contextResolveVizTheme();
  if (contextPaneVisible()) scheduleContextPaneRender();
}).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

const contextViz = {
  canvas: null,
  renderer: null,
  scene: null,
  camera: null,
  root: null,
  meshes: [],
  raycaster: null,
  pointer: null,
  resizeObserver: null,
  raf: null,
  dragging: false,
  dragMoved: false,
  dragX: 0,
  dragY: 0,
  panMode: false,
  targetX: 0,
  targetY: 2.2,
  targetZ: 0,
  activePointers: new Map(),
  primaryPointerId: null,
  pinchDistance: 0,
  pinchZoom: 38,
  pinchCenterX: 0,
  pinchCenterY: 0,
  rotationX: 0.34,
  rotationY: -0.48,
  zoom: 38,
};

// analyzeContextSnapshot walks the entire raw payload, which is expensive at
// large context sizes. Both the legacy Context pane and the Station snapshot
// builders need the same analysis, so memoize it per snapshot object. Exact
// raw loads replace the snapshot object (replaceStoredContextSnapshot), which
// naturally invalidates the memo entry.
const contextAnalysisBySnapshot = new WeakMap();

function switchActivitySubtab(name) {
  if (activeActivitySubtab === name) return;
  activeActivitySubtab = name;
  document.querySelectorAll('#activity-subtabs .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.activityTab === name);
  });
  document.getElementById('activity-log-pane').classList.toggle('active', name === 'log');
  document.getElementById('activity-context-pane').classList.toggle('active', name === 'context');
  document.getElementById('activity-managed-pane').classList.toggle('active', name === 'managed');
  document.getElementById('activity-changes-pane').classList.toggle('active', name === 'changes');
  document.getElementById('activity-control-pane').classList.toggle('active', name === 'control');
  document.getElementById('activity-log-controls')?.classList.toggle('hidden', name !== 'log');
  syncSessionWindowGridControls();
  if (name === 'context') {
    renderContextPane();
    contextStartThreeAnimation();
  } else {
    setContextFocusMode(false);
    contextStopThreeAnimation();
  }
  if (name === 'managed') {
    refreshManagedContextPane({ force: true });
  }
  if (name === 'changes') {
    const badge = document.getElementById('badge-changes');
    if (badge) { badge.style.display = 'none'; badge.textContent = ''; }
    refreshChangesList({ selectFirst: true, refreshActive: true, quiet: true });
    // Populate the timeline on first view in case the user opened the
    // Changes tab after a session had already racked up history
    // (refreshHistory is a no-op when the endpoint 404s).
    if (typeof refreshHistory === 'function') refreshHistory();
  }
  if (name === 'control') {
    refreshControlPane();
  }
  return true;
}

let activeSessionsSubtab = 'recent';

function focusNewSessionInput() {
  const input = document.getElementById('new-session-input');
  if (!input) return;
  resizeTaskTextarea(input);
  input.focus();
  if (typeof input.setSelectionRange === 'function') {
    const end = input.value.length;
    input.setSelectionRange(end, end);
  }
}

function switchSessionsSubtab(name) {
  const next = VALID_SESSIONS_SUBTABS.includes(name) ? name : 'recent';
  if (currentSessionDetail) closeSessionDetail();
  activeSessionsSubtab = next;
  document.querySelectorAll('#sessions-subtabs .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.sessionsTab === next);
  });
  document.querySelectorAll('#tab-sessions .sessions-subtab-panes > .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `sessions-pane-${next}`);
  });
  if (next === 'new') {
    updateNewSessionProjectPrefills();
    requestAnimationFrame(focusNewSessionInput);
  } else if (next === 'worktrees') {
    if (worktreeHasScannedData(_cachedWorktreeScan)) {
      renderWorktrees(_cachedWorktreeScan);
      if (worktreesLoadInFlight === 'scan') {
        setWorktreesActivityNotice('pending', 'Scanning worktrees...');
      }
    } else {
      loadWorktrees({ forceScan: !worktreesLoaded });
    }
  } else if (!sessionsLoaded) {
    loadSessions();
  } else {
    renderSessionsViews();
  }
}

function openNewSessionFromPrompt() {
  const source = document.getElementById('activity-task-input');
  const dest = document.getElementById('new-session-input');
  const draft = source?.value || '';
  if (dest && draft.trim()) {
    dest.value = draft;
  }
  routeTo('sessions', 'new');
  requestAnimationFrame(focusNewSessionInput);
}
window.openNewSessionFromPrompt = openNewSessionFromPrompt;

const SETTINGS_SUBTAB_KEY = 'intendant_settings_subtab';
const ACCESS_SUBTAB_KEY = 'intendant_access_subtab';
let activeSettingsSubtab =
  normalizeSettingsSubtab(localStorage.getItem(SETTINGS_SUBTAB_KEY) || 'account');
if (activeSettingsSubtab === 'network') {
  activeSettingsSubtab = 'account';
  localStorage.setItem(SETTINGS_SUBTAB_KEY, activeSettingsSubtab);
}
let activeAccessSubtab =
  normalizeAccessSubtab(localStorage.getItem(ACCESS_SUBTAB_KEY) || 'overview');
if (!VALID_ACCESS_SUBTABS.includes(activeAccessSubtab)) {
  activeAccessSubtab = 'overview';
  localStorage.setItem(ACCESS_SUBTAB_KEY, activeAccessSubtab);
}

function switchAccessSubtab(name) {
  const next = normalizeAccessSubtab(name);
  if (activeAccessSubtab !== next) {
    activeAccessSubtab = next;
    localStorage.setItem(ACCESS_SUBTAB_KEY, activeAccessSubtab);
  }
  document.querySelectorAll('#access-subtabs .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.accessTab === activeAccessSubtab);
  });
  document.querySelectorAll('#tab-access .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `access-pane-${activeAccessSubtab}`);
  });
  renderAccessAdminSummaries();
  if (activeAccessSubtab === 'daemons') renderDaemonsList();
  if (activeAccessSubtab === 'overview' || activeAccessSubtab === 'people') {
    refreshAccessEnrollments({ silent: true }).catch(() => {});
  }
  if (activeAccessSubtab === 'overview') {
    refreshAccessConnectStatus({ silent: true }).catch(() => {});
  }
  if (activeAccessSubtab === 'diagnostics') renderConnectHealthPanel();
}

function switchSettingsSubtab(name) {
  name = normalizeSettingsSubtab(name);
  if (name === 'network') {
    routeTo('access', 'overview');
    return;
  }
  if (activeSettingsSubtab === name) return;
  activeSettingsSubtab = name;
  localStorage.setItem(SETTINGS_SUBTAB_KEY, name);
  document.querySelectorAll('#tab-settings .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.settingsTab === name);
  });
  document.querySelectorAll('#tab-settings .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `settings-pane-${name}`);
  });
  updateSettingsSaveRow();
  if (typeof ui2SettingsOnSubtabShown === 'function') ui2SettingsOnSubtabShown(name);
}

// Deep link to the Settings → Account API-keys card — the remediation
// unfueled notices point at ("Add API keys").
// Bridged onto window below: the dashboard runs as a module script, so
// inline onclick handlers (the unfueled banner's Add-keys button) and the
// validate-dashboard harness need an explicit global.
function focusSettingsApiKeys() {
  // The API-keys card lives in the Providers & models section.
  routeTo('settings', 'providers');
  requestAnimationFrame(() => {
    const heading = document.getElementById('settings-keys-heading');
    const card = heading?.closest('.ui-card') || heading;
    if (card?.scrollIntoView) card.scrollIntoView({ behavior: 'smooth', block: 'center' });
    const firstEmpty = ['settings-key-anthropic', 'settings-key-openai', 'settings-key-gemini']
      .map(id => document.getElementById(id))
      .find(el => el && !el.value.trim());
    firstEmpty?.focus();
  });
}
window.focusSettingsApiKeys = focusSettingsApiKeys;

// Apply the remembered sub-tab on initial render, before anything
// else toggles the default.
function applyInitialSettingsSubtab() {
  if (activeSettingsSubtab === 'account') {
    updateSettingsSaveRow();
    return;
  }
  document.querySelectorAll('#tab-settings .subtab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.settingsTab === activeSettingsSubtab);
  });
  document.querySelectorAll('#tab-settings .subtab-pane').forEach(pane => {
    pane.classList.toggle('active', pane.id === `settings-pane-${activeSettingsSubtab}`);
  });
  updateSettingsSaveRow();
}

// The main Save/Reset row only actually saves the Agent sub-tab's
// settings (CU, Presence, Transcription, Recording, Live Audio +
// External Agent). Account has its own "Save Keys" button inside the
// API Keys fieldset; Network applies each daemon immediately on Add;
// Debug is read-only. So we hide the global Save row on every
// sub-tab except Agent to prevent the "I clicked Save and nothing
// happened" confusion.
function updateSettingsSaveRow() {
  const row = document.querySelector('#tab-settings .settings-save-row');
  if (!row) return;
  // The batch-saved /api/settings fields live in the Providers & models
  // and Presence & voice sections; Autonomy is live-apply and Account &
  // advanced is read-only.
  const visible = activeSettingsSubtab === 'providers' || activeSettingsSubtab === 'presence';
  row.style.display = visible ? '' : 'none';
}

let initialRouteAppliedFromHash = false;
function applyInitialRouteFromHash() {
  if (initialRouteAppliedFromHash) return;
  initialRouteAppliedFromHash = true;
  applyCurrentRoute();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', applyInitialRouteFromHash, { once: true });
} else {
  applyInitialRouteFromHash();
}
