//! One exact SCK frame. The broker drives this future to completion even when
//! its frontend is gone; it never drops start/stop futures on a deadline.

use super::{Failure, Monitor, Receipt, Screenshot};
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::time::Duration;
use tokio::sync::oneshot;

#[cfg(target_os = "macos")]
pub(super) async fn capture(
    monitor: &Monitor,
    path: Option<&Path>,
    reply: &mut oneshot::Sender<Result<Receipt, String>>,
) -> Result<Screenshot, Failure> {
    let backend = crate::display::macos::MacOSBackend::read_only_display(monitor.native_id)
        .map_err(|e| Failure::Request(e.to_string()))?;
    // The budget includes native start. Synchronous SCK calls cannot be
    // interrupted safely. If one stalls, the frontend deadline cancels its
    // receipt; this sole worker retains all ownership and admits no new work.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let frame = capture_frame(&backend, deadline, reply).await?;
    if reply.is_closed() {
        return Err(Failure::Request("capture cancelled".into()));
    }
    encode_and_store(frame, monitor, path)
}

#[cfg(any(target_os = "macos", test))]
#[async_trait::async_trait]
trait Capture: Sync {
    type Frame: Send;
    async fn start(&self) -> Result<tokio::sync::mpsc::Receiver<Self::Frame>, String>;
    async fn stop(&self) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl Capture for crate::display::macos::MacOSBackend {
    type Frame = crate::display::Frame;
    async fn start(&self) -> Result<tokio::sync::mpsc::Receiver<Self::Frame>, String> {
        use crate::display::DisplayBackend;
        self.start_capture(1).await.map_err(|e| e.to_string())
    }
    async fn stop(&self) -> Result<(), String> {
        self.stop_capture_checked().await.map_err(|e| e.to_string())
    }
}

#[cfg(any(target_os = "macos", test))]
async fn capture_frame<C: Capture>(
    backend: &C,
    deadline: tokio::time::Instant,
    reply: &mut oneshot::Sender<Result<Receipt, String>>,
) -> Result<C::Frame, Failure> {
    let started = backend.start().await;
    let frame = first_frame(started, deadline, reply).await;
    // Unconditional, including start error, closed channel, deadline and
    // request cancellation. A failed stop retires the broker, never respawns.
    backend.stop().await.map_err(Failure::Retire)?;
    frame.map_err(Failure::Request)
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn capture(
    _: &Monitor,
    _: Option<&Path>,
    _: &mut oneshot::Sender<Result<Receipt, String>>,
) -> Result<Screenshot, Failure> {
    Err(Failure::Request(
        "macOS owned monitor capture requires macOS".into(),
    ))
}

#[cfg(any(target_os = "macos", test))]
async fn first_frame<T>(
    started: Result<tokio::sync::mpsc::Receiver<T>, String>,
    deadline: tokio::time::Instant,
    reply: &mut oneshot::Sender<Result<Receipt, String>>,
) -> Result<T, String> {
    let mut frames = started?;
    if reply.is_closed() {
        return Err("capture cancelled".into());
    }
    if tokio::time::Instant::now() >= deadline {
        return Err("no exact monitor frame within 5 seconds".into());
    }
    // Biased cancellation/deadline wins even if a late frame is also ready.
    let result = tokio::select! {
        biased;
        _ = reply.closed() => Err("capture cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Err("no exact monitor frame within 5 seconds".into()),
        frame = frames.recv() => frame.ok_or_else(|| "exact monitor capture closed before its first frame".into()),
    };
    // A timer may not be marked ready until the runtime's next timer tick.
    // Check the actual clock too, including after synchronous native startup.
    if tokio::time::Instant::now() >= deadline {
        return Err("no exact monitor frame within 5 seconds".into());
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn encode_and_store(
    frame: crate::display::Frame,
    monitor: &Monitor,
    path: Option<&Path>,
) -> Result<Screenshot, Failure> {
    super::validate_dimensions(frame.width, frame.height).map_err(Failure::Request)?;
    if (frame.width, frame.height) != (monitor.width, monitor.height) {
        return Err(Failure::Request(
            "owned monitor frame geometry does not match its exact generation".into(),
        ));
    }
    let image =
        crate::display::frame_to_rgba_image(&frame).map_err(|e| Failure::Request(e.to_string()))?;
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| Failure::Request(e.to_string()))?;
    let png = png.into_inner();
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Failure::Request(e.to_string()))?;
        }
        // tempfile creates owner-private files (0600 on Unix), independently of
        // umask. Publish only complete PNGs, without overwriting existing paths.
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Failure::Request(e.to_string()))?;
        file.write_all(&png)
            .map_err(|e| Failure::Request(e.to_string()))?;
        file.persist_noclobber(path)
            .map_err(|e| Failure::Request(e.to_string()))?;
    }
    Ok(Screenshot {
        path: path.map(Path::to_path_buf),
        png,
        width: frame.width,
        height: frame.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_geometry_private_png_and_no_clobber() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owned.png");
        let monitor = Monitor {
            display_id: super::super::DISPLAY_ID_MIN,
            helper_handle: 1,
            selector: "fixture".into(),
            native_id: 42,
            width: 64,
            height: 64,
        };
        let frame = |width, height| crate::display::Frame {
            data: vec![255; (width * height * 4) as usize],
            format: crate::display::FrameFormat::Rgba,
            width,
            height,
            stride: width * 4,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        };
        assert!(encode_and_store(frame(66, 64), &monitor, Some(&path)).is_err());
        assert!(!path.exists());
        let preview = encode_and_store(frame(64, 64), &monitor, None).unwrap();
        assert!(preview.path.is_none());
        assert!(!preview.png.is_empty());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let screenshot = encode_and_store(frame(64, 64), &monitor, Some(&path)).unwrap();
        let png = image::load_from_memory(&screenshot.png).unwrap();
        assert_eq!((png.width(), png.height()), (64, 64));
        assert_eq!(std::fs::read(&path).unwrap(), screenshot.png);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(encode_and_store(frame(64, 64), &monitor, Some(&path)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), screenshot.png);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[tokio::test]
    async fn frame_wait_cancellation_deadline_and_disconnect() {
        let (mut reply, receipt) = oneshot::channel();
        let (frames, rx) = tokio::sync::mpsc::channel(1);
        frames.try_send(42).unwrap();
        drop(receipt);
        assert!(first_frame(
            Ok(rx),
            tokio::time::Instant::now() + Duration::from_secs(1),
            &mut reply
        )
        .await
        .unwrap_err()
        .contains("cancelled"));
        let (mut reply, _receipt) = oneshot::channel();
        let (frames, rx) = tokio::sync::mpsc::channel(1);
        frames.try_send(42).unwrap();
        assert!(first_frame(Ok(rx), tokio::time::Instant::now(), &mut reply)
            .await
            .is_err());
        let (frames, rx) = tokio::sync::mpsc::channel::<u32>(1);
        drop(frames);
        assert!(first_frame(
            Ok(rx),
            tokio::time::Instant::now() + Duration::from_secs(1),
            &mut reply
        )
        .await
        .unwrap_err()
        .contains("closed"));
    }

    struct Fake {
        mode: u8,
        stops: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Capture for Fake {
        type Frame = u32;
        async fn start(&self) -> Result<tokio::sync::mpsc::Receiver<u32>, String> {
            if self.mode == 0 {
                return Err("failed start".into());
            }
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            if self.mode != 1 {
                tx.try_send(42).unwrap();
            }
            Ok(rx)
        }
        async fn stop(&self) -> Result<(), String> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.mode == 5 {
                Err("uncertain stop".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn every_exit_stops_and_uncertainty_retires() {
        for mode in 0..=5 {
            let fake = Fake {
                mode,
                stops: 0.into(),
            };
            let (mut reply, receipt) = oneshot::channel();
            let mut receipt = Some(receipt);
            if mode == 2 {
                receipt.take();
            }
            let deadline = tokio::time::Instant::now()
                + if mode == 3 {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                };
            let result = capture_frame(&fake, deadline, &mut reply).await;
            assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
            if mode == 4 {
                assert!(matches!(result, Ok(42)));
            } else if mode == 5 {
                assert!(matches!(result, Err(Failure::Retire(_))));
            } else {
                assert!(matches!(result, Err(Failure::Request(_))));
            }
        }
    }
}
