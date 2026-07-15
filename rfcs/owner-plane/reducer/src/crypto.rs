//! The §13.2 crypto seam (execution-lanes-plan lane 1, work item 1).
//!
//! Every primitive the BROWSER-required families (1–5, 8) invoke in
//! their KAT lanes sits behind the [`Crypto`] trait: the §13.2
//! browser column's "import/sign/verify/encrypt/decrypt paths" —
//! SHA-256 digests, Ed25519/P-256 verification (low-S policy
//! included), P-256 point/scalar checks, HPKE open
//! (DHKEM(P-256)+HKDF-SHA256+AES-256-GCM), AES-GCM seal/open,
//! HKDF-SHA256, PBKDF2-HMAC-SHA512, and Ed25519 seed→pk. The trait is
//! deliberately NO WIDER than the browser-annotated corpus needs:
//! native-only lanes (family-3 sign-then-verify — the companion
//! schema bars `browser` there because WebCrypto cannot inject
//! signing randomness — plus the non-browser families 6/11/14) keep
//! their direct crate calls, and the fold/journal/erase ENGINES keep
//! theirs (families 6–7/9–12 have no §13.2 browser requirement; the
//! family-13 browser row is the IndexedDB Txn subset, a storage
//! substrate concern, not engine crypto).
//!
//! Maybe-async: the methods are `async fn` so a WebCrypto
//! implementation (SubtleCrypto is Promise-based) can await, while
//! [`NativeCrypto`] resolves every future immediately from the same
//! third-party crates the reducer always used — [`block_on_ready`]
//! drives those to completion with a single poll and treats a yield
//! as a bug. Failure vocabulary: `Err(String)` means the BACKEND
//! failed (an unavailable algorithm, a thrown WebCrypto exception) —
//! a lane maps it to a harness-level error; a semantic "no" (bad
//! signature, failed AEAD open, invalid point) is `Ok(false)` /
//! `Ok(None)`, exactly the distinction the lanes assert on.

use sha2::{Digest, Sha256};

/// The browser-required primitive set. Async-in-trait without a
/// `Send` bound is deliberate: the lanes are single-threaded on both
/// surfaces (the CLI harness runs vectors sequentially; wasm is
/// single-threaded and its futures are `!Send`).
#[allow(async_fn_in_trait)]
pub trait Crypto {
    /// Plain SHA-256 (domain framing is the caller's job — the lanes
    /// build `msg(tag, x)` themselves so the digest primitive stays
    /// exactly WebCrypto's `digest("SHA-256", …)`).
    async fn sha256(&self, data: &[u8]) -> Result<[u8; 32], String>;

    /// Ed25519 strict verification. Malformed pk/sig lengths or
    /// encodings are a semantic `Ok(false)`, never `Err`.
    async fn ed25519_verify(&self, pk: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String>;

    /// ECDSA-P256 over SHA-256 with the spec's low-S policy: a
    /// high-S signature is `Ok(false)` BEFORE curve math, matching
    /// §3's malleability rejection.
    async fn p256_verify(&self, pk_sec1: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String>;

    /// Is the SEC1 encoding a valid, non-identity P-256 point?
    async fn p256_point_valid(&self, sec1: &[u8]) -> Result<bool, String>;

    /// P-256 scalar → SEC1 uncompressed public key (`None` = invalid
    /// scalar).
    async fn p256_pk_of(&self, sk: &[u8; 32]) -> Result<Option<[u8; 65]>, String>;

    /// HPKE single-shot open, base mode, DHKEM(P-256, HKDF-SHA256) +
    /// HKDF-SHA256 + AES-256-GCM (`hpke-p256-v1`). `None` = the open
    /// failed (bad enc, AEAD failure).
    async fn hpke_open(
        &self,
        sk: &[u8; 32],
        enc: &[u8],
        info: &[u8],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String>;

    /// AES-256-GCM seal with an explicit nonce (the corpus's KAT
    /// wrappers fix nonces; determinism is the point).
    async fn aes_gcm_seal(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        pt: &[u8],
    ) -> Result<Vec<u8>, String>;

    /// AES-256-GCM open; `None` = authentication failure.
    async fn aes_gcm_open(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String>;

    /// HKDF-SHA256 extract-then-expand (salt, ikm, info → out_len
    /// bytes) — the exact shape of WebCrypto's HKDF `deriveBits`.
    async fn hkdf_sha256(
        &self,
        salt: &[u8],
        ikm: &[u8],
        info: &[u8],
        out_len: usize,
    ) -> Result<Vec<u8>, String>;

    /// PBKDF2-HMAC-SHA512 → 64 bytes (§2.4's fixed seed width).
    async fn pbkdf2_hmac_sha512(
        &self,
        password: &[u8],
        salt: &[u8],
        iterations: u32,
    ) -> Result<[u8; 64], String>;

    /// Ed25519 32-byte seed → public key (§2.4's final derivation
    /// step).
    async fn ed25519_pk_of_seed(&self, seed: &[u8; 32]) -> Result<[u8; 32], String>;
}

/// Drive a future that never actually yields (every [`NativeCrypto`]
/// method resolves immediately). A `Pending` is a wiring bug — an
/// async backend leaked onto the sync harness path — and panics.
pub fn block_on_ready<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!(
            "block_on_ready: the future yielded — native Crypto futures must be immediately ready"
        ),
    }
}

/// The native backend: the same third-party primitive crates the
/// reducer has always used (shared DEPENDENCIES, not shared code —
/// the differential stance is unchanged), behind immediately-ready
/// futures.
pub struct NativeCrypto;

impl Crypto for NativeCrypto {
    async fn sha256(&self, data: &[u8]) -> Result<[u8; 32], String> {
        Ok(Sha256::digest(data).into())
    }

    async fn ed25519_verify(&self, pk: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String> {
        let Ok(pk32) = <[u8; 32]>::try_from(pk) else {
            return Ok(false);
        };
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk32) else {
            return Ok(false);
        };
        let Ok(sig64) = <[u8; 64]>::try_from(sig) else {
            return Ok(false);
        };
        Ok(vk
            .verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig64))
            .is_ok())
    }

    async fn p256_verify(&self, pk_sec1: &[u8], msg: &[u8], sig: &[u8]) -> Result<bool, String> {
        Ok(p256_sig::verify(pk_sec1, msg, sig))
    }

    async fn p256_point_valid(&self, sec1: &[u8]) -> Result<bool, String> {
        Ok(hpke_ops::point_valid(sec1))
    }

    async fn p256_pk_of(&self, sk: &[u8; 32]) -> Result<Option<[u8; 65]>, String> {
        Ok(hpke_ops::pk_of(sk))
    }

    async fn hpke_open(
        &self,
        sk: &[u8; 32],
        enc: &[u8],
        info: &[u8],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        Ok(hpke_ops::open(sk, enc, info, aad, ct))
    }

    async fn aes_gcm_seal(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        pt: &[u8],
    ) -> Result<Vec<u8>, String> {
        Ok(item_crypto::aead_seal(key, nonce, aad, pt))
    }

    async fn aes_gcm_open(
        &self,
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        Ok(item_crypto::aead_open(key, nonce, aad, ct))
    }

    async fn hkdf_sha256(
        &self,
        salt: &[u8],
        ikm: &[u8],
        info: &[u8],
        out_len: usize,
    ) -> Result<Vec<u8>, String> {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(salt), ikm);
        let mut okm = vec![0u8; out_len];
        hk.expand(info, &mut okm)
            .map_err(|_| format!("HKDF output length {out_len} out of bounds"))?;
        Ok(okm)
    }

    async fn pbkdf2_hmac_sha512(
        &self,
        password: &[u8],
        salt: &[u8],
        iterations: u32,
    ) -> Result<[u8; 64], String> {
        let mut seed = [0u8; 64];
        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, iterations, &mut seed);
        Ok(seed)
    }

    async fn ed25519_pk_of_seed(&self, seed: &[u8; 32]) -> Result<[u8; 32], String> {
        Ok(ed25519_dalek::SigningKey::from_bytes(seed)
            .verifying_key()
            .to_bytes())
    }
}

/// P-256 signing/verification helpers (§3). `verify` is the
/// low-S-enforcing native path [`NativeCrypto::p256_verify`] rides;
/// `sign_low_s` serves the native-only sign-then-verify lane.
pub(crate) mod p256_sig {
    use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
    use sha2::{Digest, Sha256};

    pub fn sign_low_s(sk: &[u8; 32], msg: &[u8]) -> Option<[u8; 64]> {
        let sk = SigningKey::from_bytes(sk.into()).ok()?;
        let digest = Sha256::digest(msg);
        let sig: Signature = sk.sign_prehash(&digest).ok()?;
        let sig = sig.normalize_s().unwrap_or(sig);
        Some(sig.to_bytes().into())
    }

    /// High-S rejected before curve math.
    pub fn verify(pk_sec1: &[u8], msg: &[u8], sig: &[u8]) -> bool {
        if pk_sec1.len() != 65 || pk_sec1[0] != 0x04 {
            return false;
        }
        let Ok(vk) = VerifyingKey::from_sec1_bytes(pk_sec1) else {
            return false;
        };
        let Ok(sig) = Signature::from_slice(sig) else {
            return false;
        };
        if sig.normalize_s().is_some() {
            return false;
        }
        let digest = Sha256::digest(msg);
        vk.verify_prehash(&digest, &sig).is_ok()
    }
}

/// HPKE (`hpke-p256-v1`) helpers (§5.2).
pub(crate) mod hpke_ops {
    use hpke::aead::AesGcm256;
    use hpke::kdf::HkdfSha256;
    use hpke::kem::DhP256HkdfSha256;
    use hpke::{Deserializable, Kem as KemTrait, OpModeR};
    type Kem = DhP256HkdfSha256;

    pub fn open(sk: &[u8; 32], enc: &[u8], info: &[u8], aad: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        let sk = <Kem as KemTrait>::PrivateKey::from_bytes(sk).ok()?;
        let enc = <Kem as KemTrait>::EncappedKey::from_bytes(enc).ok()?;
        hpke::single_shot_open::<AesGcm256, HkdfSha256, Kem>(
            &OpModeR::Base,
            &sk,
            &enc,
            info,
            ct,
            aad,
        )
        .ok()
    }

    /// Is the SEC1 encoding a valid, non-identity P-256 point?
    pub fn point_valid(enc: &[u8]) -> bool {
        p256::PublicKey::from_sec1_bytes(enc).is_ok()
    }

    /// sk (32-byte scalar) → SEC1 uncompressed pk.
    pub fn pk_of(sk: &[u8; 32]) -> Option<[u8; 65]> {
        let sk = p256::SecretKey::from_slice(sk).ok()?;
        let pk = sk.public_key().to_sec1_bytes();
        pk.as_ref().try_into().ok()
    }
}

/// The §5.3 key schedule's SHARED PIECES: AAD framing (pure bytes)
/// plus the native AES-GCM primitives. The framing helpers serve
/// BOTH the trait-routed family-5 lanes (AAD construction is not
/// crypto) and the native-only family-14 re-encapsulation lane; the
/// AEAD functions back [`NativeCrypto`] and the native-only lanes.
/// The wrap-key/wrap-dek COMPOSITIONS live in the lanes themselves
/// now (HKDF + seal through the backend), not here.
pub(crate) mod item_crypto {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::Aes256Gcm;

    pub const F5_PLANE: [u8; 32] = [0; 32];
    pub const F5_ZONE: [u8; 16] = [0; 16];
    pub const WRAPKEY_SALT: &[u8] = b"intendant/wrapkey/v1";

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        v
    }

    pub fn item_aad(plane: &[u8; 32], zone: &[u8; 16]) -> Vec<u8> {
        concat(&[b"intendant/item/v1", &[0x00], plane, zone])
    }

    pub fn dekwrap_aad(
        plane: &[u8; 32],
        zone: &[u8; 16],
        epoch: u64,
        item_addr: &[u8; 32],
    ) -> Vec<u8> {
        concat(&[
            b"intendant/dekwrap/v1",
            &[0x00],
            plane,
            zone,
            &epoch.to_be_bytes(),
            item_addr,
        ])
    }

    pub fn aead_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], pt: &[u8]) -> Vec<u8> {
        Aes256Gcm::new(key.into())
            .encrypt(&(*nonce).into(), Payload { msg: pt, aad })
            .expect("AES-GCM seal")
    }

    pub fn aead_open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        Aes256Gcm::new(key.into())
            .decrypt(&(*nonce).into(), Payload { msg: ct, aad })
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native backend agrees with the crates it delegates to on
    /// a round trip (a wiring pin, not a crypto test — the 157-vector
    /// harness is the real coverage).
    #[test]
    fn native_backend_wiring() {
        let c = NativeCrypto;
        // sha256 = plain sha2.
        let got = block_on_ready(c.sha256(b"seam")).unwrap();
        let want: [u8; 32] = Sha256::digest(b"seam").into();
        assert_eq!(got, want);

        // ed25519: sign with dalek, verify through the trait; then
        // tamper.
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let sig = sk.sign(b"msg").to_bytes();
        assert!(block_on_ready(c.ed25519_verify(&pk, b"msg", &sig)).unwrap());
        assert!(!block_on_ready(c.ed25519_verify(&pk, b"other", &sig)).unwrap());
        assert!(!block_on_ready(c.ed25519_verify(&pk[..31], b"msg", &sig)).unwrap());

        // seed → pk matches dalek.
        assert_eq!(
            block_on_ready(c.ed25519_pk_of_seed(&[7u8; 32])).unwrap(),
            pk
        );

        // p256: low-S sign verifies; a high-S mutation must not.
        let sig = p256_sig::sign_low_s(&[9u8; 32], b"msg").unwrap();
        let pk = hpke_ops::pk_of(&[9u8; 32]).unwrap();
        assert!(block_on_ready(c.p256_verify(&pk, b"msg", &sig)).unwrap());
        assert!(!block_on_ready(c.p256_verify(&pk, b"other", &sig)).unwrap());

        // AES-GCM round trip + auth failure; HKDF matches wrap_key.
        let key = [3u8; 32];
        let ct = block_on_ready(c.aes_gcm_seal(&key, &[0u8; 12], b"aad", b"pt")).unwrap();
        assert_eq!(
            block_on_ready(c.aes_gcm_open(&key, &[0u8; 12], b"aad", &ct)).unwrap(),
            Some(b"pt".to_vec())
        );
        assert_eq!(
            block_on_ready(c.aes_gcm_open(&key, &[0u8; 12], b"bad", &ct)).unwrap(),
            None
        );
        let addr = [5u8; 32];
        let hk = hkdf::Hkdf::<Sha256>::new(Some(item_crypto::WRAPKEY_SALT), &key);
        let mut want32 = [0u8; 32];
        hk.expand(&addr, &mut want32).unwrap();
        assert_eq!(
            block_on_ready(c.hkdf_sha256(item_crypto::WRAPKEY_SALT, &key, &addr, 32)).unwrap(),
            want32.to_vec()
        );
    }

    /// A yielding future is a wiring bug on the native path.
    #[test]
    #[should_panic(expected = "future yielded")]
    fn block_on_ready_rejects_pending() {
        struct Never;
        impl std::future::Future for Never {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                std::task::Poll::Pending
            }
        }
        block_on_ready(Never);
    }
}
