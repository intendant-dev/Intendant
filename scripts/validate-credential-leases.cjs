#!/usr/bin/env node
'use strict';

// Credential-lease E2E validation: a real hosted Connect service, a real
// claimed daemon (spawned with NO provider keys in its environment), and
// a real browser session fueling it over the verified tunnel:
//
//   1. register + claim; hosted dashboard binds under the OPERATOR role
//      (seeded grant, default role ceilings kept — the real hosted posture;
//      operator holds credentials.manage)
//   2. lease status starts empty; /api/api-key-status all false
//   3. grant an anthropic lease -> key status flips true
//   4. renew extends the expiry; status reports the lease + usage fields
//   5. unknown-kind grant is refused server-side
//   6. revoke -> status empty, key status back to false
//
// Usage: node scripts/validate-credential-leases.cjs
//   [--connect-binary <path>] [--daemon-binary <path>]
//   [--connect-port <port>] [--daemon-port <port>]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_CONNECT_PORT = 9895;
const DEFAULT_DAEMON_PORT = 8897;
const DEFAULT_DAEMON_ID = 'credential-lease-daemon';
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
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-port') out.connectPort = Number(argv[++i]);
    else if (arg === '--daemon-port') out.daemonPort = Number(argv[++i]);
    else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/validate-credential-leases.cjs [--connect-binary <path>] [--daemon-binary <path>] [--connect-port <port>] [--daemon-port <port>]');
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

// Seed the daemon's local IAM with an OPERATOR grant for the Connect
// account — unlike the hosted-MVP validator this keeps the default role
// ceilings (connect_account -> operator), so the tunnel binds exactly the
// posture a real hosted session gets. Operator holds credentials.manage.
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
      notes: 'Credential-lease E2E operator grant',
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
      reason: 'Credential-lease E2E operator grant',
      created_at_unix_ms: now,
      revoked_at_unix_ms: null,
    }],
    audit_events: [],
  };
  fs.writeFileSync(path.join(certDir, 'iam.json'), `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
}

async function main() {
  const options = parseArgs(process.argv);
  for (const binary of [options.connectBinary, options.daemonBinary]) {
    if (!fs.existsSync(binary)) {
      throw new Error(`missing binary ${binary}; run cargo build --bin intendant --bin intendant-connect`);
    }
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-leases-'));
  const daemonHome = path.join(tmp, 'daemon-home');
  fs.mkdirSync(daemonHome, { recursive: true });
  const connectOrigin = `http://localhost:${options.connectPort}`;
  const connectApi = `http://127.0.0.1:${options.connectPort}`;
  const daemonOrigin = `http://127.0.0.1:${options.daemonPort}`;
  const connectToken = 'credential-lease-token';

  const logs = { connect: [], daemon: [] };
  const children = [];
  let browser = null;

  function spawnLogged(command, args, spawnOptions, sink) {
    const child = spawn(command, args, spawnOptions);
    children.push(child);
    child.stdout?.on('data', chunk => sink.push(String(chunk)));
    child.stderr?.on('data', chunk => sink.push(String(chunk)));
    child.once('error', err => sink.push(String((err && err.message) || err)));
    return child;
  }

  try {
    spawnLogged(options.connectBinary, [
      '--listen', `127.0.0.1:${options.connectPort}`,
      '--origin', connectOrigin,
      '--rp-id', 'localhost',
      '--static-root', path.join(options.repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', connectToken,
    ], { cwd: options.repoRoot, stdio: ['ignore', 'pipe', 'pipe'] }, logs.connect);

    // The daemon must start UNFUELED: strip provider keys so the only key
    // it can ever hold arrives as a lease over the tunnel.
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
        INTENDANT_CONNECT_RENDEZVOUS_URL: connectApi,
        INTENDANT_CONNECT_DAEMON_ID: DEFAULT_DAEMON_ID,
        INTENDANT_CONNECT_TOKEN: connectToken,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.daemon);

    await waitFor(() => httpStatus(`${connectApi}/healthz`).then(s => s === 200), START_TIMEOUT_MS, 'connect health');
    await waitFor(() => httpStatus(`${daemonOrigin}/config`).then(s => s === 200), START_TIMEOUT_MS, 'daemon readiness');

    // Unfueled baseline straight from the daemon origin.
    const keysBefore = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
    assert.strictEqual(keysBefore.anthropic, false, `daemon should start without an anthropic key: ${JSON.stringify(keysBefore)}`);

    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    const consoleMessages = [];
    page.on('console', msg => consoleMessages.push(msg.text()));
    await addVirtualAuthenticator(browser, page);

    // ── Register + claim ──
    const claimCode = await waitFor(() => {
      const all = `${logs.connect.join('')}\n${logs.daemon.join('')}`;
      const urlMatch = all.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = all.match(/claim this daemon with code ([^\s"'<>]+)/);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim code');
    await page.goto(`${connectOrigin}/connect?claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => {
      document.getElementById('account').value = `lease-user-${Date.now()}`;
    });
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    await page.locator('#claim').click();
    await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('Rendezvous route claimed'), { timeout: START_TIMEOUT_MS });
    const me = await page.evaluate(async () => fetch('/api/me').then(r => r.json()));
    writeOperatorIamGrant(daemonHome, me.user || me);
    console.log('PASS lease-claim account registered, daemon claimed, operator grant seeded');

    // ── Bind the hosted dashboard session ──
    await page.goto(`${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(DEFAULT_DAEMON_ID)}#access/advanced`, { timeout: START_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
    const bound = await waitFor(async () => {
      const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
      return status?.connected && status?.verifiedBinding?.ok && status?.grantKind ? status : null;
    }, CONNECT_TIMEOUT_MS, 'verified hosted dashboard connection');
    assert.strictEqual(bound.grantKind, 'user-client', `expected the scoped operator binding: ${JSON.stringify({ grantKind: bound.grantKind, role: bound.accessPrincipal?.role_id })}`);
    assert.strictEqual(String(bound.accessPrincipal?.role_id || ''), 'role:operator', `expected operator role: ${JSON.stringify(bound.accessPrincipal)}`);
    console.log('PASS lease-bind operator session over the verified tunnel');

    const rpc = (method, params = {}) => page.evaluate(
      ([m, p]) => window.intendantDashboardControl.request(m, p, { timeoutMs: 15000 }),
      [method, params]
    );

    // ── Empty status, then grant ──
    const statusEmpty = await rpc('api_credential_lease_status');
    assert(Array.isArray(statusEmpty.leases) && statusEmpty.leases.length === 0, `expected no leases: ${JSON.stringify(statusEmpty)}`);

    const granted = await rpc('api_credential_lease_grant', {
      kind: 'api_key:anthropic',
      label: 'E2E Anthropic',
      material: 'sk-ant-e2e-lease-material',
      ttl_ms: 120000,
      offline_ms: 0,
    });
    assert(granted.lease_id && granted.lease_id.startsWith('lease_'), `grant returned no lease id: ${JSON.stringify(granted)}`);
    assert.strictEqual(granted.replaced, false);

    const keysAfterGrant = await waitFor(async () => {
      const keys = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
      return keys.anthropic === true ? keys : null;
    }, STEP_TIMEOUT_MS, 'anthropic key visible after lease grant');
    assert.strictEqual(keysAfterGrant.openai, false, 'lease must fuel only its own kind');
    console.log(`PASS lease-grant daemon fueled (lease ${granted.lease_id.slice(0, 14)}…)`);

    // ── Renew + status ──
    const renewed = await rpc('api_credential_lease_renew', { lease_id: granted.lease_id });
    assert(renewed.expires_at_unix_ms >= granted.expires_at_unix_ms, `renew did not extend expiry: ${JSON.stringify({ granted, renewed })}`);
    const statusActive = await rpc('api_credential_lease_status');
    assert.strictEqual(statusActive.leases.length, 1, `expected one lease: ${JSON.stringify(statusActive)}`);
    const lease = statusActive.leases[0];
    assert.strictEqual(lease.kind, 'api_key:anthropic');
    assert.strictEqual(lease.label, 'E2E Anthropic');
    assert(lease.granted_by, 'lease status must record who granted it');
    assert(!JSON.stringify(statusActive).includes('sk-ant-e2e-lease-material'), 'lease status must never carry the material');
    console.log(`PASS lease-renew-status expiry extended, granted_by="${lease.granted_by}"`);

    // ── Unknown kinds are refused server-side ──
    let unknownRefused = false;
    try {
      await rpc('api_credential_lease_grant', { kind: 'api_key:mystery', material: 'x' });
    } catch (err) {
      unknownRefused = /unknown credential kind/.test(String(err && err.message || err));
    }
    assert(unknownRefused, 'unknown credential kind must be refused');
    console.log('PASS lease-validation unknown kind refused over the tunnel');

    // ── Revoke ──
    const revoked = await rpc('api_credential_lease_revoke', { lease_id: granted.lease_id });
    assert.strictEqual(revoked.revoked, 1, `expected one revocation: ${JSON.stringify(revoked)}`);
    const statusAfterRevoke = await rpc('api_credential_lease_status');
    assert.strictEqual(statusAfterRevoke.leases.length, 0, 'revoked lease still listed');
    const keysAfterRevoke = await waitFor(async () => {
      const keys = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
      return keys.anthropic === false ? keys : null;
    }, STEP_TIMEOUT_MS, 'anthropic key gone after revocation');
    assert(keysAfterRevoke, 'key status did not flip back after revoke');
    console.log('PASS lease-revoke material dropped, daemon unfueled again');

    console.log('PASS validate-credential-leases all scenarios');
  } catch (err) {
    console.error(`FAIL validate-credential-leases reason="${err.message}"`);
    console.error('--- connect tail ---');
    console.error(logs.connect.join('').split('\n').slice(-12).join('\n'));
    console.error('--- daemon tail ---');
    console.error(logs.daemon.join('').split('\n').slice(-20).join('\n'));
    process.exitCode = 1;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) {
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
    }
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch(err => {
  console.error(`FAIL validate-credential-leases reason="${err.message}"`);
  process.exit(1);
});
