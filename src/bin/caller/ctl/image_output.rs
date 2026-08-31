use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use image::{ImageFormat, ImageReader, Limits};
use serde::Serialize;
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENCODED_IMAGE_BYTES: usize = (MAX_IMAGE_BYTES as usize).div_ceil(3) * 4;
const MAX_DECODED_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 32_768;
const MAX_CAPTURE_CLOCK_SKEW_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SavedImageReceipt {
    ok: bool,
    artifact_path: String,
    sha256: String,
    media_type: String,
    byte_length: u64,
    width: u32,
    height: u32,
    captured_at: Option<String>,
    saved_at: String,
}

#[derive(Debug, Default)]
struct ScreenshotMetadata {
    source_path: Option<PathBuf>,
    media_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    captured_at: Option<String>,
}

pub(super) fn image_contents(result: &Value) -> impl Iterator<Item = (&str, Option<&str>)> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|item| {
            let data = item.get("data").and_then(Value::as_str)?;
            let media_type = item
                .get("mimeType")
                .or_else(|| item.get("mime_type"))
                .and_then(Value::as_str);
            Some((data, media_type))
        })
}

pub(super) fn save_image_output(
    result: &Value,
    requested_path: &Path,
) -> Result<SavedImageReceipt, String> {
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("refusing to save image from a failed tool result".to_string());
    }

    let metadata = screenshot_metadata(result)?;
    let (bytes, inline_media_type) = if let Some((data, media_type)) = image_contents(result).next()
    {
        if data.len() > MAX_ENCODED_IMAGE_BYTES {
            return Err(format!(
                "encoded image exceeds the {MAX_IMAGE_BYTES}-byte decoded limit"
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("failed to decode image data: {e}"))?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "decoded image exceeds the {MAX_IMAGE_BYTES}-byte limit"
            ));
        }
        (bytes, media_type.map(str::to_owned))
    } else if let Some(source_path) = metadata.source_path.as_deref() {
        (read_regular_file_bounded(source_path)?, None)
    } else {
        return Err(
            "tool result did not include an image content block or readable screenshot_path"
                .to_string(),
        );
    };

    let (actual_media_type, width, height) = inspect_image(&bytes)?;
    for declared in [inline_media_type.as_deref(), metadata.media_type.as_deref()]
        .into_iter()
        .flatten()
    {
        if !declared.eq_ignore_ascii_case(actual_media_type) {
            return Err(format!(
                "declared image media type {declared:?} does not match decoded {actual_media_type:?}"
            ));
        }
    }
    match (metadata.width, metadata.height) {
        (Some(declared_width), Some(declared_height))
            if (declared_width, declared_height) != (width, height) =>
        {
            return Err(format!(
                "declared image dimensions {declared_width}x{declared_height} do not match decoded {width}x{height}"
            ));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "screenshot metadata must declare both width and height or neither".to_string(),
            );
        }
        _ => {}
    }

    let captured_at_parsed = metadata
        .captured_at
        .as_deref()
        .map(|captured_at| {
            DateTime::parse_from_rfc3339(captured_at)
                .map_err(|e| format!("invalid captured_at timestamp {captured_at:?}: {e}"))
        })
        .transpose()?;
    let latest_permitted_capture =
        Utc::now().fixed_offset() + chrono::Duration::seconds(MAX_CAPTURE_CLOCK_SKEW_SECONDS);
    if captured_at_parsed
        .as_ref()
        .is_some_and(|captured_at| captured_at > &latest_permitted_capture)
    {
        return Err(format!(
            "captured_at is more than {MAX_CAPTURE_CLOCK_SKEW_SECONDS}s later than the local save clock"
        ));
    }

    let artifact_path = persist_private_noclobber(&bytes, requested_path)?;
    let saved_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    Ok(SavedImageReceipt {
        ok: true,
        artifact_path: artifact_path.to_string_lossy().into_owned(),
        sha256: crate::agenda::digest_bytes(&bytes),
        media_type: actual_media_type.to_string(),
        byte_length: bytes.len() as u64,
        width,
        height,
        captured_at: metadata.captured_at,
        saved_at,
    })
}

fn screenshot_metadata(result: &Value) -> Result<ScreenshotMetadata, String> {
    let value = super::text_contents(result)
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| {
            value.get("screenshot_path").is_some()
                || value.get("artifact_path").is_some()
                || value.get("width").is_some()
                || value.get("height").is_some()
                || value.get("captured_at").is_some()
                || value.get("capturedAt").is_some()
                || value.get("mime_type").is_some()
                || value.get("mimeType").is_some()
        });
    let Some(value) = value else {
        return Ok(ScreenshotMetadata::default());
    };

    let parse_u32 = |key: &str| -> Result<Option<u32>, String> {
        value
            .get(key)
            .map(|raw| {
                let integer = raw
                    .as_u64()
                    .ok_or_else(|| format!("screenshot metadata {key} must be an integer"))?;
                u32::try_from(integer).map_err(|_| format!("screenshot metadata {key} exceeds u32"))
            })
            .transpose()
    };
    let parse_aliased_string = |keys: &[&str], label: &str| -> Result<Option<String>, String> {
        let mut parsed: Option<String> = None;
        for key in keys {
            let Some(raw) = value.get(*key) else {
                continue;
            };
            let string = raw
                .as_str()
                .ok_or_else(|| format!("screenshot metadata {key} must be a string"))?;
            if let Some(existing) = parsed.as_deref() {
                if existing != string {
                    return Err(format!("conflicting screenshot metadata {label} values"));
                }
            } else {
                parsed = Some(string.to_owned());
            }
        }
        Ok(parsed)
    };

    Ok(ScreenshotMetadata {
        source_path: parse_aliased_string(&["screenshot_path", "artifact_path"], "source path")?
            .map(PathBuf::from),
        media_type: parse_aliased_string(&["mime_type", "mimeType"], "media type")?,
        width: parse_u32("width")?,
        height: parse_u32("height")?,
        captured_at: parse_aliased_string(&["captured_at", "capturedAt"], "capture time")?,
    })
}

fn read_regular_file_bounded(path: &Path) -> Result<Vec<u8>, String> {
    if intendant_platform::platform::path_leaf_is_symlink_or_reparse(path).map_err(|e| {
        format!(
            "failed to inspect screenshot source {}: {e}",
            path.display()
        )
    })? {
        return Err(format!(
            "screenshot source must not be a symlink or reparse point: {}",
            path.display()
        ));
    }

    let entry = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "failed to inspect screenshot source {}: {e}",
            path.display()
        )
    })?;
    if !entry.is_file() {
        return Err(format!(
            "screenshot source is not a regular file: {}",
            path.display()
        ));
    }
    if entry.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "screenshot source exceeds the {MAX_IMAGE_BYTES}-byte limit: {}",
            path.display()
        ));
    }
    let before_stamp = intendant_platform::platform::file_change_stamp(path, &entry)
        .filter(|stamp| stamp.identity.is_reliable())
        .ok_or_else(|| "screenshot source has no reliable file change identity".to_string())?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("failed to open screenshot source {}: {e}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|e| format!("failed to inspect opened screenshot source: {e}"))?;
    if !opened.is_file() || opened.len() > MAX_IMAGE_BYTES {
        return Err("opened screenshot source is not a bounded regular file".to_string());
    }
    let opened_identity = intendant_platform::platform::FileIdentity::from_file(&file)
        .map_err(|e| format!("failed to identify opened screenshot source: {e}"))?;
    if !opened_identity.is_reliable() || opened_identity != before_stamp.identity {
        return Err(
            "opened screenshot source identity does not match the inspected file".to_string(),
        );
    }

    let mut bytes = Vec::with_capacity(opened.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read screenshot source {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "screenshot source exceeds the {MAX_IMAGE_BYTES}-byte limit"
        ));
    }

    if intendant_platform::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|e| format!("failed to re-inspect screenshot source: {e}"))?
    {
        return Err("screenshot source changed to a symlink while it was read".to_string());
    }
    let current_metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("failed to re-inspect screenshot source metadata: {e}"))?;
    let after_stamp = intendant_platform::platform::file_change_stamp(path, &current_metadata)
        .filter(|stamp| stamp.identity.is_reliable())
        .ok_or_else(|| "screenshot source lost its reliable file change identity".to_string())?;
    if before_stamp != after_stamp || current_metadata.len() != entry.len() {
        return Err("screenshot source changed while it was read".to_string());
    }
    let current_identity = intendant_platform::platform::FileIdentity::from_path(path)
        .map_err(|e| format!("failed to re-identify screenshot source: {e}"))?;
    if !current_identity.is_reliable() || opened_identity != current_identity {
        return Err("screenshot source identity changed while it was read".to_string());
    }

    Ok(bytes)
}

fn inspect_image(bytes: &[u8]) -> Result<(&'static str, u32, u32), String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("failed to identify saved image format: {e}"))?;
    let format = reader
        .format()
        .ok_or_else(|| "saved image format could not be identified".to_string())?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        other => return Err(format!("unsupported screenshot image format: {other:?}")),
    };
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| format!("failed to read saved image dimensions: {e}"))?;
    if width == 0 || height == 0 {
        return Err("saved image dimensions must be non-zero".to_string());
    }
    let decoded_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(8))
        .ok_or_else(|| "saved image decoded-size estimate overflowed".to_string())?;
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || decoded_bytes > MAX_DECODED_IMAGE_BYTES
    {
        return Err(format!(
            "saved image exceeds the {MAX_IMAGE_DIMENSION}px dimension or {MAX_DECODED_IMAGE_BYTES}-byte decoded limit"
        ));
    }

    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("failed to identify saved image format for decoding: {e}"))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_IMAGE_BYTES);
    decoder.limits(limits);
    let decoded = decoder
        .decode()
        .map_err(|e| format!("failed to fully decode saved image pixels: {e}"))?;
    if decoded.width() != width || decoded.height() != height {
        return Err("saved image dimensions changed during full decoding".to_string());
    }
    Ok((media_type, width, height))
}

fn persist_private_noclobber(bytes: &[u8], requested_path: &Path) -> Result<PathBuf, String> {
    let file_name = requested_path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "--output must name a file".to_string())?;
    let requested_parent = requested_path.parent().unwrap_or_else(|| Path::new("."));
    if intendant_platform::platform::path_leaf_is_symlink_or_reparse(requested_parent).map_err(
        |e| {
            format!(
                "failed to inspect output directory {}: {e}",
                requested_parent.display()
            )
        },
    )? {
        return Err(format!(
            "output directory must not be a symlink or reparse point: {}",
            requested_parent.display()
        ));
    }
    let parent_metadata = std::fs::symlink_metadata(requested_parent).map_err(|e| {
        format!(
            "failed to inspect output directory {}: {e}",
            requested_parent.display()
        )
    })?;
    if !parent_metadata.is_dir() {
        return Err(format!(
            "output parent is not a directory: {}",
            requested_parent.display()
        ));
    }
    let parent = std::fs::canonicalize(requested_parent).map_err(|e| {
        format!(
            "failed to canonicalize output directory {}: {e}",
            requested_parent.display()
        )
    })?;
    let output_path = parent.join(file_name);
    match std::fs::symlink_metadata(&output_path) {
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite existing output {}",
                output_path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect output {}: {error}",
                output_path.display()
            ));
        }
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".intendant-image-")
        .tempfile_in(&parent)
        .map_err(|e| format!("failed to create private output staging file: {e}"))?;
    set_private_permissions(temporary.as_file())?;
    temporary
        .write_all(bytes)
        .map_err(|e| format!("failed to write private output staging file: {e}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| format!("failed to sync private output staging file: {e}"))?;

    let persisted = temporary.persist_noclobber(&output_path).map_err(|error| {
        format!(
            "failed to install private output {} without overwriting: {}",
            output_path.display(),
            error.error
        )
    })?;
    let finalize_result = (|| -> Result<(), String> {
        set_private_permissions(&persisted)?;
        persisted.sync_all().map_err(|e| {
            format!(
                "failed to sync private output {}: {e}",
                output_path.display()
            )
        })?;
        verify_private_regular_file(&persisted, bytes.len() as u64, &output_path)?;

        #[cfg(unix)]
        File::open(&parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("failed to sync output directory {}: {e}", parent.display()))?;
        Ok(())
    })();
    if let Err(error) = finalize_result {
        drop(persisted);
        return match std::fs::remove_file(&output_path) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; also failed to remove incomplete output {}: {cleanup_error}",
                output_path.display()
            )),
        };
    }

    Ok(output_path)
}

fn set_private_permissions(file: &File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set private output permissions: {e}"))?;
    }
    let _ = file;
    Ok(())
}

fn verify_private_regular_file(file: &File, expected_len: u64, path: &Path) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|e| format!("failed to inspect saved output {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() != expected_len {
        return Err(format!(
            "saved output {} is not the expected regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(format!(
                "saved output {} does not have mode 0600",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![0x7f; (width * height * 4) as usize];
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
            .expect("encode png");
        bytes
    }

    fn inline_result_at(bytes: &[u8], width: u32, height: u32, captured_at: &str) -> Value {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        serde_json::json!({
            "content": [
                {
                    "type": "text",
                    "text": serde_json::json!({
                        "status": "screenshot captured",
                        "width": width,
                        "height": height,
                        "captured_at": captured_at
                    }).to_string()
                },
                {"type": "image", "data": encoded, "mimeType": "image/png"}
            ]
        })
    }

    fn inline_result(bytes: &[u8], width: u32, height: u32) -> Value {
        inline_result_at(bytes, width, height, "2026-08-31T20:00:00.000Z")
    }

    #[test]
    fn inline_image_is_saved_privately_with_verified_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(3, 2);
        let receipt =
            save_image_output(&inline_result(&bytes, 3, 2), &output).expect("save verified image");

        assert_eq!(std::fs::read(&output).expect("read output"), bytes);
        assert_eq!(
            receipt.artifact_path,
            std::fs::canonicalize(&output)
                .expect("canonical output")
                .to_string_lossy()
        );
        assert_eq!(receipt.sha256, crate::agenda::digest_bytes(&bytes));
        assert_eq!(receipt.media_type, "image/png");
        assert_eq!(receipt.byte_length, bytes.len() as u64);
        assert_eq!((receipt.width, receipt.height), (3, 2));
        assert_eq!(
            receipt.captured_at.as_deref(),
            Some("2026-08-31T20:00:00.000Z")
        );
        DateTime::parse_from_rfc3339(&receipt.saved_at).expect("valid saved_at");
        let rendered = serde_json::to_string(&receipt).expect("serialize receipt");
        assert!(
            !rendered.contains("data"),
            "receipt must not contain image data"
        );
        assert!(
            !rendered.contains("base64"),
            "receipt must not mention base64"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&output)
                    .expect("output metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn existing_output_is_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        std::fs::write(&output, b"existing").expect("seed output");
        let bytes = png_bytes(1, 1);

        let error = save_image_output(&inline_result(&bytes, 1, 1), &output)
            .expect_err("existing output refused");
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert_eq!(std::fs::read(&output).expect("read output"), b"existing");
    }

    #[cfg(unix)]
    #[test]
    fn output_symlink_is_refused_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.png");
        let output = dir.path().join("proof.png");
        std::fs::write(&target, b"target").expect("seed target");
        symlink(&target, &output).expect("create symlink");
        let bytes = png_bytes(1, 1);

        save_image_output(&inline_result(&bytes, 1, 1), &output)
            .expect_err("output symlink refused");
        assert_eq!(std::fs::read(&target).expect("read target"), b"target");
    }

    #[test]
    fn path_backed_screenshot_is_supported_and_verified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("captured.png");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(4, 5);
        std::fs::write(&source, &bytes).expect("write source");
        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "screenshot_path": source,
                    "mime_type": "image/png",
                    "width": 4,
                    "height": 5,
                    "captured_at": "2026-08-31T20:00:00.000Z"
                }).to_string()
            }]
        });

        let receipt = save_image_output(&result, &output).expect("save path-backed image");
        assert_eq!((receipt.width, receipt.height), (4, 5));
        assert_eq!(std::fs::read(output).expect("read output"), bytes);
    }

    #[test]
    fn inline_image_is_preferred_over_path_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(2, 1);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let result = serde_json::json!({
            "content": [
                {
                    "type": "text",
                    "text": serde_json::json!({
                        "screenshot_path": dir.path().join("missing.png"),
                        "width": 2,
                        "height": 1,
                        "captured_at": "2026-08-31T20:00:00.000Z"
                    }).to_string()
                },
                {"type": "image", "data": encoded, "mimeType": "image/png"}
            ]
        });

        save_image_output(&result, &output).expect("inline image wins");
        assert_eq!(std::fs::read(output).expect("read output"), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn path_backed_source_symlink_is_refused() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("captured.png");
        let source = dir.path().join("source-link.png");
        let output = dir.path().join("proof.png");
        std::fs::write(&target, png_bytes(1, 1)).expect("write target");
        symlink(&target, &source).expect("create source symlink");
        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "screenshot_path": source,
                    "mime_type": "image/png",
                    "width": 1,
                    "height": 1,
                    "captured_at": "2026-08-31T20:00:00.000Z"
                }).to_string()
            }]
        });

        save_image_output(&result, &output).expect_err("source symlink refused");
        assert!(!output.exists());
    }

    #[test]
    fn dimension_mismatch_is_refused_before_output_creation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(2, 2);
        let error = save_image_output(&inline_result(&bytes, 3, 2), &output)
            .expect_err("dimension mismatch refused");
        assert!(error.contains("dimensions"), "{error}");
        assert!(!output.exists());
    }

    #[test]
    fn truncated_pixel_stream_is_refused_before_output_creation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let mut bytes = png_bytes(8, 8);
        bytes.truncate(45);
        let dimensions = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .expect("identify truncated PNG")
            .into_dimensions()
            .expect("header still carries dimensions");
        assert_eq!(dimensions, (8, 8));

        let error = save_image_output(&inline_result(&bytes, 8, 8), &output)
            .expect_err("truncated pixels refused");
        assert!(error.contains("fully decode"), "{error}");
        assert!(!output.exists());
    }

    #[test]
    fn bounded_future_capture_clock_skew_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(1, 1);
        let captured_at = (Utc::now() + chrono::Duration::seconds(60))
            .to_rfc3339_opts(SecondsFormat::Millis, true);

        save_image_output(&inline_result_at(&bytes, 1, 1, &captured_at), &output)
            .expect("bounded peer clock skew accepted");
        assert!(output.exists());
    }

    #[test]
    fn excessive_future_capture_clock_skew_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(1, 1);
        let captured_at = (Utc::now()
            + chrono::Duration::seconds(MAX_CAPTURE_CLOCK_SKEW_SECONDS + 60))
        .to_rfc3339_opts(SecondsFormat::Millis, true);

        let error = save_image_output(&inline_result_at(&bytes, 1, 1, &captured_at), &output)
            .expect_err("excessive peer clock skew refused");
        assert!(error.contains("later than the local save clock"), "{error}");
        assert!(!output.exists());
    }

    #[test]
    fn failed_tool_result_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("proof.png");
        let bytes = png_bytes(1, 1);
        let mut result = inline_result(&bytes, 1, 1);
        result["isError"] = Value::Bool(true);

        save_image_output(&result, &output).expect_err("failed result refused");
        assert!(!output.exists());
    }
}
