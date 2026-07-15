/* ── Intendant vault crypto kernel ──────────────────────────────────────────
 *
 * A dedicated Web Worker that owns ALL vault key material for its lifetime:
 * the 256-bit master key, every key-encryption key (KEK) derived from a
 * passkey PRF secret or the recovery phrase, and the MAC key. The dashboard
 * page (static/app/32-vault-custody.js) talks to it over a tiny postMessage
 * RPC and never sees any of those keys.
 *
 * WHY THIS FILE EXISTS. Before the kernel, unsealing the vault ran inside
 * the ~3.4 MB dashboard bundle: a tampered bundle could exfiltrate the
 * master key at unlock, which breaks every future revision of the vault
 * offline. This file narrows the code whose integrity the key material
 * depends on to the one small worker you are reading:
 *
 *   1. The app.html assembler (crates/app-html-assembler) hashes this file
 *      at build time and pins the sha256 into the bundle
 *      (`VAULT_KERNEL_SHA256`).
 *   2. The page refuses to run vault crypto through anything else: it
 *      fetches /vault-kernel.js, hashes the bytes, and only instantiates
 *      the worker (from a blob: URL) when the hash matches the pin. On a
 *      mismatch the vault fails closed with a loud error — there is no
 *      inline-crypto fallback.
 *   3. On the hosted service, this file is part of the artifact-transparency
 *      manifest (bin/connect/transparency.rs walks every served static
 *      file), so the hash the origin serves is committed to the public
 *      append-only log and re-checked out of band by
 *      `intendant hosted-verify` and the daemon tripwires.
 *
 * HONEST LIMITS. The kernel kills silent KEY exfiltration and offline
 * future-decryption by a swapped bundle. It does NOT bound live misuse: a
 * malicious page can still drive this RPC while the vault is unlocked —
 * read the decrypted entries (the page must render them), encrypt
 * attacker-chosen bodies, open deposits. That live window is bounded by
 * the page's own transparency story (the artifact manifest), not by the
 * kernel. Two more inbound flows are inherent: WebAuthn must run on the
 * page, so the PRF secret transits page memory on its way in (and the page
 * keeps a sessionStorage copy for reload-unlock — a pre-kernel design the
 * kernel does not change); and the decrypted body plaintext (entries,
 * settings, the deposit lane's private JWK, which must ride the sealed
 * blob) flows to the page because the UI renders and edits it. What never
 * leaves this worker: the master key, the KEKs, and the MAC key.
 *
 * EDIT DISCIPLINE. Keep this file small, dependency-free, and boring — it
 * is meant to be audited by a human in one sitting. It must stay a classic
 * (non-module) worker: the page loads it via `new Worker(blobUrl)` with no
 * type option, so any future Content-Security-Policy must allow
 * `worker-src blob:`. After ANY edit, regenerate the pin: `cargo run -p
 * app-html-assembler` (any cargo build also does it), and commit this file
 * together with static/app.html. A daemon-side parity test
 * (web_gateway/static_assets.rs) recomputes this file's sha256 and asserts
 * it equals the constant embedded in app.html, so a forgotten regeneration
 * fails the suite.
 *
 * CRYPTO COMPATIBILITY. Every algorithm below is byte-identical to the
 * pre-kernel implementation in 32-vault-custody.js — the blob format on
 * the wire and at rest did not change. The deposit-open path additionally
 * mirrors the Rust sealer (src/bin/caller/vault_deposits.rs, v1);
 * scripts/vault-deposit-parity.cjs cross-checks that pair against real
 * WebCrypto, and scripts/vault-kernel-exercise.cjs drives this worker's
 * RPC end to end under node.
 * ──────────────────────────────────────────────────────────────────────── */

'use strict';

/* ── Domain constants (must match 32-vault-custody.js / vault_deposits.rs) ── */

const VAULT_HKDF_SALT = 'intendant-vault-v1';
const VAULT_DEPOSIT_INFO = 'intendant-vault-deposit-v1';

/* ── Kernel state ──
 * The raw master key while unlocked, plus the session token minted at
 * unlock. The page holds the token and must present it on every op that
 * touches the key; `lock` wipes both. The token is a handle, not a secret
 * boundary — anything that can read the page's module scope could read it —
 * but it keeps a stale caller (or a page that lost track of lock state)
 * from silently operating on the wrong unlock generation. */

let masterKeyBytes = null; // Uint8Array(32) | null
let unlockToken = null; // string | null

function mintToken() {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return bytesToBase64Url(bytes);
}

function wipeKey() {
  if (masterKeyBytes) masterKeyBytes.fill(0);
  masterKeyBytes = null;
  unlockToken = null;
}

function sameBytes(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a[i] ^ b[i];
  return diff === 0;
}

function holdKey(kBytes) {
  // Concurrent unlock flows of the SAME vault converge on one
  // generation: re-unlocking with an identical master key keeps the
  // existing token. The page may lawfully run two unlock attempts at
  // once (a silent auto-unlock racing a user-initiated one — the
  // pre-kernel code held per-flow key copies, so that race was always
  // benign, and this keeps it benign). A DIFFERENT key (the vault was
  // re-keyed between attempts) replaces the generation and stales the
  // old token.
  if (masterKeyBytes && unlockToken && sameBytes(masterKeyBytes, kBytes)) {
    kBytes.fill(0);
    return unlockToken;
  }
  wipeKey();
  masterKeyBytes = kBytes;
  unlockToken = mintToken();
  return unlockToken;
}

/* Every op that uses the master key calls this first. */
function requireUnlocked(params) {
  if (!masterKeyBytes || !unlockToken || params.token !== unlockToken) {
    throw new Error('vault kernel is locked (or the unlock token is stale)');
  }
}

/* ── Byte helpers (byte-identical to the dashboard's) ── */

function base64UrlToBytes(value) {
  const normalized = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
  const padded = normalized + '='.repeat((4 - (normalized.length % 4)) % 4);
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function bytesToBase64Url(bytes) {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}

function concatBytes(...parts) {
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

/* ── Key derivation ── */

/* HKDF-SHA-256 (salt 'intendant-vault-v1', caller-chosen info) from a raw
 * secret down to a non-extractable AES-256-GCM key. */
async function hkdfAesKey(secretBytes, info) {
  const hkdf = await crypto.subtle.importKey('raw', secretBytes, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new TextEncoder().encode(VAULT_HKDF_SALT),
      info: new TextEncoder().encode(info),
    },
    hkdf,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt']
  );
}

/* KEK from a passkey PRF secret (dedicated vault domain and the legacy
 * fleet-secret domain both use info 'vault-kek' — they differ only in
 * WHICH secret the page hands over). */
function prfKek(secretBytes) {
  return hkdfAesKey(secretBytes, 'vault-kek');
}

/* KEK from the recovery phrase: the standard BIP39 seed stretch
 * (PBKDF2-HMAC-SHA512, salt 'mnemonic', 2048 iterations — the 128-bit
 * entropy does the security work), then our own HKDF domain. The page
 * validates/normalizes the mnemonic (it owns the wordlist UI); the NFKD
 * normalization the BIP39 spec requires happens here so the derivation is
 * self-contained. */
async function phraseKek(phrase) {
  const password = await crypto.subtle.importKey(
    'raw',
    new TextEncoder().encode(String(phrase).normalize('NFKD')),
    'PBKDF2',
    false,
    ['deriveBits']
  );
  const seed = await crypto.subtle.deriveBits(
    { name: 'PBKDF2', hash: 'SHA-512', salt: new TextEncoder().encode('mnemonic'), iterations: 2048 },
    password,
    512
  );
  return hkdfAesKey(new Uint8Array(seed), 'vault-kek-phrase');
}

/* ── Master-key envelopes ── */

function envelopeAad() {
  return new TextEncoder().encode(`${VAULT_HKDF_SALT}|kek`);
}

/* Wrap the master key under a KEK → the {iv, wrapped} crypto fields of an
 * envelope (the page owns the metadata fields: kind, id, label, …). */
async function wrapMasterKey(kek, kBytes) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const wrapped = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv, additionalData: envelopeAad() },
    kek,
    kBytes
  );
  return { iv: bytesToBase64Url(iv), wrapped: bytesToBase64Url(new Uint8Array(wrapped)) };
}

/* Unwrap an envelope; null when the KEK does not open it. */
async function unwrapMasterKey(kek, envelope) {
  try {
    const kBytes = await crypto.subtle.decrypt(
      {
        name: 'AES-GCM',
        iv: base64UrlToBytes(String(envelope.iv || '')),
        additionalData: envelopeAad(),
      },
      kek,
      base64UrlToBytes(String(envelope.wrapped || ''))
    );
    return new Uint8Array(kBytes);
  } catch {
    return null;
  }
}

/* ── Body encryption ── */

/* The body AAD binds the revision into the ciphertext, so the store cannot
 * re-label an old body with a new revision number. */
function bodyAad(revision) {
  return new TextEncoder().encode(`${VAULT_HKDF_SALT}|body|rev:${revision}`);
}

async function masterAesKey(kBytes) {
  return crypto.subtle.importKey('raw', kBytes, { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
}

async function encryptBody(kBytes, bodyObj, revision) {
  const key = await masterAesKey(kBytes);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv, additionalData: bodyAad(revision) },
    key,
    new TextEncoder().encode(JSON.stringify(bodyObj))
  );
  return { iv: bytesToBase64Url(iv), ct: bytesToBase64Url(new Uint8Array(ct)) };
}

/* Decrypt a blob's body; null on any failure (wrong key, tampered bytes,
 * relabeled revision). */
async function decryptBody(kBytes, blob) {
  try {
    const key = await masterAesKey(kBytes);
    const plaintext = await crypto.subtle.decrypt(
      {
        name: 'AES-GCM',
        iv: base64UrlToBytes(String((blob && blob.body && blob.body.iv) || '')),
        additionalData: bodyAad(Number(blob && blob.revision) || 0),
      },
      key,
      base64UrlToBytes(String((blob && blob.body && blob.body.ct) || ''))
    );
    return JSON.parse(new TextDecoder().decode(plaintext));
  } catch {
    return null;
  }
}

/* ── Blob authentication (HMAC over the whole blob) ──
 * Canonical JSON (sorted keys) because the store's serializer is free to
 * reorder object keys in transit. The MAC key derives from the master key,
 * which the store never holds, so it can neither mint nor relabel a MAC'd
 * blob. */

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    const keys = Object.keys(value).sort();
    return `{${keys.map(k => `${JSON.stringify(k)}:${canonicalJson(value[k])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

async function macKey(kBytes) {
  const hkdf = await crypto.subtle.importKey('raw', kBytes, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new TextEncoder().encode(VAULT_HKDF_SALT),
      info: new TextEncoder().encode('vault-mac-v1'),
    },
    hkdf,
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign', 'verify']
  );
}

function macPayload(blob) {
  return new TextEncoder().encode(
    `intendant-vault-mac-v1\n${Number(blob.v) || 0}\n${String(blob.kind || '')}\n` +
      `${Number(blob.revision) || 0}\n${canonicalJson(blob.envelopes || [])}\n` +
      `${canonicalJson(blob.body || {})}`
  );
}

async function computeMac(kBytes, blob) {
  const key = await macKey(kBytes);
  const mac = await crypto.subtle.sign('HMAC', key, macPayload(blob));
  return bytesToBase64Url(new Uint8Array(mac));
}

async function verifyMac(kBytes, blob) {
  const mac = String((blob && blob.mac) || '');
  if (!mac) return false;
  try {
    const key = await macKey(kBytes);
    return await crypto.subtle.verify('HMAC', key, base64UrlToBytes(mac), macPayload(blob));
  } catch {
    return false;
  }
}

/* ── Write-only deposit lane (mirrors vault_deposits.rs v1) ──
 * ECIES: P-256 ECDH → HKDF-SHA256 (empty salt, info = label-bound concat)
 * → AES-256-GCM with a label-bound AAD. The lane keypair rides the sealed
 * body (settings.deposit_lane), so its private JWK is body plaintext — it
 * transits the page like every other entry field; this worker is merely
 * where the ECDH/AEAD math runs. */

async function openDeposit(lanePrivJwk, recipientPubB64u, dep) {
  if (!dep || dep.alg !== 'ECIES-P256-HKDF-SHA256-A256GCM') {
    throw new Error(`unknown deposit alg ${dep && dep.alg}`);
  }
  const encoder = new TextEncoder();
  const privKey = await crypto.subtle.importKey(
    'jwk',
    lanePrivJwk,
    { name: 'ECDH', namedCurve: 'P-256' },
    false,
    ['deriveBits']
  );
  const ephRaw = base64UrlToBytes(String(dep.eph_pub_raw_b64u || ''));
  const ephKey = await crypto.subtle.importKey(
    'raw',
    ephRaw,
    { name: 'ECDH', namedCurve: 'P-256' },
    false,
    []
  );
  const shared = new Uint8Array(
    await crypto.subtle.deriveBits({ name: 'ECDH', public: ephKey }, privKey, 256)
  );
  const label = String(dep.label || '');
  // ring concatenates its HKDF info parts, so `info` here is the same
  // byte string the Rust sealer used.
  const info = concatBytes(
    encoder.encode(VAULT_DEPOSIT_INFO),
    ephRaw,
    base64UrlToBytes(recipientPubB64u),
    encoder.encode(label)
  );
  const hkdfKey = await crypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
  const aesKey = await crypto.subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info },
    hkdfKey,
    { name: 'AES-GCM', length: 256 },
    false,
    ['decrypt']
  );
  const plain = await crypto.subtle.decrypt(
    {
      name: 'AES-GCM',
      iv: base64UrlToBytes(String(dep.nonce_b64u || '')),
      additionalData: encoder.encode(VAULT_DEPOSIT_INFO + ':' + label),
    },
    aesKey,
    base64UrlToBytes(String(dep.ct_b64u || ''))
  );
  return new TextDecoder().decode(plain);
}

/* ── RPC operations ──
 * The page sends {id, op, params}; the kernel answers {id, ok: true,
 * result} or {id, ok: false, error}. Ops that use the master key require
 * the unlock token; the unlock ops and `create` mint it. */

const OPS = {
  /* Try every phrase envelope with the KEK derived from a normalized
   * recovery phrase. On success the kernel holds the master key.
   *   params: { phrase, envelopes }
   *   result: { unlocked, token?, envelope_id? } */
  async 'unlock-phrase'(params) {
    const kek = await phraseKek(String(params.phrase || ''));
    for (const envelope of params.envelopes || []) {
      if (!envelope || envelope.kind !== 'phrase') continue;
      const kBytes = await unwrapMasterKey(kek, envelope);
      if (kBytes) return { unlocked: true, token: holdKey(kBytes), envelope_id: envelope.id || null };
    }
    return { unlocked: false };
  },

  /* Try every prf envelope: a `prf: 'vault-v1'` marker selects the
   * dedicated vault PRF secret, markerless envelopes are legacy (KEK from
   * the fleet secret). `saw_kek` reports whether any KEK was derivable at
   * all — the page words its error message on it.
   *   params: { envelopes, secret_dedicated?, secret_legacy? }  (Uint8Array)
   *   result: { unlocked, token?, envelope_id?, envelope_prf?, saw_kek } */
  async 'unlock-prf'(params) {
    const dedicated = params.secret_dedicated ? await prfKek(params.secret_dedicated) : null;
    const legacy = params.secret_legacy ? await prfKek(params.secret_legacy) : null;
    let sawKek = false;
    for (const envelope of params.envelopes || []) {
      if (!envelope || envelope.kind !== 'prf') continue;
      const kek = envelope.prf === 'vault-v1' ? dedicated : legacy;
      if (!kek) continue;
      sawKek = true;
      const kBytes = await unwrapMasterKey(kek, envelope);
      if (kBytes) {
        return {
          unlocked: true,
          token: holdKey(kBytes),
          envelope_id: envelope.id || null,
          envelope_prf: envelope.prf || null,
          saw_kek: true,
        };
      }
    }
    return { unlocked: false, saw_kek: sawKek };
  },

  /* Which prf envelope (if any) do this session's PRF secrets open?
   * Pure probe for the enroll flow — never changes the held key.
   *   params: { envelopes, secret_dedicated?, secret_legacy? }
   *   result: { envelope_id: string|null } */
  async 'match-prf-envelope'(params) {
    const dedicated = params.secret_dedicated ? await prfKek(params.secret_dedicated) : null;
    const legacy = params.secret_legacy ? await prfKek(params.secret_legacy) : null;
    for (const envelope of params.envelopes || []) {
      if (!envelope || envelope.kind !== 'prf') continue;
      const kek = envelope.prf === 'vault-v1' ? dedicated : legacy;
      if (!kek) continue;
      const kBytes = await unwrapMasterKey(kek, envelope);
      if (kBytes) {
        kBytes.fill(0);
        return { envelope_id: envelope.id || null };
      }
    }
    return { envelope_id: null };
  },

  /* Create a fresh vault: generate the master key, wrap it into the
   * phrase envelope (mandatory) and optionally a prf envelope, encrypt an
   * empty body, MAC the assembled blob, and stay unlocked. The page owns
   * every metadata field (ids, labels, timestamps) and passes them in;
   * the kernel adds only crypto fields and the fixed format tag.
   *   params: { phrase, phrase_envelope, prf_secret?, prf_mark?,
   *             prf_envelope?, revision, now }
   *   result: { token, blob, matched_envelope_id } */
  async create(params) {
    const kBytes = crypto.getRandomValues(new Uint8Array(32));
    const revision = Number(params.revision) || 1;
    const now = Number(params.now) || Date.now();
    const envelopes = [
      Object.assign(
        {},
        params.phrase_envelope,
        await wrapMasterKey(await phraseKek(String(params.phrase || '')), kBytes)
      ),
    ];
    let matched = null;
    if (params.prf_secret && params.prf_envelope) {
      const envelope = Object.assign(
        {},
        params.prf_envelope,
        params.prf_mark ? { prf: params.prf_mark } : {},
        await wrapMasterKey(await prfKek(params.prf_secret), kBytes)
      );
      envelopes.push(envelope);
      matched = envelope.id || null;
    }
    const blob = {
      v: 1,
      kind: 'intendant-vault',
      revision,
      created_unix_ms: now,
      updated_unix_ms: now,
      envelopes,
      body: await encryptBody(kBytes, { entries: [], settings: {} }, revision),
    };
    blob.mac = await computeMac(kBytes, blob);
    return { token: holdKey(kBytes), blob, matched_envelope_id: matched };
  },

  /* Wrap the held master key under a KEK derived from a passkey PRF
   * secret → the {iv, wrapped} fields of a new prf envelope (enrolling
   * this passkey, or migrating a legacy envelope onto the dedicated
   * domain — the page decides which and owns the metadata).
   *   params: { token, prf_secret }
   *   result: { iv, wrapped } */
  async 'wrap-new-envelope'(params) {
    requireUnlocked(params);
    if (!params.prf_secret) throw new Error('wrap-new-envelope needs a PRF secret');
    return wrapMasterKey(await prfKek(params.prf_secret), masterKeyBytes);
  },

  /*   params: { token, body, revision }   result: { iv, ct } */
  async 'encrypt-body'(params) {
    requireUnlocked(params);
    return encryptBody(masterKeyBytes, params.body, Number(params.revision) || 0);
  },

  /*   params: { token, blob }   result: { body: object|null } */
  async 'decrypt-body'(params) {
    requireUnlocked(params);
    return { body: await decryptBody(masterKeyBytes, params.blob) };
  },

  /*   params: { token, blob }   result: { mac } */
  async 'compute-mac'(params) {
    requireUnlocked(params);
    return { mac: await computeMac(masterKeyBytes, params.blob) };
  },

  /*   params: { token, blob }   result: { valid: bool } */
  async 'verify-mac'(params) {
    requireUnlocked(params);
    return { valid: await verifyMac(masterKeyBytes, params.blob) };
  },

  /* Open one CLI deposit sealed to the lane public key. Requires the
   * unlocked token for discipline (deposits only fold into an unlocked
   * vault) even though the math uses the lane key, not the master key.
   *   params: { token, deposit, lane_priv_jwk, lane_pub_raw_b64u }
   *   result: { secret } */
  async 'open-deposit'(params) {
    requireUnlocked(params);
    return {
      secret: await openDeposit(
        params.lane_priv_jwk,
        String(params.lane_pub_raw_b64u || ''),
        params.deposit
      ),
    };
  },

  /* Generate the deposit-lane P-256 keypair. Extractable by necessity:
   * the private JWK must ride the sealed body so every unlocking device
   * can open deposits — it is returned to the page as body material, not
   * kept here.
   *   params: { token }   result: { priv_jwk, pub_raw_b64u } */
  async 'generate-deposit-keypair'(params) {
    requireUnlocked(params);
    const pair = await crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, [
      'deriveBits',
    ]);
    const privJwk = await crypto.subtle.exportKey('jwk', pair.privateKey);
    const pubRaw = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
    return { priv_jwk: privJwk, pub_raw_b64u: bytesToBase64Url(pubRaw) };
  },

  /* Wipe the master key and invalidate the token. Always allowed.
   *   params: {}   result: { locked: true } */
  async lock() {
    wipeKey();
    return { locked: true };
  },
};

self.onmessage = event => {
  const msg = event.data || {};
  const id = msg.id;
  const handler = OPS[msg.op];
  if (typeof handler !== 'function') {
    self.postMessage({ id, ok: false, error: `unknown vault-kernel op ${String(msg.op)}` });
    return;
  }
  Promise.resolve()
    .then(() => handler(msg.params || {}))
    .then(
      result => self.postMessage({ id, ok: true, result }),
      err => self.postMessage({ id, ok: false, error: String((err && err.message) || err) })
    );
};
