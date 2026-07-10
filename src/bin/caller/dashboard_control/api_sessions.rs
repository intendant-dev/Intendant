//! Session-scoped control requests: request/stream task spawns, session
//! lists and streams, detail/report/history, current-session uploads,
//! rollback/redo/prune/changes, context snapshots, and session search.

use super::*;

pub(crate) fn spawn_control_request(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    runtime: ControlRuntime,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) {
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    let cancel = CancellationToken::new();
    pending_requests.insert(id.clone(), cancel.clone());
    tokio::spawn(async move {
        let response = match method.as_str() {
            "api_session_report" => {
                api_session_report_task_response(id.clone(), params.as_ref(), &runtime).await
            }
            "api_session_current_upload_raw" => {
                api_session_current_upload_raw_task_response(id.clone(), params.as_ref(), &runtime)
                    .await
            }
            "api_recording_asset" => {
                api_recording_asset_task_response(id.clone(), params.as_ref(), &runtime).await
            }
            "api_session_recording_asset" => {
                api_session_recording_asset_task_response(id.clone(), params.as_ref()).await
            }
            "api_session_frame_asset" => {
                api_session_frame_asset_task_response(id.clone(), params.as_ref()).await
            }
            "api_fs_read" => api_fs_read_task_response(id.clone(), params.as_ref()).await,
            "api_transfer_download_read" => {
                api_transfer_download_read_task_response(id.clone(), params.as_ref(), &runtime)
                    .await
            }
            _ => {
                let frame =
                    control_request_response(id.clone(), method, params, runtime, cancel).await;
                ControlTaskResponse {
                    id,
                    frame,
                    byte_stream: None,
                    done: true,
                }
            }
        };
        let _ = task_tx.send(response).await;
    });
}

pub(crate) fn spawn_control_stream(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
) {
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    let cancel = CancellationToken::new();
    pending_requests.insert(id.clone(), cancel.clone());
    tokio::spawn(async move {
        match method.as_str() {
            "api_sessions_stream" => {
                stream_sessions_response(id, params.as_ref(), task_tx, cancel).await;
            }
            _ => {
                let frame = serde_json::json!({
                    "t": "stream_end",
                    "id": id,
                    "ok": false,
                    "error": format!("unknown stream method: {method}"),
                });
                let _ = task_tx
                    .send(ControlTaskResponse {
                        id,
                        frame,
                        byte_stream: None,
                        done: true,
                    })
                    .await;
            }
        }
    });
}

/// Test the client-egress path end to end: force a tiny provider call
/// through the relay for `kind` — even when a local key or lease exists —
/// and return the model's reply. The fueling panel's "test relay" button
/// and the E2E validator's deterministic hook.
pub(crate) async fn api_credential_egress_probe_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let kind = params
        .and_then(|p| optional_string_param(p, &["kind"]))
        .unwrap_or_default();
    let model = params.and_then(|p| optional_string_param(p, &["model"]));
    let provider: Box<dyn crate::provider::ChatProvider> = match kind.as_str() {
        crate::credential_egress::KIND_ANTHROPIC => {
            Box::new(crate::provider::AnthropicProvider::new_client_egress(
                model.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string()),
                200_000,
                256,
            ))
        }
        crate::credential_egress::KIND_GEMINI => {
            Box::new(crate::provider::GeminiProvider::new_client_egress(
                model.unwrap_or_else(|| "gemini-2.5-flash".to_string()),
                200_000,
                256,
            ))
        }
        other => {
            return dashboard_control_error_response(
                id,
                format!(
                    "egress probe supports {} and {}, not {other:?}",
                    crate::credential_egress::KIND_ANTHROPIC,
                    crate::credential_egress::KIND_GEMINI
                ),
            )
        }
    };
    let probe_message = crate::conversation::Message {
        role: "user".to_string(),
        content: "Reply with the single word: pong".to_string(),
        ..Default::default()
    };
    match provider.chat(&[probe_message]).await {
        Ok(response) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": { "text": response.content, "model": provider.model() },
        }),
        Err(error) => dashboard_control_error_response(id, format!("egress probe failed: {error}")),
    }
}

pub(crate) async fn control_request_response(
    id: String,
    method: String,
    params: Option<serde_json::Value>,
    runtime: ControlRuntime,
    cancel: CancellationToken,
) -> serde_json::Value {
    if cancel.is_cancelled() {
        return cancelled_control_response(id, true);
    }
    match method.as_str() {
        "api_credential_egress_probe" => {
            api_credential_egress_probe_response(id, params.as_ref()).await
        }
        "api_access_connect_unclaim" => {
            match crate::web_gateway::access_connect_unclaim_response_value(
                runtime.project_root.clone(),
            )
            .await
            {
                Ok(result) => serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": result,
                }),
                Err(error) => serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": error,
                }),
            }
        }
        "api_sessions" => api_sessions_response(id, params.as_ref()).await,
        "api_session_detail" => api_session_detail_response(id, params.as_ref()).await,
        "api_session_delete" => api_session_delete_response(id, params.as_ref()).await,
        "api_session_agent_output" => api_session_agent_output_response(id, params.as_ref()).await,
        "api_session_current_agent_output" => {
            api_session_current_agent_output_response(id, params.as_ref(), &runtime).await
        }
        "api_session_current_history" => api_session_current_history_response(id, &runtime).await,
        "api_session_current_rollback" => {
            api_session_current_rollback_response(id, params.as_ref(), &runtime).await
        }
        "api_session_current_redo" => api_session_current_redo_response(id, &runtime).await,
        "api_session_current_prune" => api_session_current_prune_response(id, &runtime).await,
        "api_session_current_changes" => {
            api_session_current_changes_response(id, params.as_ref(), &runtime).await
        }
        "api_session_context_snapshot" => {
            api_session_context_snapshot_response(id, params.as_ref()).await
        }
        "api_session_current_uploads" => api_session_current_uploads_response(id, &runtime).await,
        "api_session_current_upload_delete" => {
            api_session_current_upload_delete_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_jobs" => api_transfer_jobs_response(id, &runtime).await,
        "api_transfer_job_create" => {
            api_transfer_job_create_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_job_delete" => {
            api_transfer_job_delete_response(id, params.as_ref(), &runtime).await
        }
        "api_transfer_upload_commit" => {
            api_transfer_upload_commit_response(id, params.as_ref(), &runtime).await
        }
        "api_media_clip_start" => {
            api_media_clip_start_response(id, params.as_ref(), &runtime).await
        }
        "api_media_clip_end" => api_media_clip_end_response(id, params.as_ref(), &runtime).await,
        "api_media_clip_cancel" => {
            api_media_clip_cancel_response(id, params.as_ref(), &runtime).await
        }
        "api_fs_stat" => api_fs_stat_response(id, params.as_ref()).await,
        "api_fs_list" => api_fs_list_response(id, params.as_ref()).await,
        "api_fs_mkdir" => api_fs_mkdir_response(id, params.as_ref()).await,
        "api_fs_rename" => api_fs_rename_response(id, params.as_ref()).await,
        "api_fs_delete" => api_fs_delete_response(id, params.as_ref()).await,
        "api_sessions_search" => api_sessions_search_response(id, params.as_ref(), cancel).await,
        "api_settings" => api_settings_response(id, &runtime).await,
        "api_settings_save" => api_settings_save_response(id, params.as_ref(), &runtime).await,
        "api_control_msg" => api_control_msg_response(id, params.as_ref(), &runtime).await,
        "api_session_control_msg" => {
            api_session_control_msg_response(id, params.as_ref(), &runtime).await
        }
        "api_dashboard_action_msg" => {
            api_dashboard_action_msg_response(id, params.as_ref(), &runtime).await
        }
        "api_diagnostics_visual_freshness" => {
            api_diagnostics_visual_freshness_response(id, params.as_ref()).await
        }
        "api_key_status" => json_body_response(
            id,
            crate::web_gateway::api_key_status_response_body(),
            "api key status",
        ),
        "api_external_agents" => json_body_response(
            id,
            crate::web_gateway::external_agents_response_body(runtime.project_root.as_deref()),
            "external agents",
        ),
        "api_api_keys_save" => api_api_keys_save_response(id, params.as_ref()).await,
        "api_voice_session" => api_voice_session_response(id, &runtime).await,
        "api_project_root" => json_body_response(
            id,
            crate::web_gateway::project_root_response_body(runtime.project_root.as_deref()),
            "project root",
        ),
        "api_displays" => api_displays_response(id, &runtime).await,
        "api_recordings" => api_recordings_response(id, &runtime).await,
        "api_session_recordings" => api_session_recordings_response(id, params.as_ref()).await,
        "api_browser_workspace_snapshot" => api_browser_workspace_snapshot_response(id).await,
        "api_state_snapshot" => api_state_snapshot_response(id, &runtime).await,
        "api_display_bootstrap" => api_display_bootstrap_response(id, &runtime).await,
        "api_display_webrtc_signal" => {
            api_display_webrtc_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_display_input_authority_snapshot" => {
            api_display_input_authority_snapshot_response(id, &runtime).await
        }
        "api_display_input_authority_request" => {
            api_display_input_authority_request_response(id, params.as_ref(), &runtime).await
        }
        "api_display_input_authority_release" => {
            api_display_input_authority_release_response(id, params.as_ref(), &runtime).await
        }
        "api_session_log_replay" => api_session_log_replay_response(id, &runtime).await,
        "api_external_session_activity_replay" => {
            api_external_session_activity_replay_response(id, &runtime).await
        }
        "api_dashboard_bootstrap" => api_dashboard_bootstrap_response(id, &runtime).await,
        "api_worktrees" => api_worktrees_response(id, &runtime).await,
        "api_worktrees_inspect" => {
            api_worktrees_inspect_response(id, params.as_ref(), &runtime).await
        }
        "api_worktrees_scan" => api_worktrees_scan_response(id, &runtime).await,
        "api_worktrees_remove" => {
            api_worktrees_remove_response(id, params.as_ref(), &runtime).await
        }
        "api_worktrees_merge" => api_worktrees_merge_response(id, params.as_ref(), &runtime).await,
        "api_managed_context_records" => {
            api_managed_context_response(id, "records", params.as_ref(), &runtime).await
        }
        "api_managed_context_anchors" => {
            api_managed_context_response(id, "anchors", params.as_ref(), &runtime).await
        }
        "api_managed_context_fission" => {
            api_managed_context_response(id, "fission", params.as_ref(), &runtime).await
        }
        "api_mcp_tool_call" => api_mcp_tool_call_response(id, params.as_ref(), &runtime).await,
        "api_peer_add" => api_peer_add_response(id, params.as_ref(), &runtime).await,
        "api_peer_remove" => api_peer_remove_response(id, params.as_ref(), &runtime).await,
        "api_peer_eligible" => api_peer_eligible_response(id, params.as_ref(), &runtime).await,
        "api_peer_message" => api_peer_message_response(id, params.as_ref(), &runtime).await,
        "api_peer_task" => api_peer_task_response(id, params.as_ref(), &runtime).await,
        "api_peer_approval" => api_peer_approval_response(id, params.as_ref(), &runtime).await,
        "api_peer_webrtc_signal" => {
            api_peer_webrtc_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_file_transfer_signal" => {
            api_peer_file_transfer_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_dashboard_control_signal" => {
            api_peer_dashboard_control_signal_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_pairing_invite" => api_peer_pairing_invite_response(id, params.as_ref()).await,
        "api_peer_pairing_join" => {
            api_peer_pairing_join_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_pairing_request_access" => {
            api_peer_pairing_request_access_response(id, params.as_ref()).await
        }
        "api_peer_pairing_request_access_poll" => {
            api_peer_pairing_request_access_poll_response(id, params.as_ref(), &runtime).await
        }
        "api_peer_pairing_requests" => api_peer_pairing_requests_response(id).await,
        "api_peer_pairing_request_decision" => {
            api_peer_pairing_request_decision_response(id, params.as_ref()).await
        }
        "api_peer_pairing_identities" => api_peer_pairing_identities_response(id).await,
        "api_peer_pairing_identity_revoke" => {
            api_peer_pairing_identity_revoke_response(id, params.as_ref()).await
        }
        "api_coordinator_route" => {
            api_coordinator_route_response(id, params.as_ref(), &runtime).await
        }
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("unknown method: {method}"),
        }),
    }
}

pub(crate) async fn stream_sessions_response(
    id: String,
    params: Option<&serde_json::Value>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    cancel: CancellationToken,
) {
    let request_line = sessions_stream_request_line(params);
    let (line_tx, line_rx) = mpsc::channel::<String>(64);
    let stream_task = tokio::task::spawn_blocking(move || {
        crate::web_gateway::stream_sessions_from_request(&request_line, line_tx);
    });
    stream_json_lines_response(
        id,
        "api_sessions_stream".to_string(),
        line_rx,
        stream_task,
        task_tx,
        cancel,
    )
    .await;
}

pub(crate) async fn stream_json_lines_response(
    id: String,
    method: String,
    mut line_rx: mpsc::Receiver<String>,
    stream_task: tokio::task::JoinHandle<()>,
    task_tx: mpsc::Sender<ControlTaskResponse>,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }

    if task_tx
        .send(ControlTaskResponse {
            id: id.clone(),
            frame: serde_json::json!({
                "t": "stream_start",
                "id": id,
                "method": method,
            }),
            byte_stream: None,
            done: false,
        })
        .await
        .is_err()
    {
        return;
    }

    let mut seq: u64 = 0;
    while let Some(line) = line_rx.recv().await {
        if cancel.is_cancelled() {
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(event) => event,
            Err(e) => {
                let frame = serde_json::json!({
                    "t": "stream_end",
                    "id": id,
                    "ok": false,
                    "error": format!("session stream returned invalid JSON: {e}"),
                });
                let _ = task_tx
                    .send(ControlTaskResponse {
                        id,
                        frame,
                        byte_stream: None,
                        done: true,
                    })
                    .await;
                return;
            }
        };
        let chunk_id = format!("{id}:{seq}");
        let frame = serde_json::json!({
            "t": "stream_event",
            "id": id,
            "seq": seq,
            "chunk_id": chunk_id,
            "event": event,
        });
        seq = seq.saturating_add(1);
        if task_tx
            .send(ControlTaskResponse {
                id: id.clone(),
                frame,
                byte_stream: None,
                done: false,
            })
            .await
            .is_err()
        {
            return;
        }
    }

    let frame = match stream_task.await {
        Ok(()) => serde_json::json!({
            "t": "stream_end",
            "id": id,
            "ok": true,
            "result": {
                "events": seq,
            },
        }),
        Err(e) => serde_json::json!({
            "t": "stream_end",
            "id": id,
            "ok": false,
            "error": format!("session stream task failed: {e}"),
        }),
    };
    if !cancel.is_cancelled() {
        let _ = task_tx
            .send(ControlTaskResponse {
                id,
                frame,
                byte_stream: None,
                done: true,
            })
            .await;
    }
}

pub(crate) fn sessions_stream_request_line(params: Option<&serde_json::Value>) -> String {
    let Some(params) = params else {
        return "GET /api/sessions/stream HTTP/1.1".to_string();
    };
    let Some(limit_value) = params.get("limit") else {
        return "GET /api/sessions/stream HTTP/1.1".to_string();
    };
    let limit = match limit_value {
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("full") {
                "all".to_string()
            } else {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| *limit > 0)
                    .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT)
                    .to_string()
            }
        }
        serde_json::Value::Number(value) => value
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT)
            .to_string(),
        _ => CONTROL_DEFAULT_SESSION_LIMIT.to_string(),
    };
    format!("GET /api/sessions/stream?limit={limit} HTTP/1.1")
}

pub(crate) fn cancelled_control_response(id: String, existed: bool) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "cancelled": true,
        "error": if existed {
            "request cancelled"
        } else {
            "request not found or already completed"
        },
    })
}

pub(crate) async fn api_session_detail_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "missing session_id",
        });
    }
    let source = string_param(&params, &["source"]).trim().to_string();
    let source = if source.is_empty() {
        "intendant".to_string()
    } else {
        source
    };
    // Transport-owned decode: the tunnel trims the id (the paged body
    // helper always did); HTTP passes the raw path segment.
    let session_id = session_id.trim().to_string();
    let limit = control_session_detail_limit(&params);
    let before = control_session_detail_before(&params);
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_detail_api_response(&session_id, &source, limit, before)
    })
    .await;
    match result {
        Ok(response) => frame_api_json_body_response(id, response, "session detail"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("session detail task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_session_report_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = optional_string_param(&params, &["session_id", "sessionId", "id"])
        .unwrap_or_else(|| "current".to_string());
    let (session_log, query_ctx) = {
        let session = runtime.shared_session.read().await;
        (session.session_log.clone(), session.query_ctx.clone())
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_report_zip_for_request(
            &session_id,
            session_log.as_ref(),
            query_ctx.as_ref(),
        )
    })
    .await;
    let report = match result {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => {
            let (status, error) = match err {
                crate::web_gateway::SessionReportZipError::InvalidSessionId => {
                    (400, "invalid session id".to_string())
                }
                crate::web_gateway::SessionReportZipError::NotFound => {
                    (404, "Session not found".to_string())
                }
                crate::web_gateway::SessionReportZipError::Build(error) => {
                    (500, format!("Failed to build report: {error}"))
                }
            };
            let frame = http_body_response(
                id.clone(),
                status,
                serde_json::json!({
                    "ok": false,
                    "error": error,
                })
                .to_string(),
                "session report",
            );
            return ControlTaskResponse {
                id,
                frame,
                byte_stream: None,
                done: true,
            };
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("session report task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    frame_api_task_response(
        id,
        crate::web_gateway::session_report_api_response(report),
        "session-report",
        "session report",
    )
}

pub(crate) async fn api_session_current_upload_raw_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let Some(upload_id) = optional_string_param(&params, &["id", "upload_id", "uploadId"]) else {
        return ControlTaskResponse {
            id: id.clone(),
            frame: http_body_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": "missing upload id" }).to_string(),
                "upload raw",
            ),
            byte_stream: None,
            done: true,
        };
    };
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(offset) => offset.unwrap_or(0),
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    400,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(length) => length,
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    400,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let scope = crate::global_store::StoreScope::resolve(runtime.project_root.as_deref());
    let session_log = {
        let session = runtime.shared_session.read().await;
        session.session_log.clone()
    };
    let session_dir_result = match session_log {
        Some(ref slog) => slog
            .lock()
            .map(|log| log.dir().to_path_buf())
            .map_err(|_| "session log lock poisoned".to_string()),
        None => Ok(crate::web_gateway::pending_upload_session_dir(&scope)),
    };
    let session_dir = match session_dir_result {
        Ok(session_dir) => session_dir,
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    500,
                    serde_json::json!({ "ok": false, "error": error }).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
    };
    let upload_id_for_stream = upload_id.clone();
    let read_result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_upload_raw_api_response(
            &upload_id,
            Some((offset, length)),
            &session_dir,
            &scope,
        )
    })
    .await;
    let response = match read_result {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: http_body_response(
                    id,
                    err.status(),
                    upload_raw_tunnel_error_body(&err).to_string(),
                    "upload raw",
                ),
                byte_stream: None,
                done: true,
            };
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("upload raw task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    frame_api_task_response(
        id,
        response,
        &format!("upload:{upload_id_for_stream}"),
        "upload raw",
    )
}

/// The tunnel's historical error bodies for the upload-raw content core:
/// `{"ok":false,"error":…}` objects, the 416 additionally carrying
/// `total_size` — versus HTTP's wildcard `{"error":…}` framing of the
/// same [`crate::web_gateway::CurrentUploadRawError`] (the enumerated
/// per-lane difference).
fn upload_raw_tunnel_error_body(
    err: &crate::web_gateway::CurrentUploadRawError,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "ok": false, "error": err.message() });
    if let crate::web_gateway::CurrentUploadRawError::RangeBeyondSize { total_size } = err {
        body["total_size"] = serde_json::json!(total_size);
    }
    body
}

pub(crate) async fn api_session_current_uploads_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (project_root, session_dir) = match active_upload_handles(runtime).await {
        Ok(handles) => handles,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "current uploads",
            );
        }
    };
    let scope = crate::global_store::StoreScope::resolve(project_root.as_deref());
    let session_dir =
        session_dir.unwrap_or_else(|| crate::web_gateway::pending_upload_session_dir(&scope));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_uploads_list_api_response(&session_dir, &scope)
    })
    .await;
    match result {
        // The injected-status envelope only decorates OBJECT bodies —
        // the uploads list array passes through untouched, as it always
        // did under its historical body-only framing.
        Ok(response) => frame_api_response(id, response, "current uploads"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("current uploads task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_session_current_upload_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let name = optional_string_param(&params, &["name", "filename", "file_name"])
        .unwrap_or_else(|| "upload.bin".to_string());
    let mime = optional_string_param(&params, &["mime", "content_type", "contentType"])
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let requested_destination = optional_string_param(&params, &["destination"])
        .as_deref()
        .and_then(crate::upload_store::UploadDestination::from_str)
        .unwrap_or(crate::upload_store::UploadDestination::Task);
    let (session_log, daemon_session_id) = {
        let session = runtime.shared_session.read().await;
        (
            session.session_log.clone(),
            Some(runtime.session_id.clone()),
        )
    };
    let project_root = runtime.project_root.clone();
    let bus = runtime.bus.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_upload_commit_api_response(
            project_root.as_deref(),
            session_log.as_ref(),
            daemon_session_id.as_deref(),
            &name,
            &mime,
            requested_destination,
            upload.tmp,
            upload.received_bytes,
            &bus,
        )
    })
    .await;
    let frame = match result {
        Ok(response) => frame_api_response(id.clone(), response, "current upload"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id.clone(),
            "ok": false,
            "error": format!("upload commit task failed: {e}"),
        }),
    };
    ControlTaskResponse {
        id,
        frame,
        byte_stream: None,
        done: true,
    }
}

pub(crate) async fn api_session_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    let target =
        optional_string_param(&params, &["target"]).unwrap_or_else(|| "session".to_string());
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_delete_api_response(&session_id, &target)
    })
    .await;
    match result {
        Ok(response) => frame_api_json_body_response(id, response, "session delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("session delete task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_session_current_agent_output_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    match active_session_log_dir(runtime).await {
        Ok(Some(log_dir)) => frame_api_response(
            id,
            crate::web_gateway::current_agent_output_api_response(&body_text, &log_dir),
            "agent output",
        ),
        Ok(None) => http_body_response(
            id,
            404,
            serde_json::json!({"error": "no active session log"}).to_string(),
            "agent output",
        ),
        Err(error) => http_body_response(
            id,
            500,
            serde_json::json!({"error": error}).to_string(),
            "agent output",
        ),
    }
}

pub(crate) async fn api_session_agent_output_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "missing session_id",
        });
    }
    let source = string_param(&params, &["source"]).trim().to_string();
    let source = if source.is_empty() {
        "intendant".to_string()
    } else {
        source
    };
    let body_text = params_body_text(Some(&params));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_agent_output_api_response(&body_text, &session_id, &source)
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "session agent output"),
        Err(e) => http_body_response(
            id,
            500,
            serde_json::json!({"error": format!("session output task failed: {e}")}).to_string(),
            "session agent output",
        ),
    }
}

pub(crate) async fn api_session_current_history_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, _) = active_history_handles(runtime).await;
    frame_api_response(
        id,
        crate::web_gateway::current_history_api_response(file_watcher.as_ref()).await,
        "session history",
    )
}

pub(crate) async fn api_session_current_rollback_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let body_text = params_body_text(params);
    let (file_watcher, agent_state) = active_history_handles(runtime).await;
    frame_api_response(
        id,
        crate::web_gateway::current_rollback_api_response(
            &body_text,
            file_watcher.as_ref(),
            agent_state.as_ref(),
            &runtime.bus,
        )
        .await,
        "session rollback",
    )
}

pub(crate) async fn api_session_current_redo_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, agent_state) = active_history_handles(runtime).await;
    frame_api_response(
        id,
        crate::web_gateway::current_redo_api_response(file_watcher.as_ref(), agent_state.as_ref())
            .await,
        "session redo",
    )
}

pub(crate) async fn api_session_current_prune_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let (file_watcher, _) = active_history_handles(runtime).await;
    frame_api_response(
        id,
        crate::web_gateway::current_prune_api_response(file_watcher.as_ref()).await,
        "session prune",
    )
}

pub(crate) async fn api_session_current_changes_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let request_line = changes_request_line(params);
    let (snapshot_dir, project_root) = active_changes_handles(runtime).await;
    let home = crate::platform::home_dir();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_current_changes_api_response(
            &request_line,
            snapshot_dir.as_deref(),
            project_root.as_deref(),
            &home,
        )
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "session changes"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("session changes task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_session_context_snapshot_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if session_id.is_empty() {
        return missing_param_response(id, "session_id");
    }
    let source = optional_string_param(&params, &["source"]).unwrap_or_else(|| "intendant".into());
    let file = optional_string_param(&params, &["file"]);
    let request_id = optional_string_param(&params, &["request_id", "requestId"]);
    let request_index = match optional_u64_param(&params, &["request_index", "requestIndex"]) {
        Ok(value) => value,
        Err(error) => {
            return http_body_response(
                id,
                400,
                serde_json::json!({ "error": error }).to_string(),
                "context snapshot",
            );
        }
    };
    let ts = optional_string_param(&params, &["ts"]);
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::session_context_snapshot_api_response(
            &session_id,
            &source,
            file,
            request_id,
            request_index,
            ts,
        )
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "context snapshot"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("context snapshot task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_session_current_upload_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let upload_id = string_param(&params, &["upload_id", "uploadId", "id"]);
    let (project_root, session_dir) = match active_upload_handles(runtime).await {
        Ok(handles) => handles,
        Err(error) => {
            return http_body_response(
                id,
                500,
                serde_json::json!({ "error": error }).to_string(),
                "upload delete",
            );
        }
    };
    let bus = runtime.bus.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::current_upload_delete_api_response(
            project_root.as_deref(),
            session_dir.as_deref(),
            &upload_id,
            &bus,
        )
    })
    .await;
    match result {
        Ok(response) => frame_api_response(id, response, "upload delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("upload delete task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_sessions_search_response(
    id: String,
    params: Option<&serde_json::Value>,
    cancel: CancellationToken,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let query = string_param(&params, &["q", "query"]);
    let source_filter = string_param(&params, &["source", "source_filter", "sourceFilter"]);
    let source_filter = if source_filter.is_empty() {
        "all".to_string()
    } else {
        source_filter
    };
    let mode = string_param(&params, &["mode"]);
    let project_filter = control_project_filter(&params);
    let response = crate::web_gateway::sessions_search_api_response(
        query,
        source_filter,
        mode,
        project_filter,
        cancel,
    )
    .await;
    frame_api_json_body_response(id, response, "session search")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_control::tests::runtime;

    #[tokio::test]
    async fn session_report_rpc_returns_zip_for_active_log() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-report");
        let log = crate::session_log::SessionLog::open(session_dir.clone()).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();
        std::fs::create_dir_all(session_dir.join("turns")).unwrap();
        std::fs::write(
            session_dir.join("turns").join("turn_001_stdout.txt"),
            "hello\n",
        )
        .unwrap();

        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.session_log = Some(Arc::new(std::sync::Mutex::new(log)));
        }
        let report = api_session_report_task_response(
            "report1".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert!(report.done);
        assert_eq!(report.id, "report1");
        assert!(report.byte_stream.is_some());
        let stream = report.byte_stream.unwrap();
        assert_eq!(stream.id, "report1");
        assert_eq!(stream.stream_id, "report1:session-report");
        assert_eq!(stream.content_type, "application/zip");
        assert!(stream.filename.as_deref().unwrap_or("").ends_with(".zip"));
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["content_type"], "application/zip");
        assert!(stream.result["filename"]
            .as_str()
            .unwrap_or("")
            .ends_with(".zip"));
        assert_eq!(
            stream.result["size"].as_u64().unwrap(),
            stream.bytes.len() as u64
        );
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(stream.bytes)).unwrap();
        assert!(zip.by_name("summary.json").is_ok());
        assert!(zip.by_name("turns/turn_001_stdout.txt").is_ok());

        let invalid = api_session_report_task_response(
            "report2".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
            &rt,
        )
        .await;
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn session_delete_rpc_preserves_body_shape() {
        let invalid_session = api_session_delete_response(
            "del1".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["ok"], false);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
    }

    #[tokio::test]
    async fn context_snapshot_rpc_preserves_http_status() {
        let invalid_session = api_session_context_snapshot_response(
            "ctx1".to_string(),
            Some(&serde_json::json!({
                "session_id": "../bad",
                "file": "snapshot.json",
            })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
        assert_eq!(invalid_session["result"]["_httpStatus"], 400);
        assert_eq!(invalid_session["result"]["_httpOk"], false);

        let missing_selector = api_session_context_snapshot_response(
            "ctx2".to_string(),
            Some(&serde_json::json!({
                "session_id": "missing-session",
            })),
        )
        .await;
        assert_eq!(
            missing_selector["result"]["error"],
            "missing snapshot selector"
        );
        assert_eq!(missing_selector["result"]["_httpStatus"], 400);
        assert_eq!(missing_selector["result"]["_httpOk"], false);

        let invalid_index = api_session_context_snapshot_response(
            "ctx3".to_string(),
            Some(&serde_json::json!({
                "session_id": "missing-session",
                "request_index": "abc",
            })),
        )
        .await;
        assert_eq!(invalid_index["result"]["error"], "invalid request_index");
        assert_eq!(invalid_index["result"]["_httpStatus"], 400);
        assert_eq!(invalid_index["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn current_upload_delete_preserves_http_status() {
        // Projectless daemons resolve the daemon-global store instead of
        // refusing; deleting an id that is not there stays idempotent-ok.
        let rt_no_root = runtime();
        let no_root = api_session_current_upload_delete_response(
            "upl1".to_string(),
            Some(&serde_json::json!({ "id": "missing-upload" })),
            &rt_no_root,
        )
        .await;
        assert_eq!(no_root["t"], "response");
        assert_eq!(no_root["ok"], true);
        assert_eq!(no_root["result"]["_httpStatus"], 200);
        assert_eq!(no_root["result"]["_httpOk"], true);

        let dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(dir.path().to_path_buf());
        }
        let missing_id = api_session_current_upload_delete_response(
            "upl2".to_string(),
            Some(&serde_json::json!({})),
            &rt,
        )
        .await;
        assert_eq!(missing_id["result"]["error"], "missing upload id");
        assert_eq!(missing_id["result"]["_httpStatus"], 400);
        assert_eq!(missing_id["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn current_uploads_lists_pending_uploads() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(project.path().to_path_buf());
        }
        let bytes = b"dashboard list upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "listed.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let response = api_session_current_uploads_response("uploads1".to_string(), &rt).await;
        assert_eq!(response["t"], "response");
        assert_eq!(response["ok"], true);
        let uploads = response["result"].as_array().expect("uploads array");
        assert!(
            uploads.iter().any(|upload| upload["id"] == descriptor.id),
            "upload list did not include committed descriptor: {response}"
        );
    }

    #[tokio::test]
    async fn current_upload_raw_streams_requested_range() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let bytes = b"dashboard raw upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "raw.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let response = api_session_current_upload_raw_task_response(
            "raw1".to_string(),
            Some(&serde_json::json!({
                "id": descriptor.id,
                "offset": 10,
                "length": 6,
            })),
            &rt,
        )
        .await;
        assert!(response.done);
        assert_eq!(response.id, "raw1");
        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.id, "raw1");
        assert_eq!(stream.stream_id, format!("raw1:upload:{}", descriptor.id));
        assert_eq!(stream.content_type, "text/plain");
        assert_eq!(stream.filename.as_deref(), Some("raw.txt"));
        assert_eq!(stream.bytes, &bytes[10..16]);
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["id"], descriptor.id);
        assert_eq!(stream.result["name"], "raw.txt");
        assert_eq!(stream.result["filename"], "raw.txt");
        assert_eq!(stream.result["mime"], "text/plain");
        assert_eq!(stream.result["content_type"], "text/plain");
        assert_eq!(stream.result["size"], 6);
        assert_eq!(stream.result["total_size"], bytes.len());
        assert_eq!(stream.result["offset"], 10);
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 16);
        assert_eq!(stream.result["resumable"], true);

        let invalid = api_session_current_upload_raw_task_response(
            "raw2".to_string(),
            Some(&serde_json::json!({
                "id": descriptor.id,
                "offset": bytes.len() + 1,
                "length": 1,
            })),
            &rt,
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["t"], "response");
        assert_eq!(invalid.frame["ok"], true);
        assert_eq!(invalid.frame["result"]["_httpStatus"], 416);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
        assert_eq!(
            invalid.frame["result"]["error"],
            "range start beyond upload size"
        );
    }

    /// Route-level proof for projectless daemons (task "Wave 1F"): with no
    /// project root anywhere, a staged upload POST commits into the
    /// daemon-global store and the raw read streams the same bytes back.
    /// Fixture writes go through the process state root (the live
    /// `~/.intendant` in bin unit tests — same convention as the
    /// session-frame transfer test), so the session id is unique per run
    /// and the store dir is removed afterwards.
    #[tokio::test]
    async fn projectless_staged_upload_posts_and_reads_raw_from_global_store() {
        let mut rt = runtime();
        assert!(rt.project_root.is_none());
        rt.session_id = format!(
            "projectless-upload-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let session_store_dir = crate::global_store::global_store_root()
            .join("uploads")
            .join(&rt.session_id);

        let bytes = b"projectless staged upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        // POST: commit with no project root resolves the global store.
        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            None,
            None,
            Some(rt.session_id.as_str()),
            "projectless.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();
        assert!(
            descriptor.path.starts_with(&session_store_dir),
            "projectless upload must land in the global store, got {}",
            descriptor.path.display()
        );

        // Raw GET: the projectless runtime resolves the same store.
        let response = api_session_current_upload_raw_task_response(
            "projectless-raw".to_string(),
            Some(&serde_json::json!({ "id": descriptor.id })),
            &rt,
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&session_store_dir);
        assert!(response.done);
        let stream = response.byte_stream.expect("raw read must stream bytes");
        assert_eq!(stream.bytes, bytes);
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["id"], descriptor.id);
        cleanup.expect("global-store session dir cleanup");
    }

    #[tokio::test]
    async fn control_stream_json_lines_emit_lifecycle_frames() {
        let (line_tx, line_rx) = mpsc::channel::<String>(8);
        let stream_task = tokio::spawn(async move {
            for line in [
                r#"{"type":"start","limit":1,"quick_limit":1}"#,
                r#"{"type":"session","partial":true,"session":{"session_id":"s1"}}"#,
                r#"{"type":"done"}"#,
            ] {
                line_tx.send(format!("{line}\n")).await.unwrap();
            }
        });
        let (task_tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);

        stream_json_lines_response(
            "stream1".to_string(),
            "api_sessions_stream".to_string(),
            line_rx,
            stream_task,
            task_tx,
            CancellationToken::new(),
        )
        .await;

        let mut frames = Vec::new();
        while let Some(task) = rx.recv().await {
            frames.push(task);
            if frames.last().unwrap().done {
                break;
            }
        }

        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].frame["t"], "stream_start");
        assert_eq!(frames[0].frame["method"], "api_sessions_stream");
        assert!(!frames[0].done);
        assert_eq!(frames[1].frame["t"], "stream_event");
        assert_eq!(frames[1].frame["seq"], 0);
        assert_eq!(frames[1].frame["event"]["type"], "start");
        assert_eq!(frames[2].frame["event"]["session"]["session_id"], "s1");
        assert_eq!(frames[3].frame["event"]["type"], "done");
        assert_eq!(frames[4].frame["t"], "stream_end");
        assert_eq!(frames[4].frame["ok"], true);
        assert_eq!(frames[4].frame["result"]["events"], 3);
        assert!(frames[4].done);
    }

    #[test]
    fn session_rpc_params_parse_limits_and_ids() {
        assert_eq!(
            control_session_limit(&serde_json::json!({})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": 25})),
            Some(25)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": 0})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": "nope"})),
            Some(CONTROL_DEFAULT_SESSION_LIMIT)
        );
        assert_eq!(
            control_session_limit(&serde_json::json!({"limit": "all"})),
            None
        );
        assert_eq!(control_session_detail_limit(&serde_json::json!({})), None);
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": 25})),
            Some(25)
        );
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": "25"})),
            Some(25)
        );
        assert_eq!(
            control_session_detail_limit(&serde_json::json!({"limit": "all"})),
            None
        );
        assert_eq!(
            control_session_ids(&serde_json::json!({"ids": "a,b, c"})),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            control_session_ids(&serde_json::json!({"ids": ["a,b", "c"]})),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            sessions_stream_request_line(Some(&serde_json::json!({}))),
            "GET /api/sessions/stream HTTP/1.1"
        );
        assert_eq!(
            sessions_stream_request_line(Some(&serde_json::json!({"limit": "all"}))),
            "GET /api/sessions/stream?limit=all HTTP/1.1"
        );
        assert_eq!(
            sessions_stream_request_line(Some(&serde_json::json!({"limit": 25}))),
            "GET /api/sessions/stream?limit=25 HTTP/1.1"
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": ["a", " b "]})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": "[\"a\",\"b\"]"})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_project_filter(&serde_json::json!({"projects": "a,b"})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            control_capability_query(
                &serde_json::json!({"capabilities": ["display", "custom:gpu"]})
            ),
            "capability=display&capability=custom:gpu"
        );
    }

    // ── Sessions-family tunnel/HTTP parity fixtures (S4a, design §8) ──
    //
    // Same discipline as the fs set (api_transfers_fs.rs): the same
    // params through the ONE neutral fn — `sessions_list_api_response`,
    // `sessions_search_api_response`, `session_detail_api_response`,
    // `session_agent_output_api_response`,
    // `session_context_snapshot_api_response` — rendered by the HTTP
    // adapter and by the tunnel framers, yield IDENTICAL JSON bodies.
    // The sessions-family envelope differences, deliberate and pinned
    // here, extend the fs enumeration:
    //
    //  1. Pre-_httpStatus envelopes: api_sessions / api_session_detail /
    //     api_sessions_search predate the injected-status envelope —
    //     their frames are `{t,id,ok:true,result:<body>}` with NO
    //     `_httpStatus`/`_httpOk` (`frame_api_json_body_response`), so
    //     HTTP-side statuses (detail 400/404) do not surface on the
    //     tunnel; error bodies carry `{"error":…}` inline under ok:true.
    //  2. api_sessions result-shape guard: the tunnel rejects a
    //     non-array list body with ok:false "session list returned
    //     invalid JSON"; HTTP passes any body through under 200.
    //  3. api_session_agent_output / api_session_context_snapshot ride
    //     the injected-status envelope (`frame_api_response` →
    //     `http_body_response`): result objects gain
    //     `_httpStatus`/`_httpOk` matching the HTTP status exactly.
    //  4. Transport-owned param decode: HTTP's `ids=` comma filter
    //     distinguishes present-but-empty (→ `[]`) from absent; the
    //     tunnel's ids vocabulary cannot express the empty filter, and
    //     its ids path never applies the limit truncation (historical
    //     for_ids semantics — the tunnel maps ids requests to
    //     `limit=None`). HTTP reads limit/max/count aliases capped at
    //     SESSION_LIST_LIMIT; the tunnel reads `limit` with the
    //     CONTROL_DEFAULT_SESSION_LIMIT default and the "all"/"full"
    //     escape. The tunnel trims session_id and accepts
    //     sessionId/id aliases; HTTP takes the raw path segment.
    //  5. Header tails are HTTP-lane decoration only: list/search and
    //     the intendant-source agent-output shapes carry the
    //     wildcard-CORS tail; detail/context-snapshot the canonical
    //     tail; the external-source agent-output success rides the
    //     canonical tail (historical asymmetry). The tunnel has no
    //     header lane.
    //  6. Task-failure shapes stay transport-owned: HTTP answers 200
    //     with an `{"error":…}` body (list/detail); the tunnel emits
    //     ok:false frames.

    /// Render a neutral response through the HTTP adapter and split it
    /// into (status, body).
    fn http_status_and_body(response: crate::web_gateway::ApiResponse) -> (u16, serde_json::Value) {
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
        let body = serde_json::from_slice(&bytes[split + 4..]).expect("json body");
        (status, body)
    }

    /// Strip the injected-status tunnel envelope (difference #3): assert
    /// the `http_body_response` shape against the HTTP adapter's status
    /// and return the result minus the injected keys.
    fn tunnel_result_body(frame: &serde_json::Value, expect_status: u16) -> serde_json::Value {
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true, "{frame}");
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(
            map.remove("_httpStatus"),
            Some(serde_json::json!(expect_status)),
            "{frame}"
        );
        assert_eq!(
            map.remove("_httpOk"),
            Some(serde_json::json!((200..300).contains(&expect_status)))
        );
        result
    }

    /// The pre-_httpStatus envelope (difference #1): ok:true with the
    /// body as the verbatim result — no injected status metadata.
    fn tunnel_plain_body(frame: &serde_json::Value) -> serde_json::Value {
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true, "{frame}");
        if let Some(result) = frame["result"].as_object() {
            assert!(
                !result.contains_key("_httpStatus") && !result.contains_key("_httpOk"),
                "pre-_httpStatus envelope must not carry injected status metadata: {frame}"
            );
        }
        frame["result"].clone()
    }

    /// Unique-per-run fixture session in the live logs store (the
    /// bin-test convention; removed by the caller).
    fn parity_session_fixture(prefix: &str) -> (String, std::path::PathBuf) {
        let session_id = format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let log_dir = crate::platform::intendant_home()
            .join("logs")
            .join(&session_id);
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.agent_output_with_id("parity stdout", "", Some("Codex"), Some("parity-out-1"));
        drop(log);
        (session_id, log_dir)
    }

    #[tokio::test]
    async fn parity_sessions_list_serves_the_same_rows_on_both_transports() {
        let (session_id, log_dir) = parity_session_fixture("parity-list");

        // ids filter, plain view.
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::sessions_list_api_response(
                Some(vec![session_id.clone()]),
                None,
                false,
            ),
        );
        assert_eq!(status, 200);
        let frame = api_sessions_response(
            "parity-list".to_string(),
            Some(&serde_json::json!({ "ids": [session_id] })),
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&log_dir);
        let tunnel_body = tunnel_plain_body(&frame);
        assert!(tunnel_body.is_array(), "{frame}");
        assert_eq!(tunnel_body, http_body);
        assert!(
            tunnel_body
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session_id"] == session_id),
            "fixture session must be listed: {tunnel_body}"
        );
        cleanup.expect("parity list fixture cleanup");
    }

    #[tokio::test]
    async fn parity_sessions_list_usage_view_serves_the_same_projection() {
        let (session_id, log_dir) = parity_session_fixture("parity-usage");
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::sessions_list_api_response(
                Some(vec![session_id.clone()]),
                None,
                true,
            ),
        );
        let frame = api_sessions_response(
            "parity-usage".to_string(),
            Some(&serde_json::json!({ "ids": [session_id], "view": "usage" })),
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&log_dir);
        assert_eq!(status, 200);
        let tunnel_body = tunnel_plain_body(&frame);
        assert_eq!(tunnel_body, http_body);
        let row = &tunnel_body.as_array().unwrap()[0];
        assert!(row.get("session_id").is_some());
        assert!(
            row.get("entries").is_none() && row.get("goal").is_none(),
            "usage view must project rows down: {row}"
        );
        cleanup.expect("parity usage fixture cleanup");
    }

    #[tokio::test]
    async fn parity_sessions_search_serves_the_same_body_on_both_transports() {
        let _guard = crate::web_gateway::SESSIONS_SEARCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The no-input short-circuit: deterministic, store-free.
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::sessions_search_api_response(
                String::new(),
                "all".to_string(),
                String::new(),
                Vec::new(),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(status, 200);
        let frame = api_sessions_search_response(
            "parity-search".to_string(),
            Some(&serde_json::json!({ "q": "" })),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(tunnel_plain_body(&frame), http_body);
    }

    #[tokio::test]
    async fn parity_session_detail_serves_the_same_body_on_both_transports() {
        let (session_id, log_dir) = parity_session_fixture("parity-detail");
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_detail_api_response(&session_id, "intendant", Some(5), None),
        );
        let frame = api_session_detail_response(
            "parity-detail".to_string(),
            Some(&serde_json::json!({ "session_id": session_id, "limit": 5 })),
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&log_dir);
        assert_eq!(status, 200);
        assert_eq!(tunnel_plain_body(&frame), http_body);
        cleanup.expect("parity detail fixture cleanup");
    }

    #[tokio::test]
    async fn parity_session_detail_errors_share_bodies_under_the_plain_envelope() {
        // Difference #1 pinned from both sides: the HTTP 400 does not
        // surface on the tunnel — only the identical error body does.
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_detail_api_response("..", "intendant", None, None),
        );
        assert_eq!(status, 400);
        let frame = api_session_detail_response(
            "parity-detail-invalid".to_string(),
            Some(&serde_json::json!({ "session_id": ".." })),
        )
        .await;
        assert_eq!(tunnel_plain_body(&frame), http_body);
    }

    #[tokio::test]
    async fn parity_session_agent_output_serves_the_same_body_with_status_metadata() {
        let (session_id, log_dir) = parity_session_fixture("parity-output");
        let ids_body = r#"{"ids":["parity-out-1","parity-missing"]}"#;
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_agent_output_api_response(
                ids_body,
                &session_id,
                "intendant",
            ),
        );
        // The tunnel serializes its whole params object as the body; the
        // chunk fetch only reads `ids`.
        let frame = api_session_agent_output_response(
            "parity-output".to_string(),
            Some(&serde_json::json!({
                "session_id": session_id,
                "ids": ["parity-out-1", "parity-missing"],
            })),
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&log_dir);
        assert_eq!(status, 200);
        assert_eq!(tunnel_result_body(&frame, 200), http_body);
        cleanup.expect("parity output fixture cleanup");

        // Missing-ids: same 400 body under each envelope.
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_agent_output_api_response("{}", "abc123", "intendant"),
        );
        assert_eq!(status, 400);
        let frame = api_session_agent_output_response(
            "parity-output-missing-ids".to_string(),
            Some(&serde_json::json!({ "session_id": "abc123" })),
        )
        .await;
        assert_eq!(tunnel_result_body(&frame, 400), http_body);
    }

    // ── S4b parity: deletes + report (see the S4a enumeration above; the
    // S4b-specific envelope differences are enumerated on the worktrees
    // fixture set in api_control.rs) ──

    #[tokio::test]
    async fn parity_session_delete_serves_the_same_body_on_both_transports() {
        // Invalid id: deterministic, store-free; the delete trio rides the
        // pre-_httpStatus envelope (difference #1).
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_delete_api_response("..", "session"),
        );
        assert_eq!(status, 200);
        let frame = api_session_delete_response(
            "parity-delete".to_string(),
            Some(&serde_json::json!({ "session_id": ".." })),
        )
        .await;
        assert_eq!(tunnel_plain_body(&frame), http_body);

        // A real deletion under identical pre-state on each lane.
        let (session_id, log_dir) = parity_session_fixture("parity-delete-http");
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_delete_api_response(&session_id, "session"),
        );
        assert_eq!(status, 200);
        assert!(!log_dir.exists(), "http-lane delete must remove the dir");
        assert_eq!(http_body["ok"], true);
        assert_eq!(http_body["deleted"], "session");

        let (session_id, log_dir) = parity_session_fixture("parity-delete-rpc");
        let frame = api_session_delete_response(
            "parity-delete-2".to_string(),
            Some(&serde_json::json!({ "session_id": session_id })),
        )
        .await;
        assert!(!log_dir.exists(), "tunnel delete must remove the dir");
        let tunnel_body = tunnel_plain_body(&frame);
        assert_eq!(tunnel_body["ok"], true);
        assert_eq!(tunnel_body["deleted"], "session");
        // bytes_freed varies with the fixture inode sizes; the shape keys
        // are the parity claim.
        assert_eq!(
            tunnel_body.as_object().unwrap().keys().collect::<Vec<_>>(),
            http_body.as_object().unwrap().keys().collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn parity_session_report_serves_the_same_zip_and_meta_on_both_transports() {
        let (session_id, log_dir) = parity_session_fixture("parity-report");
        std::fs::write(log_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();

        let report = crate::web_gateway::session_report_zip_for_request(&session_id, None, None)
            .unwrap_or_else(|_| panic!("fixture report must build"));
        let response = crate::web_gateway::session_report_api_response(report);
        let (http_bytes, http_meta) = match &response {
            crate::web_gateway::ApiResponse::Bytes { bytes, meta, .. } => {
                let crate::web_gateway::BytesPayload::InMemory(payload) = bytes;
                (payload.clone(), meta.clone())
            }
            _ => panic!("report must ride the bytes lane"),
        };

        let task = api_session_report_task_response(
            "parity-report".to_string(),
            Some(&serde_json::json!({ "session_id": session_id })),
            &runtime(),
        )
        .await;
        let cleanup = std::fs::remove_dir_all(&log_dir);
        let stream = task.byte_stream.expect("tunnel byte stream");
        assert_eq!(stream.stream_id, "parity-report:session-report");
        assert_eq!(stream.content_type, "application/zip");
        assert_eq!(stream.bytes.len(), http_bytes.len());
        assert_eq!(stream.result, http_meta);
        assert_eq!(
            stream.filename.as_deref(),
            http_meta["filename"].as_str(),
            "byte-stream filename lifts from the shared meta"
        );

        // Invalid id: per-lane error framing — the tunnel answers the
        // injected-status envelope; HTTP answers wildcard json (pinned by
        // the goldens).
        let invalid = api_session_report_task_response(
            "parity-report-invalid".to_string(),
            Some(&serde_json::json!({ "session_id": ".." })),
            &runtime(),
        )
        .await;
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        cleanup.expect("parity report fixture cleanup");
    }

    #[tokio::test]
    async fn parity_session_context_snapshot_shares_bodies_with_status_metadata() {
        // Missing selector: 400 on both lanes.
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_context_snapshot_api_response(
                "abc123", "intendant", None, None, None, None,
            ),
        );
        assert_eq!(status, 400);
        let frame = api_session_context_snapshot_response(
            "parity-snapshot".to_string(),
            Some(&serde_json::json!({ "session_id": "abc123" })),
        )
        .await;
        assert_eq!(tunnel_result_body(&frame, 400), http_body);
        assert_eq!(http_body["error"], "missing snapshot selector");
    }

    // ── S4c parity: current-session + managed-context (design §8) ──
    //
    // Extends the S4a (above) and S4b (api_control.rs) enumerations.
    // The S4c-specific envelope differences, deliberate and pinned
    // across this slice's fixtures (current-session family here;
    // managed-context in api_control.rs):
    //
    //  1. All thirteen twins ride the injected-status envelope
    //     (frame_api_response → http_body_response): result OBJECTS
    //     gain _httpStatus/_httpOk matching the HTTP status; the
    //     uploads list (array body) passes through undecorated —
    //     byte-identical to its historical json_body_response framing.
    //  2. Header tails stay HTTP-lane decoration: the current/* family
    //     and upload POST/list/raw ride the wildcard-CORS tail,
    //     managed-context and the upload delete the canonical tail,
    //     raw fetches the inline Content-Disposition; the tunnel
    //     renders none of them.
    //  3. Transport-owned upload carriage: HTTP streams the raw POST
    //     body (100-continue, spool, its own 413/400 wordings); the
    //     tunnel spools upload_start/chunk/end frames with its own
    //     wire-integrity errors (sequence/size mismatch, base64). The
    //     commit leg is the one shared neutral fn. The raw read is one
    //     unbounded full body on HTTP but ranged +
    //     UPLOAD_MAX_BYTES-capped on the tunnel (the 416/413 shapes are
    //     tunnel-only), over the same content core.
    //  4. Content-core error framing stays per-lane on the raw read:
    //     wildcard `{"error":…}` (HTTP) vs `{"ok":false,"error":…}`
    //     with the 416 total_size sidecar (tunnel), both built from the
    //     one CurrentUploadRawError.
    //  5. Transport-owned param decode: tunnel id/offset/length aliases
    //     and defaults, changes path/query synthesis
    //     (changes_request_line), managed-context query synthesis
    //     (managed_context_request_line, whose missing-query shape is
    //     tunnel-only); HTTP takes raw path segments and query strings.
    //  6. Task-failure and resolution shapes stay per-lane: ok:false
    //     frames vs in-band HTTP errors; both lanes build their
    //     lock-poisoned 500s and the agent-output no-active-log 404
    //     with the same wording but their own framing.

    #[tokio::test]
    async fn parity_current_history_family_shares_bodies_with_status_metadata() {
        // No file watcher: the 503 shape on both lanes, store-free.
        let rt = runtime();
        let (status, http_body) =
            http_status_and_body(crate::web_gateway::current_history_api_response(None).await);
        assert_eq!(status, 503);
        let frame = api_session_current_history_response("parity-hist".to_string(), &rt).await;
        assert_eq!(tunnel_result_body(&frame, 503), http_body);
        assert_eq!(http_body["error"], "file watcher not active");

        let frame = api_session_current_rollback_response(
            "parity-rollback".to_string(),
            Some(&serde_json::json!({ "round_id": 1 })),
            &rt,
        )
        .await;
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::current_rollback_api_response(
                r#"{"round_id":1}"#,
                None,
                None,
                &rt.bus,
            )
            .await,
        );
        assert_eq!(status, 503);
        assert_eq!(tunnel_result_body(&frame, 503), http_body);

        let frame = api_session_current_redo_response("parity-redo".to_string(), &rt).await;
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::current_redo_api_response(None, None).await,
        );
        assert_eq!(status, 503);
        assert_eq!(tunnel_result_body(&frame, 503), http_body);

        let frame = api_session_current_prune_response("parity-prune".to_string(), &rt).await;
        let (status, http_body) =
            http_status_and_body(crate::web_gateway::current_prune_api_response(None).await);
        assert_eq!(status, 503);
        assert_eq!(tunnel_result_body(&frame, 503), http_body);
    }

    #[tokio::test]
    async fn parity_current_changes_shares_bodies_with_status_metadata() {
        // No snapshot dir / project root: the 503 watcher-absent shape.
        // The tunnel synthesizes the request line from params
        // (difference #5); both lanes then run the one neutral fn.
        let rt = runtime();
        let home = tempfile::tempdir().unwrap();
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::session_current_changes_api_response(
                "GET /api/session/current/changes HTTP/1.1",
                None,
                None,
                home.path(),
            ),
        );
        assert_eq!(status, 503);
        let frame =
            api_session_current_changes_response("parity-changes".to_string(), None, &rt).await;
        assert_eq!(tunnel_result_body(&frame, 503), http_body);
        assert_eq!(http_body["error"], "file watcher not active");
    }

    #[tokio::test]
    async fn parity_current_agent_output_shares_bodies_with_status_metadata() {
        let (session_id, log_dir) = parity_session_fixture("parity-current-output");
        {
            let rt = runtime();
            let log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            {
                let mut session = rt.shared_session.write().await;
                session.session_log = Some(Arc::new(std::sync::Mutex::new(log)));
            }

            // Success: one persisted id — found in the primary dir, so
            // neither lane runs the fallback sweep.
            let (status, http_body) = http_status_and_body(
                crate::web_gateway::current_agent_output_api_response(
                    r#"{"ids":["parity-out-1"]}"#,
                    &log_dir,
                ),
            );
            assert_eq!(status, 200);
            assert_eq!(http_body["outputs"][0]["output_id"], "parity-out-1");
            let frame = api_session_current_agent_output_response(
                "parity-current-output".to_string(),
                Some(&serde_json::json!({ "ids": ["parity-out-1"] })),
                &rt,
            )
            .await;
            assert_eq!(tunnel_result_body(&frame, 200), http_body);

            // Decode error: 400 missing-ids on both lanes.
            let (status, http_body) = http_status_and_body(
                crate::web_gateway::current_agent_output_api_response("{}", &log_dir),
            );
            assert_eq!(status, 400);
            let frame = api_session_current_agent_output_response(
                "parity-current-output-400".to_string(),
                Some(&serde_json::json!({})),
                &rt,
            )
            .await;
            assert_eq!(tunnel_result_body(&frame, 400), http_body);
            assert_eq!(http_body["error"], "missing output ids");
        }
        std::fs::remove_dir_all(&log_dir)
            .unwrap_or_else(|e| panic!("cleanup {session_id}: {e}"));
    }

    #[tokio::test]
    async fn parity_current_uploads_list_and_delete_share_bodies() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(project.path().to_path_buf());
        }
        let scope = crate::global_store::StoreScope::resolve(Some(project.path()));
        let pending = crate::web_gateway::pending_upload_session_dir(&scope);

        // Empty list: the array body passes both envelopes undecorated
        // (difference #1).
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::current_uploads_list_api_response(&pending, &scope),
        );
        assert_eq!(status, 200);
        assert_eq!(http_body, serde_json::json!([]));
        let frame =
            api_session_current_uploads_response("parity-uploads-list".to_string(), &rt).await;
        assert_eq!(tunnel_plain_body(&frame), http_body);

        // Idempotent delete of a missing id: 200 {"ok":true} on both
        // lanes (canonical tail on HTTP — difference #2).
        let (status, http_body) = http_status_and_body(
            crate::web_gateway::current_upload_delete_api_response(
                Some(project.path()),
                None,
                "parity-nope",
                &rt.bus,
            ),
        );
        assert_eq!(status, 200);
        let frame = api_session_current_upload_delete_response(
            "parity-upload-delete".to_string(),
            Some(&serde_json::json!({ "upload_id": "parity-nope" })),
            &rt,
        )
        .await;
        assert_eq!(tunnel_result_body(&frame, 200), http_body);
        assert_eq!(http_body, serde_json::json!({ "ok": true }));
    }

    #[tokio::test]
    async fn parity_current_upload_commit_and_raw_share_content() {
        use std::io::Write as _;
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        {
            let mut session = rt.shared_session.write().await;
            session.project_root_for_changes = Some(project.path().to_path_buf());
        }
        let payload = b"parity staged upload bytes".to_vec();

        // Tunnel commit: the upload lane's spooled frames end in the
        // same shared neutral fn the HTTP POST runs (difference #3 —
        // only the carriage differs).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&payload).unwrap();
        let upload = InboundUploadState {
            method: "api_session_current_upload".to_string(),
            params: serde_json::json!({ "name": "parity.txt", "mime": "text/plain" }),
            tmp,
            total_bytes: payload.len(),
            expected_chunks: 1,
            next_seq: 1,
            received_bytes: payload.len(),
        };
        let commit = api_session_current_upload_task_response(
            "parity-upload-commit".to_string(),
            upload,
            rt.clone(),
        )
        .await;
        let tunnel_descriptor = tunnel_result_body(&commit.frame, 200);
        assert_eq!(tunnel_descriptor["name"], "parity.txt");
        let upload_id = tunnel_descriptor["id"].as_str().expect("id").to_string();

        // HTTP commit through the same neutral fn: same descriptor
        // shape (ids/paths are store-generated per commit).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&payload).unwrap();
        let (status, http_descriptor) = http_status_and_body(
            crate::web_gateway::current_upload_commit_api_response(
                Some(project.path()),
                None,
                Some("parity-session"),
                "parity.txt",
                "text/plain",
                crate::upload_store::UploadDestination::Task,
                tmp,
                payload.len(),
                &rt.bus,
            ),
        );
        assert_eq!(status, 200);
        assert_eq!(
            tunnel_descriptor.as_object().unwrap().keys().collect::<Vec<_>>(),
            http_descriptor.as_object().unwrap().keys().collect::<Vec<_>>(),
        );

        // Raw read of the tunnel-committed upload: identical bytes on
        // both lanes over the one content core; the tunnel's
        // byte_stream_end.result meta and HTTP's header tail are the
        // per-lane decorations (differences #2/#3).
        let scope = crate::global_store::StoreScope::resolve(Some(project.path()));
        let pending = crate::web_gateway::pending_upload_session_dir(&scope);
        let http_raw = crate::web_gateway::current_upload_raw_api_response(
            &upload_id,
            None,
            &pending,
            &scope,
        )
        .unwrap_or_else(|_| panic!("http raw read"));
        let (http_bytes, http_meta) = match &http_raw {
            crate::web_gateway::ApiResponse::Bytes { bytes, meta, .. } => {
                let crate::web_gateway::BytesPayload::InMemory(payload) = bytes;
                (payload.clone(), meta.clone())
            }
            crate::web_gateway::ApiResponse::Json { .. } => panic!("raw read must be bytes"),
        };
        assert_eq!(http_bytes, payload);
        let raw = api_session_current_upload_raw_task_response(
            "parity-upload-raw".to_string(),
            Some(&serde_json::json!({ "id": upload_id })),
            &rt,
        )
        .await;
        let stream = raw.byte_stream.expect("tunnel raw byte stream");
        assert_eq!(stream.bytes, payload);
        assert_eq!(stream.result, http_meta);
        assert_eq!(stream.result["range_end"], payload.len());
        assert_eq!(stream.result["resumable"], true);

        // Missing id: the shared content core's NotFound framed
        // per-lane (difference #4).
        let missing = crate::web_gateway::current_upload_raw_api_response(
            "parity-missing",
            None,
            &pending,
            &scope,
        );
        let err = match missing {
            Err(err) => err,
            Ok(_) => panic!("missing upload must err"),
        };
        assert_eq!(err.status(), 404);
        assert_eq!(err.message(), "upload not found");
        let raw = api_session_current_upload_raw_task_response(
            "parity-upload-raw-404".to_string(),
            Some(&serde_json::json!({ "id": "parity-missing" })),
            &rt,
        )
        .await;
        assert!(raw.byte_stream.is_none());
        assert_eq!(raw.frame["result"]["_httpStatus"], 404);
        assert_eq!(raw.frame["result"]["ok"], false);
        assert_eq!(raw.frame["result"]["error"], "upload not found");
    }
}
