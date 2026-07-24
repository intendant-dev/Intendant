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
#[derive(Debug, Clone)]
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
        let jwt = mint_app_jwt(&self.credentials, now).map_err(ApiError::Denied)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.api_base, self.credentials.installation_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_credentials() -> GithubAppCredentials {
        GithubAppCredentials {
            v: 1,
            app_id: "123456".to_string(),
            installation_id: 987,
            private_key_pem: TEST_RSA_PKCS1_PEM.to_string(),
        }
    }

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

    /// One canned response per (method, path) — a minimal HTTP/1.1
    /// fixture standing in for api.github.com. Never live GitHub in
    /// tests.
    struct Fixture {
        base: String,
        hits: Arc<AtomicUsize>,
        token_hits: Arc<AtomicUsize>,
    }

    async fn spawn_fixture(
        routes: HashMap<(String, String), (u16, Vec<(String, String)>, String)>,
    ) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let token_hits = Arc::new(AtomicUsize::new(0));
        let routes = Arc::new(routes);
        let fixture_base = base.clone();
        let (hits_task, token_task) = (hits.clone(), token_hits.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
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
                    hits.fetch_add(1, Ordering::SeqCst);
                    if path.ends_with("/access_tokens") {
                        token_hits.fetch_add(1, Ordering::SeqCst);
                        let auth = headers.get("authorization").cloned().unwrap_or_default();
                        assert!(
                            auth.starts_with("Bearer ey"),
                            "token mint must carry the App JWT, got {auth:?}"
                        );
                    }
                    let etag_match = headers.get("if-none-match").cloned();
                    let (status, extra, body) = match routes.get(&(method, path)) {
                        Some((status, extra, body)) => {
                            let served_etag = extra
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
                                .map(|(_, v)| v.clone());
                            if served_etag.is_some() && served_etag == etag_match {
                                (304u16, extra.clone(), String::new())
                            } else {
                                (*status, extra.clone(), body.clone())
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
        }
    }

    fn token_route() -> ((String, String), (u16, Vec<(String, String)>, String)) {
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

    fn pull(number: u64, title: &str, draft: bool) -> serde_json::Value {
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
