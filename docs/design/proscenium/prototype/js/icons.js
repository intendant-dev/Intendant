/* Proscenium — icon registry. 24×24 stroke grid, 1.7 width, round caps.
   Usage: P.icon('name') → svg string; P.mountIcons(root) fills [data-icon]. */
window.P = window.P || {};

P.ICONS = {
  arch:      '<path d="M4.5 20 V11 Q4.5 4 12 4 Q19.5 4 19.5 11 V20"/><path d="M8 20 V12 Q8 7.5 12 7.5 Q16 7.5 16 12 V20" opacity=".45"/>',
  stage:     '<path d="M3 17 Q12 13 21 17"/><path d="M5 17 V9 Q5 5 12 5 Q19 5 19 9 V17" opacity=".9"/><circle cx="12" cy="11" r="1.6"/>',
  screens:   '<rect x="3" y="4.5" width="18" height="12.5" rx="2"/><path d="M9 20.5 h6 M12 17 v3.5"/>',
  files:     '<path d="M3.5 7 Q3.5 5.5 5 5.5 h4 l2 2.5 h8 Q20.5 8 20.5 9.5 V17 Q20.5 18.5 19 18.5 H5 Q3.5 18.5 3.5 17 Z"/>',
  machines:  '<rect x="3" y="4" width="8" height="7" rx="1.5"/><rect x="13" y="13" width="8" height="7" rx="1.5"/><path d="M7 11 v4 Q7 16.5 8.5 16.5 H13" opacity=".7"/>',
  key:       '<circle cx="8" cy="12" r="4"/><path d="M12 12 h9 M17.5 12 v3.5 M20.5 12 v2.5"/>',
  books:     '<path d="M5 4.5 Q5 3.5 6 3.5 H19 V17 H6.5 Q5 17 5 18.5 Q5 20 6.5 20 H19"/><path d="M5 17.5 V4.5" opacity=".6"/>',
  settings:  '<path d="M4 7 h8 M17 7 h3 M4 16.5 h3 M12 16.5 h8"/><circle cx="14.5" cy="7" r="2.2"/><circle cx="9.5" cy="16.5" r="2.2"/>',
  station:   '<circle cx="12" cy="12" r="2.2"/><circle cx="12" cy="12" r="6" opacity=".55"/><circle cx="12" cy="12" r="9.5" opacity=".3"/><circle cx="18.5" cy="7.5" r="1.3"/><circle cx="5.5" cy="15.5" r="1.3"/>',
  studio:    '<path d="M9 3.5 h6 M10 3.5 V9 L4.8 18 Q4 20 6.5 20 h11 Q20 20 19.2 18 L14 9 V3.5"/><path d="M7.5 14.5 h9" opacity=".6"/>',
  search:    '<circle cx="11" cy="11" r="6.5"/><path d="M15.8 15.8 L20.5 20.5"/>',
  mic:       '<rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11 Q5.5 16.5 12 16.5 Q18.5 16.5 18.5 11 M12 16.5 V20"/>',
  send:      '<path d="M4.5 12 L20 4.5 L13.5 20 L11 13.5 Z"/><path d="M11 13.5 L20 4.5" opacity=".6"/>',
  attach:    '<path d="M16 11.5 L9.8 17.7 Q8 19.5 6 17.7 Q4.2 15.9 6 14.1 L13.4 6.7 Q14.9 5.2 16.6 6.7 Q18.3 8.2 16.8 9.7 L9.5 17" />',
  doorbell:  '<path d="M6 16 V11 Q6 6 12 6 Q18 6 18 11 V16"/><path d="M4.5 16 h15 M10 19 Q12 20.5 14 19"/>',
  sparkle:   '<path d="M12 4 L13.8 10.2 L20 12 L13.8 13.8 L12 20 L10.2 13.8 L4 12 L10.2 10.2 Z"/>',
  density:   '<path d="M4 6 h16 M4 12 h16 M4 18 h10" /><circle cx="18" cy="18" r="2" opacity=".7"/>',
  theme:     '<path d="M12 3.5 A8.5 8.5 0 1 0 20.5 12 A7 7 0 0 1 12 3.5 Z"/>',
  chev:      '<path d="M9 5.5 L15.5 12 L9 18.5"/>',
  chevdown:  '<path d="M5.5 9 L12 15.5 L18.5 9"/>',
  plus:      '<path d="M12 5 V19 M5 12 H19"/>',
  check:     '<path d="M4.5 12.5 L10 18 L19.5 6.5"/>',
  x:         '<path d="M6 6 L18 18 M18 6 L6 18"/>',
  skip:      '<path d="M6 5 L13 12 L6 19 M13 5 L20 12 L13 19" opacity=".9"/>',
  clock:     '<circle cx="12" cy="12" r="8.5"/><path d="M12 7 V12 L15.5 14"/>',
  fuel:      '<path d="M12 3.5 Q17 9.5 17 13.5 A5 5 0 0 1 7 13.5 Q7 9.5 12 3.5 Z"/>',
  shield:    '<path d="M12 3.5 L19.5 6.5 V11 Q19.5 17.5 12 20.5 Q4.5 17.5 4.5 11 V6.5 Z"/>',
  terminal:  '<rect x="3" y="4.5" width="18" height="15" rx="2"/><path d="M7 9.5 L10.5 12.5 L7 15.5 M12.5 15.5 H17"/>',
  camera:    '<rect x="3" y="7" width="13" height="11" rx="2"/><path d="M16 11 L21 8 V17 L16 14 Z"/>',
  record:    '<circle cx="12" cy="12" r="8.5" opacity=".6"/><circle cx="12" cy="12" r="3.5"/>',
  branch:    '<circle cx="6.5" cy="6" r="2.5"/><circle cx="6.5" cy="18" r="2.5"/><circle cx="17.5" cy="8" r="2.5"/><path d="M6.5 8.5 V15.5 M17.5 10.5 Q17.5 15 12 15.5 L9 15.7" opacity=".8"/>',
  folder:    '<path d="M3.5 7 Q3.5 5.5 5 5.5 h4 l2 2.5 h8 Q20.5 8 20.5 9.5 V17 Q20.5 18.5 19 18.5 H5 Q3.5 18.5 3.5 17 Z"/>',
  file:      '<path d="M6 3.5 H14 L18.5 8 V20.5 H6 Z"/><path d="M13.5 3.5 V8.5 H18.5" opacity=".6"/>',
  external:  '<path d="M10 5.5 H5.5 V18.5 H18.5 V14"/><path d="M13.5 4.5 H19.5 V10.5 M19.5 4.5 L11 13" opacity=".8"/>',
  pause:     '<path d="M8.5 5.5 V18.5 M15.5 5.5 V18.5"/>',
  stop:      '<rect x="6.5" y="6.5" width="11" height="11" rx="1.5"/>',
  play:      '<path d="M8 5.5 L18.5 12 L8 18.5 Z"/>',
  refresh:   '<path d="M19.5 12 A7.5 7.5 0 1 1 17.2 6.3 M17.5 3.5 V7 H14" />',
  info:      '<circle cx="12" cy="12" r="8.5" opacity=".7"/><path d="M12 11 V16.5 M12 7.5 V8"/>',
  warn:      '<path d="M12 4 L21 19.5 H3 Z"/><path d="M12 10 V14.5 M12 16.8 V17.3"/>',
  question:  '<circle cx="12" cy="12" r="8.5" opacity=".7"/><path d="M9.5 9.5 Q9.5 7.5 12 7.5 Q14.5 7.5 14.5 9.5 Q14.5 11 12.5 11.8 Q12 12.2 12 13.5 M12 16.3 V16.8"/>',
  trash:     '<path d="M5 7 H19 M9.5 7 V5 Q9.5 4 10.5 4 h3 Q14.5 4 14.5 5 V7 M7 7 L7.8 19 Q7.9 20 9 20 h6 Q16.1 20 16.2 19 L17 7"/>',
  eye:       '<path d="M3 12 Q7.5 5.5 12 5.5 Q16.5 5.5 21 12 Q16.5 18.5 12 18.5 Q7.5 18.5 3 12 Z"/><circle cx="12" cy="12" r="2.8"/>',
  hand:      '<path d="M8 11.5 V5.5 Q8 4 9.3 4 Q10.6 4 10.6 5.5 V10 M10.6 10 V3.8 Q10.6 2.6 11.8 2.6 Q13 2.6 13 3.8 V10 M13 10 V5 Q13 3.8 14.2 3.8 Q15.4 3.8 15.4 5 V11 M15.4 11.5 L17 9.8 Q18 8.8 19 9.8 Q20 10.8 19 11.8 L14.5 17 Q12.5 20 9.5 19.5 Q7 19 5.8 16.5 L4.5 13.5 Q4 12 5.2 11.3 Q6.4 10.6 7.3 12 L8 13.5"/>',
  layers:    '<path d="M12 3.5 L21 8.5 L12 13.5 L3 8.5 Z"/><path d="M4.5 12.5 L12 16.5 L19.5 12.5 M4.5 16.5 L12 20.5 L19.5 16.5" opacity=".55"/>',
  history:   '<path d="M4 12 A8 8 0 1 1 6 6.5 M4 3.5 V7 H7.5" /><path d="M12 8 V12 L15 14" opacity=".8"/>',
  download:  '<path d="M12 4 V14.5 M7.5 10.5 L12 15 L16.5 10.5 M4.5 18.5 H19.5"/>',
  upload:    '<path d="M12 15 V4.5 M7.5 9 L12 4.5 L16.5 9 M4.5 18.5 H19.5"/>',
};

P.icon = function (name, size) {
  const d = P.ICONS[name] || P.ICONS.info;
  const s = size || 18;
  return '<svg viewBox="0 0 24 24" width="' + s + '" height="' + s + '" fill="none" stroke="currentColor"' +
    ' stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + d + '</svg>';
};

P.mountIcons = function (root) {
  (root || document).querySelectorAll('[data-icon]').forEach(function (el) {
    if (!el.dataset.mounted) { el.innerHTML = P.icon(el.dataset.icon); el.dataset.mounted = '1'; }
  });
};
