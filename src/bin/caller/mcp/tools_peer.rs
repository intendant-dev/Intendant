//! The peer-federation tool implementations: listing federated peers,
//! messaging a peer's agent, and delegating tasks the peer's own agent
//! executes under its own autonomy and approval policy. Stage 0 of the
//! agent-facing peer control surface — deliberately the delegation verbs
//! only; capability invocation on peers (display, computer use) is
//! future work. The operation bodies live in `crate::peer::ops`, shared
//! with the native `peer` tool so the surfaces cannot drift.

use super::*;

impl IntendantServer {
    async fn peer_registry(&self) -> Option<crate::peer::PeerRegistry> {
        self.state.read().await.peer_registry.clone()
    }

    #[tool(
        description = "List federated peer daemons: id, label, connection state, advertised capabilities, and currently visible sessions."
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
