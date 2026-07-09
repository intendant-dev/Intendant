//! X11 display backend using XShm for frame capture and xdotool for input
//! injection.
//!
//! The XShm capture loop runs on a dedicated `std::thread` (the X11 connection
//! is not `Send` across await points).  Communication with the tokio runtime is
//! via a bounded `mpsc` channel for frames and an `AtomicBool` for shutdown
//! signaling.
//!
//! If the XShm extension is unavailable, the backend falls back to `XGetImage`
//! (slower but always works).

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use async_trait::async_trait;
use intendant_core::error::CallerError;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use x11rb::connection::Connection;

/// Active capture state: holds the thread handle for cleanup.
struct CaptureState {
    thread: std::thread::JoinHandle<()>,
}

/// X11 screen capture and input injection backend.
///
/// Uses `x11rb` with the XShm extension for fast full-screen capture and
/// shells out to `xdotool` for keyboard/mouse/scroll input injection (same
/// approach as the existing `computer_use.rs` X11 backend).
pub struct X11Backend {
    capture: Mutex<Option<CaptureState>>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    display: String,
}

impl X11Backend {
    /// Create a new X11 backend.
    ///
    /// Connects to the X server (using `DISPLAY` env var), queries the root
    /// window dimensions, and caches them.  The connection is dropped after
    /// setup -- the capture thread creates its own connection.
    pub fn new() -> Result<Self, CallerError> {
        let display_str = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

        // Probe the display to get resolution.
        let (conn, screen_num) = x11rb::connect(Some(&display_str))
            .map_err(|e| CallerError::Display(format!("X11 connect: {e}")))?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        // VP8 requires even dimensions.
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;

        Ok(Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(width)),
            height: Arc::new(AtomicU32::new(height)),
            shutdown: Arc::new(AtomicBool::new(false)),
            display: display_str,
        })
    }

    /// Create a backend targeting a specific X11 display string (e.g. ":0", ":99").
    /// Virtual display sessions use this to connect to their own Xvfb server
    /// regardless of where the process-wide `DISPLAY` points.
    pub fn with_display(display_str: &str) -> Result<Self, CallerError> {
        let (conn, screen_num) = x11rb::connect(Some(display_str))
            .map_err(|e| CallerError::Display(format!("X11 connect to {display_str}: {e}")))?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;

        Ok(Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(width)),
            height: Arc::new(AtomicU32::new(height)),
            shutdown: Arc::new(AtomicBool::new(false)),
            display: display_str.to_string(),
        })
    }
}

/// Enumerate X11 displays using xrandr.
///
/// Parses `xrandr --query` output to find connected monitors.  The primary
/// monitor gets `id: 0`; additional monitors get sequential IDs from 1.
/// Falls back to the root window dimensions if xrandr is unavailable.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    let display_str = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

    // Try xrandr first for multi-monitor enumeration.
    if let Ok(output) = tokio::process::Command::new("xrandr")
        .arg("--query")
        .env("DISPLAY", &display_str)
        .output()
        .await
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let displays = parse_xrandr_output(&text);
            if !displays.is_empty() {
                return displays;
            }
        }
    }

    // Fallback: use x11rb to get the root window size (single display).
    if let Ok((conn, screen_num)) = x11rb::connect(Some(&display_str)) {
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let width = (screen.width_in_pixels as u32) & !1;
        let height = (screen.height_in_pixels as u32) & !1;
        return vec![super::DisplayInfo {
            id: 0,
            platform_id: screen_num as u64,
            name: format!("X11 Screen {} ({}x{})", screen_num, width, height),
            width,
            height,
            is_primary: true,
            kind: super::DisplayInfoKind::Display,
            application_name: None,
            window_title: None,
        }];
    }

    Vec::new()
}

/// Parse xrandr --query output into a list of `DisplayInfo`.
///
/// Looks for lines like:
///   HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
///   DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
fn parse_xrandr_output(text: &str) -> Vec<super::DisplayInfo> {
    let mut displays = Vec::new();
    let mut next_id: u32 = 1;

    for line in text.lines() {
        // Match " connected " lines that include a mode+offset pattern.
        if !line.contains(" connected ") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let output_name = parts[0];
        let is_primary = parts.iter().any(|p| *p == "primary");

        // Find the resolution+offset token: "WxH+X+Y".
        let mode_token = parts.iter().find(|p| {
            let s = **p;
            s.contains('x') && s.contains('+')
        });
        let (width, height) = if let Some(tok) = mode_token {
            parse_mode_token(tok)
        } else {
            continue; // Connected but no active mode — skip.
        };

        if width == 0 || height == 0 {
            continue;
        }

        let id = if is_primary {
            0
        } else {
            let id = next_id;
            next_id += 1;
            id
        };

        displays.push(super::DisplayInfo {
            id,
            platform_id: id as u64,
            name: format!("{} ({}x{})", output_name, width, height),
            width,
            height,
            is_primary,
            kind: super::DisplayInfoKind::Display,
            application_name: None,
            window_title: None,
        });
    }

    // Ensure primary is first.
    displays.sort_by_key(|d| if d.is_primary { 0 } else { 1 });
    displays
}

/// Parse "WxH+X+Y" into (width, height).
fn parse_mode_token(tok: &str) -> (u32, u32) {
    // "1920x1080+0+0" → split on 'x' → "1920", "1080+0+0" → split on '+' → "1080"
    let x_pos = match tok.find('x') {
        Some(p) => p,
        None => return (0, 0),
    };
    let w_str = &tok[..x_pos];
    let rest = &tok[x_pos + 1..];
    let h_str = rest.split('+').next().unwrap_or("0");
    let w = w_str.parse::<u32>().unwrap_or(0);
    let h = h_str.parse::<u32>().unwrap_or(0);
    (w, h)
}

#[async_trait]
impl DisplayBackend for X11Backend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Defensive: if a prior start_capture wasn't paired with a matching
        // stop_capture (double-grant, race between revoke-and-regrant, etc.)
        // the old thread would otherwise be silently leaked by the
        // `*self.capture.lock().await = Some(...)` overwrite below — its
        // JoinHandle gets dropped but the thread itself keeps running
        // because `shutdown` is a shared AtomicBool and nothing's flipped it
        // to true. Two parallel X11 capture threads then contend for the
        // SHM segment and the encoder sees interleaved frames. Paper this
        // over by running the normal teardown path first; it's idempotent
        // when no capture is running (early-returns inside the `if let`).
        self.stop_capture().await;

        self.shutdown.store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<Frame>(4);
        let shutdown_flag = Arc::clone(&self.shutdown);
        let display_str = self.display.clone();
        let width = self.width.load(Ordering::SeqCst);
        let height = self.height.load(Ordering::SeqCst);
        let shared_w = Arc::clone(&self.width);
        let shared_h = Arc::clone(&self.height);

        let thread = std::thread::spawn(move || {
            run_x11_capture(
                display_str,
                tx,
                shutdown_flag,
                fps,
                width,
                height,
                shared_w,
                shared_h,
            );
        });

        *self.capture.lock().await = Some(CaptureState { thread });
        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // Teardown contract: the capture thread owns the frame channel's only
        // sender, so the join below doubles as the bounded channel-close —
        // the receiver is guaranteed to see `None` once this returns. Taking
        // the state also makes double-stop / stop-without-start no-ops.
        if let Some(state) = self.capture.lock().await.take() {
            // `std::thread::join()` is blocking — parking it on the tokio
            // executor thread stalls every other async task scheduled
            // there, including the WebSocket outbound pump that's trying
            // to deliver UserDisplayRevoked to the dashboard. That
            // explained the 1-minute revoke lag and the flakiness (the
            // user sees "streaming continues live" because the broadcast
            // is queued behind a join that's itself waiting for the
            // capture thread to finish an in-flight XShmGetImage call).
            // Push the join onto the blocking pool so the executor thread
            // stays free.
            let _ = tokio::task::spawn_blocking(move || {
                let _ = state.thread.join();
            })
            .await;
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let width = self.width.load(Ordering::SeqCst) as f64;
        let height = self.height.load(Ordering::SeqCst) as f64;
        let display = &self.display;

        // In-process XTest injection over the shared per-display connection
        // (crate::x11_input) — no xdotool fork per browser input event.
        match event {
            InputEvent::KeyDown { ref code, .. } => {
                crate::x11_input::key_sequence(display, vec![(code.clone(), true)])
                    .await
                    .map_err(CallerError::Display)?;
            }
            InputEvent::KeyUp { ref code, .. } => {
                crate::x11_input::key_sequence(display, vec![(code.clone(), false)])
                    .await
                    .map_err(CallerError::Display)?;
            }
            InputEvent::MouseMove { x, y, .. } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                crate::x11_input::move_mouse(display, px, py)
                    .await
                    .map_err(CallerError::Display)?;
            }
            InputEvent::MouseDown { x, y, b } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                crate::x11_input::mouse_down(display, px, py, x11_button_from_browser(b))
                    .await
                    .map_err(CallerError::Display)?;
            }
            InputEvent::MouseUp { x, y, b } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                crate::x11_input::mouse_up(display, px, py, x11_button_from_browser(b))
                    .await
                    .map_err(CallerError::Display)?;
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                let px = (x * width) as i32;
                let py = (y * height) as i32;
                // Vertical: X11 wheel buttons 4=up, 5=down.
                if dy.abs() > f64::EPSILON {
                    let steps = dy.abs().round().max(1.0) as u32;
                    let button = if dy < 0.0 { 4 } else { 5 };
                    crate::x11_input::scroll(display, px, py, button, steps)
                        .await
                        .map_err(CallerError::Display)?;
                }
                // Horizontal: 6=left, 7=right.
                if dx.abs() > f64::EPSILON {
                    let steps = dx.abs().round().max(1.0) as u32;
                    let button = if dx < 0.0 { 6 } else { 7 };
                    crate::x11_input::scroll(display, px, py, button, steps)
                        .await
                        .map_err(CallerError::Display)?;
                }
            }
        }
        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::SeqCst),
            self.height.load(Ordering::SeqCst),
        )
    }

    fn kind(&self) -> &'static str {
        "x11"
    }
}

/// Browser mouse-button index (0=left, 1=middle, 2=right) to the X11
/// core-protocol button number (1=left, 2=middle, 3=right).
fn x11_button_from_browser(b: u8) -> u8 {
    match b {
        0 => 1,
        1 => 2,
        2 => 3,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// X11 capture thread
// ---------------------------------------------------------------------------

/// Run the X11 capture loop on a dedicated OS thread.
///
/// Connects to the X server, sets up XShm (or falls back to XGetImage),
/// and loops at the target framerate sending frames via `try_send()`.
fn run_x11_capture(
    display_str: String,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    fps: u32,
    width: u32,
    height: u32,
    shared_width: Arc<AtomicU32>,
    shared_height: Arc<AtomicU32>,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::shm;

    let frame_interval =
        std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });

    let (conn, screen_num) = match x11rb::connect(Some(&display_str)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/x11] X11 connect failed: {e}");
            return;
        }
    };

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let depth = screen.root_depth;

    // Try to use XShm for fast capture.
    use x11rb::connection::RequestConnection;
    let shm_available = conn
        .extension_information(shm::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();

    if shm_available {
        eprintln!(
            "[display/x11] XShm available, using shared memory capture {}x{}",
            width, height
        );
        run_shm_capture(
            &conn,
            root,
            depth,
            width,
            height,
            &tx,
            &shutdown,
            frame_interval,
            &shared_width,
            &shared_height,
        );
    } else {
        eprintln!(
            "[display/x11] XShm unavailable, falling back to XGetImage {}x{}",
            width, height
        );
        run_getimage_capture(
            &conn,
            root,
            depth,
            width,
            height,
            &tx,
            &shutdown,
            frame_interval,
            &shared_width,
            &shared_height,
        );
    }
}

/// Re-query root window geometry and return the (even-aligned) dimensions.
///
/// Returns `None` if the query fails (display disconnected, etc.).
fn query_root_geometry(conn: &impl x11rb::connection::Connection, root: u32) -> Option<(u32, u32)> {
    use x11rb::protocol::xproto::ConnectionExt;
    let geo = conn.get_geometry(root).ok()?.reply().ok()?;
    let w = (geo.width as u32) & !1;
    let h = (geo.height as u32) & !1;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// XShm-based capture loop.
fn run_shm_capture(
    conn: &impl x11rb::connection::Connection,
    root: u32,
    _depth: u8,
    width: u32,
    height: u32,
    tx: &mpsc::Sender<Frame>,
    shutdown: &Arc<AtomicBool>,
    frame_interval: std::time::Duration,
    shared_width: &Arc<AtomicU32>,
    shared_height: &Arc<AtomicU32>,
) {
    use x11rb::protocol::shm::ConnectionExt as ShmConnectionExt;
    use x11rb::protocol::xproto::ImageFormat;

    let mut width = width;
    let mut height = height;

    // Allocate shared memory segment.
    // 4 bytes per pixel (BGRA), full screen.
    let seg_size = (width as usize) * (height as usize) * 4;

    let shm_id = unsafe { libc::shmget(libc::IPC_PRIVATE, seg_size, libc::IPC_CREAT | 0o600) };
    if shm_id < 0 {
        eprintln!(
            "[display/x11] shmget failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    let shm_addr = unsafe { libc::shmat(shm_id, std::ptr::null(), 0) };
    if shm_addr == (-1isize) as *mut libc::c_void {
        eprintln!(
            "[display/x11] shmat failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };
        return;
    }

    // Mark for removal on last detach (cleanup even if we crash).
    unsafe { libc::shmctl(shm_id, libc::IPC_RMID, std::ptr::null_mut()) };

    // Attach to X server.
    let seg = conn.generate_id().unwrap();
    let attach_ok = conn
        .shm_attach(seg, shm_id as u32, false)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_some();
    if !attach_ok {
        eprintln!("[display/x11] ShmAttach failed, falling back to XGetImage");
        unsafe { libc::shmdt(shm_addr) };
        run_getimage_capture(
            conn,
            root,
            _depth,
            width,
            height,
            tx,
            shutdown,
            frame_interval,
            shared_width,
            shared_height,
        );
        return;
    }

    let mut frame_count: u64 = 0;
    // Track consecutive capture errors to detect hotplug / display loss.
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 30;
    // Re-query root window geometry every N frames to detect resolution changes.
    const GEOMETRY_CHECK_INTERVAL: u64 = 60;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        // Periodically check for root window resize (e.g. xrandr change).
        if frame_count > 0 && frame_count % GEOMETRY_CHECK_INTERVAL == 0 {
            if let Some((new_w, new_h)) = query_root_geometry(conn, root) {
                if new_w != width || new_h != height {
                    eprintln!(
                        "[display/x11] root window resize detected: {}x{} -> {}x{}, \
                         shm segment may be too small -- falling back to XGetImage",
                        width, height, new_w, new_h,
                    );
                    // The SHM segment was sized for the old dimensions.
                    // Detach and fall through to XGetImage which can handle
                    // any size per frame.
                    let _ = conn.shm_detach(seg);
                    let _ = conn.flush();
                    unsafe { libc::shmdt(shm_addr) };
                    width = new_w;
                    height = new_h;
                    shared_width.store(width, Ordering::SeqCst);
                    shared_height.store(height, Ordering::SeqCst);
                    run_getimage_capture(
                        conn,
                        root,
                        _depth,
                        width,
                        height,
                        tx,
                        shutdown,
                        frame_interval,
                        shared_width,
                        shared_height,
                    );
                    return;
                }
            }
        }

        let cookie = match conn.shm_get_image(
            root,
            0,
            0,
            width as u16,
            height as u16,
            0xFFFFFFFF, // plane mask: all planes
            ImageFormat::Z_PIXMAP.into(),
            seg,
            0, // offset into shm segment
        ) {
            Ok(c) => c,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] ShmGetImage request failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] ShmGetImage request failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        match cookie.reply() {
            Ok(reply) => {
                consecutive_errors = 0;
                // X11 ZPixmap at depth 24/32 is BGRA (or BGRx).
                // For ShmGetImage the data is written into the shm segment
                // tightly packed at width*4 for depth 24/32.
                let stride = width * 4;
                let data_len = stride as usize * height as usize;

                let data = unsafe { std::slice::from_raw_parts(shm_addr as *const u8, data_len) };

                let frame = Frame {
                    data: data.to_vec(),
                    format: FrameFormat::Bgra,
                    width,
                    height,
                    stride,
                    timestamp: std::time::Instant::now(),
                    dirty_rects: None,
                };

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/x11] shm frame #{} {}x{} stride={} size={}B depth={}",
                        frame_count, width, height, stride, data_len, reply.depth
                    );
                }

                let _ = tx.try_send(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] ShmGetImage reply failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] ShmGetImage reply failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    // Cleanup: detach from X server and shared memory.
    let _ = conn.shm_detach(seg);
    let _ = conn.flush();
    unsafe { libc::shmdt(shm_addr) };
}

/// Fallback XGetImage-based capture loop (no shared memory).
fn run_getimage_capture(
    conn: &impl x11rb::connection::Connection,
    root: u32,
    _depth: u8,
    width: u32,
    height: u32,
    tx: &mpsc::Sender<Frame>,
    shutdown: &Arc<AtomicBool>,
    frame_interval: std::time::Duration,
    shared_width: &Arc<AtomicU32>,
    shared_height: &Arc<AtomicU32>,
) {
    use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

    let mut width = width;
    let mut height = height;
    let mut frame_count: u64 = 0;
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 30;
    // Re-query root window geometry every N frames to detect resolution changes.
    const GEOMETRY_CHECK_INTERVAL: u64 = 60;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let start = std::time::Instant::now();

        // Periodically check for root window resize.
        if frame_count > 0 && frame_count % GEOMETRY_CHECK_INTERVAL == 0 {
            if let Some((new_w, new_h)) = query_root_geometry(conn, root) {
                if new_w != width || new_h != height {
                    eprintln!(
                        "[display/x11] root window resize detected: {}x{} -> {}x{}",
                        width, height, new_w, new_h,
                    );
                    width = new_w;
                    height = new_h;
                    shared_width.store(width, Ordering::SeqCst);
                    shared_height.store(height, Ordering::SeqCst);
                }
            }
        }

        let cookie = match conn.get_image(
            ImageFormat::Z_PIXMAP,
            root,
            0,
            0,
            width as u16,
            height as u16,
            0xFFFFFFFF, // plane mask
        ) {
            Ok(c) => c,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] GetImage request failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] GetImage request failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        match cookie.reply() {
            Ok(reply) => {
                consecutive_errors = 0;
                let data = reply.data;
                // For ZPixmap, bytes_per_line can include padding.
                // x11rb's GetImageReply doesn't expose bytes_per_line directly,
                // but the data is tightly packed for the returned visual format.
                let stride = width * 4;

                let frame = Frame {
                    data,
                    format: FrameFormat::Bgra,
                    width,
                    height,
                    stride,
                    timestamp: std::time::Instant::now(),
                    dirty_rects: None,
                };

                frame_count += 1;
                if frame_count == 1 || frame_count % 300 == 0 {
                    eprintln!(
                        "[display/x11] getimage frame #{} {}x{} stride={} size={}B",
                        frame_count,
                        width,
                        height,
                        stride,
                        frame.data.len()
                    );
                }

                let _ = tx.try_send(frame);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("[display/x11] GetImage reply failed {consecutive_errors} times, display likely disconnected: {e}");
                    break;
                }
                eprintln!("[display/x11] GetImage failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_key_codes_map_to_x11_keycodes() {
        use crate::keymap::dom_code_to_x11_keycode;
        // Spot-check well-known evdev+8 keycodes across the key classes the
        // browser input path sends; the full table (and its per-row tests)
        // lives in display/keymap.rs.
        assert_eq!(dom_code_to_x11_keycode("KeyA"), Some(38));
        assert_eq!(dom_code_to_x11_keycode("Digit1"), Some(10));
        assert_eq!(dom_code_to_x11_keycode("F1"), Some(67));
        assert_eq!(dom_code_to_x11_keycode("ShiftLeft"), Some(50));
        assert_eq!(dom_code_to_x11_keycode("ControlRight"), Some(105));
        assert_eq!(dom_code_to_x11_keycode("Enter"), Some(36));
        assert_eq!(dom_code_to_x11_keycode("Space"), Some(65));
        assert_eq!(dom_code_to_x11_keycode("ArrowUp"), Some(111));
        assert_eq!(dom_code_to_x11_keycode("PageDown"), Some(117));
        assert_eq!(dom_code_to_x11_keycode("NumpadEnter"), Some(104));
        assert_eq!(dom_code_to_x11_keycode("NumpadDecimal"), Some(91));
        assert_eq!(dom_code_to_x11_keycode("BogusKey"), None);
        assert_eq!(dom_code_to_x11_keycode(""), None);
    }

    #[test]
    fn browser_buttons_map_to_x11_buttons() {
        assert_eq!(x11_button_from_browser(0), 1);
        assert_eq!(x11_button_from_browser(1), 2);
        assert_eq!(x11_button_from_browser(2), 3);
        assert_eq!(x11_button_from_browser(9), 1);
    }

    #[test]
    fn parse_xrandr_single_monitor() {
        let output = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00    59.94
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, 0);
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].height, 1080);
        assert!(displays[0].name.contains("HDMI-1"));
    }

    #[test]
    fn parse_xrandr_multi_monitor() {
        let output = "\
Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00    59.94
DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     59.95*+  74.97
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 2);
        // Primary first
        assert_eq!(displays[0].id, 0);
        assert!(displays[0].is_primary);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].height, 1080);
        // Secondary
        assert_eq!(displays[1].id, 1);
        assert!(!displays[1].is_primary);
        assert_eq!(displays[1].width, 2560);
        assert_eq!(displays[1].height, 1440);
        assert!(displays[1].name.contains("DP-1"));
    }

    #[test]
    fn parse_xrandr_disconnected_ignored() {
        let output = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
DP-1 disconnected (normal left inverted right x axis y axis)
";
        let displays = parse_xrandr_output(output);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, 0);
    }

    #[test]
    fn parse_mode_token_basic() {
        assert_eq!(parse_mode_token("1920x1080+0+0"), (1920, 1080));
        assert_eq!(parse_mode_token("2560x1440+1920+0"), (2560, 1440));
    }

    #[test]
    fn parse_mode_token_invalid() {
        assert_eq!(parse_mode_token("primary"), (0, 0));
        assert_eq!(parse_mode_token(""), (0, 0));
    }

    /// Real-X-server teardown-contract stress: fast start/stop cycles with
    /// per-cycle bounded channel-close assertions, plus a post-stop linger
    /// (see `crate::capture_stress`). Ignored by default — needs a live X
    /// display; skips itself cleanly on a Wayland-only or headless box. Run
    /// on operator hardware:
    ///
    /// ```text
    /// cargo test -p intendant-display --lib -- --ignored real_capture_stress
    /// ```
    ///
    /// Tunables: `INTENDANT_DISPLAY_STRESS_CYCLES` (default 10),
    /// `INTENDANT_DISPLAY_STRESS_LINGER_SECS` (default 60).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "real X11 capture: needs a live DISPLAY; run via -- --ignored real_capture_stress on operator hardware"]
    async fn x11_real_capture_stress_cycles() {
        let backend = match X11Backend::new() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[capture-stress] skipping: no X11 display available: {e}");
                return;
            }
        };
        crate::capture_stress::run_real_backend_stress(&backend).await;
    }
}
