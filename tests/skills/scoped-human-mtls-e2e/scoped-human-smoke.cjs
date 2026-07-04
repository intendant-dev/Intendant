'use strict';
// Scoped-human mTLS smoke: three Playwright request contexts against one
// HTTPS daemon — the setup-minted owner cert (root), a cert bound to
// role:files-write with fs roots, and a cert bound to role:operator.
// Proves TLS fingerprint -> IAM grant -> scoped enforcement end to end.
const fs = require('fs');
const { request } = require('playwright');

const RIG = process.env.RIG || '/tmp/scoped-human-rig';
const PORT = process.env.PORT || '18820';
const ORIGIN = `https://127.0.0.1:${PORT}`;
const CERTS = `${RIG}/home/.intendant/access-certs`;
const FILES = `${RIG}/files`;
const OUTSIDE = `${RIG}/outside`;

const steps = [];
function step(name) { steps.push(name); console.log('STEP ok:', name); }
function assert(cond, why) { if (!cond) throw new Error('assert failed: ' + why); }

function ctxFor(certPath, keyPath) {
  return request.newContext({
    ignoreHTTPSErrors: true,
    clientCertificates: [{ origin: ORIGIN, certPath, keyPath }],
  });
}

(async () => {
  let root, scoped, operator;
  try {
    root = await ctxFor(`${CERTS}/client.crt`, `${CERTS}/client.key`);
    scoped = await ctxFor(`${RIG}/scoped.crt`, `${RIG}/scoped.key`);
    operator = await ctxFor(`${RIG}/operator.crt`, `${RIG}/operator.key`);
    const scopedFp = fs.readFileSync(`${RIG}/scoped.fp`, 'utf8').trim();
    const operatorFp = fs.readFileSync(`${RIG}/operator.fp`, 'utf8').trim();

    // Owner cert is root: it can read IAM state and mint grants.
    let resp = await root.get(`${ORIGIN}/api/access/iam/state`);
    assert(resp.status() === 200, 'root reads IAM state, got ' + resp.status());
    step('owner cert binds as root');

    for (const [fp, label, role, roots] of [
      [scopedFp, 'scoped-human', 'role:files-write', { fs_read_roots: [FILES], fs_write_roots: [FILES] }],
      [operatorFp, 'operator-human', 'role:operator', {}],
    ]) {
      resp = await root.post(`${ORIGIN}/api/access/iam/user-client-grants`, {
        data: { kind: 'browser_mtls_cert', fingerprint: fp, label, role_id: role, ...roots },
      });
      const body = await resp.json().catch(() => ({}));
      assert(resp.ok(), `grant upsert for ${label} ok, got ${resp.status()} ${JSON.stringify(body)}`);
    }
    step('grants bound to the two new fingerprints');

    // files-write human: fs ops work inside the roots — including the new
    // rename and delete — and every escape is refused.
    resp = await scoped.get(`${ORIGIN}/api/fs/list?path=${encodeURIComponent(FILES)}`);
    assert(resp.status() === 200, 'scoped list inside roots, got ' + resp.status());
    resp = await scoped.post(`${ORIGIN}/api/fs/write`, {
      data: { path: `${FILES}/a.txt`, content: 'scoped write\n', create_new: true },
    });
    assert(resp.status() === 200, 'scoped write inside roots, got ' + resp.status());
    resp = await scoped.post(`${ORIGIN}/api/fs/write`, {
      data: { path: `${OUTSIDE}/escape.txt`, content: 'nope', create_new: true },
    });
    assert(resp.status() === 403, 'scoped write outside roots 403, got ' + resp.status());
    assert(!fs.existsSync(`${OUTSIDE}/escape.txt`), 'nothing landed outside');
    step('files-write: write scoping enforced');

    resp = await scoped.post(`${ORIGIN}/api/fs/rename`, {
      data: { from: `${FILES}/a.txt`, to: `${FILES}/b.txt` },
    });
    assert(resp.status() === 200, 'scoped rename inside roots, got ' + resp.status());
    assert(fs.existsSync(`${FILES}/b.txt`), 'rename landed');
    resp = await scoped.post(`${ORIGIN}/api/fs/rename`, {
      data: { from: `${FILES}/b.txt`, to: `${OUTSIDE}/stolen.txt` },
    });
    assert(resp.status() === 403, 'rename destination outside roots 403, got ' + resp.status());
    assert(fs.existsSync(`${FILES}/b.txt`) && !fs.existsSync(`${OUTSIDE}/stolen.txt`), 'destination leg enforced');
    step('files-write: rename gated on both legs');

    resp = await scoped.post(`${ORIGIN}/api/fs/delete`, { data: { path: `${FILES}/b.txt` } });
    assert(resp.status() === 200, 'scoped delete inside roots, got ' + resp.status());
    assert(!fs.existsSync(`${FILES}/b.txt`), 'delete landed');
    resp = await scoped.post(`${ORIGIN}/api/fs/delete`, { data: { path: `${OUTSIDE}/secret.txt` } });
    assert(resp.status() === 403, 'delete outside roots 403, got ' + resp.status());
    assert(fs.existsSync(`${OUTSIDE}/secret.txt`), 'outside file untouched');
    step('files-write: delete scoping enforced');

    // Role ceiling: every scoped human may inspect the access model (that
    // is scoped-human's floor, by design), but never administer it.
    resp = await scoped.get(`${ORIGIN}/api/access/iam/state`);
    assert(resp.status() === 200, 'scoped IAM inspect allowed, got ' + resp.status());
    resp = await scoped.post(`${ORIGIN}/api/access/iam/user-client-grants`, {
      data: { kind: 'browser_mtls_cert', fingerprint: 'f'.repeat(64), label: 'evil', role_id: 'role:root' },
    });
    assert(resp.status() === 403, 'scoped grant mint 403, got ' + resp.status());
    // ...and has no peer.use: signaling relays AND quick controls refuse.
    for (const op of ['dashboard-control-webrtc', 'message', 'task']) {
      resp = await scoped.post(`${ORIGIN}/api/peers/intendant:ghost/${op}`, {
        data: op === 'message' ? { text: 'hi' } : op === 'task' ? { instructions: 'x' } : { offer: 'x' },
      });
      assert(resp.status() === 403, `scoped peer ${op} 403, got ` + resp.status());
    }
    step('files-write: ceiling + peer.use denials');

    // Operator: fs unrestricted, and peer.use clears the permission gate —
    // a ghost peer then fails as unknown/unavailable, never as forbidden.
    resp = await operator.get(`${ORIGIN}/api/fs/list?path=${encodeURIComponent(OUTSIDE)}`);
    assert(resp.status() === 200, 'operator list unrestricted, got ' + resp.status());
    for (const op of ['dashboard-control-webrtc', 'message']) {
      resp = await operator.post(`${ORIGIN}/api/peers/intendant:ghost/${op}`, {
        data: op === 'message' ? { text: 'hi' } : { offer: 'x' },
      });
      assert(resp.status() !== 401 && resp.status() !== 403,
        `operator peer ${op} clears authz, got ` + resp.status());
    }
    step('operator: peer.use clears the gate');

    console.log('SCOPED-HUMAN-MTLS PASS:', steps.length, 'steps');
  } catch (e) {
    console.log('SCOPED-HUMAN-MTLS FAIL after [' + steps.join(' | ') + ']:', e.message);
    process.exitCode = 1;
  } finally {
    for (const c of [root, scoped, operator]) { try { await c?.dispose(); } catch (_) {} }
  }
})();
