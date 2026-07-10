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
mod rendezvous;
pub(crate) use rendezvous::*;
mod dns;
pub(crate) use dns::*;

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
/// re-claim. Fleet-DNS publishes reuse the same window.
const UNCLAIM_MAX_SKEW_MS: u64 = 5 * 60 * 1000;
/// Daemon-signed fleet-DNS publishes: address records for the daemon's
/// own name, and short-lived ACME DNS-01 TXT tokens. The registered
/// identity key is the only authority over a name (docs/src/trust-tiers.md).
const DNS_PUBLISH_PROTOCOL: &str = "intendant-connect-dns-publish-v1";
const DNS_ACME_PROTOCOL: &str = "intendant-connect-dns-acme-v1";
/// Daemon-signed attention nudge: an agent→user request (approval /
/// question) went unseen on the box, so this service fans a Web Push out to
/// the owner's opted-in browsers. The signed body names a request KIND and
/// a session display LABEL only — never work content (push.rs).
const NOTIFY_PROTOCOL: &str = "intendant-connect-daemon-notify-v1";
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
    // Fleet DNS: build the zone, hydrate persisted records, and bind the
    // sockets BEFORE serving — a misconfigured DNS listener must fail the
    // whole startup loudly, not limp along HTTP-only.
    let dns_zone = match (&config.dns_zone, &config.dns_ns_name, &config.dns_listen) {
        (Some(zone_name), Some(ns_name), Some(_listen)) => {
            let zone = Arc::new(FleetZone::new(zone_name, ns_name)?);
            for entry in &store.dns_records {
                let addresses: Vec<std::net::IpAddr> = entry
                    .addresses
                    .iter()
                    .filter_map(|value| value.parse().ok())
                    .collect();
                if let Err(error) = zone.set_daemon_addresses(&entry.daemon_id, &addresses) {
                    eprintln!(
                        "[connect] skipping persisted dns record for {}: {error}",
                        entry.daemon_id
                    );
                }
            }
            Some(zone)
        }
        _ => None,
    };
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
        dns_zone,
    });

    tokio::spawn(presence_alert_monitor(state.clone()));
    tokio::spawn(handle_reclaim_monitor(state.clone()));
    if let (Some(zone), Some(listen)) = (state.dns_zone.clone(), state.config.dns_listen) {
        // Fail startup on an unbindable DNS listener (privileges, port
        // in use); afterwards the server runs until process exit.
        let server = bind_fleet_dns(zone, listen).await?;
        eprintln!(
            "[connect] fleet dns serving {} on {listen} (udp+tcp)",
            state.config.dns_zone.as_deref().unwrap_or_default()
        );
        tokio::spawn(async move {
            if let Err(error) = server.await {
                eprintln!("[connect] fleet dns server exited: {error}");
            }
        });
    }

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
        .route("/api/push/subscriptions", get(push_subscriptions))
        .route("/api/push/preferences", post(push_preferences))
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
        .route("/api/daemon/notify", post(daemon_notify))
        .route("/api/dns/publish", post(dns_publish))
        .route("/api/dns/acme-challenge", post(dns_acme_challenge))
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
    /// Fleet DNS (docs/src/self-hosted-rendezvous.md): the delegated
    /// subzone this service answers for authoritatively (e.g.
    /// `fleet.intendant.dev`). All three `dns_*` values must be set for
    /// the DNS server and publish endpoints to switch on; default off.
    dns_zone: Option<String>,
    /// The zone's NS host as delegated in the parent zone (e.g.
    /// `ns-fleet.intendant.dev`) — served in the apex SOA/NS records.
    dns_ns_name: Option<String>,
    /// UDP+TCP listen address for the DNS server (e.g. `0.0.0.0:53`;
    /// binding :53 unprivileged needs CAP_NET_BIND_SERVICE).
    dns_listen: Option<SocketAddr>,
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
        let mut dns_zone = std::env::var("INTENDANT_CONNECT_DNS_ZONE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let mut dns_ns_name = std::env::var("INTENDANT_CONNECT_DNS_NS_NAME")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let mut dns_listen: Option<SocketAddr> = match std::env::var("INTENDANT_CONNECT_DNS_LISTEN")
        {
            Ok(value) if !value.trim().is_empty() => Some(
                value
                    .trim()
                    .parse()
                    .map_err(|e| format!("invalid INTENDANT_CONNECT_DNS_LISTEN {value:?}: {e}"))?,
            ),
            _ => None,
        };

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
                "--dns-zone" => {
                    dns_zone = Some(args.next().ok_or("--dns-zone requires a zone name")?);
                }
                "--dns-ns-name" => {
                    dns_ns_name = Some(args.next().ok_or("--dns-ns-name requires a host name")?);
                }
                "--dns-listen" => {
                    let value = args.next().ok_or("--dns-listen requires an address")?;
                    dns_listen = Some(
                        value
                            .parse()
                            .map_err(|e| format!("invalid --dns-listen {value:?}: {e}"))?,
                    );
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
        // Fleet DNS is all-or-nothing: a partial config would serve a
        // zone nobody delegated or delegate a zone nobody serves.
        let dns_parts = [
            dns_zone.is_some(),
            dns_ns_name.is_some(),
            dns_listen.is_some(),
        ];
        if dns_parts.iter().any(|set| *set) && !dns_parts.iter().all(|set| *set) {
            return Err(
                "fleet dns needs all of --dns-zone, --dns-ns-name, and --dns-listen (or none)"
                    .to_string(),
            );
        }
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
            dns_zone,
            dns_ns_name,
            dns_listen,
        })
    }
}

fn print_help() {
    println!(
        "Usage: intendant-connect [--listen 127.0.0.1:9876] [--origin https://connect.intendant.dev] [--rp-id intendant.dev]\n\
         \n\
         Env: INTENDANT_CONNECT_LISTEN, INTENDANT_CONNECT_ORIGIN, INTENDANT_CONNECT_RP_ID,\n\
              INTENDANT_CONNECT_STATIC_ROOT, INTENDANT_CONNECT_DATA_FILE, INTENDANT_CONNECT_TOKEN,\n\
              INTENDANT_CONNECT_INVITE_REQUIRED, INTENDANT_CONNECT_OPEN_REGISTRATION,\n\
              INTENDANT_CONNECT_DNS_ZONE, INTENDANT_CONNECT_DNS_NS_NAME, INTENDANT_CONNECT_DNS_LISTEN\n\
              (--dns-zone fleet.example.com --dns-ns-name ns-fleet.example.com --dns-listen 0.0.0.0:53\n\
               enable the embedded fleet DNS server; all three or none)"
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
    /// The fleet DNS zone when the `dns_*` config group is set — the
    /// live record table the embedded server answers from (hydrated
    /// from `Store::dns_records` at startup).
    dns_zone: Option<Arc<FleetZone>>,
}

/// A daemon's published fleet-DNS addresses (`d-<label>.<zone>` A/AAAA).
/// ACME TXT challenges are deliberately NOT persisted — they live only
/// in the in-memory zone and self-expire.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DnsRecordEntry {
    daemon_id: String,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    users: Vec<UserRecord>,
    #[serde(default)]
    daemons: Vec<DaemonRecord>,
    #[serde(default)]
    dns_records: Vec<DnsRecordEntry>,
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
    /// Alert when an agent→user request (approval / question) goes unseen
    /// on a claimed daemon. Opt-in; absent on pre-existing records = false.
    #[serde(default)]
    notify_requests: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

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

}
