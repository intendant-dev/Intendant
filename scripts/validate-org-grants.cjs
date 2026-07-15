#!/usr/bin/env node
'use strict';

// Org-grant E2E (trust architecture phase 6, steps 1-4).
//
// Two daemons plus a hosted Connect service:
//   - daemon A (org daemon) holds the `acme` org root key and issues signed
//     grant documents;
//   - daemon B (member target) trusts the org key and is the daemon members
//     actually connect to over a trusted direct origin. Hosted Connect is
//     exercised only as authority-free account/route metadata.
//
// What is proven, in order:
//   1. join fold: pasting a document on an untrusting daemon stores it in
//      the browser and fails harmlessly;
//   2. trusted-local presentation: after the daemon trusts the org, its direct
//      API verifies and materializes the document while the dashboard remains
//      rooted in the trusted transport, not the browser key;
//   3. re-presentation is quiet: presenting the same document again neither
//      rewrites iam.json nor grows the audit log;
//   4. a tampered document is refused without breaking the connection;
//   5. local IAM wins: a locally revoked materialized grant is NOT
//      resurrected by a later direct re-presentation;
//   6. hosted Connect cannot carry the document on an offer: offer/ICE/close
//      are 403 and the historical `/app` redirects without a control client.
//   7. PRF fleet encryption still round-trips in the trusted direct dashboard;
//      the harness transfers the account-derived PRF value across origins.
//
// Usage:
//   PLAYWRIGHT_NODE_PATH=$PWD/node_modules node scripts/validate-org-grants.cjs \
//     [--daemon-binary target/debug/intendant] \
//     [--connect-binary target/debug/intendant-connect]

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');
const { assertHostedControlUnavailable } = require('./lib/connect-hosted-refusal.cjs');

const DEFAULT_CONNECT_PORT = 9887;
const DEFAULT_ORG_DAEMON_PORT = 8898;
const DEFAULT_MEMBER_DAEMON_PORT = 8899;
const DEFAULT_MEMBER_DAEMON_ID = 'org-grant-e2e-member';
const DEFAULT_CONNECT_TOKEN = 'org-grant-e2e-token';
const START_TIMEOUT_MS = 45000;
const CONNECT_TIMEOUT_MS = 45000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    daemonBinary: path.join(repoRoot, 'target', 'debug', 'intendant'),
    connectBinary: path.join(repoRoot, 'target', 'debug', 'intendant-connect'),
    connectPort: DEFAULT_CONNECT_PORT,
    orgDaemonPort: DEFAULT_ORG_DAEMON_PORT,
    memberDaemonPort: DEFAULT_MEMBER_DAEMON_PORT,
    memberDaemonId: DEFAULT_MEMBER_DAEMON_ID,
    connectToken: DEFAULT_CONNECT_TOKEN,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-port') out.connectPort = Number(argv[++i]);
    else if (arg === '--org-daemon-port') out.orgDaemonPort = Number(argv[++i]);
    else if (arg === '--member-daemon-port') out.memberDaemonPort = Number(argv[++i]);
    else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/validate-org-grants.cjs [--daemon-binary <path>] [--connect-binary <path>]');
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return out;
}

async function fetchJson(url, options = {}) {
  const resp = await fetch(url, options);
  const body = await resp.json().catch(() => ({}));
  return { status: resp.status, body };
}

async function postJson(url, payload) {
  return fetchJson(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(payload),
  });
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

function memberIamPath(homeDir) {
  return path.join(homeDir, '.intendant', 'access-certs', 'iam.json');
}

function readMemberIam(homeDir) {
  return JSON.parse(fs.readFileSync(memberIamPath(homeDir), 'utf8'));
}

function orgAuditCount(iam) {
  return (iam.audit_events || []).filter(e => e.action === 'materialize_org_grant').length;
}

function orgGrants(iam) {
  return (iam.grants || []).filter(g => String(g.source || '') === 'org:acme');
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

async function dashboardStatus(page) {
  return page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
}

// Wait past channel-open until the daemon's first status frame lands —
// grantKind is null until then.
async function waitForBoundConnection(page, label) {
  return waitFor(async () => {
    const status = await dashboardStatus(page);
    if (status?.connected && status?.verifiedBinding?.ok && status?.grantKind) {
      return status;
    }
    return null;
  }, CONNECT_TIMEOUT_MS, label);
}

async function reloadPage(page) {
  await page.reload({ timeout: START_TIMEOUT_MS }).catch(() => {});
  await page.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
}

async function main() {
  const options = parseArgs(process.argv);
  for (const binary of [options.daemonBinary, options.connectBinary]) {
    if (!fs.existsSync(binary)) {
      throw new Error(`missing binary ${binary}; run cargo build --bin intendant --bin intendant-connect`);
    }
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-org-grants-'));
  const orgHome = path.join(tmp, 'org-home');
  const memberHome = path.join(tmp, 'member-home');
  fs.mkdirSync(orgHome, { recursive: true });
  fs.mkdirSync(memberHome, { recursive: true });

  const orgApi = `http://127.0.0.1:${options.orgDaemonPort}`;
  const memberOrigin = `http://127.0.0.1:${options.memberDaemonPort}`;
  const connectOrigin = `http://localhost:${options.connectPort}`;
  const connectApi = `http://127.0.0.1:${options.connectPort}`;

  const logs = { connect: [], org: [], member: [] };
  const children = [];
  let browser = null;

  function spawnLogged(command, args, spawnOptions, sink) {
    const child = spawn(command, args, spawnOptions);
    children.push(child);
    child.stdout?.on('data', chunk => sink.push(String(chunk)));
    child.stderr?.on('data', chunk => sink.push(String(chunk)));
    child.once('error', err => sink.push(String(err && err.message || err)));
    return child;
  }

  try {
    // ── Org identity on daemon A ──
    const init = spawnSync(options.daemonBinary, ['org', 'init', 'acme'], {
      cwd: tmp,
      env: { ...process.env, HOME: orgHome },
      encoding: 'utf8',
    });
    assert.strictEqual(init.status, 0, `org init failed: ${init.stderr || init.stdout}`);
    const rootKey = (init.stdout.match(/org root key: (\S+)/) || [])[1];
    assert(rootKey, `org init did not print a root key: ${init.stdout}`);

    // ── Services ──
    spawnLogged(options.connectBinary, [
      '--listen', `127.0.0.1:${options.connectPort}`,
      '--origin', connectOrigin,
      '--rp-id', 'localhost',
      '--static-root', path.join(options.repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', options.connectToken,
    ], { cwd: options.repoRoot, stdio: ['ignore', 'pipe', 'pipe'] }, logs.connect);

    spawnLogged(options.daemonBinary, [
      '--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(options.orgDaemonPort),
    ], {
      cwd: tmp,
      env: { ...process.env, HOME: orgHome },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.org);

    spawnLogged(options.daemonBinary, [
      '--no-tui', '--no-tls', '--bind', '127.0.0.1', '--web', String(options.memberDaemonPort),
    ], {
      cwd: tmp,
      env: {
        ...process.env,
        HOME: memberHome,
        INTENDANT_CONNECT_RENDEZVOUS_URL: connectApi,
        INTENDANT_CONNECT_DAEMON_ID: options.memberDaemonId,
        INTENDANT_CONNECT_TOKEN: options.connectToken,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, logs.member);

    await waitFor(() => httpStatus(`${connectApi}/healthz`).then(s => s === 200), START_TIMEOUT_MS, 'connect health');
    await waitFor(() => httpStatus(`${orgApi}/config`).then(s => s === 200), START_TIMEOUT_MS, 'org daemon readiness');
    await waitFor(() => httpStatus(`${memberOrigin}/config`).then(s => s === 200), START_TIMEOUT_MS, 'member daemon readiness');

    browser = await launchBrowser({ headless: true });

    // ══ Scenario 1: trusted-local org presentation + transport ══
    const page = await browser.newPage();
    await page.goto(`${memberOrigin}/`, { waitUntil: 'domcontentloaded', timeout: CONNECT_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => localStorage.setItem('intendant_dashboard_transport', 'webrtc-control'));
    await reloadPage(page);

    // Before any org grant, loopback + a loopback Host header enters the
    // explicit trusted-local root lane. The browser key is not what grants
    // this authority.
    const rootStatus = await waitForBoundConnection(page, 'local dashboard-control before org grant');
    assert.strictEqual(rootStatus.grantKind, 'trusted-local', `expected trusted-local root lane: ${JSON.stringify(rootStatus)}`);
    assert.strictEqual(rootStatus.grantLabel, 'trusted-local', `unexpected trusted-local label: ${JSON.stringify(rootStatus)}`);
    assert.strictEqual(String(rootStatus.accessPrincipal?.role_id || ''), 'role:root', `trusted-local lane was not root: ${JSON.stringify(rootStatus.accessPrincipal)}`);

    const fingerprintLocal = await page.evaluate(() =>
      window.intendantDashboardControl._debugClientIdentity().then(i => i && i.fingerprint));
    assert(fingerprintLocal, 'member browser did not mint a client identity key');

    // Org daemon issues a document for the member's browser key.
    const issued = await postJson(`${orgApi}/api/access/org-grants/issue`, {
      handle: 'acme',
      client_key_fingerprint: fingerprintLocal,
      label: 'Member Local',
      role_id: 'role:session-reader',
      targets: ['*'],
      ttl_ms: 60 * 60 * 1000,
    });
    assert.strictEqual(issued.status, 200, `issue failed: ${JSON.stringify(issued.body)}`);
    const doc = issued.body.document;
    assert(doc && doc.sig, `issue returned no document: ${JSON.stringify(issued.body)}`);

    // Join fold on a daemon that does NOT trust the org yet: presentation
    // fails, but the document is kept for automatic presentation.
    const joinStatusText = await page.evaluate(async docJson => {
      document.getElementById('access-org-join-doc').value = docJson;
      document.getElementById('access-org-join-btn').click();
      for (let i = 0; i < 100; i += 1) {
        const text = document.getElementById('access-org-join-status').textContent;
        if (text && text !== 'Presenting…') return text;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      return document.getElementById('access-org-join-status').textContent;
    }, JSON.stringify(doc));
    assert(/does not trust org acme/.test(joinStatusText), `expected untrusted-org refusal: ${joinStatusText}`);
    assert(/kept in this browser/.test(joinStatusText), `expected kept-for-later note: ${joinStatusText}`);
    const storedDoc = await page.evaluate(() => JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{}').acme || null);
    assert(storedDoc && storedDoc.grant_id === doc.grant_id, 'join fold did not store the document');

    // Root trusts the org on the member daemon (loopback, no TLS = root).
    const trusted = await postJson(`${memberOrigin}/api/access/orgs/trust`, { handle: 'acme', root_key: rootKey });
    assert.strictEqual(trusted.status, 200, `trust failed: ${JSON.stringify(trusted.body)}`);

    // Present through the daemon's trusted-local API. This is the actual
    // authority-bearing path: signature verification and materialization
    // happen on the daemon, not through hosted or ambient browser provenance.
    const presentedLocal = await postJson(`${memberOrigin}/api/access/org-grants`, doc);
    assert.strictEqual(presentedLocal.status, 200, `trusted-local presentation failed: ${JSON.stringify(presentedLocal.body)}`);

    // The dashboard transport stays trusted-local root. The narrower org
    // document is materialized for later direct/mTLS use but does not replace
    // the trusted anchor as this session's authenticator.
    await reloadPage(page);
    const materializedStatus = await waitForBoundConnection(page, 'trusted-local connection after org materialization');
    assert.strictEqual(materializedStatus.signalingMode, 'local-http', `expected local signaling: ${JSON.stringify(materializedStatus)}`);
    assert.strictEqual(materializedStatus.grantKind, 'trusted-local', `trusted anchor was replaced by a browser grant: ${JSON.stringify(materializedStatus)}`);
    assert.strictEqual(materializedStatus.grantLabel, 'trusted-local', `unexpected trusted-local label: ${JSON.stringify(materializedStatus)}`);
    assert.strictEqual(String(materializedStatus.accessPrincipal?.role_id || ''), 'role:root', `trusted-local connection was not root: ${JSON.stringify(materializedStatus.accessPrincipal)}`);

    let iam = readMemberIam(memberHome);
    const materialized = orgGrants(iam).find(g => g.id === `grant:org:acme:${doc.grant_id}`);
    assert(materialized, `materialized grant missing from iam.json: ${JSON.stringify(orgGrants(iam))}`);
    assert.strictEqual(materialized.status, 'active');
    assert.strictEqual(materialized.role_id, 'role:session-reader');
    assert.strictEqual(materialized.expires_at_unix_ms, doc.expires_at_unix_ms);
    const auditAfterFirst = orgAuditCount(iam);
    assert(auditAfterFirst >= 1, 'no materialize audit event recorded');

    // Identical direct re-presentation is a quiet no-op.
    const presentedAgain = await postJson(`${memberOrigin}/api/access/org-grants`, doc);
    assert.strictEqual(presentedAgain.status, 200, `idempotent presentation failed: ${JSON.stringify(presentedAgain.body)}`);
    await reloadPage(page);
    const reconnected = await waitForBoundConnection(page, 'trusted-local reconnect');
    assert.strictEqual(reconnected.grantKind, 'trusted-local', `reconnect left trusted-local lane: ${JSON.stringify(reconnected)}`);
    iam = readMemberIam(memberHome);
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'idempotent re-presentation grew the audit log');
    assert.strictEqual(orgGrants(iam).length, 1, 'idempotent re-presentation duplicated the grant');

    // Tampered document: the trusted direct API refuses it without breaking
    // the independently rooted dashboard connection.
    const tamperedDoc = JSON.parse(JSON.stringify(doc));
    tamperedDoc.role_id = 'role:root';
    const tamperedPresentation = await postJson(`${memberOrigin}/api/access/org-grants`, tamperedDoc);
    assert.strictEqual(tamperedPresentation.status, 400, `tampered document was accepted: ${JSON.stringify(tamperedPresentation.body)}`);
    await reloadPage(page);
    const afterTamper = await waitForBoundConnection(page, 'connection despite tampered document');
    assert.strictEqual(afterTamper.grantKind, 'trusted-local', `tampered document changed the trusted transport: ${JSON.stringify(afterTamper)}`);
    assert.strictEqual(String(afterTamper.accessPrincipal?.role_id || ''), 'role:root', 'tampered document changed the trusted-local role');
    iam = readMemberIam(memberHome);
    assert(!orgGrants(iam).some(g => g.role_id === 'role:root'), 'tampered document materialized');
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'tampered document touched the audit log');

    // Local IAM wins: revoke the materialized grant on the daemon, then
    // directly re-present the honest signed document. The daemon must refuse
    // it and the grant must stay revoked. The trusted-local session
    // remains independently rooted; revoking the org grant changes only the
    // materialized remote-member authority asserted in daemon-side state.
    iam = readMemberIam(memberHome);
    for (const grant of iam.grants) {
      if (grant.id === `grant:org:acme:${doc.grant_id}`) {
        grant.status = 'revoked';
        grant.revoked_at_unix_ms = Date.now();
      }
    }
    fs.writeFileSync(memberIamPath(memberHome), `${JSON.stringify(iam, null, 2)}\n`);
    const revokedPresentation = await postJson(`${memberOrigin}/api/access/org-grants`, doc);
    assert.strictEqual(revokedPresentation.status, 400, `locally revoked document was accepted: ${JSON.stringify(revokedPresentation.body)}`);
    assert(/revoked locally/.test(String(revokedPresentation.body.error)), `unexpected local-revocation refusal: ${JSON.stringify(revokedPresentation.body)}`);
    iam = readMemberIam(memberHome);
    const revokedGrant = iam.grants.find(g => g.id === `grant:org:acme:${doc.grant_id}`);
    assert.strictEqual(revokedGrant.status, 'revoked', 'local revocation did not survive re-presentation');
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'refused re-presentation touched the audit log');
    await page.close();

    // ══ Scenario 2: hosted Connect rendezvous path ══
    const hosted = await browser.newPage();
    await addVirtualAuthenticator(browser, hosted);

    // Claim the member daemon under a fresh Connect account.
    const claimCode = await waitFor(() => {
      const all = `${logs.connect.join('')}\n${logs.member.join('')}`;
      const urlMatch = all.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = all.match(/one-time claim code ([a-z0-9-]+)/i);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim code');
    const iamBeforeRouteClaim = readMemberIam(memberHome);
    const claimInvariantBefore = JSON.stringify({
      principals: iamBeforeRouteClaim.principals || [],
      grants: iamBeforeRouteClaim.grants || [],
      role_ceilings: iamBeforeRouteClaim.role_ceilings || {},
    });
    await hosted.goto(`${connectOrigin}/connect#claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await hosted.evaluate(() => {
      document.getElementById('account').value = `org-member-${Date.now()}`;
    });
    await hosted.locator('#register').click();
    await hosted.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    await hosted.locator('#claim').click();
    await hosted.waitForFunction(() => document.getElementById('claim-status').textContent.includes('No machine access was granted'), { timeout: START_TIMEOUT_MS });
    const iamAfterRouteClaim = readMemberIam(memberHome);
    assert.strictEqual(JSON.stringify({
      principals: iamAfterRouteClaim.principals || [],
      grants: iamAfterRouteClaim.grants || [],
      role_ceilings: iamAfterRouteClaim.role_ceilings || {},
    }), claimInvariantBefore, 'route-only claim mutated daemon IAM');

    // Phase 5 follow-on: the passkey ceremony evaluated the PRF extension,
    // so this tab holds the fleet-encryption secret.
    const prfSecret = await hosted.evaluate(() => sessionStorage.getItem('intendant_fleet_prf_v1'));
    assert(prfSecret, 'PRF secret was not captured at registration');

    const retiredAttempts = await assertHostedControlUnavailable(
      hosted,
      connectOrigin,
      options.memberDaemonId,
      START_TIMEOUT_MS
    );
    assert.deepStrictEqual(
      retiredAttempts.map(attempt => attempt.status),
      [403, 403, 403],
      'hosted signaling was not retired'
    );

    // Exercise a second org subject through the trusted direct presentation
    // API. It deliberately never rides a hosted offer.
    const fingerprintDirect = 'bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd6677';
    const issuedDirect = await postJson(`${orgApi}/api/access/org-grants/issue`, {
      handle: 'acme',
      client_key_fingerprint: fingerprintDirect,
      label: 'Member Direct',
      role_id: 'role:session-reader',
      targets: [options.memberDaemonId],
      ttl_ms: 60 * 60 * 1000,
    });
    assert.strictEqual(issuedDirect.status, 200, `direct issue failed: ${JSON.stringify(issuedDirect.body)}`);
    const presentedDirect = await postJson(
      `${memberOrigin}/api/access/org-grants`,
      issuedDirect.body.document
    );
    assert.strictEqual(presentedDirect.status, 200, `trusted presentation failed: ${JSON.stringify(presentedDirect.body)}`);

    iam = readMemberIam(memberHome);
    const directGrant = orgGrants(iam).find(g => g.id === `grant:org:acme:${issuedDirect.body.document.grant_id}`);
    assert(directGrant, `trusted-direct grant missing: ${JSON.stringify(orgGrants(iam))}`);
    assert.strictEqual(directGrant.status, 'active');

    // ══ Scenario 3: org revocation list + renewal (phase 6 step 5) ══
    // Renewal first, while nothing is revoked: same grant_id, fresh window.
    const renewedLocal = await postJson(`${orgApi}/api/access/org-grants/renew`, doc);
    assert.strictEqual(renewedLocal.status, 200, `renew failed: ${JSON.stringify(renewedLocal.body)}`);
    assert.strictEqual(renewedLocal.body.document.grant_id, doc.grant_id, 'renewal changed grant_id');
    assert(renewedLocal.body.document.expires_at_unix_ms > doc.expires_at_unix_ms, 'renewal did not extend expiry');
    assert.strictEqual(
      renewedLocal.body.document.expires_at_unix_ms - renewedLocal.body.document.issued_at_unix_ms,
      doc.expires_at_unix_ms - doc.issued_at_unix_ms,
      'renewal changed the lifetime span'
    );

    // The org revokes the direct member by subject fingerprint.
    const revokedMember = await postJson(`${orgApi}/api/access/org-grants/revoke-member`, {
      handle: 'acme',
      subject: fingerprintDirect,
    });
    assert.strictEqual(revokedMember.status, 200, `revoke-member failed: ${JSON.stringify(revokedMember.body)}`);
    assert.strictEqual(revokedMember.body.orl.seq, 1, `expected first revocation at seq 1: ${JSON.stringify(revokedMember.body.orl)}`);

    // The list is served publicly by the org daemon and matches.
    const served = await fetchJson(`${orgApi}/api/access/orgs/acme/revocations`);
    assert.strictEqual(served.status, 200, `orl fetch failed: ${JSON.stringify(served.body)}`);
    assert.deepStrictEqual(served.body.orl, revokedMember.body.orl, 'served list differs from the revoke response');

    // Renewal of the revoked member's document is refused by the org.
    const renewRevoked = await postJson(`${orgApi}/api/access/org-grants/renew`, issuedDirect.body.document);
    assert.strictEqual(renewRevoked.status, 400, `revoked renewal unexpectedly succeeded: ${JSON.stringify(renewRevoked.body)}`);
    assert(/revoked/.test(String(renewRevoked.body.error)), `expected revoked-refusal: ${JSON.stringify(renewRevoked.body)}`);

    // The org daemon's admin publishes the list to the rendezvous
    // bulletin board (zero authority: signature-checked, rollback-proof).
    const published = await postJson(`${connectApi}/api/orgs/revocations/publish`, served.body.orl);
    assert.strictEqual(published.status, 200, `orl publish failed: ${JSON.stringify(published.body)}`);
    assert.strictEqual(published.body.stored, true);
    const board = await fetchJson(`${connectApi}/api/orgs/revocations?handle=acme&root_key=${encodeURIComponent(rootKey)}`);
    assert.strictEqual(board.status, 200, `orl board fetch failed: ${JSON.stringify(board.body)}`);
    assert.deepStrictEqual(board.body.orl, served.body.orl, 'board serves a different list');
    const stale = await postJson(`${connectApi}/api/orgs/revocations/publish`, revokedMember.body.orl);
    assert.strictEqual(stale.body.stored, false, `re-publishing same seq should be a no-op: ${JSON.stringify(stale.body)}`);

    // A member browser visiting the daemon carries the published list to
    // it automatically — no explicit apply call anywhere.
    const courier = await browser.newPage();
    await courier.goto(`${memberOrigin}/`, { waitUntil: 'domcontentloaded', timeout: CONNECT_TIMEOUT_MS });
    await waitFor(() => {
      const iamNow = readMemberIam(memberHome);
      const entry = (iamNow.trusted_orgs || []).find(o => o.handle === 'acme');
      return entry && entry.last_orl_seq === 1;
    }, CONNECT_TIMEOUT_MS, 'courier auto-apply of published revocations');
    await courier.close();

    // Re-applying the same list manually is an idempotent no-op.
    const applied = await postJson(`${memberOrigin}/api/access/orgs/revocations/apply`, served.body.orl);
    assert.strictEqual(applied.status, 200, `orl apply failed: ${JSON.stringify(applied.body)}`);
    assert.strictEqual(applied.body.applied.changed, false, `courier should have applied seq 1 already: ${JSON.stringify(applied.body.applied)}`);
    iam = readMemberIam(memberHome);
    assert.strictEqual(
      iam.grants.find(g => g.id === directGrant.id).status,
      'revoked',
      'ORL apply did not revoke the trusted-direct grant'
    );
    const trustedEntry = (iam.trusted_orgs || []).find(o => o.handle === 'acme');
    assert.strictEqual(trustedEntry.last_orl_seq, 1, 'seq not persisted');
    assert(trustedEntry.orl_revoked_subjects.includes(fingerprintDirect), 'subject not persisted');

    // Re-presenting the still-signed document over the trusted direct API is
    // refused by the persisted list; no hosted presentation path exists.
    const rePresentRevoked = await postJson(
      `${memberOrigin}/api/access/org-grants`,
      issuedDirect.body.document
    );
    assert.strictEqual(rePresentRevoked.status, 400, `revoked document re-presented: ${JSON.stringify(rePresentRevoked.body)}`);
    assert(/revocation|revoked/.test(String(rePresentRevoked.body.error)), `expected ORL refusal: ${JSON.stringify(rePresentRevoked.body)}`);
    iam = readMemberIam(memberHome);
    assert.strictEqual(iam.grants.find(g => g.id === directGrant.id).status, 'revoked', 're-presentation resurrected an ORL-revoked grant');

    // ══ Scenario 4: peer-daemon subjects (phase 6 step 6a) ══
    const peerFp = 'aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66';
    const issuedPeer = await postJson(`${orgApi}/api/access/org-grants/issue`, {
      handle: 'acme',
      peer_fingerprint: peerFp,
      label: 'Build daemon',
      role_id: 'peer:session-reader',
      targets: ['*'],
      ttl_ms: 60 * 60 * 1000,
    });
    assert.strictEqual(issuedPeer.status, 200, `peer issue failed: ${JSON.stringify(issuedPeer.body)}`);
    const peerDoc = issuedPeer.body.document;

    // Fail closed: the member daemon trusts the org for humans, but has
    // granted it no peer authority — the document is refused.
    const failClosed = await postJson(`${memberOrigin}/api/access/org-grants`, peerDoc);
    assert.strictEqual(failClosed.status, 400, `peer doc accepted without a peer cap: ${JSON.stringify(failClosed.body)}`);
    assert(/no peer authority/.test(String(failClosed.body.error)), `expected fail-closed refusal: ${JSON.stringify(failClosed.body)}`);

    // The owner raises the peer cap (re-trust keeps applied ORL state).
    const retrust = await postJson(`${memberOrigin}/api/access/orgs/trust`, {
      handle: 'acme',
      root_key: rootKey,
      max_peer_profile: 'session-reader',
    });
    assert.strictEqual(retrust.status, 200, `re-trust failed: ${JSON.stringify(retrust.body)}`);

    const presentedPeer = await postJson(`${memberOrigin}/api/access/org-grants`, peerDoc);
    assert.strictEqual(presentedPeer.status, 200, `peer present failed: ${JSON.stringify(presentedPeer.body)}`);
    assert.strictEqual(presentedPeer.body.peer_identity.profile, 'session-reader');
    const peerRecordPath = path.join(memberHome, '.intendant', 'access-certs', 'peer-access-identities', `${peerFp}.json`);
    let peerRecord = JSON.parse(fs.readFileSync(peerRecordPath, 'utf8'));
    assert.strictEqual(peerRecord.status, 'approved');
    assert.strictEqual(peerRecord.source, 'org:acme');
    assert.strictEqual(peerRecord.org_grant_id, peerDoc.grant_id);
    assert(Number(peerRecord.expires_at_unix) > Date.now() / 1000, 'peer record missing expiry');

    // ORL subject revocation sweeps the peer identity store too.
    const revokePeer = await postJson(`${orgApi}/api/access/org-grants/revoke-member`, {
      handle: 'acme',
      subject: peerFp,
    });
    assert.strictEqual(revokePeer.status, 200, `peer revoke-member failed: ${JSON.stringify(revokePeer.body)}`);
    const appliedPeer = await postJson(`${memberOrigin}/api/access/orgs/revocations/apply`, revokePeer.body.orl);
    assert.strictEqual(appliedPeer.status, 200, `peer orl apply failed: ${JSON.stringify(appliedPeer.body)}`);
    assert.strictEqual(appliedPeer.body.applied.revoked_peer_identities, 1, `peer identity not swept: ${JSON.stringify(appliedPeer.body.applied)}`);
    peerRecord = JSON.parse(fs.readFileSync(peerRecordPath, 'utf8'));
    assert.strictEqual(peerRecord.status, 'revoked', 'peer record not revoked by ORL');
    const rePresent = await postJson(`${memberOrigin}/api/access/org-grants`, peerDoc);
    assert.strictEqual(rePresent.status, 400, `revoked peer doc re-materialized: ${JSON.stringify(rePresent.body)}`);

    // ══ Scenario 5: issuer-key delegation (phase 6 step 6b) ══
    // The member daemon becomes a deputy issuer; the org root delegates.
    const issuerInit = await postJson(`${memberOrigin}/api/access/org-grants/issuers/init`, { handle: 'acme' });
    assert.strictEqual(issuerInit.status, 200, `issuer init failed: ${JSON.stringify(issuerInit.body)}`);
    const issuerKey = issuerInit.body.issuer_key;
    const delegated = await postJson(`${orgApi}/api/access/org-grants/issuers/delegate`, {
      handle: 'acme',
      issuer_key: issuerKey,
      label: 'E2E deputy',
    });
    assert.strictEqual(delegated.status, 200, `delegate failed: ${JSON.stringify(delegated.body)}`);
    const installed = await postJson(`${memberOrigin}/api/access/org-grants/issuers/install`, {
      handle: 'acme',
      certificate: delegated.body.certificate,
    });
    assert.strictEqual(installed.status, 200, `install failed: ${JSON.stringify(installed.body)}`);

    // The deputy (holding no root key) issues a chained document; the
    // member daemon materializes it and records the issuer.
    const deputyIssued = await postJson(`${memberOrigin}/api/access/org-grants/issue`, {
      handle: 'acme',
      client_key_fingerprint: 'deputy-signed-member',
      label: 'Deputy Member',
      role_id: 'role:session-reader',
      targets: ['*'],
      ttl_ms: 60 * 60 * 1000,
    });
    assert.strictEqual(deputyIssued.status, 200, `deputy issue failed: ${JSON.stringify(deputyIssued.body)}`);
    assert.strictEqual((deputyIssued.body.document.chain || []).length, 1, 'deputy document missing chain');
    assert.strictEqual(deputyIssued.body.org_root_key, rootKey, 'deputy document names wrong root');
    const presentedDeputy = await postJson(`${memberOrigin}/api/access/org-grants`, deputyIssued.body.document);
    assert.strictEqual(presentedDeputy.status, 200, `deputy doc present failed: ${JSON.stringify(presentedDeputy.body)}`);
    assert.strictEqual(presentedDeputy.body.grant.issued_via, issuerKey, 'issued_via not recorded');

    // Revoking the issuer key sweeps everything it signed and blocks
    // both new materialization and renewal.
    const revokeIssuer = await postJson(`${orgApi}/api/access/org-grants/revoke-member`, {
      handle: 'acme',
      issuer_key: issuerKey,
    });
    assert.strictEqual(revokeIssuer.status, 200, `issuer revoke failed: ${JSON.stringify(revokeIssuer.body)}`);
    const appliedIssuer = await postJson(`${memberOrigin}/api/access/orgs/revocations/apply`, revokeIssuer.body.orl);
    assert.strictEqual(appliedIssuer.status, 200, `issuer orl apply failed: ${JSON.stringify(appliedIssuer.body)}`);
    assert(appliedIssuer.body.applied.revoked_grants >= 1, `issuer sweep missed the grant: ${JSON.stringify(appliedIssuer.body.applied)}`);
    const rePresentDeputy = await postJson(`${memberOrigin}/api/access/org-grants`, deputyIssued.body.document);
    assert.strictEqual(rePresentDeputy.status, 400, `revoked-issuer doc re-materialized: ${JSON.stringify(rePresentDeputy.body)}`);
    assert(/issuer key/.test(String(rePresentDeputy.body.error)), `expected issuer refusal: ${JSON.stringify(rePresentDeputy.body)}`);
    const renewDeputy = await postJson(`${orgApi}/api/access/org-grants/renew`, deputyIssued.body.document);
    assert.strictEqual(renewDeputy.status, 400, `revoked-issuer doc renewed: ${JSON.stringify(renewDeputy.body)}`);

    // ══ Scenario 6: PRF-encrypted fleet data on a trusted direct origin ══
    // Connect's account ceremony derived the PRF value, but Connect serves no
    // dashboard code. Transfer that test value into a trusted daemon-origin
    // tab and exercise the same fleet crypto helpers there.
    const fleetPage = await browser.newPage();
    await fleetPage.goto(`${memberOrigin}/`, { waitUntil: 'domcontentloaded', timeout: CONNECT_TIMEOUT_MS });
    await fleetPage.evaluate(secret => {
      sessionStorage.setItem('intendant_fleet_prf_v1', secret);
      localStorage.setItem('intendant_dashboard_transport', 'webrtc-control');
    }, prfSecret);
    await reloadPage(fleetPage);
    const fleetStatus = await waitForBoundConnection(fleetPage, 'trusted-direct fleet crypto dashboard');
    assert.strictEqual(fleetStatus.grantKind, 'trusted-local', `fleet crypto tab was not trusted-local: ${JSON.stringify(fleetStatus)}`);
    assert.strictEqual(fleetStatus.signalingMode, 'local-http', `fleet crypto tab was not direct: ${JSON.stringify(fleetStatus)}`);
    const fleetProbe = await fleetPage.evaluate(async () => {
      const record = {
        id: 'e2e-private-daemon',
        host_id: 'e2e-private-daemon',
        label: 'Private daemon',
        url: 'https://10.9.8.7:8765/',
        ws_url: 'wss://10.9.8.7:8765/ws',
        browser_tcp_via_url: '',
        connect_daemon_id: 'e2e-private-daemon',
        connect_signaling_base: window.location.origin,
      };
      const encrypted = await window.intendantDashboardControl._debugFleetEncryptRecord(record);
      const decrypted = await window.intendantDashboardControl._debugFleetDecryptRecord(encrypted);
      return {
        encHasCiphertext: String(encrypted.enc_fields || '').startsWith('enc1:'),
        encBlankedUrl: encrypted.url === '' && encrypted.ws_url === '',
        roundTripUrl: decrypted.url,
        roundTripWs: decrypted.ws_url,
        locked: decrypted.fleet_locked === true,
      };
    });
    await fleetPage.close();
    assert.strictEqual(fleetProbe.encHasCiphertext, true, `no ciphertext envelope: ${JSON.stringify(fleetProbe)}`);
    assert.strictEqual(fleetProbe.encBlankedUrl, true, `plaintext leaked beside envelope: ${JSON.stringify(fleetProbe)}`);
    assert.strictEqual(fleetProbe.roundTripUrl, 'https://10.9.8.7:8765/', `decrypt round-trip failed: ${JSON.stringify(fleetProbe)}`);
    assert.strictEqual(fleetProbe.roundTripWs, 'wss://10.9.8.7:8765/ws', `ws decrypt round-trip failed: ${JSON.stringify(fleetProbe)}`);
    assert.strictEqual(fleetProbe.locked, false, `record locked despite key: ${JSON.stringify(fleetProbe)}`);

    console.log(JSON.stringify({
      ok: true,
      fleet_encryption: { prf: true, round_trip: true, origin: 'trusted-direct' },
      issuer: { key: issuerKey, revoked: true },
      peer_subject: { fingerprint: peerFp, final_status: peerRecord.status },
      org_root_key: rootKey,
      local_fingerprint: fingerprintLocal,
      direct_subject_fingerprint: fingerprintDirect,
      hosted_refusal: 'hosted control endpoints retired (403)',
      orl_seq: revokedMember.body.orl.seq,
      materialized_grants: orgGrants(iam).map(g => ({ id: g.id, status: g.status, role: g.role_id })),
    }, null, 2));
  } catch (err) {
    const tail = sink => sink.slice(-20).join('').split('\n').slice(-30).join('\n');
    console.error('--- connect log tail ---\n' + tail(logs.connect));
    console.error('--- org daemon log tail ---\n' + tail(logs.org));
    console.error('--- member daemon log tail ---\n' + tail(logs.member));
    throw err;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children.reverse()) {
      if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    }
    await new Promise(resolve => setTimeout(resolve, 500));
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
