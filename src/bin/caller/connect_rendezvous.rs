//! Outbound Intendant Connect rendezvous client for dashboard-control signaling.
//!
//! This module intentionally implements only signaling. It does not authorize a
//! browser, issue grants, or replace mTLS dashboard access. A production Connect
//! service must wrap this with account/passkey/device policy; this client is the
//! daemon-side transport substrate and local E2E hook.

use crate::daemon_identity::DaemonIdentity;
use crate::dashboard_control::DashboardControlRegistry;
use crate::project::ConnectConfig;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Serialize)]
struct RegisterRequest {
    protocol: &'static str,
    daemon_id: String,
    daemon_public_key: String,
}

#[derive(Debug, Deserialize)]
struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default)]
    sdp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    candidate: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct AnswerRequest {
    protocol: &'static str,
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: crate::dashboard_control::DashboardControlBinding,
}

#[derive(Debug, Serialize)]
struct ErrorRequest {
    daemon_id: String,
    request_id: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct AckRequest {
    daemon_id: String,
    request_id: String,
    ok: bool,
}

pub fn spawn_connect_rendezvous_client(
    config: ConnectConfig,
    dashboard_control: Arc<DashboardControlRegistry>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        return None;
    }
    let Some(base_url) = config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        eprintln!("[connect] enabled but no rendezvous_url is configured");
        return None;
    };
    let base_url = match Url::parse(base_url) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("[connect] invalid rendezvous_url {base_url:?}: {e}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        run_connect_rendezvous_client(config, base_url, dashboard_control).await;
    }))
}

async fn run_connect_rendezvous_client(
    config: ConnectConfig,
    base_url: Url,
    dashboard_control: Arc<DashboardControlRegistry>,
) {
    let identity = match DaemonIdentity::load_or_create_default() {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("[connect] daemon identity unavailable: {e}");
            return;
        }
    };
    let daemon_public_key = identity.public_key_b64u();
    let daemon_id = config
        .daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| daemon_public_key.clone());
    let client = match Client::builder()
        .timeout(Duration::from_millis(
            config.poll_timeout_ms.saturating_add(10_000).max(10_000),
        ))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("[connect] failed to build HTTP client: {e}");
            return;
        }
    };
    let retry_delay = Duration::from_millis(config.retry_delay_ms.max(100));
    eprintln!("[connect] rendezvous client enabled for daemon {daemon_id}");

    loop {
        match register(&client, &base_url, &config, &daemon_id, &daemon_public_key).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[connect] register failed: {e}");
                tokio::time::sleep(retry_delay).await;
                continue;
            }
        }

        loop {
            match poll_next(&client, &base_url, &config, &daemon_id).await {
                Ok(Some(event)) => {
                    handle_event(
                        &client,
                        &base_url,
                        &config,
                        &daemon_id,
                        &dashboard_control,
                        event,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[connect] poll failed: {e}");
                    tokio::time::sleep(retry_delay).await;
                    break;
                }
            }
        }
    }
}

async fn register(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    daemon_public_key: &str,
) -> Result<(), String> {
    let request = RegisterRequest {
        protocol: "intendant-connect-rendezvous-v1",
        daemon_id: daemon_id.to_string(),
        daemon_public_key: daemon_public_key.to_string(),
    };
    authenticated(
        config,
        client.post(join_url(base_url, "api/daemon/register")?),
    )
    .json(&request)
    .send()
    .await
    .map_err(|e| e.to_string())?
    .error_for_status()
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn poll_next(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
) -> Result<Option<RendezvousEvent>, String> {
    let mut url = join_url(base_url, "api/daemon/next")?;
    url.query_pairs_mut()
        .append_pair("daemon_id", daemon_id)
        .append_pair("timeout_ms", &config.poll_timeout_ms.to_string());
    let response = authenticated(config, client.get(url))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    response
        .json::<RendezvousEvent>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

async fn handle_event(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    dashboard_control: &Arc<DashboardControlRegistry>,
    event: RendezvousEvent,
) {
    match event.kind.as_str() {
        "offer" => {
            let Some(sdp) = event.sdp.as_deref().filter(|s| !s.trim().is_empty()) else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing sdp",
                )
                .await;
                return;
            };
            match dashboard_control.answer_offer(sdp.to_string()).await {
                Ok(answer) => {
                    let body = AnswerRequest {
                        protocol: "intendant-connect-rendezvous-v1",
                        daemon_id: daemon_id.to_string(),
                        request_id: event.id,
                        session_id: answer.session_id,
                        sdp: answer.sdp,
                        binding: answer.binding,
                    };
                    if let Err(e) = authenticated(
                        config,
                        client.post(match join_url(base_url, "api/daemon/answer") {
                            Ok(url) => url,
                            Err(e) => {
                                eprintln!("[connect] answer URL failed: {e}");
                                return;
                            }
                        }),
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|resp| {
                        resp.error_for_status()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }) {
                        eprintln!("[connect] post answer failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = post_error(client, base_url, config, daemon_id, &event.id, &e).await;
                }
            }
        }
        "ice" => {
            let ok = match (event.session_id.as_deref(), event.candidate.as_ref()) {
                (Some(session_id), Some(candidate)) => dashboard_control
                    .add_ice_candidate(session_id, candidate)
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            let _ = post_ack(client, base_url, config, daemon_id, &event.id, ok).await;
        }
        "close" => {
            if let Some(session_id) = event.session_id.as_deref() {
                dashboard_control.close(session_id).await;
            }
            let _ = post_ack(client, base_url, config, daemon_id, &event.id, true).await;
        }
        other => {
            let _ = post_error(
                client,
                base_url,
                config,
                daemon_id,
                &event.id,
                &format!("unknown event kind: {other}"),
            )
            .await;
        }
    }
}

async fn post_error(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    error: &str,
) -> Result<(), String> {
    let body = ErrorRequest {
        daemon_id: daemon_id.to_string(),
        request_id: request_id.to_string(),
        error: error.to_string(),
    };
    authenticated(config, client.post(join_url(base_url, "api/daemon/error")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn post_ack(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    ok: bool,
) -> Result<(), String> {
    let body = AckRequest {
        daemon_id: daemon_id.to_string(),
        request_id: request_id.to_string(),
        ok,
    };
    authenticated(config, client.post(join_url(base_url, "api/daemon/ack")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn authenticated(config: &ConnectConfig, builder: RequestBuilder) -> RequestBuilder {
    match config
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(token) => builder.bearer_auth(token),
        None => builder,
    }
}

fn join_url(base_url: &Url, path: &str) -> Result<Url, String> {
    let mut url = base_url.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "rendezvous_url cannot be a base URL".to_string())?;
        let base_segments: Vec<String> = base_url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        segments.clear();
        for segment in base_segments {
            segments.push(&segment);
        }
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_appends_under_base() {
        let base = Url::parse("https://connect.example/root/").unwrap();
        assert_eq!(
            join_url(&base, "api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }

    #[test]
    fn join_url_treats_base_path_without_slash_as_directory() {
        let base = Url::parse("https://connect.example/root?ignored=true#frag").unwrap();
        assert_eq!(
            join_url(&base, "/api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }
}
