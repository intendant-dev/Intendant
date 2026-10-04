//! Account-bound, pinned profile GETs. The default probe reads headers only;
//! explicit identity inspection downloads bounded JSON, never challenge or
//! error bodies. No cookies, attestation, redirect, or challenge solver.

use super::{profile, routing};
use crate::codex_cloud::CodexAuth;
use reqwest::{
    header::{HeaderMap, ACCEPT, CONTENT_TYPE},
    redirect::Policy,
    StatusCode,
};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const PROFILE_URL: &str = "https://chatgpt.com/backend-api/tbo/primary";
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PROFILE_BYTES: usize = 256 * 1024;

// Closed GET-only routes, never arbitrary paths or caller-selected origins.
pub(super) enum ProfileRoute<'a> {
    Primary,
    ByThread(&'a str),
    RootThread(&'a str),
    List(Option<&'a str>),
}

// No Debug: successful bodies contain private provider identities. Every
// failed POST is outcome-unknown, even a rejection, until read reconciliation.
pub(super) enum CreationOutcome {
    Received(Value, u16),
    Unknown {
        state: &'static str,
        status: Option<u16>,
    },
}

// A receipt is private, bounded provider data, never a diagnostic or IAM grant.
pub(super) enum ProbeOutcome {
    Received(Value, u16),
    Unknown {
        state: &'static str,
        status: Option<u16>,
    },
}

pub(super) struct ProfileClient {
    client: reqwest::Client,
    base: String,
    headers: HeaderMap,
}

#[derive(Debug, Serialize)]
pub(super) struct HttpReport {
    state: &'static str,
    account_routing_verified: bool,
    http_status: Option<u16>,
    eligibility_validated: bool,
    stable_dot_identity_validated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_rotation_observed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messaging_room_linked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_observation: Option<profile::SchemaObservation>,
}

impl HttpReport {
    pub(super) fn new(state: &'static str, routed: bool, status: Option<u16>) -> Self {
        Self {
            state,
            account_routing_verified: routed,
            http_status: status,
            eligibility_validated: false,
            stable_dot_identity_validated: false,
            root_rotation_observed: None,
            messaging_room_linked: None,
            schema_observation: None,
        }
    }

    pub(super) fn state_label(&self) -> &'static str {
        self.state
    }

    pub(super) fn identity_validated(&self) -> bool {
        self.stable_dot_identity_validated
    }

    pub(super) fn with_schema(mut self, observation: profile::SchemaObservation) -> Self {
        self.schema_observation = Some(observation);
        self
    }

    pub(super) fn identity_verified(rotated: bool, room_linked: bool) -> Self {
        Self {
            stable_dot_identity_validated: true,
            root_rotation_observed: Some(rotated),
            messaging_room_linked: Some(room_linked),
            ..Self::new("profile_identity_verified", true, Some(200))
        }
    }
}

pub(super) async fn diagnose(auth: &CodexAuth, read_profile: bool) -> HttpReport {
    tokio::time::timeout(IDENTITY_TIMEOUT, diagnose_inner(auth, read_profile))
        .await
        .unwrap_or_else(|_| HttpReport::new("profile_diagnostic_timed_out", false, None))
}

async fn diagnose_inner(auth: &CodexAuth, read_profile: bool) -> HttpReport {
    if let Err(failure) = routing::discover(auth).await {
        return HttpReport::new(failure.label(), false, None);
    }
    let headers = match auth.diagnostic_http_headers() {
        Ok(headers) => headers,
        Err(_) => return HttpReport::new("subscription_credential_invalid", true, None),
    };
    // Honest identity, no cookie jar, no arbitrary origin or redirect.
    let client = match client_builder().build() {
        Ok(client) => client,
        Err(_) => return HttpReport::new("http_client_unavailable", true, None),
    };
    if !read_profile {
        return probe(&client, PROFILE_URL, headers).await;
    }
    let profile_client = ProfileClient {
        client,
        base: "https://chatgpt.com/backend-api/tbo".into(),
        headers,
    };
    profile::diagnose(&profile_client, |id| async move {
        super::verify_current_root(auth, &id).await
    })
    .await
}

impl ProfileClient {
    pub(super) async fn account_bound(auth: &CodexAuth) -> Result<Self, HttpReport> {
        routing::discover(auth)
            .await
            .map_err(|failure| HttpReport::new(failure.label(), false, None))?;
        let headers = auth
            .diagnostic_http_headers()
            .map_err(|_| HttpReport::new("subscription_credential_invalid", true, None))?;
        let client = client_builder()
            .build()
            .map_err(|_| HttpReport::new("http_client_unavailable", true, None))?;
        Ok(Self {
            client,
            base: "https://chatgpt.com/backend-api/tbo".into(),
            headers,
        })
    }

    pub(super) async fn read(&self, route: ProfileRoute<'_>) -> Result<Value, HttpReport> {
        if let ProfileRoute::List(cursor) = route {
            if cursor
                .is_some_and(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
            {
                return Err(HttpReport::new("profile_cursor_invalid", true, None));
            }
            let mut url = reqwest::Url::parse(&self.base)
                .map_err(|_| HttpReport::new("profile_route_identity_invalid", true, None))?;
            url.query_pairs_mut().append_pair("limit", "25");
            if let Some(cursor) = cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            return read_json(&self.client, url.as_str(), self.headers.clone()).await;
        }
        let mut url = reqwest::Url::parse(&self.base)
            .map_err(|_| HttpReport::new("profile_route_identity_invalid", true, None))?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| HttpReport::new("profile_route_identity_invalid", true, None))?;
        match route {
            ProfileRoute::Primary => {
                segments.push("primary");
            }
            ProfileRoute::ByThread(id) => {
                if !profile::valid_thread_id(id) {
                    return Err(HttpReport::new(
                        "profile_route_identity_invalid",
                        true,
                        None,
                    ));
                }
                segments.push("by-thread").push(id);
            }
            ProfileRoute::RootThread(id) => {
                if !profile::valid_resource_id(id) {
                    return Err(HttpReport::new(
                        "profile_route_identity_invalid",
                        true,
                        None,
                    ));
                }
                // Url's segment API escapes slash, query, fragment and percent
                // characters. Provider ids cannot replace the pinned origin,
                // base route, or this fixed suffix, even when not plain atoms.
                segments.push(id).push("root-thread");
            }
            ProfileRoute::List(_) => unreachable!("handled above"),
        }
        drop(segments);
        read_json(&self.client, url.as_str(), self.headers.clone()).await
    }

    /// Exactly one explicitly additional-dot POST, not primary selection,
    /// onboarding messages, attestation or an alternate transport. The caller
    /// must durably journal intent BEFORE invoking this method.
    pub(super) async fn create_additional(&self, label: &str) -> CreationOutcome {
        let unknown = |state, status| CreationOutcome::Unknown { state, status };
        let response = match self
            .client
            .post(&self.base)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json")
            .timeout(Duration::from_secs(540))
            .json(&serde_json::json!({
                "display_name": label,
                "create_thread": true,
                "create_additional": true,
                "should_initialize": true
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return unknown("creation_transport_outcome_unknown", None),
        };
        let status = response.status();
        let gate = classify(status, response.headers());
        if gate.state != "profile_response_unvalidated" {
            return unknown(gate.state, Some(status.as_u16()));
        }
        if !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return unknown("creation_status_unconfirmed", Some(status.as_u16()));
        }
        match bounded_json(response).await {
            Ok(value) => CreationOutcome::Received(value, status.as_u16()),
            Err(_) => unknown("creation_response_outcome_unknown", Some(status.as_u16())),
        }
    }

    fn message_url(&self, room: &str) -> Result<reqwest::Url, HttpReport> {
        if !profile::valid_segment(room) {
            return Err(HttpReport::new("probe_room_identity_invalid", true, None));
        }
        let mut url = reqwest::Url::parse(&self.base)
            .map_err(|_| HttpReport::new("probe_route_invalid", true, None))?;
        url.path_segments_mut()
            .map_err(|_| HttpReport::new("probe_route_invalid", true, None))?
            .pop()
            .push("messaging")
            .push("rooms")
            .push(room)
            .push("messages");
        Ok(url)
    }

    /// One fixed, clearly labeled probe. No arbitrary text, native proofs,
    /// integrity flags, resume, alternate transport or automatic retry.
    /// Durable intent and fresh primary identity must precede this call.
    pub(super) async fn send_existing_probe(&self, room: &str, attempt: &str) -> ProbeOutcome {
        let unknown = |state, status| ProbeOutcome::Unknown { state, status };
        let url = match self.message_url(room) {
            Ok(url) => url,
            Err(_) => return unknown("probe_room_identity_invalid", None),
        };
        if !profile::valid_thread_id(attempt) {
            return unknown("probe_request_identity_invalid", None);
        }
        let response = match self
            .client
            .post(url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json")
            .timeout(Duration::from_secs(30))
            .json(&super::text_probe::request_body(attempt))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return unknown("probe_transport_outcome_unknown", None),
        };
        let status = response.status();
        let gate = classify(status, response.headers());
        if gate.state != "profile_response_unvalidated" {
            return unknown(gate.state, Some(status.as_u16()));
        }
        if !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return unknown("probe_status_unconfirmed", Some(status.as_u16()));
        }
        match bounded_json(response).await {
            Ok(value) => ProbeOutcome::Received(value, status.as_u16()),
            Err(_) => unknown("probe_receipt_outcome_unknown", Some(status.as_u16())),
        }
    }

    /// Bounded room reads only. Provider text is data and never leaves the
    /// caller's closed receipt/nonce matcher or authorizes a local action.
    pub(super) async fn read_probe_messages(
        &self,
        room: &str,
        around: Option<&str>,
    ) -> Result<Value, HttpReport> {
        if around.is_some_and(|id| !profile::valid_resource_id(id)) {
            return Err(HttpReport::new(
                "probe_message_identity_invalid",
                true,
                None,
            ));
        }
        let mut url = self.message_url(room)?;
        url.query_pairs_mut().append_pair("limit", "20");
        if let Some(around) = around {
            url.query_pairs_mut().append_pair("around", around);
        }
        read_json(&self.client, url.as_str(), self.headers.clone()).await
    }

    #[cfg(test)]
    pub(super) fn fixture(base: String, headers: HeaderMap) -> Self {
        Self {
            client: client_builder().no_proxy().build().unwrap(),
            base,
            headers,
        }
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(format!(
            "intendant-dots-diagnostic/{}",
            env!("CARGO_PKG_VERSION")
        ))
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

async fn read_json(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
) -> Result<Value, HttpReport> {
    let response = client
        .get(url)
        .headers(headers)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| HttpReport::new("http_transport_failed", true, None))?;
    let status = response.status();
    let gate = classify(status, response.headers());
    if gate.state != "profile_response_unvalidated" {
        return Err(gate);
    }
    if status != StatusCode::OK {
        return Err(HttpReport::new(
            "profile_status_unexpected",
            true,
            Some(status.as_u16()),
        ));
    }
    bounded_json(response)
        .await
        .map_err(|state| HttpReport::new(state, true, Some(status.as_u16())))
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value, &'static str> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_PROFILE_BYTES as u64)
    {
        return Err("profile_body_oversized");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "profile_body_failed")? {
        if chunk.len() > MAX_PROFILE_BYTES.saturating_sub(body.len()) {
            return Err("profile_body_oversized");
        }
        body.extend_from_slice(&chunk);
    }
    // serde_json retains its recursion bound. Never include parse errors or
    // raw JSON: provider content can reflect credentials or private text.
    serde_json::from_slice(&body).map_err(|_| "profile_body_invalid_json")
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
                assert!(request.contains("user-agent: intendant-dots-diagnostic/"));
                assert!(!request.contains("cookie:"));
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let auth = crate::codex_cloud::fixture_subscription_auth("fixture-token", "fixture-account");
            let client = client_builder().no_proxy().build().unwrap();
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

    async fn json_fixture(response: Vec<u8>) -> Result<Value, HttpReport> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/profile", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            // An oversized declared body is intentionally never downloaded.
            let _ = socket.write_all(&response).await;
        });
        let client = client_builder().no_proxy().build().unwrap();
        let result = read_json(&client, &url, HeaderMap::new()).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn profile_json_bounds_known_and_chunked_bodies_and_sanitizes_failures() {
        let result = json_fixture(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
        )
        .await
        .unwrap();
        assert!(result.is_object());
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            MAX_PROFILE_BYTES + 1
        )
        .into_bytes();
        let mut chunked = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        chunked.extend_from_slice(format!("{:x}\r\n", MAX_PROFILE_BYTES + 1).as_bytes());
        chunked.extend(vec![b'x'; MAX_PROFILE_BYTES + 1]);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let nested = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        let invalid = "fixture-token private transcript";
        for (response, state) in [
            (oversized, "profile_body_oversized"),
            (chunked, "profile_body_oversized"),
            (format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{nested}", nested.len()).into_bytes(), "profile_body_invalid_json"),
            (format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{invalid}", invalid.len()).into_bytes(), "profile_body_invalid_json"),
            (b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{".to_vec(), "profile_body_failed"),
            (b"HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n".to_vec(), "provider_edge_challenge"),
            (b"HTTP/1.1 302 Found\r\nLocation: https://other.invalid/private\r\nContent-Length: 0\r\n\r\n".to_vec(), "redirect_refused"),
        ] {
            let report = json_fixture(response).await.unwrap_err();
            assert_eq!(report.state_label(), state);
            assert!(!report.identity_validated());
            for secret in ["fixture-token", "private transcript", "other.invalid"] {
                assert!(!serde_json::to_string(&report).unwrap().contains(secret));
                assert!(!format!("{report:?}").contains(secret));
            }
        }
    }

    #[tokio::test]
    async fn opaque_profile_id_is_one_component_with_pinned_origin_and_suffix() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tbo", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            let path = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let parsed = reqwest::Url::parse(&format!("http://fixture.invalid{path}")).unwrap();
            assert_eq!(parsed.path_segments().unwrap().count(), 3);
            assert!(parsed.path().starts_with("/tbo/fixture:"));
            assert!(parsed.path().ends_with("/root-thread"));
            assert!(parsed.path().contains("%2F"));
            assert!(parsed.path().contains("%25"));
            assert!(parsed.query().is_none());
            assert!(parsed.fragment().is_none());
            assert!(!request.to_ascii_lowercase().contains("cookie:"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
        });
        let client = ProfileClient::fixture(base, HeaderMap::new());
        client
            .read(ProfileRoute::RootThread(
                "fixture:dot/opaque?query#fragment%2f",
            ))
            .await
            .unwrap();
        server.await.unwrap();
        for bad in ["", ".", "..", "\nprivate", " leading", "trailing "] {
            assert!(!profile::valid_resource_id(bad));
        }
        assert!(!profile::valid_resource_id(&"a".repeat(513)));
    }

    #[tokio::test]
    async fn profile_routes_refuse_path_injection_before_sending_any_request() {
        let client = ProfileClient::fixture("http://127.0.0.1:9/tbo".into(), HeaderMap::new());
        for route in [
            ProfileRoute::ByThread("../escape"),
            ProfileRoute::ByThread("00000000-0000-7000-8000-000000000001?query"),
            ProfileRoute::RootThread(".."),
            ProfileRoute::RootThread("dot\ninvalid"),
        ] {
            assert_eq!(
                client.read(route).await.unwrap_err().state_label(),
                "profile_route_identity_invalid"
            );
        }
    }
}
