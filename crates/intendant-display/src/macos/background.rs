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
