#!/usr/bin/env node
'use strict';

// Client-egress hosted-boundary validation. The default hosted Connect build
// has no trusted browser-to-daemon bridge, so it cannot deliver a browser-held
// credential. This test pins the honest current behavior:
//
//   1. register + route-only claim leaves daemon IAM unchanged;
//   2. persist an adversarial operator grant and forged client-key ceiling;
//   3. hosted offer/ICE/close remain hard 403s and `/app` redirects;
//   4. no hosted DataChannel or control global is constructed;
//   5. the daemon remains keyless and the provider mock sees no request.
//
// This validator intentionally does not claim a successful relay scenario.
// Direct/native bridge delivery gets its own validator when that bridge exists.
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
const { assertHostedControlUnavailable } = require('./lib/connect-hosted-refusal.cjs');

const DEFAULT_CONNECT_PORT = 9897;
const DEFAULT_DAEMON_PORT = 8917;
const DEFAULT_MOCK_PORT = 8919;
const DAEMON_ID = 'client-egress-daemon';
const START_TIMEOUT_MS = 45000;

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
      console.log(`Usage:
  node scripts/validate-client-egress.cjs [options]

Hosted-refusal scope:
  Proves an adversarial persisted operator grant cannot create a hosted
  client-egress channel. It does not exercise a successful credential relay.

Options:
  --connect-binary <path>  intendant-connect binary
  --daemon-binary <path>   intendant daemon binary
  --connect-port <port>    Connect port (default ${DEFAULT_CONNECT_PORT})
  --daemon-port <port>     daemon web port (default ${DEFAULT_DAEMON_PORT})
  --mock-port <port>       provider-capture port (default ${DEFAULT_MOCK_PORT})`);
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

// Adversarial fixture: even a real operator grant and hand-edited persisted
// ceiling must not turn hosted provenance into a control path.
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
    role_ceilings: {
      connect_account: 'role:none',
      client_key: 'role:operator',
    },
  };
  fs.writeFileSync(path.join(certDir, 'iam.json'), `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
}

// Provider tripwire: record every request, including CORS preflights. No
// request is expected in this refusal-only validator, so a response body that
// emulates a successful historical relay would only hide mistakes.
function startProviderTripwire(port, captured) {
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', chunk => chunks.push(chunk));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      captured.push({ url: req.url, headers: { ...req.headers }, body });
      res.writeHead(500, {
        'content-type': 'application/json',
        'access-control-allow-origin': '*',
        'access-control-allow-methods': 'POST, OPTIONS',
        'access-control-allow-headers': '*',
      });
      res.end('{"error":"unexpected provider request in hosted-refusal validator"}');
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
    mock = await startProviderTripwire(options.mockPort, captured);

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
    const accountName = `egress-user-${Date.now()}`;
    await page.evaluate(name => {
      document.getElementById('account').value = name;
    }, accountName);
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

    // Exercise the boundary against active, deliberately hostile persisted
    // state. A successful result on empty IAM would not prove that a local
    // operator grant and forged client-key ceiling still cannot resurrect the
    // retired hosted relay.
    const iamSnapshot = fs.existsSync(iamPath) ? fs.readFileSync(iamPath) : null;
    let retiredAttempts;
    try {
      writeAdversarialOperatorGrant(
        daemonHome,
        'cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66ee7788',
        accountName
      );
      const adversarialIam = JSON.parse(fs.readFileSync(iamPath, 'utf8'));
      assert(
        adversarialIam.grants?.some(grant => grant.status === 'active' && grant.role_id === 'role:operator'),
        'adversarial operator grant was not persisted before the refusal probe'
      );
      assert.strictEqual(
        adversarialIam.role_ceilings?.client_key,
        'role:operator',
        'adversarial client-key ceiling was not persisted before the refusal probe'
      );
      retiredAttempts = await assertHostedControlUnavailable(
        page,
        connectOrigin,
        DAEMON_ID,
        START_TIMEOUT_MS
      );
    } finally {
      if (iamSnapshot === null) fs.rmSync(iamPath, { force: true });
      else fs.writeFileSync(iamPath, iamSnapshot, { mode: 0o600 });
    }
    assert.strictEqual(fs.existsSync(iamPath), iamSnapshot !== null, 'adversarial IAM fixture changed file presence');
    if (iamSnapshot !== null) {
      assert(iamSnapshot.equals(fs.readFileSync(iamPath)), 'adversarial IAM fixture was not cleaned up');
    }
    assert(!logs.daemon.join('').includes('[dashboard/control] data channel open:'), 'hosted route unexpectedly opened a control data channel');
    const keysAfterRefusal = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
    assert.strictEqual(keysAfterRefusal.anthropic, false, 'refused hosted path unexpectedly fueled the daemon');
    assert.strictEqual(captured.length, 0, 'refused hosted path unexpectedly reached the provider mock');
    console.log(JSON.stringify({
      ok: true,
      hosted_delivery_available: false,
      hosted_signal_statuses: retiredAttempts.map(attempt => attempt.status),
      data_channel_open: false,
      daemon_anthropic_key: keysAfterRefusal.anthropic,
      provider_requests: captured.length,
    }, null, 2));
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
