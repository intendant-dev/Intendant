//! Codex sign-in ceremony routes (`/api/codex-auth/*`).
//!
//! Thin transport shims over [`crate::codex_auth_ceremony`], mirroring
//! the Claude family in `routes_claude_auth.rs` (whose hosted-provenance
//! helpers they share): the `*_api_response` cores serve both the HTTP
//! rows and their datachannel twins. Beyond the rows' `credentials.manage`
//! IAM gate, every leaf hard-refuses hosted-provenance clients (defense
//! in depth — a credential ceremony must never ride a rendezvous-mediated
//! transport even if the central gate regressed), and `start` refuses
//! daemons whose Codex/OpenAI credentials are custody-managed off-box.
//! The device flow has no code-submission leaf — the owner types the
//! one-time code into OpenAI's page, never back into the daemon.

use super::*;
use crate::auth_ceremony::{self, Provider, StartRefusal};
use crate::codex_auth_ceremony::{self, custody_refusal, SUPPORTED_MODE};

/// POST /api/codex-auth/start + the tunnel's `api_codex_auth_start`.
/// `hosted_provenance` and `project_root` arrive from the transport edge.
pub(crate) fn codex_auth_start_api_response(
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
    let command = codex_auth_ceremony::configured_codex_command(project_root);
    match codex_auth_ceremony::start_ceremony(&command, &mode) {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "status": auth_ceremony::manager().status_value_for(Provider::Codex),
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

/// The `{"mode": …}` body (empty body = the ChatGPT device default).
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

/// GET /api/codex-auth/status + the tunnel's `api_codex_auth_status`.
pub(crate) fn codex_auth_status_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    ApiResponse::json(
        200,
        JsonBody::Value(auth_ceremony::manager().status_value_for(Provider::Codex)),
    )
}

/// POST /api/codex-auth/cancel + the tunnel's `api_codex_auth_cancel`.
pub(crate) fn codex_auth_cancel_api_response(hosted_provenance: bool) -> ApiResponse {
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

pub(crate) async fn handle_codex_auth_start(
    stream: DemuxStream,
    body_text: String,
    project_root: Option<PathBuf>,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = codex_auth_start_api_response(
        request_authority_is_hosted(access),
        &body_text,
        project_root.as_deref(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_codex_auth_status(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = codex_auth_status_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_codex_auth_cancel(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = codex_auth_cancel_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_status_and_body(response: ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => {
                let text = body.into_string();
                (status, serde_json::from_str(&text).unwrap())
            }
            _ => panic!("codex-auth responses are JSON"),
        }
    }

    #[test]
    fn hosted_provenance_is_refused_on_every_leaf() {
        for response in [
            codex_auth_start_api_response(true, "", None),
            codex_auth_status_api_response(true),
            codex_auth_cancel_api_response(true),
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
    fn start_mode_parsing_defaults_to_chatgpt_device() {
        assert_eq!(start_mode_from_body("").unwrap(), SUPPORTED_MODE);
        assert_eq!(start_mode_from_body("{}").unwrap(), SUPPORTED_MODE);
        assert_eq!(
            start_mode_from_body("{\"mode\":\"chatgpt\"}").unwrap(),
            "chatgpt"
        );
        assert_eq!(
            start_mode_from_body("{\"mode\":\"api_key\"}").unwrap(),
            "api_key"
        );
        assert!(start_mode_from_body("{\"mode\":3}").is_err());
        assert!(start_mode_from_body("nope").is_err());
    }

    #[test]
    fn cancel_without_ceremony_is_a_state_conflict() {
        let (status, body) = response_status_and_body(codex_auth_cancel_api_response(false));
        assert_eq!(status, 409);
        assert!(body["error"].as_str().unwrap().contains("no sign-in"));
    }
}
