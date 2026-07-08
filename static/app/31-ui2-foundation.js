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
  // — destinations —
  activity: '<path d="M3 12h3l2.4 7 5-16 3 9h4.6"/>',
  sessions: '<path d="M12 3 3 8l9 5 9-5-9-5Z"/><path d="M3 13l9 5 9-5"/>',
  live: '<rect x="3" y="4" width="18" height="13" rx="2"/><path d="M9 21h6"/><path d="M11 8.6l4.2 2.9-4.2 2.9Z" fill="currentColor" stroke="none"/>',
  station: '<circle cx="12" cy="12" r="2.4"/><ellipse cx="12" cy="12" rx="9" ry="3.6"/><ellipse cx="12" cy="12" rx="9" ry="3.6" transform="rotate(62 12 12)"/><ellipse cx="12" cy="12" rx="9" ry="3.6" transform="rotate(-62 12 12)"/>',
  terminal: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9.5l3 2.5-3 2.5"/><path d="M13 15h4"/>',
  files: '<path d="M4 7a2 2 0 0 1 2-2h3l2 2h7a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/>',
  stats: '<path d="M5 20V11"/><path d="M12 20V4"/><path d="M19 20v-6"/>',
  access: '<path d="M12 3l7 3v5c0 4.5-3 7.6-7 9-4-1.4-7-4.5-7-9V6z"/><path d="M9 12l2 2 4-4"/>',
  vault: '<circle cx="8.5" cy="12" r="3.6"/><path d="M12.1 12H21"/><path d="M18 12v3.4"/><path d="M15 12v2.4"/>',
  settings: '<path d="M4 8h8"/><path d="M16 8h4"/><path d="M4 16h4"/><path d="M12 16h8"/><circle cx="14" cy="8" r="2"/><circle cx="8" cy="16" r="2"/>',
  debug: '<rect x="8" y="9" width="8" height="10" rx="4"/><path d="M12 5v4"/><path d="M8 12.5H4"/><path d="M8 16H4.5"/><path d="M20 12.5h-4"/><path d="M20 16h-3.5"/><path d="M9 9 6.8 6.4"/><path d="M15 9l2.2-2.6"/>',

  // — actions & affordances —
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

function ui2Enabled() {
  return document.documentElement.classList.contains('ui-v2');
}

// Persist + reload: the two chromes are alternate DOM, not alternate CSS
// only, so a live toggle re-boots the app. Callers: the palette/settings
// escape hatches and the QA harness.
function ui2SetEnabled(on) {
  try { localStorage.setItem('intendant.ui2', on ? '1' : '0'); } catch (e) { /* private mode */ }
  const url = new URL(location.href);
  url.searchParams.delete('ui');
  location.replace(url.toString());
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
window.__ui2 = { enabled: ui2Enabled, setEnabled: ui2SetEnabled, icon: ui2Icon };
