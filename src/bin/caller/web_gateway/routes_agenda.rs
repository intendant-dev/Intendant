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
    let sessions = agenda_sessions_join(&crate::platform::home_dir(), &items);
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({
            "items": items,
            "counts": counts,
            "skipped_lines": skipped_lines,
            "reminder_policy": agenda.reminder_policy(),
            "sessions": sessions,
        })),
    )
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
/// on this daemon resolves it anymore.
fn agenda_session_join_entry(
    home: &std::path::Path,
    recorded_id: &str,
) -> Option<serde_json::Value> {
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
        }));
    }
    // Native session: the id itself is the conversation.
    let name = crate::session_names::intendant_session_name(home, recorded_id)?;
    Some(serde_json::json!({
        "source": "intendant",
        "conversation_id": recorded_id,
        "key": format!("intendant\u{1f}{recorded_id}"),
        "name": name,
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

#[cfg(test)]
mod tests {
    use super::*;

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
            None,
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

        // Native session: id resolves via its log dir + metadata name.
        let native_dir = crate::platform::intendant_home_in(home)
            .join("logs")
            .join("sess-native-1");
        std::fs::create_dir_all(&native_dir).unwrap();
        std::fs::write(
            native_dir.join("session_meta.json"),
            r#"{"session_id":"sess-native-1","name":"tidy the fixtures"}"#,
        )
        .unwrap();
        let native = agenda_session_join_entry(home, "sess-native-1").expect("native resolves");
        assert_eq!(native["source"], "intendant");
        assert_eq!(native["key"], "intendant\u{1f}sess-native-1");
        assert_eq!(native["name"], "tidy the fixtures");

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
