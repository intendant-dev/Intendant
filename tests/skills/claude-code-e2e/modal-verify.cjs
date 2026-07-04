#!/usr/bin/env node
// Browser check for the New Session Claude launch options: the model-alias
// dropdown (fable/opus/sonnet/haiku + "Custom model id…" revealing the
// free-text input), the effort dropdown, and — end to end — that the chosen
// values reach the spawned CLI argv as
// `--model haiku --effort low --permission-mode acceptEdits`.
//
// Run: NODE_PATH=<repo>/node_modules node tests/skills/claude-code-e2e/modal-verify.cjs
// Needs a release build, playwright, and an authenticated `claude` CLI
// (haiku only — same policy as driver.cjs). Not for CI.
const { chromium } = require('playwright');
const { spawn, execSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const BINARY = path.join(__dirname, '..', '..', '..', 'target', 'release', 'intendant');
const WORKDIR = fs.mkdtempSync('/tmp/cc-modal-');
const PORT = 19040 + (process.pid % 50);

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
  // Idle daemon (no launch task): the modal spawns the session.
  const child = spawn(BINARY, [
    '--no-tui', '--bind', '127.0.0.1', '--no-tls', '--web', String(PORT),
  ], { cwd: WORKDIR, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stderr.on('data', (d) => process.stdout.write(`[child-err] ${d}`));
  try {
    const b = await chromium.launch();
    const p = await b.newPage({ viewport: { width: 1500, height: 950 } });
    // Connect as soon as the web port is up (retry loop, no fixed sleep).
    let connected = false;
    for (let i = 0; i < 40 && !connected; i++) {
      try {
        await p.goto(`http://127.0.0.1:${PORT}/app`, { waitUntil: 'domcontentloaded', timeout: 2000 });
        connected = true;
      } catch { await new Promise((r) => setTimeout(r, 500)); }
    }
    if (!connected) throw new Error('dashboard unreachable');
    await new Promise((r) => setTimeout(r, 1500));

    // Open Sessions → New Session.
    await p.evaluate(() => {
      [...document.querySelectorAll('.tab-btn')].find(t => t.dataset.tab === 'sessions')?.click();
    });
    await new Promise((r) => setTimeout(r, 700));
    await p.evaluate(() => {
      [...document.querySelectorAll('button, [data-subtab]')]
        .find(t => /new session/i.test((t.textContent || '').trim()))?.click();
    });
    await new Promise((r) => setTimeout(r, 700));

    // Pick claude-code; the model/effort dropdowns must enable.
    const state1 = await p.evaluate(() => {
      const agent = document.getElementById('new-session-agent');
      agent.value = 'claude-code';
      agent.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    });
    await new Promise((r) => setTimeout(r, 500));
    const fields = await p.evaluate(() => ({
      modelSel: !document.getElementById('new-session-claude-model-select')?.disabled,
      effortSel: !document.getElementById('new-session-claude-effort')?.disabled,
      customHidden: document.getElementById('new-session-claude-model-custom-row')?.classList.contains('hidden'),
      options: [...(document.getElementById('new-session-claude-model-select')?.options || [])].map(o => o.value),
    }));
    check('dropdowns-enabled-for-claude', state1 && fields.modelSel && fields.effortSel, JSON.stringify(fields.options));
    check('custom-row-hidden-by-default', fields.customHidden === true);

    // Custom choice reveals the id input; an alias choice hides it again.
    const customToggle = await p.evaluate(() => {
      const sel = document.getElementById('new-session-claude-model-select');
      sel.value = '__custom__';
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      const shown = !document.getElementById('new-session-claude-model-custom-row').classList.contains('hidden');
      sel.value = 'haiku';
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      const hiddenAgain = document.getElementById('new-session-claude-model-custom-row').classList.contains('hidden');
      return { shown, hiddenAgain };
    });
    check('custom-row-toggles', customToggle.shown && customToggle.hiddenAgain, JSON.stringify(customToggle));

    // Choose alias haiku + effort low + acceptEdits, start the session.
    await p.evaluate(() => {
      document.getElementById('new-session-claude-model-select').value = 'haiku';
      document.getElementById('new-session-claude-permission-mode').value = 'acceptEdits';
      document.getElementById('new-session-claude-effort').value = 'low';
      const input = document.getElementById('new-session-input');
      if (input) input.value = 'Reply with exactly: MODAL';
      document.getElementById('new-session-start-btn')?.click();
    });

    let argv = '';
    for (let i = 0; i < 30 && !argv; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        argv = execSync(
          "ps ax -o command | grep -E 'claude .*--effort low' | grep -v grep | head -1",
          { encoding: 'utf8' },
        ).trim();
      } catch { /* not yet */ }
    }
    check('argv-carries-alias-and-effort',
      argv.includes('--model haiku') && argv.includes('--effort low') && argv.includes('--permission-mode acceptEdits'),
      argv ? argv.slice(0, 160) : 'no matching claude process appeared');

    await b.close();
  } finally {
    // Await the child's exit so a rerun never races a dying listener on
    // the same port.
    const exited = new Promise((r) => child.once('exit', r));
    child.kill('SIGTERM');
    await Promise.race([exited, new Promise((r) => setTimeout(r, 8000))]);
  }
  const failed = checks.filter(c => !c.ok);
  console.log(`\n${checks.length - failed.length}/${checks.length} modal checks passed`);
  process.exit(failed.length ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
