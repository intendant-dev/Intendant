// ── ui-v2 chrome wiring (design-overhaul P1a) ──────────────────────────
// Builds the nav rail's destination buttons and mirrors live state out of
// the v1 chrome elements, which stay in the DOM (hidden or interim-
// visible) and keep being driven by untouched v1 logic. The v2 chrome
// never computes truth of its own: it re-presents #phase-banner,
// #sb-budget-*, #sb-session, #sb-dashboard-transport, #sb-autonomy,
// #sb-host-label and #badge-activity, and proxies clicks to #stop-btn.
// Nav buttons carry `tab-btn` + data-tab so the existing router wires
// their clicks and active state exactly like the v1 tab bar.
//
// Runs in both generations: markup is built unconditionally (cheap,
// display:none under v1) so the router can bind at boot; observers and
// mirrors start only under html.ui-v2.

const UI2_NAV_GROUPS = [
  { label: 'Work', items: [
    { tab: 'activity', label: 'Activity', icon: 'activity' },
    { tab: 'sessions', label: 'Sessions', icon: 'sessions' },
  ] },
  { label: 'Watch', items: [
    { tab: 'displays', label: 'Live display', icon: 'live' },
    { tab: 'station', label: 'Station', icon: 'station' },
  ] },
  { label: 'Machine', items: [
    { tab: 'terminal', label: 'Terminal', icon: 'terminal' },
    { tab: 'files', label: 'Files', icon: 'files' },
  ] },
  { label: 'Insight', items: [
    { tab: 'stats', label: 'Usage', icon: 'stats' },
  ] },
  { label: 'Trust', items: [
    { tab: 'access', label: 'Access', icon: 'access' },
    // Vault's own destination arrives with its program phase; until then
    // this is an honest shortcut to today's vault home.
    { route: ['access', 'advanced'], label: 'Vault', icon: 'vault' },
  ] },
  { label: 'System', items: [
    { tab: 'settings', label: 'Settings', icon: 'settings' },
    { tab: 'debug', label: 'Debug', icon: 'debug' },
  ] },
];

const UI2_TAB_TITLES = {};

function ui2BuildNav() {
  const groupsHost = document.getElementById('ui2-nav-groups');
  if (!groupsHost) return;
  for (const group of UI2_NAV_GROUPS) {
    const wrap = document.createElement('div');
    wrap.className = 'ui2-nav-group';
    const eyebrow = document.createElement('div');
    eyebrow.className = 'ui2-nav-eyebrow ui2-nav-label';
    eyebrow.textContent = group.label;
    wrap.appendChild(eyebrow);
    for (const item of group.items) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'ui2-nav-item' + (item.tab ? ' tab-btn' : '');
      if (item.tab) {
        btn.dataset.tab = item.tab;
        UI2_TAB_TITLES[item.tab] = item.label;
      } else if (item.route) {
        btn.addEventListener('click', () => routeTo(item.route[0], item.route[1]));
      }
      btn.title = item.label;
      btn.innerHTML =
        `<span class="ui2-nav-icon">${ui2Icon(item.icon, 18)}</span>` +
        `<span class="ui2-nav-item-label ui2-nav-label">${item.label}</span>`;
      if (item.tab === 'activity') {
        const badge = document.createElement('span');
        badge.className = 'ui2-nav-badge';
        badge.id = 'ui2-badge-activity';
        badge.hidden = true;
        btn.appendChild(badge);
      }
      wrap.appendChild(btn);
    }
    groupsHost.appendChild(wrap);
  }
  const stopIcon = document.getElementById('ui2-stop-icon');
  if (stopIcon) stopIcon.innerHTML = ui2Icon('stop', 13);
  const dialIcon = document.getElementById('ui2-autonomy-icon');
  if (dialIcon) dialIcon.innerHTML = ui2Icon('dial', 18);
  const chev = document.getElementById('ui2-host-chev');
  if (chev) chev.innerHTML = ui2Icon('chev', 15);
}

// Observe one source element; run apply() now and on any mutation.
function ui2Mirror(sourceId, apply, opts) {
  const el = document.getElementById(sourceId);
  if (!el) return;
  const run = () => apply(el);
  new MutationObserver(run).observe(el, opts || {
    attributes: true, childList: true, characterData: true, subtree: true,
  });
  run();
}

function ui2PhaseCategory(className) {
  const m = /phase-(idle|thinking|running|waiting|done)/.exec(className || '');
  return m ? m[1] : 'idle';
}

function ui2WireMirrors() {
  // Phase pill: category from #phase-banner's class, label from #phase-text
  // verbatim — the real 12-phase vocabulary, not the prototype's 3 states.
  ui2Mirror('phase-banner', (banner) => {
    const pill = document.getElementById('ui2-phase-pill');
    const text = document.getElementById('ui2-phase-text');
    if (!pill || !text) return;
    pill.dataset.cat = ui2PhaseCategory(banner.className);
    const label = document.getElementById('phase-text');
    text.textContent = ((label && label.textContent) || 'Idle').trim() || 'Idle';
  });

  // Stop: visible exactly when the v1 button is; label follows
  // ("Interrupting…"); click proxies so all interrupt logic stays v1's.
  ui2Mirror('stop-btn', (src) => {
    const stop = document.getElementById('ui2-stop-btn');
    if (!stop) return;
    const shown = getComputedStyle(src).display !== 'none' && !src.hidden;
    stop.hidden = !shown;
    stop.disabled = src.disabled;
    const label = document.getElementById('ui2-stop-label');
    if (label) label.textContent = (src.textContent || 'Stop').replace(/^[■\s]+/, '').trim() || 'Stop';
  });
  const stopBtn = document.getElementById('ui2-stop-btn');
  if (stopBtn) stopBtn.addEventListener('click', () => {
    const src = document.getElementById('stop-btn');
    if (src) src.click();
  });

  // Context budget.
  ui2Mirror('sb-budget-pct', (src) => {
    const pct = parseFloat(src.textContent) || 0;
    const fill = document.getElementById('ui2-ctx-fill');
    const label = document.getElementById('ui2-ctx-pct');
    if (fill) fill.style.width = Math.max(0, Math.min(100, pct)) + '%';
    if (label) label.textContent = (src.textContent || '0%').trim();
  });

  // Daemon session id.
  ui2Mirror('sb-session', (src) => {
    const out = document.getElementById('ui2-session-id');
    if (out) {
      const id = (src.textContent || '').trim();
      out.textContent = id ? `· session ${id}` : '';
    }
  });

  // Transport summary: label text + ok/warn/err state, click → diagnostics.
  ui2Mirror('sb-dashboard-transport', (src) => {
    const conn = document.getElementById('ui2-conn');
    const label = document.getElementById('ui2-conn-label');
    if (!conn || !label) return;
    label.textContent = (src.textContent || '').replace(/\s+/g, ' ').trim() || 'connecting…';
    const cls = src.className + ' ' + [...src.querySelectorAll('*')].map((n) => n.className).join(' ');
    conn.dataset.state = /err|fail/.test(cls) ? 'err' : /warn|reconnect|checking|relay/i.test(cls) ? 'warn' : /ok|ready/i.test(cls) ? 'ok' : '';
  });
  const conn = document.getElementById('ui2-conn');
  if (conn) conn.addEventListener('click', () => routeTo('access', 'diagnostics'));

  // Autonomy chip: level text mirrored; click opens Settings (the design's
  // behavior — one-click cycling stays available on the v1 strip).
  ui2Mirror('sb-autonomy', (src) => {
    const level = (src.textContent || '').trim() || '—';
    const out = document.getElementById('ui2-autonomy-level');
    const btn = document.getElementById('ui2-autonomy-btn');
    if (out) out.textContent = level;
    if (btn) btn.dataset.level = level.toLowerCase();
  });
  const autonomyBtn = document.getElementById('ui2-autonomy-btn');
  if (autonomyBtn) autonomyBtn.addEventListener('click', () => routeTo('settings', 'agent'));

  // Host identity: nav host button + avatar initials.
  ui2Mirror('sb-host-label', (src) => {
    const name = (src.textContent || '').trim() || 'local';
    const hostName = document.getElementById('ui2-host-name');
    const idName = document.getElementById('ui2-identity-name');
    const avatar = document.getElementById('ui2-identity-avatar');
    if (hostName) hostName.textContent = name;
    if (idName) idName.textContent = name;
    if (avatar) avatar.textContent = name.replace(/[^a-z0-9]/gi, '').slice(0, 2).toUpperCase() || '·';
  });
  const hostBtn = document.getElementById('ui2-host-btn');
  if (hostBtn) hostBtn.addEventListener('click', () => routeTo('access', 'daemons'));

  // Activity badge.
  ui2Mirror('badge-activity', (src) => {
    const badge = document.getElementById('ui2-badge-activity');
    if (!badge) return;
    const text = (src.textContent || '').trim();
    const shown = src.style.display !== 'none' && text;
    badge.hidden = !shown;
    badge.textContent = text;
  });

  // Page title tracks the active pane (programmatic switches included:
  // switchTab pushState()s without a hashchange, so watch pane classes).
  const updateTitle = () => {
    const active = document.querySelector('.tab-pane.active');
    const title = document.getElementById('ui2-page-title');
    if (!active || !title) return;
    const tab = active.id.replace(/^tab-/, '');
    title.textContent = UI2_TAB_TITLES[tab] || tab;
    document.querySelectorAll('#ui2-nav .ui2-nav-item[data-tab]').forEach((btn) => {
      // switchTab() owns .active on .tab-btn but early-returns when the
      // boot route is already the active pane — sync from pane truth so
      // the initial state is right too.
      btn.classList.toggle('active', btn.dataset.tab === tab);
      if (btn.dataset.tab === tab) btn.setAttribute('aria-current', 'page');
      else btn.removeAttribute('aria-current');
    });
  };
  const paneObserver = new MutationObserver(updateTitle);
  document.querySelectorAll('.tab-pane').forEach((pane) => {
    paneObserver.observe(pane, { attributes: true, attributeFilter: ['class'] });
  });
  updateTitle();
}

ui2BuildNav();
if (ui2Enabled()) {
  document.addEventListener('DOMContentLoaded', ui2WireMirrors, { once: true });
  if (document.readyState !== 'loading') ui2WireMirrors();
}
