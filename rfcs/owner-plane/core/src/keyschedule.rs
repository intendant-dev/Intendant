//! §5 item crypto — the key schedule.
//!
//! DEK → ItemCore (§5.1), KEK → recipient wraps (§5.2), and the
//! per-item derived wrap key + DEK wrap (§5.3), plus §2.4's HKDF
//! stage of the recovery derivation. The five context strings below
//! are the ENTIRE closed info/AAD inventory (§1): adding one is a
//! version event, never an implementation choice.
//!
//! Randomness discipline: nothing here draws randomness. DEKs, KEKs,
//! nonces, and HPKE ephemeral ikm are caller inputs so vectors control
//! every byte (§13.1).
//!
//! BIP-39 itself (wordlist, checksum, PBKDF2) is NOT here — it arrives
//! with the recovery vector family; [`recovery_ed25519_seed`] covers
//! the HKDF stage from the 64-byte BIP-39 seed onward.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::cbor;
use crate::domains::{h_tag, Tag};
use crate::shapes::journal::Itemcore;
use crate::shapes::{Bytes16, Bytes32, ToValue};
use crate::suite::{aead, hpke_wrap};

/// §5.1 item AEAD AAD prefix.
pub const CTX_ITEM: &[u8] = b"intendant/item/v1";
/// §5.3 DEK-wrap AEAD AAD prefix.
pub const CTX_DEKWRAP: &[u8] = b"intendant/dekwrap/v1";
/// §5.2 KEK HPKE info = aad prefix.
pub const CTX_KEK: &[u8] = b"intendant/kek/v1";
/// §5.3 wrap-key HKDF-Extract salt.
pub const CTX_WRAPKEY: &[u8] = b"intendant/wrapkey/v1";
/// §2.4 recovery HKDF-Extract salt.
pub const CTX_RECOVERY: &[u8] = b"intendant/recovery/v1";

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// HKDF-SHA256: Expand(Extract(salt, ikm), info, 32).
fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is within HKDF-SHA256 output bounds");
    okm
}

// ---------------------------------------------------------------- §5.1

/// `"intendant/item/v1" || 0x00 || plane_id || zone_id`.
pub fn item_aad(plane_id: &Bytes32, zone_id: &Bytes16) -> Vec<u8> {
    concat(&[CTX_ITEM, &[0x00], plane_id, zone_id])
}

/// §5.1: seal the exact SignedOperation triple bytes under a fresh
/// DEK (used exactly once, ever — caller's invariant; the nonce is a
/// caller input because vectors control randomness).
pub fn seal_item(
    dek: &[u8; 32],
    nonce: [u8; 12],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    op_bytes: &[u8],
) -> Itemcore {
    let ct = aead::seal(dek, &nonce, &item_aad(plane_id, zone_id), op_bytes);
    Itemcore { nonce, ct }
}

/// §5.1 inverse. `None` = AEAD failure (wrong key, wrong zone/plane
/// binding, or tamper); I4's post-decryption validation is the
/// caller's.
pub fn open_item(
    dek: &[u8; 32],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    core: &Itemcore,
) -> Option<Vec<u8>> {
    aead::open(dek, &core.nonce, &item_aad(plane_id, zone_id), &core.ct)
}

/// `item_addr = H_item(ItemCore)` — covers version+alg+nonce+ct.
pub fn item_addr(core: &Itemcore) -> Bytes32 {
    h_tag(
        Tag::Item,
        &cbor::encode(&core.to_value()).expect("itemcore encodes"),
    )
}

// ---------------------------------------------------------------- §5.2

/// `"intendant/kek/v1" || 0x00 || plane_id || zone_id || epoch_be64`
/// — both the HPKE `info` and the AEAD `aad` of a recipient wrap.
pub fn kek_context(plane_id: &Bytes32, zone_id: &Bytes16, epoch: u64) -> Vec<u8> {
    concat(&[CTX_KEK, &[0x00], plane_id, zone_id, &epoch.to_be_bytes()])
}

/// §5.2: HPKE base-mode Seal of the 32-byte KEK to the recipient
/// certificate's KEM key. Returns the kekwrap's `(enc: 65 B, ct: 48 B)`;
/// `None` = malformed recipient key (`key-malformed`).
pub fn wrap_kek(
    recipient_kem_pk: &[u8; 65],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    epoch: u64,
    kek: &[u8; 32],
    eph_ikm: &[u8; 32],
) -> Option<([u8; 65], [u8; 48])> {
    let ctx = kek_context(plane_id, zone_id, epoch);
    let (enc, ct) = hpke_wrap::seal(recipient_kem_pk, &ctx, &ctx, kek, eph_ikm)?;
    let ct: [u8; 48] = ct
        .as_slice()
        .try_into()
        .expect("32-byte KEK seals to 32 B ct + 16 B tag");
    Some((enc, ct))
}

/// §5.2 inverse. `None` covers malformed keys and AEAD failure alike.
pub fn open_kek(
    recipient_sk: &[u8; 32],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    epoch: u64,
    enc: &[u8],
    ct: &[u8],
) -> Option<[u8; 32]> {
    let ctx = kek_context(plane_id, zone_id, epoch);
    let pt = hpke_wrap::open(recipient_sk, enc, &ctx, &ctx, ct)?;
    pt.as_slice().try_into().ok()
}

// ---------------------------------------------------------------- §5.3

/// `wrap_key = HKDF-Expand(HKDF-Extract(salt = "intendant/wrapkey/v1",
/// IKM = KEK), info = item_addr, L = 32)` — the derived single-use
/// wrapping key: key/nonce uniqueness by construction (the fixed
/// zero nonce below is sound because each `(KEK, item_addr)` key
/// encrypts exactly once).
pub fn wrap_key(kek: &[u8; 32], item_addr: &Bytes32) -> [u8; 32] {
    hkdf32(CTX_WRAPKEY, kek, item_addr)
}

/// `"intendant/dekwrap/v1" || 0x00 || plane_id || zone_id ||
/// kek_epoch_be64 || item_addr`.
pub fn dekwrap_aad(
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    kek_epoch: u64,
    item_addr: &Bytes32,
) -> Vec<u8> {
    concat(&[
        CTX_DEKWRAP,
        &[0x00],
        plane_id,
        zone_id,
        &kek_epoch.to_be_bytes(),
        item_addr,
    ])
}

/// §5.3: wrap the DEK under the derived per-item key with the fixed
/// 12-zero-byte nonce. Output layout: 32 B ct ‖ 16 B tag = 48 B (the
/// itemwrap's `wrapped_dek`). Byte-idempotent per `(KEK, item_addr)`
/// pair (I2) — a differing duplicate is corruption or fork evidence.
pub fn wrap_dek(
    kek: &[u8; 32],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    kek_epoch: u64,
    item_addr: &Bytes32,
    dek: &[u8; 32],
) -> [u8; 48] {
    let wk = wrap_key(kek, item_addr);
    let aad = dekwrap_aad(plane_id, zone_id, kek_epoch, item_addr);
    aead::seal(&wk, &[0u8; 12], &aad, dek)
        .as_slice()
        .try_into()
        .expect("32-byte DEK wraps to 32 B ct + 16 B tag")
}

/// §5.3 inverse. `None` = AEAD failure (wrong KEK/epoch/zone/plane
/// binding, or tamper).
pub fn unwrap_dek(
    kek: &[u8; 32],
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    kek_epoch: u64,
    item_addr: &Bytes32,
    wrapped_dek: &[u8; 48],
) -> Option<[u8; 32]> {
    let wk = wrap_key(kek, item_addr);
    let aad = dekwrap_aad(plane_id, zone_id, kek_epoch, item_addr);
    let pt = aead::open(&wk, &[0u8; 12], &aad, wrapped_dek)?;
    pt.as_slice().try_into().ok()
}

// ---------------------------------------------------------------- §2.4

/// §2.4's HKDF stage: `ed25519_seed = HKDF-Expand(HKDF-Extract(
/// salt="intendant/recovery/v1", IKM=seed), info="ed25519-seed",
/// L=32)` — from the 64-byte BIP-39 seed.
pub fn recovery_ed25519_seed(seed: &[u8; 64]) -> [u8; 32] {
    hkdf32(CTX_RECOVERY, seed, b"ed25519-seed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::assert_pins;

    /// Verbatim spec substrings this module implements (§1 closed
    /// inventory, §5.1–§5.3, §2.4).
    const SPEC_PINS: &[&str] = &[
        // §1 — the closed context-string inventory.
        "`intendant/item/v1`, `intendant/dekwrap/v1`, `intendant/kek/v1`,
`intendant/wrapkey/v1`, `intendant/recovery/v1`. No other context
strings exist; adding one is a version event.",
        // §5.1 — item AEAD.
        r#"item_ct = AES-256-GCM-Encrypt(key = DEK, nonce = random 12 B,
            aad = "intendant/item/v1" || 0x00 || plane_id || zone_id,
            plaintext = exact SignedOperation triple bytes)
ItemCore = { v: 1, aead: "a256gcm", nonce, ct: item_ct }"#,
        "item_addr = H_item(ItemCore)",
        "# covers version+alg+nonce+ct",
        // §5.2 — the recipient-wrap HPKE contract.
        r#"# HPKE base mode Seal to the recipient certificate's KEM key,
# info = aad = "intendant/kek/v1" || 0x00 || plane_id || zone_id || epoch_be64,
# plaintext = the 32-byte KEK. Rotation MUST set new_epoch = current + 1
# with a FRESH random KEK; the wrap is fully self-identifying."#,
        // §5.3 — the derived wrap key + DEK wrap.
        r#"wrap_key    = HKDF-Expand(HKDF-Extract(salt = "intendant/wrapkey/v1",
                IKM = KEK), info = item_addr, L = 32)
wrapped_dek = AES-256-GCM-Encrypt(key = wrap_key,
                nonce = 12 zero bytes,
                aad = "intendant/dekwrap/v1" || 0x00 || plane_id ||
                      zone_id || kek_epoch_be64 || item_addr,
                plaintext = the 32-byte DEK)"#,
        "# ciphertext layout: 32 B ct || 16 B tag = 48 B",
        "Each `(KEK, item_addr)` derives one wrap key used for exactly one
encryption; rewrapping the same pair stays byte-idempotent.",
        // §2.4 — the recovery HKDF stage.
        r#"`ed25519_seed = HKDF-Expand(HKDF-Extract(salt="intendant/recovery/v1",
IKM=seed), info="ed25519-seed", L=32)`."#,
    ];

    #[test]
    fn spec_pins_are_verbatim() {
        assert_pins(SPEC_PINS);
    }

    #[test]
    fn context_strings_closed_and_distinct() {
        let all: [&[u8]; 5] = [CTX_ITEM, CTX_DEKWRAP, CTX_KEK, CTX_WRAPKEY, CTX_RECOVERY];
        for (i, a) in all.iter().enumerate() {
            assert!(a.starts_with(b"intendant/") && a.ends_with(b"/v1"));
            assert!(!a.contains(&0x00), "0x00 is the frame separator");
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 5869 A.1 (Test Case 1, SHA-256) — pins the hkdf crate.
    #[test]
    fn rfc5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut okm = [0u8; 42];
        hk.expand(&info, &mut okm).unwrap();
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    /// RFC 5869 A.2 (Test Case 2, SHA-256, longer inputs/outputs).
    #[test]
    fn rfc5869_test_case_2() {
        let ikm: Vec<u8> = (0x00u8..=0x4f).collect();
        let salt: Vec<u8> = (0x60u8..=0xaf).collect();
        let info: Vec<u8> = (0xb0u8..=0xff).collect();
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut okm = [0u8; 82];
        hk.expand(&info, &mut okm).unwrap();
        assert_eq!(
            hex(&okm),
            "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c\
             59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71\
             cc30c58179ec3e87c14c01d5c1f3434f1d87"
        );
    }

    fn ids() -> (Bytes32, Bytes16) {
        ([0xA1u8; 32], [0xB2u8; 16])
    }

    #[test]
    fn aad_layouts_exact() {
        let (plane, zone) = ids();
        let aad = item_aad(&plane, &zone);
        assert_eq!(&aad[..17], b"intendant/item/v1");
        assert_eq!(aad[17], 0x00);
        assert_eq!(&aad[18..50], &plane);
        assert_eq!(&aad[50..], &zone);

        let ctx = kek_context(&plane, &zone, 7);
        assert_eq!(&ctx[..16], b"intendant/kek/v1");
        assert_eq!(ctx[16], 0x00);
        assert_eq!(&ctx[17..49], &plane);
        assert_eq!(&ctx[49..65], &zone);
        assert_eq!(&ctx[65..], &7u64.to_be_bytes());

        let addr = [0xC3u8; 32];
        let aad = dekwrap_aad(&plane, &zone, 7, &addr);
        assert_eq!(&aad[..20], b"intendant/dekwrap/v1");
        assert_eq!(aad[20], 0x00);
        assert_eq!(&aad[21..53], &plane);
        assert_eq!(&aad[53..69], &zone);
        assert_eq!(&aad[69..77], &7u64.to_be_bytes());
        assert_eq!(&aad[77..], &addr);
    }

    #[test]
    fn item_roundtrip_and_addr() {
        let (plane, zone) = ids();
        let dek = [0x11u8; 32];
        let op = b"exact SignedOperation triple bytes".to_vec();
        let core = seal_item(&dek, [0x22; 12], &plane, &zone, &op);
        assert_eq!(core.ct.len(), op.len() + 16);
        assert_eq!(open_item(&dek, &plane, &zone, &core).unwrap(), op);

        // item_addr = H_item over the canonical ItemCore bytes.
        let expect = h_tag(Tag::Item, &cbor::encode(&core.to_value()).unwrap());
        assert_eq!(item_addr(&core), expect);

        // The AAD binds plane and zone.
        assert!(open_item(&dek, &[0u8; 32], &zone, &core).is_none());
        assert!(open_item(&dek, &plane, &[0u8; 16], &core).is_none());
        // Wrong DEK fails.
        assert!(open_item(&[0u8; 32], &plane, &zone, &core).is_none());
    }

    #[test]
    fn kek_wrap_roundtrip_sizes_and_binding() {
        let (plane, zone) = ids();
        let (sk, pk) = hpke_wrap::derive_keypair(&[0x33u8; 32]);
        let kek = [0x44u8; 32];
        let (enc, ct) = wrap_kek(&pk, &plane, &zone, 1, &kek, &[0x55u8; 32]).unwrap();
        assert_eq!(enc.len(), 65);
        assert_eq!(ct.len(), 48);
        assert_eq!(open_kek(&sk, &plane, &zone, 1, &enc, &ct), Some(kek));

        // Deterministic under the same ephemeral ikm.
        let again = wrap_kek(&pk, &plane, &zone, 1, &kek, &[0x55u8; 32]).unwrap();
        assert_eq!((enc, ct), again);

        // info = aad binds epoch and zone: any change fails the open.
        assert_eq!(open_kek(&sk, &plane, &zone, 2, &enc, &ct), None);
        assert_eq!(open_kek(&sk, &plane, &[0u8; 16], 1, &enc, &ct), None);
        // Wrong recipient key fails.
        let (sk2, _) = hpke_wrap::derive_keypair(&[0x66u8; 32]);
        assert_eq!(open_kek(&sk2, &plane, &zone, 1, &enc, &ct), None);
    }

    #[test]
    fn dek_wrap_roundtrip_idempotence_and_binding() {
        let (plane, zone) = ids();
        let kek = [0x77u8; 32];
        let addr = [0x88u8; 32];
        let dek = [0x99u8; 32];

        let w1 = wrap_dek(&kek, &plane, &zone, 3, &addr, &dek);
        assert_eq!(w1.len(), 48);
        assert_eq!(unwrap_dek(&kek, &plane, &zone, 3, &addr, &w1), Some(dek));

        // I2: byte-idempotent per (KEK, item_addr).
        assert_eq!(w1, wrap_dek(&kek, &plane, &zone, 3, &addr, &dek));

        // Distinct item_addr ⇒ distinct wrap key ⇒ distinct bytes.
        assert_ne!(wrap_key(&kek, &addr), wrap_key(&kek, &[0u8; 32]));
        assert_ne!(w1, wrap_dek(&kek, &plane, &zone, 3, &[0u8; 32], &dek));

        // The AAD binds epoch/zone/plane/addr; the KDF binds the KEK.
        assert_eq!(unwrap_dek(&kek, &plane, &zone, 4, &addr, &w1), None);
        assert_eq!(unwrap_dek(&kek, &plane, &[0u8; 16], 3, &addr, &w1), None);
        assert_eq!(unwrap_dek(&kek, &[0u8; 32], &zone, 3, &addr, &w1), None);
        assert_eq!(unwrap_dek(&[0u8; 32], &plane, &zone, 3, &addr, &w1), None);

        // Tamper anywhere in ct‖tag fails.
        let mut bad = w1;
        bad[0] ^= 1;
        assert_eq!(unwrap_dek(&kek, &plane, &zone, 3, &addr, &bad), None);
        let mut bad = w1;
        bad[47] ^= 1;
        assert_eq!(unwrap_dek(&kek, &plane, &zone, 3, &addr, &bad), None);
    }

    #[test]
    fn recovery_seed_stage() {
        let seed = [0xABu8; 64];
        let a = recovery_ed25519_seed(&seed);
        assert_eq!(a, recovery_ed25519_seed(&seed));
        assert_ne!(a, recovery_ed25519_seed(&[0xACu8; 64]));
        // Domain separation from the wrap-key schedule: same-shaped
        // inputs under the two salts diverge.
        let ikm = [0xABu8; 32];
        assert_ne!(
            hkdf32(CTX_RECOVERY, &ikm, b"ed25519-seed"),
            hkdf32(CTX_WRAPKEY, &ikm, b"ed25519-seed")
        );
    }
}
