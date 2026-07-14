//! The transparency log (RFC 6962 Merkle tree over name-binding events):
//! hash/proof primitives, the signed tree head, the log read API, and the
//! org-revocation-list bulletin board whose sightings it witnesses.

use super::*;

// ── Transparency log: RFC 6962 Merkle tree over name-binding events ──
//
// The service commits to every consequential binding it hands out
// (daemon_id → daemon key at claim time, handle creation, org
// revocation-list sightings, attestations) in an append-only log.
// Browsers pin the signed tree head and verify consistency on every
// visit, so rewriting or forking history is detectable — the rendezvous
// stays outside the daemon authority mint AND becomes checkable about the
// one thing it could quietly lie about: first introductions.

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub(crate) fn log_leaf_hash(leaf_json: &str) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + leaf_json.len());
    buf.push(0x00);
    buf.extend_from_slice(leaf_json.as_bytes());
    sha256(&buf)
}

pub(crate) fn log_node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(65);
    buf.push(0x01);
    buf.extend_from_slice(left);
    buf.extend_from_slice(right);
    sha256(&buf)
}

/// Largest power of two strictly less than n (n >= 2).
pub(crate) fn log_split_point(n: usize) -> usize {
    let mut k = 1usize;
    while k * 2 < n {
        k *= 2;
    }
    k
}

/// MTH(D[n]) per RFC 6962 §2.1.
pub(crate) fn log_tree_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    match leaves.len() {
        0 => sha256(b""),
        1 => leaves[0],
        n => {
            let k = log_split_point(n);
            log_node_hash(&log_tree_root(&leaves[..k]), &log_tree_root(&leaves[k..]))
        }
    }
}

/// PATH(m, D[n]) per RFC 6962 §2.1.1 — inclusion proof for leaf m.
pub(crate) fn log_inclusion_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let n = leaves.len();
    if n <= 1 {
        return Vec::new();
    }
    let k = log_split_point(n);
    if m < k {
        let mut path = log_inclusion_proof(m, &leaves[..k]);
        path.push(log_tree_root(&leaves[k..]));
        path
    } else {
        let mut path = log_inclusion_proof(m - k, &leaves[k..]);
        path.push(log_tree_root(&leaves[..k]));
        path
    }
}

/// PROOF(m, D[n]) per RFC 6962 §2.1.2 — consistency proof old size m → n.
pub(crate) fn log_consistency_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
    fn subproof(m: usize, leaves: &[[u8; 32]], complete: bool) -> Vec<[u8; 32]> {
        let n = leaves.len();
        if m == n {
            return if complete {
                Vec::new()
            } else {
                vec![log_tree_root(leaves)]
            };
        }
        let k = log_split_point(n);
        if m <= k {
            let mut proof = subproof(m, &leaves[..k], complete);
            proof.push(log_tree_root(&leaves[k..]));
            proof
        } else {
            let mut proof = subproof(m - k, &leaves[k..], false);
            proof.push(log_tree_root(&leaves[..k]));
            proof
        }
    }
    if m == 0 || m > leaves.len() {
        return Vec::new();
    }
    subproof(m, leaves, true)
}

/// Inclusion verification per RFC 9162 §2.1.3.2. The service only ever
/// PRODUCES proofs (browsers and the E2E validator verify with their own
/// implementations); this verifier exists to test the producers against.
#[cfg(test)]
pub(crate) fn log_verify_inclusion(
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
            r = log_node_hash(p, &r);
            if fn_.is_multiple_of(2) {
                while fn_.is_multiple_of(2) && fn_ != 0 {
                    fn_ >>= 1;
                    sn >>= 1;
                }
            }
        } else {
            r = log_node_hash(&r, p);
        }
        fn_ >>= 1;
        sn >>= 1;
    }
    sn == 0 && r == *root
}

/// Consistency verification per RFC 9162 §2.1.4.2 (test-only; see above).
#[cfg(test)]
pub(crate) fn log_verify_consistency(
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
    // When the old tree is a complete subtree the prover omits the old
    // root; conceptually it is prepended here.
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
            fr = log_node_hash(p, &fr);
            sr = log_node_hash(p, &sr);
            if fn_.is_multiple_of(2) {
                while fn_.is_multiple_of(2) && fn_ != 0 {
                    fn_ >>= 1;
                    sn >>= 1;
                }
            }
        } else {
            sr = log_node_hash(&sr, p);
        }
        fn_ >>= 1;
        sn >>= 1;
    }
    fr == *old_root && sr == *new_root && sn == 0
}

pub(crate) fn load_or_create_log_keypair(
    store: &mut Store,
) -> Result<ring::signature::EcdsaKeyPair, String> {
    let rng = ring::rand::SystemRandom::new();
    if store.log_private_pk8_b64.is_none() {
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .map_err(|_| "log key generation failed".to_string())?;
        store.log_private_pk8_b64 = Some(b64u(document.as_ref()));
    }
    let der = b64u_decode(store.log_private_pk8_b64.as_deref().unwrap_or(""))
        .map_err(|_| "stored log key is not valid base64".to_string())?;
    ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &der,
        &rng,
    )
    .map_err(|_| "stored log key is invalid".to_string())
}

pub(crate) fn log_sth_payload(size: usize, root_b64u: &str, unix_ms: u64) -> String {
    format!("intendant-log-sth-v1\n{size}\n{root_b64u}\n{unix_ms}")
}

pub(crate) fn log_leaves(store: &Store) -> Vec<[u8; 32]> {
    store
        .log_entries
        .iter()
        .map(|entry| log_leaf_hash(&entry.leaf_json))
        .collect()
}

/// The signed tree head as a bare object (no `ok` envelope) — nested by
/// responses that carry an STH alongside other data.
pub(crate) fn signed_tree_head_fields(state: &AppState, store: &Store) -> serde_json::Value {
    use ring::signature::KeyPair as _;
    let leaves = log_leaves(store);
    let root = b64u(&log_tree_root(&leaves));
    let unix_ms = now_unix_ms();
    let payload = log_sth_payload(leaves.len(), &root, unix_ms);
    let rng = ring::rand::SystemRandom::new();
    let signature = state
        .log_key
        .sign(&rng, payload.as_bytes())
        .map(|sig| b64u(sig.as_ref()))
        .unwrap_or_default();
    json!({
        "size": leaves.len(),
        "root": root,
        "unix_ms": unix_ms,
        "signature": signature,
        "public_key": b64u(state.log_key.public_key().as_ref()),
    })
}

pub(crate) fn signed_tree_head(state: &AppState, store: &Store) -> serde_json::Value {
    let mut sth = signed_tree_head_fields(state, store);
    if let Some(map) = sth.as_object_mut() {
        map.insert("ok".to_string(), json!(true));
    }
    sth
}

pub(crate) async fn log_sth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let store = state.store.lock().await;
    Ok(orl_cors(
        Json(signed_tree_head(&state, &store)).into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogRangeQuery {
    #[serde(default)]
    start: usize,
    #[serde(default)]
    count: usize,
}

pub(crate) async fn log_entries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LogRangeQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let count = query.count.clamp(1, 256);
    let store = state.store.lock().await;
    let total = store.log_entries.len();
    let start = query.start.min(total);
    let end = start.saturating_add(count).min(total);
    let entries: Vec<serde_json::Value> = store.log_entries[start..end]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            json!({
                "index": start + offset,
                "kind": entry.kind,
                "unix_ms": entry.unix_ms,
                "leaf_json": entry.leaf_json,
            })
        })
        .collect();
    Ok(orl_cors(
        Json(json!({ "ok": true, "total": total, "start": start, "entries": entries }))
            .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogProofQuery {
    index: usize,
    #[serde(default)]
    size: usize,
}

pub(crate) async fn log_proof(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LogProofQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let store = state.store.lock().await;
    let leaves = log_leaves(&store);
    let size = if query.size == 0 {
        leaves.len()
    } else {
        query.size
    };
    if size > leaves.len() || query.index >= size {
        return Err(ApiError::bad_request("index/size out of range"));
    }
    let proof: Vec<String> = log_inclusion_proof(query.index, &leaves[..size])
        .iter()
        .map(|hash| b64u(hash))
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "index": query.index,
            "size": size,
            "root": b64u(&log_tree_root(&leaves[..size])),
            "proof": proof,
        }))
        .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogConsistencyQuery {
    old: usize,
    #[serde(default)]
    new: usize,
}

pub(crate) async fn log_consistency(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LogConsistencyQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let store = state.store.lock().await;
    let leaves = log_leaves(&store);
    let new_size = if query.new == 0 {
        leaves.len()
    } else {
        query.new
    };
    if new_size > leaves.len() || query.old == 0 || query.old > new_size {
        return Err(ApiError::bad_request("old/new out of range"));
    }
    let proof: Vec<String> = log_consistency_proof(query.old, &leaves[..new_size])
        .iter()
        .map(|hash| b64u(hash))
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "old": query.old,
            "new": new_size,
            "old_root": b64u(&log_tree_root(&leaves[..query.old])),
            "new_root": b64u(&log_tree_root(&leaves[..new_size])),
            "proof": proof,
        }))
        .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogFindQuery {
    #[serde(default)]
    daemon_id: String,
    #[serde(default)]
    handle: String,
}

/// Latest log entry binding a daemon_id or handle — the lookup a browser
/// does before trusting a first introduction.
pub(crate) async fn log_find(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LogFindQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let daemon_id = query.daemon_id.trim();
    let handle = query.handle.trim();
    if daemon_id.is_empty() && handle.is_empty() {
        return Err(ApiError::bad_request("daemon_id or handle is required"));
    }
    let store = state.store.lock().await;
    let found = store
        .log_entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            let Ok(data) = serde_json::from_str::<serde_json::Value>(&entry.leaf_json) else {
                return false;
            };
            let daemon_match = !daemon_id.is_empty()
                && entry.kind == "daemon_claimed"
                && data.get("daemon_id").and_then(|v| v.as_str()) == Some(daemon_id);
            let handle_match =
                !handle.is_empty() && data.get("handle").and_then(|v| v.as_str()) == Some(handle);
            daemon_match || (daemon_id.is_empty() && handle_match)
        });
    let Some((index, entry)) = found else {
        return Ok(orl_cors(
            Json(json!({ "ok": true, "found": false })).into_response(),
        ));
    };
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "found": true,
            "index": index,
            "size": store.log_entries.len(),
            "kind": entry.kind,
            "unix_ms": entry.unix_ms,
            "leaf_json": entry.leaf_json,
        }))
        .into_response(),
    ))
}

// ── Code transparency: the served Connect bundle, committed ──
//
// The hosted origin's residual power is serving different bytes than it
// claims (trust-architecture.md's "it could serve this page with
// malicious code"). These entries commit what the service SERVES to the
// same append-only log that commits what it SAYS: at startup the service
// hashes every embedded artifact it can serve — the Connect pages exactly
// as this instance renders them (origin-injected installers included) — and appends an
// `artifact_manifest` entry when the manifest changed. Out-of-band
// monitors (`intendant hosted-verify`, the daemon tripwire in
// bin/caller/hosted_verify.rs) fetch the live artifacts and compare;
// page JS can never honestly self-verify, so nothing here is
// browser-side. Strictly additive: existing clients see one more entry
// kind in a tree that only ever grows.

pub(crate) const ARTIFACT_MANIFEST_KIND: &str = "artifact_manifest";

/// One served artifact: the URL path a GET fetches it at, and the
/// lowercase-hex sha256 of the exact bytes served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactRecord {
    pub path: String,
    pub sha256: String,
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    sha256(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The routes served from compiled-in bytes, rendered exactly as this
/// instance serves them (the pages and installers are deterministic
/// functions of the public origin). The `/connect` page matters most:
/// it carries the passkey and log-verification JS — the code a hosted
/// betrayal would most want to swap.
pub(crate) fn embedded_artifacts(config: &ServiceConfig) -> Vec<ArtifactRecord> {
    let origin = config.public_origin.as_str();
    let mut artifacts = vec![
        (
            "/".to_string(),
            sha256_hex(landing_ui_html(origin).as_bytes()),
        ),
        (
            "/connect".to_string(),
            sha256_hex(connect_page_html(origin).as_bytes()),
        ),
        (
            "/access".to_string(),
            sha256_hex(access_page_html(origin).as_bytes()),
        ),
        (
            "/trust".to_string(),
            sha256_hex(trust_ui_html(origin).as_bytes()),
        ),
        (
            "/install.sh".to_string(),
            sha256_hex(install_sh_body(origin).as_bytes()),
        ),
        (
            "/install.ps1".to_string(),
            sha256_hex(install_ps1_body(origin).as_bytes()),
        ),
        ("/logo.svg".to_string(), sha256_hex(LOGO_SVG.as_bytes())),
        ("/favicon.png".to_string(), sha256_hex(BRAND_ICON_PNG)),
        (
            "/sw.js".to_string(),
            sha256_hex(CONNECT_SERVICE_WORKER_JS.as_bytes()),
        ),
    ];
    for (name, bytes) in LANDING_ASSETS {
        artifacts.push((format!("/assets/landing/{name}"), sha256_hex(bytes)));
    }
    artifacts
        .into_iter()
        .map(|(path, sha256)| ArtifactRecord { path, sha256 })
        .collect()
}

/// The full served-artifact manifest: only compiled-in Connect pages and
/// assets, sorted by path. The daemon dashboard, WASM, and every other file
/// under the deprecated static-root input are deliberately outside both the
/// served surface and this manifest.
pub(crate) fn served_artifact_manifest(config: &ServiceConfig) -> Vec<ArtifactRecord> {
    let mut by_path = std::collections::BTreeMap::new();
    for artifact in embedded_artifacts(config) {
        by_path.insert(artifact.path, artifact.sha256);
    }
    by_path
        .into_iter()
        .map(|(path, sha256)| ArtifactRecord { path, sha256 })
        .collect()
}

/// Canonical manifest hash: sha256 (lowercase hex) over
///
/// ```text
/// intendant-artifact-manifest-v1\n
/// {path}\t{sha256}\n      (per artifact, sorted by path, byte order)
/// ```
///
/// Deliberately independent of JSON serialization so external monitors
/// can recompute it from any faithful copy of the list. REPLICATED in
/// `bin/caller/hosted_verify.rs` (the two binaries never link each
/// other); golden tests twin the two implementations.
pub(crate) fn manifest_hash_hex(artifacts: &[ArtifactRecord]) -> String {
    let mut canonical = String::from("intendant-artifact-manifest-v1\n");
    for artifact in artifacts {
        canonical.push_str(&artifact.path);
        canonical.push('\t');
        canonical.push_str(&artifact.sha256);
        canonical.push('\n');
    }
    sha256_hex(canonical.as_bytes())
}

pub(crate) fn latest_artifact_manifest_hash(store: &Store) -> Option<String> {
    store
        .log_entries
        .iter()
        .rev()
        .find(|entry| entry.kind == ARTIFACT_MANIFEST_KIND)
        .and_then(|entry| serde_json::from_str::<serde_json::Value>(&entry.leaf_json).ok())
        .and_then(|leaf| {
            leaf.get("manifest_hash")
                .and_then(|hash| hash.as_str())
                .map(str::to_string)
        })
}

/// Compute the served-artifact manifest and append an
/// `artifact_manifest` entry when it differs from the latest logged one.
/// Returns whether an entry was appended (the caller persists). Called
/// at startup, inside the same single-threaded window that loads the
/// store — the log and what the process serves cannot disagree.
pub(crate) fn record_artifact_manifest(store: &mut Store, config: &ServiceConfig) -> bool {
    let artifacts = served_artifact_manifest(config);
    let manifest_hash = manifest_hash_hex(&artifacts);
    if latest_artifact_manifest_hash(store).as_deref() == Some(manifest_hash.as_str()) {
        eprintln!(
            "[connect] artifact manifest unchanged ({} artifacts, {})",
            artifacts.len(),
            &manifest_hash[..12.min(manifest_hash.len())],
        );
        return false;
    }
    eprintln!(
        "[connect] artifact manifest logged: {} artifacts, {} (bundle {} {})",
        artifacts.len(),
        &manifest_hash[..12.min(manifest_hash.len())],
        env!("CARGO_PKG_VERSION"),
        env!("INTENDANT_GIT_SHA"),
    );
    append_log_entry(
        store,
        ARTIFACT_MANIFEST_KIND,
        json!({
            "bundle_version": env!("CARGO_PKG_VERSION"),
            "git_sha": env!("INTENDANT_GIT_SHA"),
            "manifest_hash": manifest_hash,
            "artifacts": artifacts,
        }),
    );
    true
}

/// The current artifact manifest with everything an out-of-band monitor
/// needs in one response: the exact leaf bytes, its index, an inclusion
/// proof, and the signed tree head the proof verifies against — all
/// computed under one store lock so they cohere.
pub(crate) async fn log_artifact_manifest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let store = state.store.lock().await;
    let found = store
        .log_entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| entry.kind == ARTIFACT_MANIFEST_KIND);
    let Some((index, entry)) = found else {
        return Ok(orl_cors(
            Json(json!({ "ok": true, "found": false })).into_response(),
        ));
    };
    let leaves = log_leaves(&store);
    let proof: Vec<String> = log_inclusion_proof(index, &leaves)
        .iter()
        .map(|hash| b64u(hash))
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "found": true,
            "index": index,
            "kind": entry.kind,
            "unix_ms": entry.unix_ms,
            "leaf_json": entry.leaf_json,
            "proof": proof,
            "sth": signed_tree_head_fields(&state, &store),
        }))
        .into_response(),
    ))
}

// ── Release transparency: app release artifacts, committed ──
//
// The `artifact_manifest` entries above commit what the service SERVES;
// these commit what the project RELEASES (trust-tiers.md's "update
// channel" thread). The tag-triggered release pipeline
// (.github/workflows/release.yml) hashes every artifact it published to
// the GitHub release and submits a `release_manifest` here, gated by a
// dedicated bearer token (`--release-token` /
// INTENDANT_CONNECT_RELEASE_TOKEN — deliberately not the operator
// `daemon_token`, so the CI secret can only ever append release
// manifests). Reads and proofs are public like every other log
// endpoint; `intendant hosted-verify --releases` is the out-of-band
// monitor, and the macOS app's update check surfaces logged/not-logged
// as an advisory. Strictly additive: one more entry kind in a tree
// that only ever grows.

pub(crate) const RELEASE_MANIFEST_KIND: &str = "release_manifest";

/// Serialized-body cap for a submitted release manifest (the ORL
/// bulletin's bound; a release names a handful of artifacts).
pub(crate) const MAX_RELEASE_MANIFEST_BYTES: usize = 64 * 1024;
const RELEASE_TAG_MAX: usize = 100;
const RELEASE_ARTIFACT_LIMIT: usize = 256;
const RELEASE_ARTIFACT_NAME_MAX: usize = 200;
const RELEASE_PLATFORM_LIMIT: usize = 32;

/// One released artifact: the GitHub release asset's file name, the
/// lowercase-hex sha256 of its exact bytes, and its size in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReleaseArtifactRecord {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// Canonical release-manifest hash: sha256 (lowercase hex) over
///
/// ```text
/// intendant-release-manifest-v1\n
/// {tag}\n
/// {name}\t{sha256}\t{size}\n      (per artifact, sorted by name, byte order)
/// ```
///
/// The artifact-manifest discipline: independent of JSON serialization
/// so external monitors can recompute it from any faithful copy.
/// REPLICATED in `bin/caller/hosted_verify.rs`; golden tests twin the
/// two implementations.
pub(crate) fn release_manifest_hash_hex(tag: &str, artifacts: &[ReleaseArtifactRecord]) -> String {
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

/// Bearer gate for release-manifest submission. Mirrors
/// `require_admin_auth`'s stance: an unset token must not mean an open
/// submission endpoint, so unconfigured → 503, wrong/missing → 401.
/// Pure over the configured value so tests need no `AppState`.
pub(crate) fn check_release_token(configured: Option<&str>, headers: &HeaderMap) -> ApiResult<()> {
    let Some(token) = configured.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "release submission requires the service to be started with --release-token",
        ));
    };
    let expected = format!("Bearer {token}");
    if headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err(ApiError::unauthorized(
            "missing or invalid release bearer token",
        ))
    }
}

/// A validated, canonicalized (name-sorted) release manifest as it will
/// be committed to the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedReleaseManifest {
    pub tag: String,
    pub version: String,
    pub platforms: Vec<String>,
    /// Sorted by name — the canonical order the hash covers.
    pub artifacts: Vec<ReleaseArtifactRecord>,
    pub manifest_hash: String,
}

/// Tags, versions, platform labels, and artifact names share one strict
/// vocabulary: ASCII alphanumerics plus `. _ - +`, no leading `-`/`.`.
/// Every value the pipeline actually submits fits; nothing that could
/// smuggle a path, header, or control character does.
fn valid_release_component(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value.starts_with(['-', '.'])
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Shape-validate a submitted release manifest and canonicalize it
/// (artifacts name-sorted, manifest hash computed). Every rejection is
/// a message suitable for a 400 body.
pub(crate) fn validate_release_manifest(
    body: &serde_json::Value,
) -> Result<ValidatedReleaseManifest, String> {
    let tag = body
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !valid_release_component(&tag, RELEASE_TAG_MAX) {
        return Err(
            "tag must be 1-100 chars of [A-Za-z0-9._+-], not starting with '-' or '.'".to_string(),
        );
    }
    let version = body
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !valid_release_component(&version, RELEASE_TAG_MAX) {
        return Err("version must be 1-100 chars of [A-Za-z0-9._+-]".to_string());
    }
    let platforms: Vec<String> = body
        .get("platforms")
        .and_then(|v| v.as_array())
        .ok_or("platforms must be an array of target labels")?
        .iter()
        .map(|entry| {
            let platform = entry.as_str().unwrap_or("").trim().to_string();
            if valid_release_component(&platform, RELEASE_TAG_MAX) {
                Ok(platform)
            } else {
                Err(format!("invalid platform label {entry}"))
            }
        })
        .collect::<Result<_, String>>()?;
    if platforms.is_empty() || platforms.len() > RELEASE_PLATFORM_LIMIT {
        return Err(format!(
            "platforms must name 1-{RELEASE_PLATFORM_LIMIT} targets"
        ));
    }
    let mut artifacts: Vec<ReleaseArtifactRecord> =
        body.get("artifacts")
            .and_then(|v| v.as_array())
            .ok_or("artifacts must be an array")?
            .iter()
            .map(|entry| {
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !valid_release_component(&name, RELEASE_ARTIFACT_NAME_MAX) {
                    return Err(format!("invalid artifact name {:?}", name));
                }
                let sha256 = entry
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !valid_sha256_hex(&sha256) {
                    return Err(format!(
                        "artifact {name}: sha256 must be 64 lowercase hex chars"
                    ));
                }
                let size = entry.get("size").and_then(|v| v.as_u64()).ok_or_else(|| {
                    format!("artifact {name}: size must be a non-negative integer")
                })?;
                Ok(ReleaseArtifactRecord { name, sha256, size })
            })
            .collect::<Result<_, String>>()?;
    if artifacts.is_empty() || artifacts.len() > RELEASE_ARTIFACT_LIMIT {
        return Err(format!(
            "artifacts must list 1-{RELEASE_ARTIFACT_LIMIT} files"
        ));
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    if artifacts
        .windows(2)
        .any(|pair| pair[0].name == pair[1].name)
    {
        return Err("artifact names must be unique".to_string());
    }
    let manifest_hash = release_manifest_hash_hex(&tag, &artifacts);
    Ok(ValidatedReleaseManifest {
        tag,
        version,
        platforms,
        artifacts,
        manifest_hash,
    })
}

/// Where a submission landed: a fresh log entry, or the index of the
/// identical one already there (re-runs of the pipeline are idempotent).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReleaseRecordOutcome {
    Logged { index: usize },
    Duplicate { index: usize },
}

/// Append a `release_manifest` entry unless the latest entry for this
/// tag already carries the same manifest hash. A CHANGED manifest for a
/// tag is deliberately appended, not rejected: republished artifacts are
/// exactly the history the log exists to evidence. The caller persists.
pub(crate) fn record_release_manifest(
    store: &mut Store,
    manifest: &ValidatedReleaseManifest,
) -> ReleaseRecordOutcome {
    let existing = store
        .log_entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            entry.kind == RELEASE_MANIFEST_KIND
                && serde_json::from_str::<serde_json::Value>(&entry.leaf_json)
                    .ok()
                    .and_then(|leaf| leaf.get("tag").and_then(|t| t.as_str()).map(str::to_string))
                    .as_deref()
                    == Some(manifest.tag.as_str())
        });
    if let Some((index, entry)) = existing {
        let same_hash = serde_json::from_str::<serde_json::Value>(&entry.leaf_json)
            .ok()
            .and_then(|leaf| {
                leaf.get("manifest_hash")
                    .and_then(|h| h.as_str())
                    .map(str::to_string)
            })
            .as_deref()
            == Some(manifest.manifest_hash.as_str());
        if same_hash {
            return ReleaseRecordOutcome::Duplicate { index };
        }
    }
    append_log_entry(
        store,
        RELEASE_MANIFEST_KIND,
        json!({
            "tag": manifest.tag,
            "version": manifest.version,
            "platforms": manifest.platforms,
            "manifest_hash": manifest.manifest_hash,
            "artifacts": manifest.artifacts,
        }),
    );
    ReleaseRecordOutcome::Logged {
        index: store.log_entries.len() - 1,
    }
}

/// `POST /api/log/release-manifest` — the release pipeline commits what
/// it published. Token-authed (`check_release_token`), shape-validated,
/// size-bounded; the log stays public to READ.
pub(crate) async fn release_manifest_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "release_submit", 30, 60_000).await?;
    check_release_token(state.config.release_token.as_deref(), &headers)?;
    if serde_json::to_string(&body)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_RELEASE_MANIFEST_BYTES
    {
        return Err(ApiError::bad_request("release manifest is too large"));
    }
    let manifest = validate_release_manifest(&body).map_err(ApiError::bad_request)?;
    let mut store = state.store.lock().await;
    let (logged, index) = match record_release_manifest(&mut store, &manifest) {
        ReleaseRecordOutcome::Logged { index } => (true, index),
        ReleaseRecordOutcome::Duplicate { index } => (false, index),
    };
    if logged {
        persist_locked(&state, &store)?;
        eprintln!(
            "[connect] release manifest logged: {} ({} artifacts, {})",
            manifest.tag,
            manifest.artifacts.len(),
            &manifest.manifest_hash[..12.min(manifest.manifest_hash.len())],
        );
    }
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "logged": logged,
            "index": index,
            "tag": manifest.tag,
            "manifest_hash": manifest.manifest_hash,
            "size": store.log_entries.len(),
        }))
        .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseManifestQuery {
    #[serde(default)]
    tag: String,
}

/// `GET /api/log/release-manifest[?tag=<tag>]` — the latest release
/// manifest (for a tag, or overall) with everything an out-of-band
/// monitor needs in one response: the exact leaf bytes, its index, an
/// inclusion proof, and the signed tree head — all under one store lock
/// so they cohere (the `log_artifact_manifest` shape).
pub(crate) async fn log_release_manifest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ReleaseManifestQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let tag = query.tag.trim();
    let store = state.store.lock().await;
    let found = store
        .log_entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            if entry.kind != RELEASE_MANIFEST_KIND {
                return false;
            }
            if tag.is_empty() {
                return true;
            }
            serde_json::from_str::<serde_json::Value>(&entry.leaf_json)
                .ok()
                .and_then(|leaf| leaf.get("tag").and_then(|t| t.as_str()).map(str::to_string))
                .as_deref()
                == Some(tag)
        });
    let Some((index, entry)) = found else {
        return Ok(orl_cors(
            Json(json!({ "ok": true, "found": false })).into_response(),
        ));
    };
    let leaves = log_leaves(&store);
    let proof: Vec<String> = log_inclusion_proof(index, &leaves)
        .iter()
        .map(|hash| b64u(hash))
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "found": true,
            "index": index,
            "kind": entry.kind,
            "unix_ms": entry.unix_ms,
            "leaf_json": entry.leaf_json,
            "proof": proof,
            "sth": signed_tree_head_fields(&state, &store),
        }))
        .into_response(),
    ))
}

/// The exact byte string an org root signs over its revocation list —
/// mirrors `access::org::orl_signing_payload` in the daemon. Stable
/// protocol, replicated rather than shared: this binary interprets the
/// list only enough to keep the bulletin board clean.
pub(crate) fn orl_signing_payload(list: &serde_json::Value) -> Option<Vec<u8>> {
    let org = list.get("org")?;
    let join = |key: &str| -> Option<String> {
        Some(
            list.get(key)?
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
    };
    Some(
        format!(
            "intendant-org-orl-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            org.get("handle")?.as_str()?,
            org.get("root_key")?.as_str()?,
            list.get("seq")?.as_u64()?,
            join("revoked_grant_ids")?,
            join("revoked_subjects")?,
            join("revoked_issuer_keys")?,
            list.get("issued_at_unix_ms")?.as_u64()?,
        )
        .into_bytes(),
    )
}

/// These two endpoints are cross-origin public by design: anchor-served
/// dashboards publish and fetch lists here, and the payloads carry their
/// own authority (a root signature) or none (a lookup of public data).
pub(crate) fn orl_cors(response: Response) -> Response {
    let mut response = response;
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    response
}

pub(crate) async fn orl_preflight() -> Response {
    let mut response = axum::http::StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("content-type"),
    );
    response
}

pub(crate) const MAX_ORL_BULLETIN_BYTES: usize = 64 * 1024;

pub(crate) async fn orl_publish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(list): Json<serde_json::Value>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "orl_publish", 30, 60_000).await?;
    if serde_json::to_string(&list)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_ORL_BULLETIN_BYTES
    {
        return Err(ApiError::bad_request("revocation list is too large"));
    }
    if list.get("v").and_then(|v| v.as_u64()) != Some(1)
        || list.get("kind").and_then(|v| v.as_str()) != Some("org-revocations")
    {
        return Err(ApiError::bad_request("not an org revocation list"));
    }
    let handle = list
        .pointer("/org/handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let root_key = list
        .pointer("/org/root_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let seq = list.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
    if handle.is_empty() || root_key.is_empty() {
        return Err(ApiError::bad_request("missing org handle or root key"));
    }
    let payload = orl_signing_payload(&list)
        .ok_or_else(|| ApiError::bad_request("malformed revocation list"))?;
    let key = b64u_decode(&root_key).map_err(|_| ApiError::bad_request("invalid root key"))?;
    let sig = b64u_decode(
        list.get("sig")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim(),
    )
    .map_err(|_| ApiError::bad_request("invalid signature encoding"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&payload, &sig)
        .map_err(|_| ApiError::bad_request("signature verification failed"))?;

    let mut store = state.store.lock().await;
    let now = now_unix_ms();
    let stored = if let Some(existing) = store
        .orl_bulletins
        .iter_mut()
        .find(|b| b.handle == handle && b.root_key == root_key)
    {
        if seq < existing.seq {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!(
                    "stale list: seq {seq} was already superseded by {}",
                    existing.seq
                ),
            ));
        }
        let changed = seq > existing.seq;
        if changed {
            existing.seq = seq;
            existing.list = list;
            existing.updated_unix_ms = now;
        }
        changed
    } else {
        store.orl_bulletins.push(OrlBulletinRecord {
            handle: handle.clone(),
            root_key: root_key.clone(),
            seq,
            list,
            updated_unix_ms: now,
        });
        true
    };
    if stored {
        append_log_entry(
            &mut store,
            "org_orl_published",
            json!({ "handle": handle, "root_key": root_key, "seq": seq }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(orl_cors(
        Json(json!({ "ok": true, "stored": stored, "seq": seq })).into_response(),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrlFetchQuery {
    #[serde(default)]
    handle: String,
    #[serde(default)]
    root_key: String,
}

pub(crate) async fn orl_fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<OrlFetchQuery>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "orl_fetch", 240, 60_000).await?;
    let handle = query.handle.trim();
    let root_key = query.root_key.trim();
    if handle.is_empty() || root_key.is_empty() {
        return Err(ApiError::bad_request("handle and root_key are required"));
    }
    let store = state.store.lock().await;
    let Some(record) = store
        .orl_bulletins
        .iter()
        .find(|b| b.handle == handle && b.root_key == root_key)
    else {
        return Err(ApiError::not_found(
            "no revocation list published for that org",
        ));
    };
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "seq": record.seq,
            "updated_unix_ms": record.updated_unix_ms,
            "orl": record.list,
        }))
        .into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_empty_tree_matches_ct_vector() {
        // RFC 6962: MTH({}) = SHA-256 of the empty string.
        let expected: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(log_tree_root(&[]), expected);
    }

    #[test]
    fn merkle_inclusion_round_trips_all_shapes() {
        let leaves: Vec<[u8; 32]> = (0u8..8)
            .map(|i| log_leaf_hash(&format!("entry-{i}")))
            .collect();
        for size in 1..=leaves.len() {
            let tree = &leaves[..size];
            let root = log_tree_root(tree);
            for index in 0..size {
                let proof = log_inclusion_proof(index, tree);
                assert!(
                    log_verify_inclusion(&tree[index], index, size, &proof, &root),
                    "inclusion must verify at index {index} size {size}"
                );
                // Wrong leaf must fail.
                let wrong = log_leaf_hash("evil");
                assert!(
                    !log_verify_inclusion(&wrong, index, size, &proof, &root),
                    "forged leaf must not verify at index {index} size {size}"
                );
                // Wrong index must fail (when distinguishable).
                if size > 1 {
                    let other = (index + 1) % size;
                    assert!(
                        !log_verify_inclusion(&tree[index], other, size, &proof, &root)
                            || tree[index] == tree[other],
                        "wrong index must not verify ({index} as {other}, size {size})"
                    );
                }
            }
        }
    }

    #[test]
    fn merkle_consistency_round_trips_all_pairs() {
        let leaves: Vec<[u8; 32]> = (0u8..8)
            .map(|i| log_leaf_hash(&format!("entry-{i}")))
            .collect();
        for new_size in 1..=leaves.len() {
            let new_root = log_tree_root(&leaves[..new_size]);
            for old_size in 1..=new_size {
                let old_root = log_tree_root(&leaves[..old_size]);
                let proof = log_consistency_proof(old_size, &leaves[..new_size]);
                assert!(
                    log_verify_consistency(old_size, new_size, &old_root, &new_root, &proof),
                    "consistency must verify {old_size} -> {new_size}"
                );
                // A rewritten history (different old root) must fail.
                let forged = log_leaf_hash("rewritten");
                if old_size < new_size {
                    assert!(
                        !log_verify_consistency(old_size, new_size, &forged, &new_root, &proof),
                        "forged old root must fail {old_size} -> {new_size}"
                    );
                }
            }
        }
    }

    fn test_config(state_root: &Path) -> ServiceConfig {
        ServiceConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public_origin: "https://connect.example.test".to_string(),
            rp_id: "example.test".to_string(),
            data_file: state_root.join("state.json"),
            daemon_token: None,
            release_token: None,
            cookie_secure: true,
            invite_required: false,
            open_daemon_registration: false,
            dns_zone: None,
            dns_ns_name: None,
            dns_listen: None,
        }
    }

    /// Golden manifest hash — TWINNED byte-for-byte in
    /// `bin/caller/hosted_verify.rs` (the verifier replicates the
    /// canonicalization; change both together).
    #[test]
    fn manifest_hash_is_canonical_and_pinned() {
        // Protocol-level golden shared with hosted_verify.rs. These arbitrary
        // records exercise canonicalization; the served-surface test below
        // separately proves that neither path appears in Connect's manifest.
        let artifacts = vec![
            ArtifactRecord {
                path: "/app.html".to_string(),
                sha256: sha256_hex(b"hello"),
            },
            ArtifactRecord {
                path: "/wasm-web/presence_web.js".to_string(),
                sha256: sha256_hex(b"world"),
            },
        ];
        assert_eq!(
            artifacts[0].sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            manifest_hash_hex(&artifacts),
            "d77d51c09215be374511f6763f0c50d6c84726b8ff82d3ac958e1fc5fcf7abf6"
        );
        // Order-sensitive by design: the canonical form is the sorted list.
        let reversed = vec![artifacts[1].clone(), artifacts[0].clone()];
        assert_ne!(manifest_hash_hex(&reversed), manifest_hash_hex(&artifacts));
    }

    #[test]
    fn served_manifest_contains_only_embedded_connect_routes() {
        let dir = tempfile::tempdir().unwrap();
        // These daemon-only files exist in the historical static root but
        // Connect must neither serve nor advertise them.
        std::fs::write(dir.path().join("app.html"), b"hello").unwrap();
        std::fs::create_dir_all(dir.path().join("wasm-web")).unwrap();
        std::fs::write(dir.path().join("wasm-web/presence_web.js"), b"world").unwrap();
        std::fs::write(dir.path().join("vault-kernel.js"), b"kernel").unwrap();
        let config = test_config(dir.path());
        let manifest = served_artifact_manifest(&config);
        let paths: Vec<&str> = manifest.iter().map(|a| a.path.as_str()).collect();
        // The manifest is exactly the path-sorted embedded surface.
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "manifest must be path-sorted");
        let mut embedded = embedded_artifacts(&config);
        embedded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(manifest, embedded);
        for expected in [
            "/",
            "/connect",
            "/access",
            "/trust",
            "/install.sh",
            "/install.ps1",
            "/logo.svg",
            "/favicon.png",
            "/sw.js",
            "/assets/landing/hero.webp",
        ] {
            assert!(paths.contains(&expected), "manifest must cover {expected}");
        }
        for forbidden in ["/app.html", "/wasm-web/presence_web.js", "/vault-kernel.js"] {
            assert!(
                !paths.contains(&forbidden),
                "manifest must exclude {forbidden}"
            );
        }
        // Deterministic: two computations agree (the pages embed only
        // the origin, never a timestamp or nonce).
        assert_eq!(
            manifest_hash_hex(&manifest),
            manifest_hash_hex(&served_artifact_manifest(&config))
        );
        // A disk collision cannot replace an embedded route.
        std::fs::write(dir.path().join("logo.svg"), b"not the logo").unwrap();
        let with_collision = served_artifact_manifest(&config);
        let logo = with_collision
            .iter()
            .find(|a| a.path == "/logo.svg")
            .unwrap();
        assert_eq!(logo.sha256, sha256_hex(LOGO_SVG.as_bytes()));
    }

    #[tokio::test]
    async fn production_router_bytes_match_transparency_manifest() {
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), Store::default());
        let manifest = served_artifact_manifest(&state.config);
        let app = connect_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        for artifact in manifest {
            let response = client
                .get(format!("http://{address}{}", artifact.path))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{}", artifact.path);
            let bytes = response.bytes().await.unwrap();
            assert_eq!(sha256_hex(&bytes), artifact.sha256, "{}", artifact.path);
        }
        server.abort();
    }

    #[test]
    fn artifact_manifest_entries_append_dedupe_and_prove() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        let mut store = Store::default();
        append_log_entry(&mut store, "daemon_claimed", json!({ "daemon_id": "d1" }));

        assert!(record_artifact_manifest(&mut store, &config));
        assert_eq!(store.log_entries.len(), 2);
        // Same bytes → deduplicated.
        assert!(!record_artifact_manifest(&mut store, &config));
        assert_eq!(store.log_entries.len(), 2);
        // A rendered embedded page changes with the configured public origin.
        config.public_origin = "https://connect-v2.example.test".to_string();
        assert!(record_artifact_manifest(&mut store, &config));
        assert_eq!(store.log_entries.len(), 3);

        // Round-trip the latest entry: leaf carries the list, the
        // manifest hash recomputes from it, and the inclusion proof for
        // the new kind verifies against the tree.
        let (index, entry) = store
            .log_entries
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.kind == ARTIFACT_MANIFEST_KIND)
            .unwrap();
        let leaf: serde_json::Value = serde_json::from_str(&entry.leaf_json).unwrap();
        assert_eq!(
            leaf.get("kind").and_then(|v| v.as_str()),
            Some(ARTIFACT_MANIFEST_KIND)
        );
        assert!(leaf.get("unix_ms").and_then(|v| v.as_u64()).is_some());
        assert_eq!(
            leaf.get("bundle_version").and_then(|v| v.as_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            leaf.get("git_sha").and_then(|v| v.as_str()),
            Some(env!("INTENDANT_GIT_SHA"))
        );
        let artifacts: Vec<ArtifactRecord> =
            serde_json::from_value(leaf.get("artifacts").cloned().unwrap()).unwrap();
        assert_eq!(
            leaf.get("manifest_hash").and_then(|v| v.as_str()),
            Some(manifest_hash_hex(&artifacts).as_str()),
            "manifest_hash must recompute from the carried list"
        );
        let connect = artifacts.iter().find(|a| a.path == "/connect").unwrap();
        assert_eq!(
            connect.sha256,
            sha256_hex(connect_page_html(&config.public_origin).as_bytes())
        );
        assert!(artifacts
            .iter()
            .all(|artifact| artifact.path != "/app.html"));

        let leaves = log_leaves(&store);
        let proof = log_inclusion_proof(index, &leaves);
        let root = log_tree_root(&leaves);
        assert!(log_verify_inclusion(
            &log_leaf_hash(&entry.leaf_json),
            index,
            leaves.len(),
            &proof,
            &root,
        ));
    }

    fn release_fixture() -> ValidatedReleaseManifest {
        validate_release_manifest(&json!({
            "tag": "v1.2.3",
            "version": "1.2.3",
            "platforms": ["macos-arm64"],
            "artifacts": [
                { "name": "Intendant-v1.2.3.zip", "sha256": sha256_hex(b"app zip"), "size": 5 },
                { "name": "Intendant-v1.2.3.zip.sha256", "sha256": sha256_hex(b"hash file"), "size": 3 },
            ],
        }))
        .unwrap()
    }

    /// Golden release-manifest hash — TWINNED byte-for-byte in
    /// `bin/caller/hosted_verify.rs` (the verifier replicates the
    /// canonicalization; change both together).
    #[test]
    fn release_manifest_hash_is_canonical_and_pinned() {
        let artifacts = vec![
            ReleaseArtifactRecord {
                name: "Intendant-v1.2.3.zip".to_string(),
                sha256: sha256_hex(b"hello"),
                size: 5,
            },
            ReleaseArtifactRecord {
                name: "Intendant-v1.2.3.zip.sha256".to_string(),
                sha256: sha256_hex(b"world"),
                size: 99,
            },
        ];
        assert_eq!(
            release_manifest_hash_hex("v1.2.3", &artifacts),
            "050b3579a283790ed739544295c4120ab5457a557fefc72ed374847e8af83030"
        );
        // Order-, tag-, and size-sensitive by design.
        let reversed = vec![artifacts[1].clone(), artifacts[0].clone()];
        assert_ne!(
            release_manifest_hash_hex("v1.2.3", &reversed),
            release_manifest_hash_hex("v1.2.3", &artifacts)
        );
        assert_ne!(
            release_manifest_hash_hex("v1.2.4", &artifacts),
            release_manifest_hash_hex("v1.2.3", &artifacts)
        );
        let mut resized = artifacts.clone();
        resized[0].size = 6;
        assert_ne!(
            release_manifest_hash_hex("v1.2.3", &resized),
            release_manifest_hash_hex("v1.2.3", &artifacts)
        );
    }

    #[test]
    fn release_manifest_validation_canonicalizes_and_rejects_bad_shapes() {
        let manifest = release_fixture();
        assert_eq!(manifest.tag, "v1.2.3");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.platforms, vec!["macos-arm64".to_string()]);
        // Artifacts come out name-sorted and the hash covers that order.
        assert_eq!(manifest.artifacts[0].name, "Intendant-v1.2.3.zip");
        assert_eq!(
            manifest.manifest_hash,
            release_manifest_hash_hex(&manifest.tag, &manifest.artifacts)
        );

        let base = json!({
            "tag": "v1.2.3",
            "version": "1.2.3",
            "platforms": ["macos-arm64"],
            "artifacts": [{ "name": "a.zip", "sha256": sha256_hex(b"x"), "size": 1 }],
        });
        let mutate = |key: &str, value: serde_json::Value| {
            let mut body = base.clone();
            body[key] = value;
            body
        };
        assert!(validate_release_manifest(&base).is_ok());
        for bad in [
            mutate("tag", json!("")),
            mutate("tag", json!("-rc1")),
            mutate("tag", json!("v1/../etc")),
            mutate("tag", json!("v".repeat(101))),
            mutate("version", json!("")),
            mutate("platforms", json!([])),
            mutate("platforms", json!(["ok", "bad platform!"])),
            mutate("platforms", json!("macos")),
            mutate("artifacts", json!([])),
            mutate("artifacts", json!("nope")),
            mutate(
                "artifacts",
                json!([{ "name": "a.zip", "sha256": "abc", "size": 1 }]),
            ),
            mutate(
                "artifacts",
                json!([{ "name": "a.zip", "sha256": sha256_hex(b"x").to_uppercase(), "size": 1 }]),
            ),
            mutate(
                "artifacts",
                json!([{ "name": "../evil.zip", "sha256": sha256_hex(b"x"), "size": 1 }]),
            ),
            mutate(
                "artifacts",
                json!([{ "name": "a.zip", "sha256": sha256_hex(b"x") }]),
            ),
            mutate(
                "artifacts",
                json!([
                    { "name": "a.zip", "sha256": sha256_hex(b"x"), "size": 1 },
                    { "name": "a.zip", "sha256": sha256_hex(b"y"), "size": 2 },
                ]),
            ),
        ] {
            assert!(
                validate_release_manifest(&bad).is_err(),
                "must reject {bad}"
            );
        }
    }

    #[test]
    fn release_manifest_entries_append_dedupe_and_prove() {
        let mut store = Store::default();
        append_log_entry(&mut store, "daemon_claimed", json!({ "daemon_id": "d1" }));
        let manifest = release_fixture();

        // First submission appends…
        assert_eq!(
            record_release_manifest(&mut store, &manifest),
            ReleaseRecordOutcome::Logged { index: 1 }
        );
        // …an identical re-run dedupes to the same index…
        assert_eq!(
            record_release_manifest(&mut store, &manifest),
            ReleaseRecordOutcome::Duplicate { index: 1 }
        );
        assert_eq!(store.log_entries.len(), 2);
        // …a different tag appends…
        let mut next = manifest.clone();
        next.tag = "v1.2.4".to_string();
        next.manifest_hash = release_manifest_hash_hex(&next.tag, &next.artifacts);
        assert_eq!(
            record_release_manifest(&mut store, &next),
            ReleaseRecordOutcome::Logged { index: 2 }
        );
        // …and republished artifacts under an EXISTING tag append too
        // (history, not replacement).
        let mut republished = manifest.clone();
        republished.artifacts[0].sha256 = sha256_hex(b"rebuilt zip");
        republished.manifest_hash =
            release_manifest_hash_hex(&republished.tag, &republished.artifacts);
        assert_eq!(
            record_release_manifest(&mut store, &republished),
            ReleaseRecordOutcome::Logged { index: 3 }
        );
        // The republished entry is now the latest for that tag.
        assert_eq!(
            record_release_manifest(&mut store, &republished),
            ReleaseRecordOutcome::Duplicate { index: 3 }
        );

        // Round-trip the entry: the leaf carries the list, the manifest
        // hash recomputes from it, and the inclusion proof verifies.
        let entry = &store.log_entries[1];
        assert_eq!(entry.kind, RELEASE_MANIFEST_KIND);
        let leaf: serde_json::Value = serde_json::from_str(&entry.leaf_json).unwrap();
        assert_eq!(
            leaf.get("kind").and_then(|v| v.as_str()),
            Some(RELEASE_MANIFEST_KIND)
        );
        assert!(leaf.get("unix_ms").and_then(|v| v.as_u64()).is_some());
        assert_eq!(leaf.get("tag").and_then(|v| v.as_str()), Some("v1.2.3"));
        assert_eq!(leaf.get("version").and_then(|v| v.as_str()), Some("1.2.3"));
        assert_eq!(
            leaf.get("platforms")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(1)
        );
        let artifacts: Vec<ReleaseArtifactRecord> =
            serde_json::from_value(leaf.get("artifacts").cloned().unwrap()).unwrap();
        assert_eq!(
            leaf.get("manifest_hash").and_then(|v| v.as_str()),
            Some(release_manifest_hash_hex("v1.2.3", &artifacts).as_str()),
            "manifest_hash must recompute from the carried list"
        );

        let leaves = log_leaves(&store);
        let proof = log_inclusion_proof(1, &leaves);
        let root = log_tree_root(&leaves);
        assert!(log_verify_inclusion(
            &log_leaf_hash(&entry.leaf_json),
            1,
            leaves.len(),
            &proof,
            &root,
        ));
    }

    #[test]
    fn release_token_gate_rejects_missing_and_wrong_tokens() {
        let with_auth = |value: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(
                    axum::http::header::AUTHORIZATION,
                    axum::http::HeaderValue::from_str(value).unwrap(),
                );
            }
            headers
        };
        // No token configured: submissions are OFF (503), even with a
        // guessed header — an unset token must not mean an open endpoint.
        let err = check_release_token(None, &with_auth(Some("Bearer anything"))).unwrap_err();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        let err = check_release_token(Some("  "), &with_auth(None)).unwrap_err();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        // Configured: missing and wrong bearers are 401.
        let err = check_release_token(Some("s3cret"), &with_auth(None)).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        let err = check_release_token(Some("s3cret"), &with_auth(Some("Bearer nope"))).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        let err = check_release_token(Some("s3cret"), &with_auth(Some("s3cret"))).unwrap_err();
        assert_eq!(
            err.status,
            StatusCode::UNAUTHORIZED,
            "bare token without Bearer"
        );
        // The right bearer passes.
        assert!(check_release_token(Some("s3cret"), &with_auth(Some("Bearer s3cret"))).is_ok());
    }

    #[test]
    fn log_sth_signs_and_verifies() {
        use ring::signature::KeyPair as _;
        let mut store = Store::default();
        let keypair = load_or_create_log_keypair(&mut store).unwrap();
        let root = b64u(&log_tree_root(&[log_leaf_hash("x")]));
        let payload = log_sth_payload(1, &root, 123);
        let rng = ring::rand::SystemRandom::new();
        let sig = keypair.sign(&rng, payload.as_bytes()).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            keypair.public_key().as_ref(),
        )
        .verify(payload.as_bytes(), sig.as_ref())
        .expect("STH signature must verify");
        // Key is stable across reloads.
        let reloaded = load_or_create_log_keypair(&mut store).unwrap();
        assert_eq!(
            keypair.public_key().as_ref(),
            reloaded.public_key().as_ref()
        );
    }
}
