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
    /// Required with a fresh key; optional on an update of an existing
    /// entry (absent = keep the sealed value). The repo picker's
    /// config-only writes send neither id.
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    installation_id: Option<u64>,
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

    // Resolve the credentials document: a fresh key, an ids update
    // re-sealing the existing document, or a config-only write (repo
    // picker, poll minutes) that verifies against the sealed document
    // without re-sealing it. Updates need the current key, so this is
    // the one configure-time unseal — an owner gesture under
    // CredentialsManage, not a poll.
    let ids_supplied = payload.app_id.is_some() || payload.installation_id.is_some();
    let (mut document, reseal) = match payload.private_key_pem.as_deref() {
        Some(pem) => {
            let (Some(app_id), Some(installation_id)) =
                (payload.app_id.clone(), payload.installation_id)
            else {
                return ApiResponse::json_error(
                    400,
                    "app_id and installation_id are required with a new private key",
                );
            };
            (
                crate::github_pr::credentials::GithubAppCredentials {
                    v: 1,
                    app_id,
                    installation_id: Some(installation_id),
                    slug: None,
                    private_key_pem: pem.to_string(),
                },
                true,
            )
        }
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
                    // Present ids overwrite; absent ids keep the sealed
                    // values. Completing a pending-install document
                    // keeps its ceremony-recorded slug either way.
                    if let Some(app_id) = payload.app_id.clone() {
                        existing.app_id = app_id;
                    }
                    if let Some(installation_id) = payload.installation_id {
                        existing.installation_id = Some(installation_id);
                    }
                    (existing, ids_supplied)
                }
                Err(error) => return ApiResponse::json_error(500, &error),
            }
        }
    };
    if let Err(error) = document.validate() {
        return ApiResponse::json_error(400, &error);
    }
    if reseal {
        let sealed = match document.sealed_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return ApiResponse::json_error(500, &error),
        };
        if let Err(error) = custody.store(&sealed, actor_principal, audit_origin) {
            return ApiResponse::json_error(500, &error);
        }
        // A re-sealed complete document ends the ceremony pending
        // phase — including the auto-fill that completes it. A pending
        // document (app_id-only update) stays pending.
        if document.installation_id.is_some() {
            runtime.clear_pending_install();
        }
    }

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
        Err(format!(
            "organization {handle:?} is not a GitHub org handle"
        ))
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
/// cannot seal. `origin` is the edge-validated requesting origin
/// ([`validated_request_origin`]); `None` = no usable Host header.
pub(crate) fn github_manifest_start_api_response(
    body: &[u8],
    custody: &dyn GithubAppCustody,
    slot: &crate::github_pr::manifest_ceremony::ManifestCeremonySlot,
    origin: Option<String>,
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
    let Some(origin) = origin else {
        return ApiResponse::json_error(
            400,
            "the request carries no usable Host header to compose the redirect origin from",
        );
    };
    let state = match slot.begin(origin.clone(), starter_principal.to_string(), now_ms) {
        Ok(state) => state,
        Err(error) => return ApiResponse::json_error(500, &error),
    };
    let manifest = crate::github_pr::manifest_ceremony::manifest_document(&origin, hostname_label);
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
fn ceremony_page(
    status: u16,
    title: &str,
    detail: &str,
    return_origin: Option<&str>,
) -> ApiResponse {
    // Return-to-context (UX0 ruling): the ceremony landed the user on an
    // external hop, so both the link and the auto-refresh carry them back
    // AT THE VAULT SECTION with fresh state — `?ceremony=github` tells the
    // section to bypass its status cache and scroll itself into view
    // (32-vault-custody.js reads and strips it); `#vault` is the router's
    // tab anchor. A bare origin root-drop was the owner's finding 3.
    let (link, refresh) = match return_origin {
        Some(origin) => {
            let escaped = html_escape(origin);
            (
                format!(
                    "<p><a href=\"{escaped}/?ceremony=github#vault\">\
                     Return to the dashboard's Vault section</a></p>"
                ),
                format!(
                    "<meta http-equiv=\"refresh\" \
                     content=\"6;url={escaped}/?ceremony=github#vault\">"
                ),
            )
        }
        None => (
            "<p>Return to the dashboard tab you started from — the Vault section \
             shows the integration's live state.</p>"
                .to_string(),
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
            (
                "Content-Security-Policy",
                "frame-ancestors 'none'".to_string(),
            ),
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
            &format!(
                "The created App's key did not validate ({error}). Start again from the dashboard."
            ),
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
        return ceremony_page(
            500,
            "Custody refused the seal",
            &error,
            Some(&pending.origin),
        );
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

/// Unseal + parse the sealed document for a ceremony read. `None` =
/// absent or custody-denied (the custody lane audited it by name).
fn unsealed_document(
    custody: &dyn GithubAppCustody,
) -> Option<crate::github_pr::credentials::GithubAppCredentials> {
    let bytes = custody.retrieve()?;
    crate::github_pr::credentials::GithubAppCredentials::from_sealed_bytes(&bytes).ok()
}

/// Transport-neutral core of `GET /api/integrations/github/installations`
/// (tunnel twin `api_github_installations`). App-JWT discovery: works on
/// a pending-install document — and re-establishes the runtime's
/// pending cache from the sealed doc (the restart-transient self-heal).
pub(crate) async fn github_installations_api_response(
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
) -> ApiResponse {
    if !custody.present() {
        return ApiResponse::json_error(400, "no GitHub App is configured");
    }
    let Some(document) = unsealed_document(custody) else {
        return ApiResponse::json_error(
            500,
            "custody refused the sealed credentials (the custody trail carries the deny)",
        );
    };
    if document.installation_id.is_none() {
        runtime.set_pending_install(document.slug.clone().unwrap_or_default());
    }
    let client = match crate::github_pr::client::GithubAppClient::new(runtime.api_base(), document)
    {
        Ok(client) => client,
        Err(error) => return ApiResponse::json_error(500, &error),
    };
    match client.list_installations().await {
        Ok(installations) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "installations": installations })),
        ),
        Err(error) => {
            runtime.record_error(&error);
            ApiResponse::json_error(502, format!("installation discovery failed: {error}"))
        }
    }
}

/// Transport-neutral core of `GET /api/integrations/github/repositories`
/// (tunnel twin `api_github_repositories`). Installation-token read —
/// a pending-install document is a named refusal, not an error class.
pub(crate) async fn github_repositories_api_response(
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
) -> ApiResponse {
    if !custody.present() {
        return ApiResponse::json_error(400, "no GitHub App is configured");
    }
    let Some(document) = unsealed_document(custody) else {
        return ApiResponse::json_error(
            500,
            "custody refused the sealed credentials (the custody trail carries the deny)",
        );
    };
    if document.installation_id.is_none() {
        runtime.set_pending_install(document.slug.clone().unwrap_or_default());
        return ApiResponse::json_error(
            400,
            "installation pending — install the App on GitHub and finish discovery first",
        );
    }
    let client = match crate::github_pr::client::GithubAppClient::new(runtime.api_base(), document)
    {
        Ok(client) => client,
        Err(error) => return ApiResponse::json_error(500, &error),
    };
    match client.list_installation_repositories().await {
        Ok(repositories) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "repositories": repositories })),
        ),
        Err(error) => {
            runtime.record_error(&error);
            ApiResponse::json_error(502, format!("repository listing failed: {error}"))
        }
    }
}

pub(crate) async fn handle_github_installations(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_installations_api_response(
        &DaemonGithubAppCustody,
        crate::github_pr::status::global(),
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_github_repositories(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = github_repositories_api_response(
        &DaemonGithubAppCustody,
        crate::github_pr::status::global(),
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
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
        validated_request_origin(is_tls, host_header),
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
        available: bool,
    }

    impl TempCustody {
        fn new(root: &Path) -> Self {
            Self {
                backend: intendant_custody::PlainFileBackend::new(root.join("custody")).unwrap(),
                stores: Mutex::new(0),
                available: true,
            }
        }

        /// The non-macOS shape: no backend to seal with.
        fn unavailable(root: &Path) -> Self {
            Self {
                available: false,
                ..Self::new(root)
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
            self.available
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

    // ── The one-click ceremony (Track GC) ─────────────────────────────

    use crate::github_pr::client::test_fixture::{spawn_fixture, token_route, FixtureResponse};
    use crate::github_pr::manifest_ceremony::ManifestCeremonySlot;
    use std::collections::HashMap;

    const NOW: u64 = 1_700_000_000_000;
    const CLIENT_SECRET_CANARY: &str = "canary-client-secret-3f1";
    const WEBHOOK_SECRET_CANARY: &str = "canary-webhook-secret-9d4";

    fn conversion_route(code: &str) -> ((String, String), FixtureResponse) {
        let body = serde_json::json!({
            "id": 4242,
            "slug": "intendant-example",
            "node_id": "A_node",
            "name": "Intendant (example)",
            "client_id": "Iv1.fixture",
            "client_secret": CLIENT_SECRET_CANARY,
            "webhook_secret": WEBHOOK_SECRET_CANARY,
            "pem": crate::github_pr::client::test_rsa_pem(),
            "html_url": "https://github.com/apps/intendant-example",
            "owner": {"login": "example-org"},
        })
        .to_string();
        (
            (
                "POST".to_string(),
                format!("/app-manifests/{code}/conversions"),
            ),
            (201, Vec::new(), body),
        )
    }

    fn begun_slot(now_ms: u64) -> (ManifestCeremonySlot, String) {
        let slot = ManifestCeremonySlot::default();
        let state = slot
            .begin(
                "http://127.0.0.1:8765".to_string(),
                "principal:test-starter".to_string(),
                now_ms,
            )
            .expect("begin ceremony");
        (slot, state)
    }

    fn callback_line(code: Option<&str>, state: Option<&str>) -> String {
        let mut query = Vec::new();
        if let Some(code) = code {
            query.push(format!("code={code}"));
        }
        if let Some(state) = state {
            query.push(format!("state={state}"));
        }
        format!(
            "GET /api/integrations/github/callback?{} HTTP/1.1",
            query.join("&")
        )
    }

    fn page_status(response: &ApiResponse) -> (u16, String) {
        match response {
            ApiResponse::Bytes { status, bytes, .. } => {
                let BytesPayload::InMemory(body) = bytes;
                (*status, String::from_utf8_lossy(body).to_string())
            }
            _ => panic!("ceremony pages ride the bytes lane"),
        }
    }

    /// The happy path: a valid state + code converts once, seals the
    /// pending-install document, and the secrets GitHub returned are
    /// discarded at parse — no sealed byte and no file under the test
    /// root carries either canary.
    #[tokio::test]
    async fn one_click_callback_converts_seals_pending_and_discards_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([conversion_route("goodcode123")])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, state) = begun_slot(NOW);

        let response = github_manifest_callback_api_response(
            &callback_line(Some("goodcode123"), Some(&state)),
            "test-src-happy",
            &custody,
            &slot,
            &runtime,
            NOW + 1,
        )
        .await;
        let (status, page) = page_status(&response);
        assert_eq!(status, 200, "page: {page}");
        assert!(page.contains("intendant-example"), "page names the slug");
        assert!(
            page.contains("http://127.0.0.1:8765/"),
            "page links the ceremony origin back"
        );
        assert_eq!(
            fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one conversion exchange"
        );

        let sealed = custody.retrieve().expect("sealed document exists");
        let doc = crate::github_pr::credentials::GithubAppCredentials::from_sealed_bytes(&sealed)
            .expect("parses in this build");
        assert_eq!(doc.app_id, "4242");
        assert_eq!(doc.installation_id, None, "sealed pending-install");
        assert_eq!(doc.slug.as_deref(), Some("intendant-example"));
        assert!(doc.private_key_pem.starts_with("-----BEGIN"));

        // The named secrets-discard pin: the narrow parse never retained
        // them, so no sealed byte sequence and no file under the test
        // root contains either canary.
        let sealed_text = String::from_utf8_lossy(&sealed);
        assert!(!sealed_text.contains(CLIENT_SECRET_CANARY));
        assert!(!sealed_text.contains(WEBHOOK_SECRET_CANARY));
        let mut pending_dirs = vec![dir.path().to_path_buf()];
        while let Some(scan) = pending_dirs.pop() {
            for entry in std::fs::read_dir(&scan).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending_dirs.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(
                        !text.contains(CLIENT_SECRET_CANARY)
                            && !text.contains(WEBHOOK_SECRET_CANARY),
                        "{path:?} must not carry a discarded secret"
                    );
                }
            }
        }

        assert_eq!(
            runtime.pending_install_slug().as_deref(),
            Some("intendant-example")
        );
        let body = body_json(&github_integration_status_api_response(
            Some(dir.path()),
            &custody,
            &runtime,
        ));
        assert_eq!(body["configured"], true);
        assert_eq!(body["pending_install"], true);
        assert_eq!(body["app_slug"], "intendant-example");
        assert_eq!(
            body["status"], "configured",
            "the label vocabulary is unchanged"
        );
    }

    /// The named foreign-code pin: without a valid state — absent OR
    /// wrong — the conversion endpoint is NEVER called (hit count zero),
    /// nothing seals, and the refusal page is uniform.
    #[tokio::test]
    async fn callback_without_valid_state_never_reaches_github() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([conversion_route("foreigncode")])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, _state) = begun_slot(NOW);

        let absent = github_manifest_callback_api_response(
            &callback_line(Some("foreigncode"), None),
            "test-src-foreign",
            &custody,
            &slot,
            &runtime,
            NOW + 1,
        )
        .await;
        assert_eq!(page_status(&absent).0, 403);

        let wrong = github_manifest_callback_api_response(
            &callback_line(Some("foreigncode"), Some("not-the-state")),
            "test-src-foreign",
            &custody,
            &slot,
            &runtime,
            NOW + 2,
        )
        .await;
        let (status, page) = page_status(&wrong);
        assert_eq!(status, 403);
        assert_eq!(
            page,
            page_status(&absent).1,
            "refusals are uniform: absent and wrong states read identically"
        );

        assert_eq!(
            fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the conversion endpoint must never be reached without a valid state"
        );
        assert!(!custody.present(), "nothing seals on a refused callback");
        assert_eq!(*custody.stores.lock().unwrap(), 0);
    }

    /// The named replay pin: the second identical callback refuses with
    /// zero additional conversion exchanges and the store count stays 1.
    #[tokio::test]
    async fn replayed_callback_refuses_with_zero_new_conversions_and_one_store() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([conversion_route("replaycode")])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, state) = begun_slot(NOW);
        let line = callback_line(Some("replaycode"), Some(&state));

        let first = github_manifest_callback_api_response(
            &line,
            "test-src-replay",
            &custody,
            &slot,
            &runtime,
            NOW + 1,
        )
        .await;
        assert_eq!(page_status(&first).0, 200);

        let replay = github_manifest_callback_api_response(
            &line,
            "test-src-replay",
            &custody,
            &slot,
            &runtime,
            NOW + 2,
        )
        .await;
        assert_eq!(page_status(&replay).0, 403);
        assert_eq!(
            fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a replayed callback must not reach GitHub again"
        );
        assert_eq!(*custody.stores.lock().unwrap(), 1, "one seal, ever");
    }

    /// The named expired-state pin: past the TTL the refusal is uniform
    /// and the conversion endpoint is never called.
    #[tokio::test]
    async fn expired_state_refuses_without_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([conversion_route("latecode")])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, state) = begun_slot(NOW);

        let response = github_manifest_callback_api_response(
            &callback_line(Some("latecode"), Some(&state)),
            "test-src-expired",
            &custody,
            &slot,
            &runtime,
            NOW + crate::github_pr::manifest_ceremony::MANIFEST_STATE_TTL_MS,
        )
        .await;
        assert_eq!(page_status(&response).0, 403);
        assert_eq!(fixture.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!custody.present());
    }

    /// A conversion failure after the burn ends the ceremony: the state
    /// cannot be replayed into a second attempt (fail-closed ordering).
    #[tokio::test]
    async fn failed_conversion_ends_the_ceremony() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        // No conversion route: the fixture answers 404 (an invalid code).
        let fixture = spawn_fixture(HashMap::new()).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, state) = begun_slot(NOW);
        let line = callback_line(Some("deadcode"), Some(&state));

        let first = github_manifest_callback_api_response(
            &line,
            "test-src-dead",
            &custody,
            &slot,
            &runtime,
            NOW + 1,
        )
        .await;
        assert_eq!(page_status(&first).0, 502, "conversion failure is named");
        assert_eq!(fixture.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!custody.present(), "no seal without a conversion");

        let retry = github_manifest_callback_api_response(
            &line,
            "test-src-dead",
            &custody,
            &slot,
            &runtime,
            NOW + 2,
        )
        .await;
        assert_eq!(
            page_status(&retry).0,
            403,
            "the burn preceded the exchange; the ceremony is over"
        );
        assert_eq!(
            fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a burned state never converts again"
        );
    }

    /// A malformed code is refused BEFORE the burn: a garbage probe must
    /// not cost the owner their pending ceremony.
    #[tokio::test]
    async fn malformed_code_refuses_before_burning_the_ceremony() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([conversion_route("kept-code")])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let (slot, state) = begun_slot(NOW);

        let garbage = github_manifest_callback_api_response(
            &callback_line(Some("bad%2Fcode"), Some(&state)),
            "test-src-shape",
            &custody,
            &slot,
            &runtime,
            NOW + 1,
        )
        .await;
        assert_eq!(page_status(&garbage).0, 403);
        assert_eq!(fixture.hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        // The pending ceremony survived the probe: the honest callback
        // still completes.
        let honest = github_manifest_callback_api_response(
            &callback_line(Some("kept-code"), Some(&state)),
            "test-src-shape",
            &custody,
            &slot,
            &runtime,
            NOW + 2,
        )
        .await;
        assert_eq!(page_status(&honest).0, 200);
    }

    /// The custody precheck refuses manifest-start up front on a
    /// backend-less platform — the owner must never create an App whose
    /// key cannot seal — and no ceremony state is minted.
    #[test]
    fn manifest_start_refuses_without_custody_backend() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::unavailable(dir.path());
        let slot = ManifestCeremonySlot::default();
        let response = github_manifest_start_api_response(
            b"{}",
            &custody,
            &slot,
            validated_request_origin(false, Some("127.0.0.1:8765")),
            "example-host",
            "principal:test-starter",
            NOW,
        );
        assert_eq!(status_of(&response), 400);
        assert!(body_json(&response).to_string().contains("custody backend"));
        assert!(!slot.active(NOW), "no state mints on a refused start");
    }

    #[test]
    fn manifest_start_mints_state_and_composes_the_form() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let slot = ManifestCeremonySlot::default();
        let response = github_manifest_start_api_response(
            b"{}",
            &custody,
            &slot,
            validated_request_origin(false, Some("127.0.0.1:8765")),
            "example-host",
            "principal:test-starter",
            NOW,
        );
        assert_eq!(status_of(&response), 200);
        let body = body_json(&response);
        let form_action = body["form_action"].as_str().unwrap();
        assert!(
            form_action.starts_with("https://github.com/settings/apps/new?state="),
            "personal target: {form_action}"
        );
        assert_eq!(
            body["manifest"]["redirect_url"],
            "http://127.0.0.1:8765/api/integrations/github/callback"
        );
        assert_eq!(body["manifest"]["public"], false);
        assert!(!body["manifest"]
            .as_object()
            .unwrap()
            .contains_key("hook_attributes"));
        assert!(slot.active(NOW + 1), "the ceremony is pending");

        // Org target + validation.
        let org = github_manifest_start_api_response(
            br#"{"organization": "example-org"}"#,
            &custody,
            &slot,
            validated_request_origin(true, Some("box.example:8443")),
            "example-host",
            "principal:test-starter",
            NOW,
        );
        let org_body = body_json(&org);
        assert!(org_body["form_action"]
            .as_str()
            .unwrap()
            .starts_with("https://github.com/organizations/example-org/settings/apps/new?state="));
        assert_eq!(
            org_body["manifest"]["redirect_url"],
            "https://box.example:8443/api/integrations/github/callback"
        );

        let bad_org = github_manifest_start_api_response(
            br#"{"organization": "-bad handle-"}"#,
            &custody,
            &slot,
            validated_request_origin(false, Some("127.0.0.1:8765")),
            "example-host",
            "principal:test-starter",
            NOW,
        );
        assert_eq!(status_of(&bad_org), 400);

        let no_host = github_manifest_start_api_response(
            b"{}",
            &custody,
            &slot,
            validated_request_origin(false, None),
            "example-host",
            "principal:test-starter",
            NOW,
        );
        assert_eq!(status_of(&no_host), 400);
    }

    #[test]
    fn validated_origin_refuses_authority_smuggling() {
        assert_eq!(
            validated_request_origin(false, Some("127.0.0.1:8765")).as_deref(),
            Some("http://127.0.0.1:8765")
        );
        assert_eq!(
            validated_request_origin(true, Some("Box.Example")).as_deref(),
            Some("https://box.example")
        );
        for bad in ["user@host", "host/path", "host?query=1", "host#frag", ""] {
            assert_eq!(
                validated_request_origin(false, Some(bad)),
                None,
                "{bad:?} must be refused"
            );
        }
    }

    /// The named manual-form regression pin: the five-field path still
    /// configures end to end — a complete document seals, pending stays
    /// false — and the one-click machinery is never consulted (no
    /// conversion route exists on the fixture, and the hit count proves
    /// only the verify exchange ran).
    #[tokio::test]
    async fn manual_form_regression_five_field_path_ignores_one_click_machinery() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([token_route()])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let payload = serde_json::json!({
            "app_id": "123456",
            "installation_id": 987,
            "private_key_pem": crate::github_pr::client::test_rsa_pem(),
        });
        let response = github_integration_save_api_response(
            payload.to_string().as_bytes(),
            Some(dir.path()),
            &custody,
            &runtime,
            "principal:test",
            "local",
        )
        .await;
        assert_eq!(status_of(&response), 200);
        let body = body_json(&response);
        assert_eq!(body["saved"], true);
        assert_eq!(body["configured"], true);
        assert_eq!(body["pending_install"], false);
        assert_eq!(body["status"], "valid", "the real verify exchange ran");
        let doc = crate::github_pr::credentials::GithubAppCredentials::from_sealed_bytes(
            &custody.retrieve().unwrap(),
        )
        .unwrap();
        assert_eq!(doc.installation_id, Some(987), "complete, never pending");
        assert_eq!(
            fixture.token_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly the verify exchange — no ceremony machinery"
        );
    }

    /// Completing a pending-install document through the save verb (the
    /// GC2 auto-fill path) preserves the ceremony's key + slug and
    /// clears the pending phase.
    #[tokio::test]
    async fn ids_only_save_completes_a_pending_document_and_clears_the_phase() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        let fixture = spawn_fixture(HashMap::from([token_route()])).await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        let pending = crate::github_pr::credentials::GithubAppCredentials {
            v: 1,
            app_id: "123456".to_string(),
            installation_id: None,
            slug: Some("intendant-example".to_string()),
            private_key_pem: crate::github_pr::client::test_rsa_pem().to_string(),
        };
        custody
            .store(&pending.sealed_bytes().unwrap(), "principal:test", "test")
            .unwrap();
        runtime.set_pending_install("intendant-example".to_string());

        let payload = serde_json::json!({"app_id": "123456", "installation_id": 987});
        let response = github_integration_save_api_response(
            payload.to_string().as_bytes(),
            Some(dir.path()),
            &custody,
            &runtime,
            "principal:test",
            "local",
        )
        .await;
        assert_eq!(status_of(&response), 200);
        let body = body_json(&response);
        assert_eq!(body["pending_install"], false);
        let doc = crate::github_pr::credentials::GithubAppCredentials::from_sealed_bytes(
            &custody.retrieve().unwrap(),
        )
        .unwrap();
        assert_eq!(doc.installation_id, Some(987));
        assert_eq!(
            doc.slug.as_deref(),
            Some("intendant-example"),
            "the ceremony-recorded slug survives completion"
        );
    }

    fn seal_direct(
        custody: &TempCustody,
        doc: &crate::github_pr::credentials::GithubAppCredentials,
    ) {
        custody
            .store(&doc.sealed_bytes().unwrap(), "principal:test", "test")
            .unwrap();
    }

    fn complete_doc() -> crate::github_pr::credentials::GithubAppCredentials {
        crate::github_pr::credentials::GithubAppCredentials {
            v: 1,
            app_id: "123456".to_string(),
            installation_id: Some(987),
            slug: Some("intendant-example".to_string()),
            private_key_pem: crate::github_pr::client::test_rsa_pem().to_string(),
        }
    }

    fn pending_doc() -> crate::github_pr::credentials::GithubAppCredentials {
        crate::github_pr::credentials::GithubAppCredentials {
            installation_id: None,
            ..complete_doc()
        }
    }

    /// Discovery answers under the App JWT on a PENDING document — and
    /// the unseal re-establishes the runtime's pending cache (the
    /// restart-transient self-heal).
    #[tokio::test]
    async fn installations_discovery_works_on_pending_docs_and_reheals_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        seal_direct(&custody, &pending_doc());
        let fixture = spawn_fixture(HashMap::from([(
            ("GET".to_string(), "/app/installations".to_string()),
            (
                200,
                Vec::new(),
                serde_json::json!([{
                    "id": 987,
                    "account": {"login": "example-org"},
                    "app_id": 123456,
                    "app_slug": "intendant-example",
                }])
                .to_string(),
            ),
        )]))
        .await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);
        assert!(runtime.pending_install_slug().is_none(), "fresh runtime");

        let response = github_installations_api_response(&custody, &runtime).await;
        assert_eq!(status_of(&response), 200);
        let body = body_json(&response);
        assert_eq!(body["installations"][0]["installation_id"], 987);
        assert_eq!(body["installations"][0]["account_login"], "example-org");
        assert_eq!(body["installations"][0]["app_id"], 123456);
        assert_eq!(
            runtime.pending_install_slug().as_deref(),
            Some("intendant-example"),
            "the gated unseal re-established the pending cache"
        );
    }

    /// The repo listing refuses a pending document by name and lists
    /// under the installation token once the document is complete.
    #[tokio::test]
    async fn repositories_refuse_on_pending_and_list_on_complete() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        seal_direct(&custody, &pending_doc());
        let fixture = spawn_fixture(HashMap::from([
            token_route(),
            (
                ("GET".to_string(), "/installation/repositories".to_string()),
                (
                    200,
                    Vec::new(),
                    serde_json::json!({
                        "total_count": 2,
                        "repositories": [
                            {"full_name": "example-org/repo-a"},
                            {"full_name": "example-org/repo-b"},
                        ],
                    })
                    .to_string(),
                ),
            ),
        ]))
        .await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);

        let refused = github_repositories_api_response(&custody, &runtime).await;
        assert_eq!(status_of(&refused), 400);
        assert!(body_json(&refused)
            .to_string()
            .contains("installation pending"));
        assert_eq!(
            fixture.hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a pending document never mints a token"
        );

        seal_direct(&custody, &complete_doc());
        let listed = github_repositories_api_response(&custody, &runtime).await;
        assert_eq!(status_of(&listed), 200);
        assert_eq!(
            body_json(&listed)["repositories"],
            serde_json::json!(["example-org/repo-a", "example-org/repo-b"])
        );
    }

    /// A config-only save (the repo picker's write) never touches
    /// custody: the store count stays where seeding left it, the sealed
    /// document is byte-identical, and the verify exchange still runs
    /// against the sealed key.
    #[tokio::test]
    async fn repos_only_save_never_touches_custody() {
        let dir = tempfile::tempdir().unwrap();
        let custody = TempCustody::new(dir.path());
        seal_direct(&custody, &complete_doc());
        let sealed_before = custody.retrieve().unwrap();
        assert_eq!(*custody.stores.lock().unwrap(), 1, "seeding store only");
        let fixture = spawn_fixture(HashMap::from([
            token_route(),
            (
                (
                    "GET".to_string(),
                    "/repos/example-org/repo-a/pulls".to_string(),
                ),
                (200, Vec::new(), "[]".to_string()),
            ),
        ]))
        .await;
        let runtime = GithubIntegrationRuntime::new(&fixture.base);

        let payload = serde_json::json!({"repos": ["example-org/repo-a"], "poll_minutes": 7});
        let response = github_integration_save_api_response(
            payload.to_string().as_bytes(),
            Some(dir.path()),
            &custody,
            &runtime,
            "principal:test",
            "local",
        )
        .await;
        assert_eq!(status_of(&response), 200);
        let body = body_json(&response);
        assert_eq!(body["saved"], true);
        assert_eq!(body["status"], "valid", "the verify exchange ran");
        assert_eq!(body["repos"], serde_json::json!(["example-org/repo-a"]));
        assert_eq!(body["poll_minutes"], 7);
        assert_eq!(
            *custody.stores.lock().unwrap(),
            1,
            "a config-only save must never re-seal"
        );
        assert_eq!(
            custody.retrieve().unwrap(),
            sealed_before,
            "the sealed document is byte-identical"
        );
    }

    /// The lane-parity regression pin (2026-07-25 live Safari finding):
    /// the daemonApi facade yields an `{ok, status, body}` envelope on
    /// EVERY lane — the local tunnel and the Safari/mTLS HTTP fallback
    /// alike — and the github section must render the parsed BODY, never
    /// the envelope (which renders HTTP codes as status labels and
    /// undefined fields as "unconfigured"). Structural pin: the one
    /// unwrap helper exists, checks `ok` and returns `body`, and no
    /// direct github-method `daemonApi.request` call bypasses it.
    #[test]
    fn github_section_unwraps_the_daemon_api_envelope() {
        let app = include_str!("../../../../static/app.html");
        assert!(
            app.contains("async function githubIntegrationApi("),
            "the github section's envelope-unwrap helper is gone"
        );
        for marker in ["if (!resp.ok) {", "return resp.body ?? {};"] {
            assert!(
                app.contains(marker),
                "the unwrap helper lost its envelope handling: {marker:?}"
            );
        }
        assert_eq!(
            app.matches("daemonApi.request('api_github_").count(),
            0,
            "a github-section call bypasses the envelope unwrap helper — \
             it would render the envelope (HTTP status, undefined fields) \
             on both transports"
        );
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

    /// UX0 ruling pin (binding addition 1): the ceremony feedback
    /// grammar. The four presentation classes exist in the GitHub
    /// section's fragment and stylesheet, and the legacy warning class
    /// never co-occurs with the PROGRESS presentation on any line —
    /// "progress dressed as a warning" was the owner's finding 3, and a
    /// regression fails this test, not a review.
    #[test]
    fn ceremony_feedback_grammar_is_pinned() {
        let fragment = include_str!("../../../../static/app/32-vault-custody.js");
        let styles = include_str!("../../../../static/app/ui2-vault.css");
        for class in ["is-progress", "is-attention", "is-success", "is-refusal"] {
            assert!(
                fragment.contains(class),
                "32-vault-custody.js lost grammar class {class}"
            );
            assert!(
                styles.contains(&format!(".vault-chip.{class}")),
                "ui2-vault.css lost the chip rule for {class}"
            );
        }
        for (name, content) in [("32-vault-custody.js", fragment), ("ui2-vault.css", styles)] {
            for line in content.lines() {
                if line.contains("is-progress") {
                    assert!(
                        !line.contains("warn"),
                        "{name}: the warning class co-occurs with is-progress: {line}"
                    );
                }
            }
        }
    }

    /// UX0 ruling pin (return-to-context): every ceremony page with a
    /// known origin sends the owner back AT THE VAULT SECTION with the
    /// cache-bypass marker — both the link and the auto-refresh. A bare
    /// origin root-drop was finding 3's first cause.
    #[test]
    fn ceremony_page_returns_to_the_vault_section() {
        let response = ceremony_page(200, "t", "d", Some("https://box.example:8765"));
        let (status, body) = page_status(&response);
        assert_eq!(status, 200);
        let target = "https://box.example:8765/?ceremony=github#vault";
        assert!(
            body.contains(&format!("href=\"{target}\"")),
            "the return link lost the section anchor: {body}"
        );
        assert!(
            body.contains(&format!("content=\"6;url={target}\"")),
            "the auto-refresh lost the section anchor: {body}"
        );
    }
}
