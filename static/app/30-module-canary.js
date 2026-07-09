// ── Module-death canary (arm) ───────────────────────────────────────────
// The whole dashboard is ONE <script type="module"> — every .js fragment in
// manifest order. An uncaught top-level throw during evaluation (e.g. the
// 2026-07-09 cross-fragment TDZ ReferenceError) kills every fragment after
// the throwing line: listeners and window.* exposures silently never
// initialize while the page still half-renders. This fragment arms a check
// ~3s after module start: if static/app/59-module-alive.js (the LAST
// fragment) never ran, paint an unmissable banner and tag the console with
// [module-death].
//
// Placement rules (manifest.txt): this must stay the FIRST .js fragment so
// nothing can throw before the check is armed, and 59-module-alive.js must
// stay the last so reaching it proves the whole module evaluated. The
// build-time complement is the assembler's eval-order lint
// (crates/app-html-assembler/src/eval_order.rs), which statically rejects
// direct top-level references to later-fragment let/const/class bindings.
//
// Known limit: a SyntaxError anywhere in the module aborts the PARSE, so
// this fragment never runs either (no banner) — but then no script runs at
// all, which is visibly broken; the canary targets the sneaky partial-death
// class. Everything here is dependency-free plain DOM wrapped in try/catch:
// the canary must never be what kills the module.
(() => {
  try {
    let firstError = '';
    window.addEventListener('error', (e) => {
      if (firstError || window.__intendantModuleAlive === true) return;
      try {
        const where = e && e.filename ? ' @ ' + e.filename + ':' + e.lineno : '';
        firstError = String((e && e.error && e.error.message) || (e && e.message) || 'unknown error') + where;
      } catch (_) {
        firstError = 'unknown error';
      }
    });
    setTimeout(() => {
      try {
        if (window.__intendantModuleAlive === true) return;
        document.documentElement.dataset.intendantModuleDead = '1';
        console.error(
          '[module-death] dashboard module never finished evaluating: the assembled ' +
          '<script type="module"> threw at top level' +
          (firstError ? ' (first uncaught error: ' + firstError + ')' : '') +
          ' — every fragment after the throwing line (listeners, window.* exposures) is dead. ' +
          'static/app/manifest.txt order is program order; see docs in static/app/30-module-canary.js.'
        );
        const banner = document.createElement('div');
        banner.id = 'intendant-module-death-banner';
        banner.setAttribute('role', 'alert');
        banner.style.cssText =
          'position:fixed;top:10px;left:50%;transform:translateX(-50%);' +
          'z-index:2147483647;max-width:min(760px,calc(100vw - 32px));' +
          'background:#7f1d1d;color:#fff;border:1px solid #fca5a5;border-radius:10px;' +
          'padding:10px 14px;font:600 13px/1.45 system-ui,-apple-system,sans-serif;' +
          'box-shadow:0 6px 24px rgba(0,0,0,.45);display:flex;align-items:center;gap:12px;';
        const text = document.createElement('span');
        text.textContent =
          'Dashboard failed to initialize — reload; see console ([module-death]).' +
          (firstError ? ' First error: ' + firstError : '');
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.textContent = 'Reload';
        btn.style.cssText =
          'flex:none;background:#fff;color:#7f1d1d;border:0;border-radius:7px;' +
          'padding:6px 12px;font:600 12px system-ui,-apple-system,sans-serif;cursor:pointer;';
        btn.addEventListener('click', () => location.reload());
        banner.append(text, btn);
        (document.body || document.documentElement).appendChild(banner);
      } catch (err) {
        try { console.error('[module-death] canary failed to render the banner:', err); } catch (_) { /* nothing left to try */ }
      }
    }, 3000);
  } catch (_) {
    // Never let the canary itself take the module down.
  }
})();
