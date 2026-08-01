#!/usr/bin/env node
// Credential-watch rig: scratch auth file + stub `claude` CLI under a
// fast poll; prove an out-of-band account switch is detected within one
// poll interval and delivered as the one info notification (the era
// mint and reload surface ride the same fire — pinned by unit tests).
const { spawn } = require('child_process');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const BINARY = process.argv[2];
const POLL_MS = 400;
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'credwatch-home-'));
const PROJ = fs.mkdtempSync(path.join(os.tmpdir(), 'credwatch-proj-'));

// The scratch auth artifact at the default resolution ($HOME/.claude/
// .credentials.json — HOME is the temp dir, so the real box is never
// touched). Content is fake; the watch must never read it anyway.
const CLAUDE_DIR = path.join(HOME, '.claude');
fs.mkdirSync(CLAUDE_DIR, { recursive: true });
const AUTH_FILE = path.join(CLAUDE_DIR, '.credentials.json');
fs.writeFileSync(AUTH_FILE, JSON.stringify({ fake: 'account-a-material' }));

// The stub CLI: `claude auth status` answers with whatever identity the
// state file names (absolute path baked in — the probe's env policy
// strips everything but the base allowlist, so no env plumbing).
const IDENTITY_FILE = path.join(HOME, 'identity.json');
const identity = (email) =>
  JSON.stringify({ loggedIn: true, authMethod: 'claudeai', email, subscriptionType: 'max' });
fs.writeFileSync(IDENTITY_FILE, identity('rig-a@example.com'));
const STUB = path.join(HOME, 'claude-stub');
fs.writeFileSync(
  STUB,
  `#!/bin/sh\nif [ "$1" = "auth" ] && [ "$2" = "status" ]; then /bin/cat "${IDENTITY_FILE}"; fi\nexit 0\n`
);
fs.chmodSync(STUB, 0o755);

// Point the project's claude command at the stub.
fs.writeFileSync(
  path.join(PROJ, 'intendant.toml'),
  `[agent.claude_code]\ncommand = "${STUB}"\n`
);

const script = { profiles: [{ steps: [
  { content: 'ok', tool_calls: [{ name: 'signal_done', arguments: { message: 'credwatch rig task done' } }] },
]}]};
const scriptPath = path.join(HOME, 'mock_script.json');
fs.writeFileSync(scriptPath, JSON.stringify(script));

const env = {
  ...process.env, HOME, USERPROFILE: HOME,
  PROVIDER: 'mock', INTENDANT_MOCK_SCRIPT: scriptPath,
  INTENDANT_CREDENTIAL_POLL_MS: String(POLL_MS),
};
for (const k of ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'MODEL_NAME',
  'CLAUDE_CONFIG_DIR', 'CODEX_HOME']) delete env[k];

const child = spawn(BINARY, ['--no-tui', '--web', '0', '--bind', '127.0.0.1', '--no-tls',
  '--control-socket', '--autonomy', 'full', 'trivial task'], { cwd: PROJ, env, stdio: ['ignore', 'pipe', 'pipe'] });
let exited = false;
child.on('exit', () => { exited = true; });
child.stderr.on('data', () => {});
child.stdout.on('data', () => {});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const socketPath = `/tmp/intendant-${child.pid}.sock`;
  const deadline = Date.now() + 30000;
  let sock = null;
  while (Date.now() < deadline && !sock) {
    if (exited) throw new Error('daemon exited early');
    if (fs.existsSync(socketPath)) {
      try {
        sock = await new Promise((resolve, reject) => {
          const s = net.createConnection(socketPath);
          s.on('connect', () => resolve(s));
          s.on('error', reject);
        });
      } catch { /* retry */ }
    }
    if (!sock) await sleep(250);
  }
  if (!sock) throw new Error('no control socket');

  // Let the watch's baseline tick land (first interval tick is
  // immediate; the stub answers in milliseconds): the watch adopts
  // rig-a as its identity baseline.
  await sleep(Math.max(1000, POLL_MS * 3));

  // THE OUT-OF-BAND CHANGE: flip the stub's identity and rewrite the
  // scratch auth file — exactly what a direct `claude` re-login does.
  fs.writeFileSync(IDENTITY_FILE, identity('rig-b@example.com'));
  fs.writeFileSync(AUTH_FILE, JSON.stringify({ fake: 'account-b-material-longer' }));
  const changedAt = Date.now();

  let buf = '';
  const notification = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timeout waiting for the credential-change notification')), 15000);
    sock.on('data', (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        if (!line.includes('user_notification')) continue;
        let e; try { e = JSON.parse(line); } catch { continue; }
        if (e.event !== 'user_notification') continue;
        if (e.id !== 'credential-change-claude-code') continue;
        clearTimeout(timer);
        resolve(e);
      }
    });
  });
  const elapsed = Date.now() - changedAt;
  // Detection rides the first tick after the change: within one poll
  // interval, plus probe/delivery slack.
  const withinInterval = elapsed <= POLL_MS + 1600;
  const text = String(notification.text || '');
  const textOk = text.includes('changed outside Intendant')
    && text.includes('now signed in as rig-b@example.com')
    && text.includes('(was rig-a@example.com)')
    && text.includes('reload');
  const urgencyOk = String(notification.urgency || '') === 'info';
  const noSecrets = !text.includes('account-b-material') && !text.includes('account-a-material');
  console.log(`notification after ${elapsed}ms (poll ${POLL_MS}ms): ${text}`);
  if (!withinInterval) console.log(`detection exceeded one poll interval (+slack): ${elapsed}ms`);
  if (!textOk) console.log('notification copy mismatch');
  if (!urgencyOk) console.log(`urgency mismatch: ${notification.urgency}`);
  if (!noSecrets) console.log('SECRET MATERIAL LEAKED INTO THE NOTIFICATION');
  const ok = withinInterval && textOk && urgencyOk && noSecrets;
  console.log(ok ? 'CREDENTIAL WATCH RIG PASS' : 'CREDENTIAL WATCH RIG FAIL');
  child.kill('SIGTERM');
  process.exit(ok ? 0 : 1);
})().catch((err) => { console.error('RIG ERROR: ' + err.message); child.kill('SIGTERM'); process.exit(1); });
