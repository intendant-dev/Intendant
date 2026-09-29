//! Task-scoped browser workstation; no request field selects another session.
use super::*;
use crate::browser_workspace::task_access::{self, Request, Response};

/// Read-only transport shape: no input/provisioning variant can be routed
/// through an inspect-lane command or a DisplayView-only primary gate.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(extend("type" = "object"))]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum InspectRequest {
    Status {},
    Screenshot { workspace_id: String },
}
impl From<InspectRequest> for Request {
    fn from(value: InspectRequest) -> Self {
        match value {
            InspectRequest::Status {} => Request::Status {},
            InspectRequest::Screenshot { workspace_id } => Request::Screenshot { workspace_id },
        }
    }
}

impl IntendantServer {
    #[tool(
        description = "Use your own background browser on macOS without taking over the user's desktop. First call op=open with an http(s) URL or about:blank; Intendant automatically provisions one browser for your authenticated supervised session. Keep the returned workspace_id and include it in screenshot/input/close calls so stale requests cannot target a replacement. Then use status, screenshot, keyboard {request_id,action:{type:insert_text,text}|{type:key,key}|{type:select_all}}, click {request_id,x,y}, scroll {request_id,x,y,delta_y}, navigate {request_id,url} in the same original page, or close. Input UUIDs are single-use; never replay uncertain input with a new UUID. Coordinates are window-local logical points; screenshot metadata gives the window origin and pixel scale. No per-action workspace approval. Keys are bounded page keys (Enter, Tab, ShiftTab, Backspace, Delete, arrows, Home, End, PageUp, PageDown, Escape, Space). No global cursor, system shortcuts, clipboard, personal profiles or arbitrary native-app keyboard. The browser lives across task turns and is cleaned up when its session ends. Only the registered supervised-session credential is accepted; owner/anonymous identity is not substituted."
    )]
    pub(crate) async fn task_browser(
        &self,
        Parameters(_request): Parameters<Request>,
    ) -> CallToolResult {
        text_tool_error("task_browser requires the authenticated supervised-session HTTP/ctl path")
    }

    #[tool(
        description = "Read only your authenticated supervised session's task browser: op=status returns its redacted handle; op=screenshot with that workspace_id returns memory-only pixels. Cannot create a browser or send input. Current session identity and IAM are rechecked; no owner credential or other session's workspace is substituted."
    )]
    pub(crate) async fn inspect_task_browser(
        &self,
        Parameters(_request): Parameters<InspectRequest>,
    ) -> CallToolResult {
        text_tool_error(
            "inspect_task_browser requires the authenticated supervised-session HTTP/ctl path",
        )
    }

    pub(super) async fn task_browser_as_session(
        &self,
        request: Request,
        actor: crate::access::actor::ActorBinding,
        epoch: Option<String>,
    ) -> CallToolResult {
        let autonomy = self.state.read().await.autonomy.clone();
        match task_access::execute(request, actor, epoch, &self.bus, autonomy).await {
            Response::Json(value) => {
                if value["ok"] == false {
                    text_tool_error(value.to_string())
                } else {
                    text_tool_result(value.to_string())
                }
            }
            Response::Image { metadata, png } => {
                use base64::Engine as _;
                image_tool_result(
                    metadata.to_string(),
                    base64::engine::general_purpose::STANDARD.encode(png),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_tool_schemas_keep_object_roots_and_read_only_variants() {
        let full = serde_json::to_value(IntendantServer::task_browser_tool_attr()).unwrap();
        let read = serde_json::to_value(IntendantServer::inspect_task_browser_tool_attr()).unwrap();
        assert_eq!(full["inputSchema"]["type"], "object");
        assert_eq!(read["inputSchema"]["type"], "object");
        assert_eq!(full["inputSchema"]["oneOf"].as_array().unwrap().len(), 8);
        assert_eq!(read["inputSchema"]["oneOf"].as_array().unwrap().len(), 2);
        assert!(serde_json::from_value::<InspectRequest>(
            serde_json::json!({"op":"open","url":"about:blank"})
        )
        .is_err());
        assert!(serde_json::from_value::<InspectRequest>(
            serde_json::json!({"op":"status","owner_surface":true})
        )
        .is_err());
        assert!(serde_json::from_value::<InspectRequest>(serde_json::json!({
            "op":"navigate","workspace_id":"bw-test","request_id":"id","url":"https://example.test/"
        }))
        .is_err());
        assert_eq!(
            crate::mcp::mcp_tool_operation("inspect_task_browser"),
            crate::peer::access_policy::PeerOperation::DisplayView
        );
    }
}
