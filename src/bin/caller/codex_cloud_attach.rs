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
//! then dials home's gateway over mTLS and the accepted socket *is* the
//! attachment: the lease flips `connected` while it lives, `disconnected`
//! when it dies. Authority over the worker flows home→worker in later
//! slices; the worker's inbound authority on home stays nothing.
//!
//! Private keys never transit (the daemon signs a public key, never
//! mints one for the worker), tokens are stored hashed and burned
//! atomically on first redemption, and an unknown, used, or expired
//! token is indistinguishable in the error surface.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::codex_cloud::{record_attachment_state, state_path, AttachmentState, StoreLock};

const BROKER_VERSION: u32 = 1;
/// Enrollment tokens are delivery secrets for one attach ceremony:
/// minutes, not hours.
const DEFAULT_TOKEN_TTL_S: u64 = 900;
/// The issued identity's record expiry (independent of cert validity —
/// the record is what the gateway enforces on every connection).
const DEFAULT_IDENTITY_TTL_S: u64 = 3600;
/// The public redemption doorbell's path — the one spelling shared by the
/// route table, the certless carve-out predicate, and the worker's dial.
pub(crate) const ENROLL_PATH: &str = "/api/codex-cloud/enroll";
/// The worker's first frame after the socket opens. Informational — the
/// identity was already established by the client certificate.
const HELLO_KIND: &str = "cloud-worker-hello";

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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEnrollment {
    pub task_id: String,
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
    let token = random_token()?;
    let pending = PendingEnrollment {
        task_id: task_id.to_string(),
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
    let pending = store
        .pending
        .remove(&token_hash(token))
        .ok_or_else(|| REFUSED.to_string())?;
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
    if request.version != 1 {
        return Err(format!(
            "unsupported enrollment request version {}",
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
    rcgen::SubjectPublicKeyInfo::from_pem(public_key_pem)
        .map_err(|e| format!("public_key_pem is not a valid SPKI public key: {e}"))?;

    let broker = broker_path(lease_store_path);
    let pending = consume_enrollment(&broker, token, now_ms)?;

    let profile = crate::access::access_policy::CLOUD_WORKER_PROFILE;
    let label = format!("{profile} {}", pending.task_id);
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
            task_id: pending.task_id.clone(),
            label,
            issued_at_unix_ms: now_ms,
            identity_expires_at_unix,
        },
    )?;
    // The ceremony is under way: the lease shows `awaiting` until the
    // socket actually opens. Best-effort — an untracked task still
    // enrolls (the lease may live on another store generation).
    let _ = record_attachment_state(
        lease_store_path,
        &pending.task_id,
        AttachmentState::Awaiting,
    );
    if let Some(worker) = &request.worker {
        record_worker_json(lease_store_path, &pending.task_id, worker);
    }
    Ok(EnrollResponse {
        version: 1,
        task_id: pending.task_id,
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
    loop {
        tokio::select! {
            frame = read.next() => match frame {
                None => break,
                Some(Ok(message)) if message.is_close() => break,
                Some(Ok(message)) => {
                    if let Ok(text) = message.into_text() {
                        route_worker_frame(&from_worker_tx, text.as_str());
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

/// Route one worker frame: reply kinds fan out to dashboard subscribers,
/// everything else (hello included) is dropped without dispatch.
fn route_worker_frame(from_worker_tx: &tokio::sync::broadcast::Sender<String>, text: &str) {
    let kind = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
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

fn home_url_from(args_value: Option<String>) -> Result<String, String> {
    let url = args_value
        .or_else(|| std::env::var("INTENDANT_CODEX_CLOUD_HOME_URL").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "attach needs the daemon's reachable WSS URL: pass --home-url wss://host:port or set INTENDANT_CODEX_CLOUD_HOME_URL"
                .to_string()
        })?;
    // wss:// only: the ceremony's token would ride plain HTTP on a ws://
    // base, and a plaintext socket can present no client certificate, so
    // the attachment could never form anyway.
    if !url.starts_with("wss://") {
        return Err(format!(
            "home URL must be wss://… (the enrollment token and the mTLS attachment both require TLS), got '{url}'"
        ));
    }
    Ok(url)
}

/// The attach follow-up prompt: everything in it is public except the
/// single-use token, which is the point of the ceremony.
pub fn attach_prompt(
    task_id: &str,
    home_url: &str,
    server_fingerprint: &str,
    token: &str,
) -> String {
    format!(
        "Run this exact command in the repository root and keep it running in the foreground until this turn is cancelled. Do not modify any files, do not run builds, and do not print the token back:\n\n\
```bash\n\
\"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/libexec/intendant-cloud/run-worker.sh\" -- \\\n\
  \"${{INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}}/bin/intendant\" codex-cloud agent \\\n\
  --home {home_url} --home-fingerprint {server_fingerprint} --task {task_id} --token-stdin <<'INTENDANT_ATTACH_TOKEN'\n\
{token}\n\
INTENDANT_ATTACH_TOKEN\n\
```\n\n\
If the command fails because a destination is blocked by the environment's network allowlist, report the blocked host and stop."
    )
}

pub async fn run_attach(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage:\n  intendant codex-cloud attach TASK_ID [--home-url wss://host:port] [--token-ttl-s {DEFAULT_TOKEN_TTL_S}] [--identity-ttl-s {DEFAULT_IDENTITY_TTL_S}] [--send] [--json]"
        );
        println!(
            "Mints a single-use enrollment token bound to the task and composes the attach prompt (printed by default; --send delivers it as a follow-up turn into the warm worker). The worker redeems the token for a zero-authority cloud-worker certificate and dials back; the lease's attachment lane tracks the socket."
        );
        return Ok(());
    }
    let mut task_id: Option<String> = None;
    let mut home_url_arg: Option<String> = None;
    let mut token_ttl_s = DEFAULT_TOKEN_TTL_S;
    let mut identity_ttl_s = DEFAULT_IDENTITY_TTL_S;
    let mut send = false;
    let mut json = false;
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
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let server_fingerprint = crate::access::certs::read_server_cert_fingerprint(&cert_dir)
        .ok_or_else(|| {
            "no gateway server certificate found — start the daemon once (it mints the TLS identity workers must pin), then retry"
                .to_string()
        })?;

    let broker = broker_path(&lease_store);
    let (token, pending) = mint_enrollment(
        &broker,
        &task_id,
        token_ttl_s,
        identity_ttl_s,
        crate::codex_cloud::now_unix_ms(),
    )?;
    let _ = record_attachment_state(&lease_store, &task_id, AttachmentState::Awaiting);
    let prompt = attach_prompt(&task_id, &home_url, &server_fingerprint, &token);

    if send {
        let receipt = crate::codex_cloud::follow_up_task(&lease_store, &task_id, &prompt).await?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "task_id": task_id,
                    "token_expires_at_unix_ms": pending.expires_at_unix_ms,
                    "identity_ttl_s": identity_ttl_s,
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

pub async fn run_agent(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage:\n  intendant codex-cloud agent --home wss://host:port --home-fingerprint SHA256 --task TASK_ID --token-stdin [--state-dir DIR]"
        );
        println!(
            "Runs inside a Codex Cloud worker: generates a task-local keypair, redeems the enrollment token at the home daemon's public enroll route, then dials home over mTLS and holds the attachment socket in the foreground. Launch it through run-worker.sh so all state stays task-local."
        );
        return Ok(());
    }
    let mut home: Option<String> = None;
    let mut home_fingerprint: Option<String> = None;
    let mut task: Option<String> = None;
    let mut token_stdin = false;
    let mut state_dir: Option<PathBuf> = None;
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
    let home = home.ok_or("agent requires --home wss://host:port")?;
    if !home.starts_with("wss://") {
        return Err(format!(
            "--home must be wss://… (the enrollment token and the mTLS attachment both require TLS), got '{home}'"
        ));
    }
    let home_fingerprint = home_fingerprint
        .ok_or("agent requires --home-fingerprint (pin the daemon's TLS identity)")?;
    let task = task.ok_or("agent requires --task TASK_ID")?;
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

    let pinned = vec![crate::access::pinning::parse_fingerprint(&home_fingerprint)
        .map_err(|e| format!("--home-fingerprint: {e}"))?];

    // 1. Task-local keypair; the private key never leaves this directory.
    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("generate keypair: {e}"))?;
    let key_path = state_dir.join("client.key");
    let cert_path = state_dir.join("client.crt");
    // Owner-only from creation (0600 on Unix) — never write-then-chmod a
    // private key.
    intendant_core::state_paths::write_private_file(&key_path, key_pair.serialize_pem())
        .map_err(|e| format!("write {}: {e}", key_path.display()))?;

    // 2. Redeem the token for a certificate at the public enroll route.
    let http_base = crate::peer::transport::ws_url_to_http_base(&home);
    let enroll_url = format!("{http_base}{ENROLL_PATH}");
    let client = crate::peer::transport::tls_client::reqwest_client(
        std::time::Duration::from_secs(20),
        &pinned,
        None,
    )
    .map_err(|e| format!("build enroll HTTP client: {e}"))?;
    let worker_fingerprint = collect_worker_fingerprint(crate::codex_cloud::now_unix_ms());
    let response = client
        .post(&enroll_url)
        .json(&serde_json::json!({
            "version": 1,
            "token": token,
            "public_key_pem": key_pair.public_key_pem(),
            "worker": worker_fingerprint,
        }))
        .send()
        .await
        .map_err(|e| format!("POST {enroll_url}: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.trim().chars().take(200).collect();
        return Err(format!("enrollment refused (HTTP {status}): {snippet}"));
    }
    let enrolled: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse enrollment response: {e}"))?;
    let cert_pem = enrolled
        .get("client_cert_pem")
        .and_then(serde_json::Value::as_str)
        .ok_or("enrollment response carried no client_cert_pem")?;
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
    // Same lifetime rule as the registry: the display session (and any
    // worker-launched Xvfb) survives reconnects; per-viewer stream state
    // dies with each socket.
    let mut display_state = WorkerDisplayState::default();
    let mut attempt: u32 = 0;
    loop {
        let held = hold_attachment(
            &home,
            &pinned,
            &identity,
            &task,
            &terminal_registry,
            &mut display_state,
        )
        .await;
        // The per-viewer display half is socket-scoped: the pump and
        // stream unwind on their own when the socket's channels close,
        // and this reset clears the handles so the next open starts
        // clean.
        display_state.stop_viewer();
        match held {
            Ok(()) => {
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
        git_rev: None,
        rustc: None,
        cpus: std::thread::available_parallelism()
            .ok()
            .map(|count| count.get() as u64),
        mem_kb: None,
        collected_at_unix_ms: now_ms,
    }
}

async fn hold_attachment(
    home: &str,
    pinned: &[crate::access::pinning::Fingerprint],
    identity: &crate::peer::transport::tls_client::ClientIdentityPaths,
    task: &str,
    registry: &crate::terminal::TerminalRegistry,
    display: &mut WorkerDisplayState,
) -> Result<(), String> {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = home
        .into_client_request()
        .map_err(|e| format!("bad home URL: {e}"))?;
    request.headers_mut().insert(
        "x-intendant-cloud-worker",
        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("1"),
    );
    let connector =
        crate::peer::transport::tls_client::rustls_client_config(pinned, Some(identity))
            .map_err(|e| format!("build TLS config: {e}"))?
            .map(|config| tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config)));
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
            .await
            .map_err(|e| format!("dial {home}: {e}"))?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "v": 2, "kind": HELLO_KIND, "task_id": task, "terminal": true })
            .to_string()
            .into(),
    ))
    .await
    .map_err(|e| format!("send hello: {e}"))?;
    eprintln!("[cloud-agent] attached; holding the socket");
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
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                None => break,
                Some(Ok(message)) if message.is_close() => break,
                Some(Ok(message)) => {
                    if let Ok(text) = message.into_text() {
                        serve_worker_frame(registry, text.as_str(), &out_tx, &mut forwarders, display).await;
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
        }
    }
    Ok(())
}

/// The worker's terminal server: home's authority over this worker is
/// total (the container is the sandbox), so frames arriving on the
/// authenticated attachment act as root with an unscoped spawn policy —
/// the scoped/Landlock machinery never engages. Sessions live in the
/// caller's registry so they survive a reconnect; the task turn's end
/// (or the identity expiry) tears the whole process down.
async fn serve_worker_frame(
    registry: &crate::terminal::TerminalRegistry,
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
        assert_eq!(pending.task_id, "task_e_att");
        assert!(!broker_contains_plaintext(&broker, &token));

        let consumed = consume_enrollment(&broker, &token, 2_000).unwrap();
        assert_eq!(consumed.task_id, "task_e_att");
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
    fn attach_prompt_carries_the_ceremony_and_nothing_extra() {
        let prompt = attach_prompt(
            "task_e_p",
            "wss://home.example:8443/ws",
            "ab".repeat(32).as_str(),
            "tok-secret",
        );
        assert!(prompt.contains("codex-cloud agent"));
        assert!(prompt.contains("--home wss://home.example:8443/ws"));
        assert!(prompt.contains("--task task_e_p"));
        assert!(prompt.contains("tok-secret"));
        assert!(prompt.contains("run-worker.sh"));
        assert!(prompt.contains("foreground"));
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

    #[test]
    fn home_urls_must_be_wss() {
        let err = home_url_from(Some("ws://127.0.0.1:8765/ws".into())).unwrap_err();
        assert!(err.contains("wss://"), "{err}");
        assert!(home_url_from(Some("wss://home.example:8443/ws".into())).is_ok());
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
            &tx,
            r#"{"t":"terminal_output","host_id":"cloud:x","terminal_id":"shell-0","data":"aGk="}"#,
        );
        assert!(rx.try_recv().is_ok());
        route_worker_frame(
            &tx,
            r#"{"t":"display_tiles","host_id":"cloud:x","data":"aGk="}"#,
        );
        assert!(rx.try_recv().is_ok());
        // The worker cannot inject request kinds, hellos, or junk into
        // home — its inbound authority stays nothing.
        for dropped in [
            r#"{"t":"terminal_open","host_id":"cloud:x","terminal_id":"shell-0"}"#,
            r#"{"t":"display_open","host_id":"cloud:x"}"#,
            r#"{"t":"display_input","host_id":"cloud:x","event":{"t":"mm","x":0.1,"y":0.1}}"#,
            r#"{"v":2,"kind":"cloud-worker-hello","task_id":"x"}"#,
            r#"{"t":"api_sessions"}"#,
            "not json",
        ] {
            route_worker_frame(&tx, dropped);
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
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(1024);
        let mut forwarders = std::collections::HashMap::new();
        let mut display = WorkerDisplayState {
            session: Some((0, std::sync::Arc::clone(&session))),
            ..Default::default()
        };

        let open = r#"{"t":"display_open","host_id":"cloud:t"}"#;
        serve_worker_frame(&registry, open, &out_tx, &mut forwarders, &mut display).await;
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
        serve_worker_frame(&registry, input, &out_tx, &mut forwarders, &mut display).await;

        let close = r#"{"t":"display_close","host_id":"cloud:t"}"#;
        serve_worker_frame(&registry, close, &out_tx, &mut forwarders, &mut display).await;
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
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);
        let mut forwarders = std::collections::HashMap::new();
        let open = r#"{"t":"terminal_open","host_id":"cloud:t","terminal_id":"shell-0","cols":80,"rows":24}"#;
        let mut display = WorkerDisplayState::default();
        serve_worker_frame(&registry, open, &out_tx, &mut forwarders, &mut display).await;
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
        serve_worker_frame(&registry, open, &out_tx, &mut forwarders, &mut display).await;
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
