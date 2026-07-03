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
//   7. oauth materialization lifecycle + the UI custody story + --owner
//   8. access-token OAuth mode against a mock token endpoint: browser
//      refresh with rotation written back to the vault, refresh-free
//      material on disk, near-expiry re-grant on the renewal tick, the
//      daemon's fail-closed refusal of refresh-bearing material, and the
//      full-credential opt-in still carrying the whole auth file
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

    // ── OAuth materialization lifecycle (Codex + Claude Code) ──
    // OAuth leases materialize a private auth file for the child process;
    // revocation must delete it.
    const codexGrant = await rpc('api_credential_lease_grant', {
      kind: 'oauth:codex',
      label: 'E2E Codex OAuth',
      material: JSON.stringify({ tokens: { access_token: 'at-codex-e2e' } }),
      ttl_ms: 120000,
      offline_ms: 0,
    });
    const codexAuthPath = path.join(daemonHome, '.intendant', 'leased-auth', 'codex-home', 'auth.json');
    await waitFor(() => fs.existsSync(codexAuthPath), STEP_TIMEOUT_MS, 'materialized codex auth.json');
    assert(fs.readFileSync(codexAuthPath, 'utf8').includes('at-codex-e2e'), 'codex auth.json missing leased material');
    if (process.platform !== 'win32') {
      assert.strictEqual(fs.statSync(codexAuthPath).mode & 0o777, 0o600, 'codex auth.json must be 0600');
      assert.strictEqual(fs.statSync(path.dirname(codexAuthPath)).mode & 0o777, 0o700, 'codex home dir must be 0700');
    }

    const claudeGrant = await rpc('api_credential_lease_grant', {
      kind: 'oauth:claude-code',
      label: 'E2E Claude Code OAuth',
      material: JSON.stringify({ claudeAiOauth: { accessToken: 'at-claude-e2e' } }),
      ttl_ms: 120000,
      offline_ms: 0,
    });
    const claudeCredsPath = path.join(daemonHome, '.intendant', 'leased-auth', 'claude-home', '.credentials.json');
    await waitFor(() => fs.existsSync(claudeCredsPath), STEP_TIMEOUT_MS, 'materialized claude .credentials.json');
    assert(fs.readFileSync(claudeCredsPath, 'utf8').includes('at-claude-e2e'), 'claude credentials missing leased material');
    console.log('PASS lease-oauth-materialize private auth files written for both agents');

    await rpc('api_credential_lease_revoke', { lease_id: codexGrant.lease_id });
    await waitFor(() => !fs.existsSync(codexAuthPath), STEP_TIMEOUT_MS, 'codex materialization deleted on revoke');
    assert(fs.existsSync(claudeCredsPath), 'revoking codex must not touch the claude materialization');
    await rpc('api_credential_lease_revoke', { lease_id: claudeGrant.lease_id });
    await waitFor(() => !fs.existsSync(claudeCredsPath), STEP_TIMEOUT_MS, 'claude materialization deleted on revoke');
    console.log('PASS lease-oauth-revoke materialized auth deleted per kind');

    // ── The full custody story through the rendered UI ──
    // register → create vault (phrase ceremony) → store a key → fuel the
    // daemon from the fueling panel → lease visible → revoke → unfueled.
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

    const uiKey = 'sk-ant-ui-custody-story';
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
      inputs[0].value = 'UI Anthropic';
      inputs[1].value = secret;
      const button = Array.from(section.querySelectorAll('button'))
        .find(b => b.textContent.trim() === 'Add to vault');
      if (!button) return 'no add button';
      button.click();
      return 'ok';
    }, uiKey);
    assert.strictEqual(addOutcome, 'ok', `UI add-entry failed: ${addOutcome}`);
    await waitFor(async () => (await vaultState())?.entries.some(e => e.label === 'UI Anthropic'), STEP_TIMEOUT_MS, 'entry stored');

    await uiClick('Fuel: UI Anthropic');
    await waitFor(async () => {
      const leases = await page.evaluate(() => window.intendantVault.leases());
      return leases.leases.some(l => l.kind === 'api_key:anthropic' && l.label === 'UI Anthropic');
    }, STEP_TIMEOUT_MS, 'lease visible in the fueling panel');
    const keysAfterUiFuel = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
    assert.strictEqual(keysAfterUiFuel.anthropic, true, 'UI fueling did not reach the daemon');
    const ownIds = await page.evaluate(() => window.intendantVault.leases().ownLeaseIds);
    assert.strictEqual(ownIds.length, 1, 'the granting tab must track its own lease for renewal');
    console.log('PASS lease-ui-fuel vault entry fuels the daemon through the panel');

    await uiClick('Revoke');
    await waitFor(async () => {
      const keys = await fetch(`${daemonOrigin}/api/api-key-status`).then(r => r.json());
      return keys.anthropic === false;
    }, STEP_TIMEOUT_MS, 'daemon unfueled after UI revoke');
    console.log('PASS lease-ui-revoke panel revocation unfuels the daemon');

    // ── Access-token OAuth mode (the default for oauth kinds) ──
    // A mock token endpoint stands in for auth.openai.com: it rotates the
    // refresh token on every use and mints JWT-shaped access tokens (the
    // browser reads codex expiry from the JWT exp claim). CORS headers on
    // both OPTIONS and POST are load-bearing — the refreshing page is on
    // the connect origin.
    const b64url = obj => Buffer.from(JSON.stringify(obj)).toString('base64url');
    const oauthMock = { refreshToken: 'rt-0', expiresInSec: 300, refreshCount: 0, lastBody: null };
    mockOauthServer = require('http').createServer((req, res) => {
      const cors = {
        'Access-Control-Allow-Origin': '*',
        'Access-Control-Allow-Methods': 'POST, OPTIONS',
        'Access-Control-Allow-Headers': 'content-type',
      };
      if (req.method === 'OPTIONS') {
        res.writeHead(204, cors);
        res.end();
        return;
      }
      let body = '';
      req.on('data', chunk => { body += chunk; });
      req.on('end', () => {
        let parsed = {};
        try { parsed = JSON.parse(body); } catch { /* fall through to invalid_grant */ }
        oauthMock.lastBody = parsed;
        if (parsed.grant_type !== 'refresh_token' || parsed.refresh_token !== oauthMock.refreshToken) {
          res.writeHead(401, { ...cors, 'content-type': 'application/json' });
          res.end(JSON.stringify({ error: 'invalid_grant' }));
          return;
        }
        oauthMock.refreshCount += 1;
        oauthMock.refreshToken = `rt-${oauthMock.refreshCount}`;
        res.writeHead(200, { ...cors, 'content-type': 'application/json' });
        res.end(JSON.stringify({
          // n makes consecutive tokens distinct even within one epoch
          // second (exp alone would collide on a fast run).
          access_token: `${b64url({ alg: 'none' })}.${b64url({ exp: Math.floor(Date.now() / 1000) + oauthMock.expiresInSec, n: oauthMock.refreshCount })}.e2e`,
          id_token: `idt-${oauthMock.refreshCount}`,
          refresh_token: oauthMock.refreshToken,
          expires_in: oauthMock.expiresInSec,
        }));
      });
    });
    await new Promise(resolve => mockOauthServer.listen(MOCK_OAUTH_PORT, '127.0.0.1', resolve));

    // Store the full codex auth file in the vault through the real form.
    const codexSeed = JSON.stringify({
      OPENAI_API_KEY: null,
      tokens: { access_token: 'stale-not-a-jwt', refresh_token: 'rt-0', account_id: 'acct-e2e' },
      last_refresh: '2026-01-01T00:00:00.000Z',
    });
    const addCodex = await page.evaluate(secret => {
      const section = document.getElementById('access-vault-section');
      const fold = Array.from(section.querySelectorAll('details summary'))
        .find(s => s.textContent.trim() === 'Add a credential');
      if (!fold) return 'no add fold';
      fold.parentElement.open = true;
      const selects = section.querySelectorAll('.vault-form-grid select');
      if (selects.length < 2) return 'form fields missing';
      selects[0].value = 'oauth';
      selects[0].dispatchEvent(new Event('change'));
      selects[1].value = 'codex';
      const inputs = section.querySelectorAll('.vault-form-grid input');
      inputs[0].value = 'UI Codex';
      const area = fold.parentElement.querySelector('textarea');
      if (!area) return 'no auth textarea';
      area.value = secret;
      const button = Array.from(section.querySelectorAll('button'))
        .find(b => b.textContent.trim() === 'Add to vault');
      if (!button) return 'no add button';
      button.click();
      return 'ok';
    }, codexSeed);
    assert.strictEqual(addCodex, 'ok', `UI add-codex failed: ${addCodex}`);
    const codexEntryId = await waitFor(async () =>
      (await vaultState())?.entries.find(e => e.kind === 'oauth' && e.provider === 'codex')?.id,
    STEP_TIMEOUT_MS, 'codex entry stored');

    await page.evaluate(url => window.intendantVault.setOauthEndpoints({ 'oauth:codex': url }),
      `http://127.0.0.1:${MOCK_OAUTH_PORT}/oauth/token`);
    await page.evaluate(id => window.intendantVault.fuelEntry(id), codexEntryId);
    const atLease = await waitFor(async () => {
      const status = await rpc('api_credential_lease_status');
      const lease = status.leases.find(l => l.kind === 'oauth:codex');
      return lease && lease.mode === 'access_token' ? lease : null;
    }, STEP_TIMEOUT_MS, 'access-token codex lease');
    const codexAuthAt = JSON.parse(fs.readFileSync(codexAuthPath, 'utf8'));
    assert.strictEqual(codexAuthAt.tokens.refresh_token, '', 'materialized auth must be refresh-free');
    assert.strictEqual(codexAuthAt.OPENAI_API_KEY, null, 'materialized auth must not carry an API key');
    assert.strictEqual(codexAuthAt.tokens.account_id, 'acct-e2e', 'non-secret fields must survive the strip');
    assert(String(codexAuthAt.tokens.access_token).split('.').length === 3, 'expected the freshly minted JWT on disk');
    assert.strictEqual(oauthMock.lastBody?.client_id, 'app_EMoamEEZ73f0CkXaXp7hrann', 'refresh must use the codex public client id');
    const vaultCodexAfterFuel = JSON.parse(
      (await vaultState()).entries.find(e => e.id === codexEntryId).secret
    );
    assert.strictEqual(vaultCodexAfterFuel.tokens.refresh_token, 'rt-1', 'rotated refresh token must be written back to the vault');
    assert.strictEqual(vaultCodexAfterFuel.tokens.access_token, codexAuthAt.tokens.access_token, 'vault copy tracks the fresh access token');
    console.log('PASS lease-oauth-access-token browser-refreshed grant: refresh-free on disk, rotation in the vault');

    // The 300 s token life sits inside the 10-minute margin, so one
    // renewal tick must refresh again and re-grant (new lease id).
    await page.evaluate(() => window.intendantVault.renewTick());
    const atLease2 = await waitFor(async () => {
      const status = await rpc('api_credential_lease_status');
      const lease = status.leases.find(l => l.kind === 'oauth:codex');
      return lease && lease.lease_id !== atLease.lease_id ? lease : null;
    }, STEP_TIMEOUT_MS, 're-granted access-token lease');
    assert.strictEqual(atLease2.mode, 'access_token');
    const codexAuthAt2 = JSON.parse(fs.readFileSync(codexAuthPath, 'utf8'));
    assert.notStrictEqual(codexAuthAt2.tokens.access_token, codexAuthAt.tokens.access_token, 'renewal tick must materialize the newer token');
    assert.strictEqual(codexAuthAt2.tokens.refresh_token, '', 're-granted material must stay refresh-free');
    const vaultCodexAfterTick = JSON.parse(
      (await vaultState()).entries.find(e => e.id === codexEntryId).secret
    );
    assert.strictEqual(vaultCodexAfterTick.tokens.refresh_token, 'rt-2', 'second rotation must be written back too');
    const ownAfterTick = await page.evaluate(() => window.intendantVault.leases().ownLeaseIds);
    assert(ownAfterTick.includes(atLease2.lease_id), 'the tab must renew the replacement lease');
    console.log('PASS lease-oauth-access-token-renew near-expiry tick re-granted fresh material');

    // Fail-closed on the daemon: material claiming access-token mode but
    // still carrying durable authority is refused (both oauth kinds).
    for (const [kind, material] of [
      ['oauth:codex', JSON.stringify({ tokens: { access_token: 'a', refresh_token: 'r' } })],
      ['oauth:claude-code', JSON.stringify({ claudeAiOauth: { accessToken: 'a', refreshToken: 'r' } })],
    ]) {
      let refused = false;
      try {
        await rpc('api_credential_lease_grant', { kind, label: 'x', mode: 'access_token', material });
      } catch (err) {
        refused = /refresh token/.test(String((err && err.message) || err));
      }
      assert(refused, `daemon must refuse refresh-bearing access-token material for ${kind}`);
    }
    console.log('PASS lease-oauth-fail-closed refresh-bearing access-token grants refused server-side');

    // The full-credential opt-in still leases the whole auth file.
    await page.evaluate(() => window.intendantVault.setOauthLeases(true));
    await page.evaluate(id => window.intendantVault.fuelEntry(id), codexEntryId);
    await waitFor(async () => {
      const status = await rpc('api_credential_lease_status');
      const lease = status.leases.find(l => l.kind === 'oauth:codex');
      return lease && lease.mode === 'full_credential' ? lease : null;
    }, STEP_TIMEOUT_MS, 'full-credential codex lease');
    const codexAuthFull = JSON.parse(fs.readFileSync(codexAuthPath, 'utf8'));
    assert.strictEqual(codexAuthFull.tokens.refresh_token, 'rt-2', 'full-credential mode leases the refresh token');
    await page.evaluate(() => window.intendantVault.setOauthLeases(false));
    await rpc('api_credential_lease_revoke', { kind: 'oauth:codex' });
    await waitFor(() => !fs.existsSync(codexAuthPath), STEP_TIMEOUT_MS, 'codex materialization deleted after the mode scenarios');
    console.log('PASS lease-oauth-full-credential opt-in leases the whole auth file');

    // ── --owner bootstrap (install.sh step 6) ──
    // A fresh daemon started with --owner <client-key-fingerprint> must
    // seed a root grant pinned to that key at startup, and a restart with
    // the same flag must not duplicate grants or grow the audit log.
    const ownerHome = path.join(tmp, 'owner-home');
    fs.mkdirSync(ownerHome, { recursive: true });
    const ownerFp = 'E2E_Owner_Key-Fingerprint';
    // Offset past the org-validator port block (8898/8899).
    const ownerPort = options.daemonPort + 11;
    const ownerIamPath = path.join(ownerHome, '.intendant', 'access-certs', 'iam.json');
    const spawnOwnerDaemon = () => spawnLogged(options.daemonBinary, [
      '--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(ownerPort),
      '--owner', ownerFp,
    ], {
      cwd: tmp,
      env: { ...daemonEnv, HOME: ownerHome },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.daemon);

    const ownerChild = spawnOwnerDaemon();
    await waitFor(() => httpStatus(`http://127.0.0.1:${ownerPort}/config`).then(s => s === 200), START_TIMEOUT_MS, 'owner daemon readiness');
    const ownerIam = JSON.parse(fs.readFileSync(ownerIamPath, 'utf8'));
    const ownerPrincipal = ownerIam.principals.find(p =>
      p.kind === 'client_key' && (p.authn || []).some(a => a.fingerprint === ownerFp));
    assert(ownerPrincipal, `no client_key principal for the owner fingerprint: ${JSON.stringify(ownerIam.principals)}`);
    const ownerGrants = ownerIam.grants.filter(g => g.principal_id === ownerPrincipal.id);
    assert.strictEqual(ownerGrants.length, 1, `expected exactly one owner grant: ${JSON.stringify(ownerGrants)}`);
    assert.strictEqual(ownerGrants[0].role_id, 'role:root', 'owner grant must be root');
    assert.strictEqual(ownerGrants[0].status, 'active');
    const ownerAuditCount = (ownerIam.audit_events || []).length;

    ownerChild.kill('SIGTERM');
    await new Promise(resolve => setTimeout(resolve, 500));
    spawnOwnerDaemon();
    await waitFor(() => httpStatus(`http://127.0.0.1:${ownerPort}/config`).then(s => s === 200), START_TIMEOUT_MS, 'owner daemon restart');
    const ownerIamAfter = JSON.parse(fs.readFileSync(ownerIamPath, 'utf8'));
    assert.strictEqual(
      ownerIamAfter.grants.filter(g => g.principal_id === ownerPrincipal.id).length,
      1,
      'restart with the same --owner duplicated the grant'
    );
    assert.strictEqual(
      (ownerIamAfter.audit_events || []).length,
      ownerAuditCount,
      'idempotent --owner restart grew the audit log'
    );
    console.log('PASS lease-owner-bootstrap root grant pinned once, restart idempotent');

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
