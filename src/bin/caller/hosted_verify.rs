//! Out-of-band verifier for Connect code/installer transparency
//! (docs/src/self-hosted-rendezvous.md, "Code transparency for the served
//! Connect bundle"; the evidence leg of first-contact rung three in
//! docs/src/trust-tiers.md). The rendezvous commits what it serves to its
//! append-only transparency log (`artifact_manifest` entries); this module
//! fetches the LIVE artifacts over HTTPS exactly as a browser would,
//! hashes them, and compares against the logged manifest — then verifies
//! the manifest's inclusion proof against the signed tree head and the
//! tree head's consistency against a locally pinned one under the daemon
//! state root (`~/.intendant/hosted-verify/`, honoring `$INTENDANT_HOME`).
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
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use url::Url;

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
}

fn registry() -> &'static Mutex<HostedBundleStatus> {
    static STATUS: OnceLock<Mutex<HostedBundleStatus>> = OnceLock::new();
    STATUS.get_or_init(|| {
        Mutex::new(HostedBundleStatus {
            state: "unchecked".to_string(),
            checked_unix_ms: None,
            last_error: None,
            mismatches: Vec::new(),
        })
    })
}

pub(crate) fn status_snapshot() -> HostedBundleStatus {
    registry()
        .lock()
        .expect("hosted bundle status poisoned")
        .clone()
}

fn with_status(update: impl FnOnce(&mut HostedBundleStatus)) {
    let mut status = registry().lock().expect("hosted bundle status poisoned");
    update(&mut status);
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
        let root_b64u = value
            .get("root")
            .and_then(|v| v.as_str())
            .ok_or("sth missing root")?
            .to_string();
        let unix_ms = value
            .get("unix_ms")
            .and_then(|v| v.as_u64())
            .ok_or("sth missing unix_ms")?;
        let signature = b64u_decode(
            value
                .get("signature")
                .and_then(|v| v.as_str())
                .ok_or("sth missing signature")?,
        )?;
        let public_key_b64u = value
            .get("public_key")
            .and_then(|v| v.as_str())
            .ok_or("sth missing public_key")?
            .to_string();
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
    let leaf: serde_json::Value =
        serde_json::from_str(leaf_json).map_err(|e| format!("manifest leaf is not JSON: {e}"))?;
    if leaf.get("kind").and_then(|v| v.as_str()) != Some("artifact_manifest") {
        return Err("leaf is not an artifact_manifest entry".to_string());
    }
    let artifacts: Vec<ManifestArtifact> = leaf
        .get("artifacts")
        .and_then(|v| v.as_array())
        .ok_or("manifest leaf missing artifacts")?
        .iter()
        .map(|entry| {
            let path = entry
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("artifact missing path")?
                .to_string();
            let sha256 = entry
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or("artifact missing sha256")?
                .to_string();
            if !path.starts_with('/') {
                return Err(format!("artifact path {path:?} is not absolute"));
            }
            Ok(ManifestArtifact { path, sha256 })
        })
        .collect::<Result<_, String>>()?;
    let manifest_hash = leaf
        .get("manifest_hash")
        .and_then(|v| v.as_str())
        .ok_or("manifest leaf missing manifest_hash")?
        .to_string();
    if manifest_hash_hex(&artifacts) != manifest_hash {
        return Err("manifest_hash does not recompute from the carried artifact list".to_string());
    }
    Ok(ManifestLeaf {
        unix_ms: leaf.get("unix_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        bundle_version: leaf
            .get("bundle_version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        git_sha: leaf
            .get("git_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        manifest_hash,
        artifacts,
    })
}

// ── The pinned tree head (TOFU, then consistency forever) ──

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SthPin {
    size: u64,
    /// b64u, as the service reports it.
    root: String,
    public_key: String,
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

fn load_pin(path: &Path) -> Option<SthPin> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_pin(path: &Path, pin: &SthPin) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(pin).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
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

#[derive(Debug)]
enum ArtifactFetch {
    Hashed { sha256_hex: String },
    HttpStatus(u16),
    TooLarge,
}

/// The per-artifact verdict: `None` = matches the log.
fn artifact_mismatch(artifact: &ManifestArtifact, fetched: &ArtifactFetch) -> Option<String> {
    match fetched {
        ArtifactFetch::Hashed { sha256_hex } if *sha256_hex == artifact.sha256 => None,
        ArtifactFetch::Hashed { sha256_hex } => Some(format!(
            "{}: manifest {} · served {}",
            artifact.path,
            short_hash(&artifact.sha256),
            short_hash(sha256_hex),
        )),
        ArtifactFetch::HttpStatus(status) => Some(format!("{}: HTTP {status}", artifact.path)),
        ArtifactFetch::TooLarge => Some(format!(
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
        .user_agent("intendant-hosted-verify")
        .build()
        .map_err(|e| e.to_string())
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
    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("GET {url}: {e}"))
}

/// Fetch one artifact, hashing as it streams. Transport errors bubble as
/// `Err` (the whole run becomes Unavailable); HTTP error statuses and
/// oversize bodies are verdicts, not failures.
async fn fetch_artifact(
    client: &reqwest::Client,
    url: Url,
    byte_cap: usize,
) -> Result<ArtifactFetch, String> {
    use futures_util::StreamExt as _;
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Ok(ArtifactFetch::HttpStatus(status.as_u16()));
    }
    let mut hasher = Sha256::new();
    let mut total = 0usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("GET {url}: {e}"))?;
        total += chunk.len();
        if total > byte_cap {
            return Ok(ArtifactFetch::TooLarge);
        }
        hasher.update(&chunk);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(ArtifactFetch::Hashed {
        sha256_hex: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}

/// A log-endpoint response carried through the shared first half of the
/// ritual: the tree head stands on its signature, the leaf is IN the
/// tree the head signs, and the head extends the pinned one. The caller
/// finishes its own artifact comparison, then advances the pin by
/// writing `pin_candidate` to `pin_file` — only after everything held.
struct VerifiedLogEntry {
    index: u64,
    leaf_json: String,
    sth: Sth,
    pin_file: PathBuf,
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
    let proof: Vec<[u8; 32]> = response
        .get("proof")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Unavailable("response missing proof".to_string()))?
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
    let pin = load_pin(&pin_file);
    let pinned_from_size = pin.as_ref().map(|p| p.size);
    match pin_decision(pin.as_ref(), &sth).map_err(verification)? {
        PinDecision::FirstContact | PinDecision::Unchanged => {}
        PinDecision::NeedConsistency { old_size, old_root } => {
            let url = crate::connect_rendezvous::join_url(base, "api/log/consistency")
                .map_err(Unavailable)
                .map(|mut url| {
                    url.set_query(Some(&format!("old={old_size}&new={}", sth.size)));
                    url
                })?;
            let consistency = fetch_json(client, url).await.map_err(Unavailable)?;
            let consistency_proof: Vec<[u8; 32]> = consistency
                .get("proof")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Unavailable("consistency response missing proof".to_string()))?
                .iter()
                .map(|hash| b64u_decode_hash(hash.as_str().unwrap_or_default()))
                .collect::<Result<_, String>>()
                .map_err(Unavailable)?;
            if !verify_consistency(
                old_size as usize,
                sth.size as usize,
                &old_root,
                &sth.root,
                &consistency_proof,
            ) {
                return Err(verification(
                    "consistency proof failed — history was rewritten since the pinned tree head"
                        .to_string(),
                ));
            }
        }
    }

    let pin_candidate = SthPin {
        size: sth.size,
        root: sth.root_b64u.clone(),
        public_key: sth.public_key_b64u.clone(),
        pinned_unix_ms: pin.map(|p| p.pinned_unix_ms).unwrap_or_else(now_unix_ms),
    };
    Ok(VerifiedLogEntry {
        index,
        leaf_json: leaf_json.to_string(),
        sth,
        pin_file,
        pinned_from_size,
        pin_candidate,
    })
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
    let mut mismatches = Vec::new();
    for artifact in &leaf.artifacts {
        let url = crate::connect_rendezvous::join_url(base, &artifact.path).map_err(Unavailable)?;
        let fetched = fetch_artifact(&client, url, ARTIFACT_BYTE_CAP)
            .await
            .map_err(Unavailable)?;
        if let Some(diff) = artifact_mismatch(artifact, &fetched) {
            mismatches.push(diff);
        }
    }
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

    // Everything held — advance the pin.
    save_pin(&entry.pin_file, &entry.pin_candidate).map_err(Unavailable)?;

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
    let leaf: serde_json::Value =
        serde_json::from_str(leaf_json).map_err(|e| format!("release leaf is not JSON: {e}"))?;
    if leaf.get("kind").and_then(|v| v.as_str()) != Some("release_manifest") {
        return Err("leaf is not a release_manifest entry".to_string());
    }
    let tag = leaf
        .get("tag")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or("release leaf missing tag")?
        .to_string();
    let artifacts: Vec<ReleaseArtifact> = leaf
        .get("artifacts")
        .and_then(|v| v.as_array())
        .ok_or("release leaf missing artifacts")?
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
                .ok_or("artifact missing name")?
                .to_string();
            let sha256 = entry
                .get("sha256")
                .and_then(|v| v.as_str())
                .ok_or("artifact missing sha256")?
                .to_string();
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
    let manifest_hash = leaf
        .get("manifest_hash")
        .and_then(|v| v.as_str())
        .ok_or("release leaf missing manifest_hash")?
        .to_string();
    if release_manifest_hash_hex(&tag, &artifacts) != manifest_hash {
        return Err("manifest_hash does not recompute from the carried artifact list".to_string());
    }
    Ok(ReleaseLeaf {
        unix_ms: leaf.get("unix_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        tag,
        version: leaf
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        platforms: leaf
            .get("platforms")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
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
    release
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or("GitHub release response missing assets")?
        .iter()
        .map(|asset| {
            let name = asset
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
                .ok_or("GitHub asset missing name")?
                .to_string();
            let size = asset
                .get("size")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| format!("GitHub asset {name} missing size"))?;
            let sha256 = asset
                .get("digest")
                .and_then(|v| v.as_str())
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .map(str::to_string);
            let download_url = asset
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .map(str::to_string);
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
    let body = response.json::<serde_json::Value>().await.ok();
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
    let release_url = crate::connect_rendezvous::join_url(
        github_api,
        &format!("repos/{repo}/releases/tags/{}", leaf.tag),
    )
    .map_err(Unavailable)?;
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
            match fetch_artifact(&client, url, RELEASE_DOWNLOAD_BYTE_CAP)
                .await
                .map_err(Unavailable)?
            {
                ArtifactFetch::Hashed { sha256_hex } if sha256_hex == artifact.sha256 => {
                    downloaded += 1;
                }
                ArtifactFetch::Hashed { sha256_hex } => mismatches.push(format!(
                    "{}: logged sha256 {} · downloaded {}",
                    artifact.name,
                    short_hash(&artifact.sha256),
                    short_hash(&sha256_hex),
                )),
                ArtifactFetch::HttpStatus(status) => {
                    mismatches.push(format!("{}: download HTTP {status}", artifact.name))
                }
                ArtifactFetch::TooLarge => mismatches.push(format!(
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
    save_pin(&entry.pin_file, &entry.pin_candidate).map_err(Unavailable)?;

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
    let status = crate::connect_rendezvous::status_snapshot();
    if !status.configured {
        return;
    }
    let Some(base) = status
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Url::parse(s).ok())
    else {
        return;
    };
    let now = now_unix_ms();
    match verify_hosted_bundle(&base, &crate::platform::intendant_home()).await {
        Ok(_) => with_status(|s| {
            s.state = "ok".to_string();
            s.checked_unix_ms = Some(now);
            s.last_error = None;
            s.mismatches = Vec::new();
        }),
        Err(VerifyFailure::Unavailable(error)) => with_status(|s| {
            s.last_error = Some(error);
        }),
        Err(VerifyFailure::Verification {
            summary,
            mismatches,
        }) => {
            with_status(|s| {
                s.state = "alert".to_string();
                s.checked_unix_ms = Some(now);
                s.last_error = None;
                s.mismatches = if mismatches.is_empty() {
                    vec![summary.clone()]
                } else {
                    mismatches.clone()
                };
            });
            eprintln!(
                "[hosted-verify] HOSTED BUNDLE ALERT: {} is serving Connect code/assets that do \
                 not match its public transparency log ({summary}): {:?} — treat hosted tabs \
                 against this rendezvous as compromised until the operator explains; direct \
                 and fleet-name dashboards are unaffected",
                base, mismatches,
            );
        }
    }
}

/// First tick shortly after startup (registration needs a moment), then
/// twice daily — the CT tripwire's cadence. Spawned once at gateway
/// startup, beside `fleet_cert::spawn_renewal_loop`.
pub(crate) fn spawn_hosted_bundle_monitor() {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let delay = if first { 7 * 60 } else { 12 * 60 * 60 };
            first = false;
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            check_once().await;
        }
    });
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
honoring $INTENDANT_HOME — releases share the same pin), then downloads
every listed artifact exactly as a browser would and compares hashes.
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
            "If you did not expect a deploy just now, treat hosted tabs against this \
             rendezvous as compromised and reach your daemons directly.",
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
        let artifacts = vec![ManifestArtifact {
            path: "/app.html".to_string(),
            sha256: sha256_hex(b"bundle"),
        }];
        let good = serde_json::json!({
            "kind": "artifact_manifest",
            "unix_ms": 42,
            "bundle_version": "0.1.0",
            "git_sha": "abc1234",
            "manifest_hash": manifest_hash_hex(&artifacts),
            "artifacts": [{ "path": "/app.html", "sha256": sha256_hex(b"bundle") }],
        })
        .to_string();
        let leaf = parse_manifest_leaf(&good).unwrap();
        assert_eq!(leaf.artifacts.len(), 1);
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
                    sha256_hex: sha256_hex(b"the logged bundle")
                }
            ),
            None
        );
        let diff = artifact_mismatch(
            &expected,
            &ArtifactFetch::Hashed {
                sha256_hex: sha256_hex(b"a different bundle"),
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
        assert!(artifact_mismatch(&expected, &ArtifactFetch::TooLarge)
            .unwrap()
            .contains("exceeded"));
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
        let pin = load_pin(&pin_path(state_root.path(), &base)).expect("pin created");
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
            load_pin(&pin_path(state_root.path(), &base)).unwrap().size,
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
        assert!(load_pin(&pin_path(state_root.path(), &base)).is_none());

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
        assert!(load_pin(&path).is_none());
        let pin = SthPin {
            size: 12,
            root: "cm9vdA".to_string(),
            public_key: "a2V5".to_string(),
            pinned_unix_ms: 77,
        };
        save_pin(&path, &pin).unwrap();
        let loaded = load_pin(&path).unwrap();
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
}
