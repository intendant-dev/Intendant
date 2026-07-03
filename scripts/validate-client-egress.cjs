#!/usr/bin/env node
'use strict';

// Client-egress E2E validation (credential custody, rollout step 5):
// a hosted Connect service, a claimed daemon with NO provider keys and
// ANTHROPIC_ENDPOINT pointed at a local mock, and a real browser session
// relaying provider calls with the key held only in its vault:
//
//   1. register + claim + operator grant; vault created; anthropic key
//      stored in the vault (never on the daemon)
//   2. setEgress(['api_key:anthropic']) with a host-allowlist override
//      for the mock -> relay appears in lease status (path indicator)
//   3. daemon /api/api-key-status stays anthropic:false — custody proof
//   4. egress probe RPC: daemon -> browser -> mock -> back; reply text
//      returned; mock saw x-api-key + the dangerous-direct-browser
//      header, which only the browser could have attached
//   5. host allowlist: a probe against a non-allowlisted host is refused
//      browser-side (flip the override away and probe again)
//   6. setEgress([]) -> relay gone; probe fails with 'no client-egress
//      relay'
//
// Usage: node scripts/validate-client-egress.cjs
//   [--connect-binary <path>] [--daemon-binary <path>]
//   [--connect-port <port>] [--daemon-port <port>] [--mock-port <port>]

const assert = require('assert');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_CONNECT_PORT = 9897;
const DEFAULT_DAEMON_PORT = 8917;
const DEFAULT_MOCK_PORT = 8919;
const DAEMON_ID = 'client-egress-daemon';
const START_TIMEOUT_MS = 45000;
const CONNECT_TIMEOUT_MS = 45000;
const STEP_TIMEOUT_MS = 20000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    connectBinary: path.join(repoRoot, 'target', 'debug', 'intendant-connect'),
    daemonBinary: path.join(repoRoot, 'target', 'debug', 'intendant'),
    connectPort: DEFAULT_CONNECT_PORT,
    daemonPort: DEFAULT_DAEMON_PORT,
    mockPort: DEFAULT_MOCK_PORT,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-port') out.connectPort = Number(argv[++i]);
    else if (arg === '--daemon-port') out.daemonPort = Number(argv[++i]);
    else if (arg === '--mock-port') out.mockPort = Number(argv[++i]);
    else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/validate-client-egress.cjs [--connect-binary <path>] [--daemon-binary <path>] [--connect-port <port>] [--daemon-port <port>] [--mock-port <port>]');
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return out;
}

async function httpStatus(url) {
  const resp = await fetch(url).catch(() => ({ status: 0 }));
  return resp.status || 0;
}

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (err) {
      lastError = err;
    }
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw new Error(`timed out waiting for ${label}${lastError ? `: ${lastError.message}` : ''}`);
}

async function addVirtualAuthenticator(browser, page) {
  if (browser.kind === 'playwright' && page.context) {
    const client = await page.context().newCDPSession(page);
    await client.send('WebAuthn.enable');
    await client.send('WebAuthn.addVirtualAuthenticator', {
      options: {
        protocol: 'ctap2',
        transport: 'internal',
        hasResidentKey: true,
        hasUserVerification: true,
        hasPrf: true,
        isUserVerified: true,
        automaticPresenceSimulation: true,
      },
    });
    return;
  }
  throw new Error('this validator needs the playwright driver for WebAuthn');
}

function slugComponent(value) {
  const slug = String(value || '')
    .trim()
    .replace(/[^a-zA-Z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .toLowerCase();
  return slug || 'unknown';
}

// Operator grant with the default role ceilings kept — the real hosted
// posture; operator holds credentials.manage.
function writeOperatorIamGrant(homeDir, user) {
  assert(user && user.id, `Connect user id missing: ${JSON.stringify(user)}`);
  const certDir = path.join(homeDir, '.intendant', 'access-certs');
  fs.mkdirSync(certDir, { recursive: true });
  const principalId = `principal:connect-account:${slugComponent(user.id)}`;
  const now = Date.now();
  const state = {
    schema_version: 1,
    principals: [{
      id: principalId,
      kind: 'connect_account',
      label: `@${user.account_name}`,
      status: 'active',
      source: 'local_iam_state',
      account: {
        provider: 'intendant.dev',
        user_id: user.id,
        account_name: user.account_name,
        handle: user.account_name,
      },
      organization: null,
      authn: [{
        kind: 'connect_account',
        label: 'Intendant Connect account',
        user_id: user.id,
        account_name: user.account_name,
      }],
      notes: 'Client-egress E2E operator grant',
      created_at_unix_ms: now,
    }],
    roles: [],
    grants: [{
      id: `grant:user-client:${slugComponent(principalId)}:local:role-operator`,
      principal_id: principalId,
      target_id: 'local',
      role_id: 'role:operator',
      policy_id: 'policy:operator',
      status: 'active',
      source: 'local_iam_state',
      reason: 'Client-egress E2E operator grant',
      created_at_unix_ms: now,
      revoked_at_unix_ms: null,
    }],
    audit_events: [],
  };
  fs.writeFileSync(path.join(certDir, 'iam.json'), `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
}

// Minimal Anthropic /v1/messages mock: JSON for stream:false, SSE for
// stream:true. Records every request's headers + body for assertions.
function startMockAnthropic(port, captured) {
  const server = http.createServer((req, res) => {
    // Real providers answer browser CORS (that is what the dangerous-
    // direct-browser-access opt-in is for); the mock must too, or the
    // relaying page's fetch is blocked before it ever leaves.
    const cors = {
      'access-control-allow-origin': '*',
      'access-control-allow-methods': 'POST, OPTIONS',
      'access-control-allow-headers': '*',
    };
    if (req.method === 'OPTIONS') {
      res.writeHead(204, cors);
      res.end();
      return;
    }
    const chunks = [];
    req.on('data', chunk => chunks.push(chunk));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      captured.push({ url: req.url, headers: { ...req.headers }, body });
      if (req.method !== 'POST' || !req.url.startsWith('/v1/messages')) {
        res.writeHead(404, { 'content-type': 'application/json', ...cors });
        res.end('{"error":"not found"}');
        return;
      }
      let stream = false;
      try {
        stream = JSON.parse(body).stream === true;
      } catch (_) {}
      if (!stream) {
        res.writeHead(200, { 'content-type': 'application/json', ...cors });
        res.end(JSON.stringify({
          content: [{ type: 'text', text: 'pong via client egress' }],
          usage: { input_tokens: 7, output_tokens: 4 },
        }));
        return;
      }
      res.writeHead(200, { 'content-type': 'text/event-stream', ...cors });
      const events = [
        { type: 'message_start', message: { usage: { input_tokens: 7 } } },
        { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
        { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'pong via client egress' } },
        { type: 'content_block_stop', index: 0 },
        { type: 'message_delta', delta: {}, usage: { output_tokens: 4 } },
        { type: 'message_stop' },
      ];
      for (const event of events) res.write(`data: ${JSON.stringify(event)}\n\n`);
      res.end();
    });
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => resolve(server));
  });
}

async function main() {
  const options = parseArgs(process.argv);
  for (const binary of [options.connectBinary, options.daemonBinary]) {
    if (!fs.existsSync(binary)) {
      throw new Error(`missing binary ${binary}; run cargo build --bin intendant --bin intendant-connect`);
    }
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-egress-'));
  const daemonHome = path.join(tmp, 'daemon-home');
  fs.mkdirSync(daemonHome, { recursive: true });
  const connectOrigin = `http://localhost:${options.connectPort}`;
  const connectApi = `http://127.0.0.1:${options.connectPort}`;
  const daemonOrigin = `http://127.0.0.1:${options.daemonPort}`;
  const mockHost = `127.0.0.1:${options.mockPort}`;
  const connectToken = 'client-egress-token';

  const logs = { connect: [], daemon: [] };
  const captured = [];
  const children = [];
  let browser = null;
  let mock = null;

  function spawnLogged(command, args, spawnOptions, sink) {
    const child = spawn(command, args, spawnOptions);
    children.push(child);
    child.stdout?.on('data', chunk => sink.push(String(chunk)));
    child.stderr?.on('data', chunk => sink.push(String(chunk)));
    child.once('error', err => sink.push(String((err && err.message) || err)));
    return child;
  }

  try {
    mock = await startMockAnthropic(options.mockPort, captured);

    spawnLogged(options.connectBinary, [
      '--listen', `127.0.0.1:${options.connectPort}`,
      '--origin', connectOrigin,
      '--rp-id', 'localhost',
      '--static-root', path.join(options.repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', connectToken,
    ], { cwd: options.repoRoot, stdio: ['ignore', 'pipe', 'pipe'] }, logs.connect);

    // The daemon must hold NO provider credentials — the whole point.
    const daemonEnv = { ...process.env };
    for (const name of [
      'OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY',
      'OPENAI', 'ANTHROPIC', 'GEMINI',
    ]) delete daemonEnv[name];
    spawnLogged(options.daemonBinary, [
      '--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(options.daemonPort),
    ], {
      cwd: tmp,
      env: {
        ...daemonEnv,
        HOME: daemonHome,
        ANTHROPIC_ENDPOINT: `http://${mockHost}`,
        INTENDANT_CONNECT_RENDEZVOUS_URL: connectApi,
        INTENDANT_CONNECT_DAEMON_ID: DAEMON_ID,
        INTENDANT_CONNECT_TOKEN: connectToken,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.daemon);

    await waitFor(() => httpStatus(`${connectApi}/healthz`).then(s => s === 200), START_TIMEOUT_MS, 'connect health');
    await waitFor(() => httpStatus(`${daemonOrigin}/config`).then(s => s === 200), START_TIMEOUT_MS, 'daemon readiness');

    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    const consoleMessages = [];
    page.on('console', msg => consoleMessages.push(msg.text()));
    await addVirtualAuthenticator(browser, page);

    // ── Register + claim + operator grant ──
    const claimCode = await waitFor(() => {
      const all = `${logs.connect.join('')}\n${logs.daemon.join('')}`;
      const urlMatch = all.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = all.match(/claim this daemon with code ([^\s"'<>]+)/);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim code');
    await page.goto(`${connectOrigin}/connect?claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => {
      document.getElementById('account').value = `egress-user-${Date.now()}`;
    });
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    await page.locator('#claim').click();
    await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('Rendezvous route claimed'), { timeout: START_TIMEOUT_MS });
    const me = await page.evaluate(async () => fetch('/api/me').then(r => r.json()));
    writeOperatorIamGrant(daemonHome, me.user || me);

    await page.goto(`${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(DAEMON_ID)}#access/advanced`, { timeout: START_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
    const bound = await waitFor(async () => {
      const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
      return status?.connected && status?.verifiedBinding?.ok && status?.grantKind === 'user-client' ? status : null;
    }, CONNECT_TIMEOUT_MS, 'verified operator session');
    assert.strictEqual(String(bound.accessPrincipal?.role_id || ''), 'role:operator');
    console.log('PASS egress-bind operator session over the verified tunnel');

    // ── Vault: create + store the anthropic key browser-side only ──
    const uiClick = async needle => {
      const clicked = await page.evaluate(text => {
        const buttons = Array.from(document.querySelectorAll('#access-vault-section button'));
        const button = buttons.find(b => b.textContent.trim().startsWith(text) && !b.disabled);
        if (!button) return false;
        button.click();
        return true;
      }, needle);
      assert(clicked, `vault button not found or disabled: ${needle}`);
    };
    const vaultState = () => page.evaluate(() => window.intendantVault?.state() || null);
    await waitFor(async () => (await vaultState())?.status === 'none', STEP_TIMEOUT_MS, 'vault ready to create');
    await uiClick('Create vault');
    await waitFor(() => page.evaluate(() =>
      document.querySelectorAll('#access-vault-section .vault-words .w').length === 12
    ), STEP_TIMEOUT_MS, 'phrase ceremony');
    await uiClick('I saved the phrase — create the vault');
    await waitFor(async () => (await vaultState())?.status === 'unlocked', STEP_TIMEOUT_MS, 'vault unlocked');

    const mockKey = 'sk-ant-egress-mock-key';
    const addOutcome = await page.evaluate(secret => {
      const section = document.getElementById('access-vault-section');
      const fold = Array.from(section.querySelectorAll('details summary'))
        .find(s => s.textContent.trim() === 'Add a credential');
      if (!fold) return 'no add fold';
      fold.parentElement.open = true;
      const selects = section.querySelectorAll('.vault-form-grid select');
      const inputs = section.querySelectorAll('.vault-form-grid input');
      if (selects.length < 2 || inputs.length < 2) return 'form fields missing';
      selects[0].value = 'api_key';
      selects[1].value = 'anthropic';
      inputs[0].value = 'Egress Anthropic';
      inputs[1].value = secret;
      const button = Array.from(section.querySelectorAll('button'))
        .find(b => b.textContent.trim() === 'Add to vault');
      if (!button) return 'no add button';
      button.click();
      return 'ok';
    }, mockKey);
    assert.strictEqual(addOutcome, 'ok', `UI add-entry failed: ${addOutcome}`);
    await waitFor(async () => (await vaultState())?.entries.some(e => e.provider === 'anthropic'), STEP_TIMEOUT_MS, 'key stored in the vault');
    console.log('PASS egress-vault key stored browser-side');

    // ── Enable relaying (with the mock-host allowlist override) ──
    await page.evaluate(host => window.intendantVault.setEgress(
      ['api_key:anthropic'],
      { allowHosts: { 'api_key:anthropic': host } }
    ), mockHost);
    const relayVisible = await waitFor(async () => {
      const egress = await page.evaluate(() => window.intendantVault.egress());
      return egress.relays.some(r => r.kind === 'api_key:anthropic') ? egress : null;
    }, STEP_TIMEOUT_MS, 'relay visible in lease status');
    assert(relayVisible.registered.includes('api_key:anthropic'), `not registered: ${JSON.stringify(relayVisible)}`);

    // Custody proof: the daemon still has no anthropic key of its own.
    const keys = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
    assert.strictEqual(keys.anthropic, false, `daemon must hold no anthropic key: ${JSON.stringify(keys)}`);
    console.log('PASS egress-register relay attached, daemon still keyless');

    // ── The probe: daemon -> browser -> mock -> back ──
    const probe = await page.evaluate(() => window.intendantVault.probeEgress('api_key:anthropic'));
    assert(String(probe?.text || '').includes('pong via client egress'), `unexpected probe reply: ${JSON.stringify(probe)}`);
    const seen = captured.find(r => String(r.url).startsWith('/v1/messages'));
    assert(seen, 'mock server saw no request');
    assert.strictEqual(seen.headers['x-api-key'], mockKey, 'mock did not receive the vault key');
    assert.strictEqual(seen.headers['anthropic-dangerous-direct-browser-access'], 'true',
      'browser-attached CORS opt-in header missing — did the request really go through the browser?');
    assert(seen.headers['anthropic-version'], 'daemon-built headers missing');
    console.log('PASS egress-probe relayed call round-tripped with browser-held key');

    // ── Host allowlist: flip the override away, probe must fail browser-side ──
    await page.evaluate(() => window.intendantVault.setEgress(
      ['api_key:anthropic'],
      { allowHosts: { 'api_key:anthropic': 'api.anthropic.com' } }
    ));
    let hostRefused = false;
    try {
      await page.evaluate(() => window.intendantVault.probeEgress('api_key:anthropic'));
    } catch (err) {
      hostRefused = /not allowed/.test(String(err?.message || err));
    }
    assert(hostRefused, 'relay must refuse a host outside the provider allowlist');
    console.log('PASS egress-allowlist non-provider host refused browser-side');

    // ── Disable -> relay gone -> clear error ──
    await page.evaluate(() => window.intendantVault.setEgress([]));
    await waitFor(async () => {
      const egress = await page.evaluate(() => window.intendantVault.egress());
      return egress.relays.length === 0;
    }, STEP_TIMEOUT_MS, 'relay unregistered');
    let noRelay = false;
    try {
      await page.evaluate(() => window.intendantVault.probeEgress('api_key:anthropic'));
    } catch (err) {
      noRelay = /no client-egress relay/.test(String(err?.message || err));
    }
    assert(noRelay, 'probe without a relay must say so');
    console.log('PASS egress-unregister relay withdrawn, probe fails clearly');

    console.log('PASS validate-client-egress all scenarios');
  } catch (err) {
    console.error(`FAIL validate-client-egress reason="${err.message}"`);
    console.error('--- daemon tail ---');
    console.error(logs.daemon.join('').split('\n').slice(-20).join('\n'));
    process.exitCode = 1;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) {
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
    }
    if (mock) mock.close();
    // The SIGTERM'd children may still be flushing logs; rmSync retries
    // ENOTEMPTY/EBUSY so teardown doesn't fail an otherwise-green run.
    fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 5, retryDelay: 250 });
  }
}

main().catch(err => {
  console.error(`FAIL validate-client-egress reason="${err.message}"`);
  process.exit(1);
});
