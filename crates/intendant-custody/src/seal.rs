//! The sealed-blob envelope: ChaCha20-Poly1305 under a 32-byte wrapping
//! key, with the entry name as AAD so a blob cannot be swapped between
//! entries. Pure functions — the platform-independent heart of every
//! wrapped backend, tested un-gated on every platform.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::SecureRandom;

use crate::{CustodyError, Secret};

/// Format: magic ‖ nonce ‖ ciphertext+tag. The magic names the format and
/// its version; unsealing anything else is a named failure, never a guess.
const SEALED_MAGIC: &[u8; 8] = b"ICSTDv1\n";

pub(crate) fn seal(
    key: &[u8; 32],
    entry_name: &str,
    material: &[u8],
    rng: &dyn SecureRandom,
) -> Result<Vec<u8>, CustodyError> {
    let unbound =
        UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| CustodyError::Unsealable {
            name: entry_name.to_string(),
        })?;
    let sealing = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes).map_err(|_| CustodyError::Io {
        context: format!("seal {entry_name}"),
        message: "system randomness unavailable".to_string(),
    })?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut blob = Vec::with_capacity(
        SEALED_MAGIC.len() + NONCE_LEN + material.len() + CHACHA20_POLY1305.tag_len(),
    );
    blob.extend_from_slice(SEALED_MAGIC);
    blob.extend_from_slice(&nonce_bytes);
    let mut in_out = material.to_vec();
    sealing
        .seal_in_place_append_tag(nonce, Aad::from(entry_name.as_bytes()), &mut in_out)
        .map_err(|_| CustodyError::Unsealable {
            name: entry_name.to_string(),
        })?;
    blob.extend_from_slice(&in_out);
    // The plaintext copy used for in-place sealing now holds ciphertext;
    // nothing secret remains in it.
    Ok(blob)
}

pub(crate) fn unseal(
    key: &[u8; 32],
    entry_name: &str,
    blob: &[u8],
) -> Result<Secret, CustodyError> {
    let unsealable = || CustodyError::Unsealable {
        name: entry_name.to_string(),
    };
    let body = blob
        .strip_prefix(SEALED_MAGIC.as_slice())
        .ok_or_else(unsealable)?;
    if body.len() < NONCE_LEN + CHACHA20_POLY1305.tag_len() {
        return Err(unsealable());
    }
    let (nonce_bytes, ciphertext) = body.split_at(NONCE_LEN);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| unsealable())?;
    let opening = LessSafeKey::new(unbound);
    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| unsealable())?;
    let mut in_out = ciphertext.to_vec();
    let plaintext = opening
        .open_in_place(nonce, Aad::from(entry_name.as_bytes()), &mut in_out)
        .map_err(|_| unsealable())?;
    let secret = Secret::new(plaintext.to_vec());
    // `in_out` still holds a plaintext copy in its opened prefix.
    use zeroize::Zeroize as _;
    in_out.zeroize();
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;

    const KEY: [u8; 32] = [7u8; 32];

    #[test]
    fn seal_roundtrips_and_binds_the_entry_name() {
        let rng = SystemRandom::new();
        let blob = seal(&KEY, "access-certs/client.key", b"-----KEY-----", &rng).unwrap();
        let secret = unseal(&KEY, "access-certs/client.key", &blob).unwrap();
        assert_eq!(secret.as_bytes(), b"-----KEY-----");

        // A blob is bound to its entry name: swapping entries fails closed.
        let swapped = unseal(&KEY, "access-certs/server.key", &blob);
        assert!(matches!(swapped, Err(CustodyError::Unsealable { .. })));
    }

    #[test]
    fn tampered_wrong_key_and_truncated_blobs_fail_closed() {
        let rng = SystemRandom::new();
        let blob = seal(&KEY, "entry", b"material", &rng).unwrap();

        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            unseal(&KEY, "entry", &tampered),
            Err(CustodyError::Unsealable { .. })
        ));

        let wrong_key = [8u8; 32];
        assert!(matches!(
            unseal(&wrong_key, "entry", &blob),
            Err(CustodyError::Unsealable { .. })
        ));

        assert!(matches!(
            unseal(&KEY, "entry", &blob[..SEALED_MAGIC.len() + 3]),
            Err(CustodyError::Unsealable { .. })
        ));
        assert!(matches!(
            unseal(&KEY, "entry", b"not a sealed blob"),
            Err(CustodyError::Unsealable { .. })
        ));
    }

    #[test]
    fn nonces_differ_between_seals() {
        let rng = SystemRandom::new();
        let one = seal(&KEY, "entry", b"material", &rng).unwrap();
        let two = seal(&KEY, "entry", b"material", &rng).unwrap();
        assert_ne!(one, two, "fresh nonce per seal");
    }
}
