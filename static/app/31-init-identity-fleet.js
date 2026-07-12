'use strict';

import init, { PresenceWeb } from '/wasm-web/presence_web.js';
import stationInit, { StationWeb } from '/wasm-station/station_web.js';
import * as THREE from '/three.module.min.js';

// ── Legacy federation auth compatibility ──
//
// When the daemon's `[server.auth] bearer_token` is set, federation
// REST endpoints (/api/peers*, /api/coordinator/*, /api/sessions,
// /api/worktrees) and
// /ws all require the matching token. The dashboard stores the
// operator-supplied token in localStorage when configured by older
// builds or custom tooling and:
//
//   - includes it in `Authorization: Bearer <token>` on federation
//     fetch() calls via `authedFetch()`
//   - appends it as `?token=<token>` on the /ws URL via `buildWsUrl()`,
//     since browser `WebSocket` opens can't natively set headers
//
// When unset (the normal certificate-first path), all paths sail through with no Authorization
// header and no `?token=` query — the daemon's `verify_bearer_*`
// helpers treat None as "no enforcement."
const FEDERATION_TOKEN_KEY = 'intendantFederationBearerToken';

function getFederationToken() {
  try {
    return localStorage.getItem(FEDERATION_TOKEN_KEY) || '';
  } catch {
    return '';
  }
}

function setFederationToken(token) {
  try {
    if (token && token.trim()) {
      localStorage.setItem(FEDERATION_TOKEN_KEY, token.trim());
    } else {
      localStorage.removeItem(FEDERATION_TOKEN_KEY);
    }
  } catch {
    /* localStorage unavailable (private mode etc.); silently degrade */
  }
}

// Fetch wrapper that adds `Authorization: Bearer <token>` when a
// federation token is configured. Use for federation REST endpoints;
// discovery (/.well-known/...) and dashboard bootstrap (/config) use
// plain `fetch` since they're intentionally exempt from auth.
async function authedFetch(url, opts) {
  opts = opts || {};
  const token = getFederationToken();
  if (token) {
    opts = {
      ...opts,
      headers: { ...(opts.headers || {}), Authorization: `Bearer ${token}` },
    };
  }
  return fetch(url, opts);
}

// When running in the macOS app bundle, the page is served from a custom
// scheme (intendant://) for secure-context access to getUserMedia. WebSocket
// connections bypass the scheme handler and need the real backend address.
//
// Token: appends `?token=<token>` when a federation token is stored,
// so the daemon's `verify_bearer_for_ws` accepts the upgrade. Daemon-
// to-daemon connections (IntendantWsTransport) use the Authorization
// header instead; the server checks both.
function buildWsUrl(path) {
  let url;
  if (window.__intendantPort) {
    const scheme = window.__intendantBackendTls ? 'wss://' : 'ws://';
    url = scheme + '127.0.0.1:' + window.__intendantPort + (path || '');
  } else {
    const p = location.protocol === 'https:' ? 'wss:' : 'ws:';
    url = p + '//' + location.host + (path || '');
  }
  const token = getFederationToken();
  if (token) {
    const sep = url.includes('?') ? '&' : '?';
    url += sep + 'token=' + encodeURIComponent(token);
  }
  return url;
}

// ── Constants (rendering only — all logic in WASM) ──
const SPINNER_FRAMES = ['\u280b','\u2819','\u2839','\u2838','\u283c','\u2834','\u2826','\u2827','\u2807','\u280f'];
const LEVEL_ICONS = {
  info: '\u2139', model: '\u2726', agent: '\u25B6', error: '\u2716', warn: '\u26A0',
  subagent: '\u2726', detail: '\u00B7', debug: '\u2022', presence: '\u25C9',
};
const LEVEL_TOOLTIPS = {
  info: 'Info', model: 'Model', agent: 'Agent', error: 'Error', warn: 'Warning',
  subagent: 'Sub-agent', detail: 'Detail', debug: 'Debug', presence: 'Presence',
};

// ── Minimal JS state (rendering only) ──
let app = null;
let activeTab = 'activity';
let autoScroll = true;
// Deep links (#settings, …) run switchTab during script evaluation, which
// reads these once-per-page flags — declare them before that call, not
// next to their loaders further down (TDZ).
let settingsLoaded = false;
let apiKeyStatusLoaded = false;
// Deep-linking #sessions (and #sessions/worktrees) runs loadSessions /
// loadWorktrees synchronously on that same script-evaluation path — all
// sessions/worktrees module state they touch must be declared up here,
// not next to their renderers (TDZ).
const SESSION_LIST_RECENT_LIMIT = 600;
// Sessions render in viewport-sized pages: the initial window, the
// IntersectionObserver auto-append step, and the Show-more fallback step
// are all one page. Small pages keep the tab's DOM budget flat no matter
// how large the corpus grows.
const SESSION_CARD_RENDER_PAGE = 60;
const WORKTREE_CARD_RENDER_LIMIT = 200;
// Show-more render windows: how many cards the list renderers paint before
// offering a "Show N more" button (sessions also auto-grow via a scroll
// sentinel). Reset on filter/search/sort changes and on a fresh (uncached)
// list load.
let sessionsRenderWindow = SESSION_CARD_RENDER_PAGE;
let worktreesRenderWindow = WORKTREE_CARD_RENDER_LIMIT;
let _sessionsLoadToken = 0;
let _sessionsStreamAbort = null;
let _sessionDeepSearchToken = 0;
let _sessionDeepSearchAbort = null;
let _sessionDeepSearch = {
  query: '',
  mode: 'all_keywords',
  sourceFilter: 'all',
  active: false,
  loading: false,
  waiting: false,
  loaded: false,
  error: null,
  projectFilter: [],
  searched: 0,
  truncated: false,
  limit: 0,
  truncatedFiles: 0,
  results: new Map(),
};
// Quick-search message lane (feature flag: ?message_search=on): state of
// the last /api/sessions/message-search request, unioned into the Recent
// list under the metadata lane. Declared up here with the other sessions
// module state (deep-link TDZ, above); the lane's code lives in
// 57-sessions-message-search.js and the render touchpoints in
// 57-sessions-replay.js.
let _msgSearchFlagMemo = null;
let _sessionMsgSearchToken = 0;
let _sessionMsgSearchAbort = null;
let _sessionMsgSearchTimer = null;
let _sessionMsgSearch = {
  sig: '',            // request signature the current state answers
  query: '',          // raw query text of that request
  queryLower: '',     // lowercased twin (the metadata lane compares lowercased)
  active: false,      // results applied and live
  loading: false,     // request in flight (or scheduled retry)
  state: '',          // server coverage state: ready | building | partial
  partialReason: null,
  error: '',
  unavailable: '',    // non-empty: honest "cannot serve here" note
  hits: new Map(),    // `${source}:${session_id}` → response session entry
  extrasHint: 0,      // apply-time count of hits with no row in _cachedSessions
                      // (status-line hint; render-side stubs re-derive
                      // membership per pass from `hits`)
  moreAvailable: false, // server returned a cursor (further pages exist)
  windowDays: 0,
  seq: 0,             // bumps on every visible change; render keys include it
};
let _sessionsHydrationHideTimer = null;
let _sessionsHydrationState = {
  active: false,
  done: false,
  error: '',
  phase: 'idle',
  received: 0,
  limit: SESSION_LIST_RECENT_LIMIT,
};
let _sessionsRenderTimer = null;
let _sessionsRenderLastTs = 0;
const SESSIONS_RENDER_MIN_INTERVAL_MS = 250;
// Sessions list render-pass state (see 57-sessions-replay.js): the
// #sessions deep link runs loadSessions → resetSessionsListRenderState on
// this same script-evaluation path.
const _sessionsListRenderState = new Map(); // list element id → pass state
let _sessionCardBuildSeq = 0; // stamps built cards; QA asserts node identity by it
let worktreesRequestSerial = 0;
let worktreesActivityClearTimeout = null;
let worktreesLoadPromise = null;
const pendingWorktreeRemovals = new Set();
const worktreeRemovalQueue = [];
let activeWorktreeRemovalPath = '';
// #sessions/new runs updateNewSessionProjectPrefills → the project-status
// debounce on the same eval path.
let newSessionProjectStatusTimer = null;
let newSessionProjectObservedValue = null;

// ── Hidden-pane render deferral ──
// High-frequency events (transport ticks, update_usage, session refreshes)
// used to rebuild large DOM regions in panes the user wasn't looking at.
// Renderers route through renderOrDefer(tab, key, fn): visible panes render
// immediately; hidden panes remember only the LATEST render thunk per key
// and run it once when the pane is next shown (switchTab / tab visibility).
const paneDeferredRenders = new Map();
function paneIsVisible(tab) {
  return activeTab === tab && !document.hidden;
}
function renderOrDefer(tab, key, fn) {
  if (paneIsVisible(tab)) {
    paneDeferredRenders.get(tab)?.delete(key);
    fn();
    return;
  }
  let pending = paneDeferredRenders.get(tab);
  if (!pending) {
    pending = new Map();
    paneDeferredRenders.set(tab, pending);
  }
  pending.set(key, fn);
}
function flushPaneRenders(tab) {
  const pending = paneDeferredRenders.get(tab);
  if (!pending || !pending.size) return;
  paneDeferredRenders.delete(tab);
  for (const fn of pending.values()) {
    try { fn(); } catch (err) { console.warn('[dashboard] deferred render failed', err); }
  }
}
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) flushPaneRenders(activeTab);
});
// Diagnostic handle for QA harnesses (the app script is module-scoped, so
// probes can't reach these bindings by name).
window.__intendantPaneDiag = {
  activeTab: () => activeTab,
  deferredCounts: () => {
    const out = {};
    for (const [tab, pending] of paneDeferredRenders) out[tab] = pending.size;
    return out;
  },
  // QA hooks for the log scroll behavior (real append pipeline).
  logAppendTest: (n = 50, text = 'synthetic log entry') => {
    for (let i = 0; i < n; i++) {
      renderLogEntry({ level: 'info', source: 'debug', content: `${text} ${i}` });
    }
  },
  logMetrics: () => {
    const s = document.getElementById('log-stream');
    return {
      scrollTop: s ? s.scrollTop : -1,
      scrollHeight: s ? s.scrollHeight : -1,
      clientHeight: s ? s.clientHeight : -1,
      autoScroll,
      logEntryCount,
      newBelow: logNewBelowCount,
    };
  },
};

// While the reader is scrolled up in the live log, arriving entries never
// move the view; they are counted on the jump-to-live button instead.
let logNewBelowCount = 0;
function noteMainLogNewBelow(count = 1) {
  if (autoScroll || concurrentLogDetachedFragment) return;
  logNewBelowCount += count;
  const btn = document.getElementById('scroll-bottom');
  if (!btn) return;
  btn.classList.add('visible');
  btn.textContent = (logNewBelowCount > 99 ? '99+' : String(logNewBelowCount)) + ' ↓';
  btn.title = `${logNewBelowCount} new ${logNewBelowCount === 1 ? 'entry' : 'entries'} below — jump to live`;
}
function resetMainLogNewBelow() {
  logNewBelowCount = 0;
  const btn = document.getElementById('scroll-bottom');
  if (!btn) return;
  btn.textContent = '↓';
  btn.title = 'Scroll to bottom';
}

let logEntryCount = 0;
// Idle empty state for the live log: visible only while the stream has
// zero entries. Called wherever logEntryCount changes to/from zero.
function updateLogEmptyState() {
  const empty = document.getElementById('log-empty-state');
  if (empty) empty.classList.toggle('hidden', logEntryCount > 0);
}

/* First-run fueling nudge: when the daemon holds no provider credential,
   the Activity empty state says so and points at the fix instead of
   inviting a task that would only fail. Re-checked whenever the vault
   lease state refreshes, so it clears the moment fuel lands. */
const LOG_EMPTY_DEFAULT_TITLE = 'No activity yet';
const LOG_EMPTY_DEFAULT_HINT = 'Send a task below to start the agent, or pick a session from Sessions.';

/* External-agent availability (Codex / Claude Code): which backends this
   daemon can actually spawn. External agents run on their own accounts,
   so the fueling nudge must not read as "nothing works" while one is
   present — and the new-session picker greys out backends whose CLI is
   missing so a doomed spawn is visible before Start. */
let externalAgentAvailability = null; // null = not yet known
let externalAgentAvailabilityFetch = null;
function refreshExternalAgentAvailability() {
  if (!externalAgentAvailabilityFetch) {
    externalAgentAvailabilityFetch = fetchExternalAgentAvailability()
      .then(body => {
        if (Array.isArray(body?.external_agents)) {
          externalAgentAvailability = body.external_agents;
          applyExternalAgentAvailabilityToNewSessionPicker();
        }
        return externalAgentAvailability;
      })
      .catch(() => null)
      .finally(() => { externalAgentAvailabilityFetch = null; });
  }
  return externalAgentAvailabilityFetch;
}
function installedExternalAgents() {
  return Array.isArray(externalAgentAvailability)
    ? externalAgentAvailability.filter(agent => agent && agent.installed)
    : [];
}
function applyExternalAgentAvailabilityToNewSessionPicker() {
  const select = document.getElementById('new-session-agent');
  if (!select || !Array.isArray(externalAgentAvailability)) return;
  for (const agent of externalAgentAvailability) {
    const option = select.querySelector(`option[value="${agent.id}"]`);
    if (!option) continue;
    const missing = agent.installed === false;
    option.disabled = missing;
    option.title = missing
      ? `${agent.command || agent.id} was not found on the daemon host`
      : '';
  }
}

let unfueledCheckInFlight = false;
async function refreshUnfueledEmptyState() {
  if (unfueledCheckInFlight) return true;
  unfueledCheckInFlight = true;
  try {
    // Freshest first: an active lease means fueled, and the lease list
    // updates live as the vault grants/revokes (credentials.manage).
    if (vaultLeaseState.supported === true && vaultLeaseState.leases.length > 0) {
      applyUnfueledEmptyState(false);
      return true;
    }
    // The status frame carries an aggregate `fueled` flag readable by
    // every binding; per-provider api_key_status needs settings.manage,
    // which the default hosted (operator) binding does not hold.
    const statusFueled = dashboardControlTransport?.lastStatus?.fueled;
    if (typeof statusFueled === 'boolean') {
      if (!statusFueled) await refreshExternalAgentAvailability();
      applyUnfueledEmptyState(!statusFueled);
      return true;
    }
    const keys = await fetchApiKeyStatus();
    if (!keys || typeof keys !== 'object') return false;
    const unfueled = !(keys.openai || keys.anthropic || keys.gemini);
    if (unfueled) await refreshExternalAgentAvailability();
    applyUnfueledEmptyState(unfueled);
    return true;
  } catch {
    // Unknown (transport not up yet, or a daemon predating the flag):
    // keep the default copy rather than guessing.
    return false;
  } finally {
    unfueledCheckInFlight = false;
  }
}
function applyUnfueledEmptyState(unfueled) {
  const empty = document.getElementById('log-empty-state');
  const title = empty?.querySelector('.ui-empty-title');
  const hint = empty?.querySelector('.ui-empty-hint');
  if (!empty || !title || !hint) return;
  const action = document.getElementById('log-empty-fuel');
  const externalNote = document.getElementById('log-empty-external-agents');
  if (!unfueled) {
    if (empty.dataset.unfueled) {
      delete empty.dataset.unfueled;
      title.textContent = LOG_EMPTY_DEFAULT_TITLE;
      hint.textContent = LOG_EMPTY_DEFAULT_HINT;
      if (action) action.remove();
      if (externalNote) externalNote.remove();
    }
    return;
  }
  empty.dataset.unfueled = 'true';
  // Scope the claim to the built-in agent: an unfueled daemon still runs
  // external agents (their accounts), terminal, files, and display.
  title.textContent = 'The built-in agent has no fuel yet';
  if (DASHBOARD_CONNECT_MODE) {
    hint.textContent = 'This daemon is claimed and connected, but holds no provider '
      + 'credential for the built-in agent. Grant a time-boxed lease from your vault '
      + 'and it can start working.';
    if (!document.getElementById('log-empty-fuel')) {
      const btn = document.createElement('button');
      btn.id = 'log-empty-fuel';
      btn.className = 'ui-empty-btn';
      btn.textContent = 'Fuel from your vault';
      btn.addEventListener('click', () => routeTo('access', 'advanced'));
      hint.insertAdjacentElement('afterend', btn);
    }
  } else {
    hint.textContent = 'The daemon found no provider API key for the built-in agent. '
      + 'Add ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY to .env and restart — '
      + 'or claim it through a rendezvous and grant a lease from your vault.';
  }
  applyUnfueledExternalAgentNote(empty);
}

/* The nudge's counterweight: name the external agents that still work,
   by actual auth posture — a leased or locally-signed-in backend needs
   no fuel, a signed-out one needs its own login or an oauth lease from
   the vault. Only rendered while the unfueled state is showing. */
function applyUnfueledExternalAgentNote(empty) {
  const installed = installedExternalAgents();
  let note = document.getElementById('log-empty-external-agents');
  if (!installed.length) {
    if (note) note.remove();
    return;
  }
  if (!note) {
    note = document.createElement('div');
    note.id = 'log-empty-external-agents';
    note.className = 'ui-empty-hint';
    empty.appendChild(note);
  }
  const parts = installed.map(agent => {
    const name = agent.label || agent.id;
    if (agent.leased) {
      return `${name} is fueled with a subscription lease and ready`;
    }
    if (agent.local_login === true) {
      return `${name} is signed in on the daemon and needs no fuel`;
    }
    if (agent.local_login === false) {
      return `${name} is installed but signed out — sign it in on the daemon, or lease its subscription from your vault`;
    }
    return `${name} is installed and runs on its own login — or a subscription lease from your vault`;
  });
  note.textContent = parts.join('. ') + '.';
}
// Probe once the transport can answer; in connect mode the data channel
// (possibly via TURN) can take a while, so keep retrying for ~40s rather
// than racing it. First tick is deferred past script evaluation — the
// probe reads lexical bindings (vaultLeaseState, the transport) that are
// declared further down this script.
function scheduleUnfueledProbe(attempt = 0) {
  refreshUnfueledEmptyState().then(known => {
    if (!known && attempt < 20) setTimeout(() => scheduleUnfueledProbe(attempt + 1), 2000);
  });
}
setTimeout(scheduleUnfueledProbe, 0);
let commandOutputGroupSeq = 0;
const activeCommandOutputGroups = new Map();
const commandOutputGroups = new Map();
const commandOutputGroupByEntry = new WeakMap();
const logEntryCopyTextByEntry = new WeakMap();
const wiredLogCopyButtons = new WeakSet();
let currentPhase = 'idle';
let daemonSessionFullId = '';
let currentSessionFullId = '';
let foregroundSessionFullId = '';
let maximizedSessionWindowId = '';
const sessionWindows = new Map();
const recentSessionStatusPhases = new Map();
const sessionRelationships = new Map();
const sessionRelationshipHydrationInFlight = new Set();
// Relationship-hydration termination (2026-07-05 CPU-storm incident):
// every session ID that has EVER appeared in a served row (any list,
// stream, or poll path) — hydration treats these as resolved without
// touching `_cachedSessions`, so termination no longer depends on the
// Sessions pane having loaded.
const sessionRowSeenIds = new Set();
// IDs the daemon answered "no row" for — probed at most once per page
// load (old logs reference retired `parent`/`parent-session` placeholders,
// and live Claude Code Task children get non-addressable `task-*` ids).
// Cleared the moment a row for the ID arrives, so late-created sessions
// self-heal.
const sessionRelationshipHydrationUnresolved = new Set();
let processingLogReplay = false;
let logReplayAppendBatch = null;
const sessionMetadataById = new Map();
let promptTargetLogBadgeRenderedTarget = null;
let promptTargetLogBadgeScheduledTarget = null;
let promptTargetLogBadgeRefreshFrame = 0;
const sessionUsageById = new Map();
let latestGlobalUsage = null;
let dashboardProjectRoot = '';
let sessionRelationshipRenderHandle = 0;
let sessionWindowGridFitRenderHandle = 0;
let sessionGoalTicker = null;
let sessionVitalsTicker = null;
// Cache-expiry alert dedupe: session id → the lastActivityEpoch already
// alerted for (one alert per idle period).
const sessionCacheExpiryAlerts = new Map();
const SESSION_WINDOW_RENDER_LIMIT = 600;
const SESSION_WINDOW_PREPEND_CHUNK = 300;
// In-memory history behind the render window. Long-running sessions used
// to grow this without bound; beyond the limit the head is dropped (older
// entries remain reachable through the remote paging path) with hysteresis
// so the trim doesn't run on every append.
const SESSION_WINDOW_HISTORY_LIMIT = 5000;
const SESSION_WINDOW_HISTORY_RETAIN = 4000;
const SESSION_WINDOW_TOP_LOAD_THRESHOLD_PX = 64;
const SESSION_WINDOW_STATE_KEY = 'intendant_session_windows_v1';
const SESSION_WINDOW_RESTORE_LIMIT = 6;
const SESSION_WINDOW_RESTORE_LOG_LIMIT = 250;
const SESSION_TEXT_SIGNATURE_CHAR_LIMIT = 8192;
const SESSION_RENDERED_SIGNATURE_CHAR_LIMIT = 4096;
const STATION_ACTIVITY_TEXT_CHAR_LIMIT = 1200;
const STATION_ANCHOR_DETAIL_CHAR_LIMIT = 2000;
const COMMAND_OUTPUT_EAGER_RENDER_CHAR_LIMIT = 24000;
const SESSION_PENDING_ACTIVE_MS = 30000;
const SESSION_METADATA_REFRESH_MS = 15000;
const SESSION_WINDOW_GRID_HEIGHT_KEY = 'intendant_session_window_grid_height_v3';
const SESSION_WINDOW_GRID_MIN_HEIGHT = 180;
const SESSION_WINDOW_GRID_FIT_MIN_HEIGHT = 72;
const CONCURRENT_LOG_MIN_HEIGHT = 180;
const SESSION_WINDOW_GRID_DEFAULT_RATIO = 0.5;
const SESSION_WINDOW_GRID_MAX_RATIO = 0.8;
const SESSION_ATTACH_STATUS_FRESH_MS = 15000;
const SESSION_RELATIONSHIP_KINDS = new Set(['side', 'fork', 'subagent']);
const CONCURRENT_LOG_MODE_NORMAL = 'normal';
const CONCURRENT_LOG_MODE_MINIMIZED = 'minimized';
const CONCURRENT_LOG_MODE_MAXIMIZED = 'maximized';
const CONCURRENT_LOG_MODE_KEY = 'intendant_concurrent_log_mode_v1';
let sessionMetadataRefreshTimer = null;
let sessionMetadataRefreshInFlight = null;
let sessionWindowGridHeightPx = readStoredSessionWindowGridHeight();
let sessionWindowGridResizeDrag = null;
let concurrentLogMode = readStoredConcurrentLogMode();
let concurrentLogFitToSessionWindows = true;
let concurrentLogDetachedFragment = null;
let concurrentLogDetachedScrollTop = 0;
let restoredPersistedSessionWindows = false;
let restoringPersistedSessionWindows = false;
const sessionWindowRestoreInFlight = new Set();
const externalSessionWindowSyncInFlight = new Set();
const externalSessionWindowSyncLastAt = new Map();
const externalSessionWindowSyncTimers = new Map();
const EXTERNAL_SESSION_WINDOW_SYNC_COOLDOWN_MS = 1500;
let currentSessionDetail = null;
let currentSessionDetailContext = null;
let pendingApprovalId = null;
let pendingApprovalSessionId = '';
let steerCounter = 0;
let followUpCounter = 0;
const pendingFollowUpsById = new Map();
let editMessageDraft = null;
const approvalSessionIds = new Map();
let spinnerIdx = 0;
let spinnerInterval = null;

// Terminal (lazy)

// Multi-host: stable identity of the daemon serving this dashboard.
// Resolved from the Agent Card at startup.
//
// `selfPeerId` is the routing key — the full PeerId string from
// `card.id` (e.g. "intendant:nicks-mac"). Used as the host_id value
// stored on daemon entries, as the cache key in `hostStatsCache`,
// and as the self check in every "is this the self entry?" routing
// site. Two peers sharing a label stay distinct because the
// PeerId's kind prefix plus the server-assigned label uniqueness
// make it a stable identifier, and a renamed daemon keeps the same
// id-format stability.
//
// `selfHostLabel` is the display name — the short human-readable
// label from `card.label`. Used only in UI strings (status bar,
// dropdown options, row labels). Can change across restarts
// without affecting routing because the id is the stable key.
let selfPeerId = 'local';
let selfHostLabel = 'local';
let selfGitSha = '';
let selfVersion = '';

// Pure-client UI toggles and filters that persist across refreshes
// via localStorage. Declared up front so any `let` initializers below
// that read from localStorage (e.g. `activeHostFilter`) can reference
// these without hitting a JS temporal-dead-zone error.
const DIRECT_MODE_KEY = 'intendant_direct_mode';
const VERBOSITY_KEY = 'intendant_verbosity';
const HOST_FILTER_KEY = 'intendant_host_filter';
const SESSIONS_FILTER_PROJECT_KEY = 'intendant_sessions_filter_project';
const SESSIONS_FILTER_SOURCE_KEY = 'intendant_sessions_filter_source';
const SESSIONS_FILTER_STATUS_KEY = 'intendant_sessions_filter_status';
const SESSIONS_SHOW_SUBAGENTS_KEY = 'intendant_sessions_show_subagents';
// Message-lane superseded toggle (flagged; 57-sessions-message-search.js).
const SESSIONS_MSG_SUPERSEDED_KEY = 'intendant_sessions_msg_superseded';
const SESSIONS_DEEP_FILTER_PROJECT_KEY = 'intendant_sessions_deep_filter_project';
const SESSIONS_DEEP_FILTER_SOURCE_KEY = 'intendant_sessions_deep_filter_source';
const SESSIONS_DEEP_FILTER_STATUS_KEY = 'intendant_sessions_deep_filter_status';
const SESSION_SOURCE_FILTER_OPTIONS = [
  { value: 'intendant', label: 'Intendant', plural: 'sources' },
  { value: 'external', label: 'External agents', plural: 'sources' },
  { value: 'codex', label: 'Codex', plural: 'sources' },
  { value: 'claude-code', label: 'Claude', plural: 'sources' },
];
const SESSION_STATUS_FILTER_OPTIONS = [
  { value: 'active', label: 'Active', plural: 'statuses' },
  { value: 'idle', label: 'Idle', plural: 'statuses' },
  { value: 'resident', label: 'Resident', plural: 'statuses' },
  { value: 'completed', label: 'Completed', plural: 'statuses' },
  { value: 'failed', label: 'Failed', plural: 'statuses' },
  { value: 'abandoned', label: 'Abandoned', plural: 'statuses' },
  { value: 'interrupted', label: 'Interrupted', plural: 'statuses' },
];
let sessionProjectFilterOptionsCache = [];

// Current Activity host filter selection. Empty string = show all
// hosts. Otherwise matches log entries whose `data-host-id` equals
// this value. Persisted to localStorage so it survives refreshes.
let activeHostFilter = localStorage.getItem(HOST_FILTER_KEY) || '';

// Multi-host: latest `update_usage` payload per host_id. Primary's
// payload is stored under selfPeerId (replaces any earlier 'local'
// entry once the Agent Card resolves). Secondaries store under their
// own host_id (also a PeerId). The Stats host picker reads from this
// cache on switch so you can inspect any daemon's usage without
// round-tripping the WS.
const hostStatsCache = new Map();

// Current Stats tab host selection. Empty string or selfPeerId → self.
let activeStatsHost = '';

// Multi-host: list of configured secondary daemons. Each entry:
//   { host_id, label, url, connected }
// Hydrated from `GET /api/peers` — the server-side PeerRegistry is
// the single source of truth. Per-peer events flow through the
// primary `/ws` push pipeline tagged with `host_id`; the browser
// no longer opens its own WebSocket per secondary. The self host
// (the daemon serving app.html) isn't in this list — it's implicit
// and always rendered first in the UI.
const DAEMONS_KEY = 'intendant_daemons';
const DASHBOARD_TRANSPORT_KEY = 'intendant_dashboard_transport';
const ACCESS_FLEET_KEY = 'intendant_access_fleet_v1';
const dashboardUrlParams = new URLSearchParams(window.location.search);
const DASHBOARD_ACCESS_PAGE_MODE = /^\/access\/?$/i.test(window.location.pathname || '');
const DASHBOARD_CONNECT_MODE = dashboardUrlParams.get('connect') === '1';
const DASHBOARD_CONNECT_DAEMON_ID = String(dashboardUrlParams.get('daemon_id') || '').trim();
const DASHBOARD_CONNECT_SIGNALING_BASE = String(
  dashboardUrlParams.get('connect_base') || ''
).trim().replace(/\/+$/, '');
if (DASHBOARD_ACCESS_PAGE_MODE) {
  document.body?.classList.add('access-page');
  document.title = 'Intendant Access';
}
let daemons = [];
let dashboardAccessTargets = [];
let dashboardAccessOverview = null;
let accessUserClientGrantSubmitting = false;
const accessGrantLifecycleSubmitting = new Set();
let accessFleetHostedSyncTimer = null;
let accessFleetHostedSyncInFlight = false;
let accessFleetHostedSyncDirty = false;
let accessFleetHostedCsrfToken = '';
let dashboardControlTransport = null;
let dashboardTransport = null;
let dashboardServerMessageDispatcher = null;
// Frame types the daemon has denied for this connection's grant — each
// surfaces one toast per page load (the server re-sends the denial frame
// on every attempt; repeating the toast would just nag).
const wsDeniedToastShown = new Set();
let dashboardControlEventsActive = false;
let dashboardControlLastError = '';
// Coarse failure class for dashboardControlLastError so panes can give
// honest guidance: 'refused' (the daemon itself rejected the offer),
// 'transport' (answer received but ICE/DTLS/DataChannel never became
// ready), 'signaling' (no answer at all), or '' (unclassified).
let dashboardControlLastErrorKind = '';
let dashboardConnectReconnectTimer = null;
let dashboardConnectReconnectAttempt = 0;
let dashboardConnectReconnectInFlight = false;
let dashboardConnectReconnectReason = '';
let dashboardConnectReconnectNextUnixMs = 0;
let filesDownloadAbort = null;
let filesTransferSeq = 0;
let filesTransferRunnerActive = false;
// Declared with the rest of the transfer state: a #files deep link runs
// switchTab('files') → renderFilesTransfers() during script evaluation,
// before a declaration further down would have been initialized (TDZ).
let filesTransfersRenderScheduled = false;
const filesTransfers = [];
const filesStagedUploads = new Map();
const peerFileTransferConnections = new Map();
const peerDashboardControlConnections = new Map(); // hostId|sessionId -> PeerDashboardControlConnection
const peerDashboardControlConnectionsByHost = new Map(); // hostId -> PeerDashboardControlConnection
let filesTransferDbPromise = null;
const FILES_TRANSFER_STATE_KEY = 'intendant.files.transfers.v2';
const FILES_TRANSFER_DB_NAME = 'intendant-files-transfers-v2';
const FILES_TRANSFER_DB_VERSION = 1;
const DASHBOARD_EVENT_DEDUPE_MS = 2000;
const DASHBOARD_CONTROL_MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
const DASHBOARD_CONTROL_MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
const DASHBOARD_CONTROL_UPLOAD_CHUNK_BYTES = 16 * 1024;
const DASHBOARD_CONTROL_UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
const DASHBOARD_RANGED_DOWNLOAD_CHUNK_BYTES = 2 * 1024 * 1024;
const DASHBOARD_RANGED_DOWNLOAD_MAX_BYTES = 512 * 1024 * 1024;
const DASHBOARD_CONTROL_BINDING_CLOCK_SKEW_MS = 30000;
const DASHBOARD_CONNECT_RECONNECT_MIN_MS = 1000;
const DASHBOARD_CONNECT_RECONNECT_MAX_MS = 15000;
const dashboardRecentServerMessageKeys = new Map();
const DASHBOARD_DEDUPABLE_EVENT_NAMES = new Set([
  // Idempotent lifecycle/state events that can arrive over both the legacy
  // WebSocket and the Connect event stream during transport migration.
  'turn_started',
  'done_signal',
  'task_complete',
  'round_complete',
  'session_started',
  'session_identity',
  'session_relationship',
  'session_capabilities',
  'session_goal',
  'session_vitals',
  'session_attached',
  'session_ended',
  'status',
  'approval_required',
  'approval_resolved',
  'user_question',
  'follow_up_status',
  'user_message_edit_status',
  'user_message_rewind',
  'steer_requested',
  'steer_queued',
  'steer_accepted',
  'steer_delivered',
  'steer_cancelled',
  'display_ready',
  'display_resize',
  'display_capture_lost',
  'display_approval_pending',
  'user_display_granted',
  'user_display_revoked',
  'display_request_raised',
  'display_request_resolved',
  'shared_view',
  // Unique note_id per note makes the JSON.stringify dedupe key exact.
  'session_note',
  // Same shape: unique notification id per event.
  'user_notification',
  'recording_started',
  'recording_stopped',
  'recording_deleted',
  'recording_error',
  'external_agent_changed',
  'autonomy_changed',
  'codex_thread_action_requested',
  'codex_thread_action_result',
  'session_rename_result',
  'session_agent_config_result',
  'codex_config_changed',
  'claude_config_changed',
  'usage',
  'usage_update',
  'presence_usage_update',
  'live_usage_update',
  'browser_workspace_changed',
  'upload_ready',
  'upload_deleted',
  'file_changed',
  'snapshot_created',
  'rolled_back',
  'redone',
  'history_pruned',
  'conversation_rolled_back',
  'peer_added',
  'peer_removed',
  'peer_state_changed',
]);
const DASHBOARD_CONTROL_MSG_RPC_ACTIONS = new Set([
  'set_autonomy',
  'set_approval_rule',
  'set_external_agent',
  'set_codex_command',
  'set_codex_managed_command',
  'set_codex_sandbox',
  'set_codex_approval_policy',
  'set_codex_model',
  'set_claude_model',
  'set_claude_permission_mode',
  'set_claude_allowed_tools',
  'set_codex_reasoning_effort',
  'set_codex_service_tier',
  'set_codex_web_search',
  'set_codex_network_access',
  'set_codex_writable_roots',
  'set_codex_managed_context',
  'set_codex_context_archive',
  'set_verbosity',
]);
const DASHBOARD_SESSION_CONTROL_MSG_RPC_ACTIONS = new Set([
  'approve',
  'deny',
  'skip',
  'approve_all',
  'answer_question',
  'rename_session',
  'configure_session_agent',
  'stop_session',
  'restart_session',
  'create_session',
  'start_task',
  'resume_session',
  'follow_up',
  'cancel_follow_up',
  'edit_user_message',
  'interrupt',
  'steer',
  'cancel_steer',
]);
const DASHBOARD_ACTION_MSG_RPC_ACTIONS = new Set([
  'codex_thread_action',
  'take_display',
  'release_display',
  'grant_user_display',
  'revoke_user_display',
  'resolve_display_request',
  'create_virtual_display',
  'create_browser_workspace',
  'close_browser_workspace',
  'acquire_browser_workspace',
  'release_browser_workspace',
  'setup_debug_screen',
  'teardown_debug_screen',
  'start_debug_recording',
  'stop_debug_recording',
  'start_recording',
  'stop_recording',
  'delete_recording',
  'set_diagnostics_visual_marker',
]);

/* ── Browser client identity key ──
   The durable identity of this browser *on this origin*: a WebCrypto P-256
   keypair whose non-extractable private key lives in IndexedDB. Because
   browser storage is origin-scoped, a key created under one origin can
   never be wielded by code served from any OTHER origin — which is what
   lets daemons weight key-bound sessions by their enrollment origin's
   provenance (see docs/src/trust-architecture.md). Honest limit: origin
   scoping is only as strong as the origin's own naming — whoever can
   present a valid certificate for the origin's name IS the origin to this
   browser (the first-contact rungs in docs/src/trust-tiers.md). Offers
   carry the public key plus a signature over (daemon id, client nonce,
   sdp digest, timestamp); daemons resolve the key fingerprint against
   their local IAM. */

const CLIENT_IDENTITY_DB = 'intendant-client-identity';
const CLIENT_IDENTITY_STORE = 'keys';
const CLIENT_IDENTITY_RECORD = 'v1';
let clientIdentityCache = null;
let clientIdentityLoad = null;

function clientIdentitySupported() {
  return Boolean(window.isSecureContext && crypto?.subtle && window.indexedDB);
}

function clientIdentityOpenDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(CLIENT_IDENTITY_DB, 1);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(CLIENT_IDENTITY_STORE)) {
        req.result.createObjectStore(CLIENT_IDENTITY_STORE);
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error || new Error('indexedDB open failed'));
  });
}

function clientIdentityDbGet(db) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(CLIENT_IDENTITY_STORE, 'readonly');
    const req = tx.objectStore(CLIENT_IDENTITY_STORE).get(CLIENT_IDENTITY_RECORD);
    req.onsuccess = () => resolve(req.result || null);
    req.onerror = () => reject(req.error || new Error('indexedDB read failed'));
  });
}

function clientIdentityDbPut(db, record) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(CLIENT_IDENTITY_STORE, 'readwrite');
    tx.objectStore(CLIENT_IDENTITY_STORE).put(record, CLIENT_IDENTITY_RECORD);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error || new Error('indexedDB write failed'));
  });
}

function clientIdentityDbDelete(db) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(CLIENT_IDENTITY_STORE, 'readwrite');
    tx.objectStore(CLIENT_IDENTITY_STORE).delete(CLIENT_IDENTITY_RECORD);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error || new Error('indexedDB delete failed'));
  });
}

async function clientIdentityFingerprint(publicRaw) {
  const digest = await crypto.subtle.digest('SHA-256', publicRaw);
  return dashboardBytesToBase64Url(new Uint8Array(digest));
}

/* Load (or create on first use) this origin's identity key. Returns
   { privateKey, publicRawB64u, fingerprint, createdAtMs } or null when the
   platform cannot hold one (non-secure context, storage disabled). */
async function clientIdentityGet() {
  if (clientIdentityCache) return clientIdentityCache;
  if (!clientIdentitySupported()) return null;
  if (!clientIdentityLoad) {
    clientIdentityLoad = (async () => {
      try {
        const db = await clientIdentityOpenDb();
        let record = await clientIdentityDbGet(db);
        if (!record?.privateKey || !record?.publicRaw) {
          const pair = await crypto.subtle.generateKey(
            { name: 'ECDSA', namedCurve: 'P-256' },
            false,
            ['sign']
          );
          const publicRaw = await crypto.subtle.exportKey('raw', pair.publicKey);
          record = {
            privateKey: pair.privateKey,
            publicRaw,
            createdAtMs: Date.now(),
          };
          await clientIdentityDbPut(db, record);
        }
        db.close();
        const publicBytes = new Uint8Array(record.publicRaw);
        clientIdentityCache = {
          privateKey: record.privateKey,
          publicRawB64u: dashboardBytesToBase64Url(publicBytes),
          fingerprint: await clientIdentityFingerprint(record.publicRaw),
          createdAtMs: record.createdAtMs || 0,
        };
        return clientIdentityCache;
      } catch (err) {
        console.warn('[client-identity] unavailable:', err?.message || err);
        return null;
      } finally {
        clientIdentityLoad = null;
      }
    })();
  }
  return clientIdentityLoad;
}

async function clientIdentityReset() {
  clientIdentityCache = null;
  if (!clientIdentitySupported()) return;
  const db = await clientIdentityOpenDb();
  await clientIdentityDbDelete(db);
  db.close();
}

/* ── Signed fleet records (trust architecture phase 5) ──
   Fleet entries that round-trip through the hosted metadata store are
   signed with this browser's identity key and verified on read, so the
   store can remember the fleet but cannot invent or alter it unnoticed.
   Provenance is display metadata only — authority always stays with the
   target daemon. */

function accessFleetRecordPayload(record, signedAt, version = 2) {
  // v3 (encrypted records): the URL fields travel ONLY inside enc_fields,
  // so the signed lines pin them to empty — decrypting into the in-memory
  // record never invalidates the signature.
  // v4 folds the enc line in unconditionally (may be empty) and appends
  // the daemon's owner-set trust tier, so a store cannot relabel an
  // integrated box as disposable without breaking the signature.
  // v5 appends the owner's PETNAME for the daemon — the name the owner
  // chose, bound to this record's identity, so a lookalike daemon can
  // never wear a familiar name the store or a phisher picked.
  const enc = version >= 3 ? String(record.enc_fields || '') : '';
  const blank = version >= 3 && enc;
  const lines = [
    version >= 5 ? 'intendant-fleet-record-v5'
      : version >= 4 ? 'intendant-fleet-record-v4'
        : version >= 3 ? 'intendant-fleet-record-v3'
          : version >= 2 ? 'intendant-fleet-record-v2' : 'intendant-fleet-record-v1',
    String(record.host_id || record.id || ''),
    String(record.label || ''),
    blank ? '' : String(record.url || ''),
    blank ? '' : String(record.ws_url || ''),
    blank ? '' : String(record.browser_tcp_via_url || ''),
    String(record.connect_daemon_id || ''),
  ];
  // v2 adds the daemon-advertised rendezvous base (phase 7) so a synced
  // device connects through the daemon's own rendezvous, not a default.
  if (version >= 2) lines.push(String(record.connect_signaling_base || ''));
  if (version >= 3) lines.push(enc);
  if (version >= 4) lines.push(String(record.tier || ''));
  if (version >= 5) lines.push(String(record.petname || ''));
  lines.push(String(signedAt));
  return new TextEncoder().encode(lines.join('\n'));
}

/* ── Encrypted fleet sync (trust architecture phase 5 follow-on) ──
   The WebAuthn PRF extension turns the account passkey into a secret the
   server never sees; passkeys sync across a user's devices, so every
   device can derive the same AES-GCM key and the hosted store holds only
   ciphertext for the private fields (daemon URLs). No PRF support — or a
   device that hasn't unlocked — degrades to blank URLs with the record
   otherwise intact and verifiable. */

const FLEET_PRF_SESSION_KEY = 'intendant_fleet_prf_v1';
const FLEET_ENC_PREFIX = 'enc1:';
let accessFleetAesKey = null;

async function accessFleetEncryptionKey() {
  if (accessFleetAesKey) return accessFleetAesKey;
  try {
    const prfB64u = sessionStorage.getItem(FLEET_PRF_SESSION_KEY) || '';
    if (!prfB64u || !crypto?.subtle) return null;
    const hkdf = await crypto.subtle.importKey(
      'raw', dashboardBase64UrlToBytes(prfB64u), 'HKDF', false, ['deriveKey']
    );
    accessFleetAesKey = await crypto.subtle.deriveKey(
      {
        name: 'HKDF',
        hash: 'SHA-256',
        salt: new TextEncoder().encode('intendant-fleet-sync-v1'),
        info: new TextEncoder().encode('fleet-enc'),
      },
      hkdf,
      { name: 'AES-GCM', length: 256 },
      false,
      ['encrypt', 'decrypt']
    );
    return accessFleetAesKey;
  } catch (err) {
    console.warn('[fleet-sync] encryption key unavailable:', err?.message || err);
    return null;
  }
}

function accessFleetEncryptionAvailable() {
  return Boolean(sessionStorage.getItem(FLEET_PRF_SESSION_KEY));
}

/* The hosted copy of a record: URL fields sealed into enc_fields when the
   key is present; the local store keeps plaintext. */
async function accessFleetEncryptRecord(record) {
  const key = await accessFleetEncryptionKey();
  if (!key) return record;
  const secret = {
    url: String(record.url || ''),
    ws_url: String(record.ws_url || ''),
    browser_tcp_via_url: String(record.browser_tcp_via_url || ''),
  };
  if (!secret.url && !secret.ws_url && !secret.browser_tcp_via_url && !record.enc_fields) {
    return record;
  }
  try {
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ciphertext = await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv },
      key,
      new TextEncoder().encode(JSON.stringify(secret))
    );
    return {
      ...record,
      url: '',
      ws_url: '',
      browser_tcp_via_url: '',
      enc_fields: `${FLEET_ENC_PREFIX}${dashboardBytesToBase64Url(iv)}:${dashboardBytesToBase64Url(new Uint8Array(ciphertext))}`,
    };
  } catch (err) {
    console.warn('[fleet-sync] record encryption failed:', err?.message || err);
    return record;
  }
}

/* Fill the in-memory record from its envelope when the key is available;
   otherwise mark it locked (URLs stay blank, everything else works). */
async function accessFleetDecryptRecord(record) {
  const enc = String(record?.enc_fields || '');
  if (!enc.startsWith(FLEET_ENC_PREFIX)) return record;
  const key = await accessFleetEncryptionKey();
  if (!key) return { ...record, fleet_locked: true };
  try {
    const [ivB64u, ctB64u] = enc.slice(FLEET_ENC_PREFIX.length).split(':');
    const plaintext = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: dashboardBase64UrlToBytes(ivB64u) },
      key,
      dashboardBase64UrlToBytes(ctB64u)
    );
    const secret = JSON.parse(new TextDecoder().decode(plaintext));
    return {
      ...record,
      url: String(secret.url || ''),
      ws_url: String(secret.ws_url || ''),
      browser_tcp_via_url: String(secret.browser_tcp_via_url || ''),
      fleet_locked: false,
    };
  } catch (err) {
    console.warn('[fleet-sync] record decryption failed:', err?.message || err);
    return { ...record, fleet_locked: true };
  }
}

async function accessFleetSignRecord(record) {
  const identity = await clientIdentityGet();
  if (!identity) return record;
  const signedAt = Date.now();
  try {
    // Sign at the lowest version that covers the record's fields: a
    // record without newer fields keeps its older shape, so a hosted
    // store that predates a field (and would strip it) only downgrades
    // the records that carry it, not the whole fleet.
    const version = String(record.petname || '').trim() ? 5
      : String(record.tier || '').trim() ? 4
        : (record.enc_fields ? 3 : 2);
    const signature = await crypto.subtle.sign(
      { name: 'ECDSA', hash: 'SHA-256' },
      identity.privateKey,
      accessFleetRecordPayload(record, signedAt, version)
    );
    return {
      ...record,
      record_key: identity.publicRawB64u,
      record_sig: dashboardBytesToBase64Url(new Uint8Array(signature)),
      record_signed_at_unix_ms: signedAt,
    };
  } catch (err) {
    console.warn('[fleet-sync] record signing failed:', err?.message || err);
    return record;
  }
}

/* host_id → 'verified' | 'signed' | 'unverified' | 'hosted-claim'. */
const accessFleetProvenance = new Map();

async function accessFleetVerifyRecord(record) {
  if (String(record.source || '') === 'connect_daemon') return 'hosted-claim';
  const key = String(record.record_key || '');
  const sig = String(record.record_sig || '');
  const signedAt = Number(record.record_signed_at_unix_ms || 0);
  if (!key || !sig || !signedAt) return 'unverified';
  try {
    const publicKey = await crypto.subtle.importKey(
      'raw',
      dashboardBase64UrlToBytes(key),
      { name: 'ECDSA', namedCurve: 'P-256' },
      false,
      ['verify']
    );
    let valid = false;
    for (const version of record.enc_fields ? [5, 4, 3, 2, 1] : [5, 4, 2, 1]) {
      valid = await crypto.subtle.verify(
        { name: 'ECDSA', hash: 'SHA-256' },
        publicKey,
        dashboardBase64UrlToBytes(sig),
        accessFleetRecordPayload(record, signedAt, version)
      );
      if (valid) break;
    }
    if (!valid) return 'unverified';
    const mine = clientIdentityCache?.publicRawB64u === key;
    return mine ? 'verified' : 'signed';
  } catch {
    return 'unverified';
  }
}

async function accessFleetRefreshProvenance() {
  if (!clientIdentitySupported()) return;
  let changed = false;
  for (const record of accessFleetRead().targets || []) {
    const id = String(record.host_id || record.id || '').trim();
    if (!id) continue;
    const provenance = await accessFleetVerifyRecord(record);
    if (accessFleetProvenance.get(id) !== provenance) {
      accessFleetProvenance.set(id, provenance);
      changed = true;
    }
  }
  if (changed) renderAccessAdminSummaries();
}

