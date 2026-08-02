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

/// Transport-neutral core of the two self-update-lane actions (`POST
/// /api/daemon/update-lane/{check,produce}`). The optional body names
/// the channel (`{"channel": "releases"|"dev"}`; absent = the install's
/// native lane). Check runs that channel's bounded compare; produce is
/// the owner's consent click — it starts the supervised produce job (or
/// refuses honestly: a channel this install cannot use, a job already
/// running, an unsupported platform). Both answer the lane's fresh
/// status block so the panel renders truth immediately.
pub(crate) async fn daemon_update_lane_api_response(
    produce: bool,
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let runtime = match mcp_server {
        Some(server) => server.handover_runtime().await,
        None => None,
    };
    let lane = runtime.as_ref().and_then(|runtime| runtime.update_lane());
    let Some(lane) = lane else {
        return ApiResponse::json_error(503, "the self-update lane is not wired on this daemon");
    };
    let channel = match crate::handover::parse_channel_arg(body_text) {
        Ok(channel) => channel,
        Err(refusal) => return ApiResponse::json_error(400, &refusal),
    };
    let outcome = if produce {
        lane.request_produce(channel)
    } else {
        lane.request_check(channel)
    };
    match outcome {
        Ok(block) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "started": true, "update_lane": block })),
        ),
        Err(refusal) => ApiResponse::json(
            409,
            JsonBody::Value(serde_json::json!({
                "error": "update_lane_refused",
                "detail": refusal,
                "update_lane": lane.status_block(),
            })),
        ),
    }
}

/// `POST /api/daemon/update-lane/{check,produce}` — the HTTP wrappers.
pub(crate) async fn handle_daemon_update_lane(
    stream: DemuxStream,
    produce: bool,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = daemon_update_lane_api_response(produce, &body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/daemon/successor-exec` (ruled
/// 2026-07-31; NO tunnel twin — ruling binding 4): on a CLI-launched
/// daemon the owner's explicit click spawns the verified on-disk build
/// as a successor secondary, confirms readiness, then drains toward
/// it. Body (JSON): `{"expected_git_sha": "<offered build>",
/// "requested_by": "<display label>"}` — the sha is REQUIRED (the exec
/// target is pinned by path AND hash; a click that cannot name the
/// build it offered does not run), the label is display currency.
pub(crate) async fn daemon_successor_exec_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let runtime = match mcp_server {
        Some(server) => server.handover_runtime().await,
        None => None,
    };
    let lane = runtime
        .as_ref()
        .and_then(|runtime| runtime.successor_exec_lane());
    let Some(lane) = lane else {
        return ApiResponse::json_error(503, "the successor-exec lane is not wired on this daemon");
    };
    let body = serde_json::from_str::<serde_json::Value>(body_text).unwrap_or_default();
    let Some(expected) = body
        .get("expected_git_sha")
        .and_then(|value| value.as_str())
    else {
        return ApiResponse::json_error(
            400,
            "the body must name the offered build: {\"expected_git_sha\": …} — the spawn \
             targets an exact verified artifact, never \"whatever is on disk\"",
        );
    };
    let requested_by = body
        .get("requested_by")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    match lane.request_spawn(expected, requested_by) {
        Ok(block) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "started": true, "successor_exec": block })),
        ),
        Err(refusal) => ApiResponse::json(
            409,
            JsonBody::Value(serde_json::json!({
                "error": "successor_exec_refused",
                "detail": refusal,
                "successor_exec": lane.status_block(),
            })),
        ),
    }
}

/// `POST /api/daemon/successor-exec` — the HTTP wrapper.
pub(crate) async fn handle_daemon_successor_exec(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = daemon_successor_exec_api_response(&body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// The one-click swap relay's three actions: a dashboard surface asks
/// (`request`), the app supervisor's health tick claims (`claim`), and
/// the supervisor reports the attempt's outcome (`result`). The daemon
/// only parks and serves the request — the supervisor performs the
/// swap; on an app-supervised daemon the daemon still never execs a
/// successor (the ruled successor-exec route above is the UNSUPERVISED
/// counterpart, and refuses while a supervisor is attached).
pub(crate) enum UpdateSwapAction {
    Request,
    Claim,
    Result,
}

/// Transport-neutral core of `POST /api/daemon/update-swap[/claim|/result]`.
pub(crate) async fn daemon_update_swap_api_response(
    action: UpdateSwapAction,
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
    let body = serde_json::from_str::<serde_json::Value>(body_text).unwrap_or_default();
    let now_ms = crate::handover::now_ms();
    match action {
        UpdateSwapAction::Request => {
            let requested_by = body
                .get("requested_by")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            match runtime.request_update_swap(requested_by, now_ms) {
                Ok(pending_since_ms) => ApiResponse::json(
                    200,
                    JsonBody::Value(serde_json::json!({
                        "requested": true,
                        "swap_pending_ms": pending_since_ms,
                    })),
                ),
                Err(refusal) => ApiResponse::json(
                    409,
                    JsonBody::Value(serde_json::json!({
                        "error": "update_swap_refused",
                        "detail": refusal.detail(),
                    })),
                ),
            }
        }
        UpdateSwapAction::Claim => match runtime.claim_update_swap(now_ms) {
            Some(request) => ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({
                    "pending": true,
                    "requested_by": request.requested_by,
                    "requested_ms": request.requested_ms,
                })),
            ),
            None => ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "pending": false })),
            ),
        },
        UpdateSwapAction::Result => {
            let Some(ok) = body.get("ok").and_then(|value| value.as_bool()) else {
                return ApiResponse::json_error(400, "result body needs a boolean `ok`");
            };
            let detail = body
                .get("detail")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            runtime.record_swap_result(ok, detail, now_ms);
            ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "recorded": true })),
            )
        }
    }
}

/// `POST /api/daemon/update-swap[/claim|/result]` — the HTTP wrappers.
pub(crate) async fn handle_daemon_update_swap(
    stream: DemuxStream,
    action: UpdateSwapAction,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = daemon_update_swap_api_response(action, &body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}
