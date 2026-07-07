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

mod ui;
pub(crate) use ui::*;
mod accounts;
pub(crate) use accounts::*;
mod transparency;
pub(crate) use transparency::*;
mod push;
pub(crate) use push::*;
mod fleet;
pub(crate) use fleet::*;

const PROTOCOL: &str = "intendant-connect-rendezvous-v1";
const CLAIM_PROTOCOL: &str = "intendant-connect-claim-v1";
/// v2 claim proofs bind the claiming account (user id + handle at claim
/// time) into the payload the daemon signs, so the account↔daemon binding
/// this service records is co-signed by the daemon's own key instead of
/// merely asserted by this service. v1 (account-blind) stays accepted
/// from older daemons.
const CLAIM_PROTOCOL_V2: &str = "intendant-connect-claim-v2";
/// Daemon-signed release of a claim binding (the box evicting its own
/// claim — the recovery path when the account side would never revoke).
const UNCLAIM_PROTOCOL: &str = "intendant-connect-unclaim-v1";
/// Freshness window for daemon-signed unclaim payloads: signatures bind a
/// timestamp so a captured release cannot be replayed to evict a future
/// re-claim.
const UNCLAIM_MAX_SKEW_MS: u64 = 5 * 60 * 1000;
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
        .route("/api/claims/{claim_id}/arm", post(api_claim_arm))
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
        .route("/api/daemon/unclaim", post(daemon_unclaim))
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
    /// True when `claim_code_hash` was minted by the daemon itself
    /// (first-owner bootstrap): this service holds only the hash, the
    /// freshness is presence-bound (refreshed on every register poll
    /// instead of the 10-minute TTL), and a claim against it requires the
    /// arm step before the challenge fires.
    #[serde(default)]
    claim_code_daemon_minted: bool,
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
    /// Handle snapshot at claim start — the exact string offered to the
    /// daemon for v2 proof signing, so verification reconstructs the
    /// payload byte-for-byte even if the handle is renamed mid-claim.
    account_name: String,
    daemon_id: String,
    challenge: String,
    created_unix_ms: u64,
    /// First-owner bootstrap (daemon-minted phrase): the challenge does
    /// not fire until the browser arms the claim with its identity key
    /// and phrase-derived tag.
    bootstrap_required: bool,
    armed: bool,
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

#[derive(Debug, Deserialize)]
struct ClaimStartRequest {
    #[serde(default)]
    claim_code: String,
    /// Preferred: SHA-256 (base64url) of the normalized phrase, computed
    /// client-side — this service never needs to see plaintext codes.
    #[serde(default)]
    claim_code_hash: Option<String>,
}

async fn api_claim_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimStartRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_start", 10, 60_000).await?;
    let code_hashes = match body
        .claim_code_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    {
        Some(hash) => {
            if !is_sha256_b64u(hash) {
                return Err(ApiError::bad_request(
                    "claim_code_hash must be an unpadded base64url SHA-256 digest",
                ));
            }
            vec![hash.to_string()]
        }
        None => {
            if normalize_claim_code(&body.claim_code).is_empty() {
                return Err(ApiError::bad_request("claim_code is required"));
            }
            claim_code_hash_candidates(&body.claim_code)
        }
    };
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
                    && d.claim_code_created_unix_ms.is_some_and(|created| {
                        // Daemon-minted hashes are presence-fresh (renewed
                        // on every register poll), so the same TTL check
                        // naturally covers both kinds.
                        now.saturating_sub(created) <= CLAIM_CODE_TTL_MS
                    })
            })
            .cloned()
            .ok_or_else(|| ApiError::not_found("claim code not found"))?
    };
    let needs_bootstrap_arm = daemon.claim_code_daemon_minted;
    let claim_id = Uuid::new_v4().to_string();
    let challenge = random_b64u(32);
    state.pending_claims.lock().await.insert(
        claim_id.clone(),
        PendingClaim {
            user_id: user.id,
            account_name: user.account_name.clone(),
            daemon_id: daemon.daemon_id.clone(),
            challenge: challenge.clone(),
            created_unix_ms: now_unix_ms(),
            bootstrap_required: needs_bootstrap_arm,
            armed: false,
            status: ClaimStatus::Pending,
        },
    );
    // The challenge names the claiming account so the daemon can co-sign
    // *who* it is being claimed by (v2 proofs) and show "claimed by
    // @handle" from its own signed record rather than this service's word.
    // Bootstrap claims hold the challenge until the browser arms them
    // with its identity key + phrase-derived tag (api_claim_arm).
    if !needs_bootstrap_arm {
        enqueue_event(
            &state,
            &daemon.daemon_id,
            RendezvousEvent {
                id: Uuid::new_v4().to_string(),
                kind: "claim_challenge".to_string(),
                claim_id: Some(claim_id.clone()),
                challenge: Some(challenge),
                user_id: Some(user.id.to_string()),
                account_name: Some(user.account_name.clone()),
                ..RendezvousEvent::default()
            },
        )
        .await;
    }
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_started",
            Some(user.id),
            Some(daemon.daemon_id.clone()),
            json!({ "claim_id": claim_id, "bootstrap": needs_bootstrap_arm }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": daemon.daemon_id,
        "daemon_public_key": daemon.daemon_public_key,
        "needs_bootstrap_arm": needs_bootstrap_arm,
    })))
}

#[derive(Debug, Deserialize)]
struct ClaimArmRequest {
    client_key: String,
    client_key_tag: String,
}

/// Arm a first-owner bootstrap claim: the browser presents its identity
/// key plus an HMAC tag derived from the daemon-minted phrase, and only
/// then does the claim challenge fire. This service relays both blind —
/// it holds the phrase's hash, not the phrase, so it can neither compute
/// a tag for a key of its own nor alter the browser's (the daemon
/// recomputes the tag over the exact key it enrolls).
async fn api_claim_arm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(claim_id): AxumPath<String>,
    Json(body): Json<ClaimArmRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_arm", 10, 60_000).await?;
    let client_key = body.client_key.trim().to_string();
    let client_key_tag = body.client_key_tag.trim().to_string();
    if client_key.is_empty() || client_key_tag.is_empty() {
        return Err(ApiError::bad_request(
            "client_key and client_key_tag are required",
        ));
    }
    let (daemon_id, challenge, user_id_string, account_name) = {
        let mut claims = state.pending_claims.lock().await;
        let claim = claims
            .get_mut(claim_id.trim())
            .ok_or_else(|| ApiError::not_found("claim not found"))?;
        if claim.user_id != user.id {
            return Err(ApiError::forbidden("claim belongs to a different account"));
        }
        if !matches!(claim.status, ClaimStatus::Pending) {
            return Err(ApiError::bad_request("claim is already resolved"));
        }
        if now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS {
            claim.status = ClaimStatus::Rejected {
                error: "claim timed out".to_string(),
            };
            return Err(ApiError::bad_request("claim timed out"));
        }
        if !claim.bootstrap_required {
            return Err(ApiError::bad_request("claim does not need arming"));
        }
        if claim.armed {
            return Err(ApiError::bad_request("claim is already armed"));
        }
        claim.armed = true;
        (
            claim.daemon_id.clone(),
            claim.challenge.clone(),
            claim.user_id.to_string(),
            claim.account_name.clone(),
        )
    };
    enqueue_event(
        &state,
        &daemon_id,
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "claim_challenge".to_string(),
            claim_id: Some(claim_id.trim().to_string()),
            challenge: Some(challenge),
            user_id: Some(user_id_string),
            account_name: Some(account_name),
            bootstrap_client_key: Some(client_key),
            bootstrap_client_key_tag: Some(client_key_tag),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_armed",
            Some(user.id),
            Some(daemon_id),
            json!({ "claim_id": claim_id.trim() }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
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
    /// First-owner bootstrap (fresh boxes): the daemon minted its own
    /// claim phrase locally and registers only the SHA-256 (base64url) of
    /// its normalized form. This service never sees the plaintext, so it
    /// can route a claim to the daemon but cannot claim (or enroll
    /// against) the daemon itself.
    #[serde(default)]
    bootstrap_code_hash: Option<String>,
}

async fn daemon_register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonRegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let observed_ip = client_observed_ip(&headers);
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
    let bootstrap_code_hash = body
        .bootstrap_code_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty());
    if let Some(hash) = bootstrap_code_hash {
        if !is_sha256_b64u(hash) {
            return Err(ApiError::bad_request(
                "bootstrap_code_hash must be an unpadded base64url SHA-256 digest",
            ));
        }
    }
    let mut claim_code = None;
    let mut daemon_minted = false;
    let (claimed, claimed_by, claim_code_expires_unix_ms) = {
        let mut claim_codes = state.claim_codes.lock().await;
        let mut store = state.store.lock().await;
        let now = now_unix_ms();
        for stale_id in sweep_stale_unclaimed_daemons(&mut store, now) {
            claim_codes.remove(&stale_id);
        }
        let active_claim_hashes = active_claim_code_hashes(&store, &daemon_id, now);
        // Applies the unclaimed-record claim-code policy: a daemon-minted
        // bootstrap hash wins (presence-fresh, plaintext never seen here);
        // otherwise the service mints and remints on the usual TTL.
        let apply_claim_code = |record: &mut DaemonRecord,
                                claim_codes: &mut HashMap<String, String>,
                                claim_code: &mut Option<String>|
         -> ApiResult<()> {
            match bootstrap_code_hash {
                Some(hash) => {
                    if active_claim_hashes.contains(hash) {
                        return Err(ApiError::conflict(
                            "bootstrap claim hash collides with another active claim code",
                        ));
                    }
                    claim_codes.remove(&record.daemon_id);
                    record.claim_code_hash = Some(hash.to_string());
                    record.claim_code_daemon_minted = true;
                    // Presence-bound freshness: valid while the daemon
                    // polls, instead of the 10-minute TTL.
                    record.claim_code_created_unix_ms = Some(now);
                }
                None => {
                    if record.claim_code_daemon_minted {
                        // The daemon stopped offering bootstrap (an owner
                        // appeared locally) — revert to service-minted.
                        record.claim_code_hash = None;
                        record.claim_code_daemon_minted = false;
                        record.claim_code_created_unix_ms = None;
                    }
                    *claim_code =
                        Some(ensure_claim_code(claim_codes, record, &active_claim_hashes)?);
                }
            }
            Ok(())
        };
        let (owner_user_id, code_created_unix_ms) = if let Some(existing) =
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
                apply_claim_code(existing, &mut claim_codes, &mut claim_code)?;
                daemon_minted = existing.claim_code_daemon_minted;
            }
            (existing.owner_user_id, existing.claim_code_created_unix_ms)
        } else {
            let mut record = DaemonRecord {
                daemon_id: daemon_id.clone(),
                label: None,
                daemon_public_key: daemon_public_key.clone(),
                owner_user_id: None,
                claim_code_hash: None,
                claim_code_daemon_minted: false,
                claim_code_created_unix_ms: None,
                registered_unix_ms: now,
                last_seen_unix_ms: now,
                updated_unix_ms: now,
                presence_hours: Vec::new(),
            };
            apply_claim_code(&mut record, &mut claim_codes, &mut claim_code)?;
            daemon_minted = record.claim_code_daemon_minted;
            let created = record.claim_code_created_unix_ms;
            store.daemons.push(record);
            (None, created)
        };
        persist_locked(&state, &store)?;
        // Current handle, not a claim-time snapshot: a renamed account
        // shows its new name here. The daemon's own signed claim record
        // (v2 proofs) keeps the at-claim-time identity.
        let claimed_by = owner_user_id.map(|uid| {
            (
                uid,
                store
                    .users
                    .iter()
                    .find(|u| u.id == uid)
                    .map(|u| u.account_name.clone())
                    .unwrap_or_default(),
            )
        });
        let expires = if owner_user_id.is_none() && !daemon_minted {
            code_created_unix_ms.map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        } else {
            // Claimed, or daemon-minted (presence-bound: fresh while the
            // daemon keeps polling).
            None
        };
        (owner_user_id.is_some(), claimed_by, expires)
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
        "claimed_by_user_id": claimed_by.as_ref().map(|(uid, _)| uid.to_string()),
        "claimed_by_handle": claimed_by
            .as_ref()
            .map(|(_, handle)| handle.clone())
            .filter(|handle| !handle.is_empty()),
        "claim_code": claim_code,
        "claim_code_daemon_minted": daemon_minted,
        "claim_code_expires_unix_ms": claim_code_expires_unix_ms,
        "claim_url": claim_url,
        "daemon_public_key": daemon_public_key,
        "observed_ip": observed_ip,
    })))
}

/// Shape check for a daemon-minted bootstrap hash: unpadded base64url of
/// a SHA-256 digest — exactly 43 characters of the base64url alphabet.
fn is_sha256_b64u(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_proto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_account_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_account_name: Option<String>,
    // Signed org-grant document, also relayed verbatim: the daemon verifies
    // it against the org keys it locally trusts, so this service can
    // neither mint nor amplify one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_grant: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
    // First-owner bootstrap arm fields, relayed blind: the daemon
    // recomputes the phrase-derived tag itself, so this service cannot
    // substitute a key of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap_client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap_client_key_tag: Option<String>,
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
    /// Claim-scoped errors name their claim so the claiming page shows
    /// the daemon's reason instead of timing out.
    #[serde(default)]
    claim_id: Option<String>,
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
            let _ = pending.response_tx.send(Err(body.error.clone()));
        }
    }
    if let Some(claim_id) = body
        .claim_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let mut claims = state.pending_claims.lock().await;
        if let Some(claim) = claims.get_mut(claim_id) {
            // Only the daemon the claim targets may reject it.
            if claim.daemon_id == body.daemon_id && matches!(claim.status, ClaimStatus::Pending) {
                claim.status = ClaimStatus::Rejected { error: body.error };
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct ClaimProofRequest {
    /// Which payload shape the signature covers. Absent/empty from daemons
    /// that predate the field — those always signed the v1 payload.
    #[serde(default)]
    protocol: String,
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
    let proof_protocol = if body.protocol.trim().is_empty() {
        // Daemons that predate the protocol field always signed v1.
        CLAIM_PROTOCOL
    } else {
        body.protocol.trim()
    };
    let payload = match proof_protocol {
        CLAIM_PROTOCOL => claim_signing_payload(
            &body.claim_id,
            &body.daemon_id,
            &daemon.daemon_public_key,
            &body.challenge,
        ),
        CLAIM_PROTOCOL_V2 => claim_signing_payload_v2(
            &body.claim_id,
            &body.daemon_id,
            &daemon.daemon_public_key,
            &body.challenge,
            &pending.user_id.to_string(),
            &pending.account_name,
        ),
        other => {
            reject_claim(&state, &body.claim_id, "unsupported claim proof protocol").await;
            return Err(ApiError::bad_request(format!(
                "unsupported claim proof protocol {other:?}"
            )));
        }
    };
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
            // v2 = the daemon co-signed the claiming account; v1 = the
            // binding rests on this service's account assertion alone.
            "proof": proof_protocol,
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

/// Mirrors `connect_rendezvous::claim_signing_payload_v2` in the daemon —
/// stable protocol, replicated rather than shared, like
/// [`orl_signing_payload`]. The account fields are the `PendingClaim`
/// snapshot, so a mid-claim handle rename cannot desync the two sides.
fn claim_signing_payload_v2(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
    user_id: &str,
    account_name: &str,
) -> String {
    format!(
        "{CLAIM_PROTOCOL_V2}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n{user_id}\n{account_name}\n"
    )
}

/// Mirrors `connect_rendezvous::unclaim_signing_payload` in the daemon.
fn unclaim_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!("{UNCLAIM_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n")
}

#[derive(Debug, Deserialize)]
struct DaemonUnclaimRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
}

/// Daemon-initiated release of a claim binding. This is the recovery path
/// the account side cannot provide: a squatted or mis-claimed box evicts
/// the binding with its own key (the account holder would never revoke).
/// The release is signed and timestamp-fresh, verified against the
/// *registered* daemon key, and logged to the transparency log like the
/// claim it undoes. A fresh claim code mints on the next register poll.
async fn daemon_unclaim(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonUnclaimRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_unclaim", 10, 60_000).await?;
    if body.protocol != UNCLAIM_PROTOCOL {
        return Err(ApiError::bad_request("unsupported unclaim protocol"));
    }
    let daemon_id = body.daemon_id.trim().to_string();
    let now = now_unix_ms();
    if now.abs_diff(body.issued_at_unix_ms) > UNCLAIM_MAX_SKEW_MS {
        return Err(ApiError::bad_request(
            "unclaim payload is stale — check the daemon clock and retry",
        ));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    // The signature must verify against the key this service has bound to
    // the daemon_id — the body copy only makes the signed payload
    // self-describing.
    if body.daemon_public_key.trim() != daemon.daemon_public_key {
        return Err(ApiError::bad_request(
            "daemon_public_key does not match the registered key",
        ));
    }
    let payload = unclaim_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("unclaim signature invalid"));
    }
    let Some(owner_user_id) = daemon.owner_user_id else {
        // Idempotent: releasing an unclaimed daemon is a no-op success, so
        // a daemon retrying after a lost response converges.
        return Ok(Json(json!({ "ok": true, "changed": false })));
    };
    let active_session_ids = active_dashboard_session_ids(&state, &daemon_id).await;
    let closed_sessions = active_session_ids.len();
    {
        let mut store = state.store.lock().await;
        let Some(record) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) else {
            return Err(ApiError::not_found("daemon not found"));
        };
        record.owner_user_id = None;
        record.claim_code_hash = None;
        record.claim_code_created_unix_ms = None;
        record.updated_unix_ms = now;
        store.fleet_targets.retain(|target| {
            !(target.user_id == owner_user_id
                && (target.host_id == daemon_id
                    || target.id == daemon_id
                    || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
        });
        let handle = store
            .users
            .iter()
            .find(|u| u.id == owner_user_id)
            .map(|u| u.account_name.clone())
            .unwrap_or_default();
        append_log_entry(
            &mut store,
            "daemon_unclaimed",
            json!({
                "daemon_id": daemon_id.clone(),
                "daemon_public_key": daemon.daemon_public_key.clone(),
                "handle": handle,
                "initiated_by": "daemon",
            }),
        );
        audit(
            &mut store,
            "daemon_unclaimed",
            Some(owner_user_id),
            Some(daemon_id.clone()),
            json!({ "initiated_by": "daemon", "closed_sessions": closed_sessions }),
        );
        persist_locked(&state, &store)?;
    }
    state.claim_codes.lock().await.remove(&daemon_id);
    close_active_dashboard_sessions(&state, &daemon_id, active_session_ids).await;
    log_json(
        "daemon_unclaimed",
        json!({ "daemon_id": daemon_id, "closed_sessions": closed_sessions }),
    );
    Ok(Json(json!({ "ok": true, "changed": true })))
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
    client_key_proto: Option<String>,
    #[serde(default)]
    client_key_account_user_id: Option<String>,
    #[serde(default)]
    client_key_account_name: Option<String>,
    #[serde(default)]
    org_grant: Option<serde_json::Value>,
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
            // v2 offer-signature fields, relayed verbatim like the key
            // itself: the daemon verifies the signature covers them, so
            // this service can neither mint nor alter an account claim.
            client_key_proto: body
                .client_key_proto
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_account_user_id: body
                .client_key_account_user_id
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_account_name: body
                .client_key_account_name
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
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

#[cfg(test)]
mod tests {
    use super::*;
    use bip39::Language;

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
            claim_code_daemon_minted: false,
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

    /// Pins the exact byte strings daemons sign. The daemon replicates
    /// these in `connect_rendezvous.rs` (same golden literals there) —
    /// a drift on either side fails one of the twin tests instead of
    /// shipping as an unverifiable signature.
    #[test]
    fn claim_and_unclaim_payloads_pin_the_wire_format() {
        assert_eq!(
            claim_signing_payload("claim-1", "daemon-1", "PubKey", "challenge-1"),
            "intendant-connect-claim-v1\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\n"
        );
        assert_eq!(
            claim_signing_payload_v2(
                "claim-1",
                "daemon-1",
                "PubKey",
                "challenge-1",
                "user-uuid-1",
                "lenny"
            ),
            "intendant-connect-claim-v2\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\nuser-uuid-1\nlenny\n"
        );
        assert_eq!(
            unclaim_signing_payload("daemon-1", "PubKey", 1_700_000_000_000),
            "intendant-connect-unclaim-v1\ndaemon-1\nPubKey\n1700000000000\n"
        );
    }

    /// The v2 property the whole exercise exists for: the signature is
    /// only valid for the account the daemon actually co-signed — a
    /// service (or relay) re-binding the proof to a different account
    /// fails verification.
    #[test]
    fn v2_claim_proof_signature_binds_the_claiming_account() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        use ring::signature::KeyPair as _;
        let public_key = b64u(key.public_key().as_ref());

        let signed_for_alice = claim_signing_payload_v2(
            "claim-1",
            "daemon-1",
            &public_key,
            "challenge-1",
            "alice-user-id",
            "alice",
        );
        let signature = b64u(key.sign(signed_for_alice.as_bytes()).as_ref());
        assert!(verify_ed25519_b64u(
            &public_key,
            signed_for_alice.as_bytes(),
            &signature
        ));

        let rebound_to_mallory = claim_signing_payload_v2(
            "claim-1",
            "daemon-1",
            &public_key,
            "challenge-1",
            "mallory-user-id",
            "mallory",
        );
        assert!(!verify_ed25519_b64u(
            &public_key,
            rebound_to_mallory.as_bytes(),
            &signature
        ));
    }

    /// Twin of the daemon's `claim_code_hash_matches_the_service_construction`
    /// (and the /connect page JS): one shared literal pins the hash across
    /// all three implementations.
    #[test]
    fn claim_code_hash_pins_the_cross_binary_literal() {
        assert_eq!(
            claim_code_hash("  Abandon ABILITY__able "),
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        );
        assert!(is_sha256_b64u(
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        ));
        assert!(!is_sha256_b64u("too-short"));
        assert!(!is_sha256_b64u(&format!(
            "{}=",
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        )));
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

}
