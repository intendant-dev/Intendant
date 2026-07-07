//! The peer-federation tool implementations: listing federated peers,
//! messaging a peer's agent, delegating tasks the peer's own agent
//! executes under its own autonomy and approval policy, and direct
//! computer use on peer displays (list/screenshot/actions) over the
//! peer's `/mcp` with this daemon's mTLS identity — gated peer-side by
//! the profile the peer granted us. The operation bodies live in
//! `crate::peer::ops`, shared with the native `peer` tool so the
//! surfaces cannot drift.

use super::*;

/// Fold a [`crate::peer::ops::PeerToolOutput`] into an MCP result:
/// text part first, then each screenshot as an image content block.
/// `is_error` maps to the MCP error flag while keeping any attached
/// evidence (a partially failed CU batch still shows its post-action
/// screenshot).
fn peer_output_tool_result(output: crate::peer::ops::PeerToolOutput) -> CallToolResult {
    let mut content = vec![Content::text(output.text)];
    for image in output.images {
        content.push(Content::image(image.data, image.media_type));
    }
    if output.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

impl IntendantServer {
    async fn peer_registry(&self) -> Option<crate::peer::PeerRegistry> {
        self.state.read().await.peer_registry.clone()
    }

    #[tool(
        description = "List federated peer daemons: id, label, connection state, advertised capabilities, currently visible sessions, and available displays."
    )]
    pub(crate) async fn list_peers(&self) -> String {
        crate::peer::ops::list_peers_json(self.peer_registry().await.as_ref())
    }

    #[tool(
        description = "Send a text message to a federated peer daemon's agent. Addresses the peer's current/default session unless 'session' targets one. The receiving peer authorizes against its own grants for this daemon."
    )]
    pub(crate) async fn peer_send_message(
        &self,
        Parameters(params): Parameters<PeerSendMessageParams>,
    ) -> String {
        crate::peer::ops::send_message_json(
            self.peer_registry().await.as_ref(),
            &params.peer_id,
            params.message,
            params.session,
        )
        .await
    }

    #[tool(
        description = "Delegate a task to a federated peer daemon: the peer's own agent executes the natural-language instructions on its machine under its own autonomy and approval policy. Returns a task id; progress streams to the dashboard's peers rail."
    )]
    pub(crate) async fn peer_delegate_task(
        &self,
        Parameters(params): Parameters<PeerDelegateTaskParams>,
    ) -> String {
        crate::peer::ops::delegate_task_json(
            self.peer_registry().await.as_ref(),
            &params.peer_id,
            params.instructions,
            params.context,
        )
        .await
    }

    #[tool(
        description = "List the displays a federated peer daemon currently offers (ids, names, resolutions). Invoked over the peer's /mcp with this daemon's identity; gated peer-side by the display-view grant of the profile the peer issued this daemon."
    )]
    pub(crate) async fn peer_list_displays(
        &self,
        Parameters(params): Parameters<PeerListDisplaysParams>,
    ) -> String {
        crate::peer::ops::list_displays_json(self.peer_registry().await.as_ref(), &params.peer_id)
            .await
    }

    #[tool(
        description = "Take a screenshot of a federated peer daemon's display. Returns an MCP image content block. Needs a peer-granted profile with display view (read-only-display or better)."
    )]
    pub(crate) async fn peer_take_screenshot(
        &self,
        Parameters(params): Parameters<PeerTakeScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(peer_output_tool_result(
            crate::peer::ops::take_screenshot(
                self.peer_registry().await.as_ref(),
                &params.peer_id,
                params.display_target,
            )
            .await,
        ))
    }

    #[tool(
        description = "Execute computer-use actions on a federated peer daemon's display (click, type, scroll, etc — the peer's CuAction vocabulary). Returns per-action status plus the annotated post-action screenshot. Needs a peer-granted profile with display input (peer-operator or peer-root)."
    )]
    pub(crate) async fn peer_execute_cu_actions(
        &self,
        Parameters(params): Parameters<PeerExecuteCuActionsParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(peer_output_tool_result(
            crate::peer::ops::execute_cu_actions(
                self.peer_registry().await.as_ref(),
                &params.peer_id,
                serde_json::Value::Array(params.actions),
                params.display_target,
                params.coordinate_space,
            )
            .await,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tests::test_state;
    use crate::peer::ops::FEDERATION_INACTIVE_NOTE;

    fn empty_registry() -> crate::peer::PeerRegistry {
        let (log_sink, _rx) = tokio::sync::mpsc::channel(crate::peer::LOG_CHANNEL_CAPACITY);
        crate::peer::PeerRegistry::new(log_sink)
    }

    #[test]
    fn peer_tools_degrade_gracefully_without_a_registry() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let listed: serde_json::Value =
                serde_json::from_str(&server.list_peers().await).unwrap();
            assert_eq!(listed["peers"], serde_json::json!([]));
            assert_eq!(listed["note"], FEDERATION_INACTIVE_NOTE);

            let sent = server
                .peer_send_message(Parameters(PeerSendMessageParams {
                    peer_id: "intendant:nowhere".to_string(),
                    message: "hello".to_string(),
                    session: None,
                }))
                .await;
            let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
            assert_eq!(sent["ok"], serde_json::json!(false));
            assert_eq!(sent["error"], FEDERATION_INACTIVE_NOTE);
        });
    }

    #[test]
    fn peer_tools_report_unknown_peers_against_an_empty_registry() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            state.write().await.peer_registry = Some(empty_registry());
            let server = IntendantServer::new(state, EventBus::new());

            let listed: serde_json::Value =
                serde_json::from_str(&server.list_peers().await).unwrap();
            assert_eq!(listed["peers"], serde_json::json!([]));
            assert!(listed.get("note").is_none());

            let delegated = server
                .peer_delegate_task(Parameters(PeerDelegateTaskParams {
                    peer_id: "intendant:nowhere".to_string(),
                    instructions: "bring up a display".to_string(),
                    context: None,
                }))
                .await;
            let delegated: serde_json::Value = serde_json::from_str(&delegated).unwrap();
            assert_eq!(delegated["ok"], serde_json::json!(false));
            assert!(delegated["error"]
                .as_str()
                .unwrap()
                .contains("peer not found: intendant:nowhere"));
        });
    }

    #[test]
    fn peer_cu_tools_degrade_gracefully_without_a_registry() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let displays = server
                .peer_list_displays(Parameters(PeerListDisplaysParams {
                    peer_id: "intendant:nowhere".to_string(),
                }))
                .await;
            let displays: serde_json::Value = serde_json::from_str(&displays).unwrap();
            assert_eq!(displays["ok"], serde_json::json!(false));
            assert_eq!(displays["error"], FEDERATION_INACTIVE_NOTE);

            let shot = server
                .peer_take_screenshot(Parameters(PeerTakeScreenshotParams {
                    peer_id: "intendant:nowhere".to_string(),
                    display_target: None,
                }))
                .await
                .unwrap();
            assert!(shot.is_error.unwrap_or(false));
            let shot_json = serde_json::to_value(&shot).unwrap();
            let text = shot_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            assert!(text.contains(FEDERATION_INACTIVE_NOTE));

            let acted = server
                .peer_execute_cu_actions(Parameters(PeerExecuteCuActionsParams {
                    peer_id: "intendant:nowhere".to_string(),
                    actions: vec![serde_json::json!({ "type": "screenshot" })],
                    display_target: None,
                    coordinate_space: None,
                }))
                .await
                .unwrap();
            assert!(acted.is_error.unwrap_or(false));
        });
    }

    #[test]
    fn peer_tools_dispatch_through_the_http_tool_router() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name_for_session("list_peers", serde_json::json!({}), None, None)
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            assert!(text.contains(FEDERATION_INACTIVE_NOTE));
        });
    }
}
