#!/usr/bin/env node
'use strict';

// Transparency log + attestations E2E: registers/claims through the real
// UI, then re-verifies everything with an INDEPENDENT RFC 9162
// implementation in node:crypto — STH signature, inclusion proof for the
// daemon-claim binding, consistency across appends (plus rejection of a
// forged root). Attestation flows run against local DNS-over-HTTPS and
// gist stubs via their env overrides.

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const CONNECT_PORT = 9884;
const STUB_PORT = 9883;
const DAEMON_PORT = 8892;
const DAEMON_ID = 'log-e2e-daemon';
const TOKEN = 'log-e2e-token';
const HANDLE = 'log-tester';
const START_TIMEOUT_MS = 45000;

const b64uToBuf = value => Buffer.from(value, 'base64url');
const sha256 = bytes => crypto.createHash('sha256').update(bytes).digest();
const nodeHash = (l, r) => sha256(Buffer.concat([Buffer.from([1]), l, r]));
const leafHash = json => sha256(Buffer.concat([Buffer.from([0]), Buffer.from(json)]));

function verifyInclusion(leaf, index, size, proof, root) {
  if (index >= size) return false;
  let fn = index, sn = size - 1, r = leaf;
  for (const p of proof) {
    if (sn === 0) return false;
    if (fn % 2 === 1 || fn === sn) {
      r = nodeHash(p, r);
      if (fn % 2 === 0) while (fn % 2 === 0 && fn !== 0) { fn >>= 1; sn >>= 1; }
    } else {
      r = nodeHash(r, p);
    }
    fn >>= 1; sn >>= 1;
  }
  return sn === 0 && r.equals(root);
}

function verifyConsistency(oldSize, newSize, oldRoot, newRoot, proof) {
  if (oldSize === newSize) return oldRoot.equals(newRoot) && proof.length === 0;
  if (oldSize === 0 || oldSize > newSize) return false;
  const complete = (oldSize & (oldSize - 1)) === 0;
  let i = 0;
  const first = complete ? oldRoot : proof[i++];
  if (!first) return false;
  let fn = oldSize - 1, sn = newSize - 1;
  while (fn % 2 === 1) { fn >>= 1; sn >>= 1; }
  let fr = first, sr = first;
  for (; i < proof.length; i += 1) {
    if (sn === 0) return false;
    const p = proof[i];
    if (fn % 2 === 1 || fn === sn) {
      fr = nodeHash(p, fr);
      sr = nodeHash(p, sr);
      if (fn % 2 === 0) while (fn % 2 === 0 && fn !== 0) { fn >>= 1; sn >>= 1; }
    } else {
      sr = nodeHash(sr, p);
    }
    fn >>= 1; sn >>= 1;
  }
  return fr.equals(oldRoot) && sr.equals(newRoot) && sn === 0;
}

function verifySthSignature(sth) {
  const spki = Buffer.concat([
    Buffer.from('3059301306072a8648ce3d020106082a8648ce3d030107034200', 'hex'),
    b64uToBuf(sth.public_key),
  ]);
  const key = crypto.createPublicKey({ key: spki, format: 'der', type: 'spki' });
  const payload = Buffer.from(`intendant-log-sth-v1\n${sth.size}\n${sth.root}\n${sth.unix_ms}`);
  return crypto.verify('sha256', payload, { key, dsaEncoding: 'ieee-p1363' }, b64uToBuf(sth.signature));
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

const getJson = async url => {
  const resp = await fetch(url);
  assert(resp.ok, `${url} -> ${resp.status}`);
  return resp.json();
};

async function main() {
  const repoRoot = path.resolve(__dirname, '..');
  const connectBin = path.join(repoRoot, 'target', 'debug', 'intendant-connect');
  const daemonBin = path.join(repoRoot, 'target', 'debug', 'intendant');
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-log-'));
  const children = [];
  const logs = { connect: [], daemon: [] };
  let browser = null;

  const claimLine = `intendant-handle=${HANDLE}@localhost:${CONNECT_PORT}`;
  const stub = http.createServer((req, res) => {
    if (req.url.startsWith('/dns-query')) {
      const url = new URL(req.url, 'http://x');
      const name = url.searchParams.get('name') || '';
      const body = name === '_intendant.example.com'
        ? { Status: 0, Answer: [{ name, type: 16, data: `"${claimLine}"` }] }
        : { Status: 3 };
      res.writeHead(200, { 'content-type': 'application/dns-json' });
      res.end(JSON.stringify(body));
      return;
    }
    if (req.url.startsWith('/octocat/')) {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(`proof for intendant\n${claimLine}\n`);
      return;
    }
    res.writeHead(404); res.end();
  });
  await new Promise(resolve => stub.listen(STUB_PORT, '127.0.0.1', resolve));

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
      env: {
        ...process.env,
        INTENDANT_CONNECT_DOH_URL: `http://127.0.0.1:${STUB_PORT}/dns-query`,
        INTENDANT_CONNECT_GIST_BASE: `http://127.0.0.1:${STUB_PORT}/`,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.connect);
    const base = `http://127.0.0.1:${CONNECT_PORT}`;
    const uiBase = `http://localhost:${CONNECT_PORT}`;
    await waitFor(async () => (await fetch(`${base}/healthz`).catch(() => null))?.ok, START_TIMEOUT_MS, 'connect health');

    // Genesis STH: empty log, signature must verify in node.
    const sth0 = await getJson(`${base}/api/log/sth`);
    assert.strictEqual(sth0.size, 0);
    assert(verifySthSignature(sth0), 'genesis STH signature');

    // Register through the real UI → account_created entry.
    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    page.on('console', m => { if (m.type() !== 'debug') logs.page = (logs.page || []), logs.page.push(`${m.type()}: ${m.text()}`); });
    page.on('pageerror', e => { logs.page = (logs.page || []), logs.page.push(`pageerror: ${e.message}`); });
    const client = await page.context().newCDPSession(page);
    await client.send('WebAuthn.enable');
    await client.send('WebAuthn.addVirtualAuthenticator', { options: {
      protocol: 'ctap2', transport: 'internal', hasResidentKey: true,
      hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
    await page.goto(`${uiBase}/connect`, { waitUntil: 'networkidle' });
    await page.evaluate(handle => { document.getElementById('account').value = handle; }, HANDLE);
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: 20000 });

    const sth1 = await getJson(`${base}/api/log/sth`);
    assert.strictEqual(sth1.size, 1, 'account_created must be logged');
    assert(verifySthSignature(sth1), 'STH1 signature');

    // Claim a daemon → daemon_claimed entry with the daemon key binding.
    const daemonHome = path.join(tmp, 'daemon-home');
    fs.mkdirSync(daemonHome, { recursive: true });
    spawnLogged(daemonBin, ['--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(DAEMON_PORT)], {
      cwd: tmp,
      env: {
        ...process.env, HOME: daemonHome,
        INTENDANT_CONNECT_RENDEZVOUS_URL: base,
        INTENDANT_CONNECT_DAEMON_ID: DAEMON_ID,
        INTENDANT_CONNECT_TOKEN: TOKEN,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.daemon);
    const claimCode = await waitFor(() => {
      const all = logs.connect.join('') + logs.daemon.join('');
      const m = all.match(/claim_code=([^\s"'<>]+)/) || all.match(/one-time claim code ([a-z0-9-]+)/i);
      return m && decodeURIComponent(m[1]);
    }, START_TIMEOUT_MS, 'claim code');
    await page.goto(`${uiBase}/connect#claim_code=${encodeURIComponent(claimCode)}`, { waitUntil: 'networkidle' });
    await page.evaluate(code => { document.getElementById('claim-code').value = code; }, claimCode);
    await page.locator('#claim').click();
    try {
      await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('No machine access was granted'), { timeout: START_TIMEOUT_MS });
    } catch (err) {
      const statusText = await page.evaluate(() => document.getElementById('claim-status')?.textContent || '(missing)');
      console.error(`claim-status text: ${JSON.stringify(statusText)}`);
      console.error('--- daemon log tail ---\n' + logs.daemon.join('').split('\n').slice(-10).join('\n'));
      throw err;
    }

    // The binding is findable and its inclusion proof verifies in node.
    const found = await getJson(`${base}/api/log/find?daemon_id=${DAEMON_ID}`);
    assert(found.found, 'daemon_claimed entry must exist');
    const entry = JSON.parse(found.leaf_json);
    assert.strictEqual(entry.daemon_id, DAEMON_ID);
    assert.strictEqual(entry.handle, HANDLE);
    assert(entry.daemon_public_key.length > 20, 'binding must carry the daemon key');
    const proof = await getJson(`${base}/api/log/proof?index=${found.index}&size=${found.size}`);
    assert(
      verifyInclusion(leafHash(found.leaf_json), found.index, found.size,
        proof.proof.map(b64uToBuf), b64uToBuf(proof.root)),
      'inclusion proof must verify independently'
    );
    assert(
      !verifyInclusion(leafHash('{"forged":true}'), found.index, found.size,
        proof.proof.map(b64uToBuf), b64uToBuf(proof.root)),
      'forged leaf must not verify'
    );

    // Attestations via the stubs → badges + log growth.
    const attest = await page.evaluate(async stubPort => {
      const me = await fetch('/api/me').then(r => r.json());
      const headers = { 'content-type': 'application/json', 'x-intendant-csrf': me.csrf_token || '' };
      const dns = await fetch('/api/attest/dns', { method: 'POST', headers, body: JSON.stringify({ domain: 'example.com' }) });
      const gh = await fetch('/api/attest/github', { method: 'POST', headers, body: JSON.stringify({ gist_raw_url: `http://127.0.0.1:${stubPort}/octocat/abc123/raw` }) });
      const bad = await fetch('/api/attest/dns', { method: 'POST', headers, body: JSON.stringify({ domain: 'other.example.net' }) });
      return { dns: dns.status, gh: gh.status, bad: bad.status };
    }, STUB_PORT);
    assert.deepStrictEqual(attest, { dns: 200, gh: 200, bad: 400 }, `attestations: ${JSON.stringify(attest)}`);

    const directory = await getJson(`${base}/api/directory/${HANDLE}`);
    assert(directory.found && directory.attestations.length === 2, 'directory must show both badges');
    assert(directory.attestations.some(a => a.kind === 'dns' && a.subject === 'example.com'));
    assert(directory.attestations.some(a => a.kind === 'github' && a.subject === 'github:octocat'));

    // Consistency 1 → now verifies in node; forged old root fails.
    const sthN = await getJson(`${base}/api/log/sth`);
    assert(sthN.size >= 4, `expected >=4 entries, got ${sthN.size}`);
    const consistency = await getJson(`${base}/api/log/consistency?old=1&new=${sthN.size}`);
    assert(
      verifyConsistency(1, sthN.size, b64uToBuf(sth1.root), b64uToBuf(sthN.root),
        consistency.proof.map(b64uToBuf)),
      'consistency proof must verify independently'
    );
    assert(
      !verifyConsistency(1, sthN.size, sha256(Buffer.from('rewritten')), b64uToBuf(sthN.root),
        consistency.proof.map(b64uToBuf)),
      'forged history must not verify'
    );

    // The page's own pin-and-verify agrees (pill goes green after reload).
    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForFunction(() => /consistent/.test(document.getElementById('log-pill')?.textContent || ''), { timeout: 15000 });

    console.log(JSON.stringify({
      ok: true,
      log_size: sthN.size,
      binding: { daemon_id: entry.daemon_id, handle: entry.handle },
      badges: directory.attestations.map(a => a.subject),
    }, null, 2));
  } catch (err) {
    console.error('--- connect log tail ---\n' + logs.connect.join('').split('\n').slice(-12).join('\n'));
    console.error('--- page console tail ---\n' + (logs.page || []).slice(-15).join('\n'));
    throw err;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    await new Promise(r => setTimeout(r, 400));
    for (const child of children) if (child.exitCode === null && !child.killed) child.kill('SIGKILL');
    stub.close();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().then(() => process.exit(0)).catch(err => { console.error(err.stack || err); process.exit(1); });
