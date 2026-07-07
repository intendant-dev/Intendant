//! Display glue for the controller: resolving frame hints and
//! attachments, user-display activation/deactivation, CU display
//! targeting, the CU task runner, and shared-view call handling.
//!
//! spawn_user_display_listener (the grant/revoke event listener that
//! drives activate/deactivate here) stays in main.rs: moving that one
//! fn into any module trips ~20 rustc 1.94.0 dead-code false positives
//! across display/, event.rs, frames.rs (bisected 2026-07-05; import
//! style, glob re-export, and visibility ruled out). Move it here when
//! a newer toolchain stops misfiring.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

/// Adapt a display session's [`display::DisplayEvent`] stream onto the
/// EventBus. The display pipeline has no dependency on the event layer;
/// this forwarder is where its lifecycle/telemetry events become
/// `AppEvent`s.
pub(crate) fn display_event_forwarder(bus: EventBus) -> display::DisplayEventSender {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            bus.send(match ev {
                display::DisplayEvent::CaptureLost { display_id, reason } => {
                    AppEvent::DisplayCaptureLost { display_id, reason }
                }
                display::DisplayEvent::Metrics { snapshot } => {
                    AppEvent::DisplayMetrics { snapshot }
                }
                display::DisplayEvent::Resize {
                    display_id,
                    width,
                    height,
                } => AppEvent::DisplayResize {
                    display_id,
                    width,
                    height,
                },
            });
        }
    });
    tx
}

/// Resolve `frames:` context hints into HQ images from the frame registry.
pub(crate) async fn resolve_frame_hints(
    hints: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let mut images = Vec::new();
    for hint in hints {
        if let Some(frame_list) = hint.strip_prefix("frames:") {
            let reg = registry.read().await;
            for fid in frame_list
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                match reg.read_hq(fid) {
                    Ok(data) => {
                        use base64::Engine;
                        images.push(conversation::ImageData {
                            media_type: "image/jpeg".to_string(),
                            data: base64::engine::general_purpose::STANDARD.encode(&data),
                        });
                    }
                    Err(_) => {
                        // Frame not found — skip silently
                    }
                }
            }
        }
    }
    images
}

/// Resolve explicit frame IDs into HQ images from the frame registry.
pub(crate) async fn resolve_frame_ids(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut images = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        match reg.read_hq(fid) {
            Ok(data) => {
                use base64::Engine;
                images.push(conversation::ImageData {
                    media_type: "image/jpeg".to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&data),
                });
            }
            Err(_) => {
                // Frame not found — skip silently
            }
        }
    }
    images
}

/// Resolve frame IDs into `AgentImageAttachment`s for an external agent.
///
/// Captures the on-disk path so backends like Codex can pass `LocalImage`
/// (file reference) instead of inline base64 in JSON-RPC.
#[allow(dead_code)]
pub(crate) async fn resolve_frame_attachments(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<external_agent::AgentImageAttachment> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut atts = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        let Ok(data) = reg.read_hq(fid) else { continue };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let path = reg.path_for(fid);
        atts.push(external_agent::AgentImageAttachment::from_frame_path(
            path,
            base64,
            "image/jpeg".to_string(),
        ));
    }
    atts
}

/// Resolve a mixed list of attachment identifiers (frames from the live
/// frame registry, uploads from the on-disk store) into the unified
/// `AgentAttachment` shape the backends consume.
///
/// Identifier convention:
/// - `"frame:<id>"` or plain `<id>` — a frame registry entry. Plain ids
///   remain supported for backward compatibility with the existing
///   dashboard path that submits frame ids directly.
/// - `"upload:<id>"` — an upload store descriptor. Images load base64
///   inline (for Gemini ACP); files pass through as `AgentAttachment::File`
///   and the backend's default handling prepends a prelude pointing at the
///   on-disk path.
///
/// Order is preserved from the input list so the prelude reads the files
/// in the order the user selected them.
pub(crate) async fn resolve_attachments(
    ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> Vec<external_agent::AgentAttachment> {
    resolve_attachments_with_project_roots(
        ids,
        registry,
        session_dir,
        &[project_root.to_path_buf()],
    )
    .await
}

pub(crate) async fn resolve_attachments_with_project_roots(
    ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_dir: &std::path::Path,
    project_roots: &[PathBuf],
) -> Vec<external_agent::AgentAttachment> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<external_agent::AgentAttachment> = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(upload_id) = raw.strip_prefix("upload:") {
            let Some(d) = project_roots
                .iter()
                .find_map(|root| upload_store::find_upload(upload_id, session_dir, root))
            else {
                continue;
            };
            if d.is_image() {
                // Load the bytes eagerly so Gemini ACP can base64-encode
                // inline. Codex prefers the path.
                let (base64, mime) = match std::fs::read(&d.path) {
                    Ok(bytes) => {
                        use base64::Engine;
                        (
                            base64::engine::general_purpose::STANDARD.encode(&bytes),
                            d.mime.clone(),
                        )
                    }
                    Err(_) => continue,
                };
                out.push(external_agent::AgentAttachment::Image(
                    external_agent::AgentImageAttachment::from_frame_path(
                        d.path.clone(),
                        base64,
                        mime,
                    ),
                ));
            } else {
                out.push(external_agent::AgentAttachment::File(
                    external_agent::AgentFileAttachment {
                        local_path: d.path.clone(),
                        name: d.original_name.clone().unwrap_or_else(|| d.name.clone()),
                        mime_type: d.mime.clone(),
                        size: d.size,
                    },
                ));
            }
            continue;
        }
        // Frame resolution: accept both "frame:<id>" and bare ids for
        // backward compatibility with dashboards that predate the upload
        // feature.
        let fid = raw.strip_prefix("frame:").unwrap_or(raw);
        let (data, path) = {
            let reg = registry.read().await;
            let Ok(data) = reg.read_hq(fid) else {
                continue;
            };
            (data, reg.path_for(fid))
        };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        out.push(external_agent::AgentAttachment::Image(
            external_agent::AgentImageAttachment::from_frame_path(
                path,
                base64,
                "image/jpeg".to_string(),
            ),
        ));
    }
    out
}

/// Auto-attach the latest display frame(s) from the frame registry.
pub(crate) async fn auto_attach_display_frames(
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let reg = registry.read().await;
    let mut images = Vec::new();
    for stream in reg.active_streams() {
        if stream.starts_with("display_") {
            if let Some(frame_id) = reg.latest(Some(&stream)) {
                if let Ok(data) = reg.read_hq(frame_id) {
                    use base64::Engine;
                    images.push(conversation::ImageData {
                        media_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&data),
                    });
                }
            }
        }
    }
    images
}

/// Take a fresh screenshot of the user's display for CU-first routing.
/// Tries DisplaySession first (works on Wayland), falls back to platform tools.
pub(crate) async fn capture_display_screenshot(
    log_dir: &std::path::Path,
    session_registry: &display::SharedSessionRegistry,
) -> Option<conversation::ImageData> {
    // Try DisplaySession first — works on Wayland and any display with a session
    if let Some(session) = session_registry.read().await.get(0) {
        if let Ok(png_bytes) = session.screenshot().await {
            let screenshot_path = log_dir.join("cu_reference.png");
            std::fs::write(&screenshot_path, &png_bytes).ok()?;
            use base64::Engine;
            return Some(conversation::ImageData {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&png_bytes),
            });
        }
    }

    // Fallback: platform-native screenshot tools
    #[cfg(target_os = "linux")]
    crate::linux_display_env::ensure_gui_session_env("fresh display screenshot");

    let screenshot_path = log_dir.join("cu_reference.png");
    let ok = if cfg!(target_os = "macos") {
        tokio::process::Command::new("screencapture")
            .args(["-x", &screenshot_path.to_string_lossy()])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        tokio::process::Command::new("import")
            .args([
                "-window",
                "root",
                "-display",
                &display,
                &screenshot_path.to_string_lossy(),
            ])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !ok {
        return None;
    }
    let data = std::fs::read(&screenshot_path).ok()?;
    use base64::Engine;
    Some(conversation::ImageData {
        media_type: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&data),
    })
}

// Try the CU-first path: send task to the fast CU model.
/// Returns None if CU is not available (no display, no provider).
/// `user_display_granted` is the autonomy guard's grant state, read by the
/// caller at dispatch time.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_cu_first(
    project_root: &std::path::Path,
    reference_images: &[conversation::ImageData],
    frame_images: &[conversation::ImageData],
    task: &str,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    session_registry: &display::SharedSessionRegistry,
    user_display_granted: bool,
) -> Option<Result<CuTaskResult, CallerError>> {
    slog(session_log, |l| {
        l.info(&format!(
            "try_cu_first: ref_images={}, frame_images={}, task={}",
            reference_images.len(),
            frame_images.len(),
            types::truncate_str(task, 60)
        ))
    });

    let reference_images = if reference_images.is_empty() {
        // No frames from browser streaming — try a fresh screenshot if user display
        // is granted, so CU-first can work without the Stream button being active.
        if user_display_granted {
            slog(session_log, |l| {
                l.info("try_cu_first: no registry frames, taking fresh screenshot")
            });
            match capture_display_screenshot(log_dir, session_registry).await {
                Some(img) => vec![img],
                None => {
                    slog(session_log, |l| {
                        l.info("try_cu_first: fresh screenshot failed, returning None")
                    });
                    return None;
                }
            }
        } else {
            slog(session_log, |l| {
                l.info("try_cu_first: no display images and no display grant, returning None")
            });
            return None;
        }
    } else {
        reference_images.to_vec()
    };

    let proj = Project::from_root(project_root.to_path_buf()).ok()?;
    let mut cu_provider = match provider::select_cu_provider(&proj.config.computer_use) {
        Ok(p) => {
            if !p.cu_enabled() {
                slog(session_log, |l| {
                    l.warn("CU provider selected but cu_enabled=false, skipping CU-first")
                });
                return None;
            }
            p
        }
        Err(_) => return None,
    };

    // Override cu_display with the actual display dimensions. The default
    // from select_cu_provider is sized for virtual displays (e.g. 768x1024).
    // On macOS or when targeting the user's real display, the actual resolution
    // may differ (e.g. 1512x949), causing coordinate mismatches.
    if user_display_granted {
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(0);
        let (w, h) = query_display_resolution(display_id);
        if w > 0 && h > 0 {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU display override: {}x{} (actual user display)",
                    w, h
                ))
            });
            cu_provider.set_cu_display((w, h));
        }
    }

    slog(session_log, |l| {
        l.info(&format!(
            "CU-first: {} (provider: {}, model: {})",
            types::truncate_str(task, 80),
            cu_provider.name(),
            cu_provider.model()
        ))
    });
    bus.send(event::AppEvent::PresenceLog {
        message: format!("Trying CU: {}", types::truncate_str(task, 80)),
        level: None,
        turn: None,
    });

    Some(
        run_cu_task(
            cu_provider.as_ref(),
            task,
            reference_images.to_vec(),
            frame_images.to_vec(),
            session_log,
            log_dir,
            bus,
            &proj.config.computer_use,
            None, // auto-resolve display target
            Some(session_registry),
            user_display_granted,
        )
        .await,
    )
}

/// Tear down a user display session on revoke.
///
/// Registry removal is the only part that has to complete before the
/// caller returns — once the session is out of the registry, no new
/// offer can find it. `session.stop()` then tears down the capture,
/// encoder, and clipboard tasks, which can take many seconds (each
/// awaits a thread join). We run that in the background so the
/// caller — `spawn_user_display_listener`'s `rx.recv()` loop — can
/// pick up the next event (e.g. a follow-up `UserDisplayGranted`
/// from a user who toggled off and back on) without waiting for the
/// old session's threads to exit. Before this, a toggle-off-then-on
/// cycle serialized behind `session.stop().await` — "turn on, wait
/// 20+s, turn on is instant" mapped exactly to "the old stop finally
/// finished and the listener got to the new grant".
pub(crate) async fn deactivate_user_display(
    session_registry: &display::SharedSessionRegistry,
    display_id: u32,
) {
    if let Some(session) = session_registry.write().await.remove(display_id) {
        eprintln!(
            "[user_display] Stopping display session for :{}",
            display_id
        );
        tokio::spawn(async move {
            session.stop().await;
        });
    }
}

pub(crate) fn report_user_display_capture_unavailable(
    bus: &EventBus,
    display_id: u32,
    reason: impl Into<String>,
) {
    let reason = reason.into();
    eprintln!("[user_display] {reason}");
    bus.send(AppEvent::DisplayCaptureLost { display_id, reason });
}

/// Handle user display grant: create a `DisplaySession` and emit
/// `DisplayReady` for the selected user display.
///
/// `target_display_id` is the intendant-stable display ID (0 = primary).
/// This wires the user's display into the same lifecycle as virtual displays —
/// the recording listener starts ffmpeg and the web dashboard shows a display slot.
pub(crate) async fn activate_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    target_display_id: u32,
) {
    let display_id: u32 = target_display_id;

    if let Some(session) = session_registry.read().await.get(display_id) {
        let (width, height) = session.resolution();
        eprintln!(
            "[user_display] Display :{} capture already active ({}x{}); skipping activation",
            display_id, width, height
        );
        bus.send(AppEvent::DisplayReady {
            display_id,
            width,
            height,
        });
        return;
    }

    #[cfg(target_os = "linux")]
    crate::linux_display_env::ensure_gui_session_env("user display activation");

    // On Wayland: create a DisplaySession with WaylandBackend.
    // Detect Wayland even when WAYLAND_DISPLAY isn't in our env (e.g. started
    // from a tty/ssh session while a graphical session is active).
    #[cfg(target_os = "linux")]
    let wayland_session_detected =
        std::env::var("WAYLAND_DISPLAY").is_ok() || detect_wayland_socket().is_some();

    #[cfg(target_os = "linux")]
    if wayland_session_detected {
        if let Some(socket) = detect_wayland_socket() {
            if std::env::var("WAYLAND_DISPLAY").is_err() {
                eprintln!(
                    "[user_display] WAYLAND_DISPLAY not set, detected socket: {}",
                    socket
                );
                std::env::set_var("WAYLAND_DISPLAY", &socket);
            }
            if std::env::var("XDG_RUNTIME_DIR").is_err() {
                let uid = crate::platform::current_uid();
                let runtime_dir = format!("/run/user/{}", uid);
                std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
            }
        }
        eprintln!("[user_display] Requesting Wayland screen capture via XDG portal...");
        eprintln!(
            "[user_display] A screen-sharing dialog should appear on the display — \
             enable Allow Remote Interaction, then approve it to enable video capture \
             and Computer Use input"
        );
        let backend = display::wayland::WaylandBackend::new();
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        // The portal dialog requires user interaction on the physical display.
        // If the user is accessing intendant remotely (web dashboard, SSH) they
        // may never see the dialog, so emit a status event for the dashboard to
        // surface a banner — and apply a generous timeout to avoid hanging
        // forever, falling through to X11 capture if the user never approves.
        bus.send(AppEvent::DisplayApprovalPending {
            display_id,
            backend: "wayland",
        });
        const WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS: u64 = 300;
        match tokio::time::timeout(
            std::time::Duration::from_secs(WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS),
            session.start(
                30,
                frame_registry.clone(),
                Some(display_event_forwarder(bus.clone())),
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                // Use the backend's resolution (from portal), not xdpyinfo.
                let (width, height) = session.resolution();
                let session = Arc::new(session);
                session.spawn_metrics_logger(Some(display_event_forwarder(bus.clone())));
                session_registry.write().await.insert(display_id, session);
                bus.send(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                });
                return;
            }
            Ok(Err(e)) => {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!(
                        "Wayland portal activation failed: {e}. Re-request user display access \
                         and approve the GNOME portal with Allow Remote Interaction enabled."
                    ),
                );
                return;
            }
            Err(_) => {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!(
                        "Wayland portal timed out after {WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS}s \
                         (screen-sharing dialog was not approved). Re-request user display access \
                         and approve the GNOME portal with Allow Remote Interaction enabled."
                    ),
                );
                return;
            }
        }
    }

    // X11: detect display and create a DisplaySession with X11Backend.
    #[cfg(target_os = "linux")]
    {
        let has_x11 = std::env::var("DISPLAY").is_ok() || vision::detect_x11_display().is_some();
        if has_x11 {
            // Ensure DISPLAY is set for downstream X11 capture/input paths.
            if std::env::var("DISPLAY").is_err() {
                if let Some(d) = vision::detect_x11_display() {
                    std::env::set_var("DISPLAY", &d);
                }
            }
            // If a specific display was requested, look it up from xrandr
            // enumeration and use X11Backend::with_display() for the
            // matching X display string (e.g. ":0", ":1").
            let backend = if target_display_id != 0 {
                let displays = display::enumerate_displays().await;
                if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                    eprintln!(
                        "[user_display] X11: requested display_id={}, matched '{}'",
                        target_display_id, info.name,
                    );
                    // X11 monitors share the same DISPLAY string -- the
                    // root window spans all monitors.  The enumerated
                    // displays from xrandr are sub-regions of the same
                    // root.  We still create a standard backend capturing
                    // the root window; the per-monitor distinction is used
                    // for coordinate mapping in the CU layer.
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                } else {
                    eprintln!(
                        "[user_display] X11: display_id={} not found, falling back to default",
                        target_display_id,
                    );
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                }
            } else {
                display::x11::X11Backend::new()
                    .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
            };
            if let Ok(backend) = backend {
                let session = display::DisplaySession::new(display_id, Arc::new(backend));
                if let Err(e) = session
                    .start(
                        30,
                        frame_registry.clone(),
                        Some(display_event_forwarder(bus.clone())),
                    )
                    .await
                {
                    eprintln!("[user_display] X11 display session failed: {}", e);
                } else {
                    if wayland_session_detected && x11_fallback_session_is_all_black(&session).await
                    {
                        session.stop().await;
                        report_user_display_capture_unavailable(
                            bus,
                            display_id,
                            "Wayland portal was not approved and X11 fallback captured an \
                             all-black rootless Xwayland frame. Approve the screen-sharing \
                             portal for the user session, or target a virtual Xvfb display \
                             for headed harness work."
                                .to_string(),
                        );
                        return;
                    }
                    let (width, height) = session.resolution();
                    let session = Arc::new(session);
                    session.spawn_metrics_logger(Some(display_event_forwarder(bus.clone())));
                    session_registry.write().await.insert(display_id, session);
                    bus.send(AppEvent::DisplayReady {
                        display_id,
                        width,
                        height,
                    });
                    return;
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // If a specific display was requested, resolve its platform_id (CGDisplayID)
        // from the enumerated list; macOS window entries are synthetic display
        // IDs whose platform_id is the CGWindowID.
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                match info.kind {
                    display::DisplayInfoKind::Display => {
                        display::macos::MacOSBackend::with_display_id(info.platform_id as u32)
                    }
                    display::DisplayInfoKind::Window => {
                        display::macos::MacOSBackend::with_window_id(info.platform_id as u32)
                    }
                }
            } else if let Some(window_id) =
                display::macos::window_id_from_display_id(target_display_id)
            {
                display::macos::MacOSBackend::with_window_id(window_id)
            } else {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!("display {target_display_id} is not available on this Mac"),
                );
                return;
            }
        } else {
            display::macos::MacOSBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session
            .start(30, frame_registry, Some(display_event_forwarder(bus.clone())))
            .await {
            report_user_display_capture_unavailable(
                bus,
                display_id,
                format!("macOS display session failed: {e}"),
            );
            return;
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(display_event_forwarder(bus.clone())));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[cfg(target_os = "windows")]
    {
        // If a specific display was requested, resolve its platform_id (DXGI
        // output ordinal) from the enumerated list; otherwise capture the
        // primary output. Mirrors the macOS arm.
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                display::windows::WindowsBackend::with_output_index(info.platform_id as u32)
            } else {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!("display {target_display_id} is not available on this Windows host"),
                );
                return;
            }
        } else {
            display::windows::WindowsBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session
            .start(30, frame_registry, Some(display_event_forwarder(bus.clone())))
            .await {
            report_user_display_capture_unavailable(
                bus,
                display_id,
                format!("Windows display session failed: {e}"),
            );
            return;
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(display_event_forwarder(bus.clone())));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[allow(unreachable_code)]
    {
        report_user_display_capture_unavailable(
            bus,
            display_id,
            "no supported display backend detected",
        );
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn x11_fallback_session_is_all_black(session: &display::DisplaySession) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(frame) = session.latest_frame().await {
            return !frame_has_visible_rgb(&frame);
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn frame_has_visible_rgb(frame: &display::Frame) -> bool {
    if frame.width == 0 || frame.height == 0 || frame.stride == 0 {
        return false;
    }
    let row_bytes = frame.width as usize * 4;
    let stride = frame.stride as usize;
    if stride < row_bytes || frame.data.len() < stride.saturating_mul(frame.height as usize) {
        return false;
    }

    let total_pixels = frame.width as usize * frame.height as usize;
    let step = (total_pixels / 4096).max(1);
    let mut pixel_index = 0usize;
    for y in 0..frame.height as usize {
        let row = y * stride;
        for x in 0..frame.width as usize {
            if pixel_index % step == 0 {
                let px = row + x * 4;
                if frame.data[px] > 3 || frame.data[px + 1] > 3 || frame.data[px + 2] > 3 {
                    return true;
                }
            }
            pixel_index += 1;
        }
    }
    false
}

/// Auto-register the Windows desktop as an active display at web-daemon
/// startup, so the dashboard's Video tab streams it on connect — no grant
/// click and no running agent required.
///
/// On macOS and Linux the screen is shared behind a consent gate (TCC, the
/// Wayland portal dialog) or a virtual display is launched on demand, so
/// those platforms keep activating the user display only on an explicit
/// grant. Windows has no such per-session consent step: in the headless /
/// RDP server scenario the existing desktop *is* the always-on stream, and
/// the OS-level capture permission is implicit. We therefore mirror the
/// macOS *end state* (a live `DisplaySession` already in the registry, so a
/// fresh browser connect replays `display_ready` and auto-streams) by
/// activating display 0 up front, reusing the same [`activate_user_display`]
/// machinery — which on Windows captures the existing desktop via
/// `WindowsBackend` (DXGI Desktop Duplication), NOT a virtual Xvfb display.
///
/// The autonomy grant flag is set to match a real grant, so the dashboard's
/// "your display" toggle, CU display targeting, and agent subprocesses
/// (which receive the grant on their env at the runtime spawn boundary) all
/// observe a consistent "granted" state. Activation degrades gracefully —
/// if the capture backend can't start (no interactive desktop, etc.)
/// `activate_user_display` logs and returns without registering, leaving
/// the dashboard at "No displays active" rather than failing startup.
#[cfg(target_os = "windows")]
pub(crate) async fn auto_activate_windows_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    autonomy: &SharedAutonomy,
) {
    eprintln!("[user_display] Windows: auto-registering desktop as active display (display 0)");
    {
        let mut guard = autonomy.write().await;
        guard.user_display_granted = true;
    }
    activate_user_display(bus, session_registry, frame_registry, 0).await;
}

/// Detect a Wayland compositor socket even when WAYLAND_DISPLAY is not set.
/// Checks /run/user/<uid>/ for wayland-* sockets.
#[cfg(target_os = "linux")]
pub(crate) fn detect_wayland_socket() -> Option<String> {
    let uid = crate::platform::current_uid();
    let runtime_dir = format!("/run/user/{}", uid);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match "wayland-0", "wayland-1", etc. but not ".lock" files
        if name.starts_with("wayland-") && !name.ends_with(".lock") {
            if entry.file_type().ok().is_some_and(|ft| {
                use std::os::unix::fs::FileTypeExt;
                ft.is_socket() || ft.is_file()
            }) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Parse a display target string from the presence model into a `DisplayTarget`.
///
/// Accepts "user_session" for the user's display, or ":<N>" / "<N>" for virtual.
/// `user_display_granted` is the autonomy guard's grant state, used only by
/// the unrecognized-string fallback resolution.
pub(crate) fn parse_display_target_str(
    s: &str,
    user_display_granted: bool,
) -> computer_use::DisplayTarget {
    match s.trim() {
        "user_session" | "user" | ":0" | "0" => computer_use::DisplayTarget::UserSession,
        other => {
            let num_str = other.trim_start_matches(':');
            if let Ok(id) = num_str.parse::<u32>() {
                if id == 0 {
                    computer_use::DisplayTarget::UserSession
                } else {
                    computer_use::DisplayTarget::Virtual { id }
                }
            } else {
                // Unrecognized — fall back to auto-resolve
                resolve_cu_display_target(user_display_granted)
            }
        }
    }
}

/// Resolve the display target for CU actions.
///
/// If user display access is granted (`user_display_granted`, read from the
/// autonomy guard by the caller) and the current DISPLAY is `:0` (or unset,
/// indicating no virtual display was launched), returns `UserSession`.
/// Otherwise returns `Virtual` with the current display ID. On macOS, always
/// returns `UserSession` when DISPLAY is unset (no Xvfb).
pub(crate) fn resolve_cu_display_target(user_display_granted: bool) -> computer_use::DisplayTarget {
    let display_id: Option<u32> = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| d.trim_start_matches(':').parse().ok());

    let user_granted = user_display_granted;

    match display_id {
        // DISPLAY is :0 and user granted → target user session
        Some(0) if user_granted => computer_use::DisplayTarget::UserSession,
        // DISPLAY is set to a virtual display
        Some(id) => computer_use::DisplayTarget::Virtual { id },
        // No DISPLAY set — if user granted, target their session; else default virtual
        None if user_granted => computer_use::DisplayTarget::UserSession,
        // macOS has no Xvfb — native display is always the target
        None if cfg!(target_os = "macos") => computer_use::DisplayTarget::UserSession,
        None => computer_use::DisplayTarget::Virtual { id: 99 },
    }
}

/// Maximum turns for an ephemeral CU task before giving up.
pub(crate) const CU_TASK_MAX_TURNS: usize = 20;

/// Result of an ephemeral CU task.
pub(crate) enum CuTaskResult {
    /// Task completed by the CU agent.
    Completed(LoopStats),
    /// CU agent determined this isn't a display task; escalate to the full agent.
    Escalate { task: String },
}

/// Run an ephemeral computer-use task with minimal context.
///
/// Creates a lightweight conversation (no project context, skills, or knowledge),
/// runs the CU model for a few turns until the task is done, and returns.
/// `user_display_granted` is the autonomy guard's grant state, read by the
/// caller when the task is dispatched.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_cu_task(
    provider: &dyn provider::ChatProvider,
    task: &str,
    reference_images: Vec<conversation::ImageData>,
    context_images: Vec<conversation::ImageData>,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    cu_config: &project::ComputerUseConfig,
    target_override: Option<computer_use::DisplayTarget>,
    session_registry: Option<&display::SharedSessionRegistry>,
    user_display_granted: bool,
) -> Result<CuTaskResult, CallerError> {
    // Owned form for execute_actions, which wants `&Option<_>`.
    let session_registry = session_registry.cloned();
    let mut stats = LoopStats::default();
    let mut cu_counter = 0u64;
    let backend = computer_use::DisplayBackend::from_config(&cu_config.backend);

    let display_target =
        target_override.unwrap_or_else(|| resolve_cu_display_target(user_display_granted));

    // CU-first system prompt: handle display tasks or escalate
    let system_prompt =
        "You are a fast computer-use agent. You can see and interact with a desktop display.\n\n\
        ROUTING:\n\
        - If the task involves the display (clicking, typing, scrolling, pressing buttons, \
          opening apps, interacting with GUI elements), handle it with your computer use tools.\n\
        - If the task is NOT about the display (coding, file editing, research, shell commands, \
          git, debugging, questions), call escalate_to_agent with the task description.\n\
        - If no display screenshot is provided below, call escalate_to_agent immediately.\n\n\
        WHEN HANDLING DISPLAY TASKS:\n\
        1. Examine the screenshot to identify target elements\n\
        2. Perform the required actions\n\
        3. Take a verification screenshot\n\
        4. Respond with DONE and a one-sentence summary\n\n\
        RULES:\n\
        - Perform ONLY the requested task, nothing else.\n\
        - Once done, STOP. Do not take additional actions.\n\
        - Be precise with coordinates. Act efficiently."
            .to_string();

    // No display frames at all → escalate immediately without API call
    if reference_images.is_empty() && context_images.is_empty() {
        slog(session_log, |l| {
            l.info("CU: no display frames available, escalating")
        });
        return Ok(CuTaskResult::Escalate {
            task: task.to_string(),
        });
    }

    let ref_image_count = reference_images.len();
    let mut conv = Conversation::new(system_prompt, provider.context_window());

    // Inject reference frames
    if !reference_images.is_empty() {
        conv.add_user_with_images(
            "The user was looking at this screen when they made their request:".to_string(),
            reference_images,
        );
        conv.add_assistant(
            "I can see the reference screen. I'll compare this with the current state.".to_string(),
        );
    }

    // Inject context images
    if !context_images.is_empty() {
        conv.add_user_with_images("Additional context:".to_string(), context_images);
        conv.add_assistant("Noted.".to_string());
    }

    // Add the task
    conv.add_user(task.to_string());

    slog(session_log, |l| {
        l.cu_task_start(
            task,
            provider.name(),
            provider.model(),
            provider.cu_enabled(),
            provider.cu_display(),
            ref_image_count,
        )
    });

    for turn in 1..=CU_TASK_MAX_TURNS {
        stats.turns = turn;

        slog(session_log, |l| {
            l.info(&format!("CU turn {} starting", turn))
        });

        let response = provider
            .chat_stream(conv.messages(), &|event| {
                if let provider::StreamEvent::Delta(ref delta) = event {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("[CU] {}", delta),
                        level: None,
                        turn: Some(turn),
                    });
                }
            })
            .await?;

        conv.set_usage(response.usage.clone());

        // Log structured CU turn
        {
            let mut actions_desc: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| cu.actions.iter().map(|a| format!("{:?}", a)))
                .collect();
            for tc in &response.tool_calls {
                actions_desc.push(format!(
                    "{}({})",
                    tc.name,
                    types::truncate_str(&tc.arguments, 100)
                ));
            }
            slog(session_log, |l| {
                l.cu_turn(
                    turn,
                    response.content.len(),
                    response.cu_calls.len(),
                    response.tool_calls.len(),
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    &actions_desc,
                )
            });
        }
        if !response.content.is_empty() {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} text: {}",
                    turn,
                    types::truncate_str(&response.content, 500)
                ))
            });
        }
        // Check for escalation before processing anything else
        if let Some(esc_call) = response
            .tool_calls
            .iter()
            .find(|tc| tc.name == "escalate_to_agent")
        {
            let args: serde_json::Value =
                serde_json::from_str(&esc_call.arguments).unwrap_or_default();
            let escalated_task = args["task"].as_str().unwrap_or(task).to_string();
            slog(session_log, |l| {
                l.cu_task_error("escalated", Some(&escalated_task))
            });
            return Ok(CuTaskResult::Escalate {
                task: escalated_task,
            });
        }

        // Handle unrecognized function tool calls: return error results so the
        // model knows these tools are not available in CU mode.
        let non_escalate_tools: Vec<_> = response
            .tool_calls
            .iter()
            .filter(|tc| tc.name != "escalate_to_agent")
            .collect();
        if !non_escalate_tools.is_empty() {
            let refs: Vec<conversation::ToolCallRef> = non_escalate_tools
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
            for tc in &non_escalate_tools {
                slog(session_log, |l| {
                    l.warn(&format!(
                        "CU turn {}: unrecognized tool '{}' — returning error result",
                        turn, tc.name
                    ))
                });
                conv.add_tool_result(
                    &tc.id,
                    &tc.name,
                    &format!(
                        "Error: tool '{}' is not available in computer-use mode. \
                         Use your native computer use actions (click, type, scroll, screenshot) \
                         or call escalate_to_agent to hand off to the coding agent.",
                        tc.name
                    ),
                );
            }
            continue; // let model see the error results
        }

        // Check for task completion
        let content_lower = response.content.to_lowercase();
        let is_done = content_lower.contains("done")
            && response.cu_calls.is_empty()
            && response.tool_calls.is_empty();

        // Store assistant message
        if !response.cu_calls.is_empty() {
            // CU calls: store as assistant with tool call refs
            let refs: Vec<conversation::ToolCallRef> = response
                .cu_calls
                .iter()
                .map(|cu| conversation::ToolCallRef {
                    id: cu.call_id.clone(),
                    call_id: cu.call_id.clone(),
                    name: "computer".to_string(),
                    arguments: String::new(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
        } else {
            conv.add_assistant(response.content.clone());
        }

        if is_done {
            let summary = types::truncate_str(&response.content, 200);
            slog(session_log, |l| l.cu_task_complete(turn, true, summary));
            break;
        }

        // Execute CU calls
        if !response.cu_calls.is_empty() {
            for cu_call in &response.cu_calls {
                slog(session_log, |l| {
                    l.info(&format!(
                        "CU turn {}: {} action(s)",
                        turn,
                        cu_call.actions.len()
                    ))
                });

                let results = computer_use::execute_actions(
                    &cu_call.actions,
                    display_target,
                    backend,
                    log_dir,
                    &mut cu_counter,
                    &session_registry,
                    None,
                    user_display_granted,
                )
                .await;

                let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
                let output = if results.iter().all(|r| r.success) {
                    "Actions executed successfully.".to_string()
                } else {
                    let errors: Vec<&str> =
                        results.iter().filter_map(|r| r.error.as_deref()).collect();
                    format!("Some actions failed: {}", errors.join("; "))
                };

                if let Some(screenshot) = last_screenshot {
                    let images = vec![conversation::ImageData {
                        media_type: "image/png".to_string(),
                        data: screenshot.base64_png.clone(),
                    }];
                    conv.add_cu_result(&cu_call.call_id, &output, images);
                } else {
                    conv.add_cu_result(&cu_call.call_id, &output, vec![]);
                }
            }
            continue; // next turn — let model see the results
        }

        // No CU calls and not done — model may be thinking or confused
        if response.cu_calls.is_empty() && response.tool_calls.is_empty() && !is_done {
            slog(session_log, |l| {
                l.cu_task_error(
                    &format!("CU turn {}: no actions returned (text-only response)", turn),
                    None,
                )
            });
        }
        if turn >= CU_TASK_MAX_TURNS {
            slog(session_log, |l| {
                l.cu_task_error("CU task hit max turns", None)
            });
        }
    }

    Ok(CuTaskResult::Completed(stats))
}

/// Execute native computer-use tool calls via the platform-native executor
/// and add results (with screenshots) to the conversation.
/// Handle native `shared_view` tool calls: dashboard visibility into
/// agent-owned displays (sandboxes, VMs, virtual displays). Sharing the
/// user's own screen is explicit opt-in — unlike the MCP path, this handler
/// refuses to flip the display grant itself and instead tells the model the
/// user must grant the display first; input authority is only ever granted
/// by the user from the dashboard.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_shared_view_calls(
    shared_view_calls: &[(String, serde_json::Value)],
    conversation: &mut conversation::Conversation,
    bus: &EventBus,
    autonomy: &SharedAutonomy,
    session_registry: Option<&display::SharedSessionRegistry>,
    session_id: Option<String>,
    log_dir: &std::path::Path,
    cu_counter: &mut u64,
    session_log: &SharedSessionLog,
) {
    for (call_id, args) in shared_view_calls {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or_default();
        let display_target = args
            .get("display_target")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let reason = args
            .get("reason")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let region = args.get("region").and_then(|r| {
            Some(mcp::normalize_shared_view_region_xywh(
                r.get("x")?.as_f64()?,
                r.get("y")?.as_f64()?,
                r.get("width")?.as_f64()?,
                r.get("height")?.as_f64()?,
            ))
        });

        let resolved_target = mcp::shared_view_display_target(display_target, None);
        let display_id = mcp::shared_view_display_id(resolved_target.as_deref(), None);
        let label = mcp::shared_view_target_label(display_id, resolved_target.as_deref());

        // The user's own screen is an explicit opt-in path: require the
        // existing display grant instead of flipping it from a tool call.
        // Only display-exposing verbs gate — focus/input/hide operate on
        // whatever view is already shown.
        let user_display_granted = autonomy.read().await.user_display_granted;
        let effective_user_display = match display_id {
            Some(0) => true,
            Some(_) => false,
            None => matches!(
                resolve_cu_display_target(user_display_granted),
                computer_use::DisplayTarget::UserSession
            ),
        };
        if matches!(action, "show" | "capture") && effective_user_display && !user_display_granted {
            conversation.add_tool_result(
                call_id,
                "shared_view",
                "Error: sharing the user's own screen (user_session) is an explicit opt-in — \
                 the user must grant their display first (dashboard grant or \
                 grant_user_display). Share an agent-owned display instead, e.g. \
                 display_target \"99\" for the virtual display you are working on.",
            );
            continue;
        }

        let emit = |action: &str, note: Option<String>| AppEvent::SharedView {
            session_id: session_id.clone(),
            action: action.to_string(),
            display_target: resolved_target.clone(),
            display_id,
            reason: reason.clone(),
            region: region.clone(),
            note,
        };

        let output = match action {
            "show" => {
                // (Re)activate a granted user display whose session is gone;
                // the grant listener owns the platform work.
                if display_id == Some(0) {
                    let session_missing = match session_registry {
                        Some(registry) => registry.read().await.get(0).is_none(),
                        None => false,
                    };
                    if session_missing {
                        bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                    }
                }
                bus.send(emit("show", None));
                format!("Shared view shown for {label} — the dashboard is now streaming it.")
            }
            "focus" => match region {
                Some(_) => {
                    bus.send(emit("focus", None));
                    format!("Focus highlighted on {label}.")
                }
                None => "Error: focus requires a region {x, y, width, height} with 0.0-1.0 \
                         fractions."
                    .to_string(),
            },
            "capture" => {
                bus.send(emit("capture", None));
                let target = match display_id {
                    Some(0) => computer_use::DisplayTarget::UserSession,
                    Some(id) => computer_use::DisplayTarget::Virtual { id },
                    None => resolve_cu_display_target(user_display_granted),
                };
                let screenshot_dir = log_dir.join("screenshots");
                let _ = std::fs::create_dir_all(&screenshot_dir);
                let registry = session_registry.cloned();
                let results = computer_use::execute_actions(
                    &[computer_use::CuAction::Screenshot],
                    target,
                    computer_use::DisplayBackend::detect(),
                    &screenshot_dir,
                    cu_counter,
                    &registry,
                    None,
                    user_display_granted,
                )
                .await;
                match results.first().and_then(|r| r.screenshot.as_ref()) {
                    Some(shot) => {
                        let images = vec![conversation::ImageData {
                            media_type: "image/png".to_string(),
                            data: shot.base64_png.clone(),
                        }];
                        conversation.add_tool_result_with_images(
                            call_id,
                            "shared_view",
                            &format!("Captured the current frame of {label}."),
                            images,
                        );
                        continue;
                    }
                    None => format!(
                        "Error: no frame available for {label}: {}",
                        results
                            .first()
                            .and_then(|r| r.error.as_deref())
                            .unwrap_or("unknown capture failure")
                    ),
                }
            }
            "input" => {
                bus.send(emit("input", None));
                format!(
                    "Input authority requested for {label}. The user must accept from the \
                     dashboard control — continue only after they take over or respond."
                )
            }
            "hide" => {
                bus.send(emit("hide", None));
                "Shared view dismissed.".to_string()
            }
            other => format!(
                "Error: unknown shared_view action '{other}' — use show, focus, capture, \
                 input, or hide."
            ),
        };
        slog(session_log, |l| {
            l.info(&format!("shared_view {action}: {label}"))
        });
        conversation.add_tool_result(call_id, "shared_view", &output);
    }
}

/// `user_display_granted` is the autonomy guard's grant state, read by the
/// caller before dispatching the batch.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_cu_calls(
    cu_calls: &[computer_use::CuToolCall],
    conversation: &mut conversation::Conversation,
    cu_display: Option<(u32, u32)>,
    log_dir: &std::path::Path,
    counter: &mut u64,
    session_log: &SharedSessionLog,
    session_registry: Option<&display::SharedSessionRegistry>,
    user_display_granted: bool,
) {
    // Owned form for execute_actions, which wants `&Option<_>`.
    let session_registry = session_registry.cloned();
    let display_target = if cu_display.is_some() {
        resolve_cu_display_target(user_display_granted)
    } else {
        // No CU display configured — default to virtual :99
        computer_use::DisplayTarget::Virtual { id: 99 }
    };

    for cu_call in cu_calls {
        // Build human-readable description of CU actions
        let action_descs: Vec<String> = cu_call
            .actions
            .iter()
            .map(|a| match a {
                computer_use::CuAction::Click { x, y, button } => {
                    format!("click({},{} {:?})", x, y, button)
                }
                computer_use::CuAction::DoubleClick { x, y, .. } => {
                    format!("double_click({},{})", x, y)
                }
                computer_use::CuAction::Type { text } => {
                    format!("type(\"{}\")", types::truncate_str(text, 50))
                }
                computer_use::CuAction::Key { key } => format!("key({})", key),
                computer_use::CuAction::Scroll {
                    x,
                    y,
                    direction,
                    amount,
                } => format!("scroll({},{} {:?} {})", x, y, direction, amount),
                computer_use::CuAction::MoveMouse { x, y } => format!("move({},{})", x, y),
                computer_use::CuAction::Drag {
                    start_x,
                    start_y,
                    end_x,
                    end_y,
                } => format!("drag({},{}->{},{})", start_x, start_y, end_x, end_y),
                computer_use::CuAction::TripleClick { x, y, .. } => {
                    format!("triple_click({},{})", x, y)
                }
                computer_use::CuAction::MouseDown { x, y, .. } => {
                    format!("mouse_down({},{})", x, y)
                }
                computer_use::CuAction::MouseUp { x, y, .. } => format!("mouse_up({},{})", x, y),
                computer_use::CuAction::Paste { text } => {
                    format!("paste(\"{}\")", types::truncate_str(text, 50))
                }
                computer_use::CuAction::HoldKey { key, ms } => {
                    format!("hold_key({},{}ms)", key, ms)
                }
                computer_use::CuAction::Zoom {
                    x,
                    y,
                    width,
                    height,
                } => format!("zoom({},{} {}x{})", x, y, width, height),
                computer_use::CuAction::Screenshot => "screenshot".to_string(),
                computer_use::CuAction::Wait { ms } => format!("wait({}ms)", ms),
            })
            .collect();
        let desc = action_descs.join(" → ");
        slog(session_log, |l| l.info(&format!("CU: {}", desc)));

        let backend = computer_use::DisplayBackend::detect();
        let results = computer_use::execute_actions(
            &cu_call.actions,
            display_target,
            backend,
            log_dir,
            counter,
            &session_registry,
            None,
            user_display_granted,
        )
        .await;

        // Find the last screenshot from results
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        let output = if results.iter().all(|r| r.success) {
            "Actions executed successfully.".to_string()
        } else {
            let errors: Vec<&str> = results.iter().filter_map(|r| r.error.as_deref()).collect();
            format!("Some actions failed: {}", errors.join("; "))
        };

        if let Some(screenshot) = last_screenshot {
            let images = vec![conversation::ImageData {
                media_type: "image/png".to_string(),
                data: screenshot.base64_png.clone(),
            }];
            conversation.add_cu_result(&cu_call.call_id, &output, images);
        } else {
            conversation.add_cu_result(&cu_call.call_id, &output, vec![]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[tokio::test]
    async fn resolve_attachments_includes_uploaded_files_and_images() {
        use std::io::Write as _;

        fn upload_tempfile(bytes: &[u8]) -> tempfile::NamedTempFile {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            file.flush().unwrap();
            file
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let file_upload = upload_store::commit_upload(
            upload_tempfile(b"a,b\n1,2\n"),
            "data.csv",
            "text/csv",
            8,
            upload_store::UploadDestination::Workspace,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();
        let image_upload = upload_store::commit_upload(
            upload_tempfile(b"not-really-a-png"),
            "screen.png",
            "image/png",
            16,
            upload_store::UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        let registry = Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
            &session_dir,
        )));
        let ids = vec![
            format!("upload:{}", file_upload.id),
            format!("upload:{}", image_upload.id),
        ];
        let attachments = resolve_attachments(&ids, &registry, &session_dir, &project_root).await;

        assert_eq!(attachments.len(), 2);
        match &attachments[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "data.csv");
                assert_eq!(file.mime_type, "text/csv");
                assert_eq!(file.size, 8);
                assert!(file.local_path.starts_with(
                    project_root
                        .join(".intendant")
                        .join("uploads")
                        .join("sess-1")
                ));
            }
            other => panic!("expected file upload attachment, got {other:?}"),
        }
        match &attachments[1] {
            external_agent::AgentAttachment::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert_eq!(image.local_path.as_ref(), Some(&image_upload.path));
                assert!(!image.base64.is_empty());
            }
            other => panic!("expected image upload attachment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_attachments_falls_back_to_daemon_project_uploads() {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"pending upload").unwrap();
        file.flush().unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("new-session-log");
        let launch_project_root = tmp.path().join("launch-project");
        let daemon_project_root = tmp.path().join("daemon-project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&launch_project_root).unwrap();
        std::fs::create_dir_all(&daemon_project_root).unwrap();

        let upload = upload_store::commit_upload(
            file,
            "pending.txt",
            "text/plain",
            14,
            upload_store::UploadDestination::Task,
            &session_dir,
            "daemon-session",
            &daemon_project_root,
        )
        .unwrap();

        let registry = Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
            &session_dir,
        )));
        let ids = vec![format!("upload:{}", upload.id)];
        assert!(
            resolve_attachments(&ids, &registry, &session_dir, &launch_project_root)
                .await
                .is_empty(),
            "single-root lookup should not find uploads committed under another project"
        );

        let roots = vec![launch_project_root, daemon_project_root];
        let attachments =
            resolve_attachments_with_project_roots(&ids, &registry, &session_dir, &roots).await;

        assert_eq!(attachments.len(), 1);
        match &attachments[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "pending.txt");
                assert_eq!(file.local_path, upload.path);
            }
            other => panic!("expected fallback file upload attachment, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn black_frame_detector_rejects_all_zero_rgb() {
        let frame = display::Frame {
            data: vec![0; 4 * 4 * 4],
            format: display::FrameFormat::Bgra,
            width: 4,
            height: 4,
            stride: 16,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        };

        assert!(!frame_has_visible_rgb(&frame));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn black_frame_detector_accepts_visible_rgb() {
        let mut data = vec![0; 4 * 4 * 4];
        data[10] = 64;
        let frame = display::Frame {
            data,
            format: display::FrameFormat::Bgra,
            width: 4,
            height: 4,
            stride: 16,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        };

        assert!(frame_has_visible_rgb(&frame));
    }

    struct ActiveDisplayBackend {
        width: u32,
        height: u32,
    }

    #[async_trait::async_trait]
    impl display::DisplayBackend for ActiveDisplayBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<tokio::sync::mpsc::Receiver<display::Frame>, error::CallerError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }

        async fn stop_capture(&self) {}

        async fn inject_input(
            &self,
            _event: display::InputEvent,
        ) -> Result<(), error::CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    #[test]
    fn activate_user_display_skips_activation_when_capture_already_active() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let backend = std::sync::Arc::new(ActiveDisplayBackend {
                width: 1920,
                height: 1080,
            });
            let session = std::sync::Arc::new(display::DisplaySession::new(0, backend));
            let mut registry = display::SessionRegistry::new();
            registry.insert(0, session);
            let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));

            activate_user_display(&bus, &registry, None, 0).await;

            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                })) => {
                    assert_eq!(display_id, 0);
                    assert_eq!((width, height), (1920, 1080));
                }
                other => panic!("expected DisplayReady for active capture, got {other:?}"),
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                    .await
                    .is_err(),
                "already-active capture should not emit a portal-pending event"
            );
        });
    }

    #[tokio::test]
    async fn shared_view_calls_validate_and_gate_user_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(tmp.path().to_path_buf()).unwrap(),
        ));
        let mut conversation = Conversation::new("system".to_string(), 100_000);
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let autonomy = autonomy::shared_autonomy(autonomy::AutonomyState::default());
        let mut counter = 0u64;

        let calls = vec![
            ("c1".to_string(), serde_json::json!({"action": "hide"})),
            // focus without a region must fail fast.
            ("c2".to_string(), serde_json::json!({"action": "focus"})),
            // user_session without the display grant is refused (explicit opt-in).
            (
                "c3".to_string(),
                serde_json::json!({"action": "show", "display_target": "user_session"}),
            ),
            ("c4".to_string(), serde_json::json!({"action": "bogus"})),
        ];
        handle_shared_view_calls(
            &calls,
            &mut conversation,
            &bus,
            &autonomy,
            None,
            Some("sess-1".to_string()),
            tmp.path(),
            &mut counter,
            &session_log,
        )
        .await;

        let results: Vec<_> = conversation
            .messages()
            .iter()
            .filter(|m| m.role == "tool")
            .collect();
        assert_eq!(results.len(), 4, "one result per call");
        assert!(
            results[0].content.contains("dismissed"),
            "{}",
            results[0].content
        );
        assert!(
            results[1].content.contains("requires a region"),
            "{}",
            results[1].content
        );
        assert!(
            results[2].content.contains("explicit opt-in"),
            "{}",
            results[2].content
        );
        assert!(
            results[3].content.contains("unknown shared_view action"),
            "{}",
            results[3].content
        );

        // Only the valid hide emitted a SharedView event; the gated and
        // invalid calls must not reach the dashboard.
        match rx.try_recv() {
            Ok(AppEvent::SharedView {
                action, session_id, ..
            }) => {
                assert_eq!(action, "hide");
                assert_eq!(session_id.as_deref(), Some("sess-1"));
            }
            other => panic!("expected SharedView hide event, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no further events expected");
    }
}
