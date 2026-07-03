#!/usr/bin/env node
'use strict';

// Credential vault v1 end-to-end validation against a real hosted Connect
// service and a real browser with a PRF-capable virtual authenticator:
//
//   1. register a passkey account (PRF secret captured at the ceremony)
//   2. create the vault through the Advanced-pane ceremony (phrase shown once)
//   3. voice-key migration into the vault (localStorage copy removed)
//   4. add an API-key entry through the UI
//   5. the hosted store holds only ciphertext (no plaintext substrings)
//   6. reload → silent PRF auto-unlock
//   7. lock / passkey unlock round-trip
//   8. fresh session (PRF secret dropped) → recovery-phrase unlock
//   9. "Enroll this passkey" recognizes the already-enrolled credential
//
// Usage: node scripts/validate-vault.cjs [--connect-binary <path>] [--connect-port <port>]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_CONNECT_PORT = 9893;
const START_TIMEOUT_MS = 45000;
const STEP_TIMEOUT_MS = 20000;

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
      console.log('Usage: node scripts/validate-vault.cjs [--connect-binary <path>] [--connect-port <port>]');
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

function vaultStateOf(page) {
  return page.evaluate(() => window.intendantVault?.state() || null);
}

async function waitVault(page, predicate, label) {
  return waitFor(async () => {
    const state = await vaultStateOf(page);
    return state && predicate(state) ? state : null;
  }, STEP_TIMEOUT_MS, label);
}

// Click a vault-section button by its visible text, inside the page so a
// background re-render between locate and click cannot detach the node.
async function clickVaultButton(page, text) {
  const clicked = await page.evaluate(needle => {
    const buttons = Array.from(document.querySelectorAll('#access-vault-section button'));
    const button = buttons.find(b => b.textContent.trim() === needle);
    if (!button) return false;
    button.click();
    return true;
  }, text);
  assert(clicked, `vault button not found: ${text}`);
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
    const consoleMessages = [];
    page.on('console', msg => consoleMessages.push(msg.text()));
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

    // ── 2. Create the vault through the ceremony ──
    const voiceKey = 'voice-test-gemini-key-1234';
    // The daemon_id only satisfies the /app gate; the vault needs no
    // daemon — the control transport failing to connect is irrelevant.
    await page.goto(`${connectOrigin}/app?connect=1&daemon_id=vault-validator#access/advanced`, { timeout: START_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantVault), { timeout: START_TIMEOUT_MS });
    // Seed a legacy voice key before the first unlock so migration runs.
    await page.evaluate(key => localStorage.setItem('gemini_api_key', key), voiceKey);
    await waitVault(page, s => s.status === 'none', 'vault status none before creation');

    await clickVaultButton(page, 'Create vault');
    await waitFor(() => page.evaluate(() =>
      document.querySelectorAll('#access-vault-section .vault-words .w').length === 12
    ), STEP_TIMEOUT_MS, 'phrase ceremony on screen');
    const phrase = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#access-vault-section .vault-words .w'))
        .map(w => w.textContent.replace(/^\d+/, '').trim())
        .join(' ')
    );
    assert.strictEqual(phrase.split(' ').length, 12, `ceremony did not show 12 words: ${phrase}`);
    await clickVaultButton(page, 'I saved the phrase — create the vault');
    const created = await waitVault(page, s => s.status === 'unlocked', 'vault unlocked after creation');
    assert(created.envelopes.some(e => e.kind === 'phrase'), 'no phrase envelope');
    assert(created.envelopes.some(e => e.kind === 'prf'), 'no prf envelope');
    console.log(`PASS vault-create unlocked, envelopes=[${created.envelopes.map(e => e.kind).join(',')}]`);

    // ── 3. Voice-key migration ──
    const migrated = await waitVault(
      page,
      s => s.entries.some(e => e.provider === 'gemini' && e.voice) && s.revision >= 2,
      'voice key migrated into the vault'
    );
    const legacyCopy = await page.evaluate(() => localStorage.getItem('gemini_api_key'));
    assert.strictEqual(legacyCopy, null, 'legacy localStorage voice key was not removed after publish');
    const mirrored = await page.evaluate(() => window.intendantVault.voiceApiKeyGet('gemini_api_key'));
    assert.strictEqual(mirrored, voiceKey, 'voiceApiKeyGet does not serve the migrated key');
    console.log(`PASS vault-voice-migration entry present, localStorage cleared, revision=${migrated.revision}`);

    // ── 4. Add an API-key entry through the UI ──
    const anthropicKey = 'sk-ant-test-vault-secret-9876';
    // Fill + click synchronously so a background render cannot clobber the form.
    const added = await page.evaluate(secret => {
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
      inputs[0].value = 'Validator Anthropic';
      inputs[1].value = secret;
      const button = Array.from(section.querySelectorAll('button'))
        .find(b => b.textContent.trim() === 'Add to vault');
      if (!button) return 'no add button';
      button.click();
      return 'ok';
    }, anthropicKey);
    assert.strictEqual(added, 'ok', `add-entry interaction failed: ${added}`);
    const afterAdd = await waitVault(
      page,
      s => s.entries.some(e => e.provider === 'anthropic' && e.label === 'Validator Anthropic') && s.revision >= 3,
      'anthropic entry stored and published'
    );
    console.log(`PASS vault-add-entry entries=${afterAdd.entries.length}, revision=${afterAdd.revision}`);

    // ── 5. The hosted store holds only ciphertext ──
    const rawBlob = await page.evaluate(() => fetch('/api/vault').then(r => r.text()));
    assert(rawBlob.includes('"revision"'), `vault fetch looks wrong: ${rawBlob.slice(0, 200)}`);
    assert(!rawBlob.includes(anthropicKey), 'plaintext API key visible in the hosted store');
    assert(!rawBlob.includes(voiceKey), 'plaintext voice key visible in the hosted store');
    assert(!rawBlob.includes(phrase.split(' ')[0] + ' ' + phrase.split(' ')[1]), 'phrase material visible in the hosted store');
    console.log('PASS vault-blind-store ciphertext only, no plaintext substrings');

    // ── 6. Reload → silent PRF auto-unlock ──
    await page.reload({ timeout: START_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantVault), { timeout: START_TIMEOUT_MS });
    const afterReload = await waitVault(page, s => s.status === 'unlocked', 'auto-unlock after reload');
    assert.strictEqual(afterReload.entries.length, 2, `expected 2 entries after reload, got ${afterReload.entries.length}`);
    assert(afterReload.matchedEnvelopeId, 'auto-unlock did not record the matching prf envelope');
    console.log('PASS vault-auto-unlock silent PRF unlock after reload');

    // ── 7. Lock / passkey unlock round-trip ──
    await page.evaluate(() => window.intendantVault.lock());
    await waitVault(page, s => s.status === 'locked', 'vault locked');
    await clickVaultButton(page, 'Unlock with passkey');
    await waitVault(page, s => s.status === 'unlocked', 'passkey unlock from locked state');
    console.log('PASS vault-lock-unlock passkey round-trip');

    // ── 8. Fresh session: recovery-phrase unlock ──
    await page.evaluate(() => sessionStorage.removeItem('intendant_fleet_prf_v1'));
    await page.reload({ timeout: START_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantVault), { timeout: START_TIMEOUT_MS });
    const lockedFresh = await waitVault(page, s => s.status === 'locked', 'locked without a session PRF secret');
    assert(!lockedFresh.matchedEnvelopeId, 'locked state should not remember an envelope match');
    const phraseUnlock = await page.evaluate(async p => {
      const section = document.getElementById('access-vault-section');
      const input = section.querySelector('.vault-phrase-input');
      if (!input) return 'no phrase input';
      input.value = p;
      const button = Array.from(section.querySelectorAll('button'))
        .find(b => b.textContent.trim() === 'Unlock with phrase');
      if (!button) return 'no phrase unlock button';
      button.click();
      return 'ok';
    }, phrase);
    assert.strictEqual(phraseUnlock, 'ok', `phrase unlock interaction failed: ${phraseUnlock}`);
    const unlockedByPhrase = await waitVault(page, s => s.status === 'unlocked', 'recovery-phrase unlock');
    assert(!unlockedByPhrase.matchedEnvelopeId, 'phrase unlock should not match a prf envelope');
    console.log('PASS vault-phrase-unlock recovery phrase opens the vault');

    // ── 9. Enroll-this-passkey recognizes the enrolled credential ──
    await clickVaultButton(page, 'Enroll this passkey');
    const enrolled = await waitVault(page, s => Boolean(s.matchedEnvelopeId), 'passkey enrollment resolution');
    assert.strictEqual(
      enrolled.envelopes.filter(e => e.kind === 'prf').length,
      1,
      'already-enrolled passkey should not add a duplicate envelope'
    );
    console.log('PASS vault-enroll already-enrolled passkey recognized without a duplicate envelope');

    console.log('PASS validate-vault all scenarios');
  } catch (err) {
    console.error(`FAIL validate-vault reason="${err.message}"`);
    console.error(logs.join('').split('\n').slice(-25).join('\n'));
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
  console.error(`FAIL validate-vault reason="${err.message}"`);
  process.exit(1);
});
