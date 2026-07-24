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
