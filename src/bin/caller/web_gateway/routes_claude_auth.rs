//! Claude sign-in ceremony routes (`/api/claude-auth/*`).
//!
//! Thin transport shims over [`crate::claude_auth_ceremony`]: the shared
//! `*_api_response` cores serve both the HTTP rows and their datachannel
//! twins. Beyond the rows' `credentials.manage` IAM gate, every leaf
//! hard-refuses hosted-provenance clients (defense in depth — the central
//! evaluator already denies hosted sessions, but a credential ceremony
//! must never ride a rendezvous-mediated transport even if that gate
//! regressed), and `start` refuses daemons whose Claude/Anthropic
//! credentials are custody-managed off-box ([`custody_refusal`]).

use super::*;
use crate::auth_ceremony::{self, CodeRefusal, Provider, StartRefusal};
use crate::claude_auth_ceremony::{self, custody_refusal, SUPPORTED_MODE};

/// Mandated refusal copy for rendezvous-mediated clients (shared with
/// the Codex ceremony routes).
pub(crate) const HOSTED_REFUSAL: &str =
    "credential ceremonies require a trusted direct connection";

/// True when this request's session has hosted provenance
/// ([`crate::access::iam::is_hosted_session`] when the IAM snapshot is on
/// hand; the principal-borne facts otherwise).
pub(crate) fn request_authority_is_hosted(access: &RequestAuthority) -> bool {
    match access.iam_state.as_ref() {
        Some(state) => crate::access::iam::is_hosted_session(state, &access.principal),
        None => {
            access.principal.hosted_connect
                || access.principal.authn_kind.as_deref() == Some("connect_account")
        }
    }
}

pub(crate) fn hosted_refusal_response() -> ApiResponse {
    ApiResponse::json(
        403,
        JsonBody::Value(serde_json::json!({
            "error": HOSTED_REFUSAL,
            "refusal": "hosted_provenance",
        })),
    )
}

/// POST /api/claude-auth/start + the tunnel's `api_claude_auth_start`.
/// `hosted_provenance` and `project_root` arrive from the transport edge.
pub(crate) fn claude_auth_start_api_response(
    hosted_provenance: bool,
    body_text: &str,
    project_root: Option<&std::path::Path>,
) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    if let Some(refusal) = custody_refusal() {
        return ApiResponse::json(
            403,
            JsonBody::Value(serde_json::json!({
                "error": refusal,
                "refusal": "custody",
            })),
        );
    }
    let mode = match start_mode_from_body(body_text) {
        Ok(mode) => mode,
        Err(error) => return ApiResponse::json_error(400, error),
    };
    let command = claude_auth_ceremony::configured_claude_command(project_root);
    match claude_auth_ceremony::start_ceremony(&command, &mode) {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "status": auth_ceremony::manager().status_value_for(Provider::Claude),
            })),
        ),
        Err(StartRefusal::Busy) => {
            ApiResponse::json_error(409, "a sign-in ceremony is already running on this daemon")
        }
        Err(StartRefusal::BadRequest(error)) => ApiResponse::json_error(400, error),
        Err(StartRefusal::Spawn(error)) => {
            ApiResponse::json_error(500, format!("could not start the sign-in process: {error}"))
        }
    }
}

/// The `{"mode": …}` body (empty body = the claude.ai default).
fn start_mode_from_body(body_text: &str) -> Result<String, String> {
    let trimmed = body_text.trim();
    if trimmed.is_empty() {
        return Ok(SUPPORTED_MODE.to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON body: {e}"))?;
    match value.get("mode") {
        None | Some(serde_json::Value::Null) => Ok(SUPPORTED_MODE.to_string()),
        Some(serde_json::Value::String(mode)) => Ok(mode.trim().to_string()),
        Some(_) => Err("\"mode\" must be a string".to_string()),
    }
}

/// GET /api/claude-auth/status + the tunnel's `api_claude_auth_status`.
pub(crate) fn claude_auth_status_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    ApiResponse::json(
        200,
        JsonBody::Value(auth_ceremony::manager().status_value_for(Provider::Claude)),
    )
}

/// POST /api/claude-auth/code + the tunnel's `api_claude_auth_code`.
pub(crate) fn claude_auth_code_api_response(
    hosted_provenance: bool,
    body_text: &str,
) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    let code = match serde_json::from_str::<serde_json::Value>(body_text.trim()) {
        Ok(value) => match value.get("code").and_then(|v| v.as_str()) {
            Some(code) => code.to_string(),
            None => return ApiResponse::json_error(400, "body must carry a \"code\" string"),
        },
        Err(e) => return ApiResponse::json_error(400, format!("invalid JSON body: {e}")),
    };
    match auth_ceremony::manager().submit_code(&code) {
        Ok(phase) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "phase": phase.as_str(),
            })),
        ),
        Err(CodeRefusal::Invalid(error)) => ApiResponse::json_error(400, error),
        Err(CodeRefusal::State(error)) => ApiResponse::json_error(409, error),
    }
}

/// POST /api/claude-auth/cancel + the tunnel's `api_claude_auth_cancel`.
pub(crate) fn claude_auth_cancel_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    match auth_ceremony::manager().cancel() {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "phase": "cancelled",
            })),
        ),
        Err(error) => ApiResponse::json_error(409, error),
    }
}

pub(crate) async fn handle_claude_auth_start(
    stream: DemuxStream,
    body_text: String,
    project_root: Option<PathBuf>,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_start_api_response(
        request_authority_is_hosted(access),
        &body_text,
        project_root.as_deref(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_status(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_status_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_code(
    stream: DemuxStream,
    body_text: String,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_code_api_response(request_authority_is_hosted(access), &body_text);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_cancel(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_cancel_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_status_and_body(response: ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => {
                let text = match body {
                    JsonBody::Value(value) => value.to_string(),
                    JsonBody::PreSerialized(text) => text,
                };
                (status, serde_json::from_str(&text).unwrap())
            }
            _ => panic!("claude-auth responses are JSON"),
        }
    }

    fn hosted_principal() -> crate::access::iam::AccessPrincipal {
        let mut principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test");
        principal.hosted_connect = true;
        principal
    }

    #[test]
    fn hosted_provenance_is_refused_on_every_leaf() {
        for response in [
            claude_auth_start_api_response(true, "", None),
            claude_auth_status_api_response(true),
            claude_auth_code_api_response(true, "{\"code\":\"x\"}"),
            claude_auth_cancel_api_response(true),
        ] {
            let (status, body) = response_status_and_body(response);
            assert_eq!(status, 403);
            assert_eq!(
                body["error"].as_str().unwrap(),
                "credential ceremonies require a trusted direct connection"
            );
            assert_eq!(body["refusal"], "hosted_provenance");
        }
    }

    #[test]
    fn request_authority_hosted_detection_uses_principal_facts_without_state() {
        let hosted = RequestAuthority {
            principal: hosted_principal(),
            iam_state: None,
        };
        assert!(request_authority_is_hosted(&hosted));
        let direct = RequestAuthority {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test"),
            iam_state: None,
        };
        assert!(!request_authority_is_hosted(&direct));
        // With a state snapshot the central evaluator's provenance rules
        // decide — hosted_connect stays authoritative through it.
        let hosted_with_state = RequestAuthority {
            principal: hosted_principal(),
            iam_state: Some(std::sync::Arc::new(
                crate::access::iam::LocalIamState::default(),
            )),
        };
        assert!(request_authority_is_hosted(&hosted_with_state));
    }

    #[test]
    fn code_body_shapes_are_validated() {
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "not json"));
        assert_eq!(status, 400);
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "{\"nope\":1}"));
        assert_eq!(status, 400);
        // A well-formed body with no live ceremony is a state conflict.
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "{\"code\":\"abc\"}"));
        assert_eq!(status, 409);
    }

    #[test]
    fn start_mode_parsing_defaults_to_claudeai() {
        assert_eq!(start_mode_from_body("").unwrap(), SUPPORTED_MODE);
        assert_eq!(start_mode_from_body("{}").unwrap(), SUPPORTED_MODE);
        assert_eq!(
            start_mode_from_body("{\"mode\":\"claudeai\"}").unwrap(),
            "claudeai"
        );
        assert_eq!(
            start_mode_from_body("{\"mode\":\"console\"}").unwrap(),
            "console"
        );
        assert!(start_mode_from_body("{\"mode\":3}").is_err());
        assert!(start_mode_from_body("nope").is_err());
    }

    #[test]
    fn cancel_without_ceremony_is_a_state_conflict() {
        let (status, body) = response_status_and_body(claude_auth_cancel_api_response(false));
        assert_eq!(status, 409);
        assert!(body["error"].as_str().unwrap().contains("no sign-in"));
    }
}
