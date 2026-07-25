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

/// Light global limiter on the public doorbell. The single-use token is
/// the real gate; this just keeps a scanner from grinding the store.
static ENROLL_RATE: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
const ENROLL_RATE_WINDOW_MS: u64 = 60_000;
const ENROLL_RATE_MAX: u32 = 30;

pub(crate) fn enroll_rate_ok(now_ms: u64) -> bool {
    let mut state = ENROLL_RATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if now_ms.saturating_sub(state.0) > ENROLL_RATE_WINDOW_MS {
        *state = (now_ms, 0);
    }
    if state.1 >= ENROLL_RATE_MAX {
        return false;
    }
    state.1 += 1;
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

// ── The attachment socket (listener lane) ──────────────────────────────

/// Serve one accepted cloud-worker WebSocket: resolve its task binding,
/// flip the lease `connected`, hold until the socket ends, flip
/// `disconnected`. The socket is the heartbeat — no frames are required
/// beyond the hello, and tungstenite answers pings during the read loop.
pub(crate) async fn serve_attachment_socket<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    fingerprint: &str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt as _;

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
    let (_write, mut read) = ws_stream.split();
    while let Some(frame) = read.next().await {
        match frame {
            Ok(message) if message.is_close() => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    match record_attachment_state(&lease_store, &task_id, AttachmentState::Disconnected) {
        Ok(_) => eprintln!("[codex-cloud] worker detached for {task_id}"),
        Err(error) => eprintln!("[codex-cloud] attachment disconnect for {task_id}: {error}"),
    }
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
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return Err(format!("home URL must be ws(s)://…, got '{url}'"));
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
                        .ok_or("--home-url requires a ws(s):// URL")?,
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
                home = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--home requires a ws(s):// URL")?,
                );
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
    if !(home.starts_with("wss://") || home.starts_with("ws://")) {
        return Err(format!("--home must be ws(s)://…, got '{home}'"));
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
    write_private(&key_path, key_pair.serialize_pem().as_bytes())?;

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
    let mut attempt: u32 = 0;
    loop {
        match hold_attachment(&home, &pinned, &identity, &task).await {
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

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

async fn hold_attachment(
    home: &str,
    pinned: &[crate::access::pinning::Fingerprint],
    identity: &crate::peer::transport::tls_client::ClientIdentityPaths,
    task: &str,
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
        serde_json::json!({ "v": 1, "kind": HELLO_KIND, "task_id": task })
            .to_string()
            .into(),
    ))
    .await
    .map_err(|e| format!("send hello: {e}"))?;
    eprintln!("[cloud-agent] attached; holding the socket");
    while let Some(frame) = ws.next().await {
        match frame {
            Ok(message) if message.is_close() => break,
            Ok(_) => {}
            Err(error) => return Err(format!("attachment socket: {error}")),
        }
    }
    Ok(())
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
