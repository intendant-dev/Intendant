//! First-party ChatGPT OAuth custody for Intendant's native OpenAI transport.
//!
//! This is deliberately independent of Codex's `~/.codex/auth.json`:
//! Intendant can use the same public ChatGPT OAuth client and Codex Responses
//! service without making the Codex CLI (or its on-disk schema) part of the
//! native provider boundary. A dashboard custody lease shadows the local
//! store, exactly as API-key leases shadow environment keys.

use base64::Engine as _;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::CallerError;

pub(crate) const LEASE_KIND: &str = "oauth:openai-chatgpt";

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE: &str = "https://auth.openai.com";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const AUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);
const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const STORE_LOCK_RETRY: Duration = Duration::from_millis(25);
const STORE_VERSION: u32 = 1;
const MAX_AUTH_FILE_BYTES: u64 = 64 * 1024;
const USER_AGENT: &str = concat!("intendant/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub(crate) struct ChatGptRequestAuth {
    pub(crate) access_token: String,
    pub(crate) account_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredAuth {
    version: u32,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    account_id: String,
    expires_at_unix_ms: u64,
    updated_at_unix_ms: u64,
}

#[derive(Clone)]
struct AuthEndpoints {
    user_code: String,
    device_token: String,
    oauth_token: String,
    oauth_revoke: String,
    verification_url: String,
}

impl AuthEndpoints {
    fn production() -> Self {
        Self {
            user_code: format!("{AUTH_BASE}/api/accounts/deviceauth/usercode"),
            device_token: format!("{AUTH_BASE}/api/accounts/deviceauth/token"),
            oauth_token: format!("{AUTH_BASE}/oauth/token"),
            oauth_revoke: format!("{AUTH_BASE}/oauth/revoke"),
            verification_url: format!("{AUTH_BASE}/codex/device"),
        }
    }
}

#[derive(Serialize)]
struct UserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(
        default = "default_poll_interval",
        deserialize_with = "deserialize_poll_interval"
    )]
    interval: u64,
}

fn default_poll_interval() -> u64 {
    5
}

fn deserialize_poll_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WireInterval {
        Number(u64),
        String(String),
    }

    match WireInterval::deserialize(deserializer)? {
        WireInterval::Number(value) => Ok(value),
        WireInterval::String(value) => value.trim().parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'a str>,
}

fn local_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn lease_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn default_auth_path() -> PathBuf {
    #[cfg(test)]
    {
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        return ROOT
            .get_or_init(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0);
                std::env::temp_dir().join(format!(
                    "intendant-test-chatgpt-auth-{}-{nanos}",
                    std::process::id()
                ))
            })
            .join("openai-chatgpt.json");
    }
    #[cfg(not(test))]
    {
        crate::platform::intendant_home()
            .join("auth")
            .join("openai-chatgpt.json")
    }
}

fn auth_client() -> Client {
    Client::builder()
        .timeout(AUTH_HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn account_id_from_claims(claims: &serde_json::Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(serde_json::Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string)
}

fn account_id_from_tokens(access_token: &str, id_token: Option<&str>) -> Option<String> {
    id_token
        .and_then(jwt_claims)
        .as_ref()
        .and_then(account_id_from_claims)
        .or_else(|| {
            jwt_claims(access_token)
                .as_ref()
                .and_then(account_id_from_claims)
        })
}

fn expiry_from_jwt(token: &str) -> Option<u64> {
    jwt_claims(token)?
        .get("exp")?
        .as_u64()
        .and_then(|seconds| seconds.checked_mul(1_000))
}

fn nonempty_string_at(value: &serde_json::Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer))
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// Parse Intendant's account-level schema plus the equivalent Codex token
/// nesting. The latter is an import/lease compatibility edge only; native
/// operation never reads or writes Codex's auth file.
fn parse_auth_material(material: &str) -> Result<StoredAuth, String> {
    let value: serde_json::Value = serde_json::from_str(material)
        .map_err(|error| format!("ChatGPT OAuth material is not valid JSON: {error}"))?;
    if let Some(version) = value.get("version") {
        let version = version
            .as_u64()
            .ok_or_else(|| "ChatGPT OAuth store version must be an unsigned integer".to_string())?;
        if version != u64::from(STORE_VERSION) {
            return Err(format!(
                "unsupported ChatGPT OAuth store version {version}; expected {STORE_VERSION}"
            ));
        }
    }
    let access_token = nonempty_string_at(&value, &["/access_token", "/tokens/access_token"])
        .ok_or_else(|| "ChatGPT OAuth material has no access token".to_string())?;
    let refresh_token = nonempty_string_at(&value, &["/refresh_token", "/tokens/refresh_token"]);
    let id_token = nonempty_string_at(&value, &["/id_token", "/tokens/id_token"]);
    let account_id = nonempty_string_at(&value, &["/account_id", "/tokens/account_id"])
        .or_else(|| account_id_from_tokens(&access_token, id_token.as_deref()))
        .ok_or_else(|| {
            "ChatGPT OAuth material has no account id and its tokens carry no account claim"
                .to_string()
        })?;
    let expires_at_unix_ms = value
        .get("expires_at_unix_ms")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| expiry_from_jwt(&access_token))
        .ok_or_else(|| {
            "ChatGPT OAuth material has no expiry and its access token has no exp claim".to_string()
        })?;
    let updated_at_unix_ms = value
        .get("updated_at_unix_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(now_unix_ms);
    Ok(StoredAuth {
        version: STORE_VERSION,
        access_token,
        refresh_token,
        account_id,
        expires_at_unix_ms,
        updated_at_unix_ms,
    })
}

fn request_auth_from_stored(auth: &StoredAuth) -> ChatGptRequestAuth {
    ChatGptRequestAuth {
        access_token: auth.access_token.clone(),
        account_id: auth.account_id.clone(),
    }
}

fn access_is_current(auth: &StoredAuth, now: u64, refresh_early: bool) -> bool {
    let skew = if refresh_early && auth.refresh_token.is_some() {
        REFRESH_SKEW.as_millis() as u64
    } else {
        0
    };
    auth.expires_at_unix_ms > now.saturating_add(skew)
}

fn auth_is_usable(auth: &StoredAuth) -> bool {
    access_is_current(auth, now_unix_ms(), false) || auth.refresh_token.is_some()
}

fn validate_regular_or_absent_leaf(path: &Path, label: &str) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect {label} {}: {error}", path.display())),
    };
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?
        || !metadata.file_type().is_file()
    {
        return Err(format!(
            "refuse {label} that is not a regular file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn read_auth_file(path: &Path) -> Result<Option<StoredAuth>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        || !metadata.file_type().is_file()
    {
        return Err(format!(
            "refuse ChatGPT auth store that is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_AUTH_FILE_BYTES {
        return Err(format!(
            "refuse ChatGPT auth store larger than {MAX_AUTH_FILE_BYTES} bytes: {}",
            path.display()
        ));
    }
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut material = String::new();
    file.read_to_string(&mut material)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_auth_material(&material).map(Some)
}

fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)
            .map_err(|error| format!("stat {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .map_err(|error| format!("chmod 0600 {}: {error}", path.display()))
    }
    #[cfg(windows)]
    {
        crate::platform::set_owner_private_permissions(path)
            .map_err(|error| format!("protect file ACL {}: {error}", path.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

fn sync_parent(parent: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync {}: {error}", parent.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn write_auth_file(path: &Path, auth: &StoredAuth) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("ChatGPT auth path has no parent: {}", path.display()))?;
    intendant_core::state_paths::create_private_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    validate_regular_or_absent_leaf(path, "ChatGPT auth store")?;
    let mut body = serde_json::to_vec_pretty(auth).map_err(|error| error.to_string())?;
    body.push(b'\n');
    let mut temporary = tempfile::Builder::new()
        .prefix(".openai-chatgpt-")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "create temporary auth file in {}: {error}",
                parent.display()
            )
        })?;
    temporary
        .write_all(&body)
        .map_err(|error| format!("write temporary auth file: {error}"))?;
    set_private_file_permissions(temporary.path())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("sync temporary auth file: {error}"))?;
    let persisted = temporary
        .persist(path)
        .map_err(|error| format!("atomically replace {}: {}", path.display(), error.error))?;
    persisted
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    sync_parent(parent)
}

struct AuthStoreLock {
    file: File,
}

impl AuthStoreLock {
    fn acquire(parent: &Path) -> Result<Self, String> {
        intendant_core::state_paths::create_private_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let lock_path = parent.join(".openai-chatgpt.lock");
        validate_regular_or_absent_leaf(&lock_path, "ChatGPT auth lock")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
        set_private_file_permissions(&lock_path)?;
        let started = Instant::now();
        loop {
            match File::try_lock(&file) {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= STORE_LOCK_TIMEOUT {
                        return Err(format!(
                            "timed out waiting for ChatGPT auth lock {}",
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(STORE_LOCK_RETRY);
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("lock {}: {error}", lock_path.display()))
                }
            }
        }
    }
}

impl Drop for AuthStoreLock {
    fn drop(&mut self) {
        let _ = File::unlock(&self.file);
    }
}

fn service_error(status: StatusCode, body: &str, operation: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let detail = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("error_description"))
                .or_else(|| value.get("message"))
        })
        .and_then(serde_json::Value::as_str)
        .map(|message| {
            message
                .chars()
                .filter(|character| !character.is_control())
                .take(512)
                .collect::<String>()
        });
    match detail {
        Some(detail) if !detail.is_empty() => format!("{operation} failed ({status}): {detail}"),
        _ => format!("{operation} failed ({status})"),
    }
}

async fn refresh_auth(
    client: &Client,
    endpoints: &AuthEndpoints,
    previous: &StoredAuth,
) -> Result<StoredAuth, String> {
    let refresh_token = previous.refresh_token.as_deref().ok_or_else(|| {
        "the ChatGPT access token cannot be refreshed; sign in again or reconnect the custody lease"
            .to_string()
    })?;
    let response = client
        .post(&endpoints.oauth_token)
        .header("originator", "intendant")
        .json(&RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await
        .map_err(|error| format!("refresh ChatGPT access token: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("read ChatGPT refresh response: {error}"))?;
    if !status.is_success() {
        return Err(service_error(status, &body, "ChatGPT token refresh"));
    }
    let tokens: OAuthTokenResponse = serde_json::from_str(&body)
        .map_err(|error| format!("parse ChatGPT refresh response: {error}"))?;
    auth_from_token_response(tokens, Some(previous))
}

fn auth_from_token_response(
    tokens: OAuthTokenResponse,
    previous: Option<&StoredAuth>,
) -> Result<StoredAuth, String> {
    let access_token = tokens
        .access_token
        .or_else(|| previous.map(|auth| auth.access_token.clone()))
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| "ChatGPT token response did not contain an access token".to_string())?;
    let refresh_token = tokens
        .refresh_token
        .or_else(|| previous.and_then(|auth| auth.refresh_token.clone()))
        .filter(|token| !token.trim().is_empty());
    let id_token = tokens.id_token.filter(|token| !token.trim().is_empty());
    let account_id = account_id_from_tokens(&access_token, id_token.as_deref())
        .or_else(|| previous.map(|auth| auth.account_id.clone()))
        .filter(|account_id| !account_id.trim().is_empty())
        .ok_or_else(|| "ChatGPT token response carried no account id".to_string())?;
    let now = now_unix_ms();
    let expires_at_unix_ms = expiry_from_jwt(&access_token)
        .or_else(|| {
            tokens
                .expires_in
                .and_then(|seconds| seconds.checked_mul(1_000))
                .and_then(|duration| now.checked_add(duration))
        })
        .ok_or_else(|| "ChatGPT token response carried no access-token expiry".to_string())?;
    Ok(StoredAuth {
        version: STORE_VERSION,
        access_token,
        refresh_token,
        account_id,
        expires_at_unix_ms,
        updated_at_unix_ms: now,
    })
}

async fn request_from_lease(
    snapshot: crate::credential_leases::LeasedSecretSnapshot,
    rejected_access_token: Option<&str>,
    client: &Client,
    endpoints: &AuthEndpoints,
) -> Result<ChatGptRequestAuth, CallerError> {
    let candidate = parse_auth_material(&snapshot.material).map_err(CallerError::Config)?;
    let now = now_unix_ms();
    if rejected_access_token.is_some_and(|rejected| rejected != candidate.access_token.as_str())
        && access_is_current(&candidate, now, false)
    {
        // A concurrent 401 recovery already rotated this lease while the
        // caller waited for the refresh lock. Reuse it; rotating again would
        // needlessly consume another single-use refresh token.
        return Ok(request_auth_from_stored(&candidate));
    }
    let force_refresh = rejected_access_token.is_some();
    if force_refresh && candidate.refresh_token.is_none() {
        return Err(CallerError::Config(
            "ChatGPT rejected an access-token-only custody lease; reconnect the lease with a fresh access token"
                .to_string(),
        ));
    }
    if !force_refresh && access_is_current(&candidate, now, true) {
        return Ok(request_auth_from_stored(&candidate));
    }
    if candidate.refresh_token.is_none() && access_is_current(&candidate, now, false) {
        return Ok(request_auth_from_stored(&candidate));
    }
    let refreshed = refresh_auth(client, endpoints, &candidate)
        .await
        .map_err(CallerError::Config)?;
    let replacement = serde_json::to_string(&refreshed).map_err(CallerError::Json)?;
    let rotated = crate::credential_leases::rotate_leased_secret_if_current(
        LEASE_KIND,
        &snapshot.lease_id,
        replacement,
    )
    .map_err(CallerError::Config)?;
    if !rotated {
        return Err(CallerError::Config(
            "ChatGPT custody lease changed while its token was refreshing; reconnect the lease"
                .to_string(),
        ));
    }
    Ok(request_auth_from_stored(&refreshed))
}

async fn request_from_local_store(
    path: PathBuf,
    rejected_access_token: Option<&str>,
    client: &Client,
    endpoints: &AuthEndpoints,
) -> Result<ChatGptRequestAuth, CallerError> {
    let _process_guard = local_refresh_lock().lock().await;
    let parent = path.parent().ok_or_else(|| {
        CallerError::Config(format!(
            "ChatGPT auth path has no parent: {}",
            path.display()
        ))
    })?;
    let parent = parent.to_path_buf();
    let _store_guard = tokio::task::spawn_blocking(move || AuthStoreLock::acquire(&parent))
        .await
        .map_err(|error| CallerError::Config(format!("join ChatGPT auth lock task: {error}")))?
        .map_err(CallerError::Config)?;
    let auth = read_auth_file(&path)
        .map_err(CallerError::Config)?
        .ok_or_else(|| {
            CallerError::Config(
                "ChatGPT OAuth is not signed in; run `intendant auth chatgpt login`".to_string(),
            )
        })?;
    let now = now_unix_ms();
    if rejected_access_token.is_some_and(|rejected| rejected != auth.access_token.as_str())
        && access_is_current(&auth, now, false)
    {
        return Ok(request_auth_from_stored(&auth));
    }
    let force_refresh = rejected_access_token.is_some();
    if force_refresh && auth.refresh_token.is_none() {
        return Err(CallerError::Config(
            "ChatGPT rejected a non-refreshable local access token; run `intendant auth chatgpt login`"
                .to_string(),
        ));
    }
    if !force_refresh && access_is_current(&auth, now, true) {
        return Ok(request_auth_from_stored(&auth));
    }
    if auth.refresh_token.is_none() && access_is_current(&auth, now, false) {
        return Ok(request_auth_from_stored(&auth));
    }
    let refreshed = refresh_auth(client, endpoints, &auth)
        .await
        .map_err(CallerError::Config)?;
    write_auth_file(&path, &refreshed).map_err(CallerError::Config)?;
    Ok(request_auth_from_stored(&refreshed))
}

async fn request_auth_with(
    rejected_access_token: Option<&str>,
    path: PathBuf,
    client: &Client,
    endpoints: &AuthEndpoints,
) -> Result<ChatGptRequestAuth, CallerError> {
    {
        // The guard spans provider refresh + lease compare-and-swap, so two
        // concurrent requests can never spend the same rotating refresh token.
        let _lease_guard = lease_refresh_lock().lock().await;
        if let Some(snapshot) = crate::credential_leases::leased_secret_snapshot(LEASE_KIND) {
            return request_from_lease(snapshot, rejected_access_token, client, endpoints).await;
        }
    }
    request_from_local_store(path, rejected_access_token, client, endpoints).await
}

pub(crate) async fn request_auth() -> Result<ChatGptRequestAuth, CallerError> {
    request_auth_with(
        None,
        default_auth_path(),
        &auth_client(),
        &AuthEndpoints::production(),
    )
    .await
}

pub(crate) async fn request_auth_after_unauthorized(
    rejected_access_token: &str,
) -> Result<ChatGptRequestAuth, CallerError> {
    request_auth_with(
        Some(rejected_access_token),
        default_auth_path(),
        &auth_client(),
        &AuthEndpoints::production(),
    )
    .await
}

fn local_auth_available_at(path: &Path) -> bool {
    read_auth_file(path)
        .ok()
        .flatten()
        .as_ref()
        .is_some_and(auth_is_usable)
}

pub(crate) fn available() -> bool {
    crate::credential_leases::kind_is_active(LEASE_KIND)
        || local_auth_available_at(&default_auth_path())
}

async fn request_user_code(
    client: &Client,
    endpoints: &AuthEndpoints,
) -> Result<UserCodeResponse, String> {
    let response = client
        .post(&endpoints.user_code)
        .header("originator", "intendant")
        .json(&UserCodeRequest {
            client_id: CLIENT_ID,
        })
        .send()
        .await
        .map_err(|error| format!("request ChatGPT device code: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("read ChatGPT device-code response: {error}"))?;
    if !status.is_success() {
        return Err(service_error(status, &body, "ChatGPT device-code request"));
    }
    serde_json::from_str(&body)
        .map_err(|error| format!("parse ChatGPT device-code response: {error}"))
}

async fn poll_for_authorization_code(
    client: &Client,
    endpoints: &AuthEndpoints,
    device: &UserCodeResponse,
) -> Result<DeviceTokenResponse, String> {
    let started = Instant::now();
    let interval = Duration::from_secs(device.interval.max(1));
    loop {
        let response = client
            .post(&endpoints.device_token)
            .header("originator", "intendant")
            .json(&DeviceTokenRequest {
                device_auth_id: &device.device_auth_id,
                user_code: &device.user_code,
            })
            .send()
            .await
            .map_err(|error| format!("poll ChatGPT device authorization: {error}"))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|error| format!("parse ChatGPT device authorization: {error}"));
        }
        if status != StatusCode::FORBIDDEN && status != StatusCode::NOT_FOUND {
            let body = response.text().await.unwrap_or_default();
            return Err(service_error(status, &body, "ChatGPT device authorization"));
        }
        if started.elapsed() >= DEVICE_TIMEOUT {
            return Err("ChatGPT device authorization timed out after 15 minutes".to_string());
        }
        tokio::time::sleep(interval.min(DEVICE_TIMEOUT.saturating_sub(started.elapsed()))).await;
    }
}

async fn exchange_authorization_code(
    client: &Client,
    endpoints: &AuthEndpoints,
    code: DeviceTokenResponse,
) -> Result<StoredAuth, String> {
    let redirect_uri = format!("{AUTH_BASE}/deviceauth/callback");
    let response = client
        .post(&endpoints.oauth_token)
        .header("originator", "intendant")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.authorization_code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", CLIENT_ID),
            ("code_verifier", code.code_verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|error| format!("exchange ChatGPT device authorization: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("read ChatGPT token response: {error}"))?;
    if !status.is_success() {
        return Err(service_error(status, &body, "ChatGPT token exchange"));
    }
    let tokens: OAuthTokenResponse = serde_json::from_str(&body)
        .map_err(|error| format!("parse ChatGPT token response: {error}"))?;
    auth_from_token_response(tokens, None)
}

async fn login(path: &Path) -> Result<(), String> {
    let client = auth_client();
    let endpoints = AuthEndpoints::production();
    let device = request_user_code(&client, &endpoints).await?;
    println!("Sign in to ChatGPT for Intendant Native:");
    println!();
    println!("  1. Open {}", endpoints.verification_url);
    println!("  2. Enter code: {}", device.user_code);
    println!();
    println!(
        "Continue only if you started this login from Intendant. The code expires in 15 minutes."
    );
    println!("Waiting for authorization…");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("flush login prompt: {error}"))?;
    let code = poll_for_authorization_code(&client, &endpoints, &device).await?;
    let auth = exchange_authorization_code(&client, &endpoints, code).await?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("ChatGPT auth path has no parent: {}", path.display()))?;
    let _lock = AuthStoreLock::acquire(parent)?;
    write_auth_file(path, &auth)?;
    println!("Signed in. Credentials stored at {}", path.display());
    Ok(())
}

fn masked_account_id(account_id: &str) -> String {
    if account_id.chars().count() <= 12 {
        return "<redacted>".to_string();
    }
    let prefix: String = account_id.chars().take(6).collect();
    let suffix: String = account_id
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn print_status(path: &Path) -> Result<(), String> {
    if crate::credential_leases::kind_is_active(LEASE_KIND) {
        println!("ChatGPT OAuth: signed in via active custody lease");
        println!("Effective source: {LEASE_KIND} (lease shadows local auth)");
        return Ok(());
    }
    let Some(auth) = read_auth_file(path)? else {
        println!("ChatGPT OAuth: not signed in");
        println!("Run: intendant auth chatgpt login");
        return Ok(());
    };
    println!("ChatGPT OAuth: signed in locally");
    println!("Account: {}", masked_account_id(&auth.account_id));
    println!("Access token expires: {}", auth.expires_at_unix_ms);
    println!("Store: {}", path.display());
    Ok(())
}

async fn revoke_auth(
    client: &Client,
    endpoints: &AuthEndpoints,
    auth: &StoredAuth,
) -> Result<(), String> {
    let (token, token_type_hint, client_id) = match auth.refresh_token.as_deref() {
        Some(refresh_token) => (refresh_token, "refresh_token", Some(CLIENT_ID)),
        None => (auth.access_token.as_str(), "access_token", None),
    };
    let response = client
        .post(&endpoints.oauth_revoke)
        .header("originator", "intendant")
        .json(&RevokeRequest {
            token,
            token_type_hint,
            client_id,
        })
        .send()
        .await
        .map_err(|error| format!("revoke ChatGPT token: {error}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(service_error(status, &body, "ChatGPT token revocation"))
}

async fn logout(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("ChatGPT auth path has no parent: {}", path.display()))?;
    let _lock = AuthStoreLock::acquire(parent)?;
    let Some(auth) = read_auth_file(path)? else {
        println!("No local ChatGPT OAuth login was present.");
        if crate::credential_leases::kind_is_active(LEASE_KIND) {
            println!("An active custody lease remains effective; revoke it from its custodian.");
        }
        return Ok(());
    };
    let revoke_error = revoke_auth(&auth_client(), &AuthEndpoints::production(), &auth)
        .await
        .err();
    std::fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))?;
    sync_parent(parent)?;
    println!("Removed local ChatGPT OAuth credentials.");
    if let Some(error) = revoke_error {
        eprintln!("warning: remote token revocation failed; local credentials were still removed: {error}");
    }
    if crate::credential_leases::kind_is_active(LEASE_KIND) {
        println!("An active custody lease remains effective; revoke it from its custodian.");
    }
    Ok(())
}

pub(crate) async fn run_cli(args: Vec<String>) -> Result<(), String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["chatgpt", "login"] => login(&default_auth_path()).await,
        ["chatgpt", "status"] => print_status(&default_auth_path()),
        ["chatgpt", "logout"] => logout(&default_auth_path()).await,
        _ => Err("usage: intendant auth chatgpt <login|status|logout>\n\
             login uses OpenAI's device-code flow and stores Intendant-owned credentials"
            .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn one_shot_json_server(
        response_body: String,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4_096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/oauth/token"), server)
    }

    fn jwt(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.signature")
    }

    fn fixture_auth(expires_at_unix_ms: u64) -> StoredAuth {
        let access_token = jwt(serde_json::json!({
            "exp": expires_at_unix_ms / 1_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": "account-fixture-123456"}
        }));
        StoredAuth {
            version: STORE_VERSION,
            access_token,
            refresh_token: Some("refresh-fixture".to_string()),
            account_id: "account-fixture-123456".to_string(),
            expires_at_unix_ms,
            updated_at_unix_ms: now_unix_ms(),
        }
    }

    #[test]
    fn extracts_every_observed_account_claim_shape() {
        for claims in [
            serde_json::json!({"chatgpt_account_id": "top"}),
            serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "nested"}
            }),
            serde_json::json!({"organizations": [{"id": "organization"}]}),
        ] {
            assert!(account_id_from_claims(&claims).is_some(), "{claims}");
        }
    }

    #[test]
    fn parses_intendant_and_codex_material_shapes() {
        let expiry = now_unix_ms() + 3_600_000;
        let auth = fixture_auth(expiry);
        let material = serde_json::to_string(&auth).unwrap();
        let parsed = parse_auth_material(&material).unwrap();
        assert_eq!(parsed.account_id, "account-fixture-123456");
        assert_eq!(parsed.expires_at_unix_ms, expiry);

        let codex = serde_json::json!({
            "tokens": {
                "access_token": auth.access_token,
                "refresh_token": "refresh-codex",
                "account_id": "account-codex"
            }
        });
        let parsed = parse_auth_material(&codex.to_string()).unwrap();
        assert_eq!(parsed.account_id, "account-codex");
        assert_eq!(parsed.refresh_token.as_deref(), Some("refresh-codex"));

        for version in [serde_json::json!(2), serde_json::json!("1")] {
            let mut incompatible = serde_json::to_value(&auth).unwrap();
            incompatible["version"] = version;
            let error = match parse_auth_material(&incompatible.to_string()) {
                Err(error) => error,
                Ok(_) => panic!("incompatible store version must fail"),
            };
            assert!(error.contains("version"), "{error}");
        }
    }

    #[test]
    fn access_only_material_uses_full_lifetime_without_refresh_skew() {
        let now = now_unix_ms();
        let mut auth = fixture_auth(now + 60_000);
        auth.refresh_token = None;
        assert!(access_is_current(&auth, now, true));
        auth.expires_at_unix_ms = now.saturating_sub(1);
        assert!(!access_is_current(&auth, now, false));
        assert!(!auth_is_usable(&auth));
    }

    #[test]
    fn private_store_round_trips_and_rejects_symlink_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let auth = fixture_auth(now_unix_ms() + 3_600_000);
        write_auth_file(&path, &auth).unwrap();
        let loaded = read_auth_file(&path).unwrap().unwrap();
        assert_eq!(loaded.account_id, auth.account_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt as _};
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let target = directory.path().join("target.json");
            std::fs::write(&target, serde_json::to_vec(&auth).unwrap()).unwrap();
            let link = directory.path().join("link.json");
            symlink(&target, &link).unwrap();
            let error = match read_auth_file(&link) {
                Err(error) => error,
                Ok(_) => panic!("symlink auth leaf must be rejected"),
            };
            assert!(error.contains("regular file"), "{error}");

            let broken = directory.path().join("broken.json");
            symlink(directory.path().join("missing-target"), &broken).unwrap();
            let error = write_auth_file(&broken, &auth).unwrap_err();
            assert!(error.contains("regular file"), "{error}");
        }
    }

    #[test]
    fn token_response_rotation_preserves_omitted_refresh_token() {
        let previous = fixture_auth(now_unix_ms() + 1_000);
        let new_expiry = now_unix_ms() / 1_000 + 3_600;
        let access_token = jwt(serde_json::json!({
            "exp": new_expiry,
            "chatgpt_account_id": "account-rotated"
        }));
        let refreshed = auth_from_token_response(
            OAuthTokenResponse {
                id_token: None,
                access_token: Some(access_token),
                refresh_token: None,
                expires_in: None,
            },
            Some(&previous),
        )
        .unwrap();
        assert_eq!(refreshed.refresh_token, previous.refresh_token);
        assert_eq!(refreshed.account_id, "account-rotated");
        assert_eq!(refreshed.expires_at_unix_ms, new_expiry * 1_000);
    }

    #[tokio::test]
    async fn local_refresh_rotates_atomically_without_persisting_id_token() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        write_auth_file(&path, &fixture_auth(now_unix_ms().saturating_sub(1_000))).unwrap();

        let new_expiry = now_unix_ms() / 1_000 + 3_600;
        let access_token = jwt(serde_json::json!({
            "exp": new_expiry,
            "chatgpt_account_id": "account-refreshed-123456"
        }));
        let response = serde_json::json!({
            "access_token": access_token,
            "refresh_token": "refresh-rotated",
            "id_token": "id-secret-must-not-persist"
        })
        .to_string();
        let (oauth_token, server) = one_shot_json_server(response).await;
        let endpoints = AuthEndpoints {
            user_code: oauth_token.clone(),
            device_token: oauth_token.clone(),
            oauth_token,
            oauth_revoke: "http://127.0.0.1/unused".to_string(),
            verification_url: "http://127.0.0.1/unused".to_string(),
        };

        let request_auth = request_from_local_store(path.clone(), None, &auth_client(), &endpoints)
            .await
            .unwrap();
        assert_eq!(request_auth.account_id, "account-refreshed-123456");

        let request = server.await.unwrap();
        let body_start = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()
            + 4;
        let request_body: serde_json::Value =
            serde_json::from_slice(&request[body_start..]).unwrap();
        assert_eq!(request_body["client_id"], CLIENT_ID);
        assert_eq!(request_body["grant_type"], "refresh_token");
        assert_eq!(request_body["refresh_token"], "refresh-fixture");

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("refresh-rotated"), "{persisted}");
        assert!(!persisted.contains("id_token"), "{persisted}");
        assert!(
            !persisted.contains("id-secret-must-not-persist"),
            "{persisted}"
        );
    }

    #[test]
    fn interval_accepts_string_or_number() {
        let string: UserCodeResponse = serde_json::from_value(serde_json::json!({
            "device_auth_id": "device",
            "user_code": "CODE",
            "interval": "7"
        }))
        .unwrap();
        let number: UserCodeResponse = serde_json::from_value(serde_json::json!({
            "device_auth_id": "device",
            "user_code": "CODE",
            "interval": 3
        }))
        .unwrap();
        assert_eq!(string.interval, 7);
        assert_eq!(number.interval, 3);
    }

    #[test]
    fn status_masks_account_identity() {
        assert_eq!(masked_account_id("short"), "<redacted>");
        assert_eq!(masked_account_id("account-fixture-123456"), "accoun…3456");
    }
}
