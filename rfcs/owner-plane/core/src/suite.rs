//! Suite `suite-v1` — spec §2.1–§2.3.
//!
//! Explicit-input crypto only: every key, nonce, and seed is a
//! parameter (vector RNG draws happen in the scenario builders, never
//! inside this module), so minted bytes are portable by construction.
//! Pure-Rust primitives (S4): `ed25519-dalek`, `p256`, `aes-gcm`.

use crate::cbor::{self, Value};
use crate::domains::{h_tag, msg, Tag};

/// `key_id = H_key({alg, pk})` — the one key identity (§2.2): the
/// canonical map `{alg: <suite alg id>, pk: <encoded public key>}`
/// hashed under the `key` domain. Never raw key bytes.
pub fn key_id(alg: &str, pk: &[u8]) -> [u8; 32] {
    let m = cbor::map(vec![
        ("alg", Value::Text(alg.to_string())),
        ("pk", Value::Bytes(pk.to_vec())),
    ]);
    let enc = cbor::encode(&m).expect("two distinct text keys, depth 2");
    h_tag(Tag::Key, &enc)
}

/// Ed25519 (RFC 8032) — native/daemon/recovery signatures. 32-byte
/// seed and public key, 64-byte signature; signing is deterministic,
/// so vectors draw seeds, never per-signature randomness.
pub mod ed25519 {
    use super::{msg, Tag};
    use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

    pub fn keypair(seed: &[u8; 32]) -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(seed);
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    /// Sign `x` under the domain frame: `sig = Sign(msg(tag, x))`.
    pub fn sign(sk: &SigningKey, tag: Tag, x: &[u8]) -> [u8; 64] {
        sk.sign(&msg(tag, x)).to_bytes()
    }

    /// Verify a domain-framed signature. Strict verification
    /// (`verify_strict`): the exact acceptance set for edge-case
    /// encodings is what family-2 vectors pin — divergence between
    /// implementations here is a known audit target.
    pub fn verify(pk: &[u8; 32], tag: Tag, x: &[u8], sig: &[u8; 64]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(pk) else {
            return false;
        };
        let sig = Signature::from_bytes(sig);
        vk.verify_strict(&msg(tag, x), &sig).is_ok()
    }
}

/// ECDSA P-256 / SHA-256, low-S (browser-held keys). Emitters
/// normalize to low-S; validators REJECT high-S (S1 — malleable S
/// would mint byte-different valid duplicates). Signing here is
/// RFC 6979 deterministic — core mints the "fixed signatures to
/// verify" that browser vectors carry (§13.1); browser surfaces never
/// take signing draws.
pub mod ecdsa_p256 {
    use super::{msg, Tag};
    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
    use sha2::{Digest, Sha256};

    pub fn keypair(scalar: &[u8; 32]) -> Option<(SigningKey, [u8; 65])> {
        let sk = SigningKey::from_bytes(scalar.into()).ok()?;
        let pk = sk.verifying_key().to_encoded_point(false);
        Some((sk, pk.as_bytes().try_into().expect("SEC1 uncompressed")))
    }

    /// Deterministic (RFC 6979) low-S signature over `msg(tag, x)`,
    /// raw `r‖s` big-endian.
    pub fn sign(sk: &SigningKey, tag: Tag, x: &[u8]) -> [u8; 64] {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        let digest = Sha256::digest(msg(tag, x));
        let sig: Signature = sk.sign_prehash(&digest).expect("RFC 6979 signing");
        let sig = sig.normalize_s().unwrap_or(sig);
        sig.to_bytes().into()
    }

    /// Verify: SEC1-uncompressed key (on-curve, non-identity — else
    /// `key-malformed`), raw `r‖s` signature, HIGH-S REJECTED
    /// (`sig-invalid`) before curve math.
    pub fn verify(pk_sec1: &[u8], tag: Tag, x: &[u8], sig: &[u8]) -> bool {
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
            return false; // high-S: emitters normalize, validators reject
        }
        let digest = Sha256::digest(msg(tag, x));
        vk.verify_prehash(&digest, &sig).is_ok()
    }
}

/// HPKE recipient wrap `hpke-p256-v1` (§2.1): RFC 9180 base mode,
/// DHKEM(P-256, HKDF-SHA256), HKDF-SHA256, AES-256-GCM.
///
/// Deterministic minting: the ephemeral keypair derives from ONE named
/// 32-byte vector draw via RFC 9180 `DeriveKeyPair` — `seal` feeds the
/// draw through an adapter that panics on any other consumption
/// pattern, so a dependency behavior change fails loudly instead of
/// shipping unportable fixtures.
pub mod hpke_wrap {
    use hpke::aead::AesGcm256;
    use hpke::kdf::HkdfSha256;
    use hpke::kem::DhP256HkdfSha256;
    use hpke::{Deserializable, Kem as KemTrait, OpModeR, OpModeS, Serializable};

    type Kem = DhP256HkdfSha256;

    /// Serves exactly one pre-named draw to the hpke crate, then
    /// panics — the drift alarm for the "seal = one 32-byte ephemeral
    /// ikm draw" portability contract.
    struct IkmRng {
        bytes: Vec<u8>,
        consumed: usize,
    }

    impl IkmRng {
        fn new(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                consumed: 0,
            }
        }
        fn fully_consumed(&self) -> bool {
            self.consumed == self.bytes.len()
        }
    }

    // rand_core 0.10: implementing infallible `TryRng` + the
    // `TryCryptoRng` marker makes the `Rng`/`CryptoRng` blankets apply.
    impl rand_core::TryRng for IkmRng {
        type Error = core::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            let mut b = [0u8; 4];
            self.try_fill_bytes(&mut b)?;
            Ok(u32::from_le_bytes(b))
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            let mut b = [0u8; 8];
            self.try_fill_bytes(&mut b)?;
            Ok(u64::from_le_bytes(b))
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            let end = self.consumed + dst.len();
            assert!(
                end <= self.bytes.len(),
                "hpke drew more than the named ikm draw ({} > {} bytes): \
                 the ephemeral-derivation contract changed",
                end,
                self.bytes.len()
            );
            dst.copy_from_slice(&self.bytes[self.consumed..end]);
            self.consumed = end;
            Ok(())
        }
    }
    impl rand_core::TryCryptoRng for IkmRng {}

    /// RFC 9180 `DeriveKeyPair` for DHKEM(P-256): 32-byte ikm →
    /// (scalar bytes, SEC1-uncompressed public key).
    pub fn derive_keypair(ikm: &[u8; 32]) -> ([u8; 32], [u8; 65]) {
        let (sk, pk) = <Kem as KemTrait>::derive_keypair(ikm);
        let sk: [u8; 32] = sk.to_bytes().as_slice().try_into().expect("32-byte scalar");
        let pk: [u8; 65] = pk
            .to_bytes()
            .as_slice()
            .try_into()
            .expect("SEC1 uncompressed");
        (sk, pk)
    }

    /// Base-mode single-shot seal; the ephemeral keypair =
    /// `DeriveKeyPair(eph_ikm)`. Returns `(enc, ciphertext ‖ tag)` —
    /// for a 32-byte KEK plaintext that is the kekwrap's
    /// `(enc: 65 B, ct: 48 B)`. `None` = malformed recipient key
    /// (`key-malformed`).
    pub fn seal(
        pk_recip_sec1: &[u8; 65],
        info: &[u8],
        aad: &[u8],
        pt: &[u8],
        eph_ikm: &[u8; 32],
    ) -> Option<([u8; 65], Vec<u8>)> {
        let pk = <Kem as KemTrait>::PublicKey::from_bytes(pk_recip_sec1).ok()?;
        let mut rng = IkmRng::new(eph_ikm);
        let (enc, ct) = hpke::single_shot_seal_with_rng::<AesGcm256, HkdfSha256, Kem>(
            &OpModeS::Base,
            &pk,
            info,
            pt,
            aad,
            &mut rng,
        )
        .ok()?;
        assert!(
            rng.fully_consumed(),
            "hpke drew fewer bytes than the named ikm draw: \
             the ephemeral-derivation contract changed"
        );
        let enc: [u8; 65] = enc.to_bytes().as_slice().try_into().expect("SEC1 enc");
        Some((enc, ct))
    }

    /// Base-mode single-shot open. `None` covers malformed keys and
    /// AEAD failure alike — shape code maps the outcome.
    pub fn open(
        sk_recip: &[u8; 32],
        enc: &[u8],
        info: &[u8],
        aad: &[u8],
        ct_and_tag: &[u8],
    ) -> Option<Vec<u8>> {
        let sk = <Kem as KemTrait>::PrivateKey::from_bytes(sk_recip).ok()?;
        let enc = <Kem as KemTrait>::EncappedKey::from_bytes(enc).ok()?;
        hpke::single_shot_open::<AesGcm256, HkdfSha256, Kem>(
            &OpModeR::Base,
            &sk,
            &enc,
            info,
            ct_and_tag,
            aad,
        )
        .ok()
    }
}

/// AES-256-GCM content AEAD (`a256gcm`): 32-byte key, 12-byte nonce,
/// detached use per the §3 shapes — this module returns/consumes
/// `ciphertext ‖ tag(16)` as one buffer; shape code slices per CDDL.
pub mod aead {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], pt: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new(key.into());
        cipher
            .encrypt(Nonce::from_slice(nonce), Payload { msg: pt, aad })
            .expect("AES-GCM seal is total for in-range lengths")
    }

    pub fn open(
        key: &[u8; 32],
        nonce: &[u8; 12],
        aad: &[u8],
        ct_and_tag: &[u8],
    ) -> Option<Vec<u8>> {
        let cipher = Aes256Gcm::new(key.into());
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct_and_tag,
                    aad,
                },
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc8032_test1_empty_message() {
        let seed: [u8; 32] =
            unhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .try_into()
                .unwrap();
        let (sk, pk) = ed25519::keypair(&seed);
        assert_eq!(
            hex(&pk),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        // Raw RFC vector (no domain frame) — pins the primitive.
        use ed25519_dalek::Signer;
        assert_eq!(
            hex(&sk.sign(b"").to_bytes()),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bac\
             c61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn rfc8032_test2_one_byte() {
        let seed: [u8; 32] =
            unhex("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
                .try_into()
                .unwrap();
        let (sk, pk) = ed25519::keypair(&seed);
        assert_eq!(
            hex(&pk),
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
        );
        use ed25519_dalek::Signer;
        assert_eq!(
            hex(&sk.sign(&[0x72]).to_bytes()),
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e\
             458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
        );
    }

    #[test]
    fn ed25519_domain_framing_binds_the_tag() {
        let (sk, pk) = ed25519::keypair(&[7u8; 32]);
        let sig = ed25519::sign(&sk, Tag::Op, b"header");
        assert!(ed25519::verify(&pk, Tag::Op, b"header", &sig));
        assert!(!ed25519::verify(&pk, Tag::Body, b"header", &sig));
        assert!(!ed25519::verify(&pk, Tag::Op, b"headerx", &sig));
    }

    #[test]
    fn p256_rfc6979_sample_low_s() {
        // RFC 6979 A.2.5, P-256 + SHA-256, message "sample". The RFC's
        // deterministic signature is HIGH-S; suite-v1 emitters
        // normalize, so we pin r verbatim and s = n − s_rfc via the
        // crate's own normalize (no hand arithmetic).
        let x: [u8; 32] = unhex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721")
            .try_into()
            .unwrap();
        let (sk, pk) = ecdsa_p256::keypair(&x).expect("in-range scalar");
        assert_eq!(
            hex(&pk),
            "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6\
             7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299"
        );

        // Sign the bare message (no frame) to compare against the RFC.
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(b"sample");
        let raw: p256::ecdsa::Signature = sk.sign_prehash(&digest).unwrap();
        let rfc = p256::ecdsa::Signature::from_slice(&unhex(
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716\
             f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8",
        ))
        .unwrap();
        assert_eq!(raw, rfc, "RFC 6979 determinism");
        let low = rfc.normalize_s().expect("RFC sample signature is high-S");
        assert_eq!(raw.normalize_s().unwrap(), low);

        // Our framed path emits low-S and verifies; the high-S twin of
        // the SAME framed message must be rejected.
        let sig = ecdsa_p256::sign(&sk, Tag::Op, b"m");
        let parsed = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        assert!(parsed.normalize_s().is_none(), "emitted signature is low-S");
        assert!(ecdsa_p256::verify(&pk, Tag::Op, b"m", &sig));
    }

    #[test]
    fn p256_high_s_and_malformed_keys_reject() {
        let (sk, pk) = ecdsa_p256::keypair(&[9u8; 32]).unwrap();
        let sig = ecdsa_p256::sign(&sk, Tag::Op, b"m");

        // Forge the high-S twin: s' = n − s (negate via the crate).
        let parsed = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        let (r, s) = (parsed.r(), parsed.s());
        let high = p256::ecdsa::Signature::from_scalars(*r.as_ref(), -*s.as_ref()).unwrap();
        assert!(high.normalize_s().is_some(), "twin really is high-S");
        assert!(!ecdsa_p256::verify(&pk, Tag::Op, b"m", &high.to_bytes()));

        // Malformed keys: wrong length, wrong prefix, identity, off-curve.
        assert!(!ecdsa_p256::verify(&pk[..64], Tag::Op, b"m", &sig));
        let mut compressed_prefix = pk;
        compressed_prefix[0] = 0x02;
        assert!(!ecdsa_p256::verify(&compressed_prefix, Tag::Op, b"m", &sig));
        let identity = [0u8; 65];
        assert!(!ecdsa_p256::verify(&identity, Tag::Op, b"m", &sig));
        let mut off_curve = pk;
        off_curve[64] ^= 1;
        assert!(!ecdsa_p256::verify(&off_curve, Tag::Op, b"m", &sig));
    }

    #[test]
    fn gcm_spec_test_case_16() {
        // McGrew/Viega GCM spec test case 16 (AES-256, AAD, 60-byte PT).
        let key: [u8; 32] =
            unhex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
                .try_into()
                .unwrap();
        let nonce: [u8; 12] = unhex("cafebabefacedbaddecaf888").try_into().unwrap();
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let pt = unhex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let sealed = aead::seal(&key, &nonce, &aad, &pt);
        assert_eq!(
            hex(&sealed),
            "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa\
             8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662\
             76fc6ece0f4e1768cddf8853bb2d551b"
        );
        assert_eq!(
            aead::open(&key, &nonce, &aad, &sealed).as_deref(),
            Some(&pt[..])
        );
        // Tampered AAD fails closed.
        assert_eq!(aead::open(&key, &nonce, b"x", &sealed), None);
    }

    #[test]
    fn rfc9180_p256_aes256gcm_base_mode_vector() {
        // The official CFRG test vector for EXACTLY the suite-v1 wrap
        // ciphersuite: mode 0 (base), kem 0x0010 DHKEM(P-256,
        // HKDF-SHA256), kdf 0x0001, aead 0x0002 AES-256-GCM
        // (draft-irtf-cfrg-hpke test-vectors.json; the RFC's appendix
        // prints only the AES-128 member of this KEM family). Pins
        // DeriveKeyPair, the one-draw ephemeral path, and the
        // info/aad argument wiring.
        let ikm_e: [u8; 32] =
            unhex("a90d3417c3da9cb6c6ae19b4b5dd6cc9529a4cc24efb7ae0ace1f31887a8cd6c")
                .try_into()
                .unwrap();
        let ikm_r: [u8; 32] =
            unhex("a0ce15d49e28bd47a18a97e147582d814b08cbe00109fed5ec27d1b4e9f6f5e3")
                .try_into()
                .unwrap();
        let info = unhex("4f6465206f6e2061204772656369616e2055726e");
        let aad = unhex("436f756e742d30");
        let pt = unhex("4265617574792069732074727574682c20747275746820626561757479");

        let (sk_r, pk_r) = hpke_wrap::derive_keypair(&ikm_r);
        assert_eq!(
            hex(&sk_r),
            "317f915db7bc629c48fe765587897e01e282d3e8445f79f27f65d031a88082b2"
        );
        assert_eq!(
            hex(&pk_r),
            "04abc7e49a4c6b3566d77d0304addc6ed0e98512ffccf505e6a8e3eb25c685136f\
             853148544876de76c0f2ef99cdc3a05ccf5ded7860c7c021238f9e2073d2356c"
        );

        let (enc, ct) = hpke_wrap::seal(&pk_r, &info, &aad, &pt, &ikm_e).unwrap();
        assert_eq!(
            hex(&enc),
            "04c06b4f6bebc7bb495cb797ab753f911aff80aefb86fd8b6fcc35525f3ab5f03e\
             0b21bd31a86c6048af3cb2d98e0d3bf01da5cc4c39ff5370d331a4f1f7d5a4e0"
        );
        assert_eq!(
            hex(&ct),
            "58c61a45059d0c5704560e9d88b564a8b63f1364b8d1fcb3c4c6ddc1d2917424\
             65e902cd216f8908da49f8f96f"
        );
        assert_eq!(
            hpke_wrap::open(&sk_r, &enc, &info, &aad, &ct).as_deref(),
            Some(&pt[..])
        );
    }

    #[test]
    fn hpke_wrap_kekwrap_shape_and_failure_modes() {
        let (sk_r, pk_r) = hpke_wrap::derive_keypair(&[5u8; 32]);
        let kek = [0x42u8; 32];
        let (enc, ct) = hpke_wrap::seal(&pk_r, b"info", b"aad", &kek, &[6u8; 32]).unwrap();
        // The kekwrap CDDL sizes: enc 65 B, ct 48 B (32-byte KEK + tag).
        assert_eq!(enc.len(), 65);
        assert_eq!(ct.len(), 48);
        // Deterministic given the same draw; distinct under another.
        let again = hpke_wrap::seal(&pk_r, b"info", b"aad", &kek, &[6u8; 32]).unwrap();
        assert_eq!((enc, ct.clone()), again);
        let other = hpke_wrap::seal(&pk_r, b"info", b"aad", &kek, &[7u8; 32]).unwrap();
        assert_ne!(enc, other.0);
        // Open fails closed on wrong aad/info/key or tampered bytes.
        assert_eq!(
            hpke_wrap::open(&sk_r, &enc, b"info", b"aad", &ct).as_deref(),
            Some(&kek[..])
        );
        assert!(hpke_wrap::open(&sk_r, &enc, b"info", b"x", &ct).is_none());
        assert!(hpke_wrap::open(&sk_r, &enc, b"x", b"aad", &ct).is_none());
        let (sk_other, _) = hpke_wrap::derive_keypair(&[8u8; 32]);
        assert!(hpke_wrap::open(&sk_other, &enc, b"info", b"aad", &ct).is_none());
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(hpke_wrap::open(&sk_r, &enc, b"info", b"aad", &bad).is_none());
        // Malformed recipient keys refuse to seal (key-malformed).
        let mut off_curve = pk_r;
        off_curve[64] ^= 1;
        assert!(hpke_wrap::seal(&off_curve, b"info", b"aad", &kek, &[6u8; 32]).is_none());
        assert!(hpke_wrap::seal(&[0u8; 65], b"info", b"aad", &kek, &[6u8; 32]).is_none());
    }

    #[test]
    fn key_id_is_the_framed_map_hash() {
        let pk = [0xabu8; 32];
        let expected = {
            let m = cbor::map(vec![
                ("alg", Value::Text("ed25519".into())),
                ("pk", Value::Bytes(pk.to_vec())),
            ]);
            h_tag(Tag::Key, &cbor::encode(&m).unwrap())
        };
        assert_eq!(key_id("ed25519", &pk), expected);
        // Role/alg separation: same bytes, different alg id → different identity.
        assert_ne!(key_id("ed25519", &pk), key_id("p256", &pk));
    }
}
