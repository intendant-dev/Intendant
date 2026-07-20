//! Kimi Code sign-in ceremony routes (`/api/kimi-auth/*`).
//!
//! These mirror the Codex device-flow routes: the dashboard receives only a
//! validated verification URL, a short user code, and state transitions.
//! The official Kimi CLI owns the token exchange and credential file.

use super::*;
use crate::auth_ceremony::{self, Provider, StartRefusal};
use crate::kimi_auth_ceremony::{self, custody_refusal, SUPPORTED_MODE};

pub(crate) fn kimi_auth_start_api_response(
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
    let command = kimi_auth_ceremony::configured_kimi_command(project_root);
    match kimi_auth_ceremony::start_ceremony(&command, &mode) {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "status": auth_ceremony::manager().status_value_for(Provider::Kimi),
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

fn start_mode_from_body(body_text: &str) -> Result<String, String> {
    let trimmed = body_text.trim();
    if trimmed.is_empty() {
        return Ok(SUPPORTED_MODE.to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|error| format!("invalid JSON body: {error}"))?;
    match value.get("mode") {
        None | Some(serde_json::Value::Null) => Ok(SUPPORTED_MODE.to_string()),
        Some(serde_json::Value::String(mode)) => Ok(mode.trim().to_string()),
        Some(_) => Err("\"mode\" must be a string".to_string()),
    }
}

pub(crate) fn kimi_auth_status_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    ApiResponse::json(
        200,
        JsonBody::Value(auth_ceremony::manager().status_value_for(Provider::Kimi)),
    )
}

pub(crate) fn kimi_auth_cancel_api_response(hosted_provenance: bool) -> ApiResponse {
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

pub(crate) async fn handle_kimi_auth_start(
    stream: DemuxStream,
    body_text: String,
    project_root: Option<PathBuf>,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = kimi_auth_start_api_response(
        request_authority_is_hosted(access),
        &body_text,
        project_root.as_deref(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_kimi_auth_status(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = kimi_auth_status_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_kimi_auth_cancel(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = kimi_auth_cancel_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_status_and_body(response: ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => {
                (status, serde_json::from_str(&body.into_string()).unwrap())
            }
            _ => panic!("kimi-auth responses are JSON"),
        }
    }

    #[test]
    fn hosted_provenance_is_refused_on_every_leaf() {
        for response in [
            kimi_auth_start_api_response(true, "", None),
            kimi_auth_status_api_response(true),
            kimi_auth_cancel_api_response(true),
        ] {
            let (status, body) = response_status_and_body(response);
            assert_eq!(status, 403);
            assert_eq!(
                body["error"],
                "credential ceremonies require a trusted direct connection"
            );
            assert_eq!(body["refusal"], "hosted_provenance");
        }
    }

    #[test]
    fn start_mode_defaults_and_validates_shape() {
        assert_eq!(start_mode_from_body("").unwrap(), SUPPORTED_MODE);
        assert_eq!(start_mode_from_body("{}").unwrap(), SUPPORTED_MODE);
        assert_eq!(
            start_mode_from_body("{\"mode\":\"kimi-code\"}").unwrap(),
            SUPPORTED_MODE
        );
        assert_eq!(
            start_mode_from_body("{\"mode\":\"future\"}").unwrap(),
            "future"
        );
        assert!(start_mode_from_body("{\"mode\":3}").is_err());
        assert!(start_mode_from_body("nope").is_err());
    }

    #[test]
    fn cancel_without_ceremony_is_a_state_conflict() {
        let (status, body) = response_status_and_body(kimi_auth_cancel_api_response(false));
        assert_eq!(status, 409);
        assert!(body["error"].as_str().unwrap().contains("no sign-in"));
    }
}
