//! The Memory service's HTTP surface: `GET /api/memory/search`,
//! `GET /api/memory/claim`, and `POST /api/memory/propose`, plus the
//! transport-neutral cores their dashboard-control tunnel twins reuse.
//! The IAM gate (`memory.read` / `memory.write`) runs pre-dispatch off
//! the route rows; writes funnel through the daemon's single-writer
//! [`crate::memory::MemoryHandle`]. The plane is EPHEMERAL (the
//! ratified P1 write bar): every view and response says so, and kernel
//! rejections surface the reducer's named outcome/disposition verbatim
//! in the error body (D-203 §C.2).

use super::*;

/// Transport-neutral core of `GET /api/memory/search` (tunnel twin
/// `api_memory_search`): bounded lexical search; candidates excluded
/// unless opted in; every result carries its derived status.
pub(crate) async fn memory_search_api_response(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
    args: &crate::memory::SearchArgs,
) -> ApiResponse {
    let Some(memory) = memory_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "memory service unavailable on this daemon");
    };
    let results = memory.search(args);
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({
            "results": results,
            "durability": "ephemeral",
        })),
    )
}

/// Transport-neutral core of `GET /api/memory/claim` (tunnel twin
/// `api_memory_claim`): read one claim by id prefix (≥ 8 hex chars).
pub(crate) async fn memory_claim_api_response(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
    id_prefix: &str,
) -> ApiResponse {
    let Some(memory) = memory_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "memory service unavailable on this daemon");
    };
    match memory.read(id_prefix) {
        Ok(claim) => ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "claim": claim }))),
        Err(err) => ApiResponse::json_error(memory_error_status(&err), err.to_string()),
    }
}

/// Transport-neutral core of `POST /api/memory/propose` (tunnel twin
/// `api_memory_propose`): the body is one [`crate::memory::ProposeArgs`]
/// JSON object; success returns the claim view (status `candidate`).
pub(crate) async fn memory_propose_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(memory) = memory_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "memory service unavailable on this daemon");
    };
    let args: crate::memory::ProposeArgs = match serde_json::from_str(body_text) {
        Ok(args) => args,
        Err(err) => {
            return ApiResponse::json_error(400, format!("invalid memory proposal: {err}"));
        }
    };
    match memory.propose(args) {
        Ok(claim) => ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "claim": claim }))),
        Err(err) => ApiResponse::json_error(memory_error_status(&err), err.to_string()),
    }
}

async fn memory_handle(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> Option<Arc<crate::memory::MemoryHandle>> {
    match mcp_server {
        Some(server) => server.memory_handle().await,
        None => None,
    }
}

/// Kernel rejections/pends are semantic verdicts (422), not malformed
/// requests (400); the named outcome/disposition rides the message
/// verbatim. `Unimplemented` is a kernel-boundary bug report (500).
fn memory_error_status(err: &crate::memory::MemoryError) -> u16 {
    use crate::memory::MemoryError as E;
    match err {
        E::NotFound(_) => 404,
        E::Ambiguous(..) | E::Vocabulary { .. } | E::InvalidArg(_) => 400,
        E::Rejected { .. } | E::Pending { .. } => 422,
        E::Unimplemented(_) => 500,
    }
}

pub(crate) async fn handle_memory_search(
    stream: DemuxStream,
    request_line: &str,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let args = crate::memory::SearchArgs {
        query: query_param(request_line, "q").unwrap_or_default(),
        limit: query_param(request_line, "limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        include_candidates: matches!(
            query_param(request_line, "candidates").as_deref(),
            Some("1") | Some("true")
        ),
    };
    let response = memory_search_api_response(mcp_server.as_ref(), &args).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_memory_claim(
    stream: DemuxStream,
    request_line: &str,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match query_param(request_line, "id") {
        Some(id) if !id.is_empty() => memory_claim_api_response(mcp_server.as_ref(), &id).await,
        _ => ApiResponse::json_error(400, "missing id query parameter"),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_memory_propose(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = memory_propose_api_response(&body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}
