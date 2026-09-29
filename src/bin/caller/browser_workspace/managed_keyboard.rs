//! Browser-page keyboard commands; never OS input, activation, or clipboard access.
//! Exact workspace/monitor/window authority is checked separately from the CDP
//! connection. A protocol acknowledgement does not prove application effects.
use super::*;
#[cfg(any(target_os = "macos", test))]
use futures_util::{SinkExt as _, StreamExt as _};
use schemars::JsonSchema;
use serde_json::{json, Value};
#[cfg(any(target_os = "macos", test))]
use std::collections::BTreeSet;
#[cfg(any(target_os = "macos", test))]
use tokio::net::TcpStream;
#[cfg(any(target_os = "macos", test))]
use tokio_tungstenite::{
    tungstenite::{protocol::WebSocketConfig, Message},
    WebSocketStream,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    /// Exact daemon-owned browser workspace. No caller-selected CDP endpoint.
    pub workspace_id: String,
    /// Canonical UUID, consumed once even when dispatch becomes uncertain.
    pub request_id: String,
    pub action: Action,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    /// Text insertion (including Unicode); not physical keypress simulation.
    InsertText { text: String },
    /// One complete browser-page key pair; no held keys or system shortcuts.
    Key { key: Key },
    /// Browser editing command. Never reads/writes the system clipboard.
    SelectAll,
}
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
pub(crate) enum Key {
    Enter,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Escape,
    Space,
}
#[derive(Debug, Serialize)]
pub(crate) struct ResultReceipt {
    pub ok: bool,
    pub request_id: String,
    pub commands_attempted: usize,
    pub commands_acknowledged: usize,
    pub effects_verified: bool,
    pub effects_unconfirmed: bool,
    pub error: Option<String>,
    pub mechanism: &'static str,
}
impl ResultReceipt {
    fn new(id: String) -> Self {
        Self {
            ok: false,
            request_id: id,
            commands_attempted: 0,
            commands_acknowledged: 0,
            effects_verified: false,
            effects_unconfirmed: false,
            error: None,
            mechanism: "managed_page_cdp",
        }
    }
}
fn admission(request: &Request, owner: bool) -> Result<(), String> {
    if !owner {
        return Err("managed browser keyboard requires an owner surface".into());
    }
    if request.workspace_id.is_empty() || request.workspace_id.len() > 128 {
        return Err("invalid browser workspace identity".into());
    }
    let id =
        uuid::Uuid::parse_str(&request.request_id).map_err(|_| "invalid keyboard request UUID")?;
    if id.to_string() != request.request_id {
        return Err("keyboard UUID must be canonical".into());
    }
    if let Action::InsertText { text } = &request.action {
        if text.is_empty() || text.len() > 4096 || text.contains('\0') {
            return Err("keyboard text must contain 1..4096 UTF-8 bytes and no NUL".into());
        }
    }
    Ok(())
}
fn input_commands(action: &Action) -> Vec<(&'static str, Value)> {
    if let Action::InsertText { text } = action {
        return vec![("Input.insertText", json!({"text":text}))];
    }
    let (key, code, vk, text, modifiers) = match action {
        Action::Key { key } => match key {
            Key::Enter => ("Enter", "Enter", 13, "\r", 0),
            Key::Tab => ("Tab", "Tab", 9, "", 0),
            Key::ShiftTab => ("Tab", "Tab", 9, "", 8),
            Key::Backspace => ("Backspace", "Backspace", 8, "", 0),
            Key::Delete => ("Delete", "Delete", 46, "", 0),
            Key::ArrowLeft => ("ArrowLeft", "ArrowLeft", 37, "", 0),
            Key::ArrowRight => ("ArrowRight", "ArrowRight", 39, "", 0),
            Key::ArrowUp => ("ArrowUp", "ArrowUp", 38, "", 0),
            Key::ArrowDown => ("ArrowDown", "ArrowDown", 40, "", 0),
            Key::Home => ("Home", "Home", 36, "", 0),
            Key::End => ("End", "End", 35, "", 0),
            Key::PageUp => ("PageUp", "PageUp", 33, "", 0),
            Key::PageDown => ("PageDown", "PageDown", 34, "", 0),
            Key::Escape => ("Escape", "Escape", 27, "", 0),
            Key::Space => (" ", "Space", 32, " ", 0),
        },
        Action::SelectAll => ("a", "KeyA", 65, "", 4),
        Action::InsertText { .. } => unreachable!(),
    };
    let mut down = json!({"type":"keyDown","key":key,"code":code,
        "windowsVirtualKeyCode":vk,"modifiers":modifiers,"autoRepeat":false});
    if !text.is_empty() {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    if matches!(action, Action::SelectAll) {
        down["commands"] = json!(["selectAll"]);
    }
    vec![
        ("Input.dispatchKeyEvent", down),
        (
            "Input.dispatchKeyEvent",
            json!({"type":"keyUp","key":key,"code":code,"windowsVirtualKeyCode":vk,
               "modifiers":modifiers,"autoRepeat":false}),
        ),
    ]
}
#[cfg(any(target_os = "macos", test))]
pub(super) struct Client {
    socket: WebSocketStream<TcpStream>,
    next: u64,
    bytes: usize,
    contexts: BTreeMap<i64, Value>,
}
#[cfg(any(target_os = "macos", test))]
impl Client {
    pub(super) async fn connect(port: u16, url: &str, target: &str) -> Result<Self, String> {
        Self::connect_exact(port, url, &format!("/devtools/page/{target}")).await
    }
    async fn connect_exact(port: u16, url: &str, path: &str) -> Result<Self, String> {
        if !exact_loopback_websocket_url(url, port, path) {
            return Err("keyboard endpoint is not the exact owned page".into());
        }
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            // Numeric loopback socket, no proxy, DNS, redirects or generic URLs.
            let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .map_err(|_| "keyboard loopback connection failed")?;
            let config = WebSocketConfig::default()
                .max_message_size(Some(1024 * 1024))
                .max_frame_size(Some(1024 * 1024));
            let (socket, _) =
                tokio_tungstenite::client_async_with_config(url, stream, Some(config))
                    .await
                    .map_err(|_| "keyboard WebSocket handshake failed")?;
            Ok::<_, String>(Self {
                socket,
                next: 0,
                bytes: 0,
                contexts: BTreeMap::new(),
            })
        })
        .await;
        result.map_err(|_| "keyboard connection deadline".to_string())?
    }
    pub(super) async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        tokio::time::timeout(Duration::from_secs(2), self.call_inner(method, params))
            .await
            .map_err(|_| "keyboard CDP deadline; effects unconfirmed".to_string())?
    }
    async fn call_inner(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.socket
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| "keyboard CDP send failed; effects unconfirmed")?;
        for _ in 0..64 {
            let message = self
                .socket
                .next()
                .await
                .ok_or("keyboard CDP closed")?
                .map_err(|_| "keyboard CDP receive failed")?;
            match message {
                Message::Text(text) => {
                    self.bytes = self.bytes.saturating_add(text.len());
                    if self.bytes > 4 * 1024 * 1024 {
                        return Err("keyboard response byte budget".into());
                    }
                    let reply: Value =
                        serde_json::from_str(&text).map_err(|_| "malformed keyboard CDP reply")?;
                    if reply.get("id").is_none() {
                        match reply["method"].as_str() {
                            Some("Runtime.executionContextCreated") => {
                                let context = &reply["params"]["context"];
                                let id =
                                    context["id"].as_i64().ok_or("invalid execution context")?;
                                if self.contexts.len() >= 64 && !self.contexts.contains_key(&id) {
                                    return Err("execution context inventory budget".into());
                                }
                                self.contexts.insert(id, context.clone());
                            }
                            Some("Runtime.executionContextDestroyed") => {
                                if let Some(id) = reply["params"]["executionContextId"].as_i64() {
                                    self.contexts.remove(&id);
                                }
                            }
                            Some("Runtime.executionContextsCleared") => self.contexts.clear(),
                            _ => {}
                        }
                        continue;
                    }
                    // A timed-out keyDown reply may arrive during its one keyUp.
                    // Skip stale replies, never replay either command.
                    if reply["id"].as_u64().is_some_and(|n| n < id) {
                        continue;
                    }
                    if reply["id"] != id {
                        return Err("keyboard CDP reply identity mismatch".into());
                    }
                    if reply.get("error").is_some() {
                        return Err("keyboard CDP command refused".into());
                    }
                    return reply
                        .get("result")
                        .cloned()
                        .ok_or("missing keyboard CDP result".into());
                }
                Message::Ping(data) => self
                    .socket
                    .send(Message::Pong(data))
                    .await
                    .map_err(|_| "keyboard pong failed")?,
                _ => return Err("unexpected keyboard CDP frame".into()),
            }
        }
        Err("keyboard response message budget".into())
    }
}

#[cfg(target_os = "macos")]
const FOREGROUND_PROBE: &str = "--private-macos-keyboard-foreground-v1";

/// The NSWorkspace shim is deliberately main-thread-only. Run this read-only
/// probe before normal startup, without config, credentials, sockets or input.
#[cfg(target_os = "macos")]
pub(crate) fn intercept_foreground_probe() -> Option<Result<(), String>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_none_or(|arg| arg != FOREGROUND_PROBE) {
        return None;
    }
    Some((|| {
        if args.len() != 1 {
            return Err("unexpected foreground probe arguments".into());
        }
        let pid =
            crate::platform::macos_foreground_pid().ok_or("foreground process unavailable")?;
        let birth = crate::platform::macos_process_birth(pid)
            .ok_or("foreground process birth unavailable")?;
        println!("{}", json!({"pid":pid,"seconds":birth.0,"micros":birth.1}));
        Ok(())
    })())
}

#[cfg(any(target_os = "macos", test))]
fn parse_foreground(bytes: &[u8]) -> Result<(i32, (u64, u32)), String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        pid: i32,
        seconds: u64,
        micros: u32,
    }
    if bytes.len() > 256 {
        return Err("foreground receipt exceeds budget".into());
    }
    let value: Receipt = serde_json::from_slice(bytes).map_err(|_| "invalid foreground receipt")?;
    if value.pid <= 0 || value.seconds == 0 || value.micros >= 1_000_000 {
        return Err("invalid foreground process identity".into());
    }
    Ok((value.pid, (value.seconds, value.micros)))
}
#[cfg(target_os = "macos")]
pub(super) async fn foreground() -> Result<(i32, (u64, u32)), String> {
    use tokio::io::AsyncReadExt;
    let exe = std::env::current_exe().map_err(|_| "foreground probe executable unavailable")?;
    let mut child = tokio::process::Command::new(exe)
        .arg(FOREGROUND_PROBE)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "foreground probe could not start")?;
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        let mut output = child
            .stdout
            .take()
            .ok_or("foreground probe pipe missing")?
            .take(257);
        output
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "foreground probe read failed")?;
        let status = child
            .wait()
            .await
            .map_err(|_| "foreground probe wait failed")?;
        if !status.success() {
            return Err("foreground probe refused".to_string());
        }
        parse_foreground(&bytes)
    })
    .await;
    result.map_err(|_| "foreground probe deadline".to_string())?
}
#[cfg(target_os = "macos")]
pub(super) async fn validate_native(
    workspace: &BrowserWorkspace,
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
) -> Result<(), String> {
    use crate::macos_monitor::{
        Action as MonitorAction, Value as MonitorValue, WindowAction, WindowValue,
    };
    authority.check().await?;
    if !authority.owner_surface
        && !authority
            .task
            .as_ref()
            .is_some_and(|p| p.allows_page_input(workspace))
    {
        return Err("managed browser keyboard requires owner or exact task authority".into());
    }
    if workspace.status != BrowserWorkspaceStatus::Ready
        || workspace.provider != BrowserWorkspaceProvider::Cdp
    {
        return Err("keyboard requires a ready managed browser workspace".into());
    }
    let selector = workspace
        .display_target
        .as_deref()
        .ok_or("keyboard workspace has no display binding")?;
    if !crate::macos_monitor::exact_selector(selector) {
        return Err("keyboard requires exact macOS owned monitor".into());
    }
    require_macos_monitor_live(bus, selector, authority)
        .await
        .map_err(|e| e.to_string())?;
    let binding = workspace
        .macos_window_binding
        .as_ref()
        .ok_or("keyboard workspace lacks native window binding")?;
    let pid = workspace
        .process_id
        .ok_or("keyboard workspace lacks supervised browser PID")?;
    if foreground().await?.0 as u32 == pid {
        return Err("keyboard browser became foreground".into());
    }
    let receipt = bus
        .macos_monitors
        .request(
            MonitorAction::Window(WindowAction::ValidatePageWindow {
                binding: binding.clone(),
            }),
            authority.clone(),
        )
        .await?;
    // The retained native window and its protection/ancestry/geometry remain
    // validated. OS AXFocusedUIElement is not the CDP page's input receiver:
    // a deliberately non-key browser window can correctly have no OS receiver.
    let valid = match &receipt.value {
        MonitorValue::Window(WindowValue::ValidatedPageWindow(observation)) => {
            observation.ax.validate().is_ok()
                && observation.cg.validate().is_ok()
                && observation.ax.close(observation.cg)
                && authority
                    .task
                    .as_ref()
                    .is_none_or(|p| p.checks_window(*observation))
        }
        _ => false,
    };
    if !receipt.commit() || !valid {
        return Err("native browser window validation receipt unavailable".into());
    }

    Ok(())
}
#[cfg(any(target_os = "macos", test))]
fn claim_request(ledger: &mut BTreeSet<(String, String)>, request: &Request) -> Result<(), String> {
    let identity = (request.workspace_id.clone(), request.request_id.clone());
    if ledger.contains(&identity) {
        return Err("duplicate keyboard request; input will not be replayed".into());
    }
    // A fixed daemon-lifetime bound, never evict a live request to admit a replay.
    if ledger.len() >= 8192 {
        return Err("keyboard request ledger full; no input attempted".into());
    }
    ledger.insert(identity);
    Ok(())
}
#[cfg(target_os = "macos")]
static REQUESTS: OnceLock<tokio::sync::Mutex<BTreeSet<(String, String)>>> = OnceLock::new();

pub(crate) async fn execute(
    request: Request,
    bus: &EventBus,
    authority: crate::macos_monitor::Authority,
) -> ResultReceipt {
    let mut result = ResultReceipt::new(request.request_id.clone());
    let task_workspace = if let Some(task) = authority.task.as_ref() {
        global_registry()
            .read()
            .await
            .workspaces
            .get(&request.workspace_id)
            .is_some_and(|w| task.uses_workspace(w))
    } else {
        false
    };
    if let Err(error) = admission(&request, authority.owner_surface || task_workspace) {
        result.error = Some(error);
        return result;
    }
    let _ = input_commands(&request.action);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bus;
        result.error = Some("managed background browser keyboard requires macOS".into());
        result
    }
    #[cfg(target_os = "macos")]
    {
        let lane = match bus.macos_monitors.workspace_lane.clone().try_lock_owned() {
            Ok(lane) => lane,
            Err(_) => {
                result.error = Some("browser workspace busy; no keyboard input attempted".into());
                return result;
            }
        };
        let bus = bus.clone();
        let request_id = request.request_id.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        // Once mutation starts this owned worker completes the ONE key pair even
        // if its requester disconnects; cancellation never replays an input.
        tokio::spawn(async move {
            let _lane = lane;
            if send.is_closed() {
                return;
            }
            let attempted = execute_inner(&request, &bus, &authority, &send, &mut result).await;
            if let Err(error) = attempted {
                result.error = Some(error);
            }
            result.ok = result.error.is_none();
            result.effects_unconfirmed = result.commands_attempted > 0;
            let _ = send.send(result);
        });
        receive.await.unwrap_or_else(|_| {
            let mut result = ResultReceipt::new(request_id);
            result.error =
                Some("keyboard worker ended without receipt; inspect before another action".into());
            result.effects_unconfirmed = true;
            result
        })
    }
}
#[cfg(target_os = "macos")]
pub(super) async fn owned_workspace(id: &str) -> Result<BrowserWorkspace, String> {
    let registry = global_registry();
    let mut registry = registry.write().await;
    let workspace = registry
        .workspaces
        .get(id)
        .cloned()
        .ok_or("unknown keyboard workspace")?;
    let child = registry
        .children
        .get_mut(id)
        .ok_or("owned browser supervisor missing")?;
    if child
        .try_wait()
        .map_err(|_| "browser supervisor state unavailable")?
        .is_some()
    {
        return Err("owned browser supervisor stopped".into());
    }
    Ok(workspace)
}

#[cfg(target_os = "macos")]
pub(super) async fn connect_owned_page(workspace: &BrowserWorkspace) -> Result<Client, String> {
    let port = workspace
        .debugging_port
        .ok_or("missing browser debugging port")?;
    let target = workspace
        .active_target_id
        .as_deref()
        .ok_or("missing exact browser page target")?;
    let url = workspace
        .cdp_ws_url
        .as_deref()
        .ok_or("missing exact browser page endpoint")?;
    let profile = workspace
        .profile_dir
        .as_deref()
        .ok_or("missing owned browser profile")?;
    let active = read_devtools_active_port(Path::new(profile))
        .map_err(|e| e.to_string())?
        .ok_or("owned profile endpoint disappeared")?;
    if active.port != port {
        return Err("owned profile endpoint changed".into());
    }
    let browser_url = format!("ws://127.0.0.1:{port}{}", active.browser_websocket_path);
    let mut browser =
        Client::connect_exact(port, &browser_url, &active.browser_websocket_path).await?;
    let processes = browser.call("SystemInfo.getProcessInfo", json!({})).await?;
    let processes = processes["processInfo"]
        .as_array()
        .ok_or("browser process inventory unavailable")?;
    let browser_pids = processes
        .iter()
        .filter(|row| row["type"] == "browser")
        .collect::<Vec<_>>();
    if browser_pids.len() != 1
        || browser_pids[0]["id"].as_u64() != workspace.process_id.map(u64::from)
    {
        return Err("CDP endpoint is not owned by the retained native browser process".into());
    }
    let mut client = Client::connect(port, url, target).await?;
    validate_original_page(&mut client, workspace).await?;
    Ok(client)
}

#[cfg(target_os = "macos")]
pub(super) async fn validate_original_page(
    client: &mut Client,
    workspace: &BrowserWorkspace,
) -> Result<(), String> {
    let target = workspace
        .active_target_id
        .as_deref()
        .ok_or("missing exact browser page target")?;
    let info = client
        .call("Target.getTargetInfo", json!({"targetId":target}))
        .await?;
    if info["targetInfo"]["targetId"] != target || info["targetInfo"]["type"] != "page" {
        return Err("keyboard target identity changed".into());
    }
    let inventory = client.call("Target.getTargets", json!({})).await?;
    let pages = inventory["targetInfos"]
        .as_array()
        .ok_or("missing browser target inventory")?
        .iter()
        .filter(|row| row["type"] == "page")
        .collect::<Vec<_>>();
    if pages.len() != 1 || pages[0]["targetId"] != target {
        return Err("keyboard requires exactly the managed workspace's original page".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn execute_inner(
    request: &Request,
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
    response: &tokio::sync::oneshot::Sender<ResultReceipt>,
    result: &mut ResultReceipt,
) -> Result<(), String> {
    let registry = global_registry();
    let workspace = owned_workspace(&request.workspace_id).await?;
    validate_native(&workspace, bus, authority).await?;
    {
        let mut ledger = REQUESTS.get_or_init(Default::default).lock().await;
        claim_request(&mut ledger, request)?;
    }
    let mut client = connect_owned_page(&workspace).await?;
    let receiver = page_receiver(&mut client, &request.action).await?;
    validate_native(&workspace, bus, authority).await?;
    let before = foreground().await?;
    if page_receiver(&mut client, &request.action).await? != receiver {
        return Err("focused page receiver or document changed before keyboard dispatch".into());
    }
    if before.0 as u32 == workspace.process_id.unwrap_or(0) {
        return Err("browser became foreground".into());
    }
    if response.is_closed() {
        return Err("keyboard request cancelled before input".into());
    }
    // Recheck the authenticated task immediately before its first key edge.
    // Once down may be sent, dispatch_input owns the matching release.
    authority.check().await?;
    if let Err(error) = dispatch_input(&mut client, &request.action, result).await {
        let mut registry = registry.write().await;
        if let Some(current) = registry.workspaces.get_mut(&workspace.id) {
            current.status = BrowserWorkspaceStatus::Error;
            current.lease = None;
            current.message = Some(
                "keyboard delivery uncertain; inspect and close the workspace before further input"
                    .into(),
            );
            current.updated_at = now_string();
        }
        return Err(error);
    }
    if foreground().await? != before {
        return Err(
            "foreground context changed during browser keyboard input; attribution unknown".into(),
        );
    }
    require_macos_monitor_live(
        bus,
        workspace.display_target.as_deref().unwrap_or(""),
        authority,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_page_node(node: &Value, ax: &Value, action: &Action) -> Result<i64, String> {
    let backend = node["backendNodeId"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or("focused page node identity missing")?;
    let tag = node["nodeName"]
        .as_str()
        .ok_or("focused page node type missing")?;
    if node["nodeType"] != 1
        || !matches!(
            tag,
            "INPUT" | "TEXTAREA" | "BUTTON" | "DIV" | "SPAN" | "A" | "SELECT"
        )
    {
        return Err("focused page node is unsupported (including frame/shadow routing)".into());
    }
    if node.get("contentDocument").is_some() {
        return Err("focused frame host is unsupported".into());
    }
    if let Some(roots) = node.get("shadowRoots") {
        let roots = roots
            .as_array()
            .ok_or("invalid focused shadow-root metadata")?;
        // Standard input controls have a browser-owned implementation shadow
        // tree. The actual activeElement is still the inspected INPUT/TEXTAREA/
        // SELECT host. Never accept author-created open/closed shadow routing.
        if roots.len() > 1
            || roots.iter().any(|root| {
                !matches!(tag, "INPUT" | "TEXTAREA" | "SELECT")
                    || root["shadowRootType"] != "user-agent"
                    || root["nodeType"] != 11
                    || root["nodeName"] != "#document-fragment"
            })
        {
            return Err("focused author shadow-root host is unsupported".into());
        }
    }
    let raw = node["attributes"]
        .as_array()
        .ok_or("focused page attributes unavailable")?;
    if raw.len() > 128 || raw.len() % 2 != 0 {
        return Err("focused page attribute budget".into());
    }
    let mut attrs = BTreeMap::new();
    for pair in raw.chunks_exact(2) {
        let name = pair[0].as_str().ok_or("invalid page attribute name")?;
        let value = pair[1].as_str().ok_or("invalid page attribute value")?;
        if name.len() > 128 || value.len() > 4096 || attrs.insert(name, value).is_some() {
            return Err("invalid or excessive page attribute".into());
        }
    }
    if attrs.contains_key("disabled")
        || attrs.contains_key("inert")
        || attrs.contains_key("readonly")
        || attrs
            .get("aria-disabled")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
        || attrs
            .get("aria-readonly")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
    {
        return Err("page receiver is disabled or read-only".into());
    }
    let kind = attrs
        .get("type")
        .copied()
        .unwrap_or("text")
        .to_ascii_lowercase();
    if tag == "INPUT"
        && !matches!(
            kind.as_str(),
            "text"
                | "search"
                | "email"
                | "url"
                | "tel"
                | "number"
                | "button"
                | "submit"
                | "reset"
                | "checkbox"
                | "radio"
        )
    {
        return Err("page receiver is protected or unsupported".into());
    }
    let editable = matches!(tag, "TEXTAREA")
        || (tag == "INPUT"
            && matches!(
                kind.as_str(),
                "text" | "search" | "email" | "url" | "tel" | "number"
            ))
        || attrs
            .get("contenteditable")
            .is_some_and(|v| matches!(*v, "" | "true" | "plaintext-only"));
    if !matches!(action, Action::Key { .. }) && !editable {
        return Err("text/edit input requires an eligible page field".into());
    }
    if ax["backendDOMNodeId"].as_i64() != Some(backend) || ax["ignored"] != false {
        return Err("focused page accessibility identity is unknown or ignored".into());
    }
    let role = ax["role"]["value"]
        .as_str()
        .ok_or("focused page accessibility role missing")?;
    if !matches!(
        role,
        "textbox" | "searchbox" | "button" | "link" | "combobox" | "checkbox" | "radio"
    ) {
        return Err("focused page accessibility role unsupported".into());
    }
    let props = ax["properties"]
        .as_array()
        .ok_or("focused page accessibility state unavailable")?;
    if props.len() > 64 {
        return Err("focused page accessibility state budget".into());
    }
    for prop in props {
        if matches!(
            prop["name"].as_str(),
            Some("disabled" | "readonly" | "protected" | "password")
        ) && prop["value"]["value"] != false
        {
            return Err(
                "focused page receiver has protected or unavailable accessibility state".into(),
            );
        }
    }
    Ok(backend)
}
#[cfg(any(target_os = "macos", test))]
const RECEIVER_WORLD: &str = "__intendant_keyboard_receiver_v1";
#[cfg(any(target_os = "macos", test))]
const ACTIVE_ELEMENT_GETTER: &str =
    "Reflect.apply(Object.getOwnPropertyDescriptor(Document.prototype, 'activeElement').get, document, [])";

#[cfg(any(target_os = "macos", test))]
fn isolated_context(context: &Value, frame: &str) -> Result<String, String> {
    if context["name"] != RECEIVER_WORLD
        || context["auxData"]["frameId"] != frame
        || context["auxData"]["isDefault"] != false
        || context["auxData"]["type"] != "isolated"
    {
        return Err("receiver observation must use the exact isolated page context".into());
    }
    context["uniqueId"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .map(str::to_owned)
        .ok_or("system-unique receiver context missing".into())
}

#[cfg(any(target_os = "macos", test))]
async fn page_receiver(
    client: &mut Client,
    action: &Action,
) -> Result<(String, String, i64), String> {
    let frames = client.call("Page.getFrameTree", json!({})).await?;
    let frame = &frames["frameTree"]["frame"];
    let frame_id = frame["id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or("page frame identity missing")?
        .to_owned();
    let loader_id = frame["loaderId"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or("page document identity missing")?
        .to_owned();
    // CSS :focus may not match while the OS window is deliberately background.
    // Read the DOM's activeElement in a named isolated world: page-installed
    // getters cannot choose the receiver, and no focus setter is ever called.
    client.call("Runtime.enable", json!({})).await?;
    let world = client
        .call(
            "Page.createIsolatedWorld",
            json!({
                "frameId":frame_id,"worldName":RECEIVER_WORLD,"grantUniveralAccess":false
            }),
        )
        .await?;
    let context = world["executionContextId"]
        .as_i64()
        .and_then(|id| client.contexts.get(&id))
        .ok_or("isolated receiver execution context unavailable")?;
    let unique = isolated_context(context, &frame_id)?;
    let evaluated = client
        .call(
            "Runtime.evaluate",
            json!({
                "expression":ACTIVE_ELEMENT_GETTER,"uniqueContextId":unique,
                "objectGroup":"intendant-keyboard-receiver","returnByValue":false,
                "includeCommandLineAPI":false,"generatePreview":false,"userGesture":false,
                "silent":true,"throwOnSideEffect":true,"timeout":500,
                "allowUnsafeEvalBlockedByCSP":false
            }),
        )
        .await?;
    let object = if evaluated.get("exceptionDetails").is_none()
        && evaluated["result"]["type"] == "object"
        && evaluated["result"]["subtype"] == "node"
    {
        evaluated["result"]["objectId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 1024)
    } else {
        None
    }
    .ok_or("no readable active element in the isolated managed page")?;
    let described = client
        .call(
            "DOM.describeNode",
            json!({"objectId":object,"depth":0,"pierce":false}),
        )
        .await;
    let released = client
        .call(
            "Runtime.releaseObjectGroup",
            json!({"objectGroup":"intendant-keyboard-receiver"}),
        )
        .await;
    let described = described?;
    released?;
    let node = &described["node"];
    // DOM/AX node identity and safety are checked independently of the isolated
    // getter. The protocol exposes no caller-supplied script or focus setter.
    let backend = node["backendNodeId"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or("focused page node missing")?;
    let tree = client
        .call(
            "Accessibility.getPartialAXTree",
            json!({"backendNodeId":backend,"fetchRelatives":false}),
        )
        .await?;
    let nodes = tree["nodes"]
        .as_array()
        .ok_or("page accessibility observation missing")?;
    if nodes.len() != 1 {
        return Err("page accessibility observation ambiguous".into());
    }
    if validate_page_node(node, &nodes[0], action)? != backend {
        return Err("page receiver identity changed".into());
    }
    let after = client.call("Page.getFrameTree", json!({})).await?;
    if after["frameTree"]["frame"]["id"] != frame_id
        || after["frameTree"]["frame"]["loaderId"] != loader_id
    {
        return Err("managed page document changed during receiver observation".into());
    }
    Ok((frame_id, loader_id, backend))
}

#[cfg(any(target_os = "macos", test))]
async fn dispatch_input(
    client: &mut Client,
    action: &Action,
    result: &mut ResultReceipt,
) -> Result<(), String> {
    let commands = input_commands(action);
    let mut error = None;
    for (method, params) in commands {
        result.commands_attempted += 1;
        match client.call(method, params).await {
            Ok(_) => result.commands_acknowledged += 1,
            Err(problem) => {
                if error.is_none() {
                    error = Some(problem);
                }
            }
        }
        // Even when keyDown acknowledgement was lost, attempt its one keyUp on
        // this same socket. Never reconnect, retarget, replay, or restore focus.
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(action: Action) -> Request {
        Request {
            workspace_id: "owned".into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            action,
        }
    }
    #[test]
    fn foreground_receipt_is_bounded_exact_and_duplicate_safe() {
        assert_eq!(
            parse_foreground(br#"{"pid":42,"seconds":1,"micros":2}"#).unwrap(),
            (42, (1, 2))
        );
        for value in [
            br#"{"pid":0,"seconds":1,"micros":2}"#.as_slice(),
            br#"{"pid":42,"seconds":0,"micros":2}"#,
            br#"{"pid":42,"seconds":1,"micros":1000000}"#,
            br#"{"pid":42,"pid":43,"seconds":1,"micros":2}"#,
            br#"{"pid":42,"seconds":1,"micros":2,"extra":1}"#,
        ] {
            assert!(parse_foreground(value).is_err());
        }
        assert!(parse_foreground(&vec![b' '; 257]).is_err());
    }
    #[test]
    fn authority_validation_precedes_identifier_and_payload() {
        let r = request(Action::InsertText {
            text: "text".into(),
        });
        assert!(admission(&r, true).is_ok());
        assert!(admission(&r, false).unwrap_err().contains("owner"));
        for text in [String::new(), "\0".into(), "a".repeat(4097)] {
            assert!(admission(&request(Action::InsertText { text }), true).is_err());
        }
        let mut r = request(Action::SelectAll);
        r.request_id = "not-uuid".into();
        assert!(admission(&r, true).is_err());
    }
    #[test]
    fn unicode_text_is_insertion_not_fictitious_physical_keys() {
        let commands = input_commands(&Action::InsertText {
            text: "Hello 東京 🙂\n".into(),
        });
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "Input.insertText");
        assert_eq!(commands[0].1, json!({"text":"Hello 東京 🙂\n"}));
    }
    #[test]
    fn every_admitted_key_is_exactly_one_pair_without_os_modifiers() {
        for key in [
            Key::Enter,
            Key::Tab,
            Key::Backspace,
            Key::Delete,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Escape,
            Key::Space,
        ] {
            let commands = input_commands(&Action::Key { key });
            assert_eq!(commands.len(), 2);
            assert_eq!(commands[0].0, "Input.dispatchKeyEvent");
            assert_eq!(commands[1].0, "Input.dispatchKeyEvent");
            assert_eq!(commands[0].1["type"], "keyDown");
            assert_eq!(commands[1].1["type"], "keyUp");
            for name in ["key", "code", "windowsVirtualKeyCode"] {
                assert_eq!(commands[0].1[name], commands[1].1[name]);
            }
            for (_, params) in commands {
                assert_eq!(params["modifiers"], 0);
                assert_eq!(params["autoRepeat"], false);
            }
        }
    }
    #[test]
    fn select_all_and_reverse_tab_are_bounded_page_edits() {
        let commands = input_commands(&Action::SelectAll);
        assert_eq!(commands[0].1["commands"], json!(["selectAll"]));
        assert_eq!(commands[0].1["modifiers"], 4);
        let reverse = input_commands(&Action::Key { key: Key::ShiftTab });
        assert_eq!(reverse[0].1["modifiers"], 8);
        assert_eq!(reverse[0].1["key"], "Tab");
        for action in [
            json!({"type":"key","key":"Cmd+Q"}),
            json!({"type":"copy"}),
            json!({"type":"key","key":"Tab","modifiers":4}),
            json!({"type":"cdp","method":"Page.bringToFront"}),
        ] {
            assert!(serde_json::from_value::<Action>(action).is_err());
        }
    }
    #[test]
    fn request_ledger_never_replays_or_evicts_to_admit_a_duplicate() {
        let mut ledger = BTreeSet::new();
        let r = request(Action::SelectAll);
        claim_request(&mut ledger, &r).unwrap();
        assert!(claim_request(&mut ledger, &r)
            .unwrap_err()
            .contains("duplicate"));
        for i in 1..8192 {
            ledger.insert(("old".into(), i.to_string()));
        }
        assert!(claim_request(&mut ledger, &request(Action::SelectAll))
            .unwrap_err()
            .contains("full"));
        assert_eq!(ledger.len(), 8192);
    }
    #[tokio::test]
    async fn websocket_rejects_nonloopback_or_other_page_before_connecting() {
        for url in [
            "ws://evil.test:9222/devtools/page/a",
            "ws://127.0.0.1:9223/devtools/page/a",
            "ws://127.0.0.1:9222/devtools/page/b",
            "http://127.0.0.1:9222/devtools/page/a",
        ] {
            assert!(Client::connect(9222, url, "a").await.is_err());
        }
    }
    #[tokio::test]
    async fn actual_wire_pair_matches_fixed_plan_without_activation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut seen = vec![];
            for _ in 0..2 {
                let text = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let data: Value = serde_json::from_str(&text).unwrap();
                socket
                    .send(Message::Text(
                        json!({"id":data["id"],"result":{}}).to_string().into(),
                    ))
                    .await
                    .unwrap();
                seen.push(data);
            }
            seen
        });
        let mut client = Client::connect(
            port,
            &format!("ws://127.0.0.1:{port}/devtools/page/owned"),
            "owned",
        )
        .await
        .unwrap();
        for (method, args) in input_commands(&Action::Key { key: Key::Enter }) {
            client.call(method, args).await.unwrap();
        }
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|v| v["method"] == "Input.dispatchKeyEvent"));
        assert_eq!(seen[0]["params"]["text"], "\r");
        assert_eq!(seen[1]["params"]["type"], "keyUp");
    }
    #[tokio::test]
    async fn rejected_down_still_gets_one_release_without_replaying_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut seen = Vec::new();
            for i in 0..2 {
                let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let value: Value = serde_json::from_str(&message).unwrap();
                let reply = if i == 0 {
                    json!({"id":value["id"],"error":{"message":"fixture refusal"}})
                } else {
                    json!({"id":value["id"],"result":{}})
                };
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                seen.push(value);
            }
            seen
        });
        let mut client = Client::connect(
            port,
            &format!("ws://127.0.0.1:{port}/devtools/page/owned"),
            "owned",
        )
        .await
        .unwrap();
        let mut receipt = ResultReceipt::new("fixture".into());
        assert!(
            dispatch_input(&mut client, &Action::Key { key: Key::Tab }, &mut receipt)
                .await
                .is_err()
        );
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["params"]["type"], "keyDown");
        assert_eq!(seen[1]["params"]["type"], "keyUp");
        assert_eq!(receipt.commands_attempted, 2);
        assert_eq!(receipt.commands_acknowledged, 1);
        assert!(!receipt.effects_verified);
    }
    #[tokio::test]
    async fn lost_socket_has_no_reconnect_or_retry_and_preserves_attempt_count() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let value: Value =
                serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(value["params"]["type"], "keyDown");
            socket.close(None).await.unwrap();
        });
        let mut client = Client::connect(
            port,
            &format!("ws://127.0.0.1:{port}/devtools/page/owned"),
            "owned",
        )
        .await
        .unwrap();
        let mut receipt = ResultReceipt::new("fixture".into());
        assert!(
            dispatch_input(&mut client, &Action::Key { key: Key::Enter }, &mut receipt)
                .await
                .is_err()
        );
        server.await.unwrap();
        assert_eq!(receipt.commands_attempted, 2);
        assert_eq!(receipt.commands_acknowledged, 0);
        assert!(!receipt.effects_verified);
    }
    #[tokio::test]
    async fn raw_and_facade_keyboard_share_extra_display_gates() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::mcp::tests::test_state_with_log_dir(root.path().to_path_buf());
        let bus = EventBus::new();
        let (_home, server) = crate::mcp::tests::test_server(state, bus.clone());
        let id = uuid::Uuid::new_v4().to_string();
        for (tool, args) in [
            (
                "execute_browser_workspace_keyboard",
                json!({"workspace_id":"owned","request_id":id,"action":{"type":"key","key":"Tab"}}),
            ),
            (
                "act",
                json!({"argv":["browser","keyboard","owned",id,"{\"type\":\"key\",\"key\":\"Tab\"}"]}),
            ),
        ] {
            assert!(server.macos_browser_workspace_request(tool, &args).await);
        }
        assert!(bus.macos_monitors.not_started());
    }
    #[tokio::test]
    async fn scoped_execute_refuses_before_registry_or_native_helper() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::mcp::tests::test_state_with_log_dir(root.path().to_path_buf());
        let bus = EventBus::new();
        let authority = crate::macos_monitor::Authority {
            owner_surface: false,
            task: None,
            autonomy: state.read().await.autonomy.clone(),
        };
        let result = execute(request(Action::SelectAll), &bus, authority).await;
        assert!(!result.ok);
        assert_eq!(result.commands_attempted, 0);
        assert!(!result.effects_unconfirmed);
        assert!(result.error.unwrap().contains("owner"));
        assert!(bus.macos_monitors.not_started());
    }
    #[test]
    fn facade_keyboard_is_mutating_and_uses_display_input_gate() {
        use crate::peer::access_policy::PeerOperation;
        assert_eq!(
            crate::mcp::mcp_tool_operation("execute_browser_workspace_keyboard"),
            PeerOperation::DisplayInput
        );
        let args = json!({"argv":["browser","keyboard","owned",uuid::Uuid::new_v4().to_string(),"{\"type\":\"key\",\"key\":\"Tab\"}"]});
        assert_eq!(
            crate::mcp::facade_gate_operation("act", &args),
            Some(PeerOperation::DisplayInput)
        );
        // Invalid lane is authorized at the harmless read floor, then refused
        // by command resolution; it must not resolve to the mutating tool.
        assert_eq!(
            crate::mcp::facade_gate_operation("inspect", &args),
            Some(PeerOperation::StatsRead)
        );
        assert!(crate::mcp::facade_resolved_tool("inspect", &args).is_none());
    }

    #[test]
    fn isolated_receiver_context_refuses_default_foreign_and_missing_identity() {
        let context = json!({"id":73,"uniqueId":"process-unique-context","name":RECEIVER_WORLD,
            "auxData":{"frameId":"frame","isDefault":false,"type":"isolated"}});
        assert_eq!(
            isolated_context(&context, "frame").unwrap(),
            "process-unique-context"
        );
        for field in ["uniqueId", "name", "auxData"] {
            let mut changed = context.clone();
            changed.as_object_mut().unwrap().remove(field);
            assert!(isolated_context(&changed, "frame").is_err());
        }
        let mut changed = context.clone();
        changed["auxData"]["isDefault"] = json!(true);
        assert!(isolated_context(&changed, "frame").is_err());
        assert!(isolated_context(&context, "another-frame").is_err());
        let mut changed = context;
        changed["uniqueId"] = json!("x".repeat(257));
        assert!(isolated_context(&changed, "frame").is_err());
    }

    #[test]
    fn native_control_shadow_tree_is_not_author_shadow_routing() {
        let node = json!({"backendNodeId":17,"nodeName":"INPUT","nodeType":1,"attributes":[],
            "shadowRoots":[{"shadowRootType":"user-agent","nodeType":11,"nodeName":"#document-fragment"}]});
        let ax = json!({"backendDOMNodeId":17,"ignored":false,"role":{"value":"textbox"},"properties":[]});
        assert!(validate_page_node(&node, &ax, &Action::SelectAll).is_ok());
        for kind in ["open", "closed", "unknown"] {
            let mut changed = node.clone();
            changed["shadowRoots"][0]["shadowRootType"] = json!(kind);
            assert!(validate_page_node(&changed, &ax, &Action::SelectAll).is_err());
        }
        let mut changed = node.clone();
        changed["attributes"] = json!(["type", "password"]);
        assert!(validate_page_node(&changed, &ax, &Action::SelectAll).is_err());
        let mut changed = node;
        changed["nodeName"] = json!("DIV");
        assert!(validate_page_node(&changed, &ax, &Action::SelectAll).is_err());
    }

    #[test]
    fn page_receiver_refuses_password_disabled_unknown_and_foreign_nodes() {
        let node = json!({"backendNodeId":17,"nodeName":"INPUT","nodeType":1,"attributes":[]});
        let ax = json!({"backendDOMNodeId":17,"ignored":false,"role":{"value":"textbox"},"properties":[]});
        assert_eq!(
            validate_page_node(&node, &ax, &Action::SelectAll).unwrap(),
            17
        );
        for attributes in [
            json!(["type", "password"]),
            json!(["disabled", ""]),
            json!(["readonly", ""]),
            json!(["aria-disabled", "unknown"]),
            json!(["type", "text", "type", "password"]),
        ] {
            let mut changed = node.clone();
            changed["attributes"] = attributes;
            assert!(validate_page_node(&changed, &ax, &Action::SelectAll).is_err());
        }
        for tag in ["IFRAME", "BODY", "HTML"] {
            let mut changed = node.clone();
            changed["nodeName"] = json!(tag);
            assert!(validate_page_node(&changed, &ax, &Action::SelectAll).is_err());
        }
        let mut changed = ax.clone();
        changed["backendDOMNodeId"] = json!(18);
        assert!(validate_page_node(&node, &changed, &Action::SelectAll).is_err());
        let mut changed = ax.clone();
        changed["ignored"] = json!(true);
        assert!(validate_page_node(&node, &changed, &Action::SelectAll).is_err());
        let mut changed = ax;
        changed["properties"] = json!([{"name":"protected","value":{"value":true}}]);
        assert!(validate_page_node(&node, &changed, &Action::SelectAll).is_err());
    }

    #[tokio::test]
    async fn actual_receiver_observation_uses_unique_isolated_world_and_rejects_navigation() {
        for navigated in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                for (index, method) in [
                    "Page.getFrameTree",
                    "Runtime.enable",
                    "Page.createIsolatedWorld",
                    "Runtime.evaluate",
                    "DOM.describeNode",
                    "Runtime.releaseObjectGroup",
                    "Accessibility.getPartialAXTree",
                    "Page.getFrameTree",
                ]
                .iter()
                .enumerate()
                {
                    let text = socket.next().await.unwrap().unwrap().into_text().unwrap();
                    let request: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(request["method"], *method);
                    let result = match *method {
                        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"frame",
                            "loaderId":if index==7 && navigated {"replaced"} else {"document"}}}}),
                        "Page.createIsolatedWorld" => {
                            assert_eq!(request["params"]["frameId"], "frame");
                            assert_eq!(request["params"]["grantUniveralAccess"], false);
                            let event = json!({"method":"Runtime.executionContextCreated","params":{"context":{
                                "id":73,"uniqueId":"isolated-unique","name":RECEIVER_WORLD,
                                "auxData":{"frameId":"frame","isDefault":false,"type":"isolated"}}}});
                            socket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                            json!({"executionContextId":73})
                        }
                        "Runtime.evaluate" => {
                            assert_eq!(request["params"]["expression"], ACTIVE_ELEMENT_GETTER);
                            assert_eq!(request["params"]["uniqueContextId"], "isolated-unique");
                            assert!(request["params"].get("contextId").is_none());
                            assert_eq!(request["params"]["throwOnSideEffect"], true);
                            assert_eq!(request["params"]["userGesture"], false);
                            assert_eq!(request["params"]["allowUnsafeEvalBlockedByCSP"], false);
                            json!({"result":{"type":"object","subtype":"node","objectId":"exact-object"}})
                        }
                        "DOM.describeNode" => {
                            assert_eq!(request["params"]["objectId"], "exact-object");
                            json!({"node":{"backendNodeId":17,"nodeName":"INPUT","nodeType":1,"attributes":[]}})
                        }
                        "Accessibility.getPartialAXTree" => {
                            assert_eq!(request["params"]["backendNodeId"], 17);
                            json!({"nodes":[{"backendDOMNodeId":17,"ignored":false,
                                "role":{"value":"textbox"},"properties":[]}]})
                        }
                        _ => json!({}),
                    };
                    socket
                        .send(Message::Text(
                            json!({"id":request["id"],"result":result})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            });
            let mut client = Client::connect(
                port,
                &format!("ws://127.0.0.1:{port}/devtools/page/owned"),
                "owned",
            )
            .await
            .unwrap();
            let result = page_receiver(&mut client, &Action::SelectAll).await;
            if navigated {
                assert!(result.unwrap_err().contains("document changed"));
            } else {
                assert_eq!(result.unwrap(), ("frame".into(), "document".into(), 17));
            }
            server.await.unwrap();
        }
    }
}
