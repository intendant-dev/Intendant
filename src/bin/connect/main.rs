use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use bip39::Mnemonic;
use passkey_auth::{
    AuthenticationResponse, AuthenticationState, CredentialId, PasskeyCredential,
    RegistrationResponse, RegistrationState, Webauthn,
};
use rand::{rngs::OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex, Notify};
use url::{form_urlencoded, Url};
use uuid::Uuid;

const PROTOCOL: &str = "intendant-connect-rendezvous-v1";
const CLAIM_PROTOCOL: &str = "intendant-connect-claim-v1";
const COOKIE_NAME: &str = "ic_session";
const SESSION_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const OFFER_TIMEOUT_MS: u64 = 30_000;
const CLAIM_TIMEOUT_MS: u64 = 60_000;
const CLAIM_CODE_TTL_MS: u64 = 10 * 60 * 1000;
const CLAIM_CODE_ENTROPY_BYTES: usize = 16;
const CLAIM_CODE_GENERATION_ATTEMPTS: usize = 32;
const ACTIVE_DASHBOARD_SESSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CSRF_HEADER: &str = "x-intendant-csrf";
const FLEET_TARGET_LIMIT: usize = 100;
/// Cap on a relayed org-grant document (matches the daemon's public
/// presentation endpoint body cap).
const MAX_ORG_GRANT_RELAY_BYTES: usize = 16 * 1024;
const FLEET_TEXT_MAX: usize = 160;
/// AES-GCM envelope for the owner-encrypted private fields (three URLs
/// plus overhead, base64url) — roomy but bounded.
const FLEET_ENC_MAX: usize = 4096;
// Raw P-256 point (65B) and fixed-form signature (64B) are 87/86 chars in
// base64url; leave headroom without letting the field grow unbounded.
const FLEET_SIG_MAX: usize = 200;
const FLEET_LABEL_MAX: usize = 120;
const FLEET_URL_MAX: usize = 2048;
const FLEET_CAPABILITY_LIMIT: usize = 64;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServiceConfig::from_env_and_args()?;
    let rp_origin = Url::parse(&config.public_origin)?;
    validate_rp_id_matches_origin(&config.rp_id, &rp_origin)?;
    let webauthn = Webauthn::new(&config.rp_id, "Intendant Connect", &config.public_origin)
        .require_user_verification(true)
        .strict_base64(true);
    let mut store = load_store(&config.data_file)?;
    let had_keys = store.vapid_private_pk8_b64.is_some() && store.log_private_pk8_b64.is_some();
    let vapid = load_or_create_vapid_keypair(&mut store)?;
    let log_key = load_or_create_log_keypair(&mut store)?;
    if !had_keys {
        save_store(&config.data_file, &store).map_err(|e| format!("persist service keys: {e}"))?;
    }
    let state = Arc::new(AppState {
        config: config.clone(),
        webauthn,
        vapid,
        log_key,
        push_http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?,
        store: Mutex::new(store),
        sessions: Mutex::new(HashMap::new()),
        pending_registrations: Mutex::new(HashMap::new()),
        pending_authentications: Mutex::new(HashMap::new()),
        pending_offers: Mutex::new(HashMap::new()),
        pending_claims: Mutex::new(HashMap::new()),
        event_queues: Mutex::new(HashMap::new()),
        event_notify: Notify::new(),
        claim_codes: Mutex::new(HashMap::new()),
        rate_limits: Mutex::new(HashMap::new()),
        active_sessions: Mutex::new(HashMap::new()),
    });

    tokio::spawn(presence_alert_monitor(state.clone()));
    tokio::spawn(handle_reclaim_monitor(state.clone()));

    let app = Router::new()
        .route("/", get(landing_ui))
        .route("/connect", get(connect_ui))
        .route("/access", get(access_ui))
        .route("/app", get(app_html))
        .route("/healthz", get(healthz))
        .route("/install.sh", get(install_sh))
        .route("/install.ps1", get(install_ps1))
        .route("/favicon.png", get(favicon_png))
        .route("/logo.svg", get(logo_svg))
        .route("/assets/landing/{name}", get(landing_asset))
        .route("/readyz", get(readyz))
        .route("/api/me", get(api_me))
        .route("/api/logout", post(api_logout))
        .route("/api/auth/register/start", post(auth_register_start))
        .route("/api/auth/register/finish", post(auth_register_finish))
        .route("/api/auth/login/start", post(auth_login_start))
        .route("/api/auth/login/finish", post(auth_login_finish))
        .route("/api/daemons", get(api_daemons))
        .route("/api/daemons/{daemon_id}/revoke", post(api_daemon_revoke))
        .route("/api/daemons/{daemon_id}/label", post(api_daemon_label))
        .route("/api/fleet/targets", get(api_fleet_targets))
        .route("/api/fleet/targets/sync", post(api_fleet_targets_sync))
        .route(
            "/api/fleet/targets/{target_id}/forget",
            post(api_fleet_target_forget),
        )
        .route("/api/vault", get(api_vault_fetch).post(api_vault_publish))
        .route("/api/claims/claim", post(api_claim_start))
        .route("/api/claims/{claim_id}", get(api_claim_status))
        .route("/api/audit", get(api_audit))
        .route("/api/status", get(api_status))
        .route("/api/attest/dns", post(attest_dns))
        .route("/api/attest/github", post(attest_github))
        .route(
            "/api/directory/{handle}",
            get(directory_lookup).options(orl_preflight),
        )
        .route("/api/log/sth", get(log_sth).options(orl_preflight))
        .route("/api/log/entries", get(log_entries).options(orl_preflight))
        .route("/api/log/proof", get(log_proof).options(orl_preflight))
        .route(
            "/api/log/consistency",
            get(log_consistency).options(orl_preflight),
        )
        .route("/api/log/find", get(log_find).options(orl_preflight))
        .route("/api/push/vapid-public-key", get(push_vapid_public_key))
        .route("/api/push/subscribe", post(push_subscribe))
        .route("/api/push/unsubscribe", post(push_unsubscribe))
        .route("/api/push/test", post(push_test))
        .route(
            "/api/admin/invites",
            post(admin_invites_mint).get(admin_invites_list),
        )
        .route("/api/admin/invites/revoke", post(admin_invites_revoke))
        .route("/trust", get(trust_ui))
        .route(
            "/api/orgs/revocations/publish",
            post(orl_publish).options(orl_preflight),
        )
        .route(
            "/api/orgs/revocations",
            get(orl_fetch).options(orl_preflight),
        )
        .route("/api/daemon/register", post(daemon_register))
        .route("/api/daemon/next", get(daemon_next))
        .route("/api/daemon/answer", post(daemon_answer))
        .route("/api/daemon/error", post(daemon_error))
        .route("/api/daemon/claim-proof", post(daemon_claim_proof))
        .route("/api/daemon/dry", post(daemon_dry))
        .route("/api/browser/offer", post(browser_offer))
        .route("/api/browser/ice", post(browser_ice))
        .route("/api/browser/close", post(browser_close))
        .fallback(static_asset)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "[connect] listening on http://{} with origin {} rp_id {}",
        config.listen, config.public_origin, config.rp_id
    );
    eprintln!("[connect] state file {}", config.data_file.display());
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    listen: SocketAddr,
    public_origin: String,
    rp_id: String,
    static_root: PathBuf,
    data_file: PathBuf,
    daemon_token: Option<String>,
    cookie_secure: bool,
    /// Refuse new-account registration without a valid invite code.
    /// Off by default so self-hosted instances stay zero-friction; the
    /// hosted instance turns it on. Existing accounts are unaffected.
    invite_required: bool,
    /// Let daemons register and poll without the bearer token, even when
    /// one is configured: registration is rate-limited, unclaimed records
    /// expire after a day, and the gate moves to claim time (only
    /// signed-in — on the hosted instance, invited — accounts can claim).
    /// The token keeps guarding the admin surface regardless. This is
    /// what makes the landing one-liner's claim story reachable by
    /// someone who has never seen the operator token.
    open_daemon_registration: bool,
}

impl ServiceConfig {
    fn from_env_and_args() -> Result<Self, String> {
        let mut listen: SocketAddr = std::env::var("INTENDANT_CONNECT_LISTEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 9876)));
        let mut public_origin = std::env::var("INTENDANT_CONNECT_ORIGIN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut rp_id = std::env::var("INTENDANT_CONNECT_RP_ID")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut static_root = std::env::var("INTENDANT_CONNECT_STATIC_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("static"));
        let mut data_file = std::env::var("INTENDANT_CONNECT_DATA_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_data_file());
        let mut invite_required = std::env::var("INTENDANT_CONNECT_INVITE_REQUIRED")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let mut daemon_token = std::env::var("INTENDANT_CONNECT_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let mut open_daemon_registration = std::env::var("INTENDANT_CONNECT_OPEN_REGISTRATION")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--listen" => {
                    let value = args.next().ok_or("--listen requires an address")?;
                    listen = value
                        .parse()
                        .map_err(|e| format!("invalid --listen {value:?}: {e}"))?;
                }
                "--origin" => {
                    public_origin = Some(args.next().ok_or("--origin requires a URL")?);
                }
                "--rp-id" => {
                    rp_id = Some(args.next().ok_or("--rp-id requires a domain")?);
                }
                "--static-root" => {
                    static_root =
                        PathBuf::from(args.next().ok_or("--static-root requires a path")?);
                }
                "--data-file" => {
                    data_file = PathBuf::from(args.next().ok_or("--data-file requires a path")?);
                }
                "--daemon-token" => {
                    daemon_token = Some(args.next().ok_or("--daemon-token requires a token")?);
                }
                "--invite-required" => {
                    invite_required = true;
                }
                "--open-registration" => {
                    open_daemon_registration = true;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        let public_origin =
            public_origin.unwrap_or_else(|| format!("http://localhost:{}", listen.port()));
        let parsed_origin = Url::parse(&public_origin)
            .map_err(|e| format!("invalid Connect origin {public_origin:?}: {e}"))?;
        let rp_id = rp_id.unwrap_or_else(|| {
            let host = parsed_origin.host_str().unwrap_or("localhost");
            if host == "intendant.dev" || host.ends_with(".intendant.dev") {
                "intendant.dev".to_string()
            } else {
                host.to_string()
            }
        });
        let cookie_secure = parsed_origin.scheme() == "https";
        Ok(Self {
            listen,
            public_origin: trim_trailing_slash(&public_origin),
            rp_id,
            static_root,
            data_file,
            daemon_token,
            invite_required,
            open_daemon_registration,
            cookie_secure,
        })
    }
}

fn print_help() {
    println!(
        "Usage: intendant-connect [--listen 127.0.0.1:9876] [--origin https://connect.intendant.dev] [--rp-id intendant.dev]\n\
         \n\
         Env: INTENDANT_CONNECT_LISTEN, INTENDANT_CONNECT_ORIGIN, INTENDANT_CONNECT_RP_ID,\n\
              INTENDANT_CONNECT_STATIC_ROOT, INTENDANT_CONNECT_DATA_FILE, INTENDANT_CONNECT_TOKEN,\n\
              INTENDANT_CONNECT_INVITE_REQUIRED, INTENDANT_CONNECT_OPEN_REGISTRATION"
    );
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn default_data_file() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("connect")
        .join("state.json")
}

fn validate_rp_id_matches_origin(rp_id: &str, origin: &Url) -> Result<(), String> {
    let host = origin
        .host_str()
        .ok_or_else(|| "Connect origin must include a host".to_string())?;
    if host == rp_id || host.ends_with(&format!(".{rp_id}")) {
        Ok(())
    } else {
        Err(format!(
            "rp_id {rp_id:?} is not an effective domain of origin host {host:?}"
        ))
    }
}

struct AppState {
    config: ServiceConfig,
    webauthn: Webauthn,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    pending_registrations: Mutex<HashMap<String, PendingRegistration>>,
    pending_authentications: Mutex<HashMap<String, PendingAuthentication>>,
    pending_offers: Mutex<HashMap<String, PendingOffer>>,
    pending_claims: Mutex<HashMap<String, PendingClaim>>,
    event_queues: Mutex<HashMap<String, VecDeque<RendezvousEvent>>>,
    event_notify: Notify,
    claim_codes: Mutex<HashMap<String, String>>,
    rate_limits: Mutex<HashMap<String, RateLimitBucket>>,
    active_sessions: Mutex<HashMap<String, ActiveDashboardSession>>,
    vapid: ring::signature::EcdsaKeyPair,
    log_key: ring::signature::EcdsaKeyPair,
    push_http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    users: Vec<UserRecord>,
    #[serde(default)]
    daemons: Vec<DaemonRecord>,
    #[serde(default)]
    fleet_targets: Vec<FleetTargetRecord>,
    #[serde(default)]
    audit: Vec<AuditEvent>,
    // Org revocation-list bulletin board (zero authority): the latest
    // root-signed list per (handle, root key), stored blind. Signatures
    // are checked only to keep the store clean and the sequence check
    // only prevents rollback — consumers re-verify everything.
    #[serde(default)]
    orl_bulletins: Vec<OrlBulletinRecord>,
    // Credential vault blobs (credential custody): one end-to-end
    // encrypted vault per user, stored blind. The service sees only
    // ciphertext + envelope metadata; the revision check prevents
    // rollback (the ORL `seq` trick) — devices re-verify everything.
    #[serde(default)]
    vault_blobs: Vec<VaultBlobRecord>,
    // Invite codes for gated registration. Only hashes are stored; a
    // code is a bearer secret shown once at mint time.
    #[serde(default)]
    invites: Vec<InviteRecord>,
    // Web Push: the service's VAPID signing key (PKCS#8 DER, base64)
    // and per-user browser subscriptions.
    #[serde(default)]
    vapid_private_pk8_b64: Option<String>,
    #[serde(default)]
    push_subscriptions: Vec<PushSubscriptionRecord>,
    // Transparency log: append-only name-binding events (RFC 6962-shaped
    // Merkle tree over the serialized entries) + its dedicated STH
    // signing key. Entries store their exact leaf bytes so the tree is
    // stable across serde/schema evolution forever.
    #[serde(default)]
    log_private_pk8_b64: Option<String>,
    #[serde(default)]
    log_entries: Vec<LogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogEntry {
    unix_ms: u64,
    kind: String,
    /// Canonical serialized event — the exact bytes that are leaf-hashed.
    leaf_json: String,
}

/// Append a name-binding event. MUST be called inside the same store
/// lock as the mutation it witnesses, before persist, so the log and the
/// state can never disagree about what happened.
fn append_log_entry(store: &mut Store, kind: &str, mut data: serde_json::Value) {
    let unix_ms = now_unix_ms();
    if let Some(map) = data.as_object_mut() {
        map.insert("kind".to_string(), json!(kind));
        map.insert("unix_ms".to_string(), json!(unix_ms));
    }
    store.log_entries.push(LogEntry {
        unix_ms,
        kind: kind.to_string(),
        leaf_json: data.to_string(),
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushSubscriptionRecord {
    user_id: Uuid,
    endpoint: String,
    /// Browser-held ECDH public key (65-byte uncompressed point, b64url).
    p256dh: String,
    /// 16-byte auth secret, b64url.
    auth: String,
    #[serde(default)]
    label: String,
    created_unix_ms: u64,
    /// Alert when a claimed daemon goes offline / comes back.
    #[serde(default)]
    notify_presence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InviteRecord {
    code_hash: String,
    #[serde(default)]
    label: String,
    created_unix_ms: u64,
    max_uses: u32,
    #[serde(default)]
    used_count: u32,
    #[serde(default)]
    revoked: bool,
}

fn invite_usable(invite: &InviteRecord) -> bool {
    !invite.revoked && invite.used_count < invite.max_uses
}

/// Handles nobody should be able to squat: infrastructure names, major
/// brands, and anything that reads as official. Short handles (< 3
/// chars) are reserved wholesale by the length rule.
const RESERVED_HANDLES: &[&str] = &[
    "admin",
    "administrator",
    "root",
    "system",
    "staff",
    "official",
    "support",
    "help",
    "security",
    "abuse",
    "moderator",
    "mod",
    "team",
    "info",
    "contact",
    "billing",
    "payments",
    "postmaster",
    "webmaster",
    "hostmaster",
    "noreply",
    "no-reply",
    "mail",
    "email",
    "api",
    "www",
    "app",
    "web",
    "dashboard",
    "status",
    "blog",
    "docs",
    "news",
    "dev",
    "test",
    "demo",
    "example",
    "intendant",
    "connect",
    "rendezvous",
    "daemon",
    "trust",
    "access",
    "google",
    "github",
    "apple",
    "microsoft",
    "amazon",
    "meta",
    "facebook",
    "openai",
    "anthropic",
    "claude",
    "gemini",
    "codex",
    "twitter",
    "x",
];

/// Account handles: 3-32 chars of a-z, 0-9, and '-' (no leading/trailing
/// dash), and not on the reserved list.
fn validate_account_name(name: &str) -> Result<(), String> {
    if name.len() < 3 || name.len() > 32 {
        return Err("handle must be 3-32 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err("handle may use a-z, 0-9, and '-' (not at the ends)".to_string());
    }
    if RESERVED_HANDLES.contains(&name) {
        return Err("that handle is reserved".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrlBulletinRecord {
    handle: String,
    root_key: String,
    seq: u64,
    list: serde_json::Value,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultBlobRecord {
    user_id: Uuid,
    revision: u64,
    vault: serde_json::Value,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserRecord {
    id: Uuid,
    account_name: String,
    display_name: String,
    passkeys: Vec<PasskeyCredential>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    #[serde(default)]
    last_login_unix_ms: u64,
    #[serde(default)]
    attestations: Vec<AttestationRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttestationRecord {
    /// "dns" or "github".
    kind: String,
    /// The external identity: a domain, or "github:<user>".
    subject: String,
    verified_unix_ms: u64,
    /// Where the proof lives (TXT record name / raw file URL).
    proof: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonRecord {
    daemon_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    daemon_public_key: String,
    owner_user_id: Option<Uuid>,
    claim_code_hash: Option<String>,
    claim_code_created_unix_ms: Option<u64>,
    registered_unix_ms: u64,
    last_seen_unix_ms: u64,
    updated_unix_ms: u64,
    /// Hours (unix_ms / 3_600_000) in which this daemon polled at least
    /// once — the last week of them. Pure display data: the service
    /// already sees every poll; this just remembers which hours had one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    presence_hours: Vec<u64>,
}

const PRESENCE_HOURS_KEPT: usize = 168; // 7 days

fn record_presence_hour(hours: &mut Vec<u64>, now_unix_ms: u64) -> bool {
    let hour = now_unix_ms / 3_600_000;
    if hours.last() == Some(&hour) {
        return false;
    }
    hours.push(hour);
    if hours.len() > PRESENCE_HOURS_KEPT {
        let excess = hours.len() - PRESENCE_HOURS_KEPT;
        hours.drain(0..excess);
    }
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FleetTargetRecord {
    user_id: Uuid,
    id: String,
    host_id: String,
    label: String,
    #[serde(default)]
    local: bool,
    source: String,
    #[serde(default)]
    access_domain: String,
    #[serde(default)]
    access_domain_label: String,
    #[serde(default)]
    route: String,
    #[serde(default)]
    route_label: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    auth_label: String,
    #[serde(default)]
    effective_role: String,
    #[serde(default)]
    effective_role_label: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ws_url: String,
    #[serde(default)]
    browser_tcp_via_url: String,
    // The daemon-advertised rendezvous base (phase 7) — part of the signed
    // v2 record payload, relayed verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    connect_signaling_base: String,
    // Owner-encrypted private fields (phase 5 follow-on): an opaque
    // envelope only devices holding the passkey-PRF key can open. The
    // service stores it blind.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    enc_fields: String,
    #[serde(default)]
    origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_daemon_id: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    // Owner-signature passthrough (trust architecture phase 5): the browser
    // signs its own records with its identity key and verifies them on
    // read, so this store cannot silently inject or alter fleet entries.
    // The service never interprets these fields.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    record_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    record_sig: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    record_signed_at_unix_ms: u64,
    first_seen_unix_ms: u64,
    last_seen_unix_ms: u64,
    updated_unix_ms: u64,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditEvent {
    id: String,
    unix_ms: u64,
    event: String,
    user_id: Option<Uuid>,
    daemon_id: Option<String>,
    detail: serde_json::Value,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    user_id: Uuid,
    csrf_token: String,
    expires_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct RateLimitBucket {
    window_start_unix_ms: u64,
    count: u32,
}

#[derive(Debug, Clone)]
struct ActiveDashboardSession {
    daemon_id: String,
    session_id: String,
    created_unix_ms: u64,
}

struct PendingRegistration {
    user_id: Uuid,
    account_name: String,
    display_name: String,
    new_account: bool,
    invite_code_hash: Option<String>,
    state: RegistrationState,
    expires_unix_ms: u64,
}

struct PendingAuthentication {
    user_id: Uuid,
    state: AuthenticationState,
    expires_unix_ms: u64,
}

struct PendingOffer {
    daemon_id: String,
    user_id: Uuid,
    daemon_public_key: String,
    session_grant: String,
    response_tx: oneshot::Sender<Result<BrowserAnswerResponse, String>>,
}

#[derive(Debug, Clone)]
struct PendingClaim {
    user_id: Uuid,
    daemon_id: String,
    challenge: String,
    created_unix_ms: u64,
    status: ClaimStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ClaimStatus {
    Pending,
    Approved { daemon_id: String },
    Rejected { error: String },
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": self.message,
            })),
        )
            .into_response()
    }
}

fn load_store(path: &Path) -> Result<Store, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse Connect state {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("read Connect state {}: {e}", path.display())),
    }
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize state: {e}"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create Connect state dir {}: {e}", parent.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".intendant-connect-state-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| format!("create Connect state tempfile in {}: {e}", parent.display()))?;
    tmp.write_all(&bytes)
        .map_err(|e| format!("write Connect state tempfile {}: {e}", tmp.path().display()))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|e| format!("sync Connect state tempfile {}: {e}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| format!("replace Connect state {}: {}", path.display(), e.error))?;
    Ok(())
}

fn persist_locked(state: &AppState, store: &Store) -> ApiResult<()> {
    save_store(&state.config.data_file, store).map_err(ApiError::internal)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn random_b64u(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    b64u(&buf)
}

fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64u_decode(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
}

fn sha256_b64u(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    b64u(&hasher.finalize())
}

fn normalize_account_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn user_view(user: &UserRecord) -> serde_json::Value {
    json!({
        "id": user.id,
        "account_name": user.account_name,
        "display_name": user.display_name,
        "passkey_count": user.passkeys.len(),
        "attestations": user
            .attestations
            .iter()
            .map(|a| json!({ "kind": a.kind, "subject": a.subject, "verified_unix_ms": a.verified_unix_ms }))
            .collect::<Vec<_>>(),
    })
}

fn daemon_view(daemon: &DaemonRecord) -> serde_json::Value {
    let now = now_unix_ms();
    json!({
        "daemon_id": daemon.daemon_id,
        "label": daemon.label,
        "daemon_public_key": daemon.daemon_public_key,
        "claimed": daemon.owner_user_id.is_some(),
        "online": now.saturating_sub(daemon.last_seen_unix_ms) < 45_000,
        "presence_hours": daemon.presence_hours,
        "registered_unix_ms": daemon.registered_unix_ms,
        "last_seen_unix_ms": daemon.last_seen_unix_ms,
    })
}

fn daemon_fleet_target_view(config: &ServiceConfig, daemon: &DaemonRecord) -> serde_json::Value {
    let now = now_unix_ms();
    let label = daemon
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or(&daemon.daemon_id);
    let url = format!(
        "/app?connect=1&daemon_id={}",
        form_urlencoded::byte_serialize(daemon.daemon_id.as_bytes()).collect::<String>()
    );
    let online = now.saturating_sub(daemon.last_seen_unix_ms) < 45_000;
    json!({
        "id": daemon.daemon_id,
        "host_id": daemon.daemon_id,
        "label": label,
        "local": false,
        "source": "connect_daemon",
        "access_domain": "user_client",
        "access_domain_label": "User/client access",
        "route": "hosted_connect",
        "route_label": "Hosted Connect",
        "auth": "connect_account",
        "auth_label": "Connect account",
        "effective_role": "root",
        "effective_role_label": "Root",
        "profile": "",
        "connected": online,
        "online": online,
        "claimed_daemon": true,
        "daemon_public_key": daemon.daemon_public_key,
        "url": url,
        "ws_url": "",
        "browser_tcp_via_url": "",
        "origin": config.public_origin,
        "connect_daemon_id": daemon.daemon_id,
        "capabilities": [],
        "first_seen_unix_ms": daemon.registered_unix_ms,
        "last_seen_unix_ms": daemon.last_seen_unix_ms,
        "updated_unix_ms": daemon.updated_unix_ms,
    })
}

fn fleet_target_view(target: &FleetTargetRecord) -> serde_json::Value {
    json!({
        "id": target.id,
        "host_id": target.host_id,
        "label": target.label,
        "local": target.local,
        "source": target.source,
        "access_domain": target.access_domain,
        "access_domain_label": target.access_domain_label,
        "route": target.route,
        "route_label": target.route_label,
        "auth": target.auth,
        "auth_label": target.auth_label,
        "effective_role": target.effective_role,
        "effective_role_label": target.effective_role_label,
        "profile": target.profile,
        "connected": false,
        "online": false,
        "claimed_daemon": false,
        "daemon_public_key": "",
        "url": target.url,
        "ws_url": target.ws_url,
        "browser_tcp_via_url": target.browser_tcp_via_url,
        "connect_signaling_base": target.connect_signaling_base,
        "enc_fields": target.enc_fields,
        "origin": target.origin,
        "connect_daemon_id": target.connect_daemon_id,
        "capabilities": target.capabilities,
        "record_key": target.record_key,
        "record_sig": target.record_sig,
        "record_signed_at_unix_ms": target.record_signed_at_unix_ms,
        "first_seen_unix_ms": target.first_seen_unix_ms,
        "last_seen_unix_ms": target.last_seen_unix_ms,
        "updated_unix_ms": target.updated_unix_ms,
    })
}

fn audit(
    store: &mut Store,
    event: &str,
    user_id: Option<Uuid>,
    daemon_id: Option<String>,
    detail: serde_json::Value,
) {
    store.audit.push(AuditEvent {
        id: Uuid::new_v4().to_string(),
        unix_ms: now_unix_ms(),
        event: event.to_string(),
        user_id,
        daemon_id,
        detail,
    });
    const MAX_AUDIT_EVENTS: usize = 2000;
    if store.audit.len() > MAX_AUDIT_EVENTS {
        let drop_count = store.audit.len() - MAX_AUDIT_EVENTS;
        store.audit.drain(0..drop_count);
    }
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

/// The bootstrap installer (credential custody, rollout step 6), embedded
/// at build time so the service — hosted or self-hosted — serves the
/// installer that matches its own version:
///   curl -fsSL <origin>/install.sh | sh -s -- --owner <fingerprint>
///
/// Served with this rendezvous' public origin injected as the default
/// `--connect` URL: fetching the installer from a rendezvous IS the opt-in,
/// and a fresh VPS has no other way to learn where to register — without
/// it the daemon comes up unregistered and hosted claiming dead-ends.
/// (A compiled-in default in the daemon would instead make every install
/// phone home to intendant.dev; serve-time injection keeps self-hosting
/// exact.) Explicit `--connect` / `-Connect` still wins over the default.
const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");
const INSTALL_SH_CONNECT_DEFAULT: &str = r#"CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-}""#;

/// Only a plain URL charset may be spliced into the scripts — anything
/// else (quotes, spaces, `$`) could change what the shell parses. A
/// misconfigured origin falls back to serving the script verbatim.
fn connect_default_injectable(origin: &str) -> bool {
    !origin.is_empty()
        && origin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-' | '_'))
}

fn install_sh_body(public_origin: &str) -> String {
    if !connect_default_injectable(public_origin) {
        return INSTALL_SH.to_string();
    }
    INSTALL_SH.replacen(
        INSTALL_SH_CONNECT_DEFAULT,
        &format!(r#"CONNECT_URL="${{INTENDANT_CONNECT_RENDEZVOUS_URL:-{public_origin}}}""#),
        1,
    )
}

async fn install_sh(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        install_sh_body(&state.config.public_origin),
    )
}

/// The Windows counterpart, for PowerShell:
///   & ([scriptblock]::Create((irm <origin>/install.ps1))) -Owner <fingerprint>
const INSTALL_PS1: &str = include_str!("../../../scripts/install.ps1");
const INSTALL_PS1_CONNECT_DEFAULT: &str = "    [string]$Connect = \"\",";

fn install_ps1_body(public_origin: &str) -> String {
    if !connect_default_injectable(public_origin) {
        return INSTALL_PS1.to_string();
    }
    INSTALL_PS1.replacen(
        INSTALL_PS1_CONNECT_DEFAULT,
        &format!("    [string]$Connect = \"{public_origin}\","),
        1,
    )
}

async fn install_ps1(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        install_ps1_body(&state.config.public_origin),
    )
}

/// The canonical Intendant mark, embedded so every page this binary serves
/// gets the real logo without a static root. `static/logo.svg` is the
/// macOS icon vector (macos-app/icon.svg) with the dock margin cropped in
/// viewBox space; the PNG fallback is rendered from it (`rsvg-convert -w 128`).
const LOGO_SVG: &str = include_str!("../../../static/logo.svg");
const BRAND_ICON_PNG: &[u8] = include_bytes!("../../../static/icon-128.png");

async fn logo_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        LOGO_SVG,
    )
}

async fn favicon_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BRAND_ICON_PNG,
    )
}

/// Product screenshots for the landing page, embedded like the installer so
/// every deployment serves visuals that match its own UI. Captured from a
/// staged local rig (daemon "atlas", account "@ada") — synthetic content only.
fn landing_asset_bytes(name: &str) -> Option<&'static [u8]> {
    match name {
        "hero.webp" => Some(include_bytes!("assets/landing-hero.webp")),
        "video.webp" => Some(include_bytes!("assets/landing-video.webp")),
        "vault.webp" => Some(include_bytes!("assets/landing-vault.webp")),
        "station.webp" => Some(include_bytes!("assets/landing-station.webp")),
        "claim.webp" => Some(include_bytes!("assets/landing-claim.webp")),
        "phone.webp" => Some(include_bytes!("assets/landing-phone.webp")),
        _ => None,
    }
}

async fn landing_asset(AxumPath(name): AxumPath<String>) -> Response {
    match landing_asset_bytes(&name) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let app_html = state.config.static_root.join("app.html");
    let static_ok = app_html.is_file();
    let state_parent_ok = state
        .config
        .data_file
        .parent()
        .map(|parent| parent.exists() || std::fs::create_dir_all(parent).is_ok())
        .unwrap_or(false);
    let ok = static_ok && state_parent_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "ok": ok,
            "static_app": static_ok,
            "state_parent": state_parent_ok,
        })),
    )
        .into_response()
}

async fn landing_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(landing_ui_html(&state.config.public_origin))
}

async fn connect_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Connect",
        "Rendezvous account",
    ))
}

async fn trust_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(trust_ui_html(&state.config.public_origin))
}

async fn access_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Access",
        "Rendezvous and fleet navigation",
    ))
}

async fn app_html(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    if !valid_connect_app_query(uri.query()) {
        return Ok(Redirect::to("/connect").into_response());
    }
    let path = state.config.static_root.join("app.html");
    serve_file(&state.config.static_root, &path)
}

fn valid_connect_app_query(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    let mut connect_mode = false;
    let mut daemon_id = false;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "connect" => connect_mode = value == "1",
            "daemon_id" => daemon_id = !value.trim().is_empty(),
            _ => {}
        }
    }
    connect_mode && daemon_id
}

async fn static_asset(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    let path = safe_static_path(&state.config.static_root, uri.path())
        .ok_or_else(|| ApiError::not_found("not found"))?;
    serve_file(&state.config.static_root, &path)
}

fn safe_static_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let trimmed = uri_path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let rel = Path::new(trimmed);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

fn serve_file(root: &Path, path: &Path) -> ApiResult<Response> {
    if !path.starts_with(root) || !path.is_file() {
        return Err(ApiError::not_found("not found"));
    }
    let body = std::fs::read(path).map_err(|e| ApiError::not_found(format!("not found: {e}")))?;
    let content_type = content_type_for_path(path);
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response())
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let (k, v) = part.trim().split_once('=').unwrap_or((part.trim(), ""));
        if k == name && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn session_cookie(config: &ServiceConfig, token: &str, max_age_seconds: u64) -> HeaderValue {
    let mut cookie =
        format!("{COOKIE_NAME}={token}; Max-Age={max_age_seconds}; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn clear_session_cookie(config: &ServiceConfig) -> HeaderValue {
    let mut cookie = format!("{COOKIE_NAME}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn optional_user(state: &Arc<AppState>, headers: &HeaderMap) -> Option<UserRecord> {
    let token = cookie_value(headers, COOKIE_NAME)?;
    let now = now_unix_ms();
    let user_id = {
        let mut sessions = state.sessions.lock().await;
        let session = sessions.get(&token)?;
        if session.expires_unix_ms <= now {
            sessions.remove(&token);
            return None;
        }
        session.user_id
    };
    let store = state.store.lock().await;
    store.users.iter().find(|u| u.id == user_id).cloned()
}

async fn require_user(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<UserRecord> {
    optional_user(state, headers)
        .await
        .ok_or_else(|| ApiError::unauthorized("sign in required"))
}

async fn create_session(state: &Arc<AppState>, user_id: Uuid) -> (String, String) {
    let token = random_b64u(32);
    let csrf_token = random_b64u(32);
    let session = SessionRecord {
        user_id,
        csrf_token: csrf_token.clone(),
        expires_unix_ms: now_unix_ms().saturating_add(SESSION_TTL_MS),
    };
    state.sessions.lock().await.insert(token.clone(), session);
    (token, csrf_token)
}

// ── Transparency log: RFC 6962 Merkle tree over name-binding events ──
//
// The service commits to every consequential binding it hands out
// (daemon_id → daemon key at claim time, handle creation, org
// revocation-list sightings, attestations) in an append-only log.
// Browsers pin the signed tree head and verify consistency on every
// visit, so rewriting or forking history is detectable — the rendezvous
// stays zero-authority AND becomes checkable about the one thing it
// could quietly lie about: first introductions.

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn log_leaf_hash(leaf_json: &str) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + leaf_json.len());
    buf.push(0x00);
    buf.extend_from_slice(leaf_json.as_bytes());
    sha256(&buf)
}

fn log_node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(65);
    buf.push(0x01);
    buf.extend_from_slice(left);
    buf.extend_from_slice(right);
    sha256(&buf)
}

/// Largest power of two strictly less than n (n >= 2).
fn log_split_point(n: usize) -> usize {
    let mut k = 1usize;
    while k * 2 < n {
        k *= 2;
    }
    k
}

/// MTH(D[n]) per RFC 6962 §2.1.
fn log_tree_root(leaves: &[[u8; 32]]) -> [u8; 32] {
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
fn log_inclusion_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
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
fn log_consistency_proof(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
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
fn log_verify_inclusion(
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
fn log_verify_consistency(
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

fn load_or_create_log_keypair(store: &mut Store) -> Result<ring::signature::EcdsaKeyPair, String> {
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

fn log_sth_payload(size: usize, root_b64u: &str, unix_ms: u64) -> String {
    format!("intendant-log-sth-v1\n{size}\n{root_b64u}\n{unix_ms}")
}

// ── Web Push (RFC 8291 payload encryption + RFC 8292 VAPID), pure ring ──
//
// The service authors only presence alerts — facts it inherently knows
// from the polling it exists to do. Payloads are still encrypted to the
// browser subscription (the push relay in the middle sees ciphertext),
// and the VAPID key proves the sender to the push service.

struct HkdfLen(usize);
impl ring::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_expand(prk: &ring::hkdf::Prk, info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], HkdfLen(len))
        .expect("hkdf expand length is valid")
        .fill(&mut out)
        .expect("hkdf fill length matches");
    out
}

/// Encrypt `plaintext` for a browser push subscription (RFC 8291,
/// aes128gcm coding). Returns the full request body: the RFC 8188
/// header block (salt, record size, ephemeral public key) followed by
/// the single encrypted record.
fn webpush_encrypt(
    ua_public_b64u: &str,
    auth_secret_b64u: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let ua_public = b64u_decode(ua_public_b64u.trim())
        .map_err(|_| "subscription p256dh is not valid base64url".to_string())?;
    let auth_secret = b64u_decode(auth_secret_b64u.trim())
        .map_err(|_| "subscription auth is not valid base64url".to_string())?;
    if ua_public.len() != 65 || auth_secret.len() != 16 {
        return Err("subscription keys have unexpected lengths".to_string());
    }

    let rng = ring::rand::SystemRandom::new();
    let eph_private =
        ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .map_err(|_| "ephemeral key generation failed".to_string())?;
    let eph_public = eph_private
        .compute_public_key()
        .map_err(|_| "ephemeral public key computation failed".to_string())?;
    let peer =
        ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, ua_public.clone());
    let ecdh_secret =
        ring::agreement::agree_ephemeral(eph_private, &peer, |secret| secret.to_vec())
            .map_err(|_| "ECDH agreement failed (bad subscription key?)".to_string())?;

    // IKM = HKDF(salt=auth_secret, ikm=ecdh_secret, info="WebPush: info"||0||ua_pub||as_pub, 32)
    let mut info = b"WebPush: info\x00".to_vec();
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(eph_public.as_ref());
    let prk_key =
        ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &auth_secret).extract(&ecdh_secret);
    let ikm = hkdf_expand(&prk_key, &info, 32);

    let mut salt = [0u8; 16];
    ring::rand::SecureRandom::fill(&rng, &mut salt)
        .map_err(|_| "salt generation failed".to_string())?;
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &salt).extract(&ikm);
    let cek = hkdf_expand(&prk, b"Content-Encoding: aes128gcm\x00", 16);
    let nonce = hkdf_expand(&prk, b"Content-Encoding: nonce\x00", 12);

    // Single record: plaintext || 0x02 (last-record delimiter), sealed.
    let mut record = plaintext.to_vec();
    record.push(0x02);
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek)
            .map_err(|_| "content key setup failed".to_string())?,
    );
    let nonce = ring::aead::Nonce::try_assume_unique_for_key(&nonce)
        .map_err(|_| "nonce setup failed".to_string())?;
    key.seal_in_place_append_tag(nonce, ring::aead::Aad::empty(), &mut record)
        .map_err(|_| "payload encryption failed".to_string())?;

    // RFC 8188 header: salt(16) || rs(4) || idlen(1) || keyid(as_public)
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(eph_public.as_ref());
    body.extend_from_slice(&record);
    Ok(body)
}

/// RFC 8292 `Authorization: vapid t=<jwt>, k=<pub>` for one endpoint.
fn vapid_authorization(
    keypair: &ring::signature::EcdsaKeyPair,
    endpoint: &str,
    contact: &str,
) -> Result<String, String> {
    use ring::signature::KeyPair as _;
    let endpoint_url =
        url::Url::parse(endpoint).map_err(|_| "subscription endpoint is not a URL".to_string())?;
    let audience = format!(
        "{}://{}",
        endpoint_url.scheme(),
        endpoint_url
            .host_str()
            .map(|host| match endpoint_url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            })
            .ok_or_else(|| "subscription endpoint has no host".to_string())?
    );
    let header = b64u(br#"{"typ":"JWT","alg":"ES256"}"#.as_slice());
    let claims = b64u(
        json!({
            "aud": audience,
            "exp": (now_unix_ms() / 1000) + 12 * 3600,
            "sub": contact,
        })
        .to_string()
        .as_bytes(),
    );
    let signing_input = format!("{header}.{claims}");
    let rng = ring::rand::SystemRandom::new();
    let signature = keypair
        .sign(&rng, signing_input.as_bytes())
        .map_err(|_| "VAPID signing failed".to_string())?;
    let public_b64u = b64u(keypair.public_key().as_ref());
    Ok(format!(
        "vapid t={signing_input}.{}, k={public_b64u}",
        b64u(signature.as_ref())
    ))
}

/// Fire one encrypted notification at a subscription. Returns Ok(false)
/// when the push service says the subscription is gone (prune it).
async fn send_web_push(
    http: &reqwest::Client,
    keypair: &ring::signature::EcdsaKeyPair,
    contact: &str,
    subscription: &PushSubscriptionRecord,
    payload: &serde_json::Value,
) -> Result<bool, String> {
    let body = webpush_encrypt(
        &subscription.p256dh,
        &subscription.auth,
        payload.to_string().as_bytes(),
    )?;
    let authorization = vapid_authorization(keypair, &subscription.endpoint, contact)?;
    let response = http
        .post(&subscription.endpoint)
        .header("authorization", authorization)
        .header("content-encoding", "aes128gcm")
        .header("ttl", "86400")
        .header("urgency", "normal")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("push send failed: {e}"))?;
    match response.status().as_u16() {
        200..=299 => Ok(true),
        404 | 410 => Ok(false),
        status => Err(format!("push service returned {status}")),
    }
}

fn load_or_create_vapid_keypair(
    store: &mut Store,
) -> Result<ring::signature::EcdsaKeyPair, String> {
    let rng = ring::rand::SystemRandom::new();
    if store.vapid_private_pk8_b64.is_none() {
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .map_err(|_| "VAPID key generation failed".to_string())?;
        store.vapid_private_pk8_b64 = Some(b64u(document.as_ref()));
    }
    let der = b64u_decode(store.vapid_private_pk8_b64.as_deref().unwrap_or(""))
        .map_err(|_| "stored VAPID key is not valid base64".to_string())?;
    ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &der,
        &rng,
    )
    .map_err(|_| "stored VAPID key is invalid".to_string())
}

// ── Attestations: bind a handle to an external identity, as decoration ──
//
// Verification never gates anything: handles stay first-come and keys
// stay the identity. An attestation is a checkable claim ("this handle
// is held by whoever controls example.com / github.com/user") shown as
// a badge and committed to the transparency log.

/// The exact string a proof must contain, e.g.
/// `intendant-handle=lenny@connect.intendant.dev`.
fn attestation_claim_string(config: &ServiceConfig, handle: &str) -> String {
    // Mirrors the browser's `location.host`: hostname plus the port
    // when it is not the scheme default.
    let host = Url::parse(&config.public_origin)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(port) => format!("{h}:{port}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    format!("intendant-handle={handle}@{host}")
}

fn upsert_attestation(user: &mut UserRecord, kind: &str, subject: String, proof: String) {
    user.attestations
        .retain(|a| !(a.kind == kind && a.subject == subject));
    user.attestations.push(AttestationRecord {
        kind: kind.to_string(),
        subject,
        verified_unix_ms: now_unix_ms(),
        proof,
    });
}

async fn record_verified_attestation(
    state: &Arc<AppState>,
    user_id: Uuid,
    kind: &str,
    subject: &str,
    proof: &str,
) -> ApiResult<serde_json::Value> {
    let mut store = state.store.lock().await;
    let handle = {
        let user = store
            .users
            .iter_mut()
            .find(|u| u.id == user_id)
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        upsert_attestation(user, kind, subject.to_string(), proof.to_string());
        user.account_name.clone()
    };
    append_log_entry(
        &mut store,
        "attestation",
        json!({ "handle": handle, "attestation_kind": kind, "subject": subject }),
    );
    audit(
        &mut store,
        "attestation_verified",
        Some(user_id),
        None,
        json!({ "kind": kind, "subject": subject }),
    );
    persist_locked(state, &store)?;
    Ok(json!({ "ok": true, "kind": kind, "subject": subject }))
}

#[derive(Debug, Deserialize)]
struct AttestDnsRequest {
    domain: String,
}

/// Verify a `_intendant.<domain>` TXT record via DNS-over-HTTPS (no
/// resolver dependency; override the DoH URL for tests/self-hosters
/// with INTENDANT_CONNECT_DOH_URL).
async fn attest_dns(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AttestDnsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "attest", 10, 600_000).await?;
    let domain = body
        .domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty()
        || domain.len() > 253
        || !domain.contains('.')
        || !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ApiError::bad_request("that does not look like a domain"));
    }
    let expected = attestation_claim_string(&state.config, &user.account_name);
    let doh_base = std::env::var("INTENDANT_CONNECT_DOH_URL")
        .unwrap_or_else(|_| "https://cloudflare-dns.com/dns-query".to_string());
    let response = state
        .push_http
        .get(&doh_base)
        .query(&[
            ("name", format!("_intendant.{domain}")),
            ("type", "TXT".to_string()),
        ])
        .header("accept", "application/dns-json")
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("DNS lookup failed: {e}")))?;
    let answer: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ApiError::bad_request(format!("DNS response unreadable: {e}")))?;
    let found = answer
        .get("Answer")
        .and_then(|a| a.as_array())
        .map(|records| {
            records.iter().any(|record| {
                record
                    .get("data")
                    .and_then(|d| d.as_str())
                    .map(|txt| txt.trim_matches('"').trim() == expected)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !found {
        return Err(ApiError::bad_request(format!(
            "TXT record not found. Create a TXT record at _intendant.{domain} with the exact value: {expected}"
        )));
    }
    Ok(Json(
        record_verified_attestation(
            &state,
            user.id,
            "dns",
            &domain,
            &format!("_intendant.{domain}"),
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct AttestGithubRequest {
    gist_raw_url: String,
}

/// Verify a public gist raw URL containing the claim string. The gist
/// owner (from the URL path) becomes the attested subject.
async fn attest_github(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AttestGithubRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "attest", 10, 600_000).await?;
    let raw_url = body.gist_raw_url.trim().to_string();
    let allowed_base = std::env::var("INTENDANT_CONNECT_GIST_BASE")
        .unwrap_or_else(|_| "https://gist.githubusercontent.com/".to_string());
    if !raw_url.starts_with(&allowed_base) {
        return Err(ApiError::bad_request(format!(
            "URL must be a raw gist URL starting with {allowed_base}"
        )));
    }
    let parsed = Url::parse(&raw_url).map_err(|_| ApiError::bad_request("invalid URL"))?;
    let gh_user = parsed
        .path_segments()
        .and_then(|mut segments| segments.next())
        .map(|owner| owner.to_ascii_lowercase())
        .filter(|owner| {
            !owner.is_empty() && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .ok_or_else(|| ApiError::bad_request("could not read the gist owner from the URL"))?;
    let expected = attestation_claim_string(&state.config, &user.account_name);
    let content = state
        .push_http
        .get(parsed.clone())
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("gist fetch failed: {e}")))?
        .text()
        .await
        .map_err(|e| ApiError::bad_request(format!("gist unreadable: {e}")))?;
    if content.len() > 65_536 || !content.contains(&expected) {
        return Err(ApiError::bad_request(format!(
            "the gist does not contain the exact claim line: {expected}"
        )));
    }
    let subject = format!("github:{gh_user}");
    Ok(Json(
        record_verified_attestation(&state, user.id, "github", &subject, &raw_url).await?,
    ))
}

/// Public directory: what this service will say about a handle. Zero
/// authority; all of it is re-checkable (attestation proofs are
/// external, log entries carry inclusion proofs).
async fn directory_lookup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "directory", 120, 60_000).await?;
    let handle = normalize_account_name(&handle);
    let store = state.store.lock().await;
    let Some(user) = store.users.iter().find(|u| u.account_name == handle) else {
        return Ok(orl_cors(
            Json(json!({ "ok": true, "found": false })).into_response(),
        ));
    };
    let attestations: Vec<serde_json::Value> = user
        .attestations
        .iter()
        .map(|a| {
            json!({
                "kind": a.kind,
                "subject": a.subject,
                "verified_unix_ms": a.verified_unix_ms,
                "proof": a.proof,
            })
        })
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "found": true,
            "handle": user.account_name,
            "display_name": user.display_name,
            "created_unix_ms": user.created_unix_ms,
            "attestations": attestations,
            "claimed_daemons": store
                .daemons
                .iter()
                .filter(|d| d.owner_user_id == Some(user.id))
                .count(),
        }))
        .into_response(),
    ))
}

fn log_leaves(store: &Store) -> Vec<[u8; 32]> {
    store
        .log_entries
        .iter()
        .map(|entry| log_leaf_hash(&entry.leaf_json))
        .collect()
}

fn signed_tree_head(state: &AppState, store: &Store) -> serde_json::Value {
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
        "ok": true,
        "size": leaves.len(),
        "root": root,
        "unix_ms": unix_ms,
        "signature": signature,
        "public_key": b64u(state.log_key.public_key().as_ref()),
    })
}

async fn log_sth(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "log_read", 240, 60_000).await?;
    let store = state.store.lock().await;
    Ok(orl_cors(
        Json(signed_tree_head(&state, &store)).into_response(),
    ))
}

#[derive(Debug, Deserialize)]
struct LogRangeQuery {
    #[serde(default)]
    start: usize,
    #[serde(default)]
    count: usize,
}

async fn log_entries(
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
struct LogProofQuery {
    index: usize,
    #[serde(default)]
    size: usize,
}

async fn log_proof(
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
struct LogConsistencyQuery {
    old: usize,
    #[serde(default)]
    new: usize,
}

async fn log_consistency(
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
struct LogFindQuery {
    #[serde(default)]
    daemon_id: String,
    #[serde(default)]
    handle: String,
}

/// Latest log entry binding a daemon_id or handle — the lookup a browser
/// does before trusting a first introduction.
async fn log_find(
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

async fn push_vapid_public_key(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use ring::signature::KeyPair as _;
    Json(json!({
        "ok": true,
        "public_key": b64u(state.vapid.public_key().as_ref()),
    }))
}

#[derive(Debug, Deserialize)]
struct PushSubscribeRequest {
    endpoint: String,
    #[serde(default)]
    p256dh: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    label: String,
}

async fn push_subscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PushSubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "push_subscribe", 20, 600_000).await?;
    let endpoint = body.endpoint.trim().to_string();
    if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
        return Err(ApiError::bad_request("endpoint must be a push service URL"));
    }
    if endpoint.len() > 2048 {
        return Err(ApiError::bad_request("endpoint is too long"));
    }
    let p256dh = body.p256dh.trim().to_string();
    let auth = body.auth.trim().to_string();
    match (b64u_decode(&p256dh), b64u_decode(&auth)) {
        (Ok(point), Ok(secret)) if point.len() == 65 && secret.len() == 16 => {}
        _ => return Err(ApiError::bad_request("subscription keys are malformed")),
    }
    {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| record.endpoint != endpoint);
        let per_user = store
            .push_subscriptions
            .iter()
            .filter(|record| record.user_id == user.id)
            .count();
        if per_user >= 10 {
            return Err(ApiError::bad_request(
                "too many subscriptions on this account",
            ));
        }
        store.push_subscriptions.push(PushSubscriptionRecord {
            user_id: user.id,
            endpoint,
            p256dh,
            auth,
            label: clean_fleet_text(&body.label, FLEET_LABEL_MAX),
            created_unix_ms: now_unix_ms(),
            notify_presence: true,
        });
        audit(
            &mut store,
            "push_subscribed",
            Some(user.id),
            None,
            json!({}),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct PushUnsubscribeRequest {
    #[serde(default)]
    endpoint: String,
}

async fn push_unsubscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PushUnsubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    let endpoint = body.endpoint.trim();
    let removed = {
        let mut store = state.store.lock().await;
        let before = store.push_subscriptions.len();
        store.push_subscriptions.retain(|record| {
            !(record.user_id == user.id && (endpoint.is_empty() || record.endpoint == endpoint))
        });
        let removed = before - store.push_subscriptions.len();
        if removed > 0 {
            persist_locked(&state, &store)?;
        }
        removed
    };
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

async fn push_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "push_test", 10, 600_000).await?;
    let subscriptions: Vec<PushSubscriptionRecord> = {
        let store = state.store.lock().await;
        store
            .push_subscriptions
            .iter()
            .filter(|record| record.user_id == user.id)
            .cloned()
            .collect()
    };
    if subscriptions.is_empty() {
        return Err(ApiError::bad_request(
            "no push subscriptions on this account",
        ));
    }
    let payload = json!({
        "title": "Intendant Connect",
        "body": "Test notification — this is what a computer alert will look like.",
        "url": "/connect",
    });
    let mut sent = 0;
    let mut dead = Vec::new();
    for subscription in &subscriptions {
        match send_web_push(
            &state.push_http,
            &state.vapid,
            &state.config.public_origin,
            subscription,
            &payload,
        )
        .await
        {
            Ok(true) => sent += 1,
            Ok(false) => dead.push(subscription.endpoint.clone()),
            Err(e) => eprintln!("[push] test send failed: {e}"),
        }
    }
    if !dead.is_empty() {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| !dead.contains(&record.endpoint));
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "sent": sent, "pruned": dead.len() }),
    ))
}

/// Watch claimed daemons for presence transitions and notify their
/// owners' opted-in browsers. The service only narrates facts it already
/// holds (last poll time); payloads are encrypted to each subscription.
async fn presence_alert_monitor(state: Arc<AppState>) {
    let offline_after_ms: u64 = std::env::var("INTENDANT_CONNECT_PRESENCE_OFFLINE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(180_000);
    let poll_ms: u64 = std::env::var("INTENDANT_CONNECT_PRESENCE_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    // daemon_id -> last announced state; seeded silently on startup so a
    // service restart never fires a wave of stale alerts.
    let mut announced: HashMap<String, bool> = HashMap::new();
    let mut seeded = false;
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        let now = now_unix_ms();
        let (transitions, subscriptions) = {
            let store = state.store.lock().await;
            let mut transitions: Vec<(String, String, Option<Uuid>, bool, u64)> = Vec::new();
            for daemon in store.daemons.iter().filter(|d| d.owner_user_id.is_some()) {
                let offline_for = now.saturating_sub(daemon.last_seen_unix_ms);
                let online = offline_for < offline_after_ms;
                let previous = announced.insert(daemon.daemon_id.clone(), online);
                if seeded {
                    if let Some(previous) = previous {
                        if previous != online {
                            let label = daemon
                                .label
                                .clone()
                                .unwrap_or_else(|| daemon.daemon_id.clone());
                            transitions.push((
                                daemon.daemon_id.clone(),
                                label,
                                daemon.owner_user_id,
                                online,
                                offline_for,
                            ));
                        }
                    }
                }
            }
            (transitions, store.push_subscriptions.clone())
        };
        seeded = true;
        if transitions.is_empty() {
            continue;
        }
        let mut dead = Vec::new();
        for (daemon_id, label, owner, online, offline_for) in transitions {
            let payload = json!({
                "title": if online { format!("{label} is back online") } else { format!("{label} went offline") },
                "body": if online {
                    format!("Reconnected after {} offline.", human_duration_ms(offline_for))
                } else {
                    "It stopped polling the rendezvous. The machine may be off, asleep, or disconnected.".to_string()
                },
                "url": format!("/app?connect=1&daemon_id={daemon_id}"),
            });
            for subscription in subscriptions
                .iter()
                .filter(|s| s.notify_presence && Some(s.user_id) == owner)
            {
                match send_web_push(
                    &state.push_http,
                    &state.vapid,
                    &state.config.public_origin,
                    subscription,
                    &payload,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => dead.push(subscription.endpoint.clone()),
                    Err(e) => eprintln!("[push] presence alert failed: {e}"),
                }
            }
        }
        if !dead.is_empty() {
            let mut store = state.store.lock().await;
            store
                .push_subscriptions
                .retain(|record| !dead.contains(&record.endpoint));
            if let Err(err) = persist_locked(&state, &store) {
                eprintln!("[push] failed to persist pruned subscriptions: {err:?}");
            }
        }
    }
}

/// Dormant-handle reclamation (stated policy; enforcement is opt-in via
/// INTENDANT_CONNECT_RECLAIM_AFTER_MS, 0/unset = off): an account with
/// zero claimed daemons and no sign-in past the threshold loses its
/// handle — the account survives, renamed to user-<id-prefix>, and the
/// reclamation is committed to the transparency log. Squatted-but-unused
/// names do not keep.
async fn handle_reclaim_monitor(state: Arc<AppState>) {
    let after_ms: u64 = std::env::var("INTENDANT_CONNECT_RECLAIM_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if after_ms == 0 {
        return;
    }
    let poll_ms: u64 = std::env::var("INTENDANT_CONNECT_RECLAIM_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6 * 3600 * 1000);
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms.max(60_000))).await;
        let now = now_unix_ms();
        let mut store = state.store.lock().await;
        let owners: std::collections::HashSet<Uuid> = store
            .daemons
            .iter()
            .filter_map(|d| d.owner_user_id)
            .collect();
        let mut reclaimed = Vec::new();
        for user in store.users.iter_mut() {
            if user.account_name.starts_with("user-") || owners.contains(&user.id) {
                continue;
            }
            let last_active = user
                .last_login_unix_ms
                .max(user.updated_unix_ms)
                .max(user.created_unix_ms);
            if now.saturating_sub(last_active) < after_ms {
                continue;
            }
            let freed = user.account_name.clone();
            let mut short = user.id.simple().to_string();
            short.truncate(8);
            user.account_name = format!("user-{short}");
            user.updated_unix_ms = now;
            reclaimed.push((freed, user.account_name.clone(), user.id));
        }
        if reclaimed.is_empty() {
            continue;
        }
        for (freed, renamed_to, user_id) in &reclaimed {
            append_log_entry(
                &mut store,
                "handle_reclaimed",
                json!({ "handle": freed, "renamed_to": renamed_to }),
            );
            audit(
                &mut store,
                "handle_reclaimed",
                Some(*user_id),
                None,
                json!({ "handle": freed }),
            );
            eprintln!("[reclaim] freed dormant handle {freed} (account renamed to {renamed_to})");
        }
        if let Err(err) = persist_locked(&state, &store) {
            eprintln!("[reclaim] failed to persist dormant-handle reclamation: {err:?}");
        }
    }
}

fn human_duration_ms(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes < 2 {
        return "moments".to_string();
    }
    if minutes < 120 {
        return format!("{minutes} minutes");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours} hours");
    }
    format!("{} days", hours / 24)
}

/// Admin surface: operator-only, authenticated by the daemon bearer
/// token. Unlike the daemon polling endpoints (which stay open when no
/// token is configured, for local dev), admin actions REQUIRE a
/// configured token — an unset token must not mean an open admin API.
fn require_admin_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if state.config.daemon_token.is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin endpoints require the service to be started with --daemon-token",
        ));
    }
    require_bearer_token(state, headers)
}

#[derive(Debug, Deserialize)]
struct InviteMintRequest {
    #[serde(default)]
    count: u32,
    #[serde(default)]
    label: String,
    #[serde(default)]
    max_uses: u32,
}

async fn admin_invites_mint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<InviteMintRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let count = body.count.clamp(1, 50);
    let max_uses = body.max_uses.clamp(1, 1000);
    let label = body.label.trim().to_string();
    let now = now_unix_ms();
    let mut codes = Vec::new();
    {
        let mut store = state.store.lock().await;
        for _ in 0..count {
            let code = random_b64u(12);
            store.invites.push(InviteRecord {
                code_hash: sha256_b64u(code.as_bytes()),
                label: label.clone(),
                created_unix_ms: now,
                max_uses,
                used_count: 0,
                revoked: false,
            });
            codes.push(code);
        }
        audit(
            &mut store,
            "invites_minted",
            None,
            None,
            json!({ "count": count, "label": label, "max_uses": max_uses }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "codes": codes, "max_uses": max_uses }),
    ))
}

async fn admin_invites_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let store = state.store.lock().await;
    let invites: Vec<_> = store
        .invites
        .iter()
        .map(|invite| {
            json!({
                "code_hash": invite.code_hash,
                "label": invite.label,
                "created_unix_ms": invite.created_unix_ms,
                "max_uses": invite.max_uses,
                "used_count": invite.used_count,
                "revoked": invite.revoked,
                "usable": invite_usable(invite),
            })
        })
        .collect();
    Ok(Json(
        json!({ "ok": true, "invite_required": state.config.invite_required, "invites": invites }),
    ))
}

#[derive(Debug, Deserialize)]
struct InviteRevokeRequest {
    #[serde(default)]
    code_hash: String,
    #[serde(default)]
    label: String,
}

async fn admin_invites_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<InviteRevokeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let code_hash = body.code_hash.trim();
    let label = body.label.trim();
    if code_hash.is_empty() && label.is_empty() {
        return Err(ApiError::bad_request("code_hash or label is required"));
    }
    let mut revoked = 0;
    {
        let mut store = state.store.lock().await;
        for invite in store.invites.iter_mut() {
            let matched = (!code_hash.is_empty() && invite.code_hash == code_hash)
                || (!label.is_empty() && invite.label == label);
            if matched && !invite.revoked {
                invite.revoked = true;
                revoked += 1;
            }
        }
        if revoked > 0 {
            audit(
                &mut store,
                "invites_revoked",
                None,
                None,
                json!({ "count": revoked }),
            );
            persist_locked(&state, &store)?;
        }
    }
    Ok(Json(json!({ "ok": true, "revoked": revoked })))
}

/// Bearer check against the configured operator token. Admin endpoints
/// verify through this directly (`require_admin_auth`) — never through
/// `require_daemon_auth` — so opening daemon registration can never open
/// the admin surface.
fn require_bearer_token(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(token) = state.config.daemon_token.as_deref() else {
        return Ok(());
    };
    let expected = format!("Bearer {token}");
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err(ApiError::unauthorized(
            "missing or invalid daemon bearer token",
        ))
    }
}

/// Gate for the daemon registration/polling endpoints. With
/// `--open-registration` these are anonymous by design: registration is
/// rate-limited, unclaimed records expire, and authorization moves to
/// claim time (a signed-in — on the hosted instance, invited — account).
/// Without it, the operator token (when configured) is required, which
/// suits self-hosters who want a closed fleet.
fn require_daemon_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if state.config.open_daemon_registration {
        return Ok(());
    }
    require_bearer_token(state, headers)
}

fn header_string(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn client_rate_key(headers: &HeaderMap, scope: &str) -> String {
    let peer = header_string(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next().map(str::trim).map(str::to_string))
        .filter(|v| !v.is_empty())
        .or_else(|| header_string(headers, "x-real-ip"))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{scope}:{peer}")
}

async fn check_rate_limit(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
    limit: u32,
    window_ms: u64,
) -> ApiResult<()> {
    let now = now_unix_ms();
    let key = client_rate_key(headers, scope);
    let mut buckets = state.rate_limits.lock().await;
    let bucket = buckets.entry(key).or_insert(RateLimitBucket {
        window_start_unix_ms: now,
        count: 0,
    });
    if now.saturating_sub(bucket.window_start_unix_ms) > window_ms {
        bucket.window_start_unix_ms = now;
        bucket.count = 0;
    }
    bucket.count = bucket.count.saturating_add(1);
    if bucket.count > limit {
        return Err(ApiError::too_many_requests("rate limit exceeded"));
    }
    Ok(())
}

fn require_same_origin(config: &ServiceConfig, headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = header_string(headers, "origin") else {
        return Ok(());
    };
    if trim_trailing_slash(&origin) == config.public_origin {
        Ok(())
    } else {
        Err(ApiError::forbidden("request origin is not allowed"))
    }
}

async fn require_csrf(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<()> {
    require_same_origin(&state.config, headers)?;
    let expected = header_string(headers, CSRF_HEADER)
        .ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let session_token = cookie_value(headers, COOKIE_NAME)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&session_token)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    if session.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::unauthorized("sign in required"));
    }
    if session.csrf_token == expected {
        Ok(())
    } else {
        Err(ApiError::forbidden("invalid CSRF token"))
    }
}

fn log_json(event: &str, detail: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "component": "intendant-connect",
            "event": event,
            "unix_ms": now_unix_ms(),
            "detail": detail,
        })
    );
}

async fn api_me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let Some(user) = optional_user(&state, &headers).await else {
        return Ok(Json(json!({
            "authenticated": false,
            "invite_required": state.config.invite_required,
        }))
        .into_response());
    };
    let csrf_token = if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state
            .sessions
            .lock()
            .await
            .get(&token)
            .map(|session| session.csrf_token.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Json(json!({
        "authenticated": true,
        "invite_required": state.config.invite_required,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response())
}

async fn api_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    require_csrf(&state, &headers).await?;
    if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state.sessions.lock().await.remove(&token);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie(&state.config));
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct RegisterStartRequest {
    account_name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    invite_code: String,
}

#[derive(Debug, Serialize)]
struct ChallengeStartResponse {
    ok: bool,
    flow_id: String,
    options: serde_json::Value,
}

async fn auth_register_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_start", 10, 600_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    validate_account_name(&account_name).map_err(ApiError::bad_request)?;
    // Adding a passkey to an EXISTING handle is a signed-in, same-account
    // action — otherwise anyone could attach their passkey to any handle.
    let session_user = optional_user(&state, &headers).await;
    let invite_code = body.invite_code.trim().to_string();
    let display_name = body.display_name.trim();
    let display_name = if display_name.is_empty() {
        account_name.clone()
    } else {
        display_name.to_string()
    };
    let (user_id, exclude_credentials, new_account, invite_code_hash) = {
        let store = state.store.lock().await;
        let existing = store.users.iter().find(|u| u.account_name == account_name);
        if let Some(existing) = existing {
            if session_user.as_ref().map(|u| u.id) != Some(existing.id) {
                return Err(ApiError::conflict(
                    "that handle is taken; to add a passkey to it, sign in to the account first",
                ));
            }
        }
        let new_account = existing.is_none();
        let invite_code_hash = if new_account && state.config.invite_required {
            let hash = sha256_b64u(invite_code.as_bytes());
            let usable = !invite_code.is_empty()
                && store
                    .invites
                    .iter()
                    .find(|invite| invite.code_hash == hash)
                    .map(invite_usable)
                    .unwrap_or(false);
            if !usable {
                return Err(ApiError::forbidden(
                    "registration is invite-only right now; ask an existing user or the operator for an invite code",
                ));
            }
            Some(hash)
        } else {
            None
        };
        let user_id = existing.map(|u| u.id).unwrap_or_else(Uuid::new_v4);
        let exclude = existing
            .map(|u| {
                u.passkeys
                    .iter()
                    .map(|pk| pk.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (user_id, exclude, new_account, invite_code_hash)
    };
    let (options, registration) = state.webauthn.start_registration(
        user_id.as_bytes(),
        &account_name,
        &display_name,
        &exclude_credentials,
    );
    let flow_id = Uuid::new_v4().to_string();
    let pending = PendingRegistration {
        user_id,
        account_name,
        display_name,
        new_account,
        invite_code_hash,
        state: registration,
        expires_unix_ms: now_unix_ms().saturating_add(300_000),
    };
    state
        .pending_registrations
        .lock()
        .await
        .insert(flow_id.clone(), pending);
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
struct RegisterFinishRequest {
    flow_id: String,
    credential: RegistrationResponse,
}

async fn auth_register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_finish", 30, 60_000).await?;
    let pending = state
        .pending_registrations
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("registration flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("registration flow expired"));
    }
    let passkey = state
        .webauthn
        .finish_registration(&pending.state, &body.credential)
        .map_err(|e| ApiError::bad_request(format!("finish passkey registration: {e}")))?;
    let user = {
        let mut store = state.store.lock().await;
        if store
            .users
            .iter()
            .flat_map(|u| u.passkeys.iter())
            .any(|pk| pk.id == passkey.id)
        {
            return Err(ApiError::conflict("passkey is already registered"));
        }
        if pending.new_account
            && store
                .users
                .iter()
                .any(|u| u.account_name == pending.account_name)
        {
            return Err(ApiError::conflict(
                "that handle was taken while you registered",
            ));
        }
        // Consume the invite now, inside the store lock, so a code's uses
        // can't be overspent by concurrent registrations.
        if pending.new_account && state.config.invite_required {
            let Some(hash) = pending.invite_code_hash.as_deref() else {
                return Err(ApiError::forbidden("registration is invite-only right now"));
            };
            let Some(invite) = store
                .invites
                .iter_mut()
                .find(|invite| invite.code_hash == hash)
            else {
                return Err(ApiError::forbidden("that invite code no longer exists"));
            };
            if !invite_usable(invite) {
                return Err(ApiError::forbidden(
                    "that invite code has been used up or revoked",
                ));
            }
            invite.used_count += 1;
        }
        let now = now_unix_ms();
        if let Some(user) = store.users.iter_mut().find(|u| u.id == pending.user_id) {
            user.display_name = pending.display_name.clone();
            user.passkeys.push(passkey);
            user.updated_unix_ms = now;
        } else {
            store.users.push(UserRecord {
                id: pending.user_id,
                account_name: pending.account_name.clone(),
                display_name: pending.display_name.clone(),
                passkeys: vec![passkey],
                created_unix_ms: now,
                updated_unix_ms: now,
                last_login_unix_ms: now,
                attestations: Vec::new(),
            });
            append_log_entry(
                &mut store,
                "account_created",
                json!({ "handle": pending.account_name }),
            );
        }
        audit(
            &mut store,
            "passkey_registered",
            Some(pending.user_id),
            None,
            json!({ "account_name": pending.account_name }),
        );
        persist_locked(&state, &store)?;
        store
            .users
            .iter()
            .find(|u| u.id == pending.user_id)
            .cloned()
            .ok_or_else(|| ApiError::internal("created user missing"))?
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct LoginStartRequest {
    account_name: String,
}

async fn auth_login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_start", 30, 60_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    let user = {
        let store = state.store.lock().await;
        store
            .users
            .iter()
            .find(|u| u.account_name == account_name)
            .cloned()
            .ok_or_else(|| ApiError::not_found("account not found"))?
    };
    if user.passkeys.is_empty() {
        return Err(ApiError::bad_request("account has no passkeys"));
    }
    let (options, authentication) = state
        .webauthn
        .start_authentication_with_creds_for_user(user.id.as_bytes(), &user.passkeys);
    let flow_id = Uuid::new_v4().to_string();
    state.pending_authentications.lock().await.insert(
        flow_id.clone(),
        PendingAuthentication {
            user_id: user.id,
            state: authentication,
            expires_unix_ms: now_unix_ms().saturating_add(300_000),
        },
    );
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
struct LoginFinishRequest {
    flow_id: String,
    credential: AuthenticationResponse,
}

async fn auth_login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_finish", 60, 60_000).await?;
    let pending = state
        .pending_authentications
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("login flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("login flow expired"));
    }
    let user = {
        let mut store = state.store.lock().await;
        let user = store
            .users
            .iter_mut()
            .find(|u| u.id == pending.user_id)
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        let asserted_id = CredentialId::from_b64url(&body.credential.id)
            .map_err(|e| ApiError::bad_request(format!("credential id: {e}")))?;
        let stored = user
            .passkeys
            .iter_mut()
            .find(|passkey| passkey.id == asserted_id)
            .ok_or_else(|| ApiError::bad_request("passkey did not match account"))?;
        let auth_result = state
            .webauthn
            .finish_authentication(&pending.state, &body.credential, stored)
            .map_err(|e| ApiError::bad_request(format!("finish passkey login: {e}")))?;
        stored.counter = auth_result.new_counter;
        user.updated_unix_ms = now_unix_ms();
        user.last_login_unix_ms = user.updated_unix_ms;
        let user = user.clone();
        audit(
            &mut store,
            "passkey_login",
            Some(user.id),
            None,
            json!({ "account_name": user.account_name }),
        );
        persist_locked(&state, &store)?;
        user
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}

async fn api_daemons(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let daemons = store
        .daemons
        .iter()
        .filter(|d| d.owner_user_id == Some(user.id))
        .map(daemon_view)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "daemons": daemons,
    })))
}

async fn api_fleet_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

#[derive(Debug, Deserialize)]
struct FleetTargetsSyncRequest {
    #[serde(default)]
    targets: Vec<FleetTargetInput>,
}

#[derive(Debug, Default, Deserialize)]
struct FleetTargetInput {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "hostId")]
    host_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    source: String,
    #[serde(default, alias = "accessDomain")]
    access_domain: String,
    #[serde(default, alias = "accessDomainLabel")]
    access_domain_label: String,
    #[serde(default)]
    route: String,
    #[serde(default)]
    route_key: String,
    #[serde(default, alias = "routeLabel")]
    route_label: String,
    #[serde(default)]
    auth: String,
    #[serde(default, alias = "authLabel")]
    auth_label: String,
    #[serde(default, alias = "effectiveRole")]
    effective_role: String,
    #[serde(default, alias = "effectiveRoleLabel")]
    effective_role_label: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ws_url: String,
    #[serde(default)]
    browser_tcp_via_url: String,
    #[serde(default, alias = "connectSignalingBase")]
    connect_signaling_base: String,
    #[serde(default, alias = "encFields")]
    enc_fields: String,
    #[serde(default)]
    origin: String,
    #[serde(default, alias = "connectDaemonId")]
    connect_daemon_id: String,
    #[serde(default)]
    capabilities: Vec<serde_json::Value>,
    #[serde(default, alias = "recordKey")]
    record_key: String,
    #[serde(default, alias = "recordSig")]
    record_sig: String,
    #[serde(default, alias = "recordSignedAtUnixMs")]
    record_signed_at_unix_ms: u64,
    #[serde(default, alias = "firstSeenUnixMs")]
    first_seen_unix_ms: u64,
    #[serde(default, alias = "lastSeenUnixMs")]
    last_seen_unix_ms: u64,
}

async fn api_fleet_targets_sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FleetTargetsSyncRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_targets_sync", 60, 60_000).await?;
    let now = now_unix_ms();
    let mut incoming = Vec::new();
    for input in body.targets.into_iter().take(FLEET_TARGET_LIMIT) {
        if let Some(target) = normalize_fleet_target_input(user.id, input, now) {
            incoming.push(target);
        }
    }
    let mut store = state.store.lock().await;
    let owned_daemon_ids = owned_daemon_ids(&store, user.id);
    let mut by_host: HashMap<String, FleetTargetRecord> = store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user.id)
        .map(|target| {
            let mut target = target.clone();
            canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
            (target.host_id.clone(), target)
        })
        .collect();
    for mut target in incoming {
        canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
        let previous = by_host.get(&target.host_id).cloned();
        let first_seen_unix_ms = previous
            .as_ref()
            .map(|record| record.first_seen_unix_ms)
            .filter(|value| *value > 0)
            .unwrap_or(target.first_seen_unix_ms);
        // Signature fields ride through verbatim (normalize bounded them):
        // the browser signs its records and re-verifies after the round
        // trip, so stripping them here would turn every synced row into
        // "unverified" and defeat the provenance badges.
        by_host.insert(
            target.host_id.clone(),
            FleetTargetRecord {
                first_seen_unix_ms,
                ..target
            },
        );
    }
    let mut user_targets = by_host.into_values().collect::<Vec<_>>();
    user_targets.sort_by(|a, b| {
        b.updated_unix_ms
            .cmp(&a.updated_unix_ms)
            .then_with(|| a.label.cmp(&b.label))
    });
    user_targets.truncate(FLEET_TARGET_LIMIT);
    store
        .fleet_targets
        .retain(|target| target.user_id != user.id);
    store.fleet_targets.extend(user_targets);
    persist_locked(&state, &store)?;
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

async fn api_fleet_target_forget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(target_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_target_forget", 60, 60_000).await?;
    let target_id = clean_fleet_text(&target_id, FLEET_TEXT_MAX);
    if target_id.is_empty() {
        return Err(ApiError::bad_request("target_id is required"));
    }
    let mut store = state.store.lock().await;
    let before = store.fleet_targets.len();
    store.fleet_targets.retain(|target| {
        !(target.user_id == user.id
            && (target.host_id == target_id
                || target.id == target_id
                || target.connect_daemon_id.as_deref() == Some(target_id.as_str())))
    });
    let removed = before.saturating_sub(store.fleet_targets.len());
    if removed > 0 {
        audit(
            &mut store,
            "fleet_target_forgotten",
            Some(user.id),
            Some(target_id.clone()),
            json!({ "removed": removed }),
        );
        persist_locked(&state, &store)?;
    }
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "removed": removed,
        "schema_version": 1,
        "targets": targets,
    })))
}

fn fleet_targets_for_user(
    config: &ServiceConfig,
    store: &Store,
    user_id: Uuid,
) -> Vec<serde_json::Value> {
    let owned_daemon_ids = owned_daemon_ids(store, user_id);
    let mut by_host: HashMap<String, serde_json::Value> = HashMap::new();
    for target in store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user_id)
    {
        let key = fleet_target_storage_key(target, &owned_daemon_ids);
        by_host.insert(key, fleet_target_view(target));
    }
    for daemon in store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
    {
        by_host.insert(
            daemon.daemon_id.clone(),
            daemon_fleet_target_view(config, daemon),
        );
    }
    let mut targets = by_host.into_values().collect::<Vec<_>>();
    targets.sort_by(|a, b| {
        let a_label = a.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let b_label = b.get("label").and_then(|v| v.as_str()).unwrap_or("");
        a_label.cmp(b_label)
    });
    targets
}

fn owned_daemon_ids(store: &Store, user_id: Uuid) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
        .map(|daemon| daemon.daemon_id.clone())
        .collect()
}

fn fleet_target_storage_key(
    target: &FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) -> String {
    target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
        .unwrap_or_else(|| target.host_id.clone())
}

fn canonicalize_fleet_target_for_owned_daemon(
    target: &mut FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) {
    let Some(connect_daemon_id) = target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
    else {
        return;
    };
    if target.id == connect_daemon_id && target.host_id == connect_daemon_id {
        return;
    }
    target.id = connect_daemon_id.clone();
    target.host_id = connect_daemon_id;
    // The owner signature covers host_id; rewriting it here makes that
    // signature permanently unverifiable, so drop it — the record honestly
    // reads as unsigned instead of carrying a signature that can never
    // match again.
    target.record_key = String::new();
    target.record_sig = String::new();
    target.record_signed_at_unix_ms = 0;
}

fn normalize_fleet_target_input(
    user_id: Uuid,
    input: FleetTargetInput,
    now: u64,
) -> Option<FleetTargetRecord> {
    let host_id = clean_fleet_text(
        first_non_empty(&[input.host_id.as_str(), input.id.as_str()]),
        FLEET_TEXT_MAX,
    );
    if host_id.is_empty() {
        return None;
    }
    let id = clean_fleet_text(
        first_non_empty(&[input.id.as_str(), host_id.as_str()]),
        FLEET_TEXT_MAX,
    );
    let label = clean_fleet_text(&input.label, FLEET_LABEL_MAX);
    let source = clean_fleet_token(
        first_non_empty(&[input.source.as_str(), "browser_fleet"]),
        FLEET_TEXT_MAX,
    );
    let route = clean_fleet_token(
        first_non_empty(&[input.route.as_str(), input.route_key.as_str()]),
        FLEET_TEXT_MAX,
    );
    let connect_daemon_id = clean_fleet_text(&input.connect_daemon_id, FLEET_TEXT_MAX);
    let first_seen_unix_ms = nonzero_past_or_now(input.first_seen_unix_ms, now);
    let last_seen_unix_ms = nonzero_past_or_now(input.last_seen_unix_ms, now);
    Some(FleetTargetRecord {
        user_id,
        id: if id.is_empty() { host_id.clone() } else { id },
        host_id: host_id.clone(),
        label: if label.is_empty() {
            host_id.clone()
        } else {
            label
        },
        local: input.local,
        source: if source.is_empty() {
            "browser_fleet".to_string()
        } else {
            source
        },
        access_domain: clean_fleet_token(&input.access_domain, FLEET_TEXT_MAX),
        access_domain_label: clean_fleet_text(&input.access_domain_label, FLEET_LABEL_MAX),
        route,
        route_label: clean_fleet_text(&input.route_label, FLEET_LABEL_MAX),
        auth: clean_fleet_token(&input.auth, FLEET_TEXT_MAX),
        auth_label: clean_fleet_text(&input.auth_label, FLEET_LABEL_MAX),
        effective_role: clean_fleet_token(&input.effective_role, FLEET_TEXT_MAX),
        effective_role_label: clean_fleet_text(&input.effective_role_label, FLEET_LABEL_MAX),
        profile: clean_fleet_token(&input.profile, FLEET_TEXT_MAX),
        url: clean_fleet_url(&input.url),
        ws_url: clean_fleet_url(&input.ws_url),
        browser_tcp_via_url: clean_fleet_url(&input.browser_tcp_via_url),
        connect_signaling_base: clean_fleet_url(&input.connect_signaling_base),
        enc_fields: clean_fleet_text(&input.enc_fields, FLEET_ENC_MAX),
        origin: clean_fleet_url(&input.origin),
        connect_daemon_id: if connect_daemon_id.is_empty() {
            None
        } else {
            Some(connect_daemon_id)
        },
        capabilities: clean_fleet_capabilities(input.capabilities),
        record_key: clean_fleet_text(&input.record_key, FLEET_SIG_MAX),
        record_sig: clean_fleet_text(&input.record_sig, FLEET_SIG_MAX),
        record_signed_at_unix_ms: if input.record_signed_at_unix_ms > now {
            now
        } else {
            input.record_signed_at_unix_ms
        },
        first_seen_unix_ms,
        last_seen_unix_ms,
        updated_unix_ms: now,
    })
}

fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
}

fn clean_fleet_text(value: &str, max_chars: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect::<String>()
}

fn clean_fleet_token(value: &str, max_chars: usize) -> String {
    clean_fleet_text(value, max_chars)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        .collect()
}

fn clean_fleet_url(value: &str) -> String {
    let value = clean_fleet_text(value, FLEET_URL_MAX);
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with('/') && !value.starts_with("//") {
        return value;
    }
    let Ok(url) = Url::parse(&value) else {
        return String::new();
    };
    match url.scheme() {
        "http" | "https" | "ws" | "wss" => value,
        _ => String::new(),
    }
}

fn clean_fleet_capabilities(values: Vec<serde_json::Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values.into_iter().take(FLEET_CAPABILITY_LIMIT * 2) {
        let Some(text) = value.as_str() else {
            continue;
        };
        let capability = clean_fleet_token(text, FLEET_TEXT_MAX);
        if capability.is_empty() || !seen.insert(capability.clone()) {
            continue;
        }
        out.push(capability);
        if out.len() >= FLEET_CAPABILITY_LIMIT {
            break;
        }
    }
    out
}

fn nonzero_past_or_now(value: u64, now: u64) -> u64 {
    if value == 0 || value > now {
        now
    } else {
        value
    }
}

async fn api_daemon_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_revoke", 30, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    ensure_owned_daemon(&state, user.id, &daemon_id).await?;
    let active_session_ids = active_dashboard_session_ids(&state, &daemon_id).await;
    let closed_sessions = active_session_ids.len();
    let mut store = state.store.lock().await;
    let daemon_index = store
        .daemons
        .iter()
        .position(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if store.daemons[daemon_index].owner_user_id != Some(user.id) {
        return Err(ApiError::forbidden("daemon belongs to a different account"));
    }
    let daemon = &mut store.daemons[daemon_index];
    daemon.owner_user_id = None;
    daemon.claim_code_hash = None;
    daemon.claim_code_created_unix_ms = None;
    daemon.updated_unix_ms = now_unix_ms();
    store.fleet_targets.retain(|target| {
        !(target.user_id == user.id
            && (target.host_id == daemon_id
                || target.id == daemon_id
                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
    });
    audit(
        &mut store,
        "daemon_revoked",
        Some(user.id),
        Some(daemon_id.clone()),
        json!({ "closed_sessions": closed_sessions }),
    );
    persist_locked(&state, &store)?;
    state.claim_codes.lock().await.remove(&daemon_id);
    drop(store);
    close_active_dashboard_sessions(&state, &daemon_id, active_session_ids).await;
    log_json(
        "daemon_revoked",
        json!({ "daemon_id": daemon_id, "closed_sessions": closed_sessions }),
    );
    Ok(Json(
        json!({ "ok": true, "closed_sessions": closed_sessions }),
    ))
}

#[derive(Debug, Deserialize)]
struct DaemonLabelRequest {
    label: String,
}

async fn api_daemon_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
    Json(body): Json<DaemonLabelRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_label", 60, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    let label = body.label.trim();
    if label.len() > 80 {
        return Err(ApiError::bad_request(
            "label must be 80 characters or shorter",
        ));
    }
    let mut store = state.store.lock().await;
    let daemon_index = store
        .daemons
        .iter()
        .position(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if store.daemons[daemon_index].owner_user_id != Some(user.id) {
        return Err(ApiError::forbidden("daemon belongs to a different account"));
    }
    let daemon = &mut store.daemons[daemon_index];
    daemon.label = if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    };
    daemon.updated_unix_ms = now_unix_ms();
    let view = daemon_view(daemon);
    let target_label = if label.is_empty() {
        daemon_id.as_str()
    } else {
        label
    };
    let now = now_unix_ms();
    for target in store.fleet_targets.iter_mut().filter(|target| {
        target.user_id == user.id
            && (target.host_id == daemon_id
                || target.id == daemon_id
                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str()))
    }) {
        target.label = target_label.to_string();
        target.updated_unix_ms = now;
    }
    audit(
        &mut store,
        "daemon_label_updated",
        Some(user.id),
        Some(daemon_id.clone()),
        json!({ "label": label }),
    );
    persist_locked(&state, &store)?;
    Ok(Json(json!({ "ok": true, "daemon": view })))
}

#[derive(Debug, Deserialize)]
struct ClaimStartRequest {
    claim_code: String,
}

async fn api_claim_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimStartRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_start", 10, 60_000).await?;
    let code = normalize_claim_code(&body.claim_code);
    if code.is_empty() {
        return Err(ApiError::bad_request("claim_code is required"));
    }
    let code_hashes = claim_code_hash_candidates(&body.claim_code);
    let now = now_unix_ms();
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| {
                d.owner_user_id.is_none()
                    && d.claim_code_hash
                        .as_deref()
                        .is_some_and(|hash| code_hashes.iter().any(|candidate| candidate == hash))
                    && d.claim_code_created_unix_ms
                        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
            })
            .cloned()
            .ok_or_else(|| ApiError::not_found("claim code not found"))?
    };
    let claim_id = Uuid::new_v4().to_string();
    let challenge = random_b64u(32);
    state.pending_claims.lock().await.insert(
        claim_id.clone(),
        PendingClaim {
            user_id: user.id,
            daemon_id: daemon.daemon_id.clone(),
            challenge: challenge.clone(),
            created_unix_ms: now_unix_ms(),
            status: ClaimStatus::Pending,
        },
    );
    enqueue_event(
        &state,
        &daemon.daemon_id,
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "claim_challenge".to_string(),
            claim_id: Some(claim_id.clone()),
            challenge: Some(challenge),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_started",
            Some(user.id),
            Some(daemon.daemon_id.clone()),
            json!({ "claim_id": claim_id }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": daemon.daemon_id,
    })))
}

async fn api_claim_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(claim_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let mut claims = state.pending_claims.lock().await;
    let claim = claims
        .get_mut(claim_id.trim())
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if claim.user_id != user.id {
        return Err(ApiError::forbidden("claim belongs to a different account"));
    }
    if matches!(claim.status, ClaimStatus::Pending)
        && now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS
    {
        claim.status = ClaimStatus::Rejected {
            error: "claim timed out".to_string(),
        };
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": claim.daemon_id,
        "result": claim.status,
    })))
}

async fn api_audit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let events = store
        .audit
        .iter()
        .filter(|event| event.user_id == Some(user.id))
        .rev()
        .take(100)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "events": events,
    })))
}

#[derive(Debug, Deserialize)]
struct StatusQuery {
    #[serde(default)]
    daemon_id: String,
}

async fn api_status(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatusQuery>,
) -> Json<serde_json::Value> {
    let daemon_id = query.daemon_id.trim();
    let (daemon, queued, active_sessions) = {
        let store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned();
        let queued = state
            .event_queues
            .lock()
            .await
            .get(daemon_id)
            .map(|q| q.len())
            .unwrap_or(0);
        let active_sessions = state
            .active_sessions
            .lock()
            .await
            .values()
            .filter(|session| session.daemon_id == daemon_id)
            .count();
        (daemon, queued, active_sessions)
    };
    let now = now_unix_ms();
    let claim_code_expires_unix_ms = daemon
        .as_ref()
        .and_then(|d| d.claim_code_created_unix_ms)
        .map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        .filter(|expires| *expires > now);
    Json(json!({
        "ok": true,
        "daemon_id": daemon_id,
        "registered": daemon.is_some(),
        "claimed": daemon.as_ref().and_then(|d| d.owner_user_id).is_some(),
        "label": daemon.as_ref().and_then(|d| d.label.as_deref()).unwrap_or(""),
        "daemon_public_key": daemon.as_ref().map(|d| d.daemon_public_key.as_str()).unwrap_or(""),
        "last_seen_unix_ms": daemon.as_ref().map(|d| d.last_seen_unix_ms).unwrap_or(0),
        "claim_code_expires_unix_ms": claim_code_expires_unix_ms,
        "queued": queued,
        "active_sessions": active_sessions,
        "daemon_auth_required": state.config.daemon_token.is_some()
            && !state.config.open_daemon_registration,
    }))
}

#[derive(Debug, Deserialize)]
struct DaemonRegisterRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
}

async fn daemon_register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonRegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_register", 120, 60_000).await?;
    if body.protocol != PROTOCOL {
        return Err(ApiError::bad_request("unsupported protocol"));
    }
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon_public_key = body.daemon_public_key.trim().to_string();
    if daemon_id.is_empty() || daemon_public_key.is_empty() {
        return Err(ApiError::bad_request(
            "daemon_id and daemon_public_key are required",
        ));
    }
    let mut claim_code = None;
    let claimed = {
        let mut claim_codes = state.claim_codes.lock().await;
        let mut store = state.store.lock().await;
        let now = now_unix_ms();
        for stale_id in sweep_stale_unclaimed_daemons(&mut store, now) {
            claim_codes.remove(&stale_id);
        }
        let active_claim_hashes = active_claim_code_hashes(&store, &daemon_id, now);
        let claimed_now = if let Some(existing) =
            store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id)
        {
            if existing.owner_user_id.is_some() && existing.daemon_public_key != daemon_public_key {
                return Err(ApiError::conflict(
                    "claimed daemon_id is already bound to a different daemon key",
                ));
            }
            existing.daemon_public_key = daemon_public_key.clone();
            existing.last_seen_unix_ms = now;
            record_presence_hour(&mut existing.presence_hours, now);
            existing.updated_unix_ms = now;
            if existing.owner_user_id.is_none() {
                claim_code = Some(ensure_claim_code(
                    &mut claim_codes,
                    existing,
                    &active_claim_hashes,
                )?);
            }
            existing.owner_user_id.is_some()
        } else {
            let mut record = DaemonRecord {
                daemon_id: daemon_id.clone(),
                label: None,
                daemon_public_key: daemon_public_key.clone(),
                owner_user_id: None,
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                registered_unix_ms: now,
                last_seen_unix_ms: now,
                updated_unix_ms: now,
                presence_hours: Vec::new(),
            };
            claim_code = Some(ensure_claim_code(
                &mut claim_codes,
                &mut record,
                &active_claim_hashes,
            )?);
            store.daemons.push(record);
            false
        };
        persist_locked(&state, &store)?;
        claimed_now
    };
    let claim_url = claim_code
        .as_ref()
        .map(|code| format!("{}/connect?claim_code={code}", state.config.public_origin));
    if let Some(url) = claim_url.as_deref() {
        log_json(
            "daemon_awaiting_claim",
            json!({ "daemon_id": daemon_id, "claim_url": url }),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "claimed": claimed,
        "claim_code": claim_code,
        "claim_url": claim_url,
        "daemon_public_key": daemon_public_key,
    })))
}

fn ensure_claim_code(
    claim_codes: &mut HashMap<String, String>,
    daemon: &mut DaemonRecord,
    active_claim_hashes: &HashSet<String>,
) -> ApiResult<String> {
    let now = now_unix_ms();
    let existing_is_fresh = daemon
        .claim_code_created_unix_ms
        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS);
    let existing_hash_is_unique = daemon
        .claim_code_hash
        .as_deref()
        .is_some_and(|hash| !active_claim_hashes.contains(hash));
    if existing_is_fresh && existing_hash_is_unique {
        if let Some(code) = claim_codes.get(&daemon.daemon_id).cloned() {
            return Ok(code);
        }
    }
    if !existing_is_fresh {
        claim_codes.remove(&daemon.daemon_id);
    }
    for _ in 0..CLAIM_CODE_GENERATION_ATTEMPTS {
        let code = generate_claim_code()?;
        let code_hash = claim_code_hash(&code);
        if active_claim_hashes.contains(&code_hash) {
            continue;
        }
        daemon.claim_code_hash = Some(code_hash);
        daemon.claim_code_created_unix_ms = Some(now);
        claim_codes.insert(daemon.daemon_id.clone(), code.clone());
        return Ok(code);
    }
    Err(ApiError::internal("failed to generate a unique claim code"))
}

fn generate_claim_code() -> ApiResult<String> {
    let mut entropy = [0u8; CLAIM_CODE_ENTROPY_BYTES];
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy)
        .map_err(|e| ApiError::internal(format!("generate claim mnemonic: {e}")))?;
    Ok(mnemonic.to_string().replace(' ', "-"))
}

/// A day without polling: unclaimed records past this vanish on the next
/// registration sweep, so open registration cannot grow the store without
/// bound. Claimed daemons are never touched here — a returning unclaimed
/// daemon simply re-registers and gets a fresh claim code.
const UNCLAIMED_DAEMON_TTL_MS: u64 = 24 * 60 * 60 * 1000;

fn sweep_stale_unclaimed_daemons(store: &mut Store, now: u64) -> Vec<String> {
    let mut removed = Vec::new();
    store.daemons.retain(|daemon| {
        let keep = daemon.owner_user_id.is_some()
            || now.saturating_sub(daemon.last_seen_unix_ms) < UNCLAIMED_DAEMON_TTL_MS;
        if !keep {
            removed.push(daemon.daemon_id.clone());
        }
        keep
    });
    removed
}

fn active_claim_code_hashes(store: &Store, except_daemon_id: &str, now: u64) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.daemon_id != except_daemon_id)
        .filter(|daemon| daemon.owner_user_id.is_none())
        .filter(|daemon| {
            daemon
                .claim_code_created_unix_ms
                .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
        })
        .filter_map(|daemon| daemon.claim_code_hash.clone())
        .collect()
}

fn claim_code_hash(code: &str) -> String {
    sha256_b64u(normalize_claim_code(code).as_bytes())
}

fn claim_code_hash_candidates(input: &str) -> Vec<String> {
    let mut hashes = Vec::with_capacity(2);
    let normalized = normalize_claim_code(input);
    if !normalized.is_empty() {
        hashes.push(sha256_b64u(normalized.as_bytes()));
    }
    let legacy = input.trim().replace(' ', "").to_ascii_uppercase();
    if !legacy.is_empty() && legacy != normalized {
        let hash = sha256_b64u(legacy.as_bytes());
        if !hashes.iter().any(|existing| existing == &hash) {
            hashes.push(hash);
        }
    }
    hashes
}

fn normalize_claim_code(input: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts.join("-")
}

#[derive(Debug, Deserialize)]
struct DaemonNextQuery {
    daemon_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn daemon_next(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DaemonNextQuery>,
) -> ApiResult<Response> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_next", 240, 60_000).await?;
    let daemon_id = query.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    touch_daemon(&state, &daemon_id).await?;
    let timeout = Duration::from_millis(query.timeout_ms.unwrap_or(15_000).min(30_000));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(event) = pop_event(&state, &daemon_id).await {
            return Ok(Json(event).into_response());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let remaining = deadline.saturating_duration_since(now);
        if tokio::time::timeout(remaining, state.event_notify.notified())
            .await
            .is_err()
        {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

async fn touch_daemon(state: &AppState, daemon_id: &str) -> ApiResult<()> {
    let mut store = state.store.lock().await;
    if let Some(daemon) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) {
        let now = now_unix_ms();
        daemon.last_seen_unix_ms = now;
        daemon.updated_unix_ms = now;
        record_presence_hour(&mut daemon.presence_hours, now);
        persist_locked(state, &store)?;
        Ok(())
    } else {
        Err(ApiError::not_found("daemon is not registered"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_name: Option<String>,
    // Browser identity-key fields are relayed verbatim; the daemon verifies
    // the signature end-to-end, so this service never gains authority by
    // carrying them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_sig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_ts: Option<i64>,
    // Signed org-grant document, also relayed verbatim: the daemon verifies
    // it against the org keys it locally trusts, so this service can
    // neither mint nor amplify one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_grant: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
}

async fn enqueue_event(state: &AppState, daemon_id: &str, event: RendezvousEvent) {
    let mut queues = state.event_queues.lock().await;
    queues
        .entry(daemon_id.to_string())
        .or_default()
        .push_back(event);
    drop(queues);
    state.event_notify.notify_waiters();
}

async fn pop_event(state: &AppState, daemon_id: &str) -> Option<RendezvousEvent> {
    let mut queues = state.event_queues.lock().await;
    let queue = queues.get_mut(daemon_id)?;
    let event = queue.pop_front();
    if queue.is_empty() {
        queues.remove(daemon_id);
    }
    event
}

async fn record_active_dashboard_session(state: &AppState, daemon_id: &str, session_id: &str) {
    let now = now_unix_ms();
    let mut sessions = state.active_sessions.lock().await;
    sessions.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    sessions.insert(
        session_id.to_string(),
        ActiveDashboardSession {
            daemon_id: daemon_id.to_string(),
            session_id: session_id.to_string(),
            created_unix_ms: now,
        },
    );
}

async fn active_dashboard_session_ids(state: &AppState, daemon_id: &str) -> Vec<String> {
    let now = now_unix_ms();
    let mut active = state.active_sessions.lock().await;
    active.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    active
        .values()
        .filter(|session| session.daemon_id == daemon_id)
        .map(|session| session.session_id.clone())
        .collect()
}

async fn close_active_dashboard_sessions(
    state: &AppState,
    daemon_id: &str,
    session_ids: Vec<String>,
) -> usize {
    let sessions = {
        let mut active = state.active_sessions.lock().await;
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let belongs_to_daemon = active
                .get(&session_id)
                .is_some_and(|session| session.daemon_id == daemon_id);
            if belongs_to_daemon {
                active.remove(&session_id);
                sessions.push(session_id);
            }
        }
        sessions
    };
    let closed = sessions.len();
    for session_id in sessions {
        enqueue_event(
            state,
            daemon_id,
            RendezvousEvent {
                id: Uuid::new_v4().to_string(),
                kind: "close".to_string(),
                session_id: Some(session_id),
                ..RendezvousEvent::default()
            },
        )
        .await;
    }
    closed
}

#[derive(Debug, Deserialize)]
struct DaemonAnswerRequest {
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct BrowserAnswerResponse {
    ok: bool,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
    daemon_public_key: String,
    session_grant: String,
}

async fn daemon_answer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonAnswerRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
        .ok_or_else(|| ApiError::not_found("offer not found"))?;
    if pending.daemon_id != body.daemon_id {
        let _ = pending
            .response_tx
            .send(Err("daemon_id mismatch in answer".to_string()));
        return Err(ApiError::bad_request("daemon_id mismatch"));
    }
    let validation_error = validate_dashboard_binding(
        &body.binding,
        &pending.daemon_public_key,
        &pending.session_grant,
    );
    if let Err(error) = validation_error {
        let _ = pending.response_tx.send(Err(error.clone()));
        return Err(ApiError::bad_request(error));
    }
    let answer_session_id = body.session_id.trim().to_string();
    if answer_session_id.is_empty() {
        let _ = pending
            .response_tx
            .send(Err("daemon answer missing session_id".to_string()));
        return Err(ApiError::bad_request("daemon answer missing session_id"));
    }
    record_active_dashboard_session(&state, &pending.daemon_id, &answer_session_id).await;
    let answer = BrowserAnswerResponse {
        ok: true,
        session_id: answer_session_id.clone(),
        sdp: body.sdp,
        binding: body.binding,
        daemon_public_key: pending.daemon_public_key,
        session_grant: pending.session_grant,
    };
    let _ = pending.response_tx.send(Ok(answer));
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_answered",
            Some(pending.user_id),
            Some(pending.daemon_id),
            json!({ "request_id": body.request_id, "session_id": answer_session_id }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

fn validate_dashboard_binding(
    binding: &serde_json::Value,
    daemon_public_key: &str,
    session_grant: &str,
) -> Result<(), String> {
    let binding_key = binding
        .get("daemon_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if binding_key != daemon_public_key {
        return Err("binding daemon_public_key mismatch".to_string());
    }
    let grant_hash = binding
        .get("session_grant_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expected = sha256_b64u(session_grant.as_bytes());
    if grant_hash != expected {
        return Err("binding session_grant_sha256 mismatch".to_string());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DaemonErrorRequest {
    daemon_id: String,
    request_id: String,
    error: String,
}

async fn daemon_error(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonErrorRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    if let Some(pending) = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
    {
        if pending.daemon_id == body.daemon_id {
            let _ = pending.response_tx.send(Err(body.error));
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct ClaimProofRequest {
    daemon_id: String,
    request_id: String,
    claim_id: String,
    challenge: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
struct DaemonDryRequest {
    daemon_id: String,
    #[serde(default)]
    credentials: Vec<serde_json::Value>,
}

/// A claimed daemon's credential leases expired with nothing covering
/// them (credential custody). Web-Push the owner's subscribed browsers so
/// they can reconnect a fueling session — the service only relays the
/// daemon's own report; it can't see leases.
fn dry_push_payload(
    daemon_id: &str,
    label: &str,
    credentials: &[serde_json::Value],
) -> serde_json::Value {
    let mut names: Vec<String> = credentials
        .iter()
        .filter_map(|credential| {
            credential
                .get("label")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| credential.get("kind").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .take(6)
        .collect();
    if names.is_empty() {
        names.push("credentials".to_string());
    }
    json!({
        "title": format!("{label} is unfueled"),
        "body": format!(
            "Credential lease expired: {}. Reconnect a fueling session to re-grant from the vault.",
            names.join(", ")
        ),
        "url": format!("/app?connect=1&daemon_id={daemon_id}"),
    })
}

async fn daemon_dry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonDryRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_dry", 30, 60_000).await?;
    let daemon_id = body.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    let (label, owner, subscriptions) = {
        let store = state.store.lock().await;
        let Some(daemon) = store.daemons.iter().find(|d| d.daemon_id == daemon_id) else {
            return Err(ApiError::not_found("unknown daemon"));
        };
        (
            daemon.label.clone().unwrap_or_else(|| daemon_id.clone()),
            daemon.owner_user_id,
            store.push_subscriptions.clone(),
        )
    };
    let Some(owner) = owner else {
        // Nobody has claimed this daemon — nobody to notify.
        return Ok(Json(json!({ "ok": true, "notified": 0 })));
    };
    let payload = dry_push_payload(&daemon_id, &label, &body.credentials);
    let mut notified = 0usize;
    let mut dead = Vec::new();
    for subscription in subscriptions
        .iter()
        .filter(|s| s.notify_presence && s.user_id == owner)
    {
        match send_web_push(
            &state.push_http,
            &state.vapid,
            &state.config.public_origin,
            subscription,
            &payload,
        )
        .await
        {
            Ok(true) => notified += 1,
            Ok(false) => dead.push(subscription.endpoint.clone()),
            Err(e) => eprintln!("[push] dry-daemon alert failed: {e}"),
        }
    }
    if !dead.is_empty() {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| !dead.contains(&record.endpoint));
        let _ = persist_locked(&state, &store);
    }
    Ok(Json(json!({ "ok": true, "notified": notified })))
}

async fn daemon_claim_proof(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimProofRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_claims
        .lock()
        .await
        .get(body.claim_id.trim())
        .cloned()
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if pending.daemon_id != body.daemon_id || pending.challenge != body.challenge {
        reject_claim(&state, &body.claim_id, "claim proof mismatch").await;
        return Err(ApiError::bad_request("claim proof mismatch"));
    }
    if !matches!(pending.status, ClaimStatus::Pending) {
        return Err(ApiError::bad_request("claim is already resolved"));
    }
    if now_unix_ms().saturating_sub(pending.created_unix_ms) > CLAIM_TIMEOUT_MS {
        reject_claim(&state, &body.claim_id, "claim timed out").await;
        return Err(ApiError::bad_request("claim timed out"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == body.daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let payload = claim_signing_payload(
        &body.claim_id,
        &body.daemon_id,
        &daemon.daemon_public_key,
        &body.challenge,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        reject_claim(&state, &body.claim_id, "claim signature invalid").await;
        return Err(ApiError::bad_request("claim signature invalid"));
    }
    {
        let mut store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter_mut()
            .find(|d| d.daemon_id == body.daemon_id)
            .ok_or_else(|| ApiError::not_found("daemon not found"))?;
        daemon.owner_user_id = Some(pending.user_id);
        daemon.claim_code_hash = None;
        daemon.claim_code_created_unix_ms = None;
        daemon.updated_unix_ms = now_unix_ms();
        let log_event = json!({
            "daemon_id": daemon.daemon_id,
            "daemon_public_key": daemon.daemon_public_key,
            "handle": store
                .users
                .iter()
                .find(|u| u.id == pending.user_id)
                .map(|u| u.account_name.clone())
                .unwrap_or_default(),
        });
        append_log_entry(&mut store, "daemon_claimed", log_event);
        audit(
            &mut store,
            "daemon_claimed",
            Some(pending.user_id),
            Some(body.daemon_id.clone()),
            json!({ "claim_id": body.claim_id, "request_id": body.request_id }),
        );
        persist_locked(&state, &store)?;
    }
    state.claim_codes.lock().await.remove(&body.daemon_id);
    {
        let mut claims = state.pending_claims.lock().await;
        if let Some(claim) = claims.get_mut(body.claim_id.trim()) {
            claim.status = ClaimStatus::Approved {
                daemon_id: body.daemon_id.clone(),
            };
        }
    }
    Ok(Json(json!({ "ok": true })))
}

async fn reject_claim(state: &AppState, claim_id: &str, error: &str) {
    let mut claims = state.pending_claims.lock().await;
    if let Some(claim) = claims.get_mut(claim_id.trim()) {
        claim.status = ClaimStatus::Rejected {
            error: error.to_string(),
        };
    }
}

fn claim_signing_payload(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
) -> String {
    format!("{CLAIM_PROTOCOL}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n")
}

fn verify_ed25519_b64u(public_key_b64u: &str, payload: &[u8], signature_b64u: &str) -> bool {
    let Ok(public_key) = b64u_decode(public_key_b64u) else {
        return false;
    };
    let Ok(signature) = b64u_decode(signature_b64u) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(payload, &signature)
        .is_ok()
}

#[derive(Debug, Deserialize)]
struct BrowserOfferRequest {
    daemon_id: String,
    sdp: String,
    #[serde(default)]
    client_nonce: Option<String>,
    #[serde(default)]
    client_key: Option<String>,
    #[serde(default)]
    client_key_sig: Option<String>,
    #[serde(default)]
    client_key_ts: Option<i64>,
    #[serde(default)]
    org_grant: Option<serde_json::Value>,
}

/// The exact byte string an org root signs over its revocation list —
/// mirrors `access::org::orl_signing_payload` in the daemon. Stable
/// protocol, replicated rather than shared: this binary interprets the
/// list only enough to keep the bulletin board clean.
fn orl_signing_payload(list: &serde_json::Value) -> Option<Vec<u8>> {
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
fn orl_cors(response: Response) -> Response {
    let mut response = response;
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    response
}

async fn orl_preflight() -> Response {
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

const MAX_ORL_BULLETIN_BYTES: usize = 64 * 1024;

async fn orl_publish(
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
struct OrlFetchQuery {
    #[serde(default)]
    handle: String,
    #[serde(default)]
    root_key: String,
}

async fn orl_fetch(
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

/* ── Credential vault sync (credential custody) ──
One end-to-end encrypted vault blob per account. The service stores it
blind: the body is ciphertext under the user's vault master key, and
that key travels only wrapped per enrolled unlocker (passkey PRF /
recovery phrase) — nothing here can be decrypted or forged
server-side. Blobs additionally carry a client-side HMAC keyed to the
master key (`mac`); this service cannot verify it (by design), but it
enforces the presence ratchet: once an account's stored vault carries
a MAC, a MAC-less replacement is refused so a tampering store cannot
quietly strip the integrity guarantee. The monotonic revision check
only prevents rollback (the ORL `seq` trick); a malicious store can
still withhold or serve stale, detectably once any device has seen a
newer revision. */

const MAX_VAULT_BLOB_BYTES: usize = 128 * 1024;

fn validate_vault_blob(revision: u64, vault: &serde_json::Value) -> Result<(), ApiError> {
    if serde_json::to_string(vault)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_VAULT_BLOB_BYTES
    {
        return Err(ApiError::bad_request("vault blob is too large"));
    }
    if vault.get("v").and_then(|v| v.as_u64()) != Some(1)
        || vault.get("kind").and_then(|v| v.as_str()) != Some("intendant-vault")
    {
        return Err(ApiError::bad_request("not an intendant vault blob"));
    }
    if revision == 0 {
        return Err(ApiError::bad_request("vault revision must be positive"));
    }
    if vault.get("revision").and_then(|v| v.as_u64()) != Some(revision) {
        return Err(ApiError::bad_request("vault revision does not match blob"));
    }
    let has_envelopes = vault
        .get("envelopes")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_envelopes {
        return Err(ApiError::bad_request("vault blob has no key envelopes"));
    }
    if !vault.get("body").map(|b| b.is_object()).unwrap_or(false) {
        return Err(ApiError::bad_request("vault blob has no body"));
    }
    if let Some(mac) = vault.get("mac") {
        // Blind shape check only — an HMAC-SHA-256 in base64url is 43
        // chars; the service cannot (and must not be able to) verify it.
        let plausible = mac
            .as_str()
            .map(|s| !s.is_empty() && s.len() <= 88)
            .unwrap_or(false);
        if !plausible {
            return Err(ApiError::bad_request("vault mac is malformed"));
        }
    }
    Ok(())
}

/// Store a user's vault blob if it is newer than what we hold. Returns
/// `true` when stored, `false` for an idempotent same-revision republish
/// of identical content. Rollback — and a same-revision write with
/// different content (two devices bumped independently) — is rejected
/// with 409 so the losing client refetches, merges, and bumps.
fn apply_vault_publish(
    store: &mut Store,
    user_id: Uuid,
    revision: u64,
    vault: serde_json::Value,
    now: u64,
) -> Result<bool, ApiError> {
    validate_vault_blob(revision, &vault)?;
    if let Some(existing) = store.vault_blobs.iter_mut().find(|b| b.user_id == user_id) {
        // Downgrade ratchet: this service is blind to the MAC's validity
        // but not to its presence — once the stored vault is
        // authenticated, a MAC-less replacement is refused rather than
        // silently stripping the integrity guarantee clients rely on.
        if existing.vault.get("mac").is_some() && vault.get("mac").is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "unauthenticated vault refused: the stored vault carries an integrity MAC \
                 (update this dashboard to one that signs vault blobs)"
                    .to_string(),
            ));
        }
        if revision < existing.revision
            || (revision == existing.revision && existing.vault != vault)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!(
                    "stale vault: revision {revision} conflicts with stored revision {}",
                    existing.revision
                ),
            ));
        }
        let changed = revision > existing.revision;
        if changed {
            existing.revision = revision;
            existing.vault = vault;
            existing.updated_unix_ms = now;
        }
        Ok(changed)
    } else {
        store.vault_blobs.push(VaultBlobRecord {
            user_id,
            revision,
            vault,
            updated_unix_ms: now,
        });
        Ok(true)
    }
}

async fn api_vault_fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    check_rate_limit(&state, &headers, "vault_fetch", 240, 60_000).await?;
    let store = state.store.lock().await;
    match store.vault_blobs.iter().find(|b| b.user_id == user.id) {
        Some(record) => Ok(Json(json!({
            "ok": true,
            "revision": record.revision,
            "updated_unix_ms": record.updated_unix_ms,
            "vault": record.vault,
        }))),
        None => Ok(Json(
            json!({ "ok": true, "revision": 0, "vault": serde_json::Value::Null }),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct VaultPublishRequest {
    revision: u64,
    vault: serde_json::Value,
}

async fn api_vault_publish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<VaultPublishRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "vault_publish", 60, 60_000).await?;
    let mut store = state.store.lock().await;
    let stored = apply_vault_publish(
        &mut store,
        user.id,
        body.revision,
        body.vault,
        now_unix_ms(),
    )?;
    if stored {
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "stored": stored, "revision": body.revision }),
    ))
}

async fn browser_offer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserOfferRequest>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_offer", 60, 60_000).await?;
    let daemon_id = body.daemon_id.trim().to_string();
    let sdp = body.sdp;
    if daemon_id.is_empty() || sdp.trim().is_empty() {
        return Err(ApiError::bad_request("daemon_id and sdp are required"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id && d.owner_user_id == Some(user.id))
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let request_id = Uuid::new_v4().to_string();
    let session_grant = random_b64u(32);
    let (tx, rx) = oneshot::channel();
    state.pending_offers.lock().await.insert(
        request_id.clone(),
        PendingOffer {
            daemon_id: daemon_id.clone(),
            user_id: user.id,
            daemon_public_key: daemon.daemon_public_key.clone(),
            session_grant: session_grant.clone(),
            response_tx: tx,
        },
    );
    enqueue_event(
        &state,
        &daemon_id,
        RendezvousEvent {
            id: request_id.clone(),
            kind: "offer".to_string(),
            sdp: Some(sdp),
            session_grant: Some(session_grant),
            client_nonce: body
                .client_nonce
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            user_id: Some(user.id.to_string()),
            account_name: Some(user.account_name.clone()),
            client_key: body
                .client_key
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_sig: body
                .client_key_sig
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_ts: body.client_key_ts,
            // Opaque passthrough, size-capped so the relay cannot be used
            // to firehose daemons; the daemon re-verifies and rate-limits.
            org_grant: body.org_grant.filter(|doc| {
                !doc.is_null()
                    && serde_json::to_string(doc)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX)
                        <= MAX_ORG_GRANT_RELAY_BYTES
            }),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_started",
            Some(user.id),
            Some(daemon_id.clone()),
            json!({ "request_id": request_id }),
        );
        persist_locked(&state, &store)?;
    }
    match tokio::time::timeout(Duration::from_millis(OFFER_TIMEOUT_MS), rx).await {
        Ok(Ok(Ok(answer))) => Ok(Json(answer).into_response()),
        Ok(Ok(Err(error))) => Err(ApiError::new(StatusCode::BAD_GATEWAY, error)),
        Ok(Err(_)) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "daemon answer channel closed",
        )),
        Err(_) => {
            state.pending_offers.lock().await.remove(&request_id);
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timed out waiting for daemon answer",
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserIceRequest {
    daemon_id: String,
    session_id: String,
    #[serde(default)]
    candidate: serde_json::Value,
}

async fn browser_ice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserIceRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_ice", 600, 60_000).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "ice".to_string(),
            session_id: Some(body.session_id),
            candidate: Some(body.candidate),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct BrowserCloseRequest {
    daemon_id: String,
    session_id: String,
}

async fn browser_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserCloseRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    state
        .active_sessions
        .lock()
        .await
        .remove(body.session_id.trim());
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "close".to_string(),
            session_id: Some(body.session_id),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

async fn require_owned_daemon(
    state: &AppState,
    user_id: Uuid,
    daemon_id: &str,
) -> ApiResult<DaemonRecord> {
    ensure_owned_daemon(state, user_id, daemon_id).await?;
    let store = state.store.lock().await;
    store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id.trim() && d.owner_user_id == Some(user_id))
        .cloned()
        .ok_or_else(|| ApiError::not_found("daemon not found"))
}

async fn ensure_owned_daemon(state: &AppState, user_id: Uuid, daemon_id: &str) -> ApiResult<()> {
    let daemon_id = daemon_id.trim();
    let store = state.store.lock().await;
    let daemon = store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if daemon.owner_user_id == Some(user_id) {
        Ok(())
    } else {
        Err(ApiError::forbidden("daemon belongs to a different account"))
    }
}

fn trust_ui_html(origin: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>How trust works — Intendant Connect</title>
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b; --top: #181825; --surface: #1e1e2e; --surface-2: #313244;
      --line: rgba(205, 214, 244, 0.09); --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4; --muted: #a6adc8; --muted-2: #6c7086;
      --accent: #89b4fa; --accent-hover: #74c7ec; --lavender: #b4befe;
      --ok: #a6e3a1; --warn: #f9e2af; --err: #f38ba8;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg); color: var(--text);
    }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; background-color: var(--bg); background-image: radial-gradient(1100px 520px at 50% -160px, rgba(137, 180, 250, .12) 0%, rgba(137, 180, 250, 0) 62%); background-attachment: fixed; }}
    a {{ color: var(--accent); }}
    a:hover {{ color: var(--accent-hover); }}
    code {{ color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; overflow-wrap: anywhere; }}
    header {{ border-bottom: 1px solid var(--line); background: rgba(24, 24, 37, .82); }}
    .topbar {{ width: min(760px, calc(100vw - 32px)); margin: 0 auto; min-height: 60px; display: flex; align-items: center; gap: 12px; }}
    .brand-mark {{ width: 30px; height: 30px; display: block; flex: 0 0 auto; }}
    .topbar a {{ color: var(--text); text-decoration: none; font-weight: 700; font-size: 15px; }}
    main {{ width: min(760px, calc(100vw - 32px)); margin: 0 auto; padding: 34px 0 72px; line-height: 1.62; font-size: 15px; }}
    h1 {{ font-size: 28px; letter-spacing: -.015em; line-height: 1.15; margin: 0 0 8px; }}
    .lede {{ color: var(--muted); font-size: 16px; margin: 0 0 26px; }}
    h2 {{ font-size: 18px; margin: 34px 0 8px; letter-spacing: -.01em; }}
    p {{ margin: 10px 0; color: var(--text); }}
    p.dim, li span {{ color: var(--muted); }}
    ol, ul {{ padding-left: 22px; margin: 10px 0; display: grid; gap: 8px; }}
    li strong {{ display: block; }}
    .card {{ border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .6); border-radius: 12px; padding: 16px 18px; margin: 16px 0; }}
    .card.good {{ border-color: rgba(166, 227, 161, .35); }}
    .foot {{ margin-top: 34px; padding-top: 16px; border-top: 1px solid var(--line); color: var(--muted-2); font-size: 13px; }}
  </style>
</head>
<body>
  <header><div class="topbar"><img class="brand-mark" src="/logo.svg" alt=""><a href="/connect">Intendant Connect</a></div></header>
  <main>
    <h1>How trust works here</h1>
    <p class="lede">The short version: this service makes introductions and carries ciphertext. Authority over your computers never lives here &mdash; not even when you sign in.</p>

    <h2>What this service actually does</h2>
    <p>Four jobs, all deliberately powerless: it <em>introduces</em> your browser to your computers (signaling), <em>relays</em> encrypted traffic when networks are awkward, <em>stores</em> your fleet list as client-signed records whose private fields are end-to-end encrypted, and <em>remembers</em> which computers your account claimed. Every session that reaches one of your computers is verified twice at the ends: your browser checks a signature made by the computer itself, and the computer checks a signature made by your browser&rsquo;s own key &mdash; a key that never leaves your device.</p>

    <h2>"But I sign in with a passkey&hellip;"</h2>
    <p>A fair question: doesn&rsquo;t signing in give the server something it could use?</p>
    <p>A passkey never hands over a key. Your device signs a one-time challenge, bound to this origin &mdash; the server can&rsquo;t replay it anywhere, can&rsquo;t sign anything with it, and can&rsquo;t derive anything from it. The signature proves you <em>to the rendezvous, for rendezvous-scoped things</em>: your claim list, your encrypted fleet metadata, your signaling session. The encryption key for that metadata is computed inside your authenticator (the WebAuthn PRF extension) and handed only to the page in your browser &mdash; it is not part of what the server receives.</p>

    <h2>If this service turned malicious</h2>
    <ol>
      <li><strong>It could lie in introductions.</strong><span>When relaying, it could claim your account is someone else &mdash; but computers treat account claims as the weakest identity there is: they only matter if the computer&rsquo;s owner already granted that account a role locally, hosted sessions are capped below full control by default, and the strong identity in every offer is your browser&rsquo;s end-to-end signature, which this service cannot forge.</span></li>
      <li><strong>It could deny service.</strong><span>Any relay can. You would notice, and nothing would be exposed.</span></li>
      <li><strong>It could serve this page with malicious code.</strong><span>The honest residual risk of any hosted web app. It is bounded on purpose: sessions from this origin are role-capped by every computer&rsquo;s own policy, your durable identity key is scoped to each origin (code served here can never wield the key your own computer&rsquo;s dashboard holds), and organization membership never flows through accounts. If you don&rsquo;t want to extend even this much trust, don&rsquo;t: browse via your own computer&rsquo;s address, or run your own rendezvous.</span></li>
    </ol>

    <div class="card good">
      <strong>The rule the whole design follows:</strong> privileged code is served by you or by the resource owner; authority is only ever minted by the target computer&rsquo;s local access control; global services carry introductions, ciphertext, and signatures &mdash; nothing else.
    </div>

    <h2>Notifications</h2>
    <p class="dim">Optional Web Push alerts ("your computer went offline") are composed from the polling presence this service already sees &mdash; no new knowledge &mdash; and each payload is encrypted to your browser&rsquo;s subscription, so the push relays in between carry ciphertext.</p>

    <h2>Names are checkable here</h2>
    <p class="dim">Every name binding this service hands out &mdash; which key a computer had when claimed, handle creations, revocation lists, verified badges &mdash; is committed to an append-only transparency log. Your browser pins the signed tree head and re-verifies on every visit that history only ever grew. Handles can carry <em>verified identity</em> badges (a DNS record or GitHub gist you control); verification is decoration, never authority. Dormant handles with no computers and no sign-ins are eventually freed &mdash; squatted names don&rsquo;t keep.</p>

    <h2>Organizations</h2>
    <p class="dim">Org membership is a document signed by the organization&rsquo;s own key, verified by each of its computers directly. This service stores at most the org&rsquo;s <em>revocation list</em> &mdash; also root-signed and rollback-protected, so the worst a malicious board can do is withhold it, never forge it.</p>

    <h2>Verify all of this</h2>
    <p class="dim">The component is open and self-hostable: <a href="https://lovon-spec.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">run your own rendezvous</a>, read the <a href="https://lovon-spec.github.io/Intendant/trust-architecture.html" target="_blank" rel="noopener">full trust architecture</a>, or audit the <a href="https://github.com/lovon-spec/Intendant" target="_blank" rel="noopener">source</a>.</p>

    <div class="foot">This instance: <code>{origin}</code> &mdash; one deployment of an open component, not a chokepoint.</div>
  </main>
</body>
</html>"#
    )
}

const DOCS_URL: &str = "https://lovon-spec.github.io/Intendant/";
const REPO_URL: &str = "https://github.com/lovon-spec/intendant";

/// The deployment advisor — the lead of the landing install section: four
/// questions -> one command per platform (sh or PowerShell, `--service` where it belongs)
/// plus an honest fueling plan for after the claim. A separate const so
/// its CSS/JS braces stay out of the page-level `format!`; it derives
/// the command from `location.origin` at runtime, so a self-hosted
/// rendezvous advertises its own installer here too. The default answers'
/// command is server-rendered into the terminal (via the
/// `__ADVISOR_DEFAULT_CMD__` placeholder) so the page works without JS
/// and the one-command story is visible before any click. Every question
/// is about the agent's machine — the client side needs no install and
/// therefore no questions.
const LANDING_ADVISOR_HTML: &str = r##"<div class="advisor" id="advisor">
        <style>
          .advisor { border: 1px solid var(--line); border-radius: var(--radius); background: rgba(30, 30, 46, .55); }
          .advbody { padding: 16px; display: grid; gap: 14px; }
          .advq { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
          /* Labels take their own line so option rows keep an even rhythm
             at any column width (no ragged orphan buttons). */
          .advq .ql { flex: 0 0 100%; font-size: 13.5px; color: var(--muted-2); }
          .advq button { background: transparent; border: 1px solid var(--line-strong); color: var(--muted); border-radius: 999px; padding: 5px 13px; font-size: 13px; cursor: pointer; }
          .advq button:hover { color: var(--text); border-color: var(--accent); }
          .advq button.on { background: var(--surface-2); color: var(--text); border-color: var(--accent); }
          .advout { border-top: 1px solid var(--line); padding-top: 14px; display: grid; gap: 10px; }
          .advout ul { margin: 0; padding-left: 20px; font-size: 14px; color: var(--muted); display: grid; gap: 6px; }
          .advout ul b { color: var(--text); }
          .advout ul:empty { display: none; }
        </style>
        <div class="advbody">
          <div class="advq" data-q="os">
            <span class="ql">OS on the agent's machine?</span>
            <button data-v="linux" class="on">Linux</button>
            <button data-v="macos">macOS</button>
            <button data-v="windows">Windows</button>
          </div>
          <div class="advq" data-q="box">
            <span class="ql">What kind of machine?</span>
            <button data-v="vps" class="on">A rented VPS</button>
            <button data-v="server">My own always-on machine</button>
            <button data-v="laptop">The machine I'm on now</button>
          </div>
          <div class="advq" data-q="fuel">
            <span class="ql">What will fuel it?</span>
            <button data-v="api" class="on">API keys</button>
            <button data-v="sub">Subscriptions (Codex, Claude Code)</button>
            <button data-v="both">Both</button>
          </div>
          <div class="advq" data-q="solo">
            <span class="ql">Keep working with your browser closed?</span>
            <button data-v="no" class="on">No — while I watch</button>
            <button data-v="yes">Yes — unattended runs</button>
          </div>
          <div class="advout">
            <div class="terminal">
              <div class="tbar">
                <span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>
                <span class="bftitle" id="advtitle">fresh box — sh</span>
                <button onclick="navigator.clipboard&&navigator.clipboard.writeText(document.getElementById('advcmd').textContent)">copy</button>
              </div>
              <pre><span class="ps" id="advps">$ </span><span id="advcmd">__ADVISOR_DEFAULT_CMD__</span></pre>
            </div>
            <ul id="advplan"></ul>
            <p class="installnote" id="advnote"></p>
          </div>
        </div>
        <script>
        (function () {
          var pick = { os: 'linux', box: 'vps', fuel: 'api', solo: 'no' };
          function render() {
            // </> keep raw angle brackets out of the inline
            // script (the page-level invariant the tests pin).
            var svc = pick.box !== 'laptop';
            var cmd = pick.os === 'windows'
              ? '& ([scriptblock]::Create((irm ' + location.origin + "/install.ps1))) -Owner '\u003cyour-key\u003e'" + (svc ? ' -Service' : '')
              : 'curl -fsSL ' + location.origin + '/install.sh | sh -s -- --owner \u003cyour-key\u003e' + (svc ? ' --service' : '');
            document.getElementById('advps').textContent = pick.os === 'windows' ? 'PS> ' : '$ ';
            document.getElementById('advtitle').textContent = pick.os === 'windows' ? 'fresh box — PowerShell' : 'fresh box — sh';
            document.getElementById('advcmd').textContent = cmd;
            var plan = [];
            if (pick.box === 'laptop') {
              plan.push('<b>Fueling is optional here.</b> A local .env key works as-is; the vault still adds cross-device sync and one-click revocation.');
            } else {
              var watched = pick.solo === 'no';
              if (pick.fuel !== 'sub') {
                plan.push(watched
                  ? '<b>Anthropic & Gemini: client egress.</b> The box never holds a key — its provider calls detour through this browser, and stop when it closes. OpenAI’s API refuses browser relay, so lease that one with the offline window at “while connected only”.'
                  : '<b>API keys: leases with a 24 h offline window.</b> Borrowed in memory only, never on disk, revocable from any signed-in device.');
              }
              if (pick.fuel !== 'api') {
                plan.push('<b>Subscriptions: access-token OAuth leases</b> (the default) — your browser refreshes the token and leases only the short-lived result. Codex works out of the box.');
                plan.push(watched
                  ? '<b>Claude Code</b> still needs the full-credential opt-in (Anthropic’s token endpoint refuses browser refresh) — decide per box.'
                  : '<b>Unattended subscription runs</b> beyond the token’s life (≈ 1 h) need the full-credential opt-in: the honest trade is durable authority on the box for the lease window. Claude Code always needs it today.');
              }
            }
            document.getElementById('advplan').innerHTML = plan.map(function (item) { return '<li>' + item + '</li>'; }).join('');
            var note = { vps: 'A disposable box should hold nothing durable. With client egress the key was never on it; with access-token leases what lands there dies in minutes. Wipe it — or lose it — and nothing leaks.',
                         server: 'Nothing rests on disk either way — leases only bound what a runtime compromise could spend before you revoke.',
                         laptop: 'Custody buys the least on the machine your browser already runs on.' }[pick.box];
            if (svc) {
              note += pick.os === 'windows'
                ? ' -Service installs a Task Scheduler entry (at boot when elevated, at logon otherwise) supervised by a built-in restart loop; the installer prints the log file the claim phrase lands in. Run it from PowerShell.'
                : ' --service keeps the daemon alive past this SSH session via the platform’s own supervisor — systemd where present, launchd on macOS, cron plus the built-in supervisor elsewhere — and prints where the claim phrase lands.';
            }

            document.getElementById('advnote').textContent = note;
          }
          Array.prototype.forEach.call(document.querySelectorAll('#advisor .advq'), function (row) {
            Array.prototype.forEach.call(row.querySelectorAll('button'), function (button) {
              button.addEventListener('click', function () {
                Array.prototype.forEach.call(row.querySelectorAll('button'), function (other) { other.classList.remove('on'); });
                button.classList.add('on');
                pick[row.getAttribute('data-q')] = button.getAttribute('data-v');
                render();
              });
            });
          });
          render();
        })();
        </script>
      </div>"##;

/// The public landing page at `/`. Deliberately static and dependency-free;
/// the install one-liner is origin-aware so a self-hosted rendezvous
/// advertises its own installer.
fn landing_ui_html(origin: &str) -> String {
    // The placeholder must be entity-escaped or the browser eats it as a tag.
    let install_cmd = format!("curl -fsSL {origin}/install.sh | sh -s -- --owner &lt;your-key&gt;");
    // r## because the page contains fragment links (`href="#install"`),
    // whose `"#` would terminate a plain r#-string.
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant — an operating environment for autonomous AI agents</title>
  <meta name="description" content="Give an AI agent a full machine — shell, files, display, voice — under layered human oversight. Your keys stay yours.">
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b;
      --top: #181825;
      --surface: #1e1e2e;
      --surface-2: #313244;
      --line: rgba(205, 214, 244, 0.09);
      --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4;
      --muted: #a6adc8;
      --muted-2: #6c7086;
      --accent: #89b4fa;
      --accent-hover: #74c7ec;
      --accent-ink: #11111b;
      --lavender: #b4befe;
      --ok: #a6e3a1;
      --warn: #f9e2af;
      --radius: 12px;
      --shadow: 0 18px 50px rgba(0, 0, 0, .35);
    }}
    * {{ box-sizing: border-box; }}
    html {{ scroll-behavior: smooth; }}
    @media (prefers-reduced-motion: reduce) {{ html {{ scroll-behavior: auto; }} }}
    body {{
      margin: 0;
      background:
        radial-gradient(1200px 500px at 70% -10%, rgba(137, 180, 250, .10), transparent 60%),
        radial-gradient(900px 420px at 10% 0%, rgba(180, 190, 254, .07), transparent 55%),
        var(--bg);
      color: var(--text);
      font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    }}
    a {{ color: var(--accent); text-decoration: none; }}
    a:hover {{ color: var(--accent-hover); }}
    .wrap {{ max-width: 1080px; margin: 0 auto; padding: 0 22px; }}
    header {{
      display: flex; align-items: center; justify-content: space-between;
      padding: 18px 0; flex-wrap: wrap; gap: 10px 18px;
    }}
    .mark {{ display: flex; align-items: center; font-weight: 700; letter-spacing: .3px; font-size: 17px; color: var(--text); }}
    .mark img {{ width: 26px; height: 26px; display: block; margin-right: 9px; }}
    .mark span {{ color: var(--accent); }}
    .mark .pill-alpha {{
      margin-left: 10px; padding: 2px 9px; border: 1px solid var(--line-strong);
      border-radius: 999px; font-size: 10.5px; font-weight: 700;
      letter-spacing: .12em; text-transform: uppercase; color: var(--muted-2);
    }}
    nav {{ display: flex; gap: 14px 20px; align-items: center; font-size: 14.5px; flex-wrap: wrap; }}
    nav a {{ color: var(--muted); white-space: nowrap; }}
    nav a:hover {{ color: var(--text); }}
    .btn {{
      display: inline-block; padding: 9px 18px; border-radius: 999px;
      background: var(--accent); color: var(--accent-ink); font-weight: 600;
      border: 1px solid transparent;
    }}
    .btn:hover {{ background: var(--accent-hover); color: var(--accent-ink); }}
    .btn.ghost {{ background: transparent; color: var(--text); border-color: var(--line-strong); }}
    .btn.ghost:hover {{ border-color: var(--accent); color: var(--accent); }}
    .hero {{ padding: 64px 0 10px; text-align: center; }}
    .hero h1 {{
      margin: 0 auto 18px; font-size: clamp(31px, 5.5vw, 49px); line-height: 1.13;
      letter-spacing: -.6px; max-width: 21ch;
    }}
    .hero h1 em {{ font-style: normal; color: var(--lavender); }}
    .hero p {{ margin: 0 auto 28px; font-size: 17.5px; color: var(--muted); max-width: 680px; }}
    .cta {{ display: flex; gap: 12px; flex-wrap: wrap; justify-content: center; }}
    /* Framed product shots */
    .heroshot {{ position: relative; margin: 92px 0 0; }}
    .heroshot::before {{
      content: ""; position: absolute; inset: -60px 0 auto; height: 340px;
      background: radial-gradient(640px 260px at 50% 20%, rgba(137, 180, 250, .16), transparent 70%);
      pointer-events: none;
    }}
    .browserframe {{
      position: relative; background: var(--top); border: 1px solid var(--line-strong);
      border-radius: 14px; box-shadow: var(--shadow); overflow: hidden;
    }}
    .bfbar {{
      display: flex; align-items: center; gap: 7px; padding: 10px 14px;
      border-bottom: 1px solid var(--line);
    }}
    .dot {{ width: 10px; height: 10px; border-radius: 50%; }}
    .dot.r {{ background: rgba(243, 139, 168, .75); }}
    .dot.y {{ background: rgba(249, 226, 175, .75); }}
    .dot.g {{ background: rgba(166, 227, 161, .75); }}
    .bftitle {{
      margin-left: 8px; font: 12.5px/1 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      color: var(--muted-2); letter-spacing: .3px;
    }}
    .browserframe img, .shot img {{ display: block; width: 100%; height: auto; }}
    .shotcaption {{
      margin: 14px auto 0; max-width: 740px; text-align: center;
      font-size: 13.5px; color: var(--muted-2);
    }}
    /* The tour: alternating text/screenshot rows */
    .tour {{ padding: 84px 0 0; }}
    .trow {{
      display: grid; grid-template-columns: minmax(0, .92fr) minmax(0, 1.08fr);
      gap: 48px; align-items: center; padding: 30px 0;
    }}
    .trow.rev .txt {{ order: 2; }}
    .eyebrow {{
      font-size: 12px; font-weight: 700; letter-spacing: .14em;
      text-transform: uppercase; color: var(--accent); margin-bottom: 10px;
    }}
    .trow h3 {{ margin: 0 0 12px; font-size: 23px; letter-spacing: -.3px; }}
    .trow .txt p {{ margin: 0; font-size: 15.5px; color: var(--muted); }}
    .shot {{
      background: var(--top); border: 1px solid var(--line-strong);
      border-radius: 12px; box-shadow: var(--shadow); overflow: hidden;
    }}
    .shotnote {{ margin-top: 10px; font-size: 13px; color: var(--muted-2); }}
    /* Custody: the two fueling modes, told by what travels */
    .fuelmap {{ margin-top: 16px; display: grid; gap: 9px; }}
    .fuelrow {{ display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; }}
    .fueltag {{
      flex: 0 0 auto; min-width: 96px; text-align: center; padding: 2px 8px;
      border: 1px solid var(--line-strong); border-radius: 6px;
      font: 700 10.5px/1.7 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      letter-spacing: .08em; text-transform: uppercase; color: var(--accent);
    }}
    .fuelrow:last-child .fueltag {{ color: var(--ok); }}
    .fuelflow {{ flex: 1; min-width: 230px; font-size: 13px; color: var(--muted-2); }}
    .fuelflow em {{ font-style: normal; color: var(--muted); }}
    .fuelflow .fx {{ opacity: .65; padding: 0 1px; }}
    /* The phone row: a bezel, not a browser frame */
    .phonepic {{ display: grid; justify-items: center; }}
    .phonepic .shotnote {{ text-align: center; }}
    .phoneframe {{
      width: min(280px, 72vw); padding: 10px; border-radius: 44px;
      background: #0d0d15; border: 1px solid var(--line-strong);
      box-shadow: var(--shadow);
    }}
    .phoneframe img {{ display: block; width: 100%; height: auto; border-radius: 34px; }}
    section h2 {{ font-size: 24px; margin: 0 0 20px; letter-spacing: -.3px; }}
    .sectionlede {{ margin: -10px 0 24px; font-size: 15px; color: var(--muted); max-width: 660px; }}
    section.features {{ padding: 78px 0 0; }}
    .grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 14px; }}
    .card {{
      border: 1px solid var(--line); border-radius: var(--radius);
      background: var(--surface); padding: 18px 18px 16px;
    }}
    .card h3 {{ margin: 0 0 8px; font-size: 16px; }}
    .card p {{ margin: 0; font-size: 14px; color: var(--muted); }}
    /* Install */
    /* Sits directly under the hero: "how do I use it" is the first answer
       the page gives, so it gets hero-adjacent spacing, not section spacing. */
    .install-section {{ padding: 46px 0 0; }}
    .igrid {{ display: grid; grid-template-columns: minmax(0, 1.15fr) minmax(0, .85fr); gap: 30px; align-items: start; }}
    .terminal {{
      background: var(--top); border: 1px solid var(--line-strong);
      border-radius: var(--radius); box-shadow: var(--shadow); overflow: hidden;
    }}
    .tbar {{
      display: flex; align-items: center; gap: 7px; padding: 9px 14px;
      border-bottom: 1px solid var(--line);
    }}
    .tbar .bftitle {{ flex: 1; }}
    .tbar button {{
      background: transparent; border: 1px solid var(--line-strong); color: var(--muted);
      border-radius: 6px; padding: 3px 10px; font-size: 12px; cursor: pointer;
    }}
    .tbar button:hover {{ color: var(--text); border-color: var(--accent); }}
    .terminal pre {{
      margin: 0; padding: 16px 18px;
      font: 13.5px/1.7 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      color: var(--ok);
      white-space: pre-wrap; overflow-wrap: break-word;
    }}
    .terminal pre .ps {{ color: var(--muted-2); user-select: none; }}
    .installnote {{ margin: 14px 2px 0; font-size: 13px; color: var(--muted-2); }}
    .steps {{ display: grid; gap: 12px; }}
    .step {{
      border: 1px solid var(--line); border-radius: var(--radius);
      background: rgba(30, 30, 46, .55); padding: 14px 16px; font-size: 14px; color: var(--muted);
    }}
    .step b {{ display: block; color: var(--text); margin-bottom: 4px; font-size: 14.5px; }}
    .step .n {{ color: var(--accent); font-weight: 700; margin-right: 6px; }}
    .whyname {{ padding: 78px 0 0; }}
    .whyname p {{ max-width: 65ch; margin: 0; font-size: 15.5px; color: var(--muted); line-height: 1.65; }}
    .whyname p strong {{ color: var(--text); font-weight: 600; }}
    .trustrow {{ padding: 78px 0 20px; }}
    .trustrow .card {{ background: rgba(166, 227, 161, .05); border-color: rgba(166, 227, 161, .18); }}
    footer {{
      margin-top: 64px; padding: 26px 0 40px; border-top: 1px solid var(--line);
      display: flex; justify-content: space-between; gap: 14px; flex-wrap: wrap;
      font-size: 13.5px; color: var(--muted-2);
    }}
    footer nav {{ gap: 16px; }}
    @media (max-width: 920px) {{
      .hero {{ padding-top: 46px; }}
      /* minmax(0, …) everywhere: a bare 1fr keeps the min-content floor and
         lets wide content (the install one-liner) stretch the page. */
      .trow {{ grid-template-columns: minmax(0, 1fr); gap: 16px; padding: 24px 0; }}
      .trow.rev .txt {{ order: 0; }}
      .tour {{ padding-top: 60px; }}
      .igrid {{ grid-template-columns: minmax(0, 1fr); }}
      section.features, .whyname, .trustrow {{ padding-top: 56px; }}
      .install-section {{ padding-top: 36px; }}
      .heroshot {{ margin-top: 64px; }}
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <header>
      <div class="mark"><img src="/logo.svg" alt="">intendant<span>.dev</span><span class="pill-alpha">pre-alpha</span></div>
      <nav>
        <a href="/trust">How trust works</a>
        <a href="{DOCS_URL}">Docs</a>
        <a href="{REPO_URL}">GitHub</a>
        <a href="#install">Install</a>
        <a class="btn ghost" href="/connect">Sign in</a>
      </nav>
    </header>

    <section class="hero">
      <h1>Give an AI agent a full machine — <em>under your oversight</em></h1>
      <p>
        Intendant is an open-source operating environment for autonomous AI
        agents: a shell, files, a display it can see and control, voice, and
        phone calls — with layered human supervision. It runs its own agent
        loop, supervises Codex and Claude Code as managed backends, and is
        portable across OpenAI, Anthropic, and Gemini. The agent's machine
        can run macOS, Linux, or Windows; yours just needs a browser —
        nothing to install on your side of the glass.
      </p>
      <div class="cta">
        <a class="btn" href="/connect">Open your dashboard</a>
        <a class="btn ghost" href="#install">Install a daemon</a>
      </div>
    </section>

    <section class="install-section" id="install">
      <h2>Stand up a daemon in about ninety seconds</h2>
      <p class="sectionlede">
        Four answers about the machine the agent will live on, and the exact
        command appears. That machine is the only one that installs anything
        — you can be reading this from your phone.
      </p>
      <div class="igrid">
        {advisor}
        <div>
          <div class="steps">
            <div class="step"><b><span class="n">1</span>Install</b>
              One command on a fresh box pins root authority to your browser's key. Nothing sensitive travels.</div>
            <div class="step"><b><span class="n">2</span>Claim</b>
              The daemon prints a twelve-word phrase; claim it from the browser you're already holding.</div>
            <div class="step"><b><span class="n">3</span>Fuel</b>
              Grant time-boxed credential leases from your encrypted vault — or relay calls through your browser and never hand over a key at all.</div>
          </div>
          <p class="installnote">
            New here? <a href="/connect">Sign in</a> first — your key is in the
            dashboard's Access drawer. Nothing sensitive travels in the command
            or lands on the box: the daemon boots already owned by you, you claim
            it with a twelve-word phrase, and it borrows credentials from your
            vault only while you let it.
          </p>
        </div>
      </div>
    </section>

    <section class="heroshot">
      <div class="browserframe">
        <div class="bfbar">
          <span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>
          <span class="bftitle">atlas — Intendant dashboard</span>
        </div>
        <img src="/assets/landing/hero.webp" width="2200" height="1192" fetchpriority="high"
             alt="The Intendant dashboard's Activity feed: an agent diagnoses a failing nightly job with an auto-approved tail command, proposes a one-line diff to jobs/rollup.py, waits for an approval-gated backfill run, and reports the verified result.">
      </div>
      <p class="shotcaption">
        The Activity feed on a claimed daemon: autonomy is a dial, approvals
        are explicit, and every command, diff, and decision is logged and
        replayable.
      </p>
    </section>

    <section class="tour">
      <div class="trow">
        <div class="txt">
          <div class="eyebrow">The desktop</div>
          <h3>A real desktop, watched</h3>
          <p>The agent gets a display it can see and drive — a browser, a
          terminal, whatever the task needs — and you watch it stream live
          over WebRTC. Input stays yours to share: take control at any
          moment, annotate what you see, record what happened.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/video.webp" width="2000" height="1119"
                 alt="The dashboard's Video tab streaming a live agent desktop over WebRTC: a browser and a terminal scrolling a build, with view-only, annotate, record, and take-control affordances.">
          </div>
          <div class="shotnote">Watching atlas's display, live — view-only until you hand input over.</div>
        </div>
      </div>

      <div class="trow rev">
        <div class="txt">
          <div class="eyebrow">Mission control</div>
          <h3>Every agent, one canvas</h3>
          <p>Station renders the whole machine live — sessions, approvals,
          context budgets, changes, and worktrees orbiting one WebGPU canvas.
          The same state is a keystroke away in the terminal TUI and the CLI,
          and a glance away from your phone.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/station.webp" width="2000" height="1014"
                 alt="The Station tab: a radar-style WebGPU control room showing live nodes for peers, sessions, activity, context, changes, view, controls, and worktrees.">
          </div>
          <div class="shotnote">Station — the fleet and every session's state, rendered live.</div>
        </div>
      </div>

      <div class="trow">
        <div class="txt">
          <div class="eyebrow">Credential custody</div>
          <h3>Fueling, not surrendering</h3>
          <p>Provider keys and subscription OAuth live end-to-end encrypted
          behind your passkeys, and a machine gets fuel one of two ways. A
          lease is borrowed authority — held in memory, renewed from your
          browser, dead on expiry or the moment you revoke it. Client egress
          goes further: the key never leaves your browser at all — the box's
          provider calls detour through the tab you're signed in on. A
          disposable VPS can be wiped, or seized, with nothing on it worth
          taking.</p>
          <div class="fuelmap">
            <div class="fuelrow"><span class="fueltag">lease</span>
              <span class="fuelflow">the key travels: vault <span class="fx">→</span> daemon memory <em>(expires on its own)</em> <span class="fx">→</span> provider calls from the box</span></div>
            <div class="fuelrow"><span class="fueltag">client egress</span>
              <span class="fuelflow">the calls travel: daemon <span class="fx">→</span> your browser <em>(the key stays here)</em> <span class="fx">→</span> provider</span></div>
          </div>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/vault.webp" width="1800" height="975"
                 alt="The credential vault panel: three credentials with masked secrets, two active leases expiring in 15 minutes granted by @ada, re-fuel buttons, and a client-egress relay option.">
          </div>
          <div class="shotnote">Leases expire on their own; Revoke is always one click away.</div>
        </div>
      </div>

      <div class="trow rev">
        <div class="txt">
          <div class="eyebrow">Arrival</div>
          <h3>Claim a machine with twelve words</h3>
          <p>Start the daemon anywhere and it prints a claim phrase. Paste it
          in the browser you're already holding and the box is yours — owned
          by your key from first boot, reachable from every device you sign
          in on, with the powerful knobs one fold away when you want them.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/claim.webp" width="1800" height="635"
                 alt="Intendant Connect: a claimed computer named atlas shown online with uptime history, next to the add-a-computer flow that accepts a twelve-word claim phrase.">
          </div>
          <div class="shotnote">atlas, online seconds after its claim phrase was pasted.</div>
        </div>
      </div>

      <div class="trow">
        <div class="txt">
          <div class="eyebrow">The client</div>
          <h3>Nothing to install on your side</h3>
          <p>Most agent environments start by installing software on the
          device in front of you. Intendant never does: the whole client is
          a browser tab. Approve a diff from your phone, watch the live
          desktop from a tablet, run mission control from any laptop — same
          daemon, same authority, zero client software. On intendant.dev
          there is nothing to set up at all; even fully self-hosted, the
          one-time cost is trusting a certificate, never installing an app.</p>
        </div>
        <div class="pic phonepic">
          <div class="phoneframe">
            <img loading="lazy" src="/assets/landing/phone.webp" width="780" height="1688"
                 alt="The same Intendant session on a phone: the Activity feed showing the agent's diff, an approval-gated backfill command, and the verified result — driven entirely from a mobile browser.">
          </div>
          <div class="shotnote">The rollup fix from above — same session, held in one hand.</div>
        </div>
      </div>
    </section>

    <section class="features">
      <h2>What's in the box</h2>
      <div class="grid">
        <div class="card">
          <h3>Bring your own agent</h3>
          <p>Codex and Claude Code run as managed backends — under the
          same oversight, autonomy dial, and session logging as the
          native agent loop.</p>
        </div>
        <div class="card">
          <h3>Your keys stay yours</h3>
          <p>Provider keys and subscription OAuth live end-to-end encrypted
          behind your passkeys. Daemons borrow leases that expire, or relay
          calls through your browser; disks hold nothing worth stealing.</p>
        </div>
        <div class="card">
          <h3>Every interface, any device</h3>
          <p>Web dashboard, terminal TUI, CLI, MCP, live voice, and phone
          calls — every capability reachable from each of them. The web
          client runs in any browser, phone included, with nothing to
          install client-side.</p>
        </div>
        <div class="card">
          <h3>A fleet, not a box</h3>
          <p>Daemons federate: shared displays, cross-machine sessions, and
          organization-signed access — all enforced locally by each daemon's
          own IAM, never by this service.</p>
        </div>
      </div>
    </section>

    <section class="whyname">
      <h2>Why “Intendant”</h2>
      <p>In a theater, performers play and conductors orchestrate — the
      <strong>Intendant</strong> runs the house: who gets the stage, which
      productions run, on whose authority, with the books open. Here agents
      perform, orchestrators conduct (Codex and Claude Code as guest
      conductors), and the Intendant runs the house and answers to you —
      houses federate, companies tour on signed contracts, house rules always
      win: a network of agentic networks.</p>
    </section>

    <section class="trustrow">
      <h2>Built to be distrusted</h2>
      <div class="grid">
        <div class="card">
          <h3>This service holds no authority</h3>
          <p>The rendezvous stores ciphertext and relays signaling. Your
          daemons mint and enforce their own access; passkeys and a
          transparency log keep the service honest — and you can
          <a href="/trust">read exactly what it can and cannot do</a>,
          or run your own.</p>
        </div>
        <div class="card">
          <h3>The sandbox never holds keys</h3>
          <p>Inside each daemon, the sandboxed process that executes
          commands never sees an API key, and the process that talks to
          model providers never executes commands. A hijacked conversation
          can't steal credentials; a hijacked shell can't phone home
          through the model — by construction, not by policy.</p>
        </div>
      </div>
    </section>

    <footer>
      <div>Intendant — open source, self-hostable, provider-agnostic.</div>
      <nav>
        <a href="/trust">Trust</a>
        <a href="/install.sh">install.sh</a>
        <a href="{DOCS_URL}">Docs</a>
        <a href="{REPO_URL}">GitHub</a>
      </nav>
    </footer>
  </div>
</body>
</html>"##,
        // Server-render the default answers' command (Linux VPS ⇒ --service)
        // so the terminal shows a real, origin-aware one-liner before any
        // JS runs; render() redraws the same text on load.
        advisor = LANDING_ADVISOR_HTML.replace(
            "__ADVISOR_DEFAULT_CMD__",
            &format!("{install_cmd} --service")
        ),
    )
}

fn connect_ui_html(origin: &str, product_title: &str, account_subtitle: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{product_title}</title>
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b;
      --top: #181825;
      --surface: #1e1e2e;
      --surface-2: #313244;
      --surface-3: #45475a;
      --line: rgba(205, 214, 244, 0.09);
      --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4;
      --muted: #a6adc8;
      --muted-2: #6c7086;
      --accent: #89b4fa;
      --accent-hover: #74c7ec;
      --accent-ink: #11111b;
      --lavender: #b4befe;
      --ok: #a6e3a1;
      --warn: #f9e2af;
      --err: #f38ba8;
      --focus: #f9e2af;
      --shadow: 0 18px 50px rgba(0, 0, 0, .35);
      --radius: 12px;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg);
      color: var(--text);
    }}
    * {{ box-sizing: border-box; }}
    html {{ min-height: 100%; }}
    body {{ margin: 0; min-height: 100vh; background-color: var(--bg); background-image: radial-gradient(1100px 520px at 50% -160px, rgba(137, 180, 250, .14) 0%, rgba(137, 180, 250, 0) 62%), radial-gradient(ellipse at 50% -12%, #1e1e2e 0%, #11111b 72%); background-attachment: fixed; background-repeat: no-repeat; }}
    button, input {{ font: inherit; }}
    button {{ height: 38px; padding: 0 15px; color: var(--accent-ink); background: var(--accent); border: 1px solid transparent; border-radius: 8px; font-weight: 700; cursor: pointer; transition: background .16s ease, border-color .16s ease, color .16s ease, transform .12s ease, box-shadow .16s ease; white-space: nowrap; }}
    button:hover:not(:disabled) {{ background: var(--accent-hover); transform: translateY(-1px); box-shadow: 0 6px 18px rgba(137, 180, 250, .25); }}
    button:focus-visible, input:focus-visible, a:focus-visible, summary:focus-visible {{ outline: 2px solid var(--focus); outline-offset: 2px; border-radius: 6px; }}
    button.secondary {{ color: var(--text); background: var(--surface-2); border-color: var(--line-strong); }}
    button.secondary:hover:not(:disabled) {{ background: var(--surface-3); box-shadow: none; }}
    button.ghost {{ color: var(--muted); background: transparent; border-color: var(--line); }}
    button.ghost:hover:not(:disabled) {{ color: var(--text); background: var(--surface-2); box-shadow: none; }}
    button.danger {{ color: var(--err); background: rgba(243, 139, 168, .08); border-color: rgba(243, 139, 168, .45); }}
    button.danger:hover:not(:disabled) {{ background: rgba(243, 139, 168, .16); box-shadow: none; }}
    button.linklike {{ height: auto; padding: 0; color: var(--accent); background: none; border: 0; font-weight: 700; }}
    button.linklike:hover:not(:disabled) {{ color: var(--accent-hover); transform: none; box-shadow: none; text-decoration: underline; }}
    button:disabled {{ opacity: .58; cursor: default; transform: none; box-shadow: none; }}
    input {{ width: 100%; min-width: 0; height: 42px; padding: 9px 12px; color: var(--text); background: rgba(17, 17, 27, .8); border: 1px solid var(--line-strong); border-radius: 8px; transition: border-color .16s ease; }}
    input:hover {{ border-color: rgba(205, 214, 244, .26); }}
    input::placeholder {{ color: var(--muted-2); }}
    a {{ color: var(--accent); }}
    a:hover {{ color: var(--accent-hover); }}
    code {{ color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; overflow-wrap: anywhere; }}

    header {{ border-bottom: 1px solid var(--line); background: rgba(24, 24, 37, .82); backdrop-filter: blur(10px); position: sticky; top: 0; z-index: 5; }}
    .topbar {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; min-height: 64px; display: flex; align-items: center; justify-content: space-between; gap: 18px; }}
    .brand {{ display: flex; align-items: center; gap: 12px; min-width: 0; }}
    .brand-mark {{ width: 34px; height: 34px; display: block; flex: 0 0 auto; }}
    .brand h1 {{ font-size: 17px; line-height: 1.15; margin: 0; }}
    .brand-sub {{ color: var(--muted-2); font-size: 12px; margin-top: 2px; }}
    .top-actions {{ display: flex; align-items: center; gap: 9px; }}
    .session-chip {{ display: inline-flex; align-items: center; gap: 8px; min-height: 32px; padding: 0 12px; border: 1px solid var(--line-strong); border-radius: 999px; background: var(--surface); color: var(--text); font-size: 13px; font-weight: 700; }}
    .session-chip .dot {{ width: 7px; height: 7px; border-radius: 50%; background: var(--ok); }}

    main.shell {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; padding: 26px 0 56px; display: grid; gap: 18px; animation: rise .35s ease; }}
    @keyframes rise {{ from {{ opacity: 0; transform: translateY(6px); }} to {{ opacity: 1; transform: none; }} }}
    @media (prefers-reduced-motion: reduce) {{ main.shell {{ animation: none; }} button:hover:not(:disabled) {{ transform: none; }} }}

    /* ── Signed out: hero ── */
    body.signed-out main.shell {{ width: min(560px, calc(100vw - 32px)); padding-top: 7vh; }}
    .hero {{ text-align: center; display: grid; gap: 14px; justify-items: center; padding: 8px 0 22px; }}
    .hero-mark {{ width: 58px; height: 58px; display: block; border-radius: 16px; box-shadow: var(--shadow); }}
    .hero-title {{ font-size: 32px; line-height: 1.12; margin: 6px 0 0; letter-spacing: -.015em; }}
    .hero-sub {{ color: var(--muted); font-size: 15px; line-height: 1.55; margin: 0; max-width: 46ch; }}
    .auth-card {{ border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); padding: 22px; display: grid; gap: 14px; }}
    .auth-row {{ display: flex; gap: 9px; }}
    .auth-row input {{ flex: 1 1 auto; }}
    .auth-row button {{ height: 42px; flex: 0 0 auto; }}
    .auth-alt {{ color: var(--muted); font-size: 13px; display: flex; gap: 6px; align-items: baseline; }}
    .auth-note {{ font-size: 12.5px; line-height: 1.55; color: var(--muted-2); }}
    .auth-note a {{ color: var(--muted); }}
    .auth-note a:hover {{ color: var(--accent); }}
    .feature-strip {{ list-style: none; margin: 6px 0 0; padding: 0; display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }}
    .feature-strip li {{ border: 1px solid var(--line); border-radius: 10px; background: rgba(24, 24, 37, .5); padding: 12px 13px; display: grid; gap: 4px; }}
    .feature-strip strong {{ font-size: 13px; }}
    .feature-strip span {{ color: var(--muted-2); font-size: 12px; line-height: 1.45; }}
    body.signed-in #auth {{ display: none; }}

    /* ── Signed in: computers ── */
    .section-head {{ display: flex; align-items: baseline; justify-content: space-between; gap: 14px; padding: 4px 2px 0; }}
    .section-head h2 {{ font-size: 20px; margin: 0; letter-spacing: -.01em; }}
    .section-head .sub {{ color: var(--muted-2); font-size: 13px; }}
    .computer-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; align-items: start; }}
    .computer-grid.empty {{ grid-template-columns: minmax(300px, 460px); justify-content: center; }}
    .computer-card {{ min-width: 0; border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); padding: 18px; display: grid; gap: 12px; align-content: start; transition: border-color .16s ease, transform .16s ease; }}
    .computer-card:hover {{ border-color: rgba(205, 214, 244, .24); }}
    .computer-head {{ display: flex; align-items: center; gap: 10px; min-width: 0; }}
    .computer-dot {{ width: 9px; height: 9px; border-radius: 50%; background: var(--muted-2); flex: 0 0 auto; }}
    .computer-dot.ok {{ background: var(--ok); box-shadow: 0 0 8px rgba(166, 227, 161, .6); }}
    .computer-name {{ min-width: 0; display: grid; gap: 2px; }}
    .computer-name strong {{ font-size: 15px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    .computer-name .sub {{ color: var(--muted-2); font-size: 12px; }}
    .computer-actions {{ display: flex; gap: 8px; flex-wrap: wrap; }}
    .computer-actions .open {{ flex: 1 1 auto; }}
    .presence {{ display: grid; gap: 5px; }}
    .presence-bars {{ display: flex; gap: 2px; align-items: flex-end; height: 14px; }}
    .presence-bars span {{ flex: 1 1 auto; min-width: 2px; height: 5px; border-radius: 1px; background: var(--surface-3); }}
    .presence-bars span.on {{ height: 14px; background: var(--ok); opacity: .75; }}
    .presence-label {{ color: var(--muted-2); font-size: 11px; }}
    .computer-card details {{ border-top: 1px solid var(--line); padding-top: 10px; }}
    .computer-card summary {{ color: var(--muted-2); font-size: 12px; font-weight: 700; cursor: pointer; list-style: none; }}
    .computer-card summary::before {{ content: '▸ '; }}
    .computer-card details[open] summary::before {{ content: '▾ '; }}
    .kv {{ display: grid; gap: 8px; margin-top: 10px; }}
    .kv .k {{ color: var(--muted-2); font-size: 11px; font-weight: 800; text-transform: uppercase; letter-spacing: .04em; }}
    .kv code {{ display: block; font-size: 12px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 6px; background: rgba(17, 17, 27, .55); }}
    .kv .danger-row {{ margin-top: 4px; }}
    .add-card {{ border-style: dashed; background: rgba(24, 24, 37, .45); }}
    .add-card h3 {{ margin: 0; font-size: 15px; }}
    .steps {{ margin: 0; padding: 0 0 0 18px; color: var(--muted); font-size: 13px; line-height: 1.55; display: grid; gap: 6px; }}
    .steps code {{ font-size: 12px; }}
    label {{ display: block; color: var(--muted); font-size: 12px; font-weight: 700; margin-bottom: 7px; }}
    .status {{ min-height: 18px; color: var(--muted); font-size: 13px; line-height: 1.4; overflow-wrap: anywhere; }}
    .status.status-ok {{ color: var(--ok); }}
    .status.status-err {{ color: var(--err); }}
    .status.status-warn {{ color: var(--warn); }}
    .empty-hint {{ color: var(--muted-2); font-size: 13px; }}

    /* ── Saved places + advanced ── */
    section.panel {{ min-width: 0; border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); }}
    .panel-header {{ padding: 15px 18px; border-bottom: 1px solid var(--line); display: flex; align-items: center; justify-content: space-between; gap: 14px; }}
    .panel-header h2 {{ font-size: 14px; margin: 0; }}
    .panel-header .sub {{ color: var(--muted-2); font-size: 12px; margin-top: 3px; }}
    .panel-body {{ padding: 16px 18px; }}
    .place-row {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 11px 0; border-bottom: 1px solid var(--line); }}
    .place-row:first-child {{ padding-top: 0; }}
    .place-row:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .place-main {{ min-width: 0; display: grid; gap: 3px; }}
    .place-main strong {{ font-size: 13.5px; }}
    .place-main .sub {{ color: var(--muted-2); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    .place-actions {{ display: flex; gap: 8px; flex: 0 0 auto; }}
    .place-actions button {{ height: 32px; padding: 0 12px; font-size: 12.5px; }}
    .pill {{ display: inline-flex; align-items: center; gap: 6px; width: fit-content; min-height: 24px; padding: 0 10px; border-radius: 999px; background: var(--surface-2); color: var(--muted); border: 1px solid var(--line); font-size: 12px; font-weight: 750; }}
    .pill.ok {{ color: var(--ok); border-color: rgba(166, 227, 161, .4); background: rgba(166, 227, 161, .09); }}
    .pill.warn {{ color: var(--warn); border-color: rgba(249, 226, 175, .35); background: rgba(249, 226, 175, .08); }}
    .pill .dot {{ width: 6px; height: 6px; border-radius: 50%; background: currentColor; }}
    details.advanced {{ border: 1px solid var(--line); border-radius: var(--radius); background: rgba(24, 24, 37, .4); }}
    details.advanced > summary {{ list-style: none; cursor: pointer; padding: 14px 18px; color: var(--muted); font-size: 13px; font-weight: 750; display: flex; align-items: center; gap: 8px; }}
    details.advanced > summary::before {{ content: '▸'; color: var(--muted-2); }}
    details.advanced[open] > summary::before {{ content: '▾'; }}
    details.advanced > summary .hint {{ color: var(--muted-2); font-weight: 500; }}
    .advanced-body {{ border-top: 1px solid var(--line); padding: 18px; display: grid; gap: 22px; }}
    .advanced-block {{ display: grid; gap: 10px; }}
    .advanced-block > h3 {{ margin: 0; font-size: 13px; }}
    .advanced-block > .sub {{ color: var(--muted-2); font-size: 12.5px; line-height: 1.5; margin-top: -6px; }}
    .user-id-row {{ display: flex; gap: 8px; align-items: center; }}
    .user-id-row code {{ flex: 1 1 auto; min-width: 0; color: var(--text); font-size: 12px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 6px; background: rgba(17, 17, 27, .55); }}
    .user-id-row button {{ height: 30px; padding: 0 10px; font-size: 12px; flex: 0 0 auto; }}
    .metric-row {{ display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }}
    .org-row {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 0; border-bottom: 1px solid var(--line); }}
    .org-row:first-child {{ padding-top: 0; }}
    .org-row:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .org-main {{ min-width: 0; display: grid; gap: 3px; }}
    .org-main strong {{ font-size: 13.5px; }}
    .org-main .sub {{ color: var(--muted-2); font-size: 12px; }}
    .org-side {{ display: flex; gap: 8px; align-items: center; flex: 0 0 auto; }}
    .pill.err {{ color: var(--err); border-color: rgba(243, 139, 168, .4); background: rgba(243, 139, 168, .08); }}
    .audit {{ display: grid; }}
    .event {{ padding: 11px 0; border-bottom: 1px solid var(--line); font-size: 13px; }}
    .event:first-child {{ padding-top: 0; }}
    .event:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .event-line {{ display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }}
    .event-name {{ font-weight: 750; }}
    .event time {{ color: var(--muted); font-size: 12px; white-space: nowrap; }}
    .event code {{ display: inline-block; margin-top: 3px; font-size: 12px; }}
    .hidden {{ display: none !important; }}
    .handle {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-weight: 700; }}

    @media (max-width: 700px) {{
      /* The bar wraps instead of letting the squeezed title paint under
         the session chip: brand keeps the first row, actions take the
         next when space runs out. */
      .topbar {{ min-height: auto; padding: 12px 0; flex-wrap: wrap; row-gap: 8px; }}
      .brand h1 {{ font-size: 16px; white-space: nowrap; }}
      .brand-sub {{ display: none; }}
      .top-actions {{ margin-left: auto; flex-wrap: wrap; justify-content: flex-end; }}
      .feature-strip {{ grid-template-columns: 1fr; }}
      .hero-title {{ font-size: 26px; }}
      .auth-row {{ flex-direction: column; }}
      .place-row {{ flex-direction: column; align-items: stretch; }}
      .place-actions button {{ flex: 1 1 auto; }}
    }}
  </style>
</head>
<body class="signed-out">
  <header>
    <div class="topbar">
      <div class="brand">
        <img class="brand-mark" src="/logo.svg" alt="">
        <div>
        <h1>{product_title}</h1>
          <div class="brand-sub">{account_subtitle}</div>
        </div>
      </div>
      <div class="top-actions">
        <span id="session-chip" class="session-chip hidden"><span class="dot" aria-hidden="true"></span><span id="session-chip-handle"></span></span>
        <button id="refresh" class="ghost hidden">Refresh</button>
        <button id="logout" class="ghost hidden">Sign out</button>
      </div>
    </div>
  </header>
  <main class="shell">
    <!-- ── Signed out: landing ── -->
    <section id="auth">
      <div class="hero">
        <img class="hero-mark" src="/logo.svg" alt="">
        <h2 class="hero-title">Your computers, anywhere.</h2>
        <p class="hero-sub">Sign in with a passkey and open any machine you own, from any browser. This service only makes the introduction &mdash; each computer verifies you itself and decides what you may do, end to end.</p>
      </div>
      <div class="auth-card">
        <div>
          <label for="account">Account handle</label>
          <div class="auth-row">
            <input id="account" autocomplete="username webauthn" autocapitalize="none" spellcheck="false" placeholder="your-handle">
            <button id="login">Sign in</button>
          </div>
        </div>
        <div id="invite-row" class="hidden">
          <label for="invite-code">Invite code</label>
          <input id="invite-code" autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="registration is invite-only during the alpha">
        </div>
        <div id="invite-note" class="auth-note hidden">
          Intendant is in private pre-alpha &mdash; creating an account needs an
          invite right now. No code yet? Follow the project on
          <a href="{REPO_URL}" target="_blank" rel="noopener">GitHub</a>,
          or run your own rendezvous (below) &mdash; self-hosting is never gated.
        </div>
        <div id="auth-actions" class="auth-alt">
          <span>New here?</span>
          <button id="register" class="linklike">Create your account with a passkey</button>
        </div>
        <div id="auth-status" class="status" role="status"></div>
      </div>
      <ul class="feature-strip">
        <li><strong>Passkeys only</strong><span>No passwords. Your devices already sync the key.</span></li>
        <li><strong>Holds no power</strong><span>An introducer and relay. Your computers check your identity themselves &mdash; <a href="/trust">how trust works here</a>.</span></li>
        <li><strong>Self-hostable</strong><span>Run your own rendezvous &mdash; <a href="https://lovon-spec.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">read how</a>.</span></li>
      </ul>
    </section>

    <!-- ── Signed in: computers ── -->
    <section id="manage" class="hidden">
      <div class="section-head">
        <h2>Your computers</h2>
        <div id="who" class="sub"></div>
      </div>
      <div style="height: 12px"></div>
      <div class="computer-grid">
        <div id="computer-cards" style="display: contents"></div>
        <div class="computer-card add-card">
          <h3>Add a computer</h3>
          <ol class="steps">
            <li>On that machine, start <code>intendant</code> with Connect enabled &mdash; it prints a 12&#8209;word claim phrase in its log.</li>
            <li>Paste the phrase here to link it to this account.</li>
          </ol>
          <div>
            <label for="claim-code">Claim phrase</label>
            <input id="claim-code" autocomplete="off" spellcheck="false" placeholder="twelve words from the startup log">
          </div>
          <button id="claim">Connect it</button>
          <div id="claim-status" class="status" role="status"></div>
        </div>
      </div>
    </section>

    <!-- ── Signed in: saved places (only when any) ── -->
    <section id="fleet-section" class="panel hidden">
      <div class="panel-header">
        <div>
          <h2>Saved places</h2>
          <div class="sub">Routes this account remembers across your browsers; target daemons enforce local IAM</div>
        </div>
      </div>
      <div class="panel-body">
        <div id="fleet-rows"></div>
      </div>
    </section>

    <!-- ── Signed in: the power drawer ── -->
    <details id="advanced" class="advanced hidden">
      <summary>Advanced <span class="hint">&mdash; account identity, organizations, sync encryption, audit trail</span></summary>
      <div class="advanced-body">
        <div class="advanced-block" id="session-card">
          <h3>Account</h3>
          <div class="metric-row">
            <span class="pill"><span id="session-handle" class="handle"></span></span>
            <span id="session-passkeys" class="pill"></span>
            <span id="enc-pill" class="pill"></span>
          </div>
          <div class="sub">Give this user id to a daemon owner when they grant your account access under Access &rarr; People &amp; Devices.</div>
          <div class="user-id-row">
            <code id="session-user-id"></code>
            <button id="copy-user-id" class="ghost" type="button">Copy</button>
          </div>
        </div>
        <div class="advanced-block" id="orgs-block">
          <h3>Organizations</h3>
          <div class="sub">Signed membership documents this browser holds on this origin. They never touch this server &mdash; your browser presents them directly to daemons that trust the issuing org.</div>
          <div id="org-rows"></div>
        </div>
        <div class="advanced-block">
          <h3>What this account can and cannot do</h3>
          <div class="sub">It is rendezvous and navigation only &mdash; it grants nothing by itself. Every daemon decides access through its own local IAM, dashboard sessions verify a signature from the daemon itself, and private fields in Saved places sync end&#8209;to&#8209;end encrypted when your passkey supports PRF. <a href="/trust">The full story.</a></div>
        </div>
        <div class="advanced-block" id="identity-block">
          <h3>Verified identity</h3>
          <div class="sub">Optionally prove this handle is yours by publishing a claim you control. Verification is decoration &mdash; keys stay the identity &mdash; and every verified badge is committed to this service&rsquo;s public transparency log. Your claim line: <code id="attest-claim"></code></div>
          <div class="metric-row" id="attest-badges"></div>
          <div class="kv-row">
            <input id="attest-domain" autocomplete="off" spellcheck="false" placeholder="example.com &mdash; needs TXT at _intendant.example.com">
            <button id="attest-dns-btn" class="ghost">Verify domain</button>
          </div>
          <div class="kv-row">
            <input id="attest-gist" autocomplete="off" spellcheck="false" placeholder="https://gist.githubusercontent.com/&lt;you&gt;/&hellip;/raw &mdash; containing the claim line">
            <button id="attest-github-btn" class="ghost">Verify GitHub</button>
          </div>
          <div id="attest-status" class="sub"></div>
        </div>
        <div class="advanced-block" id="log-block">
          <h3>Transparency log</h3>
          <div class="sub">Every name binding this service hands out (which key a computer had when claimed, handle creations, revocation lists, badges) is committed to an append-only log. Your browser pins the signed tree head and re-verifies consistency on every visit &mdash; rewriting history here is detectable, not just forbidden.</div>
          <div class="metric-row"><span id="log-pill" class="pill">checking&hellip;</span><button id="log-reset-trust" class="ghost hidden" title="Discard the pinned tree head and trust the log's current signing key from now on. Only do this if you expected the operator to rotate the key.">Reset trust</button></div>
        </div>
        <div class="advanced-block" id="push-block">
          <h3>Notifications</h3>
          <div class="sub">Get a notification on this browser when one of your computers goes offline or comes back. Alerts are composed from presence the rendezvous already sees, and delivered encrypted to this browser alone.</div>
          <div class="metric-row">
            <span id="push-status" class="pill">checking&hellip;</span>
            <button id="push-enable" class="secondary hidden">Enable on this browser</button>
            <button id="push-disable" class="ghost hidden">Disable</button>
            <button id="push-test" class="ghost hidden">Send a test</button>
          </div>
        </div>
        <div class="advanced-block" id="audit-section">
          <h3>Audit</h3>
          <div class="sub">Recent account activity on this rendezvous.</div>
          <div id="audit" class="audit"></div>
        </div>
        <div class="advanced-block">
          <h3>Self-host</h3>
          <div class="sub">This origin (<code>{origin}</code>) is one instance of an open component. <a href="https://lovon-spec.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">Run your own</a> and point your daemons at it.</div>
        </div>
      </div>
    </details>
  </main>
<script>
const $ = id => document.getElementById(id);
const state = {{ user: null, daemons: [], fleetTargets: [], csrfToken: '' }};
function setStatus(id, text, kind = '') {{
  const el = $(id);
  el.textContent = text || '';
  el.className = 'status' + (kind ? ' status-' + kind : '');
}}

function setBusy(id, busy) {{
  const el = $(id);
  if (!el) return;
  el.disabled = Boolean(busy);
}}

async function api(path, options = {{}}) {{
  const headers = {{
    'content-type': 'application/json',
    ...(options.headers || {{}}),
  }};
  if (state.csrfToken && !headers['x-intendant-csrf']) {{
    headers['x-intendant-csrf'] = state.csrfToken;
  }}
  const resp = await fetch(path, {{
    ...options,
    headers,
  }});
  const body = await resp.json().catch(() => ({{}}));
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${{resp.status}}`);
  return body;
}}

function b64uToBuf(value) {{
  const text = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
  const padded = text.padEnd(Math.ceil(text.length / 4) * 4, '=');
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out.buffer;
}}

function bufToB64u(value) {{
  const bytes = new Uint8Array(value || new ArrayBuffer(0));
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}}

function publicKeyOptions(start) {{
  const options = start.options && (start.options.publicKey || start.options);
  if (!options) throw new Error('missing WebAuthn options');
  options.challenge = b64uToBuf(options.challenge);
  if (options.user?.id) options.user.id = b64uToBuf(options.user.id);
  for (const cred of options.excludeCredentials || []) cred.id = b64uToBuf(cred.id);
  for (const cred of options.allowCredentials || []) cred.id = b64uToBuf(cred.id);
  return options;
}}

function registrationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    attestationObject: bufToB64u(credential.response.attestationObject),
    transports: credential.response.getTransports ? credential.response.getTransports() : [],
  }};
}}

function authenticationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    authenticatorData: bufToB64u(credential.response.authenticatorData),
    signature: bufToB64u(credential.response.signature),
    userHandle: credential.response.userHandle ? bufToB64u(credential.response.userHandle) : null,
  }};
}}

// Fleet-sync encryption (trust architecture phase 5 follow-on): evaluate
// the WebAuthn PRF extension during the passkey ceremony and stash the
// per-tab secrets; /app derives AES keys from them so private fleet fields
// and the credential vault sync end-to-end encrypted. Two salts, one
// gesture: `first` feeds fleet-sync, `second` feeds the vault — separate
// PRF domains, so the two features never share key material. The server
// never sees either output.
const FLEET_PRF_SALT = new TextEncoder().encode('intendant-fleet-sync-v1');
const VAULT_PRF_SALT = new TextEncoder().encode('intendant-vault-v1');

function prfExtensions() {{
  return {{ prf: {{ eval: {{ first: FLEET_PRF_SALT, second: VAULT_PRF_SALT }} }} }};
}}

function stashPrfSecret(credential) {{
  try {{
    const results = credential.getClientExtensionResults?.();
    const toB64u = buf => {{
      const bytes = new Uint8Array(buf);
      let bin = '';
      for (const b of bytes) bin += String.fromCharCode(b);
      return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }};
    const first = results?.prf?.results?.first;
    if (!first) return;
    sessionStorage.setItem('intendant_fleet_prf_v1', toB64u(first));
    // Older authenticators may evaluate only one salt; the vault then
    // falls back to its legacy fleet-secret derivation client-side.
    const second = results?.prf?.results?.second;
    if (second) sessionStorage.setItem('intendant_vault_prf_v1', toB64u(second));
  }} catch (err) {{
    console.warn('PRF secret unavailable:', err?.message || err);
  }}
}}

async function createPasskey() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('register', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/register/start', {{
      method: 'POST',
      body: JSON.stringify({{
        account_name: account,
        invite_code: ($('invite-code')?.value || '').trim(),
      }}),
    }});
    const credential = await navigator.credentials.create({{ publicKey: {{ ...publicKeyOptions(start), extensions: prfExtensions() }} }});
    stashPrfSecret(credential);
    const done = await api('/api/auth/register/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: registrationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('register', false);
  }}
}}

async function login() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('login', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/login/start', {{
      method: 'POST',
      body: JSON.stringify({{ account_name: account }}),
    }});
    const credential = await navigator.credentials.get({{ publicKey: {{ ...publicKeyOptions(start), extensions: prfExtensions() }} }});
    stashPrfSecret(credential);
    const done = await api('/api/auth/login/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: authenticationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('login', false);
  }}
}}

async function claimDaemon() {{
  const claimCode = $('claim-code').value.trim();
  if (!claimCode) throw new Error('Claim phrase is required');
  setBusy('claim', true);
  setStatus('claim-status', 'Waiting for daemon proof', '');
  try {{
    const start = await api('/api/claims/claim', {{
      method: 'POST',
      body: JSON.stringify({{ claim_code: claimCode }}),
    }});
    const deadline = Date.now() + 65000;
    while (Date.now() < deadline) {{
      await new Promise(resolve => setTimeout(resolve, 750));
      const status = await api(`/api/claims/${{encodeURIComponent(start.claim_id)}}`);
      if (status.result?.status === 'approved') {{
        setStatus('claim-status', `Rendezvous route claimed for ${{status.result.daemon_id}}. Next: open that daemon directly (its https://host:8765 address) as root, go to Access → People & Devices, and grant this account a role — until then the daemon will refuse hosted dashboard control.`, 'ok');
        $('claim-code').value = '';
        await refreshAll();
        return;
      }}
      if (status.result?.status === 'rejected') {{
        throw new Error(status.result.error || 'claim rejected');
      }}
    }}
    throw new Error('claim timed out');
  }} finally {{
    setBusy('claim', false);
  }}
}}

/* Read (never create) this origin's browser identity key fingerprint so
   stored org documents can be badged as bound to this browser or not. */
async function ownIdentityFingerprint() {{
  try {{
    if (!window.indexedDB || !crypto?.subtle) return '';
    const db = await new Promise((resolve, reject) => {{
      const req = indexedDB.open('intendant-client-identity', 1);
      req.onupgradeneeded = () => {{
        if (!req.result.objectStoreNames.contains('keys')) req.result.createObjectStore('keys');
      }};
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    }});
    const record = await new Promise((resolve, reject) => {{
      const tx = db.transaction('keys', 'readonly');
      const req = tx.objectStore('keys').get('v1');
      req.onsuccess = () => resolve(req.result || null);
      req.onerror = () => reject(req.error);
    }});
    db.close();
    if (!record?.publicRaw) return '';
    const digest = await crypto.subtle.digest('SHA-256', record.publicRaw);
    return bufToB64u(digest);
  }} catch {{ return ''; }}
}}

async function renderOrgs() {{
  const rows = $('org-rows');
  rows.innerHTML = '';
  let map = {{}};
  try {{ map = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{{}}') || {{}}; }} catch {{}}
  const docs = Object.values(map).filter(doc => doc && typeof doc === 'object' && doc.org?.handle);
  if (!docs.length) {{
    rows.innerHTML = '<div class="empty-hint">None stored in this browser. Daemon dashboards keep a membership document here when you join with one; it is then presented automatically on every connection.</div>';
    return;
  }}
  const ownFp = await ownIdentityFingerprint();
  const now = Date.now();
  for (const doc of docs) {{
    const expires = Number(doc.expires_at_unix_ms || 0);
    const daysLeft = Math.floor((expires - now) / 86400000);
    const expired = expires <= now;
    const role = String(doc.role_id || '').replace(/^role:/, '').replace(/^peer:/, 'daemon: ');
    const subjectFp = String(doc.subject?.peer_fingerprint || doc.subject?.client_key_fingerprint || '');
    const mine = ownFp && subjectFp === ownFp;
    const expiryText = expired
      ? 'expired — ask the org for a renewed document'
      : daysLeft < 1 ? 'expires today'
      : `expires in ${{daysLeft}} day${{daysLeft === 1 ? '' : 's'}}`;
    const row = document.createElement('div');
    row.className = 'org-row';
    row.innerHTML = `
      <div class="org-main">
        <strong>@${{escapeHtml(String(doc.org.handle))}}</strong>
        <span class="sub">${{escapeHtml(role)}} &middot; ${{mine ? 'bound to this browser' : 'bound to ' + escapeHtml(shortId(subjectFp))}} &middot; ${{escapeHtml(expiryText)}}</span>
      </div>
      <div class="org-side">
        <span class="pill ${{expired ? 'err' : (daysLeft < 5 ? 'warn' : 'ok')}}">${{expired ? 'expired' : 'active'}}</span>
        <button class="ghost" data-org-remove="${{escapeAttr(String(doc.org.handle))}}">Remove</button>
      </div>`;
    rows.appendChild(row);
  }}
  rows.querySelectorAll('[data-org-remove]').forEach(button => {{
    button.addEventListener('click', () => {{
      const handle = button.getAttribute('data-org-remove');
      if (!confirm(`Remove the stored @${{handle}} document from this browser? Access already granted on daemons is unaffected; automatic presentation stops.`)) return;
      try {{
        const current = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{{}}') || {{}};
        delete current[handle];
        localStorage.setItem('intendant_org_grants_v1', JSON.stringify(current));
      }} catch {{}}
      renderOrgs();
    }});
  }});
}}

/* ── Transparency log client: RFC 9162 verification in WebCrypto ── */
const LOG_STH_KEY = 'intendant_log_sth_v1';

async function logSha(bytes) {{
  return new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
}}

async function logNodeHash(left, right) {{
  const buf = new Uint8Array(1 + left.length + right.length);
  buf[0] = 0x01; buf.set(left, 1); buf.set(right, 1 + left.length);
  return logSha(buf);
}}

function bytesEqual(a, b) {{
  if (!a || !b || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a[i] ^ b[i];
  return diff === 0;
}}

async function logVerifyConsistency(oldSize, newSize, oldRoot, newRoot, proof) {{
  if (oldSize === newSize) return bytesEqual(oldRoot, newRoot) && proof.length === 0;
  if (oldSize === 0 || oldSize > newSize) return false;
  const complete = (oldSize & (oldSize - 1)) === 0;
  let i = 0;
  const first = complete ? oldRoot : proof[i++];
  if (!first) return false;
  let fn = oldSize - 1, sn = newSize - 1;
  while (fn % 2 === 1) {{ fn = Math.floor(fn / 2); sn = Math.floor(sn / 2); }}
  let fr = first, sr = first;
  for (; i < proof.length; i += 1) {{
    if (sn === 0) return false;
    const p = proof[i];
    if (fn % 2 === 1 || fn === sn) {{
      fr = await logNodeHash(p, fr);
      sr = await logNodeHash(p, sr);
      if (fn % 2 === 0) while (fn % 2 === 0 && fn !== 0) {{ fn = Math.floor(fn / 2); sn = Math.floor(sn / 2); }}
    }} else {{
      sr = await logNodeHash(sr, p);
    }}
    fn = Math.floor(fn / 2); sn = Math.floor(sn / 2);
  }}
  return bytesEqual(fr, oldRoot) && bytesEqual(sr, newRoot) && sn === 0;
}}

async function logVerifySthSignature(sth) {{
  try {{
    const key = await crypto.subtle.importKey(
      'raw', b64uToBuf(sth.public_key),
      {{ name: 'ECDSA', namedCurve: 'P-256' }}, false, ['verify']);
    const payload = new TextEncoder().encode(
      `intendant-log-sth-v1\n${{sth.size}}\n${{sth.root}}\n${{sth.unix_ms}}`);
    return await crypto.subtle.verify(
      {{ name: 'ECDSA', hash: 'SHA-256' }}, key, b64uToBuf(sth.signature), payload);
  }} catch {{
    return false;
  }}
}}

/* Pin the signed tree head; on every visit verify the log only ever
   appended since last time. A failed check is loud and sticky — including a
   changed log signing key, which would otherwise let the service swap in a
   fresh log and dodge the consistency proof entirely (trust-on-every-use).
   Recovering from a legitimate key rotation is an explicit user action. */
async function transparencyCheck() {{
  const pill = $('log-pill');
  const resetBtn = $('log-reset-trust');
  if (resetBtn) resetBtn.classList.add('hidden');
  try {{
    const sth = await api('/api/log/sth');
    if (!(await logVerifySthSignature(sth))) throw new Error('tree head signature invalid');
    let pinned = null;
    try {{ pinned = JSON.parse(localStorage.getItem(LOG_STH_KEY) || 'null'); }} catch {{}}
    if (pinned && pinned.size > 0) {{
      if (pinned.public_key !== sth.public_key) {{
        if (resetBtn) resetBtn.classList.remove('hidden');
        throw new Error('log signing key changed — history can no longer be verified against your pin');
      }}
      if (sth.size < pinned.size) throw new Error('log shrank — history was rewritten');
      const proof = await api(`/api/log/consistency?old=${{pinned.size}}&new=${{sth.size}}`);
      const asBytes = value => new Uint8Array(b64uToBuf(value));
      const consistent = await logVerifyConsistency(
        pinned.size, sth.size,
        asBytes(pinned.root), asBytes(sth.root),
        (proof.proof || []).map(asBytes));
      if (!consistent) throw new Error('consistency proof failed — history was rewritten');
    }}
    localStorage.setItem(LOG_STH_KEY, JSON.stringify({{
      size: sth.size, root: sth.root, public_key: sth.public_key,
      pinned_unix_ms: pinned?.pinned_unix_ms || Date.now(),
    }}));
    if (pill) {{
      const since = pinned?.pinned_unix_ms ? new Date(pinned.pinned_unix_ms).toLocaleDateString() : 'today';
      pill.textContent = `${{sth.size}} entries · consistent since ${{since}}`;
      pill.className = 'pill ok';
    }}
  }} catch (err) {{
    console.warn('[transparency] check failed:', err);
    if (pill) {{
      pill.textContent = 'VERIFICATION FAILED: ' + err.message;
      pill.className = 'pill err';
    }}
  }}
}}

function renderAttestations() {{
  const claim = $('attest-claim');
  const badges = $('attest-badges');
  if (!claim || !badges || !state.user) return;
  claim.textContent = `intendant-handle=${{state.user.account_name}}@${{location.host}}`;
  const list = state.user.attestations || [];
  badges.innerHTML = list.length
    ? list.map(a => `<span class="pill ok" title="verified ${{new Date(a.verified_unix_ms).toLocaleDateString()}}">&#10003; ${{escapeHtml(a.kind === 'dns' ? a.subject : a.subject.replace('github:', 'github.com/'))}}</span>`).join('')
    : '<span class="sub">no verifications yet</span>';
}}

async function pushSubscriptionState() {{
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) return {{ supported: false }};
  const registration = await navigator.serviceWorker.getRegistration('/');
  const subscription = registration ? await registration.pushManager.getSubscription() : null;
  return {{ supported: true, subscription }};
}}

async function renderPushBlock() {{
  const status = $('push-status');
  if (!status) return;
  const stateNow = await pushSubscriptionState().catch(() => ({{ supported: false }}));
  const enableBtn = $('push-enable');
  const disableBtn = $('push-disable');
  const testBtn = $('push-test');
  if (!stateNow.supported) {{
    status.textContent = 'not supported in this browser';
    status.className = 'pill';
    enableBtn.classList.add('hidden');
    disableBtn.classList.add('hidden');
    testBtn.classList.add('hidden');
    return;
  }}
  const on = Boolean(stateNow.subscription);
  status.textContent = on ? 'on for this browser' : 'off';
  status.className = 'pill' + (on ? ' ok' : '');
  enableBtn.classList.toggle('hidden', on);
  disableBtn.classList.toggle('hidden', !on);
  testBtn.classList.toggle('hidden', !on);
}}

async function enablePushNotifications() {{
  const permission = await Notification.requestPermission();
  if (permission !== 'granted') throw new Error('notification permission was not granted');
  const {{ public_key }} = await api('/api/push/vapid-public-key');
  const registration = await navigator.serviceWorker.register('/sw.js', {{ scope: '/' }});
  await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({{
    userVisibleOnly: true,
    applicationServerKey: b64uToBuf(public_key),
  }});
  const raw = subscription.toJSON();
  await api('/api/push/subscribe', {{
    method: 'POST',
    body: JSON.stringify({{
      endpoint: raw.endpoint,
      p256dh: raw.keys?.p256dh || '',
      auth: raw.keys?.auth || '',
      label: navigator.userAgent.slice(0, 100),
    }}),
  }});
}}

async function disablePushNotifications() {{
  const stateNow = await pushSubscriptionState();
  const endpoint = stateNow.subscription?.endpoint || '';
  if (stateNow.subscription) await stateNow.subscription.unsubscribe().catch(() => {{}});
  await api('/api/push/unsubscribe', {{ method: 'POST', body: JSON.stringify({{ endpoint }}) }});
}}

let fleetAesKey = null;
async function fleetEncryptionKey() {{
  if (fleetAesKey) return fleetAesKey;
  try {{
    const prf = sessionStorage.getItem('intendant_fleet_prf_v1') || '';
    if (!prf || !crypto?.subtle) return null;
    const hkdf = await crypto.subtle.importKey('raw', b64uToBuf(prf), 'HKDF', false, ['deriveKey']);
    fleetAesKey = await crypto.subtle.deriveKey(
      {{ name: 'HKDF', hash: 'SHA-256', salt: new TextEncoder().encode('intendant-fleet-sync-v1'), info: new TextEncoder().encode('fleet-enc') }},
      hkdf, {{ name: 'AES-GCM', length: 256 }}, false, ['decrypt']
    );
    return fleetAesKey;
  }} catch {{ return null; }}
}}

async function decryptFleetTarget(target) {{
  const enc = String(target?.enc_fields || '');
  if (!enc.startsWith('enc1:')) return target;
  const key = await fleetEncryptionKey();
  if (!key) return {{ ...target, fleet_locked: true }};
  try {{
    const [iv, ct] = enc.slice(5).split(':');
    const plain = await crypto.subtle.decrypt({{ name: 'AES-GCM', iv: b64uToBuf(iv) }}, key, b64uToBuf(ct));
    const secret = JSON.parse(new TextDecoder().decode(plain));
    return {{ ...target, url: String(secret.url || ''), ws_url: String(secret.ws_url || ''), browser_tcp_via_url: String(secret.browser_tcp_via_url || ''), fleet_locked: false }};
  }} catch {{ return {{ ...target, fleet_locked: true }}; }}
}}

async function refreshAll() {{
  setBusy('refresh', true);
  try {{
    const me = await api('/api/me');
    state.csrfToken = me.csrf_token || '';
    state.user = me.authenticated ? me.user : null;
    state.inviteRequired = me.invite_required === true;
    renderAuth();
    if (!state.user) return;
    const [daemons, fleet, audit] = await Promise.all([
      api('/api/daemons'),
      api('/api/fleet/targets'),
      api('/api/audit'),
    ]);
    state.daemons = daemons.daemons || [];
    state.fleetTargets = await Promise.all((fleet.targets || []).map(decryptFleetTarget));
    renderOrgs().catch(() => {{}});
    renderDaemons();
    renderFleetTargets();
    renderAudit(audit.events || []);
  }} finally {{
    setBusy('refresh', false);
  }}
}}

function renderAuth() {{
  const authed = Boolean(state.user);
  $('invite-row').classList.toggle('hidden', authed || !state.inviteRequired);
  $('invite-note').classList.toggle('hidden', authed || !state.inviteRequired);
  document.body.classList.toggle('signed-out', !authed);
  document.body.classList.toggle('signed-in', authed);
  $('manage').classList.toggle('hidden', !authed);
  $('advanced').classList.toggle('hidden', !authed);
  $('logout').classList.toggle('hidden', !authed);
  $('refresh').classList.toggle('hidden', !authed);
  $('session-chip').classList.toggle('hidden', !authed);
  $('auth-actions').classList.toggle('hidden', authed);
  $('account').disabled = authed;
  if (!authed) $('fleet-section').classList.add('hidden');
  if (authed) renderPushBlock().catch(() => {{}});
  if (authed) renderAttestations();
  if (authed) {{
    $('account').value = state.user.account_name || '';
    $('session-chip-handle').textContent = '@' + state.user.account_name;
    $('session-handle').textContent = '@' + state.user.account_name;
    $('session-passkeys').textContent = `${{state.user.passkey_count}} passkey${{state.user.passkey_count === 1 ? '' : 's'}}`;
    $('session-user-id').textContent = state.user.id || '';
    $('who').textContent = '@' + state.user.account_name;
    const encOn = Boolean(sessionStorage.getItem('intendant_fleet_prf_v1'));
    const enc = $('enc-pill');
    enc.textContent = encOn ? 'sync encryption: on' : 'sync encryption: off';
    enc.className = 'pill' + (encOn ? ' ok' : '');
    enc.title = encOn
      ? 'Private fields in Saved places are end-to-end encrypted with a key derived from your passkey (WebAuthn PRF). This service stores only ciphertext.'
      : 'Your passkey or browser did not offer the WebAuthn PRF extension this session, so Saved places sync public fields only.';
  }} else {{
    $('session-chip-handle').textContent = '';
    $('session-handle').textContent = '';
    $('session-passkeys').textContent = '';
    $('session-user-id').textContent = '';
    $('who').textContent = '';
  }}
}}

function renderDaemons() {{
  const grid = $('computer-cards');
  grid.innerHTML = '';
  grid.parentElement.classList.toggle('empty', state.daemons.length === 0);
  $('who').textContent = state.daemons.length
    ? `${{state.daemons.length}} linked to @${{state.user?.account_name || ''}}`
    : '';
  for (const daemon of state.daemons) {{
    const key = String(daemon.daemon_public_key || '');
    const daemonId = String(daemon.daemon_id || '');
    const hasLabel = Boolean(String(daemon.label || '').trim());
    const label = hasLabel ? String(daemon.label) : shortId(daemonId);
    const lastSeen = formatRelative(daemon.last_seen_unix_ms);
    const card = document.createElement('div');
    card.className = 'computer-card';
    card.innerHTML = `
      <div class="computer-head">
        <span class="computer-dot ${{daemon.online ? 'ok' : ''}}" aria-hidden="true"></span>
        <div class="computer-name">
          <strong title="${{escapeAttr(hasLabel ? label : daemonId)}}">${{escapeHtml(label)}}</strong>
          <span class="sub">${{daemon.online ? 'online now' : 'last seen ' + escapeHtml(lastSeen)}}</span>
        </div>
      </div>
      <div class="computer-actions">
        <button class="open" data-open="${{escapeAttr(daemonId)}}">Open</button>
        <button class="secondary" data-rename="${{escapeAttr(daemonId)}}">Rename</button>
      </div>
      ${{presenceSparkline(daemon)}}
      <details>
        <summary>Details</summary>
        <div class="kv">
          <div><div class="k">Daemon id</div><code>${{escapeHtml(daemonId)}}</code></div>
          <div><div class="k">Public key &mdash; sessions verify this end to end</div><code>${{escapeHtml(key)}}</code></div>
          <div class="danger-row"><button class="danger" data-revoke="${{escapeAttr(daemonId)}}">Disconnect from this account</button></div>
        </div>
      </details>`;
    grid.appendChild(card);
  }}
  grid.querySelectorAll('[data-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const id = button.getAttribute('data-open');
      window.location.href = `/app?connect=1&daemon_id=${{encodeURIComponent(id)}}`;
    }});
  }});
  grid.querySelectorAll('[data-revoke]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-revoke');
      if (!confirm(`Disconnect ${{id}} from this account? The computer itself is untouched; it just stops being reachable through here until claimed again.`)) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/revoke`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
  grid.querySelectorAll('[data-rename]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-rename');
      const daemon = state.daemons.find(item => item.daemon_id === id) || {{}};
      const next = prompt('Name this computer', daemon.label || daemon.daemon_id || '');
      if (next === null) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/label`, {{
        method: 'POST',
        body: JSON.stringify({{ label: next }}),
      }});
      await refreshAll();
    }});
  }});
}}

function renderFleetTargets() {{
  const rows = $('fleet-rows');
  rows.innerHTML = '';
  const claimedIds = new Set(state.daemons.map(d => String(d.daemon_id || '')));
  const places = state.fleetTargets.filter(target => {{
    const cid = String(target.connect_daemon_id || '');
    return !(target.claimed_daemon === true && cid && claimedIds.has(cid));
  }});
  $('fleet-section').classList.toggle('hidden', !state.user || places.length === 0);
  for (const target of places) {{
    const id = String(target.host_id || target.id || '');
    const rawLabel = String(target.label || '').trim();
    const label = (!rawLabel || rawLabel === id) ? (shortId(id) || 'Place') : rawLabel;
    const locked = target.fleet_locked === true;
    const route = locked
      ? 'End-to-end encrypted — opens on a device signed in with your passkey'
      : String(target.route_label || target.route || target.url || 'Remembered route');
    const online = target.online || target.connected;
    const url = String(target.url || '');
    const canForget = target.claimed_daemon !== true;
    const row = document.createElement('div');
    row.className = 'place-row';
    row.innerHTML = `
      <div class="place-main">
        <strong>${{escapeHtml(label)}}</strong>
        <span class="sub" title="${{escapeAttr(route)}}">${{escapeHtml(route)}}</span>
      </div>
      <span class="pill ${{online ? 'ok' : ''}}">${{online ? 'online' : (locked ? 'locked' : 'remembered')}}</span>
      <div class="place-actions">
        <button data-fleet-open="${{escapeAttr(url)}}" ${{url ? '' : 'disabled'}}>Open</button>
        <button class="ghost" data-fleet-forget="${{escapeAttr(id)}}" ${{canForget ? '' : 'disabled'}}>Forget</button>
      </div>`;
    rows.appendChild(row);
  }}
  rows.querySelectorAll('[data-fleet-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const url = button.getAttribute('data-fleet-open');
      if (url) window.location.href = url;
    }});
  }});
  rows.querySelectorAll('[data-fleet-forget]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-fleet-forget');
      if (!id) return;
      await api(`/api/fleet/targets/${{encodeURIComponent(id)}}/forget`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
}}

function renderAudit(events) {{
  const el = $('audit');
  el.innerHTML = '';
  if (!events.length) {{
    el.innerHTML = '<div class="empty-hint">No account activity yet.</div>';
    return;
  }}
  for (const event of events.slice(0, 30)) {{
    const div = document.createElement('div');
    div.className = 'event';
    const date = formatDate(event.unix_ms);
    const name = String(event.event || '').replaceAll('_', ' ');
    div.innerHTML = `<div class="event-line"><span class="event-name">${{escapeHtml(name)}}</span><time>${{escapeHtml(date)}}</time></div><code>${{escapeHtml(event.daemon_id || '')}}</code>`;
    el.appendChild(div);
  }}
}}

/* Last 72 hours as tiny bars (present = the daemon polled that hour),
   plus a 7-day availability figure. Display of data the rendezvous
   already has from the polling it exists to do. */
function presenceSparkline(daemon) {{
  const hours = Array.isArray(daemon.presence_hours) ? daemon.presence_hours : [];
  if (!hours.length) return '';
  const seen = new Set(hours.map(Number));
  const nowHour = Math.floor(Date.now() / 3600000);
  const span = 72;
  let bars = '';
  for (let i = span - 1; i >= 0; i -= 1) {{
    const hour = nowHour - i;
    const on = seen.has(hour);
    const when = new Date(hour * 3600000);
    bars += `<span class="${{on ? 'on' : ''}}" title="${{escapeAttr(when.toLocaleString([], {{ weekday: 'short', hour: 'numeric' }}))}} — ${{on ? 'online' : 'offline'}}"></span>`;
  }}
  let weekSeen = 0;
  for (let i = 0; i < 168; i += 1) if (seen.has(nowHour - i)) weekSeen += 1;
  const tracked = Math.min(168, Math.max(1, nowHour - Math.min(...seen) + 1));
  const pct = Math.round((weekSeen / Math.min(168, tracked)) * 100);
  return `<div class="presence"><div class="presence-bars" aria-hidden="true">${{bars}}</div><div class="presence-label">last 3 days &middot; up ${{pct}}% of the ${{tracked >= 168 ? 'week' : 'time tracked'}}</div></div>`;
}}

function compactKey(value) {{
  const key = String(value || '');
  if (key.length <= 24) return key;
  return key.slice(0, 12) + '...' + key.slice(-8);
}}

function shortId(value) {{
  const id = String(value || '');
  if (id.length > 24 && !id.includes('.')) return id.slice(0, 8) + '…' + id.slice(-4);
  return id;
}}

function formatDate(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'unknown';
  return new Date(value).toLocaleString();
}}

function formatRelative(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'never';
  const seconds = Math.max(0, Math.floor((Date.now() - value) / 1000));
  if (seconds < 10) return 'just now';
  if (seconds < 60) return `${{seconds}}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${{minutes}}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${{hours}}h ago`;
  return `${{Math.floor(hours / 24)}}d ago`;
}}

function escapeHtml(value) {{
  return String(value ?? '').replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
}}
function escapeAttr(value) {{ return escapeHtml(value); }}

$('attest-dns-btn').addEventListener('click', async () => {{
  const domain = $('attest-domain').value.trim();
  if (!domain) return;
  setStatus('attest-status', 'checking TXT record\u2026', '');
  try {{
    const r = await api('/api/attest/dns', {{ method: 'POST', body: JSON.stringify({{ domain }}) }});
    setStatus('attest-status', `verified ${{r.subject}}`, 'ok');
    await refreshAll();
  }} catch (err) {{ setStatus('attest-status', err.message, 'err'); }}
}});
$('attest-github-btn').addEventListener('click', async () => {{
  const gist_raw_url = $('attest-gist').value.trim();
  if (!gist_raw_url) return;
  setStatus('attest-status', 'fetching gist\u2026', '');
  try {{
    const r = await api('/api/attest/github', {{ method: 'POST', body: JSON.stringify({{ gist_raw_url }}) }});
    setStatus('attest-status', `verified ${{r.subject}}`, 'ok');
    await refreshAll();
  }} catch (err) {{ setStatus('attest-status', err.message, 'err'); }}
}});
transparencyCheck();
$('log-reset-trust').addEventListener('click', () => {{
  localStorage.removeItem(LOG_STH_KEY);
  transparencyCheck();
}});
$('push-enable').addEventListener('click', () => enablePushNotifications().then(renderPushBlock).catch(err => alert('Notifications: ' + err.message)));
$('push-disable').addEventListener('click', () => disablePushNotifications().then(renderPushBlock).catch(() => renderPushBlock()));
$('push-test').addEventListener('click', async () => {{
  try {{ await api('/api/push/test', {{ method: 'POST', body: '{{}}' }}); }} catch (err) {{ alert('Test failed: ' + err.message); }}
}});
$('register').addEventListener('click', () => createPasskey().catch(err => setStatus('auth-status', err.message, 'err')));
$('login').addEventListener('click', () => login().catch(err => setStatus('auth-status', err.message, 'err')));
$('claim').addEventListener('click', () => claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')));
$('refresh').addEventListener('click', () => refreshAll().catch(err => setStatus('claim-status', err.message, 'err')));
$('logout').addEventListener('click', async () => {{ await api('/api/logout', {{ method: 'POST', body: '{{}}' }}); state.user = null; state.csrfToken = ''; renderAuth(); }});
$('copy-user-id').addEventListener('click', async () => {{
  const id = state.user && state.user.id ? String(state.user.id) : '';
  if (!id) return;
  try {{
    await navigator.clipboard.writeText(id);
    const btn = $('copy-user-id');
    btn.textContent = 'Copied';
    setTimeout(() => {{ btn.textContent = 'Copy'; }}, 1200);
  }} catch (err) {{
    setStatus('auth-status', 'Copy failed: ' + ((err && err.message) || err), 'err');
  }}
}});
$('account').addEventListener('keydown', event => {{ if (event.key === 'Enter') login().catch(err => setStatus('auth-status', err.message, 'err')); }});
$('claim-code').addEventListener('keydown', event => {{ if (event.key === 'Enter') claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')); }});

const params = new URLSearchParams(location.search);
if (params.get('claim_code')) $('claim-code').value = params.get('claim_code');
// Shareable invites: /connect?invite=CODE prefills the invite field.
if (params.get('invite')) $('invite-code').value = params.get('invite');
refreshAll().catch(() => renderAuth());
</script>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bip39::Language;

    #[test]
    fn fleet_sync_round_trips_record_signatures() {
        let user_id = Uuid::new_v4();
        let record = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: "daemon-1".to_string(),
                host_id: "daemon-1".to_string(),
                label: "Anchor box".to_string(),
                record_key: "PubKeyB64u".to_string(),
                record_sig: "SigB64u".to_string(),
                record_signed_at_unix_ms: 1_700_000_000_000,
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .expect("record normalizes");
        // The service carries owner signatures verbatim — it never
        // interprets them, and the view exposes them for client-side
        // verification.
        assert_eq!(record.record_key, "PubKeyB64u");
        assert_eq!(record.record_sig, "SigB64u");
        assert_eq!(record.record_signed_at_unix_ms, 1_700_000_000_000);
        let view = fleet_target_view(&record);
        assert_eq!(view["record_key"], "PubKeyB64u");
        assert_eq!(view["record_sig"], "SigB64u");
        assert_eq!(view["record_signed_at_unix_ms"], 1_700_000_000_000u64);

        // Future timestamps clamp to the sync time instead of trusting the
        // client clock.
        let clamped = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: "daemon-2".to_string(),
                record_signed_at_unix_ms: u64::MAX,
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .expect("record normalizes");
        assert_eq!(clamped.record_signed_at_unix_ms, 1_800_000_000_000);
    }

    #[test]
    fn canonicalize_drops_signature_only_when_it_rewrites_the_record() {
        let signed = |id: &str, host_id: &str| FleetTargetRecord {
            id: id.to_string(),
            host_id: host_id.to_string(),
            connect_daemon_id: Some("daemon-1".to_string()),
            record_key: "PubKeyB64u".to_string(),
            record_sig: "SigB64u".to_string(),
            record_signed_at_unix_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let owned: HashSet<String> = ["daemon-1".to_string()].into_iter().collect();

        // Not an owned daemon: untouched, signature intact.
        let mut foreign = signed("alias", "alias");
        canonicalize_fleet_target_for_owned_daemon(&mut foreign, &HashSet::new());
        assert_eq!(foreign.host_id, "alias");
        assert_eq!(foreign.record_sig, "SigB64u");

        // Already canonical: nothing changes, so the signature still holds.
        let mut canonical = signed("daemon-1", "daemon-1");
        canonicalize_fleet_target_for_owned_daemon(&mut canonical, &owned);
        assert_eq!(canonical.host_id, "daemon-1");
        assert_eq!(canonical.record_key, "PubKeyB64u");
        assert_eq!(canonical.record_sig, "SigB64u");
        assert_eq!(canonical.record_signed_at_unix_ms, 1_700_000_000_000);

        // Alias of an owned daemon: host_id is rewritten, which makes the
        // owner signature (it covers host_id) permanently unverifiable —
        // it must be dropped, not stored broken.
        let mut alias = signed("alias", "alias");
        canonicalize_fleet_target_for_owned_daemon(&mut alias, &owned);
        assert_eq!(alias.id, "daemon-1");
        assert_eq!(alias.host_id, "daemon-1");
        assert!(alias.record_key.is_empty());
        assert!(alias.record_sig.is_empty());
        assert_eq!(alias.record_signed_at_unix_ms, 0);
    }

    fn daemon_record(
        daemon_id: &str,
        owner_user_id: Option<Uuid>,
        claim_code: Option<&str>,
        claim_code_created_unix_ms: Option<u64>,
    ) -> DaemonRecord {
        DaemonRecord {
            daemon_id: daemon_id.to_string(),
            label: None,
            daemon_public_key: format!("{daemon_id}-key"),
            owner_user_id,
            claim_code_hash: claim_code.map(claim_code_hash),
            claim_code_created_unix_ms,
            registered_unix_ms: 1,
            last_seen_unix_ms: 1,
            updated_unix_ms: 1,
            presence_hours: Vec::new(),
        }
    }

    #[test]
    fn open_registration_sweep_expires_only_stale_unclaimed_daemons() {
        let now = UNCLAIMED_DAEMON_TTL_MS * 10;
        let mut store = Store::default();
        let mut stale = daemon_record("stale-unclaimed", None, None, None);
        stale.last_seen_unix_ms = now - UNCLAIMED_DAEMON_TTL_MS - 1;
        // Claimed daemons are the owner's — staleness never sweeps them.
        let mut claimed = daemon_record("stale-claimed", Some(Uuid::new_v4()), None, None);
        claimed.last_seen_unix_ms = 0;
        let mut fresh = daemon_record("fresh-unclaimed", None, None, None);
        fresh.last_seen_unix_ms = now - 1;
        store.daemons = vec![stale, claimed, fresh];

        let removed = sweep_stale_unclaimed_daemons(&mut store, now);
        assert_eq!(removed, vec!["stale-unclaimed".to_string()]);
        let ids: Vec<&str> = store.daemons.iter().map(|d| d.daemon_id.as_str()).collect();
        assert_eq!(ids, vec!["stale-claimed", "fresh-unclaimed"]);
    }

    #[test]
    fn account_handles_enforce_charset_length_and_reservations() {
        assert!(validate_account_name("lenny").is_ok());
        assert!(validate_account_name("a-b-1").is_ok());
        assert!(validate_account_name("ab").is_err(), "too short");
        assert!(validate_account_name(&"a".repeat(33)).is_err(), "too long");
        assert!(validate_account_name("-abc").is_err(), "leading dash");
        assert!(validate_account_name("abc-").is_err(), "trailing dash");
        assert!(validate_account_name("Upper").is_err(), "uppercase");
        assert!(validate_account_name("a b").is_err(), "space");
        for reserved in ["admin", "google", "intendant", "support"] {
            assert!(
                validate_account_name(reserved).is_err(),
                "{reserved} must be reserved"
            );
        }
    }

    #[test]
    fn invites_are_single_purpose_bearer_records() {
        let mut invite = InviteRecord {
            code_hash: sha256_b64u(b"code"),
            label: "alpha".to_string(),
            created_unix_ms: 1,
            max_uses: 2,
            used_count: 0,
            revoked: false,
        };
        assert!(invite_usable(&invite));
        invite.used_count = 1;
        assert!(invite_usable(&invite));
        invite.used_count = 2;
        assert!(!invite_usable(&invite), "exhausted");
        invite.used_count = 0;
        invite.revoked = true;
        assert!(!invite_usable(&invite), "revoked");
    }

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

    #[test]
    fn webpush_body_has_rfc8188_layout() {
        // A synthetic subscription keypair: any valid P-256 point works
        // for layout checks (ring generates one for us).
        let rng = ring::rand::SystemRandom::new();
        let ua = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .unwrap();
        let ua_pub = ua.compute_public_key().unwrap();
        let auth = [7u8; 16];
        let plaintext = br#"{"title":"t"}"#;
        let body = webpush_encrypt(&b64u(ua_pub.as_ref()), &b64u(&auth), plaintext).unwrap();
        assert_eq!(&body[16..20], &4096u32.to_be_bytes(), "record size");
        assert_eq!(body[20], 65, "key id length");
        assert_eq!(body[21], 0x04, "uncompressed point marker");
        // salt(16) + rs(4) + idlen(1) + key(65) + ct(pt + delimiter + tag)
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + plaintext.len() + 1 + 16);
        // Two encryptions differ (fresh salt + ephemeral key).
        let again = webpush_encrypt(&b64u(ua_pub.as_ref()), &b64u(&auth), plaintext).unwrap();
        assert_ne!(body, again);
    }

    #[test]
    fn vapid_authorization_signs_a_verifiable_jwt_for_the_endpoint_origin() {
        use ring::signature::KeyPair as _;
        let mut store = Store::default();
        let keypair = load_or_create_vapid_keypair(&mut store).unwrap();
        let auth = vapid_authorization(
            &keypair,
            "https://push.example.net:8443/send/abc123",
            "https://connect.intendant.dev",
        )
        .unwrap();
        let token = auth
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split(", k=").next())
            .unwrap();
        let mut parts = token.split('.');
        let (header, claims, signature) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        let claims_json: serde_json::Value =
            serde_json::from_slice(&b64u_decode(claims).unwrap()).unwrap();
        assert_eq!(claims_json["aud"], "https://push.example.net:8443");
        assert_eq!(claims_json["sub"], "https://connect.intendant.dev");
        let signing_input = format!("{header}.{claims}");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            keypair.public_key().as_ref(),
        )
        .verify(signing_input.as_bytes(), &b64u_decode(signature).unwrap())
        .expect("VAPID JWT must verify against the service public key");
        // And the key survives a reload from the store.
        let reloaded = load_or_create_vapid_keypair(&mut store).unwrap();
        assert_eq!(
            keypair.public_key().as_ref(),
            reloaded.public_key().as_ref()
        );
    }

    #[test]
    fn presence_hours_dedupe_and_cap_at_a_week() {
        let mut hours = Vec::new();
        assert!(record_presence_hour(&mut hours, 3_600_000));
        assert!(!record_presence_hour(&mut hours, 3_700_000)); // same hour
        assert!(record_presence_hour(&mut hours, 7_200_000));
        assert_eq!(hours, vec![1, 2]);
        for i in 0..200u64 {
            record_presence_hour(&mut hours, (10 + i) * 3_600_000);
        }
        assert_eq!(hours.len(), PRESENCE_HOURS_KEPT);
        assert_eq!(*hours.last().unwrap(), 209);
    }

    #[test]
    fn generated_claim_code_is_12_word_bip39_mnemonic() {
        let code = generate_claim_code().unwrap();
        let parts: Vec<_> = code.split('-').collect();
        let words = Language::English.word_list();
        assert_eq!(parts.len(), 12);
        for part in &parts {
            assert!(words.contains(part), "unexpected claim word {part}");
        }
        assert_eq!(normalize_claim_code(&code), code);
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &code.replace('-', " "))
            .expect("generated phrase must be a valid BIP39 mnemonic");
        assert_eq!(mnemonic.to_entropy().len(), CLAIM_CODE_ENTROPY_BYTES);
    }

    #[test]
    fn claim_code_normalization_accepts_case_and_separator_variants() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        assert_eq!(
            normalize_claim_code("  Abandon Ability--ABLE_about.above absent absorb  "),
            code
        );
        assert_eq!(claim_code_hash(code), claim_code_hash(&code.to_uppercase()));
        assert_eq!(
            claim_code_hash(code),
            claim_code_hash("abandon ability able about above absent absorb")
        );
    }

    #[test]
    fn app_route_requires_connect_mode_and_daemon_id() {
        assert!(valid_connect_app_query(Some(
            "connect=1&daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant&connect=1"
        )));
        assert!(!valid_connect_app_query(None));
        assert!(!valid_connect_app_query(Some("")));
        assert!(!valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(!valid_connect_app_query(Some("connect=1")));
        assert!(!valid_connect_app_query(Some("connect=0&daemon_id=daemon")));
        assert!(!valid_connect_app_query(Some("connect=1&daemon_id=%20")));
    }

    #[test]
    fn trust_page_states_the_model() {
        let html = trust_ui_html("https://connect.intendant.dev");
        assert!(html.contains("<title>How trust works"));
        assert!(html.contains("rendezvous-scoped things"));
        assert!(html.contains("run your own rendezvous"));
        assert!(html.contains("<code>https://connect.intendant.dev</code>"));
    }

    #[test]
    fn access_ui_uses_access_branding() {
        let html = connect_ui_html(
            "https://intendant.dev",
            "Intendant Access",
            "Rendezvous and fleet navigation",
        );
        assert!(html.contains("<title>Intendant Access</title>"));
        assert!(html.contains("<h1>Intendant Access</h1>"));
        assert!(html.contains(">Rendezvous and fleet navigation</div>"));
        assert!(html.contains("target daemons enforce local IAM"));
    }

    #[test]
    fn transparency_pin_fails_hard_on_log_key_change() {
        // The documented pin ("rewriting history here is detectable") is only
        // real if a swapped log signing key is a verification failure, not a
        // silent re-pin; recovery must be the explicit user reset.
        let html = connect_ui_html("https://intendant.dev", "Intendant Connect", "sub");
        assert!(html.contains("pinned.public_key !== sth.public_key"));
        assert!(html.contains("log signing key changed"));
        assert!(html.contains(r#"id="log-reset-trust""#));
        assert!(html.contains("localStorage.removeItem(LOG_STH_KEY)"));
    }

    #[test]
    fn active_claim_code_hashes_only_tracks_fresh_unclaimed_other_daemons() {
        let now = now_unix_ms();
        let fresh = "abandon-ability-able-about-above-absent-absorb";
        let current = "abstract-absurd-abuse-access-accident-account-accuse";
        let expired = "achieve-acid-acoustic-acquire-across-act-action";
        let claimed = "actor-actress-actual-adapt-add-addict-address";
        let store = Store {
            users: Vec::new(),
            daemons: vec![
                daemon_record("fresh", None, Some(fresh), Some(now)),
                daemon_record("current", None, Some(current), Some(now)),
                daemon_record(
                    "expired",
                    None,
                    Some(expired),
                    Some(now.saturating_sub(CLAIM_CODE_TTL_MS + 1)),
                ),
                daemon_record("claimed", Some(Uuid::new_v4()), Some(claimed), Some(now)),
            ],
            fleet_targets: Vec::new(),
            audit: Vec::new(),
            orl_bulletins: Vec::new(),
            vault_blobs: Vec::new(),
            invites: Vec::new(),
            vapid_private_pk8_b64: None,
            push_subscriptions: Vec::new(),
            log_private_pk8_b64: None,
            log_entries: Vec::new(),
        };
        let hashes = active_claim_code_hashes(&store, "current", now);
        assert!(hashes.contains(&claim_code_hash(fresh)));
        assert!(!hashes.contains(&claim_code_hash(current)));
        assert!(!hashes.contains(&claim_code_hash(expired)));
        assert!(!hashes.contains(&claim_code_hash(claimed)));
    }

    #[test]
    fn ensure_claim_code_reuses_fresh_unique_in_memory_code() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::new();

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_eq!(returned, code);
        let expected_hash = claim_code_hash(code);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn ensure_claim_code_replaces_active_hash_collision() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::from([claim_code_hash(code)]);

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_ne!(returned, code);
        assert!(!active_hashes.contains(&claim_code_hash(&returned)));
        let expected_hash = claim_code_hash(&returned);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn fleet_target_input_is_sanitized_and_capped() {
        let user_id = Uuid::new_v4();
        let now = now_unix_ms();
        let target = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: " target\nid ".to_string(),
                host_id: " target\nid ".to_string(),
                label: " My target ".to_string(),
                local: true,
                source: "browser fleet!".to_string(),
                access_domain: "user_client".to_string(),
                access_domain_label: " User/client ".to_string(),
                route: "hosted_connect".to_string(),
                route_key: String::new(),
                route_label: " Hosted Connect ".to_string(),
                auth: "connect_account".to_string(),
                auth_label: " Connect account ".to_string(),
                effective_role: "root".to_string(),
                effective_role_label: " Root ".to_string(),
                profile: "root".to_string(),
                url: "javascript:alert(1)".to_string(),
                ws_url: "wss://example.test/ws".to_string(),
                browser_tcp_via_url: "/app?connect=1&daemon_id=daemon".to_string(),
                connect_signaling_base: String::new(),
                enc_fields: String::new(),
                origin: "https://intendant.dev".to_string(),
                connect_daemon_id: " daemon ".to_string(),
                record_key: String::new(),
                record_sig: String::new(),
                record_signed_at_unix_ms: 0,
                capabilities: vec![
                    json!("display"),
                    json!("display"),
                    json!("custom:files"),
                    json!(42),
                ],
                first_seen_unix_ms: now.saturating_add(10_000),
                last_seen_unix_ms: now.saturating_add(10_000),
            },
            now,
        )
        .expect("target should normalize");

        assert_eq!(target.user_id, user_id);
        assert_eq!(target.host_id, "targetid");
        assert_eq!(target.label, "My target");
        assert_eq!(target.source, "browserfleet");
        assert_eq!(target.url, "");
        assert_eq!(target.ws_url, "wss://example.test/ws");
        assert_eq!(
            target.browser_tcp_via_url,
            "/app?connect=1&daemon_id=daemon"
        );
        assert_eq!(target.origin, "https://intendant.dev");
        assert_eq!(target.connect_daemon_id.as_deref(), Some("daemon"));
        assert_eq!(target.capabilities, vec!["display", "custom:files"]);
        assert_eq!(target.first_seen_unix_ms, now);
        assert_eq!(target.last_seen_unix_ms, now);
    }

    #[test]
    fn fleet_targets_merge_claimed_daemons_over_remembered_records() {
        let user_id = Uuid::new_v4();
        let store = Store {
            users: Vec::new(),
            daemons: vec![DaemonRecord {
                daemon_id: "daemon-1".to_string(),
                label: Some("Live daemon".to_string()),
                daemon_public_key: "daemon-key".to_string(),
                owner_user_id: Some(user_id),
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                registered_unix_ms: 10,
                last_seen_unix_ms: now_unix_ms(),
                updated_unix_ms: 20,
                presence_hours: Vec::new(),
            }],
            fleet_targets: vec![
                FleetTargetRecord {
                    user_id,
                    id: "daemon-1".to_string(),
                    host_id: "daemon-1".to_string(),
                    label: "Stale label".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "hosted_connect".to_string(),
                    route_label: "Hosted Connect".to_string(),
                    auth: "connect_account".to_string(),
                    auth_label: "Connect account".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "intendant:192.168.64.61".to_string(),
                    host_id: "intendant:192.168.64.61".to_string(),
                    label: "192.168.64.61".to_string(),
                    local: true,
                    source: "dashboard".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "current_dashboard".to_string(),
                    route_label: "Current dashboard".to_string(),
                    auth: "trusted_dashboard".to_string(),
                    auth_label: "Trusted dashboard session".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    origin: "https://connect.intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "manual".to_string(),
                    host_id: "manual".to_string(),
                    label: "Manual target".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: String::new(),
                    access_domain_label: String::new(),
                    route: String::new(),
                    route_label: "Remembered route".to_string(),
                    auth: String::new(),
                    auth_label: String::new(),
                    effective_role: String::new(),
                    effective_role_label: String::new(),
                    profile: String::new(),
                    url: "https://manual.example".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: None,
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
            ],
            audit: Vec::new(),
            orl_bulletins: Vec::new(),
            vault_blobs: Vec::new(),
            invites: Vec::new(),
            vapid_private_pk8_b64: None,
            push_subscriptions: Vec::new(),
            log_private_pk8_b64: None,
            log_entries: Vec::new(),
        };
        let config = ServiceConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 9876)),
            public_origin: "https://intendant.dev".to_string(),
            rp_id: "intendant.dev".to_string(),
            static_root: PathBuf::from("static"),
            data_file: PathBuf::from("state.json"),
            daemon_token: None,
            invite_required: false,
            open_daemon_registration: false,
            cookie_secure: true,
        };

        let targets = fleet_targets_for_user(&config, &store, user_id);
        assert_eq!(targets.len(), 2);
        let live = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("daemon-1"))
            .expect("live daemon target");
        assert_eq!(
            live.get("label").and_then(|v| v.as_str()),
            Some("Live daemon")
        );
        assert_eq!(
            live.get("source").and_then(|v| v.as_str()),
            Some("connect_daemon")
        );
        let manual = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("manual"))
            .expect("manual target");
        assert_eq!(
            manual.get("source").and_then(|v| v.as_str()),
            Some("browser_fleet")
        );
    }

    #[test]
    fn landing_page_states_the_product_and_reuses_the_origin() {
        let html = landing_ui_html("https://rendezvous.example");
        assert!(html.contains("<title>Intendant — an operating environment"));
        // The install one-liner advertises the serving origin, so a
        // self-hosted rendezvous shows its own installer — with the
        // placeholder entity-escaped so browsers render it as text.
        assert!(html.contains("curl -fsSL https://rendezvous.example/install.sh"));
        assert!(html.contains("--owner &lt;your-key&gt;"));
        assert!(!html.contains("--owner <your-key>"));
        // Beginner path and depth are both one click away.
        assert!(html.contains(r#"href="/connect""#));
        assert!(html.contains(r#"href="/trust""#));
        assert!(html.contains(DOCS_URL));
        assert!(html.contains(REPO_URL));
        assert!(html.contains("Built to be distrusted"));
        // The tour shows the product: every embedded screenshot is referenced,
        // with alt text so the page reads without images.
        for asset in [
            "hero.webp",
            "video.webp",
            "station.webp",
            "vault.webp",
            "claim.webp",
            "phone.webp",
        ] {
            assert!(
                html.contains(&format!("/assets/landing/{asset}")),
                "landing page must reference {asset}"
            );
        }
        assert!(html.contains("alt=\"The Intendant dashboard's Activity feed"));
        // The differentiator is stated where people will read it: the client
        // installs nothing, on any device — only the agent's machine does.
        assert!(html.contains("Nothing to install on your side"));
        assert!(html.contains("nothing to install on your side of the glass"));
        // "How do I use it" is the page's first answer: the install
        // questionnaire sits directly under the hero, before the shot tour.
        let install_at = html.find(r#"<section class="install-section""#).unwrap();
        let heroshot_at = html.find(r#"<section class="heroshot""#).unwrap();
        let tour_at = html.find(r#"<section class="tour""#).unwrap();
        assert!(
            install_at < heroshot_at && heroshot_at < tour_at,
            "install must lead, then the product tour"
        );
        // The name is the thesis, stated once, quietly, before the trust row.
        assert!(html.contains("Why “Intendant”"));
        assert!(html.contains("a network of agentic networks"));
        // Custody names the two fueling modes by what travels: the key
        // (lease) vs the calls (client egress — the disposable-box mode).
        assert!(html.contains(r#"class="fuelmap""#));
        assert!(html.contains("the key travels:"));
        assert!(html.contains("the calls travel:"));
        // The canonical mark, not an ad-hoc monogram: favicon + header logo.
        assert!(html.contains(r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#));
        assert!(html.contains(r#"<link rel="icon" type="image/png" href="/favicon.png">"#));
        assert!(html.contains(r#"<img src="/logo.svg""#));
        assert!(!html.contains("data:image/svg"));
        // The deployment advisor LEADS the install section — no fold to
        // find, four questions all about the agent's machine (the client
        // side installs nothing, so it gets no questions), and
        // runtime-origin commands so self-hosted rendezvous advertise their
        // own installers there too — the sh one-liner AND the PowerShell
        // one (Windows is first-class).
        assert!(!html.contains("<details class=\"advisor\""));
        for question in [
            "OS on the agent's machine?",
            "What kind of machine?",
            "What will fuel it?",
            "Keep working with your browser closed?",
        ] {
            assert!(html.contains(question), "advisor must ask: {question}");
        }
        // The default answers' command is server-rendered, so the page
        // shows a working one-liner (Linux VPS ⇒ --service) without JS.
        assert!(html.contains(
            "curl -fsSL https://rendezvous.example/install.sh | sh -s -- --owner &lt;your-key&gt; --service"
        ));
        assert!(!html.contains("__ADVISOR_DEFAULT_CMD__"));
        assert!(html.contains("location.origin + '/install.sh"));
        assert!(html.contains("/install.ps1"));
        assert!(html.contains("--service"));
        assert!(html.contains("-Service"));
        // No init system is asserted as a given — the note speaks in
        // native-supervisor terms, not systemd.
        assert!(!html.contains("journalctl"));
        // Honest pre-alpha framing before anyone clicks Sign in.
        assert!(html.contains(r#"<span class="pill-alpha">pre-alpha</span>"#));
    }

    #[test]
    fn connect_page_frames_the_private_alpha() {
        let html = connect_ui_html(
            "https://intendant.dev",
            "Intendant Connect",
            "Rendezvous account",
        );
        // The invite dead-end explains itself and offers the two open paths.
        assert!(html.contains("private pre-alpha"));
        assert!(html.contains("self-hosting is never gated"));
        assert!(html.contains(r#"$('invite-note').classList.toggle"#));
        // Shareable invite links prefill the code.
        assert!(html.contains("params.get('invite')"));
    }

    #[test]
    fn every_page_serves_the_canonical_mark() {
        // The embedded mark is the real artwork: SVG vector + PNG fallback
        // (kept in lockstep with static/ by include_str!/include_bytes!).
        assert!(LOGO_SVG.starts_with("<svg"));
        assert!(
            LOGO_SVG.contains(r#"viewBox="16 16 480 480""#),
            "logo.svg must stay the margin-cropped view of the macOS icon"
        );
        assert_eq!(&BRAND_ICON_PNG[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            BRAND_ICON_PNG.len() > 2_048,
            "brand icon suspiciously small"
        );
        let svg_link = r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#;
        let png_link = r#"<link rel="icon" type="image/png" href="/favicon.png">"#;
        let connect = connect_ui_html(
            "https://x.example",
            "Intendant Connect",
            "Rendezvous account",
        );
        assert!(connect.contains(svg_link) && connect.contains(png_link));
        assert!(connect.contains(r#"class="brand-mark" src="/logo.svg""#));
        assert!(!connect.contains(">IC</div>"));
        let trust = trust_ui_html("https://x.example");
        assert!(trust.contains(svg_link) && trust.contains(png_link));
        assert!(!trust.contains(">IC</div>"));
    }

    #[test]
    fn landing_assets_are_embedded_webp() {
        for asset in [
            "hero.webp",
            "video.webp",
            "station.webp",
            "vault.webp",
            "claim.webp",
            "phone.webp",
        ] {
            let bytes = landing_asset_bytes(asset)
                .unwrap_or_else(|| panic!("missing embedded landing asset {asset}"));
            // RIFF....WEBP container magic.
            assert!(bytes.len() > 8_192, "{asset} suspiciously small");
            assert_eq!(&bytes[0..4], b"RIFF", "{asset} is not a RIFF container");
            assert_eq!(&bytes[8..12], b"WEBP", "{asset} is not WebP");
        }
        assert!(landing_asset_bytes("nope.webp").is_none());
        assert!(landing_asset_bytes("../secrets").is_none());
    }

    #[test]
    fn embedded_installer_is_the_bootstrap_script() {
        assert!(
            INSTALL_SH.starts_with("#!/bin/sh"),
            "installer must be a sh script"
        );
        assert!(
            INSTALL_SH.contains("--owner"),
            "installer must support the owner bootstrap"
        );
        assert!(
            INSTALL_SH.contains("cargo build --release"),
            "installer must build release binaries"
        );
        // --service must delegate to the binary's cross-platform service
        // subcommand, never hand-roll a unit (systemd is one backend of
        // four, not a dependency).
        assert!(INSTALL_SH.contains("service install --now --"));
        assert!(!INSTALL_SH.contains("/etc/systemd/system"));

        assert!(
            INSTALL_PS1.starts_with("<#"),
            "ps1 installer must open with comment help"
        );
        // Windows PowerShell 5.1 decodes BOM-less files as ANSI, and a
        // UTF-8 em-dash misdecodes into a cp1252 smart QUOTE — which the
        // parser honors, unbalancing every string after it. The bootstrap
        // script stays pure ASCII so no delivery path can corrupt it.
        assert!(INSTALL_PS1.is_ascii(), "install.ps1 must be pure ASCII");
        for needle in [
            "param(",
            "$Owner",
            "$Connect",
            "$Service",
            "cargo build --release",
            "\"service\", \"install\"",
        ] {
            assert!(
                INSTALL_PS1.contains(needle),
                "install.ps1 must contain {needle}"
            );
        }
    }

    #[test]
    fn served_installers_default_connect_to_the_serving_rendezvous() {
        // The embedded scripts must keep the sentinel lines the handlers
        // splice — if either drifts, injection silently stops and a fresh
        // VPS comes up unregistered (hosted claiming dead-ends).
        assert!(
            INSTALL_SH.contains(INSTALL_SH_CONNECT_DEFAULT),
            "install.sh connect-default sentinel drifted"
        );
        assert!(
            INSTALL_PS1.contains(INSTALL_PS1_CONNECT_DEFAULT),
            "install.ps1 connect-default sentinel drifted"
        );

        let sh = install_sh_body("https://rendezvous.example");
        assert!(sh.contains(
            r#"CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-https://rendezvous.example}""#
        ));
        assert!(!sh.contains(INSTALL_SH_CONNECT_DEFAULT));

        let ps1 = install_ps1_body("https://rendezvous.example");
        assert!(ps1.contains(r#"[string]$Connect = "https://rendezvous.example","#));
        assert!(!ps1.contains(INSTALL_PS1_CONNECT_DEFAULT));
        // The ANSI-decode trap applies to the served body, not just the
        // embedded file — the injected origin must not break the pin.
        assert!(ps1.is_ascii(), "served install.ps1 must stay pure ASCII");

        // Splice guard: only a plain URL charset reaches the scripts.
        assert!(connect_default_injectable("https://intendant.dev"));
        assert!(connect_default_injectable("http://localhost:9891"));
        assert!(!connect_default_injectable(r#"https://x"; rm -rf ~"#));
        assert!(!connect_default_injectable("https://x y"));
        assert!(!connect_default_injectable(""));
        let verbatim = install_sh_body(r#"https://x" y"#);
        assert_eq!(verbatim, INSTALL_SH, "unsafe origin must serve verbatim");
    }

    /// Windows PowerShell 5.1 executes setup-windows.ps1 straight from the
    /// fresh clone, so the BOM-less ANSI-decode trap pinned for install.ps1
    /// above applies to it identically — a non-ASCII byte that lands in
    /// code (not a comment) can decode into a cp1252 smart quote the parser
    /// honors. Keep every PowerShell file a fresh box runs pure ASCII.
    #[test]
    fn setup_windows_ps1_is_pure_ascii() {
        const SETUP_PS1: &str = include_str!("../../../scripts/setup-windows.ps1");
        assert!(SETUP_PS1.is_ascii(), "setup-windows.ps1 must be pure ASCII");
    }

    /// Real parse coverage for the PowerShell installer, on the platform
    /// that ships PowerShell — a macOS/Linux dev box cannot tokenize it.
    #[cfg(windows)]
    #[test]
    fn embedded_ps1_installer_parses() {
        let dir = std::env::temp_dir().join(format!("intendant-ps1-parse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("install.ps1");
        std::fs::write(&script, INSTALL_PS1).unwrap();
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$errs = $null; [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errs) | Out-Null; if ($errs.Count) {{ $errs | ForEach-Object {{ Write-Error $_.Message }}; exit 1 }}",
                    script.display()
                ),
            ])
            .output()
            .expect("powershell must exist on Windows");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            output.status.success(),
            "install.ps1 has parse errors: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn dry_push_payload_names_daemon_and_credentials() {
        let payload = dry_push_payload(
            "daemon-1",
            "Workshop box",
            &[
                json!({ "kind": "api_key:anthropic", "label": "Personal Anthropic" }),
                json!({ "kind": "oauth:codex" }),
            ],
        );
        assert_eq!(payload["title"].as_str(), Some("Workshop box is unfueled"));
        let body = payload["body"].as_str().unwrap();
        assert!(body.contains("Personal Anthropic"), "{body}");
        assert!(body.contains("oauth:codex"), "{body}");
        assert!(body.contains("Reconnect a fueling session"), "{body}");
        assert_eq!(
            payload["url"].as_str(),
            Some("/app?connect=1&daemon_id=daemon-1")
        );

        // No names at all still produces a sensible message.
        let fallback = dry_push_payload("d", "D", &[]);
        assert!(fallback["body"].as_str().unwrap().contains("credentials"));
    }

    fn vault_blob(revision: u64, marker: &str) -> serde_json::Value {
        json!({
            "v": 1,
            "kind": "intendant-vault",
            "revision": revision,
            "envelopes": [
                { "kind": "prf", "id": "env-1", "iv": "aW4=", "wrapped": marker },
            ],
            "body": { "iv": "aW4=", "ct": marker },
        })
    }

    #[test]
    fn vault_publish_stores_bumps_and_is_idempotent() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        assert!(apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 10).unwrap());
        assert_eq!(store.vault_blobs.len(), 1);
        assert_eq!(store.vault_blobs[0].revision, 1);
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 10);

        // Identical same-revision republish is an idempotent no-op.
        assert!(!apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 20).unwrap());
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 10);

        // A newer revision replaces the blob.
        assert!(apply_vault_publish(&mut store, user, 3, vault_blob(3, "b"), 30).unwrap());
        assert_eq!(store.vault_blobs[0].revision, 3);
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 30);

        // A second user gets an independent record.
        let other = Uuid::new_v4();
        assert!(apply_vault_publish(&mut store, other, 1, vault_blob(1, "c"), 40).unwrap());
        assert_eq!(store.vault_blobs.len(), 2);
        assert_eq!(store.vault_blobs[0].revision, 3);
    }

    fn vault_blob_with_mac(revision: u64, marker: &str, mac: &str) -> serde_json::Value {
        let mut blob = vault_blob(revision, marker);
        blob["mac"] = json!(mac);
        blob
    }

    #[test]
    fn vault_publish_enforces_the_mac_presence_ratchet() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        // Legacy MAC-less vaults are accepted, and upgrading to an
        // authenticated blob is a normal publish.
        assert!(apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 10).unwrap());
        assert!(
            apply_vault_publish(&mut store, user, 2, vault_blob_with_mac(2, "b", "bWFj"), 20)
                .unwrap()
        );

        // Once authenticated, a MAC-less replacement is refused even at a
        // newer revision — the store must not strip the guarantee.
        let err = apply_vault_publish(&mut store, user, 3, vault_blob(3, "c"), 30).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(store.vault_blobs[0].revision, 2);

        // Authenticated publishes keep flowing.
        assert!(
            apply_vault_publish(&mut store, user, 3, vault_blob_with_mac(3, "d", "bWFj"), 40)
                .unwrap()
        );

        // A malformed mac field is rejected outright.
        let err = apply_vault_publish(
            &mut store,
            user,
            4,
            vault_blob_with_mac(4, "e", &"x".repeat(89)),
            50,
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn vault_publish_rejects_rollback_and_same_revision_conflicts() {
        let mut store = Store::default();
        let user = Uuid::new_v4();
        apply_vault_publish(&mut store, user, 5, vault_blob(5, "a"), 10).unwrap();

        // Rollback to an older revision is refused.
        let err = apply_vault_publish(&mut store, user, 4, vault_blob(4, "b"), 20).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);

        // Same revision with different content is a conflict, not a
        // silent drop — the losing device must refetch, merge, and bump.
        let err = apply_vault_publish(&mut store, user, 5, vault_blob(5, "b"), 20).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(store.vault_blobs[0].revision, 5);
        assert_eq!(
            store.vault_blobs[0]
                .vault
                .pointer("/body/ct")
                .and_then(|v| v.as_str()),
            Some("a")
        );
    }

    #[test]
    fn vault_publish_rejects_malformed_blobs() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        // Wrong kind.
        let mut wrong_kind = vault_blob(1, "a");
        wrong_kind["kind"] = json!("something-else");
        assert!(apply_vault_publish(&mut store, user, 1, wrong_kind, 10).is_err());

        // Revision zero is reserved for "no vault yet".
        assert!(apply_vault_publish(&mut store, user, 0, vault_blob(0, "a"), 10).is_err());

        // Envelope-free blobs would be unrecoverable — refuse them.
        let mut no_envelopes = vault_blob(1, "a");
        no_envelopes["envelopes"] = json!([]);
        assert!(apply_vault_publish(&mut store, user, 1, no_envelopes, 10).is_err());

        // Blob revision must match the request revision.
        assert!(apply_vault_publish(&mut store, user, 2, vault_blob(1, "a"), 10).is_err());

        // Oversized blobs are refused before any store mutation.
        let mut oversized = vault_blob(1, "a");
        oversized["body"]["ct"] = json!("x".repeat(MAX_VAULT_BLOB_BYTES + 1));
        assert!(apply_vault_publish(&mut store, user, 1, oversized, 10).is_err());
        assert!(store.vault_blobs.is_empty());
    }
}
