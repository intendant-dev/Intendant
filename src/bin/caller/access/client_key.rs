//! Browser client identity keys.
//!
//! The anchor-daemon trust model (see docs/src/trust-architecture.md) gives
//! every browser a durable identity: a WebCrypto P-256 keypair whose private
//! key never leaves the browser's origin-scoped storage. Dashboard-control
//! offers carry the public key plus a signature binding the offer to this
//! daemon, the session nonce, and the SDP, so any daemon can resolve the key
//! fingerprint against its local IAM without trusting the signaling path.
//!
//! Wire format (all base64url, no padding):
//! - `client_key`: the 65-byte uncompressed SEC1 point (`0x04 || x || y`),
//!   exactly what WebCrypto `exportKey("raw")` produces for ECDSA P-256.
//! - `client_key_sig`: the 64-byte fixed-form `r || s` signature over the
//!   payload below, exactly what WebCrypto `sign()` produces (IEEE P1363).
//! - `client_key_ts`: signer's unix time in milliseconds, replay-bounded.
//!
//! Signed payload, newline-joined to avoid JSON canonicalization pitfalls:
//!
//! ```text
//! intendant-client-key-offer-v1
//! {daemon_id}            // "" on the daemon-local signaling path
//! {client_nonce}
//! {sdp_sha256_b64u}
//! {ts_unix_ms}
//! ```

use crate::daemon_identity::b64u;
use base64::Engine as _;

pub const CLIENT_KEY_OFFER_PROTOCOL: &str = "intendant-client-key-offer-v1";
/// v2 extends the signed payload with the browser's own account claim
/// (`\n{account_user_id}\n{account_name}`), so the account shown on a
/// pending enrollment can be **attested by the device key** instead of
/// taken from whatever the signaling relay asserts. Old browsers keep
/// signing v1; a v1 offer carrying account fields is rejected outright —
/// nothing may ride outside the signature.
pub const CLIENT_KEY_OFFER_PROTOCOL_V2: &str = "intendant-client-key-offer-v2";

/// Accept signatures whose timestamp is at most this far from daemon time in
/// either direction. Generous enough for clock skew, small enough that a
/// captured offer is useless quickly (replay additionally requires reusing
/// the nonce and SDP digest of a live handshake).
pub const CLIENT_KEY_MAX_SKEW_MS: i64 = 5 * 60 * 1000;

const UNCOMPRESSED_P256_POINT_LEN: usize = 65;
const FIXED_ECDSA_P256_SIG_LEN: usize = 64;

/// A client key that passed signature verification for a specific offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClientKey {
    /// base64url(sha256(raw public key point)). The stable IAM binding value.
    pub fingerprint: String,
    /// base64url of the raw public key point, retained for display/audit.
    pub public_key_b64u: String,
    /// `(account_user_id, account_name)` covered by a v2 signature — the
    /// device key itself vouched for this account claim. `None` on v1
    /// offers (any account hint then rests on the relay's word).
    pub attested_account: Option<(String, String)>,
}

/// Stable fingerprint for a raw P-256 public key point.
pub fn client_key_fingerprint(raw_point: &[u8]) -> String {
    b64u(ring::digest::digest(&ring::digest::SHA256, raw_point).as_ref())
}

/// Shape check for a value claiming to be a [`client_key_fingerprint`]:
/// unpadded base64url of a SHA-256 digest — exactly 43 characters of the
/// base64url alphabet. Lets CLI boundaries reject typos and placeholders
/// before they get pinned as root authority.
#[cfg(test)]
pub fn is_client_key_fingerprint(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The exact byte string a client signs for a v1 offer.
pub fn client_key_offer_payload(
    daemon_id: &str,
    client_nonce: &str,
    sdp: &str,
    ts_unix_ms: i64,
) -> Vec<u8> {
    let sdp_digest = b64u(ring::digest::digest(&ring::digest::SHA256, sdp.as_bytes()).as_ref());
    format!("{CLIENT_KEY_OFFER_PROTOCOL}\n{daemon_id}\n{client_nonce}\n{sdp_digest}\n{ts_unix_ms}")
        .into_bytes()
}

/// The exact byte string a client signs for a v2 offer (v1 plus the
/// browser's own account claim). Mirrored by the dashboard JS signer.
pub fn client_key_offer_payload_v2(
    daemon_id: &str,
    client_nonce: &str,
    sdp: &str,
    ts_unix_ms: i64,
    account_user_id: &str,
    account_name: &str,
) -> Vec<u8> {
    let sdp_digest = b64u(ring::digest::digest(&ring::digest::SHA256, sdp.as_bytes()).as_ref());
    format!(
        "{CLIENT_KEY_OFFER_PROTOCOL_V2}\n{daemon_id}\n{client_nonce}\n{sdp_digest}\n{ts_unix_ms}\n{account_user_id}\n{account_name}"
    )
    .into_bytes()
}

/// Verify a signed offer. `daemon_id` must be the daemon's own expectation
/// (its rendezvous id, or "" on the local signaling path); the caller decides
/// what nonce/SDP the session is actually using — verification binds the
/// signature to those exact values, so a signaling relay cannot splice a
/// signature onto a different handshake.
pub fn verify_client_key_offer(
    client_key_b64u: &str,
    signature_b64u: &str,
    ts_unix_ms: i64,
    daemon_id: &str,
    client_nonce: &str,
    sdp: &str,
    now_unix_ms: i64,
) -> Result<VerifiedClientKey, String> {
    let payload = client_key_offer_payload(daemon_id, client_nonce, sdp, ts_unix_ms);
    verify_signed_offer(
        client_key_b64u,
        signature_b64u,
        ts_unix_ms,
        now_unix_ms,
        &payload,
    )
    .map(|(fingerprint, public_key_b64u)| VerifiedClientKey {
        fingerprint,
        public_key_b64u,
        attested_account: None,
    })
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub fn verify_client_key_offer_v2(
    client_key_b64u: &str,
    signature_b64u: &str,
    ts_unix_ms: i64,
    daemon_id: &str,
    client_nonce: &str,
    sdp: &str,
    now_unix_ms: i64,
    account_user_id: &str,
    account_name: &str,
) -> Result<VerifiedClientKey, String> {
    let payload = client_key_offer_payload_v2(
        daemon_id,
        client_nonce,
        sdp,
        ts_unix_ms,
        account_user_id,
        account_name,
    );
    verify_signed_offer(
        client_key_b64u,
        signature_b64u,
        ts_unix_ms,
        now_unix_ms,
        &payload,
    )
    .map(|(fingerprint, public_key_b64u)| VerifiedClientKey {
        fingerprint,
        public_key_b64u,
        attested_account: Some((account_user_id.to_string(), account_name.to_string())),
    })
}

/// Shared key/signature/timestamp mechanics for every payload version.
fn verify_signed_offer(
    client_key_b64u: &str,
    signature_b64u: &str,
    ts_unix_ms: i64,
    now_unix_ms: i64,
    payload: &[u8],
) -> Result<(String, String), String> {
    let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key = engine
        .decode(client_key_b64u.trim())
        .map_err(|_| "client key is not valid base64url".to_string())?;
    if key.len() != UNCOMPRESSED_P256_POINT_LEN || key[0] != 0x04 {
        return Err(format!(
            "client key must be a {UNCOMPRESSED_P256_POINT_LEN}-byte uncompressed P-256 point"
        ));
    }
    let signature = engine
        .decode(signature_b64u.trim())
        .map_err(|_| "client key signature is not valid base64url".to_string())?;
    if signature.len() != FIXED_ECDSA_P256_SIG_LEN {
        return Err(format!(
            "client key signature must be {FIXED_ECDSA_P256_SIG_LEN} bytes (fixed-form r||s)"
        ));
    }
    let skew = (now_unix_ms - ts_unix_ms).abs();
    if skew > CLIENT_KEY_MAX_SKEW_MS {
        return Err(format!(
            "client key signature timestamp is {skew}ms from daemon time (max {CLIENT_KEY_MAX_SKEW_MS}ms)"
        ));
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &key)
        .verify(payload, &signature)
        .map_err(|_| "client key signature verification failed".to_string())?;
    Ok((client_key_fingerprint(&key), b64u(&key)))
}

/// Signing helpers for tests that need a real browser-shaped identity key
/// (peer-route attribution, doorbell caller-ID). Lives outside `mod tests`
/// so sibling modules' test code can reuse it.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    pub(crate) struct TestKey {
        pub(crate) pair: EcdsaKeyPair,
        pub(crate) raw_point_b64u: String,
    }

    pub(crate) fn generate_key() -> TestKey {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let raw_point_b64u = b64u(pair.public_key().as_ref());
        TestKey {
            pair,
            raw_point_b64u,
        }
    }

    pub(crate) fn sign(key: &TestKey, daemon_id: &str, nonce: &str, sdp: &str, ts: i64) -> String {
        let rng = ring::rand::SystemRandom::new();
        let payload = client_key_offer_payload(daemon_id, nonce, sdp, ts);
        b64u(key.pair.sign(&rng, &payload).unwrap().as_ref())
    }
}

/// Optional client-key fields as they appear in offer payloads, shared by the
/// rendezvous path and the daemon-local signaling path.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientKeyOfferFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_sig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_ts: Option<i64>,
    /// Which offer payload the signature covers. Absent/empty = v1
    /// (browsers that predate the field always signed v1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_proto: Option<String>,
    /// Browser-claimed account identity — only meaningful under a v2
    /// signature, which covers these exact strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_account_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_account_name: Option<String>,
}

impl ClientKeyOfferFields {
    pub fn is_present(&self) -> bool {
        self.client_key
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
            || self
                .client_key_sig
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
    }

    /// Verify against the session parameters. Returns:
    /// - `Ok(None)` when no client key was offered,
    /// - `Ok(Some(_))` on successful verification,
    /// - `Err(_)` when a key was offered but does not verify — callers must
    ///   fail closed rather than fall back, so a relay cannot strip or corrupt
    ///   the binding to downgrade a key-authenticated session.
    ///
    /// Version dispatch is strict: account fields on a v1 offer are an
    /// error (they would be riding outside the signature), and an unknown
    /// protocol is an error rather than a fallback.
    pub fn verify(
        &self,
        daemon_id: &str,
        client_nonce: &str,
        sdp: &str,
        now_unix_ms: i64,
    ) -> Result<Option<VerifiedClientKey>, String> {
        if !self.is_present() {
            return Ok(None);
        }
        let key = self.client_key.as_deref().unwrap_or("").trim();
        let sig = self.client_key_sig.as_deref().unwrap_or("").trim();
        if key.is_empty() || sig.is_empty() {
            return Err("client key offer is missing the key or the signature".to_string());
        }
        let ts = self
            .client_key_ts
            .ok_or_else(|| "client key offer is missing its timestamp".to_string())?;
        let account_user_id = self
            .client_key_account_user_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let account_name = self
            .client_key_account_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let proto = self
            .client_key_proto
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(CLIENT_KEY_OFFER_PROTOCOL);
        match proto {
            CLIENT_KEY_OFFER_PROTOCOL => {
                if account_user_id.is_some() || account_name.is_some() {
                    return Err("client key account fields require a v2-signed offer".to_string());
                }
                verify_client_key_offer(key, sig, ts, daemon_id, client_nonce, sdp, now_unix_ms)
                    .map(Some)
            }
            CLIENT_KEY_OFFER_PROTOCOL_V2 => {
                let account_user_id = account_user_id.ok_or_else(|| {
                    "v2 client key offer is missing its account user id".to_string()
                })?;
                verify_client_key_offer_v2(
                    key,
                    sig,
                    ts,
                    daemon_id,
                    client_nonce,
                    sdp,
                    now_unix_ms,
                    account_user_id,
                    account_name.unwrap_or(""),
                )
                .map(Some)
            }
            other => Err(format!("unsupported client key offer protocol {other:?}")),
        }
    }
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_shape_matches_the_generator() {
        let fp = client_key_fingerprint(b"any raw point bytes");
        assert!(is_client_key_fingerprint(&fp));
        // The exact failure that motivated the check: a placeholder (or a
        // typo'd paste) must not be pinnable as root.
        assert!(!is_client_key_fingerprint("OWNERKEY-PLACEHOLDER"));
        assert!(!is_client_key_fingerprint(""));
        assert!(!is_client_key_fingerprint(&fp[..42]));
        assert!(!is_client_key_fingerprint(&format!("{fp}=")));
        assert!(!is_client_key_fingerprint(&format!("{}+", &fp[..42])));
    }
    pub(crate) use super::test_support::{generate_key, sign, TestKey};

    #[test]
    fn verifies_a_valid_signed_offer() {
        let key = generate_key();
        let ts = 1_700_000_000_000;
        let sig = sign(&key, "daemon-a", "nonce-1", "v=0 sdp", ts);
        let verified = verify_client_key_offer(
            &key.raw_point_b64u,
            &sig,
            ts,
            "daemon-a",
            "nonce-1",
            "v=0 sdp",
            ts + 1_000,
        )
        .unwrap();
        assert_eq!(verified.public_key_b64u, key.raw_point_b64u);
        assert!(!verified.fingerprint.is_empty());
    }

    #[test]
    fn binds_daemon_nonce_sdp_and_time() {
        let key = generate_key();
        let ts = 1_700_000_000_000;
        let sig = sign(&key, "daemon-a", "nonce-1", "v=0 sdp", ts);
        let ok = |daemon: &str, nonce: &str, sdp: &str, now: i64| {
            verify_client_key_offer(&key.raw_point_b64u, &sig, ts, daemon, nonce, sdp, now)
        };
        assert!(
            ok("daemon-b", "nonce-1", "v=0 sdp", ts).is_err(),
            "daemon id must bind"
        );
        assert!(
            ok("daemon-a", "nonce-2", "v=0 sdp", ts).is_err(),
            "nonce must bind"
        );
        assert!(
            ok("daemon-a", "nonce-1", "v=1 sdp", ts).is_err(),
            "sdp must bind"
        );
        assert!(
            ok(
                "daemon-a",
                "nonce-1",
                "v=0 sdp",
                ts + CLIENT_KEY_MAX_SKEW_MS + 1
            )
            .is_err(),
            "stale timestamps must fail"
        );
        assert!(ok("daemon-a", "nonce-1", "v=0 sdp", ts).is_ok());
    }

    #[test]
    fn rejects_wrong_key_or_garbage() {
        let key = generate_key();
        let other = generate_key();
        let ts = 1_700_000_000_000;
        let sig = sign(&key, "daemon-a", "nonce-1", "v=0 sdp", ts);
        assert!(verify_client_key_offer(
            &other.raw_point_b64u,
            &sig,
            ts,
            "daemon-a",
            "nonce-1",
            "v=0 sdp",
            ts
        )
        .is_err());
        assert!(verify_client_key_offer(
            "not-base64!!",
            &sig,
            ts,
            "daemon-a",
            "nonce-1",
            "v=0 sdp",
            ts
        )
        .is_err());
        assert!(verify_client_key_offer(
            &key.raw_point_b64u,
            "AAAA",
            ts,
            "daemon-a",
            "nonce-1",
            "v=0 sdp",
            ts
        )
        .is_err());
    }

    #[test]
    fn offer_fields_fail_closed_on_partial_or_bad_input() {
        let fields = ClientKeyOfferFields {
            client_key: Some("AAAA".to_string()),
            client_key_sig: None,
            client_key_ts: None,
            ..Default::default()
        };
        assert!(fields.verify("d", "n", "sdp", 0).is_err());

        let none = ClientKeyOfferFields::default();
        assert!(none.verify("d", "n", "sdp", 0).unwrap().is_none());

        let key = generate_key();
        let ts = 1_700_000_000_000;
        let fields = ClientKeyOfferFields {
            client_key: Some(key.raw_point_b64u.clone()),
            client_key_sig: Some(sign(&key, "d", "n", "sdp", ts)),
            client_key_ts: Some(ts),
            ..Default::default()
        };
        let verified = fields.verify("d", "n", "sdp", ts).unwrap().unwrap();
        assert_eq!(
            verified.fingerprint,
            client_key_fingerprint(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&key.raw_point_b64u)
                    .unwrap()
            )
        );
        assert_eq!(verified.attested_account, None);
    }

    fn sign_v2(
        key: &TestKey,
        daemon_id: &str,
        nonce: &str,
        sdp: &str,
        ts: i64,
        user: &str,
        name: &str,
    ) -> String {
        let rng = ring::rand::SystemRandom::new();
        let payload = client_key_offer_payload_v2(daemon_id, nonce, sdp, ts, user, name);
        b64u(key.pair.sign(&rng, &payload).unwrap().as_ref())
    }

    #[test]
    fn v2_offer_attests_the_account_and_binds_it() {
        let key = generate_key();
        let ts = 1_700_000_000_000;
        let fields = ClientKeyOfferFields {
            client_key: Some(key.raw_point_b64u.clone()),
            client_key_sig: Some(sign_v2(
                &key, "daemon-a", "nonce-1", "v=0 sdp", ts, "user-1", "alice",
            )),
            client_key_ts: Some(ts),
            client_key_proto: Some(CLIENT_KEY_OFFER_PROTOCOL_V2.to_string()),
            client_key_account_user_id: Some("user-1".to_string()),
            client_key_account_name: Some("alice".to_string()),
        };
        let verified = fields
            .verify("daemon-a", "nonce-1", "v=0 sdp", ts)
            .unwrap()
            .unwrap();
        assert_eq!(
            verified.attested_account,
            Some(("user-1".to_string(), "alice".to_string()))
        );

        // A relay swapping the claimed account fails the signature.
        let swapped = ClientKeyOfferFields {
            client_key_account_user_id: Some("mallory-user".to_string()),
            ..fields.clone()
        };
        assert!(swapped
            .verify("daemon-a", "nonce-1", "v=0 sdp", ts)
            .is_err());

        // Account fields glued onto a v1-signed offer are rejected: nothing
        // may ride outside the signature.
        let glued = ClientKeyOfferFields {
            client_key: Some(key.raw_point_b64u.clone()),
            client_key_sig: Some(sign(&key, "daemon-a", "nonce-1", "v=0 sdp", ts)),
            client_key_ts: Some(ts),
            client_key_proto: None,
            client_key_account_user_id: Some("mallory-user".to_string()),
            client_key_account_name: None,
        };
        assert!(glued.verify("daemon-a", "nonce-1", "v=0 sdp", ts).is_err());

        // Unknown protocol versions fail closed instead of falling back.
        let unknown = ClientKeyOfferFields {
            client_key_proto: Some("intendant-client-key-offer-v9".to_string()),
            ..fields
        };
        assert!(unknown
            .verify("daemon-a", "nonce-1", "v=0 sdp", ts)
            .is_err());
    }
}
