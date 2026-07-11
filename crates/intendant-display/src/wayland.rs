//! Wayland display backend using XDG Desktop Portal (ashpd) for screen capture
//! and input injection, and PipeWire for frame acquisition.
//!
//! The PipeWire main loop runs on a dedicated `std::thread` (it is not `Send`).
//! Communication with the tokio runtime is via a bounded `mpsc` channel for
//! frames and an `AtomicBool` for shutdown signaling.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use async_trait::async_trait;
use futures_util::StreamExt;
use intendant_core::error::CallerError;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex, RwLock};

/// Enumerate Wayland displays.
///
/// Wayland portals do not expose display enumeration -- the user selects which
/// monitor to share via the portal dialog.  We return a single entry that
/// honestly represents this behavior.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    vec![super::DisplayInfo {
        id: 0,
        platform_id: 0,
        name: "Wayland Display (portal-selected)".to_string(),
        width: 1920,
        height: 1080,
        is_primary: true,
        kind: super::DisplayInfoKind::Display,
        application_name: None,
        window_title: None,
    }]
}

/// Portal session handle + PipeWire capture thread.
///
/// Stores the `RemoteDesktop` proxy and its `Session` handle so that
/// `inject_input()` can call the `notify_*` D-Bus methods on the original
/// portal session.  Both types carry a `'static` lifetime because the
/// underlying `zbus::Connection` is held in a global `OnceLock`.
struct PortalSession {
    /// The PipeWire node ID (used for pointer_motion_absolute stream param).
    node_id: u32,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    /// The RemoteDesktop D-Bus proxy, kept alive for input injection.
    remote_desktop: RemoteDesktop<'static>,
    /// The session handle obtained from `create_session()`.
    session: Session<'static, RemoteDesktop<'static>>,
    /// Clipboard portal proxy, present when the session was armed for
    /// clipboard access before `Start` (the portal requires the capability
    /// request up front). Consumed by the paste path; `None` when the
    /// portal backend lacks the Clipboard interface.
    clipboard: Option<Clipboard<'static>>,
}

/// Wayland screen capture and input injection backend.
///
/// Uses the XDG Desktop Portal `RemoteDesktop` + `ScreenCast` interfaces for a
/// combined session that provides both keyboard/pointer injection and PipeWire
/// video frames.
pub struct WaylandBackend {
    portal_session: Mutex<Option<PortalSession>>,
    resolution: RwLock<(u32, u32)>,
    /// Shared atomics so the PipeWire thread can update resolution on resize.
    shared_width: Arc<AtomicU32>,
    shared_height: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
}

impl WaylandBackend {
    /// Create a new backend. Resolution is populated once capture starts.
    pub fn new() -> Self {
        Self {
            portal_session: Mutex::new(None),
            resolution: RwLock::new((0, 0)),
            shared_width: Arc::new(AtomicU32::new(0)),
            shared_height: Arc::new(AtomicU32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for WaylandBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DisplayBackend for WaylandBackend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Defensive: matching the x11.rs pattern — teardown any previous
        // capture (portal session + pipewire stream) before starting a new
        // one, so a double-start doesn't leak the portal session. The
        // portal dialog re-prompts on every new RemoteDesktop session
        // anyway, so losing the old session is the right behavior here.
        // `stop_capture` is idempotent when nothing's running.
        self.stop_capture().await;

        self.shutdown.store(false, Ordering::SeqCst);

        // --- Portal session: RemoteDesktop + ScreenCast combined ---
        let remote_desktop = RemoteDesktop::new()
            .await
            .map_err(|e| CallerError::Display(format!("RemoteDesktop proxy: {e}")))?;
        let screencast = Screencast::new()
            .await
            .map_err(|e| CallerError::Display(format!("ScreenCast proxy: {e}")))?;

        let session = remote_desktop
            .create_session()
            .await
            .map_err(|e| CallerError::Display(format!("create session: {e}")))?;

        // PersistMode::DoNot is forced: joint RemoteDesktop+ScreenCast
        // sessions can't use restore_token persistence on GNOME
        // (xdg-desktop-portal-gnome rejects select_sources with
        // "Remote desktop sessions cannot persist"). Only pure ScreenCast
        // (view-only) sessions can persist. To skip the approval dialog
        // on subsequent runs you'd have to split into a persistent
        // ScreenCast session for video and a separate RemoteDesktop
        // session (ephemeral) for input injection — which changes the
        // backend lifecycle meaningfully and still shows the input
        // dialog on every run. We accept the every-launch dialog for
        // now and surface it via the DisplayApprovalPending banner so
        // remote users know to look at the guest desktop.
        remote_desktop
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(|e| CallerError::Display(format!("select devices: {e}")))?;

        screencast
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor | SourceType::Window,
                true,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(|e| CallerError::Display(format!("select sources: {e}")))?;

        // --- Clipboard capability (optional, must precede Start) ---
        // Arms the session for later paste operations; no clipboard data
        // moves here. portal backends without the Clipboard interface
        // (pre-45 portal-gnome, portal-gtk) leave it None and paste keeps
        // its unsupported error while capture and input proceed normally.
        let clipboard = match Clipboard::new().await {
            Ok(proxy) => match proxy.request(&session).await {
                Ok(()) => Some(proxy),
                Err(e) => {
                    eprintln!("[display/wayland] clipboard portal request failed: {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!("[display/wayland] clipboard portal unavailable: {e}");
                None
            }
        };

        let started = remote_desktop
            .start(&session, None)
            .await
            .map_err(|e| CallerError::Display(format!("start request: {e}")))?
            .response()
            .map_err(|e| CallerError::Display(format!("start response: {e}")))?;

        // Extract PipeWire node ID from the screencast streams.
        let streams = started.streams().ok_or_else(|| {
            CallerError::Display("no screencast streams returned by portal".to_string())
        })?;
        if streams.is_empty() {
            return Err(CallerError::Display(
                "empty stream list from portal".to_string(),
            ));
        }

        let stream = &streams[0];
        let node_id = stream.pipe_wire_node_id();
        let (width, height) = match stream.size() {
            Some((w, h)) => (w as u32, h as u32),
            None => (1920, 1080),
        };

        eprintln!(
            "[display/wayland] Portal granted stream: node_id={}, {}x{}, {} stream(s) available",
            node_id,
            width,
            height,
            streams.len(),
        );

        *self.resolution.write().await = (width, height);
        self.shared_width.store(width, Ordering::SeqCst);
        self.shared_height.store(height, Ordering::SeqCst);

        // Get PipeWire fd via the screencast portal.
        let pw_fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|e| CallerError::Display(format!("pipewire fd: {e}")))?;

        // --- Bounded frame channel: PipeWire thread -> tokio ---
        let (tx, rx) = mpsc::channel::<Frame>(4);

        // --- Spawn dedicated PipeWire thread ---
        let shutdown_flag = Arc::clone(&self.shutdown);
        let shared_w = Arc::clone(&self.shared_width);
        let shared_h = Arc::clone(&self.shared_height);
        let mut pw_thread = Some(std::thread::spawn(move || {
            run_pipewire_capture(
                pw_fd,
                node_id,
                tx,
                shutdown_flag,
                width,
                height,
                target_pipewire_framerate(fps),
                shared_w,
                shared_h,
            );
        }));

        // Prove the approved portal session includes RemoteDesktop input
        // authority before exposing it as an active DisplaySession. GNOME can
        // approve screen capture while leaving "Allow Remote Interaction" off;
        // that still yields PipeWire frames, so screenshots work, but all
        // notify_* input calls fail later with an inactive-session portal
        // error. The same 1-pixel cursor wiggle also wakes Mutter's screencast
        // pipeline after the PipeWire consumer is connected, avoiding an idle
        // black first frame.
        if let Err(e) = verify_remote_interaction(&remote_desktop, &session).await {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = pw_thread.take() {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = handle.join();
                })
                .await;
            }
            let _ = session.close().await;
            return Err(e);
        }

        // Store the RemoteDesktop proxy and session handle so inject_input()
        // can call notify_* methods on the original portal session.
        *self.portal_session.lock().await = Some(PortalSession {
            node_id,
            pw_thread,
            remote_desktop,
            session,
            clipboard,
        });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // Teardown contract: the PipeWire thread owns every sender clone of
        // the frame channel (the process-callback clone lives in the stream
        // listener, dropped when the thread's mainloop returns), so the join
        // below doubles as the bounded channel-close. Taking the state makes
        // double-stop / stop-without-start no-ops.
        if let Some(mut ps) = self.portal_session.lock().await.take() {
            if let Some(handle) = ps.pw_thread.take() {
                // Same rationale as x11.rs: don't block the executor on
                // a std::thread::join — any other async task scheduled
                // on this thread (the WebSocket outbound pump, the
                // spawn_user_display_listener loop itself) stalls for
                // the duration, making revokes feel flaky.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = handle.join();
                })
                .await;
            }
            // Explicitly close the portal session so the GNOME sharing
            // indicator disappears immediately on revoke.
            let _ = ps.session.close().await;
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        // Read the latest resolution from shared atomics (updated by the
        // PipeWire thread when frame dimensions change).
        let width = self.shared_width.load(Ordering::SeqCst);
        let height = self.shared_height.load(Ordering::SeqCst);
        let guard = self.portal_session.lock().await;
        let ps = guard.as_ref().ok_or_else(|| {
            CallerError::Display("no active portal session for input injection".to_string())
        })?;

        let rd = &ps.remote_desktop;
        let session = &ps.session;
        let node_id = ps.node_id;

        match event {
            InputEvent::KeyDown { ref code, .. } => {
                let keycode = super::keymap::dom_code_to_evdev(code).ok_or_else(|| {
                    CallerError::Display(format!("unsupported Wayland key code: {code}"))
                })?;
                rd.notify_keyboard_keycode(session, keycode as i32, KeyState::Pressed)
                    .await
                    .map_err(|e| wayland_input_error("key inject", e))?;
            }
            InputEvent::KeyUp { ref code, .. } => {
                let keycode = super::keymap::dom_code_to_evdev(code).ok_or_else(|| {
                    CallerError::Display(format!("unsupported Wayland key code: {code}"))
                })?;
                rd.notify_keyboard_keycode(session, keycode as i32, KeyState::Released)
                    .await
                    .map_err(|e| wayland_input_error("key inject", e))?;
            }
            InputEvent::MouseMove { x, y, .. } => {
                rd.notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x * width as f64,
                    y * height as f64,
                )
                .await
                .map_err(|e| wayland_input_error("pointer inject", e))?;
            }
            InputEvent::MouseDown { x, y, b } => {
                // Move to position first. If the reposition fails, the press
                // would land wherever the pointer happens to be — propagate
                // the error instead of clicking a place the model never chose.
                rd.notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x * width as f64,
                    y * height as f64,
                )
                .await
                .map_err(|e| wayland_input_error("pointer move (before press)", e))?;
                // Linux evdev button codes: BTN_LEFT=0x110, BTN_MIDDLE=0x112, BTN_RIGHT=0x111
                let button_code: i32 = match b {
                    0 => 0x110,
                    1 => 0x112,
                    2 => 0x111,
                    _ => 0x110,
                };
                rd.notify_pointer_button(session, button_code, KeyState::Pressed)
                    .await
                    .map_err(|e| wayland_input_error("button inject", e))?;
            }
            InputEvent::MouseUp { x, y, b } => {
                rd.notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x * width as f64,
                    y * height as f64,
                )
                .await
                .map_err(|e| wayland_input_error("pointer move (before release)", e))?;
                let button_code: i32 = match b {
                    0 => 0x110,
                    1 => 0x112,
                    2 => 0x111,
                    _ => 0x110,
                };
                rd.notify_pointer_button(session, button_code, KeyState::Released)
                    .await
                    .map_err(|e| wayland_input_error("button inject", e))?;
            }
            InputEvent::Scroll { dx, dy, .. } => {
                // Use discrete axis scrolling: convert raw deltas to integer
                // steps. Vertical scroll (dy) maps to Axis::Vertical, horizontal
                // (dx) to Axis::Horizontal.
                if dy.abs() > f64::EPSILON {
                    let steps = dy.round() as i32;
                    if steps != 0 {
                        rd.notify_pointer_axis_discrete(session, Axis::Vertical, steps)
                            .await
                            .map_err(|e| wayland_input_error("scroll inject", e))?;
                    }
                }
                if dx.abs() > f64::EPSILON {
                    let steps = dx.round() as i32;
                    if steps != 0 {
                        rd.notify_pointer_axis_discrete(session, Axis::Horizontal, steps)
                            .await
                            .map_err(|e| wayland_input_error("scroll inject", e))?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn inject_text(&self, text: &str) -> Result<(), CallerError> {
        let guard = self.portal_session.lock().await;
        let ps = guard.as_ref().ok_or_else(|| {
            CallerError::Display("no active portal session for text injection".to_string())
        })?;

        let rd = &ps.remote_desktop;
        let session = &ps.session;

        for ch in text.chars() {
            let keysym = super::keymap::char_to_x11_keysym(ch).ok_or_else(|| {
                CallerError::Display(format!(
                    "unsupported Wayland text character: U+{:04X}",
                    ch as u32
                ))
            })?;
            rd.notify_keyboard_keysym(session, keysym, KeyState::Pressed)
                .await
                .map_err(|e| wayland_input_error("text inject", e))?;
            rd.notify_keyboard_keysym(session, keysym, KeyState::Released)
                .await
                .map_err(|e| wayland_input_error("text inject", e))?;
        }

        Ok(())
    }

    async fn paste_text(&self, text: &str) -> Result<(), CallerError> {
        // Paste gesture emulation, matching the Windows backend: (1) make
        // exactly `text` the session's current paste payload, (2) press
        // ctrl+v in the already-approved RemoteDesktop session. The payload
        // is only handed out when the focused app asks for it during the
        // paste — the portal transfer callback is not a monitor.
        let guard = self.portal_session.lock().await;
        let ps = guard.as_ref().ok_or_else(|| {
            CallerError::Display("no active portal session for paste".to_string())
        })?;
        let Some(clipboard) = &ps.clipboard else {
            return Err(CallerError::Display(
                "the portal backend does not provide the Clipboard interface — \
                 use a type action instead"
                    .to_string(),
            ));
        };

        let transfers = clipboard
            .receive_selection_transfer()
            .await
            .map_err(|e| CallerError::Display(format!("clipboard SelectionTransfer: {e}")))?;
        futures_util::pin_mut!(transfers);

        set_paste_payload(clipboard, &ps.session).await?;

        let rd = &ps.remote_desktop;
        let session = &ps.session;
        let ctrl = super::keymap::dom_code_to_evdev("ControlLeft")
            .ok_or_else(|| CallerError::Display("keymap missing ControlLeft".to_string()))?
            as i32;
        let v = super::keymap::dom_code_to_evdev("KeyV")
            .ok_or_else(|| CallerError::Display("keymap missing KeyV".to_string()))?
            as i32;
        rd.notify_keyboard_keycode(session, ctrl, KeyState::Pressed)
            .await
            .map_err(|e| wayland_input_error("paste chord (ctrl down)", e))?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        rd.notify_keyboard_keycode(session, v, KeyState::Pressed)
            .await
            .map_err(|e| wayland_input_error("paste chord (v down)", e))?;
        rd.notify_keyboard_keycode(session, v, KeyState::Released)
            .await
            .map_err(|e| wayland_input_error("paste chord (v up)", e))?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        rd.notify_keyboard_keycode(session, ctrl, KeyState::Released)
            .await
            .map_err(|e| wayland_input_error("paste chord (ctrl up)", e))?;

        let session_id = format!("{:?}", ps.session);
        let deadline = tokio::time::sleep(PASTE_TRANSFER_DEADLINE);
        tokio::pin!(deadline);
        let mut served = 0usize;

        while served < MAX_PASTE_TRANSFERS {
            tokio::select! {
                _ = &mut deadline => break,
                transfer = transfers.next() => {
                    let Some((transfer_session, mime_type, serial)) = transfer else {
                        break;
                    };
                    if format!("{transfer_session:?}") != session_id
                        || !is_text_plain_mime(&mime_type)
                    {
                        continue;
                    }
                    write_paste_transfer(clipboard, session, serial, text).await?;
                    served += 1;
                }
            }
        }

        if served == 0 {
            return Err(CallerError::Display(
                "the focused app never requested the paste payload".to_string(),
            ));
        }

        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        (
            self.shared_width.load(Ordering::SeqCst),
            self.shared_height.load(Ordering::SeqCst),
        )
    }

    fn kind(&self) -> &'static str {
        "wayland"
    }
}

/// Bounded serving window for one paste gesture: the focused app must ask
/// for the payload within this deadline, then serving stops regardless.
// W3c (the call-local transfer loop) consumes these; allow until it lands.
#[allow(dead_code)]
const PASTE_TRANSFER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
/// A single paste legitimately fires at most a couple of transfer requests
/// (some apps ask once per advertised mime type); serving stops after this
/// many even inside the deadline.
#[allow(dead_code)]
const MAX_PASTE_TRANSFERS: usize = 2;

/// Whether a `SelectionTransfer` mime type is one of the text/plain forms we
/// advertise (exact or parameterized, e.g. `text/plain;charset=utf-8`).
/// Non-text requests are ignored by the paste serving loop.
#[cfg_attr(not(test), allow(dead_code))]
fn is_text_plain_mime(mime: &str) -> bool {
    let m = mime.trim().to_ascii_lowercase();
    m == "text/plain" || m.starts_with("text/plain;")
}

/// Advertise this session as the owner of a text/plain paste payload. The
/// bytes themselves are written only when the focused app requests them during
/// the call-local paste window.
async fn set_paste_payload(
    clipboard: &Clipboard<'static>,
    session: &Session<'static, RemoteDesktop<'static>>,
) -> Result<(), CallerError> {
    clipboard
        .set_selection(session, &["text/plain;charset=utf-8", "text/plain"])
        .await
        .map_err(|e| CallerError::Display(format!("clipboard SetSelection: {e}")))
}

async fn write_paste_transfer(
    clipboard: &Clipboard<'static>,
    session: &Session<'static, RemoteDesktop<'static>>,
    serial: u32,
    text: &str,
) -> Result<(), CallerError> {
    let fd = clipboard
        .selection_write(session, serial)
        .await
        .map_err(|e| CallerError::Display(format!("clipboard SelectionWrite: {e}")))?;
    let fd: std::os::fd::OwnedFd = fd.into();
    let file = std::fs::File::from(fd);
    let mut file = tokio::fs::File::from_std(file);

    let write_result = async {
        file.write_all(text.as_bytes()).await?;
        file.flush().await?;
        file.shutdown().await
    }
    .await;
    drop(file);

    let success = write_result.is_ok();
    clipboard
        .selection_write_done(session, serial, success)
        .await
        .map_err(|e| CallerError::Display(format!("clipboard SelectionWriteDone: {e}")))?;

    write_result.map_err(|e| CallerError::Display(format!("clipboard transfer write: {e}")))
}

async fn verify_remote_interaction(
    remote_desktop: &RemoteDesktop<'static>,
    session: &Session<'static, RemoteDesktop<'static>>,
) -> Result<(), CallerError> {
    remote_desktop
        .notify_pointer_motion(session, 1.0, 0.0)
        .await
        .map_err(wayland_remote_interaction_error)?;
    remote_desktop
        .notify_pointer_motion(session, -1.0, 0.0)
        .await
        .map_err(wayland_remote_interaction_error)?;
    Ok(())
}

fn wayland_remote_interaction_error(error: impl std::fmt::Display) -> CallerError {
    let raw = error.to_string();
    CallerError::Display(format!(
        "Wayland portal remote interaction is not active after approval: {raw}. \
         Revoke and grant the user display again, then approve the GNOME portal \
         with Allow Remote Interaction enabled before clicking Share; approving \
         screen sharing alone allows screenshots but not Computer Use input."
    ))
}

fn wayland_input_error(action: &str, error: impl std::fmt::Display) -> CallerError {
    CallerError::Display(format!(
        "{}: {}. {}",
        action,
        error,
        wayland_input_recovery_hint(),
    ))
}

fn wayland_input_recovery_hint() -> &'static str {
    "Wayland portal input is not active. Revoke and grant the user display again, then approve the GNOME portal with Allow Remote Interaction enabled; screenshot-only approval is insufficient for Computer Use input."
}

/// Manually mmap an fd-backed buffer (DMA-BUF or MemFd), copy the pixel region,
/// and munmap. Returns `None` on any failure so the caller can skip the frame.
fn mmap_fd_and_read(
    fd: std::os::raw::c_int,
    map_offset: usize,
    max_size: usize,
    chunk_offset: usize,
    chunk_size: usize,
) -> Option<Vec<u8>> {
    // Page-align the map offset downward.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let aligned_offset = map_offset & !(page_size - 1);
    let offset_delta = map_offset - aligned_offset;
    let map_len = max_size + offset_delta;

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            aligned_offset as libc::off_t,
        )
    };

    if ptr == libc::MAP_FAILED || ptr.is_null() {
        return None;
    }

    let base = unsafe { (ptr as *const u8).add(offset_delta) };
    let effective_size = if chunk_size > 0 { chunk_size } else { max_size };
    let start = chunk_offset;
    let end = (start + effective_size).min(max_size);

    let result = if start < end {
        let slice = unsafe { std::slice::from_raw_parts(base.add(start), end - start) };
        Some(slice.to_vec())
    } else {
        None
    };

    unsafe {
        libc::munmap(ptr, map_len);
    }

    result
}

/// Run the PipeWire main loop on a dedicated OS thread.
///
/// This function blocks until the `shutdown` flag is set or the PipeWire
/// connection is lost. Frames are sent to `tx` via `try_send()` -- if the
/// channel is full the frame is dropped (backpressure).
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
fn run_pipewire_capture(
    pw_fd: std::os::fd::OwnedFd,
    node_id: u32,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    width: u32,
    height: u32,
    framerate: u32,
    shared_width: Arc<AtomicU32>,
    shared_height: Arc<AtomicU32>,
) {
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::sys as spa_sys;
    use pipewire::spa::utils::{Fraction, Rectangle, SpaTypes};

    pipewire::init();

    let mainloop = match pipewire::main_loop::MainLoop::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            eprintln!("[display/wayland] pipewire MainLoop::new failed: {e}");
            return;
        }
    };

    let context = match pipewire::context::Context::new(&mainloop) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/wayland] pipewire Context::new failed: {e}");
            return;
        }
    };

    let core = match context.connect_fd(pw_fd, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/wayland] pipewire connect_fd failed: {e}");
            return;
        }
    };

    let stream = match pipewire::stream::Stream::new(
        &core,
        "intendant-capture",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[display/wayland] pipewire Stream::new failed: {e}");
            return;
        }
    };

    // Stream listener: process frames from the PipeWire buffer.
    //
    // Supports two buffer types:
    // - SHM (MemPtr): PipeWire delivers a pointer to shared memory, auto-mapped
    //   by the MAP_BUFFERS flag.
    // - DMA-BUF: PipeWire delivers a GPU memory file descriptor. If MAP_BUFFERS
    //   auto-maps it the data pointer is set and we use it directly. Otherwise
    //   we manually mmap/munmap the fd for each frame.
    //
    // PipeWire auto-negotiates the buffer type. If the compositor doesn't
    // support DMA-BUF, it falls back to SHM transparently.
    let tx_clone = tx.clone();
    let sw = Arc::clone(&shared_width);
    let sh = Arc::clone(&shared_height);
    // Track the last known dimensions so we only log on actual changes.
    let mut last_w = width;
    let mut last_h = height;
    // Log the buffer type once on the first frame.
    let mut buffer_type_logged = false;
    // Detect portal session ending (user clicks the orange share-stop
    // indicator, compositor crash, etc.) by listening for stream state
    // transitions. The XDG portal pauses the producer-side PipeWire stream
    // when the user revokes — that surfaces here as `Streaming → Paused`,
    // *not* `Unconnected` or `Error` (those only happen on hard errors or
    // explicit teardown). So once we've been in Streaming at least once,
    // treat any non-Streaming state as a stop signal: setting the shared
    // shutdown flag, the mainloop's idle callback quits, the function
    // returns, `tx` drops, the capture channel closes, and
    // `DisplayCaptureLost` fires upstream — same path as a normal
    // teardown.
    //
    // We gate on `has_been_streaming` rather than reacting to every
    // non-Streaming state, because the normal startup sequence is
    // `Unconnected → Connecting → Paused → Streaming` and we don't want to
    // tear ourselves down before the first frame ever flows.
    let state_shutdown = Arc::clone(&shutdown);
    let mut has_been_streaming = false;
    let _listener = stream
        .add_local_listener()
        .state_changed(move |_stream_ref, _: &mut (), _old, new| {
            use pipewire::stream::StreamState;
            match new {
                StreamState::Streaming => {
                    has_been_streaming = true;
                }
                _ if has_been_streaming => {
                    eprintln!(
                        "[display/wayland] stream left Streaming ({new:?}); shutting down capture"
                    );
                    state_shutdown.store(true, Ordering::SeqCst);
                }
                _ => {}
            }
        })
        .param_changed(move |stream_ref, _: &mut (), param_id, _param| {
            // When the format is negotiated, tell PipeWire we accept DMA-BUF,
            // MemFd, and MemPtr buffers. PipeWire picks the best available.
            if param_id != ParamType::Format.as_raw() {
                return;
            }
            // dataType is a bitmask: bit N = accept spa_data_type N.
            //   MemPtr  = 1 → bit 1 = 0x02
            //   MemFd   = 2 → bit 2 = 0x04
            //   DmaBuf  = 3 → bit 3 = 0x08
            let data_type_mask: i32 = (1 << spa_sys::SPA_DATA_DmaBuf)
                | (1 << spa_sys::SPA_DATA_MemFd)
                | (1 << spa_sys::SPA_DATA_MemPtr);

            let buffers_pod_bytes = pipewire::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(vec![0u8; 1024]),
                &Value::Object(Object {
                    type_: SpaTypes::ObjectParamBuffers.as_raw(),
                    id: ParamType::Buffers.as_raw(),
                    properties: vec![Property {
                        key: spa_sys::SPA_PARAM_BUFFERS_dataType,
                        flags: PropertyFlags::empty(),
                        value: Value::Int(data_type_mask),
                    }],
                }),
            );
            if let Ok((cursor, _)) = buffers_pod_bytes {
                let bytes = cursor.into_inner();
                if let Some(pod) = pipewire::spa::pod::Pod::from_bytes(&bytes) {
                    let _ = stream_ref.update_params(&mut [pod]);
                }
            }
        })
        .process(move |stream_ref, _: &mut ()| {
            if let Some(mut buffer) = stream_ref.dequeue_buffer() {
                if let Some(buf) = buffer.datas_mut().first_mut() {
                    let buf_type = buf.type_();

                    // Log the buffer type once on the first successful frame.
                    if !buffer_type_logged {
                        if buf_type == pipewire::spa::buffer::DataType::DmaBuf {
                            eprintln!("[display/wayland] Using DMA-BUF capture (zero-copy)");
                        } else {
                            eprintln!("[display/wayland] Using SHM capture");
                        }
                        buffer_type_logged = true;
                    }

                    // Read chunk metadata before taking the mutable data borrow.
                    let stride = buf.chunk().stride() as u32;
                    let chunk_size = buf.chunk().size() as usize;
                    let chunk_offset = buf.chunk().offset() as usize;

                    // Try the auto-mapped data pointer first (works for both
                    // SHM and DMA-BUF when MAP_BUFFERS is set and the buffer
                    // is mappable).
                    let pixel_data: Option<Vec<u8>> = if let Some(data) = buf.data() {
                        // Apply chunk offset/size: the valid region may be a
                        // subset of the mapped buffer.
                        let effective = if chunk_size > 0 && chunk_offset + chunk_size <= data.len()
                        {
                            &data[chunk_offset..chunk_offset + chunk_size]
                        } else {
                            data
                        };
                        Some(effective.to_vec())
                    } else if buf_type == pipewire::spa::buffer::DataType::DmaBuf
                        || buf_type == pipewire::spa::buffer::DataType::MemFd
                    {
                        // Fd-backed buffer without auto-mapping. Manually mmap
                        // the fd, copy the pixels, then munmap.
                        let raw = buf.as_raw();
                        let fd = raw.fd as std::os::raw::c_int;
                        let maxsize = raw.maxsize as usize;
                        if fd >= 0 && maxsize > 0 {
                            mmap_fd_and_read(
                                fd,
                                raw.mapoffset as usize,
                                maxsize,
                                chunk_offset,
                                chunk_size,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some(pixels) = pixel_data {
                        // Derive actual frame dimensions from the pixel data.
                        let data_len = pixels.len();
                        let frame_w = if stride > 0 {
                            let current_w = sw.load(Ordering::SeqCst);
                            if current_w > 0 {
                                current_w
                            } else {
                                stride / 4
                            }
                        } else {
                            sw.load(Ordering::SeqCst)
                        };
                        let frame_h = if stride > 0 && data_len > 0 {
                            (data_len as u32) / stride
                        } else {
                            sh.load(Ordering::SeqCst)
                        };

                        // Update shared atomics if dimensions changed.
                        if frame_w > 0 && frame_h > 0 && (frame_w != last_w || frame_h != last_h) {
                            eprintln!(
                                "[display/wayland] frame resize detected: {}x{} -> {}x{}",
                                last_w, last_h, frame_w, frame_h,
                            );
                            sw.store(frame_w, Ordering::SeqCst);
                            sh.store(frame_h, Ordering::SeqCst);
                            last_w = frame_w;
                            last_h = frame_h;
                        }

                        let frame = Frame {
                            data: pixels,
                            format: FrameFormat::Bgra,
                            width: frame_w,
                            height: frame_h,
                            stride,
                            timestamp: std::time::Instant::now(),
                            dirty_rects: None,
                        };

                        // Backpressure: drop frame if channel is full.
                        let _ = tx_clone.try_send(frame);
                    }
                }
            }
        })
        .register()
        .expect("pipewire stream listener");

    // Build format parameters for the stream.
    let format = pipewire::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pipewire::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pipewire::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pipewire::spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        pipewire::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width, height },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: width.max(8192),
                height: height.max(8192),
            }
        ),
        pipewire::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: framerate,
                denom: 1,
            },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 60, denom: 1 }
        ),
    );
    eprintln!(
        "[display/wayland] requesting PipeWire format BGRx {}x{} @ {}fps",
        width, height, framerate
    );

    let format_pod_bytes = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(vec![0u8; 1024]),
        &Value::Object(format),
    )
    .expect("pipewire format pod serialization")
    .0
    .into_inner();

    let format_pod =
        pipewire::spa::pod::Pod::from_bytes(&format_pod_bytes).expect("pipewire pod from bytes");

    stream
        .connect(
            pipewire::spa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod],
        )
        .expect("pipewire stream connect");

    // Idle callback: check shutdown flag periodically.
    let shutdown_check = shutdown.clone();
    let mainloop_weak = mainloop.downgrade();
    let _idle = mainloop.loop_().add_idle(true, move || {
        if shutdown_check.load(Ordering::SeqCst) {
            if let Some(ml) = mainloop_weak.upgrade() {
                ml.quit();
            }
        }
    });

    // Run until shutdown or error.
    mainloop.run();
}

fn target_pipewire_framerate(fps: u32) -> u32 {
    fps.clamp(1, 60)
}

#[cfg(test)]
mod tests {
    use super::{
        is_text_plain_mime, target_pipewire_framerate, wayland_input_error,
        wayland_remote_interaction_error,
    };
    use crate::keymap::char_to_x11_keysym;

    #[test]
    fn paste_mime_matching() {
        assert!(is_text_plain_mime("text/plain"));
        assert!(is_text_plain_mime("text/plain;charset=utf-8"));
        assert!(is_text_plain_mime("TEXT/PLAIN"));
        assert!(is_text_plain_mime(" text/plain "));
        assert!(!is_text_plain_mime("text/html"));
        assert!(!is_text_plain_mime("text/plainx"));
        assert!(!is_text_plain_mime("image/png"));
        assert!(!is_text_plain_mime(""));
    }

    #[test]
    fn target_pipewire_framerate_clamps_to_supported_range() {
        assert_eq!(target_pipewire_framerate(0), 1);
        assert_eq!(target_pipewire_framerate(30), 30);
        assert_eq!(target_pipewire_framerate(120), 60);
    }

    #[test]
    fn text_keysyms_cover_command_text() {
        assert_eq!(char_to_x11_keysym('g'), Some(0x67));
        assert_eq!(char_to_x11_keysym('C'), Some(0x43));
        assert_eq!(char_to_x11_keysym('-'), Some(0x2d));
        assert_eq!(char_to_x11_keysym('/'), Some(0x2f));
        assert_eq!(char_to_x11_keysym(':'), Some(0x3a));
        assert_eq!(char_to_x11_keysym('\n'), Some(0xff0d));
    }

    #[test]
    fn text_keysyms_use_unicode_keysym_for_non_latin1() {
        assert_eq!(char_to_x11_keysym('€'), Some(0x010020ac));
    }

    #[test]
    fn inactive_input_error_tells_operator_to_regrant_with_remote_interaction() {
        let err = wayland_input_error(
            "key inject",
            "Portal request failed: org.freedesktop.zbus.Error: Session is no longer active",
        )
        .to_string();

        assert!(err.contains("key inject"));
        assert!(err.contains("Allow Remote Interaction"));
        assert!(err.contains("screenshot-only approval is insufficient"));
    }

    #[test]
    fn remote_interaction_preflight_error_explains_screenshot_only_approval() {
        let err = wayland_remote_interaction_error(
            "Portal request failed: org.freedesktop.zbus.Error: Session is no longer active",
        )
        .to_string();

        assert!(err.contains("remote interaction is not active"));
        assert!(err.contains("Allow Remote Interaction"));
        assert!(err.contains("screen sharing alone allows screenshots"));
    }
}
