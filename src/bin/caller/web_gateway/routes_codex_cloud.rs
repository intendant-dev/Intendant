//! Codex Cloud worker leases over HTTP: the dashboard's read of the lease
//! store, plus an optional provider re-sync (`?refresh=1`) through the
//! daemon host's authenticated Codex CLI — the same lane as the
//! `codex-cloud` CLI namespace. Terminal transitions observed by a re-sync
//! park agenda notes before the response is written.

use super::*;

/// Transport-neutral core of `GET /api/codex-cloud/workers` (tunnel twin
/// `api_codex_cloud_workers`). The cached store always answers; a refresh
/// failure (Codex CLI missing, not authenticated) degrades to the cached
/// view with `refresh_error` set instead of failing the request.
pub(crate) async fn codex_cloud_workers_api_response(
    refresh: bool,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let store_path = crate::codex_cloud::state_path();
    let mut refresh_error: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut transitions = serde_json::Value::Array(Vec::new());
    let mut agenda_parked = 0usize;
    if refresh {
        match crate::codex_cloud::refresh_leases(&store_path, None, 20, None).await {
            Ok(outcome) => {
                if let Some(server) = mcp_server {
                    agenda_parked = server
                        .park_codex_cloud_transitions(&outcome.transitions)
                        .await;
                }
                cursor = outcome.cursor;
                transitions = serde_json::to_value(&outcome.transitions)
                    .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
            }
            Err(error) => refresh_error = Some(error),
        }
    }
    match crate::codex_cloud::cached_leases(&store_path) {
        Ok(workers) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "workers": crate::codex_cloud::leases_json(&workers),
                "refreshed": refresh && refresh_error.is_none(),
                "refresh_error": refresh_error,
                "cursor": cursor,
                "transitions": transitions,
                "agenda_parked": agenda_parked,
            })),
        ),
        Err(error) => ApiResponse::json_error(500, &error),
    }
}

pub(crate) async fn handle_codex_cloud_workers(
    stream: DemuxStream,
    request_line: &str,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let refresh = matches!(
        query_param(request_line, "refresh").as_deref(),
        Some("1") | Some("true")
    );
    let response = codex_cloud_workers_api_response(refresh, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Body of `POST /api/codex-cloud/submit`, mirroring the
/// `submit_codex_cloud_task` MCP tool's parameter names (and the
/// `codex-cloud exec` flags they wrap).
fn parse_codex_cloud_submit_body(
    body: &str,
) -> Result<crate::codex_cloud::SubmitTaskRequest, String> {
    #[derive(serde::Deserialize)]
    struct SubmitBody {
        environment_id: String,
        prompt: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        attempts: Option<u16>,
        #[serde(default)]
        title: Option<String>,
    }
    let parsed: SubmitBody =
        serde_json::from_str(body).map_err(|error| format!("invalid submit request: {error}"))?;
    let environment = parsed.environment_id.trim().to_string();
    if environment.is_empty() {
        return Err("environment_id cannot be empty".to_string());
    }
    let prompt = parsed.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("prompt cannot be empty".to_string());
    }
    let attempts = parsed.attempts.unwrap_or(1);
    if attempts == 0 {
        return Err("attempts must be a positive integer".to_string());
    }
    Ok(crate::codex_cloud::SubmitTaskRequest {
        environment,
        branch: parsed
            .branch
            .map(|branch| branch.trim().to_string())
            .filter(|branch| !branch.is_empty()),
        attempts,
        title: parsed
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty()),
        prompt,
        probe: false,
    })
}

/// Transport-neutral core of `POST /api/codex-cloud/submit` (tunnel twin
/// `api_codex_cloud_submit`): the dashboard's submit affordance, riding
/// the same submission lane as `intendant codex-cloud exec` and the
/// `submit_codex_cloud_task` MCP tool — `codex_cloud::submit_task`
/// through the daemon host's authenticated Codex CLI. A successful
/// submit records the worker lease in the store before responding, so
/// the next workers read lists the task without waiting for a provider
/// sync. Request problems are 400s; a Codex CLI failure (missing binary,
/// not authenticated, provider rejection) is a 502 carrying the lane's
/// error string verbatim.
pub(crate) async fn codex_cloud_submit_api_response(body: &str) -> ApiResponse {
    let request = match parse_codex_cloud_submit_body(body) {
        Ok(request) => request,
        Err(error) => return ApiResponse::json_error(400, error),
    };
    match crate::codex_cloud::submit_task(&crate::codex_cloud::state_path(), request).await {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
            Err(error) => ApiResponse::json_error(500, format!("serialize submit result: {error}")),
        },
        Err(error) => ApiResponse::json_error(502, &error),
    }
}

pub(crate) async fn handle_codex_cloud_submit(
    stream: DemuxStream,
    body: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = codex_cloud_submit_api_response(&body).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Method-exact membership test for the certless carve-out
/// ([`super::access_gates::allows_remote_certless_http`]): a Codex Cloud
/// worker redeeming its enrollment token has no client certificate yet —
/// the single-use token in the body is the entire authorization, so this
/// one POST sits in the doorbell class beside peer pairing and org
/// grants.
pub(crate) fn is_public_codex_cloud_enroll_path(request_line: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    method == "POST" && path == crate::codex_cloud_attach::ENROLL_PATH
}

/// The attachment broker's public redemption doorbell
/// (`POST /api/codex-cloud/enroll`): the single-use minted token is the
/// entire authorization. Refusals are uniform (an unknown, used, or
/// expired token reads identically) and the store I/O runs off the
/// reactor thread.
pub(crate) async fn handle_codex_cloud_enroll(
    stream: DemuxStream,
    body: String,
    cert_dir: PathBuf,
    source_hint: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = codex_cloud_enroll_response(body, cert_dir, source_hint).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

async fn codex_cloud_enroll_response(
    body: String,
    cert_dir: PathBuf,
    source_hint: String,
) -> ApiResponse {
    // Parse before counting: unparseable spray never consumes the
    // allowance, so it cannot starve well-formed redemptions.
    let request: crate::codex_cloud_attach::EnrollRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid enrollment request: {error}"))
        }
    };
    // The listener's per-client source bucket, like the pairing doorbell:
    // the peer IP on direct ingress, the relay-preamble bucket on
    // reachability-relay ingress — never the relay's own dial-back
    // address, which would fold every relayed worker into one queue.
    if !crate::codex_cloud_attach::enroll_rate_ok(&source_hint, crate::codex_cloud::now_unix_ms()) {
        return ApiResponse::json_error(429, "enrollment is rate limited; retry shortly");
    }
    let outcome = tokio::task::spawn_blocking(move || {
        crate::codex_cloud_attach::redeem_enrollment(
            &cert_dir,
            &crate::codex_cloud::state_path(),
            &request,
            crate::codex_cloud::now_unix_ms(),
        )
    })
    .await;
    match outcome {
        Ok(Ok(enrolled)) => match serde_json::to_value(&enrolled) {
            Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
            Err(error) => ApiResponse::json_error(500, format!("serialize enrollment: {error}")),
        },
        Ok(Err(error)) => {
            // Uniform refusal class: token problems and validation
            // problems both land 403 without distinguishing detail beyond
            // the message the broker chose to surface.
            ApiResponse::json_error(403, &error)
        }
        Err(error) => ApiResponse::json_error(500, format!("enrollment task: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_codex_cloud_submit_body;

    #[test]
    fn submit_body_defaults_mirror_the_exec_verb() {
        let request = parse_codex_cloud_submit_body(
            r#"{"environment_id":" env-1 ","prompt":"  do the thing  "}"#,
        )
        .expect("minimal body parses");
        assert_eq!(request.environment, "env-1");
        assert_eq!(request.prompt, "do the thing");
        assert_eq!(request.attempts, 1);
        assert_eq!(request.branch, None);
        assert_eq!(request.title, None);
        assert!(!request.probe);
    }

    #[test]
    fn submit_body_carries_the_optional_exec_flags() {
        let request = parse_codex_cloud_submit_body(
            r#"{"environment_id":"env-1","prompt":"p","branch":" main ","attempts":3,"title":" T "}"#,
        )
        .expect("full body parses");
        assert_eq!(request.branch.as_deref(), Some("main"));
        assert_eq!(request.attempts, 3);
        assert_eq!(request.title.as_deref(), Some("T"));
    }

    #[test]
    fn submit_body_rejections_are_request_problems() {
        assert!(parse_codex_cloud_submit_body("not json").is_err());
        assert!(parse_codex_cloud_submit_body(r#"{"environment_id":"","prompt":"p"}"#).is_err());
        assert!(parse_codex_cloud_submit_body(r#"{"environment_id":"e","prompt":"  "}"#).is_err());
        assert!(
            parse_codex_cloud_submit_body(r#"{"environment_id":"e","prompt":"p","attempts":0}"#)
                .is_err()
        );
        // Whitespace-only optionals normalize to absent, not empty flags.
        let request = parse_codex_cloud_submit_body(
            r#"{"environment_id":"e","prompt":"p","branch":"  ","title":""}"#,
        )
        .expect("blank optionals parse");
        assert_eq!(request.branch, None);
        assert_eq!(request.title, None);
    }
}
