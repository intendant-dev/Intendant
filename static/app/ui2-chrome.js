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
    { tab: 'vault', label: 'Vault', icon: 'vault' },
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
  // Read the INLINE display (what updateStopButtonVisibility writes), not
  // the computed one — under v2 the composer CSS hides the v1 button
  // unconditionally, so computed display is always none.
  ui2Mirror('stop-btn', (src) => {
    const stop = document.getElementById('ui2-stop-btn');
    if (!stop) return;
    const shown = src.style.display !== 'none' && !src.hidden;
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

  // Autonomy chip: level text mirrored (+ the truthful short tag); click
  // opens Settings → Autonomy & approvals (the design's behavior —
  // one-click cycling stays available on the v1 strip).
  ui2Mirror('sb-autonomy', (src) => {
    const level = (src.textContent || '').trim() || '—';
    const tag = UI2_AUTONOMY_TAGS[level];
    const out = document.getElementById('ui2-autonomy-level');
    const btn = document.getElementById('ui2-autonomy-btn');
    if (out) out.textContent = tag ? `${level} · ${tag}` : level;
    if (btn) btn.dataset.level = level.toLowerCase();
  });
  const autonomyBtn = document.getElementById('ui2-autonomy-btn');
  if (autonomyBtn) autonomyBtn.addEventListener('click', () => routeTo('settings', 'autonomy'));

  // Backend facts absorbed from the hidden v1 strips (provider·model,
  // turn, tokens·cost). The pinned #sb-*/#tk-* elements stay in the DOM
  // as the data source; these are pure mirrors.
  const factsModel = () => {
    const p = (document.getElementById('sb-provider')?.textContent || '').trim();
    const m = (document.getElementById('sb-model')?.textContent || '').trim();
    const el = document.getElementById('ui2-fact-model');
    if (!el) return;
    const real = (v) => v && v !== '--';
    // Placeholder facts go quiet instead of reading as debug output ("—").
    const live = real(p) || real(m);
    el.textContent = live ? `${real(p) ? p : '?'} · ${real(m) ? m : '?'}` : '';
    el.style.display = live ? '' : 'none';
  };
  ui2Mirror('sb-provider', factsModel);
  ui2Mirror('sb-model', factsModel);
  ui2Mirror('sb-turn', (src) => {
    const el = document.getElementById('ui2-fact-turn');
    if (el) el.textContent = (src.textContent || 'T0').trim();
  });
  const factsTokens = () => {
    const t = (document.getElementById('tk-tokens')?.textContent || '').trim();
    const c = (document.getElementById('tk-cost')?.textContent || '').trim();
    const el = document.getElementById('ui2-fact-tokens');
    if (!el) return;
    // "-- tok · $0" at rest is noise; show the fact once tokens are real.
    const live = t && !/^--/.test(t);
    el.textContent = live ? (c ? `${t} · ${c}` : t) : '';
    el.style.display = live ? '' : 'none';
  };
  ui2Mirror('tk-tokens', factsTokens);
  ui2Mirror('tk-cost', factsTokens);

  // Session switcher: the Focus session / composer target, as a control.
  // Options rebuild from the live window set; selection drives the same
  // focusSessionWindow() path as clicking a window. "All sessions" clears
  // the Focus filter back to the combined stream.
  //
  // Wired strictly on DOMContentLoaded: this block reads `sessionWindows`,
  // a `let` a later fragment declares in the shared module scope — at
  // chrome-boot time it is in its TDZ, and even `typeof` on a TDZ binding
  // THROWS (the module-boot rule's sharpest edge; a throw here kills
  // every fragment after this one).
  const ui2WireSwitcher = () => {
  const switcher = document.getElementById('ui2-session-switcher');
  const rebuildSwitcher = () => {
    if (!switcher || typeof sessionWindows === 'undefined') return;
    const target = typeof resolvePromptTargetSessionId === 'function'
      ? (resolvePromptTargetSessionId() || '') : '';
    const options = [['', 'all sessions']];
    for (const [sid] of sessionWindows) {
      let label = sid.slice(0, 8);
      if (typeof sessionIdentityParts === 'function') {
        const parts = sessionIdentityParts(sid) || {};
        label = parts.name || parts.shortId || label;
      }
      options.push([sid, label]);
    }
    const sig = options.map((o) => o[0]).join(',') + '|' + target;
    if (switcher.dataset.sig === sig) return;
    switcher.dataset.sig = sig;
    switcher.replaceChildren(...options.map(([value, label]) => {
      const opt = document.createElement('option');
      opt.value = value;
      opt.textContent = label;
      return opt;
    }));
    switcher.value = target;
    if (switcher.value !== target) switcher.value = '';
  };
  if (switcher) {
    switcher.addEventListener('change', () => {
      const sid = switcher.value;
      if (sid && typeof focusSessionWindow === 'function') focusSessionWindow(sid);
      else if (!sid && typeof discardPromptTargetReference === 'function') {
        const current = typeof resolvePromptTargetSessionId === 'function'
          ? resolvePromptTargetSessionId() : '';
        if (current) discardPromptTargetReference(current);
        if (typeof updatePromptTargetSessionHighlight === 'function') updatePromptTargetSessionHighlight();
      }
      if (typeof ui2ApplyFocusFilter === 'function') ui2ApplyFocusFilter();
    });
    ui2Mirror('task-target-chip', rebuildSwitcher);
    const grid = document.getElementById('session-window-grid');
    if (grid) new MutationObserver(rebuildSwitcher).observe(grid, { childList: true });
    rebuildSwitcher();
  }
  };
  if (document.readyState === 'complete') ui2WireSwitcher();
  else document.addEventListener('DOMContentLoaded', ui2WireSwitcher, { once: true });

  // Prominent theme toggle: icon shows the current theme; click flips.
  const themeBtn = document.getElementById('ui2-theme-btn');
  const themeIconSync = () => {
    const icon = document.getElementById('ui2-theme-icon');
    if (!icon || typeof ui2Theme !== 'function') return;
    const light = ui2Theme() === 'light';
    icon.innerHTML = ui2Icon(light ? 'sun' : 'moon', 16);
    if (themeBtn) themeBtn.title = light ? 'Switch to dark theme' : 'Switch to light theme';
  };
  if (themeBtn) {
    themeBtn.addEventListener('click', () => {
      ui2SetTheme(ui2Theme() === 'light' ? 'dark' : 'light');
      themeIconSync();
      if (typeof ui2SettingsRenderAppearance === 'function') ui2SettingsRenderAppearance();
    });
    new MutationObserver(themeIconSync).observe(document.documentElement, {
      attributes: true, attributeFilter: ['data-theme'],
    });
    themeIconSync();
  }

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
    // Stamp the active tab on <html> so v2 CSS can key tab-scoped chrome
    // (the Focus/Grid layout toggle shows only on Activity).
    document.documentElement.dataset.ui2Tab = tab;
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

// ── ⌘K command palette (P1b + phase 2) ────────────────────────────────
// Sections, in order: destinations (under the pane's static "Go to"
// eyebrow), Sessions (fuzzy match over the cached session corpus, only
// while typing), and Actions (contextual verbs + the theme toggle). All
// cross-fragment state is read by name at event time with typeof guards —
// the palette lives in an early fragment.

// Light fuzzy: exact substring first, else the query's characters in
// order (subsequence). Returns a sort score, higher = better; -1 = miss.
function ui2FuzzyScore(query, haystack) {
  if (!query) return 0;
  const q = query.toLowerCase();
  const h = String(haystack || '').toLowerCase();
  if (!h) return -1;
  const at = h.indexOf(q);
  if (at >= 0) return 1000 - at;
  let hi = 0;
  for (let qi = 0; qi < q.length; qi++) {
    if (q[qi] === ' ') continue;
    hi = h.indexOf(q[qi], hi);
    if (hi < 0) return -1;
    hi += 1;
  }
  return 1;
}

function ui2PaletteSessionEntries(q) {
  if (!q || q.length < 2 || typeof _cachedSessions === 'undefined' || !Array.isArray(_cachedSessions)) return [];
  // Bound the per-keystroke scan: the corpus can be tens of thousands of
  // rows; the first slice is the recent end, which is what palette
  // jumping is for. Deep history stays reachable via Sessions search.
  const rows = _cachedSessions.length > 4000 ? _cachedSessions.slice(0, 4000) : _cachedSessions;
  const scored = [];
  for (const s of rows) {
    if (!s || typeof s !== 'object') continue;
    const sid = String(s.session_id || '');
    const name = String(s.name || '').trim();
    const task = String(s.task || '').trim();
    const label = name || task || sid.slice(0, 8) || 'session';
    const score = Math.max(
      ui2FuzzyScore(q, name),
      ui2FuzzyScore(q, task),
      ui2FuzzyScore(q, sid),
    );
    if (score < 0) continue;
    scored.push({ score, entry: {
      section: 'Sessions',
      icon: 'sessions',
      label,
      hint: sid.slice(0, 8),
      matchless: true,
      run: () => {
        if (typeof sessionWindows !== 'undefined' && sid && sessionWindows.has(sid)
            && typeof focusSessionWindow === 'function') {
          routeTo('activity');
          focusSessionWindow(sid);
          return;
        }
        routeTo('sessions');
        if (typeof openSessionDetail === 'function') openSessionDetail(s);
      },
    } });
  }
  scored.sort((a, b) => b.score - a.score);
  return scored.slice(0, 6).map(x => x.entry);
}

function ui2PaletteActionEntries(q) {
  const entries = [];
  // Pending approval verbs — only while one is actually on screen.
  if (typeof pendingApprovalId !== 'undefined' && pendingApprovalId !== null
      && typeof window.sendApproval === 'function') {
    entries.push({
      section: 'Actions', icon: 'check', label: 'Approve pending approval',
      run: () => window.sendApproval('approve'),
    });
    entries.push({
      section: 'Actions', icon: 'stop', label: 'Deny pending approval',
      run: () => window.sendApproval('deny'),
    });
  }
  // Stop — mirrors the v1 button's visibility (inline display, like the
  // oversight-bar proxy).
  const stopSrc = document.getElementById('stop-btn');
  if (stopSrc && stopSrc.style.display !== 'none' && !stopSrc.disabled) {
    entries.push({
      section: 'Actions', icon: 'stop', label: 'Stop current session',
      run: () => stopSrc.click(),
    });
  }
  entries.push({
    section: 'Actions', icon: 'plus', label: 'New session',
    run: () => routeTo('sessions', 'new'),
  });
  // Deep search with the typed text — additive: prefill + focus only, the
  // pane itself is untouched.
  const deepQuery = (q || '').trim();
  if (deepQuery.length >= 2) {
    entries.push({
      section: 'Actions', icon: 'search', label: `Deep search: “${deepQuery}”`,
      matchless: true,
      run: () => {
        routeTo('sessions', 'deep');
        const input = document.getElementById('sessions-deep-search-query');
        if (input) {
          input.value = deepQuery;
          input.focus();
          input.select?.();
        }
      },
    });
  }
  // Layout verbs (CONTRACT: window.intendantLayouts is another agent's
  // module — { save(), list() -> names|{name}[], apply(name) }; hidden
  // entirely when absent or partial).
  const layouts = window.intendantLayouts;
  if (layouts && typeof layouts === 'object') {
    if (typeof layouts.save === 'function') {
      entries.push({
        section: 'Actions', icon: 'station', label: 'Save layout…',
        run: () => { try { layouts.save(); } catch (e) { console.warn('[ui2] layout save failed', e); } },
      });
    }
    if (typeof layouts.list === 'function' && typeof layouts.apply === 'function') {
      let names = [];
      try { names = layouts.list() || []; } catch (_) { names = []; }
      for (const item of names.slice(0, 8)) {
        const name = typeof item === 'string' ? item : String(item?.name || '');
        if (!name) continue;
        entries.push({
          section: 'Actions', icon: 'station', label: `Apply layout ${name}`,
          run: () => { try { layouts.apply(name); } catch (e) { console.warn('[ui2] layout apply failed', e); } },
        });
      }
    }
  }
  // The theme toggle keeps its palette seat.
  const light = typeof ui2Theme === 'function' && ui2Theme() === 'light';
  entries.push({
    section: 'Actions', icon: 'dial',
    label: light ? 'Switch to dark theme' : 'Switch to light theme',
    action: 'theme',
  });
  return entries;
}

function ui2PaletteEntries(q) {
  const query = (q || '').trim().toLowerCase();
  const entries = [];
  for (const group of UI2_NAV_GROUPS) {
    for (const item of group.items) {
      if (item.tab === 'debug') continue;
      entries.push(item);
    }
  }
  // Label-only matching for labeled entries: users type what they SEE
  // (id matching surprised — "sta" surfaced Usage via its internal id).
  // `matchless` entries carry the query themselves (sessions already
  // matched; the deep-search verb embeds it).
  const filtered = entries.filter((item) => !query || item.label.toLowerCase().includes(query));
  const actions = ui2PaletteActionEntries(q)
    .filter((item) => item.matchless || !query || item.label.toLowerCase().includes(query));
  return [...filtered, ...ui2PaletteSessionEntries(query), ...actions];
}

const ui2Palette = { open: false, selected: 0, entries: [] };

function ui2PaletteRender(filter) {
  const list = document.getElementById('ui2-palette-list');
  if (!list) return;
  const activePane = document.querySelector('.tab-pane.active');
  const activeTab = activePane ? activePane.id.replace(/^tab-/, '') : '';
  ui2Palette.entries = ui2PaletteEntries(filter);
  ui2Palette.selected = Math.min(ui2Palette.selected, Math.max(0, ui2Palette.entries.length - 1));
  list.innerHTML = '';
  if (!ui2Palette.entries.length) {
    const empty = document.createElement('div');
    empty.className = 'ui2-palette-empty';
    empty.textContent = 'No matches';
    list.appendChild(empty);
    return;
  }
  let lastSection = '';
  ui2Palette.entries.forEach((item, i) => {
    // Small section headers (Sessions / Actions); destinations stay under
    // the pane's static "Go to" eyebrow.
    if (item.section && item.section !== lastSection) {
      const eyebrow = document.createElement('div');
      eyebrow.className = 'ui2-palette-eyebrow';
      eyebrow.textContent = item.section;
      list.appendChild(eyebrow);
    }
    lastSection = item.section || lastSection;
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'ui2-palette-row' + (i === ui2Palette.selected ? ' selected' : '');
    row.setAttribute('role', 'option');
    const isCurrent = item.tab && item.tab === activeTab;
    // Session labels are user/session data — DOM text, never innerHTML.
    const icon = document.createElement('span');
    icon.className = 'ui2-nav-icon';
    icon.innerHTML = ui2Icon(item.icon, 17);
    const label = document.createElement('span');
    label.className = 'ui2-palette-row-label';
    label.textContent = item.label;
    const hint = document.createElement('span');
    hint.className = 'ui2-palette-row-hint';
    hint.textContent = item.run
      ? (item.hint || 'run')
      : (isCurrent ? 'current' : 'go');
    row.append(icon, label, hint);
    row.addEventListener('click', () => ui2PaletteGo(item));
    row.addEventListener('mousemove', () => {
      if (ui2Palette.selected !== i) { ui2Palette.selected = i; ui2PaletteRender(ui2PaletteInput().value); }
    });
    list.appendChild(row);
  });
}

function ui2PaletteInput() { return document.getElementById('ui2-palette-input'); }

function ui2PaletteGo(item) {
  ui2PaletteClose();
  if (item.action === 'theme') {
    ui2SetTheme(ui2Theme() === 'light' ? 'dark' : 'light');
    if (typeof ui2SettingsRenderAppearance === 'function') ui2SettingsRenderAppearance();
    return;
  }
  if (typeof item.run === 'function') {
    try { item.run(); } catch (e) { console.warn('[ui2] palette action failed', e); }
    return;
  }
  if (item.tab) routeTo(item.tab);
  else if (item.route) routeTo(item.route[0], item.route[1]);
}

function ui2PaletteOpen() {
  const backdrop = document.getElementById('ui2-palette-backdrop');
  if (!backdrop || ui2Palette.open) return;
  ui2Palette.open = true;
  ui2Palette.selected = 0;
  const activePane = document.querySelector('.tab-pane.active');
  backdrop.classList.toggle('ui2-no-blur', !!activePane && activePane.id === 'tab-station');
  backdrop.hidden = false;
  const input = ui2PaletteInput();
  if (input) { input.value = ''; input.focus(); }
  ui2PaletteRender('');
}

function ui2PaletteClose() {
  const backdrop = document.getElementById('ui2-palette-backdrop');
  if (!backdrop || !ui2Palette.open) return;
  ui2Palette.open = false;
  backdrop.hidden = true;
}

function ui2WirePalette() {
  const backdrop = document.getElementById('ui2-palette-backdrop');
  const input = ui2PaletteInput();
  const searchBtn = document.getElementById('ui2-search-btn');
  if (!backdrop || !input) return;
  const isMac = /Mac|iP(hone|ad|od)/.test(navigator.platform);
  const kbd = document.getElementById('ui2-search-kbd');
  if (kbd) kbd.textContent = isMac ? '⌘K' : 'Ctrl K';
  const searchIcon = document.getElementById('ui2-search-icon');
  if (searchIcon) searchIcon.innerHTML = ui2Icon('search', 15);
  const paletteSearchIcon = document.getElementById('ui2-palette-search-icon');
  if (paletteSearchIcon) paletteSearchIcon.innerHTML = ui2Icon('search', 16);
  if (searchBtn) searchBtn.addEventListener('click', ui2PaletteOpen);
  backdrop.addEventListener('mousedown', (e) => { if (e.target === backdrop) ui2PaletteClose(); });
  input.addEventListener('input', () => { ui2Palette.selected = 0; ui2PaletteRender(input.value); });
  // Capture phase: while the palette is open it owns Esc/arrows/Enter and
  // nothing leaks into the v1 Escape cascade or composer handlers.
  document.addEventListener('keydown', (e) => {
    const combo = (e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'k';
    if (combo) {
      e.preventDefault();
      e.stopPropagation();
      ui2Palette.open ? ui2PaletteClose() : ui2PaletteOpen();
      return;
    }
    if (!ui2Palette.open) return;
    if (e.key === 'Escape') {
      e.preventDefault(); e.stopPropagation(); ui2PaletteClose();
    } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault(); e.stopPropagation();
      const n = ui2Palette.entries.length;
      if (n) {
        ui2Palette.selected = (ui2Palette.selected + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
        ui2PaletteRender(input.value);
      }
    } else if (e.key === 'Enter') {
      e.preventDefault(); e.stopPropagation();
      const item = ui2Palette.entries[ui2Palette.selected];
      if (item) ui2PaletteGo(item);
    }
  }, true);
}

// ── Fuel/lease chip (display-only) ─────────────────────────────────────
// When the daemon's status reports the built-in agent unfueled but the
// vault shows a live lease, the oversight bar gets a small "fueled ·
// <time-left>" chip next to the transport control — the lease IS the fuel
// (credential custody), and without this the chrome reads as broken while
// the agent works fine. All state is read by name at event time
// (dashboardControlTransport.lastStatus.fueled, vaultLeaseState.leases,
// vaultLeaseExpiryText) with typeof guards; hidden whenever any of it is
// missing.
function ui2FuelChipSync() {
  const bar = document.getElementById('ui2-oversight');
  if (!bar) return;
  let chip = document.getElementById('ui2-fuel-chip');
  const status = (typeof dashboardControlTransport !== 'undefined' && dashboardControlTransport)
    ? dashboardControlTransport.lastStatus : null;
  const leases = (typeof vaultLeaseState !== 'undefined' && vaultLeaseState
    && Array.isArray(vaultLeaseState.leases)) ? vaultLeaseState.leases : [];
  const live = leases.filter(l => Number(l?.expires_at_unix_ms || 0) > Date.now());
  const show = !!status && status.fueled === false && live.length > 0
    && typeof vaultLeaseExpiryText === 'function';
  if (!show) {
    if (chip) chip.hidden = true;
    return;
  }
  // The longest-lived lease is the effective fuel horizon.
  const best = live.reduce((a, b) =>
    Number(a.expires_at_unix_ms || 0) >= Number(b.expires_at_unix_ms || 0) ? a : b);
  if (!chip) {
    chip = document.createElement('span');
    chip.id = 'ui2-fuel-chip';
    chip.className = 'ui2-fuel-chip';
    const conn = document.getElementById('ui2-conn');
    if (conn && conn.parentElement === bar) bar.insertBefore(chip, conn);
    else bar.appendChild(chip);
  }
  chip.hidden = false;
  chip.textContent = `fueled · ${vaultLeaseExpiryText(best)}`;
  chip.title = 'The daemon holds no local provider key, but a vault lease is fueling the built-in agent'
    + (best.kind ? ` (${best.kind})` : '') + '. Display only.';
}

ui2BuildNav();
{
  // Single-boot: a module script executes at readyState 'interactive', so
  // the old immediate call + DOMContentLoaded listener both fired and wired
  // everything twice — the doubled capture-phase keydown made one ⌘K
  // open-then-close the palette and arrows double-step. Same idiom as the
  // ui2-activity boot.
  const wire = () => {
    ui2WireMirrors();
    ui2WirePalette();
    // Fuel chip: transport-status flips repaint it via the existing
    // #sb-dashboard-transport mirror lane; the interval keeps the lease
    // countdown honest between flips.
    ui2Mirror('sb-dashboard-transport', ui2FuelChipSync);
    setInterval(ui2FuelChipSync, 30000);
    ui2FuelChipSync();
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}
