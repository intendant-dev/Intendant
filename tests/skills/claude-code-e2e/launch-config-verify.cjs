#!/usr/bin/env node
// Browser check for the per-session Launch-config modal on a live Claude
// Code window: the four claude rows appear (codex rows hidden), prefill
// reflects the session's launch pins, Save persists new pins to the
// external overlay AND live-applies the model via the `model` thread
// action ("model switched to …" in the activity log).
//
// Run: NODE_PATH=<repo>/node_modules node tests/skills/claude-code-e2e/launch-config-verify.cjs
// Haiku only; needs a release build + playwright + authenticated claude.
const { chromium } = require('playwright');
const { spawn, execSync } = require('child_process');
const path = require('path');
const fs = require('fs');
const os = require('os');

const BINARY = path.join(__dirname, '..', '..', '..', 'target', 'release', 'intendant');
const WORKDIR = fs.mkdtempSync('/tmp/cc-launchcfg-');
const PORT = 19140 + (process.pid % 50);

fs.writeFileSync(path.join(WORKDIR, 'intendant.toml'), [
  '[agent]', 'default_backend = "claude-code"', '',
  '[agent.claude_code]',
  '# E2E policy: haiku only.',
  'model = "claude-haiku-4-5-20251001"',
  'permission_mode = "default"', '',
].join('\n'));
execSync('git init -q && git add -A && git -c user.email=e@l -c user.name=e commit -qm seed', { cwd: WORKDIR, shell: '/bin/zsh' });

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};

(async () => {
  const child = spawn(BINARY, [
    '--agent', 'claude-code', '--no-tui', '--bind', '127.0.0.1', '--no-tls',
    // A tool-using task under permission_mode=default parks on an approval
    // prompt, keeping the session window live for the whole check (a no-tool
    // reply finishes in seconds and can outrun the page connect).
    '--web', String(PORT), 'Use a shell command to print hello. Then reply DONE.',
  ], { cwd: WORKDIR, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stderr.on('data', (d) => process.stdout.write(`[child-err] ${d}`));
  try {
    const b = await chromium.launch();
    const p = await b.newPage({ viewport: { width: 1500, height: 950 } });
    let connected = false;
    for (let i = 0; i < 40 && !connected; i++) {
      try {
        await p.goto(`http://127.0.0.1:${PORT}/app`, { waitUntil: 'domcontentloaded', timeout: 2000 });
        connected = true;
      } catch { await new Promise((r) => setTimeout(r, 500)); }
    }
    if (!connected) throw new Error('dashboard unreachable');

    // Wait for the live window with advertised ops (capabilities landed).
    let sid = '';
    for (let i = 0; i < 60 && !sid; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      sid = await p.evaluate(() => {
        const win = document.querySelector('.session-window');
        const compact = win?.querySelector('[data-session-window-action="compact"]');
        if (!win || !compact || compact.classList.contains('hidden')) return '';
        return win.dataset.sessionId || '';
      });
    }
    check('claude-window-live', Boolean(sid), sid.slice(0, 12));
    if (!sid) throw new Error('no live claude window');

    // Load the sessions list once: the modal prefills launch pins from the
    // cached list rows (wrapper log-dir config → row → merge), same as the
    // codex rows.
    await p.evaluate(() => {
      [...document.querySelectorAll('.tab-btn')].find(t => t.dataset.tab === 'sessions')?.click();
    });
    await new Promise((r) => setTimeout(r, 2500));
    await p.evaluate(() => {
      [...document.querySelectorAll('.tab-btn')].find(t => t.dataset.tab === 'activity')?.click();
    });
    await new Promise((r) => setTimeout(r, 700));

    // Open the Launch config modal from the kebab.
    const modal = await p.evaluate(() => {
      const win = document.querySelector('.session-window');
      const kebab = [...win.querySelectorAll('button')].find(x => (x.getAttribute('aria-label') || x.title || '') === 'Session actions');
      kebab?.click();
      win.querySelector('[data-session-window-action="configure-launch"]')?.click();
      const visible = (id) => {
        const el = document.getElementById(id);
        return Boolean(el && el.closest('.session-config-row, .modal-backdrop') && el.offsetParent !== null);
      };
      return {
        open: document.getElementById('session-config-modal')?.style.display !== 'none',
        modelRow: visible('session-config-claude-model'),
        permissionRow: visible('session-config-claude-permission-mode'),
        toolsRow: visible('session-config-claude-allowed-tools'),
        effortRow: visible('session-config-claude-effort'),
        codexSandboxHidden: document.getElementById('session-config-sandbox-row')?.style.display === 'none',
        prefill: {
          model: document.getElementById('session-config-claude-model')?.value,
          custom: document.getElementById('session-config-claude-model-custom')?.value,
          mode: document.getElementById('session-config-claude-permission-mode')?.value,
          effort: document.getElementById('session-config-claude-effort')?.value,
        },
      };
    });
    check('modal-shows-claude-rows',
      modal.open && modal.modelRow && modal.permissionRow && modal.toolsRow && modal.effortRow && modal.codexSandboxHidden,
      JSON.stringify(modal.prefill));
    // A fresh session has no per-session overrides: every field reads
    // "inherit" (launch-time pins live in the spawn-reproduction layer,
    // not the modal's overlay layer — same as codex).
    check('fresh-session-prefills-inherit',
      modal.prefill.model === 'inherit' && modal.prefill.mode === 'inherit'
        && modal.prefill.effort === 'inherit',
      JSON.stringify(modal.prefill));

    // Pin model=sonnet + effort=high, Save (no restart) → persisted pins +
    // live model switch. (Stay on haiku-family pricing? The live switch is
    // config-only until the next turn, and no further turns run — cheap.)
    await p.evaluate(() => {
      document.getElementById('session-config-claude-model').value = 'sonnet';
      document.getElementById('session-config-claude-model').dispatchEvent(new Event('change', { bubbles: true }));
      document.getElementById('session-config-claude-effort').value = 'high';
      window.saveSessionConfigModal();
    });
    // Ground truth for the save: the overlay store gains the pins under
    // this window's session id (status text/modal may close on success).
    const storePath = path.join(os.homedir(), '.intendant', 'session_agent_config.json');
    let savedEntry = null;
    for (let i = 0; i < 20 && !savedEntry; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        const store = JSON.parse(fs.readFileSync(storePath, 'utf8'));
        const entry = (store['claude-code'] || {})[sid];
        if (entry && entry.claude_model === 'sonnet' && entry.claude_effort === 'high') {
          savedEntry = entry;
        }
      } catch { /* store may be mid-write */ }
    }
    check('save-persists-overlay-pins', Boolean(savedEntry),
      savedEntry ? JSON.stringify(savedEntry).slice(0, 140) : `no sonnet+high entry for ${sid.slice(0, 12)}`);

    let liveApplied = false;
    for (let i = 0; i < 20 && !liveApplied; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      liveApplied = await p.evaluate(() =>
        [...document.querySelectorAll('.log-entry, [class*="log"]')]
          .some(el => /model switched to sonnet/i.test(el.textContent || '')));
    }
    check('model-live-applied', liveApplied);

    // Server truth: the sessions row for this sid must now carry the pins
    // (overlay application).
    const serverRow = await p.evaluate(async (sid2) => {
      const body = await (await fetch('/api/sessions')).json();
      const rows = Array.isArray(body) ? body : (body.sessions || []);
      const row = rows.find((r) => [r.session_id, r.id, r.backend_session_id]
        .map((v) => String(v || '')).includes(sid2));
      return row ? {
        claude_model: row.claude_model, claude_effort: row.claude_effort,
        session_id: row.session_id, source: row.source,
      } : null;
    }, sid);
    check('server-row-carries-pins',
      serverRow && serverRow.claude_model === 'sonnet' && serverRow.claude_effort === 'high',
      JSON.stringify(serverRow));

    // Reopen the modal (poll — the client list cache refreshes on its own
    // cycle): the saved pins must prefill.
    let reopened = null;
    for (let i = 0; i < 8 && !(reopened && reopened.model === 'sonnet'); i++) {
      await p.evaluate(() => window.closeSessionConfigModal?.());
      await p.evaluate(() => {
        [...document.querySelectorAll('.tab-btn')].find(t => t.dataset.tab === 'sessions')?.click();
      });
      await new Promise((r) => setTimeout(r, 3000));
      reopened = await p.evaluate(() => {
        [...document.querySelectorAll('.tab-btn')].find(t => t.dataset.tab === 'activity')?.click();
        const win = document.querySelector('.session-window');
        const kebab = [...win.querySelectorAll('button')].find(x => (x.getAttribute('aria-label') || x.title || '') === 'Session actions');
        kebab?.click();
        win.querySelector('[data-session-window-action="configure-launch"]')?.click();
        return {
          model: document.getElementById('session-config-claude-model')?.value,
          effort: document.getElementById('session-config-claude-effort')?.value,
        };
      });
    }
    check('reopen-prefills-saved-pins',
      reopened && reopened.model === 'sonnet' && reopened.effort === 'high',
      JSON.stringify(reopened));

    await b.close();
  } finally {
    const exited = new Promise((r) => child.once('exit', r));
    child.kill('SIGTERM');
    await Promise.race([exited, new Promise((r) => setTimeout(r, 8000))]);
  }
  const failed = checks.filter(c => !c.ok);
  console.log(`\n${checks.length - failed.length}/${checks.length} launch-config checks passed`);
  process.exit(failed.length ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
