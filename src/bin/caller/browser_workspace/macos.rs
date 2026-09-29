//! Owner-only managed Chrome-for-Testing launch onto an owned macOS monitor.
//!
//! This module does not make `macos_virtual` a generic input target. It launches
//! one disposable, nonactivating browser instance with a private profile, creates
//! exactly one background CDP window, and then enters the existing retained-window
//! bind/place lane. Input remains authorized only by the returned macos_window
//! generation and the existing bounded window tools.

use super::*;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

fn cft_bundle_path(executable: &Path) -> Result<PathBuf, BrowserWorkspaceError> {
    let macos = executable.parent().ok_or_else(|| {
        BrowserWorkspaceError::Launch(
            "managed Chrome for Testing executable has no MacOS parent".into(),
        )
    })?;
    let contents = macos.parent().ok_or_else(|| {
        BrowserWorkspaceError::Launch(
            "managed Chrome for Testing executable has no Contents parent".into(),
        )
    })?;
    let bundle = contents.parent().ok_or_else(|| {
        BrowserWorkspaceError::Launch(
            "managed Chrome for Testing executable has no app bundle".into(),
        )
    })?;
    if macos.file_name().and_then(|name| name.to_str()) != Some("MacOS")
        || contents.file_name().and_then(|name| name.to_str()) != Some("Contents")
        || bundle.extension().and_then(|ext| ext.to_str()) != Some("app")
    {
        return Err(BrowserWorkspaceError::Launch(
            "managed Chrome for Testing executable is not inside a canonical macOS app bundle"
                .into(),
        ));
    }
    Ok(bundle.to_path_buf())
}

fn parse_supervisor_pid(bytes: &[u8]) -> Result<u32, BrowserWorkspaceError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        pid: i32,
    }
    let receipt: Receipt = serde_json::from_slice(bytes).map_err(|_| {
        BrowserWorkspaceError::Launch("private browser supervisor PID receipt is invalid".into())
    })?;
    u32::try_from(receipt.pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            BrowserWorkspaceError::Launch("private browser supervisor PID must be positive".into())
        })
}

async fn supervised_browser_pid(child: &mut Child) -> Result<u32, BrowserWorkspaceError> {
    let stdout = child.stdout.as_mut().ok_or_else(|| {
        BrowserWorkspaceError::Launch("private macOS browser supervisor stdout unavailable".into())
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut line = Vec::new();
    loop {
        if line.len() >= 256 {
            return Err(BrowserWorkspaceError::Launch(
                "private macOS browser supervisor receipt exceeded 256 bytes".into(),
            ));
        }
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout_at(deadline, stdout.read(&mut byte))
            .await
            .map_err(|_| {
                BrowserWorkspaceError::Launch(
                    "private macOS browser supervisor did not publish its PID in 10 seconds".into(),
                )
            })?
            .map_err(|error| {
                BrowserWorkspaceError::Launch(format!(
                    "private macOS browser supervisor receipt failed: {error}"
                ))
            })?;
        if read == 0 {
            let status = child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.to_string())
                .unwrap_or_else(|| "unknown status".into());
            return Err(BrowserWorkspaceError::Launch(format!(
                "private macOS browser supervisor closed before PID receipt ({status})"
            )));
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] == 0 || (byte[0].is_ascii_control() && byte[0] != b'\r') {
            return Err(BrowserWorkspaceError::Launch(
                "private macOS browser supervisor receipt contained control bytes".into(),
            ));
        }
        line.push(byte[0]);
    }
    parse_supervisor_pid(&line)
}

async fn browser_endpoint(
    child: &mut Child,
    profile_dir: &Path,
) -> Result<DevToolsActivePort, BrowserWorkspaceError> {
    let client = local_cdp_client()?;
    let deadline = tokio::time::Instant::now() + CDP_STARTUP_TIMEOUT;
    let mut last_error: Option<String> = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            let detail = last_error
                .as_deref()
                .map(|error| format!("; last observation: {error}"))
                .unwrap_or_default();
            return Err(BrowserWorkspaceError::Launch(format!(
                "timed out waiting for supervised profile-bound CDP endpoint{detail}"
            )));
        }
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "private macOS browser supervisor exited before CDP was ready: {status}"
                )));
            }
            Err(error) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "cannot inspect private macOS browser supervisor: {error}"
                )));
            }
        }
        if let Some(endpoint) = read_devtools_active_port(profile_dir)? {
            let version_url = format!("http://127.0.0.1:{}/json/version", endpoint.port);
            match fetch_bounded_cdp_json_before(
                &client,
                &version_url,
                "supervised CDP version endpoint",
                deadline,
            )
            .await
            {
                Ok(version) => match validate_browser_websocket_identity(&version, &endpoint) {
                    Ok(()) => {
                        if read_devtools_active_port(profile_dir)?.as_ref() == Some(&endpoint) {
                            return Ok(endpoint);
                        }
                        last_error = Some("DevToolsActivePort changed during readiness".into());
                    }
                    Err(error) => last_error = Some(error.to_string()),
                },
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn valid_target_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

async fn create_background_target(
    endpoint: &DevToolsActivePort,
    url: &str,
) -> Result<String, BrowserWorkspaceError> {
    // Covers connect, send, pongs and response reads, not just socket.next().
    tokio::time::timeout(
        Duration::from_secs(8),
        create_background_target_inner(endpoint, url),
    )
    .await
    .map_err(|_| {
        BrowserWorkspaceError::Launch(
            "background target creation exceeded its fixed deadline; do not replay".into(),
        )
    })?
}

async fn create_background_target_inner(
    endpoint: &DevToolsActivePort,
    url: &str,
) -> Result<String, BrowserWorkspaceError> {
    let websocket_url = format!(
        "ws://127.0.0.1:{}{}",
        endpoint.port, endpoint.browser_websocket_path
    );
    if !exact_loopback_websocket_url(
        &websocket_url,
        endpoint.port,
        &endpoint.browser_websocket_path,
    ) {
        return Err(BrowserWorkspaceError::Launch(
            "profile-bound browser WebSocket endpoint was not exact loopback".into(),
        ));
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    let (mut socket, _) =
        tokio_tungstenite::connect_async_with_config(&websocket_url, Some(config), true)
            .await
            .map_err(|_| {
                BrowserWorkspaceError::Launch(
                    "private macOS browser CDP WebSocket connect failed".into(),
                )
            })?;
    socket
        .send(Message::Text(
            serde_json::json!({
                "id": 1,
                "method": "Target.createTarget",
                "params": {
                    "url": url,
                    "newWindow": true,
                    "background": true,
                    "width": 720,
                    "height": 530
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|_| BrowserWorkspaceError::Launch("background target CDP send failed".into()))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut total = 0usize;
    for _ in 0..64 {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| {
                BrowserWorkspaceError::Launch("background target CDP reply timed out".into())
            })?
            .ok_or_else(|| BrowserWorkspaceError::Launch("background target CDP closed".into()))?
            .map_err(|_| {
                BrowserWorkspaceError::Launch("background target CDP receive failed".into())
            })?;
        match message {
            Message::Text(text) => {
                total = total.saturating_add(text.len());
                if total > 2 * 1024 * 1024 {
                    return Err(BrowserWorkspaceError::Launch(
                        "background target CDP exceeded total byte limit".into(),
                    ));
                }
                let reply: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                    BrowserWorkspaceError::Launch(
                        "background target CDP returned invalid JSON".into(),
                    )
                })?;
                if reply.get("id").is_none() {
                    continue;
                }
                if reply["id"] != 1 || reply.get("error").is_some() {
                    return Err(BrowserWorkspaceError::Launch(
                        "background target CDP request failed or mismatched".into(),
                    ));
                }
                let target = reply["result"]["targetId"]
                    .as_str()
                    .filter(|id| valid_target_id(id))
                    .ok_or_else(|| {
                        BrowserWorkspaceError::Launch(
                            "background target CDP result lacked a valid target id".into(),
                        )
                    })?;
                return Ok(target.to_string());
            }
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.map_err(|_| {
                BrowserWorkspaceError::Launch("background target CDP pong failed".into())
            })?,
            _ => {
                return Err(BrowserWorkspaceError::Launch(
                    "unexpected background target CDP frame".into(),
                ));
            }
        }
    }
    Err(BrowserWorkspaceError::Launch(
        "background target CDP message limit exceeded".into(),
    ))
}

async fn exact_page_websocket(
    child: &mut Child,
    endpoint: &DevToolsActivePort,
    target_id: &str,
) -> Result<String, BrowserWorkspaceError> {
    let client = local_cdp_client()?;
    let list_url = format!("http://127.0.0.1:{}/json/list", endpoint.port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserWorkspaceError::Launch(
                "timed out waiting for exact background page target".into(),
            ));
        }
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "private macOS browser supervisor exited while page target became ready: {status}"
                )));
            }
            Err(error) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "cannot inspect private macOS browser supervisor: {error}"
                )));
            }
        }
        let targets = fetch_bounded_cdp_json_before(
            &client,
            &list_url,
            "supervised CDP target list",
            deadline,
        )
        .await?;
        if let Some(target) = exact_page_target(&targets, target_id) {
            let websocket = target
                .get("webSocketDebuggerUrl")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    BrowserWorkspaceError::Launch(
                        "exact background page target had no debugger URL".into(),
                    )
                })?;
            if exact_loopback_websocket_url(
                websocket,
                endpoint.port,
                &format!("/devtools/page/{target_id}"),
            ) {
                return Ok(websocket.to_string());
            }
            return Err(BrowserWorkspaceError::Launch(
                "exact background page debugger URL was not its loopback target".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn single_candidate(
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
    pid: i32,
) -> Result<crate::macos_monitor::placement::Candidate, BrowserWorkspaceError> {
    for attempt in 0..3 {
        let receipt = bus
            .macos_monitors
            .request(
                crate::macos_monitor::Action::Window(crate::macos_monitor::WindowAction::List {
                    pid,
                }),
                authority.clone(),
            )
            .await
            .map_err(BrowserWorkspaceError::Launch)?;
        let candidates = match &receipt.value {
            crate::macos_monitor::Value::Window(crate::macos_monitor::WindowValue::Candidates(
                candidates,
            )) => candidates.clone(),
            _ => {
                return Err(BrowserWorkspaceError::Launch(
                    "unexpected macOS browser window-list result".into(),
                ));
            }
        };
        if !receipt.commit() {
            return Err(BrowserWorkspaceError::Launch(
                "macOS browser window-list receipt expired".into(),
            ));
        }
        if candidates.len() == 1 {
            return Ok(candidates.into_iter().next().expect("length checked"));
        }
        if candidates.len() > 1 {
            return Err(BrowserWorkspaceError::Launch(
                "managed macOS browser exposed more than one candidate window".into(),
            ));
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Err(BrowserWorkspaceError::Launch(
        "managed macOS browser did not expose exactly one bounded window".into(),
    ))
}

async fn bind_and_place(
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
    selector: &str,
    browser_pid: u32,
    birth: (u64, u32),
) -> Result<MacosBindingGuard, BrowserWorkspaceError> {
    require_macos_monitor_live(bus, selector, authority).await?;
    let pid = i32::try_from(browser_pid).map_err(|_| {
        BrowserWorkspaceError::Launch("managed browser PID does not fit macOS AX".into())
    })?;
    let candidate = single_candidate(bus, authority, pid).await?;
    if candidate.identity.pid != pid
        || (
            candidate.identity.start_seconds,
            candidate.identity.start_micros,
        ) != birth
    {
        return Err(BrowserWorkspaceError::Launch(
            "browser process generation changed before binding".into(),
        ));
    }
    let receipt = bus
        .macos_monitors
        .request(
            crate::macos_monitor::Action::Window(crate::macos_monitor::WindowAction::Bind {
                selector: selector.to_string(),
                identity: candidate.identity,
                candidate: candidate.candidate,
            }),
            authority.clone(),
        )
        .await
        .map_err(BrowserWorkspaceError::Launch)?;
    let binding = match &receipt.value {
        crate::macos_monitor::Value::Window(crate::macos_monitor::WindowValue::Bound(binding)) => {
            binding.binding.clone()
        }
        _ => {
            return Err(BrowserWorkspaceError::Launch(
                "unexpected macOS browser bind result".into(),
            ));
        }
    };
    if let Some(task) = &authority.task {
        task.record_binding(&binding)
            .map_err(BrowserWorkspaceError::Launch)?;
    }
    if !receipt.commit() {
        return Err(BrowserWorkspaceError::Launch(
            "macOS browser binding receipt expired".into(),
        ));
    }
    let guard = MacosBindingGuard::new(binding.clone(), bus.clone(), authority.clone());
    let receipt = bus
        .macos_monitors
        .request(
            crate::macos_monitor::Action::Window(crate::macos_monitor::WindowAction::Place {
                binding,
                bounds: crate::macos_monitor::placement::Bounds {
                    // Leave room for the macOS top system inset. Requesting
                    // y=0 is clamped by WindowServer and cannot verify exactly.
                    x: 40.0,
                    y: 40.0,
                    width: 720.0,
                    height: 530.0,
                },
            }),
            authority.clone(),
        )
        .await
        .map_err(BrowserWorkspaceError::Launch)?;
    let (verified, evidence) = match &receipt.value {
        crate::macos_monitor::Value::Window(crate::macos_monitor::WindowValue::Placed(result)) => {
            (result.verified(), serde_json::json!({"placement":result}))
        }
        crate::macos_monitor::Value::Window(
            crate::macos_monitor::WindowValue::PlacementUnconfirmed { result, error },
        ) => (false, serde_json::json!({"placement":result,"error":error})),
        _ => (
            false,
            serde_json::json!({"error":"unexpected placement result"}),
        ),
    };
    if let (
        Some(task),
        crate::macos_monitor::Value::Window(crate::macos_monitor::WindowValue::Placed(result)),
    ) = (&authority.task, &receipt.value)
    {
        task.record_placement(result)
            .map_err(BrowserWorkspaceError::Launch)?;
    }
    if !receipt.commit() {
        return Err(BrowserWorkspaceError::Launch(format!(
            "macOS browser placement receipt expired; no retry attempted; {evidence}"
        )));
    }
    if !verified {
        return Err(BrowserWorkspaceError::Launch(format!(
            "macOS browser window placement was not verified; no retry attempted; {evidence}"
        )));
    }
    require_macos_monitor_live(bus, selector, authority).await?;
    Ok(guard)
}

struct PrelaunchProfile(Option<PathBuf>);
impl Drop for PrelaunchProfile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub(super) async fn launch(
    workspace: &BrowserWorkspace,
    profile_dir: &Path,
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
    selector: &str,
) -> Result<(Child, CdpLaunch), BrowserWorkspaceError> {
    let mut profile_guard = PrelaunchProfile(Some(profile_dir.to_path_buf()));
    let executable = find_intendant_managed_chromium_executable()
        .map(|path| ChromiumExecutable {
            path,
            source: "intendant-managed-cache".to_string(),
        })
        .ok_or_else(|| {
            BrowserWorkspaceError::Launch(
                "macos_virtual browser workspace requires Intendant-managed Chrome for Testing; run intendant setup browsers"
                    .into(),
            )
        })?;
    let bundle = cft_bundle_path(&executable.path)?;
    let navigation = launch_policy::navigation(workspace.url.as_deref())
        .map_err(|error| BrowserWorkspaceError::Launch(error.into()))?
        .unwrap_or("about:blank");

    let mut arguments: Vec<String> = vec![
        format!("--user-data-dir={}", profile_dir.display()),
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-startup-window".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-breakpad".into(),
        "--disable-client-side-phishing-detection".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-domain-reliability".into(),
        "--disable-features=AutofillServerCommunication,CertificateTransparencyComponentUpdater,MediaRouter,OptimizationHints,OptimizationGuideModelDownloading,Translate".into(),
        "--disable-popup-blocking".into(),
        "--disable-sync".into(),
        "--metrics-recording-only".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        "--disable-extensions".into(),
        "--force-renderer-accessibility=complete".into(),
    ];
    if launch_policy::current() {
        arguments.push("--test-type=gpu".into());
    }
    if arguments.len() > 64 || arguments.iter().map(String::len).sum::<usize>() > 16 * 1024 {
        return Err(BrowserWorkspaceError::Launch(
            "managed macOS browser launch argument bounds exceeded".into(),
        ));
    }

    clear_stale_devtools_active_port(profile_dir)?;
    let current_exe = std::env::current_exe().map_err(|error| {
        BrowserWorkspaceError::Launch(format!(
            "cannot resolve current Intendant binary for private browser supervisor: {error}"
        ))
    })?;
    let mut command = tokio::process::Command::new(current_exe);
    command
        .env_clear()
        .env("HOME", crate::platform::home_dir())
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", std::env::temp_dir())
        .env("LANG", "en_US.UTF-8")
        .arg("--private-macos-browser-workspace-v1")
        .arg(&bundle)
        .args(&arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // No kill_on_drop: dropping the child closes stdin. The private supervisor
    // then terminates the exact NSRunningApplication it owns before exiting.
    let mut child = command.spawn().map_err(|error| {
        BrowserWorkspaceError::Launch(format!(
            "failed to launch private macOS browser supervisor: {error}"
        ))
    })?;
    profile_guard.0 = None;
    let mut known_browser_pid = None;
    let result = async {
        let browser_pid = supervised_browser_pid(&mut child).await?;
        known_browser_pid = Some(browser_pid);
        if !crate::platform::process_alive(browser_pid) {
            return Err(BrowserWorkspaceError::Launch(
                "supervised Chrome for Testing exited immediately after launch".into(),
            ));
        }
        let birth =
            crate::platform::macos_process_birth(i32::try_from(browser_pid).map_err(|_| {
                BrowserWorkspaceError::Launch("browser PID outside macOS range".into())
            })?)
            .ok_or_else(|| {
                BrowserWorkspaceError::Launch("browser process birth unavailable".into())
            })?;
        let endpoint = browser_endpoint(&mut child, profile_dir).await?;
        let target_id = create_background_target(&endpoint, navigation).await?;
        let page_ws = exact_page_websocket(&mut child, &endpoint, &target_id).await?;
        if let Some(task) = &authority.task {
            task.record_process(browser_pid, birth)
                .map_err(BrowserWorkspaceError::Launch)?;
        }
        let guard = bind_and_place(bus, authority, selector, browser_pid, birth).await?;
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "private macOS browser supervisor exited after placement: {status}"
                )));
            }
            Err(error) => {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "cannot inspect private macOS browser supervisor after placement: {error}"
                )));
            }
        }
        if !crate::platform::process_alive(browser_pid) {
            return Err(BrowserWorkspaceError::Launch(
                "supervised Chrome for Testing exited after placement".into(),
            ));
        }
        require_macos_monitor_live(bus, selector, authority).await?;

        Ok(CdpLaunch {
            executable,
            launch_arguments: arguments,
            process_id: Some(browser_pid),
            port: endpoint.port,
            web_socket_debugger_url: Some(page_ws),
            target_id: Some(target_id),
            extension_runtime_id: None,
            macos_binding_guard: Some(guard),
        })
    }
    .await;
    match result {
        Ok(cdp) => Ok((child, cdp)),
        Err(error) => {
            if let Err(cleanup) = stop(&mut child).await {
                let mut pending = workspace.clone();
                pending.status = BrowserWorkspaceStatus::Error;
                pending.process_id = known_browser_pid;
                pending.message = Some(format!("{error}; cleanup unconfirmed: {cleanup}"));
                global_registry().write().await.insert(pending, Some(child));
                return Err(BrowserWorkspaceError::CleanupPending {
                    workspace_id: workspace.id.clone(),
                    message: format!("{error}; {cleanup}"),
                });
            }
            if let Err(cleanup) = fs::remove_dir_all(profile_dir) {
                return Err(BrowserWorkspaceError::Launch(format!(
                    "{error}; browser stopped but private profile cleanup failed: {cleanup}"
                )));
            }
            Err(error)
        }
    }
}

/// Close the retained supervisor pipe, then wait for its exact-app cleanup
/// receipt. Never kill a numeric browser PID or the supervisor while an
/// NSWorkspace launch completion is still outstanding.
pub(super) async fn stop(child: &mut Child) -> Result<(), BrowserWorkspaceError> {
    drop(child.stdin.take());
    tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .map_err(|_| {
            BrowserWorkspaceError::Launch(
                "supervisor still owns pending cleanup after 15 seconds".into(),
            )
        })?
        .map_err(|e| BrowserWorkspaceError::Launch(format!("supervisor wait failed: {e}")))?;
    let Some(stdout) = child.stdout.as_mut() else {
        return Err(BrowserWorkspaceError::Launch(
            "missing supervisor cleanup receipt".into(),
        ));
    };
    let mut bytes = Vec::new();
    stdout
        .take(4097)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| BrowserWorkspaceError::Launch(format!("cleanup receipt read failed: {e}")))?;
    if bytes.len() > 4096 {
        return Err(BrowserWorkspaceError::Launch(
            "oversized cleanup receipt".into(),
        ));
    }
    let last = bytes.split(|b| *b == b'\n').rfind(|line| !line.is_empty());
    let verified = last
        .and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .is_some_and(|reply| reply == serde_json::json!({"cleanup_verified":true}));
    if !verified {
        return Err(BrowserWorkspaceError::Launch(
            "exact native browser cleanup was not verified".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn supervisor_pid_receipt_is_closed_and_duplicate_safe() {
        assert_eq!(parse_supervisor_pid(br#"{"pid":123}"#).unwrap(), 123);
        for invalid in [
            br#"{"pid":0}"#.as_slice(),
            br#"{"pid":-1}"#,
            br#"{"pid":2147483648}"#,
            br#"{"pid":1,"pid":2}"#,
            br#"{"pid":1,"extra":true}"#,
            br#"{"pid":"1"}"#,
            br#"null"#,
        ] {
            assert!(parse_supervisor_pid(invalid).is_err(), "{invalid:?}");
        }
    }

    #[tokio::test]
    async fn background_target_is_single_nonactivating_request_without_input_retry() {
        for reply in [
            serde_json::json!({"id":1,"result":{"targetId":"exact-page"}}),
            serde_json::json!({"id":2,"result":{"targetId":"wrong"}}),
            serde_json::json!({"id":1,"error":{"message":"refused"}}),
            serde_json::json!({"id":1,"result":{"targetId":"../foreign"}}),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = DevToolsActivePort {
                port: listener.local_addr().unwrap().port(),
                browser_websocket_path: "/devtools/browser/test-owned".into(),
            };
            let expected_success = reply["result"]["targetId"] == "exact-page";
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let message = socket.next().await.unwrap().unwrap();
                let request: serde_json::Value =
                    serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(
                    request,
                    serde_json::json!({"id":1,"method":"Target.createTarget",
                    "params":{"url":"about:blank","newWindow":true,"background":true,"width":720,"height":530}})
                );
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                // Neither outcome permits a second create or an input command.
                if let Ok(Some(Ok(message))) =
                    tokio::time::timeout(Duration::from_secs(2), socket.next()).await
                {
                    assert!(!message.is_text());
                }
            });
            let result = create_background_target(&endpoint, "about:blank").await;
            assert_eq!(result.is_ok(), expected_success);
            server.await.unwrap();
        }
    }

    use super::*;

    #[test]
    fn cft_bundle_derivation_refuses_non_bundle_shapes() {
        let good = Path::new(
            "/cache/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        );
        assert_eq!(
            cft_bundle_path(good).unwrap(),
            Path::new("/cache/Google Chrome for Testing.app")
        );
        for bad in [
            Path::new("/cache/chrome"),
            Path::new("/cache/Chrome.app/MacOS/Chrome"),
            Path::new("/cache/Chrome/Contents/MacOS/Chrome"),
        ] {
            assert!(cft_bundle_path(bad).is_err());
        }
    }

    #[test]
    fn target_id_is_bounded_and_canonical() {
        assert!(valid_target_id("ABCdef_012-xyz"));
        assert!(!valid_target_id(""));
        assert!(!valid_target_id("x/y"));
        assert!(!valid_target_id(&"a".repeat(129)));
    }
}
