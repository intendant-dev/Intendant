//! The agenda's HTTP surface: `GET /api/agenda` (ledger snapshot),
//! `GET /api/agenda/ops` (raw op-log page), `GET /api/agenda/occurrences`
//! (raw occurrence-journal page), and `POST /api/agenda/op`
//! (apply one command), plus the transport-neutral cores their
//! dashboard-control tunnel twins reuse. The IAM gate (`agenda.read` /
//! `agenda.write`) runs pre-dispatch off the route rows; mutations
//! funnel through the daemon's single-writer
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
    let sessions = agenda_sessions_join(&crate::platform::home_dir(), &items);
    // Tier-1 PR state for the anchors this snapshot serves — the same
    // sibling discipline as `sessions`: keyed by the anchors' url-ref
    // locators, memory-only (the scanner's poll fetched it, not this
    // render), a locator with no entry claims nothing. Omitted entirely
    // when nothing joins.
    let pull_requests = crate::github_pr::join::tier1().for_locators(
        items
            .iter()
            .flat_map(|item| item.refs.iter())
            .map(|r| r.locator.as_str()),
    );
    let mut body = serde_json::json!({
        "items": items,
        "counts": counts,
        "skipped_lines": skipped_lines,
        "reminder_policy": agenda.reminder_policy(),
        "sessions": sessions,
    });
    if !pull_requests.is_empty() {
        body.as_object_mut()
            .expect("object body")
            .insert("pull_requests".to_string(), pull_requests.into());
    }
    ApiResponse::json(200, JsonBody::Value(body))
}

/// Transport-neutral core of `GET /api/agenda/items/{item_id}/pr-state`
/// (tunnel twin `api_agenda_pr_state`): the tier-2 render join — checks,
/// review, mergeability — fetched through the daemon cache on card
/// expand, never on list render, never stored, never an op. Absent data
/// claims nothing: no client (integration unconfigured/paused), no PR
/// ref, or any GitHub failure all serve `status: "unavailable"` — the
/// card degrades to the anchor, it never errors.
pub(crate) async fn agenda_pr_state_api_response(
    item_id: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let Some(item) = agenda.item_by_id(item_id) else {
        return ApiResponse::json_error(404, "agenda item not found");
    };
    let Some((locator, repo, number)) = item.refs.iter().find_map(|r| {
        crate::github_pr::scanner::parse_pr_locator(&r.locator)
            .map(|(repo, number)| (r.locator.clone(), repo, number))
    }) else {
        return ApiResponse::json_error(404, "item has no pull-request reference");
    };
    let client = crate::github_pr::join::published_client()
        .read()
        .expect("client slot poisoned")
        .clone();
    let unavailable = |detail: &str| {
        ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "item_id": item_id,
                "status": "unavailable",
                "detail": detail,
            })),
        )
    };
    let Some(client) = client else {
        return unavailable("integration not running");
    };
    match crate::github_pr::join::tier2()
        .fetch_through(&client, &repo, number, &locator)
        .await
    {
        Some(state) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "item_id": item_id,
                "status": "live",
                "state": state,
            })),
        ),
        None => unavailable("GitHub unreachable or the PR is gone"),
    }
}

pub(crate) async fn handle_agenda_pr_state(
    stream: DemuxStream,
    item_id: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_pr_state_api_response(&item_id, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Display-resolution join for the sessions the served items reference:
/// recorded session id → conversation row identity (`source`,
/// `conversation_id`, the Sessions-tab row `key`) + human name where one
/// exists. A **sibling** of `items`, never fields on them — the item DTO
/// stays the pure fold product. A recorded wrapper id resolves through the
/// wrapper index to its backend conversation even when superseded; a
/// dangling id (log dir gone, index pruned) simply has no entry, and every
/// surface degrades to the raw id.
fn agenda_sessions_join(
    home: &std::path::Path,
    items: &[crate::agenda::AgendaItem],
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for item in items {
        for recorded_id in item.referenced_session_ids() {
            if recorded_id.is_empty() || out.contains_key(recorded_id) {
                continue;
            }
            if let Some(entry) = agenda_session_join_entry(home, recorded_id) {
                out.insert(recorded_id.to_string(), entry);
            }
        }
    }
    out
}

/// One recorded session id → its display identity, or `None` when nothing
/// on this daemon resolves it anymore. `project_root` (additive) is the
/// session's recorded project root — the Start-now sheet's provenance
/// prefill and the follow-up resume's launch root derive from it.
fn agenda_session_join_entry(
    home: &std::path::Path,
    recorded_id: &str,
) -> Option<serde_json::Value> {
    let project_root = crate::agenda::recorded_session_project_root(home, recorded_id)
        .map(|root| root.to_string_lossy().into_owned());
    // External wrapper (any incarnation) → its backend conversation, which
    // is what the Sessions tab keys rows by.
    if let Some((source, conversation_id)) =
        crate::external_wrapper_index::conversation_for_wrapper(home, recorded_id)
    {
        let name = crate::session_names::external_session_name(home, &source, &conversation_id);
        return Some(serde_json::json!({
            "source": source,
            "conversation_id": conversation_id,
            "key": format!("{source}\u{1f}{conversation_id}"),
            "name": name,
            "project_root": project_root,
        }));
    }
    // Native session: the id itself is the conversation.
    let name = crate::session_names::intendant_session_name(home, recorded_id)?;
    Some(serde_json::json!({
        "source": "intendant",
        "conversation_id": recorded_id,
        "key": format!("intendant\u{1f}{recorded_id}"),
        "name": name,
        "project_root": project_root,
    }))
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

/// Transport-neutral core of `GET /api/agenda/definitions` (tunnel twin
/// `api_agenda_definitions`): the automation-definition catalog — house
/// and personal libraries, each validated, with provenance/shadowing
/// visible and invalid entries listed with their refusal reason.
/// Read-only; listing grants nothing (bindingness requires the stamp
/// seal under an approval digest).
pub(crate) async fn agenda_definitions_api_response(
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let catalog = agenda.definition_catalog();
    match serde_json::to_value(&catalog) {
        Ok(value) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "definitions": value })),
        ),
        Err(err) => ApiResponse::json_error(500, format!("encoding definition catalog: {err}")),
    }
}

pub(crate) async fn handle_agenda_definitions(
    stream: DemuxStream,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_definitions_api_response(mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/agenda/sealed/{sha256}` (tunnel
/// twin `api_agenda_sealed`): one sealed binding-ref snapshot's bytes by
/// pin — read-only and content-addressed (the served bytes re-hash to
/// the requested pin or the request errors; a card visited after
/// "Later" re-renders exactly what was sealed). Text serves as UTF-8;
/// anything else rides base64 so the tunnel twin stays JSON.
pub(crate) async fn agenda_sealed_api_response(
    sha256: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    match agenda.sealed_content(sha256) {
        Ok(Some(bytes)) => {
            let body = match String::from_utf8(bytes) {
                Ok(text) => serde_json::json!({
                    "sha256": sha256.trim().to_ascii_lowercase(),
                    "encoding": "utf8",
                    "content": text,
                }),
                Err(err) => {
                    use base64::Engine as _;
                    serde_json::json!({
                        "sha256": sha256.trim().to_ascii_lowercase(),
                        "encoding": "base64",
                        "content": base64::engine::general_purpose::STANDARD
                            .encode(err.into_bytes()),
                    })
                }
            };
            ApiResponse::json(200, JsonBody::Value(body))
        }
        Ok(None) => ApiResponse::json_error(404, "no sealed snapshot under that pin"),
        Err(err) => {
            let status = if err.contains("64 hex") { 400 } else { 500 };
            ApiResponse::json_error(status, err)
        }
    }
}

pub(crate) async fn handle_agenda_sealed(
    stream: DemuxStream,
    sha256: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_sealed_api_response(&sha256, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// The `POST /api/agenda/stamp` body (tunnel twin `api_agenda_stamp`):
/// the stamp command's fields without the op tag. Deny-unknown so a
/// misspelled override refuses instead of silently inheriting.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StampRequest {
    definition: String,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    fire_at_ms: Option<u64>,
    #[serde(default)]
    every_ms: Option<u64>,
    #[serde(default)]
    suspend_after: Option<u32>,
    #[serde(default)]
    agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
    #[serde(default)]
    source: Option<String>,
}

/// Transport-neutral core of `POST /api/agenda/stamp`: stamp one
/// automation definition — the daemon reads/validates/seals the file,
/// parks the instance graph, and proposes per node; the response is the
/// whole stamped graph (hub, nodes, digests, the sealed pin) for the
/// approval sheet. Parks + proposes ONLY — approval stays the owner's
/// per-effect act. `actor` is gate-resolved at the authenticated edge,
/// never parsed from the body.
pub(crate) async fn agenda_stamp_api_response(
    body_text: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
    actor: Option<crate::agenda::AgendaActor>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let request: StampRequest = match serde_json::from_str(body_text) {
        Ok(request) => request,
        Err(err) => {
            return ApiResponse::json_error(400, format!("invalid stamp request: {err}"));
        }
    };
    let cmd = crate::agenda::AgendaCommand::Stamp {
        definition: request.definition,
        project_root: request.project_root,
        fire_at_ms: request.fire_at_ms,
        every_ms: request.every_ms,
        suspend_after: request.suspend_after,
        agent_config: request.agent_config,
        source: request.source,
    };
    match agenda.stamp(cmd, actor) {
        Ok(outcome) => match serde_json::to_value(&outcome) {
            Ok(value) => {
                ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "stamp": value })))
            }
            Err(err) => ApiResponse::json_error(500, format!("encoding stamp outcome: {err}")),
        },
        Err(err) => ApiResponse::json_error(agenda_error_status(&err), err.to_string()),
    }
}

pub(crate) async fn handle_agenda_stamp(
    stream: DemuxStream,
    body_text: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    actor: Option<crate::agenda::AgendaActor>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_stamp_api_response(&body_text, mcp_server.as_ref(), actor).await;
    write_api_response(stream, response, cors, fleet_origin).await;
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

/// Transport-neutral core of `GET /api/agenda/ops` (tunnel twin
/// `api_agenda_ops`): one page of the raw append-only op log, for honest
/// per-item history and manifest-revision diffs. `since` is a 0-based
/// line cursor, `item` filters to one item's ops, `limit` defaults to
/// [`crate::agenda::AGENDA_OPS_DEFAULT_LIMIT`] and is clamped by the
/// store. Lines this build cannot fold are served verbatim with
/// `known:false` — never hidden (see [`crate::agenda::AgendaStore::read_ops`]).
pub(crate) async fn agenda_ops_api_response(
    since: u64,
    item: Option<&str>,
    limit: Option<u64>,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let limit = limit
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX))
        .unwrap_or(crate::agenda::AGENDA_OPS_DEFAULT_LIMIT);
    let page: crate::agenda::AgendaOpsPage = match agenda.read_ops(since, item, limit) {
        Ok(page) => page,
        Err(err) => {
            return ApiResponse::json_error(500, format!("reading agenda op log: {err}"));
        }
    };
    match serde_json::to_value(&page) {
        Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
        Err(err) => ApiResponse::json_error(500, format!("encoding agenda op page: {err}")),
    }
}

pub(crate) async fn handle_agenda_ops(
    stream: DemuxStream,
    request_line: &str,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let since = query_param(request_line, "since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let item = query_param(request_line, "item").filter(|v| !v.is_empty());
    let limit = query_param(request_line, "limit").and_then(|v| v.parse().ok());
    let response =
        agenda_ops_api_response(since, item.as_deref(), limit, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/agenda/occurrences` (tunnel twin
/// `api_agenda_occurrences`): one page of the raw occurrence journal —
/// the delivery/dispatch truth behind reminders and scheduled sessions.
/// Same cursor semantics as the op-log route; records this build cannot
/// fold are served verbatim with `known:false` — never hidden (see
/// [`crate::agenda::AgendaHandle::read_occurrences`]).
pub(crate) async fn agenda_occurrences_api_response(
    since: u64,
    item: Option<&str>,
    limit: Option<u64>,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let limit = limit
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX))
        .unwrap_or(crate::agenda::AGENDA_OCCURRENCES_DEFAULT_LIMIT);
    let page: crate::agenda::AgendaOccurrencesPage =
        match agenda.read_occurrences(since, item, limit) {
            Ok(page) => page,
            Err(err) => {
                return ApiResponse::json_error(500, format!("reading occurrence journal: {err}"));
            }
        };
    match serde_json::to_value(&page) {
        Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
        Err(err) => ApiResponse::json_error(500, format!("encoding occurrence page: {err}")),
    }
}

pub(crate) async fn handle_agenda_occurrences(
    stream: DemuxStream,
    request_line: &str,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let since = query_param(request_line, "since")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let item = query_param(request_line, "item").filter(|v| !v.is_empty());
    let limit = query_param(request_line, "limit").and_then(|v| v.parse().ok());
    let response =
        agenda_occurrences_api_response(since, item.as_deref(), limit, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp_with_agenda(
        dir: &std::path::Path,
    ) -> (
        Arc<crate::mcp::IntendantServer>,
        Arc<crate::agenda::AgendaHandle>,
    ) {
        let bus = crate::event::EventBus::new();
        let mut state = crate::mcp::McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            dir.join("logs"),
        );
        let agenda_dir = dir.join("agenda");
        let handle = Arc::new(crate::agenda::AgendaHandle::new(
            crate::agenda::AgendaStore::open(&agenda_dir).unwrap(),
            bus.clone(),
            &agenda_dir,
        ));
        state.agenda = Some(handle.clone());
        let server = Arc::new(crate::mcp::IntendantServer::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(state)),
            bus,
        ));
        (server, handle)
    }

    fn park_pr_anchor(agenda: &crate::agenda::AgendaHandle, locator: &str) -> String {
        agenda
            .apply(
                crate::agenda::AgendaCommand::Add {
                    kind: crate::agenda::AgendaKind::Task,
                    title: "r#5: fixture anchor".into(),
                    body: String::new(),
                    tags: vec!["pr".into()],
                    due_ms: None,
                    source: Some("github-pr-scanner".into()),
                    refs: vec![crate::agenda::AgendaRefSpec {
                        ref_type: crate::agenda::AgendaRefType::Url,
                        locator: locator.to_string(),
                        must_read: false,
                        label: None,
                    }],
                },
                Some(crate::agenda::AgendaActor::daemon()),
            )
            .unwrap()
            .id
    }

    fn json_of(response: &ApiResponse) -> serde_json::Value {
        match response {
            ApiResponse::Json { body, .. } => {
                serde_json::from_str(&body.as_text()).expect("json body")
            }
            _ => panic!("expected the JSON lane"),
        }
    }

    /// Tier 1 rides the snapshot as a sibling map keyed by served
    /// anchors' locators — items without joined refs produce no key at
    /// all, and the item DTO itself never grows a state field.
    #[tokio::test]
    async fn snapshot_serves_tier1_sibling_for_served_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let (server, agenda) = mcp_with_agenda(dir.path());
        let locator = "https://github.com/o/r/pull/5";
        park_pr_anchor(&agenda, locator);
        let open: Vec<crate::github_pr::client::PrSummary> =
            serde_json::from_value(serde_json::json!([
                crate::github_pr::client::test_fixture::pull(5, "live", true)
            ]))
            .unwrap();
        crate::github_pr::join::tier1().update_repo("o/r", &open);

        let body = json_of(&agenda_list_api_response(Some(&server)).await);
        let joined = &body["pull_requests"][locator];
        assert_eq!(joined["draft"], true);
        assert_eq!(joined["title"], "live");
        assert!(joined["fetched_at_ms"].as_u64().unwrap() > 0);
        let item = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| {
                i["tags"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|x| x == "pr"))
            })
            .unwrap();
        assert!(
            item.get("pull_request").is_none() && item.get("pr_state").is_none(),
            "join data must never become item fields"
        );
    }

    /// The tier-2 expand lane: live state from the fixture while a
    /// client is published; unavailable (never an error) without one;
    /// and the op log stays byte-identical throughout — joined state is
    /// served, never stored, never an op.
    #[tokio::test]
    async fn pr_state_serves_live_degrades_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (server, agenda) = mcp_with_agenda(dir.path());
        let locator = "https://github.com/o/r/pull/5";
        let item_id = park_pr_anchor(&agenda, locator);
        let log_path = dir.path().join("agenda").join("agenda.jsonl");
        let log_before = std::fs::read(&log_path).unwrap();

        use crate::github_pr::client::test_fixture::{
            spawn_fixture, test_credentials, token_route,
        };
        let mut routes = std::collections::HashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls/5".to_string()),
            (
                200,
                Vec::new(),
                serde_json::json!({
                    "state": "open", "merged": false, "draft": true,
                    "title": "live title", "mergeable": true,
                    "head": {"sha": "feedface00"},
                })
                .to_string(),
            ),
        );
        routes.insert(
            (
                "GET".to_string(),
                "/repos/o/r/commits/feedface00/check-runs".to_string(),
            ),
            (
                200,
                Vec::new(),
                serde_json::json!({
                    "total_count": 2,
                    "check_runs": [
                        {"status": "completed", "conclusion": "success"},
                        {"status": "completed", "conclusion": "failure"},
                    ],
                })
                .to_string(),
            ),
        );
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls/5/reviews".to_string()),
            (
                200,
                Vec::new(),
                serde_json::json!([
                    {"user": {"login": "a"}, "state": "APPROVED"},
                    {"user": {"login": "b"}, "state": "CHANGES_REQUESTED"},
                ])
                .to_string(),
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client =
            crate::github_pr::client::GithubAppClient::new(&fixture.base, test_credentials())
                .unwrap();
        crate::github_pr::join::publish_client(Some(std::sync::Arc::new(client)));

        let body = json_of(&agenda_pr_state_api_response(&item_id, Some(&server)).await);
        assert_eq!(body["status"], "live");
        assert_eq!(body["state"]["draft"], true);
        assert_eq!(body["state"]["mergeable"], true);
        assert_eq!(body["state"]["checks"]["total"], 2);
        assert_eq!(body["state"]["checks"]["failed"], 1);
        assert_eq!(body["state"]["review"]["approved"], 1);
        assert_eq!(body["state"]["review"]["changes_requested"], 1);

        // No client published: honest unavailability, never an error.
        crate::github_pr::join::publish_client(None);
        let body = json_of(&agenda_pr_state_api_response(&item_id, Some(&server)).await);
        assert_eq!(body["status"], "unavailable");

        // A non-PR item answers 404 by name.
        let plain = agenda
            .apply(
                crate::agenda::AgendaCommand::Add {
                    kind: crate::agenda::AgendaKind::Note,
                    title: "no pr here".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                    refs: Vec::new(),
                },
                None,
            )
            .unwrap();
        let response = agenda_pr_state_api_response(&plain.id, Some(&server)).await;
        match &response {
            ApiResponse::Json { status, .. } => assert_eq!(*status, 404),
            _ => panic!("expected JSON"),
        }

        // The whole lane wrote nothing but the plain item's park: strip
        // that one line and the log is byte-identical — joined state
        // never reaches the diary.
        let log_after = std::fs::read(&log_path).unwrap();
        let after_lines: Vec<&[u8]> = log_after.split(|b| *b == b'\n').collect();
        let before_lines = log_before.split(|b| *b == b'\n').count();
        assert_eq!(
            after_lines.len(),
            before_lines + 1,
            "only the plain park landed"
        );
        assert!(
            log_after.starts_with(&log_before),
            "existing lines untouched"
        );
    }

    /// The F1 provenance resolver: a recorded wrapper id — even a
    /// superseded incarnation whose own log dir is gone — resolves to its
    /// backend conversation with the Sessions-tab row key and the human
    /// name; a native id resolves through its log dir; an unknown id
    /// resolves to nothing and surfaces degrade to the raw id.
    #[test]
    fn join_entry_resolves_wrappers_natives_and_degrades() {
        let home_dir = tempfile::tempdir().unwrap();
        let home = home_dir.path();

        // Two wrapper incarnations of one backend conversation — the shape
        // a resumed external conversation produces (the second upsert
        // demotes the first to Superseded via the identity conflict). The
        // index stores each record under its log dir's identity, so the
        // dirs are NAMED by their wrapper session ids, as real wrapper log
        // dirs are.
        let wrap_project = tempfile::tempdir().unwrap();
        let wrap_a = home.join("wrappers").join("sess-wrapper-a");
        let wrap_b = home.join("wrappers").join("sess-wrapper-b");
        std::fs::create_dir_all(&wrap_a).unwrap();
        std::fs::create_dir_all(&wrap_b).unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "claude-code",
            "conv-backend-1",
            "sess-wrapper-a",
            &wrap_a,
            Some(wrap_project.path()),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "claude-code",
            "conv-backend-1",
            "sess-wrapper-b",
            &wrap_b,
            None,
        )
        .unwrap();
        crate::session_names::rename_session(
            home,
            "claude-code",
            "conv-backend-1",
            "cert sweep planning",
        )
        .unwrap();
        // The parking wrapper's dir is later pruned; the conversation must
        // keep resolving (this is exactly the dir-filtered lookups' gap).
        std::fs::remove_dir_all(&wrap_a).unwrap();

        let entry =
            agenda_session_join_entry(home, "sess-wrapper-a").expect("superseded wrapper resolves");
        assert_eq!(entry["source"], "claude-code");
        assert_eq!(entry["conversation_id"], "conv-backend-1");
        assert_eq!(entry["key"], "claude-code\u{1f}conv-backend-1");
        assert_eq!(entry["name"], "cert sweep planning");
        // The recorded project root survives the pruned log dir via the
        // index record — the Start-now sheet's provenance prefill.
        assert_eq!(
            entry["project_root"],
            wrap_project.path().to_string_lossy().as_ref()
        );

        // Native session: id resolves via its log dir + metadata name.
        let native_dir = crate::platform::intendant_home_in(home)
            .join("logs")
            .join("sess-native-1");
        std::fs::create_dir_all(&native_dir).unwrap();
        std::fs::write(
            native_dir.join("session_meta.json"),
            r#"{"session_id":"sess-native-1","name":"tidy the fixtures","project_root":"/work/native-project"}"#,
        )
        .unwrap();
        let native = agenda_session_join_entry(home, "sess-native-1").expect("native resolves");
        assert_eq!(native["source"], "intendant");
        assert_eq!(native["key"], "intendant\u{1f}sess-native-1");
        assert_eq!(native["name"], "tidy the fixtures");
        assert_eq!(native["project_root"], "/work/native-project");

        // Unknown ids produce no entry (raw-id fallback), and the join map
        // carries exactly the resolvable ids of the items it serves.
        assert!(agenda_session_join_entry(home, "never-existed").is_none());

        let item = |id: &str, sid: Option<&str>| {
            let mut store = crate::agenda::AgendaStore::open(
                &crate::platform::intendant_home_in(home)
                    .join("agenda-test")
                    .join(id),
            )
            .unwrap();
            store
                .apply_command(
                    crate::agenda::AgendaCommand::Add {
                        refs: Vec::new(),
                        kind: crate::agenda::AgendaKind::Task,
                        title: format!("item {id}"),
                        body: String::new(),
                        tags: Vec::new(),
                        due_ms: None,
                        source: None,
                    },
                    sid.map(|sid| crate::agenda::AgendaActor {
                        principal: None,
                        session_id: Some(sid.to_string()),
                        kind: Some("agent_session".to_string()),
                    }),
                    1,
                )
                .unwrap()
        };
        let items = vec![
            item("a", Some("sess-wrapper-a")),
            item("b", Some("never-existed")),
            item("c", None),
        ];
        let join = agenda_sessions_join(home, &items);
        assert_eq!(join.len(), 1);
        assert!(join.contains_key("sess-wrapper-a"));
    }
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

/// `GET /api/agenda/blobs/{item_id}/{blob_id}/raw` — raw bytes of one
/// parked-ask preview blob. Same serving posture as the session-upload
/// raw route: attachment `Content-Disposition` + `nosniff`, so blobs can
/// never render by direct navigation — the dashboard consumes via
/// authenticated fetch (html → sandboxed `srcdoc`, images → `<img>`).
pub(crate) async fn agenda_blob_raw_api_response(
    item_id: &str,
    blob_id: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let Some((descriptor, path)) = crate::agenda::find_blob(agenda.dir(), item_id, blob_id) else {
        // Malformed ids and retired items' deleted blobs both land here.
        return ApiResponse::json_error(404, "agenda blob not found");
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return ApiResponse::json_error(500, format!("read agenda blob: {err}"));
        }
    };
    let meta = serde_json::json!({
        "ok": true,
        "id": descriptor.id,
        "name": descriptor.name,
        "mime": descriptor.mime,
        "size": bytes.len() as u64,
    });
    ApiResponse::Bytes {
        status: 200,
        content_type: descriptor.mime.clone(),
        headers: vec![
            (
                "Content-Disposition",
                format!(
                    "attachment; filename=\"{}\"",
                    descriptor.name.replace('"', "")
                ),
            ),
            ("X-Content-Type-Options", "nosniff".to_string()),
            ("Cache-Control", "no-cache".to_string()),
            ("Connection", "close".to_string()),
        ],
        bytes: BytesPayload::InMemory(bytes),
        meta,
    }
}

/// Transport-neutral core of `GET /api/agenda/items/{item_id}/refs/drift`
/// (tunnel twin `api_agenda_ref_drift`): re-hash the item's file refs
/// against their recorded attach digests, on demand. This is the detail
/// view's expand-time honesty check (G1) — deliberately never computed on
/// list render, and nothing is stored: the attach-time digest in the log
/// stays the only durable fact. Digest-less file refs (foreign logs) get
/// no row — absent data claims nothing.
pub(crate) async fn agenda_ref_drift_api_response(
    item_id: &str,
    mcp_server: Option<&Arc<crate::mcp::IntendantServer>>,
) -> ApiResponse {
    let Some(agenda) = agenda_handle(mcp_server).await else {
        return ApiResponse::json_error(503, "agenda unavailable on this daemon");
    };
    let Some(item) = agenda.item_by_id(item_id) else {
        return ApiResponse::json_error(404, "agenda item not found");
    };
    let refs: Vec<serde_json::Value> = item
        .refs
        .iter()
        .filter_map(|r| {
            let digest = r.digest.as_deref()?;
            Some(serde_json::json!({
                "ref_type": r.ref_type.as_str(),
                "locator": r.locator,
                "status": crate::agenda::file_ref_drift(&r.locator, digest),
            }))
        })
        // Attestation refs (Track AO): the current run's self-report
        // pointers ride the same expand-time honesty check — verify-only
        // pins, re-hashed against the attest-time sha256; nothing is
        // sealed and nothing is stored (Q3/OPEN-3).
        .chain(
            item.effects
                .iter()
                .filter_map(|effect| effect.last_run.as_ref())
                .filter_map(|run| run.attestation.as_ref())
                .flat_map(|attestation| attestation.refs.iter())
                .map(|r| {
                    serde_json::json!({
                        "ref_type": "file",
                        "locator": r.locator,
                        "status": crate::agenda::attestation_ref_drift(&r.locator, &r.sha256),
                    })
                }),
        )
        .collect();
    // The manifests' sealed binding refs get the same expand-time honesty
    // check (Track AW §2.4), plus the live hash/mtime the Review-&-adopt
    // gesture restates as its fresh pin. Presentation only: drift never
    // changes what fires (firings execute the sealed revision), and the
    // propose intake re-verifies any restated pin against the daemon's
    // own read, so nothing served here is authority.
    let binding_refs: Vec<serde_json::Value> = item
        .effects
        .iter()
        .flat_map(|effect| {
            effect.manifest.binding_refs.iter().map(|r| {
                let live = crate::agenda::binding_ref_drift(&r.locator, &r.sha256);
                let mut row = serde_json::json!({
                    "effect_id": effect.effect_id,
                    "locator": r.locator,
                    "pin": r.sha256,
                    "status": live.status,
                });
                if let Some(sha) = live.live_sha256 {
                    row["live_sha256"] = sha.into();
                }
                if let Some(ms) = live.live_mtime_ms {
                    row["live_mtime_ms"] = ms.into();
                }
                row
            })
        })
        .collect();
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({
            "item_id": item.id,
            "refs": refs,
            "binding_refs": binding_refs,
        })),
    )
}

pub(crate) async fn handle_agenda_ref_drift(
    stream: DemuxStream,
    item_id: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_ref_drift_api_response(&item_id, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_agenda_blob_raw(
    stream: DemuxStream,
    item_id: String,
    blob_id: String,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = agenda_blob_raw_api_response(&item_id, &blob_id, mcp_server.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}
