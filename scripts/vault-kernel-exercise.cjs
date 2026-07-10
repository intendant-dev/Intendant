#!/usr/bin/env node
/* End-to-end exercise of the vault crypto kernel (static/vault-kernel.js)
   under node's real WebCrypto: loads the exact worker source with a shimmed
   `self`, drives its postMessage RPC through a full vault lifecycle —
   create, MAC verify, body round-trip, lock/unlock (phrase, dedicated PRF,
   legacy PRF), envelope wrap + enroll probe, deposit keypair + a deposit
   sealed the way the Rust CLI seals (mirroring vault_deposits.rs v1, like
   scripts/vault-deposit-parity.cjs) — plus the fail-closed cases (stale
   token, tampered blob, wrong phrase, unknown op).

   Keyless, no network, no binary needed:

     node scripts/vault-kernel-exercise.cjs

   Run it after touching the kernel (and re-run the assembler: the app.html
   pin must be regenerated in the same commit — the Rust parity test in
   web_gateway/static_assets.rs enforces that half). */
'use strict';

const assert = require('assert');
const fs = require('fs');
const path = require('path');

const subtle = globalThis.crypto.subtle;

/* ── Load the kernel with a Web-Worker-shaped `self` shim ── */

const kernelPath = path.join(__dirname, '..', 'static', 'vault-kernel.js');
const kernelSource = fs.readFileSync(kernelPath, 'utf8');

const pending = new Map();
const selfShim = {
  onmessage: null,
  postMessage(msg) {
    const waiter = pending.get(msg.id);
    if (!waiter) throw new Error(`kernel answered unknown request id ${msg.id}`);
    pending.delete(msg.id);
    waiter(msg);
  },
};
new Function('self', `${kernelSource}\n`)(selfShim);
assert(typeof selfShim.onmessage === 'function', 'kernel must install onmessage');

let seq = 0;
function call(op, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, msg => (msg.ok ? resolve(msg.result) : reject(new Error(msg.error))));
    selfShim.onmessage({ data: { id, op, params } });
  });
}

async function rejects(promise, pattern, label) {
  try {
    await promise;
  } catch (err) {
    assert(
      pattern.test(String(err.message)),
      `${label}: error ${JSON.stringify(String(err.message))} must match ${pattern}`
    );
    return;
  }
  assert.fail(`${label}: expected a rejection`);
}

function b64u(bytes) {
  return Buffer.from(bytes).toString('base64url');
}
function fromB64u(text) {
  return new Uint8Array(Buffer.from(String(text), 'base64url'));
}
function concatBytes(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let offset = 0;
  for (const p of parts) {
    out.set(p, offset);
    offset += p.length;
  }
  return out;
}

/* Seal a deposit exactly the way the Rust CLI does (vault_deposits.rs v1) —
   the kernel must open it. */
async function sealDeposit(recipientPubRaw, label, secret) {
  const INFO = 'intendant-vault-deposit-v1';
  const encoder = new TextEncoder();
  const eph = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
  const ephRaw = new Uint8Array(await subtle.exportKey('raw', eph.publicKey));
  const recipientKey = await subtle.importKey(
    'raw',
    recipientPubRaw,
    { name: 'ECDH', namedCurve: 'P-256' },
    false,
    []
  );
  const shared = new Uint8Array(
    await subtle.deriveBits({ name: 'ECDH', public: recipientKey }, eph.privateKey, 256)
  );
  const info = concatBytes(encoder.encode(INFO), ephRaw, recipientPubRaw, encoder.encode(label));
  const hkdfKey = await subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
  const aesKey = await subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info },
    hkdfKey,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt']
  );
  const nonce = globalThis.crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(
    await subtle.encrypt(
      { name: 'AES-GCM', iv: nonce, additionalData: encoder.encode(`${INFO}:${label}`) },
      aesKey,
      encoder.encode(secret)
    )
  );
  return {
    alg: 'ECIES-P256-HKDF-SHA256-A256GCM',
    label,
    eph_pub_raw_b64u: b64u(ephRaw),
    nonce_b64u: b64u(nonce),
    ct_b64u: b64u(ct),
  };
}

async function main() {
  const phrase = 'legal winner thank year wave sausage worth useful legal winner thank yellow';
  const prfSecret = globalThis.crypto.getRandomValues(new Uint8Array(32));
  const now = Date.now();

  /* 1. Create: two envelopes (phrase + dedicated-domain prf), MAC'd blob,
     kernel stays unlocked. */
  const created = await call('create', {
    phrase,
    phrase_envelope: { kind: 'phrase', id: 'env_phrase', label: 'Recovery phrase', created_unix_ms: now },
    prf_secret: new Uint8Array(prfSecret),
    prf_mark: 'vault-v1',
    prf_envelope: { kind: 'prf', id: 'env_prf', label: 'Passkey', created_unix_ms: now },
    revision: 1,
    now,
  });
  let token = created.token;
  const blob = created.blob;
  assert(token && typeof token === 'string', 'create must mint a token');
  assert.equal(created.matched_envelope_id, 'env_prf');
  assert.equal(blob.v, 1);
  assert.equal(blob.kind, 'intendant-vault');
  assert.equal(blob.revision, 1);
  assert.equal(blob.envelopes.length, 2);
  assert.equal(blob.envelopes[1].prf, 'vault-v1');
  assert(blob.mac, 'created blob must carry a MAC');

  /* 2. MAC verifies; body decrypts to the empty vault. */
  assert.equal((await call('verify-mac', { token, blob })).valid, true);
  assert.deepEqual((await call('decrypt-body', { token, blob })).body, { entries: [], settings: {} });

  /* 3. Publish-shaped round trip: re-encrypt a body at revision 2, MAC the
     rebuilt blob, verify + decrypt. */
  const body2 = { entries: [{ id: 'cred_1', kind: 'api_key', secret: 's3cr3t' }], settings: {} };
  const blob2 = { ...blob, revision: 2, body: await call('encrypt-body', { token, body: body2, revision: 2 }) };
  blob2.mac = (await call('compute-mac', { token, blob: blob2 })).mac;
  assert.equal((await call('verify-mac', { token, blob: blob2 })).valid, true);
  assert.deepEqual((await call('decrypt-body', { token, blob: blob2 })).body, body2);

  /* 4. Tampering: flip ciphertext → MAC fails and the body refuses. */
  const tampered = JSON.parse(JSON.stringify(blob2));
  const ctBytes = fromB64u(tampered.body.ct);
  ctBytes[0] ^= 0xff;
  tampered.body.ct = b64u(ctBytes);
  assert.equal((await call('verify-mac', { token, blob: tampered })).valid, false);
  assert.equal((await call('decrypt-body', { token, blob: tampered })).body, null);
  /* Revision relabeling: same bytes under a different revision refuse. */
  const relabeled = JSON.parse(JSON.stringify(blob2));
  relabeled.revision = 7;
  assert.equal((await call('decrypt-body', { token, blob: relabeled })).body, null);

  /* 5. Lock wipes: the old token must stop working. */
  const preLockToken = token;
  await call('lock');
  await rejects(call('encrypt-body', { token, body: {}, revision: 3 }), /locked/, 'stale token');

  /* 6. Phrase unlock (kernel does the NFKD + PBKDF2 + HKDF internally).
     Post-lock the generation is fresh: a NEW token. */
  const phraseUnlock = await call('unlock-phrase', { phrase, envelopes: blob2.envelopes });
  assert.equal(phraseUnlock.unlocked, true);
  assert.equal(phraseUnlock.envelope_id, 'env_phrase');
  assert.notEqual(phraseUnlock.token, preLockToken, 'a post-lock unlock must mint a fresh token');
  token = phraseUnlock.token;
  assert.deepEqual((await call('decrypt-body', { token, blob: blob2 })).body, body2);
  assert.equal(
    (await call('unlock-phrase', { phrase: 'wrong words entirely', envelopes: blob2.envelopes })).unlocked,
    false
  );
  // The failed attempt must not have wiped the prior unlock's key…
  // (holdKey only runs on success), but its token is unchanged:
  assert.deepEqual((await call('decrypt-body', { token, blob: blob2 })).body, body2);

  /* 7. PRF unlock on the dedicated domain; the enroll probe agrees.
     Same master key while already unlocked → the generation CONVERGES
     (same token): a silent auto-unlock racing a user unlock must never
     stale the winner's token. */
  const prfUnlock = await call('unlock-prf', {
    envelopes: blob2.envelopes,
    secret_dedicated: new Uint8Array(prfSecret),
    secret_legacy: null,
  });
  assert.equal(prfUnlock.unlocked, true);
  assert.equal(prfUnlock.envelope_id, 'env_prf');
  assert.equal(prfUnlock.envelope_prf, 'vault-v1');
  assert.equal(prfUnlock.token, token, 'same-key re-unlock must keep the unlock token');
  token = prfUnlock.token;
  const probe = await call('match-prf-envelope', {
    envelopes: blob2.envelopes,
    secret_dedicated: new Uint8Array(prfSecret),
    secret_legacy: null,
  });
  assert.equal(probe.envelope_id, 'env_prf');
  /* The dedicated secret presented as LEGACY must not open the marked
     envelope (domain separation by marker). */
  const wrongDomain = await call('unlock-prf', {
    envelopes: blob2.envelopes,
    secret_dedicated: null,
    secret_legacy: new Uint8Array(prfSecret),
  });
  assert.equal(wrongDomain.unlocked, false);
  assert.equal(wrongDomain.saw_kek, false, 'marked envelope must not consult the legacy KEK');
  assert.deepEqual((await call('decrypt-body', { token, blob: blob2 })).body, body2);

  /* 8. Enroll a second passkey: wrap a markerless (legacy-shaped) envelope
     and unlock through it with secret_legacy. */
  const secondSecret = globalThis.crypto.getRandomValues(new Uint8Array(32));
  const wrapped = await call('wrap-new-envelope', { token, prf_secret: new Uint8Array(secondSecret) });
  const legacyEnvelope = { kind: 'prf', id: 'env_prf2', label: 'Second key', created_unix_ms: now, ...wrapped };
  const envelopes3 = [...blob2.envelopes, legacyEnvelope];
  const legacyUnlock = await call('unlock-prf', {
    envelopes: envelopes3,
    secret_dedicated: null,
    secret_legacy: new Uint8Array(secondSecret),
  });
  assert.equal(legacyUnlock.unlocked, true);
  assert.equal(legacyUnlock.envelope_id, 'env_prf2');
  assert.equal(legacyUnlock.envelope_prf, null);
  token = legacyUnlock.token;

  /* 9. Deposit lane: kernel-generated keypair opens a CLI-shaped deposit. */
  const lane = await call('generate-deposit-keypair', { token });
  assert(lane.priv_jwk && lane.priv_jwk.kty === 'EC', 'lane private key must be an EC JWK');
  assert(lane.pub_raw_b64u, 'lane public key must export raw');
  const depositSecret = 'correct horse battery staple — π🔑';
  const deposit = await sealDeposit(fromB64u(lane.pub_raw_b64u), 'exercise gh-token', depositSecret);
  const opened = await call('open-deposit', {
    token,
    deposit,
    lane_priv_jwk: lane.priv_jwk,
    lane_pub_raw_b64u: lane.pub_raw_b64u,
  });
  assert.equal(opened.secret, depositSecret);
  /* A label swap breaks the AAD/KDF binding. */
  await rejects(
    call('open-deposit', {
      token,
      deposit: { ...deposit, label: 'relabeled' },
      lane_priv_jwk: lane.priv_jwk,
      lane_pub_raw_b64u: lane.pub_raw_b64u,
    }),
    /./,
    'relabeled deposit'
  );

  /* 10. Unknown ops fail; ops on a locked kernel fail. */
  await rejects(call('no-such-op'), /unknown vault-kernel op/, 'unknown op');
  await call('lock');
  await rejects(call('decrypt-body', { token, blob: blob2 }), /locked/, 'locked kernel');

  console.log('vault-kernel exercise: all checks passed');
}

main().catch(err => {
  console.error('vault-kernel exercise FAILED:', err);
  process.exit(1);
});
