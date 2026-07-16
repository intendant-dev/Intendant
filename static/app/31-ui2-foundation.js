// ── ui-v2 foundation: flag helpers + inline-SVG icon registry ──────────
// Design-overhaul program. The ui-v2 class itself is decided pre-paint in
// 00-head.html; this module gives the rest of the app a single flag API
// and the icon set the v2 chrome/surfaces render with. Loaded in both UI
// generations; inert under v1 (nothing calls it until v2 code does).
//
// Icon conventions (from the design handoff): 24×24 viewBox, fill:none,
// stroke:currentColor, stroke-width 1.7, round caps/joins. Entries are the
// inner SVG markup; a few glyphs carry their own fill/stroke overrides
// (filled play triangle, record dot). No emoji anywhere.

const UI2_ICON_PATHS = {
  // — destinations (redesigned in the design-system pass, 2026-07-08:
  //   drawn from the product's own motifs — timeline grammar, lineage
  //   fan, broadcast, radar, prompt+cursor, tree, heatmap, keyhole,
  //   safe wheel; settings/debug kept) —
  activity: '<circle cx="5" cy="6.2" r="1.3" fill="currentColor" stroke="none"/><circle cx="5" cy="12" r="1.3" fill="currentColor" stroke="none"/><circle cx="5" cy="17.8" r="1.3" fill="currentColor" stroke="none"/><path d="M9 6.2h7.5"/><path d="M9 12h11"/><path d="M9 17.8h5.5"/>',
  sessions: '<circle cx="12" cy="5.4" r="2.1"/><path d="M10.9 7.2 6.6 14.6"/><path d="M12 7.5v7.9"/><path d="M13.1 7.2l4.3 7.4"/><circle cx="5.8" cy="16.9" r="1.5" fill="currentColor" stroke="none"/><circle cx="12" cy="17.6" r="1.5" fill="currentColor" stroke="none"/><circle cx="18.2" cy="16.9" r="1.5" fill="currentColor" stroke="none"/>',
  live: '<rect x="3" y="4" width="18" height="13" rx="2"/><path d="M9 21h6"/><circle cx="12" cy="10.5" r="1.4" fill="currentColor" stroke="none"/><path d="M9.2 13.3a4.4 4.4 0 0 1 0-5.6"/><path d="M14.8 7.7a4.4 4.4 0 0 1 0 5.6"/>',
  station: '<circle cx="12" cy="12" r="8.4"/><path d="M12 12l5.4-6.2"/><circle cx="12" cy="12" r="1.1" fill="currentColor" stroke="none"/><circle cx="8.3" cy="14.9" r="1.3" fill="currentColor" stroke="none"/><circle cx="15.5" cy="15.2" r="1.3" fill="currentColor" stroke="none"/>',
  terminal: '<path d="M4.5 6.5 10 12l-5.5 5.5"/><rect x="13" y="15.7" width="6.5" height="2.6" rx="1.1" fill="currentColor" stroke="none"/>',
  files: '<rect x="3.5" y="3.5" width="8" height="4.8" rx="1.4"/><path d="M6.5 8.3v10h6"/><path d="M6.5 12.4h6"/><rect x="14" y="10" width="6.5" height="4.8" rx="1.4"/><rect x="14" y="16" width="6.5" height="4.8" rx="1.4"/>',
  stats: '<rect x="3.5" y="3.5" width="4.6" height="4.6" rx="1"/><rect x="9.7" y="3.5" width="4.6" height="4.6" rx="1"/><rect x="15.9" y="3.5" width="4.6" height="4.6" rx="1"/><rect x="3.5" y="9.7" width="4.6" height="4.6" rx="1"/><rect x="9.7" y="9.7" width="4.6" height="4.6" rx="1" opacity=".55" fill="currentColor" stroke="none"/><rect x="15.9" y="9.7" width="4.6" height="4.6" rx="1" opacity=".55" fill="currentColor" stroke="none"/><rect x="3.5" y="15.9" width="4.6" height="4.6" rx="1"/><rect x="9.7" y="15.9" width="4.6" height="4.6" rx="1" opacity=".55" fill="currentColor" stroke="none"/><rect x="15.9" y="15.9" width="4.6" height="4.6" rx="1" opacity=".55" fill="currentColor" stroke="none"/>',
  agenda: '<path d="M4.2 6.2l1.6 1.6 2.8-3"/><path d="M12 6.5h8"/><path d="M4.2 12.2l1.6 1.6 2.8-3"/><path d="M12 12.5h8"/><circle cx="6" cy="18.2" r="1.7"/><path d="M12 18.5h8"/>',
  access: '<path d="M12 3l7 3v5c0 4.5-3 7.6-7 9-4-1.4-7-4.5-7-9V6z"/><circle cx="12" cy="10.2" r="1.9"/><path d="M12 12.1v3"/>',
  vault: '<rect x="3.5" y="3.5" width="17" height="17" rx="4.2"/><circle cx="12" cy="12" r="3.9"/><path d="M12 5.9v2.2"/><path d="M12 15.9v2.2"/><path d="M5.9 12h2.2"/><path d="M15.9 12h2.2"/>',
  settings: '<path d="M4 8h8"/><path d="M16 8h4"/><path d="M4 16h4"/><path d="M12 16h8"/><circle cx="14" cy="8" r="2"/><circle cx="8" cy="16" r="2"/>',
  debug: '<rect x="8" y="9" width="8" height="10" rx="4"/><path d="M12 5v4"/><path d="M8 12.5H4"/><path d="M8 16H4.5"/><path d="M20 12.5h-4"/><path d="M20 16h-3.5"/><path d="M9 9 6.8 6.4"/><path d="M15 9l2.2-2.6"/>',

  // — product concepts (logo motifs + fleet vocabulary; same pass) —
  daemon: '<rect x="4" y="5" width="16" height="6.2" rx="1.8"/><rect x="4" y="12.8" width="16" height="6.2" rx="1.8"/><circle cx="7.4" cy="8.1" r="1" fill="currentColor" stroke="none"/><circle cx="7.4" cy="15.9" r="1" fill="currentColor" stroke="none"/><path d="M13 8.1h4"/><path d="M13 15.9h4"/>',
  peers: '<circle cx="6.7" cy="6.7" r="2.7"/><circle cx="17.3" cy="17.3" r="2.7"/><path d="M8.7 8.7l6.6 6.6"/>',
  lease: '<circle cx="7.2" cy="16.8" r="3.4"/><path d="M9.7 14.3 19.5 4.5"/><path d="M14.8 9.2l2.8 2.8"/><path d="M17.7 6.3l2.3 2.3"/>',
  baton: '<path d="M5 19 17.8 6.2"/><circle cx="18.6" cy="5.4" r="1.8" fill="currentColor" stroke="none"/><circle cx="4.6" cy="19.4" r="1.1" fill="currentColor" stroke="none"/>',
  org: '<path d="M4.5 20.5v-9Q4.5 4 12 4t7.5 7.5v9"/><path d="M2.5 20.5h19"/>',
  phone: '<path d="M7 3.8c.9 0 2.8 3.2 2.3 4.1-.4.8-1.6 1.2-1.6 1.2s.5 2 2.1 3.6c1.6 1.6 3.6 2.1 3.6 2.1s.4-1.2 1.2-1.6c.9-.5 4.1 1.4 4.1 2.3 0 1.6-2 3.2-3.5 3.2-2.5 0-5.5-1.4-7.8-3.7C5.1 12.7 3.7 9.7 3.7 7.2c0-1.5 1.7-3.4 3.3-3.4z"/>',

  // — actions & affordances —
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2.2"/><path d="M12 19.3v2.2"/><path d="M2.5 12h2.2"/><path d="M19.3 12h2.2"/><path d="M5.3 5.3l1.6 1.6"/><path d="M17.1 17.1l1.6 1.6"/><path d="M18.7 5.3l-1.6 1.6"/><path d="M6.9 17.1l-1.6 1.6"/>',
  moon: '<path d="M20.2 13.2A8.2 8.2 0 0 1 10.8 3.8a8.2 8.2 0 1 0 9.4 9.4Z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.6-3.6"/>',
  send: '<path d="M12 20V5"/><path d="M6 11l6-6 6 6"/>',
  plus: '<path d="M12 5v14"/><path d="M5 12h14"/>',
  attach: '<path d="M18.4 8.6l-7.7 7.7a3 3 0 0 1-4.3-4.3l7.8-7.7a2 2 0 0 1 2.9 2.9l-7.8 7.7"/>',
  mic: '<rect x="9" y="3" width="6" height="11" rx="3"/><path d="M6 11a6 6 0 0 0 12 0"/><path d="M12 17v3"/>',
  chev: '<path d="M6 9l6 6 6-6"/>',
  doc: '<path d="M6 3.5h7l4 4V20a.5.5 0 0 1-.5.5H6.5A.5.5 0 0 1 6 20z"/><path d="M13 3.5V8h4"/>',
  right: '<path d="M9 6l6 6-6 6"/>',
  check: '<path d="M20 6L9 17l-5-5"/>',
  dial: '<circle cx="9" cy="8" r="2.6"/><circle cx="15" cy="16" r="2.6"/><path d="M4 8h2.4"/><path d="M11.6 8H20"/><path d="M4 16h8.4"/><path d="M17.6 16H20"/>',
  branch: '<circle cx="6" cy="6" r="2.4"/><circle cx="6" cy="18" r="2.4"/><circle cx="18" cy="8" r="2.4"/><path d="M6 8.4v7.2"/><path d="M18 10.4c0 4.2-6 1.8-6 5.6"/>',

  // — production glyphs the prototype lacked (drawn to the same grid) —
  close: '<path d="M6 6l12 12"/><path d="M18 6L6 18"/>',
  back: '<path d="M15 6l-6 6 6 6"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/>',
  eye: '<path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z"/><circle cx="12" cy="12" r="2.8"/>',
  external: '<path d="M14 4h6v6"/><path d="M20 4l-9 9"/><path d="M19 13v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h6"/>',
  trash: '<path d="M4 7h16"/><path d="M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/><path d="M6.5 7l1 13h9l1-13"/><path d="M10 11v6"/><path d="M14 11v6"/>',
  warn: '<path d="M12 4 2.8 19.5h18.4Z"/><path d="M12 10v4.2"/><path d="M12 17.3v.2"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5"/><path d="M12 8v.2"/>',
  record: '<circle cx="12" cy="12" r="6" fill="currentColor" stroke="none"/>',
  play: '<path d="M8 5.5l11 6.5-11 6.5Z" fill="currentColor" stroke="none"/>',
  pause: '<path d="M8 5.5v13"/><path d="M16 5.5v13"/>',
  stop: '<rect x="7" y="7" width="10" height="10" rx="2" fill="currentColor" stroke="none"/>',
  'folder-open': '<path d="M4 8V6.5A1.5 1.5 0 0 1 5.5 5H9l2 2h7.5A1.5 1.5 0 0 1 20 8.5V9"/><path d="M2.8 19h14.9a1 1 0 0 0 .95-.7L21 12H6.2a1 1 0 0 0-.95.7Z"/>',
};

// Truthful short tags per autonomy level (shared by the nav chip and the
// Settings dial). Sourced from intendant-core autonomy.rs::needs_approval:
// Low runs only file reads; Medium gates Ask-ruled writes/deletes/
// destructive/network; High auto-approves every Ask rule (only Deny and
// the hard gates stop it — NOT "gate network", the prototype's tag was
// wrong); Full bypasses the rules entirely.
const UI2_AUTONOMY_TAGS = {
  Low: 'reads only',
  Medium: 'gate writes',
  High: 'auto unless denied',
  Full: 'ungated',
};

// The v1 chrome and its runtime flag are gone; the ui-v2 chrome is the
// only generation. Kept as a constant on the QA facade so harness probes
// written against the flag era keep working.
function ui2Enabled() {
  return true;
}

// Theme (design-system import): dark is the default; light remaps the
// same token names under data-theme="light". Live flip — pure CSS vars,
// no reload; browser-scoped ("daemons don't care what you wear").
function ui2Theme() {
  return document.documentElement.getAttribute('data-theme') === 'light' ? 'light' : 'dark';
}
function ui2SetTheme(theme) {
  const light = theme === 'light';
  if (light) document.documentElement.setAttribute('data-theme', 'light');
  else document.documentElement.removeAttribute('data-theme');
  try { localStorage.setItem('intendant.ui2.theme', light ? 'light' : 'dark'); } catch (e) { /* private mode */ }
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', light ? '#F5F6FB' : '#0B0C10');
}

function ui2Icon(name, size = 18) {
  const inner = UI2_ICON_PATHS[name];
  if (!inner) {
    console.warn('[ui2] unknown icon:', name);
    return '';
  }
  return `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" style="flex-shrink:0;display:block">${inner}</svg>`;
}

// QA/debug facade (mirrors the window.qa / stationProbe convention).
window.__ui2 = {
  enabled: ui2Enabled, icon: ui2Icon,
  theme: ui2Theme, setTheme: ui2SetTheme,
};
