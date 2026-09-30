//! Task-owned, offscreen Chrome for extensions whose own windows request focus.
//! This is NOT macos_virtual or a separate OS seat. There are no native capture,
//! cursor, keyboard, activation or privacy-permission calls in this backend.
//! All resources remain private to the exact authenticated Allocation.
use super::*;
use base64::Engine as _;
use serde_json::{json, Value};

pub(super) struct Workspace {
    profile: PathBuf,
    extension_root: PathBuf,
    state: Mutex<State>,
}
struct State {
    child: Option<Child>,
    pid: u32,
    birth: (u64, u32),
    endpoint: Option<DevToolsActivePort>,
    original: Option<String>,
    original_tab: Option<String>,
    original_window: Option<i64>,
    extension: BrowserWorkspaceExtension,
    views: BTreeMap<String, View>,
}
#[derive(Clone)]
struct View {
    target: String,
    // Identity is fixed to the extension origin; paths may change within its UI.
    resource: String,
}

fn bounded_id(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        })
        .map(str::to_owned)
        .ok_or_else(|| "missing bounded browser target identity".into())
}
fn runtime_from_worker(targets: &Value, worker: &str) -> Result<String, String> {
    let rows = targets
        .as_array()
        .filter(|r| r.len() <= 64)
        .ok_or("browser target budget")?;
    let legacy = Value::Array(rows.clone());
    active_extension_runtime_id(&legacy, worker)
        .ok_or_else(|| "approved extension worker is not yet uniquely available".into())
}

fn related_tab(tab: &Value, original: &Value) -> bool {
    tab["type"] == "tab"
        && original["type"] == "page"
        && original["url"]
            .as_str()
            .is_some_and(|url| !url.starts_with("chrome-extension:"))
        && tab["url"] == original["url"]
        && tab["browserContextId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        && tab["browserContextId"] == original["browserContextId"]
}

async fn bind_original_tab(
    state: &mut State,
    browser: &mut managed_keyboard::Client,
) -> Result<(), String> {
    let page = state
        .original
        .as_deref()
        .ok_or("original page unavailable")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let info = browser
            .call("Target.getTargetInfo", json!({"targetId":page}))
            .await?;
        extension_ui::validate_target(&info["targetInfo"], page, None)?;
        let window = browser
            .call("Browser.getWindowForTarget", json!({"targetId":page}))
            .await?;
        let wid = window["windowId"]
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or("original browser window missing")?;
        let tabs = browser
            .call(
                "Target.getTargets",
                json!({"filter":[{"type":"tab","exclude":false},{"exclude":true}]}),
            )
            .await?;
        let mut candidates = Vec::new();
        for tab in tabs["targetInfos"]
            .as_array()
            .filter(|r| r.len() <= 64)
            .ok_or("tab inventory unavailable")?
        {
            if !related_tab(tab, &info["targetInfo"]) {
                continue;
            }
            let id = bounded_id(&tab["targetId"])?;
            let owner = browser
                .call("Browser.getWindowForTarget", json!({"targetId":id}))
                .await?;
            if owner["windowId"].as_i64() == Some(wid) {
                candidates.push(id);
            }
        }
        if candidates.len() == 1 {
            state.original_tab = candidates.pop();
            state.original_window = Some(wid);
            return Ok(());
        }
        if candidates.len() > 1 || tokio::time::Instant::now() >= deadline {
            return Err("original tab wrapper is ambiguous or unavailable".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn check_original_tab(
    state: &State,
    browser: &mut managed_keyboard::Client,
) -> Result<String, String> {
    let original = state
        .original
        .as_deref()
        .ok_or("original page unavailable")?;
    let tab = state
        .original_tab
        .as_deref()
        .ok_or("original tab unavailable")?;
    let page_info = browser
        .call("Target.getTargetInfo", json!({"targetId":original}))
        .await?;
    extension_ui::validate_target(&page_info["targetInfo"], original, None)?;
    let tab_info = browser
        .call("Target.getTargetInfo", json!({"targetId":tab}))
        .await?;
    if tab_info["targetInfo"]["targetId"] != tab
        || !related_tab(&tab_info["targetInfo"], &page_info["targetInfo"])
    {
        return Err("original tab wrapper changed".into());
    }
    for target in [original, tab] {
        let window = browser
            .call("Browser.getWindowForTarget", json!({"targetId":target}))
            .await?;
        if window["windowId"].as_i64() != state.original_window {
            return Err("original browser window changed".into());
        }
    }
    Ok(tab.into())
}

fn launch_arguments(profile: &Path, extension: &BrowserWorkspaceExtension) -> Vec<String> {
    let mut args = vec![
        "--headless=new".into(),
        "--disable-gpu".into(),
        "--no-startup-window".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        format!("--user-data-dir={}", profile.display()),
    ];
    args.extend(browser_extension_launch_flags(Some(extension)));
    args
}

impl Workspace {
    pub(super) async fn alive(&self) -> bool {
        self.state
            .lock()
            .await
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }
    async fn browser(&self, state: &mut State) -> Result<managed_keyboard::Client, String> {
        if !state
            .child
            .as_mut()
            .is_some_and(|c| c.id() == Some(state.pid) && matches!(c.try_wait(), Ok(None)))
            || crate::platform::macos_process_birth(state.pid as i32) != Some(state.birth)
        {
            return Err("owned headless browser ended or changed".into());
        }
        let endpoint = state
            .endpoint
            .as_ref()
            .ok_or("browser endpoint is not ready")?;
        if read_devtools_active_port(&self.profile)
            .map_err(|_| "profile endpoint unavailable")?
            .as_ref()
            != Some(endpoint)
        {
            return Err("owned headless profile endpoint changed".into());
        }
        let url = format!(
            "ws://127.0.0.1:{}{}",
            endpoint.port, endpoint.browser_websocket_path
        );
        let mut client = managed_keyboard::Client::connect_exact(
            endpoint.port,
            &url,
            &endpoint.browser_websocket_path,
        )
        .await?;
        let processes = client.call("SystemInfo.getProcessInfo", json!({})).await?;
        let pids: Vec<_> = processes["processInfo"]
            .as_array()
            .ok_or("missing browser processes")?
            .iter()
            .filter(|p| p["type"] == "browser")
            .collect();
        if pids.len() != 1 || pids[0]["id"].as_u64() != Some(state.pid as u64) {
            return Err("debugging endpoint changed browser process".into());
        }
        Ok(client)
    }
    pub(super) async fn close(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        // Keep the retained Child and its files until exit has been observed.
        if state
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
        {
            if let Ok(mut client) = self.browser(&mut state).await {
                let _ = client.call("Browser.close", json!({})).await;
            }
            if let Some(child) = state.child.as_mut() {
                let waited = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
                if let Ok(Err(_)) = &waited {
                    return Err("headless browser wait failed; cleanup retained".into());
                }
                if waited.is_err() {
                    child
                        .start_kill()
                        .map_err(|_| "headless browser cleanup could not terminate its child")?;
                    tokio::time::timeout(Duration::from_secs(5), child.wait())
                        .await
                        .map_err(|_| "headless browser exit not confirmed")?
                        .map_err(|_| "headless browser wait failed")?;
                }
            }
        }
        if let Some(child) = state.child.as_mut() {
            if child
                .try_wait()
                .map_err(|_| "headless browser exit unavailable")?
                .is_none()
            {
                return Err("headless browser still running".into());
            }
        }
        state.child = None;
        make_extension_tree_writable(&self.extension_root);
        for path in [&self.extension_root, &self.profile] {
            match fs::remove_dir_all(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err("headless task-owned files need cleanup; ownership retained".into())
                }
            }
        }
        Ok(())
    }
}

pub(super) async fn provision(
    entry: &Arc<Allocation>,
    url: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    let authority = lifecycle::operation_authority(entry, Mode::Provision, cancelled);
    authority.check().await?;
    let requested = entry
        .resources
        .lock()
        .map_err(|_| "task resources unavailable")?
        .requested_extension
        .clone()
        .ok_or("extension missing")?;
    requested.validate()?;
    let id = format!("bw-{}", uuid::Uuid::new_v4().simple());
    let profile = default_profile_dir(&id);
    let extension_root = browser_extension_workspace_root(&id);
    let mut guard = create_extension_workspace_filesystem(&profile, &extension_root)
        .map_err(|_| "fresh extension filesystem creation failed")?;
    let worker = extension_policy::current()
        .approved_worker(
            &requested.archive_sha256,
            requested.archive_byte_length,
            requested.manifest_version,
            &requested.version,
        )
        .map_err(|_| "extension policy changed")?;
    let spec = BrowserExtensionArchiveSpec {
        service_worker: worker.into(),
        archive_path: PathBuf::from(&requested.archive_path),
        archive_sha256: requested.archive_sha256.clone(),
        archive_byte_length: requested.archive_byte_length,
        manifest_version: requested.manifest_version,
        version: requested.version.clone(),
    };
    let extension = prepare_browser_extension(&spec, &extension_root)
        .map_err(|_| "approved extension archive preparation failed")?;
    let executable = find_intendant_managed_chromium_executable()
        .ok_or("install managed Chrome for Testing before extension use")?;
    authority.check().await?;
    let args = launch_arguments(&profile, &extension);
    let child = tokio::process::Command::new(executable)
        .env_clear()
        .env("HOME", crate::platform::home_dir())
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", std::env::temp_dir())
        .env("LANG", "en_US.UTF-8")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "headless managed browser launch failed")?;
    let pid = child.id().ok_or("missing spawned browser identity")?;
    // Publish cleanup ownership immediately after spawning, before any await.
    let owned = Arc::new(Workspace {
        profile: profile.clone(),
        extension_root,
        state: Mutex::new(State {
            child: Some(child),
            pid,
            birth: (0, 0),
            endpoint: None,
            original: None,
            original_tab: None,
            original_window: None,
            extension,
            views: BTreeMap::new(),
        }),
    });
    {
        let mut r = entry
            .resources
            .lock()
            .map_err(|_| "task resources unavailable")?;
        r.workspace = Some(id);
        r.headless = Some(owned.clone());
        r.stage = "headless_launch";
    }
    guard.disarm();
    let mut state = owned.state.lock().await;
    state.birth = crate::platform::macos_process_birth(pid as i32)
        .ok_or("headless process birth unavailable")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        authority.check().await?;
        if !state
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
        {
            return Err("headless browser exited during startup".into());
        }
        if let Some(endpoint) =
            read_devtools_active_port(&profile).map_err(|_| "headless endpoint record invalid")?
        {
            state.endpoint = Some(endpoint);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut browser = owned.browser(&mut state).await?;
    loop {
        authority.check().await?;
        let targets = browser.call("Target.getTargets", json!({})).await?;
        if let Ok(id) = runtime_from_worker(&targets["targetInfos"], worker) {
            state.extension.runtime_id = Some(id);
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("approved extension startup timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    authority.check().await?;
    // Activating a virtual browser tab is confined to this HEADLESS process.
    // No platform window is shown and no human desktop target can be selected.
    let target = browser
        .call(
            "Target.createTarget",
            json!({"url":url,"background":false,"newWindow":true,"width":1024,"height":768}),
        )
        .await?;
    state.original = Some(bounded_id(&target["targetId"])?);
    bind_original_tab(&mut state, &mut browser).await?;
    authority.check().await?;
    let mut r = entry
        .resources
        .lock()
        .map_err(|_| "task resources unavailable")?;
    r.extension = Some(state.extension.clone());
    r.ready = true;
    r.stage = "ready";
    Ok(())
}

fn public_views(views: &BTreeMap<String, View>) -> Value {
    json!(views.iter().map(|(id,v)|json!({"view_id":id,"resource":v.resource,"kind":"extension_ui","coordinate_space":"page_css_pixels"})).collect::<Vec<_>>())
}

async fn discover(
    state: &mut State,
    browser: &mut managed_keyboard::Client,
) -> Result<Value, String> {
    let targets = browser.call("Target.getTargets", json!({})).await?;
    let original = state.original.as_deref().ok_or("original page missing")?;
    super::super::task_extension::validate_targets(
        &targets["targetInfos"],
        original,
        Some(&state.extension),
    )?;
    let eid = state
        .extension
        .runtime_id
        .as_deref()
        .ok_or("extension identity missing")?;
    let rows = targets["targetInfos"].as_array().ok_or("targets missing")?;
    let live: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|t| t["targetId"].as_str().map(str::to_owned))
        .collect();
    state.views.retain(|_, v| live.contains(&v.target));
    for row in rows {
        if row["type"] != "page" {
            continue;
        }
        let raw = row["url"].as_str().ok_or("view URL unavailable")?;
        if !super::super::task_extension::extension_url_matches(raw, eid) {
            continue;
        }
        let target = bounded_id(&row["targetId"])?;
        if let Some(view) = state.views.values_mut().find(|v| v.target == target) {
            view.resource = url::Url::parse(raw)
                .map_err(|_| "view URL invalid")?
                .path()
                .to_string();
            continue;
        }
        if state.views.len() >= 8 {
            return Err("extension view capacity exceeded".into());
        }
        let resource = url::Url::parse(raw)
            .map_err(|_| "view URL invalid")?
            .path()
            .to_string();
        state.views.insert(
            format!("bv-{}", uuid::Uuid::new_v4().simple()),
            View { target, resource },
        );
    }
    Ok(public_views(&state.views))
}

async fn page(
    state: &State,
    browser: &mut managed_keyboard::Client,
    view: Option<&str>,
) -> Result<managed_keyboard::Client, String> {
    let target = match view {
        Some(handle) => state
            .views
            .get(handle)
            .map(|v| v.target.as_str())
            .ok_or("view is not assigned to this task")?,
        None => state.original.as_deref().ok_or("original page missing")?,
    };
    let info = browser
        .call("Target.getTargetInfo", json!({"targetId":target}))
        .await?;
    extension_ui::validate_target(
        &info["targetInfo"],
        target,
        view.and(state.extension.runtime_id.as_deref()),
    )?;
    let endpoint = state.endpoint.as_ref().ok_or("endpoint missing")?;
    managed_keyboard::Client::connect_capture(
        endpoint.port,
        &format!("ws://127.0.0.1:{}/devtools/page/{target}", endpoint.port),
        target,
    )
    .await
}

pub(super) async fn execute(
    entry: &Arc<Allocation>,
    request: Request,
    cancelled: &Arc<AtomicBool>,
) -> Result<Response, String> {
    let owned = entry
        .resources
        .lock()
        .map_err(|_| "task state unavailable")?
        .headless
        .clone()
        .ok_or("headless workspace missing")?;
    let input = request.input();
    let authority = lifecycle::operation_authority(
        entry,
        if input { Mode::Input } else { Mode::Observe },
        cancelled,
    );
    authority.check().await?;
    let mut state = owned.state.lock().await;
    let mut browser = owned.browser(&mut state).await?;
    let views = discover(&mut state, &mut browser).await?;
    let id = entry
        .resources
        .lock()
        .map_err(|_| "task state unavailable")?
        .workspace
        .clone()
        .ok_or("workspace missing")?;
    if matches!(request, Request::ExtensionViews { .. }) {
        authority.check().await?;
        return Ok(Response::Json(
            json!({"ok":true,"workspace_id":id,"views":views,"backend":"headless_extension"}),
        ));
    }
    let request_id = match &request {
        Request::ExtensionPopup { request_id, .. }
        | Request::ExtensionPage { request_id, .. }
        | Request::Keyboard { request_id, .. }
        | Request::Click { request_id, .. }
        | Request::Scroll { request_id, .. }
        | Request::Navigate { request_id, .. } => Some(request_id.clone()),
        _ => None,
    };
    if let Some(ref request_id) = request_id {
        entry.claim(request_id)?;
    }
    let pointer = match &request {
        Request::Click { x, y, .. } => Some((*x, *y, None)),
        Request::Scroll { x, y, delta_y, .. } => Some((*x, *y, Some(*delta_y))),
        _ => None,
    };
    let mut attempted = 0usize;
    let mut acknowledged = 0usize;
    let result:Result<Response,String>=async {
        match request {
            Request::ExtensionPopup{..}=>{
                let original=state.original.clone().ok_or("original page missing")?;
                let tab=check_original_tab(&state,&mut browser).await?;
                let extension=state.extension.runtime_id.clone().ok_or("extension missing")?;
                authority.check().await?;
                // Logical tab activation exists only inside this headless child.
                // The browser's extension-action protocol opens the REAL popup
                // and wakes an idle MV3 worker; no wallet/controller JS is invoked.
                attempted+=1;
                browser.call("Target.activateTarget",json!({"targetId":original})).await?;
                acknowledged+=1;
                if check_original_tab(&state,&mut browser).await?!=tab{return Err("tab changed before extension action".into());}
                authority.check().await?;attempted+=1;
                browser.call("Extensions.triggerAction",json!({"id":extension,"targetId":tab})).await?;
                acknowledged+=1;
                Ok(Response::Json(json!({"ok":true,"workspace_id":id,"request_id":request_id,"popup_requested":true,"views":discover(&mut state,&mut browser).await?,"effects_verified":false,"backend":"headless_extension"})))
            }
            Request::ExtensionPage{path,..}=>{
                let path=path.as_deref().ok_or("extension_page needs a literal resource; use extension_popup for the real toolbar UI")?;
                let eid=state.extension.runtime_id.as_deref().ok_or("extension missing")?;
                let url=extension_ui::resource_url(eid,path)?;
                authority.check().await?;attempted+=1;
                browser.call("Target.createTarget",json!({"url":url,"background":true,"newWindow":false})).await?;acknowledged+=1;
                Ok(Response::Json(json!({"ok":true,"workspace_id":id,"views":discover(&mut state,&mut browser).await?,"toolbar_popup":false,"effects_verified":false})))
            }
            Request::Screenshot{view_id,..}=>{
                let mut client=page(&state,&mut browser,view_id.as_deref()).await?;
                let size=extension_ui::viewport(&client.call("Page.getLayoutMetrics",json!({})).await?)?;
                authority.check().await?;
                let frame=client.call("Page.captureScreenshot",json!({"format":"png","fromSurface":true,"captureBeyondViewport":false})).await?;
                let bytes=frame["data"].as_str().filter(|s|s.len()<=12*1024*1024).ok_or("screenshot missing or too large")?;
                let png=base64::engine::general_purpose::STANDARD.decode(bytes).map_err(|_|"invalid screenshot encoding")?;
                if png.len()<24 || &png[..8]!=b"\x89PNG\r\n\x1a\n" {return Err("invalid PNG".into());}
                let width=u32::from_be_bytes(png[16..20].try_into().expect("length checked"));
                let height=u32::from_be_bytes(png[20..24].try_into().expect("length checked"));
                if !(1..=4096).contains(&width)||!(1..=4096).contains(&height){return Err("screenshot dimensions exceed budget".into());}
                authority.check().await?;
                let _ = page(&state,&mut browser,view_id.as_deref()).await?;
                Ok(Response::Image {png,metadata:json!({"ok":true,"workspace_id":id,"view_id":view_id,"backend":"headless_extension","coordinate_space":"page_css_pixels","image_width":width,"image_height":height,"pixel_to_logical_x":size.0/width as f64,"pixel_to_logical_y":size.1/height as f64,"artifact_retained":false})})
            }
            Request::Keyboard{view_id,action,..}=>{
                managed_keyboard::validate_action(&action)?;
                let mut client=page(&state,&mut browser,view_id.as_deref()).await?;
                let before=managed_keyboard::page_receiver(&mut client,&action).await?;
                authority.check().await?;
                if managed_keyboard::page_receiver(&mut client,&action).await?!=before{return Err("receiver changed before keyboard dispatch".into());}
                let mut receipt=managed_keyboard::ResultReceipt::new(request_id.clone().unwrap_or_default());
                let result=managed_keyboard::dispatch_input(&mut client,&action,&mut receipt).await;
                attempted=receipt.commands_attempted;acknowledged=receipt.commands_acknowledged;
                result?;receipt.ok=true;
                Ok(Response::Json(json!(receipt)))
            }
            Request::Click{view_id,x,y,..} | Request::Scroll{view_id,x,y,delta_y:_,..}=>{
                let (_,_,delta)=pointer.expect("pointer variant");
                let mut client=page(&state,&mut browser,view_id.as_deref()).await?;
                let size=extension_ui::viewport(&client.call("Page.getLayoutMetrics",json!({})).await?)?;
                let commands=extension_ui::pointer_commands(x,y,delta,size)?;
                authority.check().await?;
                let mut error=None;
                for command in commands {
                    attempted+=1;
                    match client.call("Input.dispatchMouseEvent",command).await {
                        Ok(_)=>acknowledged+=1,
                        Err(e)=>if error.is_none(){error=Some(e)},
                    }
                }
                if let Some(error)=error{return Err(error);}
                Ok(Response::Json(json!({"ok":true,"request_id":request_id,"commands_attempted":attempted,"commands_acknowledged":acknowledged,"effects_verified":false,"effects_unconfirmed":true})))
            }
            Request::Navigate{url,..}=>{
                super::super::navigation::validate_url(&url)?;
                let mut client=page(&state,&mut browser,None).await?;
                let frame=client.call("Page.getFrameTree",json!({})).await?;
                let frame_id=frame["frameTree"]["frame"]["id"].as_str().ok_or("top frame unavailable")?;
                authority.check().await?;attempted+=1;
                let reply=client.call("Page.navigate",json!({"url":url,"frameId":frame_id})).await?;acknowledged+=1;
                let loader=super::super::navigation::acknowledge(&reply,frame_id)?;
                tokio::time::timeout(Duration::from_secs(8),async {
                    loop {
                        authority.check().await?;
                        let tree=client.call("Page.getFrameTree",json!({})).await?;
                        if super::super::navigation::committed(&tree,frame_id,loader.as_deref(),&url)? {break Ok::<_,String>(());}
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }).await.map_err(|_|"navigation commit not confirmed; do not replay")??;
                let _=page(&state,&mut browser,None).await?;
                Ok(Response::Json(json!({"ok":true,"request_id":request_id,"navigation_committed":true,"commands_attempted":attempted,"commands_acknowledged":acknowledged,"effects_verified":false,"effects_unconfirmed":true})))
            }
            _=>Err("unsupported headless task operation".into()),
        }
    }.await;
    let result = match result {
        Ok(value) => authority.check().await.map(|_| value),
        Err(e) => Err(e),
    };
    if result.is_err() && attempted > 0 {
        entry.quarantined.store(true, Ordering::SeqCst);
        return Ok(Response::Json(
            json!({"ok":false,"request_id":request_id,"commands_attempted":attempted,"commands_acknowledged":acknowledged,"effects_verified":false,"effects_unconfirmed":true,"error":"headless extension operation uncertain; inspect and close, do not replay"}),
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extension() -> BrowserWorkspaceExtension {
        BrowserWorkspaceExtension {
            archive_sha256: "a".repeat(64),
            archive_byte_length: 10,
            manifest_version: 3,
            version: "1.0".into(),
            service_worker: "worker.js".into(),
            load_path: "/private/test-owned/extension".into(),
            runtime_id: Some("a".repeat(32)),
        }
    }
    fn owned(root: &Path) -> Workspace {
        Workspace {
            profile: root.join("profile"),
            extension_root: root.join("extension"),
            state: Mutex::new(State {
                child: None,
                pid: 0,
                birth: (0, 0),
                endpoint: None,
                original: None,
                original_tab: None,
                original_window: None,
                extension: extension(),
                views: BTreeMap::new(),
            }),
        }
    }

    #[test]
    fn launch_is_headless_and_never_uses_desktop_or_personal_profile_flags() {
        let profile = Path::new("/private/test-owned/profile");
        let flags = launch_arguments(profile, &extension());
        assert_eq!(
            flags
                .iter()
                .filter(|f| f.as_str() == "--headless=new")
                .count(),
            1
        );
        assert!(flags.contains(&format!("--user-data-dir={}", profile.display())));
        assert!(flags.contains(&"--remote-debugging-address=127.0.0.1".into()));
        assert!(flags.contains(&"--disable-extensions-except=/private/test-owned/extension".into()));
        assert!(flags.contains(&"--load-extension=/private/test-owned/extension".into()));
        assert!(!flags.iter().any(|f| f.contains("--no-sandbox")
            || f.contains("--disable-web-security")
            || f.contains("--remote-allow-origins")
            || f.contains("--profile-directory")));
    }
    #[test]
    fn native_target_aliases_are_never_protocol_target_ids() {
        assert_eq!(bounded_id(&json!("AABB-123")).unwrap(), "AABB-123");
        for value in [
            json!(""),
            json!("../../file"),
            json!("macos_virtual:owner:1"),
            json!("user_session".repeat(20)),
            json!(1),
            Value::Null,
        ] {
            assert!(bounded_id(&value).is_err());
        }
    }
    #[test]
    fn worker_identity_requires_the_approved_entrypoint_and_unique_origin() {
        let id = "a".repeat(32);
        let one =
            json!({"type":"service_worker","url":format!("chrome-extension://{id}/worker.js")});
        assert_eq!(
            runtime_from_worker(&json!([one.clone()]), "worker.js").unwrap(),
            id
        );
        assert!(runtime_from_worker(&json!([one.clone()]), "other.js").is_err());
        assert!(runtime_from_worker(&json!([one.clone(), one]), "worker.js").is_err());
        for row in [
            json!({"type":"service_worker","url":format!("https://{id}/worker.js")}),
            json!({"type":"page","url":format!("chrome-extension://{id}/worker.js")}),
        ] {
            assert!(runtime_from_worker(&json!([row]), "worker.js").is_err());
        }
    }
    #[test]
    fn wrapper_mapping_requires_same_document_and_browser_context() {
        let page = json!({"type":"page","url":"https://example.test/","browserContextId":"owned"});
        let tab = json!({"type":"tab","url":"https://example.test/","browserContextId":"owned"});
        assert!(related_tab(&tab, &page));
        for (key, value) in [
            ("url", json!("https://other.test/")),
            ("browserContextId", json!("foreign")),
            ("type", json!("page")),
        ] {
            let mut changed = tab.clone();
            changed[key] = value;
            assert!(!related_tab(&changed, &page));
        }
        let mut page = page;
        page["url"] = json!("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/page.html");
        let mut tab = tab;
        tab["url"] = page["url"].clone();
        assert!(!related_tab(&tab, &page));
        tab["browserContextId"] = Value::Null;
        page["browserContextId"] = Value::Null;
        assert!(!related_tab(&tab, &page));
    }
    #[tokio::test]
    async fn cleanup_without_a_live_child_removes_only_its_owned_directories() {
        let root = tempfile::tempdir().unwrap();
        let workspace = owned(root.path());
        fs::create_dir(&workspace.profile).unwrap();
        fs::create_dir(&workspace.extension_root).unwrap();
        let untouched = root.path().join("unrelated");
        fs::write(&untouched, b"keep").unwrap();
        workspace.close().await.unwrap();
        workspace.close().await.unwrap();
        assert!(!workspace.profile.exists());
        assert!(!workspace.extension_root.exists());
        assert_eq!(fs::read(untouched).unwrap(), b"keep");
        assert!(!workspace.alive().await);
    }
    #[tokio::test]
    async fn cleanup_failure_retains_exact_resources_for_idempotent_retry() {
        let root = tempfile::tempdir().unwrap();
        let workspace = owned(root.path());
        fs::create_dir(&workspace.extension_root).unwrap();
        // A regular file at the expected directory is not silently discarded.
        fs::write(&workspace.profile, b"unexpected fixture").unwrap();
        assert!(workspace.close().await.is_err());
        assert!(workspace.profile.is_file());
        fs::remove_file(&workspace.profile).unwrap();
        fs::create_dir(&workspace.profile).unwrap();
        workspace.close().await.unwrap();
        assert!(!workspace.profile.exists());
    }
    #[test]
    fn view_projection_omits_private_target_and_does_not_guess_ui_kind() {
        let views = BTreeMap::from([(
            "bv-test".into(),
            View {
                target: "secret-native-target".into(),
                resource: "/popup.html".into(),
            },
        )]);
        let value = public_views(&views);
        assert!(!value.to_string().contains("secret"));
        assert_eq!(value[0]["resource"], "/popup.html");
        assert_eq!(value[0]["kind"], "extension_ui");
        assert!(value[0].get("toolbar_popup").is_none());
    }
}
