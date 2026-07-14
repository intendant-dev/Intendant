#!/usr/bin/env node
'use strict';

// Credential-lease hosted-boundary validation. Connect-origin vault storage
// does not imply delivery: the default build has no trusted native/direct
// bridge that can carry a lease from this hosted origin to the daemon.
//
// It asserts route-only claiming leaves IAM empty, hosted offers are refused,
// an adversarial operator grant + forged ceiling cannot change that outcome,
// no control DataChannel opens, and the daemon remains unfueled. Full lease
// lifecycle coverage belongs to a trusted direct/native bridge validator.
//
// Usage: node scripts/validate-credential-leases.cjs
//   [--connect-binary <path>] [--daemon-binary <path>]
//   [--connect-port <port>] [--daemon-port <port>]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_CONNECT_PORT = 9895;
const DEFAULT_DAEMON_PORT = 8897;
const MOCK_OAUTH_PORT = 8921;
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

async function hostedControlStatuses(page, daemonId) {
  return page.evaluate(async id => {
    const me = await fetch('/api/me').then(response => response.json());
    const headers = {
      'content-type': 'application/json',
      'x-intendant-csrf': me.csrf_token,
    };
    const call = async (path, body) => {
      const response = await fetch(path, {
        method: 'POST',
        headers,
        body: JSON.stringify(body),
      });
      return { status: response.status, text: await response.text() };
    };
    return {
      offer: await call('/api/browser/offer', { daemon_id: id, sdp: 'retired-hosted-offer' }),
      ice: await call('/api/browser/ice', {
        daemon_id: id,
        session_id: 'trusted-direct-session',
        candidate: { candidate: 'candidate:retired-hosted' },
      }),
      close: await call('/api/browser/close', {
        daemon_id: id,
        session_id: 'trusted-direct-session',
      }),
    };
  }, daemonId);
}

function assertHostedControlRefused(statuses, label) {
  for (const [kind, result] of Object.entries(statuses)) {
    assert.strictEqual(result.status, 403, `${label}: ${kind} returned ${result.status}: ${result.text}`);
    assert(/hosted daemon control is unavailable/i.test(result.text), `${label}: ${kind} refusal was unclear: ${result.text}`);
  }
}

function slugComponent(value) {
  const slug = String(value || '')
    .trim()
    .replace(/[^a-zA-Z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .toLowerCase();
  return slug || 'unknown';
}

// Adversarial fixture: a real operator grant plus a hand-edited persisted
// ceiling must not turn hosted provenance into a lease-delivery channel.
function writeAdversarialOperatorGrant(homeDir, fingerprint, accountName) {
  assert(fingerprint, 'hosted browser key fingerprint is required');
  const certDir = path.join(homeDir, '.intendant', 'access-certs');
  fs.mkdirSync(certDir, { recursive: true });
  const principalId = `principal:client-key:${slugComponent(fingerprint)}`;
  const now = Date.now();
  const state = {
    schema_version: 2,
    principals: [{
      id: principalId,
      kind: 'client_key',
      label: accountName ? `@${accountName} browser` : 'Hosted browser',
      status: 'active',
      source: 'local_iam_state',
      account: accountName ? { provider: 'intendant.dev', account_name: accountName, handle: accountName } : null,
      organization: null,
      authn: [{
        kind: 'client_key',
        label: 'Browser identity key',
        fingerprint,
        origin: 'hosted-connect-e2e',
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
    role_ceilings: {
      connect_account: 'role:none',
      client_key: 'role:operator',
    },
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

  const retiredOwner = spawnSync(options.daemonBinary, [
    '--owner', 'E2E-Owner-Key-Fingerprint-000000000000000AA', '--no-web',
  ], { encoding: 'utf8', timeout: 5000 });
  const retiredOwnerOutput = `${retiredOwner.stdout || ''}\n${retiredOwner.stderr || ''}`;
  assert.notStrictEqual(retiredOwner.status, 0, 'legacy --owner unexpectedly succeeded');
  assert(
    /unknown|unsupported|retired|unrecognized|unexpected argument/i.test(retiredOwnerOutput),
    `legacy --owner must fail as a parser-level refusal, got: ${retiredOwnerOutput}`
  );
  console.log('PASS lease-owner-bootstrap legacy --owner is rejected');

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
  let mockOauthServer = null;

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

    // ── Register + route-only claim ──
    const claimCode = await waitFor(() => {
      const all = `${logs.connect.join('')}\n${logs.daemon.join('')}`;
      const urlMatch = all.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = all.match(/one-time claim code ([a-z0-9-]+)/i);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim code');
    await page.goto(`${connectOrigin}/connect#claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => {
      document.getElementById('account').value = `lease-user-${Date.now()}`;
    });
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    await page.locator('#claim').click();
    await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('No machine access was granted'), { timeout: START_TIMEOUT_MS });
    const iamPath = path.join(daemonHome, '.intendant', 'access-certs', 'iam.json');
    if (fs.existsSync(iamPath)) {
      const afterClaim = JSON.parse(fs.readFileSync(iamPath, 'utf8'));
      assert.strictEqual((afterClaim.principals || []).length, 0, 'route claim unexpectedly created an IAM principal');
      assert.strictEqual((afterClaim.grants || []).length, 0, 'route claim unexpectedly created an IAM grant');
    }
    console.log('PASS lease-claim account registered, daemon route linked, IAM unchanged');

    // ── Service-side refusal before and after an adversarial grant ──
    const ungranted = await hostedControlStatuses(page, DEFAULT_DAEMON_ID);
    assertHostedControlRefused(ungranted, 'before adversarial grant');
    writeAdversarialOperatorGrant(
      daemonHome,
      'E2E-Hosted-Key-Fingerprint-0000000000000000AA',
      'lease browser'
    );
    const stillRefused = await hostedControlStatuses(page, DEFAULT_DAEMON_ID);
    assertHostedControlRefused(stillRefused, 'after adversarial operator grant');

    await page.goto(`${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(DEFAULT_DAEMON_ID)}`, {
      waitUntil: 'domcontentloaded',
      timeout: START_TIMEOUT_MS,
    });
    assert.strictEqual(new URL(page.url()).pathname, '/connect', 'retired /app route did not redirect to /connect');
    assert.strictEqual(
      await page.evaluate(() => typeof window.intendantDashboardControl),
      'undefined',
      'Connect directory unexpectedly loaded the dashboard control client'
    );
    assert(!logs.daemon.join('').includes('[dashboard/control] data channel open:'), 'hosted route unexpectedly opened a control data channel');
    const keysAfterRefusal = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
    assert.strictEqual(keysAfterRefusal.anthropic, false, 'refused hosted path unexpectedly fueled the daemon');
    console.log(JSON.stringify({
      ok: true,
      hosted_delivery_available: false,
      control_statuses: stillRefused,
      data_channel_open: false,
      daemon_anthropic_key: keysAfterRefusal.anthropic,
    }, null, 2));

  } catch (err) {
    console.error(`FAIL validate-credential-leases reason="${err.message}"`);
    console.error('--- connect tail ---');
    console.error(logs.connect.join('').split('\n').slice(-12).join('\n'));
    console.error('--- daemon tail ---');
    console.error(logs.daemon.join('').split('\n').slice(-20).join('\n'));
    process.exitCode = 1;
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (mockOauthServer) mockOauthServer.close();
    for (const child of children) {
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
    }
    // The SIGTERM'd children may still be flushing logs; rmSync retries
    // ENOTEMPTY/EBUSY so teardown doesn't fail an otherwise-green run.
    fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 5, retryDelay: 250 });
  }
}

main().catch(err => {
  console.error(`FAIL validate-credential-leases reason="${err.message}"`);
  process.exit(1);
});
