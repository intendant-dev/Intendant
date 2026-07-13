//! The reader task: consumes the codex app-server's stdout JSON-RPC stream,
//! classifies notifications/approvals against the active thread and turn,
//! and translates items into AgentEvents (file changes, web searches, collab
//! agents, plans, rate limits, command output hygiene).

use super::*;

/// Runs on a background tokio task, reading JSONL from the Codex process
/// stdout and dispatching events / resolving pending requests.
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn reader_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    approval_counter: Arc<AtomicU64>,
    active_thread_id: Arc<Mutex<Option<String>>>,
    active_turn_id: Arc<Mutex<Option<String>>>,
    active_turns: ActiveTurns,
    latest_token_usage: Arc<Mutex<Option<serde_json::Value>>>,
    context_pressure_floor: Arc<Mutex<Option<CodexContextPressureFloor>>>,
    model: Option<String>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut terminal_turns_observed: HashSet<String> = HashSet::new();
    let mut notification_state = CodexNotificationState::default();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // EOF — clear any active turn so a later interrupt_turn
                // doesn't fire against a dead process.
                active_turn_id.lock().await.take();
                active_turns.lock().await.clear();
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Process stdout closed".into(),
                    exit_code: None,
                });
                return;
            }
            Err(e) => {
                active_turn_id.lock().await.take();
                active_turns.lock().await.clear();
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: format!("IO error reading stdout: {}", e),
                    exit_code: None,
                });
                return;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: JsonRpcMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "[codex] failed to parse JSON-RPC message: {}: {:?}",
                    e, line
                );
                continue;
            }
        };

        // 1. Response to our request (has id + result/error, no method)
        if msg.method.is_none() {
            if let Some(id) = msg.id {
                let mut pending = pending_requests.lock().await;
                if let Some(tx) = pending.remove(&id) {
                    if let Some(err) = msg.error {
                        let _ =
                            tx.send(Err(format!("JSON-RPC error {}: {}", err.code, err.message)));
                    } else {
                        let _ = tx.send(Ok(msg.result.unwrap_or(serde_json::Value::Null)));
                    }
                }
            }
            continue;
        }

        let method = msg.method.as_deref().unwrap_or("");

        // 2. Server-to-client request (has method AND id) -- approval requests
        if let Some(jsonrpc_id) = msg.id {
            let request_id = format!(
                "approval-{}",
                approval_counter.fetch_add(1, Ordering::Relaxed)
            );

            let params = msg.params.unwrap_or(serde_json::Value::Null);
            pending_approvals.lock().await.insert(
                request_id.clone(),
                PendingApproval {
                    jsonrpc_id,
                    method: method.to_string(),
                    params: params.clone(),
                },
            );

            let (thread_id, turn_id) = codex_event_scope(&params);

            if is_codex_mcp_approval_method(method) {
                // Tool / MCP call approval (e.g. Codex invoking Intendant's
                // own MCP server tools, or an MCP elicitation). Resolved with
                // the `{"action": ...}` shape in `resolve_approval`, which uses
                // the same predicate. Build a best-effort human-readable
                // label — never the bare "<unknown>" placeholder.
                let label = params
                    .pointer("/params/message")
                    .or_else(|| params.pointer("/message"))
                    .or_else(|| params.pointer("/item/name"))
                    .or_else(|| params.pointer("/item/tool"))
                    .or_else(|| params.pointer("/item/toolName"))
                    .or_else(|| params.pointer("/item/title"))
                    .or_else(|| params.pointer("/tool"))
                    .or_else(|| params.pointer("/name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("MCP tool call ({method})"));
                send_scoped_agent_event(
                    &event_tx,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    AgentEvent::ApprovalRequest {
                        request_id,
                        command: label,
                        category: ApprovalCategory::McpTool,
                    },
                );
            } else if method == "item/permissions/requestApproval" {
                send_scoped_agent_event(
                    &event_tx,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    AgentEvent::ApprovalRequest {
                        request_id,
                        command: codex_permissions_approval_label(&params),
                        category: ApprovalCategory::PermissionGrant,
                    },
                );
            } else if method == "item/fileChange/requestApproval" {
                let path = params
                    .pointer("/item/path")
                    .or_else(|| params.pointer("/path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>")
                    .to_string();
                let diff = params
                    .pointer("/item/diff")
                    .or_else(|| params.pointer("/diff"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                send_scoped_agent_event(
                    &event_tx,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    AgentEvent::FileApprovalRequest {
                        request_id,
                        path,
                        diff,
                    },
                );
            } else {
                // item/commandExecution/requestApproval or unknown server requests
                let command = params
                    .pointer("/item/command")
                    .or_else(|| params.pointer("/command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>")
                    .to_string();
                send_scoped_agent_event(
                    &event_tx,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    AgentEvent::ApprovalRequest {
                        request_id,
                        command,
                        category: ApprovalCategory::CommandExecution,
                    },
                );
            }
            continue;
        }

        // 3. Notification (has method, no id)
        let params = msg.params.unwrap_or(serde_json::Value::Null);

        // Track active turn id so interrupt_turn() has a target to cancel.
        // Codex emits turn_id in several shapes across versions; accept any
        // top-level `turnId` / `turn_id` / `turn.id` / `thread.lastTurnId`.
        //
        // The app-server stream can include notifications for Codex collab
        // subagent threads. Child or stale scoped notifications must not
        // appear in the active parent turn, mutate parent usage, or complete
        // the parent drain.
        let (thread_id, turn_id) = codex_event_scope(&params);
        let active_thread_snapshot = active_thread_id.lock().await.clone();
        let active_turn_for_thread = if let Some(thread_id) = thread_id.as_deref() {
            active_turns.lock().await.get(thread_id).cloned()
        } else {
            active_turn_id.lock().await.clone()
        };
        let final_answer_completed =
            method == "item/completed" && codex_item_completed_final_answer(&params);
        let terminal_keys = codex_terminal_observation_keys(
            &params,
            turn_id.as_deref(),
            active_turn_for_thread.as_deref(),
            thread_id.as_deref(),
            final_answer_completed,
        );
        let turn_terminal_observed =
            codex_any_terminal_observed(&terminal_turns_observed, &terminal_keys);

        if codex_notification_stale_for_active_turn(
            turn_id.as_deref(),
            active_turn_for_thread.as_deref(),
        ) {
            continue;
        }

        if codex_terminal_notification_already_observed(
            method,
            final_answer_completed,
            turn_terminal_observed,
        ) {
            continue;
        }

        let status_can_complete_turn = method != "thread/status/changed"
            || codex_thread_status_can_complete_turn(
                &params,
                active_turn_for_thread.as_deref(),
                turn_terminal_observed,
            );
        if method == "thread/status/changed" && !status_can_complete_turn {
            continue;
        }

        if method == "thread/tokenUsage/updated" {
            let usage = params
                .get("tokenUsage")
                .cloned()
                .unwrap_or_else(|| params.clone());
            let usage_targets_active_thread = thread_id
                .as_deref()
                .is_none_or(|thread_id| active_thread_snapshot.as_deref() == Some(thread_id));
            let usage = if usage_targets_active_thread {
                let mut latest = latest_token_usage.lock().await;
                let usage = codex_usage_preserving_hard_context_window(usage, latest.as_ref());
                *latest = Some(usage.clone());
                drop(latest);
                update_codex_context_pressure_floor(&context_pressure_floor, &usage).await;
                usage
            } else {
                usage
            };
            let snapshot = codex_usage_snapshot(&usage, model.as_deref().unwrap_or("codex"));
            if let Some(mut snapshot) = snapshot {
                snapshot.limits = notification_state.limit_windows.clone();
                notification_state.latest_usage = Some(snapshot.clone());
                send_scoped_agent_event(
                    &event_tx,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    AgentEvent::Usage { usage: snapshot },
                );
            }
        }

        if method == "account/rateLimits/updated" {
            let windows = codex_rate_limit_windows(&params);
            if !windows.is_empty() && windows != notification_state.limit_windows {
                notification_state.limit_windows = windows;
                // Refresh the gauges between turns by re-emitting the last
                // usage snapshot with the new windows — never a bare
                // zero-usage event, which would stomp the dashboard meter.
                if let Some(mut latest) = notification_state.latest_usage.clone() {
                    latest.limits = notification_state.limit_windows.clone();
                    notification_state.latest_usage = Some(latest.clone());
                    send_scoped_agent_event(
                        &event_tx,
                        thread_id.as_deref(),
                        turn_id.as_deref(),
                        AgentEvent::Usage { usage: latest },
                    );
                }
            }
        }

        match method {
            "turn/started" | "thread/started" => {
                codex_clear_terminal_observed(&mut terminal_turns_observed, &terminal_keys);
                if let (Some(thread_id), Some(turn_id)) = (thread_id.as_deref(), turn_id.as_deref())
                {
                    active_turns
                        .lock()
                        .await
                        .insert(thread_id.to_string(), turn_id.to_string());
                    if active_thread_snapshot.as_deref() == Some(thread_id) {
                        *active_turn_id.lock().await = Some(turn_id.to_string());
                    }
                }
            }
            "turn/completed" | "turn/interrupted" | "turn/failed" => {
                codex_mark_terminal_observed(&mut terminal_turns_observed, &terminal_keys);
                if let Some(thread_id) = thread_id.as_deref() {
                    active_turns.lock().await.remove(thread_id);
                    if active_thread_snapshot.as_deref() == Some(thread_id) {
                        active_turn_id.lock().await.take();
                    }
                } else {
                    active_turn_id.lock().await.take();
                }
            }
            "thread/status/changed" => {
                if codex_thread_status_type(&params)
                    .is_some_and(|status| matches!(status, "completed" | "idle"))
                {
                    codex_mark_terminal_observed(&mut terminal_turns_observed, &terminal_keys);
                    if let Some(thread_id) = thread_id.as_deref() {
                        active_turns.lock().await.remove(thread_id);
                        if active_thread_snapshot.as_deref() == Some(thread_id) {
                            active_turn_id.lock().await.take();
                        }
                    } else {
                        active_turn_id.lock().await.take();
                    }
                }
            }
            "item/completed" if final_answer_completed => {
                codex_mark_terminal_observed(&mut terminal_turns_observed, &terminal_keys);
                if let Some(thread_id) = thread_id.as_deref() {
                    active_turns.lock().await.remove(thread_id);
                    if active_thread_snapshot.as_deref() == Some(thread_id) {
                        active_turn_id.lock().await.take();
                    }
                } else {
                    active_turn_id.lock().await.take();
                }
            }
            _ => {}
        }

        translate_notification_with_scope(
            method,
            &params,
            &event_tx,
            &mut notification_state,
            thread_id.as_deref(),
            turn_id.as_deref(),
        );
    }
}

/// Extract a turn id from a Codex response or notification payload.
///
/// Codex v2 has emitted turn ids under several names across versions; accept
/// the common shapes: `turnId`, `turn_id`, `turn.id`, `thread.lastTurnId`.
pub(crate) fn extract_turn_id(value: &serde_json::Value) -> Option<String> {
    for path in [
        "/turnId",
        "/turn_id",
        "/turn/id",
        "/thread/lastTurnId",
        "/thread/last_turn_id",
    ] {
        if let Some(s) = value.pointer(path).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

pub(crate) fn codex_event_scope(params: &serde_json::Value) -> (Option<String>, Option<String>) {
    (extract_thread_id(params), extract_turn_id(params))
}

/// Single source of truth for "this Codex approval request is an MCP
/// tool-call / elicitation" — used by BOTH the reader (to pick the
/// approval category) and `resolve_approval` (to pick the response
/// shape). The two sides once used different substring sets, so
/// `mcpTool…` requests were classified as MCP but answered in the
/// `{"decision"}` shape, which Codex ignores.
pub(crate) fn is_codex_mcp_approval_method(method: &str) -> bool {
    method.contains("mcpServer") || method.contains("elicit") || method.contains("mcpTool")
}

pub(crate) fn codex_permissions_approval_label(params: &serde_json::Value) -> String {
    let reason = params
        .pointer("/reason")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cwd = params
        .pointer("/cwd")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let permissions = params
        .pointer("/permissions")
        .unwrap_or(&serde_json::Value::Null);

    let mut requested = Vec::new();
    if permissions
        .pointer("/network/enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        requested.push("network");
    }
    if permissions
        .pointer("/fileSystem")
        .or_else(|| permissions.pointer("/file_system"))
        .is_some()
    {
        requested.push("filesystem");
    }
    let requested = if requested.is_empty() {
        "permissions".to_string()
    } else {
        requested.join(", ")
    };

    match (reason, cwd) {
        (Some(reason), Some(cwd)) => format!("permission grant: {requested}; {reason}; cwd {cwd}"),
        (Some(reason), None) => format!("permission grant: {requested}; {reason}"),
        (None, Some(cwd)) => format!("permission grant: {requested}; cwd {cwd}"),
        (None, None) => format!("permission grant: {requested}"),
    }
}

pub(crate) fn codex_permissions_approval_response(
    params: &serde_json::Value,
    decision: ApprovalDecision,
) -> serde_json::Value {
    match decision {
        ApprovalDecision::Accept | ApprovalDecision::AcceptForSession => {
            let permissions = params
                .pointer("/permissions")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let scope = match decision {
                ApprovalDecision::AcceptForSession => "session",
                _ => "turn",
            };
            serde_json::json!({
                "permissions": permissions,
                "scope": scope,
                "strictAutoReview": false,
            })
        }
        ApprovalDecision::Decline | ApprovalDecision::Cancel => serde_json::json!({
            "permissions": {},
            "scope": "turn",
            "strictAutoReview": false,
        }),
    }
}

pub(crate) fn send_scoped_agent_event(
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    event: AgentEvent,
) {
    let _ = event_tx.send(AgentEvent::scoped(
        thread_id.map(str::to_string),
        turn_id.map(str::to_string),
        event,
    ));
}

pub(crate) fn codex_thread_status_type(params: &serde_json::Value) -> Option<&str> {
    match params.get("status")? {
        serde_json::Value::String(status) => Some(status.as_str()),
        serde_json::Value::Object(status) => status.get("type").and_then(|v| v.as_str()),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn codex_notification_targets_active_thread(
    params: &serde_json::Value,
    active_thread_id: Option<&str>,
) -> bool {
    match (extract_thread_id(params), active_thread_id) {
        (Some(event_thread_id), Some(active_thread_id)) => event_thread_id == active_thread_id,
        _ => true,
    }
}

#[cfg(test)]
pub(crate) fn codex_notification_targets_active_turn(
    params: &serde_json::Value,
    active_thread_id: Option<&str>,
    active_turn_id: Option<&str>,
) -> bool {
    if let (Some(event_thread_id), Some(active_thread_id)) =
        (extract_thread_id(params), active_thread_id)
    {
        if event_thread_id != active_thread_id {
            return false;
        }
    }

    if let (Some(event_turn_id), Some(active_turn_id)) = (extract_turn_id(params), active_turn_id) {
        if event_turn_id != active_turn_id {
            return false;
        }
    }

    true
}

pub(crate) fn codex_thread_status_can_complete_turn(
    params: &serde_json::Value,
    active_turn_id: Option<&str>,
    turn_terminal_observed: bool,
) -> bool {
    let Some(status) = codex_thread_status_type(params) else {
        return false;
    };
    if !matches!(status, "completed" | "idle") {
        return false;
    }
    if turn_terminal_observed {
        return false;
    }

    active_turn_id.is_some() || extract_turn_id(params).is_some()
}

pub(crate) fn codex_notification_stale_for_active_turn(
    event_turn_id: Option<&str>,
    active_turn_id: Option<&str>,
) -> bool {
    match (event_turn_id, active_turn_id) {
        (Some(event_turn_id), Some(active_turn_id)) => event_turn_id != active_turn_id,
        _ => false,
    }
}

pub(crate) fn codex_terminal_notification_already_observed(
    method: &str,
    final_answer_completed: bool,
    turn_terminal_observed: bool,
) -> bool {
    if !turn_terminal_observed {
        return false;
    }

    matches!(
        method,
        "turn/completed" | "turn/interrupted" | "turn/failed"
    ) || (method == "item/completed" && final_answer_completed)
}

pub(crate) fn codex_final_answer_item_id(params: &serde_json::Value) -> Option<String> {
    let item = params.get("item").unwrap_or(params);
    item.get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn codex_terminal_observation_keys(
    params: &serde_json::Value,
    turn_id: Option<&str>,
    active_turn_id: Option<&str>,
    thread_id: Option<&str>,
    final_answer_completed: bool,
) -> Vec<String> {
    let mut keys = Vec::new();
    if final_answer_completed {
        if let Some(item_id) = codex_final_answer_item_id(params) {
            keys.push(format!("item:{item_id}"));
        }
    }
    if let Some(turn_id) = turn_id.map(str::trim).filter(|id| !id.is_empty()) {
        keys.push(format!("turn:{turn_id}"));
    } else if let Some(active_turn_id) = active_turn_id.map(str::trim).filter(|id| !id.is_empty()) {
        keys.push(format!("turn:{active_turn_id}"));
    } else if let Some(thread_id) = thread_id.map(str::trim).filter(|id| !id.is_empty()) {
        keys.push(format!("thread:{thread_id}"));
    }
    keys.sort();
    keys.dedup();
    keys
}

pub(crate) fn codex_any_terminal_observed(observed: &HashSet<String>, keys: &[String]) -> bool {
    keys.iter().any(|key| observed.contains(key))
}

pub(crate) fn codex_mark_terminal_observed(observed: &mut HashSet<String>, keys: &[String]) {
    observed.extend(keys.iter().cloned());
}

pub(crate) fn codex_clear_terminal_observed(observed: &mut HashSet<String>, keys: &[String]) {
    for key in keys {
        observed.remove(key);
    }
}

pub(crate) fn codex_item_completed_final_answer(params: &serde_json::Value) -> bool {
    let item = params.get("item").unwrap_or(params);
    if item.get("type").and_then(|v| v.as_str()) != Some("agentMessage") {
        return false;
    }
    if item.get("phase").and_then(|v| v.as_str()) != Some("final_answer") {
        return false;
    }
    !matches!(
        item.get("status").and_then(|v| v.as_str()),
        Some("failed" | "cancelled")
    )
}

pub(crate) fn non_empty_string_at(value: &serde_json::Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        value
            .pointer(path)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    })
}

pub(crate) fn codex_file_change_preview(params: &serde_json::Value) -> Option<String> {
    if let Some(path) = non_empty_string_at(
        params,
        &[
            "/item/path",
            "/item/filePath",
            "/item/file_path",
            "/item/name",
            "/path",
            "/filePath",
            "/file_path",
        ],
    ) {
        return Some(path);
    }

    let item = params.get("item").unwrap_or(params);
    for key in ["paths", "files"] {
        if let Some(values) = item.get(key).and_then(|v| v.as_array()) {
            let mut paths = Vec::new();
            for value in values {
                if let Some(path) = value
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| {
                        non_empty_string_at(value, &["/path", "/filePath", "/file_path", "/name"])
                    })
                {
                    paths.push(path);
                }
            }
            if !paths.is_empty() {
                return Some(paths.join(", "));
            }
        }
    }

    if let Some(changes) = item.get("changes").and_then(|v| v.as_object()) {
        let mut paths: Vec<String> = changes.keys().cloned().collect();
        paths.sort();
        if !paths.is_empty() {
            return Some(paths.join(", "));
        }
    }

    None
}

pub(crate) fn codex_web_search_preview(params: &serde_json::Value) -> String {
    if let Some(query) = non_empty_string_at(
        params,
        &[
            "/item/query",
            "/item/searchQuery",
            "/item/search_query",
            "/item/userQuery",
            "/item/user_query",
            "/item/text",
            "/item/action/query",
            "/item/action/searchQuery",
            "/item/action/search_query",
            "/item/input/query",
            "/item/input/searchQuery",
            "/item/input/search_query",
            "/item/arguments/query",
            "/item/arguments/searchQuery",
            "/item/arguments/search_query",
            "/item/args/query",
            "/item/args/searchQuery",
            "/item/args/search_query",
            "/query",
            "/searchQuery",
            "/search_query",
        ],
    ) {
        return query;
    }

    let item = params.get("item").unwrap_or(params);
    for key in ["queries", "searchQueries", "search_queries"] {
        if let Some(values) = item.get(key).and_then(|v| v.as_array()) {
            let mut queries = Vec::new();
            for value in values {
                if let Some(query) = value
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| {
                        non_empty_string_at(
                            value,
                            &["/query", "/searchQuery", "/search_query", "/text"],
                        )
                    })
                {
                    queries.push(query);
                }
            }
            if !queries.is_empty() {
                return queries.join(", ");
            }
        }
    }

    if let Some(url) = non_empty_string_at(
        params,
        &[
            "/item/url",
            "/item/source",
            "/item/action/url",
            "/item/input/url",
            "/item/arguments/url",
            "/item/args/url",
            "/url",
        ],
    ) {
        return url;
    }

    "web search".to_string()
}

pub(crate) fn string_array_at(value: &serde_json::Value, paths: &[&str]) -> Vec<String> {
    paths
        .iter()
        .find_map(|path| {
            value.pointer(path).and_then(|v| v.as_array()).map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

pub(crate) fn codex_collab_agent_states(item: &serde_json::Value) -> Vec<SubAgentState> {
    let Some(states) = item
        .get("agentsStates")
        .or_else(|| item.get("agents_states"))
        .and_then(|v| v.as_object())
    else {
        return Vec::new();
    };

    let mut out: Vec<SubAgentState> = states
        .iter()
        .filter_map(|(thread_id, state)| {
            let status = state
                .get("status")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let message = state
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            Some(SubAgentState {
                thread_id: thread_id.clone(),
                status: status.to_string(),
                message,
            })
        })
        .collect();
    out.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
    out
}

pub(crate) fn codex_collab_agent_tool_call(params: &serde_json::Value) -> Option<AgentEvent> {
    let item = params.get("item").unwrap_or(params);
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if item_type != "collabAgentToolCall" {
        return None;
    }

    let item_id = non_empty_string_at(item, &["/id"]).unwrap_or_default();
    let tool = non_empty_string_at(item, &["/tool"]).unwrap_or_else(|| "collabAgent".to_string());
    let status =
        non_empty_string_at(item, &["/status"]).unwrap_or_else(|| "inProgress".to_string());
    let sender_thread_id =
        non_empty_string_at(item, &["/senderThreadId", "/sender_thread_id"]).unwrap_or_default();
    let receiver_thread_ids =
        string_array_at(item, &["/receiverThreadIds", "/receiver_thread_ids"]);
    let prompt = non_empty_string_at(item, &["/prompt"]);
    let model = non_empty_string_at(item, &["/model"]);
    let reasoning_effort = non_empty_string_at(item, &["/reasoningEffort", "/reasoning_effort"]);
    let agents = codex_collab_agent_states(item);

    Some(AgentEvent::SubAgentToolCall {
        item_id,
        tool,
        status,
        sender_thread_id,
        receiver_thread_ids,
        prompt,
        model,
        reasoning_effort,
        agents,
    })
}

pub(crate) fn codex_plan_entries(params: &serde_json::Value) -> Vec<(String, String, String)> {
    let Some(plan) = params.get("plan").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    plan.iter()
        .filter_map(|entry| {
            let content = entry
                .get("step")
                .or_else(|| entry.get("content"))
                .or_else(|| entry.get("text"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let priority = entry
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = entry
                .get("status")
                .and_then(|v| v.as_str())
                .map(normalize_plan_status)
                .unwrap_or_default();
            Some((content.to_string(), priority, status))
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct CodexNotificationState {
    goal_known_active: bool,
    latest_usage: Option<AgentUsageSnapshot>,
    /// Latest windows from `account/rateLimits/updated`, attached to
    /// outgoing usage snapshots for the vitals limit gauges.
    limit_windows: Vec<crate::types::SessionLimitWindow>,
    command_output_hygiene: HashMap<String, CodexCommandOutputHygiene>,
}

/// Parse an `account/rateLimits/updated` payload (app-server v2 shape:
/// `rateLimits.{primary,secondary}.{usedPercent,windowDurationMins,
/// resetsAt}`, camelCase with snake_case tolerated) into vitals windows.
pub(crate) fn codex_rate_limit_windows(
    params: &serde_json::Value,
) -> Vec<crate::types::SessionLimitWindow> {
    let snapshot = params
        .get("rateLimits")
        .or_else(|| params.get("rate_limits"))
        .unwrap_or(params);
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(window) = snapshot.get(key) else {
            continue;
        };
        let Some(used) = window
            .get("usedPercent")
            .or_else(|| window.get("used_percent"))
            .and_then(|v| v.as_f64())
        else {
            continue;
        };
        let minutes = window
            .get("windowDurationMins")
            .or_else(|| window.get("window_duration_mins"))
            .or_else(|| window.get("window_minutes"))
            .and_then(|v| v.as_u64());
        windows.push(crate::types::SessionLimitWindow {
            label: codex_rate_limit_label(minutes, key),
            used_pct: Some(used.round().clamp(0.0, 100.0) as u8),
            resets_at_epoch: window
                .get("resetsAt")
                .or_else(|| window.get("resets_at"))
                .and_then(|v| v.as_u64()),
            status: None,
        });
    }
    windows
}

/// Compact gauge label for a Codex rate-limit window duration.
pub(crate) fn codex_rate_limit_label(minutes: Option<u64>, bucket: &str) -> String {
    match minutes {
        Some(300) => "5h".to_string(),
        Some(10080) => "7d".to_string(),
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => bucket.to_string(),
    }
}

pub(crate) const CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT: usize = 3;
pub(crate) const CODEX_BUILD_PROGRESS_INLINE_LIMIT: usize = 4;
pub(crate) const CODEX_COMMAND_PREVIEW_LIMIT: usize = 700;
pub(crate) const CODEX_COMMAND_OUTPUT_LINE_LIMIT: usize = 1200;
pub(crate) const CODEX_COMMAND_OUTPUT_INLINE_LIMIT: usize = 8 * 1024;
pub(crate) const CODEX_COMMAND_OUTPUT_HEAD_LIMIT: usize = 4 * 1024;
pub(crate) const CODEX_COMMAND_OUTPUT_TAIL_LIMIT: usize = 2 * 1024;
pub(crate) const CODEX_COMMAND_SOURCE_OUTPUT_INLINE_LIMIT: usize = 4 * 1024;
pub(crate) const CODEX_COMMAND_SOURCE_OUTPUT_HEAD_LIMIT: usize = 2 * 1024;
pub(crate) const CODEX_COMMAND_SOURCE_OUTPUT_TAIL_LIMIT: usize = 2 * 1024;
pub(crate) const CODEX_COMMAND_SOURCE_OUTPUT_DETECTION_MIN_BYTES: usize = 2 * 1024;
pub(crate) const CODEX_COMMAND_SOURCE_OUTPUT_DETECTION_LINE_LIMIT: usize = 200;

#[derive(Default)]
pub(crate) struct CodexCommandOutputHygiene {
    pending: String,
    warning_diagnostics_seen: usize,
    suppressing_warning_diagnostic: bool,
    suppression_notice_emitted: bool,
    build_progress_seen: usize,
    build_progress_suppressed: usize,
    build_progress_notice_emitted: bool,
    last_suppressed_build_progress: String,
    source_seen_bytes: usize,
    filtered_seen_bytes: usize,
    emitted_head_bytes: usize,
    tail: String,
    omitting_large_output: bool,
    source_like: bool,
    source_signals: CodexCommandSourceSignals,
}

#[derive(Default)]
pub(crate) struct CodexCommandSourceSignals {
    observed_lines: usize,
    non_empty_lines: usize,
    code_like_lines: usize,
    markup_like_lines: usize,
    style_like_lines: usize,
    structural_lines: usize,
    source_hint_lines: usize,
}

impl CodexCommandOutputHygiene {
    pub(crate) fn observe_command(&mut self, command: &str) {
        if codex_command_likely_source_output(command) {
            self.source_like = true;
        }
    }

    pub(crate) fn filter(&mut self, text: &str, flush: bool) -> Option<String> {
        if text.is_empty() && !(flush && (!self.pending.is_empty() || self.omitting_large_output)) {
            return None;
        }

        let mut combined = String::new();
        if !self.pending.is_empty() {
            combined.push_str(&self.pending);
            self.pending.clear();
        }
        combined.push_str(text);
        let combined = normalize_codex_command_output_record_separators(&combined);
        self.source_seen_bytes = self.source_seen_bytes.saturating_add(combined.len());
        self.source_signals.observe(&combined);
        if self
            .source_signals
            .looks_like_large_source(self.source_seen_bytes)
        {
            self.source_like = true;
        }

        let mut out = String::new();
        let mut start = 0;
        for (idx, ch) in combined.char_indices() {
            if ch == '\n' {
                let end = idx + ch.len_utf8();
                self.push_filtered_line(&combined[start..end], &mut out);
                start = end;
            }
        }

        if start < combined.len() {
            let tail = &combined[start..];
            if flush || !self.should_buffer_potential_warning_tail(tail) {
                self.push_filtered_line(tail, &mut out);
            } else {
                self.pending.push_str(tail);
            }
        }
        if flush {
            self.push_build_progress_summary(&mut out);
        }

        if out.is_empty() {
            self.filter_large_output(String::new(), flush)
        } else {
            self.filter_large_output(out, flush)
        }
    }

    pub(crate) fn push_filtered_line(&mut self, line: &str, out: &mut String) {
        if is_codex_warning_diagnostic_start(line) {
            self.warning_diagnostics_seen += 1;
            if self.warning_diagnostics_seen > CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
                self.suppressing_warning_diagnostic = true;
                if !self.suppression_notice_emitted {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("[Intendant suppressed additional repeated warning diagnostics from Codex command output]\n");
                    self.suppression_notice_emitted = true;
                }
                return;
            }
            self.suppressing_warning_diagnostic = false;
            push_compact_codex_output_line(out, line);
            return;
        }

        if self.suppressing_warning_diagnostic {
            if is_codex_warning_diagnostic_continuation(line) {
                // Codex can replay only the source excerpt after Rust's blank
                // diagnostic separator. Keep suppression active until a real
                // non-continuation line arrives so split tails like `59 ` are
                // buffered and completed before classification.
                return;
            }
            self.suppressing_warning_diagnostic = false;
        }

        if self.warning_diagnostics_seen > CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT
            && is_codex_detached_warning_diagnostic_tail_start(line)
        {
            self.suppressing_warning_diagnostic = true;
            return;
        }

        if is_codex_build_progress_line(line) {
            self.build_progress_seen += 1;
            if self.build_progress_seen > CODEX_BUILD_PROGRESS_INLINE_LIMIT {
                self.build_progress_suppressed += 1;
                self.last_suppressed_build_progress = compact_codex_output_line(line);
                if !self.build_progress_notice_emitted {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("[Intendant suppressed repetitive build progress from Codex command output]\n");
                    self.build_progress_notice_emitted = true;
                }
                return;
            }
        }

        push_compact_codex_output_line(out, line);
    }

    pub(crate) fn push_build_progress_summary(&mut self, out: &mut String) {
        if self.build_progress_suppressed == 0 {
            return;
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        let last = self.last_suppressed_build_progress.trim();
        if last.is_empty() {
            out.push_str(&format!(
                "[Intendant suppressed {} repetitive build progress lines]\n",
                self.build_progress_suppressed
            ));
        } else {
            out.push_str(&format!(
                "[Intendant suppressed {} repetitive build progress lines; last: {}]\n",
                self.build_progress_suppressed, last
            ));
        }
        self.build_progress_suppressed = 0;
        self.last_suppressed_build_progress.clear();
    }

    pub(crate) fn filter_large_output(&mut self, text: String, flush: bool) -> Option<String> {
        if text.is_empty() && !(flush && self.omitting_large_output) {
            return None;
        }

        self.filtered_seen_bytes = self.filtered_seen_bytes.saturating_add(text.len());
        let mut out = String::new();
        if self.omitting_large_output {
            self.push_tail(&text);
        } else {
            let inline_limit = self.inline_limit();
            if self.filtered_seen_bytes <= inline_limit {
                self.emitted_head_bytes = self.emitted_head_bytes.saturating_add(text.len());
                out.push_str(&text);
            } else {
                let head_limit = self.head_limit();
                let remaining_for_head = head_limit.saturating_sub(self.emitted_head_bytes);
                let split_at = codex_char_boundary_at_or_before(&text, remaining_for_head);
                out.push_str(&text[..split_at]);
                self.emitted_head_bytes = self.emitted_head_bytes.saturating_add(split_at);
                self.omitting_large_output = true;
                self.push_tail(&text[split_at..]);
                out.push_str(&codex_command_output_omission_start_notice(
                    self.emitted_head_bytes,
                ));
            }
        }

        if flush && self.omitting_large_output {
            out.push_str(&self.finish_large_output());
        }

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub(crate) fn finish_large_output(&mut self) -> String {
        let tail = std::mem::take(&mut self.tail);
        let tail_bytes = tail.len();
        let omitted_middle_bytes = self
            .filtered_seen_bytes
            .saturating_sub(self.emitted_head_bytes)
            .saturating_sub(tail_bytes);
        self.omitting_large_output = false;
        let mut out = codex_command_output_omission_tail_notice(
            self.filtered_seen_bytes,
            self.emitted_head_bytes,
            tail_bytes,
            omitted_middle_bytes,
        );
        out.push_str(&tail);
        out
    }

    pub(crate) fn inline_limit(&self) -> usize {
        if self.source_like {
            CODEX_COMMAND_SOURCE_OUTPUT_INLINE_LIMIT
        } else {
            CODEX_COMMAND_OUTPUT_INLINE_LIMIT
        }
    }

    pub(crate) fn head_limit(&self) -> usize {
        if self.source_like {
            CODEX_COMMAND_SOURCE_OUTPUT_HEAD_LIMIT
        } else {
            CODEX_COMMAND_OUTPUT_HEAD_LIMIT
        }
    }

    pub(crate) fn tail_limit(&self) -> usize {
        if self.source_like {
            CODEX_COMMAND_SOURCE_OUTPUT_TAIL_LIMIT
        } else {
            CODEX_COMMAND_OUTPUT_TAIL_LIMIT
        }
    }

    pub(crate) fn push_tail(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.tail.push_str(text);
        let tail_limit = self.tail_limit();
        if self.tail.len() <= tail_limit {
            return;
        }
        let trim_to = self.tail.len().saturating_sub(tail_limit);
        let split_at = codex_char_boundary_at_or_after(&self.tail, trim_to);
        self.tail.drain(..split_at);
    }

    pub(crate) fn should_buffer_potential_warning_tail(&self, tail: &str) -> bool {
        self.suppressing_warning_diagnostic
            || is_potential_codex_warning_prefix(tail)
            || (self.warning_diagnostics_seen > CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT
                && is_potential_codex_detached_warning_diagnostic_tail_prefix(tail))
    }
}

impl CodexCommandSourceSignals {
    pub(crate) fn observe(&mut self, text: &str) {
        if text.is_empty()
            || self.observed_lines >= CODEX_COMMAND_SOURCE_OUTPUT_DETECTION_LINE_LIMIT
        {
            return;
        }

        for line in text.lines() {
            if self.observed_lines >= CODEX_COMMAND_SOURCE_OUTPUT_DETECTION_LINE_LIMIT {
                break;
            }
            self.observed_lines += 1;
            let trimmed = codex_command_output_strip_source_line_prefix(line.trim());
            if trimmed.is_empty() {
                continue;
            }

            self.non_empty_lines += 1;
            let code_like = codex_source_line_has_code_token(trimmed);
            let markup_like = codex_source_line_has_markup_token(trimmed);
            let style_like = codex_source_line_has_style_token(trimmed);
            if code_like {
                self.code_like_lines += 1;
            }
            if markup_like {
                self.markup_like_lines += 1;
            }
            if style_like {
                self.style_like_lines += 1;
            }
            if code_like || markup_like || style_like {
                self.source_hint_lines += 1;
            }
            if codex_source_line_has_structural_token(trimmed) {
                self.structural_lines += 1;
            }
        }
    }

    pub(crate) fn looks_like_large_source(&self, seen_bytes: usize) -> bool {
        if seen_bytes <= CODEX_COMMAND_SOURCE_OUTPUT_DETECTION_MIN_BYTES
            || self.non_empty_lines < 24
        {
            return false;
        }

        let code_density = self.code_like_lines * 100 / self.non_empty_lines;
        let hint_density = self.source_hint_lines * 100 / self.non_empty_lines;
        let structural_density = self.structural_lines * 100 / self.non_empty_lines;
        (self.code_like_lines >= 8 && self.structural_lines >= 8 && code_density >= 20)
            || (self.markup_like_lines >= 8 && self.structural_lines >= 8)
            || (self.style_like_lines >= 8 && self.structural_lines >= 16)
            || (self.source_hint_lines >= 16
                && self.structural_lines >= 16
                && hint_density >= 35
                && structural_density >= 35)
    }
}

pub(crate) fn compact_codex_command_preview(command: &str) -> String {
    let compact = command.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_middle_chars_with_notice(
        &compact,
        CODEX_COMMAND_PREVIEW_LIMIT,
        "long command preview",
    )
}

pub(crate) fn codex_command_likely_source_output(command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }

    codex_command_mentions_source_reader(command) && codex_command_mentions_code_path(command)
}

pub(crate) fn codex_command_mentions_source_reader(command: &str) -> bool {
    command
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .any(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "awk" | "cat" | "grep" | "head" | "nl" | "rg" | "ripgrep" | "sed" | "tail"
            )
        })
}

pub(crate) fn codex_command_mentions_code_path(command: &str) -> bool {
    const CODE_PATH_HINTS: &[&str] = &[
        ".c",
        ".cc",
        ".cjs",
        ".cpp",
        ".cs",
        ".css",
        ".go",
        ".h",
        ".hpp",
        ".html",
        ".java",
        ".js",
        ".json",
        ".jsx",
        ".kt",
        ".mjs",
        ".php",
        ".py",
        ".rb",
        ".rs",
        ".sass",
        ".scss",
        ".sh",
        ".sql",
        ".svelte",
        ".swift",
        ".toml",
        ".ts",
        ".tsx",
        ".vue",
        ".xml",
        ".yaml",
        ".yml",
        ".zsh",
        "app/",
        "crates/",
        "lib/",
        "packages/",
        "src/",
    ];

    command.split_whitespace().any(|token| {
        let token = token
            .trim_matches(|ch: char| {
                matches!(
                    ch,
                    '\'' | '"' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
                )
            })
            .trim_end_matches([':', '|']);
        let lower = token.to_ascii_lowercase();
        CODE_PATH_HINTS.iter().any(|hint| lower.contains(hint))
    })
}

pub(crate) fn normalize_codex_command_output_record_separators(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }

    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if !normalized.is_empty() && !normalized.ends_with('\n') {
                normalized.push('\n');
            }
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            normalized.push(ch);
        }
    }
    Cow::Owned(normalized)
}

pub(crate) fn push_compact_codex_output_line(out: &mut String, line: &str) {
    out.push_str(&compact_codex_output_line(line));
}

pub(crate) fn compact_codex_output_line(line: &str) -> String {
    let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    };
    let compact = truncate_middle_chars_with_notice(
        body,
        CODEX_COMMAND_OUTPUT_LINE_LIMIT,
        "long command-output line",
    );
    if ending.is_empty() {
        compact
    } else {
        format!("{compact}{ending}")
    }
}

pub(crate) fn truncate_middle_chars_with_notice(
    text: &str,
    max_chars: usize,
    label: &str,
) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }

    let mut omitted = total.saturating_sub(max_chars);
    let mut marker = String::new();
    let mut head = 0;
    let mut tail = 0;
    for _ in 0..4 {
        marker = format!(" ...[Intendant truncated {label}; {omitted} chars omitted]... ");
        let available = max_chars.saturating_sub(marker.chars().count());
        if available == 0 {
            return text.chars().take(max_chars).collect();
        }
        head = available.saturating_mul(3) / 5;
        tail = available.saturating_sub(head);
        let next_omitted = total.saturating_sub(head + tail);
        if next_omitted == omitted {
            break;
        }
        omitted = next_omitted;
    }

    let prefix: String = text.chars().take(head).collect();
    let suffix: String = text.chars().skip(total.saturating_sub(tail)).collect();
    format!("{prefix}{marker}{suffix}")
}

pub(crate) fn codex_command_output_omission_start_notice(shown_head_bytes: usize) -> String {
    format!(
        "\n\n[Intendant is omitting additional large command output; shown first {shown_head_bytes} bytes, final tail will be shown when the command completes]\n",
    )
}

pub(crate) fn codex_command_output_omission_tail_notice(
    total_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
    omitted_middle_bytes: usize,
) -> String {
    format!(
        "\n\n[Intendant omitted {omitted_middle_bytes} bytes from the middle of {total_bytes} bytes of command output; shown head {head_bytes} bytes, final tail {tail_bytes} bytes]\n",
    )
}

pub(crate) fn codex_command_output_strip_source_line_prefix(line: &str) -> &str {
    let Some(rest) = codex_command_output_strip_numeric_line_prefix(line) else {
        return codex_command_output_strip_path_line_prefix(line).unwrap_or(line);
    };
    rest
}

pub(crate) fn codex_command_output_strip_path_line_prefix(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let prefix = &line[..colon];
    if !(prefix.contains('/') || prefix.contains('.') || prefix.contains('\\')) {
        return None;
    }
    let rest = &line[colon + 1..];
    codex_command_output_strip_numeric_line_prefix(rest)
}

pub(crate) fn codex_command_output_strip_numeric_line_prefix(line: &str) -> Option<&str> {
    let digit_count = line.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 || digit_count > 8 || digit_count >= line.len() {
        return None;
    }
    let separator = line.as_bytes()[digit_count];
    if !matches!(separator, b':' | b'\t' | b' ') {
        return None;
    }
    Some(line[digit_count + 1..].trim_start())
}

pub(crate) fn is_codex_build_progress_line(line: &str) -> bool {
    let trimmed = trim_codex_output_classification_prefix(line);
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("warning:")
        || lower.starts_with("warn:")
        || lower.starts_with("error:")
        || lower.starts_with("finished ")
    {
        return false;
    }
    if let Some(first) = trimmed.split_whitespace().next() {
        if matches!(
            first,
            "Adding"
                | "Building"
                | "Checking"
                | "Compiling"
                | "Downloaded"
                | "Downloading"
                | "Fetching"
                | "Fresh"
                | "Installing"
                | "Locking"
                | "Updating"
        ) {
            return true;
        }
    }
    trimmed.starts_with("[INFO]:")
        && (lower.contains("compiling")
            || lower.contains("checking")
            || lower.contains("installing wasm-bindgen"))
}

pub(crate) fn trim_codex_output_classification_prefix(mut line: &str) -> &str {
    line = line.trim_start();
    loop {
        let Some(after_escape) = line.strip_prefix('\u{1b}') else {
            return line;
        };
        let Some(after_csi) = after_escape.strip_prefix('[') else {
            return line;
        };
        let Some((idx, ch)) = after_csi
            .char_indices()
            .find(|(_, ch)| matches!(ch, '@'..='~'))
        else {
            return line;
        };
        line = after_csi[idx + ch.len_utf8()..].trim_start();
    }
}

pub(crate) fn codex_source_line_has_code_token(line: &str) -> bool {
    const TOKENS: &[&str] = &[
        "fn ",
        "impl ",
        "pub ",
        "struct ",
        "enum ",
        "use ",
        "mod ",
        "let ",
        "const ",
        "static ",
        "async ",
        "await",
        "match ",
        "if ",
        "else",
        "for ",
        "while ",
        "return ",
        "function ",
        "class ",
        "import ",
        "export ",
        "type ",
        "interface ",
        "var ",
        "=>",
        "document.",
        "window.",
        "querySelector",
        "addEventListener",
    ];
    TOKENS.iter().any(|token| line.contains(token))
}

pub(crate) fn codex_source_line_has_markup_token(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('<') && trimmed.contains('>') {
        return true;
    }
    trimmed.contains("</")
        || trimmed.contains("<div")
        || trimmed.contains("<span")
        || trimmed.contains("<button")
        || trimmed.contains("<script")
        || trimmed.contains("<style")
        || trimmed.contains("class=")
        || trimmed.contains(" id=")
}

pub(crate) fn codex_source_line_has_style_token(line: &str) -> bool {
    let trimmed = line.trim_start();
    (trimmed.contains(':') && trimmed.ends_with(';'))
        || (trimmed.ends_with('{')
            && (trimmed.starts_with('.')
                || trimmed.starts_with('#')
                || trimmed.starts_with('@')
                || trimmed.starts_with(":root")
                || trimmed.contains(" .")
                || trimmed.contains(" #")
                || trimmed.contains(" {")))
}

pub(crate) fn codex_source_line_has_structural_token(line: &str) -> bool {
    line.contains('{')
        || line.contains('}')
        || line.ends_with(';')
        || line.ends_with(',')
        || line.contains("=>")
        || (line.contains('<') && line.contains('>'))
}

pub(crate) fn codex_char_boundary_at_or_before(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub(crate) fn codex_char_boundary_at_or_after(text: &str, min_bytes: usize) -> usize {
    if min_bytes >= text.len() {
        return text.len();
    }
    let mut idx = min_bytes;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub(crate) fn codex_command_output_hygiene_key(item_id: &str) -> String {
    if item_id.is_empty() {
        "<unknown>".to_string()
    } else {
        item_id.to_string()
    }
}

pub(crate) fn filter_codex_command_output(
    state: &mut CodexNotificationState,
    item_id: &str,
    text: &str,
    flush: bool,
) -> Option<String> {
    let normalized = strip_codex_tool_output_envelope(text);
    let key = codex_command_output_hygiene_key(item_id);
    state
        .command_output_hygiene
        .entry(key)
        .or_default()
        .filter(&normalized, flush)
}

pub(crate) fn observe_codex_command_output_command(
    state: &mut CodexNotificationState,
    item_id: &str,
    command: &str,
) {
    let key = codex_command_output_hygiene_key(item_id);
    state
        .command_output_hygiene
        .entry(key)
        .or_default()
        .observe_command(command);
}

pub(crate) fn finish_codex_command_output(
    state: &mut CodexNotificationState,
    item_id: &str,
) -> Option<String> {
    let key = codex_command_output_hygiene_key(item_id);
    let mut hygiene = state.command_output_hygiene.remove(&key)?;
    hygiene.filter("", true)
}

pub(crate) fn strip_codex_tool_output_envelope(text: &str) -> String {
    let Some(first_end) = next_line_end(text, 0) else {
        return text.to_string();
    };
    let first = trim_line_ending(&text[..first_end]);
    if !first.starts_with("Chunk ID:") {
        return text.to_string();
    }

    let mut pos = first_end;
    let mut saw_metadata = false;
    while let Some(end) = next_line_end(text, pos) {
        let line = trim_line_ending(&text[pos..end]);
        if line == "Output:" {
            pos = end;
            return strip_codex_tool_output_body_preamble(&text[pos..]).to_string();
        }
        if is_codex_tool_output_envelope_metadata_line(line) {
            saw_metadata = true;
            pos = end;
            continue;
        }
        break;
    }

    if saw_metadata && text[pos..].trim().is_empty() {
        String::new()
    } else {
        text.to_string()
    }
}

pub(crate) fn next_line_end(text: &str, start: usize) -> Option<usize> {
    if start >= text.len() {
        return None;
    }
    text[start..]
        .find('\n')
        .map(|idx| start + idx + 1)
        .or(Some(text.len()))
}

pub(crate) fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

pub(crate) fn is_codex_tool_output_envelope_metadata_line(line: &str) -> bool {
    line.starts_with("Wall time:")
        || line.starts_with("Process running with session ID")
        || line.starts_with("Process exited with code")
        || line.starts_with("Process killed")
        || line.starts_with("Process timed out")
        || line.starts_with("Original token count:")
}

pub(crate) fn strip_codex_tool_output_body_preamble(mut body: &str) -> &str {
    if let Some(end) = next_line_end(body, 0) {
        let line = trim_line_ending(&body[..end]);
        if line.starts_with("Total output lines:")
            && line["Total output lines:".len()..]
                .trim()
                .chars()
                .all(|ch| ch.is_ascii_digit())
        {
            body = &body[end..];
            if let Some(blank_end) = next_line_end(body, 0) {
                if trim_line_ending(&body[..blank_end]).trim().is_empty() {
                    body = &body[blank_end..];
                }
            }
        }
    }
    body
}

pub(crate) fn is_codex_warning_diagnostic_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("warning:") || lower.starts_with("warn:")
}

pub(crate) fn is_potential_codex_warning_prefix(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.len() >= "warning:".len() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    "warning:".starts_with(&lower) || "warn:".starts_with(&lower)
}

pub(crate) fn is_codex_warning_diagnostic_continuation(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty()
        || trimmed.starts_with("-->")
        || trimmed.starts_with('|')
        || trimmed.starts_with('=')
        || trimmed.starts_with("...")
        || trimmed.starts_with(":::")
        || is_codex_warning_diagnostic_source_excerpt(trimmed)
        || trimmed.to_ascii_lowercase().starts_with("note:")
        || trimmed.to_ascii_lowercase().starts_with("help:")
}

pub(crate) fn is_codex_detached_warning_diagnostic_tail_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("-->") || is_codex_warning_diagnostic_source_excerpt(trimmed)
}

pub(crate) fn is_potential_codex_detached_warning_diagnostic_tail_prefix(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("-->") || "-->".starts_with(trimmed) {
        return true;
    }

    let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return false;
    }
    if digit_count >= trimmed.len() {
        return true;
    }

    let rest = trimmed[digit_count..].trim_start();
    rest.is_empty() || rest.starts_with('|')
}

pub(crate) fn is_codex_warning_diagnostic_source_excerpt(trimmed: &str) -> bool {
    let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 || digit_count >= trimmed.len() {
        return false;
    }
    trimmed[digit_count..].trim_start().starts_with('|')
}

pub(crate) fn codex_backend_error_event(
    params: &serde_json::Value,
    latest_usage: Option<&AgentUsageSnapshot>,
) -> Option<AgentEvent> {
    let error = params.get("error")?;
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Codex backend error")
        .to_string();
    let details = error
        .get("additionalDetails")
        .or_else(|| error.get("additional_details"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let code = error
        .get("codexErrorInfo")
        .or_else(|| error.get("codex_error_info"))
        .and_then(codex_error_info_label);
    let will_retry = params
        .get("willRetry")
        .or_else(|| params.get("will_retry"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let likely_generation_starvation = !will_retry
        && codex_error_near_context_limit(
            &message,
            details.as_deref(),
            code.as_deref(),
            latest_usage,
        );
    let recovery_hint =
        likely_generation_starvation.then(|| GENERATION_STARVATION_HINT.to_string());

    Some(AgentEvent::BackendError {
        message,
        code,
        details,
        will_retry,
        likely_generation_starvation,
        recovery_hint,
    })
}

pub(crate) fn codex_error_info_label(value: &serde_json::Value) -> Option<String> {
    if let Some(label) = value.as_str() {
        return Some(label.to_string());
    }
    value
        .as_object()
        .and_then(|object| object.keys().next().cloned())
}

pub(crate) fn codex_error_near_context_limit(
    message: &str,
    details: Option<&str>,
    code: Option<&str>,
    latest_usage: Option<&AgentUsageSnapshot>,
) -> bool {
    let mut text = message.to_ascii_lowercase();
    if let Some(details) = details {
        text.push('\n');
        text.push_str(&details.to_ascii_lowercase());
    }
    let incomplete = text.contains("incomplete response returned")
        || text.contains("response.incomplete")
        || text.contains("incomplete_details");
    let context_limit = text.contains("context window")
        || text.contains("context length")
        || text.contains("maximum context")
        || matches!(code, Some("contextWindowExceeded"));
    if context_limit {
        return true;
    }

    let at_reported_limit = latest_usage
        .is_some_and(|usage| usage.context_window > 0 && usage.tokens_used >= usage.context_window);
    if !at_reported_limit {
        return false;
    }

    let terminal_stream_failure = matches!(
        code,
        Some("responseStreamDisconnected" | "responseTooManyFailedAttempts")
    );

    incomplete || terminal_stream_failure
}

pub(crate) fn is_codex_noop_tool_wait_message(text: &str) -> bool {
    let normalized = text
        .trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .to_ascii_lowercase();
    if normalized.is_empty() || normalized.len() > 240 {
        return false;
    }

    let material_markers = [
        "failed",
        "failure",
        "error:",
        "completed",
        "finished",
        "succeeded",
        "success",
        "done",
        "next",
        "found",
        "changed",
        "fixed",
    ];
    if material_markers
        .iter()
        .any(|needle| normalized.contains(needle))
    {
        return false;
    }

    let standalone_wait_status = [
        "polling",
        "still active",
        "still polling",
        "still running",
        "still waiting",
        "waiting",
        "awaiting output",
        "waiting for output",
        "polling for output",
        "ongoing",
        "in progress",
    ]
    .iter()
    .any(|status| normalized == *status);
    if standalone_wait_status {
        return true;
    }

    let no_new_output_status = [
        "no output",
        "no new output",
        "nothing new",
        "no update",
        "no updates",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if no_new_output_status && normalized.split_whitespace().count() <= 6 {
        return true;
    }

    let waiting = [
        "still",
        "waiting",
        "awaiting",
        "continuing",
        "ongoing",
        "in progress",
        "running",
        "building",
        "compiling",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if !waiting {
        return false;
    }

    let tool_context = [
        "tool",
        "command",
        "process",
        "build",
        "building",
        "compile",
        "compiling",
        "cargo",
        "test",
        "tests",
        "check",
        "release",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if !tool_context {
        return false;
    }

    if normalized.split_whitespace().count() <= 12 {
        return true;
    }

    let no_material_output = [
        "no output",
        "no new output",
        "no error output",
        "no errors",
        "no error",
        "nothing new",
        "no update",
        "no updates",
        "quiet",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if !no_material_output {
        return false;
    }

    true
}

/// Translate a Codex notification into one or more `AgentEvent`s.
#[cfg(test)]
pub(crate) fn translate_notification(
    method: &str,
    params: &serde_json::Value,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let mut state = CodexNotificationState::default();
    translate_notification_with_state(method, params, event_tx, &mut state);
}

#[cfg(test)]
pub(crate) fn translate_notification_with_state(
    method: &str,
    params: &serde_json::Value,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    state: &mut CodexNotificationState,
) {
    translate_notification_with_scope(method, params, event_tx, state, None, None);
}

pub(crate) fn codex_item_event_id<'a>(
    params: &'a serde_json::Value,
    item: &'a serde_json::Value,
) -> Option<&'a str> {
    [
        item.get("id"),
        item.get("call_id"),
        item.get("callId"),
        params.get("itemId"),
        params.get("call_id"),
        params.get("callId"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|value| value.as_str().map(str::trim))
    .find(|value| !value.is_empty())
}

pub(crate) fn translate_notification_with_scope(
    method: &str,
    params: &serde_json::Value,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    state: &mut CodexNotificationState,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
) {
    match method {
        "error" => {
            if let Some(event) = codex_backend_error_event(params, state.latest_usage.as_ref()) {
                send_scoped_agent_event(event_tx, thread_id, turn_id, event);
            }
        }
        "item/agentMessage/delta" => {
            let text = params
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if is_codex_noop_tool_wait_message(&text) {
                return;
            }
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::MessageDelta { text },
            );
        }

        "item/started" => {
            let item = params.get("item").unwrap_or(params);
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let item_id = codex_item_event_id(params, item)
                .unwrap_or_default()
                .to_string();

            match item_type {
                "commandExecution" => {
                    let command = params
                        .pointer("/item/command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    observe_codex_command_output_command(state, &item_id, &command);
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::ToolStarted {
                            item_id,
                            tool_name: "command".to_string(),
                            preview: compact_codex_command_preview(&command),
                        },
                    );
                }
                "fileChange" => {
                    // Codex can emit a fileChange item before the concrete
                    // path metadata is attached. Avoid showing a blank
                    // "file_change:" activity row; the filesystem watcher
                    // will still report the actual changed files.
                    if let Some(preview) = codex_file_change_preview(params) {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::ToolStarted {
                                item_id,
                                tool_name: "file_change".to_string(),
                                preview,
                            },
                        );
                    }
                }
                "agentMessage" | "userMessage" | "reasoning" | "imageView" => {
                    // agentMessage: deltas will follow via item/agentMessage/delta.
                    // userMessage: final text normally arrives on item/completed.
                    // reasoning: model reasoning trace; nothing to emit.
                    // imageView: Codex UI bookkeeping, not a tool.
                }
                "contextCompaction" => {
                    let detail = if item_id.is_empty() {
                        "Codex compacted context".to_string()
                    } else {
                        format!("Codex compacted context ({item_id})")
                    };
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::Log {
                            level: "info".to_string(),
                            message: detail,
                        },
                    );
                }
                "mcpToolCall" => {
                    // Codex is calling an MCP tool (e.g. spawn_live_audio, take_screenshot).
                    // `/item/tool` is the current app-server v2 wire field; the
                    // others cover older payload shapes. Getting the real name
                    // matters beyond cosmetics: the managed-context rewind-only
                    // and density tool gates match the preview against the
                    // recovery-tool allowlist, and an anonymous "mcp_tool"
                    // fallback would block-and-interrupt the very recovery
                    // tools (get_status, list_rewind_anchors, rewind_context,
                    // ...) the model needs under pressure.
                    let tool_name = params
                        .pointer("/item/tool")
                        .or_else(|| params.pointer("/item/name"))
                        .or_else(|| params.pointer("/item/toolName"))
                        .or_else(|| params.pointer("/item/serverLabel"))
                        .or_else(|| params.pointer("/item/arguments/name"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("mcp_tool")
                        .to_string();
                    let server = params
                        .pointer("/item/serverName")
                        .or_else(|| params.pointer("/item/server"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let preview = if server.is_empty() {
                        tool_name.clone()
                    } else {
                        format!("{}:{}", server, tool_name)
                    };
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::ToolStarted {
                            item_id,
                            tool_name: "mcp".to_string(),
                            preview,
                        },
                    );
                }
                "webSearch" => {
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::ToolStarted {
                            item_id,
                            tool_name: "web_search".to_string(),
                            preview: codex_web_search_preview(params),
                        },
                    );
                }
                "collabAgentToolCall" => {
                    if let Some(event) = codex_collab_agent_tool_call(params) {
                        send_scoped_agent_event(event_tx, thread_id, turn_id, event);
                    }
                }
                other => {
                    eprintln!("[codex] unknown item type in item/started: {:?}", other);
                }
            }
        }

        "item/commandExecution/outputDelta" => {
            let item_id = params
                .get("itemId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_text = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(text) = filter_codex_command_output(state, &item_id, raw_text, false) {
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::ToolOutputDelta { item_id, text },
                );
            }
        }

        "item/completed" => {
            let item = params.get("item").unwrap_or(params);
            let item_id = codex_item_event_id(params, item)
                .unwrap_or_default()
                .to_string();
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // Reasoning items: surface the chain-of-thought text via a
            // dedicated event so it renders at "detail" verbosity (Verbose +
            // Debug). Skip the ToolCompleted marker — reasoning is not a tool.
            if item_type == "reasoning" {
                if let Some(text) = extract_reasoning_text(item) {
                    if !text.is_empty() {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::Reasoning { text },
                        );
                    }
                }
                return;
            }

            // agentMessage items: content arrives via either streaming deltas
            // (item/agentMessage/delta → Message) or the completed item's
            // text field. Emit Message on completion if the deltas didn't
            // already produce one. Skip the ToolCompleted marker — the
            // final message is not a tool.
            if item_type == "agentMessage" {
                let text = item.get("text").and_then(|v| v.as_str());
                if text.is_some_and(is_codex_noop_tool_wait_message) {
                    if codex_item_completed_final_answer(params) {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::TurnCompleted { message: None },
                        );
                    }
                    return;
                }
                if let Some(text) = text {
                    if !text.is_empty() {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::Message {
                                text: text.to_string(),
                            },
                        );
                    }
                }
                if codex_item_completed_final_answer(params) {
                    let message = text.map(str::to_string).filter(|text| !text.is_empty());
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::TurnCompleted { message },
                    );
                }
                return;
            }

            if item_type == "userMessage" {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::UserMessage {
                                text: text.to_string(),
                            },
                        );
                    }
                }
                return;
            }

            if item_type == "collabAgentToolCall" {
                if let Some(event) = codex_collab_agent_tool_call(item) {
                    send_scoped_agent_event(event_tx, thread_id, turn_id, event);
                }
                return;
            }

            // The remaining types are Codex UI/bookkeeping records, not tools.
            if matches!(item_type, "contextCompaction" | "imageView") {
                return;
            }

            // Extract command output from commandExecution items
            if item_type == "commandExecution" {
                if let Some(command) = item.get("command").and_then(|v| v.as_str()) {
                    observe_codex_command_output_command(state, &item_id, command);
                }
                if let Some(output) = item.get("aggregatedOutput").and_then(|v| v.as_str()) {
                    if let Some(text) = filter_codex_command_output(state, &item_id, output, true) {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::ToolOutputDelta {
                                item_id: item_id.clone(),
                                text,
                            },
                        );
                    }
                    state
                        .command_output_hygiene
                        .remove(&codex_command_output_hygiene_key(&item_id));
                } else if let Some(text) = finish_codex_command_output(state, &item_id) {
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::ToolOutputDelta {
                            item_id: item_id.clone(),
                            text,
                        },
                    );
                }
            }

            if item_type == "function_call_output" {
                if let Some(output) = item.get("output").and_then(|v| v.as_str()) {
                    if let Some(text) = filter_codex_command_output(state, &item_id, output, true) {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::ToolOutputDelta {
                                item_id: item_id.clone(),
                                text,
                            },
                        );
                    }
                    state
                        .command_output_hygiene
                        .remove(&codex_command_output_hygiene_key(&item_id));
                }
            }

            // Extract MCP tool call results
            if item_type == "mcpToolCall" {
                // MCP results may contain structured data; surface as output
                if let Some(result) = item.get("result") {
                    let text = codex_mcp_tool_result_text(result);
                    if !text.is_empty() {
                        send_scoped_agent_event(
                            event_tx,
                            thread_id,
                            turn_id,
                            AgentEvent::ToolOutputDelta {
                                item_id: item_id.clone(),
                                text,
                            },
                        );
                    }
                }
            }

            let status_str = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let status = match status_str {
                "failed" => {
                    let message = extract_failure_message(item);
                    ToolCompletionStatus::Failed { message }
                }
                "cancelled" => ToolCompletionStatus::Cancelled,
                _ => ToolCompletionStatus::Success,
            };
            if item_type == "commandExecution" {
                state
                    .command_output_hygiene
                    .remove(&codex_command_output_hygiene_key(&item_id));
            }
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::ToolCompleted { item_id, status },
            );
        }

        "turn/completed" => {
            let message = params
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::TurnCompleted { message },
            );
        }

        // Interrupted and failed turns terminate WITHOUT a `turn/completed`.
        // They must still complete the drain: the terminal-observation dedup
        // upstream marks the turn terminal on these methods and from then on
        // suppresses the `thread/status/changed: idle` fallback, so a missing
        // arm here strands the session in a running/thinking phase forever
        // (stale dashboard status, follow-ups misrouted as steers).
        "turn/interrupted" | "turn/failed" => {
            if method == "turn/failed" {
                let message = params
                    .pointer("/error/message")
                    .or_else(|| params.get("message"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("codex reported the turn as failed")
                    .to_string();
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::Log {
                        level: "error".to_string(),
                        message: format!("Codex turn failed: {message}"),
                    },
                );
            }
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::TurnCompleted { message: None },
            );
        }

        "turn/diff/updated" => {
            let unified_diff = params
                .get("diff")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let files_changed = params
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::DiffUpdated {
                    files_changed,
                    unified_diff,
                },
            );
        }

        "turn/plan/updated" => {
            let entries = codex_plan_entries(params);
            if !entries.is_empty() {
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::PlanUpdate { entries },
                );
            }
        }

        "thread/goal/updated" => {
            let goal = params.get("goal").unwrap_or(params);
            if goal.is_null() {
                if state.goal_known_active {
                    send_scoped_agent_event(event_tx, thread_id, turn_id, AgentEvent::GoalCleared);
                }
                state.goal_known_active = false;
                return;
            }
            // Codex refreshes active goal metadata frequently. Keep those
            // updates structured-only so normal activity logs do not fill with
            // status churn.
            if let Some(goal) = session_goal_from_value(goal) {
                state.goal_known_active = true;
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::GoalUpdated { goal },
                );
            }
        }

        "thread/goal/cleared" => {
            if state.goal_known_active {
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::Log {
                        level: "info".to_string(),
                        message: "Codex goal cleared".to_string(),
                    },
                );
                send_scoped_agent_event(event_tx, thread_id, turn_id, AgentEvent::GoalCleared);
            }
            state.goal_known_active = false;
        }

        "thread/name/updated" => {
            let name = params
                .get("threadName")
                .or_else(|| params.get("thread_name"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("<unnamed>");
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::Log {
                    level: "info".to_string(),
                    message: format!("Codex thread renamed: {}", name),
                },
            );
        }

        "thread/compacted" => {
            let compacted_turn_id = params
                .get("turnId")
                .or_else(|| params.get("turn_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let message = if compacted_turn_id.is_empty() {
                "Codex compacted context".to_string()
            } else {
                format!("Codex compacted context for turn {compacted_turn_id}")
            };
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::Log {
                    level: "info".to_string(),
                    message,
                },
            );
        }

        // Warnings carry user-relevant state (e.g. the managed-context
        // recovery turn announcement and its step-limit bailout). Surface
        // them as warn-level logs instead of dropping them as unknown.
        "warning" => {
            let message = params
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("<empty warning>");
            send_scoped_agent_event(
                event_tx,
                thread_id,
                turn_id,
                AgentEvent::Log {
                    level: "warn".to_string(),
                    message: format!("Codex warning: {message}"),
                },
            );
        }

        // Informational Codex v2 notifications — no action needed.
        // `serverRequest/resolved` is bookkeeping for server-initiated
        // requests the app server answered itself.
        "turn/started"
        | "thread/started"
        | "thread/closed"
        | "thread/tokenUsage/updated"
        | "account/rateLimits/updated"
        | "item/commandExecution/terminalInteraction"
        | "configWarning"
        | "serverRequest/resolved"
        | "remoteControl/status/changed" => {}

        "thread/settings/updated" => {
            if let Some(cwd) = codex_thread_settings_cwd(params) {
                send_scoped_agent_event(
                    event_tx,
                    thread_id,
                    turn_id,
                    AgentEvent::Log {
                        level: "info".to_string(),
                        message: format!("Codex thread settings applied: cwd {cwd}"),
                    },
                );
            }
        }

        "mcpServer/startupStatus/updated" => {
            let status = params.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(error) = params.get("error").and_then(|v| v.as_str()) {
                if !error.is_empty() {
                    eprintln!("[codex] MCP server '{}' {}: {}", name, status, error);
                }
            }
        }

        // thread/status/changed may signal turn or thread completion.
        // Codex v2 uses this alongside (or instead of) turn/completed.
        "thread/status/changed" => {
            if let Some(status) = codex_thread_status_type(params) {
                if status == "completed" || status == "idle" {
                    send_scoped_agent_event(
                        event_tx,
                        thread_id,
                        turn_id,
                        AgentEvent::TurnCompleted { message: None },
                    );
                }
            }
        }

        // codex-cli 0.142+ announces changes to its skills catalog on every
        // app-server spawn. Intendant doesn't consume the catalog; ignore the
        // notification instead of logging it as unknown.
        "skills/changed" => {}

        other => {
            eprintln!(
                "[codex] unknown notification method: {:?} params: {}",
                other,
                serde_json::to_string(params).unwrap_or_default()
            );
        }
    }
}

pub(crate) fn codex_mcp_tool_result_text(result: &serde_json::Value) -> String {
    let sanitized = sanitize_codex_mcp_tool_result_for_text(result);
    if let Some(s) = sanitized.as_str() {
        s.to_string()
    } else {
        serde_json::to_string_pretty(&sanitized).unwrap_or_default()
    }
}

pub(crate) fn sanitize_codex_mcp_tool_result_for_text(
    value: &serde_json::Value,
) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(sanitize_codex_mcp_tool_result_for_text)
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            if codex_mcp_result_object_is_image(map) {
                let mut out = serde_json::Map::new();
                if let Some(value) = map.get("type") {
                    out.insert("type".to_string(), value.clone());
                } else {
                    out.insert(
                        "type".to_string(),
                        serde_json::Value::String("image".to_string()),
                    );
                }
                if let Some(value) = map.get("mimeType").or_else(|| map.get("mime_type")) {
                    out.insert("mimeType".to_string(), value.clone());
                }
                if let Some(value) = map.get("screenshot_path").or_else(|| map.get("path")) {
                    out.insert("artifact_path".to_string(), value.clone());
                }
                for key in ["data", "image_url", "imageUrl"] {
                    if let Some(bytes) = map
                        .get(key)
                        .and_then(|value| value.as_str())
                        .map(|value| value.len())
                    {
                        out.insert(format!("{key}_omitted_bytes"), serde_json::json!(bytes));
                    }
                }
                out.insert(
                    "image_content".to_string(),
                    serde_json::Value::String("omitted_for_intendant_text_history".to_string()),
                );
                return serde_json::Value::Object(out);
            }

            let sanitized = map
                .iter()
                .map(|(key, value)| (key.clone(), sanitize_codex_mcp_tool_result_for_text(value)))
                .collect();
            serde_json::Value::Object(sanitized)
        }
        _ => value.clone(),
    }
}

pub(crate) fn codex_mcp_result_object_is_image(
    map: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let type_text = map
        .get("type")
        .or_else(|| map.get("mimeType"))
        .or_else(|| map.get("mime_type"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    type_text.contains("image")
        || map
            .get("image_url")
            .or_else(|| map.get("imageUrl"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.starts_with("data:image/"))
}

/// Build a failure message for a Codex `item/completed` item with
/// `status: "failed"`. Codex fills `error` for MCP tool faults and internal
/// failures, but for `commandExecution` items that ran to completion with a
/// non-zero exit it omits `error` — the diagnostic sits in `aggregatedOutput`
/// and `exitCode` instead. Prefer the structured `error` when present, else
/// synthesize something informative so downstream logs don't read
/// "unknown error" next to a real Python traceback.
pub(crate) fn extract_failure_message(item: &serde_json::Value) -> String {
    if let Some(err) = item.get("error") {
        match err {
            serde_json::Value::String(s) if !s.is_empty() => return s.clone(),
            serde_json::Value::Object(obj) => {
                if let Some(s) = obj.get("message").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            serde_json::Value::Null => {}
            other => return other.to_string(),
        }
    }

    let exit_code = item
        .get("exitCode")
        .and_then(|v| v.as_i64())
        .or_else(|| item.get("exit_code").and_then(|v| v.as_i64()));
    let output_tail = item
        .get("aggregatedOutput")
        .and_then(|v| v.as_str())
        .map(|s| {
            let trimmed = s.trim_end();
            const MAX: usize = 400;
            if trimmed.chars().count() > MAX {
                let start = trimmed.chars().count() - MAX;
                let tail: String = trimmed.chars().skip(start).collect();
                format!("…{}", tail)
            } else {
                trimmed.to_string()
            }
        })
        .filter(|s| !s.is_empty());

    match (exit_code, output_tail) {
        (Some(code), Some(tail)) => format!("command exited {}: {}", code, tail),
        (Some(code), None) => format!("command exited {} (no output)", code),
        (None, Some(tail)) => tail,
        (None, None) => "unknown error".to_string(),
    }
}

/// Extract the chain-of-thought text from a Codex `reasoning` item.
///
/// Codex v2 wraps the OpenAI Responses API reasoning shape, which has
/// historically varied: `text` (single string), `summary` (array of
/// `{type: "summary_text", text: "..."}` entries), or `content` (similar
/// array). Walk all three and concatenate whatever we find.
pub(crate) fn extract_reasoning_text(item: &serde_json::Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    }

    for key in ["summary", "content"] {
        if let Some(arr) = item.get(key).and_then(|v| v.as_array()) {
            for entry in arr {
                if let Some(s) = entry.as_str() {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                } else if let Some(s) = entry.get("text").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
            }
        } else if let Some(s) = item.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// ExternalAgent trait implementation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_approval_predicate_covers_every_mcp_shape() {
        // Reader classification and resolve_approval response shape share
        // this predicate; if any MCP-family method escapes it, the request
        // gets a {"decision"} answer Codex ignores and the call hangs.
        assert!(is_codex_mcp_approval_method(
            "item/mcpToolCall/requestApproval"
        ));
        assert!(is_codex_mcp_approval_method(
            "mcpServer/tool/requestApproval"
        ));
        assert!(is_codex_mcp_approval_method("elicitation/create"));
        assert!(!is_codex_mcp_approval_method(
            "item/commandExecution/requestApproval"
        ));
        assert!(!is_codex_mcp_approval_method(
            "item/fileChange/requestApproval"
        ));
        assert!(!is_codex_mcp_approval_method(
            "item/permissions/requestApproval"
        ));
    }

    #[test]
    fn codex_mcp_tool_result_text_omits_image_payload_blocks() {
        let base64 = "a".repeat(4096);
        let result = serde_json::json!({
            "content": [
                {
                    "type": "text",
                    "text": "{\"status\":\"screenshot captured\",\"screenshot_path\":\"/tmp/shot.png\",\"width\":1200,\"height\":800}"
                },
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "data": base64
                }
            ],
            "isError": false
        });

        let text = codex_mcp_tool_result_text(&result);
        assert!(text.contains("/tmp/shot.png"));
        assert!(text.contains("omitted_for_intendant_text_history"));
        assert!(text.contains("data_omitted_bytes"));
        assert!(!text.contains(&"a".repeat(1024)));
    }

    #[test]
    fn translate_agent_message_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"delta": "Hello world"});
        translate_notification("item/agentMessage/delta", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::MessageDelta { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected MessageDelta, got {:?}", other),
        }
    }

    #[test]
    fn translate_agent_message_delta_suppresses_tool_wait_chatter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"delta": "The build is still running..."});
        translate_notification("item/agentMessage/delta", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "streaming wait chatter should not leave the Codex adapter"
        );
    }

    #[test]
    fn translate_agent_message_delta_keeps_material_progress() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params =
            serde_json::json!({"delta": "The cargo check failed with a trait bound error."});
        translate_notification("item/agentMessage/delta", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::MessageDelta { text } => {
                assert_eq!(text, "The cargo check failed with a trait bound error.")
            }
            other => panic!("expected MessageDelta, got {:?}", other),
        }
    }

    #[test]
    fn codex_rate_limit_windows_parse_app_server_shape() {
        // App-server v2 wire shape (camelCase; snake_case tolerated).
        let params = serde_json::json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 34, "windowDurationMins": 300, "resetsAt": 1783300000},
                "secondary": {"usedPercent": 12, "windowDurationMins": 10080}
            }
        });
        let windows = codex_rate_limit_windows(&params);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].used_pct, Some(34));
        assert_eq!(windows[0].resets_at_epoch, Some(1_783_300_000));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].used_pct, Some(12));
        assert_eq!(windows[1].resets_at_epoch, None);

        let snake = serde_json::json!({
            "rate_limits": {
                "primary": {"used_percent": 91.4, "window_minutes": 60}
            }
        });
        let windows = codex_rate_limit_windows(&snake);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "1h");
        assert_eq!(windows[0].used_pct, Some(91));

        assert!(codex_rate_limit_windows(&serde_json::json!({})).is_empty());
        assert_eq!(codex_rate_limit_label(Some(2880), "primary"), "2d");
        assert_eq!(codex_rate_limit_label(Some(45), "primary"), "45m");
        assert_eq!(codex_rate_limit_label(None, "secondary"), "secondary");
    }

    #[test]
    fn translate_item_started_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-1",
            "item": {"type": "commandExecution", "command": "ls -la"}
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(tool_name, "command");
                assert_eq!(preview, "ls -la");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_command_compacts_long_preview() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let command = format!(
            "node scripts/validate-dashboard.cjs --wait-for-function '{}' --selector .target-button",
            "document.body && ".repeat(120)
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "item": {"type": "commandExecution", "command": command}
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted { preview, .. } => {
                assert!(preview.contains("node scripts/validate-dashboard.cjs"));
                assert!(preview.contains(".target-button"));
                assert!(preview.contains("truncated long command preview"));
                assert!(preview.chars().count() <= CODEX_COMMAND_PREVIEW_LIMIT);
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn source_output_command_hint_detects_common_code_reads() {
        assert!(codex_command_likely_source_output(
            "sed -n '1670,2465p' crates/example-web/src/lib.rs"
        ));
        assert!(codex_command_likely_source_output(
            "cat ./src/components/panel.tsx"
        ));
        assert!(codex_command_likely_source_output(
            "rg -n \"render\" crates/example-web/src/lib.rs"
        ));
        assert!(codex_command_likely_source_output(
            "bash -lc \"nl -ba src/main.py | sed -n '1,220p'\""
        ));

        assert!(!codex_command_likely_source_output("cargo test src/lib.rs"));
        assert!(!codex_command_likely_source_output(
            "sed -n '1,80p' /tmp/runtime.log"
        ));
        assert!(!codex_command_likely_source_output("rg timeout"));
    }

    #[test]
    fn translate_item_started_collab_spawn_agent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "item": {
                "type": "collabAgentToolCall",
                "id": "collab-1",
                "tool": "spawnAgent",
                "status": "inProgress",
                "senderThreadId": "parent-thread",
                "receiverThreadIds": ["child-thread"],
                "prompt": "review the patch",
                "model": "gpt-5.5",
                "reasoningEffort": "high",
                "agentsStates": {
                    "child-thread": {"status": "running", "message": null}
                }
            }
        });

        translate_notification("item/started", &params, &tx);

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::SubAgentToolCall {
                item_id,
                tool,
                status,
                sender_thread_id,
                receiver_thread_ids,
                prompt,
                model,
                reasoning_effort,
                agents,
            } => {
                assert_eq!(item_id, "collab-1");
                assert_eq!(tool, "spawnAgent");
                assert_eq!(status, "inProgress");
                assert_eq!(sender_thread_id, "parent-thread");
                assert_eq!(receiver_thread_ids, vec!["child-thread".to_string()]);
                assert_eq!(prompt.as_deref(), Some("review the patch"));
                assert_eq!(model.as_deref(), Some("gpt-5.5"));
                assert_eq!(reasoning_effort.as_deref(), Some("high"));
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].thread_id, "child-thread");
                assert_eq!(agents[0].status, "running");
            }
            other => panic!("expected SubAgentToolCall, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_collab_agent_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "type": "collabAgentToolCall",
                "id": "collab-2",
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "parent-thread",
                "receiverThreadIds": ["child-thread"],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agentsStates": {
                    "child-thread": {
                        "status": "completed",
                        "message": "looks good"
                    }
                }
            }
        });

        translate_notification("item/completed", &params, &tx);

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::SubAgentToolCall {
                item_id,
                tool,
                status,
                agents,
                ..
            } => {
                assert_eq!(item_id, "collab-2");
                assert_eq!(tool, "wait");
                assert_eq!(status, "completed");
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].thread_id, "child-thread");
                assert_eq!(agents[0].status, "completed");
                assert_eq!(agents[0].message.as_deref(), Some("looks good"));
            }
            other => panic!("expected SubAgentToolCall, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "collabAgentToolCall should not also emit generic ToolCompleted"
        );
    }

    #[test]
    fn translate_turn_plan_updated() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "plan": [
                {"status": "completed", "step": "Inspect current picker APIs/UI"},
                {"status": "inProgress", "step": "Add binary path browse mode"},
                {"status": "pending", "step": "Run focused checks/tests"}
            ],
            "threadId": "thread-1",
            "turnId": "turn-1"
        });

        translate_notification("turn/plan/updated", &params, &tx);

        match rx.try_recv().unwrap() {
            AgentEvent::PlanUpdate { entries } => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].0, "Inspect current picker APIs/UI");
                assert_eq!(entries[0].2, "completed");
                assert_eq!(entries[1].0, "Add binary path browse mode");
                assert_eq!(entries[1].2, "inprogress");
                assert_eq!(entries[2].0, "Run focused checks/tests");
                assert_eq!(entries[2].2, "pending");
            }
            other => panic!("expected PlanUpdate, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_web_search() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-web-1",
            "item": {
                "type": "webSearch",
                "query": "OpenAI API pricing gpt-5.5"
            }
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-web-1");
                assert_eq!(tool_name, "web_search");
                assert_eq!(preview, "OpenAI API pricing gpt-5.5");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_web_search_nested_query() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-web-2",
                "type": "webSearch",
                "arguments": {"search_query": "Anthropic Claude Opus pricing"}
            }
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-web-2");
                assert_eq!(tool_name, "web_search");
                assert_eq!(preview, "Anthropic Claude Opus pricing");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_web_search() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-web-3",
                "type": "webSearch",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-web-3");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_file_change() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-2",
            "item": {"type": "fileChange", "path": "/tmp/test.txt"}
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                item_id,
                tool_name,
                preview,
            } => {
                assert_eq!(item_id, "item-2");
                assert_eq!(tool_name, "file_change");
                assert_eq!(preview, "/tmp/test.txt");
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_file_change_without_path_is_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-2",
            "item": {"type": "fileChange"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "blank fileChange should emit nothing"
        );
    }

    #[test]
    fn translate_item_started_file_change_uses_changes_map() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-2",
            "item": {
                "type": "fileChange",
                "changes": {
                    "src/main.rs": {},
                    "src/lib.rs": {}
                }
            }
        });
        translate_notification("item/started", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolStarted {
                tool_name, preview, ..
            } => {
                assert_eq!(tool_name, "file_change");
                assert!(preview.contains("src/lib.rs"));
                assert!(preview.contains("src/main.rs"));
            }
            other => panic!("expected ToolStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_agent_message_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-3",
            "item": {"type": "agentMessage"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "agentMessage start should emit nothing"
        );
    }

    #[test]
    fn translate_codex_bookkeeping_items() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = serde_json::json!({
            "itemId": "item-4",
            "item": {"type": "contextCompaction"}
        });
        translate_notification("item/started", &started, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert!(message.contains("compacted context"));
            }
            other => panic!("expected Log for contextCompaction, got {:?}", other),
        }

        let completed = serde_json::json!({
            "item": {"id": "item-5", "type": "imageView", "status": "completed"}
        });
        translate_notification("item/completed", &completed, &tx);
        assert!(
            rx.try_recv().is_err(),
            "imageView completion should emit nothing"
        );
    }

    #[test]
    fn translate_thread_compacted_logs_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1"
        });
        translate_notification("thread/compacted", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert!(message.contains("compacted context"));
                assert!(message.contains("turn-1"));
            }
            other => panic!("expected Log for thread/compacted, got {:?}", other),
        }
    }

    #[test]
    fn translate_output_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"itemId": "item-1", "delta": "output line"});
        translate_notification("item/commandExecution/outputDelta", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(text, "output line");
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
    }

    #[test]
    fn codex_command_output_hygiene_suppresses_repeated_warning_blocks() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let input = "\
warning: unused import: `a`
 --> src/a.rs:1:1
  |

warning: unused variable: `b`
 --> src/b.rs:2:1
  |

warning: dead code
 --> src/c.rs:3:1
  |

warning: unused import: `d`
 --> src/d.rs:4:1
  |

warning: unused variable: `e`
 --> src/e.rs:5:1
  |

error: could not compile `demo`
";

        let filtered = hygiene.filter(input, true).unwrap();
        assert!(filtered.contains("warning: unused import: `a`"));
        assert!(filtered.contains("warning: unused variable: `b`"));
        assert!(filtered.contains("warning: dead code"));
        assert!(!filtered.contains("warning: unused import: `d`"));
        assert!(!filtered.contains("src/d.rs"));
        assert!(!filtered.contains("warning: unused variable: `e`"));
        assert!(filtered.contains("suppressed additional repeated warning diagnostics"));
        assert!(filtered.contains("error: could not compile `demo`"));
    }

    #[test]
    fn codex_command_output_hygiene_suppresses_rust_warning_source_excerpts() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut input = String::new();
        for idx in 0..6 {
            input.push_str(&format!(
                "\
warning: station warning {idx}
 --> crates/station-web/src/lib.rs:{line}:9
  |
{line} |     let station_warning_fragment_{idx} = render_station();
  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^
  = note: `#[warn(dead_code)]` on by default

",
                line = 100 + idx
            ));
        }
        input.push_str("error: could not compile `station-web`\n");

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("warning: station warning 0"));
        assert!(filtered.contains("station_warning_fragment_0"));
        assert!(filtered.contains("warning: station warning 2"));
        assert!(filtered.contains("station_warning_fragment_2"));
        assert!(!filtered.contains("warning: station warning 3"));
        assert!(!filtered.contains("station_warning_fragment_3"));
        assert!(!filtered.contains("warning: station warning 5"));
        assert!(!filtered.contains("station_warning_fragment_5"));
        assert_eq!(
            filtered
                .matches("suppressed additional repeated warning diagnostics")
                .count(),
            1
        );
        assert!(filtered.contains("error: could not compile `station-web`"));
    }

    #[test]
    fn codex_command_output_hygiene_suppresses_split_duplicated_warning_source_fragments() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut filtered = String::new();
        for idx in 0..CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
            let chunk = format!(
                "\
warning: inline warning {idx}
 --> src/terminal.rs:{idx}:1
  |

"
            );
            if let Some(output) = hygiene.filter(&chunk, false) {
                filtered.push_str(&output);
            }
        }

        let chunk = "\
warning: suppressed local constructor
 --> src/terminal.rs:59:12
  |
59 |     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

59 ";
        if let Some(output) = hygiene.filter(chunk, false) {
            filtered.push_str(&output);
        }

        let chunk = "\
|     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

error: could not compile `intendant`
";
        if let Some(output) = hygiene.filter(chunk, true) {
            filtered.push_str(&output);
        }

        assert!(filtered.contains("warning: inline warning 0"));
        assert!(filtered.contains("warning: inline warning 2"));
        assert!(filtered.contains("suppressed additional repeated warning diagnostics"));
        assert!(!filtered.contains("warning: suppressed local constructor"));
        assert!(!filtered.contains("pub fn local(terminal_id"));
        assert!(!filtered.contains("^^^^^"));
        assert!(filtered.contains("error: could not compile `intendant`"));
    }

    #[test]
    fn codex_command_output_hygiene_suppresses_post_limit_detached_warning_tail() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut filtered = String::new();
        for idx in 0..CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
            let chunk = format!(
                "\
warning: inline warning {idx}
 --> src/lib.rs:{idx}:1
  |

"
            );
            if let Some(output) = hygiene.filter(&chunk, false) {
                filtered.push_str(&output);
            }
        }

        let chunk = "\
warning: suppressed previous warning
 --> src/terminal.rs:59:12
  |

status: continuing after suppressed warning
";
        if let Some(output) = hygiene.filter(chunk, false) {
            filtered.push_str(&output);
        }

        if let Some(output) = hygiene.filter("59 ", false) {
            filtered.push_str(&output);
        }

        let chunk = "\
|     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

warning: variants `Help` and `Inspect` are never constructed
  --> src/bin/caller/tui/app.rs:19:5
   |
16 | pub e";
        if let Some(output) = hygiene.filter(chunk, false) {
            filtered.push_str(&output);
        }

        let chunk = "\
num AppMode {
error: could not compile `intendant`
";
        if let Some(output) = hygiene.filter(chunk, true) {
            filtered.push_str(&output);
        }

        assert!(filtered.contains("warning: inline warning 0"));
        assert!(filtered.contains("warning: inline warning 2"));
        assert!(filtered.contains("status: continuing after suppressed warning"));
        assert!(filtered.contains("suppressed additional repeated warning diagnostics"));
        assert!(!filtered.contains("warning: suppressed previous warning"));
        assert!(!filtered.contains("pub fn local(terminal_id"));
        assert!(!filtered.contains("^^^^^"));
        assert!(!filtered.contains("variants `Help` and `Inspect`"));
        assert!(!filtered.contains("src/bin/caller/tui/app.rs"));
        assert!(!filtered.contains("16 | pub enum AppMode"));
        assert!(filtered.contains("error: could not compile `intendant`"));
    }

    #[test]
    fn codex_command_output_hygiene_truncates_long_lines() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let input = format!(
            "chromium --type=renderer --headless=new {} --last-important-flag\n",
            "--very-long-arg=".repeat(300)
        );

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("chromium --type=renderer"));
        assert!(filtered.contains("--last-important-flag"));
        assert!(filtered.contains("truncated long command-output line"));
        assert!(filtered.chars().count() <= CODEX_COMMAND_OUTPUT_LINE_LIMIT + 1);
    }

    #[test]
    fn codex_command_output_hygiene_leaves_small_output_unchanged() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let input = "stdout line\nstderr: useful diagnostic\n";

        let filtered = hygiene.filter(input, true).unwrap();

        assert_eq!(filtered, input);
    }

    #[test]
    fn codex_command_output_hygiene_collapses_repetitive_build_progress() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut input = String::from("[INFO]: Compiling to Wasm...\n");
        for i in 0..40 {
            input.push_str(&format!("   Compiling crate_{i} v0.1.0\n"));
            input.push_str(&format!("    Checking helper_{i} v0.1.0\n"));
        }
        input.push_str(
            "    Finished `test` profile [unoptimized + debuginfo] target(s) in 2m 17s\n",
        );

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("[INFO]: Compiling to Wasm"));
        assert!(filtered.contains("Compiling crate_0"));
        assert!(filtered.contains("Compiling crate_1"));
        assert!(filtered.contains("suppressed repetitive build progress"));
        assert!(filtered.contains("suppressed 77 repetitive build progress lines"));
        assert!(filtered.contains("last: Checking helper_39 v0.1.0"));
        assert!(filtered.contains("Finished `test` profile"));
        assert!(!filtered.contains("Compiling crate_30"));
        assert!(
            filtered.len() < input.len() / 4,
            "build progress should be compacted, got {} bytes from {}",
            filtered.len(),
            input.len()
        );
    }

    #[test]
    fn codex_command_output_hygiene_keeps_build_failure_after_progress() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut input = String::new();
        for i in 0..30 {
            input.push_str(&format!("   Compiling failing_crate_{i} v0.1.0\n"));
        }
        input.push_str("error: could not compile `failing_crate`\n");

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("Compiling failing_crate_0"));
        assert!(filtered.contains("suppressed repetitive build progress"));
        assert!(filtered.contains("error: could not compile `failing_crate`"));
        assert!(!filtered.contains("Compiling failing_crate_20"));
    }

    #[test]
    fn codex_command_output_hygiene_collapses_carriage_return_build_progress() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let mut filtered = String::new();

        for i in 0..30 {
            let chunk = format!("\r\u{1b}[K   Compiling redraw_crate_{i} v0.1.0");
            if let Some(output) = hygiene.filter(&chunk, false) {
                filtered.push_str(&output);
            }
        }
        if let Some(output) = hygiene.filter(
            "\rerror: could not compile `redraw_crate` due to previous error\n",
            true,
        ) {
            filtered.push_str(&output);
        }

        assert!(filtered.contains("Compiling redraw_crate_0"));
        assert!(filtered.contains("Compiling redraw_crate_1"));
        assert!(filtered.contains("suppressed repetitive build progress"));
        assert!(filtered.contains("suppressed 26 repetitive build progress lines"));
        assert!(filtered.contains("last: \u{1b}[K   Compiling redraw_crate_29 v0.1.0"));
        assert!(filtered.contains("error: could not compile `redraw_crate`"));
        assert!(!filtered.contains("Compiling redraw_crate_20"));
    }

    #[test]
    fn codex_command_output_hygiene_compacts_large_static_app_html_source() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let input = large_static_app_html_js_output();

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("<script>"));
        assert!(filtered.contains("function renderStationRow0"));
        assert!(filtered.contains("END_STATIC_APP_HTML_MARKER"));
        assert!(filtered.contains("omitting additional large command output"));
        assert!(filtered.contains("bytes from the middle"));
        assert!(!filtered.contains("function renderStationRow80"));
        assert!(
            filtered.len()
                <= CODEX_COMMAND_SOURCE_OUTPUT_HEAD_LIMIT
                    + CODEX_COMMAND_SOURCE_OUTPUT_TAIL_LIMIT
                    + 512,
            "filtered output should stay bounded, got {} bytes",
            filtered.len()
        );
    }

    #[test]
    fn translate_output_delta_compacts_command_hinted_source_read() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        let start = serde_json::json!({
            "item": {
                "id": "item-source-read",
                "type": "commandExecution",
                "command": "sed -n '1670,2465p' crates/example-web/src/lib.rs"
            }
        });
        translate_notification_with_state("item/started", &start, &tx, &mut state);
        let _ = rx.try_recv().unwrap();

        let output = large_comment_heavy_source_output();
        assert!(
            output.len() < CODEX_COMMAND_OUTPUT_INLINE_LIMIT,
            "fixture should characterize the generic inline hole"
        );
        let delta = serde_json::json!({
            "itemId": "item-source-read",
            "delta": output
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &delta,
            &tx,
            &mut state,
        );

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-source-read");
                assert!(text.contains("comment heavy source line 000"));
                assert!(text.contains("omitting additional large command output"));
                assert!(text.contains("final tail will be shown when the command completes"));
                assert!(!text.contains("comment heavy source line 060"));
                assert!(!text.contains("comment heavy source line 119"));
                assert!(
                    text.len() <= CODEX_COMMAND_SOURCE_OUTPUT_HEAD_LIMIT + 512,
                    "command-hinted source output should stay bounded, got {} bytes",
                    text.len()
                );
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }

        let completed = serde_json::json!({
            "item": {
                "id": "item-source-read",
                "type": "commandExecution",
                "status": "completed"
            }
        });
        translate_notification_with_state("item/completed", &completed, &tx, &mut state);

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-source-read");
                assert!(text.contains("bytes from the middle"));
                assert!(text.contains("comment heavy source line 119"));
                assert!(!text.contains("comment heavy source line 060"));
            }
            other => panic!("expected ToolOutputDelta tail, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_uses_command_hint_without_started_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = large_comment_heavy_source_output();
        let params = serde_json::json!({
            "item": {
                "id": "item-cat-source",
                "type": "commandExecution",
                "status": "completed",
                "command": "cat src/generated_fixture.ts",
                "aggregatedOutput": output
            }
        });

        translate_notification("item/completed", &params, &tx);

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-cat-source");
                assert!(text.contains("comment heavy source line 000"));
                assert!(text.contains("comment heavy source line 119"));
                assert!(text.contains("bytes from the middle"));
                assert!(!text.contains("comment heavy source line 060"));
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_compacts_large_command_output() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = large_static_app_html_js_output();
        let params = serde_json::json!({
            "item": {
                "id": "item-static-app",
                "type": "commandExecution",
                "status": "completed",
                "aggregatedOutput": output
            }
        });

        translate_notification("item/completed", &params, &tx);

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-static-app");
                assert!(text.contains("function renderStationRow0"));
                assert!(text.contains("END_STATIC_APP_HTML_MARKER"));
                assert!(text.contains("bytes from the middle"));
                assert!(
                    text.len()
                        <= CODEX_COMMAND_SOURCE_OUTPUT_HEAD_LIMIT
                            + CODEX_COMMAND_SOURCE_OUTPUT_TAIL_LIMIT
                            + 512,
                    "translated output should stay bounded, got {} bytes",
                    text.len()
                );
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-static-app");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn codex_command_output_hygiene_compacts_very_large_non_source_output() {
        let mut hygiene = CodexCommandOutputHygiene::default();
        let input = format!(
            "BEGIN-LOG\n{}END-LOG\n",
            (0..700)
                .map(|i| format!("2026-06-06T12:00:{:02}Z INFO event number {i}\n", i % 60))
                .collect::<String>()
        );

        let filtered = hygiene.filter(&input, true).unwrap();

        assert!(filtered.contains("BEGIN-LOG"));
        assert!(filtered.contains("END-LOG"));
        assert!(filtered.contains("omitting additional large command output"));
        assert!(filtered.contains("bytes from the middle"));
        assert!(
            filtered.len()
                <= CODEX_COMMAND_OUTPUT_HEAD_LIMIT + CODEX_COMMAND_OUTPUT_TAIL_LIMIT + 512,
            "generic large output should stay bounded, got {} bytes",
            filtered.len()
        );
    }

    #[test]
    fn translate_output_delta_suppresses_warning_flood_but_keeps_errors() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        for idx in 0..5 {
            let params = serde_json::json!({
                "itemId": "item-1",
                "delta": format!("warning: noisy diagnostic {idx}\n")
            });
            translate_notification_with_state(
                "item/commandExecution/outputDelta",
                &params,
                &tx,
                &mut state,
            );
        }
        let params = serde_json::json!({"itemId": "item-1", "delta": "error: build failed\n"});
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );

        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolOutputDelta { text, .. } => texts.push(text),
                other => panic!("expected ToolOutputDelta, got {:?}", other),
            }
        }
        let joined = texts.join("");
        assert!(joined.contains("warning: noisy diagnostic 0"));
        assert!(joined.contains("warning: noisy diagnostic 1"));
        assert!(joined.contains("warning: noisy diagnostic 2"));
        assert!(!joined.contains("warning: noisy diagnostic 3"));
        assert!(!joined.contains("warning: noisy diagnostic 4"));
        assert!(joined.contains("suppressed additional repeated warning diagnostics"));
        assert!(joined.contains("error: build failed"));
    }

    #[test]
    fn translate_output_delta_coalesces_active_carriage_return_build_progress() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        for idx in 0..40 {
            let params = serde_json::json!({
                "itemId": "item-build",
                "delta": format!("\r\u{1b}[K    Checking active_crate_{idx} v0.1.0")
            });
            translate_notification_with_state(
                "item/commandExecution/outputDelta",
                &params,
                &tx,
                &mut state,
            );
        }
        let params = serde_json::json!({
            "itemId": "item-build",
            "delta": "\rerror: build failed\n"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );

        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolOutputDelta { text, .. } => texts.push(text),
                other => panic!("expected ToolOutputDelta, got {:?}", other),
            }
        }

        let joined = texts.join("");
        assert!(joined.contains("Checking active_crate_0"));
        assert!(joined.contains("Checking active_crate_1"));
        assert!(joined.contains("suppressed repetitive build progress"));
        assert!(joined.contains("error: build failed"));
        assert!(!joined.contains("Checking active_crate_20"));
        assert!(
            texts.len() <= CODEX_BUILD_PROGRESS_INLINE_LIMIT + 2,
            "active progress should emit a bounded number of deltas, got {}",
            texts.len()
        );
    }

    #[test]
    fn translate_output_delta_suppresses_split_warning_source_excerpt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        for idx in 0..CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
            let params = serde_json::json!({
                "itemId": "item-1",
                "delta": format!("warning: inline warning {idx}\n --> src/lib.rs:{idx}:1\n  |\n\n")
            });
            translate_notification_with_state(
                "item/commandExecution/outputDelta",
                &params,
                &tx,
                &mut state,
            );
        }

        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "warning: suppressed warning\n --> crates/station-web/src/lib.rs:404:9\n  |\n404 "
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "|     let split_station_warning_fragment = render_station();\n  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n  = note: split continuation must stay hidden\n\nerror: build failed\n"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );

        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolOutputDelta { text, .. } => texts.push(text),
                other => panic!("expected ToolOutputDelta, got {:?}", other),
            }
        }
        let joined = texts.join("");
        assert!(joined.contains("warning: inline warning 0"));
        assert!(joined.contains("warning: inline warning 2"));
        assert!(joined.contains("suppressed additional repeated warning diagnostics"));
        assert!(!joined.contains("warning: suppressed warning"));
        assert!(!joined.contains("split_station_warning_fragment"));
        assert!(!joined.contains("split continuation must stay hidden"));
        assert!(joined.contains("error: build failed"));
    }

    #[test]
    fn translate_output_delta_suppresses_duplicated_warning_source_excerpt_after_blank() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        for idx in 0..CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
            let params = serde_json::json!({
                "itemId": "item-1",
                "delta": format!("warning: inline warning {idx}\n --> src/lib.rs:{idx}:1\n  |\n\n")
            });
            translate_notification_with_state(
                "item/commandExecution/outputDelta",
                &params,
                &tx,
                &mut state,
            );
        }

        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "\
warning: suppressed local constructor
 --> src/terminal.rs:59:12
  |
59 |     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

59 "
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "\
|     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

error: build failed
"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );

        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolOutputDelta { text, .. } => texts.push(text),
                other => panic!("expected ToolOutputDelta, got {:?}", other),
            }
        }
        let joined = texts.join("");
        assert!(joined.contains("warning: inline warning 0"));
        assert!(joined.contains("warning: inline warning 2"));
        assert!(joined.contains("suppressed additional repeated warning diagnostics"));
        assert!(!joined.contains("warning: suppressed local constructor"));
        assert!(!joined.contains("pub fn local(terminal_id"));
        assert!(!joined.contains("^^^^^"));
        assert!(joined.contains("error: build failed"));
    }

    #[test]
    fn translate_output_delta_suppresses_post_limit_detached_warning_tail() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        for idx in 0..CODEX_WARNING_DIAGNOSTIC_INLINE_LIMIT {
            let params = serde_json::json!({
                "itemId": "item-1",
                "delta": format!("warning: inline warning {idx}\n --> src/lib.rs:{idx}:1\n  |\n\n")
            });
            translate_notification_with_state(
                "item/commandExecution/outputDelta",
                &params,
                &tx,
                &mut state,
            );
        }

        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "\
warning: suppressed previous warning
 --> src/terminal.rs:59:12
  |

status: continuing after suppressed warning
"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "59 "
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "\
|     pub fn local(terminal_id: impl Into<String>) -> Self {
   |            ^^^^^

warning: variants `Help` and `Inspect` are never constructed
  --> src/bin/caller/tui/app.rs:19:5
   |
16 | pub e"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );
        let params = serde_json::json!({
            "itemId": "item-1",
            "delta": "\
num AppMode {
error: build failed
"
        });
        translate_notification_with_state(
            "item/commandExecution/outputDelta",
            &params,
            &tx,
            &mut state,
        );

        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolOutputDelta { text, .. } => texts.push(text),
                other => panic!("expected ToolOutputDelta, got {:?}", other),
            }
        }
        let joined = texts.join("");
        assert!(joined.contains("warning: inline warning 0"));
        assert!(joined.contains("warning: inline warning 2"));
        assert!(joined.contains("status: continuing after suppressed warning"));
        assert!(joined.contains("suppressed additional repeated warning diagnostics"));
        assert!(!joined.contains("warning: suppressed previous warning"));
        assert!(!joined.contains("pub fn local(terminal_id"));
        assert!(!joined.contains("^^^^^"));
        assert!(!joined.contains("variants `Help` and `Inspect`"));
        assert!(!joined.contains("src/bin/caller/tui/app.rs"));
        assert!(!joined.contains("16 | pub enum AppMode"));
        assert!(joined.contains("error: build failed"));
    }

    fn large_static_app_html_js_output() -> String {
        let mut output = String::from("<div id=\"app\"></div>\n<script>\n");
        for i in 0..120 {
            output.push_str(&format!(
                "function renderStationRow{i}(station) {{\n  const label = station.name || 'station-{i}';\n  const node = document.querySelector('#station-{i}');\n  if (node) {{\n    node.addEventListener('click', () => window.dispatchEvent(new CustomEvent('station-select', {{ detail: label }})));\n  }}\n  return label;\n}}\n"
            ));
        }
        output.push_str("</script>\nEND_STATIC_APP_HTML_MARKER\n");
        output
    }

    fn large_comment_heavy_source_output() -> String {
        let mut output = String::new();
        for i in 0..120 {
            output.push_str(&format!("// comment heavy source line {i:03}: fixture\n"));
        }
        output
    }

    #[test]
    fn translate_terminal_interaction_is_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "call_123",
            "processId": "62701",
            "stdin": "secret input\n",
            "threadId": "thread-1",
            "turnId": "turn-1"
        });
        translate_notification("item/commandExecution/terminalInteraction", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "terminal stdin interactions should not emit activity events"
        );
    }

    #[test]
    fn translate_item_completed_success() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "completed", "aggregatedOutput": "hello\n"}
        });
        translate_notification("item/completed", &params, &tx);
        // First event: ToolOutputDelta with the aggregated output
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(text, "hello\n");
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
        // Second event: ToolCompleted
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_function_call_output_completion_uses_call_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "type": "function_call_output",
                "call_id": "call_CP7ok6SOm9fbU9zYp8Ok1IL3",
                "output": "Chunk ID: d1ff8c\nWall time: 30.0011 seconds\nProcess exited with code 0\nOriginal token count: 12\nOutput:\nactual command output\n"
            }
        });

        translate_notification("item/completed", &params, &tx);

        match rx.try_recv().unwrap() {
            AgentEvent::ToolOutputDelta { item_id, text } => {
                assert_eq!(item_id, "call_CP7ok6SOm9fbU9zYp8Ok1IL3");
                assert_eq!(text, "actual command output\n");
                assert!(!text.contains("Chunk ID:"));
                assert!(!text.contains("Wall time:"));
            }
            other => panic!("expected ToolOutputDelta, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call_CP7ok6SOm9fbU9zYp8Ok1IL3");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_function_call_output_completion_uses_top_level_call_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "callId": "call_IXwDrmqUWzOZ8mBwjyG3rJqd",
            "item": {
                "type": "function_call_output",
                "output": "Chunk ID: c36672\nWall time: 17.4574 seconds\nProcess exited with code 0\n"
            }
        });

        translate_notification("item/completed", &params, &tx);

        match rx.try_recv().unwrap() {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "call_IXwDrmqUWzOZ8mBwjyG3rJqd");
                assert_eq!(status, ToolCompletionStatus::Success);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "failed", "error": "permission denied"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(
                    status,
                    ToolCompletionStatus::Failed {
                        message: "permission denied".into()
                    }
                );
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_nonzero_exit() {
        // commandExecution that ran to completion with exit != 0: Codex omits
        // `error`, carries the diagnostic in aggregatedOutput + exitCode.
        // We must surface a real message, not "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-1",
                "type": "commandExecution",
                "status": "failed",
                "exitCode": 1,
                "aggregatedOutput": "Traceback (most recent call last):\n  File \"<string>\", line 1\nModuleNotFoundError: No module named 'odf'\n"
            }
        });
        translate_notification("item/completed", &params, &tx);
        // First the output delta, then the ToolCompleted with a real reason.
        let _ = rx.try_recv().unwrap(); // ToolOutputDelta
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted {
                status: ToolCompletionStatus::Failed { message },
                ..
            } => {
                assert!(
                    message.contains("exited 1"),
                    "message should carry exit code: {}",
                    message
                );
                assert!(
                    message.contains("ModuleNotFoundError"),
                    "message should carry output tail: {}",
                    message
                );
            }
            other => panic!("expected Failed with detailed message, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_output_only() {
        // aggregatedOutput without exitCode still beats "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-2",
                "type": "commandExecution",
                "status": "failed",
                "aggregatedOutput": "RuntimeError: could not connect to pipe\n"
            }
        });
        translate_notification("item/completed", &params, &tx);
        let _ = rx.try_recv().unwrap();
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted {
                status: ToolCompletionStatus::Failed { message },
                ..
            } => {
                assert!(
                    message.contains("could not connect to pipe"),
                    "got: {}",
                    message
                );
                assert!(
                    !message.contains("unknown error"),
                    "should not fall through to unknown: {}",
                    message
                );
            }
            other => panic!("expected Failed with output tail, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_exit_only_mentions_empty_output() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "item-2",
                "type": "commandExecution",
                "status": "failed",
                "exitCode": 1
            }
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted {
                status: ToolCompletionStatus::Failed { message },
                ..
            } => {
                assert_eq!(message, "command exited 1 (no output)");
            }
            other => panic!("expected Failed with exit-only detail, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_failed_truly_empty_falls_back() {
        // Only when we have literally nothing do we say "unknown error".
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-3", "type": "mcpToolCall", "status": "failed"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted {
                status: ToolCompletionStatus::Failed { message },
                ..
            } => {
                assert_eq!(message, "unknown error");
            }
            other => panic!("expected Failed with unknown error, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_cancelled() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "item-1", "type": "commandExecution", "status": "cancelled"}
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCompleted { item_id, status } => {
                assert_eq!(item_id, "item-1");
                assert_eq!(status, ToolCompletionStatus::Cancelled);
            }
            other => panic!("expected ToolCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_reasoning_emits_reasoning_event() {
        // Codex emits reasoning text via item/completed with type="reasoning".
        // We must surface the chain-of-thought via AgentEvent::Reasoning
        // (rendered at "detail" verbosity) instead of the old AutoApproved
        // noise path. And no ToolCompleted marker — reasoning is not a tool.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "rs_123",
                "type": "reasoning",
                "summary": [
                    {"type": "summary_text", "text": "Step 1: parse the request"},
                    {"type": "summary_text", "text": "Step 2: decide tool"}
                ],
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::Reasoning { text } => {
                assert!(text.contains("Step 1: parse the request"));
                assert!(text.contains("Step 2: decide tool"));
            }
            other => panic!("expected Reasoning, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "reasoning should not emit a ToolCompleted marker"
        );
    }

    #[test]
    fn translate_item_completed_reasoning_text_field() {
        // Fallback path: reasoning item with plain text field.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "rs_456",
                "type": "reasoning",
                "text": "raw reasoning trace"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Reasoning { text } => assert_eq!(text, "raw reasoning trace"),
            other => panic!("expected Reasoning, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_item_completed_reasoning_empty_is_silent() {
        // No text, no summary → no event.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "rs_789", "type": "reasoning"}
        });
        translate_notification("item/completed", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "empty reasoning should emit nothing"
        );
    }

    #[test]
    fn translate_item_completed_agent_message_skips_tool_completed() {
        // agentMessage items should emit Message with the final text, but
        // NOT a ToolCompleted marker — they are not tools.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_001",
                "type": "agentMessage",
                "text": "Final response text",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Message { text } => assert_eq!(text, "Final response text"),
            other => panic!("expected Message, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "agentMessage should not emit ToolCompleted"
        );
    }

    #[test]
    fn translate_item_completed_suppresses_noop_tool_wait_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_wait",
                "type": "agentMessage",
                "text": "Still building; no error output.",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "quiet tool wait chatter should not become durable model output"
        );
    }

    #[test]
    fn translate_item_completed_suppresses_short_polling_chatter() {
        for text in [
            "No output yet.",
            "Still active.",
            "Polling...",
            "The build is still running...",
        ] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let params = serde_json::json!({
                "item": {
                    "id": "msg_wait",
                    "type": "agentMessage",
                    "text": text,
                    "status": "completed"
                }
            });
            translate_notification("item/completed", &params, &tx);
            assert!(
                rx.try_recv().is_err(),
                "{text:?} should not become durable model output"
            );
        }
    }

    #[test]
    fn translate_final_answer_noop_tool_wait_completes_without_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_wait_final",
                "type": "agentMessage",
                "text": "Still waiting on the cargo build; no new output yet.",
                "phase": "final_answer",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::TurnCompleted { message } => assert_eq!(message, None),
            other => panic!("expected TurnCompleted without chatter, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_item_completed_keeps_material_no_output_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_material",
                "type": "agentMessage",
                "text": "No output yet, but I found the hung process and changed the timeout.",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Message { text } => {
                assert_eq!(
                    text,
                    "No output yet, but I found the hung process and changed the timeout."
                );
            }
            other => panic!("expected material Message, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_item_completed_keeps_material_progress_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {
                "id": "msg_progress",
                "type": "agentMessage",
                "text": "The release build finished; next I am checking the binary.",
                "status": "completed"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Message { text } => {
                assert_eq!(
                    text,
                    "The release build finished; next I am checking the binary."
                );
            }
            other => panic!("expected material Message, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_final_answer_agent_message_completes_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "id": "msg_001",
                "type": "agentMessage",
                "text": "Final response text",
                "phase": "final_answer"
            }
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Message { text } => assert_eq!(text, "Final response text"),
            other => panic!("expected Message, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message.as_deref(), Some("Final response text"));
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_item_completed_user_message_observed() {
        // userMessage items are echoes of the user's input. Surface them
        // internally so the caller can confirm accepted steers reached Codex's
        // conversation.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "item": {"id": "u_001", "type": "userMessage", "text": "hello"}
        });
        translate_notification("item/completed", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::UserMessage { text } => assert_eq!(text, "hello"),
            other => panic!("expected UserMessage, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_turn_completed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"message": "All done"});
        translate_notification("turn/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, Some("All done".into()));
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_completed_no_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({});
        translate_notification("turn/completed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_interrupted_completes_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"threadId": "thread-1", "turnId": "turn-1"});
        translate_notification("turn/interrupted", &params, &tx);
        let event = rx.try_recv().unwrap().into_scope().2;
        match event {
            AgentEvent::TurnCompleted { message } => assert_eq!(message, None),
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn translate_turn_failed_logs_error_and_completes_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "error": {"message": "model backend exploded"},
        });
        translate_notification("turn/failed", &params, &tx);
        let first = rx.try_recv().unwrap().into_scope().2;
        match first {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "error");
                assert!(message.contains("model backend exploded"), "log: {message}");
            }
            other => panic!("expected Log, got {:?}", other),
        }
        let second = rx.try_recv().unwrap().into_scope().2;
        match second {
            AgentEvent::TurnCompleted { message } => assert_eq!(message, None),
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_failed_without_error_message_still_completes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({});
        translate_notification("turn/failed", &params, &tx);
        let first = rx.try_recv().unwrap();
        match first {
            AgentEvent::Log { level, .. } => assert_eq!(level, "error"),
            other => panic!("expected Log, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentEvent::TurnCompleted { message } => assert_eq!(message, None),
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_incomplete_error_at_rewind_only_limit_marks_generation_starvation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState {
            latest_usage: Some(AgentUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 97_000,
                completion_tokens: 3_000,
                cached_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "willRetry": false,
            "error": {
                "message": "stream disconnected before completion: Incomplete response returned, reason: max_output_tokens",
                "codexErrorInfo": "other",
                "additionalDetails": "response.incomplete had incomplete_details.reason=max_output_tokens"
            }
        });

        translate_notification_with_state("error", &params, &tx, &mut state);

        match rx.try_recv().unwrap() {
            AgentEvent::BackendError {
                message,
                code,
                details,
                will_retry,
                likely_generation_starvation,
                recovery_hint,
            } => {
                assert!(message.contains("Incomplete response returned"));
                assert_eq!(code.as_deref(), Some("other"));
                assert!(details.as_deref().unwrap().contains("response.incomplete"));
                assert!(!will_retry);
                assert!(likely_generation_starvation);
                let hint = recovery_hint.expect("near-limit incomplete response needs a hint");
                assert!(hint.contains("rewind context first"));
                assert!(
                    !hint.contains("item-"),
                    "hint should not prescribe a stale anchor"
                );
            }
            other => panic!("expected BackendError, got {:?}", other),
        }
    }

    #[test]
    fn translate_incomplete_error_above_recommended_below_rewind_only_allows_normal_recovery() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState {
            latest_usage: Some(AgentUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 86_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 86.0,
                prompt_tokens: 83_000,
                completion_tokens: 3_000,
                cached_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "willRetry": false,
            "error": {
                "message": "stream disconnected before completion: Incomplete response returned, reason: max_output_tokens",
                "codexErrorInfo": "other",
                "additionalDetails": "response.incomplete had incomplete_details.reason=max_output_tokens"
            }
        });

        translate_notification_with_state("error", &params, &tx, &mut state);

        match rx.try_recv().unwrap() {
            AgentEvent::BackendError {
                likely_generation_starvation,
                recovery_hint,
                ..
            } => {
                assert!(!likely_generation_starvation);
                assert!(recovery_hint.is_none());
            }
            other => panic!("expected BackendError, got {:?}", other),
        }
    }

    #[test]
    fn translate_incomplete_error_below_context_limit_does_not_mark_starvation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState {
            latest_usage: Some(AgentUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 20_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 20.0,
                prompt_tokens: 18_000,
                completion_tokens: 2_000,
                cached_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = serde_json::json!({
            "willRetry": false,
            "error": {
                "message": "Incomplete response returned, reason: max_output_tokens",
                "codexErrorInfo": "other"
            }
        });

        translate_notification_with_state("error", &params, &tx, &mut state);

        match rx.try_recv().unwrap() {
            AgentEvent::BackendError {
                likely_generation_starvation,
                recovery_hint,
                ..
            } => {
                assert!(!likely_generation_starvation);
                assert!(recovery_hint.is_none());
            }
            other => panic!("expected BackendError, got {:?}", other),
        }
    }

    #[test]
    fn translate_scoped_notification_preserves_thread_and_turn_ids() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"message": "All done"});
        let mut state = CodexNotificationState::default();

        translate_notification_with_scope(
            "turn/completed",
            &params,
            &tx,
            &mut state,
            Some("thread-abc"),
            Some("turn-xyz"),
        );

        match rx.try_recv().unwrap() {
            AgentEvent::Scoped {
                thread_id,
                turn_id,
                event,
            } => {
                assert_eq!(thread_id.as_deref(), Some("thread-abc"));
                assert_eq!(turn_id.as_deref(), Some("turn-xyz"));
                match *event {
                    AgentEvent::TurnCompleted { message } => {
                        assert_eq!(message, Some("All done".into()));
                    }
                    other => panic!("expected scoped TurnCompleted, got {:?}", other),
                }
            }
            other => panic!("expected Scoped event, got {:?}", other),
        }
    }

    #[test]
    fn translate_diff_updated() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "diff": "--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new",
            "files": ["foo"]
        });
        translate_notification("turn/diff/updated", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::DiffUpdated {
                files_changed,
                unified_diff,
            } => {
                assert_eq!(files_changed, vec!["foo".to_string()]);
                assert!(unified_diff.contains("-old"));
            }
            other => panic!("expected DiffUpdated, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_user_message_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-10",
            "item": {"type": "userMessage"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "userMessage start should emit nothing"
        );
    }

    #[test]
    fn translate_item_started_reasoning_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "itemId": "item-11",
            "item": {"type": "reasoning"}
        });
        translate_notification("item/started", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "reasoning start should emit nothing"
        );
    }

    #[test]
    fn translate_thread_status_changed_completed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "completed"});
        translate_notification("thread/status/changed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_thread_status_changed_idle() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "idle"});
        translate_notification("thread/status/changed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_thread_status_changed_idle_object() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": {"type": "idle"}});
        translate_notification("thread/status/changed", &params, &tx);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TurnCompleted { message } => {
                assert_eq!(message, None);
            }
            other => panic!("expected TurnCompleted, got {:?}", other),
        }
    }

    #[test]
    fn translate_thread_status_changed_running_no_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({"status": "running"});
        translate_notification("thread/status/changed", &params, &tx);
        assert!(
            rx.try_recv().is_err(),
            "running status should not emit TurnCompleted"
        );
    }

    #[test]
    fn scoped_notification_rejects_child_thread_item() {
        let params = serde_json::json!({
            "threadId": "child-thread",
            "turn": {"id": "child-turn"}
        });
        assert!(!codex_notification_targets_active_thread(
            &params,
            Some("parent-thread")
        ));
        assert!(!codex_notification_targets_active_turn(
            &params,
            Some("parent-thread"),
            Some("parent-turn")
        ));
    }

    #[test]
    fn scoped_notification_rejects_stale_turn_item() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turn": {"id": "old-turn"}
        });
        assert!(codex_notification_targets_active_thread(
            &params,
            Some("parent-thread")
        ));
        assert!(!codex_notification_targets_active_turn(
            &params,
            Some("parent-thread"),
            Some("new-turn")
        ));
    }

    #[test]
    fn scoped_notification_accepts_active_turn_item() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turn": {"id": "parent-turn"}
        });
        assert!(codex_notification_targets_active_thread(
            &params,
            Some("parent-thread")
        ));
        assert!(codex_notification_targets_active_turn(
            &params,
            Some("parent-thread"),
            Some("parent-turn")
        ));
    }

    #[test]
    fn thread_status_idle_can_complete_known_active_turn_without_turn_id() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "status": {"type": "idle"}
        });
        assert!(codex_thread_status_can_complete_turn(
            &params,
            Some("parent-turn"),
            false
        ));
    }

    #[test]
    fn thread_status_idle_can_complete_known_active_turn_with_turn_id() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turnId": "parent-turn",
            "status": {"type": "idle"}
        });
        assert!(codex_thread_status_can_complete_turn(
            &params,
            Some("parent-turn"),
            false
        ));
    }

    #[test]
    fn thread_status_idle_can_complete_unknown_active_turn_with_turn_id() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turnId": "parent-turn",
            "status": {"type": "idle"}
        });
        assert!(codex_thread_status_can_complete_turn(&params, None, false));
    }

    #[test]
    fn thread_status_idle_does_not_duplicate_observed_turn_completion() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turnId": "parent-turn",
            "status": {"type": "idle"}
        });
        assert!(!codex_thread_status_can_complete_turn(&params, None, true));
    }

    #[test]
    fn final_answer_agent_message_is_terminal_only_for_completed_messages() {
        let completed = serde_json::json!({
            "item": {
                "type": "agentMessage",
                "phase": "final_answer",
                "text": "done"
            }
        });
        assert!(codex_item_completed_final_answer(&completed));

        let streaming = serde_json::json!({
            "item": {
                "type": "agentMessage",
                "phase": "answer",
                "text": "not terminal"
            }
        });
        assert!(!codex_item_completed_final_answer(&streaming));

        let failed = serde_json::json!({
            "item": {
                "type": "agentMessage",
                "phase": "final_answer",
                "status": "failed",
                "text": "failed"
            }
        });
        assert!(!codex_item_completed_final_answer(&failed));
    }

    #[test]
    fn stale_turn_scoped_final_answer_is_rejected_after_new_turn_starts() {
        assert!(codex_notification_stale_for_active_turn(
            Some("old-turn"),
            Some("new-turn")
        ));
        assert!(!codex_notification_stale_for_active_turn(
            Some("new-turn"),
            Some("new-turn")
        ));
        assert!(!codex_notification_stale_for_active_turn(
            Some("old-turn"),
            None
        ));
    }

    #[test]
    fn final_answer_item_id_dedupes_stale_completion_without_turn_id() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "item": {
                "id": "msg-final-1",
                "type": "agentMessage",
                "phase": "final_answer",
                "text": "previous checkpoint summary"
            }
        });
        let mut observed = HashSet::new();
        let first_keys = codex_terminal_observation_keys(
            &params,
            None,
            Some("old-turn"),
            Some("parent-thread"),
            true,
        );
        codex_mark_terminal_observed(&mut observed, &first_keys);

        let replayed_after_new_turn = codex_terminal_observation_keys(
            &params,
            None,
            Some("new-turn"),
            Some("parent-thread"),
            true,
        );

        assert!(codex_any_terminal_observed(
            &observed,
            &replayed_after_new_turn
        ));
        assert!(codex_terminal_notification_already_observed(
            "item/completed",
            true,
            true,
        ));
    }

    #[test]
    fn final_answer_terminal_keys_cover_following_turn_completed() {
        let params = serde_json::json!({
            "threadId": "parent-thread",
            "turnId": "turn-1",
            "item": {
                "id": "msg-final-1",
                "type": "agentMessage",
                "phase": "final_answer",
                "text": "done"
            }
        });
        let mut observed = HashSet::new();
        let final_answer_keys = codex_terminal_observation_keys(
            &params,
            Some("turn-1"),
            Some("turn-1"),
            Some("parent-thread"),
            true,
        );
        codex_mark_terminal_observed(&mut observed, &final_answer_keys);

        let turn_completed_keys = codex_terminal_observation_keys(
            &serde_json::json!({}),
            Some("turn-1"),
            None,
            Some("parent-thread"),
            false,
        );

        assert!(codex_any_terminal_observed(&observed, &turn_completed_keys));
        assert!(codex_terminal_notification_already_observed(
            "turn/completed",
            false,
            true,
        ));
    }

    #[test]
    fn translate_informational_notifications_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let empty = serde_json::json!({});
        let methods = [
            "turn/started",
            "thread/started",
            "thread/tokenUsage/updated",
            "account/rateLimits/updated",
            "item/commandExecution/terminalInteraction",
            "mcpServer/startupStatus/updated",
            "configWarning",
        ];
        for method in &methods {
            translate_notification(method, &empty, &tx);
            assert!(
                rx.try_recv().is_err(),
                "{} should not emit any event",
                method
            );
        }
    }

    #[test]
    fn translate_unknown_method_does_not_panic() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({});
        // Should log a warning but not panic
        translate_notification("some/unknown/method", &params, &tx);
    }

    #[test]
    fn extract_turn_id_top_level_camelcase() {
        let v = serde_json::json!({"turnId": "t-123"});
        assert_eq!(extract_turn_id(&v), Some("t-123".to_string()));
    }

    #[test]
    fn extract_turn_id_snake_case() {
        let v = serde_json::json!({"turn_id": "t-456"});
        assert_eq!(extract_turn_id(&v), Some("t-456".to_string()));
    }

    #[test]
    fn extract_turn_id_nested_turn_object() {
        let v = serde_json::json!({"turn": {"id": "t-789"}});
        assert_eq!(extract_turn_id(&v), Some("t-789".to_string()));
    }

    #[test]
    fn extract_turn_id_nested_thread_last_turn() {
        let v = serde_json::json!({"thread": {"lastTurnId": "t-last"}});
        assert_eq!(extract_turn_id(&v), Some("t-last".to_string()));
    }

    #[test]
    fn extract_turn_id_missing() {
        let v = serde_json::json!({"other": "value"});
        assert_eq!(extract_turn_id(&v), None);
    }

    #[test]
    fn extract_turn_id_empty_string_is_none() {
        let v = serde_json::json!({"turnId": ""});
        assert_eq!(extract_turn_id(&v), None);
    }

    #[test]
    fn permission_approval_label_summarizes_requested_grant() {
        let params = serde_json::json!({
            "cwd": "/tmp/repo",
            "reason": "need wasm cache",
            "permissions": {
                "network": {"enabled": true},
                "fileSystem": {"write": ["/tmp/repo"]}
            }
        });

        let label = codex_permissions_approval_label(&params);

        assert!(label.contains("permission grant"));
        assert!(label.contains("network"));
        assert!(label.contains("filesystem"));
        assert!(label.contains("need wasm cache"));
        assert!(label.contains("/tmp/repo"));
    }

    #[test]
    fn permission_approval_accept_grants_requested_permissions() {
        let requested = serde_json::json!({
            "network": {"enabled": true},
            "fileSystem": {"write": ["/tmp/repo"]}
        });
        let params = serde_json::json!({
            "permissions": requested.clone()
        });

        let response = codex_permissions_approval_response(&params, ApprovalDecision::Accept);

        assert_eq!(response["permissions"], requested);
        assert_eq!(response["scope"], "turn");
        assert_eq!(response["strictAutoReview"], false);
    }

    #[test]
    fn permission_approval_accept_for_session_uses_session_scope() {
        let params = serde_json::json!({
            "permissions": {
                "fileSystem": {"write": ["/tmp/repo"]}
            }
        });

        let response =
            codex_permissions_approval_response(&params, ApprovalDecision::AcceptForSession);

        assert_eq!(response["scope"], "session");
        assert_eq!(
            response["permissions"],
            serde_json::json!({"fileSystem": {"write": ["/tmp/repo"]}})
        );
    }

    #[test]
    fn permission_approval_decline_grants_empty_permissions() {
        let params = serde_json::json!({
            "permissions": {
                "network": {"enabled": true},
                "fileSystem": {"write": ["/tmp/repo"]}
            }
        });

        let response = codex_permissions_approval_response(&params, ApprovalDecision::Decline);

        assert_eq!(response["permissions"], serde_json::json!({}));
        assert_eq!(response["scope"], "turn");
        assert_eq!(response["strictAutoReview"], false);
    }

    #[test]
    fn malformed_goal_notifications_do_not_emit_badges_or_clear_noise() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        let update = serde_json::json!({
            "threadId": "thread-abc",
            "goal": {
                "threadId": "thread-abc",
                "status": "active"
            }
        });

        translate_notification_with_state("thread/goal/updated", &update, &tx, &mut state);
        assert!(
            rx.try_recv().is_err(),
            "malformed goal updates should not create visible goal state"
        );

        translate_notification_with_state(
            "thread/goal/cleared",
            &serde_json::json!({ "threadId": "thread-abc" }),
            &tx,
            &mut state,
        );
        assert!(
            rx.try_recv().is_err(),
            "ignored malformed updates should not make later startup clears noisy"
        );
    }

    #[test]
    fn goal_notifications_emit_structured_goal_updates_without_log_spam() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "turnId": null,
            "goal": {
                "threadId": "thread-abc",
                "objective": "Ship feature parity",
                "status": "paused",
                "tokenBudget": null,
                "tokensUsed": 10,
                "timeUsedSeconds": 2,
                "createdAt": 1776272400,
                "updatedAt": 1776272402
            }
        });
        translate_notification("thread/goal/updated", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::GoalUpdated { goal } => {
                assert_eq!(goal.objective, "Ship feature parity");
                assert_eq!(goal.status.as_deref(), Some("paused"));
                assert_eq!(goal.tokens_used, Some(10));
                assert_eq!(goal.elapsed_seconds, Some(2));
            }
            other => panic!("expected GoalUpdated, got {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "goal updates should not emit normal log entries"
        );
    }

    #[test]
    fn startup_goal_cleared_notification_is_silent_until_goal_seen() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        let params = serde_json::json!({ "threadId": "thread-abc" });

        translate_notification_with_state("thread/goal/cleared", &params, &tx, &mut state);

        assert!(
            rx.try_recv().is_err(),
            "cleared notifications without known prior goal are startup noise"
        );
    }

    #[test]
    fn goal_cleared_notification_logs_after_goal_update() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CodexNotificationState::default();
        let update = serde_json::json!({
            "threadId": "thread-abc",
            "goal": {
                "threadId": "thread-abc",
                "objective": "Ship feature parity",
                "status": "active"
            }
        });
        let clear = serde_json::json!({ "threadId": "thread-abc" });

        translate_notification_with_state("thread/goal/updated", &update, &tx, &mut state);
        match rx
            .try_recv()
            .expect("goal update should publish structured state")
        {
            AgentEvent::GoalUpdated { goal } => {
                assert_eq!(goal.objective, "Ship feature parity");
                assert_eq!(goal.status.as_deref(), Some("active"));
            }
            other => panic!("expected GoalUpdated, got {:?}", other),
        }

        translate_notification_with_state("thread/goal/cleared", &clear, &tx, &mut state);

        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "Codex goal cleared");
            }
            other => panic!("expected Log, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentEvent::GoalCleared => {}
            other => panic!("expected GoalCleared, got {:?}", other),
        }
    }

    #[test]
    fn thread_name_notifications_emit_log_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "threadName": "Ship feature parity"
        });
        translate_notification("thread/name/updated", &params, &tx);
        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert!(message.contains("Ship feature parity"));
            }
            other => panic!("expected Log, got {:?}", other),
        }
    }

    #[test]
    fn thread_settings_updated_surfaces_effective_cwd() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "threadSettings": {
                "cwd": "/home/user/projects/intendant-original"
            }
        });

        translate_notification("thread/settings/updated", &params, &tx);

        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(
                    message,
                    "Codex thread settings applied: cwd /home/user/projects/intendant-original"
                );
            }
            other => panic!("expected Log, got {:?}", other),
        }
    }
}
