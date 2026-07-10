#!/usr/bin/env node
/* Cross-implementation parity check for the write-only vault deposit lane:
   the Rust CLI seals (src/bin/caller/vault_deposits.rs), real WebCrypto
   opens (the same calls 32-vault-custody.js makes). Run it locally after
   touching either side — it is deliberately not in CI (needs a built
   binary):

     cargo build --bin intendant
     node scripts/vault-deposit-parity.cjs ./target/debug/intendant

   Exit 0 = the two implementations agree byte for byte. */
'use strict';
const { webcrypto } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const subtle = webcrypto.subtle;
const INFO = 'intendant-vault-deposit-v1';

function b64u(bytes) {
  return Buffer.from(bytes).toString('base64url');
}
function fromB64u(text) {
  return new Uint8Array(Buffer.from(String(text), 'base64url'));
}
function concatBytes(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let offset = 0;
  for (const p of parts) { out.set(p, offset); offset += p.length; }
  return out;
}

async function main() {
  const binary = process.argv[2] || './target/debug/intendant';
  if (!fs.existsSync(binary)) {
    console.error(`binary not found: ${binary} (build it first)`);
    process.exit(2);
  }

  // The "vault": a real WebCrypto P-256 keypair, exactly as the dashboard
  // generates it.
  const pair = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
  const pubRaw = new Uint8Array(await subtle.exportKey('raw', pair.publicKey));

  // A scratch daemon state root holding only the published deposit key.
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'vault-parity-'));
  try {
    fs.writeFileSync(path.join(home, 'vault-deposit-key.pub.json'), JSON.stringify({
      alg: 'ECDH-P256',
      pub_raw_b64u: b64u(pubRaw),
      published_unix_ms: Date.now(),
    }));

    const label = 'parity gh-token';
    const secret = 'correct horse battery staple — π🔑';
    execFileSync(binary, ['vault', 'deposit', label], {
      input: secret + '\n',
      env: { ...process.env, INTENDANT_HOME: home },
      stdio: ['pipe', 'inherit', 'inherit'],
    });

    const dir = path.join(home, 'vault-deposits.d');
    const files = fs.readdirSync(dir).filter(f => f.endsWith('.json'));
    if (files.length !== 1) throw new Error(`expected 1 deposit record, found ${files.length}`);
    const record = JSON.parse(fs.readFileSync(path.join(dir, files[0]), 'utf8'));
    if (record.alg !== 'ECIES-P256-HKDF-SHA256-A256GCM') {
      throw new Error(`unexpected alg ${record.alg}`);
    }
    if (record.label !== label) throw new Error(`label mismatch: ${record.label}`);

    // Open with the dashboard's exact algorithm (vaultOpenDeposit).
    const encoder = new TextEncoder();
    const ephRaw = fromB64u(record.eph_pub_raw_b64u);
    const ephKey = await subtle.importKey('raw', ephRaw, { name: 'ECDH', namedCurve: 'P-256' }, false, []);
    const shared = new Uint8Array(await subtle.deriveBits({ name: 'ECDH', public: ephKey }, pair.privateKey, 256));
    const info = concatBytes(encoder.encode(INFO), ephRaw, pubRaw, encoder.encode(record.label));
    const hkdfKey = await subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
    const aesKey = await subtle.deriveKey(
      { name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info },
      hkdfKey, { name: 'AES-GCM', length: 256 }, false, ['decrypt']);
    const plain = await subtle.decrypt(
      {
        name: 'AES-GCM',
        iv: fromB64u(record.nonce_b64u),
        additionalData: encoder.encode(`${INFO}:${record.label}`),
      },
      aesKey, fromB64u(record.ct_b64u));
    const text = new TextDecoder().decode(plain);
    if (text !== secret) throw new Error(`plaintext mismatch: ${JSON.stringify(text)}`);

    // Tamper check: a different label must fail AAD/KDF binding.
    let tamperFailed = false;
    try {
      const badInfo = concatBytes(encoder.encode(INFO), ephRaw, pubRaw, encoder.encode('other'));
      const badKey = await subtle.deriveKey(
        { name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info: badInfo },
        hkdfKey, { name: 'AES-GCM', length: 256 }, false, ['decrypt']);
      await subtle.decrypt(
        { name: 'AES-GCM', iv: fromB64u(record.nonce_b64u), additionalData: encoder.encode(`${INFO}:other`) },
        badKey, fromB64u(record.ct_b64u));
    } catch {
      tamperFailed = true;
    }
    if (!tamperFailed) throw new Error('tampered label decrypted — binding broken');

    console.log('PARITY OK: rust seal ↔ webcrypto open agree (round-trip + label binding)');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
}

main().catch(err => {
  console.error('PARITY FAILED:', err.message || err);
  process.exit(1);
});
