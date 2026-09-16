//! Native windows reuse existing display.view/display.input tool operations.
use super::*;
use crate::background_cu::{self, Request};

impl IntendantServer {
    pub(crate) async fn background_cu_call(
        &self,
        spec: String,
        request: Request,
        caller: ToolCallerTrust,
        compact_output: bool,
    ) -> Result<CallToolResult, McpError> {
        let (autonomy, dir) = {
            let state = self.state.read().await;
            (
                state.autonomy.clone(),
                state
                    .screenshot_dir
                    .clone()
                    .unwrap_or_else(|| state.log_dir.join("screenshots")),
            )
        };
        // Discovery, AX, capture and input are ALL user-session resources.
        // MCP's resolved per-tool IAM operation remains a separate outer gate.
        let granted = autonomy.read().await.user_display_granted;
        if !caller.allows_user_session(granted) {
            return Ok(text_tool_error(
                crate::computer_use::user_session_denied_message(),
            ));
        }
        let reply = match background_cu::run(spec, request).await {
            Ok(reply) => reply,
            Err(error) => return Ok(text_tool_error(format!("background CU: {error}"))),
        };
        let Some(png) = reply.png else {
            return Ok(if reply.failed {
                text_tool_error(reply.text)
            } else {
                text_tool_result(reply.text)
            });
        };
        // Preserve compact ctl consumers' artifact contract and MCP image blocks.
        // No caller-controlled path; unique create-new files with owner-only mode.
        let path = dir.join(format!("background-{}.png", uuid::Uuid::new_v4()));
        let write = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            std::io::Write::write_all(&mut file, &png)
        })();
        if let Err(error) = write {
            return Ok(text_tool_error(format!(
                "background screenshot storage failed: {error}"
            )));
        }
        let metadata = serde_json::json!({"detail":reply.text, "screenshot_path":path,
            "mode":"background", "status":if reply.failed {"failed"} else {"observed"}});
        if compact_output && !reply.failed {
            return Ok(compact_image_tool_result(metadata, "image/png"));
        }
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        Ok(if reply.failed {
            image_tool_error(metadata.to_string(), encoded)
        } else {
            image_tool_result(metadata.to_string(), encoded)
        })
    }

    pub(crate) async fn background_cu_actions(
        &self,
        params: ExecuteCuActionsParams,
        caller: ToolCallerTrust,
        compact_output: bool,
    ) -> Result<CallToolResult, McpError> {
        let normalized = match params.coordinate_space.as_deref() {
            None | Some("pixel") => false,
            Some("normalized_1000") => true,
            _ => {
                return Ok(text_tool_error(
                    "background coordinate_space must be pixel or normalized_1000",
                ))
            }
        };
        if params.annotate == Some(true) {
            return Ok(text_tool_error(
                "background annotation not implemented; use clean window captures",
            ));
        }
        if params.settle.and_then(|s| s.resolve()).is_some() {
            return Ok(text_tool_error("background settle not implemented; observations use a 150 ms delay, not a quiescence claim"));
        }
        if let Err(error) = background_cu::validate_actions(&params.actions) {
            return Ok(text_tool_error(error));
        }
        self.background_cu_call(
            params.display_target.unwrap_or_default(),
            Request::Actions {
                actions: params.actions,
                normalized,
                observe: params.observe.unwrap_or_default(),
            },
            caller,
            compact_output,
        )
        .await
    }
}
