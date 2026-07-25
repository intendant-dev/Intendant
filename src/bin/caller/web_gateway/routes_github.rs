//! GitHub App integration configuration over HTTP (Track PR): seal the
//! App credentials into custody, keep the non-secret watch list in
//! `[integrations.github]`, answer status without ever unsealing.
//! Save-time verification is one real exchange — mint the JWT, exchange
//! it for an installation token, and (when a watch list exists) list
//! one repo's open PRs, because a token mint alone does not prove the
//! `Pull requests: read` permission. Custody access rides a small seam
//! so tests drive a tempdir backend — never the OS keystore, never live
//! GitHub.

use super::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Bound on the configured watch list — matches the agenda's 32-cap
/// idiom for owner-entered lists.
const MAX_WATCHED_REPOS: usize = 32;

/// The custody seam the cores speak. The one production implementation
/// delegates to `key_custody`; tests substitute a tempdir-backed one.
pub(crate) trait GithubAppCustody: Send + Sync {
    fn present(&self) -> bool;
    fn backend_available(&self) -> bool;
    /// Unsealed credentials document, `None` when absent OR denied (the
    /// implementation audits denies by name; callers stay generic).
    fn retrieve(&self) -> Option<Vec<u8>>;
    fn store(&self, material: &[u8], actor: &str, origin: &str) -> Result<(), String>;
    fn remove(&self, actor: &str, origin: &str) -> Result<(), String>;
}

/// Production custody: the daemon-global estate in `key_custody`.
pub(crate) struct DaemonGithubAppCustody;

impl GithubAppCustody for DaemonGithubAppCustody {
    fn present(&self) -> bool {
        crate::key_custody::github_app_in_custody()
    }
    fn backend_available(&self) -> bool {
        crate::key_custody::custody_backend_available()
    }
    fn retrieve(&self) -> Option<Vec<u8>> {
        crate::key_custody::github_app_from_custody().map(|secret| secret.as_bytes().to_vec())
    }
    fn store(&self, material: &[u8], actor: &str, origin: &str) -> Result<(), String> {
        crate::key_custody::store_github_app(material, actor, origin)
    }
    fn remove(&self, actor: &str, origin: &str) -> Result<(), String> {
        crate::key_custody::remove_github_app(actor, origin)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubSavePayload {
    app_id: String,
    installation_id: u64,
    /// Absent on an ids/watch-list-only update of an existing entry.
    #[serde(default)]
    private_key_pem: Option<String>,
    /// Absent = leave the configured list unchanged.
    #[serde(default)]
    repos: Option<Vec<String>>,
    /// Absent = leave unchanged; floor 1 enforced at intake.
    #[serde(default)]
    poll_minutes: Option<u64>,
}

/// `"owner/repo"` — exactly one slash, both halves in GitHub's name
/// alphabet. Anything else is a typo the status surface would otherwise
/// carry forever.
fn validate_repo_name(repo: &str) -> Result<(), String> {
    let mut parts = repo.split('/');
    let (owner, name) = match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => (owner, name),
        _ => return Err(format!("repo {repo:?} is not \"owner/repo\"")),
    };
    let half_ok = |half: &str| {
        !half.is_empty()
            && half
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !half_ok(owner) || !half_ok(name) {
        return Err(format!("repo {repo:?} is not \"owner/repo\""));
    }
    Ok(())
}

/// Method-exact membership test for the certless carve-out
/// ([`super::access_gates::allows_remote_certless_http`]): GitHub's
/// manifest redirect is a top-level browser navigation carrying no
/// daemon credential — the daemon-minted single-use state in the query
/// is the entire authorization, so this one GET sits in the doorbell
/// class beside peer pairing, org grants, and Codex Cloud enrollment.
/// (A `RouteAuthz::Public` table row alone does NOT exempt a path from
/// the mTLS/origin gates — both memberships are required.)
pub(crate) fn is_public_github_manifest_callback_path(request_line: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    method == "GET" && path == crate::github_pr::manifest_ceremony::CALLBACK_PATH
}

fn integration_config(settings_root: Option<&Path>) -> crate::project::GithubIntegrationConfig {
    let Some(root) = settings_root else {
        return Default::default();
    };
    crate::project::Project::from_root(root.to_path_buf())
        .map(|proj| proj.config.integrations.github.clone())
        .unwrap_or_default()
}

/// The shared status body every verb answers with — presence, the wire
/// status label, the last exchange, and the non-secret config. Never
/// touches the keystore. `pending_install` is a status-body boolean
/// beside the ruled label vocabulary, never a new label in it: the
/// label describes exchange health, the boolean describes the ceremony
/// phase, and the two stay orthogonal. After a daemon restart
/// mid-pending it reads `false` until a gated unsealing surface
/// re-establishes it (the runtime cache's documented transient).
pub(crate) fn github_status_body(
    settings_root: Option<&Path>,
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
) -> serde_json::Value {
    let present = custody.present();
    let last = runtime.last();
    let config = integration_config(settings_root);
    let pending_slug = if present {
        runtime.pending_install_slug()
    } else {
        None
    };
    serde_json::json!({
        "configured": present,
        "status": crate::github_pr::status::status_label(present, last.as_ref()),
        "detail": crate::github_pr::status::status_detail(last.as_ref()),
        "checked_at_ms": last.as_ref().map(|outcome| outcome.at_unix_ms),
        "custody_backend_available": custody.backend_available(),
        "pending_install": pending_slug.is_some(),
        "app_slug": pending_slug.filter(|slug| !slug.is_empty()),
        "repos": config.repos,
        "poll_minutes": config.poll_minutes,
    })
}

/// Transport-neutral core of `GET /api/integrations/github/status`
/// (tunnel twin `api_github_integration_status`). Presence is blob
/// existence, state is the cached last exchange — a status poll never
/// unseals and never talks to GitHub.
pub(crate) fn github_integration_status_api_response(
    settings_root: Option<&Path>,
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::Value(github_status_body(settings_root, custody, runtime)),
    )
}

/// Transport-neutral core of `POST /api/integrations/github` (tunnel
/// twin `api_github_integration_save`).
pub(crate) async fn github_integration_save_api_response(
    body: &[u8],
    settings_root: Option<&Path>,
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
    actor_principal: &str,
    audit_origin: &str,
) -> ApiResponse {
    let payload: GithubSavePayload = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid payload: {error}"));
        }
    };
    if let Some(repos) = payload.repos.as_ref() {
        if repos.len() > MAX_WATCHED_REPOS {
            return ApiResponse::json_error(
                400,
                format!("at most {MAX_WATCHED_REPOS} watched repos"),
            );
        }
        for repo in repos {
            if let Err(error) = validate_repo_name(repo) {
                return ApiResponse::json_error(400, &error);
            }
        }
    }
    if payload.poll_minutes == Some(0) {
        return ApiResponse::json_error(400, "poll_minutes floor is 1");
    }

    // Resolve the credentials document: a fresh key, or an ids-only
    // update re-sealing the existing document. Updates need the current
    // key, so this is the one configure-time unseal — an owner gesture
    // under CredentialsManage, not a poll.
    let mut document = match payload.private_key_pem.as_deref() {
        Some(pem) => crate::github_pr::credentials::GithubAppCredentials {
            v: 1,
            app_id: payload.app_id.clone(),
            installation_id: Some(payload.installation_id),
            slug: None,
            private_key_pem: pem.to_string(),
        },
        None => {
            if !custody.present() {
                return ApiResponse::json_error(
                    400,
                    "private_key_pem is required — no sealed credentials exist to update",
                );
            }
            let Some(existing) = custody.retrieve() else {
                return ApiResponse::json_error(
                    500,
                    "custody refused the existing credentials (the custody trail carries the deny)",
                );
            };
            match crate::github_pr::credentials::GithubAppCredentials::from_sealed_bytes(&existing)
            {
                Ok(mut existing) => {
                    existing.app_id = payload.app_id.clone();
                    // Completing a pending-install document keeps its
                    // ceremony-recorded slug; a plain ids update keeps
                    // whatever was there.
                    existing.installation_id = Some(payload.installation_id);
                    existing
                }
                Err(error) => return ApiResponse::json_error(500, &error),
            }
        }
    };
    if let Err(error) = document.validate() {
        return ApiResponse::json_error(400, &error);
    }
    let sealed = match document.sealed_bytes() {
        Ok(bytes) => bytes,
        Err(error) => return ApiResponse::json_error(500, &error),
    };
    if let Err(error) = custody.store(&sealed, actor_principal, audit_origin) {
        return ApiResponse::json_error(500, &error);
    }
    // Every save writes a complete document (installation_id present),
    // so any ceremony pending-phase is over — including the GC2
    // auto-fill that completes a pending-install document.
    runtime.clear_pending_install();

    // Persist the non-secret watch config beside the sealed entry.
    let mut config_persisted = true;
    if payload.repos.is_some() || payload.poll_minutes.is_some() {
        match settings_root {
            Some(root) => match crate::project::Project::from_root(root.to_path_buf()) {
                Ok(mut proj) => {
                    if let Some(repos) = payload.repos.clone() {
                        proj.config.integrations.github.repos = repos;
                    }
                    if let Some(minutes) = payload.poll_minutes {
                        proj.config.integrations.github.poll_minutes = Some(minutes);
                    }
                    if let Err(error) = proj.save_config() {
                        return ApiResponse::json_error(500, error.to_string());
                    }
                }
                Err(error) => return ApiResponse::json_error(500, error.to_string()),
            },
            None => config_persisted = false,
        }
    }

    // One real exchange, so "valid" means something: token mint always,
    // plus a pull list when a watch list exists (permission proof).
    let verified_open_prs = match crate::github_pr::client::GithubAppClient::new(
        runtime.api_base(),
        document.clone(),
    ) {
        Ok(client) => match client.verify().await {
            Ok(()) => {
                let config = integration_config(settings_root);
                match config.repos.first() {
                    Some(repo) => match client.list_open_pulls(repo, None).await {
                        Ok(crate::github_pr::client::Conditional::Fresh { value, .. }) => {
                            runtime.record(crate::github_pr::status::CheckResult::Valid);
                            Some(value.len())
                        }
                        Ok(crate::github_pr::client::Conditional::NotModified) => {
                            runtime.record(crate::github_pr::status::CheckResult::Valid);
                            None
                        }
                        Err(error) => {
                            runtime.record_error(&error);
                            None
                        }
                    },
                    None => {
                        runtime.record(crate::github_pr::status::CheckResult::Valid);
                        None
                    }
                }
            }
            Err(error) => {
                runtime.record_error(&error);
                None
            }
        },
        Err(error) => {
            runtime.record(crate::github_pr::status::CheckResult::Denied(error));
            None
        }
    };

    let mut body = github_status_body(settings_root, custody, runtime);
    if let Some(map) = body.as_object_mut() {
        map.insert("saved".to_string(), serde_json::Value::Bool(true));
        map.insert(
            "config_persisted".to_string(),
            serde_json::Value::Bool(config_persisted),
        );
        if let Some(count) = verified_open_prs {
            map.insert("verified_open_prs".to_string(), serde_json::json!(count));
        }
    }
    ApiResponse::json(200, JsonBody::Value(body))
}

/// Transport-neutral core of `DELETE /api/integrations/github` (tunnel
/// twin `api_github_integration_remove`). Idempotent: removing an
/// unconfigured integration answers the same shape.
pub(crate) fn github_integration_remove_api_response(
    settings_root: Option<&Path>,
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
    actor_principal: &str,
    audit_origin: &str,
) -> ApiResponse {
    if let Err(error) = custody.remove(actor_principal, audit_origin) {
        return ApiResponse::json_error(500, &error);
    }
    runtime.clear();
    let mut body = github_status_body(settings_root, custody, runtime);
    if let Some(map) = body.as_object_mut() {
        map.insert("removed".to_string(), serde_json::Value::Bool(true));
    }
    ApiResponse::json(200, JsonBody::Value(body))
}

// ---------------------------------------------------------------------------
// The one-click connect ceremony (Track GC): manifest-start + callback.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestStartPayload {
    /// Org handle for an org-owned App; absent/empty = personal account.
    #[serde(default)]
    organization: Option<String>,
}

/// GitHub login grammar (orgs and users): alphanumerics and inner
/// hyphens, ≤ 39 chars. The handle rides a github.com URL path, so
/// anything else is refused at intake.
fn validate_org_handle(handle: &str) -> Result<(), String> {
    let ok = !handle.is_empty()
        && handle.len() <= 39
        && !handle.starts_with('-')
        && !handle.ends_with('-')
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!("organization {handle:?} is not a GitHub org handle"))
    }
}

/// The exact origin the requesting browser is on, rebuilt from the
/// `Host` header (the redirect must return to this origin and nowhere
/// else). Same authority discipline as the loopback trusted-local lane:
/// parse as an authority, refuse every URL component a legal Host
/// cannot carry.
pub(crate) fn validated_request_origin(is_tls: bool, host_header: Option<&str>) -> Option<String> {
    let authority = host_header?.trim();
    let url = url::Url::parse(&format!("http://{authority}")).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let scheme = if is_tls { "https" } else { "http" };
    match url.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Transport-neutral core of `POST /api/integrations/github/manifest-start`
/// (HTTP-only; no tunnel twin — see the route row). Mints the ceremony
/// state (single-flight by replacement) and answers the form target +
/// manifest for the fragment to POST. The custody precheck refuses up
/// front: the owner must never create an App whose key this daemon
/// cannot seal.
pub(crate) fn github_manifest_start_api_response(
    body: &[u8],
    custody: &dyn GithubAppCustody,
    slot: &crate::github_pr::manifest_ceremony::ManifestCeremonySlot,
    is_tls: bool,
    host_header: Option<&str>,
    hostname_label: &str,
    starter_principal: &str,
    now_ms: u64,
) -> ApiResponse {
    let payload: ManifestStartPayload = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid payload: {error}"));
        }
    };
    if !custody.backend_available() {
        return ApiResponse::json_error(
            400,
            "no credential custody backend is available on this platform — the one-click \
             ceremony cannot seal the App key it would create; use the manual form on a \
             machine with custody support",
        );
    }
    let target_org = match payload
        .organization
        .as_deref()
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
    {
        Some(handle) => match validate_org_handle(handle) {
            Ok(()) => Some(handle.to_string()),
            Err(error) => return ApiResponse::json_error(400, &error),
        },
        None => None,
    };
    let Some(origin) = validated_request_origin(is_tls, host_header) else {
        return ApiResponse::json_error(
            400,
            "the request carries no usable Host header to compose the redirect origin from",
        );
    };
    let state = match slot.begin(
        origin.clone(),
        target_org.clone(),
        starter_principal.to_string(),
        now_ms,
    ) {
        Ok(state) => state,
        Err(error) => return ApiResponse::json_error(500, &error),
    };
    let manifest =
        crate::github_pr::manifest_ceremony::manifest_document(&origin, hostname_label);
    let form_action =
        crate::github_pr::manifest_ceremony::manifest_form_action(target_org.as_deref(), &state);
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({
            "form_action": form_action,
            "manifest": manifest,
            "app_name": manifest["name"],
        })),
    )
}

/// Minimal HTML escaping for the ceremony pages (attribute + text
/// positions). Inputs are already shape-validated; this is the second
/// belt.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A self-contained ceremony page (no SPA assets, no secrets). The
/// return link is the validated ceremony origin; success pages also
/// meta-refresh there after a beat.
fn ceremony_page(status: u16, title: &str, detail: &str, return_origin: Option<&str>) -> ApiResponse {
    let (link, refresh) = match return_origin {
        Some(origin) => {
            let escaped = html_escape(origin);
            (
                format!("<p><a href=\"{escaped}/\">Return to the dashboard</a></p>"),
                format!("<meta http-equiv=\"refresh\" content=\"6;url={escaped}/\">"),
            )
        }
        None => (
            "<p>Return to the dashboard tab you started from.</p>".to_string(),
            String::new(),
        ),
    };
    let title = html_escape(title);
    let detail = html_escape(detail);
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">{refresh}\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:34rem;margin:15vh auto;\
         padding:0 1rem;line-height:1.5}}h1{{font-size:1.2rem}}</style></head>\
         <body><h1>{title}</h1><p>{detail}</p>{link}</body></html>"
    );
    ApiResponse::Bytes {
        status,
        content_type: "text/html; charset=utf-8".to_string(),
        headers: vec![
            ("Cache-Control", "no-cache".to_string()),
            ("X-Content-Type-Options", "nosniff".to_string()),
            ("Content-Security-Policy", "frame-ancestors 'none'".to_string()),
            ("Connection", "close".to_string()),
        ],
        bytes: BytesPayload::InMemory(html.into_bytes()),
        meta: serde_json::Value::Null,
    }
}

/// The uniform refusal page: unknown, replayed, expired, and absent
/// states — and malformed codes — all read identically (and none of
/// them ever reaches GitHub).
fn ceremony_refusal_page() -> ApiResponse {
    ceremony_page(
        403,
        "This connect link cannot be used",
        crate::github_pr::manifest_ceremony::STATE_REFUSED,
        None,
    )
}

/// Transport-neutral core of `GET /api/integrations/github/callback`
/// (authority-free browser navigation; the state is the entire
/// authorization). Order is load-bearing: shape checks (no burn) →
/// atomic state burn → conversion → seal → render. A request that
/// fails before the burn cannot cost the owner their pending ceremony;
/// nothing that fails the burn ever reaches GitHub.
pub(crate) async fn github_manifest_callback_api_response(
    request_line: &str,
    source_hint: &str,
    custody: &dyn GithubAppCustody,
    slot: &crate::github_pr::manifest_ceremony::ManifestCeremonySlot,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
    now_ms: u64,
) -> ApiResponse {
    use crate::github_pr::manifest_ceremony as ceremony;
    if !ceremony::callback_rate_ok(source_hint, now_ms) {
        return ceremony_page(
            429,
            "Too many attempts",
            "The connect callback is rate limited; retry shortly.",
            None,
        );
    }
    let (Some(state), Some(code)) = (
        query_param(request_line, "state"),
        query_param(request_line, "code"),
    ) else {
        return ceremony_refusal_page();
    };
    // Shape-gate the code before burning anything: a garbage probe must
    // not cost the owner their pending ceremony, and a malformed code
    // never reaches the conversion URL builder.
    if !ceremony::code_shape_ok(&code) {
        return ceremony_refusal_page();
    }
    let pending = match slot.consume(&state, now_ms) {
        Ok(pending) => pending,
        Err(_) => return ceremony_refusal_page(),
    };
    // The burn succeeded: this is the owner's ceremony completing.
    // Convert server-side; the PEM never transits the browser.
    let conversion =
        match crate::github_pr::client::convert_manifest_code(runtime.api_base(), &code).await {
            Ok(conversion) => conversion,
            Err(error) => {
                return ceremony_page(
                    502,
                    "GitHub did not complete the App creation",
                    &format!(
                        "The conversion exchange failed ({error}). The ceremony is closed — \
                         start again from the dashboard."
                    ),
                    Some(&pending.origin),
                );
            }
        };
    let mut document = crate::github_pr::credentials::GithubAppCredentials {
        v: 1,
        app_id: conversion.id.to_string(),
        installation_id: None,
        slug: Some(conversion.slug.clone()),
        private_key_pem: conversion.pem,
    };
    if let Err(error) = document.validate() {
        return ceremony_page(
            500,
            "GitHub's response could not be sealed",
            &format!("The created App's key did not validate ({error}). Start again from the dashboard."),
            Some(&pending.origin),
        );
    }
    let sealed = match document.sealed_bytes() {
        Ok(bytes) => bytes,
        Err(error) => {
            return ceremony_page(
                500,
                "GitHub's response could not be sealed",
                &error,
                Some(&pending.origin),
            );
        }
    };
    if let Err(error) = custody.store(
        &sealed,
        &pending.starter_principal,
        "github-manifest-callback",
    ) {
        return ceremony_page(500, "Custody refused the seal", &error, Some(&pending.origin));
    }
    runtime.set_pending_install(conversion.slug.clone());
    ceremony_page(
        200,
        "GitHub App created and sealed",
        &format!(
            "\"{}\" is registered and its key is sealed into custody — nothing was typed, \
             nothing was shown. Return to the dashboard to install it on your repositories.",
            conversion.slug
        ),
        Some(&pending.origin),
    )
}

pub(crate) async fn handle_github_manifest_start(
    stream: DemuxStream,
    body: &[u8],
    is_tls: bool,
    host_header: Option<&str>,
    starter_principal: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let hostname_label = tokio::task::spawn_blocking(best_effort_hostname)
        .await
        .unwrap_or_default();
    let response = github_manifest_start_api_response(
        body,
        &DaemonGithubAppCustody,
        crate::github_pr::manifest_ceremony::slot(),
        is_tls,
        host_header,
        &hostname_label,
        &starter_principal,
        now_unix_ms(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_github_manifest_callback(
    stream: DemuxStream,
    request_line: &str,
    source_hint: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_manifest_callback_api_response(
        request_line,
        &source_hint,
        &DaemonGithubAppCustody,
        crate::github_pr::manifest_ceremony::slot(),
        crate::github_pr::status::global(),
        now_unix_ms(),
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Best-effort machine label for the App name — display only, never
/// authority. The `hostname` binary exists on all three platforms; a
/// missing or failing probe degrades to the bare "Intendant" name.
fn best_effort_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

pub(crate) async fn handle_github_integration_save(
    stream: DemuxStream,
    body: &[u8],
    settings_root: Option<PathBuf>,
    actor_principal: String,
    audit_origin: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_integration_save_api_response(
        body,
        settings_root.as_deref(),
        &DaemonGithubAppCustody,
        crate::github_pr::status::global(),
        &actor_principal,
        &audit_origin,
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_github_integration_status(
    stream: DemuxStream,
    settings_root: Option<PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_integration_status_api_response(
        settings_root.as_deref(),
        &DaemonGithubAppCustody,
        crate::github_pr::status::global(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_github_integration_remove(
    stream: DemuxStream,
    settings_root: Option<PathBuf>,
    actor_principal: String,
    audit_origin: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_integration_remove_api_response(
        settings_root.as_deref(),
        &DaemonGithubAppCustody,
        crate::github_pr::status::global(),
        &actor_principal,
        &audit_origin,
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_pr::status::GithubIntegrationRuntime;
    use std::sync::Mutex;

    /// Tempdir-backed custody: the crate's plain-file backend under a
    /// test root — no OS keystore, no audit-trail writes, hermetic.
    struct TempCustody {
        backend: intendant_custody::PlainFileBackend,
        stores: Mutex<usize>,
    }

    impl TempCustody {
        fn new(root: &Path) -> Self {
            Self {
                backend: intendant_custody::PlainFileBackend::new(root.join("custody")).unwrap(),
                stores: Mutex::new(0),
            }
        }
    }

    impl GithubAppCustody for TempCustody {
        fn present(&self) -> bool {
            use intendant_custody::CustodyBackend as _;
            self.backend
                .contains(crate::key_custody::GITHUB_APP_ENTRY)
                .unwrap_or(false)
        }
        fn backend_available(&self) -> bool {
            true
        }
        fn retrieve(&self) -> Option<Vec<u8>> {
            use intendant_custody::CustodyBackend as _;
            self.backend
                .retrieve(crate::key_custody::GITHUB_APP_ENTRY)
                .ok()
                .map(|secret| secret.as_bytes().to_vec())
        }
        fn store(&self, material: &[u8], _actor: &str, _origin: &str) -> Result<(), String> {
            use intendant_custody::CustodyBackend as _;
            *self.stores.lock().unwrap() += 1;
            self.backend
                .store(crate::key_custody::GITHUB_APP_ENTRY, material)
                .map_err(|error| error.to_string())
        }
        fn remove(&self, _actor: &str, _origin: &str) -> Result<(), String> {
            use intendant_custody::CustodyBackend as _;
            self.backend
                .delete(crate::key_custody::GITHUB_APP_ENTRY)
                .map_err(|error| error.to_string())
        }
    }

    fn body_json(response: &ApiResponse) -> serde_json::Value {
        match response {
            ApiResponse::Json { body, .. } => {
                serde_json::from_str(&body.as_text()).expect("JSON body")
            }
            _ => panic!("expected the JSON lane"),
        }
    }

    fn status_of(response: &ApiResponse) -> u16 {
        match response {
            ApiResponse::Json { status, .. } => *status,
            _ => panic!("expected the JSON lane"),
        }
    }

    #[test]
    fn repo_names_validate() {
        assert!(validate_repo_name("intendant-dev/Intendant").is_ok());
        assert!(validate_repo_name("o/r.name_x-1").is_ok());
        for bad in ["", "norepo", "a/b/c", "/r", "o/", "o/r r", "o/r?x"] {
            assert!(validate_repo_name(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn status_is_unconfigured_then_configured_and_never_unseals() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let runtime = GithubIntegrationRuntime::new("http://fixture.invalid");
        let response = github_integration_status_api_response(Some(dir.path()), &custody, &runtime);
        let body = body_json(&response);
        assert_eq!(body["configured"], false);
        assert_eq!(body["status"], "unconfigured");

        use intendant_custody::CustodyBackend as _;
        custody
            .backend
            .store(crate::key_custody::GITHUB_APP_ENTRY, b"{}")
            .unwrap();
        let response = github_integration_status_api_response(Some(dir.path()), &custody, &runtime);
        let body = body_json(&response);
        assert_eq!(body["configured"], true);
        assert_eq!(body["status"], "configured");
        assert!(body["detail"].is_null());
    }

    #[tokio::test]
    async fn save_rejects_bad_payloads_before_touching_custody() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let runtime = GithubIntegrationRuntime::new("http://fixture.invalid");
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                serde_json::json!({"app_id": "1", "installation_id": 1, "unknown": true}),
                "invalid payload",
            ),
            (
                serde_json::json!({"app_id": "1", "installation_id": 1}),
                "private_key_pem is required",
            ),
            (
                serde_json::json!({
                    "app_id": "1", "installation_id": 1,
                    "private_key_pem": "not a pem"
                }),
                "PEM",
            ),
            (
                serde_json::json!({
                    "app_id": "1", "installation_id": 1,
                    "private_key_pem": "x", "repos": ["bad repo name"]
                }),
                "owner/repo",
            ),
            (
                serde_json::json!({
                    "app_id": "1", "installation_id": 1,
                    "private_key_pem": "x", "poll_minutes": 0
                }),
                "floor",
            ),
        ];
        for (payload, expect) in cases {
            let response = github_integration_save_api_response(
                payload.to_string().as_bytes(),
                Some(dir.path()),
                &custody,
                &runtime,
                "principal:test",
                "local",
            )
            .await;
            assert_eq!(status_of(&response), 400, "payload {payload} must 400");
            let body = body_json(&response).to_string();
            assert!(body.contains(expect), "{body} should mention {expect:?}");
        }
        assert_eq!(*custody.stores.lock().unwrap(), 0, "nothing may be sealed");
        assert!(!custody.present());
    }

    #[tokio::test]
    async fn remove_is_idempotent_and_clears_state() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let runtime = GithubIntegrationRuntime::new("http://fixture.invalid");
        runtime.record(crate::github_pr::status::CheckResult::Valid);
        use intendant_custody::CustodyBackend as _;
        custody
            .backend
            .store(crate::key_custody::GITHUB_APP_ENTRY, b"{}")
            .unwrap();
        let response = github_integration_remove_api_response(
            Some(dir.path()),
            &custody,
            &runtime,
            "principal:test",
            "local",
        );
        let body = body_json(&response);
        assert_eq!(body["removed"], true);
        assert_eq!(body["configured"], false);
        assert_eq!(body["status"], "unconfigured");
        assert!(runtime.last().is_none(), "remove clears the cached outcome");
        // Second remove: same shape, still 200 — deletion is an end
        // state, not an observation.
        let response = github_integration_remove_api_response(
            Some(dir.path()),
            &custody,
            &runtime,
            "principal:test",
            "local",
        );
        assert_eq!(status_of(&response), 200);
        assert_eq!(body_json(&response)["configured"], false);
    }
}
