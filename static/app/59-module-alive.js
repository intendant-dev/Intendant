// ── Module-death canary (sentinel) ──────────────────────────────────────
// MUST stay the LAST .js fragment in manifest.txt (only 59-module-close.html
// after it), and must stay dependency-free: reaching these two lines proves
// the entire dashboard module evaluated without an uncaught top-level throw.
// static/app/30-module-canary.js (the first fragment) checks for the flag
// ~3s after module start and paints a fatal [module-death] banner when it is
// missing; scripts/validate-dashboard.cjs asserts it on every validation.
// Appending fragments after this one silently weakens the canary — don't.
window.__intendantModuleAlive = true;
document.documentElement.dataset.intendantModuleAlive = '1';
