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
/// touches the keystore.
pub(crate) fn github_status_body(
    settings_root: Option<&Path>,
    custody: &dyn GithubAppCustody,
    runtime: &crate::github_pr::status::GithubIntegrationRuntime,
) -> serde_json::Value {
    let present = custody.present();
    let last = runtime.last();
    let config = integration_config(settings_root);
    serde_json::json!({
        "configured": present,
        "status": crate::github_pr::status::status_label(present, last.as_ref()),
        "detail": crate::github_pr::status::status_detail(last.as_ref()),
        "checked_at_ms": last.as_ref().map(|outcome| outcome.at_unix_ms),
        "custody_backend_available": custody.backend_available(),
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
            installation_id: payload.installation_id,
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
                    existing.installation_id = payload.installation_id;
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
