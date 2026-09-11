//! The WebCrypto backend: every [`Crypto`] primitive through
//! `crypto.subtle` (§13.2 browser column — "import/sign/verify/
//! encrypt/decrypt paths"). Composition notes, per primitive:
//!
//! - **SHA-256** — `subtle.digest`.
//! - **Ed25519 verify** — raw `importKey` + `verify`. Chromium ships
//!   Web Crypto Ed25519 (Secure Curves); if an import fails, a probe
//!   against the RFC 8032 §7.1 TEST 1 public key distinguishes
//!   "this browser has no Ed25519" (backend `Err`) from "this key is
//!   malformed" (semantic `false`).
//! - **P-256 verify** — WebCrypto ECDSA accepts high-S signatures,
//!   so the spec's low-S policy (§3) is enforced HERE by comparing
//!   the raw big-endian `s` against ⌊n/2⌋ before any curve math —
//!   the same order as the native reducer. The SEC1 shape check
//!   (65 bytes, 0x04) also mirrors the native lane, since WebCrypto
//!   would otherwise accept compressed keys the reducer rejects.
//! - **HPKE open** (`hpke-p256-v1` = DHKEM(P-256, HKDF-SHA256) +
//!   HKDF-SHA256 + AES-256-GCM, RFC 9180 base mode) — composed from
//!   WebCrypto ECDH `deriveBits` (the raw shared x-coordinate) plus
//!   the labeled-HKDF schedule built on `subtle.sign(HMAC)`:
//!   LabeledExtract = HMAC(salt-or-zeros, "HPKE-v1"‖suite‖label‖ikm)
//!   and LabeledExpand's single block T(1) = HMAC(prk, info‖0x01)
//!   covers every output here (Nsecret=Nk=32, Nn=12 ≤ 32). The
//!   recipient public key for kem_context comes from the private
//!   key's JWK export.
//! - **AES-GCM** — `subtle.encrypt`/`decrypt` (ct‖tag layout matches
//!   the Rust `aes-gcm` crate). A decrypt exception is the semantic
//!   `None`: WebCrypto reports authentication failure as an opaque
//!   `OperationError`, and the positive lanes (seal-open, full-chain
//!   opens) would surface a broken backend as loud FAILs.
//! - **HKDF / PBKDF2** — native `deriveBits` (extract-then-expand is
//!   exactly WebCrypto's HKDF shape; PBKDF2-HMAC-SHA512 → 64 B).
//! - **Ed25519 seed→pk** — PKCS#8-wrap the seed (RFC 8410 prefix),
//!   import extractable, export JWK, decode `x` (RFC 8037 requires
//!   the public half in OKP JWKs). P-256 scalar→pk works the same
//!   way over the SEC1 prefix (BoringSSL computes the public key
//!   when the ECPrivateKey omits it).

use js_sys::{Array, Object, Promise, Reflect, Uint8Array};
use owner_plane_reducer::crypto::Crypto;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CryptoKey, SubtleCrypto};

/// RFC 8410 PKCS#8 prefix for a raw 32-byte Ed25519 seed.
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// PKCS#8 prefix for a raw P-256 scalar (ECPrivateKey without the
/// optional public key).
const P256_PKCS8_PREFIX: [u8; 35] = [
    0x30, 0x41, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01,
    0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x27, 0x30, 0x25, 0x02, 0x01,
    0x01, 0x04, 0x20,
];

/// ⌊n/2⌋ for the P-256 group order — the low-S boundary (§3).
const P256_HALF_N: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0x80, 0x00, 0x00, 0x00, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xde, 0x73, 0x7d, 0x56, 0xd3, 0x8b, 0xcf, 0x42, 0x79, 0xdc, 0xe5, 0x61, 0x7e, 0x31, 0x92, 0xa8,
];

/// RFC 8032 §7.1 TEST 1 public key — the Ed25519 availability probe.
const ED25519_PROBE_PK: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

fn js_err(context: &str, v: JsValue) -> String {
    let detail = v
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| v.as_string())
        .unwrap_or_else(|| format!("{v:?}"));
    format!("{context}: {detail}")
}

fn subtle() -> Result<SubtleCrypto, String> {
    Ok(web_sys::window()
        .ok_or("no window (the lane runs in a page context)")?
        .crypto()
        .map_err(|e| js_err("crypto", e))?
        .subtle())
}

async fn jsp(context: &str, p: Result<Promise, JsValue>) -> Result<JsValue, String> {
    JsFuture::from(p.map_err(|e| js_err(context, e))?)
        .await
        .map_err(|e| js_err(context, e))
}

fn buf_to_vec(v: &JsValue) -> Vec<u8> {
    Uint8Array::new(v).to_vec()
}

fn usages(list: &[&str]) -> Array {
    list.iter().map(|u| JsValue::from_str(u)).collect()
}

/// Build a params dictionary from (key, value) pairs.
fn dict(pairs: &[(&str, &JsValue)]) -> Object {
    let o = Object::new();
    for (k, v) in pairs {
        Reflect::set(&o, &JsValue::from_str(k), v).expect("plain object set");
    }
    o
}

fn bytes_val(b: &[u8]) -> JsValue {
    Uint8Array::from(b).into()
}

/// `importKey` with a STRING algorithm (Ed25519, AES-GCM, HKDF,
/// PBKDF2). `Ok(Err(_))` = the import call rejected (semantic for
/// key material, availability for algorithms — callers decide).
async fn import_raw_str(
    alg: &str,
    key_data: &[u8],
    extractable: bool,
    key_usages: &[&str],
) -> Result<Result<CryptoKey, String>, String> {
    let s = subtle()?;
    let data = Uint8Array::from(key_data);
    let p = s.import_key_with_str("raw", &data, alg, extractable, &usages(key_usages));
    match JsFuture::from(p.map_err(|e| js_err("importKey", e))?).await {
        Ok(k) => Ok(Ok(k.unchecked_into())),
        Err(e) => Ok(Err(js_err("importKey", e))),
    }
}

/// `importKey` with an OBJECT algorithm (ECDSA / ECDH / HMAC).
async fn import_obj(
    format: &str,
    alg: &Object,
    key_data: &[u8],
    extractable: bool,
    key_usages: &[&str],
) -> Result<Result<CryptoKey, String>, String> {
    let s = subtle()?;
    let data = Uint8Array::from(key_data);
    let p = s.import_key_with_object(format, &data, alg, extractable, &usages(key_usages));
    match JsFuture::from(p.map_err(|e| js_err("importKey", e))?).await {
        Ok(k) => Ok(Ok(k.unchecked_into())),
        Err(e) => Ok(Err(js_err("importKey", e))),
    }
}

fn ec_alg(name: &str) -> Object {
    dict(&[
        ("name", &JsValue::from_str(name)),
        ("namedCurve", &JsValue::from_str("P-256")),
    ])
}

/// HMAC-SHA256 via `subtle.sign` — the labeled-HKDF building block.
async fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<[u8; 32], String> {
    let alg = dict(&[
        ("name", &JsValue::from_str("HMAC")),
        ("hash", &JsValue::from_str("SHA-256")),
    ]);
    let key = import_obj("raw", &alg, key, false, &["sign"])
        .await?
        .map_err(|e| format!("HMAC key: {e}"))?;
    let s = subtle()?;
    let out = jsp(
        "HMAC sign",
        s.sign_with_str_and_u8_array("HMAC", &key, data),
    )
    .await?;
    buf_to_vec(&out)
        .try_into()
        .map_err(|_| "HMAC output is 32 bytes".into())
}

/// Base64url (no padding) → bytes, for JWK fields.
fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = ALPHABET
            .iter()
            .position(|&a| a == c)
            .ok_or_else(|| format!("base64url byte {c:#04x}"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Export a private key's JWK and decode one base64url field.
async fn jwk_field(key: &CryptoKey, field: &str) -> Result<Vec<u8>, String> {
    let s = subtle()?;
    let jwk = jsp("exportKey(jwk)", s.export_key("jwk", key)).await?;
    let v = Reflect::get(&jwk, &JsValue::from_str(field))
        .map_err(|e| js_err("jwk field", e))?
        .as_string()
        .ok_or_else(|| format!("JWK has no string {field:?}"))?;
    b64url_decode(&v)
}

/// Import a P-256 scalar as an extractable ECDH private key.
/// `Ok(Err(_))` = invalid scalar (zero / ≥ n / malformed).
async fn import_p256_scalar(sk: &[u8; 32]) -> Result<Result<CryptoKey, String>, String> {
    let mut pkcs8 = P256_PKCS8_PREFIX.to_vec();
    pkcs8.extend_from_slice(sk);
    import_obj("pkcs8", &ec_alg("ECDH"), &pkcs8, true, &["deriveBits"]).await
}

/// SEC1 uncompressed public key of an imported P-256 private key,
/// via its JWK `x`/`y`.
async fn p256_public_sec1(key: &CryptoKey) -> Result<[u8; 65], String> {
    let x = jwk_field(key, "x").await?;
    let y = jwk_field(key, "y").await?;
    if x.len() != 32 || y.len() != 32 {
        return Err(format!("JWK coordinate widths {}/{}", x.len(), y.len()));
    }
    let mut pk = [0u8; 65];
    pk[0] = 0x04;
    pk[1..33].copy_from_slice(&x);
    pk[33..].copy_from_slice(&y);
    Ok(pk)
}

// ------------------------------------------------- HPKE composition

const HPKE_V1: &[u8] = b"HPKE-v1";
/// DHKEM(P-256, HKDF-SHA256) = 0x0010.
const SUITE_KEM: &[u8] = b"KEM\x00\x10";
/// KEM 0x0010, KDF HKDF-SHA256 0x0001, AEAD AES-256-GCM 0x0002.
const SUITE_HPKE: &[u8] = b"HPKE\x00\x10\x00\x01\x00\x02";

/// RFC 9180 §4 LabeledExtract over HMAC (empty salt = 32 zeros per
/// RFC 5869).
async fn labeled_extract(
    salt: &[u8],
    suite: &[u8],
    label: &[u8],
    ikm: &[u8],
) -> Result<[u8; 32], String> {
    let zeros = [0u8; 32];
    let key = if salt.is_empty() { &zeros[..] } else { salt };
    let mut labeled = Vec::with_capacity(HPKE_V1.len() + suite.len() + label.len() + ikm.len());
    labeled.extend_from_slice(HPKE_V1);
    labeled.extend_from_slice(suite);
    labeled.extend_from_slice(label);
    labeled.extend_from_slice(ikm);
    hmac_sha256(key, &labeled).await
}

/// RFC 9180 §4 LabeledExpand, single HKDF-Expand block (every output
/// this suite needs is ≤ 32 bytes).
async fn labeled_expand(
    prk: &[u8; 32],
    suite: &[u8],
    label: &[u8],
    info: &[u8],
    len: usize,
) -> Result<Vec<u8>, String> {
    assert!(len <= 32, "single-block expand");
    let mut labeled =
        Vec::with_capacity(2 + HPKE_V1.len() + suite.len() + label.len() + info.len());
    labeled.extend_from_slice(&(len as u16).to_be_bytes());
    labeled.extend_from_slice(HPKE_V1);
    labeled.extend_from_slice(suite);
    labeled.extend_from_slice(label);
    labeled.extend_from_slice(info);
    labeled.push(0x01);
    Ok(hmac_sha256(prk, &labeled).await?[..len].to_vec())
}

// ------------------------------------------------------ the backend

pub struct WebCryptoBackend;

impl WebCryptoBackend {
    async fn aes_gcm(
        &self,
        encrypt: bool,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        data: &[u8],
    ) -> Result<Result<Vec<u8>, String>, String> {
        let usage = if encrypt { "encrypt" } else { "decrypt" };
        let k = import_raw_str("AES-GCM", key, false, &[usage])
            .await?
            .map_err(|e| format!("AES-GCM key: {e}"))?;
        let params = dict(&[
            ("name", &JsValue::from_str("AES-GCM")),
            ("iv", &bytes_val(nonce)),
            ("additionalData", &bytes_val(aad)),
            ("tagLength", &JsValue::from_f64(128.0)),
        ]);
        let s = subtle()?;
        let p = if encrypt {
            s.encrypt_with_object_and_u8_array(&params, &k, data)
        } else {
            s.decrypt_with_object_and_u8_array(&params, &k, data)
        };
        match JsFuture::from(p.map_err(|e| js_err("AES-GCM", e))?).await {
            Ok(out) => Ok(Ok(buf_to_vec(&out))),
            Err(e) => Ok(Err(js_err("AES-GCM", e))),
        }
    }
}

impl Crypto for WebCryptoBackend {
    async fn sha256(&self, data: &[u8]) -> Result<[u8; 32], String> {
        let s = subtle()?;
        let out = jsp("digest", s.digest_with_str_and_u8_array("SHA-256", data)).await?;
        buf_to_vec(&out)
            .try_into()
            .map_err(|_| "SHA-256 output is 32 bytes".into())
    }

    async fn ed25519_verify(&self, pk: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String> {
        if pk.len() != 32 || sig.len() != 64 {
            return Ok(false);
        }
        let key = match import_raw_str("Ed25519", pk, false, &["verify"]).await? {
            Ok(k) => k,
            Err(import_err) => {
                // Malformed key vs missing algorithm: probe a known-
                // good key (RFC 8032 TEST 1). If even that fails, the
                // browser has no Web Crypto Ed25519 — a backend
                // failure, not a semantic verdict.
                return match import_raw_str("Ed25519", &ED25519_PROBE_PK, false, &["verify"])
                    .await?
                {
                    Ok(_) => Ok(false),
                    Err(_) => Err(format!("Ed25519 unavailable in this browser: {import_err}")),
                };
            }
        };
        let s = subtle()?;
        let out = jsp(
            "Ed25519 verify",
            s.verify_with_str_and_u8_array_and_u8_array("Ed25519", &key, sig, msg),
        )
        .await?;
        Ok(out.as_bool() == Some(true))
    }

    async fn p256_verify(&self, pk_sec1: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String> {
        // The native lane's shape checks, mirrored: uncompressed SEC1
        // only, 64-byte raw signature.
        if pk_sec1.len() != 65 || pk_sec1[0] != 0x04 || sig.len() != 64 {
            return Ok(false);
        }
        // The low-S policy (§3) — WebCrypto verifies high-S
        // signatures, the spec rejects them before curve math.
        if sig[32..] > P256_HALF_N[..] {
            return Ok(false);
        }
        let key = match import_obj("raw", &ec_alg("ECDSA"), pk_sec1, false, &["verify"]).await? {
            Ok(k) => k,
            Err(_) => return Ok(false),
        };
        let params = dict(&[
            ("name", &JsValue::from_str("ECDSA")),
            ("hash", &JsValue::from_str("SHA-256")),
        ]);
        let s = subtle()?;
        let out = jsp(
            "ECDSA verify",
            s.verify_with_object_and_u8_array_and_u8_array(&params, &key, sig, msg),
        )
        .await?;
        Ok(out.as_bool() == Some(true))
    }

    async fn p256_point_valid(&self, sec1: &[u8]) -> Result<bool, String> {
        Ok(import_obj("raw", &ec_alg("ECDH"), sec1, false, &[])
            .await?
            .is_ok())
    }

    async fn p256_pk_of(&self, sk: &[u8; 32]) -> Result<Option<[u8; 65]>, String> {
        match import_p256_scalar(sk).await? {
            Ok(key) => Ok(Some(p256_public_sec1(&key).await?)),
            Err(_) => Ok(None),
        }
    }

    async fn hpke_open(
        &self,
        sk: &[u8; 32],
        enc: &[u8],
        info: &[u8],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        // DHKEM(P-256) enc is exactly an uncompressed SEC1 point (the
        // native EncappedKey::from_bytes contract).
        if enc.len() != 65 {
            return Ok(None);
        }
        let Ok(enc_key) = import_obj("raw", &ec_alg("ECDH"), enc, false, &[]).await? else {
            return Ok(None);
        };
        let Ok(sk_key) = import_p256_scalar(sk).await? else {
            return Ok(None);
        };
        let pk_rm = p256_public_sec1(&sk_key).await?;

        // dh = x-coordinate of skR · pkE.
        let params = dict(&[
            ("name", &JsValue::from_str("ECDH")),
            ("public", enc_key.as_ref()),
        ]);
        let s = subtle()?;
        let dh = match JsFuture::from(
            s.derive_bits_with_object(&params, &sk_key, 256)
                .map_err(|e| js_err("ECDH deriveBits", e))?,
        )
        .await
        {
            Ok(bits) => buf_to_vec(&bits),
            Err(_) => return Ok(None),
        };

        // shared_secret = ExtractAndExpand(dh, enc ‖ pkRm).
        let mut kem_context = Vec::with_capacity(130);
        kem_context.extend_from_slice(enc);
        kem_context.extend_from_slice(&pk_rm);
        let eae_prk = labeled_extract(b"", SUITE_KEM, b"eae_prk", &dh).await?;
        let shared =
            labeled_expand(&eae_prk, SUITE_KEM, b"shared_secret", &kem_context, 32).await?;

        // KeySchedule(mode_base, shared_secret, info, psk="", psk_id="").
        let psk_id_hash = labeled_extract(b"", SUITE_HPKE, b"psk_id_hash", b"").await?;
        let info_hash = labeled_extract(b"", SUITE_HPKE, b"info_hash", info).await?;
        let mut ksc = Vec::with_capacity(65);
        ksc.push(0x00);
        ksc.extend_from_slice(&psk_id_hash);
        ksc.extend_from_slice(&info_hash);
        let secret = labeled_extract(&shared, SUITE_HPKE, b"secret", b"").await?;
        let key: [u8; 32] = labeled_expand(&secret, SUITE_HPKE, b"key", &ksc, 32)
            .await?
            .try_into()
            .expect("32-byte expand");
        let base_nonce: [u8; 12] = labeled_expand(&secret, SUITE_HPKE, b"base_nonce", &ksc, 12)
            .await?
            .try_into()
            .expect("12-byte expand");

        // seq 0 open.
        Ok(self.aes_gcm(false, &key, &base_nonce, aad, ct).await?.ok())
    }

    async fn aes_gcm_seal(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        pt: &[u8],
    ) -> Result<Vec<u8>, String> {
        // A seal over sound inputs must succeed — failure is backend.
        self.aes_gcm(true, key, nonce, aad, pt).await?
    }

    async fn aes_gcm_open(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        Ok(self.aes_gcm(false, key, nonce, aad, ct).await?.ok())
    }

    async fn hkdf_sha256(
        &self,
        salt: &[u8],
        ikm: &[u8],
        info: &[u8],
        out_len: usize,
    ) -> Result<Vec<u8>, String> {
        let key = import_raw_str("HKDF", ikm, false, &["deriveBits"])
            .await?
            .map_err(|e| format!("HKDF ikm: {e}"))?;
        let params = dict(&[
            ("name", &JsValue::from_str("HKDF")),
            ("hash", &JsValue::from_str("SHA-256")),
            ("salt", &bytes_val(salt)),
            ("info", &bytes_val(info)),
        ]);
        let s = subtle()?;
        let out = jsp(
            "HKDF deriveBits",
            s.derive_bits_with_object(&params, &key, (out_len * 8) as u32),
        )
        .await?;
        Ok(buf_to_vec(&out))
    }

    async fn pbkdf2_hmac_sha512(
        &self,
        password: &[u8],
        salt: &[u8],
        iterations: u32,
    ) -> Result<[u8; 64], String> {
        let key = import_raw_str("PBKDF2", password, false, &["deriveBits"])
            .await?
            .map_err(|e| format!("PBKDF2 password: {e}"))?;
        let params = dict(&[
            ("name", &JsValue::from_str("PBKDF2")),
            ("hash", &JsValue::from_str("SHA-512")),
            ("salt", &bytes_val(salt)),
            ("iterations", &JsValue::from_f64(iterations as f64)),
        ]);
        let s = subtle()?;
        let out = jsp(
            "PBKDF2 deriveBits",
            s.derive_bits_with_object(&params, &key, 512),
        )
        .await?;
        buf_to_vec(&out)
            .try_into()
            .map_err(|_| "PBKDF2 output is 64 bytes".into())
    }

    async fn ed25519_pk_of_seed(&self, seed: &[u8; 32]) -> Result<[u8; 32], String> {
        let mut pkcs8 = ED25519_PKCS8_PREFIX.to_vec();
        pkcs8.extend_from_slice(seed);
        let s = subtle()?;
        let data = Uint8Array::from(pkcs8.as_slice());
        let key: CryptoKey = jsp(
            "Ed25519 pkcs8 import",
            s.import_key_with_str("pkcs8", &data, "Ed25519", true, &usages(&["sign"])),
        )
        .await?
        .unchecked_into();
        let x = jwk_field(&key, "x").await?;
        x.try_into().map_err(|_| "JWK x is 32 bytes".into())
    }
}
