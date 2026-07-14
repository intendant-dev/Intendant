//! Clipboard monitor for bidirectional clipboard sync between remote display
//! and browser.
//!
//! Polls the system clipboard every 500ms and emits changes.  Supports both
//! text and image content.  Polling is gated on viewer presence: while the
//! display session has zero connected peers the tick does no clipboard
//! access at all (see [`ClipboardMonitor::start_watching`]), because on
//! macOS/Linux every poll otherwise shells out to `pbpaste`/`osascript`/
//! `xclip`/`wl-paste` — a per-second subprocess tax for a sync with no
//! recipient.
//!
//! Platform support:
//! - **macOS**: `pbpaste` / `pbcopy`, `osascript` for image clipboard
//! - **Linux (Wayland)**: `wl-paste --no-newline` / `wl-copy` (from `wl-clipboard`)
//! - **Linux (X11)**: `xclip -o -selection clipboard` / `xclip -i -selection clipboard`
//! - **Windows**: the `arboard` crate (Win32 clipboard API under the hood)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Maximum image size in bytes (5 MB).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Content types that can be on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image { mime: String, data: Vec<u8> },
}

/// Monitors the system clipboard for changes and provides methods to read/write
/// clipboard text and images.
pub struct ClipboardMonitor {
    last_text: Arc<Mutex<String>>,
    last_image_hash: Arc<Mutex<u64>>,
    shutdown: Arc<AtomicBool>,
}

impl Default for ClipboardMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardMonitor {
    pub fn new() -> Self {
        Self {
            last_text: Arc::new(Mutex::new(String::new())),
            last_image_hash: Arc::new(Mutex::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start watching the clipboard for changes.
    ///
    /// Returns a receiver that emits `ClipboardContent` whenever it changes.
    /// The polling loop runs every 500ms until `stop()` is called.
    ///
    /// `peers_connected` gates the actual clipboard reads (F3, 2026-07-13
    /// display review): while it returns `false` — the display session has
    /// zero connected viewers — each tick is a no-op, so no clipboard
    /// subprocess (`pbpaste`/`osascript`/`xclip`/`wl-paste`) or native
    /// clipboard open is spawned for a sync nobody receives. Polling
    /// resumes on the first tick after a peer connects. Content that
    /// changed **during** the pause is deliberately absorbed without
    /// emitting (a silent re-baseline): pre-gate behavior broadcast such
    /// changes to an empty peer set — i.e. delivered them to nobody, ever
    /// — and the gate preserves exactly that.
    ///
    /// Production passes a closure over the session's `peer_count` gauge —
    /// the same peer-presence signal the encoder pool's presence policy
    /// keys on. Tests inject an arbitrary closure.
    ///
    /// Known follow-up (out of scope here): replace 500ms subprocess
    /// polling on macOS with an `NSPasteboard.changeCount` probe so even
    /// the with-viewers steady state stops shelling out twice a second.
    pub fn start_watching(
        &self,
        peers_connected: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> mpsc::Receiver<ClipboardContent> {
        let (tx, rx) = mpsc::channel::<ClipboardContent>(4);
        let last_text = Arc::clone(&self.last_text);
        let last_image_hash = Arc::clone(&self.last_image_hash);
        let shutdown = Arc::clone(&self.shutdown);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            // Set while paused; the first active tick after a pause
            // re-baselines silently instead of emitting.
            let mut rebaseline = false;
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // F3 gate: zero viewers → skip all clipboard access.
                if !peers_connected() {
                    rebaseline = true;
                    continue;
                }
                if rebaseline {
                    rebaseline = false;
                    // Absorb whatever changed while paused (mirror the
                    // image-over-text priority of the live poll below)
                    // so it is not replayed to the peer that just
                    // connected — matching pre-gate semantics, where a
                    // zero-peer change was polled, "sent" to an empty
                    // peer map, and never delivered later.
                    if let Some((_mime, data)) = read_clipboard_image().await {
                        *last_image_hash.lock().await = simple_hash(&data);
                        *last_text.lock().await = String::new();
                    } else if let Some(text) = read_clipboard_text().await {
                        *last_text.lock().await = text;
                        *last_image_hash.lock().await = 0;
                    }
                    continue;
                }

                // Check for image content first (higher priority).
                if let Some((mime, data)) = read_clipboard_image().await {
                    let hash = simple_hash(&data);
                    let mut last_hash = last_image_hash.lock().await;
                    if hash != *last_hash {
                        *last_hash = hash;
                        // Clear last text so switching back to text is detected.
                        *last_text.lock().await = String::new();
                        if data.len() > MAX_IMAGE_BYTES {
                            eprintln!(
                                "[display/clipboard] skipping image: {} bytes exceeds 5 MB limit",
                                data.len()
                            );
                            continue;
                        }
                        let content = ClipboardContent::Image { mime, data };
                        if tx.send(content).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }

                // Fall back to text.
                let current = match read_clipboard_text().await {
                    Some(text) => text,
                    None => continue,
                };
                let mut last = last_text.lock().await;
                if current != *last {
                    *last = current.clone();
                    // Clear image hash so switching back to image is detected.
                    *last_image_hash.lock().await = 0;
                    if tx.send(ClipboardContent::Text(current)).await.is_err() {
                        break; // receiver dropped
                    }
                }
            }
        });

        rx
    }

    /// Inject text into the system clipboard.
    ///
    /// Also updates the internal `last_text` so the next poll does not
    /// re-emit this text as a "change".
    pub async fn set_text(&self, text: &str) -> Result<(), String> {
        write_clipboard_text(text).await?;
        *self.last_text.lock().await = text.to_string();
        Ok(())
    }

    /// Inject image data into the system clipboard.
    ///
    /// Incoming images may be JPEG, WebP, or other browser formats.  The OS
    /// clipboard backends always write PNG, so we normalise to PNG first to
    /// avoid a MIME/content mismatch.
    ///
    /// Also updates the internal `last_image_hash` so the next poll does not
    /// re-emit this image as a "change".
    pub async fn set_image(&self, mime: &str, data: &[u8]) -> Result<(), String> {
        if data.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "image too large: {} bytes exceeds 5 MB limit",
                data.len()
            ));
        }

        // Convert to PNG if the source is not already PNG.
        let png_data = if mime == "image/png" {
            data.to_vec()
        } else {
            let img = image::load_from_memory(data)
                .map_err(|e| format!("image decode failed ({mime}): {e}"))?;
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png)
                .map_err(|e| format!("PNG encode failed: {e}"))?;
            buf.into_inner()
        };

        write_clipboard_image("image/png", &png_data).await?;
        *self.last_image_hash.lock().await = simple_hash(&png_data);
        Ok(())
    }

    /// Stop the polling loop.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for ClipboardMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Simple non-cryptographic hash for change detection.
fn simple_hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Platform: macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn read_clipboard_text() -> Option<String> {
    let output = tokio::process::Command::new("pbpaste")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
async fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    // Check clipboard info for image types.
    let info_output = tokio::process::Command::new("osascript")
        .args(["-e", "the clipboard info"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !info_output.status.success() {
        return None;
    }
    let info = String::from_utf8_lossy(&info_output.stdout);
    if !info.contains("PNGf") && !info.contains("TIFF") {
        return None;
    }

    // Write clipboard PNG to a temp file via osascript, then read the file.
    let tmp = std::env::temp_dir().join(format!("intendant-clipboard-{}.png", std::process::id()));
    let script = format!(
        concat!(
            "set pngData to the clipboard as «class PNGf»\n",
            "set f to open for access POSIX file \"{}\" with write permission\n",
            "set eof of f to 0\n",
            "write pngData to f\n",
            "close access f"
        ),
        tmp.display()
    );
    let output = tokio::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return None;
    }

    let bytes = tokio::fs::read(&tmp).await.ok()?;
    let _ = tokio::fs::remove_file(&tmp).await;
    if bytes.is_empty() {
        return None;
    }
    Some(("image/png".to_string(), bytes))
}

#[cfg(target_os = "macos")]
async fn write_clipboard_text(text: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn pbcopy: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("write to pbcopy: {e}"))?;
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait pbcopy: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("pbcopy exited with non-zero status".to_string())
    }
}

#[cfg(target_os = "macos")]
async fn write_clipboard_image(_mime: &str, data: &[u8]) -> Result<(), String> {
    // Write PNG data to a temp file, then read it into clipboard via osascript.
    let tmp = std::env::temp_dir().join(format!(
        "intendant-clipboard-write-{}.png",
        std::process::id()
    ));
    tokio::fs::write(&tmp, data)
        .await
        .map_err(|e| format!("write temp file: {e}"))?;

    let script = format!(
        concat!(
            "set f to open for access POSIX file \"{}\" \n",
            "set pngData to read f as data\n",
            "close access f\n",
            "set the clipboard to {{«class PNGf»:pngData}}"
        ),
        tmp.display()
    );
    let output = tokio::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("spawn osascript: {e}"))?;

    let _ = tokio::fs::remove_file(&tmp).await;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("osascript set clipboard image failed: {stderr}"))
    }
}

// ---------------------------------------------------------------------------
// Platform: Linux
// ---------------------------------------------------------------------------

/// Which clipboard tool to use on Linux.
#[cfg(target_os = "linux")]
enum ClipboardTool {
    /// wl-clipboard (wl-copy / wl-paste) for Wayland.
    WlClipboard,
    /// xclip for X11.
    Xclip,
}

/// Detect whether we're on Wayland or X11 and pick the appropriate tool.
#[cfg(target_os = "linux")]
fn clipboard_tool() -> ClipboardTool {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ClipboardTool::WlClipboard
    } else {
        ClipboardTool::Xclip
    }
}

#[cfg(target_os = "linux")]
async fn read_clipboard_text() -> Option<String> {
    let output = match clipboard_tool() {
        ClipboardTool::WlClipboard => tokio::process::Command::new("wl-paste")
            .arg("--no-newline")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
        ClipboardTool::Xclip => tokio::process::Command::new("xclip")
            .args(["-o", "-selection", "clipboard"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
    };
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
async fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    // Check available MIME types on the clipboard.
    let targets = match clipboard_tool() {
        ClipboardTool::WlClipboard => tokio::process::Command::new("wl-paste")
            .arg("--list-types")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
        ClipboardTool::Xclip => tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
    };
    if !targets.status.success() {
        return None;
    }
    let types_str = String::from_utf8_lossy(&targets.stdout);
    if !types_str.contains("image/png") {
        return None;
    }

    // Read PNG data.
    let output = match clipboard_tool() {
        ClipboardTool::WlClipboard => tokio::process::Command::new("wl-paste")
            .args(["--type", "image/png"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
        ClipboardTool::Xclip => tokio::process::Command::new("xclip")
            .args(["-o", "-selection", "clipboard", "-t", "image/png"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .ok()?,
    };
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(("image/png".to_string(), output.stdout))
}

#[cfg(target_os = "linux")]
async fn write_clipboard_text(text: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let (cmd, args): (&str, &[&str]) = match clipboard_tool() {
        ClipboardTool::WlClipboard => ("wl-copy", &[]),
        ClipboardTool::Xclip => ("xclip", &["-i", "-selection", "clipboard"]),
    };
    let mut child = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("write to {cmd}: {e}"))?;
    }
    let status = child.wait().await.map_err(|e| format!("wait {cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with non-zero status"))
    }
}

#[cfg(target_os = "linux")]
async fn write_clipboard_image(_mime: &str, data: &[u8]) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let (cmd, args): (&str, Vec<&str>) = match clipboard_tool() {
        ClipboardTool::WlClipboard => ("wl-copy", vec!["--type", "image/png"]),
        ClipboardTool::Xclip => (
            "xclip",
            vec!["-i", "-selection", "clipboard", "-t", "image/png"],
        ),
    };
    let mut child = tokio::process::Command::new(cmd)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(data)
            .await
            .map_err(|e| format!("write to {cmd}: {e}"))?;
    }
    let status = child.wait().await.map_err(|e| format!("wait {cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with non-zero status"))
    }
}

// ---------------------------------------------------------------------------
// Platform: Windows
// ---------------------------------------------------------------------------
//
// Backed by the `arboard` crate, which wraps the Win32 clipboard API
// (OpenClipboard / GetClipboardData / SetClipboardData). `arboard::Clipboard`
// is a synchronous handle that opens the native clipboard only for the
// duration of a single transfer, so we create a fresh one per call (cheap,
// and avoids holding the clipboard open across `.await` points). All native
// work runs on `spawn_blocking` so the 500ms monitor poll never blocks a
// tokio worker thread.
//
// Image contract mirrors macOS/Linux exactly: `read_clipboard_image` returns
// `("image/png", <PNG bytes>)` and `write_clipboard_image` receives PNG bytes
// (the `ClipboardMonitor` normalises to PNG before calling). arboard speaks
// raw RGBA8 `ImageData`, so we encode RGBA -> PNG on read and decode PNG ->
// RGBA on write (the same `image` crate helpers used elsewhere in display/).
//
// Reads swallow errors into `None` (matching the macOS/Linux arms): an empty
// or text-only clipboard yields `ContentNotAvailable`, which during a 500ms
// poll loop is the normal "nothing applicable" case, not a failure worth
// logging.

#[cfg(target_os = "windows")]
async fn read_clipboard_text() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut clipboard = arboard::Clipboard::new().ok()?;
        clipboard.get_text().ok()
    })
    .await
    .ok()
    .flatten()
}

#[cfg(target_os = "windows")]
async fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    tokio::task::spawn_blocking(|| {
        let mut clipboard = arboard::Clipboard::new().ok()?;
        let image = clipboard.get_image().ok()?;
        rgba_image_to_png(image.width, image.height, &image.bytes)
            .map(|png| ("image/png".to_string(), png))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(target_os = "windows")]
async fn write_clipboard_text(text: &str) -> Result<(), String> {
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| format!("open clipboard: {e}"))?;
        clipboard
            .set_text(text)
            .map_err(|e| format!("set clipboard text: {e}"))
    })
    .await
    .map_err(|e| format!("clipboard task join: {e}"))?
}

#[cfg(target_os = "windows")]
async fn write_clipboard_image(_mime: &str, data: &[u8]) -> Result<(), String> {
    // The monitor always hands us PNG bytes; arboard wants raw RGBA8.
    let (width, height, rgba) = png_to_rgba_image(data)?;
    tokio::task::spawn_blocking(move || {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| format!("open clipboard: {e}"))?;
        let image = arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(rgba),
        };
        clipboard
            .set_image(image)
            .map_err(|e| format!("set clipboard image: {e}"))
    })
    .await
    .map_err(|e| format!("clipboard task join: {e}"))?
}

/// Encode a raw RGBA8 buffer (as produced by `arboard::Clipboard::get_image`)
/// into PNG bytes, matching the `("image/png", <PNG bytes>)` contract the
/// macOS/Linux `read_clipboard_image` arms return.
///
/// Returns `None` if the dimensions don't match the buffer length or PNG
/// encoding fails, so the caller can treat it as "nothing usable on the
/// clipboard" rather than an error.
#[cfg(target_os = "windows")]
fn rgba_image_to_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Decode PNG bytes into a raw RGBA8 buffer for `arboard::Clipboard::set_image`.
///
/// The `ClipboardMonitor` normalises every incoming image to PNG before
/// calling `write_clipboard_image`, so we only need to handle PNG here.
/// Returns `(width, height, rgba_bytes)`.
#[cfg(target_os = "windows")]
fn png_to_rgba_image(png: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("PNG decode failed: {e}"))?
        .to_rgba8();
    let (width, height) = (img.width() as usize, img.height() as usize);
    Ok((width, height, img.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_monitor_creates() {
        let monitor = ClipboardMonitor::new();
        assert!(!monitor.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn clipboard_monitor_stop_sets_flag() {
        let monitor = ClipboardMonitor::new();
        monitor.stop();
        assert!(monitor.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn simple_hash_deterministic() {
        let data = b"hello world";
        assert_eq!(simple_hash(data), simple_hash(data));
    }

    #[test]
    fn simple_hash_different_for_different_data() {
        assert_ne!(simple_hash(b"hello"), simple_hash(b"world"));
    }

    #[test]
    fn clipboard_content_text_eq() {
        let a = ClipboardContent::Text("hello".to_string());
        let b = ClipboardContent::Text("hello".to_string());
        assert_eq!(a, b);
    }

    #[test]
    fn clipboard_content_image_eq() {
        let a = ClipboardContent::Image {
            mime: "image/png".to_string(),
            data: vec![1, 2, 3],
        };
        let b = ClipboardContent::Image {
            mime: "image/png".to_string(),
            data: vec![1, 2, 3],
        };
        assert_eq!(a, b);
    }

    /// F3 gate: with zero connected peers the watcher must emit nothing —
    /// the `continue` fires before any platform clipboard read, so this
    /// test is hermetic (no `pbpaste`/`xclip`/arboard subprocess or native
    /// clipboard is ever touched). Paused tokio time auto-advances through
    /// ~10 poll intervals deterministically.
    #[tokio::test(start_paused = true)]
    async fn watcher_is_inert_while_no_peers_connected() {
        let monitor = ClipboardMonitor::new();
        let mut rx = monitor.start_watching(Arc::new(|| false));

        let emitted = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
        assert!(
            emitted.is_err(),
            "a peer-gated (zero-viewer) watcher must not emit clipboard content"
        );

        monitor.stop();
    }

    /// stop() terminates the gated loop: the receiver closes even though
    /// the peers gate never opened (the shutdown check precedes the gate).
    #[tokio::test(start_paused = true)]
    async fn watcher_stops_while_gated() {
        let monitor = ClipboardMonitor::new();
        let mut rx = monitor.start_watching(Arc::new(|| false));
        monitor.stop();

        // The loop notices the shutdown flag on its next tick and drops
        // the sender, closing the channel.
        let got = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await;
        assert!(
            matches!(got, Ok(None)),
            "stop() must end the watcher task even while peer-gated"
        );
    }

    // Windows clipboard image helpers: pure RGBA<->PNG conversions, no live
    // clipboard required. These pin the contract the `ClipboardMonitor`
    // depends on (read returns PNG, write accepts PNG).
    #[cfg(target_os = "windows")]
    #[test]
    fn rgba_to_png_is_decodable_png() {
        // 2x2 RGBA: red, green, blue, white.
        #[rustfmt::skip]
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255,    0, 255, 0, 255,
            0, 0, 255, 255,    255, 255, 255, 255,
        ];
        let png = super::rgba_image_to_png(2, 2, &rgba).expect("encode PNG");
        // Real PNG signature.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // image crate can read it back as a PNG of the right dimensions.
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("decode PNG")
            .to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rgba_to_png_rejects_mismatched_dimensions() {
        // 3 bytes can't form any non-empty RGBA image of the claimed 2x2 size.
        assert!(super::rgba_image_to_png(2, 2, &[1, 2, 3]).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn png_to_rgba_roundtrips_dimensions_and_pixels() {
        #[rustfmt::skip]
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255,    0, 255, 0, 255,
            0, 0, 255, 255,    255, 255, 255, 255,
        ];
        let png = super::rgba_image_to_png(2, 2, &rgba).expect("encode PNG");
        let (w, h, out) = super::png_to_rgba_image(&png).expect("decode PNG");
        assert_eq!((w, h), (2, 2));
        // PNG is lossless, so the RGBA bytes survive the round trip exactly.
        assert_eq!(out, rgba);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn png_to_rgba_rejects_non_png() {
        assert!(super::png_to_rgba_image(b"not a png").is_err());
    }
}
