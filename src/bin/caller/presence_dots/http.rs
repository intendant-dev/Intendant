//! A single account-bound profile GET. An edge challenge is not a dots
//! eligibility decision. No body, cookies, attestation, or challenge solver.

use super::routing;
use crate::codex_cloud::CodexAuth;
use reqwest::{
    header::{HeaderMap, ACCEPT, CONTENT_TYPE},
    redirect::Policy,
    StatusCode,
};
use serde::Serialize;
use std::time::Duration;

const PROFILE_URL: &str = "https://chatgpt.com/backend-api/tbo/primary";
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Serialize)]
pub(super) struct HttpReport {
    state: &'static str,
    account_routing_verified: bool,
    http_status: Option<u16>,
    eligibility_validated: bool,
    stable_dot_identity_validated: bool,
}

impl HttpReport {
    fn new(state: &'static str, routed: bool, status: Option<u16>) -> Self {
        Self {
            state,
            account_routing_verified: routed,
            http_status: status,
            eligibility_validated: false,
            stable_dot_identity_validated: false,
        }
    }

    pub(super) fn state_label(&self) -> &'static str {
        self.state
    }
}

pub(super) async fn diagnose(auth: &CodexAuth) -> HttpReport {
    if let Err(failure) = routing::discover(auth).await {
        return HttpReport::new(failure.label(), false, None);
    }
    let headers = match auth.diagnostic_http_headers() {
        Ok(headers) => headers,
        Err(_) => return HttpReport::new("subscription_credential_invalid", true, None),
    };
    // Honest identity, no cookie jar, no arbitrary origin or redirect.
    let client = match reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(Policy::none())
        .user_agent(format!(
            "intendant-dots-diagnostic/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
    {
        Ok(client) => client,
        Err(_) => return HttpReport::new("http_client_unavailable", true, None),
    };
    probe(&client, PROFILE_URL, headers).await
}

// Only production's pinned URL is used. The private URL seam is for
// hermetic loopback tests and is never exposed in configuration or argv.
async fn probe(client: &reqwest::Client, url: &str, headers: HeaderMap) -> HttpReport {
    match client
        .get(url)
        .headers(headers)
        .header(ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(response) => {
            // Do not download or parse a challenge, profile, error body or
            // account identifiers. Headers alone suffice for this diagnosis.
            classify(response.status(), response.headers())
        }
        Err(_) => HttpReport::new("http_transport_failed", true, None),
    }
}

fn classify(status: StatusCode, headers: &HeaderMap) -> HttpReport {
    let state = if headers.get("cf-mitigated").and_then(|v| v.to_str().ok()) == Some("challenge") {
        "provider_edge_challenge"
    } else if status.is_redirection() {
        "redirect_refused"
    } else if status == StatusCode::UNAUTHORIZED {
        "unauthorized"
    } else if status == StatusCode::FORBIDDEN {
        "forbidden_unknown"
    } else if !status.is_success() {
        "http_refused"
    } else if headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        "profile_response_unvalidated"
    } else {
        "unexpected_media_type"
    };
    HttpReport::new(state, true, Some(status.as_u16()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn http_gate_does_not_confuse_challenge_access_and_feature_support() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/html".parse().unwrap());
        headers.insert("cf-mitigated", "challenge".parse().unwrap());
        assert_eq!(
            classify(StatusCode::FORBIDDEN, &headers).state,
            "provider_edge_challenge"
        );
        headers.remove("cf-mitigated");
        assert_eq!(
            classify(StatusCode::FORBIDDEN, &headers).state,
            "forbidden_unknown"
        );
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, &headers).state,
            "unauthorized"
        );
        assert_eq!(
            classify(StatusCode::OK, &headers).state,
            "unexpected_media_type"
        );
        headers.insert(
            CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        let report = classify(StatusCode::OK, &headers);
        assert_eq!(report.state, "profile_response_unvalidated");
        assert!(!report.eligibility_validated);
        assert!(!report.stable_dot_identity_validated);
        let failure = HttpReport::new(routing::Failure::AccountMismatch.label(), false, None);
        assert!(!failure.account_routing_verified);
        assert!(failure.http_status.is_none());
    }

    #[tokio::test]
    async fn http_probe_is_read_only_redacted_and_does_not_follow_redirects() {
        for response in [
            "HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\nContent-Type: text/html\r\nContent-Length: 15\r\n\r\nprovider-secret",
            "HTTP/1.1 302 Found\r\nLocation: https://other.invalid/secret\r\nContent-Length: 15\r\n\r\nprovider-secret",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/profile", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let byte = socket.read_u8().await.unwrap();
                    request.push(byte);
                    assert!(request.len() < 8192);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("get /profile http/1.1\r\n"));
                assert!(request.contains("authorization: bearer fixture-token\r\n"));
                assert!(request.contains("chatgpt-account-id: fixture-account\r\n"));
                assert!(request.contains("originator: intendant\r\n"));
                assert!(!request.contains("cookie:"));
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let auth = crate::codex_cloud::fixture_subscription_auth("fixture-token", "fixture-account");
            let client = reqwest::Client::builder().no_proxy().redirect(Policy::none()).timeout(HTTP_TIMEOUT).build().unwrap();
            let report = probe(&client, &url, auth.diagnostic_http_headers().unwrap()).await;
            assert_eq!(report.state, if response.contains("403") { "provider_edge_challenge" } else { "redirect_refused" });
            let encoded = serde_json::to_string(&report).unwrap();
            for secret in ["fixture-token", "fixture-account", "provider-secret", "other.invalid"] {
                assert!(!encoded.contains(secret));
                assert!(!format!("{report:?}").contains(secret));
            }
            assert!(!report.eligibility_validated);
            assert!(!report.stable_dot_identity_validated);
            server.await.unwrap();
        }
    }
}
