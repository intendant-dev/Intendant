#!/usr/bin/env node
'use strict';

// Org-grant E2E (trust architecture phase 6, steps 1-4).
//
// Two daemons plus a hosted Connect service:
//   - daemon A (org daemon) holds the `acme` org root key and issues signed
//     grant documents;
//   - daemon B (member target) trusts the org key and is the daemon members
//     actually connect to, over both its own origin (local offer path) and
//     hosted Connect (rendezvous offer path).
//
// What is proven, in order:
//   1. join fold: pasting a document on an untrusting daemon stores it in
//      the browser and fails harmlessly;
//   2. offer ride-along (local path): after the daemon trusts the org, a
//      plain reconnect materializes the stored document and binds the
//      session to the scoped member principal — no explicit present call;
//   3. re-presentation is quiet: reconnecting again neither rewrites
//      iam.json nor grows the audit log;
//   4. a tampered document is refused without breaking the connection;
//   5. local IAM wins: a locally revoked materialized grant is NOT
//      resurrected by the automatic re-presentation;
//   6. offer ride-along (hosted rendezvous path): a Connect account with no
//      local IAM grant of its own connects scoped purely via the document
//      carried on the offer.
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

async function waitForScopedConnection(page, label) {
  return waitFor(async () => {
    const status = await dashboardStatus(page);
    if (status?.connected && status?.verifiedBinding?.ok && status?.grantKind === 'user-client') {
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

    // ══ Scenario 1: local offer path on the member daemon ══
    const page = await browser.newPage();
    const consoleMessages = [];
    page.on('console', msg => consoleMessages.push(msg.text()));
    await page.goto(`${memberOrigin}/`, { waitUntil: 'domcontentloaded', timeout: CONNECT_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
    await page.evaluate(() => localStorage.setItem('intendant_dashboard_transport', 'webrtc-control'));
    await reloadPage(page);

    // Before any org grant: verified key with no local IAM binding keeps
    // trusted-transport root on the local path.
    const rootStatus = await waitForBoundConnection(page, 'local dashboard-control before org grant');
    assert.strictEqual(rootStatus.grantKind, 'user-client-root', `expected ungranted key to keep local root: ${JSON.stringify(rootStatus)}`);

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

    // Reconnect: the offer carries the stored document, the daemon
    // materializes it, and the same offer resolves the scoped principal.
    await reloadPage(page);
    const scoped = await waitForScopedConnection(page, 'local ride-along scoped connection');
    assert.strictEqual(scoped.signalingMode, 'local-http', `expected local signaling: ${JSON.stringify(scoped)}`);
    assert.strictEqual(scoped.grantLabel, 'Member Local', `expected member principal label: ${JSON.stringify(scoped)}`);
    assert.strictEqual(String(scoped.accessPrincipal?.role_id || ''), 'role:session-reader', `expected session-reader role: ${JSON.stringify(scoped.accessPrincipal)}`);

    let iam = readMemberIam(memberHome);
    const materialized = orgGrants(iam).find(g => g.id === `grant:org:acme:${doc.grant_id}`);
    assert(materialized, `materialized grant missing from iam.json: ${JSON.stringify(orgGrants(iam))}`);
    assert.strictEqual(materialized.status, 'active');
    assert.strictEqual(materialized.role_id, 'role:session-reader');
    assert.strictEqual(materialized.expires_at_unix_ms, doc.expires_at_unix_ms);
    const auditAfterFirst = orgAuditCount(iam);
    assert(auditAfterFirst >= 1, 'no materialize audit event recorded');

    // Reconnect again: identical re-presentation is a quiet no-op.
    await reloadPage(page);
    await waitForScopedConnection(page, 'local ride-along reconnect');
    iam = readMemberIam(memberHome);
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'idempotent re-presentation grew the audit log');
    assert.strictEqual(orgGrants(iam).length, 1, 'idempotent re-presentation duplicated the grant');

    // Tampered document: refused without breaking the connection (the
    // already-materialized grant still resolves).
    await page.evaluate(() => {
      const map = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{}');
      map.acme.role_id = 'role:root';
      localStorage.setItem('intendant_org_grants_v1', JSON.stringify(map));
    });
    await reloadPage(page);
    const afterTamper = await waitForScopedConnection(page, 'connection despite tampered document');
    assert.strictEqual(String(afterTamper.accessPrincipal?.role_id || ''), 'role:session-reader', 'tampered document changed the bound role');
    iam = readMemberIam(memberHome);
    assert(!orgGrants(iam).some(g => g.role_id === 'role:root'), 'tampered document materialized');
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'tampered document touched the audit log');

    // Local IAM wins: revoke the materialized grant on the daemon, restore
    // the honest document in the browser, reconnect — the daemon must
    // refuse the re-presentation (surfaced in the answer and warned in the
    // console) and the grant must stay revoked. The session that follows
    // binds the now-ungranted principal, which is refused per-operation —
    // asserted here through the daemon-side state, not the client UI.
    await page.evaluate(docJson => {
      const map = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{}');
      map.acme = JSON.parse(docJson);
      localStorage.setItem('intendant_org_grants_v1', JSON.stringify(map));
    }, JSON.stringify(doc));
    iam = readMemberIam(memberHome);
    for (const grant of iam.grants) {
      if (grant.id === `grant:org:acme:${doc.grant_id}`) {
        grant.status = 'revoked';
        grant.revoked_at_unix_ms = Date.now();
      }
    }
    fs.writeFileSync(memberIamPath(memberHome), `${JSON.stringify(iam, null, 2)}\n`);
    consoleMessages.length = 0;
    await reloadPage(page);
    await waitFor(
      () => consoleMessages.some(m => /offer org grant not accepted/.test(m) && /revoked locally/.test(m)),
      CONNECT_TIMEOUT_MS,
      'daemon refusal of the revoked document'
    );
    iam = readMemberIam(memberHome);
    const revokedGrant = iam.grants.find(g => g.id === `grant:org:acme:${doc.grant_id}`);
    assert.strictEqual(revokedGrant.status, 'revoked', 'local revocation did not survive re-presentation');
    assert.strictEqual(orgAuditCount(iam), auditAfterFirst, 'refused re-presentation touched the audit log');
    await page.close();

    // ══ Scenario 2: hosted Connect rendezvous path ══
    const hosted = await browser.newPage();
    const hostedConsole = [];
    hosted.on('console', msg => hostedConsole.push(msg.text()));
    await addVirtualAuthenticator(browser, hosted);

    // Claim the member daemon under a fresh Connect account.
    const claimCode = await waitFor(() => {
      const all = `${logs.connect.join('')}\n${logs.member.join('')}`;
      const urlMatch = all.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = all.match(/claim this daemon with code ([^\s"'<>]+)/);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim code');
    await hosted.goto(`${connectOrigin}/connect?claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });
    await hosted.evaluate(() => {
      document.getElementById('account').value = `org-member-${Date.now()}`;
    });
    await hosted.locator('#register').click();
    await hosted.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), { timeout: START_TIMEOUT_MS });
    await hosted.locator('#claim').click();
    await hosted.waitForFunction(() => document.getElementById('claim-status').textContent.includes('Rendezvous route claimed'), { timeout: START_TIMEOUT_MS });

    // Without any document or account grant, the hosted offer is refused.
    await hosted.goto(`${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(options.memberDaemonId)}#activity`, { timeout: START_TIMEOUT_MS });
    await hosted.waitForFunction(() => Boolean(window.intendantDashboardControl), { timeout: START_TIMEOUT_MS });
    let refusal = null;
    try {
      refusal = await waitFor(async () => {
        const status = await dashboardStatus(hosted);
        if (status?.connected) throw new Error(`unexpectedly connected without a grant: ${JSON.stringify(status)}`);
        if (status?.lastError) return status;
        if (hostedConsole.some(m => /not authorized by this daemon/.test(m))) {
          return { lastError: 'not authorized by this daemon (from console)' };
        }
        return null;
      }, CONNECT_TIMEOUT_MS, 'hosted refusal without document');
    } catch (err) {
      const status = await dashboardStatus(hosted).catch(() => null);
      throw new Error(`${err.message}; status=${JSON.stringify(status)}; console tail=${JSON.stringify(hostedConsole.slice(-15))}`);
    }
    assert(/not authorized by this daemon/.test(String(refusal.lastError)), `expected IAM refusal: ${JSON.stringify(refusal.lastError)}`);

    // The hosted origin mints its own identity key; the org issues for it,
    // and the browser keeps the document (as the join fold would).
    const fingerprintHosted = await hosted.evaluate(() =>
      window.intendantDashboardControl._debugClientIdentity().then(i => i && i.fingerprint));
    assert(fingerprintHosted, 'hosted page did not mint a client identity key');
    assert.notStrictEqual(fingerprintHosted, fingerprintLocal, 'origin-scoped keys should differ');
    const issuedHosted = await postJson(`${orgApi}/api/access/org-grants/issue`, {
      handle: 'acme',
      client_key_fingerprint: fingerprintHosted,
      label: 'Member Hosted',
      role_id: 'role:session-reader',
      targets: [options.memberDaemonId],
      ttl_ms: 60 * 60 * 1000,
    });
    assert.strictEqual(issuedHosted.status, 200, `hosted issue failed: ${JSON.stringify(issuedHosted.body)}`);
    const storedHosted = await hosted.evaluate(
      docJson => window.intendantDashboardControl._debugOrgGrantStore(JSON.parse(docJson)),
      JSON.stringify(issuedHosted.body.document)
    );
    assert.strictEqual(storedHosted, true, 'hosted page did not store the document');

    // Reload: the rendezvous offer carries the document; the daemon
    // materializes it and the Connect session binds the scoped member
    // principal — the account itself still has no grant of its own.
    await reloadPage(hosted);
    const hostedScoped = await waitForScopedConnection(hosted, 'hosted ride-along scoped connection');
    assert.strictEqual(hostedScoped.signalingMode, 'connect-rendezvous', `expected rendezvous signaling: ${JSON.stringify(hostedScoped)}`);
    assert.strictEqual(hostedScoped.grantLabel, 'Member Hosted', `expected hosted member label: ${JSON.stringify(hostedScoped)}`);
    assert.strictEqual(String(hostedScoped.accessPrincipal?.role_id || ''), 'role:session-reader', `expected session-reader role: ${JSON.stringify(hostedScoped.accessPrincipal)}`);

    iam = readMemberIam(memberHome);
    const hostedGrant = orgGrants(iam).find(g => g.id === `grant:org:acme:${issuedHosted.body.document.grant_id}`);
    assert(hostedGrant, `hosted ride-along grant missing: ${JSON.stringify(orgGrants(iam))}`);
    assert.strictEqual(hostedGrant.status, 'active');

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

    // The org revokes the hosted member by subject fingerprint.
    const revokedMember = await postJson(`${orgApi}/api/access/org-grants/revoke-member`, {
      handle: 'acme',
      subject: fingerprintHosted,
    });
    assert.strictEqual(revokedMember.status, 200, `revoke-member failed: ${JSON.stringify(revokedMember.body)}`);
    assert.strictEqual(revokedMember.body.orl.seq, 1, `expected first revocation at seq 1: ${JSON.stringify(revokedMember.body.orl)}`);

    // The list is served publicly by the org daemon and matches.
    const served = await fetchJson(`${orgApi}/api/access/orgs/acme/revocations`);
    assert.strictEqual(served.status, 200, `orl fetch failed: ${JSON.stringify(served.body)}`);
    assert.deepStrictEqual(served.body.orl, revokedMember.body.orl, 'served list differs from the revoke response');

    // Renewal of the revoked member's document is refused by the org.
    const renewRevoked = await postJson(`${orgApi}/api/access/org-grants/renew`, issuedHosted.body.document);
    assert.strictEqual(renewRevoked.status, 400, `revoked renewal unexpectedly succeeded: ${JSON.stringify(renewRevoked.body)}`);
    assert(/revoked/.test(String(renewRevoked.body.error)), `expected revoked-refusal: ${JSON.stringify(renewRevoked.body)}`);

    // Anyone can carry the list to the member daemon; the signature and
    // monotonic seq make the courier irrelevant.
    const applied = await postJson(`${memberOrigin}/api/access/orgs/revocations/apply`, served.body.orl);
    assert.strictEqual(applied.status, 200, `orl apply failed: ${JSON.stringify(applied.body)}`);
    assert.strictEqual(applied.body.applied.changed, true);
    assert.strictEqual(applied.body.applied.revoked_grants, 1, `expected exactly the hosted grant revoked: ${JSON.stringify(applied.body.applied)}`);
    iam = readMemberIam(memberHome);
    assert.strictEqual(
      iam.grants.find(g => g.id === hostedGrant.id).status,
      'revoked',
      'ORL apply did not revoke the hosted grant'
    );
    const trustedEntry = (iam.trusted_orgs || []).find(o => o.handle === 'acme');
    assert.strictEqual(trustedEntry.last_orl_seq, 1, 'seq not persisted');
    assert(trustedEntry.orl_revoked_subjects.includes(fingerprintHosted), 'subject not persisted');

    // Re-applying the same seq is an idempotent no-op.
    const reapplied = await postJson(`${memberOrigin}/api/access/orgs/revocations/apply`, served.body.orl);
    assert.strictEqual(reapplied.body.applied.changed, false, `expected idempotent re-apply: ${JSON.stringify(reapplied.body)}`);

    // The revoked member reconnects with its still-signed document: the
    // ride-along is refused by the persisted list (daemon-side log), and
    // the revoked grant stays revoked.
    const memberLogMark = logs.member.length;
    await reloadPage(hosted);
    await waitFor(
      () => logs.member.slice(memberLogMark).join('').includes('offer org grant not accepted')
        && logs.member.slice(memberLogMark).join('').includes('revocation list'),
      CONNECT_TIMEOUT_MS,
      'daemon refusal of the ORL-revoked document'
    );
    iam = readMemberIam(memberHome);
    assert.strictEqual(iam.grants.find(g => g.id === hostedGrant.id).status, 'revoked', 'reconnect resurrected an ORL-revoked grant');

    console.log(JSON.stringify({
      ok: true,
      org_root_key: rootKey,
      local_fingerprint: fingerprintLocal,
      hosted_fingerprint: fingerprintHosted,
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
