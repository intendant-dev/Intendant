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
// stays zero-authority AND becomes checkable about the one thing it
// could quietly lie about: first introductions.

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

// ── Code transparency: the served dashboard bundle, committed ──
//
// The hosted origin's residual power is serving different bytes than it
// claims (trust-architecture.md's "it could serve this page with
// malicious code"). These entries commit what the service SERVES to the
// same append-only log that commits what it SAYS: at startup the service
// hashes every static artifact it can serve — the embedded pages exactly
// as this instance renders them (origin-injected installers included)
// and every file under the static root — and appends an
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
    ];
    for (name, bytes) in LANDING_ASSETS {
        artifacts.push((format!("/assets/landing/{name}"), sha256_hex(bytes)));
    }
    artifacts
        .into_iter()
        .map(|(path, sha256)| ArtifactRecord { path, sha256 })
        .collect()
}

/// Every file under the static root, keyed by the URL path the fallback
/// serves it at. Skips symlinked directories (no cycles) and non-UTF-8
/// names (not expressible as a URL path); unreadable files are skipped
/// with a warning — they would 404 if fetched, too.
pub(crate) fn static_root_artifacts(static_root: &Path) -> Vec<ArtifactRecord> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<ArtifactRecord>) {
        if depth > 16 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(root, &path, depth + 1, out);
                continue;
            }
            // Follow file symlinks (serve_file serves them) but never
            // symlinked directories.
            if !path.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let mut url_path = String::from("/");
            let mut expressible = true;
            for (i, component) in rel.components().enumerate() {
                let Some(part) = component.as_os_str().to_str() else {
                    expressible = false;
                    break;
                };
                if i > 0 {
                    url_path.push('/');
                }
                url_path.push_str(part);
            }
            if !expressible {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => out.push(ArtifactRecord {
                    path: url_path,
                    sha256: sha256_hex(&bytes),
                }),
                Err(error) => {
                    eprintln!(
                        "[connect] artifact manifest skipping unreadable {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
    let mut artifacts = Vec::new();
    walk(static_root, static_root, 0, &mut artifacts);
    artifacts
}

/// The full served-artifact manifest: static-root files plus the
/// embedded routes, sorted by path. On a path collision the embedded
/// artifact wins — routes take precedence over the static fallback
/// (e.g. `/logo.svg` is served embedded even though `static/logo.svg`
/// exists).
pub(crate) fn served_artifact_manifest(config: &ServiceConfig) -> Vec<ArtifactRecord> {
    let mut by_path: std::collections::BTreeMap<String, String> =
        static_root_artifacts(&config.static_root)
            .into_iter()
            .map(|artifact| (artifact.path, artifact.sha256))
            .collect();
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

    fn test_config(static_root: &Path) -> ServiceConfig {
        ServiceConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public_origin: "https://connect.example.test".to_string(),
            rp_id: "example.test".to_string(),
            static_root: static_root.to_path_buf(),
            data_file: static_root.join("state.json"),
            daemon_token: None,
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
    fn served_manifest_walks_static_root_and_embeds_routes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.html"), b"hello").unwrap();
        std::fs::create_dir_all(dir.path().join("wasm-web")).unwrap();
        std::fs::write(dir.path().join("wasm-web/presence_web.js"), b"world").unwrap();
        // The vault crypto kernel deploys into the static root like every
        // other static artifact; the walk MUST cover it — the pinned-kernel
        // design (32-vault-custody.js + static/vault-kernel.js) leans on
        // this manifest for its out-of-band verification story.
        std::fs::write(dir.path().join("vault-kernel.js"), b"kernel").unwrap();
        let config = test_config(dir.path());
        let manifest = served_artifact_manifest(&config);
        let paths: Vec<&str> = manifest.iter().map(|a| a.path.as_str()).collect();
        // Sorted, and the walk found the files at their URL paths.
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "manifest must be path-sorted");
        for expected in [
            "/",
            "/connect",
            "/access",
            "/trust",
            "/install.sh",
            "/install.ps1",
            "/logo.svg",
            "/favicon.png",
            "/assets/landing/hero.webp",
            "/app.html",
            "/wasm-web/presence_web.js",
            "/vault-kernel.js",
        ] {
            assert!(paths.contains(&expected), "manifest must cover {expected}");
        }
        let app = manifest.iter().find(|a| a.path == "/app.html").unwrap();
        assert_eq!(app.sha256, sha256_hex(b"hello"));
        let kernel = manifest
            .iter()
            .find(|a| a.path == "/vault-kernel.js")
            .unwrap();
        assert_eq!(kernel.sha256, sha256_hex(b"kernel"));
        // Deterministic: two computations agree (the pages embed only
        // the origin, never a timestamp or nonce).
        assert_eq!(
            manifest_hash_hex(&manifest),
            manifest_hash_hex(&served_artifact_manifest(&config))
        );
        // The embedded route wins a path collision with the static root.
        std::fs::write(dir.path().join("logo.svg"), b"not the logo").unwrap();
        let with_collision = served_artifact_manifest(&config);
        let logo = with_collision
            .iter()
            .find(|a| a.path == "/logo.svg")
            .unwrap();
        assert_eq!(logo.sha256, sha256_hex(LOGO_SVG.as_bytes()));
    }

    #[test]
    fn artifact_manifest_entries_append_dedupe_and_prove() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.html"), b"bundle-v1").unwrap();
        let config = test_config(dir.path());
        let mut store = Store::default();
        append_log_entry(&mut store, "daemon_claimed", json!({ "daemon_id": "d1" }));

        assert!(record_artifact_manifest(&mut store, &config));
        assert_eq!(store.log_entries.len(), 2);
        // Same bytes → deduplicated.
        assert!(!record_artifact_manifest(&mut store, &config));
        assert_eq!(store.log_entries.len(), 2);
        // Changed bytes → a new entry.
        std::fs::write(dir.path().join("app.html"), b"bundle-v2").unwrap();
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
        assert_eq!(
            artifacts
                .iter()
                .find(|a| a.path == "/app.html")
                .unwrap()
                .sha256,
            sha256_hex(b"bundle-v2")
        );

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
