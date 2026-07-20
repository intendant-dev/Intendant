//! Authenticated helpers for Kimi 0.27's reflected `/api/v2` agent RPCs.
//!
//! Kimi's v1 REST facade intentionally exposes a conservative subset of the
//! underlying agent services. The bearer-authenticated v2 dispatcher exposes
//! registered services directly at
//! `/api/v2/session/{session}/agent/{agent}/{service}/{method}`. Keep that
//! reflection boundary private here: callers get typed, allowlisted helpers
//! rather than a user-controlled service or method name.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CallerError;

use super::wire::{component, external};

const MAIN_AGENT_ID: &str = "main";
const AGENT_GOAL_SERVICE: &str = "agentGoalService";
const AGENT_PROFILE_SERVICE: &str = "agentProfileService";
const AGENT_RPC_SERVICE: &str = "agentRPCService";
const MODEL_CATALOG_SERVICE: &str = "modelCatalogService";
const KIMI_RPC_RESPONSE_LIMIT: usize = 32 * 1024 * 1024;

/// Native Kimi goal limits. Kimi 0.27 enforces all three in the goal service;
/// omitted fields leave the corresponding existing limit unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KimiGoalBudgetLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) turn_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) wall_clock_budget_ms: Option<u64>,
}

impl KimiGoalBudgetLimits {
    fn is_empty(&self) -> bool {
        self.token_budget.is_none()
            && self.turn_budget.is_none()
            && self.wall_clock_budget_ms.is_none()
    }
}

/// One tool from `IAgentRPCService.getTools`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct KimiRpcTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) active: bool,
    pub(crate) source: String,
}

/// Kimi's current model-facing context for one exact agent.
///
/// This is the native `AgentRPCService.getContext` payload, not a
/// reconstruction from the durable wire transcript. `tokenCount` is Kimi's
/// measured prefix count and can therefore legitimately be zero before the
/// first provider response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KimiRpcContext {
    pub(crate) history: Vec<Value>,
    pub(crate) token_count: u64,
}

/// One configured model alias from Kimi's app-scoped model catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct KimiRpcModel {
    pub(crate) provider: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    pub(crate) max_context_size: u64,
    #[serde(default)]
    pub(crate) capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) support_efforts: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) default_effort: Option<String>,
}

/// Result of switching one live agent to a configured model alias.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KimiRpcSetModelResult {
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) provider_name: Option<String>,
}

/// Non-sensitive agent profile facts returned by `IAgentProfileService.data`.
///
/// Unknown fields are deliberately discarded: the response also contains the
/// full system prompt, which this adapter must not retain or accidentally log.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct KimiRpcProfile {
    #[serde(default)]
    pub(crate) model_alias: Option<String>,
    #[serde(default)]
    pub(crate) profile_name: Option<String>,
    #[serde(default)]
    pub(crate) thinking_level: Option<String>,
    #[serde(default)]
    pub(crate) active_tool_names: Option<Vec<String>>,
}

#[derive(Clone)]
pub(crate) struct KimiRpcApi {
    client: reqwest::Client,
    origin: String,
    token: String,
}

impl KimiRpcApi {
    pub(crate) fn new(origin: String, token: String) -> Result<Self, CallerError> {
        let origin = normalize_loopback_origin(&origin)?;
        let client = reqwest::Client::builder()
            // Never send the private loopback bearer or RPC payloads through
            // an ambient HTTP(S)_PROXY configured for ordinary egress.
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| external(format!("failed to build Kimi v2 HTTP client: {error}")))?;
        Ok(Self {
            client,
            origin,
            token,
        })
    }

    /// Fail closed when the reflected service surface is not the 0.27 shape
    /// this adapter was built against.
    pub(crate) async fn validate_required_methods(&self) -> Result<(), CallerError> {
        let channels = self
            .request_path(Method::GET, "/api/v2/channels", None, "channels")
            .await?;
        let channels = channels
            .as_array()
            .ok_or_else(|| external("Kimi /api/v2/channels returned a malformed catalog"))?;
        for (service, methods) in [
            (
                AGENT_GOAL_SERVICE,
                &["getGoal", "markComplete", "setBudgetLimits"][..],
            ),
            (
                AGENT_RPC_SERVICE,
                &["setActiveTools", "getTools", "getContext", "clearContext"][..],
            ),
            (AGENT_PROFILE_SERVICE, &["data", "setModel"][..]),
            (MODEL_CATALOG_SERVICE, &["listModels"][..]),
        ] {
            let Some(channel) = channels
                .iter()
                .find(|channel| channel.get("name").and_then(Value::as_str) == Some(service))
            else {
                return Err(external(format!(
                    "Kimi v2 RPC catalog omitted required service {service}"
                )));
            };
            let advertised = channel
                .get("methods")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    external(format!(
                        "Kimi v2 RPC catalog returned malformed methods for {service}"
                    ))
                })?;
            for method in methods {
                if !advertised.iter().any(|candidate| {
                    candidate.get("name").and_then(Value::as_str) == Some(*method)
                        && candidate.get("kind").and_then(Value::as_str) == Some("method")
                }) {
                    return Err(external(format!(
                        "Kimi v2 RPC catalog omitted required method {service}.{method}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Read the main agent's native goal snapshot.
    ///
    /// `IAgentGoalService.getGoal()` returns `{ goal: GoalSnapshot | null }`.
    pub(crate) async fn goal_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<Value>, CallerError> {
        let value = self
            .call_agent(
                session_id,
                MAIN_AGENT_ID,
                AGENT_GOAL_SERVICE,
                "getGoal",
                serde_json::json!([]),
            )
            .await?;
        let object = value
            .as_object()
            .ok_or_else(|| external("Kimi agentGoalService.getGoal returned a malformed result"))?;
        match object.get("goal") {
            Some(Value::Null) => Ok(None),
            Some(goal @ Value::Object(_)) => Ok(Some(goal.clone())),
            Some(_) | None => Err(external(
                "Kimi agentGoalService.getGoal omitted a valid goal field",
            )),
        }
    }

    /// Mark an active native goal complete as the operator.
    ///
    /// Kimi returns the terminal snapshot, then clears its internal standing
    /// goal. `null` means there was no active goal to complete.
    pub(crate) async fn mark_goal_complete(
        &self,
        session_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<Value>, CallerError> {
        let reason = reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(|reason| serde_json::json!({ "reason": reason }))
            .unwrap_or_else(|| serde_json::json!({}));
        let value = self
            .call_agent(
                session_id,
                MAIN_AGENT_ID,
                AGENT_GOAL_SERVICE,
                "markComplete",
                serde_json::json!([reason, "user"]),
            )
            .await?;
        match value {
            Value::Null => Ok(None),
            Value::Object(_) => Ok(Some(value)),
            _ => Err(external(
                "Kimi agentGoalService.markComplete returned a malformed result",
            )),
        }
    }

    /// Set one or more native goal budgets as the operator.
    ///
    /// The returned snapshot includes Kimi's effective limits, remaining
    /// budget, and reached flags. If a new limit is already exhausted, Kimi
    /// immediately transitions the goal to `blocked`.
    pub(crate) async fn set_goal_budget_limits(
        &self,
        session_id: &str,
        limits: KimiGoalBudgetLimits,
    ) -> Result<Value, CallerError> {
        if limits.is_empty() {
            return Err(external("at least one Kimi goal budget limit is required"));
        }
        let value = self
            .call_agent(
                session_id,
                MAIN_AGENT_ID,
                AGENT_GOAL_SERVICE,
                "setBudgetLimits",
                serde_json::json!([{ "budgetLimits": limits }, "user"]),
            )
            .await?;
        if !value.is_object() {
            return Err(external(
                "Kimi agentGoalService.setBudgetLimits returned a malformed result",
            ));
        }
        Ok(value)
    }

    /// Replace the active-tool name set for one live Kimi agent.
    ///
    /// An empty list deliberately disables every optional tool. Kimi persists
    /// this profile mutation in the agent's wire journal.
    pub(crate) async fn set_active_tools(
        &self,
        session_id: &str,
        agent_id: &str,
        names: &[String],
    ) -> Result<(), CallerError> {
        self.call_agent(
            session_id,
            agent_id,
            AGENT_RPC_SERVICE,
            "setActiveTools",
            serde_json::json!({ "names": names }),
        )
        .await
        .and_then(expect_null_rpc_result)
    }

    /// Read the active/inactive state of every registered tool for one agent.
    pub(crate) async fn tools(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<Vec<KimiRpcTool>, CallerError> {
        let value = self
            .call_agent(
                session_id,
                agent_id,
                AGENT_RPC_SERVICE,
                "getTools",
                serde_json::json!({}),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| external(format!("Kimi agentRPCService.getTools malformed: {error}")))
    }

    /// Read the current, post-compaction model context of one exact agent.
    pub(crate) async fn context(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<KimiRpcContext, CallerError> {
        let value = self
            .call_agent(
                session_id,
                agent_id,
                AGENT_RPC_SERVICE,
                "getContext",
                serde_json::json!({}),
            )
            .await?;
        serde_json::from_value(value).map_err(|error| {
            external(format!(
                "Kimi agentRPCService.getContext malformed: {error}"
            ))
        })
    }

    /// Read profile facts, including the persisted active-tool base.
    pub(crate) async fn profile(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<KimiRpcProfile, CallerError> {
        let value = self
            .call_agent(
                session_id,
                agent_id,
                AGENT_PROFILE_SERVICE,
                "data",
                serde_json::json!([]),
            )
            .await?;
        serde_json::from_value(value).map_err(|error| {
            external(format!(
                "Kimi agentProfileService.data returned malformed profile facts: {error}"
            ))
        })
    }

    /// List configured model aliases without changing Kimi's global default.
    ///
    /// The catalog lets `/fast` discover a real `*-highspeed` companion for
    /// the current model rather than guessing or silently changing providers.
    pub(crate) async fn models(&self) -> Result<Vec<KimiRpcModel>, CallerError> {
        let value = self
            .call_core(MODEL_CATALOG_SERVICE, "listModels", serde_json::json!([]))
            .await?;
        serde_json::from_value(value).map_err(|error| {
            external(format!(
                "Kimi modelCatalogService.listModels malformed: {error}"
            ))
        })
    }

    /// Switch one live Kimi agent to an already configured model alias.
    ///
    /// This is intentionally narrower than profile `bind`/`update`: it cannot
    /// replace the system prompt, working directory, profile, or tool set.
    pub(crate) async fn set_model(
        &self,
        session_id: &str,
        agent_id: &str,
        alias: &str,
    ) -> Result<KimiRpcSetModelResult, CallerError> {
        let alias = alias.trim();
        if alias.is_empty() {
            return Err(external("Kimi model alias must not be empty"));
        }
        let value = self
            .call_agent(
                session_id,
                agent_id,
                AGENT_PROFILE_SERVICE,
                "setModel",
                serde_json::json!(alias),
            )
            .await?;
        let result: KimiRpcSetModelResult = serde_json::from_value(value).map_err(|error| {
            external(format!(
                "Kimi agentProfileService.setModel returned a malformed result: {error}"
            ))
        })?;
        if result.model != alias {
            return Err(external(format!(
                "Kimi agentProfileService.setModel returned model {:?} after requesting {:?}",
                result.model, alias
            )));
        }
        Ok(result)
    }

    /// Destructively clear one agent's conversation context.
    ///
    /// This is Kimi's native context-clear operation. It is not equivalent to
    /// Codex `memory/reset`, which clears a separate persistent-memory plane.
    pub(crate) async fn clear_context(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<(), CallerError> {
        self.call_agent(
            session_id,
            agent_id,
            AGENT_RPC_SERVICE,
            "clearContext",
            serde_json::json!({}),
        )
        .await
        .and_then(expect_null_rpc_result)
    }

    async fn call_agent(
        &self,
        session_id: &str,
        agent_id: &str,
        service: &'static str,
        method: &'static str,
        argument: Value,
    ) -> Result<Value, CallerError> {
        self.call_path(
            &format!(
                "/api/v2/session/{}/agent/{}/{}/{}",
                component(session_id),
                component(agent_id),
                component(service),
                component(method)
            ),
            service,
            method,
            argument,
        )
        .await
    }

    async fn call_core(
        &self,
        service: &'static str,
        method: &'static str,
        argument: Value,
    ) -> Result<Value, CallerError> {
        self.call_path(
            &format!("/api/v2/{}/{}", component(service), component(method)),
            service,
            method,
            argument,
        )
        .await
    }

    async fn call_path(
        &self,
        path: &str,
        service: &'static str,
        method: &'static str,
        argument: Value,
    ) -> Result<Value, CallerError> {
        self.request_path(
            Method::POST,
            path,
            Some(&argument),
            &format!("{service}.{method}"),
        )
        .await
    }

    async fn request_path(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        operation: &str,
    ) -> Result<Value, CallerError> {
        let authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|_| external("Kimi server token contains invalid header bytes"))?;
        let mut request = self
            .client
            .request(method, format!("{}{}", self.origin, path))
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| external(format!("Kimi v2 RPC request failed: {error}")))?;
        decode_rpc_response(response, operation).await
    }
}

fn expect_null_rpc_result(value: Value) -> Result<(), CallerError> {
    if value.is_null() {
        Ok(())
    } else {
        Err(external("Kimi void RPC returned a non-null result"))
    }
}

async fn decode_rpc_response(
    mut response: reqwest::Response,
    operation: &str,
) -> Result<Value, CallerError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > KIMI_RPC_RESPONSE_LIMIT as u64)
    {
        return Err(external(format!(
            "Kimi v2 RPC response for {operation} exceeds the {} byte limit",
            KIMI_RPC_RESPONSE_LIMIT
        )));
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(KIMI_RPC_RESPONSE_LIMIT),
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| external(format!("failed to read Kimi v2 RPC response: {error}")))?
    {
        if bytes.len().saturating_add(chunk.len()) > KIMI_RPC_RESPONSE_LIMIT {
            return Err(external(format!(
                "Kimi v2 RPC response for {operation} exceeds the {} byte limit",
                KIMI_RPC_RESPONSE_LIMIT
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| {
        external(format!(
            "Kimi v2 RPC returned non-JSON for {operation} (HTTP {status}): {error}"
        ))
    })?;
    if status.is_success() && envelope.get("code").and_then(Value::as_i64) == Some(0) {
        return Ok(envelope.get("data").cloned().unwrap_or(Value::Null));
    }
    let wire_message = envelope
        .get("msg")
        .or_else(|| envelope.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("request rejected");
    let wire_code = envelope
        .get("code")
        .map(Value::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    Err(external(format!(
        "Kimi {operation} failed (HTTP {}, code {}): {}",
        status.as_u16(),
        wire_code,
        wire_message
    )))
}

fn normalize_loopback_origin(origin: &str) -> Result<String, CallerError> {
    let url = reqwest::Url::parse(origin)
        .map_err(|error| external(format!("invalid Kimi server origin: {error}")))?;
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        return Err(external(
            "refusing non-loopback Kimi server origin for supervised session",
        ));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(external("unsupported Kimi server URL scheme"));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| external("Kimi server origin has no port"))?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{}://{}:{}", url.scheme(), host, port))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    async fn mock_server(
        response_status: &str,
        response_body: Value,
    ) -> (
        String,
        oneshot::Receiver<Vec<u8>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();
        let status = response_status.to_string();
        let body = response_body.to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let expected_len = loop {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "mock client closed before request completed");
                request.extend_from_slice(&buf[..read]);
                let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                break header_end + content_len;
            };
            while request.len() < expected_len {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "mock client closed before body completed");
                request.extend_from_slice(&buf[..read]);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = request_tx.send(request);
        });
        (format!("http://{address}"), request_rx, handle)
    }

    fn request_parts(request: &[u8]) -> (&str, String, Value) {
        let raw = std::str::from_utf8(request).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        (
            headers.lines().next().unwrap(),
            headers.to_ascii_lowercase(),
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(body).unwrap()
            },
        )
    }

    fn goal_snapshot() -> Value {
        serde_json::json!({
            "goalId": "goal-1",
            "objective": "finish",
            "status": "active",
            "turnsUsed": 2,
            "tokensUsed": 300,
            "wallClockMs": 4000,
            "budget": {
                "tokenBudget": 1000,
                "turnBudget": null,
                "wallClockBudgetMs": null,
                "remainingTokens": 700,
                "remainingTurns": null,
                "remainingWallClockMs": null,
                "tokenBudgetReached": false,
                "turnBudgetReached": false,
                "wallClockBudgetReached": false,
                "overBudget": false
            }
        })
    }

    #[test]
    fn v2_origin_remains_loopback_only() {
        assert_eq!(
            normalize_loopback_origin("http://localhost:1234/#token=secret").unwrap(),
            "http://localhost:1234"
        );
        assert!(normalize_loopback_origin("https://example.com:443").is_err());
        assert!(normalize_loopback_origin("file:///tmp/kimi.sock").is_err());
    }

    #[tokio::test]
    async fn required_method_handshake_uses_authenticated_channel_catalog() {
        let channels = serde_json::json!([
            {
                "name": "agentGoalService",
                "methods": [
                    {"name": "getGoal", "kind": "method"},
                    {"name": "markComplete", "kind": "method"},
                    {"name": "setBudgetLimits", "kind": "method"}
                ]
            },
            {
                "name": "agentRPCService",
                "methods": [
                    {"name": "setActiveTools", "kind": "method"},
                    {"name": "getTools", "kind": "method"},
                    {"name": "getContext", "kind": "method"},
                    {"name": "clearContext", "kind": "method"}
                ]
            },
            {
                "name": "agentProfileService",
                "methods": [
                    {"name": "data", "kind": "method"},
                    {"name": "setModel", "kind": "method"}
                ]
            },
            {
                "name": "modelCatalogService",
                "methods": [{"name": "listModels", "kind": "method"}]
            }
        ]);
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": channels}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "handshake-token".into()).unwrap();
        api.validate_required_methods().await.unwrap();
        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(line, "GET /api/v2/channels HTTP/1.1");
        assert!(headers.contains("\r\nauthorization: bearer handshake-token\r\n"));
        assert_eq!(body, Value::Null);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn required_method_handshake_rejects_contextless_rpc_catalog() {
        let channels = serde_json::json!([
            {
                "name": "agentGoalService",
                "methods": [
                    {"name": "getGoal", "kind": "method"},
                    {"name": "markComplete", "kind": "method"},
                    {"name": "setBudgetLimits", "kind": "method"}
                ]
            },
            {
                "name": "agentRPCService",
                "methods": [
                    {"name": "setActiveTools", "kind": "method"},
                    {"name": "getTools", "kind": "method"},
                    {"name": "clearContext", "kind": "method"}
                ]
            },
            {
                "name": "agentProfileService",
                "methods": [
                    {"name": "data", "kind": "method"},
                    {"name": "setModel", "kind": "method"}
                ]
            },
            {
                "name": "modelCatalogService",
                "methods": [{"name": "listModels", "kind": "method"}]
            }
        ]);
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": channels}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "handshake-token".into()).unwrap();
        let error = api.validate_required_methods().await.unwrap_err();
        assert!(error.to_string().contains("agentRPCService.getContext"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn mark_complete_uses_exact_reflection_method_bearer_and_encoded_scope() {
        let terminal = {
            let mut snapshot = goal_snapshot();
            snapshot["status"] = Value::String("complete".into());
            snapshot
        };
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": terminal}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "test-v2-token".into()).unwrap();
        let completed = api
            .mark_goal_complete("session/a:b", Some("all checks passed"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed["status"], "complete");

        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session%2Fa%3Ab/agent/main/agentGoalService/markComplete HTTP/1.1"
        );
        assert!(headers.contains("\r\nauthorization: bearer test-v2-token\r\n"));
        assert_eq!(
            body,
            serde_json::json!([{"reason": "all checks passed"}, "user"])
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn goal_budget_call_uses_native_limits_shape_and_user_actor() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": goal_snapshot()}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let snapshot = api
            .set_goal_budget_limits(
                "session-1",
                KimiGoalBudgetLimits {
                    token_budget: Some(12_000),
                    turn_budget: Some(8),
                    wall_clock_budget_ms: Some(90_000),
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot["goalId"], "goal-1");

        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session-1/agent/main/agentGoalService/setBudgetLimits HTTP/1.1"
        );
        assert_eq!(
            body,
            serde_json::json!([
                {
                    "budgetLimits": {
                        "tokenBudget": 12_000,
                        "turnBudget": 8,
                        "wallClockBudgetMs": 90_000
                    }
                },
                "user"
            ])
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn active_tools_target_exact_agent_and_allow_empty_replacement() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": null}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        api.set_active_tools("session/x", "agent:a/b", &[])
            .await
            .unwrap();

        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session%2Fx/agent/agent%3Aa%2Fb/agentRPCService/setActiveTools HTTP/1.1"
        );
        assert_eq!(body, serde_json::json!({"names": []}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tools_and_profile_use_the_reflected_method_names() {
        let tools_response = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": [{
                "name": "Read",
                "description": "read files",
                "active": true,
                "source": "builtin"
            }]
        });
        let (origin, request, server) = mock_server("200 OK", tools_response).await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let tools = api.tools("session-1", "main").await.unwrap();
        assert_eq!(tools[0].name, "Read");
        assert!(tools[0].active);
        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session-1/agent/main/agentRPCService/getTools HTTP/1.1"
        );
        assert_eq!(body, serde_json::json!({}));
        server.await.unwrap();

        let profile_response = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": {
                "cwd": "/repo",
                "modelAlias": "kimi-for-coding",
                "modelCapabilities": {},
                "thinkingLevel": "high",
                "systemPrompt": "hidden",
                "activeToolNames": ["Read"]
            }
        });
        let (origin, request, server) = mock_server("200 OK", profile_response).await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let profile = api.profile("session-1", "main").await.unwrap();
        assert_eq!(
            profile.active_tool_names.as_deref(),
            Some(["Read".to_string()].as_slice())
        );
        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session-1/agent/main/agentProfileService/data HTTP/1.1"
        );
        assert_eq!(body, serde_json::json!([]));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn context_uses_exact_agent_scope_and_preserves_native_history() {
        let native = serde_json::json!({
            "history": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "child prompt"}],
                    "origin": {"kind": "user"}
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "child answer"}],
                    "toolCalls": []
                }
            ],
            "tokenCount": 417
        });
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": native}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "context-token".into()).unwrap();
        let context = api.context("session/a:b", "agent/a:b").await.unwrap();
        assert_eq!(context.token_count, 417);
        assert_eq!(context.history.len(), 2);

        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session%2Fa%3Ab/agent/agent%2Fa%3Ab/agentRPCService/getContext HTTP/1.1"
        );
        assert!(headers.contains("\r\nauthorization: bearer context-token\r\n"));
        assert_eq!(body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_context_is_rejected_instead_of_retyped_as_a_transcript() {
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 0,
                "msg": "success",
                "data": {"history": "not-an-array", "tokenCount": -1}
            }),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let error = api.context("session-1", "main").await.unwrap_err();
        assert!(error.to_string().contains("getContext malformed"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn model_catalog_is_an_authenticated_core_scope_call() {
        let response = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": [{
                "provider": "kimi-code",
                "model": "kimi-code/kimi-for-coding-highspeed",
                "display_name": "K2.7 Coding Highspeed",
                "max_context_size": 262144,
                "capabilities": ["thinking"],
                "support_efforts": ["low", "high"],
                "default_effort": "high"
            }]
        });
        let (origin, request, server) = mock_server("200 OK", response).await;
        let api = KimiRpcApi::new(origin, "catalog-token".into()).unwrap();
        let models = api.models().await.unwrap();
        assert_eq!(models[0].model, "kimi-code/kimi-for-coding-highspeed");
        assert_eq!(
            models[0].display_name.as_deref(),
            Some("K2.7 Coding Highspeed")
        );

        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(line, "POST /api/v2/modelCatalogService/listModels HTTP/1.1");
        assert!(headers.contains("\r\nauthorization: bearer catalog-token\r\n"));
        assert_eq!(body, serde_json::json!([]));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn set_model_uses_exact_profile_method_and_decodes_result() {
        let response = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": {
                "model": "kimi-code/kimi-for-coding-highspeed",
                "providerName": "kimi-code"
            }
        });
        let (origin, request, server) = mock_server("200 OK", response).await;
        let api = KimiRpcApi::new(origin, "model-token".into()).unwrap();
        let result = api
            .set_model(
                "session/a:b",
                "agent:a/b",
                "kimi-code/kimi-for-coding-highspeed",
            )
            .await
            .unwrap();
        assert_eq!(result.model, "kimi-code/kimi-for-coding-highspeed");
        assert_eq!(result.provider_name.as_deref(), Some("kimi-code"));

        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v2/session/session%2Fa%3Ab/agent/agent%3Aa%2Fb/agentProfileService/setModel HTTP/1.1"
        );
        assert!(headers.contains("\r\nauthorization: bearer model-token\r\n"));
        assert_eq!(
            body,
            serde_json::json!("kimi-code/kimi-for-coding-highspeed")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn set_model_rejects_void_or_mismatched_results() {
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "msg": "success", "data": null}),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let error = api
            .set_model("session-1", "main", "kimi-code/model")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("malformed result"));
        server.await.unwrap();

        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 0,
                "msg": "success",
                "data": {"model": "kimi-code/other"}
            }),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let error = api
            .set_model("session-1", "main", "kimi-code/model")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("after requesting"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn error_envelope_preserves_wire_code_without_leaking_details() {
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 40422,
                "msg": "No active goal",
                "data": null,
                "details": {"private": "must not appear"}
            }),
        )
        .await;
        let api = KimiRpcApi::new(origin, "token".into()).unwrap();
        let error = api
            .set_goal_budget_limits(
                "session-1",
                KimiGoalBudgetLimits {
                    token_budget: Some(1_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("code 40422"), "{message}");
        assert!(message.contains("No active goal"), "{message}");
        assert!(!message.contains("private"), "{message}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_budget_is_rejected_before_network_io() {
        let api = KimiRpcApi::new("http://127.0.0.1:9".into(), "token".into()).unwrap();
        let error = api
            .set_goal_budget_limits("session-1", KimiGoalBudgetLimits::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn rpc_response_content_length_is_rejected_before_body_buffering() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                KIMI_RPC_RESPONSE_LIMIT + 1
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let api = KimiRpcApi::new(format!("http://{address}"), "bounded-token".into()).unwrap();
        let error = api.tools("session-1", "main").await.unwrap_err();
        assert!(error.to_string().contains("exceeds"));
        assert!(error.to_string().contains("getTools"));
        server.await.unwrap();
    }
}
