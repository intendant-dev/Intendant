//! Control-frame dispatch: the method match (control_frame_response) and
//! the per-channel frame handlers it fans into -- uploads, terminals,
//! presence and voice, live usage, tool requests, async queries, display
//! input spawn, cached bootstrap, and status.

use super::*;

/// Tunnel adapter for the transport-neutral api core (transport-
/// unification design §2.1): render a JSON [`crate::web_gateway::ApiResponse`]
/// into the tunnel's historical envelope — `http_body_response`, which
/// wraps the body as `{t:"response", id, ok:true, result:<body>}` and
/// injects `_httpStatus`/`_httpOk` into the result object. A byte
/// response on a JSON-only method is a wiring bug and fails closed.
pub(crate) fn frame_api_response(
    id: String,
    response: crate::web_gateway::ApiResponse,
    label: &str,
) -> serde_json::Value {
    match response {
        crate::web_gateway::ApiResponse::Json { status, body, .. } => {
            http_body_response(id, status, body.into_string(), label)
        }
        crate::web_gateway::ApiResponse::Bytes { .. } => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned an unexpected byte response"),
        }),
    }
}

/// Tunnel adapter for byte-capable methods: `Bytes` becomes a
/// `byte_stream_start/chunk/end` sequence — chunking, credits, and
/// backpressure stay wire.rs-owned — with the neutral fn's `meta`
/// object emitted verbatim as `byte_stream_end.result` (and the
/// stream's filename lifted from it); JSON responses (the error
/// shapes) ride the plain response envelope.
pub(crate) fn frame_api_task_response(
    id: String,
    response: crate::web_gateway::ApiResponse,
    stream_suffix: &str,
    label: &str,
) -> ControlTaskResponse {
    match response {
        crate::web_gateway::ApiResponse::Bytes {
            content_type,
            bytes: crate::web_gateway::BytesPayload::InMemory(bytes),
            meta,
            ..
        } => {
            let filename = meta
                .get("filename")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::Value::Null,
                byte_stream: Some(ControlByteStream {
                    id: id.clone(),
                    stream_id: format!("{id}:{stream_suffix}"),
                    content_type,
                    filename,
                    bytes,
                    result: meta,
                }),
                done: true,
            }
        }
        json @ crate::web_gateway::ApiResponse::Json { .. } => ControlTaskResponse {
            id: id.clone(),
            frame: frame_api_response(id, json, label),
            byte_stream: None,
            done: true,
        },
    }
}

pub(crate) fn control_frame_response(
    text: &str,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    outbound_queue: &mut OutboundControlQueue,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str()).unwrap_or("");
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !matches!(t, "hello" | "ping" | "request") {
        if let Err(error) = authorize_dashboard_control_frame(runtime, t) {
            return Some(dashboard_control_error_response(id, error));
        }
    }
    match t {
        "hello" => {
            runtime.response_credit_enabled = parsed
                .get("features")
                .and_then(|features| features.as_array())
                .map(|features| {
                    features.iter().any(|feature| {
                        matches!(feature.as_str(), Some("response_credit") | Some("credit"))
                    })
                })
                .unwrap_or(false);
            Some(serde_json::json!({
                "t": "hello_ack",
                "id": id,
                "protocol": CONTROL_PROTOCOL_VERSION,
                "session_id": runtime.session_id,
                "daemon_public_key": runtime.daemon_public_key,
                "features": control_features(),
            }))
        }
        "ping" => Some(serde_json::json!({
            "t": "pong",
            "id": id,
            "unix_ms": chrono::Utc::now().timestamp_millis(),
        })),
        "display_input" => {
            spawn_dashboard_display_input(parsed, runtime.clone());
            None
        }
        "terminal_open" => {
            control_terminal_open_frame(parsed, runtime, terminal_events_tx, terminal_forwarders)
        }
        "terminal_input" => control_terminal_input_frame(parsed, runtime),
        "terminal_resize" => control_terminal_resize_frame(parsed, runtime),
        "terminal_close" => control_terminal_close_frame(parsed, runtime, terminal_forwarders),
        "terminal_share" => control_terminal_share_frame(parsed, runtime, terminal_events_tx),
        "presence_frame" => control_presence_frame(parsed, runtime.clone()),
        "egress_response" | "egress_chunk" | "egress_end" | "egress_error" => {
            crate::credential_egress::handle_browser_frame(&runtime.session_id, t, &parsed);
            None
        }
        "upload_start" => {
            control_upload_start_frame(id, parsed, runtime, pending_requests, inbound_uploads)
        }
        "upload_chunk" => control_upload_chunk_frame(id, parsed, pending_requests, inbound_uploads),
        "upload_end" => control_upload_end_frame(
            id,
            parsed,
            runtime,
            task_tx,
            pending_requests,
            inbound_uploads,
        ),
        "request" => {
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let params = parsed.get("params").cloned();
            if let Err(error) = authorize_dashboard_control_method(runtime, method, params.as_ref())
            {
                return Some(dashboard_control_error_response(id, error));
            }
            match method {
                "status" => Some(status_response_frame(id, runtime)),
                "api_credential_lease_grant" => {
                    let params_ref = params.as_ref();
                    let kind = params_ref
                        .and_then(|p| optional_string_param(p, &["kind"]))
                        .unwrap_or_default();
                    let label = params_ref
                        .and_then(|p| optional_string_param(p, &["label"]))
                        .unwrap_or_default();
                    let material = params_ref
                        .and_then(|p| optional_string_param(p, &["material", "secret"]))
                        .unwrap_or_default();
                    // Oauth kinds only: "access_token" (browser-refreshed,
                    // refresh token never leaves the vault) vs the
                    // "full_credential" opt-in; omitted means full.
                    let mode = params_ref.and_then(|p| optional_string_param(p, &["mode"]));
                    let ttl_ms = match params_ref {
                        Some(p) => match optional_u64_param(p, &["ttl_ms"]) {
                            Ok(value) => value,
                            Err(error) => return Some(dashboard_control_error_response(id, error)),
                        },
                        None => None,
                    };
                    let offline_ms = match params_ref {
                        Some(p) => match optional_u64_param(p, &["offline_ms"]) {
                            Ok(value) => value,
                            Err(error) => return Some(dashboard_control_error_response(id, error)),
                        },
                        None => None,
                    };
                    Some(
                        match crate::credential_leases::grant(
                            &kind,
                            &label,
                            &material,
                            mode.as_deref(),
                            runtime.grant.label(),
                            runtime.grant.custody_origin_class(),
                            ttl_ms,
                            offline_ms,
                        ) {
                            Ok(outcome) => serde_json::json!({
                                "t": "response",
                                "id": id,
                                "ok": true,
                                "result": {
                                    "lease_id": outcome.lease_id,
                                    "kind": outcome.kind,
                                    "expires_at_unix_ms": outcome.expires_at_unix_ms,
                                    "replaced": outcome.replaced,
                                },
                            }),
                            Err(error) => dashboard_control_error_response(
                                id,
                                format!("credential lease grant failed: {error}"),
                            ),
                        },
                    )
                }
                "api_credential_lease_renew" => {
                    let lease_id = params
                        .as_ref()
                        .and_then(|p| optional_string_param(p, &["lease_id", "leaseId"]))
                        .unwrap_or_default();
                    Some(match crate::credential_leases::renew(&lease_id) {
                        Ok(expires_at_unix_ms) => serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": {
                                "lease_id": lease_id,
                                "expires_at_unix_ms": expires_at_unix_ms,
                            },
                        }),
                        Err(error) => dashboard_control_error_response(
                            id,
                            format!("credential lease renew failed: {error}"),
                        ),
                    })
                }
                "api_credential_lease_revoke" => {
                    // Selector: lease_id or kind revokes one; omitted
                    // revokes every lease on this daemon.
                    let selector = params
                        .as_ref()
                        .and_then(|p| optional_string_param(p, &["lease_id", "leaseId", "kind"]));
                    let revoked = crate::credential_leases::revoke(
                        selector.as_deref(),
                        runtime.grant.label(),
                        runtime.grant.custody_origin_class(),
                    );
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": { "revoked": revoked },
                    }))
                }
                "api_credential_lease_status" => {
                    let leases: Vec<serde_json::Value> = crate::credential_leases::status_entries()
                        .into_iter()
                        .map(|entry| {
                            serde_json::json!({
                                "lease_id": entry.lease_id,
                                "kind": entry.kind,
                                "label": entry.label,
                                "mode": entry.mode.as_str(),
                                "granted_by": entry.granted_by,
                                "granted_at_unix_ms": entry.granted_at_unix_ms,
                                "renewed_at_unix_ms": entry.renewed_at_unix_ms,
                                "expires_at_unix_ms": entry.expires_at_unix_ms,
                                "ttl_ms": entry.ttl_ms,
                                "offline_ms": entry.offline_ms,
                                "use_count": entry.use_count,
                            })
                        })
                        .collect();
                    // The per-session path indicator: which providers are
                    // currently fueled by a browser relay instead of a lease.
                    let egress: Vec<serde_json::Value> = crate::credential_egress::relay_status()
                        .into_iter()
                        .map(|relay| {
                            serde_json::json!({
                                "kind": relay.kind,
                                "label": relay.label,
                                "session_id": relay.session_id,
                                "since_unix_ms": relay.since_unix_ms,
                            })
                        })
                        .collect();
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": {
                            "leases": leases,
                            "egress": egress,
                            "expired_note": crate::credential_leases::expired_lease_note(),
                        },
                    }))
                }
                "api_credential_custody_trail" => {
                    // The daemon's own record of custody lifecycle events —
                    // metadata only, never material (see credential_audit.rs).
                    let events: Vec<serde_json::Value> = crate::credential_audit::recent(100)
                        .into_iter()
                        .map(|event| {
                            serde_json::json!({
                                "at_unix_ms": event.at_unix_ms,
                                "event": event.event,
                                "kind": event.kind,
                                "label": event.label,
                                "actor": event.actor,
                                "origin": event.origin,
                                "detail": event.detail,
                            })
                        })
                        .collect();
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": { "events": events },
                    }))
                }
                "api_daemon_vault_fetch" => {
                    // Blind storage: the blob is E2E ciphertext this daemon
                    // can neither read nor forge (vault_store.rs).
                    let result = match crate::vault_store::fetch() {
                        Some((revision, vault, updated_unix_ms)) => serde_json::json!({
                            "revision": revision,
                            "vault": vault,
                            "updated_unix_ms": updated_unix_ms,
                        }),
                        None => serde_json::json!({ "revision": 0, "vault": null }),
                    };
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": result,
                    }))
                }
                "api_daemon_vault_publish" => {
                    let params_ref = params.as_ref();
                    let revision = match params_ref
                        .map(|p| optional_u64_param(p, &["revision"]))
                        .unwrap_or(Ok(None))
                    {
                        Ok(value) => value.unwrap_or(0),
                        Err(error) => return Some(dashboard_control_error_response(id, error)),
                    };
                    let vault = params_ref
                        .and_then(|p| p.get("vault"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
                    Some(match crate::vault_store::publish(revision, vault, now) {
                        Ok(stored) => serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": { "stored": stored, "revision": revision },
                        }),
                        Err(error) => dashboard_control_error_response(id, error.message()),
                    })
                }
                "api_credential_egress_register" => {
                    let kinds: Vec<String> = params
                        .as_ref()
                        .and_then(|p| p.get("kinds"))
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|v| v.as_str())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(match runtime.control_frames_tx.clone() {
                        None => dashboard_control_error_response(
                            id,
                            "this transport cannot carry egress frames",
                        ),
                        Some(frames_tx) => match crate::credential_egress::register(
                            &runtime.session_id,
                            runtime.grant.label(),
                            runtime.grant.custody_origin_class(),
                            &kinds,
                            frames_tx,
                        ) {
                            Ok(registered) => serde_json::json!({
                                "t": "response",
                                "id": id,
                                "ok": true,
                                "result": { "registered": registered },
                            }),
                            Err(error) => dashboard_control_error_response(
                                id,
                                format!("egress registration failed: {error}"),
                            ),
                        },
                    })
                }
                "api_credential_egress_unregister" => {
                    let kinds: Option<Vec<String>> = params
                        .as_ref()
                        .and_then(|p| p.get("kinds"))
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|v| v.as_str())
                                .map(str::to_string)
                                .collect()
                        });
                    let unregistered =
                        crate::credential_egress::unregister(&runtime.session_id, kinds.as_deref());
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": { "unregistered": unregistered },
                    }))
                }
                "api_peers" => match runtime.peer_registry.as_ref() {
                    Some(registry) => {
                        let result = serde_json::from_str::<serde_json::Value>(
                            &crate::web_gateway::peers_list_response_body(registry),
                        )
                        .unwrap_or_else(|_| serde_json::json!({"peers":[]}));
                        Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        }))
                    }
                    None => Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": false,
                        "error": "peer registry unavailable",
                    })),
                },
                "api_dashboard_targets" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::dashboard_targets_response_value(
                        &runtime.agent_card,
                        runtime.peer_registry.as_ref(),
                    ),
                })),
                "api_access_overview" => {
                    let current_principal = runtime.grant.access_principal();
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": crate::web_gateway::access_overview_response_value_for_principal(
                            &runtime.agent_card,
                            runtime.peer_registry.as_ref(),
                            Some(&current_principal),
                        ),
                    }))
                }
                "api_access_iam_state" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::access_iam_state_response_value(),
                })),
                "api_access_enrollment_requests" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::access_enrollment_requests_response_value(),
                })),
                "api_access_enrollment_decide" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    match crate::web_gateway::access_enrollment_decide_response_value(
                        params,
                        &runtime.grant.access_principal(),
                    ) {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "api_access_connect_status" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::access_connect_status_response_value(),
                })),
                "api_access_connect_claim_code" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": crate::web_gateway::access_connect_claim_code_response_value(),
                })),
                "api_access_connect_config" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    match crate::web_gateway::access_connect_config_response_value(
                        params,
                        runtime.project_root.as_deref(),
                    ) {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "api_access_set_tier" | "api_access_set_hosted_ceiling" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    let actor = runtime.grant.access_principal();
                    let result = if method == "api_access_set_hosted_ceiling" {
                        crate::web_gateway::access_set_hosted_ceiling_response_value(params, &actor)
                    } else {
                        crate::web_gateway::access_set_tier_response_value(params, &actor)
                    };
                    match result {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "api_fleet_cert_request" => {
                    // Optional explicit addresses; default = every
                    // routable local address (the LAN is the point).
                    let addresses: Vec<String> = params
                        .as_ref()
                        .and_then(|p| p.get("addresses"))
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .filter(|list: &Vec<String>| !list.is_empty())
                        .unwrap_or_else(crate::fleet_cert::default_publish_addresses);
                    if crate::fleet_cert::status_snapshot().name.is_none() {
                        return Some(dashboard_control_error_response(
                            id,
                            "this daemon has no fleet name — enable Connect against a \
                             rendezvous with fleet DNS and let it register first",
                        ));
                    }
                    let spawned_addresses = addresses.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            crate::fleet_cert::request_certificate(spawned_addresses).await
                        {
                            eprintln!("[fleet-cert] request failed: {error}");
                        }
                    });
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": { "started": true, "addresses": addresses },
                    }))
                }
                "api_access_org_trust"
                | "api_access_org_revoke"
                | "api_access_org_issue"
                | "api_access_org_present"
                | "api_access_org_revoke_member"
                | "api_access_org_issuer_init"
                | "api_access_org_issuer_delegate"
                | "api_access_org_issuer_install"
                | "api_access_org_orl"
                | "api_access_org_orl_apply"
                | "api_access_org_renew" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    let result = match method {
                        "api_access_org_trust" => {
                            crate::web_gateway::access_org_trust_response_value(params)
                        }
                        "api_access_org_revoke" => {
                            crate::web_gateway::access_org_revoke_response_value(params)
                        }
                        "api_access_org_issue" => {
                            crate::web_gateway::access_org_issue_response_value(params)
                        }
                        "api_access_org_revoke_member" => {
                            crate::web_gateway::access_org_revoke_member_response_value(params)
                        }
                        "api_access_org_issuer_init" => {
                            crate::web_gateway::access_org_issuer_init_response_value(params)
                        }
                        "api_access_org_issuer_delegate" => {
                            crate::web_gateway::access_org_issuer_delegate_response_value(params)
                        }
                        "api_access_org_issuer_install" => {
                            crate::web_gateway::access_org_issuer_install_response_value(params)
                        }
                        "api_access_org_orl" => crate::web_gateway::access_org_orl_response_value(
                            params.get("handle").and_then(|v| v.as_str()).unwrap_or(""),
                        ),
                        "api_access_org_orl_apply" => {
                            crate::web_gateway::access_org_orl_apply_response_value(params)
                        }
                        "api_access_org_renew" => {
                            crate::web_gateway::access_org_renew_response_value(params)
                        }
                        _ => crate::web_gateway::access_org_present_response_value(
                            params,
                            &runtime.agent_card,
                        ),
                    };
                    match result {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "api_access_iam_upsert_user_client_grant" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    match crate::web_gateway::access_iam_upsert_user_client_grant_response_value(
                        params,
                        &runtime.grant.access_principal(),
                    ) {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "api_access_iam_update_grant" => {
                    let params = params.unwrap_or_else(|| serde_json::json!({}));
                    match crate::web_gateway::access_iam_update_grant_response_value(
                        params,
                        &runtime.grant.access_principal(),
                    ) {
                        Ok(result) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": true,
                            "result": result,
                        })),
                        Err(error) => Some(serde_json::json!({
                            "t": "response",
                            "id": id,
                            "ok": false,
                            "error": error,
                        })),
                    }
                }
                "subscribe_events" => {
                    runtime.events_subscribed = true;
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": {
                            "subscribed": true,
                        },
                    }))
                }
                "unsubscribe_events" => {
                    runtime.events_subscribed = false;
                    Some(serde_json::json!({
                        "t": "response",
                        "id": id,
                        "ok": true,
                        "result": {
                            "subscribed": false,
                        },
                    }))
                }
                "config" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": runtime.config,
                })),
                "api_agent_card" => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": true,
                    "result": runtime.agent_card,
                })),
                "api_cached_bootstrap_events" => Some(cached_bootstrap_events_response_frame(
                    id,
                    &runtime.bootstrap_caches,
                )),
                "api_sessions_stream" => {
                    spawn_control_stream(
                        id,
                        method.to_string(),
                        params,
                        task_tx.clone(),
                        pending_requests,
                    );
                    None
                }
                "api_sessions"
                | "api_session_detail"
                | "api_session_report"
                | "api_session_delete"
                | "api_session_agent_output"
                | "api_session_current_agent_output"
                | "api_session_current_history"
                | "api_session_current_rollback"
                | "api_session_current_redo"
                | "api_session_current_prune"
                | "api_session_current_changes"
                | "api_session_context_snapshot"
                | "api_session_current_uploads"
                | "api_session_current_upload_raw"
                | "api_session_current_upload_delete"
                | "api_transfer_jobs"
                | "api_transfer_job_create"
                | "api_transfer_job_delete"
                | "api_transfer_download_read"
                | "api_transfer_upload_commit"
                | "api_media_clip_start"
                | "api_media_clip_end"
                | "api_media_clip_cancel"
                | "api_fs_stat"
                | "api_fs_list"
                | "api_fs_mkdir"
                | "api_fs_rename"
                | "api_fs_delete"
                | "api_fs_read"
                | "api_sessions_search"
                | "api_settings"
                | "api_settings_save"
                | "api_control_msg"
                | "api_session_control_msg"
                | "api_dashboard_action_msg"
                | "api_diagnostics_visual_freshness"
                | "api_key_status"
                | "api_api_keys_save"
                | "api_external_agents"
                | "api_voice_session"
                | "api_project_root"
                | "api_displays"
                | "api_recordings"
                | "api_recording_asset"
                | "api_session_recordings"
                | "api_session_recording_asset"
                | "api_session_frame_asset"
                | "api_browser_workspace_snapshot"
                | "api_state_snapshot"
                | "api_display_bootstrap"
                | "api_display_webrtc_signal"
                | "api_display_input_authority_snapshot"
                | "api_display_input_authority_request"
                | "api_display_input_authority_release"
                | "api_session_log_replay"
                | "api_external_session_activity_replay"
                | "api_dashboard_bootstrap"
                | "api_worktrees"
                | "api_worktrees_inspect"
                | "api_worktrees_scan"
                | "api_worktrees_remove"
                | "api_managed_context_records"
                | "api_managed_context_anchors"
                | "api_managed_context_fission"
                | "api_mcp_tool_call"
                | "api_peer_add"
                | "api_peer_remove"
                | "api_peer_eligible"
                | "api_peer_message"
                | "api_peer_task"
                | "api_peer_approval"
                | "api_peer_webrtc_signal"
                | "api_peer_file_transfer_signal"
                | "api_peer_dashboard_control_signal"
                | "api_peer_pairing_invite"
                | "api_peer_pairing_join"
                | "api_peer_pairing_request_access"
                | "api_peer_pairing_request_access_poll"
                | "api_peer_pairing_requests"
                | "api_peer_pairing_request_decision"
                | "api_peer_pairing_identities"
                | "api_peer_pairing_identity_revoke"
                | "api_credential_egress_probe"
                | "api_access_connect_unclaim"
                | "api_coordinator_route" => {
                    spawn_control_request(
                        id,
                        method.to_string(),
                        params,
                        runtime.clone(),
                        task_tx.clone(),
                        pending_requests,
                    );
                    None
                }
                _ => Some(serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("unknown method: {method}"),
                })),
            }
        }
        "cancel" => {
            let pending_existed = pending_requests
                .remove(&id)
                .map(|token| {
                    token.cancel();
                    true
                })
                .unwrap_or(false);
            let queued_existed = outbound_queue.cancel(&id);
            let upload_existed = inbound_uploads.remove(&id).is_some();
            let existed = pending_existed || queued_existed || upload_existed;
            Some(cancelled_control_response(id, existed))
        }
        "credit" => {
            let chunks = parsed
                .get("chunks")
                .and_then(|value| value.as_u64())
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            let chunk_id = parsed.get("chunk_id").and_then(|value| value.as_str());
            outbound_queue.grant_credit(&id, chunk_id, chunks);
            None
        }
        _ => Some(serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("unknown frame type: {t}"),
        })),
    }
}

pub(crate) fn control_upload_error_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> serde_json::Value {
    http_body_response(
        id,
        status,
        serde_json::json!({
            "ok": false,
            "error": error.into(),
        })
        .to_string(),
        "dashboard upload",
    )
}

pub(crate) fn control_upload_start_frame(
    id: String,
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    if id.is_empty() {
        return Some(control_upload_error_response(id, 400, "missing request id"));
    }
    let method = frame.get("method").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(
        method,
        "api_session_current_upload"
            | "api_transfer_upload_chunk"
            | "api_fs_write"
            | "api_media_annotation_attach"
            | "api_media_annotation_submit"
            | "api_media_clip_frame"
            | "api_presence_video_frame"
    ) {
        return Some(control_upload_error_response(
            id,
            400,
            format!("unknown upload method: {method}"),
        ));
    }
    if let Err(error) = authorize_dashboard_control_upload(runtime, method) {
        return Some(control_upload_error_response(id, 403, error));
    }
    let total_bytes = frame
        .get("total_bytes")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let expected_chunks = frame
        .get("chunks")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    if total_bytes > crate::web_gateway::UPLOAD_MAX_BYTES {
        return Some(control_upload_error_response(
            id,
            413,
            format!(
                "body too large: {} bytes (cap is {})",
                total_bytes,
                crate::web_gateway::UPLOAD_MAX_BYTES
            ),
        ));
    }
    if total_bytes > 0 && expected_chunks == 0 {
        return Some(control_upload_error_response(
            id,
            400,
            "missing upload chunks",
        ));
    }
    if total_bytes == 0 && expected_chunks != 0 {
        return Some(control_upload_error_response(
            id,
            400,
            "empty upload declared chunks",
        ));
    }
    let tmp = match tempfile::NamedTempFile::new() {
        Ok(tmp) => tmp,
        Err(e) => {
            return Some(control_upload_error_response(
                id,
                500,
                format!("create tempfile: {e}"),
            ));
        }
    };
    if let Some(previous) = pending_requests.remove(&id) {
        previous.cancel();
    }
    inbound_uploads.remove(&id);
    pending_requests.insert(id.clone(), CancellationToken::new());
    inbound_uploads.insert(
        id,
        InboundUploadState {
            method: method.to_string(),
            params: frame
                .get("params")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
            tmp,
            total_bytes,
            expected_chunks,
            next_seq: 0,
            received_bytes: 0,
        },
    );
    None
}

pub(crate) fn control_upload_chunk_frame(
    id: String,
    frame: serde_json::Value,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    let Some(upload) = inbound_uploads.get_mut(&id) else {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "unknown upload id"));
    };
    let seq = frame
        .get("seq")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if seq != upload.next_seq {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            400,
            "upload chunk sequence mismatch",
        ));
    }
    let data = match frame.get("data").and_then(|value| value.as_str()) {
        Some(data) => data,
        None => {
            inbound_uploads.remove(&id);
            pending_requests.remove(&id);
            return Some(control_upload_error_response(
                id,
                400,
                "missing upload chunk data",
            ));
        }
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(_) => {
            inbound_uploads.remove(&id);
            pending_requests.remove(&id);
            return Some(control_upload_error_response(
                id,
                400,
                "invalid upload chunk data",
            ));
        }
    };
    upload.received_bytes = upload.received_bytes.saturating_add(bytes.len());
    if upload.received_bytes > upload.total_bytes {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            400,
            "upload exceeded declared size",
        ));
    }
    if let Err(e) = upload.tmp.as_file_mut().write_all(&bytes) {
        inbound_uploads.remove(&id);
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            500,
            format!("write upload tempfile: {e}"),
        ));
    }
    upload.next_seq = upload.next_seq.saturating_add(1);
    None
}

pub(crate) fn control_upload_end_frame(
    id: String,
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
) -> Option<serde_json::Value> {
    let Some(mut upload) = inbound_uploads.remove(&id) else {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "unknown upload id"));
    };
    let final_chunks = frame
        .get("chunks")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if final_chunks != upload.expected_chunks
        || upload.next_seq != upload.expected_chunks
        || upload.received_bytes != upload.total_bytes
    {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(id, 400, "incomplete upload"));
    }
    if let Err(e) = upload.tmp.as_file_mut().flush() {
        pending_requests.remove(&id);
        return Some(control_upload_error_response(
            id,
            500,
            format!("flush upload tempfile: {e}"),
        ));
    }
    let runtime = runtime.clone();
    let task_tx = task_tx.clone();
    tokio::spawn(async move {
        let response = match upload.method.as_str() {
            "api_session_current_upload" => {
                api_session_current_upload_task_response(id.clone(), upload, runtime).await
            }
            "api_transfer_upload_chunk" => {
                api_transfer_upload_chunk_task_response(id.clone(), upload, runtime).await
            }
            "api_fs_write" => api_fs_write_upload_task_response(id.clone(), upload, runtime).await,
            "api_media_annotation_attach" => {
                api_media_annotation_upload_task_response(id.clone(), upload, runtime, false).await
            }
            "api_media_annotation_submit" => {
                api_media_annotation_upload_task_response(id.clone(), upload, runtime, true).await
            }
            "api_media_clip_frame" => {
                api_media_clip_frame_upload_task_response(id.clone(), upload, runtime).await
            }
            "api_presence_video_frame" => {
                api_presence_video_frame_upload_task_response(id.clone(), upload, runtime).await
            }
            method => ControlTaskResponse {
                id: id.clone(),
                frame: control_upload_error_response(
                    id.clone(),
                    400,
                    format!("unknown upload method: {method}"),
                ),
                byte_stream: None,
                done: true,
            },
        };
        let _ = task_tx.send(response).await;
    });
    None
}

pub(crate) fn terminal_frame_key(frame: &serde_json::Value) -> (String, String) {
    let host_id = frame
        .get("host_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("local")
        .to_string();
    let terminal_id = frame
        .get("terminal_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("shell-0")
        .to_string();
    (host_id, terminal_id)
}

pub(crate) fn terminal_frame_dimension(frame: &serde_json::Value, key: &str, default: u16) -> u16 {
    frame
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(crate) fn control_terminal_open_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    let forwarder_key = (host_id.clone(), terminal_id.clone());
    if let Some(handle) = terminal_forwarders.remove(&forwarder_key) {
        handle.abort();
    }
    let registry = runtime.terminal_registry.clone();
    let terminal_events_tx = terminal_events_tx.clone();
    // Attach needs only the terminal.view floor already enforced by the
    // frame table; creating a shell needs shell.spawn, decided at frame
    // time so expiry mid-connection is honored. A grant-level fs scope
    // makes the new shell a sandboxed one.
    let actor = runtime.grant.terminal_actor();
    let spawn_policy = crate::terminal::ShellSpawnPolicy {
        may_spawn: runtime_operation_decision(
            runtime,
            crate::peer::access_policy::PeerOperation::ShellSpawn,
        )
        .allowed,
        shared: frame
            .get("shared")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        scope: runtime.grant.filesystem().cloned(),
    };
    let handle = tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id: host_id.clone(),
            terminal_id: terminal_id.clone(),
        };
        match registry
            .open_or_attach(key, cols, rows, &actor, spawn_policy)
            .await
        {
            Ok((session, _created)) => {
                let (tx, mut rx) = mpsc::unbounded_channel();
                session.attach(tx);
                let _ = terminal_events_tx.send(serde_json::json!({
                    "t": "terminal_opened",
                    "host_id": host_id.clone(),
                    "terminal_id": terminal_id.clone(),
                    "shared": session.shared(),
                    "can_share": session.managed_by(&actor),
                }));
                while let Some(event) = rx.recv().await {
                    let frame = match event {
                        crate::terminal::TerminalEvent::Output(bytes) => {
                            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            serde_json::json!({
                                "t": "terminal_output",
                                "host_id": host_id.clone(),
                                "terminal_id": terminal_id.clone(),
                                "data": data,
                            })
                        }
                        crate::terminal::TerminalEvent::Exited { status } => {
                            serde_json::json!({
                                "t": "terminal_exited",
                                "host_id": host_id.clone(),
                                "terminal_id": terminal_id.clone(),
                                "status": status,
                            })
                        }
                    };
                    if terminal_events_tx.send(frame).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = terminal_events_tx.send(serde_json::json!({
                    "t": "terminal_error",
                    "host_id": host_id,
                    "terminal_id": terminal_id,
                    "error": e.to_string(),
                }));
            }
        }
    });
    terminal_forwarders.insert(forwarder_key, handle);
    None
}

pub(crate) fn control_terminal_input_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let data_b64 = frame
        .get("data")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
        return None;
    };
    let registry = runtime.terminal_registry.clone();
    let actor = runtime.grant.terminal_actor();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        if let Some(session) = registry.get_visible(&key, &actor).await {
            session.write_input(&data);
        }
    });
    None
}

pub(crate) fn control_terminal_resize_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let cols = terminal_frame_dimension(&frame, "cols", 80);
    let rows = terminal_frame_dimension(&frame, "rows", 24);
    let registry = runtime.terminal_registry.clone();
    let actor = runtime.grant.terminal_actor();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        if let Some(session) = registry.get_visible(&key, &actor).await {
            session.resize(cols, rows);
        }
    });
    None
}

pub(crate) fn control_terminal_close_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    if let Some(handle) = terminal_forwarders.remove(&(host_id.clone(), terminal_id.clone())) {
        handle.abort();
    }
    let registry = runtime.terminal_registry.clone();
    let actor = runtime.grant.terminal_actor();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id,
            terminal_id,
        };
        registry.close_visible(&key, &actor).await;
    });
    None
}

pub(crate) fn control_terminal_share_frame(
    frame: serde_json::Value,
    runtime: &ControlRuntime,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
) -> Option<serde_json::Value> {
    let (host_id, terminal_id) = terminal_frame_key(&frame);
    let shared = frame
        .get("shared")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let registry = runtime.terminal_registry.clone();
    let actor = runtime.grant.terminal_actor();
    let terminal_events_tx = terminal_events_tx.clone();
    tokio::spawn(async move {
        let key = crate::terminal::TerminalKey {
            host_id: host_id.clone(),
            terminal_id: terminal_id.clone(),
        };
        let msg = match registry.set_shared(&key, &actor, shared).await {
            Some(state) => serde_json::json!({
                "t": "terminal_shared",
                "host_id": host_id,
                "terminal_id": terminal_id,
                "shared": state,
            }),
            None => serde_json::json!({
                "t": "terminal_error",
                "host_id": host_id,
                "terminal_id": terminal_id,
                "error": "not allowed: only the session owner or root can change sharing",
            }),
        };
        let _ = terminal_events_tx.send(msg);
    });
    None
}

pub(crate) fn control_presence_frame(
    frame: serde_json::Value,
    runtime: ControlRuntime,
) -> Option<serde_json::Value> {
    let id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let payload = frame
        .get("frame")
        .or_else(|| frame.get("payload"))
        .cloned()
        .unwrap_or(frame);
    tokio::spawn(async move {
        handle_dashboard_presence_frame(payload, runtime).await;
    });
    if id.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "t": "presence_ack",
            "id": id,
            "ok": true,
        }))
    }
}

pub(crate) async fn handle_dashboard_presence_frame(frame: serde_json::Value, runtime: ControlRuntime) {
    let frame_type = frame
        .get("t")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match frame_type {
        "presence_connect" => dashboard_presence_connect(frame, runtime).await,
        "presence_disconnect" => dashboard_presence_disconnect(runtime).await,
        "make_active" => dashboard_make_active(frame, runtime).await,
        "voice_log" => dashboard_voice_log(frame, runtime).await,
        "presence_checkpoint" => dashboard_presence_checkpoint(frame, runtime).await,
        "voice_diagnostic" => dashboard_voice_diagnostic(frame, runtime).await,
        "live_usage_update" => dashboard_live_usage_update(frame, runtime).await,
        "tool_request" => dashboard_tool_request(frame, runtime).await,
        "async_query" => dashboard_async_query(frame, runtime).await,
        _ => {
            eprintln!("[dashboard/control] ignored unsupported presence frame: {frame_type}");
        }
    }
}

pub(crate) fn dashboard_control_emit_browser_event(runtime: &ControlRuntime, payload: serde_json::Value) {
    if let Some(tx) = &runtime.control_frames_tx {
        let _ = tx.send(serde_json::json!({
            "t": "event",
            "payload": payload,
        }));
    }
}

pub(crate) async fn dashboard_presence_connect(frame: serde_json::Value, runtime: ControlRuntime) {
    let server_session_id = frame
        .get("server_session_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let last_event_seq = frame
        .get("last_event_seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let provider = frame
        .get("provider")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("provider")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("model")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let passive = frame
        .get("passive")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if let (Some(bridge), Some(control_tx)) =
        (runtime.presence.as_ref(), runtime.control_frames_tx.clone())
    {
        bridge
            .connect(DashboardPresenceConnectRequest {
                session_id: runtime.session_id.clone(),
                control_tx,
                server_session_id,
                last_event_seq,
                provider,
                model,
                passive,
            })
            .await;
        return;
    }

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(ctx) = &query_ctx {
        let conversation_ctx = crate::presence::build_conversation_context(&ctx.log_dir, 20);
        if let Some(ps) = &ctx.presence_session {
            let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
            session.set_connected(true);
            let state = ctx
                .agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let welcome = session.build_welcome(last_event_seq, &state);
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_welcome",
                    "session_id": welcome.session_id,
                    "state": welcome.state,
                    "events": welcome.events,
                    "last_checkpoint_summary": welcome.last_checkpoint_summary,
                    "current_seq": welcome.current_seq,
                    "is_active": true,
                    "conversation_context": conversation_ctx,
                }),
            );
        } else {
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_welcome",
                    "is_active": true,
                    "conversation_context": conversation_ctx,
                }),
            );
        }
    } else {
        dashboard_control_emit_browser_event(
            &runtime,
            serde_json::json!({
                "t": "presence_welcome",
                "is_active": true,
            }),
        );
    }

    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_connected(provider.as_deref(), model.as_deref());
        }
    }
    runtime.bus.send(AppEvent::PresenceConnected {
        server_session_id,
        last_event_seq,
        live_provider: provider,
        live_model: model,
    });
}

pub(crate) async fn dashboard_presence_disconnect(runtime: ControlRuntime) {
    if let Some(bridge) = runtime.presence.as_ref() {
        bridge
            .disconnect(DashboardPresenceDisconnectRequest {
                session_id: runtime.session_id.clone(),
            })
            .await;
        return;
    }

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_disconnected();
        }
    }
    runtime.bus.send(AppEvent::PresenceDisconnected);
}

pub(crate) async fn dashboard_make_active(frame: serde_json::Value, runtime: ControlRuntime) {
    let provider = frame
        .get("provider")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("provider")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            runtime
                .config
                .get("model")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    if let (Some(bridge), Some(control_tx)) =
        (runtime.presence.as_ref(), runtime.control_frames_tx.clone())
    {
        bridge
            .make_active(DashboardPresenceMakeActiveRequest {
                session_id: runtime.session_id.clone(),
                control_tx,
                provider,
                model,
            })
            .await;
        return;
    }
    dashboard_control_emit_browser_event(
        &runtime,
        serde_json::json!({
            "t": "active_granted",
            "is_active": true,
            "handover_context": "",
            "conversation_context": null,
        }),
    );
}

pub(crate) async fn dashboard_voice_log(frame: serde_json::Value, runtime: ControlRuntime) {
    let text = frame
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let seq = frame
        .get("seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let tool_context = frame
        .get("tool_context")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if let Some(bridge) = runtime.presence.as_ref() {
        bridge.record_voice_log(text.clone());
    }
    let active = runtime.shared_session.read().await;
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_log(&text, seq, tool_context.as_deref());
        }
    }
    runtime.bus.send(AppEvent::VoiceLog {
        text,
        seq,
        tool_context,
    });
}

pub(crate) async fn dashboard_presence_checkpoint(frame: serde_json::Value, runtime: ControlRuntime) {
    let summary = frame
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let last_event_seq = frame
        .get("last_event_seq")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            let checkpoint = presence_core::PresenceCheckpoint {
                summary: summary.clone(),
                last_event_seq,
            };
            let ack = ps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_checkpoint(checkpoint);
            dashboard_control_emit_browser_event(
                &runtime,
                serde_json::json!({
                    "t": "presence_checkpoint_ack",
                    "seq": ack.seq,
                }),
            );
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_checkpoint(&summary, last_event_seq);
        }
    }
    runtime.bus.send(AppEvent::PresenceCheckpointReceived {
        summary,
        last_event_seq,
    });
}

pub(crate) async fn dashboard_voice_diagnostic(frame: serde_json::Value, runtime: ControlRuntime) {
    let kind = frame
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let detail = frame
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let active = runtime.shared_session.read().await;
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(&kind, &detail);
        }
    }
    runtime.bus.send(AppEvent::VoiceDiagnostic { kind, detail });
}

pub(crate) fn json_u64(frame: &serde_json::Value, key: &str) -> u64 {
    frame.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
}

pub(crate) async fn dashboard_live_usage_update(frame: serde_json::Value, runtime: ControlRuntime) {
    runtime.bus.send(AppEvent::LiveUsageUpdate {
        provider: frame
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        model: frame
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        input_tokens: json_u64(&frame, "input_tokens"),
        output_tokens: json_u64(&frame, "output_tokens"),
        cached_tokens: json_u64(&frame, "cached_tokens"),
        total_tokens: json_u64(&frame, "total_tokens"),
        thinking_tokens: json_u64(&frame, "thinking_tokens"),
        input_text_tokens: json_u64(&frame, "input_text_tokens"),
        input_audio_tokens: json_u64(&frame, "input_audio_tokens"),
        input_image_tokens: json_u64(&frame, "input_image_tokens"),
        cached_text_tokens: json_u64(&frame, "cached_text_tokens"),
        cached_audio_tokens: json_u64(&frame, "cached_audio_tokens"),
        cached_image_tokens: json_u64(&frame, "cached_image_tokens"),
        output_text_tokens: json_u64(&frame, "output_text_tokens"),
        output_audio_tokens: json_u64(&frame, "output_audio_tokens"),
    });
}

pub(crate) fn dashboard_preview_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", truncate_str(s, max))
    }
}

pub(crate) fn dashboard_tool_result_frame(
    kind: &str,
    req_id: String,
    tool: Option<String>,
    query_result: crate::presence::ToolQueryResult,
) -> serde_json::Value {
    let mut response = serde_json::json!({
        "t": kind,
        "id": req_id,
        "result": query_result.text,
    });
    if let Some(tool) = tool {
        response["tool"] = serde_json::Value::String(tool);
    }
    if !query_result.images.is_empty() {
        let images = query_result
            .images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "mime_type": img.media_type,
                    "data": img.data,
                })
            })
            .collect();
        response["images"] = serde_json::Value::Array(images);
    }
    response
}

pub(crate) async fn dashboard_tool_request(frame: serde_json::Value, runtime: ControlRuntime) {
    let req_id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let tool = frame
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let args = frame
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    let args_preview = serde_json::to_string(&args)
        .map(|s| dashboard_preview_text(&s, 200))
        .unwrap_or_default();
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[tool_request] {}({})", tool, args_preview),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let frame_registry = active.frame_registry.clone();
    drop(active);

    let state = query_ctx
        .as_ref()
        .map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    let action = crate::presence::dispatch_tool_call(&tool, &args, &state);

    let query_result = if let crate::presence::PresenceAction::SubmitTask(envelope) = action {
        let msg = format!("Task submitted: {}", envelope.task);
        if let Some(tx) = runtime.task_tx.as_ref() {
            let _ = tx.send(envelope).await;
        } else {
            let ctrl_action = crate::presence::PresenceAction::SubmitTask(envelope);
            if let Some((ctrl, _)) = crate::presence::action_to_control_msg(&ctrl_action) {
                runtime.bus.send(AppEvent::ControlCommand(ctrl));
            }
        }
        crate::presence::ToolQueryResult::text(msg)
    } else if let Some((ctrl, msg)) = crate::presence::action_to_control_msg(&action) {
        runtime.bus.send(AppEvent::ControlCommand(ctrl));
        crate::presence::ToolQueryResult::text(msg)
    } else {
        match action {
            crate::presence::PresenceAction::TextResult(text) => {
                crate::presence::ToolQueryResult::text(text)
            }
            crate::presence::PresenceAction::NeedsIO {
                tool_name,
                args: io_args,
            } => {
                if let Some(ctx) = query_ctx.as_ref() {
                    crate::presence::handle_tool_query(
                        &ctx.agent_state,
                        &ctx.project_root,
                        &ctx.log_dir,
                        &ctx.knowledge_path,
                        &tool_name,
                        &io_args,
                        frame_registry.as_ref(),
                        ctx.context_injection.as_ref(),
                    )
                    .await
                    .unwrap_or_else(|| {
                        crate::presence::ToolQueryResult::text(format!("Unknown tool: {}", tool))
                    })
                } else {
                    crate::presence::ToolQueryResult::text(
                        "Presence query context not available".to_string(),
                    )
                }
            }
            _ => unreachable!(),
        }
    };

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[tool_response] {} -> {}",
            tool,
            dashboard_preview_text(&query_result.text, 200)
        ),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    dashboard_control_emit_browser_event(
        &runtime,
        dashboard_tool_result_frame("tool_response", req_id, None, query_result),
    );
}

pub(crate) async fn dashboard_async_query(frame: serde_json::Value, runtime: ControlRuntime) {
    let req_id = frame
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let tool = frame
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let args = frame
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[async_query] {}", tool),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    let active = runtime.shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let frame_registry = active.frame_registry.clone();
    drop(active);

    let query_result = if let Some(ctx) = query_ctx.as_ref() {
        crate::presence::handle_tool_query(
            &ctx.agent_state,
            &ctx.project_root,
            &ctx.log_dir,
            &ctx.knowledge_path,
            &tool,
            &args,
            frame_registry.as_ref(),
            ctx.context_injection.as_ref(),
        )
        .await
        .unwrap_or_else(|| {
            crate::presence::ToolQueryResult::text(format!("Unknown query tool: {}", tool))
        })
    } else {
        crate::presence::ToolQueryResult::text("Presence query context not available".to_string())
    };

    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[async_query_result] {} -> {}",
            tool,
            dashboard_preview_text(&query_result.text, 200)
        ),
        level: Some(LogLevel::Debug),
        turn: None,
    });

    dashboard_control_emit_browser_event(
        &runtime,
        dashboard_tool_result_frame("async_query_result", req_id, Some(tool), query_result),
    );
}

pub(crate) fn spawn_dashboard_display_input(frame: serde_json::Value, runtime: ControlRuntime) {
    tokio::spawn(async move {
        let display_id = frame
            .get("display_id")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let Some(event) = frame.get("event").cloned() else {
            return;
        };
        let Ok(input_event) = serde_json::from_value::<crate::display::InputEvent>(event) else {
            return;
        };
        let Some(bridge) = runtime.display_authority.as_ref() else {
            return;
        };
        if !bridge.input_authorized(&runtime.session_id, display_id) {
            return;
        }
        let session_registry = {
            let session = runtime.shared_session.read().await;
            session.session_registry.clone()
        };
        let Some(session_registry) = session_registry else {
            return;
        };
        let display_session = {
            let registry = session_registry.read().await;
            registry.get(display_id)
        };
        if let Some(display_session) = display_session {
            if let Err(e) = display_session.inject_input(input_event).await {
                eprintln!("[dashboard/control] display input injection failed: {e}");
            }
        }
    });
}

pub(crate) fn cached_bootstrap_events_response_frame(
    id: String,
    caches: &DashboardBootstrapCaches,
) -> serde_json::Value {
    let mut events = Vec::new();
    let mut malformed = Vec::new();
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "usage",
        &caches.last_usage_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "live_usage",
        &caches.last_live_usage_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "status",
        &caches.last_status_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "autonomy",
        &caches.last_autonomy_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "external_agent",
        &caches.last_external_agent_json,
    );
    push_cached_bootstrap_event(
        &mut events,
        &mut malformed,
        "user_display",
        &caches.last_user_display_json,
    );
    // Per-session change-detected state (vitals/goal) — same rationale
    // as the singleton caches above, but keyed per session.
    if let Ok(guard) = caches.session_state_lines.lock() {
        for kinds in guard.values() {
            for line in kinds.values() {
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => events.push(v),
                    Err(_) => malformed.push("session_state"),
                }
            }
        }
    }
    let event_count = events.len();

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": {
            "events": events,
            "event_count": event_count,
            "malformed_sources": malformed,
            "omitted": [
                "state_snapshot",
                "browser_workspace_snapshot",
                "display_ready",
                "display_input_authority_state",
                "session_log_replay",
                "external_session_activity_replay"
            ],
        },
    })
}

pub(crate) fn push_cached_bootstrap_event(
    events: &mut Vec<serde_json::Value>,
    malformed: &mut Vec<&'static str>,
    name: &'static str,
    cache: &Arc<std::sync::Mutex<Option<String>>>,
) {
    let Some(line) = cache.lock().ok().and_then(|guard| guard.clone()) else {
        return;
    };
    match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(value) => events.push(value),
        Err(_) => malformed.push(name),
    }
}

pub(crate) fn status_response_frame(id: String, runtime: &ControlRuntime) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    result.insert(
        "protocol".to_string(),
        serde_json::json!(CONTROL_PROTOCOL_VERSION),
    );
    result.insert(
        "session_id".to_string(),
        serde_json::json!(runtime.session_id),
    );
    result.insert(
        "daemon_public_key".to_string(),
        serde_json::json!(runtime.daemon_public_key),
    );
    result.insert(
        "created_unix_ms".to_string(),
        serde_json::json!(runtime.created_unix_ms),
    );
    result.insert("features".to_string(), serde_json::json!(control_features()));
    result.insert(
        "transport".to_string(),
        serde_json::json!("webrtc-datachannel"),
    );
    result.insert(
        "events_subscribed".to_string(),
        serde_json::json!(runtime.events_subscribed),
    );
    result.insert(
        "events_sent".to_string(),
        serde_json::json!(runtime.events_sent),
    );
    result.insert(
        "response_credit_enabled".to_string(),
        serde_json::json!(runtime.response_credit_enabled),
    );
    result.insert(
        "grant_kind".to_string(),
        serde_json::json!(runtime.grant.wire_kind()),
    );
    result.insert(
        "grant_label".to_string(),
        serde_json::json!(runtime.grant.label()),
    );
    if let Some(profile) = runtime.grant.profile() {
        result.insert("grant_profile".to_string(), serde_json::json!(profile));
    }
    let access_principal = runtime.grant.access_principal();
    result.insert("access_principal".to_string(), access_principal.as_value());
    // Whether ANY provider credential is usable (.env key or active lease).
    // A single aggregate boolean — deliberately not per-provider — so every
    // binding can drive the first-run "fuel this daemon" nudge without the
    // settings.manage permission the per-provider api_key_status needs.
    result.insert(
        "fueled".to_string(),
        serde_json::json!(crate::web_gateway::any_provider_credential_usable()),
    );
    result.insert(
        "iam_enforcement".to_string(),
        serde_json::json!({
            "operation_evaluator": true,
            "principal_kind": access_principal.kind,
            "principal_binding": match runtime.grant {
                DashboardControlGrant::TrustedLocal => "root_session",
                DashboardControlGrant::UserClientRoot { .. } => "user_client_root",
                DashboardControlGrant::UserClient { .. } => "user_client",
                DashboardControlGrant::Peer { .. } => "peer_daemon",
            },
            "user_client_grants": matches!(runtime.grant, DashboardControlGrant::UserClient { .. })
        }),
    );

    let peer_registry_available = runtime.peer_registry.is_some();
    let session_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::SessionInspect,
    );
    let session_manage = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::SessionManage,
    );
    let fs_write = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::FilesystemWrite,
    );
    let terminal = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::TerminalView,
    );
    let display_input = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::DisplayInput,
    );
    let runtime_control = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::RuntimeControl,
    );
    let access_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::AccessInspect,
    );
    let access_manage = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::AccessManage,
    );
    let peer_inspect = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::PeerInspect,
    );
    let peer_manage = runtime_allows_operation(
        runtime,
        crate::peer::access_policy::PeerOperation::PeerManage,
    );
    let peer_use =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::PeerUse);
    let message =
        runtime_allows_operation(runtime, crate::peer::access_policy::PeerOperation::Message);

    // Every gated api_* method derives its `<method>_available` boolean from
    // `CONTROL_METHODS`: operation granted && backing subsystem wired
    // (`control_method_runtime_ready`). One boolean per advertised RPC lets
    // the SPA distinguish "denied for this session" from "unsupported
    // daemon" (feature list) without probing calls.
    for spec in CONTROL_METHODS {
        if !spec.name.starts_with("api_") {
            continue;
        }
        let Some(op) = spec.op else { continue };
        let available = runtime_allows_operation(runtime, op)
            && control_method_runtime_ready(runtime, spec.name);
        result.insert(
            format!("{}_available", spec.name),
            serde_json::json!(available),
        );
    }

    // Operation aggregates, composite rollups, and frame-transport
    // availability the SPA reads — none has a single backing method in
    // `CONTROL_METHODS`, so they stay hand-written.
    let capabilities = [
        ("access_inspect_available", access_inspect),
        ("access_manage_available", access_manage),
        ("peer_inspect_available", peer_inspect),
        ("peer_manage_available", peer_manage),
        ("peer_use_available", peer_use),
        // The three display-input-authority methods roll up for the SPA's
        // single input-authority readiness check.
        (
            "api_display_input_authority_available",
            runtime.display_authority.is_some() && display_input,
        ),
        ("byte_streams_available", true),
        // Upload frames authorize per delivered method; the transport is
        // available as soon as any upload-capable operation is granted.
        (
            "upload_frames_available",
            fs_write || session_manage || runtime_control,
        ),
        ("terminal_frames_available", terminal),
        ("presence_frames_available", message),
        (
            "presence_active_handoff_available",
            runtime.presence.is_some() && message,
        ),
        ("presence_tool_request_available", message),
        ("api_media_editor_available", runtime_control),
        ("api_managed_context_available", session_inspect),
        (
            "api_peer_mutations_available",
            peer_registry_available && peer_manage,
        ),
        // message/task/approval act *through* a peer rather than mutating
        // the registry, so they ride peer.use like the signaling relays.
        (
            "api_peer_quick_controls_available",
            peer_registry_available && peer_use,
        ),
        ("api_peer_pairing_available", peer_manage || access_manage),
        (
            "api_coordinator_available",
            peer_registry_available && peer_manage,
        ),
        // Host capability, not a grant: dashboards derive the "New virtual
        // display" affordance from this (Xvfb-based, Linux-only).
        (
            "virtual_displays_available",
            crate::vision::virtual_displays_supported(),
        ),
    ];
    for (name, available) in capabilities {
        result.insert(name.to_string(), serde_json::json!(available));
    }

    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": true,
        "result": serde_json::Value::Object(result),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_control::tests::{runtime, scoped_user_client_grant};

    #[test]
    fn frame_api_response_fails_closed_on_byte_payloads() {
        let response = crate::web_gateway::ApiResponse::Bytes {
            status: 200,
            content_type: "application/octet-stream".to_string(),
            headers: Vec::new(),
            bytes: crate::web_gateway::BytesPayload::InMemory(b"x".to_vec()),
            meta: serde_json::Value::Null,
        };
        let frame = frame_api_response("bad-lane".to_string(), response, "unit probe");
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], false);
        assert!(frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("unexpected byte response"));
    }

    #[test]
    fn dashboard_preview_text_truncates_on_char_boundary() {
        let text = format!("{}{}", "a".repeat(199), "\u{00e9}");
        assert_eq!(
            dashboard_preview_text(&text, 200),
            format!("{}...", "a".repeat(199))
        );
    }

    #[test]
    fn scoped_user_client_grant_limits_dashboard_control_permissions() {
        let mut rt = runtime();
        rt.grant = scoped_user_client_grant();

        assert!(runtime_allows_operation(
            &rt,
            crate::peer::access_policy::PeerOperation::AccessInspect
        ));
        assert!(!runtime_allows_operation(
            &rt,
            crate::peer::access_policy::PeerOperation::AccessManage
        ));

        let status = status_response_frame("s1".to_string(), &rt);
        assert_eq!(status["t"], "response");
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["grant_kind"], "user-client");
        assert_eq!(
            status["result"]["access_principal"]["kind"],
            "browser_certificate"
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["principal_binding"],
            "user_client"
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["user_client_grants"],
            true
        );
        assert_eq!(status["result"]["access_inspect_available"], true);
        assert_eq!(status["result"]["access_manage_available"], false);
        assert_eq!(
            status["result"]["api_access_iam_update_grant_available"],
            false
        );
    }

    #[test]
    fn status_advertises_an_availability_boolean_for_every_gated_api_method() {
        let rt = runtime();
        let status = status_response_frame("s1".to_string(), &rt);
        for spec in CONTROL_METHODS {
            if !spec.name.starts_with("api_") || spec.op.is_none() {
                continue;
            }
            let key = format!("{}_available", spec.name);
            assert!(
                status["result"][&key].is_boolean(),
                "status result missing {key}"
            );
        }
    }

    #[test]
    fn user_client_root_grant_reports_identity_without_scoping_permissions() {
        let mut rt = runtime();
        rt.grant = DashboardControlGrant::UserClientRoot {
            principal: crate::access::iam::AccessPrincipal::root_user_client(
                "browser-mtls",
                "webrtc-datachannel",
                "Browser certificate ab123",
                None,
                None,
                vec![serde_json::json!({
                    "kind": "browser_mtls_cert",
                    "fingerprint": "ab123"
                })],
            ),
        };

        let status = status_response_frame("s1".to_string(), &rt);
        assert_eq!(status["result"]["grant_kind"], "user-client-root");
        assert_eq!(status["result"]["access_principal"]["kind"], "root_session");
        assert_eq!(
            status["result"]["access_principal"]["authn"][0]["kind"],
            "browser_mtls_cert"
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["principal_binding"],
            "user_client_root"
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["user_client_grants"],
            false
        );
        assert_eq!(status["result"]["access_manage_available"], true);
    }

    #[test]
    fn upload_start_authorizes_by_delivered_method_not_blanket_fs_write() {
        let file_operator = ControlRuntime {
            grant: DashboardControlGrant::Peer {
                fingerprint: "aabbccdd".into(),
                label: "peer".into(),
                profile: "file-operator".into(),
                filesystem: Default::default(),
            },
            ..runtime()
        };
        let mut pending = HashMap::new();
        let mut inbound_uploads = HashMap::new();

        // A filesystem-write grant must not reach runtime-control surface
        // (media annotations inject content into the session).
        let denied = control_upload_start_frame(
            "up-media".into(),
            serde_json::json!({
                "t": "upload_start",
                "id": "up-media",
                "method": "api_media_annotation_submit",
                "total_bytes": 4,
                "chunks": 1,
            }),
            &file_operator,
            &mut pending,
            &mut inbound_uploads,
        )
        .expect("denied uploads answer immediately");
        assert_eq!(denied["result"]["_httpStatus"], 403);
        assert!(
            denied["result"]["error"]
                .as_str()
                .unwrap()
                .contains("not allowed"),
            "{denied}"
        );
        assert!(inbound_uploads.is_empty());

        // The same grant covers what it actually names: transfer chunks.
        assert!(control_upload_start_frame(
            "up-chunk".into(),
            serde_json::json!({
                "t": "upload_start",
                "id": "up-chunk",
                "method": "api_transfer_upload_chunk",
                "total_bytes": 4,
                "chunks": 1,
            }),
            &file_operator,
            &mut pending,
            &mut inbound_uploads,
        )
        .is_none());
        assert!(inbound_uploads.contains_key("up-chunk"));

        // api_presence_video_frame is a first-class upload method (it has
        // an upload-end handler); runtime control admits it at start.
        let admin = ControlRuntime {
            grant: DashboardControlGrant::Peer {
                fingerprint: "aabbccdd".into(),
                label: "peer".into(),
                profile: "admin".into(),
                filesystem: Default::default(),
            },
            ..runtime()
        };
        assert!(control_upload_start_frame(
            "up-video".into(),
            serde_json::json!({
                "t": "upload_start",
                "id": "up-video",
                "method": "api_presence_video_frame",
                "total_bytes": 4,
                "chunks": 1,
            }),
            &admin,
            &mut pending,
            &mut inbound_uploads,
        )
        .is_none());
        assert!(inbound_uploads.contains_key("up-video"));
    }

    #[tokio::test]
    async fn upload_frames_commit_pending_upload() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let bytes = b"hello upload";
        let first = &bytes[..6];
        let second = &bytes[6..];

        let start = serde_json::json!({
            "t": "upload_start",
            "id": "up1",
            "method": "api_session_current_upload",
            "params": {
                "name": "note.txt",
                "mime": "text/plain",
                "destination": "task",
            },
            "encoding": "base64",
            "total_bytes": bytes.len(),
            "chunks": 2,
        });
        assert!(control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());
        assert!(pending.contains_key("up1"));

        for (seq, chunk) in [first, second].into_iter().enumerate() {
            let frame = serde_json::json!({
                "t": "upload_chunk",
                "id": "up1",
                "seq": seq,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            });
            assert!(control_frame_response(
                &frame.to_string(),
                &mut rt,
                &tx,
                &mut pending,
                &mut outbound,
                &mut inbound_uploads,
                &terminal_tx,
                &mut terminal_forwarders,
            )
            .is_none());
        }

        let end = serde_json::json!({
            "t": "upload_end",
            "id": "up1",
            "chunks": 2,
        });
        assert!(control_frame_response(
            &end.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "up1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["_httpOk"], true);
        assert_eq!(response.frame["result"]["name"], "note.txt");
        assert_eq!(response.frame["result"]["mime"], "text/plain");
        assert_eq!(response.frame["result"]["size"], bytes.len());
        let path = response.frame["result"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            AppEvent::UploadReady { descriptor } => {
                assert_eq!(descriptor.name, "note.txt");
                assert_eq!(descriptor.size, bytes.len() as u64);
            }
            other => panic!("expected upload ready event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_frames_commit_zero_byte_upload() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();

        let start = serde_json::json!({
            "t": "upload_start",
            "id": "up-empty",
            "method": "api_session_current_upload",
            "params": {
                "name": "empty.txt",
                "mime": "text/plain",
                "destination": "task",
            },
            "encoding": "base64",
            "total_bytes": 0,
            "chunks": 0,
        });
        assert!(control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());
        assert!(pending.contains_key("up-empty"));

        let end = serde_json::json!({
            "t": "upload_end",
            "id": "up-empty",
            "chunks": 0,
        });
        assert!(control_frame_response(
            &end.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
        assert_eq!(response.id, "up-empty");
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["_httpOk"], true);
        assert_eq!(response.frame["result"]["name"], "empty.txt");
        assert_eq!(response.frame["result"]["size"], 0);
        let path = response.frame["result"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn terminal_frames_open_input_and_forward_output() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.terminal_registry = Arc::new(crate::terminal::TerminalRegistry::new(
            project.path().to_path_buf(),
        ));
        let (task_tx, _task_rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let terminal_id = "dash-control-test-shell";

        let open = serde_json::json!({
            "t": "terminal_open",
            "host_id": "local",
            "terminal_id": terminal_id,
            "cols": 80,
            "rows": 24,
        });
        assert!(control_frame_response(
            &open.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());

        // Generous budget: the PTY spawn behind terminal_open (PowerShell
        // under ConPTY on a loaded Windows runner, especially) can take
        // tens of seconds before the shell paints; a passing run returns
        // the moment each frame arrives and never waits the budget out.
        let budget = Duration::from_secs(60);
        let opened = tokio::time::timeout(budget, terminal_rx.recv())
            .await
            .expect("no terminal frame within budget after terminal_open")
            .expect("terminal frame channel closed before terminal_opened");
        assert_eq!(opened["t"], "terminal_opened", "got frame: {opened}");
        assert_eq!(opened["terminal_id"], terminal_id);

        // Drain frames until the accumulated decoded output satisfies
        // `until`, panicking loudly — with everything received — on the
        // deadline. Matching runs on the accumulated transcript, not per
        // frame, so output split across chunks still matches.
        async fn drain_output_until(
            terminal_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
            budget: Duration,
            phase: &str,
            until: impl Fn(&str) -> bool,
        ) -> String {
            let deadline = tokio::time::Instant::now() + budget;
            let mut transcript = String::new();
            let mut other_frames: Vec<String> = Vec::new();
            loop {
                match tokio::time::timeout_at(deadline, terminal_rx.recv()).await {
                    Ok(Some(frame)) if frame["t"] == "terminal_output" => {
                        let data = frame["data"].as_str().unwrap_or("");
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .unwrap_or_default();
                        transcript.push_str(&String::from_utf8_lossy(&bytes));
                        if until(&transcript) {
                            return transcript;
                        }
                    }
                    Ok(Some(frame)) => other_frames.push(frame.to_string()),
                    Ok(None) => panic!(
                        "{phase}: terminal frame channel closed; output so far: \
                         {transcript:?}; other frames: {other_frames:?}"
                    ),
                    Err(_) => panic!(
                        "{phase}: no matching terminal output within {budget:?}; \
                         output so far: {transcript:?}; other frames: {other_frames:?}"
                    ),
                }
            }
        }

        // Don't type until the shell has painted something — bytes written
        // during shell startup can be silently discarded (see terminal.rs
        // tests); a dashboard user typing at a rendered prompt never races
        // this.
        drain_output_until(&mut terminal_rx, budget, "shell startup", |t| !t.is_empty()).await;

        let token = "dashboard_terminal_frame_ok";
        let input = serde_json::json!({
            "t": "terminal_input",
            "host_id": "local",
            "terminal_id": terminal_id,
            "data": base64::engine::general_purpose::STANDARD
                .encode(format!("printf '{token}\\n'\r").as_bytes()),
        });
        assert!(control_frame_response(
            &input.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        )
        .is_none());

        drain_output_until(&mut terminal_rx, budget, "token echo", |t| {
            t.contains(token)
        })
        .await;

        let close = serde_json::json!({
            "t": "terminal_close",
            "host_id": "local",
            "terminal_id": terminal_id,
        });
        let _ = control_frame_response(
            &close.to_string(),
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
        );
        for (_, handle) in terminal_forwarders {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn fs_write_upload_frames_flow_end_to_end() {
        use base64::Engine as _;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("uploaded.txt");
        let payload = b"written via upload frames";

        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        let mut frame = |text: &str,
                         rt: &mut ControlRuntime,
                         pending: &mut HashMap<String, CancellationToken>,
                         inbound: &mut HashMap<String, InboundUploadState>|
         -> Option<serde_json::Value> {
            control_frame_response(
                text,
                rt,
                &tx,
                pending,
                &mut outbound,
                inbound,
                &terminal_tx,
                &mut terminal_forwarders,
            )
        };

        // Unknown upload methods are refused at upload_start.
        let refused = frame(
            &serde_json::json!({
                "t": "upload_start",
                "id": "bad1",
                "method": "api_fs_nope",
                "params": {},
                "total_bytes": 1,
                "chunks": 1,
            })
            .to_string(),
            &mut rt,
            &mut pending,
            &mut inbound_uploads,
        )
        .unwrap();
        assert_eq!(refused["result"]["_httpStatus"], 400);
        assert_eq!(refused["result"]["ok"], false);
        assert!(refused["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown upload method"));

        // api_fs_write rides start -> chunk -> end and lands on disk.
        let start = frame(
            &serde_json::json!({
                "t": "upload_start",
                "id": "up1",
                "method": "api_fs_write",
                "params": { "path": target.to_string_lossy(), "create_new": true },
                "encoding": "base64",
                "total_bytes": payload.len(),
                "chunks": 1,
            })
            .to_string(),
            &mut rt,
            &mut pending,
            &mut inbound_uploads,
        );
        assert!(start.is_none());
        assert!(inbound_uploads.contains_key("up1"));

        let chunk = frame(
            &serde_json::json!({
                "t": "upload_chunk",
                "id": "up1",
                "seq": 0,
                "data": base64::engine::general_purpose::STANDARD.encode(payload),
            })
            .to_string(),
            &mut rt,
            &mut pending,
            &mut inbound_uploads,
        );
        assert!(chunk.is_none());

        let end = frame(
            &serde_json::json!({
                "t": "upload_end",
                "id": "up1",
                "chunks": 1,
            })
            .to_string(),
            &mut rt,
            &mut pending,
            &mut inbound_uploads,
        );
        assert!(end.is_none());

        let response = rx.recv().await.unwrap();
        assert_eq!(response.id, "up1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["created"], true);
        assert_eq!(std::fs::read(&target).unwrap(), payload);
    }

    #[tokio::test]
    async fn transfer_download_job_persists_and_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source = dir.path().join("fixture.txt");
        std::fs::write(&source, b"durable download fixture").unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project.clone());

        let status = status_response_frame("transfer-status".to_string(), &rt);
        assert_eq!(status["result"]["api_transfer_jobs_available"], true);
        assert_eq!(status["result"]["api_transfer_job_create_available"], true);
        assert_eq!(
            status["result"]["api_transfer_download_read_available"],
            true
        );

        let create = api_transfer_job_create_response(
            "transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "path": source.to_string_lossy(),
            })),
            &rt,
        )
        .await;
        assert_eq!(create["t"], "response");
        assert_eq!(create["ok"], true);
        assert_eq!(create["result"]["ok"], true);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let resume_token = create["result"]["job"]["resume_token"]
            .as_str()
            .unwrap()
            .to_string();

        let list = api_transfer_jobs_response("transfer-list".to_string(), &rt).await;
        assert_eq!(list["result"]["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(list["result"]["jobs"][0]["id"], job_id);

        let read = api_transfer_download_read_task_response(
            "transfer-read".to_string(),
            Some(&serde_json::json!({
                "resume_token": resume_token,
                "offset": 8,
                "length": 8,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        assert!(read.byte_stream.is_some());
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.id, "transfer-read");
        assert_eq!(stream.stream_id, "transfer-read:transfer-download");
        assert_eq!(stream.content_type, "text/plain; charset=utf-8");
        assert_eq!(stream.filename.as_deref(), Some("fixture.txt"));
        assert_eq!(stream.bytes, b"download");
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["id"], job_id);
        assert_eq!(stream.result["range_start"], 8);
        assert_eq!(stream.result["range_end"], 16);
        assert_eq!(stream.result["resumable"], true);
        assert_eq!(
            stream.result["total_size"].as_u64(),
            Some("durable download fixture".len() as u64)
        );
    }
}
