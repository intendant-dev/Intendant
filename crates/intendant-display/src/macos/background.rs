//! One-shot background capture, reusing the ScreenCaptureKit backend.
use super::*;

/// Return a fresh window-only PNG in logical-point dimensions. Owned worker
/// always tears capture down, even if its requesting HTTP call disappears.
/// No physical-display fallback; this backend is never used for input.
pub async fn capture_window_png(
    window_id: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CallerError> {
    if window_id == 0 || width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(CallerError::Display(
            "invalid background capture geometry".into(),
        ));
    }
    tokio::spawn(async move {
        let backend = MacOSBackend::with_background_window_id(window_id);
        let mut frames = backend.start_capture(15).await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(3), frames.recv()).await;
        backend.stop_capture().await;
        let frame = received
            .map_err(|_| CallerError::Display("window capture timed out".into()))?
            .ok_or_else(|| {
                CallerError::Display("window capture closed before first frame".into())
            })?;
        let rgba = crate::frame_to_rgba_image(&frame)?;
        let logical =
            image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Lanczos3);
        let mut png = std::io::Cursor::new(Vec::new());
        logical
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| CallerError::Display(format!("window PNG encode: {e}")))?;
        Ok(png.into_inner())
    })
    .await
    .map_err(|e| CallerError::Display(format!("window capture worker: {e}")))?
}

/// The shadow-free coordinate contract needs the macOS 14 SCK property.
pub(super) fn check_capture_os() -> Result<(), CallerError> {
    static CHECK: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    CHECK
        .get_or_init(|| {
            let out = std::process::Command::new("/usr/bin/sw_vers")
                .arg("-productVersion")
                .output()
                .map_err(|e| e.to_string())?;
            let major = String::from_utf8_lossy(&out.stdout)
                .trim()
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok());
            if out.status.success() && major.is_some_and(|v| v >= 14) {
                Ok(())
            } else {
                Err("background window capture requires macOS 14 or newer".into())
            }
        })
        .clone()
        .map_err(CallerError::Display)
}
