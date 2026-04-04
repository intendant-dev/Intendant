//! Wayland display backend using XDG Desktop Portal (ashpd) for screen capture
//! and input injection, and PipeWire for frame acquisition.
//!
//! The PipeWire main loop runs on a dedicated `std::thread` (it is not `Send`).
//! Communication with the tokio runtime is via a bounded `mpsc` channel for
//! frames and an `AtomicBool` for shutdown signaling.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};

/// Portal session handle + PipeWire capture thread.
struct PortalSession {
    /// The PipeWire node ID (used for pointer_motion_absolute stream param).
    node_id: u32,
    pw_thread: Option<std::thread::JoinHandle<()>>,
}

/// Wayland screen capture and input injection backend.
///
/// Uses the XDG Desktop Portal `RemoteDesktop` + `ScreenCast` interfaces for a
/// combined session that provides both keyboard/pointer injection and PipeWire
/// video frames.
pub struct WaylandBackend {
    portal_session: Mutex<Option<PortalSession>>,
    resolution: RwLock<(u32, u32)>,
    shutdown: Arc<AtomicBool>,
}

impl WaylandBackend {
    /// Create a new backend. Resolution is populated once capture starts.
    pub fn new() -> Self {
        Self {
            portal_session: Mutex::new(None),
            resolution: RwLock::new((0, 0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl DisplayBackend for WaylandBackend {
    async fn start_capture(
        &self,
        _fps: u32,
    ) -> Result<mpsc::Receiver<Frame>, CallerError> {
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

        let started = remote_desktop
            .start(&session, None)
            .await
            .map_err(|e| CallerError::Display(format!("start request: {e}")))?
            .response()
            .map_err(|e| CallerError::Display(format!("start response: {e}")))?;

        // Extract PipeWire node ID from the screencast streams.
        let streams = started
            .streams()
            .ok_or_else(|| {
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

        *self.resolution.write().await = (width, height);

        // Get PipeWire fd via the screencast portal.
        let pw_fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|e| CallerError::Display(format!("pipewire fd: {e}")))?;

        // Drop the session handle -- the portal keeps the session alive as long
        // as the PipeWire connection is active.
        drop(session);

        // --- Bounded frame channel: PipeWire thread -> tokio ---
        let (tx, rx) = mpsc::channel::<Frame>(4);

        // --- Spawn dedicated PipeWire thread ---
        let shutdown_flag = Arc::clone(&self.shutdown);
        let pw_thread = std::thread::spawn(move || {
            run_pipewire_capture(pw_fd, node_id, tx, shutdown_flag, width, height);
        });

        *self.portal_session.lock().await = Some(PortalSession {
            node_id,
            pw_thread: Some(pw_thread),
        });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(mut ps) = self.portal_session.lock().await.take() {
            if let Some(handle) = ps.pw_thread.take() {
                let _ = handle.join();
            }
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let rd = RemoteDesktop::new()
            .await
            .map_err(|e| CallerError::Display(format!("RemoteDesktop proxy: {e}")))?;

        // We need the session for injection.  Since the portal session lives
        // on the D-Bus side as long as PipeWire is connected, we re-create
        // a session proxy for input injection.  However, the portal requires
        // injecting on the *original* session.
        //
        // For a full implementation the session would be stored in shared state
        // accessible from the inject_input path.  For phase 1, input injection
        // is a stub that logs the event.
        let (width, height) = *self.resolution.read().await;
        let _guard = self.portal_session.lock().await;
        let _ps = _guard.as_ref().ok_or_else(|| {
            CallerError::Display("no active portal session for input injection".to_string())
        })?;

        // Input injection via the portal requires the Session handle which
        // has a non-static lifetime tied to the RemoteDesktop proxy.  Storing
        // this properly requires restructuring with a long-lived proxy.
        // Phase 1 logs the intent; full wiring is done in phase 2.
        eprintln!(
            "[display/wayland] input event (w={width}, h={height}): {event:?}",
        );

        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        self.resolution
            .try_read()
            .map(|r| *r)
            .unwrap_or((0, 0))
    }

    fn kind(&self) -> &'static str {
        "wayland"
    }
}

/// Run the PipeWire main loop on a dedicated OS thread.
///
/// This function blocks until the `shutdown` flag is set or the PipeWire
/// connection is lost. Frames are sent to `tx` via `try_send()` -- if the
/// channel is full the frame is dropped (backpressure).
fn run_pipewire_capture(
    pw_fd: std::os::fd::OwnedFd,
    node_id: u32,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    width: u32,
    height: u32,
) {
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Id, SpaTypes};

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
    let tx_clone = tx.clone();
    let _listener = stream
        .add_local_listener()
        .process(move |stream_ref, _: &mut ()| {
            if let Some(mut buffer) = stream_ref.dequeue_buffer() {
                if let Some(buf) = buffer.datas_mut().first_mut() {
                    // Read chunk metadata before taking the mutable data borrow.
                    let stride = buf.chunk().stride() as u32;
                    let chunk_size = buf.chunk().size();

                    if let Some(data) = buf.data() {
                        let frame_w = if stride > 0 { stride / 4 } else { width };
                        let frame_h = if stride > 0 {
                            chunk_size / stride
                        } else {
                            height
                        };

                        let frame = Frame {
                            data: data.to_vec(),
                            format: FrameFormat::Bgra,
                            width: frame_w,
                            height: frame_h,
                            stride,
                            timestamp: std::time::Instant::now(),
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
    let format_pod_bytes = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(vec![0u8; 1024]),
        &Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property {
                    key: FormatProperties::MediaType.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(MediaType::Video.as_raw())),
                },
                Property {
                    key: FormatProperties::MediaSubtype.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
                },
                Property {
                    key: FormatProperties::VideoFormat.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(VideoFormat::BGRx.as_raw())),
                },
            ],
        }),
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
