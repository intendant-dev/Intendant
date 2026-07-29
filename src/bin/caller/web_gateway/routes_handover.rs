//! The daemon-handover HTTP surface (Track HS3): `POST
//! /api/daemon/takeover` — ask THIS daemon to drain so a successor can
//! acquire the scheduler lease. Rides the loopback + per-port
//! admission-token same-user trust class under the route table's IAM
//! gate (owner-grade `settings` operation; hosted `role:none` can never
//! reach it — the Q2 ruling pins). Drain is one-way and idempotent; the
//! flock release itself happens on the scheduler's next wake (notified
//! here — milliseconds), so a firing pass can never straddle it.

use super::*;

/// Transport-neutral core of `POST /api/daemon/takeover`. Body (JSON,
/// optional): `{"requested_by": "<display label>"}` — display currency
/// for logs/status, never authority (the IAM gate already bound the
/// caller).
pub(crate) async fn daemon_takeover_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let runtime = match mcp_server {
        Some(server) => server.handover_runtime().await,
        None => None,
    };
    let Some(runtime) = runtime else {
        return ApiResponse::json_error(503, "handover unavailable on this daemon");
    };
    let requested_by = serde_json::from_str::<serde_json::Value>(body_text)
        .ok()
        .and_then(|body| body.get("requested_by")?.as_str().map(str::to_string));
    match runtime.request_drain(requested_by) {
        crate::handover::DrainRequest::Entered => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "status": "draining",
                "boot_id": runtime.boot_id(),
            })),
        ),
        crate::handover::DrainRequest::AlreadyDraining => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "status": "draining",
                "boot_id": runtime.boot_id(),
                "already_draining": true,
            })),
        ),
        crate::handover::DrainRequest::NotHolder => {
            let holder = crate::handover::read_lease_sidecar(runtime.state_root());
            ApiResponse::json(
                409,
                JsonBody::Value(serde_json::json!({
                    "error": "not_holder",
                    "detail": "this daemon does not hold the scheduler lease — \
                               nothing to take over",
                    "holder": holder,
                })),
            )
        }
    }
}

/// `POST /api/daemon/takeover` — the HTTP wrapper.
pub(crate) async fn handle_daemon_takeover(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = daemon_takeover_api_response(&body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/daemon/handover` (tunnel twin
/// `api_daemon_handover`): the scheduler-lease status block — this
/// boot's role, drain state, the on-disk sidecar, and every co-homed
/// boot with probed liveness. The dashboard's drain banner and
/// successor chip poll it (Track HS5).
pub(crate) async fn daemon_handover_status_api_response(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let runtime = match mcp_server {
        Some(server) => server.handover_runtime().await,
        None => None,
    };
    match runtime {
        Some(runtime) => ApiResponse::json(200, JsonBody::Value(runtime.status_json())),
        None => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "available": false })),
        ),
    }
}

/// `GET /api/daemon/handover` — the HTTP wrapper.
pub(crate) async fn handle_daemon_handover_status(
    stream: DemuxStream,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = daemon_handover_status_api_response(mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}
