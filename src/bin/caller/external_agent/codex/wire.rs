//! JSON-RPC wire vocabulary for the codex app-server pipe, pending-request
//! bookkeeping, and the inherited-MCP-server config suppression overrides.

use super::*;

#[derive(Serialize)]
pub(crate) struct JsonRpcRequest {
    pub(crate) jsonrpc: String,
    pub(crate) id: u64,
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub(crate) struct JsonRpcNotification {
    pub(crate) jsonrpc: String,
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<serde_json::Value>,
}

/// Response sent back to server-initiated requests (e.g. approval responses).
#[derive(Serialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: String,
    pub(crate) id: u64,
    pub(crate) result: serde_json::Value,
}

/// Unified incoming message: can be a response, notification, or server request.
#[derive(Deserialize)]
pub(crate) struct JsonRpcMessage {
    pub(crate) id: Option<u64>,
    pub(crate) method: Option<String>,
    pub(crate) params: Option<serde_json::Value>,
    pub(crate) result: Option<serde_json::Value>,
    pub(crate) error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct JsonRpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

// ---------------------------------------------------------------------------
// Pending-request bookkeeping
// ---------------------------------------------------------------------------

/// Value resolved for a pending outbound request: either `Ok(result)` or a
/// stringified error.
pub(crate) type RequestResult = Result<serde_json::Value, String>;

pub(crate) type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<RequestResult>>>>;

/// Maps our synthetic `request_id` strings back to the JSON-RPC `id` from
/// server-initiated approval requests.
/// Stores the original request shape so resolve_approval can answer each
/// approval method with the correct protocol response.
pub(crate) type PendingApprovals = Arc<Mutex<HashMap<String, PendingApproval>>>;

#[derive(Debug, Clone)]
pub(crate) struct PendingApproval {
    pub(crate) jsonrpc_id: u64,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// Active Codex turns keyed by native thread id. Codex can run multiple
/// threads through one app-server process, so one global active turn is not
/// enough once `/side` can start while the parent turn is still running.
pub(crate) type ActiveTurns = Arc<Mutex<HashMap<String, String>>>;

pub(crate) fn codex_mcp_server_names_from_home(home: &Path) -> Vec<String> {
    let Ok(config) = std::fs::read_to_string(home.join("config.toml")) else {
        return Vec::new();
    };
    codex_mcp_server_names_from_config_toml(&config)
}

pub(crate) fn codex_mcp_server_names_from_config_toml(config: &str) -> Vec<String> {
    let Ok(value) = config.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(servers) = value.get("mcp_servers").and_then(|value| value.as_table()) else {
        return Vec::new();
    };

    let mut names: Vec<String> = servers
        .keys()
        .filter_map(|name| {
            let trimmed = name.trim();
            if trimmed.eq_ignore_ascii_case("intendant") {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();
    names.sort();
    names
}

pub(crate) fn codex_mcp_server_disable_override(server_names: &[String]) -> Option<String> {
    if server_names.is_empty() {
        return None;
    }
    let servers = server_names
        .iter()
        .map(|name| format!("{}={{enabled=false}}", codex_toml_inline_key(name)))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("mcp_servers={{{servers}}}"))
}

pub(crate) fn codex_inherited_config_suppression_overrides(
    codex_home: Option<&Path>,
    managed_context: bool,
    inherit_configured_mcp_servers: bool,
) -> Vec<String> {
    if !managed_context || inherit_configured_mcp_servers {
        return Vec::new();
    }

    // Plugins can contribute app/connector MCP servers. Intendant-managed
    // Codex starts with only Intendant MCP unless inheritance is explicit.
    let mut overrides = vec!["features.plugins=false".to_string()];
    let server_names = codex_home
        .map(codex_mcp_server_names_from_home)
        .unwrap_or_default();
    if let Some(disable_override) = codex_mcp_server_disable_override(&server_names) {
        overrides.push(disable_override);
    }
    overrides
}

pub(crate) fn codex_toml_inline_key(name: &str) -> String {
    if codex_mcp_server_name_can_be_toml_bare_key(name) {
        name.to_string()
    } else {
        toml::Value::String(name.to_string()).to_string()
    }
}

pub(crate) fn codex_mcp_server_name_can_be_toml_bare_key(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(crate) fn inherit_configured_codex_mcp_servers() -> bool {
    std::env::var(CODEX_INHERIT_MCP_SERVERS_ENV)
        .ok()
        .as_deref()
        .map(codex_inherit_mcp_servers_env_value)
        .unwrap_or(false)
}

pub(crate) fn codex_inherit_mcp_servers_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "inherit" | "all"
    )
}

// ---------------------------------------------------------------------------
// CodexAgent
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "initialize".to_string(),
            params: Some(serde_json::json!({"key": "value"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["params"]["key"], "value");
    }

    #[test]
    fn json_rpc_request_no_params() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 2,
            method: "initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn json_rpc_notification_serialization() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&notif).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "initialized");
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn json_rpc_response_serialization() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: 5,
            result: serde_json::json!({"decision": "accept"}),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], 5);
        assert_eq!(parsed["result"]["decision"], "accept");
    }

    #[test]
    fn deserialize_response_message() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(1));
        assert!(msg.method.is_none());
        assert!(msg.result.is_some());
        assert!(msg.error.is_none());
    }

    #[test]
    fn deserialize_error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(2));
        assert!(msg.method.is_none());
        assert!(msg.result.is_none());
        let err = msg.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid request");
    }

    #[test]
    fn deserialize_notification_message() {
        let json =
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"hello"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert!(msg.id.is_none());
        assert_eq!(msg.method.as_deref(), Some("item/agentMessage/delta"));
        assert!(msg.params.is_some());
    }

    #[test]
    fn deserialize_server_request() {
        let json = r#"{"jsonrpc":"2.0","id":99,"method":"item/commandExecution/requestApproval","params":{"item":{"command":"rm -rf /"}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some(99));
        assert_eq!(
            msg.method.as_deref(),
            Some("item/commandExecution/requestApproval")
        );
        assert!(msg.params.is_some());
    }

    #[test]
    fn malformed_json_does_not_panic() {
        // Simulate what happens when the reader encounters bad JSON
        let bad_lines = vec![
            "",
            "not json at all",
            "{malformed",
            r#"{"jsonrpc":"2.0"}"#, // valid JSON but missing fields -- should not panic
        ];
        for line in bad_lines {
            // These should either parse successfully (with missing optional fields)
            // or fail gracefully without panicking
            let _result: Result<JsonRpcMessage, _> = serde_json::from_str(line);
        }
    }

    #[tokio::test]
    async fn interrupt_turn_wire_format_is_jsonrpc_request() {
        // Confirm the shape of the JSON-RPC request we emit matches what Codex
        // v2 expects: {"jsonrpc":"2.0","id":<N>,"method":"turn/interrupt",
        // "params":{"threadId":...,"turnId":...}}
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 42,
            method: "turn/interrupt".to_string(),
            params: Some(serde_json::json!({
                "threadId": "thread-abc",
                "turnId": "turn-xyz",
            })),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["method"], "turn/interrupt");
        assert_eq!(v["params"]["threadId"], "thread-abc");
        assert_eq!(v["params"]["turnId"], "turn-xyz");
    }

    #[test]
    fn steer_turn_wire_format_is_jsonrpc_request() {
        // Verify the params shape matches the spec: threadId + expectedTurnId
        // for the precondition, and input as a singleton content array of
        // type="text". Frozen format — changes here should update the
        // Codex compat docs too.
        let text = "please check tests/e2e/ first";
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "input": [{"type": "text", "text": text}],
            "expectedTurnId": "turn-xyz",
        });
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 99,
            method: "turn/steer".to_string(),
            params: Some(params),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 99);
        assert_eq!(v["method"], "turn/steer");
        assert_eq!(v["params"]["threadId"], "thread-abc");
        assert_eq!(v["params"]["expectedTurnId"], "turn-xyz");
        let input = v["params"]["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], text);
    }

    #[test]
    fn codex_mcp_server_names_from_config_toml_filters_intendant_and_keeps_quoted_keys() {
        let names = codex_mcp_server_names_from_config_toml(
            r#"
[mcp_servers.slack]
command = "slack-mcp"

[mcp_servers."linear.com"]
command = "linear-mcp"

[mcp_servers.intendant]
type = "http"

[mcp_servers.asana-prod]
command = "asana-mcp"
"#,
        );

        assert_eq!(
            names,
            vec![
                "asana-prod".to_string(),
                "linear.com".to_string(),
                "slack".to_string()
            ]
        );
    }

    #[test]
    fn codex_mcp_server_disable_override_quotes_non_bare_keys() {
        let names = vec![
            "asana-prod".to_string(),
            "linear.com".to_string(),
            "gmail workspace".to_string(),
        ];

        assert_eq!(
            codex_mcp_server_disable_override(&names).as_deref(),
            Some(
                "mcp_servers={asana-prod={enabled=false},\"linear.com\"={enabled=false},\"gmail workspace\"={enabled=false}}"
            )
        );
    }

    #[test]
    fn codex_inherit_mcp_servers_env_value_requires_explicit_truthy_value() {
        for value in ["1", "true", "yes", "on", "inherit", "all", " TRUE "] {
            assert!(codex_inherit_mcp_servers_env_value(value), "{value}");
        }
        for value in ["", "0", "false", "no", "off", "intendant"] {
            assert!(!codex_inherit_mcp_servers_env_value(value), "{value}");
        }
    }
}
