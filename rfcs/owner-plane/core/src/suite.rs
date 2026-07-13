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
