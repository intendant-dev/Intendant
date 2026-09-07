//! Skills-aware wrapper around the established `/mcp` HTTP gate.
//!
//! The existing tools transport remains byte-for-byte in `mcp_gate/legacy.rs`.
//! This thin front door handles the Skills Over MCP extension methods and an
//! extension-advertising initialize response, then delegates every other
//! request to the unchanged gate. Keeping the skill projection here avoids
//! teaching the legacy tools path about a draft extension while preserving its
//! authorization, SSE, keep-alive, and refusal behavior exactly.

use super::*;

mod legacy;
#[allow(hidden_glob_reexports)]
pub(crate) use legacy::*;

#[cfg(test)]
const SKILLS_EXTENSION: &str = "io.modelcontextprotocol/skills";

fn skills_extension_method(body: &str) -> Option<&str> {
    let request = serde_json::from_str::<serde_json::Value>(body).ok()?;
    match request.get("method").and_then(serde_json::Value::as_str)? {
        "initialize" => Some("initialize"),
        "skills/list" => Some("skills/list"),
        "skills/get" => Some("skills/get"),
        "resources/read" => Some("resources/read"),
        _ => None,
    }
}

fn skill_profile_from_request_line(request_line: &str) -> Option<String> {
    query_param(request_line, "skill_profile")
        .map(|profile| profile.trim().to_ascii_lowercase())
        .filter(|profile| !profile.is_empty())
}

fn skills_initialize_result(requested: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": legacy::negotiated_mcp_protocol_version(requested),
        "capabilities": {
            "tools": {},
            "extensions": {
                "io.modelcontextprotocol/skills": {}
            }
        },
        "serverInfo": {
            "name": "intendant",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "This server supplies the live effective Intendant skill catalog through the Skills Over MCP extension. Supporting clients should make skill name and description metadata ambient and load the selected SKILL.md lazily. Use skill_profile=openai to fold the complete catalog into one imported package when the client imposes a low named-skill limit; no logical skill is silently truncated."
    })
}

fn request_id(request: &serde_json::Value) -> Option<serde_json::Value> {
    request.get("id").cloned().filter(|id| !id.is_null())
}

fn json_rpc_response(
    id: serde_json::Value,
    result: Result<serde_json::Value, (i64, String)>,
) -> String {
    match result {
        Ok(result) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string(),
        Err((code, message)) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
        .to_string(),
    }
}

async fn write_skills_response(mut stream: DemuxStream, status: &str, body: String, cors: &str) {
    use tokio::io::AsyncWriteExt;

    let reuse = stream.exchange_reusable();
    let response = HttpResponse::with_content(status, "application/json", body)
        .header_segment(cors)
        .header("Cache-Control", "no-cache")
        .connection_reuse(reuse)
        .into_string();
    let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
    if reuse && write_ok {
        stream.park().await;
    } else {
        finalize_http_stream(&mut stream).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_mcp_post(
    stream: DemuxStream,
    body_text: String,
    header_text: &str,
    request_line: &str,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    is_tls: bool,
    tls_client_cert_present: bool,
    tls_client_cert_fingerprint: Option<String>,
    peer_addr: std::net::SocketAddr,
    bus: EventBus,
    session_token_only: bool,
) {
    let Some(method) = skills_extension_method(&body_text) else {
        return legacy::handle_mcp_post(
            stream,
            body_text,
            header_text,
            request_line,
            peer_connection_identity,
            mcp_server,
            is_tls,
            tls_client_cert_present,
            tls_client_cert_fingerprint,
            peer_addr,
            bus,
            session_token_only,
        )
        .await;
    };

    let Some(mcp) = mcp_server else {
        return legacy::handle_mcp_post(
            stream,
            body_text,
            header_text,
            request_line,
            peer_connection_identity,
            None,
            is_tls,
            tls_client_cert_present,
            tls_client_cert_fingerprint,
            peer_addr,
            bus,
            session_token_only,
        )
        .await;
    };

    let cors = legacy::mcp_cors_header_segment(header_text, is_tls);
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let access = match if session_token_only {
        legacy::session_only_mcp_access_context(&cert_dir, header_text)
    } else {
        legacy::mcp_http_access_context(
            &cert_dir,
            peer_connection_identity.as_ref(),
            tls_client_cert_fingerprint.as_deref(),
            tls_client_cert_present,
            is_tls,
            peer_addr,
            header_text,
        )
    } {
        Ok(access) => access,
        Err((status, message)) => {
            let reason = match status {
                401 => "Unauthorized",
                403 => "Forbidden",
                _ => "Error",
            };
            let body = json_rpc_response(serde_json::Value::Null, Err((-32600, message)));
            return write_skills_response(stream, &format!("{status} {reason}"), body, &cors).await;
        }
    };

    let request = match serde_json::from_str::<serde_json::Value>(&body_text) {
        Ok(request) => request,
        Err(_) => {
            // The selector only admits successfully parsed bodies, but keep
            // the established parser as the single source of parse-error text
            // if that invariant ever changes.
            return legacy::handle_mcp_post(
                stream,
                body_text,
                header_text,
                request_line,
                peer_connection_identity,
                Some(mcp),
                is_tls,
                tls_client_cert_present,
                tls_client_cert_fingerprint,
                peer_addr,
                bus,
                session_token_only,
            )
            .await;
        }
    };
    let Some(id) = request_id(&request) else {
        return write_skills_response(stream, "202 Accepted", String::new(), &cors).await;
    };
    let params = request
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let profile = skill_profile_from_request_line(request_line);

    let result = if method == "initialize" {
        if let Some(session_id) = legacy::mcp_gate_session(header_text) {
            legacy::note_supervised_mcp_serve(
                &bus,
                &session_id,
                legacy::McpServeMilestone::Initialize,
            );
        }
        let requested = params
            .get("protocolVersion")
            .and_then(serde_json::Value::as_str);
        Ok(skills_initialize_result(requested))
    } else {
        let decision = access.decision(crate::peer::access_policy::PeerOperation::StatsRead);
        if !decision.allowed {
            Err((
                -32003,
                format!(
                    "Permission denied for {method}: {} (principal {}, permission {}).",
                    decision.reason, access.principal.id, decision.permission
                ),
            ))
        } else {
            let call = match method {
                "skills/list" => mcp.skills_over_mcp_list(&params, profile.as_deref()),
                "skills/get" => mcp.skills_over_mcp_get(&params, profile.as_deref()),
                "resources/read" => mcp.skills_over_mcp_read_resource(&params, profile.as_deref()),
                _ => unreachable!("selector and dispatch must match"),
            };
            call.map_err(|message| (-32602, message))
        }
    };

    write_skills_response(stream, "200 OK", json_rpc_response(id, result), &cors).await;
}

#[cfg(test)]
mod skills_extension_tests {
    use super::*;

    #[test]
    fn selector_intercepts_only_the_extension_surface_and_initialize() {
        for method in ["initialize", "skills/list", "skills/get", "resources/read"] {
            let body =
                serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method }).to_string();
            assert_eq!(skills_extension_method(&body), Some(method));
        }
        let tools = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list"
        })
        .to_string();
        assert_eq!(skills_extension_method(&tools), None);
        assert_eq!(skills_extension_method("not json"), None);
    }

    #[test]
    fn initialize_advertises_only_implemented_capabilities() {
        let result = skills_initialize_result(Some("2025-06-18"));
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"].get("resources").is_none());
        assert!(result["capabilities"]["extensions"][SKILLS_EXTENSION].is_object());
        assert!(result["instructions"]
            .as_str()
            .unwrap()
            .contains("skill_profile=openai"));
    }

    #[test]
    fn openai_skill_profile_is_explicit_and_query_scoped() {
        assert_eq!(
            skill_profile_from_request_line(
                "POST /mcp?tool_profile=facade&skill_profile=openai HTTP/1.1"
            )
            .as_deref(),
            Some("openai")
        );
        assert_eq!(
            skill_profile_from_request_line("POST /mcp?tool_profile=facade HTTP/1.1"),
            None
        );
    }
}
