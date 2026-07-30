//! Identity of the Intendant release signing key.
//!
//! The repository commits the release PGP public key at
//! `RELEASE-SIGNING-KEY.asc` (embedded here at compile time); the release
//! pipeline stages that same file beside every release it publishes, and
//! every logged release manifest carries the key's primary fingerprint as
//! `pgp_fingerprint`. `intendant hosted-verify --releases` compares the
//! logged identity and the served key bytes against this compiled pin.
//! Cryptographic verification of the detached signatures themselves is
//! deliberately `gpg --verify`'s job (the documented ritual in
//! docs/src/getting-started.md): this module only answers "which key".
//! The fingerprint derivation below is `#[cfg(test)]` — it exists to
//! enforce the pinned constant against the committed bytes at merge
//! time, so the shipped binary never parses key material at all, let
//! alone network input.

#[cfg(test)]
use ring::digest;

/// The armored release public key as committed at the repo root
/// (`.gitattributes` pins it `-text`, so the embedded bytes are identical
/// on every platform's checkout). The release pipeline publishes this
/// exact file beside every release under [`RELEASE_SIGNING_KEY_ASSET`],
/// which is what lets the verifier demand byte-identity between the
/// served key and the reviewed one.
pub(crate) const RELEASE_SIGNING_PUBKEY_ASC: &str =
    include_str!("../../../RELEASE-SIGNING-KEY.asc");

/// Release-asset name of the public key — the repo filename, unchanged.
/// TWINNED with `bin/connect/ui.rs::RELEASE_KEY_ASSET_URL` (which must end
/// in this name), with `bin/connect/transparency.rs::RELEASE_SIGNING_KEY_ASSET`
/// (the log's submission gate), and with the staging `cp` in
/// `.github/workflows/release.yml`; tests on each side pin the literal.
pub(crate) const RELEASE_SIGNING_KEY_ASSET: &str = "RELEASE-SIGNING-KEY.asc";

/// Primary fingerprint of [`RELEASE_SIGNING_PUBKEY_ASC`], uppercase hex —
/// the compiled trust anchor for release verification. A unit test derives
/// the fingerprint from the embedded key and pins this constant, so the
/// two can only ever change together. Signing-subkey rotation does NOT
/// change this; replacing the primary key (compromise, escrow loss) does,
/// and is meant to be loud.
pub(crate) const RELEASE_SIGNING_KEY_FINGERPRINT: &str = "A9B389C058DD177B3303A13522FC08F0A26D3D18";

/// `pgp_fingerprint` vocabulary: a v4 (40 hex) or v6 (64 hex) OpenPGP
/// fingerprint, uppercase only. TWINNED with
/// `bin/connect/transparency.rs::valid_pgp_fingerprint` (the log rejects
/// what the verifier would not read); change both together.
pub(crate) fn valid_pgp_fingerprint(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c))
}

/// Decode one ASCII-armored OpenPGP block: BEGIN line, optional armor
/// headers, blank separator, base64 body, optional `=` CRC-24 line, END
/// line. Tolerates `\r\n` (a Windows checkout of the key file would still
/// parse; byte-identity checks separately pin the exact committed bytes).
/// Test-only, like the rest of the derivation chain below: the shipped
/// binary carries just the derived constant.
#[cfg(test)]
fn armor_decode(armored: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let mut in_block = false;
    let mut past_headers = false;
    let mut b64 = String::new();
    for raw in armored.lines() {
        let line = raw.trim_end_matches('\r');
        if !in_block {
            if line.starts_with("-----BEGIN PGP ") {
                in_block = true;
            }
            continue;
        }
        if line.starts_with("-----END PGP ") {
            if b64.is_empty() {
                return Err("PGP armor block carries no data".to_string());
            }
            return base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| format!("PGP armor base64 does not decode: {e}"));
        }
        if !past_headers {
            if line.is_empty() {
                past_headers = true;
            } else if !line.contains(':') {
                // Headerless armor: the first data line follows BEGIN.
                past_headers = true;
                b64.push_str(line);
            }
            continue;
        }
        if line.starts_with('=') {
            // CRC-24 armor checksum line; the base64 body above is what
            // the fingerprint derivation consumes.
            continue;
        }
        if !line.is_empty() {
            b64.push_str(line);
        }
    }
    Err("no complete PGP armor block found".to_string())
}

#[cfg(test)]
fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// Fingerprint of one public-key packet body. v4: SHA-1 over
/// `0x99 || 2-octet length || body` (RFC 4880 §12.2 — SHA-1 here is the
/// fingerprint *definition*, used as an identifier of a trusted
/// compiled-in key, not as a collision-resistant integrity check); v6:
/// SHA-256 over `0x9B || 4-octet length || body` (RFC 9580 §5.5.4).
#[cfg(test)]
fn fingerprint_of_key_packet(body: &[u8]) -> Result<String, String> {
    match body.first() {
        Some(4) => {
            let len = u16::try_from(body.len())
                .map_err(|_| "v4 public-key packet exceeds its length field".to_string())?;
            let mut material = Vec::with_capacity(body.len() + 3);
            material.push(0x99);
            material.extend_from_slice(&len.to_be_bytes());
            material.extend_from_slice(body);
            Ok(hex_upper(
                digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &material).as_ref(),
            ))
        }
        Some(6) => {
            let len = u32::try_from(body.len())
                .map_err(|_| "v6 public-key packet exceeds its length field".to_string())?;
            let mut material = Vec::with_capacity(body.len() + 5);
            material.push(0x9B);
            material.extend_from_slice(&len.to_be_bytes());
            material.extend_from_slice(body);
            Ok(hex_upper(
                digest::digest(&digest::SHA256, &material).as_ref(),
            ))
        }
        Some(version) => Err(format!("unsupported public-key packet version {version}")),
        None => Err("empty public-key packet".to_string()),
    }
}

/// Primary-key fingerprint of the first public-key packet (tag 6) in an
/// armored key block, uppercase hex. Walks RFC 4880 packet framing (old
/// and new formats); rejects indeterminate and partial lengths, which
/// never appear in exported key material. Test-only: the derivation
/// exists to enforce the constant's honesty at merge time, so the
/// shipped binary never parses key material at all.
#[cfg(test)]
pub(crate) fn primary_fingerprint_hex(armored: &str) -> Result<String, String> {
    let data = armor_decode(armored)?;
    let mut offset = 0usize;
    while offset < data.len() {
        let header = data[offset];
        if header & 0x80 == 0 {
            return Err("invalid OpenPGP packet header".to_string());
        }
        let truncated = || "truncated OpenPGP packet length".to_string();
        let (tag, body_start, body_len) = if header & 0x40 != 0 {
            let tag = header & 0x3F;
            let first = *data.get(offset + 1).ok_or_else(truncated)?;
            let (len, len_octets) = match first {
                0..=191 => (first as usize, 1usize),
                192..=223 => {
                    let second = *data.get(offset + 2).ok_or_else(truncated)?;
                    ((((first as usize) - 192) << 8) + second as usize + 192, 2)
                }
                255 => {
                    let bytes: [u8; 4] = data
                        .get(offset + 2..offset + 6)
                        .ok_or_else(truncated)?
                        .try_into()
                        .expect("sliced exactly four octets");
                    (u32::from_be_bytes(bytes) as usize, 5)
                }
                _ => return Err("partial-length OpenPGP packet in key material".to_string()),
            };
            (tag, offset + 1 + len_octets, len)
        } else {
            let tag = (header >> 2) & 0x0F;
            let (len, len_octets) = match header & 0x03 {
                0 => (
                    *data.get(offset + 1).ok_or_else(truncated)? as usize,
                    1usize,
                ),
                1 => {
                    let bytes: [u8; 2] = data
                        .get(offset + 1..offset + 3)
                        .ok_or_else(truncated)?
                        .try_into()
                        .expect("sliced exactly two octets");
                    (u16::from_be_bytes(bytes) as usize, 2)
                }
                2 => {
                    let bytes: [u8; 4] = data
                        .get(offset + 1..offset + 5)
                        .ok_or_else(truncated)?
                        .try_into()
                        .expect("sliced exactly four octets");
                    (u32::from_be_bytes(bytes) as usize, 4)
                }
                _ => return Err("indeterminate-length OpenPGP packet in key material".to_string()),
            };
            (tag, offset + 1 + len_octets, len)
        };
        let body = data
            .get(body_start..body_start + body_len)
            .ok_or("truncated OpenPGP packet body")?;
        if tag == 6 {
            return fingerprint_of_key_packet(body);
        }
        offset = body_start + body_len;
    }
    Err("no public-key packet in armor".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code-carried pin: the compiled fingerprint constant derives
    /// from the committed key bytes, or the build's trust anchor is lying.
    #[test]
    fn embedded_release_key_fingerprint_is_pinned() {
        assert_eq!(
            primary_fingerprint_hex(RELEASE_SIGNING_PUBKEY_ASC).expect("committed key parses"),
            RELEASE_SIGNING_KEY_FINGERPRINT,
        );
        assert!(valid_pgp_fingerprint(RELEASE_SIGNING_KEY_FINGERPRINT));
        assert_eq!(
            RELEASE_SIGNING_KEY_FINGERPRINT.len(),
            40,
            "the committed release key is a v4 key"
        );
    }

    /// A CRLF-mangled checkout (Windows autocrlf) must still parse to the
    /// same identity — byte-identity of the SERVED asset is a separate,
    /// stricter check.
    #[test]
    fn crlf_armor_parses_to_the_same_fingerprint() {
        let crlf = RELEASE_SIGNING_PUBKEY_ASC.replace('\n', "\r\n");
        assert_eq!(
            primary_fingerprint_hex(&crlf).expect("CRLF armor parses"),
            RELEASE_SIGNING_KEY_FINGERPRINT,
        );
    }

    #[test]
    fn armor_decode_rejects_garbage() {
        for bad in [
            "",
            "not armor at all",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n-----END PGP PUBLIC KEY BLOCK-----\n",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n!!!!\n-----END PGP PUBLIC KEY BLOCK-----\n",
        ] {
            assert!(primary_fingerprint_hex(bad).is_err(), "must reject {bad:?}");
        }
        // Valid base64 that is not OpenPGP packet framing.
        assert!(primary_fingerprint_hex(
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\naGVsbG8=\n-----END PGP PUBLIC KEY BLOCK-----\n"
        )
        .is_err());
    }

    #[test]
    fn fingerprint_vocabulary_is_uppercase_hex_of_v4_or_v6_width() {
        assert!(valid_pgp_fingerprint(RELEASE_SIGNING_KEY_FINGERPRINT));
        assert!(valid_pgp_fingerprint(&"AB12".repeat(10)));
        assert!(valid_pgp_fingerprint(&"AB12".repeat(16)));
        for bad in [
            String::new(),
            "abcd".to_string(),
            RELEASE_SIGNING_KEY_FINGERPRINT.to_lowercase(),
            RELEASE_SIGNING_KEY_FINGERPRINT[..39].to_string(),
            format!("{RELEASE_SIGNING_KEY_FINGERPRINT}A"),
            "G".repeat(40),
        ] {
            assert!(!valid_pgp_fingerprint(&bad), "must reject {bad:?}");
        }
    }

    /// The release workflow and this module can only change together: the
    /// staged asset name, the secret names the gate demands, the signing
    /// script path, and the tag/manual-only trigger set are all pinned
    /// here (the ui.rs stamp-sentinel idiom from PR #656).
    #[test]
    fn release_workflow_carries_the_pgp_lane() {
        const RELEASE_YML: &str = include_str!("../../../.github/workflows/release.yml");
        for needle in [
            RELEASE_SIGNING_KEY_ASSET,
            "PGP_SIGN_KEY_B64",
            "PGP_SIGN_KEY_PASSPHRASE",
            "scripts/release-pgp-sign.sh",
            "pgp_fingerprint",
        ] {
            assert!(
                RELEASE_YML.contains(needle),
                "release.yml must carry {needle}"
            );
        }
        // Tag/manual-only: the release pipeline must never gate the merge
        // queue or run per-PR. Pin the `on:` trigger block itself — the
        // header comment is allowed to NAME the forbidden triggers while
        // explaining why they are absent.
        let mut on_block = String::new();
        let mut in_on = false;
        for line in RELEASE_YML.lines() {
            if line == "on:" {
                in_on = true;
                continue;
            }
            if in_on {
                if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
                    break;
                }
                on_block.push_str(line);
                on_block.push('\n');
            }
        }
        assert!(
            on_block.contains("tags: [\"v*\"]"),
            "release.yml on-block must trigger on v* tags: {on_block:?}"
        );
        assert!(
            on_block.contains("workflow_dispatch:"),
            "release.yml on-block must keep manual dispatch: {on_block:?}"
        );
        for forbidden in ["pull_request", "merge_group", "schedule", "push:\n    branches"] {
            assert!(
                !on_block.contains(forbidden),
                "release.yml must stay tag/manual-only (found {forbidden} in the on-block)"
            );
        }
    }

    /// Twin pin: the asset literal shared with bin/connect (ui.rs URL tail
    /// and transparency.rs submission gate) and .github/workflows/release.yml.
    #[test]
    fn release_key_asset_name_is_the_repo_filename() {
        assert_eq!(RELEASE_SIGNING_KEY_ASSET, "RELEASE-SIGNING-KEY.asc");
    }
}
