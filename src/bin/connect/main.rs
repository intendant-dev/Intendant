use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex, Notify};
use url::Url;
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
const REGISTER_PROOF_PROTOCOL: &str = "intendant-connect-register-proof-v1";
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
const REGISTER_PROOF_MAX_SKEW_MS: u64 = 5 * 60 * 1000;
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
const CLAIM_TIMEOUT_MS: u64 = 60_000;
const CLAIM_CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// Registration rotates this credential every minute. Five minutes covers a
/// lost refresh without turning it into a durable daemon secret.
const DAEMON_SESSION_TTL_MS: u64 = 5 * 60 * 1000;
const ACTIVE_DASHBOARD_SESSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CSRF_HEADER: &str = "x-intendant-csrf";
const DAEMON_SESSION_HEADER: &str = "x-intendant-daemon-session";
const FLEET_TARGET_LIMIT: usize = 100;
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
    // Code transparency: commit what this process will serve before it
    // serves anything (docs/src/self-hosted-rendezvous.md). Appends only
    // when the manifest changed since the last logged one.
    let manifest_logged = record_artifact_manifest(&mut store, &config);
    if !had_keys || manifest_logged {
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
        daemon_sessions: Mutex::new(HashMap::new()),
        rate_limits: Mutex::new(RateLimitTable::default()),
        active_sessions: Mutex::new(HashMap::new()),
        store_dirty: StoreDirty::default(),
        log_caches: std::sync::Mutex::new(LogCaches::default()),
        static_pages: StaticPages::render(&config),
        dns_zone,
    });

    tokio::spawn(presence_alert_monitor(state.clone()));
    tokio::spawn(handle_reclaim_monitor(state.clone()));
    tokio::spawn(store_flush_monitor(state.clone()));
    tokio::spawn(in_memory_state_sweeper(state.clone()));
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

    let app = connect_router(state.clone());

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "[connect] listening on http://{} with origin {} rp_id {}",
        config.listen, config.public_origin, config.rp_id
    );
    eprintln!("[connect] state file {}", config.data_file.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Deploy-time restarts land here: without a final flush, every restart
    // would discard the pending debounce window (a daemon that went offline
    // during it would permanently lose its last presence hours).
    final_store_flush(&state).await;
    Ok(())
}

/// Resolves on SIGTERM (what systemd/deploy tooling sends) or ctrl-c. A
/// failed signal-handler registration degrades to the other signal rather
/// than aborting startup.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = sigterm.recv() => {}
                    result = tokio::signal::ctrl_c() => {
                        if let Err(err) = result {
                            eprintln!("[connect] ctrl-c handler failed: {err}");
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("[connect] SIGTERM handler registration failed: {err}");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    eprintln!("[connect] ctrl-c handler failed: {err}");
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            eprintln!("[connect] ctrl-c handler failed: {err}");
            std::future::pending::<()>().await;
        }
    }
}

/// Synchronously flush any pending debounced marks; called once after the
/// server drains on shutdown. Failure re-marks for invariant uniformity
/// (the process is exiting, but the flag state stays truthful).
async fn final_store_flush(state: &AppState) {
    let store = state.store.lock().await;
    if !state.store_dirty.take() {
        return;
    }
    if let Err(err) = save_store(&state.config.data_file, &store) {
        eprintln!("[connect] final store flush failed: {err}");
        state.store_dirty.mark();
    }
}

/// The complete production HTTP surface. Startup and route-boundary tests use
/// this same constructor so a new route or fallback cannot bypass the hosted
/// static/control cutoff without changing the exercised router.
fn connect_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(landing_ui))
        .route("/connect", get(connect_ui))
        .route("/access", get(access_ui))
        .route("/app", get(app_html))
        .route("/app.html", get(app_html))
        .route("/healthz", get(healthz))
        .route("/install.sh", get(install_sh))
        .route("/install.ps1", get(install_ps1))
        .route("/favicon.png", get(favicon_png))
        .route("/logo.svg", get(logo_svg))
        .route("/sw.js", get(service_worker_js))
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
        .route(
            "/api/log/artifact-manifest",
            get(log_artifact_manifest).options(orl_preflight),
        )
        .route(
            "/api/log/release-manifest",
            get(log_release_manifest)
                .post(release_manifest_submit)
                .options(orl_preflight),
        )
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
        .route(
            "/api/daemon/register",
            post(daemon_register).layer(DefaultBodyLimit::max(4 * 1024)),
        )
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
        .fallback(not_found)
        .with_state(state)
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    listen: SocketAddr,
    public_origin: String,
    rp_id: String,
    data_file: PathBuf,
    daemon_token: Option<String>,
    /// Bearer token for release-manifest submissions (the release
    /// pipeline's credential). Deliberately separate from the operator
    /// `daemon_token`: a CI secret that can only append release
    /// manifests to the public log must not double as the admin key.
    /// Unset = the submission endpoint answers 503 (reads stay public).
    release_token: Option<String>,
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
        // Deprecated compatibility input. Connect deliberately serves no
        // daemon-dashboard files from disk; retain the env/flag parser only
        // so existing deployment commands do not fail during migration.
        let _deprecated_static_root = std::env::var("INTENDANT_CONNECT_STATIC_ROOT").ok();
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
        let mut release_token = std::env::var("INTENDANT_CONNECT_RELEASE_TOKEN")
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
        let mut dns_listen: Option<SocketAddr> =
            match std::env::var("INTENDANT_CONNECT_DNS_LISTEN") {
                Ok(value) if !value.trim().is_empty() => {
                    Some(value.trim().parse().map_err(|e| {
                        format!("invalid INTENDANT_CONNECT_DNS_LISTEN {value:?}: {e}")
                    })?)
                }
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
                    let _ = args.next().ok_or("--static-root requires a path")?;
                }
                "--data-file" => {
                    data_file = PathBuf::from(args.next().ok_or("--data-file requires a path")?);
                }
                "--daemon-token" => {
                    daemon_token = Some(args.next().ok_or("--daemon-token requires a token")?);
                }
                "--release-token" => {
                    release_token = Some(args.next().ok_or("--release-token requires a token")?);
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
            data_file,
            daemon_token,
            release_token,
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
         Deprecated compatibility: --static-root PATH and INTENDANT_CONNECT_STATIC_ROOT are accepted but ignored;\n\
         Connect serves only its embedded discovery pages/assets and never the daemon dashboard SPA.\n\
         \n\
         Env: INTENDANT_CONNECT_LISTEN, INTENDANT_CONNECT_ORIGIN, INTENDANT_CONNECT_RP_ID,\n\
              INTENDANT_CONNECT_STATIC_ROOT, INTENDANT_CONNECT_DATA_FILE, INTENDANT_CONNECT_TOKEN,\n\
              INTENDANT_CONNECT_RELEASE_TOKEN (--release-token: gates POST /api/log/release-manifest),\n\
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
    daemon_sessions: Mutex<HashMap<String, DaemonSessionCredential>>,
    rate_limits: Mutex<RateLimitTable>,
    active_sessions: Mutex<HashMap<String, ActiveDashboardSession>>,
    /// Dirty flag + wakeup for the debounced store flusher: hot paths that
    /// only refresh presence-grade fields mark instead of persisting.
    store_dirty: StoreDirty,
    /// Derived read caches over the append-only transparency log
    /// (transparency.rs). std Mutex: held only for short synchronous
    /// extends, never across an await.
    log_caches: std::sync::Mutex<LogCaches>,
    /// Startup-rendered static pages (ui.rs): pure functions of the public
    /// origin, served as shared bytes with ETag revalidation instead of
    /// re-rendered per hit.
    static_pages: StaticPages,
    vapid: ring::signature::EcdsaKeyPair,
    log_key: ring::signature::EcdsaKeyPair,
    push_http: reqwest::Client,
    /// The fleet DNS zone when the `dns_*` config group is set — the
    /// live record table the embedded server answers from (hydrated
    /// from `Store::dns_records` at startup).
    dns_zone: Option<Arc<FleetZone>>,
}

/// Minimal production-shaped state for route-boundary tests. Tests seed the
/// durable store they need, while sharing the same WebAuthn, signing-key, and
/// in-memory state construction as the service.
#[cfg(test)]
fn production_router_test_state(root: &Path, mut store: Store) -> Arc<AppState> {
    let config = ServiceConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public_origin: "https://connect.example.test".to_string(),
        rp_id: "example.test".to_string(),
        data_file: root.join("state.json"),
        daemon_token: None,
        release_token: None,
        cookie_secure: true,
        invite_required: false,
        open_daemon_registration: false,
        dns_zone: None,
        dns_ns_name: None,
        dns_listen: None,
    };
    let webauthn = Webauthn::new(&config.rp_id, "Intendant Connect", &config.public_origin)
        .require_user_verification(true)
        .strict_base64(true);
    let vapid = load_or_create_vapid_keypair(&mut store).unwrap();
    let log_key = load_or_create_log_keypair(&mut store).unwrap();
    let static_pages = StaticPages::render(&config);
    Arc::new(AppState {
        config,
        webauthn,
        store: Mutex::new(store),
        sessions: Mutex::new(HashMap::new()),
        pending_registrations: Mutex::new(HashMap::new()),
        pending_authentications: Mutex::new(HashMap::new()),
        pending_offers: Mutex::new(HashMap::new()),
        pending_claims: Mutex::new(HashMap::new()),
        event_queues: Mutex::new(HashMap::new()),
        event_notify: Notify::new(),
        daemon_sessions: Mutex::new(HashMap::new()),
        rate_limits: Mutex::new(RateLimitTable::default()),
        active_sessions: Mutex::new(HashMap::new()),
        store_dirty: StoreDirty::default(),
        log_caches: std::sync::Mutex::new(LogCaches::default()),
        static_pages,
        vapid,
        log_key,
        push_http: reqwest::Client::new(),
        dns_zone: None,
    })
}

struct DaemonSessionCredential {
    token: String,
    expires_unix_ms: u64,
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
    /// Connect account association for discovery/routing. The legacy
    /// field name is retained for store compatibility; it is not a daemon
    /// IAM owner and carries no daemon authority.
    owner_user_id: Option<Uuid>,
    claim_code_hash: Option<String>,
    claim_code_created_unix_ms: Option<u64>,
    /// Latest accepted signed registration timestamp. Equal replays are
    /// idempotent only when they carry the same code hash; older proofs can
    /// never restore a superseded route code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_registration_proof_unix_ms: Option<u64>,
    /// Monotonic route-association generation. Claim and release mutations
    /// increment it so a delayed signed release cannot unlink a newer claim.
    #[serde(default)]
    route_link_revision: u64,
    /// Latest consumed daemon-signed release timestamp. Persisting it makes a
    /// captured release single-use across service restarts and later claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_unclaim_proof_unix_ms: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
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
    // Owner-set trust tier (docs/src/trust-tiers.md): part of the signed
    // v4 record payload, relayed verbatim. The service never interprets
    // it — clients verify it under the record signature.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    tier: String,
    // Owner-chosen petname (signed v5 line): the anti-lookalike name.
    // Same relay-blind discipline.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    petname: String,
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
    /// The bucket's own window (scope-stable: every call site uses one
    /// window per scope). Expiry decisions use this, never a global bound —
    /// a short-window bucket must not rank as live under a longer scope's
    /// lifetime, and vice versa.
    window_ms: u64,
}

/// The rate-limit table plus the bookkeeping that amortizes full-scan
/// pruning when the table sits at capacity.
#[derive(Default)]
struct RateLimitTable {
    buckets: HashMap<String, RateLimitBucket>,
    /// When the last at-capacity prune ran; saturated inserts within the
    /// interval skip the O(table) scan and fail closed directly.
    last_saturated_prune_unix_ms: u64,
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
    /// Daemon identity key snapshot for this code generation. Key rotation
    /// invalidates the pending association just like code rotation does.
    daemon_public_key: String,
    challenge: String,
    created_unix_ms: u64,
    /// Exact one-time code generation this claim started against. A later
    /// proof cannot win after the code was rotated, consumed, or claimed
    /// by a competing account.
    claim_code_hash: String,
    claim_code_created_unix_ms: u64,
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

/// Serialize the store for disk. Compact JSON: the file is machine-read
/// (serde ignores whitespace in both directions, so state files written by
/// older pretty-printing builds load unchanged and older builds read compact
/// files), and pretty printing roughly doubled the bytes of a file that is
/// rewritten in full on every persist.
fn serialize_store(store: &Store) -> Result<Vec<u8>, String> {
    serde_json::to_vec(store).map_err(|e| format!("serialize state: {e}"))
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    write_store_bytes(path, &serialize_store(store)?)
}

fn write_store_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
    tmp.write_all(bytes)
        .map_err(|e| format!("write Connect state tempfile {}: {e}", tmp.path().display()))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|e| format!("sync Connect state tempfile {}: {e}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| format!("replace Connect state {}: {}", path.display(), e.error))?;
    fsync_parent_dir(parent)
}

/// fsync the directory entry so the rename that just published the new state
/// file is itself durable — without it, a crash right after the rename can
/// roll the whole replacement back on some filesystems. Directories are not
/// open-and-fsync-able on Windows; there the pre-rename file sync plus the
/// atomic ReplaceFile stand alone.
fn fsync_parent_dir(parent: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| format!("sync Connect state dir {}: {e}", parent.display()))?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

/// Persist the current store synchronously, before the caller's response.
/// Callers hold the store lock, so the bytes are a consistent snapshot and
/// the write covers anything the debounced flusher had pending — hence the
/// dirty flag is cleared on success (marks only ever happen under the same
/// lock, so none can slip between the write and the clear).
fn persist_locked(state: &AppState, store: &Store) -> ApiResult<()> {
    save_store(&state.config.data_file, store).map_err(ApiError::internal)?;
    state.store_dirty.clear();
    Ok(())
}

/// Debounced-persistence signal. Hot paths whose mutations tolerate a
/// bounded loss window (presence refreshes: `last_seen`, presence hours, the
/// registration-proof watermark) `mark` instead of persisting;
/// `store_flush_monitor` coalesces the marks into one full-store write per
/// debounce window. Critical mutations keep their synchronous
/// `persist_locked` path unchanged.
#[derive(Default)]
struct StoreDirty {
    dirty: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl StoreDirty {
    fn mark(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_one();
    }

    fn clear(&self) {
        self.dirty
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn take(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

/// Mark the store dirty for the debounced flusher. MUST be called while
/// holding the store lock: the mark records a mutation just made under that
/// lock, and the lock is also what orders marks against `persist_locked`'s
/// clear-on-success.
fn mark_store_dirty(state: &AppState) {
    state.store_dirty.mark();
}

/// Upper bound on how much presence-grade mutation a crash can lose, and the
/// whole-service write cadence under steady daemon polling: one coalesced
/// write per window instead of several fsynced full-store rewrites per
/// daemon-minute.
const STORE_FLUSH_DEBOUNCE: Duration = Duration::from_secs(60);

async fn store_flush_monitor(state: Arc<AppState>) {
    store_flush_monitor_with(state, STORE_FLUSH_DEBOUNCE).await;
}

/// Debounce-loop body, with the window injectable so tests need not wait the
/// production 60s. The write happens with the store lock HELD (like every
/// synchronous persist): a single writer ordering means the file can never
/// regress to an older snapshot written late. `spawn_blocking` keeps the
/// blocking file I/O off the async workers; store waiters queue exactly as
/// they do for a synchronous persist today, once per window instead of per
/// request.
async fn store_flush_monitor_with(state: Arc<AppState>, debounce: Duration) {
    loop {
        state.store_dirty.notify.notified().await;
        tokio::time::sleep(debounce).await;
        let store = state.store.lock().await;
        if !state.store_dirty.take() {
            // A synchronous persist already covered the mark.
            continue;
        }
        let bytes = match serialize_store(&store) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("[connect] debounced store flush serialize failed: {err}");
                // Uniform retry invariant: every failure arm re-marks — the
                // mutations are still memory-only, and take() consumed the
                // mark.
                state.store_dirty.mark();
                continue;
            }
        };
        let path = state.config.data_file.clone();
        match tokio::task::spawn_blocking(move || write_store_bytes(&path, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                eprintln!("[connect] debounced store flush failed (will retry): {err}");
                // The mutations are still memory-only: re-mark so the next
                // window retries even without new activity.
                state.store_dirty.mark();
            }
            Err(err) => {
                eprintln!("[connect] debounced store flush task failed: {err}");
                state.store_dirty.mark();
            }
        }
        drop(store);
    }
}

/// How long a resolved (approved / rejected / timed-out) claim stays
/// queryable by the polling browser before the sweeper drops it. Claims
/// resolve or time out within `CLAIM_TIMEOUT_MS` and the claiming page polls
/// every second or two, so this is generous observation headroom.
const PENDING_CLAIM_RETENTION_MS: u64 = 15 * 60 * 1000;
const IN_MEMORY_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Periodic expiry sweep across the in-memory tables, none of which
/// otherwise remove entries except opportunistically (an expired session was
/// only dropped when its exact token was re-presented; abandoned WebAuthn
/// flows, resolved claims, and expired rate windows never were). Every rule
/// removes only entries already dead to their consumers, so the sweep never
/// changes what any live request observes.
async fn in_memory_state_sweeper(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(IN_MEMORY_SWEEP_INTERVAL).await;
        sweep_in_memory_state(&state).await;
    }
}

async fn sweep_in_memory_state(state: &AppState) {
    let now = now_unix_ms();
    state
        .sessions
        .lock()
        .await
        .retain(|_, session| session.expires_unix_ms > now);
    state
        .pending_registrations
        .lock()
        .await
        .retain(|_, pending| pending.expires_unix_ms > now);
    state
        .pending_authentications
        .lock()
        .await
        .retain(|_, pending| pending.expires_unix_ms > now);
    state
        .pending_claims
        .lock()
        .await
        .retain(|_, claim| now.saturating_sub(claim.created_unix_ms) <= PENDING_CLAIM_RETENTION_MS);
    state
        .daemon_sessions
        .lock()
        .await
        .retain(|_, session| session.expires_unix_ms > now);
    state.active_sessions.lock().await.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    prune_rate_limits(&mut state.rate_limits.lock().await.buckets, now);
}

/// Apply a durable store mutation transactionally: serialize/write the cloned
/// next state first, then publish it to the in-memory store. A failed disk
/// write therefore cannot leave memory ahead of durable state and poison an
/// otherwise valid retry (notably account/daemon route release).
fn update_store_transaction<R>(
    store: &mut Store,
    mutate: impl FnOnce(&mut Store) -> ApiResult<R>,
    persist: impl FnOnce(&Store) -> ApiResult<()>,
) -> ApiResult<R> {
    let mut next = store.clone();
    let result = mutate(&mut next)?;
    persist(&next)?;
    *store = next;
    Ok(result)
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
    let online = now.saturating_sub(daemon.last_seen_unix_ms) < 45_000;
    json!({
        "id": daemon.daemon_id,
        "host_id": daemon.daemon_id,
        "label": label,
        "local": false,
        "source": "connect_daemon",
        "access_domain": "route_metadata",
        "access_domain_label": "Route metadata only",
        "route": "hosted_connect",
        "route_label": "Hosted Connect",
        "auth": "none",
        "auth_label": "No daemon authentication",
        "effective_role": "none",
        "effective_role_label": "No access",
        "profile": "",
        "connected": online,
        "online": online,
        "claimed_daemon": true,
        "daemon_public_key": daemon.daemon_public_key,
        // Route/account metadata is deliberately not an openable control
        // target in the default hosted build.
        "url": "",
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
        "tier": target.tier,
        "petname": target.petname,
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

    fn store_with_marker() -> Store {
        let mut store = Store::default();
        store.users.push(UserRecord {
            id: Uuid::new_v4(),
            account_name: "alice".to_string(),
            display_name: "Alice".to_string(),
            passkeys: Vec::new(),
            created_unix_ms: 1,
            updated_unix_ms: 2,
            last_login_unix_ms: 3,
            attestations: Vec::new(),
        });
        append_log_entry(&mut store, "account_created", json!({ "handle": "alice" }));
        store
    }

    /// The deployed instance's state file was written pretty-printed; the
    /// compact writer and the pretty reader must both round-trip it, so a
    /// binary from either side of the transition loads the other's file.
    #[test]
    fn store_format_is_forward_and_backward_compatible_across_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_marker();

        // Legacy pretty file (what the deployed build wrote) loads cleanly.
        let legacy = dir.path().join("legacy.json");
        std::fs::write(&legacy, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        let loaded_legacy = load_store(&legacy).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded_legacy).unwrap(),
            serde_json::to_value(&store).unwrap()
        );

        // The compact writer round-trips, and its bytes stay valid JSON an
        // older (pretty-writing) serde build parses identically.
        let compact = dir.path().join("state.json");
        save_store(&compact, &store).unwrap();
        let loaded_compact = load_store(&compact).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded_compact).unwrap(),
            serde_json::to_value(&store).unwrap()
        );
    }

    #[test]
    fn store_dirty_marks_take_once_and_clear() {
        let dirty = StoreDirty::default();
        assert!(!dirty.take(), "starts clean");
        dirty.mark();
        assert!(dirty.take(), "mark is observable");
        assert!(!dirty.take(), "take consumes the mark");
        dirty.mark();
        dirty.clear();
        assert!(!dirty.take(), "a synchronous persist clears pending marks");
    }

    #[tokio::test]
    async fn persist_locked_clears_the_debounce_mark() {
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), store_with_marker());
        let store = state.store.lock().await;
        mark_store_dirty(&state);
        persist_locked(&state, &store).unwrap();
        assert!(
            !state.store_dirty.take(),
            "successful sync persist covers pending marks"
        );
        let on_disk = load_store(&state.config.data_file).unwrap();
        assert_eq!(on_disk.users.len(), 1);
    }

    #[tokio::test]
    async fn store_flush_monitor_writes_marked_state_once_debounced() {
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), store_with_marker());
        tokio::spawn(store_flush_monitor_with(
            state.clone(),
            Duration::from_millis(10),
        ));
        {
            let mut store = state.store.lock().await;
            store.daemons.push(DaemonRecord {
                daemon_id: "daemon-flush".to_string(),
                label: None,
                daemon_public_key: "key".to_string(),
                owner_user_id: None,
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                last_registration_proof_unix_ms: None,
                route_link_revision: 0,
                last_unclaim_proof_unix_ms: None,
                registered_unix_ms: 1,
                last_seen_unix_ms: 1,
                updated_unix_ms: 1,
                presence_hours: Vec::new(),
            });
            mark_store_dirty(&state);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(on_disk) = load_store(&state.config.data_file) {
                if on_disk
                    .daemons
                    .iter()
                    .any(|d| d.daemon_id == "daemon-flush")
                {
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "debounced flush never reached disk"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !state.store_dirty.take(),
            "flusher consumed the mark it wrote"
        );
    }

    /// Deploy-time restarts must not discard the pending debounce window:
    /// the graceful-shutdown path flushes marked state once, and a clean
    /// flag writes nothing.
    #[tokio::test]
    async fn final_store_flush_covers_the_pending_window() {
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), Store::default());

        // Clean flag: nothing to write (the store file does not appear).
        final_store_flush(&state).await;
        assert!(!state.config.data_file.exists());

        {
            let mut store = state.store.lock().await;
            store.users.push(UserRecord {
                id: Uuid::new_v4(),
                account_name: "late-window".to_string(),
                display_name: "Late".to_string(),
                passkeys: Vec::new(),
                created_unix_ms: 1,
                updated_unix_ms: 1,
                last_login_unix_ms: 1,
                attestations: Vec::new(),
            });
            mark_store_dirty(&state);
        }
        final_store_flush(&state).await;
        let on_disk = load_store(&state.config.data_file).unwrap();
        assert_eq!(on_disk.users.len(), 1);
        assert_eq!(on_disk.users[0].account_name, "late-window");
        assert!(!state.store_dirty.take(), "flush consumed the mark");
    }

    #[tokio::test]
    async fn sweeper_drops_only_entries_dead_to_their_consumers() {
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), Store::default());
        let now = now_unix_ms();
        {
            let mut sessions = state.sessions.lock().await;
            sessions.insert(
                "live".to_string(),
                SessionRecord {
                    user_id: Uuid::new_v4(),
                    csrf_token: "c".to_string(),
                    expires_unix_ms: now + 60_000,
                },
            );
            sessions.insert(
                "expired".to_string(),
                SessionRecord {
                    user_id: Uuid::new_v4(),
                    csrf_token: "c".to_string(),
                    expires_unix_ms: now - 1,
                },
            );
        }
        {
            let mut claims = state.pending_claims.lock().await;
            let claim = PendingClaim {
                user_id: Uuid::new_v4(),
                account_name: "alice".to_string(),
                daemon_id: "daemon-1".to_string(),
                daemon_public_key: "key".to_string(),
                challenge: "ch".to_string(),
                created_unix_ms: now,
                claim_code_hash: "h".to_string(),
                claim_code_created_unix_ms: now,
                status: ClaimStatus::Pending,
            };
            claims.insert("fresh".to_string(), claim.clone());
            claims.insert(
                "ancient".to_string(),
                PendingClaim {
                    created_unix_ms: now - PENDING_CLAIM_RETENTION_MS - 1,
                    ..claim
                },
            );
        }
        {
            let mut limits = state.rate_limits.lock().await;
            limits.buckets.insert(
                "hourly:1.2.3.4".to_string(),
                RateLimitBucket {
                    window_start_unix_ms: now - 120_000,
                    count: 3,
                    window_ms: 60 * 60_000,
                },
            );
            limits.buckets.insert(
                "short:5.6.7.8".to_string(),
                RateLimitBucket {
                    window_start_unix_ms: now - 120_000,
                    count: 900,
                    window_ms: 60_000,
                },
            );
        }
        sweep_in_memory_state(&state).await;
        let sessions = state.sessions.lock().await;
        assert!(sessions.contains_key("live"));
        assert!(!sessions.contains_key("expired"));
        let claims = state.pending_claims.lock().await;
        assert!(claims.contains_key("fresh"));
        assert!(!claims.contains_key("ancient"));
        let limits = state.rate_limits.lock().await;
        assert!(
            limits.buckets.contains_key("hourly:1.2.3.4"),
            "a bucket inside its own window stays"
        );
        assert!(
            !limits.buckets.contains_key("short:5.6.7.8"),
            "a bucket past its own window is expired even when younger than other scopes' windows"
        );
    }
}
