//! MCP binding for bounded macOS monitor lifecycle/capture and explicit window
//! placement. Ingress keeps
//! the original IAM operations (DisplayInput for lifecycle, DisplayView for
//! reads); shared WindowServer authority is an additional gate here, with
//! daemon-wide recovery inventory restricted further to owner surfaces.

use super::*;
use crate::macos_monitor::{Action, Authority, Inspection, Value, WindowAction, WindowValue};

impl IntendantServer {
    pub(super) async fn macos_window_as_caller(
        &self,
        action: WindowAction,
        caller: ToolCallerTrust,
    ) -> String {
        let authority = self.macos_monitor_authority(caller).await;
        let receipt = match self
            .bus
            .macos_monitors
            .request(Action::Window(action), authority)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => return window_error(error),
        };
        window_response(receipt)
    }

    pub(super) async fn list_macos_monitors_as_caller(&self, caller: ToolCallerTrust) -> String {
        let authority = self.macos_monitor_authority(caller).await;
        match self
            .bus
            .macos_monitors
            .inspect(Inspection::Inventory, authority)
            .await
        {
            Ok(snapshot) => serde_json::json!(snapshot).to_string(),
            Err(error) => serde_json::json!({"ok": false, "error": error}).to_string(),
        }
    }

    pub(super) async fn macos_monitor_status(
        &self,
        selector: String,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        let authority = self.macos_monitor_authority(caller).await;
        Ok(
            match self
                .bus
                .macos_monitors
                .inspect(
                    Inspection::Status {
                        selector: selector.clone(),
                    },
                    authority,
                )
                .await
            {
                Ok(snapshot) => {
                    text_tool_result(readiness_metadata(&snapshot, &selector).to_string())
                }
                Err(error) => text_tool_error(error),
            },
        )
    }

    async fn macos_monitor_authority(&self, caller: ToolCallerTrust) -> Authority {
        Authority {
            owner_surface: caller == ToolCallerTrust::OwnerSurface,
            autonomy: self.state.read().await.autonomy.clone(),
        }
    }

    pub(super) async fn create_macos_monitor(
        &self,
        params: CreateVirtualDisplayParams,
        caller: ToolCallerTrust,
    ) -> String {
        if params.minimum_display_id.is_some() || params.maximum_display_id.is_some() {
            return serde_json::json!({"ok": false, "error": "Xvfb display pool bounds are unsupported for macOS monitors"}).to_string();
        }
        let authority = self.macos_monitor_authority(caller).await;
        let result = self
            .bus
            .macos_monitors
            .request(
                Action::Create {
                    width: params.width.unwrap_or(1920),
                    height: params.height.unwrap_or(1080),
                },
                authority,
            )
            .await;
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(error) => return serde_json::json!({"ok": false, "error": error}).to_string(),
        };
        let Value::Created(monitor) = &receipt.value else {
            return serde_json::json!({"ok": false, "error": "unexpected monitor result"})
                .to_string();
        };
        let mut response = serde_json::json!(monitor.description());
        response["ok"] = true.into();
        response["note"] = "Shares the user's login session; hotplug/removal may rearrange windows. Retain display_id and capture_generation for cleanup; owner surfaces can recover committed handles with list_macos_monitors. Local response construction does not acknowledge transport delivery.".into();
        let response = response.to_string();
        if !receipt.commit() {
            return serde_json::json!({"ok":false,"error":"monitor creation receipt expired; cleanup retained by broker"}).to_string();
        } // no await between commit and returning the response
        response
    }

    pub(super) async fn destroy_macos_monitor(
        &self,
        params: DestroyVirtualDisplayParams,
        caller: ToolCallerTrust,
    ) -> String {
        let authority = self.macos_monitor_authority(caller).await;
        match self
            .bus
            .macos_monitors
            .request(
                Action::Destroy {
                    display_id: params.display_id,
                    selector: params.capture_generation.clone(),
                },
                authority,
            )
            .await
        {
            Ok(receipt) => {
                let response = serde_json::json!({"ok": true, "display_id": params.display_id,
                    "display_target": params.capture_generation, "capture_generation": params.capture_generation,
                    "closed_browser_workspace_ids": []}).to_string();
                if !receipt.commit() {
                    return serde_json::json!({"ok":false,"error":"monitor destruction response expired; destruction may already have applied"}).to_string();
                }
                response
            }
            Err(error) => serde_json::json!({"ok": false, "error": error}).to_string(),
        }
    }

    pub(super) async fn screenshot_macos_monitor(
        &self,
        selector: String,
        compact: bool,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        let authority = self.macos_monitor_authority(caller).await;
        let state = self.state.read().await;
        let directory = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        drop(state);
        let path = directory.join(format!(
            "macos-monitor-{}.png",
            uuid::Uuid::new_v4().simple()
        ));
        let receipt = match self
            .bus
            .macos_monitors
            .request(
                Action::Capture {
                    selector: selector.clone(),
                    path,
                },
                authority,
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => return Ok(text_tool_error(error)),
        };
        let Value::Captured(screenshot) = &receipt.value else {
            return Ok(text_tool_error("unexpected monitor result"));
        };
        let response = screenshot_response(screenshot, &selector, compact);
        // No await after receipt delivery: the broker just rechecked authority,
        // generation and child liveness and retains serialization until commit.
        if !receipt.commit() {
            return Ok(text_tool_error(
                "monitor screenshot receipt expired; artifact discarded",
            ));
        }
        Ok(response)
    }
}

/// Keep the common display_readiness envelope, without probing unrelated
/// native permissions or treating lifecycle metadata as capture/input readiness.
fn readiness_metadata(
    snapshot: &crate::macos_monitor::Snapshot,
    selector: &str,
) -> serde_json::Value {
    use crate::cu_readiness::{
        CuReadiness, LayerStatus, ReadinessLayer, LAYER_ACCESSIBILITY, LAYER_AUTHORITY,
        LAYER_CAPTURE, LAYER_DISPLAY, LAYER_INPUT,
    };
    let mut metadata = snapshot.status();
    let verified = metadata["lifecycle_ready"] == true;
    let broker = metadata["broker_state"].as_str().unwrap_or("unavailable");
    let readiness = CuReadiness {
        target: selector.to_string(),
        ready: false,
        summary: format!(
            "Read-only macOS monitor: lifecycle {}; broker {broker}; capture unverified; input and streaming unavailable",
            if verified { "verified" } else { "not verified" },
        ),
        layers: vec![
            ReadinessLayer {
                layer: LAYER_AUTHORITY, status: LayerStatus::Ready,
                detail: "Existing shared-session display authority was checked for this request".into(),
                fix: None,
            },
            ReadinessLayer {
                layer: LAYER_CAPTURE, status: LayerStatus::Unknown,
                detail: "Inspection does not probe Screen Recording permission or capture a frame".into(),
                fix: Some("Use take_screenshot with this exact generation to validate read-only capture".into()),
            },
            ReadinessLayer {
                layer: LAYER_ACCESSIBILITY, status: LayerStatus::Unknown,
                detail: "Accessibility permission was not probed; AX is unsupported for owned monitor selectors".into(),
                fix: None,
            },
            ReadinessLayer {
                layer: LAYER_DISPLAY,
                status: if verified { LayerStatus::Ready } else { LayerStatus::Unknown },
                detail: if verified {
                    "Exact generation resolved to its retained helper object; this is not capture readiness".into()
                } else {
                    format!("No verified owned generation in broker state {broker}; no display fallback was attempted")
                },
                fix: None,
            },
            ReadinessLayer {
                layer: LAYER_INPUT, status: LayerStatus::Blocked,
                detail: "Input is unsupported for this read-only monitor lane".into(),
                fix: Some("Do not substitute raw/native IDs or fall back to the user's primary display".into()),
            },
        ],
    };
    metadata.as_object_mut().expect("capability object").extend(
        serde_json::json!(readiness)
            .as_object()
            .expect("readiness object")
            .clone(),
    );
    metadata
}

fn screenshot_response(
    screenshot: &crate::macos_monitor::Screenshot,
    selector: &str,
    compact: bool,
) -> CallToolResult {
    let mut metadata = serde_json::json!(crate::macos_monitor::Capabilities::new(true, true));
    let fields = serde_json::json!({
        "status": "screenshot captured", "screenshot_path": screenshot.path,
        "width": screenshot.width, "height": screenshot.height,
        "captured_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "display_target": selector, "capture_generation": selector,
    });
    metadata
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    if compact {
        compact_image_tool_result(metadata, "image/png")
    } else {
        use base64::Engine;
        image_tool_result(
            metadata.to_string(),
            base64::engine::general_purpose::STANDARD.encode(&screenshot.png),
        )
    }
}

fn window_error(error: String) -> String {
    let mut response = serde_json::json!({"ok":false,"error":error});
    if error.starts_with(crate::macos_monitor::PLACEMENT_UNCONFIRMED) {
        response["effects_unconfirmed"] = true.into();
    }
    response.to_string()
}

fn window_response(receipt: crate::macos_monitor::Receipt) -> String {
    let mut response = match &receipt.value {
        Value::Window(WindowValue::Candidates(candidates)) => {
            serde_json::json!({"ok":true,"candidates":candidates})
        }
        Value::Window(WindowValue::Bound(binding)) => {
            serde_json::json!({"ok":true,"bound_window":binding})
        }
        Value::Window(WindowValue::Placed(result)) => {
            serde_json::json!({"ok":result.verified(),"placement":result})
        }
        Value::Window(WindowValue::PlacementUnconfirmed { result, error }) => {
            serde_json::json!({"ok":false,"placement":result,"effects_unconfirmed":true,"error":error})
        }
        Value::Window(WindowValue::Unbound) => serde_json::json!({"ok":true,"unbound":true}),
        _ => return window_error("unexpected window result".into()),
    };
    let ready = response.to_string();
    if !receipt.commit() {
        // Keep the complete observation, including verified effects. Only the
        // delivery/commit is unconfirmed; never replace it with a generic error.
        if response.get("placement").is_some() {
            response["ok"] = false.into();
            response["effects_unconfirmed"] = true.into();
            let expired = "window receipt expired; placement effects may already have applied; no rollback attempted";
            response["error"] = match response["error"].as_str() {
                Some(error) => format!("{error}; {expired}"),
                None => expired.into(),
            }
            .into();
        } else {
            return window_error("window receipt expired; binding cleanup retained; unbind effects may already have applied".into());
        }
        return response.to_string();
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_and_late_failure_responses_keep_partial_and_verified_evidence() {
        use crate::macos_monitor::{placement::*, Receipt};
        for verified in [false, true] {
            let bounds = Bounds {
                x: -300.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            };
            let result = PlacementResult {
                status: if verified {
                    PlacementStatus::Verified
                } else {
                    PlacementStatus::Partial
                },
                requested_global: bounds,
                before: Observation {
                    ax: bounds,
                    cg: bounds,
                },
                after: Some(Observation {
                    ax: bounds,
                    cg: bounds,
                }),
                writes_attempted: if verified { 2 } else { 1 },
                focus_interference: Some(false),
                detail: if verified {
                    None
                } else {
                    Some("stopped after position".into())
                },
            };
            for late_failure in [false, true] {
                let value = if late_failure {
                    WindowValue::PlacementUnconfirmed {
                        result: result.clone(),
                        error: "helper died".into(),
                    }
                } else {
                    WindowValue::Placed(result.clone())
                };
                let (receipt, mut committed) = Receipt::fixture(Value::Window(value));
                if !late_failure {
                    committed.close();
                }
                let response: serde_json::Value =
                    serde_json::from_str(&window_response(receipt)).unwrap();
                assert_eq!(response["ok"], false);
                assert_eq!(response["effects_unconfirmed"], true);
                assert_eq!(
                    response["placement"],
                    serde_json::to_value(&result).unwrap()
                );
            }
        }
        let response: serde_json::Value = serde_json::from_str(&window_error(format!(
            "{}; receipt deadline exceeded",
            crate::macos_monitor::PLACEMENT_UNCONFIRMED
        )))
        .unwrap();
        assert_eq!(response["effects_unconfirmed"], true);
    }

    #[tokio::test]
    async fn macos_monitor_readiness_retains_common_envelope_without_probes() {
        let broker = crate::macos_monitor::Broker::default();
        let selector = format!(
            "macos_virtual:{}:{}",
            "a".repeat(32),
            crate::macos_monitor::DISPLAY_ID_MIN
        );
        let authority = Authority {
            owner_surface: true,
            autonomy: std::sync::Arc::new(tokio::sync::RwLock::new(Default::default())),
        };
        let snapshot = broker
            .inspect(
                Inspection::Status {
                    selector: selector.clone(),
                },
                authority,
            )
            .await
            .unwrap();
        let metadata = readiness_metadata(&snapshot, &selector);
        assert_eq!(metadata["target"], selector);
        assert_eq!(metadata["ready"], false);
        assert_eq!(metadata["capture_ready"], false);
        assert_eq!(metadata["lifecycle_ready"], false);
        assert!(metadata["summary"].is_string());
        let layers = metadata["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 5);
        assert_eq!(layers[1]["status"], "unknown");
        assert_eq!(layers[4]["status"], "blocked");
        assert!(broker.not_started());
    }

    #[tokio::test]
    async fn inventory_and_status_dispatch_preserve_caller_trust_without_starting_broker() {
        let directory = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_with_log_dir(directory.path().to_path_buf());
        let autonomy = state.read().await.autonomy.clone();
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (_home, server) = super::super::tests::test_server(state, bus.clone());
        let selector = format!(
            "macos_virtual:{}:{}",
            "a".repeat(32),
            crate::macos_monitor::DISPLAY_ID_MIN
        );
        for granted in [false, true] {
            autonomy.write().await.user_display_granted = granted;
            for trust in [ToolCallerTrust::OwnerSurface, ToolCallerTrust::Scoped] {
                for (tool, args) in [
                    ("list_macos_monitors", serde_json::json!({})),
                    (
                        "inspect",
                        serde_json::json!({"argv":["display", "monitors"]}),
                    ),
                    (
                        "display_readiness",
                        serde_json::json!({"display_target":selector}),
                    ),
                    (
                        "inspect",
                        serde_json::json!({"argv":["display", "status", "--target", selector]}),
                    ),
                ] {
                    let result = server
                        .call_tool_by_name_as_caller(
                            tool,
                            args.clone(),
                            None,
                            None,
                            ToolCaller {
                                trust,
                                actor: crate::access::actor::ActorBinding::unattributed(),
                                fs_scope: None,
                            },
                        )
                        .await
                        .unwrap();
                    let result = serde_json::to_value(result).unwrap();
                    let text = result["content"][0]["text"].as_str().unwrap();
                    let inventory = tool == "list_macos_monitors" || args["argv"][1] == "monitors";
                    if trust == ToolCallerTrust::Scoped && (inventory || !granted) {
                        assert!(
                            text.contains(if inventory {
                                "requires an owner surface"
                            } else {
                                "existing explicit user-display grant"
                            }),
                            "{text}"
                        );
                    } else {
                        let data: serde_json::Value = serde_json::from_str(text).unwrap();
                        assert_eq!(data["broker_state"], "not_started");
                        if inventory {
                            assert_eq!(data["monitors"], serde_json::json!([]));
                        } else {
                            assert_eq!(data["lifecycle_status"], "unknown");
                            for field in [
                                "ready",
                                "lifecycle_ready",
                                "capture_ready",
                                "input_supported",
                                "streaming_supported",
                            ] {
                                assert_eq!(data[field], false, "{field}");
                            }
                        }
                    }
                    assert!(bus.macos_monitors.not_started());
                    assert_eq!(autonomy.read().await.user_display_granted, granted);
                }
            }
        }
        // The direct owner entry point uses the same non-starting path.
        let inventory: serde_json::Value =
            serde_json::from_str(&server.list_macos_monitors().await).unwrap();
        assert_eq!(inventory["monitors"], serde_json::json!([]));
        assert!(bus.macos_monitors.not_started());
        assert!(events.try_recv().is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn macos_monitor_raw_ids_fail_before_native_cu_ax_readiness_or_streaming() {
        use crate::computer_use::{CuAction, CuExecOptions, DisplayBackend, DisplayTarget};
        let directory = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let bus = EventBus::new();
        let id = crate::macos_monitor::DISPLAY_ID_MIN;
        let target = DisplayTarget::Virtual { id };
        let mut counter = 0;
        for backend in [
            DisplayBackend::MacOS,
            DisplayBackend::Windows,
            DisplayBackend::X11,
            DisplayBackend::Wayland,
        ] {
            let result = crate::computer_use::execute_actions(
                &[CuAction::Screenshot],
                target,
                backend,
                directory.path(),
                &mut counter,
                &None,
                None,
                true,
                None,
                CuExecOptions::default(),
            )
            .await;
            assert_eq!(
                result.results[0].error.as_deref(),
                Some(crate::macos_monitor::UNSUPPORTED)
            );
        }
        assert!(crate::computer_use::read_screen_elements(target, false)
            .await
            .unwrap_err()
            .contains(crate::macos_monitor::UNSUPPORTED));
        let readiness = crate::cu_readiness::probe_readiness(target, true, true, &None).await;
        assert!(!readiness.ready);
        assert!(readiness
            .layers
            .iter()
            .all(|layer| layer.detail == crate::macos_monitor::UNSUPPORTED));
        crate::display_glue::activate_user_display(&bus, &registry, None, id, true).await;
        assert!(registry.read().await.get_any(id).is_none());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn macos_monitor_screenshot_preserves_image_and_compact_contracts() {
        let directory = tempfile::tempdir().unwrap();
        let screenshot = crate::macos_monitor::Screenshot {
            path: directory.path().join("owned.png"),
            png: b"fixture-png".to_vec(),
            width: 640,
            height: 480,
        };
        let full = serde_json::to_value(screenshot_response(
            &screenshot,
            "macos_virtual:fixture:1",
            false,
        ))
        .unwrap();
        assert_eq!(full["content"][1]["type"], "image");
        assert_eq!(full["content"][1]["mimeType"], "image/png");
        let metadata: serde_json::Value =
            serde_json::from_str(full["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(metadata["capture_generation"], "macos_virtual:fixture:1");
        assert_eq!(metadata["capture_ready"], true);
        let compact = serde_json::to_value(screenshot_response(
            &screenshot,
            "macos_virtual:fixture:1",
            true,
        ))
        .unwrap();
        assert_eq!(compact["content"].as_array().unwrap().len(), 1);
        let metadata: serde_json::Value =
            serde_json::from_str(compact["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            metadata["screenshot_path"],
            screenshot.path.to_string_lossy().as_ref()
        );
        assert_eq!(metadata["width"], 640);
        assert_eq!(metadata["input_supported"], false);
    }

    #[test]
    fn macos_monitor_keeps_original_iam_and_never_falls_back_from_reserved_selectors() {
        use crate::peer::access_policy::PeerOperation;
        assert_eq!(
            mcp_tool_operation("create_virtual_display"),
            PeerOperation::DisplayInput
        );
        assert_eq!(
            mcp_tool_operation("destroy_virtual_display"),
            PeerOperation::DisplayInput
        );
        assert_eq!(
            mcp_tool_operation("take_screenshot"),
            PeerOperation::DisplayView
        );
        for value in [
            "macos_virtual",
            "macos_virtual:bad",
            " MACOS_VIRTUAL:42 ",
            "display_macos_virtual:7",
            ":macos_virtual:1",
            "macos_virtual:ffffffffffffffffffffffffffffffff:1",
        ] {
            assert!(resolve_display_target(value).is_err());
            assert!(resolve_concrete_shared_view_target(Some(value.into()), Some(0)).is_err());
            assert!(crate::display_glue::parse_display_target_str(value, true).is_err());
        }
    }

    #[tokio::test]
    async fn macos_monitor_scoped_lifecycle_and_capture_require_shared_session_grant() {
        let directory = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_with_log_dir(directory.path().to_path_buf());
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let server = IntendantServer::new(state, bus);
        let create = server
            .create_macos_monitor(
                CreateVirtualDisplayParams {
                    width: Some(640),
                    height: Some(480),
                    minimum_display_id: None,
                    maximum_display_id: None,
                },
                ToolCallerTrust::Scoped,
            )
            .await;
        assert!(create.contains("existing explicit user-display grant"));
        let destroy = server
            .destroy_macos_monitor(
                DestroyVirtualDisplayParams {
                    display_id: 1,
                    capture_generation: "macos_virtual:bad".into(),
                    note: None,
                },
                ToolCallerTrust::Scoped,
            )
            .await;
        assert!(destroy.contains("existing explicit user-display grant"));
        let result = server
            .screenshot_macos_monitor("macos_virtual:bad".into(), false, ToolCallerTrust::Scoped)
            .await
            .unwrap();
        assert!(serde_json::to_string(&result)
            .unwrap()
            .contains("existing explicit user-display grant"));
        assert!(events.try_recv().is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn macos_monitor_unsupported_tools_reject_before_any_display_effect() {
        let directory = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let server = IntendantServer::new(
            super::super::tests::test_state_with_log_dir(directory.path().to_path_buf()),
            bus,
        );
        for selector in [
            " MACOS_VIRTUAL:malformed ".to_string(),
            "macos_virtual".into(),
            "macos_window:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:1".into(),
            " MACOS_WINDOW:bad ".into(),
            "macos_candidate:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "display_macos_candidate:bad".into(),
            crate::macos_monitor::DISPLAY_ID_MIN.to_string(),
            format!("display_{}", crate::macos_monitor::DISPLAY_ID_MAX),
            format!(":{}", crate::macos_monitor::DISPLAY_ID_MIN),
        ] {
            let args = serde_json::json!({"display_target": selector, "display_id": 0, "actions": [{"type":"screenshot"}], "region": {"x":0.0,"y":0.0,"width":1.0,"height":1.0}});
            for tool in [
                "read_screen",
                "display_readiness",
                "execute_cu_actions",
                "show_shared_view",
                "focus_shared_view",
                "request_shared_view_input",
                "capture_shared_view_frame",
            ] {
                let result = server
                    .call_tool_by_name_for_session(tool, args.clone(), None, None)
                    .await
                    .unwrap();
                assert!(
                    serde_json::to_string(&result)
                        .unwrap()
                        .contains(crate::macos_monitor::UNSUPPORTED),
                    "{tool}"
                );
            }
            // Call the #[tool] entry points directly too: stdio's generated
            // router does not go through call_tool_by_name_for_session.
            macro_rules! params {
                () => {
                    Parameters(serde_json::from_value(args.clone()).unwrap())
                };
            }
            for result in [
                server.read_screen(params!()).await.unwrap(),
                server.display_readiness(params!()).await.unwrap(),
                server.execute_cu_actions(params!()).await.unwrap(),
                server.capture_shared_view_frame(params!()).await.unwrap(),
            ] {
                assert!(serde_json::to_string(&result)
                    .unwrap()
                    .contains(crate::macos_monitor::UNSUPPORTED));
            }
            for result in [
                server.show_shared_view(params!()).await,
                server.focus_shared_view(params!()).await,
                server.request_shared_view_input(params!()).await,
            ] {
                assert!(result.contains(crate::macos_monitor::UNSUPPORTED));
            }
            // Both peer operations reject before peer lookup or network access.
            assert!(
                crate::peer::ops::take_screenshot(None, "unused", Some(selector.clone()))
                    .await
                    .text
                    .contains(crate::macos_monitor::UNSUPPORTED)
            );
            assert!(crate::peer::ops::execute_cu_actions(
                None,
                "unused",
                serde_json::json!([]),
                Some(selector),
                None,
                None,
                None,
                None
            )
            .await
            .text
            .contains(crate::macos_monitor::UNSUPPORTED));
        }
        for display_id in [
            crate::macos_monitor::DISPLAY_ID_MIN,
            crate::macos_monitor::DISPLAY_ID_MAX,
        ] {
            let args = serde_json::json!({"display_target":"user_session", "display_id": display_id, "region": {"x":0.0,"y":0.0,"width":1.0,"height":1.0}});
            macro_rules! params {
                () => {
                    Parameters(serde_json::from_value(args.clone()).unwrap())
                };
            }
            for result in [
                server.take_display(params!()).await,
                server.release_display(params!()).await,
                server.grant_user_display(params!()).await,
                server.revoke_user_display(params!()).await,
                server.show_shared_view(params!()).await,
                server.focus_shared_view(params!()).await,
                server.request_shared_view_input(params!()).await,
            ] {
                assert!(result.contains(crate::macos_monitor::UNSUPPORTED));
            }
            let capture = server.capture_shared_view_frame(params!()).await.unwrap();
            assert!(serde_json::to_string(&capture)
                .unwrap()
                .contains(crate::macos_monitor::UNSUPPORTED));
        }
        assert!(events.try_recv().is_err());
        assert!(
            !server
                .state
                .read()
                .await
                .autonomy
                .read()
                .await
                .user_display_granted
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn window_typed_and_facade_dispatch_never_elevate_scoped_display_grants() {
        let directory = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_with_log_dir(directory.path().to_path_buf());
        let autonomy = state.read().await.autonomy.clone();
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (_home, server) = super::super::tests::test_server(state, bus.clone());
        let selector = format!(
            "macos_virtual:{}:{}",
            "a".repeat(32),
            crate::macos_monitor::DISPLAY_ID_MIN
        );
        let identity =
            serde_json::json!({"pid":123,"start_seconds":456,"start_micros":7,"window_id":8});
        let bounds = serde_json::json!({"x":0,"y":0,"width":100,"height":100});
        for granted in [false, true] {
            autonomy.write().await.user_display_granted = granted;
            for (tool, args) in [
                ("list_macos_windows", serde_json::json!({"pid":123})),
                (
                    "bind_macos_window",
                    serde_json::json!({"display_target":selector,"candidate":"macos_candidate:00000000000000000000000000000001","identity":identity}),
                ),
                (
                    "place_macos_window",
                    serde_json::json!({"binding":"macos_window:fixture:1","bounds":bounds}),
                ),
                (
                    "unbind_macos_window",
                    serde_json::json!({"binding":"macos_window:fixture:1"}),
                ),
                (
                    "inspect",
                    serde_json::json!({"argv":["display","windows","123"]}),
                ),
                (
                    "act",
                    serde_json::json!({"argv":["display","bind-window",selector,"macos_candidate:00000000000000000000000000000001",identity.to_string()]}),
                ),
                (
                    "act",
                    serde_json::json!({"argv":["display","place-window","macos_window:fixture:1",bounds.to_string()]}),
                ),
                (
                    "act",
                    serde_json::json!({"argv":["display","unbind-window","macos_window:fixture:1"]}),
                ),
            ] {
                let result = server
                    .call_tool_by_name_as_caller(
                        tool,
                        args,
                        None,
                        None,
                        ToolCaller {
                            trust: ToolCallerTrust::Scoped,
                            actor: crate::access::actor::ActorBinding::unattributed(),
                            fs_scope: None,
                        },
                    )
                    .await
                    .unwrap();
                let result = serde_json::to_value(result).unwrap();
                assert!(
                    result["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("owner surface"),
                    "{result}"
                );
                assert!(bus.macos_monitors.not_started());
                assert_eq!(autonomy.read().await.user_display_granted, granted);
            }
        }
        assert!(events.try_recv().is_err());
    }
}
