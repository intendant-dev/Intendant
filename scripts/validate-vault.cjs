#!/usr/bin/env node
'use strict';

// Hosted vault/control refusal validation against a real Connect service and a
// real browser with a PRF-capable virtual authenticator. This validator has an
// intentionally narrow scope: the historical dashboard vault UI lived under
// the now-retired hosted `/app`, so it pins that the redirect cannot recreate
// that client or a daemon-control session. It does NOT validate vault creation,
// encryption, migration, recovery, or credential relay. Vault cryptography is
// exercised separately by `vault-kernel-exercise.cjs` and trusted daemon UI
// validators.
//
//   1. register a passkey account and confirm the PRF ceremony completed;
//   2. prove hosted offer/ICE/close are hard 403s and `/app` redirects;
//   3. prove the redirected Connect page exposes no dashboard or vault client.
//
// Usage: node scripts/validate-vault.cjs [--connect-binary <path>] [--connect-port <port>]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');
const { assertHostedControlUnavailable } = require('./lib/connect-hosted-refusal.cjs');

const DEFAULT_CONNECT_PORT = 9893;
const START_TIMEOUT_MS = 45000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    connectBinary: path.join(repoRoot, 'target', 'debug', 'intendant-connect'),
    connectPort: DEFAULT_CONNECT_PORT,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-port') out.connectPort = Number(argv[++i]);
    else if (arg === '--help' || arg === '-h') {
      console.log(`Usage:
  node scripts/validate-vault.cjs [options]

Hosted-refusal-only scope:
  Confirms retired hosted dashboard/vault code is unreachable. This command
  does not exercise vault cryptography or a successful credential relay.

Options:
  --connect-binary <path>  intendant-connect binary
  --connect-port <port>    Connect port (default ${DEFAULT_CONNECT_PORT})`);
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  assert(Number.isInteger(out.connectPort) && out.connectPort > 0, 'invalid connect port');
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
    await new Promise(resolve => setTimeout(resolve, 150));
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

async function main() {
  const options = parseArgs(process.argv);
  if (!fs.existsSync(options.connectBinary)) {
    throw new Error(`missing binary ${options.connectBinary}; run cargo build --bin intendant-connect`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-vault-'));
  const connectOrigin = `http://localhost:${options.connectPort}`;
  const connectApi = `http://127.0.0.1:${options.connectPort}`;
  const logs = [];
  const children = [];
  let browser = null;

  try {
    const child = spawn(options.connectBinary, [
      '--listen', `127.0.0.1:${options.connectPort}`,
      '--origin', connectOrigin,
      '--rp-id', 'localhost',
      '--static-root', path.join(options.repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', 'vault-validator-token',
    ], { cwd: options.repoRoot, stdio: ['ignore', 'pipe', 'pipe'] });
    children.push(child);
    child.stdout?.on('data', chunk => logs.push(String(chunk)));
    child.stderr?.on('data', chunk => logs.push(String(chunk)));
    await waitFor(() => httpStatus(`${connectApi}/healthz`).then(s => s === 200), START_TIMEOUT_MS, 'connect health');

    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    await addVirtualAuthenticator(browser, page);

    // ── 1. Register a passkey account ──
    await page.goto(`${connectOrigin}/connect`, { waitUntil: 'domcontentloaded', timeout: START_TIMEOUT_MS });
    await page.evaluate(() => {
      document.getElementById('account').value = `vault-user-${Date.now()}`;
    });
    await page.locator('#register').click();
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    const prfSecret = await page.evaluate(() => sessionStorage.getItem('intendant_fleet_prf_v1'));
    assert(prfSecret, 'PRF secret was not captured at registration');
    console.log('PASS vault-register account created, PRF secret captured');

    // ── 2. The retired hosted dashboard cannot expose the vault/control ──
    const retiredAttempts = await assertHostedControlUnavailable(
      page,
      connectOrigin,
      'vault-validator',
      START_TIMEOUT_MS
    );
    const hostedVaultPresent = await page.evaluate(() => Boolean(window.intendantVault));
    assert.strictEqual(hostedVaultPresent, false, 'retired hosted dashboard exposed a vault client');
    console.log(JSON.stringify({
      ok: true,
      scope: 'hosted-refusal-only',
      hosted_account_registered: true,
      hosted_dashboard_vault_available: false,
      vault_crypto_exercised: false,
      hosted_signal_statuses: retiredAttempts.map(attempt => attempt.status),
    }, null, 2));

  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) {
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
    }
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch(err => {
  console.error(`FAIL validate-vault reason="${err.message}"`);
  process.exit(1);
});
