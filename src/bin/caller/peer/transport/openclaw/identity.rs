//! Ed25519 device identity for the OpenClaw Gateway transport:
//! generate/persist/load the keypair, derive the device fingerprint,
//! sign the challenge-bound `connect` device proof, and persist the
//! gateway-minted device token. Pure-Rust crypto via `ring` (house
//! law) — no OpenSSL, no new dependencies.
//!
//! ## Wire derivations (must match the reference client exactly)
//!
//! - **Device id** = lowercase-hex SHA-256 of the **raw 32-byte**
//!   Ed25519 public key (upstream `src/infra/device-identity.ts`
//!   `deriveDeviceIdFromPublicKey` and
//!   `src/infra/device-identity-store.ts` `fingerprintPublicKey`).
//!   The gateway re-derives the id from `device.publicKey` and rejects
//!   the connect on mismatch
//!   (`src/gateway/server/ws-connection/connect-device-proof.ts`).
//! - **Public key wire form** = unpadded base64url of the raw 32 bytes
//!   (upstream `src/infra/ed25519-signature.ts`
//!   `publicKeyRawBase64UrlFromEd25519Pem`; PEM is also accepted
//!   server-side, but the reference client sends base64url).
//! - **Signature wire form** = unpadded base64url of the 64-byte
//!   Ed25519 signature over the **UTF-8 bytes** of the pipe-joined
//!   payload string (upstream `signEd25519Payload`).
//!
//! ## Signed byte layout (device auth payload v3)
//!
//! From `packages/gateway-client/src/device-auth.ts`
//! `buildDeviceAuthPayloadV3` — eleven fields joined with `|`:
//!
//! ```text
//! v3|<deviceId>|<clientId>|<clientMode>|<role>|<scopes.join(",")>|
//! <signedAtMs as decimal>|<token or "">|<nonce>|<platform'>|<deviceFamily'>
//! ```
//!
//! (one line on the wire; wrapped here for readability). `platform'` /
//! `deviceFamily'` are normalized first (`normalizeDeviceMetadataForAuth`:
//! trim, then ASCII `A-Z` → `a-z`; empty/absent → `""`). `signedAtMs`
//! is the challenge's `ts` echoed verbatim; `nonce` is the challenge's
//! nonce; `token` is the credential the connect frame carries, resolved
//! with the gateway's own precedence
//! (`wire::ConnectAuth::signature_token`: `token` → `deviceToken` →
//! `bootstrapToken`; password never signs). The gateway rebuilds this
//! exact string from the connect params and verifies the signature
//! (`src/gateway/server/ws-connection/handshake-auth-helpers.ts`
//! `resolveDeviceSignaturePayloadVersion` — v3 first, legacy v2
//! fallback), enforcing device-id match, a ±2-minute `signedAt` skew
//! window, and nonce equality with the challenge it issued. We
//! implement v3 only — v2 (same layout minus the two metadata fields)
//! is the server-side compat lane for older clients, which we never
//! were.
//!
//! ## At-rest formats (Intendant-local, NOT wire)
//!
//! Upstream stores PEM keypairs in its SQLite state database; only the
//! wire bytes above are contract. We persist the identity as a small
//! JSON file holding the PKCS#8 v2 document `ring` generates, and the
//! device token as a JSON file alongside — both written 0600 on Unix
//! via `state_paths::write_private_file`, under caller-provided paths
//! (the transport seat owns the state-root layout; tests inject
//! tempdirs per the hermetic-tests law).

// Slice-1 seam: consumed by the OpenClaw transport actor seat landing
// concurrently with this module. Remove once the transport is wired.
#![allow(dead_code)]

use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::KeyPair as _;
use serde::{Deserialize, Serialize};

use super::wire::{ConnectAuth, ConnectChallenge, ConnectClient, ConnectDevice};
use crate::peer::PeerError;

/// Signed-payload version prefix for the v3 layout (the one the
/// reference client sends; the gateway also accepts legacy `v2`, which
/// drops the two trailing metadata fields — we never emit it).
const PAYLOAD_V3_PREFIX: &str = "v3";

/// On-disk identity format version this build reads and writes.
const IDENTITY_FILE_VERSION: u32 = 1;

/// On-disk token format version this build reads and writes.
const TOKEN_FILE_VERSION: u32 = 1;

/// A loaded Ed25519 device identity. Not `Clone` (the `ring` keypair
/// is not); the transport seat shares it behind an `Arc`.
pub(crate) struct DeviceIdentity {
    keypair: ring::signature::Ed25519KeyPair,
    device_id: String,
    public_key_b64url: String,
}

impl std::fmt::Debug for DeviceIdentity {
    /// Manual impl so key material can never leak through debug logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// Serialized identity file: `{version, created_at_ms, pkcs8_b64}`.
/// `pkcs8_b64` is standard (padded) base64 of the PKCS#8 v2 document.
#[derive(Serialize, Deserialize)]
struct PersistedDeviceIdentity {
    version: u32,
    created_at_ms: u64,
    pkcs8_b64: String,
}

impl DeviceIdentity {
    /// Load the identity at `path`, or generate + persist a fresh one
    /// (0600 on Unix) when the file does not exist. A present-but-
    /// invalid file is an error, never silently rotated: rotating the
    /// key changes the device id, which orphans the gateway-side
    /// pairing approval — that decision belongs to the operator.
    pub(crate) fn load_or_generate(path: &Path) -> Result<Self, PeerError> {
        if let Some(identity) = Self::load(path)? {
            return Ok(identity);
        }
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|e| PeerError::Auth(format!("generate device keypair: {e}")))?;
        let persisted = PersistedDeviceIdentity {
            version: IDENTITY_FILE_VERSION,
            created_at_ms: now_ms(),
            pkcs8_b64: base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref()),
        };
        write_private_json(path, &persisted)?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// Load the identity at `path`; `Ok(None)` when the file is absent.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>, PeerError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(PeerError::Io(e)),
        };
        let persisted: PersistedDeviceIdentity =
            serde_json::from_slice(&bytes).map_err(PeerError::Json)?;
        if persisted.version != IDENTITY_FILE_VERSION {
            return Err(PeerError::Auth(format!(
                "device identity file {} has unsupported version {} (this build reads {})",
                path.display(),
                persisted.version,
                IDENTITY_FILE_VERSION
            )));
        }
        let pkcs8 = base64::engine::general_purpose::STANDARD
            .decode(&persisted.pkcs8_b64)
            .map_err(|e| {
                PeerError::Auth(format!(
                    "device identity file {} is corrupt: {e}",
                    path.display()
                ))
            })?;
        Self::from_pkcs8(&pkcs8).map(Some)
    }

    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, PeerError> {
        let keypair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|e| PeerError::Auth(format!("load device keypair: {e}")))?;
        Ok(Self::from_keypair(keypair))
    }

    fn from_keypair(keypair: ring::signature::Ed25519KeyPair) -> Self {
        let public_raw = keypair.public_key().as_ref().to_vec();
        Self {
            device_id: fingerprint_public_key(&public_raw),
            public_key_b64url: URL_SAFE_NO_PAD.encode(&public_raw),
            keypair,
        }
    }

    /// Test-only deterministic construction from a raw 32-byte seed.
    #[cfg(test)]
    fn from_seed(seed: &[u8; 32]) -> Self {
        let keypair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .expect("32-byte Ed25519 seed");
        Self::from_keypair(keypair)
    }

    /// The device fingerprint: lowercase-hex SHA-256 of the raw public
    /// key. This is `connect.params.device.id` and the handle shown by
    /// `openclaw devices list` on the gateway host.
    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The wire public key: unpadded base64url of the raw 32 bytes.
    pub(crate) fn public_key_base64url(&self) -> &str {
        &self.public_key_b64url
    }

    /// Sign an already-built device-auth payload string: Ed25519 over
    /// the payload's UTF-8 bytes, unpadded-base64url output.
    pub(crate) fn sign_payload(&self, payload: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.keypair.sign(payload.as_bytes()).as_ref())
    }

    /// Build the `connect.params.device` proof for one challenge.
    ///
    /// Reads `client_id`/`client_mode`/`platform`/`device_family` from
    /// the exact [`ConnectClient`] that will ride the same connect
    /// frame, and the signature credential from the frame's
    /// [`ConnectAuth`], so the signed facts cannot drift from the sent
    /// ones (the gateway rebuilds the payload from the frame and
    /// byte-compares). `role`/`scopes` must likewise be the frame's
    /// values, in the same order. Fails when the challenge nonce trims
    /// to empty — the reference client treats such a challenge as
    /// unusable rather than signing a blank nonce.
    pub(crate) fn connect_proof(
        &self,
        client: &ConnectClient,
        role: &str,
        scopes: &[String],
        auth: Option<&ConnectAuth>,
        challenge: &ConnectChallenge,
    ) -> Result<ConnectDevice, PeerError> {
        let nonce = challenge
            .nonce_trimmed()
            .ok_or_else(|| PeerError::Auth("gateway connect challenge missing nonce".into()))?;
        let payload = build_device_auth_payload_v3(&DeviceAuthFactsV3 {
            device_id: &self.device_id,
            client_id: &client.id,
            client_mode: &client.mode,
            role,
            scopes,
            signed_at_ms: challenge.ts,
            token: auth.and_then(ConnectAuth::signature_token),
            nonce,
            platform: Some(&client.platform),
            device_family: client.device_family.as_deref(),
        });
        Ok(ConnectDevice {
            id: self.device_id.clone(),
            public_key: self.public_key_b64url.clone(),
            signature: self.sign_payload(&payload),
            signed_at: challenge.ts,
            nonce: nonce.to_string(),
        })
    }
}

/// The facts bound into a v3 device-auth payload. Field-for-field
/// mirror of upstream `DeviceAuthPayloadV3Params`.
pub(crate) struct DeviceAuthFactsV3<'a> {
    pub device_id: &'a str,
    pub client_id: &'a str,
    pub client_mode: &'a str,
    pub role: &'a str,
    pub scopes: &'a [String],
    pub signed_at_ms: u64,
    /// `ConnectAuth::signature_token` of the same frame; `None` → `""`.
    pub token: Option<&'a str>,
    pub nonce: &'a str,
    pub platform: Option<&'a str>,
    pub device_family: Option<&'a str>,
}

/// Build the exact v3 payload string the gateway byte-compares (layout
/// documented in the module header; source:
/// `packages/gateway-client/src/device-auth.ts` `buildDeviceAuthPayloadV3`).
pub(crate) fn build_device_auth_payload_v3(facts: &DeviceAuthFactsV3<'_>) -> String {
    [
        PAYLOAD_V3_PREFIX,
        facts.device_id,
        facts.client_id,
        facts.client_mode,
        facts.role,
        &facts.scopes.join(","),
        &facts.signed_at_ms.to_string(),
        facts.token.unwrap_or(""),
        facts.nonce,
        &normalize_device_metadata(facts.platform),
        &normalize_device_metadata(facts.device_family),
    ]
    .join("|")
}

/// Upstream `normalizeDeviceMetadataForAuth`: absent → `""`, else trim;
/// empty after trim → `""`; else ASCII `A-Z` lowered (only `A-Z` — the
/// upstream regex is `/[A-Z]/g`, so non-ASCII stays untouched). Both
/// ends normalize identically as long as the values are ASCII — keep
/// `platform`/`deviceFamily` ASCII.
fn normalize_device_metadata(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Lowercase-hex SHA-256 of the raw 32-byte public key — the device id.
fn fingerprint_public_key(public_key_raw: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, public_key_raw);
    hex_lower(digest.as_ref())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PeerError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            intendant_core::state_paths::create_private_dir_all(parent).map_err(PeerError::Io)?;
        }
    }
    let mut body = serde_json::to_vec_pretty(value).map_err(PeerError::Json)?;
    body.push(b'\n');
    intendant_core::state_paths::write_private_file(path, &body).map_err(PeerError::Io)
}

// ---------------------------------------------------------------------------
// Device token store
// ---------------------------------------------------------------------------

/// The persisted device token plus the contract it was granted under:
/// `hello-ok.auth.deviceToken` with the granted (not requested) role
/// and scopes, and the gateway URL it belongs to. Reconnects present
/// the token via `auth.deviceToken`; a scope/role widening beyond the
/// stored grant mints a new pairing request on the gateway, so the
/// stored scopes are what later connects should re-request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredDeviceToken {
    pub token: String,
    pub role: String,
    pub scopes: Vec<String>,
    /// The gateway this token was minted by (its WebSocket URL as
    /// configured). A token must never be replayed to another gateway.
    pub gateway_url: String,
    pub saved_at_ms: u64,
}

/// On-disk wrapper so the token file is versioned independently of the
/// in-memory shape.
#[derive(Serialize, Deserialize)]
struct PersistedDeviceToken {
    version: u32,
    #[serde(flatten)]
    token: StoredDeviceToken,
}

/// Load the stored device token; `Ok(None)` when absent. A corrupt or
/// future-versioned file is an error (fail closed — the transport then
/// falls back to bootstrap auth, which is visible, rather than
/// silently replaying a mangled token).
pub(crate) fn load_device_token(path: &Path) -> Result<Option<StoredDeviceToken>, PeerError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PeerError::Io(e)),
    };
    let persisted: PersistedDeviceToken =
        serde_json::from_slice(&bytes).map_err(PeerError::Json)?;
    if persisted.version != TOKEN_FILE_VERSION {
        return Err(PeerError::Auth(format!(
            "device token file {} has unsupported version {} (this build reads {})",
            path.display(),
            persisted.version,
            TOKEN_FILE_VERSION
        )));
    }
    Ok(Some(persisted.token))
}

/// Persist the device token (0600 on Unix), replacing any previous one.
pub(crate) fn save_device_token(path: &Path, token: &StoredDeviceToken) -> Result<(), PeerError> {
    write_private_json(
        path,
        &PersistedDeviceToken {
            version: TOKEN_FILE_VERSION,
            token: token.clone(),
        },
    )
}

/// Remove the stored device token. A missing file is already the
/// requested state. Use on `AUTH_DEVICE_TOKEN_MISMATCH`-class rejects
/// (the gateway rotated/revoked the token) before re-pairing.
pub(crate) fn clear_device_token(path: &Path) -> Result<(), PeerError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PeerError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 §7.1 TEST 1 seed — the standard Ed25519 test vector,
    /// giving the deterministic pins below independent provenance.
    const RFC8032_TEST1_SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    /// RFC 8032 §7.1 TEST 1 public key (raw 32 bytes, hex).
    const RFC8032_TEST1_PUBLIC_HEX: &str =
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn verify_wire_signature(identity: &DeviceIdentity, payload: &str, signature_b64url: &str) {
        let public = URL_SAFE_NO_PAD
            .decode(identity.public_key_base64url())
            .expect("wire public key decodes");
        let signature = URL_SAFE_NO_PAD
            .decode(signature_b64url)
            .expect("wire signature decodes");
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public)
            .verify(payload.as_bytes(), &signature)
            .expect("signature verifies against the payload bytes");
    }

    #[test]
    fn payload_v3_layout_is_pinned() {
        let scopes = vec!["operator.read".to_string(), "operator.write".to_string()];
        let payload = build_device_auth_payload_v3(&DeviceAuthFactsV3 {
            device_id: "d3adb33f",
            client_id: "gateway-client",
            client_mode: "backend",
            role: "operator",
            scopes: &scopes,
            signed_at_ms: 1_755_600_000_123,
            token: Some("tok-1"),
            nonce: "nonce-1",
            platform: Some(" Darwin "),
            device_family: None,
        });
        assert_eq!(
            payload,
            "v3|d3adb33f|gateway-client|backend|operator|operator.read,operator.write|\
             1755600000123|tok-1|nonce-1|darwin|"
        );

        // No token → empty field, not a missing one; empty scopes stay
        // an empty field; metadata normalization only lowers ASCII A-Z.
        let bare = build_device_auth_payload_v3(&DeviceAuthFactsV3 {
            device_id: "id",
            client_id: "gateway-client",
            client_mode: "backend",
            role: "operator",
            scopes: &[],
            signed_at_ms: 7,
            token: None,
            nonce: "n",
            platform: Some("Win32-Ärm"),
            device_family: Some("  "),
        });
        assert_eq!(
            bare,
            "v3|id|gateway-client|backend|operator||7||n|win32-Ärm|"
        );
    }

    #[test]
    fn fingerprint_and_wire_encodings_are_pinned() {
        let identity = DeviceIdentity::from_seed(&RFC8032_TEST1_SEED);
        // Raw public key matches the RFC vector…
        assert_eq!(
            hex_lower(
                &URL_SAFE_NO_PAD
                    .decode(identity.public_key_base64url())
                    .unwrap()
            ),
            RFC8032_TEST1_PUBLIC_HEX
        );
        // …its unpadded-base64url wire form is stable…
        assert_eq!(
            identity.public_key_base64url(),
            "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        );
        // …and the device id is sha256-hex of those raw bytes
        // (upstream fingerprintPublicKey), 64 lowercase hex chars.
        assert_eq!(
            identity.device_id(),
            "21fe31dfa154a261626bf854046fd2271b7bed4b6abe45aa58877ef47f9721b9"
        );
    }

    #[test]
    fn signature_is_deterministic_and_pinned_for_fixed_seed() {
        let identity = DeviceIdentity::from_seed(&RFC8032_TEST1_SEED);
        let payload = "v3|id|gateway-client|backend|operator|operator.read|5|tok|n|macos|";
        let signature = identity.sign_payload(payload);
        // Ed25519 is deterministic: same key + payload → same bytes.
        assert_eq!(signature, identity.sign_payload(payload));
        assert_eq!(
            signature,
            "q_RX2sSXtUohcWOZIy_qkxDYTIFjk2EzxAyUmR-k9C8N7flcLsWZ6hoAg8Iu9BuoMJNzijbPpdeb-f5CUzfTAA"
        );
        verify_wire_signature(&identity, payload, &signature);
    }

    #[test]
    fn keygen_persist_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("device-identity.json");

        let generated = DeviceIdentity::load_or_generate(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(generated.device_id().len(), 64);
        assert!(generated
            .device_id()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "identity file must be owner-only");
        }

        // A second call loads the same identity instead of rotating it.
        let reloaded = DeviceIdentity::load_or_generate(&path).unwrap();
        assert_eq!(reloaded.device_id(), generated.device_id());
        assert_eq!(
            reloaded.public_key_base64url(),
            generated.public_key_base64url()
        );
        // And the reloaded key signs identically (same private key).
        let payload = "v3|probe";
        assert_eq!(
            reloaded.sign_payload(payload),
            generated.sign_payload(payload)
        );

        // Absent file reads as None, not as an error.
        assert!(DeviceIdentity::load(&dir.path().join("missing.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn corrupt_or_future_identity_files_fail_closed() {
        let dir = tempfile::tempdir().unwrap();

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{not json").unwrap();
        assert!(DeviceIdentity::load(&corrupt).is_err());

        let future = dir.path().join("future.json");
        std::fs::write(
            &future,
            br#"{"version":2,"created_at_ms":1,"pkcs8_b64":"AA"}"#,
        )
        .unwrap();
        assert!(matches!(
            DeviceIdentity::load(&future),
            Err(PeerError::Auth(_))
        ));
    }

    #[test]
    fn connect_proof_binds_challenge_and_verifies() {
        let identity = DeviceIdentity::from_seed(&RFC8032_TEST1_SEED);
        let client = ConnectClient {
            id: super::super::wire::CLIENT_ID.into(),
            version: "0.2.0".into(),
            platform: "macOS".into(),
            mode: super::super::wire::CLIENT_MODE.into(),
            display_name: Some("Intendant".into()),
            device_family: None,
            instance_id: None,
        };
        let auth = ConnectAuth {
            token: Some("boot-tok".into()),
            ..ConnectAuth::default()
        };
        let scopes = vec!["operator.read".to_string(), "operator.write".to_string()];
        let challenge = ConnectChallenge {
            nonce: " nonce-uuid ".into(),
            ts: 1_755_600_000_000,
        };

        let proof = identity
            .connect_proof(&client, "operator", &scopes, Some(&auth), &challenge)
            .unwrap();
        assert_eq!(proof.id, identity.device_id());
        assert_eq!(proof.public_key, identity.public_key_base64url());
        assert_eq!(proof.signed_at, challenge.ts);
        assert_eq!(proof.nonce, "nonce-uuid");

        // The signature verifies against the payload rebuilt the way
        // the gateway rebuilds it from the connect frame (note the
        // platform lowercased by normalization, nonce trimmed).
        let expected_payload = build_device_auth_payload_v3(&DeviceAuthFactsV3 {
            device_id: identity.device_id(),
            client_id: &client.id,
            client_mode: &client.mode,
            role: "operator",
            scopes: &scopes,
            signed_at_ms: challenge.ts,
            token: Some("boot-tok"),
            nonce: "nonce-uuid",
            platform: Some("macOS"),
            device_family: None,
        });
        assert!(expected_payload.contains("|macos|"));
        verify_wire_signature(&identity, &expected_payload, &proof.signature);

        // An all-whitespace nonce is unusable, not signable.
        let blank = ConnectChallenge {
            nonce: "   ".into(),
            ts: 1,
        };
        assert!(identity
            .connect_proof(&client, "operator", &scopes, Some(&auth), &blank)
            .is_err());
    }

    #[test]
    fn token_store_round_trip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens").join("device-token.json");

        assert!(load_device_token(&path).unwrap().is_none());

        let token = StoredDeviceToken {
            token: "dt-secret".into(),
            role: "operator".into(),
            scopes: vec!["operator.read".into(), "operator.write".into()],
            gateway_url: "wss://gw.example:18789".into(),
            saved_at_ms: 1_755_600_000_000,
        };
        save_device_token(&path, &token).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be owner-only");
        }

        assert_eq!(load_device_token(&path).unwrap(), Some(token.clone()));

        // Overwrite replaces (rotation), not appends.
        let rotated = StoredDeviceToken {
            token: "dt-rotated".into(),
            ..token
        };
        save_device_token(&path, &rotated).unwrap();
        assert_eq!(load_device_token(&path).unwrap(), Some(rotated));

        clear_device_token(&path).unwrap();
        assert!(load_device_token(&path).unwrap().is_none());
        // Clearing an already-missing file is fine.
        clear_device_token(&path).unwrap();
    }

    #[test]
    fn future_token_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-token.json");
        std::fs::write(
            &path,
            br#"{"version":9,"token":"t","role":"operator","scopes":[],
                 "gateway_url":"wss://x","saved_at_ms":1}"#,
        )
        .unwrap();
        assert!(matches!(load_device_token(&path), Err(PeerError::Auth(_))));
    }
}
