//! The agenda's HTTP surface: `GET /api/agenda` (ledger snapshot) and
//! `POST /api/agenda/op` (apply one command), plus the transport-neutral
//! cores their dashboard-control tunnel twins reuse. The IAM gate
//! (`agenda.read` / `agenda.write`) runs pre-dispatch off the route rows;
//! mutations funnel through the daemon's single-writer
//! [`crate::agenda::AgendaHandle`], which broadcasts `agenda_changed`.

use super::*;

/// Transport-neutral core of `GET /api/agenda` (tunnel twin
/// `api_agenda_list`): every item oldest-first plus status counts, the
/// count of preserved-but-unfolded log lines, and the reminder policy
/// (read-only here — mutations ride the Settings-gated policy route).
pub(crate) async fn agenda_list_api_response(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let (items, counts, skipped_lines) = agenda.snapshot();
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({
            "items": items,
            "counts": counts,
            "skipped_lines": skipped_lines,
            "reminder_policy": agenda.reminder_policy(),
        })),
    )
}

/// Transport-neutral core of `POST /api/agenda/reminders/policy` (tunnel
/// twin `api_agenda_reminder_policy`): body is a merge-patch
/// ([`crate::agenda::ReminderPolicyPatch`] — absent keeps, `null` clears);
/// returns the effective policy. Owner policy, Settings-gated.
pub(crate) async fn agenda_reminder_policy_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let patch: crate::agenda::ReminderPolicyPatch = match serde_json::from_str(body_text) {
        Ok(patch) => patch,
        Err(err) => {
            return ApiResponse::json_error(400, format!("invalid reminder policy patch: {err}"));
        }
    };
    if patch.is_empty() {
        return ApiResponse::json_error(400, "policy patch changes nothing");
    }
    match agenda.update_reminder_policy(patch) {
        Ok(policy) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "reminder_policy": policy })),
        ),
        Err(err) => ApiResponse::json_error(500, format!("saving reminder policy: {err}")),
    }
}

pub(crate) async fn handle_agenda_reminder_policy(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_reminder_policy_api_response(&body_text, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/agenda/op` (tunnel twin
/// `api_agenda_op`): the body is one [`crate::agenda::AgendaCommand`];
/// success returns the item as it now stands (with its minted id for
/// `add`). `actor` is the caller's gate-resolved attribution, mapped at
/// the authenticated edge (HTTP dispatch / tunnel grant) — never parsed
/// from the request body.
pub(crate) async fn agenda_op_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
    actor: Option<crate::agenda::AgendaActor>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let cmd: crate::agenda::AgendaCommand = match serde_json::from_str(body_text) {
        Ok(cmd) => cmd,
        Err(err) => {
            return ApiResponse::json_error(400, format!("invalid agenda command: {err}"));
        }
    };
    match agenda.apply(cmd, actor) {
        Ok(item) => ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "item": item }))),
        Err(err) => ApiResponse::json_error(agenda_error_status(&err), err.to_string()),
    }
}

async fn agenda_handle(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> Option<Arc<crate::agenda::AgendaHandle>> {
    match mcp_server {
        Some(server) => server.agenda_handle().await,
        None => None,
    }
}

fn agenda_error_status(err: &crate::agenda::AgendaError) -> u16 {
    match err {
        crate::agenda::AgendaError::NotFound(_) => 404,
        crate::agenda::AgendaError::Invalid(_) | crate::agenda::AgendaError::Transition(_) => 400,
        crate::agenda::AgendaError::NotPermitted { .. } => 403,
        crate::agenda::AgendaError::Io(_) => 500,
    }
}

pub(crate) async fn handle_agenda_list(
    stream: DemuxStream,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_list_api_response(mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_agenda_op(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    actor: Option<crate::agenda::AgendaActor>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_op_api_response(&body_text, mcp_server.as_ref(), actor).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}
