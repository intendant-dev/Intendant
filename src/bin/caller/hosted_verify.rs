//! Out-of-band verifier for Connect code/installer transparency
//! (docs/src/self-hosted-rendezvous.md, "Code transparency for the served
//! Connect bundle"; the evidence leg of first-contact rung three in
//! docs/src/trust-tiers.md). The rendezvous commits what it serves to its
//! append-only transparency log (`artifact_manifest` entries); this module
//! fetches the LIVE artifacts over HTTPS exactly as a browser would,
//! hashes them, and compares against the logged manifest — then verifies
//! the manifest's inclusion proof against the signed tree head and the
//! tree head's consistency against a locally pinned one under the daemon
//! state root (`~/.intendant/hosted-verify/`, honoring `$INTENDANT_HOME`),
//! and rejects artifact-manifest rollback below the highest verified index.
//!
//! Deliberately OUT of band: page JS served by the origin can never
//! honestly self-verify, so the checking happens from a vantage point the
//! origin does not control. Two front doors share the verifier core:
//!
//! - `intendant hosted-verify [--connect <url>]` — the CLI monitor
//!   (nonzero exit + a per-artifact diff on mismatch);
//! - the daemon tripwire (`spawn_hosted_bundle_monitor`) — advisory and
//!   fail-open like the CT tripwire in `fleet_cert.rs`: network failures
//!   stamp `last_error` and never block anything; a divergence between
//!   served bytes and the log flips `hosted_bundle_state` to `alert` on
//!   the Connect status payload.
//!
//! Honest limits (also in the docs): the origin can still serve targeted
//! different bytes to one victim once — what the log plus independent
//! monitors buy is that *sustained* or *later-denied* equivocation leaves
//! public evidence, not that betrayal is impossible.
//!
//! The Merkle/STH primitives REPLICATE `bin/connect/transparency.rs`
//! (the two binaries never link each other — the `daemon_fleet_label`
//! precedent); golden tests below twin the service's constants.

use sha2::{Digest as _, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use url::Url;

const METADATA_RESPONSE_BYTE_CAP: usize = 2 * 1024 * 1024;
const LOG_LEAF_BYTE_CAP: usize = 1024 * 1024;
const LOG_PROOF_HASH_CAP: usize = 128;
const MANIFEST_ARTIFACT_CAP: usize = 1024;
const RELEASE_PLATFORM_CAP: usize = 64;
const GITHUB_ASSET_CAP: usize = 4096;
const ARTIFACT_PATH_BYTE_CAP: usize = 2048;
const ARTIFACT_NAME_BYTE_CAP: usize = 512;
const METADATA_STRING_BYTE_CAP: usize = 512;

// ── Status registry (the tripwire's verdict; fleet_cert.rs pattern) ──

#[derive(Clone, Debug)]
pub(crate) struct HostedBundleStatus {
    /// `unchecked` | `ok` | `alert`. Reflects the last *successful*
    /// verification run; fetch failures land in `last_error` instead.
    pub state: String,
    pub checked_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    /// The per-artifact diff behind an `alert` (or a proof-level summary).
    pub mismatches: Vec<String>,
    /// Normalized Connect URL this verdict belongs to. Never display a
    /// completed verdict after live configuration points at another service.
    pub rendezvous_url: Option<String>,
}

fn registry() -> &'static Mutex<HostedBundleStatus> {
    static STATUS: OnceLock<Mutex<HostedBundleStatus>> = OnceLock::new();
    STATUS.get_or_init(|| {
        Mutex::new(HostedBundleStatus {
            state: "unchecked".to_string(),
            checked_unix_ms: None,
            last_error: None,
            mismatches: Vec::new(),
            rendezvous_url: None,
        })
    })
}

pub(crate) fn status_snapshot() -> HostedBundleStatus {
    registry()
        .lock()
        .expect("hosted bundle status poisoned")
        .clone()
}

fn with_status<T>(update: impl FnOnce(&mut HostedBundleStatus) -> T) -> T {
    let mut status = registry().lock().expect("hosted bundle status poisoned");
    update(&mut status)
}

fn verifier_url_key(value: Option<&str>) -> Option<String> {
    let mut url = Url::parse(value?.trim()).ok()?;
    url.set_fragment(None);
    let normalized = url.as_str().trim_end_matches('/').to_string();
    (!normalized.is_empty()).then_some(normalized)
}

fn bind_status_to_url(status: &mut HostedBundleStatus, rendezvous_url: Option<String>) {
    if status.rendezvous_url == rendezvous_url {
        return;
    }
    status.state = "unchecked".to_string();
    status.checked_unix_ms = None;
    status.last_error = None;
    status.mismatches.clear();
    status.rendezvous_url = rendezvous_url;
}

fn update_status_for_url(
    status: &mut HostedBundleStatus,
    rendezvous_url: &str,
    update: impl FnOnce(&mut HostedBundleStatus),
) -> bool {
    if status.rendezvous_url.as_deref() != Some(rendezvous_url) {
        return false;
    }
    update(status);
    true
}

/// Reset a completed verdict when the live Connect destination changes.
pub(crate) fn note_connect_config(enabled: bool, rendezvous_url: Option<&str>) {
    let key = enabled.then(|| verifier_url_key(rendezvous_url)).flatten();
    with_status(|status| bind_status_to_url(status, key));
}

/// Runtime settings changes do not wait for the next daily tick.
pub(crate) fn spawn_check_now() {
    tokio::spawn(check_once());
}

fn daemon_check_lock() -> &'static tokio::sync::Mutex<()> {
    static CHECK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    CHECK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

// ── Hash + Merkle primitives (RFC 6962/9162; replicas of the service) ──

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn sha256_hex(data: &[u8]) -> String {
    sha256(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn leaf_hash(leaf_json: &str) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + leaf_json.len());
    buf.push(0x00);
    buf.extend_from_slice(leaf_json.as_bytes());
    sha256(&buf)
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(65);
    buf.push(0x01);
    buf.extend_from_slice(left);
    buf.extend_from_slice(right);
    sha256(&buf)
}

/// Inclusion verification per RFC 9162 §2.1.3.2.
fn verify_inclusion(
    leaf: &[u8; 32],
    index: usize,
    size: usize,
    proof: &[[u8; 32]],
    root: &[u8; 32],
) -> bool {
    if index >= size {
        return false;
    }
    let mut fn_ = index;
    let mut sn = size - 1;
    let mut r = *leaf;
    for p in proof {
        if sn == 0 {
            return false;
        }
        if !fn_.is_multiple_of(2) || fn_ == sn {
            r = node_hash(p, &r);
            if fn_.is_multiple_of(2) {
                while fn_.is_multiple_of(2) && fn_ != 0 {
                    fn_ >>= 1;
                    sn >>= 1;
                }
            }
        } else {
            r = node_hash(&r, p);
        }
        fn_ >>= 1;
        sn >>= 1;
    }
    sn == 0 && r == *root
}

/// Consistency verification per RFC 9162 §2.1.4.2.
fn verify_consistency(
    old_size: usize,
    new_size: usize,
    old_root: &[u8; 32],
    new_root: &[u8; 32],
    proof: &[[u8; 32]],
) -> bool {
    if old_size == new_size {
        return old_root == new_root && proof.is_empty();
    }
    if old_size == 0 || old_size > new_size {
        return false;
    }
    let complete = old_size.is_power_of_two();
    let mut iter = proof.iter();
    let first = if complete {
        *old_root
    } else {
        match iter.next() {
            Some(first) => *first,
            None => return false,
        }
    };
    let mut fn_ = old_size - 1;
    let mut sn = new_size - 1;
    while !fn_.is_multiple_of(2) {
        fn_ >>= 1;
        sn >>= 1;
    }
    let mut fr = first;
    let mut sr = first;
    for p in iter.by_ref() {
        if sn == 0 {
            return false;
        }
        if !fn_.is_multiple_of(2) || fn_ == sn {
            fr = node_hash(p, &fr);
            sr = node_hash(p, &sr);
            if fn_.is_multiple_of(2) {
                while fn_.is_multiple_of(2) && fn_ != 0 {
                    fn_ >>= 1;
                    sn >>= 1;
                }
            }
        } else {
            sr = node_hash(&sr, p);
        }
        fn_ >>= 1;
        sn >>= 1;
    }
    fr == *old_root && sr == *new_root && sn == 0
}

/// The exact byte string the service's log key signs — REPLICATES
/// `bin/connect/transparency.rs::log_sth_payload` (golden twin below).
fn sth_payload(size: u64, root_b64u: &str, unix_ms: u64) -> String {
    format!("intendant-log-sth-v1\n{size}\n{root_b64u}\n{unix_ms}")
}

/// Canonical manifest hash — REPLICATES
/// `bin/connect/transparency.rs::manifest_hash_hex` (golden twin below):
/// sha256 (lowercase hex) over `intendant-artifact-manifest-v1\n` then
/// `{path}\t{sha256}\n` per artifact in list order.
fn manifest_hash_hex(artifacts: &[ManifestArtifact]) -> String {
    let mut canonical = String::from("intendant-artifact-manifest-v1\n");
    for artifact in artifacts {
        canonical.push_str(&artifact.path);
        canonical.push('\t');
        canonical.push_str(&artifact.sha256);
        canonical.push('\n');
    }
    sha256_hex(canonical.as_bytes())
}

fn b64u_decode(value: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| format!("invalid base64url: {e}"))
}

fn b64u_decode_hash(value: &str) -> Result<[u8; 32], String> {
    let bytes = b64u_decode(value)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "hash is not 32 bytes".to_string())
}

fn bounded_string(value: &str, label: &str, byte_cap: usize) -> Result<String, String> {
    if value.len() > byte_cap || value.chars().any(char::is_control) {
        return Err(format!("{label} exceeds its string bounds"));
    }
    Ok(value.to_string())
}

fn bounded_sha256(value: &str, label: &str) -> Result<String, String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(format!("{label} is not a hexadecimal sha256 digest"));
    }
    Ok(value.to_string())
}

// ── Wire shapes ──

/// The service's signed tree head, as fetched.
#[derive(Clone, Debug)]
struct Sth {
    size: u64,
    root: [u8; 32],
    root_b64u: String,
    unix_ms: u64,
    signature: Vec<u8>,
    public_key: Vec<u8>,
    public_key_b64u: String,
}

impl Sth {
    fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let size = value
            .get("size")
            .and_then(|v| v.as_u64())
            .ok_or("sth missing size")?;
        let root_b64u = bounded_string(
            value
                .get("root")
                .and_then(|v| v.as_str())
                .ok_or("sth missing root")?,
            "sth root",
            64,
        )?;
        let unix_ms = value
            .get("unix_ms")
            .and_then(|v| v.as_u64())
            .ok_or("sth missing unix_ms")?;
        let signature_text = bounded_string(
            value
                .get("signature")
                .and_then(|v| v.as_str())
                .ok_or("sth missing signature")?,
            "sth signature",
            256,
        )?;
        let signature = b64u_decode(&signature_text)?;
        let public_key_b64u = bounded_string(
            value
                .get("public_key")
                .and_then(|v| v.as_str())
                .ok_or("sth missing public_key")?,
            "sth public key",
            256,
        )?;
        Ok(Self {
            size,
            root: b64u_decode_hash(&root_b64u)?,
            root_b64u,
            unix_ms,
            signature,
            public_key: b64u_decode(&public_key_b64u)?,
            public_key_b64u,
        })
    }

    /// ES256 over the replicated payload (the log key signs fixed-form
    /// P-256, the same encoding WebCrypto verifies in the browser).
    fn verify_signature(&self) -> Result<(), String> {
        let payload = sth_payload(self.size, &self.root_b64u, self.unix_ms);
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            &self.public_key,
        )
        .verify(payload.as_bytes(), &self.signature)
        .map_err(|_| "tree head signature invalid".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestArtifact {
    path: String,
    sha256: String,
}

/// Stable executable entrypoints that every Connect bundle manifest must
/// cover. Additional embedded assets may evolve, but omitting one of these
/// paths can never turn a smaller declaration into a successful check.
const REQUIRED_BUNDLE_PATHS: &[&str] = &[
    "/",
    "/access",
    "/connect",
    "/favicon.png",
    "/install.ps1",
    "/install.sh",
    "/logo.svg",
    "/sw.js",
    "/trust",
];

/// The parsed `artifact_manifest` leaf, self-integrity verified (the
/// carried `manifest_hash` recomputes from the carried list).
#[derive(Clone, Debug)]
struct ManifestLeaf {
    unix_ms: u64,
    bundle_version: String,
    git_sha: String,
    manifest_hash: String,
    artifacts: Vec<ManifestArtifact>,
}

fn parse_manifest_leaf(leaf_json: &str) -> Result<ManifestLeaf, String> {
    if leaf_json.len() > LOG_LEAF_BYTE_CAP {
        return Err("manifest leaf exceeds its size cap".to_string());
    }
    let leaf: serde_json::Value =
        serde_json::from_str(leaf_json).map_err(|e| format!("manifest leaf is not JSON: {e}"))?;
    if leaf.get("kind").and_then(|v| v.as_str()) != Some("artifact_manifest") {
        return Err("leaf is not an artifact_manifest entry".to_string());
    }
    let artifact_values = leaf
        .get("artifacts")
        .and_then(|v| v.as_array())
        .ok_or("manifest leaf missing artifacts")?;
    if artifact_values.len() > MANIFEST_ARTIFACT_CAP {
        return Err("manifest leaf lists too many artifacts".to_string());
    }
    let artifacts: Vec<ManifestArtifact> = artifact_values
        .iter()
        .map(|entry| {
            let path = bounded_string(
                entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or("artifact missing path")?,
                "artifact path",
                ARTIFACT_PATH_BYTE_CAP,
            )?;
            let sha256 = bounded_sha256(
                entry
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .ok_or("artifact missing sha256")?,
                "artifact sha256",
            )?;
            if !path.starts_with('/') {
                return Err(format!("artifact path {path:?} is not absolute"));
            }
            Ok(ManifestArtifact { path, sha256 })
        })
        .collect::<Result<_, String>>()?;
    if artifacts
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
    {
        return Err("manifest artifact paths are not unique and strictly sorted".to_string());
    }
    let missing_required = REQUIRED_BUNDLE_PATHS
        .iter()
        .copied()
        .filter(|required| !artifacts.iter().any(|artifact| artifact.path == *required))
        .collect::<Vec<_>>();
    if !missing_required.is_empty() {
        return Err(format!(
            "manifest omits required served paths: {}",
            missing_required.join(", ")
        ));
    }
    let manifest_hash = bounded_sha256(
        leaf.get("manifest_hash")
            .and_then(|v| v.as_str())
            .ok_or("manifest leaf missing manifest_hash")?,
        "manifest hash",
    )?;
    if manifest_hash_hex(&artifacts) != manifest_hash {
        return Err("manifest_hash does not recompute from the carried artifact list".to_string());
    }
    Ok(ManifestLeaf {
        unix_ms: leaf.get("unix_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        bundle_version: bounded_string(
            leaf.get("bundle_version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            "bundle version",
            METADATA_STRING_BYTE_CAP,
        )?,
        git_sha: bounded_string(
            leaf.get("git_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            "git sha",
            METADATA_STRING_BYTE_CAP,
        )?,
        manifest_hash,
        artifacts,
    })
}

// ── The pinned tree head (TOFU, then consistency forever) ──

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SthPin {
    size: u64,
    /// b64u, as the service reports it.
    root: String,
    public_key: String,
    pinned_unix_ms: u64,
}

/// Highest artifact-manifest observation accepted for one rendezvous. The
/// tree-head pin proves append-only history; this companion pin prevents a
/// server from selecting an older, still-included bundle leaf after a newer
/// bundle has already been verified.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ArtifactManifestPin {
    index: u64,
    manifest_hash: String,
    pinned_unix_ms: u64,
}

/// One pin per rendezvous host under the daemon state root
/// (`~/.intendant/hosted-verify/<host>.json`; `$INTENDANT_HOME` honored
/// by `platform::intendant_home`).
fn pin_path(state_root: &Path, base: &Url) -> PathBuf {
    let host = base.host_str().unwrap_or("unknown");
    let name: String = match base.port() {
        Some(port) => format!("{host}_{port}"),
        None => host.to_string(),
    }
    .chars()
    .map(|c| {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            c
        } else {
            '_'
        }
    })
    .collect();
    state_root
        .join("hosted-verify")
        .join(format!("{name}.json"))
}

fn artifact_manifest_pin_path(state_root: &Path, base: &Url) -> PathBuf {
    pin_path(state_root, base).with_extension("artifact-manifest.json")
}

fn load_pin(path: &Path) -> Result<Option<SthPin>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("parse {}: {error}", path.display()))
}

fn load_artifact_manifest_pin(path: &Path) -> Result<Option<ArtifactManifestPin>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("parse {}: {error}", path.display()))
}

fn check_artifact_manifest_high_water(
    pin: Option<&ArtifactManifestPin>,
    index: u64,
    manifest_hash: &str,
) -> Result<(), String> {
    let Some(pin) = pin else {
        return Ok(());
    };
    if index < pin.index {
        return Err(format!(
            "artifact manifest index regressed from pinned {} to {index}",
            pin.index
        ));
    }
    if index == pin.index && manifest_hash != pin.manifest_hash {
        return Err(format!(
            "artifact manifest changed at pinned index {}",
            pin.index
        ));
    }
    Ok(())
}

fn commit_artifact_manifest_pin(
    path: &Path,
    index: u64,
    manifest_hash: &str,
) -> Result<(), String> {
    let lock_dir = pin_lock_dir(path)?;
    crate::access::authority_store::with_lock(&lock_dir, || {
        let current = load_artifact_manifest_pin(path).map_err(crate::access::AccessError)?;
        check_artifact_manifest_high_water(current.as_ref(), index, manifest_hash)
            .map_err(crate::access::AccessError)?;
        if current
            .as_ref()
            .is_some_and(|pin| pin.index == index && pin.manifest_hash == manifest_hash)
        {
            return Ok(());
        }
        let pin = ArtifactManifestPin {
            index,
            manifest_hash: manifest_hash.to_string(),
            pinned_unix_ms: current
                .as_ref()
                .map(|pin| pin.pinned_unix_ms)
                .unwrap_or_else(now_unix_ms),
        };
        let text = serde_json::to_string_pretty(&pin)
            .map_err(|error| crate::access::AccessError(error.to_string()))?;
        crate::access::authority_store::atomic_write_private_locked(path, text.as_bytes())
    })
    .map_err(|error| error.to_string())
}

fn same_tree_head(left: &SthPin, right: &SthPin) -> bool {
    left.size == right.size && left.root == right.root && left.public_key == right.public_key
}

fn pin_lock_dir(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("pin path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("pin path has no file name: {}", path.display()))?;
    Ok(parent.join(".pin-locks").join(name))
}

#[derive(Debug, PartialEq, Eq)]
enum PinCommit {
    Committed,
    /// Another verifier advanced or replaced the basis while this verifier
    /// was checking. The caller must reconcile this exact observation now.
    Changed(Option<SthPin>),
}

/// Commit a verified tree head without letting an older concurrent check
/// overwrite a newer observation. A changed basis is returned as typed data
/// so the caller can reconcile it immediately rather than suppressing the
/// observation as an availability error until the next daily run.
fn commit_pin(
    path: &Path,
    basis: Option<&SthPin>,
    candidate: &SthPin,
) -> Result<PinCommit, String> {
    let lock_dir = pin_lock_dir(path)?;
    crate::access::authority_store::with_lock(&lock_dir, || {
        let current = load_pin(path).map_err(crate::access::AccessError)?;
        if current
            .as_ref()
            .is_some_and(|pin| same_tree_head(pin, candidate))
        {
            return Ok(PinCommit::Committed);
        }
        let basis_is_current = match (basis, current.as_ref()) {
            (None, None) => true,
            (Some(basis), Some(current)) => same_tree_head(basis, current),
            _ => false,
        };
        if !basis_is_current {
            return Ok(PinCommit::Changed(current));
        }

        let mut committed = candidate.clone();
        if let Some(current) = current {
            committed.pinned_unix_ms = current.pinned_unix_ms;
        }
        let text = serde_json::to_string_pretty(&committed)
            .map_err(|error| crate::access::AccessError(error.to_string()))?;
        crate::access::authority_store::atomic_write_private_locked(path, text.as_bytes())?;
        Ok(PinCommit::Committed)
    })
    .map_err(|error| error.to_string())
}

/// What the pinned tree head demands of the fetched one. Pure — the
/// network consistency fetch happens only when this says so.
#[derive(Debug, PartialEq)]
enum PinDecision {
    /// No pin yet: verify what we can, then pin (trust on first use).
    FirstContact,
    /// Same size: roots already compared equal, nothing to fetch.
    Unchanged,
    /// Log grew: fetch and verify a consistency proof old → new.
    NeedConsistency { old_size: u64, old_root: [u8; 32] },
}

fn pin_decision(pin: Option<&SthPin>, sth: &Sth) -> Result<PinDecision, String> {
    let Some(pin) = pin else {
        return Ok(PinDecision::FirstContact);
    };
    if pin.public_key != sth.public_key_b64u {
        return Err(
            "log signing key changed — history can no longer be verified against your pin \
             (if the operator legitimately rotated the key, delete the pin file to re-anchor)"
                .to_string(),
        );
    }
    if sth.size < pin.size {
        return Err(format!(
            "log shrank from pinned size {} to {} — history was rewritten",
            pin.size, sth.size
        ));
    }
    if sth.size == pin.size {
        if sth.root_b64u != pin.root {
            return Err("tree root changed at the pinned size — history was rewritten".to_string());
        }
        return Ok(PinDecision::Unchanged);
    }
    Ok(PinDecision::NeedConsistency {
        old_size: pin.size,
        old_root: b64u_decode_hash(&pin.root)?,
    })
}

// ── Artifact comparison ──

/// Bundle artifacts are ≤ a few MB; anything past this cap is already a
/// divergence, not a download worth finishing.
const ARTIFACT_BYTE_CAP: usize = 64 * 1024 * 1024;
/// Bound a whole manifest independently of its artifact count.
const BUNDLE_TOTAL_BYTE_CAP: usize = 256 * 1024 * 1024;
const BUNDLE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const ARTIFACT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const ARTIFACT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum ArtifactFetch {
    Hashed { sha256_hex: String, bytes: usize },
    HttpStatus(u16),
    TooLarge { bytes: usize },
}

/// The per-artifact verdict: `None` = matches the log.
fn artifact_mismatch(artifact: &ManifestArtifact, fetched: &ArtifactFetch) -> Option<String> {
    match fetched {
        ArtifactFetch::Hashed { sha256_hex, .. } if *sha256_hex == artifact.sha256 => None,
        ArtifactFetch::Hashed { sha256_hex, .. } => Some(format!(
            "{}: manifest {} · served {}",
            artifact.path,
            short_hash(&artifact.sha256),
            short_hash(sha256_hex),
        )),
        ArtifactFetch::HttpStatus(status) => Some(format!("{}: HTTP {status}", artifact.path)),
        ArtifactFetch::TooLarge { .. } => Some(format!(
            "{}: response exceeded {} MiB",
            artifact.path,
            ARTIFACT_BYTE_CAP / (1024 * 1024)
        )),
    }
}

fn short_hash(hex: &str) -> String {
    format!("{}…", &hex[..12.min(hex.len())])
}

// ── The verifier core ──

/// Why a run produced no verdict — split so the tripwire can fail open
/// on the left arm and alarm on the right one.
#[derive(Debug)]
pub(crate) enum VerifyFailure {
    /// Could not complete the check (network, endpoint missing, older
    /// service). Advisory surfaces record it; nothing alarms.
    Unavailable(String),
    /// The check completed and the origin diverges from its log (or the
    /// log itself failed verification). This is the loud case.
    Verification {
        summary: String,
        mismatches: Vec<String>,
    },
}

pub(crate) struct VerifyReport {
    pub log_size: u64,
    pub manifest_index: u64,
    pub manifest_unix_ms: u64,
    pub bundle_version: String,
    pub git_sha: String,
    pub manifest_hash: String,
    pub artifact_count: usize,
    /// `None` = first contact (this run created the pin).
    pub pinned_from_size: Option<u64>,
}

fn http_client() -> Result<reqwest::Client, String> {
    // No compression features are enabled, so no Accept-Encoding is sent
    // and the body bytes hashed are the bytes the origin serves.
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // Verification fetches never follow origin-controlled redirects.
        // A redirect is a distinct response to verify, not permission to
        // reach a different host (including loopback, link-local, or LAN).
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("intendant-hosted-verify")
        .build()
        .map_err(|e| e.to_string())
}

fn release_download_client() -> Result<reqwest::Client, String> {
    // GitHub release assets normally redirect to its object store. This
    // client is used only for a URL returned by the separately fetched
    // GitHub release API, never for Connect-controlled metadata. Large
    // artifacts have no short total timeout; the fetcher below separately
    // bounds connection/header wait and idle time between body chunks.
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("intendant-hosted-verify")
        .build()
        .map_err(|error| error.to_string())
}

async fn fetch_json(client: &reqwest::Client, url: Url) -> Result<serde_json::Value, String> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}"));
    }
    response_json_limited(response, &url).await
}

async fn response_json_limited(
    response: reqwest::Response,
    url: &Url,
) -> Result<serde_json::Value, String> {
    use futures_util::StreamExt as _;

    if response
        .content_length()
        .is_some_and(|length| length > METADATA_RESPONSE_BYTE_CAP as u64)
    {
        return Err(format!(
            "GET {url}: metadata response exceeds {} bytes",
            METADATA_RESPONSE_BYTE_CAP
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("GET {url}: {error}"))?;
        if body.len().saturating_add(chunk.len()) > METADATA_RESPONSE_BYTE_CAP {
            return Err(format!(
                "GET {url}: metadata response exceeds {} bytes",
                METADATA_RESPONSE_BYTE_CAP
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|error| format!("GET {url}: {error}"))
}

async fn verify_consistency_extension(
    client: &reqwest::Client,
    base: &Url,
    old_size: u64,
    old_root: &[u8; 32],
    new_size: u64,
    new_root: &[u8; 32],
) -> Result<(), VerifyFailure> {
    use VerifyFailure::{Unavailable, Verification};
    let url = crate::connect_rendezvous::join_url(base, "api/log/consistency")
        .map_err(Unavailable)
        .map(|mut url| {
            url.set_query(Some(&format!("old={old_size}&new={new_size}")));
            url
        })?;
    let consistency = fetch_json(client, url).await.map_err(Unavailable)?;
    let proof_values = consistency
        .get("proof")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Unavailable("consistency response missing proof".to_string()))?;
    if proof_values.len() > LOG_PROOF_HASH_CAP {
        return Err(Unavailable(
            "consistency response proof exceeds its element cap".to_string(),
        ));
    }
    let consistency_proof: Vec<[u8; 32]> = proof_values
        .iter()
        .map(|hash| b64u_decode_hash(hash.as_str().unwrap_or_default()))
        .collect::<Result<_, String>>()
        .map_err(Unavailable)?;
    if !verify_consistency(
        old_size as usize,
        new_size as usize,
        old_root,
        new_root,
        &consistency_proof,
    ) {
        return Err(Verification {
            summary: "consistency proof failed — history was rewritten since a verified tree head"
                .to_string(),
            mismatches: Vec::new(),
        });
    }
    Ok(())
}

/// Fetch one artifact, hashing as it streams. Transport errors bubble as
/// `Err` (the whole run becomes Unavailable); HTTP error statuses and
/// oversize bodies are verdicts, not failures.
async fn fetch_artifact(
    client: &reqwest::Client,
    url: Url,
    byte_cap: usize,
) -> Result<ArtifactFetch, String> {
    fetch_artifact_with_timeouts(
        client,
        url,
        byte_cap,
        ARTIFACT_RESPONSE_TIMEOUT,
        ARTIFACT_IDLE_TIMEOUT,
    )
    .await
}

async fn fetch_artifact_with_timeouts(
    client: &reqwest::Client,
    url: Url,
    byte_cap: usize,
    response_timeout: Duration,
    idle_timeout: Duration,
) -> Result<ArtifactFetch, String> {
    use futures_util::StreamExt as _;
    let response = tokio::time::timeout(response_timeout, client.get(url.clone()).send())
        .await
        .map_err(|_| format!("GET {url}: response headers timed out"))?
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Ok(ArtifactFetch::HttpStatus(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > byte_cap as u64)
    {
        return Ok(ArtifactFetch::TooLarge { bytes: 0 });
    }
    let mut hasher = Sha256::new();
    let mut total = 0usize;
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| format!("GET {url}: response body became idle"))?;
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|e| format!("GET {url}: {e}"))?;
        total = total.saturating_add(chunk.len());
        if total > byte_cap {
            return Ok(ArtifactFetch::TooLarge { bytes: total });
        }
        hasher.update(&chunk);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(ArtifactFetch::Hashed {
        sha256_hex: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        bytes: total,
    })
}

#[derive(Clone, Copy)]
struct BundleFetchLimits {
    per_artifact_bytes: usize,
    total_bytes: usize,
    total_timeout: Duration,
    response_timeout: Duration,
    idle_timeout: Duration,
}

const BUNDLE_FETCH_LIMITS: BundleFetchLimits = BundleFetchLimits {
    per_artifact_bytes: ARTIFACT_BYTE_CAP,
    total_bytes: BUNDLE_TOTAL_BYTE_CAP,
    total_timeout: BUNDLE_TOTAL_TIMEOUT,
    response_timeout: ARTIFACT_RESPONSE_TIMEOUT,
    idle_timeout: ARTIFACT_IDLE_TIMEOUT,
};

async fn compare_live_artifacts(
    client: &reqwest::Client,
    base: &Url,
    artifacts: &[ManifestArtifact],
    limits: BundleFetchLimits,
) -> Result<Vec<String>, VerifyFailure> {
    use VerifyFailure::Unavailable;

    let compare = async {
        let mut mismatches = Vec::new();
        let mut fetched_bytes = 0usize;
        for artifact in artifacts {
            let remaining = limits
                .total_bytes
                .checked_sub(fetched_bytes)
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    Unavailable(
                        "hosted bundle verification reached its aggregate byte budget".to_string(),
                    )
                })?;
            let fetch_cap = limits.per_artifact_bytes.min(remaining);
            let url =
                crate::connect_rendezvous::join_url(base, &artifact.path).map_err(Unavailable)?;
            let fetched = fetch_artifact_with_timeouts(
                client,
                url,
                fetch_cap,
                limits.response_timeout,
                limits.idle_timeout,
            )
            .await
            .map_err(Unavailable)?;
            match &fetched {
                ArtifactFetch::Hashed { bytes, .. } => {
                    fetched_bytes = fetched_bytes.saturating_add(*bytes);
                }
                ArtifactFetch::TooLarge { bytes } => {
                    fetched_bytes = fetched_bytes.saturating_add(*bytes);
                    if fetch_cap < limits.per_artifact_bytes || fetched_bytes > limits.total_bytes {
                        return Err(Unavailable(
                            "hosted bundle verification reached its aggregate byte budget"
                                .to_string(),
                        ));
                    }
                }
                _ => {}
            }
            if let Some(diff) = artifact_mismatch(artifact, &fetched) {
                mismatches.push(diff);
            }
        }
        Ok(mismatches)
    };

    tokio::time::timeout(limits.total_timeout, compare)
        .await
        .map_err(|_| {
            Unavailable("hosted bundle verification reached its aggregate time budget".to_string())
        })?
}

/// A log-endpoint response carried through the shared first half of the
/// ritual: the tree head stands on its signature, the leaf is IN the
/// tree the head signs, and the head extends the pinned one. The caller
/// finishes its own artifact comparison, then advances or reconciles
/// `pin_candidate` against `pin_file` — only after everything held.
struct VerifiedLogEntry {
    index: u64,
    leaf_json: String,
    sth: Sth,
    pin_file: PathBuf,
    pin_basis: Option<SthPin>,
    pinned_from_size: Option<u64>,
    pin_candidate: SthPin,
}

async fn verify_logged_entry(
    client: &reqwest::Client,
    base: &Url,
    state_root: &Path,
    response: &serde_json::Value,
) -> Result<VerifiedLogEntry, VerifyFailure> {
    use VerifyFailure::{Unavailable, Verification};
    let verification = |summary: String| Verification {
        summary,
        mismatches: Vec::new(),
    };

    // 1. The signed tree head stands on its own signature.
    let sth =
        Sth::parse(response.get("sth").unwrap_or(&serde_json::Value::Null)).map_err(Unavailable)?;
    sth.verify_signature().map_err(verification)?;

    // 2. The manifest entry is IN the tree the head signs.
    let index = response
        .get("index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| Unavailable("response missing index".to_string()))?;
    let leaf_json = response
        .get("leaf_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Unavailable("response missing leaf_json".to_string()))?;
    if leaf_json.len() > LOG_LEAF_BYTE_CAP {
        return Err(Unavailable(
            "response leaf exceeds its size cap".to_string(),
        ));
    }
    let proof_values = response
        .get("proof")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Unavailable("response missing proof".to_string()))?;
    if proof_values.len() > LOG_PROOF_HASH_CAP {
        return Err(Unavailable(
            "response inclusion proof exceeds its element cap".to_string(),
        ));
    }
    let proof: Vec<[u8; 32]> = proof_values
        .iter()
        .map(|hash| b64u_decode_hash(hash.as_str().unwrap_or_default()))
        .collect::<Result<_, String>>()
        .map_err(Unavailable)?;
    if !verify_inclusion(
        &leaf_hash(leaf_json),
        index as usize,
        sth.size as usize,
        &proof,
        &sth.root,
    ) {
        return Err(verification(
            "inclusion proof failed — the served manifest is not in the signed log".to_string(),
        ));
    }

    // 3. The tree head extends the one pinned last time (append-only).
    let pin_file = pin_path(state_root, base);
    let pin = load_pin(&pin_file).map_err(Unavailable)?;
    let pinned_from_size = pin.as_ref().map(|p| p.size);
    match pin_decision(pin.as_ref(), &sth).map_err(verification)? {
        PinDecision::FirstContact | PinDecision::Unchanged => {}
        PinDecision::NeedConsistency { old_size, old_root } => {
            verify_consistency_extension(client, base, old_size, &old_root, sth.size, &sth.root)
                .await?;
        }
    }

    let pin_candidate = SthPin {
        size: sth.size,
        root: sth.root_b64u.clone(),
        public_key: sth.public_key_b64u.clone(),
        pinned_unix_ms: pin
            .as_ref()
            .map(|p| p.pinned_unix_ms)
            .unwrap_or_else(now_unix_ms),
    };
    Ok(VerifiedLogEntry {
        index,
        leaf_json: leaf_json.to_string(),
        sth,
        pin_file,
        pin_basis: pin,
        pinned_from_size,
        pin_candidate,
    })
}

const PIN_RECONCILE_ATTEMPTS: usize = 4;

#[derive(Debug, PartialEq, Eq)]
enum ConcurrentPinOrder {
    Same,
    CandidateExtendsCurrent,
    CurrentExtendsCandidate,
}

fn concurrent_pin_order(
    candidate: &SthPin,
    current: &SthPin,
) -> Result<ConcurrentPinOrder, VerifyFailure> {
    use VerifyFailure::Verification;
    if candidate.public_key != current.public_key {
        return Err(Verification {
            summary: "concurrent transparency observations use different log signing keys"
                .to_string(),
            mismatches: Vec::new(),
        });
    }
    match candidate.size.cmp(&current.size) {
        std::cmp::Ordering::Equal if candidate.root == current.root => Ok(ConcurrentPinOrder::Same),
        std::cmp::Ordering::Equal => Err(Verification {
            summary: format!(
                "concurrent transparency observations have different roots at tree size {}",
                candidate.size
            ),
            mismatches: Vec::new(),
        }),
        std::cmp::Ordering::Greater => Ok(ConcurrentPinOrder::CandidateExtendsCurrent),
        std::cmp::Ordering::Less => Ok(ConcurrentPinOrder::CurrentExtendsCandidate),
    }
}

fn decoded_verified_pin_root(pin: &SthPin, label: &str) -> Result<[u8; 32], VerifyFailure> {
    b64u_decode_hash(&pin.root).map_err(|error| VerifyFailure::Verification {
        summary: format!("{label} transparency pin has an invalid tree root: {error}"),
        mismatches: Vec::new(),
    })
}

/// Finish a successful artifact check by advancing the pin. If another
/// verifier observed a head meanwhile, compare the two observations now:
/// same-size disagreement is an immediate verification result; differing
/// sizes must carry a valid consistency proof before either head is accepted.
async fn commit_verified_pin(
    client: &reqwest::Client,
    base: &Url,
    entry: &VerifiedLogEntry,
) -> Result<(), VerifyFailure> {
    use VerifyFailure::Unavailable;
    let mut basis = entry.pin_basis.clone();
    for _ in 0..PIN_RECONCILE_ATTEMPTS {
        match commit_pin(&entry.pin_file, basis.as_ref(), &entry.pin_candidate)
            .map_err(Unavailable)?
        {
            PinCommit::Committed => return Ok(()),
            PinCommit::Changed(None) => {
                return Err(Unavailable(
                    "transparency pin was removed while verification was running; retrying from a fresh observation is required"
                        .to_string(),
                ));
            }
            PinCommit::Changed(Some(current)) => {
                match concurrent_pin_order(&entry.pin_candidate, &current)? {
                    ConcurrentPinOrder::Same => return Ok(()),
                    ConcurrentPinOrder::CandidateExtendsCurrent => {
                        let old_root = decoded_verified_pin_root(&current, "current")?;
                        let new_root =
                            decoded_verified_pin_root(&entry.pin_candidate, "candidate")?;
                        verify_consistency_extension(
                            client,
                            base,
                            current.size,
                            &old_root,
                            entry.pin_candidate.size,
                            &new_root,
                        )
                        .await?;
                        basis = Some(current);
                    }
                    ConcurrentPinOrder::CurrentExtendsCandidate => {
                        let old_root =
                            decoded_verified_pin_root(&entry.pin_candidate, "candidate")?;
                        let new_root = decoded_verified_pin_root(&current, "current")?;
                        verify_consistency_extension(
                            client,
                            base,
                            entry.pin_candidate.size,
                            &old_root,
                            current.size,
                            &new_root,
                        )
                        .await?;
                        // Do not regress the pin. Confirm under the lock that
                        // the reconciled newer head is still current.
                        match commit_pin(&entry.pin_file, Some(&current), &current)
                            .map_err(Unavailable)?
                        {
                            PinCommit::Committed => return Ok(()),
                            PinCommit::Changed(Some(_)) => {
                                basis = entry.pin_basis.clone();
                            }
                            PinCommit::Changed(None) => {
                                return Err(Unavailable(
                                    "transparency pin was removed during immediate reconciliation; retrying from a fresh observation is required"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Err(Unavailable(
        "transparency pin kept changing during immediate reconciliation; retry the check"
            .to_string(),
    ))
}

/// The full out-of-band check against one rendezvous origin. On success
/// the tree head is (re-)pinned under `state_root`.
pub(crate) async fn verify_hosted_bundle(
    base: &Url,
    state_root: &Path,
) -> Result<VerifyReport, VerifyFailure> {
    use VerifyFailure::{Unavailable, Verification};
    let verification = |summary: String| Verification {
        summary,
        mismatches: Vec::new(),
    };
    let client = http_client().map_err(Unavailable)?;
    let manifest_url = crate::connect_rendezvous::join_url(base, "api/log/artifact-manifest")
        .map_err(Unavailable)?;
    let response = fetch_json(&client, manifest_url)
        .await
        .map_err(Unavailable)?;
    if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(Unavailable(
            "artifact-manifest endpoint returned an error".to_string(),
        ));
    }
    if response.get("found").and_then(|v| v.as_bool()) != Some(true) {
        return Err(Unavailable(
            "this rendezvous logs no artifact manifest (older intendant-connect?)".to_string(),
        ));
    }

    // The tree head, inclusion, and pin consistency — the shared ritual.
    let entry = verify_logged_entry(&client, base, state_root, &response).await?;

    // The manifest self-verifies, then the live bytes match it.
    let leaf = parse_manifest_leaf(&entry.leaf_json).map_err(verification)?;
    let manifest_pin_file = artifact_manifest_pin_path(state_root, base);
    let manifest_pin = load_artifact_manifest_pin(&manifest_pin_file).map_err(verification)?;
    check_artifact_manifest_high_water(manifest_pin.as_ref(), entry.index, &leaf.manifest_hash)
        .map_err(verification)?;
    let mismatches =
        compare_live_artifacts(&client, base, &leaf.artifacts, BUNDLE_FETCH_LIMITS).await?;
    if !mismatches.is_empty() {
        return Err(Verification {
            summary: format!(
                "{} of {} served artifacts diverge from the transparency log",
                mismatches.len(),
                leaf.artifacts.len()
            ),
            mismatches,
        });
    }

    // Everything held. Advance the selected-manifest high-water mark before
    // the tree head: a crash between the writes may cause an extra
    // consistency check, but can never reopen an older bundle observation.
    commit_artifact_manifest_pin(&manifest_pin_file, entry.index, &leaf.manifest_hash)
        .map_err(verification)?;
    commit_verified_pin(&client, base, &entry).await?;

    Ok(VerifyReport {
        log_size: entry.sth.size,
        manifest_index: entry.index,
        manifest_unix_ms: leaf.unix_ms,
        bundle_version: leaf.bundle_version,
        git_sha: leaf.git_sha,
        manifest_hash: leaf.manifest_hash,
        artifact_count: leaf.artifacts.len(),
        pinned_from_size: entry.pinned_from_size,
    })
}

// ── Release transparency (`release_manifest` entries) ──
//
// The same log also commits the project's app releases: the
// tag-triggered release pipeline submits a `release_manifest` (tag,
// version, platforms, per-artifact name + sha256 + size) after
// publishing to GitHub Releases. `--releases` verifies that leg: the
// manifest's inclusion and the pinned tree head exactly as above, then
// the GitHub release's asset METADATA (names, sizes, and the API's
// sha256 digests where present) against the logged list — release
// artifacts run to hundreds of MB, so the default path never downloads
// them; `--download` upgrades to full fetch-and-hash verification.

/// Repository whose GitHub releases the hosted log's release manifests
/// describe (`macos-app/UpdateChecker.swift` twins this slug);
/// `--repo` overrides for self-hosted forks.
const DEFAULT_RELEASE_REPO: &str = "intendant-dev/Intendant";
const GITHUB_API_BASE: &str = "https://api.github.com";

/// `--download` re-fetches whole release artifacts, which are allowed to
/// be far bigger than Connect page/installer bundles.
const RELEASE_DOWNLOAD_BYTE_CAP: usize = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReleaseArtifact {
    name: String,
    sha256: String,
    size: u64,
}

/// Canonical release-manifest hash — REPLICATES
/// `bin/connect/transparency.rs::release_manifest_hash_hex` (golden twin
/// below): sha256 (lowercase hex) over `intendant-release-manifest-v1\n`
/// `{tag}\n` then `{name}\t{sha256}\t{size}\n` per artifact in list order.
fn release_manifest_hash_hex(tag: &str, artifacts: &[ReleaseArtifact]) -> String {
    let mut canonical = String::from("intendant-release-manifest-v1\n");
    canonical.push_str(tag);
    canonical.push('\n');
    for artifact in artifacts {
        canonical.push_str(&artifact.name);
        canonical.push('\t');
        canonical.push_str(&artifact.sha256);
        canonical.push('\t');
        canonical.push_str(&artifact.size.to_string());
        canonical.push('\n');
    }
    sha256_hex(canonical.as_bytes())
}

/// The parsed `release_manifest` leaf, self-integrity verified (the
/// carried `manifest_hash` recomputes from the carried list).
#[derive(Clone, Debug)]
struct ReleaseLeaf {
    unix_ms: u64,
    tag: String,
    version: String,
    platforms: Vec<String>,
    manifest_hash: String,
    artifacts: Vec<ReleaseArtifact>,
}

fn parse_release_leaf(leaf_json: &str) -> Result<ReleaseLeaf, String> {
    if leaf_json.len() > LOG_LEAF_BYTE_CAP {
        return Err("release leaf exceeds its size cap".to_string());
    }
    let leaf: serde_json::Value =
        serde_json::from_str(leaf_json).map_err(|e| format!("release leaf is not JSON: {e}"))?;
    if leaf.get("kind").and_then(|v| v.as_str()) != Some("release_manifest") {
        return Err("leaf is not a release_manifest entry".to_string());
    }
    let tag = bounded_string(
        leaf.get("tag")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .ok_or("release leaf missing tag")?,
        "release tag",
        METADATA_STRING_BYTE_CAP,
    )?;
    let artifact_values = leaf
        .get("artifacts")
        .and_then(|v| v.as_array())
        .ok_or("release leaf missing artifacts")?;
    if artifact_values.len() > MANIFEST_ARTIFACT_CAP {
        return Err("release leaf lists too many artifacts".to_string());
    }
    let artifacts: Vec<ReleaseArtifact> = artifact_values
        .iter()
        .map(|entry| {
            let name = bounded_string(
                entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.is_empty())
                    .ok_or("artifact missing name")?,
                "release artifact name",
                ARTIFACT_NAME_BYTE_CAP,
            )?;
            let sha256 = bounded_sha256(
                entry
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .ok_or("artifact missing sha256")?,
                "release artifact sha256",
            )?;
            let size = entry
                .get("size")
                .and_then(|v| v.as_u64())
                .ok_or("artifact missing size")?;
            Ok(ReleaseArtifact { name, sha256, size })
        })
        .collect::<Result<_, String>>()?;
    if artifacts.is_empty() {
        return Err("release leaf lists no artifacts".to_string());
    }
    let manifest_hash = bounded_sha256(
        leaf.get("manifest_hash")
            .and_then(|v| v.as_str())
            .ok_or("release leaf missing manifest_hash")?,
        "release manifest hash",
    )?;
    if release_manifest_hash_hex(&tag, &artifacts) != manifest_hash {
        return Err("manifest_hash does not recompute from the carried artifact list".to_string());
    }
    Ok(ReleaseLeaf {
        unix_ms: leaf.get("unix_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        tag,
        version: bounded_string(
            leaf.get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            "release version",
            METADATA_STRING_BYTE_CAP,
        )?,
        platforms: {
            let values = leaf
                .get("platforms")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default();
            if values.len() > RELEASE_PLATFORM_CAP {
                return Err("release leaf lists too many platforms".to_string());
            }
            values
                .iter()
                .map(|value| {
                    bounded_string(
                        value.as_str().ok_or("release platform is not a string")?,
                        "release platform",
                        METADATA_STRING_BYTE_CAP,
                    )
                })
                .collect::<Result<Vec<_>, String>>()?
        },
        manifest_hash,
        artifacts,
    })
}

/// One asset as the GitHub releases API reports it. `digest`
/// (`sha256:<hex>`) is what makes the no-download default meaningful;
/// it is absent on assets uploaded before GitHub grew the field.
#[derive(Clone, Debug)]
struct GithubAsset {
    name: String,
    size: u64,
    sha256: Option<String>,
    download_url: Option<String>,
}

fn parse_github_assets(release: &serde_json::Value) -> Result<Vec<GithubAsset>, String> {
    let values = release
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or("GitHub release response missing assets")?;
    if values.len() > GITHUB_ASSET_CAP {
        return Err("GitHub release response lists too many assets".to_string());
    }
    values
        .iter()
        .map(|asset| {
            let name = bounded_string(
                asset
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.is_empty())
                    .ok_or("GitHub asset missing name")?,
                "GitHub asset name",
                ARTIFACT_NAME_BYTE_CAP,
            )?;
            let size = asset
                .get("size")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| format!("GitHub asset {name} missing size"))?;
            let sha256 = asset
                .get("digest")
                .and_then(|v| v.as_str())
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .map(|digest| bounded_sha256(digest, "GitHub asset digest"))
                .transpose()?;
            let download_url = asset
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .map(|url| bounded_string(url, "GitHub asset download URL", ARTIFACT_PATH_BYTE_CAP))
                .transpose()?;
            Ok(GithubAsset {
                name,
                size,
                sha256,
                download_url,
            })
        })
        .collect()
}

/// What the metadata comparison of one logged artifact proved.
#[derive(Debug, PartialEq, Eq)]
enum AssetCheck {
    /// GitHub reports a sha256 digest and it matches the log.
    DigestVerified,
    /// Present with the logged size, but GitHub exposes no digest for
    /// it — presence and size only (upgrade with `--download`).
    PresenceOnly,
}

/// The per-artifact metadata verdict: `Err` = a mismatch line.
fn check_release_artifact(
    artifact: &ReleaseArtifact,
    assets: &[GithubAsset],
) -> Result<AssetCheck, String> {
    let Some(asset) = assets.iter().find(|a| a.name == artifact.name) else {
        return Err(format!("{}: not on the GitHub release", artifact.name));
    };
    if asset.size != artifact.size {
        return Err(format!(
            "{}: logged {} bytes · GitHub reports {}",
            artifact.name, artifact.size, asset.size
        ));
    }
    match &asset.sha256 {
        Some(digest) if *digest == artifact.sha256 => Ok(AssetCheck::DigestVerified),
        Some(digest) => Err(format!(
            "{}: logged sha256 {} · GitHub digest {}",
            artifact.name,
            short_hash(&artifact.sha256),
            short_hash(digest),
        )),
        None => Ok(AssetCheck::PresenceOnly),
    }
}

/// Assets on the release the log never blessed are loud in both modes:
/// a quietly added artifact is exactly the equivocation this check
/// exists to catch.
fn unlogged_assets(logged: &[ReleaseArtifact], assets: &[GithubAsset]) -> Vec<String> {
    assets
        .iter()
        .filter(|asset| logged.iter().all(|artifact| artifact.name != asset.name))
        .map(|asset| {
            format!(
                "{}: on the GitHub release but not in the logged manifest",
                asset.name
            )
        })
        .collect()
}

/// GitHub API fetch that keeps the HTTP status: 404 is a verdict for
/// the caller (log commits a release GitHub does not have), everything
/// else transport-ish. Sends the API's preferred Accept header.
async fn fetch_github_json(
    client: &reqwest::Client,
    url: Url,
) -> Result<(u16, Option<serde_json::Value>), String> {
    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status().as_u16();
    let body = response_json_limited(response, &url).await.ok();
    Ok((status, body))
}

#[derive(Debug)]
pub(crate) struct ReleaseVerifyReport {
    pub log_size: u64,
    pub manifest_index: u64,
    pub manifest_unix_ms: u64,
    pub tag: String,
    pub version: String,
    pub platforms: Vec<String>,
    pub manifest_hash: String,
    pub artifact_count: usize,
    /// Metadata mode: how many artifacts GitHub's API sha256 digests
    /// confirmed, vs. presence+size only.
    pub digest_verified: usize,
    pub presence_only: usize,
    /// `--download` mode: artifacts re-downloaded and hash-verified.
    pub downloaded: usize,
    /// `None` = first contact (this run created the pin).
    pub pinned_from_size: Option<u64>,
}

fn github_release_tag_url(github_api: &Url, repo: &str, tag: &str) -> Result<Url, String> {
    let mut url =
        crate::connect_rendezvous::join_url(github_api, &format!("repos/{repo}/releases/tags"))?;
    url.path_segments_mut()
        .map_err(|_| "GitHub API URL cannot carry path segments".to_string())?
        .push(tag);
    Ok(url)
}

/// The out-of-band release check: the logged manifest (latest, or for
/// one tag) against the log's proofs and the GitHub release. Shares the
/// per-host tree-head pin with the bundle check — one log, one pin.
pub(crate) async fn verify_hosted_release(
    base: &Url,
    github_api: &Url,
    repo: &str,
    tag: Option<&str>,
    download: bool,
    state_root: &Path,
) -> Result<ReleaseVerifyReport, VerifyFailure> {
    use VerifyFailure::{Unavailable, Verification};
    let verification = |summary: String| Verification {
        summary,
        mismatches: Vec::new(),
    };
    let client = http_client().map_err(Unavailable)?;
    let mut manifest_url = crate::connect_rendezvous::join_url(base, "api/log/release-manifest")
        .map_err(Unavailable)?;
    if let Some(tag) = tag {
        let query: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("tag", tag)
            .finish();
        manifest_url.set_query(Some(&query));
    }
    let response = fetch_json(&client, manifest_url)
        .await
        .map_err(Unavailable)?;
    if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(Unavailable(
            "release-manifest endpoint returned an error".to_string(),
        ));
    }
    if response.get("found").and_then(|v| v.as_bool()) != Some(true) {
        // Asking for a SPECIFIC tag asserts "this release is logged" —
        // absence is the failure this mode exists to catch. Bare
        // `--releases` against a log with no release entries yet is
        // just nothing to verify.
        return Err(match tag {
            Some(tag) => verification(format!(
                "release {tag} is not committed to the transparency log"
            )),
            None => Unavailable(
                "this rendezvous logs no release manifests (older intendant-connect, or none submitted yet)"
                    .to_string(),
            ),
        });
    }

    // The tree head, inclusion, and pin consistency — the shared ritual.
    let entry = verify_logged_entry(&client, base, state_root, &response).await?;

    // The manifest self-verifies and answers what was asked for.
    let leaf = parse_release_leaf(&entry.leaf_json).map_err(verification)?;
    if let Some(tag) = tag {
        if leaf.tag != tag {
            return Err(verification(format!(
                "log returned a manifest for {} when {tag} was requested",
                leaf.tag
            )));
        }
    }

    // GitHub's view of the same release.
    let release_url = github_release_tag_url(github_api, repo, &leaf.tag).map_err(Unavailable)?;
    let release_url_display = release_url.to_string();
    let (status, release) = fetch_github_json(&client, release_url)
        .await
        .map_err(Unavailable)?;
    let release = match (status, release) {
        (200, Some(release)) => release,
        (200, None) => {
            return Err(Unavailable(format!(
                "GET {release_url_display}: response was not JSON"
            )))
        }
        (404, _) => {
            return Err(verification(format!(
                "GitHub has no release {} at {repo}, though the log commits one",
                leaf.tag
            )))
        }
        (status, _) => {
            return Err(Unavailable(format!(
                "GET {release_url_display}: HTTP {status}"
            )))
        }
    };
    let assets = parse_github_assets(&release).map_err(Unavailable)?;

    let mut mismatches = Vec::new();
    let mut digest_verified = 0usize;
    let mut presence_only = 0usize;
    let mut downloaded = 0usize;
    for artifact in &leaf.artifacts {
        match check_release_artifact(artifact, &assets) {
            Ok(AssetCheck::DigestVerified) => digest_verified += 1,
            Ok(AssetCheck::PresenceOnly) => presence_only += 1,
            Err(diff) => mismatches.push(diff),
        }
    }
    mismatches.extend(unlogged_assets(&leaf.artifacts, &assets));

    if download {
        let download_client = release_download_client().map_err(Unavailable)?;
        for artifact in &leaf.artifacts {
            let Some(asset) = assets.iter().find(|a| a.name == artifact.name) else {
                continue; // already a mismatch above
            };
            let Some(download_url) = asset.download_url.as_deref() else {
                mismatches.push(format!(
                    "{}: GitHub exposes no download URL for the asset",
                    artifact.name
                ));
                continue;
            };
            let url = Url::parse(download_url)
                .map_err(|e| Unavailable(format!("asset URL {download_url}: {e}")))?;
            match fetch_artifact(&download_client, url, RELEASE_DOWNLOAD_BYTE_CAP)
                .await
                .map_err(Unavailable)?
            {
                ArtifactFetch::Hashed { sha256_hex, .. } if sha256_hex == artifact.sha256 => {
                    downloaded += 1;
                }
                ArtifactFetch::Hashed { sha256_hex, .. } => mismatches.push(format!(
                    "{}: logged sha256 {} · downloaded {}",
                    artifact.name,
                    short_hash(&artifact.sha256),
                    short_hash(&sha256_hex),
                )),
                ArtifactFetch::HttpStatus(status) => {
                    mismatches.push(format!("{}: download HTTP {status}", artifact.name))
                }
                ArtifactFetch::TooLarge { .. } => mismatches.push(format!(
                    "{}: download exceeded {} GiB",
                    artifact.name,
                    RELEASE_DOWNLOAD_BYTE_CAP / (1024 * 1024 * 1024),
                )),
            }
        }
    }

    if !mismatches.is_empty() {
        return Err(Verification {
            summary: format!(
                "release {} diverges from the transparency log ({} finding{})",
                leaf.tag,
                mismatches.len(),
                if mismatches.len() == 1 { "" } else { "s" },
            ),
            mismatches,
        });
    }

    // Everything held — advance the pin.
    commit_verified_pin(&client, base, &entry).await?;

    Ok(ReleaseVerifyReport {
        log_size: entry.sth.size,
        manifest_index: entry.index,
        manifest_unix_ms: leaf.unix_ms,
        tag: leaf.tag,
        version: leaf.version,
        platforms: leaf.platforms,
        manifest_hash: leaf.manifest_hash,
        artifact_count: leaf.artifacts.len(),
        digest_verified,
        presence_only,
        downloaded,
        pinned_from_size: entry.pinned_from_size,
    })
}

// ── The daemon tripwire (advisory, fail-open; the CT tripwire's rhyme) ──

/// One check against the configured rendezvous. Skips quietly when the
/// Connect client is not enabled.
pub(crate) async fn check_once() {
    let _single_flight = daemon_check_lock().lock().await;
    check_once_inner().await;
}

async fn check_once_inner() {
    let status = crate::connect_rendezvous::status_snapshot();
    if !status.configured {
        note_connect_config(false, None);
        return;
    }
    let Some(base_text) = status
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(key) = verifier_url_key(Some(base_text)) else {
        return;
    };
    let Ok(base) = Url::parse(base_text) else {
        return;
    };
    with_status(|status| bind_status_to_url(status, Some(key.clone())));
    let now = now_unix_ms();
    match verify_hosted_bundle(&base, &crate::platform::intendant_home()).await {
        Ok(_) => with_status(|s| {
            update_status_for_url(s, &key, |s| {
                s.state = "ok".to_string();
                s.checked_unix_ms = Some(now);
                s.last_error = None;
                s.mismatches = Vec::new();
            });
        }),
        Err(VerifyFailure::Unavailable(error)) => with_status(|s| {
            update_status_for_url(s, &key, |s| {
                s.last_error = Some(error);
            });
        }),
        Err(VerifyFailure::Verification {
            summary,
            mismatches,
        }) => {
            let applied = with_status(|s| {
                update_status_for_url(s, &key, |s| {
                    s.state = "alert".to_string();
                    s.checked_unix_ms = Some(now);
                    s.last_error = None;
                    s.mismatches = if mismatches.is_empty() {
                        vec![summary.clone()]
                    } else {
                        mismatches.clone()
                    };
                })
            });
            if applied {
                eprintln!(
                    "[hosted-verify] HOSTED BUNDLE ALERT: {} is serving Connect code/assets that do \
                     not match its public transparency log ({summary}): {:?} — stop trusting hosted \
                     tabs against this rendezvous until the operator explains; direct and fleet-name \
                     dashboards are unaffected",
                    base, mismatches,
                );
            }
        }
    }
}

const HOSTED_BUNDLE_MONITOR_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

async fn run_hosted_bundle_monitor<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        // This verifier does not depend on daemon registration. Running
        // before the first sleep gives every daemon boot an immediate
        // out-of-band observation of its configured Connect origin.
        check().await;
        tokio::time::sleep(HOSTED_BUNDLE_MONITOR_INTERVAL).await;
    }
}

/// Check on boot, then daily. Spawned once at gateway startup, beside
/// `fleet_cert::spawn_renewal_loop`.
pub(crate) fn spawn_hosted_bundle_monitor() {
    tokio::spawn(run_hosted_bundle_monitor(check_once));
}

// ── The CLI front door: `intendant hosted-verify` ──

const CLI_HELP: &str = "\
Verify a hosted rendezvous against its public transparency log
(docs/src/self-hosted-rendezvous.md).

Usage: intendant hosted-verify [--connect <url>]
       intendant hosted-verify --releases [tag] [--download]
                               [--repo <owner/name>] [--connect <url>]

  --connect <url>   Rendezvous origin to verify (default: the
                    INTENDANT_CONNECT_RENDEZVOUS_URL environment
                    variable, then the hosted default)
  --releases [tag]  Verify app RELEASE artifacts instead of the served
                    dashboard: fetches the release manifest logged for
                    <tag> (default: the latest logged release) with its
                    inclusion proof and signed tree head, verifies the
                    tree head extends the pin, then compares the logged
                    artifact list against the GitHub release's asset
                    metadata — names, sizes, and the sha256 digests the
                    API reports. This proves the release is committed to
                    the append-only log and GitHub's metadata agrees; it
                    does NOT re-download the artifacts. With an explicit
                    <tag>, a release absent from the log FAILS (exit 1):
                    an unlogged release is what this mode exists to catch.
  --download        With --releases: additionally download every logged
                    artifact from the GitHub release and verify its
                    sha256 against the log — the strongest check, at
                    full artifact bandwidth.
  --repo <owner/name>
                    With --releases: the GitHub repository the logged
                    releases ship from (default intendant-dev/Intendant;
                    self-hosted forks override)

Without --releases: fetches the logged artifact manifest with its
inclusion proof and signed tree head, verifies the tree head extends the
one pinned under the daemon state root (~/.intendant/hosted-verify/,
honoring $INTENDANT_HOME — releases share the same tree-head pin), rejects
rollback below the highest artifact-manifest index already verified, then
downloads every listed artifact exactly as a browser would and compares hashes.
Exit codes: 0 verified · 1 divergence or proof failure · 2 usage ·
3 could not check (network / older service).";

pub(crate) async fn run_cli(args: Vec<String>) -> i32 {
    let mut connect: Option<String> = None;
    let mut releases = false;
    let mut release_tag: Option<String> = None;
    let mut download = false;
    let mut repo: Option<String> = None;
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--connect" => match iter.next() {
                Some(value) => connect = Some(value),
                None => {
                    eprintln!("error: --connect requires a URL");
                    return 2;
                }
            },
            "--releases" => {
                releases = true;
                if iter
                    .peek()
                    .map(|next| !next.starts_with('-'))
                    .unwrap_or(false)
                {
                    release_tag = iter.next();
                }
            }
            "--download" => download = true,
            "--repo" => match iter.next() {
                Some(value) => repo = Some(value),
                None => {
                    eprintln!("error: --repo requires owner/name");
                    return 2;
                }
            },
            "--help" | "-h" => {
                println!("{CLI_HELP}");
                return 0;
            }
            other => {
                eprintln!("error: unknown argument {other:?}\n\n{CLI_HELP}");
                return 2;
            }
        }
    }
    if (download || repo.is_some()) && !releases {
        eprintln!("error: --download and --repo only apply with --releases");
        return 2;
    }
    let base_raw = connect
        .or_else(|| {
            std::env::var("INTENDANT_CONNECT_RENDEZVOUS_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| crate::project::DEFAULT_CONNECT_RENDEZVOUS_URL.to_string());
    let base = match Url::parse(&base_raw) {
        Ok(url) => url,
        Err(error) => {
            eprintln!("error: invalid rendezvous URL {base_raw:?}: {error}");
            return 2;
        }
    };
    if releases {
        let repo = repo.unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string());
        let github_api = Url::parse(GITHUB_API_BASE).expect("static GitHub API base URL");
        return run_release_cli(
            &base,
            &base_raw,
            &github_api,
            &repo,
            release_tag.as_deref(),
            download,
        )
        .await;
    }
    println!("hosted-verify: {}", base_raw.trim_end_matches('/'));
    match verify_hosted_bundle(&base, &crate::platform::intendant_home()).await {
        Ok(report) => {
            println!("tree head: {} entries — signature OK", report.log_size);
            print_pin_line(report.pinned_from_size);
            println!(
                "manifest: log index {} · logged {} · bundle {} ({}) · {} artifacts · hash {}",
                report.manifest_index,
                format_logged_at(report.manifest_unix_ms),
                report.bundle_version,
                report.git_sha,
                report.artifact_count,
                short_hash(&report.manifest_hash),
            );
            println!(
                "artifacts: {}/{} match the log",
                report.artifact_count, report.artifact_count
            );
            println!("PASS — what this origin serves is what its transparency log commits to");
            0
        }
        Err(failure) => print_failure(
            failure,
            "If you did not expect a deploy just now, stop trusting hosted tabs against this \
             rendezvous and reach your daemons directly.",
        ),
    }
}

fn print_pin_line(pinned_from_size: Option<u64>) {
    match pinned_from_size {
        Some(size) => println!("pin: consistent with pinned size {size} — pin advanced"),
        None => println!("pin: first contact — this tree head is now pinned"),
    }
}

fn format_logged_at(unix_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("{unix_ms} (unix ms)"))
}

fn print_failure(failure: VerifyFailure, advice: &str) -> i32 {
    match failure {
        VerifyFailure::Verification {
            summary,
            mismatches,
        } => {
            eprintln!("FAIL — {summary}");
            for line in &mismatches {
                eprintln!("  {line}");
            }
            eprintln!("{advice}");
            1
        }
        VerifyFailure::Unavailable(error) => {
            eprintln!("could not check: {error}");
            3
        }
    }
}

async fn run_release_cli(
    base: &Url,
    base_raw: &str,
    github_api: &Url,
    repo: &str,
    tag: Option<&str>,
    download: bool,
) -> i32 {
    println!(
        "hosted-verify --releases: {} · repo {repo}{}",
        base_raw.trim_end_matches('/'),
        tag.map(|t| format!(" · tag {t}")).unwrap_or_default(),
    );
    match verify_hosted_release(
        base,
        github_api,
        repo,
        tag,
        download,
        &crate::platform::intendant_home(),
    )
    .await
    {
        Ok(report) => {
            println!("tree head: {} entries — signature OK", report.log_size);
            print_pin_line(report.pinned_from_size);
            println!(
                "release: {} (version {}) · platforms {} · log index {} · logged {} · {} artifacts · hash {}",
                report.tag,
                report.version,
                if report.platforms.is_empty() {
                    "unknown".to_string()
                } else {
                    report.platforms.join(",")
                },
                report.manifest_index,
                format_logged_at(report.manifest_unix_ms),
                report.artifact_count,
                short_hash(&report.manifest_hash),
            );
            if download {
                println!(
                    "artifacts: {}/{} downloaded and sha256-verified against the log",
                    report.downloaded, report.artifact_count
                );
                println!(
                    "PASS — this release is committed to the public transparency log and the \
                     published artifacts hash to what it commits"
                );
            } else {
                println!(
                    "artifacts: {} logged · {} sha256-verified via GitHub API digests · {} presence+size only",
                    report.artifact_count, report.digest_verified, report.presence_only
                );
                if report.presence_only > 0 {
                    println!(
                        "note: GitHub reports no digest for {} artifact(s) — rerun with --download \
                         for full hash verification",
                        report.presence_only
                    );
                }
                println!(
                    "PASS — this release is committed to the public transparency log and \
                     GitHub's release metadata matches it (artifacts not re-downloaded; use \
                     --download for the strongest check)"
                );
            }
            0
        }
        Err(failure) => print_failure(
            failure,
            "If a release you installed is not in the log, or its artifacts diverge, treat the \
             download as untrusted: verify its sha256 against the release page and the log \
             before running it.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_manifest_artifacts(bytes: &[u8]) -> Vec<ManifestArtifact> {
        REQUIRED_BUNDLE_PATHS
            .iter()
            .map(|path| ManifestArtifact {
                path: (*path).to_string(),
                sha256: sha256_hex(bytes),
            })
            .collect()
    }

    fn manifest_leaf_json(artifacts: &[ManifestArtifact]) -> String {
        serde_json::json!({
            "kind": "artifact_manifest",
            "unix_ms": 42,
            "bundle_version": "0.1.0",
            "git_sha": "abc1234",
            "manifest_hash": manifest_hash_hex(artifacts),
            "artifacts": artifacts
                .iter()
                .map(|artifact| serde_json::json!({
                    "path": artifact.path,
                    "sha256": artifact.sha256,
                }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_monitor_checks_on_boot_then_daily() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(run_hosted_bundle_monitor(move || {
            let tx = tx.clone();
            async move {
                tx.send(()).unwrap();
            }
        }));

        rx.recv().await.expect("boot check");
        tokio::time::advance(HOSTED_BUNDLE_MONITOR_INTERVAL - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the daily check must not run early"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        rx.recv().await.expect("daily check");
        handle.abort();
    }

    #[test]
    fn completed_verdict_is_bound_to_one_rendezvous_url() {
        let mut status = HostedBundleStatus {
            state: "unchecked".to_string(),
            checked_unix_ms: None,
            last_error: None,
            mismatches: Vec::new(),
            rendezvous_url: None,
        };
        let first = verifier_url_key(Some("https://first.example/")).unwrap();
        bind_status_to_url(&mut status, Some(first.clone()));
        assert!(update_status_for_url(&mut status, &first, |status| {
            status.state = "ok".to_string();
            status.checked_unix_ms = Some(7);
        }));

        let second = verifier_url_key(Some("https://second.example")).unwrap();
        bind_status_to_url(&mut status, Some(second.clone()));
        assert_eq!(status.state, "unchecked");
        assert_eq!(status.checked_unix_ms, None);
        assert!(!update_status_for_url(&mut status, &first, |status| {
            status.state = "alert".to_string();
        }));
        assert!(update_status_for_url(&mut status, &second, |status| {
            status.state = "ok".to_string();
        }));
    }

    #[tokio::test]
    async fn metadata_fetch_does_not_follow_a_cross_origin_loopback_redirect() {
        let target_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let target_hits_for_route = std::sync::Arc::clone(&target_hits);
        let target_router = axum::Router::new().route(
            "/private",
            axum::routing::get(move || {
                let target_hits = std::sync::Arc::clone(&target_hits_for_route);
                async move {
                    target_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(serde_json::json!({"secret": true}))
                }
            }),
        );
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/private", target_listener.local_addr().unwrap());
        let target_server = tokio::spawn(async move {
            axum::serve(target_listener, target_router).await.ok();
        });

        let source_router = axum::Router::new().route(
            "/metadata",
            axum::routing::get(move || {
                let target_url = target_url.clone();
                async move {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, target_url)],
                    )
                }
            }),
        );
        let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_url = Url::parse(&format!(
            "http://{}/metadata",
            source_listener.local_addr().unwrap()
        ))
        .unwrap();
        let source_server = tokio::spawn(async move {
            axum::serve(source_listener, source_router).await.ok();
        });

        let error = fetch_json(&http_client().unwrap(), source_url)
            .await
            .unwrap_err();
        assert!(error.contains("302"), "error was {error}");
        assert_eq!(target_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        source_server.abort();
        target_server.abort();
    }

    #[tokio::test]
    async fn metadata_fetch_and_manifest_structures_are_bounded() {
        let oversized = vec![b'x'; METADATA_RESPONSE_BYTE_CAP + 1];
        let router = axum::Router::new().route(
            "/metadata",
            axum::routing::get(move || {
                let oversized = oversized.clone();
                async move { oversized }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/metadata",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        assert!(fetch_json(&http_client().unwrap(), url)
            .await
            .unwrap_err()
            .contains("exceeds"));
        server.abort();

        let artifacts = (0..=MANIFEST_ARTIFACT_CAP)
            .map(|index| {
                serde_json::json!({
                    "path": format!("/artifact-{index}"),
                    "sha256": "0".repeat(64),
                })
            })
            .collect::<Vec<_>>();
        let leaf = serde_json::json!({
            "kind": "artifact_manifest",
            "artifacts": artifacts,
            "manifest_hash": "0".repeat(64),
        })
        .to_string();
        assert!(parse_manifest_leaf(&leaf)
            .unwrap_err()
            .contains("too many artifacts"));

        let overlong_path = format!("/{}", "a".repeat(ARTIFACT_PATH_BYTE_CAP));
        let leaf = serde_json::json!({
            "kind": "artifact_manifest",
            "artifacts": [{"path": overlong_path, "sha256": "0".repeat(64)}],
            "manifest_hash": "0".repeat(64),
        })
        .to_string();
        assert!(parse_manifest_leaf(&leaf)
            .unwrap_err()
            .contains("string bounds"));
    }

    // ── Local producers (RFC 6962 §2.1) so the replicated verifiers are
    // exercised against real trees, mirroring the service's tests. ──

    fn split_point(n: usize) -> usize {
        let mut k = 1usize;
        while k * 2 < n {
            k *= 2;
        }
        k
    }

    fn tree_root(leaves: &[[u8; 32]]) -> [u8; 32] {
        match leaves.len() {
            0 => sha256(b""),
            1 => leaves[0],
            n => {
                let k = split_point(n);
                node_hash(&tree_root(&leaves[..k]), &tree_root(&leaves[k..]))
            }
        }
    }

    fn inclusion_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
        let n = leaves.len();
        if n <= 1 {
            return Vec::new();
        }
        let k = split_point(n);
        if m < k {
            let mut path = inclusion_proof(m, &leaves[..k]);
            path.push(tree_root(&leaves[k..]));
            path
        } else {
            let mut path = inclusion_proof(m - k, &leaves[k..]);
            path.push(tree_root(&leaves[..k]));
            path
        }
    }

    fn consistency_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
        fn subproof(m: usize, leaves: &[[u8; 32]], complete: bool) -> Vec<[u8; 32]> {
            let n = leaves.len();
            if m == n {
                return if complete {
                    Vec::new()
                } else {
                    vec![tree_root(leaves)]
                };
            }
            let k = split_point(n);
            if m <= k {
                let mut proof = subproof(m, &leaves[..k], complete);
                proof.push(tree_root(&leaves[k..]));
                proof
            } else {
                let mut proof = subproof(m - k, &leaves[k..], false);
                proof.push(tree_root(&leaves[..k]));
                proof
            }
        }
        if m == 0 || m > leaves.len() {
            return Vec::new();
        }
        subproof(m, leaves, true)
    }

    #[test]
    fn merkle_verifiers_round_trip_all_shapes() {
        // The CT empty-tree vector anchors the hashing.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let leaves: Vec<[u8; 32]> = (0u8..8).map(|i| leaf_hash(&format!("entry-{i}"))).collect();
        for size in 1..=leaves.len() {
            let tree = &leaves[..size];
            let root = tree_root(tree);
            for index in 0..size {
                let proof = inclusion_proof(index, tree);
                assert!(verify_inclusion(&tree[index], index, size, &proof, &root));
                assert!(!verify_inclusion(
                    &leaf_hash("evil"),
                    index,
                    size,
                    &proof,
                    &root
                ));
            }
            for old in 1..=size {
                let proof = consistency_proof(old, tree);
                let old_root = tree_root(&leaves[..old]);
                assert!(verify_consistency(old, size, &old_root, &root, &proof));
                if old < size {
                    assert!(!verify_consistency(
                        old,
                        size,
                        &leaf_hash("rewritten"),
                        &root,
                        &proof
                    ));
                }
            }
        }
    }

    /// Golden twins of `bin/connect/transparency.rs` — the STH payload
    /// and the canonical manifest hash must match the service
    /// byte-for-byte (change both together).
    #[test]
    fn replicated_formats_twin_the_service() {
        assert_eq!(
            sth_payload(3, "rootB64u", 123),
            "intendant-log-sth-v1\n3\nrootB64u\n123"
        );
        let artifacts = vec![
            ManifestArtifact {
                path: "/app.html".to_string(),
                sha256: sha256_hex(b"hello"),
            },
            ManifestArtifact {
                path: "/wasm-web/presence_web.js".to_string(),
                sha256: sha256_hex(b"world"),
            },
        ];
        // Pinned in transparency.rs::manifest_hash_is_canonical_and_pinned.
        assert_eq!(
            manifest_hash_hex(&artifacts),
            "d77d51c09215be374511f6763f0c50d6c84726b8ff82d3ac958e1fc5fcf7abf6"
        );
    }

    #[test]
    fn sth_signature_verifies_and_rejects_tampering() {
        use ring::signature::KeyPair as _;
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let keypair = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            document.as_ref(),
            &rng,
        )
        .unwrap();
        let root = [7u8; 32];
        let root_b64u = crate::daemon_identity::b64u(&root);
        let payload = sth_payload(5, &root_b64u, 999);
        let signature = keypair.sign(&rng, payload.as_bytes()).unwrap();
        let mut sth = Sth {
            size: 5,
            root,
            root_b64u: root_b64u.clone(),
            unix_ms: 999,
            signature: signature.as_ref().to_vec(),
            public_key: keypair.public_key().as_ref().to_vec(),
            public_key_b64u: crate::daemon_identity::b64u(keypair.public_key().as_ref()),
        };
        assert!(sth.verify_signature().is_ok());
        sth.size = 6; // any field change breaks the signature
        assert!(sth.verify_signature().is_err());
    }

    #[test]
    fn manifest_leaf_parses_and_self_verifies() {
        let artifacts = required_manifest_artifacts(b"bundle");
        let good = manifest_leaf_json(&artifacts);
        let leaf = parse_manifest_leaf(&good).unwrap();
        assert_eq!(leaf.artifacts.len(), REQUIRED_BUNDLE_PATHS.len());
        assert_eq!(leaf.bundle_version, "0.1.0");
        assert_eq!(leaf.git_sha, "abc1234");
        assert_eq!(leaf.unix_ms, 42);

        // A tampered list no longer matches its own manifest_hash.
        let tampered = good.replace(&sha256_hex(b"bundle"), &sha256_hex(b"evil"));
        assert!(parse_manifest_leaf(&tampered).is_err());
        // Wrong kind is rejected.
        assert!(parse_manifest_leaf(&good.replace("artifact_manifest", "daemon_claimed")).is_err());
        // Relative paths are rejected.
        let relative = serde_json::json!({
            "kind": "artifact_manifest",
            "manifest_hash": "x",
            "artifacts": [{ "path": "app.html", "sha256": "aa" }],
        })
        .to_string();
        assert!(parse_manifest_leaf(&relative).is_err());

        let empty = manifest_leaf_json(&[]);
        assert!(parse_manifest_leaf(&empty)
            .unwrap_err()
            .contains("omits required served paths"));

        let subset = manifest_leaf_json(&required_manifest_artifacts(b"bundle")[..1]);
        assert!(parse_manifest_leaf(&subset)
            .unwrap_err()
            .contains("/connect"));

        let mut duplicate = required_manifest_artifacts(b"bundle");
        duplicate.insert(1, duplicate[0].clone());
        assert!(parse_manifest_leaf(&manifest_leaf_json(&duplicate))
            .unwrap_err()
            .contains("not unique"));
    }

    /// The golden mismatch case: a fabricated manifest against fabricated
    /// served bytes — one divergent artifact must produce exactly one
    /// precise diff line, and matching ones none.
    #[test]
    fn artifact_comparison_flags_divergence_precisely() {
        let expected = ManifestArtifact {
            path: "/app.html".to_string(),
            sha256: sha256_hex(b"the logged bundle"),
        };
        assert_eq!(
            artifact_mismatch(
                &expected,
                &ArtifactFetch::Hashed {
                    sha256_hex: sha256_hex(b"the logged bundle"),
                    bytes: b"the logged bundle".len(),
                }
            ),
            None
        );
        let diff = artifact_mismatch(
            &expected,
            &ArtifactFetch::Hashed {
                sha256_hex: sha256_hex(b"a different bundle"),
                bytes: b"a different bundle".len(),
            },
        )
        .unwrap();
        assert!(diff.starts_with("/app.html: manifest "), "diff was {diff}");
        assert!(diff.contains(&short_hash(&expected.sha256)));
        assert!(diff.contains(&short_hash(&sha256_hex(b"a different bundle"))));
        assert_eq!(
            artifact_mismatch(&expected, &ArtifactFetch::HttpStatus(404)).unwrap(),
            "/app.html: HTTP 404"
        );
        assert!(
            artifact_mismatch(&expected, &ArtifactFetch::TooLarge { bytes: 17 })
                .unwrap()
                .contains("exceeded")
        );
    }

    async fn spawn_artifact_budget_server() -> (Url, tokio::task::JoinHandle<()>) {
        let router = axum::Router::new()
            .route(
                "/one",
                axum::routing::get(|| async { axum::body::Bytes::from_static(b"abc") }),
            )
            .route(
                "/two",
                axum::routing::get(|| async { axum::body::Bytes::from_static(b"def") }),
            )
            .route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    axum::body::Bytes::from_static(b"x")
                }),
            )
            .route(
                "/stream",
                axum::routing::get(|| async {
                    let stream = futures_util::stream::unfold(0usize, |index| async move {
                        if index == 6 {
                            return None;
                        }
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        Some((
                            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"x")),
                            index + 1,
                        ))
                    });
                    axum::body::Body::from_stream(stream)
                }),
            )
            .route(
                "/oversized",
                axum::routing::get(|| async {
                    let stream = futures_util::stream::once(async {
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"abcdef"))
                    });
                    axum::body::Body::from_stream(stream)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), server)
    }

    #[tokio::test]
    async fn bundle_fetch_enforces_aggregate_byte_and_time_budgets() {
        let (base, server) = spawn_artifact_budget_server().await;
        let client = http_client().unwrap();
        let artifacts = [
            ManifestArtifact {
                path: "/one".to_string(),
                sha256: sha256_hex(b"abc"),
            },
            ManifestArtifact {
                path: "/two".to_string(),
                sha256: sha256_hex(b"def"),
            },
        ];
        let limits = BundleFetchLimits {
            per_artifact_bytes: 64,
            total_bytes: 5,
            total_timeout: Duration::from_secs(1),
            response_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
        };
        match compare_live_artifacts(&client, &base, &artifacts, limits).await {
            Err(VerifyFailure::Unavailable(error)) => {
                assert!(error.contains("aggregate byte budget"), "{error}");
            }
            other => panic!("aggregate byte budget must stop the fetch, got {other:?}"),
        }

        let oversized = [
            ManifestArtifact {
                path: "/oversized".to_string(),
                sha256: sha256_hex(b"abcdef"),
            },
            ManifestArtifact {
                path: "/oversized".to_string(),
                sha256: sha256_hex(b"abcdef"),
            },
        ];
        let limits = BundleFetchLimits {
            per_artifact_bytes: 4,
            total_bytes: 8,
            total_timeout: Duration::from_secs(1),
            response_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
        };
        match compare_live_artifacts(&client, &base, &oversized, limits).await {
            Err(VerifyFailure::Unavailable(error)) => {
                assert!(error.contains("aggregate byte budget"), "{error}");
            }
            other => {
                panic!("oversized streams must count against the aggregate budget, got {other:?}")
            }
        }

        let slow = [ManifestArtifact {
            path: "/slow".to_string(),
            sha256: sha256_hex(b"x"),
        }];
        let limits = BundleFetchLimits {
            total_bytes: 64,
            total_timeout: Duration::from_millis(10),
            ..limits
        };
        match compare_live_artifacts(&client, &base, &slow, limits).await {
            Err(VerifyFailure::Unavailable(error)) => {
                assert!(error.contains("aggregate time budget"), "{error}");
            }
            other => panic!("aggregate time budget must stop the fetch, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn release_download_accepts_a_progressing_stream_without_a_short_total_timeout() {
        let (base, server) = spawn_artifact_budget_server().await;
        let client = release_download_client().unwrap();
        let url = base.join("stream").unwrap();
        let fetched = fetch_artifact_with_timeouts(
            &client,
            url,
            64,
            Duration::from_secs(1),
            Duration::from_millis(40),
        )
        .await
        .unwrap();
        match fetched {
            ArtifactFetch::Hashed {
                sha256_hex: digest,
                bytes,
            } => {
                assert_eq!(bytes, 6);
                assert_eq!(digest, sha256_hex(b"xxxxxx"));
            }
            other => panic!("progressing stream must hash successfully, got {other:?}"),
        }
        server.abort();
    }

    #[test]
    fn pin_decisions_enforce_append_only_history() {
        let sth = |size: u64, root: [u8; 32], key: &str| Sth {
            size,
            root,
            root_b64u: crate::daemon_identity::b64u(&root),
            unix_ms: 1,
            signature: Vec::new(),
            public_key: Vec::new(),
            public_key_b64u: key.to_string(),
        };
        let root_a = [1u8; 32];
        let pin = SthPin {
            size: 4,
            root: crate::daemon_identity::b64u(&root_a),
            public_key: "key1".to_string(),
            pinned_unix_ms: 10,
        };
        // No pin: first contact.
        assert_eq!(
            pin_decision(None, &sth(4, root_a, "key1")).unwrap(),
            PinDecision::FirstContact
        );
        // Same size, same root: nothing to fetch.
        assert_eq!(
            pin_decision(Some(&pin), &sth(4, root_a, "key1")).unwrap(),
            PinDecision::Unchanged
        );
        // Growth: consistency demanded from the pinned root.
        assert_eq!(
            pin_decision(Some(&pin), &sth(7, [2u8; 32], "key1")).unwrap(),
            PinDecision::NeedConsistency {
                old_size: 4,
                old_root: root_a
            }
        );
        // Shrink, root swap at same size, and key change all fail hard.
        assert!(pin_decision(Some(&pin), &sth(3, root_a, "key1")).is_err());
        assert!(pin_decision(Some(&pin), &sth(4, [9u8; 32], "key1")).is_err());
        assert!(pin_decision(Some(&pin), &sth(7, [2u8; 32], "key2")).is_err());
    }

    /// Golden twin of `bin/connect/transparency.rs`'s release-manifest
    /// canonicalization (pinned there in
    /// `release_manifest_hash_is_canonical_and_pinned`; change both
    /// together).
    #[test]
    fn replicated_release_hash_twins_the_service() {
        let artifacts = vec![
            ReleaseArtifact {
                name: "Intendant-v1.2.3.zip".to_string(),
                sha256: sha256_hex(b"hello"),
                size: 5,
            },
            ReleaseArtifact {
                name: "Intendant-v1.2.3.zip.sha256".to_string(),
                sha256: sha256_hex(b"world"),
                size: 99,
            },
        ];
        assert_eq!(
            release_manifest_hash_hex("v1.2.3", &artifacts),
            "050b3579a283790ed739544295c4120ab5457a557fefc72ed374847e8af83030"
        );
    }

    fn release_leaf_fixture(tag: &str, artifacts: &[ReleaseArtifact]) -> String {
        serde_json::json!({
            "kind": "release_manifest",
            "unix_ms": 4242,
            "tag": tag,
            "version": tag.trim_start_matches('v'),
            "platforms": ["macos-arm64"],
            "manifest_hash": release_manifest_hash_hex(tag, artifacts),
            "artifacts": artifacts
                .iter()
                .map(|a| serde_json::json!({ "name": a.name, "sha256": a.sha256, "size": a.size }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    #[test]
    fn release_leaf_parses_and_self_verifies() {
        let artifacts = vec![ReleaseArtifact {
            name: "Intendant-v1.2.3.zip".to_string(),
            sha256: sha256_hex(b"app zip"),
            size: 7,
        }];
        let good = release_leaf_fixture("v1.2.3", &artifacts);
        let leaf = parse_release_leaf(&good).unwrap();
        assert_eq!(leaf.tag, "v1.2.3");
        assert_eq!(leaf.version, "1.2.3");
        assert_eq!(leaf.platforms, vec!["macos-arm64".to_string()]);
        assert_eq!(leaf.unix_ms, 4242);
        assert_eq!(leaf.artifacts, artifacts);

        // A tampered artifact hash no longer matches the manifest_hash.
        let tampered = good.replace(&sha256_hex(b"app zip"), &sha256_hex(b"evil zip"));
        assert!(parse_release_leaf(&tampered).is_err());
        // Wrong kind is rejected.
        assert!(
            parse_release_leaf(&good.replace("release_manifest", "artifact_manifest")).is_err()
        );
        // No artifacts is rejected.
        let empty = serde_json::json!({
            "kind": "release_manifest",
            "tag": "v1.2.3",
            "manifest_hash": release_manifest_hash_hex("v1.2.3", &[]),
            "artifacts": [],
        })
        .to_string();
        assert!(parse_release_leaf(&empty).is_err());
    }

    #[test]
    fn release_asset_metadata_comparison_flags_divergence() {
        let logged = ReleaseArtifact {
            name: "Intendant-v1.2.3.zip".to_string(),
            sha256: sha256_hex(b"the released zip"),
            size: 16,
        };
        let asset = |size: u64, digest: Option<String>| GithubAsset {
            name: "Intendant-v1.2.3.zip".to_string(),
            size,
            sha256: digest,
            download_url: None,
        };
        // Digest present and matching: fully verified from metadata.
        assert_eq!(
            check_release_artifact(&logged, &[asset(16, Some(sha256_hex(b"the released zip")))]),
            Ok(AssetCheck::DigestVerified)
        );
        // No digest: presence + size only.
        assert_eq!(
            check_release_artifact(&logged, &[asset(16, None)]),
            Ok(AssetCheck::PresenceOnly)
        );
        // Missing from the release.
        let diff = check_release_artifact(&logged, &[]).unwrap_err();
        assert!(
            diff.contains("not on the GitHub release"),
            "diff was {diff}"
        );
        // Size divergence.
        let diff = check_release_artifact(&logged, &[asset(17, None)]).unwrap_err();
        assert!(diff.contains("logged 16 bytes"), "diff was {diff}");
        // Digest divergence.
        let diff =
            check_release_artifact(&logged, &[asset(16, Some(sha256_hex(b"a swapped zip")))])
                .unwrap_err();
        assert!(diff.contains("logged sha256"), "diff was {diff}");
        // An asset the log never blessed is loud.
        let extra = GithubAsset {
            name: "extra-payload.zip".to_string(),
            size: 3,
            sha256: None,
            download_url: None,
        };
        let lines = unlogged_assets(&[logged.clone()], &[asset(16, None), extra]);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("extra-payload.zip"),
            "line was {}",
            lines[0]
        );
        assert!(unlogged_assets(&[logged], &[asset(16, None)]).is_empty());
    }

    // ── Hermetic full-flow fixtures: a real Merkle log + signed tree
    // head served from 127.0.0.1, twinned GitHub API included, so the
    // release flow runs end to end with injected base URLs and no
    // external network. ──

    struct FixtureLog {
        leaves_json: Vec<String>,
        keypair: ring::signature::EcdsaKeyPair,
        rng: ring::rand::SystemRandom,
    }

    impl FixtureLog {
        fn new(leaves_json: Vec<String>) -> Self {
            let rng = ring::rand::SystemRandom::new();
            let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
                &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &rng,
            )
            .unwrap();
            let keypair = ring::signature::EcdsaKeyPair::from_pkcs8(
                &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                document.as_ref(),
                &rng,
            )
            .unwrap();
            Self {
                leaves_json,
                keypair,
                rng,
            }
        }

        fn leaves(&self) -> Vec<[u8; 32]> {
            self.leaves_json
                .iter()
                .map(|leaf| leaf_hash(leaf))
                .collect()
        }

        fn sth_json(&self) -> serde_json::Value {
            use ring::signature::KeyPair as _;
            let leaves = self.leaves();
            let root_b64u = crate::daemon_identity::b64u(&tree_root(&leaves));
            let unix_ms = 1_700_000_000_000u64;
            let payload = sth_payload(leaves.len() as u64, &root_b64u, unix_ms);
            let signature = self.keypair.sign(&self.rng, payload.as_bytes()).unwrap();
            serde_json::json!({
                "size": leaves.len(),
                "root": root_b64u,
                "unix_ms": unix_ms,
                "signature": crate::daemon_identity::b64u(signature.as_ref()),
                "public_key": crate::daemon_identity::b64u(self.keypair.public_key().as_ref()),
            })
        }

        fn manifest_response(&self, index: usize) -> serde_json::Value {
            let leaves = self.leaves();
            let proof: Vec<String> = inclusion_proof(index, &leaves)
                .iter()
                .map(|hash| crate::daemon_identity::b64u(hash))
                .collect();
            serde_json::json!({
                "ok": true,
                "found": true,
                "index": index,
                "kind": "release_manifest",
                "unix_ms": 4242,
                "leaf_json": self.leaves_json[index],
                "proof": proof,
                "sth": self.sth_json(),
            })
        }

        fn consistency_response(&self, old: usize) -> serde_json::Value {
            let leaves = self.leaves();
            let proof: Vec<String> = consistency_proof(old, &leaves)
                .iter()
                .map(|hash| crate::daemon_identity::b64u(hash))
                .collect();
            serde_json::json!({ "ok": true, "proof": proof })
        }
    }

    struct Fixture {
        log: FixtureLog,
        manifest_index: usize,
        release_status: u16,
        release_body: serde_json::Value,
        downloads: std::collections::HashMap<String, Vec<u8>>,
    }

    /// Serve the fixture over loopback; both the "rendezvous" and the
    /// "GitHub API" live on the same ephemeral listener.
    async fn spawn_fixture_server(
        fixture: std::sync::Arc<std::sync::Mutex<Fixture>>,
    ) -> (Url, tokio::task::JoinHandle<()>) {
        use axum::extract::{Path as AxumPath, Query as AxumQuery};
        let manifest_fixture = fixture.clone();
        let consistency_fixture = fixture.clone();
        let release_fixture = fixture.clone();
        let download_fixture = fixture.clone();
        let router = axum::Router::new()
            .route(
                "/api/log/release-manifest",
                axum::routing::get(move || {
                    let fixture = manifest_fixture.clone();
                    async move {
                        let fixture = fixture.lock().unwrap();
                        axum::Json(fixture.log.manifest_response(fixture.manifest_index))
                    }
                }),
            )
            .route(
                "/api/log/consistency",
                axum::routing::get(
                    move |AxumQuery(params): AxumQuery<
                        std::collections::HashMap<String, String>,
                    >| {
                        let fixture = consistency_fixture.clone();
                        async move {
                            let old: usize =
                                params.get("old").and_then(|v| v.parse().ok()).unwrap_or(0);
                            let fixture = fixture.lock().unwrap();
                            axum::Json(fixture.log.consistency_response(old))
                        }
                    },
                ),
            )
            .route(
                "/repos/{owner}/{repo}/releases/tags/{tag}",
                axum::routing::get(move || {
                    let fixture = release_fixture.clone();
                    async move {
                        let fixture = fixture.lock().unwrap();
                        (
                            axum::http::StatusCode::from_u16(fixture.release_status).unwrap(),
                            axum::Json(fixture.release_body.clone()),
                        )
                    }
                }),
            )
            .route(
                "/dl/{name}",
                axum::routing::get(move |AxumPath(name): AxumPath<String>| {
                    let fixture = download_fixture.clone();
                    async move {
                        let fixture = fixture.lock().unwrap();
                        fixture.downloads.get(&name).cloned().unwrap_or_default()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        (url, handle)
    }

    fn release_json(assets: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "tag_name": "v1.2.3", "assets": assets })
    }

    #[test]
    fn github_release_tag_is_one_percent_encoded_path_segment() {
        let github_api = Url::parse("https://api.github.test/v3/").unwrap();
        let url = github_release_tag_url(&github_api, "owner/repo", "release/1.2").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.github.test/v3/repos/owner/repo/releases/tags/release%2F1.2"
        );
    }

    #[tokio::test]
    async fn release_flow_verifies_and_advances_pin_end_to_end() {
        let bytes = b"app bytes v1".to_vec();
        let artifact = ReleaseArtifact {
            name: "Intendant-v1.2.3.zip".to_string(),
            sha256: sha256_hex(&bytes),
            size: bytes.len() as u64,
        };
        let leaves = vec![
            serde_json::json!({ "kind": "daemon_claimed", "daemon_id": "d1" }).to_string(),
            release_leaf_fixture("v1.2.3", &[artifact.clone()]),
        ];
        let fixture = std::sync::Arc::new(std::sync::Mutex::new(Fixture {
            log: FixtureLog::new(leaves),
            manifest_index: 1,
            release_status: 200,
            release_body: serde_json::Value::Null,
            downloads: std::collections::HashMap::new(),
        }));
        let (base, server) = spawn_fixture_server(fixture.clone()).await;
        fixture.lock().unwrap().release_body = release_json(vec![serde_json::json!({
            "name": artifact.name,
            "size": artifact.size,
            "digest": format!("sha256:{}", artifact.sha256),
            "browser_download_url": format!("{base}dl/{}", artifact.name),
        })]);
        let state_root = tempfile::tempdir().unwrap();

        // First contact: full metadata verification, pin created.
        let report = verify_hosted_release(
            &base,
            &base,
            "test/repo",
            Some("v1.2.3"),
            false,
            state_root.path(),
        )
        .await
        .expect("first release verification passes");
        assert_eq!(report.log_size, 2);
        assert_eq!(report.manifest_index, 1);
        assert_eq!(report.tag, "v1.2.3");
        assert_eq!(report.version, "1.2.3");
        assert_eq!(report.platforms, vec!["macos-arm64".to_string()]);
        assert_eq!(report.artifact_count, 1);
        assert_eq!(report.digest_verified, 1);
        assert_eq!(report.presence_only, 0);
        assert_eq!(report.downloaded, 0);
        assert_eq!(report.pinned_from_size, None);
        let pin = load_pin(&pin_path(state_root.path(), &base))
            .expect("pin readable")
            .expect("pin created");
        assert_eq!(pin.size, 2);

        // The log grows: the rerun must fetch and verify consistency
        // from the pinned head, then advance the pin.
        fixture
            .lock()
            .unwrap()
            .log
            .leaves_json
            .push(serde_json::json!({ "kind": "daemon_claimed", "daemon_id": "d2" }).to_string());
        let report = verify_hosted_release(
            &base,
            &base,
            "test/repo",
            Some("v1.2.3"),
            false,
            state_root.path(),
        )
        .await
        .expect("grown-log verification passes via consistency proof");
        assert_eq!(report.log_size, 3);
        assert_eq!(report.pinned_from_size, Some(2));
        assert_eq!(
            load_pin(&pin_path(state_root.path(), &base))
                .unwrap()
                .unwrap()
                .size,
            3
        );

        // History rewritten (log shrank below the pin): loud failure.
        fixture.lock().unwrap().log.leaves_json.truncate(2);
        match verify_hosted_release(
            &base,
            &base,
            "test/repo",
            Some("v1.2.3"),
            false,
            state_root.path(),
        )
        .await
        {
            Err(VerifyFailure::Verification { summary, .. }) => {
                assert!(summary.contains("shrank"), "summary was {summary}")
            }
            other => panic!("shrunken log must fail verification, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn release_flow_flags_divergence_unlogged_assets_and_missing_release() {
        let artifact = ReleaseArtifact {
            name: "Intendant-v1.2.3.zip".to_string(),
            sha256: sha256_hex(b"the logged zip"),
            size: 14,
        };
        let leaves = vec![release_leaf_fixture("v1.2.3", &[artifact.clone()])];
        let fixture = std::sync::Arc::new(std::sync::Mutex::new(Fixture {
            log: FixtureLog::new(leaves),
            manifest_index: 0,
            release_status: 200,
            release_body: serde_json::Value::Null,
            downloads: std::collections::HashMap::new(),
        }));
        let (base, server) = spawn_fixture_server(fixture.clone()).await;
        // GitHub reports a different digest AND an asset the log never
        // blessed.
        fixture.lock().unwrap().release_body = release_json(vec![
            serde_json::json!({
                "name": artifact.name,
                "size": artifact.size,
                "digest": format!("sha256:{}", sha256_hex(b"a swapped zip")),
            }),
            serde_json::json!({ "name": "extra-payload.zip", "size": 3 }),
        ]);
        let state_root = tempfile::tempdir().unwrap();
        match verify_hosted_release(&base, &base, "test/repo", None, false, state_root.path()).await
        {
            Err(VerifyFailure::Verification {
                summary,
                mismatches,
            }) => {
                assert!(summary.contains("v1.2.3"), "summary was {summary}");
                assert_eq!(mismatches.len(), 2, "mismatches were {mismatches:?}");
                assert!(mismatches[0].contains("logged sha256"));
                assert!(mismatches[1].contains("extra-payload.zip"));
            }
            other => panic!("divergence must fail verification, got {other:?}"),
        }
        // A verification failure must NOT advance (or create) the pin.
        assert!(load_pin(&pin_path(state_root.path(), &base))
            .unwrap()
            .is_none());

        // GitHub not having the release at all is a verdict, not an
        // availability problem: the log commits something GitHub denies.
        fixture.lock().unwrap().release_status = 404;
        match verify_hosted_release(&base, &base, "test/repo", None, false, state_root.path()).await
        {
            Err(VerifyFailure::Verification { summary, .. }) => {
                assert!(summary.contains("no release"), "summary was {summary}")
            }
            other => panic!("missing GitHub release must fail verification, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn release_flow_download_mode_hashes_artifacts() {
        let bytes = b"real artifact bytes".to_vec();
        let artifact = ReleaseArtifact {
            name: "Intendant-v1.2.3.zip".to_string(),
            sha256: sha256_hex(&bytes),
            size: bytes.len() as u64,
        };
        let leaves = vec![release_leaf_fixture("v1.2.3", &[artifact.clone()])];
        let fixture = std::sync::Arc::new(std::sync::Mutex::new(Fixture {
            log: FixtureLog::new(leaves),
            manifest_index: 0,
            release_status: 200,
            release_body: serde_json::Value::Null,
            downloads: std::collections::HashMap::from([(artifact.name.clone(), bytes.clone())]),
        }));
        let (base, server) = spawn_fixture_server(fixture.clone()).await;
        // No digest from the API: the default path can only prove
        // presence+size; --download proves the bytes.
        fixture.lock().unwrap().release_body = release_json(vec![serde_json::json!({
            "name": artifact.name,
            "size": artifact.size,
            "browser_download_url": format!("{base}dl/{}", artifact.name),
        })]);
        let state_root = tempfile::tempdir().unwrap();

        let report =
            verify_hosted_release(&base, &base, "test/repo", None, false, state_root.path())
                .await
                .expect("metadata-only run passes as presence-only");
        assert_eq!(report.presence_only, 1);
        assert_eq!(report.digest_verified, 0);
        assert_eq!(report.downloaded, 0);

        let report =
            verify_hosted_release(&base, &base, "test/repo", None, true, state_root.path())
                .await
                .expect("download run hash-verifies the artifact");
        assert_eq!(report.downloaded, 1);

        // Served bytes that hash differently from the log are caught.
        fixture
            .lock()
            .unwrap()
            .downloads
            .insert(artifact.name.clone(), b"tampered artifact bytes".to_vec());
        match verify_hosted_release(&base, &base, "test/repo", None, true, state_root.path()).await
        {
            Err(VerifyFailure::Verification { mismatches, .. }) => {
                assert_eq!(mismatches.len(), 1);
                assert!(
                    mismatches[0].contains("downloaded"),
                    "was {}",
                    mismatches[0]
                );
            }
            other => panic!("tampered download must fail verification, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn release_flow_absence_is_loud_only_for_explicit_tags() {
        let router = axum::Router::new().route(
            "/api/log/release-manifest",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "ok": true, "found": false }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let state_root = tempfile::tempdir().unwrap();
        // An explicit tag missing from the log is the alarm this mode
        // exists for…
        match verify_hosted_release(
            &base,
            &base,
            "test/repo",
            Some("v9.9.9"),
            false,
            state_root.path(),
        )
        .await
        {
            Err(VerifyFailure::Verification { summary, .. }) => {
                assert!(summary.contains("not committed"), "summary was {summary}");
            }
            other => panic!("explicit missing tag must fail verification, got {other:?}"),
        }
        // …while a log with no release entries at all is just nothing to
        // verify (older service).
        match verify_hosted_release(&base, &base, "test/repo", None, false, state_root.path()).await
        {
            Err(VerifyFailure::Unavailable(error)) => {
                assert!(error.contains("no release manifests"), "error was {error}")
            }
            other => panic!("bare --releases on an empty log is unavailable, got {other:?}"),
        }
        server.abort();
    }

    #[test]
    fn pins_round_trip_per_host_under_the_state_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = Url::parse("https://connect.example.test:8443/sub/").unwrap();
        let path = pin_path(dir.path(), &base);
        assert!(path.starts_with(dir.path().join("hosted-verify")));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("connect.example.test_8443.json")
        );
        assert!(load_pin(&path).unwrap().is_none());
        let pin = SthPin {
            size: 12,
            root: "cm9vdA".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 77,
        };
        commit_pin(&path, None, &pin).unwrap();
        let loaded = load_pin(&path).unwrap().unwrap();
        assert_eq!(loaded.size, 12);
        assert_eq!(loaded.root, "cm9vdA");
        assert_eq!(loaded.public_key, "a2V5");
        assert_eq!(loaded.pinned_unix_ms, 77);
        // Distinct hosts pin separately.
        let other = pin_path(
            dir.path(),
            &Url::parse("https://other.example.test").unwrap(),
        );
        assert_ne!(path, other);
    }

    #[test]
    fn artifact_manifest_high_water_rejects_rollback_and_same_index_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = Url::parse("https://connect.example.test").unwrap();
        let path = artifact_manifest_pin_path(dir.path(), &base);
        commit_artifact_manifest_pin(&path, 7, "newer-hash").unwrap();
        commit_artifact_manifest_pin(&path, 7, "newer-hash").unwrap();

        let rollback = commit_artifact_manifest_pin(&path, 3, "older-hash").unwrap_err();
        assert!(rollback.contains("regressed"), "error was {rollback}");
        let replacement = commit_artifact_manifest_pin(&path, 7, "different-hash").unwrap_err();
        assert!(
            replacement.contains("changed at pinned index"),
            "error was {replacement}"
        );

        commit_artifact_manifest_pin(&path, 9, "latest-hash").unwrap();
        let pin = load_artifact_manifest_pin(&path).unwrap().unwrap();
        assert_eq!(pin.index, 9);
        assert_eq!(pin.manifest_hash, "latest-hash");
    }

    #[test]
    fn malformed_pin_never_becomes_first_contact() {
        let dir = tempfile::tempdir().unwrap();
        let path = pin_path(
            dir.path(),
            &Url::parse("https://connect.example.test").unwrap(),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();
        assert!(load_pin(&path).unwrap_err().contains("parse"));

        let candidate = SthPin {
            size: 1,
            root: "cm9vdA".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 77,
        };
        assert!(commit_pin(&path, None, &candidate)
            .unwrap_err()
            .contains("parse"));
    }

    #[test]
    fn stale_pin_commit_cannot_regress_a_newer_observation() {
        let dir = tempfile::tempdir().unwrap();
        let path = pin_path(
            dir.path(),
            &Url::parse("https://connect.example.test").unwrap(),
        );
        let old = SthPin {
            size: 1,
            root: "b2xk".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 77,
        };
        commit_pin(&path, None, &old).unwrap();
        let newer = SthPin {
            size: 3,
            root: "bmV3ZXI".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 99,
        };
        commit_pin(&path, Some(&old), &newer).unwrap();

        let stale_candidate = SthPin {
            size: 2,
            root: "c3RhbGU".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 77,
        };
        let PinCommit::Changed(Some(observed)) =
            commit_pin(&path, Some(&old), &stale_candidate).unwrap()
        else {
            panic!("stale commit must return the current pin");
        };
        assert!(same_tree_head(&observed, &newer));
        assert_eq!(observed.pinned_unix_ms, old.pinned_unix_ms);
        let committed = load_pin(&path).unwrap().unwrap();
        assert!(same_tree_head(&committed, &newer));
        assert_eq!(committed.pinned_unix_ms, old.pinned_unix_ms);
    }

    #[tokio::test]
    async fn concurrent_pin_growth_is_reconciled_immediately_in_both_directions() {
        use ring::signature::KeyPair as _;

        let fixture = std::sync::Arc::new(std::sync::Mutex::new(Fixture {
            log: FixtureLog::new(vec![
                serde_json::json!({ "kind": "one" }).to_string(),
                serde_json::json!({ "kind": "two" }).to_string(),
                serde_json::json!({ "kind": "three" }).to_string(),
            ]),
            manifest_index: 0,
            release_status: 404,
            release_body: serde_json::Value::Null,
            downloads: std::collections::HashMap::new(),
        }));
        let (base, server) = spawn_fixture_server(fixture.clone()).await;
        let (smaller, larger, sth) = {
            let fixture = fixture.lock().unwrap();
            let leaves = fixture.log.leaves();
            let public_key =
                crate::daemon_identity::b64u(fixture.log.keypair.public_key().as_ref());
            let pin = |size: usize| SthPin {
                size: size as u64,
                root: crate::daemon_identity::b64u(&tree_root(&leaves[..size])),
                public_key: public_key.clone(),
                pinned_unix_ms: 1,
            };
            (pin(2), pin(3), Sth::parse(&fixture.log.sth_json()).unwrap())
        };
        let client = http_client().unwrap();

        // A size-2 verifier finishing after another process pinned size 3
        // proves 2 -> 3 and keeps the newer pin.
        let newer_root = tempfile::tempdir().unwrap();
        let newer_path = pin_path(newer_root.path(), &base);
        commit_pin(&newer_path, None, &larger).unwrap();
        let older_entry = VerifiedLogEntry {
            index: 0,
            leaf_json: String::new(),
            sth: sth.clone(),
            pin_file: newer_path.clone(),
            pin_basis: None,
            pinned_from_size: None,
            pin_candidate: smaller.clone(),
        };
        commit_verified_pin(&client, &base, &older_entry)
            .await
            .unwrap();
        assert!(same_tree_head(
            &load_pin(&newer_path).unwrap().unwrap(),
            &larger
        ));

        // A size-3 verifier finishing after another process pinned size 2
        // proves the same extension and advances without waiting for a later
        // scheduled check.
        let older_root = tempfile::tempdir().unwrap();
        let older_path = pin_path(older_root.path(), &base);
        commit_pin(&older_path, None, &smaller).unwrap();
        let newer_entry = VerifiedLogEntry {
            index: 0,
            leaf_json: String::new(),
            sth,
            pin_file: older_path.clone(),
            pin_basis: None,
            pinned_from_size: None,
            pin_candidate: larger.clone(),
        };
        commit_verified_pin(&client, &base, &newer_entry)
            .await
            .unwrap();
        assert!(same_tree_head(
            &load_pin(&older_path).unwrap().unwrap(),
            &larger
        ));
        server.abort();
    }

    #[test]
    fn concurrent_same_size_root_disagreement_is_a_verification_result() {
        let candidate = SthPin {
            size: 7,
            root: "Y2FuZGlkYXRl".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 1,
        };
        let current = SthPin {
            size: 7,
            root: "Y3VycmVudA".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 1,
        };
        match concurrent_pin_order(&candidate, &current) {
            Err(VerifyFailure::Verification { summary, .. }) => {
                assert!(summary.contains("different roots"), "{summary}");
            }
            other => panic!("same-size disagreement must alarm, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_different_sizes_require_directional_reconciliation() {
        let smaller = SthPin {
            size: 7,
            root: "c21hbGxlcg".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 1,
        };
        let larger = SthPin {
            size: 9,
            root: "bGFyZ2Vy".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 1,
        };
        assert_eq!(
            concurrent_pin_order(&larger, &smaller).unwrap(),
            ConcurrentPinOrder::CandidateExtendsCurrent
        );
        assert_eq!(
            concurrent_pin_order(&smaller, &larger).unwrap(),
            ConcurrentPinOrder::CurrentExtendsCandidate
        );
    }

    #[tokio::test]
    async fn daemon_checks_are_single_flight() {
        let first = daemon_check_lock().lock().await;
        let waiting = tokio::spawn(async {
            let _second = daemon_check_lock().lock().await;
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(first);
        waiting.await.unwrap();
    }
}
