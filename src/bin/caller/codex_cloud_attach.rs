//! Codex Cloud attachment broker and worker-side agent (attach slice 1).
//!
//! A Codex Cloud worker has no inbound reachability and no durable
//! identity, so attaching it to home inverts the usual pairing: home
//! mints a **single-use, minutes-TTL enrollment token** bound to one
//! task; the token travels to the worker through the only per-task
//! channel that exists (the task prompt — normally a follow-up into the
//! warm worker); the worker generates a keypair in its task-local state
//! root, redeems `{token, public key}` at a public gateway route, and
//! receives a client certificate whose identity record carries the
//! zero-authority `cloud-worker` system profile and a hard expiry. It
//! then dials home's gateway. Direct mTLS is preferred; an explicitly
//! trusted TLS-terminating reverse proxy uses a signed, replay-protected
//! proof from the same enrolled key. The accepted socket *is* the attachment:
//! the lease flips `connected` while it lives, `disconnected` when it dies.
//! Authority over the worker flows home→worker in later slices; the worker's
//! inbound authority on home stays nothing.
//!
//! Private keys never transit (the daemon signs a public key, never
//! mints one for the worker), tokens are stored hashed and burned
//! atomically on first redemption, and an unknown, used, or expired
//! token is indistinguishable in the error surface.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::codex_cloud::{record_attachment_state, state_path, AttachmentState, StoreLock};

const BROKER_VERSION: u32 = 1;
/// Default for a manual attach ceremony. Automatic acquisition extends its
/// one-time token to the separately bounded cold-start deadline plus a small
/// grace window; neither form is a durable credential.
pub(crate) const DEFAULT_TOKEN_TTL_S: u64 = 900;
/// The issued identity's record expiry (independent of cert validity —
/// the record is what the gateway enforces on every connection).
pub(crate) const DEFAULT_IDENTITY_TTL_S: u64 = 3600;
/// The public redemption doorbell's path — the one spelling shared by the
/// route table, the certless carve-out predicate, and the worker's dial.
pub(crate) const ENROLL_PATH: &str = "/api/codex-cloud/enroll";
/// Bounded base64url copy of the enrollment JSON. Some managed egress
/// proxies preserve request headers while dropping POST bodies; the worker
/// sends both, and home requires byte equality whenever both arrive.
pub(crate) const ENROLL_REQUEST_HEADER: &str = "x-intendant-cloud-enrollment";
/// One limit shared by the HTTP body policy and the header fallback decoder.
pub(crate) const ENROLL_REQUEST_MAX_BYTES: usize = 8 * 1024;
/// Dedicated WebSocket target for Cloud attachments. Keeping the
/// application-proof fallback off the dashboard's ordinary `/ws` target
/// makes the zero-authority lane explicit even when a reverse proxy has
/// terminated the worker's client-certificate handshake.
pub(crate) const ATTACH_PATH: &str = "/api/codex-cloud/attach";
/// The worker's first frame after the socket opens. Informational — the
/// identity was already established by the client certificate.
const HELLO_KIND: &str = "cloud-worker-hello";
const ATTACH_PROOF_PROTOCOL: &str = "intendant-cloud-worker-attach-v1";
const ATTACH_PROOF_MAX_SKEW_MS: i64 = 5 * 60 * 1000;
const ATTACH_PROOF_REPLAY_TTL_MS: u64 = 10 * 60 * 1000;
const ATTACH_PROOF_REPLAY_GLOBAL_CAP: usize = 4096;
const ATTACH_PROOF_REPLAY_PER_WORKER_CAP: usize = 64;
/// Stay comfortably below the five-minute idle teardown observed on managed
/// Cloud egress paths. Both endpoints originate pings, so a half-open write
/// fails promptly and long silent commands keep their control socket.
const ATTACHMENT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

const CLOUD_WORKER_HEADER: &str = "x-intendant-cloud-worker";
const CLOUD_WORKER_FINGERPRINT_HEADER: &str = "x-intendant-cloud-worker-fingerprint";
const CLOUD_WORKER_TASK_HEADER: &str = "x-intendant-cloud-worker-task";
const CLOUD_WORKER_NONCE_HEADER: &str = "x-intendant-cloud-worker-nonce";
const CLOUD_WORKER_TIMESTAMP_HEADER: &str = "x-intendant-cloud-worker-timestamp";
const CLOUD_WORKER_PROOF_HEADER: &str = "x-intendant-cloud-worker-proof";

fn attachment_keepalive_timer() -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + ATTACHMENT_KEEPALIVE_INTERVAL,
        ATTACHMENT_KEEPALIVE_INTERVAL,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

// ── Broker store ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct BrokerStore {
    #[serde(default = "broker_version")]
    version: u32,
    /// Pending enrollments keyed by SHA-256(token) hex — the plaintext
    /// token exists only in the mint response and the task prompt.
    #[serde(default)]
    pending: BTreeMap<String, PendingEnrollment>,
    /// Issued identities keyed by client-cert fingerprint. The listener
    /// resolves an attaching socket to its task through this map.
    #[serde(default)]
    bindings: BTreeMap<String, WorkerBinding>,
    /// Hashes of accepted application-layer attachment proof nonces. The
    /// reverse-proxy lane is proof-of-possession, not a replayable bearer:
    /// a signed request is consumed once across daemon/CLI processes.
    #[serde(default)]
    proof_replay: BTreeMap<String, UsedAttachProof>,
}

fn broker_version() -> u32 {
    BROKER_VERSION
}

impl Default for BrokerStore {
    fn default() -> Self {
        Self {
            version: BROKER_VERSION,
            pending: BTreeMap::new(),
            bindings: BTreeMap::new(),
            proof_replay: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEnrollment {
    /// Bound after `codex cloud exec` returns its provider task id. Manual
    /// attach ceremonies mint already bound; automatic acquisition mints
    /// first, delivers the token over stdin, then binds it before redemption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    /// TTL for the identity record minted at redemption.
    pub identity_ttl_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerBinding {
    pub task_id: String,
    pub label: String,
    pub issued_at_unix_ms: u64,
    pub identity_expires_at_unix: i64,
    /// Raw uncompressed P-256 point, base64url. New workers use the same
    /// private key for mTLS and for the proxy-safe attachment proof. `None`
    /// keeps old broker files readable; those identities remain mTLS-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_public_key_b64u: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsedAttachProof {
    fingerprint: String,
    consumed_at_unix_ms: u64,
}

/// The broker store lives beside the lease store and follows the same
/// sidecar-lock discipline (CLI, daemon route, and listener share it).
pub(crate) fn broker_path(lease_store_path: &Path) -> PathBuf {
    lease_store_path.with_file_name("attach-broker.json")
}

fn load_broker(path: &Path) -> Result<BrokerStore, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let store: BrokerStore = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse attach broker store {}: {e}", path.display()))?;
            if store.version != BROKER_VERSION {
                return Err(format!(
                    "unsupported attach broker store version {} in {}",
                    store.version,
                    path.display()
                ));
            }
            Ok(store)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BrokerStore::default()),
        Err(e) => Err(format!("read attach broker store {}: {e}", path.display())),
    }
}

fn save_broker(path: &Path, store: &BrokerStore) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialize attach broker store: {e}"))?;
    crate::file_watcher::atomic_write(path, &bytes)
        .map_err(|e| format!("write attach broker store {}: {e}", path.display()))
}

fn broker_lock(path: &Path) -> Result<StoreLock, String> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    StoreLock::acquire_path(&path.with_file_name(name))
}

fn token_hash(token: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(token.trim().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_token() -> Result<String, String> {
    use base64::Engine as _;
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "generate enrollment token".to_string())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn prune_expired(store: &mut BrokerStore, now_ms: u64) {
    store
        .pending
        .retain(|_, pending| pending.expires_at_unix_ms > now_ms);
    let replay_floor = now_ms.saturating_sub(ATTACH_PROOF_REPLAY_TTL_MS);
    store
        .proof_replay
        .retain(|_, proof| proof.consumed_at_unix_ms >= replay_floor);
}

/// Mint a single-use enrollment token for a task. Returns the plaintext
/// token — the only time it exists outside the caller's delivery path.
pub fn mint_enrollment(
    broker_store_path: &Path,
    task_id: &str,
    token_ttl_s: u64,
    identity_ttl_s: u64,
    now_ms: u64,
) -> Result<(String, PendingEnrollment), String> {
    mint_enrollment_for(
        broker_store_path,
        Some(task_id),
        token_ttl_s,
        identity_ttl_s,
        now_ms,
    )
}

/// Mint the automatic-acquisition ceremony before the provider task id
/// exists. A correct token presented during that small window receives a
/// retryable "binding pending" refusal and is not burned.
pub(crate) fn mint_unbound_enrollment(
    broker_store_path: &Path,
    token_ttl_s: u64,
    identity_ttl_s: u64,
    now_ms: u64,
) -> Result<(String, PendingEnrollment), String> {
    mint_enrollment_for(broker_store_path, None, token_ttl_s, identity_ttl_s, now_ms)
}

fn mint_enrollment_for(
    broker_store_path: &Path,
    task_id: Option<&str>,
    token_ttl_s: u64,
    identity_ttl_s: u64,
    now_ms: u64,
) -> Result<(String, PendingEnrollment), String> {
    let token = random_token()?;
    let pending = PendingEnrollment {
        task_id: task_id.map(str::to_string),
        created_at_unix_ms: now_ms,
        expires_at_unix_ms: now_ms.saturating_add(token_ttl_s.saturating_mul(1000)),
        identity_ttl_s,
    };
    let _lock = broker_lock(broker_store_path)?;
    let mut store = load_broker(broker_store_path)?;
    prune_expired(&mut store, now_ms);
    store.pending.insert(token_hash(&token), pending.clone());
    save_broker(broker_store_path, &store)?;
    Ok((token, pending))
}

const ENROLLMENT_BINDING_PENDING: &str =
    "automatic enrollment is waiting for its provider task id; retry shortly";

pub(crate) fn enrollment_binding_pending(error: &str) -> bool {
    error == ENROLLMENT_BINDING_PENDING
}

/// Bind a still-pending automatic token to the provider task returned by
/// `codex cloud exec`. The plaintext token remains only in the acquiring
/// task; the broker continues to store its hash.
pub(crate) fn bind_enrollment(
    broker_store_path: &Path,
    token: &str,
    task_id: &str,
    now_ms: u64,
) -> Result<(), String> {
    if task_id.trim().is_empty() || task_id.len() > 256 {
        return Err("provider returned an invalid task id for enrollment".to_string());
    }
    let _lock = broker_lock(broker_store_path)?;
    let mut store = load_broker(broker_store_path)?;
    prune_expired(&mut store, now_ms);
    let pending = store
        .pending
        .get_mut(&token_hash(token))
        .ok_or_else(|| "automatic enrollment expired before its task was created".to_string())?;
    match pending.task_id.as_deref() {
        None => pending.task_id = Some(task_id.to_string()),
        Some(existing) if existing == task_id => {}
        Some(_) => return Err("automatic enrollment was already bound to another task".to_string()),
    }
    save_broker(broker_store_path, &store)
}

/// Atomically burn a token. Unknown, already-used, and expired tokens are
/// deliberately indistinguishable.
fn consume_enrollment(
    broker_store_path: &Path,
    token: &str,
    now_ms: u64,
) -> Result<PendingEnrollment, String> {
    const REFUSED: &str = "enrollment token was not found, already used, or expired";
    let _lock = broker_lock(broker_store_path)?;
    let mut store = load_broker(broker_store_path)?;
    prune_expired(&mut store, now_ms);
    let hash = token_hash(token);
    let pending = store
        .pending
        .get(&hash)
        .cloned()
        .ok_or_else(|| REFUSED.to_string())?;
    if pending.task_id.is_none() {
        return Err(ENROLLMENT_BINDING_PENDING.to_string());
    }
    store.pending.remove(&hash);
    save_broker(broker_store_path, &store)?;
    if pending.expires_at_unix_ms <= now_ms {
        return Err(REFUSED.to_string());
    }
    Ok(pending)
}

fn record_binding(
    broker_store_path: &Path,
    fingerprint: &str,
    binding: WorkerBinding,
) -> Result<(), String> {
    let _lock = broker_lock(broker_store_path)?;
    let mut store = load_broker(broker_store_path)?;
    store.bindings.insert(fingerprint.to_string(), binding);
    save_broker(broker_store_path, &store)
}

/// The listener's fingerprint → task resolution for an attaching socket.
pub(crate) fn binding_for_fingerprint(
    broker_store_path: &Path,
    fingerprint: &str,
) -> Option<WorkerBinding> {
    load_broker(broker_store_path)
        .ok()?
        .bindings
        .get(fingerprint)
        .cloned()
}

/// Whether this WebSocket request explicitly names the reserved Cloud-worker
/// lane. Browsers cannot set arbitrary WebSocket headers; native callers that
/// do set it either authenticate as a cloud-worker or fail closed instead of
/// falling through to dashboard authentication.
pub(crate) fn proxy_attachment_requested(header_text: &str) -> bool {
    crate::web_gateway::http_header_value(header_text, CLOUD_WORKER_HEADER).is_some()
}

/// Verify one application-layer Cloud-worker attachment proof and consume its
/// nonce. This is the narrow fallback for an explicitly trusted HTTPS reverse
/// proxy that terminates TLS before Intendant, so the worker's mTLS certificate
/// cannot reach the listener. The result is only a broker fingerprint; the
/// listener routes it directly to [`serve_attachment_socket`] and never turns
/// it into a dashboard principal or grant.
pub(crate) fn verify_proxy_attachment_request(
    header_text: &str,
    now_ms: u64,
) -> Result<String, String> {
    let lease_store = state_path();
    verify_proxy_attachment_request_at(&broker_path(&lease_store), header_text, now_ms)
}

fn verify_proxy_attachment_request_at(
    broker_path: &Path,
    header_text: &str,
    now_ms: u64,
) -> Result<String, String> {
    let mut request = header_text.lines().next().unwrap_or("").split_whitespace();
    if request.next() != Some("GET") || request.next() != Some(ATTACH_PATH) {
        return Err(format!(
            "cloud-worker proof is valid only on GET {ATTACH_PATH}"
        ));
    }
    if crate::web_gateway::http_header_value(header_text, CLOUD_WORKER_HEADER) != Some("1") {
        return Err("cloud-worker marker is invalid".to_string());
    }
    let required = |name: &str| {
        crate::web_gateway::http_header_value(header_text, name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing {name} header"))
    };
    let fingerprint = required(CLOUD_WORKER_FINGERPRINT_HEADER)?.to_ascii_lowercase();
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("cloud-worker fingerprint has an invalid shape".to_string());
    }
    let task_id = required(CLOUD_WORKER_TASK_HEADER)?;
    if task_id.len() > 256 {
        return Err("cloud-worker task id is too long".to_string());
    }
    let nonce = required(CLOUD_WORKER_NONCE_HEADER)?;
    if nonce.len() != 43
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("cloud-worker proof nonce has an invalid shape".to_string());
    }
    let timestamp_unix_ms: i64 = required(CLOUD_WORKER_TIMESTAMP_HEADER)?
        .parse()
        .map_err(|_| "cloud-worker proof timestamp is invalid".to_string())?;
    let now_i64 = i64::try_from(now_ms).unwrap_or(i64::MAX);
    if now_i64.saturating_sub(timestamp_unix_ms).unsigned_abs() > ATTACH_PROOF_MAX_SKEW_MS as u64 {
        return Err(format!(
            "cloud-worker proof timestamp is outside the {ATTACH_PROOF_MAX_SKEW_MS}ms window"
        ));
    }
    let signature_b64u = required(CLOUD_WORKER_PROOF_HEADER)?;
    use base64::Engine as _;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature_b64u)
        .map_err(|_| "cloud-worker proof signature is invalid base64url".to_string())?;
    if !(64..=80).contains(&signature.len()) {
        return Err("cloud-worker proof signature has an invalid shape".to_string());
    }

    let _lock = broker_lock(broker_path)?;
    let mut store = load_broker(broker_path)?;
    prune_expired(&mut store, now_ms);
    let binding = store
        .bindings
        .get(&fingerprint)
        .cloned()
        .ok_or_else(|| "cloud-worker proof does not name an active binding".to_string())?;
    if binding.task_id != task_id {
        return Err("cloud-worker proof task binding does not match".to_string());
    }
    if binding.identity_expires_at_unix <= now_i64 / 1000 {
        return Err("cloud-worker proof identity has expired".to_string());
    }
    let public_key_b64u = binding
        .proof_public_key_b64u
        .as_deref()
        .ok_or_else(|| "cloud-worker identity supports mTLS attachment only".to_string())?;
    let public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(public_key_b64u)
        .map_err(|_| "cloud-worker proof public key is invalid".to_string())?;
    if public_key.len() != 65 || public_key.first() != Some(&0x04) {
        return Err("cloud-worker proof public key has an invalid shape".to_string());
    }
    let payload =
        attachment_proof_payload(ATTACH_PATH, &fingerprint, task_id, nonce, timestamp_unix_ms);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, public_key)
        .verify(payload.as_bytes(), &signature)
        .map_err(|_| "cloud-worker proof signature verification failed".to_string())?;

    let replay_key = attach_proof_replay_key(&fingerprint, nonce);
    if store.proof_replay.contains_key(&replay_key) {
        return Err("cloud-worker proof nonce was already used".to_string());
    }
    if store.proof_replay.len() >= ATTACH_PROOF_REPLAY_GLOBAL_CAP
        || store
            .proof_replay
            .values()
            .filter(|proof| proof.fingerprint == fingerprint)
            .count()
            >= ATTACH_PROOF_REPLAY_PER_WORKER_CAP
    {
        return Err("cloud-worker proof replay window is full".to_string());
    }
    store.proof_replay.insert(
        replay_key,
        UsedAttachProof {
            fingerprint: fingerprint.clone(),
            consumed_at_unix_ms: now_ms,
        },
    );
    save_broker(broker_path, &store)?;
    Ok(fingerprint)
}

fn attachment_proof_payload(
    target: &str,
    fingerprint: &str,
    task_id: &str,
    nonce: &str,
    timestamp_unix_ms: i64,
) -> String {
    format!(
        "{ATTACH_PROOF_PROTOCOL}\nGET\n{target}\n{fingerprint}\n{task_id}\n{nonce}\n{timestamp_unix_ms}"
    )
}

fn attach_proof_replay_key(fingerprint: &str, nonce: &str) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(fingerprint.as_bytes());
    digest.update(b"\0");
    digest.update(nonce.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── Redemption (the public enroll route's core) ────────────────────────

/// Doorbell limiter, per-source first (the pairing doorbell's design in
/// `peer::access_request::enforce_create_rate_limits`): one cheap scanner
/// must not be able to hold every legitimate worker at 429, so each
/// source gets its own sliding window and the global window is only the
/// wider backstop. The single-use token stays the real gate; this keeps
/// the store from being ground. Emptied source queues are dropped so the
/// map cannot grow without bound under rotating sources.
struct EnrollRateLimiter {
    global: std::collections::VecDeque<u64>,
    per_source: std::collections::HashMap<String, std::collections::VecDeque<u64>>,
}

static ENROLL_RATE: std::sync::OnceLock<std::sync::Mutex<EnrollRateLimiter>> =
    std::sync::OnceLock::new();
const ENROLL_RATE_WINDOW_MS: u64 = 60_000;
const ENROLL_RATE_GLOBAL_MAX: usize = 60;
const ENROLL_RATE_PER_SOURCE_MAX: usize = 10;

fn prune_rate_window(queue: &mut std::collections::VecDeque<u64>, now_ms: u64) {
    while let Some(at_ms) = queue.front().copied() {
        if now_ms.saturating_sub(at_ms) < ENROLL_RATE_WINDOW_MS {
            break;
        }
        queue.pop_front();
    }
}

pub(crate) fn enroll_rate_ok(source: &str, now_ms: u64) -> bool {
    let limiter = ENROLL_RATE.get_or_init(|| {
        std::sync::Mutex::new(EnrollRateLimiter {
            global: std::collections::VecDeque::new(),
            per_source: std::collections::HashMap::new(),
        })
    });
    let mut limiter = limiter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    limiter.per_source.retain(|_, queue| {
        prune_rate_window(queue, now_ms);
        !queue.is_empty()
    });
    prune_rate_window(&mut limiter.global, now_ms);
    if limiter.global.len() >= ENROLL_RATE_GLOBAL_MAX {
        return false;
    }
    let source_queue = limiter.per_source.entry(source.to_string()).or_default();
    prune_rate_window(source_queue, now_ms);
    if source_queue.len() >= ENROLL_RATE_PER_SOURCE_MAX {
        return false;
    }
    source_queue.push_back(now_ms);
    limiter.global.push_back(now_ms);
    true
}

/// Version of the whole home↔worker attachment contract: the enroll body,
/// the WebSocket hello/proof ceremony, and the closed frame vocabulary in
/// both directions. Additive changes (new optional fingerprint fields, new
/// reply kinds the other end drops) do NOT bump it; bump it on any change
/// an older binary on the other end would misread. This is the number a
/// release-asset worker pins: `redeem_enrollment`'s refusal string is the
/// exact text such a worker surfaces as its enrollment failure, so keep it
/// actionable (it must name the environment repin).
pub const ATTACHMENT_PROTOCOL_VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    pub version: u32,
    pub token: String,
    /// SPKI public key PEM. The daemon signs it — a private key never
    /// rides this request in either direction.
    pub public_key_pem: String,
    /// Optional worker-authored runtime fingerprint (hostname/boot id),
    /// recorded on the lease like a probe's.
    #[serde(default)]
    pub worker: Option<serde_json::Value>,
}

/// Same hygiene as the follow-up lane's `CodexAuth`: the token is a live
/// secret until burned, so no derive may ever print it.
impl std::fmt::Debug for EnrollRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollRequest")
            .field("version", &self.version)
            .field("token", &"[redacted]")
            .field("public_key_pem_len", &self.public_key_pem.len())
            .field("worker", &self.worker.is_some())
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub version: u32,
    pub task_id: String,
    pub profile: String,
    pub client_cert_pem: String,
    pub identity_expires_at_unix: i64,
    pub hello_kind: String,
}

/// Redeem a token for a `cloud-worker` identity. Pure of environment:
/// every root arrives as a parameter (the route/CLI edges resolve them).
pub fn redeem_enrollment(
    cert_dir: &Path,
    lease_store_path: &Path,
    request: &EnrollRequest,
    now_ms: u64,
) -> Result<EnrollResponse, String> {
    if request.version != ATTACHMENT_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported enrollment request version {}: this daemon speaks {ATTACHMENT_PROTOCOL_VERSION} — repin INTENDANT_CLOUD_BINARY_URL/_SHA256 to a matching release, or rebuild the worker from source",
            request.version
        ));
    }
    let token = request.token.trim();
    if token.is_empty() || token.len() > 128 {
        return Err("enrollment token has an invalid shape".to_string());
    }
    let public_key_pem = request.public_key_pem.trim();
    if public_key_pem.len() > 4096 {
        return Err("public key PEM is too large".to_string());
    }
    use rcgen::PublicKeyData as _;
    let proof_public_key = rcgen::SubjectPublicKeyInfo::from_pem(public_key_pem)
        .map_err(|e| format!("public_key_pem is not a valid SPKI public key: {e}"))?;
    if proof_public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256
        || proof_public_key.der_bytes().len() != 65
        || proof_public_key.der_bytes().first() != Some(&0x04)
    {
        return Err("cloud-worker public key must be an uncompressed P-256 key".to_string());
    }
    let proof_public_key_b64u = crate::daemon_identity::b64u(proof_public_key.der_bytes());

    let broker = broker_path(lease_store_path);
    let pending = consume_enrollment(&broker, token, now_ms)?;
    let task_id = pending
        .task_id
        .expect("consume_enrollment refuses an unbound automatic token");

    let profile = crate::access::access_policy::CLOUD_WORKER_PROFILE;
    let label = format!("{profile} {task_id}");
    let cert_pem = crate::access::certs::issue_client_certificate_for_public_key(
        cert_dir,
        &label,
        public_key_pem,
    )
    .map_err(|e| format!("issue cloud-worker certificate: {e}"))?;
    let fingerprint = crate::access::access_policy::fingerprint_pem(&cert_pem)
        .map_err(|e| format!("fingerprint issued certificate: {e}"))?;
    let identity_expires_at_unix =
        ((now_ms / 1000) as i64).saturating_add(pending.identity_ttl_s as i64);
    crate::access::access_policy::write_approved_identity_expiring(
        cert_dir,
        &fingerprint,
        &label,
        profile,
        None,
        None,
        Some(identity_expires_at_unix),
    )
    .map_err(|e| format!("record cloud-worker identity: {e}"))?;
    record_binding(
        &broker,
        &fingerprint,
        WorkerBinding {
            task_id: task_id.clone(),
            label,
            issued_at_unix_ms: now_ms,
            identity_expires_at_unix,
            proof_public_key_b64u: Some(proof_public_key_b64u),
        },
    )?;
    // The ceremony is under way: the lease shows `awaiting` until the
    // socket actually opens. Best-effort — an untracked task still
    // enrolls (the lease may live on another store generation).
    let _ = record_attachment_state(lease_store_path, &task_id, AttachmentState::Awaiting);
    if let Some(worker) = &request.worker {
        record_worker_json(lease_store_path, &task_id, worker);
    }
    Ok(EnrollResponse {
        version: ATTACHMENT_PROTOCOL_VERSION,
        task_id,
        profile: profile.to_string(),
        client_cert_pem: cert_pem,
        identity_expires_at_unix,
        hello_kind: HELLO_KIND.to_string(),
    })
}

/// Best-effort: fold a worker-authored fingerprint JSON into the lease
/// (same semantics as a probe collection).
fn record_worker_json(lease_store_path: &Path, task_id: &str, value: &serde_json::Value) {
    let Ok(fingerprint) =
        serde_json::from_value::<crate::codex_cloud::WorkerFingerprint>(value.clone())
    else {
        return;
    };
    crate::codex_cloud::record_worker_fingerprint(lease_store_path, task_id, fingerprint);
}

/// Fingerprint metadata is informational and can be larger than the
/// security-critical enrollment request. Carry it on the authenticated
/// attachment socket so the duplicated enrollment body/header stays below
/// managed-proxy header limits.
fn worker_hello_fingerprint(
    task_id: &str,
    value: &serde_json::Value,
) -> Option<crate::codex_cloud::WorkerFingerprint> {
    if value.get("v").and_then(serde_json::Value::as_u64) != Some(2)
        || value.get("kind").and_then(serde_json::Value::as_str) != Some(HELLO_KIND)
        || value.get("task_id").and_then(serde_json::Value::as_str) != Some(task_id)
    {
        return None;
    }
    serde_json::from_value(value.get("worker")?.clone()).ok()
}

// ── Attachment frames + link registry (slice 2) ────────────────────────

/// Worker→home reply kinds. The worker's inbound authority on home is
/// nothing: frames off the socket are routed only to dashboard
/// subscribers of this task's cloud host, never into home's own frame
/// dispatch, and any kind outside this list is dropped on the floor.
const WORKER_REPLY_KINDS: &[&str] = &[
    "terminal_opened",
    "terminal_output",
    "terminal_exited",
    "terminal_error",
    "terminal_shared",
    "display_opened",
    "display_tiles",
    "display_closed",
    "display_error",
    "cu_result",
    crate::remote_compute::REMOTE_COMMAND_RESULT_KIND,
    crate::remote_compute::source::REMOTE_SOURCE_RESULT_KIND,
];

/// Host-id prefix that routes a dashboard terminal frame to a connected
/// cloud worker's attachment instead of the local terminal registry.
pub(crate) const CLOUD_HOST_PREFIX: &str = "cloud:";

/// `Some(task_id)` when a dashboard `host_id` addresses a cloud worker.
pub(crate) fn cloud_host_task_id(host_id: &str) -> Option<&str> {
    host_id
        .strip_prefix(CLOUD_HOST_PREFIX)
        .filter(|task| !task.is_empty())
}

const LINK_TO_WORKER_CAP: usize = 64;
const LINK_FROM_WORKER_CAP: usize = 256;

/// One connected worker's live frame channels: bounded home→worker sender
/// plus a broadcast of the worker's (filtered) reply frames.
struct AttachmentLink {
    to_worker: tokio::sync::mpsc::Sender<String>,
    from_worker: tokio::sync::broadcast::Sender<String>,
}

static ATTACHMENT_LINKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, AttachmentLink>>,
> = std::sync::OnceLock::new();

fn attachment_links() -> &'static std::sync::Mutex<std::collections::HashMap<String, AttachmentLink>>
{
    ATTACHMENT_LINKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The bridge's handle on a connected worker: a sender for home→worker
/// frames and a fresh subscription to its reply frames. `None` when the
/// task has no live attachment.
pub(crate) fn attachment_channel(
    task_id: &str,
) -> Option<(
    tokio::sync::mpsc::Sender<String>,
    tokio::sync::broadcast::Receiver<String>,
)> {
    let links = attachment_links()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    links
        .get(task_id)
        .map(|link| (link.to_worker.clone(), link.from_worker.subscribe()))
}

// ── The attachment socket (listener lane) ──────────────────────────────

/// Serve one accepted cloud-worker WebSocket: resolve its task binding,
/// flip the lease `connected`, register the frame link, and pump frames
/// until the socket ends or the identity expires — then flip
/// `disconnected`. The socket doubles as the heartbeat; tungstenite
/// answers pings during the read loop.
pub(crate) async fn serve_attachment_socket<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    fingerprint: &str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt as _, StreamExt as _};

    let lease_store = state_path();
    let broker = broker_path(&lease_store);
    let Some(binding) = binding_for_fingerprint(&broker, fingerprint) else {
        eprintln!(
            "[codex-cloud] refusing attachment from unbound cloud-worker certificate {}",
            &fingerprint[..fingerprint.len().min(12)]
        );
        return;
    };
    let task_id = binding.task_id;
    match record_attachment_state(&lease_store, &task_id, AttachmentState::Connected) {
        Ok(_) => eprintln!("[codex-cloud] worker attached for {task_id}"),
        Err(error) => eprintln!("[codex-cloud] attachment connect for {task_id}: {error}"),
    }
    let (to_worker_tx, mut to_worker_rx) = tokio::sync::mpsc::channel::<String>(LINK_TO_WORKER_CAP);
    let (from_worker_tx, _) = tokio::sync::broadcast::channel::<String>(LINK_FROM_WORKER_CAP);
    {
        let mut links = attachment_links()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        links.insert(
            task_id.clone(),
            AttachmentLink {
                to_worker: to_worker_tx.clone(),
                from_worker: from_worker_tx.clone(),
            },
        );
    }
    // The identity's hard expiry bounds the live socket too — admission
    // checks the record, but a socket opened at minute 59 must not hold
    // the attachment past the hour. Time-boxed means the clock wins.
    let remaining_ms = attachment_remaining_ms(
        binding.identity_expires_at_unix,
        crate::codex_cloud::now_unix_ms(),
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(remaining_ms);
    let (mut write, mut read) = ws_stream.split();
    let mut keepalive = attachment_keepalive_timer();
    loop {
        tokio::select! {
            frame = read.next() => match frame {
                None => break,
                Some(Ok(message)) if message.is_close() => break,
                Some(Ok(message)) => {
                    if let Ok(text) = message.into_text() {
                        route_worker_frame(&task_id, &from_worker_tx, text.as_str());
                    }
                }
                Some(Err(_)) => break,
            },
            outbound = to_worker_rx.recv() => match outbound {
                Some(text) => {
                    if write
                        .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                None => break,
            },
            _ = keepalive.tick() => {
                if write
                    .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            },
            _ = tokio::time::sleep_until(deadline) => {
                eprintln!(
                    "[codex-cloud] cloud-worker identity for {task_id} expired; closing the attachment"
                );
                let _ = write.close().await;
                break;
            }
        }
    }
    {
        // Remove only our own link: a reconnect may already have replaced
        // it, and the replacement must survive this socket's teardown.
        let mut links = attachment_links()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if links
            .get(&task_id)
            .is_some_and(|link| link.to_worker.same_channel(&to_worker_tx))
        {
            links.remove(&task_id);
        }
    }
    match record_attachment_state(&lease_store, &task_id, AttachmentState::Disconnected) {
        Ok(_) => eprintln!("[codex-cloud] worker detached for {task_id}"),
        Err(error) => eprintln!("[codex-cloud] attachment disconnect for {task_id}: {error}"),
    }
}

/// Route one worker frame: a live job's capability-bound cache requests take
/// their private bounded lane; the authenticated hello records informational
/// worker provenance; ordinary reply kinds fan out to subscribers; everything
/// else is dropped without dispatch.
fn route_worker_frame(
    task_id: &str,
    from_worker_tx: &tokio::sync::broadcast::Sender<String>,
    text: &str,
) {
    if crate::remote_compute::route_remote_cache_frame(task_id, text) {
        return;
    }
    let value = serde_json::from_str::<serde_json::Value>(text).ok();
    if let Some(fingerprint) = value
        .as_ref()
        .and_then(|value| worker_hello_fingerprint(task_id, value))
    {
        crate::codex_cloud::record_worker_fingerprint(
            &crate::codex_cloud::state_path(),
            task_id,
            fingerprint,
        );
        return;
    }
    let kind = value.and_then(|value| {
        value
            .get("t")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    });
    if kind
        .as_deref()
        .is_some_and(|kind| WORKER_REPLY_KINDS.contains(&kind))
    {
        // No subscribers is fine — output before any dashboard attaches
        // simply has no audience.
        let _ = from_worker_tx.send(text.to_string());
    }
}

/// Milliseconds of socket lifetime left before the identity's record
/// expiry — zero for an already-expired identity, so the caller's
/// deadline fires immediately.
fn attachment_remaining_ms(identity_expires_at_unix: i64, now_ms: u64) -> u64 {
    u64::try_from(identity_expires_at_unix)
        .unwrap_or(0)
        .saturating_mul(1000)
        .saturating_sub(now_ms)
}

// ── Attach verb (home side) ────────────────────────────────────────────

pub(crate) const TLS_TERMINATED_PROXY_ENV: &str = "INTENDANT_CODEX_CLOUD_TLS_TERMINATED_PROXY";

pub(crate) fn tls_terminated_proxy_from_env() -> bool {
    std::env::var(TLS_TERMINATED_PROXY_ENV)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

pub(crate) fn home_url_from(args_value: Option<String>) -> Result<String, String> {
    let url = args_value
        .or_else(|| std::env::var("INTENDANT_CODEX_CLOUD_HOME_URL").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "attach needs the daemon's reachable WSS URL: pass --home-url wss://host:port or set INTENDANT_CODEX_CLOUD_HOME_URL"
                .to_string()
        })?;
    let mut parsed = url::Url::parse(&url).map_err(|e| format!("invalid home URL: {e}"))?;
    // wss:// only: both direct mTLS and an explicitly trusted
    // TLS-terminating proxy need an encrypted public hop.
    if parsed.scheme() != "wss" {
        return Err(format!(
            "home URL must be wss://… (enrollment and attachment require TLS), got '{url}'"
        ));
    }
    if parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("home URL must contain only wss://host[:port][/path]".to_string());
    }
    // Old examples named `/ws`; the dedicated path is listener-compatible
    // with old direct-mTLS daemons and sharply scopes the proxy proof on new
    // ones, so normalize rather than making every operator rewrite config.
    parsed.set_path(ATTACH_PATH);
    Ok(parsed.to_string())
}

fn enrollment_url_from_home(home: &str) -> Result<String, String> {
    let mut parsed = url::Url::parse(home).map_err(|e| format!("invalid home URL: {e}"))?;
    parsed
        .set_scheme("https")
        .map_err(|_| "home URL cannot be converted to HTTPS".to_string())?;
    // Enrollment and attachment share one origin but deliberately use
    // different routes. Replace the normalized attachment path instead of
    // appending to it.
    parsed.set_path(ENROLL_PATH);
    Ok(parsed.to_string())
}

/// The attach follow-up prompt: everything in it is public except the
/// single-use token, which is the point of the ceremony.
pub fn attach_prompt(
    task_id: &str,
    home_url: &str,
    server_fingerprint: Option<&str>,
    token: &str,
    tls_terminated_proxy: bool,
) -> String {
    let transport_args = if tls_terminated_proxy {
        "--tls-terminated-proxy".to_string()
    } else {
        format!(
            "--home-fingerprint {}",
            server_fingerprint.expect("direct mTLS prompt needs the server fingerprint")
        )
    };
    format!(
        "Run this exact command in the repository root and keep it running in the foreground until this turn is cancelled. Do not modify any files, do not run builds, and do not print the token back:\n\n\
```bash\n\
\"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/libexec/intendant-cloud/run-worker.sh\" -- \\\n\
  \"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/bin/intendant\" codex-cloud agent \\\n\
  --home {home_url} {transport_args} --task {task_id} --token-stdin <<'INTENDANT_ATTACH_TOKEN'\n\
{token}\n\
INTENDANT_ATTACH_TOKEN\n\
```\n\n\
If the command fails because a destination is blocked by the environment's network allowlist, report the blocked host and stop."
    )
}

/// Initial task prompt for automatic acquisition. Unlike a manual follow-up,
/// the task id is not known yet; the agent learns and verifies it from the
/// enrollment response after home binds the token.
pub(crate) fn automatic_attach_prompt(
    home_url: &str,
    server_fingerprint: Option<&str>,
    token: &str,
    tls_terminated_proxy: bool,
) -> String {
    let transport_args = if tls_terminated_proxy {
        "--tls-terminated-proxy".to_string()
    } else {
        format!(
            "--home-fingerprint {}",
            server_fingerprint.expect("direct mTLS prompt needs the server fingerprint")
        )
    };
    format!(
        "Run this exact command in the repository root and keep it running in the foreground until home retires the worker. Do not modify repository files, do not run builds on your own, and do not print the token back:\n\n\
```bash\n\
\"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/libexec/intendant-cloud/run-worker.sh\" -- \\\n\
  \"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/bin/intendant\" codex-cloud agent \\\n\
  --home {home_url} {transport_args} --token-stdin <<'INTENDANT_ATTACH_TOKEN'\n\
{token}\n\
INTENDANT_ATTACH_TOKEN\n\
```\n\n\
If enrollment says its task binding is pending, keep retrying as the agent command instructs. If a destination is blocked by the environment's network allowlist, report the blocked host and stop."
    )
}

pub async fn run_attach(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage:\n  intendant codex-cloud attach TASK_ID [--home-url wss://host:port] [--tls-terminated-proxy] [--token-ttl-s {DEFAULT_TOKEN_TTL_S}] [--identity-ttl-s {DEFAULT_IDENTITY_TTL_S}] [--send] [--json]"
        );
        println!(
            "Mints a single-use enrollment token bound to the task and composes the attach prompt (printed by default; --send delivers it as a follow-up turn into the warm worker). The worker redeems the token for a zero-authority cloud-worker certificate and dials back; the lease's attachment lane tracks the socket. --tls-terminated-proxy explicitly trusts the home URL's WebPKI HTTPS reverse proxy and authenticates the worker with a signed, replay-protected application proof when mTLS cannot pass through."
        );
        return Ok(());
    }
    let mut task_id: Option<String> = None;
    let mut home_url_arg: Option<String> = None;
    let mut token_ttl_s = DEFAULT_TOKEN_TTL_S;
    let mut identity_ttl_s = DEFAULT_IDENTITY_TTL_S;
    let mut send = false;
    let mut json = false;
    let mut tls_terminated_proxy = tls_terminated_proxy_from_env();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--home-url" => {
                i += 1;
                home_url_arg = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--home-url requires a wss:// URL")?,
                );
            }
            "--token-ttl-s" => {
                i += 1;
                token_ttl_s = args
                    .get(i)
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .ok_or("--token-ttl-s requires a positive number of seconds")?;
            }
            "--identity-ttl-s" => {
                i += 1;
                identity_ttl_s = args
                    .get(i)
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .ok_or("--identity-ttl-s requires a positive number of seconds")?;
            }
            "--send" => send = true,
            "--json" => json = true,
            "--tls-terminated-proxy" => tls_terminated_proxy = true,
            other if task_id.is_none() && !other.starts_with('-') => {
                task_id = Some(other.to_string());
            }
            other => return Err(format!("unknown attach argument {other}")),
        }
        i += 1;
    }
    let task_id = task_id.ok_or("attach requires a Codex Cloud task id")?;
    let home_url = home_url_from(home_url_arg)?;
    let lease_store = state_path();
    let tracked = crate::codex_cloud::cached_leases(&lease_store)?
        .iter()
        .any(|lease| lease.task_id == task_id);
    if !tracked {
        return Err(format!(
            "task {task_id} is not in the local lease store — `intendant codex-cloud list` (or `status {task_id}`) tracks it first"
        ));
    }
    let server_fingerprint = if tls_terminated_proxy {
        None
    } else {
        let cert_dir = crate::access::backend::select_backend().cert_dir();
        Some(
            crate::access::certs::read_server_cert_fingerprint(&cert_dir).ok_or_else(|| {
                "no gateway server certificate found — start the daemon once (it mints the TLS identity workers must pin), then retry"
                    .to_string()
            })?,
        )
    };

    let broker = broker_path(&lease_store);
    let (token, pending) = mint_enrollment(
        &broker,
        &task_id,
        token_ttl_s,
        identity_ttl_s,
        crate::codex_cloud::now_unix_ms(),
    )?;
    let _ = record_attachment_state(&lease_store, &task_id, AttachmentState::Awaiting);
    let prompt = attach_prompt(
        &task_id,
        &home_url,
        server_fingerprint.as_deref(),
        &token,
        tls_terminated_proxy,
    );

    if send {
        let receipt = crate::codex_cloud::follow_up_task(&lease_store, &task_id, &prompt).await?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "task_id": task_id,
                    "token_expires_at_unix_ms": pending.expires_at_unix_ms,
                    "identity_ttl_s": identity_ttl_s,
                    "tls_terminated_proxy": tls_terminated_proxy,
                    "delivered": true,
                    "parent_turn_id": receipt.parent_turn_id,
                })
            );
        } else {
            println!("attach ceremony delivered as a follow-up into {task_id}");
            println!("  token expires: {}s from mint", token_ttl_s);
            println!("  watch: intendant codex-cloud status {task_id}");
        }
        return Ok(());
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "task_id": task_id,
                "token_expires_at_unix_ms": pending.expires_at_unix_ms,
                "identity_ttl_s": identity_ttl_s,
                "tls_terminated_proxy": tls_terminated_proxy,
                "delivered": false,
                "prompt": prompt,
            })
        );
    } else {
        println!(
            "Deliver this prompt to the task (single-use token inside, expires in {token_ttl_s}s):"
        );
        println!();
        println!("{prompt}");
    }
    Ok(())
}

// ── Worker-side agent ──────────────────────────────────────────────────

fn serialize_enrollment_request(token: &str, public_key_pem: &str) -> Result<Vec<u8>, String> {
    // Keep the duplicated body/header to the security-critical ceremony.
    // The richer, untrusted worker fingerprint follows on the authenticated
    // WebSocket hello; including it here exceeded a managed proxy's custom-
    // header ceiling and corrupted both enrollment copies in flight.
    serde_json::to_vec(&serde_json::json!({
        "version": ATTACHMENT_PROTOCOL_VERSION,
        "token": token,
        "public_key_pem": public_key_pem,
    }))
    .map_err(|e| format!("serialize enrollment request: {e}"))
}

pub async fn run_agent(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage:\n  intendant codex-cloud agent --home wss://host:port (--home-fingerprint SHA256 | --tls-terminated-proxy) [--task TASK_ID] --token-stdin [--state-dir DIR]"
        );
        println!(
            "Runs inside a Codex Cloud worker: generates a task-local keypair, redeems the enrollment token at the home daemon's public enroll route, then dials home and holds the authenticated attachment socket in the foreground. Direct mode pins home and presents mTLS; --tls-terminated-proxy explicitly trusts the URL's WebPKI reverse proxy and proves possession of the enrolled key in signed, one-use request headers. --task verifies a manual ceremony; automatic acquisition learns the task id from enrollment. Launch it through run-worker.sh so all state stays task-local."
        );
        return Ok(());
    }
    let mut home: Option<String> = None;
    let mut home_fingerprint: Option<String> = None;
    let mut task: Option<String> = None;
    let mut token_stdin = false;
    let mut state_dir: Option<PathBuf> = None;
    let mut tls_terminated_proxy = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--home" => {
                i += 1;
                home = Some(args.get(i).cloned().ok_or("--home requires a wss:// URL")?);
            }
            "--home-fingerprint" => {
                i += 1;
                home_fingerprint = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--home-fingerprint requires a SHA-256 hex fingerprint")?,
                );
            }
            "--task" => {
                i += 1;
                task = Some(args.get(i).cloned().ok_or("--task requires a task id")?);
            }
            "--token-stdin" => token_stdin = true,
            "--tls-terminated-proxy" => tls_terminated_proxy = true,
            "--state-dir" => {
                i += 1;
                state_dir = Some(PathBuf::from(
                    args.get(i).cloned().ok_or("--state-dir requires a path")?,
                ));
            }
            other => return Err(format!("unknown agent argument {other}")),
        }
        i += 1;
    }
    let home = home_url_from(Some(home.ok_or("agent requires --home wss://host:port")?))?;
    if tls_terminated_proxy && home_fingerprint.is_some() {
        return Err(
            "agent accepts either --home-fingerprint or --tls-terminated-proxy, not both"
                .to_string(),
        );
    }
    if !tls_terminated_proxy && home_fingerprint.is_none() {
        return Err(
            "agent requires --home-fingerprint unless --tls-terminated-proxy was explicitly selected"
                .to_string(),
        );
    }
    if !token_stdin {
        return Err("agent requires --token-stdin (the token never rides argv)".to_string());
    }
    let token = tokio::task::spawn_blocking(|| std::io::read_to_string(std::io::stdin()))
        .await
        .map_err(|e| format!("read stdin: {e}"))?
        .map_err(|e| format!("read stdin: {e}"))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("no enrollment token arrived on stdin".to_string());
    }

    // `run-worker.sh` exports `INTENDANT_HOME` at a task-local root
    // precisely so identity-bearing state stays out of the worker's
    // cached directories — follow it into a `cloud-agent/` subdirectory
    // rather than inventing a second state knob.
    let state_dir = state_dir
        .or_else(|| {
            std::env::var_os("INTENDANT_HOME").map(|root| PathBuf::from(root).join("cloud-agent"))
        })
        .unwrap_or_else(|| PathBuf::from(".intendant-cloud-agent"));
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| format!("create agent state dir {}: {e}", state_dir.display()))?;

    let pinned = home_fingerprint
        .as_deref()
        .map(crate::access::pinning::parse_fingerprint)
        .transpose()
        .map_err(|e| format!("--home-fingerprint: {e}"))?
        .into_iter()
        .collect::<Vec<_>>();

    // 1. Task-local keypair; the private key never leaves this directory.
    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("generate keypair: {e}"))?;
    let key_path = state_dir.join("client.key");
    let cert_path = state_dir.join("client.crt");
    // Owner-only from creation (0600 on Unix) — never write-then-chmod a
    // private key.
    intendant_core::state_paths::write_private_file(&key_path, key_pair.serialize_pem())
        .map_err(|e| format!("write {}: {e}", key_path.display()))?;

    // 2. Redeem the token for a certificate at the public enroll route.
    let enroll_url = enrollment_url_from_home(&home)?;
    let client = if tls_terminated_proxy {
        // Managed Cloud egress can terminate TLS under a private CA that is
        // installed in the worker's native trust store. This mode already
        // makes that proxy an explicit trust decision; use the same roots
        // for enrollment that the WebSocket leg uses below.
        crate::peer::transport::tls_client::reqwest_client_with_native_roots(
            std::time::Duration::from_secs(20),
        )
    } else {
        crate::peer::transport::tls_client::reqwest_client(
            std::time::Duration::from_secs(20),
            &pinned,
            None,
        )
    }
    .map_err(|e| format!("build enroll HTTP client: {e}"))?;
    let worker_fingerprint = collect_worker_fingerprint(crate::codex_cloud::now_unix_ms());
    let enrollment_payload = serialize_enrollment_request(&token, &key_pair.public_key_pem())?;
    if enrollment_payload.len() > ENROLL_REQUEST_MAX_BYTES {
        return Err(format!(
            "enrollment request is too large ({} bytes; limit {ENROLL_REQUEST_MAX_BYTES})",
            enrollment_payload.len()
        ));
    }
    let enrollment_header = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&enrollment_payload)
    };
    let enrollment_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(DEFAULT_TOKEN_TTL_S);
    let text = loop {
        let response = client
            .post(&enroll_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(ENROLL_REQUEST_HEADER, &enrollment_header)
            .body(enrollment_payload.clone())
            .send()
            .await
            .map_err(|e| format!("POST {enroll_url}: {e}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            break text;
        }
        if matches!(
            status,
            reqwest::StatusCode::CONFLICT | reqwest::StatusCode::TOO_MANY_REQUESTS
        ) && tokio::time::Instant::now() < enrollment_deadline
        {
            // The public doorbell is deliberately tight per source. A task
            // can beat its CLI receipt back to home, but retry slowly enough
            // that one legitimate worker cannot rate-limit itself.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        let snippet: String = text.trim().chars().take(200).collect();
        return Err(format!("enrollment refused (HTTP {status}): {snippet}"));
    };
    let enrolled: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse enrollment response: {e}"))?;
    let enrolled_task = enrolled
        .get("task_id")
        .and_then(serde_json::Value::as_str)
        .filter(|task_id| !task_id.is_empty())
        .ok_or("enrollment response carried no task_id")?
        .to_string();
    if let Some(expected_task) = task.as_deref() {
        if expected_task != enrolled_task {
            return Err(format!(
                "enrollment task mismatch: expected {expected_task}, home bound {enrolled_task}"
            ));
        }
    }
    let task = enrolled_task;
    let cert_pem = enrolled
        .get("client_cert_pem")
        .and_then(serde_json::Value::as_str)
        .ok_or("enrollment response carried no client_cert_pem")?;
    let fingerprint = crate::access::access_policy::fingerprint_pem(cert_pem)
        .map_err(|e| format!("fingerprint enrolled client certificate: {e}"))?;
    std::fs::write(&cert_path, cert_pem)
        .map_err(|e| format!("write {}: {e}", cert_path.display()))?;
    eprintln!(
        "[cloud-agent] enrolled for {task}; identity expires at unix {}",
        enrolled
            .get("identity_expires_at_unix")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default()
    );

    // 3. Dial home and hold the attachment. The socket is the heartbeat;
    //    losing it and failing to re-establish is the exit condition.
    let identity = crate::peer::transport::tls_client::ClientIdentityPaths {
        cert_path,
        key_path,
    };
    // Outlives individual sockets so shell sessions survive a reconnect;
    // the task turn's end (or the identity expiry) tears everything down.
    let terminal_registry = crate::terminal::TerminalRegistry::new(
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
    let remote_commands = crate::remote_compute::WorkerRemoteCommands::new(
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )?;
    // Same lifetime rule as the registry: the display session (and any
    // worker-launched Xvfb) survives reconnects; per-viewer stream state
    // dies with each socket.
    let mut display_state = WorkerDisplayState::default();
    let endpoint = AttachmentEndpoint {
        home: &home,
        pinned: &pinned,
        identity: &identity,
        key_pair: &key_pair,
        fingerprint: &fingerprint,
        task: &task,
        worker_fingerprint: &worker_fingerprint,
        tls_terminated_proxy,
    };
    let mut attempt: u32 = 0;
    loop {
        let held = hold_attachment(
            &endpoint,
            &terminal_registry,
            &remote_commands,
            &mut display_state,
        )
        .await;
        // A command whose control socket disappeared is no longer an
        // accountable remote job. Kill it and let home report detachment;
        // callers may retry against the next attachment with the same
        // revision/cache inputs.
        remote_commands.cancel_all().await;
        // The per-viewer display half is socket-scoped: the pump and
        // stream unwind on their own when the socket's channels close,
        // and this reset clears the handles so the next open starts
        // clean.
        display_state.stop_viewer();
        match held {
            Ok(true) => {
                eprintln!("[cloud-agent] retired by home");
                return Ok(());
            }
            Ok(false) => {
                attempt = 0;
                eprintln!("[cloud-agent] attachment closed by home; reconnecting");
            }
            Err(error) => {
                attempt += 1;
                if attempt > 5 {
                    return Err(format!(
                        "attachment failed after {attempt} attempts: {error}"
                    ));
                }
                eprintln!("[cloud-agent] attachment attempt {attempt} failed: {error}");
            }
        }
        let backoff = std::time::Duration::from_secs(2u64.saturating_pow(attempt.min(4)));
        tokio::time::sleep(backoff).await;
    }
}

/// Home side of the CU inversion: send one `cu_execute` frame over the
/// task's attachment and await its correlated `cu_result`. Correlation is
/// by frame id; unrelated reply traffic (tiles, terminal output) is
/// skipped. The caller was already gated on home — the worker executes
/// with home's total authority over it.
pub(crate) async fn cloud_cu_round_trip(
    task_id: &str,
    actions: serde_json::Value,
    coordinate_space: Option<&str>,
    observe: Option<serde_json::Value>,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, String> {
    let Some((to_worker, mut from_worker)) = attachment_channel(task_id) else {
        return Err("cloud worker has no live attachment".to_string());
    };
    let id = uuid::Uuid::new_v4().to_string();
    let mut frame = serde_json::json!({
        "t": "cu_execute",
        "host_id": format!("{CLOUD_HOST_PREFIX}{task_id}"),
        "id": id,
        "actions": actions,
    });
    if let Some(space) = coordinate_space {
        frame["coordinate_space"] = space.into();
    }
    if let Some(observe) = observe {
        frame["observe"] = observe;
    }
    to_worker
        .send(frame.to_string())
        .await
        .map_err(|_| "cloud worker detached".to_string())?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("cloud CU call timed out".to_string());
        }
        match tokio::time::timeout(remaining, from_worker.recv()).await {
            Ok(Ok(text)) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if value.get("t").and_then(serde_json::Value::as_str) == Some("cu_result")
                    && value.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                {
                    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
                        return Err(error.to_string());
                    }
                    return Ok(value);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err("cloud worker detached".to_string());
            }
            Err(_) => return Err("cloud CU call timed out".to_string()),
        }
    }
}

/// Worker-side display state, hoisted to `run_agent` scope like the
/// terminal registry: the display session (and any Xvfb it launched)
/// survives socket reconnects; the per-viewer tile stream and pump die
/// with each socket and are recreated by the next `display_open`.
#[derive(Default)]
pub(crate) struct WorkerDisplayState {
    session: Option<(u32, std::sync::Arc<crate::display::DisplaySession>)>,
    stream: Option<crate::display::tile_socket::TileSocketStream>,
    pump: Option<tokio::task::JoinHandle<()>>,
    input: Option<std::sync::Arc<crate::display::BrowserInputSource>>,
    /// Kill-on-drop guard for a worker-launched Xvfb — held for RAII
    /// only, never read.
    _xvfb: Option<crate::vision::XvfbGuard>,
    /// Session registry for the CU executor: `execute_actions` routes
    /// screenshots and input through the session it finds here, never a
    /// native platform API — load-bearing under the synthetic backend.
    registry: Option<crate::display::SharedSessionRegistry>,
    /// Screenshot scratch for CU batches (RAII-cleaned).
    cu_screenshots: Option<tempfile::TempDir>,
    cu_counter: u64,
}

impl WorkerDisplayState {
    /// Tear down the per-viewer half (stream + pump + input source),
    /// keeping the session/Xvfb for the next open. The capture demand
    /// probe idles a paced backend once the stream's subscription drops.
    fn stop_viewer(&mut self) {
        if let Some(stream) = self.stream.take() {
            stream.stop_nowait();
        }
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        self.input = None;
    }
}

/// Resolve and start the worker's display session: the synthetic backend
/// under the mock rig pair (same fail-closed gate as the daemon), an
/// existing X display, or a fresh Xvfb on Linux. Errors are the operator's
/// actionable message — name what is missing.
async fn start_worker_display_session(
    state: &mut WorkerDisplayState,
) -> Result<(u32, std::sync::Arc<crate::display::DisplaySession>), String> {
    use std::sync::Arc;

    // `state` only receives the Xvfb guard, which exists on Linux alone.
    #[cfg(not(target_os = "linux"))]
    let _ = &mut *state;

    let mock_synthetic = std::env::var("INTENDANT_MOCK_DISPLAY").as_deref() == Ok("synthetic")
        && std::env::var("PROVIDER").as_deref() == Ok("mock");
    let (display_id, backend): (u32, Arc<dyn crate::display::DisplayBackend>) = if mock_synthetic {
        (
            0,
            Arc::new(crate::display::synthetic::SyntheticBackend::new()),
        )
    } else if cfg!(target_os = "linux") {
        #[cfg(target_os = "linux")]
        {
            let env_display = std::env::var("DISPLAY").ok();
            let existing = env_display
                .as_deref()
                .and_then(|display| display.strip_prefix(':'))
                .and_then(|rest| rest.split('.').next())
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|id| crate::vision::virtual_display_socket_exists(*id));
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = crate::vision::conventional_virtual_display().unwrap_or(99);
                    let config = crate::vision::DisplayConfig {
                        target: intendant_platform::DisplayTarget::Virtual { id },
                        width: 1280,
                        height: 800,
                    };
                    let guard = crate::vision::launch_display(&config)
                        .await
                        .map_err(|e| format!("launch Xvfb for the worker display: {e}"))?;
                    state._xvfb = Some(guard);
                    id
                }
            };
            let backend = crate::display::x11::X11Backend::with_display(&format!(":{id}"))
                .map_err(|e| format!("connect to worker X display :{id}: {e}"))?;
            (id, Arc::new(backend))
        }
        #[cfg(not(target_os = "linux"))]
        unreachable!()
    } else {
        return Err(
            "worker display needs a Linux virtual display (Xvfb) or the synthetic mock backend"
                .to_string(),
        );
    };
    let session = Arc::new(crate::display::DisplaySession::new(display_id, backend));
    // The only possible viewer rides the tile-socket stream; never spin
    // the always-on video encoder bank in the container.
    session.disable_video_bank();
    session
        .start(15, None, None)
        .await
        .map_err(|e| format!("start worker display session: {e}"))?;
    let registry: crate::display::SharedSessionRegistry = std::sync::Arc::new(
        tokio::sync::RwLock::new(crate::display::SessionRegistry::new()),
    );
    registry
        .write()
        .await
        .insert(display_id, Arc::clone(&session));
    state.registry = Some(registry);
    Ok((display_id, session))
}

/// Ensure the worker display session exists, creating it on first use.
async fn ensure_worker_display_session(
    display: &mut WorkerDisplayState,
) -> Result<(u32, std::sync::Arc<crate::display::DisplaySession>), String> {
    if let Some((id, session)) = display.session.as_ref() {
        return Ok((*id, std::sync::Arc::clone(session)));
    }
    let (id, session) = start_worker_display_session(display).await?;
    display.session = Some((id, std::sync::Arc::clone(&session)));
    Ok((id, session))
}

/// Run one CU batch against the worker's own display session and reduce
/// the outcome to the wire summary `cu_result` carries: per-action status
/// lines in the MCP formatter's vocabulary, the observation description,
/// and the trailing screenshot. Home's authority over the worker is total,
/// so no worker-side gating applies; the browser/MCP caller was gated on
/// home before the frame ever rode the attachment.
async fn execute_worker_cu_batch(
    display: &mut WorkerDisplayState,
    frame: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (display_id, _session) = ensure_worker_display_session(display).await?;
    let mut actions: Vec<crate::computer_use::CuAction> = frame
        .get("actions")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("parse cu actions: {e}"))?
        .unwrap_or_default();
    if actions.is_empty() {
        return Err("no actions provided".to_string());
    }
    let registry = display.registry.clone();
    let target = intendant_platform::DisplayTarget::Virtual { id: display_id };
    let denorm_ref = if frame
        .get("coordinate_space")
        .and_then(serde_json::Value::as_str)
        == Some("normalized_1000")
    {
        let size = crate::computer_use::target_pixel_size(target, &registry).await;
        for action in &mut actions {
            crate::mcp::denormalize_action(action, size.0, size.1);
        }
        Some(size)
    } else {
        None
    };
    if display.cu_screenshots.is_none() {
        display.cu_screenshots =
            Some(tempfile::tempdir().map_err(|e| format!("create cu screenshot dir: {e}"))?);
    }
    let screenshot_dir = display
        .cu_screenshots
        .as_ref()
        .expect("just ensured")
        .path()
        .to_path_buf();
    let observe = frame
        .get("observe")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let outcome = crate::computer_use::execute_actions(
        &actions,
        target,
        crate::computer_use::DisplayBackend::detect(),
        &screenshot_dir,
        &mut display.cu_counter,
        &registry,
        denorm_ref,
        false,
        None,
        crate::computer_use::CuExecOptions {
            observe,
            annotate: false,
            settle: None,
        },
    )
    .await;
    let mut results = Vec::with_capacity(outcome.results.len());
    for (action, result) in actions.iter().zip(outcome.results.iter()) {
        results.push(serde_json::json!({
            "action": crate::mcp::format_cu_action_brief(action),
            "status": crate::mcp::cu_result_status(result),
            "detail": result.error.as_deref().or(result.detail.as_deref()).unwrap_or(""),
        }));
    }
    let screenshot = outcome
        .last_screenshot()
        .map(|shot| (shot.base64_png.clone(), shot.width, shot.height));
    let mut reply = serde_json::json!({
        "results": results,
        "observation": outcome.observation.describe(),
        "display_id": display_id,
    });
    if let Some((b64, width, height)) = screenshot {
        reply["screenshot_b64"] = serde_json::Value::String(b64);
        reply["screenshot_width"] = width.into();
        reply["screenshot_height"] = height.into();
    }
    Ok(reply)
}

/// Collect the boot-identity fields the probe prompt measures, in the same
/// format (`/proc` reads, whitespace-split stat field 22 — PID 1's comm
/// never contains spaces), so `same_boot` comparisons across the probe and
/// enroll lanes are meaningful. Best-effort by construction: on a non-Linux
/// host the `/proc` reads simply come back `None`.
fn collect_worker_fingerprint(now_ms: u64) -> crate::codex_cloud::WorkerFingerprint {
    let read_trim = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let pid1_start = read_trim("/proc/1/stat")
        .and_then(|stat| stat.split_whitespace().nth(21).map(str::to_string));
    crate::codex_cloud::WorkerFingerprint {
        hostname: read_trim("/proc/sys/kernel/hostname"),
        boot_id: read_trim("/proc/sys/kernel/random/boot_id"),
        pid1_start,
        unix_ms: Some(now_ms),
        git_rev: std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .to_ascii_lowercase()
            })
            .filter(|revision| !revision.is_empty()),
        rustc: None,
        intendant_version: Some(crate::build_info::pkg_version().to_string()),
        intendant_git_sha: Some(crate::build_info::git_sha().to_string()),
        intendant_target: Some(crate::build_info::target_triple().to_string()),
        cpus: std::thread::available_parallelism()
            .ok()
            .map(|count| count.get() as u64),
        mem_kb: None,
        collected_at_unix_ms: now_ms,
    }
}

trait AttachmentIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {}

impl<T> AttachmentIo for T where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static
{
}

type BoxedAttachmentIo = Box<dyn AttachmentIo>;

/// Open the TCP leg beneath the worker's WSS handshake. Codex Cloud exposes
/// agent-phase Internet access through the conventional HTTPS proxy
/// environment; tungstenite does not consult those variables itself, so a
/// direct `connect_async` silently bypasses the allowed lane. CONNECT only
/// establishes the byte tunnel — the subsequent rustls handshake still pins
/// Intendant in direct mode or validates the explicitly trusted WebPKI proxy
/// endpoint in TLS-terminated mode.
async fn open_attachment_transport(home: &str) -> Result<BoxedAttachmentIo, String> {
    let home_url = url::Url::parse(home).map_err(|e| format!("parse home URL: {e}"))?;
    let host = home_url
        .host_str()
        .ok_or("home URL has no host")?
        .to_string();
    let port = home_url
        .port_or_known_default()
        .ok_or("home URL has no port")?;
    let destination = authority(&host, port);
    let timeout = std::time::Duration::from_secs(20);

    let Some(proxy) = attachment_proxy_from_env(&host, port)? else {
        let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&destination))
            .await
            .map_err(|_| format!("dial {destination}: timed out"))?
            .map_err(|e| format!("dial {destination}: {e}"))?;
        return Ok(Box::new(stream));
    };

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let proxy_host = proxy
        .host_str()
        .ok_or("HTTPS proxy URL has no host")?
        .to_string();
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let proxy_authority = authority(&proxy_host, proxy_port);
    let mut stream =
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&proxy_authority))
            .await
            .map_err(|_| "dial HTTPS proxy: timed out".to_string())?
            .map_err(|e| format!("dial HTTPS proxy: {e}"))?;
    let proxy_auth = if proxy.username().is_empty() {
        String::new()
    } else {
        use base64::Engine as _;
        let credentials = format!(
            "{}:{}",
            proxy.username(),
            proxy.password().unwrap_or_default()
        );
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        )
    };
    let connect = format!(
        "CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\nProxy-Connection: Keep-Alive\r\n{proxy_auth}\r\n"
    );
    tokio::time::timeout(timeout, stream.write_all(connect.as_bytes()))
        .await
        .map_err(|_| "write HTTPS proxy CONNECT: timed out".to_string())?
        .map_err(|e| format!("write HTTPS proxy CONNECT: {e}"))?;

    const MAX_PROXY_RESPONSE_HEAD: usize = 16 * 1024;
    let mut response = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(end) = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break end;
        }
        if response.len() >= MAX_PROXY_RESPONSE_HEAD {
            return Err("HTTPS proxy CONNECT response headers are too large".to_string());
        }
        let mut chunk = [0u8; 1024];
        let read = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| "read HTTPS proxy CONNECT: timed out".to_string())?
            .map_err(|e| format!("read HTTPS proxy CONNECT: {e}"))?;
        if read == 0 {
            return Err("HTTPS proxy closed during CONNECT".to_string());
        }
        response.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&response[..header_end])
        .map_err(|_| "HTTPS proxy CONNECT response is not HTTP text".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("HTTPS proxy CONNECT response has no status")?;
    if status != 200 {
        return Err(format!("HTTPS proxy CONNECT refused with HTTP {status}"));
    }
    let prefetched = response[header_end..].to_vec();
    Ok(Box::new(crate::web_tls::PrefixedStream::new(
        prefetched, stream,
    )))
}

fn attachment_proxy_from_env(host: &str, port: u16) -> Result<Option<url::Url>, String> {
    let no_proxy = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok());
    if no_proxy
        .as_deref()
        .is_some_and(|rules| no_proxy_matches(rules, host, port))
    {
        return Ok(None);
    }
    let raw = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Ok(None);
    };
    let proxy = url::Url::parse(&raw).map_err(|_| "HTTPS proxy URL is invalid".to_string())?;
    if proxy.scheme() != "http" {
        return Err(
            "cloud-worker WebSocket supports an http:// CONNECT proxy; HTTPS_PROXY used another scheme"
                .to_string(),
        );
    }
    if proxy.host_str().is_none() || proxy.query().is_some() || proxy.fragment().is_some() {
        return Err("HTTPS proxy URL has an invalid shape".to_string());
    }
    Ok(Some(proxy))
}

fn no_proxy_matches(rules: &str, host: &str, port: u16) -> bool {
    let host = host
        .trim_matches(|ch| matches!(ch, '[' | ']'))
        .to_ascii_lowercase();
    rules.split(',').any(|raw| {
        let raw = raw.trim().to_ascii_lowercase();
        if raw == "*" {
            return true;
        }
        let (rule_host, rule_port) = match raw.rsplit_once(':') {
            Some((candidate, raw_port)) if raw_port.parse::<u16>().is_ok() => {
                (candidate, raw_port.parse::<u16>().ok())
            }
            _ => (raw.as_str(), None),
        };
        if rule_port.is_some_and(|rule_port| rule_port != port) {
            return false;
        }
        let rule_host = rule_host
            .trim_matches(|ch| matches!(ch, '[' | ']'))
            .trim_start_matches('.');
        !rule_host.is_empty()
            && (host == rule_host
                || host
                    .strip_suffix(rule_host)
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

struct AttachmentEndpoint<'a> {
    home: &'a str,
    pinned: &'a [crate::access::pinning::Fingerprint],
    identity: &'a crate::peer::transport::tls_client::ClientIdentityPaths,
    key_pair: &'a rcgen::KeyPair,
    fingerprint: &'a str,
    task: &'a str,
    worker_fingerprint: &'a crate::codex_cloud::WorkerFingerprint,
    tls_terminated_proxy: bool,
}

async fn hold_attachment(
    endpoint: &AttachmentEndpoint<'_>,
    registry: &crate::terminal::TerminalRegistry,
    remote_commands: &crate::remote_compute::WorkerRemoteCommands,
    display: &mut WorkerDisplayState,
) -> Result<bool, String> {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = endpoint
        .home
        .into_client_request()
        .map_err(|e| format!("bad home URL: {e}"))?;
    let nonce = random_token()?;
    let timestamp_unix_ms = i64::try_from(crate::codex_cloud::now_unix_ms()).unwrap_or(i64::MAX);
    let target = request
        .uri()
        .path_and_query()
        .map(|target| target.as_str())
        .unwrap_or(ATTACH_PATH);
    if target != ATTACH_PATH {
        return Err(format!(
            "cloud-worker attachment URL must target {ATTACH_PATH}, got {target}"
        ));
    }
    let payload = attachment_proof_payload(
        target,
        endpoint.fingerprint,
        endpoint.task,
        &nonce,
        timestamp_unix_ms,
    );
    use rcgen::SigningKey as _;
    let proof = crate::daemon_identity::b64u(
        &endpoint
            .key_pair
            .sign(payload.as_bytes())
            .map_err(|e| format!("sign attachment proof: {e}"))?,
    );
    let mut insert = |name: &'static str, value: &str| -> Result<(), String> {
        request.headers_mut().insert(
            name,
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value)
                .map_err(|_| format!("attachment header {name} has an invalid value"))?,
        );
        Ok(())
    };
    insert(CLOUD_WORKER_HEADER, "1")?;
    insert(CLOUD_WORKER_FINGERPRINT_HEADER, endpoint.fingerprint)?;
    insert(CLOUD_WORKER_TASK_HEADER, endpoint.task)?;
    insert(CLOUD_WORKER_NONCE_HEADER, &nonce)?;
    insert(
        CLOUD_WORKER_TIMESTAMP_HEADER,
        &timestamp_unix_ms.to_string(),
    )?;
    insert(CLOUD_WORKER_PROOF_HEADER, &proof)?;
    let connector = crate::peer::transport::tls_client::rustls_client_config(
        endpoint.pinned,
        Some(endpoint.identity),
        false,
    )
    .map_err(|e| format!("build TLS config: {e}"))?
    .map(|config| tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config)));
    let transport = open_attachment_transport(endpoint.home).await?;
    let (mut ws, _resp) =
        tokio_tungstenite::client_async_tls_with_config(request, transport, None, connector)
            .await
            .map_err(|e| format!("dial {}: {e}", endpoint.home))?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "v": 2,
            "kind": HELLO_KIND,
            "task_id": endpoint.task,
            "terminal": true,
            "worker": endpoint.worker_fingerprint,
        })
        .to_string()
        .into(),
    ))
    .await
    .map_err(|e| format!("send hello: {e}"))?;
    if endpoint.tls_terminated_proxy {
        eprintln!(
            "[cloud-agent] attached through the explicitly trusted TLS-terminating proxy; holding the socket"
        );
    } else {
        eprintln!("[cloud-agent] attached over direct mTLS; holding the socket");
    }
    // Replies and PTY output share one bounded outbound lane so forwarder
    // tasks never interleave partial writes on the sink.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(LINK_FROM_WORKER_CAP);
    // One PTY-output forwarder per terminal key: a re-open (browser
    // reload, shell-pane host round-trip) attaches the surviving PTY, and
    // without the abort each open would stack another listener and every
    // output chunk would reach home once per stacked open.
    let mut forwarders: std::collections::HashMap<
        crate::terminal::TerminalKey,
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    let (mut sink, mut stream) = ws.split();
    let mut keepalive = attachment_keepalive_timer();
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                None => break,
                Some(Ok(message)) if message.is_close() => break,
                Some(Ok(message)) => {
                    if let Ok(text) = message.into_text() {
                        serve_worker_frame(
                            registry,
                            remote_commands,
                            text.as_str(),
                            &out_tx,
                            &mut forwarders,
                            display,
                        ).await;
                    }
                }
                Some(Err(error)) => return Err(format!("attachment socket: {error}")),
            },
            outbound = out_rx.recv() => match outbound {
                Some(text) => {
                    sink.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                        .await
                        .map_err(|e| format!("send frame: {e}"))?;
                }
                None => break,
            },
            _ = keepalive.tick() => {
                sink.send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into()))
                    .await
                    .map_err(|e| format!("send attachment keepalive: {e}"))?;
            },
            _ = remote_commands.retired() => {
                let _ = sink.close().await;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The worker's terminal server: home's authority over this worker is
/// total (the container is the sandbox), so frames arriving on the
/// authenticated attachment act as root with an unscoped spawn policy —
/// the scoped/Landlock machinery never engages. Sessions live in the
/// caller's registry so they survive a reconnect; the task turn's end
/// (or the identity expiry) tears the whole process down.
async fn serve_worker_frame(
    registry: &crate::terminal::TerminalRegistry,
    remote_commands: &crate::remote_compute::WorkerRemoteCommands,
    text: &str,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    forwarders: &mut std::collections::HashMap<
        crate::terminal::TerminalKey,
        tokio::task::JoinHandle<()>,
    >,
    display: &mut WorkerDisplayState,
) {
    use base64::Engine as _;

    let Ok(frame) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let field = |name: &str| {
        frame
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let kind = field("t");
    let host_id = field("host_id");
    let terminal_id = field("terminal_id");
    let reply = |mut value: serde_json::Value| {
        if let Some(map) = value.as_object_mut() {
            map.insert("host_id".into(), host_id.clone().into());
            if !terminal_id.is_empty() {
                map.insert("terminal_id".into(), terminal_id.clone().into());
            }
        }
        value.to_string()
    };

    if remote_commands.serve_frame(&frame, out_tx, &host_id).await {
        return;
    }

    // Display frames carry no terminal_id and no registry key.
    match kind.as_str() {
        "display_open" => {
            display.stop_viewer();
            let (display_id, session) = match display.session.as_ref() {
                Some((id, session)) => (*id, std::sync::Arc::clone(session)),
                None => match start_worker_display_session(display).await {
                    Ok((id, session)) => {
                        display.session = Some((id, std::sync::Arc::clone(&session)));
                        (id, session)
                    }
                    Err(error) => {
                        eprintln!("[cloud-agent] display_open failed: {error}");
                        let _ = out_tx
                            .send(reply(serde_json::json!({
                                "t": "display_error", "error": error,
                            })))
                            .await;
                        return;
                    }
                },
            };
            eprintln!("[cloud-agent] display_open (:{display_id})");
            let _ = out_tx
                .send(reply(serde_json::json!({
                    "t": "display_opened", "display_id": display_id,
                })))
                .await;
            display.input = Some(session.browser_input_source(
                crate::display::BrowserInputAuthorization::new(std::sync::Arc::new(|| true)),
            ));
            let (tile_tx, mut tile_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(256);
            display.stream = Some(session.spawn_tile_socket_stream(tile_tx));
            let pump_out = out_tx.clone();
            let pump_host = host_id.clone();
            display.pump = Some(tokio::spawn(async move {
                while let Some(bytes) = tile_rx.recv().await {
                    let frame = serde_json::json!({
                        "t": "display_tiles",
                        "host_id": pump_host,
                        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    });
                    if pump_out.send(frame.to_string()).await.is_err() {
                        break;
                    }
                }
            }));
            return;
        }
        "display_input" => {
            if let (Some(input), Some(event)) =
                (display.input.as_ref(), frame.get("event").cloned())
            {
                if let Ok(event) = serde_json::from_value::<crate::display::InputEvent>(event) {
                    input.enqueue(event);
                }
            }
            return;
        }
        "display_close" => {
            display.stop_viewer();
            let _ = out_tx
                .send(reply(serde_json::json!({ "t": "display_closed" })))
                .await;
            return;
        }
        "cu_execute" => {
            let id = field("id");
            eprintln!(
                "[cloud-agent] cu_execute ({} actions)",
                frame
                    .get("actions")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0)
            );
            let reply_value = match execute_worker_cu_batch(display, &frame).await {
                Ok(mut value) => {
                    if let Some(map) = value.as_object_mut() {
                        map.insert("t".into(), "cu_result".into());
                        map.insert("id".into(), id.clone().into());
                    }
                    value
                }
                Err(error) => serde_json::json!({
                    "t": "cu_result", "id": id, "error": error,
                }),
            };
            let _ = out_tx.send(reply(reply_value)).await;
            return;
        }
        _ => {}
    }

    if terminal_id.is_empty() {
        return;
    }
    let key = crate::terminal::TerminalKey {
        host_id: host_id.clone(),
        terminal_id: terminal_id.clone(),
    };
    let actor = crate::terminal::TerminalActor::Root;
    match kind.as_str() {
        "terminal_open" => {
            let cols = frame
                .get("cols")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(80) as u16;
            let rows = frame
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(24) as u16;
            let policy = crate::terminal::ShellSpawnPolicy {
                may_spawn: true,
                shared: false,
                scope: None,
            };
            match registry
                .open_or_attach(key.clone(), cols, rows, &actor, policy)
                .await
            {
                Ok((session, spawned)) => {
                    eprintln!(
                        "[cloud-agent] terminal_open {terminal_id} ({})",
                        if spawned { "spawned" } else { "attached" }
                    );
                    if let Some(previous) = forwarders.remove(&key) {
                        previous.abort();
                    }
                    let mut listener = session.attach();
                    let _ = out_tx
                        .send(reply(serde_json::json!({
                            "t": "terminal_opened", "shared": false, "can_share": false,
                        })))
                        .await;
                    let forward_tx = out_tx.clone();
                    let forward_reply_host = host_id.clone();
                    let forward_reply_terminal = terminal_id.clone();
                    let handle = tokio::spawn(async move {
                        while let Some(event) = listener.recv().await {
                            let frame = match event {
                                crate::terminal::TerminalEvent::Output(bytes) => {
                                    serde_json::json!({
                                        "t": "terminal_output",
                                        "host_id": forward_reply_host,
                                        "terminal_id": forward_reply_terminal,
                                        "data": base64::engine::general_purpose::STANDARD
                                            .encode(&bytes),
                                    })
                                }
                                crate::terminal::TerminalEvent::Exited { status } => {
                                    let exited = serde_json::json!({
                                        "t": "terminal_exited",
                                        "host_id": forward_reply_host,
                                        "terminal_id": forward_reply_terminal,
                                        "status": status,
                                    });
                                    let _ = forward_tx.send(exited.to_string()).await;
                                    break;
                                }
                            };
                            if forward_tx.send(frame.to_string()).await.is_err() {
                                break;
                            }
                        }
                    });
                    forwarders.insert(key, handle);
                }
                Err(error) => {
                    let _ = out_tx
                        .send(reply(serde_json::json!({
                            "t": "terminal_error", "error": error.to_string(),
                        })))
                        .await;
                }
            }
        }
        "terminal_input" => {
            let data = frame
                .get("data")
                .and_then(serde_json::Value::as_str)
                .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .unwrap_or_default();
            if let Some(session) = registry.get_visible(&key, &actor).await {
                session.write_input(&data);
            }
        }
        "terminal_resize" => {
            let cols = frame
                .get("cols")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(80) as u16;
            let rows = frame
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(24) as u16;
            if let Some(session) = registry.get_visible(&key, &actor).await {
                session.resize(cols, rows);
            }
        }
        "terminal_close" => {
            // Drop the handle without aborting: the close kills the PTY,
            // the listener sees Exited, and the forwarder flushes that
            // final frame home before exiting on its own.
            forwarders.remove(&key);
            registry.close_visible(&key, &actor).await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_single_use_and_expiring() {
        let dir = tempfile::tempdir().unwrap();
        let broker = dir.path().join("attach-broker.json");
        let (token, pending) = mint_enrollment(&broker, "task_e_att", 900, 3600, 1_000).unwrap();
        assert_eq!(pending.task_id.as_deref(), Some("task_e_att"));
        assert!(!broker_contains_plaintext(&broker, &token));

        let consumed = consume_enrollment(&broker, &token, 2_000).unwrap();
        assert_eq!(consumed.task_id.as_deref(), Some("task_e_att"));
        // Burned: the same token never redeems twice.
        let again = consume_enrollment(&broker, &token, 2_000).unwrap_err();
        assert!(
            again.contains("already used") || again.contains("not found"),
            "{again}"
        );

        // Expired tokens refuse with the identical message.
        let (token, _) = mint_enrollment(&broker, "task_e_att", 1, 3600, 1_000).unwrap();
        let expired = consume_enrollment(&broker, &token, 10_000).unwrap_err();
        assert_eq!(expired, again);
    }

    #[test]
    fn automatic_token_waits_unburned_for_task_binding() {
        let dir = tempfile::tempdir().unwrap();
        let broker = dir.path().join("attach-broker.json");
        let (token, pending) = mint_unbound_enrollment(&broker, 900, 3600, 1_000).unwrap();
        assert!(pending.task_id.is_none());
        let waiting = consume_enrollment(&broker, &token, 2_000).unwrap_err();
        assert!(enrollment_binding_pending(&waiting));

        bind_enrollment(&broker, &token, "task_e_auto", 2_100).unwrap();
        let consumed = consume_enrollment(&broker, &token, 2_200).unwrap();
        assert_eq!(consumed.task_id.as_deref(), Some("task_e_auto"));
        assert!(consume_enrollment(&broker, &token, 2_300).is_err());
    }

    fn broker_contains_plaintext(path: &Path, token: &str) -> bool {
        std::fs::read_to_string(path)
            .map(|text| text.contains(token))
            .unwrap_or(false)
    }

    #[test]
    fn redemption_issues_a_zero_authority_expiring_identity() {
        let dir = tempfile::tempdir().unwrap();
        let cert_dir = dir.path().join("access");
        std::fs::create_dir_all(&cert_dir).unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(&cert_dir, &names, "cloud-attach-test", false).unwrap();
        let lease_store = dir.path().join("leases.json");
        let broker = broker_path(&lease_store);
        let (token, _) = mint_enrollment(&broker, "task_e_enroll", 900, 60, 1_000).unwrap();

        let key = rcgen::KeyPair::generate().unwrap();
        let response = redeem_enrollment(
            &cert_dir,
            &lease_store,
            &EnrollRequest {
                version: 1,
                token,
                public_key_pem: key.public_key_pem(),
                worker: None,
            },
            5_000,
        )
        .unwrap();
        assert_eq!(response.task_id, "task_e_enroll");
        assert_eq!(response.profile, "cloud-worker");

        let fingerprint =
            crate::access::access_policy::fingerprint_pem(&response.client_cert_pem).unwrap();
        let record = crate::access::access_policy::lookup_identity(&cert_dir, &fingerprint)
            .unwrap()
            .expect("identity record written");
        assert_eq!(record.profile, "cloud-worker");
        assert_eq!(record.expires_at_unix, Some(65));
        assert!(record.is_active(64));
        assert!(
            !record.is_active(66),
            "identity must expire on its record clock"
        );

        // The ceiling grants nothing at all.
        use crate::access::access_policy::{profile_allows_operation, PeerOperation};
        for op in [
            PeerOperation::PresenceRead,
            PeerOperation::StatsRead,
            PeerOperation::Message,
            PeerOperation::Task,
        ] {
            assert!(!profile_allows_operation("cloud-worker", op));
        }

        // The binding lets the listener resolve the socket to its task.
        let binding = binding_for_fingerprint(&broker, &fingerprint).expect("binding recorded");
        assert_eq!(binding.task_id, "task_e_enroll");
        assert_eq!(
            binding.proof_public_key_b64u.as_deref(),
            Some(crate::daemon_identity::b64u(key.public_key_raw()).as_str())
        );

        // A TLS-terminating proxy cannot forward the client certificate, so
        // the same enrolled key proves possession in the WebSocket request.
        // The proof is one-use even if every header is replayed byte-for-byte.
        let nonce = "n".repeat(43);
        let request = signed_proxy_request(&key, &fingerprint, "task_e_enroll", &nonce, 6_000);
        assert_eq!(
            verify_proxy_attachment_request_at(&broker, &request, 6_100).unwrap(),
            fingerprint
        );
        let replay = verify_proxy_attachment_request_at(&broker, &request, 6_200).unwrap_err();
        assert!(replay.contains("already used"), "{replay}");

        let wrong_task = signed_proxy_request(
            &key,
            &fingerprint,
            "task_e_someone_else",
            &"m".repeat(43),
            6_300,
        );
        assert!(
            verify_proxy_attachment_request_at(&broker, &wrong_task, 6_300)
                .unwrap_err()
                .contains("task binding")
        );
        let valid_after_refusal =
            signed_proxy_request(&key, &fingerprint, "task_e_enroll", &"m".repeat(43), 6_300);
        assert!(
            verify_proxy_attachment_request_at(&broker, &valid_after_refusal, 6_400).is_ok(),
            "a refused proof must not consume its nonce"
        );

        // A second redemption with the burned token fails.
        let err = redeem_enrollment(
            &cert_dir,
            &lease_store,
            &EnrollRequest {
                version: 1,
                token: "wrong".into(),
                public_key_pem: key.public_key_pem(),
                worker: None,
            },
            5_000,
        )
        .unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn enrollment_refuses_version_drift_with_repin_guidance() {
        // The version gate fires before any filesystem or token work, so
        // placeholder roots prove no side effect can precede the refusal.
        let err = redeem_enrollment(
            Path::new("unused-cert-dir"),
            Path::new("unused-lease-store"),
            &EnrollRequest {
                version: ATTACHMENT_PROTOCOL_VERSION + 1,
                token: "irrelevant".into(),
                public_key_pem: String::new(),
                worker: None,
            },
            5_000,
        )
        .unwrap_err();
        // A pinned release-asset worker surfaces this string as its
        // enrollment failure: it must name both versions and the repin.
        let refused = format!(
            "unsupported enrollment request version {}",
            ATTACHMENT_PROTOCOL_VERSION + 1
        );
        assert!(err.contains(&refused), "{err}");
        assert!(
            err.contains(&format!("speaks {ATTACHMENT_PROTOCOL_VERSION}")),
            "{err}"
        );
        assert!(err.contains("INTENDANT_CLOUD_BINARY_URL"), "{err}");
    }

    #[test]
    fn collected_fingerprint_carries_binary_provenance() {
        let fingerprint = collect_worker_fingerprint(1_234);
        assert_eq!(
            fingerprint.intendant_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(fingerprint.intendant_git_sha.is_some());
        assert!(fingerprint.intendant_target.is_some());
    }

    #[test]
    fn enrollment_request_stays_compact_and_moves_fingerprint_to_hello() {
        let key = rcgen::KeyPair::generate().unwrap();
        let token = "t".repeat(64);
        let payload = serialize_enrollment_request(&token, &key.public_key_pem()).unwrap();
        let request: EnrollRequest = serde_json::from_slice(&payload).unwrap();
        assert_eq!(request.token, token);
        assert!(request.worker.is_none());

        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
        assert!(
            encoded.len() <= 512,
            "security-critical enrollment header grew to {} characters",
            encoded.len()
        );
    }

    #[test]
    fn authenticated_hello_carries_worker_fingerprint() {
        let hello = serde_json::json!({
            "v": 2,
            "kind": HELLO_KIND,
            "task_id": "task_e_hello",
            "worker": {
                "hostname": "worker-a",
                "intendant_version": "0.2.0-alpha.2",
            },
        });
        let fingerprint = worker_hello_fingerprint("task_e_hello", &hello).unwrap();
        assert_eq!(fingerprint.hostname.as_deref(), Some("worker-a"));
        assert_eq!(
            fingerprint.intendant_version.as_deref(),
            Some("0.2.0-alpha.2")
        );
        assert!(worker_hello_fingerprint("task_e_other", &hello).is_none());
    }

    #[test]
    fn fingerprint_parse_tolerates_newer_worker_fields() {
        // An older home receiving a newer worker fingerprint must record
        // what it knows and ignore the rest — the additive half of the
        // ATTACHMENT_PROTOCOL_VERSION contract.
        let fingerprint =
            serde_json::from_value::<crate::codex_cloud::WorkerFingerprint>(serde_json::json!({
                "hostname": "worker-a",
                "intendant_version": "9.9.9",
                "field_from_the_future": {"nested": true},
            }))
            .expect("unknown fingerprint fields must never fail the parse");
        assert_eq!(fingerprint.hostname.as_deref(), Some("worker-a"));
        assert_eq!(fingerprint.intendant_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn attach_prompt_carries_the_ceremony_and_nothing_extra() {
        let prompt = attach_prompt(
            "task_e_p",
            "wss://home.example:8443/api/codex-cloud/attach",
            Some("ab".repeat(32).as_str()),
            "tok-secret",
            false,
        );
        assert!(prompt.contains("codex-cloud agent"));
        assert!(prompt.contains("--home wss://home.example:8443/api/codex-cloud/attach"));
        assert!(prompt.contains("--home-fingerprint"));
        assert!(!prompt.contains("--tls-terminated-proxy"));
        assert!(prompt.contains("--task task_e_p"));
        assert!(prompt.contains("tok-secret"));
        assert!(prompt.contains("run-worker.sh"));
        assert!(prompt.contains("foreground"));
    }

    #[test]
    fn proxy_prompt_is_explicit_and_does_not_claim_to_pin_home() {
        let prompt = attach_prompt(
            "task_e_p",
            "wss://home.example/api/codex-cloud/attach",
            None,
            "tok-secret",
            true,
        );
        assert!(prompt.contains("--tls-terminated-proxy"));
        assert!(!prompt.contains("--home-fingerprint"));
    }

    #[test]
    fn enroll_rate_limit_is_per_source_with_a_global_backstop() {
        // Streaks use a far-future epoch so parallel tests sharing the
        // process-wide limiter cannot interfere inside this window.
        let base = 3_000_000_000_000u64;
        for i in 0..ENROLL_RATE_PER_SOURCE_MAX {
            assert!(enroll_rate_ok("198.51.100.7", base + i as u64), "{i}");
        }
        // The noisy source is throttled…
        assert!(!enroll_rate_ok("198.51.100.7", base + 50));
        // …while a different source is untouched.
        assert!(enroll_rate_ok("198.51.100.8", base + 51));
        // The noisy source recovers once its window slides.
        assert!(enroll_rate_ok(
            "198.51.100.7",
            base + ENROLL_RATE_WINDOW_MS + 100
        ));
    }

    #[test]
    fn attachment_deadline_is_zero_once_the_identity_expired() {
        assert_eq!(attachment_remaining_ms(100, 100_000), 0);
        assert_eq!(attachment_remaining_ms(100, 99_000), 1_000);
        assert_eq!(attachment_remaining_ms(-5, 0), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn attachment_keepalive_waits_then_repeats() {
        let started = tokio::time::Instant::now();
        let mut keepalive = attachment_keepalive_timer();
        keepalive.tick().await;
        assert_eq!(started.elapsed(), ATTACHMENT_KEEPALIVE_INTERVAL);
        keepalive.tick().await;
        assert_eq!(started.elapsed(), ATTACHMENT_KEEPALIVE_INTERVAL * 2);
        assert!(ATTACHMENT_KEEPALIVE_INTERVAL < Duration::from_secs(5 * 60));
    }

    #[test]
    fn home_urls_must_be_wss() {
        let err = home_url_from(Some("ws://127.0.0.1:8765/ws".into())).unwrap_err();
        assert!(err.contains("wss://"), "{err}");
        assert_eq!(
            home_url_from(Some("wss://home.example:8443/ws".into())).unwrap(),
            "wss://home.example:8443/api/codex-cloud/attach"
        );
    }

    #[test]
    fn enrollment_replaces_the_attachment_path_on_the_same_origin() {
        assert_eq!(
            enrollment_url_from_home("wss://home.example:8443/api/codex-cloud/attach").unwrap(),
            "https://home.example:8443/api/codex-cloud/enroll"
        );
    }

    #[test]
    fn no_proxy_matching_covers_exact_suffix_port_and_wildcard() {
        assert!(no_proxy_matches(
            "localhost,.example.test",
            "api.example.test",
            443
        ));
        assert!(no_proxy_matches(
            "home.example.test:8443",
            "home.example.test",
            8443
        ));
        assert!(!no_proxy_matches(
            "home.example.test:8443",
            "home.example.test",
            443
        ));
        assert!(!no_proxy_matches("example.test", "notexample.test", 443));
        assert!(no_proxy_matches("*", "anything.invalid", 443));
        assert_eq!(authority("2001:db8::1", 443), "[2001:db8::1]:443");
    }

    fn signed_proxy_request(
        key: &rcgen::KeyPair,
        fingerprint: &str,
        task_id: &str,
        nonce: &str,
        timestamp_unix_ms: i64,
    ) -> String {
        use rcgen::SigningKey as _;
        let payload =
            attachment_proof_payload(ATTACH_PATH, fingerprint, task_id, nonce, timestamp_unix_ms);
        let signature = crate::daemon_identity::b64u(&key.sign(payload.as_bytes()).unwrap());
        format!(
            "GET {ATTACH_PATH} HTTP/1.1\r\nHost: home.example\r\n{CLOUD_WORKER_HEADER}: 1\r\n{CLOUD_WORKER_FINGERPRINT_HEADER}: {fingerprint}\r\n{CLOUD_WORKER_TASK_HEADER}: {task_id}\r\n{CLOUD_WORKER_NONCE_HEADER}: {nonce}\r\n{CLOUD_WORKER_TIMESTAMP_HEADER}: {timestamp_unix_ms}\r\n{CLOUD_WORKER_PROOF_HEADER}: {signature}\r\n\r\n"
        )
    }

    #[test]
    fn cloud_host_ids_parse_and_refuse_empties() {
        assert_eq!(cloud_host_task_id("cloud:task_e_1"), Some("task_e_1"));
        assert_eq!(cloud_host_task_id("cloud:"), None);
        assert_eq!(cloud_host_task_id("local"), None);
        assert_eq!(cloud_host_task_id("intendant:peer"), None);
    }

    #[test]
    fn worker_frames_fan_out_replies_and_drop_everything_else() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(8);
        // A reply kind reaches subscribers.
        route_worker_frame(
            "task-test",
            &tx,
            r#"{"t":"terminal_output","host_id":"cloud:x","terminal_id":"shell-0","data":"aGk="}"#,
        );
        assert!(rx.try_recv().is_ok());
        route_worker_frame(
            "task-test",
            &tx,
            r#"{"t":"display_tiles","host_id":"cloud:x","data":"aGk="}"#,
        );
        assert!(rx.try_recv().is_ok());
        route_worker_frame(
            "task-test",
            &tx,
            r#"{"t":"remote_command_result","host_id":"cloud:x","id":"remote-1","result":{"state":"succeeded","exit_code":0,"stdout":"","stderr":"","stdout_truncated":false,"stderr_truncated":false,"duration_ms":1}}"#,
        );
        assert!(rx.try_recv().is_ok());
        route_worker_frame(
            "task-test",
            &tx,
            r#"{"t":"remote_source_result","host_id":"cloud:x","id":"source-a","state":"ready"}"#,
        );
        assert!(rx.try_recv().is_ok());
        // The worker cannot inject request kinds, hellos, or junk into
        // home — its inbound authority stays nothing.
        for dropped in [
            r#"{"t":"terminal_open","host_id":"cloud:x","terminal_id":"shell-0"}"#,
            r#"{"t":"display_open","host_id":"cloud:x"}"#,
            r#"{"t":"display_input","host_id":"cloud:x","event":{"t":"mm","x":0.1,"y":0.1}}"#,
            r#"{"t":"remote_command_start","host_id":"cloud:x","id":"remote-1"}"#,
            r#"{"t":"remote_command_cancel","host_id":"cloud:x","id":"remote-1"}"#,
            r#"{"t":"remote_source_begin","host_id":"cloud:x","id":"source-a"}"#,
            r#"{"t":"remote_cache_request","host_id":"cloud:x","id":"remote-1","relay_token":"00000000000000000000000000000000","request_id":"11111111111111111111111111111111","op":"stat"}"#,
            r#"{"v":2,"kind":"cloud-worker-hello","task_id":"x"}"#,
            r#"{"t":"api_sessions"}"#,
            "not json",
        ] {
            route_worker_frame("task-test", &tx, dropped);
            assert!(rx.try_recv().is_err(), "{dropped}");
        }
    }

    /// The worker's display arms end to end over a seeded synthetic
    /// session (no env, no X server): open replies `display_opened` and
    /// starts the tile stream (base64 wire frames stamped with the cloud
    /// host), input enqueues, close stops the viewer and acks.
    #[tokio::test]
    async fn worker_display_open_streams_tiles_and_close_stops() {
        let backend = std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new());
        let session = std::sync::Arc::new(crate::display::DisplaySession::new(0, backend));
        session.disable_video_bank();
        session.start(10, None, None).await.expect("session starts");
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::terminal::TerminalRegistry::new(dir.path().to_path_buf());
        let remote_commands =
            crate::remote_compute::WorkerRemoteCommands::new(dir.path().to_path_buf()).unwrap();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(1024);
        let mut forwarders = std::collections::HashMap::new();
        let mut display = WorkerDisplayState {
            session: Some((0, std::sync::Arc::clone(&session))),
            ..Default::default()
        };

        let open = r#"{"t":"display_open","host_id":"cloud:t"}"#;
        serve_worker_frame(
            &registry,
            &remote_commands,
            open,
            &out_tx,
            &mut forwarders,
            &mut display,
        )
        .await;
        let opened = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("opened within deadline")
            .expect("open reply");
        assert!(opened.contains("display_opened"), "{opened}");

        let tiles = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("tiles within deadline")
            .expect("tile frame");
        let value: serde_json::Value = serde_json::from_str(&tiles).unwrap();
        assert_eq!(
            value.get("t").and_then(|v| v.as_str()),
            Some("display_tiles")
        );
        assert_eq!(
            value.get("host_id").and_then(|v| v.as_str()),
            Some("cloud:t")
        );
        assert!(value
            .get("data")
            .and_then(|v| v.as_str())
            .is_some_and(|data| !data.is_empty()));

        let input = r#"{"t":"display_input","host_id":"cloud:t","display_id":0,"event":{"t":"mm","x":0.5,"y":0.5}}"#;
        serve_worker_frame(
            &registry,
            &remote_commands,
            input,
            &out_tx,
            &mut forwarders,
            &mut display,
        )
        .await;

        let close = r#"{"t":"display_close","host_id":"cloud:t"}"#;
        serve_worker_frame(
            &registry,
            &remote_commands,
            close,
            &out_tx,
            &mut forwarders,
            &mut display,
        )
        .await;
        assert!(display.stream.is_none() && display.pump.is_none());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_closed = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv()).await {
                Ok(Some(text)) if text.contains("display_closed") => {
                    saw_closed = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_closed, "close is acked");
        session.stop().await;
    }

    /// The CU inversion round-trips over an in-process attachment link:
    /// home sends cu_execute, the fake worker answers the correlated
    /// cu_result, unrelated broadcast traffic is skipped, and a dead
    /// worker surfaces as a timeout.
    #[tokio::test]
    async fn cloud_cu_round_trip_correlates_and_times_out() {
        let (to_worker_tx, mut to_worker_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (from_worker_tx, _) = tokio::sync::broadcast::channel::<String>(8);
        attachment_links().lock().unwrap().insert(
            "task_cu_rt".to_string(),
            AttachmentLink {
                to_worker: to_worker_tx,
                from_worker: from_worker_tx.clone(),
            },
        );
        let responder = tokio::spawn(async move {
            let text = to_worker_rx.recv().await.expect("cu_execute arrives");
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(frame["t"], "cu_execute");
            assert_eq!(frame["host_id"], "cloud:task_cu_rt");
            // Noise first: unrelated reply traffic must be skipped.
            let _ = from_worker_tx.send(
                r#"{"t":"display_tiles","host_id":"cloud:task_cu_rt","data":"aGk="}"#.to_string(),
            );
            let reply = serde_json::json!({
                "t": "cu_result",
                "id": frame["id"],
                "results": [{"action": "screenshot", "status": "ok", "detail": ""}],
                "observation": "pixels",
            });
            let _ = from_worker_tx.send(reply.to_string());
        });
        let value = cloud_cu_round_trip(
            "task_cu_rt",
            serde_json::json!([{"type": "screenshot"}]),
            None,
            None,
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("round trip succeeds");
        assert_eq!(value["results"][0]["status"], "ok");
        responder.await.unwrap();

        // Silent worker: the link is live (receiver held open) but nothing
        // answers -> timeout, not a hang and not a detach.
        let (silent_tx, _silent_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (silent_from_tx, _) = tokio::sync::broadcast::channel::<String>(8);
        attachment_links().lock().unwrap().insert(
            "task_cu_silent".to_string(),
            AttachmentLink {
                to_worker: silent_tx,
                from_worker: silent_from_tx,
            },
        );
        let error = cloud_cu_round_trip(
            "task_cu_silent",
            serde_json::json!([{"type": "screenshot"}]),
            None,
            None,
            std::time::Duration::from_millis(200),
        )
        .await
        .expect_err("times out");
        assert!(error.contains("timed out"), "{error}");
        let mut links = attachment_links().lock().unwrap();
        links.remove("task_cu_rt");
        links.remove("task_cu_silent");
    }

    /// The worker CU executor runs a real batch against a seeded synthetic
    /// session: a screenshot action succeeds, the reply carries the
    /// reduced results and the screenshot payload, and no native platform
    /// API is involved (session-backed capture).
    #[tokio::test]
    async fn worker_cu_batch_screenshots_via_the_session() {
        let backend = std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new());
        let session = std::sync::Arc::new(crate::display::DisplaySession::new(0, backend));
        session.disable_video_bank();
        session.start(10, None, None).await.expect("session starts");
        let registry: crate::display::SharedSessionRegistry = std::sync::Arc::new(
            tokio::sync::RwLock::new(crate::display::SessionRegistry::new()),
        );
        registry
            .write()
            .await
            .insert(0, std::sync::Arc::clone(&session));
        let mut display = WorkerDisplayState {
            session: Some((0, std::sync::Arc::clone(&session))),
            registry: Some(registry),
            ..Default::default()
        };
        // Wait for the capture's first frame: without one the session
        // screenshot path errs and the executor falls through to the
        // native capture path, which a CI runner has no display for
        // (production behavior — a real Xvfb worker's fallback hits the
        // same display via DISPLAY; the test pins the session path).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if session.screenshot().await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "synthetic session produced no frame within the deadline"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let frame = serde_json::json!({
            "t": "cu_execute",
            "host_id": "cloud:t",
            "id": "cu-1",
            "actions": [{"type": "screenshot"}],
        });
        let value = execute_worker_cu_batch(&mut display, &frame)
            .await
            .expect("batch executes");
        let status = value["results"][0]["status"].as_str().unwrap_or("?");
        assert_ne!(status, "failed", "{value}");
        assert!(value["screenshot_b64"]
            .as_str()
            .is_some_and(|b64| !b64.is_empty()));
        session.stop().await;
    }

    #[tokio::test]
    async fn reopening_a_worker_terminal_replaces_its_forwarder() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::terminal::TerminalRegistry::new(dir.path().to_path_buf());
        let remote_commands =
            crate::remote_compute::WorkerRemoteCommands::new(dir.path().to_path_buf()).unwrap();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);
        let mut forwarders = std::collections::HashMap::new();
        let open = r#"{"t":"terminal_open","host_id":"cloud:t","terminal_id":"shell-0","cols":80,"rows":24}"#;
        let mut display = WorkerDisplayState::default();
        serve_worker_frame(
            &registry,
            &remote_commands,
            open,
            &out_tx,
            &mut forwarders,
            &mut display,
        )
        .await;
        assert_eq!(forwarders.len(), 1);
        let first = out_rx.recv().await.expect("first opened reply");
        assert!(first.contains("terminal_opened"), "{first}");
        let previous = forwarders
            .values()
            .next()
            .map(tokio::task::JoinHandle::is_finished);
        assert_eq!(previous, Some(false));
        // A second open for the same key attaches the surviving PTY and
        // must replace the listener, never stack a second one (stacked
        // listeners double every output chunk on the dashboard).
        serve_worker_frame(
            &registry,
            &remote_commands,
            open,
            &out_tx,
            &mut forwarders,
            &mut display,
        )
        .await;
        assert_eq!(forwarders.len(), 1);
        let second = out_rx.recv().await.expect("second opened reply");
        assert!(second.contains("terminal_opened"), "{second}");
    }

    #[test]
    fn enroll_request_debug_never_prints_the_token() {
        let request = EnrollRequest {
            version: 1,
            token: "attach-secret-1".into(),
            public_key_pem: "-----BEGIN PUBLIC KEY-----".into(),
            worker: None,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("secret-1"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");
    }

    #[test]
    fn system_profile_is_recognized_but_never_operator_assignable() {
        use crate::access::access_policy::{profile_class, require_known_profile, ProfileClass};
        assert_eq!(profile_class("cloud-worker"), ProfileClass::CloudWorker);
        assert!(require_known_profile("cloud-worker").is_err());
    }
}
