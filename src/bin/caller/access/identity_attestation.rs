//! Identity-bound TLS leaf attestation (Track RC Stage B2).
//!
//! Fleet-name dials — direct or through the Connect relay — are answered
//! by whatever certificate the target's SNI resolver serves for that name
//! (the WebPKI fleet certificate once one is installed), so the pairing
//! ceremony's raw `server_cert_fingerprint` pin cannot verify them: the
//! fleet leaf rotates at every ACME renewal. This module generalizes pin
//! semantics for those candidates: the target publishes a **signed
//! attestation** binding its *current* TLS leaf fingerprints to its
//! Ed25519 daemon-identity key (`crate::daemon_identity` — deliberately
//! rotation-stable), and a dialer that persisted that key at pairing
//! accepts a presented leaf iff the paired key attests it.
//!
//! Trust inheritance (B0 ruling, precision note P2): the paired identity
//! public key rides the pairing ceremony — the out-of-band invite, or the
//! doorbell approval result on the fingerprint-pinned status channel —
//! under exactly the trust class the `server_cert_fingerprint` pin rides
//! today. The attestation's authenticity to the dialer roots in that
//! ceremony, never in any key a fetched document carries.
//!
//! Verification laws (B0 ruling on Option A):
//! - **A1 (paired-key law)**: signatures verify against the PAIRED key
//!   only. The attestation's own `identity_public_key` field is bound
//!   data, not a verification input — a document signed by a *different*
//!   valid identity key refuses.
//! - **A2 (fail closed)**: enforced by the dialer (`peer::transport`) —
//!   a missing or invalid attestation fails the candidate outright; no
//!   WebPKI fallback, no unpinned fallback.
//! - **A3 (transcript discipline)**: the signature covers a versioned,
//!   domain-separated transcript ([`unsigned_payload`]) binding the
//!   daemon id, the identity public key, both current leaf fingerprints,
//!   and `issued_at_unix_ms`.
//! - **A4 (anti-rollback)**: the dialer persists the highest `issued_at`
//!   seen per paired identity ([`HighWaterStore`]) and refuses older
//!   attestations — the relay can replay, and a replayed old attestation
//!   must not re-pin a rotated-away leaf.
//! - **A5 (rotation)**: producers derive the fingerprints from the live
//!   certificate state on every render and re-sign whenever they change
//!   (`web_gateway::agent_card::AgentCardLive`), so the
//!   `install_fleet_certificate` hot-swap path yields a fresh attestation
//!   on the next fetch.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::daemon_identity::DaemonIdentity;
use crate::peer::transport::pinning::{parse_fingerprint, Fingerprint};

/// Current attestation format version. Verification refuses any other
/// value — a dialer must never accept a transcript format it cannot
/// reconstruct (fail closed on future versions, like the invite).
pub const ATTESTATION_VERSION: u32 = 1;

/// Domain-separation prefix for the signed transcript (the
/// `doorbell_transcript_v2` house pattern).
pub const ATTESTATION_DOMAIN: &str = "intendant-daemon-identity-attestation-v1";

/// Future-clock tolerance: an attestation stamped further than this
/// beyond the dialer's clock refuses instead of ratcheting the
/// high-water mark — a target with a runaway clock would otherwise
/// poison the persisted monotonicity floor and brick itself after the
/// clock is fixed. Refusal is per-attempt and self-heals as real time
/// passes the stamp.
pub const ATTESTATION_MAX_FUTURE_SKEW_MS: u64 = 10 * 60 * 1000;

/// A daemon-identity-signed binding of the daemon's current TLS leaf
/// fingerprints. Served as the `identity_attestation` block on the agent
/// card — an authority-free discovery surface — and content-signed, so
/// the fetch transport need not be trusted (the release-pinned-installer
/// pattern).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentityAttestation {
    /// Format version; see [`ATTESTATION_VERSION`].
    pub version: u32,
    /// Connect daemon id when this daemon is claimed; absent otherwise.
    /// Bound in the transcript for cross-context separation; v1 dialers
    /// carry no expected value to compare it against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_id: Option<String>,
    /// The Ed25519 daemon-identity public key (base64url, unpadded) that
    /// produced `signature`. Bound data only — verification uses the
    /// key the dialer persisted at pairing (A1).
    pub identity_public_key: String,
    /// SHA-256 of the access `server.crt` leaf DER (lowercase hex), when
    /// an access certificate is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_cert_fingerprint: Option<String>,
    /// SHA-256 of the current WebPKI fleet leaf DER (lowercase hex),
    /// when a fleet certificate is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_cert_fingerprint: Option<String>,
    /// Signing instant (unix ms). The dialer's monotonicity floor (A4).
    pub issued_at_unix_ms: u64,
    /// Ed25519 signature (base64url, unpadded) over [`unsigned_payload`].
    pub signature: String,
}

/// The domain-separated transcript the signature covers. Absent optional
/// fields serialize as empty lines so the byte layout is unambiguous.
pub fn unsigned_payload(
    version: u32,
    daemon_id: Option<&str>,
    identity_public_key: &str,
    access_cert_fingerprint: Option<&str>,
    fleet_cert_fingerprint: Option<&str>,
    issued_at_unix_ms: u64,
) -> Vec<u8> {
    format!(
        "{ATTESTATION_DOMAIN}\n{version}\n{}\n{identity_public_key}\n{}\n{}\n{issued_at_unix_ms}",
        daemon_id.unwrap_or_default(),
        access_cert_fingerprint.unwrap_or_default(),
        fleet_cert_fingerprint.unwrap_or_default(),
    )
    .into_bytes()
}

/// Sign an attestation over the given live certificate state.
pub fn sign_attestation(
    identity: &DaemonIdentity,
    daemon_id: Option<&str>,
    access_cert_fingerprint: Option<&str>,
    fleet_cert_fingerprint: Option<&str>,
    issued_at_unix_ms: u64,
) -> DaemonIdentityAttestation {
    let identity_public_key = identity.public_key_b64u();
    let payload = unsigned_payload(
        ATTESTATION_VERSION,
        daemon_id,
        &identity_public_key,
        access_cert_fingerprint,
        fleet_cert_fingerprint,
        issued_at_unix_ms,
    );
    DaemonIdentityAttestation {
        version: ATTESTATION_VERSION,
        daemon_id: daemon_id.map(str::to_string),
        identity_public_key,
        access_cert_fingerprint: access_cert_fingerprint.map(str::to_string),
        fleet_cert_fingerprint: fleet_cert_fingerprint.map(str::to_string),
        issued_at_unix_ms,
        signature: identity.sign_b64u(&payload),
    }
}

/// Verify an attestation against the key persisted at pairing (A1) and
/// return the attested pin set. Every failure is a refusal — the caller
/// fails the candidate (A2), never falls back.
pub fn verify_attestation(
    attestation: &DaemonIdentityAttestation,
    paired_identity_public_key: &str,
) -> Result<Vec<Fingerprint>, String> {
    if attestation.version != ATTESTATION_VERSION {
        return Err(format!(
            "unsupported identity attestation version {} (this build verifies version {ATTESTATION_VERSION} only)",
            attestation.version
        ));
    }
    // A1 belt: a mismatched carried key can never verify below (the
    // transcript binds it), but the distinct refusal names the actual
    // failure — a *different* daemon identity signed this document.
    if attestation.identity_public_key != paired_identity_public_key {
        return Err(
            "attestation identity key does not match the identity key paired for this peer"
                .to_string(),
        );
    }
    let payload = unsigned_payload(
        attestation.version,
        attestation.daemon_id.as_deref(),
        &attestation.identity_public_key,
        attestation.access_cert_fingerprint.as_deref(),
        attestation.fleet_cert_fingerprint.as_deref(),
        attestation.issued_at_unix_ms,
    );
    if !crate::daemon_identity::verify_b64u(
        paired_identity_public_key,
        &payload,
        &attestation.signature,
    ) {
        return Err("attestation signature does not verify against the paired identity key".into());
    }
    let mut pins = Vec::with_capacity(2);
    for (label, fp) in [
        ("access", attestation.access_cert_fingerprint.as_deref()),
        ("fleet", attestation.fleet_cert_fingerprint.as_deref()),
    ] {
        if let Some(fp) = fp {
            pins.push(
                parse_fingerprint(fp)
                    .map_err(|e| format!("attestation {label} fingerprint is invalid: {e}"))?,
            );
        }
    }
    if pins.is_empty() {
        return Err("attestation binds no certificate fingerprints".into());
    }
    Ok(pins)
}

/// Persisted per-paired-identity monotonicity floor (A4), stored beside
/// the peer credentials. One file per paired identity key — a daemon
/// paired through several card URLs is still one identity, and shares
/// one floor.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HighWaterRecord {
    version: u32,
    identity_public_key: String,
    highest_issued_at_unix_ms: u64,
}

/// File-backed high-water store rooted at a directory the caller owns
/// (production: `<access-certs>/peers/attestation-state`; tests inject
/// temp dirs).
#[derive(Debug, Clone)]
pub struct HighWaterStore {
    dir: PathBuf,
}

impl HighWaterStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn record_path(&self, paired_identity_public_key: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(paired_identity_public_key.as_bytes());
        let mut name = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            name.push_str(&format!("{byte:02x}"));
        }
        self.dir.join(format!("{name}.json"))
    }

    /// The persisted floor for one paired identity, if any. Unreadable
    /// or malformed state reads as absent — first-contact semantics —
    /// so a corrupt file degrades replay protection rather than
    /// bricking the peer.
    pub fn highest_issued_at(&self, paired_identity_public_key: &str) -> Option<u64> {
        let bytes = std::fs::read(self.record_path(paired_identity_public_key)).ok()?;
        let record: HighWaterRecord = serde_json::from_slice(&bytes).ok()?;
        (record.identity_public_key == paired_identity_public_key)
            .then_some(record.highest_issued_at_unix_ms)
    }

    /// Enforce A4 for a signature-verified attestation and advance the
    /// floor. Call only AFTER [`verify_attestation`] succeeds — an
    /// unverified document must never ratchet the store.
    pub fn enforce_monotonic(
        &self,
        paired_identity_public_key: &str,
        attestation: &DaemonIdentityAttestation,
        now_unix_ms: u64,
    ) -> Result<(), String> {
        if attestation.issued_at_unix_ms
            > now_unix_ms.saturating_add(ATTESTATION_MAX_FUTURE_SKEW_MS)
        {
            return Err(format!(
                "attestation issued_at {}ms is beyond tolerated future skew (now {}ms + {}ms)",
                attestation.issued_at_unix_ms, now_unix_ms, ATTESTATION_MAX_FUTURE_SKEW_MS
            ));
        }
        let floor = self.highest_issued_at(paired_identity_public_key);
        if let Some(floor) = floor {
            if attestation.issued_at_unix_ms < floor {
                return Err(format!(
                    "attestation issued_at {} is older than the highest previously verified attestation ({}) — refusing a possible replay",
                    attestation.issued_at_unix_ms, floor
                ));
            }
        }
        if floor != Some(attestation.issued_at_unix_ms) {
            self.persist(paired_identity_public_key, attestation.issued_at_unix_ms)?;
        }
        Ok(())
    }

    fn persist(
        &self,
        paired_identity_public_key: &str,
        issued_at_unix_ms: u64,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("create attestation state dir {}: {e}", self.dir.display()))?;
        let record = HighWaterRecord {
            version: 1,
            identity_public_key: paired_identity_public_key.to_string(),
            highest_issued_at_unix_ms: issued_at_unix_ms,
        };
        let path = self.record_path(paired_identity_public_key);
        let tmp = path.with_extension("json.tmp");
        let bytes =
            serde_json::to_vec(&record).map_err(|e| format!("serialize attestation state: {e}"))?;
        std::fs::write(&tmp, &bytes)
            .map_err(|e| format!("write attestation state {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("install attestation state {}: {e}", path.display()))?;
        Ok(())
    }
}

/// Production location of the dialer-side high-water store: beside the
/// peer credential directories under the access cert store.
pub fn default_high_water_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join("peers").join("attestation-state")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity(dir: &Path, name: &str) -> DaemonIdentity {
        DaemonIdentity::load_or_create(dir.join(name)).unwrap()
    }

    const ACCESS_FP: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const FLEET_FP: &str = "1122334455667788991122334455667788991122334455667788991122334455";

    #[test]
    fn attestation_round_trips_and_verifies_against_paired_key() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let att = sign_attestation(
            &identity,
            Some("daemon-1"),
            Some(ACCESS_FP),
            Some(FLEET_FP),
            1_000,
        );
        let json = serde_json::to_string(&att).unwrap();
        let parsed: DaemonIdentityAttestation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, att);

        let pins = verify_attestation(&parsed, &identity.public_key_b64u()).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0], parse_fingerprint(ACCESS_FP).unwrap());
        assert_eq!(pins[1], parse_fingerprint(FLEET_FP).unwrap());
    }

    /// A1: an attestation signed by a DIFFERENT valid identity key
    /// refuses — the paired key is the only verification input.
    #[test]
    fn attestation_signed_by_different_valid_key_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let paired = test_identity(dir.path(), "paired.pk8");
        let other = test_identity(dir.path(), "other.pk8");
        let att = sign_attestation(&other, None, Some(ACCESS_FP), None, 1_000);
        let err = verify_attestation(&att, &paired.public_key_b64u()).unwrap_err();
        assert!(
            err.contains("does not match the identity key paired"),
            "got: {err}"
        );

        // The self-consistency trap (the ledger's flaw): even a document
        // whose carried key verifies its own signature must refuse when
        // that key is not the paired one. Forge the carried key field to
        // the paired key while keeping the other signer's signature —
        // now the equality belt passes and the signature check itself
        // must refuse.
        let mut forged = att.clone();
        forged.identity_public_key = paired.public_key_b64u();
        let err = verify_attestation(&forged, &paired.public_key_b64u()).unwrap_err();
        assert!(
            err.contains("signature does not verify"),
            "forged carried key must fail the signature, got: {err}"
        );
    }

    /// A3: any tampered bound field breaks the signature.
    #[test]
    fn tampered_fields_break_the_signature() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let att = sign_attestation(
            &identity,
            Some("daemon-1"),
            Some(ACCESS_FP),
            Some(FLEET_FP),
            5_000,
        );
        let key = identity.public_key_b64u();

        let mut tampered = att.clone();
        tampered.fleet_cert_fingerprint =
            Some("9999999999999999999999999999999999999999999999999999999999999999".into());
        assert!(verify_attestation(&tampered, &key).is_err());

        let mut tampered = att.clone();
        tampered.issued_at_unix_ms = 6_000;
        assert!(verify_attestation(&tampered, &key).is_err());

        let mut tampered = att.clone();
        tampered.daemon_id = None;
        assert!(verify_attestation(&tampered, &key).is_err());

        assert!(verify_attestation(&att, &key).is_ok());
    }

    /// Fail closed on formats this build cannot reconstruct.
    #[test]
    fn unknown_version_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let mut att = sign_attestation(&identity, None, Some(ACCESS_FP), None, 1_000);
        att.version = 2;
        let err = verify_attestation(&att, &identity.public_key_b64u()).unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }

    /// An attestation that binds no fingerprints pins nothing and is
    /// refused rather than producing an accept-nothing verifier state
    /// that a caller could misread as verified.
    #[test]
    fn attestation_without_fingerprints_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let att = sign_attestation(&identity, None, None, None, 1_000);
        let err = verify_attestation(&att, &identity.public_key_b64u()).unwrap_err();
        assert!(err.contains("binds no certificate"), "got: {err}");
    }

    /// A4: the persisted floor refuses older attestations across store
    /// instances (restart survival), accepts equal replays of the
    /// current one, and advances on newer.
    #[test]
    fn high_water_store_enforces_monotonic_issued_at() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let key = identity.public_key_b64u();
        let store = HighWaterStore::new(dir.path().join("state"));

        let old = sign_attestation(&identity, None, Some(ACCESS_FP), None, 1_000);
        let new = sign_attestation(&identity, None, Some(ACCESS_FP), Some(FLEET_FP), 2_000);

        // First contact: any verified attestation is accepted.
        store.enforce_monotonic(&key, &old, 10_000).unwrap();
        // Newer advances the floor.
        store.enforce_monotonic(&key, &new, 10_000).unwrap();
        // Equal (a replay of the same document) stays accepted.
        store.enforce_monotonic(&key, &new, 10_000).unwrap();
        // Older refuses — the relay-replay case (rotated-away leaf).
        let err = store.enforce_monotonic(&key, &old, 10_000).unwrap_err();
        assert!(err.contains("older than the highest"), "got: {err}");

        // A fresh store instance over the same dir keeps the floor
        // (restart survival — the property A4 exists for).
        let reopened = HighWaterStore::new(dir.path().join("state"));
        let err = reopened.enforce_monotonic(&key, &old, 10_000).unwrap_err();
        assert!(err.contains("older than the highest"), "got: {err}");
    }

    /// A runaway future clock refuses instead of ratcheting the floor.
    #[test]
    fn far_future_issued_at_refuses_without_ratcheting() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity(dir.path(), "id.pk8");
        let key = identity.public_key_b64u();
        let store = HighWaterStore::new(dir.path().join("state"));

        let now: u64 = 1_000_000;
        let future = sign_attestation(
            &identity,
            None,
            Some(ACCESS_FP),
            None,
            now + ATTESTATION_MAX_FUTURE_SKEW_MS + 1,
        );
        let err = store.enforce_monotonic(&key, &future, now).unwrap_err();
        assert!(err.contains("future skew"), "got: {err}");
        assert_eq!(
            store.highest_issued_at(&key),
            None,
            "a refused future stamp must not ratchet the floor"
        );

        // Within tolerated skew is accepted.
        let near = sign_attestation(&identity, None, Some(ACCESS_FP), None, now + 1_000);
        store.enforce_monotonic(&key, &near, now).unwrap();
    }

    /// Different paired identities keep independent floors; the record
    /// self-checks its key so a hash-prefix collision cannot leak a
    /// floor across identities.
    #[test]
    fn high_water_floors_are_per_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = test_identity(dir.path(), "a.pk8");
        let b = test_identity(dir.path(), "b.pk8");
        let store = HighWaterStore::new(dir.path().join("state"));

        let att_a = sign_attestation(&a, None, Some(ACCESS_FP), None, 9_000);
        store
            .enforce_monotonic(&a.public_key_b64u(), &att_a, 10_000)
            .unwrap();
        // B has no floor yet; an older-stamped attestation from B passes.
        let att_b = sign_attestation(&b, None, Some(ACCESS_FP), None, 1_000);
        store
            .enforce_monotonic(&b.public_key_b64u(), &att_b, 10_000)
            .unwrap();
    }
}
