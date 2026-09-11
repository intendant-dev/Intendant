//! The corpus, families 1–6 (§13.3): encoding+caps, domains/key-ids,
//! signatures, HPKE, item crypto, frontier — KAT-style vectors with
//! explicit inputs (no vector RNG: key material rides as hex, drawn
//! once from fixed seeds here so the fixtures are self-contained).
//!
//! Fixture conventions carried by this module (register entries in
//! the program ledger; the Gate-A audit sweeps them):
//! - family-3 `sign-then-verify` carries no algorithm field — the
//!   vector NAME's prefix (`ed25519-` / `p256-`) selects it;
//!   `verify-fixed` discriminates by pk length (32 vs 65).
//! - family-5 item-crypto KATs fix `plane_id = [0;32]`,
//!   `zone_id = [0;16]` (the §5.3 AAD binds them; the contract
//!   carries neither).
//! - family-6 fold events: a 4-key `head` map is an acceptance; a
//!   3-key `fencecoord` map (`{lineage, gen, seq}`) is a D-33
//!   incorporated-position retirement.

use crate::cbor::{self, Value};
use crate::domains::{h_tag, Tag};
use crate::keyschedule;
use crate::shapes::journal::{Frontier, Pendingxfer, Txn, Txnrec};
use crate::shapes::{bytes, u, Bytes16, Bytes32, Erasemref, Head, Kekwrap, ToValue};
use crate::suite;
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

/// The widest legal uint (E1: ≤ 2^53 − 1) — same 9-byte encoding as
/// any 8-byte-form value.
const E1_MAX: u64 = 9_007_199_254_740_991;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn enc(v: &Value) -> Vec<u8> {
    cbor::encode(v).expect("corpus value encodes")
}

/// A KAT vector without RNG.
#[allow(clippy::too_many_arguments)]
fn kat(
    family: u8,
    name: &str,
    case_kind: &str,
    source: &str,
    surfaces: &[&str],
    inputs: JsonMap<String, Json>,
    expected: Expected,
) -> Vector {
    Vector {
        family,
        name: name.into(),
        case_kind: case_kind.into(),
        source: source.into(),
        surfaces: surfaces.iter().map(|s| s.to_string()).collect(),
        rng: None,
        inputs,
        expected,
    }
}

fn in1(k: &str, v: Json) -> JsonMap<String, Json> {
    let mut m = JsonMap::new();
    m.insert(k.into(), v);
    m
}

fn in2(k1: &str, v1: Json, k2: &str, v2: Json) -> JsonMap<String, Json> {
    let mut m = in1(k1, v1);
    m.insert(k2.into(), v2);
    m
}

fn reject(o: &str, d: &str) -> Expected {
    Expected::Negative {
        outcome: o.into(),
        disposition: d.into(),
    }
}

// ---------------------------------------------------------- family 1

const ENC_SURF: &[&str] = &["core", "browser"];

fn f1_encode_identity(name: &str, v: &Value) -> Vector {
    let b = enc(v);
    kat(
        1,
        name,
        "canonical-encode",
        "2.1",
        ENC_SURF,
        in1("bytes", json!(hex(&b))),
        Expected::Bytes(b),
    )
}

fn f1_reject(name: &str, raw: &[u8], outcome: &str) -> Vector {
    kat(
        1,
        name,
        "canonical-reject",
        "2.1",
        ENC_SURF,
        in1("bytes", json!(hex(raw))),
        reject(outcome, "reject-permanent"),
    )
}

/// The worst-width canonical `kekwrap` (D-85: 314 B) — every
/// variable-width field at its widest legal encoding.
fn kekwrap_worst() -> Kekwrap {
    Kekwrap {
        plane_id: [0xff; 32],
        zone_id: [0xff; 16],
        epoch: E1_MAX,
        recipient_device: [0xff; 16],
        recipient_kem_key: [0xff; 32],
        enc: [0xff; 65],
        ct: [0xff; 48],
    }
}

/// The worst-width canonical `erasemref` (D-85: 132 B).
fn erasemref_worst() -> Erasemref {
    Erasemref {
        item_addr: [0xff; 32],
        erase_op: [0xff; 32],
        target_op: [0xff; 32],
    }
}

fn f1_cap_fit(name: &str, template: &str, sizes: &[(&str, usize)], fits: bool) -> Vector {
    let mut m = JsonMap::new();
    for (k, v) in sizes {
        m.insert(k.to_string(), json!(v));
    }
    kat(
        1,
        name,
        "cap-fit",
        "2.1",
        ENC_SURF,
        in1("template", json!(template)),
        Expected::Result(json!({ "sizes": Json::Object(m), "fits": fits })),
    )
}

fn family1() -> Vec<Vector> {
    let mut out = Vec::new();

    // Canonical identity: uint width boundaries, one per E1 step.
    let uints: Vec<Value> = [
        0u64,
        23,
        24,
        255,
        256,
        65_535,
        65_536,
        4_294_967_295,
        4_294_967_296,
        9_007_199_254_740_991,
    ]
    .iter()
    .map(|&n| Value::Uint(n))
    .collect();
    out.push(f1_encode_identity(
        "uint-boundary-widths",
        &Value::Array(uints),
    ));

    // Map key order is by ENCODED key bytes — "pk" (0x62…) sorts
    // ahead of "alg" (0x63…) through its header byte.
    out.push(f1_encode_identity(
        "map-key-encoded-byte-order",
        &cbor::map(vec![
            ("pk", u(1)),
            ("alg", u(2)),
            ("zone_id", u(3)),
            ("recipient_kem_key", u(4)),
        ]),
    ));

    // Rejects: one per strict-reader rule.
    out.push(f1_reject("nonminimal-uint", &[0x18, 0x17], "non-canonical"));
    out.push(f1_reject(
        "unsorted-map-keys",
        // {"b": 1, "a": 2} in source order.
        &[0xa2, 0x61, 0x62, 0x01, 0x61, 0x61, 0x02],
        "non-canonical",
    ));
    out.push(f1_reject(
        "duplicate-map-key",
        &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02],
        "non-canonical",
    ));
    out.push(f1_reject(
        "indefinite-length-array",
        &[0x9f, 0x01, 0xff],
        "malformed",
    ));
    out.push({
        // Nine nested arrays: container depth 9 > 8 (E8).
        let mut b = vec![0x81; 9];
        b.push(0x01);
        f1_reject("depth-nine", &b, "depth")
    });
    out.push(f1_reject(
        "uint-above-2p53",
        // 2^53 (E1: uint ≤ 2^53 − 1).
        &[0x1b, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        "non-canonical",
    ));
    out.push(f1_reject(
        "float-rejected",
        &[0xf9, 0x3c, 0x00],
        "malformed",
    ));
    out.push(f1_reject("negative-int-rejected", &[0x20], "malformed"));
    out.push(f1_reject("trailing-bytes", &[0x01, 0x02], "malformed"));

    // Encoder-exact pinned figures (D-85/D-96).
    let kw = enc(&kekwrap_worst().to_value());
    assert_eq!(kw.len(), 314, "kekwrap worst-width figure");
    out.push(f1_cap_fit(
        "kekwrap-worst-width",
        "kekwrap-worst",
        &[("kekwrap", kw.len())],
        true,
    ));
    let em = enc(&erasemref_worst().to_value());
    assert_eq!(em.len(), 132, "erasemref worst-width figure");
    out.push(f1_cap_fit(
        "erasemref-worst-width",
        "erasemref-worst",
        &[("erasemref", em.len())],
        true,
    ));

    // The 128+128 rotation joint fit: worst wraps + worst manifest
    // in one c.kek_rotate body — the body must clear the 64-KiB
    // control-triple cap with header/signature room (§7.1's ≈58-KiB
    // joint figure).
    let body = {
        use crate::shapes::control::Ckekrotate;
        let mut wraps = Vec::new();
        let mut manifest = Vec::new();
        for i in 0..128u64 {
            let mut w = kekwrap_worst();
            // Distinct set keys (zone, epoch, recipient_device).
            w.recipient_device[..8].copy_from_slice(&i.to_be_bytes());
            wraps.push(w);
            let mut e = erasemref_worst();
            e.item_addr[..8].copy_from_slice(&i.to_be_bytes());
            manifest.push(e);
        }
        Ckekrotate {
            zone_id: [0xff; 16],
            new_epoch: E1_MAX,
            wraps,
            erase_manifest: manifest,
        }
    };
    let body_len = enc(&body.to_value()).len();
    assert!(body_len < 60 * 1024, "joint rotation body ≈58 KiB");
    out.push(f1_cap_fit(
        "rotation-128-wraps-128-manifest-joint-fit",
        "ckekrotate-128-128-worst",
        &[("body", body_len)],
        // fits: body + worst header/signature < 64 KiB.
        body_len + 4096 <= 65_536,
    ));

    // The 48-KiB checkpoint joint budget (D-96): a balanced maximal
    // page — full covers/fences/retired/proof_positions at worst
    // widths — must fit the joint cap.
    let cp = {
        use crate::shapes::control::{Checkpointobj, PrevCheckpoint, ProofPosition};
        use crate::shapes::{Fencecoord, Issuerid};
        let mut covers = Vec::new();
        let mut fences = Vec::new();
        let mut retired = Vec::new();
        for i in 0..256u64 {
            let mut lineage = [0xffu8; 16];
            lineage[..8].copy_from_slice(&i.to_be_bytes());
            covers.push(Head {
                lineage,
                gen: E1_MAX,
                seq: E1_MAX,
                op: [0xff; 32],
            });
            fences.push(Fencecoord {
                lineage,
                gen: E1_MAX,
                seq: E1_MAX,
            });
            retired.push(Head {
                lineage,
                gen: E1_MAX - 1,
                seq: E1_MAX,
                op: [0xff; 32],
            });
        }
        let proofs = (0..64u64)
            .map(|i| {
                let mut key = [0xffu8; 32];
                key[..8].copy_from_slice(&i.to_be_bytes());
                ProofPosition {
                    issuer: Issuerid::Device { cert: key },
                    through: E1_MAX,
                    head_hash: [0xff; 32],
                }
            })
            .collect();
        Checkpointobj {
            zone_id: [0xff; 16],
            prev_checkpoint: PrevCheckpoint::Op([0xff; 32]),
            covers,
            fences,
            retired,
            proof_positions: proofs,
        }
    };
    let cp_len = enc(&cp.to_value()).len();
    out.push(f1_cap_fit(
        "checkpoint-joint-budget",
        "checkpointobj-max-page",
        &[("checkpointobj", cp_len)],
        cp_len <= 48 * 1024,
    ));

    // Cap-exceed carriers (E8): byte-level, so every surface parses
    // the SAME oversized object.
    let txn17 = Txn {
        records: (0..17)
            .map(|i| {
                let mut px = Pendingxfer {
                    export_id: [0; 16],
                    release_op: [0; 32],
                    dest_zone: [0; 16],
                    content_digest: [0; 32],
                    record_count: 1,
                };
                px.release_op[..8].copy_from_slice(&(i as u64).to_be_bytes());
                Txnrec::PendingXfer(px)
            })
            .collect(),
    };
    // The typed builder enforces the cap — emit the raw map instead.
    let txn17_bytes = enc(&cbor::map(vec![(
        "records",
        Value::Array(txn17.records.iter().map(|r| r.to_value()).collect()),
    )]));
    out.push(kat(
        1,
        "txn-seventeen-records",
        "cap-exceed",
        "6.1",
        ENC_SURF,
        in1("bytes", json!(hex(&txn17_bytes))),
        reject("oversized", "reject-permanent"),
    ));

    let fc66 = {
        let heads: Vec<Value> = (1..=66u64)
            .map(|g| {
                Head {
                    lineage: [7; 16],
                    gen: g,
                    seq: 1,
                    op: [1; 32],
                }
                .to_value()
            })
            .collect();
        enc(&cbor::map(vec![
            ("zone_id", bytes(&[7; 16])),
            ("lineage", bytes(&[7; 16])),
            ("heads", Value::Array(heads)),
        ]))
    };
    out.push(kat(
        1,
        "frontierclose-sixty-six-heads",
        "cap-exceed",
        "2.1",
        ENC_SURF,
        in1("bytes", json!(hex(&fc66))),
        reject("oversized", "reject-permanent"),
    ));

    out
}

// ---------------------------------------------------------- family 2

fn family2() -> Vec<Vector> {
    let surf: &[&str] = &["core", "browser"];
    let mut out = Vec::new();

    for (name, tag, preimage) in [
        ("hash-domain-op", Tag::Op, b"abc".to_vec()),
        ("hash-domain-genstart", Tag::Genstart, {
            let mut p = vec![0x11; 16];
            p.extend_from_slice(&2u64.to_be_bytes());
            p
        }),
        ("hash-domain-frontier-empty", Tag::Frontier, Vec::new()),
    ] {
        out.push(kat(
            2,
            name,
            "hash-domain",
            "2.2",
            surf,
            in2("tag", json!(tag.name()), "preimage", json!(hex(&preimage))),
            Expected::Bytes(h_tag(tag, &preimage).to_vec()),
        ));
    }

    // One P-256 point under three identities: two typed key_ids and
    // the role-neutral mat_id (D-175's material identity).
    let (_sk, pk) = suite::ecdsa_p256::keypair(&[0x42; 32]).expect("fixed scalar valid");
    for (name, kind, alg, expected) in [
        (
            "key-id-p256",
            "key_id",
            Some("p256"),
            suite::key_id("p256", &pk),
        ),
        (
            "key-id-hpke-p256",
            "key_id",
            Some("hpke-p256-v1"),
            suite::key_id("hpke-p256-v1", &pk),
        ),
        ("mat-id-role-neutral", "mat_id", None, h_tag(Tag::Mat, &pk)),
    ] {
        let mut m = in2("kind", json!(kind), "pk", json!(hex(&pk)));
        if let Some(a) = alg {
            m.insert("alg".into(), json!(a));
        }
        out.push(kat(
            2,
            name,
            "key-id-derive",
            "2.2",
            surf,
            m,
            Expected::Bytes(expected.to_vec()),
        ));
    }

    let mut m = in2("tag_a", json!("op"), "tag_b", json!("body"));
    m.insert("preimage".into(), json!(hex(b"same-bytes")));
    out.push(kat(
        2,
        "separation-op-vs-body",
        "separation-negative",
        "2.2",
        surf,
        m,
        Expected::Result(json!({ "distinct": true })),
    ));

    out
}

// ---------------------------------------------------------- family 3

fn family3() -> Vec<Vector> {
    let surf: &[&str] = &["native-crypto", "browser"];
    let mut out = Vec::new();
    let msg = b"the plane accepts only what every replica re-derives";

    // sign-then-verify: the NAME prefix selects the algorithm (the
    // contract carries no alg field — fixture convention). Browsers
    // run verify-fixed only (WebCrypto signing is nondeterministic —
    // the vector.rs mint gate enforces the exclusion).
    out.push(kat(
        3,
        "ed25519-sign-then-verify",
        "sign-then-verify",
        "2.3",
        &["native-crypto"],
        in2("sk", json!(hex(&[0x51; 32])), "msg", json!(hex(msg))),
        Expected::Result(json!({ "valid": true, "low_s": true })),
    ));
    out.push(kat(
        3,
        "p256-sign-then-verify-low-s",
        "sign-then-verify",
        "2.3",
        &["native-crypto"],
        in2("sk", json!(hex(&[0x52; 32])), "msg", json!(hex(msg))),
        Expected::Result(json!({ "valid": true, "low_s": true })),
    ));

    // verify-fixed: pk length discriminates (32 = ed25519, 65 = p256).
    use ed25519_dalek::Signer;
    let ed_sk = ed25519_dalek::SigningKey::from_bytes(&[0x53; 32]);
    let ed_pk = ed_sk.verifying_key().to_bytes();
    let ed_sig = ed_sk.sign(msg).to_bytes();
    let fixed = |name: &str, pk: &[u8], sig: &[u8], valid: bool| {
        let mut m = in2("pk", json!(hex(pk)), "msg", json!(hex(msg)));
        m.insert("sig".into(), json!(hex(sig)));
        kat(
            3,
            name,
            "verify-fixed",
            "2.3",
            surf,
            m,
            Expected::Result(json!({ "valid": valid })),
        )
    };
    out.push(fixed("ed25519-verify-valid", &ed_pk, &ed_sig, true));
    let mut tampered = ed_sig;
    tampered[0] ^= 1;
    out.push(fixed("ed25519-verify-tampered", &ed_pk, &tampered, false));

    // P-256: a low-S signature verifies; its high-S twin MUST NOT
    // (emitters normalize, validators reject).
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use sha2::{Digest, Sha256};
    let p_sk = p256::ecdsa::SigningKey::from_bytes((&[0x54u8; 32]).into()).expect("valid scalar");
    let p_pk = p_sk.verifying_key().to_encoded_point(false);
    let digest = Sha256::digest(msg);
    let sig: p256::ecdsa::Signature = p_sk.sign_prehash(&digest).expect("RFC 6979");
    let sig = sig.normalize_s().unwrap_or(sig);
    out.push(fixed(
        "p256-verify-low-s-valid",
        p_pk.as_bytes(),
        &sig.to_bytes(),
        true,
    ));
    // High-S twin: s' = n − s (scalar negation).
    let (r, s) = sig.split_scalars();
    let high = p256::ecdsa::Signature::from_scalars(r.to_bytes(), (-*s).to_bytes())
        .expect("high-S twin is a well-formed signature");
    out.push(fixed(
        "p256-verify-high-s-rejected",
        p_pk.as_bytes(),
        &high.to_bytes(),
        false,
    ));

    out
}

// ---------------------------------------------------------- family 4

fn family4() -> Vec<Vector> {
    let surf: &[&str] = &["native-crypto", "browser"];
    let mut out = Vec::new();

    let (sk, pk) = suite::hpke_wrap::derive_keypair(&[0x61; 32]);
    let plaintext = [0x0f; 32];
    let aad = b"corpus/aad".to_vec();
    let info = b"corpus/info".to_vec();
    let (enc_pk, ct) = suite::hpke_wrap::seal(&pk, &info, &aad, &plaintext, &[0x62; 32])
        .expect("seal with fixed ephemeral ikm");
    let opened = suite::hpke_wrap::open(&sk, &enc_pk, &info, &aad, &ct).expect("round-trip opens");
    assert_eq!(opened, plaintext);

    let mut m = in2(
        "recipient_pk",
        json!(hex(&pk)),
        "recipient_sk",
        json!(hex(&sk)),
    );
    m.insert("plaintext".into(), json!(hex(&plaintext)));
    m.insert("aad".into(), json!(hex(&aad)));
    m.insert("info".into(), json!(hex(&info)));
    out.push(kat(
        4,
        "seal-open-roundtrip",
        "hpke-seal-open",
        "2.1",
        surf,
        m,
        Expected::Result(json!({
            "enc": hex(&enc_pk),
            "ct": hex(&ct),
            "opened": hex(&opened),
        })),
    ));

    // Malformed encapsulated point: not on the curve.
    let mut bad_enc = enc_pk;
    bad_enc[0] = 0x04;
    bad_enc[1..].fill(0x01);
    let mut m = in2("recipient_sk", json!(hex(&sk)), "enc", json!(hex(&bad_enc)));
    m.insert("ct".into(), json!(hex(&ct)));
    m.insert("aad".into(), json!(hex(&aad)));
    out.push(kat(
        4,
        "malformed-point-rejected",
        "hpke-negative",
        "2.1",
        surf,
        m,
        reject("key-malformed", "reject-permanent"),
    ));

    // The identity encoding (all-zero coordinates — P-256's point at
    // infinity has no SEC1 uncompressed form, so this is the
    // identity-DH probe).
    let mut zero_enc = [0u8; 65];
    zero_enc[0] = 0x04;
    let mut m = in2(
        "recipient_sk",
        json!(hex(&sk)),
        "enc",
        json!(hex(&zero_enc)),
    );
    m.insert("ct".into(), json!(hex(&ct)));
    m.insert("aad".into(), json!(hex(&aad)));
    out.push(kat(
        4,
        "identity-point-rejected",
        "hpke-negative",
        "2.1",
        surf,
        m,
        reject("key-malformed", "reject-permanent"),
    ));

    // AEAD tamper under a valid encapsulation.
    let mut bad_ct = ct.clone();
    bad_ct[0] ^= 1;
    let mut m = in2("recipient_sk", json!(hex(&sk)), "enc", json!(hex(&enc_pk)));
    m.insert("ct".into(), json!(hex(&bad_ct)));
    m.insert("aad".into(), json!(hex(&aad)));
    out.push(kat(
        4,
        "tampered-ciphertext-rejected",
        "hpke-negative",
        "2.1",
        surf,
        m,
        reject("aead-fail", "storage-quarantine"),
    ));

    out
}

// ---------------------------------------------------------- family 5

/// Family-5 KAT convention: `plane_id = [0;32]`, `zone_id = [0;16]`
/// (the §5.3 AADs bind them; the contract carries neither).
const F5_PLANE: Bytes32 = [0; 32];
const F5_ZONE: Bytes16 = [0; 16];

fn family5() -> Vec<Vector> {
    let surf: &[&str] = &["core", "native-crypto", "browser"];
    let mut out = Vec::new();

    let kek = [0x71u8; 32];
    let dek = [0x72u8; 32];
    let nonce = [0x73u8; 12];
    let plaintext = b"item plaintext under the zone key schedule".to_vec();

    let core = keyschedule::seal_item(&dek, nonce, &F5_PLANE, &F5_ZONE, &plaintext);
    let addr = keyschedule::item_addr(&core);
    let wrapped = keyschedule::wrap_dek(&kek, &F5_PLANE, &F5_ZONE, 1, &addr, &dek);
    let core_bytes = enc(&core.to_value());
    let wrap_bytes = enc(&crate::shapes::journal::Itemwrap {
        item_addr: addr,
        key_wrap_epoch: 1,
        wrapped_dek: wrapped,
    }
    .to_value());

    // Open direction: the whole §5.2/§5.3 chain — recompute the
    // address from the core bytes, derive the per-item wrap key,
    // unwrap the DEK, open the AEAD.
    let mut m = in2("kek", json!(hex(&kek)), "core", json!(hex(&core_bytes)));
    m.insert("wrap".into(), json!(hex(&wrap_bytes)));
    m.insert("kek_epoch".into(), json!(1));
    out.push(kat(
        5,
        "item-open-full-chain",
        "item-seal-open",
        "5.3",
        surf,
        m,
        Expected::Result(json!({ "plaintext": hex(&plaintext) })),
    ));

    // Rewrap byte-idempotence (I2): the deterministic wrapper.
    let again = keyschedule::wrap_dek(&kek, &F5_PLANE, &F5_ZONE, 1, &addr, &dek);
    assert_eq!(wrapped, again, "I2");
    let mut m = in2("item_addr", json!(hex(&addr)), "kek_epoch", json!(1));
    m.insert("kek".into(), json!(hex(&kek)));
    m.insert("dek".into(), json!(hex(&dek)));
    out.push(kat(
        5,
        "rewrap-byte-idempotence",
        "rewrap-idempotence",
        "5.3",
        surf,
        m,
        Expected::Result(json!({ "identical": true, "wrapper": hex(&wrapped) })),
    ));

    // wrapper-mismatch: the wrap names a DIFFERENT item_addr than the
    // core bytes derive — the address binding, before any AEAD.
    let mut wrong_addr = addr;
    wrong_addr[0] ^= 1;
    let wrong_wrap = enc(&crate::shapes::journal::Itemwrap {
        item_addr: wrong_addr,
        key_wrap_epoch: 1,
        wrapped_dek: wrapped,
    }
    .to_value());
    let mut m = in2("kek", json!(hex(&kek)), "core", json!(hex(&core_bytes)));
    m.insert("wrap".into(), json!(hex(&wrong_wrap)));
    m.insert("kek_epoch".into(), json!(1));
    out.push(kat(
        5,
        "wrapper-address-mismatch",
        "crypto-negative",
        "5.3",
        surf,
        m,
        reject("wrapper-mismatch", "storage-quarantine"),
    ));

    // AEAD tamper: address binding holds, the ciphertext does not.
    let mut bad_core = core.clone();
    bad_core.ct[0] ^= 1;
    // The wrap must name the TAMPERED core's address, or the mismatch
    // fires first — recompute.
    let bad_addr = keyschedule::item_addr(&bad_core);
    let bad_wrap = enc(&crate::shapes::journal::Itemwrap {
        item_addr: bad_addr,
        key_wrap_epoch: 1,
        wrapped_dek: keyschedule::wrap_dek(&kek, &F5_PLANE, &F5_ZONE, 1, &bad_addr, &dek),
    }
    .to_value());
    let mut m = in2(
        "kek",
        json!(hex(&kek)),
        "core",
        json!(hex(&enc(&bad_core.to_value()))),
    );
    m.insert("wrap".into(), json!(hex(&bad_wrap)));
    m.insert("kek_epoch".into(), json!(1));
    out.push(kat(
        5,
        "item-aead-tamper",
        "crypto-negative",
        "5.3",
        surf,
        m,
        reject("aead-fail", "storage-quarantine"),
    ));

    out
}

// ---------------------------------------------------------- family 6

fn head(lin: u8, gen: u64, seq: u64, op: u8) -> Head {
    Head {
        lineage: [lin; 16],
        gen,
        seq,
        op: [op; 32],
    }
}

fn frontier(heads: Vec<Head>) -> Frontier {
    Frontier {
        zone_id: [0x0a; 16],
        heads,
    }
}

/// A D-33 incorporated-position retirement event: the 3-key
/// `fencecoord` shape (an acceptance is the 4-key `head`).
fn retire_event(lin: u8, gen: u64, seq: u64) -> Value {
    cbor::map(vec![
        ("lineage", bytes(&[lin; 16])),
        ("gen", u(gen)),
        ("seq", u(seq)),
    ])
}

fn f6_fold(name: &str, initial: &Frontier, events: Vec<Value>, result: Json) -> Vector {
    let mut m = in1("initial", json!(hex(&enc(&initial.to_value()))));
    m.insert(
        "events".into(),
        json!(events.iter().map(|e| hex(&enc(e))).collect::<Vec<String>>()),
    );
    kat(
        6,
        name,
        "frontier-fold",
        "4.6",
        &["core"],
        m,
        Expected::Result(result),
    )
}

fn family6() -> Vec<Vector> {
    let mut out = Vec::new();

    // Sort key + replacement: a new lineage inserts in (lineage, gen)
    // order; a same-(lineage, gen) higher seq replaces.
    let init = frontier(vec![head(2, 1, 3, 0x21), head(5, 1, 1, 0x51)]);
    let folded = frontier(vec![
        head(1, 1, 1, 0x11),
        head(2, 1, 4, 0x22),
        head(5, 1, 1, 0x51),
    ]);
    out.push(f6_fold(
        "fold-sort-and-replace",
        &init,
        vec![
            head(1, 1, 1, 0x11).to_value(),
            head(2, 1, 4, 0x22).to_value(),
        ],
        json!({
            "frontier": hex(&enc(&folded.to_value())),
            "frontier_hash": hex(&folded.hash()),
        }),
    ));

    // Retirement drops the accepted head at or below the incorporated
    // position (D-33/D-188).
    let init = frontier(vec![head(2, 1, 2, 0x21), head(5, 1, 1, 0x51)]);
    let folded = frontier(vec![head(5, 1, 1, 0x51)]);
    out.push(f6_fold(
        "retire-at-or-below-drops",
        &init,
        vec![retire_event(2, 1, 3)],
        json!({
            "frontier": hex(&enc(&folded.to_value())),
            "frontier_hash": hex(&folded.hash()),
        }),
    ));

    // No accepted head at or below the position = a successful no-op
    // (§9.3/D-188).
    let init = frontier(vec![head(2, 1, 5, 0x21)]);
    out.push(f6_fold(
        "retire-above-head-noop",
        &init,
        vec![retire_event(2, 1, 3)],
        json!({
            "frontier": hex(&enc(&init.to_value())),
            "frontier_hash": hex(&init.hash()),
            "noop": true,
        }),
    ));

    // Negatives.
    let neg = |name: &str, initial: Vec<u8>, events: Vec<Vec<u8>>, o: &str, d: &str| {
        let mut m = in1("initial", json!(hex(&initial)));
        m.insert(
            "events".into(),
            json!(events.iter().map(|e| hex(e)).collect::<Vec<String>>()),
        );
        kat(
            6,
            name,
            "frontier-negative",
            "4.6",
            &["core"],
            m,
            reject(o, d),
        )
    };

    // Heads out of (lineage, gen) order — hand-built below the typed
    // layer (the builder sorts).
    let unsorted = enc(&cbor::map(vec![
        ("v", u(1)),
        ("zone_id", bytes(&[0x0a; 16])),
        (
            "heads",
            Value::Array(vec![
                head(5, 1, 1, 0x51).to_value(),
                head(2, 1, 3, 0x21).to_value(),
            ]),
        ),
    ]));
    out.push(neg(
        "unsorted-heads",
        unsorted,
        vec![enc(&head(1, 1, 1, 0x11).to_value())],
        "non-canonical",
        "reject-permanent",
    ));

    let dup = enc(&cbor::map(vec![
        ("v", u(1)),
        ("zone_id", bytes(&[0x0a; 16])),
        (
            "heads",
            Value::Array(vec![
                head(2, 1, 3, 0x21).to_value(),
                head(2, 1, 4, 0x22).to_value(),
            ]),
        ),
    ]));
    out.push(neg(
        "duplicate-lineage-gen-pair",
        dup,
        vec![enc(&head(1, 1, 1, 0x11).to_value())],
        "non-canonical",
        "reject-permanent",
    ));

    // Equal coordinates, differing hash: fork evidence.
    out.push(neg(
        "equal-coordinates-differing-hash",
        enc(&frontier(vec![head(2, 1, 3, 0x21)]).to_value()),
        vec![enc(&head(2, 1, 3, 0x99).to_value())],
        "fork",
        "freeze-writer",
    ));

    out
}

/// Families 1–6, in family order.
pub fn corpus() -> Vec<Vector> {
    let mut out = family1();
    out.extend(family2());
    out.extend(family3());
    out.extend(family4());
    out.extend(family5());
    out.extend(family6());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift gate: every committed corpus vector file byte-matches
    /// its builder (the tranche's gate, extended).
    #[test]
    fn committed_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus() {
            let path = dir.join(format!("f{:02}-{}.json", v.family, v.name));
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("{} not minted", path.display()));
            assert_eq!(
                committed,
                v.to_file_string(),
                "{} drifted from its builder",
                v.name
            );
        }
    }

    /// Every corpus vector passes the mint-time container/companion
    /// checks.
    #[test]
    fn corpus_vectors_check_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }

    /// The corpus is name-unique and family-ordered coverage exists
    /// for every family-1..6 case kind the companion names.
    #[test]
    fn corpus_covers_the_family_vocabularies() {
        use std::collections::BTreeSet;
        let vs = corpus();
        let names: BTreeSet<_> = vs.iter().map(|v| (v.family, v.name.clone())).collect();
        assert_eq!(names.len(), vs.len(), "duplicate vector names");
        let mut kinds: BTreeSet<(u8, String)> = BTreeSet::new();
        for v in &vs {
            kinds.insert((v.family, v.case_kind.clone()));
        }
        for (fam, kind) in [
            (1, "canonical-encode"),
            (1, "canonical-reject"),
            (1, "cap-fit"),
            (1, "cap-exceed"),
            (2, "hash-domain"),
            (2, "key-id-derive"),
            (2, "separation-negative"),
            (3, "sign-then-verify"),
            (3, "verify-fixed"),
            (4, "hpke-seal-open"),
            (4, "hpke-negative"),
            (5, "item-seal-open"),
            (5, "rewrap-idempotence"),
            (5, "crypto-negative"),
            (6, "frontier-fold"),
            (6, "frontier-negative"),
        ] {
            assert!(
                kinds.contains(&(fam, kind.to_string())),
                "family {fam} lacks {kind}"
            );
        }
    }
}
