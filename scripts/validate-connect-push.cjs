#!/usr/bin/env node
'use strict';

// Web Push E2E for the connect service: a local HTTP catcher plays the
// push service, and this script decrypts the delivered payloads with an
// INDEPENDENT RFC 8291 implementation (node:crypto) — proving the
// service's ring-based aes128gcm encryption and VAPID signing against a
// second implementation, plus the presence-alert loop end to end:
// subscribe → test push → kill the daemon → offline alert arrives.
//
// Usage: PLAYWRIGHT_NODE_PATH=$PWD/node_modules node scripts/validate-connect-push.cjs

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const CONNECT_PORT = 9885;
const CATCHER_PORT = 9886;
const DAEMON_PORT = 8893;
const DAEMON_ID = 'push-e2e-daemon';
const TOKEN = 'push-e2e-token';
const START_TIMEOUT_MS = 45000;

const b64u = buf => Buffer.from(buf).toString('base64url');

function decryptWebPush(uaEcdh, authSecret, body) {
  const salt = body.subarray(0, 16);
  const rs = body.readUInt32BE(16);
  const idlen = body[20];
  assert.strictEqual(rs, 4096, 'record size');
  assert.strictEqual(idlen, 65, 'key id length');
  const asPublic = body.subarray(21, 21 + 65);
  const ciphertext = body.subarray(21 + 65);
  const ecdhSecret = uaEcdh.computeSecret(asPublic);
  const uaPublic = uaEcdh.getPublicKey();
  const info = Buffer.concat([Buffer.from('WebPush: info\0'), uaPublic, asPublic]);
  const ikm = crypto.hkdfSync('sha256', ecdhSecret, authSecret, info, 32);
  const cek = crypto.hkdfSync('sha256', Buffer.from(ikm), salt, Buffer.from('Content-Encoding: aes128gcm\0'), 16);
  const nonce = crypto.hkdfSync('sha256', Buffer.from(ikm), salt, Buffer.from('Content-Encoding: nonce\0'), 12);
  const decipher = crypto.createDecipheriv('aes-128-gcm', Buffer.from(cek), Buffer.from(nonce));
  decipher.setAuthTag(ciphertext.subarray(ciphertext.length - 16));
  const record = Buffer.concat([decipher.update(ciphertext.subarray(0, ciphertext.length - 16)), decipher.final()]);
  assert.strictEqual(record[record.length - 1], 0x02, 'last-record delimiter');
  return JSON.parse(record.subarray(0, record.length - 1).toString('utf8'));
}

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try { const v = await fn(); if (v) return v; } catch (e) { lastError = e; }
    await new Promise(r => setTimeout(r, 250));
  }
  throw new Error(`timed out waiting for ${label}${lastError ? `: ${lastError.message}` : ''}`);
}

async function main() {
  const repoRoot = path.resolve(__dirname, '..');
  const connectBin = path.join(repoRoot, 'target', 'debug', 'intendant-connect');
  const daemonBin = path.join(repoRoot, 'target', 'debug', 'intendant');
  for (const bin of [connectBin, daemonBin]) {
    assert(fs.existsSync(bin), `missing ${bin}`);
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-push-'));
  const children = [];
  const logs = { connect: [], daemon: [] };
  let browser = null;

  const delivered = [];
  const catcher = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', c => chunks.push(c));
    req.on('end', () => {
      delivered.push({ url: req.url, headers: req.headers, body: Buffer.concat(chunks) });
      res.writeHead(201); res.end();
    });
  });
  await new Promise(resolve => catcher.listen(CATCHER_PORT, '127.0.0.1', resolve));

  function spawnLogged(cmd, args, opts, sink) {
    const child = spawn(cmd, args, opts);
    children.push(child);
    child.stdout?.on('data', c => sink.push(String(c)));
    child.stderr?.on('data', c => sink.push(String(c)));
    return child;
  }

  try {
    spawnLogged(connectBin, [
      '--listen', `127.0.0.1:${CONNECT_PORT}`,
      '--origin', `http://localhost:${CONNECT_PORT}`,
      '--rp-id', 'localhost',
      '--static-root', path.join(repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', TOKEN,
    ], {
      cwd: repoRoot,
      env: { ...process.env, INTENDANT_CONNECT_PRESENCE_OFFLINE_MS: '3000', INTENDANT_CONNECT_PRESENCE_POLL_MS: '1000' },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.connect);
    const daemonHome = path.join(tmp, 'daemon-home');
    fs.mkdirSync(daemonHome, { recursive: true });
    const daemon = spawnLogged(daemonBin, ['--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(DAEMON_PORT)], {
      cwd: tmp,
      env: {
        ...process.env,
        HOME: daemonHome,
        INTENDANT_CONNECT_RENDEZVOUS_URL: `http://127.0.0.1:${CONNECT_PORT}`,
        INTENDANT_CONNECT_DAEMON_ID: DAEMON_ID,
        INTENDANT_CONNECT_TOKEN: TOKEN,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.daemon);

    await waitFor(async () => (await fetch(`http://127.0.0.1:${CONNECT_PORT}/healthz`).catch(() => null))?.ok, START_TIMEOUT_MS, 'connect health');
    const claimCode = await waitFor(() => {
      const all = logs.connect.join('') + logs.daemon.join('');
      const m = all.match(/claim_code=([^\s"'<>]+)/) || all.match(/claim this daemon with code ([^\s"'<>]+)/);
      return m && decodeURIComponent(m[1]);
    }, START_TIMEOUT_MS, 'claim code');

    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    const client = await page.context().newCDPSession(page);
    await client.send('WebAuthn.enable');
    await client.send('WebAuthn.addVirtualAuthenticator', { options: {
      protocol: 'ctap2', transport: 'internal', hasResidentKey: true,
      hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
    await page.goto(`http://localhost:${CONNECT_PORT}/connect?claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => { document.getElementById('account').value = 'push-tester'; });
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: 20000 });
    await page.locator('#claim').click();
    await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('claimed'), { timeout: START_TIMEOUT_MS });

    // Synthetic browser subscription: node holds the UA keys.
    const uaEcdh = crypto.createECDH('prime256v1');
    uaEcdh.generateKeys();
    const authSecret = crypto.randomBytes(16);
    const subscribe = await page.evaluate(async sub => {
      const me = await fetch('/api/me').then(r => r.json());
      const resp = await fetch('/api/push/subscribe', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-intendant-csrf': me.csrf_token || '' },
        body: JSON.stringify(sub),
      });
      return { status: resp.status, body: await resp.json().catch(() => ({})) };
    }, {
      endpoint: `http://127.0.0.1:${CATCHER_PORT}/push/e2e`,
      p256dh: b64u(uaEcdh.getPublicKey()),
      auth: b64u(authSecret),
      label: 'push-e2e',
    });
    assert.strictEqual(subscribe.status, 200, `subscribe failed: ${JSON.stringify(subscribe.body)}`);

    // 1. Test push: delivered, VAPID-signed, and decryptable by node.
    const test = await page.evaluate(async () => {
      const me = await fetch('/api/me').then(r => r.json());
      const resp = await fetch('/api/push/test', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-intendant-csrf': me.csrf_token || '' },
        body: '{}',
      });
      return { status: resp.status, body: await resp.json().catch(() => ({})) };
    });
    assert.strictEqual(test.status, 200, `test push failed: ${JSON.stringify(test.body)}`);
    await waitFor(() => delivered.length >= 1, 15000, 'test push delivery');
    const first = delivered[0];
    assert(String(first.headers.authorization || '').startsWith('vapid t='), 'missing VAPID authorization');
    assert.strictEqual(first.headers['content-encoding'], 'aes128gcm');
    const testPayload = decryptWebPush(uaEcdh, authSecret, first.body);
    assert(/Test notification/.test(testPayload.body), `unexpected payload: ${JSON.stringify(testPayload)}`);

    // 2. Presence alert: kill the daemon; the offline alert must arrive
    // (threshold 3s, poll 1s) and decrypt.
    daemon.kill('SIGKILL');
    await waitFor(() => delivered.length >= 2, 30000, 'offline alert delivery');
    const alertPayload = decryptWebPush(uaEcdh, authSecret, delivered[1].body);
    assert(/went offline/.test(alertPayload.title), `unexpected alert: ${JSON.stringify(alertPayload)}`);
    assert(alertPayload.url.includes(DAEMON_ID), 'alert should deep-link to the daemon');

    console.log(JSON.stringify({ ok: true, deliveries: delivered.length, test: testPayload.title, alert: alertPayload.title }, null, 2));
  } catch (err) {
    console.error('--- connect log tail ---\n' + logs.connect.slice(-8).join('').split('\n').slice(-12).join('\n'));
    throw err;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    await new Promise(r => setTimeout(r, 400));
    for (const child of children) if (child.exitCode === null && !child.killed) child.kill('SIGKILL');
    catcher.close();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().then(() => process.exit(0)).catch(err => { console.error(err.stack || err); process.exit(1); });
