//! The App-auth client: a short-lived RS256 JWT (pure `ring`, no
//! OpenSSL) exchanged for a cached installation token, then conditional
//! REST reads over the in-tree reqwest/rustls lane. Every failure is a
//! named class ([`ApiError`]) the status surface renders honestly;
//! nothing here retries, stores, or falls back — pacing and degrade
//! policy belong to the callers (the save-time verification now, the
//! scanner in the next slice).

use base64::Engine as _;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::credentials::GithubAppCredentials;

/// The real API host; tests and rigs construct clients against a
/// fixture base instead — nothing reads this through an env override.
pub(crate) const GITHUB_API_BASE: &str = "https://api.github.com";

const API_VERSION: &str = "2022-11-28";
/// Mint a fresh installation token once the cached one has less than
/// this much life left (tokens live one hour).
const TOKEN_REFRESH_MARGIN_S: u64 = 300;
/// JWTs are backdated 60 s against clock skew and kept well under
/// GitHub's 10-minute ceiling: the full iat→exp span is 540 s, a
/// minute of margin against the 600 s maximum.
const JWT_BACKDATE_S: u64 = 60;
const JWT_LIFETIME_S: u64 = 480;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Pagination bound for one list read — a repo with more than
/// `100 × MAX_LIST_PAGES` open PRs is not a repo this integration can
/// mirror honestly, and an unbounded follow of `Link:` headers is a
/// hang waiting for a hostile server.
const MAX_LIST_PAGES: usize = 10;

/// One named failure class per degrade lane the status surface knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// Network trouble, timeouts, 5xx — transient; try again later.
    Unreachable(String),
    /// 401/403/404: bad or revoked credentials, missing permission,
    /// unknown installation. Stays until configuration changes.
    Denied(String),
    /// Primary or secondary rate limit; honor the server's delay.
    RateLimited {
        retry_after_s: Option<u64>,
        message: String,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unreachable(message) => write!(f, "unreachable: {message}"),
            ApiError::Denied(message) => write!(f, "denied: {message}"),
            ApiError::RateLimited {
                retry_after_s,
                message,
            } => match retry_after_s {
                Some(seconds) => write!(f, "rate limited (retry after {seconds}s): {message}"),
                None => write!(f, "rate limited: {message}"),
            },
        }
    }
}

/// A conditional read's outcome: the server either confirmed the cached
/// view (`NotModified`) or served a fresh value with its new validator.
pub(crate) enum Conditional<T> {
    NotModified,
    Fresh { value: T, etag: Option<String> },
}

/// Parse a PEM private key into a `ring` RSA signing key. GitHub ships
/// App keys as PKCS#1 (`BEGIN RSA PRIVATE KEY`); PKCS#8
/// (`BEGIN PRIVATE KEY`) re-wraps are accepted too.
pub(crate) fn rsa_key_from_pem(pem_text: &str) -> Result<ring::signature::RsaKeyPair, String> {
    let parsed = pem::parse(pem_text).map_err(|error| format!("private key PEM: {error}"))?;
    match parsed.tag() {
        "RSA PRIVATE KEY" => ring::signature::RsaKeyPair::from_der(parsed.contents())
            .map_err(|error| format!("private key (PKCS#1): {error}")),
        "PRIVATE KEY" => ring::signature::RsaKeyPair::from_pkcs8(parsed.contents())
            .map_err(|error| format!("private key (PKCS#8): {error}")),
        other => Err(format!(
            "unsupported PEM block {other:?} — expected an RSA private key"
        )),
    }
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub(crate) fn unix_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint the App JWT: `iss` = App ID, backdated `iat`, short `exp`,
/// RS256 over the standard two-segment signing input.
pub(crate) fn mint_app_jwt(
    credentials: &GithubAppCredentials,
    now_unix_s: u64,
) -> Result<String, String> {
    let key = rsa_key_from_pem(&credentials.private_key_pem)?;
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iat": now_unix_s.saturating_sub(JWT_BACKDATE_S),
        "exp": now_unix_s + JWT_LIFETIME_S,
        "iss": credentials.app_id,
    });
    let signing_input = format!("{header}.{}", b64url(claims.to_string().as_bytes()));
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "RSA signing failed".to_string())?;
    Ok(format!("{signing_input}.{}", b64url(&signature)))
}

struct CachedToken {
    token: String,
    expires_unix_s: u64,
}

/// One PR as the list endpoint serves it — the tier-1 fields the
/// scanner and the render join consume. Unknown fields are ignored.
/// Production reads only the list's length until the scanner slice
/// lands; the fields are that slice's seam (and are pinned by the
/// fixture tests today).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PrSummary {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) draft: bool,
    pub(crate) html_url: String,
    #[serde(default)]
    pub(crate) user: PrActor,
    pub(crate) head: PrBranch,
    pub(crate) base: PrBranch,
    #[serde(default)]
    pub(crate) updated_at: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PrActor {
    #[serde(default)]
    pub(crate) login: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PrBranch {
    #[serde(rename = "ref")]
    pub(crate) branch: String,
}

/// One PR's detail view — the terminal-state fields the scanner records
/// in its completion annotation, plus what the render join serves on
/// expand. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PullDetail {
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) merged: bool,
    #[serde(default)]
    pub(crate) merge_commit_sha: Option<String>,
    #[serde(default)]
    pub(crate) merged_at: Option<String>,
    #[serde(default)]
    pub(crate) closed_at: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) draft: bool,
    /// `null` while GitHub computes it — absent data claims nothing.
    #[serde(default)]
    pub(crate) mergeable: Option<bool>,
    #[serde(default)]
    pub(crate) head: Option<PrHead>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PrHead {
    #[serde(default)]
    pub(crate) sha: Option<String>,
}

/// Check-runs rollup for one head sha — counts, never the run list.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ChecksSummary {
    pub(crate) total: usize,
    pub(crate) completed: usize,
    pub(crate) failed: usize,
    pub(crate) succeeded: usize,
}

/// Latest-review-per-reviewer rollup.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ReviewSummary {
    pub(crate) approved: usize,
    pub(crate) changes_requested: usize,
    pub(crate) commented: usize,
}

/// One installation of the App, as the discovery surface renders it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct InstallationSummary {
    pub(crate) installation_id: u64,
    pub(crate) account_login: String,
    /// GitHub also echoes the App id per row; the auto-fill save wants
    /// it, and the sealed document is never unsealed for a status read.
    pub(crate) app_id: Option<u64>,
}

pub(crate) struct GithubAppClient {
    http: reqwest::Client,
    api_base: String,
    credentials: GithubAppCredentials,
    token: tokio::sync::Mutex<Option<CachedToken>>,
}

impl GithubAppClient {
    pub(crate) fn new(
        api_base: impl Into<String>,
        credentials: GithubAppCredentials,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent("intendant")
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| format!("http client: {error}"))?;
        Ok(Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_string(),
            credentials,
            token: tokio::sync::Mutex::new(None),
        })
    }

    /// One real round-trip proving the credentials work end to end:
    /// mint the JWT, exchange it for an installation token.
    pub(crate) async fn verify(&self) -> Result<(), ApiError> {
        self.installation_token().await.map(|_| ())
    }

    /// The cached installation token, minting through the keystore-held
    /// key only when the cache is empty or near expiry. The mutex is
    /// held across the mint so concurrent callers produce one exchange,
    /// not one each.
    async fn installation_token(&self) -> Result<String, ApiError> {
        let mut slot = self.token.lock().await;
        let now = unix_now_s();
        if let Some(cached) = slot.as_ref() {
            if cached.expires_unix_s > now + TOKEN_REFRESH_MARGIN_S {
                return Ok(cached.token.clone());
            }
        }
        // A pending-install document has no installation to mint against;
        // the named refusal keeps the ceremony phase honest (Denied stays
        // until the install completes the document).
        let Some(installation_id) = self.credentials.installation_id else {
            return Err(ApiError::Denied(
                "installation pending — install the App on GitHub and finish discovery".to_string(),
            ));
        };
        let jwt = mint_app_jwt(&self.credentials, now).map_err(ApiError::Denied)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.api_base, installation_id
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|error| ApiError::Unreachable(error.to_string()))?;
        let response = classify(response).await?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| ApiError::Unreachable(format!("token response: {error}")))?;
        let token = body
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiError::Unreachable("token response missing `token`".to_string()))?
            .to_string();
        let expires_unix_s = body
            .get("expires_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp().max(0) as u64)
            .unwrap_or(now + 3300);
        *slot = Some(CachedToken {
            token: token.clone(),
            expires_unix_s,
        });
        Ok(token)
    }

    /// The App's installations, under the App JWT alone — the discovery
    /// read of the connect ceremony, so it works on a pending-install
    /// document (no installation token exists yet). One page of 100:
    /// a private per-daemon App has one installation in practice; a
    /// hundred is not a shape this integration mirrors.
    pub(crate) async fn list_installations(&self) -> Result<Vec<InstallationSummary>, ApiError> {
        let jwt = mint_app_jwt(&self.credentials, unix_now_s()).map_err(ApiError::Denied)?;
        let url = format!("{}/app/installations?per_page=100", self.api_base);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|error| ApiError::Unreachable(error.to_string()))?;
        let response = classify(response).await?;
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|error| ApiError::Unreachable(format!("installations body: {error}")))?;
        let rows = value
            .as_array()
            .ok_or_else(|| ApiError::Unreachable("installations shape: not a list".to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(InstallationSummary {
                    installation_id: row.get("id")?.as_u64()?,
                    account_login: row
                        .get("account")
                        .and_then(|a| a.get("login"))
                        .and_then(|l| l.as_str())
                        .unwrap_or("")
                        .to_string(),
                    app_id: row.get("app_id").and_then(|v| v.as_u64()),
                })
            })
            .collect())
    }

    /// Repositories the installation can see (the repo picker's read):
    /// installation token, `full_name`s only, bounded pagination like
    /// the PR list.
    pub(crate) async fn list_installation_repositories(&self) -> Result<Vec<String>, ApiError> {
        let mut names = Vec::new();
        let mut url = format!("{}/installation/repositories?per_page=100", self.api_base);
        for _ in 0..MAX_LIST_PAGES {
            let value = match self.get_value(&url, None).await? {
                Conditional::NotModified => {
                    return Err(ApiError::Unreachable(
                        "unconditional read answered 304".to_string(),
                    ))
                }
                Conditional::Fresh { value, .. } => value,
            };
            let (page, next) = match (value.get("__page"), value.get("__next")) {
                (Some(page), Some(next)) => (page.clone(), next.as_str().map(str::to_string)),
                _ => (value, None),
            };
            if let Some(rows) = page.get("repositories").and_then(|r| r.as_array()) {
                names.extend(rows.iter().filter_map(|row| {
                    row.get("full_name")
                        .and_then(|n| n.as_str())
                        .map(str::to_string)
                }));
            }
            match next {
                Some(next_url) => url = next_url,
                None => break,
            }
        }
        Ok(names)
    }

    /// Conditional GET of an absolute API URL. `etag` rides
    /// `If-None-Match`; a 304 answers `NotModified` without a body.
    /// URLs are pinned to this client's base — a `Link:` header cannot
    /// steer reads off-host.
    async fn get_value(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<Conditional<serde_json::Value>, ApiError> {
        if !url.starts_with(&self.api_base) {
            return Err(ApiError::Unreachable(format!(
                "refusing off-base url {url:?}"
            )));
        }
        let token = self.installation_token().await?;
        let mut request = self
            .http
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Unreachable(error.to_string()))?;
        if response.status().as_u16() == 304 {
            return Ok(Conditional::NotModified);
        }
        let response = classify(response).await?;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let next = next_page_url(&response);
        let mut value: serde_json::Value = response
            .json()
            .await
            .map_err(|error| ApiError::Unreachable(format!("response body: {error}")))?;
        if let Some(next) = next {
            value = serde_json::json!({ "__page": value, "__next": next });
        }
        Ok(Conditional::Fresh { value, etag })
    }

    /// One PR's detail — the scanner's terminal-state read for a PR
    /// that left the open set (`merged`, timestamps, merge sha).
    pub(crate) async fn get_pull(&self, repo: &str, number: u64) -> Result<PullDetail, ApiError> {
        let url = format!("{}/repos/{repo}/pulls/{number}", self.api_base);
        match self.get_value(&url, None).await? {
            Conditional::NotModified => Err(ApiError::Unreachable(
                "unconditional read answered 304".to_string(),
            )),
            Conditional::Fresh { value, .. } => serde_json::from_value(value)
                .map_err(|error| ApiError::Unreachable(format!("pull detail shape: {error}"))),
        }
    }

    /// Check-runs rollup for a head sha (tier-2, expand-time only).
    pub(crate) async fn check_runs_summary(
        &self,
        repo: &str,
        sha: &str,
    ) -> Result<ChecksSummary, ApiError> {
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/check-runs?per_page=100",
            self.api_base
        );
        let value = match self.get_value(&url, None).await? {
            Conditional::NotModified => {
                return Err(ApiError::Unreachable(
                    "unconditional read answered 304".to_string(),
                ))
            }
            Conditional::Fresh { value, .. } => value,
        };
        let runs = value
            .get("check_runs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut summary = ChecksSummary {
            total: value
                .get("total_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(runs.len()),
            ..Default::default()
        };
        for run in &runs {
            let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "completed" {
                summary.completed += 1;
                match run.get("conclusion").and_then(|v| v.as_str()) {
                    Some("success") | Some("neutral") | Some("skipped") => {
                        summary.succeeded += 1;
                    }
                    Some(_) => summary.failed += 1,
                    None => {}
                }
            }
        }
        Ok(summary)
    }

    /// Latest-review-per-reviewer rollup (tier-2, expand-time only).
    pub(crate) async fn reviews_summary(
        &self,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary, ApiError> {
        let url = format!(
            "{}/repos/{repo}/pulls/{number}/reviews?per_page=100",
            self.api_base
        );
        let value = match self.get_value(&url, None).await? {
            Conditional::NotModified => {
                return Err(ApiError::Unreachable(
                    "unconditional read answered 304".to_string(),
                ))
            }
            Conditional::Fresh { value, .. } => value,
        };
        let reviews = value.as_array().cloned().unwrap_or_default();
        // Latest state per reviewer wins; comment-only reviews never
        // override an approval or a change request.
        let mut latest: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for review in &reviews {
            let login = review
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let state = review
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if login.is_empty() {
                continue;
            }
            match state.as_str() {
                "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED" => {
                    latest.insert(login, state);
                }
                "COMMENTED" => {
                    latest.entry(login).or_insert(state);
                }
                _ => {}
            }
        }
        let mut summary = ReviewSummary::default();
        for state in latest.values() {
            match state.as_str() {
                "APPROVED" => summary.approved += 1,
                "CHANGES_REQUESTED" => summary.changes_requested += 1,
                "COMMENTED" => summary.commented += 1,
                _ => {}
            }
        }
        Ok(summary)
    }

    /// Every open PR of `owner/repo` (paginated, bounded), conditional
    /// on the first page's ETag — the poll loop's one recurring read.
    pub(crate) async fn list_open_pulls(
        &self,
        repo: &str,
        etag: Option<&str>,
    ) -> Result<Conditional<Vec<PrSummary>>, ApiError> {
        let first = format!(
            "{}/repos/{repo}/pulls?state=open&per_page=100",
            self.api_base
        );
        let mut url = first;
        let mut first_etag: Option<String> = None;
        let mut pulls: Vec<PrSummary> = Vec::new();
        for page in 0..MAX_LIST_PAGES {
            let page_etag = if page == 0 { etag } else { None };
            let (value, served_etag) = match self.get_value(&url, page_etag).await? {
                Conditional::NotModified => return Ok(Conditional::NotModified),
                Conditional::Fresh { value, etag } => (value, etag),
            };
            if page == 0 {
                first_etag = served_etag;
            }
            let (page_value, next) = match value {
                serde_json::Value::Object(mut wrapped) if wrapped.contains_key("__page") => {
                    let next = wrapped
                        .remove("__next")
                        .and_then(|v| v.as_str().map(str::to_string));
                    (
                        wrapped.remove("__page").unwrap_or(serde_json::Value::Null),
                        next,
                    )
                }
                other => (other, None),
            };
            let mut parsed: Vec<PrSummary> = serde_json::from_value(page_value)
                .map_err(|error| ApiError::Unreachable(format!("pull list shape: {error}")))?;
            pulls.append(&mut parsed);
            match next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(Conditional::Fresh {
            value: pulls,
            etag: first_etag,
        })
    }
}

/// The `Link: <url>; rel="next"` pagination pointer, if any.
fn next_page_url(response: &reqwest::Response) -> Option<String> {
    let link = response.headers().get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if !part.ends_with("rel=\"next\"") {
            continue;
        }
        let url = part.split(';').next()?.trim();
        return Some(url.strip_prefix('<')?.strip_suffix('>')?.to_string());
    }
    None
}

/// What the manifest ceremony keeps from GitHub's conversion response.
/// Deliberately three fields: `client_secret` and `webhook_secret` are
/// in the response but **never materialize as retained values** — the
/// narrow parse is the discard (serde ignores the rest of the body).
#[derive(serde::Deserialize)]
pub(crate) struct ManifestConversion {
    pub(crate) id: u64,
    pub(crate) slug: String,
    pub(crate) pem: String,
}

/// Exchange a manifest-flow `code` for the created App's identity:
/// `POST {api_base}/app-manifests/{code}/conversions`. Credential-less
/// by design — the single-use code GitHub minted at the owner's Create
/// click is the entire authorization, so this is a free function, not a
/// `GithubAppClient` method (no key exists until this call returns).
/// GitHub answers 201 with the full App object; 404 for an
/// invalid/expired/used code lands in the `Denied` class via
/// [`classify`].
pub(crate) async fn convert_manifest_code(
    api_base: &str,
    code: &str,
) -> Result<ManifestConversion, ApiError> {
    let http = reqwest::Client::builder()
        .user_agent("intendant")
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| ApiError::Unreachable(format!("http client: {error}")))?;
    let url = format!(
        "{}/app-manifests/{}/conversions",
        api_base.trim_end_matches('/'),
        code
    );
    let response = http
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .send()
        .await
        // `without_url`: reqwest's Display embeds the request URL, and
        // this one carries the still-live single-use code — it must
        // never reach an error page or a log line.
        .map_err(|error| ApiError::Unreachable(error.without_url().to_string()))?;
    let response = classify(response).await?;
    response
        .json::<ManifestConversion>()
        .await
        .map_err(|error| {
            ApiError::Unreachable(format!("conversion response: {}", error.without_url()))
        })
}

/// Map a non-2xx response onto the named failure classes. 304 never
/// reaches here (handled at the call site).
async fn classify(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after_s = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let remaining_zero = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "0");
    let code = status.as_u16();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {code}"));
    match code {
        401 | 404 => Err(ApiError::Denied(message)),
        403 | 429 if retry_after_s.is_some() || remaining_zero => Err(ApiError::RateLimited {
            retry_after_s,
            message,
        }),
        403 => Err(ApiError::Denied(message)),
        _ => Err(ApiError::Unreachable(message)),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn test_rsa_pem() -> &'static str {
    TEST_RSA_PKCS1_PEM
}

/// Test-only RSA key, generated for this test suite and used nowhere
/// real — it authenticates nothing and guards nothing. Committed so the
/// JWT tests are hermetic (ring cannot generate RSA keys).
#[cfg(test)]
const TEST_RSA_PKCS1_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEogIBAAKCAQEAnKjOHqX1w5xoh4AcFEeZjCMbmUBKvI1LtsXCYXTxVh1Nnaj7
MN6BGFo7a00wks2rS+mw2Lj9Nj0yyrDsR8TuOvfG3PtpcPZgzERcplAwZ8CsRptb
bGJFsoLLHH5gsQLLVqonktw4i3EFld1amAn9w16WBkHBPz+ZRUYFyvsnemBImpVR
WfH/R/9W4DWdNDgGtCCtdgdZgMcEtw/9eFqENH4NtCXMKQUa/bEcV3vTwZ5BchjT
pxcfWeqYDDGdVs8dnA7NJ10ZyswF4YtBOgxKGwBRM1Fr3oLvs4M0v96uUaqxHYnR
nk52g9DIorJv5TXr9CRVO74ZBFASGV2luSTGIQIDAQABAoIBAENioPax8FrpxlSy
mGFowvVyjIaJDxy3sl+2BPyirsSZ6i7s5s+IhgMRnQl0tHYRHaOTq8wtFk3kWRqy
q4/bd5XJyrJ3Ok2qzMxQg4HOcGPQjsn4PYULaGt2syNYgQLi7tAidM9JBtGEFiD2
i+dmNM96uhGK6zLnimgvzIIZMkwCviULoUozPq/MrIWIEe2xQzUaWjLQ0pGE+uWk
I5wagGkUE/kWKcX4H9HjLAa97WbgRptGrymUuBW1+dU5yw91e18uSkcAPYeBBKh8
uK9jA3ckwLhhT5yjspe6KQtP1LGHJxQCXtwFREn9GX8nFwDd+AKPAoz1KnW7WeKo
UsTeJlkCgYEAzLN5asdV6b4dsSmU7mCl0yIkEseM/qVpcGDjjijHm2qUsumTvoVB
NCsYO0T5YH3F2K20aM9+i0XSegp5zCSLUi+f0MmrjY7rLxPb1Jvd6/o62/OXNQ0I
tV+6iJIuaxRQPC1N2r8S2DIbCKF3z0EFIJ92LV0v3APJn6AjCJnlskMCgYEAw+s7
4bmQN0sLi5NjGVCueOShRYHTSLD6ezGxpSkM3Zq9OnzjbnZuwgZ/G4ByieNeho7D
0iE1pocUixkGHOwnG8lWb2CO+tPWs/REmaRErn7Z1mhpOk5QerNGehv9hpeICgFi
dfzRgq7DCA2S/Is5/qKNPDrS0zc2zub7nQU6ucsCgYBX8mAfFUd/JnRhUmkvRYzZ
OljfTKbyHSVA6A+8Wx7vUgpTF/GnMF9ER6Ogi1DNORxQrMjPIx7OPZBhaLDNmYHW
LKnwLUUsi5PV5SVUoiblpNu29mAnpdLxAhEFbjDNRqv2PsytR9yT0Gs2+RCdleTb
EEfY06mlUGdG0qlan6xFOwKBgB+2Q8sVrjJFA2lkQfYnCRaoazJFAV4Sx3iJYqfJ
LTvxgA+nh2ip4uOlCY36DJAlLXe6RBgPKA/8bWbWdhbYYrwsqsD8cChJgcc/EpuL
61ITVk9ONzoo0v4JZq79ONxAStTTxIw0j/UHNKppCBG4t3pv9Ux6eQWXOlfjK3cP
EaJhAoGAdL6rY0ve46dpLStD9VD+YaKW3Dkqk2j2KmTcZa7aBJyBRn452mnmk9tv
s1GgtAyZQxxNz39Yw9NvrTMVccNrxv25qmOnN4ZbAkNZVleO89XwwZqlQkdIXkUa
+ospl9SVeFgc5EenkyN1yD+vVqnwqcEM6luggj8bfQEvDpu6FGU=
-----END RSA PRIVATE KEY-----
";

/// The same throwaway key re-wrapped as PKCS#8, for the second parse
/// path. Test-only, guards nothing.
#[cfg(test)]
const TEST_RSA_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCcqM4epfXDnGiH
gBwUR5mMIxuZQEq8jUu2xcJhdPFWHU2dqPsw3oEYWjtrTTCSzatL6bDYuP02PTLK
sOxHxO4698bc+2lw9mDMRFymUDBnwKxGm1tsYkWygsscfmCxAstWqieS3DiLcQWV
3VqYCf3DXpYGQcE/P5lFRgXK+yd6YEialVFZ8f9H/1bgNZ00OAa0IK12B1mAxwS3
D/14WoQ0fg20JcwpBRr9sRxXe9PBnkFyGNOnFx9Z6pgMMZ1Wzx2cDs0nXRnKzAXh
i0E6DEobAFEzUWvegu+zgzS/3q5RqrEdidGeTnaD0Miism/lNev0JFU7vhkEUBIZ
XaW5JMYhAgMBAAECggEAQ2Kg9rHwWunGVLKYYWjC9XKMhokPHLeyX7YE/KKuxJnq
Luzmz4iGAxGdCXS0dhEdo5OrzC0WTeRZGrKrj9t3lcnKsnc6TarMzFCDgc5wY9CO
yfg9hQtoa3azI1iBAuLu0CJ0z0kG0YQWIPaL52Y0z3q6EYrrMueKaC/MghkyTAK+
JQuhSjM+r8yshYgR7bFDNRpaMtDSkYT65aQjnBqAaRQT+RYpxfgf0eMsBr3tZuBG
m0avKZS4FbX51TnLD3V7Xy5KRwA9h4EEqHy4r2MDdyTAuGFPnKOyl7opC0/UsYcn
FAJe3AVESf0ZfycXAN34Ao8CjPUqdbtZ4qhSxN4mWQKBgQDMs3lqx1Xpvh2xKZTu
YKXTIiQSx4z+pWlwYOOOKMebapSy6ZO+hUE0Kxg7RPlgfcXYrbRoz36LRdJ6CnnM
JItSL5/QyauNjusvE9vUm93r+jrb85c1DQi1X7qIki5rFFA8LU3avxLYMhsIoXfP
QQUgn3YtXS/cA8mfoCMImeWyQwKBgQDD6zvhuZA3SwuLk2MZUK545KFFgdNIsPp7
MbGlKQzdmr06fONudm7CBn8bgHKJ416GjsPSITWmhxSLGQYc7CcbyVZvYI7609az
9ESZpESuftnWaGk6TlB6s0Z6G/2Gl4gKAWJ1/NGCrsMIDZL8izn+oo08OtLTNzbO
5vudBTq5ywKBgFfyYB8VR38mdGFSaS9FjNk6WN9MpvIdJUDoD7xbHu9SClMX8acw
X0RHo6CLUM05HFCsyM8jHs49kGFosM2ZgdYsqfAtRSyLk9XlJVSiJuWk27b2YCel
0vECEQVuMM1Gq/Y+zK1H3JPQazb5EJ2V5NsQR9jTqaVQZ0bSqVqfrEU7AoGAH7ZD
yxWuMkUDaWRB9icJFqhrMkUBXhLHeIlip8ktO/GAD6eHaKni46UJjfoMkCUtd7pE
GA8oD/xtZtZ2FthivCyqwPxwKEmBxz8Sm4vrUhNWT043OijS/glmrv043EBK1NPE
jDSP9Qc0qmkIEbi3em/1THp5BZc6V+Mrdw8RomECgYB0vqtjS97jp2ktK0P1UP5h
opbcOSqTaPYqZNxlrtoEnIFGfjnaaeaT22+zUaC0DJlDHE3Pf1jD02+tMxVxw2vG
/bmqY6c3hlsCQ1lWV47z1fDBmqVCR0heRRr6iymX1JV4WBzkR6eTI3XIP69WqfCp
wQzqW6CCPxt9AS8Om7oUZQ==
-----END PRIVATE KEY-----
";

/// Shared test support: a minimal HTTP/1.1 fixture standing in for
/// api.github.com (never live GitHub in tests), with mutable routes so
/// scenario tests can change the served world between polls, plus the
/// throwaway credentials. Used by this module's tests and the
/// scanner's.
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) type FixtureResponse = (u16, Vec<(String, String)>, String);

    pub(crate) fn test_credentials() -> GithubAppCredentials {
        GithubAppCredentials {
            v: 1,
            app_id: "123456".to_string(),
            installation_id: Some(987),
            slug: None,
            private_key_pem: TEST_RSA_PKCS1_PEM.to_string(),
        }
    }

    pub(crate) struct Fixture {
        pub(crate) base: String,
        pub(crate) hits: Arc<AtomicUsize>,
        pub(crate) token_hits: Arc<AtomicUsize>,
        routes: Arc<Mutex<HashMap<(String, String), FixtureResponse>>>,
    }

    impl Fixture {
        /// Replace one route's canned response (the world changed —
        /// a PR merged, a page appeared). Takes effect on the next
        /// request.
        pub(crate) fn set_route(&self, method: &str, path: &str, response: FixtureResponse) {
            self.routes
                .lock()
                .unwrap()
                .insert((method.to_string(), path.to_string()), response);
        }

        pub(crate) fn remove_route(&self, method: &str, path: &str) {
            self.routes
                .lock()
                .unwrap()
                .remove(&(method.to_string(), path.to_string()));
        }
    }

    pub(crate) fn token_route() -> ((String, String), FixtureResponse) {
        (
            (
                "POST".to_string(),
                "/app/installations/987/access_tokens".to_string(),
            ),
            (
                201,
                Vec::new(),
                r#"{"token":"ghs_fixture","expires_at":"2099-01-01T00:00:00Z"}"#.to_string(),
            ),
        )
    }

    pub(crate) fn pull(number: u64, title: &str, draft: bool) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "draft": draft,
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "user": {"login": "octocat"},
            "head": {"ref": "feature"},
            "base": {"ref": "main"},
            "updated_at": "2026-07-24T00:00:00Z",
            "state": "open",
        })
    }

    pub(crate) async fn spawn_fixture(
        initial: HashMap<(String, String), FixtureResponse>,
    ) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let token_hits = Arc::new(AtomicUsize::new(0));
        let routes = Arc::new(Mutex::new(initial));
        let fixture_base = base.clone();
        let (hits_task, token_task, routes_task) =
            (hits.clone(), token_hits.clone(), routes.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes_task.clone();
                let hits = hits_task.clone();
                let token_hits = token_task.clone();
                let base = fixture_base.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let mut lines = text.lines();
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    let path = target.split('?').next().unwrap_or_default().to_string();
                    let headers: HashMap<String, String> = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        .collect();
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if path.ends_with("/access_tokens") {
                        token_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let auth = headers.get("authorization").cloned().unwrap_or_default();
                        assert!(
                            auth.starts_with("Bearer ey"),
                            "token mint must carry the App JWT, got {auth:?}"
                        );
                    }
                    let etag_match = headers.get("if-none-match").cloned();
                    let looked_up = routes.lock().unwrap().get(&(method, path)).cloned();
                    let (status, extra, body) = match looked_up {
                        Some((status, extra, body)) => {
                            let served_etag = extra
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
                                .map(|(_, v)| v.clone());
                            if served_etag.is_some() && served_etag == etag_match {
                                (304u16, extra.clone(), String::new())
                            } else {
                                (status, extra.clone(), body.clone())
                            }
                        }
                        None => (404, Vec::new(), r#"{"message":"Not Found"}"#.to_string()),
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in &extra {
                        head.push_str(&format!("{name}: {}\r\n", value.replace("__BASE__", &base)));
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Fixture {
            base,
            hits,
            token_hits,
            routes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixture::*;
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    fn b64url_decode(part: &str) -> Vec<u8> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .expect("valid base64url")
    }

    #[test]
    fn both_pem_encodings_parse() {
        rsa_key_from_pem(TEST_RSA_PKCS1_PEM).expect("PKCS#1 parses");
        rsa_key_from_pem(TEST_RSA_PKCS8_PEM).expect("PKCS#8 parses");
        let error =
            rsa_key_from_pem("-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n")
                .unwrap_err();
        assert!(error.contains("unsupported PEM block"), "{error}");
    }

    #[test]
    fn jwt_is_rs256_signed_clock_safe_and_verifiable() {
        let credentials = test_credentials();
        let now = 1_784_900_000u64;
        let jwt = mint_app_jwt(&credentials, now).expect("mint");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[0])).expect("header json");
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        let claims: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[1])).expect("claims json");
        assert_eq!(claims["iss"], "123456");
        assert_eq!(claims["iat"].as_u64().unwrap(), now - JWT_BACKDATE_S);
        assert_eq!(claims["exp"].as_u64().unwrap(), now + JWT_LIFETIME_S);
        assert!(claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap() < 600);
        let key = rsa_key_from_pem(&credentials.private_key_pem).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            key.public().as_ref().to_vec(),
        );
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        public
            .verify(signing_input.as_bytes(), &b64url_decode(parts[2]))
            .expect("signature verifies against the key's public half");
    }

    #[tokio::test]
    async fn mints_installation_token_and_lists_open_pulls() {
        let mut routes = HashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            (
                200,
                vec![("etag".to_string(), "\"tag-1\"".to_string())],
                serde_json::json!([pull(1, "first", false), pull(2, "second", true)]).to_string(),
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();

        let listed = client
            .list_open_pulls("o/r", None)
            .await
            .expect("fresh list");
        let (pulls, etag) = match listed {
            Conditional::Fresh { value, etag } => (value, etag),
            Conditional::NotModified => panic!("first read cannot be 304"),
        };
        assert_eq!(pulls.len(), 2);
        assert_eq!(pulls[0].number, 1);
        assert_eq!(pulls[1].title, "second");
        assert!(pulls[1].draft);
        assert_eq!(pulls[0].head.branch, "feature");
        assert_eq!(pulls[0].user.login, "octocat");
        assert_eq!(etag.as_deref(), Some("\"tag-1\""));

        // Second read with the validator: 304, and the cached token is
        // reused (exactly one mint across both reads).
        match client
            .list_open_pulls("o/r", etag.as_deref())
            .await
            .unwrap()
        {
            Conditional::NotModified => {}
            Conditional::Fresh { .. } => panic!("expected 304 NotModified"),
        }
        assert_eq!(fixture.token_hits.load(Ordering::SeqCst), 1);
        assert!(fixture.hits.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn follows_pagination_within_the_bound() {
        let mut routes = HashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            (
                200,
                vec![
                    ("etag".to_string(), "\"page-1\"".to_string()),
                    (
                        "link".to_string(),
                        "<__BASE__/repos/o/r/pulls-page2>; rel=\"next\"".to_string(),
                    ),
                ],
                serde_json::json!([pull(1, "first", false)]).to_string(),
            ),
        );
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls-page2".to_string()),
            (
                200,
                Vec::new(),
                serde_json::json!([pull(2, "second", false)]).to_string(),
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        let listed = client.list_open_pulls("o/r", None).await.expect("list");
        let (pulls, etag) = match listed {
            Conditional::Fresh { value, etag } => (value, etag),
            Conditional::NotModified => panic!("unexpected 304"),
        };
        assert_eq!(pulls.len(), 2);
        assert_eq!(etag.as_deref(), Some("\"page-1\""), "etag is page 1's");
    }

    #[tokio::test]
    async fn denied_rate_limited_and_unreachable_classify_by_name() {
        let mut routes = HashMap::new();
        routes.insert(
            (
                "POST".to_string(),
                "/app/installations/987/access_tokens".to_string(),
            ),
            (
                401,
                Vec::new(),
                r#"{"message":"A JSON web token could not be decoded"}"#.to_string(),
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        match client.verify().await {
            Err(ApiError::Denied(message)) => assert!(message.contains("could not be decoded")),
            other => panic!("expected Denied, got {other:?}"),
        }

        let mut routes = HashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            (
                403,
                vec![
                    ("retry-after".to_string(), "30".to_string()),
                    ("x-ratelimit-remaining".to_string(), "0".to_string()),
                ],
                r#"{"message":"API rate limit exceeded"}"#.to_string(),
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        match client.list_open_pulls("o/r", None).await {
            Err(ApiError::RateLimited { retry_after_s, .. }) => {
                assert_eq!(retry_after_s, Some(30));
            }
            other => panic!("expected RateLimited, got {:?}", other.err()),
        }

        // A dead port classifies as unreachable, never a panic or hang.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = GithubAppClient::new(&dead_base, test_credentials()).unwrap();
        match client.verify().await {
            Err(ApiError::Unreachable(_)) => {}
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }
}

impl std::fmt::Debug for GithubAppClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubAppClient")
            .field("api_base", &self.api_base)
            .field("app_id", &self.credentials.app_id)
            .field("installation_id", &self.credentials.installation_id)
            .finish_non_exhaustive()
    }
}
