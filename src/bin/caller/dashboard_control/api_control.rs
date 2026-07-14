//! Daemon-control requests: settings, displays, voice sessions, state
//! and browser-workspace snapshots, log replay, dashboard/display
//! bootstrap, WebRTC signaling, input authority, worktrees, managed
//! context, MCP tool calls, ControlMsg dispatch, and the peer/pairing
//! and coordinator surfaces.

use super::*;

pub(crate) async fn api_settings_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let runtime_settings = {
        let session = runtime.shared_session.read().await;
        session.runtime_settings.clone()
    };
    frame_api_json_body_response(
        id,
        crate::web_gateway::settings_get_api_response(
            runtime.project_root.as_deref(),
            &runtime_settings,
        )
        .await,
        "settings",
    )
}

pub(crate) async fn api_displays_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    frame_api_json_body_response(
        id,
        crate::web_gateway::displays_api_response(&session_registry).await,
        "displays",
    )
}

pub(crate) async fn api_voice_session_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let provider = runtime
        .config
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("gemini");
    let model = runtime
        .config
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match crate::web_gateway::mint_session_token(provider, model).await {
        Ok(body) => http_body_response(id, 200, body, "voice session"),
        Err(msg) => http_body_response(
            id,
            502,
            serde_json::json!({ "error": msg }).to_string(),
            "voice session",
        ),
    }
}

pub(crate) async fn api_browser_workspace_snapshot_response(id: String) -> serde_json::Value {
    let workspaces = crate::browser_workspace::list_workspaces().await;
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "t": "browser_workspace_snapshot",
            "workspaces": workspaces,
        },
    })
}

pub(crate) async fn api_state_snapshot_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (daemon_session_id, query_ctx, session_log) = {
        let session = runtime.shared_session.read().await;
        (
            session.daemon_session_id.clone(),
            session.query_ctx.clone(),
            session.session_log.clone(),
        )
    };
    let state = query_ctx
        .as_ref()
        .map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    let bootstrap_session_id = daemon_session_id
        .or_else(|| {
            query_ctx
                .as_ref()
                .and_then(|ctx| control_replay_session_id_from_dir(&ctx.log_dir))
        })
        .or_else(|| session_log.as_ref().and_then(control_session_log_id))
        .unwrap_or_default();

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "t": "state_snapshot",
            "state": state,
            "connection_id": runtime.session_id.clone(),
            "config": runtime.config.clone(),
            "session_id": bootstrap_session_id,
        },
    })
}

pub(crate) async fn api_session_log_replay_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let replay_log_dir = active_replay_log_dir(runtime).await;
    // The replay conversion reads and converts the whole tail-limited
    // session log — blocking work, so it runs on the blocking pool
    // instead of stalling a runtime worker (the request lane spawns these
    // handlers as plain async tasks).
    let converted = match replay_log_dir {
        Some(log_dir) => tokio::task::spawn_blocking(move || {
            crate::web_gateway::session_log_replay_payload_for_websocket_bootstrap(&log_dir)
        })
        .await
        .unwrap_or_else(|e| {
            eprintln!("[dashboard-control] session log replay task failed: {e}");
            None
        }),
        None => None,
    };
    let mut replay = converted
        .and_then(|(payload, external_session_id)| {
            let mut value = serde_json::from_str::<serde_json::Value>(&payload).ok()?;
            if let (Some(external_session_id), Some(map)) =
                (external_session_id, value.as_object_mut())
            {
                map.insert(
                    "external_session_id".to_string(),
                    serde_json::Value::String(external_session_id),
                );
            }
            Some(value)
        })
        .unwrap_or_else(|| {
            serde_json::json!({
                "t": "log_replay",
                "entries": [],
                "available": false,
            })
        });
    if let Some(map) = replay.as_object_mut() {
        map.entry("available".to_string())
            .or_insert(serde_json::Value::Bool(true));
    }

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": replay,
    })
}

pub(crate) async fn api_dashboard_bootstrap_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let mut frames = Vec::new();
    if let Some(frame) =
        response_result(api_state_snapshot_response("bootstrap-state".into(), runtime).await)
    {
        frames.push(frame);
    }
    if let Some(result) = response_result(cached_bootstrap_events_response_frame(
        "bootstrap-cached".into(),
        &runtime.bootstrap_caches,
    )) {
        if let Some(events) = result.get("events").and_then(|value| value.as_array()) {
            frames.extend(events.iter().cloned());
        }
    }
    if let Some(frame) =
        response_result(api_browser_workspace_snapshot_response("bootstrap-browser".into()).await)
    {
        frames.push(frame);
    }
    frames.extend(display_ready_bootstrap_frames(runtime).await);
    let mut replayed_external_session_ids = HashSet::new();
    if let Some(frame) =
        response_result(api_session_log_replay_response("bootstrap-replay".into(), runtime).await)
    {
        if let Some(external_session_id) = frame
            .get("external_session_id")
            .and_then(|value| value.as_str())
        {
            replayed_external_session_ids.insert(external_session_id.to_string());
        }
        frames.push(frame);
    }
    frames.extend(
        external_session_activity_replay_frames(runtime, &replayed_external_session_ids).await,
    );
    frames.extend(display_authority_snapshot_frames(runtime).await);
    let frame_count = frames.len();
    let omitted = dashboard_bootstrap_omitted(runtime);

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
            "omitted": omitted,
        },
    })
}

pub(crate) async fn api_display_bootstrap_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let mut frames = display_ready_bootstrap_frames(runtime).await;
    frames.extend(display_authority_snapshot_frames(runtime).await);
    let frame_count = frames.len();
    let omitted = display_bootstrap_omitted(runtime);
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
            "omitted": omitted,
        },
    })
}

pub(crate) async fn api_display_webrtc_signal_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let signal = string_param(&params, &["signal", "kind", "type", "t"]);
    match signal.as_str() {
        "offer" | "display_offer" => api_display_webrtc_offer_response(id, &params, runtime).await,
        "ice" | "candidate" | "display_ice" => {
            api_display_webrtc_ice_response(id, &params, runtime).await
        }
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "missing or unknown display webrtc signal",
        }),
    }
}

pub(crate) async fn api_display_webrtc_offer_response(
    id: String,
    params: &serde_json::Value,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let display_id = display_id_param(Some(params));
    let sdp = string_param(params, &["sdp", "offer", "offer_sdp"]);
    if sdp.is_empty() {
        return missing_param_response(id, "sdp");
    }
    let Some(display_session) = active_display_session(runtime, display_id).await else {
        return display_signal_error_response(id, 404, display_id, "display session not found");
    };

    let (ice_tx, mut ice_rx) = mpsc::channel::<(crate::display::PeerId, String)>(64);
    if let Some(control_frames_tx) = runtime.control_frames_tx.clone() {
        tokio::spawn(async move {
            while let Some((_peer_id, candidate_json)) = ice_rx.recv().await {
                let candidate =
                    serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default();
                let payload = serde_json::json!({
                    "t": "display_ice",
                    "display_id": display_id,
                    "candidate": candidate,
                });
                let frame = serde_json::json!({
                    "t": "event",
                    "payload": payload,
                });
                if control_frames_tx.send(frame).is_err() {
                    break;
                }
            }
        });
    }

    let input_authorized = dashboard_display_input_authorizer(
        runtime.display_authority.clone(),
        runtime.session_id.clone(),
        display_id,
    );
    let authority_handler = crate::display::webrtc::noop_authority_handler();
    match display_session
        .handle_offer(
            runtime.display_peer_id,
            &sdp,
            &runtime.ice_config,
            Some(Arc::clone(&runtime.tcp_peer_registry)),
            runtime.tcp_advertised,
            ice_tx,
            input_authorized,
            authority_handler,
        )
        .await
    {
        Ok(answer_sdp) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": {
                "t": "display_answer",
                "display_id": display_id,
                "sdp": answer_sdp,
            },
        }),
        Err(e) => display_signal_error_response(
            id,
            502,
            display_id,
            &format!("display offer failed: {e}"),
        ),
    }
}

pub(crate) async fn api_display_webrtc_ice_response(
    id: String,
    params: &serde_json::Value,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let display_id = display_id_param(Some(params));
    let Some(candidate) = params.get("candidate").cloned() else {
        return missing_param_response(id, "candidate");
    };
    if candidate.is_null() {
        return missing_param_response(id, "candidate");
    }
    let Some(display_session) = active_display_session(runtime, display_id).await else {
        return display_signal_error_response(id, 404, display_id, "display session not found");
    };
    let candidate = candidate.to_string();
    let peer_id = runtime.display_peer_id;
    tokio::spawn(async move {
        if let Err(e) = display_session.add_ice_candidate(peer_id, &candidate).await {
            eprintln!(
                "[dashboard/control] display ICE candidate failed for display {display_id}: {e}"
            );
        }
    });
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
        },
    })
}

pub(crate) async fn active_display_session(
    runtime: &ControlRuntime,
    display_id: u32,
) -> Option<Arc<crate::display::DisplaySession>> {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    }?;
    let registry = session_registry.read().await;
    registry.get(display_id)
}

pub(crate) fn dashboard_display_input_authorizer(
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    session_id: String,
    display_id: u32,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || match display_authority.as_ref() {
        Some(bridge) => bridge.input_authorized(&session_id, display_id),
        None => true,
    })
}

pub(crate) fn display_signal_error_response(
    id: String,
    status: u16,
    display_id: u32,
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "status": status,
        "display_id": display_id,
        "error": error,
    })
}

pub(crate) async fn api_display_input_authority_snapshot_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let frames = display_authority_snapshot_frames(runtime).await;
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "available": runtime.display_authority.is_some(),
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

pub(crate) async fn api_display_input_authority_request_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return display_authority_unavailable_response(id);
    };
    let display_id = display_id_param(params);
    let frames = bridge.request(&runtime.session_id, display_id);
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

pub(crate) async fn api_display_input_authority_release_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return display_authority_unavailable_response(id);
    };
    let display_id = display_id_param(params);
    let frames = bridge.release(&runtime.session_id, display_id);
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "display_id": display_id,
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

pub(crate) fn display_authority_unavailable_response(id: String) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": false,
            "available": false,
            "_httpStatus": 503,
            "_httpOk": false,
            "error": "display input authority unavailable",
        },
    })
}

pub(crate) fn display_id_param(params: Option<&serde_json::Value>) -> u32 {
    params
        .and_then(|params| {
            params
                .get("display_id")
                .or_else(|| params.get("displayId"))
                .or_else(|| params.get("id"))
        })
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

pub(crate) async fn display_authority_snapshot_frames(
    runtime: &ControlRuntime,
) -> Vec<serde_json::Value> {
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return Vec::new();
    };
    let display_ids = active_display_ids(runtime).await;
    bridge.snapshot(&runtime.session_id, &display_ids)
}

pub(crate) fn dashboard_bootstrap_omitted(runtime: &ControlRuntime) -> Vec<&'static str> {
    if runtime.display_authority.is_some() {
        Vec::new()
    } else {
        vec!["display_input_authority_state"]
    }
}

pub(crate) fn display_bootstrap_omitted(runtime: &ControlRuntime) -> Vec<&'static str> {
    if runtime.display_authority.is_some() {
        Vec::new()
    } else {
        vec!["display_input_authority_state"]
    }
}

pub(crate) async fn display_ready_bootstrap_frames(
    runtime: &ControlRuntime,
) -> Vec<serde_json::Value> {
    let display_ids = active_display_ids(runtime).await;
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        return Vec::new();
    };

    let registry = session_registry.read().await;
    display_ids
        .into_iter()
        .filter_map(|display_id| {
            registry.get(display_id).map(|session| {
                let (width, height) = session.resolution();
                serde_json::json!({
                    "event": "display_ready",
                    "display_id": display_id,
                    "width": width,
                    "height": height,
                })
            })
        })
        .collect()
}

pub(crate) async fn active_display_ids(runtime: &ControlRuntime) -> Vec<u32> {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        return Vec::new();
    };

    let registry = session_registry.read().await;
    let mut display_ids = registry.display_ids();
    display_ids.sort_unstable();
    display_ids
}

pub(crate) async fn api_external_session_activity_replay_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let frames = external_session_activity_replay_frames(runtime, &HashSet::new()).await;
    let frame_count = frames.len();
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "frames": frames,
            "frame_count": frame_count,
        },
    })
}

pub(crate) async fn external_session_activity_replay_frames(
    runtime: &ControlRuntime,
    skip_session_ids: &HashSet<String>,
) -> Vec<serde_json::Value> {
    let mut active_external_sessions: Vec<(String, String)> = runtime
        .bootstrap_caches
        .attached_external_sessions
        .lock()
        .ok()
        .map(|guard| {
            guard
                .iter()
                .map(|(session_id, source)| (session_id.clone(), source.clone()))
                .collect()
        })
        .unwrap_or_default();
    active_external_sessions.sort_by(|a, b| a.0.cmp(&b.0));
    let to_convert: Vec<(String, String)> = active_external_sessions
        .into_iter()
        .filter(|(session_id, _)| !skip_session_ids.contains(session_id))
        .collect();
    // Each conversion reads and converts a backend-native session file —
    // blocking disk work, off the runtime workers (see
    // api_session_log_replay_response).
    tokio::task::spawn_blocking(move || {
        to_convert
            .into_iter()
            .filter_map(|(session_id, source)| {
                crate::web_gateway::external_session_activity_replay_for_websocket(
                    &source,
                    &session_id,
                )
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
            })
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        eprintln!("[dashboard-control] external session replay task failed: {e}");
        Vec::new()
    })
}

pub(crate) fn response_result(response: serde_json::Value) -> Option<serde_json::Value> {
    response.get("result").cloned()
}

pub(crate) fn control_replay_session_id_from_dir(log_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(log_dir.join("session_meta.json"))
        .ok()
        .and_then(|meta| serde_json::from_str::<crate::session_log::SessionMeta>(&meta).ok())
        .map(|meta| meta.session_id)
        .filter(|session_id| !session_id.trim().is_empty())
        .or_else(|| {
            log_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|session_id| !session_id.trim().is_empty())
        })
}

pub(crate) fn control_session_log_id(
    session_log: &Arc<std::sync::Mutex<crate::session_log::SessionLog>>,
) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.trim().is_empty())
}

pub(crate) async fn active_replay_log_dir(runtime: &ControlRuntime) -> Option<PathBuf> {
    let (query_ctx, session_log) = {
        let session = runtime.shared_session.read().await;
        (session.query_ctx.clone(), session.session_log.clone())
    };
    query_ctx
        .as_ref()
        .map(|ctx| ctx.log_dir.clone())
        .or_else(|| {
            session_log
                .as_ref()
                .and_then(|log| log.lock().ok().map(|log| log.dir().to_path_buf()))
        })
}

pub(crate) async fn api_worktrees_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    frame_api_json_body_response(
        id,
        crate::web_gateway::worktrees_list_api_response(&runtime.worktree_inventory_cache),
        "worktrees",
    )
}

pub(crate) async fn api_worktrees_inspect_response(
    id: String,
    params: Option<&serde_json::Value>,
    _runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::worktrees_inspect_api_response(&home, &body_text)
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "worktree inspect"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "ok": false,
                "error": format!("worktree inspect task failed: {e}")
            })
            .to_string(),
            "worktree inspect",
        ),
    }
}

pub(crate) async fn api_worktrees_scan_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let project_root = runtime.project_root.clone();
    let cache = runtime.worktree_inventory_cache.clone();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::worktrees_scan_api_response(&home, project_root.as_deref(), &cache)
    })
    .await;
    match result {
        Ok(response) => frame_api_json_body_response(id, response, "worktree scan"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "error": format!("worktree scan task failed: {e}")
            })
            .to_string(),
            "worktree scan",
        ),
    }
}

pub(crate) async fn api_worktrees_remove_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let cache = runtime.worktree_inventory_cache.clone();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::worktrees_remove_api_response(&home, &body_text, &cache)
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "worktree remove"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "ok": false,
                "error": format!("worktree removal task failed: {e}")
            })
            .to_string(),
            "worktree remove",
        ),
    }
}

pub(crate) async fn api_worktrees_merge_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let cache = runtime.worktree_inventory_cache.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = crate::web_gateway::merge_session_worktree_response(&home, &body_text);
        if result.0 == "200 OK" {
            if let Ok(mut guard) = cache.lock() {
                *guard = None;
            }
        }
        result
    })
    .await;
    match result {
        Ok((status_line, body)) => {
            http_body_response(id, status_line_code(status_line), body, "worktree merge")
        }
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({
                "ok": false,
                "error": format!("worktree merge task failed: {e}")
            })
            .to_string(),
            "worktree merge",
        ),
    }
}

pub(crate) async fn api_managed_context_response(
    id: String,
    kind: &'static str,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    // Transport edge: resolve the real home once; the parity fixture
    // drives the `_from_home` variant with an injected temp home.
    api_managed_context_response_from_home(id, kind, params, runtime, &crate::platform::home_dir())
        .await
}

pub(crate) async fn api_managed_context_response_from_home(
    id: String,
    kind: &'static str,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
    home: &std::path::Path,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let Some(request_line) = managed_context_request_line(kind, &params) else {
        return missing_param_response(id, "query");
    };
    let active_log_dir = match active_session_log_dir(runtime).await {
        Ok(dir) => dir,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "managed context",
            );
        }
    };
    let home = home.to_path_buf();
    let response = tokio::task::spawn_blocking(move || match kind {
        "records" => crate::web_gateway::managed_context_records_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        "anchors" => crate::web_gateway::managed_context_anchors_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        "fission" => crate::web_gateway::managed_context_fission_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
        _ => crate::web_gateway::managed_context_records_response_from_home(
            &request_line,
            active_log_dir.as_deref(),
            &home,
        ),
    })
    .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("managed context task failed: {e}"),
            });
        }
    };
    frame_api_response(id, response, "managed context")
}

pub(crate) async fn api_mcp_tool_call_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let mcp_id = params
        .get("mcp_id")
        .or_else(|| params.get("rpc_id"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!(id.clone()));
    let Some(server) = runtime.mcp_server.as_ref() else {
        return http_body_response(
            id,
            503,
            mcp_error_body(mcp_id, -32603, "MCP server not available"),
            "mcp tool call",
        );
    };
    let session_id = optional_string_param(
        &params,
        &["session_id", "session", "intendant_session", "sessionId"],
    );
    if session_id.is_none() {
        return http_body_response(
            id,
            400,
            mcp_error_body(mcp_id, -32602, "missing session_id"),
            "mcp tool call",
        );
    }
    let name = string_param(&params, &["name", "tool", "tool_name"]);
    if name.is_empty() {
        return http_body_response(
            id,
            400,
            mcp_error_body(mcp_id, -32602, "missing tool name"),
            "mcp tool call",
        );
    }
    // Layered on top of the dispatch-level `message.send` gate: the named
    // tool must also clear its own IAM operation, so a principal scoped to
    // messaging cannot reach display input or runtime control through the
    // generic tool-call RPC.
    let decision = runtime
        .grant
        .access_decision(crate::mcp::mcp_tool_operation(&name));
    if !decision.allowed {
        return http_body_response(
            id,
            403,
            mcp_error_body(
                mcp_id,
                -32603,
                &format!(
                    "permission denied for tool '{name}': {} (permission {})",
                    decision.reason, decision.permission
                ),
            ),
            "mcp tool call",
        );
    }
    let arguments = params
        .get("arguments")
        .or_else(|| params.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let managed_context = optional_managed_context_param(&params);
    match server
        .call_tool_by_name_as_caller(
            &name,
            arguments,
            session_id.as_deref(),
            managed_context,
            crate::mcp::ToolCallerTrust::from_principal(&runtime.grant.access_principal()),
        )
        .await
    {
        Ok(result) => {
            let result = serde_json::to_value(result).unwrap_or_else(|e| {
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Failed to serialize MCP tool result: {}", e),
                    }],
                    "isError": true,
                })
            });
            http_body_response(
                id,
                200,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": mcp_id,
                    "result": result,
                })
                .to_string(),
                "mcp tool call",
            )
        }
        Err(error) => http_body_response(
            id,
            200,
            mcp_error_body(mcp_id, -32603, &error),
            "mcp tool call",
        ),
    }
}

pub(crate) fn mcp_error_body(id: serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
    .to_string()
}

pub(crate) fn optional_managed_context_param(params: &serde_json::Value) -> Option<bool> {
    for name in ["managed_context", "managedContext", "codex_managed_context"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        if let Some(flag) = value.as_bool() {
            return Some(flag);
        }
        if let Some(mode) = value.as_str() {
            return Some(crate::project::codex_managed_context_enabled(mode));
        }
    }
    None
}

pub(crate) async fn api_settings_save_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    frame_api_response(
        id,
        crate::web_gateway::settings_post_api_response(
            &body_text,
            runtime.project_root.as_deref(),
            &runtime.bus,
        ),
        "settings save",
    )
}

pub(crate) async fn api_control_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
    };
    if !dashboard_control_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "control message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[dashboard-control] ControlMsg: {action}"),
        level: Some(crate::types::LogLevel::Debug),
        turn: None,
    });
    runtime.bus.send(AppEvent::ControlCommand(ctrl));
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "action": action,
        },
    })
}

pub(crate) async fn api_session_control_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
    };
    if !dashboard_session_control_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard session WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "session control message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    dispatch_dashboard_control_msg(&runtime.bus, ctrl, "session-control");
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "ok": true,
            "action": action,
        },
    })
}

pub(crate) async fn api_dashboard_action_msg_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let ctrl = match dashboard_control_msg_from_params(id.clone(), params) {
        Ok(ctrl) => ctrl,
        Err(response) => return response,
    };
    if !dashboard_action_msg_allowed(&ctrl) {
        return http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "control action not available over dashboard action WebRTC: {}",
                    dashboard_control_msg_action(&ctrl)
                ),
            })
            .to_string(),
            "dashboard action message",
        );
    }
    let action = dashboard_control_msg_action(&ctrl);
    let marker_apply = match &ctrl {
        ControlMsg::SetDiagnosticsVisualMarker {
            display_id,
            enabled,
        } => {
            let display_id = display_id.unwrap_or(0);
            Some((
                display_id,
                apply_dashboard_diagnostics_visual_marker(runtime, display_id, *enabled).await,
            ))
        }
        _ => None,
    };
    dispatch_dashboard_control_msg(&runtime.bus, ctrl, "dashboard-action");
    let mut result = serde_json::json!({
        "ok": true,
        "action": action,
    });
    if let Some((display_id, marker_result)) = marker_apply {
        if let Some(result_obj) = result.as_object_mut() {
            result_obj.insert("display_id".to_string(), serde_json::json!(display_id));
            result_obj.insert(
                "registry_available".to_string(),
                serde_json::json!(marker_result.registry_available),
            );
            result_obj.insert(
                "active_display_updated".to_string(),
                serde_json::json!(marker_result.active_display_updated),
            );
        }
    }
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": result,
    })
}

/// `state_dir` arrives from the dispatch arm — the transport edge
/// resolves `platform::intendant_home()`, so tests inject a tempdir
/// instead of appending to the live diagnostics store (the CLAUDE.md
/// tests-are-hermetic convention).
pub(crate) async fn api_diagnostics_visual_freshness_response(
    id: String,
    params: Option<&serde_json::Value>,
    state_dir: PathBuf,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return missing_param_response(id, "session_id");
    }
    let body = params
        .get("body")
        .or_else(|| params.get("ndjson"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .unwrap_or_default();
    if body.is_empty() {
        return missing_param_response(id, "body");
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::diagnostics_visual_freshness_api_response(
            &state_dir,
            &session_id,
            body.as_bytes(),
        )
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "diagnostics visual freshness"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({"error": format!("diagnostics append task failed: {e}")})
                .to_string(),
            "diagnostics visual freshness",
        ),
    }
}

pub(crate) fn dashboard_control_msg_from_params(
    id: String,
    params: Option<&serde_json::Value>,
) -> Result<ControlMsg, serde_json::Value> {
    let Some(params) = params else {
        return Err(missing_param_response(id, "message"));
    };
    let message = params
        .get("message")
        .or_else(|| params.get("control_msg"))
        .or_else(|| params.get("controlMsg"))
        .cloned()
        .unwrap_or_else(|| params.clone());
    serde_json::from_value::<ControlMsg>(message).map_err(|e| {
        http_body_response(
            id,
            400,
            serde_json::json!({
                "ok": false,
                "error": format!("invalid control message: {e}"),
            })
            .to_string(),
            "control message",
        )
    })
}

pub(crate) fn dispatch_dashboard_control_msg(
    bus: &crate::event::EventBus,
    ctrl: ControlMsg,
    scope: &str,
) {
    let action = dashboard_control_msg_action(&ctrl);
    bus.send(AppEvent::PresenceLog {
        message: format!("[dashboard-control:{scope}] ControlMsg: {action}"),
        level: Some(crate::types::LogLevel::Debug),
        turn: None,
    });
    bus.send(AppEvent::ControlCommand(ctrl));
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DiagnosticsVisualMarkerApply {
    registry_available: bool,
    active_display_updated: bool,
}

pub(crate) async fn apply_dashboard_diagnostics_visual_marker(
    runtime: &ControlRuntime,
    display_id: u32,
    enabled: bool,
) -> DiagnosticsVisualMarkerApply {
    let session_registry = {
        let session = runtime.shared_session.read().await;
        session.session_registry.clone()
    };
    let Some(session_registry) = session_registry else {
        eprintln!(
            "[dashboard/control] diagnostics visual marker request for display {display_id} ({enabled}) ignored; no session registry"
        );
        return DiagnosticsVisualMarkerApply {
            registry_available: false,
            active_display_updated: false,
        };
    };

    let active_display_updated = session_registry
        .write()
        .await
        .set_diagnostics_visual_marker(display_id, enabled);
    eprintln!(
        "[dashboard/control] diagnostics visual marker for display {display_id} = {enabled}{}",
        if active_display_updated {
            ""
        } else {
            " (pending)"
        },
    );
    DiagnosticsVisualMarkerApply {
        registry_available: true,
        active_display_updated,
    }
}

pub(crate) fn dashboard_control_msg_allowed(ctrl: &ControlMsg) -> bool {
    matches!(
        ctrl,
        ControlMsg::SetAutonomy { .. }
            | ControlMsg::SetApprovalRule { .. }
            | ControlMsg::SetExternalAgent { .. }
            | ControlMsg::SetCodexCommand { .. }
            | ControlMsg::SetCodexManagedCommand { .. }
            | ControlMsg::SetCodexSandbox { .. }
            | ControlMsg::SetCodexApprovalPolicy { .. }
            | ControlMsg::SetCodexModel { .. }
            | ControlMsg::SetCodexReasoningEffort { .. }
            | ControlMsg::SetCodexServiceTier { .. }
            | ControlMsg::SetCodexWebSearch { .. }
            | ControlMsg::SetCodexNetworkAccess { .. }
            | ControlMsg::SetCodexWritableRoots { .. }
            | ControlMsg::SetCodexManagedContext { .. }
            | ControlMsg::SetCodexContextArchive { .. }
            | ControlMsg::SetClaudeModel { .. }
            | ControlMsg::SetClaudePermissionMode { .. }
            | ControlMsg::SetClaudeAllowedTools { .. }
            | ControlMsg::SetVerbosity { .. }
    )
}

pub(crate) fn dashboard_session_control_msg_allowed(ctrl: &ControlMsg) -> bool {
    matches!(
        ctrl,
        ControlMsg::Approve { .. }
            | ControlMsg::Deny { .. }
            | ControlMsg::Skip { .. }
            | ControlMsg::ApproveAll { .. }
            | ControlMsg::AnswerQuestion { .. }
            | ControlMsg::RenameSession { .. }
            | ControlMsg::ConfigureSessionAgent { .. }
            | ControlMsg::StopSession { .. }
            | ControlMsg::RestartSession { .. }
            | ControlMsg::CreateSession { .. }
            | ControlMsg::SpawnSubAgent { .. }
            | ControlMsg::StartTask { .. }
            | ControlMsg::ResumeSession { .. }
            | ControlMsg::FollowUp { .. }
            | ControlMsg::CancelFollowUp { .. }
            | ControlMsg::EditUserMessage { .. }
            | ControlMsg::Interrupt { .. }
            | ControlMsg::Steer { .. }
            | ControlMsg::CancelSteer { .. }
    )
}

/// The "dashboard action" RPC lane's allowlist, by wire action name. Single
/// declaration: `dashboard_action_msg_allowed` gates against it, and the
/// parity test below pins the SPA's `DASHBOARD_ACTION_MSG_RPC_ACTIONS`
/// mirror (static/app/31-init-identity-fleet.js) to this exact set — adding
/// an action here without the frontend mirror (or vice versa) fails the
/// suite instead of shipping as drift. Fail-closed: a new `ControlMsg`
/// variant is not dispatchable over this lane until named here.
pub(crate) const DASHBOARD_ACTION_MSG_ACTIONS: &[&str] = &[
    "codex_thread_action",
    "take_display",
    "release_display",
    "grant_user_display",
    "revoke_user_display",
    "resolve_display_request",
    "create_virtual_display",
    "create_browser_workspace",
    "close_browser_workspace",
    "acquire_browser_workspace",
    "release_browser_workspace",
    "setup_debug_screen",
    "teardown_debug_screen",
    "start_debug_recording",
    "stop_debug_recording",
    "start_recording",
    "stop_recording",
    "delete_recording",
    "set_diagnostics_visual_marker",
];

pub(crate) fn dashboard_action_msg_allowed(ctrl: &ControlMsg) -> bool {
    DASHBOARD_ACTION_MSG_ACTIONS.contains(&dashboard_control_msg_action(ctrl))
}

pub(crate) fn dashboard_control_msg_action(ctrl: &ControlMsg) -> &'static str {
    match ctrl {
        ControlMsg::Status { .. } => "status",
        ControlMsg::Usage => "usage",
        ControlMsg::Approve { .. } => "approve",
        ControlMsg::Deny { .. } => "deny",
        ControlMsg::Skip { .. } => "skip",
        ControlMsg::ApproveAll { .. } => "approve_all",
        ControlMsg::AnswerQuestion { .. } => "answer_question",
        ControlMsg::Input { .. } => "input",
        ControlMsg::SetAutonomy { .. } => "set_autonomy",
        ControlMsg::SetApprovalRule { .. } => "set_approval_rule",
        ControlMsg::SetExternalAgent { .. } => "set_external_agent",
        ControlMsg::SetCodexCommand { .. } => "set_codex_command",
        ControlMsg::SetCodexManagedCommand { .. } => "set_codex_managed_command",
        ControlMsg::SetCodexSandbox { .. } => "set_codex_sandbox",
        ControlMsg::SetCodexApprovalPolicy { .. } => "set_codex_approval_policy",
        ControlMsg::SetCodexModel { .. } => "set_codex_model",
        ControlMsg::SetCodexReasoningEffort { .. } => "set_codex_reasoning_effort",
        ControlMsg::SetCodexServiceTier { .. } => "set_codex_service_tier",
        ControlMsg::SetCodexWebSearch { .. } => "set_codex_web_search",
        ControlMsg::SetCodexNetworkAccess { .. } => "set_codex_network_access",
        ControlMsg::SetCodexWritableRoots { .. } => "set_codex_writable_roots",
        ControlMsg::SetCodexManagedContext { .. } => "set_codex_managed_context",
        ControlMsg::SetCodexContextArchive { .. } => "set_codex_context_archive",
        ControlMsg::CodexThreadAction { .. } => "codex_thread_action",
        ControlMsg::RenameSession { .. } => "rename_session",
        ControlMsg::ConfigureSessionAgent { .. } => "configure_session_agent",
        ControlMsg::StopSession { .. } => "stop_session",
        ControlMsg::RestartSession { .. } => "restart_session",
        ControlMsg::ResumeSession { .. } => "resume_session",
        ControlMsg::SetClaudeModel { .. } => "set_claude_model",
        ControlMsg::SetClaudePermissionMode { .. } => "set_claude_permission_mode",
        ControlMsg::SetClaudeAllowedTools { .. } => "set_claude_allowed_tools",
        ControlMsg::SetVerbosity { .. } => "set_verbosity",
        ControlMsg::ScheduleControllerRestart { .. } => "schedule_controller_restart",
        ControlMsg::ControllerTurnComplete { .. } => "controller_turn_complete",
        ControlMsg::GetRestartStatus => "get_restart_status",
        ControlMsg::CancelControllerRestart { .. } => "cancel_controller_restart",
        ControlMsg::RequestControllerLoopHalt { .. } => "request_controller_loop_halt",
        ControlMsg::ClearControllerLoopHalt => "clear_controller_loop_halt",
        ControlMsg::InterveneControllerLoop { .. } => "intervene_controller_loop",
        ControlMsg::GetControllerLoopStatus => "get_controller_loop_status",
        ControlMsg::CreateSession { .. } => "create_session",
        ControlMsg::SpawnSubAgent { .. } => "spawn_sub_agent",
        ControlMsg::StartTask { .. } => "start_task",
        ControlMsg::FollowUp { .. } => "follow_up",
        ControlMsg::CancelFollowUp { .. } => "cancel_follow_up",
        ControlMsg::EditUserMessage { .. } => "edit_user_message",
        ControlMsg::QueryDetail { .. } => "query_detail",
        ControlMsg::RecallMemory { .. } => "recall_memory",
        ControlMsg::TakeDisplay { .. } => "take_display",
        ControlMsg::ReleaseDisplay { .. } => "release_display",
        ControlMsg::GrantUserDisplay { .. } => "grant_user_display",
        ControlMsg::RevokeUserDisplay { .. } => "revoke_user_display",
        ControlMsg::ResolveDisplayRequest { .. } => "resolve_display_request",
        ControlMsg::CreateVirtualDisplay { .. } => "create_virtual_display",
        ControlMsg::CreateBrowserWorkspace { .. } => "create_browser_workspace",
        ControlMsg::CloseBrowserWorkspace { .. } => "close_browser_workspace",
        ControlMsg::AcquireBrowserWorkspace { .. } => "acquire_browser_workspace",
        ControlMsg::ReleaseBrowserWorkspace { .. } => "release_browser_workspace",
        ControlMsg::ListDisplays => "list_displays",
        ControlMsg::InvokeSkill { .. } => "invoke_skill",
        ControlMsg::Quit => "quit",
        ControlMsg::SetupDebugScreen => "setup_debug_screen",
        ControlMsg::TeardownDebugScreen => "teardown_debug_screen",
        ControlMsg::StartDebugRecording => "start_debug_recording",
        ControlMsg::StopDebugRecording => "stop_debug_recording",
        ControlMsg::StartRecording { .. } => "start_recording",
        ControlMsg::StopRecording { .. } => "stop_recording",
        ControlMsg::DeleteRecording { .. } => "delete_recording",
        ControlMsg::Interrupt { .. } => "interrupt",
        ControlMsg::Steer { .. } => "steer",
        ControlMsg::CancelSteer { .. } => "cancel_steer",
        ControlMsg::WebRtcSignal { .. } => "webrtc_signal",
        ControlMsg::PeerFileTransferSignal { .. } => "peer_file_transfer_signal",
        ControlMsg::PeerDashboardControlSignal { .. } => "peer_dashboard_control_signal",
        ControlMsg::RequestDisplayInputAuthority { .. } => "request_display_input_authority",
        ControlMsg::ReleaseDisplayInputAuthority { .. } => "release_display_input_authority",
        ControlMsg::SetDiagnosticsVisualMarker { .. } => "set_diagnostics_visual_marker",
    }
}

pub(crate) async fn api_api_keys_save_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    // The transport edge resolves the ambient env path; the persist
    // core below it is path-parameterized (hermeticity convention).
    frame_api_response(
        id,
        crate::web_gateway::api_keys_save_api_response(
            crate::web_gateway::api_keys_env_path().as_deref(),
            &body_text,
        ),
        "api keys save",
    )
}

pub(crate) async fn api_peer_add_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) =
        crate::web_gateway::peers_add(registry, runtime.project_root.as_deref(), &body_text).await;
    http_body_response(id, status, body, "peer add")
}

pub(crate) async fn api_peer_remove_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_remove(registry, &body_text).await;
    http_body_response(id, status, body, "peer remove")
}

pub(crate) async fn api_peer_eligible_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let query = control_capability_query(&params);
    let (status, body) = crate::web_gateway::peers_eligible(registry, &query);
    http_body_response(id, status, body, "eligible peers")
}

pub(crate) async fn api_peer_message_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_send_message(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer message")
}

pub(crate) async fn api_peer_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_delegate_task(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer task")
}

pub(crate) async fn api_peer_approval_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_resolve_approval(registry, &peer_id, &body_text).await;
    http_body_response(id, status, body, "peer approval")
}

pub(crate) async fn api_peer_webrtc_signal_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_webrtc_signal(registry, &peer_id, &body_text, &runtime.bus).await;
    http_body_response(id, status, body, "peer webrtc signal")
}

pub(crate) async fn api_peer_file_transfer_signal_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) = crate::web_gateway::peers_file_transfer_signal(
        registry,
        &peer_id,
        &body_text,
        &runtime.bus,
    )
    .await;
    http_body_response(id, status, body, "peer file-transfer signal")
}

pub(crate) async fn api_peer_dashboard_control_signal_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let peer_id = string_param(&params, &["peer_id", "peerId", "host_id", "hostId", "id"]);
    if peer_id.is_empty() {
        return missing_param_response(id, "peer_id");
    }
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) = crate::web_gateway::peers_dashboard_control_signal(
        registry,
        &peer_id,
        &body_text,
        &runtime.bus,
    )
    .await;
    http_body_response(id, status, body, "peer dashboard-control signal")
}

pub(crate) async fn api_peer_pairing_invite_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_invite(&body_text);
    http_body_response(id, status, body, "peer pairing invite")
}

pub(crate) async fn api_peer_pairing_join_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_join(
        registry,
        runtime.project_root.as_deref(),
        &body_text,
    )
    .await;
    http_body_response(id, status, body, "peer pairing join")
}

// The pairing arms below split transport edge from core (hermeticity
// convention, the sessions family's `_from_home` shape): the ambient
// wrapper resolves the daemon's cert store once, the `_from_cert_dir`
// core is what the parity fixtures drive over injected tempdirs.

pub(crate) async fn api_peer_pairing_request_access_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_request_access_response_from_cert_dir(id, params, &cert_dir).await
}

pub(crate) async fn api_peer_pairing_request_access_response_from_cert_dir(
    id: String,
    params: Option<&serde_json::Value>,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) =
        crate::web_gateway::peers_pairing_request_access(cert_dir, &body_text).await;
    http_body_response(id, status, body, "peer access request")
}

pub(crate) async fn api_peer_pairing_request_access_poll_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_request_access_poll_response_from_cert_dir(id, params, runtime, &cert_dir)
        .await
}

pub(crate) async fn api_peer_pairing_request_access_poll_response_from_cert_dir(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) = crate::web_gateway::peers_pairing_request_access_poll(
        runtime.peer_registry.as_ref(),
        runtime.project_root.as_deref(),
        cert_dir,
        &body_text,
    )
    .await;
    http_body_response(id, status, body, "peer access request poll")
}

pub(crate) async fn api_peer_pairing_requests_response(id: String) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_requests_response_from_cert_dir(id, &cert_dir).await
}

pub(crate) async fn api_peer_pairing_requests_response_from_cert_dir(
    id: String,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let (status, body) = crate::web_gateway::peers_pairing_requests_list(cert_dir);
    http_body_response(id, status, body, "peer access requests")
}

pub(crate) async fn api_peer_pairing_request_decision_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_request_decision_response_from_cert_dir(id, params, &cert_dir).await
}

pub(crate) async fn api_peer_pairing_request_decision_response_from_cert_dir(
    id: String,
    params: Option<&serde_json::Value>,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let request_id = string_param(&params, &["request_id", "requestId", "code", "id"]);
    if request_id.is_empty() {
        return missing_param_response(id, "request_id");
    }
    let op = string_param(&params, &["op", "decision", "action"]);
    let op = if op.is_empty() {
        "approve".to_string()
    } else {
        op
    };
    let body_text = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    let (status, body) =
        crate::web_gateway::peers_pairing_request_decision(cert_dir, &request_id, &op, &body_text);
    http_body_response(id, status, body, "peer access request decision")
}

pub(crate) async fn api_peer_pairing_identities_response(id: String) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_identities_response_from_cert_dir(id, &cert_dir).await
}

pub(crate) async fn api_peer_pairing_identities_response_from_cert_dir(
    id: String,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let (status, body) = crate::web_gateway::peers_pairing_identities_list_from_cert_dir(cert_dir);
    http_body_response(id, status, body, "peer identities")
}

pub(crate) async fn api_peer_pairing_identity_revoke_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    api_peer_pairing_identity_revoke_response_from_cert_dir(id, params, &cert_dir).await
}

pub(crate) async fn api_peer_pairing_identity_revoke_response_from_cert_dir(
    id: String,
    params: Option<&serde_json::Value>,
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (status, body) =
        crate::web_gateway::peers_pairing_identity_revoke_from_cert_dir(cert_dir, &body_text);
    http_body_response(id, status, body, "peer identity revoke")
}

pub(crate) async fn api_coordinator_route_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let Some(registry) = runtime.peer_registry.as_ref() else {
        return peer_registry_unavailable_response(id);
    };
    let body_text = params_body_text(params);
    // The datachannel twin runs the S7 neutral core (POST-shaped by
    // construction); the registry check above keeps the tunnel's
    // historical frame-level error instead of the core's 503 body
    // (divergence #20 in the S7 parity enumeration).
    frame_api_response(
        id,
        crate::web_gateway::coordinator_route_api_response("POST", &body_text, Some(registry))
            .await,
        "coordinator route",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── S4b tunnel/HTTP parity: worktrees (design §8) ──
    //
    // Extends the S4a enumeration (api_sessions.rs). The S4b-specific
    // envelope differences, deliberate and pinned across this slice's
    // fixtures (worktrees here; delete/report in api_sessions.rs;
    // recordings in api_media.rs):
    //
    //  1. Body-only envelopes again: api_session_delete, api_worktrees
    //     (list), and api_worktrees_scan predate the injected-status
    //     envelope — plain `{t,id,ok:true,result}` frames; worktrees
    //     inspect/remove and api_session_recordings ride the
    //     injected-status envelope, whose keys only decorate OBJECT
    //     bodies (the recordings array passes through untouched).
    //  2. BYTES lane: the report zip and the recording listing assets
    //     serve identical bytes on both lanes; HTTP renders meta as its
    //     attachment/no-cache header tails, the tunnel emits the meta
    //     object verbatim as byte_stream_end.result. The delete tail
    //     orders the wildcard CORS header first (HTTP-lane decoration
    //     only).
    //  3. Transport-owned asset carriage: segment/frame FILES stream
    //     ranged and capped on the tunnel (offset/length params,
    //     UPLOAD_MAX_BYTES cap, 413/416 shapes) but serve as one
    //     unbounded body on HTTP; tunnel asset validators are stricter
    //     (trim, backslash) than the HTTP leaves' historical inline
    //     checks, and each lane keeps its historical error wording
    //     (json `{"ok":false,…}` vs text/plain).
    //  4. Transport-owned params: the tunnel's report id defaults to
    //     "current" and delete's target to "session"; HTTP addresses
    //     both by path segment (five delete wire shapes).
    //  5. Task-failure shapes stay per-lane (scan answers 200 with an
    //     error body on HTTP, an ok:false frame on the tunnel).

    fn parity_http_status_and_body(
        response: crate::web_gateway::ApiResponse,
    ) -> (u16, serde_json::Value) {
        let bytes = crate::web_gateway::api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        );
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header/body split");
        let head = String::from_utf8(bytes[..split].to_vec()).expect("ascii head");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("HTTP/1.1 "))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|code| code.parse::<u16>().ok())
            .expect("status line");
        (
            status,
            serde_json::from_slice(&bytes[split + 4..]).expect("json body"),
        )
    }

    #[tokio::test]
    async fn parity_worktrees_list_serves_the_same_body_on_both_transports() {
        let rt = crate::dashboard_control::tests::runtime();
        {
            let mut guard = rt.worktree_inventory_cache.lock().unwrap();
            *guard = Some(r#"{"worktrees":[],"cached":true}"#.to_string());
        }
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::worktrees_list_api_response(&rt.worktree_inventory_cache),
        );
        assert_eq!(status, 200);
        let frame = api_worktrees_response("parity-wt-list".to_string(), &rt).await;
        assert_eq!(frame["ok"], true);
        // Body-only envelope: no injected status metadata.
        assert!(frame["result"]
            .as_object()
            .is_none_or(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_worktrees_inspect_shares_bodies_with_status_metadata() {
        let rt = crate::dashboard_control::tests::runtime();
        // Invalid request body: deterministic serde wording on both lanes.
        // The injected temp home is never reached on this path, but the
        // fixture still must not hand the core a real home dir.
        let tmp_home = tempfile::tempdir().expect("temp home");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::worktrees_inspect_api_response(tmp_home.path(), "not json"),
        );
        assert_eq!(status, 400);
        let frame = api_worktrees_inspect_response(
            "parity-wt-inspect".to_string(),
            Some(&serde_json::json!("not json")),
            &rt,
        )
        .await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        // The tunnel serializes its params value as the request body —
        // "not json" arrives quoted, so the serde wording differs in the
        // offending token but both are the invalid-request shape.
        assert_eq!(result["ok"], http_body["ok"]);
        assert!(result["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid worktree inspect request:"));
        assert!(http_body["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid worktree inspect request:"));
    }

    #[tokio::test]
    async fn parity_worktrees_scan_rides_the_shared_core_and_plain_envelope() {
        // Both lanes render the ONE neutral scan (worktrees_scan_api_response)
        // over an injected temp home. A fixture must never run the real
        // scan: on a fleet runner that walks the runner account's
        // ~/projects and agent-worktree roots and reads its session
        // metadata — machine-dependent, git-subprocess-per-worktree slow,
        // and a hermeticity violation. The temp home is also what makes
        // the cross-lane equality assertable at all (live worktree state
        // moves between scans on a multi-agent box; only `scanned_at`
        // varies here).
        let tmp_home = tempfile::tempdir().expect("temp home");
        let strip_scanned_at = |mut body: serde_json::Value| {
            body.as_object_mut()
                .expect("inventory object")
                .remove("scanned_at")
                .expect("scan stamps scanned_at");
            body
        };

        let http_cache = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::worktrees_scan_api_response(tmp_home.path(), None, &http_cache),
        );
        assert_eq!(status, 200);

        let tunnel_cache = std::sync::Arc::new(std::sync::Mutex::new(None));
        let frame = frame_api_json_body_response(
            "parity-wt-scan".to_string(),
            crate::web_gateway::worktrees_scan_api_response(tmp_home.path(), None, &tunnel_cache),
            "worktree scan",
        );
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true);
        // Body-only envelope: no injected status metadata.
        let result = frame["result"].as_object().expect("inventory object");
        assert!(!result.contains_key("_httpStatus"), "{frame}");
        assert!(result.contains_key("worktrees"), "{frame}");
        assert_eq!(
            strip_scanned_at(frame["result"].clone()),
            strip_scanned_at(http_body.clone()),
            "the two lanes serve the same scan body"
        );
        // The shared cache side-effect holds each lane's served body,
        // byte-exact (same scan produced both).
        for (cache, served) in [(&http_cache, &http_body), (&tunnel_cache, &frame["result"])] {
            let cached = cache
                .lock()
                .unwrap()
                .clone()
                .expect("scan must warm the shared cache");
            let cached: serde_json::Value = serde_json::from_str(&cached).unwrap();
            assert_eq!(&cached, served, "the cache holds the served body");
        }
    }

    // ── S4c parity: managed context (the S4c envelope differences are
    // enumerated on the current-session fixture set in api_sessions.rs;
    // the tunnel's query synthesis is difference #5, the injected-status
    // envelope difference #1) ──

    #[tokio::test]
    async fn parity_managed_context_shares_bodies_with_status_metadata() {
        // An injected temp home keeps the home-scoped candidate scan
        // empty and deterministic — a fixture must never walk the
        // machine's real ~/.intendant/logs; both lanes then serve the
        // scoped empty bodies from the ONE neutral fn per kind.
        let rt = runtime();
        let session_id = "parity-mc-session";
        let tmp_home = tempfile::tempdir().expect("temp home");
        let home = tmp_home.path().to_path_buf();
        for (kind, empty_key) in [
            ("anchors", "anchors"),
            ("records", "records"),
            ("fission", "groups"),
        ] {
            let request_line =
                format!("GET /api/managed-context/{kind}?session_id={session_id} HTTP/1.1");
            let response = match kind {
                "anchors" => crate::web_gateway::managed_context_anchors_response_from_home(
                    &request_line,
                    None,
                    &home,
                ),
                "records" => crate::web_gateway::managed_context_records_response_from_home(
                    &request_line,
                    None,
                    &home,
                ),
                _ => crate::web_gateway::managed_context_fission_response_from_home(
                    &request_line,
                    None,
                    &home,
                ),
            };
            let (status, http_body) = parity_http_status_and_body(response);
            assert_eq!(status, 200, "{kind}");
            assert_eq!(http_body[empty_key], serde_json::json!([]), "{kind}");

            let frame = api_managed_context_response_from_home(
                format!("parity-mc-{kind}"),
                kind,
                Some(&serde_json::json!({ "session_id": session_id })),
                &rt,
                &home,
            )
            .await;
            let mut result = frame["result"].clone();
            let map = result.as_object_mut().expect("result object");
            assert_eq!(
                map.remove("_httpStatus"),
                Some(serde_json::json!(200)),
                "{kind}: {frame}"
            );
            assert_eq!(
                map.remove("_httpOk"),
                Some(serde_json::json!(true)),
                "{kind}"
            );
            assert_eq!(result, http_body, "{kind}");
        }

        // The tunnel-only missing-selector shape (difference #5): no
        // session selector at all cannot be synthesized into a request
        // line, so the tunnel answers its own decode error.
        let frame =
            api_managed_context_response("parity-mc-missing".to_string(), "records", None, &rt)
                .await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "missing query", "{frame}");
    }

    // ── S5 tunnel/HTTP parity: settings/keys family (design §8) ──
    //
    // Extends the S4a (api_sessions.rs), S4b (above), and S4c
    // enumerations. The S5-specific envelope differences, deliberate
    // and pinned across this set:
    //
    //  1. Envelope split: api_settings, api_key_status, and
    //     api_project_root ride the body-only envelope
    //     (frame_api_json_body_response — no injected status), so the
    //     settings GET's historical always-200 error body
    //     ({"error":"No project root"}) arrives under ok:true with no
    //     status metadata; api_settings_save and api_api_keys_save
    //     ride the injected-status envelope (frame_api_response).
    //  2. Transport-owned request carriage: HTTP hands the raw request
    //     body to the shared cores; the tunnel serializes its params
    //     object (params_body_text) — the same JSON object fed to both
    //     lanes reaches the one parse as the same bytes, but absent
    //     tunnel params arrive as "{}", never as an empty body.
    //  3. api_api_keys_save answers 200 on both lanes with failures in
    //     the body (historical always-200 lane) — tunnel failure
    //     bodies therefore still carry _httpStatus 200/_httpOk true.
    //  4. Header tails are HTTP-lane decoration only: settings
    //     GET/POST, key-status, and project-root the canonical tail;
    //     api-keys the bare wildcard tail (no Cache-Control).
    //  5. The api-keys env path resolves at each transport edge
    //     (api_keys_env_path); the persist core is path-parameterized.
    //     These fixtures drive only its pre-persist rejection paths —
    //     the hermetic success pin (tempdir env path, empty keys map)
    //     lives with the core's unit tests in web_gateway/settings.rs.

    /// The minimal valid settings payload (exactly the fields without
    /// a serde default), as the Settings tab has always POSTed it.
    fn parity_settings_payload() -> serde_json::Value {
        serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": null
        })
    }

    #[tokio::test]
    async fn parity_settings_get_serves_the_same_body_on_both_transports() {
        // Rootless: the historical always-200 error body, identical
        // across lanes, body-only envelope on the tunnel.
        let rt = runtime();
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::settings_get_api_response(
                None,
                &crate::web_gateway::RuntimeSettingsState::default(),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(http_body, serde_json::json!({"error": "No project root"}));
        let frame = api_settings_response("parity-settings-get".to_string(), &rt).await;
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_none_or(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);

        // Tempdir root: both lanes serve the same default-config payload.
        let dir = tempfile::tempdir().expect("temp project root");
        let mut rt = runtime();
        rt.project_root = Some(dir.path().to_path_buf());
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::settings_get_api_response(
                Some(dir.path()),
                &crate::web_gateway::RuntimeSettingsState::default(),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert!(http_body.get("cu_backend").is_some(), "{http_body}");
        let frame = api_settings_response("parity-settings-get-root".to_string(), &rt).await;
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_settings_save_shares_bodies_with_status_metadata() {
        // No project root: 400 body from the one core; the tunnel adds
        // the injected-status metadata.
        let rt = runtime();
        let payload = parity_settings_payload();
        let body_text = params_body_text(Some(&payload));
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::settings_post_api_response(&body_text, None, &rt.bus),
        );
        assert_eq!(status, 400);
        let frame =
            api_settings_save_response("parity-settings-save".to_string(), Some(&payload), &rt)
                .await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);

        // Tempdir roots: the success body rides both lanes and each
        // lane's save lands in its own injected root.
        let http_dir = tempfile::tempdir().expect("temp http root");
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::settings_post_api_response(
                &body_text,
                Some(http_dir.path()),
                &rt.bus,
            ));
        assert_eq!(status, 200);
        assert_eq!(http_body, serde_json::json!({"ok": true}));
        assert!(http_dir.path().join("intendant.toml").exists());

        let tunnel_dir = tempfile::tempdir().expect("temp tunnel root");
        let mut rt = runtime();
        rt.project_root = Some(tunnel_dir.path().to_path_buf());
        let frame = api_settings_save_response(
            "parity-settings-save-root".to_string(),
            Some(&payload),
            &rt,
        )
        .await;
        assert_eq!(frame["result"]["ok"], true, "{frame}");
        assert_eq!(frame["result"]["_httpStatus"], 200);
        assert!(tunnel_dir.path().join("intendant.toml").exists());
    }

    #[tokio::test]
    async fn parity_api_keys_save_rejections_share_bodies_always_200() {
        // Unknown key: rejected before any env-path use on both lanes —
        // and still 200 (difference #3), so the tunnel's failure body
        // carries _httpOk true.
        let payload = serde_json::json!({"keys": {"NOT_A_KNOWN_KEY": "x"}});
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::api_keys_save_api_response(None, &params_body_text(Some(&payload))),
        );
        assert_eq!(status, 200);
        assert_eq!(
            http_body,
            serde_json::json!({"error": "Unknown key: NOT_A_KNOWN_KEY"})
        );
        let frame = api_api_keys_save_response("parity-api-keys".to_string(), Some(&payload)).await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(200)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(true)));
        assert_eq!(result, http_body);
    }

    #[tokio::test]
    async fn parity_key_status_and_project_root_ride_the_plain_envelope() {
        // Key status: both lanes render the ONE neutral fn; the tunnel
        // frames it body-only.
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::api_key_status_api_response());
        assert_eq!(status, 200);
        let frame = frame_api_json_body_response(
            "parity-key-status".to_string(),
            crate::web_gateway::api_key_status_api_response(),
            "api key status",
        );
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_some_and(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);

        // Project root: Some and None bodies, identical across lanes.
        let dir = tempfile::tempdir().expect("temp project root");
        for root in [None, Some(dir.path())] {
            let (status, http_body) =
                parity_http_status_and_body(crate::web_gateway::project_root_api_response(root));
            assert_eq!(status, 200);
            let frame = frame_api_json_body_response(
                "parity-project-root".to_string(),
                crate::web_gateway::project_root_api_response(root),
                "project root",
            );
            assert_eq!(frame["result"], http_body);
            assert_eq!(frame["result"]["project_root"].is_null(), root.is_none());
        }
    }

    // ── S5 second slice: info/displays/diagnostics (design §8) ──
    //
    // Extends the S5 enumeration above:
    //
    //  6. api_displays and api_external_agents ride the body-only
    //     envelope; api_diagnostics_visual_freshness the
    //     injected-status envelope — all over one neutral core each.
    //  7. Transport-owned diagnostics decode: the tunnel pre-rejects a
    //     missing/empty session_id and a missing body (missing-param
    //     frames) and runs the sink on a blocking task whose failure
    //     is its own 500 shape; HTTP hands an empty session id to the
    //     sanitizer (400 "sanitizes to empty") and accepts an empty
    //     body as a zero-byte append (200 written:0).
    //  8. Header tails stay HTTP-lane decoration: displays the
    //     wildcard-CORS-with-Cache-Control tail, the sink the bare
    //     wildcard tail, external-agents the canonical tail.
    //  9. The sink's state dir and external-agents' home resolve at
    //     each transport edge; the cores are path-parameterized.

    #[tokio::test]
    async fn parity_displays_serves_the_same_body_on_both_transports() {
        // Injected display set on both lanes (a fixture must never
        // enumerate the machine's real displays — on a session-less CI
        // account the macOS enumeration never completes); no session
        // registry on either lane, so both render the ONE neutral body
        // through the `_from` core the production edges delegate to.
        let displays = vec![crate::display::DisplayInfo {
            id: 1,
            platform_id: 7,
            name: "Fixture Display".to_string(),
            width: 1280,
            height: 720,
            is_primary: true,
            kind: crate::display::DisplayInfoKind::Display,
            application_name: None,
            window_title: None,
        }];
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::displays_api_response_from(displays.clone(), &None).await,
        );
        assert_eq!(status, 200);
        assert!(http_body["displays"].is_array(), "{http_body}");
        let frame = frame_api_json_body_response(
            "parity-displays".to_string(),
            crate::web_gateway::displays_api_response_from(displays, &None).await,
            "displays",
        );
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_some_and(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_external_agents_shares_the_availability_body() {
        // Injected temp home on both lanes (a fixture must never scan
        // the live account's last-run state); the tunnel frames the
        // one neutral fn body-only.
        let home = tempfile::tempdir().expect("temp home");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::external_agents_api_response(None, home.path()),
        );
        assert_eq!(status, 200);
        assert!(http_body["external_agents"].is_array(), "{http_body}");
        let frame = frame_api_json_body_response(
            "parity-external-agents".to_string(),
            crate::web_gateway::external_agents_api_response(None, home.path()),
            "external agents",
        );
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_some_and(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_diagnostics_sink_shares_bodies_with_status_metadata() {
        let state_dir = tempfile::tempdir().expect("temp state dir");
        let ndjson = "{\"t\":\"session_start\"}\n";

        // Success: same written count from the one sink core; the
        // tunnel adds the injected-status metadata.
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::diagnostics_visual_freshness_api_response(
                state_dir.path(),
                "vf-parity-http",
                ndjson.as_bytes(),
            ),
        );
        assert_eq!(status, 200);
        assert_eq!(
            http_body,
            serde_json::json!({"ok": true, "written": ndjson.len()})
        );
        let frame = api_diagnostics_visual_freshness_response(
            "parity-vf".to_string(),
            Some(&serde_json::json!({
                "session_id": "vf-parity-tunnel",
                "body": ndjson,
            })),
            state_dir.path().to_path_buf(),
        )
        .await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(200)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(true)));
        assert_eq!(result, http_body);

        // Unusable (but present) session id: the shared sanitizer 400,
        // identical bodies (difference #7 — the empty id never reaches
        // the tunnel core; this one does on both lanes).
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::diagnostics_visual_freshness_api_response(
                state_dir.path(),
                "///",
                ndjson.as_bytes(),
            ),
        );
        assert_eq!(status, 400);
        let frame = api_diagnostics_visual_freshness_response(
            "parity-vf-bad".to_string(),
            Some(&serde_json::json!({ "session_id": "///", "body": ndjson })),
            state_dir.path().to_path_buf(),
        )
        .await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);

        // Tunnel-only pre-rejections (difference #7): missing params
        // never reach the sink core.
        let frame = api_diagnostics_visual_freshness_response(
            "parity-vf-missing".to_string(),
            None,
            state_dir.path().to_path_buf(),
        )
        .await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "missing session_id");
        let frame = api_diagnostics_visual_freshness_response(
            "parity-vf-nobody".to_string(),
            Some(&serde_json::json!({ "session_id": "vf-parity-tunnel" })),
            state_dir.path().to_path_buf(),
        )
        .await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "missing body");
    }

    // ── S6 tunnel/HTTP parity: access inspect/connect/tier family ──
    //
    // Extends the S4/S5 enumerations. The S6-specific envelope
    // differences, deliberate and pinned across this set (the org and
    // grant-mutation slices extend it):
    //
    //  10. The ok/error envelope: this family predates `_httpStatus` —
    //      2xx bodies ride the body-only `{t,id,ok:true,result}` frame,
    //      and error statuses surface the shared cores' `{"error": …}`
    //      body as the frame-level `{ok:false, error}` shape
    //      (frame_api_ok_error_response). No status metadata on either
    //      shape.
    //  11. Fleet-CORS decoration is HTTP-lane-only: the fleet-allowlist
    //      origin echo + `Vary: Origin` — and the historical
    //      undecorated shapes underneath it (the belt-and-suspenders
    //      manage-recheck 403, the tier pair's parse-error 400) — never
    //      appear on the tunnel (the golden transcripts in
    //      routes_access.rs pin the HTTP side).
    //  12. The manage re-check is an HTTP-edge belt: the tunnel's
    //      equivalent gate is the pre-dispatch method authorizer (the
    //      row-derived operation), whose denial is the authorizer's own
    //      `{ok:false, error:"…is not allowed…"}` frame, not a 403
    //      body.
    //  13. Transport-owned request carriage: the tunnel's params object
    //      is the canonical shape (§2.1) and absent params read as
    //      `{}`; HTTP parses its body text at the edge with the
    //      historical wordings ("invalid request body: …"), so
    //      parse-failure shapes are HTTP-only.
    //  14. Store resolution at the edges: both lanes resolve the
    //      ambient cert dir / project root at their dispatch arms and
    //      hand paths to the shared cores (hermeticity convention) —
    //      these fixtures inject tempdirs and never touch the live
    //      account's stores.

    #[tokio::test]
    async fn parity_dashboard_targets_rides_the_ok_error_envelope() {
        // Both lanes render the ONE neutral fn over the same inputs (an
        // empty agent card, no registry — the deterministic local-only
        // list).
        let card = serde_json::json!({});
        let principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("parity-test", "local");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::dashboard_targets_api_response(&card, None, None, Some(&principal)),
        );
        assert_eq!(status, 200);
        let frame = frame_api_ok_error_response(
            "parity-targets".to_string(),
            crate::web_gateway::dashboard_targets_api_response(&card, None, None, Some(&principal)),
            "dashboard targets",
        );
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_some_and(|map| !map.contains_key("_httpStatus")));
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_access_inspect_reads_share_bodies_over_an_injected_cert_dir() {
        // iam/state and enrollment-requests: deterministic empty-store
        // bodies from the one core each, body-only envelope on the
        // tunnel.
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_iam_state_api_response(tmp.path()),
        );
        assert_eq!(status, 200);
        let frame = frame_api_ok_error_response(
            "parity-iam-state".to_string(),
            crate::web_gateway::access_iam_state_api_response(tmp.path()),
            "access iam state",
        );
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"], http_body);
        assert_eq!(frame["result"]["schema_version"], 1);

        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_enrollment_requests_api_response(tmp.path()),
        );
        assert_eq!(status, 200);
        let frame = frame_api_ok_error_response(
            "parity-enrollment".to_string(),
            crate::web_gateway::access_enrollment_requests_api_response(tmp.path()),
            "access enrollment requests",
        );
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"], http_body);

        // Overview: same principal, same card, same store on both lanes.
        let card = serde_json::json!({});
        let principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("parity", "https");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_overview_api_response(tmp.path(), &card, None, &principal),
        );
        assert_eq!(status, 200);
        let frame = frame_api_ok_error_response(
            "parity-overview".to_string(),
            crate::web_gateway::access_overview_api_response(tmp.path(), &card, None, &principal),
            "access overview",
        );
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_access_tier_settings_shares_the_core_across_lanes() {
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("parity", "https");

        // Validation error: the shared core's wording surfaces as the
        // HTTP `{"error"}` body and the tunnel's frame-level error
        // (difference #10) — no store touched.
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_tier_settings_api_response(
                tmp.path(),
                serde_json::json!({"tier": 123}),
                &actor,
            ));
        assert_eq!(status, 400);
        assert_eq!(
            http_body,
            serde_json::json!({"error": "tier must be a string or null"})
        );
        let frame = frame_api_ok_error_response(
            "parity-tier-bad".to_string(),
            crate::web_gateway::access_tier_settings_api_response(
                tmp.path(),
                serde_json::json!({"tier": 123}),
                &actor,
            ),
            "trust tier settings",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "tier must be a string or null");

        // Success over the injected store: both lanes set the tier
        // through the one core (the `iam` overview metadata carries
        // store fingerprints, so equality is asserted on the mutation's
        // own fields).
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_tier_settings_api_response(
                tmp.path(),
                serde_json::json!({"tier": "disposable"}),
                &actor,
            ));
        assert_eq!(status, 200);
        assert_eq!(http_body["tier"], "disposable");
        let frame = frame_api_ok_error_response(
            "parity-tier".to_string(),
            crate::web_gateway::access_tier_settings_api_response(
                tmp.path(),
                serde_json::json!({"tier": "disposable"}),
                &actor,
            ),
            "trust tier settings",
        );
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"]["tier"], "disposable");
        assert_eq!(
            frame["result"]["schema_version"],
            http_body["schema_version"]
        );
    }

    #[tokio::test]
    async fn parity_connect_config_and_unclaim_deterministic_errors() {
        // Connect config, missing `enabled`: the shared validation
        // error before any store access, on both lanes.
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_connect_config_api_response(serde_json::json!({}), None),
        );
        assert_eq!(status, 400);
        assert_eq!(
            http_body,
            serde_json::json!({"error": "enabled must be true or false"})
        );
        let frame = frame_api_ok_error_response(
            "parity-connect-config".to_string(),
            crate::web_gateway::access_connect_config_api_response(serde_json::json!({}), None),
            "connect config",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "enabled must be true or false");

        // Unclaim over a tempdir project root: the deterministic
        // no-rendezvous error through the tunnel twin fn (the live
        // release path stays smoke-covered). The tempdir root keeps the
        // store off the daemon-scoped connect.toml.
        let dir = tempfile::tempdir().expect("temp project root");
        let mut rt = runtime();
        rt.project_root = Some(dir.path().to_path_buf());
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_connect_unclaim_api_response(Some(dir.path().to_path_buf()))
                .await,
        );
        assert_eq!(status, 400);
        assert_eq!(
            http_body,
            serde_json::json!({"error": "no rendezvous_url configured"})
        );
        let frame = api_access_connect_unclaim_response("parity-unclaim".to_string(), &rt).await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "no rendezvous_url configured");
    }

    // ── S6 tunnel/HTTP parity (second slice): IAM grants / enrollment
    // decide / org manage ──
    //
    // Extends the S6 enumeration above (#10–#14 all apply). The
    // slice-specific differences, deliberate and pinned:
    //
    //  15. Org-manage leaf addressing is transport-owned: HTTP maps the
    //      request path (issue is the historical default arm), the
    //      tunnel maps the method name — both land on the ONE
    //      OrgManageLeaf fan-out over the shared cores.
    //  16. The HTTP org-manage handler renders EVERY leaf through the
    //      fleet decorator (the own-origin leaves' inert `Vary: Origin`
    //      tail, golden-pinned in routes_access.rs) — pure HTTP-lane
    //      decoration; the tunnel's ok/error envelope carries none of
    //      it.

    #[tokio::test]
    async fn parity_iam_grant_mutations_share_cores_over_an_injected_cert_dir() {
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("parity", "https");

        // Upsert success: same body from the one core on both lanes
        // (fresh store per lane call is NOT possible — the second call
        // updates rather than creates — so the pin compares the
        // mutation's own fields).
        let upsert = || {
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Parity browser",
                "fingerprint": "PA:R1",
                "role_id": "role:observer"
            })
        };
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_iam_upsert_user_client_grant_api_response(
                tmp.path(),
                upsert(),
                &actor,
            ),
        );
        assert_eq!(status, 200);
        assert_eq!(http_body["created_grant"], serde_json::json!(true));
        let frame = frame_api_ok_error_response(
            "parity-iam-upsert".to_string(),
            crate::web_gateway::access_iam_upsert_user_client_grant_api_response(
                tmp.path(),
                upsert(),
                &actor,
            ),
            "iam grant upsert",
        );
        assert_eq!(frame["ok"], true);
        assert!(frame["result"]
            .as_object()
            .is_some_and(|map| !map.contains_key("_httpStatus")));
        // Second upsert of the same fingerprint updates in place.
        assert_eq!(frame["result"]["created_grant"], serde_json::json!(false));
        assert_eq!(
            frame["result"]["schema_version"],
            http_body["schema_version"]
        );

        // Update decode error: deterministic serde wording as the HTTP
        // {"error"} body and the tunnel frame-level error.
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_iam_update_grant_api_response(
                tmp.path(),
                serde_json::json!({}),
                &actor,
            ));
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-iam-update".to_string(),
            crate::web_gateway::access_iam_update_grant_api_response(
                tmp.path(),
                serde_json::json!({}),
                &actor,
            ),
            "iam grant update",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);

        // Enrollment decide, unknown fingerprint: deterministic error on
        // both lanes before any store access.
        let decide = || serde_json::json!({ "fingerprint": "PA:R1:EN", "approve": true });
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_enrollment_decide_api_response(tmp.path(), decide(), &actor),
        );
        assert_eq!(status, 400);
        assert_eq!(
            http_body,
            serde_json::json!({"error": "no pending enrollment for fingerprint PA:R1:EN"})
        );
        let frame = frame_api_ok_error_response(
            "parity-enroll-decide".to_string(),
            crate::web_gateway::access_enrollment_decide_api_response(tmp.path(), decide(), &actor),
            "enrollment decide",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(
            frame["error"],
            "no pending enrollment for fingerprint PA:R1:EN"
        );
    }

    #[tokio::test]
    async fn parity_org_manage_leaves_share_the_one_fan_out() {
        use crate::web_gateway::OrgManageLeaf;
        let tmp = tempfile::tempdir().expect("temp cert dir");

        // Leaf addressing agrees across transports (difference #15).
        for (method, path) in [
            ("api_access_org_trust", "/api/access/orgs/trust"),
            ("api_access_org_revoke", "/api/access/orgs/revoke"),
            ("api_access_org_issue", "/api/access/org-grants/issue"),
            (
                "api_access_org_revoke_member",
                "/api/access/org-grants/revoke-member",
            ),
            (
                "api_access_org_issuer_init",
                "/api/access/org-grants/issuers/init",
            ),
            (
                "api_access_org_issuer_delegate",
                "/api/access/org-grants/issuers/delegate",
            ),
            (
                "api_access_org_issuer_install",
                "/api/access/org-grants/issuers/install",
            ),
        ] {
            assert_eq!(
                OrgManageLeaf::from_control_method(method),
                Some(OrgManageLeaf::from_req_path(path)),
                "{method} vs {path}"
            );
        }
        assert_eq!(
            OrgManageLeaf::from_control_method("api_access_org_present"),
            None
        );

        // Issue without keys: the deterministic error body on both lanes
        // from the one fan-out (the signing successes need real org keys
        // and stay smoke-covered — validate-org-grants).
        let params = || serde_json::json!({ "handle": "parity-org" });
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_org_manage_api_response(
                tmp.path(),
                OrgManageLeaf::Issue,
                params(),
            ));
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-org-issue".to_string(),
            crate::web_gateway::access_org_manage_api_response(
                tmp.path(),
                OrgManageLeaf::Issue,
                params(),
            ),
            "org manage",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);

        // Issuer init: both lanes create/read the deputy key under the
        // injected store — the SAME key once created, so the bodies are
        // equal across the two calls.
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_org_manage_api_response(
                tmp.path(),
                OrgManageLeaf::IssuerInit,
                params(),
            ));
        assert_eq!(status, 200);
        let frame = frame_api_ok_error_response(
            "parity-org-issuer-init".to_string(),
            crate::web_gateway::access_org_manage_api_response(
                tmp.path(),
                OrgManageLeaf::IssuerInit,
                params(),
            ),
            "org manage",
        );
        assert_eq!(frame["ok"], true);
        assert_eq!(frame["result"], http_body);
    }

    // ── S6 tunnel/HTTP parity (third slice): the signed-org doorbell
    // quartet ──
    //
    // Extends the S6 enumeration (#10–#16 apply). The slice-specific
    // differences, deliberate and pinned:
    //
    //  17. Authority is the documented op-override pair (design §2.7):
    //      the HTTP rows are Public (the signed document/list IS the
    //      authorization, rate-limited and size-capped), while the
    //      tunnel methods gate on a bound session's operation —
    //      AccessInspect for present/orl/renew, PresenceRead for the
    //      orl-apply courier — declared as `op_override`s on the rows
    //      and pinned closed by
    //      `tunnel_op_overrides_are_a_closed_documented_enumeration`.
    //  18. ORL addressing is transport-owned: HTTP captures the org
    //      handle from the path, the tunnel reads `params.handle`.
    //  19. The ORL error is the historical 404 on HTTP; the tunnel
    //      frame carries the same error string with no status.

    #[tokio::test]
    async fn parity_org_doorbell_shares_cores_over_an_injected_cert_dir() {
        let tmp = tempfile::tempdir().expect("temp cert dir");

        // Present, undecodable document: same core error on both lanes
        // (the verify successes need real signed documents — smoke-
        // covered by validate-org-grants).
        let card = serde_json::json!({});
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_org_present_api_response(
                tmp.path(),
                serde_json::json!({}),
                &card,
            ));
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-org-present".to_string(),
            crate::web_gateway::access_org_present_api_response(
                tmp.path(),
                serde_json::json!({}),
                &card,
            ),
            "org doorbell",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);

        // ORL for an unheld org: 404 on HTTP, the same error string as
        // the tunnel's ok:false frame (differences #18/#19).
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_org_orl_api_response(tmp.path(), "parity-org"),
        );
        assert_eq!(status, 404);
        let frame = frame_api_ok_error_response(
            "parity-org-orl".to_string(),
            crate::web_gateway::access_org_orl_api_response(tmp.path(), "parity-org"),
            "org doorbell",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);

        // Apply + renew decode errors: one core wording per leaf.
        let (status, http_body) =
            parity_http_status_and_body(crate::web_gateway::access_org_orl_apply_api_response(
                tmp.path(),
                serde_json::json!({}),
            ));
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-org-orl-apply".to_string(),
            crate::web_gateway::access_org_orl_apply_api_response(
                tmp.path(),
                serde_json::json!({}),
            ),
            "org doorbell",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);

        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::access_org_renew_api_response(tmp.path(), serde_json::json!({})),
        );
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-org-renew".to_string(),
            crate::web_gateway::access_org_renew_api_response(tmp.path(), serde_json::json!({})),
            "org doorbell",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);
    }

    #[tokio::test]
    async fn parity_fleet_cert_request_shares_the_no_name_error() {
        // The S6 ROW-NEW: both lanes run the one neutral fn. The test
        // process holds no fleet name, so the deterministic error is
        // the shared shape (explicit addresses keep the fixture off the
        // NIC-enumeration default; the no-name path never spawns the
        // ACME flow).
        let params = || serde_json::json!({ "addresses": ["192.0.2.10"] });
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::fleet_cert_request_api_response(params()),
        );
        assert_eq!(status, 400);
        let frame = frame_api_ok_error_response(
            "parity-fleet-cert".to_string(),
            crate::web_gateway::fleet_cert_request_api_response(params()),
            "fleet cert request",
        );
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], http_body["error"]);
        assert_eq!(
            frame["error"],
            "this daemon has no fleet name — enable Connect against a \
             rendezvous with fleet DNS and let it register first"
        );
    }

    // ── S7 tunnel/HTTP parity: the peers/coordinator federation
    // family ──
    //
    // Extends the enumeration (#10–#19 in the S6 blocks above). Both
    // lanes have long shared the peers leaves (`peers_*` in
    // routes_peers.rs); S7 makes the HTTP side one neutral sub-router
    // unit and pins the addressing equivalence. The slice-specific
    // differences, deliberate and pinned:
    //
    //  20. Registry-gate shapes are per-lane: the HTTP sub-router and
    //      coordinator answer 503 {"error":"peer registry not
    //      configured"}; the tunnel arms pre-check and answer the
    //      frame-level {ok:false, error:"peer registry unavailable"}
    //      — a different string AND envelope, both historical.
    //  21. Addressing is transport-owned: HTTP captures the peer id
    //      (url-decoded) and the request code/decision from path
    //      segments and reads eligible capabilities from the query
    //      string; the tunnel reads params — peer_id under five
    //      aliases, request_id under four, the decision op defaulting
    //      to "approve", capabilities as array/CSV
    //      (control_capability_query) — and answers its own
    //      missing-param frame when the id is absent.
    //  22. Envelope split: api_peers (the list) rides the body-only
    //      envelope (inline dispatch arm — no injected status); every
    //      other peer method rides the `_httpStatus` injection. The
    //      family's wildcard-CORS tail and its reason ladder (502 Bad
    //      Gateway on relay failures) are HTTP-lane decoration only.
    //  23. Store resolution at the edges: both lanes resolve the
    //      ambient cert store at their transport edges and share the
    //      cert-dir-parameterized pairing leaves; fixtures inject
    //      tempdirs (hermeticity convention).

    fn empty_peer_registry() -> crate::peer::PeerRegistry {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(8);
        crate::peer::PeerRegistry::new(log_tx)
    }

    #[tokio::test]
    async fn parity_coordinator_route_shares_the_neutral_core() {
        // Decode failure: deterministic serde wording through the one
        // core on both lanes (routing successes need a connected peer
        // and stay smoke-covered by the peer validators).
        let registry = empty_peer_registry();
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::coordinator_route_api_response("POST", "{}", Some(&registry)).await,
        );
        assert_eq!(status, 400);

        let mut rt = runtime();
        rt.peer_registry = Some(empty_peer_registry());
        let frame = api_coordinator_route_response(
            "parity-coordinator".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);
    }

    #[tokio::test]
    async fn parity_registry_unavailable_shapes_stay_per_lane() {
        // Divergence #20, pinned from both sides.
        let bus = crate::event::EventBus::new();
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::peers_sub_router_api_response(
                "GET",
                "/api/peers",
                "",
                tmp.path(),
                &bus,
                None,
                None,
            )
            .await,
        );
        assert_eq!(status, 503);
        assert_eq!(http_body["error"], "peer registry not configured");

        let rt = runtime();
        let frame = api_peer_add_response("parity-no-registry".to_string(), None, &rt).await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "peer registry unavailable");
    }

    #[tokio::test]
    async fn parity_pairing_decision_addresses_by_params_on_the_tunnel() {
        // Divergence #21: HTTP takes the code and decision from path
        // segments, the tunnel from params; the unknown-decision 404
        // rises from the one shared leaf over the same injected store.
        let bus = crate::event::EventBus::new();
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::peers_sub_router_api_response(
                "POST",
                "/api/peers/pairing/requests/zzz/badop",
                "",
                tmp.path(),
                &bus,
                None,
                None,
            )
            .await,
        );
        assert_eq!(status, 404);

        let frame = api_peer_pairing_request_decision_response_from_cert_dir(
            "parity-decision".to_string(),
            Some(&serde_json::json!({"request_id": "zzz", "op": "badop"})),
            tmp.path(),
        )
        .await;
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(404)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);

        // The tunnel's own missing-param frame (no HTTP equivalent —
        // the path shape cannot omit the code).
        let frame = api_peer_pairing_request_decision_response_from_cert_dir(
            "parity-decision-missing".to_string(),
            Some(&serde_json::json!({})),
            tmp.path(),
        )
        .await;
        assert_eq!(frame["ok"], false);
        assert_eq!(frame["error"], "missing request_id");
    }

    #[tokio::test]
    async fn parity_pairing_requests_list_shares_the_leaf_store() {
        let bus = crate::event::EventBus::new();
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::peers_sub_router_api_response(
                "GET",
                "/api/peers/pairing/requests",
                "",
                tmp.path(),
                &bus,
                None,
                None,
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(http_body, serde_json::json!({"requests": []}));

        let frame = api_peer_pairing_requests_response_from_cert_dir(
            "parity-requests".to_string(),
            tmp.path(),
        )
        .await;
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(200)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(true)));
        assert_eq!(result, http_body);
    }

    #[tokio::test]
    async fn parity_eligible_addresses_by_params_on_the_tunnel() {
        // Divergence #21 on the read side: HTTP's ?capability= query vs
        // the tunnel's capabilities param (array/CSV), one leaf behind
        // both. The no-capability 400 is the deterministic shape.
        let bus = crate::event::EventBus::new();
        let tmp = tempfile::tempdir().expect("temp cert dir");
        let registry = empty_peer_registry();
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::peers_sub_router_api_response(
                "GET",
                "/api/peers/eligible",
                "",
                tmp.path(),
                &bus,
                None,
                Some(&registry),
            )
            .await,
        );
        assert_eq!(status, 400);

        let mut rt = runtime();
        rt.peer_registry = Some(empty_peer_registry());
        let frame = api_peer_eligible_response(
            "parity-eligible".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);

        // Same params, capability satisfied on both lanes: the empty
        // registry serves the empty snapshot list.
        let (status, http_body) = parity_http_status_and_body(
            crate::web_gateway::peers_sub_router_api_response(
                "GET",
                "/api/peers/eligible?capability=display",
                "",
                tmp.path(),
                &bus,
                None,
                Some(&registry),
            )
            .await,
        );
        assert_eq!(status, 200);
        let frame = api_peer_eligible_response(
            "parity-eligible-display".to_string(),
            Some(&serde_json::json!({"capabilities": ["display"]})),
            &rt,
        )
        .await;
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(200)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(true)));
        assert_eq!(result, http_body);
    }

    use crate::dashboard_control::tests::runtime;
    use crate::*;

    /// The SPA mirrors the action-message allowlist as
    /// `DASHBOARD_ACTION_MSG_RPC_ACTIONS` (static/app/31-init-identity-fleet.js)
    /// to pick the RPC lane before dispatching. That copy can't derive from
    /// this file, so pin its set to `DASHBOARD_ACTION_MSG_ACTIONS` — same
    /// pattern as the IAM catalog parity tests in `access::iam`.
    #[test]
    fn spa_action_msg_rpc_set_mirrors_dashboard_action_allowlist() {
        let app = include_str!("../../../../static/app.html");
        let start = "const DASHBOARD_ACTION_MSG_RPC_ACTIONS = new Set([";
        let from = app
            .find(start)
            .expect("DASHBOARD_ACTION_MSG_RPC_ACTIONS set not found in app.html")
            + start.len();
        let rest = &app[from..];
        let to = rest
            .find("]);")
            .expect("DASHBOARD_ACTION_MSG_RPC_ACTIONS set is unterminated");
        let js_set: std::collections::BTreeSet<&str> =
            rest[..to].split('\'').skip(1).step_by(2).collect();
        let rust_set: std::collections::BTreeSet<&str> =
            DASHBOARD_ACTION_MSG_ACTIONS.iter().copied().collect();
        assert_eq!(
            js_set, rust_set,
            "DASHBOARD_ACTION_MSG_RPC_ACTIONS (static/app/31-init-identity-fleet.js) \
             drifted from DASHBOARD_ACTION_MSG_ACTIONS"
        );
    }

    /// Every allowlisted action name must be a real `ControlMsg` wire name —
    /// a typo in the const would silently close the lane for that action.
    #[test]
    fn dashboard_action_allowlist_names_are_reachable() {
        for name in DASHBOARD_ACTION_MSG_ACTIONS {
            let probe = serde_json::json!({ "action": name });
            let parses = serde_json::from_value::<ControlMsg>(probe).is_ok();
            // Actions with required fields don't parse from the bare probe;
            // check those against the canonical name map via a sample.
            let known = parses
                || SAMPLE_ACTION_MSGS
                    .iter()
                    .any(|(sample_name, _)| sample_name == name);
            assert!(known, "allowlisted action {name:?} matches no ControlMsg");
        }
        for (name, sample) in SAMPLE_ACTION_MSGS {
            let msg: ControlMsg = serde_json::from_value(sample()).unwrap();
            assert_eq!(dashboard_control_msg_action(&msg), *name);
            assert!(
                dashboard_action_msg_allowed(&msg),
                "sample for {name:?} not admitted by the allowlist"
            );
        }
    }

    /// Samples for allowlisted actions whose variants have required fields.
    const SAMPLE_ACTION_MSGS: &[(&str, fn() -> serde_json::Value)] = &[
        (
            "codex_thread_action",
            || serde_json::json!({"action": "codex_thread_action", "session_id": "s", "op": "new", "params": {}}),
        ),
        (
            "take_display",
            || serde_json::json!({"action": "take_display", "display_id": 1}),
        ),
        (
            "resolve_display_request",
            || serde_json::json!({"action": "resolve_display_request", "session_id": "s", "id": 1, "decision": "approve", "duration": "this_session"}),
        ),
        (
            "release_display",
            || serde_json::json!({"action": "release_display", "display_id": 1}),
        ),
        (
            "create_virtual_display",
            || serde_json::json!({"action": "create_virtual_display", "width": 1280, "height": 800}),
        ),
        (
            "close_browser_workspace",
            || serde_json::json!({"action": "close_browser_workspace", "workspace_id": "w"}),
        ),
        (
            "acquire_browser_workspace",
            || serde_json::json!({"action": "acquire_browser_workspace", "workspace_id": "w", "holder_id": "h"}),
        ),
        (
            "release_browser_workspace",
            || serde_json::json!({"action": "release_browser_workspace", "workspace_id": "w", "holder_id": "h"}),
        ),
        (
            "start_recording",
            || serde_json::json!({"action": "start_recording", "stream_name": "s"}),
        ),
        (
            "stop_recording",
            || serde_json::json!({"action": "stop_recording", "stream_name": "s"}),
        ),
        (
            "delete_recording",
            || serde_json::json!({"action": "delete_recording", "stream_name": "s"}),
        ),
        (
            "set_diagnostics_visual_marker",
            || serde_json::json!({"action": "set_diagnostics_visual_marker", "enabled": true}),
        ),
    ];

    struct DashboardControlStubDisplayBackend;

    #[async_trait::async_trait]
    impl crate::display::DisplayBackend for DashboardControlStubDisplayBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::display::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }

        async fn stop_capture(&self) {}

        async fn inject_input(
            &self,
            _event: crate::display::InputEvent,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }

        fn kind(&self) -> &'static str {
            "dashboard-control-stub"
        }
    }

    #[tokio::test]
    async fn api_voice_session_preserves_endpoint_error_metadata() {
        let mut rt = runtime();
        rt.config = serde_json::json!({
            "provider": "unsupported-voice-provider",
            "model": "unused",
        });
        let response = api_voice_session_response("voice1".to_string(), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "voice1");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 502);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(
            response["result"]["error"],
            "Unknown provider: unsupported-voice-provider"
        );
    }

    #[tokio::test]
    async fn api_mcp_tool_call_reports_unavailable_server_as_http_error() {
        let rt = runtime();
        let response = api_mcp_tool_call_response(
            "mcp1".to_string(),
            Some(&serde_json::json!({
                "mcp_id": 7,
                "session_id": "session-1",
                "name": "get_status",
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "mcp1");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 503);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(response["result"]["id"], 7);
        assert_eq!(response["result"]["error"]["code"], -32603);
        assert_eq!(
            response["result"]["error"]["message"],
            "MCP server not available"
        );
    }

    #[tokio::test]
    async fn api_control_msg_dispatches_allowlisted_settings_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_control_msg_response(
            "ctrl1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "set_codex_sandbox");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::SetCodexSandbox { mode }) = event {
                assert_eq!(mode, "workspace-write");
                saw_control = true;
                break;
            }
        }
        assert!(saw_control, "allowed control message did not reach the bus");

        let rejected = api_control_msg_response(
            "ctrl2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "create_session",
                    "task": "do something",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected["t"], "response");
        assert_eq!(rejected["ok"], true);
        assert_eq!(rejected["result"]["ok"], false);
        assert_eq!(rejected["result"]["_httpStatus"], 400);
        assert!(rejected["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard WebRTC"));
    }

    #[tokio::test]
    async fn api_session_control_msg_dispatches_lifecycle_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_session_control_msg_response(
            "session-ctrl1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "interrupt",
                    "session_id": "session-a",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "interrupt");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::Interrupt { session_id, .. }) = event {
                assert_eq!(session_id.as_deref(), Some("session-a"));
                saw_control = true;
                break;
            }
        }
        assert!(saw_control, "session control message did not reach the bus");

        let accepted_create = api_session_control_msg_response(
            "session-ctrl2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "create_session",
                    "task": "noop",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(accepted_create["t"], "response");
        assert_eq!(accepted_create["ok"], true);
        assert_eq!(accepted_create["result"]["ok"], true);
        assert_eq!(accepted_create["result"]["action"], "create_session");

        let rejected_settings = api_session_control_msg_response(
            "session-ctrl3".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected_settings["t"], "response");
        assert_eq!(rejected_settings["ok"], true);
        assert_eq!(rejected_settings["result"]["ok"], false);
        assert_eq!(rejected_settings["result"]["_httpStatus"], 400);
        assert!(rejected_settings["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard session WebRTC"));
    }

    #[tokio::test]
    async fn api_dashboard_action_msg_dispatches_small_dashboard_actions_only() {
        let rt = runtime();
        let mut events = rt.bus.subscribe();
        let response = api_dashboard_action_msg_response(
            "dash-action1".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "close_browser_workspace",
                    "workspace_id": "workspace-a",
                    "reason": "test",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["action"], "close_browser_workspace");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::CloseBrowserWorkspace {
                workspace_id,
                ..
            }) = event
            {
                assert_eq!(workspace_id, "workspace-a");
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "dashboard action message did not reach the bus"
        );

        let accepted_thread = api_dashboard_action_msg_response(
            "dash-action2".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "codex_thread_action",
                    "session_id": "session-a",
                    "op": "new",
                    "params": {},
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(accepted_thread["t"], "response");
        assert_eq!(accepted_thread["ok"], true);
        assert_eq!(accepted_thread["result"]["action"], "codex_thread_action");

        let rejected_settings = api_dashboard_action_msg_response(
            "dash-action3".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_codex_sandbox",
                    "mode": "workspace-write",
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(rejected_settings["t"], "response");
        assert_eq!(rejected_settings["ok"], true);
        assert_eq!(rejected_settings["result"]["ok"], false);
        assert_eq!(rejected_settings["result"]["_httpStatus"], 400);
        assert!(rejected_settings["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available over dashboard action WebRTC"));
    }

    #[tokio::test]
    async fn api_diagnostics_visual_freshness_appends_ndjson_batch() {
        // Injected state dir: the append lands in the fixture's tempdir,
        // never the live diagnostics store (hermeticity convention; the
        // dispatch arm resolves the real dir in production).
        let state_dir = tempfile::tempdir().expect("temp state dir");
        let session_id = "dashboard-control-test-vf";
        let ndjson = "{\"t\":\"session_start\"}\n{\"t\":\"summary\"}\n";
        let response = api_diagnostics_visual_freshness_response(
            "diag-vf".to_string(),
            Some(&serde_json::json!({
                "session_id": session_id,
                "body": ndjson,
            })),
            state_dir.path().to_path_buf(),
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(response["result"]["_httpStatus"], 200);
        assert_eq!(response["result"]["written"], ndjson.len());

        let path = crate::diagnostics::visual_freshness_path_in(state_dir.path(), session_id)
            .expect("diagnostics path");
        let written = std::fs::read_to_string(&path).expect("diagnostics transcript");
        assert_eq!(written, ndjson);
    }

    #[tokio::test]
    async fn api_dashboard_action_msg_applies_diagnostics_visual_marker_to_display_registry() {
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let display_session = Arc::new(crate::display::DisplaySession::new(
            2,
            Arc::new(DashboardControlStubDisplayBackend),
        ));
        registry
            .write()
            .await
            .insert(2, Arc::clone(&display_session));
        {
            let mut session = rt.shared_session.write().await;
            session.session_registry = Some(Arc::clone(&registry));
        }

        let response = api_dashboard_action_msg_response(
            "dash-action-marker".to_string(),
            Some(&serde_json::json!({
                "message": {
                    "action": "set_diagnostics_visual_marker",
                    "display_id": 2,
                    "enabled": true,
                }
            })),
            &rt,
        )
        .await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["ok"], true);
        assert_eq!(
            response["result"]["action"],
            "set_diagnostics_visual_marker"
        );
        assert_eq!(response["result"]["display_id"], 2);
        assert_eq!(response["result"]["registry_available"], true);
        assert_eq!(response["result"]["active_display_updated"], true);
        assert!(
            display_session.diagnostics_visual_marker_enabled(),
            "dashboard-control RPC did not toggle the live display session"
        );
    }

    #[tokio::test]
    async fn peer_webrtc_signal_returns_http_error_metadata() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(8);
        let mut rt = runtime();
        rt.peer_registry = Some(crate::peer::PeerRegistry::new(log_tx));

        let params = serde_json::json!({
            "peer_id": "missing-peer",
            "display_id": 0,
            "session_id": "dashboard-test-session",
            "signal": { "kind": "close" },
        });
        let response =
            api_peer_webrtc_signal_response("webrtc1".to_string(), Some(&params), &rt).await;

        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["_httpOk"], false);
        assert_eq!(response["result"]["_httpStatus"], 404);
        assert_eq!(response["result"]["error"], "peer not found");
    }

    #[tokio::test]
    async fn state_snapshot_rpc_returns_bootstrap_message_shape() {
        let rt = runtime();
        let snapshot = api_state_snapshot_response("snap1".to_string(), &rt).await;
        assert_eq!(snapshot["t"], "response");
        assert_eq!(snapshot["id"], "snap1");
        assert_eq!(snapshot["ok"], true);
        assert_eq!(snapshot["result"]["t"], "state_snapshot");
        assert_eq!(snapshot["result"]["connection_id"], "session-1");
        assert_eq!(snapshot["result"]["config"]["provider"], "openai");
        assert_eq!(snapshot["result"]["session_id"], "");
        assert!(snapshot["result"]["state"].is_object());
    }

    #[tokio::test]
    async fn session_log_replay_rpc_returns_empty_replay_without_active_log() {
        let rt = runtime();
        let replay = api_session_log_replay_response("replay1".to_string(), &rt).await;
        assert_eq!(replay["t"], "response");
        assert_eq!(replay["id"], "replay1");
        assert_eq!(replay["ok"], true);
        assert_eq!(replay["result"]["t"], "log_replay");
        assert_eq!(replay["result"]["available"], false);
        assert_eq!(replay["result"]["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn display_bootstrap_rpc_returns_empty_frames_without_active_displays() {
        let rt = runtime();
        let bootstrap = api_display_bootstrap_response("disp1".to_string(), &rt).await;
        assert_eq!(bootstrap["t"], "response");
        assert_eq!(bootstrap["id"], "disp1");
        assert_eq!(bootstrap["ok"], true);
        assert_eq!(bootstrap["result"]["frame_count"], 0);
        assert_eq!(bootstrap["result"]["frames"].as_array().unwrap().len(), 0);
        assert!(bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_input_authority_state")));
    }

    #[tokio::test]
    async fn display_webrtc_signal_rpc_reports_missing_display() {
        let rt = runtime();
        let params = serde_json::json!({
            "signal": "offer",
            "display_id": 99,
            "sdp": "synthetic-offer",
        });
        let response =
            api_display_webrtc_signal_response("sig1".to_string(), Some(&params), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["id"], "sig1");
        assert_eq!(response["ok"], false);
        assert_eq!(response["status"], 404);
        assert_eq!(response["display_id"], 99);
        assert_eq!(response["error"], "display session not found");
    }

    #[tokio::test]
    async fn external_session_activity_replay_rpc_returns_empty_frames_without_attached_sessions() {
        let rt = runtime();
        let replay = api_external_session_activity_replay_response("ext1".to_string(), &rt).await;
        assert_eq!(replay["t"], "response");
        assert_eq!(replay["id"], "ext1");
        assert_eq!(replay["ok"], true);
        assert_eq!(replay["result"]["frame_count"], 0);
        assert_eq!(replay["result"]["frames"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dashboard_bootstrap_rpc_returns_ordered_bootstrap_frames() {
        let rt = runtime();
        let bootstrap = api_dashboard_bootstrap_response("boot1".to_string(), &rt).await;
        assert_eq!(bootstrap["t"], "response");
        assert_eq!(bootstrap["id"], "boot1");
        assert_eq!(bootstrap["ok"], true);
        let frames = bootstrap["result"]["frames"].as_array().unwrap();
        assert_eq!(bootstrap["result"]["frame_count"], frames.len());
        assert_eq!(frames[0]["t"], "state_snapshot");
        assert_eq!(frames[1]["t"], "browser_workspace_snapshot");
        assert_eq!(frames[2]["t"], "log_replay");
        assert!(!bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_ready")));
        assert!(bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("display_input_authority_state")));
        assert!(!bootstrap["result"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("external_session_activity_replay")));
    }

    #[tokio::test]
    async fn worktree_rpcs_preserve_cache_and_error_status() {
        let rt = runtime();
        {
            let mut cache = rt.worktree_inventory_cache.lock().unwrap();
            *cache = Some(
                serde_json::json!({
                    "worktrees": [{ "path": "/tmp/wt", "branch": "feature" }],
                    "summary": { "worktrees": 1 },
                })
                .to_string(),
            );
        }

        let cached = api_worktrees_response("wt1".to_string(), &rt).await;
        assert_eq!(cached["t"], "response");
        assert_eq!(cached["ok"], true);
        assert_eq!(cached["result"]["summary"]["worktrees"], 1);

        let invalid_remove =
            api_worktrees_remove_response("wt2".to_string(), Some(&serde_json::json!({})), &rt)
                .await;
        assert_eq!(invalid_remove["t"], "response");
        assert_eq!(invalid_remove["ok"], true);
        assert_eq!(invalid_remove["result"]["ok"], false);
        assert_eq!(invalid_remove["result"]["_httpStatus"], 400);
        assert_eq!(invalid_remove["result"]["_httpOk"], false);
    }
}
