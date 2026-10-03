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
    ExtensionViews {
        workspace_id: String,
    },
    Screenshot {
        workspace_id: String,
        #[serde(default)]
        view_id: Option<String>,
    },
}
impl From<InspectRequest> for Request {
    fn from(value: InspectRequest) -> Self {
        match value {
            InspectRequest::Status {} => Request::Status {},
            InspectRequest::ExtensionViews { workspace_id } => {
                Request::ExtensionViews { workspace_id }
            }
            InspectRequest::Screenshot {
                workspace_id,
                view_id,
            } => Request::Screenshot {
                workspace_id,
                view_id,
            },
        }
    }
}

impl IntendantServer {
    #[tool(
        description = "Use your own browser with the authenticated supervised-session credential. Call op=open with url (http(s) or about:blank), then keep workspace_id on every screenshot/input/close request. Without an extension, this uses the macOS virtual monitor. Optional extension={archive_path,archive_sha256,archive_byte_length,manifest_version,version} selects an MV3 archive already approved by immutable daemon startup policy; it cannot grant itself approval. Extension workspaces use backend=headless_extension: no visible browser windows or native desktop input. For the real toolbar action call extension_popup with request_id, then inspect extension_views to discover current opaque view_id handles (including extension-created notification windows). Use view_id with screenshot, keyboard, click, scroll; omit it for the original website. extension_page with an explicit relative resource opens a normal extension tab, NOT an equivalent toolbar popup. Input/navigation use canonical single-use request_id UUIDs; do not replay uncertain actions. Keyboard action={type:insert_text,text}|{type:key,key}|{type:select_all}; keys Enter, Tab, ShiftTab, Backspace, Delete, arrows, Home, End, PageUp, PageDown, Escape, Space. Screenshot metadata defines coordinates: headless pages use page_css_pixels, native windows use window_logical_points. Navigate uses url and retains the original page. Close or session stop destroys this task's ephemeral browser/profile/extension state. Do not create or import a real funded wallet or its seed into this disposable profile. Password/protected receiver checks remain in force; wallet unlock, hardware signing and actual financial authorization are not granted by this tool. No personal profiles, generic script execution, caller endpoints, system shortcuts, clipboard, user-desktop fallback, or new per-action workspace approval."
    )]
    pub(crate) async fn task_browser(
        &self,
        Parameters(_request): Parameters<Request>,
    ) -> CallToolResult {
        text_tool_error("task_browser requires the authenticated supervised-session HTTP/ctl path")
    }

    #[tool(
        description = "Read only your authenticated supervised session's task browser: op=status returns its redacted handle; op=screenshot with workspace_id and optional view_id returns memory-only pixels; op=extension_views lists only the owning task's current offscreen extension views. Cannot create a browser or send input. Current session identity and IAM are rechecked; no owner credential or other session's workspace is substituted."
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
        assert_eq!(full["inputSchema"]["oneOf"].as_array().unwrap().len(), 11);
        assert_eq!(read["inputSchema"]["oneOf"].as_array().unwrap().len(), 3);
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
    #[test]
    fn popup_control_cannot_hide_in_read_only_schema_or_forge_a_target() {
        for op in ["extension_popup", "extension_page", "keyboard"] {
            assert!(serde_json::from_value::<InspectRequest>(
                serde_json::json!({"op":op,"workspace_id":"bw-task","request_id":"id"})
            )
            .is_err());
        }
        assert!(serde_json::from_value::<InspectRequest>(
            serde_json::json!({"op":"extension_views","workspace_id":"bw-task"})
        )
        .is_ok());
        for key in [
            "target_id",
            "cdp_ws_url",
            "runtime_id",
            "expression",
            "owner_surface",
        ] {
            let mut value = serde_json::json!({"op":"extension_popup","workspace_id":"bw-task","request_id":"id"});
            value[key] = serde_json::json!("foreign");
            assert!(serde_json::from_value::<Request>(value).is_err());
        }
    }
}
