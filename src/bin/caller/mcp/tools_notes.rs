//! The `post_session_note` tool: a display-only note (text + optional
//! images) an agent posts into its own session transcript.
//!
//! Presentation rail only — the note is broadcast as
//! [`AppEvent::SessionNote`], rendered live by the dashboard, and persisted
//! to the session log for replay; it is **never** injected into any model
//! conversation (that path is [`crate::event::ContextInjection`]).
//!
//! Images arrive as base64 in the tool arguments (a sandboxed supervised
//! agent must not be able to make the unsandboxed daemon read arbitrary
//! file paths, so there deliberately is no path form), are committed into
//! the calling session's upload store, and travel onward as *references*
//! (`/api/session/current/uploads/<id>/raw`) — never inline bytes on the
//! WebSocket or in the session log.

use super::*;

/// Maximum note text size. Notes are transcript annotations, not documents.
pub(crate) const SESSION_NOTE_MAX_TEXT_BYTES: usize = 16 * 1024;
/// Maximum number of images per note.
pub(crate) const SESSION_NOTE_MAX_IMAGES: usize = 6;
/// Maximum decoded size of a single image.
pub(crate) const SESSION_NOTE_MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum total decoded image bytes per call. Sized so the base64
/// encoding (4/3 overhead) plus text and JSON-RPC framing always fits
/// under the `POST /mcp` body cap — pinned by a test below.
pub(crate) const SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Raster image MIME types a note may attach. SVG is deliberately
/// excluded: the `/raw` route serves blobs inline with their stored MIME,
/// and an inline SVG document executes scripts on the daemon origin when
/// opened via the thumbnail's click-through link. Raster types cannot.
const SESSION_NOTE_ALLOWED_MIME: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
];

#[derive(Debug)]
pub(crate) struct DecodedSessionNoteImage {
    pub(crate) name: String,
    pub(crate) mime: String,
    pub(crate) bytes: Vec<u8>,
}

fn canonical_note_image_mime(media_type: &str) -> Result<&'static str, String> {
    let normalized = media_type.trim().to_ascii_lowercase();
    let normalized = match normalized.as_str() {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    SESSION_NOTE_ALLOWED_MIME
        .iter()
        .find(|mime| **mime == normalized)
        .copied()
        .ok_or_else(|| {
            format!(
                "unsupported media_type '{media_type}'; supported: {}",
                SESSION_NOTE_ALLOWED_MIME.join(", ")
            )
        })
}

fn note_image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

/// Decode and validate the base64 images of a `post_session_note` call.
/// Enforces the count / per-image / total caps and the raster-MIME
/// allowlist with actionable error messages.
pub(crate) fn decode_session_note_images(
    images: &[SessionNoteImageParams],
) -> Result<Vec<DecodedSessionNoteImage>, String> {
    use base64::Engine as _;

    if images.len() > SESSION_NOTE_MAX_IMAGES {
        return Err(format!(
            "too many images: {} (max {SESSION_NOTE_MAX_IMAGES} per note)",
            images.len()
        ));
    }
    let mut decoded = Vec::with_capacity(images.len());
    let mut total_bytes = 0usize;
    for (index, image) in images.iter().enumerate() {
        let mime = canonical_note_image_mime(&image.media_type)
            .map_err(|e| format!("images[{index}]: {e}"))?;
        // Tolerate a data-URL prefix and embedded whitespace — both are
        // common in agent-produced base64 — then require valid standard
        // base64 for the remainder.
        let raw = image.data.trim();
        let raw = match raw.split_once("base64,") {
            Some((prefix, rest)) if prefix.starts_with("data:") => rest,
            _ => raw,
        };
        let compact: String = raw.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if compact.is_empty() {
            return Err(format!("images[{index}]: empty base64 data"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(compact.as_bytes())
            .map_err(|e| format!("images[{index}]: invalid base64: {e}"))?;
        if bytes.is_empty() {
            return Err(format!("images[{index}]: decoded image is empty"));
        }
        if bytes.len() > SESSION_NOTE_MAX_IMAGE_BYTES {
            return Err(format!(
                "images[{index}]: {} bytes exceeds the {} MB per-image cap",
                bytes.len(),
                SESSION_NOTE_MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES {
            return Err(format!(
                "total decoded image size exceeds the {} MB per-note cap",
                SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let name = image
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(crate::upload_store::sanitize_name)
            .unwrap_or_else(|| format!("note-image-{}.{}", index + 1, note_image_extension(mime)));
        decoded.push(DecodedSessionNoteImage {
            name,
            mime: mime.to_string(),
            bytes,
        });
    }
    Ok(decoded)
}

/// Current unix time in milliseconds (0 on a pre-epoch clock).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl IntendantServer {
    #[tool(
        description = "Post a display-only note into the session transcript, with optional base64 images. The note renders live in the dashboard transcript and persists for replay; it never enters any model's context. Images are committed to the session upload store and rendered as clickable thumbnails. Caps: 16 KB text, 6 images, 4 MB per image, 8 MB total."
    )]
    pub(crate) async fn post_session_note(
        &self,
        Parameters(params): Parameters<PostSessionNoteParams>,
    ) -> String {
        match self.post_session_note_inner(params).await {
            Ok(value) => value.to_string(),
            Err(message) => format!("post_session_note failed: {message}"),
        }
    }

    /// Core of `post_session_note`, shared by the stdio `#[tool]` method
    /// and the HTTP dispatch arm (which maps `Err` to an `isError` tool
    /// result so supervised callers see the refusal reason).
    pub(crate) async fn post_session_note_inner(
        &self,
        params: PostSessionNoteParams,
    ) -> Result<serde_json::Value, String> {
        let text = params.text.trim();
        if text.is_empty() {
            return Err("note text must not be empty".to_string());
        }
        if text.len() > SESSION_NOTE_MAX_TEXT_BYTES {
            return Err(format!(
                "note text is {} bytes; max {} KB",
                text.len(),
                SESSION_NOTE_MAX_TEXT_BYTES / 1024
            ));
        }
        let source = params
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| crate::types::truncate_str(s, 48).to_string());

        // The HTTP dispatch injects the URL-bound session id into
        // `params.session_id` (`with_default_mcp_session_id`), so an
        // explicit argument wins, then the caller's own session, then the
        // single-session state fallback.
        let (session_id, project_root, log_dir) = {
            let state = self.state.read().await;
            let session_id = params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let fallback = state.session_id.trim();
                    if fallback.is_empty() {
                        None
                    } else {
                        Some(fallback.to_string())
                    }
                });
            (
                session_id,
                state.project_root.clone(),
                state.log_dir.clone(),
            )
        };
        let Some(session_id) = session_id else {
            return Err(
                "no session to attach the note to; pass session_id (or call through the \
                 session-scoped MCP URL Intendant injected)"
                    .to_string(),
            );
        };

        let decoded = decode_session_note_images(&params.images)?;

        // Commit every image into the session's upload-store scope — the
        // same scope the gateway's `/raw` route resolves, so the URLs in
        // the note render in every browser now and after replay.
        let scope = crate::global_store::StoreScope::resolve(project_root.as_deref());
        let mut attachments: Vec<crate::types::SessionNoteAttachment> = Vec::new();
        for image in &decoded {
            let committed = write_note_image_tempfile(&image.bytes).and_then(|tmp| {
                crate::upload_store::commit_upload(
                    tmp,
                    &image.name,
                    &image.mime,
                    image.bytes.len() as u64,
                    crate::upload_store::UploadDestination::Task,
                    &log_dir,
                    &session_id,
                    &scope,
                )
                .map_err(|e| format!("failed to store image '{}': {e}", image.name))
            });
            match committed {
                Ok(descriptor) => attachments.push(crate::types::SessionNoteAttachment {
                    url: format!("/api/session/current/uploads/{}/raw", descriptor.id),
                    upload_id: descriptor.id,
                    name: descriptor.name,
                    mime: descriptor.mime,
                }),
                Err(message) => {
                    // Roll back blobs committed earlier in this call so a
                    // failed note doesn't strand half its attachments.
                    for attachment in &attachments {
                        let _ = crate::upload_store::delete_upload(
                            &attachment.upload_id,
                            &log_dir,
                            &scope,
                        );
                    }
                    return Err(message);
                }
            }
        }

        let note_id = format!("note-{}", Uuid::new_v4().simple());
        let ts = now_unix_ms();
        self.bus.send(AppEvent::SessionNote {
            session_id: Some(session_id.clone()),
            note_id: note_id.clone(),
            text: text.to_string(),
            attachments: attachments.clone(),
            source,
            ts,
        });

        Ok(serde_json::json!({
            "status": "posted",
            "note_id": note_id,
            "session_id": session_id,
            "attachments": attachments,
        }))
    }
}

fn write_note_image_tempfile(bytes: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    use std::io::Write as _;
    let mut tmp =
        tempfile::NamedTempFile::new().map_err(|e| format!("failed to create tempfile: {e}"))?;
    tmp.write_all(bytes)
        .and_then(|_| tmp.flush())
        .map_err(|e| format!("failed to write image tempfile: {e}"))?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn image(media_type: &str, data: String, name: Option<&str>) -> SessionNoteImageParams {
        SessionNoteImageParams {
            media_type: media_type.to_string(),
            data,
            name: name.map(str::to_string),
        }
    }

    /// The documented caps must always fit inside the `/mcp` body cap:
    /// base64 inflates 3 bytes to 4 characters, and the JSON-RPC envelope
    /// plus maximal text rides along. If someone raises the image caps or
    /// lowers the body cap, this fails instead of shipping a tool whose
    /// documented maximum request cannot be transported.
    #[test]
    fn documented_caps_fit_inside_mcp_body_cap() {
        let base64_overhead = SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES.div_ceil(3) * 4;
        let envelope_headroom = 64 * 1024;
        assert!(
            base64_overhead + SESSION_NOTE_MAX_TEXT_BYTES + envelope_headroom
                < crate::gateway_routes::MCP_BODY_CAP_BYTES,
            "session-note caps ({} base64 + {} text) exceed the /mcp body cap {}",
            base64_overhead,
            SESSION_NOTE_MAX_TEXT_BYTES,
            crate::gateway_routes::MCP_BODY_CAP_BYTES,
        );
    }

    #[test]
    fn decode_accepts_valid_images_and_data_urls() {
        let png = vec![0x89u8, b'P', b'N', b'G', 1, 2, 3];
        let decoded = decode_session_note_images(&[
            image("image/png", b64(&png), Some("shot one.png")),
            image(
                "image/jpg",
                format!("data:image/jpeg;base64,{}", b64(&png)),
                None,
            ),
            // Whitespace-wrapped base64 (agents often hard-wrap).
            image("image/webp", format!("{}\n", b64(&png)), None),
        ])
        .unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].bytes, png);
        assert_eq!(decoded[0].name, "shot_one.png");
        assert_eq!(decoded[0].mime, "image/png");
        // image/jpg normalizes to image/jpeg; unnamed images get a
        // deterministic extension-correct fallback name.
        assert_eq!(decoded[1].mime, "image/jpeg");
        assert_eq!(decoded[1].name, "note-image-2.jpg");
        assert_eq!(decoded[2].mime, "image/webp");
    }

    #[test]
    fn decode_rejects_bad_mime_bad_base64_and_empty_data() {
        let err = decode_session_note_images(&[image("text/html", b64(b"x"), None)]).unwrap_err();
        assert!(err.contains("unsupported media_type"), "{err}");
        // SVG is deliberately not an accepted note attachment type.
        let err = decode_session_note_images(&[image("image/svg+xml", b64(b"<svg/>"), None)])
            .unwrap_err();
        assert!(err.contains("unsupported media_type"), "{err}");
        let err =
            decode_session_note_images(&[image("image/png", "!!".to_string(), None)]).unwrap_err();
        assert!(err.contains("invalid base64"), "{err}");
        let err =
            decode_session_note_images(&[image("image/png", "  ".to_string(), None)]).unwrap_err();
        assert!(err.contains("empty base64"), "{err}");
    }

    #[test]
    fn decode_enforces_count_and_size_caps() {
        let tiny = b64(b"x");
        let too_many: Vec<SessionNoteImageParams> = (0..SESSION_NOTE_MAX_IMAGES + 1)
            .map(|_| image("image/png", tiny.clone(), None))
            .collect();
        let err = decode_session_note_images(&too_many).unwrap_err();
        assert!(err.contains("too many images"), "{err}");

        let oversized = vec![0u8; SESSION_NOTE_MAX_IMAGE_BYTES + 1];
        let err =
            decode_session_note_images(&[image("image/png", b64(&oversized), None)]).unwrap_err();
        assert!(err.contains("per-image cap"), "{err}");

        // Three images individually under the per-image cap but over the
        // total cap together.
        let chunk = vec![0u8; SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES / 2];
        let err = decode_session_note_images(&[
            image("image/png", b64(&chunk), None),
            image("image/png", b64(&chunk), None),
            image("image/png", b64(&chunk), None),
        ])
        .unwrap_err();
        assert!(err.contains("per-note cap"), "{err}");
    }

    fn test_server_with_project(
        project_root: &std::path::Path,
        log_dir: &std::path::Path,
        session_id: &str,
    ) -> (IntendantServer, EventBus) {
        let bus = EventBus::new();
        let mut state = McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            log_dir.to_path_buf(),
        );
        state.project_root = Some(project_root.to_path_buf());
        state.session_id = session_id.to_string();
        let server = IntendantServer::new(Arc::new(tokio::sync::RwLock::new(state)), bus.clone());
        (server, bus)
    }

    #[tokio::test]
    async fn post_session_note_commits_images_and_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let log_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let (server, bus) = test_server_with_project(&project_root, &log_dir, "");
        let mut rx = bus.subscribe();

        let png = vec![0x89u8, b'P', b'N', b'G'];
        let result = server
            .post_session_note_inner(PostSessionNoteParams {
                text: "Milestone: encoder pool rewired".to_string(),
                session_id: Some("sess-7".to_string()),
                source: Some("codex".to_string()),
                images: vec![SessionNoteImageParams {
                    media_type: "image/png".to_string(),
                    data: b64(&png),
                    name: Some("pool.png".to_string()),
                }],
            })
            .await
            .unwrap();

        assert_eq!(result["status"], "posted");
        assert_eq!(result["session_id"], "sess-7");
        let note_id = result["note_id"].as_str().unwrap();
        assert!(note_id.starts_with("note-"), "{note_id}");
        let upload_id = result["attachments"][0]["upload_id"].as_str().unwrap();
        assert_eq!(
            result["attachments"][0]["url"],
            format!("/api/session/current/uploads/{upload_id}/raw")
        );

        // The blob must land in the same scope the gateway's /raw route
        // resolves for this project root, under the note's session id,
        // with the bytes intact.
        let scope = crate::global_store::StoreScope::resolve(Some(&project_root));
        let descriptor =
            crate::upload_store::find_upload(upload_id, &log_dir, &scope).expect("blob committed");
        assert_eq!(descriptor.session_id, "sess-7");
        assert_eq!(descriptor.mime, "image/png");
        assert_eq!(std::fs::read(&descriptor.path).unwrap(), png);

        match rx.try_recv().expect("SessionNote broadcast") {
            AppEvent::SessionNote {
                session_id,
                note_id: event_note_id,
                text,
                attachments,
                source,
                ts,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-7"));
                assert_eq!(event_note_id, note_id);
                assert_eq!(text, "Milestone: encoder pool rewired");
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].upload_id, upload_id);
                assert_eq!(attachments[0].name, "pool.png");
                assert!(ts > 0);
                assert_eq!(source.as_deref(), Some("codex"));
            }
            other => panic!("expected SessionNote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_session_note_requires_text_and_some_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _bus) = test_server_with_project(tmp.path(), tmp.path(), "");

        let err = server
            .post_session_note_inner(PostSessionNoteParams {
                text: "   ".to_string(),
                session_id: Some("sess".to_string()),
                source: None,
                images: vec![],
            })
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");

        let err = server
            .post_session_note_inner(PostSessionNoteParams {
                text: "hello".to_string(),
                session_id: None,
                source: None,
                images: vec![],
            })
            .await
            .unwrap_err();
        assert!(err.contains("no session"), "{err}");
    }

    #[tokio::test]
    async fn post_session_note_falls_back_to_state_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, bus) = test_server_with_project(tmp.path(), tmp.path(), "state-sess");
        let mut rx = bus.subscribe();
        let result = server
            .post_session_note_inner(PostSessionNoteParams {
                text: "text-only note".to_string(),
                session_id: None,
                source: None,
                images: vec![],
            })
            .await
            .unwrap();
        assert_eq!(result["session_id"], "state-sess");
        assert_eq!(result["attachments"].as_array().unwrap().len(), 0);
        match rx.try_recv().unwrap() {
            AppEvent::SessionNote {
                session_id, source, ..
            } => {
                assert_eq!(session_id.as_deref(), Some("state-sess"));
                assert_eq!(source, None);
            }
            other => panic!("expected SessionNote, got {other:?}"),
        }
    }

    /// The tool must be callable through the generic HTTP dispatch path
    /// (the `/mcp` transport ctl and supervised agents use), with the
    /// URL-bound session id injected when the args omit one, and must
    /// surface validation failures as `isError` tool results.
    #[tokio::test]
    async fn post_session_note_dispatches_by_name_with_url_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, bus) = test_server_with_project(tmp.path(), tmp.path(), "");
        let mut rx = bus.subscribe();
        let result = server
            .call_tool_by_name_for_session(
                "post_session_note",
                serde_json::json!({ "text": "from dispatch" }),
                Some("url-sess"),
                None,
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        match rx.try_recv().unwrap() {
            AppEvent::SessionNote { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("url-sess"));
            }
            other => panic!("expected SessionNote, got {other:?}"),
        }

        let result = server
            .call_tool_by_name_for_session(
                "post_session_note",
                serde_json::json!({ "text": "" }),
                Some("url-sess"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}
