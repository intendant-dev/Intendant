//! These tests must never reach native GUI APIs, even on a developer's Mac.
use super::*;

#[test]
fn background_window_read_capture_and_input_require_user_authority() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let state = test_state();
        let autonomy = state.read().await.autonomy.clone();
        autonomy.write().await.user_display_granted = false;
        let server = IntendantServer::new(state, EventBus::new());
        let denied = serde_json::to_value(text_tool_error(
            crate::computer_use::user_session_denied_message(),
        ))
        .unwrap();
        for target in ["macos_windows", "macos_window:1:2:3:4", "macos_window_typo"] {
            let params =
                serde_json::from_value(serde_json::json!({"display_target":target})).unwrap();
            let read = server
                .read_screen_as_caller(Parameters(params), ToolCallerTrust::Scoped)
                .await
                .unwrap();
            assert_eq!(serde_json::to_value(read).unwrap(), denied, "read {target}");
            let params =
                serde_json::from_value(serde_json::json!({"display_target":target})).unwrap();
            let capture = server
                .take_screenshot_with_output(Parameters(params), false, ToolCallerTrust::Scoped)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(capture).unwrap(),
                denied,
                "capture {target}"
            );
            let params = serde_json::from_value(
                serde_json::json!({"display_target":target,"actions":[{"type":"screenshot"}]}),
            )
            .unwrap();
            let input = server
                .execute_cu_actions_with_output(Parameters(params), false, ToolCallerTrust::Scoped)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(input).unwrap(),
                denied,
                "input {target}"
            );
        }
        assert!(
            !autonomy.read().await.user_display_granted,
            "no route may mint a display grant"
        );
    });
}

#[test]
fn malformed_background_targets_fail_before_any_native_probe() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = IntendantServer::new(test_state(), EventBus::new());
        for target in [
            "macos_window_typo",
            "macos_window:0:0:0:0",
            "macos_window:1:2",
        ] {
            let params = serde_json::from_value(
                serde_json::json!({"display_target":target,"actions":[{"type":"screenshot"}]}),
            )
            .unwrap();
            let result = server
                .execute_cu_actions_with_output(
                    Parameters(params),
                    false,
                    ToolCallerTrust::OwnerSurface,
                )
                .await
                .unwrap();
            assert_eq!(
                result.is_error,
                Some(true),
                "must not resolve {target} to display 99 or user_session"
            );
        }
    });
}

#[test]
fn clipboard_transport_is_rejected_before_owner_native_dispatch() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = IntendantServer::new(test_state(), EventBus::new());
        let params = serde_json::from_value(serde_json::json!({
            "display_target":"macos_window:1:2:3:4", "actions":[{"type":"paste","text":"do not copy me"}]
        })).unwrap();
        let result = server.execute_cu_actions_with_output(Parameters(params), false, ToolCallerTrust::OwnerSurface).await.unwrap();
        assert_eq!(result.is_error, Some(true));
    });
}
