//! Complete task-owned entry point. The owned worker retains operation and
//! cleanup responsibility when the requesting HTTP future is cancelled.
use super::*;
use crate::macos_monitor::{Value as MonitorValue, WindowValue};
use serde_json::{json, Value};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(extend("type" = "object"))]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    /// Lazily create this session's private browser; repeating open reuses it.
    Open {
        url: String,
        #[serde(default)]
        extension: Option<super::super::task_extension::TaskExtension>,
    },
    ExtensionPopup {
        workspace_id: String,
        request_id: String,
    },
    ExtensionViews {
        workspace_id: String,
    },
    ExtensionPage {
        workspace_id: String,
        request_id: String,
        /// Relative extension HTML resource; omitted selects action.default_popup.
        #[serde(default)]
        path: Option<String>,
    },
    Navigate {
        workspace_id: String,
        request_id: String,
        url: String,
    },
    Status {},
    Screenshot {
        workspace_id: String,
        #[serde(default)]
        view_id: Option<String>,
    },
    Keyboard {
        workspace_id: String,
        #[serde(default)]
        view_id: Option<String>,
        request_id: String,
        action: managed_keyboard::Action,
    },
    /// Coordinates are window-local logical points, as described with each image.
    Click {
        workspace_id: String,
        #[serde(default)]
        view_id: Option<String>,
        request_id: String,
        x: f64,
        y: f64,
    },
    Scroll {
        workspace_id: String,
        #[serde(default)]
        view_id: Option<String>,
        request_id: String,
        x: f64,
        y: f64,
        delta_y: i32,
    },
    Close {
        workspace_id: String,
    },
}
impl Request {
    pub(super) fn input(&self) -> bool {
        matches!(
            self,
            Self::Open { .. }
                | Self::ExtensionPopup { .. }
                | Self::ExtensionPage { .. }
                | Self::Navigate { .. }
                | Self::Keyboard { .. }
                | Self::Click { .. }
                | Self::Scroll { .. }
        )
    }
    fn workspace_id(&self) -> Option<&str> {
        match self {
            Self::ExtensionPopup { workspace_id, .. }
            | Self::ExtensionViews { workspace_id }
            | Self::ExtensionPage { workspace_id, .. }
            | Self::Navigate { workspace_id, .. }
            | Self::Screenshot { workspace_id, .. }
            | Self::Keyboard { workspace_id, .. }
            | Self::Click { workspace_id, .. }
            | Self::Scroll { workspace_id, .. }
            | Self::Close { workspace_id } => Some(workspace_id),
            _ => None,
        }
    }
    fn validate(&self) -> Result<(), String> {
        if self
            .workspace_id()
            .is_some_and(|id| id.is_empty() || id.len() > 128)
        {
            return Err("invalid task workspace identity".into());
        }
        match self {
            Self::Open { url, .. } | Self::Navigate { url, .. } => {
                super::super::navigation::validate_url(url)?;
            }
            Self::Click { x, y, .. } | Self::Scroll { x, y, .. } => {
                crate::macos_monitor::pointer::Point { x: *x, y: *y }.validate()?;
            }
            _ => {}
        }
        if let Self::Open {
            extension: Some(extension),
            ..
        } = self
        {
            extension.validate()?;
        }
        if let Self::Scroll { delta_y, .. } = self {
            crate::macos_monitor::scroll::validate_delta(*delta_y)?;
        }
        Ok(())
    }
}

pub(crate) enum Response {
    Json(Value),
    Image { metadata: Value, png: Vec<u8> },
}
fn failure(error: &str) -> Response {
    Response::Json(json!({"ok":false,"error":error,"effects_verified":false}))
}

pub(crate) async fn execute(
    request: Request,
    actor: ActorBinding,
    epoch: Option<String>,
    bus: &EventBus,
    autonomy: crate::autonomy::SharedAutonomy,
) -> Response {
    if let Err(error) = request.validate() {
        return failure(&error);
    }
    let registry = &bus.macos_monitors.task_browsers;
    let (key, probe) = match registry
        .identity(&actor, epoch.as_deref(), request.input())
        .await
    {
        Ok(identity) => identity,
        Err(error) => return failure(&error),
    };
    if !cfg!(target_os = "macos") && !matches!(request, Request::Status {}) {
        return failure("task browser currently requires macOS");
    }
    let (entry, created) = if matches!(request, Request::Open { .. }) {
        let mut entries = registry.entries.lock().await;
        if let Some(entry) = entries.get(&key.session) {
            if entry.key != key {
                return failure("previous session browser cleanup is pending");
            }
            (entry.clone(), false)
        } else {
            if entries.len() >= MAX_TASK_BROWSERS {
                return failure("task browser capacity is full");
            }
            let entry = Arc::new(Allocation::new(key, actor, probe, autonomy));
            if let Request::Open { extension, .. } = &request {
                entry
                    .resources
                    .lock()
                    .expect("new allocation")
                    .requested_extension = extension.clone();
            }
            entries.insert(entry.key.session.clone(), entry.clone());
            (entry, true)
        }
    } else {
        match registry.existing(&key).await {
            Ok(entry) => (entry, false),
            Err(error) if matches!(request, Request::Status {}) => {
                return Response::Json(json!({"ok":true,"workspace":null,"message":error}))
            }
            Err(error) => return failure(&error),
        }
    };
    if let Some(expected) = request.workspace_id() {
        if entry.check_workspace_id(expected).is_err() {
            return failure("task workspace generation changed or belongs to another session");
        }
    }
    if matches!(request, Request::Close { .. }) {
        entry.revoked.store(true, Ordering::SeqCst);
    }
    let bus = bus.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if send.is_closed() && !created && !matches!(request, Request::Close { .. }) {
            return;
        }
        let result = if matches!(request, Request::Close { .. }) {
            let _guard = entry.lane.clone().lock_owned().await;
            cleanup(&entry, &bus)
                .await
                .map(|()| Response::Json(json!({"ok":true,"closed":true})))
        } else {
            match entry.admit(request.input()).await {
                Err(error) => Err(error),
                Ok(_guard) => perform(&entry, &bus, request, created, &cancelled).await,
            }
        };
        let failed_create = created && result.is_err();
        let response = result.unwrap_or_else(|error| failure(&error));
        if failed_create {
            entry.revoked.store(true, Ordering::SeqCst);
            let guard = entry.lane.clone().lock_owned().await;
            let cleaned = cleanup(&entry, &bus).await.is_ok();
            drop(guard);
            if !cleaned {
                watch(entry, bus);
            }
            let _ = send.send(response);
            return;
        }
        let abandoned = send.send(response).is_err();
        if created && abandoned {
            entry.revoked.store(true, Ordering::SeqCst);
            let _guard = entry.lane.clone().lock_owned().await;
            if cleanup(&entry, &bus).await.is_err() {
                drop(_guard);
                watch(entry, bus);
            }
        } else if created {
            watch(entry, bus);
        }
    });
    receive
        .await
        .unwrap_or_else(|_| failure("task browser worker ended; inspect before another input"))
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
pub(super) fn operation_authority(
    entry: &Arc<Allocation>,
    mode: Mode,
    cancelled: &Arc<AtomicBool>,
) -> crate::macos_monitor::Authority {
    let mut authority = entry.authority(mode);
    authority.task.as_mut().expect("task authority").cancelled = Some(cancelled.clone());
    authority
}

fn summary(entry: &Allocation) -> Result<Value, String> {
    let r = entry
        .resources
        .lock()
        .map_err(|_| "task browser state unavailable")?;
    let headless = r.requested_extension.is_some();
    let mut operations = vec![
        "screenshot",
        "click",
        "scroll",
        "keyboard",
        "navigate",
        "close",
    ];
    if headless {
        operations.extend(["extension_popup", "extension_views", "extension_page"]);
    }
    Ok(json!({"ok":true,"workspace_id":r.workspace,"ready":r.ready,
        "stopped":entry.revoked.load(Ordering::SeqCst),
        "input_uncertain":entry.quarantined.load(Ordering::SeqCst),
        "backend":if headless{"headless_extension"}else{"macos_virtual"},
        "coordinate_space":if headless{"page_css_pixels"}else{"window_logical_points"},
        "width":WIDTH,"height":HEIGHT,
        "window_on_monitor":if headless{Value::Null}else{json!({"x":40,"y":40,"width":720,"height":530})},
        "extension":r.extension.as_ref().map(|e|json!({"archive_sha256":e.archive_sha256,"version":e.version})),
        "profile_lifetime":"task_session_ephemeral",
        "supported_operations":operations,"automatic_assignment":true,"per_action_approval":false}))
}

async fn perform(
    entry: &Arc<Allocation>,
    bus: &EventBus,
    request: Request,
    created: bool,
    cancelled: &Arc<AtomicBool>,
) -> Result<Response, String> {
    #[cfg(target_os = "macos")]
    if !matches!(request, Request::Open { .. } | Request::Status {})
        && entry
            .resources
            .lock()
            .map_err(|_| "task resources unavailable")?
            .headless
            .is_some()
    {
        return headless_extension::execute(entry, request, cancelled).await;
    }
    match request {
        Request::ExtensionPopup { .. }
        | Request::ExtensionPage { .. }
        | Request::ExtensionViews { .. } => {
            Err("this task browser has no extension backend".into())
        }
        Request::Open { url, extension } => {
            if created {
                provision(entry, bus, url, cancelled).await?;
            } else {
                entry.check(false).await?;
                let resources = entry
                    .resources
                    .lock()
                    .map_err(|_| "task resources unavailable")?;
                if extension
                    .as_ref()
                    .is_some_and(|e| !e.matches(resources.extension.as_ref()))
                {
                    return Err("existing workspace has a different extension; close before creating a replacement".into());
                }
            }
            Ok(Response::Json(summary(entry)?))
        }
        Request::Navigate {
            workspace_id,
            request_id,
            url,
        } => {
            entry.claim(&request_id)?;
            let result = super::super::navigation::execute(
                &workspace_id,
                &url,
                bus,
                &operation_authority(entry, Mode::Input, cancelled),
            )
            .await;
            if !result.ok && result.effects_unconfirmed {
                entry.quarantined.store(true, Ordering::SeqCst);
            }
            let mut value =
                serde_json::to_value(result).map_err(|_| "navigation receipt encoding failed")?;
            value["request_id"] = request_id.into();
            Ok(Response::Json(value))
        }
        Request::Status {} => Ok(Response::Json(summary(entry)?)),
        Request::Screenshot { view_id, .. } => {
            if view_id.is_some() {
                return Err("view is not assigned to this task browser".into());
            }
            screenshot(entry, bus, cancelled).await
        }
        Request::Keyboard {
            request_id,
            action,
            view_id,
            ..
        } => {
            if view_id.is_some() {
                return Err("view is not assigned to this task browser".into());
            }
            entry.claim(&request_id)?;
            let workspace = ready_workspace(entry).await?;
            let mut result = managed_keyboard::execute(
                managed_keyboard::Request {
                    workspace_id: workspace.id,
                    request_id,
                    action,
                },
                bus,
                operation_authority(entry, Mode::Input, cancelled),
            )
            .await;
            if !result.ok && result.effects_unconfirmed {
                entry.quarantined.store(true, Ordering::SeqCst);
            }
            // Error details may contain private endpoint/native diagnostics.
            if result.error.is_some() {
                result.error = Some(
                    "keyboard refused or unconfirmed; inspect before any further input".into(),
                );
            }
            Ok(Response::Json(json!(result)))
        }
        Request::Click {
            request_id,
            x,
            y,
            view_id,
            ..
        } => {
            if view_id.is_some() {
                return Err("view is not assigned to this task browser".into());
            }
            entry.claim(&request_id)?;
            pointer(entry, bus, request_id, x, y, None, cancelled).await
        }
        Request::Scroll {
            request_id,
            x,
            y,
            delta_y,
            view_id,
            ..
        } => {
            if view_id.is_some() {
                return Err("view is not assigned to this task browser".into());
            }
            entry.claim(&request_id)?;
            pointer(entry, bus, request_id, x, y, Some(delta_y), cancelled).await
        }
        Request::Close { .. } => unreachable!("cleanup uses separate admission"),
    }
}

fn launch_failure_category(error: &BrowserWorkspaceError) -> &'static str {
    match error {
        BrowserWorkspaceError::Launch(message)
            if message == crate::macos_monitor::WINDOW_PERMISSION_REQUIRED =>
        {
            "os_authorization_missing"
        }
        BrowserWorkspaceError::Unsupported(_) => "unsupported",
        BrowserWorkspaceError::Io(_) => "filesystem",
        BrowserWorkspaceError::Launch(_) => "launch",
        _ => "workspace",
    }
}

async fn provision(
    entry: &Arc<Allocation>,
    bus: &EventBus,
    url: String,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if entry
        .resources
        .lock()
        .map_err(|_| "task resources unavailable")?
        .requested_extension
        .is_some()
    {
        return headless_extension::provision(entry, &url, cancelled).await;
    }
    let _lane = bus.macos_monitors.workspace_lane.clone().lock_owned().await;
    entry.check(true).await?;
    let authority = operation_authority(entry, Mode::Provision, cancelled);
    let receipt = bus
        .macos_monitors
        .request(
            MonitorAction::Create {
                width: WIDTH,
                height: HEIGHT,
            },
            authority.clone(),
        )
        .await
        .map_err(|_| "task monitor creation failed")?;
    let monitor = match &receipt.value {
        MonitorValue::Created(monitor) => monitor.clone(),
        _ => return Err("unexpected task monitor receipt".into()),
    };
    entry
        .resources
        .lock()
        .map_err(|_| "task resource tracking unavailable")?
        .monitor = Some(monitor.clone());
    if !receipt.commit() {
        return Err("task monitor receipt expired; cleanup retained".into());
    }
    let request = CreateBrowserWorkspaceRequest {
        url: Some(url),
        label: Some("Agent browser".into()),
        provider: Some("cdp".into()),
        peer_id: None,
        owner_session_id: Some(entry.key.session.clone()),
        display_target: Some(monitor.selector),
        profile_dir: None,
        viewport: None,
        extension_archive_path: None,
        extension_archive_sha256: None,
        extension_archive_byte_length: None,
        extension_manifest_version: None,
        extension_version: None,
    };
    let workspace = create_workspace_inner(request, bus, Some(authority))
        .await
        .map_err(|error| {
            let stage = entry.resources.lock().map(|r| r.stage).unwrap_or("unknown");
            // Owner-side diagnostics only. The session response below retains
            // a closed vocabulary and never receives the native error body.
            eprintln!("[task-browser] launch failure at {stage}: {error}");
            let kind = launch_failure_category(&error);
            // Closed diagnostic vocabulary only: no app-provided error body,
            // private profile path, native handle or debugging endpoint leaks.
            format!("managed task browser {kind} failure at {stage}; no input sent")
        })?;
    entry.check(true).await?;
    let mut r = entry
        .resources
        .lock()
        .map_err(|_| "task resource tracking unavailable")?;
    if r.workspace.as_deref() != Some(workspace.id.as_str())
        || r.binding != workspace.macos_window_binding
        || r.binding.is_none()
        || r.process
            .is_none_or(|(pid, _)| workspace.process_id != Some(pid))
        || workspace.active_target_id.is_none()
        || workspace.status != BrowserWorkspaceStatus::Ready
    {
        return Err("task browser ready identity mismatch".into());
    }
    r.page = workspace.active_target_id.clone();
    r.ready = true;
    r.stage = "ready";
    Ok(())
}

async fn ready_workspace(entry: &Arc<Allocation>) -> Result<BrowserWorkspace, String> {
    let id = entry
        .resources
        .lock()
        .map_err(|_| "task resource tracking unavailable")?
        .workspace
        .clone()
        .ok_or("task browser is not ready")?;
    let workspace = global_registry()
        .read()
        .await
        .workspaces
        .get(&id)
        .cloned()
        .ok_or("task browser was closed")?;
    if !entry
        .authority(Mode::Observe)
        .task
        .as_ref()
        .is_some_and(|p| p.uses_workspace(&workspace))
    {
        return Err("task browser identity or readiness changed".into());
    }
    Ok(workspace)
}

async fn verify_window(
    entry: &Arc<Allocation>,
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
) -> Result<(), String> {
    #[cfg(any(target_os = "macos", test))]
    {
        let binding = entry
            .resources
            .lock()
            .map_err(|_| "task resources unavailable")?
            .binding
            .clone()
            .ok_or("task window missing")?;
        let receipt = bus
            .macos_monitors
            .request(
                MonitorAction::Window(WindowAction::ValidatePageWindow { binding }),
                authority.clone(),
            )
            .await
            .map_err(|_| "task window observation unavailable")?;
        let valid = match &receipt.value {
            MonitorValue::Window(WindowValue::ValidatedPageWindow(window)) => authority
                .task
                .as_ref()
                .is_some_and(|p| p.checks_window(*window)),
            _ => false,
        };
        if receipt.commit() && valid {
            Ok(())
        } else {
            Err("task window geometry changed; no screenshot/action served".into())
        }
    }
    #[cfg(not(any(target_os = "macos", test)))]
    {
        let _ = (entry, bus, authority);
        Err("task window observation requires macOS".into())
    }
}

async fn screenshot(
    entry: &Arc<Allocation>,
    bus: &EventBus,
    cancelled: &Arc<AtomicBool>,
) -> Result<Response, String> {
    let _lane = bus.macos_monitors.workspace_lane.clone().lock_owned().await;
    entry.check(false).await?;
    let workspace = ready_workspace(entry).await?;
    let authority = operation_authority(entry, Mode::Observe, cancelled);
    verify_window(entry, bus, &authority).await?;
    let receipt = bus
        .macos_monitors
        .request(
            MonitorAction::Capture {
                selector: workspace.display_target.ok_or("task monitor missing")?,
                path: None,
            },
            authority.clone(),
        )
        .await
        .map_err(|_| "task screenshot failed")?;
    let (png, width, height) = match &receipt.value {
        MonitorValue::Captured(frame) if frame.path.is_none() => {
            (frame.png.clone(), frame.width, frame.height)
        }
        _ => return Err("unexpected task screenshot receipt".into()),
    };
    if !receipt.commit() {
        return Err("task screenshot receipt expired".into());
    }
    entry.check(false).await?;
    verify_window(entry, bus, &authority).await?;
    let mut metadata = summary(entry)?;
    metadata["image_width"] = width.into();
    metadata["image_height"] = height.into();
    metadata["artifact_retained"] = false.into();
    metadata["pixel_to_logical_x"] = json!(WIDTH as f64 / width as f64);
    metadata["pixel_to_logical_y"] = json!(HEIGHT as f64 / height as f64);
    Ok(Response::Image { metadata, png })
}

async fn pointer(
    entry: &Arc<Allocation>,
    bus: &EventBus,
    id: String,
    x: f64,
    y: f64,
    delta_y: Option<i32>,
    cancelled: &Arc<AtomicBool>,
) -> Result<Response, String> {
    let _lane = bus
        .macos_monitors
        .workspace_lane
        .clone()
        .try_lock_owned()
        .map_err(|_| "browser busy; no pointer input queued")?;
    entry.check(true).await?;
    let workspace = ready_workspace(entry).await?;
    let binding = workspace
        .macos_window_binding
        .ok_or("task window binding missing")?;
    let authority = operation_authority(entry, Mode::Input, cancelled);
    let point = crate::macos_monitor::pointer::Point { x, y };
    let preparation = match delta_y {
        None => WindowAction::PrepareClick {
            binding: binding.clone(),
            point,
        },
        Some(delta_y) => WindowAction::PrepareScroll {
            binding: binding.clone(),
            point,
            delta_y,
        },
    };
    let receipt = bus
        .macos_monitors
        .request(MonitorAction::Window(preparation), authority.clone())
        .await
        .map_err(|_| "task pointer preparation refused; no input sent")?;
    let token = match &receipt.value {
        MonitorValue::Window(WindowValue::PreparedPointer(p))
            if delta_y.is_none()
                && authority
                    .task
                    .as_ref()
                    .is_some_and(|permit| permit.checks_window(p.window)) =>
        {
            p.token.clone()
        }
        MonitorValue::Window(WindowValue::PreparedScroll(p))
            if delta_y == Some(p.delta_y)
                && authority
                    .task
                    .as_ref()
                    .is_some_and(|permit| permit.checks_window(p.window)) =>
        {
            p.token.clone()
        }
        _ => return Err("unexpected pointer preparation; no input sent".into()),
    };
    if !receipt.commit() {
        return Err("pointer preparation expired; no input sent".into());
    }
    entry.check(true).await?;
    let action = if delta_y.is_some() {
        WindowAction::Scroll { binding, token }
    } else {
        WindowAction::Click { binding, token }
    };
    let receipt = match bus
        .macos_monitors
        .request(MonitorAction::Window(action), authority)
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            let uncertain = error.starts_with(crate::macos_monitor::POINTER_UNCONFIRMED);
            if uncertain {
                entry.quarantined.store(true, Ordering::SeqCst);
            }
            return Ok(Response::Json(
                json!({"ok":false,"request_id":id,"effects_unconfirmed":uncertain,
                "effect_verified":false,"error":"pointer refused or uncertain; do not replay an uncertain action"}),
            ));
        }
    };
    let (ok, calls, uncertain) = match &receipt.value {
        MonitorValue::Window(WindowValue::Clicked(r)) => {
            (r.successful(), Some(r.posting_calls), r.effects_unconfirmed)
        }
        MonitorValue::Window(WindowValue::Scrolled(r)) => {
            (r.successful(), Some(r.posting_calls), r.effects_unconfirmed)
        }
        MonitorValue::Window(WindowValue::ClickUnconfirmed { result, .. }) => (
            false,
            Some(result.posting_calls),
            result.effects_unconfirmed,
        ),
        MonitorValue::Window(WindowValue::ScrollUnconfirmed { result, .. }) => (
            false,
            Some(result.posting_calls),
            result.effects_unconfirmed,
        ),
        _ => (false, None, true),
    };
    let committed = receipt.commit();
    if !ok || !committed {
        entry
            .quarantined
            .store(uncertain || !committed, Ordering::SeqCst);
    }
    Ok(Response::Json(
        json!({"ok":ok && committed,"request_id":id,"posting_calls":calls,
        "effect_verified":false,"effects_unconfirmed":uncertain || !committed}),
    ))
}

async fn cleanup(entry: &Arc<Allocation>, bus: &EventBus) -> Result<(), String> {
    entry.revoked.store(true, Ordering::SeqCst);
    let _lane = bus.macos_monitors.workspace_lane.clone().lock_owned().await;
    let resources = entry
        .resources
        .lock()
        .map_err(|_| "task resources unavailable")?
        .clone();
    #[cfg(target_os = "macos")]
    if let Some(headless) = resources.headless.as_ref() {
        headless.close().await?;
        bus.macos_monitors.task_browsers.forget(entry).await;
        return Ok(());
    }
    let authority = entry.authority(Mode::Cleanup);
    if let Some(id) = resources.workspace.as_deref() {
        if global_registry().read().await.workspaces.contains_key(id) {
            let closed = close_workspace_inner(
                id,
                Some("task browser stopped".into()),
                bus,
                Some(authority.clone()),
            )
            .await
            .map_err(|_| "task browser cleanup incomplete; owner recovery record retained")?;
            publish_workspace_event(bus, "closed", &closed);
        }
    }
    if let Some(profile) = resources.profile {
        match fs::remove_dir_all(&profile) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("task-owned profile cleanup incomplete; recovery retained".into()),
        }
    }
    if let Some(monitor) = resources.monitor {
        // An owner may have deliberately reused this monitor. Never destroy an
        // unrelated workspace as a side effect of the former task's cleanup.
        if global_registry()
            .read()
            .await
            .workspaces
            .values()
            .any(|w| w.display_target.as_deref() == Some(monitor.selector.as_str()))
        {
            return Err("task monitor has another workspace; owner cleanup required".into());
        }
        let status = bus
            .macos_monitors
            .inspect(
                Inspection::Status {
                    selector: monitor.selector.clone(),
                },
                authority.clone(),
            )
            .await
            .map_err(|_| "task monitor cleanup status unavailable")?
            .status();
        if !status["monitor"].is_null() {
            let receipt = bus
                .macos_monitors
                .request(
                    MonitorAction::Destroy {
                        display_id: monitor.display_id,
                        selector: monitor.selector,
                    },
                    authority,
                )
                .await
                .map_err(|_| "task monitor cleanup incomplete")?;
            let destroyed = matches!(receipt.value, MonitorValue::Destroyed);
            if !receipt.commit() || !destroyed {
                return Err("task monitor cleanup receipt unavailable".into());
            }
        } else if status["broker_state"] != "live" {
            return Err("task monitor cleanup could not be confirmed".into());
        }
    }
    bus.macos_monitors.task_browsers.forget(entry).await;
    Ok(())
}

fn watch(entry: Arc<Allocation>, bus: EventBus) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let guard = match entry.lane.clone().try_lock_owned() {
                Ok(g) => g,
                Err(_) => continue,
            };
            let active = entry.check(false).await.is_ok();
            let resources = entry.resources.lock().ok().map(|r| r.clone());
            #[cfg(target_os = "macos")]
            let offscreen_alive =
                if let Some(headless) = resources.as_ref().and_then(|r| r.headless.clone()) {
                    Some(headless.alive().await)
                } else {
                    None
                };
            #[cfg(not(target_os = "macos"))]
            let offscreen_alive: Option<bool> = None;
            let child_live = if let Some(alive) = offscreen_alive {
                alive
            } else if let Some(id) = resources.and_then(|r| r.workspace) {
                global_registry()
                    .write()
                    .await
                    .children
                    .get_mut(&id)
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
            } else {
                false
            };
            if active && child_live {
                drop(guard);
                continue;
            }
            entry.revoked.store(true, Ordering::SeqCst);
            if cleanup(&entry, &bus).await.is_ok() {
                return;
            }
            // Retry only idempotent teardown of our retained resources. This
            // never retries input, adopts a replacement, or drops the recovery
            // record merely because a cleanup receipt is temporarily unavailable.
            drop(guard);
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navigation_and_pointer_inputs_are_bounded_before_resources() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pass@example.test",
            "custom://open",
        ] {
            assert!(Request::Open {
                url: url.into(),
                extension: None
            }
            .validate()
            .is_err());
        }
        assert!(Request::Open {
            url: "about:blank".into(),
            extension: None
        }
        .validate()
        .is_ok());
        assert!(Request::Click {
            workspace_id: "bw-fixture".into(),
            view_id: None,
            request_id: uuid::Uuid::new_v4().to_string(),
            x: f64::NAN,
            y: 2.0
        }
        .validate()
        .is_err());
        assert!(Request::Scroll {
            workspace_id: "bw-fixture".into(),
            view_id: None,
            request_id: uuid::Uuid::new_v4().to_string(),
            x: 1.0,
            y: 2.0,
            delta_y: 601
        }
        .validate()
        .is_err());
    }
    #[test]
    fn launch_diagnostic_names_os_authorization_without_echoing_native_details() {
        assert_eq!(
            launch_failure_category(&BrowserWorkspaceError::Launch(
                crate::macos_monitor::WINDOW_PERMISSION_REQUIRED.into()
            )),
            "os_authorization_missing"
        );
        assert_eq!(
            launch_failure_category(&BrowserWorkspaceError::Launch(
                "private /Users/example/profile /devtools/page/id".into()
            )),
            "launch"
        );
    }
    #[test]
    fn explicit_navigation_is_input_and_pins_the_original_workspace() {
        let request: Request = serde_json::from_value(json!({"op":"navigate","workspace_id":"bw-first","request_id":uuid::Uuid::new_v4().to_string(),"url":"https://example.test/new"})).unwrap();
        assert!(request.input());
        assert_eq!(request.workspace_id(), Some("bw-first"));
        assert!(request.validate().is_ok());
    }

    #[test]
    fn drop_marks_only_request_cancellation_not_session_revocation() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(flag.clone());
        assert!(!flag.load(Ordering::SeqCst));
        drop(guard);
        assert!(flag.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn failed_provision_cleanup_removes_owned_profile_without_native_work() {
        let root = tempfile::tempdir().unwrap();
        let (entry, _) = super::super::tests::fixture();
        let id = format!("bw-test-{}", uuid::Uuid::new_v4().simple());
        let profile = root.path().join(&id).join("profile");
        let unrelated = root.path().join("unrelated");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir(&unrelated).unwrap();
        {
            let mut resources = entry.resources.lock().unwrap();
            resources.monitor = None;
            resources.workspace = Some(id);
            resources.expected_profile = Some(profile.clone());
            resources.binding = None;
            resources.process = None;
            resources.ready = false;
        }
        let permit = entry.authority(Mode::Provision).task.unwrap();
        assert!(entry.resources.lock().unwrap().profile.is_none());
        assert!(permit.record_profile(&unrelated.join("profile")).is_err());
        assert!(entry.resources.lock().unwrap().profile.is_none());
        permit.record_profile(&profile).unwrap();
        assert!(permit.record_profile(&profile).is_err());
        let bus = EventBus::new();
        bus.macos_monitors
            .task_browsers
            .entries
            .lock()
            .await
            .insert(entry.key.session.clone(), entry.clone());
        cleanup(&entry, &bus).await.unwrap();
        assert!(!profile.exists());
        assert!(unrelated.exists());
        assert!(bus.macos_monitors.not_started());
        assert!(bus
            .macos_monitors
            .task_browsers
            .entries
            .lock()
            .await
            .is_empty());
    }
}
