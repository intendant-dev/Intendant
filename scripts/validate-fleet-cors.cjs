#!/usr/bin/env node
'use strict';

// Fleet CORS / same-origin API hardening smoke (trust architecture phase 4,
// ported from the throwaway two-daemon session smoke).
//
// Exercises the daemon-wide origin gate with two daemons and no browser —
// the gate keys purely on the Origin header, so plain HTTP is a faithful
// driver (a real browser only adds client-side ACAO enforcement, which is
// not our code):
//   1. public bootstrap surfaces stay wildcard-readable (/config, the org
//      doorbell) even for foreign origins;
//   2. /api/* requests without an Origin header (curl, native code, the
//      macOS app's URLSession proxy) pass untouched and carry no ACAO;
//   3. foreign-origin /api/* requests are refused daemon-wide (403), on
//      fleet paths and non-fleet paths alike;
//   4. own origin and the intendant:// app scheme pass;
//   5. a fleet-allowlisted origin (an approved peer identity's card_url)
//      passes on the fleet Access APIs with the origin echoed + Vary, for
//      reads and state-changing writes both — the phase-4 fanout path.
//
// Usage: node scripts/validate-fleet-cors.cjs [--daemon-binary <path>]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');

const DEFAULT_PORT_A = 8896;
const DEFAULT_PORT_B = 8897;
const START_TIMEOUT_MS = 45000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    daemonBinary: path.join(repoRoot, 'target', 'debug', 'intendant'),
    portA: DEFAULT_PORT_A,
    portB: DEFAULT_PORT_B,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--port-a') out.portA = Number(argv[++i]);
    else if (arg === '--port-b') out.portB = Number(argv[++i]);
    else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/validate-fleet-cors.cjs [--daemon-binary <path>]');
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return out;
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

async function request(url, { method = 'GET', origin = null, body = null } = {}) {
  const headers = {};
  if (origin) headers.Origin = origin;
  if (body !== null) headers['content-type'] = 'application/json';
  const resp = await fetch(url, {
    method,
    headers,
    body: body === null ? undefined : JSON.stringify(body),
  });
  const text = await resp.text().catch(() => '');
  return {
    status: resp.status,
    acao: resp.headers.get('access-control-allow-origin'),
    vary: resp.headers.get('vary'),
    text,
  };
}

async function main() {
  const options = parseArgs(process.argv);
  if (!fs.existsSync(options.daemonBinary)) {
    throw new Error(`missing binary ${options.daemonBinary}; run cargo build --bin intendant`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-fleet-cors-'));
  const homeA = path.join(tmp, 'home-a');
  const homeB = path.join(tmp, 'home-b');
  fs.mkdirSync(homeA, { recursive: true });
  fs.mkdirSync(homeB, { recursive: true });
  const originA = `http://127.0.0.1:${options.portA}`;
  const originB = `http://127.0.0.1:${options.portB}`;
  const evilOrigin = 'https://evil.example';

  const children = [];
  const logs = { a: [], b: [] };
  function spawnDaemon(home, port, sink) {
    const child = spawn(options.daemonBinary, [
      '--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(port),
    ], {
      cwd: tmp,
      env: { ...process.env, HOME: home },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    children.push(child);
    child.stdout?.on('data', chunk => sink.push(String(chunk)));
    child.stderr?.on('data', chunk => sink.push(String(chunk)));
    return child;
  }

  try {
    spawnDaemon(homeA, options.portA, logs.a);
    spawnDaemon(homeB, options.portB, logs.b);
    await waitFor(async () => (await request(`${originA}/config`)).status === 200, START_TIMEOUT_MS, 'daemon A readiness');
    await waitFor(async () => (await request(`${originB}/config`)).status === 200, START_TIMEOUT_MS, 'daemon B readiness');

    // 1. Public bootstrap surfaces stay wildcard-readable for any origin.
    const config = await request(`${originA}/config`, { origin: evilOrigin });
    assert.strictEqual(config.status, 200, `public /config refused: ${config.status}`);
    assert.strictEqual(config.acao, '*', `public /config lost wildcard CORS: ${config.acao}`);
    const doorbell = await request(`${originA}/api/access/org-grants`, {
      method: 'POST',
      origin: evilOrigin,
      body: { junk: true },
    });
    assert.strictEqual(doorbell.status, 400, `org doorbell should process (not refuse) foreign origins: ${doorbell.status} ${doorbell.text}`);
    assert.strictEqual(doorbell.acao, '*', `org doorbell lost wildcard CORS: ${doorbell.acao}`);

    // 2. No Origin header: passes, and no ACAO is baked into API responses.
    const bare = await request(`${originA}/api/access/overview`);
    assert.strictEqual(bare.status, 200, `origin-less API read refused: ${bare.status}`);
    assert.strictEqual(bare.acao, null, `API response leaked ACAO without an Origin: ${bare.acao}`);

    // 3. Foreign origins are refused daemon-wide, fleet and non-fleet paths.
    for (const apiPath of ['/api/access/overview', '/api/sessions', '/api/access/iam/state']) {
      const refused = await request(`${originA}${apiPath}`, { origin: evilOrigin });
      assert.strictEqual(refused.status, 403, `${apiPath} allowed a foreign origin: ${refused.status}`);
      assert(/cross-origin caller is not allowed/.test(refused.text), `${apiPath} refusal lacks the gate message: ${refused.text}`);
      assert.strictEqual(refused.acao, null, `${apiPath} echoed CORS to a refused origin: ${refused.acao}`);
    }
    const evilWrite = await request(`${originA}/api/access/orgs/trust`, {
      method: 'POST',
      origin: evilOrigin,
      body: { handle: 'acme', root_key: 'junk' },
    });
    assert.strictEqual(evilWrite.status, 403, `foreign-origin fleet write not refused: ${evilWrite.status}`);

    // 4. Own origin and the app scheme pass.
    const own = await request(`${originA}/api/access/overview`, { origin: originA });
    assert.strictEqual(own.status, 200, `own origin refused: ${own.status}`);
    const app = await request(`${originA}/api/access/overview`, { origin: 'intendant://dashboard' });
    assert.strictEqual(app.status, 200, `intendant:// app origin refused: ${app.status}`);

    // 5. Fleet allowlist: before B knows A, A's origin is foreign to B…
    const before = await request(`${originB}/api/access/overview`, { origin: originA });
    assert.strictEqual(before.status, 403, `B allowed A before any trust: ${before.status}`);

    // …after B approves a peer identity whose card_url lives at A's origin
    // (the same record inbound peer approval writes), A's origin passes on
    // the fleet Access APIs with the origin echoed.
    const identityDir = path.join(homeB, '.intendant', 'access-certs', 'peer-access-identities');
    fs.mkdirSync(identityDir, { recursive: true });
    const fingerprint = 'aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66';
    fs.writeFileSync(path.join(identityDir, `${fingerprint}.json`), `${JSON.stringify({
      version: 1,
      fingerprint,
      label: 'fleet-cors-smoke peer A',
      profile: 'session-reader',
      status: 'approved',
      card_url: `${originA}/.well-known/agent-card.json`,
      created_at_unix: Math.floor(Date.now() / 1000),
    }, null, 2)}\n`);

    const after = await request(`${originB}/api/access/overview`, { origin: originA });
    assert.strictEqual(after.status, 200, `allowlisted origin still refused: ${after.status} ${after.text}`);
    assert.strictEqual(after.acao, originA, `fleet path did not echo the allowlisted origin: ${after.acao}`);
    assert(/origin/i.test(String(after.vary || '')), `fleet path missing Vary: Origin: ${after.vary}`);

    // The write side of the fanout path: processed (400 on a junk key),
    // not origin-refused, with the echo intact.
    const fleetWrite = await request(`${originB}/api/access/orgs/trust`, {
      method: 'POST',
      origin: originA,
      body: { handle: 'acme', root_key: 'junk' },
    });
    assert.strictEqual(fleetWrite.status, 400, `allowlisted fleet write was not processed: ${fleetWrite.status} ${fleetWrite.text}`);
    assert.strictEqual(fleetWrite.acao, originA, `fleet write did not echo the allowlisted origin: ${fleetWrite.acao}`);

    // Non-fleet APIs stay closed even for the allowlisted origin — the
    // allowlist opens exactly the fleet Access surface, nothing else.
    const nonFleet = await request(`${originB}/api/sessions`, { origin: originA });
    assert.strictEqual(nonFleet.status, 403, `allowlisted origin escaped the fleet surface: ${nonFleet.status}`);

    console.log(JSON.stringify({
      ok: true,
      origin_a: originA,
      origin_b: originB,
      checks: ['public-wildcard', 'doorbell-open', 'originless-pass-no-acao', 'foreign-403-daemon-wide', 'own-and-app-origin', 'fleet-allowlist-echo', 'fleet-write-processed', 'allowlist-scoped-to-fleet-paths'],
    }, null, 2));
  } catch (err) {
    const tail = sink => sink.slice(-10).join('').split('\n').slice(-15).join('\n');
    console.error('--- daemon A log tail ---\n' + tail(logs.a));
    console.error('--- daemon B log tail ---\n' + tail(logs.b));
    throw err;
  } finally {
    for (const child of children) {
      if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    }
    await new Promise(resolve => setTimeout(resolve, 400));
    for (const child of children) {
      if (child.exitCode === null && !child.killed) child.kill('SIGKILL');
    }
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main()
  .then(() => process.exit(0))
  .catch(err => {
    console.error(err && err.stack || err);
    process.exit(1);
  });
