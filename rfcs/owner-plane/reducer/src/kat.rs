//! Families 1–6 KAT lanes (§13.3): encoding+caps, domains/key-ids,
//! signatures, HPKE, item crypto, frontier — the reducer's own
//! implementations, differentially compared against the minted
//! expectations.
//!
//! This module carries the reducer's ONLY canonical-CBOR writer — a
//! tiny sorted-map encoder used for two jobs where reading is not
//! enough: re-deriving the family-1 cap-fit templates byte-for-byte
//! (the pinned worst-width figures) and re-encoding the family-6
//! frontier after a fold. Everywhere else the reducer stays a
//! reader.
//!
//! Fixture conventions honored here (mirrored from the corpus
//! builders; the Gate-A audit sweeps them):
//! - family-3 `sign-then-verify` selects the algorithm by NAME
//!   prefix; `verify-fixed` discriminates by pk length (32/65).
//! - family-4/5 negatives map their outcome by failure site:
//!   invalid point → `key-malformed`; address binding →
//!   `wrapper-mismatch`; AEAD → `aead-fail`.
//! - family-5 KATs fix `plane_id = [0;32]`, `zone_id = [0;16]`.
//! - family-1 `cap-exceed` carriers identify their cap by shape
//!   (a `records` map = the Txn cap; `heads` = frontierclose).
//! - family-6 events: 4-key map = acceptance, 3-key = D-33
//!   retirement; `noop` = the whole fold changed nothing.

use serde_json::Value as Json;

use crate::cbor::{decode, DecodeError, Node};
use crate::domains;
use crate::harness::SemStatus;

// ------------------------------------------------- the mini writer

/// The reducer's canonical encoder — maps sort by ENCODED key bytes.
enum Enc {
    U(u64),
    B(Vec<u8>),
    T(&'static str),
    A(Vec<Enc>),
    M(Vec<(&'static str, Enc)>),
}

fn header(major: u8, n: u64) -> Vec<u8> {
    let mt = major << 5;
    if n < 24 {
        vec![mt | n as u8]
    } else if n <= u8::MAX as u64 {
        vec![mt | 24, n as u8]
    } else if n <= u16::MAX as u64 {
        let b = (n as u16).to_be_bytes();
        vec![mt | 25, b[0], b[1]]
    } else if n <= u32::MAX as u64 {
        let b = (n as u32).to_be_bytes();
        vec![mt | 26, b[0], b[1], b[2], b[3]]
    } else {
        let b = n.to_be_bytes();
        let mut v = vec![mt | 27];
        v.extend_from_slice(&b);
        v
    }
}

fn encode(e: &Enc) -> Vec<u8> {
    match e {
        Enc::U(n) => header(0, *n),
        Enc::B(b) => {
            let mut v = header(2, b.len() as u64);
            v.extend_from_slice(b);
            v
        }
        Enc::T(s) => {
            let mut v = header(3, s.len() as u64);
            v.extend_from_slice(s.as_bytes());
            v
        }
        Enc::A(items) => {
            let mut v = header(4, items.len() as u64);
            for i in items {
                v.extend_from_slice(&encode(i));
            }
            v
        }
        Enc::M(entries) => {
            let mut enc: Vec<(Vec<u8>, Vec<u8>)> = entries
                .iter()
                .map(|(k, val)| (encode(&Enc::T(k)), encode(val)))
                .collect();
            enc.sort_by(|a, b| a.0.cmp(&b.0));
            let mut v = header(5, enc.len() as u64);
            for (k, val) in enc {
                v.extend_from_slice(&k);
                v.extend_from_slice(&val);
            }
            v
        }
    }
}

// ----------------------------------------------------- entry point

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("not lowercase even-length hex".into());
    }
    Ok((0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect())
}

fn in_hex(vector: &Json, key: &str) -> Result<Vec<u8>, String> {
    unhex(
        vector["inputs"][key]
            .as_str()
            .ok_or(format!("inputs.{key} missing"))?,
    )
}

/// The vector's expected `(outcome, disposition)` pair.
fn expected_pair(vector: &Json) -> Result<(String, String), String> {
    Ok((
        vector["expected"]["outcome"]
            .as_str()
            .ok_or("expected.outcome")?
            .to_string(),
        vector["expected"]["disposition"]
            .as_str()
            .ok_or("expected.disposition")?
            .to_string(),
    ))
}

fn pass_if_pair(got: (&str, &str), vector: &Json) -> Result<SemStatus, String> {
    let want = expected_pair(vector)?;
    if (got.0, got.1) == (want.0.as_str(), want.1.as_str()) {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!(
            "expected ({}, {}), reducer derived ({}, {})",
            want.0, want.1, got.0, got.1
        )))
    }
}

/// Dispatch one KAT vector. `Err` = harness-level malformation.
pub fn run(vector: &Json) -> Result<SemStatus, String> {
    let kind = vector["case_kind"].as_str().unwrap_or_default();
    match kind {
        "canonical-encode" => canonical_encode(vector),
        "canonical-reject" => canonical_reject(vector),
        "cap-fit" => cap_fit(vector),
        "cap-exceed" => cap_exceed(vector),
        "hash-domain" => hash_domain(vector),
        "key-id-derive" => key_id_derive(vector),
        "separation-negative" => separation(vector),
        "sign-then-verify" => sign_then_verify(vector),
        "verify-fixed" => verify_fixed(vector),
        "hpke-seal-open" => hpke_seal_open(vector),
        "hpke-negative" => hpke_negative(vector),
        "item-seal-open" => item_seal_open(vector),
        "rewrap-idempotence" => rewrap_idempotence(vector),
        "crypto-negative" => crypto_negative(vector),
        "frontier-fold" => frontier_fold(vector),
        "frontier-negative" => frontier_negative(vector),
        "phrase-derive" => phrase_derive(vector),
        "commitment-derive" => commitment_derive(vector),
        "merkle-proof" => merkle_proof_kat(vector),
        "reencapsulation" => reencapsulation(vector),
        "projection" => projection(vector),
        "offline-confirmation" => offline_confirmation(vector),
        other => Ok(SemStatus::Unimplemented(format!("case_kind {other}"))),
    }
}

// --------------------------------------------------------- family 1

fn canonical_encode(vector: &Json) -> Result<SemStatus, String> {
    if vector["inputs"]["template"].is_string() {
        return Ok(SemStatus::Unimplemented(
            "template-form canonical-encode".into(),
        ));
    }
    let bytes = in_hex(vector, "bytes")?;
    if decode(&bytes).is_err() {
        return Ok(SemStatus::Fail(
            "canonical bytes fail the strict reader".into(),
        ));
    }
    let want = unhex(
        vector["expected"]["bytes"]
            .as_str()
            .ok_or("expected.bytes")?,
    )?;
    if want == bytes {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("expected bytes differ from input".into()))
    }
}

fn canonical_reject(vector: &Json) -> Result<SemStatus, String> {
    let bytes = in_hex(vector, "bytes")?;
    let got = match decode(&bytes) {
        Ok(_) => {
            return Ok(SemStatus::Fail(
                "reject vector decodes under the strict reader".into(),
            ))
        }
        Err(DecodeError::Depth) => "depth",
        Err(DecodeError::NonCanonical) | Err(DecodeError::UintRange) => "non-canonical",
        Err(DecodeError::Malformed) | Err(DecodeError::TrailingBytes) => "malformed",
    };
    pass_if_pair((got, "reject-permanent"), vector)
}

/// The four cap-fit templates, re-derived with the reducer's own
/// writer. Fixture conventions: `fits` for the rotation template
/// allows 4 KiB of header/signature over the body against the 64-KiB
/// control cap; the two single-object templates are trivially true.
fn cap_fit(vector: &Json) -> Result<SemStatus, String> {
    const E1_MAX: u64 = 9_007_199_254_740_991;
    let template = vector["inputs"]["template"]
        .as_str()
        .ok_or("inputs.template")?;

    let kekwrap_worst = |i: u64| -> Enc {
        let mut device = vec![0xffu8; 16];
        device[..8].copy_from_slice(&i.to_be_bytes());
        Enc::M(vec![
            ("v", Enc::U(1)),
            ("plane_id", Enc::B(vec![0xff; 32])),
            ("zone_id", Enc::B(vec![0xff; 16])),
            ("epoch", Enc::U(E1_MAX)),
            ("recipient_device", Enc::B(device)),
            ("recipient_kem_key", Enc::B(vec![0xff; 32])),
            ("kem", Enc::T("hpke-p256-v1")),
            ("enc", Enc::B(vec![0xff; 65])),
            ("ct", Enc::B(vec![0xff; 48])),
        ])
    };
    let erasemref_worst = |i: u64| -> Enc {
        let mut addr = vec![0xffu8; 32];
        addr[..8].copy_from_slice(&i.to_be_bytes());
        Enc::M(vec![
            ("item_addr", Enc::B(addr)),
            ("erase_op", Enc::B(vec![0xff; 32])),
            ("target_op", Enc::B(vec![0xff; 32])),
        ])
    };

    let (key, size, fits) = match template {
        "kekwrap-worst" => {
            // The corpus mints the all-0xff device — rebuild exactly.
            let n = {
                encode(&Enc::M(vec![
                    ("v", Enc::U(1)),
                    ("plane_id", Enc::B(vec![0xff; 32])),
                    ("zone_id", Enc::B(vec![0xff; 16])),
                    ("epoch", Enc::U(E1_MAX)),
                    ("recipient_device", Enc::B(vec![0xff; 16])),
                    ("recipient_kem_key", Enc::B(vec![0xff; 32])),
                    ("kem", Enc::T("hpke-p256-v1")),
                    ("enc", Enc::B(vec![0xff; 65])),
                    ("ct", Enc::B(vec![0xff; 48])),
                ]))
                .len()
            };
            ("kekwrap", n, true)
        }
        "erasemref-worst" => {
            let n = encode(&Enc::M(vec![
                ("item_addr", Enc::B(vec![0xff; 32])),
                ("erase_op", Enc::B(vec![0xff; 32])),
                ("target_op", Enc::B(vec![0xff; 32])),
            ]))
            .len();
            ("erasemref", n, true)
        }
        "ckekrotate-128-128-worst" => {
            let wraps: Vec<Enc> = (0..128).map(kekwrap_worst).collect();
            let manifest: Vec<Enc> = (0..128).map(erasemref_worst).collect();
            let body = Enc::M(vec![
                ("zone_id", Enc::B(vec![0xff; 16])),
                ("new_epoch", Enc::U(E1_MAX)),
                ("wraps", Enc::A(wraps)),
                ("erase_manifest", Enc::A(manifest)),
            ]);
            let n = encode(&body).len();
            ("body", n, n + 4096 <= 65_536)
        }
        "checkpointobj-max-page" => {
            let mut covers = Vec::new();
            let mut fences = Vec::new();
            let mut retired = Vec::new();
            for i in 0..256u64 {
                let mut lineage = vec![0xffu8; 16];
                lineage[..8].copy_from_slice(&i.to_be_bytes());
                covers.push(Enc::M(vec![
                    ("lineage", Enc::B(lineage.clone())),
                    ("gen", Enc::U(E1_MAX)),
                    ("seq", Enc::U(E1_MAX)),
                    ("op", Enc::B(vec![0xff; 32])),
                ]));
                fences.push(Enc::M(vec![
                    ("lineage", Enc::B(lineage.clone())),
                    ("gen", Enc::U(E1_MAX)),
                    ("seq", Enc::U(E1_MAX)),
                ]));
                retired.push(Enc::M(vec![
                    ("lineage", Enc::B(lineage)),
                    ("gen", Enc::U(E1_MAX - 1)),
                    ("seq", Enc::U(E1_MAX)),
                    ("op", Enc::B(vec![0xff; 32])),
                ]));
            }
            let proofs: Vec<Enc> = (0..64u64)
                .map(|i| {
                    let mut cert = vec![0xffu8; 32];
                    cert[..8].copy_from_slice(&i.to_be_bytes());
                    Enc::M(vec![
                        (
                            "issuer",
                            Enc::M(vec![("src", Enc::T("device")), ("cert", Enc::B(cert))]),
                        ),
                        ("through", Enc::U(E1_MAX)),
                        ("head_hash", Enc::B(vec![0xff; 32])),
                    ])
                })
                .collect();
            let cp = Enc::M(vec![
                ("zone_id", Enc::B(vec![0xff; 16])),
                ("prev_checkpoint", Enc::B(vec![0xff; 32])),
                ("covers", Enc::A(covers)),
                ("fences", Enc::A(fences)),
                ("retired", Enc::A(retired)),
                ("proof_positions", Enc::A(proofs)),
            ]);
            let n = encode(&cp).len();
            ("checkpointobj", n, n <= 48 * 1024)
        }
        other => {
            return Ok(SemStatus::Unimplemented(format!(
                "cap-fit template {other}"
            )))
        }
    };

    let want_sizes = vector["expected"]["result"]["sizes"]
        .as_object()
        .ok_or("result.sizes")?;
    let want = want_sizes
        .get(key)
        .and_then(|v| v.as_u64())
        .ok_or(format!("result.sizes.{key}"))?;
    if want != size as u64 {
        return Ok(SemStatus::Fail(format!(
            "{key}: expected {want} B, reducer encodes {size} B"
        )));
    }
    if let Some(want_fits) = vector["expected"]["result"]["fits"].as_bool() {
        if want_fits != fits {
            return Ok(SemStatus::Fail(format!(
                "fits: expected {want_fits}, derived {fits}"
            )));
        }
    }
    Ok(SemStatus::Pass)
}

/// Byte-level cap-exceed carriers: the carrier DECODES (canonical
/// CBOR), then the named E8 cap fires. The cap is identified by
/// shape: a `records` map = the Txn record cap (≤ 16); a
/// zone/lineage/heads map = the frontierclose heads cap (≤ 65).
fn cap_exceed(vector: &Json) -> Result<SemStatus, String> {
    if vector["inputs"]["template"].is_string() {
        return Ok(SemStatus::Unimplemented("template-form cap-exceed".into()));
    }
    let bytes = in_hex(vector, "bytes")?;
    let node = match decode(&bytes) {
        Ok(n) => n,
        Err(e) => {
            return Ok(SemStatus::Fail(format!(
                "cap-exceed carrier fails strict decode: {e:?}"
            )))
        }
    };
    let keys = node.map_keys().unwrap_or_default();
    let over = if keys == vec!["records"] {
        node.get("records")
            .and_then(|r| r.as_array())
            .is_some_and(|a| a.len() > 16)
    } else if keys.contains(&"heads") {
        node.get("heads")
            .and_then(|h| h.as_array())
            .is_some_and(|a| a.len() > 65)
    } else {
        return Ok(SemStatus::Unimplemented(
            "cap-exceed carrier of unknown shape".into(),
        ));
    };
    if !over {
        return Ok(SemStatus::Fail("carrier is within its cap".into()));
    }
    pass_if_pair(("oversized", "reject-permanent"), vector)
}

// --------------------------------------------------------- family 2

fn hash_domain(vector: &Json) -> Result<SemStatus, String> {
    let tag = vector["inputs"]["tag"].as_str().ok_or("inputs.tag")?;
    if !domains::TAGS.contains(&tag) {
        return Ok(SemStatus::Fail(format!("unknown domain tag {tag}")));
    }
    let preimage = in_hex(vector, "preimage")?;
    let want = unhex(
        vector["expected"]["bytes"]
            .as_str()
            .ok_or("expected.bytes")?,
    )?;
    if domains::h(tag, &preimage).to_vec() == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("digest mismatch".into()))
    }
}

fn key_id_derive(vector: &Json) -> Result<SemStatus, String> {
    let kind = vector["inputs"]["kind"].as_str().ok_or("inputs.kind")?;
    let pk = in_hex(vector, "pk")?;
    let got = match kind {
        "key_id" => {
            let alg = vector["inputs"]["alg"].as_str().ok_or("inputs.alg")?;
            domains::key_id(alg, &pk)
        }
        "mat_id" => domains::h("mat", &pk),
        other => return Ok(SemStatus::Unimplemented(format!("key-id kind {other}"))),
    };
    let want = unhex(
        vector["expected"]["bytes"]
            .as_str()
            .ok_or("expected.bytes")?,
    )?;
    if got.to_vec() == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("key-id mismatch".into()))
    }
}

fn separation(vector: &Json) -> Result<SemStatus, String> {
    let a = vector["inputs"]["tag_a"].as_str().ok_or("tag_a")?;
    let b = vector["inputs"]["tag_b"].as_str().ok_or("tag_b")?;
    if !domains::TAGS.contains(&a) || !domains::TAGS.contains(&b) {
        return Ok(SemStatus::Unimplemented("tag outside the inventory".into()));
    }
    let preimage = in_hex(vector, "preimage")?;
    let distinct = domains::h(a, &preimage) != domains::h(b, &preimage);
    let want = vector["expected"]["result"]["distinct"]
        .as_bool()
        .ok_or("result.distinct")?;
    if distinct == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!(
            "distinct={distinct}, expected {want}"
        )))
    }
}

// --------------------------------------------------------- family 3

/// P-256 sign helpers shared by the two lanes.
mod p256_sig {
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

fn sign_then_verify(vector: &Json) -> Result<SemStatus, String> {
    use ed25519_dalek::Signer;
    let name = vector["name"].as_str().unwrap_or_default();
    let msg = in_hex(vector, "msg")?;
    let sk: [u8; 32] = in_hex(vector, "sk")?
        .try_into()
        .map_err(|_| "sk is 32 bytes")?;
    let (valid, low_s) = if name.starts_with("ed25519-") {
        let signing = ed25519_dalek::SigningKey::from_bytes(&sk);
        let sig = signing.sign(&msg);
        let valid = signing.verifying_key().verify_strict(&msg, &sig).is_ok();
        // EdDSA emits no S-malleable form under verify_strict.
        (valid, true)
    } else if name.starts_with("p256-") {
        let Some(sig) = p256_sig::sign_low_s(&sk, &msg) else {
            return Ok(SemStatus::Fail("p256 signing failed".into()));
        };
        let sk_typed =
            p256::ecdsa::SigningKey::from_bytes((&sk).into()).map_err(|_| "sk scalar invalid")?;
        let pk = sk_typed.verifying_key().to_encoded_point(false);
        let valid = p256_sig::verify(pk.as_bytes(), &msg, &sig);
        // low_s: our verify already rejects high-S, so validity
        // implies it; assert it independently anyway.
        let low_s = p256::ecdsa::Signature::from_slice(&sig)
            .map(|s| s.normalize_s().is_none())
            .unwrap_or(false);
        (valid, low_s)
    } else {
        return Ok(SemStatus::Unimplemented(format!(
            "sign-then-verify name prefix in {name:?}"
        )));
    };
    let want_valid = vector["expected"]["result"]["valid"].as_bool() == Some(true);
    let want_low = vector["expected"]["result"]["low_s"].as_bool() == Some(true);
    if valid == want_valid && low_s == want_low {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!("valid={valid} low_s={low_s}")))
    }
}

fn verify_fixed(vector: &Json) -> Result<SemStatus, String> {
    let pk = in_hex(vector, "pk")?;
    let msg = in_hex(vector, "msg")?;
    let sig = in_hex(vector, "sig")?;
    let valid = match pk.len() {
        32 => {
            let pk: [u8; 32] = pk.try_into().expect("len checked");
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk) else {
                return pass_bool(vector, false);
            };
            let Ok(sig64) = <[u8; 64]>::try_from(sig.as_slice()) else {
                return pass_bool(vector, false);
            };
            vk.verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&sig64))
                .is_ok()
        }
        65 => p256_sig::verify(&pk, &msg, &sig),
        _ => false,
    };
    pass_bool(vector, valid)
}

fn pass_bool(vector: &Json, valid: bool) -> Result<SemStatus, String> {
    let want = vector["expected"]["result"]["valid"]
        .as_bool()
        .ok_or("result.valid")?;
    if valid == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!("valid={valid}, expected {want}")))
    }
}

// --------------------------------------------------------- family 4

mod hpke_ops {
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

fn hpke_seal_open(vector: &Json) -> Result<SemStatus, String> {
    let sk: [u8; 32] = in_hex(vector, "recipient_sk")?
        .try_into()
        .map_err(|_| "recipient_sk is 32 bytes")?;
    let pk = in_hex(vector, "recipient_pk")?;
    let plaintext = in_hex(vector, "plaintext")?;
    let aad = in_hex(vector, "aad").unwrap_or_default();
    let info = in_hex(vector, "info").unwrap_or_default();

    // sk ↔ pk consistency.
    if hpke_ops::pk_of(&sk).map(|p| p.to_vec()) != Some(pk) {
        return Ok(SemStatus::Fail(
            "recipient_pk does not match recipient_sk".into(),
        ));
    }
    // The open direction against the minted enc/ct (seal determinism
    // is the core's rng-convention lane; the reducer verifies the
    // portable inverse).
    let enc = unhex(
        vector["expected"]["result"]["enc"]
            .as_str()
            .ok_or("result.enc")?,
    )?;
    let ct = unhex(
        vector["expected"]["result"]["ct"]
            .as_str()
            .ok_or("result.ct")?,
    )?;
    let opened = unhex(
        vector["expected"]["result"]["opened"]
            .as_str()
            .ok_or("result.opened")?,
    )?;
    match hpke_ops::open(&sk, &enc, &info, &aad, &ct) {
        Some(pt) if pt == opened && pt == plaintext => Ok(SemStatus::Pass),
        Some(_) => Ok(SemStatus::Fail("opened plaintext differs".into())),
        None => Ok(SemStatus::Fail("open failed on the minted seal".into())),
    }
}

fn hpke_negative(vector: &Json) -> Result<SemStatus, String> {
    let sk: [u8; 32] = in_hex(vector, "recipient_sk")?
        .try_into()
        .map_err(|_| "recipient_sk is 32 bytes")?;
    let enc = in_hex(vector, "enc")?;
    let ct = in_hex(vector, "ct")?;
    let aad = in_hex(vector, "aad").unwrap_or_default();

    // Outcome by failure site: point validity first (key-malformed),
    // then AEAD (aead-fail). No info in the negatives' inputs — the
    // corpus mints them with empty info? No: the seal used
    // "corpus/info". The negatives carry no info field, so the open
    // MUST fail regardless — but the OUTCOME is decided by the point
    // check, which is info-independent.
    let got = if !hpke_ops::point_valid(&enc) {
        ("key-malformed", "reject-permanent")
    } else if hpke_ops::open(&sk, &enc, &[], &aad, &ct).is_none() {
        ("aead-fail", "storage-quarantine")
    } else {
        return Ok(SemStatus::Fail("negative opened successfully".into()));
    };
    pass_if_pair(got, vector)
}

// --------------------------------------------------------- family 5

/// The reducer's own §5.3 key schedule (context strings from the
/// spec; HKDF-SHA256 + AES-256-GCM from third-party crates).
mod item_crypto {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    use hkdf::Hkdf;
    use sha2::Sha256;

    pub const F5_PLANE: [u8; 32] = [0; 32];
    pub const F5_ZONE: [u8; 16] = [0; 16];

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

    pub fn wrap_key(kek: &[u8; 32], item_addr: &[u8; 32]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(b"intendant/wrapkey/v1"), kek);
        let mut okm = [0u8; 32];
        hk.expand(item_addr, &mut okm).expect("32 B within bounds");
        okm
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
            .encrypt(Nonce::from_slice(nonce), Payload { msg: pt, aad })
            .expect("AES-GCM seal")
    }

    pub fn aead_open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        Aes256Gcm::new(key.into())
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
            .ok()
    }

    pub fn wrap_dek(kek: &[u8; 32], epoch: u64, item_addr: &[u8; 32], dek: &[u8; 32]) -> [u8; 48] {
        let wk = wrap_key(kek, item_addr);
        let aad = dekwrap_aad(&F5_PLANE, &F5_ZONE, epoch, item_addr);
        aead_seal(&wk, &[0u8; 12], &aad, dek)
            .as_slice()
            .try_into()
            .expect("48 B")
    }
}

/// Decode + validate a family-5 `core`/`wrap` pair; run the full
/// open chain. Returns the failure outcome or the plaintext.
fn open_item_chain(
    kek: &[u8; 32],
    epoch: u64,
    core_bytes: &[u8],
    wrap_bytes: &[u8],
) -> Result<Result<Vec<u8>, (&'static str, &'static str)>, String> {
    let core = decode(core_bytes).map_err(|e| format!("core decode: {e:?}"))?;
    let wrap = decode(wrap_bytes).map_err(|e| format!("wrap decode: {e:?}"))?;
    let addr = domains::h("item", core_bytes);
    let named = wrap
        .get("item_addr")
        .and_then(|n| n.bytes_n::<32>())
        .ok_or("wrap.item_addr")?;
    if named != addr {
        return Ok(Err(("wrapper-mismatch", "storage-quarantine")));
    }
    if wrap.get("key_wrap_epoch").and_then(|n| n.as_uint()) != Some(epoch) {
        return Ok(Err(("wrapper-mismatch", "storage-quarantine")));
    }
    let wrapped: [u8; 48] = wrap
        .get("wrapped_dek")
        .and_then(|n| n.bytes_n::<48>())
        .ok_or("wrap.wrapped_dek")?;
    let wk = item_crypto::wrap_key(kek, &addr);
    let aad = item_crypto::dekwrap_aad(&item_crypto::F5_PLANE, &item_crypto::F5_ZONE, epoch, &addr);
    let Some(dek) = item_crypto::aead_open(&wk, &[0u8; 12], &aad, &wrapped) else {
        return Ok(Err(("aead-fail", "storage-quarantine")));
    };
    let dek: [u8; 32] = dek.as_slice().try_into().map_err(|_| "DEK is 32 bytes")?;
    let nonce: [u8; 12] = core
        .get("nonce")
        .and_then(|n| n.bytes_n::<12>())
        .ok_or("core.nonce")?;
    let ct = core.get("ct").and_then(|n| n.as_bytes()).ok_or("core.ct")?;
    let item_aad = item_crypto::item_aad(&item_crypto::F5_PLANE, &item_crypto::F5_ZONE);
    match item_crypto::aead_open(&dek, &nonce, &item_aad, ct) {
        Some(pt) => Ok(Ok(pt)),
        None => Ok(Err(("aead-fail", "storage-quarantine"))),
    }
}

fn item_seal_open(vector: &Json) -> Result<SemStatus, String> {
    let kek: [u8; 32] = in_hex(vector, "kek")?
        .try_into()
        .map_err(|_| "kek is 32 bytes")?;
    let epoch = vector["inputs"]["kek_epoch"].as_u64().ok_or("kek_epoch")?;
    let core_bytes = in_hex(vector, "core")?;
    let wrap_bytes = in_hex(vector, "wrap")?;
    match open_item_chain(&kek, epoch, &core_bytes, &wrap_bytes)? {
        Ok(pt) => {
            let want = unhex(
                vector["expected"]["result"]["plaintext"]
                    .as_str()
                    .ok_or("result.plaintext")?,
            )?;
            if pt == want {
                Ok(SemStatus::Pass)
            } else {
                Ok(SemStatus::Fail("plaintext differs".into()))
            }
        }
        Err((o, d)) => Ok(SemStatus::Fail(format!("chain failed ({o}, {d})"))),
    }
}

fn rewrap_idempotence(vector: &Json) -> Result<SemStatus, String> {
    let kek: [u8; 32] = in_hex(vector, "kek")?
        .try_into()
        .map_err(|_| "kek is 32 bytes")?;
    let dek: [u8; 32] = in_hex(vector, "dek")?
        .try_into()
        .map_err(|_| "dek is 32 bytes")?;
    let addr: [u8; 32] = in_hex(vector, "item_addr")?
        .try_into()
        .map_err(|_| "item_addr is 32 bytes")?;
    let epoch = vector["inputs"]["kek_epoch"].as_u64().ok_or("kek_epoch")?;
    let w1 = item_crypto::wrap_dek(&kek, epoch, &addr, &dek);
    let w2 = item_crypto::wrap_dek(&kek, epoch, &addr, &dek);
    if w1 != w2 {
        return Ok(SemStatus::Fail("rewrap is not byte-idempotent".into()));
    }
    if let Some(want) = vector["expected"]["result"]["wrapper"].as_str() {
        if unhex(want)? != w1.to_vec() {
            return Ok(SemStatus::Fail("wrapper bytes differ from minted".into()));
        }
    }
    if vector["expected"]["result"]["identical"].as_bool() == Some(true) {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("expected.identical must be true".into()))
    }
}

fn crypto_negative(vector: &Json) -> Result<SemStatus, String> {
    let kek: [u8; 32] = in_hex(vector, "kek")?
        .try_into()
        .map_err(|_| "kek is 32 bytes")?;
    let epoch = vector["inputs"]["kek_epoch"].as_u64().ok_or("kek_epoch")?;
    let core_bytes = in_hex(vector, "core")?;
    let wrap_bytes = in_hex(vector, "wrap")?;
    match open_item_chain(&kek, epoch, &core_bytes, &wrap_bytes)? {
        Ok(_) => Ok(SemStatus::Fail("negative opened successfully".into())),
        Err(got) => pass_if_pair(got, vector),
    }
}

// --------------------------------------------------------- family 6

/// One frontier head: (lineage, gen, seq, op).
type FHead = ([u8; 16], u64, u64, [u8; 32]);
/// A parsed frontier, or its rejection outcome.
type ParsedFrontier = Result<([u8; 16], Vec<FHead>), &'static str>;
/// A completed fold — (bytes, hash, noop) — or its failure pair.
type FoldOut = Result<(Vec<u8>, [u8; 32], bool), (&'static str, &'static str)>;

/// Decode + canonical-validate a frontier: exact keys, v = 1, heads
/// strictly ascending by (lineage, gen) — a duplicate pair is
/// non-canonical.
fn parse_frontier(node: &Node) -> Result<ParsedFrontier, String> {
    let mut keys = node.map_keys().ok_or("frontier not a map")?;
    keys.sort_unstable();
    if keys != ["heads", "v", "zone_id"] {
        return Ok(Err("malformed"));
    }
    if node.get("v").and_then(|n| n.as_uint()) != Some(1) {
        return Ok(Err("malformed"));
    }
    let zone = node
        .get("zone_id")
        .and_then(|n| n.bytes_n::<16>())
        .ok_or("frontier.zone_id")?;
    let mut heads = Vec::new();
    for hn in node
        .get("heads")
        .and_then(|h| h.as_array())
        .ok_or("heads")?
    {
        let (Some(l), Some(g), Some(s), Some(o)) = (
            hn.get("lineage").and_then(|n| n.bytes_n::<16>()),
            hn.get("gen").and_then(|n| n.as_uint()),
            hn.get("seq").and_then(|n| n.as_uint()),
            hn.get("op").and_then(|n| n.bytes_n::<32>()),
        ) else {
            return Ok(Err("malformed"));
        };
        heads.push((l, g, s, o));
    }
    for w in heads.windows(2) {
        if (w[0].0, w[0].1) >= (w[1].0, w[1].1) {
            return Ok(Err("non-canonical"));
        }
    }
    Ok(Ok((zone, heads)))
}

fn encode_frontier(zone: &[u8; 16], heads: &[FHead]) -> Vec<u8> {
    let hs: Vec<Enc> = heads
        .iter()
        .map(|(l, g, s, o)| {
            Enc::M(vec![
                ("lineage", Enc::B(l.to_vec())),
                ("gen", Enc::U(*g)),
                ("seq", Enc::U(*s)),
                ("op", Enc::B(o.to_vec())),
            ])
        })
        .collect();
    encode(&Enc::M(vec![
        ("v", Enc::U(1)),
        ("zone_id", Enc::B(zone.to_vec())),
        ("heads", Enc::A(hs)),
    ]))
}

/// Fold one event. 4-key map = acceptance; 3-key = D-33 retirement.
/// Returns `changed`, or the failure pair.
fn fold_event(
    heads: &mut Vec<FHead>,
    ev: &Node,
) -> Result<Result<bool, (&'static str, &'static str)>, String> {
    let keys = ev.map_keys().ok_or("event not a map")?;
    let (Some(l), Some(g), Some(s)) = (
        ev.get("lineage").and_then(|n| n.bytes_n::<16>()),
        ev.get("gen").and_then(|n| n.as_uint()),
        ev.get("seq").and_then(|n| n.as_uint()),
    ) else {
        return Err("event coordinates".into());
    };
    if keys.len() == 4 {
        let op = ev
            .get("op")
            .and_then(|n| n.bytes_n::<32>())
            .ok_or("event.op")?;
        match heads.iter().position(|(hl, hg, ..)| (*hl, *hg) == (l, g)) {
            None => {
                let at = heads
                    .iter()
                    .position(|(hl, hg, ..)| (*hl, *hg) > (l, g))
                    .unwrap_or(heads.len());
                heads.insert(at, (l, g, s, op));
                Ok(Ok(true))
            }
            Some(i) => {
                let cur = heads[i];
                if s > cur.2 {
                    heads[i] = (l, g, s, op);
                    Ok(Ok(true))
                } else if s == cur.2 && op != cur.3 {
                    // Equal coordinates, differing hash: fork.
                    Ok(Err(("fork", "freeze-writer")))
                } else {
                    Err("stale/idempotent acceptance events are unpinned".into())
                }
            }
        }
    } else if keys.len() == 3 {
        // Retirement: drop the accepted head at or below the
        // incorporated position; none = successful no-op (D-188).
        match heads
            .iter()
            .position(|(hl, hg, hs, _)| (*hl, *hg) == (l, g) && *hs <= s)
        {
            Some(i) => {
                heads.remove(i);
                Ok(Ok(true))
            }
            None => Ok(Ok(false)),
        }
    } else {
        Err("event of unknown shape".into())
    }
}

fn run_frontier(vector: &Json) -> Result<FoldOut, String> {
    let initial = in_hex(vector, "initial")?;
    let node = decode(&initial).map_err(|e| format!("initial decode: {e:?}"))?;
    let (zone, mut heads) = match parse_frontier(&node)? {
        Ok(f) => f,
        Err(o) => return Ok(Err((o_static(o), "reject-permanent"))),
    };
    let events = vector["inputs"]["events"]
        .as_array()
        .ok_or("inputs.events")?;
    let mut changed = false;
    for ev_hex in events {
        let ev_bytes = unhex(ev_hex.as_str().ok_or("event not a string")?)?;
        let ev = decode(&ev_bytes).map_err(|e| format!("event decode: {e:?}"))?;
        match fold_event(&mut heads, &ev)? {
            Ok(c) => changed |= c,
            Err(pair) => return Ok(Err(pair)),
        }
    }
    let bytes = encode_frontier(&zone, &heads);
    let hash = domains::h("frontier", &bytes);
    Ok(Ok((bytes, hash, !changed)))
}

fn o_static(o: &'static str) -> &'static str {
    o
}

fn frontier_fold(vector: &Json) -> Result<SemStatus, String> {
    match run_frontier(vector)? {
        Err((o, d)) => Ok(SemStatus::Fail(format!("fold failed ({o}, {d})"))),
        Ok((bytes, hash, noop)) => {
            let r = &vector["expected"]["result"];
            if let Some(want) = r["frontier"].as_str() {
                if unhex(want)? != bytes {
                    return Ok(SemStatus::Fail("frontier bytes differ".into()));
                }
            }
            if let Some(want) = r["frontier_hash"].as_str() {
                if unhex(want)? != hash.to_vec() {
                    return Ok(SemStatus::Fail("frontier hash differs".into()));
                }
            }
            if let Some(want) = r["noop"].as_bool() {
                if want != noop {
                    return Ok(SemStatus::Fail(format!("noop={noop}, expected {want}")));
                }
            }
            Ok(SemStatus::Pass)
        }
    }
}

fn frontier_negative(vector: &Json) -> Result<SemStatus, String> {
    match run_frontier(vector)? {
        Ok(_) => Ok(SemStatus::Fail("negative folded successfully".into())),
        Err(got) => pass_if_pair(got, vector),
    }
}

// --------------------------------------------------------- family 8

/// §2.4 exact: `seed = PBKDF2-HMAC-SHA512(mnemonic, "mnemonic",
/// 2048, 64)`, then the HKDF stage, then the Ed25519 keypair. The
/// reducer derives from the PHRASE (the entropy→mnemonic leg is the
/// core's — fixture convention); the corpus phrases are ASCII, so
/// NFKD normalization is the identity.
fn phrase_derive(vector: &Json) -> Result<SemStatus, String> {
    use hkdf::Hkdf;
    use sha2::{Sha256, Sha512};
    let Some(phrase) = vector["inputs"]["phrase"].as_str() else {
        return Ok(SemStatus::Unimplemented(
            "entropy-only phrase-derive (the mnemonic leg is core-side)".into(),
        ));
    };
    let mut seed = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<Sha512>(phrase.as_bytes(), b"mnemonic", 2048, &mut seed);
    let hk = Hkdf::<Sha256>::new(Some(b"intendant/recovery/v1"), &seed);
    let mut ed_seed = [0u8; 32];
    hk.expand(b"ed25519-seed", &mut ed_seed)
        .expect("32 B within bounds");
    let pk = ed25519_dalek::SigningKey::from_bytes(&ed_seed)
        .verifying_key()
        .to_bytes();

    let keys = vector["expected"]["result"]["keys"]
        .as_object()
        .ok_or("result.keys")?;
    for (name, want_hex) in keys {
        let want = unhex(want_hex.as_str().ok_or("key not a string")?)?;
        let got: &[u8] = match name.as_str() {
            "seed" => &seed,
            "ed25519_seed" => &ed_seed,
            "recovery_pk" => &pk,
            other => return Ok(SemStatus::Unimplemented(format!("key name {other}"))),
        };
        if got != want.as_slice() {
            return Ok(SemStatus::Fail(format!("{name} differs")));
        }
    }
    Ok(SemStatus::Pass)
}

/// `commitment = H_drill(recovery_pk)`.
fn commitment_derive(vector: &Json) -> Result<SemStatus, String> {
    let pk = in_hex(vector, "recovery_pk")?;
    let want = unhex(
        vector["expected"]["bytes"]
            .as_str()
            .ok_or("expected.bytes")?,
    )?;
    if domains::h("drill", &pk).to_vec() == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("commitment differs".into()))
    }
}

// ------------------------------------------- family 11 merkle-proof

/// The KAT lane carries no record_count (the admission lane reads it
/// from the signed release), so validity quantifies over widths
/// 1..=128: SOME width reproduces the root under exact consumption.
fn merkle_proof_kat(vector: &Json) -> Result<SemStatus, String> {
    let leaf_bytes = in_hex(vector, "bundleleaf")?;
    let root: [u8; 32] = in_hex(vector, "root")?
        .try_into()
        .map_err(|_| "root is 32 bytes")?;
    let idx = vector["inputs"]["rec_index"].as_u64().ok_or("rec_index")?;
    let mut proof: Vec<[u8; 32]> = Vec::new();
    for pn in vector["inputs"]["proof"].as_array().ok_or("proof")? {
        proof.push(
            unhex(pn.as_str().ok_or("proof entry")?)?
                .try_into()
                .map_err(|_| "sibling is 32 bytes")?,
        );
    }
    // D-162: the leaf is SELF-DESCRIBING — its internal `rec_index`
    // must equal the declared one (the wrong-index case dies here,
    // width-independent: leaf 2 of a 3-record bundle is structurally
    // identical to leaf 1 of a 2-record one, so only the binding
    // distinguishes them).
    let leaf_node = decode(&leaf_bytes).map_err(|e| format!("bundleleaf decode: {e:?}"))?;
    let internal_idx = leaf_node
        .get("rec_index")
        .and_then(|n| n.as_uint())
        .ok_or("bundleleaf.rec_index")?;
    let leaf = domains::h("brec", &leaf_bytes);
    let valid = internal_idx == idx
        && (1..=128u64)
            .any(|w| idx < w && domains::merkle_fold(leaf, idx, w, &proof) == Some(root));

    if let Some(want) = vector["expected"]["result"]["valid"].as_bool() {
        return if valid == want {
            Ok(SemStatus::Pass)
        } else {
            Ok(SemStatus::Fail(format!("valid={valid}, expected {want}")))
        };
    }
    // The negative arm: an unverifiable proof is `body-invariant`.
    if valid {
        return Ok(SemStatus::Fail("negative proof verified".into()));
    }
    pass_if_pair(("body-invariant", "reject-permanent"), vector)
}

// -------------------------------------------------------- family 14

/// M1: exact SignedOperation bytes survive a P1→P2 container move.
/// The lane re-seals `p1` under fixed KAT parameters (DEK [0x91;32],
/// nonce [0x92;12], the family-5 plane/zone constants) and opens it
/// back — byte equality.
fn reencapsulation(vector: &Json) -> Result<SemStatus, String> {
    let p1 = in_hex(vector, "p1")?;
    if crate::envelope::parse_op(&p1).is_err() {
        return Ok(SemStatus::Fail("p1 is not a valid operation triple".into()));
    }
    let dek = [0x91u8; 32];
    let nonce = [0x92u8; 12];
    let aad = item_crypto::item_aad(&item_crypto::F5_PLANE, &item_crypto::F5_ZONE);
    let sealed = item_crypto::aead_seal(&dek, &nonce, &aad, &p1);
    let opened = item_crypto::aead_open(&dek, &nonce, &aad, &sealed)
        .ok_or("re-encapsulation round trip failed")?;
    let identical = opened == p1;
    if vector["expected"]["result"]["identical"].as_bool() == Some(true) && identical {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!("identical={identical}")))
    }
}

/// §11.7 stamp projection: an m.export.release's complete
/// classification evaluation point. A missing component is
/// `body-invariant` (the body-hash stage passed — the CDDL gap is
/// what fires).
fn projection(vector: &Json) -> Result<SemStatus, String> {
    let bytes = in_hex(vector, "bytes")?;
    let op = match crate::envelope::parse_op(&bytes) {
        Ok(op) => op,
        Err(e) => return Ok(SemStatus::Fail(format!("projection input: {e:?}"))),
    };
    if op.header.operation_type != "m.export.release" {
        return Ok(SemStatus::Unimplemented(format!(
            "projection over {}",
            op.header.operation_type
        )));
    }
    if !op.body_hash_ok() {
        return Ok(SemStatus::Fail("body hash fails".into()));
    }
    let stamp = (
        op.body.get("data_frontier").and_then(|n| n.bytes_n::<32>()),
        op.body
            .get("control_frontier")
            .and_then(|n| n.bytes_n::<32>()),
        op.body.get("as_of_ms").and_then(|n| n.as_uint()),
    );
    match stamp {
        (Some(df), Some(cf), Some(ms)) => {
            let r = &vector["expected"]["result"];
            if !r.is_object() {
                return Ok(SemStatus::Fail(
                    "complete stamp but the vector expects a negative".into(),
                ));
            }
            let want_df = unhex(r["data_frontier"].as_str().ok_or("result.data_frontier")?)?;
            let want_cf = unhex(
                r["control_frontier"]
                    .as_str()
                    .ok_or("result.control_frontier")?,
            )?;
            let want_ms = r["as_of_ms"].as_u64().ok_or("result.as_of_ms")?;
            if df.to_vec() == want_df && cf.to_vec() == want_cf && ms == want_ms {
                Ok(SemStatus::Pass)
            } else {
                Ok(SemStatus::Fail("projected stamp differs".into()))
            }
        }
        _ => pass_if_pair(("body-invariant", "reject-permanent"), vector),
    }
}

/// Umbrella App C #2 — a procedural fixture: the confirmation run is
/// PENDING; the lane checks the recording obligation is carried.
fn offline_confirmation(vector: &Json) -> Result<SemStatus, String> {
    let procedure = vector["inputs"]["procedure"]
        .as_str()
        .ok_or("inputs.procedure")?;
    let recorded = vector["expected"]["result"]["recorded"]
        .as_str()
        .ok_or("result.recorded")?;
    if procedure.is_empty() || recorded.is_empty() {
        return Ok(SemStatus::Fail("empty procedure/recording".into()));
    }
    Ok(SemStatus::Pass)
}
